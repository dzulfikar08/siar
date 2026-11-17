//! AES-256-GCM, same blob layout as the TS vault: version(1) | iv(12) | tag(16) | ciphertext.
//! The key is 32 bytes given as 64 hex chars or base64.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde_json::Value;
use std::sync::OnceLock;

use crate::util::{bail, random_bytes, Error, Result};

const VERSION: u8 = 1;

pub fn parse_key(raw: &str) -> Result<[u8; 32]> {
    let is_hex = raw.len() == 64 && raw.bytes().all(|c| c.is_ascii_hexdigit());
    let buf = if is_hex {
        (0..32)
            .map(|i| u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect::<Vec<u8>>()
    } else {
        // Node's base64 decoder is lenient: it ignores padding and whitespace, and accepts base64url.
        let cleaned: String = raw
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '=')
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                c => c,
            })
            .collect();
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(cleaned).unwrap_or_default()
    };
    match <[u8; 32]>::try_from(buf.as_slice()) {
        Ok(k) => Ok(k),
        Err(_) => bail!("SIAR_VAULT_KEY must be 32 bytes (64 hex chars or base64)"),
    }
}

fn key() -> Result<&'static [u8; 32]> {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    if let Some(k) = KEY.get() {
        return Ok(k);
    }
    let raw = match &crate::config::config().vault_key {
        Some(r) => r.clone(),
        None => bail!("SIAR_VAULT_KEY is not set. Generate one with: siar keygen"),
    };
    let k = parse_key(&raw)?;
    Ok(KEY.get_or_init(|| k))
}

pub fn seal_with(key: &[u8; 32], value: &Value) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| Error::Msg(e.to_string()))?;
    let iv = random_bytes(12);
    let pt = serde_json::to_vec(value)?;
    let ct_tag = cipher
        .encrypt(Nonce::from_slice(&iv), pt.as_slice())
        .map_err(|_| Error::Msg("Vault encryption failed".into()))?;
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    let mut out = Vec::with_capacity(1 + 12 + 16 + ct.len());
    out.push(VERSION);
    out.extend_from_slice(&iv);
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    Ok(out)
}

pub fn open_with(key: &[u8; 32], blob: &[u8]) -> Result<Value> {
    if blob.first() != Some(&VERSION) {
        bail!("Unknown vault blob version");
    }
    if blob.len() < 29 {
        bail!("Unsupported state or unable to authenticate data");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| Error::Msg(e.to_string()))?;
    let mut ct_tag = Vec::with_capacity(blob.len() - 13);
    ct_tag.extend_from_slice(&blob[29..]);
    ct_tag.extend_from_slice(&blob[13..29]);
    let pt = cipher
        .decrypt(Nonce::from_slice(&blob[1..13]), ct_tag.as_slice())
        .map_err(|_| Error::Msg("Unsupported state or unable to authenticate data".into()))?;
    Ok(serde_json::from_slice(&pt)?)
}

pub fn seal(value: &Value) -> Result<Vec<u8>> {
    seal_with(key()?, value)
}

pub fn open(blob: &[u8]) -> Result<Value> {
    open_with(key()?, blob)
}

pub fn generate_key() -> String {
    base64::engine::general_purpose::STANDARD.encode(random_bytes(32))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("meta/vault-fixture.json");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect()
    }

    #[test]
    fn opens_a_blob_sealed_by_the_ts_vault() {
        let f: Value = serde_json::from_str(FIXTURE).unwrap();
        let blob = unhex(f["blob_hex"].as_str().unwrap());
        for k in ["key_base64", "key_hex"] {
            let key = parse_key(f[k].as_str().unwrap()).unwrap();
            assert_eq!(open_with(&key, &blob).unwrap(), f["plain"]);
        }
    }

    #[test]
    fn round_trips_and_detects_tampering() {
        let key = [7u8; 32];
        let v = serde_json::json!({"access_token": "abc", "n": 1});
        let blob = seal_with(&key, &v).unwrap();
        assert_eq!(blob[0], 1);
        assert_eq!(open_with(&key, &blob).unwrap(), v);
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(open_with(&key, &bad).is_err());
        assert!(parse_key("too short").is_err());
    }
}
