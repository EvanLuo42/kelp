//! Commands that run inside a workspace.

use std::path::{Path, PathBuf};

use kelp_client::ops::{Op, Outcome, Revision};
use kelp_client::{
    Home, ResolveMode, ResolveOutcome, SkipReason, SnapshotSide, Workspace, render_diff,
};
use kelp_core::{DepotPath, LocalClId};
use kelp_proto::{Reply, Request};
use kelp_workspace::OpenAction;

use crate::util::{
    action_name, connect, file_action, notice, parse_cl, parse_cl_or_default, unexpected,
};
use crate::{Accept, ClCmd, Cmd, SnapshotCmd, daemon};

pub async fn run(home: &Home, cwd: &Path, command: Cmd, no_daemon: bool) -> anyhow::Result<()> {
    let abs =
        |paths: Vec<PathBuf>| -> Vec<PathBuf> { paths.into_iter().map(|p| cwd.join(p)).collect() };
    let mut diff_tool = None;
    let op = match command {
        Cmd::Sync { targets } => {
            let mut rev = None;
            let mut paths = Vec::new();
            for target in targets {
                match target.strip_prefix('@') {
                    Some(r) => {
                        rev = Some(match r.parse() {
                            Ok(cl) => Revision::Change(cl),
                            Err(_) => Revision::Label(r.to_owned()),
                        })
                    }
                    // A bare number is a changelist, as it always was.
                    None if target.parse::<u64>().is_ok() => {
                        rev = Some(Revision::Change(target.parse()?))
                    }
                    None => paths.push(cwd.join(target)),
                }
            }
            // The server lookups before the first progress bar cannot show one.
            notice("Checking for changes...");
            Op::SyncTo { rev, paths }
        }
        Cmd::Status => Op::Status,
        Cmd::Fstat { paths } => Op::Inspect { paths: abs(paths) },
        Cmd::Diff { paths, tool } => {
            diff_tool = crate::difftool::chosen(tool);
            Op::Diff { paths: abs(paths) }
        }
        Cmd::Clean { paths, dry_run } => {
            notice("Scanning the workspace...");
            Op::Clean {
                paths: abs(paths),
                dry_run,
            }
        }
        Cmd::Reconcile => {
            notice("Scanning the workspace...");
            Op::Reconcile
        }
        Cmd::Add { paths, cl } => open(abs(paths), OpenAction::Add, cl)?,
        Cmd::Edit { paths, cl } => open(abs(paths), OpenAction::Edit, cl)?,
        Cmd::Delete { paths, cl } => open(abs(paths), OpenAction::Delete, cl)?,
        Cmd::Revert { paths } => Op::Revert { paths: abs(paths) },
        Cmd::Move { from, to, cl } => Op::Move {
            from: cwd.join(from),
            to: cwd.join(to),
            cl: parse_cl_or_default(cl.as_deref())?,
        },
        Cmd::Resolve {
            paths,
            accept,
            tool,
        } => {
            if matches!(accept, Accept::Merge)
                && let Some(tool) = crate::mergetool::chosen(tool)
            {
                return resolve_with_tool(home, cwd, abs(paths), &tool, no_daemon).await;
            }
            let mode = match accept {
                Accept::Merge => ResolveMode::Auto,
                Accept::Yours => ResolveMode::AcceptYours,
                Accept::Theirs => ResolveMode::AcceptTheirs,
            };
            Op::Resolve {
                paths: abs(paths),
                mode,
            }
        }
        Cmd::Cl(ClCmd::New { description }) => Op::ClNew { description },
        Cmd::Cl(ClCmd::List) => Op::ClList,
        Cmd::Cl(ClCmd::Delete { cl }) => Op::ClDelete { cl: parse_cl(&cl)? },
        Cmd::Cl(ClCmd::Move { cl, paths }) => Op::ClMove {
            cl: parse_cl(&cl)?,
            paths: abs(paths),
        },
        Cmd::Cl(ClCmd::Split { path, hunk, to }) => match (hunk, to) {
            (Some(hunk), Some(to)) => Op::AssignHunk {
                path: cwd.join(path),
                hunk,
                to: parse_cl(&to)?,
            },
            _ => Op::Hunks {
                path: cwd.join(path),
            },
        },
        Cmd::Submit { cl, message, fixes } => {
            notice("Preparing to submit...");
            Op::Submit {
                cl: parse_cl_or_default(cl.as_deref())?,
                message,
                fixes,
            }
        }
        Cmd::Shelve { cl } => Op::Shelve {
            cl: parse_cl_or_default(cl.as_deref())?,
        },
        Cmd::Unshelve { shelf, cl, sync } => Op::Unshelve {
            shelf,
            cl: cl.as_deref().map(parse_cl).transpose()?,
            sync,
        },
        Cmd::Merge { from, cl: None } => {
            notice("Merging...");
            Op::Merge {
                from: from.as_deref().map(DepotPath::parse).transpose()?,
            }
        }
        Cmd::Merge { from, cl: Some(cl) } => {
            notice("Merging...");
            Op::Pick {
                from: from.as_deref().map(DepotPath::parse).transpose()?,
                cl,
            }
        }
        Cmd::Undo { cl } => Op::Undo { cl },
        Cmd::Unlock { paths, force } => Op::Unlock {
            paths: abs(paths),
            force,
        },
        Cmd::Snapshot(SnapshotCmd::Save { cl, message }) => Op::Snapshot {
            cl: parse_cl_or_default(cl.as_deref())?,
            message,
        },
        Cmd::Snapshot(SnapshotCmd::List) => Op::Snapshots,
        Cmd::Snapshot(SnapshotCmd::Restore { id }) => Op::Restore { id },
        Cmd::Snapshot(SnapshotCmd::Delete { id }) => Op::DeleteSnapshot { id },
        Cmd::Snapshot(SnapshotCmd::Diff { old, new, tool }) => {
            diff_tool = crate::difftool::chosen(tool);
            Op::DiffSnapshots {
                old: SnapshotSide::Snapshot(old),
                new: new.map_or(SnapshotSide::Working, SnapshotSide::Snapshot),
            }
        }
        command => return server_query(home, cwd, command).await,
    };
    match (daemon::execute(home, cwd, op, no_daemon).await?, diff_tool) {
        (Outcome::Diff(diffs), Some(tool)) => crate::difftool::run(&tool, &diffs)?,
        (outcome, _) => print(outcome),
    }
    Ok(())
}

