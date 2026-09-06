/// Transparent HTTP proxy intercept layer.
///
/// Binds on 127.0.0.1:6767 (the address clients point at) and forwards every
/// request unchanged to 127.0.0.1:<backend_port>, where headroom actually
/// listens. The backend port is normally 6768 but is selected at proxy spawn
/// time and stored in `crate::backend_port`; it can shift to 6769..=6790 if
/// 6768 is held by a foreign process. We re-read the port per connection so
/// the intercept (which spawns before proxy startup runs the selection) picks
/// up the chosen value as soon as it's set.
///
/// As each request passes through, any `Authorization: Bearer …` header is
/// captured into `AppState::claude_bearer_token` so the usage-stats feature
/// can call the Anthropic OAuth usage endpoint without touching the keychain.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use base64::Engine;

use crate::backend_port;
use crate::bearer::{BearerToken, BEARER_TOKEN_TTL};
use crate::models::{CodexPlanTier, CodexRateLimitSnapshot, CodexUsageWindow};

pub const INTERCEPT_PORT: u16 = 6767;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
// Request bodies arrive over loopback so even multi-MB payloads land in well
// under a second; 30s is a generous stall bound, not a throughput budget.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEADER_BYTES: usize = 64 * 1024;
// Ceiling on the client-supplied Content-Length that sizes the up-front body
// allocation in the direct-forward path. Prevents a single request with e.g.
// `Content-Length: 9000000000` from OOM-ing the tray process (which carries the
// always-on proxy). 100 MiB clears Anthropic's 32 MB request limit with room to
// spare for image payloads.
const MAX_DIRECT_BODY: usize = 100 * 1024 * 1024;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Max requests forwarded to the Python backend concurrently. Each forward
/// holds a client + backend FD for the request's full lifetime (SSE streams
/// run for minutes), so an unbounded spawn pile-up under 30+ Claude Code
/// sessions can starve accept() with EMFILE even after the startup RLIMIT
/// raise. When saturated, `handle` fails fast with 503 + Retry-After: CC/Codex
/// retry transparently, unlike a dropped connect that kills the user's turn.
/// Overridable via HEADROOM_INTERCEPT_MAX_INFLIGHT.
const DEFAULT_MAX_INFLIGHT: usize = 512;

static BACKEND_INFLIGHT: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
    std::sync::OnceLock::new();

fn backend_inflight() -> &'static Arc<tokio::sync::Semaphore> {
    BACKEND_INFLIGHT.get_or_init(|| {
        let cap = std::env::var("HEADROOM_INTERCEPT_MAX_INFLIGHT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_INFLIGHT);
        Arc::new(tokio::sync::Semaphore::new(cap))
    })
}

/// Dedicated Codex subscription-usage endpoint (ChatGPT OAuth/session auth).
/// Current Codex no longer ships `x-codex-*` on the `/responses` handshake, so
/// this is the only source left for the desktop gauge's rate-limit window.
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_USAGE_POLL_MIN_INTERVAL_SECS: u64 = 60;
const CODEX_USAGE_POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// Epoch-seconds of the last usage-poll attempt; throttles the fire-and-forget
/// GET to at most one per `CODEX_USAGE_POLL_MIN_INTERVAL_SECS`.
static CODEX_USAGE_LAST_POLL: AtomicU64 = AtomicU64::new(0);

/// Epoch-seconds of the last time the Python backend delivered response bytes
/// through this intercept. Stamped by `StampReader` on every backend->client
/// read; consumed by the watchdog to distinguish a busy backend (streams still
/// flowing, event loop alive) from a wedged one before force-killing it.
/// Direct-to-Anthropic bypass paths never stamp, so bypassed traffic can't
/// mask a dead backend.
static BACKEND_LAST_TRAFFIC_EPOCH: AtomicU64 = AtomicU64::new(0);

/// True when the backend delivered response bytes within `window`.
pub fn backend_traffic_within(window: Duration) -> bool {
    let last = BACKEND_LAST_TRAFFIC_EPOCH.load(Ordering::Acquire);
    last != 0 && now_epoch_secs().saturating_sub(last) <= window.as_secs()
}

fn stamp_backend_traffic() {
    BACKEND_LAST_TRAFFIC_EPOCH.store(now_epoch_secs(), Ordering::Release);
}

/// Provider-bound requests seen by this intercept, per agent, regardless of
/// whether they were forwarded to the backend or fell back direct-to-provider.
/// Setup verification polls these in paywall-first onboarding, where the
/// Python backend (and thus `/stats`) does not exist yet.
static INTERCEPT_CLAUDE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static INTERCEPT_CODEX_REQUESTS: AtomicU64 = AtomicU64::new(0);
static INTERCEPT_OPENCODE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static INTERCEPT_GROK_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// One-shot guard: has the `first_optimized_request` funnel beacon been sent
/// this process yet? Fires when a request is actually forwarded to the backend
/// (optimized), not on bypass/passthrough. See the fire site in `handle`.
static FIRST_OPTIMIZED_REQUEST_REPORTED: AtomicBool = AtomicBool::new(false);

/// Same one-shot guard for the `first_prompt_request` funnel beacon, which
/// fires only for prompt-sized completion POSTs (`is_prompt_request_head`) —
/// agent startup noise like the models fetch or Claude Code's tiny quota ping
/// fires `first_optimized_request` but not this. The two beacons split the
/// funnel tail into "launched an agent once" vs "actually prompted one".
static FIRST_PROMPT_REQUEST_REPORTED: AtomicBool = AtomicBool::new(false);

/// Backend reachability, logged on transition only (0=unknown, 1=reachable,
/// 2=unreachable). Without this the log records every direct-fallback request
/// but nothing when the backend finally comes up, so "did the runtime finish
/// installing, and when" is only answerable as the *absence* of the unreachable
/// line — miserable to read. Transition logging gives a positive "reachable"
/// line (with how long it was down) and collapses the per-request spam.
static BACKEND_REACHABILITY_STATE: AtomicU8 = AtomicU8::new(0);
static BACKEND_DOWN_SINCE: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static BACKEND_DOWN_CODEX_RETRY_503S: AtomicU64 = AtomicU64::new(0);
static CODEX_INFLIGHT_503_LAST_REPORTED: AtomicU64 = AtomicU64::new(0);
static CODEX_GLOBAL_BYPASS_503_LAST_REPORTED: AtomicU64 = AtomicU64::new(0);
static CODEX_STREAM_NO_TERMINAL_LAST_REPORTED: AtomicU64 = AtomicU64::new(0);
const CODEX_RECONNECT_REPORT_MIN_INTERVAL_SECS: u64 = 60;
/// Last-reported epoch-seconds per (client, status) for `report_upstream_error`:
/// one Sentry event per error class per interval. A client looping on a 4xx
/// (RUST-BT: one host, 472 events of the same 400 in 19h, 4/min in bursts)
/// otherwise turns the capture into a quota drain; the signal is the fleet-wide
/// host spread, not the per-host retry rate, and every occurrence still lands
/// in the local log. A Vec, not a map: at most a handful of (client, status)
/// pairs ever exist, and `Mutex::new(Vec::new())` is const.
static UPSTREAM_ERROR_LAST_REPORTED: Mutex<Vec<((&'static str, u16), u64)>> =
    Mutex::new(Vec::new());
const UPSTREAM_ERROR_REPORT_MIN_INTERVAL_SECS: u64 = 300;

/// Epoch-second until which Codex reconnect warnings are suppressed. Set by the
/// runtime lifecycle when it *intentionally* stops+restarts the backend (an
/// upgrade / requirements repair). A down window we caused ourselves is not an
/// outage worth paging: the old backend is killed, the wheel reinstalled, then
/// a fresh one spawned, and the down->up transition would otherwise fire a
/// false `backend_unreachable`. Every observed instance of this warning came
/// from a single rc-churn test box cycling release candidates.
static SUPPRESS_RECONNECT_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Silence Codex reconnect warnings for `window` from now. Called around an
/// app-initiated backend restart so the ensuing reconnect isn't misreported as
/// an outage. `fetch_max` so overlapping restarts never shorten the window.
// ponytail: fixed window (caller passes a generous ceiling covering
// reinstall+boot); if upgrades ever routinely exceed it, gate on the live
// `runtime_upgrade_in_progress` flag instead.
pub fn suppress_codex_reconnect_reports_for(window: Duration) {
    let until = now_epoch_secs().saturating_add(window.as_secs());
    SUPPRESS_RECONNECT_UNTIL.fetch_max(until, Ordering::Relaxed);
}

fn codex_reconnect_reports_suppressed() -> bool {
    now_epoch_secs() < SUPPRESS_RECONNECT_UNTIL.load(Ordering::Relaxed)
}

fn should_report_throttled(slot: &AtomicU64) -> bool {
    let now = now_epoch_secs();
    let mut last = slot.load(Ordering::Relaxed);
    loop {
        if last != 0 && now.saturating_sub(last) < CODEX_RECONNECT_REPORT_MIN_INTERVAL_SECS {
            return false;
        }
        match slot.compare_exchange_weak(last, now, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => last = current,
        }
    }
}

/// True when (`client`, `status`) has not been reported within
/// `UPSTREAM_ERROR_REPORT_MIN_INTERVAL_SECS`; claims the slot when it returns true.
fn should_report_upstream_error(client: &'static str, status: u16) -> bool {
    let now = now_epoch_secs();
    let mut slots = UPSTREAM_ERROR_LAST_REPORTED.lock();
    match slots.iter_mut().find(|(key, _)| *key == (client, status)) {
        Some((_, last)) => {
            if now.saturating_sub(*last) < UPSTREAM_ERROR_REPORT_MIN_INTERVAL_SECS {
                return false;
            }
            *last = now;
        }
        None => slots.push(((client, status), now)),
    }
    true
}

fn report_codex_reconnect_incident(
    cause: &'static str,
    affected_requests: u64,
    downtime: Option<Duration>,
) {
    if codex_reconnect_reports_suppressed() {
        return;
    }
    sentry::with_scope(
        |scope| {
            scope.set_tag("codex_reconnect_cause", cause);
            scope.set_extra("affected_requests", (affected_requests as i64).into());
            if let Some(downtime) = downtime {
                scope.set_extra("downtime_ms", (downtime.as_millis() as i64).into());
            }
            scope.set_fingerprint(Some(&["codex-reconnecting", cause]));
        },
        || {
            sentry::capture_message(
                &format!("Codex entered reconnect retry loop ({cause})"),
                sentry::Level::Warning,
            );
        },
    );
}

/// Record the backend's reachability and log only when it changes. Called on
/// every request from both the connect-failed and connect-succeeded paths.
fn note_backend_reachability(reachable: bool, backend_addr: SocketAddr) {
    let new_state = if reachable { 1u8 } else { 2u8 };
    let previous_state = BACKEND_REACHABILITY_STATE.swap(new_state, Ordering::Relaxed);
    if previous_state == new_state {
        return;
    }
    if !reachable && previous_state == 0 {
        // First observation of this process: the app just launched and the
        // backend is still booting (Python + litellm import, tiktoken prefetch
        // — tens of seconds on a cold machine). An agent that was already
        // running sends into that window and gets direct-fallback, which is
        // expected, not an outage. Arming the down-timer here reported every
        // launch as `backend_unreachable` (RUST-5J: ~11 hosts, 20-210s
        // "downtime", 1-2 events per host). A backend that never comes up is
        // the actionable case and has its own signal — the watchdog's
        // `proxy_unreachable_post_boot` auto-pause (RUST-5D).
        log::info!("backend {backend_addr} not up yet at launch; using per-request fallback");
        return;
    }
    if reachable {
        match BACKEND_DOWN_SINCE.lock().take() {
            Some(since) => {
                let downtime = since.elapsed();
                log::info!(
                    "backend {backend_addr} reachable (after {:.0}s unreachable)",
                    downtime.as_secs_f64()
                );
                let affected = BACKEND_DOWN_CODEX_RETRY_503S.swap(0, Ordering::AcqRel);
                // Sub-10s outages are routine restart blips (updates, gate
                // transitions); only report episodes long enough to be felt.
                if affected > 0 && downtime.as_secs() >= 10 {
                    report_codex_reconnect_incident(
                        "backend_unreachable",
                        affected,
                        Some(downtime),
                    );
                }
            }
            None => log::info!("backend {backend_addr} reachable"),
        }
    } else {
        *BACKEND_DOWN_SINCE.lock() = Some(std::time::Instant::now());
        log::info!("backend {backend_addr} unreachable; using per-request fallback");
    }
}

/// Same key shape as `/stats` `agent_usage.agents[]` (`claude-code`, `codex`)
/// so the frontend verification anchor/delta logic works against either source.
pub fn intercept_request_counts() -> std::collections::HashMap<String, u64> {
    std::collections::HashMap::from([
        (
            "claude-code".to_string(),
            INTERCEPT_CLAUDE_REQUESTS.load(Ordering::Acquire),
        ),
        (
            "codex".to_string(),
            INTERCEPT_CODEX_REQUESTS.load(Ordering::Acquire),
        ),
        (
            "opencode".to_string(),
            INTERCEPT_OPENCODE_REQUESTS.load(Ordering::Acquire),
        ),
        (
            "grok-build".to_string(),
            INTERCEPT_GROK_REQUESTS.load(Ordering::Acquire),
        ),
    ])
}

/// Whether this process has forwarded a prompt-sized completion request yet
/// (the `first_prompt_request` funnel signal). Feeds the post-install
/// checklist's "first prompt sent" row; process-local is fine there because
/// the checklist only renders during the install session itself.
pub fn first_prompt_request_seen() -> bool {
    FIRST_PROMPT_REQUEST_REPORTED.load(Ordering::Acquire)
}

/// AsyncRead wrapper that stamps `BACKEND_LAST_TRAFFIC_EPOCH` whenever the
/// inner reader yields bytes. Wrapped around the backend->client half of the
/// splices below.
struct StampReader<R>(R);

impl<R: AsyncRead + Unpin> AsyncRead for StampReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.0).poll_read(cx, buf);
        if matches!(poll, std::task::Poll::Ready(Ok(()))) && buf.filled().len() > before {
            stamp_backend_traffic();
        }
        poll
    }
}

/// AsyncRead wrapper that watches the first bytes of a spliced response for
/// the status line and records an upstream 429 for the request's client
/// bucket. Buffers at most one status line's worth of bytes, never content.
/// Sniffs the backend->client response on the zero-copy splice every non-Codex
/// client uses: counts 429s for the usage gauge and, when the status is a
/// reportable upstream error (see [`is_reportable_upstream_error`]), buffers a
/// bounded slice of the response so [`report_upstream_error`] can fire when the
/// stream ends (`Drop`). This is the Claude-side sibling of
/// `splice_with_codex_capture`'s error peek: without it, a Claude client
/// looping on a 4xx was invisible in Sentry (the 2026-09-03 Codex 401 loop had
/// no Claude equivalent only by luck).
struct ResponseSniffer<R> {
    inner: R,
    buf: Vec<u8>,
    status: Option<u16>,
    done: bool,
    client_key: &'static str,
    /// Request path for error attribution; `None` disables error capture
    /// (local proxy paths like /stats, or an unparseable request head).
    capture_path: Option<String>,
}

/// A real status line ("HTTP/1.1 429 Too Many Requests\r\n") fits well within
/// this; hitting the cap without a CRLF means a non-HTTP stream — stop looking.
const STATUS_LINE_SNIFF_CAP: usize = 64;

impl<R> ResponseSniffer<R> {
    fn new(inner: R, client_key: &'static str, capture_path: Option<String>) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            status: None,
            done: false,
            client_key,
            capture_path,
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        if self.done || bytes.is_empty() {
            return;
        }
        let take = MAX_ERROR_BODY
            .saturating_sub(self.buf.len())
            .min(bytes.len());
        self.buf.extend_from_slice(&bytes[..take]);
        if self.status.is_none() {
            if !self.buf.windows(2).any(|w| w == b"\r\n") {
                if self.buf.len() >= STATUS_LINE_SNIFF_CAP {
                    // Non-HTTP stream — stop looking.
                    self.done = true;
                    self.buf = Vec::new();
                }
                return;
            }
            self.status = parse_response_status(&self.buf);
            if self.status == Some(429) {
                crate::usage_counters::record_429(self.client_key);
            }
            let capture = self.capture_path.is_some()
                && self
                    .status
                    .is_some_and(|s| is_reportable_upstream_error(&s));
            if !capture {
                self.done = true;
                self.buf = Vec::new();
                return;
            }
        }
        // Capturing: keep the bounded slice; stop observing once full. Error
        // responses are small JSON, so the cap is about hostile inputs, not a
        // truncation we expect to hit.
        if self.buf.len() >= MAX_ERROR_BODY {
            self.done = true;
        }
    }
}

impl<R> Drop for ResponseSniffer<R> {
    fn drop(&mut self) {
        let (Some(status), Some(path)) = (self.status, self.capture_path.as_deref()) else {
            return;
        };
        if !is_reportable_upstream_error(&status) || self.buf.is_empty() {
            return;
        }
        report_upstream_error(self.client_key, status, path, &self.buf, &[]);
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ResponseSniffer<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(poll, std::task::Poll::Ready(Ok(())))
            && buf.filled().len() > before
            && !self.done
        {
            let bytes = buf.filled()[before..].to_vec();
            self.observe(&bytes);
        }
        poll
    }
}

/// Backend response reader that stamps liveness and notices whether a Codex
/// SSE stream delivered any terminal Responses API event. The small rolling
/// tail catches event names split across TCP reads without buffering content.
struct CodexTerminalReader<R> {
    inner: R,
    tail: Vec<u8>,
    saw_terminal: bool,
}

impl<R> CodexTerminalReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            tail: Vec::new(),
            saw_terminal: false,
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        if self.saw_terminal || bytes.is_empty() {
            return;
        }
        const TERMINAL_EVENTS: &[&[u8]] = &[
            b"response.completed",
            b"response.failed",
            b"response.incomplete",
            // The backend synthesizes `event: error` and closes when the
            // upstream connection drops mid-stream (streaming.py connection-
            // error path) after the 200 status line already went out. That is
            // a terminal signal the client acts on, not a silent truncation —
            // without it this reader false-fired RUST-5N on error-terminated
            // streams.
            b"event: error",
        ];
        const TAIL_BYTES: usize = 32;

        let mut combined = Vec::with_capacity(self.tail.len() + bytes.len());
        combined.extend_from_slice(&self.tail);
        combined.extend_from_slice(bytes);
        self.saw_terminal = TERMINAL_EVENTS.iter().any(|needle| {
            combined
                .windows(needle.len())
                .any(|window| window == *needle)
        });
        let keep_from = combined.len().saturating_sub(TAIL_BYTES);
        self.tail.clear();
        self.tail.extend_from_slice(&combined[keep_from..]);
    }

    fn saw_terminal(&self) -> bool {
        self.saw_terminal
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CodexTerminalReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(poll, std::task::Poll::Ready(Ok(()))) && buf.filled().len() > before {
            stamp_backend_traffic();
            let bytes = &buf.filled()[before..];
            self.observe(bytes);
        }
        poll
    }
}

/// Shared state written by the intercept layer.
pub type SharedToken = Arc<Mutex<Option<BearerToken>>>;

/// Latest Codex rate-limit snapshot captured from `x-codex-*` response headers.
/// Shared with `AppState::codex_rate_limits`; read by `pricing::fetch_codex_usage`.
pub type CodexRateLimitSlot = Arc<Mutex<Option<CodexRateLimitSnapshot>>>;

/// When set to `true`, the intercept forwards traffic directly to
/// api.anthropic.com instead of the local Python proxy. Used to keep already-
/// running Claude Code sessions alive after the pricing gate has stopped the
/// Python proxy because the user crossed the free disable threshold.
pub type BypassFlag = Arc<AtomicBool>;

/// Shared with `AppState::codex_plan_tier`; populated from the Codex OAuth bearer
/// JWT and read by `pricing::fetch_codex_usage` to pick the recommended tier.
pub type CodexPlanSlot = Arc<Mutex<Option<crate::models::CodexPlanTier>>>;

/// Why the intercept is not listening on [`INTERCEPT_PORT`], or `None` while it
/// is serving normally. Shared with `AppState::intercept_bind_error` so the UI
/// can name the real cause: clients are hard-configured to 127.0.0.1:6767, so a
/// failed bind refuses every request regardless of the Python backend's state,
/// and the banner would otherwise blame the runtime.
pub type BindErrorSlot = Arc<Mutex<Option<String>>>;

/// Channel sender used to notify a background worker that the intercept just
/// captured a bearer token whose value differs from whatever was previously
/// in the slot. Empty payload — the worker reads the bearer from `AppState`
/// directly. Cloned per-connection in `run`.
pub type FreshBearerNotifier = mpsc::Sender<()>;

pub const ANTHROPIC_DIRECT_BASE: &str = "https://api.anthropic.com";
pub const OPENAI_DIRECT_BASE: &str = "https://api.openai.com";

