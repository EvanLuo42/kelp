//! Routing workspace operations to kelpd, starting it when needed.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use kelp_client::ops::{Op, Outcome, Session};
use kelp_client::progress::Progress;
use kelp_client::{Error, Home, ipc};

use crate::progress::Bars;

const START_WAIT: Duration = Duration::from_secs(5);

/// Runs `op` through kelpd, or in this process if `in_process` is set or
/// kelpd cannot be started, showing its progress on a terminal.
pub async fn execute(home: &Home, cwd: &Path, op: Op, in_process: bool) -> anyhow::Result<Outcome> {
    let bars = Arc::new(Bars::new());
    let result = execute_with(home, cwd, op, in_process, &bars).await;
    bars.clear();
    result
}

async fn execute_with(
    home: &Home,
    cwd: &Path,
    op: Op,
    in_process: bool,
    bars: &Arc<Bars>,
) -> anyhow::Result<Outcome> {
    let in_this_process = async |op| {
        let progress: Arc<dyn Progress> = bars.clone();
        Session::new(home.clone())
            .execute_with(cwd, op, progress)
            .await
    };
    if in_process {
        tracing::debug!(?op, "running in this process");
        return Ok(in_this_process(op).await?);
    }
    tracing::debug!(?op, "running through kelpd");
    match ipc::call_with(home, cwd, op.clone(), &**bars).await {
        Ok(outcome) => return Ok(outcome),
        Err(Error::DaemonVersion(version)) => {
            tracing::info!("kelpd {version} is another version; restarting it");
            stop(home, cwd, &version).await?;
        }
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Another user serves kelpd's pipe; never talk to it, and run here.
            eprintln!("warning: {e}; running in this process");
            return Ok(in_this_process(op).await?);
        }
        Err(Error::Io(e)) => tracing::info!("kelpd is not reachable ({e}); starting it"),
        Err(e) => return Err(e.into()),
    }
    if start(home).await {
        return Ok(ipc::call_with(home, cwd, op, &**bars).await?);
    }
    eprintln!("warning: kelpd could not be started; running in this process");
    Ok(in_this_process(op).await?)
}

/// Asks the kelpd of another `version` to stop, and waits until it has.
///
/// An older kelpd answers every request of another version with its own
/// version, even a shutdown, so it cannot be stopped from here. Rather than
/// kill it, which could leave its projected workspaces mounted half way or
/// cut an operation short, this tells the user how to stop it.
async fn stop(home: &Home, cwd: &Path, version: &str) -> anyhow::Result<()> {
    let (result, pid) = shutdown(home, cwd).await;
    let refused = matches!(result, Err(Error::DaemonVersion(_)));
    if let Err(e) = &result {
        tracing::info!("kelpd did not take the shutdown: {e}");
    }
    if !refused && wait_until(home, false).await {
        return Ok(());
    }
    let (process, command) = match pid {
        Some(pid) if cfg!(windows) => (format!(" (process {pid})"), format!("taskkill /PID {pid}")),
        Some(pid) => (format!(" (process {pid})"), format!("kill {pid}")),
        None if cfg!(windows) => (String::new(), "taskkill /IM kelpd.exe".to_owned()),
        None => (String::new(), "pkill -x kelpd".to_owned()),
    };
    anyhow::bail!(
        "kelpd {version}{process} runs another version than this kelp {} and does not stop \
         when asked; stop it with `{command}` and try again, or pass --no-daemon",
        ipc::VERSION
    )
}

/// Sends kelpd a shutdown, returning its answer and, where the platform
/// tells, the id of its process.
async fn shutdown(home: &Home, cwd: &Path) -> (kelp_client::Result<Outcome>, Option<u32>) {
    use kelp_proto::MsgType;
    use kelp_proto::io::{expect_frame, write_message};
    let (mut stream, pid) = match connect_with_pid(home).await {
        Ok(connected) => connected,
        Err(e) => return (Err(e.into()), None),
    };
    let request = ipc::IpcRequest {
        version: ipc::VERSION.to_owned(),
        cwd: cwd.to_owned(),
        op: Op::Shutdown,
    };
    let result = async {
        write_message(&mut stream, MsgType::Request, &request).await?;
        loop {
            match expect_frame(&mut stream).await?.decode::<ipc::IpcReply>()? {
                ipc::IpcReply::Ok(outcome) => return Ok(outcome),
                ipc::IpcReply::Err(message) => return Err(Error::Daemon(message)),
                ipc::IpcReply::VersionMismatch(v) => return Err(Error::DaemonVersion(v)),
                ipc::IpcReply::Progress(_) => {}
            }
        }
    }
    .await;
    (result, pid)
}

