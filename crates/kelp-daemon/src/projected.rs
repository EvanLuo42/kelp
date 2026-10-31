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
    pub fn mount(&self, root: &Path, upper: &Path) -> io::Result<()> {
        let mut mounted = self.mounted.lock().expect("mounts lock");
        if mounted.contains_key(root) {
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
        let canonical = std::fs::canonicalize(root)?;
        let live = Arc::new(AtomicBool::new(true));
        let projector = Arc::new(Adapter(mount.projection(), live.clone()));
        self.session.set_projector(&canonical, Some(projector));
        mounted.insert(root.to_owned(), (mount, live));
        drop(mounted);

        let mut registry = self.registry();
        let entry = Entry {
            root: root.to_owned(),
            upper: upper.to_owned(),
        };
        if !registry.workspaces.contains(&entry) {
            registry.workspaces.push(entry);
            self.save(&registry)?;
        }
        Ok(())
    }

    /// Unmounts the workspace at `root`. It stays registered.
    pub fn unmount(&self, root: &Path) -> io::Result<()> {
        if let Ok(canonical) = std::fs::canonicalize(root) {
            self.session.set_projector(&canonical, None);
        }
        let removed = self.mounted.lock().expect("mounts lock").remove(root);
        let (mount, live) =
            removed.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not mounted"))?;
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
