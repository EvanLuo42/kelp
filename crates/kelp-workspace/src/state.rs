//! Workspace configuration and state persisted under [`META_DIR`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use kelp_core::{ContentId, DepotPath, TreeId};

use serde::{Deserialize, Serialize};

use crate::{Error, PendingChanges, Result, View};

/// The directory at the workspace root that holds Kelp's files.
pub const META_DIR: &str = ".kelp";

const CONFIG_FILE: &str = "config.toml";
const STATE_FILE: &str = "state.bin";

/// The bytes `state.bin` starts with, before the format version.
const STATE_MAGIC: &[u8; 8] = b"KELPWS\0\0";

/// The version of the workspace state format this build reads and writes.
///
/// Every change to how [`State`] is encoded must increase it and add a step
/// to [`MIGRATIONS`].
pub const STATE_VERSION: u32 = 3;

/// Converts encoded state from one version to the next.
type Migration = fn(Vec<u8>) -> Result<Vec<u8>>;

/// The migration from version `i + 1` to `i + 2` is at index `i`.
const MIGRATIONS: &[Migration] = &[
    // 2 appends `moves`; an empty map encodes as one zero byte.
    |mut payload| {
        payload.push(0);
        Ok(payload)
    },
    // 3 appends `snapshot_moves`, likewise empty.
    |mut payload| {
        payload.push(0);
        Ok(payload)
    },
];

/// The settings of a workspace, stored as TOML so that users can edit them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The workspace name, unique per user. Exclusive locks are held by a
    /// user in a named workspace.
    pub name: String,
    /// The server address, as `host:port`.
    pub server: String,
    /// The stream the workspace is bound to, such as `//game/main`.
    pub stream: String,
    /// The view in its textual form; see [`View`].
    pub view: String,
    /// Whether the workspace is projected: files appear on sync and download
    /// on first use. Projected workspaces need kelpd to be running.
    #[serde(default)]
    pub projected: bool,
    /// Build artifacts that sync places in the workspace.
    #[serde(default, rename = "artifact", skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactMount>,
}

/// A build artifact that [`Config`] asks sync to place in the workspace.
///
/// In `config.toml`:
///
/// ```toml
/// [[artifact]]
/// name = "editor-win64"
/// to = "Binaries"
/// tags = { config = "development" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMount {
    /// The artifact name, such as `editor-win64`.
    pub name: String,
    /// The workspace directory the artifact's files are written to, relative
    /// to the root with `/` separators. Kelp owns this directory: it is
    /// excluded from status and its files are read-only.
    pub to: String,
    /// The stream that publishes the artifact; defaults to the workspace's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    /// Whether the artifact must have been built from exactly the synced
    /// changelist. Otherwise the newest one built at or below it is used.
    #[serde(default)]
    pub exact: bool,
    /// Tags the artifact must have.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl ArtifactMount {
    /// Checks that [`ArtifactMount::to`] is a relative path below the
    /// workspace root, outside [`META_DIR`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArtifactMount`] if it is not.
    pub fn validate(&self) -> Result<()> {
        let bad = self.to.is_empty()
            || self.to.starts_with('/')
            || self.to.contains('\\')
            || self.to.split('/').any(|s| matches!(s, "" | "." | ".."))
            || self
                .to
                .split('/')
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case(META_DIR));
        match bad {
            true => Err(Error::InvalidArtifactMount(self.to.clone())),
            false => Ok(()),
        }
    }
}

impl Config {
    /// Creates a configuration for the workspace `name` whose view maps all of
    /// `stream` onto the workspace root.
    pub fn new(name: impl Into<String>, server: impl Into<String>, stream: &DepotPath) -> Self {
        Self {
            name: name.into(),
            server: server.into(),
            stream: stream.to_string(),
            view: format!("{stream}/... ..."),
            projected: false,
            artifacts: Vec::new(),
        }
    }

    /// Returns `true` if `root` holds a workspace configuration. A bare
    /// [`META_DIR`] is not enough: the installer puts Kelp itself in
    /// `~/.kelp`, which would make the home directory look like a workspace.
    pub fn exists(root: &Path) -> bool {
        meta_path(root, CONFIG_FILE).is_file()
    }

