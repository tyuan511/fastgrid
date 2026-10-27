//! TLS policy for SQL Server.
//!
//! SQL Server always performs a TLS handshake during login, even when the
//! client asks for no encryption — the login packet itself is protected. That
//! means certificate validation failures surface regardless of the requested
//! mode, and the server's auto-generated self-signed certificate is rejected by
//! rustls by default. So `Prefer` and `Require` deliberately skip validation
//! (matching the MySQL driver's behaviour) and only `VerifyCa` / `VerifyFull`
//! validate a chain.

use std::path::Path;

use fg_core::{Error, Result, TlsConfig, TlsMode};
use tiberius::{Config, EncryptionLevel};

/// Install the ring provider once per process. tiberius falls back to
/// aws-lc-rs when no provider is installed; the rest of the crate uses ring, so
/// pinning one provider keeps every engine on the same crypto backend.
pub fn install_provider() {
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
}

/// Apply the connection's TLS settings to a tiberius config.
pub fn apply(config: &mut Config, tls: Option<&TlsConfig>) -> Result<()> {
    let Some(tls) = tls else {
        // No TLS configuration at all: encrypt if the server offers it.
        config.encryption(EncryptionLevel::On);
        config.trust_cert();
        return Ok(());
    };
    match tls.mode {
        TlsMode::Disable => {
            config.encryption(EncryptionLevel::NotSupported);
        }
        TlsMode::Prefer => {
            config.encryption(EncryptionLevel::On);
            config.trust_cert();
        }
        TlsMode::Require => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
        TlsMode::VerifyCa => {
            config.encryption(EncryptionLevel::Required);
            trust_ca(config, tls)?;
        }
        TlsMode::VerifyFull => {
            config.encryption(EncryptionLevel::Required);
            trust_ca(config, tls)?;
            let host = tls.server_name.as_deref().ok_or_else(|| {
                Error::InvalidInput(
                    "VerifyFull needs a server name to validate the certificate against".into(),
                )
            })?;
            config.hostname_in_certificate(host);
        }
        // `TlsMode` is `#[non_exhaustive]`: an unknown value means a newer
        // build wrote this connection, and guessing an encryption level is
        // worse than refusing.
        _ => {
            return Err(Error::InvalidInput(
                "unknown TLS mode; pick one the connection form offers".into(),
            ));
        }
    }
    if let (Some(cert), Some(key)) = (&tls.client_cert_path, &tls.client_key_path) {
        config.client_certificate(Path::new(cert), Path::new(key));
    } else if tls.client_cert_path.is_some() || tls.client_key_path.is_some() {
        return Err(Error::InvalidInput(
            "a client certificate needs both a certificate and a key file".into(),
        ));
    }
    Ok(())
}

fn trust_ca(config: &mut Config, tls: &TlsConfig) -> Result<()> {
    match &tls.ca_cert_path {
        // Validating against the OS trust store is the default; a CA file is
        // layered on top of it.
        Some(path) => {
            if !Path::new(path).is_file() {
                return Err(Error::InvalidInput(format!(
                    "CA certificate `{path}` does not exist"
                )));
            }
            config.trust_cert_ca(path);
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tls(mode: TlsMode) -> TlsConfig {
        TlsConfig {
            mode,
            ..TlsConfig::default()
        }
    }

    #[test]
    fn every_supported_mode_is_accepted() {
        // `Config` exposes no getter for the encryption level, so the modes are
        // checked by the fact that applying them succeeds and only the
        // validating modes demand extra inputs.
        for mode in [TlsMode::Disable, TlsMode::Prefer, TlsMode::Require] {
            let mut config = Config::new();
            apply(&mut config, Some(&tls(mode))).unwrap();
        }
        let mut config = Config::new();
        apply(&mut config, None).unwrap();
    }

    #[test]
    fn unknown_modes_are_refused() {
        let mut config = Config::new();
        let err = apply(&mut config, Some(&tls(TlsMode::Unknown))).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn a_half_configured_client_certificate_is_refused() {
        let mut config = Config::new();
        let mut tls = tls(TlsMode::Require);
        tls.client_cert_path = Some("/tmp/cert.pem".into());
        let err = apply(&mut config, Some(&tls)).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn verify_full_needs_a_server_name() {
        let mut config = Config::new();
        let err = apply(&mut config, Some(&tls(TlsMode::VerifyFull))).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn a_missing_ca_file_is_reported_before_connecting() {
        let mut config = Config::new();
        let mut tls = tls(TlsMode::VerifyCa);
        tls.ca_cert_path = Some("/tmp/fastgrid-no-such-ca.pem".into());
        let err = apply(&mut config, Some(&tls)).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }
}
