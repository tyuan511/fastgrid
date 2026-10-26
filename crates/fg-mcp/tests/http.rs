//! The loopback Streamable HTTP transport: auth, sessions, methods,
//! and the same policy the stdio transport enforces (a headless
//! server has no GUI, so agent writes deny at once).
//!
//! Raw TCP, no HTTP client crate: the point is to prove the bytes on
//! the wire, including the status lines an SDK would see.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, Environment};
use fg_mcp::{HttpServer, serve_http};
use fg_service::{KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service, ServiceOptions};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Env {
    dir: PathBuf,
    service: Service,
    server: HttpServer,
}

impl Env {
    async fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-mcp-http-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("data.db");
        {
            let c = rusqlite::Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO items VALUES (1, 'a');").unwrap();
        }
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        // Deliberately NOT force-read-only: the gate under test is the
        // agent approval path, not the stdio read-only override.
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        let path = db.to_string_lossy().into_owned();
        let mut dev = ConnectionConfig::new(
            "dev",
            DbKind::Sqlite,
            ConnectionParams::File { path: path.clone() },
        );
        dev.environment = Environment::Development;
        service
            .save_connection(dev, PasswordUpdate::Keep)
            .await
            .unwrap();
        let mut writable =
            ConnectionConfig::new("writable", DbKind::Sqlite, ConnectionParams::File { path });
        writable.environment = Environment::Development;
        writable.mcp.allow_write = true;
        service
            .save_connection(writable, PasswordUpdate::Keep)
            .await
            .unwrap();
        let server = serve_http(service.clone(), 0).await.unwrap();
        Self {
            dir,
            service,
            server,
        }
    }

    fn port(&self) -> u16 {
        self.server.addr.port()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.server.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Response {
    status: u16,
    session: Option<String>,
    body: String,
}

async fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    session: Option<&str>,
    body: &str,
) -> Response {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(session) = session {
        head.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let session = head.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("mcp-session-id") {
            Some(value.trim().to_owned())
        } else {
            None
        }
    });
    Response {
        status,
        session,
        body: body.to_owned(),
    }
}

fn rpc(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

async fn initialize(env: &Env) -> (String, String) {
    let body = rpc(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "http-test"}}),
    );
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        None,
        &body,
    )
    .await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    let value: Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(value["result"]["protocolVersion"], "2025-06-18");
    let session = res.session.expect("initialize issues a session id");
    (session, res.body)
}

#[tokio::test]
async fn bearer_token_is_required() {
    let env = Env::new("auth").await;
    let body = rpc(1, "ping", json!({}));
    let res = request(env.port(), "POST", "/mcp", None, None, &body).await;
    assert_eq!(res.status, 401);
    let res = request(env.port(), "POST", "/mcp", Some("wrong"), None, &body).await;
    assert_eq!(res.status, 401);
    env.service.close_all();
}

#[tokio::test]
async fn method_and_path_are_routed() {
    let env = Env::new("routing").await;
    let res = request(env.port(), "GET", "/mcp", Some(&env.server.token), None, "").await;
    assert_eq!(res.status, 405);
    let res = request(
        env.port(),
        "POST",
        "/nope",
        Some(&env.server.token),
        None,
        "",
    )
    .await;
    assert_eq!(res.status, 404);
    env.service.close_all();
}

#[tokio::test]
async fn initialize_then_call_with_session() {
    let env = Env::new("session").await;
    let (session, _) = initialize(&env).await;
    // A call without a session has no identity, even after another
    // session initialized.
    let body = rpc(
        2,
        "tools/call",
        json!({"name": "list_connections", "arguments": {}}),
    );
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        None,
        &body,
    )
    .await;
    let value: Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(value["error"]["code"], -32002);
    // With the session, the same call lists the visible connection.
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        Some(&session),
        &body,
    )
    .await;
    assert_eq!(res.status, 200);
    let value: Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(value["result"]["isError"], false);
    assert!(
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("dev"),
        "body: {}",
        res.body
    );
    env.service.close_all();
}

#[tokio::test]
async fn resources_over_http() {
    let env = Env::new("resources").await;
    let (session, _) = initialize(&env).await;
    let body = rpc(2, "resources/list", json!({}));
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        Some(&session),
        &body,
    )
    .await;
    let value: Value = serde_json::from_str(&res.body).unwrap();
    let uri = value["result"]["resources"][0]["uri"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(uri.starts_with("fastgrid://"), "{uri}");
    let body = rpc(3, "resources/read", json!({"uri": uri}));
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        Some(&session),
        &body,
    )
    .await;
    let value: Value = serde_json::from_str(&res.body).unwrap();
    let text = value["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains("items"), "{text}");
    env.service.close_all();
}

#[tokio::test]
async fn headless_http_denies_agent_writes_at_once() {
    let env = Env::new("headless-write").await;
    let (session, _) = initialize(&env).await;
    // allow_write is on, but no GUI is attached to this server, so the
    // write denies immediately instead of waiting out the timeout.
    let body = rpc(
        2,
        "tools/call",
        json!({"name": "execute_write", "arguments": {"connection": "writable", "sql": "INSERT INTO items VALUES (2, 'b')"}}),
    );
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        Some(&session),
        &body,
    )
    .await;
    let value: Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(value["result"]["isError"], true);
    let text = value["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("no GUI"), "{text}");
    // allow_write off denies even earlier, with its own message.
    let body = rpc(
        3,
        "tools/call",
        json!({"name": "execute_write", "arguments": {"connection": "dev", "sql": "INSERT INTO items VALUES (3, 'c')"}}),
    );
    let res = request(
        env.port(),
        "POST",
        "/mcp",
        Some(&env.server.token),
        Some(&session),
        &body,
    )
    .await;
    let value: Value = serde_json::from_str(&res.body).unwrap();
    let text = value["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("not enabled"), "{text}");
    env.service.close_all();
}
