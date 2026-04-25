/// Transparent HTTP proxy intercept layer.
///
/// Binds on 127.0.0.1:6767 (the address clients point at) and forwards every
/// request unchanged to 127.0.0.1:6768 (where headroom actually listens).
/// As each request passes through, any `Authorization: Bearer …` header is
/// captured into `AppState::claude_bearer_token` so the usage-stats feature
/// can call the Anthropic OAuth usage endpoint without touching the keychain.
use std::net::SocketAddr;
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

/// Spawn the intercept proxy as a background Tokio task.
/// Returns immediately; the server runs until the process exits.
/// Uses a dedicated OS thread with its own Tokio runtime so it's safe to call
/// from Tauri's `.setup()` before the main async runtime has started.
pub fn spawn(token_slot: SharedToken) {
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
                match run(bind_addr, backend_addr, token_slot).await {
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
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;

    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let slot = token_slot.clone();
                tokio::spawn(handle(client, backend_addr, slot));
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

async fn handle(mut client: TcpStream, backend_addr: SocketAddr, token_slot: SharedToken) {
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
        extract_bearer, find_header_end, read_http_headers, request_is_loopback_safe, run,
        SharedToken,
    };
    use std::net::SocketAddr;
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
        let run_task = tokio::spawn(async move {
            // run() loops forever; the test cancels it via abort below.
            let _ = run(intercept_addr, backend_addr, slot_for_run).await;
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
        let run_task = tokio::spawn(async move {
            let _ = run(intercept_addr, dead_backend_addr, slot_for_run).await;
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
}
