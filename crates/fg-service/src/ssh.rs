//! SSH tunnel for network connections.
//!
//! The tunnel is a localhost listener: every TCP connection accepted on it is
//! forwarded through the SSH session to the database host with a direct-tcpip
//! channel. Drivers keep seeing a plain TCP endpoint, so the tunnel stays a
//! service-layer concern and no driver learns about SSH.
//!
//! Host keys are verified against `~/.ssh/known_hosts` and never trusted on
//! first use: an unknown key, or a changed key, fails the open with a message
//! that says what happened and how to record the key (connect once with
//! OpenSSH). Silently accepting an unknown host key would let a
//! man-in-the-middle collect the database password, so fail-closed is the
//! only safe default here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fg_core::{Error, Result, SshAuth, SshConfig};
use russh::client;
use russh::client::{AuthResult, Config, Handler};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{
    HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, decode_secret_key, known_hosts,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// What the host-key check decided, kept so a failed open can explain itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyOutcome {
    Accepted,
    /// No entry for this host in known_hosts.
    Unknown,
    /// An entry exists but the key differs: possible man-in-the-middle.
    Changed(usize),
}

struct HostKeyChecker {
    host: String,
    port: u16,
    outcome: Arc<Mutex<Option<HostKeyOutcome>>>,
}

impl Handler for HostKeyChecker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let outcome = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                match known_hosts::check_known_hosts(&self.host, self.port, key) {
                    Ok(true) => HostKeyOutcome::Accepted,
                    Ok(false) => HostKeyOutcome::Unknown,
                    Err(russh::keys::Error::KeyChanged { line }) => HostKeyOutcome::Changed(line),
                    Err(_) => HostKeyOutcome::Unknown,
                }
            }
            // Certificates carry no known_hosts entry to check against.
            PublicKeyOrCertificate::Certificate(_) => HostKeyOutcome::Unknown,
        };
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(outcome);
        }
        Ok(outcome == HostKeyOutcome::Accepted)
    }
}

/// A live SSH tunnel: connections to `127.0.0.1:local_port` reach the database
/// host through the SSH session. The forward task owns the SSH handle, so
/// dropping the tunnel aborts the forwarder and ends the session with it.
#[derive(Debug)]
pub struct SshTunnel {
    local_port: u16,
    forward_task: JoinHandle<()>,
}

