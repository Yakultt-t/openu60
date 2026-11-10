//! SMS field encryption used by the international (STD_PL) firmware.
//!
//! The stock web UI generates a random 32-byte key, RSA-encrypts its hex form with the
//! certificate from `zwrt_web web_crt_get`, hands it to the firmware through
//! `zwrt_web web_http_enstr_set`, and from then on `zwrt_wms` returns `number`/`content`
//! as base64(iv[12] || tag[16] || ciphertext) under AES-256-GCM with that key, and expects
//! the same encoding on `zte_libwms_send_sms`. The key is global, so a browser login
//! replaces it; decrypt failures therefore trigger a re-key.

use std::sync::{Mutex, OnceLock};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde_json::{json, Value};
use x509_cert::der::{Decode, DecodePem, Encode};
use x509_cert::Certificate;

use crate::ubus;

static KEY: OnceLock<Mutex<Option<[u8; 32]>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<[u8; 32]>> {
    KEY.get_or_init(|| Mutex::new(None))
}

fn current_key() -> Option<[u8; 32]> {
    *slot().lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) fn set_key_for_tests(key: [u8; 32]) {
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(key);
}

/// The firmware returns PEM without line breaks ("-----BEGIN PUBLIC KEY-----MIIB...-----END
/// PUBLIC KEY-----"), which strict PEM parsers reject, so pull the base64 body out by hand.
fn pem_body(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = pem[start..].find(&end)? + start;
    let body: String = pem[start..stop].chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(body).ok()
}

fn public_key_from_pem(pem: &str) -> Result<RsaPublicKey, String> {
    if let Some(der) = pem_body(pem, "PUBLIC KEY") {
        return RsaPublicKey::from_public_key_der(&der).map_err(|e| format!("spki der: {e}"));
    }
    if let Some(der) = pem_body(pem, "RSA PUBLIC KEY") {
        return RsaPublicKey::from_pkcs1_der(&der).map_err(|e| format!("pkcs1 der: {e}"));
    }
    if let Some(der) = pem_body(pem, "CERTIFICATE") {
        let cert = Certificate::from_der(&der).map_err(|e| format!("x509 parse: {e}"))?;
        let spki = cert
            .tbs_certificate
            .subject_public_key_info
            .to_der()
            .map_err(|e| format!("spki der: {e}"))?;
        return RsaPublicKey::from_public_key_der(&spki).map_err(|e| format!("spki rsa: {e}"));
    }
    if pem.contains("BEGIN CERTIFICATE") {
        let cert = Certificate::from_pem(pem.as_bytes()).map_err(|e| format!("x509 parse: {e}"))?;
        let der = cert
            .tbs_certificate
            .subject_public_key_info
            .to_der()
            .map_err(|e| format!("spki der: {e}"))?;
        return RsaPublicKey::from_public_key_der(&der).map_err(|e| format!("spki rsa: {e}"));
    }
    if pem.contains("BEGIN RSA PUBLIC KEY") {
        return RsaPublicKey::from_pkcs1_pem(pem).map_err(|e| format!("pkcs1: {e}"));
    }
    RsaPublicKey::from_public_key_pem(pem).map_err(|e| format!("spki pem: {e}"))
}

/// Generate a fresh key and register it with the firmware. Idempotent; cheap enough to
/// call before every SMS operation that must not race a browser login.
pub fn rekey() -> Result<(), String> {
    let crt = ubus::call("zwrt_web", "web_crt_get", Some("{}"))?;
    let pem = crt["result"]
        .as_str()
        .or_else(|| crt["crt"].as_str())
        .ok_or_else(|| "web_crt_get returned no certificate".to_string())?
        .to_string();
    let public = public_key_from_pem(&pem)?;

    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    let key_hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let encrypted = public
        .encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, key_hex.as_bytes())
        .map_err(|e| format!("rsa encrypt: {e}"))?;
    let params = json!({"web_enstr": B64.encode(encrypted)}).to_string();
    let result = ubus::call("zwrt_web", "web_http_enstr_set", Some(&params))?;
    if result["result"].as_str().map(|r| r != "success" && r != "0").unwrap_or(false) {
        return Err(format!("web_http_enstr_set refused: {result}"));
    }
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(key);
    eprintln!("[sms-crypto] session key registered");
    Ok(())
}

