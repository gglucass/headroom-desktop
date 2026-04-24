use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    ActivityEvent, ActivityFeedSnapshot, ClaudeCodeProject, LearningsMilestoneEvent, RecordEvent,
    RecordTag, RtkBatchEvent, TransformationFeedEvent, TrainSuggestionEvent, WeeklyRecapEvent,
};
use crate::storage::config_file;
use crate::tool_manager::RtkGainSummary;

// Bumped from 3 → 4 when the transformation tile moved from a proxy-fetch on
// every poll into a persisted `last_transformation` slot, same as the other
// tiles. Old persisted state is missing the field; the mismatch handler drops
// the file and starts fresh.
const SCHEMA_VERSION: u8 = 4;
// Hard cap on how big a facts file we'll even attempt to deserialize at boot.
// The pre-v2 schema embedded full request/response bodies into queues that
// could grow past 100MB; loading those synchronously hangs the boot path and
// then the IPC hot path on every save. Anything bigger than this is treated
// as a schema mismatch and reset.
const MAX_FACTS_FILE_BYTES: u64 = 2 * 1024 * 1024;

// Minimum Claude Code session count before we nudge a never-trained project.
// Below this, the user probably hasn't done enough real work on the project
// for train to find meaningful patterns.
pub(crate) const NEVER_TRAINED_MIN_SESSIONS: usize = 5;
// Cooldown between stale re-suggestions per project. Once the user has trained
// at least once, we only remind them weekly at most so the Activity feed
// doesn't turn into a nag screen.
pub(crate) const STALE_TRAIN_REFIRE_DAYS: i64 = 7;

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
    // -- record / RTK delta bookkeeping --
    #[serde(default)]
    all_time_record_tokens: u64,
    #[serde(default)]
    daily_record: Option<DailyRecordFact>,
    #[serde(default)]
    last_rtk_total_commands: u64,
    #[serde(default)]
    last_rtk_total_saved: u64,
    #[serde(default)]
    last_weekly_recap_week_key: Option<String>,
    #[serde(default)]
    learnings_milestones_fired: BTreeSet<u32>,
    // Timestamps of the last record-tag emission we actually made for each
    // scope. Used to debounce near-identical beats so a burst of compressions
    // doesn't repaint the same chip every row (24h / 25% rule in
    // `debounce_suppress`).
    #[serde(default)]
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    daily_record_emitted_at: Option<DateTime<Utc>>,
    // TrainSuggestion fire-once / cooldown maps. See observe_train_suggestions.
    #[serde(default)]
    train_suggestions_fired: BTreeSet<String>,
    #[serde(default)]
    stale_train_suggestions_fired_at: BTreeMap<String, DateTime<Utc>>,

    // -- latest-of-kind tile slots --
    // The Activity tab shows one tile per kind, populated by the most recent
    // event of that kind. Rather than persist a queue of every event ever, we
    // store only the freshest event for each tile.
    #[serde(default)]
    last_transformation: Option<TransformationFeedEvent>,
    #[serde(default)]
    last_record: Option<RecordEvent>,
    #[serde(default)]
    last_rtk_batch: Option<RtkBatchEvent>,
    #[serde(default)]
    last_learnings_milestone: Option<LearningsMilestoneEvent>,
    #[serde(default)]
    last_weekly_recap: Option<WeeklyRecapEvent>,
    #[serde(default)]
    last_train_suggestion: Option<TrainSuggestionEvent>,
}

