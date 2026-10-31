use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::lower::{ancestors, name};
use crate::{Hydrator, LowerFile, LowerTree, Projection};

/// The file in the upper directory that lists whiteouts, one path per line.
const WHITEOUTS: &str = ".kelp-whiteouts";

/// What a path refers to in the union of the two layers.
#[derive(Clone, Debug)]
pub enum Node {
    /// A file or directory in the upper directory.
    Upper(Metadata),
    /// A lower file that has not been opened yet.
    LowerFile(LowerFile),
    /// A directory that exists only because lower files are below it.
    LowerDir,
}

impl Node {
    /// Returns `true` for directories.
    pub fn is_dir(&self) -> bool {
        match self {
            Self::Upper(m) => m.is_dir(),
            Self::LowerFile(_) => false,
            Self::LowerDir => true,
        }
    }
}

/// The union of a lower tree and an upper directory, independent of how it
/// is presented to the operating system.
///
/// Paths are workspace-relative with `/` separators; the root is `""`.
pub struct Overlay {
    upper: PathBuf,
    lower: RwLock<LowerTree>,
    whiteouts: Mutex<BTreeSet<String>>,
    /// Upper files copied from the lower layer and not modified since.
    clean: Mutex<HashSet<String>>,
    /// Held while a file is copied up and while the lower layer changes, so
    /// that a copy of an old version cannot land after a new one.
    layers: Mutex<()>,
    hydrator: Arc<dyn Hydrator>,
}

