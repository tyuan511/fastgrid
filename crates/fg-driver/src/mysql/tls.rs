//! TLS for MySQL / MariaDB, mapping `fg-core`'s `TlsConfig` onto
//! `mysql_async`'s `SslOpts`.
//!
//! Mode mapping (mirrors the PostgreSQL driver's semantics):
//!
//! * `Disable` — no `SslOpts`; a plain connection. The only mode allowed
//!   with a Unix socket.
//! * `Prefer` — TLS without verification, falling back to plain when the
//!   server does not speak TLS. This matches PostgreSQL's `Prefer`: the
//!   connection is encrypted when the server offers it, verified never.
//! * `Require` — TLS without verification; the server must offer TLS.
//! * `VerifyCa` — the chain must verify against the configured CA (or the
//!   built-in roots when no CA file is given), but the name is not checked.
//! * `VerifyFull` — chain and name both verify; `server_name` overrides the
//!   name when connecting through a tunnel to 127.0.0.1.
//!
//! A note on the crypto provider: `mysql_async`'s rustls path builds its
//! `ClientConfig` from the process default provider, exactly like
//! `tokio-postgres` did — and panics when none is installed (see the
//! PostgreSQL driver's `factory_for`). `install_provider` runs before any
//! connect attempt so the first real connection cannot panic.

use fg_core::{Error, Result, TlsConfig, TlsMode};
use mysql_async::{ClientIdentity, SslOpts};
use std::path::PathBuf;

/// Install the process default rustls crypto provider (`ring`, enabled by
/// this crate). Already-installed is fine — the PostgreSQL driver installs
/// the same provider when both engines are built in.
pub fn install_provider() {
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
}

/// Build the `SslOpts` for a mode, or `None` for a plain connection.
/// `prefer` is reported separately so the caller can fall back to plain.
pub fn ssl_opts(tls: Option<&TlsConfig>) -> Result<(Option<SslOpts>, bool)> {
    let Some(tls) = tls else {
        // No TLS section at all means "prefer": the M4 connection form
        // stores `None` when the user leaves TLS at its default.
        return Ok((Some(unverified_opts(None)), true));
    };
    Ok(match tls.mode {
        TlsMode::Disable => (None, false),
        TlsMode::Prefer => (Some(unverified_opts(Some(tls))), true),
        TlsMode::Require => (Some(unverified_opts(Some(tls))), false),
        TlsMode::VerifyCa => (Some(verified_opts(tls, true)?), false),
        TlsMode::VerifyFull => (Some(verified_opts(tls, false)?), false),
        TlsMode::Unknown => {
            return Err(Error::InvalidInput(
                "unknown TLS mode in connection config".into(),
            ));
        }
        // `TlsMode` is non-exhaustive: future modes fail loudly.
        _ => {
            return Err(Error::InvalidInput(
                "unsupported TLS mode in connection config".into(),
            ));
        }
    })
}

/// TLS without any verification (Prefer / Require).
fn unverified_opts(tls: Option<&TlsConfig>) -> SslOpts {
    let mut opts = SslOpts::default().with_danger_accept_invalid_certs(true);
    if let Some(tls) = tls {
        opts = with_client_identity(opts, tls);
    }
    opts
}

/// TLS with chain verification; `skip_name` selects VerifyCa over VerifyFull.
fn verified_opts(tls: &TlsConfig, skip_name: bool) -> Result<SslOpts> {
    let mut opts = SslOpts::default();
    if let Some(ca_path) = &tls.ca_cert_path {
        if ca_path.is_empty() {
            return Err(Error::InvalidInput("TLS CA path is empty".into()));
        }
        // `PathOrBuf` is not publicly re-exported, but `.into()` infers
        // it from the parameter type.
        opts = opts
            .with_root_certs(vec![PathBuf::from(ca_path).into()])
            .with_disable_built_in_roots(true);
    }
    if skip_name {
        opts = opts.with_danger_skip_domain_validation(true);
    } else if let Some(name) = &tls.server_name
        && !name.is_empty()
    {
        opts = opts.with_danger_tls_hostname_override(Some(name.clone()));
    }
    opts = with_client_identity(opts, tls);
    Ok(opts)
}

fn with_client_identity(opts: SslOpts, tls: &TlsConfig) -> SslOpts {
    match (&tls.client_cert_path, &tls.client_key_path) {
        (Some(cert), Some(key)) if !cert.is_empty() && !key.is_empty() => opts
            .with_client_identity(Some(ClientIdentity::new(
                PathBuf::from(cert).into(),
                PathBuf::from(key).into(),
            ))),
        _ => opts,
    }
}
