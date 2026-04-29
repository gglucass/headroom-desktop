/// Transparent HTTP proxy intercept layer.
///
/// Binds on 127.0.0.1:6767 (the address clients point at) and forwards every
/// request unchanged to 127.0.0.1:6768 (where headroom actually listens).
/// As each request passes through, any `Authorization: Bearer …` header is
/// captured into `AppState::claude_bearer_token` so the usage-stats feature
/// can call the Anthropic OAuth usage endpoint without touching the keychain.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::bearer::BearerToken;

pub const INTERCEPT_PORT: u16 = 6767;
pub const HEADROOM_BACKEND_PORT: u16 = 6768;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared state written by the intercept layer.
pub type SharedToken = Arc<Mutex<Option<BearerToken>>>;

/// When set to `true`, the intercept forwards traffic directly to
/// api.anthropic.com instead of the local Python proxy. Used to keep already-
/// running Claude Code sessions alive after the pricing gate has stopped the
/// Python proxy because the user crossed the free disable threshold.
pub type BypassFlag = Arc<AtomicBool>;

pub const ANTHROPIC_DIRECT_BASE: &str = "https://api.anthropic.com";

/// Spawn the intercept proxy as a background Tokio task.
/// Returns immediately; the server runs until the process exits.
/// Uses a dedicated OS thread with its own Tokio runtime so it's safe to call
/// from Tauri's `.setup()` before the main async runtime has started.
pub fn spawn(token_slot: SharedToken, bypass: BypassFlag) {
    let upstream_base = Arc::new(ANTHROPIC_DIRECT_BASE.to_string());
    std::thread::Builder::new()
        .name("proxy-intercept".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("proxy intercept runtime");
            rt.block_on(async move {
                let bind_addr: SocketAddr = ([127, 0, 0, 1], INTERCEPT_PORT).into();
                let backend_addr: SocketAddr = ([127, 0, 0, 1], HEADROOM_BACKEND_PORT).into();
                match run(bind_addr, backend_addr, token_slot, bypass, upstream_base).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                        // Port is already bound. If /health responds over HTTP, an
                        // existing Headroom proxy owns the port (single-instance
                        // plugin should normally prevent this, but a crashed or
                        // still-exiting prior process can leave it held). Treat
                        // that as benign. Otherwise the port is foreign and we
                        // escalate to Sentry.
                        if probe_existing_intercept().await {
                            eprintln!(
                                "[proxy_intercept] port {INTERCEPT_PORT} already owned by existing Headroom proxy; exiting thread"
                            );
                        } else {
                            eprintln!(
                                "[proxy_intercept] fatal: {e} (port {INTERCEPT_PORT} held by foreign process)"
                            );
                            sentry::capture_message(
                                &format!(
                                    "proxy_intercept fatal error: {e} (port {INTERCEPT_PORT} held by foreign process)"
                                ),
                                sentry::Level::Fatal,
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("[proxy_intercept] fatal: {e}");
                        sentry::capture_message(
                            &format!("proxy_intercept fatal error: {e}"),
                            sentry::Level::Fatal,
                        );
                    }
                }
            });
        })
        .expect("spawn proxy intercept thread");
}