impl SshTunnel {
    /// Local port database clients connect to.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Open the tunnel. `secret` is the SSH password for [`SshAuth::Password`]
    /// or the key passphrase for [`SshAuth::PrivateKey`]; agent auth ignores it.
    pub async fn open(
        ssh: &SshConfig,
        secret: Option<&str>,
        target_host: &str,
        target_port: u16,
    ) -> Result<Self> {
        let config = Config {
            keepalive_interval: ssh
                .keepalive_secs
                .map(|secs| Duration::from_secs(u64::from(secs))),
            ..Config::default()
        };
        let outcome = Arc::new(Mutex::new(None));
        let checker = HostKeyChecker {
            host: ssh.host.clone(),
            port: ssh.port,
            outcome: Arc::clone(&outcome),
        };
        let mut handle = client::connect(Arc::new(config), (ssh.host.as_str(), ssh.port), checker)
            .await
            .map_err(|err| explain_connect(&outcome, &ssh.host, ssh.port, err))?;

        match &ssh.auth {
            SshAuth::Password => {
                let password = secret.ok_or_else(|| {
                    Error::Connection(format!(
                        "no SSH password saved for {}@{}",
                        ssh.user, ssh.host
                    ))
                })?;
                let result = handle
                    .authenticate_password(&ssh.user, password)
                    .await
                    .map_err(|err| {
                        Error::Connection(format!("SSH authentication failed: {err}"))
                    })?;
                check_auth(result, &ssh.user, &ssh.host)?;
            }
            SshAuth::PrivateKey { key_path } => {
                let contents = tokio::fs::read_to_string(key_path).await.map_err(|err| {
                    Error::Connection(format!("cannot read SSH key {key_path}: {err}"))
                })?;
                let key = decode_secret_key(&contents, secret).map_err(|err| {
                    Error::Connection(format!("cannot decode SSH key {key_path}: {err}"))
                })?;
                let hash_alg = if key.algorithm().is_rsa() {
                    match handle.best_supported_rsa_hash().await {
                        Ok(Some(alg)) => alg,
                        _ => Some(HashAlg::Sha512),
                    }
                } else {
                    None
                };
                let result = handle
                    .authenticate_publickey(
                        &ssh.user,
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                    )
                    .await
                    .map_err(|err| {
                        Error::Connection(format!("SSH authentication failed: {err}"))
                    })?;
                check_auth(result, &ssh.user, &ssh.host)?;
            }
            SshAuth::Agent => {
                #[cfg(unix)]
                {
                    let mut agent = AgentClient::connect_env().await.map_err(|err| {
                        Error::Connection(format!("cannot reach the SSH agent: {err}"))
                    })?;
                    let identities = agent.request_identities().await.map_err(|err| {
                        Error::Connection(format!("cannot list SSH agent identities: {err}"))
                    })?;
                    let mut accepted = false;
                    for identity in identities {
                        let AgentIdentity::PublicKey { key, .. } = identity else {
                            continue;
                        };
                        match handle
                            .authenticate_publickey_with(&ssh.user, key, None, &mut agent)
                            .await
                        {
                            Ok(AuthResult::Success) => {
                                accepted = true;
                                break;
                            }
                            Ok(_) | Err(_) => continue,
                        }
                    }
                    if !accepted {
                        return Err(Error::Connection(format!(
                            "no SSH agent identity was accepted for {}@{}",
                            ssh.user, ssh.host
                        )));
                    }
                }
                #[cfg(not(unix))]
                {
                    // russh's agent client over the environment socket is
                    // Unix-only, so this branch cannot compile on Windows.
                    return Err(Error::Unsupported(
                        "SSH agent authentication is not supported on this platform".into(),
                    ));
                }
            }
        }

        // Localhost only: the forward must never listen on a public interface.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|err| Error::Connection(format!("cannot bind the SSH forward: {err}")))?;
        let local_port = listener
            .local_addr()
            .map_err(|err| Error::Connection(format!("cannot read the SSH forward port: {err}")))?
            .port();
        let target_host = target_host.to_owned();
        let forward_task = tokio::spawn(async move {
            loop {
                let (socket, peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let host = target_host.clone();
                // The channel is opened here, on the task that owns the
                // handle, then handed to a copy task with the socket.
                let channel = match handle
                    .channel_open_direct_tcpip(
                        host,
                        u32::from(target_port),
                        "127.0.0.1",
                        u32::from(peer.port()),
                    )
                    .await
                {
                    Ok(channel) => channel,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let mut socket = socket;
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut socket).await;
                });
            }
        });

        Ok(Self {
            local_port,
            forward_task,
        })
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        self.forward_task.abort();
    }
}

fn check_auth(result: AuthResult, user: &str, host: &str) -> Result<()> {
    if result.success() {
        Ok(())
    } else {
        Err(Error::Connection(format!(
            "SSH authentication failed for {user}@{host}"
        )))
    }
}

fn explain_connect(
    outcome: &Arc<Mutex<Option<HostKeyOutcome>>>,
    host: &str,
    port: u16,
    err: russh::Error,
) -> Error {
    let decided = outcome.lock().ok().and_then(|slot| *slot);
    match decided {
        Some(HostKeyOutcome::Unknown) => Error::Connection(format!(
            "the SSH host key for {host}:{port} is not in ~/.ssh/known_hosts; \
             connect once with OpenSSH to record it, then retry"
        )),
        Some(HostKeyOutcome::Changed(line)) => Error::Connection(format!(
            "the SSH host key for {host}:{port} changed (known_hosts line {line}); \
             refusing to connect because the host may have been replaced"
        )),
        _ => Error::Connection(format!("cannot connect to {host}:{port} over SSH: {err}")),
    }
}
