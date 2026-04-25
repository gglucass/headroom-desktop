//! Tracking for "port 6768 is held by a non-headroom process". This is an
//! environmental issue (something on the user's machine, not our code), so
//! we don't fire Sentry on every detection. Instead we persist a marker to
//! disk and only escalate to Sentry once the same conflict has survived
//! multiple app launches — i.e. the user came back, didn't free the port,
//! and is stuck. Recovery (proxy comes up after a marker exists) is reported
//! to analytics with a time-to-recovery, then the marker is cleared.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::AppHandle;

use crate::analytics;
use crate::storage::{app_data_dir, config_file, ensure_data_dirs};

const MARKER_FILE: &str = "port-conflict.json";

/// Number of consecutive failed launches with the same occupant before we
/// surface a Warning-level Sentry event. 1 = noisy (every fresh detection),
/// 2 = "user closed and reopened the app and it's still broken".
const PERSISTENT_LAUNCH_THRESHOLD: u32 = 2;

const FAILURE_EVENT: &str = "proxy_start_blocked_port_conflict";
const RECOVERY_EVENT: &str = "proxy_start_recovered_port_conflict";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortConflictMarker {
    pub detected_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub occupant_cmd: Option<String>,
    pub occupant_pid: Option<u32>,
    pub consecutive_failed_launches: u32,
}

fn marker_path() -> PathBuf {
    config_file(&app_data_dir(), MARKER_FILE)
}

fn read_at(path: &Path) -> Option<PortConflictMarker> {
    let raw = fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn write_at(path: &Path, marker: &PortConflictMarker) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(marker)?;
    fs::write(path, json)
}

fn clear_at(path: &Path) -> Option<PortConflictMarker> {
    let prior = read_at(path);
    let _ = fs::remove_file(path);
    prior
}

fn clear_marker() -> Option<PortConflictMarker> {
    clear_at(&marker_path())
}

/// True if the anyhow error chain string is the
/// "port 6768 occupied by a non-headroom process" bail from
/// `tool_manager::start_headroom_background`.
pub fn is_port_conflict(err_chain: &str) -> bool {
    err_chain.contains("is occupied by a non-headroom process")
}

/// Extracts `(cmd, pid)` from the parenthetical detail in the bail string.
/// Bail format: `port 6768 is occupied by a non-headroom process (python3.1 pid 1073); ...`
/// `lsof_listener` formats the detail as `"{cmd} pid {pid}"`. Returns
/// `(None, None)` for the fallback `"unknown process"`.
pub fn parse_occupant(err_chain: &str) -> (Option<String>, Option<u32>) {
    const PREFIX: &str = "is occupied by a non-headroom process (";
    let Some(start) = err_chain.find(PREFIX) else {
        return (None, None);
    };
    let after = &err_chain[start + PREFIX.len()..];
    let Some(end) = after.find(')') else {
        return (None, None);
    };
    let detail = after[..end].trim();

    if detail == "unknown process" || detail.is_empty() {
        return (None, None);
    }

    if let Some(pid_idx) = detail.rfind(" pid ") {
        let cmd = detail[..pid_idx].trim();
        let pid = detail[pid_idx + " pid ".len()..].trim().parse::<u32>().ok();
        let cmd_opt = if cmd.is_empty() {
            None
        } else {
            Some(cmd.to_string())
        };
        return (cmd_opt, pid);
    }

    (Some(detail.to_string()), None)
}

fn record_failure_at(
    path: &Path,
    err_chain: &str,
    increment_launches: bool,
    now: DateTime<Utc>,
) -> std::io::Result<PortConflictMarker> {
    let (cmd, pid) = parse_occupant(err_chain);
    let prior = read_at(path);

    let marker = match prior {
        Some(prior) => {
            // If the occupant changed, the previous conflict is effectively
            // over — the user already cleared it, this is a new one. Reset
            // the counter so Sentry only fires on truly stuck cases.
            let same_occupant = prior.occupant_cmd == cmd;
            let consecutive_failed_launches = if !same_occupant {
                if increment_launches {
                    1
                } else {
                    0
                }
            } else if increment_launches {
                prior.consecutive_failed_launches.saturating_add(1)
            } else {
                prior.consecutive_failed_launches
            };
            PortConflictMarker {
                detected_at: if same_occupant { prior.detected_at } else { now },
                last_seen_at: now,
                occupant_cmd: cmd,
                occupant_pid: pid,
                consecutive_failed_launches,
            }
        }
        None => PortConflictMarker {
            detected_at: now,
            last_seen_at: now,
            occupant_cmd: cmd,
            occupant_pid: pid,
            consecutive_failed_launches: if increment_launches { 1 } else { 0 },
        },
    };

    write_at(path, &marker)?;
    Ok(marker)
}

