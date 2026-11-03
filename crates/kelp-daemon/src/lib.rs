//! kelpd, the per-user Kelp daemon.
//!
//! kelpd listens on the local channel described in [`kelp_client::ipc`] and
//! runs workspace operations for the CLI and the C SDK. Keeping one process
//! alive lets it reuse server connections and, later, watch workspaces and
//! serve projected file systems.

mod projected;
mod watch;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kelp_client::Home;
use kelp_client::ipc::{IpcReply, IpcRequest, VERSION};
use kelp_client::ops::{Op, Session};
use kelp_client::progress::{Progress, Update};
use kelp_proto::MsgType;
use kelp_proto::io::{expect_frame, write_message};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

/// Settings for [`serve`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The Kelp home whose user kelpd serves.
    pub home: PathBuf,
    /// Exit after this long without requests; `None` runs until shut down.
    pub idle_timeout: Option<Duration>,
}

struct Daemon {
    session: Arc<Session>,
    mounts: projected::Mounts,
    watchers: watch::Watchers,
    started: Instant,
    last_request: AtomicU64,
    /// Requests being handled; kelpd never stops for idleness while any are.
    in_flight: AtomicU64,
    shutdown: Notify,
}

impl Daemon {
    fn touch(&self) {
        self.last_request
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_request.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }
}

/// Serves requests until shut down by [`Op::Shutdown`] or the idle timeout.
///
/// # Errors
///
/// Returns an error if the channel cannot be created, for example because
/// another kelpd already serves the same home.
pub async fn serve(config: Config) -> io::Result<()> {
    let home = Home::new(&config.home);
    std::fs::create_dir_all(home.dir())?;
    let session = Arc::new(Session::new(home.clone()));
    let daemon = Arc::new(Daemon {
        mounts: projected::Mounts::new(session.clone(), tokio::runtime::Handle::current()),
        session,
        watchers: watch::Watchers::default(),
        started: Instant::now(),
        last_request: AtomicU64::new(0),
        in_flight: AtomicU64::new(0),
        shutdown: Notify::new(),
    });
    let mut listener = Listener::bind(&home).await?;
    tracing::info!(
        version = VERSION,
        home = %home.dir().display(),
        idle_timeout = ?config.idle_timeout,
        "kelpd started"
    );
    {
        let daemon = daemon.clone();
        tokio::task::spawn_blocking(move || daemon.mounts.mount_registered()).await?;
    }
    let idle_check = config.idle_timeout.map(|t| t.min(Duration::from_secs(30)));
    loop {
        let idle = async {
            match idle_check {
                Some(tick) => tokio::time::sleep(tick).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            stream = listener.accept() => {
                let stream = stream?;
                let daemon = daemon.clone();
                daemon.touch();
                daemon.in_flight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Err(e) = handle(&daemon, stream).await {
                        tracing::warn!("request failed: {e}");
                    }
                    daemon.touch();
                    daemon.in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            () = daemon.shutdown.notified() => break,
            () = idle => {
                let busy = daemon.in_flight.load(Ordering::SeqCst) > 0;
                if !busy && config.idle_timeout.is_some_and(|t| daemon.idle_for() >= t) && !daemon.mounts.any() {
                    tracing::info!("idle; exiting");
                    break;
                }
            }
        }
    }
    listener.close();
    let daemon_for_unmount = daemon.clone();
    tokio::task::spawn_blocking(move || daemon_for_unmount.mounts.unmount_all()).await?;
    tracing::info!("kelpd stopped");
    Ok(())
}

async fn handle(
    daemon: &Arc<Daemon>,
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
) -> kelp_proto::Result<()> {
    let request: IpcRequest = expect_frame(&mut stream).await?.decode()?;
    let cwd = request.cwd.clone();
    tracing::debug!(cwd = %cwd.display(), op = ?request.op, "request");
    let started = Instant::now();
    let reply = if request.version != VERSION {
        tracing::info!(client = %request.version, "client version differs");
        IpcReply::VersionMismatch(VERSION.to_owned())
    } else if request.op == Op::Shutdown {
        daemon.shutdown.notify_one();
        IpcReply::Ok(kelp_client::ops::Outcome::Done)
    } else if let Op::Mount { upper } = &request.op {
        let (daemon, root, upper) = (daemon.clone(), request.cwd.clone(), upper.clone());
        match tokio::task::spawn_blocking(move || daemon.mounts.mount(&root, &upper)).await {
            Ok(Ok(())) => IpcReply::Ok(kelp_client::ops::Outcome::Done),
            Ok(Err(e)) => IpcReply::Err(format!("cannot mount: {e}")),
            Err(e) => IpcReply::Err(e.to_string()),
        }
    } else if request.op == Op::Unmount {
        let (daemon, root) = (daemon.clone(), request.cwd.clone());
        match tokio::task::spawn_blocking(move || daemon.mounts.unmount(&root)).await {
            Ok(Ok(())) => IpcReply::Ok(kelp_client::ops::Outcome::Done),
            Ok(Err(e)) => IpcReply::Err(format!("cannot unmount: {e}")),
            Err(e) => IpcReply::Err(e.to_string()),
        }
    } else if request.op == Op::Status {
        match status(daemon, &request.cwd).await {
            Ok(outcome) => IpcReply::Ok(outcome),
            Err(e) => IpcReply::Err(e.to_string()),
        }
    } else {
        match execute(daemon, request, &mut stream).await {
            Ok(outcome) => IpcReply::Ok(outcome),
            Err(e) => IpcReply::Err(e.to_string()),
        }
    };
    daemon.touch();
    let ms = started.elapsed().as_millis() as u64;
    match &reply {
        IpcReply::Err(e) => tracing::info!(cwd = %cwd.display(), ms, "request failed: {e}"),
        _ => tracing::debug!(cwd = %cwd.display(), ms, "request done"),
    }
    write_message(&mut stream, MsgType::Reply, &reply).await
}

/// Runs a workspace operation, sending its progress to the client as it goes.
///
/// A client that goes away gets no more progress, but the operation still
/// completes, as it would without progress.
async fn execute(
    daemon: &Arc<Daemon>,
    request: IpcRequest,
    stream: &mut (impl AsyncWrite + Unpin),
) -> kelp_client::Result<kelp_client::ops::Outcome> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let progress: Arc<dyn Progress> = Arc::new(move |update: Update| {
        let _ = tx.send(update);
    });
    let work = daemon
        .session
        .execute_with(&request.cwd, request.op, progress);
    tokio::pin!(work);
    let mut listening = true;
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            Some(update) = rx.recv(), if listening => {
                listening = write_message(stream, MsgType::Reply, &IpcReply::Progress(update))
                    .await
                    .is_ok();
            }
        }
    };
    // The last updates may still be queued, such as the one finishing a phase.
    while listening && let Ok(update) = rx.try_recv() {
        listening = write_message(stream, MsgType::Reply, &IpcReply::Progress(update))
            .await
            .is_ok();
    }
    result
}

