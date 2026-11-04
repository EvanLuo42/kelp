//! Ignore rules for files that should never be added, read from `.kelpignore`.
//!
//! Each non-empty line not starting with `#` is a glob. A pattern starting with
//! `/` is anchored at the workspace root; any other pattern matches at any
//! depth. A pattern also matches everything below a matching directory.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::{Error, META_DIR, Result};

/// The name of the ignore file at the workspace root.
pub const IGNORE_FILE: &str = ".kelpignore";

/// A compiled set of ignore rules.
#[derive(Clone, Debug)]
pub struct Ignore {
    set: GlobSet,
}

impl Ignore {
    /// Compiles ignore rules from the contents of an ignore file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidIgnore`] if a pattern is not a valid glob.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_workspace::Ignore;
    ///
    /// let ignore = Ignore::parse("*.tmp\n/Saved/\n").unwrap();
    /// assert!(ignore.is_ignored("Content/a.tmp"));
    /// assert!(ignore.is_ignored("Saved/Logs/game.log"));
    /// assert!(!ignore.is_ignored("Content/Saved/a.png"));
    /// ```
    pub fn parse(src: &str) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for line in src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let pattern = line.trim_end_matches('/');
            let pattern = match pattern.strip_prefix('/') {
                Some(anchored) => anchored.to_owned(),
                None => format!("**/{pattern}"),
            };
            for p in [pattern.clone(), format!("{pattern}/**")] {
                let glob = GlobBuilder::new(&p)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| Error::InvalidIgnore(line.to_owned(), e.to_string()))?;
                builder.add(glob);
            }
        }
        let set = builder
            .build()
            .map_err(|e| Error::InvalidIgnore(src.to_owned(), e.to_string()))?;
        Ok(Self { set })
    }

    /// Reads the ignore file of the workspace rooted at `root`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or compiled.
    pub fn load(root: &Path) -> Result<Self> {
        match std::fs::read_to_string(root.join(IGNORE_FILE)) {
            Ok(src) => Self::parse(&src),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::parse(""),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `true` if the workspace path `rel` is ignored.
    ///
    /// Kelp's own [`META_DIR`] is always ignored; see [`is_meta_path`].
    pub fn is_ignored(&self, rel: &str) -> bool {
        is_meta_path(rel) || self.set.is_match(rel)
    }
}

/// Returns `true` if the workspace path `rel`, with `/` separators, is or
/// lies below a directory named like [`META_DIR`], ignoring ASCII case.
///
/// Such paths are reserved: on a case-insensitive file system `.KELP` is
/// Kelp's own directory, and a nested `.kelp` would make the directory
/// holding it look like a workspace of its own.
///
/// # Examples
///
/// ```
/// use kelp_workspace::ignore::is_meta_path;
///
/// assert!(is_meta_path(".kelp/state.bin"));
/// assert!(is_meta_path(".KELP"));
/// assert!(is_meta_path("Content/.Kelp/config.toml"));
/// assert!(!is_meta_path("Content/.kelpignore"));
/// ```
pub fn is_meta_path(rel: &str) -> bool {
    rel.split('/').any(|s| s.eq_ignore_ascii_case(META_DIR))
}