    /// Reads the configuration of the workspace rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(root: &Path) -> Result<Self> {
        let path = meta_path(root, CONFIG_FILE);
        let text = fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|e| Error::Corrupt(path, e.to_string()))
    }

    /// Writes the configuration of the workspace rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, root: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).expect("config serializes to TOML");
        write_atomic(&meta_path(root, CONFIG_FILE), text.as_bytes())
    }

    /// Parses [`Config::stream`].
    ///
    /// # Errors
    ///
    /// Returns an error if the stream is not a valid depot path.
    pub fn stream_path(&self) -> Result<DepotPath> {
        Ok(DepotPath::parse(&self.stream)?)
    }

    /// Parses [`Config::view`].
    ///
    /// # Errors
    ///
    /// Returns an error if the view is malformed.
    pub fn view(&self) -> Result<View> {
        View::parse(&self.view)
    }

    /// Checks the view and the artifact mounts.
    ///
    /// # Errors
    ///
    /// Returns an error if the stream, the view or an artifact mount is
    /// invalid.
    pub fn validate(&self) -> Result<()> {
        self.stream_path()?;
        self.view()?;
        self.artifacts
            .iter()
            .try_for_each(ArtifactMount::validate)?;
        let dirs: Vec<String> = self
            .artifacts
            .iter()
            .map(|m| format!("{}/", m.to.to_lowercase()))
            .collect();
        for (i, a) in dirs.iter().enumerate() {
            if dirs[i + 1..]
                .iter()
                .any(|b| a.starts_with(b.as_str()) || b.starts_with(a.as_str()))
            {
                return Err(Error::InvalidArtifactMount(format!(
                    "{} overlaps another artifact directory",
                    self.artifacts[i].to
                )));
            }
        }
        Ok(())
    }
}

/// A file the workspace has, as last synced or submitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaveEntry {
    /// The workspace path, relative to the root with `/` separators.
    pub local: String,
    pub content: ContentId,
    pub size: u64,
    pub executable: bool,
    /// The modification time Kelp observed after writing the file, in
    /// nanoseconds since the Unix epoch. A file whose size and modification
    /// time still match is assumed unchanged without hashing it.
    pub mtime: u64,
}

/// The content and attributes of one version of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    pub content: ContentId,
    pub size: u64,
    pub executable: bool,
}

/// Where a pending resolve came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolveKind {
    /// Sync found a newer revision of an opened file. Resolving makes the
    /// newer revision the file's base.
    Sync,
    /// A merge brought in a change to a file that was also changed in this
    /// stream. Resolving decides the content to submit; the base stays.
    Merge,
}

/// Two lines of changes to one file that must be reconciled before submit.
///
/// "Yours" is always the file in the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolve {
    pub kind: ResolveKind,
    /// The common ancestor, or `None` if the file did not exist.
    pub base: Option<FileVersion>,
    /// The incoming version, or `None` if it deletes the file.
    pub theirs: Option<FileVersion>,
}

/// One file captured by a [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub path: DepotPath,
    /// How the file was opened.
    pub action: crate::OpenAction,
    /// The working content, or `None` if the file was absent from disk.
    pub content: Option<FileVersion>,
}

/// A saved copy of the opened files of a pending changelist.
///
/// Snapshot content lives in the local content cache, so a snapshot can be
/// restored offline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u32,
    pub cl: kelp_core::LocalClId,
    pub message: String,
    /// Milliseconds since the Unix epoch.
    pub at: u64,
    /// Whether Kelp took the snapshot before an operation that overwrites work.
    pub automatic: bool,
    pub files: Vec<SnapshotFile>,
}

