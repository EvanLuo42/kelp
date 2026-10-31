//! Reading and writing frames on asynchronous byte streams.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Error, Flags, FrameHeader, HEADER_LEN, MAX_PAYLOAD, MsgType, Result};

/// A frame read from a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Decodes the payload as a postcard-encoded message of type `T`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if the payload is not a valid `T`.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(postcard::from_bytes(&self.payload)?)
    }

    /// Returns an error unless the frame has type `expected`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unexpected`] if the types differ.
    pub fn expect(&self, expected: MsgType) -> Result<()> {
        match self.header.msg_type {
            got if got == expected => Ok(()),
            got => Err(Error::Unexpected { expected, got }),
        }
    }
}

/// Writes one frame with a raw payload.
///
/// # Errors
///
/// Returns [`Error::PayloadTooLarge`] if `payload` exceeds [`MAX_PAYLOAD`], or
/// [`Error::Io`] if writing fails.
pub async fn write_raw<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg_type: MsgType,
    flags: Flags,
    payload: &[u8],
) -> Result<()> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    if len > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge(len));
    }
    let header = FrameHeader {
        flags,
        msg_type,
        request_id: 0,
        payload_len: len,
    };
    w.write_all(&header.encode()).await?;
    w.write_all(payload).await?;
    Ok(())
}

/// Writes one frame whose payload is `message` encoded with postcard.
///
/// # Errors
///
/// Returns an error if the encoded message is too large or writing fails.
pub async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg_type: MsgType,
    message: &T,
) -> Result<()> {
    let payload = postcard::to_allocvec(message)?;
    write_raw(w, msg_type, Flags::default(), &payload).await
}

/// Writes a chunk frame: the 32-byte chunk id followed by its bytes.
///
/// # Errors
///
/// Returns an error if the chunk is too large or writing fails.
pub async fn write_chunk<W: AsyncWrite + Unpin>(
    w: &mut W,
    id: &[u8; 32],
    data: &[u8],
) -> Result<()> {
    let mut payload = Vec::with_capacity(32 + data.len());
    payload.extend_from_slice(id);
    payload.extend_from_slice(data);
    write_raw(w, MsgType::Chunk, Flags::RAW, &payload).await
}

/// Splits a chunk frame's payload into the chunk id and its bytes.
///
/// # Errors
///
/// Returns [`Error::UnexpectedEof`] if the payload is shorter than an id.
pub fn split_chunk(payload: &[u8]) -> Result<([u8; 32], &[u8])> {
    let (id, data) = payload.split_at_checked(32).ok_or(Error::UnexpectedEof)?;
    Ok((id.try_into().expect("split at 32 bytes"), data))
}

/// Reads one frame, or returns `None` if the stream ended cleanly before it.
///
/// # Errors
///
/// Returns an error if the header is invalid or the stream ends mid-frame.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>> {
    let mut buf = [0u8; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        match r.read(&mut buf[filled..]).await? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(Error::UnexpectedEof),
            n => filled += n,
        }
    }
    let header = FrameHeader::decode(&buf)?;
    let mut payload = vec![0u8; header.payload_len as usize];
    r.read_exact(&mut payload)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Error::UnexpectedEof,
            _ => Error::Io(e),
        })?;
    Ok(Some(Frame { header, payload }))
}

/// Reads one frame, treating the end of the stream as an error.
///
/// # Errors
///
/// Returns [`Error::UnexpectedEof`] if the stream has ended, or any error of
/// [`read_frame`].
pub async fn expect_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    read_frame(r).await?.ok_or(Error::UnexpectedEof)
}
