//! The local channel between Kelp clients and kelpd.
//!
//! Each exchange is one [`IpcRequest`] frame followed by any number of
//! [`IpcReply::Progress`] frames and one final [`IpcReply`] frame, using the
//! `kelp/1` framing. On Unix the channel is a socket in the
//! Kelp home directory; on Windows it is a named pipe derived from it.

use std::path::{Path, PathBuf};

use kelp_proto::MsgType;
use kelp_proto::io::{expect_frame, write_message};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::ops::{Op, Outcome};
use crate::progress::{Progress, Quiet, Update};
use crate::{Error, Home, Result};

/// The version clients and kelpd must agree on.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A request to kelpd.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRequest {
    /// The client's [`VERSION`].
    pub version: String,
    /// The directory the operation runs in.
    pub cwd: PathBuf,
    pub op: Op,
}

/// kelpd's answer to an [`IpcRequest`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum IpcReply {
    Ok(Outcome),
    Err(String),
    /// kelpd runs a different version and must be restarted.
    VersionMismatch(String),
    /// How far the operation got; more frames follow. Kept last so that
    /// clients of other versions still decode the variants above.
    Progress(Update),
}

/// Returns the name of the named pipe for `home` on Windows.
pub fn pipe_name(home: &Path) -> String {
    let hash = blake3::hash(home.to_string_lossy().as_bytes()).to_hex();
    format!(r"\\.\pipe\kelpd-{}", &hash[..16])
}

/// Connects to the kelpd serving `home`.
///
/// # Errors
///
/// Returns an error if no kelpd is listening.
#[cfg(unix)]
pub async fn connect(home: &Home) -> std::io::Result<impl AsyncRead + AsyncWrite + Unpin> {
    tokio::net::UnixStream::connect(home.daemon_socket()).await
}

/// Connects to the kelpd serving `home`.
///
/// # Errors
///
/// Returns an error if no kelpd is listening.
#[cfg(windows)]
pub async fn connect(home: &Home) -> std::io::Result<impl AsyncRead + AsyncWrite + Unpin> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = pipe_name(home.dir());
    // Between accepting one client and creating the next pipe instance,
    // kelpd has none free; that lasts moments, so wait it out.
    let mut attempts = 0;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&name) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 200 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            result => return result,
        }
    }
}

/// Sends `op` to kelpd and returns its outcome.
///
/// # Errors
///
/// Returns [`Error::Io`] if kelpd cannot be reached, [`Error::Daemon`] if the
/// operation failed or kelpd runs another version, or a protocol error.
pub async fn call(home: &Home, cwd: &Path, op: Op) -> Result<Outcome> {
    call_with(home, cwd, op, &Quiet).await
}

/// Like [`call`], passing kelpd's progress reports to `progress`.
///
/// # Errors
///
/// As for [`call`].
pub async fn call_with(
    home: &Home,
    cwd: &Path,
    op: Op,
    progress: &dyn Progress,
) -> Result<Outcome> {
    let mut stream = connect(home).await?;
    let request = IpcRequest {
        version: VERSION.to_owned(),
        cwd: cwd.to_owned(),
        op,
    };
    write_message(&mut stream, MsgType::Request, &request).await?;
    loop {
        let frame = expect_frame(&mut stream).await?;
        match frame.decode::<IpcReply>()? {
            IpcReply::Ok(outcome) => return Ok(outcome),
            IpcReply::Err(message) => return Err(Error::Daemon(message)),
            IpcReply::VersionMismatch(version) => return Err(Error::DaemonVersion(version)),
            IpcReply::Progress(update) => progress.update(update),
        }
    }
}
