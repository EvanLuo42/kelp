//! `kelp issue`: where the changes linked to issues have gone.

use crate::util::unexpected;
use anyhow::bail;
use clap::Subcommand;
use kelp_client::{Connection, Workspace, artifact};
use kelp_proto::{Reply, Request};

#[derive(Debug, Subcommand)]
pub enum IssueCmd {
    /// Shows where an issue's changes have gone.
    ///
    /// Lists the changes linked to an issue, such as `PROJ-123`, and the
    /// streams they have reached.
    Show { key: String },
    /// Links a submitted changelist to issues, or unlinks it.
    Link {
        cl: u64,
        #[arg(long)]
        add: Vec<String>,
        #[arg(long)]
        remove: Vec<String>,
    },
    /// Lists the issues in a stream or artifact.
    ///
    /// Lists the issues whose changes a stream contains, or a build artifact
    /// was built from, optionally only those added since an earlier point.
    In {
        /// The stream; defaults to the workspace's.
        #[arg(long, conflicts_with = "artifact")]
        stream: Option<String>,
        /// The changelist of the stream; defaults to its head.
        #[arg(long, conflicts_with = "artifact")]
        at: Option<u64>,
        /// An earlier changelist of the stream, or with `--artifact` an
        /// earlier artifact id.
        #[arg(long)]
        since: Option<u64>,
        /// An artifact id; lists the issues of the changes it was built from.
        #[arg(long)]
        artifact: Option<u64>,
    },
}

pub async fn run(
    conn: &Connection,
    cwd: &std::path::Path,
    command: IssueCmd,
) -> anyhow::Result<()> {
    match command {
        IssueCmd::Show { key } => {
            let Reply::Issue(d) = conn.call(&Request::GetIssue { key }).await? else {
                return Err(unexpected());
            };
            println!("{}", d.key);
            if !d.changes.is_empty() {
                println!("\nchanges:");
                for c in &d.changes {
                    let fixes = if c.fixes.contains(&d.key) {
                        " (fixes)"
                    } else {
                        ""
                    };
                    let title = c.description.lines().next().unwrap_or_default();
                    println!(
                        "  {:>6}  {:<20} {:<12}{fixes} {title}",
                        c.cl, c.stream, c.author
                    );
                }
            }
            if !d.reached.is_empty() {
                println!("\nreached:");
                for (stream, cl) in &d.reached {
                    match cl {
                        0 => println!("  {stream:<24} when it was created"),
                        cl => println!("  {stream:<24} in change {cl}"),
                    }
                }
            }
        }
        IssueCmd::Link { cl, add, remove } => {
            conn.call(&Request::LinkIssues { cl, add, remove }).await?;
        }
        IssueCmd::In {
            since,
            artifact: Some(id),
            ..
        } => {
            let built = artifact::get(conn, id).await?;
            let since = match since {
                Some(earlier) => {
                    let other = artifact::get(conn, earlier).await?;
                    if !other.stream.eq_ignore_ascii_case(&built.stream) {
                        bail!(
                            "artifact {earlier} was built from {}, not {}",
                            other.stream,
                            built.stream
                        );
                    }
                    Some(other.cl)
                }
                None => None,
            };
            print_issues(conn, built.stream, Some(built.cl), since).await?;
        }
        IssueCmd::In {
            stream,
            at,
            since,
            artifact: None,
        } => {
            let stream = match stream {
                Some(s) => s,
                None => Workspace::discover(cwd)?.config().stream.clone(),
            };
            print_issues(conn, stream, at, since).await?;
        }
    }
    Ok(())
}

async fn print_issues(
    conn: &Connection,
    stream: String,
    at: Option<u64>,
    since: Option<u64>,
) -> anyhow::Result<()> {
    let Reply::IssueKeys(issues) = conn.call(&Request::IssuesIn { stream, at, since }).await?
    else {
        return Err(unexpected());
    };
    for (key, cls) in issues {
        let cls: Vec<String> = cls.iter().map(ToString::to_string).collect();
        println!("{key:<16} changes {}", cls.join(", "));
    }
    Ok(())
}
