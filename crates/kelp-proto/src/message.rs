//! Message types and the session handshake.

use serde::{Deserialize, Serialize};

use crate::Error;

/// The type of a frame's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum MsgType {
    /// A [`Hello`] from the client, the first frame of a connection.
    Hello = 1,
    /// A [`Welcome`] answering a [`Hello`].
    Welcome = 2,
    /// An [`ErrorReply`](crate::ErrorReply).
    Error = 3,
    /// A [`Request`](crate::Request).
    Request = 4,
    /// A [`Reply`](crate::Reply).
    Reply = 5,
    /// A chunk: a 32-byte chunk id followed by the chunk's bytes.
    Chunk = 6,
}

impl TryFrom<u16> for MsgType {
    type Error = Error;

    fn try_from(v: u16) -> Result<Self, Error> {
        Ok(match v {
            1 => Self::Hello,
            2 => Self::Welcome,
            3 => Self::Error,
            4 => Self::Request,
            5 => Self::Reply,
            6 => Self::Chunk,
            other => return Err(Error::UnknownMessage(other)),
        })
    }
}

/// Optional protocol features, negotiated as the intersection of both sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities(pub u64);

impl Capabilities {
    /// Shelving individual hunks of text files.
    pub const HUNK_SHELVE: Self = Self(1 << 0);
    /// The artifact store.
    pub const ARTIFACTS: Self = Self(1 << 1);
    /// Server-side WASM plugins.
    pub const WASM_PLUGINS: Self = Self(1 << 2);
    /// zstd-compressed frame payloads.
    pub const ZSTD_FRAMES: Self = Self(1 << 3);
    /// Checking read permission on content: the server answers
    /// [`GetContentChunks`](crate::Request::GetContentChunks), and holds back
    /// manifests and chunks of content the user may not read.
    pub const CONTENT_ACCESS: Self = Self(1 << 4);
    /// Listing directories: the server answers
    /// [`ListDir`](crate::Request::ListDir).
    pub const LIST_DIR: Self = Self(1 << 5);

    /// Returns `true` if all capabilities in `other` are present in `self`.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the capabilities present in both `self` and `other`.
    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// The role a server plays in its cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerRole {
    AllInOne,
    /// The leader of a commit group.
    Commit,
    Edge,
    /// A commit group member that forwards writes to the leader.
    Member,
}

/// Credentials presented in a [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    /// No credentials; only [`Request::Login`](crate::Request::Login) is allowed.
    None,
    /// An access token issued by the server.
    Bearer(String),
}

/// The client's opening message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub min_version: u16,
    pub max_version: u16,
    pub capabilities: Capabilities,
    /// A human-readable client identifier such as `kelp-cli/0.1.0`.
    pub client_name: String,
    pub auth: Auth,
}

impl Hello {
    /// Returns the highest protocol version supported by both sides, if any.
    pub fn negotiate(&self, server_min: u16, server_max: u16) -> Option<u16> {
        let v = self.max_version.min(server_max);
        (v >= self.min_version.max(server_min)).then_some(v)
    }
}

/// The server's answer to a [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u16,
    pub role: ServerRole,
    /// The highest log index applied by the server.
    pub log_index: u64,
    pub capabilities: Capabilities,
    /// The authenticated user, if the [`Hello`] carried valid credentials.
    pub user: Option<String>,
}
