//! Per-path file type rules, stored in `//<depot>/.kelp/typemap.toml`.
//!
//! Rules match stream-relative paths case-insensitively. They are applied in
//! order, and later rules override only the fields they set.

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The content kind of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// Line-oriented text that can be diffed and merged.
    Text,
    /// Opaque binary data.
    Binary,
    /// Not decided by any rule; the client sniffs the content.
    #[default]
    Auto,
}

/// Whether a file must be locked before it is edited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockPolicy {
    /// Concurrent edits are allowed.
    #[default]
    None,
    /// Only the holder of an exclusive lock may edit the file.
    Exclusive,
}

/// How line endings of text files are treated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EolPolicy {
    /// Line endings are stored exactly as submitted.
    #[default]
    Preserve,
    /// Line endings are normalized on submit.
    Normalize,
}

/// The effective type of one file after all matching rules are applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileType {
    /// The content kind.
    pub kind: FileKind,
    /// The locking policy.
    pub lock: LockPolicy,
    /// The line-ending policy.
    pub eol: EolPolicy,
    /// Whether chunks are compressed at rest.
    pub compress: bool,
    /// The number of latest revisions whose content is kept, or `None` to keep all.
    pub retain: Option<u32>,
    /// The chunker as `plugin:<name>@<version>`, or `None` for the built-in FastCDC chunker.
    pub chunker: Option<String>,
    /// The merge driver as `builtin:3way` or `plugin:<name>@<version>`.
    pub merge: Option<String>,
    /// The plugin that describes changes for people, as
    /// `plugin:<name>@<version>`.
    pub differ: Option<String>,
}

impl Default for FileType {
    fn default() -> Self {
        Self {
            kind: FileKind::Auto,
            lock: LockPolicy::None,
            eol: EolPolicy::Preserve,
            compress: true,
            retain: None,
            chunker: None,
            merge: None,
            differ: None,
        }
    }
}

impl FileType {
    /// Returns `true` if pending changes to this file may be split across changelists by hunk.
    pub fn allows_hunk_split(&self) -> bool {
        self.kind == FileKind::Text && self.lock == LockPolicy::None
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTypemap {
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
    #[serde(default, rename = "trigger")]
    triggers: Vec<RawTrigger>,
    #[serde(default)]
    issues: Option<RawIssues>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIssues {
    prefixes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrigger {
    pattern: String,
    plugin: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    pattern: String,
    kind: Option<FileKind>,
    lock: Option<LockPolicy>,
    eol: Option<EolPolicy>,
    compress: Option<bool>,
    retain: Option<u32>,
    chunker: Option<String>,
    merge: Option<String>,
    differ: Option<String>,
}

struct Rule {
    matcher: GlobMatcher,
    raw: RawRule,
}

/// An ordered list of file type rules.
///
/// # Examples
///
/// ```
/// use kelp_core::typemap::{FileKind, LockPolicy, Typemap};
///
/// let map = Typemap::parse(
///     r#"
///     [[rule]]
///     pattern = "**/*.png"
///     kind = "binary"
///     lock = "exclusive"
///     "#,
/// )
/// .unwrap();
///
/// let ty = map.resolve("Content/Hero.PNG");
/// assert_eq!(ty.kind, FileKind::Binary);
/// assert_eq!(ty.lock, LockPolicy::Exclusive);
/// ```
pub struct Typemap {
    rules: Vec<Rule>,
    triggers: Vec<(GlobMatcher, String)>,
    issue_prefixes: Option<Vec<String>>,
}

impl Typemap {
    /// Parses a typemap from TOML source containing `[[rule]]` tables.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTypemap`] if the TOML is malformed, a rule has an
    /// unknown field, or a pattern is not a valid glob.
    pub fn parse(toml_src: &str) -> Result<Self> {
        let raw: RawTypemap =
            toml::from_str(toml_src).map_err(|e| Error::InvalidTypemap(e.to_string()))?;
        if let Some(rule) = raw.rules.iter().find(|r| r.retain == Some(0)) {
            return Err(Error::InvalidTypemap(format!(
                "{}: retain must be at least 1, or the current revision would be lost",
                rule.pattern
            )));
        }
        let rules = raw
            .rules
            .into_iter()
            .map(|raw| {
                Ok(Rule {
                    matcher: matcher(&raw.pattern)?,
                    raw,
                })
            })
            .collect::<Result<_>>()?;
        let triggers = raw
            .triggers
            .into_iter()
            .map(|t| Ok((matcher(&t.pattern)?, t.plugin)))
            .collect::<Result<_>>()?;
        Ok(Self {
            rules,
            triggers,
            issue_prefixes: raw.issues.map(|i| i.prefixes),
        })
    }

    /// Returns the effective file type for a stream-relative path such as `Content/Hero.png`.
    pub fn resolve(&self, stream_relative: &str) -> FileType {
        let mut ty = FileType::default();
        for rule in self
            .rules
            .iter()
            .filter(|r| r.matcher.is_match(stream_relative))
        {
            let r = &rule.raw;
            if let Some(v) = r.kind {
                ty.kind = v;
            }
            if let Some(v) = r.lock {
                ty.lock = v;
            }
            if let Some(v) = r.eol {
                ty.eol = v;
            }
            if let Some(v) = r.compress {
                ty.compress = v;
            }
            if let Some(v) = r.retain {
                ty.retain = Some(v);
            }
            if let Some(v) = &r.chunker {
                ty.chunker = Some(v.clone());
            }
            if let Some(v) = &r.merge {
                ty.merge = Some(v.clone());
            }
            if let Some(v) = &r.differ {
                ty.differ = Some(v.clone());
            }
        }
        ty
    }

    /// Returns the plugins of the `[[trigger]]` tables whose pattern matches
    /// one of the stream-relative `paths`, in table order and without
    /// duplicates.
    ///
    /// ```
    /// use kelp_core::typemap::Typemap;
    ///
    /// let map = Typemap::parse(
    ///     r#"
    ///     [[trigger]]
    ///     pattern = "Code/**"
    ///     plugin = "plugin:lint@1"
    ///     "#,
    /// )
    /// .unwrap();
    /// assert_eq!(map.triggers(["Code/a.rs", "Code/b.rs"]), ["plugin:lint@1"]);
    /// assert!(map.triggers(["Art/a.png"]).is_empty());
    /// ```
    pub fn triggers<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<&str> {
        let paths: Vec<&str> = paths.into_iter().collect();
        let mut out: Vec<&str> = Vec::new();
        for (matcher, plugin) in &self.triggers {
            if !out.contains(&plugin.as_str()) && paths.iter().any(|p| matcher.is_match(p)) {
                out.push(plugin);
            }
        }
        out
    }

    /// Returns the project prefixes of issue keys that changelist
    /// descriptions may mention, from `[issues] prefixes = [...]`, or `None`
    /// to accept any key; see [`crate::issue::find_keys`].
    pub fn issue_prefixes(&self) -> Option<&[String]> {
        self.issue_prefixes.as_deref()
    }

    /// Returns every `plugin:` reference in the typemap, without duplicates.
    pub fn plugins(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let refs = self
            .rules
            .iter()
            .flat_map(|r| [&r.raw.chunker, &r.raw.merge, &r.raw.differ])
            .flatten()
            .chain(self.triggers.iter().map(|(_, p)| p));
        for r in refs {
            if r.starts_with("plugin:") && !out.contains(&r.as_str()) {
                out.push(r);
            }
        }
        out
    }
}

fn matcher(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::InvalidTypemap(format!("{pattern}: {e}")))?
        .compile_matcher())
}
