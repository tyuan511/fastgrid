//! End-to-end test for [`fg_service::SshTunnel`] against a real russh server.
//!
//! The tunnel is exercised the way a database client would use it: an SSH
//! server on localhost forwards to a TCP echo target, the tunnel opens with
//! password auth, and bytes sent to the tunnel's local port come back.
//!
//! The test appends one loopback-only entry to `~/.ssh/known_hosts` (the
//! tunnel verifies host keys against it and refuses unknown keys) and removes
//! that entry again on drop, even if the test panics.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fg_core::{SshAuth, SshConfig};
use fg_service::SshTunnel;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "tester";
const PASSWORD: &str = "hunter2";

struct TestServer;

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == USER && password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let target = TcpStream::connect((host, port as u16))
            .await
            .map_err(russh::Error::from)?;
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let mut target = target;
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
        });
        Ok(())
    }
}

/// Removes its known_hosts line again when dropped.
struct KnownHostsGuard {
    path: PathBuf,
    line: String,
    existed: bool,
}

impl KnownHostsGuard {
    fn add(port: u16, public_key_line: &str) -> Self {
        let path = home_dir().join(".ssh").join("known_hosts");
        let line = format!("[127.0.0.1]:{port} {public_key_line}");
        let existed = path.exists();
        let mut contents = if existed {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(&line);
        contents.push('\n');
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        Self {
            path,
            line,
            existed,
        }
    }
}

impl Drop for KnownHostsGuard {
    fn drop(&mut self) {
        if !self.existed {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        if let Ok(contents) = std::fs::read_to_string(&self.path) {
            let kept: Vec<&str> = contents.lines().filter(|l| *l != self.line).collect();
            let mut out = kept.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = std::fs::write(&self.path, out);
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is set")
}

fn ssh_config(port: u16) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".to_owned(),
        port,
        user: USER.to_owned(),
        auth: SshAuth::Password,
        keepalive_secs: None,
    }
}

/// A TCP echo target: whatever it reads, it writes back.
async fn serve_echo(listener: TcpListener) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(async move {
            let (mut read, mut write) = socket.into_split();
            let _ = tokio::io::copy(&mut read, &mut write).await;
        });
    }
}

#[tokio::test]
async fn tunnel_forwards_bytes_and_refuses_unknown_keys() {
    tokio::time::timeout(Duration::from_secs(30), async {
        // Echo target.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(serve_echo(echo));

        // SSH server key via ssh-keygen (no extra test dependency).
        let dir = std::env::temp_dir().join(format!("fg-ssh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("host_key");
        let status = std::process::Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                key_path.to_str().unwrap(),
                "-N",
                "",
                "-q",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let private = std::fs::read_to_string(&key_path).unwrap();
        let server_key = russh::keys::decode_secret_key(&private, None).unwrap();
        let public = std::process::Command::new("ssh-keygen")
            .args(["-y", "-f", key_path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(public.status.success());
        let public_line = String::from_utf8(public.stdout).unwrap();
        let public_line = public_line.trim().to_owned();

        // SSH server listener.
        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_port = ssh_listener.local_addr().unwrap().port();
        let server_config = Arc::new(server::Config {
            keys: vec![server_key],
            ..server::Config::default()
        });
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = ssh_listener.accept().await else {
                    break;
                };
                let config = Arc::clone(&server_config);
                tokio::spawn(async move {
                    let session = server::run_stream(config, socket, TestServer).await;
                    if let Ok(session) = session {
                        let _ = session.await;
                    }
                });
            }
        });

        // Unknown host key: no known_hosts entry yet, so the open must fail
        // closed instead of trusting the key.
        let err = SshTunnel::open(
            &ssh_config(ssh_port),
            Some(PASSWORD),
            "127.0.0.1",
            echo_port,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("not in ~/.ssh/known_hosts"),
            "unexpected error: {err}"
        );

        // Record the key, then the tunnel must open and forward bytes.
        let _guard = KnownHostsGuard::add(ssh_port, &public_line);
        let tunnel = SshTunnel::open(
            &ssh_config(ssh_port),
            Some(PASSWORD),
            "127.0.0.1",
            echo_port,
        )
        .await
        .unwrap();
        let mut through = TcpStream::connect(("127.0.0.1", tunnel.local_port()))
            .await
            .unwrap();
        through.write_all(b"hello-tunnel").await.unwrap();
        let mut buf = [0u8; 12];
        through.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello-tunnel");

        // Wrong password must fail.
        let err = SshTunnel::open(&ssh_config(ssh_port), Some("wrong"), "127.0.0.1", echo_port)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("authentication failed"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    })
    .await
    .expect("test timed out");
}
