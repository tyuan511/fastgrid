//! Cryptographic building blocks for fastgrid's local vault and (later) end-to-end-encrypted sync.
//! No IO, no database, no knowledge of fastgrid's data model: just keys, sealing and key wrapping.
//! Design and threat model: `docs/encryption-and-sync.md`.
//!
//! * Records are sealed with XChaCha20-Poly1305. Its 192-bit random nonce is safe to generate
//!   independently on every device, which is what makes multi-device sync possible (no counters).
//! * Passwords (export files, the future sync password) go through Argon2id.
//! * Keys are zeroized on drop and never printed.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod envelope;
mod kdf;
mod key;
mod password;

pub use envelope::{Envelope, FORMAT_VERSION, open, seal, unwrap_key, wrap_key};
pub use kdf::{KdfParams, derive_kek};
pub use key::{SecretKey, random_bytes};
pub use password::{password_open, password_seal, password_seal_with};

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    /// The system random number generator failed.
    Random,
    /// Not a valid envelope / recovery key / params.
    Format(String),
    /// Written by a newer format version this build cannot read.
    UnsupportedVersion(u8),
    /// Authentication failed: wrong key, wrong context (AAD), or tampered data.
    Decrypt,
    Kdf(String),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::Random => f.write_str("random number generator failed"),
            CryptoError::Format(m) => write!(f, "invalid data: {m}"),
            CryptoError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "encrypted with format v{v}, which this version cannot read"
                )
            }
            CryptoError::Decrypt => {
                f.write_str("decryption failed (wrong key, wrong context or corrupted data)")
            }
            CryptoError::Kdf(m) => write!(f, "key derivation failed: {m}"),
        }
    }
}

impl std::error::Error for CryptoError {}

pub type Result<T> = std::result::Result<T, CryptoError>;
