use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::models::{
    ActivityEvent, ActivityFeedSnapshot, ClaudeCodeProject, LearningsMilestoneEvent, PlanWindow,
    RecordEvent, RecordTag, TipEvent, TipKind, TrainSuggestionEvent, TransformationFeedEvent,
    WeeklyRecapEvent,
};
use crate::storage::config_file;

// Reserve bumps for fundamental outer-struct changes (e.g., the v1→v2 move
// from event queues to latest-of-kind slots). Field-level evolution of the
// nested event structs (`TransformationFeedEvent`, `RecordEvent`, etc.) does
// NOT require a bump anymore: each `last_*` slot is deserialized via
// `de_or_none`, so a reshape just empties that one slot and the rest of the
// file (record counters, fire-once sets, learnings snapshots) survives.
//
// Historical bump record:
//   1 → 2: queues replaced by single-slot tiles.
//   2 → 3, 3 → 4: tile-slot field shape changes that today would be
//                 absorbed by `de_or_none` and not need a bump.
//   4 → 5: learnings tile moved from {count, kind} to {patternsToday,
//          remindersToday, learningsToday, projectPath, ...}; same as above.
const SCHEMA_VERSION: u8 = 5;
// Hard cap on how big a facts file we'll even attempt to deserialize at boot.
// The pre-v2 schema embedded full request/response bodies into queues that
// could grow past 100MB; loading those synchronously hangs the boot path and
// then the IPC hot path on every save. Anything bigger than this is treated
// as a schema mismatch and reset. Slots no longer carry message bodies (the
// feed is fetched with `include_messages=0`), so this is the only cap left.
const MAX_FACTS_FILE_BYTES: u64 = 3 * 1024 * 1024;

// Minimum Claude Code session count before we nudge a never-trained project.
// Below this, the user probably hasn't done enough real work on the project
// for train to find meaningful patterns.
pub(crate) const NEVER_TRAINED_MIN_SESSIONS: usize = 5;
// Cooldown between stale re-suggestions per project. Once the user has trained
// at least once, we only remind them weekly at most so the Activity feed
// doesn't turn into a nag screen.
pub(crate) const STALE_TRAIN_REFIRE_DAYS: i64 = 5;
// Only nudge Train for projects the user has touched recently. An abandoned
// project with 50 sessions and no train run shouldn't claim the tile forever.
pub(crate) const TRAIN_SUGGESTION_ACTIVE_WINDOW_DAYS: i64 = 2;
// Context size (tokens actually sent) that posts the `/autocompact 400k` tip.
// A replay of two weeks of heavy Claude Code use put the saving at ~10% for a
// 400k window; users who never reach it would gain nothing, so never see it.
pub(crate) const COMPACT_TIP_MIN_CONTEXT_TOKENS: u64 = 400_000;
// Haiku 5.5 bills every token of a prompt over 100k at 5x (Anthropic pricing,
// 2026-10). Its background calls in Claude Code peaked at 82k on a heavy
// machine, so only a session or subagent running on Haiku crosses it.
pub(crate) const HAIKU_PRICE_STEP_TOKENS: u64 = 100_000;
// Fable 5.1 lists like Fable 5 but reads cache at $0.25 vs $1 / MTok. Cache
// reads only dominate once a session is long, so short ones do not post it.
pub(crate) const FABLE_TIP_MIN_CONTEXT_TOKENS: u64 = 100_000;
// A conversation back from a break past its hour of cache that wrote at least
// this much again. Smaller rewrites cost too little to be worth a tip.
pub(crate) const IDLE_TIP_MIN_REWRITE_TOKENS: u64 = 100_000;
// Weekly pace: only judged once this much of the limit is used (earlier the
// projection swings), and only when the pace runs out half a day early or more.
const WEEKLY_PACE_MIN_USED_PERCENT: f64 = 50.0;
const WEEKLY_PACE_MIN_SHORTFALL_HOURS: i64 = 12;
const WEEK_SECS: i64 = 7 * 24 * 3600;
// A project's interactive sessions start at this median or more before the
// first prompt. Claude Code alone starts near 19k (a heavy machine, 2026-10).
pub(crate) const HEAVY_STARTUP_MIN_TOKENS: u64 = 50_000;

/// What a tick knows beyond the feed, for the tips the feed cannot show.
#[derive(Debug, Default, Clone)]
pub struct TipSignals {
    pub extra_usage_in_use: bool,
    pub usage_limit_hit: bool,
    /// `autoContinueAtUsageLimit` is already on in ~/.claude/settings.json.
    pub auto_continue_on: bool,
    pub idle_resume_at: Option<DateTime<Utc>>,
    pub weekly: Option<PlanWindow>,
    /// (project folder name, median session start in tokens).
    pub heavy_startup: Option<(String, u64)>,
}

/// (used percent, reset, when this pace reaches the limit) when the weekly
/// window runs out at least `WEEKLY_PACE_MIN_SHORTFALL_HOURS` before it resets.
fn weekly_pace(
    week: PlanWindow,
    now: DateTime<Utc>,
) -> Option<(f64, DateTime<Utc>, DateTime<Utc>)> {
    let left = week.resets_at - now.timestamp();
    if !(WEEKLY_PACE_MIN_USED_PERCENT..100.0).contains(&week.used_percent)
        || !(3600..=WEEK_SECS).contains(&left)
    {
        return None;
    }
    let elapsed = (WEEK_SECS - left) as f64;
    let limit_at = week.resets_at - WEEK_SECS + (elapsed * 100.0 / week.used_percent) as i64;
    if limit_at > week.resets_at - WEEKLY_PACE_MIN_SHORTFALL_HOURS * 3600 {
        return None;
    }
    Some((
        week.used_percent,
        DateTime::from_timestamp(week.resets_at, 0)?,
        DateTime::from_timestamp(limit_at, 0)?,
    ))
}

// The "Recent large compression" tile should actually be *large*. A pipeline
// run that stripped nothing (tokens_saved ~ 0, savings_percent near 0) would
// otherwise claim the slot and render "Saved 0 tokens". A high percent on a
// tiny request ("saved 40 tokens, 30%") is also not worth surfacing, so we
// also require an absolute floor on tokens saved.
// Measured on the new-input basis (models.rs apply_new_input_basis); on the
// feed's full-transcript basis nothing cleared this in long sessions and the
// tile froze for 11 days.
const TRANSFORMATION_TILE_MIN_SAVINGS_PERCENT: f64 = 20.0;
const TRANSFORMATION_TILE_MIN_TOKENS_SAVED: u64 = 1_000;
// Even a genuine huge compression shouldn't pin the tile forever — swap in
// any qualifying event once the current one has been sitting around for this
// long, so the feed keeps circulating.
const TRANSFORMATION_TILE_STALE_AFTER_MINUTES: i64 = 10;

pub struct WeeklyTotals {
    pub total_tokens_saved: u64,
    pub total_savings_usd: f64,
    pub active_days: u32,
}

/// Tolerant deserializer for `Option<T>` fields whose inner struct may have
/// reshaped between releases. Reads the value as raw JSON first, then tries
/// to coerce it into `T`; on any failure (missing required field, wrong
/// type, enum variant gone, etc.) returns `None` instead of bubbling the
/// error up and failing the whole outer parse. Sibling fields keep their
/// values, the affected slot just goes empty until the next live observation
/// repaints it.
fn de_or_none<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(d)?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(serde_json::from_value(value).ok())
}

// Shared debounce for Daily and AllTime record tags. Once we've emitted a
// tag, the bar is already visible — a burst of beats that each nudge the
// number up by a fraction of a percent shouldn't repaint the same chip
// every row. Suppress a follow-up tag only when it lands within 24h of the
// last emission AND beats the previous by under 25%. First-ever emission
// (previous=None) or emission after 24h always fires.
fn debounce_suppress(
    previous: Option<u64>,
    last_emitted_at: Option<DateTime<Utc>>,
    tokens: u64,
    now: DateTime<Utc>,
) -> bool {
    match (previous, last_emitted_at) {
        (Some(prev), Some(prev_at)) if prev > 0 => {
            let within_24h = now.signed_duration_since(prev_at) < Duration::hours(24);
            let delta_pct = (tokens as f64 - prev as f64) / prev as f64 * 100.0;
            within_24h && delta_pct < 25.0
        }
        _ => false,
    }
}

// True when `last_worked_at` (RFC3339 from ClaudeCodeProject) is within the
// Train-suggestion active window of `observed_at`. Unparseable timestamps are
// treated as inactive — it's safer to stay silent than to nag the user about
// a project whose recency we can't verify.
fn worked_within_active_window(last_worked_at: &str, observed_at: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(last_worked_at) {
        Ok(ts) => {
            observed_at.signed_duration_since(ts.with_timezone(&Utc))
                <= Duration::days(TRAIN_SUGGESTION_ACTIVE_WINDOW_DAYS)
        }
        Err(_) => false,
    }
}

/// Per-project snapshot of the bullets seen at the start of the current UTC
/// day. `observe_learnings_today` diffs the incoming bullet set against this
/// snapshot to compute "added today" counts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProjectBulletSnapshot {
    #[serde(default)]
    claude_md: Vec<String>,
    #[serde(default)]
    memory_md: Vec<String>,
}