async fn run(
    bind_addr: SocketAddr,
    backend_addr: SocketAddr,
    token_slot: SharedToken,
    bypass: BypassFlag,
    upstream_base: Arc<String>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;

    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let slot = token_slot.clone();
                let bypass = bypass.clone();
                let upstream_base = upstream_base.clone();
                tokio::spawn(handle(client, backend_addr, slot, bypass, upstream_base));
            }
            Err(e) => {
                // EMFILE/ENFILE/ECONNABORTED are transient — log and keep serving
                // so the proxy self-heals once FDs free up, instead of dying.
                eprintln!("[proxy_intercept] accept error: {e}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

async fn handle(
    mut client: TcpStream,
    backend_addr: SocketAddr,
    token_slot: SharedToken,
    bypass: BypassFlag,
    upstream_base: Arc<String>,
) {
    // Read only through the end of the HTTP headers. We only need headers to
    // capture the bearer token, and forwarding early avoids deadlocks with
    // `Expect: 100-continue` request flows.
    let mut buf = Vec::with_capacity(4096);
    match tokio::time::timeout(
        HEADER_READ_TIMEOUT,
        read_http_headers(&mut client, &mut buf),
    )
    .await
    {
        Ok(Ok(())) => {}
        _ => return,
    }

    // Reject requests that didn't target the loopback listener or that carry
    // a browser Origin. This blocks DNS-rebinding attacks where an attacker
    // page resolves its hostname to 127.0.0.1 and drives the intercept from
    // a user's browser; CLI clients never set Origin and always send a
    // loopback Host.
    if !request_is_loopback_safe(&buf) {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    // Scan headers for a Bearer token and capture it.
    if let Some(token) = extract_bearer(&buf) {
        *token_slot.lock() = Some(BearerToken::new(token));
    }

    // When the pricing gate has bypassed Headroom, the Python proxy on
    // `backend_addr` is intentionally stopped. Forward direct to Anthropic so
    // already-running CC sessions stay alive while optimization is off.
    if bypass.load(Ordering::Acquire) {
        forward_direct_to_anthropic(client, buf, &upstream_base).await;
        return;
    }

    // Forward to the headroom backend.
    let Ok(mut backend) = TcpStream::connect(backend_addr).await else {
        // headroom not up yet — send a 502 so the client gets a clean error.
        let _ = client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    };

    if backend.write_all(&buf).await.is_err() {
        return;
    }

    let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
}

static UPSTREAM_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn upstream_client() -> &'static reqwest::Client {
    UPSTREAM_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .build()
            .expect("reqwest client for bypass forwarder")
    })
}

/// Forward the request that produced `header_buf` directly to api.anthropic.com.
///
/// Used when the pricing gate has stopped the local Python proxy. The CC
/// session keeps speaking HTTP/1.1 to 127.0.0.1:6767; we re-issue the same
/// request to the real Anthropic endpoint over TLS with `reqwest`, then stream
/// the response back as HTTP/1.1 chunked transfer.
async fn forward_direct_to_anthropic(
    mut client: TcpStream,
    header_buf: Vec<u8>,
    upstream_base: &str,
) {
    let header_end = match find_header_end(&header_buf) {
        Some(pos) => pos + 4,
        None => {
            let _ = client
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };
    let leftover_body = &header_buf[header_end..];

    let Some(parsed) = parse_request_head(&header_buf[..header_end]) else {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    };

    // These paths are served by the local Python proxy, not Anthropic. In
    // bypass mode the proxy is intentionally down, so reply 503 instead of
    // forwarding upstream (which would either fail noisily or, worse, hit a
    // real Anthropic endpoint that happens to share the path).
    // Denylist (not allowlist) so future Anthropic API versions like /v2/*
    // continue to forward automatically without requiring a desktop update.
    if is_local_proxy_path(&parsed.path) {
        let _ = client
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    let body = match parsed.content_length {
        Some(total) if total > leftover_body.len() => {
            let mut body = Vec::with_capacity(total);
            body.extend_from_slice(leftover_body);
            let mut remaining = vec![0u8; total - leftover_body.len()];
            if client.read_exact(&mut remaining).await.is_err() {
                return;
            }
            body.extend_from_slice(&remaining);
            body
        }
        Some(total) => leftover_body[..total.min(leftover_body.len())].to_vec(),
        None => leftover_body.to_vec(),
    };

    let url = format!("{}{}", upstream_base, parsed.path);
    let method = match reqwest::Method::from_bytes(parsed.method.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            let _ = client
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    let mut req = upstream_client().request(method, &url);
    for (name, value) in &parsed.headers {
        if is_hop_by_hop_request_header(name) {
            continue;
        }
        req = req.header(name, value);
    }
    if !body.is_empty() {
        req = req.body(body);
    }

    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[proxy_intercept] bypass forward failed: {e}");
            sentry::capture_message(
                &format!("proxy_intercept bypass forward failed: {e}"),
                sentry::Level::Warning,
            );
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        resp.status().as_u16(),
        resp.status().canonical_reason().unwrap_or("")
    );
    for (name, value) in resp.headers().iter() {
        if is_hop_by_hop_response_header(name.as_str()) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), v));
        }
    }
    head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    if client.write_all(head.as_bytes()).await.is_err() {
        return;
    }

    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) if !bytes.is_empty() => {
                let header = format!("{:X}\r\n", bytes.len());
                if client.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                if client.write_all(&bytes).await.is_err() {
                    return;
                }
                if client.write_all(b"\r\n").await.is_err() {
                    return;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                eprintln!("[proxy_intercept] bypass body stream error: {e}");
                return;
            }
        }
    }
    let _ = client.write_all(b"0\r\n\r\n").await;
}

