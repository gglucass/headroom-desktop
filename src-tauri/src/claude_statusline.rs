//! Per-conversation input savings for the Claude Code statusline.
//!
//! The intercept pairs each Claude Code request's `x-claude-code-session-id`
//! with the backend's `x-headroom-tokens-saved` response header (on streaming
//! responses it comes from the stream-metering vendor in the sitecustomize)
//! and records it here. The statusline script `client_adapters` installs looks
//! its conversation up in this file by the `session_id` Claude Code passes it
//! on stdin. Token counts only, never content.
//!
//! Summing a conversation's `tokens_saved` counts each removed token once:
//! Anthropic's cached prefix is frozen, so a turn's figure covers only content
//! new to the conversation (see the wheel's conversation_savings.py).
//!
//! The script parses this file with a bash regex, not a JSON parser (a Python
//! start cost ~30 ms and it runs every second), so `Session`'s field order is
//! part of the file format: serde writes fields in declaration order.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Conversations kept; the least recently active is dropped first. Every
/// Claude Code session is booked (VS Code panel chats, headless `claude -p`
/// runs such as learn scans), and 64 filled in about a day, dropping idle
/// conversations the user was still coming back to. At ~100 bytes an entry,
/// 512 is ~50 KB, cheap for the statusline script to read every second.
const MAX_SESSIONS: usize = 512;
pub(crate) const SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "claude-statusline.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Session {
    pub(crate) tokens_saved: u64,
    /// The most recent NONZERO saving and when it landed; the statusline
    /// highlights it for a few seconds. Zero-saving requests (the auto-mode
    /// classifier, title generation) share the conversation's session id and
    /// arrive right after the main request, so letting them overwrite this
    /// would blank every saving the moment it appeared.
    pub(crate) last_saved: u64,
    pub(crate) last_saved_at_ms: i64,
    /// When the intercept last handed one of this conversation's requests to
    /// the backend to compress; the statusline says "compressing" briefly.
    /// Last in the struct so older files, which lack it, still match the
    /// script's regex (it treats the field as optional).
    pub(crate) last_request_at_ms: i64,
}

impl Session {
    fn last_active_ms(&self) -> i64 {
        self.last_saved_at_ms.max(self.last_request_at_ms)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Persisted {
    pub(crate) schema_version: u32,
    pub(crate) sessions: BTreeMap<String, Session>,
}

static SESSIONS: Mutex<Option<BTreeMap<String, Session>>> = Mutex::new(None);
/// Held across serialize + write so a snapshot taken later is always written
/// later: two in-flight writes can never leave the older one on disk.
static WRITE: Mutex<()> = Mutex::new(());

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn state_path() -> PathBuf {
    crate::storage::config_file(&crate::storage::app_data_dir(), FILE_NAME)
}

fn load(path: &Path) -> BTreeMap<String, Session> {
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice::<Persisted>(&bytes) {
        Ok(persisted) if persisted.schema_version == SCHEMA_VERSION => persisted.sessions,
        Ok(persisted) => {
            log::warn!(
                "{FILE_NAME} has schema {} (expected {SCHEMA_VERSION}); backing up and starting fresh",
                persisted.schema_version
            );
            // direct-write: moves Headroom's own unparsable state aside, never a user file
            let _ = std::fs::rename(path, path.with_extension("json.bak"));
            BTreeMap::new()
        }
        Err(err) => {
            log::warn!("{FILE_NAME} is corrupt ({err}); backing up and starting fresh");
            // direct-write: moves Headroom's own unparsable state aside, never a user file
            let _ = std::fs::rename(path, path.with_extension("json.bak"));
            BTreeMap::new()
        }
    }
}

/// A request that saved nothing changes nothing here, above all never clears
/// the last saving (its start was already booked by `apply_request`).
/// Returns whether anything changed.
fn apply(
    sessions: &mut BTreeMap<String, Session>,
    session_id: &str,
    saved: u64,
    now_ms: i64,
) -> bool {
    if saved == 0 {
        return false;
    }
    let session = sessions.entry(session_id.to_string()).or_default();
    session.tokens_saved = session.tokens_saved.saturating_add(saved);
    session.last_saved = saved;
    session.last_saved_at_ms = now_ms;
    evict(sessions);
    true
}

fn apply_request(sessions: &mut BTreeMap<String, Session>, session_id: &str, now_ms: i64) {
    sessions
        .entry(session_id.to_string())
        .or_default()
        .last_request_at_ms = now_ms;
    evict(sessions);
}

fn evict(sessions: &mut BTreeMap<String, Session>) {
    while sessions.len() > MAX_SESSIONS {
        let oldest = sessions
            .iter()
            .min_by_key(|(_, s)| s.last_active_ms())
            .map(|(id, _)| id.clone());
        match oldest {
            Some(id) => sessions.remove(&id),
            None => break,
        };
    }
}

fn persist() {
    let _write = lock(&WRITE);
    let bytes = {
        let guard = lock(&SESSIONS);
        let Some(sessions) = guard.as_ref() else {
            return;
        };
        serde_json::to_vec(&Persisted {
            schema_version: SCHEMA_VERSION,
            sessions: sessions.clone(),
        })
        .unwrap_or_default()
    };
    if let Err(err) = crate::client_adapters::atomic_write(&state_path(), &bytes) {
        log::warn!("failed to persist {FILE_NAME}: {err}");
    }
}

/// Book one Claude Code request's input saving against its conversation.
pub fn record(session_id: &str, tokens_saved: i64) {
    let saved = tokens_saved.max(0) as u64;
    update(|sessions, now_ms| apply(sessions, session_id, saved, now_ms));
}

/// A request of this conversation just went to the backend for compression.
pub fn record_request(session_id: &str) {
    update(|sessions, now_ms| {
        apply_request(sessions, session_id, now_ms);
        true
    });
}

/// Mutate the in-memory map, then persist when `f` reports a change.
fn update(f: impl FnOnce(&mut BTreeMap<String, Session>, i64) -> bool) {
    {
        let mut guard = lock(&SESSIONS);
        // Unit tests stay in memory: without HEADROOM_DATA_DIR, state_path()
        // is the real profile's config dir.
        let sessions = guard.get_or_insert_with(|| {
            if cfg!(test) {
                BTreeMap::new()
            } else {
                load(&state_path())
            }
        });
        let changed = f(sessions, chrono::Utc::now().timestamp_millis());
        if cfg!(test) || !changed {
            return;
        }
    }
    // atomic_write fsyncs; keep that off the relay task that called us.
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(persist);
        }
        Err(_) => persist(),
    }
}

