//! Projected workspaces.
//!
//! A projected workspace shows every file the workspace has as soon as it is
//! synced, but downloads a file's content only when something first opens
//! it. The set of files the workspace has is the *lower* layer, a
//! [`LowerTree`] held in memory; files that were opened, written or created
//! live in an *upper* directory on disk. Deleting a lower file records a
//! whiteout that hides it.
//!
//! [`mount`] exposes the union at a mount point with the platform's
//! mechanism: FUSE on Linux and ProjFS on Windows. The returned [`Mounted`]
//! hands out a [`Projection`] through which sync updates the lower layer
//! without downloading anything.

mod lower;
mod overlay;

#[cfg(target_os = "linux")]
mod fuse;
#[cfg(windows)]
mod projfs;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

pub use lower::{LowerFile, LowerTree};
pub use overlay::{Node, Overlay};

/// Supplies the content of lower files when they are first opened.
pub trait Hydrator: Send + Sync + 'static {
    /// Writes the content of `file`, found at the workspace path `local`, to `out`.
    ///
    /// # Errors
    ///
    /// Returns an error if the content cannot be obtained; the opening
    /// program then sees an I/O error.
    fn hydrate(&self, local: &str, file: &LowerFile, out: &mut dyn Write) -> io::Result<()>;
}

/// Updates the lower layer of a mounted workspace.
pub trait Projection: Send + Sync {
    /// Makes the workspace path `local` show `file`, or nothing if `None`,
    /// and returns `true`.
    ///
    /// If the path has been modified locally, it is left alone and `false`
    /// is returned: projecting never discards someone's changes.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection cannot be updated.
    fn project(&self, local: &str, file: Option<LowerFile>) -> io::Result<bool>;
}

/// Everything needed to mount a projected workspace.
pub struct MountConfig {
    /// Where the workspace appears.
    pub mountpoint: PathBuf,
    /// The directory holding opened, written and created files. ProjFS keeps
    /// them in the mount point itself and ignores this.
    pub upper: PathBuf,
    /// The initial lower layer.
    pub lower: LowerTree,
    /// Supplies lower content on first open.
    pub hydrator: Arc<dyn Hydrator>,
}

/// A mounted projected workspace, unmounted when dropped.
pub struct Mounted {
    projection: Arc<dyn Projection>,
    _session: Box<dyn Send>,
}

impl Mounted {
    /// Returns the handle through which sync updates the lower layer.
    pub fn projection(&self) -> Arc<dyn Projection> {
        self.projection.clone()
    }
}

/// Mounts a projected workspace.
///
/// # Errors
///
/// Returns [`io::ErrorKind::Unsupported`] on platforms without a backend, or
/// the error of the platform mechanism, for example when FUSE is not
/// available.
pub fn mount(config: MountConfig) -> io::Result<Mounted> {
    #[cfg(target_os = "linux")]
    {
        fuse::mount(config)
    }
    #[cfg(windows)]
    {
        projfs::mount(config)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = config;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "projected workspaces are not supported on this platform yet",
        ))
    }
}
