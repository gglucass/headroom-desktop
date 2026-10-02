use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rand::Rng;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Url;
use serde_json::{json, Map, Value};
use tauri::{webview_version, AppHandle, Manager};

const HEADROOM_APTABASE_APP_KEY: Option<&str> = option_env!("HEADROOM_APTABASE_APP_KEY");

// Allowlist to stay under the Aptabase free-plan event quota. Only version
// visibility (app_started carries appVersion/headroom_ai_version), the billing
// funnel, and the savings milestone are worth ingesting. Everything else
// (runtime lifecycle, bootstrap, errors, misc UI) is dropped at the chokepoint.
// ponytail: allowlist over deleting 30 call sites; widen this set if a dropped
// event becomes worth tracking again.
const ALLOWED_EVENTS: &[&str] = &[
    "app_started",
    "account_activated",
    "checkout_started",
    "subscription_plan_changed",
    "subscription_reactivated",
    "invite_code_used",
    "lifetime_tokens_saved_milestone_reached",
    // At most once per install (persisted flag); measures how many users hit
    // the "setup finished but no traffic ever" state.
    "onboarding_recovery_nudge_shown",
    // Evidence-based sibling: Claude sessions grew while nothing was routed.
    "unrouted_usage_nudge_shown",
    // Emitted by the apply_client_setup command / watchdog repair since 0.8.x
    // but missing here, so they were silently dropped by the gate below.
    // Carries client_id + verified + proxy_reachable: the per-OS
    // "applied but never verified/used" slice.
    "client_setup_applied",
    "client_setup_auto_repaired",
    // Addon adoption (one event per addon, fired from install/enable/uninstall).
    "markitdown_installed",
    "markitdown_enabled",
    "markitdown_disabled",
    "markitdown_uninstalled",
    "rtk_installed",
    "rtk_enabled",
    "rtk_disabled",
    "rtk_uninstalled",
    "ponytail_installed",
    "ponytail_enabled",
    "ponytail_disabled",
    "ponytail_uninstalled",
    "caveman_installed",
    "caveman_enabled",
    "caveman_disabled",
    "caveman_uninstalled",
    "chisle_installed",
    "chisle_enabled",
    "chisle_disabled",
    "chisle_uninstalled",
    // Open-source plugin/CLI coexistence, at most once per app start and only
    // when something is actually present. Tells us how many users run the OSS
    // Claude Code plugin next to the app, whether its bare hook was absorbed, and
    // how many have an OSS proxy on :8787 taking traffic we never see.
    "oss_plugin_detected",
    // Feature engagement: learn runs (per run, `agent` property) and Activity
    // tab opens (once per app run, mirroring app_started's cadence).
    "headroom_learn_run",
    // One-click Claude Code install from the no-clients wizard panel; carries
    // ok=true/false so the button's success rate is measurable.
    "claude_code_installer_run",
    "activity_tab_opened",
];
const SESSION_TIMEOUT_SECS: i64 = 4 * 60 * 60;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 10;
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
#[cfg(debug_assertions)]
const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 2;
#[cfg(not(debug_assertions))]
const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 60;

pub struct AnalyticsClient {
    enabled: bool,
    session: Mutex<TrackingSession>,
    dispatcher: Mutex<Option<DispatcherHandle>>,
    system_props: SystemProperties,
    app_version: String,
    headroom_ai_version: Mutex<Option<String>>,
}

struct DispatcherHandle {
    sender: Sender<WorkerMessage>,
    worker: JoinHandle<()>,
}

#[derive(Clone)]
struct AnalyticsConfig {
    app_key: String,
    ingest_api_url: Url,
    flush_interval: Duration,
}

#[derive(Clone)]
struct TrackingSession {
    id: String,
    last_touch: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
struct SystemProperties {
    is_debug: bool,
    os_name: String,
    os_version: String,
    locale: String,
    engine_name: String,
    engine_version: String,
}

enum WorkerMessage {
    Event(Value),
    Shutdown,
}

impl AnalyticsClient {
    pub fn new(app_version: String) -> Self {
        let system_props = system_properties();
        let config = AnalyticsConfig::from_env();
        let dispatcher = config.as_ref().map(spawn_dispatcher);

        Self {
            enabled: config.is_some(),
            session: Mutex::new(TrackingSession::new()),
            dispatcher: Mutex::new(dispatcher),
            system_props,
            app_version,
            headroom_ai_version: Mutex::new(None),
        }
    }

    pub fn set_headroom_ai_version(&self, version: Option<String>) {
        *self.headroom_ai_version.lock() = version.and_then(non_empty_string);
    }

