/// Transparent HTTP proxy intercept layer.
///
/// Binds on 127.0.0.1:6767 (the address clients point at) and forwards every
/// request unchanged to 127.0.0.1:6768 (where headroom actually listens).
/// As each request passes through, any `Authorization: Bearer …` header is
/// captured into `AppState::claude_bearer_token` so the usage-stats feature
/// can call the Anthropic OAuth usage endpoint without touching the keychain.
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const INTERCEPT_PORT: u16 = 6767;
pub const HEADROOM_BACKEND_PORT: u16 = 6768;

/// Shared state written by the intercept layer.
pub type SharedToken = Arc<Mutex<Option<String>>>;

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
                if let Err(e) = run(token_slot).await {
                    eprintln!("[proxy_intercept] fatal: {e}");
                    sentry::capture_message(
                        &format!("proxy_intercept fatal error: {e}"),
                        sentry::Level::Fatal,
                    );
                }
            });
        })
        .expect("spawn proxy intercept thread");
}

async fn run(token_slot: SharedToken) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", INTERCEPT_PORT)).await?;

    loop {
        let (client, _) = listener.accept().await?;
        let slot = token_slot.clone();
        tokio::spawn(handle(client, slot));
    }
}

async fn handle(mut client: TcpStream, token_slot: SharedToken) {
    // Read only through the end of the HTTP headers. We only need headers to
    // capture the bearer token, and forwarding early avoids deadlocks with
    // `Expect: 100-continue` request flows.
    let mut buf = Vec::with_capacity(4096);
    if read_http_headers(&mut client, &mut buf).await.is_err() {
        return;
    }

    // Scan headers for a Bearer token and capture it.
    if let Some(token) = extract_bearer(&buf) {
        if let Ok(mut slot) = token_slot.lock() {
            *slot = Some(token);
        }
    }

    // Forward to the headroom backend.
    let Ok(mut backend) = TcpStream::connect(("127.0.0.1", HEADROOM_BACKEND_PORT)).await else {
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
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
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
    use super::{extract_bearer, find_header_end, read_http_headers};
    use tokio::io::{duplex, AsyncWriteExt};
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
}
