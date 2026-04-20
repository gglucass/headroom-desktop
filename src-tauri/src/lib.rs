mod analytics;
mod bearer;
mod client_adapters;
mod insights;
mod keychain;
mod models;
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

use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
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
    BillingPeriod, BootstrapProgress, ClaudeAccountProfile, ClaudeCodeProject, ClaudeUsage,
    ClientConnectorStatus, ClientSetupResult, ClientSetupVerification, DashboardState,
    HeadroomAuthCodeRequest, HeadroomLearnApiKeyStatus, HeadroomLearnStatus, HeadroomPricingStatus,
    HeadroomSubscriptionTier, ResearchCandidate, RuntimeStatus,
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
const HEADROOM_LEARN_KEYCHAIN_SERVICE: &str = "com.extraheadroom.headroom.headroom-learn";
const HEADROOM_LEARN_OPENAI_ACCOUNT: &str = "openai";
const HEADROOM_LEARN_ANTHROPIC_ACCOUNT: &str = "anthropic";
const HEADROOM_LEARN_GEMINI_ACCOUNT: &str = "gemini";
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

fn check_zero_spend_anomaly(dashboard: &DashboardState) {
    if ZERO_SPEND_ALERT_FIRED.load(Ordering::Relaxed) {
        return;
    }
    let affected_days: Vec<&str> = dashboard
        .daily_savings
        .iter()
        .filter(|p| p.estimated_tokens_saved > 0 && p.actual_cost_usd == 0.0 && p.total_tokens_sent == 0)
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
fn get_dashboard_state(app: AppHandle, state: State<'_, AppState>) -> DashboardState {
    let (dashboard, lifetime_token_milestones) = state.dashboard_with_lifetime_token_milestones();

    for milestone_tokens_saved in lifetime_token_milestones {
        analytics::track_event(
            &app,
            "lifetime_tokens_saved_milestone_reached",
            Some(json!({
                "milestone_tokens_saved": milestone_tokens_saved,
                "milestone_millions": milestone_tokens_saved / 1_000_000,
                "milestone_kind": lifetime_token_milestone_kind(milestone_tokens_saved),
                "lifetime_tokens_saved": dashboard.lifetime_estimated_tokens_saved,
                "lifetime_requests": dashboard.lifetime_requests,
                "launch_count": state.launch_count(),
                "launch_experience": state.launch_experience_label()
            })),
        );
        pricing::report_milestone(milestone_tokens_saved);
    }

    check_zero_spend_anomaly(&dashboard);

    dashboard
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
                capture_bootstrap_failure(&err);
                state.mark_bootstrap_failed(
                    "Installation failed: Headroom couldn't download a required file. \
                    Please check your internet connection and try restarting the app. \
                    If this keeps happening, contact support at headroom.ai/support."
                );
                emit_bootstrap_progress(&app_handle, &state);
                analytics::track_event(
                    &app_handle,
                    "bootstrap_failed",
                    Some(json!({ "phase": "install_runtime" })),
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

/// Report a bootstrap failure to Sentry. If the error chain contains a
/// `CommandFailure`, its full stdout/stderr/exit_code are sent as structured
/// `extra` fields (which Sentry does NOT truncate at the 8KB message cap),
/// so we can actually see why pip/venv failed on the user's machine.
fn capture_bootstrap_failure(err: &anyhow::Error) {
    let technical_err = format!("{err:#}");
    let cmd_failure = err
        .chain()
        .find_map(|e| e.downcast_ref::<tool_manager::CommandFailure>());

    if let Some(failure) = cmd_failure {
        sentry::with_scope(
            |scope| {
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
        sentry::capture_message(
            &format!("bootstrap_failed (install_runtime): {technical_err}"),
            sentry::Level::Error,
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

#[tauri::command]
fn get_bootstrap_progress(state: State<'_, AppState>) -> BootstrapProgress {
    state.bootstrap_progress()
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
    email: String,
) -> Result<HeadroomAuthCodeRequest, String> {
    let request = pricing::request_auth_code(&email)?;
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
fn get_headroom_learn_api_key_status() -> HeadroomLearnApiKeyStatus {
    detect_headroom_learn_api_key_status()
}

#[tauri::command]
fn set_headroom_learn_api_key(
    provider: String,
    api_key: String,
) -> Result<HeadroomLearnApiKeyStatus, String> {
    if let Some(reason) = crate::state::headroom_learn_platform_message() {
        return Err(reason);
    }

    let normalized_provider = provider.trim().to_ascii_lowercase();
    let trimmed_key = api_key.trim().to_string();
    if trimmed_key.is_empty() {
        return Err("API key cannot be empty.".into());
    }

    let mut keys = load_headroom_learn_api_keys_strict()?;
    match normalized_provider.as_str() {
        "openai" => keys.openai_api_key = Some(trimmed_key),
        "anthropic" => keys.anthropic_api_key = Some(trimmed_key),
        "gemini" => keys.gemini_api_key = Some(trimmed_key),
        _ => {
            return Err("Unsupported provider. Use openai, anthropic, or gemini.".into());
        }
    }

    write_headroom_learn_api_keys(&keys)?;
    Ok(detect_headroom_learn_api_key_status())
}

#[tauri::command]
fn start_headroom_learn(app: AppHandle, project_path: String) -> Result<(), String> {
    if let Some(reason) = crate::state::headroom_learn_platform_message() {
        return Err(reason);
    }

    if !detect_headroom_learn_api_key_status().has_api_key {
        return Err("Add an API key before running headroom learn.".into());
    }

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

    let client = reqwest::Client::builder()
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .post(url)
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
            ..Default::default()
        },
    ));

    let state = AppState::new().expect("failed to create app state");

    let builder = tauri::Builder::default()
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
                state.warm_runtime_on_launch();
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
            get_headroom_learn_status,
            get_headroom_learn_api_key_status,
            set_headroom_learn_api_key,
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct HeadroomLearnApiKeys {
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    gemini_api_key: Option<String>,
}

fn legacy_headroom_learn_api_keys_path() -> std::path::PathBuf {
    crate::storage::config_file(
        &crate::storage::app_data_dir(),
        "headroom-learn-api-keys.json",
    )
}

fn load_headroom_learn_api_keys() -> HeadroomLearnApiKeys {
    load_headroom_learn_api_keys_strict().unwrap_or_else(|err| {
        eprintln!("headroom learn API key load failed: {err}");
        HeadroomLearnApiKeys::default()
    })
}

fn load_headroom_learn_api_keys_strict() -> Result<HeadroomLearnApiKeys, String> {
    migrate_legacy_headroom_learn_api_keys_to_keychain()?;
    Ok(HeadroomLearnApiKeys {
        openai_api_key: read_headroom_learn_keychain_secret(HEADROOM_LEARN_OPENAI_ACCOUNT)?,
        anthropic_api_key: read_headroom_learn_keychain_secret(HEADROOM_LEARN_ANTHROPIC_ACCOUNT)?,
        gemini_api_key: read_headroom_learn_keychain_secret(HEADROOM_LEARN_GEMINI_ACCOUNT)?,
    })
}

fn write_headroom_learn_api_keys(keys: &HeadroomLearnApiKeys) -> Result<(), String> {
    write_headroom_learn_keychain_secret(
        HEADROOM_LEARN_OPENAI_ACCOUNT,
        keys.openai_api_key.as_deref(),
    )?;
    write_headroom_learn_keychain_secret(
        HEADROOM_LEARN_ANTHROPIC_ACCOUNT,
        keys.anthropic_api_key.as_deref(),
    )?;
    write_headroom_learn_keychain_secret(
        HEADROOM_LEARN_GEMINI_ACCOUNT,
        keys.gemini_api_key.as_deref(),
    )?;

    let path = legacy_headroom_learn_api_keys_path();
    if path.exists() {
        std::fs::remove_file(&path).map_err(|err| {
            format!(
                "Failed to remove legacy API key file {}: {err}",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn read_headroom_learn_keychain_secret(account: &str) -> Result<Option<String>, String> {
    Ok(non_empty_string(keychain::read_secret(
        HEADROOM_LEARN_KEYCHAIN_SERVICE,
        account,
    )?))
}

fn has_headroom_learn_keychain_secret(account: &str) -> Result<bool, String> {
    keychain::has_secret(HEADROOM_LEARN_KEYCHAIN_SERVICE, account)
}

fn write_headroom_learn_keychain_secret(account: &str, value: Option<&str>) -> Result<(), String> {
    let normalized = value.map(str::trim).filter(|value| !value.is_empty());
    if let Some(secret) = normalized {
        keychain::write_secret(HEADROOM_LEARN_KEYCHAIN_SERVICE, account, secret)
    } else {
        keychain::delete_secret(HEADROOM_LEARN_KEYCHAIN_SERVICE, account)
    }
}

fn migrate_legacy_headroom_learn_api_keys_to_keychain() -> Result<(), String> {
    let path = legacy_headroom_learn_api_keys_path();
    if !path.exists() {
        return Ok(());
    }

    let bytes = std::fs::read(&path).map_err(|err| {
        format!(
            "Failed to read legacy API key file {}: {err}",
            path.display()
        )
    })?;
    let legacy = serde_json::from_slice::<HeadroomLearnApiKeys>(&bytes).map_err(|err| {
        format!(
            "Failed to parse legacy API key file {}: {err}",
            path.display()
        )
    })?;

    let existing = HeadroomLearnApiKeys {
        openai_api_key: read_headroom_learn_keychain_secret(HEADROOM_LEARN_OPENAI_ACCOUNT)?,
        anthropic_api_key: read_headroom_learn_keychain_secret(HEADROOM_LEARN_ANTHROPIC_ACCOUNT)?,
        gemini_api_key: read_headroom_learn_keychain_secret(HEADROOM_LEARN_GEMINI_ACCOUNT)?,
    };

    let merged = HeadroomLearnApiKeys {
        openai_api_key: existing
            .openai_api_key
            .or_else(|| non_empty_string(legacy.openai_api_key)),
        anthropic_api_key: existing
            .anthropic_api_key
            .or_else(|| non_empty_string(legacy.anthropic_api_key)),
        gemini_api_key: existing
            .gemini_api_key
            .or_else(|| non_empty_string(legacy.gemini_api_key)),
    };

    write_headroom_learn_api_keys(&merged)
}

fn read_legacy_headroom_learn_api_keys() -> Result<HeadroomLearnApiKeys, String> {
    let path = legacy_headroom_learn_api_keys_path();
    if !path.exists() {
        return Ok(HeadroomLearnApiKeys::default());
    }

    let bytes = std::fs::read(&path).map_err(|err| {
        format!(
            "Failed to read legacy API key file {}: {err}",
            path.display()
        )
    })?;
    serde_json::from_slice::<HeadroomLearnApiKeys>(&bytes).map_err(|err| {
        format!(
            "Failed to parse legacy API key file {}: {err}",
            path.display()
        )
    })
}

fn detect_headroom_learn_api_key_status() -> HeadroomLearnApiKeyStatus {
    if let Some(status) = headroom_learn_keychain_api_key_status() {
        return status;
    }

    if non_empty_string(std::env::var("OPENAI_API_KEY").ok()).is_some() {
        return HeadroomLearnApiKeyStatus {
            has_api_key: true,
            provider: Some("openai".into()),
            source: Some("environment".into()),
        };
    }
    if non_empty_string(std::env::var("ANTHROPIC_API_KEY").ok()).is_some() {
        return HeadroomLearnApiKeyStatus {
            has_api_key: true,
            provider: Some("anthropic".into()),
            source: Some("environment".into()),
        };
    }
    if non_empty_string(std::env::var("GEMINI_API_KEY").ok())
        .or_else(|| non_empty_string(std::env::var("GOOGLE_API_KEY").ok()))
        .is_some()
    {
        return HeadroomLearnApiKeyStatus {
            has_api_key: true,
            provider: Some("gemini".into()),
            source: Some("environment".into()),
        };
    }

    if let Ok(legacy) = read_legacy_headroom_learn_api_keys() {
        if non_empty_string(legacy.openai_api_key).is_some() {
            return HeadroomLearnApiKeyStatus {
                has_api_key: true,
                provider: Some("openai".into()),
                source: Some("legacy_file".into()),
            };
        }
        if non_empty_string(legacy.anthropic_api_key).is_some() {
            return HeadroomLearnApiKeyStatus {
                has_api_key: true,
                provider: Some("anthropic".into()),
                source: Some("legacy_file".into()),
            };
        }
        if non_empty_string(legacy.gemini_api_key).is_some() {
            return HeadroomLearnApiKeyStatus {
                has_api_key: true,
                provider: Some("gemini".into()),
                source: Some("legacy_file".into()),
            };
        }
    }

    HeadroomLearnApiKeyStatus {
        has_api_key: false,
        provider: None,
        source: None,
    }
}

fn headroom_learn_keychain_api_key_status() -> Option<HeadroomLearnApiKeyStatus> {
    if keychain_secret_exists_for_status(HEADROOM_LEARN_OPENAI_ACCOUNT, "openai") {
        return Some(HeadroomLearnApiKeyStatus {
            has_api_key: true,
            provider: Some("openai".into()),
            source: Some("keychain".into()),
        });
    }
    if keychain_secret_exists_for_status(HEADROOM_LEARN_ANTHROPIC_ACCOUNT, "anthropic") {
        return Some(HeadroomLearnApiKeyStatus {
            has_api_key: true,
            provider: Some("anthropic".into()),
            source: Some("keychain".into()),
        });
    }
    if keychain_secret_exists_for_status(HEADROOM_LEARN_GEMINI_ACCOUNT, "gemini") {
        return Some(HeadroomLearnApiKeyStatus {
            has_api_key: true,
            provider: Some("gemini".into()),
            source: Some("keychain".into()),
        });
    }

    None
}

fn keychain_secret_exists_for_status(account: &str, provider: &str) -> bool {
    match has_headroom_learn_keychain_secret(account) {
        Ok(found) => found,
        Err(err) => {
            eprintln!("headroom learn keychain status failed for {provider}: {err}");
            false
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ResolvedHeadroomLearnApiKeys {
    openai: Option<(String, String)>,
    anthropic: Option<(String, String)>,
    gemini: Option<(String, String)>,
}

fn non_empty_string(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn home_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
}

fn read_json_value(path: &std::path::Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
}

fn resolve_headroom_learn_api_keys() -> ResolvedHeadroomLearnApiKeys {
    let mut resolved = ResolvedHeadroomLearnApiKeys::default();
    let saved = load_headroom_learn_api_keys();

    resolved.openai = non_empty_string(saved.openai_api_key).map(|key| (key, "keychain".into()));
    resolved.anthropic =
        non_empty_string(saved.anthropic_api_key).map(|key| (key, "keychain".into()));
    resolved.gemini = non_empty_string(saved.gemini_api_key).map(|key| (key, "keychain".into()));

    if resolved.openai.is_none() {
        resolved.openai = non_empty_string(std::env::var("OPENAI_API_KEY").ok())
            .map(|key| (key, "environment".into()));
    }
    if resolved.anthropic.is_none() {
        resolved.anthropic = non_empty_string(std::env::var("ANTHROPIC_API_KEY").ok())
            .map(|key| (key, "environment".into()));
    }
    if resolved.gemini.is_none() {
        resolved.gemini = non_empty_string(std::env::var("GEMINI_API_KEY").ok())
            .or_else(|| non_empty_string(std::env::var("GOOGLE_API_KEY").ok()))
            .map(|key| (key, "environment".into()));
    }

    let codex_auth = read_json_value(&home_dir().join(".codex").join("auth.json"));
    if resolved.openai.is_none() {
        resolved.openai = codex_auth
            .as_ref()
            .and_then(|root| {
                root.get("OPENAI_API_KEY")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
                    .or_else(|| {
                        root.get("openai_api_key")
                            .and_then(|value| value.as_str())
                            .map(|value| value.to_string())
                    })
            })
            .and_then(|value| non_empty_string(Some(value)))
            .map(|key| (key, "codex".into()));
    }

    let claude_settings = read_json_value(&home_dir().join(".claude").join("settings.json"));
    if resolved.anthropic.is_none() {
        resolved.anthropic = claude_settings
            .as_ref()
            .and_then(|root| root.get("env"))
            .and_then(|env| env.get("ANTHROPIC_API_KEY"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .and_then(|value| non_empty_string(Some(value)))
            .map(|key| (key, "claude".into()));
    }
    if resolved.openai.is_none() {
        resolved.openai = claude_settings
            .as_ref()
            .and_then(|root| root.get("env"))
            .and_then(|env| env.get("OPENAI_API_KEY"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .and_then(|value| non_empty_string(Some(value)))
            .map(|key| (key, "claude".into()));
    }

    let gemini_settings = read_json_value(&home_dir().join(".gemini").join("settings.json"));
    if resolved.gemini.is_none() {
        resolved.gemini = gemini_settings
            .as_ref()
            .and_then(|root| root.get("env"))
            .and_then(|env| {
                env.get("GEMINI_API_KEY")
                    .or_else(|| env.get("GOOGLE_API_KEY"))
            })
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .and_then(|value| non_empty_string(Some(value)))
            .map(|key| (key, "gemini".into()));
    }

    resolved
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

    let keys = resolve_headroom_learn_api_keys();
    let mut command = Command::new(&entrypoint);
    command
        .arg("learn")
        .arg("--project")
        .arg(project_path)
        .arg("--apply")
        .current_dir(project_path)
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1");
    if let Some((key, _)) = keys.openai.as_ref() {
        command.env("OPENAI_API_KEY", key);
    }
    if let Some((key, _)) = keys.anthropic.as_ref() {
        command.env("ANTHROPIC_API_KEY", key);
    }
    if let Some((key, _)) = keys.gemini.as_ref() {
        command.env("GEMINI_API_KEY", key);
        command.env("GOOGLE_API_KEY", key);
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
        let mut connector_check_counter: u32 = 0;
        let mut cached_connector_enabled: bool = client_adapters::is_claude_code_enabled();

        loop {
            // Re-check the Claude connector every ~2s (every 8 ticks at 260ms).
            if connector_check_counter % 8 == 0 {
                cached_connector_enabled = client_adapters::is_claude_code_enabled();
            }
            connector_check_counter = connector_check_counter.wrapping_add(1);

            let visual = {
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
            // not paused by the user, and not mid-boot. Anything else and we
            // reset the counter and keep watching.
            let should_be_up = runtime.installed && !runtime.paused && !runtime.starting;
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
        app_quit_requested_properties, app_update_notification_body, build_release_updater_config,
        compute_tray_window_position, install_pending_update, is_prerelease_version,
        lifetime_token_milestone_kind,
        parse_updater_endpoint_list, physical_rect_from_rect, store_checked_update,
        AvailableAppUpdate, InstallPendingUpdateFuture, InstallableAppUpdate, MonitorBounds,
        PhysicalRect, QuitSource, DEFAULT_UPDATER_ENDPOINT, DEFAULT_UPDATER_PUBLIC_KEY,
    };
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
}
