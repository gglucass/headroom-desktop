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
    /// What is on disk when installed, falling back to the pinned version when
    /// it is not. Never report the pin for an installed addon: the card would
    /// claim a version the user does not have the moment a release bumps a pin.
    pub version: String,
    pub checksum: Option<String>,
    /// Short savings/usage line for the addon card chip ("12 docs converted").
    /// None when the addon has no measurable or citable figure.
    #[serde(default)]
    pub savings_label: Option<String>,
    /// Installed, but the app now pins a newer version. Drives the Update
    /// action on the card. False for addons whose upgrade is automatic (rtk,
    /// plugins) and for anything not installed.
    #[serde(default)]
    pub update_available: bool,
    /// The version an Update would move to. None when no update is pending.
    #[serde(default)]
    pub available_version: Option<String>,
    /// Why this addon cannot be installed on the current OS/arch, shown in
    /// place of an Install button that could only ever error. None when it is
    /// installable here.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// A plugin addon the user installed through the host CLI themselves (no
    /// Headroom receipt). The card shows it installed but offers no actions:
    /// adopting it would let Headroom's uninstall remove the user's plugin.
    #[serde(default)]
    pub managed_externally: bool,
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
    #[serde(alias = "resumed")]
    Resume,
    Dashboard,
}

/// Honestly-labelled output-token reduction estimate surfaced from the proxy's
/// `/stats`. `method` is "estimated" (synthetic control vs a learned baseline)
/// or "measured" (A/B holdout); the percentage carries a 95% confidence band
/// (`ci_low_percent`..`ci_high_percent`). Output savings are counterfactual, so
/// this is never presented as an exact count.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputReduction {
    pub method: String,
    pub reduction_percent: f64,
    pub ci_low_percent: f64,
    pub ci_high_percent: f64,
    pub requests: u64,
    /// `requests` as a share of every shaped request. `None` when the figure
    /// came from the backend's `/stats` rather than the local ledger recompute,
    /// which is the only place the denominator is visible.
    pub coverage_percent: Option<f64>,
    /// False when that share is too thin for the percentage to describe the
    /// machine (`output_savings::OutputEstimate::covers_enough`). The tile and
    /// the server report withhold the percentage; the counters above still
    /// travel, so the floor can be tuned against fleet data rather than guessed
    /// at twice. The threshold lives in one place, not in every caller.
    pub publishable: bool,
}

/// Auto-learning progress from the backend's `/stats` `traffic_learner` block.
/// Patterns need `min_evidence` sightings before they're saved, so early on
/// the learner has nothing to show; `pending_patterns` lets the Optimize view
/// prove learning is alive during that window. Backends without the block
/// (< the wheel carrying headroomlabs-ai/headroom#3104) leave this None.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnerProgress {
    pub pending_patterns: u64,
    pub min_evidence: u64,
    pub patterns_saved: u64,
}

/// Lifetime savings decomposition parsed from the backend's `/stats-history`
/// `lifetime` block. Powers the "How savings are calculated" drill-down.
/// `cache_savings_usd` is the provider cache discount earned by the *client's*
/// own prompt caching — deliberately never summed into any Headroom savings
/// figure (Headroom preserves that discount; it does not cause it).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SavingsBreakdown {
    pub compression_savings_usd: f64,
    pub output_savings_usd: f64,
    /// Tool-definition tokens the proxy deferred out of the request, priced at
    /// the provider's cache-read rate rather than the full input rate: tool
    /// schemas sit at the front of the cached prefix, so what they actually
    /// cost on a repeat request is a cache read.
    pub tool_schema_savings_usd: f64,
    pub tool_schema_tokens_saved: u64,
    pub cache_savings_usd: f64,
    pub cache_read_tokens: u64,
    pub total_input_tokens: u64,
    pub total_input_cost_usd: f64,
    /// Per-model compression rates, best first. Empty on backends that predate
    /// `by_model` tracking.
    pub model_rates: Vec<ModelSavingsRate>,
}

/// One row of the backend's `/stats-history` `by_model` block.
///
/// Only the rate travels, never the dollars: `by_model` started being tracked
/// long after the lifetime counters, so its totals cover a fraction of lifetime
/// history and would visibly fail to add up next to the rows above it. A rate
/// stays meaningful on a partial sample; a dollar total does not.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelSavingsRate {
    pub model: String,
    pub requests: u64,
    pub savings_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailySavingsPoint {
    pub date: String,
    pub estimated_savings_usd: f64,
    pub estimated_tokens_saved: u64,
    pub actual_cost_usd: f64,
    pub total_tokens_sent: u64,
    /// Input that newly entered context inside the bucket (provider-billed
    /// uncached + cache-write tokens), sampled locally from the proxy's
    /// cumulative counters. This is the denominator compression had any power
    /// over: the re-sent cached prefix in `total_tokens_sent` is deliberately
    /// never rewritten, and counting it drove the displayed input rate toward
    /// zero as sessions grew (2026-09-02 fleet analysis). Zero = no coverage:
    /// backend rollups and buckets from older builds have no new-input
    /// dimension, and the rate skips them rather than mix denominators.
    #[serde(default)]
    pub new_input_tokens: u64,
    /// Tool-schema deferral for the bucket, priced at the CACHE-READ rate.
    /// Deferral is real Headroom-caused saving (those definitions are re-sent
    /// on every request unless Headroom holds them back), unlike the provider
    /// cache, which works with Headroom out of the path entirely. But the
    /// tokens sit at the front of the prompt and would have been cache reads
    /// after the first request, so pricing them at full input rate is the
    /// 0.36.0 contamination the unfold guard exists to block.
    ///
    /// Zero for every bucket before per-bucket sampling began (2026-09-02):
    /// the backend only ever exposed a lifetime cumulative counter, so there
    /// is nothing to backfill. Old bars therefore understate this layer.
    #[serde(default)]
    pub tool_schema_savings_usd: f64,
    #[serde(default)]
    pub tool_schema_tokens_saved: u64,
    /// Output-shaping savings for the bucket, kept separate from the
    /// compression figures above because it is a counterfactual estimate
    /// (synthetic control vs a learned baseline) rather than a measured diff.
    /// Zero for buckets from the local tracker, which has no output dimension.
    #[serde(default)]
    pub output_savings_usd: f64,
    #[serde(default)]
    pub output_tokens_saved: u64,
    /// Provider prompt-cache reads inside the bucket, derived from consecutive
    /// cumulative checkpoints in the backend's raw `history` array (the rollup
    /// series itself has no cache dimension). UTC-bucketed like the rollups.
    /// The local tracker archives the derived value at ingest, so coverage
    /// survives the backend's history-ring trimming. None only for buckets
    /// observed solely by the local tracker or archived before the field
    /// existed (pre-0.8.3 days are unrecoverable: their checkpoints are gone).
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    /// The provider read discount earned in the bucket, same derivation and
    /// coverage as `cache_read_tokens`.
    #[serde(default)]
    pub cache_savings_usd: Option<f64>,
    /// What the bucket's cache reads actually cost, priced per request by the
    /// backend rollup with the same function that priced `actual_cost_usd`.
    /// When present, the three cache fields all come from that rollup. None on
    /// backends without it and for buckets archived before it; consumers then
    /// fall back to `cache_savings_usd / 9`, which assumes reads bill at 0.1x
    /// and overstates the read cost on models with a steeper discount
    /// (claude-fable-5-1 reads bill at 0.025x).
    #[serde(default)]
    pub cache_read_cost_usd: Option<f64>,
    /// Locally-sampled output-shaper deltas for the bucket (poll-over-poll
    /// diffs of the estimator's durable cumulative counters, attributed to the
    /// sampling moment). None for buckets without samples: periods before this
    /// build, or while the app wasn't running. Window reduction = saved /
    /// baseline over the covered buckets.
    #[serde(default)]
    pub output_sampled_tokens_saved: Option<u64>,
    #[serde(default)]
    pub output_baseline_tokens: Option<u64>,
}