/// Resolves by merging automatically, then opening what that cannot settle
/// in the merge tool `tool`.
async fn resolve_with_tool(
    home: &Home,
    cwd: &Path,
    paths: Vec<PathBuf>,
    tool: &str,
    no_daemon: bool,
) -> anyhow::Result<()> {
    // The versions have to be taken first: a merge with conflicts overwrites
    // the workspace file with markers.
    let op = Op::ResolveInputs {
        paths: paths.clone(),
    };
    let Outcome::MergeInputs(inputs) = daemon::execute(home, cwd, op, no_daemon).await? else {
        return Err(unexpected());
    };
    let op = Op::Resolve {
        paths,
        mode: ResolveMode::Auto,
    };
    let Outcome::Resolved(mut outcomes) = daemon::execute(home, cwd, op, no_daemon).await? else {
        return Err(unexpected());
    };
    let mut by_tool = Vec::new();
    for (path, outcome) in &mut outcomes {
        if !matches!(
            outcome,
            ResolveOutcome::Conflicted | ResolveOutcome::NeedsChoice
        ) {
            continue;
        }
        let Some(input) = inputs.iter().find(|i| i.path == *path) else {
            continue;
        };
        let current = std::fs::read(&input.local).unwrap_or_default();
        println!("{path}: opening the merge tool");
        // A failure leaves this file for later without losing the others.
        match crate::mergetool::run(tool, input, &current) {
            Ok(crate::mergetool::Merged::Done) => {
                by_tool.push(input.local.clone());
                *outcome = ResolveOutcome::Merged;
            }
            Ok(crate::mergetool::Merged::Skipped(why)) => eprintln!("kelp: {path}: {why}"),
            Err(e) => eprintln!("kelp: {path}: {e:#}"),
        }
    }
    if !by_tool.is_empty() {
        let op = Op::Resolve {
            paths: by_tool,
            mode: ResolveMode::AcceptYours,
        };
        daemon::execute(home, cwd, op, no_daemon).await?;
    }
    print(Outcome::Resolved(outcomes));
    Ok(())
}

