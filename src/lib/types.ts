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

export interface HeadroomLearnApiKeyStatus {
  hasApiKey: boolean;
  provider?: string | null;
  source?: string | null;
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