/// Per-provider (anthropic / openai / unknown) attribution for a single hourly
/// bucket, sourced from the `by_provider` map added to `/stats-history` rollups
/// upstream. Surfaced only in the hourly history-chart hover; empty for buckets
/// that predate the upstream feature (local-tracker hours before the cutoff).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSavingsPoint {
    pub provider: String,
    pub estimated_savings_usd: f64,
    pub estimated_tokens_saved: u64,
    pub actual_cost_usd: f64,
    pub total_tokens_sent: u64,
    /// This provider's own read discount and read cost inside the bucket,
    /// from the backend rollup. With them the hover can take cache reads out
    /// of each provider's spend exactly, instead of applying the bucket's
    /// blended ratio to every provider. None on backends without the fields.
    #[serde(default)]
    pub cache_savings_usd: Option<f64>,
    #[serde(default)]
    pub cache_read_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlySavingsPoint {
    pub hour: String,
    pub estimated_savings_usd: f64,
    pub estimated_tokens_saved: u64,
    pub actual_cost_usd: f64,
    pub total_tokens_sent: u64,
    /// See `DailySavingsPoint::new_input_tokens`.
    #[serde(default)]
    pub new_input_tokens: u64,
    /// See `DailySavingsPoint::tool_schema_savings_usd`.
    #[serde(default)]
    pub tool_schema_savings_usd: f64,
    #[serde(default)]
    pub tool_schema_tokens_saved: u64,
    /// See `DailySavingsPoint::output_savings_usd`.
    #[serde(default)]
    pub output_savings_usd: f64,
    #[serde(default)]
    pub output_tokens_saved: u64,
    /// See `DailySavingsPoint::cache_read_tokens`.
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_savings_usd: Option<f64>,
    #[serde(default)]
    pub cache_read_cost_usd: Option<f64>,
    /// See `DailySavingsPoint::output_sampled_tokens_saved`.
    #[serde(default)]
    pub output_sampled_tokens_saved: Option<u64>,
    #[serde(default)]
    pub output_baseline_tokens: Option<u64>,
    #[serde(default)]
    pub by_provider: Vec<ProviderSavingsPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardState {
    pub app_version: String,
    pub launch_experience: LaunchExperience,
    pub bootstrap_complete: bool,
    pub python_runtime_installed: bool,
    pub lifetime_requests: usize,
    /// This process has forwarded a prompt-sized completion request (the
    /// `first_prompt_request` funnel signal). Drives the post-install
    /// checklist's "first prompt sent" row.
    pub first_prompt_request_seen: bool,
    pub lifetime_estimated_savings_usd: f64,
    pub lifetime_estimated_tokens_saved: u64,
    pub session_requests: usize,
    pub session_estimated_savings_usd: f64,
    pub session_estimated_tokens_saved: u64,
    pub session_savings_pct: f64,
    /// Counterfactual output-token reduction from the proxy's output shaper.
    /// `None` until a verbosity baseline is seeded (the dashboard hides the stat
    /// until then). Always honestly labelled (`method` + confidence band).
    pub output_reduction: Option<OutputReduction>,
    /// Whether the running wheel's rollout gate actually enabled the output
    /// shaper. `Some(false)` means blocked (e.g. blocked_by_channel on stable
    /// with the 0.37.0 wheel): the reduction above then describes an inactive
    /// feature and must not be reported to the server as a live one. `None`
    /// on wheels that predate the rollout block.
    #[serde(default)]
    pub output_shaper_active: Option<bool>,
    /// Auto-learning progress; `None` when the backend doesn't report it.
    #[serde(default)]
    pub learner_progress: Option<LearnerProgress>,
    /// Retrieval-churn gauges from `/stats`: tokens re-read by the client
    /// (total, and the subset that had been compressed away) plus explicit
    /// CCR retrieve hits. Latest observed values; `None` while the backend is
    /// unreachable or predates the counters.
    #[serde(default)]
    pub reread_tokens: Option<u64>,
    #[serde(default)]
    pub reread_compressed_tokens: Option<u64>,
    #[serde(default)]
    pub ccr_retrievals: Option<u64>,
    /// Lifetime decomposition behind the headline savings card. `None` until
    /// the backend's `/stats-history` has been fetched at least once.
    pub savings_breakdown: Option<SavingsBreakdown>,
    pub daily_savings: Vec<DailySavingsPoint>,
    pub hourly_savings: Vec<HourlySavingsPoint>,
    /// True once native savings history has loaded at least once this process.
    /// Until then the Home chart shows a loading state instead of the sparse
    /// tracker-only layer.
    pub savings_history_loaded: bool,
    pub tools: Vec<ManagedTool>,
    pub clients: Vec<ClientStatus>,
    pub recent_usage: Vec<UsageEvent>,
    /// Terms-of-Service version the app currently requires the user to accept.
    pub required_terms_version: u32,
    /// Highest terms version this user has already accepted (0 = none).
    pub accepted_terms_version: u32,
    /// Canonical Terms-of-Service URL the acceptance gate links to.
    pub terms_url: String,
}

/// Why the last bootstrap failed, in the two forms a support report needs:
/// the stable cause class (same vocabulary as the `failure_kind` Sentry tag,
/// so a mail can be matched to its issue) and the compact technical detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapFailureReport {
    pub kind: String,
    /// Pip's stderr tail, or the whole error chain when the command never ran.
    pub detail: String,
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
    // True when a shell profile (e.g. ~/.zshrc) couldn't be written because it
    // isn't writable. Core routing still works via the client's own config, so
    // this is a soft, expected degradation -- callers use it to avoid alerting.
    #[serde(default)]
    pub shell_profile_unwritable: bool,
    /// A pre-existing custom base URL (corporate gateway/proxy) that this
    /// setup replaced. The UI must tell the user their routing changed and
    /// that the original is restored on disable.
    #[serde(default)]
    pub replaced_base_url: Option<String>,
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
    #[serde(default)]
    pub verification: Option<ClientSetupVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtkRuntimeStatus {
    pub installed: bool,
    /// User-facing on/off state from the tool status toggle. False means the
    /// user opted RTK out; integrations are torn down and stay off.
    pub enabled: bool,
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
    /// True when the watchdog auto-paused after giving up on a wedged proxy,
    /// distinct from a deliberate user pause. Drives the "stopped unexpectedly"
    /// banner + Resume button.
    pub auto_paused: bool,
    /// True when the proxy is intentionally bypassed (pricing gate on an
    /// unentitled account, or watchdog give-up). The backend is deliberately
    /// not started, so `running` will never become true — the first-run screen
    /// treats this as a terminal state and lets the user into the app.
    pub bypassed: bool,
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
    /// Prose hint while the backend is failing certificate verification against
    /// the provider (TLS-inspecting network); `None` once the failures age out.
    #[serde(default)]
    pub upstream_tls_interception_hint: Option<String>,
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
    // Count of this project's session JSONL files whose mtime falls within the
    // current UTC day. Used by the learnings tile to pick the "most active
    // today" project without rescanning session files a second time.
    pub sessions_today: usize,
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
    #[serde(default)]
    pub current_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomLearnPrereqStatus {
    pub claude_cli_available: bool,
    pub claude_cli_path: Option<String>,
    pub codex_cli_available: bool,
    pub codex_cli_path: Option<String>,
    pub codex_logged_in: bool,
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
    // Per-request prefix-cache split, from the feed (#3672, native since
    // wheel 0.39.0). uncached + cache_write is the "new input" denominator the
    // overview rate uses (state.rs session_savings_pct, dashboardHelpers
    // newInputSavingsRate); see `apply_new_input_basis`. Absent on older wheels.
    #[serde(default, alias = "uncached_input_tokens")]
    pub uncached_input_tokens: Option<u64>,
    #[serde(default, alias = "cache_write_tokens")]
    pub cache_write_tokens: Option<u64>,
    // The feed's request_messages / compressed_messages / response_content
    // bodies are not modelled: the fetch asks the backend to omit them
    // (`include_messages=0`, lib.rs) and serde ignores them when an older
    // backend or a persisted activity-facts.json still carries them.
}

impl TransformationFeedEvent {
    /// Puts `savings_percent` on the NEW-INPUT basis every other displayed
    /// input-savings figure uses (invariant set 2026-09-03; the formula is
    /// `newInputSavingsRate` in dashboardHelpers.ts): saved / (saved +
    /// uncached + cache_write). The feed's own `savings_percent` divides by
    /// the whole transcript, cached prefix included, so in a long agentic
    /// session a request the overview rates at 20%+ read as ~3% here and the
    /// "large compression" tile starved (stuck from 2026-09-10).
    ///
    /// The in/out pair moves with it, or the tile reads "66% on
    /// 183,904 -> 167,728": `input_tokens_original` becomes the overview's
    /// "Baseline" (new input plus what Headroom removed) and
    /// `input_tokens_optimized` the new input that reached the provider.
    /// Left as-is when the backend does not report the split; percent and
    /// pair are `None` when nothing new entered context, matching the chart,
    /// which skips such buckets.
    pub fn apply_new_input_basis(&mut self) {
        let (Some(uncached), Some(cache_write)) =
            (self.uncached_input_tokens, self.cache_write_tokens)
        else {
            return;
        };
        let new_input = uncached.saturating_add(cache_write);
        if new_input == 0 {
            self.savings_percent = None;
            self.input_tokens_original = None;
            self.input_tokens_optimized = None;
            return;
        }
        let saved = u64::try_from(self.tokens_saved.unwrap_or(0)).unwrap_or(0);
        let baseline = saved.saturating_add(new_input);
        self.savings_percent = Some((saved as f64 / baseline as f64 * 100.0).min(100.0));
        self.input_tokens_original = Some(baseline);
        self.input_tokens_optimized = Some(new_input);
    }
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
pub struct RtkTodayStats {
    pub date: String,
    pub saved_tokens: u64,
    pub commands: u64,
}

/// Serena activity for the feed tile. Lines are pre-formatted by the same
/// code as the Addons-tab chip (`serena_savings_parts`) so the two surfaces
/// can never phrase the same numbers differently. At least one line is Some.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerenaTodayStats {
    pub calls_line: Option<String>,
    pub tokens_line: Option<String>,
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
    // Pass-through from the source transformation so the record card can show
    // the same "Tokens in → out" pair as the compression card.
    #[serde(default, alias = "input_tokens_original")]
    pub input_tokens_original: Option<u64>,
    #[serde(default, alias = "input_tokens_optimized")]
    pub input_tokens_optimized: Option<u64>,
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

// `serde(default)` on every field so a pre-v5 `activity-facts.json` (which
// had a different shape — `count`, `kind`) still deserializes via its default
// values; the SCHEMA_VERSION mismatch then drops the file and reinitialises
// from scratch. Without the defaults, the outer parse fails before we can
// reach the version check and the app panics at boot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct LearningsMilestoneEvent {
    #[serde(default = "default_observed_at")]
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub patterns_today: u32,
    #[serde(default)]
    pub reminders_today: u32,
    #[serde(default)]
    pub learnings_today: u32,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub project_display_name: Option<String>,
}

fn default_observed_at() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_else(Utc::now)
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
    pub rtk_today: Option<RtkTodayStats>,
    pub serena_today: Option<SerenaTodayStats>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudePlanTier {
    Free,
    Pro,
    Max5x,
    Max20x,
    /// Anthropic API console org (`api_individual` and friends): pay-per-token
    /// billing, no Claude subscription plan to mirror.
    Api,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadroomSubscriptionTier {
    Pro,
    Max5x,
    Max20x,
}

impl HeadroomSubscriptionTier {
    pub fn rank(self) -> u8 {
        match self {
            HeadroomSubscriptionTier::Pro => 1,
            HeadroomSubscriptionTier::Max5x => 2,
            HeadroomSubscriptionTier::Max20x => 3,
        }
    }
}

/// The Headroom subscription tier that matches a detected Claude plan. Unknown
/// maps to Max x20 (these are paying org customers whose taxonomy we couldn't
/// decode, so pitch the top plan rather than under-recommend). Free carries no
/// paid Headroom equivalent.
pub fn headroom_tier_for_claude_plan(plan: &ClaudePlanTier) -> Option<HeadroomSubscriptionTier> {
    match plan {
        ClaudePlanTier::Pro => Some(HeadroomSubscriptionTier::Pro),
        ClaudePlanTier::Max5x => Some(HeadroomSubscriptionTier::Max5x),
        ClaudePlanTier::Max20x => Some(HeadroomSubscriptionTier::Max20x),
        // Undecodable plan: don't drive a confident upsell off a guess. Returning
        // None lets a known Codex plan supply the recommendation instead (and
        // fires no mismatch when Claude is the only signal). Honors the contract
        // in `pricing::detect_tier_mismatch`. The pricing gate's separate
        // Unknown -> Max x20 *threshold* fallback is unaffected.
        ClaudePlanTier::Unknown => None,
        // API-billed org: no subscription to mirror. The server's usage-band
        // pitch (account.recommended_tier, computed from user_daily_savings
        // with the >=5:1 savings ROI clamp) drives the recommendation; a local
        // guess would overpitch light API users. None also keeps
        // detect_tier_mismatch quiet, same contract as Unknown.
        ClaudePlanTier::Api => None,
        ClaudePlanTier::Free => None,
    }
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
    CodexWeeklyUsageLimitReached,
    TrialEnded,
}

/// The OpenAI/ChatGPT plan behind a Codex session, decoded best-effort from the
/// `chatgpt_plan_type` claim in the Codex OAuth bearer JWT
/// (`proxy_intercept::decode_codex_plan_tier`). Drives the Codex upgrade nudge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexPlanTier {
    Free,
    Go,
    Plus,
    /// ChatGPT Pro Lite, the ~$100/mo mid-tier between Plus and Pro. Seen in
    /// the fleet from 2026-08 (caught by the `plan_raw` passthrough) before
    /// OpenAI announced it; any other spelling still falls through to
    /// `Unknown` and rides out as a raw claim.
    ProLite,
    Pro,
    Team,
    Business,
    SelfServeBusinessUsageBased,
    /// A self-serve Business workspace whose seats sit on the Pro Lite level.
    /// Seen in the fleet from 2026-06, classified 2026-09-07 after the raw
    /// claim showed up unmapped.
    SelfServeBusinessProlite,
    Enterprise,
    EnterpriseCbpUsageBased,
    Edu,
    /// Second spelling of the ChatGPT Edu claim, minted alongside `edu` and
    /// first seen 2026-09-06. Same plan, same price parity.
    Education,
    Unknown,
}

impl CodexPlanTier {
    /// Parse the raw `chatgpt_plan_type` claim value.
    pub fn from_claim(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "free" => CodexPlanTier::Free,
            "go" => CodexPlanTier::Go,
            "plus" => CodexPlanTier::Plus,
            "prolite" => CodexPlanTier::ProLite,
            "pro" => CodexPlanTier::Pro,
            "team" => CodexPlanTier::Team,
            "business" => CodexPlanTier::Business,
            "self_serve_business_usage_based" => CodexPlanTier::SelfServeBusinessUsageBased,
            "self_serve_business_prolite" => CodexPlanTier::SelfServeBusinessProlite,
            "enterprise" => CodexPlanTier::Enterprise,
            "enterprise_cbp_usage_based" => CodexPlanTier::EnterpriseCbpUsageBased,
            "edu" => CodexPlanTier::Edu,
            "education" => CodexPlanTier::Education,
            _ => CodexPlanTier::Unknown,
        }
    }

    /// Stable wire value for the `X-Headroom-Codex-Plan` header, mirroring
    /// `pricing::plan_tier_header_value` for Claude. Kept in sync with the
    /// server's `TrialIdentity::CODEX_PLAN_TIERS`.
    pub fn as_header_str(&self) -> &'static str {
        match self {
            CodexPlanTier::Free => "free",
            CodexPlanTier::Go => "go",
            CodexPlanTier::Plus => "plus",
            CodexPlanTier::ProLite => "prolite",
            CodexPlanTier::Pro => "pro",
            CodexPlanTier::Team => "team",
            CodexPlanTier::Business => "business",
            CodexPlanTier::SelfServeBusinessUsageBased => "self_serve_business_usage_based",
            CodexPlanTier::SelfServeBusinessProlite => "self_serve_business_prolite",
            CodexPlanTier::Enterprise => "enterprise",
            CodexPlanTier::EnterpriseCbpUsageBased => "enterprise_cbp_usage_based",
            CodexPlanTier::Edu => "edu",
            CodexPlanTier::Education => "education",
            CodexPlanTier::Unknown => "unknown",
        }
    }
}

