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
    /// Kelp's own [`META_DIR`] is always ignored.
    pub fn is_ignored(&self, rel: &str) -> bool {
        rel == META_DIR || rel.starts_with(&format!("{META_DIR}/")) || self.set.is_match(rel)
    }
}