/// What a held intercept port means, given who (if anyone) is listening on it
/// and how long we have been trying.
///
/// Split out from the bind loop so the decision is testable: the loop itself
/// is an infinite async retry that cannot be driven from a unit test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeldPortVerdict {
    /// Bind says in-use but nothing is LISTENING, and we are still inside the
    /// window where a previous instance's sockets could be draining. Windows
    /// only in practice: Rust sets `SO_REUSEADDR` on Unix but not there, so an
    /// exiting instance's accepted connections hold the port through
    /// TIME_WAIT. Self-healing, so it is not worth an error report.
    Draining,
    /// Same shape, but past the longest drain Windows can be configured for
    /// (`TcpTimedWaitDelay` maxes at 300s). No longer safe to assume it clears.
    Stuck,
    /// A live foreign listener. Does not clear on its own, and we can name it,
    /// so this is the one the user can actually act on.
    Foreign { name: String, pid: u32 },
}

/// Whether a verdict makes a `SO_REUSEADDR` rebind safe.
///
/// Only `Draining` does, and the distinction is a correctness boundary rather
/// than a preference. On Windows `SO_REUSEADDR` also permits binding over a
/// socket that is actively LISTENING, so reusing on `Foreign` would bind us
/// alongside a live holder, and reusing on any verdict reached while another
/// Headroom held the port would defeat single-instance protection and split
/// traffic across two proxies. `Draining` is the only verdict that guarantees
/// nothing is listening.
pub(crate) fn verdict_permits_reuse(verdict: &HeldPortVerdict) -> bool {
    matches!(verdict, HeldPortVerdict::Draining)
}

pub(crate) fn classify_held_port(
    occupant: Option<(String, u32)>,
    elapsed: std::time::Duration,
    drain_grace: std::time::Duration,
) -> HeldPortVerdict {
    match occupant {
        Some((name, pid)) => HeldPortVerdict::Foreign { name, pid },
        None if elapsed < drain_grace => HeldPortVerdict::Draining,
        None => HeldPortVerdict::Stuck,
    }
}

/// Locale-invariant identity for an OS error.
///
/// `io::Error`'s `Display` is the *localized* platform string, so keying a
/// Sentry message on it fingerprints one bug once per OS language: the held
/// intercept port arrived as RUST-7D (English) and RUST-7B (Spanish), which
/// can never be resolved together. The numeric code is the same everywhere;
/// the localized text still ships as an extra for the human reading it.
fn os_error_key(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("os error {code}"),
        // Not all io::Errors come from the OS (e.g. synthesized by a wrapper).
        // Nothing locale-dependent to strip, so the text is the best key.
        None => e.to_string(),
    }
}

