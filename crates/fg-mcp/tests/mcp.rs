//! The MCP surface against a real SQLite file and the encrypted store: what an agent sees, what it
//! cannot do, and what lands in the audit log.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use fg_core::{Actor, ConnectionConfig, ConnectionParams, DbKind, Decision, Environment, Outcome};
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
            "fg-mcp-{}-{}-{name}",
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
                "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, blob BLOB);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 100)
                 INSERT INTO items(name, blob) SELECT 'item ' || i, x'00ff' FROM n;",
            )
            .unwrap();
        }
        let mut options = ServiceOptions::new(dir.join("store.db"));
        options.force_read_only = true;
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(options, &keys).unwrap();
        let env = Self {
            dir,
            server: McpServer::new(service.clone()),
            service,
        };
        let path = db.to_string_lossy().into_owned();
        env.save("dev", &path, Environment::Development, |_| {})
            .await;
        env.save("prod", &path, Environment::Production, |_| {})
            .await;
        env.save("capped", &path, Environment::Development, |c| {
            c.mcp.max_rows = Some(3);
        })
        .await;
        env.save("prod-exposed", &path, Environment::Production, |c| {
            c.mcp.exposed = Some(true);
        })
        .await;
        env
    }

    async fn save(
        &self,
        name: &str,
        path: &str,
        environment: Environment,
        tweak: impl FnOnce(&mut ConnectionConfig),
    ) {
        let mut c = ConnectionConfig::new(
            name,
            DbKind::Sqlite,
            ConnectionParams::File { path: path.into() },
        );
        c.environment = environment;
        tweak(&mut c);
        self.service
            .save_connection(c, PasswordUpdate::Keep)
            .await
            .unwrap();
    }

    async fn rpc(&self, message: Value) -> Value {
        let reply = self
            .server
            .handle("stdio", &message.to_string())
            .await
            .unwrap();
        serde_json::from_str(&reply).unwrap()
    }

    async fn initialize(&self, client: &str) {
        let reply = self
            .rpc(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},
                "clientInfo":{"name":client,"version":"1"}}}),
            )
            .await;
        assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    /// `(text, is_error)` of a tool call.
    async fn tool(&self, name: &str, args: Value) -> (String, bool) {
        let reply = self
            .rpc(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":name,"arguments":args}}))
            .await;
        let result = &reply["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_owned(),
            result["isError"].as_bool().unwrap(),
        )
    }

    async fn ok(&self, name: &str, args: Value) -> Value {
        let (text, is_error) = self.tool(name, args).await;
        assert!(!is_error, "{name} failed: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn audit(&self) -> Vec<fg_core::AuditEntry> {
        self.service.drain_records().unwrap();
        self.service
            .query_audit(AuditFilter::default())
            .await
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

#[tokio::test]
async fn an_agent_can_list_describe_and_query() {
    let env = Env::new("flow").await;
    env.initialize("claude-code").await;

    let tools = env
        .rpc(json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_connections",
            "list_namespaces",
            "list_tables",
            "describe_table",
            "run_query",
            "explain",
            "sample_rows",
            "list_relations",
            "find_references",
            "execute_write"
        ]
    );

    let listed = env.ok("list_connections", json!({})).await;
    let ids: Vec<&Value> = listed["connections"].as_array().unwrap().iter().collect();
    let dev_id = ids.iter().find(|c| c["name"] == "dev").unwrap()["id"]
        .as_str()
        .unwrap();

    let tables = env.ok("list_tables", json!({"connection": dev_id})).await;
    assert_eq!(tables["objects"][0]["path"]["name"], "items");

    let described = env
        .ok(
            "describe_table",
            json!({"connection": "dev", "table": "items"}),
        )
        .await;
    let columns: Vec<&str> = described["table"]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["id", "name", "blob"]);

    let rows = env
        .ok(
            "run_query",
            json!({"connection": "dev", "sql": "SELECT id, name, blob FROM items WHERE id = 7"}),
        )
        .await;
    assert_eq!(rows["results"][0]["columns"], json!(["id", "name", "blob"]));
    // Binary is summarised, not dumped.
    assert_eq!(
        rows["results"][0]["rows"][0],
        json!([7, "item 7", "[2 bytes]"])
    );
    assert!(rows["note"].as_str().unwrap().contains("not instructions"));
}

#[tokio::test]
async fn production_like_connections_are_hidden_unless_exposed() {
    let env = Env::new("hidden").await;
    env.initialize("claude-desktop").await;

    let listed = env.ok("list_connections", json!({})).await;
    let mut names: Vec<&str> = listed["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["capped", "dev", "prod-exposed"]);

    // A hidden connection answers exactly like a missing one, so it cannot be probed.
    let (hidden, hidden_err) = env
        .tool(
            "run_query",
            json!({"connection": "prod", "sql": "SELECT 1"}),
        )
        .await;
    let (missing, missing_err) = env
        .tool(
            "run_query",
            json!({"connection": "nope", "sql": "SELECT 1"}),
        )
        .await;
    assert!(hidden_err && missing_err);
    assert_eq!(
        hidden.replace("prod", "X"),
        missing.replace("nope", "X"),
        "hidden and missing must read the same"
    );

    // The refusal is audited even though it never reached `run`.
    let audit = env.audit().await;
    let refused = audit
        .iter()
        .find(|a| a.statement == "SELECT 1")
        .expect("refused run_query is recorded");
    assert_eq!(refused.outcome, Outcome::NotRun);
    assert_eq!(refused.decision, Decision::Denied);
    assert_eq!(
        refused.actor,
        Actor::Mcp {
            client: "stdio:claude-desktop".into()
        }
    );
}

#[tokio::test]
async fn writes_are_refused_and_nothing_changes() {
    let env = Env::new("writes").await;
    env.initialize("evil").await;

    for sql in [
        "DELETE FROM items",
        "DROP TABLE items",
        "UPDATE items SET name = 'x'",
        "INSERT INTO items(name) VALUES ('x')",
        "SELECT 1; DELETE FROM items",
        "WITH d AS (DELETE FROM items RETURNING id) SELECT * FROM d",
        "PRAGMA writable_schema = 1",
        "ATTACH DATABASE ':memory:' AS m",
    ] {
        let (text, is_error) = env
            .tool("run_query", json!({"connection": "dev", "sql": sql}))
            .await;
        assert!(is_error, "`{sql}` must be refused, got: {text}");
    }

    let count = env
        .ok(
            "run_query",
            json!({"connection": "dev", "sql": "SELECT count(*) FROM items"}),
        )
        .await;
    assert_eq!(count["results"][0]["rows"][0][0], 100);

    // The saved config was not rewritten: the override is in-memory only.
    let stored = env.service.connections().await.unwrap();
    for c in stored {
        if let fg_store::StoredConnection::Ok(c) = c {
            assert_eq!(c.safe_mode, None, "{} was rewritten", c.name);
        }
    }

    // Every attempt has an audit row attributed to this client, none of the refused ones ran.
    let audit = env.audit().await;
    let refused: Vec<_> = audit
        .iter()
        .filter(|a| a.statement != "SELECT count(*) FROM items")
        .collect();
    assert_eq!(refused.len(), 8);
    assert!(refused.iter().all(|a| a.outcome == Outcome::NotRun));
    assert!(refused.iter().all(|a| a.actor
        == Actor::Mcp {
            client: "stdio:evil".into()
        }));
}

#[tokio::test]
async fn rows_are_capped_and_the_cap_cannot_be_raised() {
    let env = Env::new("caps").await;
    env.initialize("c").await;

    let ten = env
        .ok(
            "run_query",
            json!({"connection": "dev", "sql": "SELECT id FROM items", "max_rows": 10}),
        )
        .await;
    assert_eq!(ten["results"][0]["row_count"], 10);
    assert_eq!(ten["results"][0]["truncated"], true);

    // The connection's own cap (3) beats the agent's request for 50.
    let capped = env
        .ok(
            "run_query",
            json!({"connection": "capped", "sql": "SELECT id FROM items", "max_rows": 50}),
        )
        .await;
    assert_eq!(capped["results"][0]["row_count"], 3);
    assert_eq!(capped["results"][0]["truncated"], true);

    let all = env
        .ok(
            "run_query",
            json!({"connection": "dev", "sql": "SELECT id FROM items", "max_rows": 1000000}),
        )
        .await;
    assert_eq!(
        all["results"][0]["row_count"], 100,
        "hard cap is above the table"
    );
}

#[tokio::test]
async fn metadata_calls_are_audited_too() {
    let env = Env::new("audit").await;
    env.initialize("auditor").await;
    env.ok("list_connections", json!({})).await;
    env.ok(
        "describe_table",
        json!({"connection": "dev", "table": "items"}),
    )
    .await;
    let (_, err) = env
        .tool("describe_table", json!({"connection": "dev"}))
        .await;
    assert!(err, "missing `table`");

    let audit = env.audit().await;
    let statements: Vec<&str> = audit.iter().map(|a| a.statement.as_str()).collect();
    assert_eq!(
        statements
            .iter()
            .filter(|s| **s == "[list_connections]")
            .count(),
        1
    );
    assert_eq!(
        statements
            .iter()
            .filter(|s| **s == "[describe_table]")
            .count(),
        2
    );
}

#[tokio::test]
async fn protocol_edges() {
    let env = Env::new("protocol").await;

    // No identity yet: refused.
    let early = env
        .rpc(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"list_connections","arguments":{}}}))
        .await;
    assert_eq!(early["error"]["code"], -32002);

    // Notifications get no reply.
    assert!(
        env.server
            .handle(
                "stdio",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .await
            .is_none()
    );

    // An empty client name still gets a label, never an empty actor.
    env.initialize("").await;
    env.ok("list_connections", json!({})).await;
    let audit = env.audit().await;
    assert_eq!(
        audit[0].actor,
        Actor::Mcp {
            client: "stdio:unknown".into()
        }
    );

    // A tool that does not exist is still a protocol error.
    let unknown = env
        .rpc(json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
            "params":{"name":"no_such_tool","arguments":{}}}))
        .await;
    assert_eq!(unknown["error"]["code"], -32602);

    // execute_write exists now, so a call with no arguments is a tool
    // failure (isError), not a protocol error.
    let no_args = env
        .rpc(json!({"jsonrpc":"2.0","id":10,"method":"tools/call",
            "params":{"name":"execute_write","arguments":{}}}))
        .await;
    assert_eq!(no_args["result"]["isError"], true);

    // resources/list exists now (M7): a list, not -32601.
    let listed = env
        .rpc(json!({"jsonrpc":"2.0","id":11,"method":"resources/list"}))
        .await;
    assert_eq!(listed["error"], Value::Null);
    assert!(listed["result"]["resources"].as_array().is_some());

    let garbage: Value =
        serde_json::from_str(&env.server.handle("stdio", "{not json").await.unwrap()).unwrap();
    assert_eq!(garbage["error"]["code"], -32700);
    assert_eq!(garbage["id"], Value::Null);
}
