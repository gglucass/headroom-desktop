//! Per-day counters for provider rate limiting (HTTP 429s), failed requests
//! and client mix, observed at the intercept proxy. Aggregate counts only — never content.
//! The counters ride the milestone/heartbeat savings payload to headroom-web
//! (`SavingsDay.client_requests` / `rate_limit_429s` / `failed_requests`).
//!
//! Day keys are LOCAL dates via `storage::user_day_key`, matching the local
//! tracker buckets that make up the recent days of the merged savings series
//! these counters are joined against (state.rs `merge_daily_savings`). Older
//! series days are UTC rollups and join approximately — UserDailySaving on
//! the server documents its day boundaries as approximate for that reason.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Retention for the on-disk map. The savings payload only reads back 30
/// days; 60 gives slack without letting the file grow unbounded.
const MAX_DAYS: usize = 60;

/// Minimum interval between disk flushes. Counters are aggregate telemetry:
/// losing the final <30s of increments on quit is acceptable, blocking the
/// request hot path on a write per request is not.
const SAVE_INTERVAL: Duration = Duration::from_secs(30);

const SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "usage-counters.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct DayCounters {
    /// Provider-bound requests per client bucket, same keys as
    /// `intercept_request_counts` (`claude-code`, `codex`, `opencode`,
    /// `grok-build`, `github-copilot`).
    pub client_requests: BTreeMap<String, u64>,
    /// Upstream HTTP 429 responses per client bucket.
    pub rate_limit_429s: BTreeMap<String, u64>,
    /// Requests the user saw fail, per client bucket: any non-429 4xx or 5xx,
    /// and for Codex a dropped response or a 200 stream failed in-band. No
    /// class is excused as "the provider's" or "the network's": a single
    /// request cannot tell, and both 2026-10 showstoppers (a 502 loop behind
    /// an idle-killing middlebox, Codex's in-band web_search compaction
    /// failure) were excused that way and never reached Sentry. headroom-web
    /// tells them apart across the fleet: an outage hits every version at
    /// once, a Headroom bug clusters on one.
    pub failed_requests: BTreeMap<String, u64>,
    /// `failed_requests` split by what failed, keyed `<client>:<kind>`: the
    /// HTTP status, `dropped` (no response) or `stream` (a Codex 2xx stream
    /// failed in-band). Sentry never sees most failures (a 5xx, an expired
    /// key's 401, a dropped response), so a stuck host's cause was unknowable
    /// from the server (users 743 and 4433 on 2026-10-16).
    pub failure_kinds: BTreeMap<String, u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PersistedCounters {
    schema_version: u32,
    days: BTreeMap<String, DayCounters>,
}

