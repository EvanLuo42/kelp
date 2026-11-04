//! Raft messages between members, carried as [`Request::Raft`] over the
//! ordinary `kelp/1` protocol.

use kelp_client::{Connection, Trust};
use kelp_proto::rpc::RaftRpc;
use kelp_proto::{Auth, Reply, Request};
use openraft::BasicNode;
use openraft::error::{
    InstallSnapshotError, NetworkError, PayloadTooLarge, RPCError, RaftError, RemoteError,
    Unreachable,
};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{NodeId, TypeConfig};

/// The largest encoded message sent to another member, leaving room in a
/// protocol frame for the request around it.
const MAX_MESSAGE: usize = kelp_proto::MAX_PAYLOAD as usize - (64 << 10);

/// Creates connections to other members.
///
/// All members share one TLS identity, so each pins its own certificate.
#[derive(Clone)]
pub(crate) struct Network {
    pub(crate) secret: String,
    pub(crate) fingerprint: String,
}

impl RaftNetworkFactory<TypeConfig> for Network {
    type Network = Peer;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Peer {
        self.peer(target, node.addr.clone())
    }
}

impl Network {
    pub(crate) fn peer(&self, target: NodeId, addr: String) -> Peer {
        Peer {
            target,
            addr,
            secret: self.secret.clone(),
            fingerprint: self.fingerprint.clone(),
            conn: None,
        }
    }
}

/// A connection to one other member.
pub(crate) struct Peer {
    target: NodeId,
    addr: String,
    secret: String,
    fingerprint: String,
    conn: Option<Connection>,
}

/// Why a call to another member failed.
pub(crate) enum CallError<E> {
    /// The member could not be reached.
    Unreachable(Unreachable),
    /// The message was lost on the way.
    Network(NetworkError),
    /// The member received the message and answered with an error.
    Remote(E),
}

impl<E: std::error::Error> CallError<E> {
    fn into_rpc(self, target: NodeId) -> RPCError<NodeId, BasicNode, E> {
        match self {
            Self::Unreachable(e) => RPCError::Unreachable(e),
            Self::Network(e) => RPCError::Network(e),
            Self::Remote(e) => RPCError::RemoteError(RemoteError::new(target, e)),
        }
    }
}

impl<E> std::fmt::Display for CallError<E>
where
    E: std::fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => e.fmt(f),
            Self::Network(e) => e.fmt(f),
            Self::Remote(e) => e.fmt(f),
        }
    }
}

impl Peer {
    /// Sends `request` as `rpc` and returns the member's answer.
    pub(crate) async fn call<Req, Resp, E>(
        &mut self,
        rpc: RaftRpc,
        request: &Req,
    ) -> Result<Resp, CallError<E>>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
        E: DeserializeOwned,
    {
        let payload = postcard::to_allocvec(request).expect("raft messages serialize");
        let conn = match self.conn.as_ref().filter(|c| !c.is_closed()) {
            Some(c) => c.clone(),
            None => {
                let trust = Trust::Pinned(self.fingerprint.clone());
                let c = Connection::connect(&self.addr, trust, Auth::None)
                    .await
                    .map_err(|e| CallError::Unreachable(Unreachable::new(&e)))?;
                crate::handler::check_peer_version(&c)
                    .map_err(|e| CallError::Unreachable(Unreachable::new(&e)))?;
                // A member accepts a message larger than a frame only on a
                // connection that has shown the group's secret, so introduce
                // this one with a small message first.
                if payload.len() > MAX_MESSAGE {
                    let probe = Request::Raft {
                        secret: self.secret.clone(),
                        format: kelp_meta::db::FORMAT_VERSION,
                        rpc: RaftRpc::Probe,
                        payload: postcard::to_allocvec(&()).expect("unit serializes"),
                    };
                    c.call(&probe)
                        .await
                        .map_err(|e| CallError::Unreachable(Unreachable::new(&e)))?;
                }
                self.conn = Some(c.clone());
                c
            }
        };
        let request = Request::Raft {
            secret: self.secret.clone(),
            format: kelp_meta::db::FORMAT_VERSION,
            rpc,
            payload,
        };
        let reply = match conn.call(&request).await {
            Ok(Reply::Raft(bytes)) => bytes,
            Ok(_) => return Err(CallError::Network(NetworkError::new(&Unexpected))),
            Err(e) => {
                self.conn = None;
                return Err(CallError::Network(NetworkError::new(&e)));
            }
        };
        let result: Result<Resp, E> =
            postcard::from_bytes(&reply).map_err(|e| CallError::Network(NetworkError::new(&e)))?;
        result.map_err(CallError::Remote)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unexpected reply from another member")]
struct Unexpected;

impl RaftNetwork<TypeConfig> for Peer {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        let target = self.target;
        let entries = rpc.entries.len() as u64;
        if entries > 1 && postcard::to_allocvec(&rpc).map_or(0, |b| b.len()) > MAX_MESSAGE {
            // openraft sends fewer entries at a time when told.
            return Err(RPCError::PayloadTooLarge(
                PayloadTooLarge::new_entries_hint(entries / 2),
            ));
        }
        self.call(RaftRpc::AppendEntries, &rpc)
            .await
            .map_err(|e| e.into_rpc(target))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        let target = self.target;
        self.call(RaftRpc::InstallSnapshot, &rpc)
            .await
            .map_err(|e| e.into_rpc(target))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        let target = self.target;
        self.call(RaftRpc::Vote, &rpc)
            .await
            .map_err(|e| e.into_rpc(target))
    }
}