/// Spawn the intercept proxy as a background Tokio task.
/// Returns immediately; the server runs until the process exits.
/// Uses a dedicated OS thread with its own Tokio runtime so it's safe to call
/// from Tauri's `.setup()` before the main async runtime has started.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    token_slot: SharedToken,
    codex_slot: CodexRateLimitSlot,
    codex_plan_slot: CodexPlanSlot,
    bypass: BypassFlag,
    claude_only_bypass: BypassFlag,
    codex_bypass: BypassFlag,
    fresh_bearer_tx: FreshBearerNotifier,
    bind_error: BindErrorSlot,
) {
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
                // The intercept is the app's front door: client configs point
                // all traffic at this port, so a bind failure must never end
                // the thread permanently — the squatter (a crashed prior
                // instance mid-exit, or a foreign process) may release the
                // port at any time, and giving up strands every client on a
                // dead endpoint with no recovery until app relaunch. Retry
                // forever; report each distinct error to Sentry once.
                let mut reported_errors: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                // One reclaim attempt per launch: past the grace, a
                // held-but-not-serving port is most often a stranded prior
                // Headroom desktop instance (updater relaunch), which nothing
                // else ever clears -- see reclaim_stranded_intercept_holder.
                let mut reclaim_attempted = false;
                // A restart -- the updater relaunch, or the "Restart now"
                // button -- starts the new process while the old one still
                // holds the port, so the first bind after launch routinely
                // fails through no fault of the machine. Publishing that
                // immediately paints "Headroom is not hooked up" over a window
                // that heals itself on the next retry, and files a Sentry error
                // that fixed itself.
                //
                // The grace is measured in TIME, not attempts. It used to be
                // "one attempt", and with a flat 15s retry that gave the old
                // process exactly 15s to exit -- enough on macOS, not on
                // Windows, where an exiting process can hold the socket longer
                // and every update therefore filed one self-healing
                // `os error 10048` (RUST-7M: one event per release, i.e. one
                // per update relaunch, not one per launch). `run` never returns
                // once it has bound (accept errors log and keep serving), so
                // neither the clock nor the counter needs a reset.
                const RELAUNCH_GRACE: std::time::Duration =
                    std::time::Duration::from_secs(90);
                // ...but the UI banner is on a shorter clock. `run` clears the
                // slot the instant it binds, so a real overlap never reaches
                // this; anything still held after it is something the user has
                // to be told about.
                const HINT_GRACE: std::time::Duration = std::time::Duration::from_secs(15);
                // Past RELAUNCH_GRACE a held port is still not necessarily a
                // problem: on Windows the exiting instance's accepted
                // connections hold the port through TIME_WAIT with nothing
                // in LISTENING state, and `TcpTimedWaitDelay` tops out at
                // 300s. Below that a listener-less port is still draining;
                // above it, it is stuck and worth saying so.
                const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(300);
                let launched_at = tokio::time::Instant::now();
                let mut consecutive_failures = 0usize;
                // Set only by the `Draining` verdict, which is the one case
                // where nothing is listening and the port is held purely by
                // sockets on their way out. Never reset: once we have
                // established that, a plain bind has nothing left to prove,
                // and `run` does not return after it binds.
                let mut reuse_addr = false;
                loop {
                    match run(
                        bind_addr,
                        reuse_addr,
                        token_slot.clone(),
                        codex_slot.clone(),
                        codex_plan_slot.clone(),
                        bypass.clone(),
                        claude_only_bypass.clone(),
                        codex_bypass.clone(),
                        fresh_bearer_tx.clone(),
                        upstream_base.clone(),
                        bind_error.clone(),
                    )
                    .await
                    {
                        Ok(()) => return,
                        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                            consecutive_failures += 1;
                            // If /health responds over HTTP, an existing
                            // Headroom proxy owns the port (single-instance
                            // plugin should normally prevent this, but a
                            // crashed or still-exiting prior process can leave
                            // it held) — benign, just wait for it to go away.
                            // Otherwise the port is foreign; escalate once.
                            if probe_existing_intercept().await {
                                log::info!(
                                    "[proxy_intercept] port {INTERCEPT_PORT} owned by existing Headroom proxy; retrying in 15s"
                                );
                            } else if launched_at.elapsed() < RELAUNCH_GRACE {
                                // Sentry stays quiet for the whole grace: a
                                // bind that heals itself is not an error worth
                                // a report. The banner must not, or the window
                                // sits on "runtime offline, proxy unreachable"
                                // -- which blames the Python runtime for a port
                                // that never opened -- for the full 90s.
                                if launched_at.elapsed() >= HINT_GRACE {
                                    *bind_error.lock() = Some(e.to_string());
                                }
                                log::info!(
                                    "[proxy_intercept] port {INTERCEPT_PORT} still held {}s after launch (a restart overlapping the previous instance looks exactly like this); retrying ({e})",
                                    launched_at.elapsed().as_secs()
                                );
                            } else {
                                // Nothing answered /health, so the port is
                                // held without being served: bind says in-use
                                // while connect is refused. Observed causes
                                // include a leftover Headroom whose socket
                                // outlived it, an unrelated app, and reserved
                                // ranges. Say only what was measured -- an
                                // earlier "held by foreign process" wording
                                // asserted the holder was not ours and sent a
                                // whole investigation down the wrong path.
                                *bind_error.lock() = Some(e.to_string());
                                // Identity-gated: only ever kills a process
                                // running this exact executable, so a foreign
                                // holder or reserved range is untouched and
                                // falls through to the report below. Blocking
                                // shell-outs are fine here: bind failed, so
                                // nothing is being served on this runtime.
                                if !reclaim_attempted {
                                    reclaim_attempted = true;
                                    if crate::tool_manager::reclaim_stranded_intercept_holder(
                                        INTERCEPT_PORT,
                                    ) {
                                        log::info!(
                                            "[proxy_intercept] reclaimed stranded instance on port {INTERCEPT_PORT}; retrying bind"
                                        );
                                        continue;
                                    }
                                }
                                // Who actually holds it decides whether this
                                // is worth a report. `listener_process` only
                                // ever names a socket in LISTENING state, so
                                // `None` here means bind says in-use while
                                // nothing is listening -- on Windows that is
                                // the previous instance's accepted connections
                                // draining through TIME_WAIT, because Rust
                                // sets SO_REUSEADDR on Unix but not there.
                                // RUST-7M was exactly this: one event per
                                // update relaunch, Windows-only, every one of
                                // them self-healing. Reporting it filed an
                                // error at the user that named no cause they
                                // could act on and that fixed itself minutes
                                // later. `Some` is the opposite case -- a live
                                // foreign listener, which does not clear on
                                // its own and which we can name.
                                let occupant =
                                    crate::tool_manager::listener_process(INTERCEPT_PORT);
                                let key = os_error_key(&e);
                                let verdict = classify_held_port(
                                    occupant,
                                    launched_at.elapsed(),
                                    DRAIN_GRACE,
                                );
                                // Nothing listening means the only thing
                                // holding the port is sockets draining, so the
                                // next attempt can take it back now instead of
                                // waiting out TcpTimedWaitDelay.
                                if verdict_permits_reuse(&verdict) {
                                    reuse_addr = true;
                                }
                                match verdict {
                                    HeldPortVerdict::Draining => {
                                        // Still a real outage from the user's
                                        // side, so the banner stays -- but it
                                        // says what is happening instead of
                                        // the bare OS string, which reads as
                                        // "the runtime is broken".
                                        *bind_error.lock() = Some(format!(
                                            "port {INTERCEPT_PORT} is still being released after a restart; reconnecting"
                                        ));
                                        log::info!(
                                            "[proxy_intercept] port {INTERCEPT_PORT} in use but nothing is listening {}s after launch (a previous instance's sockets draining looks exactly like this); retrying in 15s ({e})",
                                            launched_at.elapsed().as_secs()
                                        );
                                    }
                                    HeldPortVerdict::Stuck => {
                                        // Past the longest drain Windows can
                                        // be configured for, so "it will
                                        // clear itself" has stopped being
                                        // true. Nothing to name, but worth
                                        // knowing about.
                                        log::warn!(
                                            "[proxy_intercept] port {INTERCEPT_PORT} still in use with nothing listening after {}s; retrying in 15s ({e})",
                                            launched_at.elapsed().as_secs()
                                        );
                                        if reported_errors.insert(format!("stuck:{key}")) {
                                            sentry::with_scope(
                                                |scope| {
                                                    scope.set_extra(
                                                        "os_error", e.to_string().into());
                                                    scope.set_extra(
                                                        "held_secs",
                                                        launched_at.elapsed().as_secs().into());
                                                },
                                                || {
                                                    sentry::capture_message(
                                                        &format!(
                                                            "proxy_intercept bind failed: {key} (port {INTERCEPT_PORT} in use with no listener past the drain window; retrying)"
                                                        ),
                                                        sentry::Level::Error,
                                                    );
                                                },
                                            );
                                        }
                                    }
                                    HeldPortVerdict::Foreign { name, pid } => {
                                        // Actionable: the user can quit this.
                                        // Reclaim already declined it, so it
                                        // is not one of ours.
                                        log::warn!(
                                            "[proxy_intercept] port {INTERCEPT_PORT} is held by {name} (pid {pid}); retrying in 15s ({e})"
                                        );
                                        *bind_error.lock() = Some(format!(
                                            "port {INTERCEPT_PORT} is held by {name} (pid {pid})"
                                        ));
                                        if reported_errors.insert(format!("foreign:{key}:{name}")) {
                                            sentry::with_scope(
                                                |scope| {
                                                    scope.set_extra(
                                                        "os_error", e.to_string().into());
                                                    scope.set_extra(
                                                        "occupant", name.clone().into());
                                                    scope.set_extra("occupant_pid", pid.into());
                                                },
                                                || {
                                                    sentry::capture_message(
                                                        &format!(
                                                            "proxy_intercept bind failed: {key} (port {INTERCEPT_PORT} held by {name}; retrying)"
                                                        ),
                                                        sentry::Level::Error,
                                                    );
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            if consecutive_failures == 1 {
                                log::info!(
                                    "[proxy_intercept] error on the first bind attempt: {e}; retrying"
                                );
                            } else {
                                *bind_error.lock() = Some(e.to_string());
                                log::warn!("[proxy_intercept] error: {e}; retrying");
                                let key = os_error_key(&e);
                                if reported_errors.insert(key.clone()) {
                                    sentry::with_scope(
                                        |scope| {
                                            scope.set_extra("os_error", e.to_string().into());
                                        },
                                        || {
                                            sentry::capture_message(
                                                &format!("proxy_intercept error: {key} (retrying)"),
                                                sentry::Level::Error,
                                            );
                                        },
                                    );
                                }
                            }
                        }
                    }
                    // Retry fast at first: a relaunch overlap clears within a
                    // second or two of the old process finally exiting, and a
                    // flat 15s kept the app's front door shut for that whole
                    // window after every single update. Settle back to 15s once
                    // the holder looks like a genuine squatter rather than the
                    // instance we just replaced.
                    let backoff = match consecutive_failures {
                        0..=3 => 1,
                        4..=8 => 3,
                        _ => 15,
                    };
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                }
            });
        })
        .expect("spawn proxy intercept thread");
}

#[allow(clippy::too_many_arguments)]
/// Bind the intercept port on Windows with `SO_REUSEADDR` set.
///
/// Rust sets `SO_REUSEADDR` for you on Unix but not on Windows, so a listener
/// there cannot rebind a port whose previous owner left connections in
/// TIME_WAIT -- which is every update relaunch, for up to `TcpTimedWaitDelay`
/// (120s by default, 300s at most). That wait was RUST-7M.
///
/// Only ever reached from the `Draining` verdict, and that is load-bearing.
/// `SO_REUSEADDR` on Windows also lets a bind succeed over a socket that is
/// actively LISTENING, so using it unconditionally would let a second Headroom
/// bind 6767 alongside the first and split traffic between two proxies -- the
/// `probe_existing_intercept` branch relies on that bind failing. `Draining`
/// is only reached when `listener_process` found nothing in LISTENING state,
/// which rules out both another Headroom and a foreign holder, leaving the
/// kernel's TIME_WAIT reservation as the only thing this can bind over.
#[cfg(windows)]
fn reuse_bound_std_listener(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    // Matches the backlog tokio's own `TcpListener::bind` requests.
    socket.listen(1024)?;
    let listener: std::net::TcpListener = socket.into();
    // tokio's reactor requires this; `from_std` documents it as the caller's job.
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Plain bind, or a `SO_REUSEADDR` bind when the caller has established that
/// nothing is listening and the port is only held by draining sockets.
///
/// `reuse_addr` is inert off Windows: Unix already sets the option, so the
/// plain path there is the reuse path.
async fn bind_intercept(addr: SocketAddr, reuse_addr: bool) -> std::io::Result<TcpListener> {
    #[cfg(windows)]
    if reuse_addr {
        return TcpListener::from_std(reuse_bound_std_listener(addr)?);
    }
    #[cfg(not(windows))]
    let _ = reuse_addr;
    TcpListener::bind(addr).await
}

async fn run(
    bind_addr: SocketAddr,
    reuse_addr: bool,
    token_slot: SharedToken,
    codex_slot: CodexRateLimitSlot,
    codex_plan_slot: CodexPlanSlot,
    bypass: BypassFlag,
    claude_only_bypass: BypassFlag,
    codex_bypass: BypassFlag,
    fresh_bearer_tx: FreshBearerNotifier,
    upstream_base: Arc<String>,
    bind_error: BindErrorSlot,
) -> std::io::Result<()> {
    let listener = bind_intercept(bind_addr, reuse_addr).await?;
    // Serving again: clear whatever the previous attempt recorded so a
    // recovered port stops showing a stale cause in the UI.
    *bind_error.lock() = None;

    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let slot = token_slot.clone();
                let codex_slot = codex_slot.clone();
                let codex_plan_slot = codex_plan_slot.clone();
                let bypass = bypass.clone();
                let claude_only_bypass = claude_only_bypass.clone();
                let codex_bypass = codex_bypass.clone();
                let upstream_base = upstream_base.clone();
                let tx = fresh_bearer_tx.clone();
                tokio::spawn(handle(
                    client,
                    slot,
                    codex_slot,
                    codex_plan_slot,
                    bypass,
                    claude_only_bypass,
                    codex_bypass,
                    tx,
                    upstream_base,
                ));
            }
            Err(e) => {
                // EMFILE/ENFILE/ECONNABORTED are transient — log and keep serving
                // so the proxy self-heals once FDs free up, instead of dying.
                log::warn!("[proxy_intercept] accept error: {e}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

/// Returns `true` when `candidate` differs from whatever fresh bearer is
/// already in `slot`. An empty slot or a slot whose previous bearer has
/// aged out of `BEARER_TOKEN_TTL` both count as "changed" — the worker
/// should re-confirm identity in either case.
fn bearer_value_changed(slot: &SharedToken, candidate: &str) -> bool {
    let lock = slot.lock();
    lock.as_ref()
        .and_then(|t| t.value_if_fresh(BEARER_TOKEN_TTL))
        .map(|v| v != candidate)
        .unwrap_or(true)
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    mut client: TcpStream,
    token_slot: SharedToken,
    codex_slot: CodexRateLimitSlot,
    codex_plan_slot: CodexPlanSlot,
    bypass: BypassFlag,
    claude_only_bypass: BypassFlag,
    codex_bypass: BypassFlag,
    fresh_bearer_tx: FreshBearerNotifier,
    upstream_base: Arc<String>,
) {
    // Re-read the backend port on each connection. `tool_manager` selects the
    // port (and may switch to a fallback) when the proxy spawn runs, which
    // happens after this thread is already accepting; reading per-connection
    // means existing clients pick up the chosen port without restarting.
    let backend_addr: SocketAddr = ([127, 0, 0, 1], backend_port::get()).into();
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

    // Whether this is a Codex request. Parsed once here and reused for the
    // Codex plan capture, Codex-only bypass, counters, and response handling.
    let parsed_head = find_header_end(&buf).and_then(|end| parse_request_head(&buf[..end + 4]));
    let is_codex = parsed_head.as_ref().is_some_and(is_codex_request_head);
    let is_chatgpt_codex = is_codex && request_uses_chatgpt_auth(&buf);
    let is_local_backend_path = parsed_head
        .as_ref()
        .is_some_and(|head| is_local_proxy_path(&head.path));
    // OpenCode is classified by User-Agent, not path: it speaks both API
    // shapes (`/v1/messages` and `/v1/responses`), so path-based
    // classification would split it between the Claude and Codex buckets.
    // Both of its @ai-sdk transports send `opencode/<version> ...` (verified
    // against opencode 1.18.5), matching the backend's CLIENT_UA_MAP prefix.
    let is_opencode =
        extract_header_value(&buf, "user-agent").is_some_and(|ua| ua.starts_with("opencode/"));
    // Grok Build posts OpenAI-format chat completions, so path classification
    // says Codex; the UA disambiguates. Current builds send `grok-shell/`
    // (verified against grok 0.2.112); the backend's own UA map expects the
    // older `grok/`, so match both and rely on the explicit X-Client stamp
    // below for backend classification.
    let is_grok = extract_header_value(&buf, "user-agent")
        .is_some_and(|ua| ua.starts_with("grok-shell/") || ua.starts_with("grok/"));

    // One classification for the process counters, the per-day usage
    // counters, and the 429 sniffers below. Same keys as
    // `intercept_request_counts`.
    let client_key: &'static str = if is_opencode {
        "opencode"
    } else if is_grok {
        "grok-build"
    } else if is_codex {
        "codex"
    } else {
        "claude-code"
    };

    // Count provider-bound requests only: local proxy paths (/readyz, /stats,
    // ...) are probes, not client traffic, and unparseable heads can't be
    // attributed to an agent.
    if let Some(head) = parsed_head.as_ref() {
        if !is_local_proxy_path(&head.path) {
            match client_key {
                "opencode" => INTERCEPT_OPENCODE_REQUESTS.fetch_add(1, Ordering::AcqRel),
                "grok-build" => INTERCEPT_GROK_REQUESTS.fetch_add(1, Ordering::AcqRel),
                "codex" => INTERCEPT_CODEX_REQUESTS.fetch_add(1, Ordering::AcqRel),
                _ => INTERCEPT_CLAUDE_REQUESTS.fetch_add(1, Ordering::AcqRel),
            };
            crate::usage_counters::record_request(client_key);
        }
    }

    // Route Grok through the backend's per-request upstream selection: the
    // backend's OpenAI handler honours `x-headroom-base-url` (verified live
    // against api.x.ai with bearer passthrough), so grok traffic gets the
    // full compression pipeline and the correct upstream from the shared
    // backend instance. Stamped BEFORE the bypass branches below so the
    // no-direct-upstream 503 guard covers grok too - the direct forwarder
    // only knows the Anthropic/OpenAI bases, and forwarding an xAI key to
    // api.openai.com is the exact misroute this connector was blocked on.
    if is_grok {
        stamp_request_header(
            &mut buf,
            "x-headroom-base-url",
            b"x-headroom-base-url: https://api.x.ai\r\n",
        );
        stamp_client_header(&mut buf, b"X-Client: grok_build\r\n");
    }

    // Codex fetches its model catalog via `GET <base_url>/models` and caches it
    // in ~/.codex/models_cache.json. When OpenAI serves `use_responses_lite:
    // true` for a model, Codex switches to the "responses lite" transport,
    // which OpenAI rejects for proxied traffic ("This model is not supported
    // when using X-OpenAI-Internal-Codex-Responses-Lite", enforcement tightened
    // 2026-06-26). Detect the catalog fetch here so the response splice below
    // can force the flag to false, keeping Codex on the full Responses path —
    // which works through the proxy.
    // Grok excluded: its GET /v1/models is an xAI catalog, and the Codex
    // responses-lite rewrite would buffer it and emit Codex-labelled Sentry
    // noise for traffic unrelated to that bug.
    let is_models_fetch = !is_grok && parsed_head.as_ref().is_some_and(is_codex_models_fetch);

    // Scan headers for a Bearer token and capture it. When the token's
    // value differs from what was previously in the slot — or the slot was
    // empty / its previous token has aged out of the TTL — signal the
    // identity-pusher worker so it can re-confirm the user's Claude
    // identity with headroom-web. The send is non-blocking; the actual
    // OAuth-profile fetch happens off the request hot path.
    if let Some(token) = extract_bearer(&buf) {
        // For Codex requests the bearer is an OpenAI OAuth JWT carrying the
        // ChatGPT plan; decode it so the Codex gate can recommend a tier. It
        // must never land in the Claude bearer slot: pricing would send it to
        // Anthropic's OAuth profile/usage endpoints (cross-provider credential
        // transmission) where it only earns 401s.
        if is_codex && !is_opencode && !is_grok {
            if let Some(tier) = decode_codex_plan_tier(&token) {
                *codex_plan_slot.lock() = Some(tier);
            }
        } else if !is_codex && !is_opencode && !is_grok {
            // OpenCode/Grok bearers are the user's own provider keys (or
            // OAuth tokens for a possibly different account); landing them in
            // the Claude identity slot would transmit them to Anthropic's
            // OAuth endpoints and flap pricing/identity. Grok matters here
            // because its non-completion endpoints (/v1/api-key, ...) are not
            // path-classified as Codex.
            let changed = bearer_value_changed(&token_slot, &token);
            *token_slot.lock() = Some(BearerToken::new(token));
            if changed {
                let _ = fresh_bearer_tx.send(());
            }
        }
    }

    // The current Codex WS handshake no longer carries `x-codex-*` response
    // headers, so `splice_with_codex_capture` below comes up empty. Fetch the
    // live subscription window from the dedicated usage endpoint instead.
    // Throttled and fire-and-forget, so the request hot path is untouched.
    if is_codex && !is_opencode && !is_grok {
        maybe_spawn_codex_usage_poll(&buf, &codex_slot);
        // Codex stamps `X-OpenAI-Internal-Codex-Responses-Lite` on the
        // `/responses` WS handshake. OpenAI tightened enforcement on 2026-06-26
        // for gpt-5.5/gpt-5.4/gpt-5.4-mini, so the same Codex setup fails through
        // Headroom with "This model is not supported ..." while succeeding when
        // bypassed. Drop the header before any forwarding branch (backend/direct).
        //
        // STOPGAP: redundant with upstream headroom PR #1543, which strips this
        // in the backend's `handle_openai_responses_ws` (covers OSS-direct users
        // too). Remove this line once the bundled package includes that fix.
        strip_request_header(&mut buf, "X-OpenAI-Internal-Codex-Responses-Lite");
    }

    // When the pricing gate has bypassed Headroom, the Python proxy on
    // `backend_addr` is intentionally stopped. Forward direct to Anthropic so
    // already-running CC sessions stay alive while optimization is off.
    // ChatGPT-authenticated Codex cannot be sent to api.openai.com: its OAuth
    // token is scoped for chatgpt.com/backend-api/codex and the Platform API
    // rejects it with a misleading missing `api.responses.write` 401. Return a
    // retryable response instead of misrouting the credential.
    // OpenCode's transport plugin routes third-party providers (Google, custom
    // gateways) here with the real upstream in `x-headroom-base-url`. The
    // direct forwarder only knows the Anthropic/OpenAI bases, so forwarding
    // such a request would send it (and its credential) to the wrong vendor -
    // the exact grok-class misroute. 503-retry instead; these windows are
    // short because the backend is kept alive whenever OpenCode is enabled.
    let is_plugin_routed = request_has_header(&buf, "x-headroom-base-url");

    if bypass.load(Ordering::Acquire) {
        if is_chatgpt_codex || is_plugin_routed {
            if is_chatgpt_codex && should_report_throttled(&CODEX_GLOBAL_BYPASS_503_LAST_REPORTED) {
                report_codex_reconnect_incident("global_bypass", 1, None);
            }
            write_retryable_service_unavailable(&mut client).await;
        } else {
            forward_direct_to_anthropic(client, buf, &upstream_base).await;
        }
        return;
    }

    // Claude-only bypass: the pricing gate paused Claude optimization but Codex
    // is still enabled, so the Python backend is kept alive for Codex. Forward
    // only Claude (non-Codex) traffic direct; Codex falls through to the backend
    // below. This keeps a Claude overage from pausing Codex optimization.
    // OpenCode is exempt: it runs on the user's own API keys, so a Claude
    // plan gate has nothing to do with its billing — keep it optimized.
    // !is_grok: a grok request on a non-OpenAI path must not be direct-
    // forwarded to api.anthropic.com with its xAI bearer; the backend stays
    // alive whenever grok is enabled, so falling through is always routable.
    // Local backend paths (/stats, /readyz, ...) also fall through: the backend
    // is alive here (it is serving Codex), but the direct forwarder dead-ends
    // them in its local-path 503 guard, so the dashboard lost the layers a
    // live backend was ready to serve for the whole claude-only window.
    if !is_codex
        && !is_opencode
        && !is_grok
        && !is_local_backend_path
        && claude_only_bypass.load(Ordering::Acquire)
    {
        forward_direct_to_anthropic(client, buf, &upstream_base).await;
        return;
    }

    // Codex-only gate: keep Codex routed through the Python backend so it can
    // preserve the correct upstream for either ChatGPT OAuth or an API key,
    // but tell it to skip optimization for this request.
    if is_codex && !is_opencode && !is_grok && codex_bypass.load(Ordering::Acquire) {
        stamp_headroom_bypass_header(&mut buf);
    }

    // Bound concurrent backend forwards. The bypass/direct paths above return
    // before this point, so only backend-bound traffic is throttled. When the
    // permit pool is exhausted, fail fast with 503 + Retry-After instead of
    // connecting and holding another FD pair — a client that gets an immediate
    // 503 retries transparently; a hung/dropped connect kills the turn. The
    // permit is held in `_permit` until `handle` returns (through the splice).
    let Ok(_permit) = backend_inflight().clone().try_acquire_owned() else {
        log::info!("[proxy_intercept] backend in-flight cap reached; returning 503");
        if is_codex
            && !is_opencode
            && !is_grok
            && should_report_throttled(&CODEX_INFLIGHT_503_LAST_REPORTED)
        {
            report_codex_reconnect_incident("backend_inflight_cap", 1, None);
        }
        let _ = client
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        return;
    };

    // Forward to the headroom backend.
    let Ok(mut backend) = TcpStream::connect(backend_addr).await else {
        // Backend down or mid-restart (crash, gate transition, post-update
        // cold boot — which deliberately holds the bypass flags off for up to
        // 10 minutes): fall back per-request to the native provider for Claude
        // and API-key Codex. ChatGPT-authenticated Codex must retry until the
        // backend returns because its OAuth token is not valid at the Platform
        // API used by the direct forwarder.
        // info, not warn: warn would ship to Sentry per request; the watchdog's
        // capture_watchdog_give_up already reports genuine down episodes.
        note_backend_reachability(false, backend_addr);
        if is_chatgpt_codex {
            BACKEND_DOWN_CODEX_RETRY_503S.fetch_add(1, Ordering::AcqRel);
            write_retryable_service_unavailable(&mut client).await;
        } else if is_plugin_routed {
            // See the bypass branch above: no correct direct upstream exists
            // for plugin-routed third-party providers.
            write_retryable_service_unavailable(&mut client).await;
        } else {
            forward_direct_to_anthropic(client, buf, &upstream_base).await;
        }
        return;
    };
    note_backend_reachability(true, backend_addr);

    // We've committed to forwarding this request to Headroom's backend (it's
    // reachable and connected) -- i.e. it will actually be optimized, not passed
    // straight through to the provider. This is the honest "Headroom is working"
    // funnel signal: it can't fire on bypass/direct-to-provider or before the
    // backend is up (e.g. during bootstrap). Once per process; server is
    // first-write-wins so an extra send is cheap.
    //
    // `!is_local_backend_path` is load-bearing, not a tidy-up: the desktop polls
    // its own dashboard through this listener (`127.0.0.1:6767/stats`), so
    // without the guard the app fired this beacon at itself on the first
    // successful poll after bootstrap -- landing in the same second as
    // `bootstrap_completed`, before any client was even configured, and
    // overstating the funnel step past the number of installs that finished
    // bootstrapping. Same exclusion the per-client counters above already make.
    // Note it also gates the `swap`: a local poll must not burn the one-shot.
    if !is_local_backend_path && !FIRST_OPTIMIZED_REQUEST_REPORTED.swap(true, Ordering::AcqRel) {
        crate::pricing::report_funnel_step_device_only("first_optimized_request");
    }
    if parsed_head.as_ref().is_some_and(is_prompt_request_head)
        && !FIRST_PROMPT_REQUEST_REPORTED.swap(true, Ordering::AcqRel)
    {
        crate::pricing::report_funnel_step_device_only("first_prompt_request");
    }

    // Codex GUI/IDE clients don't send a `codex-cli/` User-Agent, so the
    // backend's UA-based classifier can't tell they're Codex and treats a
    // compression timeout as a fail-closed HTTP 413 instead of taking the
    // codex fail-open path. Codex treats that 413 as a hard connection failure
    // and stops connecting. We already know by request path that this is Codex
    // traffic, so stamp `X-Client: codex` (which the backend honours over the
    // User-Agent) to keep Codex GUI and Codex CLI on the same backend path.
    if is_opencode {
        // OpenCode already self-identifies by UA, but its OpenAI-path traffic
        // would otherwise be stamped codex below; an explicit X-Client wins
        // over every downstream heuristic (backend honours it over the UA).
        stamp_client_header(&mut buf, b"X-Client: opencode\r\n");
    } else if is_codex {
        stamp_codex_client_header(&mut buf);
    }

    // Force one request per connection so every request gets the full
    // interception path above — see force_connection_close. WebSocket
    // handshakes are exempt: the upgrade needs `Connection: Upgrade`, and an
    // upgraded socket carries no further HTTP request heads to miss.
    if !request_has_header(&buf, "upgrade") {
        force_connection_close(&mut buf);
    }

    if backend.write_all(&buf).await.is_err() {
        return;
    }

    // For Codex (OpenAI) requests, sniff the backend response head so we can
    // capture the `x-codex-*` rate-limit headers that feed the usage gauge.
    // Codex always streams, so the Python backend's own capture (non-streaming
    // only) never fires for it — this proxy is the only component left in the
    // response path that sees those headers. Every other client (Claude) keeps
    // the untouched zero-copy splice.
    if is_models_fetch {
        splice_with_models_lite_rewrite(client, backend).await;
    } else if is_codex && !is_opencode && !is_grok {
        let req_path = parse_request_head(&buf).map(|p| p.path).unwrap_or_default();
        splice_with_codex_capture(client, backend, &codex_slot, &req_path).await;
    } else {
        // Same shape as copy_bidirectional, split so the backend->client half
        // can stamp traffic liveness for the watchdog.
        let (mut client_rd, mut client_wr) = client.split();
        let (backend_rd, mut backend_wr) = backend.split();
        let upstream = async {
            let _ = tokio::io::copy(&mut client_rd, &mut backend_wr).await;
            let _ = backend_wr.shutdown().await;
        };
        let downstream = async {
            // Local proxy paths (/stats, /readyz probes) are our own traffic;
            // a 404 there is the squatter case (RUST-87), not a client error.
            // Client reachability probes are expected noise, see
            // is_client_probe_path.
            let error_path = parsed_head
                .as_ref()
                .filter(|head| {
                    !is_local_proxy_path(&head.path) && !is_client_probe_path(&head.path)
                })
                .map(|head| head.path.clone());
            let mut stamped = ResponseSniffer::new(StampReader(backend_rd), client_key, error_path);
            let _ = tokio::io::copy(&mut stamped, &mut client_wr).await;
            let _ = client_wr.shutdown().await;
        };
        tokio::join!(upstream, downstream);
    }
}

/// Upper bound on a `/v1/models` response body we're willing to buffer for the
/// lite-flag rewrite. Real model catalogs are a few KB.
const MAX_MODELS_BODY: usize = 2 * 1024 * 1024;
const MODELS_BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Splice client <-> backend for a Codex `GET /v1/models` catalog fetch,
/// rewriting `"use_responses_lite": true` to `false` in the JSON response so
/// Codex stays on the full Responses transport (the lite transport is rejected
/// by OpenAI when re-originated by a proxy). Fail-open: on non-200, compressed
/// or chunked bodies, oversize payloads, truncated reads, or non-JSON content,
/// the response is forwarded byte-for-byte untouched.
async fn splice_with_models_lite_rewrite(mut client: TcpStream, mut backend: TcpStream) {
    let mut head = Vec::with_capacity(4096);
    let read_head = tokio::time::timeout(
        HEADER_READ_TIMEOUT,
        read_http_headers(&mut backend, &mut head),
    )
    .await;
    if !matches!(read_head, Ok(Ok(()))) {
        if !head.is_empty() && client.write_all(&head).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
        return;
    }

    // `read_http_headers` may over-read leading body bytes past the terminator.
    let head_end = find_header_end(&head).map(|e| e + 4).unwrap_or(head.len());
    let status = parse_response_status(&head);
    let content_length =
        extract_header_value(&head, "content-length").and_then(|v| v.parse::<usize>().ok());
    let compressed = extract_header_value(&head, "content-encoding").is_some();
    let rewritable = matches!(status, Some(200))
        && !compressed
        && content_length.is_some_and(|n| n <= MAX_MODELS_BODY);

    if rewritable {
        let total = content_length.unwrap_or(0);
        let mut body = head.split_off(head_end);
        while body.len() < total {
            let mut tmp = [0u8; 4096];
            match tokio::time::timeout(MODELS_BODY_READ_TIMEOUT, backend.read(&mut tmp)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => body.extend_from_slice(&tmp[..n]),
            }
        }
        // Bytes past `total` belong to the next keep-alive response.
        let extra = if body.len() > total {
            body.split_off(total)
        } else {
            Vec::new()
        };
        if body.len() == total {
            match rewrite_use_responses_lite(&body) {
                ModelsRewrite::Rewritten {
                    body: rewritten,
                    flags_flipped,
                } => {
                    set_response_content_length(&mut head, rewritten.len());
                    body = rewritten;
                    // Normal operation, not a signal: at Info this still went
                    // to Sentry via capture_message and became the project's
                    // highest-volume issue (RUST-4M, ~750 events/14d). Local
                    // log only; the warning variants below still report.
                    log::info!(
                        "codex models rewrite applied: flipped {flags_flipped} use_responses_lite flag(s)"
                    );
                }
                ModelsRewrite::Unchanged => {}
                ModelsRewrite::Unparseable => {
                    report_models_rewrite(
                        "unparseable_json",
                        sentry::Level::Warning,
                        &format!("200 models response, {} bytes, not JSON", body.len()),
                    );
                }
            }
        } else {
            report_models_rewrite(
                "truncated_body",
                sentry::Level::Warning,
                &format!("read {} of {total} body bytes", body.len()),
            );
        }
        for part in [&head, &body, &extra] {
            if !part.is_empty() && client.write_all(part).await.is_err() {
                return;
            }
        }
    } else {
        // A 200 catalog we could not inspect means an affected user silently
        // keeps `use_responses_lite: true` — exactly the failure this rewrite
        // exists to prevent, so surface it. Non-200s are routine (auth errors,
        // upstream hiccups) and already covered by client-side retries.
        if status == Some(200) {
            let reason = if compressed {
                "compressed"
            } else if content_length.is_none() {
                "no_content_length"
            } else {
                "oversize"
            };
            report_models_rewrite(
                reason,
                sentry::Level::Warning,
                &format!("200 models response skipped (content_length={content_length:?})"),
            );
        }
        if client.write_all(&head).await.is_err() {
            return;
        }
    }
    // Remainder: body of a non-rewritable response and/or keep-alive reuse.
    let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
}

/// Outcome of attempting the lite-flag rewrite on a models-catalog body.
enum ModelsRewrite {
    /// Body is not JSON (or re-serialization failed) — forwarded untouched.
    Unparseable,
    /// Valid JSON with no `use_responses_lite: true` — forwarded untouched.
    Unchanged,
    /// One or more flags flipped; `body` is the re-serialized payload.
    Rewritten { body: Vec<u8>, flags_flipped: usize },
}

/// Force every `use_responses_lite: true` in a models-catalog JSON payload to
/// `false`.
fn rewrite_use_responses_lite(body: &[u8]) -> ModelsRewrite {
    fn force_false(v: &mut serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(map) => {
                let mut flipped = 0;
                for (key, val) in map.iter_mut() {
                    if key == "use_responses_lite" && *val == serde_json::Value::Bool(true) {
                        *val = serde_json::Value::Bool(false);
                        flipped += 1;
                    } else {
                        flipped += force_false(val);
                    }
                }
                flipped
            }
            serde_json::Value::Array(items) => items.iter_mut().map(force_false).sum(),
            _ => 0,
        }
    }

    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ModelsRewrite::Unparseable;
    };
    let flags_flipped = force_false(&mut value);
    if flags_flipped == 0 {
        return ModelsRewrite::Unchanged;
    }
    match serde_json::to_vec(&value) {
        Ok(body) => ModelsRewrite::Rewritten {
            body,
            flags_flipped,
        },
        Err(_) => ModelsRewrite::Unparseable,
    }
}

/// Report a models-rewrite event to Sentry. `kind` is one of `applied`,
/// `unparseable_json`, `truncated_body`, `compressed`, `no_content_length`,
/// `oversize` — fingerprinted per kind so each failure class is its own issue
/// (mirrors report_upstream_error's grouping rationale).
fn report_models_rewrite(kind: &str, level: sentry::Level, detail: &str) {
    sentry::with_scope(
        |scope| {
            scope.set_tag("models_rewrite", kind);
            scope.set_extra("detail", detail.to_string().into());
            scope.set_fingerprint(Some(&["codex-models-rewrite", kind]));
        },
        || {
            sentry::capture_message(&format!("codex models rewrite {kind}: {detail}"), level);
        },
    );
}

/// Replace (or insert) the `Content-Length` header in a response head after a
/// body rewrite changed its size. `head` must end with the `\r\n\r\n`
/// terminator and contain no body bytes.
fn set_response_content_length(head: &mut Vec<u8>, len: usize) {
    strip_request_header(head, "content-length");
    if let Some(end) = find_header_end(head) {
        let insert_at = end + 2;
        head.splice(
            insert_at..insert_at,
            format!("Content-Length: {len}\r\n").into_bytes(),
        );
    }
}

/// Splice client <-> backend while sniffing the backend's response head for
/// `x-codex-*` rate-limit headers. Only the response head is read up-front (the
/// body/SSE bytes that follow are spliced through verbatim), so streaming
/// responses are neither buffered nor delayed beyond their header block. On any
/// read error before the head completes, whatever was read is still forwarded,
/// so the response is never corrupted.
async fn splice_with_codex_capture(
    mut client: TcpStream,
    mut backend: TcpStream,
    codex_slot: &CodexRateLimitSlot,
    req_path: &str,
) {
    let (mut client_rd, mut client_wr) = client.split();
    let (mut backend_rd, mut backend_wr) = backend.split();

    // Set once the client stops sending — EOF or error on its read half. When
    // that happens *before* the backend finishes streaming, the client walked
    // away (Codex cancels a turn with ESC) and the truncated SSE stream is the
    // consequence, not a Headroom fault. See the terminal-event check below.
    let client_gone = Arc::new(AtomicBool::new(false));

    // client -> backend: opaque copy (request body / pipelined requests).
    let upstream = {
        let client_gone = Arc::clone(&client_gone);
        async move {
            let _ = tokio::io::copy(&mut client_rd, &mut backend_wr).await;
            client_gone.store(true, Ordering::Relaxed);
            let _ = backend_wr.shutdown().await;
        }
    };

    // backend -> client: capture the response head, then stream the remainder.
    let downstream = async {
        let mut head = Vec::with_capacity(4096);
        let read_head = tokio::time::timeout(
            HEADER_READ_TIMEOUT,
            read_http_headers(&mut backend_rd, &mut head),
        )
        .await;

        if matches!(read_head, Ok(Ok(()))) {
            stamp_backend_traffic();
            if let Some(snapshot) = parse_codex_rate_limit_headers(&head) {
                *codex_slot.lock() = Some(snapshot);
            }
            if parse_response_status(&head) == Some(429) {
                crate::usage_counters::record_429("codex");
            }
        }

        // Forward the head bytes we read first (full head on success, partial
        // on timeout/EOF — `read_http_headers` may also include leading body
        // bytes it over-read). The error-body peek below must never sit in
        // front of this write: it used to delay the client's status line by up
        // to 3s when the backend dallied after the head.
        if client_wr.write_all(&head).await.is_err() {
            return;
        }
        // On an upstream error status, peek one bounded chunk of the error
        // body for a Sentry report and forward it immediately. Codex error
        // responses are small JSON (not the SSE stream), so the streaming
        // happy path never takes this branch.
        if let Some(status) = parse_response_status(&head).filter(is_reportable_upstream_error) {
            let mut chunk = vec![0u8; MAX_ERROR_BODY];
            let n = match tokio::time::timeout(ERROR_BODY_READ_TIMEOUT, backend_rd.read(&mut chunk))
                .await
            {
                Ok(Ok(n)) => n,
                _ => 0,
            };
            chunk.truncate(n);
            if client_wr.write_all(&chunk).await.is_err() {
                return;
            }
            report_upstream_error("codex", status, req_path, &head, &chunk);
        }
        let monitor_terminal = is_codex_sse_response(&head, req_path);
        let mut streamed = CodexTerminalReader::new(backend_rd);
        if monitor_terminal {
            let body_start = find_header_end(&head)
                .map(|end| end + 4)
                .unwrap_or(head.len());
            streamed.observe(&head[body_start..]);
        }
        let copy_result = tokio::io::copy(&mut streamed, &mut client_wr).await;
        // A cancelled turn reaches here with `copy_result == Ok`: the client
        // closed first, the backend then EOF'd the stream, and our writes to the
        // half-dead socket never errored. Without the `client_gone` guard every
        // ESC produced a RUST-5N event (276 in 7 days, streamed_bytes scattered
        // from 5 KB to 140 KB — the signature of arbitrary user aborts, not a
        // buffer-boundary truncation). A client that half-closes its write side
        // while still reading will now be missed too; a canary that only fires
        // on real truncation is worth that.
        if monitor_terminal
            && copy_result.is_ok()
            && !client_gone.load(Ordering::Relaxed)
            && !streamed.saw_terminal()
            && should_report_throttled(&CODEX_STREAM_NO_TERMINAL_LAST_REPORTED)
        {
            report_codex_stream_without_terminal(req_path, copy_result.unwrap_or(0));
        }
        let _ = client_wr.shutdown().await;
    };

    tokio::join!(upstream, downstream);
}

/// Bound on the error-body slice we peek for a Sentry report (and forward).
const MAX_ERROR_BODY: usize = 8192;
const ERROR_BODY_READ_TIMEOUT: Duration = Duration::from_secs(3);

fn is_codex_sse_response(head: &[u8], req_path: &str) -> bool {
    req_path.starts_with("/v1/responses")
        && parse_response_status(head).is_some_and(|status| (200..300).contains(&status))
        && extract_header_value(head, "content-type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
        && extract_header_value(head, "content-encoding").is_none()
}

fn report_codex_stream_without_terminal(req_path: &str, streamed_bytes: u64) {
    sentry::with_scope(
        |scope| {
            scope.set_tag("codex_reconnect_cause", "stream_ended_without_terminal");
            scope.set_tag("codex_request_path", req_path);
            scope.set_tag("codex_transport", "http_sse");
            scope.set_extra("streamed_bytes", (streamed_bytes as i64).into());
            scope.set_fingerprint(Some(&[
                "codex-reconnecting",
                "stream-ended-without-terminal",
            ]));
        },
        || {
            sentry::capture_message(
                "Codex response stream ended without a terminal event",
                sentry::Level::Warning,
            );
        },
    );
}

/// Parse the status code from an HTTP response head's status line
/// (`HTTP/1.1 400 Bad Request` -> `400`).
fn parse_response_status(head: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(head).ok()?;
    let first = text.split("\r\n").next()?;
    first.split_whitespace().nth(1)?.parse().ok()
}

/// Whether an upstream status is worth peeking the error body for a possible
/// Sentry event. 429 (rate limit) is routine and excluded outright, as is 402:
/// the provider's billing state (no credits, no payment method) is a property
/// of the user's account, never of the request we built, and no release of
/// ours changes it (RUST-CP: opencode 402 on /v1/chat/completions). 401 gets
/// the peek but is only reported when the body says NO auth header was sent at
/// all (a setup bug on our side: the 2026-09-03 Windows case, where a Codex
/// provider block written before `codex login` omitted `requires_openai_auth`
/// and every request 401'd invisibly) -- an invalid/expired key 401 stays
/// unreported (RUST-46), see [`report_upstream_error`].
fn is_reportable_upstream_error(status: &u16) -> bool {
    *status >= 400 && !matches!(status, 402 | 429)
}

/// True when a 401 body says the request carried no credentials at all. That
/// means the CLIENT attached nothing -- a configuration bug we likely caused --
/// as opposed to an invalid or expired key, which only the user can fix.
/// Matches the providers' static message strings via the structural
/// `error.message` field; no free text is kept:
/// - OpenAI: "Missing bearer or basic authentication in header"
/// - Anthropic: "Could not resolve authentication method. Expected either
///   x-api-key or authorization header to be provided."
fn is_missing_auth_error(body: &[u8]) -> bool {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let err = json.get("error").unwrap_or(&json);
    err.get("message")
        .and_then(|v| v.as_str())
        .map(|m| m.to_ascii_lowercase())
        .is_some_and(|m| {
            m.contains("missing bearer") || m.contains("could not resolve authentication")
        })
}

/// Report an upstream error to Sentry with the client, status, request path and
/// a structural summary of the error body (never the raw body: provider 400s
/// frequently echo request fields, so raw attachment would leak prompt
/// fragments into Sentry). `client` is one of the `client_key` values
/// ("claude-code", "codex", "opencode", "grok-build").
fn report_upstream_error(
    client: &'static str,
    status: u16,
    req_path: &str,
    head: &[u8],
    chunk: &[u8],
) {
    let head_body = find_header_end(head)
        .map(|e| &head[(e + 4).min(head.len())..])
        .unwrap_or(&[]);
    let mut body: Vec<u8> = Vec::with_capacity(head_body.len() + chunk.len());
    body.extend_from_slice(head_body);
    body.extend_from_slice(chunk);
    let snippet = codex_error_summary(&body);
    let path = req_path.to_string();
    // The raw body stays on-device: the local log keeps full debugging detail
    // (provider 400s often quote request fields, so only the structural summary
    // above may leave the machine via Sentry). The "<client> upstream error"
    // prefix is what the logging.rs bridge skip rule keys on (RUST-5Q) -- the
    // explicit capture below is the only Sentry path.
    let raw_snippet: String = String::from_utf8_lossy(&body).chars().take(2000).collect();
    log::warn!("{client} upstream error {status} on {path}: {raw_snippet}");
    // Upstream 5xx is a provider-side transient (502/503/504/500 proxy_error)
    // that Headroom neither caused nor can fix. Capturing every one just burns
    // Sentry quota (RUST-46/4G/4T were all this). Keep full detail in the local
    // log::warn! above; only forward non-5xx classes (4xx auth/challenge, novel
    // statuses) that can indicate an actionable request-construction bug.
    if (500..600).contains(&status) {
        return;
    }
    // A geo-block is a property of where the user is, not of anything we sent:
    // OpenAI refuses the request before it is ever evaluated, and no release we
    // ship can change the outcome. Same reasoning that already excludes 401 and
    // 429 above, applied one level deeper because the status alone does not say
    // it -- a 403 can also be an org-verification challenge, which IS
    // actionable, so the body's `code` is what splits them. RUST-4H collected
    // 139 of these from hosts in unsupported regions. The raw body still
    // reaches the local log::warn! above.
    if is_geo_blocked_codex_error(&body) {
        return;
    }
    // 401 splits on the body, like the geo-block above: "Missing bearer" means
    // the client sent NO credentials -- a setup bug (ours to fix, and worth an
    // issue: the 2026-09-03 flagless-provider-block loop was invisible in
    // Sentry precisely because 401 was excluded wholesale). Any other 401 is an
    // invalid/expired key, which is the user's to fix (RUST-46 noise).
    if status == 401 && !is_missing_auth_error(&body) {
        return;
    }
    // After the drop filters, so a discarded class never claims the slot of a
    // reportable one; before the capture, so a retry loop costs one event per
    // interval instead of one per request (RUST-BT).
    if !should_report_upstream_error(client, status) {
        return;
    }
    // Group by client and status so each upstream failure class is its own
    // Sentry issue. Without an explicit fingerprint, Sentry parameterizes the
    // message and collapses 401 noise, 403 challenges and real 502/503
    // connection errors into one un-triageable bucket that regresses the moment
    // any sibling status reappears (RUST-46). Codex keeps its historical
    // fingerprint so existing issues and their triage state carry over.
    let status_str = status.to_string();
    let fingerprint: Vec<&str> = if client == "codex" {
        vec!["codex-upstream-error", status_str.as_str()]
    } else {
        vec!["upstream-error", client, status_str.as_str()]
    };
    // Tags, not extras: an extra can only be read one event at a time, so the
    // shape RUST-4V's 578 events shared was never visible from the issue view.
    // All values are bounded and content-free, so they stay aggregatable.
    // Anthropic's tool-search 400s stream as SSE (event: error\ndata: {...}), so
    // codex_error_shape_tag's JSON parse yields "non-json" and the RUST-BT bucket
    // ("claude-code 400 on /v1/messages?beta=true") can't tell a tool_reference
    // 400 from any other. Classify by signature first so we can measure what the
    // ENABLE_TOOL_SEARCH rollout is actually costing (the tag value is a fixed
    // classification string, never the tool name).
    let shape = anthropic_error_shape(&body)
        .map(str::to_string)
        .unwrap_or_else(|| codex_error_shape_tag(&body));
    let content_type = response_content_type(head);
    // Default vs user-configured Anthropic upstream, never the URL itself. A
    // relay that lacks a route answers with a bare 405 (RUST-C4: 64 empty-body
    // 405s on /v1/messages/count_tokens from two hosts) and nothing on the
    // event said whether api.anthropic.com or the user's relay sent it.
    let upstream_base = if crate::upstream_override::get()
        .configured_upstream()
        .is_some()
    {
        "custom"
    } else {
        "default"
    };
    sentry::with_scope(
        |scope| {
            scope.set_tag("upstream_client", client);
            scope.set_tag("upstream_status", status);
            scope.set_tag("upstream_request_path", &path);
            scope.set_tag("upstream_error_shape", &shape);
            scope.set_tag("upstream_base", upstream_base);
            if let Some(content_type) = content_type.as_deref() {
                scope.set_tag("upstream_response_content_type", content_type);
            }
            scope.set_extra("error_body", snippet.clone().into());
            scope.set_fingerprint(Some(&fingerprint));
        },
        || {
            sentry::capture_message(
                &format!("{client} upstream error {status} on {path}"),
                sentry::Level::Warning,
            );
        },
    );
}

/// True when an upstream error body carries OpenAI's unsupported-region code.
/// Only the structural `code` field is read; no free text is inspected or kept.
fn is_geo_blocked_codex_error(body: &[u8]) -> bool {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let err = json.get("error").unwrap_or(&json);
    err.get("code").and_then(|v| v.as_str()) == Some("unsupported_country_region_territory")
}

/// The response's media type with any parameters (`; charset=...`) stripped, so
/// it stays a low-cardinality tag. Distinguishes a JSON error from an HTML
/// gateway page or an SSE frame without reading a byte of the body.
fn response_content_type(head: &[u8]) -> Option<String> {
    let value = extract_header_value(head, "content-type")?;
    let media_type = value.split(';').next()?.trim().to_ascii_lowercase();
    (!media_type.is_empty()).then_some(media_type)
}

/// Reduce an upstream error body to structural fields safe for Sentry:
/// `error.type` / `error.code` / `error.param`, never free-text (the
/// `message` field and raw bodies can quote request content).
fn codex_error_summary(body: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(json) => {
            let err = json.get("error").unwrap_or(&json);
            let field = |key: &str| err.get(key).and_then(|v| v.as_str());
            let (kind, code, param) = (field("type"), field("code"), field("param"));
            // All three absent means the body parsed but carried none of the
            // schema we know how to read, and "type=- code=- param=-" then
            // says only "something failed" -- RUST-4V collected 578 events
            // that were all exactly that string and stayed untriageable for
            // two months. Describe the shape instead, so the next one is a
            // lead rather than another tally mark.
            if kind.is_none() && code.is_none() && param.is_none() {
                return format!(
                    "no structural error fields; shape={} ({} bytes)",
                    codex_error_body_shape(&json),
                    body.len()
                );
            }
            format!(
                "type={} code={} param={}",
                kind.unwrap_or("-"),
                code.unwrap_or("-"),
                param.unwrap_or("-")
            )
        }
        // Truncated (peek is bounded) or non-JSON body — report size only.
        Err(_) => format!("unparseable error body ({} bytes)", body.len()),
    }
}

/// Content-free descriptor of an error body's JSON shape.
///
/// Key NAMES are schema; only values can quote the request, and none are read
/// here. Top-level keys of an HTTP error body are chosen by the API, not by the
/// user, so naming them cannot leak prompt content even when a 400 echoes
/// request fields. `is_safe_shape_key` is the belt-and-braces: anything that
/// does not look like an identifier is dropped rather than forwarded.
fn codex_error_body_shape(json: &serde_json::Value) -> String {
    use serde_json::Value;
    match json {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map
                .keys()
                .map(String::as_str)
                .filter(|key| is_safe_shape_key(key))
                .collect();
            keys.sort_unstable();
            keys.truncate(SHAPE_MAX_KEYS);
            format!("object{{{}}}", keys.join(","))
        }
        Value::Array(_) => "array".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Null => "null".to_string(),
    }
}

