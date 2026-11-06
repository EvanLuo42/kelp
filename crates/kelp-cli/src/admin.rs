//! Commands that work outside a workspace: accounts, depots, streams and settings.

use std::path::Path;

use anyhow::bail;
use kelp_client::{Home, Workspace};
use kelp_core::DepotPath;
use kelp_proto::rpc::{ImportInfo, ProfileChange, StreamKind};
use kelp_proto::{Reply, Request};
use kelp_workspace::Config;

use crate::daemon;
use crate::util::{
    LoginMethod, connect, link_sso, login, notice, pick_server, read_new_password, read_password,
    unexpected, workspace_name,
};
use crate::{Cmd, DepotCmd, DocCmd, GroupCmd, Kind, LabelCmd, StreamCmd, TotpCmd, UserCmd};
use kelp_client::ops::Op;

/// Runs `command` if it does not need a workspace, or hands it back.
pub async fn run(
    home: &Home,
    cwd: &Path,
    server: Option<String>,
    command: Cmd,
) -> anyhow::Result<Option<Cmd>> {
    let server = || pick_server(home, cwd, server.clone());
    match command {
        Cmd::Login {
            server,
            user,
            password_stdin,
            sso,
            link,
            token_stdin,
        } => {
            let method = match (link, sso, token_stdin) {
                (true, _, _) => {
                    link_sso(home, &server).await?;
                    return Ok(None);
                }
                (_, true, _) => LoginMethod::Sso,
                (_, _, true) => LoginMethod::Token,
                _ => LoginMethod::Password {
                    user,
                    stdin: password_stdin,
                },
            };
            login(home, &server, method).await?
        }
        Cmd::Totp(TotpCmd::Enable) => {
            let conn = connect(home, &server()?).await?;
            let Reply::TotpSecret { secret, uri } = conn.call(&Request::TotpEnroll).await? else {
                return Err(unexpected());
            };
            println!("Add this account to your authenticator app:\n  {uri}\n  (secret {secret})");
            let code = rpassword::prompt_password("Code shown by the app: ")?;
            conn.call(&Request::TotpConfirm { secret, code }).await?;
            println!("Two-factor login is on.");
        }
        Cmd::Totp(TotpCmd::Disable { user }) => {
            let conn = connect(home, &server()?).await?;
            let code = match user {
                Some(_) => None,
                None => Some(rpassword::prompt_password("Authenticator code: ")?),
            };
            conn.call(&Request::TotpDisable { user, code }).await?;
            println!("Two-factor login is off.");
        }
        Cmd::Logout { server } => {
            // Revoking is best effort: the server may be unreachable, and
            // forgetting the token still logs this machine out.
            if let Ok(conn) = connect(home, &server).await
                && let Err(e) = conn.call(&Request::Logout).await
            {
                eprintln!("could not revoke the token on the server: {e}");
            }
            let mut creds = home.credentials()?;
            if let Some(entry) = creds.servers.get_mut(&server) {
                entry.token = None;
            }
            home.save_credentials(&creds)?;
        }
        Cmd::User(UserCmd::Add {
            name,
            admin,
            service,
            replica,
            password_stdin,
        }) => {
            let conn = connect(home, &server()?).await?;
            let password = match service || replica {
                true => None,
                false => Some(read_new_password(
                    password_stdin,
                    &format!("Password for {name}: "),
                    true,
                )?),
            };
            conn.call(&Request::CreateUser {
                name: name.clone(),
                password,
                admin,
                replica,
            })
            .await?;
            println!("Created user {name}.");
        }
        Cmd::Passwd {
            user,
            password_stdin,
        } => {
            let conn = connect(home, &server()?).await?;
            let current = match user {
                Some(_) => None,
                None => Some(read_password(password_stdin, "Current password: ", false)?),
            };
            let password = read_new_password(password_stdin, "New password: ", !password_stdin)?;
            conn.call(&Request::SetPassword {
                user,
                current,
                password,
            })
            .await?;
            println!("Password changed.");
        }
        Cmd::User(UserCmd::List) => {
            let Reply::Users(users) = connect(home, &server()?)
                .await?
                .call(&Request::ListUsers)
                .await?
            else {
                return Err(unexpected());
            };
            for u in users {
                let mut notes = Vec::new();
                if u.admin {
                    notes.push("administrator");
                }
                if u.replica {
                    notes.push("edge");
                }
                if u.sso.is_some() {
                    notes.push("single sign-on");
                }
                if u.disabled {
                    notes.push("disabled");
                }
                match notes.is_empty() {
                    true => println!("{}", u.name),
                    false => println!("{} ({})", u.name, notes.join(", ")),
                }
            }
        }
        Cmd::User(UserCmd::Disable { name }) => {
            connect(home, &server()?)
                .await?
                .call(&Request::SetDisabled {
                    user: name.clone(),
                    disabled: true,
                })
                .await?;
            println!("Disabled {name}; their tokens no longer work.");
        }
        Cmd::User(UserCmd::Enable { name }) => {
            connect(home, &server()?)
                .await?
                .call(&Request::SetDisabled {
                    user: name.clone(),
                    disabled: false,
                })
                .await?;
            println!("Enabled {name}.");
        }
        Cmd::User(UserCmd::Show { name }) => {
            let Reply::User(u) = connect(home, &server()?)
                .await?
                .call(&Request::GetUser { name })
                .await?
            else {
                return Err(unexpected());
            };
            let kind = match (u.admin, u.replica, u.password, &u.sso) {
                (_, true, ..) => "edge account",
                (true, ..) => "administrator",
                (_, _, false, None) => "service account",
                _ => "user",
            };
            println!("{} ({kind})", u.name);
            if let Some(display_name) = &u.display_name {
                println!("display name: {display_name}");
            }
            // The first address is the primary one.
            if let Some((primary, others)) = u.emails.split_first() {
                println!("email: {primary} (primary)");
                for email in others {
                    println!("email: {email}");
                }
            }
            println!("password: {}", if u.password { "yes" } else { "no" });
            println!(
                "single sign-on: {}",
                u.sso.as_deref().unwrap_or("not linked")
            );
            if let Some(expected) = u.expected_sso {
                println!("links on first sign-on as: {expected}");
            }
            if !u.aliases.is_empty() {
                println!("former names: {}", u.aliases.join(", "));
            }
        }
        Cmd::User(UserCmd::Link { name, sso_name }) => {
            connect(home, &server()?)
                .await?
                .call(&Request::ExpectSso {
                    user: name.clone(),
                    sso_name: Some(sso_name.clone()),
                })
                .await?;
            println!("{name} will be linked when {sso_name} first signs on.");
        }
        Cmd::User(UserCmd::Unlink { name }) => {
            let conn = connect(home, &server()?).await?;
            conn.call(&Request::UnlinkSso { user: name.clone() })
                .await?;
            conn.call(&Request::ExpectSso {
                user: name.clone(),
                sso_name: None,
            })
            .await?;
            println!("Unlinked {name}; set a password or link it again for it to log in.");
        }
        Cmd::User(UserCmd::Merge { from, into }) => {
            connect(home, &server()?)
                .await?
                .call(&Request::MergeUser {
                    from: from.clone(),
                    into: into.clone(),
                })
                .await?;
            println!("Merged {from} into {into}; {from} is now a former name.");
        }
        Cmd::User(
            cmd @ (UserCmd::Name { .. }
            | UserCmd::AddEmail { .. }
            | UserCmd::RemoveEmail { .. }
            | UserCmd::PrimaryEmail { .. }),
        ) => {
            let (user, change, done) = match cmd {
                UserCmd::Name { display_name, user } => {
                    let done = match &display_name {
                        Some(n) => format!("Display name set to {n}."),
                        None => "Display name cleared.".to_owned(),
                    };
                    (user, ProfileChange::DisplayName(display_name), done)
                }
                UserCmd::AddEmail { email, user } => {
                    let done = format!("Added {email}.");
                    (user, ProfileChange::AddEmail(email), done)
                }
                UserCmd::RemoveEmail { email, user } => {
                    let done = format!("Removed {email}.");
                    (user, ProfileChange::RemoveEmail(email), done)
                }
                UserCmd::PrimaryEmail { email, user } => {
                    let done = format!("{email} is now the primary address.");
                    (user, ProfileChange::PrimaryEmail(email), done)
                }
                _ => unreachable!("matched above"),
            };
            connect(home, &server()?)
                .await?
                .call(&Request::EditProfile { user, change })
                .await?;
            println!("{done}");
        }
        Cmd::Token { user, days } => {
            let conn = connect(home, &server()?).await?;
            let Reply::Token { token, .. } = conn
                .call(&Request::IssueToken {
                    user,
                    ttl_days: days,
                })
                .await?
            else {
                return Err(unexpected());
            };
            println!("{token}");
        }
        Cmd::Group(GroupCmd::Set { name, members }) => {
            connect(home, &server()?)
                .await?
                .call(&Request::SetGroup { name, members })
                .await?;
        }
        Cmd::Group(GroupCmd::List) => {
            let Reply::Groups(groups) = connect(home, &server()?)
                .await?
                .call(&Request::ListGroups)
                .await?
            else {
                return Err(unexpected());
            };
            for g in groups {
                println!("{}: {}", g.name, g.members.join(" "));
            }
        }
        Cmd::Protect(DocCmd::Get) => {
            let conn = connect(home, &server()?).await?;
            print_doc(
                conn.call(&Request::GetProtections).await?,
                "write * //...   (default)",
            )?;
        }
        Cmd::Protect(DocCmd::Set { file }) => {
            let source = std::fs::read_to_string(file)?;
            connect(home, &server()?)
                .await?
                .call(&Request::SetProtections { source })
                .await?;
        }
        Cmd::Typemap { command, depot } => {
            let depot = match depot {
                Some(d) => d,
                None => Workspace::discover(cwd)?.stream().depot().to_owned(),
            };
            let conn = connect(home, &server()?).await?;
            match command {
                DocCmd::Get => print_doc(
                    conn.call(&Request::GetTypemap { depot }).await?,
                    "# no typemap",
                )?,
                DocCmd::Set { file } => {
                    let source = std::fs::read_to_string(file)?;
                    conn.call(&Request::SetTypemap { depot, source }).await?;
                }
            }
        }
        Cmd::Gc { dry_run } => {
            notice("Collecting garbage; this can take a while...");
            let Reply::Gc(r) = connect(home, &server()?)
                .await?
                .call(&Request::Gc { dry_run })
                .await?
            else {
                return Err(unexpected());
            };
            let verb = if r.dry_run { "would delete" } else { "deleted" };
            println!(
                "{verb} {} chunks, {} manifests and {} tree nodes; kept {} chunks",
                r.chunks_deleted, r.manifests_deleted, r.nodes_deleted, r.chunks_kept
            );
            if !r.dry_run {
                println!(
                    "compacted {} packs, freeing {} bytes",
                    r.packs_compacted, r.bytes_freed
                );
            }
        }
        Cmd::Backup {
            dest,
            metadata_only,
        } => {
            notice("Backing up; this can take a while...");
            let Reply::BackedUp {
                log_index,
                chunks_copied,
                chunks_present,
                bytes_copied,
            } = connect(home, &server()?)
                .await?
                .call(&Request::Backup {
                    dest: dest.clone(),
                    metadata_only,
                })
                .await?
            else {
                return Err(unexpected());
            };
            println!(
                "Backed up through log entry {log_index} to {dest}: copied {chunks_copied} chunks ({bytes_copied} bytes), {chunks_present} already there."
            );
        }
        Cmd::Fsck { deep } => {
            notice("Checking the server's data; this can take a while...");
            let Reply::Fsck(r) = connect(home, &server()?)
                .await?
                .call(&Request::Fsck { deep })
                .await?
            else {
                return Err(unexpected());
            };
            println!(
                "checked {} audit entries, {} files, {} chunks",
                r.audit_entries, r.contents, r.chunks
            );
            for e in &r.errors {
                println!("error: {e}");
            }
            for c in &r.missing_manifests {
                println!("missing manifest {c}");
            }
            for c in &r.missing_chunks {
                println!("missing chunk {c}");
            }
            for c in &r.corrupt_chunks {
                println!("corrupt chunk {c}");
            }
            let damaged =
                r.missing_manifests.len() + r.missing_chunks.len() + r.corrupt_chunks.len();
            if damaged > 0 || !r.errors.is_empty() {
                bail!("the server's data is damaged");
            }
        }
        Cmd::Relocate { to, from } => relocate(home, cwd, &to, from).await?,
        Cmd::Tier => {
            notice("Moving chunks to cold storage; this can take a while...");
            let Reply::Tiered { chunks, bytes } = connect(home, &server()?)
                .await?
                .call(&Request::Tier)
                .await?
            else {
                return Err(unexpected());
            };
            println!("moved {chunks} chunks ({bytes} bytes) to cold storage");
        }
        Cmd::Obliterate { paths, yes } => {
            if !yes {
                bail!("obliterate destroys history and cannot be undone; pass --yes to confirm");
            }
            connect(home, &server()?)
                .await?
                .call(&Request::Obliterate {
                    paths: paths.clone(),
                })
                .await?;
            println!(
                "Obliterated {}. Run `kelp gc` to free the space.",
                paths.join(", ")
            );
        }
        Cmd::Audit {
            from,
            limit,
            verify,
        } => {
            let conn = connect(home, &server()?).await?;
            if verify {
                let Reply::AuditVerified { entries, head } =
                    conn.call(&Request::VerifyAudit).await?
                else {
                    return Err(unexpected());
                };
                println!("{entries} entries verified; head {head}");
            } else {
                let Reply::Audit(entries) = conn.call(&Request::AuditLog { from, limit }).await?
                else {
                    return Err(unexpected());
                };
                for e in entries {
                    println!(
                        "{:>6}  {:<12} {}  {}",
                        e.index,
                        e.actor,
                        &e.hash[..12],
                        e.summary
                    );
                }
            }
        }
        Cmd::Depot(DepotCmd::Create { name }) => {
            connect(home, &server()?)
                .await?
                .call(&Request::CreateDepot { name: name.clone() })
                .await?;
            println!("Created depot //{name} with stream //{name}/main.");
        }
        Cmd::Stream(StreamCmd::Create { path, parent, kind }) => {
            let kind = match kind {
                Kind::Development => StreamKind::Development,
                Kind::Release => StreamKind::Release,
                Kind::Task => StreamKind::Task,
            };
            let request = Request::CreateStream {
                path: path.clone(),
                kind,
                parent: Some(parent.clone()),
            };
            connect(home, &server()?).await?.call(&request).await?;
            println!("Created {kind:?} stream {path} from {parent}.");
        }
        Cmd::Stream(StreamCmd::Delete { path, yes }) => {
            if !yes {
                bail!("deleting {path} cannot be undone; add --yes");
            }
            connect(home, &server()?)
                .await?
                .call(&Request::DeleteStream {
                    stream: path.clone(),
                })
                .await?;
            println!("Deleted {path}; its changelists stay in the history.");
        }
        Cmd::Stream(StreamCmd::Imports { command, stream }) => {
            let stream = match stream {
                Some(s) => s,
                None => Workspace::discover(cwd)?.stream().to_string(),
            };
            let conn = connect(home, &server()?).await?;
            match command {
                DocCmd::Get => {
                    let Reply::Imports(imports) =
                        conn.call(&Request::GetImports { stream }).await?
                    else {
                        return Err(unexpected());
                    };
                    if imports.is_empty() {
                        println!("# no imports");
                    }
                    for i in imports {
                        println!("{}", format_import(&i));
                    }
                }
                DocCmd::Set { file } => {
                    let imports = parse_imports(&std::fs::read_to_string(file)?)?;
                    conn.call(&Request::SetImports { stream, imports }).await?;
                }
            }
        }
        Cmd::Cluster => {
            let Reply::Cluster(info) = connect(home, &server()?)
                .await?
                .call(&Request::ClusterStatus)
                .await?
            else {
                return Err(unexpected());
            };
            let leader = info.leader.map_or("none".to_owned(), |l| l.to_string());
            let voters: Vec<String> = info.voters.iter().map(u64::to_string).collect();
            println!("member {} of {}", info.member, voters.join(", "));
            println!("leader: {leader}");
            println!(
                "applied through: {}",
                info.applied.map_or("nothing".to_owned(), |a| a.to_string())
            );
        }
        Cmd::Label(command) => {
            let conn = connect(home, &server()?).await?;
            match command {
                LabelCmd::Set {
                    name,
                    cl,
                    stream,
                    message,
                } => {
                    let stream = match stream {
                        Some(s) => s,
                        None => Workspace::discover(cwd)?.stream().to_string(),
                    };
                    conn.call(&Request::SetLabel {
                        name: name.clone(),
                        stream,
                        cl,
                        description: message,
                    })
                    .await?;
                    let Reply::Label(label) = conn.call(&Request::GetLabel { name }).await? else {
                        return Err(unexpected());
                    };
                    println!("{} is {}@{}", label.name, label.stream, label.cl);
                }
                LabelCmd::Delete { name } => {
                    conn.call(&Request::DeleteLabel { name }).await?;
                }
                LabelCmd::List => {
                    let Reply::Labels(labels) = conn.call(&Request::ListLabels).await? else {
                        return Err(unexpected());
                    };
                    for l in labels {
                        println!(
                            "{:<24} {}@{} {} {}",
                            l.name, l.stream, l.cl, l.owner, l.description
                        );
                    }
                }
            }
        }
        Cmd::Shelves {
            stream,
            user,
            limit,
        } => {
            let request = Request::ListShelves {
                stream,
                owner: user,
                limit,
            };
            let Reply::Shelves(shelves) = connect(home, &server()?).await?.call(&request).await?
            else {
                return Err(unexpected());
            };
            for s in shelves {
                let first_line = s.description.lines().next().unwrap_or_default();
                let owner = s.owner_name.as_deref().unwrap_or(&s.owner);
                let files = match s.files {
                    1 => "1 file".to_owned(),
                    n => format!("{n} files"),
                };
                println!(
                    "shelf {:<6} {} by {owner}@{} ({files}) {first_line}",
                    s.cl, s.stream, s.workspace
                );
            }
        }
        Cmd::Ls { path, long, dirs } => {
            crate::browse::ls(home, cwd, &server()?, path, long, dirs).await?
        }
        Cmd::Tree { path, depth, dirs } => {
            crate::browse::tree(home, cwd, &server()?, path, depth, dirs).await?
        }
        Cmd::Streams => {
            let Reply::Streams(streams) = connect(home, &server()?)
                .await?
                .call(&Request::ListStreams)
                .await?
            else {
                return Err(unexpected());
            };
            for s in streams {
                let parent = s.parent.map(|p| format!("  from {p}")).unwrap_or_default();
                println!(
                    "{:<24} {:<12} @{}{parent}",
                    s.path,
                    format!("{:?}", s.kind).to_lowercase(),
                    s.cl
                );
            }
        }
        Cmd::Init {
            stream,
            directory,
            name,
            projected,
        } => {
            let here = cwd;
            let cwd = &directory.map_or_else(|| here.to_owned(), |d| here.join(d));
            std::fs::create_dir_all(cwd)?;
            let server = server()?;
            let stream = DepotPath::parse(&stream)?;
            let conn = connect(home, &server).await?;
            conn.call(&Request::ResolveStream {
                stream: stream.to_string(),
                at: None,
            })
            .await?;
            let name = name.unwrap_or_else(|| workspace_name(cwd));
            let mut config = Config::new(&name, &server, &stream);
            if projected {
                if std::fs::read_dir(cwd).is_ok_and(|mut d| d.next().is_some()) {
                    bail!("a projected workspace needs an empty directory");
                }
                config.projected = true;
                // ProjFS keeps opened files in the virtualization root itself;
                // FUSE keeps them in a separate upper directory.
                let upper = if cfg!(windows) {
                    cwd.clone()
                } else {
                    home.dir().join("vfs").join(&name)
                };
                Workspace::create(&upper, config)?;
                daemon::execute(home, cwd, Op::Mount { upper }, false).await?;
            } else {
                Workspace::create(cwd, config)?;
            }
            let kind = if projected {
                "projected workspace"
            } else {
                "workspace"
            };
            println!(
                "Created {kind} {name} for {stream} at {}. Run `kelp sync` to fetch files.",
                cwd.display()
            );
            if projected && cwd == here {
                println!(
                    "Your shell still sees the directory as it was before mounting; `cd` out and back in."
                );
            }
        }
        Cmd::Unmount => {
            daemon::execute(home, cwd, Op::Unmount, false).await?;
        }
        Cmd::Plugin(command) => {
            let conn = connect(home, &server()?).await?;
            crate::plugin::run(&conn, &home.cache(), command).await?;
        }
        Cmd::Webhook(command) => {
            crate::webhook::run(&connect(home, &server()?).await?, command).await?;
        }
        Cmd::Issue(command) => {
            crate::issue::run(&connect(home, &server()?).await?, cwd, command).await?;
        }
        Cmd::Review(command) => {
            let conn = connect(home, &server()?).await?;
            crate::review::run(&conn, &home.cache(), command).await?;
        }
        Cmd::Artifact { command, stream } => {
            crate::artifact::run(home, cwd, server()?, stream, command).await?;
        }
        command => return Ok(Some(command)),
    }
    Ok(None)
}

