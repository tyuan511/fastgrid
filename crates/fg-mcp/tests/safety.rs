//! The M7 safety set: every read-only bypass attempt is refused, the
//! table survives them all, and every refusal lands in the audit log.
//! Plus the new read tools and resources against real foreign keys.
//!
//! The service here is force-read-only, the stdio posture: these
//! refusals must hold whatever the connection's own safe mode says.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, Decision, Environment};
use fg_mcp::McpServer;
use fg_service::{
    AuditFilter, KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service, ServiceOptions,
};
use serde_json::{Value, json};

struct Env {
    dir: PathBuf,
    service: Service,
    server: McpServer,
}

impl Env {
    async fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-mcp-safety-{}-{}-{name}",
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
            c.execute_batch(
                "CREATE TABLE parents(id INTEGER PRIMARY KEY, name TEXT);
                 CREATE TABLE children(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parents(id) ON DELETE CASCADE, name TEXT);
                 INSERT INTO parents VALUES (1, 'p1');
                 INSERT INTO children VALUES (1, 1, 'c1');
                 CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 100)
                 INSERT INTO items(name) SELECT 'item ' || i FROM n;",
            )
            .unwrap();
        }
        let mut options = ServiceOptions::new(dir.join("store.db"));
        options.force_read_only = true;
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(options, &keys).unwrap();
        let path = db.to_string_lossy().into_owned();
        let mut dev = ConnectionConfig::new("dev", DbKind::Sqlite, ConnectionParams::File { path });
        dev.environment = Environment::Development;
        service
            .save_connection(dev, PasswordUpdate::Keep)
            .await
            .unwrap();
        Self {
            dir,
            server: McpServer::new(service.clone()),
            service,
        }
    }

    async fn call(&self, name: &str, args: Value) -> Value {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}});
        let reply = self
            .server
            .handle("stdio", &message.to_string())
            .await
            .unwrap();
        serde_json::from_str(&reply).unwrap()
    }

    async fn init(&self) {
        let message = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "safety-test"}}});
        let reply = self
            .server
            .handle("stdio", &message.to_string())
            .await
            .unwrap();
        assert!(reply.contains("2025-06-18"), "{reply}");
    }

    fn count(&self, table: &str) -> i64 {
        let c = rusqlite::Connection::open(self.dir.join("data.db")).unwrap();
        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.service.close_all();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn is_error(reply: &Value) -> bool {
    reply["result"]["isError"] == true
}

fn tool_text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().unwrap_or("")
}

#[tokio::test]
async fn read_only_bypass_corpus_is_all_refused() {
    let env = Env::new("bypass").await;
    env.init().await;
    // Each of these must be refused, not run: a write hiding behind a
    // read, a second statement, a comment, a CTE, SELECT INTO, a
    // utility write, or a bare write.
    let attempts = [
        "SELECT 1; DROP TABLE items",
        "/* lead */ DELETE FROM items",
        "WITH x AS (DELETE FROM items RETURNING *) SELECT * FROM x",
        "SELECT * INTO stolen FROM items",
        "PRAGMA writable_schema = ON",
        "VACUUM",
        "DROP TABLE items",
        "DELETE FROM items WHERE id = 1",
        "INSERT INTO items VALUES (101, 'evil')",
    ];
    for sql in attempts {
        let reply = env
            .call("run_query", json!({"connection": "dev", "sql": sql}))
            .await;
        assert!(is_error(&reply), "not refused: {sql}\n{reply}");
    }
    // execute_write meets the same wall on a read-only server, before
    // any allow_write question even arises.
    let reply = env
        .call(
            "execute_write",
            json!({"connection": "dev", "sql": "DELETE FROM items WHERE id = 1"}),
        )
        .await;
    assert!(is_error(&reply), "{reply}");
    // Nothing was touched.
    assert_eq!(env.count("items"), 100);
    assert_eq!(env.count("parents"), 1);
    assert_eq!(env.count("children"), 1);
    // Every refusal left exactly one audit row behind it.
    let rows = env
        .service
        .query_audit(AuditFilter::default())
        .await
        .unwrap();
    let denied = rows
        .iter()
        .filter(|r| r.decision == Decision::Denied)
        .count();
    assert!(
        denied > attempts.len(),
        "expected {} denied rows, got {denied} of {}",
        attempts.len() + 1,
        rows.len()
    );
}