    pub fn track_event(&self, name: &str, properties: Option<Value>) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        let normalized_name = normalize_event_name(name);
        if normalized_name.is_empty() {
            return Ok(());
        }
        if !ALLOWED_EVENTS.contains(&normalized_name.as_str()) {
            return Ok(());
        }

        let mut props = sanitize_properties(properties)
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        if let Some(version) = self.headroom_ai_version.lock().clone() {
            props
                .entry("headroom_ai_version".to_string())
                .or_insert(Value::String(version));
        }
        let props_value = if props.is_empty() {
            Value::Null
        } else {
            Value::Object(props)
        };

        let event = json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "sessionId": self.session_id(),
            "eventName": normalized_name,
            "systemProps": {
                "isDebug": self.system_props.is_debug,
                "osName": self.system_props.os_name,
                "osVersion": self.system_props.os_version,
                "locale": self.system_props.locale,
                "engineName": self.system_props.engine_name,
                "engineVersion": self.system_props.engine_version,
                "appVersion": self.app_version,
                "sdkVersion": "headroom-desktop"
            },
            "props": props_value
        });

        let dispatcher = self.dispatcher.lock();
        let handle = dispatcher
            .as_ref()
            .ok_or_else(|| "analytics dispatcher unavailable".to_string())?;
        handle
            .sender
            .send(WorkerMessage::Event(event))
            .map_err(|_| "analytics dispatcher stopped".to_string())
    }

    pub fn shutdown(&self) {
        if !self.enabled {
            return;
        }

        let Some(handle) = self.dispatcher.lock().take() else {
            return;
        };

        let _ = handle.sender.send(WorkerMessage::Shutdown);
        // Quit calls this on the main thread, so never join unbounded: a
        // request already in flight (or a periodic flush that was running when
        // Shutdown arrived) ignores the worker's own deadline for up to the
        // 10s HTTP timeout per chunk. Past the deadline the worker is left
        // detached; process exit reclaims it.
        let deadline = std::time::Instant::now() + SHUTDOWN_DEADLINE;
        while !handle.worker.is_finished() {
            if std::time::Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = handle.worker.join();
    }

    fn session_id(&self) -> String {
        let mut session = self.session.lock();
        let now = chrono::Utc::now();
        if (now - session.last_touch).num_seconds() > SESSION_TIMEOUT_SECS {
            *session = TrackingSession::new();
        } else {
            session.last_touch = now;
        }
        session.id.clone()
    }
}

impl TrackingSession {
    fn new() -> Self {
        Self {
            id: new_session_id(),
            last_touch: chrono::Utc::now(),
        }
    }
}

impl AnalyticsConfig {
    fn from_env() -> Option<Self> {
        let app_key = resolve_app_key()?;
        let mut parts = app_key.split('-');
        let _app = parts.next()?;
        let region = parts.next()?;
        let _suffix = parts.next()?;
        if parts.next().is_some() {
            return None;
        }

        let ingest_api_url = match region {
            "EU" => "https://eu.aptabase.com/api/v0/events",
            "US" => "https://us.aptabase.com/api/v0/events",
            "DEV" => "http://localhost:3000/api/v0/events",
            _ => return None,
        };

        Some(Self {
            app_key,
            ingest_api_url: ingest_api_url.parse().ok()?,
            flush_interval: Duration::from_secs(DEFAULT_FLUSH_INTERVAL_SECS),
        })
    }
}

pub fn resolve_app_key() -> Option<String> {
    std::env::var("HEADROOM_APTABASE_APP_KEY")
        .ok()
        .and_then(non_empty_string)
        .or_else(|| HEADROOM_APTABASE_APP_KEY.and_then(|value| non_empty_string(value.to_string())))
}

// The client is managed part-way through `setup()`, which runs AFTER the
// webview windows exist. On Windows, a call that pumps the message loop during
// webview creation re-enters us before that `manage()`, and `state()` panics
// inside a callback that cannot unwind -> process abort (Sentry RUST-HF/HG).
// Analytics is never worth a crash, so every accessor tolerates a missing
// client and drops the event.
fn client<'a>(app: &'a AppHandle, what: &str) -> Option<tauri::State<'a, AnalyticsClient>> {
    let client = app.try_state::<AnalyticsClient>();
    if client.is_none() {
        log_stderr(format_args!(
            "analytics not ready yet, dropping {}",
            what.trim()
        ));
    }
    client
}

pub fn track_event(app: &AppHandle, name: &str, properties: Option<Value>) {
    let Some(client) = client(app, name) else {
        return;
    };
    if let Err(err) = client.track_event(name, properties) {
        log_stderr(format_args!(
            "failed to track analytics event {}: {err}",
            name.trim()
        ));
    }
}