/// Moves the login for the server at `from` to `to`, and points the
/// workspace around `cwd` at `to` if it used `from`.
async fn relocate(home: &Home, cwd: &Path, to: &str, from: Option<String>) -> anyhow::Result<()> {
    let ws = Workspace::discover(cwd).ok();
    let from = match (from, &ws) {
        (Some(f), _) => f,
        (None, Some(ws)) => ws.config().server.clone(),
        (None, None) => bail!("not in a workspace; pass --from"),
    };
    home.move_login(&from, to)?;
    connect(home, to).await.map_err(|e| {
        anyhow::anyhow!(
            "{e}\nIf the server was restored with a new identity, run `kelp logout {to}` and `kelp login {to}`."
        )
    })?;
    if let Some(mut ws) = ws
        && ws.config().server == from
    {
        ws.set_server(to)?;
        println!("Workspace {} now uses {to}.", ws.config().name);
    }
    println!("Moved your login from {from} to {to}.");
    Ok(())
}

fn print_doc(reply: Reply, fallback: &str) -> anyhow::Result<()> {
    let Reply::Text(text) = reply else {
        return Err(unexpected());
    };
    println!("{}", text.as_deref().unwrap_or(fallback).trim_end());
    Ok(())
}

fn format_import(i: &ImportInfo) -> String {
    let from = if i.from.is_empty() { "." } else { &i.from };
    let mut line = format!("{} {from} {}", i.source, i.to);
    if let Some(at) = i.at {
        line.push_str(&format!(" at={at}"));
    }
    if i.writable {
        line.push_str(" writable");
    }
    line
}

fn parse_imports(text: &str) -> anyhow::Result<Vec<ImportInfo>> {
    let mut imports = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let [source, from, to, rest @ ..] = words.as_slice() else {
            bail!(
                "line {}: expected `<source stream> <from> <to> [at=<cl>] [writable]`",
                n + 1
            );
        };
        let mut import = ImportInfo {
            source: (*source).to_owned(),
            from: match *from {
                "." => String::new(),
                f => f.trim_matches('/').to_owned(),
            },
            to: to.trim_matches('/').to_owned(),
            at: None,
            writable: false,
        };
        for word in rest {
            match word.strip_prefix("at=") {
                Some(cl) => import.at = Some(cl.parse()?),
                None if *word == "writable" => import.writable = true,
                None => bail!("line {}: unknown option {word:?}", n + 1),
            }
        }
        imports.push(import);
    }
    Ok(imports)
}
