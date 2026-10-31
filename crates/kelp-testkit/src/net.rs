//! A real server on a loopback port, and clients that talk to it.
//!
//! Enabled by the `net` feature.

use std::path::PathBuf;
use std::sync::Arc;

use kelp_client::{Connection, ContentCache, Home, ServerEntry, Trust, Workspace};
use kelp_core::DepotPath;
use kelp_proto::{Auth, Reply, Request};
use kelp_server::{Config, Server, ShutdownHandle, Upstream};
use kelp_workspace::Config as WorkspaceConfig;

use crate::{ManualClock, Sandbox};

/// The administrator created by [`TestServer::start`].
pub const ADMIN: &str = "admin";

/// The password of every user created through this module.
pub const PASSWORD: &str = "correct horse battery staple";

/// A Kelp server with its own data directory and a [`ManualClock`].
///
/// The server stops when the value is dropped.
pub struct TestServer {
    addr: String,
    fingerprint: String,
    clock: Arc<ManualClock>,
    shutdown: ShutdownHandle,
    task: Option<tokio::task::JoinHandle<()>>,
    data: PathBuf,
    metrics: Option<std::net::SocketAddr>,
    _sandbox: Option<Sandbox>,
}

impl TestServer {
    /// Initialises a data directory and starts serving on a loopback port.
    ///
    /// # Panics
    ///
    /// Panics if the server cannot be started.
    pub async fn start() -> Self {
        Self::start_with(None).await
    }

    /// Like [`start`](Self::start), storing chunks as `storage` describes.
    ///
    /// # Panics
    ///
    /// Panics if the server cannot be started.
    pub async fn start_with(storage: Option<kelp_server::StorageConfig>) -> Self {
        Self::start_configured(|c| c.storage = storage).await
    }

    /// Like [`start`](Self::start), letting `configure` change the
    /// configuration first.
    ///
    /// # Panics
    ///
    /// Panics if the server cannot be started.
    pub async fn start_configured(configure: impl FnOnce(&mut Config)) -> Self {
        let sandbox = Sandbox::new();
        let clock = Arc::new(ManualClock::default());
        let data = sandbox.dir("data");
        Server::init(&data, ADMIN, PASSWORD, clock.as_ref()).expect("initialise server");
        let mut config = Config::new(&data, "127.0.0.1:0".parse().expect("loopback address"));
        configure(&mut config);
        Self::serve(sandbox, clock, config)
    }

    /// Starts a server on the existing data directory `data`, such as a
    /// restored backup, with this server's clock. The caller keeps `data`
    /// alive.
    ///
    /// # Panics
    ///
    /// Panics if the server cannot be started.
    pub fn start_on(&self, data: &std::path::Path) -> Self {
        let config = Config::new(data, "127.0.0.1:0".parse().expect("loopback address"));
        Self::serve(Sandbox::new(), self.clock.clone(), config)
    }

    /// Starts an edge of this server that replicates with a new edge account
    /// named `name`. The edge shares this server's clock.
    ///
    /// # Panics
    ///
    /// Panics if the account cannot be created or the edge cannot be started.
    pub async fn start_edge(&self, name: &str) -> Self {
        let token = self.edge_token(name).await;
        self.start_edge_with(token)
    }

    /// Starts an edge of this server that replicates with `token`.
    ///
    /// # Panics
    ///
    /// Panics if the edge cannot be started.
    pub fn start_edge_with(&self, token: String) -> Self {
        self.start_edge_via(vec![self.addr.clone()], token)
    }

    /// Starts an edge that replicates with `token` from the first of
    /// `addrs` that answers, such as members of a commit group.
    ///
    /// # Panics
    ///
    /// Panics if the edge cannot be started.
    pub fn start_edge_via(&self, addrs: Vec<String>, token: String) -> Self {
        let sandbox = Sandbox::new();
        let mut config = Config::new(
            sandbox.dir("data"),
            "127.0.0.1:0".parse().expect("loopback address"),
        );
        config.upstream = Some(Upstream {
            addrs,
            fingerprint: self.fingerprint.clone(),
            token,
        });
        Self::serve(sandbox, self.clock.clone(), config)
    }

    /// Creates the edge account `name` and returns a token for it.
    ///
    /// # Panics
    ///
    /// Panics if the account cannot be created.
    pub async fn edge_token(&self, name: &str) -> String {
        let admin = self.login(ADMIN).await;
        let create = Request::CreateUser {
            name: name.to_owned(),
            password: None,
            admin: false,
            replica: true,
        };
        admin.call(&create).await.expect("create edge account");
        let issue = Request::IssueToken {
            user: name.to_owned(),
            ttl_days: 30,
        };
        match admin.call(&issue).await.expect("issue edge token") {
            Reply::Token { token, .. } => token,
            other => panic!("unexpected token reply {other:?}"),
        }
    }

    fn serve(sandbox: Sandbox, clock: Arc<ManualClock>, config: Config) -> Self {
        let server = Self::bind(clock.clone(), config).expect("bind server");
        Self::run(Some(sandbox), clock, server)
    }

    pub(crate) fn bind(clock: Arc<ManualClock>, mut config: Config) -> kelp_server::Result<Server> {
        config.clock = clock;
        config.webhook_poll = std::time::Duration::from_millis(10);
        config.webhook_retry = std::time::Duration::from_secs(60);
        Server::bind(config)
    }

