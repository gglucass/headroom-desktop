use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    ActivityEvent, ClaudeCodeProject, LearningsMilestoneEvent, MilestoneEvent, NewModelEvent,
    RecordEvent, RecordTag, RtkBatchEvent, SavingsMilestoneEvent, StreakEvent,
    TransformationFeedEvent, TrainSuggestionEvent, WeeklyRecapEvent,
};
use crate::storage::config_file;
use crate::tool_manager::RtkGainSummary;

const SCHEMA_VERSION: u8 = 1;
const RECENT_EVENTS_CAP: usize = 300;

// Minimum Claude Code session count before we nudge a never-trained project.
// Below this, the user probably hasn't done enough real work on the project
// for train to find meaningful patterns.
pub(crate) const NEVER_TRAINED_MIN_SESSIONS: usize = 5;
// Cooldown between stale re-suggestions per project. Once the user has trained
// at least once, we only remind them weekly at most so the Activity feed
// doesn't turn into a nag screen.
pub(crate) const STALE_TRAIN_REFIRE_DAYS: i64 = 7;

// High-signal events (records, milestones, streaks, etc.) are rare and
// intrinsically interesting. Protect them from FIFO eviction caused by
// bursts of compressions / rtk batches / memories, which can push 150+
// events through the buffer in a single session and otherwise drown them
// out before the UI ever sees them.
pub(crate) fn is_high_signal(event: &ActivityEvent) -> bool {
    match event {
        ActivityEvent::Transformation(_)
        | ActivityEvent::Memory(_)
        | ActivityEvent::RtkBatch(_) => false,
        ActivityEvent::Milestone(_)
        | ActivityEvent::Record(_)
        | ActivityEvent::NewModel(_)
        | ActivityEvent::Streak(_)
        | ActivityEvent::SavingsMilestone(_)
        | ActivityEvent::LearningsMilestone(_)
        | ActivityEvent::WeeklyRecap(_)
        | ActivityEvent::TrainSuggestion(_) => true,
    }
}

// Persisted compression history cap. Big enough to cover a few days of
// moderate traffic but small enough to keep `activity-facts.json` well
// under a megabyte even with ~500-byte JSON events.
const TRANSFORMATION_HISTORY_CAP: usize = 500;

const FIRST_STREAKS: [u32; 4] = [3, 7, 14, 30];
const REPEATING_STREAK_STEP: u32 = 30;

fn milestone_kind(milestone_tokens_saved: u64) -> &'static str {
    match milestone_tokens_saved {
        100_000 => "first_100k",
        1_000_000 => "first_1m",
        5_000_000 => "first_5m",
        10_000_000 => "first_10m",
        _ => "repeating_10m",
    }
}

fn savings_milestone_kind(milestone_usd: u64) -> &'static str {
    match milestone_usd {
        10 => "first_10",
        50 => "first_50",
        100 => "first_100",
        _ => "repeating_100",
    }
}

/// Stable identity for a transformation, used to dedup the persisted
/// history when the proxy's sliding window re-returns the same events on
/// subsequent polls. `request_id` is the authoritative key; fall back to
/// `timestamp` for older payloads without it. Returns `None` when neither
/// is present — such events can't be deduped, so we don't persist them
/// (the proxy will keep surfacing them while they're in its window).
fn transformation_fingerprint(event: &TransformationFeedEvent) -> Option<String> {
    event.request_id.clone().or_else(|| event.timestamp.clone())
}

/// Append `event` to `history` if no entry with the same fingerprint exists.
/// Enforces the cap by popping the oldest entries from the front. Returns
/// `true` iff a new entry was actually added (i.e. the caller should mark
/// state dirty).
fn push_transformation_history_unique(
    history: &mut VecDeque<TransformationFeedEvent>,
    event: &TransformationFeedEvent,
    cap: usize,
) -> bool {
    let Some(fp) = transformation_fingerprint(event) else {
        return false;
    };
    // Walk newest-first; dups are almost always at the tail from the most
    // recent poll, so this is O(k) in the overlap between polls, not O(n).
    for existing in history.iter().rev() {
        if transformation_fingerprint(existing).as_deref() == Some(fp.as_str()) {
            return false;
        }
    }
    history.push_back(event.clone());
    while history.len() > cap {
        history.pop_front();
    }
    true
}

fn streaks_crossed(previous: u32, current: u32) -> Vec<u32> {
    if current <= previous {
        return Vec::new();
    }
    let mut thresholds = FIRST_STREAKS
        .into_iter()
        .filter(|t| previous < *t && current >= *t)
        .collect::<Vec<_>>();
    let first_repeating = previous / REPEATING_STREAK_STEP + 1;
    let last_repeating = current / REPEATING_STREAK_STEP;
    for index in first_repeating..=last_repeating {
        let days = index.saturating_mul(REPEATING_STREAK_STEP);
        if !thresholds.contains(&days) {
            thresholds.push(days);
        }
    }
    thresholds
}

