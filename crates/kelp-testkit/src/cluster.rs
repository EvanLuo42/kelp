//! A commit group of servers on loopback ports that share an S3 bucket.
//!
//! Enabled by the `net` and `s3` features together.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kelp_proto::{Reply, Request, ServerRole};
use kelp_server::{ClusterConfig, Config, Server, StorageConfig, StorageKind};

use crate::net::{ADMIN, PASSWORD, TestServer};
use crate::{ManualClock, Sandbox};

const SECRET: &str = "test cluster secret, shared by every member";

/// How long [`TestCluster::leader`] waits for a leader.
const ELECTION: Duration = Duration::from_secs(30);

/// A commit group whose members can be stopped and started again.
///
/// Member 0 is initialized with the [`ADMIN`] account and founds the group;
/// the others start empty and share its TLS identity.
pub struct TestCluster {
    sandbox: Sandbox,
    clock: Arc<ManualClock>,
    storage: StorageConfig,
    members: BTreeMap<u64, String>,
    running: Vec<Option<TestServer>>,
}

impl TestCluster {
    /// Prepares the data directories of `size` members without starting
    /// any, or returns `None` if no S3 test server is configured.
    ///
    /// # Panics
    ///
    /// Panics if a directory cannot be prepared.
    pub fn prepare(size: usize) -> Option<Self> {
        let s3 = crate::s3::config()?;
        let sandbox = Sandbox::new();
        let clock = Arc::new(ManualClock::default());
        let founder = sandbox.dir("member-0");
        Server::init(&founder, ADMIN, PASSWORD, clock.as_ref()).expect("initialise founder");
        for i in 1..size {
            let tls = sandbox.dir(&format!("member-{i}/tls"));
            for file in ["cert.der", "key.der"] {
                std::fs::copy(founder.join("tls").join(file), tls.join(file))
                    .expect("copy identity");
            }
        }
        let members = (0..size)
            .map(|i| (i as u64 + 1, format!("127.0.0.1:{}", free_port())))
            .collect();
        Some(Self {
            sandbox,
            clock,
            storage: StorageConfig {
                kind: StorageKind::S3,
                s3: Some(s3),
            },
            members,
            running: (0..size).map(|_| None).collect(),
        })
    }

    /// Prepares and starts a group of `size` members, or returns `None` if
    /// no S3 test server is configured.
    ///
    /// # Panics
    ///
    /// Panics if a member cannot be started.
    pub async fn start(size: usize) -> Option<Self> {
        let mut cluster = Self::prepare(size)?;
        for i in 0..size {
            cluster.start_member(i).await;
        }
        cluster.wait_for_voters().await;
        Some(cluster)
    }

    /// Waits until every configured member votes.
    ///
    /// # Panics
    ///
    /// Panics if the group has not grown within thirty seconds.
    pub async fn wait_for_voters(&self) {
        let wanted: Vec<u64> = self.members.keys().copied().collect();
        let deadline = tokio::time::Instant::now() + ELECTION;
        while tokio::time::Instant::now() < deadline {
            let leader = self.leader().await;
            let admin = self.member(leader).login(ADMIN).await;
            if let Ok(Reply::Cluster(info)) = admin.call(&Request::ClusterStatus).await
                && info.voters == wanted
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the group did not grow to {wanted:?}");
    }

    /// Starts member `i`, waiting until the previous run has released its
    /// data directory.
    ///
    /// # Panics
    ///
    /// Panics if the member is running or cannot be started.
    pub async fn start_member(&mut self, i: usize) {
        let config = self.config(i, true);
        self.running[i] = Some(self.bind(config).await);
    }

    /// Tries once to start member `i`, returning why it cannot start.
    ///
    /// # Errors
    ///
    /// Returns the server's error if it refuses its data directory.
    pub fn try_start_member(&mut self, i: usize) -> Result<(), kelp_server::Error> {
        let server = TestServer::bind(self.clock.clone(), self.config(i, true))?;
        self.running[i] = Some(TestServer::run(None, self.clock.clone(), server));
        Ok(())
    }

    /// Returns the data directory of member `i`.
    pub fn data_dir(&self, i: usize) -> std::path::PathBuf {
        self.sandbox.path(&format!("member-{i}"))
    }

    /// Starts member `i` as an ordinary server outside the group, with the
    /// group's storage.
    ///
    /// # Panics
    ///
    /// Panics if the member is running or cannot be started.
    pub async fn start_alone(&mut self, i: usize) {
        let config = self.config(i, false);
        self.running[i] = Some(self.bind(config).await);
    }

    async fn bind(&self, config: Config) -> TestServer {
        let mut tries = 0;
        loop {
            match TestServer::bind(self.clock.clone(), config.clone()) {
                Ok(server) => return TestServer::run(None, self.clock.clone(), server),
                Err(_) if tries < 100 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("cannot start member: {e}"),
            }
        }
    }

    fn config(&self, i: usize, member: bool) -> Config {
        let id = i as u64 + 1;
        let addr = self.members[&id].parse().expect("member address");
        let mut config = Config::new(self.sandbox.dir(&format!("member-{i}")), addr);
        config.storage = Some(self.storage.clone());
        if member {
            config.cluster = Some(ClusterConfig {
                id,
                secret: SECRET.into(),
                members: self.members.clone(),
            });
        }
        config
    }

    /// Stops member `i` and waits until it has shut down.
    ///
    /// # Panics
    ///
    /// Panics if the member is not running.
    pub async fn stop_member(&mut self, i: usize) {
        self.running[i]
            .take()
            .expect("member is running")
            .stop()
            .await;
    }

    /// Deletes everything in the data directory of member `i`, which must be
    /// stopped, except its TLS identity, as when a disk is replaced.
    ///
    /// # Panics
    ///
    /// Panics if the member is running or the directory cannot be cleared.
    pub fn wipe_member(&self, i: usize) {
        assert!(self.running[i].is_none(), "stop member {i} first");
        let dir = self.sandbox.path(&format!("member-{i}"));
        for entry in std::fs::read_dir(&dir).expect("read member directory") {
            let path = entry.expect("directory entry").path();
            if path.file_name().is_some_and(|n| n == "tls") {
                continue;
            }
            match path.is_dir() {
                true => std::fs::remove_dir_all(&path),
                false => std::fs::remove_file(&path),
            }
            .expect("delete member data");
        }
    }

    /// Returns member `i`.
    ///
    /// # Panics
    ///
    /// Panics if the member is not running.
    pub fn member(&self, i: usize) -> &TestServer {
        self.running[i].as_ref().expect("member is running")
    }

    /// Waits until a running member leads the group and returns its index.
    ///
    /// # Panics
    ///
    /// Panics if no member leads within thirty seconds.
    pub async fn leader(&self) -> usize {
        let deadline = tokio::time::Instant::now() + ELECTION;
        while tokio::time::Instant::now() < deadline {
            for (i, member) in self.running.iter().enumerate() {
                let Some(member) = member else { continue };
                if member.anonymous().await.welcome().role == ServerRole::Commit {
                    return i;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no member leads the group");
    }

    /// Returns the cluster secret, for tests of its checks.
    pub fn secret(&self) -> &str {
        SECRET
    }
}

fn free_port() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a free port");
    socket.local_addr().expect("socket address").port()
}
