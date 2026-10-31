//! Depot paths of the form `//depot/stream/dir/file.ext`.
//!
//! Paths preserve their original spelling but compare by a folded key (NFC
//! normalization followed by lowercasing), so two paths that differ only in
//! case or Unicode normalization can never coexist on the server.

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::{Error, Result};

const WINDOWS_RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A validated, case-preserving depot path.
///
/// Equality, ordering and hashing use the case-folded [`key`](Self::key);
/// [`Display`](fmt::Display) and [`as_str`](Self::as_str) return the original
/// spelling.
///
/// # Examples
///
/// ```
/// use kelp_core::DepotPath;
///
/// let a = DepotPath::parse("//Game/Main/Hero.PNG").unwrap();
/// let b = DepotPath::parse("//game/main/hero.png").unwrap();
/// assert_eq!(a, b);
/// assert_eq!(a.as_str(), "//Game/Main/Hero.PNG");
/// ```
#[derive(Clone, Serialize, Deserialize)]
pub struct DepotPath {
    original: String,
    key: String,
}

impl DepotPath {
    /// Parses and validates a depot path.
    ///
    /// The input is NFC-normalized. Every segment must be non-empty, must not be
    /// `.` or `..`, must not contain control characters or any of
    /// `\ : * ? " < > |`, must not end with a space or dot, and must not be a
    /// reserved Windows device name such as `CON` or `LPT1`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if `s` does not start with `//`, is empty
    /// after the prefix, or contains an invalid segment.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_core::DepotPath;
    ///
    /// let path = DepotPath::parse("//game/main/Content/Hero.png").unwrap();
    /// assert_eq!(path.depot(), "game");
    /// assert_eq!(path.file_name(), "Hero.png");
    ///
    /// assert!(DepotPath::parse("game/main").is_err());
    /// assert!(DepotPath::parse("//game/../main").is_err());
    /// ```
    pub fn parse(s: &str) -> Result<Self> {
        let invalid = |reason| Error::InvalidPath {
            path: s.to_owned(),
            reason,
        };
        let rest = s
            .strip_prefix("//")
            .ok_or_else(|| invalid("must start with //"))?;
        let original: String = rest.nfc().collect();
        if original.is_empty() {
            return Err(invalid("empty path"));
        }
        for segment in original.split('/') {
            validate_segment(segment).map_err(invalid)?;
        }
        let key = fold(&original);
        Ok(Self {
            original: format!("//{original}"),
            key,
        })
    }

    /// Returns the path as the user spelled it, after NFC normalization.
    pub fn as_str(&self) -> &str {
        &self.original
    }

    /// Returns the case-folded comparison key, without the leading `//`.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the first segment, which names the depot.
    pub fn depot(&self) -> &str {
        self.segments().next().unwrap_or_default()
    }

    /// Returns an iterator over the `/`-separated segments, without the leading `//`.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.original[2..].split('/')
    }

    /// Returns the last segment.
    pub fn file_name(&self) -> &str {
        self.segments().last().unwrap_or_default()
    }

    /// Creates a new path by appending `child` to `self`.
    ///
    /// `child` may contain several `/`-separated segments.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if the resulting path is invalid.
    pub fn join(&self, child: &str) -> Result<Self> {
        Self::parse(&format!("{}/{child}", self.original))
    }

    /// Returns `true` if `self` equals `other` or lies underneath it.
    ///
    /// Only whole segments match, so `//g/ContentExtra` does not start with
    /// `//g/Content`.
    pub fn starts_with(&self, other: &DepotPath) -> bool {
        self.key == other.key
            || (self.key.starts_with(&other.key) && self.key.as_bytes()[other.key.len()] == b'/')
    }
}

/// Returns the case-folded form of a name or path, NFC-normalized and then
/// lowercased, which is how depot paths, tree entries and locks compare.
///
/// # Examples
///
/// ```
/// assert_eq!(kelp_core::path::fold("Content/Hero.PNG"), "content/hero.png");
/// ```
pub fn fold(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

fn validate_segment(segment: &str) -> std::result::Result<(), &'static str> {
    if segment.is_empty() {
        return Err("empty path segment");
    }
    if segment == "." || segment == ".." {
        return Err("relative path segment");
    }
    if segment
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err("forbidden character");
    }
    if segment.ends_with(' ') || segment.ends_with('.') {
        return Err("segment ends with space or dot");
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        return Err("reserved Windows device name");
    }
    Ok(())
}

impl PartialEq for DepotPath {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for DepotPath {}

impl Hash for DepotPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl PartialOrd for DepotPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DepotPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl fmt::Debug for DepotPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.original)
    }
}

impl fmt::Display for DepotPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}
