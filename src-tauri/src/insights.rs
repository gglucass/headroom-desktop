use chrono::Utc;

use crate::models::{DailyInsight, InsightCategory, InsightSeverity, UsageEvent};

pub fn generate_daily_insights(events: &[UsageEvent]) -> Vec<DailyInsight> {
    let total_savings: f64 = events
        .iter()
        .map(|event| event.estimated_cost_savings_usd)
        .sum();

    let total_requests = events.len();

    vec![
        DailyInsight {
            id: format!("savings-{}", Utc::now().timestamp()),
            category: InsightCategory::Savings,
            severity: InsightSeverity::Info,
            title: "Daily savings snapshot".into(),
            recommendation: if total_requests == 0 {
                "Route a client through Headroom to begin building a savings baseline.".into()
            } else {
                "Keep Headroom enabled globally while the MVP focuses on core optimization reliability."
                    .into()
            },
            evidence: format!(
                "{} requests analyzed locally with an estimated ${:.2} in savings.",
                total_requests, total_savings
            ),
            related_workspace: None,
        },
        DailyInsight {
            id: "workflow-cadence".into(),
            category: InsightCategory::Workflow,
            severity: InsightSeverity::Warning,
            title: "Bootstrap the managed Python runtime".into(),
            recommendation:
                "Install the managed runtime during onboarding so Headroom can run reliably.".into(),
            evidence:
                "Headroom MVP depends on a managed Python environment for the Headroom stage."
                    .into(),
            related_workspace: Some("headroom".into()),
        },
    ]
}