pub struct WeeklyTotals {
    pub total_tokens_saved: u64,
    pub total_savings_usd: f64,
    pub active_days: u32,
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
    #[serde(default)]
    seen_models: BTreeSet<String>,
    #[serde(default)]
    all_time_record_tokens: u64,
    #[serde(default)]
    daily_record: Option<DailyRecordFact>,
    #[serde(default)]
    last_rtk_total_commands: u64,
    #[serde(default)]
    last_rtk_total_saved: u64,
    #[serde(default)]
    recent_events: VecDeque<ActivityEvent>,
    #[serde(default)]
    current_streak: u32,
    #[serde(default)]
    longest_streak: u32,
    #[serde(default)]
    last_active_day: Option<String>,
    #[serde(default)]
    last_weekly_recap_week_key: Option<String>,
    #[serde(default)]
    learnings_milestones_fired: BTreeSet<u32>,
    #[serde(default)]
    prompt_all_time_record_tokens: u64,
    #[serde(default)]
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    daily_record_emitted_at: Option<DateTime<Utc>>,
    // Rolling window of compression events, deduped by request_id (fallback
    // timestamp). The proxy keeps its own sliding window but loses it on
    // restart; persisting here lets the Activity feed carry history across
    // app restarts without reading the proxy's state. Cap kept modest —
    // users don't need a forever archive, just enough context to feel
    // continuous.
    #[serde(default)]
    transformation_history: VecDeque<TransformationFeedEvent>,
    // Projects we've already nudged with a "never trained" TrainSuggestion.
    // Fire-once per project, ever — once it's in the set we never re-emit the
    // never-trained kind for that project (even after the user trains and
    // re-resets state). Stale re-suggestions have their own throttle map.
    #[serde(default)]
    train_suggestions_fired: BTreeSet<String>,
    // Last time we emitted a "stale" TrainSuggestion per project. Used to
    // throttle re-fires to `STALE_TRAIN_REFIRE_DAYS` so a long-neglected
    // project doesn't churn a new Activity row on every observer tick.
    #[serde(default)]
    stale_train_suggestions_fired_at: std::collections::BTreeMap<String, DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
struct TurnAccumulator {
    tokens_saved: u64,
    seen_request_ids: BTreeSet<String>,
    call_count: u32,
    last_updated: DateTime<Utc>,
    model: Option<String>,
    workspace: Option<String>,
    record_emitted: bool,
}

// Cap the in-memory map and age-out old turns so a long-running app doesn't
// grow unbounded. 2 hours comfortably exceeds any realistic agent-loop turn
// length; 1024 caps the worst case of many short-lived turns.
const TURN_ACCUMULATOR_TTL_HOURS: i64 = 2;
const MAX_TURN_ACCUMULATORS: usize = 1024;

pub struct ActivityFacts {
    path: PathBuf,
    seen_models: BTreeSet<String>,
    all_time_record_tokens: u64,
    daily_record: Option<DailyRecordFact>,
    last_rtk_total_commands: u64,
    last_rtk_total_saved: u64,
    recent_events: VecDeque<ActivityEvent>,
    current_streak: u32,
    longest_streak: u32,
    last_active_day: Option<String>,
    last_weekly_recap_week_key: Option<String>,
    learnings_milestones_fired: BTreeSet<u32>,
    prompt_all_time_record_tokens: u64,
    // Timestamps of the last record-tag emission we actually made for each
    // scope (not just the last time the underlying counter updated). Used to
    // debounce near-identical beats: a burst of compressions that each nudge
    // the record up by a fraction of a percent would otherwise flood the
    // feed with chips that keep saying the same thing.
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    daily_record_emitted_at: Option<DateTime<Utc>>,
    transformation_history: VecDeque<TransformationFeedEvent>,
    train_suggestions_fired: BTreeSet<String>,
    stale_train_suggestions_fired_at: std::collections::BTreeMap<String, DateTime<Utc>>,
    turn_accumulators: HashMap<String, TurnAccumulator>,
    rtk_initialized: bool,
    dirty: bool,
}

impl ActivityFacts {
    pub fn load_or_create(base_dir: &Path) -> Result<Self> {
        let path = config_file(base_dir, "activity-facts.json");
        if !path.exists() {
            return Ok(Self::empty(path));
        }

        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let persisted = serde_json::from_slice::<PersistedActivityFacts>(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if persisted.schema_version != SCHEMA_VERSION {
            return Ok(Self::empty(path));
        }

        Ok(Self {
            path,
            seen_models: persisted.seen_models,
            all_time_record_tokens: persisted.all_time_record_tokens,
            daily_record: persisted.daily_record,
            last_rtk_total_commands: persisted.last_rtk_total_commands,
            last_rtk_total_saved: persisted.last_rtk_total_saved,
            recent_events: persisted.recent_events,
            current_streak: persisted.current_streak,
            longest_streak: persisted.longest_streak,
            last_active_day: persisted.last_active_day,
            last_weekly_recap_week_key: persisted.last_weekly_recap_week_key,
            learnings_milestones_fired: persisted.learnings_milestones_fired,
            prompt_all_time_record_tokens: persisted.prompt_all_time_record_tokens,
            all_time_record_emitted_at: persisted.all_time_record_emitted_at,
            daily_record_emitted_at: persisted.daily_record_emitted_at,
            transformation_history: persisted.transformation_history,
            train_suggestions_fired: persisted.train_suggestions_fired,
            stale_train_suggestions_fired_at: persisted.stale_train_suggestions_fired_at,
            turn_accumulators: HashMap::new(),
            rtk_initialized: true,
            dirty: false,
        })
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            seen_models: BTreeSet::new(),
            all_time_record_tokens: 0,
            daily_record: None,
            last_rtk_total_commands: 0,
            last_rtk_total_saved: 0,
            recent_events: VecDeque::new(),
            current_streak: 0,
            longest_streak: 0,
            last_active_day: None,
            last_weekly_recap_week_key: None,
            learnings_milestones_fired: BTreeSet::new(),
            prompt_all_time_record_tokens: 0,
            all_time_record_emitted_at: None,
            daily_record_emitted_at: None,
            transformation_history: VecDeque::new(),
            train_suggestions_fired: BTreeSet::new(),
            stale_train_suggestions_fired_at: std::collections::BTreeMap::new(),
            turn_accumulators: HashMap::new(),
            rtk_initialized: false,
            dirty: false,
        }
    }

    pub fn recent_events(&self) -> Vec<ActivityEvent> {
        // Defense against any residual duplication: if multiple Record events
        // tagged Daily share the same day, keep only the most recent one
        // (highest observed_at). Walk newest-first, remember days we've
        // already emitted a Daily-tagged Record for, and drop subsequent
        // ones. Turn-only Records don't carry a day tag and aren't filtered.
        let mut seen_dr_days: BTreeSet<String> = BTreeSet::new();
        let mut kept_rev: Vec<ActivityEvent> = Vec::with_capacity(self.recent_events.len());
        for event in self.recent_events.iter().rev() {
            if let ActivityEvent::Record(rec) = event {
                if rec.tags.contains(&RecordTag::Daily) {
                    let day = rec
                        .day
                        .clone()
                        .unwrap_or_else(|| rec.observed_at.format("%Y-%m-%d").to_string());
                    if !seen_dr_days.insert(day) {
                        continue;
                    }
                }
            }
            kept_rev.push(event.clone());
        }
        kept_rev.reverse();
        kept_rev
    }