pub struct ActivityFacts {
    path: PathBuf,
    all_time_record_tokens: u64,
    daily_record: Option<DailyRecordFact>,
    last_rtk_total_commands: u64,
    last_rtk_total_saved: u64,
    last_weekly_recap_week_key: Option<String>,
    learnings_milestones_fired: BTreeSet<u32>,
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    daily_record_emitted_at: Option<DateTime<Utc>>,
    train_suggestions_fired: BTreeSet<String>,
    stale_train_suggestions_fired_at: BTreeMap<String, DateTime<Utc>>,
    // Latest-of-kind tile slots. Each observe_* writes to its slot; the
    // snapshot builder reads them to populate the frontend response.
    last_transformation: Option<TransformationFeedEvent>,
    last_record: Option<RecordEvent>,
    last_rtk_batch: Option<RtkBatchEvent>,
    last_learnings_milestone: Option<LearningsMilestoneEvent>,
    last_weekly_recap: Option<WeeklyRecapEvent>,
    last_train_suggestion: Option<TrainSuggestionEvent>,
    rtk_initialized: bool,
    dirty: bool,
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

        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let persisted = serde_json::from_slice::<PersistedActivityFacts>(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if persisted.schema_version != SCHEMA_VERSION {
            // Best-effort delete so the next save replaces the stale file
            // outright rather than silently leaving the old payload behind.
            let _ = std::fs::remove_file(&path);
            return Ok(Self::empty(path));
        }

        Ok(Self {
            path,
            all_time_record_tokens: persisted.all_time_record_tokens,
            daily_record: persisted.daily_record,
            last_rtk_total_commands: persisted.last_rtk_total_commands,
            last_rtk_total_saved: persisted.last_rtk_total_saved,
            last_weekly_recap_week_key: persisted.last_weekly_recap_week_key,
            learnings_milestones_fired: persisted.learnings_milestones_fired,
            all_time_record_emitted_at: persisted.all_time_record_emitted_at,
            daily_record_emitted_at: persisted.daily_record_emitted_at,
            train_suggestions_fired: persisted.train_suggestions_fired,
            stale_train_suggestions_fired_at: persisted.stale_train_suggestions_fired_at,
            last_transformation: persisted.last_transformation,
            last_record: persisted.last_record,
            last_rtk_batch: persisted.last_rtk_batch,
            last_learnings_milestone: persisted.last_learnings_milestone,
            last_weekly_recap: persisted.last_weekly_recap,
            last_train_suggestion: persisted.last_train_suggestion,
            rtk_initialized: true,
            dirty: false,
        })
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            all_time_record_tokens: 0,
            daily_record: None,
            last_rtk_total_commands: 0,
            last_rtk_total_saved: 0,
            last_weekly_recap_week_key: None,
            learnings_milestones_fired: BTreeSet::new(),
            all_time_record_emitted_at: None,
            daily_record_emitted_at: None,
            train_suggestions_fired: BTreeSet::new(),
            stale_train_suggestions_fired_at: BTreeMap::new(),
            last_transformation: None,
            last_record: None,
            last_rtk_batch: None,
            last_learnings_milestone: None,
            last_weekly_recap: None,
            last_train_suggestion: None,
            rtk_initialized: false,
            dirty: false,
        }
    }

    /// Build the Activity-tab snapshot from the latest-of-kind slots. One slot
    /// per tile; each tile on the frontend renders its slot or a placeholder.
    pub fn activity_feed_snapshot(&self) -> ActivityFeedSnapshot {
        ActivityFeedSnapshot {
            transformation: self.last_transformation.clone(),
            record: self.last_record.clone(),
            rtk_batch: self.last_rtk_batch.clone(),
            learnings_milestone: self.last_learnings_milestone.clone(),
            weekly_recap: self.last_weekly_recap.clone(),
            train_suggestion: self.last_train_suggestion.clone(),
        }
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

        // Refresh the transformation tile slot if this event is newer than
        // what we have. The proxy feed is a sliding window replayed on every
        // poll, and the observer iterates oldest-first; the last write wins
        // naturally, but the guard is defensive against out-of-order replays.
        let is_newer = self
            .last_transformation
            .as_ref()
            .and_then(|prev| prev.timestamp.as_deref())
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map(|prev_ts| prev_ts.with_timezone(&Utc) <= observed_at)
            .unwrap_or(true);
        if is_newer {
            self.last_transformation = Some(event.clone());
            self.dirty = true;
        }

        let tokens_saved = event
            .tokens_saved
            .and_then(|n| if n > 0 { Some(n as u64) } else { None });

        if let Some(tokens) = tokens_saved {
            let today = now.format("%Y-%m-%d").to_string();
            let event_day = observed_at.format("%Y-%m-%d").to_string();
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
                    if !debounce_suppress(previous_same_day, self.daily_record_emitted_at, tokens, now) {
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
                let day = if beats_day_today { Some(today.clone()) } else { None };
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
                    request_messages: event.request_messages.clone(),
                    compressed_messages: event.compressed_messages.clone(),
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
                    request_messages: event.request_messages.clone(),
                    compressed_messages: event.compressed_messages.clone(),
                };
                emitted.push(ActivityEvent::Record(record));
            }
        }

        emitted
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
        let batch = RtkBatchEvent {
            observed_at,
            commands_delta,
            tokens_saved_delta,
            total_commands: summary.total_commands,
            total_saved: summary.total_saved,
        };

        self.last_rtk_total_commands = summary.total_commands;
        self.last_rtk_total_saved = summary.total_saved;
        self.last_rtk_batch = Some(batch.clone());
        self.dirty = true;
        Some(ActivityEvent::RtkBatch(batch))
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
        let milestone = LearningsMilestoneEvent {
            observed_at,
            count: THRESHOLD,
            kind: "first_3".into(),
        };
        self.last_learnings_milestone = Some(milestone.clone());
        self.dirty = true;
        Some(ActivityEvent::LearningsMilestone(milestone))
    }

    /// Scan project metadata and emit a `TrainSuggestion` for any project that
    /// matches a trigger. Two kinds:
    ///
    /// - `"never_trained"` — user has logged `NEVER_TRAINED_MIN_SESSIONS`+
    ///   sessions but never run Train on this project. Fires once per project,
    ///   ever (gated by `train_suggestions_fired`).
    /// - `"stale"` — user has trained before but worked on the project 2+
    ///   active days since. Throttled to at most once per
    ///   `STALE_TRAIN_REFIRE_DAYS` per project via
    ///   `stale_train_suggestions_fired_at` so the Activity feed doesn't turn
    ///   into a nag screen.
    pub fn observe_train_suggestions(
        &mut self,
        projects: &[ClaudeCodeProject],
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        let mut events: Vec<TrainSuggestionEvent> = Vec::new();
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

            events.push(TrainSuggestionEvent {
                observed_at,
                project_path: project.project_path.clone(),
                project_display_name: project.display_name.clone(),
                session_count: project.session_count as u32,
                active_days_since_last_learn: active_days,
                kind: kind.into(),
            });

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
            // Tile shows one — latch the latest by observed_at. (All emissions
            // in a single observe call share the same `observed_at`, so this
            // effectively keeps the last project iterated.)
            if let Some(latest) = events.iter().max_by_key(|e| e.observed_at).cloned() {
                self.last_train_suggestion = Some(latest);
            }
            self.dirty = true;
        }

        // Clear a stale latch: the tile should stop showing "no Train run
        // yet" for a project the user has clearly moved past.
        //   - "never_trained" suggestions clear once the project has been
        //     trained (last_learn_ran_at becomes Some).
        //   - "stale" suggestions clear once active_days_since_last_learn
        //     drops below the threshold.
        //   - Any suggestion clears if the project's cwd no longer exists on
        //     disk — `~/.claude/projects/` keeps session files for folders
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
        events
            .into_iter()
            .map(ActivityEvent::TrainSuggestion)
            .collect()
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
        self.dirty = true;
        Some(ActivityEvent::WeeklyRecap(recap))
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let persisted = PersistedActivityFacts {
            schema_version: SCHEMA_VERSION,
            all_time_record_tokens: self.all_time_record_tokens,
            daily_record: self.daily_record.clone(),
            last_rtk_total_commands: self.last_rtk_total_commands,
            last_rtk_total_saved: self.last_rtk_total_saved,
            last_weekly_recap_week_key: self.last_weekly_recap_week_key.clone(),
            learnings_milestones_fired: self.learnings_milestones_fired.clone(),
            all_time_record_emitted_at: self.all_time_record_emitted_at,
            daily_record_emitted_at: self.daily_record_emitted_at,
            train_suggestions_fired: self.train_suggestions_fired.clone(),
            stale_train_suggestions_fired_at: self.stale_train_suggestions_fired_at.clone(),
            last_transformation: self.last_transformation.clone(),
            last_record: self.last_record.clone(),
            last_rtk_batch: self.last_rtk_batch.clone(),
            last_learnings_milestone: self.last_learnings_milestone.clone(),
            last_weekly_recap: self.last_weekly_recap.clone(),
            last_train_suggestion: self.last_train_suggestion.clone(),
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
            request_messages: None,
            compressed_messages: None,
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

    #[test]
    fn observe_transformation_keeps_newer_event_when_older_arrives_after() {
        // The proxy feed is a sliding window replayed every poll; the observer
        // iterates oldest-first so the "last write wins" path works naturally,
        // but if an earlier event shows up after a later one (out-of-order
        // replay), the guard in observe_transformation_at must keep the newer
        // one in the slot.
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();

        let mut newer = mk_transformation(Some("m"), Some(100), Some(10.0));
        newer.request_id = Some("req-newer".into());
        newer.timestamp = Some("2026-04-22T12:00:00Z".into());
        facts.observe_transformation_at(&newer, at(12, 0), at(12, 0));

        let mut older = mk_transformation(Some("m"), Some(200), Some(20.0));
        older.request_id = Some("req-older".into());
        older.timestamp = Some("2026-04-22T09:00:00Z".into());
        facts.observe_transformation_at(&older, at(9, 0), at(12, 0));

        let slot = facts
            .activity_feed_snapshot()
            .transformation
            .expect("transformation slot");
        assert_eq!(
            slot.request_id.as_deref(),
            Some("req-newer"),
            "older replay must not clobber the newer slot"
        );
    }

}
