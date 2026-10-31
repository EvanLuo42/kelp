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
            let _ = ipc::call(home, cwd, Op::Shutdown).await;
            wait_until(home, false).await;
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
    if let Err(e) = command.spawn() {
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
