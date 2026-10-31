//! The commit group role: several servers that agree on one metadata log
//! with Raft and keep chunks in one shared object store.
//!
//! Every member applies the same [`Proposal`]s in the same order, so their
//! metadata databases stay equal. The leader handles writes; the other
//! members serve reads from their own database and send writes on to the
//! leader under the requesting user's token, the way an edge does, then
//! wait until they have applied the result. Any member can therefore serve
//! any client, and when the leader fails another takes over without losing
//! a committed change.

mod network;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kelp_core::time::Clock;
use kelp_meta::MetaDb;
use kelp_meta::db::{Outcome, Proposal};
use kelp_proto::rpc::RaftRpc;
use kelp_proto::{ErrorKind, ErrorReply};
use openraft::error::{CheckIsLeaderError, ClientWriteError, RaftError};
use openraft::{BasicNode, LogId, Raft};
use serde::{Deserialize, Serialize};

use network::Network;

use store::{LogStore, StateMachine};

/// The identifier of a member.
pub type NodeId = u64;

openraft::declare_raft_types!(
    /// The types the commit group's Raft instance works with.
    pub TypeConfig:
        D = Proposal,
        R = Response,
        Node = BasicNode,
        SnapshotData = tokio::fs::File,
);

/// What applying a log entry produced on a member.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response(Result<Outcome, Rejection>);

/// Why a proposal was not applied; every member rejects it the same way.
#[derive(Debug, Serialize, Deserialize)]
pub struct Rejection {
    kind: ErrorKind,
    message: String,
}

impl From<ErrorReply> for Rejection {
    fn from(e: ErrorReply) -> Self {
        Self {
            kind: e.kind,
            message: e.message,
        }
    }
}

/// How long a member waits to apply what the leader has committed.
const CATCH_UP: Duration = Duration::from_secs(10);

/// The settings of a commit group member, from `<data>/cluster.toml`:
///
/// ```toml
/// id = 1
/// secret = "a long random string shared by all members"
///
/// [members]
/// 1 = "kelp-1.studio:7474"
/// 2 = "kelp-2.studio:7474"
/// 3 = "kelp-3.studio:7474"
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// This member's identifier; a key of `members`.
    pub id: NodeId,
    /// Authenticates members to each other.
    pub secret: String,
    /// Every member's identifier and address.
    pub members: BTreeMap<NodeId, String>,
}

impl ClusterConfig {
    /// Reads `<data>/cluster.toml`, or returns `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Config`] if the file cannot be read or is
    /// invalid.
    pub fn load(data_dir: &Path) -> crate::Result<Option<Self>> {
        let path = data_dir.join("cluster.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(crate::Error::Config(format!("{}: {e}", path.display()))),
        };
        let config: Self = toml::from_str(&text)
            .map_err(|e| crate::Error::Config(format!("{}: {e}", path.display())))?;
        config.validate()?;
        Ok(Some(config))
    }

    fn validate(&self) -> crate::Result<()> {
        if !self.members.contains_key(&self.id) {
            return Err(crate::Error::Config(format!(
                "member {} is not in the member list",
                self.id
            )));
        }
        if self.secret.len() < 16 {
            return Err(crate::Error::Config(
                "the cluster secret must be at least 16 characters".into(),
            ));
        }
        Ok(())
    }

    /// The member that initializes a new group.
    fn founder(&self) -> NodeId {
        *self.members.keys().next().expect("validated")
    }
}

/// Brings the Raft log in `data_dir` to this build's database format, once
/// the metadata database has been migrated; see [`LogStore::upgrade`].
pub(crate) fn upgrade_log(data_dir: &Path, db: &Arc<MetaDb>) -> crate::Result<()> {
    let log = LogStore::open(&data_dir.join("raft.redb"))
        .map_err(|e| crate::Error::Config(format!("raft log: {e}")))?;
    let machine = StateMachine::new(db.clone(), data_dir.join("raft-snapshot"))?;
    log.upgrade(&machine)
}

/// A running member of a commit group.
pub(crate) struct Cluster {
    id: NodeId,
    raft: Raft<TypeConfig>,
    network: Network,
    secret: String,
    members: BTreeMap<NodeId, String>,
    clock: Arc<dyn Clock>,
    /// When this member last became the leader, in milliseconds.
    leader_since: Arc<AtomicU64>,
}