    pub(crate) fn run(sandbox: Option<Sandbox>, clock: Arc<ManualClock>, server: Server) -> Self {
        let addr = server.local_addr().expect("server address").to_string();
        let fingerprint = server.identity().fingerprint();
        let shutdown = server.shutdown_handle();
        let data = server.data_dir().to_owned();
        let metrics = server.metrics_addr();
        let task = tokio::spawn(server.run());
        Self {
            addr,
            fingerprint,
            clock,
            shutdown,
            task: Some(task),
            data,
            metrics,
            _sandbox: sandbox,
        }
    }

    /// Stops the server and waits until it has released its data directory.
    pub async fn stop(mut self) {
        self.shutdown.shutdown();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Returns the server address as `host:port`.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Returns the fingerprint of the server's certificate.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns the address of the monitoring endpoint, if configured.
    pub fn metrics_addr(&self) -> Option<std::net::SocketAddr> {
        self.metrics
    }

    /// Returns the server's data directory.
    pub fn data_dir(&self) -> std::path::PathBuf {
        self.data.clone()
    }

    /// Returns the server's clock.
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// Connects without credentials, pinning the server's certificate.
    ///
    /// # Panics
    ///
    /// Panics if the connection fails.
    pub async fn anonymous(&self) -> Connection {
        Connection::connect(
            &self.addr,
            Trust::Pinned(self.fingerprint.clone()),
            Auth::None,
        )
        .await
        .expect("connect to test server")
    }

    /// Connects and logs in as `user` with [`PASSWORD`].
    ///
    /// # Panics
    ///
    /// Panics if the connection or login fails.
    pub async fn login(&self, user: &str) -> Connection {
        let conn = self.anonymous().await;
        let request = Request::Login {
            user: user.to_owned(),
            password: PASSWORD.to_owned(),
            code: None,
        };
        match conn.call(&request).await.expect("log in") {
            Reply::Token { .. } => conn,
            other => panic!("unexpected login reply {other:?}"),
        }
    }

    /// Creates a regular user and returns a connection logged in as them.
    ///
    /// # Panics
    ///
    /// Panics if the user cannot be created.
    pub async fn add_user(&self, name: &str) -> Connection {
        let request = Request::CreateUser {
            name: name.to_owned(),
            password: Some(PASSWORD.to_owned()),
            admin: false,
            replica: false,
        };
        self.login(ADMIN)
            .await
            .call(&request)
            .await
            .expect("create user");
        self.login(name).await
    }

    /// Sends `request` as the administrator and returns the reply.
    ///
    /// # Panics
    ///
    /// Panics if the request fails.
    pub async fn admin(&self, request: Request) -> Reply {
        self.login(ADMIN)
            .await
            .call(&request)
            .await
            .expect("administrator request")
    }

    /// Creates a depot and its `main` stream.
    ///
    /// # Panics
    ///
    /// Panics if the depot cannot be created.
    pub async fn create_depot(&self, name: &str) {
        let request = Request::CreateDepot {
            name: name.to_owned(),
        };
        self.login(ADMIN)
            .await
            .call(&request)
            .await
            .expect("create depot");
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.shutdown();
    }
}

/// A user's machine: a sandbox with a Kelp home directory and workspaces.
pub struct TestClient {
    sandbox: Sandbox,
    home: Home,
}

impl TestClient {
    /// Creates an empty client machine.
    pub fn new() -> Self {
        let sandbox = Sandbox::new();
        let home = Home::new(sandbox.dir("home"));
        Self { sandbox, home }
    }

    /// Returns the machine's sandbox, in which workspaces live.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// Returns the Kelp home directory.
    pub fn home(&self) -> &Home {
        &self.home
    }

    /// Returns the machine's content cache.
    pub fn cache(&self) -> ContentCache {
        self.home.cache()
    }

    /// Returns the absolute path of the workspace named `name`.
    pub fn workspace_root(&self, name: &str) -> PathBuf {
        self.sandbox.path(name)
    }

    /// Creates a workspace named `name` bound to `stream` on `server`.
    ///
    /// # Panics
    ///
    /// Panics if the workspace cannot be created.
    pub fn create_workspace(
        &self,
        server: &TestServer,
        name: &str,
        stream: &DepotPath,
    ) -> Workspace {
        let config = WorkspaceConfig::new(name, server.addr(), stream);
        Workspace::create(&self.sandbox.dir(name), config).expect("create workspace")
    }
}

impl TestClient {
    /// Logs in to `server` as `user` and stores the credentials in this
    /// machine's Kelp home, as `kelp login` would.
    ///
    /// # Panics
    ///
    /// Panics if the login fails.
    pub async fn login(&self, server: &TestServer, user: &str) {
        let conn = server.anonymous().await;
        let request = Request::Login {
            user: user.to_owned(),
            password: PASSWORD.to_owned(),
            code: None,
        };
        let Reply::Token { token, .. } = conn.call(&request).await.expect("log in") else {
            panic!("unexpected login reply");
        };
        let mut creds = self.home.credentials().expect("read credentials");
        let entry = ServerEntry {
            fingerprint: server.fingerprint().to_owned(),
            user: Some(user.to_owned()),
            token: Some(token),
        };
        creds.servers.insert(server.addr().to_owned(), entry);
        self.home
            .save_credentials(&creds)
            .expect("save credentials");
    }

    /// Starts kelpd for this machine and waits until it accepts requests.
    ///
    /// # Panics
    ///
    /// Panics if kelpd does not come up.
    pub async fn start_daemon(&self) {
        let config = kelp_daemon::Config {
            home: self.home.dir().to_owned(),
            idle_timeout: None,
        };
        tokio::spawn(kelp_daemon::serve(config));
        for _ in 0..200 {
            if kelp_client::ipc::connect(&self.home).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("kelpd did not start");
    }
}

impl Default for TestClient {
    fn default() -> Self {
        Self::new()
    }
}
