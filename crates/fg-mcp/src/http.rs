//! The local Streamable HTTP transport: plain JSON responses, no SSE.
//!
//! Streamable HTTP only needs server-sent events for server-initiated
//! streams; fastgrid has none, so every request gets a plain JSON
//! response, which the spec allows and the official SDK accepts.
//!
//! The server binds loopback only and checks a bearer token on every
//! request. The token lives in the store (`McpHttpSettings`) and is
//! read fresh per request, so rotating it in the GUI takes effect
//! without restarting the server. One request per connection, then
//! close: no keep-alive framing to get wrong.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use fg_core::McpHttpSettings;
use fg_service::Service;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::server::McpServer;

const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 4 * 1024 * 1024;

/// A running loopback MCP server. `stop` aborts the accept loop;
/// in-flight requests finish on their own.
pub struct HttpServer {
    pub addr: SocketAddr,
    pub token: String,
    task: JoinHandle<()>,
}

impl HttpServer {
    pub fn stop(&self) {
        self.task.abort();
    }
}

/// Start the loopback server. `port` 0 picks a free port. The bound
/// port is written back to settings so the UI can show the real URL;
/// a missing token is generated and saved on first start.
pub async fn serve_http(service: Service, port: u16) -> io::Result<HttpServer> {
    let mut settings: McpHttpSettings = service
        .setting()
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    if settings.token.is_none() {
        settings.token = Some(new_token()?);
        service
            .set_setting(settings.clone())
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    let token = settings.token.clone().unwrap_or_default();
    let listener = TcpListener::bind((std::net::Ipv4Addr::new(127, 0, 0, 1), port)).await?;
    let addr = listener.local_addr()?;
    if settings.port != Some(addr.port()) {
        settings.port = Some(addr.port());
        service
            .set_setting(settings.clone())
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    let server = Arc::new(McpServer::new(service));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(server, stream).await {
                    let _ = e;
                }
            });
        }
    });
    Ok(HttpServer { addr, token, task })
}

/// Replace the stored bearer token and return the new one. The
/// running server reads the token per request, so rotation needs no
/// restart; a token issued before rotation stops working at once.
pub async fn rotate_http_token(service: &Service) -> io::Result<String> {
    let mut settings: McpHttpSettings = service
        .setting()
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    let token = new_token()?;
    settings.token = Some(token.clone());
    service
        .set_setting(settings)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(token)
}

/// 32 random bytes as hex. `getrandom` is the OS source on every
/// platform fastgrid targets; a failure fails the start rather than
/// minting a guessable token.
fn new_token() -> io::Result<String> {
    let bytes = fg_crypto::random_bytes::<32>().map_err(|e| io::Error::other(e.to_string()))?;
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

fn new_session_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("fg-{n:x}-{nanos:x}")
}

/// Compare without early exit: a local token still should not leak
/// its prefix through timing to another local process.
fn bearer_matches(expected: &str, presented: &str) -> bool {
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_HEADERS + MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        if let Some(pos) = find_subslice(&raw, b"\r\n\r\n") {
            break pos + 4;
        }
        if raw.len() > MAX_HEADERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let path = target.split('?').next().unwrap_or("").to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let mut request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    // A client that announces `Expect: 100-continue` waits for it
    // before sending the body; answer, then read as usual.
    if request
        .header("expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
    }
    let length: usize = request
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = raw[header_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(length);
    request.body = body;
    Ok(request)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    session: Option<&str>,
    body: &str,
) -> io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(session) = session {
        out.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

async fn handle_connection(server: Arc<McpServer>, mut stream: TcpStream) -> io::Result<()> {
    let request = read_request(&mut stream).await?;
    if request.path != "/mcp" {
        return write_response(
            &mut stream,
            404,
            "Not Found",
            None,
            "{\"error\":\"not found\"}",
        )
        .await;
    }
    if request.method != "POST" {
        // GET on /mcp would be an SSE stream; fastgrid sends none.
        return write_response(
            &mut stream,
            405,
            "Method Not Allowed",
            None,
            "{\"error\":\"only POST is supported\"}",
        )
        .await;
    }
    // The token is read per request, so a rotation lands immediately.
    let settings: McpHttpSettings = server
        .service()
        .setting()
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    let expected = settings.token.unwrap_or_default();
    let presented = request
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if expected.is_empty() || !bearer_matches(&expected, presented) {
        return write_response(
            &mut stream,
            401,
            "Unauthorized",
            None,
            "{\"error\":\"a valid bearer token is required\"}",
        )
        .await;
    }
    // Session: the client's id when it sends one, a fresh one for an
    // initialize without one, and empty otherwise (calls then fail
    // with "initialize first", which is the honest answer for a
    // session with no identity).
    let mut session = request.header("mcp-session-id").unwrap_or("").to_owned();
    let body = String::from_utf8_lossy(&request.body).into_owned();
    let is_initialize = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
        .is_some_and(|m| m == "initialize");
    if session.is_empty() && is_initialize {
        session = new_session_id();
    }
    let Some(reply) = server.handle(&session, &body).await else {
        // A notification: no JSON-RPC reply by spec, so 202 with an
        // empty body rather than a fabricated error object.
        stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    };
    let session_header = if session.is_empty() {
        None
    } else {
        Some(session.as_str())
    };
    write_response(&mut stream, 200, "OK", session_header, &reply).await
}
