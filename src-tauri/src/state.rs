use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
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
use crate::client_adapters::{detect_clients, ensure_rtk_integrations, rtk_integration_status};
use crate::insights::generate_daily_insights;
use crate::models::{
    ActivityEvent, BootstrapProgress, ClaudeAccountProfile, ClaudeCodeProject, ClientStatus,
    DailyInsight, DailySavingsPoint, DashboardState, HeadroomLearnStatus, HourlySavingsPoint,
    LaunchExperience, RtkRuntimeStatus, RuntimeStatus, RuntimeUpgradeFailure,
    RuntimeUpgradeProgress, TransformationFeedEvent, UpgradeFailurePhase, UsageEvent,
};
use crate::pricing;
use crate::storage::{app_data_dir, config_file, ensure_data_dirs, telemetry_file};
use crate::tool_manager::{
    BootstrapStepUpdate, HeadroomRelease, ManagedRuntime, RtkGainSummary,
    RuntimeMaintenanceKind, ToolManager,
};

/// After this many consecutive failed auto-attempts at the same app version,
/// we stop auto-retrying and surface a persistent banner with a Retry button.
pub const MAX_UPGRADE_AUTO_RETRIES: u32 = 2;

/// Absolute maximum time we'll wait for the new proxy to come up during
/// boot validation, regardless of observed activity. Bounded so an
/// indefinitely-hung process is still detected eventually. Adaptive stall
/// detection (below) normally fires long before this.
pub const RUNTIME_UPGRADE_BOOT_MAX_SECS: u64 = 600;

/// Once this much wall-time has elapsed without /livez success, start
/// checking the proxy log's mtime for progress. Before this, we stay quiet
/// — most fast boots finish well under this threshold.
pub const RUNTIME_UPGRADE_STALL_GRACE_SECS: u64 = 60;

/// If the proxy log hasn't been written to in this long (AND we're past
/// the grace period), the proxy is considered stalled and we roll back.
pub const RUNTIME_UPGRADE_STALL_SILENCE_SECS: u64 = 45;

enum RuntimeMaintenancePlan {
    Upgrade(HeadroomRelease),
    RequirementsRepair,
}