/// Records a port-conflict failure to the marker file. `increment_launches`
/// must be true exactly once per app launch (call from `warm_runtime_on_launch`)
/// and false for in-session retries (watchdog, retry button) — those still
/// refresh `last_seen_at` but don't bump the launch counter.
pub fn record_failure(err_chain: &str, increment_launches: bool) -> Option<PortConflictMarker> {
    if !is_port_conflict(err_chain) {
        return None;
    }
    let _ = ensure_data_dirs(&app_data_dir());
    record_failure_at(&marker_path(), err_chain, increment_launches, Utc::now()).ok()
}

fn build_failure_props(marker: &PortConflictMarker) -> Map<String, Value> {
    let mut props = Map::new();
    if let Some(cmd) = &marker.occupant_cmd {
        props.insert("occupant_cmd".to_string(), Value::from(cmd.clone()));
    }
    if let Some(pid) = marker.occupant_pid {
        props.insert("occupant_pid".to_string(), Value::from(pid));
    }
    props.insert(
        "consecutive_failed_launches".to_string(),
        Value::from(marker.consecutive_failed_launches),
    );
    props
}

fn capture_persistent_to_sentry(marker: &PortConflictMarker) {
    let occupant = marker
        .occupant_cmd
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let fingerprint: [&str; 2] = ["port_conflict_persistent", occupant.as_str()];
    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "port_conflict_persistent");
            scope.set_tag("occupant_cmd", occupant.as_str());
            scope.set_extra(
                "consecutive_failed_launches",
                marker.consecutive_failed_launches.into(),
            );
            scope.set_extra("detected_at", marker.detected_at.to_rfc3339().into());
            scope.set_extra("last_seen_at", marker.last_seen_at.to_rfc3339().into());
            if let Some(pid) = marker.occupant_pid {
                scope.set_extra("occupant_pid", pid.into());
            }
            scope.set_fingerprint(Some(fingerprint.as_slice()));
        },
        || {
            sentry::capture_message(
                &format!(
                    "port_conflict_persistent: {} held port 6768 across {} launches",
                    occupant, marker.consecutive_failed_launches
                ),
                sentry::Level::Warning,
            );
        },
    );
}

/// Top-level hook: call after `ensure_headroom_running` returns Err.
/// Returns `true` if the failure was a port conflict (caller should skip
/// `capture_headroom_start_failure` for it). For unrelated errors, returns
/// `false` and the caller should keep its existing reporting.
pub fn note_proxy_failed(
    app: &AppHandle,
    err: &anyhow::Error,
    increment_launches: bool,
) -> bool {
    let err_chain = format!("{err:#}");
    if !is_port_conflict(&err_chain) {
        return false;
    }
    let Some(marker) = record_failure(&err_chain, increment_launches) else {
        return true;
    };

    analytics::track_event(
        app,
        FAILURE_EVENT,
        Some(Value::Object(build_failure_props(&marker))),
    );

    if increment_launches && marker.consecutive_failed_launches >= PERSISTENT_LAUNCH_THRESHOLD {
        capture_persistent_to_sentry(&marker);
    }

    true
}