    /// Persisted compression history, oldest-first. Callers typically merge
    /// this with the proxy's live feed and dedup by request_id.
    pub fn transformation_history(&self) -> Vec<TransformationFeedEvent> {
        self.transformation_history.iter().cloned().collect()
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

        // Persist compression history so it survives across app restarts.
        // The proxy's /transformations/feed is an in-memory sliding window
        // and returns nothing on a cold start; without this the Activity
        // feed would show an empty slate every time the proxy restarts.
        if push_transformation_history_unique(
            &mut self.transformation_history,
            event,
            TRANSFORMATION_HISTORY_CAP,
        ) {
            self.dirty = true;
        }

        if let Some(model) = event.model.as_ref() {
            if !model.is_empty() && self.seen_models.insert(model.clone()) {
                emitted.push(ActivityEvent::NewModel(NewModelEvent {
                    observed_at,
                    model: model.clone(),
                    provider: event.provider.clone(),
                    workspace: event.workspace.clone(),
                }));
            }
        }

        let tokens_saved = event
            .tokens_saved
            .and_then(|n| if n > 0 { Some(n as u64) } else { None });

        if let Some(tokens) = tokens_saved {
            let today = now.format("%Y-%m-%d").to_string();
            let event_day = observed_at.format("%Y-%m-%d").to_string();
            let mut tags: Vec<RecordTag> = Vec::new();
            let mut all_time_previous: Option<u64> = None;

            // Only track + celebrate a Daily record for events that happened
            // today. The proxy's feed is a sliding window that re-returns
            // historical transformations on every poll. Without this guard,
            // each day boundary in the feed oscillates `daily_record` and
            // emits a fresh (duplicate) Daily-tagged Record on every poll —
            // which stacks up in `recent_events` and shows the user the same
            // record N times.
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
                    if !debounce_suppress(previous_same_day, self.daily_record_emitted_at, tokens, now) {
                        self.daily_record_emitted_at = Some(now);
                        tags.push(RecordTag::Daily);
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
                if !debounce_suppress(previous, self.all_time_record_emitted_at, tokens, now) {
                    self.all_time_record_emitted_at = Some(now);
                    tags.push(RecordTag::AllTime);
                    all_time_previous = previous;
                }
            }

            if !tags.is_empty() {
                let day = if tags.contains(&RecordTag::Daily) {
                    Some(today)
                } else {
                    None
                };
                emitted.push(ActivityEvent::Record(RecordEvent {
                    observed_at,
                    tags,
                    tokens_saved: tokens,
                    savings_percent: event.savings_percent,
                    model: event.model.clone(),
                    provider: event.provider.clone(),
                    request_id: event.request_id.clone(),
                    previous_record: all_time_previous,
                    day,
                    workspace: event.workspace.clone(),
                    turn_id: None,
                    call_count: None,
                }));
            }
        }

        if let Some(prompt_record) = self.process_turn(event, observed_at) {
            emitted.push(prompt_record);
        }

        let streak_events = self.process_streak(observed_at);
        if !streak_events.is_empty() {
            emitted.extend(streak_events.clone());
            self.push_recent(streak_events);
            self.dirty = true;
        }

        if !emitted.is_empty() {
            // Daily/all-time/new-model events are already in `emitted` but not
            // yet in recent_events (streak events are already pushed above).
            let non_streak: Vec<ActivityEvent> = emitted
                .iter()
                .filter(|e| !matches!(e, ActivityEvent::Streak(_)))
                .cloned()
                .collect();
            self.push_recent(non_streak);
            self.dirty = true;
        }
        emitted
    }

    fn process_turn(
        &mut self,
        event: &TransformationFeedEvent,
        observed_at: DateTime<Utc>,
    ) -> Option<ActivityEvent> {
        // Emits an all-time record at the *prompt* level — summed tokens saved
        // across every agent-loop API call from one user prompt, grouped by
        // the proxy-emitted turn_id. Returns None if the feed event lacks a
        // turn_id (older proxy) or request_id (can't dedupe across the feed's
        // sliding window, which re-surfaces the same event on every poll).
        let turn_id = event.turn_id.as_ref()?;
        let request_id = event.request_id.as_ref()?;
        let tokens_saved =
            event
                .tokens_saved
                .and_then(|n| if n > 0 { Some(n as u64) } else { None })?;

        let previous_record = self.prompt_all_time_record_tokens;

        let acc = self.turn_accumulators.entry(turn_id.clone()).or_default();

        if !acc.seen_request_ids.insert(request_id.clone()) {
            // Same transformation re-observed on a later feed poll — already counted.
            return None;
        }

        acc.tokens_saved = acc.tokens_saved.saturating_add(tokens_saved);
        acc.call_count = acc.call_count.saturating_add(1);
        acc.last_updated = observed_at;
        if acc.model.is_none() {
            acc.model = event.model.clone();
        }
        if acc.workspace.is_none() {
            acc.workspace = event.workspace.clone();
        }

        let beats_record = acc.tokens_saved > self.prompt_all_time_record_tokens;
        let should_emit = beats_record && !acc.record_emitted;
        // Persist the new high-water mark even on subsequent transformations
        // of the same turn so restarts compare against the correct value.
        if beats_record {
            self.prompt_all_time_record_tokens = acc.tokens_saved;
            self.dirty = true;
        }
        let emitted = if should_emit {
            acc.record_emitted = true;
            Some(ActivityEvent::Record(RecordEvent {
                observed_at,
                tags: vec![RecordTag::Turn],
                tokens_saved: acc.tokens_saved,
                savings_percent: None,
                model: acc.model.clone(),
                provider: None,
                request_id: None,
                previous_record: if previous_record == 0 {
                    None
                } else {
                    Some(previous_record)
                },
                day: None,
                workspace: acc.workspace.clone(),
                turn_id: Some(turn_id.clone()),
                call_count: Some(acc.call_count),
            }))
        } else {
            None
        };

        self.prune_turn_accumulators(observed_at);
        emitted
    }

