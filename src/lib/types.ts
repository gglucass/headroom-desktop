export type ToolStatus = "not_installed" | "installing" | "healthy" | "degraded";

export interface ManagedTool {
  id: string;
  name: string;
  description: string;
  runtime: "python";
  required: boolean;
  enabled: boolean;
  status: ToolStatus;
  sourceUrl: string;
  version: string;
  checksum?: string | null;
}

export interface PipelineStageMetric {
  stageId: string;
  stageName: string;
  applied: boolean;
  estimatedTokensSaved: number;
  addedLatencyMs: number;
  notes: string[];
}

export interface UsageEvent {
  id: string;
  timestamp: string;
  client: string;
  workspace: string;
  upstreamTarget: string;
  stages: PipelineStageMetric[];
  estimatedInputTokens: number;
  estimatedOutputTokens: number;
  estimatedCostSavingsUsd: number;
  latencyMs: number;
  outcome: "success" | "bypassed" | "error";
}

export interface DailyInsight {
  id: string;
  category: "savings" | "workflow" | "health";
  severity: "info" | "warning" | "critical";
  title: string;
  recommendation: string;
  evidence: string;
  relatedWorkspace?: string | null;
}

export interface ClientStatus {
  id: string;
  name: string;
  installed: boolean;
  configured: boolean;
  health: "healthy" | "attention" | "not_detected";
  notes: string[];
}

export type LaunchExperience = "first_run" | "resume" | "dashboard";

export interface DailySavingsPoint {
  date: string;
  estimatedSavingsUsd: number;
  estimatedTokensSaved: number;
  actualCostUsd: number;
  totalTokensSent: number;
}

export interface HourlySavingsPoint {
  hour: string;
  estimatedSavingsUsd: number;
  estimatedTokensSaved: number;
  actualCostUsd: number;
  totalTokensSent: number;
}

export interface DashboardState {
  appVersion: string;
  launchExperience: LaunchExperience;
  bootstrapComplete: boolean;
  pythonRuntimeInstalled: boolean;
  lifetimeRequests: number;
  lifetimeEstimatedSavingsUsd: number;
  lifetimeEstimatedTokensSaved: number;
  sessionRequests: number;
  sessionEstimatedSavingsUsd: number;
  sessionEstimatedTokensSaved: number;
  sessionSavingsPct: number;
  dailySavings: DailySavingsPoint[];
  hourlySavings: HourlySavingsPoint[];
  tools: ManagedTool[];
  clients: ClientStatus[];
  recentUsage: UsageEvent[];
  insights: DailyInsight[];
}

export interface BootstrapProgress {
  running: boolean;
  complete: boolean;
  failed: boolean;
  currentStep: string;
  message: string;
  currentStepEtaSeconds: number;
  overallPercent: number;
}

export interface ResearchCandidate {
  name: string;
  category: string;
  repository: string;
  runtime: string;
  license: string;
  localOnlyFit: string;
  installMethod: string;
  maintenance: string;
  decision: "include" | "defer" | "research";
  notes: string;
}

export interface ClientSetupResult {
  clientId: string;
  applied: boolean;
  alreadyConfigured: boolean;
  summary: string;
  changedFiles: string[];
  backupFiles: string[];
  nextSteps: string[];
  verification: ClientSetupVerification;
}

export interface ClientSetupVerification {
  clientId: string;
  verified: boolean;
  proxyReachable: boolean;
  checks: string[];
  failures: string[];
}

export interface ClientConnectorStatus {
  clientId: string;
  name: string;
  installed: boolean;
  enabled: boolean;
  verified: boolean;
  lastConfiguredAt?: string | null;
}

export interface RuntimeStatus {
  platform: string;
  supportTier: string;
  installed: boolean;
  running: boolean;
  starting: boolean;
  paused: boolean;
  proxyReachable: boolean;
  headroomPid?: number | null;
  mcpConfigured?: boolean | null;
  mcpError?: string | null;
  mlInstalled?: boolean | null;
  kompressEnabled?: boolean | null;
  headroomLearnSupported: boolean;
  headroomLearnDisabledReason?: string | null;
  startupError?: string | null;
  startupErrorHint?: string | null;
  runtimeUpgradeFailure?: RuntimeUpgradeFailure | null;
  rtk: {
    installed: boolean;
    version?: string | null;
    pathConfigured: boolean;
    hookConfigured: boolean;
    totalCommands?: number | null;
    totalSaved?: number | null;
    avgSavingsPct?: number | null;
  };
}

