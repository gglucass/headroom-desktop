mod activity_facts;
mod analytics;
mod backend_port;
mod bearer;
mod claude_cli;
mod client_adapters;
mod device;
mod insights;
mod keychain;
mod logging;
mod memory_scrubber;
mod models;
mod output_savings;
mod port_conflict;
mod pricing;
mod proc;
mod proxy_intercept;
mod savings_canary;
mod state;
mod storage;
mod tool_manager;
mod upstream_override;
mod usage_counters;

/// Cross-module lock for tests that repoint $HOME / $CODEX_HOME. Env vars are
/// process-global, so home-mutating tests in different modules (client_adapters
/// TestHome, tool_manager HomeGuard) must serialize on one shared lock or a
/// guard drop can restore the real HOME mid-test and leak writes into the
/// developer's real agent configs.
#[cfg(test)]
pub(crate) mod test_env_lock {
    pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Hold this for the whole time `$HOME` is swapped. Tests that only READ
    /// the home dir need it too: `logging::scrub_home` and everything behind
    /// `dirs::home_dir()` resolve it at call time, so an unlocked swapper in
    /// another module makes their assertion silently vacuous -- and
    /// `device.rs` removes `$HOME` outright, which turns the scrub into a
    /// no-op and fails the test outright.
    pub(crate) fn lock_home() -> std::sync::MutexGuard<'static, ()> {
        HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use chrono::{Local, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;
use tauri::{
    AppHandle, PhysicalPosition, PhysicalSize, Position, Rect, State, Window, WindowEvent,
};
use tauri::{Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::models::{
    ActivityFeedResponse, BillingPeriod, BootstrapFailureReport, BootstrapProgress,
    ClaudeAccountProfile, ClaudeCodeProject, ClaudeUsage, ClientConnectorStatus, ClientSetupResult,
    ClientSetupVerification, DailySavingsPoint, DashboardState, HeadroomAuthCodeRequest,
    HeadroomLearnPrereqStatus, HeadroomLearnStatus, HeadroomPricingStatus,
    HeadroomSubscriptionTier, RuntimeStatus, RuntimeUpgradeProgress, TransformationFeedResponse,
};
use crate::state::AppState;

const UPDATER_PUBLIC_KEY: Option<&str> = option_env!("HEADROOM_UPDATER_PUBLIC_KEY");
const UPDATER_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_ENDPOINTS");
const UPDATER_STAGING_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_STAGING_ENDPOINTS");
const SENTRY_DSN: Option<&str> = option_env!("HEADROOM_SENTRY_DSN");
const DEFAULT_UPDATER_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk3QkUyNEU0MjVBMkRDM0MKUldRODNLSWw1Q1MrbC93MitlYTVoUXViSXJQNGVQWDdBRXA0Qkl4WGtpSEttNm5YTDB3QWtncEoK";
const DEFAULT_UPDATER_ENDPOINT: &str =
    "https://github.com/gglucass/headroom-desktop/releases/latest/download/latest.json";
/// Cadence of the background liveness ping. Long enough to be negligible
/// backend load (4 calls/day/user), short enough that admin can tell a
/// running-but-idle app from a quit one within half a day.
const LIVENESS_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
const BETA_CHANNEL_ENV: &str = "HEADROOM_BETA_CHANNEL";
const BETA_CHANNEL_SENTINEL: &str = "beta_channel";
const AUTOSTART_LAUNCH_ARG: &str = "--autostart";
/// Headless revert of Headroom's edits to other tools, for package managers that
/// can run a command before deleting the bundle. See `handle_uninstall_flag`.
const UNINSTALL_LAUNCH_ARG: &str = "--uninstall";
const HEADROOM_DASHBOARD_URL: &str = "http://127.0.0.1:6767/dashboard";
const MAIN_WINDOW_WIDTH: u32 = 760;
const MAIN_WINDOW_HEIGHT: u32 = 560;
/// Extra window height for the platforms that render the preview-build notice
/// (two wrapped 11px/1.4 lines plus the banner's gap) and reserve real layout
/// width for their scrollbars, which wraps text elsewhere too. Applied to the
/// main window and the launcher: both are fixed-size and both size their copy
/// off 100vh, so the same wider-font/narrower-viewport reflow overflows them.
#[cfg(not(target_os = "macos"))]
const PREVIEW_NOTICE_EXTRA_HEIGHT: u32 = 72;
/// Mirrors the `launcher` entry in tauri.conf.json.
#[cfg(not(target_os = "macos"))]
const LAUNCHER_WINDOW_WIDTH: u32 = 760;
#[cfg(not(target_os = "macos"))]
const LAUNCHER_WINDOW_HEIGHT: u32 = 540;
const TRAY_WINDOW_VERTICAL_GAP: i32 = 10;
const MAIN_WINDOW_BLUR_HIDE_DELAY_MS: u64 = 150;

type InstallPendingUpdateFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuitSource {
    SettingsButton,
    TrayMenu,
}

impl QuitSource {
    fn label(self) -> &'static str {
        match self {
            Self::SettingsButton => "settings_button",
            Self::TrayMenu => "tray_menu",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "phase")]
enum AppUpdateProgress {
    #[serde(rename = "downloading")]
    Downloading { downloaded: u64, total: Option<u64> },
    #[serde(rename = "installing")]
    Installing,
}

const APP_UPDATE_PROGRESS_EVENT: &str = "app-update://progress";

type AppUpdateProgressEmitter = Arc<dyn Fn(AppUpdateProgress) + Send + Sync + 'static>;

#[cfg(test)]
fn noop_app_update_progress_emitter() -> AppUpdateProgressEmitter {
    Arc::new(|_| {})
}

trait InstallableAppUpdate: Send {
    fn metadata(&self) -> AvailableAppUpdate;
    fn install(self, progress: AppUpdateProgressEmitter) -> InstallPendingUpdateFuture;
}

struct TauriPendingUpdate(Update);

impl InstallableAppUpdate for TauriPendingUpdate {
    fn metadata(&self) -> AvailableAppUpdate {
        let published_at = self.0.date.as_ref().and_then(|date| {
            date.format(&time::format_description::well_known::Rfc3339)
                .ok()
        });

        AvailableAppUpdate {
            current_version: self.0.current_version.clone(),
            version: self.0.version.clone(),
            published_at,
            notes: self.0.body.clone(),
        }
    }

    fn install(self, progress: AppUpdateProgressEmitter) -> InstallPendingUpdateFuture {
        Box::pin(async move {
            let downloaded = Arc::new(AtomicU64::new(0));
            let on_chunk_downloaded = Arc::clone(&downloaded);
            let on_chunk_progress = Arc::clone(&progress);
            let on_finish_progress = Arc::clone(&progress);
            self.0
                .download_and_install(
                    move |chunk_len, content_length| {
                        let total = on_chunk_downloaded
                            .fetch_add(chunk_len as u64, Ordering::Relaxed)
                            + chunk_len as u64;
                        on_chunk_progress(AppUpdateProgress::Downloading {
                            downloaded: total,
                            total: content_length,
                        });
                    },
                    move || {
                        on_finish_progress(AppUpdateProgress::Installing);
                    },
                )
                .await
                .map_err(|err| err.to_string())
        })
    }
}

struct PendingAppUpdate(Mutex<Option<TauriPendingUpdate>>);

#[derive(Debug, Clone)]
struct ReleaseUpdaterConfig {
    pubkey: String,
    endpoints: Vec<reqwest::Url>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct AppUpdateConfiguration {
    enabled: bool,
    current_version: String,
    endpoint_count: usize,
    configuration_error: Option<String>,
    beta_channel_enabled: bool,
    // macOS install() swaps the .app in place with no privilege prompt, so the
    // frontend may stage updates silently. Windows install() exits the app to
    // run the installer, and Linux .deb raises a polkit prompt - both must
    // stay behind an explicit user click.
    silent_install_supported: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct AvailableAppUpdate {
    current_version: String,
    version: String,
    published_at: Option<String>,
    notes: Option<String>,
}

static ZERO_SPEND_ALERT_FIRED: AtomicBool = AtomicBool::new(false);
static ZERO_SAVINGS_ALERT_FIRED: AtomicBool = AtomicBool::new(false);

// Once per process, like the intercept's `first_optimized_request` beacon:
// re-fires every launch until a send lands (server is first-write-wins), so
// one failed POST delays the funnel step instead of losing it forever.
static FIRST_SAVINGS_FUNNEL_REPORTED: AtomicBool = AtomicBool::new(false);

// Set when the watchdog has captured a Sentry event for the current "down
// episode". Reset whenever the proxy is observed reachable again, so a
// subsequent crash re-fires.
static WATCHDOG_DOWN_CAPTURED: AtomicBool = AtomicBool::new(false);

// Set after the first port-conflict start failure has been captured this
// session. Subsequent in-session port conflicts stay silent so the dashboard
// doesn't drown in the sleep/wake / kill -9 race noise.
static PORT_CONFLICT_CAPTURED: AtomicBool = AtomicBool::new(false);

// Same once-per-session shape as PORT_CONFLICT_CAPTURED, for a runtime the
// machine's security policy refuses to execute at all. See
// `capture_headroom_start_failure`.
static ENDPOINT_PROTECTION_CAPTURED: AtomicBool = AtomicBool::new(false);

// Guards the quit-time `clear_client_setups()` so it runs at most once per
// process. The exit handler fires for both `ExitRequested` and `Exit`, and a
// second `clear_client_setups()` call is destructive: its `disable_client_setup`
// loop wipes `remembered_clients` and then skips the snapshot re-save because
// `configured_clients` is already empty, leaving nothing for the next launch's
// `restore_client_setups()` to bring back.
static EXIT_CLEAR_DONE: AtomicBool = AtomicBool::new(false);

// Set at the start of every exit path (settings/tray quit, Cmd-Q / dock quit,
// restart_app) BEFORE stop_headroom runs. The proxy watchdog polls every 5s
// and restarts an unreachable backend; without this flag a probe that races
// exit teardown respawns the backend stop_headroom just killed, leaving an
// orphaned Python proxy holding the port against the next launch (observed:
// "watchdog: ... attempting restart" logged 1s into quit teardown).
// Checked at the watchdog loop top and inside ensure_headroom_running, the
// choke point every respawn path routes through.
pub(crate) static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

// Spend fields (actual_cost_usd, total_tokens_sent) were added to SavingsRecord in
// schema v6, shipped in 0.2.40 on 2026-04-13. Records written before that date
// deserialize those fields as 0 via #[serde(default)], producing false positives.
const SPEND_SCHEMA_CUTOFF_DATE: &str = "2026-04-13";

// Trigger on compression *dollar* savings, not the all-layers token total.
// `estimated_tokens_saved` folds in CLI context-tool filtering (RTK / lean-ctx),
// whose tokens are avoided before they ever reach a model request -- so they
// legitimately produce savings with zero tokens_sent and zero cost, tripping
// this probe on days dominated by that layer. `estimated_savings_usd` is
// proxy-compression-only (the proxy prices it at the model rate and excludes CLI
// filtering and prefix-cache discounts), so it is > 0 iff a real model request
// was compressed -- which implies tokens were sent and a cost incurred. Zero
// spend against it is the genuine pipeline anomaly.
// `min_date` (inclusive) bounds the scan to recent days. Historical days are
// immutable: a day written by a backend from before it reported spend fields
// keeps savings-with-zero-spend forever, and since ZERO_SPEND_ALERT_FIRED is
// per-process, alerting on the whole history re-fired the same June days on
// every app launch (Sentry RUST-3S/3V, 125 events). Only a recent day can be
// a live pipeline anomaly worth an alert. `max_date_exclusive` (exclusive)
// then drops the still-accumulating live day, whose cost/token counters lag
// its savings accumulator (Sentry RUST-4S) -- see the caller for how that
// boundary is chosen across the UTC/local day-key split.
fn zero_spend_affected_days<'a>(
    daily_savings: &'a [DailySavingsPoint],
    min_date: &str,
    max_date_exclusive: &str,
) -> Vec<&'a str> {
    // Only meaningful when the proxy actually reports spend. `total_tokens_sent`
    // and `actual_cost_usd` come from Option fields on /stats; a proxy build
    // that omits them lands every day at 0, indistinguishable from a real
    // "reported zero" (Sentry RUST-3S/3V). A compressed request always sends
    // tokens, so on a reporting proxy compression savings never coincide with
    // zero reported spend -- meaning if no day in the window reports any spend,
    // the user is simply on a non-reporting proxy and every zero is a reporting
    // gap, not an anomaly. Native-rollup ingestion overwrites settled days with
    // authoritative backend spend, so this self-heals once they upgrade.
    let proxy_reports_spend = daily_savings
        .iter()
        .any(|p| p.total_tokens_sent > 0 || p.actual_cost_usd > 0.0);
    if !proxy_reports_spend {
        return Vec::new();
    }
    daily_savings
        .iter()
        .filter(|p| {
            p.date.as_str() >= SPEND_SCHEMA_CUTOFF_DATE
                && p.date.as_str() >= min_date
                && p.date.as_str() < max_date_exclusive
                && p.estimated_savings_usd > 0.000_001
                && p.actual_cost_usd == 0.0
                && p.total_tokens_sent == 0
        })
        .map(|p| p.date.as_str())
        .collect()
}

// day -> first Instant it was observed desynced this process. The backend's
// cost counter transiently lags its savings accumulator (self-heals within a
// rollup), and a single-snapshot probe latched exactly that window on 11
// machines (Sentry RUST-4S). Healed days are pruned; only a day still
// desynced a full window later is a real pipeline anomaly.
static ZERO_SPEND_FIRST_SEEN: Mutex<std::collections::BTreeMap<String, std::time::Instant>> =
    Mutex::new(std::collections::BTreeMap::new());
const ZERO_SPEND_PERSIST_WINDOW: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Track desynced days across probe calls; true once any day has stayed
/// desynced for `window`.
fn persistent_zero_spend(
    first_seen: &mut std::collections::BTreeMap<String, std::time::Instant>,
    affected_days: &[&str],
    now: std::time::Instant,
    window: std::time::Duration,
) -> bool {
    first_seen.retain(|day, _| affected_days.contains(&day.as_str()));
    for day in affected_days {
        first_seen.entry((*day).to_string()).or_insert(now);
    }
    first_seen
        .values()
        .any(|&seen| now.duration_since(seen) >= window)
}

fn check_zero_spend_anomaly(dashboard: &DashboardState) {
    if ZERO_SPEND_ALERT_FIRED.load(Ordering::Relaxed) {
        return;
    }
    // Alert only on the most recent *settled* day. The live day's rollup is
    // mid-accumulation: the backend's cost/token counters flush a beat after
    // its savings accumulator, so today can show savings with zero spend past
    // the persist window during an idle stretch -- the RUST-4S false positive,
    // not a pipeline bug. A day is still live if it equals "today" in *either*
    // keying: backend daily rollups are UTC-day keyed, the local tracker's
    // buckets are local-day keyed, and merge_daily_savings folds both into one
    // date-keyed series. Take the earlier "today" as the settled boundary
    // (exclusive) so neither live bucket is scanned, and the day before it as
    // the lower bound so immutable history can't re-fire on launch (RUST-3S/3V).
    let settled_boundary = chrono::Local::now()
        .date_naive()
        .min(chrono::Utc::now().date_naive());
    let max_date_exclusive = settled_boundary.format("%Y-%m-%d").to_string();
    let min_date = (settled_boundary - chrono::Days::new(1))
        .format("%Y-%m-%d")
        .to_string();
    let affected_days =
        zero_spend_affected_days(&dashboard.daily_savings, &min_date, &max_date_exclusive);
    if !persistent_zero_spend(
        &mut ZERO_SPEND_FIRST_SEEN.lock(),
        &affected_days,
        std::time::Instant::now(),
        ZERO_SPEND_PERSIST_WINDOW,
    ) {
        return;
    }
    ZERO_SPEND_ALERT_FIRED.store(true, Ordering::Relaxed);
    // Fixed fingerprint: the per-user date list in the message split this
    // one probe across multiple Sentry issues (RUST-3S vs RUST-3V).
    sentry::with_scope(
        |scope| {
            scope.set_fingerprint(Some(&["zero-spend-anomaly"]));
            scope.set_extra("affected_days", affected_days.join(", ").into());
        },
        || {
            sentry::capture_message(
                "graph shows compression savings but zero tokens spent on recent day(s)",
                sentry::Level::Warning,
            );
        },
    );
}

/// Bug-shaped funnel drop: the backend has processed a meaningful number of
/// requests but lifetime savings never moved off zero — an optimizer or
/// config problem, not churn. One fingerprinted event; fires once per process
/// like the zero-spend probe above.
fn check_zero_savings_anomaly(dashboard: &DashboardState) {
    const MIN_LIFETIME_REQUESTS: usize = 25;
    if ZERO_SAVINGS_ALERT_FIRED.load(Ordering::Relaxed) {
        return;
    }
    if dashboard.lifetime_requests < MIN_LIFETIME_REQUESTS
        || dashboard.lifetime_estimated_tokens_saved > 0
    {
        return;
    }
    ZERO_SAVINGS_ALERT_FIRED.store(true, Ordering::Relaxed);
    sentry::with_scope(
        |scope| {
            scope.set_fingerprint(Some(&["zero-savings-anomaly"]));
            scope.set_extra("lifetime_requests", dashboard.lifetime_requests.into());
        },
        || {
            sentry::capture_message(
                "backend processed requests but lifetime savings is still zero",
                sentry::Level::Warning,
            );
        },
    );
}

/// Setup finished, but the backend has never processed a single request —
/// the classic cause is a terminal/editor still running with the pre-install
/// environment. One native notification, once ever (persisted flag), only on
/// a return launch, and only after ten minutes of uptime so a returning user
/// who codes right away moots it before it fires.
fn maybe_fire_onboarding_recovery_nudge(
    app: &AppHandle,
    state: &AppState,
    dashboard: &DashboardState,
) {
    // Test affordance: HEADROOM_FAKE_ONBOARDING_NUDGE=connected|disconnected
    // fires the nudge on the next dashboard poll with the chosen copy, skipping
    // the uptime, zero-traffic and return-launch gates. Fires once per process,
    // because Home polls the dashboard every 5 seconds and would otherwise turn
    // this into a notification storm, and deliberately does NOT consume the
    // persisted one-shot flag, so testing the copy never burns a real user's
    // only chance to see it.
    if let Some(mode) = fake_override("HEADROOM_FAKE_ONBOARDING_NUDGE") {
        static FORCED_NUDGE_FIRED: AtomicBool = AtomicBool::new(false);
        if FORCED_NUDGE_FIRED.swap(true, Ordering::AcqRel) {
            return;
        }
        let (title, body) = onboarding_recovery_copy(mode != "disconnected");
        let _ = show_notification_impl(app, title, body, None);
        return;
    }

    static FIRST_POLLED_AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let first_polled_at = *FIRST_POLLED_AT.get_or_init(std::time::Instant::now);
    if first_polled_at.elapsed() < std::time::Duration::from_secs(10 * 60) {
        return;
    }
    if dashboard.lifetime_requests > 0 {
        return;
    }
    if !state.try_mark_onboarding_recovery_notified() {
        return;
    }
    // Resolved only after the one-shot gate is consumed, so the setup-state
    // read happens once per install rather than on every poll.
    //
    // Trustworthy at this point specifically: clear_client_setups() empties
    // configured_clients on every quit and restore_client_setups() repopulates
    // it during startup, so a "nothing enabled" reading is a lie during early
    // boot. Ten minutes of uptime puts this well past that window. On error,
    // assume something is connected and keep the restart-your-terminal copy,
    // which is the more common cause.
    let any_connector_enabled = client_adapters::list_client_connectors(&state.cached_clients())
        .map(|connectors| connectors.iter().any(|connector| connector.enabled))
        .unwrap_or(true);
    let (title, body) = onboarding_recovery_copy(any_connector_enabled);

    let _ = show_notification_impl(app, title, body, None);
    analytics::track_event(
        app,
        "onboarding_recovery_nudge_shown",
        Some(json!({ "any_connector_enabled": any_connector_enabled })),
    );
}

/// Copy for the recovery nudge, split by why nothing has come through. The
/// old single string always blamed a stale terminal environment, which reads
/// as nonsense to someone who never got a connector turned on: there is no
/// routing to pick up, so restarting anything changes nothing.
fn onboarding_recovery_copy(any_connector_enabled: bool) -> (&'static str, &'static str) {
    if any_connector_enabled {
        return (
            "Headroom isn't seeing any traffic",
            "Setup finished, but no Claude Code or ChatGPT requests have come through yet. \
             Restart your terminal or editor so they pick up the new settings.",
        );
    }
    (
        "Headroom isn't connected to anything yet",
        "No coding tool is connected, so there's nothing for Headroom to trim. \
         Open Headroom and turn on the connector for Claude Code or Codex.",
    )
}

/// Evidence-gated sibling of the recovery nudge: Claude Code session files
/// grew during THIS app run while the proxy forwarded nothing, which is proof
/// the user is coding AROUND Headroom (almost always a terminal still holding
/// its pre-Headroom environment) — the condition the generic nudge can only
/// guess at from a timer. Reports the `unrouted_usage_detected` funnel step
/// independently of the notification's once-per-install gate, so the fleet
/// count measures the leak, not the nag budget.
/// ponytail: reads Claude Code sessions only; Codex/OpenCode usage is
/// invisible to it until their session paths are taught here.
fn maybe_fire_unrouted_usage_nudge(app: &AppHandle, state: &AppState, dashboard: &DashboardState) {
    static FIRST_POLLED_AT: std::sync::OnceLock<chrono::DateTime<Utc>> = std::sync::OnceLock::new();
    let since = *FIRST_POLLED_AT.get_or_init(Utc::now);
    // Let startup settle before trusting anything: connector state is
    // restored during early boot (see the recovery nudge's ten-minute note),
    // and the projects cache needs a warmer pass. Three minutes still catches
    // the first stale-terminal session of the day.
    if Utc::now() - since < chrono::Duration::minutes(3) {
        return;
    }
    if dashboard.lifetime_requests > 0 || !state.setup_wizard_complete() {
        return;
    }
    // Cached (~90s warmer cadence), so polling this every 5s costs nothing.
    let Ok(projects) = state.list_claude_code_projects() else {
        return;
    };
    if !claude_sessions_touched_since(&projects, since) {
        return;
    }
    static UNROUTED_BEACON_SENT: AtomicBool = AtomicBool::new(false);
    if !UNROUTED_BEACON_SENT.swap(true, Ordering::AcqRel) {
        pricing::report_funnel_step(state, "unrouted_usage_detected");
    }
    if !state.try_mark_unrouted_usage_notified() {
        return;
    }
    let _ = show_notification_impl(
        app,
        "Claude Code isn't going through Headroom",
        "You've used Claude Code since Headroom started, but none of that traffic came \
         through, so nothing was optimized. Restart your terminal or editor so it picks \
         up the new settings.",
        None,
    );
    analytics::track_event(app, "unrouted_usage_nudge_shown", None);
}

/// True when any Claude Code project's last session activity is newer than
/// `since`. `last_worked_at` is the RFC3339 string the project scan derives
/// from session-file mtimes; unparseable values count as untouched.
fn claude_sessions_touched_since(
    projects: &[crate::models::ClaudeCodeProject],
    since: chrono::DateTime<Utc>,
) -> bool {
    projects.iter().any(|project| {
        chrono::DateTime::parse_from_rfc3339(&project.last_worked_at)
            .map(|worked_at| worked_at.with_timezone(&Utc) > since)
            .unwrap_or(false)
    })
}

/// The payoff moment: lifetime savings crossed zero during THIS app session.
/// Gated on the first poll having observed zero, so an upgrade on an install
/// whose savings predate this session can never fire a fake "first savings"
/// notification. The persisted flag makes it once-ever: lifetime totals derive
/// from retained per-day buckets and can fall back to zero after long
/// inactivity, which would otherwise re-congratulate a returning user.
fn maybe_fire_first_savings_notification(
    app: &AppHandle,
    state: &AppState,
    dashboard: &DashboardState,
) {
    static TOKENS_SAVED_AT_FIRST_POLL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let at_first_poll =
        *TOKENS_SAVED_AT_FIRST_POLL.get_or_init(|| dashboard.lifetime_estimated_tokens_saved);
    if at_first_poll > 0 || dashboard.lifetime_estimated_tokens_saved == 0 {
        return;
    }
    if !state.try_mark_first_savings_notified() {
        return;
    }
    let _ = show_notification_impl(
        app,
        "First savings recorded",
        &first_savings_body(
            dashboard.lifetime_estimated_savings_usd,
            dashboard.lifetime_estimated_tokens_saved,
        ),
        None,
    );
    analytics::track_event(app, "first_savings_notification_shown", None);
}

/// Below this, a dollar figure undersells the moment more than it proves it:
/// "$0.02 saved" reads as "this does nothing" just like "under a cent" did.
/// Deliberately well above the rounding floor - the bar is "worth saying out
/// loud", not "nonzero".
const FIRST_SAVINGS_USD_WORTH_QUOTING: f64 = 0.33;

/// A small dollar figure is the wrong lead for the first-run payoff moment:
/// the first prompts' worth of trimming rarely clears a third of a cent's
/// worth of impressiveness. Under the threshold the token count carries it and
/// dollars stay out of the sentence entirely.
fn first_savings_body(usd: f64, tokens: u64) -> String {
    if usd >= FIRST_SAVINGS_USD_WORTH_QUOTING {
        format!(
            "${usd:.2} saved across {} tokens Headroom trimmed for you. \
             It compounds from here - keep coding.",
            format_token_count(tokens)
        )
    } else {
        format!(
            "Headroom just trimmed {} tokens out of your prompts. \
             That compounds with every session - keep coding.",
            format_token_count(tokens)
        )
    }
}

/// "1,240" under 100k, then "124k" / "1.2M" — notification-sized precision.
fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 100_000 {
        format!("{}k", tokens / 1_000)
    } else {
        let mut out = String::new();
        for (i, ch) in tokens.to_string().chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out.chars().rev().collect()
    }
}

/// Trimmed, lowercased value of a `HEADROOM_FAKE_*` test override, or None when
/// it is unset/empty or this build does not honor overrides.
///
/// Same enablement rule as the older fake-gate affordances: inert in stable, so
/// only RC versions (X.Y.Z-rc.N) can be talked into faking anything, and even
/// there only when a tester sets the var explicitly.
fn fake_override(name: &str) -> Option<String> {
    if !env!("CARGO_PKG_VERSION").contains("-rc") {
        return None;
    }
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_lowercase()),
        _ => None,
    }
}

/// Test overrides the frontend needs to see. Read fresh from the environment on
/// every call rather than cached, so the value always reflects how the app was
/// launched.
#[tauri::command]
fn get_debug_overrides() -> DebugOverrides {
    DebugOverrides {
        setup_stall: fake_override("HEADROOM_FAKE_SETUP_STALL")
            .filter(|mode| mode == "no_traffic" || mode == "no_savings" || mode == "drift"),
    }
}

/// Test affordance (opt-in via env, works in release/RC builds): when
/// HEADROOM_FAKE_WEEKLY_GATE is set, overwrite daily savings with a synthetic
/// 7-day history so the upgrade-banner dollar figures render on a fresh machine
/// with no real usage. Per-day USD from HEADROOM_FAKE_DAILY_SAVINGS (default 5).
/// Dormant (no-op) unless the env var is explicitly set.
fn maybe_inject_fake_daily_savings(dashboard: &mut DashboardState) {
    // Inert in stable: only RC versions (X.Y.Z-rc.N) honor the override env var.
    if !env!("CARGO_PKG_VERSION").contains("-rc") {
        return;
    }
    if std::env::var("HEADROOM_FAKE_WEEKLY_GATE")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        return;
    }
    let per_day: f64 = std::env::var("HEADROOM_FAKE_DAILY_SAVINGS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(5.0);
    let today = Utc::now();
    dashboard.daily_savings = (0..7)
        .map(|i| DailySavingsPoint {
            date: (today - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string(),
            estimated_savings_usd: per_day,
            estimated_tokens_saved: 0,
            tool_schema_savings_usd: 0.0,
            tool_schema_tokens_saved: 0,
            actual_cost_usd: 0.0,
            total_tokens_sent: 0,
            new_input_tokens: 0,
            output_savings_usd: 0.0,
            output_tokens_saved: 0,
            cache_read_tokens: None,
            cache_savings_usd: None,
            output_sampled_tokens_saved: None,
            output_baseline_tokens: None,
        })
        .collect();
    // Keep the headline card in sync with the buckets it derives from.
    dashboard.lifetime_estimated_savings_usd = dashboard
        .daily_savings
        .iter()
        .map(|point| point.estimated_savings_usd)
        .sum();
}

#[tauri::command]
async fn get_dashboard_state(app: AppHandle) -> Result<DashboardState, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state: State<'_, AppState> = app.state();
        let (mut dashboard, pending_milestones) = state.dashboard_with_pending_milestones();

        // Built from the REAL dashboard, before the demo-data injector below.
        let report = (!pending_milestones.token.is_empty()
            || pending_milestones.cumulative_report.is_some())
        .then(|| savings_report(&dashboard))
        .flatten();

        for milestone_tokens_saved in &pending_milestones.token {
            analytics::track_event(
                &app,
                "lifetime_tokens_saved_milestone_reached",
                Some(json!({
                    "milestone_tokens_saved": *milestone_tokens_saved,
                    "milestone_millions": milestone_tokens_saved / 1_000_000,
                    "milestone_kind": lifetime_token_milestone_kind(*milestone_tokens_saved),
                    "lifetime_tokens_saved": dashboard.lifetime_estimated_tokens_saved,
                    "lifetime_requests": dashboard.lifetime_requests,
                    "launch_count": state.launch_count(),
                    "launch_experience": state.launch_experience_label()
                })),
            );
            pricing::report_milestone(*milestone_tokens_saved, report.as_ref());
        }

        if let Some(total) = pending_milestones.cumulative_report {
            pricing::report_milestone(total, report.as_ref());
        }

        check_zero_spend_anomaly(&dashboard);
        check_zero_savings_anomaly(&dashboard);
        maybe_fire_onboarding_recovery_nudge(&app, &state, &dashboard);
        maybe_fire_unrouted_usage_nudge(&app, &state, &dashboard);
        maybe_fire_first_savings_notification(&app, &state, &dashboard);

        // Funnel finish line, keyed on real savings only (before the fake-data
        // injector below touches the USD figure). This used to be sent from the
        // frontend behind a once-per-install localStorage gate, where a single
        // failed POST lost the step forever and undercounted the funnel tail.
        if dashboard.lifetime_estimated_tokens_saved > 0
            && !FIRST_SAVINGS_FUNNEL_REPORTED.swap(true, Ordering::AcqRel)
        {
            pricing::report_funnel_step(&state, "first_savings_recorded");
        }

        maybe_inject_fake_daily_savings(&mut dashboard);

        dashboard
    })
    .await
    .map_err(|err| err.to_string())
}

#[tauri::command]
fn get_app_update_configuration(app: AppHandle) -> AppUpdateConfiguration {
    let current_version = app.package_info().version.to_string();
    let beta_channel_enabled = beta_channel_enabled();
    match release_updater_config(&current_version, beta_channel_enabled) {
        Ok(Some(config)) => AppUpdateConfiguration {
            enabled: true,
            current_version,
            endpoint_count: config.endpoints.len(),
            configuration_error: None,
            beta_channel_enabled,
            silent_install_supported: cfg!(target_os = "macos"),
        },
        Ok(None) => AppUpdateConfiguration {
            enabled: false,
            current_version,
            endpoint_count: 0,
            configuration_error: None,
            beta_channel_enabled,
            silent_install_supported: cfg!(target_os = "macos"),
        },
        Err(ref err) => {
            sentry::capture_message(
                &format!("app update configuration error: {err}"),
                sentry::Level::Error,
            );
            AppUpdateConfiguration {
                enabled: false,
                current_version,
                endpoint_count: 0,
                configuration_error: Some(err.clone()),
                beta_channel_enabled,
                silent_install_supported: cfg!(target_os = "macos"),
            }
        }
    }
}

#[tauri::command]
async fn check_for_app_update(
    app: AppHandle,
    pending_update: State<'_, PendingAppUpdate>,
) -> Result<Option<AvailableAppUpdate>, String> {
    let current_version = app.package_info().version.to_string();
    let config = release_updater_config(&current_version, beta_channel_enabled())?
        .ok_or_else(|| "Update checks are not configured in this build.".to_string())?;

    // On Windows the plugin installs by calling `ShellExecuteW(installer)` and
    // then `std::process::exit(0)` -- no Tauri exit event, no destructors, and
    // no chance for us to stop the Python backend we spawned. That child
    // outlives the app and keeps :6768, so the freshly installed build finds
    // its own port "held by unknown process" and falls back to 6769 (RUST-7F)
    // while an old-version backend squats the real one. One orphaned backend
    // per update, until the user reboots.
    //
    // `on_before_exit` is the only hook ahead of that exit, and it is
    // Windows-only by construction (the unix `install_inner` never calls it),
    // so macOS/Linux keep tearing down through `restart_app` exactly as before.
    // The installer does not launch until this returns, so it must be bounded:
    // `stop_headroom` caps itself at ~2s on the lifecycle lock plus ~2s on the
    // child before it force-kills.
    let teardown = app.clone();
    let updater = app
        .updater_builder()
        .pubkey(config.pubkey)
        .endpoints(config.endpoints)
        .map_err(|err| err.to_string())?
        .on_before_exit(move || {
            log::info!("update: stopping the backend before the installer exits the app");
            SHUTTING_DOWN.store(true, Ordering::Release);
            let state: tauri::State<'_, AppState> = teardown.state();
            state.stop_headroom();
        })
        .build()
        .map_err(|err| err.to_string())?;

    let checked_update =
        classify_update_check(updater.check().await).map(|update| update.map(TauriPendingUpdate));

    store_checked_update(checked_update, &pending_update.0)
}

/// A manifest with no entry for this platform means "nothing for you yet", not a
/// failure worth showing. Both release workflows now publish latest.json only
/// once every platform has built, but a channel can still legitimately omit one
/// (linux-x86_64 ships on the rc channel only), and installs predating that fix
/// would otherwise surface the raw "None of the fallback platforms ... were
/// found" error in Tools status.
fn classify_update_check<U>(
    checked: Result<Option<U>, tauri_plugin_updater::Error>,
) -> Result<Option<U>, String> {
    match checked {
        Ok(update) => Ok(update),
        Err(
            tauri_plugin_updater::Error::TargetNotFound(_)
            | tauri_plugin_updater::Error::TargetsNotFound(_),
        ) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

#[tauri::command]
async fn install_app_update(
    app: AppHandle,
    pending_update: State<'_, PendingAppUpdate>,
) -> Result<(), String> {
    let emitter_app = app.clone();
    let emitter: AppUpdateProgressEmitter = Arc::new(move |event| {
        let _ = emitter_app.emit(APP_UPDATE_PROGRESS_EVENT, &event);
    });
    install_pending_update(&pending_update.0, emitter).await
}

fn store_checked_update<U>(
    checked_update: Result<Option<U>, String>,
    pending_update: &Mutex<Option<U>>,
) -> Result<Option<AvailableAppUpdate>, String>
where
    U: InstallableAppUpdate,
{
    let update = checked_update?;
    let mut pending = pending_update.lock();

    if let Some(update) = update {
        let metadata = update.metadata();
        *pending = Some(update);
        Ok(Some(metadata))
    } else {
        *pending = None;
        Ok(None)
    }
}

async fn install_pending_update<U>(
    pending_update: &Mutex<Option<U>>,
    progress: AppUpdateProgressEmitter,
) -> Result<(), String>
where
    U: InstallableAppUpdate,
{
    let update = {
        let mut pending = pending_update.lock();
        pending
            .take()
            .ok_or_else(|| "No downloaded update is ready to install.".to_string())?
    };

    // The window hides 150ms after losing focus, and a .deb install raises a
    // polkit password prompt that takes it. The user authenticates, the update
    // lands, and the window they were just looking at is gone - leaving "Restart
    // now" behind a tray click nobody would think to make. Hold the window open
    // for the length of the install.
    INSTALLING_UPDATE.store(true, Ordering::Release);
    let result = update.install(progress).await;
    INSTALLING_UPDATE.store(false, Ordering::Release);
    result
}

/// Set while an update install is running, to keep the privilege prompt it
/// raises from hiding the window behind it. See `handle_window_event`.
static INSTALLING_UPDATE: AtomicBool = AtomicBool::new(false);

#[tauri::command]
async fn restart_app(app: AppHandle) {
    // Idempotency guard: the frontend "Restart now" button isn't disabled after
    // the first click (only during install), so a double-click fires this command
    // twice. Each call arms its own detached `open -n` relauncher, and `open -n`
    // unconditionally spawns a NEW instance (bypassing single-instance) — two
    // calls => two app instances running in parallel after an update. Run the
    // teardown+relaunch at most once per process lifetime.
    static RESTARTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RESTARTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        log::info!("restart_app: already in progress, ignoring duplicate invocation");
        return;
    }
    log::info!("restart_app: tearing down for relaunch");
    SHUTTING_DOWN.store(true, Ordering::Release);

    // Tauri 2.x has an open bug on macOS (tauri-apps/tauri#13923, #11392)
    // where `request_restart()` and `restart()` exit the process but never
    // relaunch — especially with `tauri-plugin-single-instance` loaded.
    // Workaround: spawn a detached relauncher via `open -n` against this
    // app's .app bundle (which is in-place updated by the updater).
    //
    // The relauncher is armed BEFORE the teardown below, because that teardown
    // can block the main thread for a long time (stop_headroom() does a
    // `child.wait()` with no timeout on the Python backend, and
    // analytics::shutdown() joins a worker whose last act is a network flush) —
    // observed as "Headroom is not responding". A previous version used a blind
    // `sleep 1` before `open -n`, which raced that teardown: if we hadn't
    // exited (and released the single-instance lock) within 1s, the new
    // instance saw the lock held, focused the dying old window, and bailed —
    // so the app was killed but never came back.
    //
    // Instead the relauncher waits for THIS pid to actually die (lock released)
    // before launching, and force-kills us after a deadline if teardown truly
    // deadlocks, so the lock is always freed and the new instance can boot.
    #[cfg(target_os = "macos")]
    {
        match current_app_bundle_path() {
            Some(bundle) => {
                let quoted = shell_quote_path(&bundle);
                // The relauncher runs AFTER this process exits, so the Rust
                // logger is gone by the time `open` runs. Have the script append
                // its own outcome (open's exit code) to the desktop log so a
                // field failure is diagnosable instead of silent. A non-zero rc
                // points at the launch itself (Gatekeeper, App Translocation,
                // a missing/stale bundle); rc=0 with no relaunch points at the
                // freshly-installed build crashing on its own startup.
                let log_quoted = shell_quote_path(&logging::log_path());
                log::info!("restart_app: relaunching via `open -n` against bundle {bundle:?}");
                spawn_relauncher(&format!(
                    "/usr/bin/open -n {quoted}; rc=$?; \
                     echo \"$(date '+%Y-%m-%d %H:%M:%S') relauncher: open -n {quoted} exited rc=$rc (alive=$alive)\" >> {log_quoted}"
                ));
            }
            None => {
                // No enclosing .app bundle (dev build, or an app launched from a
                // path with no `.app` ancestor). `open -n` has nothing to target;
                // the app will quit without relaunching.
                log::error!(
                    "restart_app: current_app_bundle_path() returned None (current_exe={:?}); cannot relaunch",
                    std::env::current_exe()
                );
            }
        }
    }

    // Stop the proxy before relaunching so the new build starts a fresh proxy
    // with current args (otherwise the orphan keeps serving traffic and the
    // new desktop reuses it via the reachability check). Without this, any
    // proxy-arg change shipped by an upgrade silently never takes effect.
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.stop_headroom();
    }
    analytics::shutdown(&app);

    // macOS hands the relaunch to the detached relauncher above, so it exits
    // rather than request_restart(): letting tauri spawn the new process too
    // would leave two instances running. Every other platform relaunches through
    // tauri.
    #[cfg(target_os = "macos")]
    {
        app.exit(0);
        return;
    }

    #[cfg(not(target_os = "macos"))]
    {
        app.request_restart();
    }
}

/// Walks up from `current_exe` to find the enclosing `.app` bundle path.
#[cfg(target_os = "macos")]
fn current_app_bundle_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .map(|p| p.to_path_buf())
}

#[cfg(target_os = "macos")]
fn shell_quote_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    // POSIX single-quote escaping: anything inside '...' is literal except
    // ', which we close-escape-open. Safe against spaces / special chars in
    // the bundle path (e.g. `/Applications/Headroom RC.app`).
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Spawns a detached shell that waits for THIS process to die (so the
/// single-instance lock and the proxy port are released), force-kills it after
/// ~10s if teardown deadlocks, and only then runs `launch`.
///
/// `launch` is a shell snippet and is expected to log its own outcome; `$alive`
/// is in scope for it (0 = we exited on our own, 1 = we had to be killed). The
/// force-kill is the whole point: it is what keeps a blocked teardown from
/// stranding the user on a dead window with no way back to the new build.
#[cfg(target_os = "macos")]
fn spawn_relauncher(launch: &str) {
    let cmd = relauncher_script(std::process::id(), &relauncher_expect_name(), launch);
    match crate::proc::command("/bin/sh").arg("-c").arg(cmd).spawn() {
        Ok(_) => log::info!("restart_app: relauncher spawned"),
        Err(err) => log::error!("restart_app: failed to spawn relauncher: {err}"),
    }
}

/// What `ps -o comm=` reports for THIS process, for the identity gate below.
///
/// Not derivable from the executable name: the Linux .deb installs the binary as
/// `/usr/bin/headroom-desktop` but the kernel reports `headroom`, so matching on
/// the file name never fires and the gate silently blocks every force-kill. Read
/// the value the kernel will actually report instead. macOS has no `/proc`, but
/// its `ps -o comm=` prints the full executable path, so the file name matches as
/// a substring there.
#[cfg(target_os = "macos")]
fn relauncher_expect_name() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/comm")
            .map(|comm| comm.trim().to_string())
            .unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default()
    }
}

/// `expect`: what `ps -o comm=` should still report for `pid` at force-kill
/// time. The pid is resolved now but signalled up to 10s later, and by then it
/// may belong to something else entirely - on a box that recycles pids fast, an
/// unguarded `kill -9` lands on an innocent process (and if that process is a
/// session or group leader, it takes the user's whole desktop session with it).
/// Same rule as the port-reclaim path: verify identity before signalling.
#[cfg(target_os = "macos")]
fn relauncher_script(pid: u32, expect: &str, launch: &str) -> String {
    let log_quoted = shell_quote_path(&logging::log_path());
    format!(
        "alive=1; \
         for i in $(seq 1 100); do \
           if ! kill -0 {pid} 2>/dev/null; then alive=0; break; fi; \
           sleep 0.1; \
         done; \
         if [ \"$alive\" = 1 ] && [ -n \"{expect}\" ]; then \
           now=$(ps -o comm= -p {pid} 2>/dev/null); \
           case \"$now\" in \
             *{expect}*) kill -9 {pid} 2>/dev/null; sleep 0.5;; \
             *) alive=stale; \
                echo \"$(date '+%Y-%m-%d %H:%M:%S') relauncher: pid {pid} is now '$now', not ours; not killing\" >> {log_quoted};; \
           esac; \
         fi; \
         {launch}"
    )
}

/// Best-effort: schedule the running `.app` bundle to be moved to the user's
/// Trash once this process exits. Returns the bundle path that was scheduled,
/// or `None` if there is no enclosing bundle, it is App-Translocated, or the
/// detached helper could not be spawned.
///
/// We can't delete our own running bundle inline, so we spawn a detached shell
/// that waits for our PID to exit (mirroring the `restart_app` relauncher) and
/// then `mv`s the bundle into `~/.Trash`. `mv` is used rather than a Finder
/// "delete" because by the time it runs the app is gone and could not answer a
/// Finder automation (TCC) prompt; moving into `~/.Trash` needs no such
/// permission and keeps the uninstall recoverable.
#[cfg(target_os = "macos")]
fn schedule_app_bundle_trash() -> Option<std::path::PathBuf> {
    let bundle = current_app_bundle_path()?;

    // App Translocation: the app was launched quarantined (e.g. straight from a
    // DMG, never moved to /Applications) and runs from a randomized read-only
    // copy under `.../AppTranslocation/...`. Trashing that copy does nothing
    // useful and leaves the real install in place, so skip it.
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        log::warn!(
            "uninstall: skipping app-bundle removal; running from translocated path {bundle:?}"
        );
        return None;
    }

    let pid = std::process::id();
    let quoted = shell_quote_path(&bundle);
    let log_quoted = shell_quote_path(&logging::log_path());
    let cmd = format!(
        "alive=1; \
         for i in $(seq 1 100); do \
           if ! kill -0 {pid} 2>/dev/null; then alive=0; break; fi; \
           sleep 0.1; \
         done; \
         if [ \"$alive\" = 1 ]; then kill -9 {pid} 2>/dev/null; sleep 0.5; fi; \
         base=$(basename {quoted}); \
         dest=\"$HOME/.Trash/$base\"; \
         if [ -e \"$dest\" ]; then dest=\"$HOME/.Trash/${{base%.app}} $(date +%s).app\"; fi; \
         mv -f {quoted} \"$dest\"; rc=$?; \
         echo \"$(date '+%Y-%m-%d %H:%M:%S') uninstall: mv {quoted} -> $dest exited rc=$rc (alive=$alive)\" >> {log_quoted}",
        pid = pid,
        quoted = quoted,
        log_quoted = log_quoted,
    );
    match crate::proc::command("/bin/sh").arg("-c").arg(cmd).spawn() {
        Ok(_) => {
            log::info!("uninstall: scheduled app-bundle trash for {bundle:?}");
            Some(bundle)
        }
        Err(err) => {
            log::error!("uninstall: failed to spawn app-bundle trasher: {err}");
            None
        }
    }
}

/// Best-effort Windows counterpart of `schedule_app_bundle_trash`: hand off to
/// the NSIS uninstaller so Headroom also leaves Add/Remove Programs, the Start
/// menu and $INSTDIR. `perform_full_cleanup` cannot do any of that itself, and
/// on a currentUser install it cannot even finish the data wipe, because
/// $INSTDIR *is* %LOCALAPPDATA%\Headroom and the running exe is undeletable.
///
/// `/S` makes the uninstaller copy itself to %TEMP% and kill us on the way
/// through, so the whole install dir goes. Its PREUNINSTALL hook re-runs
/// `--uninstall`, which is a no-op sweep by then.
#[cfg(target_os = "windows")]
fn schedule_windows_uninstaller() -> Option<std::path::PathBuf> {
    let uninstaller = std::env::current_exe()
        .ok()?
        .parent()?
        .join("uninstall.exe");
    // Absent for a dev/portable build: cleanup already ran, nothing to hand off.
    if !uninstaller.exists() {
        log::info!("uninstall: no NSIS uninstaller at {uninstaller:?}, skipping");
        return None;
    }
    match crate::proc::command(&uninstaller).arg("/S").spawn() {
        Ok(_) => {
            log::info!("uninstall: launched NSIS uninstaller {uninstaller:?}");
            Some(uninstaller)
        }
        Err(err) => {
            log::error!("uninstall: failed to launch NSIS uninstaller: {err}");
            None
        }
    }
}

#[tauri::command]
fn show_app_update_notification(app: AppHandle, version: String) -> Result<(), String> {
    show_app_update_notification_impl(&app, &version)
}

fn app_update_notification_body(version: &str) -> String {
    let trimmed = version.trim();
    let lead = if trimmed.is_empty() {
        "A Headroom update is ready to install.".to_string()
    } else {
        format!("Headroom {trimmed} is ready to install.")
    };

    format!("{lead} Open Headroom to review the release and install it.")
}

fn show_app_update_notification_impl(app: &AppHandle, version: &str) -> Result<(), String> {
    let body = app_update_notification_body(version);
    show_notification_impl(
        app,
        "Headroom Update Available",
        &body,
        Some("update".into()),
    )
}

#[tauri::command]
fn show_notification(
    app: AppHandle,
    title: String,
    body: String,
    action: Option<String>,
) -> Result<(), String> {
    show_notification_impl(&app, &title, &body, action)
}

#[cfg(target_os = "macos")]
fn show_notification_impl(
    app: &AppHandle,
    title: &str,
    body: &str,
    _action: Option<String>,
) -> Result<(), String> {
    let title = title.to_string();
    let body = body.to_string();
    let identifier = if tauri::is_dev() {
        "com.apple.Terminal".to_string()
    } else {
        app.config().identifier.clone()
    };

    std::thread::spawn(move || {
        // set_application is guarded by a Once internally, so repeat calls are cheap.
        let _ = mac_notification_sys::set_application(&identifier);
        let _ = mac_notification_sys::Notification::new()
            .title(&title)
            .message(&body)
            // Waiting for clicks spins a private NSRunLoop in mac-notification-sys
            // and can hold a full CPU core while the notification is pending.
            .asynchronous(true)
            .send();
    });
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn show_notification_impl(
    app: &AppHandle,
    title: &str,
    body: &str,
    _action: Option<String>,
) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| format!("Could not show notification: {e}"))
}

#[tauri::command]
async fn install_addon(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<DashboardState, String> {
    match id.as_str() {
        "markitdown" => {
            state
                .tool_manager
                .install_markitdown()
                .map_err(|err| err.to_string())?;
            client_adapters::enable_markitdown_integration(
                &state.tool_manager.markitdown_entrypoint(),
                &state.tool_manager.markitdown_shim_path(),
                &state.tool_manager.managed_python(),
            )
            .map_err(|err| {
                format!("markitdown installed but enabling integration failed: {err:#}")
            })?;
        }
        "rtk" => {
            state
                .tool_manager
                .install_rtk()
                .map_err(|err| err.to_string())?;
            client_adapters::set_rtk_enabled(
                true,
                &state.tool_manager.rtk_entrypoint(),
                &state.tool_manager.managed_python(),
            )
            .map_err(|err| format!("rtk installed but enabling integration failed: {err:#}"))?;
        }
        "ponytail" | "caveman" => {
            let codex_outdated = state
                .tool_manager
                .install_plugin(&id)
                .map_err(|err| err.to_string())?;
            if codex_outdated {
                let name = if id == "caveman" {
                    "Caveman"
                } else {
                    "Ponytail"
                };
                let _ = show_notification_impl(
                    &app,
                    &format!("Update the Codex CLI to finish {name} setup"),
                    &format!("{name} is installed for Claude Code. Your Codex CLI is too old to add it -- update the Codex CLI, then re-install {name} to enable it there too."),
                    None,
                );
            }
        }
        "serena" => {
            state
                .tool_manager
                .install_serena()
                .map_err(|err| err.to_string())?;
        }
        "context7" => {
            state
                .tool_manager
                .install_context7()
                .map_err(|err| err.to_string())?;
        }
        "codebase-memory" => {
            state
                .tool_manager
                .install_codebase_memory()
                .map_err(|err| err.to_string())?;
        }
        other => return Err(format!("unknown addon: {other}")),
    }
    analytics::track_event(&app, &format!("{id}_installed"), None);
    Ok(state.dashboard())
}

#[tauri::command]
async fn set_addon_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<DashboardState, String> {
    match id.as_str() {
        "markitdown" => {
            state
                .tool_manager
                .set_markitdown_enabled(enabled)
                .map_err(|err| err.to_string())?;
            if enabled {
                client_adapters::enable_markitdown_integration(
                    &state.tool_manager.markitdown_entrypoint(),
                    &state.tool_manager.markitdown_shim_path(),
                    &state.tool_manager.managed_python(),
                )
                .map_err(|err| err.to_string())?;
            } else {
                client_adapters::disable_markitdown_integration(
                    &state.tool_manager.markitdown_shim_path(),
                )
                .map_err(|err| err.to_string())?;
            }
        }
        "ponytail" | "caveman" => {
            state
                .tool_manager
                .set_plugin_enabled(&id, enabled)
                .map_err(|err| err.to_string())?;
        }
        "serena" => {
            state
                .tool_manager
                .set_serena_enabled(enabled)
                .map_err(|err| err.to_string())?;
        }
        "context7" => {
            state
                .tool_manager
                .set_context7_enabled(enabled)
                .map_err(|err| err.to_string())?;
        }
        "codebase-memory" => {
            state
                .tool_manager
                .set_codebase_memory_enabled(enabled)
                .map_err(|err| err.to_string())?;
        }
        other => return Err(format!("unknown addon: {other}")),
    }
    let action = if enabled { "enabled" } else { "disabled" };
    analytics::track_event(&app, &format!("{id}_{action}"), None);
    Ok(state.dashboard())
}

#[tauri::command]
async fn uninstall_addon(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<DashboardState, String> {
    match id.as_str() {
        "markitdown" => {
            let _ = client_adapters::disable_markitdown_integration(
                &state.tool_manager.markitdown_shim_path(),
            );
            state
                .tool_manager
                .uninstall_markitdown()
                .map_err(|err| err.to_string())?;
        }
        "rtk" => {
            client_adapters::set_rtk_enabled(
                false,
                &state.tool_manager.rtk_entrypoint(),
                &state.tool_manager.managed_python(),
            )
            .map_err(|err| err.to_string())?;
            state
                .tool_manager
                .uninstall_rtk()
                .map_err(|err| err.to_string())?;
        }
        "ponytail" | "caveman" => {
            state
                .tool_manager
                .uninstall_plugin(&id)
                .map_err(|err| err.to_string())?;
        }
        "serena" => {
            state
                .tool_manager
                .uninstall_serena()
                .map_err(|err| err.to_string())?;
        }
        "context7" => {
            state
                .tool_manager
                .uninstall_context7()
                .map_err(|err| err.to_string())?;
        }
        "codebase-memory" => {
            state
                .tool_manager
                .uninstall_codebase_memory()
                .map_err(|err| err.to_string())?;
        }
        other => return Err(format!("unknown addon: {other}")),
    }
    analytics::track_event(&app, &format!("{id}_uninstalled"), None);
    Ok(state.dashboard())
}

fn emit_bootstrap_progress(app: &AppHandle, state: &AppState) {
    let _ = app.emit("bootstrap_progress", state.bootstrap_progress());
}

/// Fire-and-forget, download-only warm-up for the consented bootstrap: pulls
/// the Python tarball and pinned wheel into the downloads cache while the
/// user is still in signup/client-setup. Failures log at info, not warn —
/// offline here is normal (bootstrap simply downloads later) and warn would
/// ship every offline launcher to Sentry.
#[tauri::command]
fn prefetch_bootstrap_artifacts(app: AppHandle) {
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app.state();
        let started = std::time::Instant::now();
        match state.tool_manager.prefetch_bootstrap_artifacts() {
            Ok(()) => log::info!(
                "bootstrap artifact prefetch finished in {:.1}s",
                started.elapsed().as_secs_f64()
            ),
            Err(err) => log::info!("bootstrap artifact prefetch skipped: {err:#}"),
        }
    });
}

#[tauri::command]
fn start_bootstrap(app: AppHandle) -> Result<(), String> {
    let already_installed = {
        let state: tauri::State<'_, AppState> = app.state();
        let already_installed = state.tool_manager.python_runtime_installed();
        state.begin_bootstrap()?;
        emit_bootstrap_progress(&app, &state);
        already_installed
    };

    if already_installed {
        analytics::track_event(
            &app,
            "bootstrap_skipped",
            Some(json!({ "reason": "already_installed" })),
        );
    } else {
        analytics::track_event(&app, "bootstrap_started", None);
        pricing::report_funnel_step(&app.state::<AppState>(), "bootstrap_started");
    }

    let app_handle = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_handle.state();

        if !already_installed {
            let result = state.tool_manager.bootstrap_all_with_progress(|step| {
                state.update_bootstrap_step(step);
                emit_bootstrap_progress(&app_handle, &state);
            });
            if let Err(err) = result {
                let kind = classify_bootstrap_failure(&err);
                // Dedupe repeat captures per machine: a policy verdict (e.g.
                // Application Control) fails identically on every relaunch,
                // and RUST-AN was one machine re-filing it 21 times in a day.
                // `Other` is a grab-bag split by pip category in the
                // fingerprint, so the dedupe key carries the category too --
                // a different cause within 24h must still report.
                let capture_key = match kind {
                    BootstrapFailureKind::Other => format!(
                        "other:{}",
                        tool_manager::pip_failure_category(&tool_manager::compact_pip_failure(
                            &err
                        ))
                    ),
                    _ => kind.as_str().to_string(),
                };
                if state
                    .tool_manager
                    .should_capture_bootstrap_failure(&capture_key)
                {
                    capture_bootstrap_failure(&err, kind);
                } else {
                    log::warn!(
                        "skipping Sentry capture for bootstrap_failed ({}): same failure \
                         reported within 24h",
                        kind.as_str()
                    );
                }
                // This failure got a verdict and a Sentry event; drop the
                // attempt marker so the next launch does not also report it
                // as bootstrap_abandoned.
                state.tool_manager.clear_bootstrap_attempt();
                *state.bootstrap_failure_report.lock() = Some(BootstrapFailureReport {
                    kind: kind.as_str().into(),
                    detail: tool_manager::compact_pip_failure(&err),
                });
                state.mark_bootstrap_failed(user_message_for(kind));
                emit_bootstrap_progress(&app_handle, &state);
                analytics::track_event(
                    &app_handle,
                    "bootstrap_failed",
                    Some(json!({ "phase": "install_runtime", "kind": kind.as_str() })),
                );
                pricing::report_funnel_step(&state, "bootstrap_failed");
                return;
            }

            if let Err(err) = client_adapters::ensure_rtk_integrations(
                &state.tool_manager.rtk_entrypoint(),
                &state.tool_manager.managed_python(),
            ) {
                log::warn!("RTK integrations failed after start_bootstrap thread: {err:#}");
            }
        }

        // Show "Starting Headroom" in the install loader while we wait for the
        // proxy to come up. This runs for both fresh installs and already-installed
        // re-runs. On a fresh machine macOS Gatekeeper scans the entire venv on
        // first execution (30-60s); keeping `complete: false` here means the user
        // cannot click Continue until the proxy is actually reachable.
        state.mark_bootstrap_proxy_starting();
        emit_bootstrap_progress(&app_handle, &state);

        // Hold `runtime_starting = true` for the entire spawn + wait window so
        // the tray spinner and UI share a single source of truth for "headroom
        // is booting but not yet serving". `ensure_headroom_running` toggles
        // this flag internally, but flips it back to false the instant
        // `start_headroom_background()` returns (process spawn only, not
        // readiness) — so we re-assert it here, *after* that call, and clear
        // it only once the proxy is reachable (or we time out). This mirrors
        // `warm_runtime_on_launch`.
        // Seed the output-shaper savings baseline BEFORE starting the proxy
        // (runtime is installed by this point). The proxy's recorder loads the
        // baseline once at boot and clobbers a later write on flush, so seeding
        // first is what lets the dashboard estimate appear without an app
        // relaunch. Idempotent and bounded; we are on the bootstrap thread, so
        // the one-time scan does not block the UI.
        state.tool_manager.seed_verbosity_baseline_if_needed();

        let ensure_result = state.ensure_headroom_running();
        state.set_runtime_starting(true);

        if let Err(err) = ensure_result {
            log::debug!("headroom auto-start failed after bootstrap: {err}");
            // Bootstrap finishes and immediately tries to start the proxy;
            // a port conflict here counts as a "fresh launch" stuck case.
            let handled = port_conflict::note_proxy_failed(&app_handle, &err, true);
            if !handled {
                capture_headroom_start_failure("headroom auto-start failed after bootstrap", &err);
            }
            // Fall through so the user is not stuck on the install loader
            // indefinitely. The test screen will show a retry option.
        } else {
            port_conflict::note_proxy_started(&app_handle);
            // The intercept layer on 6767 is always bound by the Rust app, so
            // reachability really means "headroom's backend on 6768 is up".
            // We probe it by hitting 6767/readyz — the intercept forwards to
            // 6768, answering 503 (local, no upstream fallback for proxy
            // paths) until the backend actually responds, so a 2xx confirms
            // the full chain is live. Gatekeeper's first-launch
            // scan of the bundled venv can take 30-60s, so we wait up to 60s
            // to match the ETA shown to the user.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while std::time::Instant::now() < deadline {
                if state::headroom_proxy_reachable() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }

        state.set_runtime_starting(false);
        state.mark_bootstrap_complete();
        emit_bootstrap_progress(&app_handle, &state);
        analytics::track_event(&app_handle, "bootstrap_completed", None);
        pricing::report_funnel_step(&state, "bootstrap_completed");
    });

    Ok(())
}

#[derive(Copy, Clone, Debug)]
enum BootstrapFailureKind {
    /// Corporate proxy / AV / VPN injecting a self-signed root, so pip can't
    /// verify pypi.org or github.com. Not our bug, but users here are stuck
    /// until they configure `REQUESTS_CA_BUNDLE` or disable TLS inspection.
    SslInterception,
    /// Python's `tempfile` couldn't create a directory in any candidate
    /// location (TMPDIR, /tmp, /var/tmp, /usr/tmp, cwd). Disk full, TCC
    /// blocking writes, or a stale macOS per-user temp dir. Not our bug,
    /// but the default "couldn't download a file" message is misleading
    /// because pip never even got to the network.
    NoUsableTempDir,
    /// Transient network/download problem: the server returned a 5xx (e.g.
    /// GitHub's 504 Gateway Time-out on a release asset), the connection was
    /// reset, DNS failed, or a request timed out. Not our bug and not the
    /// user's environment — it's self-recoverable, so we frame it softly and
    /// the user just needs to click Try again.
    NetworkDownload,
    /// Our lock pinned a version that has no wheel for this interpreter or
    /// platform, so pip resolves nothing and *every* retry fails identically
    /// (RUST-6S/RUST-1G: `onnxruntime==1.27.0` on Intel macOS, where releases
    /// stop at 1.23.2). This is a defect in a build we shipped, not a fault of
    /// the user's machine — the only fix is a newer app, so the message must
    /// send them to the updater instead of to their network settings.
    UnsupportedPin,
    /// The OS refused a read/write pip needed -- a TCC prompt denied, an MDM or
    /// endpoint-protection policy guarding the app-support directory, or a
    /// half-owned venv left by an install that ran as another user. Retrying
    /// changes nothing until the permission does, so the message must not send
    /// the user back to the Try again button.
    Permission,
    /// Windows loaded a foreign OpenSSL into our interpreter. `_ssl.pyd` does
    /// not provide `OPENSSL_Applink`, so a libcrypto built with uplink aborts
    /// the moment it is used -- ensurepip dies before pip speaks a word
    /// (RUST-8K). Nothing about the machine will change on its own, so like
    /// `Permission` this must not send the user back to Try again: 25 events
    /// from one host were 25 relaunches into the same wall.
    SslLibraryConflict,
    /// Windows Application Control (Smart App Control, WDAC, or AppLocker)
    /// refused to run a file we just extracted: "An Application Control policy
    /// has blocked this file" (os error 4551). A policy verdict, so every
    /// retry hits the same wall (RUST-8K, third cause: venv creation died on
    /// it, and the retry got further only for _ssl's DLLs to be blocked the
    /// same way). The user action is a security setting, not Try again.
    AppControlBlocked,
    /// pip fell back to building a package from source and the build failed.
    /// Since every install passes `--only-binary=:all:` (see `PIP_ONLY_BINARY`)
    /// this should be unreachable, so reaching it means *we* shipped a lock or
    /// a vendored wheel that does not cover this machine. Never tell the user
    /// to install Xcode: needing a compiler at all is our bug, not their setup.
    SourceBuild,
    Other,
}

impl BootstrapFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            BootstrapFailureKind::SslInterception => "ssl_interception",
            BootstrapFailureKind::NoUsableTempDir => "no_usable_tempdir",
            BootstrapFailureKind::NetworkDownload => "network_download",
            BootstrapFailureKind::UnsupportedPin => "unsupported_pin",
            // Same vocabulary as `pip_failure_category`, so a support mail's
            // failure_kind lines up with the pip-layer Sentry issue.
            BootstrapFailureKind::Permission => "permission",
            BootstrapFailureKind::SslLibraryConflict => "ssl_library_conflict",
            BootstrapFailureKind::AppControlBlocked => "app_control_blocked",
            BootstrapFailureKind::SourceBuild => "build",
            BootstrapFailureKind::Other => "other",
        }
    }
}

/// Sentry drops an extra larger than roughly 16KB, so every log/output tail we
/// attach is capped well under that. One constant so the three call sites stay
/// in step.
const SENTRY_EXTRA_TAIL_BYTES: usize = 12_000;

/// Last `max_bytes` of `text`, prefixed with how much was dropped.
///
/// Slices on a char boundary. The three call sites this replaced each did
/// `&s[s.len() - 12_000..]` with a raw byte offset, which panics ("byte index N
/// is not a char boundary") the moment the cut lands mid-codepoint -- and these
/// inputs are precisely where non-ASCII turns up: app logs quoting Windows
/// paths with accented usernames, and Python's non-ASCII startup banner
/// (RUST-7C). Panicking here loses the very report being assembled, and on the
/// `bootstrap_abandoned` path it happens during startup, before there is a
/// window to show anything in.
fn tail_bytes_for_sentry(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Round the cut forward to the next boundary: dropping a few extra bytes is
    // always safe, and this can never run past the end because `text.len()` is
    // itself a boundary.
    let mut cut = text.len() - max_bytes;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[truncated {cut} bytes]\n...{}", &text[cut..])
}

/// Drop the per-request connection-pool debug lines from an app-log tail.
///
/// The health poll opens a connection to the local proxy several times a second
/// and `reqwest` logs every one, so on a machine that sat at a bootstrap step
/// for minutes these crowd out everything else. RUST-9Y spent its entire 12KB
/// budget on them and arrived with nothing about the install in it at all,
/// which is the one thing the extra exists to carry. Only this exact line is
/// dropped -- connection *failures* and every other target are kept.
fn strip_connection_noise(log: &str) -> String {
    log.lines()
        .filter(|line| !line.contains("reqwest::connect: starting new connection"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn classify_bootstrap_failure(err: &anyhow::Error) -> BootstrapFailureKind {
    // pip/venv failures surface as CommandFailure, where stdout/stderr carry the
    // real signal. Our own reqwest downloads (Python runtime, rtk binary) have no
    // CommandFailure, so fall back to the formatted error chain for those.
    let cmd_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::CommandFailure>());
    let haystack = match cmd_failure {
        Some(failure) => format!("{}\n{}", failure.stdout, failure.stderr),
        None => format!("{err:#}"),
    };

    if haystack.contains("CERTIFICATE_VERIFY_FAILED")
        || haystack.contains("self-signed certificate in certificate chain")
        || haystack.contains("self signed certificate in certificate chain")
    {
        BootstrapFailureKind::SslInterception
    } else if is_ssl_library_conflict_signal(&haystack) {
        BootstrapFailureKind::SslLibraryConflict
    } else if is_app_control_signal(&haystack) {
        BootstrapFailureKind::AppControlBlocked
    } else if haystack.contains("No usable temporary directory found") {
        BootstrapFailureKind::NoUsableTempDir
    } else if is_unsupported_pin_signal(&haystack) {
        BootstrapFailureKind::UnsupportedPin
    } else if is_permission_signal(&haystack) {
        BootstrapFailureKind::Permission
    } else if is_source_build_signal(&haystack) {
        BootstrapFailureKind::SourceBuild
    } else if is_network_download_signal(&haystack) {
        BootstrapFailureKind::NetworkDownload
    } else {
        BootstrapFailureKind::Other
    }
}

/// True when a foreign OpenSSL aborted the interpreter.
///
/// Shares its signal string with `pip_failure_category`'s `openssl-applink`
/// bucket so the two layers agree. `OPENSSL_Uplink` alone is enough: the abort
/// prints it whether or not the `no OPENSSL_Applink` line survives the pipe.
fn is_ssl_library_conflict_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no openssl_applink") || lower.contains("openssl_uplink")
}

/// True when Windows Application Control (Smart App Control, WDAC, AppLocker)
/// blocked a file we just extracted. Windows localizes the prose like every
/// other error, so the numeric code is the locale-independent handle
/// (ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION = 4551). Shares its strings with
/// `pip_failure_category`'s `app-control` bucket so the two layers agree.
fn is_app_control_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("application control policy has blocked")
        || lower.contains("(os error 4551)")
        // Windows localizes the verdict and only `CreateProcess` failures
        // carry the numeric code. When the policy blocks a DLL that Python is
        // importing, CPython reports it as an ImportError with the OS prose
        // and no code at all (RUST-BB/BA: `_sqlite3`, Spanish Windows). The
        // Spanish text is verbatim from those events; the structural check in
        // `is_blocked_runtime_dll_signal` covers every other locale.
        || lower.contains("control de aplicaciones bloque")
}

/// True when Windows refused to load one of the bundled interpreter's own
/// extension DLLs.
///
/// CPython reports that as `ImportError: DLL load failed while importing
/// <module>: <reason>`. The prefix is CPython's and English on every locale;
/// the reason is Windows' `FormatMessage` prose, localized, and for an
/// Application Control verdict it carries no numeric code -- so it slips past
/// `is_app_control_signal` on any non-English machine (RUST-BB/BA/5C: one host,
/// Spanish Windows, `_sqlite3` blocked, filed as three Error-level issues).
///
/// Scoped to the standard-library extensions that python-build-standalone
/// ships: those are self-contained (their dependent DLLs sit in the same
/// folder), so a load failure is a policy or antivirus verdict on our freshly
/// installed files, or a quarantined one -- not a dependency the user could
/// install. Third-party extensions (onnxruntime, torch) are deliberately NOT
/// here: their load failures are usually a missing Visual C++ runtime, which
/// `classify_startup_error` handles from onnxruntime's own English warning.
pub(crate) fn is_blocked_runtime_dll_signal(text: &str) -> bool {
    const STDLIB_EXTENSIONS: &[&str] = &[
        "_sqlite3",
        "_ssl",
        "_ctypes",
        "_socket",
        "_hashlib",
        "_bz2",
        "_lzma",
        "_decimal",
        "_multiprocessing",
        "_asyncio",
        "_overlapped",
        "_queue",
        "_uuid",
        "_elementtree",
        "_zoneinfo",
        "pyexpat",
        "select",
        "unicodedata",
    ];
    // Anchor on `dll load failed` and step over an optional `while `: CPython
    // 3.8+ writes "while importing", but the copy of this chain that reaches us
    // is not always CPython's own -- upstream re-wraps the traceback into
    // `last_startup_error`, and RUST-5C's event carries both spellings across
    // its two fields. The module name after it is what makes the match
    // specific, so tolerating the shorter phrasing costs no precision.
    const MARKER: &str = "dll load failed ";
    let lower = text.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(idx) = rest.find(MARKER) {
        let after = &rest[idx + MARKER.len()..];
        let after = after.strip_prefix("while ").unwrap_or(after);
        let module = after
            .strip_prefix("importing ")
            .unwrap_or("")
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("");
        if STDLIB_EXTENSIONS.contains(&module) {
            return true;
        }
        rest = after;
    }
    false
}

/// PATH directories holding an OpenSSL that could be loaded into our
/// interpreter ahead of the one we ship.
///
/// The abort says only that the wrong libcrypto won, never whose it is, which
/// is why RUST-8K sat unactionable. Windows resolves a DLL by base name, so
/// naming every PATH directory that has one turns "some program on your PC"
/// into a directory the user can act on. Our own runtime is never on PATH, so
/// every hit here is foreign by construction.
///
/// Returns an empty list off Windows, where these names do not exist -- no
/// `cfg`, so the scan stays testable on any platform.
pub(crate) fn conflicting_openssl_dirs(path_var: &str) -> Vec<String> {
    const NAMES: &[&str] = &["libcrypto-3-x64.dll", "libssl-3-x64.dll"];
    let mut hits = Vec::new();
    for dir in std::env::split_paths(path_var) {
        if NAMES.iter().any(|name| dir.join(name).is_file()) {
            let shown = dir.display().to_string();
            if !hits.contains(&shown) {
                hits.push(shown);
            }
        }
    }
    hits
}

/// True when pip could not resolve a pin at all — no wheel exists for this
/// interpreter/platform. Checked before [`is_network_download_signal`] because
/// pip echoes every index and find-links URL it consulted, so an unrelated
/// timeout word in that preamble must not steal the classification.
fn is_unsupported_pin_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // "No matching distribution" is only a verdict about OUR pin when pip
    // actually read an index. When every index/find-links fetch died
    // (RUST-90/91: TLS-broken middleware on one machine), pip reports every
    // pin as "(from versions: none)" -- that is the network, and calling it
    // unsupported_pin sends the user to the updater instead of their
    // connection.
    if tool_manager::pip_index_fetch_failed(&lower) {
        return false;
    }
    lower.contains("no matching distribution found")
        || lower.contains("could not find a version that satisfies")
}

/// True when the OS refused an operation pip needed. Shares its signal strings
/// with `pip_failure_category`'s `permission` bucket so the two layers agree.
/// Checked before [`is_network_download_signal`] for the same reason as
/// [`is_unsupported_pin_signal`]: pip's index-URL preamble is full of network
/// vocabulary that would otherwise win.
fn is_permission_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("permission denied")
        || lower.contains("check the permissions")
        || lower.contains("access is denied")
        || lower.contains("errno 13")
}

/// True when pip tried to build a package from source and failed. Mirrors
/// `pip_failure_category`'s `build` bucket.
fn is_source_build_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("failed building wheel")
        || lower.contains("microsoft visual c++")
        || lower.contains("error: subprocess-exited-with-error")
}

/// True when a bootstrap failure looks like a transient network/download
/// problem (server 5xx, connection reset, DNS failure, request timeout) rather
/// than a configuration or environment fault. These are self-recoverable: the
/// user just needs to retry, so we frame them softly and report them to Sentry
/// as warnings instead of errors.
fn is_network_download_signal(text: &str) -> bool {
    // Signatures from reqwest (`error_for_status`, transport errors) and curl/pip
    // network failures. Lowercased once; keep entries lowercase.
    const SIGNALS: &[&str] = &[
        "http status server error", // reqwest error_for_status on any 5xx
        "gateway time-out",         // 502/504 from GitHub's edge
        "bad gateway",
        "service unavailable",
        "error sending request",
        "could not fetch url",  // pip: an index/find-links fetch died
        "max retries exceeded", // urllib3 inside pip
        "operation timed out",
        "connection timed out",
        "timed out",
        "connection refused",
        "connection reset",
        "connection closed",
        "tcp connect error",
        "dns error",
        "failed to lookup address",
        "could not resolve host",
        "network is unreachable",
        "temporary failure in name resolution",
    ];
    let lower = text.to_ascii_lowercase();
    SIGNALS.iter().any(|signal| lower.contains(signal))
}

fn user_message_for(kind: BootstrapFailureKind) -> &'static str {
    match kind {
        BootstrapFailureKind::SslInterception => {
            "Installation failed: your network is intercepting secure connections \
             (self-signed certificate in the TLS chain), so Headroom can't verify \
             pypi.org or github.com. This usually means a corporate proxy, VPN, or \
             antivirus is inspecting HTTPS traffic. Set both the REQUESTS_CA_BUNDLE \
             and SSL_CERT_FILE environment variables to your organization's CA \
             bundle, or disable TLS inspection for pypi.org, \
             files.pythonhosted.org, github.com, and huggingface.co, then restart \
             the app. Contact support@extraheadroom.com if you need help."
        }
        BootstrapFailureKind::NoUsableTempDir => {
            "Installation failed: Headroom can't create temporary files on this Mac. \
             This usually means your disk is full, or security software (like an MDM \
             profile or endpoint protection) is blocking writes to /tmp and \
             /var/folders. Free up disk space, restart your Mac, and try again. \
             If it still fails, contact support@extraheadroom.com."
        }
        BootstrapFailureKind::NetworkDownload => {
            "Couldn't reach the download server. This is usually a temporary \
             network or server hiccup, not a problem with your Mac. Check your \
             internet connection and click Try again. If it keeps failing, a \
             firewall, VPN, or corporate proxy may be blocking pypi.org and \
             files.pythonhosted.org - try another network or contact \
             support@extraheadroom.com."
        }
        BootstrapFailureKind::UnsupportedPin => {
            "Installation failed: this version of Headroom can't build its \
             runtime on this computer - one of its components has no release \
             for your processor. This is a bug in the version you're running, not \
             a problem with your machine, and retrying will keep failing. \
             Click Check for updates to get a newer Headroom, which fixes it. \
             If no update is offered, contact support@extraheadroom.com."
        }
        BootstrapFailureKind::Permission => {
            "Installation failed: Headroom wasn't allowed to write the files it \
             needs. That is almost always security software, an MDM profile, or \
             a permission prompt that was declined - not your network, and not \
             something clicking Try again will change. Restart your computer and \
             reopen Headroom. If it still fails, use Contact support below and \
             we'll read the details it sends."
        }
        BootstrapFailureKind::SslLibraryConflict => {
            "Installation failed: another program on this PC has put a conflicting \
             copy of OpenSSL (libcrypto-3-x64.dll or libssl-3-x64.dll) on your \
             PATH, and Windows loads it into Headroom's Python instead of ours. \
             That crashes the installer before it starts, and clicking Try again \
             will keep hitting it. Search your PATH for those two files - older \
             Git, PostgreSQL, OpenVPN and Anaconda installs are the usual sources \
             - remove that folder from PATH, then restart your PC. Use Contact \
             support below and we'll read the details it sends, including which \
             folders we found."
        }
        BootstrapFailureKind::AppControlBlocked => {
            "Installation failed: Windows Application Control (Smart App Control, \
             AppLocker, or a company WDAC policy) blocked the files Headroom just \
             installed, and clicking Try again will keep hitting the same block. \
             On a personal PC, check Windows Security > App & browser control. On \
             a work PC, ask your IT team to allow Headroom's install folder \
             (%LOCALAPPDATA%\\Headroom). Use Contact support below and we'll read \
             the details it sends."
        }
        BootstrapFailureKind::SourceBuild => {
            "Installation failed: Headroom couldn't assemble its runtime on this \
             computer, because one of its components has no prebuilt release for \
             your system. Nothing is missing on your machine - this is a bug in \
             the version you're running. Click Check for updates to get a newer \
             Headroom. If no update is offered, use Contact support below."
        }
        BootstrapFailureKind::Other => {
            "Installation failed: Headroom couldn't download a required file. \
             Check your internet connection, then click Try again. \
             If this keeps happening, contact support at support@extraheadroom.com."
        }
    }
}

/// Report a bootstrap failure to Sentry. If the error chain contains a
/// `CommandFailure`, its full stdout/stderr/exit_code are sent as structured
/// `extra` fields (which Sentry does NOT truncate at the 8KB message cap),
/// so we can actually see why pip/venv failed on the user's machine.
fn capture_bootstrap_failure(err: &anyhow::Error, kind: BootstrapFailureKind) {
    let technical_err = format!("{err:#}");
    let cmd_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::CommandFailure>());

    // Match against stderr (where the real signal lives for CommandFailure)
    // in addition to the error chain. For non-CommandFailure paths the
    // chain is all we have.
    let endpoint_protection_suspected = is_endpoint_protection_signal(&technical_err)
        || cmd_failure
            .map(|f| is_endpoint_protection_signal(&f.stderr))
            .unwrap_or(false);

    // ENOSPC is environmental; skip the Sentry capture (see notes on
    // `capture_upgrade_failure`).
    let disk_full = is_disk_full_signal(&technical_err)
        || cmd_failure
            .map(|f| is_disk_full_signal(&f.stderr))
            .unwrap_or(false);
    if disk_full {
        log::warn!(
            "skipping Sentry capture for bootstrap_failed ({}): disk full (ENOSPC)",
            kind.as_str()
        );
        return;
    }

    // `Other` is the grab-bag, and a grab-bag on its own fingerprint is how
    // RUST-1G reached 123 events that no fix could resolve and no one could
    // triage -- so the only way to quiet it was to archive it, which is why
    // nobody was paged. pip already classifies these more finely than we do
    // (`no-pip`, `missing-file`, ...), so borrow that to split the bucket and
    // let each distinct cause open -- and alert on -- its own issue. The named
    // kinds are already specific; leave their fingerprints alone.
    let other_category = matches!(kind, BootstrapFailureKind::Other)
        .then(|| tool_manager::pip_failure_category(&tool_manager::compact_pip_failure(err)));

    // Transient network/download failures are self-recoverable via the retry
    // button; report them as warnings so they don't pollute the error feed.
    let level = match kind {
        BootstrapFailureKind::NetworkDownload => sentry::Level::Warning,
        _ => sentry::Level::Error,
    };

    if let Some(failure) = cmd_failure {
        sentry::with_scope(
            |scope| {
                let mut fp: Vec<&str> = vec!["bootstrap_failed", kind.as_str()];
                fp.extend(other_category);
                scope.set_fingerprint(Some(fp.as_slice()));
                scope.set_tag("failure_kind", kind.as_str());
                scope.set_tag(
                    "endpoint_protection_suspected",
                    if endpoint_protection_suspected {
                        "true"
                    } else {
                        "false"
                    },
                );
                scope.set_extra("program", failure.program.clone().into());
                scope.set_extra("args", failure.args.join(" ").into());
                scope.set_extra(
                    "exit_code",
                    failure
                        .exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                        .into(),
                );
                scope.set_extra(
                    "signal",
                    failure
                        .signal
                        .map(|s| s.to_string().into())
                        .unwrap_or(serde_json::Value::Null),
                );
                scope.set_extra("stdout", failure.stdout.clone().into());
                scope.set_extra("stderr", failure.stderr.clone().into());
                scope.set_extra("error_chain", technical_err.clone().into());
                if matches!(kind, BootstrapFailureKind::SslLibraryConflict) {
                    // The abort never names the DLL that won. Without this the
                    // next 25 events are as unactionable as the last 25.
                    let dirs = conflicting_openssl_dirs(&std::env::var("PATH").unwrap_or_default());
                    scope.set_extra(
                        "openssl_dirs_on_path",
                        if dirs.is_empty() {
                            "none found on PATH (library was likely injected into the process)"
                                .to_string()
                        } else {
                            dirs.join("; ")
                        }
                        .into(),
                    );
                    // The abort is the keylog FILE* path whenever this is set
                    // (see tool_manager::strip_unusable_sslkeylogfile). Name
                    // only: the value is a user path.
                    scope.set_extra(
                        "sslkeylogfile_set",
                        std::env::vars_os()
                            .any(|(k, v)| {
                                k.to_str()
                                    .is_some_and(|k| k.eq_ignore_ascii_case("SSLKEYLOGFILE"))
                                    && !v.to_string_lossy().trim().is_empty()
                            })
                            .into(),
                    );
                }
            },
            || {
                sentry::capture_message("bootstrap_failed (install_runtime)", level);
            },
        );
    } else {
        sentry::with_scope(
            |scope| {
                let mut fp: Vec<&str> = vec!["bootstrap_failed", kind.as_str()];
                fp.extend(other_category);
                scope.set_fingerprint(Some(fp.as_slice()));
                scope.set_tag("failure_kind", kind.as_str());
                scope.set_tag(
                    "endpoint_protection_suspected",
                    if endpoint_protection_suspected {
                        "true"
                    } else {
                        "false"
                    },
                );
                scope.set_extra("error_chain", technical_err.clone().into());
            },
            || {
                sentry::capture_message(
                    &format!("bootstrap_failed (install_runtime): {technical_err}"),
                    level,
                );
            },
        );
    }
}

/// True when a Headroom proxy startup error chain looks like an environmental
/// port conflict (another process — possibly a stale headroom child — holds
/// the proxy port). Used to route these failures to a separate, rate-limited
/// Sentry fingerprint so the dashboard isn't drowned in non-actionable noise.
pub(crate) fn is_port_conflict_failure(technical_err: &str) -> bool {
    port_conflict::is_port_conflict(technical_err)
        || technical_err.contains("headroom proxy already running on port")
}

/// Report a headroom proxy startup failure to Sentry. If the error chain
/// contains a `HeadroomStartupFailure`, its log tail, log path, and invocation
/// are sent as structured `extra` fields so we can see what Python printed
/// before failing to bind the port.
/// Coarse cause class for a managed-backend start failure, used as the Sentry
/// fingerprint.
///
/// `HeadroomStartupFailure`'s `Display` embeds the program path, the full argv
/// and the log tail, and Sentry groups an un-fingerprinted `capture_message` by
/// its message text. So ONE condition -- the managed backend will not stay up --
/// opened a fresh issue per command line and per exit code, none of them
/// resolvable: RUST-9F/AF/AH/AJ/AK were all this failure wearing different
/// argv. Same split as the pip (RUST-6M/6N/6P) and plugin (RUST-6K) captures.
/// The variable detail still ships as `extra` on every event; only this bounded
/// class reaches the fingerprint.
fn headroom_start_failure_category(reason: &str) -> String {
    if let Some(rest) = reason.strip_prefix("exited with status ") {
        // `ExitStatus` renders as "exit code: 0xffffffff" (Windows),
        // "exit status: 1" or "signal: 6 (SIGABRT)" (unix), followed by
        // " before opening port N". Keep the status -- 0xffffffff is its own
        // bug class, a native DLL init crash (RUST-9F/9T) -- drop the port.
        let status = rest
            .split(" before opening port")
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_start_matches("exit code:")
            .trim_start_matches("exit status:")
            .trim();
        format!("exited-{status}")
    } else if reason.starts_with("wait check failed") {
        "wait-check-failed".to_string()
    } else if reason.starts_with("never opened port") {
        "startup-timeout".to_string()
    } else {
        "other".to_string()
    }
}

pub(crate) fn capture_headroom_start_failure(context: &str, err: &anyhow::Error) {
    let technical_err = format!("{err:#}");

    // Environmental failures: another process holds port 6768, or a stale
    // headroom proxy is still bound. The user gets an actionable hint via
    // `state::classify_startup_error` and the persistent-conflict case is
    // surfaced separately by `port_conflict::note_proxy_failed`. Capture once
    // per session at Warning level under a distinct fingerprint so the
    // dashboard sees real failures (stale child holding the port,
    // sleep/wake race) without drowning in non-actionable noise.
    let is_port_conflict = is_port_conflict_failure(&technical_err);

    let startup_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::HeadroomStartupFailure>());

    let headline = format!("{context}: {technical_err}");
    let truncated = headline.chars().take(400).collect::<String>();

    if is_port_conflict {
        if PORT_CONFLICT_CAPTURED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        sentry::with_scope(
            |scope| {
                let fp: &[&str] = &["proxy_start_port_conflict"];
                scope.set_fingerprint(Some(fp));
                if let Some(failure) = startup_failure {
                    scope.set_extra("program", failure.program.clone().into());
                    scope.set_extra("args", failure.args.join(" ").into());
                    scope.set_extra("log_path", failure.log_path.clone().into());
                    scope.set_extra("log_tail", failure.log_tail.clone().into());
                    scope.set_extra("reason", failure.reason.clone().into());
                }
                scope.set_extra("error_chain", technical_err.clone().into());
            },
            || {
                sentry::capture_message(&truncated, sentry::Level::Warning);
            },
        );
        return;
    }

    // Windows Application Control (Smart App Control / WDAC / AppLocker), an
    // EDR agent, or Gatekeeper refusing to execute the runtime we just
    // installed. Two things were wrong with letting this fall through:
    //
    //   - Level::Error, for a machine-policy verdict no release we ship can
    //     change. `classify_startup_error` already turns it into an actionable
    //     hint for the user; Sentry is not where that gets fixed.
    //   - No fingerprint, so Sentry grouped on the message -- which embeds the
    //     full proxy command line, including the port and the user's home
    //     path. One cause, one issue per machine: RUST-AD and RUST-AC are the
    //     same Windows host's same App Control block, filed twice because the
    //     two call sites prefix it differently.
    //
    // Once per session at Warning under a stable fingerprint, so the cohort
    // stays countable without either splintering or drowning the dashboard.
    if is_endpoint_protection_signal(&technical_err) {
        if ENDPOINT_PROTECTION_CAPTURED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        sentry::with_scope(
            |scope| {
                let fp: &[&str] = &["proxy_start_endpoint_protection"];
                scope.set_fingerprint(Some(fp));
                if let Some(failure) = startup_failure {
                    scope.set_extra("program", failure.program.clone().into());
                    scope.set_extra("args", failure.args.join(" ").into());
                    scope.set_extra("log_path", failure.log_path.clone().into());
                    scope.set_extra("log_tail", failure.log_tail.clone().into());
                    scope.set_extra("reason", failure.reason.clone().into());
                }
                scope.set_extra("error_chain", technical_err.clone().into());
            },
            || {
                sentry::capture_message(
                    "headroom runtime blocked by endpoint protection at start",
                    sentry::Level::Warning,
                );
            },
        );
        return;
    }

    if let Some(failure) = startup_failure {
        let category = headroom_start_failure_category(&failure.reason);
        sentry::with_scope(
            |scope| {
                // `context` is a bounded set of call sites and keeps the
                // launch/tray lifecycles apart; `category` is the cause class.
                // Neither can fragment, unlike the argv in the message text.
                scope.set_fingerprint(Some(&["headroom-start-failed", context, &category]));
                // Who signalled a child that died before binding: our own
                // recent kills, oldest first, or empty when we sent none.
                scope.set_extra(
                    "recent_app_kills",
                    state::recent_app_kills_summary().join("\n").into(),
                );
                scope.set_extra("program", failure.program.clone().into());
                scope.set_extra("args", failure.args.join(" ").into());
                scope.set_extra("log_path", failure.log_path.clone().into());
                scope.set_extra("log_tail", failure.log_tail.clone().into());
                scope.set_extra("reason", failure.reason.clone().into());
                scope.set_extra("error_chain", technical_err.clone().into());
            },
            || {
                sentry::capture_message(&truncated, sentry::Level::Error);
            },
        );
    } else {
        sentry::capture_message(&truncated, sentry::Level::Error);
    }
}

/// Pure payload for `capture_watchdog_give_up`. Built before any Sentry side
/// effects so it can be unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchdogGiveUpReport {
    pub message: String,
    pub tracked_child_exit_status: String,
    pub bypass_active: bool,
    pub runtime_upgrade_in_progress: bool,
    pub consecutive_failures: u32,
    pub log_tail: Option<String>,
    /// Last error returned by `ensure_headroom_running` during this down
    /// episode, if any. Distinguishes "spawn keeps erroring" (Some) from
    /// "spawn returned Ok but `/readyz` never came back" (None) — the two
    /// failure modes look identical without this field.
    pub last_startup_error: Option<String>,
    /// PID of the tracked Python child at give-up time, if we own a Child
    /// handle. Useful for ad-hoc correlation with external `ps`/Activity
    /// Monitor snapshots the user can attach to a bug report.
    pub tracked_pid: Option<u32>,
    /// Whether the backend loopback port still accepts a TCP connection.
    /// Distinguishes "process gone, port closed" (false) from "process
    /// alive but event loop wedged" (true) — the kernel completes
    /// `accept()` even when uvicorn can't service HTTP. See
    /// `state::tcp_port_accepts_connection` for full semantics.
    pub port_accepts_tcp: bool,
    /// Accumulated CPU seconds for the tracked PID at give-up time.
    /// None when no tracked child or `ps` failed. Combined with
    /// `log_silent_secs`, lets us see whether the child was burning CPU
    /// silently (sync compute) vs idle/blocked (deadlock, await never
    /// resolving).
    pub process_cpu_secs: Option<u64>,
    /// Seconds since the newest `headroom-proxy*.log` file was last
    /// modified. None when there is no proxy log on disk yet, or the
    /// mtime is in the future (clock skew).
    pub log_silent_secs: Option<u64>,
    /// Outcome of probing `/readyz` directly on the backend port at
    /// give-up time. Disambiguates intercept-layer failures (intercept
    /// fails, backend `ok`) from Python-layer failures (both fail).
    /// One of: `ok`, `timeout`, `refused`, `http_<status>`, `error: <msg>`.
    pub backend_readyz_outcome: String,
}

/// Coarse bucket for `backend_readyz_outcome` used in the Sentry fingerprint.
/// Deliberately drops the high-cardinality tails (`http_503:<checks>`,
/// `error: <msg>`) to a stable category so the issue splits by failure *shape*
/// (dead port vs wedged vs failing-check vs intercept-layer) without
/// re-fragmenting per machine.
pub(crate) fn readyz_outcome_fingerprint_key(outcome: &str) -> &'static str {
    if outcome == "ok" {
        // Backend healthy -> the fault is in the Rust intercept layer, not Python.
        "readyz_ok"
    } else if outcome == "timeout" {
        "readyz_timeout"
    } else if outcome == "refused" {
        "readyz_refused"
    } else if outcome.starts_with("http_503") {
        "readyz_503"
    } else if outcome.starts_with("http_") {
        "readyz_http_other"
    } else {
        "readyz_error"
    }
}

/// Fingerprint bucket for the tracked child's state at give-up.
/// `"still_alive_or_untracked"` (exit_status None) hides two opposite cases:
/// a child genuinely mid-boot (has a pid) vs no tracked child at all
/// (pid None — backend absent, we hold no handle). They want different fixes,
/// so keep them in separate Sentry issues. See RUST-53.
pub(crate) fn child_state_fingerprint_key(
    exit_status: &str,
    tracked_pid: Option<u32>,
) -> &'static str {
    if exit_status != "still_alive_or_untracked" {
        "child_exited"
    } else if tracked_pid.is_some() {
        "child_alive"
    } else {
        "child_untracked"
    }
}

/// Extra fingerprint bucket for a give-up whose `last_startup_error` names a
/// cause with its own remedy. `None` for the ordinary case (no startup error,
/// or one we cannot classify), so the fingerprint those events already carry
/// is untouched.
pub(crate) fn startup_error_fingerprint_key(
    last_startup_error: Option<&str>,
) -> Option<&'static str> {
    let err = last_startup_error?;
    if is_endpoint_protection_signal(err) {
        Some("startup_endpoint_protection")
    } else if is_port_conflict_failure(err) {
        Some("startup_port_conflict")
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_watchdog_give_up_report(
    consecutive_failures: u32,
    bypass_active: bool,
    runtime_upgrade_in_progress: bool,
    exit_status: Option<String>,
    log_tail: Option<String>,
    last_startup_error: Option<String>,
    tracked_pid: Option<u32>,
    port_accepts_tcp: bool,
    process_cpu_secs: Option<u64>,
    log_silent_secs: Option<u64>,
    backend_readyz_outcome: String,
) -> WatchdogGiveUpReport {
    WatchdogGiveUpReport {
        message: format!(
            "proxy_unreachable_post_boot (auto_paused after {consecutive_failures} failures)"
        ),
        tracked_child_exit_status: exit_status
            .unwrap_or_else(|| "still_alive_or_untracked".to_string()),
        bypass_active,
        runtime_upgrade_in_progress,
        consecutive_failures,
        log_tail: log_tail.filter(|s| !s.is_empty()),
        last_startup_error: last_startup_error.filter(|s| !s.is_empty()),
        tracked_pid,
        port_accepts_tcp,
        process_cpu_secs,
        log_silent_secs,
        backend_readyz_outcome,
    }
}

/// Probe `/readyz` on the backend port directly (bypassing the Rust intercept
/// on 6767) and classify the outcome, also returning the raw response body
/// (truncated) for non-2xx responses so the give-up Sentry event carries the
/// backend's own per-check breakdown instead of just our classification.
/// The watchdog uses a 5s budget so a backend that's merely slow under heavy
/// compression load isn't mistaken for a dead one.
fn probe_backend_readyz_with_body(timeout: std::time::Duration) -> (String, Option<String>) {
    let port = crate::backend_port::get();
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(err) => return (format!("error: {err}"), None),
    };
    let url = format!("http://127.0.0.1:{port}/readyz");
    match client.get(&url).send() {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                ("ok".to_string(), None)
            } else if status.as_u16() == 503 {
                // 503 = readiness failure: the process is alive and answering,
                // but a component check is false. Parse the body's per-check
                // breakdown so the watchdog can tell a transient upstream blip
                // (`http_503:upstream`) apart from a wedged core component and
                // route them differently. Falls back to bare "http_503" when the
                // body can't be read or parsed.
                match response.text() {
                    Ok(body) => {
                        let snippet: String = body.chars().take(500).collect();
                        let outcome = match serde_json::from_str::<serde_json::Value>(&body) {
                            Ok(json) => {
                                let csv = readyz_failed_checks_csv(&json);
                                if csv.is_empty() {
                                    "http_503".to_string()
                                } else {
                                    format!("http_503:{csv}")
                                }
                            }
                            Err(_) => "http_503".to_string(),
                        };
                        (outcome, Some(snippet))
                    }
                    Err(_) => ("http_503".to_string(), None),
                }
            } else {
                (format!("http_{}", status.as_u16()), None)
            }
        }
        Err(err) => {
            if err.is_timeout() {
                ("timeout".to_string(), None)
            } else if err.is_connect() {
                ("refused".to_string(), None)
            } else {
                (format!("error: {err}"), None)
            }
        }
    }
}

fn probe_backend_readyz_outcome_with_timeout(timeout: std::time::Duration) -> String {
    probe_backend_readyz_with_body(timeout).0
}

/// One retry on a bare `http_503`. Bare means the status line arrived but the
/// body couldn't be read or parsed within budget — evidence of load, not of a
/// wedged core. Without the retry, an upstream-only 503 whose body read ran
/// past the deadline was classified wedged-core and got a healthy process
/// force-killed (Sentry RUST-2X: bare http_503, port accepting TCP, low CPU).
/// A second bare result stands: two failed body reads in ~10s is itself a
/// wedge signal.
fn classify_backend_readyz(
    mut probe: impl FnMut() -> (String, Option<String>),
) -> (String, Option<String>) {
    let first = probe();
    if first.0 == "http_503" {
        return probe();
    }
    first
}

/// Comma-joined, sorted names of the unhealthy components in a `/readyz`
/// payload — those whose `checks.<name>.ready` is `false`. Empty when the body
/// has no `checks` object or every check is ready.
///
/// Soft components (`"optional": true`) are excluded: the backend itself drops
/// them from its own readiness verdict, so they never cause the 503 and must
/// not shape our diagnosis of it. Kompress is the one that matters — its model
/// downloads lazily, so on installs without the ML extras it reports
/// `ready: false` forever. Left in, it rode along on every sleep-wake
/// `upstream` blip and turned `http_503:upstream` into
/// `http_503:kompress,upstream`, defeating `readyz_failure_is_upstream_only`
/// and auto-pausing 10 healthy users (Sentry RUST-5E). Matched by name too,
/// because backends before ~0.33 emit no `optional` flag.
pub(crate) fn readyz_failed_checks_csv(body: &serde_json::Value) -> String {
    let Some(checks) = body.get("checks").and_then(|c| c.as_object()) else {
        return String::new();
    };
    let mut failed: Vec<&str> = checks
        .iter()
        .filter(|(_, v)| v.get("ready").and_then(|r| r.as_bool()) == Some(false))
        .filter(|(name, v)| {
            name.as_str() != "kompress" && v.get("optional").and_then(|o| o.as_bool()) != Some(true)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    failed.sort_unstable();
    failed.join(",")
}

/// Failing-check names parsed out of a `http_503:<a>,<b>` outcome string.
/// `None` for any other outcome (including a bare `http_503` whose body
/// couldn't be parsed), so callers treat unknown 503s as the conservative
/// give-up default.
fn parse_readyz_failed_checks(outcome: &str) -> Option<Vec<&str>> {
    outcome
        .strip_prefix("http_503:")
        .map(|rest| rest.split(',').filter(|s| !s.is_empty()).collect())
}

/// True when `/readyz` returned 503 and the *only* unhealthy component is the
/// upstream-connectivity probe. The proxy process is healthy; this is a
/// transient network/upstream blip (the upstream check is cached 30s) that
/// self-heals on the next refresh. Tearing Python down and bypassing routes to
/// the same unreachable upstream, so it buys nothing.
fn readyz_failure_is_upstream_only(outcome: &str) -> bool {
    matches!(parse_readyz_failed_checks(outcome), Some(checks) if checks == ["upstream"])
}

/// True when `/readyz` returned 503 with at least one *core* component
/// (startup, http_client, cache, rate_limiter, memory) unhealthy — a wedged
/// backend that a restart may clear, distinct from a pure upstream blip.
fn readyz_failure_has_core_unhealthy(outcome: &str) -> bool {
    parse_readyz_failed_checks(outcome)
        .map(|checks| checks.iter().any(|c| *c != "upstream"))
        .unwrap_or(false)
}

/// Whether two cumulative CPU samples (`ps -o time=`, whole seconds) taken
/// `elapsed_secs` apart represent a process actively burning CPU. Uses the
/// *rate*, not the delta: `ps` reports whole seconds, so a single incidental
/// tick at a second boundary reads as +1, which over a short window looks like
/// activity. Require >0.5 CPU-sec/sec so a real spin (~1.0) passes while a lone
/// boundary tick (~0.25 over a ~4s window) does not.
fn cpu_rate_indicates_burn(before: u64, after: u64, elapsed_secs: f64) -> bool {
    elapsed_secs > 0.0 && (after.saturating_sub(before) as f64) / elapsed_secs > 0.5
}

/// Capture once per "down episode" when the watchdog gives up on restarting
/// the proxy. Fires before stop_headroom tears down the tracked child handle
/// and proxy log, so the payload reflects the failure we're recovering from.
///
/// `backend_readyz_outcome` is probed by the watchdog before deciding to give
/// up (so the rescue path can inspect it) and threaded through here to avoid
/// a second probe.
fn capture_watchdog_give_up(
    state: &AppState,
    consecutive_failures: u32,
    bypass_active: bool,
    backend_readyz_outcome: String,
    // Raw (truncated) /readyz response body, when one was readable.
    readyz_body: Option<String>,
    // (secs since the last wall-clock jump was observed, jump size in secs).
    // Present when the machine slept at some point this app session — lets
    // triage tell post-wake episodes apart from genuine wedges.
    wall_jump: Option<(u64, u64)>,
) {
    if WATCHDOG_DOWN_CAPTURED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let exit_status = state.headroom_process_exited();
    let upgrade_in_progress = state.runtime_upgrade_in_progress();
    let logs_dir = state.tool_manager.logs_dir();
    let log_tail = tool_manager::newest_proxy_log_path(&logs_dir)
        .map(|path| tool_manager::tail_log_file(&path, 100));
    let last_startup_error = state.last_startup_error.lock().clone();

    let tracked_pid: Option<u32> = state
        .headroom_process
        .lock()
        .as_ref()
        .map(|child| child.id());
    let port_accepts_tcp = crate::state::proxy_port_accepts_connection();
    let process_cpu_secs = tracked_pid.and_then(crate::state::tracked_process_cpu_time_secs);
    // CPU *rate*, not cumulative. `process_cpu_secs` is lifetime CPU
    // (`ps -o time=`); any long-lived-but-now-idle process carries a large
    // cumulative value, so using it as a deadlock proxy mislabels a healthy
    // idle process as a deadlock (Sentry proxy_unreachable_post_boot showed 12s
    // cumulative + 28min silent flagged as Error). Re-sample over a ~4s window
    // and defer the rate judgement to `cpu_rate_indicates_burn`.
    let cpu_actively_burning = match (tracked_pid, process_cpu_secs) {
        (Some(pid), Some(before)) => {
            let started = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_secs(4));
            let elapsed = started.elapsed().as_secs_f64();
            crate::state::tracked_process_cpu_time_secs(pid)
                .map(|after| cpu_rate_indicates_burn(before, after, elapsed))
                .unwrap_or(false)
        }
        _ => false,
    };
    let log_silent_secs = crate::state::newest_proxy_log_mtime(&logs_dir).and_then(|mtime| {
        std::time::SystemTime::now()
            .duration_since(mtime)
            .ok()
            .map(|d| d.as_secs())
    });

    let report = build_watchdog_give_up_report(
        consecutive_failures,
        bypass_active,
        upgrade_in_progress,
        exit_status,
        log_tail,
        last_startup_error,
        tracked_pid,
        port_accepts_tcp,
        process_cpu_secs,
        log_silent_secs,
        backend_readyz_outcome,
    );

    // Default to Warning: give-up is the documented recovery path, not a
    // bug. Escalate to Error only when there's a real signal something is
    // stuck — spawn keeps erroring, or the child is alive and *actively*
    // burning CPU (likely deadlock) while the log has gone quiet. Plain
    // network/restart blips stay at Warning so they don't pollute the Error
    // inbox.
    let cpu_deadlock_signal = cpu_actively_burning && report.log_silent_secs.unwrap_or(0) >= 30;
    // A spawn that keeps failing is an Error -- unless the machine's own
    // security policy is what keeps failing it. That verdict is already
    // reported once per session by `capture_headroom_start_failure`, has a
    // user-facing remedy, and cannot be changed by anything we ship; filing
    // the give-up that follows it as an Error just re-reports the same block
    // (RUST-5C's latest events were one Spanish Windows host whose `_sqlite3`
    // was blocked, escalated to Error three times over).
    let startup_key = startup_error_fingerprint_key(report.last_startup_error.as_deref());
    let startup_is_policy = startup_key == Some("startup_endpoint_protection");
    let level =
        if (report.last_startup_error.is_some() && !startup_is_policy) || cpu_deadlock_signal {
            sentry::Level::Error
        } else {
            sentry::Level::Warning
        };

    sentry::with_scope(
        |scope| {
            // Split the grab-bag: one flat fingerprint collapsed every distinct
            // failure mode (port refused vs alive-but-503 vs intercept-layer)
            // across every release into a single issue that can never be
            // resolved — a sibling shape always reappears. Key on the coarse
            // readyz classification and whether the child is still alive so each
            // genuinely-different wedge gets its own issue and lifecycle.
            let readyz_key = readyz_outcome_fingerprint_key(&report.backend_readyz_outcome);
            let child_key =
                child_state_fingerprint_key(&report.tracked_child_exit_status, report.tracked_pid);
            // A recognised startup cause gets its own issue: an endpoint
            // protection block and a port conflict are not the wedge the
            // readyz/child keys describe, and each has its own lifecycle.
            // Absent (the common case) the fingerprint is unchanged, so the
            // existing issues keep their history.
            let mut fp: Vec<&str> = vec!["proxy_unreachable_post_boot", readyz_key, child_key];
            if let Some(key) = startup_key {
                fp.push(key);
            }
            scope.set_fingerprint(Some(fp.as_slice()));
            scope.set_extra(
                "tracked_child_exit_status",
                report.tracked_child_exit_status.clone().into(),
            );
            scope.set_extra("bypass_active", report.bypass_active.into());
            scope.set_extra(
                "runtime_upgrade_in_progress",
                report.runtime_upgrade_in_progress.into(),
            );
            scope.set_extra(
                "consecutive_failures",
                (report.consecutive_failures as i64).into(),
            );
            if let Some(tail) = &report.log_tail {
                scope.set_extra("proxy_log_tail", tail.clone().into());
            }
            if let Some(err) = &report.last_startup_error {
                scope.set_extra("last_startup_error", err.clone().into());
            }
            if let Some(pid) = report.tracked_pid {
                scope.set_extra("tracked_pid", (pid as i64).into());
            }
            scope.set_extra("port_accepts_tcp", report.port_accepts_tcp.into());
            if let Some(cpu) = report.process_cpu_secs {
                scope.set_extra("process_cpu_secs", (cpu as i64).into());
            }
            if let Some(silent) = report.log_silent_secs {
                scope.set_extra("log_silent_secs", (silent as i64).into());
            }
            scope.set_extra(
                "backend_readyz_outcome",
                report.backend_readyz_outcome.clone().into(),
            );
            if let Some(body) = &readyz_body {
                scope.set_extra("readyz_body", body.clone().into());
            }
            if let Some((since, gap)) = wall_jump {
                scope.set_extra("secs_since_wall_jump", (since as i64).into());
                scope.set_extra("wall_jump_gap_secs", (gap as i64).into());
            }
        },
        || {
            sentry::capture_message(&report.message, level);
        },
    );
}

/// Diagnostic snapshot taken at the moment a boot-validation failure is
/// captured. Distinguishes "the new proxy never spawned" (tracked_child=false)
/// from "spawned but crashed before writing logs" (no new log) from "spawned
/// and bound but unreachable" (port_bound=true, log written, /livez never
/// answered). None for install-phase failures where no proxy launch happened.
///
/// When `tracked_child` is false, the secondary fields below identify which
/// `ensure_headroom_running` short-circuit fired or whether the spawn errored
/// outright — without these, every "Stalled" / "NotStarted" event looks
/// identical in Sentry.
#[derive(Default, Clone)]
pub(crate) struct UpgradeBootDiagnostics {
    pub tracked_child: bool,
    pub new_proxy_log_written: bool,
    pub proxy_port_bound: bool,
    pub python_installed: bool,
    pub proxy_bypass: bool,
    pub pricing_allows_optimization: bool,
    pub runtime_paused: bool,
    /// Who actually held the backend port at failure time: `free`,
    /// `headroom`, or `foreign: <cmd> pid <n>`. `proxy_port_bound` alone
    /// cannot separate a wedged child of ours from a foreign squatter.
    pub port_occupant: String,
    pub ensure_error: Option<String>,
    /// Last ~100 lines of pip stdout/stderr from the install pass that
    /// produced the venv we're now booting. Pip can return exit 0 while
    /// leaving the venv broken (skipped packages, ABI-mismatched native
    /// deps); this tail is the only forensic record of what pip actually
    /// did. Empty string when no pip ran (e.g. requirements-repair).
    pub pip_output_tail: String,
}

/// Report a runtime upgrade failure to Sentry. `phase` is "install" for
/// pip/smoke-test failures, "boot_validation" for "installed but didn't boot".
/// `outcome` is the BootValidationOutcome label when phase is boot_validation.
pub(crate) fn capture_upgrade_failure(
    err: &anyhow::Error,
    restored: bool,
    phase: &str,
    outcome: Option<&str>,
    duration_ms: Option<u64>,
    target_version: Option<&str>,
    fallback_version: Option<&str>,
    log_tail: Option<&str>,
    boot_diagnostics: Option<UpgradeBootDiagnostics>,
) {
    let technical_err = format!("{err:#}");
    let cmd_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::CommandFailure>());

    // ENOSPC is environmental — the user can't fix it by retrying, and the
    // pip log dump bloats Sentry with thousands of "Requirement already
    // satisfied" lines per report. Drop the Sentry capture; the user still
    // sees the disk-full hint via `classify_upgrade_error`, and the local
    // failure is recorded by the caller's `record_upgrade_failure` +
    // analytics::track_event.
    let cmd_stderr = cmd_failure.map(|f| f.stderr.as_str()).unwrap_or("");
    if is_disk_full_signal(&technical_err) || is_disk_full_signal(cmd_stderr) {
        log::warn!(
            "skipping Sentry capture for runtime_upgrade_failed ({phase}): disk full (ENOSPC)"
        );
        return;
    }

    // Sentry drops extras larger than ~16KB. Cap the tail aggressively so the
    // tail's tail (where the panic/error usually lives) survives.
    let log_tail_capped = log_tail.map(|s| tail_bytes_for_sentry(s, SENTRY_EXTRA_TAIL_BYTES));

    let outcome_for_fingerprint = outcome.unwrap_or("none");
    let fingerprint: [&str; 3] = ["runtime_upgrade", phase, outcome_for_fingerprint];

    // Bake diagnostic fields into the message so they appear in the issue
    // title/preview without requiring a drill-down into tags. The first ~400
    // chars of the err chain are usually enough to disambiguate.
    let mut summary = format!("runtime_upgrade_failed ({phase})");
    if let Some(o) = outcome {
        summary.push_str(&format!(" outcome={o}"));
    }
    if let Some(d) = duration_ms {
        summary.push_str(&format!(" duration_ms={d}"));
    }
    let err_capped: String = technical_err.chars().take(400).collect();
    summary.push_str(&format!(" err={err_capped}"));

    let endpoint_protection_suspected = is_endpoint_protection_signal(&technical_err);

    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "runtime_upgrade");
            scope.set_tag("upgrade_phase", phase);
            scope.set_tag(
                "endpoint_protection_suspected",
                if endpoint_protection_suspected {
                    "true"
                } else {
                    "false"
                },
            );
            if let Some(o) = outcome {
                scope.set_tag("outcome", o);
            }
            if let Some(t) = target_version {
                scope.set_tag("target_version", t);
            }
            if let Some(f) = fallback_version {
                scope.set_tag("fallback_version", f);
            }
            scope.set_extra("rollback_restored", restored.into());
            scope.set_extra("error_chain", technical_err.clone().into());
            if let Some(d) = duration_ms {
                scope.set_extra("duration_ms", d.into());
            }
            if let Some(tail) = log_tail_capped.as_deref() {
                scope.set_extra("log_tail", tail.into());
            }
            if let Some(diag) = boot_diagnostics.as_ref() {
                scope.set_tag(
                    "tracked_child",
                    if diag.tracked_child { "true" } else { "false" },
                );
                scope.set_tag(
                    "new_proxy_log_written",
                    if diag.new_proxy_log_written {
                        "true"
                    } else {
                        "false"
                    },
                );
                scope.set_tag(
                    "proxy_port_bound",
                    if diag.proxy_port_bound {
                        "true"
                    } else {
                        "false"
                    },
                );
                scope.set_extra("tracked_child", diag.tracked_child.into());
                scope.set_extra("new_proxy_log_written", diag.new_proxy_log_written.into());
                scope.set_extra("proxy_port_bound", diag.proxy_port_bound.into());
                scope.set_extra("python_installed", diag.python_installed.into());
                scope.set_extra("proxy_bypass", diag.proxy_bypass.into());
                scope.set_extra(
                    "pricing_allows_optimization",
                    diag.pricing_allows_optimization.into(),
                );
                scope.set_extra("runtime_paused", diag.runtime_paused.into());
                if !diag.port_occupant.is_empty() {
                    // Tag on the kind only: the detail carries a pid, which
                    // would explode tag cardinality.
                    scope.set_tag(
                        "port_occupant",
                        diag.port_occupant
                            .split(':')
                            .next()
                            .unwrap_or("unknown")
                            .to_string(),
                    );
                    scope.set_extra("port_occupant", diag.port_occupant.clone().into());
                }
                if let Some(err) = diag.ensure_error.as_deref() {
                    scope.set_extra("ensure_headroom_running_error", err.into());
                }
                if !diag.pip_output_tail.is_empty() {
                    // Cap aggressively — Sentry drops extras > ~16KB and the
                    // tail (where pip warnings/skips/successfully-installed
                    // lines live) is the most informative part.
                    let tail =
                        tail_bytes_for_sentry(&diag.pip_output_tail, SENTRY_EXTRA_TAIL_BYTES);
                    scope.set_extra("pip_install_output", tail.into());
                }
            }
            if let Some(failure) = cmd_failure {
                scope.set_extra("program", failure.program.clone().into());
                scope.set_extra("args", failure.args.join(" ").into());
                scope.set_extra(
                    "exit_code",
                    failure
                        .exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                        .into(),
                );
                scope.set_extra(
                    "signal",
                    failure
                        .signal
                        .map(|s| s.to_string().into())
                        .unwrap_or(serde_json::Value::Null),
                );
                scope.set_extra("stdout", failure.stdout.clone().into());
                scope.set_extra("stderr", failure.stderr.clone().into());
            }
            scope.set_fingerprint(Some(fingerprint.as_slice()));
        },
        || {
            // Build the anyhow chain as exception values. With at least one
            // exception present, the AttachStacktraceIntegration attaches the
            // stacktrace to the exception rather than emitting a synthetic
            // thread frame full of sentry/backtrace internals.
            let mut exception_values: Vec<sentry::protocol::Exception> = err
                .chain()
                .map(|e| sentry::protocol::Exception {
                    ty: "anyhow::Error".to_string(),
                    value: Some(e.to_string()),
                    ..Default::default()
                })
                .collect();
            // Sentry convention: innermost cause first.
            exception_values.reverse();

            let event = sentry::protocol::Event {
                message: Some(summary.clone()),
                level: sentry::protocol::Level::Error,
                exception: exception_values.into(),
                ..Default::default()
            };
            sentry::capture_event(event);
        },
    );
}

/// High-confidence signatures that an install/runtime failure was caused by
/// endpoint-protection software (antivirus or EDR) blocking the freshly
/// installed native code. Conservative on purpose — we only match patterns
/// that are unlikely to surface from anything else, so the user-facing hint
/// stays trustworthy. If the matcher grows past ~6 patterns we should split
/// it by failure surface (install vs runtime) and consider tightening.
///
/// Input is matched case-insensitively.
pub(crate) fn is_endpoint_protection_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Apple's loader rejecting a fresh signature (codesign tampered or not
    // recognized by the kernel — almost always EDR injecting/rewriting).
    if lower.contains("code signature invalid")
        || lower.contains("code signature could not be verified")
    {
        return true;
    }
    // `dlopen` reports the "tried: ... (operation not permitted)" suffix when
    // a sandbox/AV blocks a freshly-extracted .so/.dylib. The "library not
    // loaded" prefix alone is too noisy (covers ordinary missing-dep cases),
    // so require the "not permitted" companion.
    if (lower.contains("library not loaded") || lower.contains("dlopen"))
        && lower.contains("not permitted")
    {
        return true;
    }
    // SIGKILL with no app-side cause is the classic EDR signature — the
    // process is killed before it can write a useful error. Plain "killed"
    // is too noisy (covers OOM, user pkill), so require the explicit signal
    // marker. CommandFailure formats this as "signal=9" or "Killed: 9".
    if lower.contains("signal=9") || lower.contains("killed: 9") || lower.contains("exit code 137")
    {
        return true;
    }
    // `Operation not permitted` paired with a freshly-installed native
    // extension path strongly implicates AV that hooks open(2)/exec(2). The
    // bare phrase appears in too many unrelated permission errors, so we
    // gate it on "site-packages" (where pip just wrote the file) or ".so" /
    // ".dylib" appearing in the same chain.
    if lower.contains("operation not permitted")
        && (lower.contains("site-packages") || lower.contains(".so") || lower.contains(".dylib"))
    {
        return true;
    }
    // A Windows Application Control verdict (Smart App Control / WDAC /
    // AppLocker) is definitionally endpoint protection blocking fresh code.
    if is_app_control_signal(&lower) {
        return true;
    }
    // The same verdict seen from inside Python, where the OS prose is
    // localized and carries no code (RUST-BB/BA/5C).
    if is_blocked_runtime_dll_signal(&lower) {
        return true;
    }
    // The RUST-9F probe (tool_manager::probe_onnx_import) importing
    // onnxruntime in a bare interpreter and hitting its 15s kill: a native
    // DLL that neither loads nor fails is being scanned or held by something
    // outside the process. First seen on a corporate-asset Windows 11 host
    // (RUST-C7) whose backend had also aborted while sklearn pre-loaded its
    // OpenMP DLL. The probe only runs after a 0xffffffff exit, and "killed"
    // only comes from the timeout (Windows has no signals), so the phrase is
    // specific. An `(exit N)` verdict is a broken venv, not this.
    if lower.contains("import onnxruntime failed (killed)") {
        return true;
    }
    false
}

/// True when an install/upgrade failure was caused by the user's disk
/// running out of space. ENOSPC is environmental — the user can't fix it
/// by retrying, only by freeing space — so we use this to drop noisy
/// pip-log Sentry reports and emit a single clear local log line instead.
/// The user-facing hint is produced separately by `classify_upgrade_error`.
pub(crate) fn is_disk_full_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no space left on device")
        || lower.contains("errno 28")
        || lower.contains("enospc")
        || lower.contains("disk full")
}

/// Shared hint copy for endpoint-protection failures. Two variants because
/// the install-time and runtime surfaces want slightly different "what to
/// do" wording (retry the install vs allow the runtime dir + click Retry).
const ENDPOINT_PROTECTION_HINT_INSTALL: &str =
    "Looks like endpoint protection (antivirus or EDR) blocked the new native code. \
     Allow Headroom in your security software, then retry.";

const ENDPOINT_PROTECTION_HINT_RUNTIME: &str =
    "A Headroom component was killed at launch — usually endpoint protection (antivirus or EDR) \
     interfering with freshly-installed code. Allow `~/Library/Application Support/Headroom` \
     in your security software, then click Retry.";

/// The Windows wording: the block there is almost always Application Control
/// (Smart App Control on a personal PC, WDAC or AppLocker on a managed one),
/// which has a specific place to look and a specific folder to allow. The
/// macOS text above named a `~/Library` path to Windows users (RUST-BB).
const ENDPOINT_PROTECTION_HINT_RUNTIME_WINDOWS: &str =
    "Windows blocked part of Headroom's runtime from loading — usually Application Control \
     (Smart App Control, AppLocker, or a company WDAC policy) or antivirus / endpoint protection \
     acting on freshly-installed files. On a personal PC, check Windows Security > App & browser control; \
     on a work PC, ask IT to allow %LOCALAPPDATA%\\Headroom. Then click Retry. If nothing is \
     blocking it, reinstall the runtime from Settings > Advanced.";

pub(crate) fn endpoint_protection_hint_install() -> String {
    ENDPOINT_PROTECTION_HINT_INSTALL.to_string()
}

pub(crate) fn endpoint_protection_hint_runtime() -> String {
    if cfg!(windows) {
        ENDPOINT_PROTECTION_HINT_RUNTIME_WINDOWS.to_string()
    } else {
        ENDPOINT_PROTECTION_HINT_RUNTIME.to_string()
    }
}

/// Map common runtime-upgrade failure modes to a short user-facing hint.
pub(crate) fn classify_upgrade_error(err: &anyhow::Error) -> Option<String> {
    let chain_raw = format!("{err:#}");
    // Endpoint protection check uses the raw chain (the matcher does its own
    // case-folding) so signal patterns like "signal=9" match exactly.
    if is_endpoint_protection_signal(&chain_raw) {
        return Some(endpoint_protection_hint_install());
    }
    let chain = chain_raw.to_ascii_lowercase();
    if chain.contains("network")
        || chain.contains("timed out")
        || chain.contains("dns")
        || chain.contains("connection refused")
        || chain.contains("could not resolve")
    {
        return Some("Couldn't reach PyPI. Check your network and retry.".into());
    }
    if chain.contains("no space") || chain.contains("disk full") || chain.contains("enospc") {
        return Some(
            "Not enough disk space to install the update. Free up space and retry.".into(),
        );
    }
    if chain.contains("sha256") || chain.contains("checksum") || chain.contains("digest") {
        return Some("The downloaded wheel's checksum didn't match. Retry to redownload.".into());
    }
    if chain.contains("import") && chain.contains("smoke test") {
        return Some(
            "The new Headroom version couldn't be imported. Try retrying or reinstalling.".into(),
        );
    }
    if chain.contains("resolution") || chain.contains("no matching distribution") {
        return Some(
            "Pip couldn't resolve dependencies for the new version. Please report this.".into(),
        );
    }
    None
}

#[tauri::command]
fn get_bootstrap_progress(state: State<'_, AppState>) -> BootstrapProgress {
    state.bootstrap_progress()
}

/// Cause class + technical detail of the last bootstrap failure, so the
/// install screen's "Contact support" mail carries something we can act on.
/// `None` when no bootstrap has failed this session.
#[tauri::command]
fn get_bootstrap_failure_report(state: State<'_, AppState>) -> Option<BootstrapFailureReport> {
    state.bootstrap_failure_report.lock().clone()
}

#[tauri::command]
fn get_runtime_upgrade_progress(state: State<'_, AppState>) -> RuntimeUpgradeProgress {
    state.runtime_upgrade_progress()
}

#[tauri::command]
fn retry_runtime_upgrade(app: AppHandle) -> Result<(), String> {
    let app_clone = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_clone.state();
        state.retry_runtime_upgrade(&app_clone, false);
    });
    Ok(())
}

/// User-initiated recovery path. Same flow as `retry_runtime_upgrade` but
/// skips the in-place upgrade attempt and goes straight to atomic rebuild.
/// Surfaced as the "Retry with full rebuild" button on a boot-validation
/// failure: the in-place pip succeeded (smoke test passed) but the proxy
/// never booted, which usually means stale native libs from the previous
/// pin survived the upgrade. The rebuild path nukes the venv and starts
/// fresh, fixing the broken state at the cost of re-downloading wheels.
#[tauri::command]
fn retry_runtime_upgrade_with_rebuild(app: AppHandle) -> Result<(), String> {
    let app_clone = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_clone.state();
        state.retry_runtime_upgrade(&app_clone, true);
    });
    Ok(())
}

#[tauri::command]
fn dismiss_runtime_upgrade_failure(state: State<'_, AppState>) -> Result<(), String> {
    state.dismiss_upgrade_failure();
    Ok(())
}

#[tauri::command]
async fn get_runtime_status(app: AppHandle) -> Result<RuntimeStatus, String> {
    // Off the main thread: a cache miss inside runtime_status() does a
    // blocking /readyz probe with a 1500ms timeout against two hosts, and the
    // frontend polls this command on a 3s interval — a sync command would
    // freeze the UI for the probe duration whenever the proxy is down.
    tauri::async_runtime::spawn_blocking(move || {
        let state: State<'_, AppState> = app.state();
        state.runtime_status()
    })
    .await
    .map_err(|err| err.to_string())
}

/// Debug-only: force the proxy intercept's bypass flag on/off so a developer
/// can manually exercise the gated path (Python proxy stopped, traffic routed
/// direct to api.anthropic.com) without crossing the real disable threshold.
/// Compiled out of release builds.
#[cfg(debug_assertions)]
#[tauri::command]
fn debug_force_proxy_bypass(state: State<'_, AppState>, on: bool) -> Result<bool, String> {
    log::debug!("[debug_force_proxy_bypass] requested on={on}");
    state
        .proxy_bypass
        .store(on, std::sync::atomic::Ordering::Release);
    log::debug!(
        "[debug_force_proxy_bypass] stored bypass={}",
        state
            .proxy_bypass
            .load(std::sync::atomic::Ordering::Acquire)
    );
    if on {
        state.stop_headroom();
        log::debug!("[debug_force_proxy_bypass] stop_headroom complete");
    } else {
        // Recover from any auto-pause / client teardown that may have run
        // while bypass was active (the watchdog's give-up path or the
        // pricing gate's `disable_client_setup` call).
        client_adapters::restore_client_setups();
        state.set_runtime_paused(false);
        state
            .ensure_headroom_running()
            .map_err(|err| err.to_string())?;
    }
    Ok(state
        .proxy_bypass
        .load(std::sync::atomic::Ordering::Acquire))
}

#[tauri::command]
async fn get_headroom_logs(
    state: State<'_, AppState>,
    max_lines: Option<usize>,
) -> Result<Vec<String>, String> {
    let limit = max_lines.unwrap_or(120).clamp(20, 500);
    state
        .tool_manager
        .read_headroom_log_tail(limit)
        .map_err(|err| err.to_string())
}

/// Authoritative "did the proxy receive a request" signal for the connector
/// verification UI. Reads `/stats` on the live Rust front proxy and returns
/// `requests.total`. The earlier verification path scanned the python proxy
/// log for /v1/messages lines, but Claude Code traffic flows through the
/// Rust proxy on 6767 — the python log only ever sees background/internal
/// activity, so the regex match never fired even when the user's calls were
/// being optimized normally.
///
/// `None` means the proxy is unreachable or `/stats` failed; the frontend
/// must distinguish that from `Some(0)` ("up but no traffic yet"), otherwise
/// a transient unreachable → reachable transition would look like a counter
/// jump from 0 → N and falsely flip the badge to healthy.
#[tauri::command]
async fn get_headroom_request_count() -> Option<u64> {
    // Blocking reqwest call — keep it off the async workers; the setup
    // verification UI polls this while a connector is in 'verifying'.
    tokio::task::spawn_blocking(fetch_proxy_request_count_stats)
        .await
        .ok()
        .flatten()
}

fn fetch_proxy_request_count_stats() -> Option<u64> {
    parse_request_count_from_stats_body(&fetch_proxy_stats_body()?)
}

fn stats_client() -> Option<&'static reqwest::blocking::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::blocking::Client>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_millis(500))
                .build()
                .ok()
        })
        .as_ref()
}

fn fetch_proxy_stats_body() -> Option<String> {
    let client = stats_client()?;
    for host in ["127.0.0.1", "localhost"] {
        let url = format!("http://{host}:6767/stats");
        let Ok(response) = client.get(&url).send() else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        if let Ok(body) = response.text() {
            return Some(body);
        }
    }
    None
}

/// Per-agent request counts from `/stats` `agent_usage.agents[]`, keyed by the
/// proxy's agent id (`claude-code`, `codex`, ...). Used by setup verification
/// so a prompt sent to one client only flips that client's row, not all rows.
#[tauri::command]
async fn get_headroom_request_counts_by_agent() -> Option<std::collections::HashMap<String, u64>> {
    let body = tokio::task::spawn_blocking(fetch_proxy_stats_body)
        .await
        .ok()
        .flatten()?;
    parse_request_counts_by_agent(&body)
}

/// In-process per-agent counters from the Rust intercept. Same key shape as
/// `get_headroom_request_counts_by_agent`, but works with no Python backend —
/// paywall-first setup verification polls this while in passthrough.
#[tauri::command]
fn get_intercept_request_counts_by_agent() -> std::collections::HashMap<String, u64> {
    proxy_intercept::intercept_request_counts()
}

/// Running agent processes keyed by connector id, for the verify screen's
/// "these sessions still hold old settings" callout. Undercounts are fine
/// (the callout just stays quiet); false positives are not, so matching is
/// strict on the executable/script basename.
#[tauri::command]
fn get_running_agent_process_counts() -> std::collections::HashMap<String, usize> {
    #[cfg(windows)]
    {
        // tasklist reports image names only, so npm installs running under
        // node.exe are invisible here. ponytail: undercount accepted; teach
        // this Get-CimInstance CommandLine matching if Windows verify data
        // says the callout stays too quiet.
        let output = crate::proc::command("tasklist")
            .args(["/NH", "/FO", "CSV"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        agent_process_counts_from_lines(
            output
                .lines()
                .filter_map(|line| line.split(',').next())
                .map(|image| image.trim_matches('"')),
        )
    }
    #[cfg(not(windows))]
    {
        let output = crate::proc::command("ps")
            .args(["-axo", "args="])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        agent_process_counts_from_lines(output.lines())
    }
}

/// Runs the official Claude Code installer, so the no-clients wizard panel
/// can offer one click instead of a copy-paste terminal round-trip. Exactly
/// the script the panel shows for manual use; nothing is decided here, the
/// panel re-probes connectors afterwards and the installer's own output comes
/// back on failure. Blocking for its ~30-60s is fine: Tauri runs sync
/// commands off the UI thread and the button holds a busy state. No timeout —
/// a hung download leaves the button busy, which the user can abandon for the
/// manual command sitting right under it.
#[tauri::command]
fn install_claude_code_cli() -> Result<(), String> {
    #[cfg(windows)]
    let output = crate::proc::command("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "irm https://claude.ai/install.ps1 | iex",
        ])
        .output();
    // pipefail so a failed download surfaces as a failure instead of bash
    // happily interpreting half a script.
    #[cfg(not(windows))]
    let output = crate::proc::command("bash")
        .args([
            "-c",
            "set -o pipefail; curl -fsSL https://claude.ai/install.sh | bash",
        ])
        .output();

    let output = output.map_err(|err| format!("could not run the installer: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = last_nonempty_lines(
        if stderr.trim().is_empty() {
            &stdout
        } else {
            &stderr
        },
        3,
    );
    Err(if detail.is_empty() {
        format!("the installer exited with {}", output.status)
    } else {
        detail
    })
}

/// The last `n` non-empty lines joined with spaces — installer output ends
/// with the useful error, and a wall of curl progress is not a message.
fn last_nonempty_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rev()
        .take(n)
        .collect();
    lines.reverse();
    lines.join(" ")
}

/// One command line (unix `ps -axo args=`) or bare image name (windows
/// tasklist) per item. An agent counts when the first token's basename is its
/// binary, or when a node/bun interpreter is running a script whose basename
/// is the agent (the npm install shape: `node /usr/local/bin/claude`).
/// Basename-only matching keeps `grep claude` and editors with a file named
/// "claude" open from counting; paths containing spaces mis-split and are
/// accepted as an undercount.
fn agent_process_counts_from_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> std::collections::HashMap<String, usize> {
    fn basename(token: &str) -> &str {
        let token = token.trim_end_matches(".exe").trim_end_matches(".EXE");
        token.rsplit(['/', '\\']).next().unwrap_or(token)
    }
    fn connector_for(base: &str) -> Option<&'static str> {
        match base {
            "claude" => Some("claude_code"),
            "codex" => Some("codex"),
            "opencode" => Some("opencode"),
            "grok" => Some("grok_build"),
            _ => None,
        }
    }
    let mut counts = std::collections::HashMap::new();
    for line in lines {
        let mut tokens = line.split_whitespace();
        let Some(first) = tokens.next() else { continue };
        let first_base = basename(first);
        let connector = connector_for(first_base).or_else(|| {
            (first_base == "node" || first_base == "bun")
                .then(|| tokens.next().map(basename).and_then(connector_for))
                .flatten()
        });
        if let Some(id) = connector {
            *counts.entry(id.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchFlags {
    pub paywall_first: bool,
}

/// Frontend-visible test overrides. Every field is None on a stable build and
/// on any RC launched without the matching env var, so the shipped default is
/// "no overrides" and the UI paths below stay exactly as they are in production.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugOverrides {
    /// "no_traffic" or "no_savings" when HEADROOM_FAKE_SETUP_STALL forces the
    /// setup-stall alert to fire immediately, ignoring uptime, savings,
    /// connector state, the account gate and the once-per-day throttle.
    pub setup_stall: Option<String>,
}

/// Cached launch flags. On a cold cache, performs one bounded config fetch so
/// a fresh first launch does not miss its server bucket.
#[tauri::command]
fn get_launch_flags() -> LaunchFlags {
    LaunchFlags {
        paywall_first: pricing::paywall_first_flag_or_refresh(),
    }
}

pub(crate) fn parse_request_counts_by_agent(
    body: &str,
) -> Option<std::collections::HashMap<String, u64>> {
    let root = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let mut counts = std::collections::HashMap::new();
    if let Some(agents) = root
        .get("agent_usage")
        .and_then(|v| v.get("agents"))
        .and_then(|v| v.as_array())
    {
        for agent in agents {
            if let (Some(key), Some(requests)) = (
                agent.get("agent").and_then(|v| v.as_str()),
                agent.get("requests").and_then(|v| v.as_u64()),
            ) {
                counts.insert(key.to_string(), requests);
            }
        }
    }
    Some(counts)
}

/// Pull `requests.total` (or any of the legacy spellings) out of a /stats
/// JSON body. Mirrors the lookup in `state::parse_headroom_stats_from_json`
/// but trimmed to just the counter we need for verification.
pub(crate) fn parse_request_count_from_stats_body(body: &str) -> Option<u64> {
    let root = serde_json::from_str::<serde_json::Value>(body).ok()?;
    if let Some(total) = root
        .get("requests")
        .and_then(|v| v.get("total"))
        .and_then(|v| v.as_u64())
    {
        return Some(total);
    }
    for key in ["total_requests", "totalRequests", "requests_total"] {
        if let Some(total) = find_u64_key_recursive_local(&root, key) {
            return Some(total);
        }
    }
    None
}

fn find_u64_key_recursive_local(value: &serde_json::Value, key: &str) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(|v| v.as_u64()) {
                return Some(found);
            }
            for v in map.values() {
                if let Some(found) = find_u64_key_recursive_local(v, key) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(found) = find_u64_key_recursive_local(item, key) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

#[tauri::command]
async fn get_rtk_activity(
    state: State<'_, AppState>,
    max_lines: Option<usize>,
) -> Result<Vec<String>, String> {
    let limit = max_lines.unwrap_or(120).clamp(20, 500);
    state
        .tool_manager
        .read_rtk_activity(limit)
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn get_tool_logs(
    state: State<'_, AppState>,
    tool_id: String,
    max_lines: Option<usize>,
) -> Result<Vec<String>, String> {
    let limit = max_lines.unwrap_or(120).clamp(20, 500);
    state
        .tool_manager
        .read_tool_log_tail(&tool_id, limit)
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn get_claude_code_projects(
    state: State<'_, AppState>,
) -> Result<Vec<ClaudeCodeProject>, String> {
    state
        .list_claude_code_projects()
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn get_claude_usage(state: State<'_, AppState>) -> Result<ClaudeUsage, String> {
    pricing::fetch_claude_usage(&state)
}

#[tauri::command]
fn get_claude_profile(state: State<'_, AppState>) -> ClaudeAccountProfile {
    pricing::detect_claude_profile(&state)
}

#[tauri::command]
async fn get_headroom_pricing_status(
    state: State<'_, AppState>,
) -> Result<HeadroomPricingStatus, String> {
    let status = pricing::get_pricing_status(&state)?;
    // Reconcile the runtime with the freshly evaluated status. Bridges the
    // gap between "user just upgraded" (subscription_active flips on) and
    // "Headroom optimization actually resumes" — without this, the pricing
    // gate's bypass flag would stay set and Python would stay down until
    // the next app launch.
    state.apply_pricing_gate_status(
        &status,
        crate::client_adapters::any_gate_exempt_client_enabled(),
    );
    state.apply_codex_pricing_gate_status(status.codex.as_ref());
    state.report_weekly_limit_transitions(&status);
    Ok(status)
}

/// Fire-and-forget install-wizard funnel beacon from the frontend. Returns
/// immediately; `pricing::report_funnel_step` does the POST on a detached
/// thread so a slow/offline network never blocks the wizard.
#[tauri::command]
fn report_funnel_step(state: State<'_, AppState>, step: String) {
    pricing::report_funnel_step(&state, &step);
}

/// Credentials handed over by a `headroom://auth` magic link, waiting for the
/// UI to claim them.
///
/// A cold start races the frontend: macOS delivers the URL before React has
/// mounted a listener, so an event alone is lost exactly when the app was
/// launched *by* the link. The slot is the source of truth and the event is
/// only a nudge for the already-running case; both paths drain it through
/// `take_pending_magic_link`.
static PENDING_MAGIC_LINK: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// Everything a `headroom://` URL triggers: show the launcher, park any magic
/// link credentials, and reconcile pricing off-thread.
///
/// Reached from three places because no single one covers every platform: the
/// `on_open_url` listener (macOS, and Windows/Linux warm starts via the
/// single-instance argv hand-off), and the `get_current()` drain in setup
/// (Windows/Linux cold start, where the plugin parses argv before the listener
/// exists).
fn handle_headroom_deep_link(app: &AppHandle, url: &tauri::Url) {
    let _ = show_launcher_window(app);
    capture_magic_link_auth(app, url);
    // Run the reconciliation on a worker thread - the deep-link callback is on
    // the main thread and we don't want pricing's blocking HTTP call there.
    let app_handle = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_handle.state();
        match pricing::get_pricing_status(&state) {
            Ok(status) => {
                state.apply_pricing_gate_status(
                    &status,
                    crate::client_adapters::any_gate_exempt_client_enabled(),
                );
                state.apply_codex_pricing_gate_status(status.codex.as_ref());
                // Payload-less on purpose: this status was fetched before any
                // magic link in the same URL was redeemed, so it is stale by
                // the time it lands. The frontend refetches instead.
                let _ = app_handle.emit("pricing-refreshed", ());
            }
            Err(err) => {
                sentry::capture_message(
                    &format!("deep link pricing refresh failed: {err}"),
                    sentry::Level::Warning,
                );
            }
        }
    });
}

/// Parks the email/code from `headroom://auth?email=..&code=..` and nudges the UI.
///
/// The browser never signs the user in (it cannot supply the device
/// fingerprints `verify_code` needs), so this is the ordinary typed-code flow
/// with the typing removed.
fn capture_magic_link_auth(app: &AppHandle, url: &tauri::Url) {
    let Some(credentials) = parse_magic_link_auth(url) else {
        return;
    };
    if let Ok(mut slot) = PENDING_MAGIC_LINK.lock() {
        *slot = Some(credentials);
    }
    let _ = app.emit("magic-link-auth", ());
}

/// `headroom://auth?email=..&code=..` -> `(email, code)`.
///
/// Every other `headroom://` URL is the post-checkout return and must fall
/// through to the pricing refresh untouched.
fn parse_magic_link_auth(url: &tauri::Url) -> Option<(String, String)> {
    if url.host_str() != Some("auth") {
        return None;
    }
    let mut email = None;
    let mut code = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "email" => email = Some(value.into_owned()),
            "code" => code = Some(value.into_owned()),
            _ => {}
        }
    }
    let email = email?;
    let code = code?;
    (!email.is_empty() && !code.is_empty()).then_some((email, code))
}

/// Drains the pending magic-link credentials. Returns `None` on a normal
/// launch; one-shot so a reload cannot replay a spent code.
#[tauri::command]
fn take_pending_magic_link() -> Option<(String, String)> {
    PENDING_MAGIC_LINK
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

#[tauri::command]
async fn request_headroom_auth_code(
    app: AppHandle,
    state: State<'_, AppState>,
    email: String,
) -> Result<HeadroomAuthCodeRequest, String> {
    let request = pricing::request_auth_code(&state, &email)?;
    analytics::track_event(&app, "auth_code_requested", None);
    Ok(request)
}

#[tauri::command]
async fn verify_headroom_auth_code(
    app: AppHandle,
    state: State<'_, AppState>,
    email: String,
    code: String,
    invite_code: Option<String>,
) -> Result<HeadroomPricingStatus, String> {
    let used_invite_code = invite_code
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let status = pricing::verify_auth_code(&state, &email, &code, invite_code.as_deref())?;
    // Reconcile the runtime with the freshly evaluated status. Mirrors
    // `get_headroom_pricing_status` so a user who signs up after grace
    // expiry doesn't have to wait for the next 60s pricing poll for
    // Python to come back online.
    //
    // On a worker thread, not inline: a gate flip here starts or stops the
    // Python backend, and `ensure_headroom_running` blocks across a full
    // cold boot (`start_headroom_background` waits up to
    // HEADROOM_STARTUP_TIMEOUT_MS = 5min per spawn variant, longer on a
    // Windows first launch with Defender scanning the venv). Awaiting that
    // kept the sign-in button on "Verifying..." for minutes after the
    // account was already connected. Same idiom as
    // `handle_headroom_deep_link`.
    {
        let app_handle = app.clone();
        let status = status.clone();
        std::thread::spawn(move || {
            let state: tauri::State<'_, AppState> = app_handle.state();
            state.apply_pricing_gate_status(
                &status,
                crate::client_adapters::any_gate_exempt_client_enabled(),
            );
            state.apply_codex_pricing_gate_status(status.codex.as_ref());
        });
    }
    analytics::track_event(
        &app,
        "auth_verified",
        Some(json!({ "invite_code_used": used_invite_code })),
    );
    // Pricing status is per-window UI state, so the window that did not run
    // the sign-in keeps rendering the signed-out code form until its own poll
    // ticks. Broadcast so every window re-reads it now.
    let _ = app.emit("pricing-refreshed", &status);
    Ok(status)
}

#[tauri::command]
async fn sign_out_headroom_account(app: AppHandle) -> Result<(), String> {
    pricing::sign_out()?;
    // Same broadcast as verify: the other window must not keep showing the
    // account as signed in.
    let _ = app.emit("pricing-refreshed", ());
    Ok(())
}

#[tauri::command]
async fn activate_headroom_account(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<HeadroomPricingStatus, String> {
    let lifetime_tokens_saved = state.dashboard().lifetime_estimated_tokens_saved;
    let status = pricing::activate_account(&state, lifetime_tokens_saved)?;
    analytics::track_event(&app, "account_activated", None);
    Ok(status)
}

#[tauri::command]
async fn create_headroom_checkout_session(
    app: AppHandle,
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
) -> Result<String, String> {
    let url = pricing::create_checkout_session(subscription_tier.clone(), billing_period)?;
    analytics::track_event(
        &app,
        "checkout_started",
        Some(json!({
            "subscription_tier": subscription_tier_label(&subscription_tier)
        })),
    );
    Ok(url)
}

#[tauri::command]
async fn change_headroom_subscription_plan(
    app: AppHandle,
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
) -> Result<(), String> {
    pricing::change_subscription_plan(subscription_tier.clone(), billing_period)?;
    analytics::track_event(
        &app,
        "subscription_plan_changed",
        Some(json!({
            "subscription_tier": subscription_tier_label(&subscription_tier)
        })),
    );
    Ok(())
}

#[tauri::command]
async fn reactivate_headroom_subscription(app: AppHandle) -> Result<(), String> {
    pricing::reactivate_subscription()?;
    analytics::track_event(&app, "subscription_reactivated", None);
    Ok(())
}

#[tauri::command]
async fn get_headroom_billing_portal_url(target: Option<String>) -> Result<String, String> {
    pricing::get_billing_portal_url(target)
}

/// Step one of cancelling: record the reason before the client opens the
/// billing portal, so a user who bails after this point is still counted.
#[tauri::command]
async fn submit_headroom_cancellation_intent(
    reason: String,
    note: Option<String>,
) -> Result<(), String> {
    pricing::submit_cancellation_intent(&reason, note.as_deref().unwrap_or_default())
}

#[tauri::command]
fn get_headroom_learn_status(
    state: State<'_, AppState>,
    project_path: Option<String>,
) -> HeadroomLearnStatus {
    state.headroom_learn_status(project_path.as_deref())
}

#[tauri::command]
fn get_headroom_learn_prereq_status(
    state: State<'_, AppState>,
    force: Option<bool>,
) -> HeadroomLearnPrereqStatus {
    if force.unwrap_or(false) {
        state.invalidate_headroom_learn_prereq_cache();
    }
    state.headroom_learn_prereq_status()
}

#[tauri::command]
async fn get_transformations_feed(limit: Option<u32>) -> TransformationFeedResponse {
    let limit = limit.unwrap_or(50).min(100);
    fetch_transformations_feed(limit).unwrap_or_else(|_| TransformationFeedResponse {
        log_full_messages: false,
        transformations: Vec::new(),
        proxy_reachable: false,
    })
}

/// Read-only snapshot of the activity feed. Observation — fetching the proxy,
/// writing to ActivityFacts, persisting — happens on a dedicated background
/// timer (see `spawn_activity_observer`), so this command never mutates state.
/// That keeps the IPC hot path short: one in-memory lock + a cheap /readyz
/// ping to the local proxy.
#[tauri::command]
async fn get_activity_feed(state: State<'_, AppState>) -> Result<ActivityFeedResponse, String> {
    Ok(ActivityFeedResponse {
        tiles: state.activity_feed_snapshot(),
        proxy_reachable: crate::state::headroom_proxy_reachable(),
    })
}

/// Observation cadence for background activity milestones. A modest delay is
/// fine here; foreground Activity still polls separately, and the
/// memory-export path is intentionally kept away from tight loops.
const ACTIVITY_OBSERVER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
/// Rescan cadence for the Claude projects cache. This keeps Optimize mostly
/// warm without doing filesystem-heavy project scans every minute forever.
const CLAUDE_PROJECTS_WARM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(75);
/// Matches the frontend's `ACTIVITY_FEED_WINDOW` in App.tsx so the observer
/// sees the same transformations the UI will display.
const ACTIVITY_OBSERVER_LIMIT: u32 = 150;

fn spawn_activity_observer(app: AppHandle) {
    std::thread::spawn(move || {
        // Small warm-up so we don't race with runtime bring-up; the first
        // proxy fetch lands a few seconds after the proxy is actually up.
        std::thread::sleep(std::time::Duration::from_secs(3));
        loop {
            run_activity_observation(&app);
            std::thread::sleep(ACTIVITY_OBSERVER_INTERVAL);
        }
    });
}

/// Keeps `list_claude_code_projects` cache warm on a background thread so the
/// IPC path never pays the projects-dir scan (hundreds of `stat` calls plus
/// per-project metadata reads). Pure cache-fill with no side effects —
/// `list_claude_code_projects` is idempotent and only writes to its own
/// cache slot.
fn spawn_claude_projects_warmer(app: AppHandle) {
    std::thread::spawn(move || {
        // Stagger from the activity observer so both background threads
        // don't simultaneously contend on fs / IPC at boot.
        std::thread::sleep(std::time::Duration::from_secs(5));
        loop {
            let state: tauri::State<'_, AppState> = app.state();
            let _ = state.list_claude_code_projects();
            std::thread::sleep(CLAUDE_PROJECTS_WARM_INTERVAL);
        }
    });
}

fn run_activity_observation(app: &AppHandle) {
    let state: tauri::State<'_, AppState> = app.state();

    let _ = state.maybe_emit_weekly_recap();

    if let Ok(feed) = fetch_transformations_feed(ACTIVITY_OBSERVER_LIMIT) {
        let _ = state.observe_activity_from_transformations(&feed.transformations);
        // Same batch, second reader: flags a client whose requests all stopped
        // compressing (see savings_canary for why the server cannot see this).
        savings_canary::observe(&feed.transformations);
    }

    let projects = state.list_claude_code_projects().unwrap_or_default();

    // Memory.db "patterns today" comes from the export JSON's `created_at`
    // field. Everything else (reminders / learnings today) is derived from
    // per-project CLAUDE.md + MEMORY.md bullet diffs.
    let memory_path = headroom_memory_db_path();
    let patterns_today = if memory_path.exists() {
        memory_export_cached(&state, &memory_path)
            .ok()
            .and_then(|stdout| count_memories_created_today(&stdout, Utc::now()).ok())
            .unwrap_or(0) as u32
    } else {
        0
    };

    // Collect current bullet sets for every project the user has touched
    // today, so `observe_learnings_today` has a baseline regardless of which
    // one ends up being "most active".
    let project_inputs: Vec<crate::activity_facts::LearningsProjectInput> = projects
        .iter()
        .filter(|p| p.sessions_today > 0)
        .map(|p| {
            let applied = read_applied_patterns_for_project(&p.project_path);
            crate::activity_facts::LearningsProjectInput {
                project_path: p.project_path.clone(),
                project_display_name: p.display_name.clone(),
                claude_md_bullets: flatten_applied_bullets(&applied.claude_md),
                memory_md_bullets: flatten_applied_bullets(&applied.memory_md),
            }
        })
        .collect();

    // Most active = highest sessions_today; ties broken by most recent
    // last_worked_at so the chip tracks what the user is working on right now.
    let active_project_path = projects
        .iter()
        .filter(|p| p.sessions_today > 0)
        .max_by(|a, b| {
            a.sessions_today
                .cmp(&b.sessions_today)
                .then(a.last_worked_at.cmp(&b.last_worked_at))
        })
        .map(|p| p.project_path.clone());

    let _ = state.observe_learnings_today(
        patterns_today,
        project_inputs,
        active_project_path.as_deref(),
    );

    // No point nudging the user to run Train if the claude CLI isn't installed —
    // they'd just hit an install prompt. The Optimize tab surfaces the install
    // UI in that case; let them fix prereqs first.
    if state.headroom_learn_prereq_status().claude_cli_available {
        let _ = state.observe_train_suggestions(&projects);
    }
}

fn flatten_applied_bullets(sections: &[crate::models::AppliedSection]) -> Vec<String> {
    sections
        .iter()
        .flat_map(|sec| sec.bullets.iter().cloned())
        .collect()
}

#[tauri::command]
async fn list_live_learnings(
    state: State<'_, AppState>,
    project_path: String,
) -> Result<Vec<crate::models::LiveLearning>, String> {
    let memory_path = headroom_memory_db_path();
    if !memory_path.exists() {
        return Ok(Vec::new());
    }
    let stdout = memory_export_cached(&state, &memory_path)?;
    parse_live_learnings(&stdout, &project_path)
}

#[tauri::command]
async fn list_live_learnings_for_projects(
    state: State<'_, AppState>,
    project_paths: Vec<String>,
) -> Result<std::collections::HashMap<String, Vec<crate::models::LiveLearning>>, String> {
    let memory_path = headroom_memory_db_path();
    if !memory_path.exists() {
        return Ok(empty_live_learnings_for_projects(&project_paths));
    }
    let stdout = memory_export_cached(&state, &memory_path)?;
    aggregate_live_learnings(&stdout, &project_paths)
}

fn empty_live_learnings_for_projects(
    project_paths: &[String],
) -> std::collections::HashMap<String, Vec<crate::models::LiveLearning>> {
    let mut out = std::collections::HashMap::with_capacity(project_paths.len());
    for p in project_paths {
        out.insert(p.clone(), Vec::new());
    }
    out
}

fn aggregate_live_learnings(
    stdout: &str,
    project_paths: &[String],
) -> Result<std::collections::HashMap<String, Vec<crate::models::LiveLearning>>, String> {
    let mut out = std::collections::HashMap::with_capacity(project_paths.len());
    for p in project_paths {
        let learnings = parse_live_learnings(stdout, p)?;
        out.insert(p.clone(), learnings);
    }
    Ok(out)
}

fn memory_export_cached(state: &State<'_, AppState>, memory_path: &Path) -> Result<String, String> {
    if let Some(cached) = state.cached_memory_export() {
        return Ok(cached);
    }
    let entrypoint = state.tool_manager.headroom_entrypoint();
    let stdout = run_memory_export(&entrypoint, memory_path)?;
    state.store_memory_export(stdout.clone());
    Ok(stdout)
}

#[tauri::command]
async fn delete_live_learning(state: State<'_, AppState>, memory_id: String) -> Result<(), String> {
    let memory_path = headroom_memory_db_path();
    if !memory_path.exists() {
        return Err("Memory database does not exist.".into());
    }
    let entrypoint = state.tool_manager.headroom_entrypoint();
    let output = crate::proc::command(&entrypoint)
        .arg("memory")
        .arg("delete")
        .arg(&memory_id)
        .arg("--force")
        .arg("--db-path")
        .arg(&memory_path)
        .env("PYTHONNOUSERSITE", "1")
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "headroom memory delete failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    state.invalidate_memory_export_cache();
    Ok(())
}

#[tauri::command]
async fn list_applied_patterns(
    project_path: String,
) -> Result<crate::models::AppliedPatterns, String> {
    Ok(read_applied_patterns_for_project(&project_path))
}

#[tauri::command]
async fn list_applied_patterns_for_projects(
    project_paths: Vec<String>,
) -> Result<std::collections::HashMap<String, crate::models::AppliedPatterns>, String> {
    let mut out = std::collections::HashMap::with_capacity(project_paths.len());
    for p in project_paths {
        let patterns = read_applied_patterns_for_project(&p);
        out.insert(p, patterns);
    }
    Ok(out)
}

fn read_applied_patterns_for_project(project_path: &str) -> crate::models::AppliedPatterns {
    let claude_md = claude_learn_md_path(project_path);
    let memory_md = crate::tool_manager::claude_project_memory_file(project_path);

    crate::models::AppliedPatterns {
        claude_md: read_applied_block(&claude_md),
        memory_md: read_applied_block(&memory_md),
    }
}

/// Upstream `headroom learn` writes project learnings to the personal
/// CLAUDE.local.md (issue #1072); older versions wrote the team-shared
/// CLAUDE.md. Prefer whichever file currently holds a Headroom block,
/// local first.
fn claude_learn_md_path(project_path: &str) -> std::path::PathBuf {
    let root = std::path::Path::new(project_path);
    let local = root.join("CLAUDE.local.md");
    if !read_applied_block(&local).is_empty() {
        return local;
    }
    let shared = root.join("CLAUDE.md");
    if !read_applied_block(&shared).is_empty() {
        return shared;
    }
    local
}

#[tauri::command]
async fn delete_applied_pattern(
    project_path: String,
    file_kind: String,
    section_title: String,
    bullet_text: String,
) -> Result<(), String> {
    let path = match file_kind.as_str() {
        "claude" => claude_learn_md_path(&project_path),
        "memory" => crate::tool_manager::claude_project_memory_file(&project_path),
        other => return Err(format!("Unknown file_kind: {other}")),
    };
    if !path.exists() {
        return Err(format!("{} does not exist.", path.display()));
    }
    let content =
        std::fs::read_to_string(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let updated =
        crate::tool_manager::delete_applied_bullet(&content, &section_title, &bullet_text);
    if updated == content {
        return Ok(()); // no-op; nothing to write
    }
    crate::client_adapters::atomic_write(&path, updated.as_bytes())
        .map_err(|err| format!("write {}: {err:#}", path.display()))?;
    Ok(())
}

fn read_applied_block(path: &std::path::Path) -> Vec<crate::models::AppliedSection> {
    match std::fs::read_to_string(path) {
        Ok(content) => crate::tool_manager::parse_headroom_learn_block(&content),
        Err(_) => Vec::new(),
    }
}

/// Shells `headroom memory export --db-path <db>` and returns raw JSON stdout.
fn run_memory_export(entrypoint: &Path, db_path: &Path) -> Result<String, String> {
    let output = crate::proc::command(entrypoint)
        .arg("memory")
        .arg("export")
        .arg("--db-path")
        .arg(db_path)
        .env("PYTHONNOUSERSITE", "1")
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(format!("headroom memory export exited {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_live_learnings(
    json: &str,
    project_path: &str,
) -> Result<Vec<crate::models::LiveLearning>, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        id: String,
        #[serde(default)]
        content: String,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        importance: Option<f64>,
        #[serde(default)]
        metadata: serde_json::Value,
        #[serde(default)]
        entity_refs: Vec<String>,
    }

    let raws: Vec<Raw> = serde_json::from_str(json.trim()).map_err(|err| err.to_string())?;
    let mut out: Vec<crate::models::LiveLearning> = Vec::new();
    for r in raws {
        let source = r
            .metadata
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if source != "traffic_learner" {
            continue;
        }
        if !pattern_matches_project(&r.content, &r.entity_refs, project_path) {
            continue;
        }
        let category = r
            .metadata
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let evidence_count = r
            .metadata
            .get("evidence_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;
        out.push(crate::models::LiveLearning {
            id: r.id,
            content: r.content,
            category,
            importance: r.importance.unwrap_or(0.5),
            evidence_count,
            created_at: r.created_at.unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// True if any absolute path in `content` or `entity_refs` is under `project_path`.
fn pattern_matches_project(content: &str, entity_refs: &[String], project_path: &str) -> bool {
    let root = project_path.trim_end_matches(['/', '\\']);
    if root.is_empty() {
        return false;
    }
    // Windows project paths use '\'; build the child-path needle with the
    // separator the path itself uses.
    let sep = if root.contains('\\') { '\\' } else { '/' };
    let needle_slash = format!("{root}{sep}");
    if content.contains(root) {
        // Guard against /x/ab matching /x/a — require either exact or followed by /
        if content.contains(&needle_slash)
            || content.contains(&format!("{root}\""))
            || content.contains(&format!("{root}`"))
        {
            return true;
        }
    }
    for r in entity_refs {
        if r == root || r.starts_with(&needle_slash) {
            return true;
        }
    }
    false
}

#[tauri::command]
async fn start_headroom_learn(
    app: AppHandle,
    agent: String,
    project_path: Option<String>,
) -> Result<(), String> {
    let agent = LearnAgent::parse(&agent)?;
    if matches!(agent, LearnAgent::Claude) && project_path.is_none() {
        return Err("A project path is required for Claude Headroom Learn.".into());
    }
    check_headroom_learn_prereqs(
        agent,
        crate::state::headroom_learn_platform_message().as_deref(),
        &detect_headroom_learn_prereq_status(),
    )?;

    // Codex isn't project-organized, so its run-status is keyed on a stable id.
    let run_key = match agent {
        LearnAgent::Claude => project_path.clone().unwrap_or_default(),
        LearnAgent::Codex => "codex".to_string(),
        LearnAgent::Opencode => "opencode".to_string(),
        LearnAgent::Grok => "grok".to_string(),
    };
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.begin_headroom_learn_run(&run_key)?;
    }

    let app_handle = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_handle.state();
        let run = execute_headroom_learn_run(&state, agent, project_path.as_deref());
        state.complete_headroom_learn_run(run.success, run.summary, run.error, run.output_tail);
    });

    Ok(())
}

#[tauri::command]
fn show_dashboard_window(app: AppHandle) -> Result<(), String> {
    if !onboarding_complete(&app) {
        show_launcher_window(&app).map_err(|err| err.to_string())?;
        return Err("Complete onboarding before opening the tray dashboard.".into());
    }

    ensure_runtime_ready_for_tray(&app);
    hide_launcher_window(&app).map_err(|err| err.to_string())?;
    show_main_window(&app, None).map_err(|err| err.to_string())
}

#[tauri::command]
async fn open_headroom_dashboard() -> Result<(), String> {
    open_external_link_impl(HEADROOM_DASHBOARD_URL)
        .map_err(|err| format!("Failed to open Headroom dashboard: {err}"))
}

fn open_external_link_impl(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("mailto:"))
    {
        return Err("Only http, https, and mailto links are supported.".into());
    }

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = crate::proc::command("open");
        command.arg(trimmed);
        command
    };

    #[cfg(target_os = "linux")]
    {
        for opener in ["xdg-open", "gio", "kde-open5", "wslview"] {
            let mut command = crate::proc::command(opener);
            if opener == "gio" {
                command.args(["open", trimmed]);
            } else {
                command.arg(trimmed);
            }
            match command.status() {
                Ok(status) if status.success() => return Ok(()),
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(format!(
                        "Could not launch external link with {opener}: {err}"
                    ))
                }
            }
        }
        return Err(
            "No URL opener found. Install xdg-utils (provides xdg-open) to open links.".into(),
        );
    }

    // Never route this through `cmd /C start`: cmd re-parses its command line,
    // so `&`, `|`, `^`, and `%VAR%` inside an otherwise valid URL are live
    // shell syntax (`https://x/?a=1&calc` runs calc), and legitimate query
    // strings break the same way. ShellExecuteW gets the URL as one opaque
    // argument.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let url_wide = std::ffi::OsStr::new(trimmed)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        let operation = "open"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                url_wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // Values above 32 are success per the ShellExecuteW contract.
        if result as isize > 32 {
            return Ok(());
        }
        return Err(format!(
            "ShellExecuteW failed to open the link (code {}).",
            result as isize
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let status = command
            .status()
            .map_err(|err| format!("Could not launch external link: {err}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("External link opener exited with {status}."))
        }
    }
}

#[tauri::command]
async fn open_external_link(url: String) -> Result<(), String> {
    open_external_link_impl(&url)
}

#[tauri::command]
fn track_analytics_event(app: AppHandle, name: String, properties: Option<Value>) {
    analytics::track_event(&app, &name, properties);
}

#[tauri::command]
async fn submit_contact_request(
    url: String,
    email: String,
    message: Option<String>,
) -> Result<(), String> {
    let trimmed = email.trim();
    if trimmed.is_empty() || !trimmed.contains('@') {
        return Err("Enter a valid email address.".to_string());
    }

    let target = validate_contact_request_url(&url)
        .ok_or_else(|| "Could not reach the contact form.".to_string())?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| err.to_string())?;
    let message_owned = message
        .map(|m| m.trim().chars().take(2000).collect::<String>())
        .unwrap_or_default();
    let response = client
        .post(target)
        .form(&[
            ("contact_request[email]", trimmed),
            ("contact_request[message]", message_owned.as_str()),
        ])
        .send()
        .await
        .map_err(|err| err.to_string())?;

    // Rails answers a successful POST with a 302 to /#pricing. Redirect policy
    // is none for SSRF defense, so accept 3xx as success here. 422 and 503 are
    // the controller's explicit error renders.
    match response.status().as_u16() {
        200..=399 => Ok(()),
        422 => Err("Enter a valid email address.".to_string()),
        503 => Err("Email delivery still needs to be configured.".to_string()),
        status => Err(format!("Contact request failed with status {status}.")),
    }
}

// Scheme + host allowlist for the contact form endpoint. The URL reaches this
// Tauri command from the webview, so we must not assume it is trustworthy —
// an SSRF primitive here would let a compromised frame POST to arbitrary
// hosts, including loopback services.
fn validate_contact_request_url(raw: &str) -> Option<reqwest::Url> {
    const ALLOWED_HOSTS: &[&str] = &["extraheadroom.com", "www.extraheadroom.com"];
    let parsed = reqwest::Url::parse(raw).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?;
    if !ALLOWED_HOSTS.contains(&host) {
        return None;
    }
    Some(parsed)
}

/// Coarse bucket for a client-setup failure, used in the Sentry fingerprint so
/// the message-based grab-bag splits by failure shape. Walks the anyhow chain
/// for the first io::Error and maps it to a stable category; permission-denied
/// and no-space are filtered out before capture, so they normally won't reach
/// this, but they stay mapped for completeness.
pub(crate) fn client_setup_error_kind(err: &anyhow::Error) -> &'static str {
    for cause in err.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::NotFound => "not_found",
                std::io::ErrorKind::AlreadyExists => "already_exists",
                std::io::ErrorKind::PermissionDenied => "permission_denied",
                _ => match io.raw_os_error() {
                    Some(28) => "no_space",
                    Some(30) => "read_only_fs",
                    _ => "io_other",
                },
            };
        }
    }
    "other"
}

#[tauri::command]
async fn apply_client_setup(
    app: AppHandle,
    client_id: String,
) -> Result<ClientSetupResult, String> {
    // Two recovery paths land on the tray-banner "Re-enable" button:
    //   1. Watchdog give-up — pauses the runtime and clears client setups.
    //   2. Pricing gate (grace expiry, weekly cap) — sets `proxy_bypass` and
    //      calls `stop_headroom()` without flipping `runtime_paused`.
    // Both leave Python stopped, so re-enable has to clear bypass and bring
    // the runtime back. Without this, env vars get rewritten but the proxy
    // stays down and Claude Code traffic flows unoptimized until the next
    // pricing poll (or, in the watchdog case, until restart).
    let state: tauri::State<'_, AppState> = app.state();
    let bypassed = state
        .proxy_bypass
        .load(std::sync::atomic::Ordering::Acquire);
    if state.runtime_is_paused() || bypassed {
        if let Err(err) = state.resume_runtime() {
            // Local log keeps the full chain; the capture below is the Sentry
            // path (fingerprinted, and silent for a machine-policy block).
            // Bridging this warn instead grouped on a message that embeds the
            // port and the user's home path -- RUST-AD.
            log::info!("apply_client_setup: resume_runtime failed: {err:#}");
            capture_headroom_start_failure("apply_client_setup: resume_runtime failed", &err);
        }
    }
    match client_adapters::apply_client_setup(&client_id) {
        Ok(result) => {
            // Funnel beacon lives here (not the launcher UI) so every apply
            // path counts: launcher auto-configure, the manual client-setup
            // screen, and the dashboard connector toggle. First-write-wins
            // server-side, so post-onboarding re-applies are no-ops.
            pricing::report_funnel_step(&state, "client_setup_applied");
            analytics::track_event(
                &app,
                "client_setup_applied",
                Some(json!({
                    "client_id": result.client_id.clone(),
                    "already_configured": result.already_configured,
                    "verified": result.verification.verified,
                    "proxy_reachable": result.verification.proxy_reachable
                })),
            );
            // Setup returned Ok, but the post-write verification read the
            // files back and found the expected side effect missing. That's
            // the same class of bug as the MCP fallback silent-success —
            // subprocess/file-write succeeded yet the integration is not
            // actually in place. Capture to Sentry so we see it.
            // An unwritable shell profile is an expected, environmental
            // degradation (core routing still works via app-owned config), so
            // don't alert on the verification miss it causes.
            if !result.verification.verified && !result.shell_profile_unwritable {
                sentry::with_scope(
                    |scope| {
                        scope.set_extra(
                            "proxy_reachable",
                            result.verification.proxy_reachable.into(),
                        );
                        scope.set_extra("checks", json!(result.verification.checks).into());
                        scope.set_extra("failures", json!(result.verification.failures).into());
                        scope.set_extra("already_configured", result.already_configured.into());
                    },
                    || {
                        sentry::capture_message(
                            &format!(
                                "client setup for {client_id} completed but verification failed",
                            ),
                            sentry::Level::Warning,
                        );
                    },
                );
            }
            Ok(result)
        }
        Err(err) => {
            let msg = err.to_string();
            // Permission-denied (os error 13) and disk-full (ENOSPC, os error
            // 28) writes are unwritable-file environment issues, not app bugs --
            // surface to the user but keep them out of Sentry.
            if !msg.starts_with("Automatic setup is not supported yet")
                && !client_adapters::is_permission_denied(&err)
                && !client_adapters::is_no_space(&err)
            {
                // Split the grab-bag: a message-based fingerprint collapsed every
                // client x every failure cause into one issue, so resolving one
                // shape (e.g. the codex rename race) regressed the moment a
                // sibling shape (ENOSPC on another client) reappeared. Key on
                // client_id + a coarse error kind so real code failures separate
                // from environmental noise and each stays independently resolvable.
                let kind = client_setup_error_kind(&err);
                sentry::with_scope(
                    |scope| {
                        let fp: &[&str] = &["client_setup_failed", client_id.as_str(), kind];
                        scope.set_fingerprint(Some(fp));
                    },
                    || {
                        sentry::capture_message(
                            &format!("client setup failed for {client_id}: {err:#}"),
                            sentry::Level::Error,
                        );
                    },
                );
            }
            // Unlike the Sentry capture above, the funnel beacon has no
            // exclusions: permission-denied and disk-full are environmental,
            // but the per-OS funnel still needs them counted as "setup was
            // attempted and did not stick" (invisible on Windows otherwise).
            pricing::report_funnel_step(&state, "client_setup_failed");
            Err(msg)
        }
    }
}

#[tauri::command]
async fn verify_client_setup(client_id: String) -> Result<ClientSetupVerification, String> {
    client_adapters::verify_client_setup(&client_id).map_err(|err| err.to_string())
}

/// Watchdog-driven silent self-heal (see `client_adapters::repair_client_setups`).
/// Skipped while the runtime is paused or bypassed: the pricing gate and the
/// watchdog give-up path tear client configs down on purpose, and repairing
/// behind their backs would fight the gate.
#[tauri::command]
async fn repair_client_setups(app: AppHandle) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state: State<'_, AppState> = app.state();
        if state.runtime_is_paused()
            || state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Vec::new();
        }
        let repaired = client_adapters::repair_client_setups();
        for client_id in &repaired {
            analytics::track_event(
                &app,
                "client_setup_auto_repaired",
                Some(json!({ "client": client_id })),
            );
        }
        repaired
    })
    .await
    .map_err(|err| err.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnroutedClient {
    client_id: String,
    name: String,
    /// Connection toggle is on (config present) yet the agent bypassed it.
    enabled: bool,
    /// Connection was re-applied just now; the agent needs a restart to pick
    /// it up. Only ever true when `enabled`.
    reapplied: bool,
    active_at: String,
}

/// Agents that ran on this machine while Headroom, up the whole time, saw
/// nothing from them: local session artifacts newer than any proxied request.
/// The hourly self-heal above cannot see this case, because the config files
/// verify fine - the agent just isn't reading them (connection switched off,
/// launched from a stale environment). An enabled connection is re-applied
/// on the spot; a disabled one is only reported, so the UI can ask before
/// turning it back on.
#[tauri::command]
async fn detect_unrouted_clients(
    app: AppHandle,
    app_started_at_ms: i64,
) -> Result<Vec<UnroutedClient>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use std::sync::{Mutex, OnceLock};
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
        let state: State<'_, AppState> = app.state();
        // Paused or bypassed: the agent going direct is the intended state.
        if state.runtime_is_paused()
            || state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Vec::new();
        }
        // ponytail: process-wide hourly throttle, same shape as
        // repair_client_setups; the artifact walk stats up to 20k entries.
        static LAST_SCAN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
        {
            let mut last = LAST_SCAN.get_or_init(|| Mutex::new(None)).lock().unwrap();
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(3600)) {
                return Vec::new();
            }
            *last = Some(Instant::now());
        }
        let app_started_at = UNIX_EPOCH + Duration::from_millis(app_started_at_ms.max(0) as u64);
        let now = SystemTime::now();
        let mut found = Vec::new();
        for (client_id, name, counter_key) in [
            ("codex", "ChatGPT", "codex"),
            ("claude_code", "Claude Code", "claude-code"),
        ] {
            let activity = client_adapters::client_local_activity_at(client_id);
            let requests = usage_counters::requests_since_yesterday(counter_key);
            if !client_adapters::client_ran_unrouted(activity, requests, app_started_at, now) {
                continue;
            }
            let enabled = match client_id {
                "codex" => client_adapters::is_codex_enabled(),
                _ => client_adapters::is_claude_code_enabled(),
            };
            let reapplied = enabled && client_adapters::apply_client_setup(client_id).is_ok();
            let active_at: chrono::DateTime<chrono::Utc> = activity.unwrap_or(now).into();
            // warn: the log bridge forwards it to Sentry, the only fleet-wide
            // trace of an agent silently running outside Headroom.
            log::warn!(
                "unrouted client {client_id}: active locally at {active_at}, no proxied request since yesterday; enabled={enabled} reapplied={reapplied}"
            );
            analytics::track_event(
                &app,
                "client_unrouted_detected",
                Some(json!({ "client": client_id, "enabled": enabled, "reapplied": reapplied })),
            );
            found.push(UnroutedClient {
                client_id: client_id.into(),
                name: name.into(),
                enabled,
                reapplied,
                active_at: active_at.to_rfc3339(),
            });
        }
        found
    })
    .await
    .map_err(|err| err.to_string())
}

#[tauri::command]
async fn detect_oss_remnants() -> Result<Vec<String>, String> {
    Ok(client_adapters::detect_oss_remnants())
}

#[tauri::command]
async fn get_client_connectors(
    state: State<'_, AppState>,
) -> Result<Vec<ClientConnectorStatus>, String> {
    client_adapters::list_client_connectors(&state.cached_clients()).map_err(|err| err.to_string())
}

#[tauri::command]
async fn disable_client_setup(app: AppHandle, client_id: String) -> Result<(), String> {
    client_adapters::disable_client_setup(&client_id).map_err(|err| err.to_string())?;
    analytics::track_event(
        &app,
        "client_setup_disabled",
        Some(json!({ "client_id": client_id })),
    );
    Ok(())
}

/// Frontend shape of the configured upstream. Separate from the persisted
/// struct so `launch-profile.json` keeps the snake_case of every other field
/// in it while the UI gets the camelCase it uses everywhere else. The token is
/// never in either -- only whether one is stored.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamOverrideView {
    mode: &'static str,
    base_url: String,
    has_token: bool,
    provider: String,
    model: String,
    context_window: String,
    /// The presets the dropdown offers. Shipped with the view so the labels and
    /// the values that get written can never drift apart.
    providers: Vec<ProviderPresetView>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPresetView {
    id: &'static str,
    label: &'static str,
    base_url: &'static str,
    model: &'static str,
}

impl From<crate::state::UpstreamOverride> for UpstreamOverrideView {
    fn from(value: crate::state::UpstreamOverride) -> Self {
        use crate::state::UpstreamOverrideMode::*;
        UpstreamOverrideView {
            mode: match value.mode {
                Off => "off",
                Fallback => "fallback",
                Override => "override",
            },
            base_url: value.base_url,
            has_token: value.has_token,
            provider: value.provider,
            model: value.model,
            context_window: value.context_window,
            providers: client_adapters::PROVIDER_PRESETS
                .iter()
                .map(|preset| ProviderPresetView {
                    id: preset.id,
                    label: preset.label,
                    base_url: preset.base_url,
                    model: preset.model,
                })
                .collect(),
        }
    }
}

#[tauri::command]
async fn get_upstream_override(app: AppHandle) -> UpstreamOverrideView {
    let state: tauri::State<'_, AppState> = app.state();
    state.upstream_override().into()
}

/// Save the configured upstream and restart the proxy onto it.
///
/// `token`: `None` leaves the stored one alone (the field renders as "set" and
/// is only sent when the user types a new one), `Some("")` clears it, anything
/// else replaces it.
///
/// `provider` is a preset id from `client_adapters::PROVIDER_PRESETS`, and when
/// set it supplies the URL, the model ids and the context window -- the user
/// only brings a token. Empty means the endpoint was entered by hand, in which
/// case `model` and `context_window` are optional: empty means "do not write
/// that key", which leaves a provider that maps Claude model ids itself alone.
#[tauri::command]
async fn save_upstream_override(
    app: AppHandle,
    mode: String,
    base_url: String,
    token: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    context_window: Option<String>,
) -> Result<UpstreamOverrideView, String> {
    use crate::state::{UpstreamOverride, UpstreamOverrideMode};

    let mode = match mode.as_str() {
        "off" => UpstreamOverrideMode::Off,
        "fallback" => UpstreamOverrideMode::Fallback,
        "override" => UpstreamOverrideMode::Override,
        other => return Err(format!("unknown upstream mode: {other}")),
    };

    let provider = provider.unwrap_or_default().trim().to_string();
    let preset = match provider.as_str() {
        "" => None,
        id => Some(
            client_adapters::provider_preset(id)
                .ok_or_else(|| format!("unknown provider: {id}"))?,
        ),
    };

    // Off keeps neither URL nor token: a cleared override must not leave a
    // provider credential behind in the client config for the next launch.
    let base_url = if mode == UpstreamOverrideMode::Off {
        String::new()
    } else {
        match preset {
            Some(preset) => preset.base_url.to_string(),
            None => crate::state::normalize_upstream_base_url(&base_url)?,
        }
    };

    let has_token = if mode == UpstreamOverrideMode::Off {
        upstream_override::delete_token()?;
        client_adapters::apply_upstream_auth_token(None).map_err(|err| err.to_string())?;
        false
    } else {
        match token.as_deref() {
            Some("") => {
                upstream_override::delete_token()?;
                client_adapters::apply_upstream_auth_token(None).map_err(|err| err.to_string())?;
                false
            }
            Some(value) => {
                upstream_override::write_token(value)?;
                client_adapters::apply_upstream_auth_token(Some(value))
                    .map_err(|err| err.to_string())?;
                true
            }
            // Untouched: re-apply the stored one, because cc-switch or a hand
            // edit may have overwritten the copy in the client's settings.
            None => match upstream_override::read_token() {
                Some(stored) => {
                    client_adapters::apply_upstream_auth_token(Some(&stored))
                        .map_err(|err| err.to_string())?;
                    true
                }
                None => false,
            },
        }
    };

    // Same rule as base_url and the token: Off keeps nothing, so a stale model
    // id cannot outlive the endpoint that served it.
    let configured = mode != UpstreamOverrideMode::Off;
    let (model, small_model, context_window) = match (configured, preset) {
        (false, _) => (String::new(), String::new(), String::new()),
        (true, Some(preset)) => (
            preset.model.to_string(),
            preset.small_model.to_string(),
            preset.context_window.to_string(),
        ),
        (true, None) => {
            // A hand-entered endpoint gets the one model id the user gave us in
            // every slot, cheap tier included: we have no way to know which
            // smaller model it serves.
            let model = model.unwrap_or_default().trim().to_string();
            let window = context_window.unwrap_or_default().trim().to_string();
            if !window.is_empty() && !window.chars().all(|c| c.is_ascii_digit()) {
                return Err("The context window must be a whole number of tokens.".into());
            }
            (model.clone(), model, window)
        }
    };
    client_adapters::apply_upstream_provider_env(configured.then_some(
        client_adapters::ProviderClientEnv {
            model: &model,
            small_model: &small_model,
            context_window: &context_window,
        },
    ))
    .map_err(|err| err.to_string())?;

    let next = UpstreamOverride {
        mode,
        base_url,
        has_token,
        provider: if configured { provider } else { String::new() },
        model,
        context_window,
    };
    let state: tauri::State<'_, AppState> = app.state();
    state.set_upstream_override(next.clone());

    // ANTHROPIC_TARGET_API_URL is read at boot, so the running proxy is still
    // pointed at the old upstream until it is replaced. Same hard restart the
    // paused-banner button uses: stop_headroom kills the group so a wedged
    // process cannot survive the change.
    state.stop_headroom();
    state.set_runtime_auto_paused(false);
    state.resume_runtime().map_err(|err| err.to_string())?;
    std::thread::spawn(|| {
        client_adapters::restore_client_setups();
    });
    analytics::track_event(
        &app,
        "upstream_override_saved",
        Some(json!({
            "mode": match next.mode {
                UpstreamOverrideMode::Off => "off",
                UpstreamOverrideMode::Fallback => "fallback",
                UpstreamOverrideMode::Override => "override",
            },
            // The endpoint itself is the user's business; only whether one and
            // a token are set.
            "has_base_url": !next.base_url.is_empty(),
            "has_token": next.has_token,
            // Which preset, or "custom" -- enough to see whether the dropdown
            // covers what people actually run without logging endpoints.
            "provider": if next.provider.is_empty() { "custom" } else { next.provider.as_str() },
        })),
    );
    Ok(next.into())
}

#[tauri::command]
async fn clear_client_setups() -> Result<(), String> {
    client_adapters::clear_client_setups().map_err(|err| err.to_string())
}

#[tauri::command]
async fn pause_headroom(app: AppHandle) -> Result<(), String> {
    let state: tauri::State<'_, AppState> = app.state();
    state.set_runtime_paused(true);
    // A deliberate user pause is not an auto-pause; clear the flag so the
    // self-heal loop doesn't fight the user by auto-resuming.
    state.set_runtime_auto_paused(false);
    state.stop_headroom();
    // Users grandfathered in before `setup_wizard_complete` existed satisfy the
    // onboarding gate only via "launch_count > 1 && a client is configured".
    // The clear below empties configured_clients, which flipped that gate false
    // and sent the next tray click into the launcher instead of the dashboard.
    // Freeze the answer we already have before clearing.
    if state.setup_wizard_satisfied() {
        state.mark_setup_wizard_complete();
    }
    client_adapters::clear_client_setups().map_err(|err| err.to_string())?;
    analytics::track_event(&app, "runtime_paused", None);
    Ok(())
}

#[tauri::command]
async fn start_headroom(app: AppHandle) -> Result<(), String> {
    let state: tauri::State<'_, AppState> = app.state();
    state.resume_runtime().map_err(|err| err.to_string())?;
    std::thread::spawn(|| {
        client_adapters::restore_client_setups();
    });
    analytics::track_event(&app, "runtime_resumed", None);
    Ok(())
}

/// Hard kill + restart of the proxy, wired to the "Resume" button on the
/// paused/auto-paused banner. Unlike `start_headroom`/`resume_runtime` — which
/// no-op when the tracked child is alive-but-hung — this kills the process
/// group first (`stop_headroom` SIGKILLs the group and reaps orphans), so a
/// wedged process is actually replaced by a fresh one. This is the one-click
/// equivalent of the manual quit-and-relaunch users do today.
#[tauri::command]
async fn force_restart_headroom(app: AppHandle) -> Result<(), String> {
    let state: tauri::State<'_, AppState> = app.state();
    state.stop_headroom();
    state.set_runtime_auto_paused(false);
    state.resume_runtime().map_err(|err| err.to_string())?;
    std::thread::spawn(|| {
        client_adapters::restore_client_setups();
    });
    analytics::track_event(&app, "runtime_force_restarted", None);
    Ok(())
}

#[tauri::command]
async fn hide_launcher_animated(app: AppHandle) {
    // The launcher close animation now lives in the webview/CSS layer.
    // Keep the backend hide on the straightforward window path instead of
    // mutating window geometry from a background thread.
    // Async so the hide doesn't queue behind main-thread work during startup;
    // window ops proxy to the main thread internally either way.
    if let Err(err) = hide_launcher_window(&app) {
        log::warn!("hide_launcher_animated: failed to hide launcher: {err:#}");
    }
}

/// Whether the running executable's path still resolves on disk. Mirrors the
/// `current_exe()?` + `canonicalize()?` pair that tauri-plugin-autostart does
/// at init, so we can skip the plugin instead of panicking out of startup.
fn exe_path_resolvable(exe: std::io::Result<std::path::PathBuf>) -> bool {
    exe.and_then(|path| path.canonicalize()).is_ok()
}

/// The autostart plugin is only registered when the executable path resolves
/// (see `run`), so every caller has to tolerate its absence. `app.autolaunch()`
/// panics when the plugin was skipped; this returns `None` instead.
fn autolaunch(
    app: &AppHandle,
) -> Option<tauri::State<'_, tauri_plugin_autostart::AutoLaunchManager>> {
    app.try_state::<tauri_plugin_autostart::AutoLaunchManager>()
}

#[cfg(target_os = "macos")]
const AUTOSTART_UNAVAILABLE: &str =
    "Autostart is unavailable: Headroom could not resolve its own application path. \
     Move Headroom to /Applications and relaunch.";

#[cfg(not(target_os = "macos"))]
const AUTOSTART_UNAVAILABLE: &str =
    "Autostart is unavailable: Headroom could not resolve its own application path. \
     Reinstall Headroom and relaunch.";

#[tauri::command]
async fn get_autostart_enabled(app: AppHandle) -> Result<bool, String> {
    let Some(manager) = autolaunch(&app) else {
        return Ok(false);
    };
    manager.is_enabled().map_err(|err| err.to_string())
}

#[tauri::command]
async fn set_autostart_enabled(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = autolaunch(&app).ok_or(AUTOSTART_UNAVAILABLE)?;
    if enabled {
        manager.enable().map_err(|err| err.to_string())?;
    } else {
        manager.disable().map_err(|err| err.to_string())?;
    }
    manager.is_enabled().map_err(|err| err.to_string())
}

#[tauri::command]
async fn set_rtk_enabled(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let state: tauri::State<'_, AppState> = app.state();
    client_adapters::set_rtk_enabled(
        enabled,
        &state.tool_manager.rtk_entrypoint(),
        &state.tool_manager.managed_python(),
    )
    .map_err(|err| err.to_string())?;
    state.invalidate_runtime_status_cache();
    let action = if enabled { "enabled" } else { "disabled" };
    analytics::track_event(&app, &format!("rtk_{action}"), None);
    Ok(!client_adapters::is_rtk_disabled())
}

#[tauri::command]
fn get_auto_learn_enabled() -> bool {
    !client_adapters::is_auto_learn_disabled()
}

/// Toggle passive traffic learning. The flag is only read when the proxy is
/// spawned, so restart it here to make the change take effect immediately.
/// Manual Learn scans are unaffected either way.
#[tauri::command]
async fn set_auto_learn_enabled(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let state: tauri::State<'_, AppState> = app.state();
    client_adapters::set_auto_learn_enabled(enabled).map_err(|err| err.to_string())?;
    state.stop_headroom();
    if let Err(err) = state.ensure_headroom_running() {
        log::warn!("set_auto_learn_enabled: proxy restart failed: {err:#}");
    }
    state.invalidate_runtime_status_cache();
    let action = if enabled { "enabled" } else { "disabled" };
    analytics::track_event(&app, &format!("auto_learn_{action}"), None);
    Ok(!client_adapters::is_auto_learn_disabled())
}

#[tauri::command]
async fn uninstall_and_quit(app: AppHandle) -> Result<Vec<String>, String> {
    // Prevent the launch-time OSS-plugin worker from mutating Claude's hook
    // cache after cleanup has restored it.
    SHUTTING_DOWN.store(true, Ordering::Release);
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.stop_headroom();
        // Plugin addons live in the hosts' plugin registries, outside Headroom's
        // own footprint that perform_full_cleanup() wipes, so remove them here
        // while we still have the ToolManager. Best-effort.
        for plugin_id in ["ponytail", "caveman"] {
            if let Err(err) = state.tool_manager.uninstall_plugin(plugin_id) {
                log::warn!("uninstall: removing {plugin_id} plugin failed: {err:#}");
            }
        }

        // Same story for the other addon integrations: MarkItDown's managed
        // blocks/permission and the MCP registrations (serena, context7,
        // codebase-memory) live in the host agents' own configs. Unregister
        // via the Python helpers while the runtime and MCP ledger still
        // exist; perform_full_cleanup() strips whatever a broken runtime
        // leaves behind. All best-effort.
        let _ = client_adapters::disable_markitdown_integration(
            &state.tool_manager.markitdown_shim_path(),
        );
        if state.tool_manager.serena_installed() {
            if let Err(err) = state.tool_manager.uninstall_serena() {
                log::warn!("uninstall: removing serena failed: {err:#}");
            }
        }
        if state.tool_manager.context7_installed() {
            if let Err(err) = state.tool_manager.uninstall_context7() {
                log::warn!("uninstall: removing context7 failed: {err:#}");
            }
        }
        if state.tool_manager.codebase_memory_installed() {
            if let Err(err) = state.tool_manager.uninstall_codebase_memory() {
                log::warn!("uninstall: removing codebase-memory failed: {err:#}");
            }
        }
    }

    // Turn off the login item if it was ever enabled, so the system stops
    // listing Headroom as a background item even if the user later reinstalls.
    if let Some(manager) = autolaunch(&app) {
        let _ = manager.disable();
    }

    let mut removed = client_adapters::perform_full_cleanup();

    // Trash the running .app bundle itself once we exit. Best-effort and
    // macOS-only; everything above only removed Headroom's on-disk footprint
    // (config, runtime, caches), not the application.
    #[cfg(target_os = "macos")]
    if let Some(bundle) = schedule_app_bundle_trash() {
        removed.push(bundle.display().to_string());
    }

    // Same on Windows, via the NSIS uninstaller.
    #[cfg(target_os = "windows")]
    if let Some(uninstaller) = schedule_windows_uninstaller() {
        removed.push(uninstaller.display().to_string());
    }

    analytics::track_event(
        &app,
        "uninstall_completed",
        Some(json!({ "removed_paths": removed.len() })),
    );
    analytics::shutdown(&app);
    if let Some(client) = sentry::Hub::current().client() {
        client.flush(Some(std::time::Duration::from_secs(2)));
    }

    let handle = app.clone();
    // Give the frontend a moment to receive the command response before the
    // process exits, so the confirmation toast can render.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        handle.exit(0);
    });

    Ok(removed)
}

#[tauri::command]
async fn quit_headroom(app: AppHandle) {
    exit_headroom(&app, QuitSource::SettingsButton);
}

fn launched_from_autostart() -> bool {
    std::env::args().any(|arg| arg == AUTOSTART_LAUNCH_ARG)
}

/// Handle `--uninstall` and exit; return normally otherwise.
///
/// Exists for package managers that delete the app themselves and so cannot
/// rely on the app being alive to clean up after itself: a Homebrew cask calls
/// this from its `uninstall script:` stanza, and the NSIS uninstaller calls it
/// from `NSIS_HOOK_PREUNINSTALL`, both *before* the binary is removed.
///
/// Scope differs by platform because the packagers do:
/// - macOS/Linux: revert our edits to *other* tools (agent configs, shell rc
///   blocks, hooks, MCP registrations, keychain, backup files) and leave
///   Headroom's own data alone. A cask's `uninstall` must not destroy user
///   data, that is what `zap` is for.
/// - Windows: full cleanup, data included. There is no `zap` counterpart, the
///   NSIS uninstaller is the only uninstall a user gets, and everything it
///   cannot reach itself (the multi-GB managed runtime, model caches,
///   ~\.headroom) would otherwise survive and be inherited by a reinstall.
///
/// Quitting a *running* instance already reverts the routing layer via
/// `clear_client_setups`, so this mainly covers the case where the app was
/// force-killed or never launched, plus the pieces quitting does not touch.
fn handle_uninstall_flag() {
    if !std::env::args().any(|arg| arg == UNINSTALL_LAUNCH_ARG) {
        return;
    }

    #[cfg(target_os = "windows")]
    let removed = client_adapters::perform_full_cleanup();
    #[cfg(not(target_os = "windows"))]
    let removed = client_adapters::revert_external_mutations();
    for path in &removed {
        println!("removed {path}");
    }
    println!("Headroom: reverted {} item(s).", removed.len());
    std::process::exit(0);
}

fn exit_headroom(app: &AppHandle, source: QuitSource) {
    SHUTTING_DOWN.store(true, Ordering::Release);
    let runtime_paused = {
        let state: tauri::State<'_, AppState> = app.state();
        let runtime_paused = state.runtime_is_paused();
        state.stop_headroom();
        // Mark the quit-time clear as done so the RunEvent::Exit handler skips
        // its redundant clear_client_setups(). A second call would wipe the
        // remembered_clients snapshot we just saved (configured_clients is now
        // empty, so the re-save is skipped while the disable loop still removes
        // remembered entries), leaving connectors disabled on next launch.
        if !EXIT_CLEAR_DONE.swap(true, Ordering::AcqRel) {
            let _ = client_adapters::clear_client_setups();
        }
        runtime_paused
    };

    analytics::track_event(
        app,
        "app_quit_requested",
        Some(app_quit_requested_properties(source, runtime_paused)),
    );
    analytics::shutdown(app);
    if let Some(client) = sentry::Hub::current().client() {
        client.flush(Some(std::time::Duration::from_secs(2)));
    }
    app.exit(0);
}

fn app_quit_requested_properties(source: QuitSource, runtime_paused: bool) -> Value {
    json!({
        "source": source.label(),
        "runtime_paused": runtime_paused,
    })
}

/// Tauri's build error is fatal either way, but on Windows the usual cause is a
/// missing WebView2 runtime and the old `.expect` panicked before any window
/// existed, so the user saw literally nothing (Sentry RUST-8J: 8 machines in 14
/// days, every one blocked on first run). Explain it, offer the download, then
/// panic as before so the event still reaches Sentry.
fn fatal_build_error(err: tauri::Error) -> ! {
    let message = err.to_string();
    #[cfg(target_os = "windows")]
    if is_missing_webview_runtime(&message) {
        show_webview2_missing_dialog();
    }
    panic!("error while building tauri application: {message}");
}

/// Matches `tauri_runtime::Error::WebviewRuntimeNotInstalled` by its Display
/// text, which is cheaper than taking a direct tauri-runtime dependency just to
/// name the variant.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_missing_webview_runtime(message: &str) -> bool {
    message.contains("webview runtime")
}

/// MessageBoxW rather than a Tauri dialog: there is no webview left to render
/// one in, which is the whole problem.
#[cfg(target_os = "windows")]
fn show_webview2_missing_dialog() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONERROR, MB_SETFOREGROUND, MB_YESNO,
    };

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let text = wide(concat!(
        "Headroom needs the Microsoft Edge WebView2 runtime, ",
        "which is not installed on this PC.\n\n",
        "Open the download page? Install the Evergreen Runtime, ",
        "then start Headroom again."
    ));
    let caption = wide("Headroom cannot start");
    let choice = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_ICONERROR | MB_SETFOREGROUND,
        )
    };
    if choice == IDYES {
        let _ = open_external_link_impl("https://developer.microsoft.com/microsoft-edge/webview2/");
    }
}

pub fn run() {
    let _sentry = sentry::init((
        SENTRY_DSN.unwrap_or(""),
        sentry::ClientOptions {
            release: sentry::release_name!(),
            attach_stacktrace: true,
            before_send: Some(std::sync::Arc::new(logging::sanitize_event)),
            ..Default::default()
        },
    ));

    // Initialize the panic-safe file logger after Sentry so warn!/error!
    // records flow into Sentry too. Failure here cannot abort startup.
    let _ = logging::init();

    // Linux AppImage launches export PYTHONHOME pointing into the transient
    // /tmp/.mount_* squashfs (RUST-5C/RUST-1M: the managed venv's python
    // resolved its stdlib there and exited 1 before the proxy port opened,
    // bricking every start). Nothing we spawn ever wants the host's
    // PYTHONHOME; scrub it once here so every child -- proxy, health checks,
    // addons -- starts clean, instead of auditing each spawn site.
    std::env::remove_var("PYTHONHOME");

    // Must come before any Tauri/state setup: this path never shows a window and
    // never starts the proxy, it just undoes our edits to other tools and exits.
    handle_uninstall_flag();

    // Raise the open-file soft limit to the hard limit. macOS launches GUI apps
    // with RLIMIT_NOFILE soft = 256, which the intercept proxy exhausts under
    // bursty load (each proxied request holds a client + backend FD), producing
    // EMFILE on accept(). The hard limit is far higher; the kernel clamps to
    // kern.maxfilesperproc if rlim_max is RLIM_INFINITY.
    #[cfg(unix)]
    unsafe {
        let mut lim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur < lim.rlim_max {
            lim.rlim_cur = lim.rlim_max;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }

    #[cfg(target_os = "linux")]
    {
        let has_display =
            std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok();
        if !has_display {
            log::error!(
                "Headroom requires a graphical display. Set DISPLAY or WAYLAND_DISPLAY before launching."
            );
            std::process::exit(1);
        }
    }

    // Raw pre-upgrade snapshot of the user-state files, before AppState::new
    // can parse (and a schema change silently reset) any of them. See check 14
    // of docs/beta-smoke-test.md.
    storage::snapshot_state_on_version_change(&storage::app_data_dir(), env!("CARGO_PKG_VERSION"));

    let state = AppState::new().expect("failed to create app state");

    // A previous bootstrap attempt that never reached a verdict: the app was
    // quit, crashed, or killed mid-install, so neither bootstrap_completed nor
    // the error branch ever ran. Production funnel data (2026-08-26) shows
    // these silent deaths outnumber classified failures ~4:1; this is the only
    // signal they leave.
    if let Some(abandoned) = state.tool_manager.take_abandoned_bootstrap() {
        // The tail of the previous run's app log usually holds the last thing
        // the install did before dying. Same 12KB cap as
        // capture_upgrade_failure: Sentry drops extras past ~16KB. Connection-
        // pool debug lines are dropped first so the budget goes to the install
        // rather than to health polling (RUST-9Y).
        let log_tail = std::fs::read_to_string(logging::log_path())
            .ok()
            .map(|s| tail_bytes_for_sentry(&strip_connection_noise(&s), SENTRY_EXTRA_TAIL_BYTES))
            .unwrap_or_else(|| "app log unreadable".into());
        sentry::with_scope(
            |scope| {
                let fp = ["bootstrap_abandoned", abandoned.step.as_str()];
                scope.set_fingerprint(Some(fp.as_slice()));
                scope.set_tag("abandoned_step", &abandoned.step);
                scope.set_extra("percent", u64::from(abandoned.percent).into());
                scope.set_extra("app_log_tail", log_tail.into());
            },
            || {
                sentry::capture_message(
                    &format!(
                        "bootstrap_abandoned (died at \"{}\" {}%)",
                        abandoned.step, abandoned.percent
                    ),
                    sentry::Level::Warning,
                );
            },
        );
        // Funnel mirror so the server-side stall query can tell "died
        // mid-install but came back" from "gone for good". Unknown step names
        // are ignored by servers that predate this one.
        pricing::report_funnel_step(&state, "bootstrap_abandoned");
    }

    let mut builder =
        tauri::Builder::default().plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // Second launch: focus the existing window and exit the new process.
            let _ = show_launcher_window(app);
            // On Windows/Linux the OS answers a `headroom://` link by spawning
            // a NEW process with the URL in argv; the running instance is never
            // notified, and the new one dies here. Replay argv into the primary
            // instance's deep-link plugin so `on_open_url` fires and the magic
            // link is actually redeemed instead of just raising the launcher.
            // No cfg guard: `handle_cli_arguments` already no-ops off
            // Windows/Linux, and compiling it everywhere keeps macOS CI
            // checking the call.
            use tauri_plugin_deep_link::DeepLinkExt;
            app.deep_link().handle_cli_arguments(args.into_iter());
        }));

    // tauri-plugin-autostart canonicalizes current_exe() while initializing and
    // fails with ENOENT when the bundle path no longer resolves - the .app was
    // moved, replaced mid-launch, or sits on an unmounted volume. Builder::build
    // turns any plugin init failure into a panic, so that bricked startup
    // entirely (RUST-6Q). Autostart is optional; launching is not. Every
    // `autolaunch()` caller handles the plugin being absent.
    if exe_path_resolvable(std::env::current_exe()) {
        builder = builder.plugin(
            tauri_plugin_autostart::Builder::new()
                .args([AUTOSTART_LAUNCH_ARG])
                .build(),
        );
    } else {
        log::warn!(
            "autostart plugin skipped: current_exe() does not resolve (bundle moved or replaced); \
             launching without login-item support"
        );
    }

    let builder = builder
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init());

    builder
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                // Accessory policy makes this a menu-bar-only app (no dock icon).
                // Do NOT also call set_dock_visibility(false): it uses Carbon's
                // TransformProcessType, which Apple warns must not be mixed with
                // setActivationPolicy on the same process and intermittently
                // registers a dock icon. LSUIElement=true in Info.plist already
                // covers the packaged bundle.
                app.set_activation_policy(ActivationPolicy::Accessory);

                // The tray dropdown must open on whatever Space/desktop the
                // user is currently on. A plain NSWindow stays bound to the
                // Space it was last shown on, so with multiple desktops (or
                // "Displays have separate Spaces") show() revealed it on the
                // wrong desktop. CanJoinAllSpaces is the standard menu-bar
                // popover behavior and fixes both cases.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_visible_on_all_workspaces(true);
                }
            }

            // Windows and Linux render an extra preview-build notice in the
            // callout banner that macOS never shows, and their scrollbars take
            // layout width (macOS overlays them), so the same 760x560 frame
            // clips the bottom of Home. Give those platforms the banner's
            // height back. Applied here rather than in tauri.conf.json because
            // Tauri's platform config overlay replaces the whole `windows`
            // array, which would duplicate every field just to change one.
            // The launcher needs the same treatment: it is fixed-size too, and
            // the wider system font wrapping its headline to three lines pushed
            // the install screen into `.intro-shell`'s scroll fallback.
            #[cfg(not(target_os = "macos"))]
            {
                use tauri::LogicalSize;
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_size(LogicalSize::new(
                        MAIN_WINDOW_WIDTH,
                        MAIN_WINDOW_HEIGHT + PREVIEW_NOTICE_EXTRA_HEIGHT,
                    ));
                }
                if let Some(window) = app.get_webview_window("launcher") {
                    let _ = window.set_size(LogicalSize::new(
                        LAUNCHER_WINDOW_WIDTH,
                        LAUNCHER_WINDOW_HEIGHT + PREVIEW_NOTICE_EXTRA_HEIGHT,
                    ));
                    let _ = window.center();
                }
            }

            let launched_from_autostart = launched_from_autostart();
            // Autostart is opt-in. Users enable it explicitly from Settings,
            // which avoids triggering macOS's "Background item added" prompt
            // on first launch.

            app.manage(analytics::AnalyticsClient::new(
                app.package_info().version.to_string(),
            ));
            app.manage(TraySessionSavings(Mutex::new(0.0)));
            setup_tray(app.handle())?;
            spawn_tray_runtime_icon_updater(app.handle().clone());
            spawn_tray_savings_updater(app.handle().clone());
            spawn_proxy_watchdog(app.handle().clone());
            spawn_activity_observer(app.handle().clone());
            spawn_claude_projects_warmer(app.handle().clone());
            let state: tauri::State<'_, AppState> = app.state();
            let app_handle = app.handle().clone();
            analytics::set_headroom_ai_version(
                &app_handle,
                state.tool_manager.installed_headroom_version(),
            );
            analytics::track_event(
                &app_handle,
                "app_started",
                Some(json!({
                    "launch_experience": state.launch_experience_label(),
                    "launch_count": state.launch_count(),
                    "runtime_installed": state.tool_manager.python_runtime_installed(),
                    "autostart_launch": launched_from_autostart
                })),
            );

            // Absorb the open-source `headroom` Claude Code plugin. Its hooks
            // run a bare `headroom init hook ensure`, which exits 127 here
            // because the app ships no `headroom` on PATH. When the plugin is
            // present and no real CLI is visible, replace only that exact hook
            // command with `exit 0`. No global PATH changes, nothing pointing
            // at a file of ours that could later go missing, and the same
            // rewrite works on Windows, macOS, and Linux.
            // `HEADROOM_ABSORB_OSS_PLUGIN=0` opts out and restores.
            //
            // Off the setup closure: nothing downstream waits on the result,
            // and the status probe opens a TCP connection to :8787. That must
            // never sit on the main thread's launch path, least of all for the
            // majority of users who do not have the plugin at all.
            // A plugin update lands a fresh version dir with the bare command
            // back, so one pass at startup is not enough for a tray app that
            // stays up for weeks. Re-check on a slow timer; the check itself is
            // a receipt read that returns immediately for everyone we are not
            // already managing.
            const OSS_PLUGIN_RECHECK_INTERVAL: std::time::Duration =
                std::time::Duration::from_secs(300);
            let oss_handle = app_handle.clone();
            std::thread::spawn(move || {
                let oss = client_adapters::absorb_oss_plugin();
                // Only worth an event when there is something to report; every
                // launch already counts in `app_started`. `base_url_ours: false`
                // alongside `oss_proxy_8787: true` is the state where the user
                // ran `headroom init` themselves and their traffic bypasses us,
                // so our savings under-report — measured, not fought.
                if oss.plugin_installed || oss.oss_proxy_8787 {
                    analytics::track_event(
                        &oss_handle,
                        "oss_plugin_detected",
                        Some(json!({
                            "plugin_installed": oss.plugin_installed,
                            "hook_absorbed": oss.hook_absorbed,
                            "cli_on_path": oss.cli_on_path,
                            "oss_proxy_8787": oss.oss_proxy_8787,
                            "base_url_ours": oss.base_url_ours
                        })),
                    );
                }
                // Startup cadence for the event, not for the repair.
                loop {
                    std::thread::sleep(OSS_PLUGIN_RECHECK_INTERVAL);
                    if SHUTTING_DOWN.load(Ordering::Acquire) {
                        return;
                    }
                    if client_adapters::oss_plugin_hook_needs_absorbing() {
                        client_adapters::absorb_oss_plugin();
                    }
                }
            });
            // Wire up the bearer-triggered identity-pusher worker. The
            // intercept thread sends a signal here every time it captures a
            // bearer whose value differs from what was previously in the
            // slot; the worker calls `pricing::warm_and_push_identity`,
            // which warms the OAuth profile cache and posts the populated
            // IdentityPayload to `desktop/grace/start`. Throttled to one
            // OAuth fetch per 24 h once the identity is complete.
            //
            // Each iteration is wrapped in `catch_unwind` so a panic inside
            // the HTTP / parsing stack doesn't silently kill the worker
            // thread (which would leave bearer signals piling up in the
            // channel forever). On panic we log + report and resume the
            // recv loop on the next signal.
            // Warm the paywall-first flag cache once per launch. Fire-and-forget:
            // get_launch_flags serves cached-or-false immediately either way.
            std::thread::Builder::new()
                .name("paywall-flag-fetch".into())
                .spawn(pricing::refresh_paywall_first_flag)
                .ok();

            let (fresh_bearer_tx, fresh_bearer_rx) = std::sync::mpsc::channel::<()>();
            let app_handle_for_pusher = app.handle().clone();
            std::thread::Builder::new()
                .name("identity-pusher".into())
                .spawn(move || {
                    while fresh_bearer_rx.recv().is_ok() {
                        // Coalesce: drain any signals that piled up while
                        // we were processing the previous one.
                        while fresh_bearer_rx.try_recv().is_ok() {}
                        let app_handle = app_handle_for_pusher.clone();
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                                let state: tauri::State<'_, AppState> = app_handle.state();
                                pricing::warm_and_push_identity(&state);
                            }));
                        if result.is_err() {
                            log::error!(
                                "identity-pusher worker panicked during warm_and_push_identity"
                            );
                            sentry::capture_message(
                                "identity-pusher worker panicked",
                                sentry::Level::Error,
                            );
                        }
                    }
                })
                .expect("spawn identity pusher");

            // Liveness ping: an idle app (no agent traffic) otherwise makes
            // zero backend calls after launch, so the server cannot tell
            // "running but idle" from "quit" — last_active_at freezes and
            // check-in emails misfire. get_pricing_status posts grace/start
            // and, when signed in, GETs desktop/account (which refreshes
            // last_active_at); it also re-evaluates the server-silent /
            // auth-silent Sentry alarms on processes that run for days.
            // Status is read, not applied: gate changes keep flowing through
            // the existing lifecycle triggers only.
            let app_handle_for_ping = app.handle().clone();
            std::thread::Builder::new()
                .name("liveness-ping".into())
                .spawn(move || loop {
                    std::thread::sleep(LIVENESS_PING_INTERVAL);
                    let app_handle = app_handle_for_ping.clone();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let state: tauri::State<'_, AppState> = app_handle.state();
                        let _ = pricing::get_pricing_status(&state);
                    }));
                    if result.is_err() {
                        log::error!("liveness ping panicked");
                    }
                })
                .expect("spawn liveness ping");

            // Start the intercept layer before anything else touches port 6767.
            proxy_intercept::spawn(
                std::sync::Arc::clone(&state.claude_bearer_token),
                std::sync::Arc::clone(&state.codex_rate_limits),
                std::sync::Arc::clone(&state.codex_plan_tier),
                std::sync::Arc::clone(&state.proxy_bypass),
                std::sync::Arc::clone(&state.claude_only_bypass),
                std::sync::Arc::clone(&state.codex_bypass),
                fresh_bearer_tx,
                std::sync::Arc::clone(&state.intercept_bind_error),
            );
            if state.should_present_on_launch() && !launched_from_autostart {
                let _ = show_launcher_window(app.handle());
            }
            if state.tool_manager.python_runtime_installed() {
                state.set_runtime_starting(true);
            }
            // Strip noisy traffic_learner error_recovery patterns before the
            // proxy starts re-flushing them. See memory_scrubber for context.
            std::thread::spawn(|| {
                memory_scrubber::scrub_all(&headroom_memory_db_path());
            });
            std::thread::spawn(move || {
                let state: tauri::State<'_, AppState> = app_handle.state();
                state.warm_runtime_on_launch(&app_handle);
            });
            // Restore previously connected client integrations in the background.
            std::thread::spawn(|| {
                client_adapters::restore_client_setups();
                // restore_client_setups only retags Codex threads back to the
                // headroom provider for clients in `remembered_clients`, which a
                // plain Cmd-Q / dock quit / app-update restart never populates
                // (only pause and the Settings "Quit" do). Those exit paths still
                // run the quit-time headroom->openai retag, so without this the
                // Codex history menu stays empty after an update restart. Mirror
                // the quit retag whenever Codex is still configured.
                if client_adapters::is_codex_enabled() {
                    client_adapters::retag_codex_threads_to_headroom();
                }
            });

            // headroom:// deep link — Polar's checkout success page redirects
            // here. Triggers an immediate pricing refresh so the gate releases
            // within seconds of payment instead of waiting for the 5s poll.
            use tauri_plugin_deep_link::DeepLinkExt;
            let deep_link_app = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                // NOTE: never call `eprintln!`/`println!` here. When macOS
                // launches the app fresh via a URL scheme, stderr is not
                // connected to a valid fd and any stdio write panics with
                // EIO. Use `log::*` (panic-safe file logger) instead.
                //
                // This callback is invoked synchronously from tao's
                // `application:openURLs:` handler, which is `extern "C"` -
                // any panic that escapes here aborts the whole process via
                // `panic_cannot_unwind`. Wrap the body in `catch_unwind` so
                // an internal failure degrades gracefully instead.
                let deep_link_app = deep_link_app.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for url in event.urls() {
                        if url.scheme() == "headroom" {
                            handle_headroom_deep_link(&deep_link_app, &url);
                            // Only handle the first headroom:// URL in the batch.
                            break;
                        }
                    }
                }));
                if result.is_err() {
                    sentry::capture_message("deep link callback panicked", sentry::Level::Error);
                }
            });

            // Cold start on Windows/Linux: the OS passes the URL as argv and
            // the plugin parses it during its own init - before the listener
            // above existed - so the launching URL only survives in
            // `get_current()`. Always `None` on macOS, which delivers via
            // `RunEvent::Opened` after setup.
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    if url.scheme() == "headroom" {
                        handle_headroom_deep_link(&app.handle().clone(), &url);
                        break;
                    }
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| handle_window_event(window, event))
        .manage(state)
        .manage(PendingAppUpdate(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            get_dashboard_state,
            get_app_update_configuration,
            check_for_app_update,
            install_app_update,
            restart_app,
            show_app_update_notification,
            show_notification,
            install_addon,
            set_addon_enabled,
            uninstall_addon,
            prefetch_bootstrap_artifacts,
            start_bootstrap,
            get_bootstrap_progress,
            get_bootstrap_failure_report,
            get_runtime_upgrade_progress,
            retry_runtime_upgrade,
            retry_runtime_upgrade_with_rebuild,
            dismiss_runtime_upgrade_failure,
            get_runtime_status,
            get_headroom_logs,
            get_headroom_request_count,
            get_headroom_request_counts_by_agent,
            get_intercept_request_counts_by_agent,
            get_running_agent_process_counts,
            install_claude_code_cli,
            get_launch_flags,
            get_rtk_activity,
            get_tool_logs,
            get_claude_code_projects,
            get_claude_usage,
            get_claude_profile,
            get_headroom_pricing_status,
            report_funnel_step,
            take_pending_magic_link,
            request_headroom_auth_code,
            verify_headroom_auth_code,
            sign_out_headroom_account,
            activate_headroom_account,
            create_headroom_checkout_session,
            change_headroom_subscription_plan,
            reactivate_headroom_subscription,
            get_headroom_billing_portal_url,
            submit_headroom_cancellation_intent,
            get_activity_feed,
            list_live_learnings,
            list_live_learnings_for_projects,
            delete_live_learning,
            list_applied_patterns,
            list_applied_patterns_for_projects,
            delete_applied_pattern,
            get_headroom_learn_status,
            get_headroom_learn_prereq_status,
            get_transformations_feed,
            start_headroom_learn,
            apply_client_setup,
            verify_client_setup,
            repair_client_setups,
            detect_unrouted_clients,
            detect_oss_remnants,
            get_client_connectors,
            disable_client_setup,
            clear_client_setups,
            get_upstream_override,
            save_upstream_override,
            pause_headroom,
            start_headroom,
            force_restart_headroom,
            track_analytics_event,
            get_debug_overrides,
            show_dashboard_window,
            open_headroom_dashboard,
            open_external_link,
            submit_contact_request,
            hide_launcher_animated,
            complete_setup_wizard,
            accept_terms,
            get_autostart_enabled,
            set_autostart_enabled,
            set_rtk_enabled,
            get_auto_learn_enabled,
            set_auto_learn_enabled,
            uninstall_and_quit,
            quit_headroom,
            #[cfg(debug_assertions)]
            debug_force_proxy_bypass
        ])
        .build(tauri::generate_context!())
        .unwrap_or_else(|err| fatal_build_error(err))
        .run(|app, event| {
            // Tear down the proxy on every exit path (Cmd-Q, dock quit, signal,
            // or our explicit quit/restart commands). Without this, the proxy
            // outlives the desktop and the next launch reuses an orphan.
            if matches!(
                event,
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
            ) {
                SHUTTING_DOWN.store(true, Ordering::Release);
                // Step markers: this teardown runs on the UI thread, so a step
                // that blocks freezes the app mid-quit and emits nothing (Sentry
                // only receives warn!/error!). The last marker in the log names
                // the step that hung.
                log::info!("exit: stop_headroom");
                let state: tauri::State<'_, AppState> = app.state();
                state.stop_headroom();
                // Gracefully reverse every client's base-URL override (and shell
                // blocks) on quit so Claude Code / Codex fall back to talking
                // directly to their native providers while Headroom is not
                // running, instead of pointing at a now-dead proxy on 6767. The
                // snapshot is remembered so the next launch's
                // restore_client_setups re-applies it. Guarded to run once: the
                // exit handler fires for both ExitRequested and Exit, and a
                // second clear_client_setups wipes the remembered snapshot.
                if !EXIT_CLEAR_DONE.swap(true, Ordering::AcqRel) {
                    log::info!("exit: clear_client_setups");
                    if let Err(err) = client_adapters::clear_client_setups() {
                        log::warn!("exit: clear_client_setups failed: {err}");
                    }
                }
                // Hand Codex threads back to the native provider so its history
                // menu stays whole while Headroom is not running. Cmd-Q / dock
                // quit / signals skip exit_headroom -> clear_client_setups, so
                // this is the only retag they get; the next launch re-applies the
                // headroom tag via restore_client_setups. Best-effort.
                log::info!("exit: retag_codex_threads_to_native");
                client_adapters::retag_codex_threads_to_native();
                log::info!("exit: teardown complete");
            }
        });
}

fn subscription_tier_label(tier: &HeadroomSubscriptionTier) -> &'static str {
    match tier {
        HeadroomSubscriptionTier::Pro => "pro",
        HeadroomSubscriptionTier::Max5x => "max5x",
        HeadroomSubscriptionTier::Max20x => "max20x",
    }
}

fn lifetime_token_milestone_kind(milestone_tokens_saved: u64) -> &'static str {
    match milestone_tokens_saved {
        1_000_000 => "first_1m",
        5_000_000 => "first_5m",
        10_000_000 => "first_10m",
        _ => "repeating_10m",
    }
}

/// How many recent days of savings travel with the milestone/heartbeat post.
const SAVINGS_REPORT_DAYS: usize = 30;

/// The output-reduction fields the savings report should carry. The desktop
/// requests the shaper, but the wheel's rollout gate can block it by channel
/// (all stable installs on the 0.37.0 wheel). A blocked shaper produces no
/// live reduction, so the ledger-recomputed figure would report an "estimated"
/// percentage for a feature that never ran; label it inactive and withhold the
/// percent instead. Unknown state (older wheels without the rollout block)
/// reports as before.
fn reported_output_reduction(
    reduction: Option<&crate::models::OutputReduction>,
    shaper_active: Option<bool>,
) -> (Option<f64>, Option<String>) {
    if shaper_active == Some(false) {
        return (None, Some("inactive".to_string()));
    }
    (
        reduction.map(|o| o.reduction_percent),
        reduction.map(|o| o.method.clone()),
    )
}

/// Projects the dashboard's real savings figures into the payload
/// headroom-web stores for the admin profile. Must be called on the dashboard
/// BEFORE `maybe_inject_fake_daily_savings`, or demo data reaches the server.
///
/// `None` until `/stats-history` has hydrated `savings_breakdown`. The first
/// dashboard render each session fires a report immediately, which on a cold
/// launch beats the backend; a snapshot built then would carry zero rate
/// denominators, and the server overwrites its stored snapshot
/// unconditionally, so those zeros clobber the previous good report and the
/// admin profile renders blank rates (user 1681, 2026-09-02). The milestone
/// heartbeat still posts without the snapshot; the next due report carries
/// the real figures.
fn savings_report(dashboard: &DashboardState) -> Option<pricing::SavingsReport> {
    let breakdown = dashboard.savings_breakdown.as_ref()?;
    let (output_reduction_percent, output_reduction_method) = reported_output_reduction(
        dashboard.output_reduction.as_ref(),
        dashboard.output_shaper_active,
    );
    Some(pricing::SavingsReport {
        lifetime_savings_usd: dashboard.lifetime_estimated_savings_usd,
        lifetime_tokens_saved: dashboard.lifetime_estimated_tokens_saved,
        total_input_tokens: breakdown.total_input_tokens,
        cache_read_tokens: breakdown.cache_read_tokens,
        total_input_cost_usd: breakdown.total_input_cost_usd,
        cache_savings_usd: breakdown.cache_savings_usd,
        output_reduction_percent,
        output_reduction_method,
        reread_tokens: dashboard.reread_tokens,
        reread_compressed_tokens: dashboard.reread_compressed_tokens,
        ccr_retrievals: dashboard.ccr_retrievals,
        days: recent_savings_days(&dashboard.daily_savings),
    })
}

/// The most recent `SAVINGS_REPORT_DAYS` days that saw any traffic, oldest
/// first. Empty days are skipped so a user who was away for a week still
/// reports a full window of real activity.
fn recent_savings_days(points: &[DailySavingsPoint]) -> Vec<pricing::SavingsDay> {
    // The counters use local day keys, exact for the merged series' recent
    // (local-tracker) buckets and approximate for its older UTC rollup days —
    // same boundary caveat the server's UserDailySaving already documents.
    let counters = usage_counters::recent_days();
    let mut days: Vec<_> = points
        .iter()
        .filter(|point| point.estimated_tokens_saved > 0 || point.total_tokens_sent > 0)
        .rev()
        .take(SAVINGS_REPORT_DAYS)
        .map(|point| {
            let day_counters = counters.get(&point.date);
            pricing::SavingsDay {
                date: point.date.clone(),
                savings_usd: point.estimated_savings_usd,
                output_savings_usd: point.output_savings_usd,
                tokens_saved: point.estimated_tokens_saved,
                tokens_sent: point.total_tokens_sent,
                actual_cost_usd: point.actual_cost_usd,
                cache_read_tokens: point.cache_read_tokens,
                cache_savings_usd: point.cache_savings_usd,
                output_sampled_tokens_saved: point.output_sampled_tokens_saved,
                output_baseline_tokens: point.output_baseline_tokens,
                client_requests: day_counters.map(|c| c.client_requests.clone()),
                rate_limit_429s: day_counters.map(|c| c.rate_limit_429s.clone()),
            }
        })
        .collect();
    days.reverse();
    days
}

fn is_prerelease_version(version: &str) -> bool {
    version.contains('-')
}

fn beta_channel_enabled_from(env: Option<&str>, sentinel_exists: bool) -> bool {
    let env_yes = matches!(
        env.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1") | Some("true") | Some("yes")
    );
    env_yes || sentinel_exists
}

fn beta_channel_enabled() -> bool {
    let env = std::env::var(BETA_CHANNEL_ENV).ok();
    let sentinel_exists = crate::storage::app_data_dir()
        .join(BETA_CHANNEL_SENTINEL)
        .exists();
    beta_channel_enabled_from(env.as_deref(), sentinel_exists)
}

fn select_updater_endpoints<'a>(
    configured_stable: Option<&'a str>,
    configured_staging: Option<&'a str>,
    prefer_staging: bool,
) -> Option<&'a str> {
    if prefer_staging {
        configured_staging.or(configured_stable)
    } else {
        configured_stable
    }
}

fn release_updater_config(
    current_version: &str,
    beta_channel_enabled: bool,
) -> Result<Option<ReleaseUpdaterConfig>, String> {
    resolve_release_updater_config(
        current_version,
        beta_channel_enabled,
        UPDATER_PUBLIC_KEY,
        UPDATER_ENDPOINTS,
        UPDATER_STAGING_ENDPOINTS,
        cfg!(debug_assertions),
    )
}

fn resolve_release_updater_config(
    current_version: &str,
    beta_channel_enabled: bool,
    configured_pubkey: Option<&str>,
    configured_stable: Option<&str>,
    configured_staging: Option<&str>,
    debug_assertions: bool,
) -> Result<Option<ReleaseUpdaterConfig>, String> {
    let configured_pubkey = configured_pubkey
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let configured_stable = configured_stable
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let configured_staging = configured_staging
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let prefer_staging = is_prerelease_version(current_version) || beta_channel_enabled;
    let configured_endpoints =
        select_updater_endpoints(configured_stable, configured_staging, prefer_staging);

    match (configured_pubkey, configured_endpoints) {
        (Some(pubkey), Some(endpoint_spec)) => {
            build_release_updater_config(pubkey, endpoint_spec).map(Some)
        }
        (Some(_), None) => Err(
            "Updater public key is configured, but HEADROOM_UPDATER_ENDPOINTS is missing."
                .to_string(),
        ),
        (None, Some(_)) => Err(
            "HEADROOM_UPDATER_ENDPOINTS is configured, but HEADROOM_UPDATER_PUBLIC_KEY is missing."
                .to_string(),
        ),
        (None, None) => {
            if debug_assertions {
                Ok(None)
            } else {
                build_release_updater_config(DEFAULT_UPDATER_PUBLIC_KEY, DEFAULT_UPDATER_ENDPOINT)
                    .map(Some)
            }
        }
    }
}

fn build_release_updater_config(
    pubkey: &str,
    endpoint_spec: &str,
) -> Result<ReleaseUpdaterConfig, String> {
    let endpoints = parse_updater_endpoint_list(endpoint_spec)?;

    if endpoints.is_empty() {
        return Err("HEADROOM_UPDATER_ENDPOINTS did not include any valid URLs.".into());
    }

    Ok(ReleaseUpdaterConfig {
        pubkey: pubkey.to_string(),
        endpoints,
    })
}

fn parse_updater_endpoint_list(raw: &str) -> Result<Vec<reqwest::Url>, String> {
    let values = if let Ok(json) = serde_json::from_str::<Vec<String>>(raw) {
        let values = json
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !values.is_empty() {
            values
        } else {
            Vec::new()
        }
    } else {
        raw.split(|ch| ch == ',' || ch == '\n')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    };

    if values.is_empty() {
        return Err(
            "HEADROOM_UPDATER_ENDPOINTS must be a JSON array or comma-separated list of HTTPS URLs."
                .into(),
        );
    }

    values
        .into_iter()
        .map(|value| {
            let url = reqwest::Url::parse(&value)
                .map_err(|err| format!("Invalid updater URL {value}: {err}"))?;
            if url.scheme() != "https" {
                return Err(format!("Updater endpoint {} must use HTTPS.", url.as_str()));
            }
            Ok(url)
        })
        .collect()
}

pub fn headroom_memory_db_path() -> std::path::PathBuf {
    crate::storage::memory_db_path(&crate::storage::app_data_dir())
}

/// Which coding agent a Headroom Learn run targets. Determines the session
/// source, the analysis backend, and which context/memory files get written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LearnAgent {
    Claude,
    Codex,
    Opencode,
    Grok,
}

impl LearnAgent {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "claude" => Ok(LearnAgent::Claude),
            "codex" => Ok(LearnAgent::Codex),
            "opencode" => Ok(LearnAgent::Opencode),
            "grok" => Ok(LearnAgent::Grok),
            other => Err(format!("Unknown Headroom Learn agent: {other}")),
        }
    }

    /// Stable Sentry tag value; inverse of `parse`.
    fn as_tag(self) -> &'static str {
        match self {
            LearnAgent::Claude => "claude",
            LearnAgent::Codex => "codex",
            LearnAgent::Opencode => "opencode",
            LearnAgent::Grok => "grok",
        }
    }
}

pub(crate) fn detect_headroom_learn_prereq_status() -> HeadroomLearnPrereqStatus {
    let claude_path = claude_cli::detect_claude_cli();
    let codex_path = client_adapters::detect_codex_cli();
    HeadroomLearnPrereqStatus {
        claude_cli_available: claude_path.is_some(),
        claude_cli_path: claude_path.map(|p| p.display().to_string()),
        codex_cli_available: codex_path.is_some(),
        codex_cli_path: codex_path.map(|p| p.display().to_string()),
        codex_logged_in: client_adapters::codex_logged_in(),
    }
}

fn check_headroom_learn_prereqs(
    agent: LearnAgent,
    platform_disabled_reason: Option<&str>,
    prereq: &HeadroomLearnPrereqStatus,
) -> Result<(), String> {
    if let Some(reason) = platform_disabled_reason {
        return Err(reason.to_string());
    }
    match agent {
        LearnAgent::Claude => {
            if !prereq.claude_cli_available {
                return Err(
                    "Install the Claude Code CLI (`claude`) to enable Headroom Learn.".into(),
                );
            }
        }
        LearnAgent::Codex => {
            if !prereq.codex_cli_available {
                return Err(
                    "Install the Codex CLI (`codex`) to enable Headroom Learn for Codex.".into(),
                );
            }
            if !prereq.codex_logged_in {
                return Err("Sign in to the Codex CLI with your ChatGPT account to enable Headroom Learn for Codex.".into());
            }
        }
        // OpenCode/Grok sessions are read directly from disk; the analysis
        // step still needs an LLM, which the desktop runs through the Claude
        // Code or Codex CLI (no API keys are available in the app env).
        LearnAgent::Opencode | LearnAgent::Grok => {
            if !prereq.claude_cli_available
                && !(prereq.codex_cli_available && prereq.codex_logged_in)
            {
                return Err(
                    "Headroom Learn analyzes sessions with the Claude Code or a signed-in Codex CLI - install one to enable it for this agent.".into(),
                );
            }
        }
    }
    Ok(())
}

/// Count entries in a `headroom memory export` JSON payload whose `created_at`
/// parses into the same UTC day as `now`. The export writes `created_at` as an
/// RFC3339-ish string without a timezone suffix (`2026-04-21T10:00:00`); we
/// treat those as UTC, matching the rest of the activity pipeline.
fn count_memories_created_today(
    json: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<usize, String> {
    let raw: Vec<serde_json::Value> =
        serde_json::from_str(json.trim()).map_err(|err| err.to_string())?;
    let today = now.date_naive();
    Ok(raw
        .into_iter()
        .filter_map(|v| {
            v.get("created_at")
                .and_then(|c| c.as_str())
                .and_then(parse_memory_created_at)
        })
        .filter(|dt| dt.date_naive() == today)
        .count())
}

fn parse_memory_created_at(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // The export omits timezone info (`2026-04-21T10:00:00`); treat as UTC.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        return Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            naive,
            chrono::Utc,
        ));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            naive,
            chrono::Utc,
        ));
    }
    None
}

fn fetch_transformations_feed(limit: u32) -> Result<TransformationFeedResponse, String> {
    fetch_transformations_feed_from("http://127.0.0.1:6767", limit)
}

#[derive(serde::Deserialize)]
struct RawTransformationsFeedResponse {
    log_full_messages: bool,
    transformations: Vec<crate::models::TransformationFeedEvent>,
}

fn fetch_transformations_feed_from(
    base_url: &str,
    limit: u32,
) -> Result<TransformationFeedResponse, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(2000))
        .build()
        .map_err(|err| err.to_string())?;
    let url = format!("{base_url}/transformations/feed?limit={limit}");
    let response = client.get(url).send().map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("proxy returned HTTP {}", response.status()));
    }
    let raw: RawTransformationsFeedResponse = response.json().map_err(|err| err.to_string())?;
    Ok(TransformationFeedResponse {
        log_full_messages: raw.log_full_messages,
        transformations: raw.transformations,
        proxy_reachable: true,
    })
}

struct HeadroomLearnRunResult {
    success: bool,
    summary: String,
    error: Option<String>,
    output_tail: Vec<String>,
}

/// Detect `headroom.learn.analyzer` warnings that mean the LLM never produced
/// recommendations even though the CLI exited 0. Returns a user-facing message
/// joining all such warnings, or None if the run was clean.
fn extract_llm_failure_warnings(stderr: &str) -> Option<String> {
    const MARKER: &str = "LLM analysis failed:";
    let messages: Vec<String> = stderr
        .lines()
        .filter_map(|line| {
            line.split_once(MARKER)
                .map(|(_, rest)| format!("{} {}", MARKER, rest.trim()))
        })
        .collect();
    if messages.is_empty() {
        None
    } else {
        Some(messages.join("\n"))
    }
}

/// Strip the machine-specific part out of an LLM-failure signature so the same
/// failure groups as one Sentry issue across the fleet.
///
/// Upstream renders the reason as ``LLM analysis failed: `<cli> -p --output-format
/// stream-json --verbose` failed (exit N):``, where `<cli>` is the *resolved*
/// path to the agent binary on that machine. That path is different on every
/// host, and because the signature is the fingerprint, one failure class opened
/// one issue per user: RUST-A2 (`~\AppData\Roaming\npm\claude.CMD`) and
/// RUST-9Z (a bare `claude`) are the same thing seen twice. Rewriting the
/// program to its bare stem groups them while leaving every discriminator that
/// actually means something -- the flags and the exit code -- in place. (The
/// exit code stays: 1 and 3221226505/0xC0000409 really are different failures,
/// an error exit versus the CLI crashing.)
///
/// Path-shaped input only: anything without a separator is already bare and is
/// returned untouched, so a reason that is not command-shaped passes through.
fn normalize_learn_failure_signature(signature: &str) -> String {
    let Some(open) = signature.find('`') else {
        return signature.to_string();
    };
    let rest = &signature[open + 1..];
    let Some(close) = rest.find('`') else {
        return signature.to_string();
    };
    let command = &rest[..close];
    let (program, args) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    // Both separators unconditionally: these strings arrive from Windows hosts
    // and are read on any platform, so `Path` (which does not treat `\` as a
    // separator off Windows) is the wrong tool.
    let stem = program.rsplit(['/', '\\']).next().unwrap_or(program);
    // Drop the executable suffix so `claude.CMD`, `claude.exe` and `claude` are
    // one program. Only the last dot, and only when it looks like an extension.
    let stem = match stem.rsplit_once('.') {
        Some((base, ext)) if !base.is_empty() && ext.chars().all(char::is_alphanumeric) => base,
        _ => stem,
    };
    let normalized_command = if args.is_empty() {
        stem.to_string()
    } else {
        format!("{stem} {args}")
    };
    format!(
        "{}`{normalized_command}`{}",
        &signature[..open],
        &rest[close + 1..]
    )
}

/// True when a `headroom learn` failure was the coding agent's CLI refusing to
/// run because nobody is signed in to it on this machine.
///
/// This is a user-environment condition, not an app bug: the analyzer shells
/// out to `claude`/`codex`/`opencode`, and if that CLI has no session it exits
/// non-zero with its own login prompt. RUST-B6 is the whole class -- four
/// events whose only content was `Not logged in - Please run /login`, which no
/// change on our side can resolve. It stays out of Sentry and becomes an
/// actionable message in the UI instead (see `learn_agent_auth_hint`).
fn learn_failure_is_agent_auth(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Each needle is a full CLI phrase, not a bare word: "login" alone would
    // also match a project's own source lines echoed back in the output.
    const NEEDLES: &[&str] = &[
        "not logged in",
        "please run /login",
        "run /login",
        "please log in",
        "not authenticated",
        "authentication_error",
        "invalid api key",
        "oauth token has expired",
        "oauth session expired",
        "failed to authenticate",
        "credentials have expired",
        "session has expired",
        "please run `codex login`",
        "run `codex login`",
        "no credentials found",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// The user-facing remedy for [`learn_failure_is_agent_auth`], naming the CLI
/// the run actually shelled out to.
fn learn_agent_auth_hint(agent: LearnAgent) -> String {
    let (cli, command) = match agent {
        LearnAgent::Claude => ("Claude Code", "claude"),
        LearnAgent::Codex => ("Codex", "codex"),
        LearnAgent::Opencode => ("opencode", "opencode"),
        LearnAgent::Grok => ("Grok", "grok"),
    };
    format!(
        "{cli} is not signed in on this machine, so headroom learn could not run its analysis. \
         Open a terminal, run `{command}`, sign in, then start the scan again."
    )
}

/// The CLI line saying a `headroom learn` failure was the coding agent hitting
/// its plan's session/usage limit, or None when that is not the cause.
///
/// Same user-environment class as [`learn_failure_is_agent_auth`] -- nothing on
/// our side can change the outcome (RUST-BF: `You've hit your session limit
/// \u{b7} resets 9:10am (America/Chicago)`) -- but deliberately a separate
/// classifier, because the auth remedy ("run /login") is wrong advice for a
/// limit. Staying out of Sentry also sidesteps a grouping break: the reset
/// clock in the message lands in the fingerprint, so this class could never
/// group into one issue. Returns the matched line so the UI hint can echo the
/// CLI's own reset time.
fn learn_failure_agent_limit_line(text: &str) -> Option<&str> {
    // Full CLI phrases, like the auth needles: "limit" alone would match a
    // project's own source lines echoed back in the output.
    const NEEDLES: &[&str] = &[
        "hit your session limit",
        "session limit reached",
        "hit your usage limit",
        "usage limit reached",
    ];
    text.lines().map(str::trim).find(|line| {
        let lower = line.to_ascii_lowercase();
        NEEDLES.iter().any(|needle| lower.contains(needle))
    })
}

/// The user-facing remedy for [`learn_failure_agent_limit_line`], echoing the
/// CLI's own line so the reset time survives to the UI.
fn learn_agent_limit_hint(agent: LearnAgent, limit_line: &str) -> String {
    let cli = match agent {
        LearnAgent::Claude => "Claude Code",
        LearnAgent::Codex => "Codex",
        LearnAgent::Opencode => "opencode",
        LearnAgent::Grok => "Grok",
    };
    format!(
        "{cli} hit your plan's usage limit, so headroom learn could not run its analysis \
         (\"{limit_line}\"). Start the scan again after the limit resets."
    )
}

/// True when a `headroom learn` failure was the agent CLI's own backend
/// rejecting the user's configured model override -- the same
/// user-environment class as [`learn_failure_is_agent_auth`] (RUST-BQ:
/// `[claude-code:unrecognized_model] {"model":"mimo-v2.5",...}` from a
/// custom model/router setup). Nothing on our side can change the outcome,
/// and the model name would land in the fingerprint, so this class could
/// never group into one Sentry issue anyway. The default failure message
/// echoes the CLI's own line, which names the rejected model.
fn learn_failure_is_agent_model_rejected(text: &str) -> bool {
    text.to_ascii_lowercase().contains("unrecognized_model")
}

/// The text a learn failure is fingerprinted on.
///
/// Upstream's first stderr line is a marker whose reason is EMPTY -- ``LLM
/// analysis failed: `claude -p ...` failed (exit 1):`` -- because the child
/// CLI writes its diagnosis to its own stream, which upstream appends on the
/// FOLLOWING line. Fingerprinting the marker alone collapsed every distinct
/// cause onto one issue with nothing in it to tell them apart (RUST-74, then
/// RUST-B6). The marker's next non-empty line is the actual reason, so join
/// it in.
///
/// The marker is not always the FIRST line: upstream's own logging can precede
/// it -- RUST-BC's stderr opens with an onnxruntime C-API warning from
/// `_ort.py`, which is unrelated to the failure and titled the issue anyway.
/// Preamble like that is per-machine, so fingerprinting it merges every
/// distinct failure on the hosts that emit it. Scan for the marker instead of
/// only checking line one.
///
/// stderr only at the call site: stdout echoes written memory files back
/// verbatim and must never reach a Sentry title.
fn learn_failure_signature_source(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let Some(first) = lines.first() else {
        return "no output".to_string();
    };
    // Only the `failed (exit N):` marker is followed by a one-line diagnosis
    // worth grouping on. Other markers also end at a colon but introduce a
    // DUMP -- `returned unparseable output. First 2000 chars:` (RUST-B7) is
    // followed by the model's raw output, which is derived from the user's
    // sessions and must not become a Sentry title. With no marker at all the
    // first line is still the best guess.
    let Some(marker) = lines
        .iter()
        .position(|line| learn_marker_expects_reason_line(line))
    else {
        return first.to_string();
    };
    match lines.get(marker + 1) {
        Some(reason) => format!("{} {reason}", lines[marker]),
        None => lines[marker].to_string(),
    }
}

/// True for upstream's ``... `<cli> ...` failed (exit N):`` marker, whose
/// reason -- the child CLI's own stderr -- lands on the next line.
fn learn_marker_expects_reason_line(line: &str) -> bool {
    line.ends_with("):") && line.contains(" failed (exit ")
}

/// Turn a `headroom learn` stdout line into the step shown under the scan timer,
/// or None to leave the current step alone.
///
/// Deliberately a whitelist, not a pass-through: after `[WROTE]` the CLI prints
/// the whole written file back, so "show the last line" would put memory-file
/// contents in the UI.
fn learn_step_label(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(model) = line.strip_prefix("Analyzing with ") {
        // The long phase: one LLM call, minutes of silence. Name the backend so
        // it's clear whose session is running.
        let model = model.trim_end_matches('.');
        let backend = match model {
            "claude-cli" => "Claude Code",
            "codex-cli" => "ChatGPT",
            "gemini-cli" => "Gemini",
            other => other,
        };
        return Some(format!("Analyzing with {backend}"));
    }
    if let Some(count) = line.strip_prefix("Recommendations: ") {
        return Some(format!("Found {count} patterns"));
    }
    if let Some(path) = line
        .strip_prefix("[WROTE] ")
        .or_else(|| line.strip_prefix("[WOULD WRITE] "))
    {
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        return Some(format!("Updating {name}"));
    }
    if line.starts_with("No conversation data")
        || line.starts_with("No failures or patterns")
        || line.starts_with("No actionable patterns")
    {
        return Some(line.to_string());
    }
    None
}

/// Run the scan, forwarding its stage lines to the run status as they arrive.
///
/// Equivalent to `command.output()` for the caller: the full streams are
/// reassembled so the existing success/failure handling is unchanged. The only
/// difference is that stdout is observed line by line on the way through, since
/// a scan runs a headless agent session against the user's own hooks and a bare
/// timer gives them no way to see that.
fn stream_headroom_learn_output(
    state: &AppState,
    command: &mut Command,
) -> std::io::Result<std::process::Output> {
    use std::io::{BufRead, BufReader, Read};

    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    // stderr drains on its own thread: a child that fills one pipe while we
    // read the other would deadlock.
    let mut stderr_pipe = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });

    let mut stdout = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        // split() over lines(): keeps the bytes verbatim, so one non-UTF-8 byte
        // can't abort the capture the caller depends on.
        for chunk in BufReader::new(pipe).split(b'\n') {
            let Ok(chunk) = chunk else { break };
            if let Some(step) = learn_step_label(&String::from_utf8_lossy(&chunk)) {
                state.set_headroom_learn_step(step);
            }
            stdout.extend_from_slice(&chunk);
            stdout.push(b'\n');
        }
    }

    let status = child.wait()?;
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn execute_headroom_learn_run(
    state: &AppState,
    agent: LearnAgent,
    project_path: Option<&str>,
) -> HeadroomLearnRunResult {
    // `run_id` keys the run-status + log file; `project_name` is the user-facing
    // label. Codex isn't project-organized, so it uses a stable "codex" id.
    let (run_id, project_name): (&str, String) = match agent {
        LearnAgent::Claude => {
            let path = project_path.unwrap_or("");
            let name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string();
            (path, name)
        }
        LearnAgent::Codex => ("codex", "ChatGPT sessions".to_string()),
        LearnAgent::Opencode => ("opencode", "OpenCode sessions".to_string()),
        LearnAgent::Grok => ("grok", "Grok sessions".to_string()),
    };
    let entrypoint = state.tool_manager.headroom_entrypoint();
    if !entrypoint.exists() {
        return HeadroomLearnRunResult {
            success: false,
            summary: format!("headroom learn failed for {project_name}."),
            error: Some(format!(
                "Headroom entrypoint not found at {}",
                entrypoint.display()
            )),
            output_tail: Vec::new(),
        };
    }
    // Pre-flight: the Claude scan passes --project to the CLI, where Click's
    // Path(readable=True) rejects a missing/unreadable dir with exit 2. That's a
    // user-environment condition (project moved/deleted, or macOS TCC blocking
    // ~/Documents et al.), not an app bug, so short-circuit here instead of
    // spawning and reporting the failure to Sentry. read_dir mirrors Click's
    // readability check and surfaces both the missing-path and TCC-denied cases.
    if let LearnAgent::Claude = agent {
        let path = project_path.unwrap_or_default();
        if path.is_empty() || std::fs::read_dir(path).is_err() {
            return HeadroomLearnRunResult {
                success: false,
                summary: format!("headroom learn failed for {project_name}."),
                error: Some(format!(
                    "Project path is not readable: {path}\n\
                     It may have been moved or deleted, or Headroom needs \
                     Files & Folders / Full Disk Access to read it."
                )),
                output_tail: Vec::new(),
            };
        }
    }

    // Heal a start-only learn block before the wheel's writer runs: it only
    // replaces start..end and silently writes nothing otherwise. Personal
    // files only; the team-shared CLAUDE.md is git-tracked and never touched.
    if let LearnAgent::Claude = agent {
        let path = project_path.unwrap_or_default();
        for file in [
            Path::new(path).join("CLAUDE.local.md"),
            crate::tool_manager::claude_project_memory_file(path),
        ] {
            crate::tool_manager::repair_headroom_learn_block_file(&file);
        }
    }

    let cli_path = match agent {
        LearnAgent::Claude => claude_cli::detect_claude_cli(),
        LearnAgent::Codex => client_adapters::detect_codex_cli(),
        // Analysis CLI, not the agent's own binary: prefer Claude, fall back
        // to Codex (prereq check guarantees one exists).
        LearnAgent::Opencode | LearnAgent::Grok => {
            claude_cli::detect_claude_cli().or_else(client_adapters::detect_codex_cli)
        }
    };

    let mut command = crate::proc::command(&entrypoint);
    command.arg("learn").arg("--apply");
    // The analysis CLI is killed after this long with no stream event. Upstream
    // defaults to 60s, which kills a healthy run: the digest is large, it is
    // routed through our own proxy, and the model can think past a minute
    // before the first token (RUST-6W: 14 kills across 8 unrelated machines,
    // every release from 0.8.2 to 0.8.6). Same failure the /stats probe had at
    // 500ms -- a cap tight enough to turn "slow" into "broken".
    //
    // The idle cap is what catches a genuine hang: a wedged CLI stops
    // streaming, and 180s of silence kills it whatever the wall clock says.
    command.env("HEADROOM_LEARN_CLI_IDLE_TIMEOUT_SECS", "180");
    // The hard cap bounds a run that IS still streaming. Upstream's 300s
    // default was sized for a small digest; a user with 1600 sessions and
    // 69k calls (RUST-B8) needs the model to read and write for longer than
    // that while producing events the whole time, and the cap killed a
    // healthy run at five minutes with a hint to raise this very variable.
    // Fifteen minutes: this is a background job with its own step timer in
    // the UI, and the idle cap above still ends a stall within three.
    command.env("HEADROOM_LEARN_CLI_TIMEOUT_SECS", "900");
    match agent {
        LearnAgent::Claude => {
            // Per-project Claude scan; writes CLAUDE.md / MEMORY.md for the
            // passed --project.
            command
                .arg("--project")
                .arg(project_path.unwrap_or_default())
                .arg("--agent")
                .arg("claude")
                .env("HEADROOM_LEARN_CLI", "claude");
        }
        LearnAgent::Codex => {
            // Codex scans all of ~/.codex/sessions (no --project) and writes
            // ~/.codex/AGENTS.md + instructions.md. Force --model codex-cli so
            // analysis runs through `codex exec` on the user's ChatGPT
            // subscription rather than auto-detecting an API key or the claude CLI.
            command
                .arg("--agent")
                .arg("codex")
                .arg("--model")
                .arg("codex-cli")
                .env("HEADROOM_LEARN_CLI", "codex");
        }
        LearnAgent::Opencode | LearnAgent::Grok => {
            command.arg("--agent").arg(match agent {
                LearnAgent::Opencode => "opencode",
                _ => "grok",
            });
            // Session parsing is plugin-side; the analysis LLM runs through
            // whichever supported CLI is installed (mirrors the prereq check).
            if claude_cli::detect_claude_cli().is_some() {
                command.env("HEADROOM_LEARN_CLI", "claude");
            } else {
                command
                    .arg("--model")
                    .arg("codex-cli")
                    .env("HEADROOM_LEARN_CLI", "codex");
            }
        }
    }
    command
        // Run from an app-owned directory. For Claude the project is passed
        // explicitly via --project, so CWD is irrelevant; running elsewhere also
        // avoids getcwd() EPERM in spawned CLI shells when the project lives in a
        // TCC-protected location. The entrypoint's parent (inside Application
        // Support) is always accessible.
        .current_dir(
            entrypoint
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from("/")),
        )
        .env("PYTHONNOUSERSITE", "1")
        // The live step line depends on stage output arriving as it happens.
        // click.echo already flushes per call, but a plain print() anywhere in
        // the CLI would sit in an 8KB pipe buffer until exit.
        .env("PYTHONUNBUFFERED", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        // Force the selected CLI backend: the analyzer picks LiteLLM over
        // HEADROOM_LEARN_CLI / --model codex-cli when any of these keys is set
        // in the parent env.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        // Don't pin ANTHROPIC_MODEL here: it's a LiteLLM identifier that the
        // analyzer never reads on the CLI path. Worse, it's inherited by the
        // spawned `claude -p` subprocess, where Claude Code's CLI does honor it —
        // and "claude-sonnet-4-6" is not a valid Claude Code model alias,
        // routing the call to a slow/hung path past 120s.
        .env_remove("ANTHROPIC_MODEL");
    if let Some(dir) = cli_path.as_ref().and_then(|p| p.parent()) {
        command.env("PATH", crate::proc::path_with_dir_prepended(dir));
    }
    // Seed a step before the first line arrives: session scanning is silent, and
    // an empty line that pops in later would shift the row.
    state.set_headroom_learn_step("Reading sessions".into());
    let output = stream_headroom_learn_output(state, &mut command);

    let (summary, success, error, output_tail, stdout, stderr, status_copy) = match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let merged = if stderr.trim().is_empty() {
                stdout.clone()
            } else if stdout.trim().is_empty() {
                stderr.clone()
            } else {
                format!("{stdout}\n{stderr}")
            };
            let output_tail = crate::state::tail_lines(&merged, 32);
            if output.status.success() {
                if let Some(warnings) = extract_llm_failure_warnings(&stderr) {
                    // The CLI exits 0 here: the analyzer logged a WARNING and
                    // returned zero recommendations. Without this capture the
                    // whole class is invisible to us: users report a bare
                    // "failed (exit 1):" and we have no idea whether it was a
                    // usage limit, a dead local route, or our own 400. Warning
                    // (not Error) because some causes are user-environment;
                    // fingerprinted on the reason so the split is visible.
                    let raw_signature: String = warnings
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("no reason")
                        .chars()
                        .take(160)
                        .collect();
                    // Fingerprint and title on the normalized form: the raw one
                    // carries the machine's resolved CLI path and split one
                    // failure class per user (RUST-A2/RUST-9Z). The unmodified
                    // text stays in `reason` and `stderr_tail` below.
                    let signature = normalize_learn_failure_signature(&raw_signature);
                    // Same user-environment carve-out as the non-zero-exit
                    // branch below (RUST-B6, RUST-BF): when the agent CLI has no
                    // signed-in session or its plan hit a usage limit, nothing
                    // on our side can change the outcome.
                    let agent_not_signed_in = learn_failure_is_agent_auth(&stderr);
                    let agent_limit_line =
                        learn_failure_agent_limit_line(&stderr).map(str::to_string);
                    if !agent_not_signed_in && agent_limit_line.is_none() {
                        sentry::with_scope(
                            |scope| {
                                scope.set_tag("flow", "headroom_learn");
                                scope.set_tag("learn_outcome", "llm_analysis_failed");
                                scope.set_tag("learn_agent", agent.as_tag());
                                scope.set_extra("reason", warnings.clone().into());
                                scope.set_extra("raw_signature", raw_signature.clone().into());
                                scope.set_extra("project_name", project_name.to_string().into());
                                // The marker line ends at its colon: upstream
                                // appends the child's stderr, and `claude -p
                                // --output-format stream-json` writes its diagnosis
                                // to stdout, so `reason` arrives empty and every
                                // distinct cause -- usage limit, dead local route,
                                // our own 400 -- collapses onto one fingerprint
                                // (RUST-74: 14 events over six users' machines with
                                // nothing in them to tell apart). The analyzer's
                                // surrounding log lines are the only context left
                                // on this side of the process boundary.
                                //
                                // stderr ONLY. stdout echoes written memory files
                                // back verbatim (see `learn_step_label`), and that
                                // is the user's project content -- it must never
                                // reach Sentry.
                                // Pre-redacted, and under a `_redacted` key so
                                // it can be allowlisted in the project's
                                // Sentry `safeFields` without also un-scrubbing
                                // the raw `stderr_tail` that already-shipped
                                // builds keep sending. Same reasoning as
                                // `proxy_log_tail`: one `sk-ant-…` anywhere in
                                // the value and the scrubber replaces the whole
                                // field with `[Filtered]`, which is how RUST-BC
                                // arrived undiagnosable.
                                scope.set_extra(
                                    "stderr_tail_redacted",
                                    crate::tool_manager::redact_sensitive(
                                        &crate::state::tail_lines(&stderr, 32).join("\n"),
                                    )
                                    .into(),
                                );
                                scope.set_fingerprint(Some(
                                    ["headroom_learn_llm_failure", signature.as_str()].as_slice(),
                                ));
                            },
                            || {
                                sentry::capture_message(
                                    &format!(
                                        "headroom learn produced no recommendations: {signature}"
                                    ),
                                    sentry::Level::Warning,
                                );
                            },
                        );
                    }
                    let (summary, detail) = if agent_not_signed_in {
                        (
                            format!("headroom learn needs a signed-in agent for {project_name}."),
                            learn_agent_auth_hint(agent),
                        )
                    } else if let Some(line) = &agent_limit_line {
                        (
                            format!(
                                "headroom learn hit the agent's usage limit for {project_name}."
                            ),
                            learn_agent_limit_hint(agent, line),
                        )
                    } else {
                        (
                            format!(
                                "headroom learn could not produce recommendations for {project_name}."
                            ),
                            warnings,
                        )
                    };
                    (
                        summary,
                        false,
                        Some(detail),
                        output_tail,
                        stdout,
                        stderr,
                        output.status.to_string(),
                    )
                } else {
                    (
                        format!("headroom learn completed for {project_name}."),
                        true,
                        None,
                        output_tail,
                        stdout,
                        stderr,
                        output.status.to_string(),
                    )
                }
            } else {
                let fail_tail = if output_tail.is_empty() {
                    "No output captured.".to_string()
                } else {
                    output_tail.join("\n")
                };
                let exit_code_str = output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                let signal_num: Option<i32> = {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        output.status.signal()
                    }
                    #[cfg(not(unix))]
                    {
                        None
                    }
                };
                // First non-empty line of stderr (or stdout if stderr empty),
                // truncated, used both in the message and the fingerprint so
                // events group by failure mode instead of the capture-site stack.
                let signature_source = if !stderr.trim().is_empty() {
                    stderr.as_str()
                } else {
                    stdout.as_str()
                };
                // Same normalization as the exit-0 branch above, and for the
                // same reason: when upstream's first stderr line is the
                // command-shaped reason, the resolved CLI path in it would
                // split one failure class per machine. A no-op on any line that
                // is not command-shaped.
                // The marker line's reason sits on the NEXT line, so join it in
                // when the first line ends at its dangling colon -- but only
                // for stderr. `signature_source` falls back to stdout when
                // stderr is empty, and stdout echoes written memory files back
                // verbatim (see `learn_step_label`): that is the user's project
                // content and must never reach a Sentry title.
                let signature_head = if stderr.trim().is_empty() {
                    signature_source
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("no output")
                        .to_string()
                } else {
                    learn_failure_signature_source(signature_source)
                };
                let signature: String = normalize_learn_failure_signature(
                    signature_head
                        .chars()
                        .take(160)
                        .collect::<String>()
                        .as_str(),
                );
                // Redacted before Sentry sees them: the agent CLI's stderr on an
                // auth failure is exactly where a key can appear, and one
                // `sk-ant-…` costs the WHOLE field to the scrubber (RUST-BC's
                // stderr arrived `[Filtered]`, so the failure could not be
                // diagnosed at all). `stdout_head` stays raw and stays scrubbed:
                // it echoes the user's memory files, which we do not want
                // readable in Sentry even when it survives.
                let stderr_head = crate::tool_manager::redact_sensitive(
                    &stderr.chars().take(2000).collect::<String>(),
                );
                let stdout_head: String = stdout.chars().take(2000).collect();
                let cli_path_str = cli_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not_found".into());
                let summary_msg =
                    format!("headroom learn failed (exit={exit_code_str}) {signature}");
                let fingerprint: [&str; 3] =
                    ["headroom_learn", exit_code_str.as_str(), signature.as_str()];
                // Defense in depth against a TOCTOU race: the path can become
                // unreadable between the pre-flight read_dir check and the CLI
                // run. Click reports that as exit 2 with "is not readable" — a
                // user-environment condition, not an app bug, so don't report it.
                //
                // The agent CLI having no signed-in session is the same class
                // (RUST-B6), as is its plan hitting a usage limit (RUST-BF).
                // Matched against the whole stderr rather than the signature:
                // upstream's marker line ends before the child's diagnosis,
                // which can land several lines further down.
                let agent_not_signed_in = learn_failure_is_agent_auth(&stderr);
                let agent_limit_line = learn_failure_agent_limit_line(&stderr).map(str::to_string);
                let agent_model_rejected = learn_failure_is_agent_model_rejected(&stderr);
                let user_env_condition = signature.contains("is not readable")
                    || agent_not_signed_in
                    || agent_limit_line.is_some()
                    || agent_model_rejected;
                if !user_env_condition {
                    sentry::with_scope(
                        |scope| {
                            scope.set_tag("flow", "headroom_learn");
                            scope.set_tag("learn_agent", agent.as_tag());
                            scope.set_tag("exit_code", &exit_code_str);
                            scope.set_extra("exit_status", output.status.to_string().into());
                            scope.set_extra(
                                "signal",
                                signal_num
                                    .map(|s| s.to_string().into())
                                    .unwrap_or(serde_json::Value::Null),
                            );
                            scope.set_extra(
                                "output_tail_redacted",
                                crate::tool_manager::redact_sensitive(&fail_tail).into(),
                            );
                            scope.set_extra("stderr_head_redacted", stderr_head.into());
                            scope.set_extra("stdout_head", stdout_head.into());
                            scope.set_extra("cli_path", cli_path_str.into());
                            scope.set_extra("project_name", project_name.to_string().into());
                            scope.set_fingerprint(Some(fingerprint.as_slice()));
                        },
                        || {
                            sentry::capture_message(&summary_msg, sentry::Level::Error);
                        },
                    );
                }
                // A missing agent session has a remedy the user can act on;
                // the raw exit status and output tail do not name it.
                let user_error = if agent_not_signed_in {
                    learn_agent_auth_hint(agent)
                } else if let Some(line) = &agent_limit_line {
                    learn_agent_limit_hint(agent, line)
                } else {
                    format!(
                        "headroom learn exited with {}.\n{}",
                        output.status, fail_tail
                    )
                };
                let user_summary = if agent_not_signed_in {
                    format!("headroom learn needs a signed-in agent for {project_name}.")
                } else if agent_limit_line.is_some() {
                    format!("headroom learn hit the agent's usage limit for {project_name}.")
                } else {
                    format!("headroom learn failed for {project_name}.")
                };
                (
                    user_summary,
                    false,
                    Some(user_error),
                    output_tail,
                    stdout,
                    stderr,
                    output.status.to_string(),
                )
            }
        }
        Err(err) => {
            sentry::capture_message(
                &format!("headroom learn spawn failed: {err}"),
                sentry::Level::Error,
            );
            (
                format!("headroom learn failed for {project_name}."),
                false,
                Some(format!("Could not start headroom learn: {err}")),
                Vec::new(),
                String::new(),
                String::new(),
                "spawn_error".to_string(),
            )
        }
    };

    let log_path = state.tool_manager.headroom_learn_log_path(run_id);
    let log_content = format!(
        "[{}] headroom learn --agent {} (target={})\nstatus: {}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}\n",
        Utc::now().to_rfc3339(),
        match agent {
            LearnAgent::Claude => "claude",
            LearnAgent::Codex => "codex",
            LearnAgent::Opencode => "opencode",
            LearnAgent::Grok => "grok",
        },
        run_id,
        status_copy,
        stdout,
        stderr
    );
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(log_path, log_content);

    HeadroomLearnRunResult {
        success,
        summary,
        error,
        output_tail,
    }
}

/// The tray's pause/resume item, kept here so the tray updater loop can flip its
/// label. `TrayIcon` has no menu getter.
static TRAY_PAUSE_ITEM: std::sync::OnceLock<tauri::menu::MenuItem<tauri::Wry>> =
    std::sync::OnceLock::new();

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = tauri::menu::MenuItem::with_id(app, "show", "Show Headroom", true, None::<&str>)?;
    // Text flips to "Resume Headroom" while paused, from the tray updater loop.
    let pause = tauri::menu::MenuItem::with_id(app, "pause", "Pause Headroom", true, None::<&str>)?;
    let quit = tauri::menu::MenuItem::with_id(app, "quit", "Quit Headroom", true, None::<&str>)?;
    let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
    let menu = tauri::menu::Menu::with_items(app, &[&show, &pause, &separator, &quit])?;
    let _ = TRAY_PAUSE_ITEM.set(pause.clone());
    #[cfg(target_os = "macos")]
    let popup_menu = menu.clone();
    let mut tray_builder = tauri::tray::TrayIconBuilder::with_id("headroom-tray")
        .menu(&menu)
        .icon_as_template(false)
        .tooltip("Headroom")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                let _ = toggle_main_window(tray.app_handle(), Some(rect));
            }

            // macOS only. With a menu attached, Windows and Linux open it
            // themselves on right-click; popping a second one here raced the
            // built-in and left the tray with no usable menu at all, so there
            // was no way to quit from the tray. macOS does not auto-open on
            // right-click (only left, which `show_menu_on_left_click(false)`
            // turns off), so it still needs the manual popup.
            #[cfg(target_os = "macos")]
            if let TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                let window = app
                    .get_webview_window("main")
                    .or_else(|| app.get_webview_window("launcher"));

                if let Some(window) = window {
                    let _ = window.popup_menu(&popup_menu);
                }
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if onboarding_complete(app) {
                    let _ = hide_launcher_window(app);
                    let _ = show_main_window(app, None);
                    let app_bg = app.clone();
                    std::thread::spawn(move || ensure_runtime_ready_for_tray(&app_bg));
                } else {
                    let _ = show_launcher_window(app);
                }
            }
            "pause" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let paused = {
                        let state: tauri::State<'_, AppState> = app.state();
                        state.runtime_is_paused()
                    };
                    let result = if paused {
                        start_headroom(app).await
                    } else {
                        pause_headroom(app).await
                    };
                    if let Err(err) = result {
                        log::warn!("tray pause toggle failed: {err}");
                    }
                });
            }
            "quit" => {
                exit_headroom(app, QuitSource::TrayMenu);
            }
            _ => {}
        });

    if let Some(icon) = app.default_window_icon() {
        tray_builder = tray_builder.icon(icon.clone());
    }

    tray_builder.build(app)?;

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayRuntimeVisual {
    Off,
    Booting,
    Running,
    Paused,
    Unhealthy,
    Disconnected,
}

struct TrayRuntimeIcons {
    off: tauri::image::Image<'static>,
    paused: tauri::image::Image<'static>,
    running_rgba: Vec<u8>,
    running_dims: (u32, u32),
    booting_frames: Vec<tauri::image::Image<'static>>,
}

fn debounced_tray_runtime_visual(
    raw_visual: TrayRuntimeVisual,
    last_non_booting: Option<TrayRuntimeVisual>,
    unhealthy_streak: &mut u8,
) -> TrayRuntimeVisual {
    const UNHEALTHY_DEBOUNCE_TICKS: u8 = 8;

    if raw_visual == TrayRuntimeVisual::Unhealthy {
        *unhealthy_streak = unhealthy_streak.saturating_add(1);
        if *unhealthy_streak < UNHEALTHY_DEBOUNCE_TICKS {
            if matches!(
                last_non_booting,
                Some(TrayRuntimeVisual::Running) | Some(TrayRuntimeVisual::Disconnected)
            ) {
                return last_non_booting.expect("checked Some above");
            }
        }
        return TrayRuntimeVisual::Unhealthy;
    }

    *unhealthy_streak = 0;
    raw_visual
}

fn spawn_tray_runtime_icon_updater(app: AppHandle) {
    let icons = match build_tray_runtime_icons() {
        Ok(icons) => icons,
        Err(err) => {
            sentry::capture_message(
                &format!("failed to build runtime tray icons: {err}"),
                sentry::Level::Warning,
            );
            return;
        }
    };

    std::thread::spawn(move || {
        let mut frame_index = 0usize;
        let mut last_non_booting: Option<TrayRuntimeVisual> = None;
        let mut last_displayed_dollars: Option<u32> = None;
        let mut last_tooltip: Option<String> = None;
        let mut last_pause_label: Option<&str> = None;
        let mut unhealthy_streak: u8 = 0;
        let mut last_connector_check = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now);
        let mut cached_connector_enabled: bool = client_adapters::is_claude_code_enabled()
            || client_adapters::any_gate_exempt_client_enabled();

        loop {
            // Quitting stops the backend on purpose, which this loop would
            // otherwise read as the connector dropping out and announce with a
            // "Headroom is disconnected" notification on the way out the door.
            if SHUTTING_DOWN.load(Ordering::Acquire) {
                return;
            }
            // Re-check connectors at most every ~2s, regardless of whether the
            // tick rate is booting-fast (260ms) or idle-slow (1500ms). Time-based
            // instead of tick-count based so the cadence stays correct across the
            // adaptive sleep below. "Connected" means any supported connector
            // (Claude Code or Codex) is routing through Headroom.
            if last_connector_check.elapsed() >= std::time::Duration::from_secs(2) {
                cached_connector_enabled = client_adapters::is_claude_code_enabled()
                    || client_adapters::any_gate_exempt_client_enabled();
                last_connector_check = std::time::Instant::now();
            }

            let raw_visual = {
                let state: tauri::State<'_, AppState> = app.state();
                let runtime = state.runtime_status();
                if runtime.running {
                    if cached_connector_enabled {
                        TrayRuntimeVisual::Running
                    } else {
                        TrayRuntimeVisual::Disconnected
                    }
                } else if runtime.starting {
                    TrayRuntimeVisual::Booting
                } else if runtime.paused {
                    TrayRuntimeVisual::Paused
                } else if runtime.installed && !runtime.proxy_reachable {
                    // The fast reachability probe (1.5s via the 6767 intercept)
                    // missed, but it flaps on transient upstream-connectivity
                    // blips and brief backend busyness (compression /
                    // embedding) while the process is perfectly alive. Mirror
                    // the watchdog's tolerance instead of immediately flashing
                    // "proxy unreachable, attempting restart": re-probe the
                    // backend /readyz directly, and treat an `ok` or
                    // upstream-only-503 outcome as healthy (the process is fine;
                    // only the cached upstream probe is down). Only a genuinely
                    // non-answering backend shows Unhealthy. This probe runs
                    // only on the rare !proxy_reachable tick, so its cost is off
                    // the happy path.
                    let outcome = probe_backend_readyz_outcome_with_timeout(
                        std::time::Duration::from_secs(5),
                    );
                    if outcome == "ok" || readyz_failure_is_upstream_only(&outcome) {
                        if cached_connector_enabled {
                            TrayRuntimeVisual::Running
                        } else {
                            TrayRuntimeVisual::Disconnected
                        }
                    } else {
                        TrayRuntimeVisual::Unhealthy
                    }
                } else {
                    TrayRuntimeVisual::Off
                }
            };
            let visual =
                debounced_tray_runtime_visual(raw_visual, last_non_booting, &mut unhealthy_streak);

            if let Some(tray) = app.tray_by_id("headroom-tray") {
                let tooltip = match visual {
                    TrayRuntimeVisual::Booting => "Headroom — starting",
                    TrayRuntimeVisual::Running => "Headroom — active",
                    TrayRuntimeVisual::Paused => {
                        "Headroom — paused (Claude Code or ChatGPT running normally)"
                    }
                    TrayRuntimeVisual::Unhealthy => {
                        "Headroom — proxy unreachable, attempting restart"
                    }
                    TrayRuntimeVisual::Disconnected => {
                        "Headroom — Claude Code or ChatGPT not connected"
                    }
                    TrayRuntimeVisual::Off => "Headroom — off",
                };

                let pause_label = if visual == TrayRuntimeVisual::Paused {
                    "Resume Headroom"
                } else {
                    "Pause Headroom"
                };
                if last_pause_label != Some(pause_label) {
                    if let Some(item) = TRAY_PAUSE_ITEM.get() {
                        let _ = item.set_text(pause_label);
                        last_pause_label = Some(pause_label);
                    }
                }

                let mut icon_changed = false;
                match visual {
                    TrayRuntimeVisual::Booting => {
                        let icon =
                            icons.booting_frames[frame_index % icons.booting_frames.len()].clone();
                        let _ = tray.set_icon(Some(icon));
                        icon_changed = true;
                        frame_index = (frame_index + 1) % icons.booting_frames.len();
                        last_non_booting = Some(TrayRuntimeVisual::Booting);
                    }
                    TrayRuntimeVisual::Running => {
                        let dollars = {
                            let savings_state: tauri::State<'_, TraySessionSavings> = app.state();
                            let v = *savings_state.0.lock();
                            let d = v.floor() as u32;
                            #[cfg(debug_assertions)]
                            let d = d.max(1);
                            d
                        };
                        let changed_visual = last_non_booting != Some(TrayRuntimeVisual::Running);
                        let changed_dollars = last_displayed_dollars != Some(dollars);
                        if changed_visual || changed_dollars {
                            let (bw, bh) = icons.running_dims;
                            let (new_rgba, new_w, new_h) =
                                build_running_with_savings(&icons.running_rgba, bw, bh, dollars);
                            let _ = tray.set_icon(Some(tauri::image::Image::new_owned(
                                new_rgba, new_w, new_h,
                            )));
                            icon_changed = true;
                            last_non_booting = Some(TrayRuntimeVisual::Running);
                            last_displayed_dollars = Some(dollars);
                        }
                    }
                    TrayRuntimeVisual::Off => {
                        if last_non_booting != Some(TrayRuntimeVisual::Off) {
                            let _ = tray.set_icon(Some(icons.off.clone()));
                            icon_changed = true;
                            last_non_booting = Some(TrayRuntimeVisual::Off);
                        }
                    }
                    TrayRuntimeVisual::Paused => {
                        if last_non_booting != Some(TrayRuntimeVisual::Paused) {
                            let _ = tray.set_icon(Some(icons.paused.clone()));
                            icon_changed = true;
                            last_non_booting = Some(TrayRuntimeVisual::Paused);
                            last_displayed_dollars = None;
                        }
                    }
                    TrayRuntimeVisual::Unhealthy => {
                        if last_non_booting != Some(TrayRuntimeVisual::Unhealthy) {
                            let _ = tray.set_icon(Some(icons.off.clone()));
                            icon_changed = true;
                            last_non_booting = Some(TrayRuntimeVisual::Unhealthy);
                            last_displayed_dollars = None;
                        }
                    }
                    TrayRuntimeVisual::Disconnected => {
                        if last_non_booting != Some(TrayRuntimeVisual::Disconnected) {
                            let _ = tray.set_icon(Some(icons.off.clone()));
                            icon_changed = true;
                            // Only notify when transitioning from a healthy running
                            // state — not on first boot or from other non-running states.
                            if last_non_booting == Some(TrayRuntimeVisual::Running) {
                                let _ = show_notification_impl(
                                    &app,
                                    "Headroom",
                                    "Claude Code or ChatGPT is disconnected — open Headroom to re-enable.",
                                    Some("connectors".into()),
                                );
                            }
                            last_non_booting = Some(TrayRuntimeVisual::Disconnected);
                            last_displayed_dollars = None;
                        }
                    }
                }

                // set_icon clobbers the tooltip on macOS, so re-apply whenever
                // we just swapped the icon — not only on tooltip text change.
                let tooltip_changed = last_tooltip.as_deref() != Some(tooltip);
                if icon_changed || tooltip_changed {
                    match tray.set_tooltip(Some(tooltip)) {
                        Ok(()) => last_tooltip = Some(tooltip.to_string()),
                        // Windows returns E_FAIL (0x80004005) while the
                        // notification area is busy -- explorer restarting, or
                        // a shell extension holding it. Caching the tooltip
                        // anyway froze the wrong hover text until the text
                        // happened to change again; leaving `last_tooltip`
                        // stale keeps `tooltip_changed` true so the next tick
                        // retries and the failure heals itself (RUST-7P).
                        Err(err) => log::warn!("tray: set_tooltip failed: {err}"),
                    }
                }
            } else {
                break;
            }

            // Only transitional states need quick polling. In steady state the
            // tray icon is unchanged, and `runtime_status()` is one of the few
            // always-on paths that can still hit the local proxy / filesystem.
            let sleep = match visual {
                TrayRuntimeVisual::Booting => std::time::Duration::from_millis(260),
                TrayRuntimeVisual::Unhealthy => std::time::Duration::from_millis(1500),
                _ => std::time::Duration::from_secs(5),
            };
            std::thread::sleep(sleep);
        }
    });
}

/// Should the watchdog expect the Python proxy to be reachable right now?
///
/// All five inputs are required to be in their "ready" state for the proxy
/// to be supposed-up. Pulled out as a pure function so the truth table is
/// trivially testable — every clause is load-bearing and removing one
/// silently turns the watchdog into a thrash loop. Specifically `bypass`
/// being false matters: when the pricing gate has flipped on `proxy_bypass`
/// the Rust intercept is routing direct to api.anthropic.com, so a missing
/// Python is intentional, not a failure.
fn watchdog_should_be_up(
    installed: bool,
    paused: bool,
    starting: bool,
    upgrading: bool,
    bypass: bool,
) -> bool {
    installed && !paused && !starting && !upgrading && !bypass
}

/// Backoff schedule for the self-heal auto-resume loop after the watchdog has
/// given up and auto-paused. Keyed by the number of failed resume attempts so
/// far: 30s, 1m, 2m, then a 5m cap for all later attempts. Retries continue
/// indefinitely at the cap so a transient outage (laptop slept on battery,
/// transient network) self-heals whenever it clears, without hammering restart.
fn auto_resume_backoff(failed_attempts: u32) -> std::time::Duration {
    let secs = match failed_attempts {
        0 => 30,
        1 => 60,
        2 => 120,
        _ => 300,
    };
    std::time::Duration::from_secs(secs)
}

/// Every 5s, check whether the Python proxy is actually reachable while the
/// app thinks the runtime should be up. If it isn't, try to restart via
/// `ensure_headroom_running`. After 3 consecutive failures (~15s down) we
/// give up: pause the runtime, flip `proxy_bypass=true` so the Rust intercept
/// passes traffic straight through to api.anthropic.com, and notify the user.
/// The user's `~/.claude/settings.json` env, hook, and shell blocks stay
/// intact — `start_headroom` clears bypass and brings Python back up without
/// needing to re-install anything on disk.
fn spawn_proxy_watchdog(app: AppHandle) {
    const POLL: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_CONSECUTIVE_FAILURES: u32 = 3;
    // If a tick takes far longer than POLL of wall time, the system was
    // suspended (laptop sleep, App Nap throttle). Don't blame Python for
    // not responding to the first probe after resume — uvicorn's event
    // loop may need a beat to catch up before /readyz answers.
    const RESUME_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(30);

    std::thread::spawn(move || {
        let mut consecutive_failures: u32 = 0;
        // Wall-clock (not `Instant`) timestamp of the previous tick. On macOS
        // `Instant`/`mach_absolute_time` FREEZES while the system is asleep, so
        // a laptop that slept for minutes (common on battery) would measure as
        // only a few seconds of monotonic time and the `just_resumed` guard
        // below would never fire — the watchdog would count the sleep as 3
        // backend failures and auto-pause a perfectly healthy process. The
        // wall clock advances across sleep, so the resume gap is real.
        let mut last_tick_wall = std::time::SystemTime::now();
        // Self-heal scheduling after a give-up auto-pause. `auto_pause_next_retry`
        // is the earliest Instant at which we re-attempt a resume; `auto_pause_failed`
        // counts failed attempts to grow the backoff (see `auto_resume_backoff`).
        let mut auto_pause_next_retry: Option<std::time::Instant> = None;
        let mut auto_pause_failed: u32 = 0;
        // Set after a forced kill+restart of a hung process. Prevents the
        // hung-kill path from looping forever if the new process also hangs:
        // on the second trip through MAX_CONSECUTIVE_FAILURES we fall through
        // to the permanent give-up path instead. Resets when the proxy
        // recovers so a later hang triggers another rescue attempt.
        let mut hung_kill_attempted = false;
        // Last observed wall-clock jump (sleep/suspend): when it was seen and
        // how large it was. Carried into the give-up Sentry event so triage
        // can tell post-wake episodes apart from genuine wedges.
        let mut last_wall_jump: Option<(std::time::Instant, u64)> = None;
        // Fire the one-shot Kompress model prefetch the first time we observe a
        // healthy proxy this launch. `maybe_prefetch_kompress` is itself guarded
        // and no-ops when the model is already cached; this flag just avoids
        // spawning a throwaway thread on every subsequent tick.
        let mut kompress_prefetch_spawned = false;
        // One-shot tiktoken vocab-cache seeding, deliberately NOT gated on a
        // healthy proxy: the machines that need it most are the ones whose
        // backend boot is wedged on a stalled vocab download (RUST-5D) and
        // would never reach the healthy branch.
        let mut tiktoken_prefetch_spawned = false;

        loop {
            std::thread::sleep(POLL);
            if SHUTTING_DOWN.load(Ordering::Acquire) {
                return;
            }
            let now_wall = std::time::SystemTime::now();
            let elapsed = now_wall
                .duration_since(last_tick_wall)
                .unwrap_or(std::time::Duration::ZERO);
            last_tick_wall = now_wall;
            let just_resumed = elapsed > RESUME_THRESHOLD;

            let state: tauri::State<'_, AppState> = app.state();
            let runtime = state.runtime_status();

            // Self-heal: if a previous give-up auto-paused the runtime, keep
            // trying to bring it back on a backoff instead of staying dead
            // until the user intervenes. A deliberate user pause
            // (auto_paused=false) is never retried here. We clear the pause and
            // hard-restart, then let the normal path below own the outcome:
            // it either observes the proxy recover or re-gives-up, which
            // reschedules the next retry with a longer backoff.
            if runtime.auto_paused {
                let due = auto_pause_next_retry
                    .map(|t| std::time::Instant::now() >= t)
                    .unwrap_or(true);
                if due {
                    log::info!(
                        "watchdog: auto-resume attempt (failed_attempts={auto_pause_failed}); killing wedged proxy and restarting"
                    );
                    // Replace the wedged child outright — `resume_runtime` ->
                    // `ensure_headroom_running` no-ops on an alive-but-hung
                    // process (try_wait says running), so a plain resume can't
                    // fix it. stop_headroom SIGKILLs the group and reaps orphans.
                    state.stop_headroom();
                    consecutive_failures = 0;
                    hung_kill_attempted = false;
                    if let Err(err) = state.resume_runtime() {
                        // resume_runtime already cleared the auto_paused flag;
                        // the normal path will re-give-up and reschedule.
                        log::info!("watchdog: auto-resume resume_runtime failed: {err:#}");
                    }
                    auto_pause_next_retry = None;
                }
                continue;
            }

            if !tiktoken_prefetch_spawned && runtime.installed {
                tiktoken_prefetch_spawned = true;
                let app_clone = app.clone();
                std::thread::spawn(move || {
                    let state: tauri::State<'_, AppState> = app_clone.state();
                    if let Err(err) = state.tool_manager.prefetch_tiktoken_encodings() {
                        // warn -> Sentry: fleet signal for vocab-fetch
                        // failures — but once per MACHINE, not per launch. A
                        // firewalled CDN would otherwise re-report on every
                        // start, forever.
                        let marker = state
                            .tool_manager
                            .logs_dir()
                            .join("tiktoken-prefetch.warned");
                        if marker.exists() {
                            log::info!("tiktoken prefetch failed (repeat): {err:#}");
                        } else {
                            let _ = std::fs::write(&marker, b"1");
                            log::warn!("tiktoken prefetch failed: {err:#}");
                        }
                    }
                });
            }

            // Only care when the runtime is supposed to be up: installed,
            // not paused by the user, not mid-boot, not mid-upgrade, and not
            // intentionally bypassed. When `proxy_bypass` is set the pricing
            // gate has stopped Python on purpose; the Rust intercept is
            // routing direct to api.anthropic.com, so trying to restart the
            // backend would just thrash and eventually trip the auto-pause
            // path below.
            let bypass_active = state
                .proxy_bypass
                .load(std::sync::atomic::Ordering::Acquire);
            let should_be_up = watchdog_should_be_up(
                runtime.installed,
                runtime.paused,
                runtime.starting,
                state.runtime_upgrade_in_progress(),
                bypass_active,
            );
            if !should_be_up {
                if consecutive_failures > 0 {
                    log::debug!(
                        "watchdog: skip restart (installed={}, paused={}, starting={}, upgrading={}, bypass={}); resetting failure counter",
                        runtime.installed,
                        runtime.paused,
                        runtime.starting,
                        state.runtime_upgrade_in_progress(),
                        bypass_active
                    );
                }
                consecutive_failures = 0;
                continue;
            }

            if runtime.proxy_reachable {
                consecutive_failures = 0;
                hung_kill_attempted = false;
                // Healthy again — reset the self-heal backoff so a future
                // wedge starts its retries fresh at 30s.
                auto_pause_failed = 0;
                auto_pause_next_retry = None;
                // End of "down episode" — re-arm Sentry capture so a future
                // crash fires a fresh event.
                WATCHDOG_DOWN_CAPTURED.store(false, Ordering::Release);
                if !kompress_prefetch_spawned {
                    kompress_prefetch_spawned = true;
                    let app_clone = app.clone();
                    std::thread::spawn(move || {
                        let state: tauri::State<'_, AppState> = app_clone.state();
                        state.maybe_prefetch_kompress();
                    });
                }
                continue;
            }

            // System resumed from sleep/throttle — give Python one POLL to
            // catch up before counting failures. Without this, the watchdog
            // probes a still-paged-out uvicorn 3× in 15s and auto-pauses a
            // process that would have recovered on its own.
            if just_resumed {
                log::info!(
                    "watchdog: probe skipped (system resumed after {elapsed:?}); resetting failure counter"
                );
                last_wall_jump = Some((std::time::Instant::now(), elapsed.as_secs()));
                consecutive_failures = 0;
                continue;
            }

            // Tolerant confirmation before counting a strike. The standard
            // reachability check (`is_headroom_proxy_reachable`) uses a tight
            // 1.5s timeout via the 6767 intercept, and a busy backend on a
            // contended machine can miss that window while perfectly healthy.
            // (This once compounded with a `nice`-wrapped backend, dropped
            // 2026-08-17; the tolerance stays as defense in depth, since an
            // oversubscribed box starves a default-priority process too.)
            // Re-probe the backend's /readyz directly with a 5s budget — if it
            // answers, the process is alive and merely busy, not down.
            let tolerant_outcome =
                probe_backend_readyz_outcome_with_timeout(std::time::Duration::from_secs(5));
            if tolerant_outcome == "ok" {
                log::info!(
                    "watchdog: backend /readyz answered on tolerant 5s re-probe; not counting failure"
                );
                consecutive_failures = 0;
                continue;
            }
            // A 503 whose only failing check is upstream connectivity means the
            // process itself is alive and healthy — only the cached upstream
            // probe is down (network blip / sleep-wake). /readyz is a readiness
            // signal, not a liveness one; don't count it as the process dying.
            if readyz_failure_is_upstream_only(&tolerant_outcome) {
                log::info!(
                    "watchdog: backend /readyz 503 with only upstream unhealthy (transient connectivity); not counting failure"
                );
                consecutive_failures = 0;
                continue;
            }

            consecutive_failures = consecutive_failures.saturating_add(1);
            log::info!(
                "watchdog: proxy unreachable (failure {consecutive_failures}/{MAX_CONSECUTIVE_FAILURES}, bypass={bypass_active}), attempting restart"
            );

            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                // Busy, not wedged: if the backend delivered response bytes
                // through the 6767 intercept within the last few seconds, its
                // event loop is demonstrably alive — /readyz is just starving
                // behind heavy streaming load (30+ concurrent Claude Code
                // sessions saturate the pre-upstream semaphore and the probe
                // misses its window). Force-killing here truncates every
                // in-flight SSE stream ("Connection closed mid-response"), so
                // hold off as long as bytes keep moving. A truly wedged or
                // dead backend delivers nothing and ages past the window.
                if proxy_intercept::backend_traffic_within(std::time::Duration::from_secs(10)) {
                    log::info!(
                        "watchdog: probes failing but backend streamed bytes within 10s; busy not wedged, resetting counter"
                    );
                    consecutive_failures = 0;
                    continue;
                }
                // Before pausing, probe the backend directly on its loopback
                // port. `is_headroom_proxy_reachable` goes through the Rust
                // intercept on 6767, which forwards to Python on 6768 with a
                // 1.5s timeout — a slow cold-boot (ONNX embedder downloading
                // model.onnx from huggingface during lifespan startup) can
                // make 6767 time out while the backend was about to recover.
                // If the backend now answers /readyz directly, treat the 3
                // intercept failures as a transient blip rather than a dead
                // process: reset the counter and keep probing. We're already
                // 15s into the down episode, so one extra POLL of patience is
                // cheap compared to auto-pausing a process that just came up.
                // 5s budget + one retry on bare http_503: this is the probe
                // whose verdict decides a force-kill, so it gets the honest
                // budget, not the 1.5s reachability one (see
                // classify_backend_readyz / Sentry RUST-2X).
                let (backend_readyz_outcome, readyz_body) = classify_backend_readyz(|| {
                    probe_backend_readyz_with_body(std::time::Duration::from_secs(5))
                });
                if backend_readyz_outcome == "ok" {
                    log::info!(
                        "watchdog: backend /readyz answers ok after {consecutive_failures} intercept failures; skipping auto-pause and resetting counter"
                    );
                    consecutive_failures = 0;
                    continue;
                }
                // Upstream-only 503: process alive and answering, only the
                // cached upstream-connectivity probe is failing. Bypassing to
                // Anthropic routes to the same unreachable upstream and buys
                // nothing, and the process self-heals on the next 30s upstream
                // re-check — so keep it up instead of auto-pausing. Backstops
                // the same guard at the tolerant re-probe above.
                if readyz_failure_is_upstream_only(&backend_readyz_outcome) {
                    log::info!(
                        "watchdog: backend /readyz 503 (upstream-only) after {consecutive_failures} failures; process healthy, skipping auto-pause"
                    );
                    consecutive_failures = 0;
                    continue;
                }
                // Wedged backend: /readyz never responds ("timeout", the event
                // loop is held), or it 503s with a *core* component unhealthy
                // (startup/cache/memory/etc. failed to initialize), or it 503s
                // with a body we couldn't read/parse (bare "http_503" — the
                // status line came back but the body read timed out under load).
                // All three mean the process is alive and answering HTTP but not
                // ready, a state a clean restart may clear. ensure_headroom_running
                // returns Ok immediately when try_wait says the child is still
                // alive, so the three restart attempts above were all no-ops.
                // Kill the stuck process and start fresh before giving up
                // permanently. Once per down episode (hung_kill_attempted) so a
                // persistently-wedged new process doesn't loop; it falls through
                // to the give-up path below.
                if (backend_readyz_outcome == "timeout"
                    || backend_readyz_outcome == "http_503"
                    || readyz_failure_has_core_unhealthy(&backend_readyz_outcome))
                    && !hung_kill_attempted
                {
                    log::info!(
                        "watchdog: backend wedged ({backend_readyz_outcome}) after {consecutive_failures} failures; force-killing and restarting"
                    );
                    hung_kill_attempted = true;
                    // Wedges leave no diagnostics (the proxy log just goes
                    // silent), so ask the backend for a faulthandler dump of
                    // all Python threads before killing it.
                    state.dump_backend_stacks();
                    state.stop_headroom();
                    consecutive_failures = 0;
                    match state.ensure_headroom_running() {
                        Ok(()) => port_conflict::note_proxy_started(&app),
                        Err(err) => {
                            log::warn!("watchdog: hung-kill restart failed: {err:#}");
                            port_conflict::note_proxy_failed(&app, &err, false);
                        }
                    }
                    continue;
                }
                // Cold-boot rescue. "refused" means the backend port never
                // bound; combined with a tracked child that is still alive,
                // that is the signature of a process mid-cold-boot — uvicorn's
                // lifespan is synchronously pulling multi-GB model weights from
                // HuggingFace (kompress-base, ModernBERT, MiniLM) before it
                // binds. A watchdog-initiated restart spawns via
                // `start_headroom_background`, which returns before /readyz is
                // up and clears `starting` immediately, so the 15s give-up
                // clock ticks against a download that legitimately needs
                // minutes (see Sentry `proxy_unreachable_post_boot`). Hand the
                // child to the same boot-validation loop the launch path uses:
                // it waits out HF-cache growth / CPU / log activity under a
                // 600s ceiling, so a real download survives while a genuine
                // pre-bind hang still stalls out (~90s) and falls through to
                // the auto-pause below. Scoped to "refused" on purpose: a bound
                // "timeout" is the deadlock the hung-kill path already owns, and
                // a bound child would let /livez answer green and thrash this
                // loop forever.
                if backend_readyz_outcome == "refused" && state.tracked_child_alive() {
                    log::info!(
                        "watchdog: backend refused after {consecutive_failures} failures but tracked child is alive; waiting out cold boot before auto-pausing"
                    );
                    let outcome = state.wait_for_boot_validation(|_elapsed, _active| {});
                    if outcome.is_ok() {
                        log::info!(
                            "watchdog: cold boot completed (backend reachable); resetting failure counter"
                        );
                        consecutive_failures = 0;
                        hung_kill_attempted = false;
                        WATCHDOG_DOWN_CAPTURED.store(false, Ordering::Release);
                        continue;
                    }
                    log::info!(
                        "watchdog: cold-boot wait ended without reachability ({}); proceeding to auto-pause",
                        outcome.label()
                    );
                }
                // info! not warn!/error!: this is the documented recovery
                // path (flip bypass, pause runtime, notify user). FileLogger
                // forwards both warn! and error! to Sentry as capture_message,
                // which would produce a payload-less duplicate of the
                // structured event built by capture_watchdog_give_up below —
                // that one already carries the exit status, log tail, and
                // backend probe.
                log::info!(
                    "watchdog: giving up after {MAX_CONSECUTIVE_FAILURES} failures; pausing runtime and bypassing to Anthropic"
                );
                // Flip bypass FIRST so the Rust intercept passes new
                // requests straight through to Anthropic instead of returning
                // 502 in the window between Python being torn down and the
                // user noticing. See proxy_intercept.rs:161 — without this,
                // every request lands on the unreachable backend branch.
                //
                // "First" means before the diagnostic capture below, not just
                // before stop_headroom: capture_watchdog_give_up re-probes the
                // backend and sleeps ~4s to sample a CPU rate, so capturing
                // first held every request on the dead-backend branch for that
                // whole window purely to decide a Sentry level. Storing the
                // flag tears nothing down, so the capture still observes the
                // same pre-teardown state.
                state
                    .proxy_bypass
                    .store(true, std::sync::atomic::Ordering::Release);
                // Capture once per down episode, BEFORE stop_headroom tears
                // down the tracked child and the proxy log handle, so the
                // exit status and log tail reflect the failure we're about
                // to recover from. `bypass_active` is the snapshot read at the
                // top of this tick, so the flip above does not change what is
                // reported.
                capture_watchdog_give_up(
                    &*state,
                    consecutive_failures,
                    bypass_active,
                    backend_readyz_outcome,
                    readyz_body,
                    last_wall_jump.map(|(at, gap)| (at.elapsed().as_secs(), gap)),
                );
                state.set_runtime_paused(true);
                // Mark this as an AUTO pause (distinct from a user pause) so the
                // self-heal loop above will keep retrying and the UI shows the
                // "stopped unexpectedly" banner with a Resume button.
                state.set_runtime_auto_paused(true);
                state.stop_headroom();
                analytics::track_event(&app, "runtime_auto_paused", None);
                let _ = show_notification_impl(
                    &app,
                    "Headroom paused",
                    "Headroom couldn't restart its proxy. Requests are passing through unmodified — it'll keep retrying automatically, or open Headroom and hit Resume.",
                    Some("connectors".into()),
                );
                // Arm the self-heal: first retry after 30s, backing off on
                // repeated failures (auto_resume_backoff). The retry runs in the
                // `runtime.auto_paused` branch at the top of the loop.
                auto_pause_next_retry =
                    Some(std::time::Instant::now() + auto_resume_backoff(auto_pause_failed));
                auto_pause_failed = auto_pause_failed.saturating_add(1);
                consecutive_failures = 0;
                continue;
            }

            // Otherwise try to bring it back. When we own no child, tear down
            // whatever holds the backend port first (identity-verified inside
            // stop_headroom) — exactly what the auto-pause self-heal does. A
            // bare ensure_headroom_running can short-circuit Ok on its own
            // (cached/laxer) reachability view and "restart" nothing, letting
            // strikes reach give-up without a single spawn attempt (RUST-53).
            if !state.tracked_child_alive() {
                state.stop_headroom();
            }
            match state.ensure_headroom_running() {
                Ok(()) => port_conflict::note_proxy_started(&app),
                Err(err) => {
                    // info! not warn!: FileLogger forwards warn!/error! to
                    // Sentry as a payload-less capture_message. This fires on
                    // every failed retry during a down episode; the structured,
                    // actionable signal is capture_watchdog_give_up above, sent
                    // once per episode after MAX_CONSECUTIVE_FAILURES.
                    log::info!("watchdog: ensure_headroom_running failed: {err:#}");
                    // In-session retry: don't bump the launch counter.
                    port_conflict::note_proxy_failed(&app, &err, false);
                }
            }
        }
    });
}

fn spawn_tray_savings_updater(app: AppHandle) {
    // The tray icon's dollar badge only redraws when the integer value
    // changes (see `changed_dollars` in `spawn_tray_runtime_icon_updater`),
    // so polling faster than the number ticks up is wasted work. 20s is
    // fast enough that the badge feels live during active traffic and slow
    // enough that `build_dashboard` runs ~3x/min instead of 12x/min.
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
    std::thread::spawn(move || loop {
        std::thread::sleep(INTERVAL);
        let state: tauri::State<'_, AppState> = app.state();
        let dashboard = state.dashboard();
        let today_key = Local::now().format("%Y-%m-%d").to_string();
        let savings: f64 = dashboard
            .hourly_savings
            .iter()
            .filter(|p| p.hour.starts_with(&today_key))
            // Both Headroom layers, matching the home chart's headline total.
            .map(|p| p.estimated_savings_usd + p.output_savings_usd)
            .sum();
        let savings_state: tauri::State<'_, TraySessionSavings> = app.state();
        *savings_state.0.lock() = savings;
        let _ = app.emit("savings-today-updated", savings);
    });
}

fn build_tray_runtime_icons() -> anyhow::Result<TrayRuntimeIcons> {
    let decoded = image::load_from_memory_with_format(
        include_bytes!("../icons/32x32.png"),
        image::ImageFormat::Png,
    )?
    .to_rgba8();
    let width = decoded.width();
    let height = decoded.height();
    let rgba = decoded.into_vec();

    let off_rgba = add_red_badge_dot(to_grayscale_strength(&rgba, 1.0), width, height);
    // Paused intentionally has no badge — distinguishes "user chose off" from
    // "broken and needs attention" at a glance.
    let paused_rgba = to_grayscale_strength(&rgba, 1.0);
    let booting_base = to_grayscale_strength(&rgba, 0.5);
    let booting_90 = rotate_90_cw(&booting_base, width, height);
    let booting_180 = rotate_90_cw(&booting_90, width, height);
    let booting_270 = rotate_90_cw(&booting_180, width, height);

    Ok(TrayRuntimeIcons {
        off: tauri::image::Image::new_owned(off_rgba, width, height),
        paused: tauri::image::Image::new_owned(paused_rgba, width, height),
        running_rgba: rgba,
        running_dims: (width, height),
        booting_frames: vec![
            tauri::image::Image::new_owned(booting_base, width, height),
            tauri::image::Image::new_owned(booting_90, width, height),
            tauri::image::Image::new_owned(booting_180, width, height),
            tauri::image::Image::new_owned(booting_270, width, height),
        ],
    })
}

fn to_grayscale_strength(rgba: &[u8], strength: f32) -> Vec<u8> {
    let s = strength.clamp(0.0, 1.0);
    let mut out = rgba.to_vec();
    for pixel in out.chunks_exact_mut(4) {
        let r = pixel[0] as f32;
        let g = pixel[1] as f32;
        let b = pixel[2] as f32;
        let gray = 0.299 * r + 0.587 * g + 0.114 * b;
        pixel[0] = (r * (1.0 - s) + gray * s).round() as u8;
        pixel[1] = (g * (1.0 - s) + gray * s).round() as u8;
        pixel[2] = (b * (1.0 - s) + gray * s).round() as u8;
    }
    out
}

fn rotate_90_cw(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; rgba.len()];
    let w = width as usize;
    let h = height as usize;

    for y in 0..h {
        for x in 0..w {
            let src_idx = (y * w + x) * 4;
            let dst_x = h - 1 - y;
            let dst_y = x;
            let dst_idx = (dst_y * w + dst_x) * 4;
            out[dst_idx..dst_idx + 4].copy_from_slice(&rgba[src_idx..src_idx + 4]);
        }
    }
    out
}

fn add_red_badge_dot(mut rgba: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
    let w = width as i32;
    let h = height as i32;
    let cx = w - 5;
    let cy = 5;
    let radius = 3i32;

    for y in 0..h {
        for x in 0..w {
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy <= radius * radius {
                let idx = ((y as usize * width as usize) + x as usize) * 4;
                rgba[idx] = 217;
                rgba[idx + 1] = 76;
                rgba[idx + 2] = 76;
                rgba[idx + 3] = 255;
            }
        }
    }

    rgba
}

fn handle_window_event(window: &Window, event: &WindowEvent) {
    match event {
        WindowEvent::Focused(false) => {
            // An update install steals focus with a privilege prompt; hiding
            // underneath it strands the user mid-flow.
            if INSTALLING_UPDATE.load(Ordering::Acquire) {
                return;
            }
            if window.label() == "main" {
                let window = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(
                        MAIN_WINDOW_BLUR_HIDE_DELAY_MS,
                    ));

                    let still_unfocused = matches!(window.is_focused(), Ok(false));
                    let still_visible = matches!(window.is_visible(), Ok(true));
                    if still_unfocused && still_visible {
                        let _ = window.hide();
                    }
                });
            }
        }
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
        }
        _ => {}
    }
}

struct TraySessionSavings(Mutex<f64>);

// Returns a (possibly wider) RGBA image with whole-dollar savings stacked
// vertically to the right of the base icon. Returns the base unchanged when
// dollars == 0.
fn build_running_with_savings(
    base: &[u8],
    base_w: u32,
    base_h: u32,
    dollars: u32,
) -> (Vec<u8>, u32, u32) {
    if dollars == 0 {
        return (base.to_vec(), base_w, base_h);
    }

    const CHAR_W: usize = 3;
    const CHAR_H: usize = 5;
    const H_MARGIN: usize = 2; // pixel gap between icon and text column

    let text = if dollars >= 1000 {
        format!("{}K", dollars / 1000)
    } else {
        dollars.to_string()
    };
    let chars: Vec<u8> = text.bytes().collect();
    let n = chars.len();

    // 2-digit values get a slightly larger gap since there's room.
    let row_gap_px: usize = if n <= 2 { 2 } else { 1 };

    // Largest dot size that fits: n*CHAR_H*dot + (n-1)*row_gap_px <= base_h
    let available = (base_h as usize).saturating_sub(n.saturating_sub(1) * row_gap_px);
    let max_dot = if n <= 2 { 3 } else { 2 };
    let dot = (available / (n * CHAR_H)).clamp(1, max_dot);

    let col_px_w = CHAR_W * dot + H_MARGIN;
    let new_w = base_w + col_px_w as u32;
    let h = base_h as usize;
    let bw = base_w as usize;
    let nw = new_w as usize;

    let mut out = vec![0u8; nw * h * 4];

    // Copy base icon into left portion.
    for y in 0..h {
        let src = y * bw * 4;
        let dst = y * nw * 4;
        out[dst..dst + bw * 4].copy_from_slice(&base[src..src + bw * 4]);
    }

    // Stack digits vertically in the right column, centred on the icon height.
    let total_h = n * CHAR_H * dot + n.saturating_sub(1) * row_gap_px;
    let y0 = h.saturating_sub(total_h) / 2;
    let x0 = bw + H_MARGIN;

    for (ci, &c) in chars.iter().enumerate() {
        let glyph = pixel_char(c);
        let cy = y0 + ci * (CHAR_H * dot + row_gap_px);
        for (row, cols) in glyph.iter().enumerate() {
            for (col, &on) in cols.iter().enumerate() {
                if on == 0 {
                    continue;
                }
                for dy in 0..dot {
                    for dx in 0..dot {
                        let px = x0 + col * dot + dx;
                        let py = cy + row * dot + dy;
                        if px < nw && py < h {
                            let i = (py * nw + px) * 4;
                            out[i] = 80;
                            out[i + 1] = 210;
                            out[i + 2] = 100;
                            out[i + 3] = 240;
                        }
                    }
                }
            }
        }
    }

    // macOS menu bars accept wide status images; everywhere else the tray
    // cell is square and the shell squashes a wide image into it, distorting
    // the icon. Pad to square so downscaling stays uniform.
    if cfg!(target_os = "macos") {
        return (out, new_w, base_h);
    }
    let side = nw.max(h);
    let mut square = vec![0u8; side * side * 4];
    let y_off = (side - h) / 2;
    for y in 0..h {
        let src = y * nw * 4;
        let dst = (y + y_off) * side * 4;
        square[dst..dst + nw * 4].copy_from_slice(&out[src..src + nw * 4]);
    }
    (square, side as u32, side as u32)
}

// Each glyph is [[col0, col1, col2]; 5 rows], top to bottom.
fn pixel_char(c: u8) -> [[u8; 3]; 5] {
    match c {
        b'0' => [[1, 1, 1], [1, 0, 1], [1, 0, 1], [1, 0, 1], [1, 1, 1]],
        b'1' => [[0, 1, 0], [1, 1, 0], [0, 1, 0], [0, 1, 0], [1, 1, 1]],
        b'2' => [[1, 1, 1], [0, 0, 1], [1, 1, 1], [1, 0, 0], [1, 1, 1]],
        b'3' => [[1, 1, 1], [0, 0, 1], [1, 1, 1], [0, 0, 1], [1, 1, 1]],
        b'4' => [[1, 0, 1], [1, 0, 1], [1, 1, 1], [0, 0, 1], [0, 0, 1]],
        b'5' => [[1, 1, 1], [1, 0, 0], [1, 1, 1], [0, 0, 1], [1, 1, 1]],
        b'6' => [[1, 1, 1], [1, 0, 0], [1, 1, 1], [1, 0, 1], [1, 1, 1]],
        b'7' => [[1, 1, 1], [0, 0, 1], [0, 0, 1], [0, 0, 1], [0, 0, 1]],
        b'8' => [[1, 1, 1], [1, 0, 1], [1, 1, 1], [1, 0, 1], [1, 1, 1]],
        b'9' => [[1, 1, 1], [1, 0, 1], [1, 1, 1], [0, 0, 1], [1, 1, 1]],
        b'K' => [[1, 0, 1], [1, 1, 0], [1, 0, 0], [1, 1, 0], [1, 0, 1]],
        _ => [[0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0]],
    }
}

fn toggle_main_window(app: &AppHandle, anchor_rect: Option<Rect>) -> tauri::Result<()> {
    // Teardown in flight: uninstall deletes the managed runtime while we are
    // still alive, so onboarding_complete() flips false and a tray click here
    // would hide the uninstall progress and pop Get Started over a teardown the
    // user cannot dismiss. Ignore clicks once we are on the way out.
    if SHUTTING_DOWN.load(Ordering::Acquire) {
        return Ok(());
    }

    if !onboarding_complete(app) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
        show_launcher_window(app)?;
        return Ok(());
    }

    hide_launcher_window(app)?;

    let Some(window) = app.get_webview_window("main") else {
        return Err(tauri::Error::WebviewNotFound);
    };

    if window.is_visible()? {
        window.hide()?;
    } else {
        show_main_window(app, anchor_rect)?;
        // Start/verify headroom in the background so the window appears immediately.
        let app_bg = app.clone();
        std::thread::spawn(move || ensure_runtime_ready_for_tray(&app_bg));
    }

    Ok(())
}

fn ensure_runtime_ready_for_tray(app: &AppHandle) {
    let state: tauri::State<'_, AppState> = app.state();
    if state.runtime_is_paused() {
        return;
    }
    match state.ensure_headroom_running() {
        Ok(()) => port_conflict::note_proxy_started(app),
        Err(err) => {
            // The managed runtime can disappear out from under a running app
            // (disk cleanup, AV quarantine, a wiped Application Support dir), or
            // race away between the onboarding_complete gate and this call. In
            // that case ensure_headroom_running bails "managed python not found"
            // -- a recoverable not-installed state, not a startup crash, so
            // capturing it as one produced misleading Sentry noise (RUST-1M).
            // Route back to the setup window, which re-runs bootstrap to restore
            // the runtime, instead of treating it as a failed start.
            if !state.tool_manager.python_runtime_installed() {
                log::warn!(
                    "ensure_runtime_ready_for_tray: managed runtime missing; routing to setup: {err:#}"
                );
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
                let _ = show_launcher_window(app);
                return;
            }
            // Tray open is in-session (not a fresh launch); pass false so the
            // launch counter is preserved instead of double-counting clicks.
            let handled = port_conflict::note_proxy_failed(app, &err, false);
            if !handled {
                capture_headroom_start_failure("ensure_runtime_ready_for_tray failed", &err);
            }
        }
    }
}

fn onboarding_complete(app: &AppHandle) -> bool {
    let state: tauri::State<'_, AppState> = app.state();
    if !state.tool_manager.python_runtime_installed() {
        return false;
    }
    state.setup_wizard_satisfied()
}

#[tauri::command]
fn complete_setup_wizard(state: tauri::State<'_, AppState>) {
    state.mark_setup_wizard_complete();
}

#[tauri::command]
async fn accept_terms(app: AppHandle, version: u32) {
    // Local acceptance is the authoritative gate (works offline / pre-signin).
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.mark_terms_accepted(version);
    }
    // Best-effort: tell the server now. `fetch_grace_start` is blocking, so
    // run it off the IPC thread; failures are swallowed and the value rides
    // along on the next identity push regardless.
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app.state();
        crate::pricing::push_terms_acceptance(&state, version);
    });
}

fn show_main_window(app: &AppHandle, anchor_rect: Option<Rect>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Err(tauri::Error::WebviewNotFound);
    };

    if let Some(rect) = anchor_rect {
        position_tray_window(&window, rect)?;
    } else {
        #[cfg(target_os = "linux")]
        position_near_panel(&window)?;
    }

    window.show()?;
    let _ = window.unminimize();
    window.set_focus()?;
    Ok(())
}

fn show_launcher_window(app: &AppHandle) -> tauri::Result<()> {
    // Choke point for every "route back to setup" path (tray click, tray menu,
    // ensure_runtime_ready_for_tray, show_dashboard_window, second instance).
    // During uninstall the runtime is already gone, so all of them would raise
    // the onboarding window on top of a quitting app.
    if SHUTTING_DOWN.load(Ordering::Acquire) {
        return Ok(());
    }

    let Some(window) = app.get_webview_window("launcher") else {
        return Err(tauri::Error::WebviewNotFound);
    };

    let _ = window.center();
    window.show()?;
    let _ = window.unminimize();
    let _ = window.center();
    window.set_focus()?;
    Ok(())
}

fn hide_launcher_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("launcher") {
        // No is_visible() guard: NSWindow.isVisible false-negatives (miniaturized
        // window, hidden app) made the guard skip real hides, leaving the launcher
        // stuck on screen. hide() on an already-hidden window is a no-op anyway.
        window.hide()?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysicalRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn position_tray_window(window: &tauri::WebviewWindow, rect: Rect) -> tauri::Result<()> {
    let scale_factor = window.scale_factor()?;
    let tray_rect = physical_rect_from_rect(rect, scale_factor);
    let window_size = window
        .outer_size()
        .unwrap_or_else(|_| PhysicalSize::new(MAIN_WINDOW_WIDTH, MAIN_WINDOW_HEIGHT));
    let monitor_bounds = resolve_monitor_bounds(window, tray_rect);
    let target = compute_tray_window_position(tray_rect, window_size, monitor_bounds);

    window.set_position(Position::Physical(target))
}

/// Linux tray backends (libappindicator/StatusNotifier) never report click
/// events or an icon rect, so `show_main_window` gets no anchor there and the
/// window stays wherever the config's `center: true` put it. Drop it into the
/// panel corner instead.
#[cfg(target_os = "linux")]
fn position_near_panel(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    let Some(monitor) = window
        .current_monitor()?
        .or_else(|| window.primary_monitor().ok().flatten())
    else {
        return Ok(());
    };
    let area = monitor.work_area();
    let work_area = MonitorBounds {
        x: area.position.x,
        y: area.position.y,
        width: i32::try_from(area.size.width).unwrap_or(i32::MAX),
        height: i32::try_from(area.size.height).unwrap_or(i32::MAX),
    };
    let window_size = window
        .outer_size()
        .unwrap_or_else(|_| PhysicalSize::new(MAIN_WINDOW_WIDTH, MAIN_WINDOW_HEIGHT));

    window.set_position(Position::Physical(compute_panel_corner_position(
        work_area,
        window_size,
    )))
}

/// Top-right of the work area (which already excludes the panel), inset by the
/// same gap the tray-anchored path uses.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn compute_panel_corner_position(
    work_area: MonitorBounds,
    window_size: PhysicalSize<u32>,
) -> PhysicalPosition<i32> {
    let window_width = i32::try_from(window_size.width).unwrap_or(i32::MAX);
    let inset_x = work_area
        .width
        .saturating_sub(window_width)
        .saturating_sub(TRAY_WINDOW_VERTICAL_GAP)
        .max(0);

    PhysicalPosition::new(
        work_area.x.saturating_add(inset_x),
        work_area.y.saturating_add(TRAY_WINDOW_VERTICAL_GAP),
    )
}

fn physical_rect_from_rect(rect: Rect, scale_factor: f64) -> PhysicalRect {
    let (x, y) = match rect.position {
        Position::Physical(position) => (position.x, position.y),
        Position::Logical(position) => (
            (position.x * scale_factor).round() as i32,
            (position.y * scale_factor).round() as i32,
        ),
    };
    let (width, height) = match rect.size {
        tauri::Size::Physical(size) => (
            i32::try_from(size.width).unwrap_or(i32::MAX),
            i32::try_from(size.height).unwrap_or(i32::MAX),
        ),
        tauri::Size::Logical(size) => (
            (size.width * scale_factor).round() as i32,
            (size.height * scale_factor).round() as i32,
        ),
    };

    PhysicalRect {
        x,
        y,
        width,
        height,
    }
}

fn resolve_monitor_bounds(
    window: &tauri::WebviewWindow,
    tray_rect: PhysicalRect,
) -> Option<MonitorBounds> {
    let anchor_x = tray_rect.x + (tray_rect.width / 2);
    let anchor_y = tray_rect.y + (tray_rect.height / 2);

    if let Ok(monitors) = window.available_monitors() {
        if let Some(bounds) = monitors
            .into_iter()
            .map(monitor_bounds_from_monitor)
            .find(|bounds| point_within_monitor(*bounds, anchor_x, anchor_y))
        {
            return Some(bounds);
        }
    }

    window
        .current_monitor()
        .ok()
        .flatten()
        .map(monitor_bounds_from_monitor)
}

fn monitor_bounds_from_monitor(monitor: tauri::Monitor) -> MonitorBounds {
    MonitorBounds {
        x: monitor.position().x,
        y: monitor.position().y,
        width: i32::try_from(monitor.size().width).unwrap_or(i32::MAX),
        height: i32::try_from(monitor.size().height).unwrap_or(i32::MAX),
    }
}

fn point_within_monitor(bounds: MonitorBounds, x: i32, y: i32) -> bool {
    let max_x = bounds.x.saturating_add(bounds.width);
    let max_y = bounds.y.saturating_add(bounds.height);
    x >= bounds.x && x < max_x && y >= bounds.y && y < max_y
}

fn compute_tray_window_position(
    tray_rect: PhysicalRect,
    window_size: PhysicalSize<u32>,
    monitor_bounds: Option<MonitorBounds>,
) -> PhysicalPosition<i32> {
    let window_width = i32::try_from(window_size.width).unwrap_or(i32::MAX);
    let window_height = i32::try_from(window_size.height).unwrap_or(i32::MAX);
    let centered_x = tray_rect
        .x
        .saturating_add(tray_rect.width / 2)
        .saturating_sub(window_width / 2);
    let below_y = tray_rect
        .y
        .saturating_add(tray_rect.height)
        .saturating_add(TRAY_WINDOW_VERTICAL_GAP);

    if let Some(bounds) = monitor_bounds {
        let max_x = bounds
            .x
            .saturating_add(bounds.width.saturating_sub(window_width).max(0));
        let clamped_x = centered_x.clamp(bounds.x, max_x);

        let max_y = bounds
            .y
            .saturating_add(bounds.height.saturating_sub(window_height).max(0));
        let above_y = tray_rect
            .y
            .saturating_sub(window_height)
            .saturating_sub(TRAY_WINDOW_VERTICAL_GAP);
        let target_y =
            if below_y.saturating_add(window_height) <= bounds.y.saturating_add(bounds.height) {
                below_y
            } else {
                above_y.clamp(bounds.y, max_y)
            };

        return PhysicalPosition::new(clamped_x, target_y);
    }

    PhysicalPosition::new(centered_x, below_y)
}

#[cfg(test)]
mod tests {
    use super::{agent_process_counts_from_lines, claude_sessions_touched_since};
    use super::{
        aggregate_live_learnings, app_quit_requested_properties, app_update_notification_body,
        auto_resume_backoff, beta_channel_enabled_from, build_release_updater_config,
        build_watchdog_give_up_report, check_headroom_learn_prereqs, child_state_fingerprint_key,
        classify_backend_readyz, classify_bootstrap_failure, classify_update_check,
        classify_upgrade_error, client_setup_error_kind, compute_panel_corner_position,
        compute_tray_window_position, conflicting_openssl_dirs, count_memories_created_today,
        cpu_rate_indicates_burn, debounced_tray_runtime_visual, delete_applied_pattern,
        empty_live_learnings_for_projects, exe_path_resolvable, extract_llm_failure_warnings,
        fake_override, fetch_transformations_feed_from, first_savings_body, format_token_count,
        install_pending_update, is_blocked_runtime_dll_signal, is_disk_full_signal,
        is_endpoint_protection_signal, is_network_download_signal, is_port_conflict_failure,
        is_prerelease_version, learn_agent_auth_hint, learn_agent_limit_hint,
        learn_failure_agent_limit_line, learn_failure_is_agent_auth,
        learn_failure_is_agent_model_rejected, learn_failure_signature_source, learn_step_label,
        lifetime_token_milestone_kind, noop_app_update_progress_emitter,
        normalize_learn_failure_signature, onboarding_recovery_copy, parse_live_learnings,
        parse_magic_link_auth, parse_request_count_from_stats_body, parse_request_counts_by_agent,
        parse_updater_endpoint_list, pattern_matches_project, persistent_zero_spend,
        physical_rect_from_rect, read_applied_patterns_for_project, readyz_failed_checks_csv,
        readyz_failure_has_core_unhealthy, readyz_failure_is_upstream_only,
        readyz_outcome_fingerprint_key, recent_savings_days, resolve_release_updater_config,
        savings_report, select_updater_endpoints, startup_error_fingerprint_key,
        store_checked_update, strip_connection_noise, tail_bytes_for_sentry,
        take_pending_magic_link, user_message_for, watchdog_should_be_up, zero_spend_affected_days,
        AppUpdateProgress, AppUpdateProgressEmitter, AvailableAppUpdate, BootstrapFailureKind,
        DailySavingsPoint, HeadroomLearnPrereqStatus, InstallPendingUpdateFuture,
        InstallableAppUpdate, LearnAgent, MonitorBounds, PhysicalRect, QuitSource,
        TrayRuntimeVisual, DEFAULT_UPDATER_ENDPOINT, DEFAULT_UPDATER_PUBLIC_KEY,
        PENDING_MAGIC_LINK,
    };
    use parking_lot::Mutex;
    use serde_json::json;
    use std::sync::Arc;
    use tauri::{LogicalPosition, LogicalSize, PhysicalSize, Position, Rect, Size};

    struct FakePendingUpdate {
        metadata: AvailableAppUpdate,
        install_result: Result<(), String>,
    }

    impl InstallableAppUpdate for FakePendingUpdate {
        fn metadata(&self) -> AvailableAppUpdate {
            self.metadata.clone()
        }

        fn install(self, _progress: AppUpdateProgressEmitter) -> InstallPendingUpdateFuture {
            Box::pin(async move { self.install_result })
        }
    }

    #[test]
    fn exe_path_resolvable_rejects_a_bundle_that_no_longer_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("Headroom");
        std::fs::write(&exe, b"").expect("write exe");
        assert!(exe_path_resolvable(Ok(exe.clone())));

        // The RUST-6Q shape: the .app was moved or replaced while launching, so
        // the path the process was started from no longer canonicalizes.
        std::fs::remove_file(&exe).expect("remove exe");
        assert!(!exe_path_resolvable(Ok(exe)));

        assert!(!exe_path_resolvable(Err(std::io::Error::from(
            std::io::ErrorKind::NotFound
        ))));

        // Sanity: a healthy process (this test binary) passes.
        assert!(exe_path_resolvable(std::env::current_exe()));
    }

    fn sample_available_update(version: &str) -> AvailableAppUpdate {
        AvailableAppUpdate {
            current_version: "0.2.9".into(),
            version: version.into(),
            published_at: Some("2026-04-02T12:00:00Z".into()),
            notes: Some("Bug fixes.".into()),
        }
    }

    fn daily_point(
        date: &str,
        savings_usd: f64,
        tokens_saved: u64,
        cost_usd: f64,
        tokens_sent: u64,
    ) -> DailySavingsPoint {
        DailySavingsPoint {
            date: date.into(),
            estimated_savings_usd: savings_usd,
            estimated_tokens_saved: tokens_saved,
            tool_schema_savings_usd: 0.0,
            tool_schema_tokens_saved: 0,
            actual_cost_usd: cost_usd,
            total_tokens_sent: tokens_sent,
            new_input_tokens: 0,
            output_savings_usd: 0.0,
            output_tokens_saved: 0,
            cache_read_tokens: None,
            cache_savings_usd: None,
            output_sampled_tokens_saved: None,
            output_baseline_tokens: None,
        }
    }

    #[test]
    fn recent_savings_days_keeps_last_30_active_days_oldest_first() {
        let mut points: Vec<_> = (1..=40)
            .map(|i| daily_point(&format!("2026-06-{i:02}"), 0.5, 1_000, 1.0, 9_000))
            .collect();
        // Two idle days in the middle must not consume window slots.
        points[35] = daily_point("2026-06-36", 0.0, 0, 0.0, 0);
        points[36] = daily_point("2026-06-37", 0.0, 0, 0.0, 0);

        let days = recent_savings_days(&points);

        assert_eq!(days.len(), 30);
        assert_eq!(days.first().unwrap().date, "2026-06-09");
        assert_eq!(days.last().unwrap().date, "2026-06-40");
        assert!(days.iter().all(|d| d.tokens_saved == 1_000));
    }

    #[test]
    fn recent_savings_days_is_empty_without_traffic() {
        let points = vec![daily_point("2026-06-01", 0.0, 0, 0.0, 0)];
        assert!(recent_savings_days(&points).is_empty());
    }

    fn dashboard_for_report(
        breakdown: Option<crate::models::SavingsBreakdown>,
    ) -> crate::models::DashboardState {
        crate::models::DashboardState {
            app_version: "test".into(),
            launch_experience: crate::models::LaunchExperience::Dashboard,
            bootstrap_complete: true,
            python_runtime_installed: true,
            lifetime_requests: 1,
            first_prompt_request_seen: true,
            lifetime_estimated_savings_usd: 307.66,
            lifetime_estimated_tokens_saved: 63_712_824,
            session_requests: 0,
            session_estimated_savings_usd: 0.0,
            session_estimated_tokens_saved: 0,
            session_savings_pct: 0.0,
            output_reduction: None,
            output_shaper_active: None,
            learner_progress: None,
            reread_tokens: None,
            reread_compressed_tokens: None,
            ccr_retrievals: None,
            savings_breakdown: breakdown,
            daily_savings: Vec::new(),
            hourly_savings: Vec::new(),
            savings_history_loaded: false,
            tools: Vec::new(),
            clients: Vec::new(),
            recent_usage: Vec::new(),
            insights: Vec::new(),
            required_terms_version: 1,
            accepted_terms_version: 1,
            terms_url: String::new(),
        }
    }

    #[test]
    fn savings_report_withheld_until_history_hydrates() {
        // A report built before /stats-history answers would post zero rate
        // denominators, which the server stores over a previous good snapshot.
        assert!(savings_report(&dashboard_for_report(None)).is_none());
    }

    #[test]
    fn savings_report_carries_breakdown_denominators() {
        let breakdown = crate::models::SavingsBreakdown {
            compression_savings_usd: 100.0,
            output_savings_usd: 0.0,
            tool_schema_savings_usd: 0.0,
            tool_schema_tokens_saved: 0,
            cache_savings_usd: 30.0,
            cache_read_tokens: 2_000,
            total_input_tokens: 1_000,
            total_input_cost_usd: 11.09,
            model_rates: Vec::new(),
        };
        let report =
            savings_report(&dashboard_for_report(Some(breakdown))).expect("hydrated report");
        assert_eq!(report.total_input_cost_usd, 11.09);
        assert_eq!(report.cache_savings_usd, 30.0);
        assert_eq!(report.lifetime_savings_usd, 307.66);
        assert_eq!(report.lifetime_tokens_saved, 63_712_824);
    }

    #[test]
    fn zero_spend_ignores_days_with_only_cli_filtering_savings() {
        // CLI/RTK filtering inflates the token total but never the compression
        // dollar figure (those tokens never reach a model request), so a day with
        // token savings but zero compression-USD is not an anomaly.
        let days = vec![daily_point("2026-06-16", 0.0, 5_000, 0.0, 0)];
        assert!(zero_spend_affected_days(&days, "2026-01-01", "2099-01-01").is_empty());
    }

    #[test]
    fn zero_spend_flags_compression_savings_with_no_spend() {
        // On a spend-reporting proxy (the 06-15 day proves it reports), a separate
        // compression-savings day that recorded zero spend is the genuine anomaly.
        let days = vec![
            daily_point("2026-06-15", 0.20, 9_000, 0.50, 12_000),
            daily_point("2026-06-16", 0.12, 5_000, 0.0, 0),
        ];
        assert_eq!(
            zero_spend_affected_days(&days, "2026-01-01", "2099-01-01"),
            vec!["2026-06-16"]
        );
    }

    #[test]
    fn zero_spend_suppressed_when_proxy_never_reports_spend() {
        // Old proxy that omits spend fields: every day lands at zero spend, so a
        // compression-savings day is a reporting gap, not an anomaly (RUST-3S/3V).
        let days = vec![
            daily_point("2026-06-15", 0.20, 9_000, 0.0, 0),
            daily_point("2026-06-16", 0.12, 5_000, 0.0, 0),
        ];
        assert!(zero_spend_affected_days(&days, "2026-01-01", "2099-01-01").is_empty());
    }

    #[test]
    fn zero_spend_ignores_compression_days_that_recorded_spend() {
        let days = vec![daily_point("2026-06-16", 0.12, 5_000, 0.34, 8_000)];
        assert!(zero_spend_affected_days(&days, "2026-01-01", "2099-01-01").is_empty());
    }

    #[test]
    fn zero_spend_ignores_pre_schema_cutoff_days() {
        // Pre-v6 records deserialize spend fields as 0; never flag them even when
        // the proxy otherwise reports spend (the 06-16 day).
        let days = vec![
            daily_point("2026-06-16", 0.20, 9_000, 0.50, 12_000),
            daily_point("2026-04-12", 0.12, 5_000, 0.0, 0),
        ];
        assert!(zero_spend_affected_days(&days, "2026-01-01", "2099-01-01").is_empty());
    }

    #[test]
    fn zero_spend_ignores_days_older_than_min_date() {
        // Regression (RUST-3S/3V): historical zero-spend days are immutable
        // (written by a backend that predated spend reporting) and re-fired
        // the alert on every launch. Only days >= min_date may flag.
        let days = vec![
            daily_point("2026-06-15", 0.20, 9_000, 0.50, 12_000),
            daily_point("2026-06-16", 0.12, 5_000, 0.0, 0),
            daily_point("2026-07-03", 0.08, 2_000, 0.0, 0),
        ];
        assert_eq!(
            zero_spend_affected_days(&days, "2026-07-02", "2099-01-01"),
            vec!["2026-07-03"]
        );
    }

    #[test]
    fn zero_spend_ignores_the_live_day() {
        // RUST-4S: the current day's rollup is mid-accumulation (cost/token
        // counters lag the savings accumulator), so its savings-with-zero-spend
        // is not an anomaly. Only the most recent *settled* day (< the boundary)
        // may flag; a desynced live day at/after the boundary is excluded.
        let days = vec![
            daily_point("2026-07-13", 0.20, 9_000, 0.50, 12_000),
            daily_point("2026-07-14", 0.12, 5_000, 0.0, 0),
            daily_point("2026-07-15", 0.08, 2_000, 0.0, 0),
        ];
        // Boundary = 2026-07-15 (today): 07-14 settled and flags, 07-15 excluded.
        assert_eq!(
            zero_spend_affected_days(&days, "2026-07-14", "2026-07-15"),
            vec!["2026-07-14"]
        );
    }

    #[test]
    fn zero_spend_requires_desync_to_persist_across_probes() {
        use std::time::{Duration, Instant};
        let mut first_seen = std::collections::BTreeMap::new();
        let window = Duration::from_secs(600);
        let t0 = Instant::now();

        // First sighting never fires.
        assert!(!persistent_zero_spend(
            &mut first_seen,
            &["2026-07-13"],
            t0,
            window
        ));
        // Still inside the window: no fire.
        assert!(!persistent_zero_spend(
            &mut first_seen,
            &["2026-07-13"],
            t0 + Duration::from_secs(60),
            window
        ));
        // Backend healed (day dropped out), then re-desynced: timer restarts.
        assert!(!persistent_zero_spend(&mut first_seen, &[], t0, window));
        assert!(!persistent_zero_spend(
            &mut first_seen,
            &["2026-07-13"],
            t0 + window,
            window
        ));
        // Same day still desynced a full window later: fire.
        assert!(persistent_zero_spend(
            &mut first_seen,
            &["2026-07-13"],
            t0 + window + window,
            window
        ));
    }

    #[test]
    fn app_quit_requested_properties_include_source_and_runtime_state() {
        assert_eq!(
            app_quit_requested_properties(QuitSource::SettingsButton, false),
            json!({
                "source": "settings_button",
                "runtime_paused": false,
            })
        );
        assert_eq!(
            app_quit_requested_properties(QuitSource::TrayMenu, true),
            json!({
                "source": "tray_menu",
                "runtime_paused": true,
            })
        );
    }

    #[test]
    fn tray_visual_keeps_running_during_brief_unhealthy_probe_blips() {
        let mut unhealthy_streak = 0;

        for _ in 0..7 {
            assert_eq!(
                debounced_tray_runtime_visual(
                    TrayRuntimeVisual::Unhealthy,
                    Some(TrayRuntimeVisual::Running),
                    &mut unhealthy_streak,
                ),
                TrayRuntimeVisual::Running
            );
        }

        assert_eq!(
            debounced_tray_runtime_visual(
                TrayRuntimeVisual::Unhealthy,
                Some(TrayRuntimeVisual::Running),
                &mut unhealthy_streak,
            ),
            TrayRuntimeVisual::Unhealthy
        );
    }

    #[test]
    fn tray_visual_resets_unhealthy_streak_after_recovery() {
        let mut unhealthy_streak = 0;

        assert_eq!(
            debounced_tray_runtime_visual(
                TrayRuntimeVisual::Unhealthy,
                Some(TrayRuntimeVisual::Running),
                &mut unhealthy_streak,
            ),
            TrayRuntimeVisual::Running
        );
        assert_eq!(
            debounced_tray_runtime_visual(
                TrayRuntimeVisual::Running,
                Some(TrayRuntimeVisual::Running),
                &mut unhealthy_streak,
            ),
            TrayRuntimeVisual::Running
        );
        assert_eq!(unhealthy_streak, 0);
    }

    #[test]
    fn tray_savings_icon_shape_matches_platform() {
        let base = vec![255u8; 32 * 32 * 4];
        let (out, w, h) = super::build_running_with_savings(&base, 32, 32, 1);
        assert_eq!(out.len(), (w * h * 4) as usize);
        if cfg!(target_os = "macos") {
            // Menu bar accepts wide images: icon column plus text column.
            assert!(w > h);
            assert_eq!(h, 32);
        } else {
            // Square tray cells squash non-square images; output must be square.
            assert_eq!(w, h);
            assert!(w > 32);
        }
        // Zero dollars: base passes through untouched on every platform.
        let (out0, w0, h0) = super::build_running_with_savings(&base, 32, 32, 0);
        assert_eq!((w0, h0), (32, 32));
        assert_eq!(out0, base);
    }

    #[test]
    fn updater_endpoint_parser_accepts_json_arrays() {
        let parsed = parse_updater_endpoint_list(
            r#"["https://updates.example.com/latest.json", " https://backup.example.com/feed "]"#,
        )
        .expect("json endpoint list");

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].as_str(),
            "https://updates.example.com/latest.json"
        );
        assert_eq!(parsed[1].as_str(), "https://backup.example.com/feed");
    }

    #[test]
    fn updater_endpoint_parser_accepts_comma_or_newline_lists() {
        let parsed = parse_updater_endpoint_list(
            "https://updates.example.com/latest.json,\nhttps://backup.example.com/feed",
        )
        .expect("delimited endpoint list");

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].as_str(),
            "https://updates.example.com/latest.json"
        );
        assert_eq!(parsed[1].as_str(), "https://backup.example.com/feed");
    }

    #[test]
    fn updater_endpoint_parser_rejects_empty_or_insecure_values() {
        let empty = parse_updater_endpoint_list(" \n , ").expect_err("empty list should fail");
        assert!(empty.contains("HEADROOM_UPDATER_ENDPOINTS"));

        let insecure = parse_updater_endpoint_list("http://updates.example.com/latest.json")
            .expect_err("http endpoint should fail");
        assert!(insecure.contains("must use HTTPS"));
    }

    #[test]
    fn prerelease_versions_are_detected() {
        assert!(is_prerelease_version("0.2.44-rc.1"));
        assert!(is_prerelease_version("0.2.44-staging"));
        assert!(!is_prerelease_version("0.2.44"));
        assert!(!is_prerelease_version("1.0.0"));
    }

    #[test]
    fn beta_channel_enabled_from_recognises_truthy_env_values() {
        assert!(beta_channel_enabled_from(Some("1"), false));
        assert!(beta_channel_enabled_from(Some("true"), false));
        assert!(beta_channel_enabled_from(Some("TRUE"), false));
        assert!(beta_channel_enabled_from(Some(" yes "), false));
    }

    #[test]
    fn beta_channel_enabled_from_rejects_other_env_values() {
        assert!(!beta_channel_enabled_from(None, false));
        assert!(!beta_channel_enabled_from(Some(""), false));
        assert!(!beta_channel_enabled_from(Some("0"), false));
        assert!(!beta_channel_enabled_from(Some("false"), false));
        assert!(!beta_channel_enabled_from(Some("no"), false));
    }

    #[test]
    fn beta_channel_enabled_from_honours_sentinel_file() {
        assert!(beta_channel_enabled_from(None, true));
        assert!(beta_channel_enabled_from(Some("0"), true));
    }

    #[test]
    fn select_updater_endpoints_uses_stable_when_not_preferring_staging() {
        assert_eq!(
            select_updater_endpoints(Some("https://stable"), Some("https://staging"), false),
            Some("https://stable")
        );
        assert_eq!(
            select_updater_endpoints(Some("https://stable"), None, false),
            Some("https://stable")
        );
        assert_eq!(
            select_updater_endpoints(None, Some("https://staging"), false),
            None
        );
    }

    #[test]
    fn select_updater_endpoints_prefers_staging_when_available() {
        assert_eq!(
            select_updater_endpoints(Some("https://stable"), Some("https://staging"), true),
            Some("https://staging")
        );
    }

    #[test]
    fn select_updater_endpoints_falls_back_to_stable_when_staging_missing() {
        assert_eq!(
            select_updater_endpoints(Some("https://stable"), None, true),
            Some("https://stable")
        );
        assert_eq!(select_updater_endpoints(None, None, true), None);
    }

    #[test]
    fn resolve_release_updater_config_picks_stable_for_stable_version_with_beta_off() {
        let config = resolve_release_updater_config(
            "0.3.0",
            false,
            Some(DEFAULT_UPDATER_PUBLIC_KEY),
            Some("https://stable.example.com/latest.json"),
            Some("https://staging.example.com/latest.json"),
            false,
        )
        .expect("config")
        .expect("Some(config)");

        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(
            config.endpoints[0].as_str(),
            "https://stable.example.com/latest.json"
        );
    }

    #[test]
    fn resolve_release_updater_config_picks_staging_when_beta_channel_on() {
        let config = resolve_release_updater_config(
            "0.3.0",
            true,
            Some(DEFAULT_UPDATER_PUBLIC_KEY),
            Some("https://stable.example.com/latest.json"),
            Some("https://staging.example.com/latest.json"),
            false,
        )
        .expect("config")
        .expect("Some(config)");

        assert_eq!(
            config.endpoints[0].as_str(),
            "https://staging.example.com/latest.json"
        );
    }

    #[test]
    fn resolve_release_updater_config_picks_staging_for_prerelease_even_with_beta_off() {
        let config = resolve_release_updater_config(
            "0.3.1-rc.2",
            false,
            Some(DEFAULT_UPDATER_PUBLIC_KEY),
            Some("https://stable.example.com/latest.json"),
            Some("https://staging.example.com/latest.json"),
            false,
        )
        .expect("config")
        .expect("Some(config)");

        assert_eq!(
            config.endpoints[0].as_str(),
            "https://staging.example.com/latest.json"
        );
    }

    #[test]
    fn resolve_release_updater_config_falls_back_to_stable_when_staging_unconfigured() {
        let config = resolve_release_updater_config(
            "0.3.0",
            true,
            Some(DEFAULT_UPDATER_PUBLIC_KEY),
            Some("https://stable.example.com/latest.json"),
            None,
            false,
        )
        .expect("config")
        .expect("Some(config)");

        assert_eq!(
            config.endpoints[0].as_str(),
            "https://stable.example.com/latest.json"
        );
    }

    #[test]
    fn resolve_release_updater_config_returns_default_feed_when_nothing_configured_in_release() {
        let config = resolve_release_updater_config("0.3.0", false, None, None, None, false)
            .expect("config")
            .expect("Some(config)");

        assert_eq!(config.endpoints[0].as_str(), DEFAULT_UPDATER_ENDPOINT);
    }

    #[test]
    fn resolve_release_updater_config_disables_updates_in_debug_when_unconfigured() {
        let result = resolve_release_updater_config("0.3.0", true, None, None, None, true)
            .expect("debug config resolves to None");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_release_updater_config_errors_when_pubkey_missing() {
        let err = resolve_release_updater_config(
            "0.3.0",
            false,
            None,
            Some("https://stable.example.com/latest.json"),
            None,
            false,
        )
        .expect_err("missing pubkey error");
        assert!(err.contains("HEADROOM_UPDATER_PUBLIC_KEY"));
    }

    #[test]
    fn resolve_release_updater_config_errors_when_endpoints_missing() {
        let err = resolve_release_updater_config(
            "0.3.0",
            false,
            Some(DEFAULT_UPDATER_PUBLIC_KEY),
            None,
            None,
            false,
        )
        .expect_err("missing endpoints error");
        assert!(err.contains("HEADROOM_UPDATER_ENDPOINTS"));
    }

    #[test]
    fn updater_release_config_accepts_official_default_feed() {
        let config =
            build_release_updater_config(DEFAULT_UPDATER_PUBLIC_KEY, DEFAULT_UPDATER_ENDPOINT)
                .expect("official updater config");

        assert_eq!(config.pubkey, DEFAULT_UPDATER_PUBLIC_KEY);
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(
            config.endpoints[0].as_str(),
            "https://github.com/gglucass/headroom-desktop/releases/latest/download/latest.json"
        );
    }

    // The override is opt-in AND build-gated: a stable build must ignore the
    // env var entirely so a shipped release can never be talked into faking a
    // setup failure for a healthy user.
    #[test]
    fn fake_overrides_are_inert_unless_this_is_an_rc_build() {
        let is_rc = env!("CARGO_PKG_VERSION").contains("-rc");
        std::env::set_var("HEADROOM_FAKE_OVERRIDE_PROBE", "no_traffic");
        let resolved = fake_override("HEADROOM_FAKE_OVERRIDE_PROBE");
        std::env::remove_var("HEADROOM_FAKE_OVERRIDE_PROBE");

        if is_rc {
            assert_eq!(resolved.as_deref(), Some("no_traffic"));
        } else {
            assert_eq!(resolved, None, "stable builds must ignore HEADROOM_FAKE_*");
        }
    }

    #[test]
    fn fake_override_treats_blank_and_unset_alike() {
        std::env::set_var("HEADROOM_FAKE_OVERRIDE_BLANK", "   ");
        let blank = fake_override("HEADROOM_FAKE_OVERRIDE_BLANK");
        std::env::remove_var("HEADROOM_FAKE_OVERRIDE_BLANK");

        assert_eq!(blank, None);
        assert_eq!(fake_override("HEADROOM_FAKE_OVERRIDE_NEVER_SET"), None);
    }

    #[test]
    fn onboarding_recovery_copy_blames_a_stale_environment_only_when_something_is_connected() {
        let (title, body) = onboarding_recovery_copy(true);
        assert_eq!(title, "Headroom isn't seeing any traffic");
        assert!(
            body.contains("Restart your terminal or editor"),
            "got: {body}"
        );

        // Telling a user with no connector to restart their terminal sends them
        // after a problem they don't have: there is no routing to pick up.
        let (title, body) = onboarding_recovery_copy(false);
        assert_eq!(title, "Headroom isn't connected to anything yet");
        assert!(!body.contains("Restart"), "got: {body}");
        assert!(body.contains("turn on the connector"), "got: {body}");
    }

    #[test]
    fn app_update_notification_body_mentions_the_target_version() {
        assert_eq!(
            app_update_notification_body("0.3.0"),
            "Headroom 0.3.0 is ready to install. Open Headroom to review the release and install it."
        );
        assert_eq!(
            app_update_notification_body("   "),
            "A Headroom update is ready to install. Open Headroom to review the release and install it."
        );
    }

    #[test]
    fn macos_notifications_do_not_wait_for_clicks() {
        // Normalize CRLF: Windows checkouts embed \r\n, breaking the \n-joined
        // patterns below.
        let source = include_str!("lib.rs").replace('\r', "");
        let source = source.as_str();
        let start = source
            .find("#[cfg(target_os = \"macos\")]\nfn show_notification_impl")
            .expect("macOS notification implementation exists");
        let rest = &source[start..];
        let end = rest
            .find("\n#[cfg(not(target_os = \"macos\"))]")
            .expect("non-macOS notification implementation follows macOS implementation");
        let macos_impl = &rest[..end];

        assert!(
            macos_impl.contains(".asynchronous(true)"),
            "macOS notifications must be fire-and-forget so they do not spin a click-wait run loop"
        );
        assert!(
            !macos_impl.contains(".wait_for_click("),
            "wait_for_click caused Headroom to hold a full CPU core while notifications were pending"
        );
    }

    #[test]
    fn store_checked_update_tracks_available_update_metadata() {
        let pending = Mutex::new(None);
        let metadata = sample_available_update("0.3.0");

        let result = store_checked_update(
            Ok(Some(FakePendingUpdate {
                metadata: metadata.clone(),
                install_result: Ok(()),
            })),
            &pending,
        )
        .expect("available update");

        assert_eq!(result, Some(metadata.clone()));
        let stored = pending.lock();
        assert_eq!(
            stored.as_ref().expect("pending update").metadata(),
            metadata
        );
    }

    #[test]
    fn store_checked_update_clears_pending_update_when_feed_is_current() {
        let pending = Mutex::new(Some(FakePendingUpdate {
            metadata: sample_available_update("0.3.0"),
            install_result: Ok(()),
        }));

        let result =
            store_checked_update::<FakePendingUpdate>(Ok(None), &pending).expect("no update");

        assert_eq!(result, None);
        assert!(pending.lock().is_none());
    }

    #[test]
    fn store_checked_update_preserves_pending_update_when_check_errors() {
        let existing = sample_available_update("0.3.0");
        let pending = Mutex::new(Some(FakePendingUpdate {
            metadata: existing.clone(),
            install_result: Ok(()),
        }));

        let error =
            store_checked_update::<FakePendingUpdate>(Err("feed unavailable".into()), &pending)
                .expect_err("check failure should bubble up");

        assert_eq!(error, "feed unavailable");
        let stored = pending.lock();
        assert_eq!(
            stored.as_ref().expect("pending update").metadata(),
            existing
        );
    }

    #[test]
    fn classify_update_check_treats_a_missing_platform_as_no_update() {
        // What a Windows install saw while a release was mid-flight, before
        // the workflows moved the manifest publish to the end.
        let missing =
            classify_update_check::<()>(Err(tauri_plugin_updater::Error::TargetsNotFound(vec![
                "windows-x86_64-nsis".into(),
                "windows-x86_64".into(),
            ])))
            .expect("a platform-less manifest is not an error");
        assert!(missing.is_none());

        assert!(
            classify_update_check::<()>(Err(tauri_plugin_updater::Error::TargetNotFound(
                "linux-x86_64".into()
            )))
            .expect("single-target miss is not an error")
            .is_none()
        );

        let real = classify_update_check::<()>(Err(tauri_plugin_updater::Error::Network(
            "connection reset".into(),
        )))
        .expect_err("other failures still bubble up");
        assert!(real.contains("connection reset"), "{real}");
    }

    /// The privilege prompt a .deb install raises steals focus, and the blur
    /// handler hides the window 150ms later. The flag is what keeps "Restart
    /// now" on screen instead of behind an unexplained tray click.
    #[test]
    fn install_pending_update_holds_the_window_open_while_it_runs() {
        struct FlagObservingUpdate(Arc<Mutex<Option<bool>>>);

        impl InstallableAppUpdate for FlagObservingUpdate {
            fn metadata(&self) -> AvailableAppUpdate {
                unreachable!("metadata is not read on the install path")
            }

            fn install(self, _progress: AppUpdateProgressEmitter) -> InstallPendingUpdateFuture {
                Box::pin(async move {
                    *self.0.lock() =
                        Some(super::INSTALLING_UPDATE.load(std::sync::atomic::Ordering::Acquire));
                    Ok(())
                })
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let pending = Mutex::new(Some(FlagObservingUpdate(Arc::clone(&seen))));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        runtime
            .block_on(install_pending_update(
                &pending,
                noop_app_update_progress_emitter(),
            ))
            .expect("install");

        assert_eq!(
            *seen.lock(),
            Some(true),
            "window would hide behind the privilege prompt mid-install"
        );
        assert!(
            !super::INSTALLING_UPDATE.load(std::sync::atomic::Ordering::Acquire),
            "flag outlived the install, so the window can never auto-hide again"
        );
    }

    #[test]
    fn install_pending_update_requires_a_checked_update() {
        let pending = Mutex::new(None::<FakePendingUpdate>);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let error = runtime
            .block_on(install_pending_update(
                &pending,
                noop_app_update_progress_emitter(),
            ))
            .expect_err("missing update should fail");

        assert_eq!(error, "No downloaded update is ready to install.");
    }

    #[test]
    fn install_pending_update_runs_the_installer_and_clears_the_slot() {
        let pending = Mutex::new(Some(FakePendingUpdate {
            metadata: sample_available_update("0.3.0"),
            install_result: Ok(()),
        }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        runtime
            .block_on(install_pending_update(
                &pending,
                noop_app_update_progress_emitter(),
            ))
            .expect("install succeeds");

        assert!(pending.lock().is_none());
    }

    #[test]
    fn install_pending_update_forwards_progress_to_emitter() {
        struct ProgressEmittingFake {
            metadata: AvailableAppUpdate,
            events: Vec<AppUpdateProgress>,
        }

        impl InstallableAppUpdate for ProgressEmittingFake {
            fn metadata(&self) -> AvailableAppUpdate {
                self.metadata.clone()
            }

            fn install(self, progress: AppUpdateProgressEmitter) -> InstallPendingUpdateFuture {
                Box::pin(async move {
                    for event in self.events {
                        progress(event);
                    }
                    Ok(())
                })
            }
        }

        let pending = Mutex::new(Some(ProgressEmittingFake {
            metadata: sample_available_update("0.3.0"),
            events: vec![
                AppUpdateProgress::Downloading {
                    downloaded: 1_024,
                    total: Some(2_048),
                },
                AppUpdateProgress::Downloading {
                    downloaded: 2_048,
                    total: Some(2_048),
                },
                AppUpdateProgress::Installing,
            ],
        }));
        let captured: Arc<Mutex<Vec<AppUpdateProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_emit = Arc::clone(&captured);
        let emitter: AppUpdateProgressEmitter = Arc::new(move |event| {
            captured_for_emit.lock().push(event);
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        runtime
            .block_on(install_pending_update(&pending, emitter))
            .expect("install succeeds");

        let events = captured.lock().clone();
        assert_eq!(
            events,
            vec![
                AppUpdateProgress::Downloading {
                    downloaded: 1_024,
                    total: Some(2_048),
                },
                AppUpdateProgress::Downloading {
                    downloaded: 2_048,
                    total: Some(2_048),
                },
                AppUpdateProgress::Installing,
            ]
        );
    }

    #[test]
    fn app_update_progress_serializes_with_phase_tag() {
        let downloading = serde_json::to_value(&AppUpdateProgress::Downloading {
            downloaded: 1024,
            total: Some(4096),
        })
        .expect("serialize downloading");
        assert_eq!(
            downloading,
            serde_json::json!({
                "phase": "downloading",
                "downloaded": 1024,
                "total": 4096,
            })
        );

        let installing =
            serde_json::to_value(&AppUpdateProgress::Installing).expect("serialize installing");
        assert_eq!(installing, serde_json::json!({ "phase": "installing" }));

        let unknown_total = serde_json::to_value(&AppUpdateProgress::Downloading {
            downloaded: 512,
            total: None,
        })
        .expect("serialize downloading with unknown total");
        assert_eq!(
            unknown_total,
            serde_json::json!({
                "phase": "downloading",
                "downloaded": 512,
                "total": null,
            })
        );
    }

    #[test]
    fn install_pending_update_returns_install_failures_after_taking_the_slot() {
        let pending = Mutex::new(Some(FakePendingUpdate {
            metadata: sample_available_update("0.3.0"),
            install_result: Err("signature mismatch".into()),
        }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let error = runtime
            .block_on(install_pending_update(
                &pending,
                noop_app_update_progress_emitter(),
            ))
            .expect_err("install failure");

        assert_eq!(error, "signature mismatch");
        assert!(pending.lock().is_none());
    }

    #[test]
    fn tray_window_position_clamps_to_right_monitor_edge() {
        let target = compute_tray_window_position(
            PhysicalRect {
                x: 1430,
                y: 0,
                width: 24,
                height: 24,
            },
            PhysicalSize::new(760, 560),
            Some(MonitorBounds {
                x: 0,
                y: 0,
                width: 1440,
                height: 900,
            }),
        );

        assert_eq!(target.x, 680);
        assert_eq!(target.y, 34);
    }

    #[test]
    fn panel_corner_position_hugs_the_work_area_top_right() {
        // Work area starts below a 28px top panel on a second monitor.
        let target = compute_panel_corner_position(
            MonitorBounds {
                x: 1440,
                y: 28,
                width: 1440,
                height: 872,
            },
            PhysicalSize::new(760, 560),
        );

        assert_eq!(target.x, 2110);
        assert_eq!(target.y, 38);
    }

    #[test]
    fn panel_corner_position_stays_on_screen_when_window_is_wider() {
        let target = compute_panel_corner_position(
            MonitorBounds {
                x: 0,
                y: 0,
                width: 600,
                height: 400,
            },
            PhysicalSize::new(760, 560),
        );

        assert_eq!(target.x, 0);
        assert_eq!(target.y, 10);
    }

    #[test]
    fn tray_window_position_moves_above_when_bottom_would_overflow() {
        let target = compute_tray_window_position(
            PhysicalRect {
                x: 500,
                y: 730,
                width: 24,
                height: 24,
            },
            PhysicalSize::new(760, 560),
            Some(MonitorBounds {
                x: 0,
                y: 0,
                width: 1440,
                height: 900,
            }),
        );

        assert_eq!(target.x, 132);
        assert_eq!(target.y, 160);
    }

    #[test]
    fn logical_tray_rects_are_converted_with_scale_factor() {
        let rect = Rect {
            position: Position::Logical(LogicalPosition::new(100.0, 20.0)),
            size: Size::Logical(LogicalSize::new(12.0, 12.0)),
        };

        let physical = physical_rect_from_rect(rect, 2.0);

        assert_eq!(
            physical,
            PhysicalRect {
                x: 200,
                y: 40,
                width: 24,
                height: 24,
            }
        );
    }

    #[test]
    fn token_milestone_kind_labels_first_and_repeating_thresholds() {
        assert_eq!(lifetime_token_milestone_kind(1_000_000), "first_1m");
        assert_eq!(lifetime_token_milestone_kind(5_000_000), "first_5m");
        assert_eq!(lifetime_token_milestone_kind(10_000_000), "first_10m");
        assert_eq!(lifetime_token_milestone_kind(20_000_000), "repeating_10m");
    }

    #[test]
    fn first_savings_body_leads_with_tokens_below_the_quotable_threshold() {
        for usd in [0.004, 0.015, 0.32] {
            let small = first_savings_body(usd, 2_431);
            assert!(small.contains("2,431 tokens"), "{small}");
            assert!(!small.contains('$'), "{small}");
            assert!(!small.to_lowercase().contains("cent"), "{small}");
        }

        let real_money = first_savings_body(1.5, 124_500);
        assert!(
            real_money.starts_with("$1.50 saved across 124k tokens"),
            "{real_money}"
        );
    }

    #[test]
    fn format_token_count_scales_by_magnitude() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(950), "950");
        assert_eq!(format_token_count(1_240), "1,240");
        assert_eq!(format_token_count(99_999), "99,999");
        assert_eq!(format_token_count(124_500), "124k");
        assert_eq!(format_token_count(1_240_000), "1.2M");
    }

    fn learn_prereq(
        claude: bool,
        codex_cli: bool,
        codex_logged_in: bool,
    ) -> HeadroomLearnPrereqStatus {
        HeadroomLearnPrereqStatus {
            claude_cli_available: claude,
            claude_cli_path: claude.then(|| "/usr/bin/claude".to_string()),
            codex_cli_available: codex_cli,
            codex_cli_path: codex_cli.then(|| "/usr/bin/codex".to_string()),
            codex_logged_in,
        }
    }

    /// The three tail sites used to slice on a raw byte offset, which panics
    /// when the cut lands mid-codepoint. These inputs carry non-ASCII routinely
    /// (Windows paths with accented usernames, Python's startup banner), so the
    /// crash reporter could take the process down while building its report.
    #[test]
    fn sentry_tail_never_splits_a_codepoint() {
        // A 3-byte codepoint repeated: every offset that is not a multiple of 3
        // is mid-character, so most caps land on one.
        let text = "\u{2500}".repeat(8_000);
        assert_eq!(text.len(), 24_000);
        for cap in [11_999, 12_000, 12_001, 1, 2, 23_999] {
            let tail = tail_bytes_for_sentry(&text, cap);
            assert!(tail.len() <= cap + 64, "cap {cap} respected (plus prefix)");
            assert!(tail.contains("[truncated "), "cap {cap} announces the cut");
            assert!(
                tail.trim_start_matches(|c| c != '\u{2500}').is_empty()
                    || tail.ends_with('\u{2500}'),
                "cap {cap} produced valid text"
            );
        }
        // Under the cap the text is returned whole, with no prefix.
        assert_eq!(tail_bytes_for_sentry("short", 12_000), "short");
        assert_eq!(tail_bytes_for_sentry("", 12_000), "");
    }

    /// RUST-9Y arrived with 12KB of connection-pool debug lines and nothing
    /// about the install it was supposed to be reporting on.
    #[test]
    fn abandoned_bootstrap_tail_drops_connection_pool_noise() {
        let log = "2026-08-27 18:06:32.282 DEBUG reqwest::connect: starting new connection: http://localhost:6767/\n                   2026-08-27 18:06:33.000 INFO installing pip into the managed venv\n                   2026-08-27 18:06:33.100 DEBUG reqwest::connect: starting new connection: http://127.0.0.1:6767/\n                   2026-08-27 18:06:34.000 WARN pip install attempt 1 failed";
        let stripped = strip_connection_noise(log);
        assert!(!stripped.contains("starting new connection"));
        assert!(stripped.contains("installing pip into the managed venv"));
        assert!(stripped.contains("pip install attempt 1 failed"));
        // A connection *failure* is signal, not noise, and stays.
        let failure = "DEBUG reqwest::connect: connection refused for http://localhost:6767/";
        assert_eq!(strip_connection_noise(failure), failure);
    }

    /// RUST-A2 and RUST-9Z are one failure class that opened two issues because
    /// the fingerprint carried each machine's resolved CLI path.
    #[test]
    fn learn_failure_signatures_group_across_machines() {
        let windows = normalize_learn_failure_signature(
            "LLM analysis failed: `~\\AppData\\Roaming\\npm\\claude.CMD -p --output-format stream-json --verbose` failed (exit 1):",
        );
        let bare = normalize_learn_failure_signature(
            "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):",
        );
        let posix = normalize_learn_failure_signature(
            "LLM analysis failed: `/Users/x/.nvm/versions/node/v22/bin/claude -p --output-format stream-json --verbose` failed (exit 1):",
        );
        assert_eq!(windows, bare);
        assert_eq!(posix, bare);
        assert_eq!(
            bare,
            "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):"
        );

        // The exit code still discriminates: an error exit and a Windows crash
        // (0xC0000409) are different failures and must stay different issues.
        assert_ne!(
            bare,
            normalize_learn_failure_signature(
                "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 3221226505):",
            )
        );
        // So does the agent.
        assert_ne!(
            bare,
            normalize_learn_failure_signature(
                "LLM analysis failed: `codex -p --output-format stream-json --verbose` failed (exit 1):",
            )
        );
    }

    /// Anything not command-shaped passes through untouched -- the normalizer
    /// must never eat a reason it does not recognize.
    #[test]
    fn learn_failure_signature_normalization_is_a_no_op_off_the_command_shape() {
        for raw in [
            "no reason",
            "Credit balance is too low",
            "LLM analysis failed: usage limit reached",
            "unterminated `backtick",
        ] {
            assert_eq!(normalize_learn_failure_signature(raw), raw);
        }
    }

    #[test]
    fn check_headroom_learn_prereqs_passes_when_cli_available() {
        let prereq = learn_prereq(true, false, false);
        assert!(check_headroom_learn_prereqs(LearnAgent::Claude, None, &prereq).is_ok());
    }

    #[test]
    fn check_headroom_learn_prereqs_returns_install_message_when_cli_missing() {
        let prereq = learn_prereq(false, false, false);
        let err = check_headroom_learn_prereqs(LearnAgent::Claude, None, &prereq).unwrap_err();
        assert!(
            err.contains("Install the Claude Code CLI"),
            "expected install hint, got: {err}"
        );
    }

    #[test]
    fn check_headroom_learn_prereqs_prefers_platform_message_over_cli_check() {
        let prereq = learn_prereq(false, false, false);
        let err =
            check_headroom_learn_prereqs(LearnAgent::Claude, Some("Linux not supported"), &prereq)
                .unwrap_err();
        assert_eq!(err, "Linux not supported");
    }

    #[test]
    fn check_headroom_learn_prereqs_codex_passes_when_cli_present_and_logged_in() {
        let prereq = learn_prereq(false, true, true);
        assert!(check_headroom_learn_prereqs(LearnAgent::Codex, None, &prereq).is_ok());
    }

    #[test]
    fn check_headroom_learn_prereqs_codex_requires_cli_install() {
        let prereq = learn_prereq(true, false, false);
        let err = check_headroom_learn_prereqs(LearnAgent::Codex, None, &prereq).unwrap_err();
        assert!(
            err.contains("Install the Codex CLI"),
            "expected codex install hint, got: {err}"
        );
    }

    #[test]
    fn check_headroom_learn_prereqs_codex_requires_login_when_cli_present() {
        let prereq = learn_prereq(false, true, false);
        let err = check_headroom_learn_prereqs(LearnAgent::Codex, None, &prereq).unwrap_err();
        assert!(
            err.contains("Sign in to the Codex CLI"),
            "expected codex sign-in hint, got: {err}"
        );
    }

    #[test]
    fn fetch_transformations_feed_decodes_proxy_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = serde_json::json!({
                "log_full_messages": true,
                "transformations": [{
                    "request_id": "req-1",
                    "timestamp": "2026-04-21T10:00:00Z",
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-6",
                    "input_tokens_original": 1000,
                    "input_tokens_optimized": 250,
                    "tokens_saved": 750,
                    "savings_percent": 75.0,
                    "transforms_applied": ["interceptor:ast-grep"]
                }]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let result =
            fetch_transformations_feed_from(&format!("http://127.0.0.1:{port}"), 50).unwrap();
        server.join().unwrap();

        assert!(result.proxy_reachable);
        assert!(result.log_full_messages);
        assert_eq!(result.transformations.len(), 1);
        let event = &result.transformations[0];
        assert_eq!(event.request_id.as_deref(), Some("req-1"));
        assert_eq!(event.provider.as_deref(), Some("anthropic"));
        assert_eq!(event.tokens_saved, Some(750));
        assert_eq!(event.transforms_applied, vec!["interceptor:ast-grep"]);
    }

    #[test]
    fn fetch_transformations_feed_returns_error_on_non_2xx_status() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response =
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
        });

        let err =
            fetch_transformations_feed_from(&format!("http://127.0.0.1:{port}"), 50).unwrap_err();
        server.join().unwrap();
        assert!(
            err.contains("503"),
            "expected status code in error, got: {err}"
        );
    }

    #[test]
    fn count_memories_created_today_only_counts_today_entries() {
        use chrono::TimeZone;
        let json = r#"[
            {"id":"a","created_at":"2026-04-22T10:00:00"},
            {"id":"b","created_at":"2026-04-22T23:59:59"},
            {"id":"c","created_at":"2026-04-21T23:00:00"},
            {"id":"d","created_at":null},
            {"id":"e"}
        ]"#;
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
        assert_eq!(count_memories_created_today(json, now).unwrap(), 2);
    }

    #[test]
    fn count_memories_created_today_accepts_rfc3339_with_tz() {
        use chrono::TimeZone;
        let json = r#"[
            {"id":"a","created_at":"2026-04-22T10:00:00Z"},
            {"id":"b","created_at":"2026-04-22T02:00:00-09:00"}
        ]"#;
        // 2026-04-22T02:00:00-09:00 == 2026-04-22T11:00:00Z, both land on today.
        let now = chrono::Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
        assert_eq!(count_memories_created_today(json, now).unwrap(), 2);
    }

    #[test]
    fn count_memories_created_today_handles_empty_and_errors() {
        let now = chrono::Utc::now();
        assert_eq!(count_memories_created_today("[]", now).unwrap(), 0);
        assert!(count_memories_created_today("not json", now).is_err());
    }

    #[test]
    fn pattern_matches_project_requires_path_boundary() {
        assert!(pattern_matches_project(
            "File `/x/a/b/foo.py` missing",
            &[],
            "/x/a/b",
        ));
        // /x/ab must not match when root is /x/a
        assert!(!pattern_matches_project(
            "File `/x/ab/foo.py` missing",
            &[],
            "/x/a",
        ));
    }

    #[test]
    fn pattern_matches_project_via_entity_refs() {
        assert!(pattern_matches_project(
            "Command failed",
            &["/x/a/tool.py".to_string()],
            "/x/a",
        ));
    }

    #[test]
    fn parse_live_learnings_filters_and_parses() {
        let json = serde_json::to_string(&json!([
            {
                "id": "1",
                "content": "Pattern mentioning /x/a/foo.py",
                "created_at": "2026-04-22T10:00:00Z",
                "importance": 0.8,
                "metadata": {
                    "source": "traffic_learner",
                    "category": "environment",
                    "evidence_count": 3
                },
                "entity_refs": []
            },
            {
                "id": "2",
                "content": "Unrelated project /y/z",
                "metadata": {"source": "traffic_learner", "category": "environment"},
                "entity_refs": []
            },
            {
                "id": "3",
                "content": "/x/a/bar.py",
                "metadata": {"source": "other"},
                "entity_refs": []
            }
        ]))
        .unwrap();

        let learnings = parse_live_learnings(&json, "/x/a").unwrap();
        assert_eq!(learnings.len(), 1);
        assert_eq!(learnings[0].id, "1");
        assert_eq!(learnings[0].category, "environment");
        assert_eq!(learnings[0].evidence_count, 3);
        assert_eq!(learnings[0].importance, 0.8);
    }

    #[test]
    fn aggregate_live_learnings_returns_entry_per_path_including_empty() {
        let json = serde_json::to_string(&json!([
            {
                "id": "a1",
                "content": "Pattern in /x/a/foo.py",
                "metadata": {"source": "traffic_learner", "category": "environment"},
                "entity_refs": []
            },
            {
                "id": "b1",
                "content": "Pattern in /x/b/bar.py",
                "metadata": {"source": "traffic_learner", "category": "environment"},
                "entity_refs": []
            }
        ]))
        .unwrap();

        let paths = vec![
            "/x/a".to_string(),
            "/x/b".to_string(),
            "/x/empty".to_string(),
        ];
        let map = aggregate_live_learnings(&json, &paths).unwrap();

        assert_eq!(map.len(), 3, "one entry per requested path");
        assert_eq!(map.get("/x/a").unwrap().len(), 1);
        assert_eq!(map.get("/x/a").unwrap()[0].id, "a1");
        assert_eq!(map.get("/x/b").unwrap().len(), 1);
        assert_eq!(map.get("/x/b").unwrap()[0].id, "b1");
        assert!(
            map.get("/x/empty").unwrap().is_empty(),
            "paths with no matches get an empty Vec, not a missing key",
        );
    }

    #[test]
    fn aggregate_live_learnings_bubbles_json_errors() {
        let paths = vec!["/x/a".to_string()];
        let err = aggregate_live_learnings("not json", &paths).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn empty_live_learnings_for_projects_fills_each_path_with_empty_vec() {
        let paths = vec!["/x/a".to_string(), "/x/b".to_string()];
        let map = empty_live_learnings_for_projects(&paths);
        assert_eq!(map.len(), 2);
        assert!(map.get("/x/a").unwrap().is_empty());
        assert!(map.get("/x/b").unwrap().is_empty());
    }

    #[test]
    fn fetch_transformations_feed_returns_error_when_proxy_unreachable() {
        // Bind and immediately drop a listener so we know the port is free.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let err =
            fetch_transformations_feed_from(&format!("http://127.0.0.1:{port}"), 50).unwrap_err();
        assert!(!err.is_empty(), "expected a non-empty error message");
    }

    // ── classify_bootstrap_failure ───────────────────────────────────────────

    fn make_command_failure(stderr: &str) -> crate::tool_manager::CommandFailure {
        crate::tool_manager::CommandFailure {
            program: "/usr/bin/pip".into(),
            args: vec!["install".into()],
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code: Some(1),
            signal: None,
        }
    }

    #[test]
    fn classify_bootstrap_failure_flags_unresolvable_pin_as_unsupported_pin() {
        // The exact shape from RUST-1G/RUST-6S: our lock pinned a version with
        // no Intel-macOS wheel, so every retry failed identically while the
        // user was told to check their internet connection.
        let err: anyhow::Error = make_command_failure(
            "ERROR: Could not find a version that satisfies the requirement \
             onnxruntime==1.27.0 (from versions: 1.23.0, 1.23.1, 1.23.2)\n\
             ERROR: No matching distribution found for onnxruntime==1.27.0",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::UnsupportedPin
        ));
    }

    #[test]
    fn classify_bootstrap_failure_flags_denied_writes_as_permission() {
        let err: anyhow::Error = make_command_failure(
            "ERROR: Could not install packages due to an OSError: \
             [Errno 13] Permission denied: '/Users/x/Library/Application Support/\
             Headroom/headroom/runtime/venv/lib/python3.12/site-packages/foo'\n\
             Check the permissions.",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::Permission
        ));
    }

    #[test]
    fn classify_bootstrap_failure_flags_a_foreign_openssl_as_ssl_library_conflict() {
        // Verbatim from the RUST-8K event (host GIDI, 0.8.7, Windows 10.0.26200):
        // the abort line, then ensurepip's own traceback. This landed in `Other`,
        // whose message sends the user back to Try again -- 25 times.
        let err: anyhow::Error = make_command_failure(
            "OPENSSL_Uplink(00007FF926407C58,08): no OPENSSL_Applink\r\n\
             Traceback (most recent call last):\r\n\
             \x20 File \"<frozen runpy>\", line 198, in _run_module_as_main\r\n\
             \x20 File \"ensurepip\\__init__.py\", line 200, in _bootstrap\r\n\
             \x20   return _run_pip([*args, *_PACKAGE_NAMES], additional_paths)",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::SslLibraryConflict
        ));
    }

    #[test]
    fn ssl_library_conflict_does_not_tell_the_user_to_retry() {
        // The whole point of the kind: `Other` says "click Try again", which is
        // what kept this host looping. Guard the property, not the wording.
        let message = user_message_for(BootstrapFailureKind::SslLibraryConflict);
        assert!(
            message.contains("keep hitting it"),
            "message must say retrying will not help: {message}"
        );
        assert!(
            message.contains("libcrypto-3-x64.dll"),
            "message must name the file the user has to find: {message}"
        );
    }

    #[test]
    fn conflicting_openssl_dirs_names_every_path_dir_holding_one() {
        let root = tempfile::tempdir().expect("tempdir");
        let guilty = root.path().join("some-other-app");
        let innocent = root.path().join("plain");
        std::fs::create_dir_all(&guilty).expect("mkdir");
        std::fs::create_dir_all(&innocent).expect("mkdir");
        std::fs::write(guilty.join("libcrypto-3-x64.dll"), b"x").expect("write");

        let path_var = std::env::join_paths([&innocent, &guilty])
            .expect("join_paths")
            .into_string()
            .expect("utf8");
        let hits = conflicting_openssl_dirs(&path_var);

        assert_eq!(hits, vec![guilty.display().to_string()]);
        // An empty result is meaningful too -- it says the library was injected
        // rather than found on PATH -- so it must not be a false negative.
        assert!(conflicting_openssl_dirs(&innocent.display().to_string()).is_empty());
    }

    #[test]
    fn classify_bootstrap_failure_flags_app_control_as_app_control_blocked() {
        // RUST-8K, third cause, verbatim: Smart App Control / WDAC refused
        // the venv python. The spawn dies with an io::Error (no
        // CommandFailure), so the chain is the haystack.
        let err =
            anyhow::anyhow!("An Application Control policy has blocked this file. (os error 4551)")
                .context("starting python.exe -m venv --without-pip")
                .context("creating Headroom-managed virtualenv");
        assert_eq!(
            classify_bootstrap_failure(&err).as_str(),
            "app_control_blocked"
        );
        // Localized Windows prose keeps only the numeric code.
        let localized = anyhow::anyhow!("정책에 의해 차단되었습니다. (os error 4551)");
        assert_eq!(
            classify_bootstrap_failure(&localized).as_str(),
            "app_control_blocked"
        );
    }

    #[test]
    fn classify_bootstrap_failure_flags_a_source_build_as_source_build() {
        // Unreachable while every install passes `--only-binary=:all:`; if it
        // fires, a wheel we promised is missing for this machine.
        let err: anyhow::Error = make_command_failure(
            "  error: subprocess-exited-with-error\n\
             error: command '/usr/bin/clang' failed with exit code 1\n\
             ERROR: Failed building wheel for hnswlib",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::SourceBuild
        ));
    }

    #[test]
    fn permission_and_source_build_win_over_the_network_heuristic() {
        // Same trap as `unsupported_pin_wins_over_the_network_heuristic`: pip
        // names every index it consulted before it names the real failure.
        for (stderr, expected) in [
            (
                "WARNING: Retrying after connection timed out\n\
                 ERROR: Could not install packages due to an OSError: \
                 [Errno 13] Permission denied",
                "permission",
            ),
            (
                "WARNING: Retrying after connection timed out\n\
                 ERROR: Failed building wheel for hnswlib",
                "build",
            ),
        ] {
            let err: anyhow::Error = make_command_failure(stderr).into();
            assert_eq!(classify_bootstrap_failure(&err).as_str(), expected);
        }
    }

    #[test]
    fn no_failure_message_ever_asks_the_user_to_install_a_compiler() {
        // Needing a toolchain to install a desktop app is our bug, never the
        // user's homework -- `PIP_ONLY_BINARY` exists so it cannot be theirs.
        for kind in [
            BootstrapFailureKind::SslInterception,
            BootstrapFailureKind::NoUsableTempDir,
            BootstrapFailureKind::NetworkDownload,
            BootstrapFailureKind::UnsupportedPin,
            BootstrapFailureKind::Permission,
            BootstrapFailureKind::AppControlBlocked,
            BootstrapFailureKind::SourceBuild,
            BootstrapFailureKind::Other,
        ] {
            let msg = user_message_for(kind).to_ascii_lowercase();
            for banned in ["xcode", "command line tools", "compiler", "cargo", "rustup"] {
                assert!(
                    !msg.contains(banned),
                    "{} message tells the user about {banned}: {msg}",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn only_retryable_failures_mention_the_network() {
        // The bug behind RUST-1G was not the bad pin, it was that every cause
        // printed "check your internet connection" over a button that could
        // never work. Only NetworkDownload is actually retryable.
        for kind in [
            BootstrapFailureKind::UnsupportedPin,
            BootstrapFailureKind::Permission,
            BootstrapFailureKind::AppControlBlocked,
            BootstrapFailureKind::SourceBuild,
        ] {
            let msg = user_message_for(kind).to_ascii_lowercase();
            assert!(
                !msg.contains("internet connection"),
                "{} blames the network: {msg}",
                kind.as_str()
            );
        }
        assert!(user_message_for(BootstrapFailureKind::NetworkDownload)
            .to_ascii_lowercase()
            .contains("internet connection"));
    }

    #[test]
    fn unsupported_pin_wins_over_the_network_heuristic() {
        // pip echoes every index it consulted before reporting the resolution
        // failure, so a timeout word in that preamble must not steal the
        // classification and send the user to their network settings.
        let err: anyhow::Error = make_command_failure(
            "WARNING: Retrying after connection timed out\n\
             ERROR: No matching distribution found for onnxruntime==1.27.0",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::UnsupportedPin
        ));
    }

    #[test]
    fn total_index_fetch_failure_is_network_not_unsupported_pin() {
        // Verbatim shape from RUST-90/91 (one Intel mac behind TLS-breaking
        // middleware): pip could not fetch ANY index URL, so resolution said
        // "(from versions: none)" for a pin that exists everywhere. The user
        // needs their network fixed; the updater cannot help.
        let err: anyhow::Error = make_command_failure(
            "Could not fetch URL https://pypi.org/simple/aiohappyeyeballs/: \
             There was a problem confirming the ssl certificate: \
             HTTPSConnectionPool(host='pypi.org', port=443): Max retries \
             exceeded with url: /simple/aiohappyeyeballs/ (Caused by \
             SSLError(SSLEOFError(8, '[SSL: UNEXPECTED_EOF_WHILE_READING]'))) - skipping\n\
             ERROR: Could not find a version that satisfies the requirement \
             aiohappyeyeballs==2.6.2 (from versions: none)\n\
             ERROR: No matching distribution found for aiohappyeyeballs==2.6.2",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::NetworkDownload
        ));
    }

    #[test]
    fn unsupported_pin_message_sends_the_user_to_the_updater_not_the_network() {
        // The whole point: retrying is futile, so the copy must not imply it.
        let msg = user_message_for(BootstrapFailureKind::UnsupportedPin);
        assert!(msg.contains("Check for updates"));
        assert!(!msg.contains("internet connection"));
    }

    #[test]
    fn classify_bootstrap_failure_flags_certificate_verify_failed_as_ssl_interception() {
        let err: anyhow::Error = make_command_failure(
            "ssl.SSLError: [SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::SslInterception
        ));
    }

    #[test]
    fn classify_bootstrap_failure_flags_self_signed_with_hyphen_as_ssl_interception() {
        let err: anyhow::Error = make_command_failure(
            "Could not fetch URL: self-signed certificate in certificate chain",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::SslInterception
        ));
    }

    #[test]
    fn classify_bootstrap_failure_flags_self_signed_without_hyphen_as_ssl_interception() {
        let err: anyhow::Error = make_command_failure(
            "Could not fetch URL: self signed certificate in certificate chain",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::SslInterception
        ));
    }

    #[test]
    fn classify_bootstrap_failure_flags_no_usable_temporary_directory() {
        let err: anyhow::Error = make_command_failure(
            "FileNotFoundError: [Errno 2] No usable temporary directory found in \
             ['/var/folders/lp/.../T/', '/tmp', '/var/tmp', '/usr/tmp', \
             '/Users/x/Library/Application Support/Headroom/headroom']",
        )
        .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::NoUsableTempDir
        ));
    }

    #[test]
    fn classify_bootstrap_failure_flags_pip_connection_reset_as_network() {
        let err: anyhow::Error =
            make_command_failure("ConnectionResetError: [Errno 54] Connection reset by peer")
                .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::NetworkDownload
        ));
    }

    #[test]
    fn classify_bootstrap_failure_returns_other_for_unrelated_command_errors() {
        let err: anyhow::Error =
            make_command_failure("ModuleNotFoundError: No module named 'headroom'").into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::Other
        ));
    }

    #[test]
    fn classify_bootstrap_failure_returns_other_for_unrecognized_non_command_chain() {
        let err = anyhow::anyhow!("something unexpected went wrong");
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::Other
        ));
    }

    // ── read_applied_patterns_for_project + delete_applied_pattern ───────────

    fn write_claude_md_with_headroom_block(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("CLAUDE.md");
        let content = "\
# Project notes

Some unrelated content.

<!-- headroom:learn:start -->
## Headroom Learned Patterns
*Auto-generated by `headroom learn`*

### First Section
- First bullet.
- Second bullet.

### Second Section
- Third bullet.
<!-- headroom:learn:end -->
";
        std::fs::write(&path, content).expect("write CLAUDE.md");
        path
    }

    #[test]
    fn read_applied_patterns_returns_empty_when_no_files_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        assert!(result.claude_md.is_empty(), "no CLAUDE.md → empty sections");
        // memory.md lives under ~/.claude — we don't override HOME here, so we
        // can't assert it's empty. The CLAUDE.md side covers the parsing path.
    }

    #[test]
    fn read_applied_patterns_parses_claude_md_headroom_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());

        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        let titles: Vec<&str> = result.claude_md.iter().map(|s| s.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| *t == "First Section"),
            "first section parsed, got titles: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| *t == "Second Section"),
            "second section parsed, got titles: {titles:?}"
        );
        let first = result
            .claude_md
            .iter()
            .find(|s| s.title == "First Section")
            .expect("first section");
        assert_eq!(first.bullets.len(), 2);
    }

    #[tokio::test]
    async fn read_applied_patterns_prefers_claude_local_md_over_legacy_claude_md() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());
        let local = tmp.path().join("CLAUDE.local.md");
        std::fs::write(
            &local,
            "<!-- headroom:learn:start -->\n\
             ## Headroom Learned Patterns\n\
             ### Local Section\n\
             - Local bullet.\n\
             <!-- headroom:learn:end -->\n",
        )
        .expect("write CLAUDE.local.md");

        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        let titles: Vec<&str> = result.claude_md.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Local Section"],
            "CLAUDE.local.md block wins over the legacy CLAUDE.md block"
        );

        // Deletes must target the same file the read came from.
        delete_applied_pattern(
            tmp.path().to_str().unwrap().to_string(),
            "claude".into(),
            "Local Section".into(),
            "Local bullet.".into(),
        )
        .await
        .expect("delete bullet from CLAUDE.local.md");
        let on_disk = std::fs::read_to_string(&local).unwrap();
        assert!(
            !on_disk.contains("Local bullet."),
            "bullet removed from CLAUDE.local.md, got:\n{on_disk}"
        );
    }

    #[tokio::test]
    async fn delete_applied_pattern_removes_one_bullet_and_keeps_section() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());

        delete_applied_pattern(
            tmp.path().to_str().unwrap().to_string(),
            "claude".into(),
            "First Section".into(),
            "First bullet.".into(),
        )
        .await
        .expect("delete bullet");

        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        let first = result
            .claude_md
            .iter()
            .find(|s| s.title == "First Section")
            .expect("First Section preserved when one of two bullets deleted");
        assert_eq!(first.bullets, vec!["Second bullet.".to_string()]);
        assert!(
            result.claude_md.iter().any(|s| s.title == "Second Section"),
            "other sections preserved"
        );
    }

    #[tokio::test]
    async fn delete_applied_pattern_drops_last_section_and_keeps_block_parseable() {
        // Regression: deleting the last bullet in the last section used to
        // truncate the block's trailing end marker, leaving the file
        // unparseable. After the fix, the block must still be reparseable
        // and the surviving section intact.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());

        delete_applied_pattern(
            tmp.path().to_str().unwrap().to_string(),
            "claude".into(),
            "Second Section".into(),
            "Third bullet.".into(),
        )
        .await
        .expect("delete bullet");

        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        let titles: Vec<&str> = result.claude_md.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["First Section"],
            "Second Section dropped, First Section preserved"
        );
        let first = result
            .claude_md
            .iter()
            .find(|s| s.title == "First Section")
            .expect("First Section");
        assert_eq!(
            first.bullets,
            vec!["First bullet.".to_string(), "Second bullet.".to_string()]
        );

        // The on-disk file should still contain the end marker so a future
        // read won't return an empty result.
        let on_disk = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(
            on_disk.contains("<!-- headroom:learn:end -->"),
            "end marker preserved on disk, got:\n{on_disk}"
        );
    }

    #[tokio::test]
    async fn delete_applied_pattern_rejects_unknown_file_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());

        let err = delete_applied_pattern(
            tmp.path().to_str().unwrap().to_string(),
            "garbage".into(),
            "First Section".into(),
            "First bullet.".into(),
        )
        .await
        .expect_err("unknown file_kind rejected");
        assert!(
            err.contains("Unknown file_kind"),
            "expected Unknown file_kind error, got: {err}"
        );
    }

    #[test]
    fn watchdog_should_be_up_requires_runtime_installed() {
        // Even if every other gate is "ready", a missing runtime means the
        // watchdog should not expect Python to be reachable yet.
        assert!(!watchdog_should_be_up(false, false, false, false, false));
    }

    #[test]
    fn watchdog_should_be_up_when_all_gates_clear() {
        // Installed, not paused, not booting, not upgrading, not bypassed —
        // this is the one input combination that must return true.
        assert!(watchdog_should_be_up(true, false, false, false, false));
    }

    #[test]
    fn watchdog_should_be_up_respects_user_pause() {
        assert!(!watchdog_should_be_up(true, true, false, false, false));
    }

    #[test]
    fn watchdog_should_be_up_skips_during_boot() {
        assert!(!watchdog_should_be_up(true, false, true, false, false));
    }

    #[test]
    fn watchdog_should_be_up_skips_during_runtime_upgrade() {
        assert!(!watchdog_should_be_up(true, false, false, true, false));
    }

    /// Critical regression guard. Removing the bypass clause from
    /// `watchdog_should_be_up` would silently turn the watchdog into a thrash
    /// loop the moment the pricing gate fires — it would keep restarting
    /// Python while the bypass forwarder is doing its job, eventually
    /// tripping the auto-pause path that strips Claude Code's env var.
    #[test]
    fn watchdog_should_be_up_skips_when_pricing_gate_bypassed() {
        assert!(!watchdog_should_be_up(true, false, false, false, true));
    }

    #[test]
    fn auto_resume_backoff_escalates_then_caps() {
        use std::time::Duration;
        // 30s -> 1m -> 2m for the first three attempts, then a 5m cap that holds
        // for all later attempts so a persistent outage retries indefinitely
        // without hammering restart.
        assert_eq!(auto_resume_backoff(0), Duration::from_secs(30));
        assert_eq!(auto_resume_backoff(1), Duration::from_secs(60));
        assert_eq!(auto_resume_backoff(2), Duration::from_secs(120));
        assert_eq!(auto_resume_backoff(3), Duration::from_secs(300));
        assert_eq!(auto_resume_backoff(50), Duration::from_secs(300));
    }

    /// Ordering guard for the give-up path. `capture_watchdog_give_up`
    /// re-probes the backend and sleeps ~4s to sample a CPU rate, so running it
    /// before the bypass flip holds every in-flight request on the unreachable
    /// backend branch for that whole window — purely to decide a Sentry level.
    /// Setting the flag tears nothing down, so it must come first.
    #[test]
    fn watchdog_give_up_flips_bypass_before_capturing_diagnostics() {
        let source = include_str!("lib.rs");
        let start = source
            .find("fn spawn_proxy_watchdog")
            .expect("watchdog implementation exists");
        let body = &source[start..];

        let bypass_flip = body
            .find(".store(true, std::sync::atomic::Ordering::Release)")
            .expect("give-up path flips proxy_bypass");
        let capture = body
            .find("capture_watchdog_give_up(")
            .expect("give-up path captures diagnostics");

        assert!(
            bypass_flip < capture,
            "proxy_bypass must be set before capture_watchdog_give_up, which sleeps ~4s"
        );
    }

    #[test]
    fn is_port_conflict_failure_matches_non_headroom_bail() {
        assert!(is_port_conflict_failure(
            "port 6768 is occupied by a non-headroom process (python3.1 pid 1073); ..."
        ));
    }

    #[test]
    fn is_port_conflict_failure_matches_already_running_message() {
        // Distinct from a foreign-process conflict: a stale headroom child
        // still bound to the port.
        assert!(is_port_conflict_failure(
            "spawn aborted: headroom proxy already running on port 6768"
        ));
    }

    #[test]
    fn is_port_conflict_failure_rejects_unrelated_errors() {
        // Generic startup failures must NOT route to the rate-limited port-
        // conflict fingerprint — they need the Error-level capture.
        assert!(!is_port_conflict_failure(
            "ModuleNotFoundError: No module named 'headroom'"
        ));
        assert!(!is_port_conflict_failure(
            "venv interpreter exited with status 1"
        ));
        assert!(!is_port_conflict_failure(""));
    }

    #[test]
    fn headroom_start_failure_category_collapses_the_argv_grab_bag() {
        // RUST-9F/AF/AH/AJ/AK: one condition, five issues, because the
        // un-fingerprinted message carried the program path and full argv.
        use super::headroom_start_failure_category as cat;

        // The exit status is the bug class and survives; the port does not.
        assert_eq!(
            cat("exited with status exit code: 0xffffffff before opening port 6768"),
            "exited-0xffffffff"
        );
        assert_eq!(
            cat("exited with status exit status: 1 before opening port 6768"),
            "exited-1"
        );
        // A different port is the SAME bug -- this is what used to split.
        assert_eq!(
            cat("exited with status exit status: 1 before opening port 6767"),
            "exited-1"
        );
        assert_eq!(
            cat("exited with status signal: 6 (SIGABRT) before opening port 6768"),
            "exited-signal: 6 (SIGABRT)"
        );
        // The two non-exit shapes stay separate: a wedged start and a crashed
        // one are different bugs.
        assert_eq!(
            cat("never opened port 6768 within 45000ms"),
            "startup-timeout"
        );
        assert_eq!(
            cat("wait check failed: No child processes (os error 10)"),
            "wait-check-failed"
        );
        assert_eq!(cat("something upstream changed"), "other");
    }

    #[test]
    fn parse_request_count_reads_nested_requests_total() {
        let body = json!({
            "requests": { "total": 42, "active": 1 },
            "tokens": { "saved": 100 }
        })
        .to_string();
        assert_eq!(parse_request_count_from_stats_body(&body), Some(42));
    }

    #[test]
    fn parse_request_count_falls_back_to_legacy_keys() {
        // Older /stats payloads exposed the count under flat keys. The
        // verification poller has to keep working against any of them or it
        // will get stuck on a runtime mid-upgrade between schema versions.
        let body = json!({ "total_requests": 7 }).to_string();
        assert_eq!(parse_request_count_from_stats_body(&body), Some(7));

        let body = json!({ "totalRequests": 9 }).to_string();
        assert_eq!(parse_request_count_from_stats_body(&body), Some(9));

        let body = json!({ "nested": { "requests_total": 11 } }).to_string();
        assert_eq!(parse_request_count_from_stats_body(&body), Some(11));
    }

    #[test]
    fn parse_request_count_returns_none_when_absent() {
        let body = json!({ "tokens": { "saved": 100 } }).to_string();
        assert_eq!(parse_request_count_from_stats_body(&body), None);
        assert_eq!(parse_request_count_from_stats_body("not json"), None);
    }

    #[test]
    fn parse_request_counts_by_agent_keys_by_agent_id() {
        let body = json!({
            "agent_usage": {
                "agents": [
                    { "agent": "claude-code", "requests": 5 },
                    { "agent": "codex", "requests": 2 }
                ]
            }
        })
        .to_string();
        let counts = parse_request_counts_by_agent(&body).unwrap();
        assert_eq!(counts.get("claude-code"), Some(&5));
        assert_eq!(counts.get("codex"), Some(&2));

        // Proxy up, no traffic yet: empty map, not None.
        let empty = json!({ "agent_usage": { "agents": [] } }).to_string();
        assert!(parse_request_counts_by_agent(&empty).unwrap().is_empty());

        // Unparseable body is None so the poller treats it as unreachable.
        assert!(parse_request_counts_by_agent("not json").is_none());
    }

    #[test]
    fn build_watchdog_give_up_report_uses_exit_status_when_present() {
        let report = build_watchdog_give_up_report(
            3,
            false,
            false,
            Some("exit status: 1".to_string()),
            Some("Traceback (most recent call last):\n  ...".to_string()),
            None,
            None,
            false,
            None,
            None,
            "ok".to_string(),
        );
        assert_eq!(report.tracked_child_exit_status, "exit status: 1");
        assert_eq!(report.consecutive_failures, 3);
        assert_eq!(
            report.message,
            "proxy_unreachable_post_boot (auto_paused after 3 failures)"
        );
        assert_eq!(
            report.log_tail.as_deref(),
            Some("Traceback (most recent call last):\n  ...")
        );
    }

    #[test]
    fn build_watchdog_give_up_report_falls_back_when_child_untracked() {
        // headroom_process_exited returns None when no Child handle is held
        // or the OS hasn't reaped the child. Payload must still be useful.
        let report = build_watchdog_give_up_report(
            5,
            true,
            false,
            None,
            None,
            None,
            None,
            false,
            None,
            None,
            "refused".to_string(),
        );
        assert_eq!(report.tracked_child_exit_status, "still_alive_or_untracked");
        assert!(report.bypass_active);
        assert!(report.log_tail.is_none());
    }

    #[test]
    fn build_watchdog_give_up_report_drops_empty_log_tail() {
        // tail_log_file returns "" when the log file is missing or unreadable.
        // Empty tails must not become an empty `proxy_log_tail` Sentry extra.
        let report = build_watchdog_give_up_report(
            3,
            false,
            false,
            None,
            Some(String::new()),
            None,
            None,
            false,
            None,
            None,
            "timeout".to_string(),
        );
        assert!(report.log_tail.is_none());
    }

    #[test]
    fn build_watchdog_give_up_report_propagates_upgrade_flag() {
        let report = build_watchdog_give_up_report(
            3,
            false,
            true,
            None,
            None,
            None,
            None,
            false,
            None,
            None,
            "timeout".to_string(),
        );
        assert!(report.runtime_upgrade_in_progress);
    }

    #[test]
    fn build_watchdog_give_up_report_carries_last_startup_error() {
        let report = build_watchdog_give_up_report(
            3,
            false,
            false,
            None,
            None,
            Some("Address already in use (os error 48)".to_string()),
            None,
            false,
            None,
            None,
            "refused".to_string(),
        );
        assert_eq!(
            report.last_startup_error.as_deref(),
            Some("Address already in use (os error 48)")
        );
    }

    #[test]
    fn build_watchdog_give_up_report_drops_empty_last_startup_error() {
        let report = build_watchdog_give_up_report(
            3,
            false,
            false,
            None,
            None,
            Some(String::new()),
            None,
            false,
            None,
            None,
            "ok".to_string(),
        );
        assert!(report.last_startup_error.is_none());
    }

    #[test]
    fn build_watchdog_give_up_report_carries_diagnostic_fields() {
        // Busy-event-loop signature: process alive, port still binds,
        // backend /readyz times out, log silent for ~30s.
        let report = build_watchdog_give_up_report(
            3,
            false,
            false,
            None,
            None,
            None,
            Some(54321),
            true,
            Some(120),
            Some(30),
            "timeout".to_string(),
        );
        assert_eq!(report.tracked_pid, Some(54321));
        assert!(report.port_accepts_tcp);
        assert_eq!(report.process_cpu_secs, Some(120));
        assert_eq!(report.log_silent_secs, Some(30));
        assert_eq!(report.backend_readyz_outcome, "timeout");
    }

    #[test]
    fn readyz_failed_checks_csv_lists_only_unhealthy_sorted() {
        let body = serde_json::json!({
            "checks": {
                "startup": { "ready": true },
                "upstream": { "ready": false },
                "memory": { "ready": false },
                "cache": { "ready": true },
            }
        });
        assert_eq!(readyz_failed_checks_csv(&body), "memory,upstream");
    }

    /// RUST-5E: a sleep-wake `upstream` blip on a machine where the kompress
    /// model never loaded produced `http_503:kompress,upstream`, which is not
    /// upstream-*only*, so the watchdog force-killed a healthy backend and
    /// auto-paused. Soft checks must not reach the outcome string.
    #[test]
    fn readyz_failed_checks_csv_drops_optional_checks() {
        let body = serde_json::json!({
            "checks": {
                "upstream": { "ready": false },
                // 0.33+ backends flag it; older ones only have the name.
                "kompress": { "ready": false, "optional": true },
                "embeddings": { "ready": false, "optional": true },
            }
        });
        assert_eq!(readyz_failed_checks_csv(&body), "upstream");
        assert!(readyz_failure_is_upstream_only(&format!(
            "http_503:{}",
            readyz_failed_checks_csv(&body)
        )));

        let legacy = serde_json::json!({
            "checks": { "upstream": { "ready": false }, "kompress": { "ready": false } }
        });
        assert_eq!(readyz_failed_checks_csv(&legacy), "upstream");

        // A genuinely wedged core still reports, optional siblings or not.
        let wedged = serde_json::json!({
            "checks": { "memory": { "ready": false }, "kompress": { "ready": false } }
        });
        assert_eq!(readyz_failed_checks_csv(&wedged), "memory");
    }

    #[test]
    fn readyz_failed_checks_csv_empty_when_all_ready_or_no_checks() {
        let all_ready = serde_json::json!({ "checks": { "upstream": { "ready": true } } });
        assert_eq!(readyz_failed_checks_csv(&all_ready), "");
        let no_checks = serde_json::json!({ "ready": false });
        assert_eq!(readyz_failed_checks_csv(&no_checks), "");
    }

    #[test]
    fn client_setup_error_kind_buckets_by_io_shape() {
        let enospc =
            anyhow::Error::new(std::io::Error::from_raw_os_error(28)).context("creating backup");
        assert_eq!(client_setup_error_kind(&enospc), "no_space");

        let exists = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
            .context("renaming codex config");
        assert_eq!(client_setup_error_kind(&exists), "already_exists");

        let not_found = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(client_setup_error_kind(&not_found), "not_found");

        // No io::Error in the chain -> "other".
        assert_eq!(client_setup_error_kind(&anyhow::anyhow!("boom")), "other");
    }

    #[test]
    fn readyz_fingerprint_key_buckets_shapes_and_drops_cardinality() {
        assert_eq!(readyz_outcome_fingerprint_key("ok"), "readyz_ok");
        assert_eq!(readyz_outcome_fingerprint_key("timeout"), "readyz_timeout");
        assert_eq!(readyz_outcome_fingerprint_key("refused"), "readyz_refused");
        // high-cardinality tails collapse to one bucket
        assert_eq!(readyz_outcome_fingerprint_key("http_503"), "readyz_503");
        assert_eq!(
            readyz_outcome_fingerprint_key("http_503:memory,upstream"),
            "readyz_503"
        );
        assert_eq!(
            readyz_outcome_fingerprint_key("http_500"),
            "readyz_http_other"
        );
        assert_eq!(
            readyz_outcome_fingerprint_key("error: connection reset by peer"),
            "readyz_error"
        );
    }

    #[test]
    fn child_state_fingerprint_key_splits_alive_from_untracked() {
        // exit_status present -> the child died, regardless of pid.
        assert_eq!(
            child_state_fingerprint_key("exited_with_1", Some(42)),
            "child_exited"
        );
        assert_eq!(
            child_state_fingerprint_key("exited_with_1", None),
            "child_exited"
        );
        // "still_alive_or_untracked" splits on pid presence: a tracked pid is a
        // genuinely-alive (mid-boot) child; None is no handle / backend absent.
        assert_eq!(
            child_state_fingerprint_key("still_alive_or_untracked", Some(42)),
            "child_alive"
        );
        assert_eq!(
            child_state_fingerprint_key("still_alive_or_untracked", None),
            "child_untracked"
        );
    }

    #[test]
    fn classify_backend_readyz_retries_bare_503_once() {
        // Bare http_503 = unreadable body under load; a second read that
        // parses must win (RUST-2X: upstream-only blips were misclassified
        // as wedged-core and force-killed).
        let mut calls = 0;
        let (outcome, body) = classify_backend_readyz(|| {
            calls += 1;
            if calls == 1 {
                ("http_503".to_string(), None)
            } else {
                ("http_503:upstream".to_string(), Some("{...}".to_string()))
            }
        });
        assert_eq!(calls, 2);
        assert_eq!(outcome, "http_503:upstream");
        assert_eq!(body.as_deref(), Some("{...}"));

        // Two bare results stand as a wedge signal.
        let mut calls = 0;
        let (outcome, _) = classify_backend_readyz(|| {
            calls += 1;
            ("http_503".to_string(), None)
        });
        assert_eq!(calls, 2);
        assert_eq!(outcome, "http_503");

        // Anything other than bare http_503 is accepted on the first probe.
        for first in ["ok", "http_503:upstream", "timeout", "refused"] {
            let mut calls = 0;
            let (outcome, _) = classify_backend_readyz(|| {
                calls += 1;
                (first.to_string(), None)
            });
            assert_eq!(calls, 1);
            assert_eq!(outcome, first);
        }
    }

    #[test]
    fn readyz_failure_is_upstream_only_matches_only_upstream() {
        assert!(readyz_failure_is_upstream_only("http_503:upstream"));
        assert!(!readyz_failure_is_upstream_only("http_503:upstream,memory"));
        assert!(!readyz_failure_is_upstream_only("http_503:memory"));
        assert!(!readyz_failure_is_upstream_only("http_503"));
        assert!(!readyz_failure_is_upstream_only("ok"));
        assert!(!readyz_failure_is_upstream_only("timeout"));
    }

    #[test]
    fn readyz_failure_has_core_unhealthy_ignores_upstream_only() {
        assert!(readyz_failure_has_core_unhealthy("http_503:memory"));
        assert!(readyz_failure_has_core_unhealthy(
            "http_503:upstream,memory"
        ));
        assert!(readyz_failure_has_core_unhealthy(
            "http_503:startup,upstream"
        ));
        assert!(!readyz_failure_has_core_unhealthy("http_503:upstream"));
        assert!(!readyz_failure_has_core_unhealthy("http_503"));
        assert!(!readyz_failure_has_core_unhealthy("ok"));
        assert!(!readyz_failure_has_core_unhealthy("timeout"));
    }

    #[test]
    fn cpu_rate_indicates_burn_separates_spin_from_boundary_tick() {
        // Real spin: ~1 CPU-sec per wall-sec over the window.
        assert!(cpu_rate_indicates_burn(100, 104, 4.0));
        // Lone boundary tick: a single +1 over a ~4s window is rate 0.25.
        assert!(!cpu_rate_indicates_burn(100, 101, 4.0));
        // Idle: counter flat.
        assert!(!cpu_rate_indicates_burn(100, 100, 4.0));
        // Exactly at the 0.5 threshold does not count (strictly greater).
        assert!(!cpu_rate_indicates_burn(100, 102, 4.0));
        assert!(cpu_rate_indicates_burn(100, 103, 4.0));
    }

    #[test]
    fn cpu_rate_indicates_burn_guards_degenerate_inputs() {
        // Zero elapsed: avoid divide-by-zero, report not burning.
        assert!(!cpu_rate_indicates_burn(100, 200, 0.0));
        // `ps` counter going backwards (pid reuse / sampling skew): saturating
        // sub yields 0, not a panic or huge rate.
        assert!(!cpu_rate_indicates_burn(200, 100, 4.0));
    }

    #[test]
    fn learn_agent_tags_round_trip_with_parse() {
        // The Sentry tag values are a grouping contract: renaming one silently
        // splits every historical event for that agent into a new bucket.
        for agent in [
            LearnAgent::Claude,
            LearnAgent::Codex,
            LearnAgent::Opencode,
            LearnAgent::Grok,
        ] {
            assert_eq!(LearnAgent::parse(agent.as_tag()), Ok(agent));
        }
    }

    /// RUST-74 arrived with `reason` = "LLM analysis failed: `claude -p ...`
    /// failed (exit 1):" and nothing after the colon, 14 events over six users'
    /// machines that no fix could be aimed at. The capture now attaches the
    /// analyzer's stderr tail -- and must attach ONLY that: stdout echoes
    /// written memory files back verbatim (see `learn_step_label`), so reading
    /// it here would start shipping users' project content to Sentry.
    #[test]
    fn learn_llm_failure_capture_attaches_stderr_tail_and_never_stdout() {
        let source = include_str!("lib.rs");
        let (_, after) = source
            .split_once(r#"set_tag("learn_outcome", "llm_analysis_failed")"#)
            .expect("capture block present");
        let block = &after[..after.find("capture_message").expect("capture block end")];
        assert!(
            block.contains("tail_lines(&stderr"),
            "the analyzer's stderr tail is the only cause context we have: {block}"
        );
        assert!(
            !block.contains("&stdout") && !block.contains("&merged"),
            "stdout carries memory-file contents and must never reach Sentry: {block}"
        );
    }

    #[test]
    fn learn_failure_is_agent_auth_matches_the_cli_login_prompts() {
        // RUST-B6 verbatim: four events whose only content was the child CLI
        // saying nobody is signed in.
        let stderr = "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nNot logged in \u{b7} Please run /login\n";
        assert!(learn_failure_is_agent_auth(stderr));
        assert!(learn_failure_is_agent_auth(
            "Error: not authenticated. Please run `codex login`."
        ));
        assert!(learn_failure_is_agent_auth(
            "AuthenticationError: invalid API key"
        ));
        assert!(learn_failure_is_agent_auth("Your OAuth token has expired."));
        // RUST-BN verbatim: expired session the CLI could not refresh.
        assert!(learn_failure_is_agent_auth(
            "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nFailed to authenticate: OAuth session expired and could not be refreshed"
        ));
    }

    #[test]
    fn learn_failure_is_agent_auth_does_not_swallow_real_failures() {
        // None of these are auth: the "run /login" remedy would be wrong
        // advice. (The usage-limit line is user-environment too, but it is the
        // LIMIT classifier's job, with its own remedy.)
        for stderr in [
            "LLM analysis failed: `claude -p` did not respond within 120s.",
            "ModuleNotFoundError: No module named 'headroom.learn'",
            "Error: rate limit exceeded, try again later",
            "usage limit reached for this session",
            "",
        ] {
            assert!(!learn_failure_is_agent_auth(stderr), "for: {stderr}");
        }
    }

    #[test]
    fn learn_failure_agent_limit_line_matches_the_cli_limit_messages() {
        // RUST-BF verbatim: the child CLI's diagnosis on the line after
        // upstream's marker.
        let stderr = "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nYou've hit your session limit \u{b7} resets 9:10am (America/Chicago)\n";
        let line = learn_failure_agent_limit_line(stderr).expect("should match");
        // The matched line (not the marker) so the hint carries the reset time.
        assert!(line.starts_with("You've hit your session limit"), "{line}");
        assert!(line.contains("resets 9:10am"), "{line}");

        assert!(learn_failure_agent_limit_line("usage limit reached for this session").is_some());
        assert!(learn_failure_agent_limit_line("You've hit your usage limit.").is_some());
    }

    #[test]
    fn learn_failure_agent_limit_line_does_not_swallow_real_failures() {
        // These must keep reporting: they are ours to fix (or transient).
        for stderr in [
            "LLM analysis failed: `claude -p` did not respond within 120s.",
            "Error: rate limit exceeded, try again later",
            "Not logged in \u{b7} Please run /login",
            "Credit balance is too low",
            // "limit" in a project's own echoed source must not match.
            "const SESSION_LIMIT = 5;",
            "",
        ] {
            assert!(
                learn_failure_agent_limit_line(stderr).is_none(),
                "for: {stderr}"
            );
        }
    }

    #[test]
    fn learn_failure_is_agent_model_rejected_matches_the_cli_tag_only() {
        // RUST-BQ verbatim: a custom model override the CLI's backend rejects.
        assert!(learn_failure_is_agent_model_rejected(
            "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\n[claude-code:unrecognized_model] {\"model\":\"mimo-v2.5\",\"query_source\":\"generate_session_title\"}\nAPI Error: 400 status code (no body)"
        ));
        // These must keep reporting: they are ours to fix (or transient).
        for stderr in [
            "LLM analysis failed: `claude -p` did not respond within 120s.",
            "API Error: 400 status code (no body)",
            "Prompt is too long",
            "",
        ] {
            assert!(
                !learn_failure_is_agent_model_rejected(stderr),
                "for: {stderr}"
            );
        }
    }

    #[test]
    fn learn_agent_limit_hint_echoes_the_reset_time_and_names_the_cli() {
        let hint = learn_agent_limit_hint(
            LearnAgent::Claude,
            "You've hit your session limit \u{b7} resets 9:10am (America/Chicago)",
        );
        assert!(hint.contains("Claude Code"), "{hint}");
        assert!(hint.contains("resets 9:10am"), "{hint}");
        assert!(learn_agent_limit_hint(LearnAgent::Codex, "usage limit reached").contains("Codex"));
    }

    #[test]
    fn learn_failure_signature_source_joins_the_dangling_marker_line() {
        // Upstream's marker ends at a colon and appends the child's real
        // diagnosis on the next line. Fingerprinting the marker alone put every
        // distinct cause on one issue (RUST-74, then RUST-B6).
        let stderr = "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nNot logged in \u{b7} Please run /login\n";
        let signature = learn_failure_signature_source(stderr);
        assert!(signature.contains("Not logged in"), "got: {signature}");

        // Two different causes behind the same marker must not collapse.
        let other = "LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nCredit balance is too low\n";
        assert_ne!(signature, learn_failure_signature_source(other));
    }

    #[test]
    fn learn_failure_signature_source_never_joins_a_dump_marker() {
        // RUST-B7: "First 2000 chars:" introduces the model's raw output,
        // derived from the user's own sessions. It ends at a colon like the
        // exit marker does, but must never be pulled into a Sentry title.
        let stderr = "LLM analysis failed: `claude -p --output-format stream-json --verbose` \
                      returned unparseable output. First 2000 chars:\n\
                      {\"type\":\"assistant\",\"text\":\"The auth module in src/login.ts ...\"}\n";
        let signature = learn_failure_signature_source(stderr);
        assert!(signature.ends_with("First 2000 chars:"), "got: {signature}");
        assert!(!signature.contains("login.ts"), "got: {signature}");
    }

    #[test]
    fn learn_failure_signature_source_skips_unrelated_stderr_preamble() {
        // RUST-BC: an onnxruntime C-API warning from upstream's own logger is
        // the first stderr line on any host with onnxruntime < 1.24. It has
        // nothing to do with the failure, and fingerprinting it merged every
        // distinct learn failure on those machines onto one issue.
        let preamble = "onnxruntime 1.20 exposes an older C API than Rust detection requires (1.24+); leaving ORT_DYLIB_PATH unset and using Python detection\n";
        let stderr = format!(
            "{preamble}LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nCredit balance is too low\n"
        );
        let signature = learn_failure_signature_source(&stderr);
        assert!(
            signature.contains("Credit balance is too low"),
            "got: {signature}"
        );
        assert!(!signature.contains("onnxruntime"), "got: {signature}");

        // Two causes behind the same preamble must still not collapse.
        let other = format!(
            "{preamble}LLM analysis failed: `claude -p --output-format stream-json --verbose` failed (exit 1):\nNot logged in \u{b7} Please run /login\n"
        );
        assert_ne!(signature, learn_failure_signature_source(&other));

        // No marker anywhere: the first line is still all there is.
        assert!(
            learn_failure_signature_source(preamble).starts_with("onnxruntime 1.20"),
            "preamble-only stderr should fall back to line one"
        );
    }

    #[test]
    fn learn_failure_signature_source_leaves_a_self_contained_line_alone() {
        let stderr = "ModuleNotFoundError: No module named 'headroom'\nTraceback follows\n";
        assert_eq!(
            learn_failure_signature_source(stderr),
            "ModuleNotFoundError: No module named 'headroom'"
        );
        assert_eq!(learn_failure_signature_source("   \n\n"), "no output");
    }

    #[test]
    fn learn_agent_auth_hint_names_the_cli_the_run_shelled_out_to() {
        let hint = learn_agent_auth_hint(LearnAgent::Claude);
        assert!(hint.contains("Claude Code"), "got: {hint}");
        assert!(hint.contains("`claude`"), "got: {hint}");
        assert!(learn_agent_auth_hint(LearnAgent::Codex).contains("`codex`"));
    }

    #[test]
    fn extract_llm_failure_warnings_returns_none_for_clean_stderr() {
        let stderr =
            "2026-05-04 09:00:00,000 - headroom.learn.analyzer - INFO - using claude CLI backend\n";
        assert!(extract_llm_failure_warnings(stderr).is_none());
    }

    #[test]
    fn extract_llm_failure_warnings_extracts_single_timeout() {
        let stderr = "2026-05-03 22:18:50,070 - headroom.learn.analyzer - WARNING - LLM analysis failed: `claude -p` did not respond within 120s. Check network connectivity or try a different backend with --model <litellm-model-name>.\n";
        let extracted = extract_llm_failure_warnings(stderr).expect("warning extracted");
        assert!(extracted.starts_with("LLM analysis failed:"));
        assert!(extracted.contains("did not respond within 120s"));
    }

    #[test]
    fn extract_llm_failure_warnings_joins_multiple_lines() {
        let stderr = "\
2026-05-03 22:18:50,070 - headroom.learn.analyzer - WARNING - LLM analysis failed: `claude -p` did not respond within 120s.
2026-05-03 22:20:50,749 - headroom.learn.analyzer - WARNING - LLM analysis failed: `claude -p` did not respond within 120s.
";
        let extracted = extract_llm_failure_warnings(stderr).expect("warnings extracted");
        assert_eq!(extracted.matches("LLM analysis failed:").count(), 2);
        assert!(extracted.contains('\n'));
    }

    #[test]
    fn learn_step_label_maps_the_cli_stages() {
        assert_eq!(
            learn_step_label("  Analyzing with claude-cli...").as_deref(),
            Some("Analyzing with Claude Code")
        );
        assert_eq!(
            learn_step_label("  Recommendations: 7").as_deref(),
            Some("Found 7 patterns")
        );
        assert_eq!(
            learn_step_label("  [WROTE] /Users/x/proj/CLAUDE.md").as_deref(),
            Some("Updating CLAUDE.md")
        );
        assert_eq!(
            learn_step_label("  No conversation data found.").as_deref(),
            Some("No conversation data found.")
        );
    }

    #[test]
    fn learn_step_label_ignores_decoration_and_written_file_contents() {
        // After [WROTE] the CLI echoes the file back. None of it may reach the
        // UI, or the step line ends on a random memory bullet.
        for line in [
            "",
            "============================================================",
            "[claude] headroom-desktop",
            "Path: /Users/x/proj",
            "  ──────────────────────────────────────────────────",
            "  - Working test command: cargo test --lib",
            "  ## Headroom Learned Patterns",
        ] {
            assert_eq!(learn_step_label(line), None, "line leaked: {line:?}");
        }
    }

    #[test]
    fn stream_headroom_learn_output_reassembles_both_streams_and_forwards_steps() {
        let base_dir =
            std::env::temp_dir().join(format!("headroom-learn-stream-{}", uuid::Uuid::new_v4()));
        let state = crate::state::AppState::new_in(base_dir.clone()).expect("app state");
        state.mark_headroom_learn_running_for_test();

        let mut command = crate::proc::command("sh");
        command.arg("-c").arg(
            "printf '[claude] proj\\n  Analyzing with claude-cli...\\n  Recommendations: 3\\n'; \
             printf 'warn line\\n' >&2; exit 7",
        );
        let output = super::stream_headroom_learn_output(&state, &mut command).expect("spawned");

        // Same contract as `command.output()`: exit status and both streams intact.
        assert_eq!(output.status.code(), Some(7));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Analyzing with claude-cli..."), "{stdout}");
        assert!(stdout.contains("Recommendations: 3"), "{stdout}");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "warn line");
        // Last recognized line wins; the decorative header is not a step.
        assert_eq!(
            state.headroom_learn_status(None).current_step.as_deref(),
            Some("Found 3 patterns")
        );

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn classify_bootstrap_failure_flags_github_504_as_network() {
        // Mirrors the reqwest chain produced when error_for_status hits a 504 on
        // a GitHub release asset (the install_rtk download path).
        let err = anyhow::anyhow!(
            "HTTP status server error (504 Gateway Time-out) for url \
             (https://github.com/rtk-ai/rtk/releases/download/v0.42.0/rtk-aarch64-apple-darwin.tar.gz)"
        )
        .context("downloading https://github.com/rtk-ai/rtk/releases/download/v0.42.0/rtk-aarch64-apple-darwin.tar.gz");
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::NetworkDownload
        ));
    }

    #[test]
    fn is_network_download_signal_matches_transient_failures() {
        for sample in [
            "HTTP status server error (504 Gateway Time-out)",
            "error sending request for url (https://pypi.org/...)",
            "tcp connect error: Connection refused (os error 61)",
            "dns error: failed to lookup address information",
            "operation timed out",
        ] {
            assert!(is_network_download_signal(sample), "should match: {sample}");
        }
    }

    #[test]
    fn is_network_download_signal_ignores_config_failures() {
        assert!(!is_network_download_signal("CERTIFICATE_VERIFY_FAILED"));
        assert!(!is_network_download_signal(
            "No usable temporary directory found"
        ));
        assert!(!is_network_download_signal(
            "checksum mismatch for ...: expected abc, got def"
        ));
    }

    // Endpoint-protection signature matcher: kept conservative on purpose, so
    // every match here represents a pattern we believe is high-confidence AV/
    // EDR interference. Adding looser patterns dilutes the user-facing hint.

    #[test]
    fn is_endpoint_protection_signal_matches_code_signature_failures() {
        assert!(is_endpoint_protection_signal(
            "dyld[1234]: code signature invalid for '/path/to/_mmh3.so'"
        ));
        assert!(is_endpoint_protection_signal(
            "ERROR: code signature could not be verified for headroom_core"
        ));
    }

    #[test]
    fn is_endpoint_protection_signal_matches_dlopen_not_permitted() {
        let raw = "ImportError: dlopen(/Users/x/site-packages/torch/lib/libtorch.dylib, 0x0006): \
                   tried: '/Users/x/site-packages/torch/lib/libtorch.dylib' (operation not permitted)";
        assert!(is_endpoint_protection_signal(raw));

        // "Library not loaded" variant of the same dyld error.
        let raw2 = "Library not loaded: @rpath/libonnxruntime.dylib \
                    Reason: tried: '...' (operation not permitted)";
        assert!(is_endpoint_protection_signal(raw2));
    }

    #[test]
    fn is_endpoint_protection_signal_matches_sigkill_signatures() {
        assert!(is_endpoint_protection_signal(
            "command exited with signal=9 (no stderr)"
        ));
        assert!(is_endpoint_protection_signal("headroom: Killed: 9"));
        assert!(is_endpoint_protection_signal(
            "exit code 137 from /venv/bin/python -m headroom.proxy.server"
        ));
    }

    #[test]
    fn is_endpoint_protection_signal_matches_fresh_so_permission_denial() {
        assert!(is_endpoint_protection_signal(
            "open() Operation not permitted on /Users/x/site-packages/mmh3.cpython-312-darwin.so"
        ));
        assert!(is_endpoint_protection_signal(
            "Operation not permitted: cannot exec /venv/lib/libtorch_python.dylib"
        ));
    }

    #[test]
    fn is_endpoint_protection_signal_matches_windows_app_control() {
        assert!(is_endpoint_protection_signal(
            "An Application Control policy has blocked this file. (os error 4551)"
        ));
        // Localized Windows prose: only the numeric code survives.
        assert!(is_endpoint_protection_signal(
            "차단되었습니다. (os error 4551)"
        ));
    }

    /// The exact strings RUST-AD and RUST-AC carried: one Windows host, one
    /// App Control policy, filed as two issues because the two call sites
    /// prefix the same error differently and neither set a fingerprint. Both
    /// must reach the endpoint-protection branch of
    /// `capture_headroom_start_failure`, which is what collapses them.
    #[test]
    fn a_windows_app_control_block_is_recognised_at_proxy_start() {
        // Spanish Windows, verbatim from the event. Only "os error 4551"
        // survives localization, so that is what has to carry the match.
        let resume = "apply_client_setup: resume_runtime failed: starting headroom background \
                      process: ~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\headroom.exe \
                      proxy --port 6768 --no-http2 --log-messages --no-ccr --learn \
                      --no-memory-tools --no-memory-context --memory-db-path \
                      ~\\AppData\\Local\\Headroom\\memory.db: Una directiva de Control de \
                      aplicaciones bloqueó este archivo. (os error 4551)";
        let autostart = "headroom auto-start failed after bootstrap: starting headroom \
                         background process: ...: Una directiva de Control de aplicaciones \
                         bloqueó este archivo. (os error 4551)";

        assert!(is_endpoint_protection_signal(resume), "RUST-AD unmatched");
        assert!(
            is_endpoint_protection_signal(autostart),
            "RUST-AC unmatched"
        );

        // A port conflict must NOT be swallowed by the new branch: it has its
        // own fingerprint and its own remediation.
        assert!(!is_endpoint_protection_signal(
            "starting headroom background process: port 6768 is occupied by a \
             non-headroom process"
        ));
    }

    /// RUST-BB/BA/5C verbatim: the App Control verdict seen from inside
    /// Python. No `os error 4551` -- CPython's ImportError carries Windows'
    /// localized prose and nothing else -- so the code-based match above
    /// missed it and one host was filed as three Error-level issues.
    #[test]
    fn an_app_control_block_on_a_stdlib_dll_is_recognised_from_the_import_error() {
        let chain = "unable to keep headroom running in background (prior attempts: \
                     ~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\headroom.exe \
                     proxy --port 6768 exited with status exit code: 0xffffffff before opening \
                     port 6768) (onnx probe: onnxruntime imports cleanly): python.exe -m \
                     headroom.proxy.server exited with status exit code: 1 before opening port 6768\n\
                     --- log tail ---\n  File \"...\\Lib\\sqlite3\\dbapi2.py\", line 27, in <module>\n    \
                     from _sqlite3 import *\nImportError: DLL load failed while importing _sqlite3: \
                     Una directiva de Control de aplicaciones bloqueó este archivo.\n--- end log ---";
        assert!(is_blocked_runtime_dll_signal(chain));
        assert!(is_endpoint_protection_signal(chain));
        assert_eq!(
            startup_error_fingerprint_key(Some(chain)),
            Some("startup_endpoint_protection")
        );

        // Any locale: the structural half needs no prose at all.
        assert!(is_blocked_runtime_dll_signal(
            "ImportError: DLL load failed while importing _ssl: 지정된 모듈을 찾을 수 없습니다."
        ));

        // RUST-5C's `last_startup_error` carries the same verdict without the
        // `while`, in the copy upstream re-wraps into the error chain. Both
        // spellings must reach the same verdict or the give-up escalates to
        // Error for a block we already reported once.
        let no_while = "ImportError: DLL load failed importing _sqlite3: \
                        Una directiva de Control de aplicaciones bloqueó este archivo.";
        assert!(is_blocked_runtime_dll_signal(no_while));
        assert_eq!(
            startup_error_fingerprint_key(Some(no_while)),
            Some("startup_endpoint_protection")
        );
        // Still module-scoped, so the looser anchor cannot pull in a
        // third-party load failure or a bare "DLL load failed:".
        assert!(!is_blocked_runtime_dll_signal(
            "ImportError: DLL load failed importing onnxruntime_pybind11_state: not found."
        ));
        assert!(!is_blocked_runtime_dll_signal(
            "ImportError: DLL load failed: The specified module could not be found."
        ));
    }

    #[test]
    fn a_third_party_dll_load_failure_is_not_endpoint_protection() {
        // onnxruntime/torch failing to load is usually a missing Visual C++
        // runtime, which has its own hint; it must not be sold as antivirus.
        for raw in [
            "ImportError: DLL load failed while importing onnxruntime_pybind11_state: \
             The specified module could not be found.",
            "ImportError: DLL load failed while importing _C: The specified module could not be found.",
            "ModuleNotFoundError: No module named '_sqlite3'",
        ] {
            assert!(!is_blocked_runtime_dll_signal(raw), "for: {raw}");
            assert!(!is_endpoint_protection_signal(raw), "for: {raw}");
        }
    }

    #[test]
    fn startup_error_fingerprint_key_only_names_causes_with_a_remedy() {
        assert_eq!(startup_error_fingerprint_key(None), None);
        assert_eq!(
            startup_error_fingerprint_key(Some(
                "python.exe -m headroom.proxy.server exited with status exit code: 1 \
                 before opening port 6768"
            )),
            None,
            "an unclassified crash keeps the fingerprint the existing issues carry"
        );
        assert_eq!(
            startup_error_fingerprint_key(Some(
                "starting headroom background process: port 6768 is occupied by a \
                 non-headroom process"
            )),
            Some("startup_port_conflict")
        );
    }

    #[test]
    fn is_endpoint_protection_signal_matches_a_killed_onnx_probe() {
        // RUST-C7 verbatim: both spawn variants 0xffffffff, probe timed out.
        assert!(is_endpoint_protection_signal(
            "unable to keep headroom running in background (onnx probe: import onnxruntime \
             failed (killed): command timed out after 15000ms) (prior attempts: headroom.exe: \
             exited with status exit code: 0xffffffff before opening port 6768)"
        ));
        // A clean probe or a real import error is not endpoint protection.
        assert!(!is_endpoint_protection_signal(
            "(onnx probe: onnxruntime imports cleanly)"
        ));
        assert!(!is_endpoint_protection_signal(
            "(onnx probe: import onnxruntime failed (exit 1): ModuleNotFoundError: No module named 'onnxruntime')"
        ));
    }

    #[test]
    fn is_endpoint_protection_signal_does_not_overmatch_benign_errors() {
        // Bare "killed" with no signal marker — could be OOM, user pkill, etc.
        assert!(!is_endpoint_protection_signal(
            "process killed before completing"
        ));
        // "Library not loaded" without the "not permitted" gate — ordinary
        // missing-dep error, very common during dev.
        assert!(!is_endpoint_protection_signal(
            "Library not loaded: @rpath/libfoo.dylib — Reason: image not found"
        ));
        // "Operation not permitted" without a fresh-extension context — could
        // be any random filesystem permission issue.
        assert!(!is_endpoint_protection_signal(
            "Operation not permitted on /private/var/db/foo.txt"
        ));
        // Generic network/disk errors must not falsely trigger.
        assert!(!is_endpoint_protection_signal(
            "Could not resolve host: pypi.org"
        ));
        assert!(!is_endpoint_protection_signal("ENOSPC: no space left"));
    }

    #[test]
    fn missing_webview_runtime_is_recognised_from_tauris_own_message() {
        // Verbatim Display text of tauri_runtime::Error::WebviewRuntimeNotInstalled
        // (tauri-runtime 2.11.3), which is what Sentry RUST-8J reports.
        assert!(super::is_missing_webview_runtime(
            "Could not find the webview runtime, make sure it is installed"
        ));
        // Any other build failure must keep the plain panic, no dialog.
        assert!(!super::is_missing_webview_runtime("window not found"));
        assert!(!super::is_missing_webview_runtime(
            "the event loop has been closed"
        ));
    }

    #[test]
    fn is_disk_full_signal_matches_pip_enospc_failures() {
        assert!(is_disk_full_signal(
            "ERROR: Could not install packages due to an OSError: [Errno 28] No space left on device"
        ));
        assert!(is_disk_full_signal(
            "OSError: [Errno 28] No space left on device"
        ));
        assert!(is_disk_full_signal("ENOSPC: no space left"));
        assert!(is_disk_full_signal("disk full"));
        // Case-insensitive.
        assert!(is_disk_full_signal("NO SPACE LEFT ON DEVICE"));
    }

    #[test]
    fn is_disk_full_signal_does_not_overmatch() {
        assert!(!is_disk_full_signal("network unreachable"));
        assert!(!is_disk_full_signal("permission denied"));
        assert!(!is_disk_full_signal("Could not resolve host: pypi.org"));
    }

    #[test]
    fn classify_upgrade_error_returns_endpoint_protection_hint_before_other_classifiers() {
        // Even when the error contains a "network" keyword (which would
        // otherwise hit the network classifier), the AV signal wins because
        // it's a more specific match for the actual cause.
        let err =
            anyhow::anyhow!("network unreachable during install — child exited with signal=9");
        let hint = classify_upgrade_error(&err).expect("must classify");
        assert!(
            hint.contains("endpoint protection"),
            "expected EDR hint, got: {hint}"
        );
    }

    /// The gate is only as good as the name it matches on, and that name is not
    /// the executable's: the Linux .deb ships `/usr/bin/headroom-desktop` while
    /// the kernel reports `headroom`. Check the derivation against what `ps`
    /// actually says about this very process.
    #[cfg(target_os = "macos")]
    #[test]
    fn relauncher_expect_name_matches_what_ps_reports() {
        let expect = super::relauncher_expect_name();
        assert!(!expect.is_empty(), "no name to gate the force-kill on");

        let out = crate::proc::command("ps")
            .args(["-o", "comm=", "-p", &std::process::id().to_string()])
            .output()
            .expect("run ps");
        let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();

        assert!(
            reported.contains(&expect),
            "gate would never fire: ps reports {reported:?}, gate matches on {expect:?}"
        );
    }

    /// The force-kill must sit behind the identity check, not beside it: this
    /// script SIGKILLs a pid it resolved up to 10 seconds earlier.
    #[cfg(target_os = "macos")]
    #[test]
    fn relauncher_force_kill_is_gated_on_identity() {
        let script = super::relauncher_script(4242, "headroom", "true");
        let gate = script
            .find("ps -o comm= -p 4242")
            .expect("no identity check");
        let kill = script.find("kill -9 4242").expect("no force-kill");
        assert!(
            gate < kill,
            "force-kill runs before the identity check: {script}"
        );
        assert!(
            script[gate..kill].contains("*headroom*)"),
            "force-kill is not inside the matching case arm: {script}"
        );
    }

    /// The verify-screen callout must never claim an agent is running when it
    /// is not: only argv[0]'s basename, or an agent script run by node/bun
    /// (the npm install shape), may count — never a filename later in the
    /// command line (`grep claude`, an editor on a file named "claude").
    #[test]
    fn agent_process_counts_match_binaries_and_interpreter_scripts_only() {
        let lines = [
            "/usr/local/bin/claude --resume",
            "node /Users/x/.nvm/versions/node/v20/bin/claude",
            "bun /home/y/.local/bin/opencode run",
            "codex exec",
            "grep claude proxy.log",
            "vim claude",
            "node server.js",
            "/Applications/Headroom.app/Contents/MacOS/headroom-desktop",
        ];
        let counts = agent_process_counts_from_lines(lines.into_iter());
        assert_eq!(counts.get("claude_code"), Some(&2));
        assert_eq!(counts.get("codex"), Some(&1));
        assert_eq!(counts.get("opencode"), Some(&1));
        assert_eq!(counts.get("grok_build"), None);
    }

    #[test]
    fn agent_process_counts_strip_windows_image_extensions() {
        let counts =
            agent_process_counts_from_lines(["claude.exe", "node.exe", "codex.EXE"].into_iter());
        assert_eq!(counts.get("claude_code"), Some(&1));
        assert_eq!(counts.get("codex"), Some(&1));
        assert_eq!(counts.len(), 2);
    }

    /// Installer failures must surface the installer's words, not curl's
    /// progress bars; blank output still yields a usable message upstream.
    #[test]
    fn last_nonempty_lines_keeps_the_tail_and_drops_noise() {
        let text = "step one\n\n  progress 42%  \nerror: no network\n\n";
        assert_eq!(
            super::last_nonempty_lines(text, 2),
            "progress 42% error: no network"
        );
        assert_eq!(super::last_nonempty_lines("\n \n", 3), "");
    }

    /// The unrouted-usage nudge keys on session mtimes newer than app start;
    /// an unparseable `last_worked_at` must read as untouched, not as fresh.
    #[test]
    fn claude_sessions_touched_since_compares_rfc3339_and_ignores_junk() {
        fn project(last_worked_at: &str) -> crate::models::ClaudeCodeProject {
            crate::models::ClaudeCodeProject {
                id: "p".into(),
                project_path: "/tmp/p".into(),
                display_name: "p".into(),
                last_worked_at: last_worked_at.into(),
                session_count: 1,
                sessions_today: 1,
                last_learn_ran_at: None,
                has_persisted_learnings: false,
                active_days_since_last_learn: 0,
                last_learn_pattern_count: None,
            }
        }
        let since = chrono::Utc::now();
        let before = (since - chrono::Duration::minutes(5)).to_rfc3339();
        let after = (since + chrono::Duration::minutes(5)).to_rfc3339();

        assert!(!claude_sessions_touched_since(&[project(&before)], since));
        assert!(claude_sessions_touched_since(
            &[project(&before), project(&after)],
            since
        ));
        assert!(!claude_sessions_touched_since(
            &[project("not-a-date")],
            since
        ));
        assert!(!claude_sessions_touched_since(&[], since));
    }

    /// An app that exited on its own must be relaunched without any kill at all.
    #[cfg(target_os = "macos")]
    #[test]
    fn relauncher_skips_the_kill_when_the_app_already_exited() {
        let dir = std::env::temp_dir().join(format!("hr-relaunch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let marker = dir.join("launched");

        // A pid that is dead for certain: spawn, reap, then reuse its number.
        let mut done = crate::proc::command("true").spawn().expect("spawn true");
        let dead = done.id();
        done.wait().expect("reap");

        let script = super::relauncher_script(
            dead,
            "headroom",
            &format!("touch {}", super::shell_quote_path(&marker)),
        );
        let started = std::time::Instant::now();
        let status = crate::proc::command("/bin/sh")
            .arg("-c")
            .arg(&script)
            .status()
            .expect("run relauncher");

        assert!(status.success(), "relauncher failed: {script}");
        assert!(marker.exists(), "launch never ran: {script}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited on a pid that was already gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The relauncher only ever runs after we are dead, so a quoting slip in it
    /// is silent until a user updates and never comes back. Syntax-check the
    /// real snippets (both carry quotes, `$(...)`, and a path with a space).
    #[cfg(target_os = "macos")]
    #[test]
    fn relauncher_script_is_valid_shell() {
        use std::path::Path;
        let app = super::shell_quote_path(Path::new("/Applications/Headroom RC.app"));
        let log = super::shell_quote_path(Path::new("/Users/a b/Library/Logs/Headroom/d.log"));
        let launches = [
            // macOS
            format!(
                "/usr/bin/open -n {app}; rc=$?; \
                 echo \"$(date '+%Y-%m-%d %H:%M:%S') relauncher: open -n {app} exited rc=$rc (alive=$alive)\" >> {log}"
            ),
            // Linux
            format!(
                "{app} >/dev/null 2>&1 & \
                 new=$!; sleep 1; \
                 if kill -0 $new 2>/dev/null; then st=running; else st=DIED; fi; \
                 echo \"$(date '+%Y-%m-%d %H:%M:%S') relauncher: launched {app} pid $new ($st, alive=$alive)\" >> {log}"
            ),
        ];
        for launch in launches {
            let script = super::relauncher_script(4242, "headroom", &launch);
            assert!(
                script.contains("kill -9 4242"),
                "lost the force-kill backstop: {script}"
            );
            let status = crate::proc::command("/bin/sh")
                .arg("-n")
                .arg("-c")
                .arg(&script)
                .status()
                .expect("run sh -n");
            assert!(status.success(), "sh rejected the script: {script}");
        }
    }

    #[test]
    fn magic_link_auth_extracts_email_and_code() {
        let url = tauri::Url::parse("headroom://auth?email=a%40b.com&code=123456").unwrap();
        assert_eq!(
            parse_magic_link_auth(&url),
            Some(("a@b.com".to_string(), "123456".to_string()))
        );
    }

    #[test]
    fn magic_link_auth_ignores_the_checkout_return_url() {
        // Every other headroom:// URL must fall through to the pricing refresh.
        for raw in [
            "headroom://checkout-complete",
            "headroom://",
            "headroom://auth",
        ] {
            let url = tauri::Url::parse(raw).unwrap();
            assert_eq!(parse_magic_link_auth(&url), None, "{raw}");
        }
    }

    #[test]
    fn magic_link_auth_rejects_half_filled_links() {
        for raw in [
            "headroom://auth?email=a%40b.com",
            "headroom://auth?code=123456",
            "headroom://auth?email=&code=123456",
            "headroom://auth?email=a%40b.com&code=",
        ] {
            let url = tauri::Url::parse(raw).unwrap();
            assert_eq!(parse_magic_link_auth(&url), None, "{raw}");
        }
    }

    /// The slot is one-shot: a window reload re-runs the claim on mount, and a
    /// second hand-out would replay a code the server has already spent. Sole
    /// owner of PENDING_MAGIC_LINK in the suite, so no serialisation needed.
    #[test]
    fn pending_magic_link_is_handed_out_exactly_once() {
        assert_eq!(take_pending_magic_link(), None, "slot starts empty");

        *PENDING_MAGIC_LINK.lock().unwrap() = Some(("a@b.com".to_string(), "123456".to_string()));

        assert_eq!(
            take_pending_magic_link(),
            Some(("a@b.com".to_string(), "123456".to_string()))
        );
        assert_eq!(
            take_pending_magic_link(),
            None,
            "a reload must not replay a spent code"
        );
    }
}

#[cfg(test)]
mod output_reduction_report_tests {
    use super::reported_output_reduction;
    use crate::models::OutputReduction;

    fn reduction() -> OutputReduction {
        OutputReduction {
            method: "estimated".to_string(),
            reduction_percent: 28.0,
            ci_low_percent: 20.0,
            ci_high_percent: 36.0,
            requests: 19_644,
        }
    }

    #[test]
    fn blocked_shaper_reports_inactive_without_a_percent() {
        let (pct, method) = reported_output_reduction(Some(&reduction()), Some(false));
        assert_eq!(pct, None);
        assert_eq!(method.as_deref(), Some("inactive"));
    }

    #[test]
    fn active_shaper_reports_the_reduction_unchanged() {
        let (pct, method) = reported_output_reduction(Some(&reduction()), Some(true));
        assert_eq!(pct, Some(28.0));
        assert_eq!(method.as_deref(), Some("estimated"));
    }

    #[test]
    fn unknown_rollout_state_keeps_the_old_behavior() {
        let (pct, method) = reported_output_reduction(Some(&reduction()), None);
        assert_eq!(pct, Some(28.0));
        assert_eq!(method.as_deref(), Some("estimated"));
        let (none_pct, none_method) = reported_output_reduction(None, None);
        assert_eq!(none_pct, None);
        assert_eq!(none_method, None);
    }
}