impl Cluster {
    /// Starts this member's Raft instance on the databases in `data_dir`.
    ///
    /// The member with the lowest identifier founds a new group, carrying
    /// over the history its database already has. Every other member must
    /// start with an empty database and receives that history from the
    /// leader.
    pub(crate) async fn start(
        config: ClusterConfig,
        data_dir: &Path,
        db: Arc<MetaDb>,
        fingerprint: String,
        clock: Arc<dyn Clock>,
    ) -> crate::Result<Self> {
        let log = LogStore::open(&data_dir.join("raft.redb"))
            .map_err(|e| crate::Error::Config(format!("raft log: {e}")))?;
        let machine = StateMachine::new(db.clone(), data_dir.join("raft-snapshot"))?;
        let fresh = log
            .is_empty()
            .map_err(|e| crate::Error::Config(e.to_string()))?
            && machine
                .applied()
                .map_err(|e| crate::Error::Config(e.to_string()))?
                .is_none();
        let history = db.read()?.last_index()?.0;
        let founder = config.founder() == config.id;
        if fresh && history > 0 && !founder {
            return Err(crate::Error::Config(format!(
                "member {} has data of its own; start it with an empty data directory and copy only its TLS identity, or make it the member with the lowest id",
                config.id
            )));
        }
        let raft_config = openraft::Config {
            cluster_name: "kelp".into(),
            election_timeout_min: 300,
            election_timeout_max: 600,
            heartbeat_interval: 100,
            max_payload_entries: 64,
            install_snapshot_timeout: 30_000,
            snapshot_max_chunk_size: 1 << 20,
            ..Default::default()
        }
        .validate()
        .map_err(|e| crate::Error::Config(e.to_string()))?;
        let network = Network {
            secret: config.secret.clone(),
            fingerprint,
        };
        let raft = Raft::new(
            config.id,
            Arc::new(raft_config),
            network.clone(),
            log,
            machine,
        )
        .await
        .map_err(|e| crate::Error::Config(format!("raft: {e}")))?;
        let cluster = Self {
            id: config.id,
            raft,
            network,
            secret: config.secret,
            members: config.members,
            clock,
            leader_since: Arc::new(AtomicU64::new(u64::MAX)),
        };
        cluster.watch_leadership();
        if fresh {
            // A member without history may hold a stale membership from a
            // snapshot, or have forgotten a vote; it must not lead or vote
            // until it has caught up with the group.
            cluster.raft.runtime_config().elect(false);
            tokio::spawn(join(
                cluster.raft.clone(),
                cluster.network.clone(),
                cluster.id,
                cluster.members.clone(),
                founder,
            ));
        }
        tokio::spawn(maintain_membership(
            cluster.raft.clone(),
            cluster.id,
            cluster.members.clone(),
        ));
        Ok(cluster)
    }

