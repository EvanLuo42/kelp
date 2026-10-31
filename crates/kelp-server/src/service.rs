//! Running the server as a systemd service that starts at boot.
//!
//! [`install`] writes a unit file and enables it; [`uninstall`] reverses
//! that. Both talk to systemd through a [`Systemctl`], so the unit can be
//! inspected without touching the machine's services.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

/// Whether a unit belongs to the whole machine or to one user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A system unit, started at boot; installing it needs root.
    System,
    /// A user unit, started at the user's login, or at boot with lingering
    /// enabled (`loginctl enable-linger`).
    User,
}

/// The edge settings of a service; see [`crate::Upstream`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeSpec {
    pub upstream: String,
    pub fingerprint: String,
    pub token_file: PathBuf,
}

/// What a service runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The unit name without `.service`.
    pub name: String,
    pub scope: Scope,
    /// The `kelp-server` executable.
    pub binary: PathBuf,
    /// The absolute data directory.
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    /// The account a system service runs as; root if `None`.
    pub run_as: Option<String>,
    pub edge: Option<EdgeSpec>,
}

/// Runs `systemctl`.
pub trait Systemctl {
    /// Runs `systemctl` with `args` for units of `scope`.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be run or fails.
    fn run(&self, scope: Scope, args: &[&str]) -> Result<()>;
}

/// The machine's real `systemctl`.
pub struct HostSystemctl;

impl Systemctl for HostSystemctl {
    fn run(&self, scope: Scope, args: &[&str]) -> Result<()> {
        let mut command = Command::new("systemctl");
        if scope == Scope::User {
            command.arg("--user");
        }
        let output = command.args(args).output()?;
        match output.status.success() {
            true => Ok(()),
            false => Err(Error::Config(format!(
                "systemctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }
}

/// Returns the directory systemd reads units of `scope` from.
///
/// # Errors
///
/// Returns [`Error::Config`] if the home directory of a user unit is unknown.
pub fn unit_dir(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::System => Ok(PathBuf::from("/etc/systemd/system")),
        Scope::User => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
                .ok_or_else(|| Error::Config("cannot find the home directory".into()))?;
            Ok(config.join("systemd").join("user"))
        }
    }
}

/// Returns the text of the unit file for `spec`.
pub fn unit_file(spec: &ServiceSpec) -> String {
    let mut exec = vec![
        quote(&spec.binary.display().to_string()),
        "--data".into(),
        quote(&spec.data_dir.display().to_string()),
        "serve".into(),
        "--listen".into(),
        spec.listen.to_string(),
    ];
    if let Some(edge) = &spec.edge {
        exec.extend([
            "--upstream".into(),
            quote(&edge.upstream),
            "--upstream-fingerprint".into(),
            quote(&edge.fingerprint),
            "--token-file".into(),
            quote(&edge.token_file.display().to_string()),
        ]);
    }
    let role = if spec.edge.is_some() {
        "edge"
    } else {
        "server"
    };
    let mut unit = String::new();
    let _ = writeln!(unit, "# Written by `kelp-server service install`.");
    let _ = writeln!(unit, "[Unit]");
    let _ = writeln!(
        unit,
        "Description=Kelp {role} ({})",
        escape(&spec.data_dir.display().to_string())
    );
    let _ = writeln!(unit, "After=network-online.target");
    let _ = writeln!(unit, "Wants=network-online.target");
    let _ = writeln!(unit);
    let _ = writeln!(unit, "[Service]");
    let _ = writeln!(unit, "Type=simple");
    let _ = writeln!(unit, "ExecStart={}", exec.join(" "));
    if let (Scope::System, Some(user)) = (spec.scope, &spec.run_as) {
        let _ = writeln!(unit, "User={user}");
    }
    let _ = writeln!(unit, "Restart=on-failure");
    let _ = writeln!(unit, "RestartSec=5");
    let _ = writeln!(unit, "KillSignal=SIGINT");
    let _ = writeln!(unit, "TimeoutStopSec=60");
    let _ = writeln!(unit, "LimitNOFILE=65536");
    let _ = writeln!(unit);
    let _ = writeln!(unit, "[Install]");
    let target = match spec.scope {
        Scope::System => "multi-user.target",
        Scope::User => "default.target",
    };
    let _ = writeln!(unit, "WantedBy={target}");
    unit
}

/// Writes the unit of `spec` into `unit_dir`, then has systemd load it,
/// enable it at boot and start it. Returns the unit file's path.
///
/// # Errors
///
/// Returns an error if the unit cannot be written or `systemctl` fails.
pub fn install(spec: &ServiceSpec, unit_dir: &Path, systemctl: &dyn Systemctl) -> Result<PathBuf> {
    std::fs::create_dir_all(unit_dir)?;
    let path = unit_dir.join(format!("{}.service", spec.name));
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, unit_file(spec))?;
    std::fs::rename(&tmp, &path)?;
    systemctl.run(spec.scope, &["daemon-reload"])?;
    systemctl.run(
        spec.scope,
        &["enable", "--now", &format!("{}.service", spec.name)],
    )?;
    Ok(path)
}

/// Stops and disables the service `name` and removes its unit from
/// `unit_dir`.
///
/// # Errors
///
/// Returns [`Error::Config`] if there is no such unit, or an error if
/// `systemctl` fails.
pub fn uninstall(
    name: &str,
    scope: Scope,
    unit_dir: &Path,
    systemctl: &dyn Systemctl,
) -> Result<()> {
    let path = unit_dir.join(format!("{name}.service"));
    if !path.is_file() {
        return Err(Error::Config(format!(
            "there is no unit {}",
            path.display()
        )));
    }
    systemctl.run(scope, &["disable", "--now", &format!("{name}.service")])?;
    std::fs::remove_file(&path)?;
    systemctl.run(scope, &["daemon-reload"])
}

/// Quotes an `ExecStart` argument for systemd.
fn quote(arg: &str) -> String {
    let arg = escape(arg);
    match arg.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\' || c == '\'') {
        true => format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")),
        false => arg,
    }
}

/// Escapes systemd's `%` specifiers.
fn escape(s: &str) -> String {
    s.replace('%', "%%")
}
