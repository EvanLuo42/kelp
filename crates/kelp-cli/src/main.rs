//! The `kelp` command-line client.

mod admin;
mod artifact;
mod daemon;
mod difftool;
mod issue;
mod plugin;
mod progress;
mod review;
mod util;
mod webhook;
mod ws;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "kelp", version, about = "Kelp version control")]
struct Cli {
    /// Runs as if started in this directory.
    #[arg(short = 'C', global = true)]
    dir: Option<PathBuf>,
    /// The server for commands that run outside a workspace.
    #[arg(long, global = true)]
    server: Option<String>,
    /// Runs workspace commands in this process instead of kelpd.
    #[arg(
        long,
        global = true,
        env = "KELP_NO_DAEMON",
        action = clap::ArgAction::SetTrue,
        value_parser = clap::builder::FalseyValueParser::new(),
    )]
    no_daemon: bool,
    /// Logs more to standard error with each `-v`, up to `-vvv`.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Logs in to a server, trusting its certificate on first use.
    Login {
        server: String,
        /// User name; defaults to the operating system user.
        #[arg(long)]
        user: Option<String>,
        /// Reads the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
        /// Logs in through the server's single sign-on provider in a browser.
        #[arg(long, conflicts_with_all = ["user", "password_stdin"])]
        sso: bool,
        /// Reads an access token, such as one from `kelp token`, from
        /// standard input and logs in with it. Build machines log in to
        /// service accounts this way.
        #[arg(long, conflicts_with_all = ["user", "password_stdin", "sso"])]
        token_stdin: bool,
        /// Links the single sign-on account to the user you are logged in
        /// as, who from then on logs in only through single sign-on.
        #[arg(long, requires = "sso")]
        link: bool,
    },
    /// Turns two-factor login on or off.
    ///
    /// The second factor is a code from an authenticator app.
    #[command(subcommand)]
    Totp(TotpCmd),
    /// Forgets the token for a server.
    Logout { server: String },
    /// Changes a password.
    ///
    /// Changes your own password, or with `--user`, another user's
    /// (administrators only).
    Passwd {
        #[arg(long)]
        user: Option<String>,
        /// Reads the current and new password from standard input, one per
        /// line (only the new one with `--user`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Manages users (administrators only).
    #[command(subcommand)]
    User(UserCmd),
    /// Issues an access token (administrators only).
    ///
    /// Typically for a service account.
    Token {
        user: String,
        #[arg(long, default_value_t = 90)]
        days: u32,
    },
    /// Manages groups.
    #[command(subcommand)]
    Group(GroupCmd),
    /// Shows or replaces the protections table (administrators only).
    #[command(subcommand)]
    Protect(DocCmd),
    /// Backs up the server (administrators only).
    ///
    /// Writes the backup into a directory on the server's machine.
    /// Restore it with `kelp-server restore`.
    Backup {
        dest: String,
        /// Leaves chunks in the server's object storage.
        #[arg(long)]
        metadata_only: bool,
    },
    /// Checks the server's files can be read (administrators only).
    Fsck {
        /// Also reads and hashes every chunk.
        #[arg(long)]
        deep: bool,
    },
    /// Points the workspace and login at a moved server.
    ///
    /// Use it after the server changes address, for example after
    /// `kelp-server restore` on another machine.
    Relocate {
        to: String,
        /// The old address; defaults to the workspace's server.
        #[arg(long)]
        from: Option<String>,
    },
    /// Deletes content nothing refers to any more (administrators only).
    Gc {
        /// Reports what would be deleted without deleting it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Moves unused history to cold storage (administrators only).
    ///
    /// Moves history that no stream currently uses.
    /// Needs tiered storage.
    Tier,
    /// Permanently removes files and history (administrators only).
    ///
    /// A path ending in `/...` names a directory.
    Obliterate {
        paths: Vec<String>,
        /// Confirms that history should be destroyed.
        #[arg(long)]
        yes: bool,
    },
    /// Shows or verifies the audit log (administrators only).
    Audit {
        /// The first log index to show.
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: u32,
        /// Checks the hash chain against the command log instead of listing it.
        #[arg(long)]
        verify: bool,
    },
    /// Manages depots (administrators only).
    #[command(subcommand)]
    Depot(DepotCmd),
    /// Registers and lists WASM plugins.
    #[command(subcommand)]
    Plugin(plugin::PluginCmd),
    /// Manages webhooks (administrators only).
    #[command(subcommand)]
    Webhook(webhook::WebhookCmd),
    /// Shows where the changes linked to issues have gone.
    #[command(subcommand)]
    Issue(issue::IssueCmd),
    /// Manages reviews of shelves.
    ///
    /// Reviews feed the submit gate.
    #[command(subcommand)]
    Review(review::ReviewCmd),
    /// Manages build artifacts.
    Artifact {
        #[command(subcommand)]
        command: artifact::ArtifactCmd,
        /// The stream; defaults to the workspace's.
        #[arg(long, global = true)]
        stream: Option<String>,
    },
    /// Shows or replaces a depot's typemap.
    Typemap {
        #[command(subcommand)]
        command: DocCmd,
        /// The depot; defaults to the workspace's.
        #[arg(long, global = true)]
        depot: Option<String>,
    },
    /// Manages streams.
    #[command(subcommand)]
    Stream(StreamCmd),
    /// Lists streams.
    Streams,
    /// Shows the commit group as the server sees it (administrators).
    Cluster,
    /// Creates a workspace bound to a stream.
    ///
    /// The workspace goes in `directory`, or the current directory.
    Init {
        stream: String,
        directory: Option<PathBuf>,
        /// The workspace name; defaults to the directory name plus a random suffix.
        #[arg(long)]
        name: Option<String>,
        /// Creates a projected workspace: files appear on sync and download on
        /// first use. Needs kelpd and, on Linux, FUSE.
        #[arg(long)]
        projected: bool,
    },
    /// Unmounts the projected workspace.
    ///
    /// Unmounts the one in the current directory until kelpd mounts it again.
    Unmount,
    /// Syncs the workspace to a changelist, label or the head.
    ///
    /// Brings the workspace, or only the paths given, to a changelist or
    /// label, or to the head of its stream: `kelp sync`, `kelp sync @120`,
    /// `kelp sync Content/Maps @ship-1.0`.
    Sync { targets: Vec<String> },
    /// Manages labels.
    ///
    /// A label names the workspace's stream as of a changelist, or its head.
    #[command(subcommand)]
    Label(LabelCmd),
    /// Shows opened files and unopened changes.
    Status,
    /// Opens all unopened changes in the default changelist.
    Reconcile,
    /// Opens new files for add.
    Add {
        paths: Vec<PathBuf>,
        #[arg(short, long)]
        cl: Option<String>,
    },
    /// Opens files for edit, locking exclusive files.
    Edit {
        paths: Vec<PathBuf>,
        #[arg(short, long)]
        cl: Option<String>,
    },
    /// Opens files for delete and removes them from disk.
    Delete {
        paths: Vec<PathBuf>,
        #[arg(short, long)]
        cl: Option<String>,
    },
    /// Reverts opened files.
    Revert { paths: Vec<PathBuf> },
    /// Resolves files changed both here and in the stream.
    Resolve {
        paths: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = Accept::Merge)]
        accept: Accept,
    },
    /// Manages pending changelists.
    #[command(subcommand)]
    Cl(ClCmd),
    /// Submits a pending changelist.
    Submit {
        #[arg(short, long)]
        cl: Option<String>,
        /// The description; defaults to the changelist's description.
        #[arg(short, long)]
        message: Option<String>,
        /// An issue the changelist fixes, such as `PROJ-123`; may be
        /// repeated. Issues the description mentions are linked too.
        #[arg(long = "fixes")]
        fixes: Vec<String>,
    },
    /// Shelves a pending changelist on the server.
    Shelve {
        #[arg(short, long)]
        cl: Option<String>,
    },
    /// Opens the files of a shelf in this workspace.
    ///
    /// The files must be at the revisions the shelf was based on. `--sync`
    /// brings just those files there first, which is what a reviewer wants
    /// in a clean workspace.
    Unshelve {
        shelf: u64,
        #[arg(short, long)]
        cl: Option<String>,
        /// Syncs the shelf's files to the revisions it was based on first.
        #[arg(short, long)]
        sync: bool,
    },
    /// Deletes a shelf.
    DeleteShelf { shelf: u64 },
    /// Lists shelves, newest first.
    Shelves {
        /// Limits the list to one stream.
        #[arg(short, long)]
        stream: Option<String>,
        /// Limits the list to the shelves of one user.
        #[arg(short, long)]
        user: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Merges another stream into a new changelist.
    ///
    /// The source defaults to the parent stream.
    /// With `--cl`, picks only that changelist's changes, from the stream it
    /// was submitted to unless `--from` names another.
    Merge {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        cl: Option<u64>,
    },
    /// Opens the reverse of a submitted changelist in a new changelist.
    Undo { cl: u64 },
    /// Copies this workspace's stream into its parent.
    CopyUp {
        #[arg(short, long)]
        message: String,
    },
    /// Lists locked files in the stream's lock domain.
    Locks,
    /// Releases locks.
    ///
    /// `--force` releases anyone's (administrators only).
    Unlock {
        paths: Vec<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    /// Manages snapshots of pending changelists.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// Shows the state of files.
    ///
    /// Whether each is opened, modified, locked or out of date.
    Fstat { paths: Vec<PathBuf> },
    /// Shows how files differ from the versions synced.
    ///
    /// Covers opened files and unopened changes, under `paths` if given.
    /// Files whose type names a differ plugin are shown as the plugin
    /// describes them.
    Diff {
        paths: Vec<PathBuf>,
        /// Opens each file in a diff tool instead, such as
        /// `"meld {old} {new}"`; defaults to `KELP_DIFF_TOOL`.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Discards changes to files that are not opened.
    ///
    /// Puts files that are not opened back as synced and deletes files the
    /// workspace does not track. The discarded changes are not kept.
    Clean {
        paths: Vec<PathBuf>,
        /// Only lists what would be restored or deleted.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Moves or renames files, keeping their history.
    ///
    /// Works on a file or a directory of files.
    Move {
        from: PathBuf,
        to: PathBuf,
        #[arg(short, long)]
        cl: Option<String>,
    },
    /// Shows who last changed each line of a file.
    ///
    /// Prints each line of a text file with the changelist and author that
    /// last changed it.
    Annotate { path: String },
    /// Prints a file as of a changelist.
    ///
    /// For example `kelp print Source/main.rs@120`.
    /// Depot paths such as `//engine/main/a.h` work too.
    Print { path: String },
    /// Lists the revisions of a file.
    Filelog { path: PathBuf },
    /// Lists recent changelists.
    Changes {
        /// Limits the list to one stream; defaults to the workspace's.
        #[arg(short, long)]
        stream: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Shows one changelist.
    Describe { cl: u64 },
}

#[derive(Debug, Subcommand)]
enum SnapshotCmd {
    /// Saves the opened files of a changelist.
    Save {
        #[arg(short, long)]
        cl: Option<String>,
        #[arg(short, long, default_value = "")]
        message: String,
    },
    /// Lists snapshots.
    List,
    /// Restores a snapshot's files and reopens them.
    Restore { id: u32 },
    /// Deletes a snapshot.
    Delete { id: u32 },
    /// Shows the differences between two snapshots, or a snapshot and the disk.
    Diff {
        old: u32,
        new: Option<u32>,
        /// Opens each file in a diff tool instead; defaults to
        /// `KELP_DIFF_TOOL`.
        #[arg(long)]
        tool: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Accept {
    /// Merges text automatically; other files need a choice.
    Merge,
    Yours,
    Theirs,
}

#[derive(Debug, Subcommand)]
enum TotpCmd {
    /// Requires a code from an authenticator app when you log in.
    Enable,
    /// Stops requiring a code; administrators can do this for any user.
    Disable {
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum UserCmd {
    /// Creates a user.
    Add {
        name: String,
        #[arg(long)]
        admin: bool,
        /// Creates a service account that has no password and uses tokens.
        #[arg(long)]
        service: bool,
        /// Creates the service account an edge server replicates with.
        #[arg(long)]
        replica: bool,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Shows a user, by current or former name.
    Show { name: String },
    /// Lists every user.
    List,
    /// Stops a user from logging in.
    ///
    /// Also revokes their tokens; their history and locks stay.
    Disable { name: String },
    /// Lets a disabled user log in again.
    Enable { name: String },
    /// Links a single sign-on name to a user.
    ///
    /// Lets the first single sign-on as `sso_name` log in as `name` and
    /// link the two, for a provider user name that differs.
    Link {
        name: String,
        #[arg(long = "sso")]
        sso_name: String,
    },
    /// Removes a user's single sign-on link.
    Unlink { name: String },
    /// Merges or renames a user.
    ///
    /// Merges user `from` into `into`, or renames `from` if `into` does not
    /// exist; history keeps the old name, which becomes an alias.
    Merge { from: String, into: String },
    /// Sets the name shown for a user in changes and reviews.
    ///
    /// Without a name, clears it. Defaults to yourself; others need an
    /// administrator.
    Name {
        display_name: Option<String>,
        #[arg(long)]
        user: Option<String>,
    },
    /// Adds an email address to a user; the first becomes the primary one.
    AddEmail {
        email: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Removes an email address from a user.
    RemoveEmail {
        email: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Makes one of a user's email addresses the primary one.
    PrimaryEmail {
        email: String,
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum GroupCmd {
    /// Sets the members of a group; no members deletes it.
    Set { name: String, members: Vec<String> },
    /// Lists groups.
    List,
}

#[derive(Debug, Subcommand)]
enum DocCmd {
    /// Prints the current document.
    Get,
    /// Replaces the document with the contents of a file.
    Set { file: PathBuf },
}

#[derive(Debug, Subcommand)]
enum LabelCmd {
    /// Sets a label, moving it if it exists.
    Set {
        name: String,
        /// The changelist; defaults to the head.
        #[arg(long)]
        cl: Option<u64>,
        /// The stream; defaults to the workspace's.
        #[arg(long)]
        stream: Option<String>,
        #[arg(long, short, default_value = "")]
        message: String,
    },
    /// Deletes a label.
    Delete { name: String },
    /// Lists labels.
    List,
}

#[derive(Debug, Subcommand)]
enum DepotCmd {
    /// Creates a depot and its `main` stream.
    Create { name: String },
}

#[derive(Debug, Subcommand)]
enum StreamCmd {
    /// Creates a stream branching from a parent.
    Create {
        path: String,
        #[arg(long)]
        parent: String,
        #[arg(long, value_enum, default_value_t = Kind::Development)]
        kind: Kind,
    },
    /// Deletes a stream, keeping its history.
    ///
    /// Administrators delete any stream but a mainline; others delete task
    /// streams.
    Delete {
        path: String,
        /// Confirms the deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Shows or replaces a stream's imports (administrators replace them).
    ///
    /// The document has one import per line:
    /// `<source stream> <from> <to> [at=<cl>] [writable]`, where `from` is a
    /// directory of the source, or `.` for all of it, and `to` is where it
    /// appears in the stream. Lines starting with `#` are ignored.
    Imports {
        #[command(subcommand)]
        command: DocCmd,
        /// The stream; defaults to the workspace's.
        #[arg(long, global = true)]
        stream: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Kind {
    Development,
    Release,
    Task,
}

#[derive(Debug, Subcommand)]
enum ClCmd {
    /// Creates a pending changelist.
    New { description: String },
    /// Lists pending changelists.
    List,
    /// Deletes a pending changelist, moving its files to the default changelist.
    Delete { cl: String },
    /// Moves opened files to another changelist.
    Move { cl: String, paths: Vec<PathBuf> },
    /// Lists the hunks of an opened text file, or moves one to another changelist.
    Split {
        path: PathBuf,
        /// The hunk number to move, as listed.
        #[arg(long, requires = "to")]
        hunk: Option<usize>,
        /// The changelist to move the hunk to.
        #[arg(long)]
        to: Option<String>,
    },
}

fn main() {
    // Parsing this many subcommands and polling the future of every command
    // together overflow the 1 MiB main thread stack of Windows in debug
    // builds, so everything runs on a thread with a larger stack.
    std::thread::Builder::new()
        .name("kelp".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Runtime::new()
                .expect("start the async runtime")
                .block_on(async_main())
        })
        .expect("spawn the main thread")
        .join()
        .unwrap_or_else(|_| std::process::exit(101));
}

async fn async_main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    if let Err(e) = Box::pin(run(cli)).await {
        tracing::debug!("{e:?}");
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Logs warnings to standard error, and more with each `-v`; `KELP_LOG` or
/// else `RUST_LOG` overrides the level.
fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("KELP_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .without_time()
        .with_target(verbose > 1)
        .init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let home = util::home()?;
    let cwd = match &cli.dir {
        Some(d) => std::env::current_dir()?.join(d),
        None => std::env::current_dir()?,
    };
    match admin::run(&home, &cwd, cli.server, cli.command).await? {
        Some(command) => ws::run(&home, &cwd, command, cli.no_daemon).await,
        None => Ok(()),
    }
}