pub fn set_headroom_ai_version(app: &AppHandle, version: Option<String>) {
    if let Some(client) = client(app, "headroom_ai_version") {
        client.set_headroom_ai_version(version);
    }
}

fn log_stderr(args: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "{args}");
}

pub fn shutdown(app: &AppHandle) {
    if let Some(client) = client(app, "shutdown") {
        client.shutdown();
    }
}

fn spawn_dispatcher(config: &AnalyticsConfig) -> DispatcherHandle {
    let (sender, receiver) = mpsc::channel();
    let config = config.clone();
    let worker = thread::spawn(move || dispatcher_loop(receiver, config));
    DispatcherHandle { sender, worker }
}

fn dispatcher_loop(receiver: Receiver<WorkerMessage>, config: AnalyticsConfig) {
    let http_client = build_http_client(&config);
    let mut queue = VecDeque::new();

    loop {
        match receiver.recv_timeout(config.flush_interval) {
            Ok(WorkerMessage::Event(event)) => {
                queue.push_back(event);
            }
            Ok(WorkerMessage::Shutdown) => {
                // The app's exit handler joins this thread: bound the final
                // flush so quitting offline can't hang the app for
                // chunks × HTTP timeout (10s each). Unsent events are dropped
                // — it's telemetry, and the process is exiting.
                let deadline = std::time::Instant::now() + SHUTDOWN_DEADLINE;
                flush_queue(&http_client, &config, &mut queue, Some(deadline));
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_queue(&http_client, &config, &mut queue, None);
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_queue(&http_client, &config, &mut queue, None);
                return;
            }
        }
    }
}

fn build_http_client(config: &AnalyticsConfig) -> Client {
    let mut headers = HeaderMap::new();
    let app_key_header =
        HeaderValue::from_str(&config.app_key).expect("failed to define App Key header value");
    headers.insert("App-Key", app_key_header);
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));

    // proxy-ok: analytics ingest is a public endpoint; a corporate proxy must be honored
    Client::builder()
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .default_headers(headers)
        .user_agent(user_agent())
        .build()
        .expect("could not build analytics http client")
}

fn flush_queue(
    client: &Client,
    config: &AnalyticsConfig,
    queue: &mut VecDeque<Value>,
    deadline: Option<std::time::Instant>,
) {
    if queue.is_empty() {
        return;
    }

    let mut failed = Vec::new();
    while !queue.is_empty() {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let chunk_len = queue.len().min(25);
        let events: Vec<Value> = queue.drain(..chunk_len).collect();
        let response = client
            .post(config.ingest_api_url.clone())
            .json(&events)
            .send();
        match response {
            Ok(response) if response.status().is_success() => {}
            Ok(response) if response.status().is_server_error() => {
                log_stderr(format_args!(
                    "aptabase server error {} while sending {} event(s)",
                    response.status(),
                    events.len()
                ));
                failed.extend(events);
            }
            Ok(response) => {
                log_stderr(format_args!(
                    "aptabase rejected {} event(s) with status {}",
                    events.len(),
                    response.status()
                ));
            }
            Err(err) => {
                log_stderr(format_args!(
                    "aptabase send failed for {} event(s): {err}",
                    events.len()
                ));
                failed.extend(events);
            }
        }
    }

    for event in failed {
        queue.push_back(event);
    }
}

fn normalize_event_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn sanitize_properties(properties: Option<Value>) -> Option<Value> {
    let Value::Object(object) = properties? else {
        return None;
    };

    let mut sanitized = Map::new();
    for (key, value) in object {
        let normalized_key = key.trim();
        if normalized_key.is_empty() {
            continue;
        }

        let Some(sanitized_value) = sanitize_value(value) else {
            continue;
        };
        sanitized.insert(normalized_key.to_string(), sanitized_value);
    }

    if sanitized.is_empty() {
        None
    } else {
        Some(Value::Object(sanitized))
    }
}

fn sanitize_value(value: Value) -> Option<Value> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(Value::String(trimmed.to_string()))
            }
        }
        Value::Number(number) => Some(Value::Number(number)),
        Value::Bool(flag) => Some(Value::String(if flag { "true" } else { "false" }.into())),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn system_properties() -> SystemProperties {
    let info = os_info::get();
    SystemProperties {
        is_debug: cfg!(debug_assertions),
        os_name: match info.os_type() {
            os_info::Type::Macos => "macOS".to_string(),
            os_info::Type::Windows => "Windows".to_string(),
            _ if std::env::var("container").is_ok() => "Flatpak".to_string(),
            _ => info.os_type().to_string(),
        },
        os_version: info.version().to_string(),
        locale: sys_locale::get_locale().unwrap_or_default(),
        engine_name: engine_name().to_string(),
        engine_version: webview_version().unwrap_or_default(),
    }
}

