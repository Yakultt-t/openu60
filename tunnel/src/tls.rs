//! QUIC/TLS configuration. Certificates are self-signed and only the Ed25519 public key inside
//! them matters: the phone pins the router's key, the router keeps an allow-list of paired phones.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring as provider, verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

use crate::identity::Identity;
use crate::proto::PubKey;

pub const ALPN: &[u8] = b"ou60/1";
const ED25519_OID: &str = "1.3.101.112";

/// Phones allowed to open a tunnel to the router, by Ed25519 public key.
pub type AllowList = Arc<RwLock<HashSet<PubKey>>>;

/// Extracts the Ed25519 public key from a DER certificate.
pub fn public_key_of_cert(der: &[u8]) -> Option<PubKey> {
    use der::Decode;
    let cert = x509_cert::Certificate::from_der(der).ok()?;
    let spki = &cert.tbs_certificate.subject_public_key_info;
    if spki.algorithm.oid.to_string() != ED25519_OID {
        return None;
    }
    spki.subject_public_key.raw_bytes().try_into().ok()
}

#[derive(Debug)]
struct PinnedServer {
    expected: PubKey,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match public_key_of_cert(end_entity) {
            Some(pk) if pk == self.expected => Ok(ServerCertVerified::assertion()),
            Some(pk) => {
                log::warn!("router presented unexpected key {}", hex::encode(pk));
                Err(rustls::Error::General("router key mismatch".into()))
            }
            None => Err(rustls::Error::General("router certificate is not Ed25519".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Accepts any phone that presents a well-formed Ed25519 certificate and proves it holds the
/// key. Whether that key is *paired* is decided after the handshake (see `device.rs`): an
/// unpaired phone only reaches the router's bootstrap port, where it can log in with the
/// password and pair itself. The router's identity is still pinned on the phone side, so the
/// relay cannot impersonate it.
#[derive(Debug)]
struct AnyPhoneVerifier {
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyPhoneVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        match public_key_of_cert(end_entity) {
            Some(_) => Ok(ClientCertVerified::assertion()),
            None => Err(rustls::Error::General("phone certificate is not Ed25519".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Transport tuning shared by both ends. The MTU is pinned at 1200 so a packet still fits once
/// the relay adds its 22-byte header; discovery would otherwise grow it past that.
fn transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(60).try_into().expect("idle timeout")));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.initial_mtu(1200);
    t.min_mtu(1200);
    t.mtu_discovery_config(None);
    t.datagram_receive_buffer_size(Some(256 * 1024));
    t.datagram_send_buffer_size(256 * 1024);
    t
}

pub fn client_config(identity: &Identity, router_key: PubKey) -> Result<quinn::ClientConfig, String> {
    let provider = Arc::new(provider::default_provider());
    let algs = provider.signature_verification_algorithms;
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("tls versions: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer { expected: router_key, algs }))
        .with_client_auth_cert(vec![identity.cert()], identity.private_key())
        .map_err(|e| format!("client cert: {e}"))?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(cfg).map_err(|e| format!("quic client config: {e}"))?;
    let mut c = quinn::ClientConfig::new(Arc::new(quic));
    c.transport_config(Arc::new(transport()));
    Ok(c)
}

pub fn server_config(identity: &Identity) -> Result<quinn::ServerConfig, String> {
    let provider = Arc::new(provider::default_provider());
    let algs = provider.signature_verification_algorithms;
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("tls versions: {e}"))?
        .with_client_cert_verifier(Arc::new(AnyPhoneVerifier { algs }))
        .with_single_cert(vec![identity.cert()], identity.private_key())
        .map_err(|e| format!("server cert: {e}"))?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(cfg).map_err(|e| format!("quic server config: {e}"))?;
    let mut s = quinn::ServerConfig::with_crypto(Arc::new(quic));
    s.transport_config(Arc::new(transport()));
    Ok(s)
}
