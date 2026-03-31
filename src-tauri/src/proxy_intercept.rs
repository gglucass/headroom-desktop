/// Transparent HTTP proxy intercept layer.
///
/// Binds on 127.0.0.1:6767 (the address clients point at) and forwards every
/// request unchanged to 127.0.0.1:6768 (where headroom actually listens).
/// As each request passes through, any `Authorization: Bearer …` header is
/// captured into `AppState::claude_bearer_token` so the usage-stats feature
/// can call the Anthropic OAuth usage endpoint without touching the keychain.
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
                }
            });
        })
        .expect("spawn proxy intercept thread");
}

async fn run(token_slot: SharedToken) -> std::io::Result<()> {
    let listener =
        TcpListener::bind(("127.0.0.1", INTERCEPT_PORT)).await?;

    loop {
        let (client, _) = listener.accept().await?;
        let slot = token_slot.clone();
        tokio::spawn(handle(client, slot));
    }
}

async fn handle(mut client: TcpStream, token_slot: SharedToken) {
    // Read the full request from the client into memory.
    // Requests are typically small (a few KB of headers + JSON body for LLM
    // calls); we read up to 64 MB to handle large prompts.
    let mut buf = Vec::with_capacity(4096);
    if read_http_message(&mut client, &mut buf).await.is_err() {
        return;
    }

    // Scan headers for a Bearer token and capture it.
    if let Some(token) = extract_bearer(&buf) {
        if let Ok(mut slot) = token_slot.lock() {
            *slot = Some(token);
        }
    }

    // Forward to the headroom backend.
    let Ok(mut backend) =
        TcpStream::connect(("127.0.0.1", HEADROOM_BACKEND_PORT)).await
    else {
        // headroom not up yet — send a 502 so the client gets a clean error.
        let _ = client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    };

    if backend.write_all(&buf).await.is_err() {
        return;
    }

    // Pipe the rest of both directions concurrently.
    let (mut cr, mut cw) = client.into_split();
    let (mut br, mut bw) = backend.into_split();

    let c2b = tokio::io::copy(&mut cr, &mut bw);
    let b2c = tokio::io::copy(&mut br, &mut cw);
    let _ = tokio::join!(c2b, b2c);
}

/// Read exactly one HTTP message (headers + body) from `stream` into `buf`.
/// Stops once we have a complete framed message so we can inspect headers
/// before forwarding, without blocking waiting for more data.
async fn read_http_message(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> std::io::Result<()> {
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

        // Find the end of the HTTP headers.
        let Some(header_end) = find_header_end(buf) else {
            continue;
        };

        // Parse Content-Length so we know how much body to wait for.
        let header_section = &buf[..header_end];
        let content_length = parse_content_length(header_section);
        let body_so_far = buf.len().saturating_sub(header_end + 4);

        if body_so_far >= content_length {
            return Ok(());
        }

        // Keep reading until we have the full body.
        let remaining = content_length - body_so_far;
        let prev_len = buf.len();
        buf.resize(prev_len + remaining, 0);
        stream.read_exact(&mut buf[prev_len..]).await?;
        return Ok(());
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> usize {
    let text = std::str::from_utf8(headers).unwrap_or("");
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
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