fn open(paths: Vec<PathBuf>, action: OpenAction, cl: Option<String>) -> anyhow::Result<Op> {
    Ok(Op::Open {
        paths,
        action,
        cl: parse_cl_or_default(cl.as_deref())?,
    })
}

fn print(outcome: Outcome) {
    match outcome {
        Outcome::Done => {}
        Outcome::Synced(report) => {
            for p in &report.updated {
                println!("updated  {p}");
            }
            for p in &report.deleted {
                println!("deleted  {p}");
            }
            for (p, why) in &report.skipped {
                let why = match why {
                    SkipReason::Opened => "opened; run `kelp resolve`",
                    SkipReason::LocallyModified => "modified locally without being opened",
                    SkipReason::Untracked => "an untracked file is in the way",
                };
                eprintln!("skipped  {p} ({why})");
            }
            for a in &report.artifacts {
                match a.now {
                    Some((id, cl)) => println!("artifact {} <- {} {id} (@{cl})", a.to, a.name),
                    None => println!("artifact {} emptied", a.to),
                }
            }
            println!("Synced to changelist {}.", report.cl);
        }
        Outcome::Status {
            status,
            changelists,
            have_cl,
        } => {
            for cl in &changelists {
                let files: Vec<_> = status
                    .opened
                    .iter()
                    .filter(|(_, _, owner)| *owner == cl.id)
                    .collect();
                let shelf = cl
                    .shelf
                    .map(|s| format!(" (shelved as {s})"))
                    .unwrap_or_default();
                match (cl.id, files.is_empty()) {
                    (LocalClId::DEFAULT, true) => continue,
                    (LocalClId::DEFAULT, false) => println!("Changelist default:"),
                    (_, true) => {
                        println!("Changelist {} \"{}\"{shelf}: empty", cl.id, cl.description);
                        continue;
                    }
                    (_, false) => println!("Changelist {} \"{}\"{shelf}:", cl.id, cl.description),
                }
                for (path, action, _) in files {
                    let note = if status.unresolved.contains(path) {
                        "  (needs resolve)"
                    } else {
                        ""
                    };
                    println!("  {:<7}{path}{note}", action_name(*action));
                }
            }
            if status.has_unopened_changes() {
                println!("Not opened (run `kelp reconcile`):");
                for (paths, label) in [
                    (&status.modified, "edit"),
                    (&status.added, "add"),
                    (&status.deleted, "delete"),
                ] {
                    for p in paths {
                        println!("  {label:<7}{p}");
                    }
                }
            }
            if status.opened.is_empty() && !status.has_unopened_changes() {
                println!("No changes; synced to changelist {have_cl}.");
            }
        }
        Outcome::Opened(opened) => {
            for (path, action) in opened {
                println!("{:<7}{path}", action_name(action));
            }
        }
        Outcome::Reverted(paths) => {
            for p in paths {
                println!("reverted {p}");
            }
        }
        Outcome::Resolved(outcomes) => {
            for (path, outcome) in outcomes {
                let text = match outcome {
                    ResolveOutcome::Merged => "merged",
                    ResolveOutcome::TookYours => "kept yours",
                    ResolveOutcome::TookTheirs => "took theirs",
                    ResolveOutcome::Conflicted => {
                        "conflicts; edit the file, then `kelp resolve --accept yours`"
                    }
                    ResolveOutcome::NeedsChoice => "needs `--accept yours` or `--accept theirs`",
                };
                println!("{path}: {text}");
            }
        }
        Outcome::Submitted(n) => println!("Submitted changelist {n}."),
        Outcome::Shelved(n) => println!("Shelved as {n}."),
        Outcome::Unshelved { cl, opened, synced } => {
            for (path, cl) in synced {
                println!("synced {path} to changelist {cl}");
            }
            for (path, action) in opened {
                println!("{:<7}{path}", action_name(action));
            }
            println!("Unshelved into changelist {cl}.");
        }
        Outcome::Merged(report) => match report.changelist {
            None => println!("Nothing to merge."),
            Some(cl) => {
                for (path, action) in &report.opened {
                    let note = if report.conflicts.contains(path) {
                        "  (needs resolve)"
                    } else {
                        ""
                    };
                    println!("{:<7}{path}{note}", action_name(*action));
                }
                println!(
                    "Merge through changelist {} opened in {cl}.",
                    report.through
                );
            }
        },
        Outcome::Changelist(id) => println!("Created changelist {id}."),
        Outcome::Changelists(list) => {
            for cl in list {
                println!(
                    "{:<8}{:>5} files  {}",
                    cl.id.to_string(),
                    cl.files,
                    cl.description
                );
            }
        }
        Outcome::Hunks(hunks) => {
            for h in hunks {
                println!(
                    "{:>3}  {:<8} lines {}-{}  {}",
                    h.number,
                    h.owner.to_string(),
                    h.first_line,
                    h.last_line,
                    h.preview
                );
            }
        }
        Outcome::Snapshot(id) => println!("Saved snapshot {id}."),
        Outcome::Snapshots(list) => {
            for s in list {
                let kind = if s.automatic { "auto" } else { "    " };
                println!(
                    "{:>4}  {kind}  {:<8} {} files  {}",
                    s.id,
                    s.cl.to_string(),
                    s.files.len(),
                    s.message
                );
            }
        }
        Outcome::Restored(paths) => {
            for p in paths {
                println!("restored {p}");
            }
        }
        Outcome::Files(reports) => {
            for r in reports {
                let mut notes = Vec::new();
                match r.opened {
                    Some((action, cl)) => {
                        notes.push(format!("opened for {} in {cl}", action_name(action)))
                    }
                    None if !r.tracked && r.exists => notes.push("not tracked".into()),
                    None if !r.tracked => notes.push("not in the workspace".into()),
                    None => {}
                }
                if r.modified {
                    notes.push("modified without being opened".into());
                }
                if r.needs_resolve {
                    notes.push("needs resolve".into());
                }
                if r.out_of_date {
                    notes.push("newer revision on the server".into());
                }
                match (&r.locked_by, r.locked_here) {
                    (Some(_), true) => notes.push("locked by this workspace".into()),
                    (Some((user, ws)), false) => notes.push(format!("locked by {user}@{ws}")),
                    (None, _) if r.exclusive => notes.push("exclusive".into()),
                    _ => {}
                }
                if notes.is_empty() {
                    notes.push("up to date".into());
                }
                println!("{}: {}", r.path, notes.join(", "));
            }
        }
        Outcome::Diff(diffs) => {
            for d in diffs {
                print!("{}", render_diff(&d));
            }
        }
        // Only the merge tool asks for these, and it does not print them.
        Outcome::MergeInputs(inputs) => {
            for input in inputs {
                println!("{}", input.path);
            }
        }
        Outcome::Cleaned(report) => {
            for path in &report.restored {
                println!("restored {path}");
            }
            for path in &report.removed {
                println!("deleted  {path}");
            }
        }
    }
}

