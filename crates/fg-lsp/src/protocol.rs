//! The LSP protocol surface: which methods this server answers and with what.
//!
//! Split from [`crate::wire`] on purpose. `Server` takes a JSON request and returns a JSON
//! response, with no IO at all; the process loop in `fg-cli` is then a `read -> handle -> write`
//! pipe over that. A protocol that can only be tested by spawning a process is a protocol that
//! cannot be tested.
//!
//! Scope, deliberately small: this is a SQL analysis server for one document at a time, not an
//! editor integration framework. It implements the lifecycle (`initialize`, `initialized`,
//! `shutdown`, `exit`), document sync (`textDocument/didOpen`, `didChange`, `didClose`),
//! `textDocument/completion`, `textDocument/diagnostic` and the `publishDiagnostics` notification
//! that goes with it. Hover, go-to-definition and code actions are the next increment; the analysis
//! they need is already in [`crate::scope`].
//!
//! Two rules the spec makes easy to break:
//! * `initialize` must be answered before anything else, and its `capabilities` decide what the
//!   client will ever send.
//! * Positions are **line / character**, not bytes. Every conversion happens here, once.

use std::collections::HashMap;

use fg_core::{ColumnMeta, DbKind, ObjectPath};
use serde_json::{Value, json};

use crate::complete::{CompletionKind, complete_at};
use crate::diagnostics::Diagnostic;
use crate::scope::Catalog;

/// Everything the server needs that is not in a request.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The engine, so the lexer picks the right quoting and comment rules.
    pub kind: DbKind,
    /// Objects the server knows about, for completing tables that are not in scope yet.
    pub objects: Vec<ObjectPath>,
    /// Columns per object, keyed by path.
    pub columns: Vec<(ObjectPath, Vec<ColumnMeta>)>,
    /// Diagnostics pushed by the client (a failed run), keyed by document URI. Cleared when the
    /// document changes, because the text they described is gone.
    pub reported: Vec<(String, Diagnostic)>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // SQLite is the engine the editor ships with; a client that cares sets this.
            kind: DbKind::Sqlite,
            objects: Vec::new(),
            columns: Vec::new(),
            reported: Vec::new(),
        }
    }
}

impl Catalog for ServerConfig {
    fn columns(&self, path: &ObjectPath) -> Option<&[ColumnMeta]> {
        self.columns
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.as_slice())
    }

    fn resolve(&self, name: &str) -> Option<ObjectPath> {
        self.objects
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .cloned()
    }
}

/// One open document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Document {
    uri: String,
    text: String,
    /// LSP version, echoed back so the client can drop stale pushes.
    version: i64,
}

/// A request or notification, already parsed.
enum Incoming {
    /// Has an `id`: the client expects exactly one response.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// No `id`: never answered.
    Notification { method: String, params: Value },
}

/// The protocol state machine. Owns the documents; answers one message at a time.
pub struct Server {
    config: ServerConfig,
    documents: HashMap<String, Document>,
    /// Set once `initialize` was answered: a request before it is a protocol error.
    initialized: bool,
    /// Set by `shutdown`: after it, only `exit` is legal.
    shutting_down: bool,
    /// Set by `exit`: the process should stop. The spec says the client closes stdin, but a client
    /// that only sends `exit` would otherwise hang the process waiting for more input.
    exit_requested: bool,
}