/// Price-parity map from an OpenAI plan to the recommended Headroom upgrade
/// tier, by per-seat Codex allowance:
/// - Go ($8) / Plus ($20) -> Pro: individual, low spend.
/// - Business / Team -> Pro: a Standard Business seat ($20-25) carries a
///   Plus-level Codex allowance, so Pro is honest parity. (Team is legacy
///   Business, folded into Business by OpenAI.) The earlier one-tier "org
///   budget" bump to Max x5 read as a wrong plan detection to Business users
///   and was reverted 2026-08-18. Note this is intentionally NOT parity with
///   Claude Team (-> Max x20): a Claude Team seat grants Max-tier limits.
///   If/when Business Premium seats ($100, 5x) ship a distinct plan claim,
///   map that claim to Max x5/x20 -- do not re-bump Standard seats.
/// - Self-serve usage-based -> Max x5: credit-billed Codex seats with no fixed
///   seat price; spend is open-ended, so pitch the middle tier.
/// - Pro ($100/$200) -> Max x20: individual already paying top dollar.
/// - Enterprise / enterprise CBP usage-based -> Max x20: $60+/seat at a 150-seat
///   minimum, the genuine high-budget tier.
/// - Edu / Education -> Max x5: institutional but discounted. OpenAI mints
///   both spellings for the same plan.
/// - Self-serve Business Pro Lite -> Max x5: a Business workspace on Pro Lite
///   seats, and both halves of that name already price at Max x5.
/// - Unknown -> Max x20: plan claim couldn't be decoded, so pitch the top plan
///   rather than under-recommend.
///
/// Free carries no recommendation (already on the no-cost tier).
pub fn headroom_tier_for_codex_plan(plan: &CodexPlanTier) -> Option<HeadroomSubscriptionTier> {
    match plan {
        CodexPlanTier::Go | CodexPlanTier::Plus | CodexPlanTier::Team | CodexPlanTier::Business => {
            Some(HeadroomSubscriptionTier::Pro)
        }
        CodexPlanTier::ProLite
        | CodexPlanTier::SelfServeBusinessUsageBased
        | CodexPlanTier::SelfServeBusinessProlite
        | CodexPlanTier::Edu
        | CodexPlanTier::Education => Some(HeadroomSubscriptionTier::Max5x),
        CodexPlanTier::Pro
        | CodexPlanTier::Enterprise
        | CodexPlanTier::EnterpriseCbpUsageBased
        | CodexPlanTier::Unknown => Some(HeadroomSubscriptionTier::Max20x),
        CodexPlanTier::Free => None,
    }
}

