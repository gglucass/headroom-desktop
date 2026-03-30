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
    pub rtk: RtkRuntimeStatus,
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
pub struct HeadroomLearnApiKeyStatus {
    pub has_api_key: bool,
    pub provider: Option<String>,
    pub source: Option<String>,
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
