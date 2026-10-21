//! `fastgrid-cli` - the headless binary.
//!
//! Two things it does today: report its version, and serve the SQL language server over stdio.
//! The MCP server (roadmap M2.5) plugs in here later; it shares the same rule as this one, that the
//! process must work with no display attached.

use std::io;

use fg_lsp::Server;
use fg_lsp::protocol::ServerConfig;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("lsp") => {
            if let Err(e) = serve_lsp() {
                eprintln!("fastgrid-cli lsp: {e}");
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