fn user_agent() -> String {
    let props = system_properties();
    format!(
        "{}/{} {}/{} {}",
        props.os_name, props.os_version, props.engine_name, props.engine_version, props.locale
    )
}

fn engine_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "WebKitGTK"
    }
    #[cfg(target_os = "macos")]
    {
        "WebKit"
    }
    #[cfg(target_os = "windows")]
    {
        "WebView2"
    }
}

fn new_session_id() -> String {
    let epoch_in_seconds = chrono::Utc::now().timestamp().max(0) as u64;
    let mut rng = rand::rng();
    let random: u64 = rng.random_range(0..=99_999_999);
    format!("{epoch_in_seconds}{random:08}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        new_session_id, sanitize_properties, spawn_dispatcher, system_properties, AnalyticsClient,
        AnalyticsConfig, TrackingSession, ALLOWED_EVENTS,
    };
    use parking_lot::Mutex;
    use std::time::{Duration, Instant};

    #[test]
    fn sanitize_properties_keeps_supported_values() {
        let properties = sanitize_properties(Some(json!({
            "client_id": "claude_code",
            "requests": 3,
            "enabled": true,
            "ignored": null,
            "nested": { "value": 1 },
            "list": [1, 2, 3]
        })))
        .expect("properties should be preserved");

        assert_eq!(
            properties,
            json!({
                "client_id": "claude_code",
                "requests": 3,
                "enabled": "true"
            })
        );
    }

    #[test]
    fn sanitize_properties_discards_empty_payloads() {
        assert!(sanitize_properties(Some(json!({ "empty": "   " }))).is_none());
        assert!(sanitize_properties(Some(json!(["not", "an", "object"]))).is_none());
    }

    #[test]
    fn analytics_config_parses_supported_regions() {
        std::env::set_var("HEADROOM_APTABASE_APP_KEY", "A-EU-123");
        let config = AnalyticsConfig::from_env().expect("valid config");
        assert_eq!(
            config.ingest_api_url.as_str(),
            "https://eu.aptabase.com/api/v0/events"
        );
        std::env::remove_var("HEADROOM_APTABASE_APP_KEY");
    }

    #[test]
    fn allowlist_keeps_version_and_billing_drops_noise() {
        assert!(ALLOWED_EVENTS.contains(&"app_started"));
        assert!(ALLOWED_EVENTS.contains(&"account_activated"));
        assert!(!ALLOWED_EVENTS.contains(&"runtime_paused"));
        assert!(!ALLOWED_EVENTS.contains(&"bootstrap_skipped"));
    }

    // A panicking accessor aborts the process when a frontend command lands
    // before setup's manage() (Sentry RUST-HF/HG). Mocking a tauri App just to
    // prove that costs more than reading the source, so guard the source.
    #[test]
    fn accessors_never_unwrap_managed_state() {
        let source = include_str!("analytics.rs");
        // Needles are assembled so this test does not match itself.
        let panicking = format!("app.{}::<AnalyticsClient>", "state");
        let guarded = format!("app.try_{}::<AnalyticsClient>", "state");
        assert!(
            !source.contains(&panicking),
            "use try_state via client(): state() panics before manage()"
        );
        assert_eq!(source.matches(&guarded).count(), 1);
    }

    // Quit runs shutdown() on the main thread. A black-holed ingest endpoint
    // (default-deny egress firewall) must not freeze it for the 10s request
    // timeout, nor for a periodic flush that was already in flight.
    #[test]
    fn shutdown_returns_promptly_when_ingest_endpoint_never_answers() {
        // Bound but never accepted: the kernel completes the handshake into
        // the backlog and the request then waits for a reply that never comes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let config = AnalyticsConfig {
            app_key: "A-DEV-123".into(),
            ingest_api_url: format!("http://{}/api/v0/events", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
            flush_interval: Duration::from_millis(50),
        };
        let client = AnalyticsClient {
            enabled: true,
            session: Mutex::new(TrackingSession::new()),
            dispatcher: Mutex::new(Some(spawn_dispatcher(&config))),
            system_props: system_properties(),
            app_version: "0.0.0".into(),
            headroom_ai_version: Mutex::new(None),
        };
        client
            .track_event("app_started", None)
            .expect("queue event");
        // Let the periodic flush start and block on the silent endpoint.
        std::thread::sleep(Duration::from_millis(500));

        let started = Instant::now();
        client.shutdown();
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(4),
            "shutdown blocked for {waited:?}"
        );
    }

    #[test]
    fn session_ids_follow_aptabase_format() {
        let session_id = new_session_id();
        assert_eq!(session_id.len(), 18);
        assert!(session_id.chars().all(|ch| ch.is_ascii_digit()));
    }
}
