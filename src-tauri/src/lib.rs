mod activity_facts;
mod analytics;
mod bearer;
mod claude_cli;
mod client_adapters;
mod device;
mod insights;
mod keychain;
mod models;
mod notifications;
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
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;
use tauri::{Emitter, Manager};
use tauri::{
    AppHandle, PhysicalPosition, PhysicalSize, Position, Rect, State, Window, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::models::{
    ActivityEvent, ActivityFeedResponse, BillingPeriod, BootstrapProgress, ClaudeAccountProfile,
    ClaudeCodeProject, ClaudeUsage, ClientConnectorStatus, ClientSetupResult,
    ClientSetupVerification, DashboardState, HeadroomAuthCodeRequest, HeadroomLearnPrereqStatus,
    HeadroomLearnStatus, HeadroomPricingStatus, HeadroomSubscriptionTier, MemoryFeedEvent,
    ResearchCandidate, RuntimeStatus, RuntimeUpgradeProgress, TransformationFeedResponse,
};
use crate::state::AppState;

const UPDATER_PUBLIC_KEY: Option<&str> = option_env!("HEADROOM_UPDATER_PUBLIC_KEY");
const UPDATER_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_ENDPOINTS");
const UPDATER_STAGING_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_STAGING_ENDPOINTS");
const SENTRY_DSN: Option<&str> = option_env!("HEADROOM_SENTRY_DSN");
const DEFAULT_UPDATER_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk3QkUyNEU0MjVBMkRDM0MKUldRODNLSWw1Q1MrbC93MitlYTVoUXViSXJQNGVQWDdBRXA0Qkl4WGtpSEttNm5YTDB3QWtncEoK";
const DEFAULT_UPDATER_ENDPOINT: &str =
    "https://github.com/gglucass/headroom-desktop/releases/latest/download/latest.json";
const AUTOSTART_LAUNCH_ARG: &str = "--autostart";
const HEADROOM_DASHBOARD_URL: &str = "http://127.0.0.1:6767/dashboard";
const MAIN_WINDOW_WIDTH: u32 = 760;
const MAIN_WINDOW_HEIGHT: u32 = 560;
const TRAY_WINDOW_VERTICAL_GAP: i32 = 10;

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

trait InstallableAppUpdate: Send {
    fn metadata(&self) -> AvailableAppUpdate;
    fn install(self) -> InstallPendingUpdateFuture;
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

    fn install(self) -> InstallPendingUpdateFuture {
        Box::pin(async move {
            self.0
                .download_and_install(|_, _| {}, || {})
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

        state.record_activity_milestones(&pending_milestones);

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
            if let Some(payload) =
                notifications::notification_for_token_milestone(*milestone_tokens_saved)
            {
                let _ = show_notification_impl(&app, &payload.title, &payload.body, payload.action);
            }
        }

        for milestone_usd in &pending_milestones.usd {
            analytics::track_event(
                &app,
                "lifetime_usd_savings_milestone_reached",
                Some(json!({
                    "milestone_usd": *milestone_usd,
                    "lifetime_estimated_savings_usd": dashboard.lifetime_estimated_savings_usd,
                    "lifetime_requests": dashboard.lifetime_requests,
                    "launch_count": state.launch_count(),
                    "launch_experience": state.launch_experience_label()
                })),
            );
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
    match release_updater_config(&current_version) {
        Ok(Some(config)) => AppUpdateConfiguration {
            enabled: true,
            current_version,
            endpoint_count: config.endpoints.len(),
            configuration_error: None,
        },
        Ok(None) => AppUpdateConfiguration {
            enabled: false,
            current_version,
            endpoint_count: 0,
            configuration_error: None,
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
    let config = release_updater_config(&current_version)?
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
async fn install_app_update(pending_update: State<'_, PendingAppUpdate>) -> Result<(), String> {
    install_pending_update(&pending_update.0).await
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

async fn install_pending_update<U>(pending_update: &Mutex<Option<U>>) -> Result<(), String>
where
    U: InstallableAppUpdate,
{
    let update = {
        let mut pending = pending_update.lock();
        pending
            .take()
            .ok_or_else(|| "No downloaded update is ready to install.".to_string())?
    };

    update.install().await
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
    app.request_restart();
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
    show_notification_impl(app, "Headroom Update Available", &body, Some("update".into()))
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
    action: Option<String>,
) -> Result<(), String> {
    let app_handle = app.clone();
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
        let response = mac_notification_sys::Notification::new()
            .title(&title)
            .message(&body)
            .wait_for_click(true)
            .send();
        let clicked = matches!(
            response,
            Ok(mac_notification_sys::NotificationResponse::Click)
                | Ok(mac_notification_sys::NotificationResponse::ActionButton(_))
                | Ok(mac_notification_sys::NotificationResponse::Reply(_))
        );
        if clicked {
            let _ = show_main_window(&app_handle, None);
            let _ = app_handle.emit(
                "notification-clicked",
                json!({ "action": action }),
            );
        }
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
        eprintln!("failed to ensure RTK integrations after bootstrap: {err}");
        sentry::capture_message(
            &format!("RTK integrations failed after bootstrap_runtime: {err}"),
            sentry::Level::Warning,
        );
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
                eprintln!("failed to ensure RTK integrations after bootstrap: {err}");
                sentry::capture_message(
                    &format!("RTK integrations failed after start_bootstrap thread: {err}"),
                    sentry::Level::Warning,
                );
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
            eprintln!("headroom auto-start failed after bootstrap: {err}");
            capture_headroom_start_failure("headroom auto-start failed after bootstrap", &err);
            // Fall through so the user is not stuck on the install loader
            // indefinitely. The test screen will show a retry option.
        } else {
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
    Other,
}

impl BootstrapFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            BootstrapFailureKind::SslInterception => "ssl_interception",
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
        BootstrapFailureKind::Other => {
            "Installation failed: Headroom couldn't download a required file. \
             Please check your internet connection and try restarting the app. \
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

    if let Some(failure) = cmd_failure {
        sentry::with_scope(
            |scope| {
                scope.set_tag("failure_kind", kind.as_str());
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
                scope.set_extra("stdout", failure.stdout.clone().into());
                scope.set_extra("stderr", failure.stderr.clone().into());
                scope.set_extra("error_chain", technical_err.clone().into());
            },
            || {
                sentry::capture_message(
                    "bootstrap_failed (install_runtime)",
                    sentry::Level::Error,
                );
            },
        );
    } else {
        sentry::with_scope(
            |scope| {
                scope.set_tag("failure_kind", kind.as_str());
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

/// Report a headroom proxy startup failure to Sentry. If the error chain
/// contains a `HeadroomStartupFailure`, its log tail, log path, and invocation
/// are sent as structured `extra` fields so we can see what Python printed
/// before failing to bind the port.
pub(crate) fn capture_headroom_start_failure(context: &str, err: &anyhow::Error) {
    let technical_err = format!("{err:#}");
    let startup_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::HeadroomStartupFailure>());

    let headline = format!("{context}: {technical_err}");
    let truncated = headline.chars().take(400).collect::<String>();

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

/// Report a runtime upgrade failure to Sentry. `phase` is "install" for
/// pip/smoke-test failures, "boot_validation" for "installed but didn't boot".
pub(crate) fn capture_upgrade_failure(err: &anyhow::Error, restored: bool, phase: &str) {
    let technical_err = format!("{err:#}");
    let cmd_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::CommandFailure>());

    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "runtime_upgrade");
            scope.set_tag("upgrade_phase", phase);
            scope.set_extra("rollback_restored", restored.into());
            scope.set_extra("error_chain", technical_err.clone().into());
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
                scope.set_extra("stdout", failure.stdout.clone().into());
                scope.set_extra("stderr", failure.stderr.clone().into());
            }
        },
        || {
            sentry::capture_message(
                &format!("runtime_upgrade_failed ({phase})"),
                sentry::Level::Error,
            );
        },
    );
}

/// Map common runtime-upgrade failure modes to a short user-facing hint.
pub(crate) fn classify_upgrade_error(err: &anyhow::Error) -> Option<String> {
    let chain = format!("{err:#}").to_ascii_lowercase();
    if chain.contains("network") || chain.contains("timed out") || chain.contains("dns")
        || chain.contains("connection refused") || chain.contains("could not resolve")
    {
        return Some("Couldn't reach PyPI. Check your network and retry.".into());
    }
    if chain.contains("no space") || chain.contains("disk full") || chain.contains("enospc") {
        return Some("Not enough disk space to install the update. Free up space and retry.".into());
    }
    if chain.contains("sha256") || chain.contains("checksum") || chain.contains("digest") {
        return Some("The downloaded wheel's checksum didn't match. Retry to redownload.".into());
    }
    if chain.contains("import") && chain.contains("smoke test") {
        return Some("The new Headroom version couldn't be imported. Try retrying or reinstalling.".into());
    }
    if chain.contains("resolution") || chain.contains("no matching distribution") {
        return Some("Pip couldn't resolve dependencies for the new version. Please report this.".into());
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
        state.retry_runtime_upgrade(&app_clone);
    });
    Ok(())
}

#[tauri::command]
fn get_runtime_status(state: State<'_, AppState>) -> RuntimeStatus {
    state.runtime_status()
}

#[tauri::command]
fn get_headroom_logs(
    state: State<'_, AppState>,
    max_lines: Option<usize>,
) -> Result<Vec<String>, String> {
    let limit = max_lines.unwrap_or(120).clamp(20, 500);
    state
        .tool_manager
        .read_headroom_log_tail(limit)
        .map_err(|err| err.to_string())
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
    pricing::get_pricing_status(&state)
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
fn get_headroom_billing_portal_url() -> Result<String, String> {
    pricing::get_billing_portal_url()
}

#[tauri::command]
fn get_headroom_learn_status(
    state: State<'_, AppState>,
    project_path: Option<String>,
) -> HeadroomLearnStatus {
    state.headroom_learn_status(project_path.as_deref())
}

#[tauri::command]
fn get_headroom_learn_prereq_status() -> HeadroomLearnPrereqStatus {
    detect_headroom_learn_prereq_status()
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

#[tauri::command]
fn get_activity_feed(
    app: AppHandle,
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> ActivityFeedResponse {
    let limit = limit.unwrap_or(50).min(500);
    let transformations = fetch_transformations_feed(limit).ok();
    let memory_path = headroom_memory_db_path();
    let memories = if memory_path.exists() {
        match memory_export_cached(&state, &memory_path) {
            Ok(stdout) => parse_memory_export(&stdout, limit as usize).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let mut fresh_events: Vec<ActivityEvent> = Vec::new();

    if let Some(event) = state.maybe_emit_weekly_recap() {
        fresh_events.push(event);
    }

    let observation = match transformations.as_ref() {
        Some(feed) => state.observe_activity_from_transformations(&feed.transformations),
        None => crate::state::ActivityObservation::default(),
    };
    fresh_events.extend(observation.fresh.iter().cloned());

    if let Some(event) = state.observe_learnings_count(memories.len()) {
        fresh_events.push(event);
    }

    for payload in notifications::collect_notification_payloads(&fresh_events) {
        let _ = show_notification_impl(&app, &payload.title, &payload.body, payload.action);
    }

    let memory_available = memory_path.exists();
    build_activity_feed_response(
        transformations,
        memories,
        observation.recent,
        memory_available,
        limit as usize,
    )
}

/// Pure merge/sort core of `get_activity_feed`. Combines transformations,
/// memories, and synthesized activity events into a single chronological
/// stream sorted DESC by timestamp, capped at `limit` events.
fn build_activity_feed_response(
    transformations: Option<TransformationFeedResponse>,
    memories: Vec<MemoryFeedEvent>,
    synthetic_events: Vec<ActivityEvent>,
    memory_available: bool,
    limit: usize,
) -> ActivityFeedResponse {
    let mut events: Vec<ActivityEvent> = Vec::new();
    let (proxy_reachable, log_full_messages) = match transformations {
        Some(t) => {
            for ev in t.transformations {
                events.push(ActivityEvent::Transformation(ev));
            }
            (t.proxy_reachable, t.log_full_messages)
        }
        None => (false, false),
    };
    for m in memories {
        events.push(ActivityEvent::Memory(m));
    }
    for e in synthetic_events {
        events.push(e);
    }

    events.sort_by(|a, b| activity_event_timestamp(b).cmp(&activity_event_timestamp(a)));
    let mut events = coalesce_rtk_batches(events);
    events.truncate(limit);

    ActivityFeedResponse {
        events,
        log_full_messages,
        proxy_reachable,
        memory_available,
    }
}

/// Merge adjacent `RtkBatch` events within a 10-minute window into a single
/// aggregated row. The feed polls every 2s, so an active RTK session produces
/// many small batches; coalescing keeps the feed readable without touching the
/// persistent fact store. Expects `events` sorted DESC by timestamp.
fn coalesce_rtk_batches(events: Vec<ActivityEvent>) -> Vec<ActivityEvent> {
    let window = chrono::Duration::minutes(10);
    let mut out: Vec<ActivityEvent> = Vec::with_capacity(events.len());
    for ev in events {
        if let ActivityEvent::RtkBatch(curr) = &ev {
            if let Some(ActivityEvent::RtkBatch(prev)) = out.last_mut() {
                if prev.observed_at.signed_duration_since(curr.observed_at) <= window {
                    prev.commands_delta =
                        prev.commands_delta.saturating_add(curr.commands_delta);
                    prev.tokens_saved_delta = prev
                        .tokens_saved_delta
                        .saturating_add(curr.tokens_saved_delta);
                    continue;
                }
            }
        }
        out.push(ev);
    }
    out
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

fn memory_export_cached(
    state: &State<'_, AppState>,
    memory_path: &Path,
) -> Result<String, String> {
    if let Some(cached) = state.cached_memory_export() {
        return Ok(cached);
    }
    let entrypoint = state.tool_manager.headroom_entrypoint();
    let stdout = run_memory_export(&entrypoint, memory_path)?;
    state.store_memory_export(stdout.clone());
    Ok(stdout)
}

#[tauri::command]
fn delete_live_learning(
    state: State<'_, AppState>,
    memory_id: String,
) -> Result<(), String> {
    let memory_path = headroom_memory_db_path();
    if !memory_path.exists() {
        return Err("Memory database does not exist.".into());
    }
    let entrypoint = state.tool_manager.headroom_entrypoint();
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
fn list_applied_patterns(
    project_path: String,
) -> Result<crate::models::AppliedPatterns, String> {
    let claude_md = std::path::PathBuf::from(&project_path).join("CLAUDE.md");
    let memory_md = crate::tool_manager::claude_project_memory_file(&project_path);

    let claude_sections = read_applied_block(&claude_md);
    let memory_sections = read_applied_block(&memory_md);

    Ok(crate::models::AppliedPatterns {
        claude_md: claude_sections,
        memory_md: memory_sections,
    })
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
    let updated = crate::tool_manager::delete_applied_bullet(
        &content,
        &section_title,
        &bullet_text,
    );
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
        return Err(format!(
            "headroom memory export exited {}",
            output.status
        ));
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

fn activity_event_timestamp(event: &ActivityEvent) -> String {
    match event {
        ActivityEvent::Transformation(t) => t.timestamp.clone().unwrap_or_default(),
        ActivityEvent::Memory(m) => m.created_at.clone(),
        ActivityEvent::RtkBatch(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::Milestone(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::DailyRecord(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::AllTimeRecord(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::NewModel(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::Streak(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::SavingsMilestone(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::WeeklyRecap(e) => e.observed_at.to_rfc3339(),
        ActivityEvent::LearningsMilestone(e) => e.observed_at.to_rfc3339(),
    }
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
                    return Err(format!("Could not launch external link with {opener}: {err}"))
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
async fn submit_contact_request(url: String, email: String) -> Result<(), String> {
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
    let response = client
        .post(target)
        .form(&[("contact_request[email]", trimmed)])
        .send()
        .await
        .map_err(|err| err.to_string())?;

    match response.status().as_u16() {
        200..=299 => Ok(()),
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
                        scope.set_extra(
                            "already_configured",
                            result.already_configured.into(),
                        );
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
                    &format!("client setup failed for {client_id}: {msg}"),
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
    let window = match app.get_webview_window("launcher") {
        Some(w) => w,
        None => return,
    };

    let start_pos = match window.outer_position() {
        Ok(p) => p,
        Err(_) => {
            let _ = window.hide();
            return;
        }
    };
    let start_size = match window.outer_size() {
        Ok(s) => s,
        Err(_) => {
            let _ = window.hide();
            return;
        }
    };

    // Resolve the tray icon center as the animation target.
    let target = app
        .tray_by_id("headroom-tray")
        .and_then(|t| t.rect().ok().flatten())
        .map(|r| {
            let (tx, ty) = match r.position {
                Position::Physical(p) => (p.x as f64, p.y as f64),
                Position::Logical(p) => (p.x, p.y),
            };
            let (tw, th) = match r.size {
                tauri::Size::Physical(s) => (s.width as f64, s.height as f64),
                tauri::Size::Logical(s) => (s.width, s.height),
            };
            (tx + tw / 2.0, ty + th / 2.0)
        })
        .unwrap_or_else(|| {
            // Fallback: top-right of screen (typical menu bar area).
            (start_pos.x as f64 + start_size.width as f64, 0.0)
        });

    let start_cx = start_pos.x as f64 + start_size.width as f64 / 2.0;
    let start_cy = start_pos.y as f64 + start_size.height as f64 / 2.0;
    let sw = start_size.width as f64;
    let sh = start_size.height as f64;
    let (target_cx, target_cy) = target;

    std::thread::spawn(move || {
        let frame_ms = 16u64;
        let frames = 24u32; // ~384ms total

        for i in 1..=frames {
            let t = i as f64 / frames as f64;
            let ease = t * t * t; // ease-in cubic — slow start, fast finish

            let scale = 1.0 - ease;
            let cx = start_cx + (target_cx - start_cx) * ease;
            let cy = start_cy + (target_cy - start_cy) * ease;
            let w = (sw * scale).max(1.0) as u32;
            let h = (sh * scale).max(1.0) as u32;
            let x = (cx - w as f64 / 2.0).round() as i32;
            let y = (cy - h as f64 / 2.0).round() as i32;

            let _ = window.set_size(tauri::Size::Physical(PhysicalSize::new(w, h)));
            let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));

            if i < frames {
                std::thread::sleep(std::time::Duration::from_millis(frame_ms));
            }
        }

        let _ = window.hide();
        // Restore original size so the window is ready for next open.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = window.set_size(tauri::Size::Physical(PhysicalSize::new(
            start_size.width,
            start_size.height,
        )));
    });
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
        .plugin(tauri_plugin_notification::init());

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
            let state: tauri::State<'_, AppState> = app.state();
            let app_handle = app.handle().clone();
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
            // Start the intercept layer before anything else touches port 6767.
            proxy_intercept::spawn(std::sync::Arc::clone(&state.claude_bearer_token));
            if state.should_present_on_launch() && !launched_from_autostart {
                let _ = show_launcher_window(app.handle());
            }
            if state.tool_manager.python_runtime_installed() {
                state.set_runtime_starting(true);
            }
            std::thread::spawn(move || {
                let state: tauri::State<'_, AppState> = app_handle.state();
                state.warm_runtime_on_launch(&app_handle);
            });
            // Restore previously connected client integrations in the background.
            std::thread::spawn(|| {
                client_adapters::restore_client_setups();
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
            get_runtime_status,
            get_headroom_logs,
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
            get_headroom_billing_portal_url,
            get_activity_feed,
            list_live_learnings,
            list_live_learnings_for_projects,
            delete_live_learning,
            list_applied_patterns,
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
            get_autostart_enabled,
            set_autostart_enabled,
            uninstall_and_quit,
            quit_headroom
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Tear down the proxy on every exit path (Cmd-Q, dock quit, signal,
            // or our explicit quit/restart commands). Without this, the proxy
            // outlives the desktop and the next launch reuses an orphan.
            if matches!(event, tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit) {
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

fn release_updater_config(current_version: &str) -> Result<Option<ReleaseUpdaterConfig>, String> {
    let configured_pubkey = UPDATER_PUBLIC_KEY
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let configured_stable = UPDATER_ENDPOINTS
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let configured_staging = UPDATER_STAGING_ENDPOINTS
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let prefer_staging = is_prerelease_version(current_version);
    let configured_endpoints = if prefer_staging {
        configured_staging.or(configured_stable)
    } else {
        configured_stable
    };

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
            if cfg!(debug_assertions) {
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

fn detect_headroom_learn_prereq_status() -> HeadroomLearnPrereqStatus {
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


fn parse_memory_export(json: &str, limit: usize) -> Result<Vec<MemoryFeedEvent>, String> {
    #[derive(serde::Deserialize)]
    struct RawMemory {
        id: String,
        #[serde(default)]
        content: String,
        #[serde(default, alias = "scope_level")]
        scope: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        importance: Option<f64>,
    }
    let raw: Vec<RawMemory> =
        serde_json::from_str(json.trim()).map_err(|err| err.to_string())?;
    let mut events: Vec<MemoryFeedEvent> = raw
        .into_iter()
        .filter_map(|m| {
            Some(MemoryFeedEvent {
                id: m.id,
                created_at: normalize_utc_timestamp(&m.created_at?)?,
                scope: m.scope.unwrap_or_else(|| "unknown".into()),
                content: m.content,
                importance: m.importance.unwrap_or(0.0),
            })
        })
        .collect();
    events.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    events.truncate(limit);
    Ok(events)
}

/// Normalize an ISO-8601 timestamp string to RFC3339 UTC (`...Z`).
///
/// Headroom's memory DB stores naive ISO strings (no offset). JS `Date` parses
/// those as local time, so downstream display would be off by the local tz
/// offset. Treat naive strings as UTC and emit `Z`-suffixed output so both the
/// feed sort and the frontend render match system time.
fn normalize_utc_timestamp(raw: &str) -> Option<String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc).to_rfc3339());
    }
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()
        .map(|naive| naive.and_utc().to_rfc3339())
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
    let raw: RawTransformationsFeedResponse =
        response.json().map_err(|err| err.to_string())?;
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
        .current_dir(project_path)
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        .env("HEADROOM_LEARN_CLI", "claude");
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
                (
                    format!("headroom learn completed for {project_name}."),
                    true,
                    None,
                    output_tail,
                    stdout,
                    stderr,
                    output.status.to_string(),
                )
            } else {
                let fail_tail = if output_tail.is_empty() {
                    "No output captured.".to_string()
                } else {
                    output_tail.join("\n")
                };
                sentry::with_scope(
                    |scope| {
                        scope.set_tag("flow", "headroom_learn");
                        scope.set_context(
                            "learn",
                            sentry::protocol::Context::Other(
                                [
                                    ("exit_status".into(), output.status.to_string().into()),
                                    ("output_tail".into(), fail_tail.clone().into()),
                                ]
                                .into(),
                            ),
                        );
                    },
                    || {
                        sentry::capture_message(
                            "headroom learn exited with non-zero status",
                            sentry::Level::Error,
                        )
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
    let pause = tauri::menu::MenuItem::with_id(app, "pause", "Pause Headroom", true, None::<&str>)?;
    let quit = tauri::menu::MenuItem::with_id(app, "quit", "Quit Headroom", true, None::<&str>)?;
    let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
    let menu = tauri::menu::Menu::with_items(app, &[&show, &separator, &pause, &quit])?;
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
            "quit" => {
                exit_headroom(app, QuitSource::TrayMenu);
            }
            "pause" => {
                let state: tauri::State<'_, AppState> = app.state();
                state.set_runtime_paused(true);
                state.stop_headroom();
                let _ = client_adapters::clear_client_setups();
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
            eprintln!("failed to build runtime tray icons: {err}");
            return;
        }
    };

    std::thread::spawn(move || {
        let mut frame_index = 0usize;
        let mut last_non_booting: Option<TrayRuntimeVisual> = None;
        let mut last_displayed_dollars: Option<u32> = None;
        let mut last_tooltip: Option<String> = None;
        let mut unhealthy_streak: u8 = 0;
        let mut connector_check_counter: u32 = 0;
        let mut cached_connector_enabled: bool = client_adapters::is_claude_code_enabled();

        loop {
            // Re-check the Claude connector every ~2s (every 8 ticks at 260ms).
            if connector_check_counter % 8 == 0 {
                cached_connector_enabled = client_adapters::is_claude_code_enabled();
            }
            connector_check_counter = connector_check_counter.wrapping_add(1);

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
            let visual = debounced_tray_runtime_visual(
                raw_visual,
                last_non_booting,
                &mut unhealthy_streak,
            );

            if let Some(tray) = app.tray_by_id("headroom-tray") {
                let tooltip = match visual {
                    TrayRuntimeVisual::Booting => "Headroom — starting",
                    TrayRuntimeVisual::Running => "Headroom — active",
                    TrayRuntimeVisual::Paused => {
                        "Headroom — paused (Claude Code running normally)"
                    }
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
                            let (new_rgba, new_w, new_h) = build_running_with_savings(&icons.running_rgba, bw, bh, dollars);
                            let _ = tray.set_icon(Some(tauri::image::Image::new_owned(new_rgba, new_w, new_h)));
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
                        eprintln!("tray: set_tooltip failed: {err}");
                    }
                    last_tooltip = Some(tooltip.to_string());
                }
            } else {
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(260));
        }
    });
}

/// Every 5s, check whether the Python proxy is actually reachable while the
/// app thinks the runtime should be up. If it isn't, try to restart via
/// `ensure_headroom_running`. After 3 consecutive failures (~15s down) we
/// give up: pause the runtime, strip Headroom's interception (BASE_URL,
/// hooks, shell blocks) so Claude Code falls back to its normal behavior,
/// and notify the user. The user can re-enable from the menu when ready —
/// `start_headroom` re-applies everything via `restore_client_setups`.
fn spawn_proxy_watchdog(app: AppHandle) {
    const POLL: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_CONSECUTIVE_FAILURES: u32 = 3;

    std::thread::spawn(move || {
        let mut consecutive_failures: u32 = 0;

        loop {
            std::thread::sleep(POLL);

            let state: tauri::State<'_, AppState> = app.state();
            let runtime = state.runtime_status();

            // Only care when the runtime is supposed to be up: installed,
            // not paused by the user, not mid-boot, and not mid-upgrade.
            // Anything else and we reset the counter and keep watching.
            let should_be_up = runtime.installed
                && !runtime.paused
                && !runtime.starting
                && !state.runtime_upgrade_in_progress();
            if !should_be_up {
                consecutive_failures = 0;
                continue;
            }

            if runtime.proxy_reachable {
                consecutive_failures = 0;
                continue;
            }

            consecutive_failures = consecutive_failures.saturating_add(1);
            eprintln!(
                "watchdog: proxy unreachable (failure {consecutive_failures}/{MAX_CONSECUTIVE_FAILURES}), attempting restart"
            );

            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                eprintln!(
                    "watchdog: giving up after {MAX_CONSECUTIVE_FAILURES} failures; disabling interception"
                );
                state.set_runtime_paused(true);
                state.stop_headroom();
                if let Err(err) = client_adapters::clear_client_setups() {
                    eprintln!("watchdog: clear_client_setups failed: {err}");
                }
                analytics::track_event(&app, "runtime_auto_paused", None);
                let _ = show_notification_impl(
                    &app,
                    "Headroom paused",
                    "Headroom couldn't restart its proxy — interception disabled so Claude Code keeps working. Open Headroom to try again.",
                    Some("connectors".into()),
                );
                consecutive_failures = 0;
                continue;
            }

            // Otherwise try to bring it back.
            if let Err(err) = state.ensure_headroom_running() {
                eprintln!("watchdog: ensure_headroom_running failed: {err:#}");
            }
        }
    });
}

fn spawn_tray_savings_updater(app: AppHandle) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
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
        }
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
                let _ = window.hide();
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
        b'0' => [[1,1,1],[1,0,1],[1,0,1],[1,0,1],[1,1,1]],
        b'1' => [[0,1,0],[1,1,0],[0,1,0],[0,1,0],[1,1,1]],
        b'2' => [[1,1,1],[0,0,1],[1,1,1],[1,0,0],[1,1,1]],
        b'3' => [[1,1,1],[0,0,1],[1,1,1],[0,0,1],[1,1,1]],
        b'4' => [[1,0,1],[1,0,1],[1,1,1],[0,0,1],[0,0,1]],
        b'5' => [[1,1,1],[1,0,0],[1,1,1],[0,0,1],[1,1,1]],
        b'6' => [[1,1,1],[1,0,0],[1,1,1],[1,0,1],[1,1,1]],
        b'7' => [[1,1,1],[0,0,1],[0,0,1],[0,0,1],[0,0,1]],
        b'8' => [[1,1,1],[1,0,1],[1,1,1],[1,0,1],[1,1,1]],
        b'9' => [[1,1,1],[1,0,1],[1,1,1],[0,0,1],[1,1,1]],
        b'K' => [[1,0,1],[1,1,0],[1,0,0],[1,1,0],[1,0,1]],
        _    => [[0,0,0],[0,0,0],[0,0,0],[0,0,0],[0,0,0]],
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

    let window = app
        .get_webview_window("main")
        .expect("main window should exist");

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
    if let Err(err) = state.ensure_headroom_running() {
        eprintln!("failed to ensure headroom runtime for tray: {err}");
        capture_headroom_start_failure("ensure_runtime_ready_for_tray failed", &err);
    }
}

fn onboarding_complete(app: &AppHandle) -> bool {
    let state: tauri::State<'_, AppState> = app.state();
    if !state.tool_manager.python_runtime_installed() {
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

fn show_main_window(app: &AppHandle, anchor_rect: Option<Rect>) -> tauri::Result<()> {
    let window = app
        .get_webview_window("main")
        .expect("main window should exist");

    if let Some(rect) = anchor_rect {
        position_tray_window(&window, rect)?;
    }

    window.show()?;
    let _ = window.unminimize();
    window.set_focus()?;
    Ok(())
}

fn show_launcher_window(app: &AppHandle) -> tauri::Result<()> {
    let window = app
        .get_webview_window("launcher")
        .expect("launcher window should exist");

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
        build_activity_feed_response, build_release_updater_config, check_headroom_learn_prereqs,
        coalesce_rtk_batches, compute_tray_window_position, debounced_tray_runtime_visual,
        empty_live_learnings_for_projects, fetch_transformations_feed_from,
        install_pending_update, is_prerelease_version, lifetime_token_milestone_kind,
        normalize_utc_timestamp, parse_live_learnings, parse_memory_export,
        parse_updater_endpoint_list, pattern_matches_project, physical_rect_from_rect,
        store_checked_update, AvailableAppUpdate, HeadroomLearnPrereqStatus,
        InstallPendingUpdateFuture, InstallableAppUpdate, MonitorBounds, PhysicalRect, QuitSource,
        TrayRuntimeVisual, DEFAULT_UPDATER_ENDPOINT, DEFAULT_UPDATER_PUBLIC_KEY,
    };
    use crate::models::{
        ActivityEvent, MemoryFeedEvent, MilestoneEvent, NewModelEvent, RecordEvent,
        RtkBatchEvent, TransformationFeedEvent, TransformationFeedResponse,
    };
    use chrono::{DateTime, TimeZone, Timelike, Utc};
    use serde_json::json;
    use parking_lot::Mutex;
    use tauri::{LogicalPosition, LogicalSize, PhysicalSize, Position, Rect, Size};

    struct FakePendingUpdate {
        metadata: AvailableAppUpdate,
        install_result: Result<(), String>,
    }

    impl InstallableAppUpdate for FakePendingUpdate {
        fn metadata(&self) -> AvailableAppUpdate {
            self.metadata.clone()
        }

        fn install(self) -> InstallPendingUpdateFuture {
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
            .block_on(install_pending_update(&pending))
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
            .block_on(install_pending_update(&pending))
            .expect("install succeeds");

        assert!(pending.lock().is_none());
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
            .block_on(install_pending_update(&pending))
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

        let err = fetch_transformations_feed_from(&format!("http://127.0.0.1:{port}"), 50)
            .unwrap_err();
        server.join().unwrap();
        assert!(err.contains("503"), "expected status code in error, got: {err}");
    }

    #[test]
    fn parse_memory_export_decodes_recent_entries_sorted_descending() {
        let json = r#"[
            {"id":"a","content":"oldest","scope_level":"user","created_at":"2026-04-21T10:00:00","importance":0.5},
            {"id":"b","content":"newest","scope_level":"session","created_at":"2026-04-21T12:00:00","importance":0.9},
            {"id":"c","content":"middle","scope_level":"agent","created_at":"2026-04-21T11:00:00","importance":0.3}
        ]"#;
        let events = parse_memory_export(json, 10).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].id, "b");
        assert_eq!(events[0].content, "newest");
        assert_eq!(events[0].scope, "session");
        assert_eq!(events[1].id, "c");
        assert_eq!(events[2].id, "a");
    }

    #[test]
    fn parse_memory_export_caps_to_limit() {
        let json = r#"[
            {"id":"1","content":"a","scope_level":"user","created_at":"2026-04-21T10:00:00","importance":0.1},
            {"id":"2","content":"b","scope_level":"user","created_at":"2026-04-21T11:00:00","importance":0.1},
            {"id":"3","content":"c","scope_level":"user","created_at":"2026-04-21T12:00:00","importance":0.1}
        ]"#;
        let events = parse_memory_export(json, 2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, "3");
        assert_eq!(events[1].id, "2");
    }

    #[test]
    fn parse_memory_export_handles_empty_array() {
        let events = parse_memory_export("[]", 10).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parse_memory_export_skips_entries_without_created_at() {
        let json = r#"[
            {"id":"1","content":"valid","scope_level":"user","created_at":"2026-04-21T10:00:00","importance":0.5},
            {"id":"2","content":"missing","scope_level":"user","importance":0.1}
        ]"#;
        let events = parse_memory_export(json, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "1");
    }

    #[test]
    fn parse_memory_export_returns_error_on_malformed_json() {
        let err = parse_memory_export("not json", 10).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn parse_memory_export_normalizes_naive_created_at_to_utc_rfc3339() {
        let json = r#"[
            {"id":"a","content":"x","scope_level":"user","created_at":"2026-04-21T10:00:00","importance":0.1}
        ]"#;
        let events = parse_memory_export(json, 10).unwrap();
        assert_eq!(events.len(), 1);
        // Naive timestamp must be treated as UTC and emitted with an offset so
        // JS `new Date(...)` parses it as UTC rather than local.
        assert!(
            events[0].created_at.ends_with("+00:00") || events[0].created_at.ends_with("Z"),
            "expected UTC offset suffix, got {}",
            events[0].created_at
        );
    }

    #[test]
    fn parse_memory_export_preserves_existing_offset() {
        let json = r#"[
            {"id":"a","content":"x","scope_level":"user","created_at":"2026-04-21T10:00:00-07:00","importance":0.1}
        ]"#;
        let events = parse_memory_export(json, 10).unwrap();
        assert_eq!(events.len(), 1);
        // -07:00 wall-clock 10:00 == UTC 17:00.
        let dt = DateTime::parse_from_rfc3339(&events[0].created_at).unwrap();
        assert_eq!(dt.with_timezone(&Utc).hour(), 17);
    }

    #[test]
    fn normalize_utc_timestamp_handles_naive_and_rfc3339() {
        let naive = normalize_utc_timestamp("2026-04-21T10:00:00").unwrap();
        let parsed = DateTime::parse_from_rfc3339(&naive).unwrap();
        assert_eq!(parsed.with_timezone(&Utc).to_rfc3339(),
                   "2026-04-21T10:00:00+00:00");

        let fractional = normalize_utc_timestamp("2026-04-21T10:00:00.123").unwrap();
        assert!(DateTime::parse_from_rfc3339(&fractional).is_ok());

        let with_z = normalize_utc_timestamp("2026-04-21T10:00:00Z").unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(&with_z).unwrap().with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2026-04-21T10:00:00+00:00").unwrap().with_timezone(&Utc)
        );

        assert!(normalize_utc_timestamp("not a date").is_none());
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

        let paths = vec!["/x/a".to_string(), "/x/b".to_string(), "/x/empty".to_string()];
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

        let err = fetch_transformations_feed_from(&format!("http://127.0.0.1:{port}"), 50)
            .unwrap_err();
        assert!(!err.is_empty(), "expected a non-empty error message");
    }

    fn make_transformation(timestamp: &str) -> TransformationFeedEvent {
        TransformationFeedEvent {
            request_id: Some(format!("req-{timestamp}")),
            timestamp: Some(timestamp.to_string()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-6".into()),
            input_tokens_original: Some(1000),
            input_tokens_optimized: Some(250),
            tokens_saved: Some(750),
            savings_percent: Some(75.0),
            transforms_applied: vec!["interceptor:ast-grep".into()],
            workspace: None,
        }
    }

    fn make_memory(id: &str, created_at: &str) -> MemoryFeedEvent {
        MemoryFeedEvent {
            id: id.into(),
            created_at: created_at.into(),
            scope: "user".into(),
            content: format!("memory {id}"),
            importance: 0.5,
        }
    }

    fn timestamp_of(event: &ActivityEvent) -> String {
        match event {
            ActivityEvent::Transformation(t) => t.timestamp.clone().unwrap_or_default(),
            ActivityEvent::Memory(m) => m.created_at.clone(),
            ActivityEvent::RtkBatch(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::Milestone(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::DailyRecord(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::AllTimeRecord(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::NewModel(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::Streak(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::SavingsMilestone(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::WeeklyRecap(e) => e.observed_at.to_rfc3339(),
            ActivityEvent::LearningsMilestone(e) => e.observed_at.to_rfc3339(),
        }
    }

    #[test]
    fn build_activity_feed_response_returns_empty_when_no_inputs() {
        let resp = build_activity_feed_response(None, vec![], vec![], false, 50);
        assert!(resp.events.is_empty());
        assert!(!resp.proxy_reachable);
        assert!(!resp.log_full_messages);
        assert!(!resp.memory_available);
    }

    #[test]
    fn build_activity_feed_response_propagates_proxy_metadata() {
        let transformations = TransformationFeedResponse {
            log_full_messages: true,
            proxy_reachable: true,
            transformations: vec![],
        };
        let resp = build_activity_feed_response(Some(transformations), vec![], vec![], true, 50);
        assert!(resp.proxy_reachable);
        assert!(resp.log_full_messages);
        assert!(resp.memory_available);
    }

    #[test]
    fn build_activity_feed_response_sorts_mixed_events_descending_by_timestamp() {
        let transformations = TransformationFeedResponse {
            log_full_messages: true,
            proxy_reachable: true,
            transformations: vec![
                make_transformation("2026-04-21T10:00:00Z"),
                make_transformation("2026-04-21T12:00:00Z"),
            ],
        };
        let memories = vec![
            make_memory("a", "2026-04-21T11:00:00Z"),
            make_memory("b", "2026-04-21T13:00:00Z"),
        ];

        let resp = build_activity_feed_response(Some(transformations), memories, vec![], true, 50);

        let timestamps: Vec<String> = resp.events.iter().map(timestamp_of).collect();
        assert_eq!(
            timestamps,
            vec![
                "2026-04-21T13:00:00Z".to_string(),
                "2026-04-21T12:00:00Z".to_string(),
                "2026-04-21T11:00:00Z".to_string(),
                "2026-04-21T10:00:00Z".to_string(),
            ]
        );
    }

    #[test]
    fn build_activity_feed_response_caps_to_limit_after_sorting() {
        let transformations = TransformationFeedResponse {
            log_full_messages: false,
            proxy_reachable: true,
            transformations: vec![
                make_transformation("2026-04-21T10:00:00Z"),
                make_transformation("2026-04-21T12:00:00Z"),
            ],
        };
        let memories = vec![
            make_memory("a", "2026-04-21T11:00:00Z"),
            make_memory("b", "2026-04-21T13:00:00Z"),
        ];

        let resp = build_activity_feed_response(Some(transformations), memories, vec![], true, 2);

        assert_eq!(resp.events.len(), 2);
        let timestamps: Vec<String> = resp.events.iter().map(timestamp_of).collect();
        assert_eq!(
            timestamps,
            vec![
                "2026-04-21T13:00:00Z".to_string(),
                "2026-04-21T12:00:00Z".to_string(),
            ]
        );
    }

    #[test]
    fn build_activity_feed_response_treats_transformation_with_no_timestamp_as_oldest() {
        // Transformations from the proxy may have null timestamps; they
        // shouldn't shove themselves to the top of the feed.
        let mut t_no_ts = make_transformation("ignored");
        t_no_ts.timestamp = None;
        let transformations = TransformationFeedResponse {
            log_full_messages: false,
            proxy_reachable: true,
            transformations: vec![t_no_ts, make_transformation("2026-04-21T12:00:00Z")],
        };
        let memories = vec![make_memory("m", "2026-04-21T11:00:00Z")];

        let resp = build_activity_feed_response(Some(transformations), memories, vec![], true, 50);

        let last = resp.events.last().unwrap();
        match last {
            ActivityEvent::Transformation(t) => assert!(t.timestamp.is_none()),
            _ => panic!("expected the no-timestamp transformation to sort last"),
        }
    }

    #[test]
    fn build_activity_feed_response_with_only_memories_still_sorts_them() {
        let memories = vec![
            make_memory("a", "2026-04-21T10:00:00Z"),
            make_memory("b", "2026-04-21T12:00:00Z"),
            make_memory("c", "2026-04-21T11:00:00Z"),
        ];
        let resp = build_activity_feed_response(None, memories, vec![], true, 50);
        assert_eq!(resp.events.len(), 3);
        let ids: Vec<&str> = resp
            .events
            .iter()
            .map(|e| match e {
                ActivityEvent::Memory(m) => m.id.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }

    #[test]
    fn build_activity_feed_response_merges_synthetic_events_into_sorted_stream() {
        let transformations = TransformationFeedResponse {
            log_full_messages: false,
            proxy_reachable: true,
            transformations: vec![make_transformation("2026-04-21T10:00:00Z")],
        };
        let memories = vec![make_memory("m", "2026-04-21T12:30:00Z")];
        let synthetic = vec![
            ActivityEvent::Milestone(MilestoneEvent {
                observed_at: Utc.with_ymd_and_hms(2026, 4, 21, 14, 0, 0).unwrap(),
                milestone_tokens_saved: 1_000_000,
                kind: "first_1m".into(),
            }),
            ActivityEvent::NewModel(NewModelEvent {
                observed_at: Utc.with_ymd_and_hms(2026, 4, 21, 9, 0, 0).unwrap(),
                model: "claude-opus-4-7".into(),
                provider: Some("anthropic".into()),
                workspace: None,
            }),
            ActivityEvent::RtkBatch(RtkBatchEvent {
                observed_at: Utc.with_ymd_and_hms(2026, 4, 21, 13, 0, 0).unwrap(),
                commands_delta: 5,
                tokens_saved_delta: 1234,
                total_commands: 100,
                total_saved: 50_000,
            }),
            ActivityEvent::AllTimeRecord(RecordEvent {
                observed_at: Utc.with_ymd_and_hms(2026, 4, 21, 11, 0, 0).unwrap(),
                tokens_saved: 9_999,
                savings_percent: Some(88.0),
                model: Some("claude-opus-4-7".into()),
                provider: Some("anthropic".into()),
                request_id: Some("r".into()),
                previous_record: Some(500),
                day: None,
                workspace: None,
            }),
        ];

        let resp =
            build_activity_feed_response(Some(transformations), memories, synthetic, true, 50);

        assert_eq!(resp.events.len(), 6);
        let kinds: Vec<&str> = resp
            .events
            .iter()
            .map(|e| match e {
                ActivityEvent::Transformation(_) => "transformation",
                ActivityEvent::Memory(_) => "memory",
                ActivityEvent::RtkBatch(_) => "rtkBatch",
                ActivityEvent::Milestone(_) => "milestone",
                ActivityEvent::DailyRecord(_) => "dailyRecord",
                ActivityEvent::AllTimeRecord(_) => "allTimeRecord",
                ActivityEvent::NewModel(_) => "newModel",
                ActivityEvent::Streak(_) => "streak",
                ActivityEvent::SavingsMilestone(_) => "savingsMilestone",
                ActivityEvent::WeeklyRecap(_) => "weeklyRecap",
                ActivityEvent::LearningsMilestone(_) => "learningsMilestone",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "milestone",
                "rtkBatch",
                "memory",
                "allTimeRecord",
                "transformation",
                "newModel",
            ]
        );
    }

    fn rtk(hour: u32, minute: u32, commands: u64, tokens: u64, total: u64) -> ActivityEvent {
        ActivityEvent::RtkBatch(RtkBatchEvent {
            observed_at: Utc
                .with_ymd_and_hms(2026, 4, 21, hour, minute, 0)
                .unwrap(),
            commands_delta: commands,
            tokens_saved_delta: tokens,
            total_commands: total,
            total_saved: total * 500,
        })
    }

    #[test]
    fn coalesce_rtk_batches_merges_events_within_10_minutes() {
        // Sorted DESC by timestamp (newer first).
        let events = vec![
            rtk(12, 0, 5, 1000, 100),
            rtk(11, 55, 3, 600, 95),
            rtk(11, 51, 2, 400, 92),
        ];
        let merged = coalesce_rtk_batches(events);
        assert_eq!(merged.len(), 1);
        match &merged[0] {
            ActivityEvent::RtkBatch(b) => {
                assert_eq!(b.commands_delta, 10);
                assert_eq!(b.tokens_saved_delta, 2000);
                // Totals remain from the newest (first) entry.
                assert_eq!(b.total_commands, 100);
                assert_eq!(b.observed_at.hour(), 12);
            }
            _ => panic!("expected RtkBatch"),
        }
    }

    #[test]
    fn coalesce_rtk_batches_keeps_events_outside_window_separate() {
        let events = vec![rtk(12, 0, 5, 1000, 100), rtk(11, 45, 3, 600, 95)];
        let merged = coalesce_rtk_batches(events);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn coalesce_rtk_batches_does_not_merge_across_non_rtk_event() {
        let events = vec![
            rtk(12, 0, 5, 1000, 100),
            ActivityEvent::Milestone(MilestoneEvent {
                observed_at: Utc.with_ymd_and_hms(2026, 4, 21, 11, 55, 0).unwrap(),
                milestone_tokens_saved: 1_000_000,
                kind: "first_1m".into(),
            }),
            rtk(11, 51, 2, 400, 92),
        ];
        let merged = coalesce_rtk_batches(events);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn coalesce_rtk_batches_is_noop_on_empty_and_single() {
        assert!(coalesce_rtk_batches(vec![]).is_empty());
        let one = coalesce_rtk_batches(vec![rtk(12, 0, 5, 1000, 100)]);
        assert_eq!(one.len(), 1);
    }

}