/// What a workspace has and what it has opened.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// The stream tree the workspace was last synced to.
    pub have_root: Option<TreeId>,
    /// The changelist of [`State::have_root`].
    pub have_cl: u64,
    /// The files the workspace has.
    pub have: BTreeMap<DepotPath, HaveEntry>,
    /// Files whose [`HaveEntry`] may differ from [`State::have_root`], because
    /// they were submitted or skipped since the last sync.
    pub dirty: BTreeSet<DepotPath>,
    pub pending: PendingChanges,
    /// Files that need [`Resolve`] before they can be submitted.
    pub resolves: BTreeMap<DepotPath, Resolve>,
    /// The source of the depot's typemap as last fetched, if it has one.
    pub typemap: Option<String>,
    /// Snapshots of pending changelists, oldest first.
    pub snapshots: Vec<Snapshot>,
    /// The artifacts in the workspace, by [`ArtifactMount::to`].
    pub artifacts: BTreeMap<String, ArtifactHave>,
    /// Opened moves: each file opened for [`OpenAction::MoveTo`] and the
    /// file opened for [`OpenAction::MoveFrom`] it comes from.
    ///
    /// [`OpenAction::MoveTo`]: crate::OpenAction::MoveTo
    /// [`OpenAction::MoveFrom`]: crate::OpenAction::MoveFrom
    pub moves: BTreeMap<DepotPath, DepotPath>,
    /// The opened moves among the files of each snapshot, by
    /// [`Snapshot::id`], as in [`State::moves`]. Snapshots without moves,
    /// and those taken before Kelp recorded this, have no entry.
    pub snapshot_moves: BTreeMap<u32, BTreeMap<DepotPath, DepotPath>>,
}

/// A build artifact the workspace has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactHave {
    pub id: u64,
    /// The changelist the artifact was built from.
    pub cl: u64,
    /// The artifact's files, relative to its directory.
    pub files: BTreeMap<String, FileVersion>,
}

impl State {
    /// Reads the state of the workspace rooted at `root`, or returns an empty
    /// state if none has been saved.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or decoded.
    pub fn load(root: &Path) -> Result<Self> {
        let path = meta_path(root, STATE_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let corrupt = |why: String| Error::Corrupt(path.clone(), why);
                let Some(rest) = bytes.strip_prefix(STATE_MAGIC.as_slice()) else {
                    return Err(corrupt(
                        "written by a development build of Kelp without format versioning; \
                         create the workspace again"
                            .into(),
                    ));
                };
                let (version, payload) = rest
                    .split_at_checked(4)
                    .ok_or_else(|| corrupt("truncated".into()))?;
                let version = u32::from_le_bytes(version.try_into().expect("four bytes"));
                if version == 0 || version > STATE_VERSION {
                    return Err(corrupt(format!(
                        "state format {version} is newer than this build understands ({STATE_VERSION}); \
                         upgrade Kelp"
                    )));
                }
                let mut payload = payload.to_vec();
                for step in &MIGRATIONS[version as usize - 1..] {
                    payload = step(payload)?;
                }
                postcard::from_bytes(&payload).map_err(|e| corrupt(e.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns when the state of the workspace rooted at `root` was last
    /// saved, in nanoseconds since the Unix epoch, or `None` if it never was.
    ///
    /// This is the modification time of the state file, so it comes from the
    /// same clock, at the same granularity, as the times of the files in the
    /// workspace.
    pub fn saved_at(root: &Path) -> Option<u64> {
        let modified = fs::metadata(meta_path(root, STATE_FILE))
            .and_then(|m| m.modified())
            .ok()?;
        let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(since.as_nanos() as u64)
    }

    /// Writes the state of the workspace rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, root: &Path) -> Result<()> {
        let mut bytes = STATE_MAGIC.to_vec();
        bytes.extend_from_slice(&STATE_VERSION.to_le_bytes());
        bytes.extend(postcard::to_allocvec(self).expect("state serializes"));
        write_atomic(&meta_path(root, STATE_FILE), &bytes)
    }
}

fn meta_path(root: &Path, file: &str) -> PathBuf {
    root.join(META_DIR).join(file)
}

/// Writes `bytes` to a temporary file next to `path` and renames it into place,
/// so that readers never observe a partially written file.
///
/// The data is flushed to disk before the rename, and the rename before
/// returning, so that a crash leaves either the old or the new file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().expect("workspace files live in a directory");
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
