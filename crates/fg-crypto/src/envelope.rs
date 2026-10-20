//! Sealed-record format (version 1):
//!
//! ```text
//! byte 0      format version (1)
//! byte 1      algorithm (1 = XChaCha20-Poly1305)
//! bytes 2..6  key id, u32 big-endian: which data key sealed this (rotation)
//! bytes 6..30 nonce (24 random bytes)
//! bytes 30..  ciphertext || 16-byte tag
//! ```
//!
//! The header and the caller's context (`aad`) are authenticated. The context binds a ciphertext
//! to *where it lives* (table + record id), so a ciphertext copied onto another row or table fails
//! to open instead of silently being accepted.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::{CryptoError, Result, SecretKey, random_bytes};

pub const FORMAT_VERSION: u8 = 1;
const ALG_XCHACHA20_POLY1305: u8 = 1;
const HEADER_LEN: usize = 6;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Parsed view of a sealed blob (no decryption).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope<'a> {
    pub key_id: u32,
    header: &'a [u8],
    nonce: &'a [u8],
    ciphertext: &'a [u8],
}

impl<'a> Envelope<'a> {
    pub fn parse(blob: &'a [u8]) -> Result<Self> {
        if blob.len() < HEADER_LEN + NONCE_LEN + TAG_LEN {
            return Err(CryptoError::Format("sealed blob is too short".into()));
        }
        if blob[0] != FORMAT_VERSION {
            return Err(CryptoError::UnsupportedVersion(blob[0]));
        }
        if blob[1] != ALG_XCHACHA20_POLY1305 {
            return Err(CryptoError::Format(format!(
                "unknown algorithm {}",
                blob[1]
            )));
        }
        let key_id = u32::from_be_bytes([blob[2], blob[3], blob[4], blob[5]]);
        Ok(Self {
            key_id,
            header: &blob[..HEADER_LEN],
            nonce: &blob[HEADER_LEN..HEADER_LEN + NONCE_LEN],
            ciphertext: &blob[HEADER_LEN + NONCE_LEN..],
        })
    }

    pub fn open(&self, key: &SecretKey, aad: &[u8]) -> Result<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new(&Key::from(*key.as_bytes()));
        let nonce_bytes: [u8; NONCE_LEN] = self
            .nonce
            .try_into()
            .map_err(|_| CryptoError::Format("bad nonce".into()))?;
        let full_aad = full_aad(self.header, aad);
        cipher
            .decrypt(
                &XNonce::from(nonce_bytes),
                Payload {
                    msg: self.ciphertext,
                    aad: &full_aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt)
    }
}

fn full_aad(header: &[u8], aad: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(header.len() + aad.len());
    v.extend_from_slice(header);
    v.extend_from_slice(aad);
    v
}

/// Encrypt `plaintext` under `key`, labelled with `key_id` and bound to `aad`.
pub fn seal(key: &SecretKey, key_id: u32, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut header = [0u8; HEADER_LEN];
    header[0] = FORMAT_VERSION;
    header[1] = ALG_XCHACHA20_POLY1305;
    header[2..6].copy_from_slice(&key_id.to_be_bytes());
    let nonce_bytes = random_bytes::<NONCE_LEN>()?;

    let cipher = XChaCha20Poly1305::new(&Key::from(*key.as_bytes()));
    let full_aad = full_aad(&header, aad);
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &full_aad,
            },
        )
        .map_err(|_| CryptoError::Format("plaintext too long".into()))?;

    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a blob produced by [`seal`]. Which key to use is up to the caller
/// (see [`Envelope::parse`] to read the `key_id` first).
pub fn open(key: &SecretKey, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    Envelope::parse(blob)?.open(key, aad)
}

/// Encrypt one key under another (key wrapping). `context` identifies what is being wrapped.
pub fn wrap_key(kek: &SecretKey, dek: &SecretKey, key_id: u32, context: &[u8]) -> Result<Vec<u8>> {
    seal(kek, key_id, &wrap_aad(context), dek.as_bytes())
}

pub fn unwrap_key(kek: &SecretKey, context: &[u8], wrapped: &[u8]) -> Result<SecretKey> {
    let bytes = open(kek, &wrap_aad(context), wrapped)?;
    SecretKey::try_from_slice(&bytes)
}

fn wrap_aad(context: &[u8]) -> Vec<u8> {
    let mut v = b"fg-wrap|".to_vec();
    v.extend_from_slice(context);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretKey {
        SecretKey::generate().unwrap()
    }

    #[test]
    fn roundtrip_and_key_id() {
        let k = key();
        let blob = seal(&k, 7, b"connection|abc", b"hello").unwrap();
        assert_eq!(Envelope::parse(&blob).unwrap().key_id, 7);
        assert_eq!(open(&k, b"connection|abc", &blob).unwrap(), b"hello");
    }

    #[test]
    fn nonces_differ_so_equal_plaintexts_seal_differently() {
        let k = key();
        assert_ne!(
            seal(&k, 1, b"", b"x").unwrap(),
            seal(&k, 1, b"", b"x").unwrap()
        );
    }

    #[test]
    fn wrong_key_wrong_context_and_tampering_all_fail() {
        let k = key();
        let blob = seal(&k, 1, b"ctx", b"secret").unwrap();
        assert_eq!(open(&key(), b"ctx", &blob), Err(CryptoError::Decrypt));
        assert_eq!(open(&k, b"other-row", &blob), Err(CryptoError::Decrypt));
        for i in [0usize, 2, 10, blob.len() - 1] {
            let mut bad = blob.clone();
            bad[i] ^= 1;
            assert!(
                open(&k, b"ctx", &bad).is_err(),
                "flipping byte {i} must be detected"
            );
        }
    }

    #[test]
    fn header_is_authenticated() {
        // Relabelling the key id must not go unnoticed.
        let k = key();
        let mut blob = seal(&k, 1, b"c", b"x").unwrap();
        blob[5] = 2;
        assert_eq!(open(&k, b"c", &blob), Err(CryptoError::Decrypt));
    }

    #[test]
    fn newer_format_versions_are_reported_not_misread() {
        let k = key();
        let mut blob = seal(&k, 1, b"c", b"x").unwrap();
        blob[0] = 9;
        assert_eq!(
            open(&k, b"c", &blob),
            Err(CryptoError::UnsupportedVersion(9))
        );
        assert!(matches!(
            Envelope::parse(&[1, 1, 0]),
            Err(CryptoError::Format(_))
        ));
    }

    #[test]
    fn key_wrapping_roundtrip() {
        let (kek, dek) = (key(), key());
        let wrapped = wrap_key(&kek, &dek, 1, b"wrap-1").unwrap();
        assert_eq!(unwrap_key(&kek, b"wrap-1", &wrapped).unwrap(), dek);
        assert!(unwrap_key(&kek, b"wrap-2", &wrapped).is_err());
        assert!(unwrap_key(&key(), b"wrap-1", &wrapped).is_err());
    }
}
