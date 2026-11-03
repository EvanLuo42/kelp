//! Progress of long workspace operations.
//!
//! Operations that touch many files, such as sync and submit, report how far
//! they got through a [`Progress`] carried by the [`ContentCache`]. Reports
//! are throttled, so a receiver may draw each one.
//!
//! [`ContentCache`]: crate::ContentCache

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How often a [`Meter`] reports at most.
const INTERVAL: Duration = Duration::from_millis(100);

/// A stage of a long operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Comparing files with the workspace and looking up what to fetch.
    Scan,
    /// Downloading chunks into the local cache.
    Download,
    /// Writing files into the workspace.
    Write,
    /// Reading and chunking workspace files.
    Read,
    /// Uploading chunks to the server.
    Upload,
}

impl Phase {
    /// Returns a short lowercase name for display.
    pub fn name(self) -> &'static str {
        match self {
            Phase::Scan => "scanning",
            Phase::Download => "downloading",
            Phase::Write => "writing",
            Phase::Read => "reading",
            Phase::Upload => "uploading",
        }
    }
}

/// How far a [`Phase`] got. Items are files or chunks, depending on the phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub phase: Phase,
    pub items: u64,
    pub total_items: u64,
    pub bytes: u64,
    /// The bytes the phase will process, if known in advance.
    pub total_bytes: Option<u64>,
}

impl Update {
    /// Returns `true` once the phase has processed everything.
    pub fn is_finished(&self) -> bool {
        self.items >= self.total_items
    }
}

/// Receives progress reports.
pub trait Progress: Send + Sync {
    fn update(&self, update: Update);
}

/// Ignores all reports.
pub struct Quiet;

impl Progress for Quiet {
    fn update(&self, _: Update) {}
}

impl<F: Fn(Update) + Send + Sync> Progress for F {
    fn update(&self, update: Update) {
        self(update)
    }
}

/// Counts the progress of one phase and reports it now and then.
pub struct Meter {
    sink: Arc<dyn Progress>,
    update: Update,
    last: Option<Instant>,
}

impl Meter {
    /// Starts `phase` of `total_items` items and reports it, unless it is empty.
    pub fn start(
        sink: Arc<dyn Progress>,
        phase: Phase,
        total_items: u64,
        total_bytes: Option<u64>,
    ) -> Self {
        let mut meter = Self {
            sink,
            update: Update {
                phase,
                items: 0,
                total_items,
                bytes: 0,
                total_bytes,
            },
            last: None,
        };
        if total_items > 0 {
            meter.report();
        }
        meter
    }

    /// Adds `items` items of `bytes` bytes, reporting if it is time to, or
    /// if the phase is finished.
    pub fn add(&mut self, items: u64, bytes: u64) {
        self.update.items += items;
        self.update.bytes += bytes;
        let due = self.last.is_none_or(|t| t.elapsed() >= INTERVAL);
        if due || self.update.is_finished() {
            self.report();
        }
    }

    fn report(&mut self) {
        self.last = Some(Instant::now());
        self.sink.update(self.update);
    }
}
