//! Helpers shared by commands: locating state, connecting and parsing.

use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, bail};
use kelp_client::{Connection, Home, ServerEntry, Trust, Workspace};
use kelp_core::LocalClId;
use kelp_proto::rpc::FileAction;
use kelp_proto::{Auth, ErrorKind, Reply, Request};
use kelp_workspace::OpenAction;

/// Returns the Kelp home directory from `KELP_HOME` or the platform default.
pub fn home() -> anyhow::Result<Home> {
    Home::default_location().context("cannot find a configuration directory; set KELP_HOME")
}

/// Returns the server of the workspace around `cwd`, `server`, or the only
/// server the user has logged in to.
pub fn pick_server(home: &Home, cwd: &Path, server: Option<String>) -> anyhow::Result<String> {
    if let Some(s) = server {
        return Ok(s);
    }
    if let Ok(ws) = Workspace::discover(cwd) {
        return Ok(ws.config().server.clone());
    }
    let creds = home.credentials()?;
    let mut servers = creds.servers.keys();
    match (servers.next(), servers.next()) {
        (Some(only), None) => Ok(only.clone()),
        (None, _) => bail!("not logged in to any server; run `kelp login <server>`"),
        _ => bail!("logged in to several servers; pass --server"),
    }
}

async fn password_login(
    conn: &Connection,
    server: &str,
    user: Option<String>,
    password_stdin: bool,
) -> anyhow::Result<(String, String)> {
    let user = match user {
        Some(u) => u,
        None => os_user().context("cannot determine the operating system user; pass --user")?,
    };
    let password = read_password(
        password_stdin,
        &format!("Password for {user}@{server}: "),
        false,
    )?;
    let mut request = Request::Login {
        user,
        password,
        code: None,
    };
    loop {
        match conn.call(&request).await {
            Ok(Reply::Token { token, user, .. }) => return Ok((token, user)),
            Ok(_) => return Err(unexpected()),
            Err(kelp_client::Error::Remote(e)) if e.kind == ErrorKind::CodeRequired => {
                let Request::Login { code, .. } = &mut request else {
                    unreachable!("the request is a login");
                };
                if code.is_some() {
                    bail!("{}", e.message);
                }
                *code = Some(rpassword::prompt_password("Authenticator code: ")?);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

async fn sso_login(conn: &Connection, link: bool) -> anyhow::Result<(String, String)> {
    let Reply::DeviceCode(device) = conn.call(&Request::OidcStart).await? else {
        return Err(unexpected());
    };
    match &device.verification_uri_complete {
        Some(uri) => eprintln!("Open {uri} to log in (code {}).", device.user_code),
        None => eprintln!(
            "Open {} and enter the code {}.",
            device.verification_uri, device.user_code
        ),
    }
    let interval = std::time::Duration::from_secs(device.interval.max(1).into());
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in.into());
    let poll = match link {
        true => Request::OidcLink {
            device_code: device.device_code,
        },
        false => Request::OidcPoll {
            device_code: device.device_code,
        },
    };
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        match conn.call(&poll).await? {
            Reply::Token { token, user, .. } => return Ok((token, user)),
            Reply::AuthPending => {}
            _ => return Err(unexpected()),
        }
    }
    bail!("the login was not approved in time")
}

/// Tells the user on a terminal what a step with no progress bar is doing, so
/// a long wait does not look like a hang.
pub fn notice(message: &str) {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        eprintln!("{message}");
    }
}

/// The error for a reply of the wrong kind, which in practice means the
/// client and server are different versions of Kelp.
pub fn unexpected() -> anyhow::Error {
    anyhow::anyhow!(
        "the server sent an unexpected reply; are the client and server the same version of Kelp?"
    )
}

/// Connects to `server` with the stored token and pinned fingerprint.
pub async fn connect(home: &Home, server: &str) -> anyhow::Result<Connection> {
    Ok(home.connect(server).await?)
}

/// How [`login`] authenticates.
pub enum LoginMethod {
    /// A password, for `user` or the operating system user, read from
    /// standard input if `stdin` is set.
    Password { user: Option<String>, stdin: bool },
    /// The server's single sign-on provider.
    Sso,
    /// An access token read from standard input.
    Token,
}

/// Logs in to `server` and stores the token.
pub async fn login(home: &Home, server: &str, method: LoginMethod) -> anyhow::Result<()> {
    let mut creds = home.credentials()?;
    let known = creds.servers.get(server).map(|e| e.fingerprint.clone());
    let trust = known.clone().map_or(Trust::FirstUse, Trust::Pinned);
    let token = match method {
        LoginMethod::Token => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            Some(line.trim().to_owned())
        }
        _ => None,
    };
    let auth = token.clone().map_or(Auth::None, Auth::Bearer);
    let conn = Connection::connect(server, trust, auth).await?;
    if known.is_none() {
        eprintln!(
            "Trusting {server} with certificate fingerprint\n  {}",
            conn.fingerprint()
        );
    }
    let (token, user) = match (method, token) {
        (LoginMethod::Token, Some(token)) => match conn.welcome().user.clone() {
            Some(user) => (token, user),
            None => bail!("the server does not accept the token; it may have expired"),
        },
        (LoginMethod::Sso, _) => sso_login(&conn, false).await?,
        (LoginMethod::Password { user, stdin }, _) => {
            password_login(&conn, server, user, stdin).await?
        }
        (LoginMethod::Token, None) => unreachable!("the token was read above"),
    };
    let entry = ServerEntry {
        fingerprint: conn.fingerprint().to_owned(),
        user: Some(user.clone()),
        token: Some(token),
    };
    creds.servers.insert(server.to_owned(), entry);
    home.save_credentials(&creds)?;
    println!("Logged in to {server} as {user}.");
    Ok(())
}

