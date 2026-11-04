//! Mounting projected workspaces and keeping track of them.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kelp_client::ops::Session;
use kelp_client::{ProjectedFile, Projector, Workspace};
use kelp_core::typemap::LockPolicy;
use kelp_core::{ContentId, Hash};
use kelp_vfs::{Hydrator, LowerFile, LowerTree, MountConfig, Mounted, Projection};
use serde::{Deserialize, Serialize};

/// Returns the current time in nanoseconds since the Unix epoch.
///
/// A projected file reports the time it was synced as its modification
/// time, so build tools that compare timestamps see synced files as new.
/// Sync records the same time, which lets status recognize an unchanged
/// file without reading it.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default, rename = "workspace")]
    workspaces: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    root: PathBuf,
    upper: PathBuf,
}

/// The projected workspaces kelpd has mounted.
pub struct Mounts {
    registry_path: PathBuf,
    session: Arc<Session>,
    runtime: tokio::runtime::Handle,
    /// Each mount, and whether it is still mounted for the projectors handed
    /// out for it.
    mounted: Mutex<HashMap<PathBuf, (Mounted, Arc<AtomicBool>)>>,
}

impl Mounts {
    pub fn new(session: Arc<Session>, runtime: tokio::runtime::Handle) -> Self {
        let registry_path = session.home().dir().join("projections.toml");
        Self {
            registry_path,
            session,
            runtime,
            mounted: Mutex::default(),
        }
    }

    /// Returns `true` if any workspace is mounted.
    pub fn any(&self) -> bool {
        !self.mounted.lock().expect("mounts lock").is_empty()
    }

    /// Mounts every registered workspace, logging failures.
    pub fn mount_registered(&self) {
        for entry in self.registry().workspaces {
            match self.mount(&entry.root, &entry.upper) {
                Ok(()) => tracing::info!("mounted {}", entry.root.display()),
                Err(e) => tracing::warn!("cannot mount {}: {e}", entry.root.display()),
            }
        }
    }

    /// Mounts the workspace whose files live in `upper` at `root`, and
    /// registers it so that kelpd mounts it again after a restart.
    ///
    /// Mounts are known by the canonical form of `root`, the same path the
    /// session finds the workspace by, however `root` was spelled.
    pub fn mount(&self, root: &Path, upper: &Path) -> io::Result<()> {
        let canonical = std::fs::canonicalize(root)?;
        let mut mounted = self.mounted.lock().expect("mounts lock");
        if mounted.contains_key(&canonical) {
            return Ok(());
        }
        let ws = Workspace::open(upper).map_err(io::Error::other)?;
        let mut lower = LowerTree::new();
        for (path, have) in &ws.state().have {
            let read_only = ws.file_type(path).lock == LockPolicy::Exclusive
                && ws.state().pending.action(path).is_none();
            let file = LowerFile {
                content: have.content.0.0,
                size: have.size,
                executable: have.executable,
                read_only,
                mtime: if have.mtime == 0 { now() } else { have.mtime },
            };
            lower.insert(&have.local, file);
        }
        let hydrator = Arc::new(ContentHydrator {
            session: self.session.clone(),
            server: ws.config().server.clone(),
            runtime: self.runtime.clone(),
        });
        let config = MountConfig {
            mountpoint: root.to_owned(),
            upper: upper.to_owned(),
            lower,
            hydrator,
        };
        let mount = kelp_vfs::mount(config)?;
        let live = Arc::new(AtomicBool::new(true));
        let projector = Arc::new(Adapter(mount.projection(), live.clone()));
        self.session.set_projector(&canonical, Some(projector));
        mounted.insert(canonical.clone(), (mount, live));
        drop(mounted);

        // Registries written before mounts were known by canonical paths
        // may spell the same workspace differently.
        let mut registry = self.registry();
        let known = registry
            .workspaces
            .iter()
            .any(|e| e.upper == upper && same_path(&e.root, &canonical));
        if !known {
            registry.workspaces.push(Entry {
                root: root.to_owned(),
                upper: upper.to_owned(),
            });
            self.save(&registry)?;
        }
        Ok(())
    }

    /// Unmounts the workspace at or around `dir`. It stays registered.
    pub fn unmount(&self, dir: &Path) -> io::Result<()> {
        let dir = std::fs::canonicalize(dir)?;
        let mut mounted = self.mounted.lock().expect("mounts lock");
        // The deepest mount containing `dir`, so that unmounting works from
        // anywhere in the workspace.
        let root = mounted
            .keys()
            .filter(|root| dir.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not mounted"))?;
        let (mount, live) = mounted.remove(&root).expect("mount found above");
        drop(mounted);
        self.session.set_projector(&root, None);
        live.store(false, Ordering::SeqCst);
        drop(mount);
        Ok(())
    }

    /// Unmounts everything.
    pub fn unmount_all(&self) {
        for (_, (mount, live)) in self.mounted.lock().expect("mounts lock").drain() {
            live.store(false, Ordering::SeqCst);
            drop(mount);
        }
    }

    fn registry(&self) -> Registry {
        std::fs::read_to_string(&self.registry_path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, registry: &Registry) -> io::Result<()> {
        let text = toml::to_string_pretty(registry).map_err(io::Error::other)?;
        let tmp = self.registry_path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.registry_path)
    }
}

/// Returns whether `path` names `canonical`, or did when it still existed.
fn same_path(path: &Path, canonical: &Path) -> bool {
    std::fs::canonicalize(path).map_or(path == canonical, |p| p == canonical)
}

/// Fetches content from the local cache, or the server when it is missing.
struct ContentHydrator {
    session: Arc<Session>,
    server: String,
    runtime: tokio::runtime::Handle,
}

impl Hydrator for ContentHydrator {
    fn hydrate(&self, _local: &str, file: &LowerFile, out: &mut dyn Write) -> io::Result<()> {
        let content = ContentId(Hash(file.content));
        let cache = self.session.home().cache();
        let data = self.runtime.block_on(async {
            if let Ok(data) = cache.read(&content).await {
                return Ok(data);
            }
            let conn = self.session.connection(&self.server).await?;
            cache.fetch(&conn, &[content]).await?;
            cache.read(&content).await
        });
        out.write_all(&data.map_err(io::Error::other)?)
    }
}

/// A mount's projection, refusing to change anything once the workspace
/// is unmounted: an operation that started before would otherwise see every
/// file as missing.
struct Adapter(Arc<dyn Projection>, Arc<AtomicBool>);

impl Projector for Adapter {
    fn project(&self, local: &str, file: Option<ProjectedFile>) -> kelp_client::Result<bool> {
        if !self.1.load(Ordering::SeqCst) {
            return Err(kelp_client::Error::Invalid(
                "the projected workspace was unmounted during the operation".into(),
            ));
        }
        let file = file.map(|f| LowerFile {
            content: f.version.content.0.0,
            size: f.version.size,
            executable: f.version.executable,
            read_only: f.read_only,
            mtime: now(),
        });
        Ok(self.0.project(local, file)?)
    }
}
