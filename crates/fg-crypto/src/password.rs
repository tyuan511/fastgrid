//! Password-protected blobs: what encrypted export files are made of, and what will protect the
//! sync key stored on the remote.
//!
//! ```text
//! "FGPW" | version (1) | kdf_len u16 BE | kdf params JSON | sealed envelope
//! ```
//! The KDF parameters travel with the blob (public), so the cost can be raised later and old files
//! still open. Magic, version, parameters and the caller's `context` are all authenticated.

use crate::{CryptoError, KdfParams, Result, derive_kek, envelope, seal};

const MAGIC: &[u8; 4] = b"FGPW";
const VERSION: u8 = 1;

fn aad(kdf_json: &[u8], context: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC);
    v.push(VERSION);
    v.extend_from_slice(kdf_json);
    v.push(0);
    v.extend_from_slice(context);
    v
}

/// Encrypt with a key derived from `password` using the recommended (strong) KDF cost.
pub fn password_seal(password: &str, context: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    password_seal_with(&KdfParams::recommended()?, password, context, plaintext)
}

/// Like [`password_seal`] with explicit KDF parameters (tests use cheap ones).
pub fn password_seal_with(
    params: &KdfParams,
    password: &str,
    context: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let kdf_json = params.to_json().into_bytes();
    let kdf_len = u16::try_from(kdf_json.len())
        .map_err(|_| CryptoError::Format("kdf parameters too large".into()))?;
    let key = derive_kek(password, params)?;
    let sealed = seal(&key, 0, &aad(&kdf_json, context), plaintext)?;

    let mut out = Vec::with_capacity(7 + kdf_json.len() + sealed.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&kdf_len.to_be_bytes());
    out.extend_from_slice(&kdf_json);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Fails with [`CryptoError::Decrypt`] for a wrong password, wrong `context` or tampered data.
pub fn password_open(password: &str, context: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 7 || &blob[..4] != MAGIC {
        return Err(CryptoError::Format(
            "not a password-protected fastgrid file".into(),
        ));
    }
    if blob[4] != VERSION {
        return Err(CryptoError::UnsupportedVersion(blob[4]));
    }
    let kdf_len = usize::from(u16::from_be_bytes([blob[5], blob[6]]));
    let kdf_json = blob
        .get(7..7 + kdf_len)
        .ok_or_else(|| CryptoError::Format("truncated header".into()))?;
    let sealed = &blob[7 + kdf_len..];
    let params = KdfParams::from_json(
        std::str::from_utf8(kdf_json).map_err(|_| CryptoError::Format("bad header".into()))?,
    )?;
    let key = derive_kek(password, &params)?;
    envelope::Envelope::parse(sealed)?.open(&key, &aad(kdf_json, context))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> KdfParams {
        KdfParams::with_cost(8, 1, 1).unwrap()
    }

    #[test]
    fn roundtrip() {
        let blob = password_seal_with(&fast(), "pw", b"export", b"data").unwrap();
        assert_eq!(password_open("pw", b"export", &blob).unwrap(), b"data");
    }

    #[test]
    fn wrong_password_context_or_tampering_fail() {
        let blob = password_seal_with(&fast(), "pw", b"export", b"data").unwrap();
        assert_eq!(
            password_open("nope", b"export", &blob),
            Err(CryptoError::Decrypt)
        );
        assert_eq!(
            password_open("pw", b"sync", &blob),
            Err(CryptoError::Decrypt)
        );
        for i in [8usize, 30, blob.len() - 1] {
            let mut bad = blob.clone();
            bad[i] ^= 1;
            assert!(password_open("pw", b"export", &bad).is_err(), "byte {i}");
        }
    }

    #[test]
    fn rejects_foreign_and_newer_files() {
        assert!(matches!(
            password_open("p", b"c", b"PK\x03\x04 zip file"),
            Err(CryptoError::Format(_))
        ));
        let mut blob = password_seal_with(&fast(), "pw", b"c", b"x").unwrap();
        blob[4] = 9;
        assert_eq!(
            password_open("pw", b"c", &blob),
            Err(CryptoError::UnsupportedVersion(9))
        );
    }

    #[test]
    fn plaintext_is_not_visible_in_the_blob() {
        let blob = password_seal_with(&fast(), "pw", b"c", b"very-recognisable-secret").unwrap();
        assert!(!blob.windows(8).any(|w| w == b"very-rec"));
    }
}