#[cfg(unix)]
async fn connect_with_pid(home: &Home) -> std::io::Result<(tokio::net::UnixStream, Option<u32>)> {
    let stream = tokio::net::UnixStream::connect(home.daemon_socket()).await?;
    let pid = stream
        .peer_cred()
        .ok()
        .and_then(|c| c.pid())
        .and_then(|p| u32::try_from(p).ok());
    Ok((stream, pid))
}

#[cfg(windows)]
async fn connect_with_pid(
    home: &Home,
) -> std::io::Result<(
    tokio::net::windows::named_pipe::NamedPipeClient,
    Option<u32>,
)> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetNamedPipeServerProcessId(pipe: *mut std::ffi::c_void, pid: *mut u32) -> i32;
    }
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = ipc::pipe_name(home.dir());
    let mut attempts = 0;
    let pipe = loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&name) {
            // kelpd has no free pipe instance for a moment after each accept.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 200 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => break result?,
        }
    };
    let mut pid = 0u32;
    // SAFETY: the handle is an open pipe owned by `pipe`, and `pid` outlives the call.
    let ok = unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) };
    Ok((pipe, (ok != 0).then_some(pid)))
}

/// Starts kelpd next to the current executable and waits for it to listen.
async fn start(home: &Home) -> bool {
    let Some(exe) = std::env::current_exe().ok().and_then(|e| {
        e.parent()
            .map(|d| d.join(format!("kelpd{}", std::env::consts::EXE_SUFFIX)))
    }) else {
        return false;
    };
    tracing::debug!("starting {}", exe.display());
    let mut command = Command::new(exe);
    command
        .arg("--home")
        .arg(home.dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    if let Err(e) = spawn(&mut command) {
        tracing::info!("cannot start kelpd: {e}");
        return false;
    }
    wait_until(home, true).await
}

/// Waits until kelpd is reachable (`up`) or gone, and returns whether it happened.
async fn wait_until(home: &Home, up: bool) -> bool {
    let step = Duration::from_millis(20);
    let mut waited = Duration::ZERO;
    while waited < START_WAIT {
        if ipc::connect(home).await.is_ok() == up {
            return true;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    false
}

#[cfg(unix)]
fn spawn(command: &mut Command) -> std::io::Result<std::process::Child> {
    command.spawn()
}

/// Spawns kelpd without letting it inherit this process's standard handles.
///
/// Windows creates a child with every inheritable handle of its parent, even
/// when the child's own standard handles are set to NUL. If whoever ran
/// `kelp` captured its output through a pipe, kelpd would then hold that
/// pipe's write end for as long as it runs, and the caller would wait for the
/// end of the output until kelpd exits after its idle timeout.
#[cfg(windows)]
fn spawn(command: &mut Command) -> std::io::Result<std::process::Child> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        fn GetHandleInformation(handle: *mut std::ffi::c_void, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const STD_HANDLES: [u32; 3] = [-10i32 as u32, -11i32 as u32, -12i32 as u32];
    const HANDLE_FLAG_INHERIT: u32 = 1;
    const INVALID_HANDLE_VALUE: isize = -1;

    // Handles that were inheritable, to make so again once kelpd has started.
    let mut cleared = Vec::new();
    for which in STD_HANDLES {
        // SAFETY: plain calls; invalid or null handles make them fail.
        unsafe {
            let handle = GetStdHandle(which);
            if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags = 0u32;
            if GetHandleInformation(handle, &mut flags) != 0
                && flags & HANDLE_FLAG_INHERIT != 0
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0
            {
                cleared.push(handle);
            }
        }
    }
    let child = command.spawn();
    for handle in cleared {
        // SAFETY: the handle was valid above and is still owned by this process.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    child
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}
