use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    NotInstalled,
    Installing,
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedTool {
    pub id: String,
    pub name: String,
    pub description: String,
    pub runtime: String,
    pub required: bool,
    pub enabled: bool,
    pub status: ToolStatus,
    pub source_url: String,
    pub version: String,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineStageMetric {
    pub stage_id: String,
    pub stage_name: String,
    pub applied: bool,
    pub estimated_tokens_saved: u64,
    pub added_latency_ms: u64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOutcome {
    Success,
    Bypassed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub client: String,
    pub workspace: String,
    pub upstream_target: String,
    pub stages: Vec<PipelineStageMetric>,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
    pub estimated_cost_savings_usd: f64,
    pub latency_ms: u64,
    pub outcome: UsageOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightCategory {
    Savings,
    Workflow,
    Health,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyInsight {
    pub id: String,
    pub category: InsightCategory,
    pub severity: InsightSeverity,
    pub title: String,
    pub recommendation: String,
    pub evidence: String,
    pub related_workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientHealth {
    Healthy,
    Attention,
    NotDetected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientStatus {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub configured: bool,
    pub health: ClientHealth,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchExperience {
    FirstRun,
    Resume,
    Dashboard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySavingsPoint {
    pub date: String,
    pub estimated_savings_usd: f64,
    pub estimated_tokens_saved: u64,
    pub actual_cost_usd: f64,
    pub total_tokens_sent: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlySavingsPoint {
    pub hour: String,
    pub estimated_savings_usd: f64,
    pub estimated_tokens_saved: u64,
    pub actual_cost_usd: f64,
    pub total_tokens_sent: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardState {
    pub app_version: String,
    pub launch_experience: LaunchExperience,
    pub bootstrap_complete: bool,
    pub python_runtime_installed: bool,
    pub lifetime_requests: usize,
    pub lifetime_estimated_savings_usd: f64,
    pub lifetime_estimated_tokens_saved: u64,
    pub session_requests: usize,
    pub session_estimated_savings_usd: f64,
    pub session_estimated_tokens_saved: u64,
    pub session_savings_pct: f64,
    pub daily_savings: Vec<DailySavingsPoint>,
    pub hourly_savings: Vec<HourlySavingsPoint>,
    pub tools: Vec<ManagedTool>,
    pub clients: Vec<ClientStatus>,
    pub recent_usage: Vec<UsageEvent>,
    pub insights: Vec<DailyInsight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapProgress {
    pub running: bool,
    pub complete: bool,
    pub failed: bool,
    pub current_step: String,
    pub message: String,
    pub current_step_eta_seconds: u64,
    pub overall_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSetupResult {
    pub client_id: String,
    pub applied: bool,
    pub already_configured: bool,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub backup_files: Vec<String>,
    pub next_steps: Vec<String>,
    pub verification: ClientSetupVerification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSetupVerification {
    pub client_id: String,
    pub verified: bool,
    pub proxy_reachable: bool,
    pub checks: Vec<String>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConnectorStatus {
    pub client_id: String,
    pub name: String,
    pub installed: bool,
    pub enabled: bool,
    pub verified: bool,
    pub last_configured_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtkRuntimeStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path_configured: bool,
    pub hook_configured: bool,
    pub total_commands: Option<u64>,
    pub total_saved: Option<u64>,
    pub avg_savings_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub platform: String,
    pub support_tier: String,
    pub installed: bool,
    pub running: bool,
    pub starting: bool,
    pub paused: bool,
    pub proxy_reachable: bool,
    pub headroom_pid: Option<u32>,
    pub mcp_configured: Option<bool>,
    pub mcp_error: Option<String>,
    pub ml_installed: Option<bool>,
    pub kompress_enabled: Option<bool>,
    pub headroom_learn_supported: bool,
    pub headroom_learn_disabled_reason: Option<String>,
    pub startup_error: Option<String>,
    pub startup_error_hint: Option<String>,
    pub runtime_upgrade_failure: Option<RuntimeUpgradeFailure>,
    pub rtk: RtkRuntimeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUpgradeProgress {
    pub running: bool,
    pub complete: bool,
    pub failed: bool,
    pub current_step: String,
    pub message: String,
    pub overall_percent: u8,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeFailurePhase {
    Install,
    BootValidation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUpgradeFailure {
    pub app_version: String,
    pub target_headroom_version: String,
    pub fallback_headroom_version: Option<String>,
    pub failure_phase: UpgradeFailurePhase,
    pub attempts: u32,
    pub first_attempt_at: DateTime<Utc>,
    pub last_attempt_at: DateTime<Utc>,
    pub error_message: String,
    pub error_hint: Option<String>,
    pub rollback_restored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeProject {
    pub id: String,
    pub project_path: String,
    pub display_name: String,
    pub last_worked_at: String,
    pub session_count: usize,
    pub last_learn_ran_at: Option<String>,
    pub has_persisted_learnings: bool,
    pub active_days_since_last_learn: usize,
    pub last_learn_pattern_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomLearnStatus {
    pub running: bool,
    pub project_path: Option<String>,
    pub project_display_name: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub elapsed_seconds: Option<u64>,
    pub progress_percent: u8,
    pub summary: String,
    pub success: Option<bool>,
    pub error: Option<String>,
    pub last_run_at: Option<String>,
    pub output_tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomLearnPrereqStatus {
    pub claude_cli_available: bool,
    pub claude_cli_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransformationFeedEvent {
    #[serde(default, alias = "request_id")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, alias = "input_tokens_original")]
    pub input_tokens_original: Option<u64>,
    #[serde(default, alias = "input_tokens_optimized")]
    pub input_tokens_optimized: Option<u64>,
    #[serde(default, alias = "tokens_saved")]
    pub tokens_saved: Option<i64>,
    #[serde(default, alias = "savings_percent")]
    pub savings_percent: Option<f64>,
    #[serde(default, alias = "transforms_applied")]
    pub transforms_applied: Vec<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default, alias = "turn_id")]
    pub turn_id: Option<String>,
    // Raw request/response payload captured by the proxy's RequestLogger when
    // `log_full_messages` is enabled. Pass-through as `serde_json::Value` so
    // the exact Anthropic/OpenAI message shape (role + structured content
    // blocks) reaches the frontend unchanged; the desktop renders it, it
    // does not need to re-parse it.
    #[serde(default, alias = "request_messages")]
    pub request_messages: Option<serde_json::Value>,
    // Post-compression message list — what was actually sent upstream after
    // Headroom's pipeline ran. Present only on proxies that carry this field
    // (compressed_messages was added after request_messages was already in
    // use, so older proxies will emit `None` here).
    #[serde(default, alias = "compressed_messages")]
    pub compressed_messages: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransformationFeedResponse {
    pub log_full_messages: bool,
    pub transformations: Vec<TransformationFeedEvent>,
    pub proxy_reachable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveLearning {
    pub id: String,
    pub content: String,
    pub category: String,
    pub importance: f64,
    pub evidence_count: u32,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppliedSection {
    pub title: String,
    pub bullets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedPatterns {
    pub claude_md: Vec<AppliedSection>,
    pub memory_md: Vec<AppliedSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtkBatchEvent {
    pub observed_at: DateTime<Utc>,
    pub commands_delta: u64,
    pub tokens_saved_delta: u64,
    pub total_commands: u64,
    pub total_saved: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum RecordTag {
    Daily,
    Weekly,
    AllTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordEvent {
    pub observed_at: DateTime<Utc>,
    pub tags: Vec<RecordTag>,
    pub tokens_saved: u64,
    pub savings_percent: Option<f64>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub request_id: Option<String>,
    pub previous_record: Option<u64>,
    pub day: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    // Carried forward from the source transformation so the record row can
    // show what the record-setting compression was actually about. Populated
    // only when the proxy's `log_full_messages` is enabled. `compressed_messages`
    // is only populated by proxies that carry the field (see struct doc above).
    #[serde(default)]
    pub request_messages: Option<serde_json::Value>,
    #[serde(default)]
    pub compressed_messages: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyRecapEvent {
    pub observed_at: DateTime<Utc>,
    pub week_start: String,
    pub week_end: String,
    pub total_tokens_saved: u64,
    pub total_savings_usd: f64,
    pub active_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningsMilestoneEvent {
    pub observed_at: DateTime<Utc>,
    pub count: u32,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrainSuggestionEvent {
    pub observed_at: DateTime<Utc>,
    pub project_path: String,
    pub project_display_name: String,
    pub session_count: u32,
    pub active_days_since_last_learn: u32,
    // "never_trained" | "stale"
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
pub enum ActivityEvent {
    #[serde(rename = "transformation")]
    Transformation(TransformationFeedEvent),
    #[serde(rename = "rtkBatch")]
    RtkBatch(RtkBatchEvent),
    #[serde(rename = "record")]
    Record(RecordEvent),
    #[serde(rename = "weeklyRecap")]
    WeeklyRecap(WeeklyRecapEvent),
    #[serde(rename = "learningsMilestone")]
    LearningsMilestone(LearningsMilestoneEvent),
    #[serde(rename = "trainSuggestion")]
    TrainSuggestion(TrainSuggestionEvent),
}

/// One slot per tile kind. `None` renders as a placeholder on the frontend,
/// `Some(event)` renders the live row. Built from `ActivityFacts`'s latest-of-
/// kind slots — no event stream, no dedupe logic on either side of the IPC
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFeedSnapshot {
    pub transformation: Option<TransformationFeedEvent>,
    pub record: Option<RecordEvent>,
    pub rtk_batch: Option<RtkBatchEvent>,
    pub learnings_milestone: Option<LearningsMilestoneEvent>,
    pub weekly_recap: Option<WeeklyRecapEvent>,
    pub train_suggestion: Option<TrainSuggestionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFeedResponse {
    pub tiles: ActivityFeedSnapshot,
    pub proxy_reachable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDecision {
    Include,
    Defer,
    Research,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResearchCandidate {
    pub name: String,
    pub category: String,
    pub repository: String,
    pub runtime: String,
    pub license: String,
    pub local_only_fit: String,
    pub install_method: String,
    pub maintenance: String,
    pub decision: CandidateDecision,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeUsageWindow {
    /// 0–100 percentage consumed
    pub utilization: f64,
    pub resets_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    /// 0–100 percentage consumed
    pub utilization: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeUsage {
    pub five_hour: Option<ClaudeUsageWindow>,
    pub seven_day: Option<ClaudeUsageWindow>,
    pub extra_usage: Option<ClaudeExtraUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAuthMethod {
    ClaudeAiOauth,
    ApiKey,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudePlanTier {
    Free,
    Pro,
    Max5x,
    Max20x,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadroomSubscriptionTier {
    Pro,
    Max5x,
    Max20x,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BillingPeriod {
    Annual,
    Monthly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingGateReason {
    SignInRequired,
    WeeklyUsageLimitReached,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountProfile {
    pub auth_method: ClaudeAuthMethod,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub billing_type: Option<String>,
    pub account_created_at: Option<DateTime<Utc>>,
    pub subscription_created_at: Option<DateTime<Utc>>,
    pub has_extra_usage_enabled: bool,
    pub plan_tier: ClaudePlanTier,
    pub plan_detection_source: Option<String>,
    pub weekly_utilization_pct: Option<f64>,
    pub five_hour_utilization_pct: Option<f64>,
    pub extra_usage_monthly_limit: Option<f64>,
    pub profile_fetch_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomAccountProfile {
    pub email: String,
    pub trial_started_at: Option<DateTime<Utc>>,
    pub trial_ends_at: Option<DateTime<Utc>>,
    pub trial_active: bool,
    pub subscription_active: bool,
    pub subscription_tier: Option<HeadroomSubscriptionTier>,
    pub subscription_started_at: Option<DateTime<Utc>>,
    pub subscription_renews_at: Option<DateTime<Utc>>,
    pub subscription_amount_cents: Option<i64>,
    pub subscription_billing_period: Option<String>,
    pub subscription_discount_duration: Option<String>,
    pub subscription_discount_duration_in_months: Option<i64>,
    pub invite_code: Option<String>,
    pub accepted_invites_count: usize,
    pub invite_bonus_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomPricingStatus {
    pub authenticated: bool,
    pub local_grace_started_at: DateTime<Utc>,
    pub local_grace_ends_at: DateTime<Utc>,
    pub local_grace_active: bool,
    pub account_sync_error: Option<String>,
    pub needs_authentication: bool,
    pub optimization_allowed: bool,
    pub should_nudge: bool,
    pub gate_reason: Option<PricingGateReason>,
    pub gate_message: String,
    pub nudge_threshold_percent: Option<f64>,
    pub disable_threshold_percent: Option<f64>,
    pub effective_disable_threshold_percent: Option<f64>,
    pub recommended_subscription_tier: Option<HeadroomSubscriptionTier>,
    pub recommended_subscription_price_usd: Option<f64>,
    pub claude: ClaudeAccountProfile,
    pub account: Option<HeadroomAccountProfile>,
    pub launch_discount_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomAuthCodeRequest {
    pub email: String,
    pub expires_in_seconds: u64,
}
