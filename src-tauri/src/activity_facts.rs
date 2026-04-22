use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{
    ActivityEvent, LearningsMilestoneEvent, MilestoneEvent, NewModelEvent, RecordEvent,
    RtkBatchEvent, SavingsMilestoneEvent, StreakEvent, TransformationFeedEvent, WeeklyRecapEvent,
};
use crate::storage::config_file;
use crate::tool_manager::RtkGainSummary;

const SCHEMA_VERSION: u8 = 1;
const RECENT_EVENTS_CAP: usize = 200;

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
}

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
    rtk_initialized: bool,
    dirty: bool,
}

impl ActivityFacts {
    pub fn load_or_create(base_dir: &Path) -> Result<Self> {
        let path = config_file(base_dir, "activity-facts.json");
        if !path.exists() {
            return Ok(Self::empty(path));
        }

        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
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
            rtk_initialized: false,
            dirty: false,
        }
    }

    pub fn recent_events(&self) -> Vec<ActivityEvent> {
        self.recent_events.iter().cloned().collect()
    }

    pub fn observe_transformation(
        &mut self,
        event: &TransformationFeedEvent,
        observed_at: DateTime<Utc>,
    ) -> Vec<ActivityEvent> {
        let mut emitted = Vec::new();

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
            let day = observed_at.format("%Y-%m-%d").to_string();
            let beats_day = match &self.daily_record {
                Some(existing) if existing.day == day => tokens > existing.tokens_saved,
                _ => true,
            };
            if beats_day {
                self.daily_record = Some(DailyRecordFact {
                    day: day.clone(),
                    tokens_saved: tokens,
                    observed_at,
                    model: event.model.clone(),
                    provider: event.provider.clone(),
                    request_id: event.request_id.clone(),
                    savings_percent: event.savings_percent,
                });
                emitted.push(ActivityEvent::DailyRecord(RecordEvent {
                    observed_at,
                    tokens_saved: tokens,
                    savings_percent: event.savings_percent,
                    model: event.model.clone(),
                    provider: event.provider.clone(),
                    request_id: event.request_id.clone(),
                    previous_record: None,
                    day: Some(day),
                    workspace: event.workspace.clone(),
                }));
            }

            if tokens > self.all_time_record_tokens {
                let previous = if self.all_time_record_tokens == 0 {
                    None
                } else {
                    Some(self.all_time_record_tokens)
                };
                self.all_time_record_tokens = tokens;
                emitted.push(ActivityEvent::AllTimeRecord(RecordEvent {
                    observed_at,
                    tokens_saved: tokens,
                    savings_percent: event.savings_percent,
                    model: event.model.clone(),
                    provider: event.provider.clone(),
                    request_id: event.request_id.clone(),
                    previous_record: previous,
                    day: None,
                    workspace: event.workspace.clone(),
                }));
            }
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
        let week_end = today_local
            .pred_opt()
            .unwrap_or(today_local);
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
            self.recent_events.pop_front();
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
        };
        let bytes = serde_json::to_vec_pretty(&persisted)
            .context("serializing activity facts")?;
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

    #[test]
    fn daily_record_updates_only_on_beat_and_resets_on_day_change() {
        let (_tmp, base) = base_dir();
        let mut facts = ActivityFacts::load_or_create(&base).unwrap();
        let events = facts.observe_transformation(
            &mk_transformation(Some("a"), Some(500), Some(50.0)),
            at(10, 0),
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, ActivityEvent::DailyRecord(_))));
        let events2 = facts.observe_transformation(
            &mk_transformation(Some("a"), Some(200), Some(20.0)),
            at(10, 1),
        );
        assert!(!events2
            .iter()
            .any(|e| matches!(e, ActivityEvent::DailyRecord(_))));
        let next_day = Utc.with_ymd_and_hms(2026, 4, 23, 1, 0, 0).unwrap();
        let events3 = facts.observe_transformation(
            &mk_transformation(Some("a"), Some(100), Some(10.0)),
            next_day,
        );
        assert!(events3
            .iter()
            .any(|e| matches!(e, ActivityEvent::DailyRecord(_))));
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
                ActivityEvent::AllTimeRecord(r) => Some(r),
                _ => None,
            })
            .expect("all-time record event");
        assert_eq!(record.previous_record, Some(500));
        assert_eq!(record.tokens_saved, 900);
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
        assert!(events
            .iter()
            .any(|e| matches!(e, ActivityEvent::Streak(s) if s.days == 3 && s.kind == "threshold")));
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
        assert!(events
            .iter()
            .any(|e| matches!(e, ActivityEvent::Streak(s) if s.kind == "new_record" && s.days == 3)));
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
        let mut transformation =
            mk_transformation(Some("claude-x"), Some(1_000), Some(50.0));
        transformation.workspace = Some("/Users/u/Code/demo-repo".into());
        let events = facts.observe_transformation(&transformation, at(10, 0));
        let new_model = events
            .iter()
            .find_map(|e| match e {
                ActivityEvent::NewModel(m) => Some(m),
                _ => None,
            })
            .expect("new model");
        assert_eq!(new_model.workspace.as_deref(), Some("/Users/u/Code/demo-repo"));
        let daily = events
            .iter()
            .find_map(|e| match e {
                ActivityEvent::DailyRecord(r) => Some(r),
                _ => None,
            })
            .expect("daily record");
        assert_eq!(daily.workspace.as_deref(), Some("/Users/u/Code/demo-repo"));
        let all_time = events
            .iter()
            .find_map(|e| match e {
                ActivityEvent::AllTimeRecord(r) => Some(r),
                _ => None,
            })
            .expect("all-time record");
        assert_eq!(all_time.workspace.as_deref(), Some("/Users/u/Code/demo-repo"));
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
}
