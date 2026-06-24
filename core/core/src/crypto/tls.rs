//! TLS material + rustls config helpers for the TLS-based transports (WSS/QUIC).
//!
//! The crypto provider is pinned to `ring` (see workspace `Cargo.toml`). We
//! install it as the process default once, lazily.

use crate::{Error, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;

/// ALPN protocol id advertised by both ends.
pub const ALPN: &[u8] = b"et1";

fn ensure_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Generate a self-signed certificate + key (PEM) for the given SAN names.
pub fn generate_self_signed(sans: Vec<String>) -> Result<(String, String)> {
    let certified = rcgen::generate_simple_self_signed(sans)
        .map_err(|e| Error::Crypto(format!("rcgen self-signed: {e}")))?;
    Ok((certified.cert.pem(), certified.key_pair.serialize_pem()))
}

/// Parse a PEM certificate chain into owned DER blobs.
pub fn parse_cert_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Crypto(format!("parse cert pem: {e}")))?;
    if certs.is_empty() {
        return Err(Error::Crypto("no certificates in PEM".into()));
    }
    Ok(certs)
}

/// Parse a PEM private key (PKCS#8 / RSA / SEC1).
pub fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| Error::Crypto(format!("parse key pem: {e}")))?
        .ok_or_else(|| Error::Crypto("no private key in PEM".into()))
}

/// Build a rustls `ServerConfig` from PEM cert + key.
pub fn server_config(cert_pem: &str, key_pem: &str) -> Result<Arc<rustls::ServerConfig>> {
    ensure_provider();
    let certs = parse_cert_chain(cert_pem)?;
    let key = parse_private_key(key_pem)?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Crypto(format!("server tls config: {e}")))?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(cfg))
}

/// Build a rustls `ClientConfig`. `skip_verify` accepts any certificate
/// (self-hosted, explicit); otherwise the optional pinned cert (PEM) is the
/// only trusted root.
pub fn client_config(skip_verify: bool, pinned_pem: Option<&str>) -> Result<Arc<rustls::ClientConfig>> {
    ensure_provider();
    let mut cfg = if skip_verify {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(client_roots(pinned_pem)?)
            .with_no_client_auth()
    };
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(cfg))
}

/// Build the trusted-root set for a verifying client:
/// - a pinned PEM (self-signed servers) → trust *only* that cert,
/// - otherwise → trust the public Mozilla CA set, so servers fronted by a real
///   certificate (e.g. nginx + Let's Encrypt) verify normally.
fn client_roots(pinned_pem: Option<&str>) -> Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(pem) = pinned_pem {
        for cert in parse_cert_chain(pem)? {
            roots.add(cert).map_err(|e| Error::Crypto(format!("add pinned cert: {e}")))?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    Ok(roots)
}

/// QUIC requires TLS 1.3 only. Build a 1.3-only rustls `ServerConfig` for quinn.
pub fn quic_server_config(cert_pem: &str, key_pem: &str) -> Result<rustls::ServerConfig> {
    ensure_provider();
    let certs = parse_cert_chain(cert_pem)?;
    let key = parse_private_key(key_pem)?;
    let mut cfg = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Crypto(format!("quic server tls config: {e}")))?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

/// 1.3-only rustls `ClientConfig` for quinn.
pub fn quic_client_config(skip_verify: bool, pinned_pem: Option<&str>) -> Result<rustls::ClientConfig> {
    ensure_provider();
    let builder = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13]);
    let mut cfg = if skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        builder
            .with_root_certificates(client_roots(pinned_pem)?)
            .with_no_client_auth()
    };
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

/// Accept-any-certificate verifier for self-hosted servers (`tls_skip_verify`).
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
