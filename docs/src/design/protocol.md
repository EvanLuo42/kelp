# The Wire Protocol

Kelp speaks `kelp/1` over QUIC with TLS 1.3.
Every request uses its own bidirectional stream, so a slow download does not block other requests.

## Frames

```text
0      1       2       4          6          8            12            16
| 'K'  | fver  | flags | msg_type | reserved | request_id | payload_len |
| payload: a postcard-encoded message, or raw chunk bytes; at most 16 MiB |
```

The header is 16 bytes, little-endian.
There are six message types: `Hello`, `Welcome`, `Error`, `Request`, `Reply` and `Chunk`.
Operations are variants of the `Request` and `Reply` enums.
New variants are only appended, so an older peer still decodes the ones it knows.
Any other change to a message, such as a new field, changes the protocol version; see below.

A client writes a `Request` frame, followed by `Chunk` frames for an upload, and closes its side.
The server answers with a `Reply` or an `Error`, preceded by `Chunk` frames for a download.
A `Chunk` payload is a 32-byte chunk id followed by the data, and the receiver checks the hash.

## Handshake

The client sends `Hello { min_version, max_version, capabilities, auth }`, where `auth` optionally carries a token.
The server answers `Welcome { version, role, log_index, capabilities, user }`.
The version is the highest one both sides speak.
If there is none, the server answers with an `Error` that says which side to upgrade, and refuses requests on that connection.
`Hello`, `Welcome` and `Error` never change shape, so peers of every version can read them.
Servers check the version an upstream server or another commit group member accepted in the same way.
Without a token, only login requests are allowed.

## Trust

Servers use self-signed certificates.
A client pins the certificate's BLAKE3 fingerprint on first use and refuses a different one later.
Members of a commit group share one certificate, so a fingerprint holds for any of them.

Integrations should use the C SDK through kelpd rather than speak the protocol directly.
