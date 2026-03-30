use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{PipelineStageMetric, UsageEvent, UsageOutcome};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRequest {
    pub client: String,
    pub workspace: String,
    pub upstream_target: String,
    pub original_prompt_tokens: u64,
    pub optimized_prompt_tokens: u64,
    pub stages: Vec<PipelineStageMetric>,
    pub latency_ms: u64,
}

pub fn summarize_request(request: PipelineRequest) -> UsageEvent {
    let estimated_cost_savings_usd = ((request
        .original_prompt_tokens
        .saturating_sub(request.optimized_prompt_tokens))
        as f64)
        / 10_000.0;

    UsageEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        client: request.client,
        workspace: request.workspace,
        upstream_target: request.upstream_target,
        stages: request.stages,
        estimated_input_tokens: request.optimized_prompt_tokens,
        estimated_output_tokens: 0,
        estimated_cost_savings_usd,
        latency_ms: request.latency_ms,
        outcome: UsageOutcome::Success,
    }
}

pub fn default_stage_metrics() -> Vec<PipelineStageMetric> {
    vec![PipelineStageMetric {
        stage_id: "headroom".into(),
        stage_name: "Headroom".into(),
        applied: true,
        estimated_tokens_saved: 2_140,
        added_latency_ms: 96,
        notes: vec!["Mandatory stage executed successfully".into()],
    }]
}
