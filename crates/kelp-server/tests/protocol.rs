//! Peers of an older protocol version are refused with an error they can
//! read, rather than left to misread messages.

use std::sync::Arc;

use kelp_proto::io::{expect_frame, write_message};
use kelp_proto::{
    Auth, Capabilities, ErrorKind, ErrorReply, Hello, MIN_PROTOCOL_VERSION, MsgType,
    PROTOCOL_VERSION, Request,
};
use kelp_testkit::net::TestServer;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// Accepts any certificate; the test talks to its own server.
#[derive(Debug)]
struct AnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
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
            &self.0.signature_verification_algorithms,
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
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Opens a QUIC connection to `server` the way any Kelp client does.
async fn raw_connection(server: &TestServer) -> (quinn::Endpoint, quinn::Connection) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![kelp_proto::ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let addr = server.addr().parse().unwrap();
    let conn = endpoint.connect(addr, "kelp").unwrap().await.unwrap();
    (endpoint, conn)
}

/// Sends one frame on a new stream and reads the answering frame.
async fn exchange<T: serde::Serialize>(
    conn: &quinn::Connection,
    msg_type: MsgType,
    message: &T,
) -> kelp_proto::io::Frame {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_message(&mut send, msg_type, message).await.unwrap();
    send.finish().unwrap();
    expect_frame(&mut recv).await.unwrap()
}

fn hello(min_version: u16, max_version: u16) -> Hello {
    Hello {
        min_version,
        max_version,
        capabilities: Capabilities::default(),
        client_name: "kelp-client/0.1.0".into(),
        auth: Auth::None,
    }
}

#[test]
fn profiles_changed_the_wire_format() {
    assert!(MIN_PROTOCOL_VERSION >= 2);
    assert!(PROTOCOL_VERSION >= MIN_PROTOCOL_VERSION);
}

#[tokio::test]
async fn version_1_clients_are_told_to_upgrade() {
    let server = TestServer::start().await;
    let (_endpoint, conn) = raw_connection(&server).await;

    // What Kelp 0.1.0 sends.
    let frame = exchange(&conn, MsgType::Hello, &hello(1, 1)).await;
    assert_eq!(frame.header.msg_type, MsgType::Error);
    let error: ErrorReply = frame.decode().unwrap();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(error.message.contains("upgrade the client"), "{error}");
    assert!(error.message.contains("kelp-client/0.1.0"), "{error}");

    // Requests on a connection whose Hello was refused are not decoded.
    let frame = exchange(&conn, MsgType::Request, &Request::ListStreams).await;
    assert_eq!(frame.header.msg_type, MsgType::Error);
    let error: ErrorReply = frame.decode().unwrap();
    assert!(error.message.contains("Hello"), "{error}");
}

#[tokio::test]
async fn clients_offering_older_versions_get_the_newest() {
    let server = TestServer::start().await;
    let (_endpoint, conn) = raw_connection(&server).await;
    let frame = exchange(&conn, MsgType::Hello, &hello(1, PROTOCOL_VERSION)).await;
    assert_eq!(frame.header.msg_type, MsgType::Welcome);
    let welcome: kelp_proto::Welcome = frame.decode().unwrap();
    assert_eq!(welcome.version, PROTOCOL_VERSION);

    let frame = exchange(&conn, MsgType::Hello, &hello(PROTOCOL_VERSION + 1, u16::MAX)).await;
    let error: ErrorReply = frame.decode().unwrap();
    assert!(error.message.contains("upgrade the server"), "{error}");
}
