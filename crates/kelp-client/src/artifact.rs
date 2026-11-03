//! Publishing and fetching build artifacts.
//!
//! An artifact is a directory of files that a build produced from one
//! changelist of a stream, such as an editor build or cooked data. The server
//! keeps it as a tree like any other, so publishing only uploads chunks it
//! does not have yet.

use std::collections::BTreeMap;
use std::path::Path;

use kelp_core::{DepotPath, FileManifest};
use kelp_proto::rpc::{ArtifactInfo, FileInfo};
use kelp_proto::{Reply, Request};
use tokio::task::JoinSet;

use crate::fsutil::is_executable;
use crate::progress::Phase;
use crate::workspace::PARALLEL_INGESTS;
use crate::{Connection, ContentCache, Error, Result};

/// Which artifact [`find`] looks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactQuery {
    pub name: String,
    /// The changelist the artifact must be built from, or at or below which
    /// the newest one is wanted; the newest overall if `None`.
    pub at: Option<u64>,
    /// Whether the artifact must be built from exactly [`ArtifactQuery::at`].
    pub exact: bool,
    /// Tags the artifact must have.
    pub tags: BTreeMap<String, String>,
}

/// Publishes the files below `dir` as artifact `name` of `stream`, built from
/// changelist `cl`, and returns the artifact's id.
///
/// # Errors
///
/// Returns an error if `dir` cannot be read, has no files, or the server
/// rejects the artifact, for example because `cl` is not a changelist of
/// `stream`.
pub async fn publish(
    conn: &Connection,
    cache: &ContentCache,
    stream: &DepotPath,
    cl: u64,
    name: &str,
    tags: &BTreeMap<String, String>,
    dir: &Path,
) -> Result<u64> {
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry.map_err(|e| Error::Io(e.into()))?;
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(dir)
                .expect("walk stays below dir");
            let rel: Vec<_> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect();
            paths.push((rel.join("/"), entry.path().to_owned()));
        }
    }
    if paths.is_empty() {
        return Err(Error::Invalid(format!("{} has no files", dir.display())));
    }

    let mut ingests = JoinSet::new();
    let mut manifests: Vec<Option<(FileManifest, bool)>> = vec![None; paths.len()];
    let mut meter = cache.meter(Phase::Read, paths.len() as u64, None);
    for (i, (_, abs)) in paths.iter().enumerate() {
        if ingests.len() >= PARALLEL_INGESTS {
            let (j, done): (_, (FileManifest, _)) = ingests
                .join_next()
                .await
                .expect("ingests are pending")
                .map_err(join_error)??;
            meter.add(1, done.0.size);
            manifests[j] = Some(done);
        }
        let (cache, abs) = (cache.clone(), abs.clone());
        ingests.spawn(async move {
            let data = tokio::fs::read(&abs).await?;
            let executable = is_executable(&abs)?;
            Ok::<_, Error>((i, (cache.ingest(&data).await?, executable)))
        });
    }
    while let Some(done) = ingests.join_next().await {
        let (j, done) = done.map_err(join_error)??;
        meter.add(1, done.0.size);
        manifests[j] = Some(done);
    }
    let manifests: Vec<_> = manifests
        .into_iter()
        .map(|m| m.expect("every file is ingested"))
        .collect();
    let only: Vec<FileManifest> = manifests.iter().map(|(m, _)| m.clone()).collect();
    cache.upload(conn, &only).await?;

    let files = paths
        .into_iter()
        .zip(manifests)
        .map(|((rel, _), (m, executable))| {
            (
                rel,
                FileInfo {
                    content: m.content,
                    size: m.size,
                    executable,
                },
            )
        })
        .collect();
    let request = Request::PublishArtifact {
        stream: stream.to_string(),
        cl,
        name: name.to_owned(),
        tags: tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        files,
    };
    match conn.call(&request).await? {
        Reply::Published { id } => Ok(id),
        _ => Err(Error::UnexpectedReply),
    }
}

/// Lists artifacts of `stream` matching `query`, newest first.
///
/// # Errors
///
/// Returns an error if the server cannot be reached or the stream is not
/// visible to the user.
pub async fn list(
    conn: &Connection,
    stream: &DepotPath,
    query: &ArtifactQuery,
    limit: u32,
) -> Result<Vec<ArtifactInfo>> {
    let request = Request::FindArtifacts {
        stream: stream.to_string(),
        name: query.name.clone(),
        at: query.at,
        tags: query
            .tags
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        limit,
    };
    let Reply::Artifacts(found) = conn.call(&request).await? else {
        return Err(Error::UnexpectedReply);
    };
    Ok(found)
}

/// Returns the artifact of `stream` that `query` selects, if any.
///
/// # Errors
///
/// Returns an error if the server cannot be reached or the stream is not
/// visible to the user.
pub async fn find(
    conn: &Connection,
    stream: &DepotPath,
    query: &ArtifactQuery,
) -> Result<Option<ArtifactInfo>> {
    let found = list(conn, stream, query, 1).await?.into_iter().next();
    Ok(found.filter(|a| !query.exact || Some(a.cl) == query.at))
}

/// Describes artifact `id`.
///
/// # Errors
///
/// Returns an error if the artifact does not exist or the user may not see
/// its stream.
pub async fn get(conn: &Connection, id: u64) -> Result<ArtifactInfo> {
    match conn.call(&Request::GetArtifact { id }).await? {
        Reply::Artifacts(mut found) if found.len() == 1 => Ok(found.remove(0)),
        _ => Err(Error::UnexpectedReply),
    }
}

/// Returns the files of artifact `id`, relative to the artifact's directory.
///
/// # Errors
///
/// Returns an error if the artifact does not exist, or the server sends a
/// path that would leave the artifact's directory.
pub async fn files(conn: &Connection, id: u64) -> Result<Vec<(String, FileInfo)>> {
    let Reply::ArtifactFiles(files) = conn.call(&Request::ArtifactFiles { id }).await? else {
        return Err(Error::UnexpectedReply);
    };
    if let Some((bad, _)) = files.iter().find(|(p, _)| !is_safe(p)) {
        return Err(Error::Invalid(format!(
            "artifact {id} has an invalid path {bad:?}"
        )));
    }
    Ok(files)
}

/// Downloads artifact `id` into `dest` and returns its files.
///
/// Existing files are replaced; other files in `dest` are left alone.
///
/// # Errors
///
/// Returns an error if the artifact does not exist or a file cannot be
/// written.
pub async fn fetch(
    conn: &Connection,
    cache: &ContentCache,
    id: u64,
    dest: &Path,
) -> Result<Vec<(String, FileInfo)>> {
    let files = files(conn, id).await?;
    let contents: Vec<_> = files.iter().map(|(_, f)| f.content).collect();
    cache.fetch(conn, &contents).await?;
    std::fs::create_dir_all(dest)?;
    let tmp = crate::cache::unique_temp(dest);
    let bytes = files.iter().map(|(_, f)| f.size).sum();
    let mut meter = cache.meter(Phase::Write, files.len() as u64, Some(bytes));
    for (rel, file) in &files {
        meter.add(1, file.size);
        cache
            .materialize(&file.content, file.executable, &dest.join(rel), &tmp)
            .await?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(files)
}

/// Returns `true` if `path` is a relative path that stays below its base.
pub(crate) fn is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains(':')
        && path.split('/').all(|s| !matches!(s, "" | "." | ".."))
}

fn join_error(e: tokio::task::JoinError) -> Error {
    Error::Invalid(format!("ingest task failed: {e}"))
}
