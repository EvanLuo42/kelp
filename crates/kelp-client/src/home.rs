use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use kelp_proto::Auth;

use crate::{Connection, ContentCache, Error, Result, Trust};

/// The per-user Kelp directory holding credentials and the content cache.
///
/// The CLI locates it from the `KELP_HOME` environment variable or the
/// platform configuration directory; tests point it into a sandbox.
#[derive(Clone, Debug)]
pub struct Home {
    dir: PathBuf,
}

/// What the client knows about one server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// The pinned certificate fingerprint.
    pub fingerprint: String,
    pub user: Option<String>,
    pub token: Option<String>,
}

/// Known servers, keyed by address.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerEntry>,
}

impl Home {
    /// Creates a handle to the Kelp home directory at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Returns the home named by `KELP_HOME`, or `kelp` in the platform
    /// configuration directory.
    pub fn default_location() -> Option<Self> {
        if let Some(dir) = std::env::var_os("KELP_HOME") {
            return Some(Self::new(dir));
        }
        dirs::config_dir().map(|d| Self::new(d.join("kelp")))
    }

    /// Returns the directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the machine-wide content cache, shared by all workspaces.
    pub fn cache(&self) -> ContentCache {
        ContentCache::new(self.dir.join("cache"))
    }

    /// Reads the stored credentials, or returns none if no file exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn credentials(&self) -> Result<Credentials> {
        match fs::read_to_string(self.credentials_path()) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Invalid(format!("credentials file: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes the credentials, readable only by the current user where the
    /// platform supports it.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save_credentials(&self, credentials: &Credentials) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.credentials_path();
        let tmp = path.with_extension("tmp");
        fs::write(
            &tmp,
            toml::to_string_pretty(credentials).expect("credentials serialize"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, path)?;
        Ok(())
    }

    /// Connects to `server` with the stored token and pinned certificate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotLoggedIn`] if there are no usable credentials for
    /// the server, or an error if connecting fails.
    pub async fn connect(&self, server: &str) -> Result<Connection> {
        self.connect_inner(server).await
    }

    /// Copies the login for the server at `from` to its new address `to`,
    /// after the server moved, unless there is already a login for `to`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotLoggedIn`] if there is no login for `from`, or an
    /// error if the credentials cannot be read or written.
    pub fn move_login(&self, from: &str, to: &str) -> Result<()> {
        let mut creds = self.credentials()?;
        let entry = creds
            .servers
            .get(from)
            .cloned()
            .ok_or_else(|| Error::NotLoggedIn(from.to_owned()))?;
        creds.servers.entry(to.to_owned()).or_insert(entry);
        self.save_credentials(&creds)
    }

    async fn connect_inner(&self, server: &str) -> Result<Connection> {
        let creds = self.credentials()?;
        let entry = creds
            .servers
            .get(server)
            .ok_or_else(|| Error::NotLoggedIn(server.to_owned()))?;
        let auth = entry.token.clone().map_or(Auth::None, Auth::Bearer);
        let conn =
            Connection::connect(server, Trust::Pinned(entry.fingerprint.clone()), auth).await?;
        if conn.welcome().user.is_none() {
            return Err(Error::NotLoggedIn(server.to_owned()));
        }
        Ok(conn)
    }

    /// Returns the path of the kelpd socket on Unix, or a name derived from
    /// it for the named pipe on Windows.
    pub fn daemon_socket(&self) -> PathBuf {
        self.dir.join("kelpd.sock")
    }

    fn credentials_path(&self) -> PathBuf {
        self.dir.join("credentials.toml")
    }
}