/// Cap on keys named in a shape descriptor. Bounds both the message length and
/// the cardinality of the `codex_error_shape` tag built from it.
const SHAPE_MAX_KEYS: usize = 8;

/// Whether a JSON key is safe to forward as schema: a short, identifier-shaped
/// name. Rejects anything long or punctuated, which is what a key carrying
/// user content would look like.
fn is_safe_shape_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 32
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// The shape descriptor as a Sentry TAG value.
///
/// Deliberately a tag and not an extra: extras cannot be aggregated or
/// searched, so `error_body` could only ever be read one event at a time --
/// which is why RUST-4V's 578 events never revealed that they shared one
/// shape. A tag makes "which shapes are these?" a single query.
fn codex_error_shape_tag(body: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(json) => codex_error_body_shape(&json),
        Err(_) if body.is_empty() => "empty".to_string(),
        Err(_) => "non-json".to_string(),
    }
}

/// Classify an Anthropic invalid_request 400 by signature, so the tool-search
/// history 400s stop hiding inside the generic RUST-BT bucket. Substring match
/// on the raw bytes because these stream as SSE (JSON parse fails). Returns a
/// fixed, content-free classification (never the offending tool name), or None
/// when the body is not one of these shapes (fall back to the codex classifier).
fn anthropic_error_shape(body: &[u8]) -> Option<&'static str> {
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        needle.len() <= haystack.len() && haystack.windows(needle.len()).any(|w| w == needle)
    }
    if contains(body, b"not found in available tools") {
        Some("tool_reference_not_found")
    } else if contains(body, b"All tools cannot be deferred") {
        Some("all_tools_deferred")
    } else {
        None
    }
}

