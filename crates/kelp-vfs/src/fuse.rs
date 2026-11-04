//! The FUSE presentation of an [`Overlay`] on Linux.

use std::collections::{HashMap, HashSet};
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
    /// Inodes whose path was unlinked or replaced while the kernel still
    /// knew them; they keep their last path until forgotten, and answer
    /// through an open handle if there is one.
    orphans: HashSet<u64>,
    next_inode: u64,
    files: HashMap<u64, Handle>,
    next_handle: u64,
    uid: u32,
    gid: u32,
}

/// An open file.
struct Handle {
    file: File,
    /// The file's current workspace path, which follows renames.
    local: String,
    ino: u64,
    /// Whether the handle counts as a writer of `local` in the overlay.
    writer: bool,
}

impl Fuse {
    fn new(overlay: Arc<Overlay>) -> Self {
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            overlay,
            paths: HashMap::from([(ROOT, String::new())]),
            inodes: HashMap::from([(String::new(), ROOT)]),
            orphans: HashSet::new(),
            next_inode: ROOT + 1,
            files: HashMap::new(),
            next_handle: 1,
            uid,
            gid,
        }
    }

    /// Returns the path of `ino`; an orphan has none, since another file
    /// may live at its old path.
    fn path(&self, ino: u64) -> Result<String, i32> {
        if self.orphans.contains(&ino) {
            return Err(libc::ENOENT);
        }
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
            Node::Upper(meta) => self.upper_attr(ino, &meta),
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

    fn open_handle(&mut self, ino: u64, local: &str, flags: i32) -> Result<u64, i32> {
        let writing = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if writing
            && let Some(Node::LowerFile(f)) = self.overlay.node(local)
            && f.read_only
        {
            return Err(libc::EACCES);
        }
        // A writer is registered before the file is opened, so that sync
        // cannot replace the copy between hydrating and writing it.
        let path = match writing {
            true => self.overlay.open_writer(local),
            false => self.overlay.hydrate(local),
        }
        .map_err(errno)?;
        let opened = File::options()
            .read(true)
            .write(writing)
            .open(path)
            .and_then(|file| {
                if flags & libc::O_TRUNC != 0 {
                    self.overlay.touched(local);
                    file.set_len(0)?;
                }
                Ok(file)
            });
        let file = match opened {
            Ok(file) => file,
            Err(e) => {
                if writing {
                    self.overlay.close_writer(local);
                }
                return Err(errno(e));
            }
        };
        Ok(self.add_handle(file, local, ino, writing))
    }

    fn add_handle(&mut self, file: File, local: &str, ino: u64, writer: bool) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        let local = local.to_owned();
        self.files.insert(
            fh,
            Handle {
                file,
                local,
                ino,
                writer,
            },
        );
        fh
    }

    /// Detaches the inodes at and below `local`, which is gone, from their
    /// paths. Ones still open become orphans until the kernel forgets them,
    /// so that their handles keep working.
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
                if self.files.values().any(|h| h.ino == ino) {
                    self.orphans.insert(ino);
                } else {
                    self.paths.remove(&ino);
                }
            }
        }
    }

    /// Moves the inodes at and below `from` to `to`, after a rename, so
    /// that the kernel's inodes keep naming the same files.
    fn move_path(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        // Whatever the rename replaced is gone.
        self.forget_path(to);
        let moved: Vec<(String, String)> = self
            .inodes
            .keys()
            .filter_map(|p| Some((p.clone(), crate::overlay::rebase(p, from, to)?)))
            .collect();
        for (old, new) in moved {
            if let Some(ino) = self.inodes.remove(&old) {
                self.paths.insert(ino, new.clone());
                self.inodes.insert(new, ino);
            }
        }
        for handle in self.files.values_mut() {
            if let Some(new) = crate::overlay::rebase(&handle.local, from, to) {
                handle.local = new;
            }
        }
    }

    /// Returns the attributes of `ino`, through an open handle for an
    /// orphan.
    fn attr_of(&mut self, ino: u64) -> Result<FileAttr, i32> {
        if !self.orphans.contains(&ino) {
            let local = self.path(ino)?;
            return self.attr(&local);
        }
        let handle = self.orphan_handle(ino, None)?;
        let meta = handle.file.metadata().map_err(errno)?;
        Ok(self.upper_attr(ino, &meta))
    }

    /// Returns the handle `fh`, or else any handle, open on the orphan `ino`.
    fn orphan_handle(&self, ino: u64, fh: Option<u64>) -> Result<&Handle, i32> {
        fh.and_then(|fh| self.files.get(&fh))
            .filter(|h| h.ino == ino)
            .or_else(|| self.files.values().find(|h| h.ino == ino))
            .ok_or(libc::ENOENT)
    }

    fn upper_attr(&self, ino: u64, meta: &std::fs::Metadata) -> FileAttr {
        let mtime = UNIX_EPOCH
            + Duration::new(meta.mtime().max(0) as u64, meta.mtime_nsec().max(0) as u32);
        FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.blocks(),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: UNIX_EPOCH,
            kind: if meta.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: (meta.permissions().mode() & 0o7777) as u16,
            nlink: meta.nlink() as u32,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
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

    fn forget(&mut self, _req: &Request<'_>, ino: u64, _nlookup: u64) {
        // The kernel forgets an inode once it no longer uses it at all; an
        // orphan has no path to be found by again.
        if self.orphans.remove(&ino) {
            self.paths.remove(&ino);
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.attr_of(ino) {
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
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let times = (atime.is_some() || mtime.is_some()).then(|| {
            let mut times = FileTimes::new();
            if let Some(t) = atime {
                times = times.set_accessed(time(t));
            }
            if let Some(t) = mtime {
                times = times.set_modified(time(t));
            }
            times
        });
        let result = (|| -> Result<FileAttr, i32> {
            if self.orphans.contains(&ino) {
                // An unlinked file still open: change it through its handle.
                let file = &self.orphan_handle(ino, fh)?.file;
                if let Some(mode) = mode {
                    file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
                        .map_err(errno)?;
                }
                if let Some(size) = size {
                    file.set_len(size).map_err(errno)?;
                }
                if let Some(times) = times {
                    file.set_times(times).map_err(errno)?;
                }
                return self.attr_of(ino);
            }
            let local = self.path(ino)?;
            let only_mode = size.is_none() && times.is_none();
            if let (Some(mode), true) = (mode, only_mode)
                && let Some(Node::LowerFile(f)) = self.overlay.node(&local)
                && (mode & 0o111 != 0) == f.executable
                && self.overlay.set_lower_read_only(&local, mode & 0o200 == 0)
            {
                return self.attr(&local);
            }
            // Holding a writer keeps sync from replacing the copy midway.
            let path = self.overlay.open_writer(&local).map_err(errno)?;
            let changed = (|| -> io::Result<()> {
                if let Some(mode) = mode {
                    // A copy whose mode differs from the lower file's is no
                    // longer what sync may replace.
                    self.overlay.touched(&local);
                    let mode = std::fs::Permissions::from_mode(mode & 0o7777);
                    std::fs::set_permissions(&path, mode)?;
                }
                if let Some(size) = size {
                    self.overlay.touched(&local);
                    File::options()
                        .write(true)
                        .open(&path)
                        .and_then(|f| f.set_len(size))?;
                }
                if let Some(times) = times {
                    File::options()
                        .write(true)
                        .open(&path)
                        .and_then(|f| f.set_times(times))?;
                }
                Ok(())
            })();
            self.overlay.close_writer(&local);
            changed.map_err(errno)?;
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
            self.move_path(&from, &to);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.path(ino).and_then(|p| self.open_handle(ino, &p, flags)) {
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
            let attr = self.attr(&local)?;
            // A created file is never clean, so sync leaves it alone
            // without counting it as a writer.
            Ok((attr, self.add_handle(file, &local, attr.ino, false)))
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
        let Some(Handle { file, .. }) = self.files.get(&fh) else {
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
        let Some(Handle { file, local, .. }) = self.files.get(&fh) else {
            return reply.error(libc::EBADF);
        };
        // The copy stops being clean before it changes; the handle being a
        // writer keeps sync from replacing it meanwhile.
        self.overlay.touched(local);
        match file.write_all_at(data, offset as u64) {
            Ok(()) => reply.written(data.len() as u32),
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
        match self.files.get(&fh).map(|h| h.file.sync_all()) {
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
        if let Some(handle) = self.files.remove(&fh)
            && handle.writer
        {
            self.overlay.close_writer(&handle.local);
        }
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