/// Codex (OpenAI/ChatGPT) account identity, the Codex analog of
/// [`ClaudeAccountProfile`]. `plan_tier` + `account_uuid` are available from the
/// live access-token bearer the intercept proxy sees; `email` and
/// `organization_type` only exist in the on-disk `~/.codex/auth.json` id_token,
/// so they require reading that file (see `pricing::detect_codex_profile`). All
/// fields ride along to headroom-web on the `X-Headroom-Codex-*` identity
/// headers, mirroring the Claude fields one-for-one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAccountProfile {
    pub email: Option<String>,
    /// `chatgpt_account_id` from the OAuth JWT (or `tokens.account_id`).
    pub account_uuid: Option<String>,
    pub plan_tier: Option<CodexPlanTier>,
    /// Sanitized raw `chatgpt_plan_type` claim, kept ONLY when it doesn't
    /// decode to a known [`CodexPlanTier`]. Rides to headroom-web as the
    /// `X-Headroom-Codex-Plan` value in place of `"unknown"`, so a new OpenAI
    /// plan (e.g. Business Premium seats) shows up in the fleet by name the
    /// day it ships instead of vanishing into the `unknown` bucket.
    #[serde(default)]
    pub plan_raw: Option<String>,
    /// Raw org signal: the user's `role` in their default org
    /// (`organizations[0].role`, e.g. owner/admin/member). Present for
    /// Business/Enterprise/Team seats. No Codex analog to Claude's
    /// `organization_type` taxonomy string exists, so role is the raw value.
    pub organization_type: Option<String>,
    /// Reserved: Codex exposes no rate-limit-tier claim today.
    pub rate_limit_tier: Option<String>,
    /// Derived: `None` for free/unknown, `"subscription"` for paid personal
    /// plans, the plan string (`"business"`/`"enterprise"`) when an org seat.
    pub billing_type: Option<String>,
    /// Where `plan_tier` came from (`"id_token"`, `"access_token"`, `"none"`),
    /// for server-side auditing of sparse captures.
    pub plan_detection_source: Option<String>,
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
    /// Raw `organization_type` from the OAuth profile, kept verbatim so the
    /// server can audit which taxonomy strings we haven't enumerated yet
    /// (specifically when `plan_tier` ends up `Unknown`).
    pub organization_type: Option<String>,
    /// Raw `rate_limit_tier` — same purpose as `organization_type`.
    pub rate_limit_tier: Option<String>,
    /// Raw per-user `user_rate_limit_tier`. On Team/Enterprise orgs the
    /// org-level `rate_limit_tier` (e.g. "raven") can't distinguish standard
    /// from premium seats; this field carries the seat-level limits.
    #[serde(default)]
    pub user_rate_limit_tier: Option<String>,
    /// Raw `seat_tier` — Anthropic's per-seat entitlement on Team/Enterprise
    /// orgs. Same audit purpose as `user_rate_limit_tier`.
    #[serde(default)]
    pub seat_tier: Option<String>,
    pub weekly_utilization_pct: Option<f64>,
    /// When the Claude seven-day usage window resets (RFC3339). Drives the
    /// "savings you'll miss before reset" counterfactual on the weekly gate.
    pub weekly_resets_at: Option<DateTime<Utc>>,
    pub five_hour_utilization_pct: Option<f64>,
    pub extra_usage_monthly_limit: Option<f64>,
    pub profile_fetch_error: Option<String>,
}