/// Parse the `x-codex-*` rate-limit headers out of a raw HTTP response head
/// (status line + headers up to the blank line). Mirrors the schema in upstream
/// `headroom/subscription/codex_rate_limits.py`. Returns `None` when there is no
/// usable signal (no windows and no credits balance).
fn parse_codex_rate_limit_headers(head: &[u8]) -> Option<CodexRateLimitSnapshot> {
    let text = std::str::from_utf8(head).ok()?;

    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break; // end of header block
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let parse_window = |prefix: &str| -> Option<CodexUsageWindow> {
        let used_percent: f64 = headers
            .get(&format!("x-codex-{prefix}-used-percent"))?
            .parse()
            .ok()?;
        let window_minutes = headers
            .get(&format!("x-codex-{prefix}-window-minutes"))
            .and_then(|v| v.parse::<i64>().ok());
        let reset_at = headers
            .get(&format!("x-codex-{prefix}-reset-at"))
            .and_then(|v| v.parse::<i64>().ok());
        Some(CodexUsageWindow {
            used_percent,
            window_label: window_minutes.map(codex_window_label),
            window_minutes,
            seconds_until_reset: reset_at.map(|r| (r - now).max(0)),
        })
    };

    let primary = parse_window("primary");
    let secondary = parse_window("secondary");
    let credits_balance = headers
        .get("x-codex-credits-balance")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let credits_unlimited = headers
        .get("x-codex-credits-unlimited")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);
    let limit_name = headers
        .get("x-codex-limit-name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if primary.is_none() && secondary.is_none() && credits_balance.is_none() {
        return None;
    }

    Some(CodexRateLimitSnapshot {
        limit_name,
        primary,
        secondary,
        credits_balance,
        credits_unlimited,
    })
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Extract a single request header value (case-insensitive) from raw HTTP bytes.
/// `read_http_headers` over-reads in 4KB chunks, so `buf` can carry the start
/// of the body past the header terminator — slice to the head before UTF-8
/// validation or a multi-byte character split at the chunk boundary makes the
/// whole lookup fail (same guard as `extract_bearer`).
fn extract_header_value(buf: &[u8], name: &str) -> Option<String> {
    let head_end = find_header_end(buf).unwrap_or(buf.len());
    let text = std::str::from_utf8(&buf[..head_end]).ok()?;
    for line in text.lines() {
        if line.is_empty() {
            break; // end of header block
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case(name) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

// Subset of the `GET /wham/usage` JSON body we map onto a snapshot. Unknown
// fields are ignored by serde.
#[derive(serde::Deserialize)]
struct UsageWindowJson {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(serde::Deserialize)]
struct UsageRateLimitJson {
    primary_window: Option<UsageWindowJson>,
    secondary_window: Option<UsageWindowJson>,
}

#[derive(serde::Deserialize)]
struct UsageCreditsJson {
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    balance: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct UsagePayloadJson {
    rate_limit: Option<UsageRateLimitJson>,
    credits: Option<UsageCreditsJson>,
    rate_limit_reached_type: Option<String>,
}

fn balance_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn codex_window_from_usage(win: &UsageWindowJson, now: i64) -> Option<CodexUsageWindow> {
    let used_percent = win.used_percent?;
    if used_percent.is_nan() {
        return None;
    }
    // Round window seconds up to whole minutes, matching codex-rs.
    let window_minutes = win
        .limit_window_seconds
        .filter(|s| *s > 0)
        .map(|s| (s + 59) / 60);
    Some(CodexUsageWindow {
        used_percent,
        window_label: window_minutes.map(codex_window_label),
        window_minutes,
        seconds_until_reset: win.reset_at.map(|r| (r - now).max(0)),
    })
}

/// Map a parsed `GET /wham/usage` body onto a [`CodexRateLimitSnapshot`].
/// Mirrors `parse_codex_usage_payload` in upstream `codex_rate_limits.py` and
/// the header parser above. Returns `None` when there is no usable signal.
fn codex_snapshot_from_usage_payload(payload: &UsagePayloadJson) -> Option<CodexRateLimitSnapshot> {
    let now = now_epoch_secs() as i64;
    let rate_limit = payload.rate_limit.as_ref();
    let primary = rate_limit
        .and_then(|r| r.primary_window.as_ref())
        .and_then(|w| codex_window_from_usage(w, now));
    let secondary = rate_limit
        .and_then(|r| r.secondary_window.as_ref())
        .and_then(|w| codex_window_from_usage(w, now));

    let (credits_balance, credits_unlimited) = match payload.credits.as_ref() {
        Some(c) => {
            let has_credits = c.has_credits.unwrap_or(false);
            // Only surface a balance when the account has credits; a "0"
            // balance on a no-credits plan is noise to the gauge.
            let balance = if has_credits {
                c.balance
                    .as_ref()
                    .and_then(balance_to_string)
                    .filter(|s| !s.is_empty())
            } else {
                None
            };
            (balance, c.unlimited.unwrap_or(false))
        }
        None => (None, false),
    };

    let limit_name = payload
        .rate_limit_reached_type
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if primary.is_none() && secondary.is_none() && credits_balance.is_none() {
        return None;
    }

    Some(CodexRateLimitSnapshot {
        limit_name,
        primary,
        secondary,
        credits_balance,
        credits_unlimited,
    })
}

/// GET the live Codex usage window (blocking; runs on a `spawn_blocking` thread).
fn fetch_codex_usage_snapshot(
    token: &str,
    account_id: &str,
    user_agent: &str,
) -> Option<CodexRateLimitSnapshot> {
    let client = reqwest::blocking::Client::builder()
        .timeout(CODEX_USAGE_POLL_TIMEOUT)
        .build()
        .ok()?;
    let resp = client
        .get(CODEX_USAGE_URL)
        .bearer_auth(token)
        .header("ChatGPT-Account-Id", account_id)
        .header("User-Agent", user_agent)
        .header("Accept", "application/json")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let payload: UsagePayloadJson = resp.json().ok()?;
    codex_snapshot_from_usage_payload(&payload)
}

/// Fire-and-forget a throttled usage poll to refresh `codex_slot`.
///
/// Scoped to ChatGPT sessions by requiring both a bearer token and a
/// `ChatGPT-Account-Id` header (API-key Codex traffic carries neither), and
/// throttled to one live GET per `CODEX_USAGE_POLL_MIN_INTERVAL_SECS`.
fn maybe_spawn_codex_usage_poll(buf: &[u8], codex_slot: &CodexRateLimitSlot) {
    let Some(token) = extract_bearer(buf) else {
        return;
    };
    let Some(account_id) = extract_header_value(buf, "chatgpt-account-id") else {
        return;
    };

    let now = now_epoch_secs();
    let last = CODEX_USAGE_LAST_POLL.load(Ordering::Relaxed);
    if now.saturating_sub(last) < CODEX_USAGE_POLL_MIN_INTERVAL_SECS {
        return;
    }
    // Claim the slot; lose the race -> another connection is already polling.
    if CODEX_USAGE_LAST_POLL
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let user_agent =
        extract_header_value(buf, "user-agent").unwrap_or_else(|| "headroom-desktop".to_string());
    let slot = codex_slot.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(snapshot) = fetch_codex_usage_snapshot(&token, &account_id, &user_agent) {
            *slot.lock() = Some(snapshot);
        }
    });
}

/// Best-effort decode of one claim from the nested OpenAI auth object in a
/// Codex OAuth bearer JWT. No signature verification: callers use this only
/// to classify routing or show a local plan hint, never to grant access.
pub(crate) fn decode_codex_auth_claim(token: &str, claim: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    // JWT payloads are base64url without padding; tolerate either form.
    let trimmed = payload_b64.trim_end_matches('=');
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    json.get("https://api.openai.com/auth")
        .and_then(|auth| auth.get(claim))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Best-effort decode of the ChatGPT plan from a Codex OAuth bearer JWT.
fn decode_codex_plan_tier(token: &str) -> Option<CodexPlanTier> {
    decode_codex_auth_claim(token, "chatgpt_plan_type").map(|plan| CodexPlanTier::from_claim(&plan))
}

/// ChatGPT subscription OAuth and Platform API keys use different upstreams.
/// The account header is authoritative; the JWT claim covers clients that omit
/// it on a particular request.
fn request_uses_chatgpt_auth(buf: &[u8]) -> bool {
    extract_header_value(buf, "chatgpt-account-id").is_some()
        || extract_bearer(buf)
            .is_some_and(|token| decode_codex_auth_claim(&token, "chatgpt_account_id").is_some())
}

/// Window label derived from a minute count, matching upstream's
/// `CodexRateLimitWindow.window_label` (`<60` -> "Nm", else "Hh" / "HhMMm").
fn codex_window_label(window_minutes: i64) -> String {
    if window_minutes < 60 {
        return format!("{window_minutes}m");
    }
    let hours = window_minutes / 60;
    let mins = window_minutes % 60;
    if mins == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{mins:02}m")
    }
}

static UPSTREAM_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn upstream_client() -> &'static reqwest::Client {
    UPSTREAM_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // Connect timeout only — no overall timeout, since bypassed SSE
            // streams legitimately run for minutes. Without it, a
            // SYN-blackholed network hangs every bypass request until the
            // client's own deadline.
            .connect_timeout(std::time::Duration::from_secs(10))
            // reqwest honors HTTP(S)_PROXY env vars by default, which would
            // silently route "direct to provider" traffic through a corporate
            // proxy the intercept path never uses.
            .no_proxy()
            .build()
            .expect("reqwest client for bypass forwarder")
    })
}

async fn write_retryable_service_unavailable(client: &mut TcpStream) {
    let _ = client
        .write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
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

    // Codex points OPENAI_BASE_URL at this intercept proxy, so in bypass mode
    // OpenAI traffic (e.g. /v1/responses) lands here too. Codex billing is
    // OpenAI's, separate from Headroom's Claude account gate, so don't break
    // Codex when the gate trips — forward Codex requests to OpenAI directly
    // rather than (wrongly) to api.anthropic.com.
    let effective_base: &str = if is_codex_request_head(&parsed) {
        OPENAI_DIRECT_BASE
    } else {
        upstream_base
    };

    let header_value = |name: &str| {
        parsed
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    // A WebSocket/upgrade handshake needs its own path: Upgrade/Connection are
    // hop-by-hop for the plain forward below, and Codex's current transport is
    // WS on /v1/responses — a 501 here would hard-break Codex in exactly the
    // bypass modes meant to keep it alive. Tunnel the upgrade via hyper's
    // connection takeover instead.
    if header_value("upgrade").is_some() {
        let url = format!("{}{}", effective_base, parsed.path);
        tunnel_upgrade_direct(client, &parsed, leftover_body, &url).await;
        return;
    }

    // A chunked body can't be reassembled here — body reading below tracks
    // Content-Length only, so forwarding would silently truncate the request.
    // The CLI clients always send Content-Length; answer 411 honestly for
    // anything that doesn't.
    if parsed.content_length.is_none()
        && header_value("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        let _ = client
            .write_all(b"HTTP/1.1 411 Length Required\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    // An `Expect: 100-continue` client holds the body back until it sees the
    // interim response — without this it deadlocks against our body read
    // below until one side times out.
    if header_value("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
        && client
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .is_err()
    {
        return;
    }

    // Cap the client-declared body before it sizes the allocation below.
    if parsed.content_length.is_some_and(|n| n > MAX_DIRECT_BODY) {
        let _ = client
            .write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    let body = match parsed.content_length {
        Some(total) if total > leftover_body.len() => {
            let mut body = Vec::with_capacity(total);
            body.extend_from_slice(leftover_body);
            let mut remaining = vec![0u8; total - leftover_body.len()];
            // Timeout like every other socket read in this file — a client
            // that stalls mid-body must not pin this task forever.
            match tokio::time::timeout(BODY_READ_TIMEOUT, client.read_exact(&mut remaining)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
            body.extend_from_slice(&remaining);
            body
        }
        Some(total) => leftover_body[..total.min(leftover_body.len())].to_vec(),
        None => leftover_body.to_vec(),
    };

    // Stale tool-search reference rescue, direct path only. Anthropic
    // validates every replayed tool_reference against this request's tools
    // array and 400s the whole request when one is missing — permanently,
    // since the client replays the same history every turn. The Python
    // backend sanitizes optimized traffic (headroomlabs-ai/headroom#2507),
    // but bypass/backend-down requests skip it — and a session that did CCR
    // while optimized references the proxy-injected headroom_retrieve tool,
    // so entering bypass would brick it exactly when Headroom pauses. The
    // rewrite is the API's own validation predicate: it only ever converts a
    // guaranteed 400 into a working request. Content-Length is hop-by-hop
    // here (reqwest recomputes it), so shrinking the body is safe.
    let body = if is_codex_request_head(&parsed) {
        body
    } else {
        sanitize_stale_tool_references(body, &parsed.path)
    };

    let url = format!("{}{}", effective_base, parsed.path);
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
            log::warn!("proxy_intercept bypass forward failed: {e}");
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };
    if resp.status().as_u16() == 429 {
        crate::usage_counters::record_429(if is_codex_request_head(&parsed) {
            "codex"
        } else {
            "claude-code"
        });
    }

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
                log::debug!("[proxy_intercept] bypass body stream error: {e}");
                return;
            }
        }
    }
    let _ = client.write_all(b"0\r\n\r\n").await;
}

/// Tunnel a WebSocket/upgrade handshake to the upstream through the shared
/// reqwest client. hyper keeps the connection on a 101 and hands it over via
/// `Response::upgrade()`, after which both sockets are spliced verbatim. Used
/// by the bypass forwarder so gated/bypassed Codex WS sessions keep working.
async fn tunnel_upgrade_direct(
    mut client: TcpStream,
    parsed: &ParsedRequestHead,
    leftover: &[u8],
    url: &str,
) {
    let method = match reqwest::Method::from_bytes(parsed.method.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            let _ = client
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    let mut req = upstream_client().request(method, url);
    for (name, value) in &parsed.headers {
        // Unlike the plain forward, Connection/Upgrade/Sec-WebSocket-* must
        // survive: hyper needs the upgrade intent to keep the connection for
        // takeover. Only strip what we rewrite ourselves.
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        req = req.header(name, value);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("proxy_intercept bypass upgrade forward failed: {e}");
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    let status = resp.status();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (name, value) in resp.headers().iter() {
        if name.as_str().eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        if let Ok(v) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), v));
        }
    }
    head.push_str("\r\n");

    if status != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        // Handshake refused — relay the upstream's verdict and close.
        let body = resp.bytes().await.unwrap_or_default();
        if client.write_all(head.as_bytes()).await.is_ok() {
            let _ = client.write_all(&body).await;
        }
        return;
    }

    let mut upstream = match resp.upgrade().await {
        Ok(u) => u,
        Err(e) => {
            log::warn!("proxy_intercept bypass upgrade takeover failed: {e}");
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };
    if client.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    // Frames the client sent before the handshake completed.
    if !leftover.is_empty() && upstream.write_all(leftover).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
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

/// Drop replayed tool-search `tool_reference` entries whose `tool_name` is
/// absent from the request's `tools` array. Anthropic rejects the whole
/// request with `400 Tool reference '<name>' not found in available tools`
/// otherwise, and since the client replays the same history every turn the
/// session stays bricked. The filter is exactly the API's validation
/// predicate, so a resolvable reference is never touched and any change can
/// only turn a guaranteed 400 into a working request. Fails open: anything
/// unexpected (non-messages path, no marker substring, unparseable JSON)
/// returns the body unchanged.
fn sanitize_stale_tool_references(body: Vec<u8>, path: &str) -> Vec<u8> {
    if !path.starts_with("/v1/messages") {
        return body;
    }
    const NEEDLE: &[u8] = b"tool_search_tool_result";
    if !body.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
        return body;
    }
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let available: std::collections::HashSet<String> = value
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut dropped: Vec<String> = Vec::new();
    if let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for message in messages {
            let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_search_tool_result") {
                    continue;
                }
                // GA wire shape nests refs at content.tool_references (dict);
                // a list-shaped content is handled defensively.
                let refs = match block.get_mut("content") {
                    Some(serde_json::Value::Object(obj)) => obj.get_mut("tool_references"),
                    Some(list @ serde_json::Value::Array(_)) => Some(list),
                    _ => None,
                };
                let Some(serde_json::Value::Array(refs)) = refs else {
                    continue;
                };
                refs.retain(|r| {
                    let name = r.get("tool_name").and_then(|n| n.as_str());
                    let is_stale = r.get("type").and_then(|t| t.as_str()) == Some("tool_reference")
                        && name.is_some_and(|n| !available.contains(n));
                    if is_stale {
                        dropped.push(name.unwrap_or_default().to_owned());
                    }
                    !is_stale
                });
            }
        }
    }
    if dropped.is_empty() {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(rewritten) => {
            log::warn!(
                "[proxy_intercept] dropped {} stale tool_search reference(s) {:?} from a direct-forwarded request — absent from the tools array, upstream would 400 the session permanently",
                dropped.len(),
                dropped
            );
            rewritten
        }
        Err(_) => body,
    }
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

/// Case-insensitive check for a header field name in an HTTP request head.
/// `buf` is the full request including the `\r\n\r\n` terminator; only field
/// names (the text before the first `:` on each header line) are matched.
fn request_has_header(buf: &[u8], name: &str) -> bool {
    let end = find_header_end(buf).unwrap_or(buf.len());
    let Ok(text) = std::str::from_utf8(&buf[..end]) else {
        return false;
    };
    text.split("\r\n")
        .skip(1) // request line
        .filter_map(|line| line.split_once(':'))
        .any(|(field, _)| field.trim().eq_ignore_ascii_case(name))
}

/// Remove a header line (case-insensitive field name) from an HTTP request
/// head, preserving the request line, every other header, the `\r\n\r\n`
/// terminator and the body. No-op if the header is absent or the terminator is
/// missing. `Content-Length` is unaffected: it counts body bytes, untouched here.
fn strip_request_header(buf: &mut Vec<u8>, name: &str) {
    let range = {
        let Some(end) = find_header_end(buf) else {
            return;
        };
        let Ok(head) = std::str::from_utf8(&buf[..end]) else {
            return;
        };
        let mut offset = match head.find("\r\n") {
            Some(p) => p + 2, // skip the request line
            None => return,
        };
        let mut found = None;
        while offset < head.len() {
            let rest = &head[offset..];
            let line_len = rest.find("\r\n").unwrap_or(rest.len());
            if rest[..line_len]
                .split_once(':')
                .map(|(field, _)| field.trim().eq_ignore_ascii_case(name))
                .unwrap_or(false)
            {
                found = Some(offset..offset + line_len + 2);
                break;
            }
            offset += line_len + 2;
        }
        found
    };
    if let Some(r) = range {
        let stop = r.end.min(buf.len());
        buf.splice(r.start..stop, std::iter::empty());
    }
}

/// Rewrite the request head to `Connection: close` so the backend closes the
/// connection after one response (and echoes the header, so the client opens
/// a fresh connection for its next request instead of reusing this one).
///
/// Everything this proxy does per request — origin check, bearer capture,
/// lite-header strip, `X-Client: codex` stamp — is applied only to the first
/// request head on a connection; after that the socket is an opaque splice,
/// so a keep-alive reuse would carry a second request past all of it. One
/// request per connection makes the interception complete by construction,
/// at the cost of a loopback TCP handshake per request. No-op if the header
/// terminator is missing.
fn force_connection_close(buf: &mut Vec<u8>) {
    if find_header_end(buf).is_none() {
        return;
    }
    while request_has_header(buf, "connection") {
        strip_request_header(buf, "connection");
    }
    let Some(end) = find_header_end(buf) else {
        return;
    };
    let insert_at = end + 2;
    buf.splice(insert_at..insert_at, *b"Connection: close\r\n");
}

/// Insert `X-Client: codex` into a request head so the Python backend's
/// `classify_client` identifies Codex traffic even when the client's
/// User-Agent isn't `codex-cli/` (e.g. the Codex GUI/IDE). A client that
/// already self-identified via `X-Client` is left untouched. No-op if the
/// header terminator is missing.
fn stamp_codex_client_header(buf: &mut Vec<u8>) {
    stamp_client_header(buf, b"X-Client: codex\r\n");
}

fn stamp_client_header(buf: &mut Vec<u8>, header_line: &'static [u8]) {
    stamp_request_header(buf, "x-client", header_line);
}

/// Append `header_line` as the last request header unless a header named
/// `guard_name` is already present. No-op if the terminator is missing.
fn stamp_request_header(buf: &mut Vec<u8>, guard_name: &str, header_line: &'static [u8]) {
    if request_has_header(buf, guard_name) {
        return;
    }
    let Some(end) = find_header_end(buf) else {
        return;
    };
    // `end` points at the first `\r` of the `\r\n\r\n` terminator. Inserting at
    // `end + 2` (start of the blank line) appends a new last header line while
    // preserving the terminating CRLF.
    let insert_at = end + 2;
    buf.splice(insert_at..insert_at, header_line.iter().copied());
}

/// Tell the Python backend to preserve routing/auth handling but skip Headroom
/// optimization for this Codex request.
fn stamp_headroom_bypass_header(buf: &mut Vec<u8>) {
    if request_has_header(buf, "x-headroom-bypass") {
        return;
    }
    let Some(end) = find_header_end(buf) else {
        return;
    };
    let insert_at = end + 2;
    buf.splice(insert_at..insert_at, *b"X-Headroom-Bypass: true\r\n");
}

/// Paths served by the local Python proxy (not Anthropic). Matches the prefix
/// so sub-paths (e.g. `/transformations/feed`) and query strings are covered,
/// while preventing partial matches (e.g. `/healthcheck` does not match
/// `/health`).
/// Paths a client requests to check reachability, never to reach a provider.
/// The backend has no route for them, so the status is always an error and
/// never one a release of ours can change: /api/hello is Claude Code's
/// connectivity probe (backend 404s, Claude Code accepts any status as proof
/// of life: RUST-BS); the bare root draws the backend's 421 unrouted-path gate
/// (RUST-BY: 25 events across 7 hosts in 12h, every one on "/"); /v1/settings
/// is grok-build's startup config fetch, which api.x.ai answers 404 text/plain
/// with or without us (RUST-CG: 4 same-second events on one host). /mcp is not
/// a provider path at all: no client we support reaches a provider through it,
/// the backend has no route for it, and the one time it arrived (RUST-CV,
/// opencode, a JSON-RPC-shaped 405) it was an MCP client aimed at Headroom's
/// base URL, which nothing we ship answers. Deliberately NOT folded into
/// is_local_proxy_path: bypass mode must keep forwarding these upstream (where
/// /api/hello 200s), not answer 503.
fn is_client_probe_path(path: &str) -> bool {
    matches!(path, "/" | "/api/hello" | "/v1/settings" | "/mcp")
}

fn is_local_proxy_path(path: &str) -> bool {
    const LOCAL_PREFIXES: &[&str] = &[
        "/readyz",
        "/livez",
        "/health",
        "/stats",
        // Needs its own entry: the boundary check below treats "-" as a new
        // path, so "/stats" does not cover it. Its absence made the desktop's
        // own savings poll count as client traffic — the first one after the
        // backend came up fired the first_optimized_request beacon in the same
        // second as bootstrap_completed, for every install, agent or not
        // (exactly the false positive 1abf148 fixed for /stats).
        "/stats-history",
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

/// OpenAI-specific API paths used by the Codex CLI. These have no Anthropic
/// counterpart (Claude uses `/v1/messages` / `/v1/complete`), so matching by
/// path is unambiguous and lets bypass-mode forward Codex traffic to OpenAI.
fn is_openai_path(path: &str) -> bool {
    const OPENAI_PREFIXES: &[&str] = &[
        "/v1/responses",
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/embeddings",
    ];
    OPENAI_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('?'))
    })
}

/// A real prompt carries the agent's multi-kilobyte system prompt; startup
/// noise (quota ping, models fetch, count_tokens probes) stays well under
/// 2 KB, so the threshold sits in a wide safe band between the two.
const PROMPT_REQUEST_MIN_BODY_BYTES: usize = 8 * 1024;

/// True for a "user actually prompted an agent" request, as opposed to
/// agent-startup noise: a POST to a completion endpoint with a prompt-sized
/// body. Subpaths are deliberately excluded (`/v1/messages/count_tokens`
/// carries a full conversation but is not a prompt); query strings count
/// (Claude Code posts `/v1/messages?beta=true`). Title-generation calls can
/// pass the size bar, but those only ever follow a real prompt, so the
/// funnel meaning holds. A chunked body (no Content-Length) is skipped; the
/// next prompt fires the once-per-process beacon instead.
fn is_prompt_request_head(head: &ParsedRequestHead) -> bool {
    const PROMPT_PATHS: &[&str] = &["/v1/messages", "/v1/responses", "/v1/chat/completions"];
    head.method.eq_ignore_ascii_case("POST")
        && PROMPT_PATHS.iter().any(|prefix| {
            head.path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('?'))
        })
        && head
            .content_length
            .is_some_and(|len| len >= PROMPT_REQUEST_MIN_BODY_BYTES)
}

fn request_head_has_header(head: &ParsedRequestHead, name: &str) -> bool {
    head.headers
        .iter()
        .any(|(field, _)| field.eq_ignore_ascii_case(name))
}

fn is_codex_models_fetch(head: &ParsedRequestHead) -> bool {
    head.method.eq_ignore_ascii_case("GET")
        && (head.path == "/v1/models" || head.path.starts_with("/v1/models?"))
        // `/v1/models` exists on both providers. Claude Code sends Anthropic
        // markers; Codex does not.
        && !request_head_has_header(head, "anthropic-version")
        && !request_head_has_header(head, "x-api-key")
}

