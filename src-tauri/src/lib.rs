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
mod port_conflict;
mod pricing;
mod proxy_intercept;
mod research;
mod state;
mod storage;
mod tool_manager;

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
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::models::{
    ActivityFeedResponse, BillingPeriod, BootstrapProgress, ClaudeAccountProfile,
    ClaudeCodeProject, ClaudeUsage, ClientConnectorStatus, ClientSetupResult,
    ClientSetupVerification, DashboardState, HeadroomAuthCodeRequest, HeadroomLearnPrereqStatus,
    HeadroomLearnStatus, HeadroomPricingStatus, HeadroomSubscriptionTier, ResearchCandidate,
    RuntimeStatus, RuntimeUpgradeProgress, TransformationFeedResponse,
};
use crate::state::AppState;

const UPDATER_PUBLIC_KEY: Option<&str> = option_env!("HEADROOM_UPDATER_PUBLIC_KEY");
const UPDATER_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_ENDPOINTS");
const UPDATER_STAGING_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_STAGING_ENDPOINTS");
const SENTRY_DSN: Option<&str> = option_env!("HEADROOM_SENTRY_DSN");
const DEFAULT_UPDATER_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk3QkUyNEU0MjVBMkRDM0MKUldRODNLSWw1Q1MrbC93MitlYTVoUXViSXJQNGVQWDdBRXA0Qkl4WGtpSEttNm5YTDB3QWtncEoK";
const DEFAULT_UPDATER_ENDPOINT: &str =
    "https://github.com/gglucass/headroom-desktop/releases/latest/download/latest.json";
const BETA_CHANNEL_ENV: &str = "HEADROOM_BETA_CHANNEL";
const BETA_CHANNEL_SENTINEL: &str = "beta_channel";
const AUTOSTART_LAUNCH_ARG: &str = "--autostart";
const HEADROOM_DASHBOARD_URL: &str = "http://127.0.0.1:6767/dashboard";
const MAIN_WINDOW_WIDTH: u32 = 760;
const MAIN_WINDOW_HEIGHT: u32 = 560;
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

// Set when the watchdog has captured a Sentry event for the current "down
// episode". Reset whenever the proxy is observed reachable again, so a
// subsequent crash re-fires.
static WATCHDOG_DOWN_CAPTURED: AtomicBool = AtomicBool::new(false);

// Set after the first port-conflict start failure has been captured this
// session. Subsequent in-session port conflicts stay silent so the dashboard
// doesn't drown in the sleep/wake / kill -9 race noise.
static PORT_CONFLICT_CAPTURED: AtomicBool = AtomicBool::new(false);

// Spend fields (actual_cost_usd, total_tokens_sent) were added to SavingsRecord in
// schema v6, shipped in 0.2.40 on 2026-04-13. Records written before that date
// deserialize those fields as 0 via #[serde(default)], producing false positives.
const SPEND_SCHEMA_CUTOFF_DATE: &str = "2026-04-13";

fn check_zero_spend_anomaly(dashboard: &DashboardState) {
    if ZERO_SPEND_ALERT_FIRED.load(Ordering::Relaxed) {
        return;
    }
    let affected_days: Vec<&str> = dashboard
        .daily_savings
        .iter()
        .filter(|p| {
            p.date.as_str() >= SPEND_SCHEMA_CUTOFF_DATE
                && p.estimated_tokens_saved > 0
                && p.actual_cost_usd == 0.0
                && p.total_tokens_sent == 0
        })
        .map(|p| p.date.as_str())
        .collect();
    if affected_days.is_empty() {
        return;
    }
    ZERO_SPEND_ALERT_FIRED.store(true, Ordering::Relaxed);
    sentry::capture_message(
        &format!(
            "graph shows tokens saved but zero tokens spent on days: {}",
            affected_days.join(", ")
        ),
        sentry::Level::Warning,
    );
}

#[tauri::command]
async fn get_dashboard_state(app: AppHandle) -> Result<DashboardState, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state: State<'_, AppState> = app.state();
        let (dashboard, pending_milestones) = state.dashboard_with_pending_milestones();

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
            pricing::report_milestone(*milestone_tokens_saved);
        }

        check_zero_spend_anomaly(&dashboard);

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
        },
        Ok(None) => AppUpdateConfiguration {
            enabled: false,
            current_version,
            endpoint_count: 0,
            configuration_error: None,
            beta_channel_enabled,
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

    let updater = app
        .updater_builder()
        .pubkey(config.pubkey)
        .endpoints(config.endpoints)
        .map_err(|err| err.to_string())?
        .build()
        .map_err(|err| err.to_string())?;

    let checked_update = updater
        .check()
        .await
        .map(|update| update.map(TauriPendingUpdate))
        .map_err(|err| err.to_string());

    store_checked_update(checked_update, &pending_update.0)
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

    update.install(progress).await
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    // Stop the proxy before relaunching so the new build starts a fresh proxy
    // with current args (otherwise the orphan keeps serving traffic and the
    // new desktop reuses it via the reachability check). Without this, any
    // proxy-arg change shipped by an upgrade silently never takes effect.
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.stop_headroom();
    }
    analytics::shutdown(&app);

    // Tauri 2.x has an open bug on macOS (tauri-apps/tauri#13923, #11392)
    // where `request_restart()` and `restart()` exit the process but never
    // relaunch — especially with `tauri-plugin-single-instance` loaded.
    // Workaround: spawn a detached relauncher via `open -n` against this
    // app's .app bundle (which is in-place updated by the updater), then
    // exit cleanly so the single-instance lock is released before the new
    // process starts.
    #[cfg(target_os = "macos")]
    {
        if let Some(bundle) = current_app_bundle_path() {
            // Tiny sleep so the relaunch fires after our exit releases the
            // single-instance lock; without it the new process can detect
            // the still-dying old one and bail.
            let cmd = format!(
                "sleep 1 && /usr/bin/open -n {}",
                shell_quote_path(&bundle)
            );
            let _ = Command::new("/bin/sh").arg("-c").arg(cmd).spawn();
        }
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
fn get_research_candidates() -> Vec<ResearchCandidate> {
    research::candidate_matrix()
}

#[tauri::command]
fn bootstrap_runtime(state: State<'_, AppState>) -> Result<DashboardState, String> {
    state
        .tool_manager
        .bootstrap_all()
        .map_err(|err| err.to_string())?;
    if let Err(err) = client_adapters::ensure_rtk_integrations(
        &state.tool_manager.rtk_entrypoint(),
        &state.tool_manager.managed_python(),
    ) {
        log::warn!("RTK integrations failed after bootstrap_runtime: {err}");
    }
    state
        .ensure_headroom_running()
        .map_err(|err| format!("bootstrap complete but failed to start headroom: {err}"))?;

    Ok(state.dashboard())
}

fn emit_bootstrap_progress(app: &AppHandle, state: &AppState) {
    let _ = app.emit("bootstrap_progress", state.bootstrap_progress());
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
                capture_bootstrap_failure(&err, kind);
                state.mark_bootstrap_failed(user_message_for(kind));
                emit_bootstrap_progress(&app_handle, &state);
                analytics::track_event(
                    &app_handle,
                    "bootstrap_failed",
                    Some(json!({ "phase": "install_runtime", "kind": kind.as_str() })),
                );
                return;
            }

            if let Err(err) = client_adapters::ensure_rtk_integrations(
                &state.tool_manager.rtk_entrypoint(),
                &state.tool_manager.managed_python(),
            ) {
                log::warn!("RTK integrations failed after start_bootstrap thread: {err}");
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
            // We probe it by hitting 6767/health — the intercept forwards to
            // 6768 and returns 502 until the backend actually responds, so a
            // 2xx confirms the full chain is live. Gatekeeper's first-launch
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
    Other,
}

impl BootstrapFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            BootstrapFailureKind::SslInterception => "ssl_interception",
            BootstrapFailureKind::NoUsableTempDir => "no_usable_tempdir",
            BootstrapFailureKind::Other => "other",
        }
    }
}

