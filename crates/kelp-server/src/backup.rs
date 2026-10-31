//! Backing up a server, restoring it, and moving it to another machine.
//!
//! A backup is itself a data directory: `meta.redb`, `chunks/`, `tls/` and
//! `server.toml`. Restoring copies it into place and checks it with
//! [`fsck`](crate::fsck::fsck), and a server can also be started on the
//! backup directly. Because the TLS identity is part of the backup, clients
//! keep trusting the restored server and only need its new address.
//!
//! Backing up into the same directory again is incremental: chunks already
//! there are not copied again. The metadata is replaced last, so an
//! interrupted backup leaves the previous one usable.

use std::collections::HashSet;
use std::path::Path;

use kelp_cas::{BlobStore, PackStore};
use kelp_core::ChunkId;
use kelp_meta::MetaDb;

use crate::fsck::{FsckReport, fsck};
use crate::{Blobs, Error, Result, StorageConfig, StorageKind};

/// What [`backup`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackupReport {
    /// The last log entry the backup contains.
    pub log_index: u64,
    /// Chunks copied in this run.
    pub chunks_copied: u64,
    /// Chunks the backup already had.
    pub chunks_present: u64,
    pub bytes_copied: u64,
    /// Whether chunks were left in the server's object storage.
    pub metadata_only: bool,
}

/// Writes a backup of the server whose data directory is `data_dir` into
/// `dest`, taking a consistent snapshot of `db` and copying the chunks every
/// file still in use needs from `blobs`.
///
/// With `metadata_only`, chunks are not copied and the backup keeps the
/// server's storage settings; this suits servers whose chunks are in an
/// object store with its own backups. Otherwise the backup stores the chunks
/// itself, in local storage.
///
/// The caller must keep garbage collection from deleting chunks while this
/// runs.
///
/// # Errors
///
/// Returns an error if a store cannot be read or `dest` cannot be written,
/// or [`Error::Config`] if a chunk the snapshot needs is missing.
pub async fn backup(
    db: &MetaDb,
    blobs: &Blobs,
    data_dir: &Path,
    dest: &Path,
    metadata_only: bool,
) -> Result<BackupReport> {
    std::fs::create_dir_all(dest)?;
    let staged = dest.join("meta.redb.new");
    if staged.exists() {
        std::fs::remove_file(&staged)?;
    }
    db.snapshot_to(&staged)?;
    let snapshot = MetaDb::open(&staged)?;
    let mut report = BackupReport {
        log_index: snapshot.read()?.last_index()?.0,
        metadata_only,
        ..BackupReport::default()
    };

    if !metadata_only {
        let reader = snapshot.read()?;
        let mut chunks: HashSet<ChunkId> = HashSet::new();
        for content in reader.live_contents()? {
            if let Some(manifest) = reader.manifest(&content)? {
                chunks.extend(manifest.chunks.iter().map(|c| c.id));
            }
        }
        drop(reader);
        let target = PackStore::open(dest.join("chunks"))?;
        let mut chunks: Vec<ChunkId> = chunks.into_iter().collect();
        chunks.sort();
        for batch in chunks.chunks(1024) {
            let present = target.has(batch).await?;
            for (id, present) in batch.iter().zip(present) {
                if present {
                    report.chunks_present += 1;
                    continue;
                }
                let data = blobs.get(*id).await.map_err(|e| match e {
                    kelp_cas::Error::NotFound(_) => {
                        Error::Config(format!("chunk {id} is missing from the server; run fsck"))
                    }
                    e => e.into(),
                })?;
                report.bytes_copied += data.len() as u64;
                report.chunks_copied += 1;
                target.put(*id, data).await?;
            }
            target.sync().await?;
        }
    }
    drop(snapshot);

    let tls = data_dir.join("tls");
    if tls.is_dir() {
        std::fs::create_dir_all(dest.join("tls"))?;
        for entry in std::fs::read_dir(&tls)? {
            let entry = entry?;
            std::fs::copy(entry.path(), dest.join("tls").join(entry.file_name()))?;
        }
    }
    write_settings(data_dir, dest, metadata_only)?;
    std::fs::rename(&staged, dest.join("meta.redb"))?;
    sync_dir(dest)?;
    Ok(report)
}

/// Opens the stores of the data directory `data_dir` while no server uses
/// it.
///
/// # Errors
///
/// Returns [`Error::NotInitialised`] if there is no database, or
/// [`Error::Config`] if a running server holds it.
pub fn open_offline(data_dir: &Path) -> Result<(MetaDb, Blobs)> {
    let path = data_dir.join("meta.redb");
    if !path.is_file() {
        return Err(Error::NotInitialised(data_dir.to_owned()));
    }
    let db = MetaDb::open(&path).map_err(|e| match e {
        kelp_meta::Error::Db(m) if m.to_lowercase().contains("already open") => Error::Config(
            "a server is using this data directory; stop it, or use `kelp backup` / `kelp fsck` while it runs"
                .into(),
        ),
        e => e.into(),
    })?;
    let blobs = Blobs::open(&StorageConfig::load(data_dir)?, &data_dir.join("chunks"))?;
    Ok((db, blobs))
}

/// Copies `server.toml`, switching storage to local if the backup holds the
/// chunks itself.
fn write_settings(data_dir: &Path, dest: &Path, metadata_only: bool) -> Result<()> {
    let text = match std::fs::read_to_string(data_dir.join("server.toml")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut settings: toml::Table =
        toml::from_str(&text).map_err(|e| Error::Config(format!("server.toml: {e}")))?;
    if !metadata_only {
        settings.remove("storage");
    }
    let text = toml::to_string_pretty(&settings).expect("TOML tables serialize");
    std::fs::write(dest.join("server.toml"), text)?;
    Ok(())
}

/// Copies the backup `from` into the new data directory `data_dir` and
/// checks it. With `new_identity`, the TLS identity is left behind, so the
/// server gets a new certificate and clients must trust it again.
///
/// # Errors
///
/// Returns [`Error::AlreadyInitialised`] if `data_dir` is not empty,
/// [`Error::Config`] if `from` is not a backup, or an error if copying
/// fails. A restored directory that fails its check is reported, not
/// removed.
pub async fn restore(from: &Path, data_dir: &Path, new_identity: bool) -> Result<FsckReport> {
    if !from.join("meta.redb").is_file() {
        return Err(Error::Config(format!(
            "{} is not a Kelp backup",
            from.display()
        )));
    }
    if std::fs::read_dir(data_dir).is_ok_and(|mut d| d.next().is_some()) {
        return Err(Error::AlreadyInitialised(data_dir.to_owned()));
    }
    copy_tree(from, data_dir, &|name| {
        name == "meta.redb.new" || (new_identity && name == "tls")
    })?;
    sync_dir(data_dir)?;
    let db = MetaDb::open(&data_dir.join("meta.redb"))?;
    let storage = StorageConfig::load(data_dir)?;
    let blobs = Blobs::open(&storage, &data_dir.join("chunks"))?;
    if storage.kind != StorageKind::Local {
        eprintln!(
            "note: chunks stay in the {:?} storage the backup refers to",
            storage.kind
        );
    }
    fsck(&db, &blobs, false).await
}

fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&str) -> bool) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip(&name.to_string_lossy()) {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target, &|_| false)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
            // Flushing needs write access on Windows.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&target)?
                .sync_all()?;
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    let _ = dir;
    Ok(())
}
