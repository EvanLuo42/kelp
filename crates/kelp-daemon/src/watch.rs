//! Workspace watchers that keep a [`DiskIndex`] current for fast status.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use kelp_client::{DiskIndex, Workspace};
use kelp_workspace::META_DIR;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

const FENCE_TIMEOUT: Duration = Duration::from_secs(2);

/// Changes reported since the index was last brought up to date.
#[derive(Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    /// The watcher lost events or the ignore rules changed.
    rescan: bool,
    /// The highest fence number whose event has arrived.
    fence: u64,
}

struct Watched {
    _watcher: RecommendedWatcher,
    pending: Arc<(Mutex<Pending>, Condvar)>,
    index: Option<DiskIndex>,
}

/// The watchers of all workspaces kelpd has served.
#[derive(Default)]
pub struct Watchers {
    workspaces: Mutex<HashMap<PathBuf, Arc<Mutex<Watched>>>>,
    fences: AtomicU64,
}

impl Watchers {
    /// Returns an index of `ws`'s files that reflects every change made
    /// before the call, starting a watcher on first use.
    ///
    /// Returns `None` if the workspace cannot be watched; callers then fall
    /// back to listing the disk.
    pub fn index(&self, ws: &Workspace) -> Option<DiskIndex> {
        // Watching a projected workspace would list every projected directory.
        if ws.config().projected {
            return None;
        }
        let watched = self.watched(ws.root())?;
        let mut watched = watched.lock().expect("watch lock");
        self.fence(ws.root(), &watched.pending);
        let (paths, rescan) = {
            let mut pending = watched.pending.0.lock().expect("pending lock");
            (
                std::mem::take(&mut pending.paths),
                std::mem::take(&mut pending.rescan),
            )
        };
        match (&mut watched.index, rescan) {
            (Some(index), false) => {
                for path in paths {
                    if index.refresh(ws, &path).is_err() {
                        watched.index = None;
                        break;
                    }
                }
            }
            _ => watched.index = None,
        }
        if watched.index.is_none() {
            watched.index = DiskIndex::scan(ws).ok();
        }
        watched.index.clone()
    }

    fn watched(&self, root: &Path) -> Option<Arc<Mutex<Watched>>> {
        let mut workspaces = self.workspaces.lock().expect("watchers lock");
        if let Some(w) = workspaces.get(root) {
            return Some(w.clone());
        }
        let pending: Arc<(Mutex<Pending>, Condvar)> = Arc::default();
        let events = pending.clone();
        let meta = root.join(META_DIR);
        let ignore_file = root.join(kelp_workspace::ignore::IGNORE_FILE);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let (lock, ready) = &*events;
                let mut pending = lock.lock().expect("pending lock");
                match event {
                    Ok(event) => {
                        if event.need_rescan() {
                            pending.rescan = true;
                        }
                        for path in event.paths {
                            if let Some(n) = fence_number(&meta, &path) {
                                pending.fence = pending.fence.max(n);
                            } else if path == ignore_file {
                                pending.rescan = true;
                            } else if !path.starts_with(&meta) {
                                pending.paths.insert(path);
                            }
                        }
                    }
                    Err(_) => pending.rescan = true,
                }
                ready.notify_all();
            })
            .ok()?;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;
        let watched = Arc::new(Mutex::new(Watched {
            _watcher: watcher,
            pending,
            index: None,
        }));
        workspaces.insert(root.to_owned(), watched.clone());
        Some(watched)
    }

    /// Writes a fence file and waits until the watcher reports it, so that
    /// every earlier change has been reported too.
    fn fence(&self, root: &Path, pending: &Arc<(Mutex<Pending>, Condvar)>) {
        let n = self.fences.fetch_add(1, Ordering::Relaxed) + 1;
        let path = root.join(META_DIR).join(format!("fence-{n}"));
        if std::fs::write(&path, b"").is_err() {
            pending.0.lock().expect("pending lock").rescan = true;
            return;
        }
        let (lock, ready) = &**pending;
        let guard = lock.lock().expect("pending lock");
        let (mut guard, timeout) = ready
            .wait_timeout_while(guard, FENCE_TIMEOUT, |p| p.fence < n)
            .expect("pending lock");
        if timeout.timed_out() {
            guard.rescan = true;
        }
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
}

fn fence_number(meta: &Path, path: &Path) -> Option<u64> {
    let name = path.strip_prefix(meta).ok()?.to_str()?;
    name.strip_prefix("fence-")?.parse().ok()
}
