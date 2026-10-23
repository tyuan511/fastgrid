//! `fastgrid-cli mcp --stdio` as a process: newline-delimited JSON in, newline-delimited JSON out,
//! nothing else on stdout. The policy itself is covered in `fg-mcp`; this pins the transport.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn the_stdio_server_speaks_one_json_message_per_line() {
    let dir = std::env::temp_dir().join(format!("fg-cli-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fastgrid-cli"))
        .args(["mcp", "--stdio"])
        .env("FASTGRID_DATA_DIR", &dir)
        .env("FASTGRID_MEMORY_KEYCHAIN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut send = |message: Value| {
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    };
    let mut read = || -> Value {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("stdout carries only JSON lines")
    };

    send(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"clientInfo":{"name":"process-test"}}}));
    let init = read();
    assert_eq!(init["result"]["serverInfo"]["name"], "fastgrid");

    // A notification produces no reply line; the next reply must be for the next request.
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"list_connections","arguments":{}}}));
    let listed = read();
    assert_eq!(listed["id"], 2);
    assert_eq!(listed["result"]["isError"], false);

    drop(send);
    drop(read);
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "closing stdin is a clean exit");
    let _ = std::fs::remove_dir_all(&dir);
}