    fn prune_turn_accumulators(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::hours(TURN_ACCUMULATOR_TTL_HOURS);
        self.turn_accumulators
            .retain(|_, acc| acc.last_updated >= cutoff);
        if self.turn_accumulators.len() > MAX_TURN_ACCUMULATORS {
            // Drop the oldest entries by last_updated until within cap.
            let mut by_age: Vec<(String, DateTime<Utc>)> = self
                .turn_accumulators
                .iter()
                .map(|(k, v)| (k.clone(), v.last_updated))
                .collect();
            by_age.sort_by_key(|(_, ts)| *ts);
            let excess = self.turn_accumulators.len() - MAX_TURN_ACCUMULATORS;
            for (k, _) in by_age.into_iter().take(excess) {
                self.turn_accumulators.remove(&k);
            }
        }
    }

    fn process_streak(&mut self, observed_at: DateTime<Utc>) -> Vec<ActivityEvent> {
        let today = observed_at.date_naive();
        let today_key = today.format("%Y-%m-%d").to_string();
        let prev_day = self
            .last_active_day
            .as_deref()
            .and_then(|key| NaiveDate::parse_from_str(key, "%Y-%m-%d").ok());

        if let Some(prev) = prev_day {
            if today <= prev {
                return Vec::new();
            }
        }

        let previous_streak = self.current_streak;
        let previous_longest = self.longest_streak;

        let new_streak = match prev_day {
            Some(prev) if today == prev.succ_opt().unwrap_or(prev) => previous_streak + 1,
            Some(_) => 1,
            None => 1,
        };

        self.current_streak = new_streak;
        self.last_active_day = Some(today_key);
        if new_streak > self.longest_streak {
            self.longest_streak = new_streak;
        }

        let mut events: Vec<ActivityEvent> = streaks_crossed(previous_streak, new_streak)
            .into_iter()
            .map(|days| {
                ActivityEvent::Streak(StreakEvent {
                    observed_at,
                    days,
                    kind: "threshold".into(),
                })
            })
            .collect();

        if new_streak > previous_longest && previous_longest > 0 {
            events.push(ActivityEvent::Streak(StreakEvent {
                observed_at,
                days: new_streak,
                kind: "new_record".into(),
            }));
        }

        events
    }

    pub fn observe_rtk(
        &mut self,
        summary: &RtkGainSummary,
        observed_at: DateTime<Utc>,
    ) -> Option<ActivityEvent> {
        if !self.rtk_initialized {
            self.last_rtk_total_commands = summary.total_commands;
            self.last_rtk_total_saved = summary.total_saved;
            self.rtk_initialized = true;
            self.dirty = true;
            return None;
        }

        if summary.total_commands < self.last_rtk_total_commands
            || summary.total_saved < self.last_rtk_total_saved
        {
            self.last_rtk_total_commands = summary.total_commands;
            self.last_rtk_total_saved = summary.total_saved;
            self.dirty = true;
            return None;
        }

        if summary.total_commands == self.last_rtk_total_commands {
            return None;
        }

        let commands_delta = summary
            .total_commands
            .saturating_sub(self.last_rtk_total_commands);
        let tokens_saved_delta = summary
            .total_saved
            .saturating_sub(self.last_rtk_total_saved);
        let event = ActivityEvent::RtkBatch(RtkBatchEvent {
            observed_at,
            commands_delta,
            tokens_saved_delta,
            total_commands: summary.total_commands,
            total_saved: summary.total_saved,
        });

        self.last_rtk_total_commands = summary.total_commands;
        self.last_rtk_total_saved = summary.total_saved;
        self.push_recent(vec![event.clone()]);
        self.dirty = true;
        Some(event)
    }

    pub fn record_milestones(
        &mut self,
        milestones: &[u64],
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        if milestones.is_empty() {
            return Vec::new();
        }
        let events: Vec<ActivityEvent> = milestones
            .iter()
            .map(|tokens| {
                ActivityEvent::Milestone(MilestoneEvent {
                    observed_at,
                    milestone_tokens_saved: *tokens,
                    kind: milestone_kind(*tokens).to_string(),
                })
            })
            .collect();
        self.push_recent(events.clone());
        self.dirty = true;
        events
    }

    pub fn record_savings_milestones(
        &mut self,
        milestones: &[u64],
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        if milestones.is_empty() {
            return Vec::new();
        }
        let events: Vec<ActivityEvent> = milestones
            .iter()
            .map(|usd| {
                ActivityEvent::SavingsMilestone(SavingsMilestoneEvent {
                    observed_at,
                    milestone_usd: *usd,
                    kind: savings_milestone_kind(*usd).to_string(),
                })
            })
            .collect();
        self.push_recent(events.clone());
        self.dirty = true;
        events
    }

    pub fn observe_learnings_count(
        &mut self,
        count: usize,
        observed_at: DateTime<Utc>,
    ) -> Option<ActivityEvent> {
        const THRESHOLD: u32 = 3;
        if count < THRESHOLD as usize {
            return None;
        }
        if self.learnings_milestones_fired.contains(&THRESHOLD) {
            return None;
        }
        self.learnings_milestones_fired.insert(THRESHOLD);
        let event = ActivityEvent::LearningsMilestone(LearningsMilestoneEvent {
            observed_at,
            count: THRESHOLD,
            kind: "first_3".into(),
        });
        self.push_recent(vec![event.clone()]);
        self.dirty = true;
        Some(event)
    }

