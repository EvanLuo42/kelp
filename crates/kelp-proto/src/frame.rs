//! Frame headers.
//!
//! Every frame starts with a fixed 16-byte little-endian header:
//!
//! ```text
//! 0      1        2       4          6          8            12            16
//! | 'K'  | fver   | flags | msg_type | reserved | request_id | payload_len |
//! ```

use crate::{Error, MsgType, Result};

/// The first byte of every frame.
pub const MAGIC: u8 = b'K';

/// The header layout version written by this build.
pub const FRAME_VERSION: u8 = 1;

/// The size of an encoded [`FrameHeader`] in bytes.
pub const HEADER_LEN: usize = 16;

/// The largest payload a single frame may carry, in bytes.
pub const MAX_PAYLOAD: u32 = 16 << 20;

/// The largest message, in bytes, that may be split across frames marked
/// [`Flags::CONTINUED`].
///
/// Peers that have not logged in are held to [`MAX_PAYLOAD`].
pub const MAX_MESSAGE: u32 = 1 << 30;

/// Bit flags carried in a frame header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u16);

impl Flags {
    /// The frame is the last one of its request or response.
    pub const END_OF_STREAM: Self = Self(1 << 0);
    /// The payload is a zstd frame.
    pub const COMPRESSED: Self = Self(1 << 1);
    /// The payload is raw bytes rather than a postcard-encoded message.
    pub const RAW: Self = Self(1 << 2);
    /// The message continues in the next frame, which has the same type.
    ///
    /// A message larger than [`MAX_PAYLOAD`] is sent as several frames, all
    /// but the last with this flag.
    pub const CONTINUED: Self = Self(1 << 3);

    /// Returns `true` if all bits of `other` are set in `self`.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Flags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A decoded frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub flags: Flags,
    pub msg_type: MsgType,
    /// Correlates frames of one exchange on multiplexed transports.
    pub request_id: u32,
    pub payload_len: u32,
}

impl FrameHeader {
    /// Encodes the header into its 16-byte wire form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = MAGIC;
        b[1] = FRAME_VERSION;
        b[2..4].copy_from_slice(&self.flags.0.to_le_bytes());
        b[4..6].copy_from_slice(&(self.msg_type as u16).to_le_bytes());
        b[8..12].copy_from_slice(&self.request_id.to_le_bytes());
        b[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        b
    }

    /// Decodes a header from its 16-byte wire form.
    ///
    /// # Errors
    ///
    /// Returns an error if the magic or version byte is wrong, the message type
    /// is unknown, or the payload length exceeds [`MAX_PAYLOAD`].
    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self> {
        if b[0] != MAGIC {
            return Err(Error::BadMagic(b[0]));
        }
        if b[1] != FRAME_VERSION {
            return Err(Error::UnsupportedFrameVersion(b[1]));
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let payload_len = u32_at(12);
        if payload_len > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            flags: Flags(u16_at(2)),
            msg_type: MsgType::try_from(u16_at(4))?,
            request_id: u32_at(8),
            payload_len,
        })
    }
}
