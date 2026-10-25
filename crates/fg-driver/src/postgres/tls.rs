//! rustls connector for PostgreSQL, driven by `ConnectionConfig::tls`.
//!
//! PostgreSQL's `sslmode` values map onto rustls verifiers:
//! * `Disable` — no TLS at all.
//! * `Prefer` — try TLS, fall back to plaintext when the server refuses it.
//! * `Require` — TLS with no certificate verification (encryption only).
//! * `VerifyCa` — the chain must lead to a trusted root; the host name is *not*
//!   checked, which is what distinguishes it from `VerifyFull`. rustls's stock verifier
//!   always checks the name, so this mode uses a custom verifier that runs only the
//!   chain check (`verify_server_cert_signed_by_trust_anchor`) and delegates signature
//!   verification to the stock verifier.
//! * `VerifyFull` — stock rustls verification: chain plus host name.
//!
//! Client certificates and an extra CA file come from PEM files when configured.
//! SCRAM channel binding (`tls-server-end-point`) is not offered: the stream reports
//! no binding data, so a server demanding `SCRAM-SHA-256-PLUS` refuses; plain
//! `SCRAM-SHA-256` and password auth work.

use std::fs;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use fg_core::{Error, Result, TlsConfig, TlsMode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, OtherError,
    RootCertStore, SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::Socket;
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use tokio_rustls::TlsConnector;
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};

/// Factory handed to tokio-postgres: one connector per connection, with the domain
/// tokio-postgres passes in (the host) used for SNI and verification unless the
/// connection overrides it with `tls.server_name`.
#[derive(Clone)]
pub struct RustlsFactory {
    config: Arc<ClientConfig>,
    server_name_override: Option<String>,
}

impl RustlsFactory {
    fn server_name(&self, domain: &str) -> std::result::Result<ServerName<'static>, TlsError> {
        let name = self.server_name_override.as_deref().unwrap_or(domain);
        ServerName::try_from(name.to_owned())
            .map_err(|_| TlsError::General("bad server name".into()))
    }
}

impl MakeTlsConnect<Socket> for RustlsFactory {
    type Stream = RustlsStream;
    type TlsConnect = RustlsConnector;
    type Error = TlsError;

    fn make_tls_connect(&mut self, domain: &str) -> std::result::Result<RustlsConnector, TlsError> {
        Ok(RustlsConnector {
            config: self.config.clone(),
            server_name: self.server_name(domain)?,
        })
    }
}

/// One TLS handshake over an already-open socket.
pub struct RustlsConnector {
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
}

impl TlsConnect<Socket> for RustlsConnector {
    type Stream = RustlsStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream>> + Send>>;

    fn connect(self, stream: Socket) -> Self::Future {
        Box::pin(async move {
            let connector = TlsConnector::from(self.config);
            let tls = connector.connect(self.server_name, stream).await?;
            Ok(RustlsStream(tls))
        })
    }
}

/// tokio-rustls stream adapted to tokio-postgres's `TlsStream`. Channel binding is
/// deliberately `none` (see the module docs).
pub struct RustlsStream(tokio_rustls::client::TlsStream<Socket>);

impl AsyncRead for RustlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for RustlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl TlsStream for RustlsStream {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

/// Build the rustls configuration for a connection's TLS settings, or `None` for
/// `Disable` (and for `tls: None`, which defaults to `Prefer` per `TlsMode::default`).
pub fn build_factory(tls: Option<&TlsConfig>) -> Result<Option<RustlsFactory>> {
    let Some(tls) = tls else {
        return Ok(Some(factory_for(&TlsConfig::default())?));
    };
    if tls.mode == TlsMode::Disable {
        return Ok(None);
    }
    Ok(Some(factory_for(tls)?))
}

/// Trust roots as both a rustls store (stock verifier) and a DER list (the chain-only
/// verifier, which builds its anchors from DERs directly).
fn load_roots(tls: &TlsConfig) -> Result<(RootCertStore, Vec<CertificateDer<'static>>)> {
    let mut roots = RootCertStore::empty();
    let mut ders: Vec<CertificateDer<'static>> = Vec::new();
    if let Some(path) = &tls.ca_cert_path {
        let pem = fs::read(path).map_err(|e| Error::Connection(format!("read CA file: {e}")))?;
        let mut reader = std::io::Cursor::new(pem);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::Connection(format!("parse CA file: {e}")))?;
        ders.extend(certs.iter().cloned());
        roots.add_parsable_certificates(certs);
    }
    // System roots: `VerifyCa`/`VerifyFull` against a private CA file alone would break
    // public servers, and a missing CA file must not silently mean "no roots".
    let native = rustls_native_certs::load_native_certs();
    if native.certs.is_empty() {
        return Err(Error::Connection(
            "no system TLS roots could be loaded".into(),
        ));
    }
    for cert in native.certs {
        let _ = roots.add(cert.clone());
        ders.push(cert);
    }
    Ok((roots, ders))
}

