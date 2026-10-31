//! `kelp artifact`: publishing, listing and fetching build artifacts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Subcommand;
use kelp_client::artifact::{self, ArtifactQuery};
use kelp_client::{Home, Workspace};
use kelp_core::DepotPath;
use kelp_proto::Request;

use crate::util::connect;

#[derive(Debug, Subcommand)]
pub enum ArtifactCmd {
    /// Publishes a directory as an artifact.
    ///
    /// Publishes the files below the directory as an artifact built from a
    /// changelist.
    Publish {
        /// The directory whose files make up the artifact.
        dir: PathBuf,
        /// The artifact name, such as `editor-win64`.
        #[arg(long)]
        name: String,
        /// The changelist the files were built from; defaults to the one the
        /// workspace is synced to.
        #[arg(long)]
        cl: Option<u64>,
        /// A `key=value` tag, such as `platform=win64`; may be repeated.
        #[arg(long = "tag", value_parser = parse_tag)]
        tags: Vec<(String, String)>,
    },
    /// Lists artifacts, newest first.
    List {
        #[arg(long)]
        name: String,
        /// Lists only artifacts built at or below this changelist.
        #[arg(long)]
        at: Option<u64>,
        #[arg(long = "tag", value_parser = parse_tag)]
        tags: Vec<(String, String)>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Downloads an artifact into a directory.
    Get {
        /// The artifact id, as listed.
        id: u64,
        dir: PathBuf,
    },
    /// Keeps an artifact regardless of retention (administrators only).
    Pin {
        id: u64,
        /// Removes the pin instead.
        #[arg(long)]
        unpin: bool,
    },
    /// Deletes an artifact (administrators only).
    Delete { id: u64 },
    /// Keeps only the newest artifacts of a name (administrators only).
    Retain {
        #[arg(long)]
        name: String,
        /// How many to keep; all are kept if omitted.
        #[arg(long)]
        keep: Option<u32>,
    },
}

/// Runs `command`; `stream` defaults to the stream of the workspace around `cwd`.
pub async fn run(
    home: &Home,
    cwd: &Path,
    server: String,
    stream: Option<String>,
    command: ArtifactCmd,
) -> anyhow::Result<()> {
    let ws = Workspace::discover(cwd).ok();
    let stream = || -> anyhow::Result<DepotPath> {
        match (&stream, &ws) {
            (Some(s), _) => Ok(DepotPath::parse(s)?),
            (None, Some(ws)) => Ok(ws.config().stream_path()?),
            (None, None) => bail!("not in a workspace; pass --stream"),
        }
    };
    let conn = connect(home, &server).await?;
    match command {
        ArtifactCmd::Publish {
            dir,
            name,
            cl,
            tags,
        } => {
            let cl = match (cl, &ws) {
                (Some(cl), _) => cl,
                (None, Some(ws)) if ws.state().have_cl > 0 => ws.state().have_cl,
                _ => bail!("pass --cl with the changelist the files were built from"),
            };
            let stream = stream()?;
            let tags: BTreeMap<_, _> = tags.into_iter().collect();
            let dir = cwd.join(dir);
            let id = artifact::publish(&conn, &home.cache(), &stream, cl, &name, &tags, &dir)
                .await
                .with_context(|| format!("publishing {}", dir.display()))?;
            println!("Published artifact {id}: {name} of {stream}@{cl}.");
        }
        ArtifactCmd::List {
            name,
            at,
            tags,
            limit,
        } => {
            let query = ArtifactQuery {
                name,
                at,
                exact: false,
                tags: tags.into_iter().collect(),
            };
            for a in artifact::list(&conn, &stream()?, &query, limit).await? {
                let tags: Vec<_> = a.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
                println!(
                    "{:>6}  @{:<8} {:>6} files {:>12} bytes  {:<12}{}{}",
                    a.id,
                    a.cl,
                    a.files,
                    a.bytes,
                    a.by,
                    tags.join(" "),
                    if a.pinned { "  (pinned)" } else { "" }
                );
            }
        }
        ArtifactCmd::Get { id, dir } => {
            let files = artifact::fetch(&conn, &home.cache(), id, &cwd.join(&dir)).await?;
            println!("Wrote {} files to {}.", files.len(), dir.display());
        }
        ArtifactCmd::Pin { id, unpin } => {
            conn.call(&Request::PinArtifact { id, pinned: !unpin })
                .await?;
        }
        ArtifactCmd::Delete { id } => {
            conn.call(&Request::DeleteArtifact { id }).await?;
        }
        ArtifactCmd::Retain { name, keep } => {
            let request = Request::SetArtifactRetention {
                stream: stream()?.to_string(),
                name,
                keep_last: keep,
            };
            conn.call(&request).await?;
        }
    }
    Ok(())
}

fn parse_tag(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!("expected key=value, got {s:?}")),
    }
}
