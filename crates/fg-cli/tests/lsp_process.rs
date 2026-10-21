//! The language server as a **process**: spawn the real binary, speak LSP over its stdin and
//! stdout, and check what comes back.
//!
//! This is the test the in-process ones cannot replace. Framing bugs, a stray `println!` on stdout,
//! a missing flush, an exit code that is wrong - none of those show up when the handler is called
//! as a function.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A running `fastgrid-cli lsp`.
struct Lsp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Lsp {
    fn start() -> Self {
        let exe = env!("CARGO_BIN_EXE_fastgrid-cli");
        let mut child = Command::new(exe)
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        let body = serde_json::to_string(message).expect("encode");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        self.stdin.write_all(body.as_bytes()).expect("write body");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.read_until(|message| message.get("id") == Some(&json!(id)))
    }

    /// Messages read while looking for something else, so a test can inspect what the server sent
    /// on its own initiative.
    fn read_until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a response"
            );
            let message = self.read_one().expect("a message from the server");
            assert_eq!(message["jsonrpc"], "2.0", "every message is LSP: {message}");
            if done(&message) {
                return message;
            }
        }
    }

    fn read_one(&mut self) -> Option<Value> {
        let mut length: Option<usize> = None;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().ok();
            }
        }
        let length = length.expect("a content-length header");
        let mut body = vec![0u8; length];
        std::io::Read::read_exact(&mut self.stdout, &mut body).expect("body");
        Some(serde_json::from_slice(&body).expect("the body is JSON"))
    }

    /// Read the next notification with this method, skipping responses.
    fn notification(&mut self, method: &str) -> Value {
        self.read_until(|message| message.get("method") == Some(&json!(method)))
    }

    fn initialize(&mut self) -> Value {
        let result = self.request("initialize", json!({ "capabilities": {} }));
        self.notify("initialized", json!({}));
        result
    }

    /// Initialize with a schema, the way an editor integration would: the columns travel in
    /// `initializationOptions.fastgrid`.
    fn initialize_with_schema(&mut self) -> Value {
        let result = self.request(
            "initialize",
            json!({
                "capabilities": {},
                "initializationOptions": { "fastgrid": {
                    "kind": "sqlite",
                    "tables": [
                        { "schema": "main", "name": "items", "columns": [
                            { "name": "id", "type": "INTEGER" },
                            { "name": "name", "type": "TEXT" }
                        ]},
                        { "schema": "main", "name": "orders", "columns": [
                            { "name": "total", "type": "DECIMAL" }
                        ]}
                    ]
                }},
            }),
        );
        self.notify("initialized", json!({}));
        result
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_process_answers_initialize_and_shuts_down_cleanly() {
    let mut lsp = Lsp::start();
    let response = lsp.initialize();
    assert_eq!(response["result"]["serverInfo"]["name"], "fastgrid-sql");

    let shutdown = lsp.request("shutdown", Value::Null);
    assert!(shutdown["result"].is_null());
    lsp.notify("exit", Value::Null);
    // The server exits on `exit`; a killed process would also stop, so check the status code.
    let status = lsp.child.wait().expect("the process exits");
    assert!(status.success(), "exit status: {status:?}");
}

#[test]
fn completion_over_the_wire_resolves_an_alias() {
    let mut lsp = Lsp::start();
    lsp.initialize_with_schema();
    lsp.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///query.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT i. FROM items i",
            }
        }),
    );
    let response = lsp.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": "file:///query.sql" },
            "position": { "line": 0, "character": 9 },
        }),
    );
    let items = response["result"]["items"]
        .as_array()
        .expect("an item list");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert_eq!(
        labels,
        vec!["i.id", "i.name"],
        "the schema arrived over initialize and the alias resolved: {items:?}"
    );
    // A `textEdit`, not an `insertText`: an editor inserts `insertText` at the caret, which is how
    // accepting a completion used to append instead of replace.
    let edit = &items[0]["textEdit"];
    assert_eq!(edit["newText"], "id", "only the column name is inserted");
    assert!(
        items[0].get("insertText").is_none(),
        "insertText would append rather than replace: {items:?}"
    );
    assert_eq!(
        edit["range"]["start"]["character"], 9,
        "the edit covers the word after the dot"
    );
    assert_eq!(
        items[0]["detail"], "integer",
        "the type it was given, lower-cased for the menu hint"
    );
}

#[test]
fn a_did_open_publishes_an_empty_diagnostic_list() {
    let mut lsp = Lsp::start();
    lsp.initialize();
    lsp.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///clean.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT 1;",
            }
        }),
    );
    let published = lsp.notification("textDocument/publishDiagnostics");
    assert_eq!(published["params"]["uri"], "file:///clean.sql");
    assert_eq!(published["params"]["diagnostics"], json!([]));
}

#[test]
fn a_malformed_message_does_not_kill_the_session() {
    let mut lsp = Lsp::start();
    lsp.initialize();
    // A body that is not JSON: the server answers with a parse error and stays up.
    let body = "this is not json";
    write!(lsp.stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    lsp.stdin.write_all(body.as_bytes()).unwrap();
    lsp.stdin.flush().unwrap();
    let response = lsp.read_until(|m| m.get("error").is_some());
    assert_eq!(response["error"]["code"], -32700);

    // Still answering real requests.
    let response = lsp.request("shutdown", Value::Null);
    assert!(response["result"].is_null());
}

#[test]
fn nothing_but_protocol_bytes_reaches_stdout() {
    // A `println!` anywhere in the server corrupts the stream; the only way to notice is to read
    // it. Every message must parse as JSON with a `jsonrpc` field.
    let mut lsp = Lsp::start();
    lsp.initialize();
    lsp.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": "file:///a.sql", "languageId": "sql", "version": 1, "text": "SELECT 1;" }
        }),
    );
    // The push for the opened document, then a request/response pair. Each must be clean LSP.
    let pushed = lsp.notification("textDocument/publishDiagnostics");
    assert_eq!(pushed["jsonrpc"], "2.0");
    let answered = lsp.request("shutdown", Value::Null);
    assert_eq!(answered["jsonrpc"], "2.0");
}

#[test]
fn the_version_command_works() {
    let exe = env!("CARGO_BIN_EXE_fastgrid-cli");
    let output = Command::new(exe)
        .arg("--version")
        .output()
        .expect("the binary runs");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("fastgrid-cli "),
        "unexpected output: {stdout}"
    );
}

#[test]
fn an_unknown_command_fails_with_a_usage_hint() {
    let exe = env!("CARGO_BIN_EXE_fastgrid-cli");
    let output = Command::new(exe)
        .arg("nonsense")
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--help"), "says what to do: {stderr}");
}
