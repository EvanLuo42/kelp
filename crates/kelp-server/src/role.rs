//! What part a server plays, and where it sends the requests it does not
//! handle itself.

use std::sync::{Arc, OnceLock};

use kelp_client::{Connection, Trust};
use kelp_proto::{Auth, ErrorKind, ErrorReply, ServerRole};

use crate::State;
use crate::cluster::Cluster;
use crate::edge::{Edge, upstream_error};

/// The part a server plays.
pub(crate) enum Role {
    /// The server applies every change itself.
    AllInOne,
    /// A replica of a commit server's log, which forwards changes.
    Edge(Box<Edge>),
    /// A member of a commit group, which forwards changes while another
    /// member leads.
    Member(Membership),
}

/// The commit group a member belongs to, once [`Server::run`] has started
/// it.
///
/// [`Server::run`]: crate::Server::run
pub(crate) struct Membership {
    cluster: OnceLock<Arc<Cluster>>,
    /// The TLS identity the members share, which the leader presents.
    fingerprint: String,
}

impl Membership {
    pub(crate) fn new(fingerprint: String) -> Self {
        Self {
            cluster: OnceLock::new(),
            fingerprint,
        }
    }

    pub(crate) fn get(&self) -> Option<&Arc<Cluster>> {
        self.cluster.get()
    }

    pub(crate) fn set(&self, cluster: Arc<Cluster>) {
        let _ = self.cluster.set(cluster);
    }

    fn started(&self) -> Result<&Arc<Cluster>, ErrorReply> {
        self.get().ok_or_else(|| {
            ErrorReply::new(
                ErrorKind::Unavailable,
                "the commit group is starting; try again",
            )
        })
    }
}

/// Where a server sends what it does not handle itself.
#[derive(Clone, Copy)]
pub(crate) enum Forwarder<'a> {
    /// The commit server an edge replicates.
    Upstream(&'a Edge),
    /// The leader of this member's commit group.
    Leader(&'a Membership),
}

impl Forwarder<'_> {
    /// Connects to where requests go, authenticating with `auth`.
    pub(crate) async fn connect(self, auth: Auth) -> Result<Connection, ErrorReply> {
        match self {
            Self::Upstream(edge) => edge.connect(auth).await,
            Self::Leader(membership) => {
                let addr = membership.started()?.leader_addr().await?;
                let trust = Trust::Pinned(membership.fingerprint.clone());
                Connection::connect(&addr, trust, auth)
                    .await
                    .map_err(upstream_error)
            }
        }
    }

    /// Waits until this server reflects everything committed where requests
    /// go when this was called.
    pub(crate) async fn catch_up(self) -> Result<(), ErrorReply> {
        match self {
            Self::Upstream(edge) => edge.catch_up().await,
            Self::Leader(membership) => membership.started()?.catch_up().await,
        }
    }

    /// Returns whether the server stores content where requests go too, so
    /// uploads need not be kept a second time.
    pub(crate) fn shares_storage(self) -> bool {
        matches!(self, Self::Leader(_))
    }
}

impl State {
    /// Returns the replication state of an edge.
    pub(crate) fn edge(&self) -> Option<&Edge> {
        match &self.role {
            Role::Edge(edge) => Some(edge.as_ref()),
            _ => None,
        }
    }

    /// Returns the commit group this server is a member of, once it runs.
    pub(crate) fn cluster(&self) -> Option<&Arc<Cluster>> {
        match &self.role {
            Role::Member(membership) => membership.get(),
            _ => None,
        }
    }

    /// Returns where changes go if this server does not apply them itself.
    pub(crate) fn forwarder(&self) -> Option<Forwarder<'_>> {
        match &self.role {
            Role::AllInOne => None,
            Role::Edge(edge) => Some(Forwarder::Upstream(edge)),
            Role::Member(m) if m.get().is_some_and(|c| c.is_leader()) => None,
            Role::Member(m) => Some(Forwarder::Leader(m)),
        }
    }

    /// Waits until this server reflects everything committed anywhere when
    /// this was called.
    pub(crate) async fn catch_up(&self) -> Result<(), ErrorReply> {
        match &self.role {
            Role::AllInOne => Ok(()),
            Role::Edge(edge) => edge.catch_up().await,
            Role::Member(m) => m.started()?.catch_up().await,
        }
    }

    /// Returns the role reported to clients.
    pub(crate) fn server_role(&self) -> ServerRole {
        match &self.role {
            Role::AllInOne => ServerRole::AllInOne,
            Role::Edge(_) => ServerRole::Edge,
            Role::Member(_) if self.forwarder().is_some() => ServerRole::Member,
            Role::Member(_) => ServerRole::Commit,
        }
    }
}
