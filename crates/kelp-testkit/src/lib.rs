//! Shared fixtures for Kelp tests.
//!
//! Every test that touches the file system, the clock or other process-wide
//! state goes through this crate, so that tests stay isolated from each other
//! and from the machine they run on.

#[cfg(all(feature = "net", feature = "s3"))]
pub mod cluster;
#[cfg(feature = "net")]
pub mod http;
#[cfg(feature = "meta")]
pub mod meta;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "wasm")]
pub mod wasm;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kelp_core::DepotPath;
use kelp_core::time::{Clock, Millis};

/// A temporary directory that is removed when dropped.
///
/// All file system access in tests is rooted in a sandbox. Paths passed to
/// its helpers are relative to the sandbox root and use `/` separators.
#[derive(Debug)]
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// Creates an empty sandbox.
    ///
    /// # Panics
    ///
    /// Panics if the temporary directory cannot be created.
    pub fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("kelp-test-")
                .tempdir()
                .expect("create sandbox"),
        }
    }

    /// Returns the sandbox root.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Returns the absolute path of `rel` without touching the file system.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// Creates the directory `rel` and its parents, returning its absolute path.
    ///
    /// # Panics
    ///
    /// Panics if the directory cannot be created.
    pub fn dir(&self, rel: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(&path).expect("create sandbox directory");
        path
    }

    /// Writes `contents` to `rel`, creating parent directories as needed.
    ///
    /// # Panics
    ///
    /// Panics if the file cannot be written.
    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write sandbox file");
        path
    }

    /// Reads the file at `rel`.
    ///
    /// # Panics
    ///
    /// Panics if the file cannot be read.
    pub fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.path(rel)).expect("read sandbox file")
    }

    /// Returns `true` if `rel` exists.
    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`Clock`] that only moves when told to.
///
/// The default clock starts at `1_700_000_000_000` milliseconds, in November 2023.
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    /// Creates a clock reading `start` milliseconds since the Unix epoch.
    pub fn new(start: Millis) -> Self {
        Self {
            now: AtomicU64::new(start),
        }
    }

    /// Moves the clock forward by `millis`.
    pub fn advance(&self, millis: Millis) {
        self.now.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new(1_700_000_000_000)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Millis {
        self.now.load(Ordering::SeqCst)
    }
}

/// Parses a depot path.
///
/// # Panics
///
/// Panics if `s` is not a valid depot path.
pub fn depot_path(s: &str) -> DepotPath {
    DepotPath::parse(s).unwrap_or_else(|e| panic!("bad test path {s:?}: {e}"))
}

/// Returns `len` deterministic pseudo-random bytes derived from `seed`.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}
