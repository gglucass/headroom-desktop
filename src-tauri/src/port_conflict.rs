//! Tracking for "port 6768 is held by a non-headroom process". This is an
//! environmental issue (something on the user's machine, not our code), so
//! we don't fire Sentry on every detection. Instead we persist a marker to
//! disk and only escalate to Sentry once the same conflict has survived
//! multiple app launches — i.e. the user came back, didn't free the port,
//! and is stuck. Recovery (proxy comes up after a marker exists) clears the
//! marker.

use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::storage::{app_data_dir, config_file};

const MARKER_FILE: &str = "port-conflict.json";

/// Number of consecutive failed launches with the same occupant before we
/// surface a Warning-level Sentry event. 1 = noisy (every fresh detection),
/// 2 = "user closed and reopened the app and it's still broken".
const PERSISTENT_LAUNCH_THRESHOLD: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
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

fn read_at(path: &Path) -> anyhow::Result<Option<PortConflictMarker>> {
    let raw = match crate::state::read_state_file(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_slice(&raw) {
        Ok(marker) => Ok(Some(marker)),
        Err(err) => {
            let backup = path.with_extension("json.corrupt");
            log::warn!("{} is unparsable ({err}); backing it up", path.display());
            crate::client_adapters::move_aside(path, &backup).with_context(|| {
                format!("backing up {} to {}", path.display(), backup.display())
            })?;
            Ok(None)
        }
    }
}

fn write_at(path: &Path, marker: &PortConflictMarker) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(marker)?;
    crate::client_adapters::atomic_write(path, &json)
}

fn clear_at(path: &Path) -> anyhow::Result<()> {
    read_at(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
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
    // The paren that closes ours, not the first one: an occupant can carry its
    // own, and `DENIED_OCCUPANT` does, which cut RUST-PP's label to
    // "Windows itself (WinError 10013".
    let mut depth = 0usize;
    let Some(end) = after.char_indices().find_map(|(i, c)| match c {
        '(' => {
            depth += 1;
            None
        }
        ')' if depth == 0 => Some(i),
        ')' => {
            depth -= 1;
            None
        }
        _ => None,
    }) else {
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
) -> anyhow::Result<PortConflictMarker> {
    let (cmd, pid) = parse_occupant(err_chain);
    let prior = read_at(path)?;

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
                detected_at: if same_occupant && prior.detected_at != DateTime::UNIX_EPOCH {
                    prior.detected_at
                } else if same_occupant && prior.last_seen_at != DateTime::UNIX_EPOCH {
                    prior.last_seen_at
                } else {
                    now
                },
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
    match record_failure_at(&marker_path(), err_chain, increment_launches, Utc::now()) {
        Ok(marker) => Some(marker),
        Err(err) => {
            log::info!("could not record port conflict: {err:#}");
            None
        }
    }
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
pub fn note_proxy_failed(err: &anyhow::Error, increment_launches: bool) -> bool {
    let err_chain = format!("{err:#}");
    if !is_port_conflict(&err_chain) {
        return false;
    }
    let Some(marker) = record_failure(&err_chain, increment_launches) else {
        return true;
    };

    if increment_launches && marker.consecutive_failed_launches >= PERSISTENT_LAUNCH_THRESHOLD {
        capture_persistent_to_sentry(&marker);
    }

    true
}

/// Top-level hook: call after `ensure_headroom_running` returns Ok. Clears
/// the marker if one is present, so the launch counter starts over.
pub fn note_proxy_started() {
    if let Err(err) = clear_at(&marker_path()) {
        log::info!("could not clear port conflict: {err:#}");
    }
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

    /// New bail shape emitted by `tool_manager::start_headroom_background`
    /// when even the fallback range (6769-6790) is exhausted. The marker
    /// substring `"is occupied by a non-headroom process"` is preserved so
    /// `is_port_conflict` and `parse_occupant` continue to match.
    const SAMPLE_BAIL_ALL_FOREIGN: &str = "port 6768 is occupied by a non-headroom process \
        (rapportd pid 594) and fallback ports 6769-6790 are also unavailable; cannot start proxy. \
        Reboot to clear stuck listeners, then relaunch Headroom.";

    #[test]
    fn is_port_conflict_matches_bail_string() {
        assert!(is_port_conflict(SAMPLE_BAIL));
        assert!(is_port_conflict(SAMPLE_BAIL_ALL_FOREIGN));
        assert!(!is_port_conflict(
            "headroom proxy already running on port 6768 (likely a stale process)"
        ));
        assert!(!is_port_conflict(
            "exited with status 1 before opening port 6768"
        ));
    }

    #[test]
    fn parse_occupant_works_on_all_foreign_bail_shape() {
        let (cmd, pid) = parse_occupant(SAMPLE_BAIL_ALL_FOREIGN);
        assert_eq!(cmd.as_deref(), Some("rapportd"));
        assert_eq!(pid, Some(594));
    }

    #[test]
    fn parse_occupant_extracts_cmd_and_pid() {
        let (cmd, pid) = parse_occupant(SAMPLE_BAIL);
        assert_eq!(cmd.as_deref(), Some("python3.1"));
        assert_eq!(pid, Some(1073));
    }

    #[test]
    fn parse_occupant_handles_multi_word_cmd() {
        let raw =
            "port 6768 is occupied by a non-headroom process (Google Chrome Helper pid 4242); ...";
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
    fn record_failure_preserves_a_marker_missing_its_last_seen_timestamp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        let original = record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        let mut json = serde_json::to_value(original).unwrap();
        json.as_object_mut().unwrap().remove("last_seen_at");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

        let marker = record_failure_at(&path, SAMPLE_BAIL, true, ts(2000)).unwrap();
        assert_eq!(marker.consecutive_failed_launches, 2);
        assert_eq!(marker.detected_at, ts(1000));
        assert_eq!(marker.last_seen_at, ts(2000));
    }

    #[test]
    fn record_failure_backs_up_a_corrupt_marker_before_replacing_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        let corrupt = b"{\"detected_at\":";
        fs::write(&path, corrupt).unwrap();

        let marker = record_failure_at(&path, SAMPLE_BAIL, true, ts(2000)).unwrap();
        assert_eq!(marker.consecutive_failed_launches, 1);
        assert_eq!(
            fs::read(path.with_extension("json.corrupt")).unwrap(),
            corrupt
        );
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
    fn clear_removes_file_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        clear_at(&path).unwrap();
        assert!(!path.exists());
        clear_at(&path).unwrap();
    }

    #[test]
    fn marker_round_trips_through_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marker.json");
        let original = record_failure_at(&path, SAMPLE_BAIL, true, ts(1000)).unwrap();
        let reloaded = read_at(&path).unwrap().expect("marker should be readable");
        assert_eq!(reloaded, original);
    }
}