fn is_codex_request_head(head: &ParsedRequestHead) -> bool {
    is_openai_path(&head.path) || is_codex_models_fetch(head)
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
/// Only the header block is scanned: `read_http_headers` over-reads, so `buf`
/// can carry the start of the body, and body bytes must never be able to
/// plant an Authorization line that poisons the captured token.
fn extract_bearer(buf: &[u8]) -> Option<String> {
    let end = find_header_end(buf).unwrap_or(buf.len());
    let text = std::str::from_utf8(&buf[..end]).ok()?;
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
        bearer_value_changed, bind_intercept, classify_held_port, codex_error_shape_tag,
        codex_error_summary, codex_snapshot_from_usage_payload, codex_window_label,
        decode_codex_plan_tier, extract_bearer, extract_header_value, find_header_end,
        intercept_request_counts, is_client_probe_path, is_codex_request_head,
        is_codex_sse_response, is_geo_blocked_codex_error, is_hop_by_hop_request_header,
        is_hop_by_hop_response_header, is_local_proxy_path, is_missing_auth_error, is_openai_path,
        is_prompt_request_head, is_reportable_upstream_error, os_error_key,
        parse_codex_rate_limit_headers, parse_request_head, parse_response_status,
        read_http_headers, request_has_header, request_is_loopback_safe, request_uses_chatgpt_auth,
        response_content_type, rewrite_use_responses_lite, run, sanitize_stale_tool_references,
        set_response_content_length, should_report_throttled, should_report_upstream_error,
        stamp_client_header, stamp_codex_client_header, stamp_headroom_bypass_header,
        stamp_request_header, strip_request_header, verdict_permits_reuse, BypassFlag,
        CodexTerminalReader, HeldPortVerdict, ModelsRewrite, ParsedRequestHead, ResponseSniffer,
        SharedToken, FIRST_OPTIMIZED_REQUEST_REPORTED,
    };
    use crate::backend_port;
    use crate::bearer::BearerToken;
    use crate::models::CodexPlanTier;
    use base64::Engine;
    use parking_lot::Mutex;
    use serial_test::serial;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Arc;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// The whole point of the fix: the SAME bug on two differently-localized
    /// machines must produce one Sentry fingerprint, not two (RUST-7B Spanish
    /// vs RUST-7D English, both WSAEADDRINUSE).
    #[test]
    fn os_error_key_is_locale_invariant() {
        // Win32 10048 (WSAEADDRINUSE) as the OS reports it in two languages.
        let english = std::io::Error::from_raw_os_error(10048);
        let spanish = std::io::Error::from_raw_os_error(10048);
        assert_eq!(os_error_key(&english), os_error_key(&spanish));
        assert_eq!(os_error_key(&english), "os error 10048");

        // A different code must still be a different key.
        assert_ne!(
            os_error_key(&std::io::Error::from_raw_os_error(10048)),
            os_error_key(&std::io::Error::from_raw_os_error(10061))
        );

        // Non-OS errors have no code; fall back to the text so they stay distinct.
        let synthetic = std::io::Error::other("synthesized by a wrapper");
        assert_eq!(os_error_key(&synthetic), "synthesized by a wrapper");
    }

    const HELD_GRACE: std::time::Duration = std::time::Duration::from_secs(300);

    /// RUST-7M: Windows-only, one event per update relaunch, every one
    /// self-healing. Nothing is LISTENING because the port is held by the
    /// previous instance's connections in TIME_WAIT, so there is no pid to
    /// name and nothing for the user to do about it.
    #[test]
    fn a_listener_less_port_inside_the_grace_is_draining_not_an_error() {
        assert_eq!(
            classify_held_port(None, std::time::Duration::from_secs(120), HELD_GRACE),
            HeldPortVerdict::Draining
        );
    }

    /// The 90s RELAUNCH_GRACE expires while Windows' TcpTimedWaitDelay (120s
    /// by default) is still running, which is exactly why RUST-7M reported at
    /// all. Past 90s but inside the drain window must stay quiet.
    #[test]
    fn the_relaunch_grace_alone_does_not_cover_the_windows_drain() {
        assert_eq!(
            classify_held_port(None, std::time::Duration::from_secs(91), HELD_GRACE),
            HeldPortVerdict::Draining
        );
    }

    /// The safety boundary for the SO_REUSEADDR rebind. Draining is the only
    /// verdict that establishes nothing is listening, and on Windows
    /// SO_REUSEADDR binds over live listeners too -- so widening this to any
    /// other verdict would let a second Headroom bind 6767 alongside the
    /// first, or park us next to a foreign holder, instead of failing.
    #[test]
    fn only_a_draining_port_may_be_rebound_with_reuseaddr() {
        assert!(verdict_permits_reuse(&HeldPortVerdict::Draining));
        assert!(!verdict_permits_reuse(&HeldPortVerdict::Stuck));
        assert!(!verdict_permits_reuse(&HeldPortVerdict::Foreign {
            name: "Affinity".into(),
            pid: 54915
        }));
    }

    /// Off Windows the flag is inert (Unix already sets SO_REUSEADDR), so both
    /// paths must still produce a usable listener.
    #[tokio::test]
    async fn bind_intercept_binds_either_way() {
        for reuse in [false, true] {
            let listener = bind_intercept("127.0.0.1:0".parse().unwrap(), reuse)
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local_addr");
            assert_ne!(addr.port(), 0);
            let joined = tokio::spawn(async move { listener.accept().await.is_ok() });
            let _client = TcpStream::connect(addr).await.expect("connect");
            assert!(joined.await.expect("join"));
        }
    }

    #[test]
    fn a_listener_less_port_past_the_grace_is_stuck() {
        assert_eq!(
            classify_held_port(None, std::time::Duration::from_secs(301), HELD_GRACE),
            HeldPortVerdict::Stuck
        );
    }

    /// A named holder does not clear on its own, so the drain window is
    /// irrelevant to it -- report immediately rather than sitting on it 300s.
    #[test]
    fn a_live_listener_is_foreign_however_early_it_shows_up() {
        assert_eq!(
            classify_held_port(
                Some(("Affinity".into(), 54915)),
                std::time::Duration::from_secs(1),
                HELD_GRACE
            ),
            HeldPortVerdict::Foreign {
                name: "Affinity".into(),
                pid: 54915
            }
        );
    }

    #[test]
    fn a_live_listener_past_the_grace_is_still_named_not_stuck() {
        assert_eq!(
            classify_held_port(
                Some(("node".into(), 99)),
                std::time::Duration::from_secs(9_999),
                HELD_GRACE
            ),
            HeldPortVerdict::Foreign {
                name: "node".into(),
                pid: 99
            }
        );
    }
    use tokio::time::{timeout, Duration};

    #[test]
    #[serial]
    fn codex_reconnect_reports_suppress_within_window() {
        use std::sync::atomic::Ordering;
        super::SUPPRESS_RECONNECT_UNTIL.store(0, Ordering::Release);
        assert!(!super::codex_reconnect_reports_suppressed());
        super::suppress_codex_reconnect_reports_for(Duration::from_secs(3600));
        assert!(super::codex_reconnect_reports_suppressed());
        super::SUPPRESS_RECONNECT_UNTIL.store(0, Ordering::Release);
    }

    #[test]
    #[serial]
    fn launch_time_unreachable_does_not_arm_the_down_timer() {
        use std::sync::atomic::Ordering;
        let addr: SocketAddr = "127.0.0.1:6768".parse().unwrap();
        super::BACKEND_REACHABILITY_STATE.store(0, Ordering::Release);
        *super::BACKEND_DOWN_SINCE.lock() = None;

        // Backend still booting when the first request lands: no down window.
        super::note_backend_reachability(false, addr);
        assert!(super::BACKEND_DOWN_SINCE.lock().is_none());

        // A later drop, once we have seen it up, is a real outage.
        super::note_backend_reachability(true, addr);
        super::note_backend_reachability(false, addr);
        assert!(super::BACKEND_DOWN_SINCE.lock().is_some());

        super::BACKEND_REACHABILITY_STATE.store(0, Ordering::Release);
        *super::BACKEND_DOWN_SINCE.lock() = None;
    }

    #[test]
    #[serial]
    fn backend_traffic_window_tracks_stamps() {
        use std::sync::atomic::Ordering;
        super::BACKEND_LAST_TRAFFIC_EPOCH.store(0, Ordering::Release);
        assert!(!super::backend_traffic_within(Duration::from_secs(10)));
        super::stamp_backend_traffic();
        assert!(super::backend_traffic_within(Duration::from_secs(10)));
        super::BACKEND_LAST_TRAFFIC_EPOCH.store(
            super::now_epoch_secs().saturating_sub(11),
            Ordering::Release,
        );
        assert!(!super::backend_traffic_within(Duration::from_secs(10)));
    }

    #[tokio::test]
    #[serial]
    async fn stamp_reader_stamps_on_backend_bytes() {
        use std::sync::atomic::Ordering;
        super::BACKEND_LAST_TRAFFIC_EPOCH.store(0, Ordering::Release);
        let (mut writer, backend_side) = duplex(64);
        writer.write_all(b"data: chunk\n\n").await.unwrap();
        let mut reader = super::StampReader(backend_side);
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(n, 13);
        assert!(super::backend_traffic_within(Duration::from_secs(10)));
    }

    #[test]
    fn finds_header_boundary() {
        let request = b"POST /v1/messages HTTP/1.1\r\nHost: localhost\r\n\r\n{\"x\":1}";
        assert_eq!(find_header_end(request), Some(43));
    }

    #[test]
    fn openai_paths_route_to_openai_in_bypass() {
        // Codex's Responses API and the OpenAI chat/completions family must be
        // recognized as OpenAI traffic so bypass mode forwards them to OpenAI,
        // not api.anthropic.com.
        assert!(is_openai_path("/v1/responses"));
        assert!(is_openai_path("/v1/responses/abc?stream=true"));
        assert!(is_openai_path("/v1/chat/completions"));
        assert!(is_openai_path("/v1/completions"));
        assert!(is_openai_path("/v1/embeddings"));
        // Anthropic paths must NOT be misrouted to OpenAI.
        assert!(!is_openai_path("/v1/messages"));
        assert!(!is_openai_path("/v1/complete"));
        assert!(!is_openai_path("/v1/models"));
        // Codex's own usage tracker endpoints stay local.
        assert!(is_local_proxy_path("/stats"));
        assert!(!is_openai_path("/stats"));
        // The desktop's own savings poll: "-" is not a path boundary, so this
        // needs its own prefix entry or it counts as client traffic and fires
        // the first_optimized_request beacon at bootstrap-complete.
        assert!(is_local_proxy_path("/stats-history"));
        assert!(is_local_proxy_path("/stats-history?limit=100"));
    }

    #[test]
    fn models_fetch_is_codex_only_without_anthropic_markers() {
        let codex =
            parse_request_head(b"GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        assert!(is_codex_request_head(&codex));

        let anthropic = parse_request_head(
        b"GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\nanthropic-version: 2023-06-01\r\nx-api-key: key\r\n\r\n",
    )
    .unwrap();
        assert!(!is_codex_request_head(&anthropic));
    }

    #[test]
    fn extracts_bearer_token_case_insensitively() {
        let request = b"POST / HTTP/1.1\r\nAuthorization: Bearer test-token\r\n\r\n";
        assert_eq!(extract_bearer(request).as_deref(), Some("test-token"));
    }

    #[test]
    fn detects_chatgpt_codex_auth_from_header_or_jwt() {
        assert!(request_uses_chatgpt_auth(
            b"POST /v1/responses HTTP/1.1\r\nChatGPT-Account-Id: acct_1\r\n\r\n"
        ));

        let jwt = jwt_with_plan("plus");
        let request = format!("POST /v1/responses HTTP/1.1\r\nAuthorization: Bearer {jwt}\r\n\r\n");
        assert!(request_uses_chatgpt_auth(request.as_bytes()));

        assert!(!request_uses_chatgpt_auth(
            b"POST /v1/responses HTTP/1.1\r\nAuthorization: Bearer sk-platform-key\r\n\r\n"
        ));
    }

    #[test]
    fn extract_bearer_ignores_authorization_lines_in_the_body() {
        // read_http_headers over-reads, so the buffer can contain body bytes.
        // A body line that looks like an Authorization header must not be
        // captured as a credential.
        let request = b"POST / HTTP/1.1\r\nContent-Type: text/plain\r\n\r\nAuthorization: Bearer attacker-value\r\n";
        assert_eq!(extract_bearer(request), None);

        // A real header still wins with body bytes present.
        let request =
            b"POST / HTTP/1.1\r\nAuthorization: Bearer real\r\n\r\nAuthorization: Bearer fake\r\n";
        assert_eq!(extract_bearer(request).as_deref(), Some("real"));
    }

    #[test]
    fn bearer_value_changed_treats_empty_slot_as_changed() {
        let slot: SharedToken = Arc::new(Mutex::new(None));
        assert!(bearer_value_changed(&slot, "any-token"));
    }

    #[test]
    fn bearer_value_changed_skips_signal_when_value_matches() {
        let slot: SharedToken = Arc::new(Mutex::new(Some(BearerToken::new("token-A".into()))));
        assert!(!bearer_value_changed(&slot, "token-A"));
    }

    #[test]
    fn bearer_value_changed_signals_when_value_differs() {
        let slot: SharedToken = Arc::new(Mutex::new(Some(BearerToken::new("token-A".into()))));
        assert!(bearer_value_changed(&slot, "token-B"));
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
    #[serial]
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

        // Point the intercept's per-connection backend lookup at our fake
        // backend's ephemeral port. Serialized via #[serial] so tests that
        // mutate the global don't race. Deliberately the unnamed key: every
        // serial test in this crate shares it. A named group (#[serial(foo)])
        // is an INDEPENDENT lock, so it would let a backend-port test run
        // alongside an unnamed one and hand the loser a port pointing at
        // someone else's fixture. Do not reintroduce named groups here.
        backend_port::set(backend_addr.port());

        // Run the intercept on its own ephemeral port.
        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener); // free the port; run() rebinds the same one
        let slot_for_run = token_slot.clone();
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        // Unroutable on purpose. These tests assert on a local fake backend and
        // must never reach the real API: when a stale backend_port sent them down
        // the direct-to-provider fallback, they silently made live calls to
        // api.anthropic.com and failed on the resulting 401 body instead of saying
        // so. Port 1 on loopback refuses instantly, so a stray fallback is a fast,
        // legible failure rather than a network round trip.
        let upstream_base = Arc::new("http://127.0.0.1:1".to_string());
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            // run() loops forever; the test cancels it via abort below.
            let _ = run(
                intercept_addr,
                false,
                slot_for_run,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass_for_run,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base,
                Arc::new(Mutex::new(None)),
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
        backend_port::reset_for_tests();
    }

    /// The desktop polls its own dashboard through this listener
    /// (`127.0.0.1:6767/stats`), so a local path reaching a live backend must
    /// not fire the `first_optimized_request` funnel beacon -- it is supposed to
    /// mean "a coding tool sent a request", and self-polling made it fire in the
    /// same second bootstrap finished, before any client was configured.
    ///
    /// Only the negative case is asserted: letting the beacon fire would spawn a
    /// thread POSTing to the real headroom-web.
    #[tokio::test]
    #[serial]
    async fn local_backend_path_does_not_fire_first_optimized_request() {
        use std::sync::atomic::Ordering;

        let (backend_listener, backend_addr) = bind_ephemeral().await;
        let backend_task = tokio::spawn(async move {
            let (mut sock, _) = backend_listener.accept().await.expect("backend accept");
            read_until_header_end(&mut sock).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        backend_port::set(backend_addr.port());

        // Other tests in this serial group may have flipped the one-shot; clear
        // it so the assertion below is about this request and not test order.
        FIRST_OPTIMIZED_REQUEST_REPORTED.store(false, Ordering::Release);

        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        // Unroutable on purpose -- see the sibling test: a stray direct fallback
        // must fail fast locally instead of calling api.anthropic.com.
        let upstream_base = Arc::new("http://127.0.0.1:1".to_string());
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base,
                Arc::new(Mutex::new(None)),
            )
            .await;
        });

        let mut client = None;
        for _ in 0..50 {
            if let Ok(stream) = TcpStream::connect(intercept_addr).await {
                client = Some(stream);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("connect to intercept");
        client
            .write_all(b"GET /stats HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write stats probe");

        // The backend task completing proves the request was forwarded, so the
        // beacon's fire site was reached and the guard is what held it back.
        backend_task.await.expect("backend served the stats probe");

        assert!(
            !FIRST_OPTIMIZED_REQUEST_REPORTED.load(Ordering::Acquire),
            "/stats is the app polling itself, not client traffic"
        );

        run_task.abort();
        backend_port::reset_for_tests();
    }

    #[tokio::test]
    #[serial]
    async fn intercept_falls_back_direct_when_backend_is_unreachable() {
        // Pick a backend port that nothing is listening on. Bind+immediately
        // drop a listener to grab a free port, then connect attempts will fail.
        let (probe, dead_backend_addr) = bind_ephemeral().await;
        drop(probe);
        backend_port::set(dead_backend_addr.port());

        // Mock upstream: answers 200 to whatever arrives. API traffic must
        // land here (per-request direct fallback) instead of getting a 502.
        let (upstream_listener, upstream_addr) = bind_ephemeral().await;
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = upstream_listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let slot_for_run = token_slot.clone();
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        let upstream_base = Arc::new(format!("http://127.0.0.1:{}", upstream_addr.port()));
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                slot_for_run,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass_for_run,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base,
                Arc::new(Mutex::new(None)),
            )
            .await;
        });

        let read_response = |mut client: TcpStream| async move {
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
            response
        };

        let mut client = None;
        for _ in 0..50 {
            if let Ok(c) = TcpStream::connect(intercept_addr).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut client = client.expect("intercept reachable");

        let counts_before = intercept_request_counts();

        let request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\n\r\n",
            intercept_addr.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let response = read_response(client).await;
        let response_str = std::str::from_utf8(&response).unwrap_or("");
        assert!(
            response_str.starts_with("HTTP/1.1 200"),
            "expected direct-to-upstream 200 fallback, got: {response_str:?}"
        );

        // The passthrough forward must still count as provider-bound traffic
        // (paywall-first setup verification relies on this with no backend).
        let counts_after_api = intercept_request_counts();
        assert_eq!(
            counts_after_api["claude-code"] - counts_before["claude-code"],
            1,
            "claude-code counter should increment on passthrough forward"
        );
        assert_eq!(
            counts_after_api["codex"], counts_before["codex"],
            "codex counter should not move for an Anthropic-path request"
        );

        // ChatGPT-authenticated Codex cannot use the Platform API direct
        // fallback. It must retry until the auth-aware Python backend returns.
        let mut codex_client = TcpStream::connect(intercept_addr)
            .await
            .expect("codex connect");
        codex_client
            .write_all(
                b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1\r\nChatGPT-Account-Id: acct_1\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .expect("write codex request");
        let response = read_response(codex_client).await;
        let response_str = std::str::from_utf8(&response).unwrap_or("");
        assert!(
            response_str.starts_with("HTTP/1.1 503"),
            "expected retryable 503 for ChatGPT Codex, got: {response_str:?}"
        );
        assert!(
            response_str.contains("\r\nRetry-After: 1\r\n"),
            "ChatGPT Codex 503 should ask the client to retry: {response_str:?}"
        );
        let counts_after_codex = intercept_request_counts();
        assert_eq!(
            counts_after_codex["codex"] - counts_after_api["codex"],
            1,
            "codex counter should increment on retryable fallback"
        );
        assert_eq!(
            counts_after_codex["claude-code"], counts_after_api["claude-code"],
            "claude-code counter should not move for Codex request"
        );

        // Local proxy paths (health probes, stats) must NOT leak upstream on
        // fallback: the boot-time readyz poll would otherwise flap green and
        // real probes would generate provider traffic every 250ms.
        let mut probe_client = TcpStream::connect(intercept_addr)
            .await
            .expect("probe connect");
        probe_client
            .write_all(b"GET /readyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write probe");
        let response = read_response(probe_client).await;
        let response_str = std::str::from_utf8(&response).unwrap_or("");
        assert!(
            response_str.starts_with("HTTP/1.1 503"),
            "expected local 503 for /readyz on fallback, got: {response_str:?}"
        );

        // Probes are not client traffic: local paths must not count.
        let counts_after_probe = intercept_request_counts();
        assert_eq!(
            counts_after_probe["claude-code"], counts_after_api["claude-code"],
            "/readyz probe must not increment the claude-code counter"
        );

        run_task.abort();
        backend_port::reset_for_tests();
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
    fn prompt_request_head_matches_real_prompts_only() {
        let head = |method: &str, path: &str, content_length: Option<usize>| ParsedRequestHead {
            method: method.into(),
            path: path.into(),
            headers: Vec::new(),
            content_length,
        };
        // Real prompts: prompt-sized completion POSTs, query string allowed.
        assert!(is_prompt_request_head(&head(
            "POST",
            "/v1/messages?beta=true",
            Some(20_000)
        )));
        assert!(is_prompt_request_head(&head(
            "POST",
            "/v1/messages",
            Some(8192)
        )));
        assert!(is_prompt_request_head(&head(
            "POST",
            "/v1/responses",
            Some(30_000)
        )));
        assert!(is_prompt_request_head(&head(
            "POST",
            "/v1/chat/completions",
            Some(9000)
        )));
        // Startup noise: tiny quota ping, models fetch.
        assert!(!is_prompt_request_head(&head(
            "POST",
            "/v1/messages",
            Some(500)
        )));
        assert!(!is_prompt_request_head(&head("GET", "/v1/models", None)));
        // count_tokens carries a full conversation but is not a prompt.
        assert!(!is_prompt_request_head(&head(
            "POST",
            "/v1/messages/count_tokens?beta=true",
            Some(50_000)
        )));
        // Chunked body (no Content-Length): skip, next prompt fires it.
        assert!(!is_prompt_request_head(&head("POST", "/v1/messages", None)));
    }

    #[test]
    fn stamp_codex_client_header_inserts_last_header() {
        let mut buf =
            b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:6767\r\nUser-Agent: codex_vscode/1.0\r\n\r\n"
                .to_vec();
        stamp_codex_client_header(&mut buf);
        let parsed = parse_request_head(&buf).expect("still a valid request head");
        assert_eq!(parsed.path, "/v1/responses");
        assert!(
            parsed
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-client") && v == "codex"),
            "X-Client: codex should be present: {:?}",
            parsed.headers
        );
        // Header block stays well-formed (single blank-line terminator).
        assert!(buf.ends_with(b"X-Client: codex\r\n\r\n"));
        assert_eq!(buf.windows(4).filter(|w| *w == b"\r\n\r\n").count(), 1);
    }

    #[test]
    fn stamp_client_header_stamps_opencode() {
        let mut buf = b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:6767\r\n\r\n".to_vec();
        stamp_client_header(&mut buf, b"X-Client: opencode\r\n");
        assert!(buf.ends_with(b"X-Client: opencode\r\n\r\n"));
        // Second stamp (e.g. the codex path-based branch) must not double up.
        stamp_codex_client_header(&mut buf);
        assert_eq!(
            buf.windows(b"X-Client".len())
                .filter(|w| *w == b"X-Client")
                .count(),
            1
        );
    }

    #[test]
    fn intercept_request_counts_exposes_all_agent_keys() {
        let counts = intercept_request_counts();
        for key in ["claude-code", "codex", "opencode", "grok-build"] {
            assert!(counts.contains_key(key), "missing agent key {key}");
        }
    }

    #[test]
    fn stamp_request_header_grok_base_url_and_client() {
        let mut buf =
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:6767\r\nUser-Agent: grok-shell/0.2.112 (macos; aarch64)\r\n\r\n{}"
                .to_vec();
        stamp_request_header(
            &mut buf,
            "x-headroom-base-url",
            b"x-headroom-base-url: https://api.x.ai\r\n",
        );
        stamp_client_header(&mut buf, b"X-Client: grok_build\r\n");
        assert!(request_has_header(&buf, "x-headroom-base-url"));
        assert!(request_has_header(&buf, "x-client"));
        assert!(buf.ends_with(b"\r\n\r\n{}"), "body preserved");
        // Re-stamping must not duplicate either header.
        stamp_request_header(
            &mut buf,
            "x-headroom-base-url",
            b"x-headroom-base-url: https://api.x.ai\r\n",
        );
        stamp_codex_client_header(&mut buf);
        assert_eq!(
            buf.windows(b"x-headroom-base-url".len())
                .filter(|w| *w == b"x-headroom-base-url")
                .count(),
            1
        );
        assert_eq!(
            buf.windows(b"X-Client".len())
                .filter(|w| *w == b"X-Client")
                .count(),
            1
        );
    }

    #[test]
    fn stamp_codex_client_header_preserves_body_bytes() {
        // The proxy only buffers the head, but a request may arrive with the
        // body already appended; the insertion must not corrupt it.
        let mut buf = b"POST /v1/responses HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        stamp_codex_client_header(&mut buf);
        assert!(buf.ends_with(b"\r\n\r\nhello"));
    }

    #[test]
    fn stamp_codex_client_header_respects_explicit_client() {
        let original = b"POST /v1/responses HTTP/1.1\r\nX-Client: aider\r\n\r\n".to_vec();
        let mut buf = original.clone();
        stamp_codex_client_header(&mut buf);
        assert_eq!(buf, original, "an explicit X-Client must be left untouched");
    }

    #[test]
    fn stamp_codex_client_header_noop_without_terminator() {
        let mut buf = b"POST /v1/responses HTTP/1.1\r\nHost: x".to_vec();
        let original = buf.clone();
        stamp_codex_client_header(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn stamp_headroom_bypass_header_inserts_once_and_preserves_body() {
        let mut buf = b"POST /v1/responses HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        stamp_headroom_bypass_header(&mut buf);
        stamp_headroom_bypass_header(&mut buf);

        let parsed = parse_request_head(&buf).expect("still a valid request head");
        assert_eq!(
            parsed
                .headers
                .iter()
                .filter(|(k, v)| {
                    k.eq_ignore_ascii_case("x-headroom-bypass") && v.eq_ignore_ascii_case("true")
                })
                .count(),
            1
        );
        assert!(buf.ends_with(b"\r\n\r\nhello"));
    }

    #[test]
    fn codex_terminal_reader_detects_split_terminal_events() {
        let mut reader = CodexTerminalReader::new(tokio::io::empty());
        reader.observe(b"data: {\"type\":\"response.com");
        assert!(!reader.saw_terminal());
        reader.observe(b"pleted\"}\n\n");
        assert!(reader.saw_terminal());

        let mut missing = CodexTerminalReader::new(tokio::io::empty());
        missing.observe(b"data: {\"type\":\"response.output_text.delta\"}\n\n");
        assert!(!missing.saw_terminal());

        // A mid-stream `event: error` frame is a terminal signal (RUST-5N).
        let mut errored = CodexTerminalReader::new(tokio::io::empty());
        errored.observe(b"data: {\"type\":\"response.output_text.delta\"}\n\n");
        assert!(!errored.saw_terminal());
        errored.observe(b"event: error\ndata: {\"type\":\"error\"}\n\n");
        assert!(errored.saw_terminal());
    }

    #[test]
    fn upstream_error_reports_are_throttled_per_client_and_status() {
        // Unique keys: the table is process-global and other tests share it.
        assert!(should_report_upstream_error("throttle-test", 498));
        assert!(!should_report_upstream_error("throttle-test", 498));
        // A different status on the same client is its own class.
        assert!(should_report_upstream_error("throttle-test", 497));
        // Same status on another client too: the fingerprint keys on both.
        assert!(should_report_upstream_error("throttle-test-2", 498));
    }

    #[test]
    fn client_probe_paths_are_excluded_from_error_capture_but_not_local() {
        for probe in ["/", "/api/hello", "/v1/settings", "/mcp"] {
            assert!(is_client_probe_path(probe), "{probe}");
            assert!(
                !is_local_proxy_path(probe),
                "{probe} must still forward in bypass"
            );
        }
        for real in [
            "/v1/messages",
            "/v1/messages?beta=true",
            "/v1/responses",
            "/api/hello/x",
        ] {
            assert!(!is_client_probe_path(real), "{real}");
        }
    }

    #[test]
    fn reconnect_reports_are_throttled_per_cause() {
        let slot = AtomicU64::new(0);
        assert!(should_report_throttled(&slot));
        assert!(!should_report_throttled(&slot));
    }

    #[test]
    fn codex_sse_monitor_only_tracks_uncompressed_responses_streams() {
        let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
        assert!(is_codex_sse_response(sse, "/v1/responses"));
        assert!(!is_codex_sse_response(sse, "/v1/models"));
        assert!(!is_codex_sse_response(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/event-stream\r\n\r\n",
            "/v1/responses"
        ));
        assert!(!is_codex_sse_response(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: gzip\r\n\r\n",
            "/v1/responses"
        ));
    }

    #[test]
    fn parse_response_status_reads_status_line() {
        assert_eq!(
            parse_response_status(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n"),
            Some(400)
        );
        assert_eq!(parse_response_status(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(parse_response_status(b"garbage"), None);
    }

    #[test]
    fn is_reportable_upstream_error_excludes_2xx_402_and_429() {
        assert!(is_reportable_upstream_error(&400));
        assert!(is_reportable_upstream_error(&500));
        assert!(!is_reportable_upstream_error(&200));
        assert!(!is_reportable_upstream_error(&429));
        // RUST-CP: 402 is the user's provider billing state, not our request.
        assert!(!is_reportable_upstream_error(&402));
        assert!(is_reportable_upstream_error(&403));
        // 401 gets the body peek; report_upstream_error drops the
        // invalid-key kind and keeps only the missing-auth-header kind.
        assert!(is_reportable_upstream_error(&401));
    }

    #[test]
    fn is_missing_auth_error_splits_setup_bugs_from_bad_keys() {
        // The 2026-09-03 Windows loop: client sent NO auth header at all.
        assert!(is_missing_auth_error(
            br#"{"error":{"message":"Missing bearer or basic authentication in header","type":"invalid_request_error","param":null,"code":null}}"#
        ));
        // Anthropic's no-credentials variant (Claude client sent no header).
        assert!(is_missing_auth_error(
            br#"{"type":"error","error":{"type":"authentication_error","message":"Could not resolve authentication method. Expected either x-api-key or authorization header to be provided."}}"#
        ));
        // Invalid/expired key is the user's problem, not reportable (RUST-46).
        assert!(!is_missing_auth_error(
            br#"{"error":{"message":"Incorrect API key provided: sk-abc...","type":"invalid_request_error","code":"invalid_api_key"}}"#
        ));
        assert!(!is_missing_auth_error(
            br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
        ));
        assert!(!is_missing_auth_error(b"<html>401</html>"));
        assert!(!is_missing_auth_error(b""));
    }

    #[test]
    fn response_sniffer_captures_reportable_errors_only() {
        // 200: nothing buffered, nothing to report on drop.
        let mut ok = ResponseSniffer::new((), "claude-code", Some("/v1/messages".into()));
        ok.observe(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: x");
        assert!(ok.done && ok.buf.is_empty() && ok.status == Some(200));

        // 400 on a client path: bounded capture retained for the Drop report.
        let mut err = ResponseSniffer::new((), "claude-code", Some("/v1/messages".into()));
        err.observe(b"HTTP/1.1 400 Bad Request\r\n");
        err.observe(b"Content-Type: application/json\r\n\r\n{\"error\":{}}");
        assert_eq!(err.status, Some(400));
        assert!(!err.done, "keeps observing until the cap");
        assert!(err.buf.ends_with(b"{\"error\":{}}"), "body slice captured");
        // Neutralize the Drop report: unit tests must not emit Sentry events.
        err.capture_path = None;

        // Local proxy path (capture disabled): 404 is the RUST-87 squatter
        // case, not a client error.
        let mut local = ResponseSniffer::new((), "claude-code", None);
        local.observe(b"HTTP/1.1 404 Not Found\r\n\r\n");
        assert!(local.done && local.buf.is_empty());

        // 429 is counted, never captured.
        let mut limited = ResponseSniffer::new((), "claude-code", Some("/v1/messages".into()));
        limited.observe(b"HTTP/1.1 429 Too Many Requests\r\n\r\n");
        assert!(limited.done && limited.buf.is_empty());

        // Non-HTTP stream: stops looking at the cap.
        let mut raw = ResponseSniffer::new((), "claude-code", Some("/v1/messages".into()));
        raw.observe(&[0u8; 128]);
        assert!(raw.done && raw.status.is_none() && raw.buf.is_empty());
    }

    #[test]
    fn strip_request_header_removes_lite_header_and_preserves_body() {
        let mut buf = b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:6767\r\nX-OpenAI-Internal-Codex-Responses-Lite: 1\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        strip_request_header(&mut buf, "X-OpenAI-Internal-Codex-Responses-Lite");
        assert!(!request_has_header(
            &buf,
            "X-OpenAI-Internal-Codex-Responses-Lite"
        ));
        // Surrounding headers, terminator and body intact.
        assert!(request_has_header(&buf, "host"));
        assert!(request_has_header(&buf, "content-length"));
        assert!(buf.ends_with(b"\r\n\r\nhello"));
        assert_eq!(buf.windows(4).filter(|w| *w == b"\r\n\r\n").count(), 1);
    }

    #[test]
    fn strip_request_header_noop_when_absent() {
        let mut buf = b"POST /v1/responses HTTP/1.1\r\nHost: x\r\n\r\n".to_vec();
        let original = buf.clone();
        strip_request_header(&mut buf, "X-OpenAI-Internal-Codex-Responses-Lite");
        assert_eq!(buf, original);
    }

    #[test]
    fn force_connection_close_replaces_keep_alive_and_preserves_body() {
        let mut buf =
            b"POST /v1/messages HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n{\"a\":1}"
                .to_vec();
        super::force_connection_close(&mut buf);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Connection: close\r\n"));
        assert!(!text.contains("keep-alive"));
        assert!(
            text.ends_with("\r\n\r\n{\"a\":1}"),
            "body preserved: {text}"
        );
        assert_eq!(text.matches("Connection:").count(), 1);
    }

    #[test]
    fn force_connection_close_inserts_when_no_connection_header() {
        let mut buf = b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n".to_vec();
        super::force_connection_close(&mut buf);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Connection: close\r\n"));
        // Still exactly one header terminator, at the end.
        assert!(text.ends_with("\r\n\r\n"));
        assert_eq!(text.matches("\r\n\r\n").count(), 1);
    }

    #[test]
    fn force_connection_close_noop_without_terminator() {
        let mut buf = b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec();
        let original = buf.clone();
        super::force_connection_close(&mut buf);
        assert_eq!(buf, original);
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
        for name in [
            "Authorization",
            "anthropic-version",
            "x-api-key",
            "Content-Type",
        ] {
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
    #[serial]
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
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let bypass: BypassFlag = Arc::new(AtomicBool::new(true));
        // Bypass means we never actually contact the backend; pin to an
        // unused loopback port so any accidental connect would fail fast.
        backend_port::set(1);
        let upstream_base_arc = Arc::new(upstream_base);
        let token_for_run = token_slot.clone();
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                token_for_run,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base_arc,
                Arc::new(Mutex::new(None)),
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
        backend_port::reset_for_tests();
    }

    #[tokio::test]
    #[serial]
    async fn bypass_returns_502_when_upstream_unreachable() {
        // Bind+drop to grab a free port nothing is listening on.
        let (probe, dead_addr) = bind_ephemeral().await;
        drop(probe);
        let upstream_base = format!("http://127.0.0.1:{}", dead_addr.port());

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let bypass: BypassFlag = Arc::new(AtomicBool::new(true));
        backend_port::set(1);
        let upstream_base_arc = Arc::new(upstream_base);
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                token_slot,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base_arc,
                Arc::new(Mutex::new(None)),
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
        backend_port::reset_for_tests();
    }

    /// New: the intercept must read the backend port per connection so that
    /// when `tool_manager` selects a fallback port mid-launch, in-flight
    /// clients get routed to the new backend without a thread restart.
    #[tokio::test]
    #[serial]
    async fn intercept_picks_up_backend_port_changes_between_connections() {
        let (first_listener, first_addr) = bind_ephemeral().await;
        let (second_listener, second_addr) = bind_ephemeral().await;

        let first_task = tokio::spawn(async move {
            let (mut sock, _) = first_listener.accept().await.expect("first accept");
            let _ = read_until_header_end(&mut sock).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            "first"
        });
        let second_task = tokio::spawn(async move {
            let (mut sock, _) = second_listener.accept().await.expect("second accept");
            let _ = read_until_header_end(&mut sock).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            "second"
        });

        backend_port::set(first_addr.port());

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        // Unroutable on purpose. These tests assert on a local fake backend and
        // must never reach the real API: when a stale backend_port sent them down
        // the direct-to-provider fallback, they silently made live calls to
        // api.anthropic.com and failed on the resulting 401 body instead of saying
        // so. Port 1 on loopback refuses instantly, so a stray fallback is a fast,
        // legible failure rather than a network round trip.
        let upstream_base = Arc::new("http://127.0.0.1:1".to_string());
        let token_for_run = token_slot.clone();
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                token_for_run,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass_for_run,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base,
                Arc::new(Mutex::new(None)),
            )
            .await;
        });

        // Wait for the intercept to bind, then send the first request.
        let mut first_client = None;
        for _ in 0..50 {
            if let Ok(c) = TcpStream::connect(intercept_addr).await {
                first_client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut first_client = first_client.expect("intercept reachable");
        let req = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\n\r\n",
            intercept_addr.port()
        );
        first_client
            .write_all(req.as_bytes())
            .await
            .expect("write first req");

        let routed_first = timeout(Duration::from_secs(2), first_task)
            .await
            .expect("first backend received request")
            .expect("first task ok");
        assert_eq!(routed_first, "first");

        // Switch the global to the second backend; next connection routes there.
        backend_port::set(second_addr.port());

        let mut second_client = TcpStream::connect(intercept_addr)
            .await
            .expect("connect second");
        second_client
            .write_all(req.as_bytes())
            .await
            .expect("write second req");

        let routed_second = timeout(Duration::from_secs(2), second_task)
            .await
            .expect("second backend received request")
            .expect("second task ok");
        assert_eq!(routed_second, "second");

        run_task.abort();
        backend_port::reset_for_tests();
    }

    // ── codex rate-limit header parsing ─────────────────────────────────────

    #[test]
    fn parse_codex_headers_decodes_primary_secondary_credits() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let head = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             x-codex-limit-name: gpt-5.2-codex\r\n\
             x-codex-primary-used-percent: 42.5\r\n\
             x-codex-primary-window-minutes: 300\r\n\
             x-codex-primary-reset-at: {}\r\n\
             x-codex-secondary-used-percent: 12\r\n\
             x-codex-secondary-window-minutes: 10080\r\n\
             x-codex-secondary-reset-at: {}\r\n\
             x-codex-credits-balance: $5.00\r\n\
             x-codex-credits-unlimited: false\r\n\
             \r\n",
            now + 7200,
            now + 86400,
        );
        let snap = parse_codex_rate_limit_headers(head.as_bytes()).expect("snapshot");
        assert_eq!(snap.limit_name.as_deref(), Some("gpt-5.2-codex"));
        let primary = snap.primary.expect("primary");
        assert_eq!(primary.used_percent, 42.5);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.window_label.as_deref(), Some("5h"));
        // Reset is ~7200s out; allow a couple seconds of clock slack.
        let secs = primary.seconds_until_reset.expect("reset");
        assert!((7195..=7200).contains(&secs), "got {secs}");
        let secondary = snap.secondary.expect("secondary");
        assert_eq!(secondary.window_label.as_deref(), Some("168h"));
        assert_eq!(snap.credits_balance.as_deref(), Some("$5.00"));
        assert!(!snap.credits_unlimited);
    }

    #[test]
    fn parse_codex_headers_case_insensitive_and_clamps_past_reset() {
        let head = "HTTP/1.1 429 Too Many Requests\r\n\
             X-Codex-Primary-Used-Percent: 99\r\n\
             X-Codex-Primary-Window-Minutes: 45\r\n\
             X-Codex-Primary-Reset-At: 100\r\n\
             \r\n";
        let snap = parse_codex_rate_limit_headers(head.as_bytes()).expect("snapshot");
        let primary = snap.primary.expect("primary");
        assert_eq!(primary.used_percent, 99.0);
        assert_eq!(primary.window_label.as_deref(), Some("45m"));
        // reset-at is in the distant past -> clamped to 0.
        assert_eq!(primary.seconds_until_reset, Some(0));
    }

    #[test]
    fn parse_codex_headers_absent_returns_none() {
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n";
        assert!(parse_codex_rate_limit_headers(head.as_bytes()).is_none());
    }

    #[test]
    fn parse_codex_headers_partial_head_returns_none() {
        // No header terminator / garbage — must not panic, no signal.
        let head = "HTTP/1.1 200 OK\r\nx-codex-limit-name: codex";
        assert!(parse_codex_rate_limit_headers(head.as_bytes()).is_none());
    }

    // A faithful GET /wham/usage body (shape captured from a live Plus account).
    const USAGE_BODY: &str = r#"{
        "plan_type": "plus",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {"used_percent": 23, "limit_window_seconds": 18000, "reset_at": 1781276043},
            "secondary_window": {"used_percent": 6, "limit_window_seconds": 604800, "reset_at": 1781622947}
        },
        "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
        "rate_limit_reached_type": null,
        "promo": null
    }"#;

    #[test]
    fn usage_payload_maps_to_snapshot() {
        let payload = serde_json::from_str(USAGE_BODY).expect("json");
        let snap = codex_snapshot_from_usage_payload(&payload).expect("snapshot");
        let primary = snap.primary.expect("primary");
        assert_eq!(primary.used_percent, 23.0);
        assert_eq!(primary.window_minutes, Some(300)); // 18000s rounded up
        let secondary = snap.secondary.expect("secondary");
        assert_eq!(secondary.used_percent, 6.0);
        assert_eq!(secondary.window_minutes, Some(10080)); // 604800s
                                                           // has_credits=false -> "0" balance must not surface as noise.
        assert_eq!(snap.credits_balance, None);
        assert!(!snap.credits_unlimited);
    }

    #[test]
    fn usage_window_minutes_rounds_up() {
        let payload = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":61}}}"#,
        )
        .expect("json");
        let snap = codex_snapshot_from_usage_payload(&payload).expect("snapshot");
        assert_eq!(snap.primary.expect("primary").window_minutes, Some(2));
    }

    #[test]
    fn usage_credits_balance_kept_when_has_credits() {
        let payload = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"used_percent":5}},"credits":{"has_credits":true,"unlimited":false,"balance":"$5.00"}}"#,
        )
        .expect("json");
        let snap = codex_snapshot_from_usage_payload(&payload).expect("snapshot");
        assert_eq!(snap.credits_balance.as_deref(), Some("$5.00"));
    }

    #[test]
    fn usage_empty_payload_returns_none() {
        let payload = serde_json::from_str("{}").expect("json");
        assert!(codex_snapshot_from_usage_payload(&payload).is_none());
        let payload = serde_json::from_str(r#"{"rate_limit":{}}"#).expect("json");
        assert!(codex_snapshot_from_usage_payload(&payload).is_none());
    }

    #[test]
    fn usage_window_missing_used_percent_skipped() {
        let payload = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"limit_window_seconds":60}}}"#,
        )
        .expect("json");
        assert!(codex_snapshot_from_usage_payload(&payload).is_none());
    }

    #[test]
    fn extract_header_value_is_case_insensitive() {
        let req = b"GET /v1/responses HTTP/1.1\r\nHost: x\r\nChatGPT-Account-Id: acct_9\r\n\r\n";
        assert_eq!(
            extract_header_value(req, "chatgpt-account-id").as_deref(),
            Some("acct_9")
        );
        assert!(extract_header_value(req, "x-missing").is_none());
    }

    fn jwt_with_plan(plan: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload_json = format!(
            "{{\"https://api.openai.com/auth\":{{\"chatgpt_plan_type\":\"{plan}\",\"chatgpt_account_id\":\"acct_1\"}}}}"
        );
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn decode_codex_plan_tier_reads_chatgpt_plan_type() {
        assert_eq!(
            decode_codex_plan_tier(&jwt_with_plan("plus")),
            Some(CodexPlanTier::Plus)
        );
        assert_eq!(
            decode_codex_plan_tier(&jwt_with_plan("pro")),
            Some(CodexPlanTier::Pro)
        );
        // Unrecognized claim value still decodes, mapped to Unknown.
        assert_eq!(
            decode_codex_plan_tier(&jwt_with_plan("mystery")),
            Some(CodexPlanTier::Unknown)
        );
    }

    #[test]
    fn decode_codex_plan_tier_rejects_malformed_tokens() {
        assert!(decode_codex_plan_tier("not-a-jwt").is_none());
        assert!(decode_codex_plan_tier("only.two").is_none());
        // Valid JWT shape but no auth claim.
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"sub\":\"x\"}");
        assert!(decode_codex_plan_tier(&format!("h.{payload}.s")).is_none());
    }

    #[test]
    fn codex_window_label_formats() {
        assert_eq!(codex_window_label(45), "45m");
        assert_eq!(codex_window_label(300), "5h");
        assert_eq!(codex_window_label(10080), "168h");
        assert_eq!(codex_window_label(90), "1h30m");
    }

    fn expect_rewritten(result: ModelsRewrite) -> (Vec<u8>, usize) {
        match result {
            ModelsRewrite::Rewritten {
                body,
                flags_flipped,
            } => (body, flags_flipped),
            _ => panic!("expected Rewritten"),
        }
    }

    #[test]
    fn rewrite_use_responses_lite_forces_false() {
        let body = br#"{"models":[{"slug":"gpt-5.5","use_responses_lite":true},{"slug":"gpt-5.4","use_responses_lite":false}]}"#;
        let (rewritten, flipped) = expect_rewritten(rewrite_use_responses_lite(body));
        assert_eq!(flipped, 1);
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        for model in value["models"].as_array().unwrap() {
            assert_eq!(model["use_responses_lite"], serde_json::Value::Bool(false));
        }
        // Other fields survive.
        assert_eq!(value["models"][0]["slug"], "gpt-5.5");
    }

    #[test]
    fn rewrite_use_responses_lite_handles_nested_flag() {
        let body = br#"{"data":{"items":[{"info":{"use_responses_lite":true}}]}}"#;
        let (rewritten, flipped) = expect_rewritten(rewrite_use_responses_lite(body));
        assert_eq!(flipped, 1);
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(
            value["data"]["items"][0]["info"]["use_responses_lite"],
            serde_json::Value::Bool(false)
        );
    }

    #[test]
    fn rewrite_use_responses_lite_noop_when_nothing_to_change() {
        // All-false catalog: no rewrite, response stays byte-identical.
        assert!(matches!(
            rewrite_use_responses_lite(
                br#"{"models":[{"slug":"gpt-5.5","use_responses_lite":false}]}"#
            ),
            ModelsRewrite::Unchanged
        ));
        // Non-boolean value is left alone.
        assert!(matches!(
            rewrite_use_responses_lite(br#"{"use_responses_lite":"true"}"#),
            ModelsRewrite::Unchanged
        ));
        // Non-JSON body: fail-open, reported as unparseable.
        assert!(matches!(
            rewrite_use_responses_lite(b"<html>challenge</html>"),
            ModelsRewrite::Unparseable
        ));
    }

    #[tokio::test]
    #[serial]
    async fn intercept_rewrites_use_responses_lite_in_models_response() {
        let models_json = br#"{"models":[{"slug":"gpt-5.5","use_responses_lite":true}]}"#.to_vec();
        let (backend_listener, backend_addr) = bind_ephemeral().await;
        let backend_task = tokio::spawn(async move {
            let (mut sock, _) = backend_listener.accept().await.expect("backend accept");
            let _ = read_until_header_end(&mut sock).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                models_json.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(&models_json).await;
            // Keep the connection open briefly so the splice can finish.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        backend_port::set(backend_addr.port());

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let slot_for_run = token_slot.clone();
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        // Unroutable on purpose. These tests assert on a local fake backend and
        // must never reach the real API: when a stale backend_port sent them down
        // the direct-to-provider fallback, they silently made live calls to
        // api.anthropic.com and failed on the resulting 401 body instead of saying
        // so. Port 1 on loopback refuses instantly, so a stray fallback is a fast,
        // legible failure rather than a network round trip.
        let upstream_base = Arc::new("http://127.0.0.1:1".to_string());
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                slot_for_run,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass_for_run,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base,
                Arc::new(Mutex::new(None)),
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
            "GET /v1/models?client_version=1.0.0 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer test-token-123\r\n\r\n",
            intercept_addr.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");

        // Read head + body of the (rewritten) response.
        let mut response = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let mut tmp = [0u8; 4096];
            let n = match tokio::time::timeout_at(deadline, client.read(&mut tmp)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break,
            };
            response.extend_from_slice(&tmp[..n]);
            if let Some(end) = find_header_end(&response) {
                let head = std::str::from_utf8(&response[..end + 4]).expect("utf8 head");
                let content_length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .expect("content-length present")
                    .trim()
                    .parse()
                    .expect("numeric content-length");
                if response.len() >= end + 4 + content_length {
                    break;
                }
            }
        }

        let end = find_header_end(&response).expect("response head complete");
        let body: serde_json::Value =
            serde_json::from_slice(&response[end + 4..]).expect("json body");
        assert_eq!(
            body["models"][0]["use_responses_lite"],
            serde_json::Value::Bool(false),
            "lite flag rewritten to false: {body}"
        );
        assert_eq!(body["models"][0]["slug"], "gpt-5.5");

        run_task.abort();
        backend_task.abort();
        backend_port::reset_for_tests();
    }

    #[tokio::test]
    #[serial]
    async fn intercept_skips_models_rewrite_for_anthropic_fetch() {
        // Same catalog shape, but the request carries Anthropic markers —
        // the Codex-only lite-flag rewrite must leave it untouched.
        let models_json = br#"{"models":[{"slug":"gpt-5.5","use_responses_lite":true}]}"#.to_vec();
        let (backend_listener, backend_addr) = bind_ephemeral().await;
        let backend_task = tokio::spawn(async move {
            let (mut sock, _) = backend_listener.accept().await.expect("backend accept");
            let _ = read_until_header_end(&mut sock).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                models_json.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(&models_json).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        backend_port::set(backend_addr.port());

        let token_slot: SharedToken = Arc::new(Mutex::new(None));
        let intercept_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("intercept bind");
        let intercept_addr = intercept_listener.local_addr().expect("intercept addr");
        drop(intercept_listener);
        let slot_for_run = token_slot.clone();
        let bypass_for_run: BypassFlag = Arc::new(AtomicBool::new(false));
        // Unroutable on purpose. These tests assert on a local fake backend and
        // must never reach the real API: when a stale backend_port sent them down
        // the direct-to-provider fallback, they silently made live calls to
        // api.anthropic.com and failed on the resulting 401 body instead of saying
        // so. Port 1 on loopback refuses instantly, so a stray fallback is a fast,
        // legible failure rather than a network round trip.
        let upstream_base = Arc::new("http://127.0.0.1:1".to_string());
        let (fresh_bearer_tx, _fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
        let run_task = tokio::spawn(async move {
            let _ = run(
                intercept_addr,
                false,
                slot_for_run,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                bypass_for_run,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                fresh_bearer_tx,
                upstream_base,
                Arc::new(Mutex::new(None)),
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
            "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nanthropic-version: 2023-06-01\r\n\r\n",
            intercept_addr.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");

        let mut response = Vec::new();
        let mut tmp = [0u8; 4096];
        let read_completed = timeout(Duration::from_secs(2), async {
            loop {
                match client.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        response.extend_from_slice(&tmp[..n]);
                        if let Some(end) = find_header_end(&response) {
                            if serde_json::from_slice::<serde_json::Value>(&response[end + 4..])
                                .is_ok()
                            {
                                break;
                            }
                        }
                    }
                }
            }
        })
        .await;

        let end = find_header_end(&response).expect("response head complete");
        let raw = &response[end + 4..];
        // Self-diagnosing on failure: this test is flaky under parallel load
        // and "json body: trailing characters" alone cannot distinguish a
        // read that ran out of time from a body whose framing is wrong.
        // Truncation would report "EOF while parsing"; "trailing characters"
        // means the slice did not start where we think it did.
        let body: serde_json::Value = serde_json::from_slice(raw).unwrap_or_else(|e| {
            panic!(
                "json body: {e}\n  read loop finished within budget: {}\n  {} body bytes, \
                 first 80: {:?}",
                read_completed.is_ok(),
                raw.len(),
                String::from_utf8_lossy(&raw[..raw.len().min(80)])
            )
        });
        assert_eq!(
            body["models"][0]["use_responses_lite"],
            serde_json::Value::Bool(true),
            "anthropic-marked models fetch must pass through unrewritten: {body}"
        );

        run_task.abort();
        backend_task.abort();
        backend_port::reset_for_tests();
    }

    #[test]
    fn codex_error_summary_extracts_structural_fields_only() {
        let body = br#"{"error":{"message":"Invalid prompt: SECRET user content here","type":"invalid_request_error","param":"messages","code":"invalid_prompt"}}"#;
        let summary = codex_error_summary(body);
        assert_eq!(
            summary,
            "type=invalid_request_error code=invalid_prompt param=messages"
        );
        assert!(
            !summary.contains("SECRET"),
            "free-text message must never reach Sentry: {summary}"
        );
    }

    #[test]
    fn codex_error_summary_handles_non_json() {
        assert_eq!(
            codex_error_summary(b"<html>gateway error</html>"),
            "unparseable error body (26 bytes)"
        );
    }

    #[test]
    fn codex_error_summary_describes_shape_when_fields_are_absent() {
        // The RUST-4V case: valid JSON, none of type/code/param, which used to
        // render as the useless "type=- code=- param=-".
        let summary = codex_error_summary(br#"{"detail":"nope","status":400}"#);
        assert_eq!(
            summary,
            "no structural error fields; shape=object{detail,status} (30 bytes)"
        );
        // `{"error": "..."}` — error present but a string, so no fields resolve.
        assert_eq!(
            codex_error_summary(br#"{"error":"boom"}"#),
            "no structural error fields; shape=object{error} (16 bytes)"
        );
        // An empty object and a bare null are both distinguishable now.
        assert_eq!(
            codex_error_summary(b"{}"),
            "no structural error fields; shape=object{} (2 bytes)"
        );
        assert_eq!(
            codex_error_summary(b"null"),
            "no structural error fields; shape=null (4 bytes)"
        );
    }

    #[test]
    fn codex_error_summary_still_prefers_structural_fields() {
        // A partially-populated body must keep the field rendering, not fall
        // through to the shape branch.
        assert_eq!(
            codex_error_summary(br#"{"error":{"code":"invalid_prompt"}}"#),
            "type=- code=invalid_prompt param=-"
        );
    }

    #[test]
    fn codex_error_shape_tag_stays_low_cardinality_and_content_free() {
        assert_eq!(
            codex_error_shape_tag(br#"{"error":{"code":"x"}}"#),
            "object{error}"
        );
        assert_eq!(codex_error_shape_tag(b"<html>502</html>"), "non-json");
        assert_eq!(codex_error_shape_tag(b""), "empty");
        assert_eq!(codex_error_shape_tag(b"[]"), "array");

        // Keys are sorted (stable tag value) and capped at SHAPE_MAX_KEYS.
        let many = (0..20)
            .map(|i| format!("\"k{i:02}\":1"))
            .collect::<Vec<_>>()
            .join(",");
        let tag = codex_error_shape_tag(format!("{{{many}}}").as_bytes());
        assert_eq!(tag, "object{k00,k01,k02,k03,k04,k05,k06,k07}");

        // A key shaped like user content is dropped rather than forwarded.
        let leaky = br#"{"Summarise this: my password is hunter2":1,"detail":2}"#;
        let tag = codex_error_shape_tag(leaky);
        assert_eq!(tag, "object{detail}");
        assert!(!tag.contains("hunter2"), "{tag}");
    }

    #[test]
    fn anthropic_error_shape_classifies_tool_search_400s_content_free() {
        // Streams as SSE, so it must match on the raw framed bytes.
        let sse = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Tool reference 'mcp__headroom__headroom_compress' not found in available tools\"}}\n\n";
        assert_eq!(
            super::anthropic_error_shape(sse),
            Some("tool_reference_not_found")
        );
        // The classification never carries the offending tool name.
        assert!(!super::anthropic_error_shape(sse)
            .unwrap()
            .contains("headroom"));

        let deferred = br#"{"error":{"message":"At least one tool must have defer_loading=false. All tools cannot be deferred."}}"#;
        assert_eq!(
            super::anthropic_error_shape(deferred),
            Some("all_tools_deferred")
        );

        // Unrelated bodies fall through to the codex classifier.
        assert_eq!(
            super::anthropic_error_shape(br#"{"error":"overloaded"}"#),
            None
        );
        assert_eq!(super::anthropic_error_shape(b""), None);
    }

    #[test]
    fn response_content_type_strips_parameters() {
        let head =
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json; charset=utf-8\r\n\r\n";
        assert_eq!(
            response_content_type(head).as_deref(),
            Some("application/json")
        );
        let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
        assert_eq!(
            response_content_type(sse).as_deref(),
            Some("text/event-stream")
        );
        assert_eq!(response_content_type(b"HTTP/1.1 400 Bad\r\n\r\n"), None);
    }

    #[test]
    fn geo_blocked_codex_error_detects_unsupported_region() {
        // The exact shape RUST-4H captured 139 times.
        let body = br#"{"error":{"message":"Country, region, or territory not supported","type":"request_forbidden","code":"unsupported_country_region_territory","param":null}}"#;
        assert!(is_geo_blocked_codex_error(body));
    }

    #[test]
    fn geo_blocked_codex_error_ignores_other_403s() {
        // An org-verification 403 IS actionable and must still reach Sentry.
        let body =
            br#"{"error":{"type":"request_forbidden","code":"organization_must_be_verified"}}"#;
        assert!(!is_geo_blocked_codex_error(body));
        // Unrelated shapes must not be mistaken for a geo-block.
        assert!(!is_geo_blocked_codex_error(
            br#"{"error":{"type":"invalid_request_error","code":"invalid_prompt"}}"#
        ));
        assert!(!is_geo_blocked_codex_error(b"<html>gateway error</html>"));
        assert!(!is_geo_blocked_codex_error(b""));
        // A null code must not panic or match.
        assert!(!is_geo_blocked_codex_error(br#"{"error":{"code":null}}"#));
    }

    #[test]
    fn set_response_content_length_replaces_existing() {
        let mut head =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n"
                .to_vec();
        set_response_content_length(&mut head, 12345);
        let text = String::from_utf8(head).unwrap();
        assert!(text.contains("Content-Length: 12345\r\n"));
        assert!(!text.contains("Content-Length: 10\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
    }

    #[tokio::test]
    async fn inflight_semaphore_fails_fast_when_exhausted() {
        // A saturated pool must reject via try_acquire_owned so `handle` takes
        // the 503 branch instead of connecting and holding another FD pair.
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let held = sem.clone().try_acquire_owned().expect("first permit");
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "should be saturated"
        );
        drop(held);
        assert!(sem.try_acquire_owned().is_ok(), "permit released on drop");
    }

    // Exact GA wire shape captured from a live bricked session: content is a
    // dict holding tool_references.
    fn tool_search_body(ref_names: &[&str], tool_names: &[&str]) -> Vec<u8> {
        let refs: Vec<serde_json::Value> = ref_names
            .iter()
            .map(|n| serde_json::json!({"type": "tool_reference", "tool_name": n}))
            .collect();
        let tools: Vec<serde_json::Value> = tool_names
            .iter()
            .map(|n| serde_json::json!({"name": n, "input_schema": {"type": "object"}}))
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64,
            "tools": tools,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "searching"},
                    {"type": "tool_search_tool_result",
                     "tool_use_id": "srvtoolu_x",
                     "content": {"type": "tool_search_tool_search_result",
                                 "tool_references": refs}}
                ]}
            ]
        }))
        .unwrap()
    }

    fn reference_names(body: &[u8]) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        v["messages"][1]["content"][1]["content"]["tool_references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tool_name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn sanitize_drops_only_the_stale_reference() {
        let body = tool_search_body(
            &["headroom_retrieve", "mcp__headroom__headroom_retrieve"],
            &["mcp__headroom__headroom_retrieve"],
        );
        let out = sanitize_stale_tool_references(body, "/v1/messages?beta=true");
        assert_eq!(
            reference_names(&out),
            vec!["mcp__headroom__headroom_retrieve"]
        );
    }

    #[test]
    fn sanitize_noop_returns_identical_bytes() {
        let body = tool_search_body(&["kept"], &["kept"]);
        let out = sanitize_stale_tool_references(body.clone(), "/v1/messages");
        assert_eq!(out, body, "resolvable references must be byte-untouched");
    }

    #[test]
    fn sanitize_all_stale_leaves_empty_reference_list() {
        // An empty tool_references list is accepted upstream (observed in a
        // live transcript), so the block itself must survive.
        let body = tool_search_body(&["gone_a", "gone_b"], &["other"]);
        let out = sanitize_stale_tool_references(body, "/v1/messages");
        assert!(reference_names(&out).is_empty());
    }

    #[test]
    fn sanitize_skips_non_messages_paths_and_bad_json() {
        let body = tool_search_body(&["gone"], &[]);
        let out = sanitize_stale_tool_references(body.clone(), "/v1/complete");
        assert_eq!(out, body, "non-messages path must be untouched");

        let junk = b"tool_search_tool_result but not json".to_vec();
        let out = sanitize_stale_tool_references(junk.clone(), "/v1/messages");
        assert_eq!(out, junk, "unparseable body must fail open");
    }

    #[test]
    fn sanitize_handles_list_shaped_content() {
        let body = serde_json::to_vec(&serde_json::json!({
            "tools": [{"name": "kept"}],
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_search_tool_result", "content": [
                    {"type": "tool_reference", "tool_name": "gone"},
                    {"type": "tool_reference", "tool_name": "kept"}
                ]}
            ]}]
        }))
        .unwrap();
        let out = sanitize_stale_tool_references(body, "/v1/messages");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let refs = v["messages"][0]["content"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["tool_name"], "kept");
    }
}