#[tokio::test]
async fn explain_never_executes() {
    let env = Env::new("explain").await;
    env.init().await;
    // A plain read explains fine.
    let reply = env
        .call(
            "explain",
            json!({"connection": "dev", "sql": "SELECT * FROM items WHERE id = 1"}),
        )
        .await;
    assert!(!is_error(&reply), "{reply}");
    // EXPLAIN ANALYZE of a write would execute it: refused.
    let reply = env
        .call(
            "explain",
            json!({"connection": "dev", "sql": "EXPLAIN ANALYZE DELETE FROM items"}),
        )
        .await;
    assert!(is_error(&reply), "{reply}");
    // Two statements, the second hidden: refused even though the
    // first one reads.
    let reply = env
        .call(
            "explain",
            json!({"connection": "dev", "sql": "SELECT 1; DELETE FROM items"}),
        )
        .await;
    assert!(is_error(&reply), "{reply}");
    // A bare write: refused before any plan runs.
    let reply = env
        .call(
            "explain",
            json!({"connection": "dev", "sql": "DELETE FROM items"}),
        )
        .await;
    assert!(is_error(&reply), "{reply}");
    assert_eq!(env.count("items"), 100);
}

#[tokio::test]
async fn new_read_tools_and_resources() {
    let env = Env::new("tools").await;
    env.init().await;
    // sample_rows: a peek, capped by the caller's limit.
    let reply = env
        .call(
            "sample_rows",
            json!({"connection": "dev", "namespace": "main", "table": "items", "limit": 3}),
        )
        .await;
    assert!(!is_error(&reply), "{reply}");
    let text = tool_text(&reply);
    let value: Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["results"][0]["row_count"], 3);
    // list_relations: children -> parents, with the cascade action.
    let reply = env
        .call(
            "list_relations",
            json!({"connection": "dev", "namespace": "main"}),
        )
        .await;
    assert!(!is_error(&reply), "{reply}");
    let value: Value = serde_json::from_str(tool_text(&reply)).unwrap();
    let relations = value["relations"].as_array().unwrap();
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0]["from"]["name"], "children");
    assert_eq!(relations[0]["key"]["referenced"]["name"], "parents");
    // find_references: the reverse direction.
    let reply = env
        .call(
            "find_references",
            json!({"connection": "dev", "namespace": "main", "table": "parents"}),
        )
        .await;
    assert!(!is_error(&reply), "{reply}");
    let value: Value = serde_json::from_str(tool_text(&reply)).unwrap();
    assert_eq!(value["referenced_by"][0]["table"], "children");
    assert_eq!(value["referenced_by"][0]["from_columns"][0], "parent_id");
    // resources/list names one snapshot per visible connection, and
    // resources/read builds it with real tables inside.
    let message = json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {}});
    let reply: Value = serde_json::from_str(
        &env.server
            .handle("stdio", &message.to_string())
            .await
            .unwrap(),
    )
    .unwrap();
    let uri = reply["result"]["resources"][0]["uri"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(uri.starts_with("fastgrid://"), "{uri}");
    let message = json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read",
        "params": {"uri": uri}});
    let reply: Value = serde_json::from_str(
        &env.server
            .handle("stdio", &message.to_string())
            .await
            .unwrap(),
    )
    .unwrap();
    let text = reply["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains("children"), "{text}");
    assert!(text.contains("parents"), "{text}");
    // An unknown resource is a protocol error, not a leak: same shape
    // as a hidden one.
    let message = json!({"jsonrpc": "2.0", "id": 4, "method": "resources/read",
        "params": {"uri": "fastgrid://nope/schema"}});
    let reply: Value = serde_json::from_str(
        &env.server
            .handle("stdio", &message.to_string())
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(reply.get("error").is_some(), "{reply}");
}
