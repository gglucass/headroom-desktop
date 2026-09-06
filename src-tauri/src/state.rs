use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::activity_facts::{ActivityFacts, WeeklyTotals};
use crate::analytics;
use crate::bearer::{BearerToken, BEARER_TOKEN_TTL};
use crate::client_adapters::{
    detect_clients, ensure_rtk_integrations, is_rtk_disabled, rtk_integration_status,
};
use crate::insights::generate_daily_insights;
use crate::models::{
    ActivityEvent, BootstrapFailureReport, BootstrapProgress, ClaudeAccountProfile,
    ClaudeCodeProject, ClientStatus, CodexAccountProfile, CodexRateLimitSnapshot, DailyInsight,
    DailySavingsPoint, DashboardState, HeadroomLearnPrereqStatus, HeadroomLearnStatus,
    HourlySavingsPoint, LaunchExperience, RtkRuntimeStatus, RuntimeStatus, RuntimeUpgradeFailure,
    RuntimeUpgradeProgress, TransformationFeedEvent, UpgradeFailurePhase, UsageEvent,
};
use crate::pricing;
use crate::storage::{app_data_dir, config_file, ensure_data_dirs, telemetry_file};
use crate::tool_manager::{
    BootstrapStepUpdate, HeadroomRelease, ManagedRuntime, RtkGainSummary, RuntimeMaintenanceKind,
    ToolManager,
};

/// After this many consecutive failed auto-attempts at the same app version,
/// we stop auto-retrying and surface a persistent banner with a Retry button.
pub const MAX_UPGRADE_AUTO_RETRIES: u32 = 2;

/// Current Terms-of-Service version the user must have accepted to use the app.
/// BUMP THIS whenever the terms on extraheadroom.com/terms change: a release
/// shipping a higher value forces every user to re-accept on first launch,
/// because their locally-stored `accepted_terms_version` will be lower.
pub const REQUIRED_TERMS_VERSION: u32 = 1;

/// Canonical Terms-of-Service URL opened from the acceptance gate.
pub const TERMS_URL: &str = "https://extraheadroom.com/terms";

/// Absolute maximum time we'll wait for the new proxy to come up during
/// boot validation, regardless of observed activity. Bounded so an
/// indefinitely-hung process is still detected eventually. Adaptive stall
/// detection (below) normally fires long before this.
pub const RUNTIME_UPGRADE_BOOT_MAX_SECS: u64 = 600;

/// Hard ceiling that overrides the soft cap above *only* while the HF model
/// cache is still actively growing — i.e. a first-run model download is in
/// flight. A slow connection legitimately needs longer than 600s to pull the
/// multi-GB ONNX weights; failing the upgrade at the soft cap rolled back a
/// download that was still progressing and spammed an Error-level Sentry event
/// (RUST-4A). The stall guard (silence window below) still catches a genuinely
/// idle process long before this. `TimedOut` past this ceiling means the
/// download itself is pathologically slow, not that the proxy hung.
pub const RUNTIME_UPGRADE_BOOT_HARD_MAX_SECS: u64 = 1800;

/// Once this much wall-time has elapsed without /livez success, start
/// checking the proxy log's mtime (and the HF cache size) for progress.
/// Before this, we stay quiet — most fast boots finish well under this
/// threshold.
pub const RUNTIME_UPGRADE_STALL_GRACE_SECS: u64 = 60;

/// If neither the proxy log nor the HF cache has grown in this long
/// (AND we're past the grace period), the proxy is considered stalled
/// and we roll back. Bumped from 45s → 90s after a real first-run upgrade
/// failed: the python process printed its banner, then went silent for
/// ~50s while loading multi-GB ONNX models from the freshly-downloaded
/// HF cache. The log was idle but the proxy was making progress.
pub const RUNTIME_UPGRADE_STALL_SILENCE_SECS: u64 = 90;

enum RuntimeMaintenancePlan {
    Upgrade(HeadroomRelease),
    RequirementsRepair,
}

#[derive(Debug, Default, Clone)]
pub struct PendingMilestones {
    pub token: Vec<u64>,
    /// Current lifetime token total to POST as a cumulative-savings heartbeat,
    /// set when the throttle window has elapsed and the total has grown. `None`
    /// most ticks; the server records it as `max`, so it's idempotent.
    pub cumulative_report: Option<u64>,
}

/// How often the desktop posts its lifetime token total to keep the server's
/// `cumulative_tokens_saved` / `last_active_at` fresh between 1M milestones.
/// ponytail: in-memory throttle (resets on restart → one harmless post on
/// launch); shorten if admin freshness needs to be tighter.
const CUMULATIVE_REPORT_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Debug, Default, Clone)]
pub struct ActivityObservation {
    #[allow(dead_code)] // read by tests; production callers discard observations
    pub fresh: Vec<ActivityEvent>,
}

/// Emit the runtime upgrade progress event on the given AppHandle.
pub fn emit_runtime_upgrade_progress(app: &tauri::AppHandle, state: &AppState) {
    use tauri::Emitter;
    let _ = app.emit("runtime_upgrade_progress", state.runtime_upgrade_progress());
}

/// Escape hatch: set `HEADROOM_SKIP_RUNTIME_UPGRADE=1` to boot past a
/// persistently-failing upgrade without editing disk state.
pub fn runtime_upgrade_disabled_by_env() -> bool {
    matches!(
        std::env::var("HEADROOM_SKIP_RUNTIME_UPGRADE")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// One-shot probe of the new proxy. Hits `/livez` on the backend port
/// directly first (bypasses the intercept layer on 6767). Falls back to
/// `/health` for older headroom-ai versions that don't expose `/livez`, then
/// through the intercept layer on 6767 as a last resort — which also succeeds
/// if the proxy is alive but too CPU-saturated to answer a direct probe
/// quickly, since the intercept has its own retry + longer timeout path.
fn probe_proxy_livez(client: &reqwest::blocking::Client) -> bool {
    let backend = crate::backend_port::get();
    let urls = [
        format!("http://127.0.0.1:{backend}/livez"),
        format!("http://127.0.0.1:{backend}/health"),
        "http://127.0.0.1:6767/livez".to_string(),
        "http://127.0.0.1:6767/health".to_string(),
    ];
    for url in &urls {
        if client
            .get(url)
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// HuggingFace hub cache path — where transformers/huggingface_hub write
/// downloaded model weights. Returns None if we can't resolve the path or it
/// doesn't exist yet (first-run pre-download).
fn hf_hub_cache_dir() -> Option<std::path::PathBuf> {
    crate::tool_manager::hf_hub_cache_dir().filter(|path| path.exists())
}

/// Total byte size of every regular file under ``path``. Used as a
/// "is the proxy downloading models right now" signal: HF model
/// downloads land in this tree and grow it monotonically, even when
/// the python process is otherwise quiet (no log writes). Errors
/// during the walk are swallowed — a partial sum is still a useful
/// signal, and a zero sum just means we miss this tick of evidence.
///
/// Bounded by ``max_entries`` to keep cost predictable on a warm
/// cache that already has tens of thousands of files.
fn total_dir_size_bytes(path: &std::path::Path, max_entries: usize) -> u64 {
    let mut total: u64 = 0;
    let mut visited: usize = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if visited >= max_entries {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited >= max_entries {
                break;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                // HF cache uses symlinks under ``snapshots/`` pointing into
                // ``blobs/``. Counting the blobs is enough; following the
                // symlink would double-count.
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
    }
    total
}

/// Whether log mtime advanced since the last poll. Counts the
/// transition None → Some(t) (first observation after the proxy
/// began writing) as advancement; a Some → None transition does not
/// (logs don't disappear during a healthy boot).
fn log_mtime_advanced(
    prev: Option<std::time::SystemTime>,
    current: Option<std::time::SystemTime>,
) -> bool {
    current.is_some() && current != prev
}

/// Whether the HF cache grew since the last poll. The first
/// observation (no prev) counts as growth iff the directory has
/// any content — that handles the "cache appeared partway through
/// boot" case where the dir didn't exist when we started but does
/// now. A shrink (which can happen if HF prunes its cache during
/// boot — rare but possible) is *not* growth.
fn hf_cache_grew(prev: Option<u64>, current: u64) -> bool {
    match prev {
        Some(p) => current > p,
        None => current > 0,
    }
}

/// Whether the proxy is bound to its loopback port. Activity-only
/// signal — does NOT imply reachability. The kernel still completes
/// `accept()` even when uvicorn's event loop is held by an in-flight
/// upstream call (e.g. a forwarded `POST /v1/messages` retrying
/// against a 429-ing Anthropic), so a successful TCP connect proves
/// the python process is alive and bound, even when no HTTP endpoint
/// (`/livez`, `/health`, `/stats`) answers within the probe window.
/// 1s timeout is enough for a localhost SYN/SYN-ACK and short enough
/// not to dominate the 500ms loop tick if the OS is mid-bind.
fn tcp_port_accepts_connection(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Probe the proxy's loopback port with a 1s timeout. See
/// [`tcp_port_accepts_connection`] for semantics. The backend port is
/// normally 6768 but may have been switched to a fallback by `backend_port`.
pub(crate) fn proxy_port_accepts_connection() -> bool {
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], crate::backend_port::get()).into();
    tcp_port_accepts_connection(addr, std::time::Duration::from_secs(1))
}

/// Parse the `ps -p PID -o time=` accumulated CPU time format.
/// macOS `ps` emits this as `MM:SS.ss`, `HH:MM:SS`, or `D-HH:MM:SS`
/// depending on duration. Returns whole seconds; sub-second precision
/// is dropped (we only care about per-tick advancement, which is
/// always >=1s of CPU work to register).
fn parse_ps_cpu_time(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (days, rest) = match trimmed.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0u64, trimmed),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (h, m, s_raw) = match parts.as_slice() {
        [h, m, s] => (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?, *s),
        [m, s] => (0u64, m.parse::<u64>().ok()?, *s),
        _ => return None,
    };
    // Drop fractional seconds.
    let s_whole = s_raw.split('.').next()?.parse::<u64>().ok()?;
    Some(days * 86400 + h * 3600 + m * 60 + s_whole)
}

/// Read accumulated CPU time (seconds) for ``pid`` via macOS `ps`.
/// Returns None if the process is gone or `ps` fails. Cheap enough
/// to call on a 500ms boot-validation tick — fork+exec of a tiny
/// system binary, no I/O beyond the kernel proc table.
pub(crate) fn tracked_process_cpu_time_secs(pid: u32) -> Option<u64> {
    let output = crate::proc::command("ps")
        .args(["-p", &pid.to_string(), "-o", "time="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ps_cpu_time(&String::from_utf8_lossy(&output.stdout))
}

/// Whether the tracked process's accumulated CPU time advanced since
/// the previous observation. Catches the "alive but silent" case —
/// e.g. ONNX graph compile, model load, any synchronous CPU-bound
/// work in the proxy's lifespan startup that produces no log writes,
/// no HF cache growth, and doesn't yet bind :6768. Treats the first
/// observation (None → Some(>0)) as growth so a process that's
/// already burned cycles before we started polling counts as active;
/// None → Some(0) is "just spawned, not yet doing work" and is NOT
/// growth (matches `hf_cache_grew` semantics).
fn cpu_time_advanced(prev: Option<u64>, current: Option<u64>) -> bool {
    match (prev, current) {
        (Some(p), Some(c)) => c > p,
        (None, Some(c)) => c > 0,
        _ => false,
    }
}

/// Pure decision function for the boot-validation stall guard.
/// Extracted from the polling loop so it can be tested without
/// mocking the filesystem, the network, and a clock.
///
/// Returns true iff we have waited past the grace window AND
/// nothing has refreshed the activity timer for the silence window.
/// Boundaries are strict (>) so consts read intuitively as
/// "starts checking after grace, fires after silence."
fn boot_validation_stalled(
    elapsed: std::time::Duration,
    activity_age: std::time::Duration,
    grace: std::time::Duration,
    silence: std::time::Duration,
) -> bool {
    elapsed > grace && activity_age > silence
}

/// Pure decision for the boot-validation absolute timeout. The soft cap
/// (`max`) applies normally, but while a first-run model download is still
/// growing the HF cache we wait up to `hard_max` instead — a slow download is
/// not a failed boot (Sentry RUST-4A). Extracted from the polling loop so the
/// ceiling logic is testable without a clock or filesystem.
fn boot_validation_timed_out(
    elapsed: std::time::Duration,
    max: std::time::Duration,
    hard_max: std::time::Duration,
    download_active: bool,
) -> bool {
    let ceiling = if download_active { hard_max } else { max };
    elapsed >= ceiling
}

/// Newest mtime of any `headroom-proxy*.log` file in the logs directory, as
/// a "is the proxy doing anything" signal. Returns None if no logs yet.
pub(crate) fn newest_proxy_log_mtime(logs_dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let entries = std::fs::read_dir(logs_dir).ok()?;
    let mut newest: Option<std::time::SystemTime> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("headroom-proxy") || !name_str.ends_with(".log") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                newest = Some(match newest {
                    Some(prev) if prev > mtime => prev,
                    _ => mtime,
                });
            }
        }
    }
    newest
}

/// User-facing message shown during boot validation. Evolves with elapsed
/// time and whether the proxy log is actively being written to. Cycles
/// through a rotating set of sub-messages per phase so the UI never looks
/// frozen even when all phases last a while.
fn boot_validation_message(elapsed_secs: u64, active: bool) -> String {
    let prefix = if elapsed_secs < 10 {
        "Launching Headroom".to_string()
    } else if elapsed_secs < 30 {
        if active {
            "Warming up Headroom's runtime".to_string()
        } else {
            "Launching Headroom".to_string()
        }
    } else if elapsed_secs < 90 {
        // Rotate across a few descriptive phrasings so the line changes
        // every ~10 seconds instead of repeating identically.
        let rotation = (elapsed_secs / 10) % 3;
        match rotation {
            0 => "Preparing Headroom's ML subsystems".to_string(),
            1 => "Loading optimization pipeline".to_string(),
            _ => "Initializing caches and request handlers".to_string(),
        }
    } else if elapsed_secs < 240 {
        let rotation = (elapsed_secs / 15) % 3;
        match rotation {
            0 => "Downloading Headroom's ML models (first-run only)".to_string(),
            1 => "Fetching model weights from Hugging Face".to_string(),
            _ => "Preparing model caches for first-time use".to_string(),
        }
    } else {
        "Finishing up the first-run download — slower connections may take several more minutes"
            .to_string()
    };

    let hint = if active {
        " · activity detected"
    } else if elapsed_secs > 60 {
        " · this is normal for a first-time upgrade"
    } else {
        ""
    };

    format!("{prefix}… ({}s elapsed{})", elapsed_secs, hint)
}

/// Reasons `ensure_headroom_running` may have returned `Ok(())` without
/// actually spawning a tracked child. Captured immediately after the call so
/// a "Stalled" / "NotStarted" Sentry event can attribute the silent no-op.
#[derive(Debug, Clone)]
struct PostSpawnSnapshot {
    tracked_child: bool,
    python_installed: bool,
    proxy_bypass: bool,
    pricing_allows_optimization: bool,
    runtime_paused: bool,
    proxy_reachable: bool,
    ensure_error: Option<String>,
}

/// Outcome of the boot-validation loop.
#[derive(Debug)]
pub enum BootValidationOutcome {
    /// Proxy reachable via /livez within the max timeout.
    Reachable,
    /// Proxy process exited before becoming reachable.
    ProcessExited,
    /// No log activity for long enough that we consider the proxy stalled.
    Stalled,
    /// Hit the absolute max without reachability or obvious failure.
    TimedOut,
    /// `ensure_headroom_running` short-circuited or errored — there is no
    /// tracked child to wait on AND no externally-reachable proxy on :6768.
    /// Reported instead of `Stalled` so we don't burn ~120s waiting for a
    /// process that was never going to start.
    NotStarted,
    /// The backend port is held by a named process that is not ours, so the
    /// freshly spawned child can never bind it. That squatter's accept()
    /// keeps the TCP activity signal green, which is how RUST-4A burned the
    /// full 600s boot budget; bail as soon as the occupant is identified.
    ForeignPortOccupant,
}

impl BootValidationOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, BootValidationOutcome::Reachable)
    }
    pub fn label(&self) -> &'static str {
        match self {
            BootValidationOutcome::Reachable => "reachable",
            BootValidationOutcome::ProcessExited => "process_exited",
            BootValidationOutcome::Stalled => "stalled",
            BootValidationOutcome::TimedOut => "timed_out",
            BootValidationOutcome::NotStarted => "not_started",
            BootValidationOutcome::ForeignPortOccupant => "foreign_port_occupant",
        }
    }
}

pub struct AppState {
    pub tool_manager: ToolManager,
    pub recent_usage: Mutex<Vec<UsageEvent>>,
    pub headroom_process: Mutex<Option<Child>>,
    lifecycle_lock: Mutex<()>,
    /// Held for the full duration of a runtime upgrade. A second call to
    /// `run_upgrade_with_ui` tries `try_lock` and bails if already held.
    upgrade_lock: Mutex<()>,
    pub runtime_paused: Mutex<bool>,
    /// True when the watchdog auto-paused the runtime after giving up on
    /// restarting a wedged/unreachable proxy — as opposed to a deliberate
    /// user pause (`runtime_paused` with this false). Drives the self-heal
    /// auto-resume loop (only auto-paused runtimes are retried) and the
    /// "stopped unexpectedly" banner. Cleared by any successful resume and by
    /// an explicit user pause.
    pub runtime_auto_paused: AtomicBool,
    pub runtime_starting: Mutex<bool>,
    /// True while an atomic runtime upgrade is running (install + boot validation).
    /// Gates the watchdog from auto-pausing during the ~minutes-long upgrade.
    pub runtime_upgrade_in_progress: Mutex<bool>,
    pub runtime_upgrade_progress: Mutex<RuntimeUpgradeProgress>,
    pub last_startup_error: Mutex<Option<String>>,
    /// Exit status of the last tracked child that died on its own (not via
    /// `stop_headroom`). Every `runtime_status` poll reaps an exited child and
    /// used to discard the status, so a crash-looping backend reached the
    /// watchdog give-up capture as "still_alive_or_untracked" with no exit
    /// code (Sentry RUST-53). Cleared on each successful spawn.
    pub last_child_natural_exit: Mutex<Option<String>>,
    pub bootstrap_progress: Mutex<BootstrapProgress>,
    /// Cause class and technical detail of the most recent bootstrap failure,
    /// captured at the failure site where pip's stderr tail is still in hand.
    /// The install screen has no other route to those -- it renders only the
    /// friendly message -- so a user report would otherwise repeat back copy
    /// we wrote and name nothing actionable. `None` until a bootstrap fails.
    pub bootstrap_failure_report: Mutex<Option<BootstrapFailureReport>>,
    pub headroom_learn_state: Mutex<HeadroomLearnRuntimeState>,
    /// Last Claude AI OAuth bearer token seen passing through the proxy intercept.
    /// Only populated when the user runs Claude Code authenticated via Claude AI (not API key).
    /// Wrapped in Arc so the proxy_intercept task can share it without going through AppState.
    pub claude_bearer_token: Arc<Mutex<Option<BearerToken>>>,
    /// Latest Codex (OpenAI) rate-limit snapshot captured by the proxy intercept
    /// from `x-codex-*` response headers. Wrapped in Arc so the proxy_intercept
    /// task can update it without going through AppState; read by
    /// `pricing::fetch_codex_usage` to drive the Codex usage gauge.
    pub codex_rate_limits: Arc<Mutex<Option<CodexRateLimitSnapshot>>>,
    /// OpenAI/ChatGPT plan decoded from the latest Codex OAuth bearer JWT seen by
    /// the proxy intercept (`proxy_intercept::decode_codex_plan_tier`). Read by
    /// `pricing::fetch_codex_usage` to pick the recommended upgrade tier.
    pub codex_plan_tier: Arc<Mutex<Option<crate::models::CodexPlanTier>>>,
    /// Why the always-on intercept is not listening on 6767, written by
    /// `proxy_intercept::spawn`. Separate from `last_startup_error` (which the
    /// Python runtime's start path clears) because the two fail independently.
    pub intercept_bind_error: crate::proxy_intercept::BindErrorSlot,
    /// When true, the Rust intercept on :6767 forwards traffic directly to
    /// api.anthropic.com instead of the Python proxy on :6768. Flipped on by
    /// `enforce_pricing_gate` once a Pro/Max user crosses the disable threshold
    /// without a Headroom subscription, so existing CC sessions stay alive
    /// while optimization is genuinely off.
    pub proxy_bypass: Arc<AtomicBool>,
    /// Codex-only parallel to `proxy_bypass`: when true, the intercept forwards
    /// OpenAI-path (Codex) traffic directly to api.openai.com while leaving the
    /// Python proxy up for Claude. Flipped by `apply_codex_pricing_gate_status`
    /// once a free user crosses the weekly Codex disable threshold, so Codex
    /// gating never pauses Claude optimization for mixed users.
    pub codex_bypass: Arc<AtomicBool>,
    /// Set when the Claude pricing gate trips but Codex is still enabled, so the
    /// Python backend is kept alive for Codex. The intercept forwards only
    /// non-Codex (Claude) traffic direct in this mode; Codex keeps routing
    /// through the backend. Mutually exclusive with `proxy_bypass` (which tears
    /// the backend down and forwards everything direct). This keeps a Claude
    /// overage from pausing Codex optimization for mixed users, the symmetric
    /// counterpart to `codex_bypass`.
    pub claude_only_bypass: Arc<AtomicBool>,
    /// Debounce streak for `codex_bypass`, mirroring `pricing_gate_violation_streak`.
    codex_gate_violation_streak: Arc<AtomicU32>,
    /// Number of consecutive `apply_pricing_gate_status` calls that reported
    /// `optimization_allowed=false` while bypass was off. Acts as a debounce:
    /// the ungated→gated transition only fires once this hits
    /// `PRICING_GATE_DEBOUNCE_POLLS`. Reset to 0 on any ungated poll. Prevents
    /// a single bad pricing read (network blip, brief utilization spike) from
    /// flipping the gate off and back on within minutes.
    pricing_gate_violation_streak: Arc<AtomicU32>,
    /// Per-session rising-edge latches so the weekly-limit nudge is reported to
    /// the server at most once per condition while it holds, instead of on every
    /// 60s pricing poll. They reset to `false` on any non-gated poll and on app
    /// restart; the server throttles to ~one email per weekly window, so a
    /// re-report after restart is harmless.
    weekly_limit_reached_reported: Arc<AtomicBool>,
    weekly_limit_approaching_reported: Arc<AtomicBool>,
    launch_profile: Mutex<LaunchProfile>,
    launch_profile_path: std::path::PathBuf,
    last_known_good_plan: Mutex<Option<LastKnownGoodPlan>>,
    last_known_good_plan_path: std::path::PathBuf,
    savings_tracker: Mutex<SavingsTracker>,
    /// `(last_reported_total, reported_at)` for the cumulative-savings
    /// heartbeat. In-memory, so it resets on restart. See
    /// [`CUMULATIVE_REPORT_INTERVAL`].
    cumulative_report_throttle: Mutex<Option<(u64, Instant)>>,
    activity_facts: Mutex<ActivityFacts>,
    cached_clients: Mutex<Option<(Vec<ClientStatus>, Instant)>>,
    cached_headroom_stats: Mutex<Option<(Option<HeadroomDashboardStats>, Instant)>>,
    /// Last `/stats` payload that actually arrived, with the time it did.
    /// Kept apart from `cached_headroom_stats` so the miss backoff and the
    /// retention window measure different things: that cache stamps the last
    /// FETCH (and must expire fast on success, slowly on failure), this one
    /// stamps the last real ANSWER.
    last_good_headroom_stats: Mutex<Option<(HeadroomDashboardStats, Instant)>>,
    /// `(history, fetched_at, fresh)` — `fresh` is false when `history` is a
    /// retained last-good value served because the latest fetch failed (proxy
    /// paused/unreachable), so it re-probes on the short miss TTL.
    cached_headroom_history: Mutex<Option<(Option<HeadroomSavingsHistoryResponse>, Instant, bool)>>,
    cached_rtk_gain_summary: Mutex<Option<(Option<RtkGainSummary>, Instant)>>,
    cached_rtk_today_stats: Mutex<Option<(Option<crate::models::RtkTodayStats>, Instant)>>,
    cached_claude_profile: Mutex<Option<(Option<String>, ClaudeAccountProfile, Instant)>>,
    /// TTL-cached Codex identity profile, the Codex analog of
    /// `cached_claude_profile`. Built by `pricing::detect_codex_profile` from
    /// `~/.codex/auth.json` + the live `codex_plan_tier` slot; no network fetch,
    /// so the cache is a plain value + timestamp.
    cached_codex_profile: Mutex<Option<(Option<CodexAccountProfile>, Instant)>>,
    /// When the current run of transient profile-fetch failures began. Set the
    /// first time we suppress a transient error (and serve the last good
    /// profile), cleared on the next successful fetch. Once the run exceeds
    /// `STALE_PROFILE_ESCALATE_AFTER` we stop suppressing and surface the
    /// banner — the token-rotation gap has lasted long enough to be real.
    stale_profile_since: Mutex<Option<Instant>>,
    /// Last `IdentityFingerprint` we successfully posted to
    /// `desktop/grace/start`. Used by the bearer-triggered identity-pusher
    /// worker to skip redundant posts when the same Claude account/plan is
    /// already on file with headroom-web.
    last_pushed_identity_fingerprint: Mutex<Option<crate::pricing::IdentityFingerprint>>,
    /// When we most recently completed a fresh OAuth profile fetch that
    /// returned a *complete* identity (UUID + email + non-Unknown plan
    /// tier). The identity-pusher worker uses this to throttle further
    /// `/api/oauth/profile` calls to ~once per 24 h once we already know
    /// who the user is. `Instant`, so it resets on app restart — first
    /// post-restart bearer always triggers a fresh fetch.
    last_complete_identity_fetch_at: Mutex<Option<Instant>>,
    /// Cached stdout of `headroom memory export`. Shared by every OptimizePanel
    /// that mounts at once — without it, N panels = N Python cold-starts.
    cached_memory_export: Mutex<Option<(String, Instant)>>,
    /// Cached result of `list_claude_code_projects`. Scanning the projects dir,
    /// reading session files, and computing per-project learn metadata is the
    /// main cost of opening the Optimize tab. TTL is short enough that
    /// just-finished learn runs appear promptly once their explicit
    /// invalidation fires.
    cached_claude_code_projects: Mutex<Option<(Vec<ClaudeCodeProject>, Instant)>>,
    /// Cached `detect_headroom_learn_prereq_status`. The Claude CLI location
    /// can't change without explicit user action during a session, and the
    /// fallback shell probe can take up to 2s, so we keep this sticky and
    /// expose an invalidator for the user's "Re-check" button.
    cached_headroom_learn_prereq: Mutex<Option<HeadroomLearnPrereqStatus>>,
    /// Cached `runtime_status()` output. The tray-icon updater, proxy
    /// watchdog, and frontend pollers all ask for runtime status on tight
    /// intervals; each uncached call hits `is_headroom_proxy_reachable`
    /// (blocking HTTP) plus a handful of file stats. A short TTL dedupes
    /// the work across all those callers without any visible lag.
    cached_runtime_status: Mutex<Option<(RuntimeStatus, Instant)>>,
    /// Set once we've kicked off (or skipped) the one-shot Kompress model
    /// prefetch for this app launch, so `maybe_prefetch_kompress` never fires
    /// the ~260MB download more than once per process.
    kompress_prefetch_attempted: AtomicBool,
    /// Latched true the first time native savings history loads this process.
    /// Drives the Home chart's startup loading state so the sparse tracker-only
    /// layer is never shown before the full history merges in.
    savings_history_loaded: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct HeadroomLearnRuntimeState {
    running: bool,
    project_path: Option<String>,
    started_at: Option<chrono::DateTime<Utc>>,
    finished_at: Option<chrono::DateTime<Utc>>,
    success: Option<bool>,
    summary: String,
    error: Option<String>,
    output_tail: Vec<String>,
    /// Live one-liner for the run in progress, from the CLI's own stage output.
    /// A scan spawns a headless agent session that fires the user's hooks, so a
    /// bare timer reads as a black box; this is what the row shows instead.
    current_step: Option<String>,
}

impl AppState {
    pub fn new() -> Result<Self> {
        Self::new_in(app_data_dir())
    }

    pub(crate) fn new_in(base_dir: PathBuf) -> Result<Self> {
        ensure_data_dirs(&base_dir)?;

        let runtime = ManagedRuntime::bootstrap_root(&base_dir);
        let tool_manager = ToolManager::new(runtime);
        let (launch_profile, launch_profile_path) = LaunchProfile::load_or_create(&base_dir)?;
        // The proxy spawn reads the override through the module-level cache
        // (no AppState in hand there, same as the backend port), so a launch
        // has to publish what it just loaded or the first spawn of the session
        // would boot at the default upstream.
        crate::upstream_override::publish(launch_profile.upstream_override.clone());
        let (last_known_good_plan, last_known_good_plan_path) = LastKnownGoodPlan::load(&base_dir);
        let savings_tracker = SavingsTracker::load_or_create(&base_dir)?;
        let activity_facts = ActivityFacts::load_or_create(&base_dir)?;

        let state = Self {
            tool_manager,
            recent_usage: Mutex::new(Vec::new()),
            headroom_process: Mutex::new(None),
            lifecycle_lock: Mutex::new(()),
            upgrade_lock: Mutex::new(()),
            runtime_paused: Mutex::new(false),
            runtime_auto_paused: AtomicBool::new(false),
            runtime_starting: Mutex::new(false),
            runtime_upgrade_in_progress: Mutex::new(false),
            runtime_upgrade_progress: Mutex::new(RuntimeUpgradeProgress {
                running: false,
                complete: false,
                failed: false,
                current_step: "Idle".into(),
                message: String::new(),
                overall_percent: 0,
                from_version: None,
                to_version: None,
            }),
            last_startup_error: Mutex::new(None),
            last_child_natural_exit: Mutex::new(None),
            bootstrap_progress: Mutex::new(BootstrapProgress {
                running: false,
                complete: false,
                failed: false,
                current_step: "Idle".into(),
                message: "Installer has not started.".into(),
                current_step_eta_seconds: 0,
                overall_percent: 0,
            }),
            bootstrap_failure_report: Mutex::new(None),
            claude_bearer_token: Arc::new(Mutex::new(None)),
            codex_rate_limits: Arc::new(Mutex::new(None)),
            codex_plan_tier: Arc::new(Mutex::new(None)),
            intercept_bind_error: Arc::new(Mutex::new(None)),
            proxy_bypass: Arc::new(AtomicBool::new(false)),
            claude_only_bypass: Arc::new(AtomicBool::new(false)),
            codex_bypass: Arc::new(AtomicBool::new(false)),
            codex_gate_violation_streak: Arc::new(AtomicU32::new(0)),
            pricing_gate_violation_streak: Arc::new(AtomicU32::new(0)),
            weekly_limit_reached_reported: Arc::new(AtomicBool::new(false)),
            weekly_limit_approaching_reported: Arc::new(AtomicBool::new(false)),
            headroom_learn_state: Mutex::new(HeadroomLearnRuntimeState {
                running: false,
                project_path: None,
                started_at: None,
                finished_at: None,
                success: None,
                summary: "Select a project to run headroom learn.".into(),
                error: None,
                output_tail: Vec::new(),
                current_step: None,
            }),
            launch_profile: Mutex::new(launch_profile),
            launch_profile_path,
            last_known_good_plan: Mutex::new(last_known_good_plan),
            last_known_good_plan_path,
            savings_tracker: Mutex::new(savings_tracker),
            cumulative_report_throttle: Mutex::new(None),
            activity_facts: Mutex::new(activity_facts),
            cached_clients: Mutex::new(None),
            cached_headroom_stats: Mutex::new(None),
            last_good_headroom_stats: Mutex::new(None),
            cached_headroom_history: Mutex::new(None),
            cached_rtk_gain_summary: Mutex::new(None),
            cached_rtk_today_stats: Mutex::new(None),
            cached_claude_profile: Mutex::new(None),
            cached_codex_profile: Mutex::new(None),
            stale_profile_since: Mutex::new(None),
            last_pushed_identity_fingerprint: Mutex::new(None),
            last_complete_identity_fetch_at: Mutex::new(None),
            cached_memory_export: Mutex::new(None),
            cached_claude_code_projects: Mutex::new(None),
            cached_headroom_learn_prereq: Mutex::new(None),
            cached_runtime_status: Mutex::new(None),
            kompress_prefetch_attempted: AtomicBool::new(false),
            savings_history_loaded: AtomicBool::new(false),
        };

        Ok(state)
    }

    pub fn warm_runtime_on_launch(&self, app: &tauri::AppHandle) {
        // Always check for a mid-upgrade interrupt first. If the last app
        // run was killed between move-aside and commit, the venv.backup/
        // dir holds the real working environment and the live venv is a
        // partial install. Restore before doing anything else.
        let _ = self.tool_manager.recover_from_interrupted_upgrade();

        if !self.tool_manager.python_runtime_installed() {
            // First-run; start_bootstrap (wizard) handles install.
            return;
        }

        self.set_runtime_starting(true);
        self.enforce_pricing_gate();
        self.stop_python_if_gated();

        // App-version-triggered atomic runtime upgrade. Runs here — right after
        // the (local, cached) pricing gate and BEFORE the rtk GitHub refresh
        // below — because this is the work the user clicked "Restart now" for,
        // and the decision is a local receipt-vs-pin comparison with no network.
        // ensure_rtk_current() hits GitHub Releases and can stall for seconds on
        // a slow link; keeping the upgrade ahead of it means the "Preparing
        // update" progress UI appears within milliseconds of launch instead of
        // after that network round-trip, so the app no longer looks dead while a
        // slow connection resolves rtk. The pricing gate stays ahead of the
        // upgrade on purpose: run_upgrade_with_ui reads the gate flags at the end
        // of boot validation to decide whether to stop the validation Python.
        // Replaces the old receipt-vs-pinned drift path.
        if self.should_run_runtime_upgrade(app) {
            // Auto-trigger never forces rebuild — that's reserved for the
            // user-facing "Retry with full rebuild" recovery flow.
            self.run_upgrade_with_ui(app, false);
        } else {
            // No Python maintenance needed, but the desktop app version may
            // still have moved (cosmetic-only release on the same headroom-ai
            // pin). Without this stamp the launch profile drifts: every
            // version in the chain that ships the same headroom-ai never
            // gets recorded, and `previous_app_version` reads back as
            // whatever desktop version most recently changed the Python pin
            // — which can be many releases stale.
            let current_app_version = app.package_info().version.to_string();
            if self.can_stamp_no_maintenance(&current_app_version) {
                self.stamp_app_version(&current_app_version);
            }
        }

        // rtk is pinned to a specific version in source. On an app upgrade the
        // bundled binary on disk can be stale because bootstrap only runs on
        // first-run. Reinstall if the receipt's version doesn't match the
        // pinned version. install_rtk hits GitHub Releases, so this needs
        // network — failure here is logged and we move on. Ordered AFTER the
        // runtime upgrade so a slow GitHub round-trip can't delay the upgrade UI.
        match self.tool_manager.ensure_rtk_current() {
            Ok(true) => log::info!("rtk refreshed to pinned version on launch"),
            Ok(false) => {}
            Err(err) => log::warn!("rtk version check on launch failed: {err}"),
        }

        if let Err(err) = ensure_rtk_integrations(
            &self.tool_manager.rtk_entrypoint(),
            &self.tool_manager.managed_python(),
        ) {
            log::warn!("RTK integrations failed during warm_runtime_on_launch: {err:#}");
        }

        // Pre-wrapper installs still have a bare symlink shim; refresh it so
        // the conversion counter starts recording without a reinstall.
        if self.tool_manager.markitdown_installed() {
            if let Err(err) = self.tool_manager.ensure_markitdown_shim() {
                log::warn!("markitdown shim refresh failed during warm_runtime_on_launch: {err:#}");
            }
        }

        // Heals installs already in the field: older installs and pre-fix venv
        // swaps never vendored the MSVC runtime DLLs, so on a redist-less
        // Windows box torch/onnxruntime cannot load (RUST-7W/8V/8W). Runs
        // before the proxy starts; a stat-only no-op everywhere else.
        if let Err(err) = self.tool_manager.ensure_msvc_runtime_dlls() {
            log::warn!("MSVC runtime DLL vendoring failed during warm_runtime_on_launch: {err:#}");
        }

        // Independent of the upgrade: if MCP is not configured (e.g. it failed
        // during a prior install), retry it now.
        if let Err(err) = self.tool_manager.ensure_mcp_configured() {
            // install_headroom_mcp captures rich structured data to Sentry
            // at the failure site; log to file only to avoid a duplicate
            // (and stripped) Sentry event from the FileLogger forwarder.
            log::info!("headroom MCP configuration failed: {err:#}");
        }

        // Seed the output-shaper savings baseline BEFORE starting the proxy.
        // This is the launch path for already-installed users (start_bootstrap
        // only runs the first-install wizard), so without it the seeding never
        // runs after an app update. It must precede proxy start: the recorder
        // loads the baseline once at boot and clobbers a later-written one on
        // flush, so seeding first is what lets the number appear without an app
        // relaunch. Idempotent and bounded; we are already on a background
        // thread, so the one-time scan does not block the UI.
        self.tool_manager.seed_verbosity_baseline_if_needed();

        match self.ensure_headroom_running() {
            Ok(()) => {
                crate::port_conflict::note_proxy_started(app);
            }
            Err(err) => {
                log::debug!("failed to auto-start headroom during app launch: {err}");
                let handled = crate::port_conflict::note_proxy_failed(app, &err, true);
                if !handled {
                    crate::capture_headroom_start_failure(
                        "headroom auto-start failed during launch",
                        &err,
                    );
                }
            }
        }

        // Hold `starting` until the probe `runtime_status()` uses
        // (`is_headroom_proxy_reachable` → 6767/readyz) actually returns true.
        // `wait_for_boot_validation` accepts /livez, which can flip green
        // before /readyz does; clearing `starting` on livez alone opens a
        // window where the UI poller sees !running && !starting and fires
        // the "Headroom isn't running" notification while readiness is still
        // loading.
        //
        // 5-minute ceiling: cold-boot in the Python proxy synchronously warms
        // an ONNX embedder (hf_hub_download of all-MiniLM-L6-v2), which on
        // first launch or with a slow network can hold /readyz at 503 for
        // 30s+. The old 60s deadline cleared `starting` before /readyz came
        // up, letting the watchdog auto-pause a process that was about to
        // recover — see Sentry `proxy_unreachable_post_boot`. The loop breaks
        // immediately on reachability, so a longer ceiling has no cost in the
        // happy path; this only changes behavior for genuinely slow boots.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        while std::time::Instant::now() < deadline {
            if is_headroom_proxy_reachable() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }

        self.set_runtime_starting(false);
    }

    fn runtime_maintenance_plan_for_app_version(
        &self,
        current_app_version: &str,
    ) -> Option<RuntimeMaintenancePlan> {
        if runtime_upgrade_disabled_by_env() {
            log::debug!("HEADROOM_SKIP_RUNTIME_UPGRADE is set — skipping runtime upgrade check.");
            return None;
        }
        let profile = self.launch_profile.lock();
        let version_matches = profile
            .last_launched_app_version
            .as_deref()
            .map(|v| v == current_app_version)
            .unwrap_or(false);
        if version_matches {
            return None;
        }
        if let Some(failure) = profile.last_runtime_upgrade_failure.as_ref() {
            if failure.app_version == current_app_version
                && failure.attempts >= MAX_UPGRADE_AUTO_RETRIES
            {
                return None;
            }
        }
        drop(profile);
        if let Some(release) = self.tool_manager.check_headroom_upgrade() {
            return Some(RuntimeMaintenancePlan::Upgrade(release));
        }
        if self.tool_manager.requirements_are_stale() {
            return Some(RuntimeMaintenancePlan::RequirementsRepair);
        }
        None
    }

    /// Returns true if the app version changed since the last successful
    /// launch AND an actual upgrade is needed (either headroom-ai version
    /// mismatch or requirements lock drift). Also gates on the retry budget
    /// from any prior upgrade failure, and on `HEADROOM_SKIP_RUNTIME_UPGRADE`.
    pub fn should_run_runtime_upgrade(&self, app: &tauri::AppHandle) -> bool {
        self.runtime_maintenance_plan_for_app_version(&app.package_info().version.to_string())
            .is_some()
    }

    /// Run a full atomic runtime upgrade with UI progress + boot validation.
    ///
    /// Acquires `upgrade_lock` to guard against concurrent launches. Stops
    /// the proxy, runs `atomic_upgrade_headroom`, then validates the new
    /// runtime by waiting for proxy reachability. On boot-validation failure,
    /// rolls back to the previous venv and records a failure so the UI can
    /// render a retry banner.
    ///
    /// `force_rebuild` skips the in-place upgrade attempt and goes straight
    /// to atomic rebuild. Set by the user-facing "Retry with full rebuild"
    /// flow when an in-place upgrade installed cleanly but the proxy
    /// failed to boot — typically an ABI mismatch in native deps that pip
    /// can't detect.
    pub fn run_upgrade_with_ui(&self, app: &tauri::AppHandle, force_rebuild: bool) {
        let _guard = match self.upgrade_lock.try_lock() {
            Some(g) => g,
            None => {
                log::debug!("run_upgrade_with_ui: upgrade already running; skipping");
                return;
            }
        };

        let current_app_version = app.package_info().version.to_string();
        let maintenance_plan =
            match self.runtime_maintenance_plan_for_app_version(&current_app_version) {
                Some(plan) => plan,
                None => {
                    // App version changed but no runtime maintenance is actually
                    // needed — just stamp the version.
                    self.stamp_app_version(&current_app_version);
                    return;
                }
            };
        let maintenance_kind = match &maintenance_plan {
            RuntimeMaintenancePlan::Upgrade(_) => RuntimeMaintenanceKind::Upgrade,
            RuntimeMaintenancePlan::RequirementsRepair => {
                RuntimeMaintenanceKind::RequirementsRepair
            }
        };
        let target_version = match &maintenance_plan {
            RuntimeMaintenancePlan::Upgrade(release) => release.version().to_string(),
            RuntimeMaintenancePlan::RequirementsRepair => self
                .tool_manager
                .installed_headroom_version()
                .unwrap_or_else(|| "unknown".into()),
        };
        let installed_version = self.tool_manager.installed_headroom_version();

        // User-facing from/to are the app versions — headroom-ai versions are
        // an implementation detail tracked in the failure record only.
        let previous_app_version = self.launch_profile.lock().last_launched_app_version.clone();

        // Snapshot the newest proxy log mtime BEFORE we stop the old proxy and
        // install the new one. At failure time we compare against this to tell
        // "the new proxy wrote some logs (so it at least started python)" from
        // "the new proxy never produced any log activity (likely failed to
        // spawn or crashed pre-import)".
        let pre_upgrade_log_mtime = newest_proxy_log_mtime(&self.tool_manager.logs_dir());

        *self.runtime_upgrade_in_progress.lock() = true;
        self.invalidate_runtime_status_cache();

        // We're about to stop the old backend and spawn a fresh one. Suppress
        // Codex reconnect warnings across the whole reinstall+boot so the
        // self-inflicted down->up transition isn't paged as `backend_unreachable`.
        crate::proxy_intercept::suppress_codex_reconnect_reports_for(
            std::time::Duration::from_secs(600),
        );

        // Clear the flag on EVERY exit, including a panic anywhere in the
        // ~500-line body below. This runs on a bare spawned thread with no
        // catch_unwind and parking_lot mutexes don't poison, so without this
        // guard a panic would leave the flag stuck true for the process
        // lifetime — which disables the watchdog auto-pause and suppresses
        // the pricing gate (see ensure_headroom_running) until app restart.
        struct UpgradeFlagGuard<'a>(&'a AppState);
        impl Drop for UpgradeFlagGuard<'_> {
            fn drop(&mut self) {
                *self.0.runtime_upgrade_in_progress.lock() = false;
                self.0.invalidate_runtime_status_cache();
            }
        }
        let _upgrade_flag_guard = UpgradeFlagGuard(self);

        // Set up progress state + emit initial event.
        self.set_upgrade_progress(|p| {
            p.running = true;
            p.complete = false;
            p.failed = false;
            p.current_step = "Preparing update".into();
            p.message = "Wrapping up the Headroom update.".into();
            p.overall_percent = 0;
            p.from_version = previous_app_version.clone();
            p.to_version = Some(current_app_version.clone());
        });
        emit_runtime_upgrade_progress(app, self);

        self.stop_headroom();

        analytics::track_event(
            app,
            "runtime_upgrade_started",
            Some(serde_json::json!({
                "maintenance_kind": match maintenance_kind {
                    RuntimeMaintenanceKind::Upgrade => "upgrade",
                    RuntimeMaintenanceKind::RequirementsRepair => "requirements_repair",
                },
                "from_version": installed_version,
                "to_version": target_version,
                "app_version": current_app_version,
            })),
        );

        let start = std::time::Instant::now();
        let app_for_progress = app.clone();
        // The callees only require FnMut (no 'static/Send), so capturing
        // &self directly is fine and keeps the borrow checker in play.
        let progress = move |step: BootstrapStepUpdate| {
            self.set_upgrade_progress(|p| {
                p.current_step = step.step.to_string();
                p.message = step.message.clone();
                p.overall_percent = step.percent;
            });
            emit_runtime_upgrade_progress(&app_for_progress, self);
        };

        use crate::tool_manager::UpgradeOutcome;
        let needs_commit_or_rollback = matches!(maintenance_kind, RuntimeMaintenanceKind::Upgrade);
        // Ok carries the pip-output tail captured during install — empty
        // string for RequirementsRepair (no pip ran in our wrapper) and for
        // any path that didn't request a capture. Held across the
        // install→boot-validation boundary so a later boot-validation
        // failure can attach it to the Sentry event.
        let install_result: Result<String, (bool, anyhow::Error)> = match maintenance_plan {
            RuntimeMaintenancePlan::Upgrade(release) => {
                match self
                    .tool_manager
                    .atomic_upgrade_headroom(&release, progress, force_rebuild)
                {
                    UpgradeOutcome::InstalledPendingValidation { pip_output_tail } => {
                        Ok(pip_output_tail)
                    }
                    UpgradeOutcome::InstallFailed { restored, error } => Err((restored, error)),
                }
            }
            RuntimeMaintenancePlan::RequirementsRepair => self
                .tool_manager
                .repair_stale_requirements_with_progress(progress)
                .map(|()| String::new())
                .map_err(|error| (false, error)),
        };
        let install_pip_output_tail: String = match install_result {
            Err((restored, error)) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                log::warn!(
                    "run_upgrade_with_ui: install failed after {duration_ms}ms (restored={restored}): {error:#}"
                );
                let restarted = self.ensure_headroom_running().is_ok();
                let hint = crate::classify_upgrade_error(&error);
                let fallback_hint = match maintenance_kind {
                    RuntimeMaintenanceKind::Upgrade if restored && restarted => {
                        Some("Restarted Headroom with the previous runtime.".into())
                    }
                    RuntimeMaintenanceKind::Upgrade if restored => {
                        Some("Restored the previous runtime, but Headroom still needs a manual restart.".into())
                    }
                    RuntimeMaintenanceKind::Upgrade => {
                        Some("Headroom update failed and the previous runtime could not be restored automatically.".into())
                    }
                    RuntimeMaintenanceKind::RequirementsRepair if restarted => {
                        Some("Restarted Headroom with the existing runtime.".into())
                    }
                    RuntimeMaintenanceKind::RequirementsRepair => {
                        Some("Dependency repair failed and Headroom could not be restarted automatically.".into())
                    }
                };
                self.record_upgrade_failure(RuntimeUpgradeFailure {
                    app_version: current_app_version.clone(),
                    target_headroom_version: target_version.clone(),
                    fallback_headroom_version: installed_version.clone(),
                    failure_phase: UpgradeFailurePhase::Install,
                    attempts: 0, // filled in by record_upgrade_failure
                    first_attempt_at: Utc::now(),
                    last_attempt_at: Utc::now(),
                    error_message: format!("{error:#}"),
                    error_hint: hint.or(fallback_hint),
                    rollback_restored: restored || restarted,
                });
                crate::capture_upgrade_failure(
                    &error,
                    restored,
                    "install",
                    None,
                    Some(duration_ms),
                    Some(target_version.as_str()),
                    installed_version.as_deref(),
                    None,
                    None,
                );
                analytics::track_event(
                    app,
                    "runtime_upgrade_failed",
                    Some(serde_json::json!({
                        "phase": "install",
                        "maintenance_kind": match maintenance_kind {
                            RuntimeMaintenanceKind::Upgrade => "upgrade",
                            RuntimeMaintenanceKind::RequirementsRepair => "requirements_repair",
                        },
                        "attempt": self.upgrade_failure_attempts(&current_app_version),
                        "app_version": current_app_version,
                        "restored": restored,
                        "restarted": restarted,
                        "duration_ms": duration_ms,
                    })),
                );
                self.set_upgrade_progress(|p| {
                    p.running = false;
                    p.complete = false;
                    p.failed = true;
                    p.current_step = "Install failed".into();
                    p.message = match maintenance_kind {
                        RuntimeMaintenanceKind::Upgrade if restored && restarted => {
                            "Headroom update couldn't install. The previous runtime was restored and restarted.".into()
                        }
                        RuntimeMaintenanceKind::Upgrade if restored => {
                            "Headroom update couldn't install. The previous runtime was restored, but it still needs a restart.".into()
                        }
                        RuntimeMaintenanceKind::Upgrade => {
                            "Headroom update couldn't install, and the previous runtime could not be restored automatically.".into()
                        }
                        RuntimeMaintenanceKind::RequirementsRepair if restarted => {
                            "Headroom dependency repair failed. Restarted Headroom with the existing runtime.".into()
                        }
                        RuntimeMaintenanceKind::RequirementsRepair => {
                            "Headroom dependency repair failed, and Headroom could not be restarted automatically.".into()
                        }
                    };
                    p.overall_percent = 100;
                });
                emit_runtime_upgrade_progress(app, self);
                return;
            }
            Ok(tail) => tail,
        };

        // Boot validation: start the proxy and wait for reachability.
        self.set_upgrade_progress(|p| {
            p.current_step = "Verifying update".into();
            p.message =
                "Launching updated Headroom. This can take a minute — Headroom may need to download new ML models.".into();
            p.overall_percent = 97;
        });
        emit_runtime_upgrade_progress(app, self);

        let ensure_err = self
            .ensure_headroom_running()
            .err()
            .map(|err| format!("{err:#}"));
        if let Some(err) = ensure_err.as_deref() {
            log::warn!("run_upgrade_with_ui: new proxy failed to spawn: {err}");
        }
        // Snapshot the conditions that gate ensure_headroom_running so a
        // silent short-circuit ("we returned Ok(()) but never spawned") is
        // attributable in Sentry instead of surfacing as a blank "Stalled".
        let post_spawn = PostSpawnSnapshot {
            tracked_child: self.headroom_process.lock().is_some(),
            python_installed: self.tool_manager.python_runtime_installed(),
            proxy_bypass: self.proxy_bypass.load(std::sync::atomic::Ordering::Acquire),
            pricing_allows_optimization: self.pricing_allows_optimization(),
            runtime_paused: self.runtime_is_paused(),
            proxy_reachable: is_headroom_proxy_reachable(),
            ensure_error: ensure_err,
        };
        log::info!(
            "run_upgrade_with_ui: post-spawn tracked_child={} python_installed={} \
             proxy_bypass={} pricing_allows_optimization={} runtime_paused={} \
             proxy_reachable={} ensure_error={:?}",
            post_spawn.tracked_child,
            post_spawn.python_installed,
            post_spawn.proxy_bypass,
            post_spawn.pricing_allows_optimization,
            post_spawn.runtime_paused,
            post_spawn.proxy_reachable,
            post_spawn.ensure_error,
        );

        let outcome = if !post_spawn.tracked_child && !post_spawn.proxy_reachable {
            // No child to wait on AND nothing already listening on :6768.
            // wait_for_boot_validation would burn ~120s of grace+silence
            // here for nothing — bail with a distinct outcome so the
            // failure path knows it's a non-start, not a hang.
            log::warn!(
                "run_upgrade_with_ui: skipping boot validation: no tracked child and no reachable proxy"
            );
            BootValidationOutcome::NotStarted
        } else {
            let app_for_progress = app.clone();
            self.wait_for_boot_validation(move |elapsed, active| {
                let elapsed_secs = elapsed.as_secs();
                let message = boot_validation_message(elapsed_secs, active);
                // Gently creep 97 → 99.5 over the max budget so the bar keeps
                // moving — the user sees *something* happen during long waits.
                let percent = 97
                    + ((elapsed_secs as u128 * 250 / RUNTIME_UPGRADE_BOOT_MAX_SECS as u128).min(250)
                        as u8)
                        / 100;
                self.set_upgrade_progress(|p| {
                    p.message = message;
                    p.overall_percent = percent.min(99);
                });
                emit_runtime_upgrade_progress(&app_for_progress, self);
            })
        };
        let boot_ok = outcome.is_ok();
        let outcome_label = outcome.label();
        let duration_ms = start.elapsed().as_millis() as u64;
        log::debug!(
            "run_upgrade_with_ui: boot validation {outcome_label} after {}s",
            duration_ms / 1000
        );

        if boot_ok {
            if needs_commit_or_rollback {
                if let Err(err) = self.tool_manager.commit_headroom_upgrade() {
                    log::warn!("commit_headroom_upgrade: non-fatal: {err:#}");
                }
            }
            self.stamp_app_version(&current_app_version);
            self.clear_upgrade_failure();
            self.set_upgrade_progress(|p| {
                p.running = false;
                p.complete = true;
                p.failed = false;
                p.current_step = "Done".into();
                p.message = match maintenance_kind {
                    RuntimeMaintenanceKind::Upgrade => {
                        format!("Headroom updated to {}.", current_app_version)
                    }
                    RuntimeMaintenanceKind::RequirementsRepair => {
                        "Headroom runtime repair completed.".into()
                    }
                };
                p.overall_percent = 100;
            });
            emit_runtime_upgrade_progress(app, self);
            analytics::track_event(
                app,
                "runtime_upgrade_completed",
                Some(serde_json::json!({
                    "maintenance_kind": match maintenance_kind {
                        RuntimeMaintenanceKind::Upgrade => "upgrade",
                        RuntimeMaintenanceKind::RequirementsRepair => "requirements_repair",
                    },
                    "from_version": installed_version,
                    "to_version": target_version,
                    "duration_ms": duration_ms,
                })),
            );
            analytics::set_headroom_ai_version(app, self.tool_manager.installed_headroom_version());
            // ensure_headroom_running's gate guards were suppressed during
            // validation so a gated user's brand-new venv could actually be
            // validated (otherwise we'd commit untested or roll back a
            // perfectly good install). Now that the upgrade has committed,
            // restore the gate state by stopping the validation Python if any
            // gate is asserting Python should be down. Client-side routing is
            // already pointed direct-to-Anthropic by whoever asserted the
            // gate, so the validation Python wasn't receiving traffic anyway.
            // Claude-only gate (Codex enabled) keeps Python up for Codex —
            // same carve-out as stop_python_if_gated / ensure_headroom_running
            // (RUST-53); without it every upgrade bounces the backend for
            // gated Codex users (stop here, watchdog respawn ~5-10s later).
            let gate_wants_python_down =
                self.proxy_bypass.load(std::sync::atomic::Ordering::Acquire)
                    || (!self.pricing_allows_optimization()
                        && !crate::client_adapters::any_gate_exempt_client_enabled())
                    || self.runtime_is_paused();
            if gate_wants_python_down {
                log::info!(
                    "run_upgrade_with_ui: validation succeeded; stopping validation Python because a gate is active"
                );
                self.stop_headroom();
            }
            return;
        }

        // Boot validation failed — roll back to the previous venv when we have
        // one, otherwise leave the repaired runtime in place and surface the
        // failure so the next launch can retry.
        log::warn!(
            "run_upgrade_with_ui: boot validation failed ({}); rolling back to {:?}",
            outcome_label,
            installed_version
        );
        // Diagnostics for Sentry — capture before stop_headroom() tears down
        // the tracked child and the proxy port. These three booleans
        // distinguish the failure modes that all surface as "Stalled":
        //   tracked_child=false → ensure_headroom_running silently no-op'd
        //   new_proxy_log_written=false → spawn happened but python never
        //                                 reached the logging setup
        //   proxy_port_bound=false → uvicorn never reached its bind() call
        let new_proxy_log_written = log_mtime_advanced(
            pre_upgrade_log_mtime,
            newest_proxy_log_mtime(&self.tool_manager.logs_dir()),
        );
        let boot_diagnostics = crate::UpgradeBootDiagnostics {
            tracked_child: self.headroom_process.lock().is_some(),
            new_proxy_log_written,
            proxy_port_bound: proxy_port_accepts_connection(),
            port_occupant: crate::tool_manager::describe_proxy_port_occupant(
                crate::backend_port::get(),
            ),
            python_installed: post_spawn.python_installed,
            proxy_bypass: post_spawn.proxy_bypass,
            pricing_allows_optimization: post_spawn.pricing_allows_optimization,
            runtime_paused: post_spawn.runtime_paused,
            ensure_error: post_spawn.ensure_error.clone(),
            pip_output_tail: install_pip_output_tail.clone(),
        };

        // Capture the tail of the proxy log BEFORE stop_headroom runs — for
        // a process that crashed on its own, we want what was written right
        // before the exit. Skip when no fresh writes happened during this
        // validation window: the on-disk log is from a previous run and is
        // actively misleading (the May 2026 incident showed 30 lines from a
        // healthy proxy 16h before the failure).
        let log_tail = if new_proxy_log_written {
            crate::tool_manager::newest_proxy_log_path(&self.tool_manager.logs_dir())
                .map(|path| crate::tool_manager::tail_log_file(&path, 30))
                .filter(|s| !s.is_empty())
        } else {
            None
        };

        self.stop_headroom();
        let rollback_result = if needs_commit_or_rollback {
            self.tool_manager.rollback_headroom_upgrade()
        } else {
            Ok(())
        };
        let rollback_restored = needs_commit_or_rollback && rollback_result.is_ok();
        if let Err(err) = rollback_result {
            log::error!("run_upgrade_with_ui: rollback failed: {err:#}");
        }
        analytics::set_headroom_ai_version(app, self.tool_manager.installed_headroom_version());
        let restarted = self.ensure_headroom_running().is_ok();

        let err_msg = match log_tail.as_deref() {
            Some(tail) => format!(
                "Headroom maintenance for app {} failed boot validation ({}, ran {}ms; internal headroom-ai target: {}, fallback: {:?}).\n\n--- last proxy log lines ---\n{}",
                current_app_version,
                outcome_label,
                duration_ms,
                target_version,
                installed_version,
                tail
            ),
            None => format!(
                "Headroom maintenance for app {} failed boot validation ({}, ran {}ms; internal headroom-ai target: {}, fallback: {:?}).\n\n(no new proxy log lines written during validation window)",
                current_app_version,
                outcome_label,
                duration_ms,
                target_version,
                installed_version
            ),
        };
        // Info-level: capture_upgrade_failure below fires a fully-tagged
        // Level::Error Sentry event with target/fallback versions, log tail,
        // boot diagnostics, and pip output. A warn! here would just produce
        // a duplicate, less informative event.
        log::info!("run_upgrade_with_ui: {err_msg}");
        let err = anyhow::anyhow!("{}", err_msg);
        // The rollback restores the bundled headroom-ai Python package, not the
        // desktop app itself — so user-facing rollback strings reference the
        // Python target/fallback versions (e.g. 0.20.15 → 0.19.0) rather than
        // the desktop app version (which never reverts).
        let fallback_pkg_label = installed_version
            .clone()
            .unwrap_or_else(|| "the previous version".into());
        // When the fallback did not come back either, the new venv's spawn
        // error is the best explanation we have of why: a host that refuses
        // every runtime (RUST-2Z's regression was a WinError 10013 socket
        // verdict that failed 0.35.0 exactly as it failed 0.37.0) otherwise
        // reads "Reverted to headroom-ai 0.35.0." under a banner while
        // nothing is running.
        let startup_hint = if restarted {
            None
        } else {
            post_spawn
                .ensure_error
                .as_deref()
                .and_then(classify_startup_error)
        };
        let error_hint = boot_validation_error_hint(
            maintenance_kind,
            rollback_restored,
            restarted,
            &fallback_pkg_label,
            startup_hint.as_deref(),
        );
        self.record_upgrade_failure(RuntimeUpgradeFailure {
            app_version: current_app_version.clone(),
            target_headroom_version: target_version.clone(),
            fallback_headroom_version: installed_version.clone(),
            failure_phase: if maintenance_kind == RuntimeMaintenanceKind::Upgrade {
                UpgradeFailurePhase::BootValidation
            } else {
                UpgradeFailurePhase::Install
            },
            attempts: 0,
            first_attempt_at: Utc::now(),
            last_attempt_at: Utc::now(),
            error_message: err_msg.clone(),
            error_hint,
            rollback_restored: rollback_restored || restarted,
        });
        crate::capture_upgrade_failure(
            &err,
            rollback_restored || restarted,
            if maintenance_kind == RuntimeMaintenanceKind::Upgrade {
                "boot_validation"
            } else {
                "requirements_repair_boot_validation"
            },
            Some(outcome_label),
            Some(duration_ms),
            Some(target_version.as_str()),
            installed_version.as_deref(),
            log_tail.as_deref(),
            Some(boot_diagnostics),
        );
        analytics::track_event(
            app,
            "runtime_upgrade_failed",
            Some(serde_json::json!({
                "phase": "boot_validation",
                "maintenance_kind": match maintenance_kind {
                    RuntimeMaintenanceKind::Upgrade => "upgrade",
                    RuntimeMaintenanceKind::RequirementsRepair => "requirements_repair",
                },
                "attempt": self.upgrade_failure_attempts(&current_app_version),
                "app_version": current_app_version,
                "restored": rollback_restored,
                "restarted": restarted,
                "duration_ms": duration_ms,
            })),
        );
        // Reuse the headroom-ai labels constructed above for the error_hint —
        // same rationale: rollback is about the Python package, not the app.
        let target_pkg_label = target_version.clone();
        self.set_upgrade_progress(|p| {
            p.running = false;
            p.complete = false;
            p.failed = true;
            p.current_step = "Update didn't start".into();
            p.message = match maintenance_kind {
                RuntimeMaintenanceKind::Upgrade if rollback_restored && restarted => {
                    format!(
                        "headroom-ai {} installed but didn't start. Reverted to headroom-ai {} and restarted it.",
                        target_pkg_label, fallback_pkg_label
                    )
                }
                RuntimeMaintenanceKind::Upgrade if rollback_restored => {
                    format!(
                        "headroom-ai {} installed but didn't start. Reverted to headroom-ai {}.",
                        target_pkg_label, fallback_pkg_label
                    )
                }
                RuntimeMaintenanceKind::Upgrade => format!(
                    "headroom-ai {} installed but didn't start, and rollback failed. Reinstall from the Dashboard.",
                    target_pkg_label
                ),
                RuntimeMaintenanceKind::RequirementsRepair if restarted => {
                    "Headroom runtime repair finished, but startup validation still failed after restart.".into()
                }
                RuntimeMaintenanceKind::RequirementsRepair => {
                    "Headroom runtime repair finished, but startup validation failed. Reinstall from the Dashboard.".into()
                }
            };
            p.overall_percent = 100;
        });
        emit_runtime_upgrade_progress(app, self);
    }

    /// User-initiated retry of a previously-failed runtime upgrade. Resets
    /// the attempts counter so `should_run_runtime_upgrade` lets it through,
    /// then invokes `run_upgrade_with_ui` directly.
    ///
    /// `force_rebuild` is the "Retry with full rebuild" path — skips the
    /// in-place attempt and runs atomic rebuild from scratch. Use when the
    /// previous attempt installed cleanly but the proxy never booted (the
    /// ABI-mismatch failure mode).
    pub fn retry_runtime_upgrade(&self, app: &tauri::AppHandle, force_rebuild: bool) {
        {
            let mut profile = self.launch_profile.lock();
            if let Some(failure) = profile.last_runtime_upgrade_failure.as_mut() {
                failure.attempts = 0;
            }
            persist_launch_profile(&self.launch_profile_path, &profile);
        }
        self.run_upgrade_with_ui(app, force_rebuild);
    }

    pub fn runtime_upgrade_in_progress(&self) -> bool {
        *self.runtime_upgrade_in_progress.lock()
    }

    /// Returns true if the tracked Headroom process has DEFINITIVELY exited.
    ///
    /// Only reports exited on `Ok(Some(status))` — i.e., the OS told us the
    /// child reaped — or on a natural death recorded by an earlier reap
    /// (`last_child_natural_exit`; the runtime_status pollers race this call
    /// and clear the handle first). A bare `None` handle with no recorded
    /// death is NOT treated as exited, because `ensure_headroom_running`
    /// intentionally skips spawning when the intercept layer already reports
    /// the proxy reachable; in that case there's a live proxy we just don't
    /// own the Child handle for. `Err` (child was reaped by someone else) is
    /// also not treated as exited — the OS-level process may well still be
    /// serving traffic.
    pub(crate) fn headroom_process_exited(&self) -> Option<String> {
        let mut guard = self.headroom_process.lock();
        match guard.as_mut() {
            None => self.last_child_natural_exit.lock().clone(),
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    let status = format!("{status}");
                    *self.last_child_natural_exit.lock() = Some(status.clone());
                    Some(status)
                }
                Ok(None) => None,
                Err(err) => {
                    log::warn!(
                        "headroom_process_exited: try_wait returned Err (treating as still alive): {err}"
                    );
                    None
                }
            },
        }
    }

    /// True iff we own a tracked proxy child that has not yet exited.
    /// Distinguishes "alive (possibly still cold-booting)" from both
    /// "exited/crashed" and "no tracked child at all". `headroom_process_exited`
    /// collapses the latter two into `None`, but the watchdog needs to tell
    /// them apart: an unreachable backend whose tracked child is still alive is
    /// a download-in-progress worth waiting on, whereas a missing or exited
    /// child is a genuine failure to auto-pause immediately.
    pub(crate) fn tracked_child_alive(&self) -> bool {
        let mut guard = self.headroom_process.lock();
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Adaptive boot validation loop. Probes `/livez` on the backend port
    /// (default 6768; may be a fallback in 6769..=6790) until the proxy
    /// responds, the proxy process exits, the log goes silent past the
    /// stall threshold, or `RUNTIME_UPGRADE_BOOT_MAX_SECS` elapses. On each
    /// pass through the loop, emits a progress update via `on_progress`.
    ///
    /// "Activity" is the union of four signals: (1) a write to any
    /// ``headroom-proxy*.log`` file, (2) growth in the HuggingFace hub
    /// cache, (3) a successful TCP connect to the backend loopback port,
    /// and (4) advancement of the tracked child's accumulated CPU time.
    /// Any one resets the silence timer. The HF signal is what keeps
    /// slow-but-progressing first-run downloads from being killed —
    /// when transformers/huggingface_hub is silently pulling multi-GB
    /// model weights, the python process writes nothing to its log,
    /// but the cache directory grows monotonically. The TCP signal
    /// covers the case where the proxy is alive and bound but its
    /// asyncio event loop is held by an in-flight forwarded request
    /// (e.g. a `POST /v1/messages` retrying against a 429-ing
    /// upstream) — the kernel still completes ``accept()`` even when
    /// uvicorn isn't draining the socket, so a successful connect
    /// proves the python process is alive even though no HTTP
    /// endpoint answers. The CPU-time signal covers a fourth case
    /// that all three above miss: lifespan-phase work that's neither
    /// writing logs nor downloading models nor yet bound to the port,
    /// e.g. ONNX graph compilation or eager-loading already-cached
    /// models. As long as the python process is burning CPU, it's
    /// not deadlocked.
    pub(crate) fn wait_for_boot_validation<F>(&self, mut on_progress: F) -> BootValidationOutcome
    where
        F: FnMut(std::time::Duration, bool),
    {
        use std::time::{Duration, Instant};

        // 5s is generous: /livez is a cheap endpoint, but the proxy event
        // loop can be held by the GIL while the pipeline chews through a
        // large Claude request (tokenization, ONNX inference, etc). The
        // previous 1.5s timeout false-fired during those bursts.
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return BootValidationOutcome::TimedOut,
        };

        let logs_dir = self.tool_manager.logs_dir();
        let hf_cache = hf_hub_cache_dir();
        // Cap the walk so a warm cache (post-install: ~3-5 GB across tens
        // of thousands of files) doesn't dominate the 500ms loop tick.
        // 50k entries is well above any healthy first-run install.
        const HF_CACHE_WALK_CAP: usize = 50_000;

        // Capture the tracked PID once at loop entry. If `headroom_process`
        // is None now (e.g. ensure_headroom_running short-circuited or the
        // spawn errored), it stays None for the duration — capturing once
        // avoids re-acquiring the lock every 500ms.
        let tracked_pid: Option<u32> = self.headroom_process.lock().as_ref().map(|c| c.id());

        let start = Instant::now();
        let mut last_log_activity = start;
        let mut last_seen_mtime = newest_proxy_log_mtime(&logs_dir);
        let mut last_hf_size = hf_cache
            .as_deref()
            .map(|p| total_dir_size_bytes(p, HF_CACHE_WALK_CAP));
        // Last time the HF cache actually grew — the download-in-progress
        // signal that lifts the soft timeout to the hard ceiling.
        let mut last_hf_growth_at: Option<Instant> = None;
        let mut last_cpu_secs: Option<u64> = tracked_pid.and_then(tracked_process_cpu_time_secs);
        let mut last_progress = Instant::now()
            .checked_sub(Duration::from_secs(5))
            .unwrap_or_else(Instant::now);

        let mut foreign_checked = false;
        let max = Duration::from_secs(RUNTIME_UPGRADE_BOOT_MAX_SECS);
        let hard_max = Duration::from_secs(RUNTIME_UPGRADE_BOOT_HARD_MAX_SECS);
        let grace = Duration::from_secs(RUNTIME_UPGRADE_STALL_GRACE_SECS);
        let silence = Duration::from_secs(RUNTIME_UPGRADE_STALL_SILENCE_SECS);
        let progress_interval = Duration::from_secs(2);

        loop {
            if probe_proxy_livez(&client) {
                return BootValidationOutcome::Reachable;
            }

            if let Some(exit_status) = self.headroom_process_exited() {
                log::warn!(
                    "wait_for_boot_validation: tracked proxy child exited with status {exit_status}"
                );
                return BootValidationOutcome::ProcessExited;
            }

            // A download is "active" if the HF cache grew within the silence
            // window. `last_hf_growth_at` reflects the previous tick's HF
            // observation (refreshed ~500ms below); the silence tolerance
            // absorbs that staleness.
            let download_active = last_hf_growth_at.is_some_and(|at| at.elapsed() < silence);
            let elapsed = start.elapsed();
            if boot_validation_timed_out(elapsed, max, hard_max, download_active) {
                return BootValidationOutcome::TimedOut;
            }

            // Refresh log activity observation.
            let current_mtime = newest_proxy_log_mtime(&logs_dir);
            if log_mtime_advanced(last_seen_mtime, current_mtime) {
                last_seen_mtime = current_mtime;
                last_log_activity = Instant::now();
            }

            // Refresh HF cache observation. Growth in this tree means the
            // proxy is downloading model weights — the most common
            // not-actually-stuck cause of log silence on first-run installs.
            if let Some(cache_path) = hf_cache.as_deref() {
                let current_size = total_dir_size_bytes(cache_path, HF_CACHE_WALK_CAP);
                if hf_cache_grew(last_hf_size, current_size) {
                    last_log_activity = Instant::now();
                    last_hf_growth_at = Some(Instant::now());
                }
                last_hf_size = Some(current_size);
            }

            // Refresh TCP-bound observation. If the kernel accepts a
            // connection on :6768, the python process is alive and
            // listening — even if the asyncio loop is currently held
            // by an in-flight forwarded request and no HTTP endpoint
            // answers. This is the load-bearing signal that keeps a
            // busy-but-alive proxy from being killed as "stalled".
            let port_bound = proxy_port_accepts_connection();
            if port_bound {
                last_log_activity = Instant::now();
            }

            // A bound port that never answers /livez may be a foreign
            // squatter whose accept() keeps the activity signal green for
            // the whole boot budget (RUST-4A: 625s against a port the child
            // could never bind). Identify the occupant once, after the grace
            // window; unknown/unowned shapes keep waiting, since that is the
            // updater-relaunch race the settle logic absorbs (RUST-7F).
            if port_bound && !foreign_checked && elapsed > grace {
                foreign_checked = true;
                if let Some(detail) = crate::tool_manager::proxy_port_held_by_named_foreign(
                    crate::backend_port::get(),
                ) {
                    log::warn!(
                        "wait_for_boot_validation: backend port held by foreign process ({detail}); failing fast"
                    );
                    return BootValidationOutcome::ForeignPortOccupant;
                }
            }

            // Refresh CPU-time observation. Catches lifespan-phase work
            // that's invisible to the three signals above — e.g. ONNX
            // graph compile or eager-loading pre-cached models, which
            // can sit silent for >90s while the python process is hot
            // on a CPU. Only fires for the tracked child; if we don't
            // own a Child handle (rare — ensure_headroom_running
            // short-circuited or errored), this signal is unavailable
            // and we lean on the other three.
            let mut cpu_advanced = false;
            if let Some(pid) = tracked_pid {
                let current_cpu_secs = tracked_process_cpu_time_secs(pid);
                if cpu_time_advanced(last_cpu_secs, current_cpu_secs) {
                    last_log_activity = Instant::now();
                    cpu_advanced = true;
                }
                last_cpu_secs = current_cpu_secs;
            }

            let activity_age = last_log_activity.elapsed();
            let has_recent_activity = activity_age < silence
                && (current_mtime.is_some()
                    || last_hf_size.unwrap_or(0) > 0
                    || port_bound
                    || cpu_advanced);

            // Past grace period and nothing has moved in either signal
            // for the silence window → treat as stalled.
            if boot_validation_stalled(elapsed, activity_age, grace, silence) {
                return BootValidationOutcome::Stalled;
            }

            if last_progress.elapsed() >= progress_interval {
                on_progress(elapsed, has_recent_activity);
                last_progress = Instant::now();
            }

            std::thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn runtime_upgrade_progress(&self) -> RuntimeUpgradeProgress {
        self.runtime_upgrade_progress.lock().clone()
    }

    pub fn runtime_upgrade_failure(&self) -> Option<RuntimeUpgradeFailure> {
        self.launch_profile
            .lock()
            .last_runtime_upgrade_failure
            .clone()
    }

    fn set_upgrade_progress<F>(&self, mutate: F)
    where
        F: FnOnce(&mut RuntimeUpgradeProgress),
    {
        let mut p = self.runtime_upgrade_progress.lock();
        mutate(&mut p);
    }

    fn stamp_app_version(&self, version: &str) {
        let mut profile = self.launch_profile.lock();
        profile.last_launched_app_version = Some(version.to_string());
        persist_launch_profile(&self.launch_profile_path, &profile);
    }

    /// True when the launch-profile stamp can be safely advanced to
    /// `current_app_version` from `warm_runtime_on_launch` even though no
    /// runtime maintenance ran.
    ///
    /// Refuses to stamp when:
    /// - the stamp already matches (no work; avoids a redundant disk write), or
    /// - there's an unresolved upgrade failure for this exact app version
    ///   (stamping would mask the failure record the retry banner relies on).
    fn can_stamp_no_maintenance(&self, current_app_version: &str) -> bool {
        let profile = self.launch_profile.lock();
        if profile.last_launched_app_version.as_deref() == Some(current_app_version) {
            return false;
        }
        if let Some(failure) = profile.last_runtime_upgrade_failure.as_ref() {
            if failure.app_version == current_app_version {
                return false;
            }
        }
        true
    }

    fn clear_upgrade_failure(&self) {
        let mut profile = self.launch_profile.lock();
        profile.last_runtime_upgrade_failure = None;
        persist_launch_profile(&self.launch_profile_path, &profile);
    }

    pub fn dismiss_upgrade_failure(&self) {
        self.clear_upgrade_failure();
        self.invalidate_runtime_status_cache();
    }

    fn record_upgrade_failure(&self, mut failure: RuntimeUpgradeFailure) {
        let mut profile = self.launch_profile.lock();
        let attempts = match profile.last_runtime_upgrade_failure.as_ref() {
            Some(prev) if prev.app_version == failure.app_version => {
                prev.attempts.saturating_add(1)
            }
            _ => 1,
        };
        failure.attempts = attempts;
        if let Some(prev) = profile.last_runtime_upgrade_failure.as_ref() {
            if prev.app_version == failure.app_version {
                failure.first_attempt_at = prev.first_attempt_at;
            }
        }
        profile.last_runtime_upgrade_failure = Some(failure);
        persist_launch_profile(&self.launch_profile_path, &profile);
    }

    fn upgrade_failure_attempts(&self, app_version: &str) -> u32 {
        self.launch_profile
            .lock()
            .last_runtime_upgrade_failure
            .as_ref()
            .filter(|f| f.app_version == app_version)
            .map(|f| f.attempts)
            .unwrap_or(0)
    }

    pub fn launch_count(&self) -> u64 {
        self.launch_profile.lock().launch_count
    }

    pub fn launch_experience_label(&self) -> &'static str {
        if !self.setup_wizard_satisfied() {
            return "first_run";
        }
        match self.launch_profile.lock().launch_experience {
            LaunchExperience::FirstRun => "first_run",
            LaunchExperience::Resume => "resume",
            LaunchExperience::Dashboard => "dashboard",
        }
    }

    pub fn setup_wizard_complete(&self) -> bool {
        self.launch_profile.lock().setup_wizard_complete
    }

    pub fn setup_wizard_satisfied(&self) -> bool {
        let profile = self.launch_profile.lock().clone();
        setup_wizard_satisfied_for_profile(&profile, configured_client_present())
    }

    pub fn mark_setup_wizard_complete(&self) {
        let mut profile = self.launch_profile.lock();
        if profile.setup_wizard_complete {
            return;
        }
        profile.setup_wizard_complete = true;
        persist_launch_profile(&self.launch_profile_path, &profile);
    }

    /// One-shot gate for the "setup finished but no traffic ever" recovery
    /// notification. True at most once per install. Flips and persists the
    /// flag on the call that returns true.
    pub fn try_mark_onboarding_recovery_notified(&self) -> bool {
        let mut profile = self.launch_profile.lock();
        if !onboarding_recovery_nudge_due(&profile) {
            return false;
        }
        profile.onboarding_recovery_notified = true;
        persist_launch_profile(&self.launch_profile_path, &profile);
        true
    }

    /// One-shot gate for the evidence-based "coding around Headroom" nudge.
    /// Unlike the generic recovery nudge it does not require a return launch:
    /// the evidence (Claude sessions grew during THIS run while the proxy
    /// forwarded nothing) is exactly as strong on the install-day launch.
    /// Firing also consumes the generic nudge's flag — both say "restart your
    /// terminal", and a second notification with the same advice is a nag,
    /// not a reminder. The reverse does NOT hold: the generic nudge having
    /// fired earlier does not block this one, because the evidence copy names
    /// what actually happened and is worth one more attempt.
    pub fn try_mark_unrouted_usage_notified(&self) -> bool {
        let mut profile = self.launch_profile.lock();
        if !unrouted_usage_nudge_due(&profile) {
            return false;
        }
        profile.unrouted_usage_notified = true;
        profile.onboarding_recovery_notified = true;
        persist_launch_profile(&self.launch_profile_path, &profile);
        true
    }

    /// One-shot gate for the first-savings celebration notification: returns
    /// true exactly once per install, persisted like the recovery nudge flag.
    pub fn try_mark_first_savings_notified(&self) -> bool {
        let mut profile = self.launch_profile.lock();
        if profile.first_savings_notified {
            return false;
        }
        profile.first_savings_notified = true;
        persist_launch_profile(&self.launch_profile_path, &profile);
        true
    }

    pub fn accepted_terms_version(&self) -> u32 {
        self.launch_profile.lock().accepted_terms_version
    }

    pub fn mark_terms_accepted(&self, version: u32) {
        let mut profile = self.launch_profile.lock();
        if profile.accepted_terms_version >= version {
            return;
        }
        profile.accepted_terms_version = version;
        persist_launch_profile(&self.launch_profile_path, &profile);
    }

    pub fn upstream_override(&self) -> UpstreamOverride {
        self.launch_profile.lock().upstream_override.clone()
    }

    /// Persist the override and publish it for the next proxy spawn. The token
    /// is handled separately (keychain); this only records whether one exists.
    pub fn set_upstream_override(&self, next: UpstreamOverride) {
        {
            let mut profile = self.launch_profile.lock();
            if profile.upstream_override == next {
                return;
            }
            profile.upstream_override = next.clone();
            persist_launch_profile(&self.launch_profile_path, &profile);
        }
        crate::upstream_override::publish(next);
    }

    pub fn cached_clients(&self) -> Vec<ClientStatus> {
        const TTL: Duration = Duration::from_secs(8);
        let mut cache = self.cached_clients.lock();
        if let Some((ref clients, at)) = *cache {
            if at.elapsed() < TTL {
                return clients.clone();
            }
        }
        let clients = detect_clients();
        *cache = Some((clients.clone(), Instant::now()));
        clients
    }

    pub fn cached_memory_export(&self) -> Option<String> {
        // Long TTL is safe because:
        //   - live-learning deletion explicitly calls `invalidate_memory_export_cache`
        //   - the activity observer background thread keeps the cache warm on an
        //     independent cadence, so cache misses rarely land on the IPC path
        const TTL: Duration = Duration::from_secs(60);
        let cache = self.cached_memory_export.lock();
        if let Some((ref s, at)) = *cache {
            if at.elapsed() < TTL {
                return Some(s.clone());
            }
        }
        None
    }

    pub fn store_memory_export(&self, stdout: String) {
        *self.cached_memory_export.lock() = Some((stdout, Instant::now()));
    }

    pub fn invalidate_memory_export_cache(&self) {
        *self.cached_memory_export.lock() = None;
    }

    /// Returns the captured Claude bearer token if it is still within its TTL.
    /// Returns `None` if no token has been captured or the last capture is
    /// stale — in either case the caller should prompt the user to send a
    /// fresh request through the proxy.
    pub fn current_bearer_token(&self) -> Option<String> {
        self.claude_bearer_token
            .lock()
            .as_ref()
            .and_then(|token| token.value_if_fresh(BEARER_TOKEN_TTL).map(str::to_string))
    }

    pub fn cached_claude_profile(&self) -> ClaudeAccountProfile {
        const TTL: Duration = Duration::from_secs(300);
        // How long a run of transient profile-fetch failures may suppress the
        // banner before we surface it. Comfortably longer than a normal token
        // rotation gap, short enough that a genuinely expired/revoked token
        // still gets the user's attention.
        const STALE_PROFILE_ESCALATE_AFTER: Duration = Duration::from_secs(15 * 60);

        let current_token = self.current_bearer_token();

        {
            let cache = self.cached_claude_profile.lock();
            if let Some((cached_token, profile, at)) = &*cache {
                if *cached_token == current_token && at.elapsed() < TTL {
                    return profile.clone();
                }
            }
        }

        let detection = pricing::detect_claude_profile_uncached(self);
        let profile = detection.profile;
        if pricing::is_identity_complete(&profile) {
            self.record_complete_identity_fetch();
            *self.stale_profile_since.lock() = None;
        }

        // During a token-rotation gap the captured bearer is briefly stale and
        // Anthropic rejects the profile fetch (401/403, or a 5xx/network blip).
        // Rather than flashing an alarming "sign out" banner, keep serving the
        // last identity-complete profile until a fresh bearer flows through and
        // the next fetch succeeds. We re-key it to the current token so repeated
        // UI polls within this gap don't re-hit Anthropic with the stale token.
        //
        // If the failures persist past STALE_PROFILE_ESCALATE_AFTER the gap is
        // no longer a momentary rotation blip, so we stop suppressing and let
        // the real error (and its banner) through.
        if detection.error_is_transient && !pricing::is_identity_complete(&profile) {
            let escalate = {
                let mut since = self.stale_profile_since.lock();
                let started = since.get_or_insert_with(Instant::now);
                started.elapsed() >= STALE_PROFILE_ESCALATE_AFTER
            };
            if !escalate {
                let mut cache = self.cached_claude_profile.lock();
                if let Some((_, prev, _)) = cache.as_ref() {
                    if pricing::is_identity_complete(prev) {
                        let good = prev.clone();
                        *cache = Some((current_token, good.clone(), Instant::now()));
                        return good;
                    }
                }
            }
        }

        let mut cache = self.cached_claude_profile.lock();
        *cache = Some((current_token, profile.clone(), Instant::now()));
        profile
    }

    /// True iff a `desktop/grace/start` post with this exact set of Claude
    /// fields has already been recorded as successful in this session.
    /// Identity-pusher worker uses this to skip repeat posts when the bearer
    /// rotates but the underlying account/plan has not changed.
    pub fn identity_fingerprint_already_pushed(
        &self,
        fp: &crate::pricing::IdentityFingerprint,
    ) -> bool {
        self.last_pushed_identity_fingerprint
            .lock()
            .as_ref()
            .map(|prev| prev == fp)
            .unwrap_or(false)
    }

    /// Mark the given fingerprint as the most recent one we've pushed to
    /// `desktop/grace/start`. Called by the worker after a successful post,
    /// and by the sign-in / activation paths that send the same payload.
    pub fn record_pushed_identity_fingerprint(&self, fp: crate::pricing::IdentityFingerprint) {
        *self.last_pushed_identity_fingerprint.lock() = Some(fp);
    }

    /// True iff a fresh OAuth profile fetch returned a *complete* identity
    /// (UUID + email + non-Unknown plan tier) within `max_age`. The
    /// identity-pusher worker uses this to throttle further OAuth calls.
    pub fn complete_identity_fetched_within(&self, max_age: Duration) -> bool {
        self.last_complete_identity_fetch_at
            .lock()
            .as_ref()
            .map(|at| at.elapsed() < max_age)
            .unwrap_or(false)
    }

    /// Record that we just successfully fetched a complete OAuth identity.
    /// Called from `cached_claude_profile()` whenever a fresh fetch returns
    /// a fully populated profile, so every code path that re-warms the
    /// profile cache contributes to the throttle window.
    fn record_complete_identity_fetch(&self) {
        *self.last_complete_identity_fetch_at.lock() = Some(Instant::now());
    }

    /// The most recent classifier output that was something other than
    /// `Unknown`. Used by the pricing gate to keep applying real thresholds
    /// when a transient OAuth-profile fetch returns sparse fields and the
    /// live classifier returns Unknown.
    pub fn last_known_good_plan_tier(&self) -> Option<crate::models::ClaudePlanTier> {
        self.last_known_good_plan
            .lock()
            .as_ref()
            .map(|p| p.plan_tier.clone())
    }

    /// Persist a classifier result if it carries real signal. Unknown is
    /// silently ignored — it's "we don't know yet", never an authoritative
    /// downgrade.
    pub fn record_known_good_plan_tier(&self, tier: &crate::models::ClaudePlanTier) {
        if matches!(tier, crate::models::ClaudePlanTier::Unknown) {
            return;
        }
        let entry = LastKnownGoodPlan {
            plan_tier: tier.clone(),
            recorded_at: Utc::now(),
        };
        {
            let mut cache = self.last_known_good_plan.lock();
            if let Some(existing) = cache.as_ref() {
                // Same tier as before — skip the disk write to avoid touching
                // the file on every classification refresh.
                if matches!(
                    (&existing.plan_tier, tier),
                    (
                        crate::models::ClaudePlanTier::Free,
                        crate::models::ClaudePlanTier::Free
                    ) | (
                        crate::models::ClaudePlanTier::Pro,
                        crate::models::ClaudePlanTier::Pro
                    ) | (
                        crate::models::ClaudePlanTier::Max5x,
                        crate::models::ClaudePlanTier::Max5x
                    ) | (
                        crate::models::ClaudePlanTier::Max20x,
                        crate::models::ClaudePlanTier::Max20x
                    ) | (
                        crate::models::ClaudePlanTier::Api,
                        crate::models::ClaudePlanTier::Api
                    )
                ) {
                    return;
                }
            }
            *cache = Some(entry.clone());
        }
        persist_last_known_good_plan(&self.last_known_good_plan_path, &entry);
    }

    /// How long a last-good `/stats` payload may stand in for a failed fetch.
    ///
    /// The layers only `/stats` reports (output shaping, tool schema) describe
    /// configuration, which does not move between polls, so serving the
    /// previous answer through a transient timeout is honest -- and blanking
    /// them is the entire harm the warning names ("dashboard loses the
    /// layers"). Bounded so a backend that is starved for good eventually
    /// shows absent layers rather than an ever-staler snapshot presented as
    /// live.
    const HEADROOM_STATS_RETAIN_LAST_GOOD: Duration = Duration::from_secs(10 * 60);

    fn cached_headroom_stats(&self) -> Option<HeadroomDashboardStats> {
        match self.polled_headroom_stats() {
            Some(stats) => {
                *self.last_good_headroom_stats.lock() = Some((stats.clone(), Instant::now()));
                Some(stats)
            }
            // Retain the previous good payload rather than blanking the
            // dashboard on one timeout. The stamp is the age of the DATA, not
            // of the failed fetch, so the window counts from the last real
            // answer and repeated failures cannot extend it.
            None => self
                .last_good_headroom_stats
                .lock()
                .as_ref()
                .filter(|(_, at)| at.elapsed() < Self::HEADROOM_STATS_RETAIN_LAST_GOOD)
                .map(|(stats, _)| stats.clone()),
        }
    }

    /// The raw poll behind [`Self::cached_headroom_stats`]: cache lookup, then
    /// a live fetch on miss. Returns `None` for "this poll had no answer",
    /// which the caller may still cover with a retained payload.
    fn polled_headroom_stats(&self) -> Option<HeadroomDashboardStats> {
        // Dashboard polls at 5s; a 4s TTL caused every poll to miss and
        // re-fetch from the proxy. 12s gives at least one cache hit between
        // dashboard refreshes while keeping session savings visibly fresh.
        const TTL: Duration = Duration::from_secs(12);
        // A failure is held far longer than a success, which is the opposite of
        // `cached_headroom_history` and deliberate: the dominant failure here
        // is a `/stats` rebuild that outruns its 15s timeout on a backend busy
        // serving a session. Re-probing that every 12s keeps a 15s blocking
        // request in flight essentially all the time, so the poll itself
        // becomes part of the starvation it is reporting -- RUST-86 shipped
        // 1601 events that way. At 60s the probe still recovers within a few
        // seconds of the backend freeing up, at a fifth of the load, and the
        // retained payload above covers the gap.
        const MISS_TTL: Duration = Duration::from_secs(60);
        {
            let cache = self.cached_headroom_stats.lock();
            if let Some((stats, at)) = cache.as_ref() {
                let ttl = if stats.is_some() { TTL } else { MISS_TTL };
                if at.elapsed() < ttl {
                    return stats.clone();
                }
            }
        }
        // Fetch with the guard dropped: holding it across the network call
        // (readyz probe + stats request, several seconds when the proxy is
        // down) serialized every concurrent dashboard builder behind one
        // stalled fetch. A rare duplicate fetch is cheaper than that.
        let stats = fetch_headroom_dashboard_stats();
        *self.cached_headroom_stats.lock() = Some((stats.clone(), Instant::now()));
        stats
    }

    fn cached_headroom_history(&self) -> Option<HeadroomSavingsHistoryResponse> {
        // Lifetime history moves slowly — the daily/hourly buckets that drive
        // the Home charts only change a handful of times per minute under
        // active traffic. A 30s TTL absorbs most dashboard polls while still
        // updating the chart's most-recent bucket within one full refresh.
        const TTL: Duration = Duration::from_secs(30);
        // A miss (backend not yet reachable on cold start, or a retained
        // last-good value while the proxy is paused) is cached briefly so the
        // chart resolves/recovers within a few seconds, instead of holding the
        // startup loading state or stale data for a full 30s.
        const MISS_TTL: Duration = Duration::from_secs(3);
        {
            let cache = self.cached_headroom_history.lock();
            if let Some((history, at, fresh)) = cache.as_ref() {
                let ttl = if *fresh { TTL } else { MISS_TTL };
                if at.elapsed() < ttl {
                    return history.clone();
                }
            }
        }
        // Guard dropped across the fetch — see cached_headroom_stats.
        match fetch_headroom_savings_history() {
            Some(history) => {
                *self.cached_headroom_history.lock() =
                    Some((Some(history.clone()), Instant::now(), true));
                Some(history)
            }
            None => {
                // Retain the last good history so a transient proxy pause
                // doesn't revert the Home chart to the sparse tracker-only
                // layer. Mark it stale so we re-probe on the short miss TTL and
                // recover quickly once the proxy returns.
                let mut cache = self.cached_headroom_history.lock();
                let retained = cache.as_ref().and_then(|(h, _, _)| h.clone());
                *cache = Some((retained.clone(), Instant::now(), false));
                retained
            }
        }
    }

    fn cached_rtk_gain_summary(&self) -> Option<RtkGainSummary> {
        const TTL: Duration = Duration::from_secs(10);
        let mut cache = self.cached_rtk_gain_summary.lock();
        if let Some((stats, at)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return stats.clone();
            }
        }
        let stats = self.tool_manager.rtk_gain_summary();
        *cache = Some((stats.clone(), Instant::now()));
        stats
    }

    fn cached_rtk_today_stats(&self) -> Option<crate::models::RtkTodayStats> {
        const TTL: Duration = Duration::from_secs(10);
        let mut cache = self.cached_rtk_today_stats.lock();
        if let Some((stats, at)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return stats.clone();
            }
        }
        let stats = self.tool_manager.rtk_today_stats();
        *cache = Some((stats.clone(), Instant::now()));
        stats
    }

    pub fn dashboard(&self) -> DashboardState {
        // Callers that take this read-only path (tray updater, bootstrap
        // finalize, account activation) must NOT drain pending milestones —
        // doing so silently consumes crossings before `get_dashboard_state`
        // can fire the aptabase event and the in-app notification.
        self.build_dashboard(false).0
    }

    /// Observe a batch of transformations into ActivityFacts (for feed
    /// synthetic-event detection: new-model / daily-record / all-time-record),
    /// persist any changes, and return the emitted synthetic events plus the
    /// current bounded history of recent synthetic events.
    pub fn observe_activity_from_transformations(
        &self,
        transformations: &[TransformationFeedEvent],
    ) -> ActivityObservation {
        let mut facts = self.activity_facts.lock();
        let mut fresh: Vec<ActivityEvent> = Vec::new();
        let mut ordered: Vec<&TransformationFeedEvent> = transformations.iter().collect();
        // Feed arrives newest-first; observe oldest-first so records update in order.
        ordered.sort_by(|a, b| {
            a.timestamp
                .clone()
                .unwrap_or_default()
                .cmp(&b.timestamp.clone().unwrap_or_default())
        });
        for transformation in ordered {
            let observed_at = transformation
                .timestamp
                .as_deref()
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            fresh.extend(facts.observe_transformation(transformation, observed_at));
        }

        let _ = facts.save_if_dirty();
        ActivityObservation { fresh }
    }

    pub fn observe_learnings_today(
        &self,
        patterns_today: u32,
        project_inputs: Vec<crate::activity_facts::LearningsProjectInput>,
        active_project_path: Option<&str>,
    ) -> crate::models::LearningsMilestoneEvent {
        let mut facts = self.activity_facts.lock();
        let event = facts.observe_learnings_today(
            patterns_today,
            project_inputs,
            active_project_path,
            Utc::now(),
        );
        let _ = facts.save_if_dirty();
        event
    }

    /// Scan the Claude Code project list for candidates that should be
    /// prompted to run Train. Delegates the decision logic and bookkeeping
    /// (fire-once for never-trained, 7-day cooldown for stale) to
    /// `ActivityFacts::observe_train_suggestions`.
    pub fn observe_train_suggestions(&self, projects: &[ClaudeCodeProject]) -> Vec<ActivityEvent> {
        let mut facts = self.activity_facts.lock();
        let events = facts.observe_train_suggestions(projects, Utc::now());
        let _ = facts.save_if_dirty();
        events
    }

    /// Read-only snapshot of the latest-of-kind slots. The `get_activity_feed`
    /// IPC command wraps this straight into the response; observation runs on
    /// a backend timer and is the sole writer.
    pub fn activity_feed_snapshot(&self) -> crate::models::ActivityFeedSnapshot {
        let mut snapshot = self.activity_facts.lock().activity_feed_snapshot();
        snapshot.rtk_today = self.cached_rtk_today_stats();
        // No state-level cache like rtk's: both serena sources already sit
        // behind their own 60s caches inside ToolManager.
        snapshot.serena_today = self.tool_manager.serena_today_stats();
        snapshot
    }

    /// Emit a weekly recap rolling up the 7 days ending last Sunday.
    /// Previously Monday-only; now runs on any day whose check is due so the
    /// first launch after an upgrade catches up on last week's recap if it
    /// was missed. Two gates: `weekly_recap_check_due` (once per 24h) and
    /// the per-week key inside `maybe_record_weekly_recap`.
    pub fn maybe_emit_weekly_recap(&self) -> Option<ActivityEvent> {
        let now = Utc::now();
        // Cheap pre-check — skip aggregation entirely if we've already
        // checked within 24h. The callee re-checks defensively.
        if !self.activity_facts.lock().weekly_recap_check_due(now) {
            return None;
        }

        let today = Local::now().date_naive();
        let recap_monday = most_recent_monday(today);
        let start = recap_monday.checked_sub_days(chrono::Days::new(7))?;
        let end = recap_monday.pred_opt()?;

        let totals = {
            let tracker = self.savings_tracker.lock();
            aggregate_weekly_totals(&tracker.daily_savings, start, end)
        };

        let mut facts = self.activity_facts.lock();
        let event = facts.maybe_record_weekly_recap(recap_monday, totals, now);
        let _ = facts.save_if_dirty();
        event
    }

    pub fn dashboard_with_pending_milestones(&self) -> (DashboardState, PendingMilestones) {
        self.build_dashboard(true)
    }

    fn build_dashboard(
        &self,
        drain_pending_milestones: bool,
    ) -> (DashboardState, PendingMilestones) {
        let tools = self.tool_manager.list_tools();
        let clients = self.cached_clients();
        let recent_usage = self.recent_usage.lock().clone();
        let insights = build_insights(
            &recent_usage,
            &clients,
            self.tool_manager.python_runtime_installed(),
        );
        let (mut snapshot, mut daily_savings, mut hourly_savings) = {
            let tracker = self.savings_tracker.lock();
            (
                tracker.snapshot(),
                tracker.daily_savings(),
                tracker.hourly_savings(),
            )
        };
        let mut pending_milestones = PendingMilestones::default();

        let stats = self.cached_headroom_stats();
        let history = self.cached_headroom_history();
        if history.is_some() {
            self.savings_history_loaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if let Some(stats) = stats.as_ref() {
            if let Some((updated, updated_daily, updated_hourly)) =
                self.record_savings_snapshot(stats)
            {
                snapshot = updated;
                daily_savings = updated_daily;
                hourly_savings = updated_hourly;
            }
        }

        if let Some(stats) = stats.as_ref() {
            if let Some(requests) = stats.session_requests {
                snapshot.session_requests = requests;
            }
            if let Some(saved_usd) = stats.session_estimated_savings_usd {
                snapshot.session_estimated_savings_usd = saved_usd;
            }
            if let Some(saved_tokens) = stats.session_estimated_tokens_saved {
                snapshot.session_estimated_tokens_saved = saved_tokens;
            }
            if let Some(savings_pct) = stats.session_savings_pct {
                snapshot.session_savings_pct = savings_pct;
            }
        }

        let mut savings_breakdown = history.as_ref().and_then(|h| h.lifetime.clone());

        // Recomputed from the shaper's own ledger rather than taken from
        // `/stats`, which credits strata the baseline never observed against a
        // global mean and flips to the A/B number on a single sample per arm.
        // See `output_savings`. The backend's figure is a fallback ONLY when
        // the ledger carries no evidence at all (missing, mid-write, or no
        // shaped traffic yet), so a torn read never blanks the tile. A
        // readable ledger that scores nothing shows nothing: falling back
        // there put the credited number on exactly the machines the recompute
        // refuses to score (all-codex traffic vs a claude-seeded baseline
        // read as "Output -100%" on Windows, 0.9.7-rc.7).
        let ledger_read = crate::output_savings::estimate();
        let backend_output_fallback_allowed = matches!(
            ledger_read,
            crate::output_savings::LedgerEstimate::NoEvidence
        );
        let ledger_estimate = ledger_read.scored();
        let output_reduction = ledger_estimate
            .as_ref()
            .map(|e| crate::models::OutputReduction {
                method: e.method.to_string(),
                reduction_percent: e.reduction_percent,
                ci_low_percent: e.ci_low_percent,
                ci_high_percent: e.ci_high_percent,
                requests: e.requests,
            })
            .or_else(|| {
                if !backend_output_fallback_allowed {
                    return None;
                }
                stats
                    .as_ref()
                    .and_then(|s| s.output_reduction.as_ref())
                    .map(|o| crate::models::OutputReduction {
                        method: o.method.clone(),
                        reduction_percent: o.reduction_percent,
                        ci_low_percent: o.ci_low_percent,
                        ci_high_percent: o.ci_high_percent,
                        requests: o.requests,
                    })
            });

        let learner_progress = stats.as_ref().and_then(|s| s.learner_progress.clone());

        if let Some(history) = history.as_ref() {
            let cutoff_date = savings_history_cutoff_date();
            let cutoff_hour = format!("{cutoff_date}T00:00");
            // Both drops target the same bucket -- the rollup's leading delta,
            // measured from a zero baseline -- from different evidence, so only
            // one may run. The parser's is exact (it can see the point cap was
            // hit); this one is the fallback for an untrimmed history whose
            // series still starts after the local tracker, e.g. a reset backend
            // data dir. Running both ate a real day, and with only two buckets
            // in the window it left nothing at all.
            let (native_daily, native_hourly) = if history.backfill_bucket_dropped {
                (history.daily_savings(), history.hourly_savings())
            } else {
                (
                    settle_rollup_backfill(
                        history.daily_savings(),
                        daily_savings.iter().map(|p| p.date.as_str()).min(),
                        history.ring_start.as_ref(),
                        |p| p.date.as_str(),
                    ),
                    settle_rollup_backfill(
                        history.hourly_savings(),
                        hourly_savings.iter().map(|p| p.hour.as_str()).min(),
                        history.ring_start.as_ref(),
                        |p| p.hour.as_str(),
                    ),
                )
            };

            // Lock the backend's authoritative settled rollups into the local
            // archive so they survive its history trimming and fill gaps from
            // periods the app wasn't running.
            {
                let today_key = local_day_key(Local::now());
                let utc_today_key = chrono::Utc::now().format("%Y-%m-%d").to_string();
                let mut tracker = self.savings_tracker.lock();
                if tracker.ingest_native_rollups(
                    &native_daily,
                    &native_hourly,
                    &cutoff_date,
                    &today_key,
                    &utc_today_key,
                ) {
                    let _ = tracker.persist_state();
                }
            }

            daily_savings = merge_daily_savings(daily_savings, native_daily, &cutoff_date);
            hourly_savings = merge_hourly_savings(hourly_savings, native_hourly, &cutoff_hour);
        }

        // Overlay the locally-sampled output series onto the merged points.
        // Neither merge source carries it: backend rollups have no baseline
        // dimension and tracker buckets predate the sampler. Daily joins on
        // UTC date keys, hourly on local hour keys — matching each list.
        //
        // Cache fields overlay from the archive too, and the archive wins:
        // the history points carry a fresh derivation from the backend's
        // sliding compacted checkpoint ring, which drifts poll to poll for
        // settled periods (a settled day's Input % visibly wandered on
        // 2026-08-18). Ingest ran just above, so settled buckets are frozen
        // and the live UTC day's archive equals this poll's derivation;
        // live local-day hours are never ingested, keep None here, and so
        // fall through to the fresh derivation.
        {
            let tracker = self.savings_tracker.lock();
            for point in daily_savings.iter_mut() {
                if let Some(sample) = tracker.output_daily_samples.get(&point.date) {
                    point.output_sampled_tokens_saved = Some(sample.saved_tokens);
                    point.output_baseline_tokens = Some(sample.baseline_tokens);
                }
                if let Some(bucket) = tracker.daily_savings.get(&point.date) {
                    point.cache_read_tokens = bucket.cache_read_tokens.or(point.cache_read_tokens);
                    point.cache_savings_usd = bucket.cache_savings_usd.or(point.cache_savings_usd);
                }
                if let Some(tokens) = tracker.tool_schema_daily_samples.get(&point.date) {
                    point.tool_schema_tokens_saved = *tokens;
                    point.tool_schema_savings_usd = bucket_tool_schema_usd(
                        point.estimated_savings_usd,
                        point.estimated_tokens_saved,
                        *tokens,
                    );
                }
            }
            for point in hourly_savings.iter_mut() {
                if let Some(sample) = tracker.output_hourly_samples.get(&point.hour) {
                    point.output_sampled_tokens_saved = Some(sample.saved_tokens);
                    point.output_baseline_tokens = Some(sample.baseline_tokens);
                }
                if let Some(bucket) = tracker.hourly_savings.get(&point.hour) {
                    point.cache_read_tokens = bucket.cache_read_tokens.or(point.cache_read_tokens);
                    point.cache_savings_usd = bucket.cache_savings_usd.or(point.cache_savings_usd);
                }
                if let Some(tokens) = tracker.tool_schema_hourly_samples.get(&point.hour) {
                    point.tool_schema_tokens_saved = *tokens;
                    point.tool_schema_savings_usd = bucket_tool_schema_usd(
                        point.estimated_savings_usd,
                        point.estimated_tokens_saved,
                        *tokens,
                    );
                }
            }
        }

        let (launch_experience, accepted_terms_version) = {
            let profile = self.launch_profile.lock();
            (
                profile.launch_experience.clone(),
                profile.accepted_terms_version,
            )
        };

        // Lifetime totals are derived from the same per-day buckets the history
        // chart renders, so the headline card and the chart can never disagree
        // on compression. Output shaping is the one exception -- see
        // `lifetime_output_savings_usd`.
        let lifetime_compression_savings_usd: f64 = daily_savings
            .iter()
            .map(|point| point.estimated_savings_usd)
            .sum();
        let (lifetime_tool_schema_tokens_saved, cached_output_estimator_tokens) = {
            let tracker = self.savings_tracker.lock();
            (
                tracker.lifetime_tool_schema_tokens_saved,
                tracker.last_output_estimator_tokens_saved,
            )
        };
        // Until the backend is reachable (cold start), price the output layer
        // off the last persisted estimator reading instead of the bucket sum,
        // so the headline doesn't dip by hundreds of dollars for the first
        // minutes and then jump back up.
        // Same ledger recomputation as the tile above: the dollar row and the
        // percentage have to describe one estimate, or the drill-down stops
        // explaining the headline.
        let lifetime_output_savings_usd = lifetime_output_savings_usd(
            &daily_savings,
            ledger_estimate
                .as_ref()
                .map(|e| e.tokens_saved)
                .or_else(|| {
                    // Same gate as the tile above: the backend's token total
                    // carries the global-mean credit, so it may only stand in
                    // when the ledger itself has nothing to say.
                    if !backend_output_fallback_allowed {
                        return None;
                    }
                    stats
                        .as_ref()
                        .and_then(|s| s.output_reduction.as_ref())
                        .map(|r| r.tokens_saved)
                })
                .or(cached_output_estimator_tokens),
        );
        let lifetime_tool_schema_savings_usd =
            tool_schema_savings_usd(&daily_savings, lifetime_tool_schema_tokens_saved);
        // All three Headroom layers. Deferral was excluded while the chart had
        // no per-bucket record of it -- the headline then claimed savings no
        // bar could show. The chart gained that segment on 2026-09-02, so the
        // headline, the chart and the drill-down now count the same layers,
        // and the drill-down rows genuinely sum to this figure. Expect a
        // one-time step on upgrade: the lifetime deferral counter reaches back
        // to 0.7.5. The token total and the reported daily rows stay
        // layer-unchanged (see the milestone comment below and
        // recent_savings_days in lib.rs).
        let lifetime_estimated_savings_usd = lifetime_compression_savings_usd
            + lifetime_output_savings_usd
            + lifetime_tool_schema_savings_usd;
        warn_once_if_savings_rate_implausible(&daily_savings, || {
            self.tool_manager.installed_headroom_version()
        });
        // Tokens stay input-only: the card is labelled "Total input tokens
        // saved", and this total also drives the milestone notifications, which
        // must not jump when a new savings layer starts reporting.
        let lifetime_estimated_tokens_saved: u64 = daily_savings
            .iter()
            .map(|point| point.estimated_tokens_saved)
            .sum();

        // The drill-down has to add up to the headline it explains, so the
        // Headroom rows come from those same buckets rather than the backend's
        // `lifetime` block. That block is stitched differently (it counts the
        // rollup's backfill bucket for a period the tracker also covers), and
        // its output figure is process-scoped besides: it restarts at zero on
        // every backend restart, reporting $0.72 against $105 of daily deltas
        // on 2026-08-06. Cache and spend rows below stay as reported -- they are
        // context, never summed into a Headroom total.
        if let Some(breakdown) = savings_breakdown.as_mut() {
            breakdown.compression_savings_usd = lifetime_compression_savings_usd;
            breakdown.output_savings_usd = lifetime_output_savings_usd;
            breakdown.tool_schema_savings_usd = lifetime_tool_schema_savings_usd;
            breakdown.tool_schema_tokens_saved = lifetime_tool_schema_tokens_saved;
        }

        // Token milestones fire off the displayed lifetime total via a persisted
        // high-water mark, so they can't double-fire when a day's bucket is
        // re-rolled downward by the backend.
        if drain_pending_milestones {
            let mut tracker = self.savings_tracker.lock();
            let crossed = tracker.note_lifetime_token_total(lifetime_estimated_tokens_saved);
            if !crossed.is_empty() {
                let _ = tracker.persist_state();
                pending_milestones.token.extend(crossed);
            }
        }

        // Cumulative-savings heartbeat: post the current lifetime total on a
        // throttled cadence (not just at 1M milestones) so the server's
        // cumulative_tokens_saved / last_active_at don't lag by millions of
        // tokens. First observation each session reports immediately.
        if drain_pending_milestones && lifetime_estimated_tokens_saved > 0 {
            let mut throttle = self.cumulative_report_throttle.lock();
            let due = match *throttle {
                Some((reported, at)) => {
                    lifetime_estimated_tokens_saved > reported
                        && at.elapsed() >= CUMULATIVE_REPORT_INTERVAL
                }
                None => true,
            };
            if due {
                *throttle = Some((lifetime_estimated_tokens_saved, Instant::now()));
                pending_milestones.cumulative_report = Some(lifetime_estimated_tokens_saved);
            }
        }

        (
            DashboardState {
                app_version: env!("CARGO_PKG_VERSION").into(),
                launch_experience,
                bootstrap_complete: self.tool_manager.python_runtime_installed(),
                python_runtime_installed: self.tool_manager.python_runtime_installed(),
                lifetime_requests: snapshot.lifetime_requests,
                first_prompt_request_seen: crate::proxy_intercept::first_prompt_request_seen(),
                lifetime_estimated_savings_usd,
                lifetime_estimated_tokens_saved,
                session_requests: snapshot.session_requests,
                session_estimated_savings_usd: snapshot.session_estimated_savings_usd,
                session_estimated_tokens_saved: snapshot.session_estimated_tokens_saved,
                session_savings_pct: snapshot.session_savings_pct,
                output_reduction,
                output_shaper_active: stats.as_ref().and_then(|s| s.output_shaper_active),
                learner_progress,
                reread_tokens: stats.as_ref().and_then(|s| s.reread_tokens),
                reread_compressed_tokens: stats.as_ref().and_then(|s| s.reread_compressed_tokens),
                ccr_retrievals: stats.as_ref().and_then(|s| s.ccr_retrievals),
                savings_breakdown,
                daily_savings,
                hourly_savings,
                savings_history_loaded: self
                    .savings_history_loaded
                    .load(std::sync::atomic::Ordering::Relaxed),
                tools,
                clients,
                recent_usage,
                insights,
                required_terms_version: REQUIRED_TERMS_VERSION,
                accepted_terms_version,
                terms_url: TERMS_URL.to_string(),
            },
            pending_milestones,
        )
    }

    /// Cache TTL for `list_claude_code_projects`. Long enough that rapid tab
    /// switches and pre-warms hit the cache instead of re-scanning the
    /// projects directory. A dedicated background thread
    /// (`spawn_claude_projects_warmer`) keeps this fresh at ~75s cadence so
    /// most Optimize opens still avoid a cold filesystem scan.
    /// Completed learn runs explicitly invalidate via
    /// `invalidate_claude_code_projects_cache`, so staleness isn't a concern
    /// for learn-driven UI updates.
    const CLAUDE_PROJECTS_CACHE_TTL: Duration = Duration::from_secs(90);

    pub fn list_claude_code_projects(&self) -> Result<Vec<ClaudeCodeProject>> {
        if let Some(cached) = self.cached_claude_code_projects_fresh() {
            return Ok(cached);
        }
        let projects = self.list_claude_code_projects_uncached()?;
        *self.cached_claude_code_projects.lock() = Some((projects.clone(), Instant::now()));
        Ok(projects)
    }

    fn cached_claude_code_projects_fresh(&self) -> Option<Vec<ClaudeCodeProject>> {
        let cache = self.cached_claude_code_projects.lock();
        if let Some((ref projects, at)) = *cache {
            if at.elapsed() < Self::CLAUDE_PROJECTS_CACHE_TTL {
                return Some(projects.clone());
            }
        }
        None
    }

    pub fn invalidate_claude_code_projects_cache(&self) {
        *self.cached_claude_code_projects.lock() = None;
    }

    pub fn headroom_learn_prereq_status(&self) -> HeadroomLearnPrereqStatus {
        if let Some(cached) = self.cached_headroom_learn_prereq.lock().clone() {
            return cached;
        }
        let status = crate::detect_headroom_learn_prereq_status();
        *self.cached_headroom_learn_prereq.lock() = Some(status.clone());
        status
    }

    pub fn invalidate_headroom_learn_prereq_cache(&self) {
        *self.cached_headroom_learn_prereq.lock() = None;
    }

    fn list_claude_code_projects_uncached(&self) -> Result<Vec<ClaudeCodeProject>> {
        let projects_dir = claude_projects_dir();
        if !projects_dir.exists() {
            return Ok(Vec::new());
        }

        let mut grouped_projects = BTreeMap::<String, ClaudeProjectScan>::new();
        let entries = std::fs::read_dir(&projects_dir)
            .with_context(|| format!("reading {}", projects_dir.display()))?;

        for entry in entries.filter_map(|item| item.ok()) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let folder_name = entry
                .file_name()
                .to_str()
                .map(|value| value.to_string())
                .unwrap_or_default();
            if folder_name.is_empty() || folder_name.starts_with('.') {
                continue;
            }

            let session_files = list_session_jsonl_files(&path);
            if session_files.is_empty() {
                continue;
            }

            let latest_file = session_files
                .iter()
                .max_by_key(|file| {
                    std::fs::metadata(file)
                        .and_then(|meta| meta.modified())
                        .ok()
                })
                .cloned();
            let Some(latest_file) = latest_file else {
                continue;
            };

            let Some(modified) = std::fs::metadata(&latest_file)
                .and_then(|meta| meta.modified())
                .ok()
            else {
                continue;
            };

            let project_path = extract_cwd_from_session_file(&latest_file)
                .unwrap_or_else(|| decode_project_folder_name(&folder_name));
            // Skip ghost projects: `~/.claude/projects/` holds session files
            // for folders that have since been moved or deleted. Falling back
            // to the raw (non-canonical) path surfaces these as live projects,
            // triggers Train suggestions that can never resolve, and — when a
            // ghost shares a basename with a real project — makes the Activity
            // tile look like it's nagging about the working copy.
            let project_path = match std::fs::canonicalize(&project_path) {
                Ok(p) => strip_extended_length_prefix(p.to_string_lossy().into_owned()),
                Err(_) => continue,
            };
            if project_path.trim().is_empty() {
                continue;
            }
            let scan = grouped_projects.entry(project_path).or_default();
            scan.last_worked_at = scan.last_worked_at.max(Some(modified));
            scan.add_session_files(session_files);
        }

        let mut projects = Vec::new();
        for (project_path, scan) in grouped_projects {
            let Some(project) = build_claude_code_project(&self.tool_manager, project_path, scan)
            else {
                continue;
            };
            projects.push(project);
        }

        projects.sort_by(|left, right| right.last_worked_at.cmp(&left.last_worked_at));
        Ok(projects)
    }

    pub fn begin_headroom_learn_run(&self, project_path: &str) -> Result<(), String> {
        if project_path.trim().is_empty() {
            return Err("Select a project before running headroom learn.".into());
        }
        if !self.tool_manager.python_runtime_installed() {
            return Err("Install Headroom runtime before running headroom learn.".into());
        }
        if !self.tool_manager.headroom_entrypoint().exists() {
            return Err("Headroom runtime is not available yet.".into());
        }
        let project = Path::new(project_path);
        if !project.exists() {
            return Err(format!(
                "Project path does not exist: {}",
                project.display()
            ));
        }
        if !project.is_dir() {
            return Err(format!(
                "Project path is not a directory: {}",
                project.display()
            ));
        }

        let mut state = self.headroom_learn_state.lock();
        if state.running {
            return Err("headroom learn is already running.".into());
        }

        state.running = true;
        state.project_path = Some(project_path.to_string());
        state.started_at = Some(Utc::now());
        state.finished_at = None;
        state.success = None;
        state.summary = format!(
            "Running headroom learn for {}.",
            project
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(project_path)
        );
        state.error = None;
        state.output_tail = Vec::new();
        state.current_step = None;
        Ok(())
    }

    /// Test hook: flip the run flag without the runtime and project prereqs
    /// that `begin_headroom_learn_run` enforces.
    #[cfg(test)]
    pub(crate) fn mark_headroom_learn_running_for_test(&self) {
        self.headroom_learn_state.lock().running = true;
    }

    /// Record what the running scan is doing. Ignored when no run is active, so
    /// a line arriving after completion cannot leave a stale step on screen.
    pub fn set_headroom_learn_step(&self, step: String) {
        let mut state = self.headroom_learn_state.lock();
        if state.running {
            state.current_step = Some(step);
        }
    }

    pub fn complete_headroom_learn_run(
        &self,
        success: bool,
        summary: String,
        error: Option<String>,
        output_tail: Vec<String>,
    ) {
        let mut state = self.headroom_learn_state.lock();
        state.running = false;
        state.finished_at = Some(Utc::now());
        state.success = Some(success);
        state.summary = summary;
        state.error = error;
        state.output_tail = output_tail;
        state.current_step = None;
        drop(state);
        // A completed run rewrites CLAUDE.md / MEMORY.md and updates the learn
        // log's mtime, so the cached project list (which depends on both) is
        // now stale — force a fresh scan on the next read.
        self.invalidate_claude_code_projects_cache();
    }

    pub fn headroom_learn_status(
        &self,
        selected_project_path: Option<&str>,
    ) -> HeadroomLearnStatus {
        let state = self.headroom_learn_state.lock().clone();

        let current_project_path = state.project_path.clone();
        let lookup_project_path = selected_project_path
            .map(|path| path.to_string())
            .or_else(|| current_project_path.clone());
        let project_display_name = current_project_path.as_deref().map(project_display_name);
        let last_run_at = lookup_project_path
            .as_deref()
            .and_then(|path| self.tool_manager.headroom_learn_last_run_at(path));
        let started_at = state.started_at.map(|value| value.to_rfc3339());
        let finished_at = state.finished_at.map(|value| value.to_rfc3339());
        let elapsed_seconds = if state.running {
            state
                .started_at
                .map(|started| (Utc::now() - started).num_seconds().max(0) as u64)
        } else {
            match (state.started_at, state.finished_at) {
                (Some(started), Some(finished)) => {
                    Some((finished - started).num_seconds().max(0) as u64)
                }
                _ => None,
            }
        };
        let progress_percent = if state.running {
            let elapsed = elapsed_seconds.unwrap_or(0) as f64;
            (8.0 + (1.0 - (-elapsed / 36.0).exp()) * 84.0).round() as u8
        } else if state.finished_at.is_some() {
            100
        } else {
            0
        };

        HeadroomLearnStatus {
            running: state.running,
            project_path: current_project_path,
            project_display_name,
            started_at,
            finished_at,
            elapsed_seconds,
            progress_percent,
            summary: state.summary,
            success: state.success,
            error: state.error,
            last_run_at,
            output_tail: state.output_tail,
            current_step: state.current_step,
        }
    }

    fn record_savings_snapshot(
        &self,
        stats: &HeadroomDashboardStats,
    ) -> Option<(
        SavingsTotalsSnapshot,
        Vec<DailySavingsPoint>,
        Vec<HourlySavingsPoint>,
    )> {
        let mut tracker = self.savings_tracker.lock();
        let snapshot = tracker.observe(stats)?;
        let daily_savings = tracker.daily_savings();
        let hourly_savings = tracker.hourly_savings();
        Some((snapshot, daily_savings, hourly_savings))
    }

    pub fn should_present_on_launch(&self) -> bool {
        true
    }

    pub fn bootstrap_progress(&self) -> BootstrapProgress {
        self.bootstrap_progress.lock().clone()
    }

    pub fn begin_bootstrap(&self) -> Result<(), String> {
        let python_installed = self.tool_manager.python_runtime_installed();
        let mut progress = self.bootstrap_progress.lock();
        let (next, result) = begin_bootstrap_transition(&progress, python_installed);
        *progress = next;
        result
    }

    pub fn update_bootstrap_step(&self, step: BootstrapStepUpdate) {
        let mut progress = self.bootstrap_progress.lock();
        // The success-path install otherwise writes no timeline to the app log
        // at all (pip lines go only to the failure-time capture, updates only
        // to the frontend), which left a 6m35s dependency step forensically
        // blank. Dedup against the previous update; the raw download counter
        // rewrites its message ~4x/s and is skipped.
        if (progress.current_step != step.step || progress.message != step.message)
            && !step.message.starts_with("Downloading ")
        {
            log::info!("bootstrap step: {} - {}", step.step, step.message);
        }
        *progress = apply_bootstrap_step(&progress, step);
    }

    pub fn mark_bootstrap_proxy_starting(&self) {
        let mut progress = self.bootstrap_progress.lock();
        *progress = BootstrapProgress {
            running: true,
            complete: false,
            failed: false,
            current_step: "Starting Headroom".into(),
            message: "Starting Headroom for the first time (this can take ~1-2 minutes)…".into(),
            current_step_eta_seconds: 45,
            overall_percent: 95,
        };
    }

    pub fn mark_bootstrap_complete(&self) {
        let mut progress = self.bootstrap_progress.lock();
        *progress = bootstrap_complete_state();
    }

    pub fn mark_bootstrap_failed<S: Into<String>>(&self, message: S) {
        let mut progress = self.bootstrap_progress.lock();
        *progress = bootstrap_failed_state(&progress, message.into());
    }

    pub fn ensure_headroom_running(&self) -> Result<()> {
        // Exit teardown is in progress: stop_headroom has run (or is about
        // to), and a proxy spawned now would be orphaned when the process
        // exits moments later, holding the port against the next launch.
        // Unconditional — even mid-upgrade-validation, quit wins.
        if crate::SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire) {
            log::info!("ensure_headroom_running: app is shutting down; not starting proxy");
            return Ok(());
        }
        if !self.tool_manager.python_runtime_installed() {
            return Ok(());
        }

        // Suppress the gate guards while a runtime upgrade is mid-validation.
        // The post-install boot validation in `run_upgrade_with_ui` calls
        // back into this function to bring the new venv up; if any of the
        // three gates below fires there, we silent-Ok-exit, the post-spawn
        // snapshot finds nothing running, and a perfectly good upgrade gets
        // rolled back as `not_started`. Routing isn't affected: client-side
        // configuration (`disable_client_setup`/`clear_client_setups`) is
        // mutated by whoever asserted the gate, so Claude Code is already
        // pointed direct-to-Anthropic regardless of whether Python is
        // bound on :6768. After validation, `run_upgrade_with_ui` calls
        // `stop_headroom()` if a gate is still active so we don't leave
        // the validation Python running where the user expected it down.
        let in_upgrade_validation = *self.runtime_upgrade_in_progress.lock();

        if !in_upgrade_validation {
            // When the pricing gate has flipped on `proxy_bypass`, Python is
            // intentionally down — the Rust intercept is routing direct to
            // Anthropic. Don't restart Python here; that would just defeat the
            // gate and (via the watchdog's failure path) eventually auto-pause
            // the runtime.
            if self.proxy_bypass.load(std::sync::atomic::Ordering::Acquire) {
                log::debug!("ensure_headroom_running: short-circuit (proxy_bypass active)");
                return Ok(());
            }

            if !self.pricing_allows_optimization() {
                self.enforce_pricing_gate();
                self.stop_python_if_gated();
                // Only the FULL gate declines the spawn. Under the Claude-only
                // gate (Codex enabled) Python must keep running for Codex, so
                // fall through and spawn (RUST-53: returning Ok here left
                // gated Codex users with no backend at all — the watchdog's
                // restarts silently no-opped until auto-pause, and the
                // self-heal loop re-declined forever). Mirrors
                // `stop_python_if_gated`'s codex carve-out.
                if !crate::client_adapters::any_gate_exempt_client_enabled() {
                    return Ok(());
                }
            }

            if self.runtime_is_paused() {
                return Ok(());
            }
        }

        // Tear down any orphan proxy from an older desktop build BEFORE taking
        // the lifecycle lock, since `stop_headroom` acquires the same lock.
        // The orphan check: a proxy is reachable, but its argv is missing flags
        // this build relies on (e.g. --log-messages, --learn). Without this we
        // would happily reuse a v0.2.x proxy that pre-dates the Activity feed.
        if is_headroom_proxy_reachable()
            && !crate::tool_manager::running_proxy_matches_expected_args()
        {
            log::debug!(
                "headroom proxy is reachable but its argv predates this build; restarting it"
            );
            self.stop_headroom();
        }

        // Serialize lifecycle transitions so launch warm-up, tray open, and the
        // watchdog cannot race into concurrent proxy spawns before the backend
        // port is reachable and `headroom_process` has been recorded.
        let _lifecycle_guard = self.lifecycle_lock.lock();

        // Another caller may have brought the runtime up while we waited.
        if !self.tool_manager.python_runtime_installed() {
            return Ok(());
        }
        // Same upgrade-validation suppression as above. Re-read the flag
        // because the upgrade could have completed between the two reads
        // (lifecycle_lock can block for the duration of another spawn).
        if !*self.runtime_upgrade_in_progress.lock() {
            if !self.pricing_allows_optimization() {
                self.enforce_pricing_gate();
                // Same Claude-only carve-out as the pre-lock check above.
                if !crate::client_adapters::any_gate_exempt_client_enabled() {
                    // Full gate: Python must come down, and enforce_pricing_gate
                    // just set proxy_bypass, which makes the pricing poll's
                    // swap-edge stop a no-op — so the teardown is owed HERE.
                    // (Codex flipped off between the two checks; skipping the
                    // stop would leave Python running under full bypass until
                    // the gate exits.) stop takes lifecycle_lock: release ours
                    // first.
                    drop(_lifecycle_guard);
                    self.stop_python_if_gated();
                    return Ok(());
                }
            }
            if self.runtime_is_paused() {
                return Ok(());
            }
        }

        // If the proxy is already live (e.g. started externally, or by us under
        // the lifecycle lock just above), treat runtime as healthy without
        // forcing another launcher.
        //
        // `is_headroom_proxy_reachable` probes 6767, the INTERCEPT, but the
        // question here is whether the BACKEND needs spawning. Those come apart
        // when the intercept is wedged over a healthy backend (a Windows
        // failure mode, see the 6767 bind history): the intercept probe says
        // "down", we fall through to spawn, 6768 is still held by that healthy
        // backend, and `reclaim_orphan_proxy` refuses to kill a healthy
        // occupant and bails -- every launch, until reboot. Three of those in a
        // row auto-pauses the runtime, which BYPASSES it, so the user keeps
        // coding and silently saves nothing. That is Sentry RUST-6J into
        // RUST-5C, and the largest Windows cluster we have.
        //
        // Asking the backend directly closes it. The argv check is what keeps
        // this safe: adopting a backend from an OLDER app build would silently
        // run a mismatched wheel and, worse, quietly disable the exact-pin
        // prefix-floor vendor. A mismatched argv fails this test and falls
        // through to the existing teardown-and-respawn path unchanged.
        let backend_serving =
            crate::tool_manager::probe_backend_readyz_ok(crate::backend_port::get());
        if runtime_already_serving(
            is_headroom_proxy_reachable(),
            backend_serving,
            // Only consult argv when it can change the answer: it shells out.
            backend_serving && crate::tool_manager::running_proxy_matches_expected_args(),
        ) {
            *self.last_startup_error.lock() = None;
            return Ok(());
        }

        {
            let mut process = self.headroom_process.lock();

            if let Some(existing) = process.as_mut() {
                match existing.try_wait() {
                    Ok(None) => return Ok(()),
                    Ok(Some(status)) => {
                        *self.last_child_natural_exit.lock() = Some(format!("{status}"));
                        *process = None;
                    }
                    Err(_) => {
                        *process = None;
                    }
                }
            }
        } // release lock before the blocking start

        self.set_runtime_starting(true);
        // During upgrade boot validation, reclaim 6768 even from a still-healthy
        // old proxy — we're replacing it, so leaving it alone would strand the
        // new venv unable to bind and roll the upgrade back as `not_started`.
        let reclaim_healthy_orphan = *self.runtime_upgrade_in_progress.lock();
        let started = self
            .tool_manager
            .start_headroom_background(reclaim_healthy_orphan);
        self.set_runtime_starting(false);

        match started {
            Ok(child) => {
                *self.headroom_process.lock() = Some(child);
                *self.last_startup_error.lock() = None;
                // Fresh child: a death recorded for its predecessor is no
                // longer diagnostic of the current episode.
                *self.last_child_natural_exit.lock() = None;
                Ok(())
            }
            Err(err) => {
                *self.last_startup_error.lock() = Some(format!("{err:#}"));
                Err(err)
            }
        }
    }

    pub fn runtime_status(&self) -> RuntimeStatus {
        // Multiple pollers (tray icon updater at 260ms, proxy watchdog at 5s,
        // frontend interval at 3s, ad-hoc pre-warms) all land here and each
        // uncached call does a blocking HTTP `/readyz` plus several file
        // stats. A short TTL collapses them into one fetch without any
        // perceptible staleness — the longest-cadence caller is 5s, so 2s
        // TTL gives each poll a fresh read while deduping within bursts.
        const TTL: Duration = Duration::from_secs(2);
        {
            let cache = self.cached_runtime_status.lock();
            if let Some((status, at)) = cache.as_ref() {
                if at.elapsed() < TTL {
                    return status.clone();
                }
            }
        }
        let status = self.compute_runtime_status();
        *self.cached_runtime_status.lock() = Some((status.clone(), Instant::now()));
        status
    }

    fn compute_runtime_status(&self) -> RuntimeStatus {
        let installed = self.tool_manager.python_runtime_installed();
        let paused = self.runtime_is_paused();
        let auto_paused = self.runtime_is_auto_paused();
        let proxy_reachable = is_headroom_proxy_reachable();
        let mcp_configured = self.tool_manager.headroom_mcp_configured();
        let mcp_error = self.tool_manager.headroom_mcp_error();
        let ml_installed = self.tool_manager.headroom_ml_installed();
        let platform = current_platform();
        let support_tier = current_platform_support_tier();
        let headroom_learn_disabled_reason = headroom_learn_platform_message();
        let kompress_enabled = if installed && proxy_reachable {
            self.tool_manager.headroom_kompress_enabled()
        } else {
            None
        };
        let rtk_installed = self.tool_manager.rtk_installed();
        let rtk_version = self.tool_manager.installed_rtk_version();
        let (rtk_path_configured, rtk_hook_configured) =
            rtk_integration_status().unwrap_or((false, false));
        let rtk_gain_summary = self.cached_rtk_gain_summary();
        let headroom_pid = {
            let mut process = self.headroom_process.lock();
            if let Some(existing) = process.as_mut() {
                match existing.try_wait() {
                    Ok(None) => Some(existing.id()),
                    Ok(Some(status)) => {
                        // Keep the status: this poller runs every few seconds
                        // and reaps a crashed child before the watchdog's
                        // give-up capture can ask how it died (RUST-53).
                        *self.last_child_natural_exit.lock() = Some(format!("{status}"));
                        *process = None;
                        None
                    }
                    Err(_) => {
                        *process = None;
                        None
                    }
                }
            } else {
                None
            }
        };

        let effective_running = installed && !paused && proxy_reachable;

        let startup_error = self.last_startup_error.lock().clone();
        // A failed intercept bind outranks any backend startup error: every
        // client is hard-configured to 127.0.0.1:6767, so nothing routes no
        // matter how healthy the Python runtime is, and the backend's own
        // error (if any) is downstream noise. Without this the banner reports
        // "runtime offline, proxy unreachable" and points the user at the
        // runtime, while the actual cause is that the port never opened.
        let startup_error_hint = self
            .intercept_bind_error
            .lock()
            .as_deref()
            .map(intercept_bind_hint)
            .or_else(|| startup_error.as_deref().and_then(classify_startup_error));

        RuntimeStatus {
            platform: platform.into(),
            support_tier: support_tier.into(),
            installed,
            running: effective_running,
            starting: self.runtime_is_starting() && !effective_running,
            paused,
            auto_paused,
            bypassed: self.proxy_bypass.load(std::sync::atomic::Ordering::Acquire),
            proxy_reachable,
            headroom_pid,
            mcp_configured,
            mcp_error,
            ml_installed,
            kompress_enabled,
            headroom_learn_supported: headroom_learn_disabled_reason.is_none(),
            headroom_learn_disabled_reason,
            startup_error,
            startup_error_hint,
            runtime_upgrade_failure: self.runtime_upgrade_failure(),
            rtk: RtkRuntimeStatus {
                installed: rtk_installed,
                enabled: !is_rtk_disabled(),
                version: rtk_version,
                path_configured: rtk_path_configured,
                hook_configured: rtk_hook_configured,
                total_commands: rtk_gain_summary.as_ref().map(|stats| stats.total_commands),
                total_saved: rtk_gain_summary.as_ref().map(|stats| stats.total_saved),
                avg_savings_pct: rtk_gain_summary.as_ref().map(|stats| stats.avg_savings_pct),
            },
        }
    }

    pub fn set_runtime_paused(&self, paused: bool) {
        let mut runtime_paused = self.runtime_paused.lock();
        *runtime_paused = paused;
        drop(runtime_paused);
        self.invalidate_runtime_status_cache();
    }

    pub fn runtime_is_paused(&self) -> bool {
        *self.runtime_paused.lock()
    }

    pub fn set_runtime_auto_paused(&self, auto_paused: bool) {
        self.runtime_auto_paused
            .store(auto_paused, std::sync::atomic::Ordering::Release);
        self.invalidate_runtime_status_cache();
    }

    pub fn runtime_is_auto_paused(&self) -> bool {
        self.runtime_auto_paused
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_runtime_starting(&self, starting: bool) {
        let mut runtime_starting = self.runtime_starting.lock();
        *runtime_starting = starting;
        drop(runtime_starting);
        self.invalidate_runtime_status_cache();
    }

    /// Drops the cached `RuntimeStatus` so the next call recomputes. Wired
    /// into every path that mutates visible runtime state (pause, resume,
    /// starting, upgrade phase) so user-initiated changes show up on the
    /// tray icon and settings UI within one tray-updater tick instead of
    /// waiting out the 2s TTL.
    pub fn invalidate_runtime_status_cache(&self) {
        *self.cached_runtime_status.lock() = None;
    }

    pub fn runtime_is_starting(&self) -> bool {
        *self.runtime_starting.lock()
    }

    pub fn resume_runtime(&self) -> Result<()> {
        self.set_runtime_paused(false);
        // Any successful resume clears the auto-pause flag so the self-heal
        // loop stops retrying and the banner drops the "stopped unexpectedly"
        // framing.
        self.set_runtime_auto_paused(false);
        // User explicitly resuming = "go back to optimizing." Clear bypass
        // so `ensure_headroom_running` doesn't short-circuit on the bypass
        // check (state.rs ~2247). If pricing still says we're gated, the
        // next pricing poll will re-set bypass; if not, Python comes up
        // and traffic flows through optimization again.
        self.proxy_bypass
            .store(false, std::sync::atomic::Ordering::Release);
        self.claude_only_bypass
            .store(false, std::sync::atomic::Ordering::Release);
        self.ensure_headroom_running()
    }

    /// Ask the backend to dump all Python thread stacks into its own log
    /// (SIGUSR1 handler registered by the desktop-injected sitecustomize.py),
    /// then give it a moment to flush. Called by the watchdog right before a
    /// wedge force-kill so a silent hang leaves evidence of where the event
    /// loop was stuck. Blocking sleep is fine: only the watchdog thread calls
    /// this, once per down episode.
    pub fn dump_backend_stacks(&self) {
        let Some(pid) = self.headroom_process.lock().as_ref().map(|c| c.id()) else {
            return;
        };
        let _ = crate::proc::command("/bin/kill")
            .arg("-USR1")
            .arg(pid.to_string())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    pub fn stop_headroom(&self) {
        // `ensure_headroom_running` holds this lock across a blocking backend
        // start, so a launch racing this stop can hold it for the length of that
        // spawn. Quit must not wait on it: `restart_app` calls us before it can
        // post the exit request, so a lock held here leaves the app alive with
        // its window stuck on "Restarting..." and no relaunch - and it is the
        // only unbounded wait on that path (the child wait below is capped at
        // 2s, the analytics flush at 3s). Stop unguarded rather than never: the
        // caller has already set SHUTTING_DOWN, and the pkill sweep at the end
        // reaps whatever a racing spawn manages to leave behind.
        let _lifecycle_guard = self
            .lifecycle_lock
            .try_lock_for(STOP_LIFECYCLE_LOCK_TIMEOUT)
            .or_else(|| {
                log::warn!(
                    "stop_headroom: lifecycle lock still held after {}s; stopping without it",
                    STOP_LIFECYCLE_LOCK_TIMEOUT.as_secs()
                );
                None
            });
        // Without the lock, another lifecycle transition in THIS app is mid-
        // spawn, and its child is exactly what the sweep below would match:
        // it has our pid as parent and has not bound the port yet. Reap only
        // true orphans in that case (see kill_processes_by_command_pattern).
        let lock_held = _lifecycle_guard.is_some();
        // Every app-initiated stop is a down window we caused: a watchdog
        // restart, a pricing-gate pause, a port rebind, quit. The down->up
        // transition that follows is not an outage worth paging, and when the
        // stop came from the watchdog auto-pause it duplicates
        // `proxy_unreachable_post_boot` with none of its diagnostics
        // (RUST-5J/5D). A backend that dies on its own never routes through
        // here, so genuine crashes still report. The window has to outlast a
        // cold boot (tiktoken prefetch alone gets 120s); the upgrade path
        // above uses 600s for a reinstall+boot.
        crate::proxy_intercept::suppress_codex_reconnect_reports_for(
            std::time::Duration::from_secs(300),
        );
        // `starting` belongs to the transition holding the lifecycle lock. An
        // unguarded stop clearing it mid-spawn told the watchdog the runtime
        // should be up, so 15s later it hung-killed the spawn it should have
        // been waiting for (RUST-CD: three stops in 12s, then 0xffffffff).
        if lock_held {
            self.set_runtime_starting(false);
        }
        let mut process = self.headroom_process.lock();

        if let Some(mut child) = process.take() {
            let pid = child.id() as i32;
            terminate_process_tree(pid, false);
            // Bounded wait: a backend that ignores SIGTERM (mid-request, stuck
            // shutdown) must not block this caller forever. stop_headroom runs
            // on the UI thread during restart_app, so an unbounded child.wait()
            // freezes the app ("not responding"). Give it ~2s, then SIGKILL the
            // process group and reap.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            terminate_process_tree(pid, true);
                            // The group kill only reaches the child if the child
                            // is actually in that group, and `child.wait()` is
                            // unbounded when it isn't: rc15 sat here for the
                            // eight seconds between its logged -KILL and the
                            // relauncher force-killing the app. Signal the pid we
                            // hold a handle to as well - the kernel cannot recycle
                            // it before we reap, so this one always lands.
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        }

        // Also clean up detached/orphaned Headroom-managed headroom proxies
        // so quitting the UI cannot leave the background listener behind.
        // We deliberately drop the port number from the match pattern: the
        // proxy may have fallen back to 6769..=6790 if 6768 was foreign-held,
        // and the python module path / entrypoint subcommand is unique enough
        // to identify our proxies regardless of port.
        let managed_python = self.tool_manager.managed_python();
        let headroom_entrypoint = self.tool_manager.headroom_entrypoint();
        let command_patterns = [
            (managed_python.as_path(), "-m headroom.proxy.server"),
            (headroom_entrypoint.as_path(), "proxy --port"),
            // On macOS the entrypoint re-execs itself as `python -m
            // headroom.cli proxy ...` for malloc tuning (upstream
            // cli/proxy.py `_reexec_with_malloc_tuning`), so that is what a
            // live or orphaned backend's argv actually reads there; neither
            // pattern above matches it.
            (managed_python.as_path(), "-m headroom.cli proxy"),
        ];
        for (exe, args_pattern) in command_patterns {
            log::info!(
                "stop_headroom: pkill -f {:?}",
                format!("{} {args_pattern}", exe.display())
            );
            if let Err(err) = kill_processes_by_command_pattern(exe, args_pattern, lock_held) {
                // `:#` prints the whole context chain: a spawn failure's io
                // error (RUST-6H's 0.9.5 wave) is invisible without it.
                log::warn!("failed to clean detached headroom proxy processes: {err:#}");
            }
        }
        log::info!("stop_headroom: done");
    }

    /// One-shot, best-effort prefetch of the Kompress ML model on a fresh
    /// install. Blocks (run on a background thread) — downloads the ~260MB
    /// model the proxy would otherwise fetch lazily on first request, so a new
    /// user has ML compression ready before any traffic and never sees a
    /// lingering "Kompress disabled" banner.
    ///
    /// Skips immediately (no work) when: already attempted this launch, the
    /// runtime isn't installed/reachable, the `[ml]` extras aren't installed,
    /// the model is already cached, or Kompress already reports enabled.
    ///
    /// On a successful download, if the proxy has been idle (no recent
    /// proxy-log activity) it does one graceful restart so startup eager-load
    /// re-reports `Kompress: ENABLED`. If the proxy is actively serving, it
    /// skips the restart — `headroom_kompress_enabled` detects the lazy-load
    /// marker on the next request instead, so the status still flips on its own.
    pub fn maybe_prefetch_kompress(&self) {
        // One-shot guard: claim the attempt; bail if another call already did.
        if self
            .kompress_prefetch_attempted
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }

        if !self.tool_manager.python_runtime_installed() || !is_headroom_proxy_reachable() {
            return;
        }
        // Only meaningful when the ML extras are present but the model isn't
        // loaded yet. If ml isn't installed, prefetch can't help; if Kompress
        // already reports enabled, there's nothing to do.
        if self.tool_manager.headroom_ml_installed() != Some(true) {
            return;
        }
        if self.tool_manager.kompress_model_cached()
            || self.tool_manager.headroom_kompress_enabled() == Some(true)
        {
            return;
        }

        log::info!("kompress prefetch: downloading model on fresh install");
        match self.tool_manager.prefetch_kompress_model() {
            Ok(crate::tool_manager::KompressPrefetchOutcome::Downloaded) => {}
            Ok(crate::tool_manager::KompressPrefetchOutcome::Failed { cause }) => {
                // Explicit per-category fingerprint: message-based grouping
                // lumped every cause (network blip, ModuleNotFoundError, disk
                // full) into one grab-bag issue (RUST-3C/RUST-45), so resolving
                // one shape regressed when a sibling reappeared. The log::warn
                // is local-only (skip_sentry rule) to avoid double-reporting.
                let category = cause
                    .strip_prefix('[')
                    .and_then(|rest| rest.split_once(']'))
                    .map(|(cat, _)| cat)
                    .unwrap_or("other");
                sentry::with_scope(
                    |scope| {
                        scope.set_fingerprint(Some(&["kompress-prefetch-download", category]));
                    },
                    || {
                        sentry::capture_message(
                            &format!("kompress prefetch download error: {cause}"),
                            sentry::Level::Warning,
                        );
                    },
                );
                log::warn!("kompress prefetch download error: {cause}");
                return;
            }
            Err(err) => {
                log::warn!("kompress prefetch failed: {err:#}");
                return;
            }
        }
        log::info!("kompress prefetch: model cached");

        // Invalidate the runtime-status cache so the freshly-cached state is
        // reflected on the next poll regardless of the restart decision.
        *self.cached_runtime_status.lock() = None;

        // Surface "enabled" proactively only when safe: a restart drops any
        // in-flight request, so we require the proxy to be idle first.
        if self.runtime_is_paused() || self.runtime_is_starting() {
            return;
        }
        // Onboarding in progress: the launcher's "test your setup" step passes
        // the idle heuristic (no traffic yet), but a restart here takes the
        // backend down for the model-load boot exactly when the user sends
        // their first test prompt. Lazy-load detection covers enablement.
        // Must be the pure completion flag: `setup_wizard_satisfied()` flips
        // true mid-onboarding via its legacy heuristic (launch_count > 1 +
        // clients configured) the moment the client-setup step writes configs
        // — exactly the state during the test step. Legacy installs that
        // never ran the wizard keep the flag false forever; for them the
        // restart stays deferred and Kompress enables via lazy-load instead.
        if !self.setup_wizard_complete() {
            log::info!(
                "kompress prefetch: onboarding in progress, deferring restart to lazy-load detection"
            );
            return;
        }
        let idle = newest_proxy_log_mtime(&self.tool_manager.logs_dir())
            .and_then(|mtime| std::time::SystemTime::now().duration_since(mtime).ok())
            .map(|age| age >= std::time::Duration::from_secs(20))
            .unwrap_or(true);
        if !idle {
            log::info!("kompress prefetch: proxy busy, deferring restart to lazy-load detection");
            return;
        }

        log::info!("kompress prefetch: restarting proxy to load cached model");
        self.stop_headroom();
        if let Err(err) = self.ensure_headroom_running() {
            log::warn!("kompress prefetch: restart after download failed: {err:#}");
        }
        *self.cached_runtime_status.lock() = None;
    }

    fn pricing_allows_optimization(&self) -> bool {
        pricing::get_pricing_status(self)
            .map(|status| status.optimization_allowed)
            .unwrap_or(true)
    }

    /// Flip the bypass flag based on current pricing. Safe to call while
    /// holding `lifecycle_lock` — this never tries to acquire it. Stopping
    /// the Python proxy is `stop_python_if_gated`'s job (it does take the
    /// lock) and must be invoked separately.
    ///
    /// Does NOT touch `client-setup.json`, `~/.claude/settings.json`, or
    /// shell blocks. Those are durable user setup, not runtime state — the
    /// bypass flag alone is enough to make the Rust intercept pass traffic
    /// straight through to api.anthropic.com while Python is down.
    fn enforce_pricing_gate(&self) {
        use std::sync::atomic::Ordering::Release;
        match pricing::get_pricing_status(self) {
            Ok(status) if !status.optimization_allowed => {
                // Gated. When Codex is still enabled, use the Claude-only
                // bypass (Python stays up for Codex) instead of the full
                // bypass, so a Claude overage doesn't pause Codex. Mirrors
                // `apply_pricing_gate_status`. Python lifecycle is handled by
                // `stop_python_if_gated` / `ensure_headroom_running` — this
                // only flips the flags (lock-safe).
                if crate::client_adapters::any_gate_exempt_client_enabled() {
                    self.claude_only_bypass.store(true, Release);
                    self.proxy_bypass.store(false, Release);
                } else {
                    self.proxy_bypass.store(true, Release);
                    self.claude_only_bypass.store(false, Release);
                }
            }
            Ok(_) => {
                self.proxy_bypass.store(false, Release);
                self.claude_only_bypass.store(false, Release);
            }
            // Leave the flags on their last known value: a pricing lookup that
            // failed is not evidence the user became gated, and flipping
            // bypass on a transient error would drop them to unoptimized
            // traffic. Deliberately fail-open, but NOT silent -- if this
            // starts failing persistently the gate freezes on whatever it last
            // decided and nothing else in the app would ever say so. warn!
            // bridges to Sentry, so a sustained outage surfaces instead of
            // looking like a healthy ungated fleet.
            Err(err) => {
                log::warn!("enforce_pricing_gate: pricing status unavailable, leaving gate flags unchanged: {err}");
            }
        }
    }

    /// Stop the Python proxy when pricing currently disallows optimization.
    /// Acquires `lifecycle_lock`, so callers MUST NOT already hold it.
    fn stop_python_if_gated(&self) {
        // Only tear Python down on a FULL bypass. When Codex is enabled the gate
        // is Claude-only and Python must stay up to keep optimizing Codex.
        if !self.pricing_allows_optimization()
            && !crate::client_adapters::any_gate_exempt_client_enabled()
        {
            self.stop_headroom();
        }
    }

    /// Reconcile the runtime against a freshly evaluated pricing status.
    /// Detects gated→ungated and ungated→gated transitions and runs the
    /// matching side-effects (start/stop the Python proxy, flip the bypass
    /// flag). Idempotent on no-op cases — safe to call from every pricing
    /// poll.
    ///
    /// The ungated→gated transition is debounced: the bypass flip only
    /// fires once `optimization_allowed=false` has been observed for
    /// `PRICING_GATE_DEBOUNCE_POLLS` consecutive polls. The gated→ungated
    /// direction has no debounce — recovery should be immediate.
    ///
    /// Acquires `lifecycle_lock` (via `stop_headroom` / `ensure_headroom_running`),
    /// so callers MUST NOT already hold it.
    /// `codex_keep_alive` tells the gate that Codex is still enabled and
    /// entitled, so a Claude overage must not pause Codex optimization. When
    /// set, a tripped Claude gate enters `claude_only_bypass` (Python stays up,
    /// only Claude traffic forwards direct) instead of the full `proxy_bypass`
    /// (Python torn down, everything direct). Computed at the call site via
    /// `client_adapters::is_codex_enabled()` so this method stays pure for tests.
    pub fn apply_pricing_gate_status(
        &self,
        status: &crate::models::HeadroomPricingStatus,
        codex_keep_alive: bool,
    ) {
        use std::sync::atomic::Ordering::{Acquire, Release};
        let was_bypassed = self.proxy_bypass.load(Acquire) || self.claude_only_bypass.load(Acquire);
        let should_bypass = !status.optimization_allowed;

        if should_bypass {
            if !was_bypassed {
                // Debounce the ungated → gated transition: only flip once we've
                // seen `PRICING_GATE_DEBOUNCE_POLLS` consecutive gated readings.
                let prev = self
                    .pricing_gate_violation_streak
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                let streak = prev.saturating_add(1);
                if streak < PRICING_GATE_DEBOUNCE_POLLS {
                    log::info!(
                        "pricing_gate: gated reading {streak}/{PRICING_GATE_DEBOUNCE_POLLS} — debouncing before bypass flip"
                    );
                    return;
                }
            }
            // Enter (or re-sync) the Claude gate. Idempotent: the swap guards
            // below only fire stop_headroom/ensure_headroom_running on a real
            // mode transition, so calling this on every gated poll is cheap and
            // also flips us between full and Claude-only bypass if Codex's
            // enable state changed while already gated.
            self.enter_claude_gate(codex_keep_alive);
        } else {
            // Any ungated reading clears the violation streak so a later
            // gated reading starts the debounce window over.
            self.pricing_gate_violation_streak.store(0, Release);
            if was_bypassed {
                self.exit_claude_gate();
            }
        }
    }

    /// Fire-and-forget weekly-limit reporting to headroom-web on the rising edge
    /// of each condition. The server emails free-plan users, ignores subscribers,
    /// and throttles to ~one per weekly window; the per-session latches here just
    /// avoid re-posting on every 60s poll while the condition holds.
    pub fn report_weekly_limit_transitions(&self, status: &crate::models::HeadroomPricingStatus) {
        use std::sync::atomic::Ordering::Relaxed;
        match crate::pricing::weekly_limit_signal(status) {
            Some(nudge) if nudge.status == "reached" => {
                if !self.weekly_limit_reached_reported.swap(true, Relaxed) {
                    crate::pricing::report_weekly_limit(nudge.status, nudge.cap_percent);
                }
            }
            Some(nudge) if nudge.status == "approaching" => {
                if !self.weekly_limit_approaching_reported.swap(true, Relaxed) {
                    crate::pricing::report_weekly_limit(nudge.status, nudge.cap_percent);
                }
            }
            _ => {
                self.weekly_limit_reached_reported.store(false, Relaxed);
                self.weekly_limit_approaching_reported.store(false, Relaxed);
            }
        }
    }

    /// Apply the gated state. `codex_keep_alive=false` → full bypass: tear Python
    /// down and forward everything direct. `true` → Claude-only bypass: keep
    /// Python up for Codex, forward only Claude traffic direct. Idempotent.
    fn enter_claude_gate(&self, codex_keep_alive: bool) {
        use std::sync::atomic::Ordering::{AcqRel, Release};
        if codex_keep_alive {
            self.claude_only_bypass.store(true, Release);
            // If we had fully torn Python down before Codex became eligible,
            // bring it back so Codex keeps getting optimized.
            if self.proxy_bypass.swap(false, AcqRel) {
                if let Err(err) = self.ensure_headroom_running() {
                    log::warn!("enter_claude_gate: ensure_headroom_running failed: {err:#}");
                }
            }
        } else {
            self.claude_only_bypass.store(false, Release);
            // Flip bypass FIRST so the intercept passes new requests straight
            // through while we tear Python down — otherwise there's a window
            // where 6767 → 6768 connect fails and Claude Code sees 502.
            if !self.proxy_bypass.swap(true, AcqRel) {
                self.stop_headroom();
            }
        }
    }

    /// Transition: gated → ungated (user upgraded, or weekly usage rolled over).
    /// Clear both bypass flags and, if Python was torn down, bring it back. No
    /// client_setups restore needed — gating never tore them down.
    fn exit_claude_gate(&self) {
        use std::sync::atomic::Ordering::{AcqRel, Release};
        self.claude_only_bypass.store(false, Release);
        if self.proxy_bypass.swap(false, AcqRel) {
            if let Err(err) = self.ensure_headroom_running() {
                log::warn!("exit_claude_gate: ensure_headroom_running failed: {err:#}");
            }
        }
    }

    pub fn codex_plan_tier(&self) -> crate::models::CodexPlanTier {
        (*self.codex_plan_tier.lock()).unwrap_or(crate::models::CodexPlanTier::Unknown)
    }

    /// TTL-cached Codex identity profile, the Codex analog of
    /// `cached_claude_profile`. Reads `~/.codex/auth.json` at most once per TTL.
    /// `None` when nothing is known yet (no auth.json and no live capture).
    pub fn cached_codex_profile(&self) -> Option<CodexAccountProfile> {
        const TTL: Duration = Duration::from_secs(300);
        {
            let cache = self.cached_codex_profile.lock();
            if let Some((profile, at)) = &*cache {
                if at.elapsed() < TTL {
                    return profile.clone();
                }
            }
        }
        let profile = pricing::detect_codex_profile(self);
        *self.cached_codex_profile.lock() = Some((profile.clone(), Instant::now()));
        profile
    }

    /// Codex-only parallel to `apply_pricing_gate_status`. Flips `codex_bypass`
    /// from the Codex gate's `optimization_allowed`, debounced the same way.
    /// Unlike the Claude gate this NEVER stops the Python backend — enforcement
    /// is per-request in the intercept (OpenAI-path traffic forwards direct),
    /// so a Codex overage can't pause Claude optimization for a mixed user.
    pub fn apply_codex_pricing_gate_status(&self, codex: Option<&crate::models::CodexUsage>) {
        let was_bypassed = self.codex_bypass.load(std::sync::atomic::Ordering::Acquire);
        // No Codex usage signal yet → leave the current state untouched rather
        // than clearing a gate that a transient empty poll didn't disprove.
        let Some(codex) = codex else {
            return;
        };
        let should_bypass = !codex.optimization_allowed;

        if should_bypass {
            if was_bypassed {
                return;
            }
            let prev = self
                .codex_gate_violation_streak
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let streak = prev.saturating_add(1);
            if streak < PRICING_GATE_DEBOUNCE_POLLS {
                log::info!(
                    "codex_gate: gated reading {streak}/{PRICING_GATE_DEBOUNCE_POLLS} — debouncing before bypass flip"
                );
                return;
            }
            self.codex_bypass
                .store(true, std::sync::atomic::Ordering::Release);
        } else {
            self.codex_gate_violation_streak
                .store(0, std::sync::atomic::Ordering::Release);
            if was_bypassed {
                self.codex_bypass
                    .store(false, std::sync::atomic::Ordering::Release);
            }
        }
    }
}

/// Number of consecutive gated pricing polls required before flipping
/// `proxy_bypass` on. With the React UI's 60s focused / 600s blurred poll
/// cadence, 2 polls = 1–10 minutes minimum before a gated state takes effect.
/// Tuned to ride out single-poll spikes (Anthropic returning a stale or
/// momentary high utilization, transient network failures clearing auth
/// state) without delaying real threshold crossings meaningfully.
const PRICING_GATE_DEBOUNCE_POLLS: u32 = 2;

pub(crate) fn current_platform() -> &'static str {
    std::env::consts::OS
}

pub(crate) fn current_platform_support_tier() -> &'static str {
    support_tier_for_platform(current_platform())
}

pub(crate) fn support_tier_for_platform(os: &str) -> &'static str {
    match os {
        "linux" | "windows" => "experimental",
        _ => "stable",
    }
}

/// Platform kill switch for Headroom Learn. Linux was gated here through
/// 0.8.4-rc.4; Learn is the same Python backend on every platform, so nothing
/// is gated now. Return a message here to disable Learn on a platform again.
pub(crate) fn headroom_learn_platform_message() -> Option<String> {
    None
}

impl Drop for AppState {
    fn drop(&mut self) {
        let mut process = self.headroom_process.lock();
        if let Some(mut child) = process.take() {
            let pid = child.id() as i32;
            terminate_process_tree(pid, false);
            let _ = child.wait();
        }
    }
}

fn user_home_dir() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
}

fn claude_projects_dir() -> PathBuf {
    user_home_dir().join(".claude").join("projects")
}

#[derive(Debug, Default)]
struct ClaudeProjectScan {
    last_worked_at: Option<std::time::SystemTime>,
    session_files: Vec<PathBuf>,
    seen_session_files: HashSet<PathBuf>,
}

impl ClaudeProjectScan {
    fn add_session_files(&mut self, session_files: Vec<PathBuf>) {
        for session_file in session_files {
            let dedupe_key = canonical_session_file_path(&session_file);
            if self.seen_session_files.insert(dedupe_key) {
                self.session_files.push(session_file);
            }
        }
    }
}

fn build_claude_code_project(
    tool_manager: &ToolManager,
    project_path: String,
    scan: ClaudeProjectScan,
) -> Option<ClaudeCodeProject> {
    let last_worked_at: chrono::DateTime<Utc> = scan.last_worked_at?.into();
    let session_count = scan.session_files.len();
    let mut hasher = Sha256::new();
    hasher.update(project_path.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let id = digest[..12].to_string();
    let display_name = Path::new(&project_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| project_path.clone());

    let learn_summary = tool_manager.headroom_learn_project_summary(&project_path);
    let last_learn_ran_at = learn_summary.last_run_at;
    let has_persisted_learnings = learn_summary.has_persisted_learnings;
    let last_learn_pattern_count = learn_summary.pattern_count;
    let learn_time = last_learn_ran_at
        .as_ref()
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|ts| ts.with_timezone(&Utc));
    let today = Utc::now().date_naive();
    let mut days_since_learn: HashSet<chrono::NaiveDate> = HashSet::new();
    let mut sessions_today: usize = 0;
    for file in &scan.session_files {
        let Ok(meta) = std::fs::metadata(file) else {
            continue;
        };
        let Ok(m) = meta.modified() else {
            continue;
        };
        let t: chrono::DateTime<Utc> = m.into();
        if t.date_naive() == today {
            sessions_today += 1;
        }
        if let Some(learn_time) = learn_time {
            if t > learn_time {
                days_since_learn.insert(t.date_naive());
            }
        }
    }
    let active_days_since_last_learn = if learn_time.is_some() {
        days_since_learn.len()
    } else {
        0
    };

    Some(ClaudeCodeProject {
        id,
        project_path,
        display_name,
        last_worked_at: last_worked_at.to_rfc3339(),
        session_count,
        sessions_today,
        last_learn_ran_at,
        has_persisted_learnings,
        active_days_since_last_learn,
        last_learn_pattern_count,
    })
}

fn list_session_jsonl_files(project_dir: &Path) -> Vec<PathBuf> {
    let mut files = std::fs::read_dir(project_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(|entry| entry.ok()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("jsonl"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    });
    files
}

/// On Windows, `std::fs::canonicalize` returns extended-length paths
/// (`\\?\C:\...`). This string leaves the app as `headroom learn --project
/// <path>`, where the pinned Python CLI matches it against transcript `cwd`
/// values by literal `Path` equality -- the prefix made every match fail, so
/// learn runs completed in half a second with "No project data found" and
/// exit 0 (observed on the 0.9.1-rc.4 Windows smoke). Strip it back to the
/// plain form; no-op on Unix, where the prefix never occurs.
fn strip_extended_length_prefix(path: String) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path
    }
}

fn canonical_session_file_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn extract_cwd_from_session_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;

    for line in reader.lines().map_while(|line| line.ok()).take(300) {
        if !line.contains("\"cwd\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(|item| item.as_str()) {
            if !cwd.trim().is_empty() {
                return Some(cwd.to_string());
            }
        }
    }

    None
}

fn decode_project_folder_name(folder_name: &str) -> String {
    // Claude Code's folder-name convention is lossy: it maps '/' to '-' without
    // escaping existing hyphens, so paths like `/a/b-c` and `/a/b/c` produce the
    // same folder. We mirror that convention here and accept the ambiguity --
    // the primary resolver (`extract_cwd_from_session_file`) reads the real cwd
    // from session JSONL, so this fallback only runs when that fails.
    if !folder_name.starts_with('-') {
        return folder_name.to_string();
    }
    let rebuilt = format!("/{}", folder_name.trim_start_matches('-').replace('-', "/"));
    if rebuilt.trim().is_empty() {
        folder_name.to_string()
    } else {
        rebuilt
    }
}

fn project_display_name(project_path: &str) -> String {
    Path::new(project_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(|name| name.to_string())
        .unwrap_or_else(|| project_path.to_string())
}

pub fn tail_lines(text: &str, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    lines
}

/// How an explicitly configured upstream interacts with a cc-switch capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamOverrideMode {
    /// Nothing configured: the runtime default, or whatever cc-switch captures.
    #[default]
    Off,
    /// Boot with the configured upstream, but let a later cc-switch capture
    /// win. This is what the proxy does natively -- ANTHROPIC_TARGET_API_URL
    /// sets the boot default and the reconciler overwrites it at runtime.
    Fallback,
    /// The configured upstream wins, including over a cc-switch capture. The
    /// reconciler still rewrites the client's base_url back to the intercept,
    /// it just does not get to move the upstream.
    Override,
}

/// A user-configured Anthropic-compatible upstream (GLM, Kimi, DeepSeek).
///
/// The token is deliberately NOT a field here: it lives in the OS keychain and
/// `has_token` only records that one exists, so launch-profile.json never
/// carries a credential. Everything in this struct is safe to log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamOverride {
    pub mode: UpstreamOverrideMode,
    /// Normalized by `normalize_upstream_base_url`; empty when unset.
    pub base_url: String,
    pub has_token: bool,
    /// Id of the preset in `client_adapters::PROVIDER_PRESETS` this came from,
    /// or empty for a hand-entered endpoint. Only the dropdown reads it: the
    /// URL and model below are already resolved.
    pub provider: String,
    /// Model id the provider serves, written to every big `ANTHROPIC_DEFAULT_*_MODEL`
    /// slot. Empty when unset, which leaves the provider to map Claude ids.
    pub model: String,
    /// Context window in tokens for `CLAUDE_CODE_AUTO_COMPACT_WINDOW`. Kept as
    /// a string because empty means unset; digits are validated on save.
    pub context_window: String,
}

impl UpstreamOverride {
    /// The upstream to boot with, or None when nothing usable is configured.
    /// A mode without a URL is not an upstream, so it is treated as unset
    /// rather than booting the proxy at an empty target.
    pub fn configured_upstream(&self) -> Option<&str> {
        if self.mode == UpstreamOverrideMode::Off || self.base_url.is_empty() {
            return None;
        }
        Some(self.base_url.as_str())
    }

    /// Whether the configured upstream must survive a cc-switch capture.
    pub fn pins_upstream(&self) -> bool {
        self.mode == UpstreamOverrideMode::Override && self.configured_upstream().is_some()
    }
}

/// Accept only what can safely become `ANTHROPIC_TARGET_API_URL` and be written
/// into a user's settings.json: an absolute http(s) URL, no whitespace, no
/// trailing slash (the reconciler's loop guard compares stripped URLs, so a
/// trailing slash there would make every tick rewrite).
pub fn normalize_upstream_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter the provider's base URL.".into());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("The base URL cannot contain spaces.".into());
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("The base URL must start with http:// or https://".into());
    }
    let stripped = trimmed.trim_end_matches('/');
    let host = stripped
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    if host.is_empty() {
        return Err("That base URL has no host.".into());
    }
    Ok(stripped.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct LaunchProfile {
    launch_count: u64,
    launch_experience: LaunchExperience,
    lifetime_requests: usize,
    lifetime_estimated_savings_usd: f64,
    lifetime_estimated_tokens_saved: u64,
    #[serde(default)]
    setup_wizard_complete: bool,
    #[serde(default)]
    last_launched_app_version: Option<String>,
    #[serde(default)]
    last_runtime_upgrade_failure: Option<RuntimeUpgradeFailure>,
    /// Highest Terms-of-Service version the user has accepted. Defaults to 0
    /// for profiles written before this field existed, so existing users are
    /// re-prompted by the acceptance gate when `REQUIRED_TERMS_VERSION` > 0.
    #[serde(default)]
    accepted_terms_version: u32,
    /// User-configured Anthropic-compatible upstream. Off/empty for everyone
    /// who has not set one, which is the pre-existing behaviour.
    #[serde(default)]
    upstream_override: UpstreamOverride,
    /// One-shot: the "setup finished but no traffic ever" recovery
    /// notification has fired. Persisted so it can never nag twice.
    #[serde(default)]
    onboarding_recovery_notified: bool,
    /// One-shot: the first-savings celebration notification has fired.
    /// Persisted because lifetime savings derive from retained per-day buckets
    /// and can fall back to zero after long inactivity — without this a
    /// returning user could be re-congratulated on "first" savings.
    #[serde(default)]
    first_savings_notified: bool,
    /// One-shot: the evidence-based "coding around Headroom" nudge has fired
    /// (Claude Code sessions grew during a run that forwarded nothing).
    #[serde(default)]
    unrouted_usage_notified: bool,
}

fn persist_launch_profile(path: &std::path::Path, profile: &LaunchProfile) {
    if let Ok(bytes) = serde_json::to_vec_pretty(profile) {
        let _ = crate::client_adapters::atomic_write(path, &bytes);
    }
}

impl Default for LaunchProfile {
    fn default() -> Self {
        Self::fresh()
    }
}

impl LaunchProfile {
    fn fresh() -> Self {
        LaunchProfile {
            launch_count: 0,
            launch_experience: LaunchExperience::FirstRun,
            lifetime_requests: 0,
            lifetime_estimated_savings_usd: 0.0,
            lifetime_estimated_tokens_saved: 0,
            setup_wizard_complete: false,
            last_launched_app_version: None,
            last_runtime_upgrade_failure: None,
            accepted_terms_version: 0,
            upstream_override: UpstreamOverride::default(),
            onboarding_recovery_notified: false,
            first_savings_notified: false,
            unrouted_usage_notified: false,
        }
    }

    fn load_or_create(base_dir: &std::path::Path) -> Result<(Self, std::path::PathBuf)> {
        let path = config_file(base_dir, "launch-profile.json");

        // A corrupt or truncated profile (0-byte file from a crash mid-write,
        // RUST-1P) must not crash startup — that's an unrecoverable launch
        // loop until the user manually deletes the file. Degrade to a fresh
        // profile; the warn still reaches Sentry for visibility.
        let previous = if path.exists() {
            std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    serde_json::from_slice::<LaunchProfile>(&bytes).map_err(anyhow::Error::from)
                })
                .unwrap_or_else(|err| {
                    log::warn!(
                        "launch profile at {} unreadable ({err}); backing up and starting fresh",
                        path.display()
                    );
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    Self::fresh()
                })
        } else {
            Self::fresh()
        };

        let mut current = previous;
        current.launch_count += 1;

        // Migrate legacy seeded demo totals to true zero-based tracking.
        if current.lifetime_requests == 138
            && (current.lifetime_estimated_savings_usd - 31.72).abs() < f64::EPSILON
            && current.lifetime_estimated_tokens_saved == 512_844
        {
            current.lifetime_requests = 0;
            current.lifetime_estimated_savings_usd = 0.0;
            current.lifetime_estimated_tokens_saved = 0;
        }

        if current.launch_count == 1 {
            current.launch_experience = LaunchExperience::FirstRun;
        } else {
            current.launch_experience = LaunchExperience::Resume;
        }

        // Best-effort persist: a failed write here (e.g. EPERM from locked-down
        // Application Support perms, RUST-1P) must not crash startup. The profile
        // is telemetry; degrade to the in-memory copy and continue.
        if let Ok(bytes) = serde_json::to_vec_pretty(&current) {
            if let Err(e) = crate::client_adapters::atomic_write(&path, &bytes) {
                log::warn!("could not persist {}: {e:#}", path.display());
            }
        }

        Ok((current, path))
    }
}

fn configured_client_present() -> bool {
    crate::client_adapters::is_claude_code_enabled()
        || crate::client_adapters::any_gate_exempt_client_enabled()
}

fn setup_wizard_satisfied_for_profile(
    profile: &LaunchProfile,
    legacy_clients_configured: bool,
) -> bool {
    profile.setup_wizard_complete || (profile.launch_count > 1 && legacy_clients_configured)
}

/// Whether the "setup finished but no traffic ever" nudge may still fire:
/// wizard finished, this is a return launch (the install session itself is
/// excluded so the nudge can't race the wizard), and it never fired before.
fn onboarding_recovery_nudge_due(profile: &LaunchProfile) -> bool {
    !profile.onboarding_recovery_notified
        && profile.setup_wizard_complete
        && profile.launch_count >= 2
}

/// Whether the evidence-based unrouted-usage nudge may still fire: wizard
/// finished (during the wizard the verify screen owns this moment) and it
/// never fired before. No return-launch requirement — see
/// `try_mark_unrouted_usage_notified`.
fn unrouted_usage_nudge_due(profile: &LaunchProfile) -> bool {
    !profile.unrouted_usage_notified && profile.setup_wizard_complete
}

/// Last classification that returned a non-Unknown tier. Persisted so the
/// pricing gate can keep applying the right thresholds when Anthropic's
/// OAuth profile transiently comes back sparse and the live classifier
/// returns Unknown.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LastKnownGoodPlan {
    plan_tier: crate::models::ClaudePlanTier,
    recorded_at: DateTime<Utc>,
}

impl LastKnownGoodPlan {
    fn load(base_dir: &std::path::Path) -> (Option<Self>, std::path::PathBuf) {
        let path = config_file(base_dir, "last-known-good-plan.json");
        let value = if path.exists() {
            std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
        } else {
            None
        };
        (value, path)
    }
}

fn persist_last_known_good_plan(path: &std::path::Path, plan: &LastKnownGoodPlan) {
    if let Ok(bytes) = serde_json::to_vec_pretty(plan) {
        let _ = crate::client_adapters::atomic_write(path, &bytes);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SavingsTotalsSnapshot {
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_savings_pct: f64,
    lifetime_requests: usize,
}

const FIRST_LIFETIME_TOKEN_MILESTONES: [u64; 3] = [100_000, 1_000_000, 5_000_000];
const REPEATING_LIFETIME_TOKEN_MILESTONE_STEP: u64 = 10_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct SavingsRecord {
    /// Schema version for forward-compatibility and migration detection.
    /// v0 = legacy (USD derived from tokens/10000)
    /// v2 = day-scoped deltas
    /// v3 = session-scoped deltas matching Headroom /stats
    /// v4 = session-scoped deltas plus actual usage totals
    /// v5 = v4 plus hour-scoped bucket keys
    /// v6 = v5 plus spend metrics sourced from /stats actual-input fields only
    /// v7 = v6 plus spend backfills distributed across session history
    schema_version: u8,
    id: String,
    observed_at: chrono::DateTime<Utc>,
    day_key: String,
    hour_key: String,
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_actual_cost_usd: f64,
    session_total_tokens_sent: u64,
    delta_requests: usize,
    delta_estimated_savings_usd: f64,
    delta_estimated_tokens_saved: u64,
    delta_actual_cost_usd: f64,
    delta_total_tokens_sent: u64,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavingsObservation {
    observed_at: chrono::DateTime<Utc>,
    last_activity_at: Option<chrono::DateTime<Utc>>,
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_actual_cost_usd: f64,
    session_total_tokens_sent: u64,
}

impl SavingsObservation {
    fn last_activity_at(&self) -> chrono::DateTime<Utc> {
        self.last_activity_at.unwrap_or(self.observed_at)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct DailySavingsBucket {
    estimated_savings_usd: f64,
    estimated_tokens_saved: u64,
    actual_cost_usd: f64,
    total_tokens_sent: u64,
    // New-input tokens (uncached + cache-write) inside the bucket, written
    // only by the session sampler. Remote rollups and buckets persisted by
    // older builds carry `total_tokens_sent` on the FULL-FORWARDED basis
    // (cache-polluted), so they leave this 0 = "no coverage" and the
    // new-input rate skips them instead of mixing denominators.
    new_input_tokens: u64,
    // Output-shaping layer, added after the compression fields existed: buckets
    // persisted by older builds must keep parsing, hence container `default`.
    output_savings_usd: f64,
    output_tokens_saved: u64,
    // Provider cache reads inside the bucket, copied from the backend-derived
    // deltas at ingest. The backend's raw history is a point-capped ring, so
    // the derivation goes None once a period's checkpoints age out; this
    // archived copy is what keeps the compressible-input rate computable
    // (and its chip visible) for old periods. None for buckets archived
    // before this field existed or observed only by the local tracker.
    cache_read_tokens: Option<u64>,
    cache_savings_usd: Option<f64>,
}

/// One bucket of the locally-sampled output-shaper series: poll-over-poll
/// deltas of the estimator's durable cumulative `tokens_saved` and
/// `baseline_tokens`. The backend's rollups carry no per-bucket baseline, so
/// this local series is the only source of a window-scoped output reduction.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct OutputSampleBucket {
    saved_tokens: u64,
    baseline_tokens: u64,
}

/// Semantics version of the sampled output series. The samples are deltas of
/// `crate::output_savings::estimate()`, so a sample is only comparable to
/// others taken under the same estimator rules. Bump this when those rules
/// change: the persisted *watermark pair* is then dropped once on load, so the
/// next poll reseeds against the new cumulative instead of parking above it
/// (the new number is smaller, and a mark left at the old one silences the
/// sampler until it re-climbs the difference — weeks, on a real ledger).
///
/// Recorded BUCKETS are deliberately kept across a bump. They are the history
/// the chart draws, and a percentage measured honestly under the rules of its
/// day is worth more than a gap: v1 dropped them and simply erased two weeks
/// of a user's output history. Only the seed pair is unit-sensitive.
/// Version history: 0 = pre-field (prefix-fallback lookup), 1 = exact-stratum
/// lookup with MIN_BASELINE_N (2026-08-22).
const OUTPUT_SAMPLE_SERIES_VERSION: u8 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PersistedSavingsState {
    // Container-level `default`: a field added (or removed) by another release
    // must never fail the whole parse — that used to silently wipe all
    // daily/hourly history and lifetime counters on upgrade/downgrade.
    schema_version: u8,
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_savings_pct: f64,
    lifetime_requests: usize,
    /// High-water mark of the lifetime token total (summed from `daily_savings`)
    /// at which token milestones were last fired. `None` on profiles written
    /// before this field existed, so milestones for already-earned savings are
    /// seeded (suppressed) on first load rather than firing all at once.
    lifetime_token_milestone_high_water: Option<u64>,
    /// Running total of tool-definition tokens the proxy deferred, accumulated
    /// from the poll-over-poll delta of a process-cumulative `/stats` counter
    /// the backend never persists itself.
    lifetime_tool_schema_tokens_saved: u64,
    last_observation: Option<SavingsObservation>,
    display_session_baseline: Option<SavingsObservation>,
    session_savings_history: Vec<HeadroomSavingsHistoryPoint>,
    /// Cumulative new-input (uncached + cache-write) sampled once per stats
    /// poll. Same shape as `session_savings_history`, but the `u64` holds
    /// tokens SENT, not saved. Diffed per hour to give the chart a real
    /// per-bucket denominator instead of smearing the session total across
    /// hours in proportion to savings.
    session_new_input_history: Vec<HeadroomSavingsHistoryPoint>,
    session_hourly_buckets: BTreeMap<String, DailySavingsBucket>,
    daily_savings: BTreeMap<String, DailySavingsBucket>,
    hourly_savings: BTreeMap<String, DailySavingsBucket>,
    /// Locally-sampled output-shaper deltas. Daily keys are UTC dates (they
    /// join onto the backend's UTC-bucketed daily rollups); hourly keys are
    /// local (`local_hour_key`, joining the local-keyed hourly points).
    output_daily_samples: BTreeMap<String, OutputSampleBucket>,
    output_hourly_samples: BTreeMap<String, OutputSampleBucket>,
    /// Locally-sampled tool-schema deferral deltas, keyed exactly like the
    /// output samples above. The backend exposes tool-schema savings ONLY as a
    /// lifetime cumulative counter: it is absent from the `history`
    /// checkpoints, from `series.hourly`, and from that series' `by_model`
    /// entries (verified against a 0.37.0 wheel, 2026-09-02). Sampling the
    /// deltas here is therefore the only per-bucket record of this layer that
    /// can exist, and it necessarily starts empty -- there is nothing to
    /// backfill from, so any chart that adds the layer steps up on the day
    /// sampling began and must say so rather than imply a real jump.
    tool_schema_daily_samples: BTreeMap<String, u64>,
    tool_schema_hourly_samples: BTreeMap<String, u64>,
    /// See [`OUTPUT_SAMPLE_SERIES_VERSION`]. Container default (0) marks a
    /// file written before the field existed, whose series is in old units.
    output_sample_series_version: u8,
    /// Last reading of the output shaper's durable estimator total. Cached so
    /// the lifetime headline can price this layer while the backend is still
    /// starting: without it, cold start falls back to the (understating)
    /// bucket sum and the total visibly dips for the first minutes.
    last_output_estimator_tokens_saved: Option<u64>,
    /// Companion baseline for the reading above. Persisted only so the next
    /// launch can seed its sampling watermark as a *pair* -- see
    /// `sample_output_reduction`.
    last_output_estimator_baseline_tokens: Option<u64>,
}

struct SavingsTracker {
    records_path: std::path::PathBuf,
    state_path: std::path::PathBuf,
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_savings_pct: f64,
    lifetime_requests: usize,
    /// High-water mark of the lifetime token total at which token milestones
    /// were last fired. Seeded from the current bucket sum on first load after
    /// upgrade so already-earned savings don't re-fire every milestone.
    lifetime_token_milestone_high_water: u64,
    /// See `PersistedSavingsState::lifetime_tool_schema_tokens_saved`.
    lifetime_tool_schema_tokens_saved: u64,
    /// Last raw reading of the backend's process-cumulative counter. `None`
    /// until the first poll seeds it: that first reading is a baseline, never a
    /// delta, so a backend that outlived the app can't be counted twice. A
    /// later reading below the watermark means a restart, which reseeds the
    /// same way. Deliberately not persisted -- it describes a backend process,
    /// not the user's history.
    tool_schema_process_total: Option<u64>,
    last_observation: Option<SavingsObservation>,
    display_session_baseline: Option<SavingsObservation>,
    session_savings_history: Vec<HeadroomSavingsHistoryPoint>,
    session_new_input_history: Vec<HeadroomSavingsHistoryPoint>,
    session_hourly_buckets: BTreeMap<String, DailySavingsBucket>,
    daily_savings: BTreeMap<String, DailySavingsBucket>,
    hourly_savings: BTreeMap<String, DailySavingsBucket>,
    /// See `PersistedSavingsState::output_daily_samples`.
    output_daily_samples: BTreeMap<String, OutputSampleBucket>,
    output_hourly_samples: BTreeMap<String, OutputSampleBucket>,
    /// See `PersistedSavingsState::tool_schema_daily_samples`.
    tool_schema_daily_samples: BTreeMap<String, u64>,
    tool_schema_hourly_samples: BTreeMap<String, u64>,
    /// High-water (tokens_saved, baseline_tokens) of the shaper's durable
    /// estimator. Not persisted directly: the first post-launch reading seeds
    /// it (never emitting a delta, so a closed-app gap isn't billed to the
    /// launch bucket), and it only ever advances -- see
    /// `sample_output_reduction` for why a dip must not move it down.
    output_sample_watermark: Option<(u64, u64)>,
    /// See `PersistedSavingsState::last_output_estimator_tokens_saved`.
    last_output_estimator_tokens_saved: Option<u64>,
    /// See `PersistedSavingsState::last_output_estimator_baseline_tokens`.
    last_output_estimator_baseline_tokens: Option<u64>,
    // Write throttle — only flush to disk at most once per minute
    last_written_at: Option<std::time::Instant>,
}

impl SavingsTracker {
    fn load_or_create(base_dir: &Path) -> Result<Self> {
        let records_path = telemetry_file(base_dir, "savings-records.jsonl");
        let state_path = config_file(base_dir, "savings-state.json");
        if !records_path.exists() {
            // Telemetry only — a full disk or locked Application Support must
            // not abort AppState::new() and crash-loop launch (Sentry RUST-1P).
            if let Err(err) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&records_path)
            {
                log::warn!(
                    "creating {} failed ({err}); savings records disabled",
                    records_path.display()
                );
            }
        }

        // A corrupt file must not brick launch, but it must also not be
        // silently replaced: back it up for recovery and say so in the log.
        let persisted_state = match load_persisted_savings_state(&state_path) {
            Ok(state) => state,
            Err(err) => {
                log::warn!("savings-state.json unreadable ({err}); backing up");
                let _ = std::fs::rename(&state_path, state_path.with_extension("json.corrupt"));
                None
            }
        }
        // Missing/corrupt/schema-mismatched state used to mean starting the
        // user's savings history from zero even though savings-records.jsonl
        // holds every observation delta — rebuild the buckets from it instead.
        // Approximate is fine: the backend's settled-day rollups overwrite
        // these keys on the next stats poll anyway.
        .or_else(|| {
            let rebuilt = rebuild_persisted_savings_from_records(&records_path);
            if rebuilt.is_some() {
                log::warn!(
                    "savings-state.json missing or unusable; rebuilt history from savings-records.jsonl"
                );
            }
            rebuilt
        });

        // Seed the milestone high-water from the persisted value, or (on first
        // load after upgrade) from the current bucket sum so already-earned
        // savings don't re-fire every milestone at once.
        let lifetime_token_milestone_high_water = persisted_state
            .as_ref()
            .and_then(|state| state.lifetime_token_milestone_high_water)
            .unwrap_or_else(|| {
                persisted_state.as_ref().map_or(0, |state| {
                    state
                        .daily_savings
                        .values()
                        .map(|bucket| bucket.estimated_tokens_saved)
                        .sum()
                })
            });

        // Sampled series recorded under older estimator semantics (see
        // OUTPUT_SAMPLE_SERIES_VERSION): completed days stay -- they are the
        // user's history -- but the watermark seed pair is dropped so the
        // first poll reseeds against the new, smaller cumulative.
        //
        // The bucket covering the moment of the bump is the exception. It is a
        // part-day of OLD-units samples that new-units samples are about to be
        // added to, so it represents nothing: on 2026-08-22 it left the day
        // chip reading 93% (pure pre-bump ping artifacts) while every other
        // number had moved to the new estimator. Dropping just that bucket
        // costs the current day's partial figure -- which the all-time
        // fallback covers -- and keeps every completed day.
        let output_series_current = persisted_state.as_ref().is_some_and(|state| {
            state.output_sample_series_version >= OUTPUT_SAMPLE_SERIES_VERSION
        });
        // Daily keys are UTC dates, hourly keys are local (see the field docs),
        // so the seam has to be named in each series' own timezone.
        let seam_day_utc = Utc::now().format("%Y-%m-%d").to_string();
        let seam_day_local = local_day_key(Local::now());
        if persisted_state.is_some() && !output_series_current {
            log::info!(
                "sampled output series predates estimator semantics v{OUTPUT_SAMPLE_SERIES_VERSION}; reseeding the watermark, dropping the {seam_day_utc} seam bucket, keeping completed days"
            );
        }

        let mut tracker = Self {
            records_path,
            state_path,
            session_requests: 0,
            session_estimated_savings_usd: 0.0,
            session_estimated_tokens_saved: 0,
            session_savings_pct: 0.0,
            lifetime_requests: persisted_state
                .as_ref()
                .map_or(0, |state| state.lifetime_requests),
            lifetime_token_milestone_high_water,
            lifetime_tool_schema_tokens_saved: persisted_state
                .as_ref()
                .map_or(0, |state| state.lifetime_tool_schema_tokens_saved),
            tool_schema_process_total: None,
            last_observation: persisted_state
                .as_ref()
                .and_then(|state| state.last_observation.clone()),
            display_session_baseline: persisted_state
                .as_ref()
                .and_then(|state| state.display_session_baseline.clone()),
            session_savings_history: persisted_state
                .as_ref()
                .map_or_else(Vec::new, |state| state.session_savings_history.clone()),
            session_new_input_history: persisted_state
                .as_ref()
                .map_or_else(Vec::new, |state| state.session_new_input_history.clone()),
            session_hourly_buckets: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| state.session_hourly_buckets.clone()),
            daily_savings: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| state.daily_savings.clone()),
            hourly_savings: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| state.hourly_savings.clone()),
            output_daily_samples: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| {
                    let mut samples = state.output_daily_samples.clone();
                    if !output_series_current {
                        samples.remove(&seam_day_utc);
                    }
                    samples
                }),
            output_hourly_samples: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| {
                    let mut samples = state.output_hourly_samples.clone();
                    if !output_series_current {
                        samples.retain(|key, _| !key.starts_with(&seam_day_local));
                    }
                    samples
                }),
            tool_schema_daily_samples: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| {
                    state.tool_schema_daily_samples.clone()
                }),
            tool_schema_hourly_samples: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| {
                    state.tool_schema_hourly_samples.clone()
                }),
            output_sample_watermark: None,
            last_output_estimator_tokens_saved: persisted_state
                .as_ref()
                .filter(|_| output_series_current)
                .and_then(|state| state.last_output_estimator_tokens_saved),
            last_output_estimator_baseline_tokens: persisted_state
                .as_ref()
                .filter(|_| output_series_current)
                .and_then(|state| state.last_output_estimator_baseline_tokens),
            last_written_at: None,
        };
        // Best-effort: persistence failing (ENOSPC/EACCES) degrades to
        // in-memory stats; it is retried on every observe tick anyway.
        if let Err(err) = tracker.persist_state() {
            // `{err:#}`: persist_state returns an anyhow chain, so plain Display
            // would bridge only the outer context to Sentry and hide the cause.
            log::warn!("initial savings-state persist failed: {err:#}");
        }
        Ok(tracker)
    }

    fn snapshot(&self) -> SavingsTotalsSnapshot {
        let baseline = self.display_session_baseline.as_ref();
        let session_requests = baseline.map_or(self.session_requests, |baseline| {
            self.session_requests
                .saturating_sub(baseline.session_requests)
        });
        let session_estimated_savings_usd =
            baseline.map_or(self.session_estimated_savings_usd, |baseline| {
                (self.session_estimated_savings_usd - baseline.session_estimated_savings_usd)
                    .max(0.0)
            });
        let session_estimated_tokens_saved =
            baseline.map_or(self.session_estimated_tokens_saved, |baseline| {
                self.session_estimated_tokens_saved
                    .saturating_sub(baseline.session_estimated_tokens_saved)
            });
        let session_savings_pct = if let Some(baseline) = baseline {
            let total_tokens_sent = self
                .last_observation
                .as_ref()
                .map(|observation| observation.session_total_tokens_sent)
                .unwrap_or(0)
                .saturating_sub(baseline.session_total_tokens_sent);
            let total_before = session_estimated_tokens_saved.saturating_add(total_tokens_sent);
            if total_before > 0 {
                session_estimated_tokens_saved as f64 / total_before as f64 * 100.0
            } else {
                0.0
            }
        } else {
            self.session_savings_pct
        };

        SavingsTotalsSnapshot {
            session_requests,
            session_estimated_savings_usd,
            session_estimated_tokens_saved,
            session_savings_pct,
            lifetime_requests: self.lifetime_requests,
        }
    }

    fn daily_savings(&self) -> Vec<DailySavingsPoint> {
        self.daily_savings
            .iter()
            .map(|(date, bucket)| DailySavingsPoint {
                date: date.clone(),
                estimated_savings_usd: bucket.estimated_savings_usd,
                estimated_tokens_saved: bucket.estimated_tokens_saved,
                tool_schema_savings_usd: 0.0,
                tool_schema_tokens_saved: 0,
                actual_cost_usd: bucket.actual_cost_usd,
                total_tokens_sent: bucket.total_tokens_sent,
                new_input_tokens: bucket.new_input_tokens,
                output_savings_usd: bucket.output_savings_usd,
                output_tokens_saved: bucket.output_tokens_saved,
                // Archived from the backend-derived deltas at ingest; None for
                // buckets the local tracker observed on its own.
                cache_read_tokens: bucket.cache_read_tokens,
                cache_savings_usd: bucket.cache_savings_usd,
                // Filled by the sampler overlay in build_dashboard.
                output_sampled_tokens_saved: None,
                output_baseline_tokens: None,
            })
            .collect()
    }

    fn hourly_savings(&self) -> Vec<HourlySavingsPoint> {
        self.hourly_savings
            .iter()
            .map(|(hour, bucket)| HourlySavingsPoint {
                hour: hour.clone(),
                estimated_savings_usd: bucket.estimated_savings_usd,
                estimated_tokens_saved: bucket.estimated_tokens_saved,
                tool_schema_savings_usd: 0.0,
                tool_schema_tokens_saved: 0,
                actual_cost_usd: bucket.actual_cost_usd,
                total_tokens_sent: bucket.total_tokens_sent,
                new_input_tokens: bucket.new_input_tokens,
                output_savings_usd: bucket.output_savings_usd,
                output_tokens_saved: bucket.output_tokens_saved,
                cache_read_tokens: bucket.cache_read_tokens,
                cache_savings_usd: bucket.cache_savings_usd,
                output_sampled_tokens_saved: None,
                output_baseline_tokens: None,
                // The local pre-cutoff tracker has no provider dimension.
                by_provider: Vec::new(),
            })
            .collect()
    }

    /// Fold the backend's authoritative rollups into the local archive so they
    /// survive its history trimming and fill gaps from periods the app wasn't
    /// running. Only settled days in `[cutoff_date, today_key)` are written:
    /// today's live buckets are left to `observe`, and pre-cutoff days are
    /// skipped (pre-v6 schema drift). Native values overwrite the tracker's own
    /// observed values for those keys, mirroring the display-time merge.
    /// Returns true if any bucket changed (caller should persist).
    fn ingest_native_rollups(
        &mut self,
        daily: &[DailySavingsPoint],
        hourly: &[HourlySavingsPoint],
        cutoff_date: &str,
        today_key: &str,
        utc_today_key: &str,
    ) -> bool {
        let cutoff_hour = format!("{cutoff_date}T00:00");
        let mut changed = false;
        for point in daily {
            // Daily rollups are UTC-day keyed; hourly keys below stay local and
            // are guarded by the local today_key.
            //
            // The live UTC day is archived too, as it accumulates. Waiting for
            // it to settle loses it outright at heavy volume: the backend keeps
            // 5000 history points, which can be under 24h, so by UTC midnight
            // the day's rollup has become the buffer's first (backfill) bucket
            // and drop_rollup_backfill discards it before we get here. That is
            // how 2026-08-13 collapsed from the backend's real total to the
            // $3.44 of traffic the local tracker had happened to observe.
            if point.date.as_str() < cutoff_date || point.date.as_str() > utc_today_key {
                continue;
            }
            // Only ever grow the live bucket. The tracker keys its own deltas by
            // LOCAL day, so ahead of UTC it already holds hours the backend's
            // UTC bucket has not reached; a mid-day snapshot must not shrink it.
            if point.date.as_str() == utc_today_key {
                if let Some(existing) = self.daily_savings.get(&point.date) {
                    if point.total_tokens_sent <= existing.total_tokens_sent {
                        continue;
                    }
                }
            }
            // A desynced backend rollup (compression savings but zero
            // tokens/cost, because its cost counter lags the savings
            // accumulator; see RUST-4S/3S/3V) must not clobber a local bucket
            // that recorded real spend that day. The display-time fallback in
            // merge_daily_savings relies on the tracker still holding that
            // ground truth; overwriting it here made the fallback fail and the
            // zero-spend anomaly probe fire.
            let history_desynced = point.estimated_savings_usd > 0.000_001
                && point.actual_cost_usd == 0.0
                && point.total_tokens_sent == 0;
            if history_desynced {
                if let Some(existing) = self.daily_savings.get(&point.date) {
                    if existing.total_tokens_sent > 0 {
                        continue;
                    }
                }
            }
            // Cache deltas are re-derived every poll from the backend's
            // point-capped (and compacted) checkpoint ring, so a settled day's
            // re-derivation only ever loses coverage relative to what was
            // archived while the day was live and complete: freeze at the
            // first archived value. The live UTC day keeps taking the fresh
            // derivation, which grows with the day.
            let archived = self.daily_savings.get(&point.date).copied();
            let archived_read = archived.and_then(|b| b.cache_read_tokens);
            let archived_usd = archived.and_then(|b| b.cache_savings_usd);
            let live_day = point.date.as_str() == utc_today_key;
            let bucket = DailySavingsBucket {
                estimated_savings_usd: point.estimated_savings_usd,
                estimated_tokens_saved: point.estimated_tokens_saved,
                actual_cost_usd: point.actual_cost_usd,
                total_tokens_sent: point.total_tokens_sent,
                // Remote rollups carry no new-input dimension; keep whatever
                // the local session sampler banked for this bucket.
                new_input_tokens: archived.map_or(0, |b| b.new_input_tokens),
                output_savings_usd: point.output_savings_usd,
                output_tokens_saved: point.output_tokens_saved,
                cache_read_tokens: if live_day {
                    point.cache_read_tokens.or(archived_read)
                } else {
                    archived_read.or(point.cache_read_tokens)
                },
                cache_savings_usd: if live_day {
                    point.cache_savings_usd.or(archived_usd)
                } else {
                    archived_usd.or(point.cache_savings_usd)
                },
            };
            if archived.as_ref() != Some(&bucket) {
                self.daily_savings.insert(point.date.clone(), bucket);
                changed = true;
            }
        }
        for point in hourly {
            if point.hour.as_str() < cutoff_hour.as_str()
                || day_key_from_hour_key(&point.hour).as_str() >= today_key
            {
                continue;
            }
            // Hourly ingest only ever sees settled hours, so freeze cache
            // coverage at the first archived value (see the daily loop above).
            let archived = self.hourly_savings.get(&point.hour).copied();
            let bucket = DailySavingsBucket {
                estimated_savings_usd: point.estimated_savings_usd,
                estimated_tokens_saved: point.estimated_tokens_saved,
                actual_cost_usd: point.actual_cost_usd,
                total_tokens_sent: point.total_tokens_sent,
                // Remote rollups carry no new-input dimension; keep whatever
                // the local session sampler banked for this bucket.
                new_input_tokens: archived.map_or(0, |b| b.new_input_tokens),
                output_savings_usd: point.output_savings_usd,
                output_tokens_saved: point.output_tokens_saved,
                cache_read_tokens: archived
                    .and_then(|b| b.cache_read_tokens)
                    .or(point.cache_read_tokens),
                cache_savings_usd: archived
                    .and_then(|b| b.cache_savings_usd)
                    .or(point.cache_savings_usd),
            };
            if archived.as_ref() != Some(&bucket) {
                self.hourly_savings.insert(point.hour.clone(), bucket);
                changed = true;
            }
        }
        changed
    }

    /// Advance the milestone high-water mark to `total` and return any lifetime
    /// token milestones crossed in the process. Never fires on a decrease, so a
    /// backend re-roll that lowers a day's bucket can't double-fire.
    fn note_lifetime_token_total(&mut self, total: u64) -> Vec<u64> {
        if total <= self.lifetime_token_milestone_high_water {
            return Vec::new();
        }
        let crossed =
            lifetime_token_milestones_crossed(self.lifetime_token_milestone_high_water, total);
        self.lifetime_token_milestone_high_water = total;
        crossed
    }

    /// Fold this poll's reading of the backend's process-cumulative
    /// tool-schema counter into the lifetime total.
    ///
    /// The backend reports this layer in `/stats` but never writes it to the
    /// rollups, so unlike compression there is no durable server-side total to
    /// read -- accumulating the deltas here is the only record of it. The first
    /// reading of any backend process is a baseline, so a backend that was
    /// already running (or that restarted and reset its counter) contributes
    /// only what it does from that point on. That under-counts by at most one
    /// poll interval, which is the safe direction.
    fn accumulate_tool_schema_tokens(&mut self, reading: u64) {
        let delta = match self.tool_schema_process_total {
            Some(previous) => reading.saturating_sub(previous),
            None => 0,
        };
        self.tool_schema_process_total = Some(reading);
        self.lifetime_tool_schema_tokens_saved =
            self.lifetime_tool_schema_tokens_saved.saturating_add(delta);
        if delta > 0 {
            // Same keying as the output samples: UTC day (joins the backend's
            // UTC daily rollups), local hour (joins the local-keyed hourly
            // points). Attributed to the moment of observation, so a delta
            // spanning a bucket edge lands wholly in the later bucket -- the
            // same approximation the output sampler makes.
            let now_utc = Utc::now();
            let day_key = now_utc.format("%Y-%m-%d").to_string();
            let hour_key = local_hour_key(now_utc.with_timezone(&Local));
            *self.tool_schema_daily_samples.entry(day_key).or_insert(0) += delta;
            *self.tool_schema_hourly_samples.entry(hour_key).or_insert(0) += delta;
        }
    }

    fn observe(&mut self, stats: &HeadroomDashboardStats) -> Option<SavingsTotalsSnapshot> {
        if let Some(reading) = stats.tool_schema_tokens_saved {
            self.accumulate_tool_schema_tokens(reading);
        }
        // Sampled from the locally recomputed estimate ONLY, never the
        // backend's `/stats` figure: its global-mean credit is what the
        // recompute exists to remove, and sampling it as a fallback booked an
        // Opus-mean "reduction" into the daily buckets of exactly the machines
        // the recompute refuses to score (read as "Output -100%" on an
        // all-codex Windows machine, 0.9.7-rc.7). A ledger with no evidence
        // just skips this poll's sample; a readable ledger that scores nothing
        // also convicts every sample the retired fallback recorded -- see
        // `drop_unscoreable_output_samples`.
        match crate::output_savings::estimate() {
            crate::output_savings::LedgerEstimate::Scored(e) => {
                self.sample_output_reduction(Some((e.tokens_saved, e.baseline_tokens)));
            }
            crate::output_savings::LedgerEstimate::Unscored => {
                self.drop_unscoreable_output_samples();
            }
            crate::output_savings::LedgerEstimate::NoEvidence => {}
        }
        let session_tokens_saved = stats.session_estimated_tokens_saved?;
        let session_savings_usd = stats.session_estimated_savings_usd.unwrap_or(0.0).max(0.0);
        let session_requests = stats.session_requests.unwrap_or(0);
        let session_total_tokens_sent = stats.session_total_tokens_sent;
        let session_actual_cost_usd = stats.session_actual_cost_usd.map(|value| value.max(0.0));
        let first_observation = self.last_observation.is_none();
        let previous = self.last_observation.clone();
        let requests_went_back = previous.as_ref().is_some_and(|prev| {
            stats.session_requests.is_some() && session_requests < prev.session_requests
        });
        let reset_detected = previous.as_ref().is_some_and(|prev| {
            session_tokens_saved < prev.session_estimated_tokens_saved
                || session_total_tokens_sent.is_some_and(|value| {
                    prev.session_total_tokens_sent > 0 && value < prev.session_total_tokens_sent
                })
                || session_actual_cost_usd.is_some_and(|value| {
                    prev.session_actual_cost_usd > 0.0
                        && value + 0.000_001 < prev.session_actual_cost_usd
                })
                || requests_went_back
        });
        let rollover_display_session = previous.as_ref().is_some_and(|prev| {
            should_rollover_display_session(prev.last_activity_at(), Utc::now())
        });

        let (
            delta_requests,
            delta_usd,
            delta_tokens,
            delta_actual_cost_usd,
            delta_total_tokens_sent,
        ) = if let Some(prev) = previous.as_ref() {
            if reset_detected {
                (
                    session_requests,
                    session_savings_usd,
                    session_tokens_saved,
                    session_actual_cost_usd.unwrap_or(0.0),
                    session_total_tokens_sent.unwrap_or(0),
                )
            } else {
                (
                    session_requests.saturating_sub(prev.session_requests),
                    (session_savings_usd - prev.session_estimated_savings_usd).max(0.0),
                    session_tokens_saved.saturating_sub(prev.session_estimated_tokens_saved),
                    session_actual_cost_usd.map_or(0.0, |value| {
                        if prev.session_actual_cost_usd > 0.0 {
                            (value - prev.session_actual_cost_usd).max(0.0)
                        } else {
                            0.0
                        }
                    }),
                    session_total_tokens_sent.map_or(0, |value| {
                        if prev.session_total_tokens_sent > 0 {
                            value.saturating_sub(prev.session_total_tokens_sent)
                        } else {
                            0
                        }
                    }),
                )
            }
        } else {
            (
                session_requests,
                session_savings_usd,
                session_tokens_saved,
                session_actual_cost_usd.unwrap_or(0.0),
                session_total_tokens_sent.unwrap_or(0),
            )
        };
        if reset_detected {
            self.session_savings_history.clear();
            self.session_new_input_history.clear();
        }
        self.session_savings_history =
            merge_session_savings_history(&self.session_savings_history, &stats.savings_history);
        // Sample the cumulative new-input scalar into its own series, pinned to
        // the proxy's latest request timestamp so it buckets alongside the saved
        // series. One point per poll: between polls the proxy may have handled
        // several requests, so a hop across an hour boundary lumps that gap's
        // sent tokens into the later hour (sub-poll fuzz; the saved numerator
        // stays exact because the proxy reports it per request).
        if let Some(sent_total) = session_total_tokens_sent {
            let sampled_at = stats
                .savings_history
                .last()
                .map(|point| point.timestamp)
                .unwrap_or_else(Utc::now);
            self.session_new_input_history = merge_session_savings_history(
                &self.session_new_input_history,
                &[HeadroomSavingsHistoryPoint {
                    timestamp: sampled_at,
                    total_tokens_saved: sent_total,
                }],
            );
        }

        let previous_session_hourly_buckets = self.session_hourly_buckets.clone();
        let current_session_hourly_buckets = derive_session_hourly_buckets(
            stats,
            &self.session_savings_history,
            &self.session_new_input_history,
        );
        let current_session_hourly_buckets_map = current_session_hourly_buckets
            .iter()
            .cloned()
            .collect::<BTreeMap<_, _>>();
        let session_buckets_changed = !current_session_hourly_buckets.is_empty()
            && current_session_hourly_buckets_map != previous_session_hourly_buckets;
        let delta_hourly_buckets = if first_observation || reset_detected {
            current_session_hourly_buckets.clone()
        } else {
            diff_hourly_buckets(
                &previous_session_hourly_buckets,
                &current_session_hourly_buckets,
            )
        };

        self.session_requests = session_requests;
        self.session_estimated_savings_usd = session_savings_usd;
        self.session_estimated_tokens_saved = session_tokens_saved;
        self.session_savings_pct = stats.session_savings_pct.unwrap_or(0.0);
        if reset_detected {
            self.display_session_baseline = None;
        } else if rollover_display_session {
            self.display_session_baseline = previous.clone();
        }

        let changed = delta_requests > 0
            || delta_tokens > 0
            || delta_total_tokens_sent > 0
            || delta_usd > 0.000_001
            || delta_actual_cost_usd > 0.000_001
            || session_buckets_changed;
        if delta_requests > 0 || delta_tokens > 0 || delta_usd > 0.0 {
            self.lifetime_requests = self.lifetime_requests.saturating_add(delta_requests);
        }

        let baseline_hourly_buckets = if (first_observation || reset_detected)
            && (session_requests > 0
                || session_tokens_saved > 0
                || session_savings_usd > 0.0
                || session_total_tokens_sent.unwrap_or(0) > 0
                || session_actual_cost_usd.unwrap_or(0.0) > 0.0)
        {
            self.ingest_hourly_buckets(&current_session_hourly_buckets);
            current_session_hourly_buckets.clone()
        } else {
            Vec::new()
        };
        if !first_observation && !reset_detected && session_buckets_changed {
            self.replace_session_hourly_buckets(
                &previous_session_hourly_buckets,
                &current_session_hourly_buckets,
            );
        }
        if first_observation || reset_detected {
            self.session_hourly_buckets = current_session_hourly_buckets_map;
        } else if session_buckets_changed {
            self.session_hourly_buckets = current_session_hourly_buckets_map;
        }
        if reset_detected && current_session_hourly_buckets.is_empty() {
            self.session_hourly_buckets.clear();
        }

        self.last_observation = Some(SavingsObservation {
            session_requests,
            session_estimated_savings_usd: session_savings_usd,
            session_estimated_tokens_saved: session_tokens_saved,
            observed_at: Utc::now(),
            last_activity_at: Some(if changed {
                Utc::now()
            } else {
                previous
                    .as_ref()
                    .map(|prev| prev.last_activity_at())
                    .unwrap_or_else(Utc::now)
            }),
            session_actual_cost_usd: session_actual_cost_usd.unwrap_or(
                previous
                    .as_ref()
                    .map_or(0.0, |prev| prev.session_actual_cost_usd),
            ),
            session_total_tokens_sent: session_total_tokens_sent.unwrap_or(
                previous
                    .as_ref()
                    .map_or(0, |prev| prev.session_total_tokens_sent),
            ),
        });

        let now = std::time::Instant::now();
        let has_any_value = session_requests > 0
            || session_tokens_saved > 0
            || session_savings_usd > 0.0
            || session_total_tokens_sent.unwrap_or(0) > 0
            || session_actual_cost_usd.unwrap_or(0.0) > 0.0;
        let should_write = has_any_value
            && (first_observation
                || reset_detected
                || (changed
                    && self
                        .last_written_at
                        .map_or(true, |t| now.duration_since(t).as_secs() >= 60)));
        if should_write {
            self.last_written_at = Some(now);
            if first_observation || reset_detected {
                for record in build_hourly_backfill_records(
                    &baseline_hourly_buckets,
                    session_requests,
                    session_savings_usd,
                    session_tokens_saved,
                    session_actual_cost_usd.unwrap_or(0.0),
                    session_total_tokens_sent.unwrap_or(0),
                ) {
                    let _ = self.append_record(&record);
                }
            } else {
                if baseline_hourly_buckets.is_empty()
                    && delta_requests == 0
                    && delta_hourly_buckets.is_empty()
                {
                } else if baseline_hourly_buckets.is_empty() {
                    let record = SavingsRecord {
                        schema_version: 7,
                        id: Uuid::new_v4().to_string(),
                        observed_at: Utc::now(),
                        day_key: local_day_key(Local::now()),
                        hour_key: local_hour_key(Local::now()),
                        session_requests,
                        session_estimated_savings_usd: session_savings_usd,
                        session_estimated_tokens_saved: session_tokens_saved,
                        session_actual_cost_usd: session_actual_cost_usd.unwrap_or(0.0),
                        session_total_tokens_sent: session_total_tokens_sent.unwrap_or(0),
                        delta_requests,
                        delta_estimated_savings_usd: 0.0,
                        delta_estimated_tokens_saved: 0,
                        delta_actual_cost_usd: 0.0,
                        delta_total_tokens_sent: 0,
                        source: "headroom_dashboard".into(),
                    };
                    let _ = self.append_record(&record);
                } else {
                    for record in build_hourly_delta_records(
                        &baseline_hourly_buckets,
                        session_requests,
                        session_savings_usd,
                        session_tokens_saved,
                        session_actual_cost_usd.unwrap_or(0.0),
                        session_total_tokens_sent.unwrap_or(0),
                        delta_requests,
                    ) {
                        let _ = self.append_record(&record);
                    }
                }
            }
        }
        let _ = self.persist_state();

        Some(self.snapshot())
    }

    fn ingest_hourly_buckets(&mut self, buckets: &[(String, DailySavingsBucket)]) {
        for (hour_key, bucket) in buckets {
            self.add_hourly_delta(
                hour_key,
                bucket.estimated_savings_usd,
                bucket.estimated_tokens_saved,
                bucket.actual_cost_usd,
                bucket.total_tokens_sent,
                bucket.new_input_tokens,
            );
            self.add_daily_delta(
                &day_key_from_hour_key(hour_key),
                bucket.estimated_savings_usd,
                bucket.estimated_tokens_saved,
                bucket.actual_cost_usd,
                bucket.total_tokens_sent,
                bucket.new_input_tokens,
            );
        }
    }

    fn replace_session_hourly_buckets(
        &mut self,
        previous: &BTreeMap<String, DailySavingsBucket>,
        current: &[(String, DailySavingsBucket)],
    ) {
        for (hour_key, bucket) in previous {
            self.subtract_hourly_delta(
                hour_key,
                bucket.estimated_savings_usd,
                bucket.estimated_tokens_saved,
                bucket.actual_cost_usd,
                bucket.total_tokens_sent,
                bucket.new_input_tokens,
            );
            self.subtract_daily_delta(
                &day_key_from_hour_key(hour_key),
                bucket.estimated_savings_usd,
                bucket.estimated_tokens_saved,
                bucket.actual_cost_usd,
                bucket.total_tokens_sent,
                bucket.new_input_tokens,
            );
        }
        self.ingest_hourly_buckets(current);
    }

    fn add_daily_delta(
        &mut self,
        day_key: &str,
        usd: f64,
        tokens: u64,
        actual_cost_usd: f64,
        total_tokens_sent: u64,
        new_input_tokens: u64,
    ) {
        if usd <= 0.0 && tokens == 0 && actual_cost_usd <= 0.0 && total_tokens_sent == 0 {
            return;
        }
        let entry = self.daily_savings.entry(day_key.to_string()).or_default();
        entry.estimated_savings_usd += usd.max(0.0);
        entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_add(tokens);
        entry.actual_cost_usd += actual_cost_usd.max(0.0);
        entry.total_tokens_sent = entry.total_tokens_sent.saturating_add(total_tokens_sent);
        entry.new_input_tokens = entry.new_input_tokens.saturating_add(new_input_tokens);
    }

    fn subtract_daily_delta(
        &mut self,
        day_key: &str,
        usd: f64,
        tokens: u64,
        actual_cost_usd: f64,
        total_tokens_sent: u64,
        new_input_tokens: u64,
    ) {
        let mut should_remove = false;
        if let Some(entry) = self.daily_savings.get_mut(day_key) {
            entry.estimated_savings_usd = (entry.estimated_savings_usd - usd.max(0.0)).max(0.0);
            entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_sub(tokens);
            entry.actual_cost_usd = (entry.actual_cost_usd - actual_cost_usd.max(0.0)).max(0.0);
            entry.total_tokens_sent = entry.total_tokens_sent.saturating_sub(total_tokens_sent);
            entry.new_input_tokens = entry.new_input_tokens.saturating_sub(new_input_tokens);
            should_remove = entry.estimated_savings_usd <= 0.0
                && entry.estimated_tokens_saved == 0
                && entry.actual_cost_usd <= 0.0
                && entry.total_tokens_sent == 0;
        }
        if should_remove {
            self.daily_savings.remove(day_key);
        }
    }

    fn add_hourly_delta(
        &mut self,
        hour_key: &str,
        usd: f64,
        tokens: u64,
        actual_cost_usd: f64,
        total_tokens_sent: u64,
        new_input_tokens: u64,
    ) {
        if usd <= 0.0 && tokens == 0 && actual_cost_usd <= 0.0 && total_tokens_sent == 0 {
            return;
        }
        let entry = self.hourly_savings.entry(hour_key.to_string()).or_default();
        entry.estimated_savings_usd += usd.max(0.0);
        entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_add(tokens);
        entry.actual_cost_usd += actual_cost_usd.max(0.0);
        entry.total_tokens_sent = entry.total_tokens_sent.saturating_add(total_tokens_sent);
        entry.new_input_tokens = entry.new_input_tokens.saturating_add(new_input_tokens);
    }

    fn subtract_hourly_delta(
        &mut self,
        hour_key: &str,
        usd: f64,
        tokens: u64,
        actual_cost_usd: f64,
        total_tokens_sent: u64,
        new_input_tokens: u64,
    ) {
        let mut should_remove = false;
        if let Some(entry) = self.hourly_savings.get_mut(hour_key) {
            entry.estimated_savings_usd = (entry.estimated_savings_usd - usd.max(0.0)).max(0.0);
            entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_sub(tokens);
            entry.actual_cost_usd = (entry.actual_cost_usd - actual_cost_usd.max(0.0)).max(0.0);
            entry.total_tokens_sent = entry.total_tokens_sent.saturating_sub(total_tokens_sent);
            entry.new_input_tokens = entry.new_input_tokens.saturating_sub(new_input_tokens);
            should_remove = entry.estimated_savings_usd <= 0.0
                && entry.estimated_tokens_saved == 0
                && entry.actual_cost_usd <= 0.0
                && entry.total_tokens_sent == 0;
        }
        if should_remove {
            self.hourly_savings.remove(hour_key);
        }
    }

    fn append_record(&self, record: &SavingsRecord) -> Result<()> {
        // Append-only and never read back, so unrotated it grows ~50-100 MB/yr
        // on heavy use. Rotate at 10 MB, keeping one generation for recovery.
        // ponytail: single .1 generation; add numbered rotation if the archive
        // ever gains a reader.
        const MAX_RECORDS_BYTES: u64 = 10 * 1024 * 1024;
        if std::fs::metadata(&self.records_path)
            .map(|m| m.len() > MAX_RECORDS_BYTES)
            .unwrap_or(false)
        {
            let rotated = self.records_path.with_extension("jsonl.1");
            let _ = std::fs::rename(&self.records_path, rotated);
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.records_path)
            .with_context(|| format!("opening {}", self.records_path.display()))?;
        let serialized = serde_json::to_string(record).context("serializing savings record")?;
        use std::io::Write;
        file.write_all(serialized.as_bytes())
            .with_context(|| format!("writing {}", self.records_path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("writing {}", self.records_path.display()))?;
        Ok(())
    }

    /// Bucket the poll-over-poll delta of the output shaper's durable
    /// cumulative counters into the local day/hour sample maps. Between polls
    /// several requests may land; the whole gap is attributed to the sampling
    /// moment (same tradeoff as `session_new_input_history`).
    ///
    /// The invariant is that a counter that goes backwards may cost us a
    /// sample but must never manufacture one. Two ways it goes backwards:
    ///
    /// - The backend restarted onto a lagging durable checkpoint, so it
    ///   re-earns ground already banked. Holding the mark makes the catch-up
    ///   free, which is right — it was counted the first time.
    /// - The estimator was genuinely wiped and restarts near zero. Its climb
    ///   is real new work, so the mark has to rebase or the sampler goes
    ///   silent forever.
    ///
    /// Seeding matters as much as the dip. A fresh launch seeds from the
    /// higher of the live reading and the last persisted one: seeding on a
    /// regressed reading alone bills the backend's entire catch-up climb to
    /// the launch bucket (2026-08-17: 906k phantom saved tokens, ~2.8x, from
    /// one backend restart 15s after the app started).
    fn sample_output_reduction(&mut self, current: Option<(u64, u64)>) {
        let Some(current) = current else {
            return;
        };
        let previous = self.output_sample_watermark;
        // Read before the writes below clobber them: on the first poll of a
        // launch these still hold the *previous* run's last reading, which is
        // exactly what the seed needs.
        let persisted = (
            self.last_output_estimator_tokens_saved.unwrap_or(0),
            self.last_output_estimator_baseline_tokens.unwrap_or(0),
        );
        // Cache the raw reading as-is (not a high-water mark): the cold-start
        // fallback must mirror what the live path would show, including the
        // "re-seeded estimator below the bucket sum" case its caller guards.
        self.last_output_estimator_tokens_saved = Some(current.0);
        self.last_output_estimator_baseline_tokens = Some(current.1);

        let Some((prev_saved, prev_baseline)) = previous else {
            // First reading of this launch: seed, never emit. The gap since
            // the last run belongs to no bucket we can name.
            self.output_sample_watermark =
                Some((current.0.max(persisted.0), current.1.max(persisted.1)));
            return;
        };

        if current.0 < prev_saved || current.1 < prev_baseline {
            // ponytail: "wiped" = fell below half the mark. A lagging
            // checkpoint dips by a poll's worth of work; a wipe drops to ~0.
            // Tighten if a real reset ever lands shallower than that.
            let wiped = current.0 < prev_saved / 2;
            self.output_sample_watermark = Some(if wiped {
                current
            } else {
                (prev_saved, prev_baseline)
            });
            return;
        }

        self.output_sample_watermark = Some(current);
        let delta_saved = current.0 - prev_saved;
        let delta_baseline = current.1 - prev_baseline;
        if delta_saved == 0 && delta_baseline == 0 {
            return;
        }
        let now_utc = Utc::now();
        let now_local = now_utc.with_timezone(&Local);
        // Daily keys are UTC to join the backend's UTC-bucketed daily rollups;
        // hourly keys are local to join the local-keyed hourly points.
        let day_key = now_utc.format("%Y-%m-%d").to_string();
        let hour_key = local_hour_key(now_local);
        for (map, key) in [
            (&mut self.output_daily_samples, day_key),
            (&mut self.output_hourly_samples, hour_key),
        ] {
            let entry = map.entry(key).or_default();
            entry.saved_tokens += delta_saved;
            entry.baseline_tokens += delta_baseline;
        }
    }

    /// A readable ledger that scores no strata convicts this machine's entire
    /// sampled output series. Scoreability only grows -- the verbosity
    /// baseline is seeded once and never relearned, control accumulators only
    /// accumulate, and the qualifying thresholds are constants -- so a machine
    /// unscoreable today was unscoreable when every existing bucket was
    /// recorded, and an unscoreable local estimate never emits samples. The
    /// only possible source is therefore the backend-figure fallback that
    /// shipped through 0.9.7-rc (global-mean credit; the Windows "Output
    /// -100%" chip), so the buckets are dropped rather than kept as history.
    /// The cold-start estimator cache and the watermark go with them: both
    /// were seeded from the same credited cumulative, and a mark parked at
    /// that larger figure would silence the sampler long after the control
    /// arm makes this machine scoreable. Runs every poll; a no-op once clean.
    fn drop_unscoreable_output_samples(&mut self) {
        if self.output_daily_samples.is_empty()
            && self.output_hourly_samples.is_empty()
            && self.last_output_estimator_tokens_saved.is_none()
            && self.last_output_estimator_baseline_tokens.is_none()
            && self.output_sample_watermark.is_none()
        {
            return;
        }
        log::info!(
            "output ledger scores no strata; dropping {} daily / {} hourly sampled buckets recorded by the retired backend-figure fallback",
            self.output_daily_samples.len(),
            self.output_hourly_samples.len()
        );
        self.output_daily_samples.clear();
        self.output_hourly_samples.clear();
        self.last_output_estimator_tokens_saved = None;
        self.last_output_estimator_baseline_tokens = None;
        self.output_sample_watermark = None;
    }

    fn persisted_state(&self) -> PersistedSavingsState {
        PersistedSavingsState {
            schema_version: 3,
            session_requests: self.session_requests,
            session_estimated_savings_usd: self.session_estimated_savings_usd,
            session_estimated_tokens_saved: self.session_estimated_tokens_saved,
            session_savings_pct: self.session_savings_pct,
            lifetime_requests: self.lifetime_requests,
            lifetime_token_milestone_high_water: Some(self.lifetime_token_milestone_high_water),
            lifetime_tool_schema_tokens_saved: self.lifetime_tool_schema_tokens_saved,
            last_observation: self.last_observation.clone(),
            display_session_baseline: self.display_session_baseline.clone(),
            session_savings_history: self.session_savings_history.clone(),
            session_new_input_history: self.session_new_input_history.clone(),
            session_hourly_buckets: self.session_hourly_buckets.clone(),
            daily_savings: self.daily_savings.clone(),
            hourly_savings: self.hourly_savings.clone(),
            output_daily_samples: self.output_daily_samples.clone(),
            output_hourly_samples: self.output_hourly_samples.clone(),
            tool_schema_daily_samples: self.tool_schema_daily_samples.clone(),
            tool_schema_hourly_samples: self.tool_schema_hourly_samples.clone(),
            last_output_estimator_tokens_saved: self.last_output_estimator_tokens_saved,
            last_output_estimator_baseline_tokens: self.last_output_estimator_baseline_tokens,
            output_sample_series_version: OUTPUT_SAMPLE_SERIES_VERSION,
        }
    }

    /// Drop hourly buckets older than this many days before persisting. The
    /// dashboard's hourly charts only look back days, not months, while the
    /// map otherwise grows by up to 24 keys/day forever — and the whole file
    /// is rewritten on every observe tick, so its size is a per-minute I/O
    /// cost. Daily buckets are kept indefinitely (365/year is nothing).
    const HOURLY_RETENTION_DAYS: i64 = 30;

    fn prune_hourly_savings(&mut self) {
        // Anchor retention to the newest bucket rather than the wall clock so
        // a returning user's charts don't vanish before new data arrives.
        let latest_day = self
            .hourly_savings
            .keys()
            .chain(self.session_hourly_buckets.keys())
            .filter_map(|key| key.get(..10))
            .max()
            .and_then(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok());
        let Some(latest_day) = latest_day else {
            return;
        };
        let cutoff = (latest_day - chrono::Duration::days(Self::HOURLY_RETENTION_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        // Keys are "YYYY-MM-DDTHH:00", so day-key prefix comparison is date order.
        self.hourly_savings
            .retain(|key, _| key.as_str() >= cutoff.as_str());
        self.session_hourly_buckets
            .retain(|key, _| key.as_str() >= cutoff.as_str());
        self.output_hourly_samples
            .retain(|key, _| key.as_str() >= cutoff.as_str());
        self.tool_schema_hourly_samples
            .retain(|key, _| key.as_str() >= cutoff.as_str());
    }

    fn persist_state(&mut self) -> Result<()> {
        self.prune_hourly_savings();
        // Compact (not pretty) JSON: this is a machine-read file rewritten on
        // every observe tick; pretty-printing roughly doubled the write.
        let serialized =
            serde_json::to_vec(&self.persisted_state()).context("serializing savings state")?;
        // Temp+rename: a crash/power loss mid-write used to leave truncated
        // JSON that the next launch silently replaced with a fresh tracker.
        crate::client_adapters::atomic_write(&self.state_path, &serialized)
            .with_context(|| format!("writing {}", self.state_path.display()))?;
        Ok(())
    }
}

/// The Monday of the week that contains `d`, or `d` itself if it already is
/// a Monday. Used by the weekly recap: the recap for `d` covers the 7 days
/// ending the day before this Monday.
fn most_recent_monday(d: chrono::NaiveDate) -> chrono::NaiveDate {
    let days_past = d.weekday().num_days_from_monday() as u64;
    d.checked_sub_days(chrono::Days::new(days_past))
        .unwrap_or(d)
}

fn aggregate_weekly_totals(
    daily_savings: &BTreeMap<String, DailySavingsBucket>,
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
) -> WeeklyTotals {
    let start_key = start.format("%Y-%m-%d").to_string();
    let end_key = end.format("%Y-%m-%d").to_string();
    let mut total_tokens_saved: u64 = 0;
    let mut total_savings_usd: f64 = 0.0;
    let mut active_days: u32 = 0;
    for (day_key, bucket) in daily_savings.range(start_key..=end_key) {
        let has_activity = bucket.estimated_tokens_saved > 0 || bucket.estimated_savings_usd > 0.0;
        if has_activity {
            active_days += 1;
        }
        total_tokens_saved = total_tokens_saved.saturating_add(bucket.estimated_tokens_saved);
        total_savings_usd += bucket.estimated_savings_usd;
        let _ = day_key;
    }
    WeeklyTotals {
        total_tokens_saved,
        total_savings_usd,
        active_days,
    }
}

fn lifetime_token_milestones_crossed(previous_total: u64, current_total: u64) -> Vec<u64> {
    if current_total <= previous_total {
        return Vec::new();
    }

    let mut milestones = FIRST_LIFETIME_TOKEN_MILESTONES
        .into_iter()
        .filter(|threshold| previous_total < *threshold && current_total >= *threshold)
        .collect::<Vec<_>>();

    let first_repeating_index = previous_total / REPEATING_LIFETIME_TOKEN_MILESTONE_STEP + 1;
    let last_repeating_index = current_total / REPEATING_LIFETIME_TOKEN_MILESTONE_STEP;
    for index in first_repeating_index..=last_repeating_index {
        milestones.push(index.saturating_mul(REPEATING_LIFETIME_TOKEN_MILESTONE_STEP));
    }

    milestones
}

/// Rebuild a best-effort `PersistedSavingsState` from the append-only
/// savings-records.jsonl (current + one rotated generation) by summing each
/// record's observation deltas into day/hour buckets. Used when
/// savings-state.json is missing, corrupt, or schema-mismatched. Session
/// state is not recoverable (and doesn't matter across a restart); the
/// milestone high-water is seeded from the rebuilt total so already-earned
/// milestones don't re-fire.
fn rebuild_persisted_savings_from_records(records_path: &Path) -> Option<PersistedSavingsState> {
    let mut daily: BTreeMap<String, DailySavingsBucket> = BTreeMap::new();
    let mut hourly: BTreeMap<String, DailySavingsBucket> = BTreeMap::new();
    let mut lifetime_requests: usize = 0;
    let mut any = false;

    // Rotated generation first (older records), then the live file.
    for path in [
        records_path.with_extension("jsonl.1"),
        records_path.to_path_buf(),
    ] {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            let Ok(record) = serde_json::from_str::<SavingsRecord>(line) else {
                continue; // tolerate a torn tail line or legacy garbage
            };
            // Pre-v5 records lack hour keys and use older delta semantics.
            if record.schema_version < 5 || record.day_key.is_empty() {
                continue;
            }
            any = true;
            lifetime_requests = lifetime_requests.saturating_add(record.delta_requests);
            let bucket = daily.entry(record.day_key.clone()).or_default();
            bucket.estimated_savings_usd += record.delta_estimated_savings_usd.max(0.0);
            bucket.estimated_tokens_saved = bucket
                .estimated_tokens_saved
                .saturating_add(record.delta_estimated_tokens_saved);
            bucket.actual_cost_usd += record.delta_actual_cost_usd.max(0.0);
            bucket.total_tokens_sent = bucket
                .total_tokens_sent
                .saturating_add(record.delta_total_tokens_sent);
            if !record.hour_key.is_empty() {
                let bucket = hourly.entry(record.hour_key.clone()).or_default();
                bucket.estimated_savings_usd += record.delta_estimated_savings_usd.max(0.0);
                bucket.estimated_tokens_saved = bucket
                    .estimated_tokens_saved
                    .saturating_add(record.delta_estimated_tokens_saved);
                bucket.actual_cost_usd += record.delta_actual_cost_usd.max(0.0);
                bucket.total_tokens_sent = bucket
                    .total_tokens_sent
                    .saturating_add(record.delta_total_tokens_sent);
            }
        }
    }

    if !any {
        return None;
    }
    let rebuilt_token_total: u64 = daily.values().map(|b| b.estimated_tokens_saved).sum();
    Some(PersistedSavingsState {
        schema_version: 3,
        lifetime_requests,
        lifetime_token_milestone_high_water: Some(rebuilt_token_total),
        daily_savings: daily,
        hourly_savings: hourly,
        ..Default::default()
    })
}

fn load_persisted_savings_state(path: &Path) -> Result<Option<PersistedSavingsState>> {
    if !path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let persisted = serde_json::from_slice::<PersistedSavingsState>(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    if persisted.schema_version == 3 {
        Ok(Some(persisted))
    } else {
        // Unknown schema (e.g. downgrade after a bad update): preserve the
        // file — the fresh tracker's first persist would otherwise overwrite
        // the user's entire savings history with zeros.
        log::warn!(
            "{} has schema {} (expected 3); backing up and starting fresh",
            path.display(),
            persisted.schema_version
        );
        let _ = std::fs::rename(path, path.with_extension("json.schema-mismatch"));
        Ok(None)
    }
}

fn build_insights(
    recent_usage: &[UsageEvent],
    clients: &[ClientStatus],
    python_runtime_installed: bool,
) -> Vec<DailyInsight> {
    let mut insights = generate_daily_insights(recent_usage);

    if !python_runtime_installed {
        insights.push(DailyInsight {
            id: "runtime-missing".into(),
            category: crate::models::InsightCategory::Health,
            severity: crate::models::InsightSeverity::Warning,
            title: "Managed Python runtime not installed".into(),
            recommendation:
                "Complete bootstrap so Headroom can be installed into Headroom-managed storage."
                    .into(),
            evidence:
                "Headroom keeps the initial app download small and installs tools after first launch."
                    .into(),
            related_workspace: None,
        });
    }

    if clients.iter().all(|client| !client.installed) {
        insights.push(DailyInsight {
            id: "clients-missing".into(),
            category: crate::models::InsightCategory::Workflow,
            severity: crate::models::InsightSeverity::Info,
            title: "No supported clients detected yet".into(),
            recommendation:
                "Install a supported client to start routing requests through Headroom.".into(),
            evidence: "Client adapters look for known local executables during startup.".into(),
            related_workspace: None,
        });
    }

    insights
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct HeadroomSavingsHistoryPoint {
    timestamp: chrono::DateTime<Utc>,
    total_tokens_saved: u64,
}

#[derive(Debug, Default, Clone)]
struct HeadroomDashboardStats {
    session_requests: Option<usize>,
    session_estimated_savings_usd: Option<f64>,
    session_estimated_tokens_saved: Option<u64>,
    session_savings_pct: Option<f64>,
    session_actual_cost_usd: Option<f64>,
    session_total_tokens_sent: Option<u64>,
    savings_history: Vec<HeadroomSavingsHistoryPoint>,
    output_reduction: Option<OutputReduction>,
    /// Whether the wheel's rollout gate actually enabled the output shaper
    /// (`rollout.features[name=="proxy_output_shaper"].enabled`). The desktop
    /// requests the shaper via HEADROOM_OUTPUT_SHAPER=1, but a wheel's rollout
    /// registry can block it by channel (the 0.37.0 wheel gates it to beta,
    /// silently disabling it on stable). None on wheels without the block.
    output_shaper_active: Option<bool>,
    /// Tool-definition tokens the proxy kept out of the model's context by
    /// deferring heavy tool schemas. Process-cumulative (it resets when the
    /// backend restarts), and the backend never writes it to its rollups, so
    /// the tracker accumulates the poll-over-poll delta itself.
    tool_schema_tokens_saved: Option<u64>,
    /// Auto-learning progress (`/stats` `traffic_learner`); None on backends
    /// that predate the block or when learning is disabled.
    learner_progress: Option<crate::models::LearnerProgress>,
    /// Retrieval-churn gauges: tokens the client re-read (`waste_signals`,
    /// persisted by the backend) and explicit CCR retrieve hits
    /// (`compression.ccr_retrievals`, process-scoped). Reported to the server
    /// as latest-observed values, never accumulated here.
    reread_tokens: Option<u64>,
    reread_compressed_tokens: Option<u64>,
    ccr_retrievals: Option<u64>,
}

/// Counterfactual output-token reduction from the proxy's output shaper,
/// parsed from `/stats` (`savings.by_layer.output_shaping`). `method` is
/// "estimated" (synthetic control vs a learned baseline) or "measured" (A/B
/// holdout); the percentage always carries a 95% confidence band. Only
/// populated when the proxy reports `available: true` (i.e. a baseline exists).
#[derive(Debug, Clone)]
struct OutputReduction {
    method: String,
    reduction_percent: f64,
    ci_low_percent: f64,
    ci_high_percent: f64,
    requests: u64,
    /// Lifetime output tokens the shaper's durable estimator says were never
    /// emitted. Unlike the rollup's `output_tokens_saved_delta` this survives
    /// backend restarts and covers every request since the baseline was seeded,
    /// including the period before the rollups carried the layer at all.
    tokens_saved: u64,
    /// The estimator's durable cumulative baseline (what the model would have
    /// emitted unshaped). Sampled poll-over-poll by the tracker to build the
    /// per-bucket output series; never surfaced to the frontend directly.
    baseline_tokens: u64,
}

/// One provider's slice of a rollup bucket's delta, parsed from the upstream
/// `by_provider` map (`anthropic` / `openai` / `unknown`). Field names mirror the
/// bucket total; `hourly_savings` maps these to the display `ProviderSavingsPoint`.
#[derive(Debug, Default, Clone)]
struct ProviderRollupDelta {
    provider: String,
    tokens_saved: u64,
    compression_savings_usd_delta: f64,
    total_input_tokens_delta: u64,
    total_input_cost_usd_delta: f64,
}

#[derive(Debug, Default, Clone)]
struct HeadroomSavingsRollupPoint {
    timestamp: chrono::DateTime<Utc>,
    tokens_saved: u64,
    compression_savings_usd_delta: f64,
    total_input_tokens_delta: u64,
    total_input_cost_usd_delta: f64,
    // Output-shaping deltas; absent on backends older than the layer, which
    // then read as zero and simply contribute nothing to the bucket.
    output_savings_usd_delta: f64,
    output_tokens_saved_delta: u64,
    // Cache reads inside the bucket, derived from the payload's raw `history`
    // checkpoints (the rollup series has no cache dimension). None when no
    // checkpoint fell inside the bucket.
    cache_read_tokens_delta: Option<u64>,
    // The read discount earned inside the bucket, same derivation. Actual
    // read cost = this / 9 (reads bill at ~0.1x; the discount is the 0.9x).
    cache_savings_usd_delta: Option<f64>,
    by_provider: Vec<ProviderRollupDelta>,
}

#[derive(Debug, Default, Clone)]
struct HeadroomSavingsHistoryResponse {
    hourly: Vec<HeadroomSavingsRollupPoint>,
    daily: Vec<HeadroomSavingsRollupPoint>,
    /// The payload's top-level `lifetime` block, when present. Carries the
    /// compression / output-shaping / cache-discount decomposition shown in
    /// the savings drill-down.
    lifetime: Option<crate::models::SavingsBreakdown>,
    /// The upstream history was point-capped, so the parser already removed the
    /// spurious against-zero leading bucket. `drop_rollup_backfill` must then
    /// leave the series alone: applying both drops eats a real day, and with
    /// only two buckets in the window it emptied the series outright.
    backfill_bucket_dropped: bool,
    /// Cumulative counter totals at the oldest raw `history` checkpoint.
    /// The rollup's leading bucket diffs that checkpoint against a zero
    /// baseline, so these totals are exactly the pre-ring history folded into
    /// that bucket: ~zero for a data dir created at the series start (a
    /// genuine first day), large when counters survived a reset or trim.
    /// None when the payload carries no raw history.
    ring_start: Option<RingStartTotals>,
}

impl HeadroomSavingsHistoryResponse {
    fn daily_savings(&self) -> Vec<DailySavingsPoint> {
        self.daily
            .iter()
            .map(|point| DailySavingsPoint {
                // The backend buckets daily rollups at UTC midnight, so the
                // bucket's identity is its UTC date. Converting to Local here
                // used to relabel every bucket as the *previous* local day for
                // users west of UTC, shifting the chart and overwriting
                // genuine local-day archive buckets with another period's
                // totals.
                // ponytail: labels are UTC days; exact local-day rollups need
                // backend-side local bucketing (or reconstruction from hourly).
                date: point.timestamp.format("%Y-%m-%d").to_string(),
                estimated_savings_usd: point.compression_savings_usd_delta,
                estimated_tokens_saved: point.tokens_saved,
                tool_schema_savings_usd: 0.0,
                tool_schema_tokens_saved: 0,
                actual_cost_usd: point.total_input_cost_usd_delta,
                total_tokens_sent: point.total_input_tokens_delta,
                // Backend history has no new-input dimension: this point's
                // sent tokens are full-forwarded (cache-polluted). 0 = no coverage.
                new_input_tokens: 0,
                output_savings_usd: point.output_savings_usd_delta,
                output_tokens_saved: point.output_tokens_saved_delta,
                cache_read_tokens: point.cache_read_tokens_delta,
                cache_savings_usd: point.cache_savings_usd_delta,
                // Filled by the sampler overlay in build_dashboard.
                output_sampled_tokens_saved: None,
                output_baseline_tokens: None,
            })
            .collect()
    }

    fn hourly_savings(&self) -> Vec<HourlySavingsPoint> {
        self.hourly
            .iter()
            .map(|point| HourlySavingsPoint {
                hour: local_hour_key(point.timestamp.with_timezone(&Local)),
                estimated_savings_usd: point.compression_savings_usd_delta,
                estimated_tokens_saved: point.tokens_saved,
                tool_schema_savings_usd: 0.0,
                tool_schema_tokens_saved: 0,
                actual_cost_usd: point.total_input_cost_usd_delta,
                total_tokens_sent: point.total_input_tokens_delta,
                // Backend history has no new-input dimension: this point's
                // sent tokens are full-forwarded (cache-polluted). 0 = no coverage.
                new_input_tokens: 0,
                output_savings_usd: point.output_savings_usd_delta,
                output_tokens_saved: point.output_tokens_saved_delta,
                cache_read_tokens: point.cache_read_tokens_delta,
                cache_savings_usd: point.cache_savings_usd_delta,
                output_sampled_tokens_saved: None,
                output_baseline_tokens: None,
                by_provider: point
                    .by_provider
                    .iter()
                    .map(|p| crate::models::ProviderSavingsPoint {
                        provider: p.provider.clone(),
                        estimated_savings_usd: p.compression_savings_usd_delta,
                        estimated_tokens_saved: p.tokens_saved,
                        actual_cost_usd: p.total_input_cost_usd_delta,
                        total_tokens_sent: p.total_input_tokens_delta,
                    })
                    .collect(),
            })
            .collect()
    }
}

/// How often a failing `/stats` fetch may warn. Silence is what let a 500ms
/// timeout eat three days of output-shaping samples unnoticed, but the
/// dashboard retries every 12s and `log::warn!` bridges to Sentry, so an
/// unthrottled warn would flood it.
/// The window DOUBLES per consecutive warn up to `STATS_FETCH_WARN_MAX_INTERVAL`,
/// because some causes are permanent and unfixable by any release we ship:
/// RUST-87 is a single mac whose 6767 belongs to another app, and the flat
/// 15-minute window sent 96 identical events a day from that one host with no
/// end state. A first failure still speaks immediately; only the repeats decay.
/// A SUSTAINED run of successes clears the streak, so a condition that truly
/// heals and comes back is loud again. One success is NOT enough: the common
/// starved-backend shape flaps -- a cold `/stats` rebuild crosses 15s only
/// while the proxy is busy serving a session -- so clearing on the first
/// success reset the backoff between every pair of timeouts and the streak
/// never reached 2. That is how RUST-86 sent 97 events in 2 days from a single
/// host: the flat-window volume this backoff exists to prevent. The streak
/// clears only after `STATS_FETCH_RECOVERY_WINDOW` of unbroken successes; any
/// failure restarts that run.
/// ponytail: one global slot, not per-category -- a host has one cause at a
/// time in practice; key it by category if that stops being true.
const STATS_FETCH_WARN_INTERVAL: Duration = Duration::from_secs(900);
const STATS_FETCH_WARN_MAX_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// How long `/stats` must fetch cleanly before a recovery counts as real. The
/// dashboard polls every 12s, so this is ~25 consecutive good fetches -- long
/// enough that a busy-proxy flap cannot span it, short enough that a genuine
/// fix is loud again within one sitting.
const STATS_FETCH_RECOVERY_WINDOW: Duration = Duration::from_secs(300);
static STATS_FETCH_WARNED_AT: Mutex<Option<(Instant, u32)>> = Mutex::new(None);
/// When the current unbroken run of successful fetches began; `None` when the
/// last fetch failed or nothing has failed yet.
/// Lock order: take `STATS_FETCH_WARNED_AT` before this one.
static STATS_FETCH_RECOVERED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Window a warn must clear before the `streak`-th consecutive one may speak:
/// 15m, 30m, 1h, 2h, 4h, then capped. `streak` is 1-based.
fn stats_fetch_warn_interval(streak: u32) -> Duration {
    STATS_FETCH_WARN_INTERVAL
        .saturating_mul(1u32 << streak.clamp(1, 6).saturating_sub(1))
        .min(STATS_FETCH_WARN_MAX_INTERVAL)
}

/// Coarse cause class for a `/stats` failure, used as the Sentry fingerprint.
///
/// The reason is interpolated into the message and Sentry groups bridged log
/// lines by message text, so a timeout and an HTTP 404 landed in one
/// un-resolvable grab-bag (RUST-6V: 53 timeouts + 47 404s under one issue).
/// They are different bugs -- a timeout is a starved backend, a 404 is a
/// foreign or ancient server answering on 6767 -- and each needs its own
/// lifecycle. Statuses stay separate from each other for the same reason.
fn stats_fetch_failure_category(reason: &str) -> String {
    if reason.starts_with("timed out") {
        "timeout".to_string()
    } else if let Some(rest) = reason.strip_prefix("HTTP ") {
        match rest.split_whitespace().next() {
            Some(code) => format!("http-{code}"),
            None => "http".to_string(),
        }
    } else if reason.starts_with("payload had no") {
        "payload".to_string()
    } else if reason.starts_with("no local host answered") {
        "unreachable".to_string()
    } else {
        "transport".to_string()
    }
}

fn warn_stats_fetch_failed(reason: &str) {
    let mut last = STATS_FETCH_WARNED_AT.lock();
    // Any failure breaks the recovery run -- including one this window
    // throttles, which is still evidence the condition has not healed.
    *STATS_FETCH_RECOVERED_AT.lock() = None;
    let streak = match *last {
        Some((at, streak)) => {
            if at.elapsed() < stats_fetch_warn_interval(streak) {
                return;
            }
            streak.saturating_add(1)
        }
        None => 1,
    };
    *last = Some((Instant::now(), streak));
    drop(last);
    let category = stats_fetch_failure_category(reason);
    // A 4xx means SOMETHING answered 6767 without the backend's routes, and
    // the readyz gate cannot tell it from an ancient-but-ours proxy (a 404
    // there deliberately counts as reachable). The listener's identity is the
    // one fact that splits "foreign squatter" from "orphaned old Headroom" --
    // RUST-87 shipped three unattributable 404s before this. Throttled to one
    // lookup per 15-minute warn window, so the lsof subprocess is free here.
    // `foreign_holder` stays false when the lookup returns None: "we could not
    // resolve the listener" is not evidence that it is someone else's, and
    // guessing wrong here silently drops a real backend fault.
    let (held_by, foreign_holder) = if category.starts_with("http-4") {
        match crate::tool_manager::listener_identity_and_ownership(6767) {
            Some((who, is_ours)) => (format!("; port 6767 is held by {who}"), !is_ours),
            None => (String::new(), false),
        }
    } else {
        (String::new(), false)
    };
    let message = format!(
        "headroom /stats fetch failed ({reason}){held_by}; dashboard loses the layers \
         only this endpoint reports (output shaping, tool schema)"
    );
    // A 4xx answered by a process that is demonstrably not ours means another
    // application owns 6767 on this host. Nothing we ship changes that -- the
    // backoff above was added for exactly this case (RUST-87) and only slowed
    // the bleed: one mac still sent 129 events with no end state, because a
    // throttle cannot reach zero. The user-visible remedy is freeing the port,
    // which the local log states in full; Sentry gains nothing from a repeat.
    // Our OWN backend answering 4xx is a real fault and still reports.
    if !foreign_holder {
        sentry::with_scope(
            |scope| {
                scope.set_fingerprint(Some(&["stats-fetch-failed", &category]));
            },
            || {
                sentry::capture_message(&message, sentry::Level::Warning);
            },
        );
    }
    // Local only: the fingerprinted capture above is the Sentry path, and the
    // bridged warn would double-report it under the old flat grouping.
    log::warn!("{message}");
}

/// Record a successful `/stats` fetch, clearing the warn backoff only once the
/// successes have been unbroken for `STATS_FETCH_RECOVERY_WINDOW`.
///
/// The window is what makes the backoff hold under a FLAPPING cause. Resetting
/// on the first success meant a host alternating timeout/success re-armed an
/// immediate warn every poll, so the streak never advanced past 1 and the
/// 15m..6h decay never applied (RUST-86).
fn note_stats_fetch_success() {
    let mut warned = STATS_FETCH_WARNED_AT.lock();
    let mut recovered = STATS_FETCH_RECOVERED_AT.lock();
    if warned.is_none() {
        // No outage to recover from; keep the marker clear so the next
        // failure's first warn still speaks immediately.
        *recovered = None;
        return;
    }
    match *recovered {
        // The run has spanned the window: the cause is really gone.
        Some(since) if since.elapsed() >= STATS_FETCH_RECOVERY_WINDOW => {
            *warned = None;
            *recovered = None;
        }
        // Run in progress but too short to trust yet.
        Some(_) => {}
        // First success after a failure: start timing the run.
        None => *recovered = Some(Instant::now()),
    }
}

fn fetch_headroom_dashboard_stats() -> Option<HeadroomDashboardStats> {
    if !is_headroom_proxy_reachable() {
        return None;
    }

    // 500ms was silently fatal: `/stats` rebuilds its whole payload per call
    // and crossed half a second as history grew, so every fetch timed out and
    // the dashboard lost the layers only this endpoint reports (output
    // shaping, tool schema) while `/stats-history` kept the rest looking live.
    // `?cached=1` is the backend's dashboard fast path, but its snapshot TTL
    // (5s) is shorter than our poll interval (12s), so in practice every
    // fetch is a cold rebuild: ~3s idle, past 5s while the proxy is busy
    // serving a session (RUST-6V). 15s turns those into slow successes; a
    // fetch that still times out means the backend is genuinely starved.
    const STATS_FETCH_TIMEOUT_SECS: u64 = 15;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(STATS_FETCH_TIMEOUT_SECS))
        .build()
        .ok()?;

    let hosts = ["127.0.0.1", "localhost"];
    let mut last_failure: Option<String> = None;

    for host in hosts {
        let url = format!("http://{host}:6767/stats?cached=1");
        let response = match client.get(&url).send() {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                last_failure = Some(format!("HTTP {}", response.status()));
                continue;
            }
            // A timeout means the listener accepted and then stalled. Both host
            // names resolve to that same listener, so retrying the other alias
            // only doubles the stall inside a dashboard build.
            Err(err) if err.is_timeout() => {
                warn_stats_fetch_failed(&format!("timed out after {STATS_FETCH_TIMEOUT_SECS}s"));
                return None;
            }
            Err(err) => {
                last_failure = Some(err.to_string());
                continue;
            }
        };

        let body = match response.text() {
            Ok(body) => body,
            Err(err) => {
                last_failure = Some(err.to_string());
                continue;
            }
        };

        if let Some(parsed) = parse_headroom_stats_from_json(&body) {
            // Only a SUSTAINED recovery resets the backoff; a lone success
            // between two timeouts must not (see STATS_FETCH_RECOVERY_WINDOW).
            note_stats_fetch_success();
            return Some(parsed);
        }
        last_failure = Some("payload had no recognised savings fields".to_string());
    }

    warn_stats_fetch_failed(last_failure.as_deref().unwrap_or("no local host answered"));
    None
}

fn fetch_headroom_savings_history() -> Option<HeadroomSavingsHistoryResponse> {
    if !is_headroom_proxy_reachable() {
        return None;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?;

    let hosts = ["127.0.0.1", "localhost"];

    for host in hosts {
        let url = format!("http://{host}:6767/stats-history");
        let response = match client.get(&url).send() {
            Ok(response) if response.status().is_success() => response,
            _ => continue,
        };

        let body = match response.text() {
            Ok(body) => body,
            Err(_) => continue,
        };

        if let Some(parsed) = parse_headroom_stats_history_from_json(&body) {
            return Some(parsed);
        }
    }

    None
}

/// Parse auto-learning progress from a `/stats` payload's `traffic_learner`
/// block (headroomlabs-ai/headroom#3104). Null/absent (older backend or
/// learning disabled) parses to `None`, so the UI falls back to static copy.
fn parse_learner_progress(root: &Value) -> Option<crate::models::LearnerProgress> {
    let node = value_at_path(root, &["traffic_learner"])?;
    if !node.is_object() {
        return None;
    }
    Some(crate::models::LearnerProgress {
        pending_patterns: node
            .get("pending_patterns")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        min_evidence: node
            .get("min_evidence")
            .and_then(Value::as_u64)
            .unwrap_or(5),
        patterns_saved: node
            .get("patterns_saved")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

/// Parse the output-shaper reduction estimate from a `/stats` payload. Lives
/// under `savings.by_layer.output_shaping`, with `tokens.output_reduction` as a
/// fallback. Returns `None` unless the proxy reports `available: true`, so the
/// dashboard hides the stat until a baseline has been seeded.
fn parse_output_reduction(root: &Value) -> Option<OutputReduction> {
    let node = value_at_path(root, &["savings", "by_layer", "output_shaping"])
        .or_else(|| value_at_path(root, &["tokens", "output_reduction"]))?;

    if !node
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    // A reduction is definitionally within [0, 100]. On a freshly-seeded
    // baseline the synthetic-control estimate divides by a near-zero baseline
    // and blows up (e.g. -6130%), which the dashboard renders as a
    // double-negative "Output −-6,130.7%". Treat an out-of-range estimate as
    // "baseline not ready yet" and hide the stat, same as available:false.
    let reduction_percent = node
        .get("reduction_percent")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if !reduction_percent.is_finite() || !(0.0..=100.0).contains(&reduction_percent) {
        return None;
    }

    Some(OutputReduction {
        method: node
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("estimated")
            .to_string(),
        reduction_percent,
        ci_low_percent: node
            .get("ci_low_percent")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        ci_high_percent: node
            .get("ci_high_percent")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        requests: node.get("requests").and_then(Value::as_u64).unwrap_or(0),
        tokens_saved: node
            .get("tokens_saved")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        baseline_tokens: node
            .get("baseline_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn parse_headroom_stats_from_json(body: &str) -> Option<HeadroomDashboardStats> {
    let root = serde_json::from_str::<Value>(body).ok()?;

    let path_requests = value_at_path_u64(&root, &["requests", "total"])
        .and_then(|value| usize::try_from(value).ok());
    let path_tokens = value_at_path_u64(&root, &["tokens", "saved"])
        .or_else(|| value_at_path_u64(&root, &["tokens", "compression_saved"]))
        .or_else(|| value_at_path_u64(&root, &["compression", "tokens_saved"]));
    let path_usd = value_at_path_f64(&root, &["cost", "compression_savings_usd"])
        .or_else(|| value_at_path_f64(&root, &["cost", "compression_saved_usd"]))
        .or_else(|| value_at_path_f64(&root, &["compression", "savings_usd"]));
    let path_actual_cost_usd = value_at_path_f64(&root, &["cost", "total_input_cost_usd"])
        .or_else(|| value_at_path_f64(&root, &["cost", "cost_with_headroom_usd"]))
        .or_else(|| value_at_path_f64(&root, &["cost", "actual_input_cost_usd"]))
        .or_else(|| value_at_path_f64(&root, &["cost", "input_actual_cost_usd"]))
        .or_else(|| value_at_path_f64(&root, &["cost", "input_cost_usd"]))
        .or_else(|| value_at_path_f64(&root, &["cost", "actual_cost_usd"]))
        .or_else(|| value_at_path_f64(&root, &["cost", "actual_usd"]))
        .or_else(|| value_at_path_f64(&root, &["cost", "actual_input_usd"]));
    let path_savings_pct = value_at_path_f64(&root, &["tokens", "savings_percent"]);
    let requests = path_requests.or_else(|| {
        find_u64_key_recursive(
            &root,
            &["total_requests", "totalRequests", "requests_total"],
        )
        .and_then(|value| usize::try_from(value).ok())
    });

    let tokens = path_tokens.or_else(|| {
        find_u64_key_recursive(
            &root,
            &[
                "compressionTokensSaved",
                "compression_tokens_saved",
                "totalCompressionTokensSaved",
                "total_compression_tokens_saved",
            ],
        )
    });

    let usd = path_usd.or_else(|| {
        find_f64_key_recursive(
            &root,
            &[
                "compressionSavingsUsd",
                "compression_savings_usd",
                "compressionSavedUsd",
                "compression_saved_usd",
                "compressionCostSavedUsd",
                "compression_cost_saved_usd",
            ],
        )
    });
    // Denominator for the savings ratio = "new input" this turn only.
    // Claude Code re-sends the entire conversation every turn; the cached
    // prefix (cache_read tokens) is forwarded but Headroom deliberately never
    // compresses it -- doing so would bust the provider prefix cache for a net
    // loss. Counting those re-sent cached tokens in the denominator drove the
    // displayed ratio toward zero as sessions grew longer and caching got more
    // effective. Under provider prompt caching, genuinely-new content lands in
    // cache_write (1.25x), NOT in uncached_input -- which collapses to ~0 and
    // would blow the ratio up to ~100%. New input we can actually compress is
    // cache_write + uncached, so measure compression against that.
    let new_input_tokens = {
        let cache_write =
            value_at_path_u64(&root, &["prefix_cache", "totals", "cache_write_tokens"]);
        let uncached =
            value_at_path_u64(&root, &["prefix_cache", "totals", "uncached_input_tokens"]).or_else(
                || find_u64_key_recursive(&root, &["uncachedInputTokens", "uncached_input_tokens"]),
            );
        match (cache_write, uncached) {
            (None, None) => None,
            (write, uncached) => Some(write.unwrap_or(0).saturating_add(uncached.unwrap_or(0))),
        }
    };
    let total_after_compression = value_at_path_u64(&root, &["tokens", "input"])
        .or_else(|| value_at_path_u64(&root, &["cost", "total_input_tokens"]))
        .or_else(|| value_at_path_u64(&root, &["tokens", "actual_input_tokens"]))
        .or_else(|| value_at_path_u64(&root, &["tokens", "input_tokens"]))
        .or_else(|| value_at_path_u64(&root, &["tokens", "total_after_compression"]))
        .or_else(|| value_at_path_u64(&root, &["tokens", "after_compression"]))
        .or_else(|| value_at_path_u64(&root, &["tokens", "sent"]))
        .or_else(|| {
            find_u64_key_recursive(
                &root,
                &[
                    "actualInputTokens",
                    "actual_input_tokens",
                    "totalInputTokens",
                    "total_input_tokens",
                    "inputTokens",
                    "input_tokens",
                    "totalAfterCompression",
                    "total_after_compression",
                    "tokensSent",
                    "tokens_sent",
                    "totalTokensSent",
                    "total_tokens_sent",
                ],
            )
        });
    // Prefer new input (cache_write + uncached); fall back to total forwarded
    // tokens for proxy builds that do not report prefix-cache totals (back-compat).
    // Filter the primary to >0 *before* the fallback: new_input_tokens is
    // Some(0) on a fully-cached snapshot, and `.or` only fires on None -- without
    // this the Some(0) skips the fallback and is then dropped, losing a valid count.
    let session_total_tokens_sent = new_input_tokens
        .filter(|value| *value > 0)
        .or(total_after_compression)
        .filter(|value| *value > 0);
    // `summary.compression` carries the process-cumulative counter. The
    // `savings.by_layer` block reports the same layer but only over the recent
    // request window, so it is a fallback for shape, not a preferred source.
    let tool_schema_tokens_saved = value_at_path_u64(
        &root,
        &["summary", "compression", "tool_schema_tokens_saved"],
    )
    .or_else(|| {
        value_at_path_u64(
            &root,
            &["savings", "by_layer", "tool_search", "tokens_saved"],
        )
    });
    // INVARIANT (set with Garm 2026-09-03; do NOT change the basis without
    // asking him first): every displayed input-savings figure is measured
    // against JUST the new input Headroom can compress, on both numerator and
    // denominator. See the banner on `newInputSavingsRate` in
    // dashboardHelpers.ts for the full rule. This denominator (new_input_tokens
    // below) is byte-identical at 0.9.2 and 0.9.4 -- the ~25%->~5% drop in that
    // era was compression collapsing on the wheel swap, NOT a denominator
    // change. A denominator change is a product decision: ask first.
    //
    // Ratio against new input: compression-only saved / (saved + new input).
    // `tokens.saved` is ALL-LAYERS -- it includes tool-schema deferral, which
    // is disjoint from the checkpoint series and never part of new input
    // (schemas ride the cached prefix) -- so subtract it; the wheel's own
    // `new_input_savings_percent` pairs compression-only the same way. The
    // proxy's `tokens.savings_percent` stays a last-resort fallback.
    let session_savings_pct = tokens
        .map(|saved| saved.saturating_sub(tool_schema_tokens_saved.unwrap_or(0)))
        .and_then(|saved| {
            session_total_tokens_sent.and_then(|sent| {
                let total_before = saved.saturating_add(sent);
                if total_before > 0 {
                    Some(saved as f64 / total_before as f64 * 100.0)
                } else {
                    None
                }
            })
        })
        .or(path_savings_pct);
    let actual_cost_usd = path_actual_cost_usd.or_else(|| {
        find_f64_key_recursive(
            &root,
            &[
                "totalInputCostUsd",
                "total_input_cost_usd",
                "costWithHeadroomUsd",
                "cost_with_headroom_usd",
                "actualInputCostUsd",
                "actual_input_cost_usd",
                "inputActualCostUsd",
                "input_actual_cost_usd",
                "inputCostUsd",
                "input_cost_usd",
                "actualCostUsd",
                "actual_cost_usd",
                "actualUsd",
                "actual_usd",
                "actualInputUsd",
                "actual_input_usd",
            ],
        )
    });
    let savings_history = value_at_path(&root, &["compression_savings_history"])
        .or_else(|| value_at_path(&root, &["compression", "savings_history"]))
        .or_else(|| value_at_path(&root, &["savings_history"]))
        .and_then(parse_savings_history)
        .unwrap_or_default();

    let output_reduction = parse_output_reduction(&root);
    // Rollout truth for the shaper: a ledger-recomputed reduction is only a
    // live claim while the wheel actually runs the shaper. Absent block => None
    // (older wheels), and the report gate stays open.
    let output_shaper_active = value_at_path(&root, &["rollout", "features"])
        .and_then(Value::as_array)
        .and_then(|features| {
            features
                .iter()
                .find(|f| f.get("name").and_then(Value::as_str) == Some("proxy_output_shaper"))
        })
        .and_then(|f| f.get("enabled").and_then(Value::as_bool));

    let learner_progress = parse_learner_progress(&root);

    // Retrieval-churn gauges. `waste_signals` is the backend's persisted
    // waste ledger; `reread_compressed` is the token volume of content a
    // client re-fetched after Headroom had compressed it away -- the direct
    // "context filled faster" churn signal. Absent on older backends.
    let reread_tokens = value_at_path_u64(&root, &["waste_signals", "reread"]);
    let reread_compressed_tokens =
        value_at_path_u64(&root, &["waste_signals", "reread_compressed"]);
    let ccr_retrievals = value_at_path_u64(&root, &["compression", "ccr_retrievals"]);

    if requests.is_none()
        && tokens.is_none()
        && usd.is_none()
        && session_total_tokens_sent.is_none()
        && actual_cost_usd.is_none()
        && output_reduction.is_none()
        && learner_progress.is_none()
    {
        None
    } else {
        Some(HeadroomDashboardStats {
            learner_progress,
            reread_tokens,
            reread_compressed_tokens,
            ccr_retrievals,
            session_requests: requests,
            session_estimated_savings_usd: usd,
            session_estimated_tokens_saved: tokens,
            session_savings_pct,
            session_actual_cost_usd: actual_cost_usd.map(|value| value.max(0.0)),
            session_total_tokens_sent,
            savings_history,
            output_reduction,
            output_shaper_active,
            tool_schema_tokens_saved,
        })
    }
}

/// True when the upstream stored history hit its point-count cap and older
/// checkpoints were trimmed away. In that state the oldest surviving rollup
/// bucket carries a spurious carried-over cumulative as its delta.
fn upstream_history_trimmed(root: &Value) -> bool {
    let stored = value_at_path_u64(root, &["history_summary", "stored_points"]);
    let cap = value_at_path_u64(root, &["retention", "max_history_points"]);
    matches!((stored, cap), (Some(stored), Some(cap)) if cap > 0 && stored >= cap)
}

/// Remove the oldest bucket (smallest timestamp) from a rollup series.
fn drop_oldest_rollup_bucket(series: &mut Vec<HeadroomSavingsRollupPoint>) {
    if let Some((idx, _)) = series
        .iter()
        .enumerate()
        .min_by_key(|(_, point)| point.timestamp)
    {
        series.remove(idx);
    }
}

/// Cumulative counter totals at the raw ring's oldest checkpoint. See the
/// field doc on `HeadroomSavingsHistoryResponse::ring_start`.
#[derive(Debug, Default, Clone, Copy)]
struct RingStartTotals {
    tokens_saved: u64,
    compression_savings_usd: f64,
    total_input_tokens: u64,
    total_input_cost_usd: f64,
    output_tokens_saved: u64,
    output_savings_usd: f64,
}

fn ring_start_totals(root: &Value) -> Option<RingStartTotals> {
    let Some(Value::Array(items)) = value_at_path(root, &["history"]) else {
        return None;
    };
    items
        .iter()
        .filter_map(|item| {
            let map = item.as_object()?;
            let timestamp = map
                .get("timestamp")
                .and_then(|value| value.as_str())
                .and_then(parse_history_timestamp)?;
            Some((timestamp, map))
        })
        .min_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, map)| RingStartTotals {
            tokens_saved: map
                .get("total_tokens_saved")
                .and_then(parse_u64_value)
                .unwrap_or(0),
            compression_savings_usd: map
                .get("compression_savings_usd")
                .and_then(parse_f64_value)
                .unwrap_or(0.0),
            total_input_tokens: map
                .get("total_input_tokens")
                .and_then(parse_u64_value)
                .unwrap_or(0),
            total_input_cost_usd: map
                .get("total_input_cost_usd")
                .and_then(parse_f64_value)
                .unwrap_or(0.0),
            output_tokens_saved: map
                .get("output_tokens_saved")
                .and_then(parse_u64_value)
                .unwrap_or(0),
            output_savings_usd: map
                .get("output_savings_usd")
                .and_then(parse_f64_value)
                .unwrap_or(0.0),
        })
}

fn parse_headroom_stats_history_from_json(body: &str) -> Option<HeadroomSavingsHistoryResponse> {
    let root = serde_json::from_str::<Value>(body).ok()?;
    let mut hourly = value_at_path(&root, &["series", "hourly"])
        .and_then(parse_savings_rollup_series)
        .unwrap_or_default();
    let mut daily = value_at_path(&root, &["series", "daily"])
        .and_then(parse_savings_rollup_series)
        .unwrap_or_default();

    // The rollup series carries no cache dimension, but the payload's raw
    // `history` checkpoints do (cumulative, sampled per request) — diff them
    // into per-bucket cache-read deltas and attach to the rollup points.
    // Keys are UTC (same identity as the rollup buckets; see daily_savings).
    let (daily_cache_reads, hourly_cache_reads) = derive_cache_read_deltas(&root);
    for point in &mut daily {
        let delta = daily_cache_reads.get(&point.timestamp.format("%Y-%m-%d").to_string());
        point.cache_read_tokens_delta = delta.map(|d| d.read_tokens);
        point.cache_savings_usd_delta = delta.map(|d| d.savings_usd);
    }
    for point in &mut hourly {
        let delta = hourly_cache_reads.get(&point.timestamp.format("%Y-%m-%dT%H").to_string());
        point.cache_read_tokens_delta = delta.map(|d| d.read_tokens);
        point.cache_savings_usd_delta = delta.map(|d| d.savings_usd);
    }

    // When the upstream stored history has been trimmed (point-count cap
    // reached), the backend's rollup diffs the oldest surviving checkpoint from
    // a zero baseline, dumping its entire cumulative into the first bucket's
    // delta. That produces a huge spurious spike at the window's leading edge
    // that slides forward as old checkpoints age out. Drop that boundary bucket
    // so the chart shows real per-bucket savings. Untrimmed histories (new
    // users) keep their genuine first bucket. Lifetime totals are unaffected.
    let backfill_bucket_dropped = upstream_history_trimmed(&root);
    if backfill_bucket_dropped {
        drop_oldest_rollup_bucket(&mut daily);
        drop_oldest_rollup_bucket(&mut hourly);
    }

    let lifetime = parse_savings_breakdown(&root);

    if hourly.is_empty() && daily.is_empty() && lifetime.is_none() {
        None
    } else {
        Some(HeadroomSavingsHistoryResponse {
            hourly,
            daily,
            lifetime,
            backfill_bucket_dropped,
            ring_start: ring_start_totals(&root),
        })
    }
}

/// Parse the `/stats-history` top-level `lifetime` block into the savings
/// decomposition. Requires `compression_savings_usd` (schema v3+); everything
/// else defaults to zero so older backends missing a field still show the
/// rows they do report. Cache savings stay a separate labelled figure — they
/// are the client's provider-cache discount, never Headroom's claim.
fn parse_savings_breakdown(root: &Value) -> Option<crate::models::SavingsBreakdown> {
    let compression_savings_usd =
        value_at_path_f64(root, &["lifetime", "compression_savings_usd"])?;
    Some(crate::models::SavingsBreakdown {
        compression_savings_usd,
        // Overwritten at render time from the merged daily buckets (see
        // `dashboard_snapshot`); this process-scoped field resets with the
        // backend and is only the fallback when no rollups exist yet.
        output_savings_usd: value_at_path_f64(root, &["lifetime", "output_savings_usd"])
            .unwrap_or(0.0),
        // The backend has no lifetime figure for this layer at all -- it only
        // reports a process-cumulative counter, which the local tracker
        // accumulates. Filled in at render time, same as the row above.
        tool_schema_savings_usd: 0.0,
        tool_schema_tokens_saved: 0,
        cache_savings_usd: value_at_path_f64(root, &["lifetime", "cache_savings_usd"])
            .unwrap_or(0.0),
        cache_read_tokens: value_at_path_u64(root, &["lifetime", "cache_read_tokens"]).unwrap_or(0),
        total_input_tokens: value_at_path_u64(root, &["lifetime", "total_input_tokens"])
            .unwrap_or(0),
        total_input_cost_usd: value_at_path_f64(root, &["lifetime", "total_input_cost_usd"])
            .unwrap_or(0.0),
        model_rates: parse_model_rates(root),
    })
}

/// Below this many requests a model's rate says more about which handful of
/// prompts it happened to see than about how well compression works on it.
const MIN_MODEL_RATE_REQUESTS: u64 = 100;

/// Per-model compression rates from the `by_model` block, best rate first.
///
/// `passthrough:*` entries are dropped: they are token-count and model-list
/// probes that carry no compressible content, so they always sit at 0% and
/// would read as failures rather than as the non-events they are.
fn parse_model_rates(root: &Value) -> Vec<crate::models::ModelSavingsRate> {
    let Some(entries) = value_at_path(root, &["by_model"]).and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut rates: Vec<_> = entries
        .iter()
        .filter(|(model, _)| !model.starts_with("passthrough:"))
        .filter_map(|(model, node)| {
            let requests = value_at_path_u64(node, &["requests"])?;
            if requests < MIN_MODEL_RATE_REQUESTS {
                return None;
            }
            Some(crate::models::ModelSavingsRate {
                model: model.clone(),
                requests,
                savings_percent: value_at_path_f64(node, &["savings_percent"])?,
            })
        })
        .collect();
    // Ties break on sample size so the sturdier row leads.
    rates.sort_by(|a, b| {
        b.savings_percent
            .partial_cmp(&a.savings_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.requests.cmp(&a.requests))
    });
    rates
}

fn value_at_path_u64(root: &Value, path: &[&str]) -> Option<u64> {
    let value = value_at_path(root, path)?;
    parse_u64_value(value)
}

fn value_at_path_f64(root: &Value, path: &[&str]) -> Option<f64> {
    let value = value_at_path(root, path)?;
    parse_f64_value(value)
}

fn value_at_path<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = root;
    for segment in path {
        match current {
            Value::Object(map) => {
                current = map.get(*segment)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn parse_savings_history(value: &Value) -> Option<Vec<HeadroomSavingsHistoryPoint>> {
    let Value::Array(items) = value else {
        return None;
    };
    let points = items
        .iter()
        .filter_map(parse_savings_history_point)
        .collect::<Vec<_>>();
    Some(points)
}

/// One bucket's worth of cache deltas diffed from the raw `history`
/// checkpoints: read tokens, plus the read *discount* in dollars
/// (`cache_savings_usd` = what the reads would have cost at the full input
/// rate minus the ~0.1x they did cost, so actual read cost = discount / 9 —
/// provider-priced upstream, no client-side model-price guessing).
#[derive(Debug, Default, Clone, Copy)]
struct CacheDelta {
    read_tokens: u64,
    savings_usd: f64,
}

/// Per-UTC-day and per-UTC-hour cache deltas, diffed from the raw `history`
/// checkpoints (each carries the GLOBAL cumulative `cache_read_tokens` and
/// `cache_savings_usd` at the moment of one request). Each consecutive diff is
/// attributed to the later checkpoint's bucket. The first checkpoint has no
/// predecessor and is skipped — which also avoids dumping the whole cumulative
/// into the window's leading edge on trimmed histories, mirroring
/// `drop_oldest_rollup_bucket`. Diffs clamp at zero so a counter reset can
/// only lose a sample, never produce a negative or inflated bucket.
fn derive_cache_read_deltas(
    root: &Value,
) -> (
    std::collections::HashMap<String, CacheDelta>,
    std::collections::HashMap<String, CacheDelta>,
) {
    let mut daily: std::collections::HashMap<String, CacheDelta> = std::collections::HashMap::new();
    let mut hourly: std::collections::HashMap<String, CacheDelta> =
        std::collections::HashMap::new();
    let Some(Value::Array(items)) = value_at_path(root, &["history"]) else {
        return (daily, hourly);
    };
    let mut checkpoints: Vec<(chrono::DateTime<Utc>, u64, f64)> = items
        .iter()
        .filter_map(|item| {
            let map = item.as_object()?;
            let timestamp = map
                .get("timestamp")
                .and_then(|value| value.as_str())
                .and_then(parse_history_timestamp)?;
            let cache_read = map.get("cache_read_tokens").and_then(parse_u64_value)?;
            // Older backends may lack the dollar counter; carry zero so the
            // token dimension still works and the dollar delta reads as 0.
            let cache_savings_usd = map
                .get("cache_savings_usd")
                .and_then(parse_f64_value)
                .unwrap_or(0.0);
            Some((timestamp, cache_read, cache_savings_usd))
        })
        .collect();
    checkpoints.sort_by_key(|(timestamp, _, _)| *timestamp);
    for pair in checkpoints.windows(2) {
        let (_, previous_read, previous_usd) = pair[0];
        let (timestamp, current_read, current_usd) = pair[1];
        let delta = CacheDelta {
            read_tokens: current_read.saturating_sub(previous_read),
            savings_usd: (current_usd - previous_usd).max(0.0),
        };
        for (map, key) in [
            (&mut daily, timestamp.format("%Y-%m-%d").to_string()),
            (&mut hourly, timestamp.format("%Y-%m-%dT%H").to_string()),
        ] {
            let entry = map.entry(key).or_default();
            entry.read_tokens += delta.read_tokens;
            entry.savings_usd += delta.savings_usd;
        }
    }
    (daily, hourly)
}

fn parse_savings_rollup_series(value: &Value) -> Option<Vec<HeadroomSavingsRollupPoint>> {
    let Value::Array(items) = value else {
        return None;
    };
    let points = items
        .iter()
        .filter_map(parse_savings_rollup_point)
        .collect::<Vec<_>>();
    Some(points)
}

fn parse_savings_history_point(value: &Value) -> Option<HeadroomSavingsHistoryPoint> {
    match value {
        Value::Array(items) if items.len() >= 2 => {
            let timestamp = items.first()?.as_str().and_then(parse_history_timestamp)?;
            let total_tokens_saved = parse_u64_value(items.get(1)?)?;
            Some(HeadroomSavingsHistoryPoint {
                timestamp,
                total_tokens_saved,
            })
        }
        Value::Object(map) => {
            let timestamp = map
                .get("timestamp")
                .and_then(|value| value.as_str())
                .and_then(parse_history_timestamp)?;
            let total_tokens_saved = map
                .get("total_tokens_saved")
                .or_else(|| map.get("tokens_saved"))
                .and_then(parse_u64_value)?;
            Some(HeadroomSavingsHistoryPoint {
                timestamp,
                total_tokens_saved,
            })
        }
        _ => None,
    }
}

fn parse_savings_rollup_point(value: &Value) -> Option<HeadroomSavingsRollupPoint> {
    let Value::Object(map) = value else {
        return None;
    };

    let timestamp = map
        .get("timestamp")
        .and_then(|value| value.as_str())
        .and_then(parse_history_timestamp)?;

    Some(HeadroomSavingsRollupPoint {
        timestamp,
        // Attached afterwards from the raw history checkpoints; the rollup
        // object itself has no cache field.
        cache_read_tokens_delta: None,
        cache_savings_usd_delta: None,
        tokens_saved: map
            .get("tokens_saved")
            .and_then(parse_u64_value)
            .unwrap_or_default(),
        compression_savings_usd_delta: map
            .get("compression_savings_usd_delta")
            .and_then(parse_f64_value)
            .unwrap_or_default()
            .max(0.0),
        total_input_tokens_delta: map
            .get("total_input_tokens_delta")
            .and_then(parse_u64_value)
            .unwrap_or_default(),
        total_input_cost_usd_delta: map
            .get("total_input_cost_usd_delta")
            .and_then(parse_f64_value)
            .unwrap_or_default()
            .max(0.0),
        output_savings_usd_delta: map
            .get("output_savings_usd_delta")
            .and_then(parse_f64_value)
            .unwrap_or_default()
            .max(0.0),
        output_tokens_saved_delta: map
            .get("output_tokens_saved_delta")
            .and_then(parse_u64_value)
            .unwrap_or_default(),
        by_provider: parse_rollup_by_provider(map.get("by_provider")),
    })
}

/// Parse the upstream `by_provider` map (`{ "anthropic": { tokens_saved, ... }, ... }`)
/// into a deterministically-ordered list. Missing/empty yields an empty Vec, so
/// pre-feature buckets carry no provider breakdown.
fn parse_rollup_by_provider(value: Option<&Value>) -> Vec<ProviderRollupDelta> {
    let Some(Value::Object(providers)) = value else {
        return Vec::new();
    };
    let mut out: Vec<ProviderRollupDelta> = providers
        .iter()
        .map(|(provider, entry)| {
            let get_u64 = |key: &str| entry.get(key).and_then(parse_u64_value).unwrap_or_default();
            let get_f64 = |key: &str| {
                entry
                    .get(key)
                    .and_then(parse_f64_value)
                    .unwrap_or_default()
                    .max(0.0)
            };
            ProviderRollupDelta {
                provider: provider.clone(),
                tokens_saved: get_u64("tokens_saved"),
                compression_savings_usd_delta: get_f64("compression_savings_usd_delta"),
                total_input_tokens_delta: get_u64("total_input_tokens_delta"),
                total_input_cost_usd_delta: get_f64("total_input_cost_usd_delta"),
            }
        })
        .collect();
    out.sort_by(|a, b| a.provider.cmp(&b.provider));
    out
}

fn parse_history_timestamp(text: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .and_then(|timestamp| Local.from_local_datetime(&timestamp).single())
                .map(|timestamp| timestamp.with_timezone(&Utc))
        })
}

fn local_day_key(timestamp: chrono::DateTime<Local>) -> String {
    crate::storage::user_day_key(timestamp)
}

// Boundary between local tracker (pre-cutoff, authoritative) and /stats-history
// (cutoff and later, authoritative). Release builds pin to the date the schema
// stabilized; debug builds track "today" so dev sessions never fall behind the
// history source while iterating.
fn savings_history_cutoff_date() -> String {
    if cfg!(debug_assertions) {
        local_day_key(Local::now())
    } else {
        "2026-06-02".to_string()
    }
}

fn local_hour_key(timestamp: chrono::DateTime<Local>) -> String {
    timestamp.format("%Y-%m-%dT%H:00").to_string()
}

fn day_key_from_hour_key(hour_key: &str) -> String {
    hour_key.split('T').next().unwrap_or(hour_key).to_string()
}

fn should_rollover_display_session(
    last_activity_at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
) -> bool {
    let last_local = last_activity_at.with_timezone(&Local);
    let now_local = now.with_timezone(&Local);
    now_local.date_naive() > last_local.date_naive()
        && now.signed_duration_since(last_activity_at) >= chrono::Duration::hours(1)
}

fn derive_session_buckets_with_key<F>(
    stats: &HeadroomDashboardStats,
    history: &[HeadroomSavingsHistoryPoint],
    sent_history: &[HeadroomSavingsHistoryPoint],
    bucket_key_for_timestamp: F,
) -> Vec<(String, DailySavingsBucket)>
where
    F: Fn(chrono::DateTime<Local>) -> String,
{
    let total_tokens = stats.session_estimated_tokens_saved.unwrap_or(0);
    let total_usd = stats.session_estimated_savings_usd.unwrap_or(0.0).max(0.0);
    let total_tokens_sent = stats.session_total_tokens_sent.unwrap_or(0);
    let total_actual_cost_usd = stats.session_actual_cost_usd.unwrap_or(0.0).max(0.0);
    if total_tokens == 0
        && total_usd <= 0.0
        && total_tokens_sent == 0
        && total_actual_cost_usd <= 0.0
    {
        return Vec::new();
    }

    // The session saved counter is all-layers while the checkpoint series is
    // compression-only ("bare message figure", tool_search disjoint), so
    // proportions over the raw session total dropped the tool-schema share of
    // sent on the floor even at full history coverage.
    let compression_total =
        total_tokens.saturating_sub(stats.tool_schema_tokens_saved.unwrap_or(0));

    let mut buckets = BTreeMap::<String, DailySavingsBucket>::new();
    let Some(first_point) = history.first().copied() else {
        return Vec::new();
    };
    let mut previous_total = first_point.total_tokens_saved;
    let mut history_total = 0u64;

    for point in history.iter().copied().skip(1) {
        let delta_tokens = point.total_tokens_saved.saturating_sub(previous_total);
        previous_total = point.total_tokens_saved;
        if delta_tokens == 0 {
            continue;
        }
        history_total = history_total.saturating_add(delta_tokens);
        let bucket_key = bucket_key_for_timestamp(point.timestamp.with_timezone(&Local));
        let entry = buckets.entry(bucket_key).or_default();
        entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_add(delta_tokens);
    }

    if buckets.is_empty() || history_total == 0 || history_total > total_tokens {
        return Vec::new();
    }

    if total_tokens > 0 && total_usd > 0.0 {
        let usd_per_token = total_usd / total_tokens as f64;
        for bucket in buckets.values_mut() {
            bucket.estimated_savings_usd = bucket.estimated_tokens_saved as f64 * usd_per_token;
        }
    }

    // Per-hour denominator. Prefer the real thing: diff the cumulative
    // new-input series (sampled once per poll) the same way as saved above.
    // A single reading only carries the session cumulative, and the first
    // sample's baseline can't be attributed to an hour — so the sampled deltas
    // only cover sent that landed *after* the app started observing this proxy
    // session. When coverage is thin (cold start, attach-to-running-proxy,
    // rolling-window truncation) fall back to the old proportional smear rather
    // than under-report; once sampling accounts for ~all session sent, use it
    // and every hour shows its true ratio instead of the session-wide one.
    let mut sampled_sent: BTreeMap<String, u64> = BTreeMap::new();
    let mut sampled_total = 0u64;
    if let Some(first_sent) = sent_history.first().copied() {
        let mut previous_sent = first_sent.total_tokens_saved;
        for point in sent_history.iter().copied().skip(1) {
            let delta_sent = point.total_tokens_saved.saturating_sub(previous_sent);
            previous_sent = point.total_tokens_saved;
            if delta_sent == 0 {
                continue;
            }
            sampled_total = sampled_total.saturating_add(delta_sent);
            let bucket_key = bucket_key_for_timestamp(point.timestamp.with_timezone(&Local));
            *sampled_sent.entry(bucket_key).or_default() += delta_sent;
        }
    }
    // ponytail: coverage threshold. 0.9 = sampled deltas must account for >=90%
    // of session sent before we trust per-hour attribution. Raise if the chart
    // flickers between smear and real early in sessions; lower to switch sooner.
    let sampling_covers_session =
        total_tokens_sent > 0 && sampled_total as f64 >= 0.9 * total_tokens_sent as f64;

    if sampling_covers_session {
        // Real per-hour sent. A bucket may appear for an hour with no saved
        // delta — an honest 0%-savings hour.
        for (bucket_key, sent) in sampled_sent {
            let bucket = buckets.entry(bucket_key).or_default();
            bucket.total_tokens_sent = sent;
            bucket.new_input_tokens = sent;
        }
    } else if compression_total > 0 && total_tokens_sent > 0 {
        // Fallback: smear the session total across buckets in proportion to
        // savings. Every hour reads the session-wide ratio, but nothing is
        // dumped or under-counted while sampling coverage is still thin.
        let keys = buckets.keys().cloned().collect::<Vec<_>>();
        for key in keys.iter() {
            let bucket = buckets.get_mut(key).expect("bucket exists");
            // Proportions over the compression-only session total: this
            // preserves the unattributable-remainder design (history covering
            // a fraction of the session attributes only that fraction of
            // sent) without the tool-schema layer skewing the fraction.
            bucket.total_tokens_sent = ((bucket.estimated_tokens_saved as u128
                * total_tokens_sent as u128)
                / compression_total as u128) as u64;
            // Same new-input scale (the session cumulative IS the new-input
            // series), just smeared: window sums stay exact, only the
            // intra-window attribution is approximate.
            bucket.new_input_tokens = bucket.total_tokens_sent;
        }
    }

    if total_tokens > 0 && total_actual_cost_usd > 0.0 {
        let keys = buckets.keys().cloned().collect::<Vec<_>>();
        for key in keys.iter() {
            let bucket = buckets.get_mut(key).expect("bucket exists");
            bucket.actual_cost_usd = total_actual_cost_usd
                * (bucket.estimated_tokens_saved as f64 / total_tokens as f64);
        }
    }

    buckets.into_iter().collect()
}

fn merge_session_savings_history(
    existing: &[HeadroomSavingsHistoryPoint],
    incoming: &[HeadroomSavingsHistoryPoint],
) -> Vec<HeadroomSavingsHistoryPoint> {
    let mut merged = BTreeMap::new();
    for point in existing.iter().chain(incoming.iter()) {
        merged
            .entry(point.timestamp)
            .and_modify(|value: &mut u64| *value = (*value).max(point.total_tokens_saved))
            .or_insert(point.total_tokens_saved);
    }

    let mut normalized = Vec::with_capacity(merged.len());
    let mut previous_total = 0u64;
    for (timestamp, total_tokens_saved) in merged {
        if !normalized.is_empty() && total_tokens_saved < previous_total {
            continue;
        }
        previous_total = total_tokens_saved;
        normalized.push(HeadroomSavingsHistoryPoint {
            timestamp,
            total_tokens_saved,
        });
    }
    normalized
}

fn derive_session_hourly_buckets(
    stats: &HeadroomDashboardStats,
    history: &[HeadroomSavingsHistoryPoint],
    sent_history: &[HeadroomSavingsHistoryPoint],
) -> Vec<(String, DailySavingsBucket)> {
    derive_session_buckets_with_key(stats, history, sent_history, local_hour_key)
}

fn diff_hourly_buckets(
    previous: &BTreeMap<String, DailySavingsBucket>,
    current: &[(String, DailySavingsBucket)],
) -> Vec<(String, DailySavingsBucket)> {
    current
        .iter()
        .filter_map(|(hour_key, bucket)| {
            let prior = previous.get(hour_key).copied().unwrap_or_default();
            let delta = DailySavingsBucket {
                estimated_savings_usd: (bucket.estimated_savings_usd - prior.estimated_savings_usd)
                    .max(0.0),
                estimated_tokens_saved: bucket
                    .estimated_tokens_saved
                    .saturating_sub(prior.estimated_tokens_saved),
                actual_cost_usd: (bucket.actual_cost_usd - prior.actual_cost_usd).max(0.0),
                total_tokens_sent: bucket
                    .total_tokens_sent
                    .saturating_sub(prior.total_tokens_sent),
                new_input_tokens: bucket
                    .new_input_tokens
                    .saturating_sub(prior.new_input_tokens),
                output_savings_usd: (bucket.output_savings_usd - prior.output_savings_usd).max(0.0),
                output_tokens_saved: bucket
                    .output_tokens_saved
                    .saturating_sub(prior.output_tokens_saved),
                // Session observations carry no cache dimension.
                ..Default::default()
            };
            if delta.estimated_savings_usd <= 0.0
                && delta.estimated_tokens_saved == 0
                && delta.actual_cost_usd <= 0.0
                && delta.total_tokens_sent == 0
            {
                None
            } else {
                Some((hour_key.clone(), delta))
            }
        })
        .collect()
}

fn build_hourly_backfill_records(
    buckets: &[(String, DailySavingsBucket)],
    session_requests: usize,
    session_savings_usd: f64,
    session_tokens_saved: u64,
    session_actual_cost_usd: f64,
    session_total_tokens_sent: u64,
) -> Vec<SavingsRecord> {
    if buckets.is_empty() {
        return vec![SavingsRecord {
            schema_version: 7,
            id: Uuid::new_v4().to_string(),
            observed_at: Utc::now(),
            day_key: local_day_key(Local::now()),
            hour_key: local_hour_key(Local::now()),
            session_requests,
            session_estimated_savings_usd: session_savings_usd,
            session_estimated_tokens_saved: session_tokens_saved,
            session_actual_cost_usd,
            session_total_tokens_sent,
            delta_requests: session_requests,
            delta_estimated_savings_usd: 0.0,
            delta_estimated_tokens_saved: 0,
            delta_actual_cost_usd: 0.0,
            delta_total_tokens_sent: 0,
            source: "headroom_dashboard_backfill".into(),
        }];
    }

    let latest_index = buckets.len() - 1;
    buckets
        .iter()
        .enumerate()
        .map(|(index, (hour_key, bucket))| SavingsRecord {
            schema_version: 7,
            id: Uuid::new_v4().to_string(),
            observed_at: Utc::now(),
            day_key: day_key_from_hour_key(hour_key),
            hour_key: hour_key.clone(),
            session_requests: if index == latest_index {
                session_requests
            } else {
                0
            },
            session_estimated_savings_usd: if index == latest_index {
                session_savings_usd
            } else {
                0.0
            },
            session_estimated_tokens_saved: if index == latest_index {
                session_tokens_saved
            } else {
                0
            },
            session_actual_cost_usd: if index == latest_index {
                session_actual_cost_usd
            } else {
                0.0
            },
            session_total_tokens_sent: if index == latest_index {
                session_total_tokens_sent
            } else {
                0
            },
            delta_requests: if index == latest_index {
                session_requests
            } else {
                0
            },
            delta_estimated_savings_usd: bucket.estimated_savings_usd,
            delta_estimated_tokens_saved: bucket.estimated_tokens_saved,
            delta_actual_cost_usd: bucket.actual_cost_usd,
            delta_total_tokens_sent: bucket.total_tokens_sent,
            source: "headroom_dashboard_backfill".into(),
        })
        .collect()
}

fn build_hourly_delta_records(
    buckets: &[(String, DailySavingsBucket)],
    session_requests: usize,
    session_savings_usd: f64,
    session_tokens_saved: u64,
    session_actual_cost_usd: f64,
    session_total_tokens_sent: u64,
    delta_requests: usize,
) -> Vec<SavingsRecord> {
    if buckets.is_empty() {
        return Vec::new();
    }

    let latest_index = buckets.len() - 1;
    buckets
        .iter()
        .enumerate()
        .filter(|(_, (_, bucket))| bucket.actual_cost_usd > 0.0)
        .map(|(index, (hour_key, bucket))| SavingsRecord {
            schema_version: 7,
            id: Uuid::new_v4().to_string(),
            observed_at: Utc::now(),
            day_key: day_key_from_hour_key(hour_key),
            hour_key: hour_key.clone(),
            session_requests: if index == latest_index {
                session_requests
            } else {
                0
            },
            session_estimated_savings_usd: if index == latest_index {
                session_savings_usd
            } else {
                0.0
            },
            session_estimated_tokens_saved: if index == latest_index {
                session_tokens_saved
            } else {
                0
            },
            session_actual_cost_usd: if index == latest_index {
                session_actual_cost_usd
            } else {
                0.0
            },
            session_total_tokens_sent: if index == latest_index {
                session_total_tokens_sent
            } else {
                0
            },
            delta_requests: if index == latest_index {
                delta_requests
            } else {
                0
            },
            delta_estimated_savings_usd: bucket.estimated_savings_usd,
            delta_estimated_tokens_saved: bucket.estimated_tokens_saved,
            delta_actual_cost_usd: bucket.actual_cost_usd,
            delta_total_tokens_sent: bucket.total_tokens_sent,
            source: "headroom_dashboard".into(),
        })
        .collect()
}

fn find_u64_key_recursive(value: &Value, keys: &[&str]) -> Option<u64> {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                if keys
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate))
                {
                    if let Some(parsed) = parse_u64_value(v) {
                        return Some(parsed);
                    }
                }
                if let Some(found) = find_u64_key_recursive(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_u64_key_recursive(item, keys)),
        _ => None,
    }
}

fn find_f64_key_recursive(value: &Value, keys: &[&str]) -> Option<f64> {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                if keys
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate))
                {
                    if let Some(parsed) = parse_f64_value(v) {
                        return Some(parsed);
                    }
                }
                if let Some(found) = find_f64_key_recursive(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_f64_key_recursive(item, keys)),
        _ => None,
    }
}

fn parse_u64_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(num) => num
            .as_u64()
            .or_else(|| {
                num.as_i64()
                    .and_then(|v| if v >= 0 { Some(v as u64) } else { None })
            })
            .or_else(|| {
                num.as_f64()
                    .and_then(|v| if v >= 0.0 { Some(v as u64) } else { None })
            }),
        Value::String(text) => parse_u64_from_text(text),
        _ => None,
    }
}

fn parse_f64_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(num) => num.as_f64(),
        Value::String(text) => parse_f64_from_text(text),
        _ => None,
    }
}

fn parse_u64_from_text(text: &str) -> Option<u64> {
    let mut numeric = String::new();
    let mut started = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            numeric.push(ch);
            started = true;
            continue;
        }
        if started && (ch == ',' || ch == '_') {
            continue;
        }
        if started {
            break;
        }
    }
    if numeric.is_empty() {
        None
    } else {
        numeric.parse::<u64>().ok()
    }
}

fn parse_f64_from_text(text: &str) -> Option<f64> {
    let mut numeric = String::new();
    let mut started = false;
    for ch in text.chars() {
        let is_numeric = ch.is_ascii_digit() || ch == '.' || ch == '-';
        if is_numeric {
            numeric.push(ch);
            started = true;
            continue;
        }
        if started && (ch == ',' || ch == '_' || ch == '$' || ch.is_ascii_whitespace()) {
            continue;
        }
        if started {
            break;
        }
    }
    if numeric.is_empty() || numeric == "-" || numeric == "." {
        None
    } else {
        numeric.parse::<f64>().ok()
    }
}

pub(crate) fn headroom_proxy_reachable() -> bool {
    // Status/UI boundary: tolerant by design. The tight 1.5s probe flaps red
    // under load when the backend is busy with compression/embedding,
    // even though traffic still flows ("red light, works"). Use a 5s ceiling
    // matching the watchdog's tolerance — a healthy /readyz still answers in
    // milliseconds, so the dot stays responsive; the larger budget only bites
    // when the backend is genuinely slow.
    probe_proxy_readyz(Duration::from_secs(5))
}

/// The `error_hint` recorded for a boot-validation failure. `startup_hint` is
/// `classify_startup_error`'s reading of the new runtime's spawn error and is
/// only passed when the fallback did not restart either, so the banner can
/// say why nothing is running instead of just what was reverted.
pub(crate) fn boot_validation_error_hint(
    kind: RuntimeMaintenanceKind,
    rollback_restored: bool,
    restarted: bool,
    fallback_pkg_label: &str,
    startup_hint: Option<&str>,
) -> Option<String> {
    match kind {
        RuntimeMaintenanceKind::Upgrade if rollback_restored && restarted => Some(format!(
            "Reverted to headroom-ai {fallback_pkg_label} and restarted it."
        )),
        RuntimeMaintenanceKind::Upgrade if rollback_restored => Some(match startup_hint {
            Some(hint) => format!(
                "Reverted to headroom-ai {fallback_pkg_label}, but it didn't start either. {hint}"
            ),
            None => format!("Reverted to headroom-ai {fallback_pkg_label}."),
        }),
        RuntimeMaintenanceKind::Upgrade => startup_hint.map(str::to_string),
        RuntimeMaintenanceKind::RequirementsRepair if restarted => Some(
            "Headroom restarted with the repaired runtime, but validation still failed.".into(),
        ),
        RuntimeMaintenanceKind::RequirementsRepair => startup_hint.map(str::to_string),
    }
}

/// Turn a raw `last_startup_error` string (the anyhow chain from
/// `start_headroom_background`) into a short user-friendly explanation plus a
/// suggested next step. Returns `None` for shapes we don't recognize, in which
/// case the UI falls back to a generic "open logs" prompt.
pub(crate) fn classify_startup_error(raw: &str) -> Option<String> {
    // High-confidence endpoint protection signature: SIGKILL with no
    // app-side cause, dlopen-not-permitted, fresh-extension permission
    // denial, etc. Defer to the shared matcher in lib.rs so this list
    // doesn't drift from the install-time classifier.
    if crate::is_endpoint_protection_signal(raw) {
        return Some(crate::endpoint_protection_hint_runtime());
    }
    // WSAEACCES on the loopback socket asyncio needs before anything else
    // runs (RUST-CY): the chain also matches the generic exited-before-port
    // branch below, which would send the user to a traceback whose last line
    // is localized Windows prose. Two causes, two checks; the hint names both.
    if crate::is_loopback_socket_denied_signal(raw) {
        return Some(crate::loopback_socket_denied_hint());
    }
    if raw.contains("is occupied by a non-headroom process") {
        // Only reaches here when even the fallback port range was unavailable
        // (`tool_manager` scans 6768..=6790 before bailing). At that point the
        // user has 23 unrelated daemons in that range — a reboot is the only
        // realistic remediation, since common offenders like rapportd reset
        // their port at login.
        return Some(
            "A port Headroom needs is held by another app on your machine. \
             Reboot to clear stuck listeners, then relaunch Headroom."
                .into(),
        );
    }
    if raw.contains("headroom proxy already running on port") {
        return Some(
            "A previous Headroom proxy is still running in the background. \
             Quit and relaunch Headroom to reset it."
                .into(),
        );
    }
    if raw.contains("never opened port") {
        return Some(
            "The Headroom runtime took too long to start. \
             On first launch, macOS Gatekeeper can scan the bundled Python runtime for ~1-2 minutes. \
             Wait a moment and click Retry. If it keeps failing, open Headroom logs from Settings."
                .into(),
        );
    }
    // Incomplete/corrupted runtime: a headroom.* module is missing from the
    // installed venv (interrupted upgrade or partial extraction left an import
    // dangling -- e.g. registry.py importing headroom.providers.claude that was
    // never laid down; see Sentry RUST-3Y). Must precede the generic
    // exited-before-port branch, whose message is vaguer. A full reinstall is
    // the reliable fix, so name it directly.
    // The backend prints its whole banner, then dies before binding: its
    // native deps (onnxruntime/torch) need the MSVC runtime, which a bare
    // Windows box does not ship (RUST-8V/8W, and RUST-7W for the non-fatal
    // prefetch half of the same cause). Without this branch the user gets the
    // generic "crashed at startup, read the logs" -- the log DOES name the
    // missing redistributable, three lines above the death, but nobody reads
    // that far. The warning text comes from onnxruntime's Python, so it is
    // English on every locale; do not match Windows' localized DLL errors.
    // Must precede the exited-before-port branch, which would swallow it.
    if raw.contains("Visual C++ Redistributable is not installed") {
        return Some(
            "Headroom's runtime needs the Microsoft Visual C++ Redistributable, \
             which is missing on this machine -- its native libraries fail to load without it. \
             Install it from https://aka.ms/vs/17/release/vc_redist.x64.exe, then relaunch Headroom."
                .into(),
        );
    }
    // `encodings` is the first stdlib module CPython imports; its absence is
    // the base runtime's `Lib` tree being gone while `python.exe` survived
    // (RUST-C8). Same remedy as a missing headroom.* module, and the
    // installed gate now routes the next launch to bootstrap's reinstall.
    if raw.contains("ModuleNotFoundError: No module named 'headroom")
        || raw.contains("No module named 'encodings'")
    {
        return Some(
            "Headroom's runtime is missing some of its own files, so it can't start \
             -- the install looks incomplete or was interrupted. \
             Reinstall the runtime from Settings > Advanced to fix it."
                .into(),
        );
    }
    if raw.contains("exited with status") && raw.contains("before opening port") {
        return Some(
            "The Headroom Python runtime crashed at startup. \
             Open Headroom logs from Settings to see the traceback, \
             or reinstall the runtime from Settings > Advanced."
                .into(),
        );
    }
    None
}

/// Explain a failed intercept bind in the terms the user can act on.
///
/// Deliberately does NOT assert a cause. WSAEADDRINUSE (os error 10048) has
/// several: a leftover Headroom still holding the socket, an unrelated app on
/// the port, or a reserved range (Hyper-V / WSL2 / Docker). An earlier version
/// of this text claimed the reserved range outright and told users to run
/// `net stop winnat`, which fails on any machine where winnat is not even
/// running -- confidently wrong advice is worse than none. Name the port, hand
/// over the command that identifies the holder, and let the user look.
pub(crate) fn intercept_bind_hint(raw: &str) -> String {
    let port = crate::proxy_intercept::INTERCEPT_PORT;
    if raw.contains("os error 10048") {
        return format!(
            "Port {port} is already held by another process, so Headroom cannot open it and \
             clients get \"connection refused\". In PowerShell, \
             `Get-NetTCPConnection -LocalPort {port}` names the owning process: if it is a \
             leftover Headroom, quit it and relaunch. If nothing owns the port, check for a \
             reserved range with `netsh int ipv4 show excludedportrange protocol=tcp`."
        );
    }
    format!(
        "Headroom cannot open port {port}, so no client traffic can reach it ({raw}). \
         Another app is holding the port -- quit it, or reboot to clear stuck listeners."
    )
}

fn is_headroom_proxy_reachable() -> bool {
    probe_proxy_readyz(Duration::from_millis(1500))
}

/// Whether the runtime is already serving, so `ensure_headroom_running` can
/// return without spawning. Split out from the probes so the decision itself is
/// testable: the probes hit the network and shell out, this does not.
///
/// A reachable intercept is sufficient (the pre-existing rule). A healthy
/// backend alone is NOT: it must also be running this build's argv, or we would
/// adopt an older build's proxy and silently run a mismatched wheel.
fn runtime_already_serving(
    intercept_reachable: bool,
    backend_serving: bool,
    backend_argv_is_current: bool,
) -> bool {
    intercept_reachable || (backend_serving && backend_argv_is_current)
}

fn probe_proxy_readyz(timeout: Duration) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };

    ["127.0.0.1", "localhost"].iter().any(|host| {
        client
            .get(format!("http://{host}:6767/readyz"))
            .send()
            .map(proxy_readyz_response_is_reachable)
            .unwrap_or(false)
    })
}

/// Whether a `/readyz` response means the proxy is up and serving.
///
/// 2xx / 404 are reachable via [`proxy_readyz_status_is_reachable`]. A 503 whose
/// *only* failing check is `upstream` is also reachable: the process is alive
/// and answering, and the upstream-connectivity probe is cached 30s and
/// self-heals on the next refresh. Counting it as down flaps the UI banner
/// "crashed" on every transient network blip even though nothing restarted
/// (mirrors the watchdog's `readyz_failure_is_upstream_only`). Any other 503 /
/// 5xx stays not-reachable so the watchdog keeps waiting / restarting.
fn proxy_readyz_response_is_reachable(response: reqwest::blocking::Response) -> bool {
    let status = response.status();
    if proxy_readyz_status_is_reachable(status) {
        return true;
    }
    if status.as_u16() == 503 {
        return response
            .text()
            .map(|body| proxy_readyz_503_body_is_upstream_only(&body))
            .unwrap_or(false);
    }
    false
}

/// Whether a `/readyz` HTTP status alone means the proxy is up and serving.
///
/// 2xx is ready. A 404 means an older proxy build that predates the `/readyz`
/// route is answering -- it's up and serving traffic, just lacks the endpoint,
/// so it must count as reachable or the watchdog auto-pauses a working proxy
/// (Sentry RUST-2X). A 503 is inconclusive from the status line alone --
/// `proxy_readyz_response_is_reachable` inspects the body to tell an
/// upstream-only blip from a wedged core; every other 5xx stays not-reachable.
fn proxy_readyz_status_is_reachable(status: reqwest::StatusCode) -> bool {
    status.is_success() || status == reqwest::StatusCode::NOT_FOUND
}

/// True when a 503 `/readyz` body's only unhealthy component is `upstream`.
/// Reuses the watchdog's per-check parser so both paths agree on what a
/// healthy-but-upstream-blipped process looks like. False when the body can't be
/// parsed (a bare 503 under load) -- conservative, matching the status-only path.
fn proxy_readyz_503_body_is_upstream_only(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .map(|json| crate::readyz_failed_checks_csv(&json) == "upstream")
        .unwrap_or(false)
}

/// Terminate a process tree. Windows uses taskkill /T (subtree); Unix signals
/// the process group by negating the pid.
/// How long `stop_headroom` waits for a concurrent lifecycle transition before
/// stopping the backend anyway.
const STOP_LIFECYCLE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// The last few app-initiated kills, for attribution when a spawn dies by
/// signal before binding (RUST-CA/CB/1K: SIGTERM after the banner on five
/// macOS hosts, sender unknown). A kill we sent moments earlier is the first
/// suspect; an empty or stale ring says the sender was external.
static RECENT_APP_KILLS: Mutex<std::collections::VecDeque<(Instant, &'static str, String)>> =
    Mutex::new(std::collections::VecDeque::new());
const RECENT_APP_KILLS_CAP: usize = 8;

pub(crate) fn note_app_kill(source: &'static str, detail: String) {
    let mut ring = RECENT_APP_KILLS.lock();
    if ring.len() == RECENT_APP_KILLS_CAP {
        ring.pop_front();
    }
    ring.push_back((Instant::now(), source, detail));
}

/// Oldest first, each as "<age>s ago <source>: <detail>".
pub(crate) fn recent_app_kills_summary() -> Vec<String> {
    let now = Instant::now();
    RECENT_APP_KILLS
        .lock()
        .iter()
        .map(|(at, source, detail)| {
            format!(
                "{}s ago {source}: {detail}",
                now.duration_since(*at).as_secs()
            )
        })
        .collect()
}

fn terminate_process_tree(pid: i32, force: bool) {
    if cfg!(target_os = "windows") {
        let mut command = crate::proc::command("taskkill");
        command.args(["/PID", &pid.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        let _ = command.status();
    } else {
        let Some(target) = group_kill_target(pid) else {
            log::error!("refusing to signal process group for pid {pid}: not a pid we spawned");
            return;
        };
        let signal = if force { "-KILL" } else { "-TERM" };
        // Attribution: this is the only signal we send that reaches processes we
        // did not spawn, so the group has to be in the log to be provable later.
        log::info!("terminate_process_tree: {signal} to process group {target}");
        note_app_kill("terminate_process_tree", format!("{signal} group {target}"));
        let _ = crate::proc::command("/bin/kill")
            .arg(signal)
            .arg(target)
            .status();
    }
}

/// The `kill` argument for signalling `pid`'s process group, or `None` when
/// `pid` must never be used as a group target.
///
/// `kill -TERM -0` does not mean "no process", it means "every process in MY
/// group" - and on a Linux desktop this app can share its group with the login
/// session, so a 0 here SIGTERMs xfce4-session, the window manager and the rest
/// of the session out from under the user. Pid 1 is init. Neither is ever a
/// backend we spawned, so neither is worth the blast radius.
fn group_kill_target(pid: i32) -> Option<String> {
    (pid > 1).then(|| format!("-{pid}"))
}

/// NTSTATUS 0xC0000142, surfaced by `ExitStatus::code()` as -1073741502 (the
/// number in RUST-7N's title). Not cfg(windows)-gated so the conversion test
/// below runs everywhere.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const STATUS_DLL_INIT_FAILED: i32 = 0xC0000142_u32 as i32;

/// True when a powershell exit code means "the user session is ending", not
/// "the sweep failed". Same rationale for every member (see the RUST-7N note
/// at the call site): logoff/shutdown reaps the whole session, orphans
/// included, so a sweep that was cut short here has nothing left to do, and
/// the next launch's reclaim_orphan_proxy covers any survivor.
/// - 0xC0000142 STATUS_DLL_INIT_FAILED: powershell cannot even start (RUST-7N).
/// - 0x40010004 DBG_TERMINATE_PROCESS: powershell was terminated by the
///   session's console-control teardown mid-run (RUST-9A).
/// - 0xC000013A STATUS_CONTROL_C_EXIT: same teardown, delivered as ctrl-close.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn is_session_teardown_exit(code: i32) -> bool {
    const DBG_TERMINATE_PROCESS: i32 = 0x40010004;
    const STATUS_CONTROL_C_EXIT: i32 = 0xC000013A_u32 as i32;
    matches!(
        code,
        STATUS_DLL_INIT_FAILED | DBG_TERMINATE_PROCESS | STATUS_CONTROL_C_EXIT
    )
}

/// Exit code the Windows sweep script uses for "the process enumeration itself
/// failed" (WMI unavailable or access-denied), as opposed to powershell's own
/// exit codes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const PS_SWEEP_ENUMERATION_FAILED: i32 = 3;

/// Escape a value for use inside a single-quoted PowerShell `-like` pattern.
/// `[`/`]` are wildcard metacharacters to `-like`, and an embedded `'` would
/// close the string literal early -- a Windows username containing `'` could
/// otherwise break out of it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn escape_powershell_like(value: &str) -> String {
    value
        .replace('`', "``")
        .replace('\'', "''")
        .replace('[', "`[")
        .replace(']', "`]")
}

/// The PowerShell one-liner that force-kills every process whose command line
/// matches both the executable and the argument pattern.
///
/// `Win32_Process.CommandLine` wraps the executable in double quotes (e.g.
/// `"C:\...\python.exe" -m headroom.proxy.server`), so matching
/// "{exe} {args_pattern}" as one substring never hits -- the quote right after
/// the exe breaks the adjacency. Match the exe and the args as two independent
/// `-like` clauses instead.
///
/// Excluding our own PID matters: this powershell process's `CommandLine`
/// embeds both `-like` patterns as literals, so without the guard it matches
/// its own filter and force-kills itself mid-pipeline -- exit -1, and the real
/// targets after it in the enumeration are never killed (RUST-6F/6G/6H: 44
/// events on the first 0.7.7 Windows install).
///
/// The script reports its own verdict rather than letting powershell infer one.
/// `powershell -Command` exits 1 whenever the last pipeline element left `$?`
/// false, and `-ErrorAction SilentlyContinue` suppresses the message without
/// clearing that flag -- so a `Stop-Process` against a pid that exited on its
/// own between the enumeration and the kill (the common case here: we just
/// asked the proxy to stop) made a fully successful sweep look like a failure.
/// That is the second RUST-6F/RUST-6G wave: 22 warnings from a machine where
/// nothing was wrong. `exit 0` means the sweep ran; `PS_SWEEP_ENUMERATION_FAILED`
/// means the enumeration itself failed, which is the only outcome worth a
/// report. A powershell that cannot start at all reaches neither and still
/// surfaces through its own exit code (see `is_session_teardown_exit`).
/// ponytail: a Stop-Process the machine's policy denies still exits 0, so a
/// matched pid that SURVIVES the sweep is invisible here. For
/// kill_venv_lock_holders the follow-up pip run fails loudly on the held
/// lock, which is where that case surfaces today; count failed kills in the
/// script if that ever stops being true.
///
/// Parent filter (same rule as the unix sweep, see `sweep_should_kill`): a
/// match is killed only when its parent is gone (Windows never reparents an
/// orphan, so its ParentProcessId names a dead pid) or, when
/// `include_own_children`, when its parent is this app (`self_pid`). A match
/// whose parent is another live process is a relaunched instance's backend or
/// a sibling thread's in-flight spawn. `Stop-Process` is `TerminateProcess(-1)`,
/// so that victim reports `exit code: 0xffffffff before opening port` with the
/// banner printed and a clean onnx probe -- the "silent" half of RUST-9F
/// (RUST-CD: three unguarded stops in 12s, then exactly that failure).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_process_sweep_script(
    exe: &std::path::Path,
    args_pattern: &str,
    self_pid: u32,
    include_own_children: bool,
) -> String {
    let exe_pattern = escape_powershell_like(&exe.display().to_string());
    let args_escaped = escape_powershell_like(args_pattern);
    let own = if include_own_children {
        "$true"
    } else {
        "$false"
    };
    format!(
        "try {{ $me = {self_pid}; Get-CimInstance Win32_Process -ErrorAction Stop \
         | Where-Object {{ $_.ProcessId -ne $PID -and $_.ProcessId -ne $me \
         -and $_.CommandLine -like '*{exe_pattern}*' \
         -and $_.CommandLine -like '*{args_escaped}*' \
         -and (($_.ParentProcessId -eq $me -and {own}) \
         -or -not (Get-Process -Id $_.ParentProcessId -ErrorAction SilentlyContinue)) }} \
         | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }} }} \
         catch {{ exit {PS_SWEEP_ENUMERATION_FAILED} }}; exit 0"
    )
}

/// Whether the sweep may signal a matching process, given who its parent is.
///
/// The sweep exists to reap proxies nobody holds a handle to: orphans of a
/// previous app instance (reparented to pid 1) and children of this process
/// whose handle was lost. A match whose parent is some OTHER live process is
/// not ours to kill: the command line is the same for every Headroom instance,
/// and a quitting instance's sweep was landing on the proxy the freshly
/// relaunched instance was still bringing up - SIGTERM after the banner,
/// before the port, on both spawn variants (RUST-CA/CB, RUST-1K: five macOS
/// hosts). `include_own_children` is false when the caller could not take the
/// lifecycle lock: then a sibling transition in this process is mid-spawn and
/// its child is likewise off limits.
fn sweep_should_kill(ppid: u32, self_pid: u32, include_own_children: bool) -> bool {
    ppid <= 1 || (include_own_children && ppid == self_pid)
}

/// Parses `ps -o pid=,ppid=` output into `(pid, ppid)` pairs; junk lines skip.
fn parse_pid_ppid(output: &str) -> Vec<(u32, u32)> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect()
}

fn kill_processes_by_command_pattern(
    exe: &std::path::Path,
    args_pattern: &str,
    include_own_children: bool,
) -> Result<()> {
    // An unresolved runtime path degrades the pattern from "our backend at this
    // exact path" to a loose substring, and `pkill -f` applies it to every
    // process the user owns. Refuse rather than guess.
    if exe.parent().is_none() || exe.as_os_str().is_empty() {
        return Err(anyhow!(
            "refusing to pkill with an unresolved executable path {exe:?}"
        ));
    }

    #[cfg(unix)]
    {
        // pgrep + a parent check instead of pkill: see sweep_should_kill.
        let pattern = format!("{} {args_pattern}", exe.display());
        let found = crate::proc::command("pgrep")
            .args(["-f", &pattern])
            .output()
            .with_context(|| format!("running pgrep for pattern '{pattern}'"))?;
        // pgrep exits 1 for "no match".
        if !found.status.success() {
            if found.status.code() == Some(1) {
                return Ok(());
            }
            return Err(anyhow!(
                "pgrep exited with status {:?} for pattern '{}'",
                found.status.code(),
                pattern
            ));
        }
        let pids: Vec<String> = String::from_utf8_lossy(&found.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if pids.is_empty() {
            return Ok(());
        }
        let listed = crate::proc::command("ps")
            .args(["-o", "pid=,ppid=", "-p", &pids.join(",")])
            .output()
            .with_context(|| format!("running ps for pids {pids:?}"))?;
        let self_pid = std::process::id();
        for (pid, ppid) in parse_pid_ppid(&String::from_utf8_lossy(&listed.stdout)) {
            if pid == self_pid {
                continue;
            }
            if !sweep_should_kill(ppid, self_pid, include_own_children) {
                log::info!(
                    "process sweep: leaving pid {pid} (parent {ppid} is alive and not us) for '{pattern}'"
                );
                continue;
            }
            log::info!("process sweep: -TERM pid {pid} (parent {ppid}) for '{pattern}'");
            note_app_kill("process_sweep", format!("-TERM pid {pid} (parent {ppid})"));
            let _ = crate::proc::command("/bin/kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        let script = windows_process_sweep_script(
            exe,
            args_pattern,
            std::process::id(),
            include_own_children,
        );
        let status = crate::proc::command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .with_context(|| {
                format!(
                    "running powershell kill for exe '{}' args '{args_pattern}'",
                    exe.display()
                )
            })?;

        if status.success() {
            return Ok(());
        }

        // STATUS_DLL_INIT_FAILED: powershell cannot start because the user
        // session is ending (logoff/shutdown reaches stop_headroom via
        // WM_ENDSESSION). The logoff reaps every process in the session,
        // orphans included, and the next launch's reclaim_orphan_proxy covers
        // anything that survives - a sweep that cannot run here has nothing
        // left to do (RUST-7N). The venv-lock-holder caller loses nothing
        // either: if powershell is this broken outside a logoff, the pip
        // install right after fails with its own actionable error.
        if status.code().is_some_and(is_session_teardown_exit) {
            log::info!(
                "powershell exited with session-teardown status {:?}; skipping process sweep for '{}'",
                status.code(),
                exe.display()
            );
            return Ok(());
        }

        if status.code() == Some(PS_SWEEP_ENUMERATION_FAILED) {
            return Err(anyhow!(
                "powershell could not enumerate processes (Win32_Process query failed) \
                 for exe '{}' args '{}'",
                exe.display(),
                args_pattern
            ));
        }

        return Err(anyhow!(
            "powershell exited with status {:?} for exe '{}' args '{}'",
            status.code(),
            exe.display(),
            args_pattern
        ));
    }

    #[cfg(all(not(unix), not(target_os = "windows")))]
    {
        let _ = (exe, args_pattern);
        Ok(())
    }
}

/// Kill every process whose command line references the managed venv
/// directory. Windows-only: pip cannot overwrite files a running process
/// holds open, so an upgrade's `--force-reinstall` — and the rollback that
/// retries the same operation — both die with permission errors when an
/// IDE-spawned MCP server or stray python is still running from the venv
/// (RUST-6Z/70: install failed, restored=false, runtime bricked).
/// `stop_headroom` doesn't cover these: it only matches the proxy's own
/// command patterns. Unix replaces in-use files fine, so this is a no-op
/// there. Identity is verified by the venv path in the command line, never
/// by port.
pub(crate) fn kill_venv_lock_holders(venv_dir: &std::path::Path) {
    if !cfg!(target_os = "windows") {
        return;
    }
    // Empty args pattern makes the exe-path clause the only real filter:
    // any process whose command line mentions the venv dir.
    if let Err(err) = kill_processes_by_command_pattern(venv_dir, "", true) {
        log::warn!("killing venv lock holders before venv mutation failed: {err:#}");
    }
}

/// Merge daily savings from tracker (pre-cutoff) and native headroom history (post-cutoff).
/// Drop the first bucket of a backend rollup series when the local tracker
/// already covers an earlier period.
///
/// The backend's rollups start accumulating the day the feature lands, and the
/// first bucket's delta is measured against zero — so it carries every request
/// that predates the series. Observed 2026-08-06: a single daily bucket holding
/// $5,134 of compression savings, against $5,382 of lifetime. Counting it
/// alongside the tracker's own per-day record of the same period would nearly
/// double the headline total (the series is also re-trimmed over time, so that
/// backfill bucket moves forward and the double count follows it).
///
/// A tracker with no earlier coverage means the bucket is the only record of
/// that history, so it stays. When the tracker does hold the same date, the
/// merge falls back to its value, so nothing is lost.
fn drop_rollup_backfill<T>(
    native: Vec<T>,
    earliest_local: Option<&str>,
    key: impl Fn(&T) -> &str,
) -> Vec<T> {
    let Some(earliest_local) = earliest_local else {
        return native;
    };
    let Some(first) = native.iter().map(|p| key(p).to_string()).min() else {
        return native;
    };
    if earliest_local >= first.as_str() {
        return native;
    }
    native.into_iter().filter(|p| key(p) != first).collect()
}

/// A rollup bucket whose leading against-zero delta can be settled exactly by
/// subtracting the ring's starting totals (see `settle_rollup_backfill`).
trait BackfillSettle {
    fn subtract_ring_start(&mut self, start: &RingStartTotals);
    fn is_empty_after_settle(&self) -> bool;
}

impl BackfillSettle for DailySavingsPoint {
    fn subtract_ring_start(&mut self, start: &RingStartTotals) {
        self.estimated_tokens_saved = self
            .estimated_tokens_saved
            .saturating_sub(start.tokens_saved);
        self.estimated_savings_usd =
            (self.estimated_savings_usd - start.compression_savings_usd).max(0.0);
        self.total_tokens_sent = self
            .total_tokens_sent
            .saturating_sub(start.total_input_tokens);
        self.actual_cost_usd = (self.actual_cost_usd - start.total_input_cost_usd).max(0.0);
        self.output_tokens_saved = self
            .output_tokens_saved
            .saturating_sub(start.output_tokens_saved);
        self.output_savings_usd = (self.output_savings_usd - start.output_savings_usd).max(0.0);
    }
    fn is_empty_after_settle(&self) -> bool {
        self.estimated_tokens_saved == 0 && self.total_tokens_sent == 0
    }
}

impl BackfillSettle for HourlySavingsPoint {
    fn subtract_ring_start(&mut self, start: &RingStartTotals) {
        self.estimated_tokens_saved = self
            .estimated_tokens_saved
            .saturating_sub(start.tokens_saved);
        self.estimated_savings_usd =
            (self.estimated_savings_usd - start.compression_savings_usd).max(0.0);
        self.total_tokens_sent = self
            .total_tokens_sent
            .saturating_sub(start.total_input_tokens);
        self.actual_cost_usd = (self.actual_cost_usd - start.total_input_cost_usd).max(0.0);
        self.output_tokens_saved = self
            .output_tokens_saved
            .saturating_sub(start.output_tokens_saved);
        self.output_savings_usd = (self.output_savings_usd - start.output_savings_usd).max(0.0);
    }
    fn is_empty_after_settle(&self) -> bool {
        self.estimated_tokens_saved == 0 && self.total_tokens_sent == 0
    }
}

/// Like `drop_rollup_backfill`, but exact when the payload's raw ring is
/// available: the leading bucket's spurious content is precisely the ring's
/// starting cumulative totals, so subtract those and keep whatever real
/// traffic remains. Whole-bucket dropping stays as the fallback without a
/// ring, and for a bucket the subtraction empties. Dropping unconditionally
/// cost a full real day when the backend data dir was fresh (ring start
/// ~zero): the tracker predating the ring made today look like backfill, and
/// the lifetime card fell back to the tracker's partial observation
/// (2026-08-27: $0.50 shown against a $1.24 day).
fn settle_rollup_backfill<T: BackfillSettle>(
    native: Vec<T>,
    earliest_local: Option<&str>,
    ring_start: Option<&RingStartTotals>,
    key: impl Fn(&T) -> &str,
) -> Vec<T> {
    let Some(ring_start) = ring_start else {
        return drop_rollup_backfill(native, earliest_local, key);
    };
    let Some(earliest_local) = earliest_local else {
        return native;
    };
    let Some(first) = native.iter().map(|p| key(p).to_string()).min() else {
        return native;
    };
    if earliest_local >= first.as_str() {
        return native;
    }
    let mut native = native;
    if let Some(point) = native.iter_mut().find(|p| key(p) == first) {
        point.subtract_ring_start(ring_start);
    }
    native.retain(|p| key(p) != first.as_str() || !p.is_empty_after_settle());
    native
}

/// What a cached prefix token costs relative to a fresh input token.
///
/// Anthropic bills a cache read at 10% of the input rate; OpenAI's discount is
/// shallower, so 10% is the conservative choice across providers.
const CACHE_READ_PRICE_RATIO: f64 = 0.10;

/// The most any provider plausibly bills for a single input token, in USD per
/// million. Claude Fable 5 tops the Anthropic table at $10/M, but OpenAI's pro
/// tiers go higher (o3-pro $20/M; o1-pro is $150/M but rare enough to accept a
/// false fire on). $25 clears a blended o3-pro-heavy mix while staying under
/// the ~$33/M signature the 0.36.0 tool-schema contamination produced -- the
/// event this canary exists to catch. RUST-89's lone post-b86b91b event was an
/// o3-pro-class mix reading $20.11/M on the pinned, uncontaminated wheel.
const MAX_PLAUSIBLE_INPUT_USD_PER_M: f64 = 25.0;

/// True when the buckets imply a savings $/token that no provider charges for
/// an input token. A saved input token is worth exactly the rate it would have
/// been billed at, so a rate above every published input price means the
/// upstream wheel changed what `compression_savings_usd` contains (0.36.0
/// folded full-input-rate tool-schema dollars into it, implying $33/M).
///
/// The first cut compared against the rate actually paid on the same buckets --
/// self-calibrating, and wrong: `total_tokens_sent` counts provider cache
/// reads, which bill at a tenth of fresh input, so the paid rate is diluted by
/// the user's cache hit rate. Past ~37% reads that dilution alone clears a 1.5x
/// check, and Claude Code routinely runs 80%+. RUST-89/8C is what that cost: 12
/// events across 9 hosts on the pinned, uncontaminated 0.35.0, plus a live
/// /stats here reading 5.9x. The buckets carry no cache-free denominator
/// (`cache_read_tokens` is None for any day the app did not observe), so the
/// check is an absolute ceiling instead. Price paid: contamination that stays
/// under the ceiling on a cheap-model mix goes unseen.
fn savings_rate_implausible(daily_savings: &[DailySavingsPoint]) -> Option<f64> {
    let saved_usd: f64 = daily_savings.iter().map(|p| p.estimated_savings_usd).sum();
    let saved_tokens: u64 = daily_savings.iter().map(|p| p.estimated_tokens_saved).sum();
    // Volume floors: a handful of cheap requests is noise, not a semantics change.
    if saved_tokens < 1_000_000 || saved_usd < 1.0 {
        return None;
    }
    let savings_per_m = saved_usd / saved_tokens as f64 * 1e6;
    (savings_per_m > MAX_PLAUSIBLE_INPUT_USD_PER_M).then_some(savings_per_m)
}

/// Warn-only canary, once per process: a contaminated rate silently corrected
/// is worse than a loud one, so nothing is clamped -- the warn reaches Sentry
/// through the log bridge and the numbers keep rendering as reported.
fn warn_once_if_savings_rate_implausible(
    daily_savings: &[DailySavingsPoint],
    installed_wheel: impl FnOnce() -> Option<String>,
) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if let Some(savings_per_m) = savings_rate_implausible(daily_savings) {
        WARNED.store(true, std::sync::atomic::Ordering::Relaxed);
        // The raw sums and wheel version make the event self-diagnosing: a
        // legit expensive-model mix, a foreign backend on 6767, and real
        // contamination are indistinguishable from the rate alone (RUST-89,
        // 2026-09-01: $93.70/M on a 0.35.0-pinned install, undecidable).
        let saved_usd: f64 = daily_savings.iter().map(|p| p.estimated_savings_usd).sum();
        let saved_tokens: u64 = daily_savings.iter().map(|p| p.estimated_tokens_saved).sum();
        let wheel = installed_wheel().unwrap_or_else(|| "unknown".into());
        log::warn!(
            "savings rate implausible: buckets imply ${savings_per_m:.2}/M saved (${saved_usd:.2} \
             across {saved_tokens} tokens, wheel {wheel}), above the \
             ${MAX_PLAUSIBLE_INPUT_USD_PER_M:.2}/M ceiling for an input token; upstream savings \
             semantics likely changed under the pinned wheel"
        );
    }
}

/// Lifetime dollars saved by tool-schema deferral.
///
/// Tool definitions are re-sent on every request and sit at the very front of
/// the prompt, so on all but the first request of a session they would have
/// been billed as cache reads, not fresh input. Pricing them at the full input
/// rate -- the way compression is priced -- would overstate this layer roughly
/// tenfold, so it gets the cache-read rate instead.
///
/// The per-token input rate is the one the compression layer already implies
/// from the same buckets, so a user on cheaper models is priced at their own
/// models' rates. Returns 0 until there are enough compression buckets to
/// derive a rate from.
/// Per-bucket twin of [`tool_schema_savings_usd`]: prices one bucket's deferral
/// with the blended $/token that same bucket's compression implies, at the
/// cache-read ratio. Falls back to zero rather than guessing when the bucket
/// has no compression to blend from -- a made-up price on an empty bucket
/// would be indistinguishable from real savings.
fn bucket_tool_schema_usd(bucket_usd: f64, bucket_tokens: u64, tool_tokens: u64) -> f64 {
    if tool_tokens == 0 || bucket_tokens == 0 || bucket_usd <= 0.0 {
        return 0.0;
    }
    (bucket_usd / bucket_tokens as f64) * CACHE_READ_PRICE_RATIO * tool_tokens as f64
}

fn tool_schema_savings_usd(daily_savings: &[DailySavingsPoint], tokens_saved: u64) -> f64 {
    if tokens_saved == 0 {
        return 0.0;
    }
    let usd: f64 = daily_savings.iter().map(|p| p.estimated_savings_usd).sum();
    let tokens: u64 = daily_savings.iter().map(|p| p.estimated_tokens_saved).sum();
    if tokens == 0 || usd <= 0.0 {
        return 0.0;
    }
    (usd / tokens as f64) * CACHE_READ_PRICE_RATIO * tokens_saved as f64
}

/// Lifetime dollars saved by output shaping.
///
/// The daily buckets only carry this layer from the day the backend's rollups
/// started reporting it, which is far later than the shaper itself started
/// working: on 2026-08-06 the buckets held 2.66M output tokens across 4 days
/// while the shaper's own durable estimator held 17.38M across 48,638 requests.
/// Summing the buckets therefore understates the layer roughly six-fold.
///
/// The estimator has no timestamps -- it is a set of stratified running sums --
/// so its total can only be a lifetime figure, and it is priced with the
/// blended $/token the tracked buckets already imply rather than a hardcoded
/// rate, so a user on cheaper models is priced at their own models' rates.
///
/// Falls back to the bucket sum whenever the estimator is absent, is smaller
/// (a re-seeded baseline), or the buckets carry no rate to price with. The two
/// sources measure the same layer, so this replaces the bucket sum, never adds
/// to it. `estimator_tokens_saved` is the live `/stats` reading when the
/// backend is up, or the tracker's persisted last reading during cold start.
fn lifetime_output_savings_usd(
    daily_savings: &[DailySavingsPoint],
    estimator_tokens_saved: Option<u64>,
) -> f64 {
    let bucket_usd: f64 = daily_savings.iter().map(|p| p.output_savings_usd).sum();
    let bucket_tokens: u64 = daily_savings.iter().map(|p| p.output_tokens_saved).sum();

    let Some(tokens_saved) = estimator_tokens_saved else {
        return bucket_usd;
    };
    if bucket_tokens == 0 || bucket_usd <= 0.0 || tokens_saved <= bucket_tokens {
        return bucket_usd;
    }

    let usd_per_token = bucket_usd / bucket_tokens as f64;
    usd_per_token * tokens_saved as f64
}

/// For days before `cutoff_date` (exclusive), the tracker is preferred.
/// For days on/after `cutoff_date`, native history is preferred.
/// Falls back to whichever source has data when the preferred one is absent.
fn merge_daily_savings(
    tracker: Vec<DailySavingsPoint>,
    history: Vec<DailySavingsPoint>,
    cutoff_date: &str,
) -> Vec<DailySavingsPoint> {
    use std::collections::BTreeMap;
    // Index the local tracker by date so a desynced history point can fall back
    // to it (see the zero-spend guard below).
    let tracker_by_date: BTreeMap<String, DailySavingsPoint> = tracker
        .iter()
        .map(|p| (p.date.clone(), p.clone()))
        .collect();

    let mut by_date: BTreeMap<String, DailySavingsPoint> = BTreeMap::new();
    // Post-cutoff: history wins, tracker fills gaps so today's local activity still shows.
    // Pre-cutoff: tracker-only; history is ignored to avoid pulling in pre-v6 schema drift.
    for p in history {
        if p.date.as_str() >= cutoff_date {
            // The backend rollup transiently reports compression savings with zero
            // tokens/cost when its cost counter lags the savings accumulator (a
            // desync that self-heals; see RUST-3S/3V). When that happens and the
            // local tracker recorded real spend that day, prefer the tracker point
            // rather than surfacing a savings-with-zero-spend day.
            let history_desynced = p.estimated_savings_usd > 0.000_001
                && p.actual_cost_usd == 0.0
                && p.total_tokens_sent == 0;
            if history_desynced {
                if let Some(t) = tracker_by_date.get(p.date.as_str()) {
                    if t.total_tokens_sent > 0 {
                        by_date.insert(p.date.clone(), t.clone());
                        continue;
                    }
                }
            }
            by_date.insert(p.date.clone(), p);
        }
    }
    for p in tracker {
        if p.date.as_str() < cutoff_date {
            by_date.insert(p.date.clone(), p);
        } else {
            match by_date.entry(p.date.clone()) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    // History wins the bucket, but the backend rollup has no
                    // new-input dimension: keep the locally-sampled value so
                    // the new-input rate keeps its coverage.
                    let merged = entry.get_mut();
                    merged.new_input_tokens = merged.new_input_tokens.max(p.new_input_tokens);
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(p);
                }
            }
        }
    }
    by_date.into_values().collect()
}

/// Same logic as `merge_daily_savings` but for hourly buckets keyed by hour string.
fn merge_hourly_savings(
    tracker: Vec<HourlySavingsPoint>,
    history: Vec<HourlySavingsPoint>,
    cutoff_hour: &str,
) -> Vec<HourlySavingsPoint> {
    use std::collections::BTreeMap;
    let mut by_hour: BTreeMap<String, HourlySavingsPoint> = BTreeMap::new();
    for p in history {
        if p.hour.as_str() >= cutoff_hour {
            by_hour.insert(p.hour.clone(), p);
        }
    }
    for p in tracker {
        if p.hour.as_str() < cutoff_hour {
            by_hour.insert(p.hour.clone(), p);
        } else {
            match by_hour.entry(p.hour.clone()) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    // See merge_daily_savings: rollups carry no new-input.
                    let merged = entry.get_mut();
                    merged.new_input_tokens = merged.new_input_tokens.max(p.new_input_tokens);
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(p);
                }
            }
        }
    }
    by_hour.into_values().collect()
}

fn begin_bootstrap_transition(
    current: &BootstrapProgress,
    python_installed: bool,
) -> (BootstrapProgress, Result<(), String>) {
    if python_installed {
        return (
            BootstrapProgress {
                running: false,
                complete: true,
                failed: false,
                current_step: "Install complete".into(),
                message: "Managed runtime already installed.".into(),
                current_step_eta_seconds: 0,
                overall_percent: 100,
            },
            Ok(()),
        );
    }
    if current.running {
        return (current.clone(), Err("Bootstrap is already running.".into()));
    }
    (
        BootstrapProgress {
            running: true,
            complete: false,
            failed: false,
            current_step: "Preparing install".into(),
            message: "Initializing installer workflow.".into(),
            current_step_eta_seconds: 3,
            overall_percent: 2,
        },
        Ok(()),
    )
}

fn apply_bootstrap_step(
    _current: &BootstrapProgress,
    step: BootstrapStepUpdate,
) -> BootstrapProgress {
    BootstrapProgress {
        running: true,
        complete: false,
        failed: false,
        current_step: step.step.into(),
        message: step.message,
        current_step_eta_seconds: step.eta_seconds,
        overall_percent: step.percent,
    }
}

fn bootstrap_complete_state() -> BootstrapProgress {
    BootstrapProgress {
        running: false,
        complete: true,
        failed: false,
        current_step: "Install complete".into(),
        message: "Headroom is ready.".into(),
        current_step_eta_seconds: 0,
        overall_percent: 100,
    }
}

fn bootstrap_failed_state(current: &BootstrapProgress, message: String) -> BootstrapProgress {
    BootstrapProgress {
        running: false,
        complete: false,
        failed: true,
        current_step: "Install failed".into(),
        message,
        current_step_eta_seconds: 0,
        overall_percent: current.overall_percent.max(1),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn strip_extended_length_prefix_handles_windows_and_unix_forms() {
        let f = super::strip_extended_length_prefix;
        assert_eq!(
            f(r"\\?\C:\Users\garm\code\headroom-desktop-main".into()),
            r"C:\Users\garm\code\headroom-desktop-main"
        );
        assert_eq!(
            f(r"\\?\UNC\server\share\proj".into()),
            r"\\server\share\proj"
        );
        assert_eq!(
            f("/Users/garm/code/headroom-desktop".into()),
            "/Users/garm/code/headroom-desktop"
        );
    }

    #[test]
    fn status_dll_init_failed_matches_the_exit_code_sentry_reports() {
        // RUST-7N's title says "powershell exited with status Some(-1073741502)";
        // the benign-classification in kill_processes_by_command_pattern only
        // works if the hex constant converts to exactly that decimal.
        assert_eq!(super::STATUS_DLL_INIT_FAILED, -1073741502);
    }

    #[test]
    fn session_teardown_exits_are_benign_but_real_failures_are_not() {
        // RUST-9A's title says "powershell exited with status Some(1073807364)"
        // (0x40010004, DBG_TERMINATE_PROCESS: the session killed powershell
        // mid-sweep). Teardown statuses must be classified benign; a plain
        // script failure (exit 1) or the self-kill of RUST-6F (-1) must not.
        use super::is_session_teardown_exit;
        assert!(is_session_teardown_exit(super::STATUS_DLL_INIT_FAILED));
        assert!(is_session_teardown_exit(1073807364)); // DBG_TERMINATE_PROCESS
        assert!(is_session_teardown_exit(-1073741510)); // STATUS_CONTROL_C_EXIT
        assert!(!is_session_teardown_exit(0));
        assert!(!is_session_teardown_exit(1));
        assert!(!is_session_teardown_exit(-1));
    }

    #[test]
    fn stats_fetch_failure_category_splits_the_grab_bag() {
        // RUST-6V held 53 timeouts + 47 404s under one un-resolvable issue.
        use super::stats_fetch_failure_category as cat;
        assert_eq!(cat("timed out after 15s"), "timeout");
        assert_eq!(cat("HTTP 404 Not Found"), "http-404");
        assert_eq!(cat("HTTP 503 Service Unavailable"), "http-503");
        assert_eq!(cat("payload had no recognised savings fields"), "payload");
        assert_eq!(cat("no local host answered"), "unreachable");
        assert_eq!(
            cat("error sending request for url (http://127.0.0.1:6767/stats)"),
            "transport"
        );
    }

    use chrono::{Datelike, Local, TimeZone, Timelike, Utc};

    use crate::storage::{config_file, ensure_data_dirs, telemetry_file};

    use crate::models::{
        ActivityEvent, BootstrapProgress, DailySavingsPoint, HourlySavingsPoint,
        RuntimeUpgradeFailure, UpgradeFailurePhase,
    };
    use crate::tool_manager::BootstrapStepUpdate;

    use super::{
        aggregate_weekly_totals, apply_bootstrap_step, begin_bootstrap_transition,
        boot_validation_stalled, boot_validation_timed_out, bootstrap_complete_state,
        bootstrap_failed_state, classify_startup_error, cpu_time_advanced, drop_rollup_backfill,
        hf_cache_grew, intercept_bind_hint, lifetime_output_savings_usd,
        lifetime_token_milestones_crossed, log_mtime_advanced, merge_daily_savings,
        merge_hourly_savings, most_recent_monday, note_stats_fetch_success,
        parse_headroom_stats_from_json, parse_headroom_stats_history_from_json, parse_ps_cpu_time,
        proxy_readyz_503_body_is_upstream_only, proxy_readyz_status_is_reachable,
        rebuild_persisted_savings_from_records, savings_rate_implausible, settle_rollup_backfill,
        stats_fetch_warn_interval, support_tier_for_platform, tcp_port_accepts_connection,
        tool_schema_savings_usd, total_dir_size_bytes, warn_stats_fetch_failed, AppState,
        BootValidationOutcome, ClaudeProjectScan, DailySavingsBucket, Duration,
        HeadroomDashboardStats, HeadroomSavingsHistoryPoint, Instant, OutputSampleBucket,
        PersistedSavingsState, RingStartTotals, SavingsObservation, SavingsRecord, SavingsTracker,
        OUTPUT_SAMPLE_SERIES_VERSION, STATS_FETCH_RECOVERED_AT, STATS_FETCH_RECOVERY_WINDOW,
        STATS_FETCH_WARNED_AT, STATS_FETCH_WARN_INTERVAL, STATS_FETCH_WARN_MAX_INTERVAL,
    };

    #[test]
    fn drop_rollup_backfill_removes_first_bucket_the_tracker_already_covers() {
        // The rollup's first delta is measured against zero, so it carries all
        // pre-series history -- counting it next to the tracker's own record of
        // the same period nearly doubles the lifetime total (2026-08-06: a
        // $5,134 bucket against $5,382 of lifetime savings).
        let native = vec![
            daily("2026-08-02", 900_000, 5134.28),
            daily("2026-08-03", 500, 3.35),
        ];
        let tracker_covers_earlier = ["2026-07-15"];
        let kept = drop_rollup_backfill(
            native.clone(),
            tracker_covers_earlier.iter().copied().min(),
            |p| p.date.as_str(),
        );
        assert_eq!(
            kept.iter().map(|p| p.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-08-03"]
        );

        // Fresh install: the backfill bucket is the only record of that
        // history, so it has to stay.
        let kept = drop_rollup_backfill(native.clone(), None, |p| p.date.as_str());
        assert_eq!(kept.len(), 2);

        // Tracker starts with the series: nothing predates it, nothing to drop.
        let kept = drop_rollup_backfill(native, Some("2026-08-02"), |p| p.date.as_str());
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn settle_rollup_backfill_keeps_a_real_day_minus_the_ring_start() {
        // Fresh backend data dir: the ring's first checkpoint is near zero, so
        // the leading bucket is a genuine day, not backfill. Dropping it showed
        // a $0.50 lifetime against a $0.91 "saved today" on 2026-08-27.
        let native = vec![daily("2026-08-27", 480_748, 1.244165)];
        let ring_start = RingStartTotals {
            tokens_saved: 1_917,
            compression_savings_usd: 0.003834,
            total_input_tokens: 48_545,
            total_input_cost_usd: 0.11771,
            output_tokens_saved: 0,
            output_savings_usd: 0.0,
        };
        let settled = settle_rollup_backfill(native, Some("2026-08-20"), Some(&ring_start), |p| {
            p.date.as_str()
        });
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].estimated_tokens_saved, 478_831);
        assert!((settled[0].estimated_savings_usd - 1.240331).abs() < 1e-9);

        // A true backfill lump -- counters survived a trim or reset, so the
        // whole bucket is pre-ring history -- still empties and drops.
        let native = vec![
            daily("2026-08-02", 900_000, 5134.28),
            daily("2026-08-03", 500, 3.35),
        ];
        let lump = RingStartTotals {
            tokens_saved: 900_000,
            compression_savings_usd: 5134.28,
            ..RingStartTotals::default()
        };
        let settled =
            settle_rollup_backfill(native.clone(), Some("2026-07-15"), Some(&lump), |p| {
                p.date.as_str()
            });
        assert_eq!(
            settled.iter().map(|p| p.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-08-03"]
        );

        // No raw ring in the payload: fall back to the whole-bucket drop.
        let settled = settle_rollup_backfill(native.clone(), Some("2026-07-15"), None, |p| {
            p.date.as_str()
        });
        assert_eq!(settled.len(), 1);

        // Tracker does not predate the ring: untouched either way.
        let settled =
            settle_rollup_backfill(native, Some("2026-08-02"), Some(&lump), |p| p.date.as_str());
        assert_eq!(settled.len(), 2);
    }

    #[test]
    fn parser_extracts_ring_start_from_the_oldest_raw_checkpoint() {
        let body = r#"{
            "lifetime": { "tokens_saved": 10, "compression_savings_usd": 0.1 },
            "series": { "daily": [] },
            "history": [
                { "timestamp": "2026-08-27T11:00:00Z", "total_tokens_saved": 60,
                  "compression_savings_usd": 0.6, "total_input_tokens": 600,
                  "total_input_cost_usd": 0.06, "cache_read_tokens": 0 },
                { "timestamp": "2026-08-27T10:00:00Z", "total_tokens_saved": 50,
                  "compression_savings_usd": 0.5, "total_input_tokens": 500,
                  "total_input_cost_usd": 0.05, "cache_read_tokens": 0 }
            ]
        }"#;
        let parsed = parse_headroom_stats_history_from_json(body).expect("parsed");
        let start = parsed.ring_start.expect("ring start");
        assert_eq!(start.tokens_saved, 50);
        assert!((start.compression_savings_usd - 0.5).abs() < 1e-9);
        assert_eq!(start.total_input_tokens, 500);
        assert_eq!(start.output_tokens_saved, 0);
    }

    #[test]
    fn lifetime_card_never_reads_below_saved_today_on_a_fresh_ring() {
        // Full display-pipeline regression for 2026-08-27: fresh backend data
        // dir (ring starts near zero) + tracker history older than the ring.
        // The daily settle used to drop the whole live day, so the lifetime
        // card (daily sum) read $0.50 while "saved today" (hourly sum, which
        // only lost one hour) read $0.91.
        let ring_start = RingStartTotals {
            tokens_saved: 1_917,
            compression_savings_usd: 0.003834,
            ..RingStartTotals::default()
        };
        // The backend's rollups for the live day.
        let native_daily = vec![daily("2026-08-27", 480_748, 1.244165)];
        let mut native_hourly = vec![
            hourly("2026-08-27T10:00", 250_000),
            hourly("2026-08-27T11:00", 180_000),
            hourly("2026-08-27T12:00", 50_748),
        ];
        native_hourly[0].estimated_savings_usd = 0.53;
        native_hourly[1].estimated_savings_usd = 0.60;
        native_hourly[2].estimated_savings_usd = 0.114165;
        // The local tracker predates the ring and only saw part of today.
        let tracker_daily = vec![
            daily("2026-08-20", 134_000, 0.30),
            daily("2026-08-27", 200_000, 0.50),
        ];
        let tracker_hourly = vec![
            hourly("2026-08-20T09:00", 134_000),
            hourly("2026-08-27T11:00", 200_000),
        ];

        let settled_daily = settle_rollup_backfill(
            native_daily,
            tracker_daily.iter().map(|p| p.date.as_str()).min(),
            Some(&ring_start),
            |p| p.date.as_str(),
        );
        let settled_hourly = settle_rollup_backfill(
            native_hourly,
            tracker_hourly.iter().map(|p| p.hour.as_str()).min(),
            Some(&ring_start),
            |p| p.hour.as_str(),
        );
        let merged_daily = merge_daily_savings(tracker_daily, settled_daily, "2026-06-02");
        let merged_hourly =
            merge_hourly_savings(tracker_hourly, settled_hourly, "2026-06-02T00:00");

        // The two figures the Home screen renders.
        let lifetime: f64 = merged_daily.iter().map(|p| p.estimated_savings_usd).sum();
        let today: f64 = merged_hourly
            .iter()
            .filter(|p| p.hour.starts_with("2026-08-27"))
            .map(|p| p.estimated_savings_usd + p.output_savings_usd)
            .sum();
        assert!(
            lifetime >= today,
            "lifetime {lifetime} must cover saved-today {today}"
        );
        // Today survives at full ring value minus the ring start, on both
        // series -- not the tracker's partial $0.50 observation.
        assert!((today - 1.240331).abs() < 1e-9, "{today}");
        assert!((lifetime - (0.30 + 1.240331)).abs() < 1e-9, "{lifetime}");
    }

    #[test]
    fn lifetime_output_savings_prices_the_estimators_full_history() {
        // Buckets: 2 days, 100k tokens for $2.50 -> $25/M blended.
        let mut buckets = vec![daily("2026-08-04", 0, 0.0), daily("2026-08-05", 0, 0.0)];
        buckets[0].output_tokens_saved = 40_000;
        buckets[0].output_savings_usd = 1.0;
        buckets[1].output_tokens_saved = 60_000;
        buckets[1].output_savings_usd = 1.5;

        // The estimator covers history the rollups never carried: price all of
        // it at the buckets' own rate.
        let usd = lifetime_output_savings_usd(&buckets, Some(1_000_000));
        assert!((usd - 25.0).abs() < 1e-9, "{usd}");

        // Re-seeded / lagging estimator: never go below what we can see.
        let usd = lifetime_output_savings_usd(&buckets, Some(10_000));
        assert!((usd - 2.5).abs() < 1e-9, "{usd}");

        // No estimate at all (old backend, unseeded baseline).
        let usd = lifetime_output_savings_usd(&buckets, None);
        assert!((usd - 2.5).abs() < 1e-9, "{usd}");

        // No priced buckets yet: nothing to extrapolate a rate from.
        let empty = vec![daily("2026-08-04", 0, 0.0)];
        assert_eq!(lifetime_output_savings_usd(&empty, Some(1_000_000)), 0.0);
    }

    #[test]
    fn tool_schema_tokens_accumulate_across_backend_restarts() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("config")).expect("config dir");
        let mut tracker = SavingsTracker::load_or_create(dir.path()).expect("tracker");

        // First reading of a process is a baseline, never a delta: a backend
        // that was already running when the app attached must not have its
        // whole counter counted as new savings.
        tracker.accumulate_tool_schema_tokens(30_000);
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 0);

        tracker.accumulate_tool_schema_tokens(50_000);
        tracker.accumulate_tool_schema_tokens(90_000);
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 60_000);

        // Backend restart: the counter drops. Reseed rather than underflow or
        // re-add the whole new total.
        tracker.accumulate_tool_schema_tokens(1_000);
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 60_000);
        tracker.accumulate_tool_schema_tokens(4_000);
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 63_000);

        // The lifetime total survives a reload; the process watermark does not,
        // so the next backend's first reading is a baseline again.
        tracker.persist_state().expect("persist");
        let reloaded = SavingsTracker::load_or_create(dir.path()).expect("reload");
        assert_eq!(reloaded.lifetime_tool_schema_tokens_saved, 63_000);
        assert_eq!(reloaded.tool_schema_process_total, None);
    }

    #[test]
    fn tool_schema_savings_are_priced_at_the_cache_read_rate() {
        // 1M compression tokens for $5 -> $5/M input, so deferred tool tokens
        // are priced at $0.50/M.
        let buckets = vec![daily("2026-08-05", 1_000_000, 5.0)];
        let usd = tool_schema_savings_usd(&buckets, 2_000_000);
        assert!((usd - 1.0).abs() < 1e-9, "{usd}");

        // Nothing deferred, or no compression buckets to derive a rate from.
        assert_eq!(tool_schema_savings_usd(&buckets, 0), 0.0);
        assert_eq!(tool_schema_savings_usd(&[], 2_000_000), 0.0);
        assert_eq!(
            tool_schema_savings_usd(&[daily("2026-08-05", 0, 0.0)], 2_000_000),
            0.0
        );
    }

    #[test]
    fn savings_rate_canary_flags_a_rate_no_input_token_can_cost() {
        let with_spend = |tokens: u64, usd: f64, sent: u64, paid: f64| DailySavingsPoint {
            actual_cost_usd: paid,
            total_tokens_sent: sent,
            ..daily("2026-08-21", tokens, usd)
        };

        // Healthy: $5/M saved, Opus 5's input rate.
        assert_eq!(
            savings_rate_implausible(&[with_spend(2_000_000, 10.0, 20_000_000, 100.0)]),
            None
        );

        // The 0.36.0 fold shape: $33/M implied, past every published input price.
        let savings_per_m =
            savings_rate_implausible(&[with_spend(2_000_000, 66.0, 20_000_000, 100.0)])
                .expect("contaminated rate must trip the canary");
        assert!((savings_per_m - 33.0).abs() < 1e-6, "{savings_per_m}");

        // RUST-89/8C: a cache-heavy user on the pinned wheel. $5/M saved against
        // 20M tokens sent for $17 -- a $0.85/M paid rate, because cache reads
        // bill at a tenth and dominate the denominator. The old relative check
        // read that as 5.9x and fired; a plausible savings rate must not.
        assert_eq!(
            savings_rate_implausible(&[with_spend(2_000_000, 10.0, 20_000_000, 17.0)]),
            None
        );

        // Below the volume/spend floors nothing fires, however wild the rate:
        // a handful of cheap requests is noise, not a semantics change.
        assert_eq!(
            savings_rate_implausible(&[with_spend(500_000, 66.0, 20_000_000, 100.0)]),
            None
        );
        assert_eq!(
            savings_rate_implausible(&[with_spend(2_000_000, 0.5, 20_000_000, 100.0)]),
            None
        );
        assert_eq!(savings_rate_implausible(&[]), None);
    }

    #[test]
    fn readyz_404_counts_as_reachable_but_503_does_not() {
        use reqwest::StatusCode;
        assert!(proxy_readyz_status_is_reachable(StatusCode::OK));
        // Old proxy without the /readyz route, still serving (RUST-2X).
        assert!(proxy_readyz_status_is_reachable(StatusCode::NOT_FOUND));
        // Up but not ready / errored: keep waiting, count as unreachable.
        assert!(!proxy_readyz_status_is_reachable(
            StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(!proxy_readyz_status_is_reachable(
            StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn readyz_503_upstream_only_body_counts_as_reachable() {
        // Only the cached upstream probe is down: process alive and serving, so
        // don't flap the UI banner "crashed" on a transient network blip.
        let upstream_only = r#"{"checks":{"startup":{"ready":true},"upstream":{"ready":false}}}"#;
        assert!(proxy_readyz_503_body_is_upstream_only(upstream_only));
        // A core component down is a real readiness failure: stay unreachable.
        let core_down = r#"{"checks":{"cache":{"ready":false},"upstream":{"ready":false}}}"#;
        assert!(!proxy_readyz_503_body_is_upstream_only(core_down));
        // Bare / unparseable 503 body (body read starved under load): conservative.
        assert!(!proxy_readyz_503_body_is_upstream_only("not json"));
        // Nothing unhealthy (shouldn't be a 503, but be safe): not upstream-only.
        let all_ready = r#"{"checks":{"upstream":{"ready":true}}}"#;
        assert!(!proxy_readyz_503_body_is_upstream_only(all_ready));
        // RUST-5E: a never-loaded kompress model tagged along on every
        // sleep-wake blip and made the process look crashed. Soft check, ignore.
        let with_kompress =
            r#"{"checks":{"upstream":{"ready":false},"kompress":{"ready":false,"optional":true}}}"#;
        assert!(proxy_readyz_503_body_is_upstream_only(with_kompress));
    }

    #[test]
    fn boot_validation_stalled_within_grace_window_is_never_stalled() {
        use std::time::Duration;
        let grace = Duration::from_secs(60);
        let silence = Duration::from_secs(90);
        // Inside grace, ignore activity_age entirely.
        assert!(!boot_validation_stalled(
            Duration::from_secs(30),
            Duration::from_secs(120),
            grace,
            silence,
        ));
        // Boundary: elapsed == grace is NOT past grace (strict >).
        assert!(!boot_validation_stalled(
            Duration::from_secs(60),
            Duration::from_secs(120),
            grace,
            silence,
        ));
    }

    #[test]
    fn boot_validation_stalled_past_grace_with_recent_activity_is_not_stalled() {
        use std::time::Duration;
        let grace = Duration::from_secs(60);
        let silence = Duration::from_secs(90);
        // Past grace but log/HF moved within the silence window.
        assert!(!boot_validation_stalled(
            Duration::from_secs(120),
            Duration::from_secs(30),
            grace,
            silence,
        ));
        // Boundary: activity_age == silence is NOT past silence.
        assert!(!boot_validation_stalled(
            Duration::from_secs(120),
            Duration::from_secs(90),
            grace,
            silence,
        ));
    }

    #[test]
    fn boot_validation_stalled_past_grace_and_silence_fires() {
        use std::time::Duration;
        let grace = Duration::from_secs(60);
        let silence = Duration::from_secs(90);
        // Past grace, activity stale → stalled.
        assert!(boot_validation_stalled(
            Duration::from_secs(120),
            Duration::from_secs(91),
            grace,
            silence,
        ));
        // Reproduces the original Sentry trace shape (with old 45s
        // silence): 64.7s elapsed, ~50s of silence past mtime → stall.
        assert!(boot_validation_stalled(
            Duration::from_secs(64),
            Duration::from_secs(50),
            Duration::from_secs(60),
            Duration::from_secs(45),
        ));
        // Same trace with the new 90s silence and (no) HF growth signal:
        // would still stall, but only after another 40s. Without HF
        // growth refreshing activity_age, this is the worst-case bound.
        assert!(!boot_validation_stalled(
            Duration::from_secs(64),
            Duration::from_secs(50),
            Duration::from_secs(60),
            Duration::from_secs(90),
        ));
    }

    #[test]
    fn boot_validation_timed_out_respects_download_ceiling() {
        use std::time::Duration;
        let max = Duration::from_secs(600);
        let hard_max = Duration::from_secs(1800);
        // Idle proxy at the soft cap → timed out (current behaviour preserved).
        assert!(boot_validation_timed_out(
            Duration::from_secs(600),
            max,
            hard_max,
            false,
        ));
        // Slow first-run download still growing at the soft cap → keep waiting
        // (this is RUST-4A: don't roll back a live download).
        assert!(!boot_validation_timed_out(
            Duration::from_secs(725),
            max,
            hard_max,
            true,
        ));
        // Download that never finishes eventually hits the hard ceiling.
        assert!(boot_validation_timed_out(
            Duration::from_secs(1800),
            max,
            hard_max,
            true,
        ));
        // Below the soft cap: never timed out regardless of download state.
        assert!(!boot_validation_timed_out(
            Duration::from_secs(120),
            max,
            hard_max,
            false,
        ));
    }

    #[test]
    fn boot_validation_outcome_labels_are_stable() {
        // These labels become Sentry tags and analytics dimensions —
        // changing them silently invalidates dashboards.
        assert_eq!(BootValidationOutcome::Reachable.label(), "reachable");
        assert_eq!(
            BootValidationOutcome::ProcessExited.label(),
            "process_exited"
        );
        assert_eq!(BootValidationOutcome::Stalled.label(), "stalled");
        assert_eq!(BootValidationOutcome::TimedOut.label(), "timed_out");
        assert_eq!(BootValidationOutcome::NotStarted.label(), "not_started");
        assert_eq!(
            BootValidationOutcome::ForeignPortOccupant.label(),
            "foreign_port_occupant"
        );
        assert!(!BootValidationOutcome::ForeignPortOccupant.is_ok());
        assert!(BootValidationOutcome::Reachable.is_ok());
        assert!(!BootValidationOutcome::NotStarted.is_ok());
    }

    #[test]
    fn log_mtime_advanced_detects_first_observation_and_new_writes() {
        use std::time::{Duration, SystemTime};
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t2 = t1 + Duration::from_secs(1);

        // First time we see a log file.
        assert!(log_mtime_advanced(None, Some(t1)));
        // Newer write after a previous observation.
        assert!(log_mtime_advanced(Some(t1), Some(t2)));
        // No change.
        assert!(!log_mtime_advanced(Some(t1), Some(t1)));
        // Log "vanished" (shouldn't happen on a healthy boot, but the
        // function must not declare activity in that case).
        assert!(!log_mtime_advanced(Some(t1), None));
        // Both None — pre-first-write state, no activity.
        assert!(!log_mtime_advanced(None, None));
    }

    #[test]
    fn hf_cache_grew_returns_true_only_on_growth() {
        // First observation after the cache dir appeared. Empty dir
        // doesn't count as activity (HF created the dir but hasn't
        // started downloading yet).
        assert!(!hf_cache_grew(None, 0));
        // First observation with content — counts as growth.
        assert!(hf_cache_grew(None, 100));
        // Strictly grew.
        assert!(hf_cache_grew(Some(100), 200));
        // Unchanged.
        assert!(!hf_cache_grew(Some(100), 100));
        // Shrunk (HF cache pruning during boot — rare, but the function
        // shouldn't lie and call this growth).
        assert!(!hf_cache_grew(Some(200), 100));
    }

    #[test]
    fn parse_ps_cpu_time_handles_macos_formats() {
        // MM:SS.ss (most common — processes under an hour of CPU)
        assert_eq!(parse_ps_cpu_time("0:00.05"), Some(0));
        assert_eq!(parse_ps_cpu_time("0:42.13"), Some(42));
        assert_eq!(parse_ps_cpu_time("12:34.99"), Some(12 * 60 + 34));
        // HH:MM:SS (longer-lived processes)
        assert_eq!(parse_ps_cpu_time("1:23:45"), Some(3600 + 23 * 60 + 45));
        // D-HH:MM:SS (multi-day uptime)
        assert_eq!(
            parse_ps_cpu_time("2-01:23:45"),
            Some(2 * 86400 + 3600 + 23 * 60 + 45)
        );
        // Whitespace tolerated (ps emits a trailing newline)
        assert_eq!(parse_ps_cpu_time("  0:42.13\n"), Some(42));
        // Bad input returns None rather than panicking.
        assert_eq!(parse_ps_cpu_time(""), None);
        assert_eq!(parse_ps_cpu_time("   "), None);
        assert_eq!(parse_ps_cpu_time("not-a-time"), None);
        assert_eq!(parse_ps_cpu_time("1:2:3:4"), None);
    }

    #[test]
    fn cpu_time_advanced_detects_growth_only() {
        // Strictly grew → activity.
        assert!(cpu_time_advanced(Some(3), Some(5)));
        // First observation with non-zero CPU → activity (process was
        // already burning cycles before we started polling).
        assert!(cpu_time_advanced(None, Some(5)));
        // First observation with zero CPU → not yet doing work.
        assert!(!cpu_time_advanced(None, Some(0)));
        // Unchanged (whole-second resolution; sub-second growth is
        // dropped by the parser, so equal seconds means "no second
        // elapsed of CPU time").
        assert!(!cpu_time_advanced(Some(5), Some(5)));
        // ps stopped reporting (process gone) — not activity.
        assert!(!cpu_time_advanced(Some(5), None));
        // Both None — process never tracked or never observed.
        assert!(!cpu_time_advanced(None, None));
    }

    #[test]
    fn tcp_port_accepts_connection_true_when_listener_bound() {
        use std::net::TcpListener;
        use std::time::Duration;

        // Bind to an ephemeral port; OS picks an unused one.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = listener.local_addr().expect("local_addr");

        assert!(tcp_port_accepts_connection(addr, Duration::from_secs(1)));

        // The listener never accept()s — but the kernel still completes
        // the connect, which is the whole point: an alive-but-busy
        // proxy whose event loop is held still passes this check.
        drop(listener);
    }

    #[test]
    fn tcp_port_accepts_connection_false_when_no_listener() {
        use std::net::{SocketAddr, TcpListener};
        use std::time::Duration;

        // Bind to grab a port, then drop the listener so nothing is
        // listening on it. The OS can hand that freed port to another
        // process between drop() and connect_timeout(), so retry with
        // fresh ephemeral ports until one stays closed long enough to
        // observe. If every attempt across N tries shows accepted, the
        // function is genuinely broken.
        for _ in 0..16 {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
            let addr: SocketAddr = listener.local_addr().expect("local_addr");
            drop(listener);

            if !tcp_port_accepts_connection(addr, Duration::from_millis(200)) {
                return;
            }
        }
        panic!("tcp_port_accepts_connection returned true on 16 freshly-released ephemeral ports");
    }

    #[test]
    fn total_dir_size_bytes_returns_zero_for_missing_path() {
        let missing =
            std::env::temp_dir().join(format!("headroom-no-such-{}", uuid::Uuid::new_v4()));
        assert_eq!(total_dir_size_bytes(&missing, 1000), 0);
    }

    #[test]
    fn total_dir_size_bytes_sums_files_recursively() {
        let id = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("headroom-hf-test-{id}"));
        fs::create_dir_all(root.join("subdir/deeper")).expect("mkdir");
        fs::write(root.join("a.bin"), vec![0u8; 100]).expect("write a");
        fs::write(root.join("subdir/b.bin"), vec![0u8; 200]).expect("write b");
        fs::write(root.join("subdir/deeper/c.bin"), vec![0u8; 50]).expect("write c");

        assert_eq!(total_dir_size_bytes(&root, 1000), 350);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn total_dir_size_bytes_skips_symlinks_to_avoid_double_count() {
        // HF hub layout: snapshots/<rev>/<file> is a symlink into blobs/<sha>.
        // Counting both would overstate. We count only real files.
        let id = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("headroom-hf-symlink-test-{id}"));
        fs::create_dir_all(root.join("blobs")).expect("mkdir blobs");
        fs::create_dir_all(root.join("snapshots")).expect("mkdir snapshots");
        fs::write(root.join("blobs/file1"), vec![0u8; 500]).expect("write blob");

        let symlink_target = root.join("blobs/file1");
        let symlink_path = root.join("snapshots/file1");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&symlink_target, &symlink_path).expect("symlink");

        // 500 bytes (the blob), not 1000 (blob + symlink content).
        assert_eq!(total_dir_size_bytes(&root, 1000), 500);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn total_dir_size_bytes_respects_max_entries_cap() {
        let id = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("headroom-hf-cap-test-{id}"));
        fs::create_dir_all(&root).expect("mkdir");
        for i in 0..20 {
            fs::write(root.join(format!("f{i}")), vec![0u8; 10]).expect("write");
        }
        // With a tight cap, we may visit fewer than all 20 files. The
        // exact early-stop count depends on read_dir's iteration order;
        // assert only that we sum at most ``cap * file_size``.
        let total_capped = total_dir_size_bytes(&root, 5);
        assert!(total_capped <= 50, "got {total_capped}");
        let total_full = total_dir_size_bytes(&root, 1000);
        assert_eq!(total_full, 200);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_startup_error_port_timeout() {
        let raw = "unable to keep headroom running in background (prior attempts: \
            /Users/x/venv/bin/headroom proxy --port 6768 never opened port 6768 within 60000ms): \
            /Users/x/venv/bin/python3 -m headroom.proxy.server --port 6768 --no-http2 never opened port 6768 within 60000ms";
        let hint = classify_startup_error(raw).expect("timeout should classify");
        assert!(hint.contains("Gatekeeper"), "got: {hint}");
        assert!(hint.contains("Retry"));
    }

    #[test]
    fn classify_startup_error_python_crash() {
        let raw = "unable to keep headroom running in background (prior attempts: \
            /home/h/venv/bin/headroom proxy --port 6768 exited with status exit status: 1 before opening port 6768): \
            /home/h/venv/bin/python3 -m headroom.proxy.server --port 6768 --no-http2 exited with status exit status: 1 before opening port 6768";
        let hint = classify_startup_error(raw).expect("crash should classify");
        assert!(hint.contains("crashed at startup"), "got: {hint}");
        assert!(hint.contains("logs"));
    }

    #[test]
    fn classify_startup_error_missing_headroom_module() {
        // RUST-3Y: corrupted/incomplete install -- a headroom.* module is gone.
        // The full err chain carries the proxy log tail with the traceback.
        let raw = "unable to keep headroom running in background: \
            /h/venv/bin/python3 -m headroom.proxy.server --port 6768 exited with status exit status: 1 \
            before opening port 6768\n--- log tail ---\nTraceback (most recent call last):\n  \
            File registry.py, line 11\n    from headroom.providers.claude import DEFAULT_API_URL\n\
            ModuleNotFoundError: No module named 'headroom.providers.claude'\n--- end log ---";
        let hint = classify_startup_error(raw).expect("missing module should classify");
        assert!(
            hint.contains("missing some of its own files"),
            "got: {hint}"
        );
        assert!(hint.contains("Reinstall"));
        // Must win over the generic crash branch (which also matches this raw).
        assert!(!hint.contains("crashed at startup"), "got: {hint}");
    }

    #[test]
    fn classify_startup_error_missing_stdlib_encodings() {
        // RUST-C8 verbatim shape: getpath fell back to the cwd as prefix, then
        // core init died before any Python frame existed.
        let raw = "unable to keep headroom running in background: \
            C:\\U\\venv\\Scripts\\python.exe -m headroom.proxy.server --port 6768 exited with \
            status exit code: 1 before opening port 6768\n--- log tail ---\n\
            Could not find platform independent libraries <prefix>\n\
            Fatal Python error: init_fs_encoding: failed to get the Python codec of the \
            filesystem encoding\nPython runtime state: core initialized\n\
            ModuleNotFoundError: No module named 'encodings'\n--- end log ---";
        let hint = classify_startup_error(raw).expect("missing stdlib should classify");
        assert!(
            hint.contains("missing some of its own files"),
            "got: {hint}"
        );
        assert!(!hint.contains("crashed at startup"), "got: {hint}");
    }

    /// RUST-8V/8W: the runtime prints its full banner and then dies before
    /// binding, because its native deps cannot load without the MSVC
    /// redistributable. The log names the cause; the hint must too, instead of
    /// sending the user to read it.
    #[test]
    fn classify_startup_error_missing_msvc_redistributable() {
        let raw = "unable to keep headroom running in background: \
            ~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\headroom.exe proxy --port 6768 \
            exited with status exit code: 0xffffffff before opening port 6768\n--- log tail ---\n\
            Press Ctrl+C to stop.\n\nMicrosoft Visual C++ Redistributable is not installed, \
            this may lead to the DLL load failure.\n\
            It can be downloaded at https://aka.ms/vs/17/release/vc_redist.x64.exe\n";
        let hint = classify_startup_error(raw).expect("missing redist should classify");
        assert!(hint.contains("Visual C++ Redistributable"), "got: {hint}");
        assert!(hint.contains("vc_redist.x64.exe"), "got: {hint}");
        // Must win over the generic exited-before-port branch, which also matches.
        assert!(!hint.contains("crashed at startup"), "got: {hint}");
    }

    #[test]
    fn classify_startup_error_foreign_port() {
        let raw =
            "port 6768 is occupied by a non-headroom process (pid 1234 node); cannot start proxy.";
        let hint = classify_startup_error(raw).expect("foreign port should classify");
        assert!(hint.contains("Reboot"), "got: {hint}");
    }

    /// Regression (Sentry RUST-7D, RUST-7B, RUST-64): WSAEADDRINUSE on 6767
    /// leaves the app dead to every client, so the banner must name the port
    /// rather than blaming the Python runtime -- which in that state is running
    /// fine on a port of its own. The hint must hand over the command that
    /// identifies the holder instead of asserting which one it is.
    #[test]
    fn intercept_bind_hint_names_the_port_and_how_to_find_the_holder() {
        let hint = intercept_bind_hint(
            "Only one usage of each socket address (protocol/network address/port) is \
             normally permitted. (os error 10048)",
        );
        assert!(hint.contains("6767"), "{hint}");
        assert!(hint.contains("Get-NetTCPConnection"), "{hint}");
        // Must not assert a single cause: winnat is not running on every
        // affected machine, and `net stop winnat` fails outright there.
        assert!(!hint.contains("net stop winnat"), "{hint}");
    }

    #[test]
    fn intercept_bind_hint_falls_back_to_the_raw_cause() {
        let hint = intercept_bind_hint("Address already in use (os error 48)");
        assert!(hint.contains("os error 48"), "{hint}");
        assert!(!hint.contains("Get-NetTCPConnection"), "{hint}");
    }

    #[test]
    fn classify_startup_error_foreign_port_with_fallback_exhausted() {
        let raw =
            "port 6768 is occupied by a non-headroom process (rapportd pid 594) and fallback ports 6769-6790 are also unavailable; cannot start proxy. Reboot to clear stuck listeners, then relaunch Headroom.";
        let hint = classify_startup_error(raw).expect("all-foreign should classify");
        assert!(hint.contains("Reboot"), "got: {hint}");
    }

    #[test]
    fn classify_startup_error_endpoint_protection_signal_kill() {
        let raw = "unable to keep headroom running in background (prior attempts: \
                   /Users/x/venv/bin/headroom proxy --port 6768 exited with signal=9): \
                   /Users/x/venv/bin/python3 -m headroom.proxy.server exited with signal=9";
        let hint = classify_startup_error(raw).expect("SIGKILL should classify");
        assert!(
            hint.contains("endpoint protection"),
            "expected EDR hint, got: {hint}"
        );
        assert!(hint.contains("Retry"), "hint should be actionable: {hint}");
    }

    #[test]
    fn classify_startup_error_endpoint_protection_dlopen_blocked() {
        let raw = "ImportError: dlopen(/Users/x/Library/Application Support/Headroom/headroom/runtime/venv/\
                   lib/python3.12/site-packages/torch/lib/libtorch.dylib, 0x0006): tried: '...' \
                   (operation not permitted)";
        let hint = classify_startup_error(raw).expect("dlopen-blocked should classify");
        assert!(
            hint.contains("endpoint protection"),
            "expected EDR hint, got: {hint}"
        );
    }

    #[test]
    fn classify_startup_error_recognises_a_blocked_stdlib_dll_on_windows() {
        // RUST-BB verbatim shape: the proxy dies importing `_sqlite3` because
        // Application Control blocked the DLL. No numeric code, localized
        // prose, and the chain ALSO matches the generic "exited ... before
        // opening port" branch -- which would have told the user to read a
        // traceback instead of naming the policy.
        let raw = "unable to keep headroom running in background: python.exe -m \
                   headroom.proxy.server --port 6768 exited with status exit code: 1 \
                   before opening port 6768\n--- log tail ---\n    from _sqlite3 import *\n\
                   ImportError: DLL load failed while importing _sqlite3: Una directiva de \
                   Control de aplicaciones bloqueó este archivo.\n--- end log ---";
        let hint = classify_startup_error(raw).expect("blocked DLL should classify");
        assert!(
            hint.contains("endpoint protection"),
            "expected the endpoint-protection hint, got: {hint}"
        );
        assert!(
            !hint.contains("see the traceback"),
            "generic branch won: {hint}"
        );
    }

    /// RUST-CY verbatim shape: asyncio's socketpair fallback refused a
    /// loopback listen on a Russian-locale Windows 11 host, exit 1 with the
    /// banner printed and the port never opened, identically on 0.37.0 and
    /// the 0.35.0 rollback. Only the code survives localization.
    #[test]
    fn classify_startup_error_recognises_a_denied_loopback_socket_on_windows() {
        let raw = "unable to keep headroom running in background (prior attempts: headroom.exe: \
                   exited with status exit code: 1 before opening port 6768): exited with status \
                   exit code: 1 before opening port 6768 (~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\python.exe \
                   -m headroom.proxy.server --port 6768 --no-http2 --log-messages; log: ...)\n\
                   --- log tail ---\n  File \"...\\Lib\\socket.py\", line 616, in _fallback_socketpair\n    \
                   lsock.listen()\nPermissionError: [WinError 10013] Сделана попытка доступа к сокету \
                   методом, запрещенным правами доступа\n--- end log ---";
        let hint = classify_startup_error(raw).expect("denied loopback socket should classify");
        assert!(hint.contains("10013"), "hint should name the code: {hint}");
        assert!(
            hint.contains("excludedportrange"),
            "hint should hand over the reserved-range check: {hint}"
        );
        assert!(
            !hint.contains("see the traceback"),
            "generic branch won: {hint}"
        );
        assert!(
            !hint.contains("endpoint protection"),
            "must not assert a cause the code does not prove: {hint}"
        );
    }

    #[test]
    fn boot_validation_error_hint_explains_a_fallback_that_did_not_start_either() {
        use crate::tool_manager::RuntimeMaintenanceKind as Kind;
        let hint = super::boot_validation_error_hint(
            Kind::Upgrade,
            true,
            false,
            "0.35.0",
            Some("Windows refused a socket. Then click Retry."),
        )
        .expect("restored fallback always has a hint");
        assert_eq!(
            hint,
            "Reverted to headroom-ai 0.35.0, but it didn't start either. \
             Windows refused a socket. Then click Retry."
        );
        // Restarted fine: the startup hint is never passed, wording unchanged.
        assert_eq!(
            super::boot_validation_error_hint(Kind::Upgrade, true, true, "0.35.0", None).as_deref(),
            Some("Reverted to headroom-ai 0.35.0 and restarted it.")
        );
        // Restored but unclassified: the pre-existing wording.
        assert_eq!(
            super::boot_validation_error_hint(Kind::Upgrade, true, false, "0.35.0", None)
                .as_deref(),
            Some("Reverted to headroom-ai 0.35.0.")
        );
        // Nothing restored: the classification is all there is to say.
        assert_eq!(
            super::boot_validation_error_hint(Kind::Upgrade, false, false, "0.35.0", Some("x"))
                .as_deref(),
            Some("x")
        );
        assert_eq!(
            super::boot_validation_error_hint(Kind::Upgrade, false, false, "0.35.0", None),
            None
        );
        assert_eq!(
            super::boot_validation_error_hint(
                Kind::RequirementsRepair,
                false,
                true,
                "0.35.0",
                None
            )
            .as_deref(),
            Some("Headroom restarted with the repaired runtime, but validation still failed.")
        );
        assert_eq!(
            super::boot_validation_error_hint(
                Kind::RequirementsRepair,
                false,
                false,
                "0.35.0",
                Some("x")
            )
            .as_deref(),
            Some("x")
        );
    }

    #[test]
    fn classify_startup_error_endpoint_protection_takes_priority_over_port_path() {
        // SIGKILL while waiting on the port could surface as both a
        // port-timeout AND a kill signature. EDR wins because it points to
        // the actual root cause; otherwise the user spends time on a
        // network/firewall red herring.
        let raw = "unable to keep headroom running in background (prior attempts: \
                   /venv/bin/headroom proxy --port 6768 never opened port 6768 within 60000ms: \
                   Killed: 9)";
        let hint = classify_startup_error(raw).expect("should classify");
        assert!(
            hint.contains("endpoint protection"),
            "expected EDR to win over port hint, got: {hint}"
        );
    }

    /// Defensive: classify_startup_error must NOT regress on any of the
    /// bail strings that tool_manager actually produces. If the message
    /// shape drifts (e.g. someone tweaks the bail wording), this test
    /// fails and forces the classifier to be updated alongside.
    #[test]
    fn classify_startup_error_handles_every_tool_manager_bail_format() {
        // 1. all-foreign exhaustion
        let raw = "port 6768 is occupied by a non-headroom process (rapportd pid 594) and fallback ports 6769-6790 are also unavailable; cannot start proxy. \
                   Reboot to clear stuck listeners, then relaunch Headroom.";
        assert!(
            classify_startup_error(raw).is_some(),
            "all-foreign bail must classify"
        );

        // 2. stale headroom proxy
        let raw = "headroom proxy already running on port 6768 (likely a stale process from a prior session). \
                   Run `lsof -iTCP:6768 -sTCP:LISTEN` to find and kill it, then retry.";
        assert!(
            classify_startup_error(raw).is_some(),
            "stale proxy bail must classify"
        );

        // 3. spawn timeout (port never opened) — phrased generically over
        //    whatever port the proxy ended up on, so test with a fallback port.
        let raw = "never opened port 6770 within 60000ms";
        assert!(
            classify_startup_error(raw).is_some(),
            "spawn timeout must classify on any port"
        );

        // 4. python crash
        let raw = "exited with status 1 before opening port 6770";
        assert!(
            classify_startup_error(raw).is_some(),
            "python crash must classify on any port"
        );
    }

    #[test]
    fn classify_startup_error_stale_headroom() {
        let raw = "headroom proxy already running on port 6768 (likely a stale process from a prior session).";
        let hint = classify_startup_error(raw).expect("stale should classify");
        assert!(hint.contains("relaunch"), "got: {hint}");
    }

    #[test]
    fn classify_startup_error_unknown_returns_none() {
        assert!(classify_startup_error("some other error").is_none());
    }

    #[test]
    fn launch_profile_missing_new_fields_deserialize_as_none() {
        // Legacy profile JSON from before we added last_launched_app_version
        // and last_runtime_upgrade_failure. Must still parse.
        let legacy = br#"{
            "launch_count": 3,
            "launch_experience": "resume",
            "lifetime_requests": 0,
            "lifetime_estimated_savings_usd": 0.0,
            "lifetime_estimated_tokens_saved": 0
        }"#;
        let profile: super::LaunchProfile =
            serde_json::from_slice(legacy).expect("legacy profile parses");
        assert!(profile.last_launched_app_version.is_none());
        assert!(profile.last_runtime_upgrade_failure.is_none());
        assert!(!profile.setup_wizard_complete);
        // Legacy profiles predate terms gating: default to 0 so the gate
        // re-prompts once REQUIRED_TERMS_VERSION > 0.
        assert_eq!(profile.accepted_terms_version, 0);
    }

    #[test]
    fn setup_wizard_satisfied_requires_completion_or_legacy_configured_client() {
        let mut profile = super::LaunchProfile {
            launch_count: 2,
            launch_experience: crate::models::LaunchExperience::Resume,
            lifetime_requests: 0,
            lifetime_estimated_savings_usd: 0.0,
            lifetime_estimated_tokens_saved: 0,
            setup_wizard_complete: false,
            last_launched_app_version: None,
            last_runtime_upgrade_failure: None,
            accepted_terms_version: 0,
            upstream_override: super::UpstreamOverride::default(),
            onboarding_recovery_notified: false,
            first_savings_notified: false,
            unrouted_usage_notified: false,
        };

        assert!(!super::setup_wizard_satisfied_for_profile(&profile, false));
        assert!(super::setup_wizard_satisfied_for_profile(&profile, true));

        profile.launch_count = 1;
        assert!(!super::setup_wizard_satisfied_for_profile(&profile, true));

        profile.setup_wizard_complete = true;
        assert!(super::setup_wizard_satisfied_for_profile(&profile, false));
    }

    /// The deadlock this exists for: a wedged 6767 intercept over a healthy
    /// 6768 backend used to fall through to the spawn path, where reclaim
    /// refuses to kill a healthy occupant and bails, every launch, until three
    /// failures auto-paused (and therefore BYPASSED) the runtime. Sentry
    /// RUST-6J -> RUST-5C, the largest Windows cluster.
    #[test]
    fn runtime_already_serving_accepts_a_healthy_backend_behind_a_wedged_intercept() {
        use super::runtime_already_serving as serving;

        // The pre-existing rule is untouched: a reachable intercept is enough.
        assert!(serving(true, false, false));
        assert!(serving(true, true, true));

        // The fix: intercept down, backend healthy and running this build.
        assert!(serving(false, true, true));

        // A healthy backend from an OLDER build must NOT be adopted. Doing so
        // would silently run a mismatched wheel and disable the exact-pin
        // prefix-floor vendor, so this has to fall through to respawn.
        assert!(!serving(false, true, false));

        // Nothing serving at all still spawns.
        assert!(!serving(false, false, false));
        assert!(!serving(false, false, true));
    }

    #[test]
    fn onboarding_recovery_nudge_due_requires_return_launch_and_fires_once() {
        let mut profile = super::LaunchProfile {
            launch_count: 2,
            launch_experience: crate::models::LaunchExperience::Resume,
            lifetime_requests: 0,
            lifetime_estimated_savings_usd: 0.0,
            lifetime_estimated_tokens_saved: 0,
            setup_wizard_complete: true,
            last_launched_app_version: None,
            last_runtime_upgrade_failure: None,
            accepted_terms_version: 0,
            upstream_override: super::UpstreamOverride::default(),
            onboarding_recovery_notified: false,
            first_savings_notified: false,
            unrouted_usage_notified: false,
        };
        assert!(super::onboarding_recovery_nudge_due(&profile));

        // Install session itself never nudges.
        profile.launch_count = 1;
        assert!(!super::onboarding_recovery_nudge_due(&profile));
        // The evidence-based sibling has no return-launch gate: sessions
        // growing during the install-day run with zero traffic is exactly the
        // moment it exists for.
        assert!(super::unrouted_usage_nudge_due(&profile));
        profile.launch_count = 2;

        // Unfinished wizard never nudges, either variant.
        profile.setup_wizard_complete = false;
        assert!(!super::onboarding_recovery_nudge_due(&profile));
        assert!(!super::unrouted_usage_nudge_due(&profile));
        profile.setup_wizard_complete = true;

        // Once fired, never again.
        profile.onboarding_recovery_notified = true;
        assert!(!super::onboarding_recovery_nudge_due(&profile));
        profile.unrouted_usage_notified = true;
        assert!(!super::unrouted_usage_nudge_due(&profile));
    }

    /// What a user can type into the upstream field. The trailing-slash strip
    /// is not cosmetic: the reconciler's loop guard compares a stripped url
    /// against the configured one, so "https://host/" would make every tick
    /// see a mismatch and rewrite settings.json.
    #[test]
    fn upstream_base_urls_are_normalized_or_rejected_with_a_reason() {
        use super::normalize_upstream_base_url as norm;

        assert_eq!(
            norm("https://api.z.ai/api/anthropic").unwrap(),
            "https://api.z.ai/api/anthropic"
        );
        assert_eq!(
            norm("  https://api.z.ai/api/anthropic/  ").unwrap(),
            "https://api.z.ai/api/anthropic"
        );
        // Local endpoints are a legitimate upstream (another proxy, a mock).
        assert_eq!(
            norm("http://127.0.0.1:8000").unwrap(),
            "http://127.0.0.1:8000"
        );

        // Every rejection has to say what to fix: this text is the field error.
        for bad in [
            "",
            "   ",
            "api.z.ai",
            "ftp://api.z.ai",
            "https://",
            "http:// api.z.ai",
        ] {
            let err = norm(bad).unwrap_err();
            assert!(!err.is_empty(), "{bad:?} must be rejected with a reason");
        }
    }

    #[test]
    fn persist_launch_profile_round_trips_new_fields() {
        let id = uuid::Uuid::new_v4();
        let path = std::env::temp_dir().join(format!("headroom-launch-profile-test-{}.json", id));
        let profile = super::LaunchProfile {
            launch_count: 1,
            launch_experience: crate::models::LaunchExperience::Resume,
            lifetime_requests: 0,
            lifetime_estimated_savings_usd: 0.0,
            lifetime_estimated_tokens_saved: 0,
            setup_wizard_complete: true,
            last_launched_app_version: Some("0.2.50".into()),
            last_runtime_upgrade_failure: Some(crate::models::RuntimeUpgradeFailure {
                app_version: "0.2.50".into(),
                target_headroom_version: "0.8.2".into(),
                fallback_headroom_version: Some("0.6.5".into()),
                failure_phase: crate::models::UpgradeFailurePhase::BootValidation,
                attempts: 2,
                first_attempt_at: Utc::now(),
                last_attempt_at: Utc::now(),
                error_message: "timed out".into(),
                error_hint: Some("Reverted to 0.6.5".into()),
                rollback_restored: true,
            }),
            accepted_terms_version: 3,
            upstream_override: super::UpstreamOverride {
                mode: super::UpstreamOverrideMode::Override,
                base_url: "https://api.z.ai/api/anthropic".into(),
                has_token: true,
                ..Default::default()
            },
            onboarding_recovery_notified: true,
            first_savings_notified: true,
            unrouted_usage_notified: true,
        };
        super::persist_launch_profile(&path, &profile);

        let bytes = std::fs::read(&path).expect("persisted");
        let round_tripped: super::LaunchProfile =
            serde_json::from_slice(&bytes).expect("re-parses");
        assert_eq!(
            round_tripped.last_launched_app_version.as_deref(),
            Some("0.2.50")
        );
        let failure = round_tripped
            .last_runtime_upgrade_failure
            .expect("failure present");
        assert_eq!(failure.attempts, 2);
        assert_eq!(failure.target_headroom_version, "0.8.2");
        assert_eq!(
            failure.failure_phase,
            crate::models::UpgradeFailurePhase::BootValidation
        );
        assert_eq!(round_tripped.accepted_terms_version, 3);
        assert!(round_tripped.onboarding_recovery_notified);
        assert!(round_tripped.first_savings_notified);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rebuild_savings_from_records_sums_deltas_per_bucket() {
        let id = uuid::Uuid::new_v4();
        let records_path = std::env::temp_dir().join(format!("headroom-rebuild-test-{id}.jsonl"));
        let mk = |day: &str, hour: &str, tokens: u64, requests: usize| {
            serde_json::to_string(&SavingsRecord {
                schema_version: 7,
                id: "r".into(),
                observed_at: Utc::now(),
                day_key: day.into(),
                hour_key: hour.into(),
                delta_requests: requests,
                delta_estimated_savings_usd: 0.5,
                delta_estimated_tokens_saved: tokens,
                delta_actual_cost_usd: 0.1,
                delta_total_tokens_sent: tokens * 10,
                ..Default::default()
            })
            .unwrap()
        };
        let lines = [
            mk("2026-06-10", "2026-06-10T09:00", 100, 2),
            mk("2026-06-10", "2026-06-10T10:00", 50, 1),
            mk("2026-06-11", "2026-06-11T09:00", 25, 1),
            "not json at all".to_string(), // torn tail line is tolerated
        ];
        std::fs::write(&records_path, lines.join("\n")).unwrap();

        let rebuilt =
            rebuild_persisted_savings_from_records(&records_path).expect("rebuild from records");
        assert_eq!(rebuilt.lifetime_requests, 4);
        assert_eq!(
            rebuilt.daily_savings["2026-06-10"].estimated_tokens_saved,
            150
        );
        assert_eq!(
            rebuilt.daily_savings["2026-06-11"].estimated_tokens_saved,
            25
        );
        assert_eq!(
            rebuilt.hourly_savings["2026-06-10T09:00"].estimated_tokens_saved,
            100
        );
        // Milestones seeded from the rebuilt total so they don't re-fire.
        assert_eq!(rebuilt.lifetime_token_milestone_high_water, Some(175));

        let _ = std::fs::remove_file(&records_path);
    }

    #[test]
    fn rebuild_savings_from_records_returns_none_without_usable_records() {
        let id = uuid::Uuid::new_v4();
        let records_path = std::env::temp_dir().join(format!("headroom-rebuild-none-{id}.jsonl"));
        assert!(rebuild_persisted_savings_from_records(&records_path).is_none());
        std::fs::write(&records_path, "garbage\n").unwrap();
        assert!(rebuild_persisted_savings_from_records(&records_path).is_none());
        let _ = std::fs::remove_file(&records_path);
    }

    #[test]
    fn bucket_tool_schema_usd_prices_at_the_cache_read_rate() {
        // A bucket that saved $1.00 over 1M compression tokens implies
        // $1/M. Deferral is priced at a TENTH of that, because those schema
        // tokens would have been cache reads after the first request --
        // pricing them at full input rate is the 0.36.0 contamination.
        let priced = super::bucket_tool_schema_usd(1.0, 1_000_000, 1_000_000);
        assert!(
            (priced - 0.10).abs() < 1e-9,
            "expected a tenth of the blended rate, got {priced}"
        );

        // Ten times the deferral is ten times the dollars.
        let more = super::bucket_tool_schema_usd(1.0, 1_000_000, 10_000_000);
        assert!((more - 1.0).abs() < 1e-9, "got {more}");

        // Nothing to blend from -> zero, never a guessed price. A made-up
        // figure on an empty bucket is indistinguishable from real savings.
        assert_eq!(super::bucket_tool_schema_usd(0.0, 1_000_000, 5_000), 0.0);
        assert_eq!(super::bucket_tool_schema_usd(1.0, 0, 5_000), 0.0);
        assert_eq!(super::bucket_tool_schema_usd(1.0, 1_000_000, 0), 0.0);
    }

    #[test]
    fn tool_schema_samples_bucket_deltas_and_survive_a_backend_restart() {
        let mut tracker = make_tracker();
        let day_key = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // First reading of a process is a baseline: it must not bill the whole
        // cumulative counter to the current bucket.
        tracker.accumulate_tool_schema_tokens(10_000);
        assert!(tracker.tool_schema_daily_samples.is_empty());
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 0);

        tracker.accumulate_tool_schema_tokens(10_500);
        assert_eq!(tracker.tool_schema_daily_samples[&day_key], 500);
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 500);

        // Deltas accumulate into the same bucket.
        tracker.accumulate_tool_schema_tokens(11_000);
        assert_eq!(tracker.tool_schema_daily_samples[&day_key], 1_000);

        // A backend restart resets the proxy's counter to a small value. The
        // decrease must never emit a delta (that would bill the whole new
        // process total again); it re-anchors, losing at most one poll, and
        // the NEXT delta must be measured against the new anchor.
        tracker.accumulate_tool_schema_tokens(200);
        assert_eq!(
            tracker.tool_schema_daily_samples[&day_key], 1_000,
            "a counter reset must not add a phantom delta"
        );
        tracker.accumulate_tool_schema_tokens(700);
        assert_eq!(
            tracker.tool_schema_daily_samples[&day_key], 1_500,
            "after re-anchoring, deltas resume from the new process total"
        );
        assert_eq!(tracker.lifetime_tool_schema_tokens_saved, 1_500);

        // The hourly bucket carries the same total under a local-hour key.
        let hour_key = super::local_hour_key(chrono::Utc::now().with_timezone(&chrono::Local));
        assert_eq!(tracker.tool_schema_hourly_samples[&hour_key], 1_500);

        // Round-trips through the persisted state.
        let reloaded = tracker.persisted_state();
        assert_eq!(reloaded.tool_schema_daily_samples[&day_key], 1_500);
        assert_eq!(reloaded.tool_schema_hourly_samples[&hour_key], 1_500);
    }

    #[test]
    fn sample_output_reduction_buckets_deltas_and_reseeds_on_reset() {
        let mut tracker = make_tracker();
        // First reading seeds the watermark, never a delta.
        tracker.sample_output_reduction(Some((1_000, 3_000)));
        assert!(tracker.output_daily_samples.is_empty());
        tracker.sample_output_reduction(Some((1_400, 4_000)));
        let day_key = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let day = tracker
            .output_daily_samples
            .get(&day_key)
            .copied()
            .expect("day bucket");
        assert_eq!(day.saved_tokens, 400);
        assert_eq!(day.baseline_tokens, 1_000);
        // Every reading refreshes the cached estimator total, including a
        // below-watermark one — the cold-start fallback must show what the
        // live path would.
        assert_eq!(tracker.last_output_estimator_tokens_saved, Some(1_400));
        // A reading below the watermark (estimator rebuilt) reseeds silently.
        tracker.sample_output_reduction(Some((100, 200)));
        tracker.sample_output_reduction(Some((150, 300)));
        assert_eq!(tracker.last_output_estimator_tokens_saved, Some(150));
        let day = tracker
            .output_daily_samples
            .get(&day_key)
            .copied()
            .expect("day bucket");
        assert_eq!(day.saved_tokens, 450);
        assert_eq!(day.baseline_tokens, 1_100);
        let hourly_total: u64 = tracker
            .output_hourly_samples
            .values()
            .map(|bucket| bucket.saved_tokens)
            .sum();
        assert_eq!(hourly_total, 450);
    }

    #[test]
    fn unscoreable_ledger_purges_the_fallback_recorded_output_series() {
        let mut tracker = make_tracker();
        // Buckets and marks as the retired backend-figure fallback left them
        // on an rc.7 machine.
        tracker.sample_output_reduction(Some((1_000, 3_000)));
        tracker.sample_output_reduction(Some((1_400, 4_000)));
        assert!(!tracker.output_daily_samples.is_empty());
        assert!(!tracker.output_hourly_samples.is_empty());

        tracker.drop_unscoreable_output_samples();
        assert!(tracker.output_daily_samples.is_empty());
        assert!(tracker.output_hourly_samples.is_empty());
        assert_eq!(tracker.last_output_estimator_tokens_saved, None);
        assert_eq!(tracker.last_output_estimator_baseline_tokens, None);
        assert_eq!(tracker.output_sample_watermark, None);
        // The purge reaches the persisted state, not just this launch.
        assert!(tracker.persisted_state().output_daily_samples.is_empty());

        // Once the control arm makes the machine scoreable, sampling reseeds
        // from the (smaller) local cumulative without a phantom delta: the
        // first reading seeds, only the second emits.
        tracker.sample_output_reduction(Some((100, 200)));
        assert!(tracker.output_daily_samples.is_empty());
        tracker.sample_output_reduction(Some((160, 300)));
        let day_key = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let day = tracker
            .output_daily_samples
            .get(&day_key)
            .copied()
            .expect("day bucket");
        assert_eq!(day.saved_tokens, 60);
        assert_eq!(day.baseline_tokens, 100);
    }

    #[test]
    fn shallow_estimator_dip_is_not_rebilled_when_it_climbs_back() {
        // A backend that restarts onto a lagging durable checkpoint reports
        // below where it left off and then re-earns ground already banked.
        // Rebasing to the dip would bill that catch-up a second time.
        let mut tracker = make_tracker();
        let day_key = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let saved_today = |t: &SavingsTracker| {
            t.output_daily_samples
                .get(&day_key)
                .map_or(0, |bucket| bucket.saved_tokens)
        };

        tracker.sample_output_reduction(Some((1_000, 3_000)));
        tracker.sample_output_reduction(Some((1_400, 4_000)));
        assert_eq!(saved_today(&tracker), 400);

        // Shallow dip, then the whole climb back to where it was.
        tracker.sample_output_reduction(Some((1_200, 3_500)));
        tracker.sample_output_reduction(Some((1_400, 4_000)));
        assert_eq!(
            saved_today(&tracker),
            400,
            "re-earned ground must be free, not counted twice"
        );

        // Genuine progress past the old mark still counts.
        tracker.sample_output_reduction(Some((1_500, 4_200)));
        assert_eq!(saved_today(&tracker), 500);
    }

    #[test]
    fn launch_seeds_from_the_persisted_reading_not_a_regressed_one() {
        // The 2026-08-17 bug: the app relaunched, the backend restarted 15s
        // later onto a stale checkpoint, and the first poll seeded on that
        // regressed value -- billing the entire catch-up (906k tokens, ~2.8x)
        // to the launch-day bucket.
        let mut tracker = make_tracker();
        tracker.last_output_estimator_tokens_saved = Some(27_000);
        tracker.last_output_estimator_baseline_tokens = Some(65_000);
        assert!(tracker.output_sample_watermark.is_none(), "fresh launch");

        tracker.sample_output_reduction(Some((26_000, 63_000)));
        assert_eq!(
            tracker.output_sample_watermark,
            Some((27_000, 65_000)),
            "seed at the high-water of live and persisted, not the dip"
        );

        tracker.sample_output_reduction(Some((27_100, 65_200)));
        let day_key = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let day = tracker
            .output_daily_samples
            .get(&day_key)
            .copied()
            .expect("day bucket");
        assert_eq!(
            day.saved_tokens, 100,
            "only progress past the persisted mark"
        );
        assert_eq!(day.baseline_tokens, 200);
    }

    #[test]
    fn cached_output_estimator_total_survives_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("config")).expect("config dir");
        let mut tracker = SavingsTracker::load_or_create(dir.path()).expect("tracker");
        assert_eq!(tracker.last_output_estimator_tokens_saved, None);
        tracker.last_output_estimator_tokens_saved = Some(22_000_000);
        tracker.persist_state().expect("persist");
        let reloaded = SavingsTracker::load_or_create(dir.path()).expect("reload");
        assert_eq!(
            reloaded.last_output_estimator_tokens_saved,
            Some(22_000_000)
        );
    }

    #[test]
    fn old_units_sampled_output_series_keeps_history_and_reseeds_the_watermark() {
        // A file written before OUTPUT_SAMPLE_SERIES_VERSION existed carries
        // sampled buckets recorded under the old estimator plus a watermark
        // pair far above the new cumulative. The watermark must go (it would
        // silence the sampler for weeks); the buckets must NOT -- dropping
        // them erased two weeks of a real user's output history.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("config")).expect("config dir");
        let mut old = PersistedSavingsState {
            schema_version: 3,
            lifetime_requests: 12,
            last_output_estimator_tokens_saved: Some(11_644_822),
            last_output_estimator_baseline_tokens: Some(36_147_591),
            output_sample_series_version: 0,
            ..Default::default()
        };
        old.output_daily_samples.insert(
            "2026-08-21".into(),
            OutputSampleBucket {
                saved_tokens: 6_360,
                baseline_tokens: 6_444,
            },
        );
        old.output_hourly_samples.insert(
            "2026-08-21T09:00".into(),
            OutputSampleBucket {
                saved_tokens: 1_586,
                baseline_tokens: 1_611,
            },
        );
        // The seam: a part-day of old-units samples that new-units samples
        // would be added to. Kept, it reads as the day's figure (93% of pure
        // ping artifacts, 2026-08-22) long after everything else has moved on.
        let seam_day_utc = Utc::now().format("%Y-%m-%d").to_string();
        let seam_hour_local = super::local_hour_key(Local::now());
        old.output_daily_samples.insert(
            seam_day_utc.clone(),
            OutputSampleBucket {
                saved_tokens: 17_995,
                baseline_tokens: 19_332,
            },
        );
        old.output_hourly_samples.insert(
            seam_hour_local.clone(),
            OutputSampleBucket {
                saved_tokens: 1_590,
                baseline_tokens: 1_611,
            },
        );
        std::fs::write(
            config_file(dir.path(), "savings-state.json"),
            serde_json::to_vec(&old).expect("serialize"),
        )
        .expect("write old state");

        let tracker = SavingsTracker::load_or_create(dir.path()).expect("load");
        assert!(
            !tracker.output_daily_samples.contains_key(&seam_day_utc),
            "the mixed-units seam bucket is dropped"
        );
        assert!(!tracker.output_hourly_samples.contains_key(&seam_hour_local));
        assert_eq!(
            tracker.output_daily_samples.get("2026-08-21"),
            Some(&OutputSampleBucket {
                saved_tokens: 6_360,
                baseline_tokens: 6_444,
            }),
            "recorded history survives an estimator-semantics bump"
        );
        assert_eq!(tracker.output_hourly_samples.len(), 1);
        assert_eq!(tracker.last_output_estimator_tokens_saved, None);
        assert_eq!(tracker.last_output_estimator_baseline_tokens, None);
        assert_eq!(tracker.lifetime_requests, 12, "unrelated fields survive");

        // The initial persist in load_or_create stamps the current version, so
        // the reseed happens exactly once and later buckets accumulate on top.
        let mut tracker = tracker;
        tracker.output_daily_samples.insert(
            "2026-08-22".into(),
            OutputSampleBucket {
                saved_tokens: 100,
                baseline_tokens: 400,
            },
        );
        tracker.last_output_estimator_tokens_saved = Some(5_308_371);
        tracker.persist_state().expect("persist");
        let reloaded = SavingsTracker::load_or_create(dir.path()).expect("reload");
        assert_eq!(reloaded.output_daily_samples.len(), 2);
        assert_eq!(
            reloaded.last_output_estimator_tokens_saved,
            Some(5_308_371),
            "watermark survives once the series is current"
        );
    }

    fn make_tracker() -> SavingsTracker {
        let id = uuid::Uuid::new_v4();
        let records_path = std::env::temp_dir().join(format!("headroom-savings-test-{}.jsonl", id));
        let state_path = std::env::temp_dir().join(format!("headroom-savings-state-{}.json", id));
        SavingsTracker {
            records_path,
            state_path,
            session_requests: 0,
            session_estimated_savings_usd: 0.0,
            session_estimated_tokens_saved: 0,
            session_savings_pct: 0.0,
            lifetime_requests: 0,
            lifetime_token_milestone_high_water: 0,
            lifetime_tool_schema_tokens_saved: 0,
            tool_schema_process_total: None,
            last_observation: None,
            display_session_baseline: None,
            session_savings_history: Vec::new(),
            session_new_input_history: Vec::new(),
            session_hourly_buckets: std::collections::BTreeMap::new(),
            daily_savings: std::collections::BTreeMap::new(),
            hourly_savings: std::collections::BTreeMap::new(),
            output_daily_samples: std::collections::BTreeMap::new(),
            output_hourly_samples: std::collections::BTreeMap::new(),
            tool_schema_daily_samples: std::collections::BTreeMap::new(),
            tool_schema_hourly_samples: std::collections::BTreeMap::new(),
            output_sample_watermark: None,
            last_output_estimator_tokens_saved: None,
            last_output_estimator_baseline_tokens: None,
            last_written_at: None,
        }
    }

    fn history_point_at(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        total_tokens_saved: u64,
    ) -> HeadroomSavingsHistoryPoint {
        HeadroomSavingsHistoryPoint {
            timestamp: Utc
                .with_ymd_and_hms(year, month, day, hour, 0, 0)
                .single()
                .expect("valid timestamp"),
            total_tokens_saved,
        }
    }

    fn temp_test_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }

    fn write_headroom_receipt(base_dir: &PathBuf, version: &str, requirements_lock_sha256: &str) {
        let runtime = crate::tool_manager::ManagedRuntime::bootstrap_root(base_dir);
        fs::create_dir_all(&runtime.tools_dir).expect("create tools dir");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            format!(
                r#"{{
                    "version":"{}",
                    "artifact":{{"requirementsLockSha256":"{}"}}
                }}"#,
                version, requirements_lock_sha256
            ),
        )
        .expect("write receipt");
    }

    #[test]
    fn note_lifetime_token_total_fires_high_water_milestones_once() {
        let mut tracker = make_tracker();
        // First crossing past 100k fires; staying flat or dipping fires nothing.
        assert_eq!(tracker.note_lifetime_token_total(150_000), vec![100_000]);
        assert_eq!(
            tracker.note_lifetime_token_total(120_000),
            Vec::<u64>::new()
        );
        assert_eq!(
            tracker.note_lifetime_token_total(150_000),
            Vec::<u64>::new()
        );
        // Advancing past the next thresholds fires each crossed milestone once.
        assert_eq!(
            tracker.note_lifetime_token_total(5_500_000),
            vec![1_000_000, 5_000_000]
        );
        // Repeating 10M-step milestones fire as the total climbs past them.
        assert_eq!(
            tracker.note_lifetime_token_total(21_000_000),
            vec![10_000_000, 20_000_000]
        );
    }

    #[test]
    fn aggregate_weekly_totals_sums_active_days_in_window() {
        use std::collections::BTreeMap;
        let mut daily: BTreeMap<String, DailySavingsBucket> = BTreeMap::new();
        daily.insert(
            "2026-04-19".into(), // outside window (Sunday of week before)
            DailySavingsBucket {
                estimated_savings_usd: 1.0,
                estimated_tokens_saved: 50,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        daily.insert(
            "2026-04-20".into(),
            DailySavingsBucket {
                estimated_savings_usd: 2.5,
                estimated_tokens_saved: 200,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        daily.insert(
            "2026-04-23".into(),
            DailySavingsBucket {
                estimated_savings_usd: 1.0,
                estimated_tokens_saved: 100,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        daily.insert(
            "2026-04-26".into(),
            DailySavingsBucket {
                estimated_savings_usd: 0.0,
                estimated_tokens_saved: 0, // zero activity day — not counted
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        daily.insert(
            "2026-04-27".into(), // outside window (today Monday)
            DailySavingsBucket {
                estimated_savings_usd: 99.0,
                estimated_tokens_saved: 9999,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        let start = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
        let totals = aggregate_weekly_totals(&daily, start, end);
        assert_eq!(totals.active_days, 2);
        assert_eq!(totals.total_tokens_saved, 300);
        assert!((totals.total_savings_usd - 3.5).abs() < 1e-9);
    }

    #[test]
    fn most_recent_monday_maps_every_weekday_to_this_weeks_monday() {
        use chrono::NaiveDate;
        // Monday 2026-04-27 — itself.
        assert_eq!(
            most_recent_monday(NaiveDate::from_ymd_opt(2026, 4, 27).unwrap()),
            NaiveDate::from_ymd_opt(2026, 4, 27).unwrap()
        );
        // Wednesday 2026-04-29 — back to Monday 27.
        assert_eq!(
            most_recent_monday(NaiveDate::from_ymd_opt(2026, 4, 29).unwrap()),
            NaiveDate::from_ymd_opt(2026, 4, 27).unwrap()
        );
        // Sunday 2026-05-03 — back to Monday 27 (6 days back).
        assert_eq!(
            most_recent_monday(NaiveDate::from_ymd_opt(2026, 5, 3).unwrap()),
            NaiveDate::from_ymd_opt(2026, 4, 27).unwrap()
        );
    }

    /// The whole-session blast radius, in one test: 0 means "my own group", and
    /// on a Linux desktop that group can be the login session.
    #[test]
    fn group_kill_refuses_targets_that_are_not_a_child_of_ours() {
        assert_eq!(
            super::group_kill_target(0),
            None,
            "0 is our own process group"
        );
        assert_eq!(super::group_kill_target(1), None, "1 is init");
        assert_eq!(
            super::group_kill_target(-1),
            None,
            "-1 is everything we can signal"
        );
        assert_eq!(
            super::group_kill_target(4242).as_deref(),
            Some("-4242"),
            "a real child's group must still be signalled"
        );
    }

    /// A launch racing a quit used to strand the app: `stop_headroom` waited on
    /// `lifecycle_lock` forever, so `restart_app` never reached the exit request
    /// and the window sat on "Restarting..." until it was killed by hand.
    #[test]
    fn recent_app_kills_keeps_the_newest_with_ages() {
        for i in 0..12 {
            super::note_app_kill("ring-test", format!("entry {i}"));
        }
        let summary = super::recent_app_kills_summary();
        assert!(summary.len() <= super::RECENT_APP_KILLS_CAP, "{summary:?}");
        assert!(
            summary.last().unwrap().ends_with("ring-test: entry 11"),
            "{summary:?}"
        );
        assert!(summary.iter().all(|l| l.contains("s ago ")), "{summary:?}");
        assert!(
            !summary.iter().any(|l| l.ends_with("entry 3")),
            "oldest must roll off: {summary:?}"
        );
    }

    #[test]
    fn sweep_only_reaps_orphans_and_optionally_own_children() {
        use super::sweep_should_kill;
        let me = 4242;
        // Orphan of a previous instance (reparented to launchd/init).
        assert!(sweep_should_kill(1, me, true));
        assert!(sweep_should_kill(1, me, false));
        assert!(sweep_should_kill(0, me, false));
        // Our own untracked child: ours to kill only when we hold the
        // lifecycle lock; otherwise a sibling transition is mid-spawn on it.
        assert!(sweep_should_kill(me, me, true));
        assert!(!sweep_should_kill(me, me, false));
        // Another live process's child (a relaunched Headroom instance, or a
        // shell running the venv by hand): never ours.
        assert!(!sweep_should_kill(777, me, true));
        assert!(!sweep_should_kill(777, me, false));
    }

    #[test]
    fn parse_pid_ppid_reads_ps_output_and_skips_junk() {
        let out = "  501     1\n 502   501\n\nPID PPID\n  abc 12\n 503  4242\n";
        assert_eq!(
            super::parse_pid_ppid(out),
            vec![(501, 1), (502, 501), (503, 4242)]
        );
        assert!(super::parse_pid_ppid("").is_empty());
    }

    #[test]
    fn stop_headroom_gives_up_on_a_held_lifecycle_lock() {
        let base_dir = temp_test_dir("headroom-stop-lifecycle-lock");
        let state = std::sync::Arc::new(AppState::new_in(base_dir.clone()).expect("app state"));

        // Baseline the same call with the lock free. Everything after the lock
        // is two process sweeps that shell out (`pkill` / `Get-CimInstance`),
        // and they cost whatever the machine costs: ~1.2s on a warm CI runner,
        // ~48s on a cold-cache Windows one still scanning freshly built
        // binaries. Subtracting it makes the ceiling below bound the LOCK WAIT
        // rather than the runner -- it was failing on the latter.
        let started = Instant::now();
        state.stop_headroom();
        let baseline = started.elapsed();

        let holder = std::sync::Arc::clone(&state);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = holder.lifecycle_lock.lock();
            locked_tx.send(()).expect("signal locked");
            // Outlive the timeout, then let the guard drop.
            let _ = release_rx.recv();
        });
        locked_rx.recv().expect("lock taken");

        let started = Instant::now();
        state.stop_headroom();
        let waited = started.elapsed();

        let _ = release_tx.send(());
        holder.join().expect("holder thread");

        assert!(
            waited >= super::STOP_LIFECYCLE_LOCK_TIMEOUT,
            "returned before the lock timeout ({waited:?}), so it never waited for the lock"
        );
        let lock_wait = waited.saturating_sub(baseline);
        assert!(
            lock_wait < super::STOP_LIFECYCLE_LOCK_TIMEOUT * 4,
            "stop_headroom blocked on the held lifecycle lock for {lock_wait:?} \
             (total {waited:?}, sweep baseline {baseline:?})"
        );
    }

    #[test]
    fn learn_step_is_recorded_only_while_a_run_is_active() {
        let base_dir = temp_test_dir("headroom-learn-step");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        // No active run: a stray line must not put a step on screen.
        state.set_headroom_learn_step("Analyzing with Claude Code".into());
        assert_eq!(state.headroom_learn_status(None).current_step, None);

        state.mark_headroom_learn_running_for_test();
        state.set_headroom_learn_step("Analyzing with Claude Code".into());
        assert_eq!(
            state.headroom_learn_status(None).current_step.as_deref(),
            Some("Analyzing with Claude Code")
        );

        state.complete_headroom_learn_run(true, "done".into(), None, Vec::new());
        assert_eq!(state.headroom_learn_status(None).current_step, None);

        let _ = fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn observe_activity_separates_fresh_from_recent_across_calls() {
        use crate::models::TransformationFeedEvent;
        let base_dir = temp_test_dir("headroom-activity-observation");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        let transformation = TransformationFeedEvent {
            request_id: Some("r1".into()),
            timestamp: Some("2026-04-22T10:00:00Z".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-7".into()),
            input_tokens_original: Some(10_000),
            input_tokens_optimized: Some(2_000),
            tokens_saved: Some(8_000),
            savings_percent: Some(80.0),
            transforms_applied: vec!["kompress".into()],
            workspace: Some("/Users/u/Code/demo".into()),
            turn_id: None,
            request_messages: None,
            compressed_messages: None,
        };

        let first = state.observe_activity_from_transformations(&[transformation.clone()]);
        assert!(
            !first.fresh.is_empty(),
            "first observation should emit fresh events"
        );
        // First compression that beats the zero baseline emits a Daily+AllTime
        // Record.
        assert!(
            first
                .fresh
                .iter()
                .any(|e| matches!(e, ActivityEvent::Record(_))),
            "first record should fire"
        );
        // Snapshot after the first observation has the record slot populated.
        let first_snapshot = state.activity_feed_snapshot();
        assert!(first_snapshot.record.is_some());
        assert!(first_snapshot.transformation.is_some());

        let second = state.observe_activity_from_transformations(&[transformation]);
        assert!(
            second.fresh.is_empty(),
            "second observation of same transformation should emit no fresh events"
        );
        // Snapshot still carries the slots across the no-op second call.
        let second_snapshot = state.activity_feed_snapshot();
        assert!(second_snapshot.record.is_some());
        assert!(second_snapshot.transformation.is_some());

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn dashboard_includes_managed_tools() {
        let base_dir = temp_test_dir("headroom-app-state");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        let dashboard = state.dashboard();

        assert!(dashboard.tools.iter().any(|tool| tool.id == "headroom"));
        assert!(dashboard.tools.iter().any(|tool| tool.id == "rtk"));
        assert!(dashboard
            .insights
            .iter()
            .any(|insight| !insight.title.is_empty()));

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn headline_includes_tool_schema_usd_but_milestone_tokens_do_not() {
        let base_dir = temp_test_dir("headroom-headline-tool-schema");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        {
            let mut tracker = state.savings_tracker.lock();
            tracker.daily_savings.insert(
                "2026-09-01".to_string(),
                DailySavingsBucket {
                    estimated_savings_usd: 2.0,
                    estimated_tokens_saved: 1_000_000,
                    ..Default::default()
                },
            );
            tracker.lifetime_tool_schema_tokens_saved = 500_000;
        }
        // Prime both poll caches with a fresh miss: on a dev machine the real
        // proxy answers on 6767, and its live stats would replace the seeded
        // buckets above.
        *state.cached_headroom_stats.lock() = Some((None, Instant::now()));
        *state.cached_headroom_history.lock() = Some((None, Instant::now(), true));

        // The output layer prices off ~/.headroom/output_savings.json; on a
        // developer machine that real ledger adds hundreds of dollars to the
        // headline. Swap HOME (restored on drop, panic included) so the
        // arithmetic below is exact.
        struct HomeSwap {
            prev: Option<std::ffi::OsString>,
            _lock: std::sync::MutexGuard<'static, ()>,
        }
        impl Drop for HomeSwap {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
        let fake_home = temp_test_dir("headroom-headline-home");
        fs::create_dir_all(&fake_home).expect("create fake home");
        let _home = HomeSwap {
            prev: std::env::var_os("HOME"),
            _lock: crate::test_env_lock::lock_home(),
        };
        std::env::set_var("HOME", &fake_home);

        let dashboard = state.dashboard();
        // Deferral priced with the blended $/token the buckets imply, at the
        // cache-read ratio: (2.0 / 1M) * 0.10 * 500k = 0.10 on top of 2.0.
        assert!(
            (dashboard.lifetime_estimated_savings_usd - 2.10).abs() < 1e-9,
            "headline must include the deferral layer, got {}",
            dashboard.lifetime_estimated_savings_usd
        );
        // Token milestones fire off this total; a savings layer that starts
        // reporting must not move it (or every user gets a milestone storm).
        assert_eq!(dashboard.lifetime_estimated_tokens_saved, 1_000_000);

        fs::remove_dir_all(base_dir).expect("remove temp dir");
        let _ = fs::remove_dir_all(fake_home);
    }

    #[test]
    fn proxy_bypass_initialises_to_false() {
        let base_dir = temp_test_dir("headroom-bypass-init");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        assert!(
            !state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "fresh AppState must default to bypass=off so the intercept routes through the Python proxy"
        );
        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    fn pricing_status_with_optimization(allowed: bool) -> crate::models::HeadroomPricingStatus {
        use crate::models::{
            ClaudeAccountProfile, ClaudeAuthMethod, ClaudePlanTier, HeadroomPricingStatus,
        };
        let now = chrono::Utc::now();
        HeadroomPricingStatus {
            authenticated: true,
            codex_plan_tier: None,
            local_grace_started_at: now,
            local_grace_ends_at: now,
            local_grace_active: false,
            account_sync_error: None,
            needs_authentication: false,
            optimization_allowed: allowed,
            should_nudge: false,
            nudge_level: 0,
            gate_reason: None,
            gate_message: String::new(),
            nudge_threshold_percent: None,
            effective_nudge_thresholds_percent: None,
            disable_threshold_percent: None,
            effective_disable_threshold_percent: None,
            recommended_subscription_tier: None,
            tier_mismatch: None,
            claude: ClaudeAccountProfile {
                auth_method: ClaudeAuthMethod::Unknown,
                email: None,
                display_name: None,
                account_uuid: None,
                organization_uuid: None,
                billing_type: None,
                account_created_at: None,
                subscription_created_at: None,
                has_extra_usage_enabled: false,
                plan_tier: ClaudePlanTier::Unknown,
                plan_detection_source: None,
                organization_type: None,
                rate_limit_tier: None,
                user_rate_limit_tier: None,
                seat_tier: None,
                weekly_utilization_pct: None,
                weekly_resets_at: None,
                five_hour_utilization_pct: None,
                extra_usage_monthly_limit: None,
                profile_fetch_error: None,
            },
            codex: None,
            account: None,
            launch_discount_active: false,
            active_percent_off: 0,
            pricing_cohorts: Vec::new(),
            intro_offer: None,
            plan_prices: None,
        }
    }

    #[test]
    fn apply_pricing_gate_status_flips_bypass_on_for_gated_status() {
        let base_dir = temp_test_dir("headroom-bypass-on");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        assert!(!state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        // Debounce: first gated reading just bumps the streak.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        assert!(
            !state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "first gated reading must not flip bypass yet"
        );

        // Second consecutive gated reading crosses the debounce threshold.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        assert!(
            state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "second consecutive gated reading must flip bypass=true"
        );
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn apply_pricing_gate_status_uses_claude_only_bypass_when_codex_kept_alive() {
        let base_dir = temp_test_dir("headroom-claude-only-bypass");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        // Two consecutive gated readings cross the debounce threshold, but with
        // codex_keep_alive=true the gate must use the Claude-only bypass so the
        // Python backend stays up for Codex.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), true);
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), true);
        assert!(
            state
                .claude_only_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "Claude-only bypass must flip on when Codex is kept alive"
        );
        assert!(
            !state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "full bypass must stay off so Python keeps serving Codex"
        );

        // An ungated reading clears both flags.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(true), true);
        assert!(!state
            .claude_only_bypass
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(!state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        fs::remove_dir_all(base_dir).ok();
    }

    fn codex_usage_with_optimization(allowed: bool) -> crate::models::CodexUsage {
        crate::models::CodexUsage {
            limit_name: None,
            primary: None,
            secondary: None,
            credits_balance: None,
            credits_unlimited: false,
            optimization_allowed: allowed,
            should_nudge: false,
            nudge_level: 0,
            gate_reason: None,
            recommended_subscription_tier: None,
            weekly_used_percent: None,
            gate_message: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn apply_codex_gate_flips_codex_bypass_without_stopping_backend() {
        let base_dir = temp_test_dir("headroom-codex-bypass");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        assert!(!state
            .codex_bypass
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(
            !state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "Claude bypass must stay untouched by the Codex gate"
        );

        // Debounce: first gated reading just bumps the streak.
        state.apply_codex_pricing_gate_status(Some(&codex_usage_with_optimization(false)));
        assert!(!state
            .codex_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        // Second consecutive gated reading crosses the debounce threshold.
        state.apply_codex_pricing_gate_status(Some(&codex_usage_with_optimization(false)));
        assert!(state
            .codex_bypass
            .load(std::sync::atomic::Ordering::Acquire));
        // Crucially the Claude-wide bypass never flipped, so Claude stays optimized.
        assert!(!state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        // An ungated reading clears the Codex bypass again.
        state.apply_codex_pricing_gate_status(Some(&codex_usage_with_optimization(true)));
        assert!(!state
            .codex_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn apply_codex_gate_ignores_absent_usage() {
        let base_dir = temp_test_dir("headroom-codex-bypass-none");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        // Flip it on first.
        state.apply_codex_pricing_gate_status(Some(&codex_usage_with_optimization(false)));
        state.apply_codex_pricing_gate_status(Some(&codex_usage_with_optimization(false)));
        assert!(state
            .codex_bypass
            .load(std::sync::atomic::Ordering::Acquire));
        // A poll with no Codex signal must leave the gate as-is, not clear it.
        state.apply_codex_pricing_gate_status(None);
        assert!(state
            .codex_bypass
            .load(std::sync::atomic::Ordering::Acquire));
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn apply_pricing_gate_status_resets_streak_on_ungated_reading() {
        let base_dir = temp_test_dir("headroom-bypass-debounce-reset");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        // One gated reading bumps the streak to 1.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        assert!(!state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        // Ungated reading resets the streak — a single-poll spike clears.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(true), false);

        // Now another gated reading is the first of a new window, not the
        // second of the old one. Bypass must still be off.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        assert!(
            !state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "an intervening ungated reading must reset the debounce streak"
        );
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn apply_pricing_gate_status_clears_bypass_for_ungated_status() {
        let base_dir = temp_test_dir("headroom-bypass-off");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        // Pre-set the flag, simulating that the gate fired earlier.
        state
            .proxy_bypass
            .store(true, std::sync::atomic::Ordering::Release);

        state.apply_pricing_gate_status(&pricing_status_with_optimization(true), false);

        assert!(
            !state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire),
            "ungated status must clear bypass — this is the upgrade-recovery path"
        );
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn apply_pricing_gate_status_is_idempotent_when_state_already_matches() {
        let base_dir = temp_test_dir("headroom-bypass-noop");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        // Already off + ungated status → still off (no transition triggered).
        state.apply_pricing_gate_status(&pricing_status_with_optimization(true), false);
        assert!(!state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        // Two consecutive gated readings cross the debounce threshold and flip.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        assert!(state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        // Already on + gated status → still on.
        state.apply_pricing_gate_status(&pricing_status_with_optimization(false), false);
        assert!(state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire));

        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn last_known_good_plan_returns_none_on_fresh_install() {
        let base_dir = temp_test_dir("headroom-last-known-good-fresh");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        assert!(state.last_known_good_plan_tier().is_none());
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn record_known_good_plan_tier_skips_unknown() {
        use crate::models::ClaudePlanTier;
        let base_dir = temp_test_dir("headroom-last-known-good-skip-unknown");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        state.record_known_good_plan_tier(&ClaudePlanTier::Pro);
        state.record_known_good_plan_tier(&ClaudePlanTier::Unknown);
        assert!(matches!(
            state.last_known_good_plan_tier(),
            Some(ClaudePlanTier::Pro)
        ));
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn last_known_good_plan_persists_across_appstate_reload() {
        use crate::models::ClaudePlanTier;
        let base_dir = temp_test_dir("headroom-last-known-good-persist");
        {
            let state = AppState::new_in(base_dir.clone()).expect("app state");
            state.record_known_good_plan_tier(&ClaudePlanTier::Max5x);
        }
        let reloaded = AppState::new_in(base_dir.clone()).expect("reloaded app state");
        assert!(matches!(
            reloaded.last_known_good_plan_tier(),
            Some(ClaudePlanTier::Max5x)
        ));
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn record_known_good_plan_tier_overwrites_with_newer_known_tier() {
        use crate::models::ClaudePlanTier;
        let base_dir = temp_test_dir("headroom-last-known-good-overwrite");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        state.record_known_good_plan_tier(&ClaudePlanTier::Pro);
        state.record_known_good_plan_tier(&ClaudePlanTier::Max20x);
        assert!(matches!(
            state.last_known_good_plan_tier(),
            Some(ClaudePlanTier::Max20x)
        ));
        fs::remove_dir_all(base_dir).ok();
    }

    #[test]
    fn runtime_maintenance_plan_prefers_requirements_repair_when_only_lock_is_stale() {
        let base_dir = temp_test_dir("headroom-maintenance-repair");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        write_headroom_receipt(
            &base_dir,
            crate::tool_manager::HEADROOM_PINNED_VERSION,
            "stale",
        );

        let plan = state.runtime_maintenance_plan_for_app_version(env!("CARGO_PKG_VERSION"));
        assert!(matches!(
            plan,
            Some(super::RuntimeMaintenancePlan::RequirementsRepair)
        ));

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn runtime_maintenance_plan_prefers_upgrade_over_requirements_repair() {
        let base_dir = temp_test_dir("headroom-maintenance-upgrade");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        write_headroom_receipt(&base_dir, "0.6.5", "stale");

        let plan = state.runtime_maintenance_plan_for_app_version(env!("CARGO_PKG_VERSION"));
        match plan {
            Some(super::RuntimeMaintenancePlan::Upgrade(release)) => {
                assert_eq!(
                    release.version(),
                    crate::tool_manager::HEADROOM_PINNED_VERSION
                );
            }
            _ => panic!("expected version upgrade plan"),
        }

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn runtime_maintenance_plan_skips_when_current_app_version_already_succeeded() {
        let base_dir = temp_test_dir("headroom-maintenance-stamped");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        write_headroom_receipt(&base_dir, "0.9.7", "stale");
        state.stamp_app_version(env!("CARGO_PKG_VERSION"));

        let plan = state.runtime_maintenance_plan_for_app_version(env!("CARGO_PKG_VERSION"));
        assert!(plan.is_none());

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn runtime_maintenance_plan_skips_when_retry_budget_is_exhausted() {
        let base_dir = temp_test_dir("headroom-maintenance-budget");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        write_headroom_receipt(&base_dir, "0.6.5", "stale");

        for _ in 0..super::MAX_UPGRADE_AUTO_RETRIES {
            state.record_upgrade_failure(RuntimeUpgradeFailure {
                app_version: env!("CARGO_PKG_VERSION").into(),
                target_headroom_version: "0.8.2".into(),
                fallback_headroom_version: Some("0.6.5".into()),
                failure_phase: UpgradeFailurePhase::Install,
                attempts: 0,
                first_attempt_at: Utc::now(),
                last_attempt_at: Utc::now(),
                error_message: "failed".into(),
                error_hint: None,
                rollback_restored: true,
            });
        }

        let plan = state.runtime_maintenance_plan_for_app_version(env!("CARGO_PKG_VERSION"));
        assert!(plan.is_none());

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn can_stamp_no_maintenance_allows_stamp_when_version_changed_with_no_failure() {
        let base_dir = temp_test_dir("can-stamp-fresh");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        // Stamp set to an older app version, no failure record.
        state.stamp_app_version("0.3.6-rc.3");
        assert!(state.can_stamp_no_maintenance("0.3.12-rc.3"));
        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn can_stamp_no_maintenance_skips_stamp_when_already_current() {
        let base_dir = temp_test_dir("can-stamp-idempotent");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        state.stamp_app_version("0.3.12-rc.3");
        assert!(!state.can_stamp_no_maintenance("0.3.12-rc.3"));
        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn can_stamp_no_maintenance_skips_stamp_when_failure_recorded_for_current_version() {
        let base_dir = temp_test_dir("can-stamp-with-failure");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        state.stamp_app_version("0.3.6-rc.3");
        state.record_upgrade_failure(RuntimeUpgradeFailure {
            app_version: "0.3.12-rc.3".into(),
            target_headroom_version: "0.20.15".into(),
            fallback_headroom_version: Some("0.19.0".into()),
            failure_phase: UpgradeFailurePhase::BootValidation,
            attempts: 0,
            first_attempt_at: Utc::now(),
            last_attempt_at: Utc::now(),
            error_message: "failed".into(),
            error_hint: None,
            rollback_restored: true,
        });
        assert!(!state.can_stamp_no_maintenance("0.3.12-rc.3"));
        // Still allows stamping for an unrelated future version, since the
        // failure record is keyed on the specific version that failed.
        assert!(state.can_stamp_no_maintenance("0.3.13"));
        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn lifetime_token_milestones_include_firsts_and_repeating_tens() {
        assert_eq!(
            lifetime_token_milestones_crossed(0, 5_000_000),
            vec![100_000, 1_000_000, 5_000_000]
        );
        assert_eq!(
            lifetime_token_milestones_crossed(9_500_000, 21_000_000),
            vec![10_000_000, 20_000_000]
        );
        assert_eq!(lifetime_token_milestones_crossed(0, 150_000), vec![100_000]);
    }

    #[test]
    fn dashboard_read_path_preserves_pending_milestones_for_analytics() {
        // Regression guard: `state.dashboard()` (tray updater, bootstrap
        // finalize, account activation) must not advance the milestone
        // high-water mark. Only `dashboard_with_pending_milestones()` — the
        // path that fires the aptabase event, pricing report, and in-app
        // notification — may consume crossings. A prior refactor drained on
        // every call, so the tray updater's 5s heartbeat silently ate crossings.
        let base_dir = temp_test_dir("headroom-milestone-preservation");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        // savings_history backfills the daily buckets the lifetime total (and
        // hence milestones) is derived from; a cumulative 1.5M crosses 100k+1M.
        let stats = HeadroomDashboardStats {
            output_shaper_active: None,
            reread_tokens: None,
            reread_compressed_tokens: None,
            ccr_retrievals: None,
            learner_progress: None,
            output_reduction: None,
            tool_schema_tokens_saved: None,
            session_requests: Some(1),
            session_estimated_savings_usd: Some(1.0),
            session_estimated_tokens_saved: Some(1_500_000),
            session_savings_pct: Some(50.0),
            session_actual_cost_usd: Some(0.5),
            session_total_tokens_sent: Some(1_500_000),
            savings_history: vec![
                history_point_at(2026, 3, 20, 11, 0),
                history_point_at(2026, 3, 20, 12, 1_500_000),
            ],
        };
        *state.cached_headroom_stats.lock() = Some((Some(stats), std::time::Instant::now()));
        // Pin the history cache to a fresh miss so build_dashboard doesn't try
        // to fetch native rollups over the network during the test.
        *state.cached_headroom_history.lock() = Some((None, std::time::Instant::now(), true));

        // Read-only path observes (building buckets) but must not surface or
        // consume milestones.
        let _ = state.dashboard();

        let (_, drained) = state.dashboard_with_pending_milestones();
        assert_eq!(
            drained.token,
            vec![100_000, 1_000_000],
            "drain path must surface milestones for the accrued lifetime total"
        );

        let (_, drained_again) = state.dashboard_with_pending_milestones();
        assert!(
            drained_again.token.is_empty(),
            "second drain finds nothing: milestones fire exactly once"
        );

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn session_counters_follow_headroom_stats() {
        let mut tracker = make_tracker();

        let first = tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(10),
                session_estimated_savings_usd: Some(1.2),
                session_estimated_tokens_saved: Some(1_200),
                session_savings_pct: Some(24.0),
                session_actual_cost_usd: Some(3.8),
                session_total_tokens_sent: Some(3_800),
                savings_history: Vec::new(),
            })
            .expect("first snapshot");
        assert_eq!(first.session_requests, 10);
        assert_eq!(first.session_estimated_tokens_saved, 1_200);
        assert!((first.session_estimated_savings_usd - 1.2).abs() < 1e-9);
        assert!((first.session_savings_pct - 24.0).abs() < 1e-9);
        assert_eq!(first.lifetime_requests, 10);

        let second = tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(12),
                session_estimated_savings_usd: Some(1.5),
                session_estimated_tokens_saved: Some(1_500),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(4.5),
                session_total_tokens_sent: Some(4_500),
                savings_history: Vec::new(),
            })
            .expect("second snapshot");
        assert_eq!(second.session_requests, 12);
        assert_eq!(second.session_estimated_tokens_saved, 1_500);
        assert!((second.session_estimated_savings_usd - 1.5).abs() < 1e-9);
        assert_eq!(second.lifetime_requests, 12);
    }

    #[test]
    fn new_session_resets_live_session_and_keeps_lifetime() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(10),
                session_estimated_savings_usd: Some(1.0),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(20.0),
                session_actual_cost_usd: Some(4.0),
                session_total_tokens_sent: Some(4_000),
                savings_history: Vec::new(),
            })
            .expect("initial session");

        let reset = tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(2),
                session_estimated_savings_usd: Some(0.2),
                session_estimated_tokens_saved: Some(200),
                session_savings_pct: Some(18.0),
                session_actual_cost_usd: Some(0.9),
                session_total_tokens_sent: Some(900),
                savings_history: Vec::new(),
            })
            .expect("reset snapshot");
        assert_eq!(reset.session_requests, 2);
        assert_eq!(reset.session_estimated_tokens_saved, 200);
        assert!((reset.session_estimated_savings_usd - 0.2).abs() < 1e-9);
        assert_eq!(reset.lifetime_requests, 12);
    }

    #[test]
    fn first_observation_backfills_daily_history_from_headroom() {
        let mut tracker = make_tracker();
        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(4),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 11, 0),
                    history_point_at(2026, 3, 20, 12, 400),
                    history_point_at(2026, 3, 21, 12, 1_000),
                ],
            })
            .expect("snapshot");

        let daily = tracker.daily_savings();
        let expected_days = [
            Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0)
                .single()
                .expect("day one")
                .with_timezone(&Local)
                .format("%Y-%m-%d")
                .to_string(),
            Utc.with_ymd_and_hms(2026, 3, 21, 12, 0, 0)
                .single()
                .expect("day two")
                .with_timezone(&Local)
                .format("%Y-%m-%d")
                .to_string(),
        ];
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].date, expected_days[0]);
        assert_eq!(daily[0].estimated_tokens_saved, 400);
        assert_eq!(daily[0].total_tokens_sent, 1_200);
        assert_eq!(daily[1].date, expected_days[1]);
        assert_eq!(daily[1].estimated_tokens_saved, 600);
        assert_eq!(daily[1].total_tokens_sent, 1_800);
        assert!(
            (daily[0].estimated_savings_usd + daily[1].estimated_savings_usd - 0.5).abs() < 1e-9
        );
        assert!((daily[0].actual_cost_usd - 0.12).abs() < 1e-9);
        assert!((daily[1].actual_cost_usd - 0.18).abs() < 1e-9);
    }

    #[test]
    fn first_observation_backfills_hourly_history_for_today() {
        let mut tracker = make_tracker();
        let today = Local::now().date_naive();

        // Pick three local-time hours today and convert to UTC components for
        // history_point_at. Feeding the local date directly into UTC builders
        // breaks in any TZ where local-hour-N maps to a different UTC date.
        let to_utc_components = |local_hour: u32| -> (i32, u32, u32, u32) {
            let utc = Local
                .with_ymd_and_hms(today.year(), today.month(), today.day(), local_hour, 0, 0)
                .single()
                .expect("unambiguous local time")
                .with_timezone(&Utc);
            (utc.year(), utc.month(), utc.day(), utc.hour())
        };
        let (y0, m0, d0, h0) = to_utc_components(8);
        let (y1, m1, d1, h1) = to_utc_components(9);
        let (y2, m2, d2, h2) = to_utc_components(15);

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(4),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(y0, m0, d0, h0, 0),
                    history_point_at(y1, m1, d1, h1, 400),
                    history_point_at(y2, m2, d2, h2, 1_000),
                ],
            })
            .expect("snapshot");

        let today_key = today.format("%Y-%m-%d").to_string();
        let hourly = tracker
            .hourly_savings()
            .into_iter()
            .filter(|point| point.hour.starts_with(&format!("{today_key}T")))
            .collect::<Vec<_>>();
        let expected_first_hour = format!("{today_key}T09:00");
        let expected_second_hour = format!("{today_key}T15:00");
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].hour, expected_first_hour);
        assert_eq!(hourly[0].estimated_tokens_saved, 400);
        assert_eq!(hourly[1].hour, expected_second_hour);
        assert_eq!(hourly[1].estimated_tokens_saved, 600);
        assert_eq!(hourly[0].total_tokens_sent, 1_200);
        assert_eq!(hourly[1].total_tokens_sent, 1_800);
    }

    #[test]
    fn claude_project_scan_dedupes_repeated_session_files() {
        let test_dir = temp_test_dir("headroom-project-scan");
        fs::create_dir_all(&test_dir).expect("create temp dir");
        let session_file = test_dir.join("session.jsonl");
        fs::write(&session_file, "{\"cwd\":\"/tmp/project\"}\n").expect("write session file");

        let mut scan = ClaudeProjectScan::default();
        scan.add_session_files(vec![session_file.clone(), session_file]);

        assert_eq!(scan.session_files.len(), 1);

        fs::remove_dir_all(&test_dir).expect("remove temp dir");
    }

    #[cfg(unix)]
    #[test]
    fn claude_project_scan_dedupes_symlinked_session_files() {
        use std::os::unix::fs::symlink;

        let test_dir = temp_test_dir("headroom-project-scan-symlink");
        fs::create_dir_all(&test_dir).expect("create temp dir");
        let real_dir = test_dir.join("real");
        let alias_dir = test_dir.join("alias");
        fs::create_dir_all(&real_dir).expect("create real dir");
        symlink(&real_dir, &alias_dir).expect("create alias symlink");

        let real_file = real_dir.join("session.jsonl");
        let alias_file = alias_dir.join("session.jsonl");
        fs::write(&real_file, "{\"cwd\":\"/tmp/project\"}\n").expect("write session file");

        let mut scan = ClaudeProjectScan::default();
        scan.add_session_files(vec![real_file, alias_file]);

        assert_eq!(scan.session_files.len(), 1);

        fs::remove_dir_all(&test_dir).expect("remove temp dir");
    }

    #[test]
    fn parse_headroom_stats_uses_compression_scoped_savings_fields() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "persistent_savings": {
                    "lifetime": {
                        "tokens_saved": 2400,
                        "compression_savings_usd": 0.84
                    }
                },
                "requests": { "total": 5 },
                "tokens": {
                    "saved": 1200,
                    "total_after_compression": 3600
                },
                "cost": {
                    "compression_savings_usd": 0.42,
                    "savings_usd": 9.99,
                    "net_savings_usd": 8.88,
                    "actual_cost_usd": 1.23
                },
                "savings_history": [
                    ["2026-03-21T10:00:00Z", 1200]
                ]
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_requests, Some(5));
        assert_eq!(parsed.session_estimated_tokens_saved, Some(1_200));
        assert_eq!(parsed.session_estimated_savings_usd, Some(0.42));
        assert_eq!(parsed.session_actual_cost_usd, Some(1.23));
        assert_eq!(parsed.session_total_tokens_sent, Some(3_600));
        assert_eq!(parsed.savings_history.len(), 1);
        // No traffic_learner block in this payload (older backend shape).
        assert!(parsed.learner_progress.is_none());
    }

    /// The /stats contract, in one place: every primary JSON path this app
    /// consumes from the backend, each carrying a distinct sentinel. Run this
    /// against the diff of `savings_tracker.py` / `prometheus_metrics.py` /
    /// `server.py` before any wheel bump -- when upstream moves or re-defines
    /// a field (0.36.0 silently widened `compression_savings_usd` to include
    /// tool-schema dollars), this is the test that should force the
    /// conversation. A failing assertion here means the bump changes what
    /// users' savings numbers mean, not just how they are produced.
    #[test]
    fn stats_contract_pins_every_consumed_path() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 41 },
                "tokens": { "saved": 1200, "input": 34000 },
                "cost": { "compression_savings_usd": 3.25, "total_input_cost_usd": 7.5 },
                "prefix_cache": {
                    "totals": { "cache_write_tokens": 900, "uncached_input_tokens": 100 }
                },
                "compression_savings_history": [
                    { "timestamp": "2026-08-21T09:00:00Z", "total_tokens_saved": 700 },
                    { "timestamp": "2026-08-21T10:00:00Z", "total_tokens_saved": 1200 }
                ],
                "summary": { "compression": { "tool_schema_tokens_saved": 777 } },
                "savings": {
                    "by_layer": {
                        "output_shaping": {
                            "available": true,
                            "method": "holdout",
                            "reduction_percent": 12.5,
                            "ci_low_percent": 10.0,
                            "ci_high_percent": 15.0,
                            "requests": 9,
                            "tokens_saved": 4321,
                            "baseline_tokens": 34568
                        },
                        "tool_search": { "tokens_saved": 555 }
                    }
                },
                "traffic_learner": {
                    "requests_processed": 40,
                    "patterns_extracted": 7,
                    "patterns_saved": 1,
                    "pending_patterns": 3,
                    "min_evidence": 5,
                    "history_size": 12
                },
                "waste_signals": { "reread": 91000, "reread_compressed": 4500 },
                "compression": { "ccr_retrievals": 6 },
                "rollout": {
                    "features": [
                        {
                            "name": "proxy_output_shaper",
                            "enabled": false,
                            "decision": "blocked_by_channel"
                        }
                    ]
                }
            }"#,
        )
        .expect("contract fixture must parse");

        assert_eq!(parsed.session_requests, Some(41));
        assert_eq!(parsed.session_estimated_tokens_saved, Some(1200));
        assert_eq!(parsed.session_estimated_savings_usd, Some(3.25));
        assert_eq!(parsed.session_actual_cost_usd, Some(7.5));
        // New-input denominator: cache_write + uncached (1000), never
        // tokens.input -- re-sent cached prefix must not dilute the ratio.
        assert_eq!(parsed.session_total_tokens_sent, Some(1000));
        let pct = parsed.session_savings_pct.expect("pct derived");
        // Compression-only numerator: all-layers saved (1200) minus the
        // tool-schema cumulative (777), over itself plus new input (1000).
        assert!((pct - 423.0 / 1423.0 * 100.0).abs() < 1e-9, "{pct}");

        assert_eq!(parsed.savings_history.len(), 2);
        assert_eq!(parsed.savings_history[0].total_tokens_saved, 700);
        assert_eq!(parsed.savings_history[1].total_tokens_saved, 1200);

        // summary.compression is the process-cumulative counter and must win
        // over the windowed by_layer figure (555).
        assert_eq!(parsed.tool_schema_tokens_saved, Some(777));

        let output = parsed.output_reduction.expect("output layer parsed");
        assert_eq!(output.method, "holdout");
        assert!((output.reduction_percent - 12.5).abs() < 1e-9);
        assert_eq!(output.tokens_saved, 4321);
        assert_eq!(output.baseline_tokens, 34568);

        let learner = parsed.learner_progress.expect("learner parsed");
        assert_eq!(learner.pending_patterns, 3);

        // The rollout block decides whether the shaper's reduction is a live
        // claim; blocked_by_channel must parse as an explicit false.
        assert_eq!(parsed.output_shaper_active, Some(false));

        // Retrieval-churn gauges ride the savings report to the server.
        assert_eq!(parsed.reread_tokens, Some(91000));
        assert_eq!(parsed.reread_compressed_tokens, Some(4500));
        assert_eq!(parsed.ccr_retrievals, Some(6));

        // Fallback contract: without the cumulative counter, the windowed
        // by_layer figure is still accepted for shape.
        let fallback = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 1 },
                "savings": { "by_layer": { "tool_search": { "tokens_saved": 555 } } }
            }"#,
        )
        .expect("fallback fixture must parse");
        assert_eq!(fallback.tool_schema_tokens_saved, Some(555));
        // No rollout block (older wheel) => unknown, and the report gate must
        // stay open rather than mislabel the layer inactive.
        assert_eq!(fallback.output_shaper_active, None);
    }

    #[test]
    fn parse_learner_progress_reads_traffic_learner_block() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "traffic_learner": {
                    "requests_processed": 40,
                    "patterns_extracted": 7,
                    "patterns_saved": 1,
                    "pending_patterns": 3,
                    "min_evidence": 5,
                    "history_size": 12
                }
            }"#,
        )
        .expect("parsed stats");

        let learner = parsed.learner_progress.expect("learner block parsed");
        assert_eq!(learner.pending_patterns, 3);
        assert_eq!(learner.min_evidence, 5);
        assert_eq!(learner.patterns_saved, 1);

        // Learning disabled: backend reports null, which must parse to None
        // (not a zeroed struct implying "alive with nothing pending").
        let disabled = parse_headroom_stats_from_json(
            r#"{ "requests": { "total": 5 }, "traffic_learner": null }"#,
        )
        .expect("parsed stats");
        assert!(disabled.learner_progress.is_none());
    }

    #[test]
    fn parse_output_reduction_reads_available_estimate_from_by_layer() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": { "saved": 1200 },
                "savings": {
                    "by_layer": {
                        "output_shaping": {
                            "available": true,
                            "method": "estimated",
                            "reduction_percent": 18.4,
                            "ci_low_percent": 9.1,
                            "ci_high_percent": 27.7,
                            "requests": 340
                        }
                    }
                }
            }"#,
        )
        .expect("parsed stats");

        let reduction = parsed.output_reduction.expect("output reduction present");
        assert_eq!(reduction.method, "estimated");
        assert_eq!(reduction.reduction_percent, 18.4);
        assert_eq!(reduction.ci_low_percent, 9.1);
        assert_eq!(reduction.ci_high_percent, 27.7);
        assert_eq!(reduction.requests, 340);
    }

    #[test]
    fn parse_output_reduction_is_none_when_unavailable() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": { "saved": 1200 },
                "savings": {
                    "by_layer": {
                        "output_shaping": { "available": false }
                    }
                }
            }"#,
        )
        .expect("parsed stats");
        assert!(parsed.output_reduction.is_none());
    }

    #[test]
    fn parse_output_reduction_hides_out_of_range_estimate() {
        // Fresh install: baseline barely seeded, synthetic control blows up
        // negative. Must hide rather than render "Output −-6,130.7%".
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": { "saved": 1200 },
                "savings": {
                    "by_layer": {
                        "output_shaping": {
                            "available": true,
                            "method": "estimated",
                            "reduction_percent": -6130.7,
                            "ci_low_percent": -8000.0,
                            "ci_high_percent": -4000.0,
                            "requests": 3
                        }
                    }
                }
            }"#,
        )
        .expect("parsed stats");
        assert!(parsed.output_reduction.is_none());
    }

    /// RUST-6F/RUST-6G, second wave: the sweep worked, but powershell exited 1
    /// because a `Stop-Process` in the pipeline failed (the pid was already
    /// gone -- we had just asked the proxy to stop), and `-ErrorAction
    /// SilentlyContinue` hides the message without clearing `$?`. The script
    /// must state its own verdict so a successful sweep cannot be read as a
    /// failure, while a genuinely broken enumeration still is one.
    #[test]
    fn the_windows_sweep_script_reports_its_own_verdict() {
        use super::{windows_process_sweep_script, PS_SWEEP_ENUMERATION_FAILED};
        let script = windows_process_sweep_script(
            std::path::Path::new(r"C:\Users\a\venv\Scripts\headroom.exe"),
            "proxy --port",
            4242,
            true,
        );

        assert!(
            script.trim_end().ends_with("exit 0"),
            "a sweep that ran must not inherit powershell's $?-derived exit code: {script}"
        );
        assert!(
            script.contains(&format!("catch {{ exit {PS_SWEEP_ENUMERATION_FAILED} }}")),
            "a failed enumeration must stay distinguishable: {script}"
        );
        assert!(
            script.contains("-ErrorAction Stop"),
            "without this the catch never fires and every failure looks clean: {script}"
        );
        // The self-kill guard this script already depended on (first RUST-6F
        // wave) must survive the rewrite.
        assert!(
            script.contains("$_.ProcessId -ne $PID"),
            "lost the self-kill guard: {script}"
        );
        // Stop-Process stays best-effort: one unkillable pid must not abort
        // the sweep for the rest.
        assert!(
            script.contains("Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue"),
            "the kill must stay best-effort: {script}"
        );

        // The script is built from a `\`-continued literal, which strips the
        // newline AND the next line's indentation -- so a trailing space on the
        // wrong side of the backslash silently glues two tokens together
        // ("$PID-and"). This only ever runs on Windows, so a slip here is
        // invisible until a user hits it.
        assert!(
            !script.contains('\n'),
            "the script must stay a single -Command line: {script}"
        );
        for token in [
            " | Where-Object ",
            " | ForEach-Object ",
            " -and $_.CommandLine ",
        ] {
            assert!(
                script.contains(token),
                "tokens got glued together around {token:?}: {script}"
            );
        }
        assert_eq!(
            script.matches('{').count(),
            script.matches('}').count(),
            "unbalanced braces: {script}"
        );
    }

    /// RUST-CD: `Stop-Process` on a sibling instance's (or sibling thread's)
    /// in-flight backend is the 0xffffffff-after-banner failure. The script
    /// must carry the parent rule, and must drop our own children when the
    /// caller does not hold the lifecycle lock.
    #[test]
    fn the_windows_sweep_script_filters_on_parent() {
        use super::windows_process_sweep_script;
        let exe = std::path::Path::new(r"C:\Users\a\venv\Scripts\headroom.exe");
        let held = windows_process_sweep_script(exe, "proxy --port", 4242, true);
        assert!(held.contains("$me = 4242;"), "{held}");
        assert!(held.contains("$_.ProcessId -ne $me"), "{held}");
        assert!(
            held.contains("($_.ParentProcessId -eq $me -and $true)"),
            "{held}"
        );
        assert!(
            held.contains(
                "-not (Get-Process -Id $_.ParentProcessId -ErrorAction SilentlyContinue)"
            ),
            "orphans (dead parent) must still be reaped: {held}"
        );
        let unheld = windows_process_sweep_script(exe, "proxy --port", 4242, false);
        assert!(
            unheld.contains("($_.ParentProcessId -eq $me -and $false)"),
            "{unheld}"
        );
    }

    /// A `'` in a Windows username would close the single-quoted `-like`
    /// literal early, and `[`/`]` are wildcards to `-like`.
    #[test]
    fn the_windows_sweep_script_escapes_hostile_paths() {
        use super::windows_process_sweep_script;
        let script = windows_process_sweep_script(
            std::path::Path::new(r"C:\Users\O'Brien [dev]\venv\Scripts\headroom.exe"),
            "proxy --port",
            4242,
            true,
        );
        assert!(script.contains("O''Brien"), "unescaped quote: {script}");
        assert!(script.contains("`[dev`]"), "unescaped wildcard: {script}");
        // Every `-like` literal must still be balanced after escaping.
        assert_eq!(
            script.matches('\'').count() % 2,
            0,
            "escaping left an unbalanced string literal: {script}"
        );
    }

    #[test]
    #[serial_test::serial(stats_fetch_warn)]
    fn stats_fetch_warn_is_throttled_within_the_window() {
        // The dashboard retries /stats every 12s and this warn bridges to
        // Sentry, so only the first failure in a window may speak.
        *STATS_FETCH_WARNED_AT.lock() = None;

        warn_stats_fetch_failed("timed out after 5s");
        let (first, streak) = (*STATS_FETCH_WARNED_AT.lock()).expect("first failure warns");
        assert_eq!(streak, 1);

        warn_stats_fetch_failed("timed out after 5s");
        assert_eq!(
            (*STATS_FETCH_WARNED_AT.lock()).expect("still stamped").0,
            first,
            "a repeat inside the window must not warn again"
        );

        // Instant has no epoch to subtract from on a freshly-booted machine.
        if let Some(stale) =
            Instant::now().checked_sub(STATS_FETCH_WARN_INTERVAL + Duration::from_secs(1))
        {
            *STATS_FETCH_WARNED_AT.lock() = Some((stale, 1));
            warn_stats_fetch_failed("timed out after 5s");
            let (at, streak) = (*STATS_FETCH_WARNED_AT.lock()).expect("re-stamped");
            assert!(
                at > stale,
                "a failure after the window elapsed must warn again"
            );
            assert_eq!(
                streak, 2,
                "the streak advances so the next window is longer"
            );

            // The window that just elapsed no longer clears the new one.
            *STATS_FETCH_WARNED_AT.lock() = Some((stale, 2));
            warn_stats_fetch_failed("timed out after 5s");
            assert_eq!(
                (*STATS_FETCH_WARNED_AT.lock()).expect("unchanged").0,
                stale,
                "a permanent cause must back off instead of warning every window"
            );
        }

        *STATS_FETCH_WARNED_AT.lock() = None;
    }

    #[test]
    #[serial_test::serial(stats_fetch_warn)]
    fn a_lone_success_between_failures_does_not_reset_the_backoff() {
        // RUST-86: a starved backend flaps -- /stats times out only while the
        // proxy is busy -- so timeout/success/timeout was the common shape.
        // Clearing the streak on the FIRST success re-armed an immediate warn
        // every poll, so the streak never advanced past 1 and the 15m..6h
        // decay never applied: 97 events in 2 days from one host.
        *STATS_FETCH_WARNED_AT.lock() = None;
        *STATS_FETCH_RECOVERED_AT.lock() = None;

        warn_stats_fetch_failed("timed out after 15s");
        let (_, streak) = (*STATS_FETCH_WARNED_AT.lock()).expect("first failure warns");
        assert_eq!(streak, 1);

        // One good poll starts a recovery run but must NOT clear the streak.
        note_stats_fetch_success();
        let (stamped, streak) = (*STATS_FETCH_WARNED_AT.lock()).expect("streak survives");
        assert_eq!(streak, 1, "a lone success must not clear the backoff");
        assert!(
            (*STATS_FETCH_RECOVERED_AT.lock()).is_some(),
            "the success starts timing a recovery run"
        );

        // The next failure warns only when the window has elapsed, and it
        // breaks the recovery run.
        warn_stats_fetch_failed("timed out after 15s");
        assert_eq!(
            (*STATS_FETCH_WARNED_AT.lock()).expect("still stamped").0,
            stamped,
            "the flap must stay throttled, not warn again immediately"
        );
        assert!(
            (*STATS_FETCH_RECOVERED_AT.lock()).is_none(),
            "a failure restarts the recovery run"
        );

        // A run that spans the window is a real recovery: the streak clears
        // and the next outage is loud again.
        if let Some(stale) = Instant::now().checked_sub(STATS_FETCH_RECOVERY_WINDOW) {
            *STATS_FETCH_RECOVERED_AT.lock() = Some(stale);
            note_stats_fetch_success();
            assert!(
                (*STATS_FETCH_WARNED_AT.lock()).is_none(),
                "a sustained recovery clears the backoff"
            );

            warn_stats_fetch_failed("timed out after 15s");
            let (_, streak) = (*STATS_FETCH_WARNED_AT.lock()).expect("loud again");
            assert_eq!(streak, 1, "a healed-then-broken cause warns immediately");
        }

        *STATS_FETCH_WARNED_AT.lock() = None;
        *STATS_FETCH_RECOVERED_AT.lock() = None;
    }

    #[test]
    fn stats_miss_serves_the_last_good_payload_and_stops_hammering_the_backend() {
        // RUST-86: a /stats rebuild that outruns its 15s timeout used to blank
        // the dashboard layers AND get re-probed every 12s, so a 15s blocking
        // request was in flight nearly all the time against the very backend
        // that was already starved.
        let state = AppState::new().expect("state");
        let good = HeadroomDashboardStats {
            output_shaper_active: None,
            tool_schema_tokens_saved: Some(4_242),
            ..HeadroomDashboardStats::default()
        };
        *state.last_good_headroom_stats.lock() = Some((good.clone(), Instant::now()));
        // A failed poll, cached as a miss.
        *state.cached_headroom_stats.lock() = Some((None, Instant::now()));

        let served = state
            .cached_headroom_stats()
            .expect("the retained payload covers a transient failure");
        assert_eq!(served.tool_schema_tokens_saved, Some(4_242));

        // The miss is still cached: no fetch was attempted, so the 15s probe
        // is not re-armed on the next dashboard poll.
        let (cached, _) = (*state.cached_headroom_stats.lock())
            .clone()
            .expect("miss stays cached");
        assert!(
            cached.is_none(),
            "serving a retained payload must not \
             overwrite the miss and reset the backoff"
        );

        // Past the retention window the layers go absent rather than being
        // presented as live forever.
        let stale = Instant::now()
            .checked_sub(AppState::HEADROOM_STATS_RETAIN_LAST_GOOD + Duration::from_secs(1));
        if let Some(stale) = stale {
            *state.last_good_headroom_stats.lock() = Some((good, stale));
            assert!(
                state.cached_headroom_stats().is_none(),
                "a retained payload must expire"
            );
        }
    }

    #[test]
    fn stats_fetch_warn_interval_backs_off_and_caps() {
        // RUST-87: one host whose 6767 is owned by another app warned 96x/day
        // under a flat window. Backoff turns an unfixable cause into ~8/day.
        assert_eq!(stats_fetch_warn_interval(1), STATS_FETCH_WARN_INTERVAL);
        assert_eq!(stats_fetch_warn_interval(2), STATS_FETCH_WARN_INTERVAL * 2);
        assert_eq!(stats_fetch_warn_interval(5), STATS_FETCH_WARN_INTERVAL * 16);
        assert_eq!(stats_fetch_warn_interval(6), STATS_FETCH_WARN_MAX_INTERVAL);
        assert_eq!(
            stats_fetch_warn_interval(u32::MAX),
            STATS_FETCH_WARN_MAX_INTERVAL
        );
        // A streak of 0 is unreachable, but must not shift by -1.
        assert_eq!(stats_fetch_warn_interval(0), STATS_FETCH_WARN_INTERVAL);

        // 24h of continuous failure, first warn at t=0.
        let mut elapsed = Duration::ZERO;
        let mut warns = 1u32;
        while elapsed < Duration::from_secs(24 * 3600) {
            elapsed += stats_fetch_warn_interval(warns);
            warns += 1;
        }
        assert!(warns <= 10, "expected <=10 warns/day, got {warns}");
    }

    #[test]
    fn parse_output_reduction_falls_back_to_tokens_block() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": {
                    "saved": 1200,
                    "output_reduction": {
                        "available": true,
                        "method": "measured",
                        "reduction_percent": 22.0,
                        "ci_low_percent": 15.0,
                        "ci_high_percent": 29.0,
                        "requests": 90
                    }
                }
            }"#,
        )
        .expect("parsed stats");
        let reduction = parsed.output_reduction.expect("output reduction present");
        assert_eq!(reduction.method, "measured");
        assert_eq!(reduction.requests, 90);
    }

    #[test]
    fn parse_headroom_stats_ratio_uses_new_input_not_cached_prefix() {
        // The cached prefix (cache_read) is re-sent every turn but never
        // compressed; it must not inflate the savings denominator. Under prompt
        // caching, new content lands in cache_write (here 7_000) plus any
        // uncached input (1_000), so new input is 8_000 and the ratio is
        // 2000 / (2000 + 8000) = 20% -- the 92_000 cache_read is excluded.
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 7 },
                "tokens": {
                    "saved": 2000,
                    "input": 100000
                },
                "prefix_cache": {
                    "totals": {
                        "cache_read_tokens": 92000,
                        "cache_write_tokens": 7000,
                        "uncached_input_tokens": 1000
                    }
                }
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_estimated_tokens_saved, Some(2_000));
        // Denominator is new input (cache_write + uncached), not the 100_000
        // forwarded total and not uncached alone.
        assert_eq!(parsed.session_total_tokens_sent, Some(8_000));
        let pct = parsed.session_savings_pct.expect("savings pct");
        assert!((pct - 20.0).abs() < 1e-9, "expected 20%, got {pct}");
    }

    #[test]
    fn parse_headroom_stats_falls_back_to_total_when_new_input_is_zero() {
        // Fully-cached snapshot: prefix_cache.totals is present but cache_write
        // and uncached are both 0, so new_input_tokens is Some(0). The fallback
        // to the forwarded total (50_000) must still apply -- otherwise the
        // Some(0) skips `.or` and is dropped, leaving savings with zero spend.
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "tokens": {
                    "saved": 2000,
                    "input": 50000
                },
                "prefix_cache": {
                    "totals": {
                        "cache_read_tokens": 92000,
                        "cache_write_tokens": 0,
                        "uncached_input_tokens": 0
                    }
                }
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_estimated_tokens_saved, Some(2_000));
        assert_eq!(parsed.session_total_tokens_sent, Some(50_000));
    }

    #[test]
    fn parse_headroom_stats_history_reads_hourly_and_daily_rollups() {
        let parsed = parse_headroom_stats_history_from_json(
            r#"{
                "lifetime": {
                    "tokens_saved": 205,
                    "compression_savings_usd": 0.205
                },
                "series": {
                    "hourly": [
                        {
                            "timestamp": "2026-03-27T09:00:00Z",
                            "tokens_saved": 150,
                            "compression_savings_usd_delta": 0.15,
                            "total_tokens_saved": 150,
                            "compression_savings_usd": 0.15
                        },
                        {
                            "timestamp": "2026-03-27T10:00:00Z",
                            "tokens_saved": 25,
                            "compression_savings_usd_delta": 0.025,
                            "total_tokens_saved": 175,
                            "compression_savings_usd": 0.175
                        }
                    ],
                    "daily": [
                        {
                            "timestamp": "2026-03-27T00:00:00Z",
                            "tokens_saved": 175,
                            "compression_savings_usd_delta": 0.175,
                            "total_tokens_saved": 175,
                            "compression_savings_usd": 0.175
                        }
                    ]
                }
            }"#,
        )
        .expect("parsed history");

        assert_eq!(parsed.hourly.len(), 2);
        assert_eq!(parsed.hourly[0].tokens_saved, 150);
        assert!((parsed.hourly[0].compression_savings_usd_delta - 0.15).abs() < 1e-9);
        assert_eq!(parsed.daily.len(), 1);

        let daily_points = parsed.daily_savings();
        assert_eq!(daily_points.len(), 1);
        assert_eq!(daily_points[0].date, "2026-03-27");
        assert_eq!(daily_points[0].estimated_tokens_saved, 175);
        assert!((daily_points[0].estimated_savings_usd - 0.175).abs() < 1e-9);
        assert_eq!(daily_points[0].actual_cost_usd, 0.0);
        assert_eq!(daily_points[0].total_tokens_sent, 0);

        let hourly_points = parsed.hourly_savings();
        assert_eq!(hourly_points.len(), 2);
        let expected_hour = Utc
            .with_ymd_and_hms(2026, 3, 27, 9, 0, 0)
            .single()
            .expect("hour")
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:00")
            .to_string();
        assert_eq!(hourly_points[0].hour, expected_hour);
        assert_eq!(hourly_points[0].estimated_tokens_saved, 150);
        assert!((hourly_points[0].estimated_savings_usd - 0.15).abs() < 1e-9);
        // No by_provider in this fixture -> empty breakdown.
        assert!(hourly_points[0].by_provider.is_empty());
        // No raw history in this fixture -> no cache dimension.
        assert_eq!(daily_points[0].cache_read_tokens, None);
        assert_eq!(hourly_points[0].cache_read_tokens, None);
    }

    #[test]
    fn parse_headroom_stats_history_derives_cache_reads_from_checkpoints() {
        // The rollup series has no cache dimension; consecutive cumulative
        // history checkpoints are diffed into per-bucket deltas. First
        // checkpoint's delta is unattributable and skipped; a counter reset
        // (cumulative going backwards) clamps to zero rather than inflating.
        let parsed = parse_headroom_stats_history_from_json(
            r#"{
                "history": [
                    {"timestamp": "2026-03-27T09:10:00Z", "cache_read_tokens": 1000, "cache_savings_usd": 0.9},
                    {"timestamp": "2026-03-27T09:20:00Z", "cache_read_tokens": 1400, "cache_savings_usd": 1.26},
                    {"timestamp": "2026-03-27T10:05:00Z", "cache_read_tokens": 2400, "cache_savings_usd": 2.16},
                    {"timestamp": "2026-03-28T08:00:00Z", "cache_read_tokens": 2000, "cache_savings_usd": 1.8}
                ],
                "series": {
                    "hourly": [
                        {"timestamp": "2026-03-27T09:00:00Z", "tokens_saved": 1, "compression_savings_usd_delta": 0.1},
                        {"timestamp": "2026-03-27T10:00:00Z", "tokens_saved": 1, "compression_savings_usd_delta": 0.1},
                        {"timestamp": "2026-03-28T08:00:00Z", "tokens_saved": 1, "compression_savings_usd_delta": 0.1}
                    ],
                    "daily": [
                        {"timestamp": "2026-03-27T00:00:00Z", "tokens_saved": 2, "compression_savings_usd_delta": 0.2},
                        {"timestamp": "2026-03-28T00:00:00Z", "tokens_saved": 1, "compression_savings_usd_delta": 0.1}
                    ]
                }
            }"#,
        )
        .expect("parsed history");

        let daily_points = parsed.daily_savings();
        // 09:10 checkpoint skipped (no predecessor); 400 + 1000 land on the 27th.
        assert_eq!(daily_points[0].cache_read_tokens, Some(1400));
        // Reset on the 28th (2400 -> 2000) clamps to a zero-delta bucket.
        assert_eq!(daily_points[1].cache_read_tokens, Some(0));
        // Dollar dimension rides the same diffs: 0.36 + 0.90 on the 27th,
        // clamped to zero across the reset on the 28th.
        assert!((daily_points[0].cache_savings_usd.unwrap() - 1.26).abs() < 1e-9);
        assert_eq!(daily_points[1].cache_savings_usd, Some(0.0));

        let hourly_points = parsed.hourly_savings();
        assert_eq!(hourly_points[0].cache_read_tokens, Some(400));
        assert_eq!(hourly_points[1].cache_read_tokens, Some(1000));
        assert_eq!(hourly_points[2].cache_read_tokens, Some(0));
    }

    #[test]
    fn model_rates_rank_by_rate_and_drop_unrepresentative_rows() {
        let parsed = parse_headroom_stats_history_from_json(
            r#"{
                "lifetime": { "compression_savings_usd": 100.0 },
                "by_model": {
                    "claude-opus-5": { "requests": 4823, "savings_percent": 2.67 },
                    "claude-sonnet-5": { "requests": 5663, "savings_percent": 37.86 },
                    "claude-fable-5": { "requests": 7014, "savings_percent": 4.37 },
                    "gpt-5.5": { "requests": 38, "savings_percent": 8.74 },
                    "passthrough:count_tokens": { "requests": 9001, "savings_percent": 0.0 }
                }
            }"#,
        )
        .expect("parsed history");

        let rates = parsed.lifetime.expect("lifetime breakdown").model_rates;
        let names: Vec<&str> = rates.iter().map(|r| r.model.as_str()).collect();
        // Best rate first; gpt-5.5 is under the 100-request floor and the
        // passthrough probe is excluded however many requests it racked up.
        assert_eq!(
            names,
            ["claude-sonnet-5", "claude-fable-5", "claude-opus-5"]
        );
        assert_eq!(rates[0].requests, 5663);
        assert!((rates[0].savings_percent - 37.86).abs() < 1e-9);
    }

    #[test]
    fn parse_headroom_stats_history_reads_lifetime_savings_breakdown() {
        // The lifetime block decomposes savings for the drill-down. Cache
        // savings must come through as their own labelled figure -- they are
        // the client's provider-cache discount, never folded into Headroom's
        // compression number.
        let parsed = parse_headroom_stats_history_from_json(
            r#"{
                "lifetime": {
                    "tokens_saved": 1000,
                    "compression_savings_usd": 5147.32,
                    "output_savings_usd": 4.87,
                    "cache_read_tokens": 1690483122,
                    "cache_savings_usd": 10859.4,
                    "total_input_tokens": 7703977209,
                    "total_input_cost_usd": 24912.66
                },
                "series": {
                    "daily": [
                        {
                            "timestamp": "2026-03-27T00:00:00Z",
                            "tokens_saved": 175,
                            "compression_savings_usd_delta": 0.175
                        }
                    ]
                }
            }"#,
        )
        .expect("parsed history");

        let breakdown = parsed.lifetime.expect("lifetime breakdown present");
        assert!((breakdown.compression_savings_usd - 5147.32).abs() < 1e-9);
        assert!((breakdown.output_savings_usd - 4.87).abs() < 1e-9);
        assert!((breakdown.cache_savings_usd - 10859.4).abs() < 1e-9);
        assert_eq!(breakdown.cache_read_tokens, 1690483122);
        assert_eq!(breakdown.total_input_tokens, 7703977209);
        assert!((breakdown.total_input_cost_usd - 24912.66).abs() < 1e-9);
        // No by_model block in this fixture -> no rows, not a panic.
        assert!(breakdown.model_rates.is_empty());

        // Older backend without the cache fields: breakdown still parses with
        // zeroed extras instead of disappearing.
        let sparse = parse_headroom_stats_history_from_json(
            r#"{
                "lifetime": { "tokens_saved": 205, "compression_savings_usd": 0.205 },
                "series": { "daily": [] }
            }"#,
        )
        .expect("parsed sparse history");
        let sparse_breakdown = sparse.lifetime.expect("sparse breakdown present");
        assert!((sparse_breakdown.compression_savings_usd - 0.205).abs() < 1e-9);
        assert_eq!(sparse_breakdown.cache_savings_usd, 0.0);
        assert_eq!(sparse_breakdown.cache_read_tokens, 0);
    }

    #[test]
    fn parse_headroom_stats_history_drops_carryover_boundary_when_trimmed() {
        // stored_points == max_history_points => the stored history was trimmed,
        // so the oldest rollup bucket (10:00 / day-of) carries a spurious
        // cumulative delta and must be dropped; the real 11:00 bucket stays.
        let body = r#"{
            "lifetime": { "tokens_saved": 1000, "compression_savings_usd": 10.0 },
            "retention": { "max_history_points": 5000 },
            "history_summary": { "stored_points": 5000 },
            "series": {
                "hourly": [
                    {
                        "timestamp": "2026-06-09T10:00:00Z",
                        "tokens_saved": 900,
                        "compression_savings_usd_delta": 9.0,
                        "total_tokens_saved": 900,
                        "compression_savings_usd": 9.0
                    },
                    {
                        "timestamp": "2026-06-09T11:00:00Z",
                        "tokens_saved": 100,
                        "compression_savings_usd_delta": 1.0,
                        "total_tokens_saved": 1000,
                        "compression_savings_usd": 10.0
                    }
                ],
                "daily": [
                    {
                        "timestamp": "2026-06-09T00:00:00Z",
                        "tokens_saved": 900,
                        "compression_savings_usd_delta": 9.0,
                        "total_tokens_saved": 900,
                        "compression_savings_usd": 9.0
                    },
                    {
                        "timestamp": "2026-06-10T00:00:00Z",
                        "tokens_saved": 100,
                        "compression_savings_usd_delta": 1.0,
                        "total_tokens_saved": 1000,
                        "compression_savings_usd": 10.0
                    }
                ]
            }
        }"#;
        let parsed = parse_headroom_stats_history_from_json(body).expect("parsed history");

        // Boundary bucket dropped.
        assert_eq!(parsed.daily.len(), 1);
        assert_eq!(parsed.daily[0].tokens_saved, 100);
        assert_eq!(parsed.hourly.len(), 1);
        assert_eq!(parsed.hourly[0].tokens_saved, 100);

        // ...and the caller must be told, so drop_rollup_backfill does not take
        // a second bite. Both target the same bucket; running both on a
        // two-bucket window left the native series empty, so the chart silently
        // fell back to the tracker's own partial observation for TODAY.
        assert!(parsed.backfill_bucket_dropped);
        let survivors = drop_rollup_backfill(
            parsed.daily_savings(),
            Some("2026-03-30"), // tracker long predates the series
            |p| p.date.as_str(),
        );
        assert!(
            survivors.is_empty(),
            "guard rests on this: a second drop empties the series"
        );
    }

    #[test]
    fn parse_headroom_stats_history_keeps_first_bucket_when_not_trimmed() {
        // stored_points < max_history_points => untrimmed (new user); the first
        // bucket is the genuine origin and must be preserved.
        let body = r#"{
            "lifetime": { "tokens_saved": 1000, "compression_savings_usd": 10.0 },
            "retention": { "max_history_points": 5000 },
            "history_summary": { "stored_points": 12 },
            "series": {
                "daily": [
                    {
                        "timestamp": "2026-06-09T00:00:00Z",
                        "tokens_saved": 900,
                        "compression_savings_usd_delta": 9.0,
                        "total_tokens_saved": 900,
                        "compression_savings_usd": 9.0
                    },
                    {
                        "timestamp": "2026-06-10T00:00:00Z",
                        "tokens_saved": 100,
                        "compression_savings_usd_delta": 1.0,
                        "total_tokens_saved": 1000,
                        "compression_savings_usd": 10.0
                    }
                ]
            }
        }"#;
        let parsed = parse_headroom_stats_history_from_json(body).expect("parsed history");
        assert_eq!(parsed.daily.len(), 2);
        assert_eq!(parsed.daily[0].tokens_saved, 900);
    }

    #[test]
    fn parse_headroom_stats_history_attributes_hourly_by_provider() {
        let parsed = parse_headroom_stats_history_from_json(
            r#"{
                "series": {
                    "hourly": [
                        {
                            "timestamp": "2026-03-27T09:00:00Z",
                            "tokens_saved": 140,
                            "compression_savings_usd_delta": 0.14,
                            "total_input_tokens_delta": 200,
                            "total_input_cost_usd_delta": 0.40,
                            "by_provider": {
                                "openai": {
                                    "tokens_saved": 40,
                                    "compression_savings_usd_delta": 0.04,
                                    "total_input_tokens_delta": 80,
                                    "total_input_cost_usd_delta": 0.16
                                },
                                "anthropic": {
                                    "tokens_saved": 100,
                                    "compression_savings_usd_delta": 0.10,
                                    "total_input_tokens_delta": 120,
                                    "total_input_cost_usd_delta": 0.24
                                }
                            }
                        }
                    ]
                }
            }"#,
        )
        .expect("parsed history");

        // Parsed rollup keeps every provider, sorted by name for stable display.
        let providers = &parsed.hourly[0].by_provider;
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider, "anthropic");
        assert_eq!(providers[1].provider, "openai");

        // hourly_savings() maps the delta fields onto the display point.
        let hourly_points = parsed.hourly_savings();
        let by_provider = &hourly_points[0].by_provider;
        assert_eq!(by_provider.len(), 2);
        let anthropic = &by_provider[0];
        assert_eq!(anthropic.provider, "anthropic");
        assert_eq!(anthropic.estimated_tokens_saved, 100);
        assert!((anthropic.estimated_savings_usd - 0.10).abs() < 1e-9);
        assert_eq!(anthropic.total_tokens_sent, 120);
        assert!((anthropic.actual_cost_usd - 0.24).abs() < 1e-9);
        let openai = &by_provider[1];
        assert_eq!(openai.provider, "openai");
        assert_eq!(openai.estimated_tokens_saved, 40);
        // Per-provider tokens-saved sum back to the bucket total.
        assert_eq!(
            anthropic.estimated_tokens_saved + openai.estimated_tokens_saved,
            hourly_points[0].estimated_tokens_saved
        );
    }

    #[test]
    fn parse_headroom_stats_accepts_naive_local_savings_history_timestamps() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": {
                    "input": 3600,
                    "saved": 1200
                },
                "cost": {
                    "compression_savings_usd": 0.42,
                    "total_input_cost_usd": 0.08
                },
                "savings_history": [
                    ["2026-03-24T11:52:00.866732", 1200]
                ]
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.savings_history.len(), 1);
    }

    #[test]
    fn parse_headroom_stats_prefers_actual_input_cost_and_ignores_generic_total_cost() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": {
                    "saved": 1200,
                    "actual_input_tokens": 3600
                },
                "cost": {
                    "compression_savings_usd": 0.42,
                    "actual_input_cost_usd": 0.08,
                    "total_usd": 1.75
                }
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_actual_cost_usd, Some(0.08));
        assert_eq!(parsed.session_total_tokens_sent, Some(3_600));
    }

    #[test]
    fn parse_headroom_stats_reads_total_input_fields_from_stats_cost_block() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": {
                    "input": 3600,
                    "saved": 1200
                },
                "cost": {
                    "compression_savings_usd": 0.42,
                    "total_input_cost_usd": 0.08,
                    "cost_with_headroom_usd": 0.08
                }
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_actual_cost_usd, Some(0.08));
        assert_eq!(parsed.session_total_tokens_sent, Some(3_600));
    }

    #[test]
    fn parse_headroom_stats_does_not_derive_spend_when_actual_cost_is_missing() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": {
                    "saved": 1200,
                    "total_after_compression": 3600
                },
                "cost": {
                    "compression_savings_usd": 0.42,
                    "total_usd": 1.75
                }
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_actual_cost_usd, None);
        assert_eq!(parsed.session_total_tokens_sent, Some(3_600));
    }

    #[test]
    fn parse_headroom_stats_does_not_derive_tokens_sent_when_missing() {
        let parsed = parse_headroom_stats_from_json(
            r#"{
                "requests": { "total": 5 },
                "tokens": {
                    "saved": 1200,
                    "savings_percent": 25.0
                },
                "cost": {
                    "compression_savings_usd": 0.42,
                    "actual_input_cost_usd": 0.08
                }
            }"#,
        )
        .expect("parsed stats");

        assert_eq!(parsed.session_total_tokens_sent, None);
        assert_eq!(parsed.session_actual_cost_usd, Some(0.08));
    }

    #[test]
    fn first_observation_without_savings_history_does_not_invent_hourly_bucket_totals() {
        let mut tracker = make_tracker();
        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(4),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: Vec::new(),
            })
            .expect("snapshot");

        assert!(tracker.hourly_savings().is_empty());
        assert!(tracker.daily_savings().is_empty());
    }

    #[test]
    fn spend_backfill_is_distributed_across_existing_session_hours() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(4),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.0),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 11, 0),
                    history_point_at(2026, 3, 20, 12, 400),
                    history_point_at(2026, 3, 21, 12, 1_000),
                ],
            })
            .expect("first snapshot");

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(4),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 11, 0),
                    history_point_at(2026, 3, 20, 12, 400),
                    history_point_at(2026, 3, 21, 12, 1_000),
                ],
            })
            .expect("second snapshot");

        let daily = tracker.daily_savings();
        assert_eq!(daily.len(), 2);
        assert!((daily[0].actual_cost_usd - 0.12).abs() < 1e-9);
        assert!((daily[1].actual_cost_usd - 0.18).abs() < 1e-9);
    }

    #[test]
    fn incremental_updates_use_savings_history_hour_keys_instead_of_now() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(1),
                session_estimated_savings_usd: Some(0.2),
                session_estimated_tokens_saved: Some(400),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.12),
                session_total_tokens_sent: Some(1_200),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 8, 0),
                    history_point_at(2026, 3, 20, 9, 400),
                ],
            })
            .expect("first snapshot");

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(2),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 9, 400),
                    history_point_at(2026, 3, 20, 10, 1_000),
                ],
            })
            .expect("second snapshot");

        let hourly = tracker.hourly_savings();
        let expected_first_hour = Utc
            .with_ymd_and_hms(2026, 3, 20, 9, 0, 0)
            .single()
            .expect("first hour")
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:00")
            .to_string();
        let expected_second_hour = Utc
            .with_ymd_and_hms(2026, 3, 20, 10, 0, 0)
            .single()
            .expect("second hour")
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:00")
            .to_string();

        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].hour, expected_first_hour);
        assert_eq!(hourly[0].estimated_tokens_saved, 400);
        assert_eq!(hourly[1].hour, expected_second_hour);
        assert_eq!(hourly[1].estimated_tokens_saved, 600);
        assert_eq!(hourly[1].total_tokens_sent, 1_800);
    }

    #[test]
    fn observing_repairs_stale_current_session_hourly_overlay() {
        let mut tracker = make_tracker();
        tracker.last_observation = Some(SavingsObservation {
            observed_at: Utc::now(),
            last_activity_at: Some(Utc::now()),
            session_requests: 10,
            session_estimated_savings_usd: 10.0,
            session_estimated_tokens_saved: 10_000,
            session_actual_cost_usd: 1.0,
            session_total_tokens_sent: 5_000,
        });
        tracker.session_hourly_buckets.insert(
            "2026-03-24T13:00".into(),
            DailySavingsBucket {
                estimated_savings_usd: 20.0,
                estimated_tokens_saved: 6_000_000,
                actual_cost_usd: 0.01,
                total_tokens_sent: 600_000,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        tracker.hourly_savings.insert(
            "2026-03-24T13:00".into(),
            DailySavingsBucket {
                estimated_savings_usd: 20.0,
                estimated_tokens_saved: 6_000_000,
                actual_cost_usd: 0.01,
                total_tokens_sent: 600_000,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );
        tracker.daily_savings.insert(
            "2026-03-24".into(),
            DailySavingsBucket {
                estimated_savings_usd: 20.0,
                estimated_tokens_saved: 6_000_000,
                actual_cost_usd: 0.01,
                total_tokens_sent: 600_000,
                output_savings_usd: 0.0,
                output_tokens_saved: 0,
                ..Default::default()
            },
        );

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(11),
                session_estimated_savings_usd: Some(10.1),
                session_estimated_tokens_saved: Some(10_200),
                session_savings_pct: Some(50.0),
                session_actual_cost_usd: Some(1.01),
                session_total_tokens_sent: Some(5_100),
                savings_history: vec![
                    history_point_at(2026, 3, 24, 11, 0),
                    history_point_at(2026, 3, 24, 12, 10_200),
                ],
            })
            .expect("snapshot");

        let hourly = tracker.hourly_savings();
        assert_eq!(hourly.len(), 1);
        assert_eq!(hourly[0].estimated_tokens_saved, 10_200);
        assert!((hourly[0].estimated_savings_usd - 10.1).abs() < 1e-9);
    }

    #[test]
    fn persisted_session_history_prevents_rolling_window_from_reassigning_older_hour() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(2),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 8, 0),
                    history_point_at(2026, 3, 20, 9, 400),
                    history_point_at(2026, 3, 20, 10, 1_000),
                ],
            })
            .expect("first snapshot");

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(3),
                session_estimated_savings_usd: Some(0.6),
                session_estimated_tokens_saved: Some(1_200),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.36),
                session_total_tokens_sent: Some(3_600),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 10, 1_000),
                    history_point_at(2026, 3, 20, 10, 1_200),
                ],
            })
            .expect("second snapshot");

        let hourly = tracker.hourly_savings();
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].estimated_tokens_saved, 400);
        assert_eq!(hourly[1].estimated_tokens_saved, 800);
    }

    #[test]
    fn single_visible_history_point_does_not_invent_hourly_attribution() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(1),
                session_estimated_savings_usd: Some(0.2),
                session_estimated_tokens_saved: Some(400),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.12),
                session_total_tokens_sent: Some(1_200),
                savings_history: vec![history_point_at(2026, 3, 20, 9, 400)],
            })
            .expect("snapshot");

        assert!(tracker.hourly_savings().is_empty());
        assert!(tracker.daily_savings().is_empty());
    }

    #[test]
    fn visible_hours_only_get_attributable_tokens_sent_and_spend() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(5),
                session_estimated_savings_usd: Some(10.0),
                session_estimated_tokens_saved: Some(10_000),
                session_savings_pct: Some(50.0),
                session_actual_cost_usd: Some(4.0),
                session_total_tokens_sent: Some(8_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 8, 7_000),
                    history_point_at(2026, 3, 20, 9, 8_000),
                    history_point_at(2026, 3, 20, 10, 10_000),
                ],
            })
            .expect("snapshot");

        let hourly = tracker.hourly_savings();
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].estimated_tokens_saved, 1_000);
        assert_eq!(hourly[1].estimated_tokens_saved, 2_000);
        assert_eq!(hourly[0].total_tokens_sent, 800);
        assert_eq!(hourly[1].total_tokens_sent, 1_600);
        assert!((hourly[0].actual_cost_usd - 0.4).abs() < 1e-9);
        assert!((hourly[1].actual_cost_usd - 0.8).abs() < 1e-9);
    }

    #[test]
    fn multi_poll_sampling_gives_real_per_hour_sent_not_the_smear() {
        // Three polls sample the cumulative new-input scalar over time. Once the
        // sampled deltas cover ~all session sent, each hour gets its true sent
        // (6000 / 4000) instead of the savings-proportional smear (~3333 / 6667).
        let mut tracker = make_tracker();

        for (requests, saved, sent, history) in [
            (
                1usize,
                0u64,
                0u64,
                vec![history_point_at(2026, 3, 20, 8, 0)],
            ),
            (
                2,
                1_000,
                6_000,
                vec![
                    history_point_at(2026, 3, 20, 8, 0),
                    history_point_at(2026, 3, 20, 9, 1_000),
                ],
            ),
            (
                3,
                3_000,
                10_000,
                vec![
                    history_point_at(2026, 3, 20, 8, 0),
                    history_point_at(2026, 3, 20, 9, 1_000),
                    history_point_at(2026, 3, 20, 10, 3_000),
                ],
            ),
        ] {
            tracker
                .observe(&HeadroomDashboardStats {
                    output_shaper_active: None,
                    reread_tokens: None,
                    reread_compressed_tokens: None,
                    ccr_retrievals: None,
                    learner_progress: None,
                    output_reduction: None,
                    tool_schema_tokens_saved: None,
                    session_requests: Some(requests),
                    session_estimated_savings_usd: Some(saved as f64 / 1000.0),
                    session_estimated_tokens_saved: Some(saved),
                    session_savings_pct: Some(30.0),
                    session_actual_cost_usd: Some(1.0),
                    session_total_tokens_sent: Some(sent),
                    savings_history: history,
                })
                .expect("snapshot");
        }

        let hourly = tracker.hourly_savings();
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].estimated_tokens_saved, 1_000);
        assert_eq!(hourly[0].total_tokens_sent, 6_000);
        assert_eq!(hourly[1].estimated_tokens_saved, 2_000);
        assert_eq!(hourly[1].total_tokens_sent, 4_000);
        // The same sampled values land in the new-input field: session
        // buckets are the only writer, and they ARE the new-input basis.
        assert_eq!(hourly[0].new_input_tokens, 6_000);
        assert_eq!(hourly[1].new_input_tokens, 4_000);
    }

    #[test]
    fn rolling_window_does_not_dump_unattributable_remainder_into_last_hour() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(5),
                session_estimated_savings_usd: Some(10.0),
                session_estimated_tokens_saved: Some(10_000),
                session_savings_pct: Some(50.0),
                session_actual_cost_usd: Some(4.0),
                session_total_tokens_sent: Some(8_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 8, 0),
                    history_point_at(2026, 3, 20, 9, 4_000),
                    history_point_at(2026, 3, 20, 10, 7_000),
                ],
            })
            .expect("first snapshot");

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(6),
                session_estimated_savings_usd: Some(10.0),
                session_estimated_tokens_saved: Some(10_000),
                session_savings_pct: Some(50.0),
                session_actual_cost_usd: Some(4.0),
                session_total_tokens_sent: Some(8_000),
                savings_history: vec![
                    history_point_at(2026, 3, 20, 10, 7_000),
                    history_point_at(2026, 3, 20, 11, 10_000),
                ],
            })
            .expect("second snapshot");

        let hourly = tracker.hourly_savings();
        assert_eq!(hourly.len(), 3);
        assert_eq!(hourly[2].estimated_tokens_saved, 3_000);
        assert_eq!(hourly[2].total_tokens_sent, 2_400);
        assert!((hourly[2].actual_cost_usd - 1.2).abs() < 1e-9);
    }

    #[test]
    fn missing_optional_spend_fields_do_not_trigger_session_reset() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(10),
                session_estimated_savings_usd: Some(1.0),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(20.0),
                session_actual_cost_usd: Some(4.0),
                session_total_tokens_sent: Some(4_000),
                savings_history: Vec::new(),
            })
            .expect("first snapshot");

        let second = tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(11),
                session_estimated_savings_usd: Some(1.2),
                session_estimated_tokens_saved: Some(1_200),
                session_savings_pct: Some(20.0),
                session_actual_cost_usd: None,
                session_total_tokens_sent: None,
                savings_history: Vec::new(),
            })
            .expect("second snapshot");

        assert_eq!(second.lifetime_requests, 11);
    }

    #[test]
    fn overnight_inactivity_rolls_only_the_display_session() {
        let mut tracker = make_tracker();
        let now = Utc::now();
        let prior_activity = (now - chrono::Duration::hours(2))
            .with_timezone(&Local)
            .date_naive()
            .pred_opt()
            .expect("prior day")
            .and_hms_opt(23, 0, 0)
            .expect("valid time")
            .and_local_timezone(Local)
            .single()
            .expect("local timestamp")
            .with_timezone(&Utc);

        tracker.last_observation = Some(SavingsObservation {
            observed_at: now - chrono::Duration::minutes(5),
            last_activity_at: Some(prior_activity),
            session_requests: 10,
            session_estimated_savings_usd: 5.0,
            session_estimated_tokens_saved: 1_000,
            session_actual_cost_usd: 2.0,
            session_total_tokens_sent: 4_000,
        });
        tracker.session_requests = 10;
        tracker.session_estimated_savings_usd = 5.0;
        tracker.session_estimated_tokens_saved = 1_000;
        tracker.session_savings_pct = 20.0;
        tracker.lifetime_requests = 10;

        let snapshot = tracker
            .observe(&HeadroomDashboardStats {
                output_shaper_active: None,
                reread_tokens: None,
                reread_compressed_tokens: None,
                ccr_retrievals: None,
                learner_progress: None,
                output_reduction: None,
                tool_schema_tokens_saved: None,
                session_requests: Some(11),
                session_estimated_savings_usd: Some(5.5),
                session_estimated_tokens_saved: Some(1_100),
                session_savings_pct: Some(21.57),
                session_actual_cost_usd: Some(2.4),
                session_total_tokens_sent: Some(4_400),
                savings_history: Vec::new(),
            })
            .expect("snapshot");

        assert_eq!(snapshot.session_requests, 1);
        assert_eq!(snapshot.session_estimated_tokens_saved, 100);
        assert!((snapshot.session_estimated_savings_usd - 0.5).abs() < 1e-9);
        assert!((snapshot.session_savings_pct - 20.0).abs() < 1e-9);
        assert_eq!(snapshot.lifetime_requests, 11);
    }

    #[test]
    fn launch_profile_load_or_create_survives_corrupt_file() {
        let base_dir = std::env::temp_dir().join(format!(
            "headroom-launch-profile-test-{}",
            uuid::Uuid::new_v4()
        ));
        ensure_data_dirs(&base_dir).expect("create temp dirs");
        let path = crate::storage::config_file(&base_dir, "launch-profile.json");
        std::fs::write(&path, "").expect("write empty profile"); // RUST-1P: 0-byte file

        let (profile, _) = super::LaunchProfile::load_or_create(&base_dir)
            .expect("must not fail on corrupt profile");
        assert_eq!(profile.launch_count, 1);
        assert!(matches!(
            profile.launch_experience,
            crate::models::LaunchExperience::FirstRun
        ));
        // The rewritten file parses again on the next launch.
        let (profile, _) = super::LaunchProfile::load_or_create(&base_dir).expect("reload");
        assert_eq!(profile.launch_count, 2);
        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn load_or_create_ignores_old_persisted_snapshot_schema() {
        let base_dir = std::env::temp_dir().join(format!(
            "headroom-savings-state-test-{}",
            uuid::Uuid::new_v4()
        ));
        ensure_data_dirs(&base_dir).expect("create temp dirs");

        std::fs::write(telemetry_file(&base_dir, "savings-records.jsonl"), "")
            .expect("write empty journal");
        let persisted = PersistedSavingsState {
            schema_version: 1,
            session_requests: 5,
            session_estimated_savings_usd: 0.9,
            session_estimated_tokens_saved: 900,
            session_savings_pct: 18.0,
            lifetime_requests: 12,
            lifetime_token_milestone_high_water: None,
            lifetime_tool_schema_tokens_saved: 0,
            tool_schema_daily_samples: std::collections::BTreeMap::new(),
            tool_schema_hourly_samples: std::collections::BTreeMap::new(),
            last_observation: Some(SavingsObservation {
                observed_at: Utc::now(),
                last_activity_at: Some(Utc::now()),
                session_requests: 5,
                session_estimated_savings_usd: 0.9,
                session_estimated_tokens_saved: 900,
                session_actual_cost_usd: 0.0,
                session_total_tokens_sent: 0,
            }),
            display_session_baseline: None,
            session_savings_history: Vec::new(),
            session_new_input_history: Vec::new(),
            session_hourly_buckets: std::collections::BTreeMap::new(),
            daily_savings: std::collections::BTreeMap::new(),
            hourly_savings: std::collections::BTreeMap::new(),
            output_daily_samples: std::collections::BTreeMap::new(),
            output_hourly_samples: std::collections::BTreeMap::new(),
            last_output_estimator_tokens_saved: None,
            last_output_estimator_baseline_tokens: None,
            output_sample_series_version: OUTPUT_SAMPLE_SERIES_VERSION,
        };
        std::fs::write(
            config_file(&base_dir, "savings-state.json"),
            serde_json::to_vec_pretty(&persisted).expect("serialize persisted state"),
        )
        .expect("write persisted state");

        let tracker = SavingsTracker::load_or_create(&base_dir).expect("load tracker");
        assert_eq!(tracker.lifetime_token_milestone_high_water, 0);
        assert_eq!(tracker.lifetime_requests, 0);

        let _ = std::fs::remove_dir_all(base_dir);
    }

    fn daily(date: &str, tokens: u64, usd: f64) -> DailySavingsPoint {
        DailySavingsPoint {
            date: date.to_string(),
            estimated_tokens_saved: tokens,
            tool_schema_savings_usd: 0.0,
            tool_schema_tokens_saved: 0,
            estimated_savings_usd: usd,
            actual_cost_usd: 0.0,
            total_tokens_sent: 0,
            new_input_tokens: 0,
            output_savings_usd: 0.0,
            output_tokens_saved: 0,
            cache_read_tokens: None,
            cache_savings_usd: None,
            output_sampled_tokens_saved: None,
            output_baseline_tokens: None,
        }
    }

    fn hourly(hour: &str, tokens: u64) -> HourlySavingsPoint {
        HourlySavingsPoint {
            hour: hour.to_string(),
            estimated_tokens_saved: tokens,
            tool_schema_savings_usd: 0.0,
            tool_schema_tokens_saved: 0,
            estimated_savings_usd: 0.0,
            actual_cost_usd: 0.0,
            total_tokens_sent: 0,
            new_input_tokens: 0,
            by_provider: Vec::new(),
            output_savings_usd: 0.0,
            output_tokens_saved: 0,
            cache_read_tokens: None,
            cache_savings_usd: None,
            output_sampled_tokens_saved: None,
            output_baseline_tokens: None,
        }
    }

    #[test]
    fn ingest_native_rollups_writes_settled_days_only_and_is_idempotent() {
        let mut tracker = make_tracker();
        let cutoff = "2026-06-02";
        let today = "2026-06-16";
        let native_daily = vec![
            daily("2026-06-01", 999, 9.99), // pre-cutoff -> skipped
            daily("2026-06-10", 100, 1.0),  // settled -> ingested
            daily("2026-06-16", 500, 5.0),  // live UTC day -> archived as it grows
        ];
        let native_hourly = vec![
            hourly("2026-06-10T09:00", 40), // settled day -> ingested
            hourly("2026-06-16T09:00", 60), // today -> skipped
        ];

        assert!(tracker.ingest_native_rollups(&native_daily, &native_hourly, cutoff, today, today));

        let daily_dates: Vec<String> = tracker
            .daily_savings()
            .into_iter()
            .map(|p| p.date)
            .collect();
        assert_eq!(daily_dates, vec!["2026-06-10", "2026-06-16"]);
        let hourly_keys: Vec<String> = tracker
            .hourly_savings()
            .into_iter()
            .map(|p| p.hour)
            .collect();
        assert_eq!(hourly_keys, vec!["2026-06-10T09:00"]);

        // Re-ingesting identical data must not report a change (no needless persist).
        assert!(!tracker.ingest_native_rollups(
            &native_daily,
            &native_hourly,
            cutoff,
            today,
            today
        ));
    }

    #[test]
    fn ingest_native_rollups_archives_the_live_day_but_never_shrinks_it() {
        // The backend's history buffer can hold under 24h at heavy volume, so a
        // day that is only archived once it settles is already gone: it has
        // become the buffer's backfill bucket and drop_rollup_backfill removes
        // it. Archiving the live day as it accumulates is what makes yesterday
        // survive the wrap (2026-08-13 otherwise fell to $3.44).
        let mut tracker = make_tracker();
        let cutoff = "2026-06-02";
        let today = "2026-06-16";

        let mut live = daily("2026-06-16", 500, 5.0);
        live.total_tokens_sent = 10_000;
        assert!(tracker.ingest_native_rollups(&[live.clone()], &[], cutoff, today, today));

        // The tracker keys its own deltas by LOCAL day, so ahead of UTC it can
        // already hold hours this UTC bucket has not reached. A snapshot with
        // less spend must not replace what is archived.
        let mut smaller = daily("2026-06-16", 1, 0.01);
        smaller.total_tokens_sent = 9_000;
        assert!(!tracker.ingest_native_rollups(&[smaller], &[], cutoff, today, today));

        let point = tracker
            .daily_savings()
            .into_iter()
            .find(|p| p.date == "2026-06-16")
            .expect("live day archived");
        assert_eq!(point.estimated_tokens_saved, 500);

        // Grown since the last render -> take the newer value.
        let mut grown = daily("2026-06-16", 900, 9.0);
        grown.total_tokens_sent = 20_000;
        assert!(tracker.ingest_native_rollups(&[grown], &[], cutoff, today, today));
        let point = tracker
            .daily_savings()
            .into_iter()
            .find(|p| p.date == "2026-06-16")
            .expect("live day archived");
        assert_eq!(point.estimated_tokens_saved, 900);

        // Next day the backend drops it as the backfill bucket; the archive
        // still has the full-day value, so the merge no longer falls back to
        // the tracker's own partial observation.
        assert!(!tracker.ingest_native_rollups(&[], &[], cutoff, "2026-06-17", "2026-06-17"));
        let merged = merge_daily_savings(tracker.daily_savings(), vec![], cutoff);
        let day = merged
            .iter()
            .find(|p| p.date == "2026-06-16")
            .expect("archived day survives");
        assert_eq!(day.estimated_tokens_saved, 900);
    }

    #[test]
    fn ingest_native_rollups_keeps_archived_cache_coverage_when_checkpoints_age_out() {
        let mut tracker = make_tracker();
        let cutoff = "2026-06-02";

        // While the day's checkpoints are still inside the backend's history
        // ring, the derived cache deltas arrive and are archived.
        let mut covered = daily("2026-06-10", 100, 1.0);
        covered.total_tokens_sent = 10_000;
        covered.cache_read_tokens = Some(5_000);
        covered.cache_savings_usd = Some(0.9);
        let mut covered_hour = hourly("2026-06-10T09:00", 40);
        covered_hour.cache_read_tokens = Some(2_000);
        covered_hour.cache_savings_usd = Some(0.4);
        assert!(tracker.ingest_native_rollups(
            &[covered],
            &[covered_hour],
            cutoff,
            "2026-06-16",
            "2026-06-16"
        ));

        // Later the checkpoints age out of the ring and the same buckets
        // re-derive as None. The archived coverage must survive, and an
        // otherwise-identical snapshot must not report a change.
        let mut uncovered = daily("2026-06-10", 100, 1.0);
        uncovered.total_tokens_sent = 10_000;
        assert!(!tracker.ingest_native_rollups(
            &[uncovered],
            &[hourly("2026-06-10T09:00", 40)],
            cutoff,
            "2026-06-16",
            "2026-06-16"
        ));

        // While trimming eats the day, re-derivations still return Some but
        // with shrinking coverage. Settled buckets are frozen at the first
        // archived value, so the worse re-derivation must not overwrite.
        let mut partial = daily("2026-06-10", 100, 1.0);
        partial.total_tokens_sent = 10_000;
        partial.cache_read_tokens = Some(1_000);
        partial.cache_savings_usd = Some(0.2);
        let mut partial_hour = hourly("2026-06-10T09:00", 40);
        partial_hour.cache_read_tokens = Some(500);
        partial_hour.cache_savings_usd = Some(0.1);
        assert!(!tracker.ingest_native_rollups(
            &[partial],
            &[partial_hour],
            cutoff,
            "2026-06-16",
            "2026-06-16"
        ));

        let day = tracker
            .daily_savings()
            .into_iter()
            .find(|p| p.date == "2026-06-10")
            .expect("archived day");
        assert_eq!(day.cache_read_tokens, Some(5_000));
        assert_eq!(day.cache_savings_usd, Some(0.9));
        let hour = tracker
            .hourly_savings()
            .into_iter()
            .find(|p| p.hour == "2026-06-10T09:00")
            .expect("archived hour");
        assert_eq!(hour.cache_read_tokens, Some(2_000));
        assert_eq!(hour.cache_savings_usd, Some(0.4));

        // The live UTC day is the opposite: its derivation grows with the
        // day, so the fresh value wins over the previously archived one.
        let mut live = daily("2026-06-16", 10, 0.1);
        live.total_tokens_sent = 1_000;
        live.cache_read_tokens = Some(100);
        live.cache_savings_usd = Some(0.01);
        assert!(tracker.ingest_native_rollups(&[live], &[], cutoff, "2026-06-16", "2026-06-16"));
        let mut live_grown = daily("2026-06-16", 20, 0.2);
        live_grown.total_tokens_sent = 2_000;
        live_grown.cache_read_tokens = Some(300);
        live_grown.cache_savings_usd = Some(0.03);
        assert!(tracker.ingest_native_rollups(
            &[live_grown],
            &[],
            cutoff,
            "2026-06-16",
            "2026-06-16"
        ));
        let today = tracker
            .daily_savings()
            .into_iter()
            .find(|p| p.date == "2026-06-16")
            .expect("live day archived");
        assert_eq!(today.cache_read_tokens, Some(300));
        assert_eq!(today.cache_savings_usd, Some(0.03));
    }

    #[test]
    fn ingest_native_rollups_overwrites_stale_tracker_value_with_authoritative() {
        let mut tracker = make_tracker();
        // A prior, approximate self-observed value for a settled day.
        assert!(tracker.ingest_native_rollups(
            &[daily("2026-06-10", 50, 0.5)],
            &[],
            "2026-06-02",
            "2026-06-16",
            "2026-06-16",
        ));
        // Backend reports the authoritative (different) value -> overwrite + change.
        assert!(tracker.ingest_native_rollups(
            &[daily("2026-06-10", 100, 1.0)],
            &[],
            "2026-06-02",
            "2026-06-16",
            "2026-06-16",
        ));
        let point = tracker
            .daily_savings()
            .into_iter()
            .find(|p| p.date == "2026-06-10")
            .expect("settled day present");
        assert_eq!(point.estimated_tokens_saved, 100);
    }

    #[test]
    fn ingest_native_rollups_keeps_local_spend_when_backend_rollup_desynced() {
        // RUST-4S: a backend rollup that reports savings but zero tokens/cost
        // must not overwrite a settled local bucket that recorded real spend,
        // or the zero-spend anomaly probe fires on a false positive.
        let mut tracker = make_tracker();
        tracker.add_daily_delta("2026-06-10", 1.0, 100, 2.5, 5000, 0);

        // Desynced backend point for the same day: savings, no spend.
        let desynced = tracker.ingest_native_rollups(
            &[daily("2026-06-10", 200, 3.0)],
            &[],
            "2026-06-02",
            "2026-06-16",
            "2026-06-16",
        );
        assert!(!desynced, "desynced rollup must be skipped, not applied");

        let point = tracker
            .daily_savings()
            .into_iter()
            .find(|p| p.date == "2026-06-10")
            .expect("local spend day preserved");
        assert_eq!(point.total_tokens_sent, 5000);
        assert_eq!(point.actual_cost_usd, 2.5);
    }

    #[test]
    fn ingest_native_rollups_leaves_days_absent_from_native_untouched() {
        // Guards the integrity property: once the trimmed boundary bucket is
        // dropped by the parser it is absent from `native`, so ingestion must
        // never clobber the good value archived on the prior render.
        let mut tracker = make_tracker();
        assert!(tracker.ingest_native_rollups(
            &[daily("2026-06-10", 100, 1.0)],
            &[],
            "2026-06-02",
            "2026-06-16",
            "2026-06-16",
        ));
        // Next render: June 10 is now the dropped boundary (absent); only the
        // newer settled day arrives.
        assert!(tracker.ingest_native_rollups(
            &[daily("2026-06-11", 70, 0.7)],
            &[],
            "2026-06-02",
            "2026-06-16",
            "2026-06-16",
        ));
        let by_date: std::collections::BTreeMap<String, u64> = tracker
            .daily_savings()
            .into_iter()
            .map(|p| (p.date, p.estimated_tokens_saved))
            .collect();
        assert_eq!(by_date.get("2026-06-10"), Some(&100)); // preserved
        assert_eq!(by_date.get("2026-06-11"), Some(&70)); // newly archived
    }

    // merge_daily_savings

    #[test]
    fn merge_daily_tracker_preferred_before_cutoff() {
        let tracker = vec![daily("2026-04-13", 500, 1.0)];
        let history = vec![daily("2026-04-13", 999, 2.0)];
        let result = merge_daily_savings(tracker, history, "2026-04-20");
        assert_eq!(result.len(), 1);
        // tracker wins pre-cutoff
        assert_eq!(result[0].estimated_tokens_saved, 500);
    }

    #[test]
    fn merge_daily_history_preferred_on_and_after_cutoff() {
        let tracker = vec![daily("2026-04-20", 100, 0.5)];
        let history = vec![daily("2026-04-20", 800, 2.0)];
        let result = merge_daily_savings(tracker, history, "2026-04-20");
        assert_eq!(result.len(), 1);
        // history wins on cutoff date
        assert_eq!(result[0].estimated_tokens_saved, 800);
    }

    #[test]
    fn merge_daily_keeps_local_new_input_when_history_wins() {
        // Backend rollups carry no new-input dimension. If history winning the
        // bucket dropped the locally-sampled value, every poll would wipe the
        // new-input rate's coverage for the day the user is looking at.
        let tracker = vec![DailySavingsPoint {
            new_input_tokens: 4_000,
            ..daily("2026-04-20", 100, 0.5)
        }];
        let history = vec![daily("2026-04-20", 800, 2.0)];
        let result = merge_daily_savings(tracker, history, "2026-04-20");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].estimated_tokens_saved, 800);
        assert_eq!(result[0].new_input_tokens, 4_000);
    }

    #[test]
    fn merge_daily_prefers_tracker_when_history_has_savings_but_zero_spend() {
        // Backend rollup desync: savings recorded, tokens/cost zero (RUST-3S/3V).
        let history = vec![daily("2026-04-21", 800, 2.0)];
        let tracker = vec![DailySavingsPoint {
            date: "2026-04-21".to_string(),
            estimated_tokens_saved: 400,
            tool_schema_savings_usd: 0.0,
            tool_schema_tokens_saved: 0,
            estimated_savings_usd: 1.5,
            actual_cost_usd: 9.0,
            total_tokens_sent: 123_456,
            new_input_tokens: 0,
            output_savings_usd: 0.0,
            output_tokens_saved: 0,
            cache_read_tokens: None,
            cache_savings_usd: None,
            output_sampled_tokens_saved: None,
            output_baseline_tokens: None,
        }];
        let result = merge_daily_savings(tracker, history, "2026-04-20");
        assert_eq!(result.len(), 1);
        // Tracker point (with real spend) wins over the desynced history point.
        assert_eq!(result[0].total_tokens_sent, 123_456);
        assert_eq!(result[0].actual_cost_usd, 9.0);
    }

    #[test]
    fn merge_daily_keeps_history_when_zero_spend_but_tracker_also_lacks_spend() {
        // No real spend anywhere -> nothing to fall back to; history is kept as-is.
        let history = vec![daily("2026-04-21", 800, 2.0)];
        let tracker = vec![daily("2026-04-21", 100, 0.5)];
        let result = merge_daily_savings(tracker, history, "2026-04-20");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].estimated_tokens_saved, 800);
        assert_eq!(result[0].total_tokens_sent, 0);
    }

    #[test]
    fn merge_daily_fallback_when_only_tracker_has_post_cutoff_day() {
        let tracker = vec![daily("2026-04-21", 300, 1.2)];
        let result = merge_daily_savings(tracker, vec![], "2026-04-20");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].estimated_tokens_saved, 300);
    }

    #[test]
    fn merge_daily_drops_history_pre_cutoff() {
        // Pre-cutoff is tracker-only: empty tracker + pre-cutoff history => no entry.
        // This protects against pre-v6 schema drift leaking into the graph.
        let history = vec![daily("2026-04-10", 400, 1.5)];
        let result = merge_daily_savings(vec![], history, "2026-04-20");
        assert!(result.is_empty());
    }

    #[test]
    fn merge_daily_combines_days_from_both_sources() {
        let tracker = vec![daily("2026-04-10", 200, 0.8), daily("2026-04-13", 300, 1.0)];
        let history = vec![daily("2026-04-20", 500, 2.0), daily("2026-04-21", 600, 2.5)];
        let mut result = merge_daily_savings(tracker, history, "2026-04-20");
        result.sort_by(|a, b| a.date.cmp(&b.date));
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].date, "2026-04-10");
        assert_eq!(result[3].date, "2026-04-21");
    }

    // merge_hourly_savings

    #[test]
    fn merge_hourly_tracker_preferred_before_cutoff() {
        let tracker = vec![hourly("2026-04-13T10:00", 500)];
        let history = vec![hourly("2026-04-13T10:00", 999)];
        let result = merge_hourly_savings(tracker, history, "2026-04-20T00:00");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].estimated_tokens_saved, 500);
    }

    #[test]
    fn merge_hourly_history_preferred_on_and_after_cutoff() {
        let tracker = vec![hourly("2026-04-20T09:00", 100)];
        let history = vec![hourly("2026-04-20T09:00", 800)];
        let result = merge_hourly_savings(tracker, history, "2026-04-20T00:00");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].estimated_tokens_saved, 800);
    }

    #[test]
    fn merge_hourly_drops_history_pre_cutoff() {
        // Pre-cutoff is tracker-only: empty tracker + pre-cutoff history => no entries.
        let tracker: Vec<HourlySavingsPoint> = vec![];
        let history = vec![
            hourly("2026-04-13T09:00", 400),
            hourly("2026-04-13T10:00", 600),
        ];
        let result = merge_hourly_savings(tracker, history, "2026-04-20T00:00");
        assert!(result.is_empty());
    }

    #[test]
    fn tracker_observe_called_updates_hourly_savings_even_with_history_present() {
        // Regression: tracker.observe() must be called regardless of whether native
        // history is available, so that hourly buckets stay current.
        let today = chrono::Local::now();
        let hp = |hour: u32, total: u64| -> HeadroomSavingsHistoryPoint {
            history_point_at(today.year(), today.month(), today.day(), hour, total)
        };
        let mut tracker = make_tracker();

        // First observation: 1_000 tokens saved, history shows 0→1_000 across hours 9→10.
        tracker.observe(&HeadroomDashboardStats {
            output_shaper_active: None,
            reread_tokens: None,
            reread_compressed_tokens: None,
            ccr_retrievals: None,
            learner_progress: None,
            output_reduction: None,
            tool_schema_tokens_saved: None,
            session_requests: Some(1),
            session_estimated_savings_usd: Some(1.0),
            session_estimated_tokens_saved: Some(1_000),
            session_savings_pct: Some(30.0),
            session_actual_cost_usd: Some(0.5),
            session_total_tokens_sent: Some(3_000),
            savings_history: vec![hp(9, 0), hp(10, 1_000)],
        });
        let total_first: u64 = tracker
            .hourly_savings()
            .iter()
            .map(|p| p.estimated_tokens_saved)
            .sum();

        // Second observation: 3_000 tokens saved, history adds hour 11.
        tracker.observe(&HeadroomDashboardStats {
            output_shaper_active: None,
            reread_tokens: None,
            reread_compressed_tokens: None,
            ccr_retrievals: None,
            learner_progress: None,
            output_reduction: None,
            tool_schema_tokens_saved: None,
            session_requests: Some(3),
            session_estimated_savings_usd: Some(3.0),
            session_estimated_tokens_saved: Some(3_000),
            session_savings_pct: Some(30.0),
            session_actual_cost_usd: Some(1.5),
            session_total_tokens_sent: Some(9_000),
            savings_history: vec![hp(9, 0), hp(10, 1_000), hp(11, 3_000)],
        });
        let total_second: u64 = tracker
            .hourly_savings()
            .iter()
            .map(|p| p.estimated_tokens_saved)
            .sum();

        assert!(
            total_second > total_first,
            "hourly savings should grow with each observe call: first={total_first} second={total_second}"
        );
    }

    fn idle_progress() -> BootstrapProgress {
        BootstrapProgress {
            running: false,
            complete: false,
            failed: false,
            current_step: String::new(),
            message: String::new(),
            current_step_eta_seconds: 0,
            overall_percent: 0,
        }
    }

    #[test]
    fn begin_bootstrap_skips_install_when_python_already_installed() {
        let (next, result) = begin_bootstrap_transition(&idle_progress(), true);
        assert!(result.is_ok());
        assert!(next.complete);
        assert!(!next.running);
        assert!(!next.failed);
        assert_eq!(next.overall_percent, 100);
    }

    #[test]
    fn begin_bootstrap_starts_when_python_missing() {
        let (next, result) = begin_bootstrap_transition(&idle_progress(), false);
        assert!(result.is_ok());
        assert!(next.running);
        assert!(!next.complete);
        assert!(!next.failed);
        assert_eq!(next.overall_percent, 2);
    }

    #[test]
    fn begin_bootstrap_rejects_reentry_while_running() {
        let running = BootstrapProgress {
            running: true,
            overall_percent: 42,
            ..idle_progress()
        };
        let (next, result) = begin_bootstrap_transition(&running, false);
        assert!(result.is_err());
        // State is preserved when re-entry is rejected.
        assert_eq!(next.overall_percent, 42);
        assert!(next.running);
    }

    #[test]
    fn begin_bootstrap_after_failure_restarts_cleanly() {
        let failed = BootstrapProgress {
            failed: true,
            overall_percent: 50,
            message: "boom".into(),
            ..idle_progress()
        };
        let (next, result) = begin_bootstrap_transition(&failed, false);
        assert!(result.is_ok());
        assert!(next.running);
        assert!(!next.failed);
        assert_eq!(next.overall_percent, 2);
    }

    #[test]
    fn apply_step_normalizes_into_running_state() {
        let failed = BootstrapProgress {
            failed: true,
            ..idle_progress()
        };
        let next = apply_bootstrap_step(
            &failed,
            BootstrapStepUpdate {
                step: "Downloading Python",
                message: "Fetching runtime".into(),
                eta_seconds: 30,
                percent: 40,
            },
        );
        assert!(next.running);
        assert!(!next.failed);
        assert!(!next.complete);
        assert_eq!(next.current_step, "Downloading Python");
        assert_eq!(next.overall_percent, 40);
        assert_eq!(next.current_step_eta_seconds, 30);
    }

    #[test]
    fn complete_state_pins_to_full_progress() {
        let next = bootstrap_complete_state();
        assert!(next.complete);
        assert!(!next.running);
        assert!(!next.failed);
        assert_eq!(next.overall_percent, 100);
    }

    #[test]
    fn failed_state_preserves_current_percent_with_min_of_one() {
        let current = BootstrapProgress {
            running: true,
            overall_percent: 72,
            ..idle_progress()
        };
        let next = bootstrap_failed_state(&current, "download error".into());
        assert!(next.failed);
        assert!(!next.running);
        assert!(!next.complete);
        assert_eq!(next.overall_percent, 72);
        assert_eq!(next.message, "download error");
    }

    #[test]
    fn failed_state_floors_zero_percent_to_one() {
        let next = bootstrap_failed_state(&idle_progress(), "early failure".into());
        assert_eq!(next.overall_percent, 1);
        assert!(next.failed);
    }

    #[test]
    fn support_tier_for_platform_marks_windows_experimental() {
        assert_eq!(support_tier_for_platform("linux"), "experimental");
        assert_eq!(support_tier_for_platform("windows"), "experimental");
        assert_eq!(support_tier_for_platform("macos"), "stable");
    }
}