struct Store {
    path: PathBuf,
    days: BTreeMap<String, DayCounters>,
    dirty: bool,
    last_saved: Instant,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

impl Store {
    fn load_or_create(base_dir: &Path) -> Self {
        let path = crate::storage::config_file(base_dir, FILE_NAME);
        let days = match crate::state::read_state_file(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedCounters>(&bytes) {
                Ok(persisted) if persisted.schema_version == SCHEMA_VERSION => persisted.days,
                Ok(persisted) => {
                    // A version this build does not know is kept, not
                    // overwritten on the next save: a downgrade used to wipe
                    // the counters a newer build had written.
                    log::warn!(
                        "{FILE_NAME} has schema {} (expected {SCHEMA_VERSION}); backing up and starting fresh",
                        persisted.schema_version
                    );
                    let _ = crate::client_adapters::move_aside(
                        &path,
                        &path.with_extension("json.schema-mismatch"),
                    );
                    BTreeMap::new()
                }
                Err(err) => {
                    // Never silently overwrite a file we failed to parse:
                    // back it up so a truncation bug stays diagnosable.
                    log::warn!("{FILE_NAME} is corrupt ({err}); backing up and starting fresh");
                    // Retried with a copy fallback (see move_aside), so a
                    // scanner hold cannot let the fresh save overwrite it.
                    let _ =
                        crate::client_adapters::move_aside(&path, &path.with_extension("json.bak"));
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        Self {
            path,
            days,
            dirty: false,
            last_saved: Instant::now(),
        }
    }

    /// Bookkeeping only: returns the snapshot to write when a flush is due.
    /// The caller writes it after releasing STORE (see `with_store`).
    fn maybe_save(&mut self) -> Option<(PathBuf, Vec<u8>)> {
        if !self.dirty || self.last_saved.elapsed() < SAVE_INTERVAL {
            return None;
        }
        let persisted = PersistedCounters {
            schema_version: SCHEMA_VERSION,
            days: std::mem::take(&mut self.days),
        };
        let bytes = serde_json::to_vec(&persisted).unwrap_or_default();
        self.days = persisted.days;
        self.dirty = false;
        self.last_saved = Instant::now();
        Some((self.path.clone(), bytes))
    }
}

fn persist(path: &Path, bytes: &[u8]) {
    if let Err(err) = crate::client_adapters::atomic_write(path, bytes) {
        log::warn!("failed to persist {FILE_NAME}: {err}");
    }
}

fn today_key() -> String {
    crate::storage::user_day_key(chrono::Local::now())
}

fn with_store(f: impl FnOnce(&mut Store)) {
    let pending = {
        let mut guard = match STORE.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let store =
            guard.get_or_insert_with(|| Store::load_or_create(&crate::storage::app_data_dir()));
        f(store);
        store.maybe_save()
    };
    // atomic_write fsyncs (and retries on Windows): keep it off STORE and off
    // the intercept's single runtime thread that records every request. The
    // 30s SAVE_INTERVAL keeps two snapshots from racing to the rename.
    let Some((path, bytes)) = pending else {
        return;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || persist(&path, &bytes));
        }
        Err(_) => persist(&path, &bytes),
    }
}

fn bump(
    days: &mut BTreeMap<String, DayCounters>,
    day: String,
    client: &str,
    field: fn(&mut DayCounters) -> &mut BTreeMap<String, u64>,
) {
    *field(days.entry(day).or_default())
        .entry(client.to_string())
        .or_default() += 1;
    // BTreeMap orders ISO date keys chronologically, so pruning the first
    // entry always drops the oldest day.
    while days.len() > MAX_DAYS {
        let oldest = days.keys().next().cloned();
        match oldest {
            Some(key) => days.remove(&key),
            None => break,
        };
    }
}

/// Count one provider-bound request for a client bucket (today, local day).
pub fn record_request(client: &str) {
    with_store(|store| {
        bump(&mut store.days, today_key(), client, |c| {
            &mut c.client_requests
        });
        store.dirty = true;
    });
}

/// Count one upstream HTTP 429 for a client bucket (today, local day).
pub fn record_429(client: &str) {
    with_store(|store| {
        bump(&mut store.days, today_key(), client, |c| {
            &mut c.rate_limit_429s
        });
        store.dirty = true;
    });
}

/// Count one request the user saw fail for a client bucket (today, local day),
/// and its `kind` (see `DayCounters::failure_kinds`).
pub fn record_failure(client: &str, kind: &str) {
    with_store(|store| {
        let day = today_key();
        bump(&mut store.days, day.clone(), client, |c| {
            &mut c.failed_requests
        });
        bump(&mut store.days, day, &format!("{client}:{kind}"), |c| {
            &mut c.failure_kinds
        });
        store.dirty = true;
    });
}

/// Provider-bound requests seen from `client` today and yesterday (local
/// days). Two buckets so a request just before midnight still reads as
/// recent at 00:05.
pub fn requests_since_yesterday(client: &str) -> u64 {
    let keys = since_yesterday_keys(chrono::Local::now());
    let mut total = 0;
    with_store(|store| {
        for key in &keys {
            if let Some(day) = store.days.get(key) {
                total += day.client_requests.get(client).copied().unwrap_or(0);
            }
        }
    });
    total
}

/// Today's and yesterday's local day keys. Calendar-day arithmetic: `now -
/// 24h` skips yesterday in the first hour after a spring-forward day.
fn since_yesterday_keys(now: chrono::DateTime<chrono::Local>) -> [String; 2] {
    let today = crate::storage::user_day(now);
    let yesterday = today.pred_opt().unwrap_or(today);
    [today, yesterday].map(|day| day.format("%Y-%m-%d").to_string())
}

/// Snapshot of all retained days, for joining into the savings payload.
pub fn recent_days() -> BTreeMap<String, DayCounters> {
    let mut out = BTreeMap::new();
    with_store(|store| out = store.days.clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_counts_requests_429s_and_failures_separately() {
        let mut days = BTreeMap::new();
        let day = || "2026-08-17".to_string();
        bump(&mut days, day(), "claude-code", |c| &mut c.client_requests);
        bump(&mut days, day(), "claude-code", |c| &mut c.client_requests);
        bump(&mut days, day(), "claude-code", |c| &mut c.rate_limit_429s);
        bump(&mut days, day(), "codex", |c| &mut c.client_requests);
        bump(&mut days, day(), "codex", |c| &mut c.failed_requests);
        bump(&mut days, day(), "codex:502", |c| &mut c.failure_kinds);

        let day = &days["2026-08-17"];
        assert_eq!(day.client_requests["claude-code"], 2);
        assert_eq!(day.client_requests["codex"], 1);
        assert_eq!(day.rate_limit_429s["claude-code"], 1);
        assert!(!day.rate_limit_429s.contains_key("codex"));
        assert_eq!(day.failed_requests["codex"], 1);
        assert!(!day.failed_requests.contains_key("claude-code"));
        assert_eq!(day.failure_kinds["codex:502"], 1);
    }

    #[test]
    fn bump_prunes_oldest_days_past_retention() {
        let mut days = BTreeMap::new();
        for i in 0..(MAX_DAYS + 5) {
            bump(
                &mut days,
                format!("2026-01-{:02}", i + 1),
                "claude-code",
                |c| &mut c.client_requests,
            );
        }
        assert_eq!(days.len(), MAX_DAYS);
        assert!(!days.contains_key("2026-01-01"));
        assert!(days.contains_key(&format!("2026-01-{:02}", MAX_DAYS + 5)));
    }

    #[test]
    fn persisted_counters_tolerate_missing_fields() {
        // A future field added to DayCounters must not wipe history on
        // rollback; serde(default) has to absorb both directions.
        let parsed: PersistedCounters =
            serde_json::from_str(r#"{"schemaVersion":1,"days":{"2026-08-17":{}}}"#).unwrap();
        assert_eq!(parsed.days["2026-08-17"], DayCounters::default());
    }

    #[test]
    fn schema_mismatch_is_backed_up_not_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crate::storage::config_file(dir.path(), FILE_NAME);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        let newer = format!(r#"{{"schemaVersion":{},"days":{{}}}}"#, SCHEMA_VERSION + 1);
        std::fs::write(&path, &newer).expect("seed");

        let store = Store::load_or_create(dir.path());

        assert!(store.days.is_empty());
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.schema-mismatch"))
                .expect("backup kept"),
            newer
        );
    }

    #[test]
    fn maybe_save_leaves_the_write_to_the_caller() {
        // maybe_save runs under STORE on the intercept's single runtime
        // thread; the fsync'd write must happen after both are released.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crate::storage::config_file(dir.path(), FILE_NAME);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        let mut days = BTreeMap::new();
        bump(&mut days, "2026-08-17".into(), "codex", |c| {
            &mut c.client_requests
        });
        let mut store = Store {
            path: path.clone(),
            days,
            dirty: true,
            last_saved: Instant::now()
                .checked_sub(SAVE_INTERVAL + Duration::from_secs(1))
                .expect("instant"),
        };

        let (pending_path, bytes) = store.maybe_save().expect("flush due");

        assert!(!path.exists(), "maybe_save wrote the file under the lock");
        assert!(!store.dirty);
        assert!(store.maybe_save().is_none(), "throttle restarts");
        persist(&pending_path, &bytes);
        let back: PersistedCounters =
            serde_json::from_slice(&std::fs::read(&path).expect("written")).expect("parse");
        assert_eq!(back.days["2026-08-17"].client_requests["codex"], 1);
    }

    #[test]
    fn since_yesterday_keys_use_calendar_days_across_spring_forward() {
        // 00:30 on the day after a spring-forward day is only 23.5h after
        // that day's 00:00, so `now - 24h` lands two calendar days back.
        // US (03-08) and EU (03-29) 2026 transitions, so the local zone of
        // either kind of machine hits one of them.
        for (today, yesterday) in [("2026-03-09", "2026-03-08"), ("2026-03-30", "2026-03-29")] {
            let local = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d")
                .expect("date")
                .and_hms_opt(0, 30, 0)
                .expect("time");
            let now = chrono::TimeZone::from_local_datetime(&chrono::Local, &local)
                .earliest()
                .expect("local time exists");

            assert_eq!(since_yesterday_keys(now), [today, yesterday]);
        }
    }

    #[test]
    fn round_trips_through_json() {
        let mut days = BTreeMap::new();
        bump(&mut days, "2026-08-17".into(), "codex", |c| {
            &mut c.rate_limit_429s
        });
        let persisted = PersistedCounters {
            schema_version: SCHEMA_VERSION,
            days,
        };
        let bytes = serde_json::to_vec(&persisted).unwrap();
        let back: PersistedCounters = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.days["2026-08-17"].rate_limit_429s["codex"], 1);
    }
}
