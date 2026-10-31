//! The FUSE presentation of an [`Overlay`] on Linux.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, FileTimes};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};

use crate::{MountConfig, Mounted, Node, Overlay};

/// Attributes are never cached, because sync changes the lower layer.
const TTL: Duration = Duration::ZERO;
const ROOT: u64 = 1;

pub(crate) fn mount(config: MountConfig) -> io::Result<Mounted> {
    let overlay = Arc::new(Overlay::new(config.upper, config.lower, config.hydrator)?);
    let fs = Fuse::new(overlay.clone());
    let options = [
        MountOption::FSName("kelp".into()),
        MountOption::DefaultPermissions,
    ];
    let session = fuser::spawn_mount2(fs, &config.mountpoint, &options)?;
    Ok(Mounted {
        projection: overlay,
        _session: Box::new(session),
    })
}

struct Fuse {
    overlay: Arc<Overlay>,
    paths: HashMap<u64, String>,
    inodes: HashMap<String, u64>,
    next_inode: u64,
    files: HashMap<u64, (File, String)>,
    next_handle: u64,
    uid: u32,
    gid: u32,
}

impl Fuse {
    fn new(overlay: Arc<Overlay>) -> Self {
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            overlay,
            paths: HashMap::from([(ROOT, String::new())]),
            inodes: HashMap::from([(String::new(), ROOT)]),
            next_inode: ROOT + 1,
            files: HashMap::new(),
            next_handle: 1,
            uid,
            gid,
        }
    }

    fn path(&self, ino: u64) -> Result<String, i32> {
        self.paths.get(&ino).cloned().ok_or(libc::ENOENT)
    }

    fn child(&self, parent: u64, name: &OsStr) -> Result<String, i32> {
        let dir = self.path(parent)?;
        let name = name.to_str().ok_or(libc::EINVAL)?;
        Ok(if dir.is_empty() {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        })
    }

    fn inode(&mut self, local: &str) -> u64 {
        if let Some(ino) = self.inodes.get(local) {
            return *ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.inodes.insert(local.to_owned(), ino);
        self.paths.insert(ino, local.to_owned());
        ino
    }

    fn attr(&mut self, local: &str) -> Result<FileAttr, i32> {
        let node = self.overlay.node(local).ok_or(libc::ENOENT)?;
        let ino = self.inode(local);
        let at = |nanos: u64| UNIX_EPOCH + Duration::from_nanos(nanos);
        let base = FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        Ok(match node {
            Node::Upper(meta) => {
                let mtime = UNIX_EPOCH
                    + Duration::new(meta.mtime().max(0) as u64, meta.mtime_nsec().max(0) as u32);
                FileAttr {
                    size: meta.len(),
                    blocks: meta.blocks(),
                    atime: mtime,
                    mtime,
                    ctime: mtime,
                    kind: if meta.is_dir() {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    perm: (meta.permissions().mode() & 0o7777) as u16,
                    nlink: meta.nlink() as u32,
                    ..base
                }
            }
            Node::LowerFile(f) => FileAttr {
                size: f.size,
                blocks: f.size.div_ceil(512),
                atime: at(f.mtime),
                mtime: at(f.mtime),
                ctime: at(f.mtime),
                kind: FileType::RegularFile,
                perm: match (f.executable, f.read_only) {
                    (true, true) => 0o555,
                    (true, false) => 0o755,
                    (false, true) => 0o444,
                    (false, false) => 0o644,
                },
                ..base
            },
            Node::LowerDir => base,
        })
    }

    fn open_handle(&mut self, local: &str, flags: i32) -> Result<u64, i32> {
        let writing = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if writing
            && let Some(Node::LowerFile(f)) = self.overlay.node(local)
            && f.read_only
        {
            return Err(libc::EACCES);
        }
        let path = self.overlay.hydrate(local).map_err(errno)?;
        let file = File::options()
            .read(true)
            .write(writing)
            .open(path)
            .map_err(errno)?;
        if flags & libc::O_TRUNC != 0 {
            file.set_len(0).map_err(errno)?;
            self.overlay.touched(local);
        }
        let fh = self.next_handle;
        self.next_handle += 1;
        self.files.insert(fh, (file, local.to_owned()));
        Ok(fh)
    }

    fn forget_path(&mut self, local: &str) {
        let prefix = format!("{local}/");
        let gone: Vec<String> = self
            .inodes
            .keys()
            .filter(|p| *p == local || p.starts_with(&prefix))
            .cloned()
            .collect();
        for path in gone {
            if let Some(ino) = self.inodes.remove(&path) {
                self.paths.remove(&ino);
            }
        }
    }
}

fn errno(e: io::Error) -> i32 {
    e.raw_os_error().unwrap_or(match e.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::DirectoryNotEmpty => libc::ENOTEMPTY,
        _ => libc::EIO,
    })
}

