use crate::models::ActivityEvent;

#[derive(Debug, Clone)]
pub struct NotificationPayload {
    pub title: String,
    pub body: String,
    pub action: Option<String>,
}

const FIRST_NOTIFIABLE_TOKEN_MILESTONES: [u64; 5] =
    [100_000, 1_000_000, 5_000_000, 10_000_000, 50_000_000];
const REPEATING_NOTIFIABLE_TOKEN_STEP: u64 = 100_000_000;

pub fn token_milestone_is_notifiable(tokens: u64) -> bool {
    if FIRST_NOTIFIABLE_TOKEN_MILESTONES.contains(&tokens) {
        return true;
    }
    tokens >= REPEATING_NOTIFIABLE_TOKEN_STEP && tokens % REPEATING_NOTIFIABLE_TOKEN_STEP == 0
}

fn format_tokens_short(tokens: u64) -> String {
    if tokens >= 1_000_000_000 {
        let value = tokens as f64 / 1_000_000_000.0;
        if tokens % 1_000_000_000 == 0 {
            format!("{}B", value as u64)
        } else {
            format!("{:.1}B", value)
        }
    } else if tokens >= 1_000_000 {
        let value = tokens as f64 / 1_000_000.0;
        if tokens % 1_000_000 == 0 {
            format!("{}M", value as u64)
        } else {
            format!("{:.1}M", value)
        }
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub fn notification_for_token_milestone(tokens: u64) -> Option<NotificationPayload> {
    if !token_milestone_is_notifiable(tokens) {
        return None;
    }
    let label = format_tokens_short(tokens);
    Some(NotificationPayload {
        title: format!("{label} tokens saved"),
        body: format!("Your lifetime savings with Headroom just crossed {label}."),
        action: Some("activity".into()),
    })
}

/// Map a batch of freshly-emitted activity events to the notifications that
/// should fire for them. Preserves input order and drops events whose kind
/// (or threshold, for token milestones) isn't notifiable.
pub fn collect_notification_payloads(events: &[ActivityEvent]) -> Vec<NotificationPayload> {
    events.iter().filter_map(notification_for_event).collect()
}

pub fn notification_for_event(event: &ActivityEvent) -> Option<NotificationPayload> {
    match event {
        ActivityEvent::Milestone(m) => notification_for_token_milestone(m.milestone_tokens_saved),
        ActivityEvent::AllTimeRecord(r) => Some(NotificationPayload {
            title: "New record compression".into(),
            body: format!(
                "Saved {} tokens on a single request. Your biggest one yet.",
                format_with_commas(r.tokens_saved)
            ),
            action: Some("activity".into()),
        }),
        ActivityEvent::PromptAllTimeRecord(r) => Some(NotificationPayload {
            title: "New record prompt".into(),
            body: format!(
                "Saved {} tokens across {} call{} on a single prompt. Your biggest one yet.",
                format_with_commas(r.tokens_saved),
                r.call_count,
                if r.call_count == 1 { "" } else { "s" }
            ),
            action: Some("activity".into()),
        }),
        ActivityEvent::Streak(s) if s.kind == "new_record" => Some(NotificationPayload {
            title: "New longest streak".into(),
            body: format!("You're on a {}-day run with Headroom. Keep it up.", s.days),
            action: Some("activity".into()),
        }),
        ActivityEvent::WeeklyRecap(r) => Some(NotificationPayload {
            title: "Your week with Headroom".into(),
            body: format!(
                "Last week: {} tokens saved, ${:.2} across {} active day{}.",
                format_with_commas(r.total_tokens_saved),
                r.total_savings_usd,
                r.active_days,
                if r.active_days == 1 { "" } else { "s" }
            ),
            action: Some("activity".into()),
        }),
        ActivityEvent::LearningsMilestone(_) => Some(NotificationPayload {
            title: "Headroom is learning from you".into(),
            body: "Three patterns extracted from your work so far. Check the Optimize panel to see what's being applied."
                .into(),
            action: Some("optimize".into()),
        }),
        _ => None,
    }
}

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        LearningsMilestoneEvent, MemoryFeedEvent, MilestoneEvent, NewModelEvent, RecordEvent,
        RtkBatchEvent, SavingsMilestoneEvent, StreakEvent, TransformationFeedEvent,
        WeeklyRecapEvent,
    };
    use chrono::{TimeZone, Utc};

    fn ts() -> chrono::DateTime<chrono::Utc> {
        Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap()
    }

    #[test]
    fn token_milestone_notifiable_table() {
        assert!(token_milestone_is_notifiable(100_000));
        assert!(!token_milestone_is_notifiable(500_000));
        assert!(token_milestone_is_notifiable(1_000_000));
        assert!(!token_milestone_is_notifiable(2_000_000));
        assert!(token_milestone_is_notifiable(5_000_000));
        assert!(token_milestone_is_notifiable(10_000_000));
        assert!(!token_milestone_is_notifiable(20_000_000));
        assert!(!token_milestone_is_notifiable(30_000_000));
        assert!(!token_milestone_is_notifiable(40_000_000));
        assert!(token_milestone_is_notifiable(50_000_000));
        assert!(!token_milestone_is_notifiable(60_000_000));
        assert!(token_milestone_is_notifiable(100_000_000));
        assert!(!token_milestone_is_notifiable(150_000_000));
        assert!(token_milestone_is_notifiable(200_000_000));
        assert!(token_milestone_is_notifiable(500_000_000));
    }

    #[test]
    fn format_tokens_short_handles_all_tiers() {
        assert_eq!(format_tokens_short(500), "500");
        assert_eq!(format_tokens_short(100_000), "100K");
        assert_eq!(format_tokens_short(1_000_000), "1M");
        assert_eq!(format_tokens_short(1_500_000), "1.5M");
        assert_eq!(format_tokens_short(50_000_000), "50M");
        assert_eq!(format_tokens_short(1_000_000_000), "1B");
    }

    #[test]
    fn format_with_commas_standard() {
        assert_eq!(format_with_commas(0), "0");
        assert_eq!(format_with_commas(999), "999");
        assert_eq!(format_with_commas(1_000), "1,000");
        assert_eq!(format_with_commas(12_345_678), "12,345,678");
    }

    #[test]
    fn notifies_for_eligible_milestone() {
        let ev = ActivityEvent::Milestone(MilestoneEvent {
            observed_at: ts(),
            milestone_tokens_saved: 5_000_000,
            kind: "first_5m".into(),
        });
        let p = notification_for_event(&ev).expect("should notify");
        assert!(p.title.contains("5M"));
        assert!(p.body.contains("5M"));
    }

    #[test]
    fn does_not_notify_for_feed_only_milestone() {
        let ev = ActivityEvent::Milestone(MilestoneEvent {
            observed_at: ts(),
            milestone_tokens_saved: 20_000_000,
            kind: "repeating_10m".into(),
        });
        assert!(notification_for_event(&ev).is_none());
    }

    #[test]
    fn notifies_for_all_time_record() {
        let ev = ActivityEvent::AllTimeRecord(RecordEvent {
            observed_at: ts(),
            tokens_saved: 12_345,
            savings_percent: Some(90.0),
            model: Some("m".into()),
            provider: None,
            request_id: None,
            previous_record: Some(500),
            day: None,
            workspace: None,
        });
        let p = notification_for_event(&ev).expect("should notify");
        assert!(p.title.contains("New record"));
        assert!(p.body.contains("12,345"));
    }

    #[test]
    fn notifies_for_new_record_streak_only() {
        let record = ActivityEvent::Streak(StreakEvent {
            observed_at: ts(),
            days: 7,
            kind: "new_record".into(),
        });
        assert!(notification_for_event(&record).is_some());

        let threshold = ActivityEvent::Streak(StreakEvent {
            observed_at: ts(),
            days: 7,
            kind: "threshold".into(),
        });
        assert!(notification_for_event(&threshold).is_none());
    }

    #[test]
    fn notifies_for_weekly_recap() {
        let ev = ActivityEvent::WeeklyRecap(WeeklyRecapEvent {
            observed_at: ts(),
            week_start: "2026-04-20".into(),
            week_end: "2026-04-26".into(),
            total_tokens_saved: 12_500,
            total_savings_usd: 4.25,
            active_days: 5,
        });
        let p = notification_for_event(&ev).expect("should notify");
        assert!(p.body.contains("12,500"));
        assert!(p.body.contains("$4.25"));
        assert!(p.body.contains("5 active days"));
    }

    #[test]
    fn notifies_for_learnings_milestone() {
        let ev = ActivityEvent::LearningsMilestone(LearningsMilestoneEvent {
            observed_at: ts(),
            count: 3,
            kind: "first_3".into(),
        });
        assert!(notification_for_event(&ev).is_some());
    }

    #[test]
    fn collect_payloads_filters_and_preserves_order() {
        let events = vec![
            ActivityEvent::Streak(StreakEvent {
                observed_at: ts(),
                days: 3,
                kind: "threshold".into(), // dropped
            }),
            ActivityEvent::Milestone(MilestoneEvent {
                observed_at: ts(),
                milestone_tokens_saved: 1_000_000, // kept
                kind: "first_1m".into(),
            }),
            ActivityEvent::Milestone(MilestoneEvent {
                observed_at: ts(),
                milestone_tokens_saved: 20_000_000, // dropped (not in notify set)
                kind: "repeating_10m".into(),
            }),
            ActivityEvent::LearningsMilestone(LearningsMilestoneEvent {
                observed_at: ts(),
                count: 3,
                kind: "first_3".into(), // kept
            }),
            ActivityEvent::Streak(StreakEvent {
                observed_at: ts(),
                days: 10,
                kind: "new_record".into(), // kept
            }),
        ];
        let payloads = collect_notification_payloads(&events);
        assert_eq!(payloads.len(), 3);
        assert!(payloads[0].title.contains("1M"));
        assert!(payloads[1].title.contains("Headroom is learning"));
        assert!(payloads[2].title.contains("New longest streak"));
    }

    #[test]
    fn collect_payloads_empty_on_empty_input() {
        assert!(collect_notification_payloads(&[]).is_empty());
    }

    #[test]
    fn does_not_notify_for_excluded_kinds() {
        let candidates = [
            ActivityEvent::DailyRecord(RecordEvent {
                observed_at: ts(),
                tokens_saved: 100,
                savings_percent: None,
                model: None,
                provider: None,
                request_id: None,
                previous_record: None,
                day: Some("2026-04-22".into()),
                workspace: None,
            }),
            ActivityEvent::SavingsMilestone(SavingsMilestoneEvent {
                observed_at: ts(),
                milestone_usd: 10,
                kind: "first_10".into(),
            }),
            ActivityEvent::NewModel(NewModelEvent {
                observed_at: ts(),
                model: "x".into(),
                provider: None,
                workspace: None,
            }),
            ActivityEvent::RtkBatch(RtkBatchEvent {
                observed_at: ts(),
                commands_delta: 1,
                tokens_saved_delta: 1,
                total_commands: 1,
                total_saved: 1,
            }),
            ActivityEvent::Transformation(TransformationFeedEvent {
                request_id: None,
                timestamp: None,
                provider: None,
                model: None,
                input_tokens_original: None,
                input_tokens_optimized: None,
                tokens_saved: None,
                savings_percent: None,
                transforms_applied: vec![],
                workspace: None,
                turn_id: None,
            }),
            ActivityEvent::Memory(MemoryFeedEvent {
                id: "m".into(),
                created_at: "2026-04-22T10:00:00Z".into(),
                scope: "user".into(),
                content: "x".into(),
                importance: 0.5,
                evidence_count: 1,
            }),
        ];
        for ev in candidates.iter() {
            assert!(
                notification_for_event(ev).is_none(),
                "unexpected notify for {:?}",
                ev
            );
        }
    }
}
