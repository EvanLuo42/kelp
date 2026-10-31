//! Mapping between depot paths and workspace paths.
//!
//! A view is a list of lines, each mapping a depot directory to a workspace
//! directory, or excluding a depot directory when prefixed with `-`:
//!
//! ```text
//! //game/main/...                          ...
//! -//game/main/Content/Cinematics/...
//! //game/main/Content/Cinematics/Ep1/...   Cine/Ep1/...
//! ```
//!
//! Later lines override earlier ones. Only a trailing `...` wildcard is
//! supported.

use kelp_core::DepotPath;

use crate::{Error, Result};

#[derive(Debug, Clone)]
struct Mapping {
    exclude: bool,
    depot_prefix: String,
    depot_key: String,
    local: String,
}

/// A parsed workspace view.
#[derive(Debug, Clone, Default)]
pub struct View {
    mappings: Vec<Mapping>,
}

impl View {
    /// Parses a view from its textual form.
    ///
    /// Blank lines and lines starting with `#` are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidView`] if a line is malformed, or
    /// [`Error::Core`] if a depot path is invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_core::DepotPath;
    /// use kelp_workspace::View;
    ///
    /// let view = View::parse("//game/main/... ...\n-//game/main/Raw/...").unwrap();
    /// let path = DepotPath::parse("//game/main/Source/main.rs").unwrap();
    /// assert_eq!(view.map(&path).as_deref(), Some("Source/main.rs"));
    /// assert_eq!(view.map(&DepotPath::parse("//game/main/Raw/a.psd").unwrap()), None);
    /// ```
    pub fn parse(src: &str) -> Result<Self> {
        let mut mappings = Vec::new();
        for line in src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let bad = |why| Error::InvalidView(line.to_owned(), why);
            let (exclude, rest) = match line.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, line),
            };
            let mut parts = rest.split_whitespace();
            let depot = parts.next().ok_or_else(|| bad("missing depot side"))?;
            let depot_prefix = depot
                .strip_suffix("...")
                .ok_or_else(|| bad("depot side must end with ..."))?;
            let local = match (exclude, parts.next()) {
                (true, None) => String::new(),
                (true, Some(_)) => return Err(bad("exclusions take no workspace side")),
                (false, Some(l)) => l
                    .strip_suffix("...")
                    .ok_or_else(|| bad("workspace side must end with ..."))?
                    .to_owned(),
                (false, None) => return Err(bad("missing workspace side")),
            };
            if !local.is_empty() {
                let dir = local
                    .strip_suffix('/')
                    .ok_or_else(|| bad("workspace side must be a directory"))?;
                let escapes = dir.starts_with('/')
                    || dir.contains(['\\', ':'])
                    || dir.split('/').any(|s| matches!(s, "" | "." | ".."))
                    || dir
                        .split('/')
                        .next()
                        .is_some_and(|s| s.eq_ignore_ascii_case(crate::META_DIR));
                if escapes {
                    return Err(bad("workspace side must stay inside the workspace"));
                }
            }
            let dir = depot_prefix
                .strip_suffix('/')
                .ok_or_else(|| bad("depot side must be a directory"))?;
            let dir = DepotPath::parse(dir)?;
            mappings.push(Mapping {
                exclude,
                depot_prefix: format!("{dir}/"),
                depot_key: format!("{}/", dir.key()),
                local,
            });
        }
        Ok(Self { mappings })
    }

    /// Returns a view mapping all of `stream` onto the workspace root.
    pub fn whole_stream(stream: &DepotPath) -> Self {
        Self::parse(&format!("{stream}/... ...")).expect("a stream path forms a valid view line")
    }

    /// Returns the workspace path of `path`, or `None` if it is outside the view.
    pub fn map(&self, path: &DepotPath) -> Option<String> {
        let key = path.key();
        let m = self
            .mappings
            .iter()
            .rev()
            .find(|m| key.starts_with(&m.depot_key))?;
        if m.exclude {
            return None;
        }
        let depth = m.depot_key.matches('/').count();
        let rest: Vec<&str> = path.segments().skip(depth).collect();
        Some(format!("{}{}", m.local, rest.join("/")))
    }

    /// Returns the depot path that maps to the workspace path `local`, or `None`
    /// if no depot path in the view maps there.
    pub fn to_depot(&self, local: &str) -> Option<DepotPath> {
        self.mappings
            .iter()
            .rev()
            .filter(|m| !m.exclude)
            .find_map(|m| {
                let rest = local.strip_prefix(&m.local)?;
                let depot = DepotPath::parse(&format!("{}{rest}", m.depot_prefix)).ok()?;
                (self.map(&depot).as_deref() == Some(local)).then_some(depot)
            })
    }
}