/// Links the single sign-on account approved in a browser to the user
/// logged in to `server`, and stores the new token.
pub async fn link_sso(home: &Home, server: &str) -> anyhow::Result<()> {
    let conn = home.connect(server).await?;
    let (token, user) = sso_login(&conn, true).await?;
    let mut creds = home.credentials()?;
    let entry = ServerEntry {
        fingerprint: conn.fingerprint().to_owned(),
        user: Some(user.clone()),
        token: Some(token),
    };
    creds.servers.insert(server.to_owned(), entry);
    home.save_credentials(&creds)?;
    println!(
        "Linked {user} to single sign-on; log in with `kelp login {server} --sso` from now on."
    );
    Ok(())
}

pub fn os_user() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty())
}

pub fn read_password(from_stdin: bool, prompt: &str, confirm: bool) -> anyhow::Result<String> {
    if from_stdin {
        return read_line_password(std::io::stdin().lock());
    }
    let password = rpassword::prompt_password(prompt)?;
    if confirm && rpassword::prompt_password("Repeat: ")? != password {
        bail!("passwords do not match");
    }
    Ok(password)
}

/// Like [`read_password`], for a password being set: it must not be empty.
pub fn read_new_password(from_stdin: bool, prompt: &str, confirm: bool) -> anyhow::Result<String> {
    let password = read_password(from_stdin, prompt, confirm)?;
    if password.is_empty() {
        bail!("the password must not be empty");
    }
    Ok(password)
}

/// Reads a password from the first line of `input`. Running out of input
/// before a line is an error rather than an empty password, so that a
/// missing pipe cannot set or try one.
fn read_line_password(mut input: impl BufRead) -> anyhow::Result<String> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!("expected a password on standard input");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// Parses `default`, `L3` or `3` as a pending changelist id.
pub fn parse_cl(s: &str) -> anyhow::Result<LocalClId> {
    if s.eq_ignore_ascii_case("default") {
        return Ok(LocalClId::DEFAULT);
    }
    let digits = s.strip_prefix(['L', 'l']).unwrap_or(s);
    Ok(LocalClId(
        digits
            .parse()
            .with_context(|| format!("invalid changelist {s:?}"))?,
    ))
}

pub fn parse_cl_or_default(s: Option<&str>) -> anyhow::Result<LocalClId> {
    Ok(s.map(parse_cl).transpose()?.unwrap_or(LocalClId::DEFAULT))
}

pub fn action_name(action: OpenAction) -> &'static str {
    match action {
        OpenAction::Add => "add",
        OpenAction::Edit => "edit",
        OpenAction::Delete => "delete",
        OpenAction::MoveFrom => "move/from",
        OpenAction::MoveTo => "move/to",
    }
}

pub fn file_action(action: FileAction) -> &'static str {
    match action {
        FileAction::Add => "add",
        FileAction::Edit => "edit",
        FileAction::Delete => "delete",
    }
}

/// Returns `<dir name>-<8 hex digits>` for a new workspace in `dir`.
pub fn workspace_name(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map_or("ws".into(), |n| n.to_string_lossy().into_owned());
    let base: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let seed = format!("{}{:?}", dir.display(), std::time::SystemTime::now());
    format!(
        "{}-{}",
        if base.is_empty() { "ws" } else { &base },
        &blake3::hash(seed.as_bytes()).to_hex()[..8]
    )
}

#[cfg(test)]
mod tests {
    use super::read_line_password;

    #[test]
    fn a_password_on_stdin_is_one_line() {
        assert_eq!(
            read_line_password(&b"s3cret\r\nmore"[..]).unwrap(),
            "s3cret"
        );
        assert_eq!(read_line_password(&b"s3cret"[..]).unwrap(), "s3cret");
        assert_eq!(read_line_password(&b"\n"[..]).unwrap(), "");
    }

    #[test]
    fn no_input_is_not_an_empty_password() {
        let err = read_line_password(&b""[..]).unwrap_err();
        assert!(err.to_string().contains("expected a password"), "{err}");
    }
}
