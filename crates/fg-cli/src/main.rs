//! `fastgrid-cli` - the headless binary.
//!
//! What it does: report its version, serve the SQL language server over stdio, and serve the
//! read-only MCP server over stdio. Both share one rule: the process must work with no display
//! attached, and nothing but protocol bytes may reach stdout.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use fg_lsp::Server;
use fg_lsp::protocol::ServerConfig;
use fg_mcp::McpServer;
use fg_service::{KeychainKeyProvider, MemoryBackend, Service, ServiceOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("lsp") => {
            if let Err(e) = serve_lsp() {
                eprintln!("fastgrid-cli lsp: {e}");
                std::process::exit(1);
            }
        }
        Some("mcp") => {
            if args.next().as_deref() != Some("--stdio") {
                eprintln!("fastgrid-cli mcp: only `--stdio` is supported; try --help");
                std::process::exit(2);
            }
            if let Err(e) = serve_mcp() {
                eprintln!("fastgrid-cli mcp: {e}");
                std::process::exit(1);
            }
        }
        Some("--version") | Some("-V") => {
            println!("fastgrid-cli {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") | None => {
            println!(
                "fastgrid-cli {}\n\n\
                 Usage:\n  \
                 fastgrid-cli lsp        Serve the SQL language server over stdio (LSP)\n  \
                 fastgrid-cli mcp --stdio  Serve read-only MCP over stdio\n  \
                 fastgrid-cli --version  Print the version\n",
                env!("CARGO_PKG_VERSION")
            );
        }
        Some(other) => {
            eprintln!("fastgrid-cli: unknown command `{other}`; try --help");
            std::process::exit(2);
        }
    }
}

/// Read LSP messages from stdin and write responses to stdout until the client closes the stream.
///
/// Nothing but protocol bytes goes to stdout: a stray `println!` would corrupt the stream, so
/// diagnostics about the server itself go to stderr.
fn serve_lsp() -> io::Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut server = Server::new(ServerConfig::default());
    while let Some(message) = fg_lsp::read_message(&mut input)? {
        let responses = server.handle(&message.body);
        let exit = server.exit_requested();
        for response in responses {
            // A message that cannot be serialised is a bug here, not a client problem; report it
            // and keep the stream alive rather than tearing the session down.
            match serde_json::to_string(&response) {
                Ok(body) => fg_lsp::write_message(&mut output, &body)?,
                Err(e) => eprintln!("fastgrid-cli lsp: could not encode a response: {e}"),
            }
        }
        // `exit` is the client saying it is done; waiting for stdin to close would hang a client
        // that keeps the pipe open.
        if exit {
            break;
        }
    }
    Ok(())
}

/// Same location as the GUI, so both see one store. `FASTGRID_DATA_DIR` overrides it.
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FASTGRID_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("fastgrid")
}

/// Serve MCP over stdio: one JSON-RPC message per line in, one per line out.
///
/// Every connection is opened force-read-only (engine session and classification), and refusals
/// are audited. Requests are handled one at a time; a client that wants to cancel a long query
/// relies on the statement timeout rather than a mid-call cancel.
fn serve_mcp() -> io::Result<()> {
    let mut options = ServiceOptions::new(data_dir().join("store.db"));
    options.force_read_only = true;
    // Development and tests only, mirroring the GUI: the key then lives in this process alone.
    let started = if std::env::var_os("FASTGRID_MEMORY_KEYCHAIN").is_some() {
        Service::start(
            options,
            &KeychainKeyProvider::with_backend(MemoryBackend::default()),
        )
    } else {
        Service::start(options, &KeychainKeyProvider::os())
    };
    let service = started.map_err(|e| io::Error::other(e.to_string()))?;
    let server = McpServer::new(service);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = runtime.block_on(server.handle(&line)) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}