export interface RuntimeUpgradeProgress {
  running: boolean;
  complete: boolean;
  failed: boolean;
  currentStep: string;
  message: string;
  overallPercent: number;
  fromVersion?: string | null;
  toVersion?: string | null;
}

export type UpgradeFailurePhase = "install" | "boot_validation";

export interface RuntimeUpgradeFailure {
  appVersion: string;
  targetHeadroomVersion: string;
  fallbackHeadroomVersion?: string | null;
  failurePhase: UpgradeFailurePhase;
  attempts: number;
  firstAttemptAt: string;
  lastAttemptAt: string;
  errorMessage: string;
  errorHint?: string | null;
  rollbackRestored: boolean;
}

export interface AppUpdateConfiguration {
  enabled: boolean;
  currentVersion: string;
  endpointCount: number;
  configurationError?: string | null;
}

export interface AvailableAppUpdate {
  currentVersion: string;
  version: string;
  publishedAt?: string | null;
  notes?: string | null;
}

export interface ClaudeCodeProject {
  id: string;
  projectPath: string;
  displayName: string;
  lastWorkedAt: string;
  sessionCount: number;
  lastLearnRanAt: string | null;
  hasPersistedLearnings: boolean;
  activeDaysSinceLastLearn: number;
  lastLearnPatternCount: number | null;
}

export interface HeadroomLearnStatus {
  running: boolean;
  projectPath?: string | null;
  projectDisplayName?: string | null;
  startedAt?: string | null;
  finishedAt?: string | null;
  elapsedSeconds?: number | null;
  progressPercent: number;
  summary: string;
  success?: boolean | null;
  error?: string | null;
  lastRunAt?: string | null;
  outputTail: string[];
}

export interface HeadroomLearnPrereqStatus {
  claudeCliAvailable: boolean;
  claudeCliPath?: string | null;
}

// A single entry in `requestMessages`. Intentionally loose — the proxy passes
// through whatever shape the upstream provider uses (Anthropic: `content` is a
// string or structured blocks list; OpenAI: string-only). The UI extracts
// displayable text in `ActivityFeed.tsx`.
export interface TransformationRequestMessage {
  role?: string;
  content?: string | Array<{ type?: string; text?: string; [k: string]: unknown }>;
  [k: string]: unknown;
}

export interface TransformationFeedEvent {
  requestId?: string | null;
  timestamp?: string | null;
  provider?: string | null;
  model?: string | null;
  inputTokensOriginal?: number | null;
  inputTokensOptimized?: number | null;
  tokensSaved?: number | null;
  savingsPercent?: number | null;
  transformsApplied: string[];
  workspace?: string | null;
  turnId?: string | null;
  // Populated only when the proxy was started with `--log-messages` (or
  // `HEADROOM_LOG_MESSAGES=1`), reflected in
  // `TransformationFeedResponse.logFullMessages`. Both fields are pass-through
  // from the proxy's `RequestLogger` — the desktop renders them, it does not
  // reinterpret them.
  requestMessages?: TransformationRequestMessage[] | null;
  responseContent?: string | null;
}

export interface TransformationFeedResponse {
  logFullMessages: boolean;
  proxyReachable: boolean;
  transformations: TransformationFeedEvent[];
}

export interface MemoryFeedEvent {
  id: string;
  createdAt: string;
  scope: string;
  content: string;
  importance: number;
  evidenceCount: number;
  category?: string | null;
}

// Today's running totals of evidence>=2 patterns headroom has flushed to file.
// Resets at local midnight (the `day` field is the local YYYY-MM-DD the
// counts apply to). The activity tile uses this snapshot to render
// "X memories written to MEMORY.md and Y learnings written to CLAUDE.md".
export interface MemoryFlushEvent {
  observedAt: string;
  day: string;
  memoryMdCount: number;
  claudeMdCount: number;
}

export interface LiveLearning {
  id: string;
  content: string;
  category: string;
  importance: number;
  evidenceCount: number;
  createdAt: string;
}

export interface AppliedSection {
  title: string;
  bullets: string[];
}

export interface AppliedPatterns {
  claudeMd: AppliedSection[];
  memoryMd: AppliedSection[];
}

export interface RtkBatchEvent {
  observedAt: string;
  commandsDelta: number;
  tokensSavedDelta: number;
  totalCommands: number;
  totalSaved: number;
}

export type RecordTag = "daily" | "weekly" | "allTime";

