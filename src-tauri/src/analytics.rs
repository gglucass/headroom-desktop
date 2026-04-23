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
const SESSION_TIMEOUT_SECS: i64 = 4 * 60 * 60;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 10;
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
        }
    }

    pub fn track_event(&self, name: &str, properties: Option<Value>) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        let normalized_name = normalize_event_name(name);
        if normalized_name.is_empty() {
            return Ok(());
        }

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
            "props": sanitize_properties(properties)
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

pub fn track_event(app: &AppHandle, name: &str, properties: Option<Value>) {
    let client = app.state::<AnalyticsClient>();
    if let Err(err) = client.track_event(name, properties) {
        log_stderr(format_args!(
            "failed to track analytics event {}: {err}",
            name.trim()
        ));
    }
}

fn log_stderr(args: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "{args}");
}

pub fn shutdown(app: &AppHandle) {
    let client = app.state::<AnalyticsClient>();
    client.shutdown();
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
                flush_queue(&http_client, &config, &mut queue);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_queue(&http_client, &config, &mut queue);
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_queue(&http_client, &config, &mut queue);
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

    Client::builder()
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .default_headers(headers)
        .user_agent(user_agent())
        .build()
        .expect("could not build analytics http client")
}

fn flush_queue(client: &Client, config: &AnalyticsConfig, queue: &mut VecDeque<Value>) {
    if queue.is_empty() {
        return;
    }

    let mut failed = Vec::new();
    while !queue.is_empty() {
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

    use super::{new_session_id, sanitize_properties, AnalyticsConfig};

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
    fn session_ids_follow_aptabase_format() {
        let session_id = new_session_id();
        assert_eq!(session_id.len(), 18);
        assert!(session_id.chars().all(|ch| ch.is_ascii_digit()));
    }
}