fn time(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

impl Filesystem for Fuse {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.child(parent, name).and_then(|p| self.attr(&p)) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.path(ino).and_then(|p| self.attr(&p)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let result = (|| -> Result<FileAttr, i32> {
            let local = self.path(ino)?;
            let only_mode = size.is_none() && atime.is_none() && mtime.is_none();
            if let (Some(mode), true) = (mode, only_mode)
                && let Some(Node::LowerFile(f)) = self.overlay.node(&local)
                && (mode & 0o111 != 0) == f.executable
                && self.overlay.set_lower_read_only(&local, mode & 0o200 == 0)
            {
                return self.attr(&local);
            }
            let path = self.overlay.hydrate(&local).map_err(errno)?;
            if let Some(mode) = mode {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(errno)?;
            }
            if let Some(size) = size {
                File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_len(size))
                    .map_err(errno)?;
                self.overlay.touched(&local);
            }
            if atime.is_some() || mtime.is_some() {
                let mut times = FileTimes::new();
                if let Some(t) = atime {
                    times = times.set_accessed(time(t));
                }
                if let Some(t) = mtime {
                    times = times.set_modified(time(t));
                }
                File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_times(times))
                    .map_err(errno)?;
            }
            self.attr(&local)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let result = self.child(parent, name).and_then(|p| {
            self.overlay.mkdir(&p).map_err(errno)?;
            self.attr(&p)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self
            .child(parent, name)
            .and_then(|p| self.overlay.unlink(&p).map_err(errno).map(|()| p))
        {
            Ok(p) => {
                self.forget_path(&p);
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self
            .child(parent, name)
            .and_then(|p| self.overlay.rmdir(&p).map_err(errno).map(|()| p))
        {
            Ok(p) => {
                self.forget_path(&p);
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        const NOREPLACE: u32 = 1;
        let result = (|| -> Result<(), i32> {
            // Anything but RENAME_NOREPLACE, such as RENAME_EXCHANGE, would
            // need semantics the overlay does not offer; refuse rather than
            // silently replacing the target.
            if flags & !NOREPLACE != 0 {
                return Err(libc::EINVAL);
            }
            let (from, to) = (self.child(parent, name)?, self.child(newparent, newname)?);
            if flags & NOREPLACE != 0 && self.overlay.node(&to).is_some() {
                return Err(libc::EEXIST);
            }
            self.overlay.rename(&from, &to).map_err(errno)?;
            self.forget_path(&from);
            self.forget_path(&to);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.path(ino).and_then(|p| self.open_handle(&p, flags)) {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| -> Result<(FileAttr, u64), i32> {
            let local = self.child(parent, name)?;
            let file = self.overlay.create(&local).map_err(errno)?;
            file.set_permissions(std::fs::Permissions::from_mode(mode & !umask & 0o7777))
                .map_err(errno)?;
            let fh = self.next_handle;
            self.next_handle += 1;
            self.files.insert(fh, (file, local.clone()));
            Ok((self.attr(&local)?, fh))
        })();
        match result {
            Ok((attr, fh)) => reply.created(&TTL, &attr, 0, fh, 0),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some((file, _)) = self.files.get(&fh) else {
            return reply.error(libc::EBADF);
        };
        let mut buf = vec![0; size as usize];
        let mut filled = 0;
        while filled < buf.len() {
            match file.read_at(&mut buf[filled..], offset as u64 + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return reply.error(errno(e)),
            }
        }
        reply.data(&buf[..filled]);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some((file, local)) = self.files.get(&fh) else {
            return reply.error(libc::EBADF);
        };
        match file.write_all_at(data, offset as u64) {
            Ok(()) => {
                self.overlay.touched(local);
                reply.written(data.len() as u32);
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.files.get(&fh).map(|(f, _)| f.sync_all()) {
            Some(Ok(())) | None => reply.ok(),
            Some(Err(e)) => reply.error(errno(e)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.files.remove(&fh);
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let result = (|| -> Result<Vec<(u64, FileType, String)>, i32> {
            let dir = self.path(ino)?;
            let mut entries = vec![
                (ino, FileType::Directory, ".".to_owned()),
                (ino, FileType::Directory, "..".to_owned()),
            ];
            for (name, node) in self.overlay.list(&dir).map_err(errno)? {
                let local = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                let kind = if node.is_dir() {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                entries.push((self.inode(&local), kind, name));
            }
            Ok(entries)
        })();
        match result {
            Ok(entries) => {
                for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(ino, (i + 1) as i64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }
}
