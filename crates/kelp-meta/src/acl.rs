//! Path-level access control.
//!
//! A protections table is a list of lines of the form `level who path`:
//!
//! ```text
//! write  *              //...
//! read   group:artists  //engine/...
//! none   user:bob       //game/main/Secret/...
//! ```
//!
//! `level` is `none`, `read` or `write`; `who` is `*`, `user:<name>` or
//! `group:<name>`; `path` is a depot path, ending in `...` to cover a whole
//! directory. For a given user and path the last matching line wins, and a
//! path no line matches is inaccessible. Administrators bypass the table.

use kelp_core::DepotPath;

use crate::{Error, Result};

/// An access level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    None,
    Read,
    Write,
}

/// Who a line of a rules table applies to: `*`, `user:<name>` or
/// `group:<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Who {
    /// Everyone.
    Everyone,
    /// One user, by folded name.
    User(String),
    /// The members of a group, by folded name.
    Group(String),
}

impl Who {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.split_once(':') {
            None if s == "*" => Some(Self::Everyone),
            Some(("user", name)) if !name.is_empty() => Some(Self::User(name.to_lowercase())),
            Some(("group", name)) if !name.is_empty() => Some(Self::Group(name.to_lowercase())),
            _ => None,
        }
    }

    /// Returns `true` if `subject` is one of the users this names.
    pub fn includes(&self, subject: Subject<'_>) -> bool {
        match self {
            Self::Everyone => true,
            Self::User(name) => *name == subject.user.to_lowercase(),
            Self::Group(name) => subject.groups.iter().any(|g| g.to_lowercase() == *name),
        }
    }
}

impl std::fmt::Display for Who {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Everyone => f.write_str("*"),
            Self::User(name) => write!(f, "user:{name}"),
            Self::Group(name) => write!(f, "group:{name}"),
        }
    }
}

/// The path of a line of a rules table: a file, or a directory followed by
/// `...` for everything below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathPattern {
    /// The folded path without `//`; a directory prefix ending in `/` (or
    /// empty for everything) when `recursive`.
    key: String,
    recursive: bool,
}

impl PathPattern {
    pub(crate) fn parse(path: &str) -> std::result::Result<Self, String> {
        Ok(match path.strip_suffix("...") {
            Some("//") => Self {
                key: String::new(),
                recursive: true,
            },
            Some(dir) => {
                let dir = dir.strip_suffix('/').ok_or("`...` must follow a `/`")?;
                let dir = DepotPath::parse(dir).map_err(|e| e.to_string())?;
                Self {
                    key: format!("{}/", dir.key()),
                    recursive: true,
                }
            }
            None => Self {
                key: DepotPath::parse(path)
                    .map_err(|e| e.to_string())?
                    .key()
                    .to_owned(),
                recursive: false,
            },
        })
    }

    pub(crate) fn matches(&self, path: &DepotPath) -> bool {
        match self.recursive {
            true => path.key().starts_with(&self.key),
            false => path.key() == self.key,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    level: Level,
    who: Who,
    path: PathPattern,
}

/// A parsed protections table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protections {
    rules: Vec<Rule>,
}

/// The identity a permission check is made for.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    /// The user name.
    pub user: &'a str,
    /// The groups the user belongs to.
    pub groups: &'a [String],
}

impl Protections {
    /// Returns the table used before an administrator sets one: everyone may
    /// write everywhere.
    pub fn open() -> Self {
        Self::parse("write * //...").expect("the default table is valid")
    }

    /// Parses a protections table.
    ///
    /// Blank lines and lines starting with `#` are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if a line is malformed.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_core::DepotPath;
    /// use kelp_meta::acl::{Level, Protections, Subject};
    ///
    /// let table = Protections::parse("write * //...\nnone user:bob //game/main/Secret/...").unwrap();
    /// let bob = Subject { user: "bob", groups: &[] };
    /// let secret = DepotPath::parse("//game/main/Secret/plan.txt").unwrap();
    /// assert_eq!(table.level(bob, &secret), Level::None);
    /// ```
    pub fn parse(src: &str) -> Result<Self> {
        let mut rules = Vec::new();
        for line in src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let bad = |why: &str| Error::Invalid(format!("protections line {line:?}: {why}"));
            let mut parts = line.split_whitespace();
            let (Some(level), Some(who), Some(path), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(bad("expected `level who path`"));
            };
            let level = match level {
                "none" => Level::None,
                "read" => Level::Read,
                "write" => Level::Write,
                _ => return Err(bad("level must be none, read or write")),
            };
            let who =
                Who::parse(who).ok_or_else(|| bad("who must be *, user:<name> or group:<name>"))?;
            let path = PathPattern::parse(path).map_err(|e| bad(&e))?;
            rules.push(Rule { level, who, path });
        }
        Ok(Self { rules })
    }

    /// Returns the access level of `subject` on the file `path`.
    pub fn level(&self, subject: Subject<'_>, path: &DepotPath) -> Level {
        self.rules
            .iter()
            .rev()
            .find(|r| r.who.includes(subject) && r.path.matches(path))
            .map_or(Level::None, |r| r.level)
    }

    /// Returns `true` if some line could grant `subject` at least read access
    /// to a file below the directory `dir`.
    ///
    /// This is used to decide whether a stream is visible at all; access to
    /// individual files is still checked with [`level`](Self::level).
    pub fn may_read_below(&self, subject: Subject<'_>, dir: &DepotPath) -> bool {
        let prefix = format!("{}/", dir.key());
        self.rules.iter().any(|r| {
            r.level >= Level::Read
                && r.who.includes(subject)
                && if r.path.recursive {
                    prefix.starts_with(&r.path.key) || r.path.key.starts_with(&prefix)
                } else {
                    r.path.key.starts_with(&prefix)
                }
        })
    }
}
