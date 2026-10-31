use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::{Error, Result};

/// The server's self-signed TLS certificate and key.
///
/// Clients trust the server by pinning [`Identity::fingerprint`] on first
/// connection.
#[derive(Clone, Debug)]
pub struct Identity {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl Identity {
    /// Loads the identity from `dir`, generating and saving one if absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the files cannot be read or written, or if
    /// certificate generation fails.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        if cert_path.exists() && key_path.exists() {
            return Ok(Self {
                cert: std::fs::read(cert_path)?,
                key: std::fs::read(key_path)?,
            });
        }
        let generated = rcgen::generate_simple_self_signed(vec!["kelp".to_owned()])
            .map_err(|e| Error::Tls(e.to_string()))?;
        let identity = Self {
            cert: generated.cert.der().to_vec(),
            key: generated.key_pair.serialize_der(),
        };
        std::fs::create_dir_all(dir)?;
        std::fs::write(&key_path, &identity.key)?;
        std::fs::write(&cert_path, &identity.cert)?;
        Ok(identity)
    }

    /// Returns the BLAKE3 hash of the DER certificate as lowercase hex.
    pub fn fingerprint(&self) -> String {
        blake3::hash(&self.cert).to_hex().to_string()
    }

    pub(crate) fn server_config(&self) -> Result<quinn::ServerConfig> {
        let tls_err = |e: rustls::Error| Error::Tls(e.to_string());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls_err)?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(self.cert.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone())),
            )
            .map_err(tls_err)?;
        tls.alpn_protocols = vec![kelp_proto::ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|e| Error::Tls(e.to_string()))?;
        Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
    }
}
