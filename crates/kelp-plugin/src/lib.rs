//! WASM component plugins.
//!
//! Plugins are WebAssembly components implementing one or more of the worlds
//! in `wit/kelp.wit`: [`chunker`](Plugin::chunk), [`merger`](Plugin::merge),
//! [`differ`](Plugin::describe) and [`trigger`](Plugin::pre_submit). A
//! [`Host`] compiles them and runs each call in a fresh, sandboxed instance:
//! plugins get no file system, network, clock or randomness, only their
//! arguments, and each call is bounded by [`Limits`].

mod host;

use std::fmt;

pub use host::{Change, FileVersion, Host, Limits, MergeResult, Plugin, Span, Verdict};

/// The error type for plugin operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A string was not a valid [`PluginRef`].
    #[error("invalid plugin reference {0:?}, expected plugin:<name>@<version> or builtin:<name>")]
    InvalidRef(String),
    /// The bytes are not a valid component, or it imports something the
    /// host does not provide.
    #[error("invalid plugin: {0}")]
    Invalid(String),
    /// The plugin does not implement the world needed for a call.
    #[error("the plugin is not a {0}")]
    WrongWorld(PluginWorld),
    /// The plugin trapped, ran out of fuel or exceeded its memory limit.
    #[error("the plugin failed: {0}")]
    Trap(String),
}

/// A specialized [`Result`](std::result::Result) type for plugin operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The WIT world a plugin component implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginWorld {
    /// Splits file content into chunks.
    Chunker,
    /// Merges divergent revisions of a file.
    Merger,
    /// Produces a human-readable diff between revisions.
    Differ,
    /// Runs in response to server events such as submits.
    Trigger,
}

/// A plugin reference as written in a typemap, such as `plugin:texture-mips@1.2.0`.
///
/// [`Display`](fmt::Display) formats the reference back into the same syntax
/// accepted by [`parse`](Self::parse).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginRef {
    /// A plugin compiled into Kelp, written `builtin:<name>`.
    Builtin(String),
    /// A WASM component plugin, written `plugin:<name>@<version>`.
    Wasm {
        /// The plugin name.
        name: String,
        /// The plugin version.
        version: String,
    },
}

impl PluginRef {
    /// Parses a plugin reference.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRef`] if `s` is neither `builtin:<name>` nor
    /// `plugin:<name>@<version>` with a non-empty name and version.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_plugin::PluginRef;
    ///
    /// let r = PluginRef::parse("plugin:texture-mips@1.2.0").unwrap();
    /// assert_eq!(
    ///     r,
    ///     PluginRef::Wasm {
    ///         name: "texture-mips".into(),
    ///         version: "1.2.0".into(),
    ///     }
    /// );
    /// assert_eq!(r.to_string(), "plugin:texture-mips@1.2.0");
    ///
    /// assert!(PluginRef::parse("plugin:no-version").is_err());
    /// ```
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(name) = s.strip_prefix("builtin:") {
            return Ok(Self::Builtin(name.to_owned()));
        }
        let bad = || Error::InvalidRef(s.to_owned());
        let rest = s.strip_prefix("plugin:").ok_or_else(bad)?;
        let (name, version) = rest.split_once('@').ok_or_else(bad)?;
        if name.is_empty() || version.is_empty() {
            return Err(bad());
        }
        Ok(Self::Wasm {
            name: name.to_owned(),
            version: version.to_owned(),
        })
    }
}

impl PluginWorld {
    /// All worlds, in declaration order.
    pub const ALL: [Self; 4] = [Self::Chunker, Self::Merger, Self::Differ, Self::Trigger];

    /// Returns the name of the function a component of this world exports.
    pub fn export(self) -> &'static str {
        match self {
            Self::Chunker => "chunk",
            Self::Merger => "merge",
            Self::Differ => "describe",
            Self::Trigger => "pre-submit",
        }
    }
}

impl fmt::Display for PluginWorld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Chunker => "chunker",
            Self::Merger => "merger",
            Self::Differ => "differ",
            Self::Trigger => "trigger",
        })
    }
}

impl fmt::Display for PluginRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin(n) => write!(f, "builtin:{n}"),
            Self::Wasm { name, version } => write!(f, "plugin:{name}@{version}"),
        }
    }
}