/// Per-project bullet lists supplied by the caller for the current observation.
/// The caller reads CLAUDE.md / MEMORY.md for every project that saw session
/// activity today so `observe_learnings_today` has a baseline regardless of
/// which project ends up being picked as most-active.
pub struct LearningsProjectInput {
    pub project_path: String,
    pub project_display_name: String,
    pub claude_md_bullets: Vec<String>,
    pub memory_md_bullets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyRecordFact {
    pub day: String,
    pub tokens_saved: u64,
    pub observed_at: DateTime<Utc>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub request_id: Option<String>,
    pub savings_percent: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersistedActivityFacts {
    schema_version: u8,
    // -- record bookkeeping --
    #[serde(default)]
    all_time_record_tokens: u64,
    #[serde(default, deserialize_with = "de_or_none")]
    daily_record: Option<DailyRecordFact>,
    #[serde(default)]
    last_weekly_recap_week_key: Option<String>,
    #[serde(default)]
    learnings_snapshot_day: Option<NaiveDate>,
    #[serde(default)]
    learnings_snapshots: BTreeMap<String, ProjectBulletSnapshot>,
    // Timestamps of the last record-tag emission we actually made for each
    // scope. Used to debounce near-identical beats so a burst of compressions
    // doesn't repaint the same chip every row (24h / 25% rule in
    // `debounce_suppress`).
    #[serde(default)]
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    daily_record_emitted_at: Option<DateTime<Utc>>,
    // Wall-clock time of the last weekly-recap *check* (not emission). Used
    // to rate-limit check attempts to once per 24h — the emission itself is
    // idempotent via `last_weekly_recap_week_key`, but the gate stops us
    // from re-aggregating `daily_savings` on every observation tick.
    // Absence (None) means "never checked" → due immediately, which is what
    // triggers the catch-up recap on the first launch after an upgrade.
    #[serde(default)]
    last_weekly_recap_check_at: Option<DateTime<Utc>>,
    // TrainSuggestion fire-once / cooldown maps. See observe_train_suggestions.
    #[serde(default)]
    train_suggestions_fired: BTreeSet<String>,
    #[serde(default)]
    stale_train_suggestions_fired_at: BTreeMap<String, DateTime<Utc>>,
    // Fire-once per tip kind (`TipKind::key`), apart from the slot so a
    // dismissed tip never re-arms. Strings, not the enum: a kind from a newer
    // build must not fail this whole file on a downgrade.
    #[serde(default)]
    tips_fired: BTreeSet<String>,

    // -- latest-of-kind tile slots --
    // The Activity tab shows one tile per kind, populated by the most recent
    // event of that kind. Rather than persist a queue of every event ever, we
    // store only the freshest event for each tile. Each slot is loaded via
    // `de_or_none` so a reshape of the inner struct empties just that slot
    // instead of failing the whole file load (see SCHEMA_VERSION comment).
    #[serde(default, deserialize_with = "de_or_none")]
    last_transformation: Option<TransformationFeedEvent>,
    #[serde(default, deserialize_with = "de_or_none")]
    last_record: Option<RecordEvent>,
    #[serde(default, deserialize_with = "de_or_none")]
    last_learnings_milestone: Option<LearningsMilestoneEvent>,
    #[serde(default, deserialize_with = "de_or_none")]
    last_weekly_recap: Option<WeeklyRecapEvent>,
    #[serde(default, deserialize_with = "de_or_none")]
    last_train_suggestion: Option<TrainSuggestionEvent>,
    #[serde(default, deserialize_with = "de_or_none")]
    last_tip: Option<TipEvent>,
}

pub struct ActivityFacts {
    path: PathBuf,
    all_time_record_tokens: u64,
    daily_record: Option<DailyRecordFact>,
    last_weekly_recap_week_key: Option<String>,
    learnings_snapshot_day: Option<NaiveDate>,
    learnings_snapshots: BTreeMap<String, ProjectBulletSnapshot>,
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    daily_record_emitted_at: Option<DateTime<Utc>>,
    last_weekly_recap_check_at: Option<DateTime<Utc>>,
    train_suggestions_fired: BTreeSet<String>,
    stale_train_suggestions_fired_at: BTreeMap<String, DateTime<Utc>>,
    tips_fired: BTreeSet<String>,
    // Latest-of-kind tile slots. Each observe_* writes to its slot; the
    // snapshot builder reads them to populate the frontend response.
    last_transformation: Option<TransformationFeedEvent>,
    last_record: Option<RecordEvent>,
    last_learnings_milestone: Option<LearningsMilestoneEvent>,
    last_weekly_recap: Option<WeeklyRecapEvent>,
    last_train_suggestion: Option<TrainSuggestionEvent>,
    last_tip: Option<TipEvent>,
    // In memory only: the last conversation back after its cache expired,
    // joined against the feed's rewrites (see observe_signal_tips).
    idle_resume_at: Option<DateTime<Utc>>,
    dirty: bool,
    // Set when the file exists but could not be read. The in-memory facts
    // start empty, so saving them would overwrite the user's real history;
    // the file is left for the next launch to read instead.
    read_failed: bool,
}

impl ActivityFacts {
    pub fn load_or_create(base_dir: &Path) -> Result<Self> {
        let path = config_file(base_dir, "activity-facts.json");
        if !path.exists() {
            return Ok(Self::empty(path));
        }

        // Pre-v2 schemas accumulated full request/response bodies in two
        // queues and could grow into the 100s of MB. Refuse to even load
        // those — drop the file and start fresh. Keeps boot fast and the
        // IPC hot path unblocked.
        if let Ok(metadata) = std::fs::metadata(&path) {
            if metadata.len() > MAX_FACTS_FILE_BYTES {
                let _ = std::fs::remove_file(&path);
                return Ok(Self::empty(path));
            }
        }

        // An unreadable file (EACCES/EROFS, a Windows AV lock) must not brick
        // launch any more than a corrupt one: degrade to a fresh start, but
        // never save that emptiness over it. An I/O error says nothing about
        // the bytes, so they are not quarantined either (a second failure
        // would overwrite the one `.corrupt` slot, see load_setup_state).
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                log::warn!("activity-facts.json unreadable ({err}); starting fresh without saving");
                let mut facts = Self::empty(path);
                facts.read_failed = true;
                return Ok(facts);
            }
        };
        // A corrupt file (e.g. truncated by a crash mid-write) must never
        // brick launch: an Err here propagates to AppState::new()'s expect()
        // and panics on every start until the user deletes the file by hand.
        // Move the bytes aside (not delete them) and start fresh.
        let persisted = match serde_json::from_slice::<PersistedActivityFacts>(&bytes) {
            Ok(persisted) => persisted,
            Err(err) => {
                crate::client_adapters::quarantine_unparsable(
                    &path,
                    &format!("activity facts: {err}"),
                );
                return Ok(Self::empty(path));
            }
        };
        if persisted.schema_version != SCHEMA_VERSION {
            // Best-effort delete so the next save replaces the stale file
            // outright rather than silently leaving the old payload behind.
            let _ = std::fs::remove_file(&path);
            // Salvage the format-agnostic bookkeeping: schema bumps reshape
            // the *tile slots*, but the record counters, recap dedupe keys,
            // and fire-once sets are stable scalars — wiping them used to
            // re-fire the weekly recap and reset all-time records for every
            // user on every bump (four so far).
            let mut carried = Self::empty(path);
            carried.all_time_record_tokens = persisted.all_time_record_tokens;
            carried.all_time_record_emitted_at = persisted.all_time_record_emitted_at;
            carried.last_weekly_recap_week_key = persisted.last_weekly_recap_week_key;
            carried.last_weekly_recap_check_at = persisted.last_weekly_recap_check_at;
            carried.train_suggestions_fired = persisted.train_suggestions_fired;
            carried.stale_train_suggestions_fired_at = persisted.stale_train_suggestions_fired_at;
            carried.tips_fired = persisted.tips_fired;
            carried.dirty = true;
            return Ok(carried);
        }

        Ok(Self {
            path,
            all_time_record_tokens: persisted.all_time_record_tokens,
            daily_record: persisted.daily_record,
            last_weekly_recap_week_key: persisted.last_weekly_recap_week_key,
            learnings_snapshot_day: persisted.learnings_snapshot_day,
            learnings_snapshots: persisted.learnings_snapshots,
            all_time_record_emitted_at: persisted.all_time_record_emitted_at,
            daily_record_emitted_at: persisted.daily_record_emitted_at,
            last_weekly_recap_check_at: persisted.last_weekly_recap_check_at,
            train_suggestions_fired: persisted.train_suggestions_fired,
            stale_train_suggestions_fired_at: persisted.stale_train_suggestions_fired_at,
            tips_fired: persisted.tips_fired,
            last_transformation: persisted.last_transformation,
            last_record: persisted.last_record,
            last_learnings_milestone: persisted.last_learnings_milestone,
            last_weekly_recap: persisted.last_weekly_recap,
            last_train_suggestion: persisted.last_train_suggestion,
            last_tip: persisted.last_tip,
            idle_resume_at: None,
            dirty: false,
            read_failed: false,
        })
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            all_time_record_tokens: 0,
            daily_record: None,
            last_weekly_recap_week_key: None,
            learnings_snapshot_day: None,
            learnings_snapshots: BTreeMap::new(),
            all_time_record_emitted_at: None,
            daily_record_emitted_at: None,
            last_weekly_recap_check_at: None,
            train_suggestions_fired: BTreeSet::new(),
            stale_train_suggestions_fired_at: BTreeMap::new(),
            tips_fired: BTreeSet::new(),
            last_transformation: None,
            last_record: None,
            last_learnings_milestone: None,
            last_weekly_recap: None,
            last_train_suggestion: None,
            last_tip: None,
            idle_resume_at: None,
            dirty: false,
            read_failed: false,
        }
    }

    /// Build the Activity-tab snapshot from the latest-of-kind slots. One slot
    /// per tile; each tile on the frontend renders its slot or a placeholder.
    pub fn activity_feed_snapshot(&self) -> ActivityFeedSnapshot {
        ActivityFeedSnapshot {
            transformation: self.last_transformation.clone(),
            record: self.last_record.clone(),
            rtk_today: None,
            serena_today: None,
            learnings_milestone: self.last_learnings_milestone.clone(),
            weekly_recap: self.last_weekly_recap.clone(),
            train_suggestion: self.last_train_suggestion.clone(),
            tip: self.last_tip.clone(),
        }
    }

    /// A tip is posted and the Activity tab has not shown it yet.
    pub fn tip_unseen(&self) -> bool {
        self.last_tip.as_ref().is_some_and(|tip| !tip.seen)
    }

    pub fn mark_tip_seen(&mut self) {
        if let Some(tip) = self.last_tip.as_mut().filter(|tip| !tip.seen) {
            tip.seen = true;
            self.dirty = true;
        }
    }

    pub fn dismiss_tip(&mut self) {
        if self.last_tip.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn tip_fired(&self, kind: TipKind) -> bool {
        self.tips_fired.contains(kind.key())
    }

    /// Post the first candidate that has not fired yet. One tip at a time: a
    /// new one waits (its condition recurs) until the current one is seen.
    fn post_first_tip(&mut self, candidates: impl IntoIterator<Item = Option<TipEvent>>) {
        if self.last_tip.as_ref().is_some_and(|tip| !tip.seen) {
            return;
        }
        let Some(tip) = candidates
            .into_iter()
            .flatten()
            .find(|tip| !self.tip_fired(tip.kind))
        else {
            return;
        };
        self.tips_fired.insert(tip.kind.key().to_string());
        self.last_tip = Some(tip);
        self.dirty = true;
    }

    /// Tips a Claude Code request in the feed qualifies for.
    fn observe_tips(&mut self, event: &TransformationFeedEvent, observed_at: DateTime<Utc>) {
        if event.provider.as_deref() != Some("anthropic") {
            return;
        }
        let model = event.model.as_deref().unwrap_or_default();
        let tokens = event.context_tokens().unwrap_or(0);
        let haiku = model.starts_with("claude-haiku");
        // Written again rather than read from cache: what an expired prompt cache costs.
        let rewritten = event
            .uncached_input_tokens
            .unwrap_or(0)
            .saturating_add(event.cache_write_tokens.unwrap_or(0));
        let after_idle_resume = self.idle_resume_at.is_some_and(|resumed| {
            observed_at >= resumed - Duration::seconds(10)
                && observed_at <= resumed + Duration::minutes(15)
        });
        let tip = |kind| TipEvent::new(kind, observed_at, tokens);
        self.post_first_tip([
            (model.starts_with("claude-haiku-5-5") && tokens > HAIKU_PRICE_STEP_TOKENS)
                .then(|| tip(TipKind::HaikuLongContext)),
            (is_fable_5_0(model) && tokens >= FABLE_TIP_MIN_CONTEXT_TOKENS)
                .then(|| tip(TipKind::FableUpgrade)),
            (after_idle_resume
                && rewritten >= IDLE_TIP_MIN_REWRITE_TOKENS
                && rewritten.saturating_mul(10) >= tokens.saturating_mul(9))
            .then(|| tip(TipKind::IdleBreak)),
            (!haiku && tokens >= COMPACT_TIP_MIN_CONTEXT_TOKENS)
                .then(|| tip(TipKind::CompactWindow)),
        ]);
    }

    /// Tips from what the intercept, the plan usage headers and the session
    /// transcripts show, not the feed. Called once per observer tick, before
    /// the feed batch, so the idle-resume join sees this tick's signal.
    pub fn observe_signal_tips(&mut self, signals: &TipSignals, now: DateTime<Utc>) {
        self.idle_resume_at = signals.idle_resume_at;
        let pace = signals.weekly.and_then(|week| weekly_pace(week, now));
        self.post_first_tip([
            (signals.usage_limit_hit && !signals.auto_continue_on)
                .then(|| TipEvent::new(TipKind::AutoContinue, now, 0)),
            pace.map(|(used_percent, resets_at, limit_at)| TipEvent {
                used_percent: Some(used_percent),
                resets_at: Some(resets_at),
                limit_at: Some(limit_at),
                ..TipEvent::new(TipKind::WeeklyPace, now, 0)
            }),
            signals
                .extra_usage_in_use
                .then(|| TipEvent::new(TipKind::ExtraUsageCache, now, 0)),
            signals
                .heavy_startup
                .clone()
                .map(|(project, median)| TipEvent {
                    project: Some(project),
                    ..TipEvent::new(TipKind::HeavyStartup, now, median)
                }),
        ]);
    }

    pub fn observe_transformation(
        &mut self,
        event: &TransformationFeedEvent,
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        self.observe_transformation_at(event, observed_at, Utc::now())
    }

    pub fn observe_transformation_at(
        &mut self,
        event: &TransformationFeedEvent,
        observed_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        let mut emitted = Vec::new();

        self.observe_tips(event, observed_at);

        let tokens_saved = event
            .tokens_saved
            .and_then(|n| if n > 0 { Some(n as u64) } else { None });
        let savings_percent = event.savings_percent.unwrap_or(0.0);

        if let Some(tokens) = tokens_saved {
            // "Recent large compression" tile: must actually be large, and
            // we replace the current pick only if the new one is bigger or
            // the current one has been pinned longer than the stale window.
            if savings_percent > TRANSFORMATION_TILE_MIN_SAVINGS_PERCENT
                && tokens >= TRANSFORMATION_TILE_MIN_TOKENS_SAVED
            {
                let parse_ts = |ts: Option<&str>| {
                    ts.and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                        .map(|ts| ts.with_timezone(&Utc))
                };
                let should_replace = match self.last_transformation.as_ref() {
                    None => true,
                    Some(prev) => {
                        let prev_tokens = prev
                            .tokens_saved
                            .and_then(|n| u64::try_from(n).ok())
                            .unwrap_or(0);
                        let prev_ts = parse_ts(prev.timestamp.as_deref());
                        // The feed re-returns the same window on every poll,
                        // so only a newer event may take the tile. Replaying
                        // older ones swapped a stale pick for itself, or an
                        // older bigger event and the rotated pick took turns,
                        // rewriting activity-facts.json on every idle tick.
                        let newer = match (parse_ts(event.timestamp.as_deref()), prev_ts) {
                            (Some(ts), Some(prev_ts)) => ts > prev_ts,
                            _ => true,
                        };
                        // Only swap a same-or-smaller event in when the
                        // tile's current pick has aged past the stale
                        // window. Comparison is against wall-clock `now`
                        // so the rule matches the user's experience
                        // ("tile hasn't moved in 10 minutes, rotate it").
                        newer
                            && (tokens > prev_tokens
                                || prev_ts.is_none_or(|prev_ts| {
                                    now.signed_duration_since(prev_ts)
                                        > Duration::minutes(TRANSFORMATION_TILE_STALE_AFTER_MINUTES)
                                }))
                    }
                };
                if should_replace {
                    self.last_transformation = Some(event.clone());
                    self.dirty = true;
                }
            }

            // Local calendar days: UTC keys reset the daily record at
            // mid-afternoon for US users and disagreed with the savings chart.
            let today = crate::storage::user_day_key(now);
            let event_day = crate::storage::user_day_key(observed_at);
            let mut emit_tags: Vec<RecordTag> = Vec::new();
            let mut tile_tags: Vec<RecordTag> = Vec::new();
            let mut all_time_previous: Option<u64> = None;
            let mut beats_day_today = false;
            let mut beats_all_time = false;

            // Only track + celebrate a Daily record for events that happened
            // today. The proxy's feed is a sliding window that re-returns
            // historical transformations on every poll; without this guard,
            // each day boundary in the feed would oscillate `daily_record`
            // and emit a fresh (duplicate) Daily-tagged Record on every poll.
            if event_day == today {
                // `beats_day` plus the previous same-day tokens (None when
                // this is the first Daily of a new calendar day — so the
                // 24h/25% debounce can't accidentally suppress today's first
                // celebration just because yesterday's ran < 24h ago).
                let (beats_day, previous_same_day) = match &self.daily_record {
                    Some(existing) if existing.day == today => {
                        (tokens > existing.tokens_saved, Some(existing.tokens_saved))
                    }
                    _ => (true, None),
                };
                if beats_day {
                    self.daily_record = Some(DailyRecordFact {
                        day: today.clone(),
                        tokens_saved: tokens,
                        observed_at,
                        model: event.model.clone(),
                        provider: event.provider.clone(),
                        request_id: event.request_id.clone(),
                        savings_percent: event.savings_percent,
                    });
                    beats_day_today = true;
                    tile_tags.push(RecordTag::Daily);
                    if !debounce_suppress(
                        previous_same_day,
                        self.daily_record_emitted_at,
                        tokens,
                        now,
                    ) {
                        self.daily_record_emitted_at = Some(now);
                        emit_tags.push(RecordTag::Daily);
                    }
                }
            }

            if tokens > self.all_time_record_tokens {
                let previous_tokens = self.all_time_record_tokens;
                let previous = if previous_tokens == 0 {
                    None
                } else {
                    Some(previous_tokens)
                };
                self.all_time_record_tokens = tokens;
                beats_all_time = true;
                tile_tags.push(RecordTag::AllTime);
                all_time_previous = previous;
                if !debounce_suppress(previous, self.all_time_record_emitted_at, tokens, now) {
                    self.all_time_record_emitted_at = Some(now);
                    emit_tags.push(RecordTag::AllTime);
                }
            }

            // Tile slot always reflects the current best — a beat that bumps
            // the internal counter should show on the Activity tab even if its
            // notification was debounced. Otherwise the Record tile drifts
            // behind after a burst of small beats and users see an outdated
            // number while "Recent large compression" shows a bigger one.
            if beats_day_today || beats_all_time {
                let day = if beats_day_today {
                    Some(today.clone())
                } else {
                    None
                };
                let tile_record = RecordEvent {
                    observed_at,
                    tags: tile_tags,
                    tokens_saved: tokens,
                    savings_percent: event.savings_percent,
                    model: event.model.clone(),
                    provider: event.provider.clone(),
                    request_id: event.request_id.clone(),
                    previous_record: all_time_previous,
                    day: day.clone(),
                    workspace: event.workspace.clone(),
                    input_tokens_original: event.input_tokens_original,
                    input_tokens_optimized: event.input_tokens_optimized,
                };
                self.last_record = Some(tile_record);
                self.dirty = true;
            }

            if !emit_tags.is_empty() {
                let day = if emit_tags.contains(&RecordTag::Daily) {
                    Some(today)
                } else {
                    None
                };
                let record = RecordEvent {
                    observed_at,
                    tags: emit_tags,
                    tokens_saved: tokens,
                    savings_percent: event.savings_percent,
                    model: event.model.clone(),
                    provider: event.provider.clone(),
                    request_id: event.request_id.clone(),
                    previous_record: all_time_previous,
                    day,
                    workspace: event.workspace.clone(),
                    input_tokens_original: event.input_tokens_original,
                    input_tokens_optimized: event.input_tokens_optimized,
                };
                emitted.push(ActivityEvent::Record(record));
            }
        }

        emitted
    }

    /// Refresh the learnings tile with today's diff against a per-project
    /// bullet-set snapshot taken on the first observation of each day.
    ///
    /// - `patterns_today`: already-computed count of memory.db entries whose
    ///   `created_at` falls within today (caller filters the export JSON).
    /// - `project_inputs`: current CLAUDE.md / MEMORY.md bullet sets for every
    ///   project that had session activity today. Keyed by absolute project
    ///   path. Snapshots are taken against this set, so giving us a baseline
    ///   for any project the user might touch later in the day.
    /// - `active_project_path`: the project attributed to today's counts (the
    ///   one with the most Claude Code session files modified today). `None`
    ///   when the user hasn't worked on anything today.
    pub fn observe_learnings_today(
        &mut self,
        patterns_today: u32,
        project_inputs: Vec<LearningsProjectInput>,
        active_project_path: Option<&str>,
        observed_at: DateTime<Utc>,
    ) -> LearningsMilestoneEvent {
        let today = crate::storage::user_day(observed_at);
        let day_changed = self.learnings_snapshot_day != Some(today);

        if day_changed {
            // New local day — drop yesterday's snapshots and re-baseline against
            // whatever the caller just observed. Today's diffs against this
            // set start at zero.
            self.learnings_snapshots.clear();
            self.learnings_snapshot_day = Some(today);
            for input in &project_inputs {
                self.learnings_snapshots.insert(
                    input.project_path.clone(),
                    ProjectBulletSnapshot {
                        claude_md: input.claude_md_bullets.clone(),
                        memory_md: input.memory_md_bullets.clone(),
                    },
                );
            }
            self.dirty = true;
        } else {
            // Same day — add baselines for any project we haven't seen yet.
            // Existing snapshots are left alone so subsequent diffs keep the
            // start-of-day baseline.
            for input in &project_inputs {
                if !self.learnings_snapshots.contains_key(&input.project_path) {
                    self.learnings_snapshots.insert(
                        input.project_path.clone(),
                        ProjectBulletSnapshot {
                            claude_md: input.claude_md_bullets.clone(),
                            memory_md: input.memory_md_bullets.clone(),
                        },
                    );
                    self.dirty = true;
                }
            }
        }

        let active_input = active_project_path.and_then(|path| {
            project_inputs
                .iter()
                .find(|input| input.project_path == path)
        });

        let (learnings_today, reminders_today, project_path, project_display_name) =
            if let Some(input) = active_input {
                let snapshot = self.learnings_snapshots.get(&input.project_path);
                let claude_baseline: BTreeSet<&str> = snapshot
                    .map(|s| s.claude_md.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                let memory_baseline: BTreeSet<&str> = snapshot
                    .map(|s| s.memory_md.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                let learnings_today = input
                    .claude_md_bullets
                    .iter()
                    .filter(|b| !claude_baseline.contains(b.as_str()))
                    .count() as u32;
                let reminders_today = input
                    .memory_md_bullets
                    .iter()
                    .filter(|b| !memory_baseline.contains(b.as_str()))
                    .count() as u32;
                (
                    learnings_today,
                    reminders_today,
                    Some(input.project_path.clone()),
                    Some(input.project_display_name.clone()),
                )
            } else {
                (0, 0, None, None)
            };

        let event = LearningsMilestoneEvent {
            observed_at,
            patterns_today,
            reminders_today,
            learnings_today,
            project_path,
            project_display_name,
        };

        // Compare without observed_at: it is `now` on every 20s tick, so
        // including it rewrote activity-facts.json on every tick forever. The
        // stored observed_at therefore dates the last change.
        let changed = self.last_learnings_milestone.as_ref().is_none_or(|prev| {
            *prev
                != LearningsMilestoneEvent {
                    observed_at: prev.observed_at,
                    ..event.clone()
                }
        });
        if changed {
            self.last_learnings_milestone = Some(event.clone());
            self.dirty = true;
        }
        event
    }

    /// Scan project metadata and emit a `TrainSuggestion` for the first
    /// (most recently worked) project that matches a trigger. Applies to both
    /// kinds only when the project's folder still exists and the user has
    /// worked on it within `TRAIN_SUGGESTION_ACTIVE_WINDOW_DAYS` - the tile
    /// is for ongoing work, not abandoned folders. Two kinds:
    ///
    /// - `"never_trained"` — user has logged `NEVER_TRAINED_MIN_SESSIONS`+
    ///   sessions but never run Train on this project. Fires once per project,
    ///   ever (gated by `train_suggestions_fired`).
    /// - `"stale"` — user has trained before but worked on the project 2+
    ///   active days since. Throttled to at most once per
    ///   `STALE_TRAIN_REFIRE_DAYS` per project via
    ///   `stale_train_suggestions_fired_at` so the Activity feed doesn't turn
    ///   into a nag screen.
    ///
    /// The tile shows one suggestion, so while the latched one still
    /// qualifies only that project may re-fire. Every other project keeps its
    /// fire-once flag and cooldown for when the tile frees up; spending them
    /// all on one tick meant only one project was ever nudged.
    pub fn observe_train_suggestions(
        &mut self,
        projects: &[ClaudeCodeProject],
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        // Clear a stale latch: the tile should stop showing "no Train run
        // yet" for a project the user has clearly moved past.
        //   - "never_trained" suggestions clear once the project has been
        //     trained (last_learn_ran_at becomes Some).
        //   - "stale" suggestions clear once active_days_since_last_learn
        //     drops below the threshold.
        //   - Any suggestion clears if the user hasn't touched the project
        //     in the active window - same gate that blocks emission, applied
        //     to the latch so an abandoned project doesn't stay pinned.
        //   - Any suggestion clears if the project's cwd no longer exists on
        //     disk - `~/.claude/projects/` keeps session files for folders
        //     that have been moved or deleted, so scanning surfaces "ghost"
        //     projects whose display_name collides with the current working
        //     copy and confuses the tile ("23 sessions on headroom-desktop
        //     and no Train run yet" for a path that isn't there anymore).
        if let Some(latched) = self.last_train_suggestion.as_ref() {
            let still_qualifies = projects
                .iter()
                .find(|p| p.project_path == latched.project_path)
                .map(|p| {
                    if !Path::new(&p.project_path).exists() {
                        return false;
                    }
                    if !worked_within_active_window(&p.last_worked_at, observed_at) {
                        return false;
                    }
                    match latched.kind.as_str() {
                        "never_trained" => p.last_learn_ran_at.is_none(),
                        "stale" => p.active_days_since_last_learn >= 2,
                        _ => true,
                    }
                })
                .unwrap_or(false);
            if !still_qualifies {
                self.last_train_suggestion = None;
                self.dirty = true;
            }
        }
        let latched_path = self
            .last_train_suggestion
            .as_ref()
            .map(|s| s.project_path.clone());

        for project in projects {
            if latched_path
                .as_ref()
                .is_some_and(|path| *path != project.project_path)
            {
                continue;
            }
            if !worked_within_active_window(&project.last_worked_at, observed_at)
                || !Path::new(&project.project_path).exists()
            {
                continue;
            }
            let (kind, active_days) = if project.last_learn_ran_at.is_none() {
                if project.session_count < NEVER_TRAINED_MIN_SESSIONS {
                    continue;
                }
                if self.train_suggestions_fired.contains(&project.project_path) {
                    continue;
                }
                ("never_trained", 0u32)
            } else if project.active_days_since_last_learn >= 2 {
                let throttled = self
                    .stale_train_suggestions_fired_at
                    .get(&project.project_path)
                    .is_some_and(|last| {
                        observed_at.signed_duration_since(*last)
                            < Duration::days(STALE_TRAIN_REFIRE_DAYS)
                    });
                if throttled {
                    continue;
                }
                ("stale", project.active_days_since_last_learn as u32)
            } else {
                continue;
            };

            match kind {
                "never_trained" => {
                    self.train_suggestions_fired
                        .insert(project.project_path.clone());
                }
                "stale" => {
                    self.stale_train_suggestions_fired_at
                        .insert(project.project_path.clone(), observed_at);
                }
                _ => {}
            }
            let event = TrainSuggestionEvent {
                observed_at,
                project_path: project.project_path.clone(),
                project_display_name: project.display_name.clone(),
                session_count: project.session_count as u32,
                active_days_since_last_learn: active_days,
                kind: kind.into(),
            };
            self.last_train_suggestion = Some(event.clone());
            self.dirty = true;
            return vec![ActivityEvent::TrainSuggestion(event)];
        }
        Vec::new()
    }

    /// True when we haven't checked the weekly recap within the last 24h.
    /// Used as a cheap pre-gate in the caller so `daily_savings` isn't
    /// re-aggregated on every observation tick. First-ever call (never
    /// checked) returns true, which is what drives the catch-up recap on
    /// the first launch after an upgrade.
    pub fn weekly_recap_check_due(&self, now: DateTime<Utc>) -> bool {
        self.last_weekly_recap_check_at
            .map(|last| now.signed_duration_since(last) > Duration::hours(24))
            .unwrap_or(true)
    }

    /// Record a weekly recap for the 7 days ending the day before
    /// `recap_monday` (so `recap_monday = 2026-04-27` recaps Mon 04-20 → Sun
    /// 04-26). Caller is responsible for passing a Monday; the function
    /// trusts that invariant.
    ///
    /// Two idempotency gates:
    ///   1. `weekly_recap_check_due` — skip if we ran a check within 24h.
    ///   2. `last_weekly_recap_week_key` — skip if this specific week has
    ///      already been recapped.
    ///
    /// The first gate always updates `last_weekly_recap_check_at` when it
    /// passes, even if the second gate or `active_days == 0` blocks emission
    /// — otherwise a dead week would trigger aggregation every poll forever.
    pub fn maybe_record_weekly_recap(
        &mut self,
        recap_monday: NaiveDate,
        totals: WeeklyTotals,
        observed_at: DateTime<Utc>,
    ) -> Option<ActivityEvent> {
        if !self.weekly_recap_check_due(observed_at) {
            return None;
        }
        self.last_weekly_recap_check_at = Some(observed_at);
        self.dirty = true;

        let week_key = recap_monday.format("%Y-%m-%d").to_string();
        if self.last_weekly_recap_week_key.as_deref() == Some(week_key.as_str()) {
            return None;
        }
        if totals.active_days == 0 {
            return None;
        }
        let week_start = recap_monday
            .checked_sub_days(chrono::Days::new(7))
            .unwrap_or(recap_monday);
        let week_end = recap_monday.pred_opt().unwrap_or(recap_monday);
        let recap = WeeklyRecapEvent {
            observed_at,
            week_start: week_start.format("%Y-%m-%d").to_string(),
            week_end: week_end.format("%Y-%m-%d").to_string(),
            total_tokens_saved: totals.total_tokens_saved,
            total_savings_usd: totals.total_savings_usd,
            active_days: totals.active_days,
        };
        self.last_weekly_recap_week_key = Some(week_key);
        self.last_weekly_recap = Some(recap.clone());
        Some(ActivityEvent::WeeklyRecap(recap))
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty || self.read_failed {
            return Ok(());
        }
        let persisted = PersistedActivityFacts {
            schema_version: SCHEMA_VERSION,
            all_time_record_tokens: self.all_time_record_tokens,
            daily_record: self.daily_record.clone(),
            last_weekly_recap_week_key: self.last_weekly_recap_week_key.clone(),
            learnings_snapshot_day: self.learnings_snapshot_day,
            learnings_snapshots: self.learnings_snapshots.clone(),
            all_time_record_emitted_at: self.all_time_record_emitted_at,
            daily_record_emitted_at: self.daily_record_emitted_at,
            last_weekly_recap_check_at: self.last_weekly_recap_check_at,
            train_suggestions_fired: self.train_suggestions_fired.clone(),
            stale_train_suggestions_fired_at: self.stale_train_suggestions_fired_at.clone(),
            tips_fired: self.tips_fired.clone(),
            last_transformation: self.last_transformation.clone(),
            last_record: self.last_record.clone(),
            last_learnings_milestone: self.last_learnings_milestone.clone(),
            last_weekly_recap: self.last_weekly_recap.clone(),
            last_train_suggestion: self.last_train_suggestion.clone(),
            last_tip: self.last_tip.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&persisted).context("serializing activity facts")?;
        crate::client_adapters::atomic_write(&self.path, &bytes)?;
        self.dirty = false;
        Ok(())
    }
}

/// `claude-fable-5`, dated or with a `[1m]` suffix, but not `claude-fable-5-1`
/// or any later point release.
fn is_fable_5_0(model: &str) -> bool {
    let Some(rest) = model.strip_prefix("claude-fable-5") else {
        return false;
    };
    rest.is_empty()
        || rest.starts_with('[')
        || rest
            .strip_prefix('-')
            .is_some_and(|date| date.len() >= 8 && date.bytes().take(8).all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use tempfile::TempDir;

    #[test]
    fn load_or_create_survives_unparseable_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let path = base.join("config").join("activity-facts.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"schemaVersion\": 3, trunc").unwrap();

        let facts = ActivityFacts::load_or_create(&base).expect("corrupt file must not error");
        assert_eq!(facts.all_time_record_tokens, 0);
        assert!(
            !path.exists(),
            "corrupt file is moved aside for a fresh start"
        );
        assert_eq!(
            std::fs::read(base.join("config").join("activity-facts.json.corrupt")).unwrap(),
            b"{\"schemaVersion\": 3, trunc",
            "the corrupt bytes are kept, not deleted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_left_for_the_next_launch_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, base) = base_dir();
        let path = base.join("config").join("activity-facts.json");
        let original = format!(
            r#"{{"schemaVersion": {SCHEMA_VERSION}, "allTimeRecordTokens": 91234, "trainSuggestionsFired": ["/x/demo"]}}"#
        );
        std::fs::write(&path, &original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            return; // running as root: the mode does not block the read
        }

        let mut facts =
            ActivityFacts::load_or_create(&base).expect("unreadable file must not error");
        // The first observer tick always dirties fresh facts (new-day baseline).
        facts.observe_learnings_today(0, Vec::new(), None, at(10, 0));
        facts.save_if_dirty().unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "empty in-memory facts must not be saved over a file we could not read"
        );
        let reloaded = ActivityFacts::load_or_create(&base).unwrap();
        assert_eq!(reloaded.all_time_record_tokens, 91234);
    }

    #[test]
    fn schema_mismatch_salvages_records_and_recap_dedupe() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let path = base.join("config").join("activity-facts.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // An old-schema file: tile slots are stale, but the record counter and
        // recap dedupe key must survive the bump instead of re-firing
        // notifications and zeroing all-time records fleet-wide.
        std::fs::write(
            &path,
            br#"{"schemaVersion": 3, "allTimeRecordTokens": 91234, "lastWeeklyRecapWeekKey": "2026-W26"}"#,
        )
        .unwrap();

        let facts = ActivityFacts::load_or_create(&base).expect("mismatch must not error");
        assert_eq!(facts.all_time_record_tokens, 91234);
        assert_eq!(
            facts.last_weekly_recap_week_key.as_deref(),
            Some("2026-W26")
        );
        assert!(
            facts.last_transformation.is_none(),
            "tile slots from the old schema are dropped"
        );
    }

    fn mk_transformation(
        model: Option<&str>,
        tokens_saved: Option<i64>,
        savings_percent: Option<f64>,
    ) -> TransformationFeedEvent {
        TransformationFeedEvent {
            request_id: Some("req-1".into()),
            timestamp: Some("2026-04-22T10:00:00Z".into()),
            provider: Some("anthropic".into()),
            model: model.map(str::to_string),
            input_tokens_original: Some(1000),
            input_tokens_optimized: Some(300),
            tokens_saved,
            savings_percent,
            transforms_applied: vec!["kompress".into()],
            workspace: None,
            turn_id: None,
            uncached_input_tokens: None,
            cache_write_tokens: None,
            cache_read_tokens: None,
        }
    }

    fn base_dir() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        let base = tmp.path().to_path_buf();
        (tmp, base)
    }

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 22, h, m, 0).unwrap()
    }

    fn sized(provider: &str, model: &str, tokens: u64) -> TransformationFeedEvent {
        TransformationFeedEvent {
            provider: Some(provider.into()),
            model: Some(model.into()),
            input_tokens_optimized: Some(tokens),
            ..mk_transformation(None, None, None)
        }
    }

    fn posted(facts: &ActivityFacts) -> Option<(TipKind, u64)> {
        facts
            .activity_feed_snapshot()
            .tip
            .map(|tip| (tip.kind, tip.context_tokens))
    }

    #[test]
    fn compact_tip_fires_once_on_the_first_400k_claude_request() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let opus = "claude-opus-5-5";

        facts.observe_transformation_at(&sized("anthropic", opus, 399_999), at(10, 0), at(10, 0));
        facts.observe_transformation_at(&sized("openai", "gpt-6", 900_000), at(10, 1), at(10, 1));
        assert_eq!(posted(&facts), None);

        facts.observe_transformation_at(&sized("anthropic", opus, 400_000), at(10, 2), at(10, 2));
        assert_eq!(posted(&facts), Some((TipKind::CompactWindow, 400_000)));
        assert!(facts.tip_unseen());

        facts.mark_tip_seen();
        facts.save_if_dirty().unwrap();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        assert!(!facts.tip_unseen(), "seen survives a restart");

        facts.dismiss_tip();
        facts.observe_transformation_at(&sized("anthropic", opus, 700_000), at(11, 0), at(11, 0));
        assert_eq!(posted(&facts), None, "a dismissed tip never comes back");
    }

    #[test]
    fn tips_pick_the_kind_the_request_qualifies_for() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        // Haiku under the price step, and old Fable on a short session: nothing.
        facts.observe_transformation_at(
            &sized("anthropic", "claude-haiku-5-5", 100_000),
            at(9, 0),
            at(9, 0),
        );
        facts.observe_transformation_at(
            &sized("anthropic", "claude-fable-5", 99_999),
            at(9, 1),
            at(9, 1),
        );
        assert_eq!(posted(&facts), None);

        // Haiku over the step gets its own tip even at 400k+, never the compact one.
        facts.observe_transformation_at(
            &sized("anthropic", "claude-haiku-5-5", 450_000),
            at(9, 2),
            at(9, 2),
        );
        assert_eq!(posted(&facts), Some((TipKind::HaikuLongContext, 450_000)));

        // One at a time: an unseen tip is never replaced.
        facts.observe_transformation_at(
            &sized("anthropic", "claude-fable-5[1m]", 500_000),
            at(9, 3),
            at(9, 3),
        );
        assert_eq!(posted(&facts), Some((TipKind::HaikuLongContext, 450_000)));

        // Seen, the next one may take the slot; Fable 5 outranks the compact tip.
        facts.mark_tip_seen();
        facts.observe_transformation_at(
            &sized("anthropic", "claude-fable-5[1m]", 500_000),
            at(9, 4),
            at(9, 4),
        );
        assert_eq!(posted(&facts), Some((TipKind::FableUpgrade, 500_000)));

        // With Fable's fired, the same request falls through to the compact tip.
        facts.dismiss_tip();
        facts.observe_transformation_at(
            &sized("anthropic", "claude-fable-5[1m]", 500_000),
            at(9, 5),
            at(9, 5),
        );
        assert_eq!(posted(&facts), Some((TipKind::CompactWindow, 500_000)));
    }

    #[test]
    fn tips_size_the_whole_prompt_not_the_new_input() {
        // What the feed fetch hands the observer: apply_new_input_basis has
        // already turned input_tokens_optimized into new input alone.
        let mut event = TransformationFeedEvent {
            uncached_input_tokens: Some(2_000),
            cache_write_tokens: Some(8_000),
            cache_read_tokens: Some(440_000),
            ..sized("anthropic", "claude-opus-5-5", 0)
        };
        event.apply_new_input_basis();
        assert_eq!(event.input_tokens_optimized, Some(10_000));
        assert_eq!(event.context_tokens(), Some(450_000));

        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation_at(&event, at(10, 0), at(10, 0));
        assert_eq!(posted(&facts), Some((TipKind::CompactWindow, 450_000)));
    }

    fn rewrite(cache_read: u64, rewritten: u64) -> TransformationFeedEvent {
        TransformationFeedEvent {
            uncached_input_tokens: Some(1_000),
            cache_write_tokens: Some(rewritten - 1_000),
            cache_read_tokens: Some(cache_read),
            ..sized("anthropic", "claude-opus-5-5", 0)
        }
    }

    #[test]
    fn idle_break_needs_a_resume_and_a_whole_context_rewrite() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let resumed = TipSignals {
            idle_resume_at: Some(at(10, 0)),
            ..TipSignals::default()
        };

        // A rewrite with no resume nearby: a cache bust, not a break.
        facts.observe_transformation_at(&rewrite(0, 300_000), at(9, 0), at(9, 0));
        // A resumed conversation that read its cache: nothing expired.
        facts.observe_signal_tips(&resumed, at(10, 0));
        facts.observe_transformation_at(&rewrite(250_000, 20_000), at(10, 1), at(10, 1));
        assert_eq!(posted(&facts), None);

        facts.observe_transformation_at(&rewrite(10_000, 300_000), at(10, 2), at(10, 2));
        assert_eq!(posted(&facts), Some((TipKind::IdleBreak, 310_000)));
    }

    #[test]
    fn weekly_pace_projects_when_the_limit_runs_out() {
        let now = at(12, 0);
        let week = |used_percent, days_left: i64| PlanWindow {
            used_percent,
            resets_at: now.timestamp() + days_left * 24 * 3600,
        };
        // 4 of 7 days gone, 80% used: 100% lands at day 5, two days early.
        let (used, resets, limit) = weekly_pace(week(80.0, 3), now).expect("on pace to run out");
        assert_eq!(used, 80.0);
        assert_eq!((resets - limit).num_hours(), 48);
        // On track, under the floor, already out, or a stale window: nothing.
        assert_eq!(weekly_pace(week(55.0, 3), now), None);
        assert_eq!(weekly_pace(week(45.0, 6), now), None);
        assert_eq!(weekly_pace(week(100.0, 2), now), None);
        assert_eq!(weekly_pace(week(80.0, -1), now), None);
    }

    #[test]
    fn signal_tips_follow_their_signals() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let mut signals = TipSignals {
            usage_limit_hit: true,
            auto_continue_on: true,
            ..TipSignals::default()
        };
        facts.observe_signal_tips(&signals, at(10, 0));
        assert_eq!(posted(&facts), None, "auto-continue is already on");

        signals.auto_continue_on = false;
        signals.extra_usage_in_use = true;
        facts.observe_signal_tips(&signals, at(10, 1));
        assert_eq!(posted(&facts), Some((TipKind::AutoContinue, 0)));

        facts.mark_tip_seen();
        facts.observe_signal_tips(&signals, at(10, 2));
        assert_eq!(posted(&facts), Some((TipKind::ExtraUsageCache, 0)));

        facts.mark_tip_seen();
        signals.heavy_startup = Some(("headroom-desktop".into(), 69_399));
        facts.observe_signal_tips(&signals, at(10, 3));
        let tip = facts.activity_feed_snapshot().tip.unwrap();
        assert_eq!(tip.kind, TipKind::HeavyStartup);
        assert_eq!(
            (tip.project.as_deref(), tip.context_tokens),
            (Some("headroom-desktop"), 69_399)
        );
    }

    #[test]
    fn fable_5_0_matches_only_the_old_model() {
        for model in [
            "claude-fable-5",
            "claude-fable-5[1m]",
            "claude-fable-5-20260301",
        ] {
            assert!(is_fable_5_0(model), "{model}");
        }
        for model in [
            "claude-fable-5-1",
            "claude-fable-5-2",
            "claude-opus-5-5",
            "fable-5",
        ] {
            assert!(!is_fable_5_0(model), "{model}");
        }
    }

    fn is_daily_record(e: &ActivityEvent) -> bool {
        matches!(e, ActivityEvent::Record(r) if r.tags.contains(&RecordTag::Daily))
    }

    fn is_all_time_record(e: &ActivityEvent) -> bool {
        matches!(e, ActivityEvent::Record(r) if r.tags.contains(&RecordTag::AllTime))
    }

    #[test]
    fn daily_record_updates_only_on_beat_and_resets_on_day_change() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let events = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(500), Some(50.0)),
            at(10, 0),
            at(10, 0),
        );
        assert!(events.iter().any(is_daily_record));
        let events2 = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(200), Some(20.0)),
            at(10, 1),
            at(10, 1),
        );
        assert!(!events2.iter().any(is_daily_record));
        let next_day = Utc.with_ymd_and_hms(2026, 4, 23, 1, 0, 0).unwrap();
        let events3 = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(100), Some(10.0)),
            next_day,
            next_day,
        );
        assert!(events3.iter().any(is_daily_record));
    }

    #[test]
    fn historical_transformations_do_not_fire_daily_record() {
        // Regression: the proxy's /transformations/feed is a sliding window
        // that replays historical transformations on every poll. With multiple
        // days in the feed, the single-scalar `daily_record` used to oscillate
        // and fire a fresh DailyRecord every poll, piling duplicates into
        // recent_events. Today: historical events MUST NOT fire.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let today = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
        let yesterday = Utc.with_ymd_and_hms(2026, 4, 21, 12, 0, 0).unwrap();
        let two_days_ago = Utc.with_ymd_and_hms(2026, 4, 20, 12, 0, 0).unwrap();

        // Poll 1: today's tx + two historical ones.
        facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(500), Some(50.0)),
            today,
            today,
        );
        facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(700), Some(60.0)),
            yesterday,
            today,
        );
        facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(800), Some(70.0)),
            two_days_ago,
            today,
        );

        // Poll 2: SAME feed re-observed. None of the three must emit another
        // DailyRecord — previously all three would fire because the single
        // `daily_record.day` oscillated between 22, 21, 20 and back.
        for (obs_at, tokens) in [(today, 500i64), (yesterday, 700), (two_days_ago, 800)] {
            let events = facts.observe_transformation_at(
                &mk_transformation(Some("a"), Some(tokens), Some(50.0)),
                obs_at,
                today,
            );
            assert!(
                !events.iter().any(is_daily_record),
                "re-observing same tx (obs_at={obs_at}) must not re-fire DailyRecord",
            );
        }
    }

    #[test]
    fn all_time_record_includes_previous_record() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation(
            &mk_transformation(Some("a"), Some(500), Some(50.0)),
            at(10, 0),
        );
        let events = facts.observe_transformation(
            &mk_transformation(Some("a"), Some(900), Some(90.0)),
            at(10, 1),
        );
        let record = events
            .iter()
            .find_map(|e| match e {
                ActivityEvent::Record(r) if r.tags.contains(&RecordTag::AllTime) => Some(r),
                _ => None,
            })
            .expect("all-time record event");
        assert_eq!(record.previous_record, Some(500));
        assert_eq!(record.tokens_saved, 900);
    }

    #[test]
    fn all_time_record_debounces_tiny_beats_within_a_day() {
        // First all-time sets the bar and emits the tag. A 0.5% beat 10 min
        // later still advances the counter but MUST NOT re-tag — otherwise
        // consecutive Record cards both claim "All-time" and the chip loses
        // meaning. A subsequent beat >= 25% re-fires, as does any beat after
        // 24h have passed.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();

        let first = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_000), Some(50.0)),
            t0,
            t0,
        );
        assert!(first.iter().any(is_all_time_record));

        let tiny = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_005), Some(50.0)),
            t0 + Duration::minutes(10),
            t0 + Duration::minutes(10),
        );
        assert!(
            !tiny.iter().any(is_all_time_record),
            "0.5% beat within 24h must be suppressed",
        );
        assert_eq!(
            facts.all_time_record_tokens, 1_005,
            "counter still advances even when tag suppressed"
        );

        // >=25% beat inside 24h re-fires.
        let big = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_300), Some(50.0)),
            t0 + Duration::minutes(20),
            t0 + Duration::minutes(20),
        );
        assert!(big.iter().any(is_all_time_record));

        // Tiny beat but > 24h later re-fires.
        let late = t0 + Duration::hours(24) + Duration::minutes(30);
        let late_ev = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_305), Some(50.0)),
            late,
            late,
        );
        assert!(late_ev.iter().any(is_all_time_record));
    }

    #[test]
    fn daily_record_debounces_tiny_beats_within_the_day() {
        // Same debounce rules apply to the Daily tag: a tiny beat within 24h
        // is suppressed, but a >=25% beat re-fires. The first Daily of a new
        // calendar day always fires regardless of the 24h clock.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();

        let first = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_000), Some(50.0)),
            t0,
            t0,
        );
        assert!(first.iter().any(is_daily_record));

        let tiny = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_005), Some(50.0)),
            t0 + Duration::minutes(10),
            t0 + Duration::minutes(10),
        );
        assert!(
            !tiny.iter().any(is_daily_record),
            "0.5% daily beat within 24h must be suppressed",
        );

        let big = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(1_300), Some(50.0)),
            t0 + Duration::minutes(20),
            t0 + Duration::minutes(20),
        );
        assert!(big.iter().any(is_daily_record));

        // Next day: first beat always fires even if the previous day's Daily
        // was < 24h ago — a new calendar day deserves its own celebration.
        let next = Utc.with_ymd_and_hms(2026, 4, 23, 2, 0, 0).unwrap();
        let next_ev = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(500), Some(50.0)),
            next,
            next,
        );
        assert!(next_ev.iter().any(is_daily_record));
    }

    #[test]
    fn single_transformation_beating_daily_and_all_time_emits_one_record_with_both_tags() {
        // A single transformation that qualifies for Daily and All-time must
        // produce exactly one Record event carrying both tags — not two.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let t = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();
        let events = facts.observe_transformation_at(
            &mk_transformation(Some("a"), Some(10_000), Some(80.0)),
            t,
            t,
        );
        let records: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ActivityEvent::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 1, "must emit exactly one Record");
        assert_eq!(records[0].tags, vec![RecordTag::Daily, RecordTag::AllTime]);
        assert_eq!(records[0].tokens_saved, 10_000);
        assert!(records[0].day.is_some());
    }

    #[test]
    fn save_and_reload_is_idempotent() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation(
            &mk_transformation(Some("claude-x"), Some(1000), Some(60.0)),
            at(10, 0),
        );
        facts.save_if_dirty().unwrap();

        let mut reloaded = ActivityFacts::load_or_create(&base).unwrap();
        let events = reloaded.observe_transformation(
            &mk_transformation(Some("claude-x"), Some(500), Some(50.0)),
            at(11, 0),
        );
        assert!(events.is_empty(), "no new events after reload");
        assert_eq!(reloaded.all_time_record_tokens, 1000);
    }

    /// Bring every `de_or_none`-decorated slot into a populated state by
    /// running each kind of observation once. Returns the project path used
    /// for learnings/train so the assertion side can reference it.
    fn populate_all_de_or_none_slots(facts: &mut ActivityFacts, project_path: &str) {
        // daily_record + last_transformation + last_record.
        facts.observe_transformation_at(
            &mk_transformation(Some("claude-x"), Some(5_000), Some(70.0)),
            at(10, 0),
            at(10, 0),
        );
        // last_learnings_milestone.
        facts.observe_learnings_today(
            2,
            vec![mk_learn_input(project_path, "demo", &["a"], &["b"])],
            Some(project_path),
            at(10, 0),
        );
        // last_train_suggestion. The project path must exist on disk or the
        // observe call un-latches the slot it just set (see latch-clearing
        // block in observe_train_suggestions).
        facts.observe_train_suggestions(
            &[mk_project(project_path, 5, None, 0, "2026-04-22T10:00:00Z")],
            at(10, 0),
        );
        // last_weekly_recap.
        let monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 100,
                total_savings_usd: 1.0,
                active_days: 1,
            },
            at(10, 0),
        );
    }

    /// Snapshot of which slots are populated. Used by the per-field sweep so
    /// the body of one field's test reads as "this field empty, the other
    /// five still full" against a single struct.
    struct SlotPresence {
        daily_record: bool,
        last_transformation: bool,
        last_record: bool,
        last_learnings_milestone: bool,
        last_weekly_recap: bool,
        last_train_suggestion: bool,
    }

    impl SlotPresence {
        fn of(facts: &ActivityFacts) -> Self {
            Self {
                daily_record: facts.daily_record.is_some(),
                last_transformation: facts.last_transformation.is_some(),
                last_record: facts.last_record.is_some(),
                last_learnings_milestone: facts.last_learnings_milestone.is_some(),
                last_weekly_recap: facts.last_weekly_recap.is_some(),
                last_train_suggestion: facts.last_train_suggestion.is_some(),
            }
        }
    }

    /// Corrupt one camelCase field on disk and assert that load drops only
    /// that slot, leaves every other slot intact, and preserves scalars.
    fn assert_only_the_named_slot_drops(field: &'static str) {
        let (_tmp, base) = base_dir();
        let project_path = base.to_str().expect("utf-8 path").to_string();

        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        populate_all_de_or_none_slots(&mut facts, &project_path);
        facts.save_if_dirty().unwrap();
        let pre = SlotPresence::of(&facts);
        assert!(
            pre.daily_record
                && pre.last_transformation
                && pre.last_record
                && pre.last_learnings_milestone
                && pre.last_weekly_recap
                && pre.last_train_suggestion,
            "setup: every slot must be populated before corrupting one"
        );

        let path = base.join("config").join("activity-facts.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // A bare string is rejected by every event struct's deserializer.
        value[field] = serde_json::json!("not-a-valid-event");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let reloaded = ActivityFacts::load_or_create(&base).unwrap();
        let post = SlotPresence::of(&reloaded);
        assert_eq!(
            reloaded.all_time_record_tokens, 5_000,
            "scalar all_time_record_tokens survives corruption of {field}",
        );

        let want_dropped = match field {
            "dailyRecord" => &post.daily_record,
            "lastTransformation" => &post.last_transformation,
            "lastRecord" => &post.last_record,
            "lastLearningsMilestone" => &post.last_learnings_milestone,
            "lastWeeklyRecap" => &post.last_weekly_recap,
            "lastTrainSuggestion" => &post.last_train_suggestion,
            other => panic!("unknown field {other}"),
        };
        assert!(
            !want_dropped,
            "{field} should drop to None after corruption"
        );

        // All siblings must still be present.
        for (name, present) in [
            ("dailyRecord", post.daily_record),
            ("lastTransformation", post.last_transformation),
            ("lastRecord", post.last_record),
            ("lastLearningsMilestone", post.last_learnings_milestone),
            ("lastWeeklyRecap", post.last_weekly_recap),
            ("lastTrainSuggestion", post.last_train_suggestion),
        ] {
            if name != field {
                assert!(
                    present,
                    "sibling {name} should survive corruption of {field}",
                );
            }
        }
    }

    #[test]
    fn each_de_or_none_slot_is_independently_fault_tolerant() {
        // Sweep every field decorated with `deserialize_with = "de_or_none"`
        // so we catch the failure mode where the attribute was omitted on
        // one of them — that bug would silently break load for any user
        // whose corresponding event struct ever reshapes.
        for field in [
            "dailyRecord",
            "lastTransformation",
            "lastRecord",
            "lastLearningsMilestone",
            "lastWeeklyRecap",
            "lastTrainSuggestion",
        ] {
            assert_only_the_named_slot_drops(field);
        }
    }

    #[test]
    fn weekly_recap_same_week_does_not_re_emit_after_24h() {
        // Week-key idempotency: even if the 24h check gate has passed, the
        // same week key must not produce a second emission.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 4, 27, 10, 0, 0).unwrap();
        let first = facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 500,
                total_savings_usd: 2.5,
                active_days: 4,
            },
            t0,
        );
        assert!(first.is_some());
        // 48h later — 24h gate is open, but week key matches.
        let second = facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 999,
                total_savings_usd: 5.0,
                active_days: 7,
            },
            t0 + Duration::hours(48),
        );
        assert!(second.is_none());
    }

    #[test]
    fn weekly_recap_24h_gate_blocks_rapid_re_check() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 4, 27, 10, 0, 0).unwrap();
        // First call stamps the check timestamp regardless of outcome.
        facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 0,
                total_savings_usd: 0.0,
                active_days: 0,
            },
            t0,
        );
        // Different week, same day — 24h gate still blocks.
        let next_monday = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let blocked = facts.maybe_record_weekly_recap(
            next_monday,
            WeeklyTotals {
                total_tokens_saved: 500,
                total_savings_usd: 2.0,
                active_days: 3,
            },
            t0 + Duration::hours(12),
        );
        assert!(blocked.is_none());
        // > 24h later, the gate opens and the new week emits.
        let fresh = facts.maybe_record_weekly_recap(
            next_monday,
            WeeklyTotals {
                total_tokens_saved: 500,
                total_savings_usd: 2.0,
                active_days: 3,
            },
            t0 + Duration::hours(25),
        );
        assert!(fresh.is_some());
    }

    #[test]
    fn weekly_recap_catches_up_on_first_launch_after_gap() {
        // First launch after an upgrade: last_weekly_recap_check_at is None,
        // the check is due, and a recap fires for the most recent completed
        // week — even when observed on a Wednesday.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let recap_monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let wednesday = Utc.with_ymd_and_hms(2026, 4, 29, 10, 0, 0).unwrap();
        let event = facts
            .maybe_record_weekly_recap(
                recap_monday,
                WeeklyTotals {
                    total_tokens_saved: 1_234,
                    total_savings_usd: 3.0,
                    active_days: 5,
                },
                wednesday,
            )
            .expect("catch-up recap must emit");
        match event {
            ActivityEvent::WeeklyRecap(e) => {
                assert_eq!(e.week_start, "2026-04-20");
                assert_eq!(e.week_end, "2026-04-26");
                assert_eq!(e.active_days, 5);
            }
            _ => panic!("expected weekly recap"),
        }
    }

    #[test]
    fn weekly_recap_skips_empty_week() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let out = facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 0,
                total_savings_usd: 0.0,
                active_days: 0,
            },
            Utc::now(),
        );
        assert!(out.is_none());
    }

    #[test]
    fn weekly_recap_check_due_returns_true_when_never_checked() {
        let (_tmp, base) = base_dir();
        let facts = ActivityFacts::load_or_create(&base).unwrap();
        assert!(facts.weekly_recap_check_due(Utc::now()));
    }

    #[test]
    fn workspace_threads_through_to_record_events() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let mut transformation = mk_transformation(Some("claude-x"), Some(1_000), Some(50.0));
        transformation.workspace = Some("/Users/u/Code/demo-repo".into());
        let events = facts.observe_transformation_at(&transformation, at(10, 0), at(10, 0));
        let record = events
            .iter()
            .find_map(|e| match e {
                ActivityEvent::Record(r) => Some(r),
                _ => None,
            })
            .expect("record event");
        assert!(record.tags.contains(&RecordTag::Daily));
        assert!(record.tags.contains(&RecordTag::AllTime));
        assert_eq!(record.workspace.as_deref(), Some("/Users/u/Code/demo-repo"));
    }

    fn mk_learn_input(
        path: &str,
        display: &str,
        claude_md: &[&str],
        memory_md: &[&str],
    ) -> LearningsProjectInput {
        LearningsProjectInput {
            project_path: path.into(),
            project_display_name: display.into(),
            claude_md_bullets: claude_md.iter().map(|s| (*s).to_string()).collect(),
            memory_md_bullets: memory_md.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn learnings_today_baselines_on_first_observation_then_counts_diff() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        // First observation of the day: baseline is taken, today counts = 0.
        let first = facts.observe_learnings_today(
            0,
            vec![mk_learn_input(
                "/x/demo",
                "demo",
                &["existing learning"],
                &["existing reminder"],
            )],
            Some("/x/demo"),
            at(10, 0),
        );
        assert_eq!(first.learnings_today, 0);
        assert_eq!(first.reminders_today, 0);
        assert_eq!(first.project_display_name.as_deref(), Some("demo"));

        // Second observation, same day, with two new bullets in each file.
        let second = facts.observe_learnings_today(
            3,
            vec![mk_learn_input(
                "/x/demo",
                "demo",
                &["existing learning", "new learning A", "new learning B"],
                &["existing reminder", "new reminder"],
            )],
            Some("/x/demo"),
            at(11, 0),
        );
        assert_eq!(second.patterns_today, 3);
        assert_eq!(second.learnings_today, 2);
        assert_eq!(second.reminders_today, 1);
    }

    #[test]
    fn learnings_today_resets_snapshot_on_new_day() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        // Day one — baseline with three bullets.
        let day1 = Utc.with_ymd_and_hms(2026, 4, 23, 10, 0, 0).unwrap();
        facts.observe_learnings_today(
            0,
            vec![mk_learn_input("/x/demo", "demo", &["a", "b", "c"], &[])],
            Some("/x/demo"),
            day1,
        );

        // Day two — snapshot should reset, and 'd' is a new bullet today.
        let day2 = Utc.with_ymd_and_hms(2026, 4, 24, 10, 0, 0).unwrap();
        let before_add = facts.observe_learnings_today(
            0,
            vec![mk_learn_input("/x/demo", "demo", &["a", "b", "c"], &[])],
            Some("/x/demo"),
            day2,
        );
        assert_eq!(before_add.learnings_today, 0, "new day re-baselines to 0");

        let after_add = facts.observe_learnings_today(
            0,
            vec![mk_learn_input(
                "/x/demo",
                "demo",
                &["a", "b", "c", "d"],
                &[],
            )],
            Some("/x/demo"),
            day2.with_hour(11).unwrap(),
        );
        assert_eq!(after_add.learnings_today, 1);
    }

    #[test]
    fn learnings_today_no_active_project_yields_zero_counts() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        let event = facts.observe_learnings_today(0, Vec::new(), None, at(10, 0));
        assert_eq!(event.patterns_today, 0);
        assert_eq!(event.learnings_today, 0);
        assert_eq!(event.reminders_today, 0);
        assert!(event.project_path.is_none());
        assert!(event.project_display_name.is_none());
    }

    #[test]
    fn learnings_today_persists_snapshot_across_reload() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_learnings_today(
            0,
            vec![mk_learn_input("/x/demo", "demo", &["a"], &["m"])],
            Some("/x/demo"),
            at(10, 0),
        );
        facts.save_if_dirty().unwrap();

        let mut reloaded = ActivityFacts::load_or_create(&base).unwrap();
        let event = reloaded.observe_learnings_today(
            0,
            vec![mk_learn_input(
                "/x/demo",
                "demo",
                &["a", "new"],
                &["m", "n"],
            )],
            Some("/x/demo"),
            at(11, 0),
        );
        assert_eq!(event.learnings_today, 1);
        assert_eq!(event.reminders_today, 1);
    }

    #[test]
    fn learnings_tile_unchanged_counts_do_not_rewrite_the_file() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let input = || vec![mk_learn_input("/x/demo", "demo", &["a"], &["m"])];
        facts.observe_learnings_today(2, input(), Some("/x/demo"), at(10, 0));
        facts.save_if_dirty().unwrap();

        // The observer re-reads the same bullets every tick; only the clock moved.
        facts.observe_learnings_today(2, input(), Some("/x/demo"), at(10, 1));
        assert!(
            !facts.dirty,
            "an unchanged tile must not rewrite activity-facts.json every tick"
        );
        let tile = facts.activity_feed_snapshot().learnings_milestone.unwrap();
        assert_eq!(
            tile.observed_at,
            at(10, 0),
            "observed_at dates the last change"
        );

        // A real change still repaints the tile.
        facts.observe_learnings_today(3, input(), Some("/x/demo"), at(10, 2));
        assert!(facts.dirty);
        let tile = facts.activity_feed_snapshot().learnings_milestone.unwrap();
        assert_eq!(tile.observed_at, at(10, 2));
    }

    #[test]
    fn weekly_recap_window_spans_previous_seven_days() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let event = facts
            .maybe_record_weekly_recap(
                monday,
                WeeklyTotals {
                    total_tokens_saved: 500,
                    total_savings_usd: 2.5,
                    active_days: 4,
                },
                Utc::now(),
            )
            .unwrap();
        match event {
            ActivityEvent::WeeklyRecap(e) => {
                assert_eq!(e.week_start, "2026-04-20");
                assert_eq!(e.week_end, "2026-04-26");
                assert_eq!(e.active_days, 4);
            }
            _ => panic!("expected weekly recap"),
        }
    }

    fn mk_project(
        path: &str,
        sessions: usize,
        last_learn: Option<&str>,
        active_days: usize,
        last_worked_at: &str,
    ) -> ClaudeCodeProject {
        ClaudeCodeProject {
            id: path.chars().take(12).collect(),
            project_path: path.into(),
            display_name: path.rsplit('/').next().unwrap_or(path).into(),
            last_worked_at: last_worked_at.into(),
            session_count: sessions,
            sessions_today: 0,
            last_learn_ran_at: last_learn.map(str::to_string),
            has_persisted_learnings: last_learn.is_some(),
            active_days_since_last_learn: active_days,
            last_learn_pattern_count: None,
        }
    }

    #[test]
    fn train_suggestion_never_trained_fires_once_over_threshold() {
        let (tmp, base) = base_dir();
        let demo = project_dir(&tmp, "demo");
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project(&demo, 5, None, 0, "2026-04-22T10:00:00Z")];
        let first = facts.observe_train_suggestions(&projects, at(10, 0));
        assert_eq!(first.len(), 1);
        match &first[0] {
            ActivityEvent::TrainSuggestion(e) => {
                assert_eq!(e.kind, "never_trained");
                assert_eq!(e.project_path, demo);
                assert_eq!(e.session_count, 5);
            }
            _ => panic!("expected TrainSuggestion"),
        }
        let second = facts.observe_train_suggestions(&projects, at(11, 0));
        assert!(
            second.is_empty(),
            "never-trained must fire once per project"
        );
    }

    #[test]
    fn train_suggestion_never_trained_below_threshold_silent() {
        let (tmp, base) = base_dir();
        let demo = project_dir(&tmp, "demo");
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project(&demo, 4, None, 0, "2026-04-22T10:00:00Z")];
        assert!(facts
            .observe_train_suggestions(&projects, at(10, 0))
            .is_empty());
    }

    #[test]
    fn train_suggestion_stale_throttled_to_weekly() {
        let (tmp, base) = base_dir();
        let demo = project_dir(&tmp, "demo");
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // Rebuild the project with a fresh last_worked_at at each observation
        // — the active-window gate requires the user to still be touching the
        // project; we're testing the *cooldown*, not the active window.
        let mk = |worked_at: &str| {
            vec![mk_project(
                &demo,
                10,
                Some("2026-04-15T10:00:00Z"),
                3,
                worked_at,
            )]
        };
        let day0 = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();
        let first = facts.observe_train_suggestions(&mk("2026-04-22T10:00:00Z"), day0);
        assert_eq!(first.len(), 1);
        match &first[0] {
            ActivityEvent::TrainSuggestion(e) => assert_eq!(e.kind, "stale"),
            _ => panic!("expected stale TrainSuggestion"),
        }
        let day3 = day0 + Duration::days(3);
        assert!(
            facts
                .observe_train_suggestions(&mk("2026-04-25T10:00:00Z"), day3)
                .is_empty(),
            "within cooldown must not re-fire"
        );
        let day8 = day0 + Duration::days(8);
        let third = facts.observe_train_suggestions(&mk("2026-04-30T10:00:00Z"), day8);
        assert_eq!(third.len(), 1, "after cooldown, stale must re-fire");
    }

    #[test]
    fn train_suggestion_persists_across_reload() {
        let (tmp, base) = base_dir();
        let demo = project_dir(&tmp, "demo");
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project(&demo, 5, None, 0, "2026-04-22T10:00:00Z")];
        assert_eq!(
            facts.observe_train_suggestions(&projects, at(10, 0)).len(),
            1
        );
        facts.save_if_dirty().unwrap();
        let mut reloaded = ActivityFacts::load_or_create(&base).unwrap();
        assert!(
            reloaded
                .observe_train_suggestions(&projects, at(11, 0))
                .is_empty(),
            "fired set must survive reload"
        );
    }

    fn project_dir(tmp: &TempDir, name: &str) -> String {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn train_suggestion_spends_the_fire_once_flag_only_on_the_project_it_shows() {
        let (tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let (a, b) = (project_dir(&tmp, "a"), project_dir(&tmp, "b"));
        // A moved or deleted folder (a ghost) that is still in ~/.claude/projects.
        let ghost = tmp.path().join("gone").to_string_lossy().into_owned();
        let untrained = |path: &str| mk_project(path, 5, None, 0, "2026-04-22T10:00:00Z");
        let shown = |facts: &ActivityFacts| {
            facts
                .activity_feed_snapshot()
                .train_suggestion
                .map(|s| s.project_path)
        };
        // Most recently worked first, as list_claude_code_projects sorts them.
        let projects = vec![untrained(&ghost), untrained(&a), untrained(&b)];

        let first = facts.observe_train_suggestions(&projects, at(10, 0));
        assert_eq!(first.len(), 1, "the tile shows one nudge, so fire one");
        assert_eq!(shown(&facts), Some(a.clone()));
        assert_eq!(
            facts.train_suggestions_fired,
            BTreeSet::from([a.clone()]),
            "the ghost and b keep their fire-once flags"
        );

        // While a's nudge holds the tile, b waits with its flag intact.
        assert!(facts
            .observe_train_suggestions(&projects, at(10, 1))
            .is_empty());
        assert_eq!(shown(&facts), Some(a.clone()));

        // Once a is trained, b takes the tile.
        let trained_a = mk_project(
            &a,
            5,
            Some("2026-04-22T10:02:00Z"),
            0,
            "2026-04-22T10:02:00Z",
        );
        let projects = vec![untrained(&ghost), trained_a, untrained(&b)];
        assert_eq!(
            facts.observe_train_suggestions(&projects, at(10, 3)).len(),
            1
        );
        assert_eq!(shown(&facts), Some(b));
    }

    #[test]
    fn train_suggestion_skipped_when_project_idle_for_days() {
        let (tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // Over the never-trained session threshold, but the user hasn't
        // touched the project in 3 days — outside the active window.
        let never_trained = vec![mk_project(
            &project_dir(&tmp, "abandoned"),
            10,
            None,
            0,
            "2026-04-19T10:00:00Z",
        )];
        assert!(
            facts
                .observe_train_suggestions(&never_trained, at(10, 0))
                .is_empty(),
            "never-trained must not fire for idle projects"
        );
        // Same gate applies to the stale branch.
        let stale = vec![mk_project(
            &project_dir(&tmp, "abandoned-stale"),
            10,
            Some("2026-04-15T10:00:00Z"),
            5,
            "2026-04-19T10:00:00Z",
        )];
        assert!(
            facts
                .observe_train_suggestions(&stale, at(10, 0))
                .is_empty(),
            "stale must not fire for idle projects"
        );
    }

    #[test]
    fn last_transformation_slot_survives_reload() {
        // The whole point of the slot refactor is that tiles persist across
        // restarts. Seed a transformation, save, reload, assert the slot is
        // still populated with the same request_id.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let mut event = mk_transformation(Some("claude-opus-4-7"), Some(1_000), Some(50.0));
        event.request_id = Some("req-persist".into());
        facts.observe_transformation_at(&event, at(10, 0), at(10, 0));
        facts.save_if_dirty().unwrap();

        let reloaded = ActivityFacts::load_or_create(&base).unwrap();
        let snapshot = reloaded.activity_feed_snapshot();
        let slot = snapshot.transformation.expect("transformation slot");
        assert_eq!(slot.request_id.as_deref(), Some("req-persist"));
    }

    fn mk_tile_event(
        request_id: &str,
        timestamp: &str,
        tokens: i64,
        pct: f64,
    ) -> TransformationFeedEvent {
        let mut ev = mk_transformation(Some("m"), Some(tokens), Some(pct));
        ev.request_id = Some(request_id.into());
        ev.timestamp = Some(timestamp.into());
        ev
    }

    #[test]
    fn transformation_tile_skips_zero_token_events() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let ev = mk_tile_event("req-zero", "2026-04-22T10:00:00Z", 0, 90.0);
        facts.observe_transformation_at(&ev, at(10, 0), at(10, 0));
        assert!(facts.activity_feed_snapshot().transformation.is_none());
    }

    #[test]
    fn transformation_tile_skips_low_savings_percent() {
        // 20% exactly is still below the strictly-greater-than gate.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let ev = mk_tile_event("req-small", "2026-04-22T10:00:00Z", 1_000, 20.0);
        facts.observe_transformation_at(&ev, at(10, 0), at(10, 0));
        assert!(facts.activity_feed_snapshot().transformation.is_none());
    }

    #[test]
    fn transformation_tile_skips_tokens_saved_below_floor() {
        // High percent on a tiny request ("saved 900 tokens, 90%") must not
        // claim the tile — the absolute floor is 1_000 tokens saved.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let ev = mk_tile_event("req-below", "2026-04-22T10:00:00Z", 999, 90.0);
        facts.observe_transformation_at(&ev, at(10, 0), at(10, 0));
        assert!(facts.activity_feed_snapshot().transformation.is_none());
    }

    #[test]
    fn transformation_tile_replaces_when_new_event_is_larger() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let first = mk_tile_event("req-first", "2026-04-22T10:00:00Z", 1_000, 50.0);
        facts.observe_transformation_at(&first, at(10, 0), at(10, 0));

        // Two minutes later, a bigger qualifying event arrives — replace.
        let bigger = mk_tile_event("req-bigger", "2026-04-22T10:02:00Z", 2_000, 60.0);
        facts.observe_transformation_at(&bigger, at(10, 2), at(10, 2));

        let slot = facts
            .activity_feed_snapshot()
            .transformation
            .expect("transformation slot");
        assert_eq!(slot.request_id.as_deref(), Some("req-bigger"));
    }

    #[test]
    fn transformation_tile_keeps_larger_event_when_new_one_is_smaller_and_fresh() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let big = mk_tile_event("req-big", "2026-04-22T10:00:00Z", 5_000, 80.0);
        facts.observe_transformation_at(&big, at(10, 0), at(10, 0));

        // Smaller event 5 minutes later — inside the 10-min stale window, so
        // the bigger pick must stay.
        let small = mk_tile_event("req-small", "2026-04-22T10:05:00Z", 1_000, 30.0);
        facts.observe_transformation_at(&small, at(10, 5), at(10, 5));

        let slot = facts
            .activity_feed_snapshot()
            .transformation
            .expect("transformation slot");
        assert_eq!(slot.request_id.as_deref(), Some("req-big"));
    }

    #[test]
    fn transformation_tile_rotates_stale_pick_even_for_smaller_event() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let big = mk_tile_event("req-big", "2026-04-22T10:00:00Z", 5_000, 80.0);
        facts.observe_transformation_at(&big, at(10, 0), at(10, 0));

        // 11 minutes later — past the stale window — a smaller qualifying
        // event arrives. The tile should rotate even though it's smaller.
        let small = mk_tile_event("req-fresh", "2026-04-22T10:11:00Z", 1_000, 30.0);
        facts.observe_transformation_at(&small, at(10, 11), at(10, 11));

        let slot = facts
            .activity_feed_snapshot()
            .transformation
            .expect("transformation slot");
        assert_eq!(slot.request_id.as_deref(), Some("req-fresh"));
    }

    #[test]
    fn replaying_the_same_feed_window_leaves_the_transformation_tile_clean() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // The feed re-returns the same window on every poll, oldest first.
        let big = mk_tile_event("req-big", "2026-04-22T10:00:00Z", 5_000, 80.0);
        let small = mk_tile_event("req-small", "2026-04-22T10:05:00Z", 1_000, 30.0);
        let poll = |facts: &mut ActivityFacts, now| {
            facts.observe_transformation_at(&big, at(10, 0), now);
            facts.observe_transformation_at(&small, at(10, 5), now);
        };
        poll(&mut facts, at(10, 20));
        facts.save_if_dirty().unwrap();

        // Idle: the pick is past the stale window, but nothing newer arrived.
        poll(&mut facts, at(10, 40));
        assert!(
            !facts.dirty,
            "a replayed window must not rewrite activity-facts.json every tick"
        );
        let slot = facts.activity_feed_snapshot().transformation.unwrap();
        assert_eq!(slot.request_id.as_deref(), Some("req-small"));
    }
}