/// A single Codex (OpenAI) rate-limit window, sourced from the `x-codex-*`
/// response headers our intercept proxy captures off live Codex traffic
/// (`proxy_intercept::parse_codex_rate_limit_headers`). Windows are labeled by
/// minute count (e.g. "5h", "7d") derived the same way upstream does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexUsageWindow {
    pub used_percent: f64,
    pub window_label: Option<String>,
    pub window_minutes: Option<i64>,
    pub seconds_until_reset: Option<i64>,
}

/// Codex subscription usage surfaced alongside the Claude profile in the pricing
/// status. Present only when the Codex connector is enabled and at least one
/// Codex response with rate-limit headers has flowed through the proxy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexUsage {
    pub limit_name: Option<String>,
    pub primary: Option<CodexUsageWindow>,
    pub secondary: Option<CodexUsageWindow>,
    pub credits_balance: Option<String>,
    pub credits_unlimited: bool,
    /// True while Codex optimization is permitted. Flips false once weekly
    /// (secondary-window) usage reaches the disable threshold on a free
    /// Headroom account — the Codex-only parallel to the Claude gate.
    pub optimization_allowed: bool,
    /// True once weekly usage crosses the soft nudge threshold.
    pub should_nudge: bool,
    /// Number of nudge thresholds crossed (0..=3), mirroring the Claude gate.
    pub nudge_level: u8,
    /// Set when the gate pauses Codex optimization.
    pub gate_reason: Option<PricingGateReason>,
    /// Headroom tier to recommend, derived from the detected OpenAI plan.
    pub recommended_subscription_tier: Option<HeadroomSubscriptionTier>,
    /// Utilization of the window the gate was actually evaluated against: the
    /// longest one the plan publishes, which is NOT necessarily `secondary`.
    /// Measured 2026-08-17, both free (30-day) and Plus (7-day) report their
    /// long window as `primary` with `secondary` null, so reading `secondary`
    /// here left this permanently null for most of the fleet.
    pub weekly_used_percent: Option<f64>,
    /// Seconds until that same window resets. Mirrors `claude.weekly_resets_at`
    /// so the forgone-savings upgrade copy has a horizon to multiply by.
    #[serde(default)]
    pub weekly_resets_in_seconds: Option<i64>,
    /// Display copy for the codex usage state (active / nudging / near-limit).
    pub gate_message: String,
    /// The tier-dependent nudge ladder the gate applied (10/15/20 for
    /// Max-like plans, 25/35/45 for Go/Plus). Drives notification copy so
    /// titles never hardcode a ladder the gate isn't using.
    #[serde(default)]
    pub effective_nudge_thresholds_percent: Vec<f64>,
    /// The tier-dependent pause threshold the gate applied.
    #[serde(default)]
    pub effective_disable_threshold_percent: f64,
}