fn classify_bootstrap_failure(err: &anyhow::Error) -> BootstrapFailureKind {
    let Some(failure) = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::CommandFailure>())
    else {
        return BootstrapFailureKind::Other;
    };
    let haystack = format!("{}\n{}", failure.stdout, failure.stderr);
    if haystack.contains("CERTIFICATE_VERIFY_FAILED")
        || haystack.contains("self-signed certificate in certificate chain")
        || haystack.contains("self signed certificate in certificate chain")
    {
        BootstrapFailureKind::SslInterception
    } else if haystack.contains("No usable temporary directory found") {
        BootstrapFailureKind::NoUsableTempDir
    } else {
        BootstrapFailureKind::Other
    }
}

fn user_message_for(kind: BootstrapFailureKind) -> &'static str {
    match kind {
        BootstrapFailureKind::SslInterception => {
            "Installation failed: your network is intercepting secure connections \
             (self-signed certificate in the TLS chain), so Headroom can't verify \
             pypi.org or github.com. This usually means a corporate proxy, VPN, or \
             antivirus is inspecting HTTPS traffic. Set the REQUESTS_CA_BUNDLE \
             environment variable to your organization's CA bundle, or disable TLS \
             inspection for pypi.org, files.pythonhosted.org, and github.com, then \
             restart the app. Contact support@extraheadroom.com if you need help."
        }
        BootstrapFailureKind::NoUsableTempDir => {
            "Installation failed: Headroom can't create temporary files on this Mac. \
             This usually means your disk is full, or security software (like an MDM \
             profile or endpoint protection) is blocking writes to /tmp and \
             /var/folders. Free up disk space, restart your Mac, and try again. \
             If it still fails, contact support@extraheadroom.com."
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

    if let Some(failure) = cmd_failure {
        sentry::with_scope(
            |scope| {
                scope.set_tag("failure_kind", kind.as_str());
                scope.set_tag(
                    "endpoint_protection_suspected",
                    if endpoint_protection_suspected { "true" } else { "false" },
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
            },
            || {
                sentry::capture_message("bootstrap_failed (install_runtime)", sentry::Level::Error);
            },
        );
    } else {
        sentry::with_scope(
            |scope| {
                scope.set_tag("failure_kind", kind.as_str());
                scope.set_tag(
                    "endpoint_protection_suspected",
                    if endpoint_protection_suspected { "true" } else { "false" },
                );
                scope.set_extra("error_chain", technical_err.clone().into());
            },
            || {
                sentry::capture_message(
                    &format!("bootstrap_failed (install_runtime): {technical_err}"),
                    sentry::Level::Error,
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

    if let Some(failure) = startup_failure {
        sentry::with_scope(
            |scope| {
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

/// Probe `/readyz` on the backend port directly (bypassing the Rust
/// intercept on 6767) and classify the outcome for inclusion in a
/// give-up Sentry event. 1.5s timeout matches `is_headroom_proxy_reachable`
/// so a `timeout` here corresponds to the same wait the watchdog already
/// experienced.
fn probe_backend_readyz_outcome() -> String {
    let port = crate::backend_port::get();
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
    {
        Ok(c) => c,
        Err(err) => return format!("error: {err}"),
    };
    let url = format!("http://127.0.0.1:{port}/readyz");
    match client.get(&url).send() {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                "ok".to_string()
            } else {
                format!("http_{}", status.as_u16())
            }
        }
        Err(err) => {
            if err.is_timeout() {
                "timeout".to_string()
            } else if err.is_connect() {
                "refused".to_string()
            } else {
                format!("error: {err}")
            }
        }
    }
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
    let process_cpu_secs =
        tracked_pid.and_then(crate::state::tracked_process_cpu_time_secs);
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
    // stuck — spawn keeps erroring, or the child is alive and burning CPU
    // (likely deadlock) while the log has gone quiet. Plain network/restart
    // blips stay at Warning so they don't pollute the Error inbox.
    let cpu_deadlock_signal = report.tracked_pid.is_some()
        && report.process_cpu_secs.unwrap_or(0) >= 5
        && report.log_silent_secs.unwrap_or(0) >= 30;
    let level = if report.last_startup_error.is_some() || cpu_deadlock_signal {
        sentry::Level::Error
    } else {
        sentry::Level::Warning
    };

    sentry::with_scope(
        |scope| {
            let fp: &[&str] = &["proxy_unreachable_post_boot"];
            scope.set_fingerprint(Some(fp));
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
    let log_tail_capped = log_tail.map(|s| {
        if s.len() > 12_000 {
            let cut = s.len() - 12_000;
            format!("[truncated {cut} bytes]\n...{}", &s[cut..])
        } else {
            s.to_string()
        }
    });

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
                if endpoint_protection_suspected { "true" } else { "false" },
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
                scope.set_tag("tracked_child", if diag.tracked_child { "true" } else { "false" });
                scope.set_tag(
                    "new_proxy_log_written",
                    if diag.new_proxy_log_written { "true" } else { "false" },
                );
                scope.set_tag(
                    "proxy_port_bound",
                    if diag.proxy_port_bound { "true" } else { "false" },
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
                if let Some(err) = diag.ensure_error.as_deref() {
                    scope.set_extra("ensure_headroom_running_error", err.into());
                }
                if !diag.pip_output_tail.is_empty() {
                    // Cap aggressively — Sentry drops extras > ~16KB and the
                    // tail (where pip warnings/skips/successfully-installed
                    // lines live) is the most informative part.
                    let tail = if diag.pip_output_tail.len() > 12_000 {
                        let cut = diag.pip_output_tail.len() - 12_000;
                        format!(
                            "[truncated {cut} bytes]\n...{}",
                            &diag.pip_output_tail[cut..]
                        )
                    } else {
                        diag.pip_output_tail.clone()
                    };
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
    if lower.contains("signal=9")
        || lower.contains("killed: 9")
        || lower.contains("exit code 137")
    {
        return true;
    }
    // `Operation not permitted` paired with a freshly-installed native
    // extension path strongly implicates AV that hooks open(2)/exec(2). The
    // bare phrase appears in too many unrelated permission errors, so we
    // gate it on "site-packages" (where pip just wrote the file) or ".so" /
    // ".dylib" appearing in the same chain.
    if lower.contains("operation not permitted")
        && (lower.contains("site-packages")
            || lower.contains(".so")
            || lower.contains(".dylib"))
    {
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

pub(crate) fn endpoint_protection_hint_install() -> String {
    ENDPOINT_PROTECTION_HINT_INSTALL.to_string()
}

pub(crate) fn endpoint_protection_hint_runtime() -> String {
    ENDPOINT_PROTECTION_HINT_RUNTIME.to_string()
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
fn get_runtime_status(state: State<'_, AppState>) -> RuntimeStatus {
    state.runtime_status()
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
    Ok(state.proxy_bypass.load(std::sync::atomic::Ordering::Acquire))
}

#[tauri::command]
fn get_external_headroom_config(
    state: State<'_, AppState>,
) -> crate::models::ExternalHeadroomConfig {
    state.external_headroom_config()
}

#[tauri::command]
fn set_external_headroom_config(
    state: State<'_, AppState>,
    config: crate::models::ExternalHeadroomConfig,
) -> Result<(), String> {
    state
        .set_external_headroom_config(config)
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn detect_installed_headroom(
    state: State<'_, AppState>,
) -> Result<Option<crate::models::DetectedHeadroom>, String> {
    let mut in_path = false;
    let mut binary_path = None;

    if let Ok(output) = std::process::Command::new("headroom")
        .arg("--version")
        .output()
    {
        if output.status.success() {
            in_path = true;
            binary_path = Some("headroom".to_string());
        }
    }

    if binary_path.is_none() {
        if let Ok(output) = std::process::Command::new("rtk")
            .arg("--version")
            .output()
        {
            if output.status.success() {
                in_path = true;
                binary_path = Some("rtk".to_string());
            }
        }
    }

    if binary_path.is_none() {
        let common_paths = [
            "/opt/homebrew/bin/headroom",
            "/usr/local/bin/headroom",
            "/opt/homebrew/bin/rtk",
            "/usr/local/bin/rtk",
        ];
        for path in &common_paths {
            if std::path::Path::new(path).exists() {
                binary_path = Some(path.to_string());
                break;
            }
        }
    }

    if binary_path.is_none() {
        if let Some(home) = dirs::home_dir() {
            for name in &["headroom", "rtk"] {
                let user_local_path = home.join(".local").join("bin").join(name);
                if user_local_path.exists() {
                    binary_path = Some(user_local_path.to_string_lossy().to_string());
                    break;
                }
            }
        }
    }

    let mut running_locally = false;
    let mut running_port = None;
    let ports_to_check = [8787, 6768];

    let current_port = crate::backend_port::get();
    let check_ports = if ports_to_check.contains(&current_port) {
        vec![8787, 6768]
    } else {
        vec![8787, 6768, current_port]
    };

    for port in check_ports {
        if probe_headroom_on_port("127.0.0.1", port) {
            running_locally = true;
            running_port = Some(port);
            break;
        }
    }

    if !running_locally {
        let config = state.external_headroom_config();
        if config.enabled && probe_headroom_on_port(&config.host, config.port) {
            running_locally = true;
            running_port = Some(config.port);
        }
    }

    if binary_path.is_none() && !running_locally {
        Ok(None)
    } else {
        Ok(Some(crate::models::DetectedHeadroom {
            binary_path,
            in_path,
            running_locally,
            running_port,
        }))
    }
}

fn probe_headroom_on_port(host: &str, port: u16) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(300))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    
    let url = format!("http://{host}:{port}/stats");
    if let Ok(res) = client.get(&url).send() {
        if res.status().is_success() {
            return true;
        }
    }
    
    for path in &["/readyz", "/livez", "/health"] {
        let url = format!("http://{host}:{port}{path}");
        if let Ok(res) = client.get(&url).send() {
            if res.status().is_success() {
                return true;
            }
        }
    }
    
    false
}

#[tauri::command]
fn get_headroom_logs(
    state: State<'_, AppState>,
    max_lines: Option<usize>,
) -> Result<Vec<String>, String> {
    if state.external_headroom_config().enabled {
        let config = state.external_headroom_config();
        return Ok(vec![format!(
            "Connected to external headroom at {}:{}. Local logs are not written.",
            config.host, config.port
        )]);
    }
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
fn get_headroom_request_count() -> Option<u64> {
    fetch_proxy_request_count_stats()
}

fn fetch_proxy_request_count_stats() -> Option<u64> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .ok()?;
    for host in ["127.0.0.1", "localhost"] {
        let url = format!("http://{host}:6767/stats");
        let Ok(response) = client.get(&url).send() else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.text() else { continue };
        if let Some(count) = parse_request_count_from_stats_body(&body) {
            return Some(count);
        }
    }
    None
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
fn get_rtk_activity(
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
fn get_tool_logs(
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
fn get_claude_code_projects(state: State<'_, AppState>) -> Result<Vec<ClaudeCodeProject>, String> {
    state
        .list_claude_code_projects()
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn get_claude_usage(state: State<'_, AppState>) -> Result<ClaudeUsage, String> {
    pricing::fetch_claude_usage(&state)
}

#[tauri::command]
fn get_claude_profile(state: State<'_, AppState>) -> ClaudeAccountProfile {
    pricing::detect_claude_profile(&state)
}

#[tauri::command]
fn get_headroom_pricing_status(
    state: State<'_, AppState>,
) -> Result<HeadroomPricingStatus, String> {
    let status = pricing::get_pricing_status(&state)?;
    // Reconcile the runtime with the freshly evaluated status. Bridges the
    // gap between "user just upgraded" (subscription_active flips on) and
    // "Headroom optimization actually resumes" — without this, the pricing
    // gate's bypass flag would stay set and Python would stay down until
    // the next app launch.
    state.apply_pricing_gate_status(&status);
    Ok(status)
}

#[tauri::command]
fn request_headroom_auth_code(
    app: AppHandle,
    state: State<'_, AppState>,
    email: String,
) -> Result<HeadroomAuthCodeRequest, String> {
    let request = pricing::request_auth_code(&state, &email)?;
    analytics::track_event(&app, "auth_code_requested", None);
    Ok(request)
}

#[tauri::command]
fn verify_headroom_auth_code(
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
    state.apply_pricing_gate_status(&status);
    analytics::track_event(
        &app,
        "auth_verified",
        Some(json!({ "invite_code_used": used_invite_code })),
    );
    Ok(status)
}

#[tauri::command]
fn sign_out_headroom_account() -> Result<(), String> {
    pricing::sign_out()
}

#[tauri::command]
fn activate_headroom_account(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<HeadroomPricingStatus, String> {
    let lifetime_tokens_saved = state.dashboard().lifetime_estimated_tokens_saved;
    let status = pricing::activate_account(&state, lifetime_tokens_saved)?;
    analytics::track_event(&app, "account_activated", None);
    Ok(status)
}

#[tauri::command]
fn create_headroom_checkout_session(
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
fn change_headroom_subscription_plan(
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
fn reactivate_headroom_subscription(app: AppHandle) -> Result<(), String> {
    pricing::reactivate_subscription()?;
    analytics::track_event(&app, "subscription_reactivated", None);
    Ok(())
}

#[tauri::command]
fn get_headroom_billing_portal_url(target: Option<String>) -> Result<String, String> {
    pricing::get_billing_portal_url(target)
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
fn get_transformations_feed(limit: Option<u32>) -> TransformationFeedResponse {
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
fn get_activity_feed(state: State<'_, AppState>) -> ActivityFeedResponse {
    ActivityFeedResponse {
        tiles: state.activity_feed_snapshot(),
        proxy_reachable: crate::state::headroom_proxy_reachable(),
    }
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
fn list_live_learnings(
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
fn list_live_learnings_for_projects(
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

fn resolve_headroom_binary(state: &AppState) -> std::path::PathBuf {
    if state.external_headroom_config().enabled {
        if let Ok(output) = std::process::Command::new("headroom").arg("--version").output() {
            if output.status.success() {
                return std::path::PathBuf::from("headroom");
            }
        }
        
        for path in &["/opt/homebrew/bin/headroom", "/usr/local/bin/headroom"] {
            let p = std::path::PathBuf::from(path);
            if p.exists() {
                return p;
            }
        }
        
        if let Some(home) = dirs::home_dir() {
            let p = home.join(".local").join("bin").join("headroom");
            if p.exists() {
                return p;
            }
        }
    }
    state.tool_manager.headroom_entrypoint()
}

fn memory_export_cached(state: &State<'_, AppState>, memory_path: &Path) -> Result<String, String> {
    if let Some(cached) = state.cached_memory_export() {
        return Ok(cached);
    }
    let entrypoint = resolve_headroom_binary(state);
    if !entrypoint.exists() && entrypoint.to_string_lossy() != "headroom" {
        return Err("Headroom binary not found. Please make sure headroom is installed on your system.".into());
    }
    let stdout = run_memory_export(&entrypoint, memory_path)?;
    state.store_memory_export(stdout.clone());
    Ok(stdout)
}

#[tauri::command]
fn delete_live_learning(state: State<'_, AppState>, memory_id: String) -> Result<(), String> {
    let memory_path = headroom_memory_db_path();
    if !memory_path.exists() {
        return Err("Memory database does not exist.".into());
    }
    let entrypoint = resolve_headroom_binary(&state);
    if !entrypoint.exists() && entrypoint.to_string_lossy() != "headroom" {
        return Err("Headroom binary not found. Please make sure headroom is installed on your system.".into());
    }
    let output = Command::new(&entrypoint)
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
fn list_applied_patterns(project_path: String) -> Result<crate::models::AppliedPatterns, String> {
    Ok(read_applied_patterns_for_project(&project_path))
}

#[tauri::command]
fn list_applied_patterns_for_projects(
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
    let claude_md = std::path::PathBuf::from(project_path).join("CLAUDE.md");
    let memory_md = crate::tool_manager::claude_project_memory_file(project_path);

    crate::models::AppliedPatterns {
        claude_md: read_applied_block(&claude_md),
        memory_md: read_applied_block(&memory_md),
    }
}

#[tauri::command]
fn delete_applied_pattern(
    project_path: String,
    file_kind: String,
    section_title: String,
    bullet_text: String,
) -> Result<(), String> {
    let path = match file_kind.as_str() {
        "claude" => std::path::PathBuf::from(&project_path).join("CLAUDE.md"),
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
    std::fs::write(&path, updated).map_err(|err| format!("write {}: {err}", path.display()))?;
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
    let output = Command::new(entrypoint)
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
    let root = project_path.trim_end_matches('/');
    if root.is_empty() {
        return false;
    }
    let needle_slash = format!("{root}/");
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
fn start_headroom_learn(app: AppHandle, project_path: String) -> Result<(), String> {
    check_headroom_learn_prereqs(
        crate::state::headroom_learn_platform_message().as_deref(),
        &detect_headroom_learn_prereq_status(),
    )?;

    {
        let state: tauri::State<'_, AppState> = app.state();
        state.begin_headroom_learn_run(&project_path)?;
    }

    let app_handle = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_handle.state();
        let run = execute_headroom_learn_run(&state, &project_path);
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
fn open_headroom_dashboard() -> Result<(), String> {
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
        let mut command = Command::new("open");
        command.arg(trimmed);
        command
    };

    #[cfg(target_os = "linux")]
    {
        for opener in ["xdg-open", "gio", "kde-open5", "wslview"] {
            let mut command = Command::new(opener);
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

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", trimmed]);
        command
    };

    #[cfg(not(target_os = "linux"))]
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
fn open_external_link(url: String) -> Result<(), String> {
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

#[tauri::command]
fn apply_client_setup(app: AppHandle, client_id: String) -> Result<ClientSetupResult, String> {
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
            log::warn!("apply_client_setup: resume_runtime failed: {err:#}");
        }
    }
    match client_adapters::apply_client_setup(&client_id) {
        Ok(result) => {
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
            if !result.verification.verified {
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
            if !msg.starts_with("Automatic setup is not supported yet")
                && !msg.starts_with("Codex integration has been disabled")
            {
                sentry::capture_message(
                    &format!("client setup failed for {client_id}: {err:#}"),
                    sentry::Level::Error,
                );
            }
            Err(msg)
        }
    }
}

#[tauri::command]
fn verify_client_setup(client_id: String) -> Result<ClientSetupVerification, String> {
    client_adapters::verify_client_setup(&client_id).map_err(|err| err.to_string())
}

#[tauri::command]
fn get_client_connectors(state: State<'_, AppState>) -> Result<Vec<ClientConnectorStatus>, String> {
    client_adapters::list_client_connectors(&state.cached_clients()).map_err(|err| err.to_string())
}

#[tauri::command]
fn disable_client_setup(app: AppHandle, client_id: String) -> Result<(), String> {
    client_adapters::disable_client_setup(&client_id).map_err(|err| err.to_string())?;
    analytics::track_event(
        &app,
        "client_setup_disabled",
        Some(json!({ "client_id": client_id })),
    );
    Ok(())
}

#[tauri::command]
fn clear_client_setups() -> Result<(), String> {
    client_adapters::clear_client_setups().map_err(|err| err.to_string())
}

#[tauri::command]
fn pause_headroom(app: AppHandle) -> Result<(), String> {
    let state: tauri::State<'_, AppState> = app.state();
    state.set_runtime_paused(true);
    state.stop_headroom();
    client_adapters::clear_client_setups().map_err(|err| err.to_string())?;
    analytics::track_event(&app, "runtime_paused", None);
    Ok(())
}

#[tauri::command]
fn start_headroom(app: AppHandle) -> Result<(), String> {
    let state: tauri::State<'_, AppState> = app.state();
    state.resume_runtime().map_err(|err| err.to_string())?;
    std::thread::spawn(|| {
        client_adapters::restore_client_setups();
    });
    analytics::track_event(&app, "runtime_resumed", None);
    Ok(())
}

#[tauri::command]
fn hide_launcher_animated(app: AppHandle) {
    // The launcher close animation now lives in the webview/CSS layer.
    // Keep the backend hide on the straightforward window path instead of
    // mutating window geometry from a background thread.
    let _ = hide_launcher_window(&app);
}

#[tauri::command]
fn get_autostart_enabled(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|err| err.to_string())
}

#[tauri::command]
fn set_autostart_enabled(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|err| err.to_string())?;
    } else {
        manager.disable().map_err(|err| err.to_string())?;
    }
    manager.is_enabled().map_err(|err| err.to_string())
}

#[tauri::command]
fn uninstall_and_quit(app: AppHandle) -> Result<Vec<String>, String> {
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.stop_headroom();
    }

    // Turn off the login item if it was ever enabled, so the system stops
    // listing Headroom as a background item even if the user later reinstalls.
    let _ = app.autolaunch().disable();

    let removed = client_adapters::perform_full_cleanup();

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
fn quit_headroom(app: AppHandle) {
    exit_headroom(&app, QuitSource::SettingsButton);
}

fn launched_from_autostart() -> bool {
    std::env::args().any(|arg| arg == AUTOSTART_LAUNCH_ARG)
}

fn exit_headroom(app: &AppHandle, source: QuitSource) {
    let runtime_paused = {
        let state: tauri::State<'_, AppState> = app.state();
        let runtime_paused = state.runtime_is_paused();
        state.stop_headroom();
        let _ = client_adapters::clear_client_setups();
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

pub fn run() {
    let _sentry = sentry::init((
        SENTRY_DSN.unwrap_or(""),
        sentry::ClientOptions {
            release: sentry::release_name!(),
            attach_stacktrace: true,
            ..Default::default()
        },
    ));

    // Initialize the panic-safe file logger after Sentry so warn!/error!
    // records flow into Sentry too. Failure here cannot abort startup.
    let _ = logging::init();

    #[cfg(target_os = "linux")]
    {
        let has_display = std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok();
        if !has_display {
            log::error!(
                "Headroom requires a graphical display. Set DISPLAY or WAYLAND_DISPLAY before launching."
            );
            std::process::exit(1);
        }
    }

    let state = AppState::new().expect("failed to create app state");

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second launch: focus the existing window and exit the new process.
            let _ = show_launcher_window(app);
        }))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args([AUTOSTART_LAUNCH_ARG])
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init());

    builder
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(ActivationPolicy::Accessory);
                app.set_dock_visibility(false);
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
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            move || {
                                let state: tauri::State<'_, AppState> = app_handle.state();
                                pricing::warm_and_push_identity(&state);
                            },
                        ));
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

            // Start the intercept layer before anything else touches port 6767.
            proxy_intercept::spawn(
                std::sync::Arc::clone(&state.claude_bearer_token),
                std::sync::Arc::clone(&state.proxy_bypass),
                fresh_bearer_tx,
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
                // `application:openURLs:` handler, which is `extern "C"` —
                // any panic that escapes here aborts the whole process via
                // `panic_cannot_unwind`. Wrap the body in `catch_unwind` so
                // an internal failure degrades gracefully instead.
                let deep_link_app = deep_link_app.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for url in event.urls() {
                        if url.scheme() == "headroom" {
                            let app_handle = deep_link_app.clone();
                            let _ = show_launcher_window(&app_handle);
                            // Run the reconciliation on a worker thread — the
                            // deep-link callback is on the main thread and we
                            // don't want pricing's blocking HTTP call there.
                            std::thread::spawn(move || {
                                let state: tauri::State<'_, AppState> = app_handle.state();
                                match pricing::get_pricing_status(&state) {
                                    Ok(status) => {
                                        state.apply_pricing_gate_status(&status);
                                        let _ = app_handle.emit("pricing-refreshed", &status);
                                    }
                                    Err(err) => {
                                        sentry::capture_message(
                                            &format!("deep link pricing refresh failed: {err}"),
                                            sentry::Level::Warning,
                                        );
                                    }
                                }
                            });
                            // Only handle the first headroom:// URL in the batch.
                            break;
                        }
                    }
                }));
                if result.is_err() {
                    sentry::capture_message(
                        "deep link callback panicked",
                        sentry::Level::Error,
                    );
                }
            });
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
            get_research_candidates,
            bootstrap_runtime,
            start_bootstrap,
            get_bootstrap_progress,
            get_runtime_upgrade_progress,
            retry_runtime_upgrade,
            retry_runtime_upgrade_with_rebuild,
            dismiss_runtime_upgrade_failure,
            get_runtime_status,
            get_headroom_logs,
            get_headroom_request_count,
            get_rtk_activity,
            get_tool_logs,
            get_claude_code_projects,
            get_claude_usage,
            get_claude_profile,
            get_headroom_pricing_status,
            request_headroom_auth_code,
            verify_headroom_auth_code,
            sign_out_headroom_account,
            activate_headroom_account,
            create_headroom_checkout_session,
            change_headroom_subscription_plan,
            reactivate_headroom_subscription,
            get_headroom_billing_portal_url,
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
            get_client_connectors,
            disable_client_setup,
            clear_client_setups,
            pause_headroom,
            start_headroom,
            track_analytics_event,
            show_dashboard_window,
            open_headroom_dashboard,
            open_external_link,
            submit_contact_request,
            hide_launcher_animated,
            complete_setup_wizard,
            reset_setup_wizard,
            get_autostart_enabled,
            set_autostart_enabled,
            uninstall_and_quit,
            quit_headroom,
            get_external_headroom_config,
            set_external_headroom_config,
            detect_installed_headroom,
            #[cfg(debug_assertions)]
            debug_force_proxy_bypass
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Tear down the proxy on every exit path (Cmd-Q, dock quit, signal,
            // or our explicit quit/restart commands). Without this, the proxy
            // outlives the desktop and the next launch reuses an orphan.
            if matches!(
                event,
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
            ) {
                let state: tauri::State<'_, AppState> = app.state();
                state.stop_headroom();
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

fn is_prerelease_version(version: &str) -> bool {
    version.contains('-')
}

fn beta_channel_enabled_from(env: Option<&str>, sentinel_exists: bool) -> bool {
    let env_yes = matches!(
        env.map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
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

pub(crate) fn detect_headroom_learn_prereq_status() -> HeadroomLearnPrereqStatus {
    let path = claude_cli::detect_claude_cli();
    HeadroomLearnPrereqStatus {
        claude_cli_available: path.is_some(),
        claude_cli_path: path.map(|p| p.display().to_string()),
    }
}

fn check_headroom_learn_prereqs(
    platform_disabled_reason: Option<&str>,
    prereq: &HeadroomLearnPrereqStatus,
) -> Result<(), String> {
    if let Some(reason) = platform_disabled_reason {
        return Err(reason.to_string());
    }
    if !prereq.claude_cli_available {
        return Err("Install the Claude Code CLI (`claude`) to enable Headroom Learn.".into());
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

fn execute_headroom_learn_run(state: &AppState, project_path: &str) -> HeadroomLearnRunResult {
    let project_name = Path::new(project_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(project_path);
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

    let mut command = Command::new(&entrypoint);
    command
        .arg("learn")
        .arg("--project")
        .arg(project_path)
        .arg("--apply")
        // Headroom-desktop only manages CLAUDE.md / MEMORY.md for Claude Code.
        // Skip codex / gemini analysis so we don't burn LLM budget producing
        // recommendations the desktop won't apply anywhere.
        .arg("--agent")
        .arg("claude")
        .current_dir(project_path)
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        .env("HEADROOM_LEARN_CLI", "claude")
        // Force the claude CLI backend: the analyzer picks LiteLLM over
        // HEADROOM_LEARN_CLI when any of these keys is set in the parent env.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        // Don't pin ANTHROPIC_MODEL here: it's a LiteLLM identifier that the
        // analyzer never reads on the CLI path (litellm is skipped when
        // API keys are stripped above). Worse, it's inherited by the spawned
        // `claude -p` subprocess, where Claude Code's CLI does honor it —
        // and "claude-sonnet-4-6" is not a valid Claude Code model alias,
        // which routes the call to a slow/hung path past 120s. Letting
        // claude -p use its default model fixes the hang.
        .env_remove("ANTHROPIC_MODEL");
    if let Some(claude_path) = claude_cli::detect_claude_cli() {
        if let Some(dir) = claude_path.parent() {
            let existing = std::env::var("PATH").unwrap_or_default();
            let augmented = if existing.is_empty() {
                dir.display().to_string()
            } else {
                format!("{}:{}", dir.display(), existing)
            };
            command.env("PATH", augmented);
        }
    }
    let output = command.output();

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
                    (
                        format!("headroom learn could not produce recommendations for {project_name}."),
                        false,
                        Some(warnings),
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
                let signature: String = signature_source
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("no output")
                    .chars()
                    .take(160)
                    .collect();
                let stderr_head: String = stderr.chars().take(2000).collect();
                let stdout_head: String = stdout.chars().take(2000).collect();
                let claude_cli_path = claude_cli::detect_claude_cli()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not_found".into());
                let summary_msg = format!(
                    "headroom learn failed (exit={exit_code_str}) {signature}"
                );
                let fingerprint: [&str; 3] =
                    ["headroom_learn", exit_code_str.as_str(), signature.as_str()];
                sentry::with_scope(
                    |scope| {
                        scope.set_tag("flow", "headroom_learn");
                        scope.set_tag("exit_code", &exit_code_str);
                        scope.set_extra("exit_status", output.status.to_string().into());
                        scope.set_extra(
                            "signal",
                            signal_num
                                .map(|s| s.to_string().into())
                                .unwrap_or(serde_json::Value::Null),
                        );
                        scope.set_extra("output_tail", fail_tail.clone().into());
                        scope.set_extra("stderr_head", stderr_head.into());
                        scope.set_extra("stdout_head", stdout_head.into());
                        scope.set_extra("claude_cli_path", claude_cli_path.into());
                        scope.set_extra("project_name", project_name.to_string().into());
                        scope.set_fingerprint(Some(fingerprint.as_slice()));
                    },
                    || {
                        sentry::capture_message(&summary_msg, sentry::Level::Error);
                    },
                );
                (
                    format!("headroom learn failed for {project_name}."),
                    false,
                    Some(format!(
                        "headroom learn exited with {}.\n{}",
                        output.status, fail_tail
                    )),
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

    let log_path = state.tool_manager.headroom_learn_log_path(project_path);
    let log_content = format!(
        "[{}] headroom learn --project {}\nstatus: {}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}\n",
        Utc::now().to_rfc3339(),
        project_path,
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

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = tauri::menu::MenuItem::with_id(app, "show", "Show Headroom", true, None::<&str>)?;
    let settings = tauri::menu::MenuItem::with_id(app, "settings", "Settings...", true, None::<&str>)?;
    let quit = tauri::menu::MenuItem::with_id(app, "quit", "Quit Headroom", true, None::<&str>)?;
    let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
    let menu = tauri::menu::Menu::with_items(app, &[&show, &settings, &separator, &quit])?;
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
            "settings" => {
                let _ = hide_launcher_window(app);
                let _ = show_main_window(app, None);
                let _ = app.emit("open-settings", &());
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
        let mut unhealthy_streak: u8 = 0;
        let mut last_connector_check = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now);
        let mut cached_connector_enabled: bool = client_adapters::is_claude_code_enabled();

        loop {
            // Re-check the Claude connector at most every ~2s, regardless of
            // whether the tick rate is booting-fast (260ms) or idle-slow
            // (1500ms). Time-based instead of tick-count based so the cadence
            // stays correct across the adaptive sleep below.
            if last_connector_check.elapsed() >= std::time::Duration::from_secs(2) {
                cached_connector_enabled = client_adapters::is_claude_code_enabled();
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
                    // Runtime should be up (installed, not paused, not booting)
                    // but the proxy isn't answering. Treat as unhealthy so the
                    // user has a visible signal the watchdog is working on it.
                    TrayRuntimeVisual::Unhealthy
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
                    TrayRuntimeVisual::Paused => "Headroom — paused (Claude Code running normally)",
                    TrayRuntimeVisual::Unhealthy => {
                        "Headroom — proxy unreachable, attempting restart"
                    }
                    TrayRuntimeVisual::Disconnected => "Headroom — Claude Code not connected",
                    TrayRuntimeVisual::Off => "Headroom — off",
                };

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
                                    "Claude Code is disconnected — open Headroom to re-enable.",
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
                    if let Err(err) = tray.set_tooltip(Some(tooltip)) {
                        log::warn!("tray: set_tooltip failed: {err}");
                    }
                    last_tooltip = Some(tooltip.to_string());
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
        let mut last_tick = std::time::Instant::now();
        // Set after a forced kill+restart of a hung process. Prevents the
        // hung-kill path from looping forever if the new process also hangs:
        // on the second trip through MAX_CONSECUTIVE_FAILURES we fall through
        // to the permanent give-up path instead. Resets when the proxy
        // recovers so a later hang triggers another rescue attempt.
        let mut hung_kill_attempted = false;

        loop {
            std::thread::sleep(POLL);
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_tick);
            last_tick = now;
            let just_resumed = elapsed > RESUME_THRESHOLD;

            let state: tauri::State<'_, AppState> = app.state();
            let runtime = state.runtime_status();

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
                // End of "down episode" — re-arm Sentry capture so a future
                // crash fires a fresh event.
                WATCHDOG_DOWN_CAPTURED.store(false, Ordering::Release);
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
                consecutive_failures = 0;
                continue;
            }

            consecutive_failures = consecutive_failures.saturating_add(1);
            log::info!(
                "watchdog: proxy unreachable (failure {consecutive_failures}/{MAX_CONSECUTIVE_FAILURES}, bypass={bypass_active}), attempting restart"
            );

            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
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
                let backend_readyz_outcome = probe_backend_readyz_outcome();
                if backend_readyz_outcome == "ok" {
                    log::info!(
                        "watchdog: backend /readyz answers ok after {consecutive_failures} intercept failures; skipping auto-pause and resetting counter"
                    );
                    consecutive_failures = 0;
                    continue;
                }
                // Hung process: TCP port is accepting connections (the process
                // is alive) but /readyz never responds. ensure_headroom_running
                // returns Ok immediately when try_wait says the child is still
                // alive, so the three restart attempts above were all no-ops.
                // Kill the stuck process and start fresh before giving up
                // permanently. We only do this once per down episode
                // (hung_kill_attempted) so a persistently-hung new process
                // doesn't loop; it falls through to the give-up path below.
                if backend_readyz_outcome == "timeout" && !hung_kill_attempted {
                    log::info!(
                        "watchdog: backend /readyz hung (timeout) after {consecutive_failures} failures; force-killing and restarting"
                    );
                    hung_kill_attempted = true;
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
                // Capture once per down episode, BEFORE stop_headroom tears
                // down the tracked child and the proxy log handle, so the
                // exit status and log tail reflect the failure we're about
                // to recover from.
                capture_watchdog_give_up(
                    &*state,
                    consecutive_failures,
                    bypass_active,
                    backend_readyz_outcome,
                );
                // Flip bypass FIRST so the Rust intercept passes new
                // requests straight through to Anthropic instead of returning
                // 502 in the window between Python being torn down and the
                // user noticing. See proxy_intercept.rs:161 — without this,
                // every request lands on the unreachable backend branch.
                state
                    .proxy_bypass
                    .store(true, std::sync::atomic::Ordering::Release);
                state.set_runtime_paused(true);
                state.stop_headroom();
                analytics::track_event(&app, "runtime_auto_paused", None);
                let _ = show_notification_impl(
                    &app,
                    "Headroom paused",
                    "Headroom couldn't restart its proxy. Requests are passing through unmodified — open Headroom to retry.",
                    Some("connectors".into()),
                );
                consecutive_failures = 0;
                continue;
            }

            // Otherwise try to bring it back.
            match state.ensure_headroom_running() {
                Ok(()) => port_conflict::note_proxy_started(&app),
                Err(err) => {
                    log::warn!("watchdog: ensure_headroom_running failed: {err:#}");
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
            .map(|p| p.estimated_savings_usd)
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

    (out, new_w, base_h)
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
    let is_external = {
        let config = state.external_headroom_config();
        config.enabled
    };
    if !is_external && !state.tool_manager.python_runtime_installed() {
        return false;
    }
    // Only require wizard completion on the very first launch. Existing users
    // (launch_count > 1) already went through setup before this flag existed.
    state.setup_wizard_complete() || state.launch_count() > 1
}

#[tauri::command]
fn complete_setup_wizard(state: tauri::State<'_, AppState>) {
    state.mark_setup_wizard_complete();
}

#[tauri::command]
fn reset_setup_wizard(state: tauri::State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    state.reset_setup_wizard();
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
    if let Some(launcher) = app.get_webview_window("launcher") {
        let _ = show_launcher_window(&app);
        let _ = launcher.eval("window.location.reload()");
    }
    Ok(())
}

fn show_main_window(app: &AppHandle, anchor_rect: Option<Rect>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Err(tauri::Error::WebviewNotFound);
    };

    if let Some(rect) = anchor_rect {
        position_tray_window(&window, rect)?;
    }

    window.show()?;
    let _ = window.unminimize();
    window.set_focus()?;
    Ok(())
}

fn show_launcher_window(app: &AppHandle) -> tauri::Result<()> {
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
        if window.is_visible()? {
            window.hide()?;
        }
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
    use super::{
        aggregate_live_learnings, app_quit_requested_properties, app_update_notification_body,
        beta_channel_enabled_from, build_release_updater_config, build_watchdog_give_up_report,
        check_headroom_learn_prereqs, classify_bootstrap_failure, classify_upgrade_error,
        compute_tray_window_position, count_memories_created_today, debounced_tray_runtime_visual,
        delete_applied_pattern, empty_live_learnings_for_projects, extract_llm_failure_warnings,
        fetch_transformations_feed_from, install_pending_update,
        noop_app_update_progress_emitter, is_disk_full_signal,
        is_endpoint_protection_signal, is_port_conflict_failure, is_prerelease_version,
        lifetime_token_milestone_kind,
        parse_live_learnings, parse_request_count_from_stats_body, parse_updater_endpoint_list,
        pattern_matches_project, physical_rect_from_rect, read_applied_patterns_for_project,
        resolve_release_updater_config, select_updater_endpoints, store_checked_update,
        watchdog_should_be_up, AppUpdateProgress, AppUpdateProgressEmitter, AvailableAppUpdate,
        BootstrapFailureKind, HeadroomLearnPrereqStatus, InstallPendingUpdateFuture,
        InstallableAppUpdate, MonitorBounds, PhysicalRect, QuitSource, TrayRuntimeVisual,
        DEFAULT_UPDATER_ENDPOINT, DEFAULT_UPDATER_PUBLIC_KEY,
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

    fn sample_available_update(version: &str) -> AvailableAppUpdate {
        AvailableAppUpdate {
            current_version: "0.2.9".into(),
            version: version.into(),
            published_at: Some("2026-04-02T12:00:00Z".into()),
            notes: Some("Bug fixes.".into()),
        }
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
        assert_eq!(select_updater_endpoints(None, Some("https://staging"), false), None);
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
        let source = include_str!("lib.rs");
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
    fn check_headroom_learn_prereqs_passes_when_cli_available() {
        let prereq = HeadroomLearnPrereqStatus {
            claude_cli_available: true,
            claude_cli_path: Some("/usr/bin/claude".into()),
        };
        assert!(check_headroom_learn_prereqs(None, &prereq).is_ok());
    }

    #[test]
    fn check_headroom_learn_prereqs_returns_install_message_when_cli_missing() {
        let prereq = HeadroomLearnPrereqStatus {
            claude_cli_available: false,
            claude_cli_path: None,
        };
        let err = check_headroom_learn_prereqs(None, &prereq).unwrap_err();
        assert!(
            err.contains("Install the Claude Code CLI"),
            "expected install hint, got: {err}"
        );
    }

    #[test]
    fn check_headroom_learn_prereqs_prefers_platform_message_over_cli_check() {
        let prereq = HeadroomLearnPrereqStatus {
            claude_cli_available: false,
            claude_cli_path: None,
        };
        let err = check_headroom_learn_prereqs(Some("Linux not supported"), &prereq).unwrap_err();
        assert_eq!(err, "Linux not supported");
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
    fn classify_bootstrap_failure_returns_other_for_unrelated_command_errors() {
        let err: anyhow::Error =
            make_command_failure("ConnectionResetError: [Errno 54] Connection reset by peer")
                .into();
        assert!(matches!(
            classify_bootstrap_failure(&err),
            BootstrapFailureKind::Other
        ));
    }

    #[test]
    fn classify_bootstrap_failure_returns_other_when_no_command_failure_in_chain() {
        let err = anyhow::anyhow!("network is down");
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
        let titles: Vec<&str> = result
            .claude_md
            .iter()
            .map(|s| s.title.as_str())
            .collect();
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

    #[test]
    fn delete_applied_pattern_removes_one_bullet_and_keeps_section() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());

        delete_applied_pattern(
            tmp.path().to_str().unwrap().to_string(),
            "claude".into(),
            "First Section".into(),
            "First bullet.".into(),
        )
        .expect("delete bullet");

        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        let first = result
            .claude_md
            .iter()
            .find(|s| s.title == "First Section")
            .expect("First Section preserved when one of two bullets deleted");
        assert_eq!(first.bullets, vec!["Second bullet.".to_string()]);
        assert!(
            result
                .claude_md
                .iter()
                .any(|s| s.title == "Second Section"),
            "other sections preserved"
        );
    }

    #[test]
    fn delete_applied_pattern_drops_last_section_and_keeps_block_parseable() {
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
        .expect("delete bullet");

        let result = read_applied_patterns_for_project(tmp.path().to_str().unwrap());
        let titles: Vec<&str> = result
            .claude_md
            .iter()
            .map(|s| s.title.as_str())
            .collect();
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

    #[test]
    fn delete_applied_pattern_rejects_unknown_file_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_claude_md_with_headroom_block(tmp.path());

        let err = delete_applied_pattern(
            tmp.path().to_str().unwrap().to_string(),
            "garbage".into(),
            "First Section".into(),
            "First bullet.".into(),
        )
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
        assert!(is_endpoint_protection_signal(
            "headroom: Killed: 9"
        ));
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
    fn is_disk_full_signal_matches_pip_enospc_failures() {
        assert!(is_disk_full_signal(
            "ERROR: Could not install packages due to an OSError: [Errno 28] No space left on device"
        ));
        assert!(is_disk_full_signal("OSError: [Errno 28] No space left on device"));
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
        let err = anyhow::anyhow!(
            "network unreachable during install — child exited with signal=9"
        );
        let hint = classify_upgrade_error(&err).expect("must classify");
        assert!(
            hint.contains("endpoint protection"),
            "expected EDR hint, got: {hint}"
        );
    }
}
