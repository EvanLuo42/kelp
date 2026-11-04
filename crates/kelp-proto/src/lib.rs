//! The `kelp/1` wire protocol.
//!
//! A connection carries [frames](frame), each a fixed 16-byte header followed
//! by a payload. Every request runs on its own bidirectional stream: the
//! client writes a [`Request`] frame, optionally followed by chunk frames, and
//! the server answers with a [`Reply`] or an [`ErrorReply`] frame, optionally
//! preceded by chunk frames.
//!
//! The protocol is transport-agnostic. Frames run over QUIC streams, and over
//! any other ordered byte stream such as the local kelpd IPC channel.

pub mod frame;
pub mod io;
pub mod message;
pub mod rpc;

pub use frame::{Flags, FrameHeader, HEADER_LEN, MAX_PAYLOAD};
pub use message::{Auth, Capabilities, Hello, MsgType, ServerRole, Welcome};
pub use rpc::{ErrorKind, ErrorReply, Reply, Request, RequestClass};

/// The ALPN protocol identifier negotiated during the TLS handshake.
pub const ALPN: &[u8] = b"kelp/1";

/// The highest protocol version this build speaks.
///
/// Messages are postcard-encoded, which carries no field names and
/// identifies enum variants by position, so a peer decodes a message only
/// if it has exactly the same shape. Appending an enum variant is
/// compatible. Anything else, such as adding, removing or reordering a
/// struct field or inserting a variant before others, changes the wire
/// format: bump this and [`MIN_PROTOCOL_VERSION`] together so that older
/// peers are refused at the [`Hello`] instead of misreading messages.
///
/// Version history:
///
/// 1. Kelp 0.1.0.
/// 2. User profiles: `Request::EditProfile` was inserted before
///    `Request::Raft`, and `UserInfo`, `ChangeSummary` and `ReviewSummary`
///    gained fields.
pub const PROTOCOL_VERSION: u16 = 2;

/// The lowest protocol version this build speaks.
///
/// The [`Hello`], [`Welcome`] and [`ErrorReply`] messages never change
/// shape, so a peer of any version can read the error that refuses it.
pub const MIN_PROTOCOL_VERSION: u16 = 2;

/// The default port of `kelp-server`.
pub const DEFAULT_PORT: u16 = 7474;

/// An error encountered while encoding, decoding or transferring frames.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The first header byte was not the frame magic.
    #[error("bad frame magic {0:#04x}")]
    BadMagic(u8),
    /// The frame uses a header layout this build does not understand.
    #[error("unsupported frame version {0}")]
    UnsupportedFrameVersion(u8),
    /// The declared payload length exceeds [`MAX_PAYLOAD`].
    #[error("payload of {0} bytes exceeds limit")]
    PayloadTooLarge(u32),
    /// The header names a message type this build does not know.
    #[error("unknown message type {0}")]
    UnknownMessage(u16),
    /// A frame of a different type than expected arrived.
    #[error("expected {expected:?} frame, got {got:?}")]
    Unexpected { expected: MsgType, got: MsgType },
    /// The stream ended before a complete frame was read.
    #[error("stream ended unexpectedly")]
    UnexpectedEof,
    /// The payload could not be decoded.
    #[error("malformed payload: {0}")]
    Decode(#[from] postcard::Error),
    /// The underlying stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A specialized [`Result`](std::result::Result) type for protocol operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;