export interface RecordEvent {
  observedAt: string;
  tags: RecordTag[];
  tokensSaved: number;
  savingsPercent: number | null;
  model: string | null;
  provider: string | null;
  requestId: string | null;
  previousRecord: number | null;
  day: string | null;
  workspace?: string | null;
  // Carried forward from the record-setting transformation so the record row
  // can surface the same request/response detail as the compression card.
  // Populated only when the proxy's `log_full_messages` is enabled.
  requestMessages?: TransformationRequestMessage[] | null;
  responseContent?: string | null;
}

export interface StreakEvent {
  observedAt: string;
  days: number;
  kind: string;
}

export interface SavingsMilestoneEvent {
  observedAt: string;
  milestoneUsd: number;
  kind: string;
}

export interface WeeklyRecapEvent {
  observedAt: string;
  weekStart: string;
  weekEnd: string;
  totalTokensSaved: number;
  totalSavingsUsd: number;
  activeDays: number;
}

export interface LearningsMilestoneEvent {
  observedAt: string;
  count: number;
  kind: string;
}

export interface TrainSuggestionEvent {
  observedAt: string;
  projectPath: string;
  projectDisplayName: string;
  sessionCount: number;
  activeDaysSinceLastLearn: number;
  // "never_trained" | "stale"
  kind: string;
}

export type ActivityEvent =
  | { kind: "transformation"; data: TransformationFeedEvent }
  | { kind: "rtkBatch"; data: RtkBatchEvent }
  | { kind: "record"; data: RecordEvent }
  | { kind: "streak"; data: StreakEvent }
  | { kind: "savingsMilestone"; data: SavingsMilestoneEvent }
  | { kind: "weeklyRecap"; data: WeeklyRecapEvent }
  | { kind: "learningsMilestone"; data: LearningsMilestoneEvent }
  | { kind: "trainSuggestion"; data: TrainSuggestionEvent }
  | { kind: "memoryFlush"; data: MemoryFlushEvent };

export interface ActivityFeedResponse {
  events: ActivityEvent[];
  logFullMessages: boolean;
  proxyReachable: boolean;
  memoryAvailable: boolean;
}

export type ClaudeAuthMethod = "claude_ai_oauth" | "api_key" | "unknown";

export type ClaudePlanTier = "free" | "pro" | "max5x" | "max20x" | "unknown";

export type HeadroomSubscriptionTier = "pro" | "max5x" | "max20x";

export type BillingPeriod = "annual" | "monthly";

export type PricingGateReason = "sign_in_required" | "weekly_usage_limit_reached";

export interface ClaudeAccountProfile {
  authMethod: ClaudeAuthMethod;
  email?: string | null;
  displayName?: string | null;
  accountUuid?: string | null;
  organizationUuid?: string | null;
  billingType?: string | null;
  accountCreatedAt?: string | null;
  subscriptionCreatedAt?: string | null;
  hasExtraUsageEnabled: boolean;
  planTier: ClaudePlanTier;
  planDetectionSource?: string | null;
  weeklyUtilizationPct?: number | null;
  fiveHourUtilizationPct?: number | null;
  extraUsageMonthlyLimit?: number | null;
  profileFetchError?: string | null;
}

export interface HeadroomAccountProfile {
  email: string;
  trialStartedAt?: string | null;
  trialEndsAt?: string | null;
  trialActive: boolean;
  subscriptionActive: boolean;
  subscriptionTier?: HeadroomSubscriptionTier | null;
  subscriptionStartedAt?: string | null;
  subscriptionRenewsAt?: string | null;
  subscriptionAmountCents?: number | null;
  subscriptionBillingPeriod?: string | null;
  subscriptionDiscountDuration?: string | null;
  subscriptionDiscountDurationInMonths?: number | null;
  inviteCode?: string | null;
  acceptedInvitesCount: number;
  inviteBonusPercent: number;
}

export interface HeadroomPricingStatus {
  authenticated: boolean;
  localGraceStartedAt: string;
  localGraceEndsAt: string;
  localGraceActive: boolean;
  accountSyncError?: string | null;
  needsAuthentication: boolean;
  optimizationAllowed: boolean;
  shouldNudge: boolean;
  gateReason?: PricingGateReason | null;
  gateMessage: string;
  nudgeThresholdPercent?: number | null;
  disableThresholdPercent?: number | null;
  effectiveDisableThresholdPercent?: number | null;
  recommendedSubscriptionTier?: HeadroomSubscriptionTier | null;
  recommendedSubscriptionPriceUsd?: number | null;
  claude: ClaudeAccountProfile;
  account?: HeadroomAccountProfile | null;
  launchDiscountActive: boolean;
}

export interface HeadroomAuthCodeRequest {
  email: string;
  expiresInSeconds: number;
}
