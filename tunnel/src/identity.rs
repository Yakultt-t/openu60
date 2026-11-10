//! One Ed25519 key per endpoint: it signs relay registrations and is the key inside the
//! self-signed certificate used for the QUIC handshake. The router's id is derived from it.

use std::path::Path;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ED25519};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, ED25519};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::proto::{DeviceId, PubKey};

pub struct Identity {
    pkcs8: Vec<u8>,
    key: Ed25519KeyPair,
    pub public: PubKey,
    cert_der: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("id", &device_id_hex(&self.device_id())).finish()
    }
}

impl Identity {
    pub fn generate() -> Result<Self, String> {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|e| format!("keygen: {e}"))?;
        Self::from_pkcs8(doc.as_ref())
    }

    pub fn from_pkcs8(der: &[u8]) -> Result<Self, String> {
        let key = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der)
            .map_err(|e| format!("bad identity key: {e}"))?;
        let public: PubKey = key
            .public_key()
            .as_ref()
            .try_into()
            .map_err(|_| "bad public key length".to_string())?;
        let cert_der = make_cert(der, &public)?;
        Ok(Self { pkcs8: der.to_vec(), key, public, cert_der })
    }

    /// Loads the key at `path`, creating it (mode 600) on first use.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if let Ok(der) = std::fs::read(path) {
            if let Ok(id) = Self::from_pkcs8(&der) {
                return Ok(id);
            }
            log::warn!("identity at {} unreadable, regenerating", path.display());
        }
        let id = Self::generate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        write_private(path, &id.pkcs8)?;
        Ok(id)
    }

    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    pub fn device_id(&self) -> DeviceId {
        device_id_of(&self.public)
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let sig = self.key.sign(msg);
        let mut out = [0u8; 64];
        out.copy_from_slice(sig.as_ref());
        out
    }

    pub fn cert(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.cert_der.clone())
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.pkcs8.clone()))
    }
}

fn make_cert(pkcs8: &[u8], public: &PubKey) -> Result<Vec<u8>, String> {
    let kp = KeyPair::from_pkcs8_der_and_sign_algo(&PrivatePkcs8KeyDer::from(pkcs8), &PKCS_ED25519)
        .map_err(|e| format!("rcgen key: {e}"))?;
    let name = format!("{}.openu60", device_id_hex(&device_id_of(public)));
    let mut params = CertificateParams::new(vec![name.clone()]).map_err(|e| format!("cert params: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    params.distinguished_name = dn;
    let cert = params.self_signed(&kp).map_err(|e| format!("self-sign: {e}"))?;
    Ok(cert.der().to_vec())
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.write_all(data).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn device_id_of(public: &PubKey) -> DeviceId {
    let digest = ring::digest::digest(&ring::digest::SHA256, public);
    let mut id = [0u8; 8];
    id.copy_from_slice(&digest.as_ref()[..8]);
    id
}

pub fn device_id_hex(id: &DeviceId) -> String {
    hex::encode(id)
}

pub fn parse_device_id(s: &str) -> Option<DeviceId> {
    let bytes = hex::decode(s.trim()).ok()?;
    bytes.try_into().ok()
}

pub fn parse_pubkey(s: &str) -> Option<PubKey> {
    let bytes = hex::decode(s.trim()).ok()?;
    bytes.try_into().ok()
}

pub fn verify(public: &PubKey, msg: &[u8], sig: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public).verify(msg, sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_and_cert() {
        let id = Identity::generate().unwrap();
        let sig = id.sign(b"hello");
        assert!(verify(&id.public, b"hello", &sig));
        assert!(!verify(&id.public, b"hellp", &sig));
        let again = Identity::from_pkcs8(id.pkcs8()).unwrap();
        assert_eq!(again.public, id.public);
        assert_eq!(crate::tls::public_key_of_cert(&id.cert_der), Some(id.public));
        assert_eq!(device_id_hex(&id.device_id()).len(), 16);
    }
}
