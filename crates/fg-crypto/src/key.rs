use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{CryptoError, Result};

/// 256-bit symmetric key. Zeroized on drop; `Debug` never shows the bytes.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    pub fn generate() -> Result<Self> {
        Ok(Self(random_bytes::<32>()?))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn try_from_slice(bytes: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::Format("key must be 32 bytes".into()))?;
        Ok(Self(arr))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Clone for SecretKey {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl PartialEq for SecretKey {
    /// Constant-time comparison.
    fn eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl Eq for SecretKey {}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|_| CryptoError::Random)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_random_and_debug_is_redacted() {
        let a = SecretKey::generate().unwrap();
        let b = SecretKey::generate().unwrap();
        assert_ne!(a, b);
        assert_eq!(a, a.clone());
        assert_eq!(format!("{a:?}"), "SecretKey(..)");
    }

    #[test]
    fn slice_length_is_checked() {
        assert!(SecretKey::try_from_slice(&[0; 31]).is_err());
        assert!(SecretKey::try_from_slice(&[0; 32]).is_ok());
    }
}