/// (tokens saved this conversation, saved on its last request).
#[cfg(test)]
pub(crate) fn recorded(session_id: &str) -> Option<(u64, u64)> {
    lock(&SESSIONS)
        .as_ref()?
        .get(session_id)
        .map(|s| (s.tokens_saved, s.last_saved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savings_accumulate_per_conversation_and_stay_silent_until_one_lands() {
        let mut sessions = BTreeMap::new();
        apply(&mut sessions, "a", 0, 1);
        assert!(sessions.is_empty(), "a zero saving must not open an entry");

        apply(&mut sessions, "a", 15_000, 2);
        apply(&mut sessions, "b", 700, 3);
        // The classifier request that follows saves nothing: it must not
        // blank the saving that just landed.
        assert!(!apply(&mut sessions, "a", 0, 4));
        let a = &sessions["a"];
        assert_eq!(
            (a.tokens_saved, a.last_saved, a.last_saved_at_ms),
            (15_000, 15_000, 2)
        );
        assert_eq!(sessions["b"].tokens_saved, 700);
    }

    #[test]
    fn a_request_opens_the_entry_and_keeps_the_last_saving() {
        let mut sessions = BTreeMap::new();
        apply_request(&mut sessions, "a", 5);
        assert_eq!(sessions["a"].tokens_saved, 0);
        assert_eq!(sessions["a"].last_request_at_ms, 5);
        apply(&mut sessions, "a", 900, 6);
        apply_request(&mut sessions, "a", 7);
        let a = &sessions["a"];
        assert_eq!(
            (a.tokens_saved, a.last_saved, a.last_request_at_ms),
            (900, 900, 7)
        );
    }

    #[test]
    fn least_recently_updated_conversation_is_evicted_at_the_cap() {
        let mut sessions = BTreeMap::new();
        for i in 0..MAX_SESSIONS as i64 {
            apply(&mut sessions, &format!("s{i}"), 1, i);
        }
        // A new request counts as activity, not only a saving.
        apply_request(&mut sessions, "s0", 1_000);
        apply(&mut sessions, "new", 1, 1_001);
        assert_eq!(sessions.len(), MAX_SESSIONS);
        assert!(sessions.contains_key("s0"), "refreshed session was evicted");
        assert!(!sessions.contains_key("s1"), "oldest session survived");
    }
}