/// Raw Codex rate-limit snapshot captured by the intercept proxy from the
/// `x-codex-*` response headers. Internal only (not serialized to the UI):
/// `pricing::fetch_codex_usage` reads the latest snapshot and derives the
/// display-facing `CodexUsage` (nudge state, gate copy) on the fly.
#[derive(Debug, Clone, Default)]
pub struct CodexRateLimitSnapshot {
    pub limit_name: Option<String>,
    pub primary: Option<CodexUsageWindow>,
    pub secondary: Option<CodexUsageWindow>,
    pub credits_balance: Option<String>,
    pub credits_unlimited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomAccountProfile {
    pub email: String,
    pub trial_started_at: Option<DateTime<Utc>>,
    pub trial_ends_at: Option<DateTime<Utc>>,
    /// Usage-day trials (web TrialPolicy::USAGE_SWITCH_AT onward): saving
    /// days left before the wall. None on calendar trials and old servers.
    #[serde(default)]
    pub trial_usage_days_left: Option<u32>,
    pub trial_active: bool,
    pub subscription_active: bool,
    pub subscription_tier: Option<HeadroomSubscriptionTier>,
    pub subscription_started_at: Option<DateTime<Utc>>,
    pub subscription_renews_at: Option<DateTime<Utc>>,
    pub subscription_amount_cents: Option<i64>,
    pub subscription_billing_period: Option<String>,
    pub subscription_discount_duration: Option<String>,
    pub subscription_discount_duration_in_months: Option<i64>,
    #[serde(default)]
    pub subscription_cancel_at_period_end: bool,
    #[serde(default)]
    pub subscription_ends_at: Option<DateTime<Utc>>,
    /// What the next renewal actually bills, per billing cycle, when the server
    /// knows it better than the client can derive it from the discount fields.
    /// Only a redeemed save offer fills these in today.
    #[serde(default)]
    pub subscription_renewal_cents: Option<i64>,
    #[serde(default)]
    pub subscription_renewal_ends_at: Option<DateTime<Utc>>,
    /// A downgrade scheduled for the next cycle. The subscription keeps
    /// reporting the plan being paid for until it lands, so these fields are
    /// the only sign the change exists.
    #[serde(default)]
    pub subscription_pending_tier: Option<HeadroomSubscriptionTier>,
    #[serde(default)]
    pub subscription_pending_billing_period: Option<String>,
    #[serde(default)]
    pub subscription_pending_effective_at: Option<DateTime<Utc>>,
    pub invite_code: Option<String>,
    pub accepted_invites_count: usize,
    pub invite_bonus_percent: f64,
    /// AppSumo-entitled accounts cannot change plan in place: there is no
    /// Polar subscription (or card) behind the entitlement. The server names
    /// the route that works - "appsumo" (their AppSumo account page) while
    /// the deal is live, "checkout" (fresh Polar checkout) afterwards.
    /// None for everyone else, keeping normal routing.
    #[serde(default)]
    pub upgrade_action: Option<String>,
    /// The AppSumo lifetime tier behind a Polar subscription they bought on
    /// top of it. Every tier at or below it is theirs for free: switching to
    /// one ends the Polar subscription at period end instead of billing it.
    /// None without both.
    #[serde(default)]
    pub appsumo_lifetime_tier: Option<HeadroomSubscriptionTier>,
    /// Server-computed pitch tier for API-billed orgs: their usage band mapped
    /// onto subscriber plans (>=5:1 savings ROI clamp), from
    /// user_daily_savings. None for everyone else - local recommendation
    /// logic applies unchanged.
    #[serde(default)]
    pub recommended_tier: Option<HeadroomSubscriptionTier>,
    // Early adopters whose earliest trial identity predates the paywall keep a
    // capped free tier instead of the post-trial hard block. serde default so
    // older cached payloads (no field) still deserialize.
    #[serde(default)]
    pub grandfathered: bool,
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
    pub nudge_level: u8,
    pub gate_reason: Option<PricingGateReason>,
    pub gate_message: String,
    pub nudge_threshold_percent: Option<f64>,
    pub effective_nudge_thresholds_percent: Option<Vec<f64>>,
    pub disable_threshold_percent: Option<f64>,
    pub effective_disable_threshold_percent: Option<f64>,
    pub recommended_subscription_tier: Option<HeadroomSubscriptionTier>,
    pub tier_mismatch: Option<TierMismatch>,
    pub claude: ClaudeAccountProfile,
    /// Codex subscription usage, populated only when the Codex connector is
    /// enabled and the backend has captured at least one rate-limit snapshot.
    #[serde(default)]
    pub codex: Option<CodexUsage>,
    /// ChatGPT plan decoded from the Codex OAuth JWT by the intercept proxy.
    /// Populated by `get_pricing_status` only; drives the paywall tier
    /// recommendation pre-purchase (`tier_mismatch` requires an active
    /// subscription, so it can't serve that role).
    #[serde(default)]
    pub codex_plan_tier: Option<CodexPlanTier>,
    pub account: Option<HeadroomAccountProfile>,
    pub launch_discount_active: bool,
    /// Percent off applied to the currently-selling founder-pricing cohort
    /// (0 when full price). Drives the discounted prices in the upgrade view.
    #[serde(default)]
    pub active_percent_off: i64,
    /// The founder-pricing ladder (founder -> early -> standard) rendered as a
    /// scarcity stepper. Empty when the server reports no ladder.
    #[serde(default)]
    pub pricing_cohorts: Vec<PricingCohort>,
    /// Slack-style intro offer (50% off the first `duration_months` months on
    /// every plan). `None` when the server doesn't advertise one.
    #[serde(default)]
    pub intro_offer: Option<IntroOffer>,
    /// Per-month list prices in cents, keyed tier -> billing period, served by
    /// headroom-web so a price change ships without an app release. `None`
    /// from servers predating the field, and the frontend then falls back to
    /// its compiled-in table.
    #[serde(default)]
    pub plan_prices: Option<PlanPrices>,
}

/// Per-month list prices in cents: tier ("pro" | "max5x" | "max20x") ->
/// billing period ("annual" | "monthly") -> cents. Deliberately untyped keys:
/// the desktop passes the table through to the frontend verbatim, so a new
/// tier added server-side needs no Rust change.
pub type PlanPrices = std::collections::HashMap<String, std::collections::HashMap<String, i64>>;

/// Intro-offer terms surfaced by headroom-web account/config payloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IntroOffer {
    pub active: bool,
    /// Percent off the first `duration_months` months, on MONTHLY plans only.
    pub percent_off: i64,
    /// Percent off the first yearly invoice. Not `percent_off` rescaled: since
    /// 2026-09 the two billing periods carry independent discounts (monthly
    /// 25% off three months, annual 12.5% off one invoice), so this cannot be
    /// derived from the monthly number. `None` from servers predating the
    /// split, and the frontend then falls back to
    /// `percent_off * duration_months / 12`, which is what the single percent
    /// always meant on a yearly invoice.
    ///
    /// f64, not i64: the annual percent is fractional (12.5). Dropping this
    /// field is not a visible error, it just silently prices annual off the
    /// fallback -- which is how a build shipped quoting 6.25%.
    pub annual_percent_off: Option<f64>,
    pub duration_months: i64,
}