#[derive(Debug, Default, Clone)]
pub struct PendingMilestones {
    pub token: Vec<u64>,
    pub usd: Vec<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct ActivityObservation {
    pub fresh: Vec<ActivityEvent>,
    pub recent: Vec<ActivityEvent>,
}

impl PendingMilestones {
    pub fn is_empty(&self) -> bool {
        self.token.is_empty() && self.usd.is_empty()
    }
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
        std::env::var("HEADROOM_SKIP_RUNTIME_UPGRADE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// One-shot probe of the new proxy. Hits `/livez` on port 6768 directly
/// first (bypasses the intercept layer on 6767). Falls back to `/health`
/// for older headroom-ai versions that don't expose `/livez`, then through
/// the intercept layer on 6767 as a last resort — which also succeeds if
/// the proxy is alive but too CPU-saturated to answer a direct probe
/// quickly, since the intercept has its own retry + longer timeout path.
fn probe_proxy_livez(client: &reqwest::blocking::Client) -> bool {
    let urls = [
        "http://127.0.0.1:6768/livez",
        "http://127.0.0.1:6768/health",
        "http://127.0.0.1:6767/livez",
        "http://127.0.0.1:6767/health",
    ];
    for url in urls {
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

/// Newest mtime of any `headroom-proxy*.log` file in the logs directory, as
/// a "is the proxy doing anything" signal. Returns None if no logs yet.
fn newest_proxy_log_mtime(logs_dir: &std::path::Path) -> Option<std::time::SystemTime> {
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
        "Finishing up the first-run download — slower connections may take several more minutes".to_string()
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
    pub runtime_starting: Mutex<bool>,
    /// True while an atomic runtime upgrade is running (install + boot validation).
    /// Gates the watchdog from auto-pausing during the ~minutes-long upgrade.
    pub runtime_upgrade_in_progress: Mutex<bool>,
    pub runtime_upgrade_progress: Mutex<RuntimeUpgradeProgress>,
    pub last_startup_error: Mutex<Option<String>>,
    pub bootstrap_progress: Mutex<BootstrapProgress>,
    pub headroom_learn_state: Mutex<HeadroomLearnRuntimeState>,
    /// Last Claude AI OAuth bearer token seen passing through the proxy intercept.
    /// Only populated when the user runs Claude Code authenticated via Claude AI (not API key).
    /// Wrapped in Arc so the proxy_intercept task can share it without going through AppState.
    pub claude_bearer_token: Arc<Mutex<Option<BearerToken>>>,
    launch_profile: Mutex<LaunchProfile>,
    launch_profile_path: std::path::PathBuf,
    savings_tracker: Mutex<SavingsTracker>,
    activity_facts: Mutex<ActivityFacts>,
    cached_clients: Mutex<Option<(Vec<ClientStatus>, Instant)>>,
    cached_headroom_stats: Mutex<Option<(Option<HeadroomDashboardStats>, Instant)>>,
    cached_headroom_history: Mutex<Option<(Option<HeadroomSavingsHistoryResponse>, Instant)>>,
    cached_rtk_gain_summary: Mutex<Option<(Option<RtkGainSummary>, Instant)>>,
    cached_claude_profile: Mutex<Option<(Option<String>, ClaudeAccountProfile, Instant)>>,
    /// Cached stdout of `headroom memory export`. Shared by every OptimizePanel
    /// that mounts at once — without it, N panels = N Python cold-starts.
    cached_memory_export: Mutex<Option<(String, Instant)>>,
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
}

impl AppState {
    pub fn new() -> Result<Self> {
        Self::new_in(app_data_dir())
    }

    fn new_in(base_dir: PathBuf) -> Result<Self> {
        ensure_data_dirs(&base_dir)?;

        let runtime = ManagedRuntime::bootstrap_root(&base_dir);
        let tool_manager = ToolManager::new(runtime);
        let (launch_profile, launch_profile_path) = LaunchProfile::load_or_create(&base_dir)?;
        let savings_tracker = SavingsTracker::load_or_create(&base_dir)?;
        let activity_facts = ActivityFacts::load_or_create(&base_dir)?;

        let state = Self {
            tool_manager,
            recent_usage: Mutex::new(Vec::new()),
            headroom_process: Mutex::new(None),
            lifecycle_lock: Mutex::new(()),
            upgrade_lock: Mutex::new(()),
            runtime_paused: Mutex::new(false),
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
            bootstrap_progress: Mutex::new(BootstrapProgress {
                running: false,
                complete: false,
                failed: false,
                current_step: "Idle".into(),
                message: "Installer has not started.".into(),
                current_step_eta_seconds: 0,
                overall_percent: 0,
            }),
            claude_bearer_token: Arc::new(Mutex::new(None)),
            headroom_learn_state: Mutex::new(HeadroomLearnRuntimeState {
                running: false,
                project_path: None,
                started_at: None,
                finished_at: None,
                success: None,
                summary: "Select a project to run headroom learn.".into(),
                error: None,
                output_tail: Vec::new(),
            }),
            launch_profile: Mutex::new(launch_profile),
            launch_profile_path,
            savings_tracker: Mutex::new(savings_tracker),
            activity_facts: Mutex::new(activity_facts),
            cached_clients: Mutex::new(None),
            cached_headroom_stats: Mutex::new(None),
            cached_headroom_history: Mutex::new(None),
            cached_rtk_gain_summary: Mutex::new(None),
            cached_claude_profile: Mutex::new(None),
            cached_memory_export: Mutex::new(None),
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

        if let Err(err) = ensure_rtk_integrations(
            &self.tool_manager.rtk_entrypoint(),
            &self.tool_manager.managed_python(),
        ) {
            eprintln!("failed to ensure RTK integrations during app launch: {err}");
            sentry::capture_message(
                &format!("RTK integrations failed during warm_runtime_on_launch: {err}"),
                sentry::Level::Warning,
            );
        }

        // App-version-triggered atomic runtime upgrade. Replaces the old
        // receipt-vs-pinned drift path.
        if self.should_run_runtime_upgrade(app) {
            self.run_upgrade_with_ui(app);
        }

        // Independent of the upgrade: if MCP is not configured (e.g. it failed
        // during a prior install), retry it now.
        if let Err(err) = self.tool_manager.ensure_mcp_configured() {
            eprintln!("failed to configure headroom MCP: {err}");
            sentry::capture_message(
                &format!("headroom MCP configuration failed: {err}"),
                sentry::Level::Warning,
            );
        }

        if let Err(err) = self.ensure_headroom_running() {
            eprintln!("failed to auto-start headroom during app launch: {err}");
            crate::capture_headroom_start_failure(
                "headroom auto-start failed during launch",
                &err,
            );
        }

        self.set_runtime_starting(false);
    }

    fn runtime_maintenance_plan_for_app_version(
        &self,
        current_app_version: &str,
    ) -> Option<RuntimeMaintenancePlan> {
        if runtime_upgrade_disabled_by_env() {
            eprintln!(
                "HEADROOM_SKIP_RUNTIME_UPGRADE is set — skipping runtime upgrade check."
            );
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
    pub fn run_upgrade_with_ui(&self, app: &tauri::AppHandle) {
        let _guard = match self.upgrade_lock.try_lock() {
            Some(g) => g,
            None => {
                eprintln!("run_upgrade_with_ui: upgrade already running; skipping");
                return;
            }
        };

        let current_app_version = app.package_info().version.to_string();
        let maintenance_plan = match self.runtime_maintenance_plan_for_app_version(&current_app_version)
        {
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
            RuntimeMaintenancePlan::RequirementsRepair => RuntimeMaintenanceKind::RequirementsRepair,
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
        let previous_app_version = self
            .launch_profile
            .lock()
            .last_launched_app_version
            .clone();

        *self.runtime_upgrade_in_progress.lock() = true;

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
        // SAFETY: self has a stable address for the duration of this call; the
        // closure runs inline and does not outlive this scope.
        let self_ptr: *const AppState = self as *const AppState;
        let progress = move |step: BootstrapStepUpdate| {
            let state_ref = unsafe { &*self_ptr };
            state_ref.set_upgrade_progress(|p| {
                p.current_step = step.step.to_string();
                p.message = step.message.clone();
                p.overall_percent = step.percent;
            });
            emit_runtime_upgrade_progress(&app_for_progress, state_ref);
        };

        use crate::tool_manager::UpgradeOutcome;
        let needs_commit_or_rollback = matches!(maintenance_kind, RuntimeMaintenanceKind::Upgrade);
        let install_result = match maintenance_plan {
            RuntimeMaintenancePlan::Upgrade(release) => {
                match self.tool_manager.atomic_upgrade_headroom(&release, progress) {
                    UpgradeOutcome::InstalledPendingValidation => Ok(()),
                    UpgradeOutcome::InstallFailed { restored, error } => Err((restored, error)),
                }
            }
            RuntimeMaintenancePlan::RequirementsRepair => self
                .tool_manager
                .repair_stale_requirements_with_progress(progress)
                .map_err(|error| (false, error)),
        };
        match install_result {
            Err((restored, error)) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                eprintln!(
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
                crate::capture_upgrade_failure(&error, restored, "install");
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
                *self.runtime_upgrade_in_progress.lock() = false;
                return;
            }
            Ok(()) => {}
        }

        // Boot validation: start the proxy and wait for reachability.
        self.set_upgrade_progress(|p| {
            p.current_step = "Verifying update".into();
            p.message =
                "Launching updated Headroom. This can take a minute — Headroom may need to download new ML models.".into();
            p.overall_percent = 97;
        });
        emit_runtime_upgrade_progress(app, self);

        if let Err(err) = self.ensure_headroom_running() {
            eprintln!("run_upgrade_with_ui: new proxy failed to spawn: {err:#}");
        }
        // Diagnostic: confirm we actually have a tracked child. If this is
        // false, something about ensure_headroom_running short-circuited
        // (e.g., python_runtime_installed returned false because READY flag
        // was missing, pricing gate fired, runtime paused).
        {
            let has_tracked = self.headroom_process.lock().is_some();
            eprintln!(
                "run_upgrade_with_ui: post-spawn tracked_child={} python_installed={}",
                has_tracked,
                self.tool_manager.python_runtime_installed()
            );
        }

        let app_for_progress = app.clone();
        let self_ptr_progress: *const AppState = self as *const AppState;
        let outcome = self.wait_for_boot_validation(move |elapsed, active| {
            let state_ref = unsafe { &*self_ptr_progress };
            let elapsed_secs = elapsed.as_secs();
            let message = boot_validation_message(elapsed_secs, active);
            // Gently creep 97 → 99.5 over the max budget so the bar keeps
            // moving — the user sees *something* happen during long waits.
            let percent = 97
                + ((elapsed_secs as u128 * 250 / RUNTIME_UPGRADE_BOOT_MAX_SECS as u128)
                    .min(250) as u8) / 100;
            state_ref.set_upgrade_progress(|p| {
                p.message = message;
                p.overall_percent = percent.min(99);
            });
            emit_runtime_upgrade_progress(&app_for_progress, state_ref);
        });
        let boot_ok = outcome.is_ok();
        let outcome_label = outcome.label();
        let duration_ms = start.elapsed().as_millis() as u64;
        eprintln!(
            "run_upgrade_with_ui: boot validation {outcome_label} after {}s",
            duration_ms / 1000
        );

        if boot_ok {
            if needs_commit_or_rollback {
                if let Err(err) = self.tool_manager.commit_headroom_upgrade() {
                    eprintln!("commit_headroom_upgrade: non-fatal: {err:#}");
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
            *self.runtime_upgrade_in_progress.lock() = false;
            return;
        }

        // Boot validation failed — roll back to the previous venv when we have
        // one, otherwise leave the repaired runtime in place and surface the
        // failure so the next launch can retry.
        eprintln!(
            "run_upgrade_with_ui: boot validation failed ({}); rolling back to {:?}",
            outcome_label, installed_version
        );
        // Capture the tail of the proxy log BEFORE stop_headroom runs — for
        // a process that crashed on its own, we want what was written right
        // before the exit.
        let log_tail = crate::tool_manager::newest_proxy_log_path(&self.tool_manager.logs_dir())
            .map(|path| crate::tool_manager::tail_log_file(&path, 30))
            .filter(|s| !s.is_empty());
        self.stop_headroom();
        let rollback_result = if needs_commit_or_rollback {
            self.tool_manager.rollback_headroom_upgrade()
        } else {
            Ok(())
        };
        let rollback_restored = needs_commit_or_rollback && rollback_result.is_ok();
        if let Err(err) = rollback_result {
            eprintln!("run_upgrade_with_ui: rollback failed: {err:#}");
        }
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
                "Headroom maintenance for app {} failed boot validation ({}, ran {}ms; internal headroom-ai target: {}, fallback: {:?}).",
                current_app_version,
                outcome_label,
                duration_ms,
                target_version,
                installed_version
            ),
        };
        eprintln!("run_upgrade_with_ui: {err_msg}");
        let err = anyhow::anyhow!("{}", err_msg);
        let previous_app_label = previous_app_version
            .clone()
            .unwrap_or_else(|| "the previous version".into());
        let error_hint = match maintenance_kind {
            RuntimeMaintenanceKind::Upgrade if rollback_restored && restarted => {
                Some(format!("Reverted to Headroom {} and restarted it.", previous_app_label))
            }
            RuntimeMaintenanceKind::Upgrade if rollback_restored => {
                Some(format!("Reverted to Headroom {}.", previous_app_label))
            }
            RuntimeMaintenanceKind::RequirementsRepair if restarted => {
                Some("Headroom restarted with the repaired runtime, but validation still failed.".into())
            }
            RuntimeMaintenanceKind::RequirementsRepair => None,
            _ => None,
        };
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
        let current_app_label = current_app_version.clone();
        let fallback_app_label = previous_app_version
            .clone()
            .unwrap_or_else(|| "the previous version".into());
        self.set_upgrade_progress(|p| {
            p.running = false;
            p.complete = false;
            p.failed = true;
            p.current_step = "Update didn't start".into();
            p.message = match maintenance_kind {
                RuntimeMaintenanceKind::Upgrade if rollback_restored && restarted => {
                    format!(
                        "Headroom {} installed but didn't start. Reverted to {} and restarted it.",
                        current_app_label, fallback_app_label
                    )
                }
                RuntimeMaintenanceKind::Upgrade if rollback_restored => {
                    format!(
                        "Headroom {} installed but didn't start. Reverted to {}.",
                        current_app_label, fallback_app_label
                    )
                }
                RuntimeMaintenanceKind::Upgrade => format!(
                    "Headroom {} installed but didn't start, and rollback failed. Reinstall from the Dashboard.",
                    current_app_label
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
        *self.runtime_upgrade_in_progress.lock() = false;
    }

    /// User-initiated retry of a previously-failed runtime upgrade. Resets
    /// the attempts counter so `should_run_runtime_upgrade` lets it through,
    /// then invokes `run_upgrade_with_ui` directly.
    pub fn retry_runtime_upgrade(&self, app: &tauri::AppHandle) {
        {
            let mut profile = self.launch_profile.lock();
            if let Some(failure) = profile.last_runtime_upgrade_failure.as_mut() {
                failure.attempts = 0;
            }
            persist_launch_profile(&self.launch_profile_path, &profile);
        }
        self.run_upgrade_with_ui(app);
    }

    pub fn runtime_upgrade_in_progress(&self) -> bool {
        *self.runtime_upgrade_in_progress.lock()
    }

    /// Returns true if the tracked Headroom process has DEFINITIVELY exited.
    ///
    /// Only reports exited on `Ok(Some(status))` — i.e., the OS told us the
    /// child reaped. `None` (no tracked child) is NOT treated as exited,
    /// because `ensure_headroom_running` intentionally skips spawning when
    /// the intercept layer already reports the proxy reachable; in that
    /// case there's a live proxy we just don't own the Child handle for.
    /// `Err` (child was reaped by someone else) is also not treated as
    /// exited — the OS-level process may well still be serving traffic.
    fn headroom_process_exited(&self) -> Option<String> {
        let mut guard = self.headroom_process.lock();
        match guard.as_mut() {
            None => None,
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(format!("{status}")),
                Ok(None) => None,
                Err(err) => {
                    eprintln!(
                        "headroom_process_exited: try_wait returned Err (treating as still alive): {err}"
                    );
                    None
                }
            },
        }
    }

    /// Adaptive boot validation loop. Probes `/livez` on port 6768 until the
    /// proxy responds, the proxy process exits, the log goes silent past the
    /// stall threshold, or `RUNTIME_UPGRADE_BOOT_MAX_SECS` elapses. On each
    /// pass through the loop, emits a progress update via `on_progress`.
    fn wait_for_boot_validation<F>(
        &self,
        mut on_progress: F,
    ) -> BootValidationOutcome
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
        let start = Instant::now();
        let mut last_log_activity = start;
        let mut last_seen_mtime = newest_proxy_log_mtime(&logs_dir);
        let mut last_progress = Instant::now()
            .checked_sub(Duration::from_secs(5))
            .unwrap_or_else(Instant::now);

        let max = Duration::from_secs(RUNTIME_UPGRADE_BOOT_MAX_SECS);
        let grace = Duration::from_secs(RUNTIME_UPGRADE_STALL_GRACE_SECS);
        let silence = Duration::from_secs(RUNTIME_UPGRADE_STALL_SILENCE_SECS);
        let progress_interval = Duration::from_secs(2);

        loop {
            if probe_proxy_livez(&client) {
                return BootValidationOutcome::Reachable;
            }

            if let Some(exit_status) = self.headroom_process_exited() {
                eprintln!(
                    "wait_for_boot_validation: tracked proxy child exited with status {exit_status}"
                );
                return BootValidationOutcome::ProcessExited;
            }

            let elapsed = start.elapsed();
            if elapsed >= max {
                return BootValidationOutcome::TimedOut;
            }

            // Refresh log activity observation.
            let current_mtime = newest_proxy_log_mtime(&logs_dir);
            if current_mtime.is_some() && current_mtime != last_seen_mtime {
                last_seen_mtime = current_mtime;
                last_log_activity = Instant::now();
            }
            let activity_age = last_log_activity.elapsed();
            let has_recent_activity = current_mtime.is_some() && activity_age < silence;

            // Past grace period and no recent log writes → treat as stalled.
            if elapsed > grace && activity_age > silence {
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

    fn clear_upgrade_failure(&self) {
        let mut profile = self.launch_profile.lock();
        profile.last_runtime_upgrade_failure = None;
        persist_launch_profile(&self.launch_profile_path, &profile);
    }

    fn record_upgrade_failure(&self, mut failure: RuntimeUpgradeFailure) {
        let mut profile = self.launch_profile.lock();
        let attempts = match profile.last_runtime_upgrade_failure.as_ref() {
            Some(prev) if prev.app_version == failure.app_version => prev.attempts.saturating_add(1),
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
        match self.launch_profile.lock().launch_experience {
            LaunchExperience::FirstRun => "first_run",
            LaunchExperience::Resume => "resume",
            LaunchExperience::Dashboard => "dashboard",
        }
    }

    pub fn setup_wizard_complete(&self) -> bool {
        self.launch_profile.lock().setup_wizard_complete
    }

    pub fn mark_setup_wizard_complete(&self) {
        let mut profile = self.launch_profile.lock();
        if profile.setup_wizard_complete {
            return;
        }
        profile.setup_wizard_complete = true;
        persist_launch_profile(&self.launch_profile_path, &profile);
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
        const TTL: Duration = Duration::from_secs(5);
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

        let current_token = self.current_bearer_token();

        {
            let cache = self
                .cached_claude_profile
                .lock()
                ;
            if let Some((cached_token, profile, at)) = &*cache {
                if *cached_token == current_token && at.elapsed() < TTL {
                    return profile.clone();
                }
            }
        }

        let profile = pricing::detect_claude_profile_uncached(self);
        let mut cache = self
            .cached_claude_profile
            .lock()
            ;
        *cache = Some((current_token, profile.clone(), Instant::now()));
        profile
    }

    fn cached_headroom_stats(&self) -> Option<HeadroomDashboardStats> {
        const TTL: Duration = Duration::from_secs(4);
        let mut cache = self
            .cached_headroom_stats
            .lock()
            ;
        if let Some((stats, at)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return stats.clone();
            }
        }
        let stats = fetch_headroom_dashboard_stats();
        *cache = Some((stats.clone(), Instant::now()));
        stats
    }

    fn cached_headroom_history(&self) -> Option<HeadroomSavingsHistoryResponse> {
        const TTL: Duration = Duration::from_secs(8);
        let mut cache = self
            .cached_headroom_history
            .lock()
            ;
        if let Some((history, at)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return history.clone();
            }
        }
        let history = fetch_headroom_savings_history();
        *cache = Some((history.clone(), Instant::now()));
        history
    }

    fn cached_rtk_gain_summary(&self) -> Option<RtkGainSummary> {
        const TTL: Duration = Duration::from_secs(10);
        let mut cache = self
            .cached_rtk_gain_summary
            .lock()
            ;
        if let Some((stats, at)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return stats.clone();
            }
        }
        let stats = self.tool_manager.rtk_gain_summary();
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

        if let Some(summary) = self.cached_rtk_gain_summary() {
            if let Some(event) = facts.observe_rtk(&summary, Utc::now()) {
                fresh.push(event);
            }
        }

        let _ = facts.save_if_dirty();
        ActivityObservation {
            fresh,
            recent: facts.recent_events(),
        }
    }

    pub fn observe_learnings_count(&self, count: usize) -> Option<ActivityEvent> {
        let mut facts = self.activity_facts.lock();
        let event = facts.observe_learnings_count(count, Utc::now());
        let _ = facts.save_if_dirty();
        event
    }

    /// Record milestone crossings into ActivityFacts so they appear in the
    /// next activity feed poll. Called alongside the existing telemetry path.
    pub fn record_activity_milestones(&self, milestones: &PendingMilestones) {
        if milestones.is_empty() {
            return;
        }
        let mut facts = self.activity_facts.lock();
        let observed_at = Utc::now();
        if !milestones.token.is_empty() {
            facts.record_milestones(&milestones.token, observed_at);
        }
        if !milestones.usd.is_empty() {
            facts.record_savings_milestones(&milestones.usd, observed_at);
        }
        let _ = facts.save_if_dirty();
    }

    /// On Monday, emit a weekly recap rolling up the previous 7 days of
    /// savings. Idempotent per-week: only fires once per Monday even if the
    /// activity feed polls many times that day.
    pub fn maybe_emit_weekly_recap(&self) -> Option<ActivityEvent> {
        let today = Local::now().date_naive();
        if today.weekday() != chrono::Weekday::Mon {
            return None;
        }
        let start = today.checked_sub_days(chrono::Days::new(7))?;
        let end = today.pred_opt()?;

        let totals = {
            let tracker = self.savings_tracker.lock();
            aggregate_weekly_totals(&tracker.daily_savings, start, end)
        };

        let mut facts = self.activity_facts.lock();
        let event = facts.maybe_record_weekly_recap(today, totals, Utc::now());
        let _ = facts.save_if_dirty();
        event
    }

    pub fn dashboard_with_pending_milestones(&self) -> (DashboardState, PendingMilestones) {
        self.build_dashboard(true)
    }

    fn build_dashboard(&self, drain_pending_milestones: bool) -> (DashboardState, PendingMilestones) {
        let tools = self.tool_manager.list_tools();
        let clients = self.cached_clients();
        let recent_usage = self
            .recent_usage
            .lock()

            .clone();
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

        if let Some(stats) = stats.as_ref() {
            if let Some((updated, updated_daily, updated_hourly, milestones)) =
                self.record_savings_snapshot(stats, drain_pending_milestones)
            {
                snapshot = updated;
                daily_savings = updated_daily;
                hourly_savings = updated_hourly;
                pending_milestones = milestones;
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

        if let Some(history) = history.as_ref() {
            if let Some(saved_usd) = history.lifetime_estimated_savings_usd {
                snapshot.lifetime_estimated_savings_usd = saved_usd;
            }
            if let Some(saved_tokens) = history.lifetime_estimated_tokens_saved {
                snapshot.lifetime_estimated_tokens_saved = saved_tokens;
            }
            let cutoff_date = savings_history_cutoff_date();
            let cutoff_hour = format!("{cutoff_date}T00:00");
            daily_savings =
                merge_daily_savings(daily_savings, history.daily_savings(), &cutoff_date);
            hourly_savings =
                merge_hourly_savings(hourly_savings, history.hourly_savings(), &cutoff_hour);
        }

        (
            DashboardState {
                app_version: env!("CARGO_PKG_VERSION").into(),
                launch_experience: self.launch_profile.lock().launch_experience.clone(),
                bootstrap_complete: self.tool_manager.python_runtime_installed(),
                python_runtime_installed: self.tool_manager.python_runtime_installed(),
                lifetime_requests: snapshot.lifetime_requests,
                lifetime_estimated_savings_usd: snapshot.lifetime_estimated_savings_usd,
                lifetime_estimated_tokens_saved: snapshot.lifetime_estimated_tokens_saved,
                session_requests: snapshot.session_requests,
                session_estimated_savings_usd: snapshot.session_estimated_savings_usd,
                session_estimated_tokens_saved: snapshot.session_estimated_tokens_saved,
                session_savings_pct: snapshot.session_savings_pct,
                daily_savings,
                hourly_savings,
                tools,
                clients,
                recent_usage,
                insights,
            },
            pending_milestones,
        )
    }

    pub fn list_claude_code_projects(&self) -> Result<Vec<ClaudeCodeProject>> {
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
            let project_path = std::fs::canonicalize(&project_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(project_path);
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

        let mut state = self
            .headroom_learn_state
            .lock()
            ;
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
        Ok(())
    }

    pub fn complete_headroom_learn_run(
        &self,
        success: bool,
        summary: String,
        error: Option<String>,
        output_tail: Vec<String>,
    ) {
        let mut state = self
            .headroom_learn_state
            .lock()
            ;
        state.running = false;
        state.finished_at = Some(Utc::now());
        state.success = Some(success);
        state.summary = summary;
        state.error = error;
        state.output_tail = output_tail;
    }

    pub fn headroom_learn_status(
        &self,
        selected_project_path: Option<&str>,
    ) -> HeadroomLearnStatus {
        let state = self
            .headroom_learn_state
            .lock()
            
            .clone();

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
        }
    }

    fn record_savings_snapshot(
        &self,
        stats: &HeadroomDashboardStats,
        drain_pending_milestones: bool,
    ) -> Option<(
        SavingsTotalsSnapshot,
        Vec<DailySavingsPoint>,
        Vec<HourlySavingsPoint>,
        PendingMilestones,
    )> {
        let mut tracker = self.savings_tracker.lock();
        let snapshot = tracker.observe(stats)?;
        let daily_savings = tracker.daily_savings();
        let hourly_savings = tracker.hourly_savings();
        let milestones = if drain_pending_milestones {
            PendingMilestones {
                token: tracker.take_pending_lifetime_token_milestones(),
                usd: tracker.take_pending_lifetime_usd_milestones(),
            }
        } else {
            PendingMilestones::default()
        };
        Some((snapshot, daily_savings, hourly_savings, milestones))
    }

    pub fn should_present_on_launch(&self) -> bool {
        true
    }

    pub fn bootstrap_progress(&self) -> BootstrapProgress {
        self.bootstrap_progress
            .lock()
            
            .clone()
    }

    pub fn begin_bootstrap(&self) -> Result<(), String> {
        let python_installed = self.tool_manager.python_runtime_installed();
        let mut progress = self
            .bootstrap_progress
            .lock()
            ;
        let (next, result) = begin_bootstrap_transition(&progress, python_installed);
        *progress = next;
        result
    }

    pub fn update_bootstrap_step(&self, step: BootstrapStepUpdate) {
        let mut progress = self
            .bootstrap_progress
            .lock()
            ;
        *progress = apply_bootstrap_step(&progress, step);
    }

    pub fn mark_bootstrap_proxy_starting(&self) {
        let mut progress = self
            .bootstrap_progress
            .lock()
            ;
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
        let mut progress = self
            .bootstrap_progress
            .lock()
            ;
        *progress = bootstrap_complete_state();
    }

    pub fn mark_bootstrap_failed<S: Into<String>>(&self, message: S) {
        let mut progress = self
            .bootstrap_progress
            .lock()
            ;
        *progress = bootstrap_failed_state(&progress, message.into());
    }

    pub fn ensure_headroom_running(&self) -> Result<()> {
        if !self.tool_manager.python_runtime_installed() {
            return Ok(());
        }

        if !self.pricing_allows_optimization() {
            self.enforce_pricing_gate();
            return Ok(());
        }

        if self.runtime_is_paused() {
            return Ok(());
        }

        // Tear down any orphan proxy from an older desktop build BEFORE taking
        // the lifecycle lock, since `stop_headroom` acquires the same lock.
        // The orphan check: a proxy is reachable, but its argv is missing flags
        // this build relies on (e.g. --log-messages, --learn). Without this we
        // would happily reuse a v0.2.x proxy that pre-dates the Activity feed.
        if is_headroom_proxy_reachable()
            && !crate::tool_manager::running_proxy_matches_expected_args()
        {
            eprintln!(
                "headroom proxy is reachable but its argv predates this build; restarting it"
            );
            self.stop_headroom();
        }

        // Serialize lifecycle transitions so launch warm-up, tray open, and the
        // watchdog cannot race into concurrent proxy spawns before port 6768 is
        // reachable and `headroom_process` has been recorded.
        let _lifecycle_guard = self.lifecycle_lock.lock();

        // Another caller may have brought the runtime up while we waited.
        if !self.tool_manager.python_runtime_installed() {
            return Ok(());
        }
        if !self.pricing_allows_optimization() {
            self.enforce_pricing_gate();
            return Ok(());
        }
        if self.runtime_is_paused() {
            return Ok(());
        }

        // If the proxy is already live (e.g. started externally, or by us under
        // the lifecycle lock just above), treat runtime as healthy without
        // forcing another launcher.
        if is_headroom_proxy_reachable() {
            *self.last_startup_error.lock() = None;
            return Ok(());
        }

        {
            let mut process = self
                .headroom_process
                .lock()
                ;

            if let Some(existing) = process.as_mut() {
                match existing.try_wait() {
                    Ok(None) => return Ok(()),
                    Ok(Some(_)) | Err(_) => {
                        *process = None;
                    }
                }
            }
        } // release lock before the blocking start

        self.set_runtime_starting(true);
        let started = self.tool_manager.start_headroom_background();
        self.set_runtime_starting(false);

        match started {
            Ok(child) => {
                *self.headroom_process.lock() = Some(child);
                *self.last_startup_error.lock() = None;
                Ok(())
            }
            Err(err) => {
                *self.last_startup_error.lock() = Some(format!("{err:#}"));
                Err(err)
            }
        }
    }

    pub fn runtime_status(&self) -> RuntimeStatus {
        let installed = self.tool_manager.python_runtime_installed();
        let paused = self.runtime_is_paused();
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
            let mut process = self
                .headroom_process
                .lock()
                ;
            if let Some(existing) = process.as_mut() {
                match existing.try_wait() {
                    Ok(None) => Some(existing.id()),
                    Ok(Some(_)) | Err(_) => {
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
        let startup_error_hint = startup_error
            .as_deref()
            .and_then(classify_startup_error);

        RuntimeStatus {
            platform: platform.into(),
            support_tier: support_tier.into(),
            installed,
            running: effective_running,
            starting: self.runtime_is_starting() && !effective_running,
            paused,
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
        let mut runtime_paused = self
            .runtime_paused
            .lock()
            ;
        *runtime_paused = paused;
    }

    pub fn runtime_is_paused(&self) -> bool {
        *self
            .runtime_paused
            .lock()
            
    }

    pub fn set_runtime_starting(&self, starting: bool) {
        let mut runtime_starting = self
            .runtime_starting
            .lock()
            ;
        *runtime_starting = starting;
    }

    pub fn runtime_is_starting(&self) -> bool {
        *self
            .runtime_starting
            .lock()
            
    }

    pub fn resume_runtime(&self) -> Result<()> {
        self.set_runtime_paused(false);
        self.ensure_headroom_running()
    }

    pub fn stop_headroom(&self) {
        let _lifecycle_guard = self.lifecycle_lock.lock();
        self.set_runtime_starting(false);
        let mut process = self
            .headroom_process
            .lock()
            ;

        if let Some(mut child) = process.take() {
            let pid = child.id() as i32;
            let _ = std::process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(format!("-{pid}"))
                .status();
            let _ = child.wait();
        }

        // Also clean up detached/orphaned Headroom-managed headroom proxies
        // so quitting the UI cannot leave the background listener behind.
        let managed_python = self.tool_manager.managed_python();
        let command_patterns = [
            format!(
                "{} -m headroom.proxy.server --port 6768 --no-http2",
                managed_python.display()
            ),
            format!(
                "{} proxy --port 6768",
                self.tool_manager.headroom_entrypoint().display()
            ),
        ];
        for pattern in command_patterns {
            if let Err(err) = kill_processes_by_command_pattern(&pattern) {
                eprintln!("failed to clean detached headroom proxy processes: {err}");
            }
        }
    }

    fn pricing_allows_optimization(&self) -> bool {
        pricing::get_pricing_status(self)
            .map(|status| status.optimization_allowed)
            .unwrap_or(true)
    }

    fn enforce_pricing_gate(&self) {
        match pricing::get_pricing_status(self) {
            Ok(status) if !status.optimization_allowed => {
                let _ = crate::client_adapters::disable_client_setup("claude_code");
            }
            _ => {}
        }
    }
}

pub(crate) fn current_platform() -> &'static str {
    std::env::consts::OS
}

pub(crate) fn current_platform_support_tier() -> &'static str {
    match current_platform() {
        "linux" => "experimental",
        _ => "stable",
    }
}

pub(crate) fn headroom_learn_platform_message() -> Option<String> {
    match current_platform() {
        "linux" => Some(
            "Headroom Learn is disabled on Linux preview builds. Core proxy routing works, but Learn and secure API key storage are not production-ready yet."
                .into(),
        ),
        _ => None,
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        let mut process = self.headroom_process.lock();
        if let Some(mut child) = process.take() {
            let pid = child.id() as i32;
            let _ = std::process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(format!("-{pid}"))
                .status();
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

    let last_learn_ran_at = tool_manager.headroom_learn_last_run_at(&project_path);
    let has_persisted_learnings =
        tool_manager.headroom_learn_has_persisted_learnings(&project_path);
    let last_learn_pattern_count = tool_manager.headroom_learn_pattern_count(&project_path);
    let active_days_since_last_learn = if let Some(ref learn_at_str) = last_learn_ran_at {
        chrono::DateTime::parse_from_rfc3339(learn_at_str)
            .ok()
            .map(|learn_at| {
                let learn_time = learn_at.with_timezone(&Utc);
                let mut days = HashSet::new();
                for file in &scan.session_files {
                    if let Ok(meta) = std::fs::metadata(file) {
                        if let Ok(m) = meta.modified() {
                            let t: chrono::DateTime<Utc> = m.into();
                            if t > learn_time {
                                days.insert(t.date_naive());
                            }
                        }
                    }
                }
                days.len()
            })
            .unwrap_or(0)
    } else {
        0
    };

    Some(ClaudeCodeProject {
        id,
        project_path,
        display_name,
        last_worked_at: last_worked_at.to_rfc3339(),
        session_count,
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
    if folder_name.starts_with('-') {
        let mut normalized = folder_name.replace("--", "__dash__");
        normalized = normalized.trim_start_matches('-').to_string();
        let rebuilt = format!("/{}", normalized.replace('-', "/").replace("__dash__", "-"));
        if !rebuilt.trim().is_empty() {
            return rebuilt;
        }
    }
    folder_name.to_string()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn persist_launch_profile(path: &std::path::Path, profile: &LaunchProfile) {
    if let Ok(bytes) = serde_json::to_vec_pretty(profile) {
        let _ = std::fs::write(path, bytes);
    }
}

impl LaunchProfile {
    fn load_or_create(base_dir: &std::path::Path) -> Result<(Self, std::path::PathBuf)> {
        let path = config_file(base_dir, "launch-profile.json");

        let previous = if path.exists() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice::<LaunchProfile>(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?
        } else {
            LaunchProfile {
                launch_count: 0,
                launch_experience: LaunchExperience::FirstRun,
                lifetime_requests: 0,
                lifetime_estimated_savings_usd: 0.0,
                lifetime_estimated_tokens_saved: 0,
                setup_wizard_complete: false,
                last_launched_app_version: None,
                last_runtime_upgrade_failure: None,
            }
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

        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&current).context("serializing launch profile")?,
        )
        .with_context(|| format!("writing {}", path.display()))?;

        Ok((current, path))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SavingsTotalsSnapshot {
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_savings_pct: f64,
    lifetime_requests: usize,
    lifetime_estimated_savings_usd: f64,
    lifetime_estimated_tokens_saved: u64,
}

const FIRST_LIFETIME_TOKEN_MILESTONES: [u64; 3] = [100_000, 1_000_000, 5_000_000];
const REPEATING_LIFETIME_TOKEN_MILESTONE_STEP: u64 = 10_000_000;

const FIRST_LIFETIME_USD_MILESTONES: [u64; 3] = [10, 50, 100];
const REPEATING_LIFETIME_USD_MILESTONE_STEP: u64 = 100;

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
struct DailySavingsBucket {
    estimated_savings_usd: f64,
    estimated_tokens_saved: u64,
    actual_cost_usd: f64,
    total_tokens_sent: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedSavingsState {
    schema_version: u8,
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_savings_pct: f64,
    lifetime_requests: usize,
    lifetime_estimated_savings_usd: f64,
    lifetime_estimated_tokens_saved: u64,
    last_observation: Option<SavingsObservation>,
    display_session_baseline: Option<SavingsObservation>,
    session_savings_history: Vec<HeadroomSavingsHistoryPoint>,
    session_hourly_buckets: BTreeMap<String, DailySavingsBucket>,
    daily_savings: BTreeMap<String, DailySavingsBucket>,
    hourly_savings: BTreeMap<String, DailySavingsBucket>,
}

struct SavingsTracker {
    records_path: std::path::PathBuf,
    state_path: std::path::PathBuf,
    session_requests: usize,
    session_estimated_savings_usd: f64,
    session_estimated_tokens_saved: u64,
    session_savings_pct: f64,
    lifetime_requests: usize,
    lifetime_estimated_savings_usd: f64,
    lifetime_estimated_tokens_saved: u64,
    last_observation: Option<SavingsObservation>,
    display_session_baseline: Option<SavingsObservation>,
    session_savings_history: Vec<HeadroomSavingsHistoryPoint>,
    session_hourly_buckets: BTreeMap<String, DailySavingsBucket>,
    daily_savings: BTreeMap<String, DailySavingsBucket>,
    hourly_savings: BTreeMap<String, DailySavingsBucket>,
    pending_lifetime_token_milestones: Vec<u64>,
    pending_lifetime_usd_milestones: Vec<u64>,
    // Write throttle — only flush to disk at most once per minute
    last_written_at: Option<std::time::Instant>,
}

impl SavingsTracker {
    fn load_or_create(base_dir: &Path) -> Result<Self> {
        let records_path = telemetry_file(base_dir, "savings-records.jsonl");
        let state_path = config_file(base_dir, "savings-state.json");
        if !records_path.exists() {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&records_path)
                .with_context(|| format!("creating {}", records_path.display()))?;
        }

        let persisted_state = load_persisted_savings_state(&state_path).ok().flatten();

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
            lifetime_estimated_savings_usd: persisted_state
                .as_ref()
                .map_or(0.0, |state| state.lifetime_estimated_savings_usd),
            lifetime_estimated_tokens_saved: persisted_state
                .as_ref()
                .map_or(0, |state| state.lifetime_estimated_tokens_saved),
            last_observation: persisted_state
                .as_ref()
                .and_then(|state| state.last_observation.clone()),
            display_session_baseline: persisted_state
                .as_ref()
                .and_then(|state| state.display_session_baseline.clone()),
            session_savings_history: persisted_state
                .as_ref()
                .map_or_else(Vec::new, |state| state.session_savings_history.clone()),
            session_hourly_buckets: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| state.session_hourly_buckets.clone()),
            daily_savings: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| state.daily_savings.clone()),
            hourly_savings: persisted_state
                .as_ref()
                .map_or_else(BTreeMap::new, |state| state.hourly_savings.clone()),
            pending_lifetime_token_milestones: Vec::new(),
            pending_lifetime_usd_milestones: Vec::new(),
            last_written_at: None,
        };
        tracker.persist_state()?;
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
            lifetime_estimated_savings_usd: self.lifetime_estimated_savings_usd,
            lifetime_estimated_tokens_saved: self.lifetime_estimated_tokens_saved,
        }
    }

    fn daily_savings(&self) -> Vec<DailySavingsPoint> {
        self.daily_savings
            .iter()
            .map(|(date, bucket)| DailySavingsPoint {
                date: date.clone(),
                estimated_savings_usd: bucket.estimated_savings_usd,
                estimated_tokens_saved: bucket.estimated_tokens_saved,
                actual_cost_usd: bucket.actual_cost_usd,
                total_tokens_sent: bucket.total_tokens_sent,
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
                actual_cost_usd: bucket.actual_cost_usd,
                total_tokens_sent: bucket.total_tokens_sent,
            })
            .collect()
    }

    fn take_pending_lifetime_token_milestones(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_lifetime_token_milestones)
    }

    fn take_pending_lifetime_usd_milestones(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_lifetime_usd_milestones)
    }

    fn observe(&mut self, stats: &HeadroomDashboardStats) -> Option<SavingsTotalsSnapshot> {
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
        }
        self.session_savings_history =
            merge_session_savings_history(&self.session_savings_history, &stats.savings_history);

        let previous_session_hourly_buckets = self.session_hourly_buckets.clone();
        let current_session_hourly_buckets =
            derive_session_hourly_buckets(stats, &self.session_savings_history);
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
        let previous_lifetime_tokens_saved = self.lifetime_estimated_tokens_saved;
        let previous_lifetime_estimated_savings_usd = self.lifetime_estimated_savings_usd;
        if delta_requests > 0 || delta_tokens > 0 || delta_usd > 0.0 {
            self.lifetime_requests = self.lifetime_requests.saturating_add(delta_requests);
            self.lifetime_estimated_savings_usd += delta_usd;
            self.lifetime_estimated_tokens_saved = self
                .lifetime_estimated_tokens_saved
                .saturating_add(delta_tokens);
        }
        self.pending_lifetime_token_milestones
            .extend(lifetime_token_milestones_crossed(
                previous_lifetime_tokens_saved,
                self.lifetime_estimated_tokens_saved,
            ));
        self.pending_lifetime_usd_milestones
            .extend(lifetime_usd_milestones_crossed(
                previous_lifetime_estimated_savings_usd,
                self.lifetime_estimated_savings_usd,
            ));

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
            );
            self.add_daily_delta(
                &day_key_from_hour_key(hour_key),
                bucket.estimated_savings_usd,
                bucket.estimated_tokens_saved,
                bucket.actual_cost_usd,
                bucket.total_tokens_sent,
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
            );
            self.subtract_daily_delta(
                &day_key_from_hour_key(hour_key),
                bucket.estimated_savings_usd,
                bucket.estimated_tokens_saved,
                bucket.actual_cost_usd,
                bucket.total_tokens_sent,
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
    ) {
        if usd <= 0.0 && tokens == 0 && actual_cost_usd <= 0.0 && total_tokens_sent == 0 {
            return;
        }
        let entry = self.daily_savings.entry(day_key.to_string()).or_default();
        entry.estimated_savings_usd += usd.max(0.0);
        entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_add(tokens);
        entry.actual_cost_usd += actual_cost_usd.max(0.0);
        entry.total_tokens_sent = entry.total_tokens_sent.saturating_add(total_tokens_sent);
    }

    fn subtract_daily_delta(
        &mut self,
        day_key: &str,
        usd: f64,
        tokens: u64,
        actual_cost_usd: f64,
        total_tokens_sent: u64,
    ) {
        let mut should_remove = false;
        if let Some(entry) = self.daily_savings.get_mut(day_key) {
            entry.estimated_savings_usd = (entry.estimated_savings_usd - usd.max(0.0)).max(0.0);
            entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_sub(tokens);
            entry.actual_cost_usd = (entry.actual_cost_usd - actual_cost_usd.max(0.0)).max(0.0);
            entry.total_tokens_sent = entry.total_tokens_sent.saturating_sub(total_tokens_sent);
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
    ) {
        if usd <= 0.0 && tokens == 0 && actual_cost_usd <= 0.0 && total_tokens_sent == 0 {
            return;
        }
        let entry = self.hourly_savings.entry(hour_key.to_string()).or_default();
        entry.estimated_savings_usd += usd.max(0.0);
        entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_add(tokens);
        entry.actual_cost_usd += actual_cost_usd.max(0.0);
        entry.total_tokens_sent = entry.total_tokens_sent.saturating_add(total_tokens_sent);
    }

    fn subtract_hourly_delta(
        &mut self,
        hour_key: &str,
        usd: f64,
        tokens: u64,
        actual_cost_usd: f64,
        total_tokens_sent: u64,
    ) {
        let mut should_remove = false;
        if let Some(entry) = self.hourly_savings.get_mut(hour_key) {
            entry.estimated_savings_usd = (entry.estimated_savings_usd - usd.max(0.0)).max(0.0);
            entry.estimated_tokens_saved = entry.estimated_tokens_saved.saturating_sub(tokens);
            entry.actual_cost_usd = (entry.actual_cost_usd - actual_cost_usd.max(0.0)).max(0.0);
            entry.total_tokens_sent = entry.total_tokens_sent.saturating_sub(total_tokens_sent);
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

    fn persisted_state(&self) -> PersistedSavingsState {
        PersistedSavingsState {
            schema_version: 3,
            session_requests: self.session_requests,
            session_estimated_savings_usd: self.session_estimated_savings_usd,
            session_estimated_tokens_saved: self.session_estimated_tokens_saved,
            session_savings_pct: self.session_savings_pct,
            lifetime_requests: self.lifetime_requests,
            lifetime_estimated_savings_usd: self.lifetime_estimated_savings_usd,
            lifetime_estimated_tokens_saved: self.lifetime_estimated_tokens_saved,
            last_observation: self.last_observation.clone(),
            display_session_baseline: self.display_session_baseline.clone(),
            session_savings_history: self.session_savings_history.clone(),
            session_hourly_buckets: self.session_hourly_buckets.clone(),
            daily_savings: self.daily_savings.clone(),
            hourly_savings: self.hourly_savings.clone(),
        }
    }

    fn persist_state(&mut self) -> Result<()> {
        let serialized = serde_json::to_vec_pretty(&self.persisted_state())
            .context("serializing savings state")?;
        std::fs::write(&self.state_path, serialized)
            .with_context(|| format!("writing {}", self.state_path.display()))?;
        Ok(())
    }
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
        let has_activity =
            bucket.estimated_tokens_saved > 0 || bucket.estimated_savings_usd > 0.0;
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

fn lifetime_usd_milestones_crossed(previous_usd: f64, current_usd: f64) -> Vec<u64> {
    let previous = previous_usd.max(0.0).floor() as u64;
    let current = current_usd.max(0.0).floor() as u64;
    if current <= previous {
        return Vec::new();
    }

    let mut milestones = FIRST_LIFETIME_USD_MILESTONES
        .into_iter()
        .filter(|threshold| previous < *threshold && current >= *threshold)
        .collect::<Vec<_>>();

    let first_repeating_index = previous / REPEATING_LIFETIME_USD_MILESTONE_STEP + 1;
    let last_repeating_index = current / REPEATING_LIFETIME_USD_MILESTONE_STEP;
    for index in first_repeating_index..=last_repeating_index {
        let dollars = index.saturating_mul(REPEATING_LIFETIME_USD_MILESTONE_STEP);
        if !milestones.contains(&dollars) {
            milestones.push(dollars);
        }
    }

    milestones
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
}

#[derive(Debug, Default, Clone, Copy)]
struct HeadroomSavingsRollupPoint {
    timestamp: chrono::DateTime<Utc>,
    tokens_saved: u64,
    compression_savings_usd_delta: f64,
    total_input_tokens_delta: u64,
    total_input_cost_usd_delta: f64,
}

#[derive(Debug, Default, Clone)]
struct HeadroomSavingsHistoryResponse {
    lifetime_estimated_savings_usd: Option<f64>,
    lifetime_estimated_tokens_saved: Option<u64>,
    hourly: Vec<HeadroomSavingsRollupPoint>,
    daily: Vec<HeadroomSavingsRollupPoint>,
}

impl HeadroomSavingsHistoryResponse {
    fn daily_savings(&self) -> Vec<DailySavingsPoint> {
        self.daily
            .iter()
            .map(|point| DailySavingsPoint {
                date: local_day_key(point.timestamp.with_timezone(&Local)),
                estimated_savings_usd: point.compression_savings_usd_delta,
                estimated_tokens_saved: point.tokens_saved,
                actual_cost_usd: point.total_input_cost_usd_delta,
                total_tokens_sent: point.total_input_tokens_delta,
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
                actual_cost_usd: point.total_input_cost_usd_delta,
                total_tokens_sent: point.total_input_tokens_delta,
            })
            .collect()
    }
}

fn fetch_headroom_dashboard_stats() -> Option<HeadroomDashboardStats> {
    if !is_headroom_proxy_reachable() {
        return None;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?;

    let hosts = ["127.0.0.1", "localhost"];

    for host in hosts {
        let url = format!("http://{host}:6767/stats");
        let response = match client.get(&url).send() {
            Ok(response) if response.status().is_success() => response,
            _ => continue,
        };

        let body = match response.text() {
            Ok(body) => body,
            Err(_) => continue,
        };

        if let Some(parsed) = parse_headroom_stats_from_json(&body) {
            return Some(parsed);
        }
    }

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
    let total_before_compression =
        value_at_path_u64(&root, &["tokens", "total_before_compression"]).or_else(|| {
            find_u64_key_recursive(
                &root,
                &["totalBeforeCompression", "total_before_compression"],
            )
        });
    let session_savings_pct = path_savings_pct.or_else(|| {
        total_before_compression.and_then(|total_before| {
            tokens.and_then(|saved| {
                if total_before > 0 {
                    Some(saved as f64 / total_before as f64 * 100.0)
                } else {
                    None
                }
            })
        })
    });
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
    let session_total_tokens_sent = total_after_compression.filter(|value| *value > 0);
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

    if requests.is_none()
        && tokens.is_none()
        && usd.is_none()
        && session_total_tokens_sent.is_none()
        && actual_cost_usd.is_none()
    {
        None
    } else {
        Some(HeadroomDashboardStats {
            session_requests: requests,
            session_estimated_savings_usd: usd,
            session_estimated_tokens_saved: tokens,
            session_savings_pct,
            session_actual_cost_usd: actual_cost_usd.map(|value| value.max(0.0)),
            session_total_tokens_sent,
            savings_history,
        })
    }
}

fn parse_headroom_stats_history_from_json(body: &str) -> Option<HeadroomSavingsHistoryResponse> {
    let root = serde_json::from_str::<Value>(body).ok()?;
    let lifetime_estimated_tokens_saved = value_at_path_u64(&root, &["lifetime", "tokens_saved"]);
    let lifetime_estimated_savings_usd =
        value_at_path_f64(&root, &["lifetime", "compression_savings_usd"]);
    let hourly = value_at_path(&root, &["series", "hourly"])
        .and_then(parse_savings_rollup_series)
        .unwrap_or_default();
    let daily = value_at_path(&root, &["series", "daily"])
        .and_then(parse_savings_rollup_series)
        .unwrap_or_default();

    if lifetime_estimated_tokens_saved.is_none()
        && lifetime_estimated_savings_usd.is_none()
        && hourly.is_empty()
        && daily.is_empty()
    {
        None
    } else {
        Some(HeadroomSavingsHistoryResponse {
            lifetime_estimated_savings_usd: lifetime_estimated_savings_usd
                .map(|value| value.max(0.0)),
            lifetime_estimated_tokens_saved,
            hourly,
            daily,
        })
    }
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
    })
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
    timestamp.format("%Y-%m-%d").to_string()
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

    if total_tokens > 0 && total_tokens_sent > 0 {
        let keys = buckets.keys().cloned().collect::<Vec<_>>();
        for key in keys.iter() {
            let bucket = buckets.get_mut(key).expect("bucket exists");
            bucket.total_tokens_sent = ((bucket.estimated_tokens_saved as u128
                * total_tokens_sent as u128)
                / total_tokens as u128) as u64;
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
) -> Vec<(String, DailySavingsBucket)> {
    derive_session_buckets_with_key(stats, history, local_hour_key)
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
    is_headroom_proxy_reachable()
}

/// Turn a raw `last_startup_error` string (the anyhow chain from
/// `start_headroom_background`) into a short user-friendly explanation plus a
/// suggested next step. Returns `None` for shapes we don't recognize, in which
/// case the UI falls back to a generic "open logs" prompt.
pub(crate) fn classify_startup_error(raw: &str) -> Option<String> {
    if raw.contains("is occupied by a non-headroom process") {
        return Some(
            "Port 6768 is in use by another app on your machine. \
             Run `lsof -iTCP:6768 -sTCP:LISTEN` in a terminal to find it, \
             quit that process, then click Retry."
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

fn is_headroom_proxy_reachable() -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };

    ["127.0.0.1", "localhost"].iter().any(|host| {
        client
            .get(format!("http://{host}:6767/readyz"))
            .send()
            .map(|response| response.status().is_success())
            .unwrap_or(false)
    })
}

fn kill_processes_by_command_pattern(pattern: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("pkill")
            .args(["-f", pattern])
            .status()
            .with_context(|| format!("running pkill for pattern '{pattern}'"))?;

        if status.success() || status.code() == Some(1) {
            return Ok(());
        }

        return Err(anyhow!(
            "pkill exited with status {:?} for pattern '{}'",
            status.code(),
            pattern
        ));
    }

    #[cfg(not(unix))]
    {
        let _ = pattern;
        Ok(())
    }
}

/// Merge daily savings from tracker (pre-cutoff) and native headroom history (post-cutoff).
/// For days before `cutoff_date` (exclusive), the tracker is preferred.
/// For days on/after `cutoff_date`, native history is preferred.
/// Falls back to whichever source has data when the preferred one is absent.
fn merge_daily_savings(
    tracker: Vec<DailySavingsPoint>,
    history: Vec<DailySavingsPoint>,
    cutoff_date: &str,
) -> Vec<DailySavingsPoint> {
    use std::collections::BTreeMap;
    let mut by_date: BTreeMap<String, DailySavingsPoint> = BTreeMap::new();
    // Post-cutoff: history wins, tracker fills gaps so today's local activity still shows.
    // Pre-cutoff: tracker-only; history is ignored to avoid pulling in pre-v6 schema drift.
    for p in history {
        if p.date.as_str() >= cutoff_date {
            by_date.insert(p.date.clone(), p);
        }
    }
    for p in tracker {
        if p.date.as_str() < cutoff_date {
            by_date.insert(p.date.clone(), p);
        } else {
            by_date.entry(p.date.clone()).or_insert(p);
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
            by_hour.entry(p.hour.clone()).or_insert(p);
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

    use chrono::{Datelike, Local, TimeZone, Utc};

    use crate::storage::{config_file, ensure_data_dirs, telemetry_file};

    use crate::models::{
        ActivityEvent, BootstrapProgress, DailySavingsPoint, HourlySavingsPoint,
        RuntimeUpgradeFailure, UpgradeFailurePhase,
    };
    use crate::tool_manager::BootstrapStepUpdate;

    use super::{
        aggregate_weekly_totals, apply_bootstrap_step, begin_bootstrap_transition,
        bootstrap_complete_state, bootstrap_failed_state, classify_startup_error,
        lifetime_token_milestones_crossed, lifetime_usd_milestones_crossed, merge_daily_savings,
        merge_hourly_savings, parse_headroom_stats_from_json,
        parse_headroom_stats_history_from_json, AppState, ClaudeProjectScan, DailySavingsBucket,
        HeadroomDashboardStats, HeadroomSavingsHistoryPoint, PersistedSavingsState,
        SavingsObservation, SavingsTracker,
    };

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
    fn classify_startup_error_foreign_port() {
        let raw = "port 6768 is occupied by a non-headroom process (pid 1234 node); cannot start proxy.";
        let hint = classify_startup_error(raw).expect("foreign port should classify");
        assert!(hint.contains("lsof -iTCP:6768"), "got: {hint}");
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
        let _ = std::fs::remove_file(&path);
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
            lifetime_estimated_savings_usd: 0.0,
            lifetime_estimated_tokens_saved: 0,
            last_observation: None,
            display_session_baseline: None,
            session_savings_history: Vec::new(),
            session_hourly_buckets: std::collections::BTreeMap::new(),
            daily_savings: std::collections::BTreeMap::new(),
            hourly_savings: std::collections::BTreeMap::new(),
            pending_lifetime_token_milestones: Vec::new(),
            pending_lifetime_usd_milestones: Vec::new(),
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
    fn lifetime_usd_milestones_first_and_repeating() {
        assert_eq!(lifetime_usd_milestones_crossed(0.0, 5.0), Vec::<u64>::new());
        assert_eq!(lifetime_usd_milestones_crossed(9.99, 10.01), vec![10]);
        assert_eq!(
            lifetime_usd_milestones_crossed(5.0, 120.0),
            vec![10, 50, 100]
        );
        assert_eq!(lifetime_usd_milestones_crossed(200.0, 205.0), Vec::<u64>::new());
        assert_eq!(
            lifetime_usd_milestones_crossed(199.5, 301.0),
            vec![200, 300]
        );
    }

    #[test]
    fn savings_tracker_queues_pending_usd_milestones_on_observe() {
        let mut tracker = make_tracker();
        tracker.lifetime_estimated_savings_usd = 7.5;
        let stats = HeadroomDashboardStats {
            session_requests: Some(1),
            session_estimated_savings_usd: Some(60.0),
            session_estimated_tokens_saved: Some(1),
            session_savings_pct: Some(1.0),
            session_actual_cost_usd: Some(0.0),
            session_total_tokens_sent: Some(0),
            savings_history: Vec::new(),
        };
        tracker.observe(&stats);
        let milestones = tracker.take_pending_lifetime_usd_milestones();
        assert_eq!(milestones, vec![10, 50]);
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
            },
        );
        daily.insert(
            "2026-04-20".into(),
            DailySavingsBucket {
                estimated_savings_usd: 2.5,
                estimated_tokens_saved: 200,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
            },
        );
        daily.insert(
            "2026-04-23".into(),
            DailySavingsBucket {
                estimated_savings_usd: 1.0,
                estimated_tokens_saved: 100,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
            },
        );
        daily.insert(
            "2026-04-26".into(),
            DailySavingsBucket {
                estimated_savings_usd: 0.0,
                estimated_tokens_saved: 0, // zero activity day — not counted
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
            },
        );
        daily.insert(
            "2026-04-27".into(), // outside window (today Monday)
            DailySavingsBucket {
                estimated_savings_usd: 99.0,
                estimated_tokens_saved: 9999,
                actual_cost_usd: 0.0,
                total_tokens_sent: 0,
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
    fn observe_activity_separates_fresh_from_recent_across_calls() {
        use crate::models::TransformationFeedEvent;
        let base_dir = temp_test_dir("headroom-activity-observation");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        let transformation = TransformationFeedEvent {
            request_id: Some("r1".into()),
            timestamp: Some("2026-04-22T10:00:00Z".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-7".into()),
            input_tokens_original: Some(1000),
            input_tokens_optimized: Some(200),
            tokens_saved: Some(800),
            savings_percent: Some(80.0),
            transforms_applied: vec!["kompress".into()],
            workspace: Some("/Users/u/Code/demo".into()),
            turn_id: None,
        };

        let first = state.observe_activity_from_transformations(&[transformation.clone()]);
        assert!(!first.fresh.is_empty(), "first observation should emit fresh events");
        assert!(
            first
                .fresh
                .iter()
                .any(|e| matches!(e, ActivityEvent::NewModel(_))),
            "first seen model should fire"
        );
        assert_eq!(first.fresh.len(), first.recent.len());

        let second = state.observe_activity_from_transformations(&[transformation]);
        assert!(
            second.fresh.is_empty(),
            "second observation of same transformation should emit no fresh events"
        );
        assert_eq!(
            second.recent.len(),
            first.recent.len(),
            "recent history persists across calls"
        );

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
    fn runtime_maintenance_plan_prefers_requirements_repair_when_only_lock_is_stale() {
        let base_dir = temp_test_dir("headroom-maintenance-repair");
        let state = AppState::new_in(base_dir.clone()).expect("app state");
        write_headroom_receipt(&base_dir, "0.9.7", "stale");

        let plan = state.runtime_maintenance_plan_for_app_version(env!("CARGO_PKG_VERSION"));
        assert!(matches!(plan, Some(super::RuntimeMaintenancePlan::RequirementsRepair)));

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
                assert_eq!(release.version(), "0.9.7");
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
    fn lifetime_token_milestones_include_firsts_and_repeating_tens() {
        assert_eq!(
            lifetime_token_milestones_crossed(0, 5_000_000),
            vec![100_000, 1_000_000, 5_000_000]
        );
        assert_eq!(
            lifetime_token_milestones_crossed(9_500_000, 21_000_000),
            vec![10_000_000, 20_000_000]
        );
        assert_eq!(
            lifetime_token_milestones_crossed(0, 150_000),
            vec![100_000]
        );
    }

    #[test]
    fn tracker_queues_new_lifetime_token_milestones_once() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
                session_requests: Some(1),
                session_estimated_savings_usd: Some(1.0),
                session_estimated_tokens_saved: Some(12_000_000),
                session_savings_pct: Some(50.0),
                session_actual_cost_usd: Some(0.5),
                session_total_tokens_sent: Some(12_000_000),
                savings_history: Vec::new(),
            })
            .expect("snapshot");

        assert_eq!(
            tracker.take_pending_lifetime_token_milestones(),
            vec![100_000, 1_000_000, 5_000_000, 10_000_000]
        );
        assert!(tracker.take_pending_lifetime_token_milestones().is_empty());

        tracker
            .observe(&HeadroomDashboardStats {
                session_requests: Some(2),
                session_estimated_savings_usd: Some(2.0),
                session_estimated_tokens_saved: Some(21_000_000),
                session_savings_pct: Some(50.0),
                session_actual_cost_usd: Some(1.0),
                session_total_tokens_sent: Some(21_000_000),
                savings_history: Vec::new(),
            })
            .expect("snapshot");

        assert_eq!(
            tracker.take_pending_lifetime_token_milestones(),
            vec![20_000_000]
        );
    }

    #[test]
    fn dashboard_read_path_preserves_pending_milestones_for_analytics() {
        // Regression guard: `state.dashboard()` (tray updater, bootstrap
        // finalize, account activation) must not drain pending milestones.
        // Only `dashboard_with_pending_milestones()` — the path that actually
        // fires the aptabase event, pricing report, and in-app notification —
        // may consume them. A prior refactor drained on every call, so the
        // tray updater's 5s heartbeat silently ate ~50-100% of crossings.
        let base_dir = temp_test_dir("headroom-milestone-preservation");
        let state = AppState::new_in(base_dir.clone()).expect("app state");

        let stats = HeadroomDashboardStats {
            session_requests: Some(1),
            session_estimated_savings_usd: Some(1.0),
            session_estimated_tokens_saved: Some(1_500_000),
            session_savings_pct: Some(50.0),
            session_actual_cost_usd: Some(0.5),
            session_total_tokens_sent: Some(1_500_000),
            savings_history: Vec::new(),
        };

        let (_, _, _, read_only) = state
            .record_savings_snapshot(&stats, false)
            .expect("snapshot");
        assert!(
            read_only.token.is_empty(),
            "read-only path must not surface milestones"
        );
        assert!(read_only.usd.is_empty());

        let (_, _, _, drained) = state
            .record_savings_snapshot(&stats, true)
            .expect("snapshot");
        assert_eq!(
            drained.token,
            vec![100_000, 1_000_000],
            "drain=true must surface milestones queued by the earlier read-only observe"
        );

        let (_, _, _, drained_again) = state
            .record_savings_snapshot(&stats, true)
            .expect("snapshot");
        assert!(
            drained_again.token.is_empty(),
            "second drain finds nothing: milestones fire exactly once"
        );
        assert!(drained_again.usd.is_empty());

        fs::remove_dir_all(base_dir).expect("remove temp dir");
    }

    #[test]
    fn session_counters_follow_headroom_stats() {
        let mut tracker = make_tracker();

        let first = tracker
            .observe(&HeadroomDashboardStats {
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
        assert_eq!(first.lifetime_estimated_tokens_saved, 1_200);
        assert!((first.lifetime_estimated_savings_usd - 1.2).abs() < 1e-9);

        let second = tracker
            .observe(&HeadroomDashboardStats {
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
        assert_eq!(second.lifetime_estimated_tokens_saved, 1_500);
        assert!((second.lifetime_estimated_savings_usd - 1.5).abs() < 1e-9);
    }

    #[test]
    fn new_session_resets_live_session_and_keeps_lifetime() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
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
        assert_eq!(reset.lifetime_estimated_tokens_saved, 1_200);
        assert!((reset.lifetime_estimated_savings_usd - 1.2).abs() < 1e-9);
    }

    #[test]
    fn first_observation_backfills_daily_history_from_headroom() {
        let mut tracker = make_tracker();
        tracker
            .observe(&HeadroomDashboardStats {
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

        tracker
            .observe(&HeadroomDashboardStats {
                session_requests: Some(4),
                session_estimated_savings_usd: Some(0.5),
                session_estimated_tokens_saved: Some(1_000),
                session_savings_pct: Some(25.0),
                session_actual_cost_usd: Some(0.3),
                session_total_tokens_sent: Some(3_000),
                savings_history: vec![
                    history_point_at(today.year(), today.month(), today.day(), 8, 0),
                    history_point_at(today.year(), today.month(), today.day(), 9, 400),
                    history_point_at(today.year(), today.month(), today.day(), 15, 1_000),
                ],
            })
            .expect("snapshot");

        let today_key = today.format("%Y-%m-%d").to_string();
        let hourly = tracker
            .hourly_savings()
            .into_iter()
            .filter(|point| point.hour.starts_with(&format!("{today_key}T")))
            .collect::<Vec<_>>();
        let expected_first_hour = Utc
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 9, 0, 0)
            .single()
            .expect("first hour")
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:00")
            .to_string();
        let expected_second_hour = Utc
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 15, 0, 0)
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

        assert_eq!(parsed.lifetime_estimated_tokens_saved, Some(205));
        assert_eq!(parsed.lifetime_estimated_savings_usd, Some(0.205));
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
            },
        );
        tracker.hourly_savings.insert(
            "2026-03-24T13:00".into(),
            DailySavingsBucket {
                estimated_savings_usd: 20.0,
                estimated_tokens_saved: 6_000_000,
                actual_cost_usd: 0.01,
                total_tokens_sent: 600_000,
            },
        );
        tracker.daily_savings.insert(
            "2026-03-24".into(),
            DailySavingsBucket {
                estimated_savings_usd: 20.0,
                estimated_tokens_saved: 6_000_000,
                actual_cost_usd: 0.01,
                total_tokens_sent: 600_000,
            },
        );

        tracker
            .observe(&HeadroomDashboardStats {
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
    fn rolling_window_does_not_dump_unattributable_remainder_into_last_hour() {
        let mut tracker = make_tracker();

        tracker
            .observe(&HeadroomDashboardStats {
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
                session_requests: Some(11),
                session_estimated_savings_usd: Some(1.2),
                session_estimated_tokens_saved: Some(1_200),
                session_savings_pct: Some(20.0),
                session_actual_cost_usd: None,
                session_total_tokens_sent: None,
                savings_history: Vec::new(),
            })
            .expect("second snapshot");

        assert!((second.lifetime_estimated_savings_usd - 1.2).abs() < 1e-9);
        assert_eq!(second.lifetime_estimated_tokens_saved, 1_200);
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
        tracker.lifetime_estimated_savings_usd = 5.0;
        tracker.lifetime_estimated_tokens_saved = 1_000;

        let snapshot = tracker
            .observe(&HeadroomDashboardStats {
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
        assert_eq!(snapshot.lifetime_estimated_tokens_saved, 1_100);
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
            lifetime_estimated_savings_usd: 2.4,
            lifetime_estimated_tokens_saved: 2_400,
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
            session_hourly_buckets: std::collections::BTreeMap::new(),
            daily_savings: std::collections::BTreeMap::new(),
            hourly_savings: std::collections::BTreeMap::new(),
        };
        std::fs::write(
            config_file(&base_dir, "savings-state.json"),
            serde_json::to_vec_pretty(&persisted).expect("serialize persisted state"),
        )
        .expect("write persisted state");

        let tracker = SavingsTracker::load_or_create(&base_dir).expect("load tracker");
        assert!((tracker.lifetime_estimated_savings_usd - 0.0).abs() < 1e-9);
        assert_eq!(tracker.lifetime_estimated_tokens_saved, 0);
        assert_eq!(tracker.lifetime_requests, 0);

        let _ = std::fs::remove_dir_all(base_dir);
    }

    fn daily(date: &str, tokens: u64, usd: f64) -> DailySavingsPoint {
        DailySavingsPoint {
            date: date.to_string(),
            estimated_tokens_saved: tokens,
            estimated_savings_usd: usd,
            actual_cost_usd: 0.0,
            total_tokens_sent: 0,
        }
    }

    fn hourly(hour: &str, tokens: u64) -> HourlySavingsPoint {
        HourlySavingsPoint {
            hour: hour.to_string(),
            estimated_tokens_saved: tokens,
            estimated_savings_usd: 0.0,
            actual_cost_usd: 0.0,
            total_tokens_sent: 0,
        }
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
}