fn ensure_key() -> Result<[u8; 32], String> {
    if let Some(k) = current_key() {
        return Ok(k);
    }
    rekey()?;
    current_key().ok_or_else(|| "no key after rekey".to_string())
}

/// Decrypt one field. `Ok(None)` means the value was not encrypted (plain text passthrough).
pub fn decrypt_field(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    // Firmware without field encryption hands out plain UCS-2 hex, which is also valid
    // base64; real ciphertext is never all hex digits, so pass it through untouched.
    if value.len() % 4 == 0 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let raw = match B64.decode(value) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    if raw.len() <= 28 {
        return Ok(None);
    }
    let key = ensure_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("aes key: {e}"))?;
    let (iv, rest) = raw.split_at(12);
    let (tag, ct) = rest.split_at(16);
    let mut msg = Vec::with_capacity(ct.len() + 16);
    msg.extend_from_slice(ct);
    msg.extend_from_slice(tag);
    let plain = cipher
        .decrypt(Nonce::from_slice(iv), Payload { msg: &msg, aad: b"" })
        .map_err(|_| "gcm tag mismatch".to_string())?;
    Ok(Some(String::from_utf8_lossy(&plain).into_owned()))
}

pub fn encrypt_field(value: &str) -> Result<String, String> {
    let key = ensure_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("aes key: {e}"))?;
    let mut iv = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut iv);
    let out = cipher
        .encrypt(Nonce::from_slice(&iv), value.as_bytes())
        .map_err(|e| format!("gcm encrypt: {e}"))?;
    let (ct, tag) = out.split_at(out.len() - 16);
    let mut packed = Vec::with_capacity(28 + ct.len());
    packed.extend_from_slice(&iv);
    packed.extend_from_slice(tag);
    packed.extend_from_slice(ct);
    Ok(B64.encode(packed))
}

/// Decrypted `number` fields arrive as UCS-2 hex ("002B0038..." = "+8..."); turn them back into
/// plain text so clients can group threads and dial. Content stays hex (clients decode it).
fn ucs2_hex_to_text(value: &str) -> Option<String> {
    if value.is_empty() || value.len() % 4 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let units: Option<Vec<u16>> = (0..value.len())
        .step_by(4)
        .map(|i| u16::from_str_radix(&value[i..i + 4], 16).ok())
        .collect();
    let text = String::from_utf16_lossy(&units?);
    // A real UCS-2 number decodes to digits/+/*/# and letters (alphanumeric sender IDs).
    if text.chars().all(|c| c.is_alphanumeric() || "+*#-. ".contains(c)) { Some(text) } else { None }
}

/// Decrypt the `number` and `content` of every message in a `zte_libwms_get_sms_data`
/// reply. On a tag mismatch (browser replaced the key) the caller should re-fetch after
/// `rekey()`; this returns `Err` in that case so the caller can do exactly that.
pub fn decrypt_messages(data: &mut Value) -> Result<(), String> {
    let Some(list) = data["messages"].as_array_mut() else { return Ok(()) };
    for item in list.iter_mut() {
        for field in ["number", "content"] {
            let Some(raw) = item[field].as_str().map(|s| s.to_string()) else { continue };
            if let Some(plain) = decrypt_field(&raw)? {
                let plain = if field == "number" {
                    ucs2_hex_to_text(&plain).unwrap_or(plain)
                } else {
                    plain
                };
                item[field] = Value::String(plain);
            }
        }
    }
    Ok(())
}

/// Encrypt the outgoing `number` and `message_body` of a send request.
pub fn encrypt_send_request(body: &mut Value) -> Result<(), String> {
    for field in ["number", "message_body"] {
        let Some(raw) = body[field].as_str().map(|s| s.to_string()) else { continue };
        body[field] = Value::String(encrypt_field(&raw)?);
    }
    Ok(())
}
