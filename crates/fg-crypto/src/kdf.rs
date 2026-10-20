use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

use crate::{CryptoError, Result, SecretKey, random_bytes};

/// Argon2id parameters. Stored next to every passphrase-wrapped key (public, not secret), so the
/// cost can be raised later without breaking existing wraps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Always `"argon2id"` today; a different value means a newer format.
    pub algorithm: String,
    /// Memory in KiB.
    pub m_cost: u32,
    pub t_cost: u32,
    pub parallelism: u32,
    #[serde(with = "hex_bytes")]
    pub salt: Vec<u8>,
}

impl KdfParams {
    /// Interactive-login strength: 64 MiB, 3 passes. Random salt.
    pub fn recommended() -> Result<Self> {
        Self::with_cost(64 * 1024, 3, 1)
    }

    pub fn with_cost(m_cost: u32, t_cost: u32, parallelism: u32) -> Result<Self> {
        Ok(Self {
            algorithm: "argon2id".into(),
            m_cost,
            t_cost,
            parallelism,
            salt: random_bytes::<16>()?.to_vec(),
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| CryptoError::Format(e.to_string()))
    }
}

/// Derive a key-encryption key from a passphrase.
pub fn derive_kek(passphrase: &str, params: &KdfParams) -> Result<SecretKey> {
    if params.algorithm != "argon2id" {
        return Err(CryptoError::Kdf(format!(
            "unknown algorithm {}",
            params.algorithm
        )));
    }
    let argon_params = Params::new(params.m_cost, params.t_cost, params.parallelism, Some(32))
        .map_err(|e| CryptoError::Kdf(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), &params.salt, &mut out)
        .map_err(|e| CryptoError::Kdf(e.to_string()))?;
    let key = SecretKey::from_bytes(out);
    zeroize::Zeroize::zeroize(&mut out);
    Ok(key)
}

mod hex_bytes {
    use data_encoding::HEXLOWER;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&HEXLOWER.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        HEXLOWER
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters so tests stay fast; production uses `recommended()`.
    fn fast() -> KdfParams {
        KdfParams::with_cost(8, 1, 1).unwrap()
    }

    #[test]
    fn same_passphrase_and_salt_give_the_same_key() {
        let p = fast();
        assert_eq!(
            derive_kek("hunter2", &p).unwrap(),
            derive_kek("hunter2", &p).unwrap()
        );
        assert_ne!(
            derive_kek("hunter2", &p).unwrap(),
            derive_kek("hunter3", &p).unwrap()
        );
        assert_ne!(
            derive_kek("hunter2", &p).unwrap(),
            derive_kek("hunter2", &fast()).unwrap()
        );
    }

    #[test]
    fn params_roundtrip_through_json() {
        let p = fast();
        assert_eq!(KdfParams::from_json(&p.to_json()).unwrap(), p);
    }

    #[test]
    fn unknown_algorithms_are_rejected() {
        let mut p = fast();
        p.algorithm = "scrypt2030".into();
        assert!(matches!(derive_kek("x", &p), Err(CryptoError::Kdf(_))));
    }

    #[test]
    fn recommended_cost_is_what_we_document() {
        let p = KdfParams::recommended().unwrap();
        assert_eq!(
            (p.m_cost, p.t_cost, p.parallelism, p.salt.len()),
            (65536, 3, 1, 16)
        );
    }
}