/// Top-level hook: call after `ensure_headroom_running` returns Ok. Clears
/// the marker if one is present and emits a recovery analytics event with
/// `time_to_recovery_seconds`.
pub fn note_proxy_started(app: &AppHandle) {
    let Some(prior) = clear_marker() else {
        return;
    };

    let now = Utc::now();
    let time_to_recovery_seconds = now
        .signed_duration_since(prior.detected_at)
        .num_seconds()
        .max(0);

    let mut props = build_failure_props(&prior);
    props.insert(
        "time_to_recovery_seconds".to_string(),
        Value::from(time_to_recovery_seconds),
    );

    analytics::track_event(app, RECOVERY_EVENT, Some(Value::Object(props)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    const SAMPLE_BAIL: &str = "port 6768 is occupied by a non-headroom process \
        (python3.1 pid 1073); cannot start proxy. \
        Run `lsof -iTCP:6768 -sTCP:LISTEN` to identify it.";

    #[test]
    fn is_port_conflict_matches_bail_string() {
        assert!(is_port_conflict(SAMPLE_BAIL));
        assert!(!is_port_conflict(
            "headroom proxy already running on port 6768 (likely a stale process)"
        ));
        assert!(!is_port_conflict(
            "exited with status 1 before opening port 6768"
        ));
    }

    #[test]
    fn parse_occupant_extracts_cmd_and_pid() {
        let (cmd, pid) = parse_occupant(SAMPLE_BAIL);
        assert_eq!(cmd.as_deref(), Some("python3.1"));
        assert_eq!(pid, Some(1073));
    }

    #[test]
    fn parse_occupant_handles_multi_word_cmd() {
        let raw = "port 6768 is occupied by a non-headroom process (Google Chrome Helper pid 4242); ...";
        let (cmd, pid) = parse_occupant(raw);
        assert_eq!(cmd.as_deref(), Some("Google Chrome Helper"));
        assert_eq!(pid, Some(4242));
    }

    #[test]
    fn parse_occupant_returns_none_for_unknown_process() {
        let raw = "port 6768 is occupied by a non-headroom process (unknown process); ...";
        let (cmd, pid) = parse_occupant(raw);
        assert!(cmd.is_none());
        assert!(pid.is_none());
    }

    #[test]
    fn parse_occupant_returns_none_when_pattern_absent() {
        assert_eq!(parse_occupant("some other error"), (None, None));
    }

    #[test]
    fn record_failure_creates_marker_with_counter_one_on_launch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        let m = record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        assert_eq!(m.consecutive_failed_launches, 1);
        assert_eq!(m.detected_at, ts(1000));
        assert_eq!(m.last_seen_at, ts(1000));
        assert_eq!(m.occupant_cmd.as_deref(), Some("python3.1"));
        assert_eq!(m.occupant_pid, Some(1073));
    }

    #[test]
    fn record_failure_does_not_bump_counter_for_in_session_retry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        let m = record_failure_at(&path, SAMPLE_BAIL, false, ts(1500)).unwrap();
        assert_eq!(m.consecutive_failed_launches, 1);
        assert_eq!(m.detected_at, ts(1000), "detected_at preserved");
        assert_eq!(m.last_seen_at, ts(1500), "last_seen_at refreshed");
    }

    #[test]
    fn record_failure_increments_on_subsequent_launch_with_same_occupant() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        let m = record_failure_at(&path, SAMPLE_BAIL, true, ts(2000)).unwrap();
        assert_eq!(m.consecutive_failed_launches, 2);
        assert_eq!(m.detected_at, ts(1000));
    }

    #[test]
    fn record_failure_resets_counter_when_occupant_changes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        record_failure_at(&path, SAMPLE_BAIL, true, ts(2000)).unwrap();
        let other = "port 6768 is occupied by a non-headroom process (node pid 99); ...";
        let m = record_failure_at(&path, other, true, ts(3000)).unwrap();
        assert_eq!(m.consecutive_failed_launches, 1);
        assert_eq!(m.detected_at, ts(3000), "new conflict resets detected_at");
        assert_eq!(m.occupant_cmd.as_deref(), Some("node"));
    }

    #[test]
    fn clear_returns_prior_marker_and_removes_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        let prior = clear_at(&path);
        assert!(prior.is_some());
        assert!(!path.exists());
        assert!(clear_at(&path).is_none(), "second clear is a no-op");
    }

    #[test]
    fn marker_round_trips_through_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        let original = record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        let reloaded = read_at(&path).expect("marker should be readable");
        assert_eq!(reloaded, original);
    }
}