    fn watch_leadership(&self) {
        let mut metrics = self.raft.metrics();
        let since = self.leader_since.clone();
        let clock = self.clock.clone();
        let id = self.id;
        tokio::spawn(async move {
            let mut leading = false;
            loop {
                let now_leading = metrics.borrow().current_leader == Some(id);
                if now_leading && !leading {
                    since.store(clock.now(), Ordering::SeqCst);
                }
                if !now_leading {
                    since.store(u64::MAX, Ordering::SeqCst);
                }
                leading = now_leading;
                if metrics.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// Returns whether this member currently leads the group.
    pub(crate) fn is_leader(&self) -> bool {
        self.raft.metrics().borrow().current_leader == Some(self.id)
    }

    /// Returns when this member became the leader, in milliseconds, or
    /// `None` if it does not lead.
    pub(crate) fn leader_since(&self) -> Option<u64> {
        match self.leader_since.load(Ordering::SeqCst) {
            u64::MAX => None,
            at => Some(at),
        }
    }

    /// Returns the address of the current leader, waiting a while for an
    /// election to finish.
    pub(crate) async fn leader_addr(&self) -> Result<String, ErrorReply> {
        let leader = self
            .raft
            .wait(Some(CATCH_UP))
            .metrics(|m| m.current_leader.is_some(), "find the leader")
            .await
            .ok()
            .and_then(|m| m.current_leader);
        leader
            .and_then(|id| self.members.get(&id).cloned())
            .ok_or_else(|| {
                ErrorReply::new(
                    ErrorKind::Unavailable,
                    "the commit group has no leader at the moment; try again",
                )
            })
    }

    /// Commits `proposal` through the group and returns what applying it
    /// produced. Only the leader proposes.
    pub(crate) async fn propose(&self, proposal: Proposal) -> Result<Outcome, ErrorReply> {
        let proposal_copy = proposal.clone();
        match self.raft.client_write(proposal).await {
            Ok(response) => response
                .data
                .0
                .map_err(|r| ErrorReply::new(r.kind, r.message)),
            // openraft answers so before logging the entry, or after a
            // conflicting leader removed it, so it never commits; the new
            // leader can commit it instead.
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                self.propose_through_leader(forward.leader_id, proposal_copy)
                    .await
            }
            Err(e) => Err(ErrorReply::new(ErrorKind::Unavailable, e.to_string())),
        }
    }

    /// Has the leader commit `proposal` and waits until this member has
    /// applied it.
    async fn propose_through_leader(
        &self,
        leader: Option<NodeId>,
        proposal: Proposal,
    ) -> Result<Outcome, ErrorReply> {
        let not_leader = || {
            ErrorReply::new(
                ErrorKind::NotLeader,
                "this member stopped leading the commit group; try again",
            )
        };
        let leader = match leader.filter(|l| *l != self.id) {
            Some(leader) => leader,
            None => {
                self.leader_addr().await?;
                let current = self.raft.metrics().borrow().current_leader;
                current.filter(|l| *l != self.id).ok_or_else(not_leader)?
            }
        };
        let addr = self.members.get(&leader).cloned().ok_or_else(not_leader)?;
        let mut peer = self.network.peer(leader, addr);
        let outcome = peer
            .call::<_, Outcome, Rejection>(RaftRpc::Propose, &proposal)
            .await
            .map_err(|e| match e {
                network::CallError::Remote(r) => ErrorReply::new(r.kind, r.message),
                network::CallError::Unreachable(e) => {
                    ErrorReply::new(ErrorKind::NotLeader, e.to_string())
                }
                network::CallError::Network(e) => {
                    ErrorReply::new(ErrorKind::NotLeader, e.to_string())
                }
            })?;
        self.catch_up().await?;
        Ok(outcome)
    }

    /// Waits until this member has applied everything the group had
    /// committed when this was called.
    pub(crate) async fn catch_up(&self) -> Result<(), ErrorReply> {
        let leader = self.raft.metrics().borrow().current_leader;
        let read = match leader {
            Some(leader) if leader == self.id => self.read_index().await?,
            Some(leader) => {
                let addr = self.leader_addr().await?;
                let mut peer = self.network.peer(leader, addr);
                peer.call::<_, Option<LogId<NodeId>>, ReadIndexError>(RaftRpc::ReadIndex, &())
                    .await
                    .map_err(|e| unavailable(e.to_string()))?
            }
            None => {
                return Err(unavailable(
                    "the commit group has no leader at the moment; try again",
                ));
            }
        };
        let Some(read) = read else {
            return Ok(());
        };
        self.raft
            .wait(Some(CATCH_UP))
            .applied_index_at_least(Some(read.index), "catch up")
            .await
            .map(|_| ())
            .map_err(|e| unavailable(format!("this member is behind the commit group: {e}")))
    }

    async fn read_index(&self) -> Result<Option<LogId<NodeId>>, ErrorReply> {
        self.raft
            .ensure_linearizable()
            .await
            .map_err(|e| unavailable(e.to_string()))
    }

    /// Answers a message from another member.
    pub(crate) async fn serve(
        &self,
        secret: &str,
        format: u64,
        rpc: RaftRpc,
        payload: &[u8],
    ) -> Result<Vec<u8>, ErrorReply> {
        // blake3 hashes compare in constant time.
        if blake3::hash(secret.as_bytes()) != blake3::hash(self.secret.as_bytes()) {
            return Err(ErrorReply::new(
                ErrorKind::Forbidden,
                "not a member of this commit group",
            ));
        }
        // Members exchange log entries and snapshots in the database's own
        // encoding, which a member with another format would misread.
        if format != kelp_meta::db::FORMAT_VERSION {
            let message = format!(
                "a member has database format {format} and this one {}; \
                 run the same Kelp version on every member",
                kelp_meta::db::FORMAT_VERSION
            );
            tracing::error!("{message}");
            return Err(ErrorReply::new(ErrorKind::Invalid, message));
        }
        let invalid = |e: postcard::Error| ErrorReply::new(ErrorKind::Invalid, e.to_string());
        Ok(match rpc {
            RaftRpc::AppendEntries => {
                let request = postcard::from_bytes(payload).map_err(invalid)?;
                encode(&self.raft.append_entries(request).await)
            }
            RaftRpc::Vote => {
                let request = postcard::from_bytes(payload).map_err(invalid)?;
                encode(&self.raft.vote(request).await)
            }
            RaftRpc::InstallSnapshot => {
                let request = postcard::from_bytes(payload).map_err(invalid)?;
                encode(&self.raft.install_snapshot(request).await)
            }
            RaftRpc::Propose => {
                let proposal: Proposal = postcard::from_bytes(payload).map_err(invalid)?;
                let result: Result<Outcome, Rejection> =
                    match self.raft.client_write(proposal).await {
                        Ok(response) => response.data.0,
                        Err(e) => Err(Rejection {
                            kind: ErrorKind::NotLeader,
                            message: e.to_string(),
                        }),
                    };
                encode(&result)
            }
            RaftRpc::Probe => {
                let joined: Result<bool, ReadIndexError> = self
                    .raft
                    .is_initialized()
                    .await
                    .map_err(|e| ReadIndexError(e.to_string()));
                encode(&joined)
            }
            RaftRpc::ReadIndex => {
                let result: Result<Option<LogId<NodeId>>, ReadIndexError> =
                    self.raft.ensure_linearizable().await.map_err(
                        |e: RaftError<NodeId, CheckIsLeaderError<NodeId, BasicNode>>| {
                            ReadIndexError(e.to_string())
                        },
                    );
                encode(&result)
            }
        })
    }

    /// Describes the group as this member sees it.
    pub(crate) fn status(&self) -> kelp_proto::rpc::ClusterInfo {
        let m = self.raft.metrics();
        let m = m.borrow();
        kelp_proto::rpc::ClusterInfo {
            member: self.id,
            leader: m.current_leader,
            voters: m.membership_config.membership().voter_ids().collect(),
            applied: m.last_applied.map(|l| l.index),
        }
    }

    /// Stops the Raft instance.
    pub(crate) async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
    }
}

/// Why the leader could not tell how far the log is committed.
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{0}")]
struct ReadIndexError(String);

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("raft messages serialize")
}

fn unavailable(message: impl Into<String>) -> ErrorReply {
    ErrorReply::new(ErrorKind::Unavailable, message)
}

/// Brings a member that starts without history into the group, then lets
/// it take part in elections.
///
/// The founder founds the group with itself alone once every other
/// configured member answers and none belongs to a group yet. Any other
/// member, and a founder whose data directory was replaced while the group
/// runs, waits for the leader to add it and catches up with what the
/// leader has committed first.
async fn join(
    raft: Raft<TypeConfig>,
    network: Network,
    id: NodeId,
    members: BTreeMap<NodeId, String>,
    founder: bool,
) {
    let mut told = false;
    loop {
        if founder
            && !raft.is_initialized().await.unwrap_or(true)
            && unjoined(&network, id, &members).await
        {
            let me = BTreeMap::from([(id, BasicNode::new(&members[&id]))]);
            raft.runtime_config().elect(true);
            match raft.initialize(me).await {
                Ok(()) => return,
                Err(e) => tracing::warn!("cannot found the commit group: {e}"),
            }
        } else if caught_up(&raft, &network, id, &members).await {
            raft.runtime_config().elect(true);
            return;
        } else if !told {
            tracing::info!("member {id} is waiting to join the commit group");
            told = true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Returns whether every other member answers and none belongs to a group.
async fn unjoined(network: &Network, id: NodeId, members: &BTreeMap<NodeId, String>) -> bool {
    for (peer, addr) in members.iter().filter(|(peer, _)| **peer != id) {
        let mut peer = network.peer(*peer, addr.clone());
        let joined = peer
            .call::<_, bool, ReadIndexError>(RaftRpc::Probe, &())
            .await;
        if !matches!(joined, Ok(false)) {
            return false;
        }
    }
    true
}

/// Returns whether this member has applied everything another member that
/// leads had committed when asked.
async fn caught_up(
    raft: &Raft<TypeConfig>,
    network: &Network,
    id: NodeId,
    members: &BTreeMap<NodeId, String>,
) -> bool {
    let leader = raft.metrics().borrow().current_leader;
    let Some((leader, addr)) = leader
        .filter(|l| *l != id)
        .and_then(|l| members.get(&l).map(|a| (l, a.clone())))
    else {
        return false;
    };
    let mut peer = network.peer(leader, addr);
    let read = peer
        .call::<_, Option<LogId<NodeId>>, ReadIndexError>(RaftRpc::ReadIndex, &())
        .await;
    let Ok(read) = read else {
        return false;
    };
    let Some(read) = read else {
        return true;
    };
    raft.wait(Some(CATCH_UP))
        .applied_index_at_least(Some(read.index), "catch up before voting")
        .await
        .is_ok()
}

/// Keeps the group's voters equal to the configured members while this
/// member leads.
///
/// A group is founded by one member alone. Before the first other member
/// joins, the leader takes a snapshot and drops the log it covers: the
/// founder's database may hold history from before the group existed, which
/// is in no log entry, so joining members must start from the snapshot.
async fn maintain_membership(
    raft: Raft<TypeConfig>,
    id: NodeId,
    members: BTreeMap<NodeId, String>,
) {
    let wanted: BTreeSet<NodeId> = members.keys().copied().collect();
    let mut metrics = raft.metrics();
    loop {
        let (leading, voters) = {
            let m = metrics.borrow();
            let voters: BTreeSet<NodeId> = m.membership_config.membership().voter_ids().collect();
            (m.current_leader == Some(id), voters)
        };
        if leading
            && voters != wanted
            && let Err(e) = grow(&raft, &members, &wanted).await
        {
            tracing::info!("cannot add members to the commit group yet: {e}");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        if metrics.changed().await.is_err() {
            return;
        }
    }
}

async fn grow(
    raft: &Raft<TypeConfig>,
    members: &BTreeMap<NodeId, String>,
    wanted: &BTreeSet<NodeId>,
) -> Result<(), String> {
    let m = raft
        .wait(Some(CATCH_UP))
        .metrics(|m| m.last_applied.is_some(), "apply the first entry")
        .await
        .map_err(|e| e.to_string())?;
    let known: BTreeSet<NodeId> = m
        .membership_config
        .membership()
        .nodes()
        .map(|(id, _)| *id)
        .collect();
    let (purged, applied) = (m.purged, m.last_applied);
    if purged.is_none() {
        let applied = applied.ok_or("nothing is applied yet")?;
        raft.trigger().snapshot().await.map_err(|e| e.to_string())?;
        raft.wait(Some(CATCH_UP))
            .metrics(
                |m| m.snapshot.is_some_and(|s| s.index >= applied.index),
                "snapshot the founder's history",
            )
            .await
            .map_err(|e| e.to_string())?;
        raft.trigger()
            .purge_log(applied.index)
            .await
            .map_err(|e| e.to_string())?;
        raft.wait(Some(CATCH_UP))
            .metrics(|m| m.purged.is_some(), "drop the log the snapshot covers")
            .await
            .map_err(|e| e.to_string())?;
    }
    for (id, addr) in members {
        if !known.contains(id) {
            raft.add_learner(*id, BasicNode::new(addr), false)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    raft.change_membership(wanted.clone(), false)
        .await
        .map_err(|e| e.to_string())?;
    // Members that join from a snapshot must learn the new membership from
    // it, not from entries that may still be on their way.
    raft.trigger().snapshot().await.map_err(|e| e.to_string())?;
    Ok(())
}
