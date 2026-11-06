use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use kelp_core::{ChunkId, ContentId, Hash, TreeId};
use kelp_proto::io::{expect_frame, split_chunk, write_chunk, write_message};
use kelp_proto::rpc::{FileDelta, FileInfo, StreamInfo};
use kelp_proto::{
    Auth, Capabilities, ErrorReply, Hello, MIN_PROTOCOL_VERSION, MsgType, PROTOCOL_VERSION, Reply,
    Request, Welcome,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::task::JoinSet;

use crate::{Error, Result};

const UPLOAD_BATCH_BYTES: usize = 32 << 20;
const BATCH_CHUNKS: usize = 256;
const PARALLEL_STREAMS: usize = 8;

/// How to decide whether to trust a server's certificate.
///
/// Servers use self-signed certificates, so clients pin the certificate's
/// fingerprint the first time they connect, like SSH host keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Accept only a certificate with this fingerprint.
    Pinned(String),
    /// Accept any certificate and report its fingerprint.
    FirstUse,
}

/// A connection to a Kelp server.
#[derive(Clone)]
pub struct Connection {
    conn: quinn::Connection,
    _endpoint: quinn::Endpoint,
    fingerprint: String,
    welcome: Welcome,
}

impl Connection {
    /// Connects to `server` (`host` or `host:port`) and performs the handshake.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UntrustedServer`] if the certificate does not match a
    /// pinned fingerprint, [`Error::Remote`] if the server rejects `auth`, or
    /// [`Error::Connect`] if the server cannot be reached.
    pub async fn connect(server: &str, trust: Trust, auth: Auth) -> Result<Self> {
        let addr = resolve(server).await?;
        let seen = Arc::new(Mutex::new(None));
        let pinned = match &trust {
            Trust::Pinned(fp) => Some(fp.clone()),
            Trust::FirstUse => None,
        };
        let config = client_config(pinned.clone(), seen.clone())?;
        let bind: SocketAddr = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        }
        .parse()
        .expect("valid address");
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(config);
        let connect_err =
            |e: &dyn std::fmt::Display| Error::Connect(server.to_owned(), e.to_string());
        let connecting = endpoint
            .connect(addr, "kelp")
            .map_err(|e| connect_err(&e))?;
        let conn = match connecting.await {
            Ok(conn) => conn,
            Err(e) => {
                let actual = seen.lock().expect("fingerprint lock").clone();
                return Err(match (pinned, actual) {
                    (Some(expected), Some(actual)) if expected != actual => {
                        Error::UntrustedServer { expected, actual }
                    }
                    _ => connect_err(&e),
                });
            }
        };
        let fingerprint = seen
            .lock()
            .expect("fingerprint lock")
            .clone()
            .expect("verifier ran during handshake");

        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| connect_err(&e))?;
        let hello = Hello {
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            capabilities: Capabilities::default(),
            client_name: concat!("kelp-client/", env!("CARGO_PKG_VERSION")).to_owned(),
            auth,
        };
        write_message(&mut send, MsgType::Hello, &hello).await?;
        finish(&mut send);
        let frame = expect_frame(&mut recv).await?;
        let welcome = match frame.header.msg_type {
            MsgType::Welcome => frame.decode()?,
            MsgType::Error => return Err(Error::Remote(frame.decode()?)),
            got => {
                return Err(kelp_proto::Error::Unexpected {
                    expected: MsgType::Welcome,
                    got,
                }
                .into());
            }
        };
        Ok(Self {
            conn,
            _endpoint: endpoint,
            fingerprint,
            welcome,
        })
    }

    /// Returns the fingerprint of the server's certificate.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns `true` once the connection has been closed by either side.
    pub fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    /// Returns the server's answer to the handshake.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Sends a request and waits for its reply.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Remote`] if the server rejects the request, or an error
    /// if the connection fails.
    pub async fn call(&self, request: &Request) -> Result<Reply> {
        let (mut send, mut recv) = self.open().await?;
        write_message(&mut send, MsgType::Request, request).await?;
        finish(&mut send);
        read_reply(&mut recv).await
    }

    /// Returns `stream` as of changelist `at`, or its head.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Remote`] if the stream does not exist or may not be
    /// read, or an error if the connection fails.
    pub async fn resolve_stream(&self, stream: &str, at: Option<u64>) -> Result<StreamInfo> {
        let request = Request::ResolveStream {
            stream: stream.to_owned(),
            at,
        };
        match self.call(&request).await? {
            Reply::Stream(info) => Ok(info),
            _ => Err(Error::UnexpectedReply),
        }
    }

    /// Returns the files that differ between the trees `from` and `to` of
    /// `stream`, and the entries of `paths` in `to`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Remote`] if the stream may not be read, or an error
    /// if the connection fails.
    pub async fn diff_tree(
        &self,
        stream: &str,
        from: Option<TreeId>,
        to: TreeId,
        paths: Vec<String>,
    ) -> Result<(Vec<FileDelta>, Vec<Option<FileInfo>>)> {
        let request = Request::DiffTree {
            stream: stream.to_owned(),
            from,
            to,
            paths,
        };
        match self.call(&request).await? {
            Reply::TreeDiff { changes, lookups } => Ok((changes, lookups)),
            _ => Err(Error::UnexpectedReply),
        }
    }

    /// Returns what is below the stream-relative `path` of `stream`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the server is too old to list directories,
    /// [`Error::Remote`] if the path does not exist or may not be read, or an
    /// error if the connection fails.
    pub async fn list_dir(
        &self,
        stream: &str,
        path: &str,
        options: kelp_proto::rpc::ListOptions,
    ) -> Result<Vec<kelp_proto::rpc::DirEntry>> {
        // An older server cannot decode the request and only drops the stream.
        if !self.welcome().capabilities.contains(Capabilities::LIST_DIR) {
            return Err(Error::Invalid(
                "this server is too old to list directories; upgrade kelp-server".into(),
            ));
        }
        let request = Request::ListDir {
            stream: stream.to_owned(),
            path: path.to_owned(),
            options,
        };
        match self.call(&request).await? {
            Reply::DirEntries(entries) => Ok(entries),
            _ => Err(Error::UnexpectedReply),
        }
    }

    /// Uploads chunks, several batches in parallel.
    ///
    /// # Errors
    ///
    /// Returns an error if any batch fails.
    pub async fn put_chunks(&self, chunks: Vec<(ChunkId, Bytes)>) -> Result<()> {
        let mut batches: Vec<Vec<(ChunkId, Bytes)>> = Vec::new();
        let mut size = 0;
        for chunk in chunks {
            if batches
                .last()
                .is_none_or(|b| b.len() >= BATCH_CHUNKS || size >= UPLOAD_BATCH_BYTES)
            {
                batches.push(Vec::new());
                size = 0;
            }
            size += chunk.1.len();
            batches.last_mut().expect("batch pushed").push(chunk);
        }
        self.run_parallel(batches, |conn, batch| async move {
            let (mut send, mut recv) = conn.open().await?;
            write_message(
                &mut send,
                MsgType::Request,
                &Request::PutChunks {
                    count: batch.len() as u32,
                },
            )
            .await?;
            for (id, data) in &batch {
                write_chunk(&mut send, &id.0.0, data).await?;
            }
            finish(&mut send);
            read_reply(&mut recv).await.map(drop)
        })
        .await
        .map(drop)
    }

    /// Downloads chunks, several batches in parallel.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk is missing on the server or does not
    /// match its id.
    pub async fn get_chunks(&self, ids: &[ChunkId]) -> Result<Vec<(ChunkId, Bytes)>> {
        let batches: Vec<Request> = ids
            .chunks(BATCH_CHUNKS)
            .map(|b| Request::GetChunks { ids: b.to_vec() })
            .collect();
        self.download_chunks(batches).await
    }

    /// Downloads chunks, each named with the content it belongs to, so a
    /// server that checks read permission on content can allow it.
    ///
    /// Falls back to [`get_chunks`](Self::get_chunks) for servers that do
    /// not check.
    ///
    /// # Errors
    ///
    /// As for [`get_chunks`](Self::get_chunks), and [`Error::Remote`] if the
    /// user may not read the content.
    pub async fn get_content_chunks(
        &self,
        wanted: &[(ChunkId, ContentId)],
    ) -> Result<Vec<(ChunkId, Bytes)>> {
        if !self
            .welcome
            .capabilities
            .contains(Capabilities::CONTENT_ACCESS)
        {
            let ids: Vec<ChunkId> = wanted.iter().map(|(id, _)| *id).collect();
            return self.get_chunks(&ids).await;
        }
        let batches: Vec<Request> = wanted
            .chunks(BATCH_CHUNKS)
            .map(|b| {
                let mut contents: Vec<ContentId> = b.iter().map(|(_, c)| *c).collect();
                contents.sort();
                contents.dedup();
                Request::GetContentChunks {
                    contents,
                    ids: b.iter().map(|(id, _)| *id).collect(),
                }
            })
            .collect();
        self.download_chunks(batches).await
    }

    async fn download_chunks(&self, batches: Vec<Request>) -> Result<Vec<(ChunkId, Bytes)>> {
        let results = self
            .run_parallel(batches, |conn, request| async move {
                let (mut send, mut recv) = conn.open().await?;
                write_message(&mut send, MsgType::Request, &request).await?;
                finish(&mut send);
                let mut out = Vec::new();
                loop {
                    let frame = expect_frame(&mut recv).await?;
                    match frame.header.msg_type {
                        MsgType::Chunk => {
                            let (id, data) = split_chunk(&frame.payload)?;
                            let id = ChunkId(Hash(id));
                            if ChunkId::of(data) != id {
                                return Err(Error::Invalid(format!(
                                    "server sent corrupt chunk {id}"
                                )));
                            }
                            out.push((id, Bytes::copy_from_slice(data)));
                        }
                        MsgType::Reply => return Ok(out),
                        MsgType::Error => return Err(Error::Remote(frame.decode::<ErrorReply>()?)),
                        _ => return Err(Error::UnexpectedReply),
                    }
                }
            })
            .await?;
        Ok(results.into_iter().flatten().collect())
    }

    async fn run_parallel<T, R, F, Fut>(&self, items: Vec<T>, f: F) -> Result<Vec<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(Connection, T) -> Fut,
        Fut: Future<Output = Result<R>> + Send + 'static,
    {
        let mut set = JoinSet::new();
        let mut out = Vec::new();
        for item in items {
            if set.len() >= PARALLEL_STREAMS
                && let Some(done) = set.join_next().await
            {
                out.push(done.map_err(|e| Error::Invalid(e.to_string()))??);
            }
            set.spawn(f(self.clone(), item));
        }
        while let Some(done) = set.join_next().await {
            out.push(done.map_err(|e| Error::Invalid(e.to_string()))??);
        }
        Ok(out)
    }

    async fn open(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        self.conn
            .open_bi()
            .await
            .map_err(|e| Error::Connect(self.conn.remote_address().to_string(), e.to_string()))
    }
}