/// Runs commands that only query the server for the workspace's stream.
async fn server_query(home: &Home, cwd: &Path, command: Cmd) -> anyhow::Result<()> {
    let ws = Workspace::discover(cwd)?;
    let conn = connect(home, &ws.config().server).await?;
    match command {
        Cmd::CopyUp { message } => {
            let request = Request::CopyUp {
                stream: ws.stream().to_string(),
                description: message,
            };
            let Reply::Submitted { cl, .. } = conn.call(&request).await? else {
                return Err(unexpected());
            };
            println!("Copied {} to its parent as changelist {cl}.", ws.stream());
        }
        Cmd::Locks => {
            let Reply::Locks(locks) = conn
                .call(&Request::ListLocks {
                    stream: ws.stream().to_string(),
                })
                .await?
            else {
                return Err(unexpected());
            };
            for l in locks {
                println!("{:<40} {}@{}", l.path, l.user, l.workspace);
            }
        }
        Cmd::DeleteShelf { shelf } => {
            conn.call(&Request::DeleteShelf { cl: shelf }).await?;
        }
        Cmd::Annotate { path } => {
            let depot = match path.starts_with("//") {
                true => kelp_core::DepotPath::parse(&path)?,
                false => ws.depot_path_of(&cwd.join(&path))?,
            };
            let lines = kelp_client::annotate(&conn, &crate::progress::cache(home), &depot).await?;
            // One round trip per changelist adds up; make them all at once.
            let cls: std::collections::BTreeSet<u64> = lines.iter().map(|l| l.cl).collect();
            let mut describes = tokio::task::JoinSet::new();
            for cl in cls {
                let conn = conn.clone();
                describes.spawn(async move { (cl, conn.call(&Request::Describe { cl }).await) });
            }
            let mut authors = std::collections::HashMap::new();
            while let Some(done) = describes.join_next().await {
                let (cl, reply) = done?;
                let author = match reply? {
                    Reply::Describe(d) => d.summary.author_name.unwrap_or(d.summary.author),
                    _ => String::new(),
                };
                authors.insert(cl, author);
            }
            for line in &lines {
                print!("{:>6} {:<12} {}", line.cl, authors[&line.cl], line.text);
            }
        }
        Cmd::Print { path } => {
            let (spec, at) = match path.rsplit_once('@') {
                Some((spec, rev)) => match rev.parse() {
                    Ok(cl) => (spec, Some(cl)),
                    Err(_) => {
                        let request = Request::GetLabel {
                            name: rev.to_owned(),
                        };
                        let Reply::Label(label) = conn.call(&request).await? else {
                            return Err(unexpected());
                        };
                        (spec, Some(label.cl))
                    }
                },
                None => (path.as_str(), None),
            };
            let depot = match spec.starts_with("//") {
                true => kelp_core::DepotPath::parse(spec)?,
                false => ws.depot_path_of(&cwd.join(spec))?,
            };
            let bytes =
                kelp_client::read_file(&conn, &crate::progress::cache(home), &depot, at).await?;
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
        Cmd::Filelog { path } => {
            let depot = ws.depot_path_of(&cwd.join(path))?;
            let Reply::Filelog(revs) = conn
                .call(&Request::Filelog {
                    path: depot.to_string(),
                })
                .await?
            else {
                return Err(unexpected());
            };
            println!("{depot}");
            for r in revs {
                let purged = if r.purged { "  (purged)" } else { "" };
                println!(
                    "  #{:<4} change {:<6} {:<7} {} bytes{purged}",
                    r.rev,
                    r.cl,
                    file_action(r.action),
                    r.size
                );
            }
        }
        Cmd::Changes { stream, limit } => {
            let stream = Some(stream.unwrap_or_else(|| ws.stream().to_string()));
            let Reply::Changes(changes) = conn.call(&Request::Changes { stream, limit }).await?
            else {
                return Err(unexpected());
            };
            for c in changes {
                let first_line = c.description.lines().next().unwrap_or_default();
                let author = c.author_name.as_deref().unwrap_or(&c.author);
                println!("change {:<6} by {author:<12} {first_line}", c.cl);
            }
        }
        Cmd::Describe { cl } => {
            let Reply::Describe(d) = conn.call(&Request::Describe { cl }).await? else {
                return Err(unexpected());
            };
            let s = &d.summary;
            let author = match (&s.author_name, &s.author_email) {
                (Some(name), Some(email)) => format!("{name} <{email}> ({})", s.author),
                (Some(name), None) => format!("{name} ({})", s.author),
                (None, Some(email)) => format!("{} <{email}>", s.author),
                (None, None) => s.author.clone(),
            };
            println!("change {} on {} by {author}\n", s.cl, s.stream);
            for line in d.summary.description.lines() {
                println!("    {line}");
            }
            if !d.summary.issues.is_empty() {
                let issues: Vec<String> = d
                    .summary
                    .issues
                    .iter()
                    .map(|k| match d.summary.fixes.contains(k) {
                        true => format!("{k} (fixed)"),
                        false => k.clone(),
                    })
                    .collect();
                println!("\n    issues: {}", issues.join(", "));
            }
            println!();
            for (path, action) in d.files {
                println!("  {:<7}{path}", file_action(action));
            }
        }
        _ => unreachable!("handled by admin::run"),
    }
    Ok(())
}