struct ParsedRequestHead {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    content_length: Option<usize>,
}

fn parse_request_head(buf: &[u8]) -> Option<ParsedRequestHead> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        }
        headers.push((name, value));
    }
    Some(ParsedRequestHead {
        method,
        path,
        headers,
        content_length,
    })
}

fn is_hop_by_hop_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailers"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "upgrade"
            | "host"
            | "content-length"
            | "accept-encoding"
    )
}

fn is_hop_by_hop_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailers"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "upgrade"
            | "content-length"
            | "content-encoding"
    )
}

/// Return true if something at 127.0.0.1:INTERCEPT_PORT answers /health with a
/// response that begins with `HTTP/` — that matches both our intercept (which
/// forwards to the python backend and may return 200 or 502) and no realistic
/// foreign process we expect to encounter on this port.
async fn probe_existing_intercept() -> bool {
    let connect = TcpStream::connect(("127.0.0.1", INTERCEPT_PORT));
    let Ok(Ok(mut stream)) = tokio::time::timeout(PROBE_TIMEOUT, connect).await else {
        return false;
    };
    let req = b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(req).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 16];
    let Ok(Ok(n)) = tokio::time::timeout(PROBE_TIMEOUT, stream.read(&mut buf)).await else {
        return false;
    };
    buf.get(..n).is_some_and(|b| b.starts_with(b"HTTP/"))
}

/// Read through the end of the HTTP headers from `stream` into `buf`.
///
/// Forwarding immediately after the header block is enough for token capture
/// and avoids hanging on protocols that wait for a `100 Continue` response
/// before sending the request body.
async fn read_http_headers<R>(stream: &mut R, buf: &mut Vec<u8>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut tmp = [0u8; 4096];

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed connection",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);

        if find_header_end(buf).is_some() {
            return Ok(());
        }

        if buf.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers exceed maximum size",
            ));
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Paths served by the local Python proxy (not Anthropic). Matches the prefix
/// so sub-paths (e.g. `/transformations/feed`) and query strings are covered,
/// while preventing partial matches (e.g. `/healthcheck` does not match
/// `/health`).
fn is_local_proxy_path(path: &str) -> bool {
    const LOCAL_PREFIXES: &[&str] = &[
        "/readyz",
        "/livez",
        "/health",
        "/stats",
        "/transformations",
        "/dashboard",
        "/debug",
        "/subscription-window",
        "/quota",
        "/metrics",
        "/cache",
    ];
    LOCAL_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('?'))
    })
}

/// Return true if the request's Host header targets the loopback listener
/// and no browser Origin header is present. Protects against DNS-rebinding
/// attacks that aim the user's browser at 127.0.0.1 via an attacker domain.
fn request_is_loopback_safe(buf: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(buf) else {
        return false;
    };
    let mut host: Option<&str> = None;
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("origin:") {
            return false;
        }
        if host.is_none() && lower.starts_with("host:") {
            host = Some(line["host:".len()..].trim());
        }
    }
    match host {
        Some(value) => host_is_loopback(value),
        None => false,
    }
}

