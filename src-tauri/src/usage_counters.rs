//! Per-day counters for provider rate limiting (HTTP 429s) and client mix,
//! observed at the intercept proxy. Aggregate counts only — never content.
//! The counters ride the milestone/heartbeat savings payload to headroom-web
//! (`SavingsDay.client_requests` / `SavingsDay.rate_limit_429s`).
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
    /// `grok-build`).
    pub client_requests: BTreeMap<String, u64>,
    /// Upstream HTTP 429 responses per client bucket.
    pub rate_limit_429s: BTreeMap<String, u64>,
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
        let days = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedCounters>(&bytes) {
                Ok(persisted) if persisted.schema_version == SCHEMA_VERSION => persisted.days,
                Ok(_) => BTreeMap::new(),
                Err(err) => {
                    // Never silently overwrite a file we failed to parse:
                    // back it up so a truncation bug stays diagnosable.
                    log::warn!("{FILE_NAME} is corrupt ({err}); backing up and starting fresh");
                    let _ = std::fs::rename(&path, path.with_extension("json.bak"));
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

    fn maybe_save(&mut self) {
        if !self.dirty || self.last_saved.elapsed() < SAVE_INTERVAL {
            return;
        }
        let persisted = PersistedCounters {
            schema_version: SCHEMA_VERSION,
            days: std::mem::take(&mut self.days),
        };
        let bytes = serde_json::to_vec(&persisted).unwrap_or_default();
        self.days = persisted.days;
        if let Err(err) = crate::client_adapters::atomic_write(&self.path, &bytes) {
            log::warn!("failed to persist {FILE_NAME}: {err}");
        }
        self.dirty = false;
        self.last_saved = Instant::now();
    }
}

fn today_key() -> String {
    crate::storage::user_day_key(chrono::Local::now())
}

fn with_store(f: impl FnOnce(&mut Store)) {
    let mut guard = match STORE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let store = guard.get_or_insert_with(|| Store::load_or_create(&crate::storage::app_data_dir()));
    f(store);
    store.maybe_save();
}

fn bump(days: &mut BTreeMap<String, DayCounters>, day: String, client: &str, is_429: bool) {
    let counters = days.entry(day).or_default();
    let map = if is_429 {
        &mut counters.rate_limit_429s
    } else {
        &mut counters.client_requests
    };
    *map.entry(client.to_string()).or_default() += 1;
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
        bump(&mut store.days, today_key(), client, false);
        store.dirty = true;
    });
}

/// Count one upstream HTTP 429 for a client bucket (today, local day).
pub fn record_429(client: &str) {
    with_store(|store| {
        bump(&mut store.days, today_key(), client, true);
        store.dirty = true;
    });
}

/// Provider-bound requests seen from `client` today and yesterday (local
/// days). Two buckets so a request just before midnight still reads as
/// recent at 00:05.
pub fn requests_since_yesterday(client: &str) -> u64 {
    let now = chrono::Local::now();
    let keys = [
        crate::storage::user_day_key(now),
        crate::storage::user_day_key(now - chrono::Duration::days(1)),
    ];
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
    fn bump_counts_requests_and_429s_separately() {
        let mut days = BTreeMap::new();
        bump(&mut days, "2026-08-17".into(), "claude-code", false);
        bump(&mut days, "2026-08-17".into(), "claude-code", false);
        bump(&mut days, "2026-08-17".into(), "claude-code", true);
        bump(&mut days, "2026-08-17".into(), "codex", false);

        let day = &days["2026-08-17"];
        assert_eq!(day.client_requests["claude-code"], 2);
        assert_eq!(day.client_requests["codex"], 1);
        assert_eq!(day.rate_limit_429s["claude-code"], 1);
        assert!(!day.rate_limit_429s.contains_key("codex"));
    }

    #[test]
    fn bump_prunes_oldest_days_past_retention() {
        let mut days = BTreeMap::new();
        for i in 0..(MAX_DAYS + 5) {
            bump(
                &mut days,
                format!("2026-01-{:02}", i + 1),
                "claude-code",
                false,
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
    fn round_trips_through_json() {
        let mut days = BTreeMap::new();
        bump(&mut days, "2026-08-17".into(), "codex", true);
        let persisted = PersistedCounters {
            schema_version: SCHEMA_VERSION,
            days,
        };
        let bytes = serde_json::to_vec(&persisted).unwrap();
        let back: PersistedCounters = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.days["2026-08-17"].rate_limit_429s["codex"], 1);
    }
}