    /// Scan project metadata and emit a `TrainSuggestion` for any project that
    /// matches a trigger. Two kinds:
    ///
    /// - `"never_trained"` — user has logged `NEVER_TRAINED_MIN_SESSIONS`+
    ///   sessions but never run Train on this project. Fires once per project,
    ///   ever (gated by `train_suggestions_fired`). The caller dispatches a
    ///   macOS notification for this kind via
    ///   `notifications::notification_for_event`.
    /// - `"stale"` — user has trained before but worked on the project 2+
    ///   active days since. Throttled to at most once per
    ///   `STALE_TRAIN_REFIRE_DAYS` per project via
    ///   `stale_train_suggestions_fired_at` so the Activity feed doesn't turn
    ///   into a nag screen. No notification for this kind — the existing
    ///   inline "consider rerunning" hint on the Optimize tab covers the
    ///   signal, this just surfaces it on the Activity tab too.
    pub fn observe_train_suggestions(
        &mut self,
        projects: &[ClaudeCodeProject],
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        let mut events: Vec<ActivityEvent> = Vec::new();
        for project in projects {
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

            events.push(ActivityEvent::TrainSuggestion(TrainSuggestionEvent {
                observed_at,
                project_path: project.project_path.clone(),
                project_display_name: project.display_name.clone(),
                session_count: project.session_count as u32,
                active_days_since_last_learn: active_days,
                kind: kind.into(),
            }));

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
        }

        if !events.is_empty() {
            self.push_recent(events.clone());
            self.dirty = true;
        }
        events
    }

    pub fn maybe_record_weekly_recap(
        &mut self,
        today_local: NaiveDate,
        totals: WeeklyTotals,
        observed_at: DateTime<Utc>,
    ) -> Option<ActivityEvent> {
        if today_local.weekday() != chrono::Weekday::Mon {
            return None;
        }
        let week_key = today_local.format("%Y-%m-%d").to_string();
        if self.last_weekly_recap_week_key.as_deref() == Some(week_key.as_str()) {
            return None;
        }
        if totals.active_days == 0 {
            return None;
        }
        let week_start = today_local
            .pred_opt()
            .and_then(|d| d.checked_sub_days(chrono::Days::new(6)))
            .unwrap_or(today_local);
        let week_end = today_local.pred_opt().unwrap_or(today_local);
        let event = ActivityEvent::WeeklyRecap(WeeklyRecapEvent {
            observed_at,
            week_start: week_start.format("%Y-%m-%d").to_string(),
            week_end: week_end.format("%Y-%m-%d").to_string(),
            total_tokens_saved: totals.total_tokens_saved,
            total_savings_usd: totals.total_savings_usd,
            active_days: totals.active_days,
        });
        self.last_weekly_recap_week_key = Some(week_key);
        self.push_recent(vec![event.clone()]);
        self.dirty = true;
        Some(event)
    }

    fn push_recent(&mut self, events: Vec<ActivityEvent>) {
        for event in events {
            self.recent_events.push_back(event);
        }
        while self.recent_events.len() > RECENT_EVENTS_CAP {
            // Prefer evicting the oldest non-high-signal event so a burst
            // of compressions can't push out a learnings milestone or a
            // new daily record. Fall back to pop_front only when the entire
            // buffer is high-signal (rare — these kinds are sparse).
            if let Some(idx) = self
                .recent_events
                .iter()
                .position(|event| !is_high_signal(event))
            {
                self.recent_events.remove(idx);
            } else {
                self.recent_events.pop_front();
            }
        }
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let persisted = PersistedActivityFacts {
            schema_version: SCHEMA_VERSION,
            seen_models: self.seen_models.clone(),
            all_time_record_tokens: self.all_time_record_tokens,
            daily_record: self.daily_record.clone(),
            last_rtk_total_commands: self.last_rtk_total_commands,
            last_rtk_total_saved: self.last_rtk_total_saved,
            recent_events: self.recent_events.clone(),
            current_streak: self.current_streak,
            longest_streak: self.longest_streak,
            last_active_day: self.last_active_day.clone(),
            last_weekly_recap_week_key: self.last_weekly_recap_week_key.clone(),
            learnings_milestones_fired: self.learnings_milestones_fired.clone(),
            prompt_all_time_record_tokens: self.prompt_all_time_record_tokens,
            all_time_record_emitted_at: self.all_time_record_emitted_at,
            daily_record_emitted_at: self.daily_record_emitted_at,
            transformation_history: self.transformation_history.clone(),
            train_suggestions_fired: self.train_suggestions_fired.clone(),
            stale_train_suggestions_fired_at: self.stale_train_suggestions_fired_at.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&persisted).context("serializing activity facts")?;
        std::fs::write(&self.path, bytes)
            .with_context(|| format!("writing {}", self.path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

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

    #[test]
    fn new_model_emits_once_per_model() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let events = facts.observe_transformation(
            &mk_transformation(Some("claude-opus-4-7"), Some(500), Some(50.0)),
            at(10, 0),
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, ActivityEvent::NewModel(_))));
        let events2 = facts.observe_transformation(
            &mk_transformation(Some("claude-opus-4-7"), Some(400), Some(40.0)),
            at(10, 1),
        );
        assert!(!events2
            .iter()
            .any(|e| matches!(e, ActivityEvent::NewModel(_))));
    }

    fn is_daily_record(e: &ActivityEvent) -> bool {
        matches!(e, ActivityEvent::Record(r) if r.tags.contains(&RecordTag::Daily))
    }

    fn is_all_time_record(e: &ActivityEvent) -> bool {
        matches!(e, ActivityEvent::Record(r) if r.tags.contains(&RecordTag::AllTime))
    }