impl Server {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            documents: HashMap::new(),
            initialized: false,
            shutting_down: false,
            exit_requested: false,
        }
    }

    /// Handle one message. Returns what to write back, if anything.
    ///
    /// A notification returns no response by definition; the `publishDiagnostics` it produces is
    /// returned as a second message so the caller can write both.
    pub fn handle(&mut self, body: &str) -> Vec<Value> {
        let incoming = match parse(body) {
            Ok(incoming) => incoming,
            Err(response) => return vec![response],
        };
        match incoming {
            Incoming::Notification { method, params } => self.notification(&method, params),
            Incoming::Request { id, method, params } => {
                vec![self.request(id, &method, params)]
            }
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        // `initialize` is the only request allowed before initialization.
        if !self.initialized && method != "initialize" {
            return error(id, -32002, "server not initialized");
        }
        if self.shutting_down && method != "shutdown" {
            return error(id, -32600, "server is shutting down");
        }
        match method {
            "initialize" => {
                self.initialized = true;
                self.apply_initialization_options(&params);
                let capabilities = json!({
                    "textDocumentSync": {
                        // Full sync: the analysis re-reads the whole buffer anyway, and a diff
                        // protocol here would be complexity without a payoff.
                        "openClose": true,
                        "change": 1,
                    },
                    "completionProvider": {
                        // `.` starts a qualified column list.
                        "triggerCharacters": [".", " "],
                        "resolveProvider": false,
                    },
                    "diagnosticProvider": {
                        "interFileDependencies": false,
                        "workspaceDiagnostics": false,
                    },
                });
                let info = json!({
                    "name": "fastgrid-sql",
                    "version": env!("CARGO_PKG_VERSION"),
                });
                ok(
                    id,
                    json!({ "capabilities": capabilities, "serverInfo": info }),
                )
            }
            "shutdown" => {
                self.shutting_down = true;
                ok(id, Value::Null)
            }
            "textDocument/completion" => {
                let Some((uri, line, character)) = position_params(&params) else {
                    return error(id, -32602, "completion needs textDocument.uri and position");
                };
                let Some(document) = self.documents.get(&uri) else {
                    return error(id, -32602, "unknown document");
                };
                let offset = offset_of(&document.text, line, character);
                let completions = complete_at(
                    &document.text,
                    offset,
                    self.config.kind,
                    &self.config,
                    &self.config.objects,
                );
                let items: Vec<Value> = completions
                    .into_iter()
                    .map(|completion| {
                        let kind = match completion.kind {
                            CompletionKind::Column => 5,   // Field
                            CompletionKind::Relation => 6, // Variable
                            CompletionKind::Table => 7,    // Class
                            CompletionKind::Keyword => 14, // Keyword
                        };
                        // `textEdit`, not `insertText`: an editor inserts `insertText` at the
                        // caret, so accepting `select` after typing `se` would give `seselect`.
                        // The edit replaces the word the user already typed.
                        let (start_line, start_character) =
                            position_of(&document.text, completion.replace.start);
                        let (end_line, end_character) =
                            position_of(&document.text, completion.replace.end);
                        let mut item = json!({
                            "label": completion.label,
                            "kind": kind,
                            "textEdit": {
                                "range": {
                                    "start": { "line": start_line, "character": start_character },
                                    "end": { "line": end_line, "character": end_character },
                                },
                                "newText": completion.text,
                            },
                        });
                        if let Some(detail) = completion.detail {
                            item["detail"] = Value::String(detail);
                        }
                        item
                    })
                    .collect();
                // `isIncomplete: false` lets the client filter the list locally as the user types,
                // which is what makes a 50-item menu feel instant.
                ok(id, json!({ "isIncomplete": false, "items": items }))
            }
            "textDocument/diagnostic" => {
                let Some(uri) = params
                    .get("textDocument")
                    .and_then(|d| d.get("uri"))
                    .and_then(Value::as_str)
                else {
                    return error(id, -32602, "diagnostic needs textDocument.uri");
                };
                let Some(document) = self.documents.get(uri) else {
                    return error(id, -32602, "unknown document");
                };
                ok(
                    id,
                    json!({ "kind": "full", "items": self.diagnostic_items(document) }),
                )
            }
            other => error(id, -32601, &format!("method not found: {other}")),
        }
    }

    fn notification(&mut self, method: &str, params: Value) -> Vec<Value> {
        match method {
            // Sent after `initialize` was answered; nothing to do, but it must not error.
            "initialized" => Vec::new(),
            "exit" => {
                self.exit_requested = true;
                Vec::new()
            }
            "textDocument/didOpen" => {
                let Some((uri, version, text)) = open_params(&params) else {
                    return Vec::new();
                };
                let document = Document {
                    uri: uri.clone(),
                    text,
                    version,
                };
                self.documents.insert(uri.clone(), document);
                vec![self.publish_diagnostics(&uri)]
            }
            "textDocument/didChange" => {
                let Some(uri) = params
                    .get("textDocument")
                    .and_then(|d| d.get("uri"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    return Vec::new();
                };
                let version = params
                    .get("textDocument")
                    .and_then(|d| d.get("version"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // Full sync: the last content change is the whole document.
                let text = params
                    .get("contentChanges")
                    .and_then(Value::as_array)
                    .and_then(|changes| changes.last())
                    .and_then(|change| change.get("text"))
                    .and_then(Value::as_str);
                let Some(text) = text else {
                    return Vec::new();
                };
                if let Some(document) = self.documents.get_mut(&uri) {
                    document.text = text.to_owned();
                    document.version = version;
                    // The text a pushed diagnostic described is gone.
                    self.config.reported.retain(|(at, _)| at != &uri);
                    vec![self.publish_diagnostics(&uri)]
                } else {
                    Vec::new()
                }
            }
            "textDocument/didClose" => {
                let Some(uri) = params
                    .get("textDocument")
                    .and_then(|d| d.get("uri"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    return Vec::new();
                };
                self.documents.remove(&uri);
                // An empty publish is how the spec says "this document has no problems now".
                vec![json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "diagnostics": [] },
                })]
            }
            _ => Vec::new(),
        }
    }

    /// Read the schema a client sends in `initialize`.
    ///
    /// LSP has no notion of "the database schema", so it travels as an initialization option under
    /// this server's own key:
    ///
    /// ```json
    /// { "initializationOptions": { "fastgrid": {
    ///     "kind": "sqlite",
    ///     "tables": [ { "schema": "main", "name": "items",
    ///                   "columns": [ { "name": "id", "type": "INTEGER" } ] } ] } } }
    /// ```
    ///
    /// Unknown or malformed entries are skipped: a client with an older shape should get a working
    /// server without columns, not an error.
    fn apply_initialization_options(&mut self, params: &Value) {
        let Some(options) = params
            .get("initializationOptions")
            .and_then(|o| o.get("fastgrid"))
        else {
            return;
        };
        if let Some(kind) = options.get("kind").and_then(Value::as_str) {
            self.config.kind = match kind.to_ascii_lowercase().as_str() {
                "postgres" | "postgresql" => DbKind::Postgres,
                "mysql" | "mariadb" => DbKind::MySql,
                _ => DbKind::Sqlite,
            };
        }
        let Some(tables) = options.get("tables").and_then(Value::as_array) else {
            return;
        };
        for table in tables {
            let Some(name) = table.get("name").and_then(Value::as_str) else {
                continue;
            };
            let namespace = table.get("schema").and_then(Value::as_str);
            let path = ObjectPath::new(namespace, name);
            let columns: Vec<ColumnMeta> = table
                .get("columns")
                .and_then(Value::as_array)
                .map(|columns| {
                    columns
                        .iter()
                        .filter_map(|column| {
                            let name = column.get("name").and_then(Value::as_str)?;
                            let native = column
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            Some(ColumnMeta::new(
                                name,
                                fg_core::DataType::new(native, category_of(native)),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.config.objects.push(path.clone());
            self.config.columns.push((path, columns));
        }
    }

    /// The diagnostics for a document: the ones the client reported for it, converted to LSP ranges.
    fn diagnostic_items(&self, document: &Document) -> Vec<Value> {
        self.config
            .reported
            .iter()
            .filter(|(uri, _)| uri == &document.uri)
            .map(|(_, diagnostic)| {
                let start = position_of(&document.text, diagnostic.range.start);
                let end = position_of(&document.text, diagnostic.range.end);
                json!({
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 },
                    },
                    "severity": 1, // Error: these come from a failed statement.
                    "source": "fastgrid",
                    "message": diagnostic.message,
                })
            })
            .collect()
    }

    fn publish_diagnostics(&self, uri: &str) -> Value {
        let items = self
            .documents
            .get(uri)
            .map(|document| self.diagnostic_items(document))
            .unwrap_or_default();
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": items },
        })
    }

    /// Replace the diagnostics for one document (the client ran a statement and it failed).
    pub fn set_diagnostics(&mut self, uri: impl Into<String>, diagnostics: Vec<Diagnostic>) {
        let uri = uri.into();
        self.config.reported.retain(|(at, _)| at != &uri);
        self.config
            .reported
            .extend(diagnostics.into_iter().map(|d| (uri.clone(), d)));
    }

    /// Whether the client asked the server to stop (`exit`). An IO loop should end after writing
    /// whatever the last message produced.
    pub fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    /// The diagnostics to push for a document after `set_diagnostics`, for a caller that has an IO
    /// loop of its own.
    pub fn diagnostics_for(&self, uri: &str) -> Value {
        self.publish_diagnostics(uri)
    }
}

/// A native type name to the category the UI and the completion hints use.
fn category_of(native: &str) -> fg_core::TypeCategory {
    use fg_core::TypeCategory::*;
    let base = native.to_ascii_lowercase();
    let base = base.split(['(', ' ']).next().unwrap_or_default();
    match base {
        "int" | "integer" | "bigint" | "smallint" | "tinyint" | "serial" => Integer,
        "real" | "float" | "double" => Float,
        "decimal" | "numeric" | "money" => Decimal,
        "bool" | "boolean" => Bool,
        "date" => Date,
        "time" => Time,
        "timestamp" | "datetime" => Timestamp,
        "timestamptz" => TimestampTz,
        "uuid" => Uuid,
        "json" | "jsonb" => Json,
        "bytea" | "blob" | "binary" | "varbinary" => Binary,
        "text" | "varchar" | "char" | "character" | "nvarchar" => Text,
        _ => Other,
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn parse(body: &str) -> Result<Incoming, Value> {
    let value: Value = serde_json::from_str(body).map_err(|e| {
        // A body that is not JSON cannot have an id, so the error carries `null`.
        error(Value::Null, -32700, &format!("parse error: {e}"))
    })?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(method) = method else {
        // A response from the client (to a server-initiated request). Nothing here sends one, so it
        // is ignored rather than answered.
        return Err(Value::Null);
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("id") {
        Some(id) => Ok(Incoming::Request {
            id: id.clone(),
            method,
            params,
        }),
        None => Ok(Incoming::Notification { method, params }),
    }
}

fn position_params(params: &Value) -> Option<(String, u32, u32)> {
    let uri = params.get("textDocument")?.get("uri")?.as_str()?.to_owned();
    let position = params.get("position")?;
    let line = u32::try_from(position.get("line")?.as_u64()?).ok()?;
    let character = u32::try_from(position.get("character")?.as_u64()?).ok()?;
    Some((uri, line, character))
}

fn open_params(params: &Value) -> Option<(String, i64, String)> {
    let document = params.get("textDocument")?;
    let uri = document.get("uri")?.as_str()?.to_owned();
    let version = document.get("version").and_then(Value::as_i64).unwrap_or(0);
    let text = document.get("text")?.as_str()?.to_owned();
    Some((uri, version, text))
}

/// LSP `(line, character)` to a byte offset. `character` counts UTF-16 code units, which is what
/// the spec says and what a JS client sends - not bytes and not chars.
pub fn offset_of(text: &str, line: u32, character: u32) -> usize {
    let mut offset = 0;
    for (index, current) in text.split('\n').enumerate() {
        if index == line as usize {
            // Walk the line in UTF-16 units.
            let mut units = 0u32;
            for (ix, ch) in current.char_indices() {
                if units >= character {
                    return offset + ix;
                }
                units += ch.len_utf16() as u32;
            }
            return offset + current.len();
        }
        offset += current.len() + 1;
    }
    text.len()
}

/// A byte offset back to LSP `(line, character)`, in UTF-16 units.
///
/// Public because any client of the analysis needs the same conversion: the GUI turns a completion's
/// byte range into an LSP `Range` with it, and doing that twice would be two chances to get UTF-16
/// wrong.
pub fn position_of(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (ix, ch) in text.char_indices() {
        if ix >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = ix + 1;
        }
    }
    let prefix = &text[line_start..offset];
    let character: u32 = prefix.chars().map(|c| c.len_utf16() as u32).sum();
    (line, character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fg_core::{DataType, TypeCategory};

    fn path(name: &str) -> ObjectPath {
        ObjectPath::new(Some("main"), name)
    }

    fn column(name: &str, native: &str, category: TypeCategory) -> ColumnMeta {
        ColumnMeta::new(name, DataType::new(native, category))
    }

    fn server() -> Server {
        Server::new(ServerConfig {
            kind: DbKind::Sqlite,
            objects: vec![path("items"), path("orders")],
            columns: vec![
                (
                    path("items"),
                    vec![
                        column("id", "INTEGER", TypeCategory::Integer),
                        column("name", "TEXT", TypeCategory::Text),
                    ],
                ),
                (
                    path("orders"),
                    vec![column("total", "DECIMAL", TypeCategory::Decimal)],
                ),
            ],
            reported: Vec::new(),
        })
    }

    fn request(server: &mut Server, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        server.handle(&body.to_string()).remove(0)
    }

    fn notify(server: &mut Server, method: &str, params: Value) -> Vec<Value> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        server.handle(&body.to_string())
    }

    fn open(server: &mut Server, uri: &str, text: &str) -> Vec<Value> {
        notify(
            server,
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "sql", "version": 1, "text": text } }),
        )
    }

    fn complete_at_position(
        server: &mut Server,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Vec<Value> {
        let response = request(
            server,
            "textDocument/completion",
            json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } }),
        );
        response["result"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn initialize_reports_what_the_server_can_do() {
        let mut server = server();
        let response = request(&mut server, "initialize", json!({ "capabilities": {} }));
        let result = &response["result"];
        assert_eq!(result["serverInfo"]["name"], "fastgrid-sql");
        assert_eq!(
            result["capabilities"]["textDocumentSync"]["change"], 1,
            "full sync"
        );
        assert!(
            result["capabilities"]["completionProvider"]["triggerCharacters"]
                .as_array()
                .is_some_and(|t| t.iter().any(|c| c == ".")),
            "`.` triggers a column list"
        );
    }

    #[test]
    fn a_request_before_initialize_is_refused() {
        let mut server = server();
        let response = request(
            &mut server,
            "textDocument/completion",
            json!({ "textDocument": { "uri": "file:///a.sql" }, "position": { "line": 0, "character": 0 } }),
        );
        assert_eq!(response["error"]["code"], -32002);
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let response = request(&mut server, "textDocument/hover", json!({}));
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn a_body_that_is_not_json_is_a_parse_error() {
        let mut server = server();
        let response = server.handle("not json").remove(0);
        assert_eq!(response["error"]["code"], -32700);
        assert!(response["id"].is_null());
    }

    #[test]
    fn did_open_publishes_an_empty_diagnostic_list() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let published = open(&mut server, "file:///a.sql", "SELECT 1;");
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0]["method"], "textDocument/publishDiagnostics",
            "the client is told the document is clean"
        );
        assert_eq!(published[0]["params"]["diagnostics"], json!([]));
    }

    #[test]
    fn completion_comes_from_the_analysed_scope() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        open(&mut server, "file:///a.sql", "SELECT i. FROM items i");
        let items = complete_at_position(&mut server, "file:///a.sql", 0, 9);
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert_eq!(labels, vec!["i.id", "i.name"], "{items:?}");
        assert_eq!(items[0]["kind"], 5, "columns are LSP Field");
        assert_eq!(items[0]["detail"], "integer");
        // A `textEdit` over the typed word, not an `insertText` at the caret: accepting `id` after
        // typing `i.` must replace the empty word, not insert after it.
        let edit = &items[0]["textEdit"];
        assert_eq!(edit["newText"], "id");
        assert_eq!(edit["range"]["start"]["line"], 0);
        assert_eq!(
            edit["range"]["start"]["character"], 9,
            "the range starts where the word starts (after the dot)"
        );
        assert_eq!(edit["range"]["end"]["character"], 9, "and it is empty here");
        assert!(
            items[0].get("insertText").is_none(),
            "insertText would make the editor insert instead of replace"
        );
    }

    #[test]
    fn a_position_inside_a_multibyte_line_maps_to_the_right_bytes() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        // `中文` is two UTF-16 units per character; `character` counts those, not bytes.
        open(
            &mut server,
            "file:///a.sql",
            "SELECT '中文' FROM items i WHERE i.",
        );
        // Line 0, character = the UTF-16 length of the whole line: the caret is *after* the dot.
        let text = "SELECT '中文' FROM items i WHERE i.";
        let units: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
        let items = complete_at_position(&mut server, "file:///a.sql", 0, units);
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert_eq!(labels, vec!["i.id", "i.name"], "{items:?}");
    }

    #[test]
    fn accepting_a_completion_replaces_the_typed_prefix() {
        // The bug this pins: typing `se` and accepting `SELECT` used to produce `seSELECT`, because
        // the editor inserts `insertText` at the caret. The edit's range must cover `se`.
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let uri = "file:///a.sql";
        open(&mut server, uri, "se");
        let items = complete_at_position(&mut server, uri, 0, 2);
        let select = items
            .iter()
            .find(|i| i["label"] == "SELECT")
            .expect("the keyword is offered");
        let range = &select["textEdit"]["range"];
        assert_eq!(range["start"]["character"], 0, "from the start of `se`");
        assert_eq!(range["end"]["character"], 2, "to the caret");
        assert_eq!(select["textEdit"]["newText"], "SELECT");
    }

    #[test]
    fn completion_on_the_second_line_uses_that_line() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        open(
            &mut server,
            "file:///a.sql",
            "SELECT 1;\nSELECT o. FROM orders o",
        );
        let items = complete_at_position(&mut server, "file:///a.sql", 1, 9);
        assert_eq!(items[0]["label"], "o.total");
    }

    #[test]
    fn a_pushed_diagnostic_is_published_as_a_range() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let uri = "file:///a.sql";
        open(&mut server, uri, "SELECT 1;\nSELEC 2;");
        // The client ran the second statement and it failed at its byte 0.
        let diagnostic = crate::diagnostics::diagnostic_for(
            "SELECT 1;\nSELEC 2;",
            DbKind::Sqlite,
            1,
            &fg_core::QueryError {
                message: "near \"SELEC\"".into(),
                code: Some("1".into()),
                position: Some(0),
            },
        )
        .expect("a diagnostic");
        server.set_diagnostics(uri, vec![diagnostic]);
        let published = server.diagnostics_for(uri);
        let items = &published["params"]["diagnostics"];
        assert_eq!(items[0]["range"]["start"]["line"], 1, "the second line");
        assert_eq!(items[0]["range"]["start"]["character"], 0);
        assert_eq!(items[0]["range"]["end"]["character"], 5, "`SELEC`");
        assert_eq!(items[0]["severity"], 1, "an error");
        assert_eq!(items[0]["source"], "fastgrid");
    }

    #[test]
    fn editing_clears_a_stale_diagnostic() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let uri = "file:///a.sql";
        open(&mut server, uri, "SELEC 2;");
        server.set_diagnostics(
            uri,
            vec![Diagnostic {
                range: 0..5,
                message: "near SELEC".into(),
                code: None,
                statement: Some(0),
            }],
        );
        // The user fixed it.
        let published = notify(
            &mut server,
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{ "text": "SELECT 2;" }],
            }),
        );
        assert_eq!(
            published[0]["params"]["diagnostics"],
            json!([]),
            "the text the error described is gone"
        );
    }

    #[test]
    fn did_close_clears_the_document() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let uri = "file:///a.sql";
        open(&mut server, uri, "SELECT 1;");
        let published = notify(
            &mut server,
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
        assert_eq!(published[0]["params"]["diagnostics"], json!([]));
        // And the document is gone: completion now errors instead of answering from a stale buffer.
        let response = request(
            &mut server,
            "textDocument/completion",
            json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 0 } }),
        );
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn shutdown_then_exit_is_the_normal_end() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        let response = request(&mut server, "shutdown", Value::Null);
        assert!(response["result"].is_null());
        assert!(notify(&mut server, "exit", Value::Null).is_empty());
    }

    #[test]
    fn a_request_after_shutdown_is_refused() {
        let mut server = server();
        request(&mut server, "initialize", json!({}));
        request(&mut server, "shutdown", Value::Null);
        let response = request(&mut server, "textDocument/completion", json!({}));
        assert_eq!(response["error"]["code"], -32600);
    }

    #[test]
    fn positions_round_trip_through_utf16() {
        let text = "SELECT '中文' AS 名;\nSELECT 2;";
        for offset in 0..=text.len() {
            if !text.is_char_boundary(offset) {
                continue;
            }
            let (line, character) = position_of(text, offset);
            assert_eq!(offset_of(text, line, character), offset, "at {offset}");
        }
    }

    #[test]
    fn a_position_past_the_end_of_the_document_is_clamped() {
        assert_eq!(offset_of("SELECT 1;", 99, 99), 9);
        assert_eq!(offset_of("SELECT 1;", 0, 99), 9);
    }
}