fn host_is_loopback(host: &str) -> bool {
    let name = host
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .trim_start_matches('[')
        .trim_end_matches(']');
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

/// Extract the bearer token value from raw HTTP request bytes, if present.
fn extract_bearer(buf: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(buf).ok()?;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("authorization:") {
            if let Some(_) = rest.trim().strip_prefix("bearer ") {
                // Find "bearer " in the original line (case-insensitive) and
                // return the token with its original casing intact.
                let bearer_pos = lower.find("bearer ").unwrap_or(0) + 7;
                return Some(line[bearer_pos..].trim().to_string());
            }
            // x-api-key style — not usable for the OAuth usage endpoint.
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        extract_bearer, find_header_end, is_hop_by_hop_request_header,
        is_hop_by_hop_response_header, parse_request_head, read_http_headers,
        request_is_loopback_safe, run, BypassFlag, SharedToken,
    };
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use parking_lot::Mutex;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{timeout, Duration};

    #[test]
    fn finds_header_boundary() {
        let request = b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\n\r\n{\"x\":1}";
        assert_eq!(find_header_end(request), Some(43));
    }

    #[test]
    fn extracts_bearer_token_case_insensitively() {
        let request = b"POST / HTTP/1.1\r\nAuthorization: Bearer test-token\r\n\r\n";
        assert_eq!(extract_bearer(request).as_deref(), Some("test-token"));
    }

    #[test]
    fn loopback_host_without_origin_is_accepted() {
        let req = b"POST / HTTP/1.1\r\nHost: 127.0.0.1:6767\r\n\r\n";
        assert!(request_is_loopback_safe(req));
        let req = b"POST / HTTP/1.1\r\nHost: localhost:6767\r\n\r\n";
        assert!(request_is_loopback_safe(req));
        let req = b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert!(request_is_loopback_safe(req));
    }

    #[test]
    fn non_loopback_host_is_rejected() {
        let req = b"POST / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n";
        assert!(!request_is_loopback_safe(req));
        let req = b"POST / HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n";
        assert!(!request_is_loopback_safe(req));
    }

    #[test]
    fn origin_header_causes_rejection_even_on_loopback() {
        let req =
            b"POST / HTTP/1.1\r\nHost: 127.0.0.1:6767\r\nOrigin: https://evil.example.com\r\n\r\n";
        assert!(!request_is_loopback_safe(req));
    }

    #[test]
    fn missing_host_header_is_rejected() {
        let req = b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        assert!(!request_is_loopback_safe(req));
    }

    #[tokio::test]
    async fn header_read_does_not_wait_for_continue_body() {
        let (mut client, mut server_stream) = duplex(1024);

        let writer = tokio::spawn(async move {
            client
                .write_all(
                    b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n",
                )
                .await
                .expect("write headers");
        });

        let mut buf = Vec::new();
        timeout(
            Duration::from_millis(250),
            read_http_headers(&mut server_stream, &mut buf),
        )
        .await
        .expect("headers should complete without waiting for body")
        .expect("header read succeeds");

        assert!(buf.windows(4).any(|window| window == b"\r\n\r\n"));
        writer.await.expect("writer task");
    }

    /// Bind a fresh `TcpListener` on an ephemeral port and return its address.
    async fn bind_ephemeral() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        (listener, addr)
    }

    /// Read header bytes from `stream` up through (and including) the `\r\n\r\n`
    /// boundary so the test can assert what the intercept forwarded.
    async fn read_until_header_end(stream: &mut TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        for _ in 0..32 {
            let n = stream.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        buf
    }

    #[tokio::test]
    async fn intercept_captures_bearer_and_forwards_headers_to_backend() {
        // Fake backend: accept one connection, read its header block, hold the
        // connection open long enough for the test to inspect what arrived.
        let (backend_listener, backend_addr) = bind_ephemeral().await;
        let backend_task = tokio::spawn(async move {
            let (mut sock, _) = backend_listener.accept().await.expect("backend accept");
            let received = read_until_header_end(&mut sock).await;
            // Send a stub response so the client side of copy_bidirectional has
            // something to consume.
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            received
        });

        // Run the intercept on its own ephemeral port.
        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0").await.expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener); // free the port; run() rebinds the same one
        let slot_for_run = token_slot.clone();
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        let upstream_base = Arc::new("https://api.anthropic.com".to_string());
        let run_task = tokio::spawn(async move {
            // run() loops forever; the test cancels it via abort below.
            let _ = run(
                intercept_addr,
                backend_addr,
                slot_for_run,
                bypass_for_run,
                upstream_base,
            )
            .await;
        });

        // Give run() a moment to bind. A brief retry loop on connect is more
        // reliable than a fixed sleep, since CI can be slow.
        let mut client = None;
        for _ in 0..50 {
            if let Ok(c) = TcpStream::connect(intercept_addr).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("intercept reachable");

        let request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer test-token-123\r\nContent-Length: 0\r\n\r\n",
            intercept_addr.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");

        let received = timeout(Duration::from_secs(2), backend_task)
            .await
            .expect("backend forwarded request in time")
            .expect("backend task ok");

        // Headers should have been forwarded verbatim — including the Bearer.
        let received_str = std::str::from_utf8(&received).expect("utf8");
        assert!(
            received_str.contains("POST /v1/messages HTTP/1.1"),
            "request line forwarded: {received_str:?}"
        );
        assert!(
            received_str.contains("Authorization: Bearer test-token-123"),
            "bearer header forwarded: {received_str:?}"
        );

        // The bearer token should have been captured into the shared slot.
        let captured = token_slot.lock().clone();
        let bearer = captured.expect("bearer captured");
        // BearerToken stores its value but doesn't expose it directly — verify
        // via value_if_fresh with a generous TTL.
        assert_eq!(
            bearer
                .value_if_fresh(Duration::from_secs(60))
                .map(|s| s.to_string()),
            Some("test-token-123".to_string())
        );

        run_task.abort();
    }

    #[tokio::test]
    async fn intercept_returns_502_when_backend_is_unreachable() {
        // Pick a backend port that nothing is listening on. Bind+immediately
        // drop a listener to grab a free port, then connect attempts will fail.
        let (probe, dead_backend_addr) = bind_ephemeral().await;
        drop(probe);

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0").await.expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let slot_for_run = token_slot.clone();
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        let upstream_base = Arc::new("https://api.anthropic.com".to_string());
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                dead_backend_addr,
                slot_for_run,
                bypass_for_run,
                upstream_base,
            )
            .await;
        });

        let mut client = None;
        for _ in 0..50 {
            if let Ok(c) = TcpStream::connect(intercept_addr).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("intercept reachable");

        let request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\n\r\n",
            intercept_addr.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");

        let mut response = Vec::new();
        let mut tmp = [0u8; 256];
        let _ = timeout(Duration::from_secs(2), async {
            loop {
                let n = client.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&tmp[..n]);
                if response.len() >= 16 {
                    break;
                }
            }
        })
        .await;
        let response_str = std::str::from_utf8(&response).unwrap_or("");
        assert!(
            response_str.starts_with("HTTP/1.1 502"),
            "expected 502 Bad Gateway, got: {response_str:?}"
        );

        run_task.abort();
    }

    #[test]
    fn parse_request_head_extracts_method_path_and_content_length() {
        let buf = b"POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:6767\r\nAuthorization: Bearer abc\r\nContent-Length: 42\r\n\r\n";
        let parsed = parse_request_head(buf).expect("parsed");
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/v1/messages");
        assert_eq!(parsed.content_length, Some(42));
        assert!(parsed
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer abc"));
    }

    #[test]
    fn parse_request_head_handles_missing_content_length() {
        let buf = b"GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let parsed = parse_request_head(buf).expect("parsed");
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/v1/models");
        assert_eq!(parsed.content_length, None);
    }

    #[test]
    fn parse_request_head_returns_none_for_garbage() {
        // Only one token before \r\n -> no path -> None.
        let buf = b"NOTHTTP\r\n\r\n";
        assert!(parse_request_head(buf).is_none());
    }

    #[test]
    fn hop_by_hop_request_header_recognises_canonical_names() {
        for name in [
            "Connection",
            "keep-alive",
            "TRANSFER-ENCODING",
            "te",
            "trailers",
            "Proxy-Authorization",
            "Upgrade",
            "Host",
            "Content-Length",
            "Accept-Encoding",
        ] {
            assert!(
                is_hop_by_hop_request_header(name),
                "{name} should be hop-by-hop on the request side"
            );
        }
        // Headers we want to forward must NOT be flagged.
        for name in ["Authorization", "anthropic-version", "x-api-key", "Content-Type"] {
            assert!(
                !is_hop_by_hop_request_header(name),
                "{name} must be forwarded"
            );
        }
    }

    #[test]
    fn hop_by_hop_response_header_recognises_canonical_names() {
        for name in [
            "Connection",
            "Keep-Alive",
            "transfer-encoding",
            "Content-Length",
            "Content-Encoding",
        ] {
            assert!(
                is_hop_by_hop_response_header(name),
                "{name} should be hop-by-hop on the response side"
            );
        }
        for name in [
            "Content-Type",
            "anthropic-ratelimit-requests-remaining",
            "x-request-id",
        ] {
            assert!(
                !is_hop_by_hop_response_header(name),
                "{name} must be forwarded"
            );
        }
    }

    /// Drive the bypass branch end-to-end: intercept on :6767 with bypass=true
    /// forwards a request to a fake upstream, then streams the upstream's
    /// response back to the client as HTTP/1.1 chunked transfer.
    #[tokio::test]
    async fn bypass_forwards_request_to_upstream_and_streams_response_back() {
        let (upstream_listener, upstream_addr) = bind_ephemeral().await;
        let upstream_base = format!("http://127.0.0.1:{}", upstream_addr.port());

        let upstream_task = tokio::spawn(async move {
            let (mut sock, _) = upstream_listener.accept().await.expect("upstream accept");
            // Read until headers + content-length body have arrived.
            let mut received = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end: Option<usize> = None;
            let mut content_length: usize = 0;
            for _ in 0..256 {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(pos) = find_header_end(&received) {
                        header_end = Some(pos + 4);
                        let header_text = std::str::from_utf8(&received[..pos]).unwrap_or("");
                        for line in header_text.lines() {
                            let lower = line.to_ascii_lowercase();
                            if let Some(rest) = lower.strip_prefix("content-length:") {
                                content_length = rest.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                }
                if let Some(end) = header_end {
                    if received.len() >= end + content_length {
                        break;
                    }
                }
            }
            // Reply with a small SSE-style payload over Content-Length so
            // reqwest can fully consume the response.
            let body = b"event: message\ndata: hi\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nx-request-id: req-test-1\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.write_all(body).await;
            let _ = sock.shutdown().await;
            received
        });

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0").await.expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let bypass: BypassFlag = Arc::new(AtomicBool::new(true));
        let backend_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let upstream_base_arc = Arc::new(upstream_base);
        let token_for_run = token_slot.clone();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                backend_addr,
                token_for_run,
                bypass,
                upstream_base_arc,
            )
            .await;
        });

        let mut client = None;
        for _ in 0..50 {
            if let Ok(c) = TcpStream::connect(intercept_addr).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("intercept reachable");

        let req_body = br#"{"model":"claude"}"#;
        let request_head = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer test-bypass-token\r\nContent-Type: application/json\r\nAccept-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            intercept_addr.port(),
            req_body.len()
        );
        client
            .write_all(request_head.as_bytes())
            .await
            .expect("write headers");
        client.write_all(req_body).await.expect("write body");

        let received = timeout(Duration::from_secs(5), upstream_task)
            .await
            .expect("upstream got request in time")
            .expect("upstream task ok");
        let received_str = std::str::from_utf8(&received).expect("utf8");

        assert!(
            received_str.starts_with("POST /v1/messages HTTP/1.1"),
            "request line forwarded verbatim: {received_str:?}"
        );
        let received_lower = received_str.to_ascii_lowercase();
        assert!(
            received_lower.contains("authorization: bearer test-bypass-token"),
            "Authorization forwarded: {received_str:?}"
        );
        assert!(
            received_lower.contains("content-type: application/json"),
            "Content-Type forwarded: {received_str:?}"
        );
        // Hop-by-hop request headers must be stripped before reaching upstream.
        assert!(
            !received_lower.contains("accept-encoding:"),
            "Accept-Encoding must be stripped: {received_str:?}"
        );
        // Body forwarded.
        assert!(
            received_str.contains(r#"{"model":"claude"}"#),
            "request body forwarded: {received_str:?}"
        );
        // Bearer captured into the shared slot.
        assert!(token_slot.lock().is_some(), "bearer was captured");

        // Now read the response the intercept relayed back to the client.
        let mut response = Vec::new();
        let mut tmp = [0u8; 4096];
        let _ = timeout(Duration::from_secs(5), async {
            for _ in 0..256 {
                let n = client.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&tmp[..n]);
                // Stop once the chunked terminator has arrived.
                if response.windows(5).any(|w| w == b"0\r\n\r\n") {
                    break;
                }
            }
        })
        .await;
        let response_str = std::str::from_utf8(&response).expect("utf8");

        assert!(
            response_str.starts_with("HTTP/1.1 200"),
            "response status forwarded: {response_str:?}"
        );
        let response_lower = response_str.to_ascii_lowercase();
        assert!(
            response_lower.contains("transfer-encoding: chunked"),
            "intercept rewrote response as chunked: {response_str:?}"
        );
        // Content-Length must have been stripped — replaced by chunked framing.
        assert!(
            !response_lower.contains("content-length:"),
            "Content-Length stripped on response: {response_str:?}"
        );
        // Forwarded response headers preserved.
        assert!(
            response_lower.contains("x-request-id: req-test-1"),
            "non-hop-by-hop response header forwarded: {response_str:?}"
        );
        // Body present somewhere in the chunked stream.
        assert!(
            response_str.contains("event: message"),
            "response body forwarded: {response_str:?}"
        );
        assert!(
            response_str.contains("data: hi"),
            "response body forwarded: {response_str:?}"
        );
        // Chunked terminator at the end.
        assert!(
            response_str.contains("0\r\n\r\n"),
            "chunked terminator written: {response_str:?}"
        );

        run_task.abort();
    }

    #[tokio::test]
    async fn bypass_returns_502_when_upstream_unreachable() {
        // Bind+drop to grab a free port nothing is listening on.
        let (probe, dead_addr) = bind_ephemeral().await;
        drop(probe);
        let upstream_base = format!("http://127.0.0.1:{}", dead_addr.port());

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0").await.expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let bypass: BypassFlag = Arc::new(AtomicBool::new(true));
        let backend_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let upstream_base_arc = Arc::new(upstream_base);
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                backend_addr,
                token_slot,
                bypass,
                upstream_base_arc,
            )
            .await;
        });

        let mut client = None;
        for _ in 0..50 {
            if let Ok(c) = TcpStream::connect(intercept_addr).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("intercept reachable");
        let request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\n\r\n",
            intercept_addr.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");

        let mut response = Vec::new();
        let mut tmp = [0u8; 256];
        let _ = timeout(Duration::from_secs(5), async {
            loop {
                let n = client.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&tmp[..n]);
                if response.len() >= 16 {
                    break;
                }
            }
        })
        .await;
        let response_str = std::str::from_utf8(&response).unwrap_or("");
        assert!(
            response_str.starts_with("HTTP/1.1 502"),
            "expected 502 when upstream unreachable, got: {response_str:?}"
        );

        run_task.abort();
    }
}
