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

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::ActivationPolicy;
use tauri::Manager;
use tauri::{
    AppHandle, PhysicalPosition, PhysicalSize, Position, Rect, State, Window, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::models::{
    BootstrapProgress, ClaudeAccountProfile, ClaudeCodeProject, ClaudeUsage, ClientConnectorStatus,
    ClientSetupResult, ClientSetupVerification, DashboardState, HeadroomAuthCodeRequest,
    HeadroomLearnApiKeyStatus, HeadroomLearnStatus, HeadroomPricingStatus, ResearchCandidate,
    RuntimeStatus,
};
use crate::state::AppState;

const UPDATER_PUBLIC_KEY: Option<&str> = option_env!("HEADROOM_UPDATER_PUBLIC_KEY");
const UPDATER_ENDPOINTS: Option<&str> = option_env!("HEADROOM_UPDATER_ENDPOINTS");
const AUTOSTART_LAUNCH_ARG: &str = "--autostart";
const HEADROOM_DASHBOARD_URL: &str = "http://127.0.0.1:6767/dashboard";
const HEADROOM_LEARN_KEYCHAIN_SERVICE: &str = "com.garm.headroom.headroom-learn";
const HEADROOM_CLAUDE_TOKEN_SERVICE: &str = "com.garm.headroom";
const HEADROOM_CLAUDE_TOKEN_ACCOUNT: &str = "claude-bearer-token";
const HEADROOM_LEARN_OPENAI_ACCOUNT: &str = "openai";
const HEADROOM_LEARN_ANTHROPIC_ACCOUNT: &str = "anthropic";
const HEADROOM_LEARN_GEMINI_ACCOUNT: &str = "gemini";

struct PendingAppUpdate(Mutex<Option<Update>>);

#[derive(Debug, Clone)]
struct ReleaseUpdaterConfig {
    pubkey: String,
    endpoints: Vec<reqwest::Url>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateConfiguration {
    enabled: bool,
    current_version: String,
    endpoint_count: usize,
    configuration_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableAppUpdate {
    current_version: String,
    version: String,
    published_at: Option<String>,
    notes: Option<String>,
}

#[tauri::command]
fn get_dashboard_state(state: State<'_, AppState>) -> DashboardState {
    state.dashboard()
}

#[tauri::command]
fn get_app_update_configuration(app: AppHandle) -> AppUpdateConfiguration {
    match release_updater_config() {
        Ok(Some(config)) => AppUpdateConfiguration {
            enabled: true,
            current_version: app.package_info().version.to_string(),
            endpoint_count: config.endpoints.len(),
            configuration_error: None,
        },
        Ok(None) => AppUpdateConfiguration {
            enabled: false,
            current_version: app.package_info().version.to_string(),
            endpoint_count: 0,
            configuration_error: None,
        },
        Err(err) => AppUpdateConfiguration {
            enabled: false,
            current_version: app.package_info().version.to_string(),
            endpoint_count: 0,
            configuration_error: Some(err),
        },
    }
}

#[tauri::command]
async fn check_for_app_update(
    app: AppHandle,
    pending_update: State<'_, PendingAppUpdate>,
) -> Result<Option<AvailableAppUpdate>, String> {
    let config = release_updater_config()?
        .ok_or_else(|| "Update checks are not configured in this build.".to_string())?;

    let updater = app
        .updater_builder()
        .pubkey(config.pubkey)
        .endpoints(config.endpoints)
        .map_err(|err| err.to_string())?
        .build()
        .map_err(|err| err.to_string())?;

    let update = updater.check().await.map_err(|err| err.to_string())?;
    let mut pending = pending_update
        .0
        .lock()
        .map_err(|_| "Failed to lock pending update state.".to_string())?;

    if let Some(update) = update {
        let published_at = update.date.as_ref().and_then(|date| {
            date.format(&time::format_description::well_known::Rfc3339)
                .ok()
        });
        let metadata = AvailableAppUpdate {
            current_version: update.current_version.clone(),
            version: update.version.clone(),
            published_at,
            notes: update.body.clone(),
        };
        *pending = Some(update);
        Ok(Some(metadata))
    } else {
        *pending = None;
        Ok(None)
    }
}

#[tauri::command]
async fn install_app_update(pending_update: State<'_, PendingAppUpdate>) -> Result<(), String> {
    let update = {
        let mut pending = pending_update
            .0
            .lock()
            .map_err(|_| "Failed to lock pending update state.".to_string())?;
        pending
            .take()
            .ok_or_else(|| "No downloaded update is ready to install.".to_string())?
    };

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.request_restart();
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
    }
    state
        .ensure_headroom_running()
        .map_err(|err| format!("bootstrap complete but failed to start headroom: {err}"))?;

    Ok(state.dashboard())
}

#[tauri::command]
fn start_bootstrap(app: AppHandle) -> Result<(), String> {
    {
        let state: tauri::State<'_, AppState> = app.state();
        state.begin_bootstrap()?;
    }

    let app_handle = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<'_, AppState> = app_handle.state();

        let result = state
            .tool_manager
            .bootstrap_all_with_progress(|step| state.update_bootstrap_step(step));
        if let Err(err) = result {
            state.mark_bootstrap_failed(format!("Installation failed: {err}"));
            return;
        }

        if let Err(err) = client_adapters::ensure_rtk_integrations(
            &state.tool_manager.rtk_entrypoint(),
            &state.tool_manager.managed_python(),
        ) {
            eprintln!("failed to ensure RTK integrations after bootstrap: {err}");
        }

        state.update_bootstrap_step(crate::tool_manager::BootstrapStepUpdate {
            step: "Starting Headroom",
            message: "Starting Headroom in the background.".into(),
            eta_seconds: 5,
            percent: 98,
        });

        if let Err(err) = state.ensure_headroom_running() {
            state.mark_bootstrap_failed(format!(
                "Install completed but Headroom failed to start: {err}"
            ));
            return;
        }

        state.mark_bootstrap_complete();
    });

    Ok(())
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
fn request_headroom_auth_code(email: String) -> Result<HeadroomAuthCodeRequest, String> {
    pricing::request_auth_code(&email)
}

#[tauri::command]
fn verify_headroom_auth_code(
    state: State<'_, AppState>,
    email: String,
    code: String,
    invite_code: Option<String>,
) -> Result<HeadroomPricingStatus, String> {
    pricing::verify_auth_code(&state, &email, &code, invite_code.as_deref())
}

#[tauri::command]
fn sign_out_headroom_account() -> Result<(), String> {
    pricing::sign_out()
}

#[tauri::command]
fn activate_headroom_account(state: State<'_, AppState>) -> Result<HeadroomPricingStatus, String> {
    pricing::activate_account(&state)
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
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(trimmed);
        command
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", trimmed]);
        command
    };

    let status = command
        .status()
        .map_err(|err| format!("Could not launch external link: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("External link opener exited with {status}."))
    }
}

#[tauri::command]
fn open_external_link(url: String) -> Result<(), String> {
    open_external_link_impl(&url)
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
fn apply_client_setup(client_id: String) -> Result<ClientSetupResult, String> {
    client_adapters::apply_client_setup(&client_id).map_err(|err| err.to_string())
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
fn disable_client_setup(client_id: String) -> Result<(), String> {
    client_adapters::disable_client_setup(&client_id).map_err(|err| err.to_string())
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
    Ok(())
}

#[tauri::command]
fn start_headroom(app: AppHandle) -> Result<(), String> {
    let state: tauri::State<'_, AppState> = app.state();
    state.resume_runtime().map_err(|err| err.to_string())?;
    std::thread::spawn(|| {
        client_adapters::restore_client_setups();
    });
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
fn quit_headroom(app: AppHandle) {
    let state: tauri::State<'_, AppState> = app.state();
    state.stop_headroom();
    let _ = client_adapters::clear_client_setups();
    app.exit(0);
}

fn launched_from_autostart() -> bool {
    std::env::args().any(|arg| arg == AUTOSTART_LAUNCH_ARG)
}

pub fn run() {
    let state = AppState::new().expect("failed to create app state");

    tauri::Builder::default()
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args([AUTOSTART_LAUNCH_ARG])
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(ActivationPolicy::Accessory);
                app.set_dock_visibility(false);
            }

            let launched_from_autostart = launched_from_autostart();
            if let Ok(false) = app.autolaunch().is_enabled() {
                if let Err(err) = app.autolaunch().enable() {
                    eprintln!("failed to enable autostart: {err}");
                }
            }

            setup_tray(app.handle())?;
            spawn_tray_runtime_icon_updater(app.handle().clone());
            let state: tauri::State<'_, AppState> = app.state();
            // Start the intercept layer before anything else touches port 6767.
            proxy_intercept::spawn(std::sync::Arc::clone(&state.claude_bearer_token));
            if state.should_present_on_launch() && !launched_from_autostart {
                let _ = show_launcher_window(app.handle());
            }
            let app_handle = app.handle().clone();
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
            show_dashboard_window,
            open_headroom_dashboard,
            open_external_link,
            submit_contact_request,
            hide_launcher_animated,
            quit_headroom
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn release_updater_config() -> Result<Option<ReleaseUpdaterConfig>, String> {
    let Some(pubkey) = UPDATER_PUBLIC_KEY
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    let endpoint_spec = UPDATER_ENDPOINTS
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Updater public key is configured, but HEADROOM_UPDATER_ENDPOINTS is missing."
                .to_string()
        })?;

    let endpoints = parse_updater_endpoint_list(endpoint_spec)?;

    if endpoints.is_empty() {
        return Err("HEADROOM_UPDATER_ENDPOINTS did not include any valid URLs.".into());
    }

    Ok(Some(ReleaseUpdaterConfig {
        pubkey: pubkey.to_string(),
        endpoints,
    }))
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
        Err(err) => (
            format!("headroom learn failed for {project_name}."),
            false,
            Some(format!("Could not start headroom learn: {err}")),
            Vec::new(),
            String::new(),
            String::new(),
            "spawn_error".to_string(),
        ),
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
                let state: tauri::State<'_, AppState> = app.state();
                state.stop_headroom();
                let _ = client_adapters::clear_client_setups();
                app.exit(0);
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
}

struct TrayRuntimeIcons {
    off: tauri::image::Image<'static>,
    running: tauri::image::Image<'static>,
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

        loop {
            let visual = {
                let state: tauri::State<'_, AppState> = app.state();
                let runtime = state.runtime_status();
                if runtime.running {
                    TrayRuntimeVisual::Running
                } else if runtime.starting {
                    TrayRuntimeVisual::Booting
                } else {
                    TrayRuntimeVisual::Off
                }
            };

            if let Some(tray) = app.tray_by_id("headroom-tray") {
                match visual {
                    TrayRuntimeVisual::Booting => {
                        let icon =
                            icons.booting_frames[frame_index % icons.booting_frames.len()].clone();
                        let _ = tray.set_icon(Some(icon));
                        frame_index = (frame_index + 1) % icons.booting_frames.len();
                    }
                    TrayRuntimeVisual::Running => {
                        if last_non_booting != Some(TrayRuntimeVisual::Running) {
                            let _ = tray.set_icon(Some(icons.running.clone()));
                            last_non_booting = Some(TrayRuntimeVisual::Running);
                        }
                    }
                    TrayRuntimeVisual::Off => {
                        if last_non_booting != Some(TrayRuntimeVisual::Off) {
                            let _ = tray.set_icon(Some(icons.off.clone()));
                            last_non_booting = Some(TrayRuntimeVisual::Off);
                        }
                    }
                }
            } else {
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(260));
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
    let booting_base = to_grayscale_strength(&rgba, 0.5);
    let booting_90 = rotate_90_cw(&booting_base, width, height);
    let booting_180 = rotate_90_cw(&booting_90, width, height);
    let booting_270 = rotate_90_cw(&booting_180, width, height);

    Ok(TrayRuntimeIcons {
        off: tauri::image::Image::new_owned(off_rgba, width, height),
        running: tauri::image::Image::new_owned(rgba, width, height),
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
    }
}

fn onboarding_complete(app: &AppHandle) -> bool {
    let state: tauri::State<'_, AppState> = app.state();
    state.tool_manager.python_runtime_installed()
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

fn position_tray_window(window: &tauri::WebviewWindow, rect: Rect) -> tauri::Result<()> {
    let window_width = 760.0;
    let (tray_x, tray_y) = match rect.position {
        Position::Physical(position) => (position.x as f64, position.y as f64),
        Position::Logical(position) => (position.x, position.y),
    };
    let (tray_width, tray_height) = match rect.size {
        tauri::Size::Physical(size) => (size.width as f64, size.height as f64),
        tauri::Size::Logical(size) => (size.width, size.height),
    };

    let tray_midpoint = tray_x + (tray_width / 2.0);
    let target_x = (tray_midpoint - (window_width / 2.0)).round() as i32;
    let target_y = (tray_y + tray_height + 10.0).round() as i32;

    window.set_position(Position::Physical(PhysicalPosition::new(
        target_x, target_y,
    )))
}

#[cfg(test)]
mod tests {
    use super::parse_updater_endpoint_list;

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
}