/// One rung of the founder-pricing ladder, surfaced by headroom-web. `status`
/// is "sold_out" | "active" | "upcoming"; `spots_left` is set only for the
/// active, capacity-bound cohort.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingCohort {
    pub key: String,
    pub label: String,
    pub percent_off: i64,
    #[serde(default)]
    pub capacity: Option<i64>,
    pub status: String,
    #[serde(default)]
    pub spots_left: Option<i64>,
}

/// Which provider's detected plan drives a [`TierMismatch`] recommendation, so
/// the upgrade banner can name the right connector. `Both` when the Claude and
/// Codex plans imply the same recommended tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierRecommendationSource {
    Claude,
    Codex,
    Both,
}

/// Set when an active subscriber's paid Headroom tier is lower than the tier
/// implied by their detected Claude or Codex plan. `clamped` flips true once the
/// grace window has elapsed, at which point standard paid-plan usage gating
/// applies — scoped per product via the `*_undercovered` flags: a Codex-only
/// mismatch must never pause Claude optimization (and vice versa), since the
/// other product is exactly what the user is paying for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TierMismatch {
    pub paid_tier: HeadroomSubscriptionTier,
    pub recommended_tier: HeadroomSubscriptionTier,
    pub recommended_source: TierRecommendationSource,
    pub grace_ends_at: DateTime<Utc>,
    pub clamped: bool,
    /// True when the Claude-implied tier alone exceeds the paid tier; only then
    /// may the clamp gate Claude traffic.
    #[serde(default)]
    pub claude_undercovered: bool,
    /// True when the Codex-implied tier alone exceeds the paid tier; only then
    /// may the clamp meter Codex traffic.
    #[serde(default)]
    pub codex_undercovered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadroomAuthCodeRequest {
    pub email: String,
    pub expires_in_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_plan_tier_round_trips_as_snake_case() {
        for (tier, wire) in [
            (CodexPlanTier::Free, "free"),
            (CodexPlanTier::Plus, "plus"),
            (CodexPlanTier::Enterprise, "enterprise"),
            (CodexPlanTier::Education, "education"),
            (
                CodexPlanTier::SelfServeBusinessProlite,
                "self_serve_business_prolite",
            ),
            (CodexPlanTier::Unknown, "unknown"),
        ] {
            assert_eq!(serde_json::to_value(tier).unwrap(), json!(wire));
            let parsed: CodexPlanTier = serde_json::from_value(json!(wire)).unwrap();
            assert_eq!(parsed, tier);
        }
    }

    #[test]
    fn codex_plan_tier_from_claim_is_trimmed_case_insensitive_with_unknown_fallback() {
        assert_eq!(CodexPlanTier::from_claim("Plus"), CodexPlanTier::Plus);
        assert_eq!(CodexPlanTier::from_claim("  TEAM "), CodexPlanTier::Team);
        // ChatGPT Pro Lite: the claim OpenAI mints for the ~$100/mo mid-tier.
        assert_eq!(CodexPlanTier::from_claim("prolite"), CodexPlanTier::ProLite);
        assert_eq!(CodexPlanTier::ProLite.as_header_str(), "prolite");
        // Both spellings of ChatGPT Edu, and the Business/Pro Lite composite.
        assert_eq!(
            CodexPlanTier::from_claim("education"),
            CodexPlanTier::Education
        );
        assert_eq!(CodexPlanTier::Education.as_header_str(), "education");
        assert_eq!(
            CodexPlanTier::from_claim("self_serve_business_prolite"),
            CodexPlanTier::SelfServeBusinessProlite
        );
        assert_eq!(
            CodexPlanTier::SelfServeBusinessProlite.as_header_str(),
            "self_serve_business_prolite"
        );
        assert_eq!(
            CodexPlanTier::from_claim("chatgptpaidplan"),
            CodexPlanTier::Unknown
        );
        assert_eq!(CodexPlanTier::from_claim(""), CodexPlanTier::Unknown);
    }

    #[test]
    fn codex_plan_maps_to_price_parity_headroom_tier() {
        use HeadroomSubscriptionTier::*;
        // Business/Team: Standard seats carry a Plus-level Codex allowance, so
        // they price-match Pro (reverted from the Max x5 org bump 2026-08-18).
        for plan in [
            CodexPlanTier::Go,
            CodexPlanTier::Plus,
            CodexPlanTier::Team,
            CodexPlanTier::Business,
        ] {
            assert_eq!(headroom_tier_for_codex_plan(&plan), Some(Pro));
        }
        for plan in [
            CodexPlanTier::Pro,
            CodexPlanTier::Enterprise,
            CodexPlanTier::EnterpriseCbpUsageBased,
        ] {
            assert_eq!(headroom_tier_for_codex_plan(&plan), Some(Max20x));
        }
        for plan in [
            CodexPlanTier::ProLite,
            CodexPlanTier::SelfServeBusinessUsageBased,
            CodexPlanTier::SelfServeBusinessProlite,
            CodexPlanTier::Edu,
            CodexPlanTier::Education,
        ] {
            assert_eq!(headroom_tier_for_codex_plan(&plan), Some(Max5x));
        }
        assert_eq!(headroom_tier_for_codex_plan(&CodexPlanTier::Free), None);
        assert_eq!(
            headroom_tier_for_codex_plan(&CodexPlanTier::Unknown),
            Some(Max20x)
        );
    }

    #[test]
    fn codex_usage_window_deserializes_camel_case_keys() {
        let parsed: CodexUsageWindow = serde_json::from_value(json!({
            "usedPercent": 42.5,
            "windowLabel": "7d",
            "windowMinutes": 10080,
            "secondsUntilReset": 3600,
        }))
        .unwrap();
        assert_eq!(parsed.used_percent, 42.5);
        assert_eq!(parsed.window_label.as_deref(), Some("7d"));
        assert_eq!(parsed.window_minutes, Some(10080));
        assert_eq!(parsed.seconds_until_reset, Some(3600));
    }

    #[test]
    fn codex_usage_serializes_camel_case_and_round_trips() {
        let usage = CodexUsage {
            limit_name: Some("codex".into()),
            secondary: Some(CodexUsageWindow {
                used_percent: 80.0,
                window_label: Some("7d".into()),
                window_minutes: Some(10080),
                seconds_until_reset: None,
            }),
            optimization_allowed: true,
            should_nudge: true,
            nudge_level: 2,
            weekly_used_percent: Some(80.0),
            gate_message: "Approaching weekly limit".into(),
            ..Default::default()
        };

        let value = serde_json::to_value(&usage).unwrap();
        // Wire contract for the TS frontend: camelCase keys, not snake_case.
        for key in [
            "optimizationAllowed",
            "shouldNudge",
            "nudgeLevel",
            "gateMessage",
        ] {
            assert!(value.get(key).is_some(), "missing camelCase key {key}");
        }
        assert!(value.get("optimization_allowed").is_none());

        let back: CodexUsage = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), value);
        assert_eq!(back.nudge_level, 2);
        assert_eq!(back.secondary.unwrap().used_percent, 80.0);
        assert!(back.primary.is_none());
    }

    #[test]
    fn intro_offer_carries_the_annual_percent_through_to_the_frontend() {
        // The frontend prices the annual tab off annualPercentOff. serde drops
        // unknown fields, so a struct missing it deserializes fine and quietly
        // hands the frontend nothing -- which shipped an annual card quoting
        // 6.25% (the fallback) instead of the 12.5% Polar actually charges.
        let value = json!({
            "active": true,
            "percentOff": 25,
            "annualPercentOff": 12.5,
            "durationMonths": 3,
        });
        let offer: IntroOffer = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(offer.percent_off, 25);
        assert_eq!(offer.annual_percent_off, Some(12.5));
        assert_eq!(offer.duration_months, 3);
        // Round-trips back out unchanged: the desktop re-serializes this to the
        // webview, so a field that survives parsing but not emitting is the
        // same bug one layer along.
        assert_eq!(serde_json::to_value(&offer).unwrap(), value);
    }

    #[test]
    fn intro_offer_without_the_annual_percent_stays_deserializable() {
        // Servers predating the split send only percentOff; the frontend
        // derives the annual number itself in that case.
        let offer: IntroOffer = serde_json::from_value(json!({
            "active": true,
            "percentOff": 50,
            "durationMonths": 3,
        }))
        .unwrap();
        assert_eq!(offer.annual_percent_off, None);
        assert_eq!(offer.percent_off, 50);
    }

    #[test]
    fn pricing_cohort_defaults_optional_capacity_fields_when_absent() {
        // headroom-web omits capacity/spotsLeft for sold-out and upcoming rungs;
        // the #[serde(default)] contract must keep those payloads deserializable.
        let cohort: PricingCohort = serde_json::from_value(json!({
            "key": "founder",
            "label": "Founder",
            "percentOff": 40,
            "status": "active",
        }))
        .unwrap();
        assert_eq!(cohort.percent_off, 40);
        assert_eq!(cohort.status, "active");
        assert_eq!(cohort.capacity, None);
        assert_eq!(cohort.spots_left, None);
    }
}