fn finish(send: &mut quinn::SendStream) {
    let _ = send.finish();
}

async fn read_reply(recv: &mut quinn::RecvStream) -> Result<Reply> {
    let frame = expect_frame(recv).await?;
    match frame.header.msg_type {
        MsgType::Reply => Ok(frame.decode()?),
        MsgType::Error => Err(Error::Remote(frame.decode()?)),
        _ => Err(Error::UnexpectedReply),
    }
}

async fn resolve(server: &str) -> Result<SocketAddr> {
    let with_port = if server
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        server.to_owned()
    } else {
        format!("{server}:{}", kelp_proto::DEFAULT_PORT)
    };
    tokio::net::lookup_host(&with_port)
        .await
        .map_err(|e| Error::Connect(server.to_owned(), e.to_string()))?
        .next()
        .ok_or_else(|| Error::Connect(server.to_owned(), "no address found".into()))
}

fn client_config(
    pinned: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinVerifier {
        pinned,
        seen,
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::Invalid(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![kelp_proto::ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

/// Accepts the server certificate if its fingerprint matches the pinned one,
/// and records the fingerprint it saw.
#[derive(Debug)]
struct PinVerifier {
    pinned: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = blake3::hash(end_entity).to_hex().to_string();
        *self.seen.lock().expect("fingerprint lock") = Some(fingerprint.clone());
        match &self.pinned {
            Some(pinned) if *pinned != fingerprint => Err(rustls::Error::General(
                "server certificate fingerprint mismatch".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