fn load_client_cert(
    tls: &TlsConfig,
) -> Result<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>> {
    let (Some(cert_path), Some(key_path)) = (&tls.client_cert_path, &tls.client_key_path) else {
        return Ok(None);
    };
    let cert_pem =
        fs::read(cert_path).map_err(|e| Error::Connection(format!("read client cert: {e}")))?;
    let mut reader = std::io::Cursor::new(cert_pem);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Connection(format!("parse client cert: {e}")))?;
    let key_pem =
        fs::read(key_path).map_err(|e| Error::Connection(format!("read client key: {e}")))?;
    let mut reader = std::io::Cursor::new(key_pem);
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| Error::Connection(format!("parse client key: {e}")))?
        .ok_or_else(|| Error::Connection("client key file has no private key".into()))?;
    Ok(Some((certs, key)))
}

fn factory_for(tls: &TlsConfig) -> Result<RustlsFactory> {
    // Install a crypto provider first: both WebPkiServerVerifier::builder()
    // and ClientConfig::builder() below read the process default provider and
    // panic when none is installed, instead of returning an error.
    default_provider()?;
    let (roots, ders) = load_roots(tls)?;
    let roots = Arc::new(roots);
    let client_cert = load_client_cert(tls)?;
    let verifier: Arc<dyn ServerCertVerifier> = match tls.mode {
        TlsMode::VerifyCa => Arc::new(ChainOnlyVerifier {
            roots_der: ders,
            inner: rustls::client::WebPkiServerVerifier::builder(roots.clone())
                .build()
                .map_err(|e| Error::Connection(format!("build TLS verifier: {e}")))?,
        }),
        TlsMode::Require => Arc::new(NoVerify),
        // VerifyFull and Prefer share the stock verifier; Prefer's fallback is handled
        // by the caller retrying without TLS.
        _ => rustls::client::WebPkiServerVerifier::builder(roots.clone())
            .build()
            .map_err(|e| Error::Connection(format!("build TLS verifier: {e}")))?,
    };
    let builder = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match client_cert {
        Some((certs, key)) => builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| Error::Connection(format!("client cert rejected: {e}")))?,
        None => builder.with_no_client_auth(),
    };
    Ok(RustlsFactory {
        config: Arc::new(config),
        server_name_override: tls.server_name.clone(),
    })
}

/// The rustls crypto provider: the app may have installed one already (in which case
/// this is a no-op); otherwise install ring's. Without a provider no TLS handshake
/// can run at all.
fn default_provider() -> Result<&'static Arc<CryptoProvider>> {
    if let Some(provider) = CryptoProvider::get_default() {
        return Ok(provider);
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    CryptoProvider::get_default()
        .ok_or_else(|| Error::Connection("no TLS crypto provider available".into()))
}

/// `VerifyCa`: chain to a trusted root, no host-name check. Signature verification
/// delegates to the stock verifier; only the name check is skipped.
#[derive(Debug)]
struct ChainOnlyVerifier {
    roots_der: Vec<CertificateDer<'static>>,
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl ServerCertVerifier for ChainOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let cert = EndEntityCert::try_from(end_entity)
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        let anchors = self
            .roots_der
            .iter()
            .map(anchor_from_trusted_cert)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                TlsError::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(e))))
            })?;
        let provider = default_provider()
            .map_err(|_| TlsError::General("no TLS crypto provider available".into()))?;
        cert.verify_for_usage(
            provider.signature_verification_algorithms.all,
            &anchors,
            intermediates,
            now,
            KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|e| {
            let detail = match &e {
                webpki::Error::CertExpired { .. } => CertificateError::Expired,
                webpki::Error::CertNotValidYet { .. } => CertificateError::NotValidYet,
                webpki::Error::UnknownIssuer => CertificateError::UnknownIssuer,
                _ => CertificateError::Other(OtherError(Arc::new(e))),
            };
            TlsError::InvalidCertificate(detail)
        })?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// `Require`: encryption without authentication. Everything verifies.
#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        match default_provider() {
            Ok(provider) => provider
                .signature_verification_algorithms
                .supported_schemes(),
            Err(_) => Vec::new(),
        }
    }
}
