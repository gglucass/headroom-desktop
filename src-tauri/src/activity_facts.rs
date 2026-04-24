use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    ActivityEvent, ActivityFeedSnapshot, ClaudeCodeProject, LearningsMilestoneEvent, RecordEvent,
    RecordTag, TransformationFeedEvent, TrainSuggestionEvent, WeeklyRecapEvent,
};
use crate::storage::config_file;

// Bumped from 4 → 5 when the learnings tile moved from a one-shot milestone
// (count, kind) to a daily diff shape (patterns/reminders/learnings today +
// project attribution). Old persisted state can't be reused since the event
// fields changed, so the mismatch handler drops the file and starts fresh.
const SCHEMA_VERSION: u8 = 5;
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
pub(crate) const STALE_TRAIN_REFIRE_DAYS: i64 = 5;
// Only nudge Train for projects the user has touched recently. An abandoned
// project with 50 sessions and no train run shouldn't claim the tile forever.
pub(crate) const TRAIN_SUGGESTION_ACTIVE_WINDOW_DAYS: i64 = 2;

// The "Recent large compression" tile should actually be *large*. A pipeline
// run that stripped nothing (tokens_saved ~ 0, savings_percent near 0) would
// otherwise claim the slot and render "Saved 0 tokens".
const TRANSFORMATION_TILE_MIN_SAVINGS_PERCENT: f64 = 20.0;
// Even a genuine huge compression shouldn't pin the tile forever — swap in
// any qualifying event once the current one has been sitting around for this
// long, so the feed keeps circulating.
const TRANSFORMATION_TILE_STALE_AFTER_MINUTES: i64 = 10;

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
    #[serde(default)]
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

    // -- latest-of-kind tile slots --
    // The Activity tab shows one tile per kind, populated by the most recent
    // event of that kind. Rather than persist a queue of every event ever, we
    // store only the freshest event for each tile.
    #[serde(default)]
    last_transformation: Option<TransformationFeedEvent>,
    #[serde(default)]
    last_record: Option<RecordEvent>,
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
    last_weekly_recap_week_key: Option<String>,
    learnings_snapshot_day: Option<NaiveDate>,
    learnings_snapshots: BTreeMap<String, ProjectBulletSnapshot>,
    all_time_record_emitted_at: Option<DateTime<Utc>>,
    daily_record_emitted_at: Option<DateTime<Utc>>,
    last_weekly_recap_check_at: Option<DateTime<Utc>>,
    train_suggestions_fired: BTreeSet<String>,
    stale_train_suggestions_fired_at: BTreeMap<String, DateTime<Utc>>,
    // Latest-of-kind tile slots. Each observe_* writes to its slot; the
    // snapshot builder reads them to populate the frontend response.
    last_transformation: Option<TransformationFeedEvent>,
    last_record: Option<RecordEvent>,
    last_learnings_milestone: Option<LearningsMilestoneEvent>,
    last_weekly_recap: Option<WeeklyRecapEvent>,
    last_train_suggestion: Option<TrainSuggestionEvent>,
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
            last_weekly_recap_week_key: persisted.last_weekly_recap_week_key,
            learnings_snapshot_day: persisted.learnings_snapshot_day,
            learnings_snapshots: persisted.learnings_snapshots,
            all_time_record_emitted_at: persisted.all_time_record_emitted_at,
            daily_record_emitted_at: persisted.daily_record_emitted_at,
            last_weekly_recap_check_at: persisted.last_weekly_recap_check_at,
            train_suggestions_fired: persisted.train_suggestions_fired,
            stale_train_suggestions_fired_at: persisted.stale_train_suggestions_fired_at,
            last_transformation: persisted.last_transformation,
            last_record: persisted.last_record,
            last_learnings_milestone: persisted.last_learnings_milestone,
            last_weekly_recap: persisted.last_weekly_recap,
            last_train_suggestion: persisted.last_train_suggestion,
            dirty: false,
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
            last_transformation: None,
            last_record: None,
            last_learnings_milestone: None,
            last_weekly_recap: None,
            last_train_suggestion: None,
            dirty: false,
        }
    }

    /// Build the Activity-tab snapshot from the latest-of-kind slots. One slot
    /// per tile; each tile on the frontend renders its slot or a placeholder.
    pub fn activity_feed_snapshot(&self) -> ActivityFeedSnapshot {
        ActivityFeedSnapshot {
            transformation: self.last_transformation.clone(),
            record: self.last_record.clone(),
            rtk_today: None,
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

        let tokens_saved = event
            .tokens_saved
            .and_then(|n| if n > 0 { Some(n as u64) } else { None });
        let savings_percent = event.savings_percent.unwrap_or(0.0);

        if let Some(tokens) = tokens_saved {
            // "Recent large compression" tile: must actually be large, and
            // we replace the current pick only if the new one is bigger or
            // the current one has been pinned longer than the stale window.
            if savings_percent > TRANSFORMATION_TILE_MIN_SAVINGS_PERCENT {
                let should_replace = match self.last_transformation.as_ref() {
                    None => true,
                    Some(prev) => {
                        let prev_tokens = prev
                            .tokens_saved
                            .and_then(|n| u64::try_from(n).ok())
                            .unwrap_or(0);
                        if tokens > prev_tokens {
                            true
                        } else {
                            // Only swap a same-or-smaller event in when the
                            // tile's current pick has aged past the stale
                            // window. Comparison is against wall-clock `now`
                            // so the rule matches the user's experience
                            // ("tile hasn't moved in 10 minutes, rotate it").
                            prev.timestamp
                                .as_deref()
                                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                                .map(|prev_ts| {
                                    now.signed_duration_since(prev_ts.with_timezone(&Utc))
                                        > Duration::minutes(
                                            TRANSFORMATION_TILE_STALE_AFTER_MINUTES,
                                        )
                                })
                                .unwrap_or(true)
                        }
                    }
                };
                if should_replace {
                    self.last_transformation = Some(event.clone());
                    self.dirty = true;
                }
            }

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
                    input_tokens_original: event.input_tokens_original,
                    input_tokens_optimized: event.input_tokens_optimized,
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
                    input_tokens_original: event.input_tokens_original,
                    input_tokens_optimized: event.input_tokens_optimized,
                    request_messages: event.request_messages.clone(),
                    compressed_messages: event.compressed_messages.clone(),
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
        let today = observed_at.date_naive();
        let day_changed = self.learnings_snapshot_day != Some(today);

        if day_changed {
            // New UTC day — drop yesterday's snapshots and re-baseline against
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

        if self.last_learnings_milestone.as_ref() != Some(&event) {
            self.last_learnings_milestone = Some(event.clone());
            self.dirty = true;
        }
        event
    }

    /// Scan project metadata and emit a `TrainSuggestion` for any project that
    /// matches a trigger. Applies to both kinds only when the user has worked
    /// on the project within `TRAIN_SUGGESTION_ACTIVE_WINDOW_DAYS` — the tile
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
    pub fn observe_train_suggestions(
        &mut self,
        projects: &[ClaudeCodeProject],
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        let mut events: Vec<TrainSuggestionEvent> = Vec::new();
        for project in projects {
            if !worked_within_active_window(&project.last_worked_at, observed_at) {
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
        //   - Any suggestion clears if the user hasn't touched the project
        //     in the active window — same gate that blocks emission, applied
        //     to the latch so an abandoned project doesn't stay pinned.
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
        events
            .into_iter()
            .map(ActivityEvent::TrainSuggestion)
            .collect()
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
        if !self.dirty {
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
            last_transformation: self.last_transformation.clone(),
            last_record: self.last_record.clone(),
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
    use chrono::{TimeZone, Timelike};
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
            vec![mk_learn_input(
                "/x/demo",
                "demo",
                &["a", "b", "c"],
                &[],
            )],
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
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project(
            "/Users/u/Code/demo",
            5,
            None,
            0,
            "2026-04-22T10:00:00Z",
        )];
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
        let projects = vec![mk_project(
            "/Users/u/Code/demo",
            4,
            None,
            0,
            "2026-04-22T10:00:00Z",
        )];
        assert!(facts
            .observe_train_suggestions(&projects, at(10, 0))
            .is_empty());
    }

    #[test]
    fn train_suggestion_stale_throttled_to_weekly() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // Rebuild the project with a fresh last_worked_at at each observation
        // — the active-window gate requires the user to still be touching the
        // project; we're testing the *cooldown*, not the active window.
        let mk = |worked_at: &str| {
            vec![mk_project(
                "/Users/u/Code/demo",
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
        let third =
            facts.observe_train_suggestions(&mk("2026-04-30T10:00:00Z"), day8);
        assert_eq!(third.len(), 1, "after cooldown, stale must re-fire");
    }

    #[test]
    fn train_suggestion_persists_across_reload() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let projects = vec![mk_project(
            "/Users/u/Code/demo",
            5,
            None,
            0,
            "2026-04-22T10:00:00Z",
        )];
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
    fn train_suggestion_skipped_when_project_idle_for_days() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        // Over the never-trained session threshold, but the user hasn't
        // touched the project in 3 days — outside the active window.
        let never_trained = vec![mk_project(
            "/Users/u/Code/abandoned",
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
            "/Users/u/Code/abandoned-stale",
            10,
            Some("2026-04-15T10:00:00Z"),
            5,
            "2026-04-19T10:00:00Z",
        )];
        assert!(
            facts.observe_train_suggestions(&stale, at(10, 0)).is_empty(),
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

    fn mk_tile_event(request_id: &str, timestamp: &str, tokens: i64, pct: f64) -> TransformationFeedEvent {
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

}