impl Overlay {
    /// Creates an overlay of `lower` and the directory `upper`, loading the
    /// whiteouts recorded there.
    ///
    /// # Errors
    ///
    /// Returns an error if `upper` cannot be created or its whiteouts read.
    pub fn new(upper: PathBuf, lower: LowerTree, hydrator: Arc<dyn Hydrator>) -> io::Result<Self> {
        fs::create_dir_all(&upper)?;
        let whiteouts = match fs::read_to_string(upper.join(WHITEOUTS)) {
            Ok(text) => text
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            upper,
            lower: RwLock::new(lower),
            whiteouts: Mutex::new(whiteouts),
            clean: Mutex::default(),
            layers: Mutex::default(),
            hydrator,
        })
    }

    /// Returns the upper directory's path for `local`.
    pub fn upper_path(&self, local: &str) -> PathBuf {
        if local.is_empty() {
            self.upper.clone()
        } else {
            self.upper.join(local)
        }
    }

    /// Returns what `local` refers to, if anything.
    pub fn node(&self, local: &str) -> Option<Node> {
        if local == WHITEOUTS {
            return None;
        }
        if let Ok(meta) = fs::symlink_metadata(self.upper_path(local)) {
            return Some(Node::Upper(meta));
        }
        if self.is_whited_out(local) {
            return None;
        }
        let lower = self.lower.read().expect("lower lock");
        match lower.file(local) {
            Some(file) => Some(Node::LowerFile(*file)),
            None if lower.is_dir(local) => Some(Node::LowerDir),
            None => None,
        }
    }

    /// Returns the entries of the directory `dir`, by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the upper directory cannot be listed.
    pub fn list(&self, dir: &str) -> io::Result<BTreeMap<String, Node>> {
        let mut entries = BTreeMap::new();
        let names: Vec<String> = self
            .lower
            .read()
            .expect("lower lock")
            .children(dir)
            .map(str::to_owned)
            .collect();
        for name in names {
            let local = join(dir, &name);
            if let Some(node) = self.node(&local) {
                entries.insert(name, node);
            }
        }
        match fs::read_dir(self.upper_path(dir)) {
            Ok(read) => {
                for entry in read {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if dir.is_empty() && name == WHITEOUTS {
                        continue;
                    }
                    entries.insert(name, Node::Upper(entry.metadata()?));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(entries)
    }

    /// Makes sure `local` exists in the upper directory, copying a lower file
    /// up through the hydrator, and returns its upper path.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] if nothing is at `local`, or the
    /// hydrator's error.
    pub fn hydrate(&self, local: &str) -> io::Result<PathBuf> {
        let path = self.upper_path(local);
        let _layers = self.layers.lock().expect("layers lock");
        match self.node(local) {
            Some(Node::Upper(_)) => {}
            Some(Node::LowerFile(file)) => {
                self.ensure_upper_parent(local)?;
                let tmp = path.with_file_name(format!(".kelp-hydrate-{}", name(local)));
                let mut out = BufWriter::new(File::create(&tmp)?);
                self.hydrator.hydrate(local, &file, &mut out)?;
                out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
                set_mode(&tmp, file.executable, file.read_only)?;
                fs::rename(&tmp, &path)?;
                self.clean
                    .lock()
                    .expect("clean lock")
                    .insert(local.to_owned());
            }
            Some(Node::LowerDir) => {
                self.ensure_upper_parent(local)?;
                fs::create_dir_all(&path)?;
            }
            None => return Err(io::ErrorKind::NotFound.into()),
        }
        Ok(path)
    }

    /// Records that the upper copy of `local` was modified.
    pub fn touched(&self, local: &str) {
        self.clean.lock().expect("clean lock").remove(local);
    }

    /// Creates an empty upper file at `local`, replacing any lower file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created.
    pub fn create(&self, local: &str) -> io::Result<File> {
        reject_reserved(local)?;
        self.ensure_upper_parent(local)?;
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(self.upper_path(local))?;
        self.set_whiteout(local, false)?;
        self.touched(local);
        Ok(file)
    }

    /// Creates the directory `local`.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::AlreadyExists`] if something is there.
    pub fn mkdir(&self, local: &str) -> io::Result<()> {
        reject_reserved(local)?;
        if self.node(local).is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        self.ensure_upper_parent(local)?;
        fs::create_dir(self.upper_path(local))?;
        self.set_whiteout(local, false)
    }

    /// Removes the file `local`, hiding its lower version.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] if nothing is there.
    pub fn unlink(&self, local: &str) -> io::Result<()> {
        let node = self.node(local).ok_or(io::ErrorKind::NotFound)?;
        if let Node::Upper(_) = node {
            fs::remove_file(self.upper_path(local))?;
        }
        self.touched(local);
        if self.lower.read().expect("lower lock").file(local).is_some() {
            self.set_whiteout(local, true)?;
        }
        Ok(())
    }

    /// Removes the empty directory `local`.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::DirectoryNotEmpty`] if anything is below it.
    pub fn rmdir(&self, local: &str) -> io::Result<()> {
        if !self.list(local)?.is_empty() {
            return Err(io::ErrorKind::DirectoryNotEmpty.into());
        }
        match fs::remove_dir(self.upper_path(local)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        if self.lower.read().expect("lower lock").is_dir(local) {
            self.set_whiteout(local, true)?;
        }
        Ok(())
    }

    /// Renames `from` to `to`, copying lower files up first.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] if `from` does not exist, or an
    /// error if content cannot be hydrated.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        reject_reserved(to)?;
        let node = self.node(from).ok_or(io::ErrorKind::NotFound)?;
        if node.is_dir() {
            let below: Vec<String> = self
                .lower
                .read()
                .expect("lower lock")
                .files_below(from)
                .map(|(p, _)| p.to_owned())
                .collect();
            for path in below {
                if self.node(&path).is_some() {
                    self.hydrate(&path)?;
                }
            }
        }
        let source = self.hydrate(from)?;
        self.ensure_upper_parent(to)?;
        fs::rename(source, self.upper_path(to))?;
        self.touched(from);
        self.touched(to);
        self.set_whiteout(to, false)?;
        let lower = self.lower.read().expect("lower lock");
        let hide = lower.file(from).is_some() || lower.is_dir(from);
        drop(lower);
        if hide {
            self.set_whiteout(from, true)?;
        }
        Ok(())
    }

    /// Changes whether a lower file is read-only without copying it up.
    /// Returns `false` if `local` is not a lower file.
    pub fn set_lower_read_only(&self, local: &str, read_only: bool) -> bool {
        if !matches!(self.node(local), Some(Node::LowerFile(_))) {
            return false;
        }
        let mut lower = self.lower.write().expect("lower lock");
        let Some(mut file) = lower.file(local).copied() else {
            return false;
        };
        file.read_only = read_only;
        lower.insert(local, file);
        true
    }

    fn is_whited_out(&self, local: &str) -> bool {
        let whiteouts = self.whiteouts.lock().expect("whiteout lock");
        whiteouts.contains(local)
            || ancestors(local).any(|a| !a.is_empty() && whiteouts.contains(a))
    }

    fn set_whiteout(&self, local: &str, hidden: bool) -> io::Result<()> {
        let mut whiteouts = self.whiteouts.lock().expect("whiteout lock");
        let changed = if hidden {
            whiteouts.insert(local.to_owned())
        } else {
            let prefix = format!("{local}/");
            let before = whiteouts.len();
            whiteouts.retain(|w| w != local && !w.starts_with(&prefix));
            // Something new at `local` replaces what the lower tree has
            // there: a directory created where one was deleted starts
            // empty, so its lower entries stay hidden.
            {
                let lower = self.lower.read().expect("lower lock");
                if lower.is_dir(local) {
                    for child in lower.children(local) {
                        whiteouts.insert(join(local, child));
                    }
                }
            }
            // Recreating a path below a hidden directory unhides the directories
            // on the way down, so their other lower entries must be hidden one by one.
            let hidden_ancestor = ancestors(local)
                .filter(|a| !a.is_empty() && whiteouts.contains(*a))
                .last()
                .map(str::to_owned);
            if let Some(top) = hidden_ancestor {
                whiteouts.remove(&top);
                let lower = self.lower.read().expect("lower lock");
                let mut dir = top;
                while let Some(rest) = local.strip_prefix(&format!("{dir}/")) {
                    let next = rest.split('/').next().expect("a non-empty remainder");
                    for child in lower.children(&dir) {
                        if child != next {
                            whiteouts.insert(join(&dir, child));
                        }
                    }
                    dir = join(&dir, next);
                }
            }
            whiteouts.len() != before
        };
        if changed {
            let text: String = whiteouts.iter().map(|w| format!("{w}\n")).collect();
            let tmp = self.upper.join(format!("{WHITEOUTS}.tmp"));
            fs::write(&tmp, text)?;
            fs::rename(tmp, self.upper.join(WHITEOUTS))?;
        }
        Ok(())
    }

    /// Creates the upper directories containing `local`.
    fn ensure_upper_parent(&self, local: &str) -> io::Result<()> {
        match local.rfind('/') {
            Some(i) => fs::create_dir_all(self.upper_path(&local[..i])),
            None => Ok(()),
        }
    }
}

impl Projection for Overlay {
    fn project(&self, local: &str, file: Option<LowerFile>) -> io::Result<bool> {
        let _layers = self.layers.lock().expect("layers lock");
        let upper = self.upper_path(local);
        if fs::symlink_metadata(&upper).is_ok() {
            if !self.clean.lock().expect("clean lock").contains(local) {
                return Ok(false);
            }
            fs::remove_file(&upper)?;
        }
        self.touched(local);
        {
            let mut lower = self.lower.write().expect("lower lock");
            match file {
                Some(f) => lower.insert(local, f),
                None => lower.remove(local),
            }
        }
        if file.is_some() {
            self.set_whiteout(local, false)?;
        }
        Ok(true)
    }
}

/// Refuses names the overlay keeps its own state under, which a user file
/// would otherwise overwrite.
fn reject_reserved(local: &str) -> io::Result<()> {
    let name = local.rsplit('/').next().unwrap_or(local);
    match local == WHITEOUTS || name.starts_with(".kelp-hydrate-") {
        true => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{local} is reserved for Kelp"),
        )),
        false => Ok(()),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool, read_only: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match (executable, read_only) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, _executable: bool, read_only: bool) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(read_only);
    fs::set_permissions(path, perms)
}