/// Answers status from the workspace's watched index, without listing the disk.
///
/// Unless another operation holds the workspace, the status also records
/// the files that hashing found unchanged, so the next one need not hash
/// them again; otherwise it only reads.
async fn status(
    daemon: &Arc<Daemon>,
    cwd: &std::path::Path,
) -> kelp_client::Result<kelp_client::ops::Outcome> {
    let ws = kelp_client::Workspace::discover(cwd)?;
    let daemon = daemon.clone();
    tokio::task::spawn_blocking(move || {
        let index = daemon.watchers.index(&ws);
        let refreshed = kelp_client::ops::if_unlocked(ws.root(), || {
            // Read the state again now that no operation can change it.
            let mut ws = kelp_client::Workspace::open(ws.root())?;
            let status = ws.refresh_status(index.as_ref())?;
            Ok::<_, kelp_client::Error>(kelp_client::ops::status_outcome(&ws, status))
        })?;
        if let Some(outcome) = refreshed {
            return outcome;
        }
        let status = match index {
            Some(index) => ws.status_indexed(&index)?,
            None => ws.status()?,
        };
        Ok(kelp_client::ops::status_outcome(&ws, status))
    })
    .await
    .map_err(|e| kelp_client::Error::Daemon(e.to_string()))?
}

#[cfg(unix)]
struct Listener {
    inner: tokio::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl Listener {
    async fn bind(home: &Home) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let path = home.daemon_socket();
        if kelp_client::ipc::connect(home).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "kelpd is already running",
            ));
        }
        let _ = std::fs::remove_file(&path);
        let inner = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { inner, path })
    }

    async fn accept(&mut self) -> io::Result<tokio::net::UnixStream> {
        Ok(self.inner.accept().await?.0)
    }

    fn close(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
struct Listener {
    name: String,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl Listener {
    async fn bind(home: &Home) -> io::Result<Self> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = kelp_client::ipc::pipe_name(home.dir());
        let next = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)?;
        Ok(Self { name, next })
    }

    async fn accept(&mut self) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        use tokio::net::windows::named_pipe::ServerOptions;
        self.next.connect().await?;
        let fresh = ServerOptions::new().create(&self.name)?;
        Ok(std::mem::replace(&mut self.next, fresh))
    }

    fn close(self) {}
}
