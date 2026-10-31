use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use kelp_core::time::SystemClock;
use kelp_server::{Config, Server, Upstream};

#[derive(Debug, Parser)]
#[command(version, about = "Kelp version control server")]
struct Args {
    /// Data directory holding metadata, chunks and the TLS identity.
    #[arg(long, global = true, default_value = "kelp-data")]
    data: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Creates the data directory and the first administrator.
    Init {
        /// Administrator user name; defaults to the operating system user.
        #[arg(long)]
        admin: Option<String>,
        /// Reads the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Writes a backup of a stopped server into a directory; backing up into
    /// the same directory again only copies new chunks. While the server
    /// runs, use `kelp backup` instead.
    Backup {
        out: PathBuf,
        /// Leaves chunks in the server's object storage.
        #[arg(long)]
        metadata_only: bool,
    },
    /// Creates the data directory from a backup, for example on another
    /// machine, and checks it.
    Restore {
        from: PathBuf,
        /// Generates a new TLS certificate instead of keeping the old one;
        /// clients must then trust the server again.
        #[arg(long)]
        new_identity: bool,
    },
    /// Checks that every file in use can be read.
    Fsck {
        /// Also reads and hashes every chunk.
        #[arg(long)]
        deep: bool,
    },
    /// Runs the server as a systemd service that starts at boot.
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Serves the protocol until interrupted.
    Serve {
        #[arg(long, default_value = "[::]:7474")]
        listen: SocketAddr,
        /// Runs as an edge of the commit server at this address, which the
        /// data directory then replicates. Needs no `init`. Several members
        /// of a commit group can be given, separated by commas; they are
        /// tried in turn.
        #[arg(long, requires_all = ["upstream_fingerprint", "token_file"])]
        upstream: Option<String>,
        /// The fingerprint of the commit server's certificate.
        #[arg(long)]
        upstream_fingerprint: Option<String>,
        /// A file holding the token of an edge account, created with
        /// `kelp user add <name> --replica` and `kelp token <name>`.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Serves `/healthz` and Prometheus `/metrics` over plain HTTP at
        /// this address, such as `127.0.0.1:9184`.
        #[arg(long)]
        metrics: Option<SocketAddr>,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCmd {
    /// Writes a unit for this data directory, enables it at boot and starts
    /// it. A system unit needs root; `--user` installs one for the current
    /// user instead.
    Install {
        #[arg(long)]
        user: bool,
        /// The unit name.
        #[arg(long, default_value = "kelp-server")]
        name: String,
        #[arg(long, default_value = "[::]:7474")]
        listen: SocketAddr,
        /// The account a system unit runs as; it must own the data directory.
        #[arg(long, conflicts_with = "user")]
        run_as: Option<String>,
        /// Runs an edge of this commit server; see `serve --upstream`.
        #[arg(long, requires_all = ["upstream_fingerprint", "token_file"])]
        upstream: Option<String>,
        #[arg(long)]
        upstream_fingerprint: Option<String>,
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Prints the unit instead of installing it.
        #[arg(long)]
        print: bool,
    },
    /// Stops and disables the service and removes its unit.
    Uninstall {
        #[arg(long)]
        user: bool,
        #[arg(long, default_value = "kelp-server")]
        name: String,
    },
    /// Shows the service's state and recent log.
    Status {
        #[arg(long)]
        user: bool,
        #[arg(long, default_value = "kelp-server")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging();
    // The future of every command together is too large for the 1 MiB main
    // thread stack of Windows in debug builds.
    Box::pin(run(args)).await
}

async fn run(args: Args) -> anyhow::Result<()> {
    match args.command {
        Cmd::Init {
            admin,
            password_stdin,
        } => {
            let admin = match admin {
                Some(a) => a,
                None => {
                    os_user().context("cannot determine the operating system user; pass --admin")?
                }
            };
            let password = read_password(password_stdin, &format!("Password for {admin}: "))?;
            let identity = Server::init(&args.data, &admin, &password, &SystemClock)?;
            println!(
                "Initialised {} with administrator {admin}.",
                args.data.display()
            );
            println!("Server fingerprint: {}", identity.fingerprint());
        }
        Cmd::Backup { out, metadata_only } => {
            let (db, blobs) = kelp_server::backup::open_offline(&args.data)?;
            let r =
                kelp_server::backup::backup(&db, &blobs, &args.data, &out, metadata_only).await?;
            println!(
                "Backed up through log entry {} to {}: copied {} chunks ({} bytes), {} already there.",
                r.log_index,
                out.display(),
                r.chunks_copied,
                r.bytes_copied,
                r.chunks_present
            );
        }
        Cmd::Restore { from, new_identity } => {
            let report = kelp_server::backup::restore(&from, &args.data, new_identity).await?;
            print_fsck(&report);
            if !report.is_clean() {
                bail!("the restored data is incomplete");
            }
            println!("Restored {} into {}.", from.display(), args.data.display());
        }
        Cmd::Fsck { deep } => {
            let (db, blobs) = kelp_server::backup::open_offline(&args.data)?;
            let report = kelp_server::fsck::fsck(&db, &blobs, deep).await?;
            print_fsck(&report);
            if !report.is_clean() {
                bail!("the data is damaged");
            }
        }
        Cmd::Service(command) => service(&args.data, command)?,
        Cmd::Serve {
            listen,
            upstream,
            upstream_fingerprint,
            token_file,
            metrics,
        } => {
            let mut config = Config::new(&args.data, listen);
            config.metrics = metrics;
            if let (Some(addr), Some(fingerprint), Some(token_file)) =
                (upstream, upstream_fingerprint, token_file)
            {
                let token = std::fs::read_to_string(&token_file)
                    .with_context(|| format!("reading {}", token_file.display()))?;
                config.upstream = Some(Upstream {
                    addrs: addr.split(',').map(|a| a.trim().to_owned()).collect(),
                    fingerprint,
                    token: token.trim().to_owned(),
                });
            }
            let edge = config.upstream.as_ref().map(|u| u.addrs.join(", "));
            let server = Server::bind(config)?;
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                listen = %server.local_addr()?,
                data = %args.data.display(),
                metrics = ?metrics,
                "kelp-server started"
            );
            tracing::info!(fingerprint = %server.identity().fingerprint(), "server identity");
            if let Some(commit) = edge {
                tracing::info!(upstream = %commit, "running as an edge");
            }
            let shutdown = server.shutdown_handle();
            tokio::select! {
                () = server.run() => {}
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("interrupted; shutting down");
                    shutdown.shutdown();
                }
            }
            tracing::info!("kelp-server stopped");
        }
    }
    Ok(())
}

/// Logs to standard error, filtered by `KELP_LOG` or else `RUST_LOG`, such as
/// `KELP_LOG=debug` or `KELP_LOG=kelp_server=trace`.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("KELP_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,openraft=warn,quinn=warn,rustls=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
}

fn service(data: &std::path::Path, command: ServiceCmd) -> anyhow::Result<()> {
    use kelp_server::service::{self, EdgeSpec, HostSystemctl, Scope, ServiceSpec};
    let scope = |user: bool| if user { Scope::User } else { Scope::System };
    match command {
        ServiceCmd::Install {
            user,
            name,
            listen,
            run_as,
            upstream,
            upstream_fingerprint,
            token_file,
            print,
        } => {
            let edge = match (upstream, upstream_fingerprint, token_file) {
                (Some(upstream), Some(fingerprint), Some(token_file)) => Some(EdgeSpec {
                    upstream,
                    fingerprint,
                    token_file: std::path::absolute(token_file)?,
                }),
                _ => None,
            };
            if edge.is_none() && !data.join("meta.redb").is_file() {
                bail!(
                    "{} is not initialised; run `kelp-server --data {} init` first",
                    data.display(),
                    data.display()
                );
            }
            std::fs::create_dir_all(data)?;
            let spec = ServiceSpec {
                name,
                scope: scope(user),
                binary: std::env::current_exe()?,
                data_dir: std::fs::canonicalize(data)?,
                listen,
                run_as,
                edge,
            };
            if print {
                print!("{}", service::unit_file(&spec));
                return Ok(());
            }
            let path = service::install(&spec, &service::unit_dir(spec.scope)?, &HostSystemctl)?;
            println!("Installed {} and started it.", path.display());
            if user {
                println!(
                    "It starts when you log in. To start it at boot, run `loginctl enable-linger $USER`."
                );
            }
        }
        ServiceCmd::Uninstall { user, name } => {
            let scope = scope(user);
            service::uninstall(&name, scope, &service::unit_dir(scope)?, &HostSystemctl)?;
            println!("Removed {name}.service.");
        }
        ServiceCmd::Status { user, name } => {
            let mut command = std::process::Command::new("systemctl");
            if user {
                command.arg("--user");
            }
            command
                .args(["status", "--no-pager", &format!("{name}.service")])
                .status()?;
        }
    }
    Ok(())
}

fn print_fsck(r: &kelp_server::fsck::FsckReport) {
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
}

fn os_user() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty())
}

fn read_password(from_stdin: bool, prompt: &str) -> anyhow::Result<String> {
    let password = if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    } else {
        let first = rpassword::prompt_password(prompt)?;
        if rpassword::prompt_password("Repeat: ")? != first {
            bail!("passwords do not match");
        }
        first
    };
    if password.len() < 8 {
        bail!("password must be at least 8 characters");
    }
    Ok(password)
}