    fn is_turn_record(e: &ActivityEvent) -> bool {
        matches!(e, ActivityEvent::Record(r) if r.tags.contains(&RecordTag::Turn))
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
    fn recent_events_dedupes_daily_records_by_day() {
        // Belt-and-suspenders: even if something injects multiple DailyRecord
        // events for the same day into recent_events, the API surface only
        // shows one row per day (newest observed_at wins).
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let obs_early = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        let obs_late = Utc.with_ymd_and_hms(2026, 4, 22, 15, 0, 0).unwrap();
        let mk = |obs: DateTime<Utc>, tokens: u64| RecordEvent {
            observed_at: obs,
            tags: vec![RecordTag::Daily],
            tokens_saved: tokens,
            savings_percent: Some(10.0),
            model: Some("a".into()),
            provider: Some("anthropic".into()),
            request_id: Some("r".into()),
            previous_record: None,
            day: Some("2026-04-22".into()),
            workspace: None,
            turn_id: None,
            call_count: None,
        };
        facts
            .recent_events
            .push_back(ActivityEvent::Record(mk(obs_early, 100)));
        facts
            .recent_events
            .push_back(ActivityEvent::Record(mk(obs_late, 200)));

        let visible = facts.recent_events();
        let drs: Vec<_> = visible
            .iter()
            .filter_map(|e| match e {
                ActivityEvent::Record(r) if r.tags.contains(&RecordTag::Daily) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(drs.len(), 1);
        assert_eq!(drs[0].observed_at, obs_late, "newest entry wins");
    }

    #[test]
    fn push_recent_preserves_high_signal_events_under_burst() {
        // A long coding session emits hundreds of compression events. Without
        // kind-aware eviction, a rare learnings milestone pushed early would
        // be FIFO'd out before the frontend ever saw it. Confirm it survives.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let milestone_at = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        facts.push_recent(vec![ActivityEvent::LearningsMilestone(
            LearningsMilestoneEvent {
                observed_at: milestone_at,
                count: 100,
                kind: "first_100".into(),
            },
        )]);

        // Flood past the cap with transformations.
        let flood: Vec<ActivityEvent> = (0..RECENT_EVENTS_CAP + 50)
            .map(|i| {
                ActivityEvent::Transformation(TransformationFeedEvent {
                    request_id: Some(format!("req-{i}")),
                    timestamp: Some("2026-04-22T10:00:00Z".into()),
                    provider: Some("anthropic".into()),
                    model: Some("claude".into()),
                    input_tokens_original: Some(1000),
                    input_tokens_optimized: Some(300),
                    tokens_saved: Some(500),
                    savings_percent: Some(50.0),
                    transforms_applied: vec!["kompress".into()],
                    workspace: None,
                    turn_id: None,
                })
            })
            .collect();
        facts.push_recent(flood);

        assert!(facts.recent_events.len() <= RECENT_EVENTS_CAP);
        let milestone_count = facts
            .recent_events
            .iter()
            .filter(|e| matches!(e, ActivityEvent::LearningsMilestone(_)))
            .count();
        assert_eq!(
            milestone_count, 1,
            "learnings milestone must survive a compression burst"
        );
    }

    #[test]
    fn transformation_history_persists_across_observations_and_dedups() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let t = at(10, 0);

        let mut tx_a = mk_transformation(Some("m"), Some(500), Some(50.0));
        tx_a.request_id = Some("req-A".into());
        tx_a.timestamp = Some("2026-04-22T10:00:00Z".into());
        let mut tx_b = mk_transformation(Some("m"), Some(600), Some(55.0));
        tx_b.request_id = Some("req-B".into());
        tx_b.timestamp = Some("2026-04-22T10:01:00Z".into());

        facts.observe_transformation_at(&tx_a, t, t);
        facts.observe_transformation_at(&tx_b, t, t);
        // Re-observing the same events (what the proxy's sliding window
        // returns every poll) must not duplicate them in history.
        facts.observe_transformation_at(&tx_a, t, t);
        facts.observe_transformation_at(&tx_b, t, t);

