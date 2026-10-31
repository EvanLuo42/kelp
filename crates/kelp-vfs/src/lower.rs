use std::collections::{BTreeSet, HashMap};

/// A file of the lower layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowerFile {
    /// An identifier of the content, passed back to the [`Hydrator`](crate::Hydrator).
    pub content: [u8; 32],
    pub size: u64,
    pub executable: bool,
    /// Whether the file must not be written until it is checked out.
    pub read_only: bool,
    /// The modification time reported for the file, in nanoseconds since the
    /// Unix epoch.
    pub mtime: u64,
}

/// The files of the lower layer, indexed by directory.
///
/// Paths are workspace-relative with `/` separators; the root is `""`.
#[derive(Clone, Debug, Default)]
pub struct LowerTree {
    files: HashMap<String, LowerFile>,
    children: HashMap<String, BTreeSet<String>>,
    /// The number of files below each directory.
    dirs: HashMap<String, usize>,
}

impl LowerTree {
    /// Creates an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the file at `local`.
    pub fn insert(&mut self, local: &str, file: LowerFile) {
        if self.files.insert(local.to_owned(), file).is_some() {
            return;
        }
        let mut child = local;
        for dir in ancestors(local) {
            *self.dirs.entry(dir.to_owned()).or_default() += 1;
            self.children
                .entry(dir.to_owned())
                .or_default()
                .insert(name(child).to_owned());
            child = dir;
        }
    }

    /// Removes the file at `local`, and directories left empty.
    pub fn remove(&mut self, local: &str) {
        if self.files.remove(local).is_none() {
            return;
        }
        let mut child = local.to_owned();
        let mut child_gone = true;
        for dir in ancestors(local) {
            if child_gone && let Some(names) = self.children.get_mut(dir) {
                names.remove(name(&child));
            }
            let count = self
                .dirs
                .get_mut(dir)
                .expect("ancestors of a file are counted");
            *count -= 1;
            child_gone = *count == 0 && !dir.is_empty();
            if child_gone {
                self.dirs.remove(dir);
                self.children.remove(dir);
            }
            child = dir.to_owned();
        }
    }

    /// Returns the file at `local`.
    pub fn file(&self, local: &str) -> Option<&LowerFile> {
        self.files.get(local)
    }

    /// Returns `true` if `local` is the root or a directory with files below it.
    pub fn is_dir(&self, local: &str) -> bool {
        local.is_empty() || self.dirs.contains_key(local)
    }

    /// Returns the names of the files and directories directly in `dir`.
    pub fn children(&self, dir: &str) -> impl Iterator<Item = &str> {
        self.children
            .get(dir)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// Returns all files below `dir`, or all files for the root.
    pub fn files_below<'a>(
        &'a self,
        dir: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a LowerFile)> + 'a {
        self.files
            .iter()
            .filter(move |(p, _)| {
                dir.is_empty() || p.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
            })
            .map(|(p, f)| (p.as_str(), f))
    }

    /// Returns the number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Returns `true` if there are no files.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Returns the directories containing `local`, innermost first, ending with the root.
pub(crate) fn ancestors(local: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(local);
    std::iter::from_fn(move || {
        let current = rest?;
        let parent = current.rfind('/').map_or("", |i| &current[..i]);
        rest = (!current.is_empty() && !parent.is_empty()).then_some(parent);
        if current.is_empty() {
            None
        } else {
            Some(parent)
        }
    })
}

/// Returns the last segment of `local`.
pub(crate) fn name(local: &str) -> &str {
    local.rsplit('/').next().unwrap_or(local)
}
