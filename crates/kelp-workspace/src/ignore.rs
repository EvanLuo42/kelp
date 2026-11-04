//! Ignore rules for files that should never be added, read from `.kelpignore`.
//!
//! Each non-empty line not starting with `#` is a glob. A pattern containing a
//! `/` (other than a trailing one) is anchored at the workspace root; any other
//! pattern matches at any depth. A trailing `/` makes the pattern match
//! directories only. A pattern also matches everything below a matching
//! directory.
//!
//! A line starting with `!` re-includes paths matched by an earlier pattern.
//! The last matching line wins, so `!` must come after the pattern it undoes.
//! As in gitignore, a file cannot be re-included once its parent directory is
//! ignored as a whole: write `Saved/*` rather than `Saved/` before `!Saved/keep.txt`.
//! Use `\!` or `\#` to match a name that starts with `!` or `#`.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::{Error, META_DIR, Result};

/// The name of the ignore file at the workspace root.
pub const IGNORE_FILE: &str = ".kelpignore";

/// A compiled set of ignore rules.
#[derive(Clone, Debug)]
pub struct Ignore {
    set: GlobSet,
    rules: Vec<Rule>,
}

/// One line of the ignore file. It owns two consecutive globs in the set: the
/// path itself, then everything below it.
#[derive(Clone, Copy, Debug)]
struct Rule {
    /// Written with a leading `!`.
    negated: bool,
    /// Written with a trailing `/`: the path itself only matches a directory.
    dir_only: bool,
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
    /// assert!(ignore.is_ignored("Content/a.tmp", false));
    /// assert!(ignore.is_ignored("Saved/Logs/game.log", false));
    /// assert!(!ignore.is_ignored("Content/Saved/a.png", false));
    ///
    /// let ignore = Ignore::parse("*.tmp\n!keep.tmp\n").unwrap();
    /// assert!(ignore.is_ignored("a.tmp", false));
    /// assert!(!ignore.is_ignored("Content/keep.tmp", false));
    ///
    /// // A slash in the middle anchors the pattern; a trailing one means directories only.
    /// let ignore = Ignore::parse("docs/*.md\nbuild/\n").unwrap();
    /// assert!(ignore.is_ignored("docs/a.md", false));
    /// assert!(!ignore.is_ignored("x/docs/a.md", false));
    /// assert!(ignore.is_ignored("x/build", true));
    /// assert!(!ignore.is_ignored("x/build", false));
    /// assert!(ignore.is_ignored("x/build/a.o", false));
    /// ```
    pub fn parse(src: &str) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut rules = Vec::new();
        for line in src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let (negate, line) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                // `\!` and `\#` match a literal leading `!` or `#`.
                None => (
                    false,
                    line.strip_prefix('\\')
                        .filter(|r| r.starts_with(['!', '#']))
                        .unwrap_or(line),
                ),
            };
            if line.is_empty() {
                continue;
            }
            let pattern = line.trim_end_matches('/');
            rules.push(Rule {
                negated: negate,
                dir_only: pattern.len() < line.len(),
            });
            let pattern = match pattern.strip_prefix('/') {
                Some(anchored) => anchored.to_owned(),
                None if pattern.contains('/') => pattern.to_owned(),
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
        Ok(Self { set, rules })
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
    /// `is_dir` says whether `rel` is a directory, which matters for patterns
    /// with a trailing `/`.
    /// Kelp's own [`META_DIR`] is always ignored; see [`is_meta_path`].
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if is_meta_path(rel) {
            return true;
        }
        // The last matching rule wins.
        self.set
            .matches(rel)
            .into_iter()
            .filter(|glob| glob % 2 == 1 || is_dir || !self.rules[glob / 2].dir_only)
            .max()
            .is_some_and(|glob| !self.rules[glob / 2].negated)
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