        let history = facts.transformation_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].request_id.as_deref(), Some("req-A"));
        assert_eq!(history[1].request_id.as_deref(), Some("req-B"));
    }

    #[test]
    fn push_transformation_history_unique_enforces_cap_by_evicting_oldest() {
        use std::collections::VecDeque;
        let mut history: VecDeque<TransformationFeedEvent> = VecDeque::new();
        let make = |n: u64| {
            let mut ev = mk_transformation(Some("m"), Some(n as i64), Some(10.0));
            ev.request_id = Some(format!("req-{n}"));
            ev
        };
        for n in 0..5 {
            assert!(push_transformation_history_unique(
                &mut history,
                &make(n),
                3
            ));
        }
        assert_eq!(history.len(), 3);
        assert_eq!(
            history.front().unwrap().request_id.as_deref(),
            Some("req-2")
        );
        assert_eq!(history.back().unwrap().request_id.as_deref(), Some("req-4"));
    }

    #[test]
    fn push_transformation_history_unique_skips_fingerprint_less_events() {
        // Events with neither request_id nor timestamp can't be deduped, so
        // we don't persist them — the proxy keeps returning them live.
        use std::collections::VecDeque;
        let mut history: VecDeque<TransformationFeedEvent> = VecDeque::new();
        let mut ev = mk_transformation(Some("m"), Some(100), Some(10.0));
        ev.request_id = None;
        ev.timestamp = None;
        assert!(!push_transformation_history_unique(&mut history, &ev, 10));
        assert!(history.is_empty());
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

    fn mk_turn(turn_id: &str, request_id: &str, tokens_saved: i64) -> TransformationFeedEvent {
        let mut e = mk_transformation(Some("a"), Some(tokens_saved), Some(10.0));
        e.turn_id = Some(turn_id.into());
        e.request_id = Some(request_id.into());
        e
    }

    #[test]
    fn prompt_all_time_record_sums_across_turn_and_emits_once() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        // First turn beats the implicit zero record, so it emits with
        // previous_record=None.
        let first = facts.observe_transformation(&mk_turn("turn-A", "req-1", 500), at(10, 0));
        let first_rec = first
            .iter()
            .find_map(|e| match e {
                ActivityEvent::Record(r) if r.tags.contains(&RecordTag::Turn) => Some(r),
                _ => None,
            })
            .expect("initial record event");
        assert_eq!(first_rec.tokens_saved, 500);
        assert_eq!(first_rec.call_count, Some(1));
        assert_eq!(first_rec.previous_record, None);
        assert_eq!(first_rec.turn_id.as_deref(), Some("turn-A"));

        // Start a second turn. First call: 300 (under record of 500). Second
        // call same turn: +400 = 700 total, beats the 500 record.
        let second = facts.observe_transformation(&mk_turn("turn-B", "req-2", 300), at(10, 1));
        assert!(!second.iter().any(is_turn_record));
        let third = facts.observe_transformation(&mk_turn("turn-B", "req-3", 400), at(10, 2));
        let rec = third
            .iter()
            .find_map(|e| match e {
                ActivityEvent::Record(r) if r.tags.contains(&RecordTag::Turn) => Some(r),
                _ => None,
            })
            .expect("prompt record event");
        assert_eq!(rec.tokens_saved, 700);
        assert_eq!(rec.call_count, Some(2));
        assert_eq!(rec.previous_record, Some(500));
        assert_eq!(rec.turn_id.as_deref(), Some("turn-B"));

        // Further growth within the same turn must NOT re-emit.
        let fourth = facts.observe_transformation(&mk_turn("turn-B", "req-4", 100), at(10, 3));
        assert!(!fourth.iter().any(is_turn_record));
        // But the persisted record should reflect the new total.
        assert_eq!(facts.prompt_all_time_record_tokens, 800);
    }

    #[test]
    fn prompt_record_dedupes_replayed_feed_events() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        facts.observe_transformation(&mk_turn("t1", "r1", 400), at(10, 0));
        // Same request_id observed again on a later poll — must not double-count.
        facts.observe_transformation(&mk_turn("t1", "r1", 400), at(10, 1));
        // Now one genuinely new call in the same turn.
        facts.observe_transformation(&mk_turn("t1", "r2", 100), at(10, 2));

        assert_eq!(facts.prompt_all_time_record_tokens, 500);
        assert_eq!(facts.turn_accumulators.get("t1").unwrap().call_count, 2);
    }

    #[test]
    fn prompt_record_skipped_when_turn_id_absent() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // Older proxy: turn_id None.
        let events = facts.observe_transformation(
            &mk_transformation(Some("a"), Some(5000), Some(50.0)),
            at(10, 0),
        );
        assert!(!events.iter().any(is_turn_record));
        assert_eq!(facts.prompt_all_time_record_tokens, 0);
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
    fn rtk_first_observation_is_silent_and_growth_emits() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        assert!(facts
            .observe_rtk(
                &RtkGainSummary {
                    total_commands: 10,
                    total_saved: 5_000,
                    avg_savings_pct: 50.0,
                },
                at(10, 0)
            )
            .is_none());
        let ev = facts
            .observe_rtk(
                &RtkGainSummary {
                    total_commands: 12,
                    total_saved: 6_500,
                    avg_savings_pct: 52.0,
                },
                at(10, 1),
            )
            .expect("rtk batch");
        match ev {
            ActivityEvent::RtkBatch(b) => {
                assert_eq!(b.commands_delta, 2);
                assert_eq!(b.tokens_saved_delta, 1_500);
            }
            _ => panic!("wrong event kind"),
        }
    }

    #[test]
    fn rtk_shrink_resets_silently() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_rtk(
            &RtkGainSummary {
                total_commands: 100,
                total_saved: 50_000,
                avg_savings_pct: 50.0,
            },
            at(10, 0),
        );
        assert!(facts
            .observe_rtk(
                &RtkGainSummary {
                    total_commands: 10,
                    total_saved: 5_000,
                    avg_savings_pct: 50.0,
                },
                at(10, 1)
            )
            .is_none());
        // Subsequent growth above the new baseline emits.
        let ev = facts.observe_rtk(
            &RtkGainSummary {
                total_commands: 12,
                total_saved: 6_000,
                avg_savings_pct: 50.0,
            },
            at(10, 2),
        );
        assert!(matches!(ev, Some(ActivityEvent::RtkBatch(_))));
    }

    #[test]
    fn record_milestones_appends_events() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let events = facts.record_milestones(&[1_000_000, 5_000_000], at(10, 0));
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ActivityEvent::Milestone(_)));
        let empty = facts.record_milestones(&[], at(10, 1));
        assert!(empty.is_empty());
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

    fn day_at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn streak_advances_on_consecutive_days() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), Some(10.0)),
            day_at(2026, 4, 20, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), Some(10.0)),
            day_at(2026, 4, 21, 10),
        );
        let events = facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), Some(10.0)),
            day_at(2026, 4, 22, 10),
        );
        assert_eq!(facts.current_streak, 3);
        assert!(events.iter().any(
            |e| matches!(e, ActivityEvent::Streak(s) if s.days == 3 && s.kind == "threshold")
        ));
    }

    #[test]
    fn streak_resets_on_gap() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 20, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 21, 10),
        );
        // Skip 22nd, next activity on 23rd.
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 23, 10),
        );
        assert_eq!(facts.current_streak, 1);
        assert_eq!(facts.longest_streak, 2);
    }

    #[test]
    fn streak_is_idempotent_on_same_day_replay() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 22, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(200), None),
            day_at(2026, 4, 22, 11),
        );
        assert_eq!(facts.current_streak, 1);
    }

    #[test]
    fn streak_noop_on_historical_replay() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 22, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 21, 10),
        );
        assert_eq!(facts.current_streak, 1);
        assert_eq!(facts.last_active_day.as_deref(), Some("2026-04-22"));
    }

    #[test]
    fn streak_new_record_emits_when_surpassing_longest() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // Build a streak of 2, break, then streak of 3.
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 1, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 2, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 10, 10),
        );
        facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 11, 10),
        );
        let events = facts.observe_transformation(
            &mk_transformation(Some("m"), Some(100), None),
            day_at(2026, 4, 12, 10),
        );
        assert_eq!(facts.current_streak, 3);
        assert_eq!(facts.longest_streak, 3);
        assert!(events.iter().any(
            |e| matches!(e, ActivityEvent::Streak(s) if s.kind == "new_record" && s.days == 3)
        ));
    }

    #[test]
    fn streaks_crossed_emits_once_per_threshold() {
        assert_eq!(streaks_crossed(0, 2), Vec::<u32>::new());
        assert_eq!(streaks_crossed(2, 3), vec![3]);
        assert_eq!(streaks_crossed(3, 7), vec![7]);
        assert_eq!(streaks_crossed(0, 30), vec![3, 7, 14, 30]);
        assert_eq!(streaks_crossed(30, 60), vec![60]);
        assert_eq!(streaks_crossed(60, 120), vec![90, 120]);
    }

    #[test]
    fn savings_milestone_kind_labels_first_and_repeating_thresholds() {
        assert_eq!(savings_milestone_kind(10), "first_10");
        assert_eq!(savings_milestone_kind(50), "first_50");
        assert_eq!(savings_milestone_kind(100), "first_100");
        assert_eq!(savings_milestone_kind(200), "repeating_100");
    }

    #[test]
    fn weekly_recap_emits_only_on_monday() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let tuesday = NaiveDate::from_ymd_opt(2026, 4, 21).unwrap();
        let out = facts.maybe_record_weekly_recap(
            tuesday,
            WeeklyTotals {
                total_tokens_saved: 100,
                total_savings_usd: 1.0,
                active_days: 3,
            },
            Utc::now(),
        );
        assert!(out.is_none());
    }

    #[test]
    fn weekly_recap_emits_once_per_week() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let first = facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 500,
                total_savings_usd: 2.5,
                active_days: 4,
            },
            Utc::now(),
        );
        assert!(first.is_some());
        let second = facts.maybe_record_weekly_recap(
            monday,
            WeeklyTotals {
                total_tokens_saved: 999,
                total_savings_usd: 5.0,
                active_days: 7,
            },
            Utc::now(),
        );
        assert!(second.is_none());
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
    fn workspace_threads_through_to_new_model_and_record_events() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let mut transformation = mk_transformation(Some("claude-x"), Some(1_000), Some(50.0));
        transformation.workspace = Some("/Users/u/Code/demo-repo".into());
        let events = facts.observe_transformation_at(&transformation, at(10, 0), at(10, 0));
        let new_model = events
            .iter()
            .find_map(|e| match e {
                ActivityEvent::NewModel(m) => Some(m),
                _ => None,
            })
            .expect("new model");
        assert_eq!(
            new_model.workspace.as_deref(),
            Some("/Users/u/Code/demo-repo")
        );
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

    #[test]
    fn milestone_kind_includes_first_100k() {
        assert_eq!(milestone_kind(100_000), "first_100k");
        assert_eq!(milestone_kind(1_000_000), "first_1m");
        assert_eq!(milestone_kind(20_000_000), "repeating_10m");
    }

    #[test]
    fn learnings_milestone_fires_once_at_three() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        assert!(facts.observe_learnings_count(2, at(10, 0)).is_none());
        let event = facts
            .observe_learnings_count(3, at(10, 1))
            .expect("should fire at 3");
        match event {
            ActivityEvent::LearningsMilestone(e) => {
                assert_eq!(e.count, 3);
                assert_eq!(e.kind, "first_3");
            }
            _ => panic!("expected learnings milestone"),
        }
        assert!(facts.observe_learnings_count(5, at(10, 2)).is_none());
    }

    #[test]
    fn learnings_milestone_idempotent_across_reload() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        facts.observe_learnings_count(10, at(10, 0));
        facts.save_if_dirty().unwrap();

        let mut reloaded = ActivityFacts::load_or_create(&base).unwrap();
        assert!(reloaded.observe_learnings_count(20, at(11, 0)).is_none());
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
    ) -> ClaudeCodeProject {
        ClaudeCodeProject {
            id: path.chars().take(12).collect(),
            project_path: path.into(),
            display_name: path.rsplit('/').next().unwrap_or(path).into(),
            last_worked_at: "2026-04-22T10:00:00Z".into(),
            session_count: sessions,
            last_learn_ran_at: last_learn.map(str::to_string),
            has_persisted_learnings: last_learn.is_some(),
            active_days_since_last_learn: active_days,
            last_learn_pattern_count: None,
        }
    }

    #[test]
    fn train_suggestion_never_trained_fires_once_over_threshold() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project("/Users/u/Code/demo", 5, None, 0)];
        let first = facts.observe_train_suggestions(&projects, at(10, 0));
        assert_eq!(first.len(), 1);
        match &first[0] {
            ActivityEvent::TrainSuggestion(e) => {
                assert_eq!(e.kind, "never_trained");
                assert_eq!(e.project_path, "/Users/u/Code/demo");
                assert_eq!(e.session_count, 5);
            }
            _ => panic!("expected TrainSuggestion"),
        }
        let second = facts.observe_train_suggestions(&projects, at(11, 0));
        assert!(second.is_empty(), "never-trained must fire once per project");
    }

    #[test]
    fn train_suggestion_never_trained_below_threshold_silent() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project("/Users/u/Code/demo", 4, None, 0)];
        assert!(facts
            .observe_train_suggestions(&projects, at(10, 0))
            .is_empty());
    }

    #[test]
    fn train_suggestion_stale_throttled_to_weekly() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project(
            "/Users/u/Code/demo",
            10,
            Some("2026-04-15T10:00:00Z"),
            3,
        )];
        let day0 = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();
        let first = facts.observe_train_suggestions(&projects, day0);
        assert_eq!(first.len(), 1);
        match &first[0] {
            ActivityEvent::TrainSuggestion(e) => assert_eq!(e.kind, "stale"),
            _ => panic!("expected stale TrainSuggestion"),
        }
        let day3 = day0 + Duration::days(3);
        assert!(
            facts.observe_train_suggestions(&projects, day3).is_empty(),
            "within 7-day cooldown must not re-fire"
        );
        let day8 = day0 + Duration::days(8);
        let third = facts.observe_train_suggestions(&projects, day8);
        assert_eq!(third.len(), 1, "after cooldown, stale must re-fire");
    }

    #[test]
    fn train_suggestion_persists_across_reload() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project("/Users/u/Code/demo", 5, None, 0)];
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

    #[test]
    fn train_suggestion_survives_low_signal_eviction() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project("/Users/u/Code/demo", 5, None, 0)];
        facts.observe_train_suggestions(&projects, at(10, 0));
        for _ in 0..(RECENT_EVENTS_CAP + 10) {
            facts.push_recent(vec![ActivityEvent::Transformation(mk_transformation(
                Some("claude-x"),
                Some(1),
                Some(1.0),
            ))]);
        }
        let recent = facts.recent_events();
        assert!(
            recent
                .iter()
                .any(|e| matches!(e, ActivityEvent::TrainSuggestion(_))),
            "TrainSuggestion must survive FIFO eviction"
        );
    }
}
