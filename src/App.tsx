import {
  useEffect,
  useRef,
  useState,
  type ElementType,
  type FormEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent,
  type ReactElement,
  type ReactNode
} from "react";
import {
  Bell,
  Brain,
  CaretLeft,
  Cpu,
  CurrencyCircleDollar,
  CurrencyDollar,
  Info,
  EnvelopeSimple,
  GearSix,
  House,
  Heart,
  Key,
  SignOut,
  Sliders,
  Sparkle,
} from "@phosphor-icons/react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from "recharts";
import headroomLogo from "./assets/headroom-logo.svg";
import packageJson from "../package.json";
import {
  getAppUpdateInstallStatusCopy,
  getBlockedAppUpdateCheckPatch,
  loadAppUpdateConfiguration,
  runAppUpdateCheck,
  runAppUpdateInstall,
  sendAppUpdateNotification,
  shouldNotifyAboutAvailableAppUpdate,
  type AppUpdateStatePatch,
} from "./lib/appUpdate";
import { maybeFireTrialNotifications } from "./lib/trialNotifications";
import {
  describeInvokeError,
  getNextLowerUpgradePlanId,
  getUpgradePlans,
  upgradePlanIntentLabel,
  type PricingAudience,
  type UpgradePlanId
} from "./lib/appHelpers";
import {
  bootstrapFailureSignature,
  buildBootstrapFailureReport,
  buildBootstrapInvokeFailureReport,
  reportBootstrapFailure
} from "./lib/bootstrapSentry";
import {
  aggregateClientConnectors,
  addDays,
  addMonths,
  apiProviderLabel,
  buildHourlySavingsChartData,
  buildHourlySavingsWindow,
  buildMonthlySavingsChartData,
  buildMonthlySavingsWindow,
  compactNumber,
  currency,
  currencyExact,
  dayOfMonthTickFormatter,
  earliestHourlyDay,
  earliestSavingsMonth,
  findClientVerificationLogLine,
  formatDateTime,
  formatDayKey,
  formatLearnStatus,
  formatMonthLabel,
  formatSelectedDayLabel,
  hourOfDayTickFormatter,
  percent1,
  resolveApiProvider,
  sortClientConnectors,
  startOfDay,
  startOfMonth,
  type ApiProvider,
  type SavingsChartDatum
} from "./lib/dashboardHelpers";
import { mockDashboard } from "./lib/mockData";
import {
  cachePricingStatus,
  type CachedPricing,
  formatPercentValue,
  formatRemainingDays,
  subscriptionTierLabel
} from "./lib/pricing";
import { trackAnalyticsEvent, trackInstallMilestoneOnce } from "./lib/analytics";
import type {
  AppUpdateConfiguration,
  AvailableAppUpdate,
  BootstrapProgress,
  ClaudePlanTier,
  HeadroomAuthCodeRequest,
  HeadroomPricingStatus,
  ClaudeCodeProject,
  ClientConnectorStatus,
  ClientSetupResult,
  DailySavingsPoint,
  DashboardState,
  HeadroomLearnApiKeyStatus,
  HeadroomLearnStatus,
  HourlySavingsPoint,
  RuntimeStatus,
} from "./lib/types";

type TrayView =
  | "home"
  | "optimization"
  | "health"
  | "notifications"
  | "upgrade"
  | "upgradeAuth"
  | "settings";

interface NavItem {
  id: TrayView;
  label: string;
  icon: ElementType;
}

const navItems: NavItem[] = [
  { id: "home", label: "Home", icon: House },
  { id: "optimization", label: "Optimize", icon: Sliders },
  { id: "health", label: "Health", icon: Heart },
  { id: "notifications", label: "Activity", icon: Bell },
];

const connectorSetupDetails: Record<string, string> = {
  claude_code:
    "Headroom injects ANTHROPIC_BASE_URL into shell profiles and ~/.claude/settings.json so Claude Code connects through Headroom. Headroom also installs RTK, adds it to your shell PATH, and enables Claude Code auto-rewrite for bash commands."
};

const connectorSupportWarnings: Record<string, string> = {};

const connectorUnavailableReasons: Record<string, string> = {
  claude_code:
    "Claude Code was not detected. Install Claude Code and restart Headroom."
};

const launcherConnectorFallback: ClientConnectorStatus[] = [
  {
    clientId: "claude_code",
    name: "Claude Code",
    installed: false,
    enabled: false,
    verified: false
  }
];

const idleBootstrapProgress: BootstrapProgress = {
  running: false,
  complete: false,
  failed: false,
  currentStep: "Idle",
  message: "Installer has not started.",
  currentStepEtaSeconds: 0,
  overallPercent: 0
};

const idleHeadroomLearnStatus: HeadroomLearnStatus = {
  running: false,
  progressPercent: 0,
  summary: "Select a project to run headroom learn.",
  outputTail: []
};

const idleHeadroomLearnApiKeyStatus: HeadroomLearnApiKeyStatus = {
  hasApiKey: false,
  provider: null
};

interface ApiKeyGuide {
  providerLabel: string;
  keyUrl: string;
  billingUrl: string;
}

const apiKeyGuides: Record<ApiProvider, ApiKeyGuide> = {
  openai: {
    providerLabel: "OpenAI",
    keyUrl: "https://platform.openai.com/api-keys",
    billingUrl: "https://platform.openai.com/settings/organization/billing/overview"
  },
  anthropic: {
    providerLabel: "Claude",
    keyUrl: "https://console.anthropic.com/settings/keys",
    billingUrl: "https://console.anthropic.com/settings/billing"
  },
  gemini: {
    providerLabel: "Gemini (Google AI Studio)",
    keyUrl: "https://aistudio.google.com/app/apikey",
    billingUrl: "https://ai.google.dev/gemini-api/docs/billing"
  }
};

const SALES_CONTACT_URL = (
  import.meta.env.VITE_HEADROOM_SALES_CONTACT_URL ??
  ""
).trim() || "mailto:hello@example.com";
const CONTACT_FORM_URL = (
  import.meta.env.VITE_HEADROOM_CONTACT_FORM_URL ??
  ""
).trim();

type StartupPhase = "window" | "dashboard" | "bootstrap" | "runtime" | "ready";

const authCodeExpiryFallbackSeconds = 900;
const APP_UPDATE_BACKGROUND_INITIAL_DELAY_MS = 12_000;
const APP_UPDATE_BACKGROUND_CHECK_INTERVAL_MS = 60 * 60 * 1000;

async function loadDashboard(): Promise<DashboardState> {
  try {
    return await invoke<DashboardState>("get_dashboard_state");
  } catch {
    return mockDashboard;
  }
}

function SavingsChartTooltip({
  active,
  payload,
  chartMode
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: SavingsChartDatum }>;
  chartMode: SavingsChartMode;
}) {
  const point = payload?.[0]?.payload;
  if (!active || !point) {
    return null;
  }

  return (
    <div className="savings-chart__tooltip">
      <strong>{point.bucketLabel}</strong>
      {chartMode === "usd" ? (
        <div className="savings-chart__tooltip-group">
          <span className="savings-chart__tooltip-label">Dollars</span>
          <span className="savings-chart__tooltip-item">
            <i
              aria-hidden="true"
              className="savings-chart__tooltip-dot savings-chart__tooltip-dot--saved-usd"
            />
            Saved {currencyExact(point.estimatedSavingsUsd)}
          </span>
          <span className="savings-chart__tooltip-item">
            <i
              aria-hidden="true"
              className="savings-chart__tooltip-dot savings-chart__tooltip-dot--actual-usd"
            />
            Spent {currencyExact(point.actualCostUsd)}
          </span>
        </div>
      ) : (
        <div className="savings-chart__tooltip-group">
          <span className="savings-chart__tooltip-label">Tokens</span>
          <span className="savings-chart__tooltip-item">
            <i
              aria-hidden="true"
              className="savings-chart__tooltip-dot savings-chart__tooltip-dot--saved-tokens"
            />
            Saved {compactNumber(point.estimatedTokensSaved)} tokens
          </span>
          <span className="savings-chart__tooltip-item">
            <i
              aria-hidden="true"
              className="savings-chart__tooltip-dot savings-chart__tooltip-dot--actual-tokens"
            />
            Spent {compactNumber(point.totalTokensSent)} tokens
          </span>
        </div>
      )}
    </div>
  );
}

function delay(ms: number) {
  return new Promise<void>((resolve) => {
    window.setTimeout(resolve, ms);
  });
}

const PRICING_CACHE_KEY = "headroom.cachedPricing";
function readCachedPricing(): CachedPricing {
  try {
    const raw = localStorage.getItem(PRICING_CACHE_KEY);
    if (raw) return JSON.parse(raw) as CachedPricing;
  } catch {}
  return {};
}
function writeCachedPricing(pricing: CachedPricing) {
  try {
    localStorage.setItem(PRICING_CACHE_KEY, JSON.stringify(pricing));
  } catch {}
}

type SavingsChartView = "month" | "day";
type SavingsChartMode = "usd" | "tokens";

function DailySavingsChart({
  data,
  hourlyData,
  resetSignal,
  chartMode,
  setChartMode
}: {
  data: DailySavingsPoint[];
  hourlyData: HourlySavingsPoint[];
  resetSignal: number;
  chartMode: SavingsChartMode;
  setChartMode: (mode: SavingsChartMode) => void;
}) {
  const currentMonth = startOfMonth(new Date());
  const today = startOfDay(new Date());
  const [visibleMonth, setVisibleMonth] = useState(() => currentMonth);
  const [visibleDay, setVisibleDay] = useState(() => today);
  const [view, setView] = useState<SavingsChartView>("day");
  const firstSavingsMonth = earliestSavingsMonth(data);
  const firstHourlyDay = earliestHourlyDay(hourlyData);
  const monthlyData = buildMonthlySavingsChartData(buildMonthlySavingsWindow(data, visibleMonth));
  const hourlyChartData = buildHourlySavingsChartData(buildHourlySavingsWindow(hourlyData, visibleDay));
  const chartData = view === "month" ? monthlyData : hourlyChartData;
  const canViewPreviousMonth = firstSavingsMonth ? visibleMonth > firstSavingsMonth : false;
  const canViewNextMonth = visibleMonth < currentMonth;
  const canViewPreviousDay = firstHourlyDay ? visibleDay > firstHourlyDay : false;
  const canViewNextDay = visibleDay < today;
  const label = view === "month" ? formatMonthLabel(visibleMonth) : formatSelectedDayLabel(visibleDay);

  useEffect(() => {
    const now = new Date();
    setVisibleMonth(startOfMonth(now));
    setVisibleDay(startOfDay(now));
  }, [resetSignal]);

  return (
    <div className="savings-chart">
      <section
        aria-label={view === "month" ? `Monthly history for ${label}` : `Hourly history for ${label}`}
        className="savings-chart__panel"
      >
        <div className="savings-chart__panel-header">
          <div className="savings-chart__title-row">
            <strong>History</strong>
            <div className="savings-chart__toggle" aria-label="Metric">
              <button
                className={`savings-chart__toggle-button${chartMode === "usd" ? " is-active" : ""}`}
                onClick={() => setChartMode("usd")}
                type="button"
              >
                $
              </button>
              <button
                className={`savings-chart__toggle-button${chartMode === "tokens" ? " is-active" : ""}`}
                onClick={() => setChartMode("tokens")}
                type="button"
              >
                tokens
              </button>
            </div>
          </div>
          <div className="savings-chart__nav">
            <div className="savings-chart__toggle" aria-label="History view">
              <button
                className={`savings-chart__toggle-button${view === "month" ? " is-active" : ""}`}
                onClick={() => setView("month")}
                type="button"
              >
                month
              </button>
              <button
                className={`savings-chart__toggle-button${view === "day" ? " is-active" : ""}`}
                onClick={() => setView("day")}
                type="button"
              >
                day
              </button>
            </div>
            <button
              className="savings-chart__nav-button"
              disabled={view === "month" ? !canViewPreviousMonth : !canViewPreviousDay}
              onClick={() =>
                view === "month"
                  ? setVisibleMonth((current) => addMonths(current, -1))
                  : setVisibleDay((current) => addDays(current, -1))
              }
              type="button"
            >
              Prev
            </button>
            <span className="savings-chart__range-label">{label}</span>
            <button
              className="savings-chart__nav-button"
              disabled={view === "month" ? !canViewNextMonth : !canViewNextDay}
              onClick={() =>
                view === "month"
                  ? setVisibleMonth((current) => addMonths(current, 1))
                  : setVisibleDay((current) => addDays(current, 1))
              }
              type="button"
            >
              Next
            </button>
          </div>
        </div>
        <div className="savings-chart__canvas savings-chart__canvas--combined">
          <div className="savings-chart__overlay" aria-hidden="true">
            <span className="savings-chart__overlay-total">
              {chartMode === "usd"
                ? currency(chartData.reduce((s, d) => s + d.estimatedSavingsUsd, 0))
                : compactNumber(chartData.reduce((s, d) => s + d.estimatedTokensSaved, 0))}
            </span>
            <span className="savings-chart__overlay-label">
              {view === "day" ? "saved today" : "saved this month"}
            </span>
          </div>
          <ResponsiveContainer height="100%" width="100%">
            <BarChart
              barCategoryGap="5%"
              barGap={1}
              data={chartData}
              margin={{ top: 64, right: 2, left: 2, bottom: 0 }}
            >
              <defs>
                <linearGradient id="actualUsdGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#c96a30" />
                  <stop offset="100%" stopColor="#ED834E" />
                </linearGradient>
                <linearGradient id="savingsUsdGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#3a7f74" />
                  <stop offset="100%" stopColor="#4F9E91" />
                </linearGradient>
                <linearGradient id="actualTokensGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#c96a30" />
                  <stop offset="100%" stopColor="#ED834E" />
                </linearGradient>
                <linearGradient id="savingsTokensGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#d4b832" stopOpacity="0.35" />
                  <stop offset="100%" stopColor="#EBCC6E" stopOpacity="0.25" />
                </linearGradient>
              </defs>
              <CartesianGrid stroke="rgba(36, 31, 29, 0.06)" strokeDasharray="2 8" vertical={false} />
              <XAxis
                axisLine={false}
                dataKey="bucketKey"
                interval={0}
                minTickGap={view === "month" ? 8 : 8}
                tickFormatter={view === "month" ? dayOfMonthTickFormatter : hourOfDayTickFormatter}
                tick={{ fill: "#7a7169", fontSize: 10 }}
                tickLine={false}
              />
              <YAxis hide yAxisId="usd" />
              <YAxis hide yAxisId="tokens" />
              <Tooltip content={(props) => <SavingsChartTooltip {...props} chartMode={chartMode} />} cursor={{ fill: "rgba(36, 31, 29, 0.05)" }} />
              {chartMode === "usd" && (
                <>
                  <Bar
                    dataKey="actualCostUsd"
                    fill="url(#actualUsdGradient)"
                    maxBarSize={16}
                    stackId="usd"
                    yAxisId="usd"
                  />
                  <Bar
                    dataKey="estimatedSavingsUsd"
                    fill="url(#savingsUsdGradient)"
                    maxBarSize={16}
                    radius={[1, 1, 0, 0]}
                    stackId="usd"
                    yAxisId="usd"
                  />
                </>
              )}
              {chartMode === "tokens" && (
                <>
                  <Bar
                    dataKey="totalTokensSent"
                    fill="url(#actualTokensGradient)"
                    maxBarSize={16}
                    stackId="tokens"
                    yAxisId="tokens"
                  />
                  <Bar
                    dataKey="estimatedTokensSaved"
                    fill="url(#savingsTokensGradient)"
                    maxBarSize={16}
                    stackId="tokens"
                    yAxisId="tokens"
                    shape={(props: any) => {
                      const { x, y, width, height, fill } = props;
                      if (!width || !height) return <g />;
                      const sw = 1.5;
                      return (
                        <rect
                          x={x + sw / 2}
                          y={y + sw / 2}
                          width={Math.max(0, width - sw)}
                          height={Math.max(0, height - sw)}
                          fill={fill}
                          stroke="#EBCC6E"
                          strokeWidth={sw}
                          rx={1}
                        />
                      );
                    }}
                  />
                </>
              )}
            </BarChart>
          </ResponsiveContainer>
        </div>
      </section>
    </div>
  );
}


function renderConnectorLogo(clientId: string) {
  return <Sparkle className="client-logo__glyph" size={20} weight="duotone" />;
}

interface LauncherShellProps {
  shellClassName: string;
  spinnerClassName: string;
  copyClassName: string;
  onMouseDown: (event: MouseEvent<HTMLElement>) => void;
  version: string;
  children: ReactNode;
  showSpinner?: boolean;
}

interface ProxyVerificationRow {
  clientId: string;
  name: string;
  state: "processing" | "waiting" | "verified";
  message: string;
}

function LauncherShell({
  shellClassName,
  spinnerClassName,
  copyClassName,
  onMouseDown,
  version,
  children,
  showSpinner = true,
}: LauncherShellProps) {
  return (
    <main className="app-shell app-shell--launcher">
      <section
        className={shellClassName}
        onMouseDown={onMouseDown}
      >
        <div className="hero__badge hero__badge--launcher">
          <img src={headroomLogo} alt="" aria-hidden="true" />
          <span>v{version}</span>
        </div>
        {showSpinner && (
          <img
            className={spinnerClassName}
            src={headroomLogo}
            alt=""
            aria-hidden="true"
          />
        )}
        <div className="intro-shell__content">
          <div className={copyClassName}>{children}</div>
        </div>
      </section>
    </main>
  );
}

export default function App() {
  const [dashboard, setDashboard] = useState<DashboardState>(mockDashboard);
  const [bootstrapping, setBootstrapping] = useState(false);
  const [bootstrapProgress, setBootstrapProgress] =
    useState<BootstrapProgress>(idleBootstrapProgress);
  const [bootstrapError, setBootstrapError] = useState<string | null>(null);
  const [windowLabel, setWindowLabel] = useState<"main" | "launcher" | null>(null);
  const [startupPhase, setStartupPhase] = useState<StartupPhase>("window");
  const [startupPercent, setStartupPercent] = useState(10);
  const [startupCopy, setStartupCopy] = useState("Opening launch window…");
  const [startupReady, setStartupReady] = useState(false);
  const [activeView, setActiveView] = useState<TrayView>("home");
  const [pricingAudience, setPricingAudience] = useState<PricingAudience>("individual");
  const [showInstallStep, setShowInstallStep] = useState(false);
  const [showPostInstallGuide, setShowPostInstallGuide] = useState(false);
  const [showClientSetupStep, setShowClientSetupStep] = useState(false);
  const [showProxyVerificationStep, setShowProxyVerificationStep] = useState(false);
  const [connectors, setConnectors] = useState<ClientConnectorStatus[]>([]);
  const [openConnectorHelpId, setOpenConnectorHelpId] = useState<string | null>(null);
  const [openConnectorWarningId, setOpenConnectorWarningId] = useState<string | null>(null);
  const [connectorsBusy, setConnectorsBusy] = useState(false);
  const [connectorPhase, setConnectorPhase] = useState<"disabled" | "verifying" | "healthy">("healthy");
  const reenableLogAnchorRef = useRef<string | null>(null);
  const [connectorsError, setConnectorsError] = useState<string | null>(null);
  const [proxyVerificationRows, setProxyVerificationRows] = useState<ProxyVerificationRow[]>([]);
  const [proxyVerificationHint, setProxyVerificationHint] = useState<string | null>(null);
  const proxyVerificationLastSignatureRef = useRef("");
  const proxyVerificationBaselineLinesRef = useRef<Set<string>>(new Set());
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatus | null>(null);
  const [appUpdateConfig, setAppUpdateConfig] = useState<AppUpdateConfiguration | null>(null);
  const [appUpdateAvailable, setAppUpdateAvailable] = useState<AvailableAppUpdate | null>(null);
  const [appUpdateBusy, setAppUpdateBusy] = useState(false);
  const [appUpdateInstallBusy, setAppUpdateInstallBusy] = useState(false);
  const [appUpdateReadyToRestart, setAppUpdateReadyToRestart] = useState(false);
  const [showAppUpdateDialog, setShowAppUpdateDialog] = useState(false);
  const [appUpdateStatusCopy, setAppUpdateStatusCopy] = useState<string | null>(null);
  const [showHeadroomDetails, setShowHeadroomDetails] = useState(false);
  const [headroomLogLines, setHeadroomLogLines] = useState<string[]>([]);
  const headroomLogRef = useRef<HTMLPreElement | null>(null);
  const [showRtkDetails, setShowRtkDetails] = useState(false);
  const [rtkActivityLines, setRtkActivityLines] = useState<string[]>([]);
  const rtkActivityRef = useRef<HTMLPreElement | null>(null);
  const [claudeProjects, setClaudeProjects] = useState<ClaudeCodeProject[]>([]);
  const [claudeProjectsBusy, setClaudeProjectsBusy] = useState(false);
  const [claudeProjectsError, setClaudeProjectsError] = useState<string | null>(null);
  const [showAllClaudeProjects, setShowAllClaudeProjects] = useState(false);
  const [selectedClaudeProjectPath, setSelectedClaudeProjectPath] = useState<string | null>(null);
  const [headroomLearnStatus, setHeadroomLearnStatus] =
    useState<HeadroomLearnStatus>(idleHeadroomLearnStatus);
  const previousHeadroomLearnRunningRef = useRef(false);
  const [headroomLearnBusy, setHeadroomLearnBusy] = useState(false);
  const [headroomApiKeyStatus, setHeadroomApiKeyStatus] =
    useState<HeadroomLearnApiKeyStatus>(idleHeadroomLearnApiKeyStatus);
  const [pricingStatus, setPricingStatus] = useState<HeadroomPricingStatus | null>(null);
  const [cachedPricing] = useState<CachedPricing>(() => readCachedPricing());
  const [pricingBusy, setPricingBusy] = useState(false);
  const [pricingError, setPricingError] = useState<string | null>(null);
  const pricingRefreshInFlightRef = useRef(false);
  const [authEmail, setAuthEmail] = useState("");
  const [authCode, setAuthCode] = useState("");
  const [authCodeRequestedFor, setAuthCodeRequestedFor] = useState<string | null>(null);
  const [authCodeExpirySeconds, setAuthCodeExpirySeconds] = useState(authCodeExpiryFallbackSeconds);
  const [authRequestBusy, setAuthRequestBusy] = useState(false);
  const [authVerifyBusy, setAuthVerifyBusy] = useState(false);
  const [authFlowError, setAuthFlowError] = useState<string | null>(null);
  const [authFlowSuccess, setAuthFlowSuccess] = useState<string | null>(null);
  const [pendingUpgradePlanId, setPendingUpgradePlanId] = useState<UpgradePlanId | null>(null);
  const [showAllUpgradePlans, setShowAllUpgradePlans] = useState(false);
  const desktopActivationSentRef = useRef(false);
  const [showApiKeyDialog, setShowApiKeyDialog] = useState(false);
  const [apiKeyProvider, setApiKeyProvider] = useState<ApiProvider>("anthropic");
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [apiKeySaving, setApiKeySaving] = useState(false);
  const [apiKeyPasting, setApiKeyPasting] = useState(false);
  const [apiKeyError, setApiKeyError] = useState<string | null>(null);
  const [pendingLearnProjectPath, setPendingLearnProjectPath] = useState<string | null>(null);
  const apiKeyInputRef = useRef<HTMLInputElement | null>(null);

  const [stepSignature, setStepSignature] = useState("");
  const [stepStartedAtMs, setStepStartedAtMs] = useState<number | null>(null);
  const [stepEtaSeedSeconds, setStepEtaSeedSeconds] = useState(0);
  const [stepBasePercent, setStepBasePercent] = useState(0);
  const [chartResetSignal, setChartResetSignal] = useState(0);
  const [chartMode, setChartMode] = useState<SavingsChartMode>("usd");
  const [showSavingsInfo, setShowSavingsInfo] = useState(false);
  const [upgradeActionBusy, setUpgradeActionBusy] = useState<UpgradePlanId | null>(null);
  const [upgradeActionError, setUpgradeActionError] = useState<string | null>(null);
  const [contactEmail, setContactEmail] = useState("");
  const [contactSubmitBusy, setContactSubmitBusy] = useState(false);
  const [contactSubmitError, setContactSubmitError] = useState<string | null>(null);
  const [contactSubmitSuccess, setContactSubmitSuccess] = useState<string | null>(null);
  const appSemver = appUpdateConfig?.currentVersion ?? packageJson.version;
  const bootstrapFailureSignatureRef = useRef("");
  const mainWindowLastBlurAtRef = useRef<number | null>(null);
  const mainWindowLastSeenDayRef = useRef(formatDayKey(new Date()));
  const appUpdateKnownVersionRef = useRef<string | null>(null);
  const appUpdateReadyToRestartRef = useRef(false);
  const appUpdateBusyRef = useRef(false);
  const appUpdateInstallBusyRef = useRef(false);
  const hasShownLearnKeyReadNoticeRef = useRef(false);
  const apiKeyGuide = apiKeyGuides[apiKeyProvider];
  const apiKeyDialogStorageCopy =
    pendingLearnProjectPath
      ? "Saving stores this key in macOS Keychain, then Headroom reads it immediately to start Learn for the selected project."
      : "Saving stores this key in macOS Keychain. Headroom reads it later only when you start Learn or update the saved key.";
  const upgradePlansState = getUpgradePlans(
    pricingAudience,
    pricingStatus?.claude.planTier ?? cachedPricing.planTier,
    pricingStatus?.recommendedSubscriptionTier ?? cachedPricing.recommendedSubscriptionTier,
    pricingStatus?.account?.subscriptionTier ?? cachedPricing.subscriptionTier,
    pricingStatus?.account?.subscriptionActive ?? false
  );
  const contactEmailValid = /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(contactEmail.trim());
  const authEmailValid = /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(authEmail.trim());
  const showInstallProgress =
    bootstrapping ||
    bootstrapProgress.running ||
    bootstrapProgress.complete ||
    bootstrapProgress.failed ||
    bootstrapProgress.overallPercent > 0;

  const isLastScreen =
    windowLabel === "launcher" &&
    !showInstallStep &&
    !showClientSetupStep &&
    !showProxyVerificationStep &&
    (bootstrapProgress.complete || showPostInstallGuide || dashboard.bootstrapComplete);
  useEffect(() => {
    if (!showHeadroomDetails || !headroomLogRef.current) {
      return;
    }
    headroomLogRef.current.scrollTop = headroomLogRef.current.scrollHeight;
  }, [showHeadroomDetails, headroomLogLines]);

  useEffect(() => {
    if (!showRtkDetails || !rtkActivityRef.current) {
      return;
    }
    rtkActivityRef.current.scrollTop = rtkActivityRef.current.scrollHeight;
  }, [showRtkDetails, rtkActivityLines]);

  useEffect(() => {
    if (!authEmail && pricingStatus?.claude.email) {
      setAuthEmail(pricingStatus.claude.email);
    }
  }, [authEmail, pricingStatus?.claude.email]);

  useEffect(() => {
    setShowAllUpgradePlans(false);
  }, [pricingAudience]);

  useEffect(() => {
    if (!pricingStatus?.authenticated) {
      desktopActivationSentRef.current = false;
    }
  }, [pricingStatus?.authenticated]);

  useEffect(() => {
    if (!pricingStatus) return;
    writeCachedPricing(cachePricingStatus(pricingStatus));
  }, [pricingStatus]);

  useEffect(() => {
    const claudeConnector = getClaudeConnector(connectors);
    if (!claudeConnector?.installed) {
      return;
    }
    trackInstallMilestoneOnce("claude_code_detected", {
      enabled: claudeConnector.enabled,
      verified: claudeConnector.verified
    });
  }, [connectors]);

  useEffect(() => {
    const claudeConnector = getClaudeConnector(connectors);
    if (!claudeConnector?.enabled) {
      return;
    }
    trackInstallMilestoneOnce("optimization_enabled", {
      verified: claudeConnector.verified
    });
  }, [connectors]);

  useEffect(() => {
    if (dashboard.lifetimeRequests <= 0) {
      return;
    }
    trackInstallMilestoneOnce("first_optimized_request", {
      lifetime_requests: dashboard.lifetimeRequests,
      launch_experience: dashboard.launchExperience
    });
  }, [dashboard.launchExperience, dashboard.lifetimeRequests]);

  useEffect(() => {
    if (
      dashboard.lifetimeEstimatedTokensSaved <= 0 &&
      dashboard.lifetimeEstimatedSavingsUsd <= 0
    ) {
      return;
    }
    trackInstallMilestoneOnce("first_savings_recorded", {
      lifetime_tokens_saved: dashboard.lifetimeEstimatedTokensSaved,
      lifetime_savings_usd: Number(dashboard.lifetimeEstimatedSavingsUsd.toFixed(4))
    });
  }, [dashboard.lifetimeEstimatedSavingsUsd, dashboard.lifetimeEstimatedTokensSaved]);

  useEffect(() => {
    let active = true;

    const runStartupChecks = async () => {
      const updateStartup = (phase: StartupPhase, percent: number, message: string) => {
        if (!active) {
          return;
        }
        setStartupPhase(phase);
        setStartupPercent((current) => Math.max(current, percent));
        setStartupCopy(message);
      };

      updateStartup("window", 12, "Opening launch window…");
      const label = getCurrentWindow().label;
      if (active) {
        if (label === "main" || label === "launcher") {
          setWindowLabel(label);
        } else {
          setWindowLabel("main");
        }
      }

      updateStartup("dashboard", 35, "Loading local dashboard state…");
      const dashboardResult = await loadDashboard();
      if (!active) {
        return;
      }
      setDashboard(dashboardResult);

      updateStartup("bootstrap", 58, "Checking runtime install state…");
      const bootstrapResult = await invoke<BootstrapProgress>("get_bootstrap_progress").catch(
        () => idleBootstrapProgress
      );
      if (!active) {
        return;
      }
      setBootstrapProgress(bootstrapResult);
      if (bootstrapResult.running) {
        setBootstrapping(true);
      }
      if (label === "launcher" && (bootstrapResult.complete || dashboardResult.bootstrapComplete)) {
        if (dashboardResult.launchExperience === "first_run") {
          setShowInstallStep(true);
        } else {
          setShowPostInstallGuide(true);
        }
      }

      updateStartup("runtime", 80, "Preparing Headroom runtime…");
      const [runtimeResult, pricingResult] = await Promise.all([
        invoke<RuntimeStatus>("get_runtime_status").catch(() => null),
        invoke<HeadroomPricingStatus>("get_headroom_pricing_status").catch(() => null),
        refreshConnectors(),
      ]);
      if (!active) {
        return;
      }
      if (runtimeResult) {
        setRuntimeStatus(runtimeResult);
      }
      if (pricingResult) {
        setPricingStatus(pricingResult);
      }

      updateStartup(
        "ready",
        95,
        label === "launcher" ? "Preparing launch checklist…" : "Preparing tray dashboard…"
      );
      window.setTimeout(() => {
        if (!active) {
          return;
        }
        setStartupPercent(100);
        setStartupCopy("Headroom is ready.");
        setStartupReady(true);
      }, 120);
    };

    void runStartupChecks();

    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (startupReady) {
      return;
    }

    const phaseCaps: Record<StartupPhase, number> = {
      window: 28,
      dashboard: 54,
      bootstrap: 76,
      runtime: 92,
      ready: 99
    };
    const cap = phaseCaps[startupPhase];

    const interval = window.setInterval(() => {
      setStartupPercent((current) => {
        if (current >= cap) {
          return current;
        }
        return Math.min(cap, current + (current < 20 ? 2 : 1));
      });
    }, 260);

    return () => {
      window.clearInterval(interval);
    };
  }, [startupPhase, startupReady]);

  useEffect(() => {
    if (!bootstrapping) {
      return;
    }

    let active = true;
    let completionHandled = false;
    const interval = window.setInterval(() => {
      void invoke<BootstrapProgress>("get_bootstrap_progress")
        .then(async (progress) => {
          if (!active) {
            return;
          }

          setBootstrapProgress(progress);

          if (progress.failed) {
            const failureReport = buildBootstrapFailureReport(progress);
            const failureSignature = bootstrapFailureSignature(failureReport);
            if (bootstrapFailureSignatureRef.current !== failureSignature) {
              bootstrapFailureSignatureRef.current = failureSignature;
              reportBootstrapFailure(failureReport);
            }
            setBootstrapError(progress.message);
            setBootstrapping(false);
            completionHandled = true;
            window.clearInterval(interval);
            return;
          }

          if (progress.complete && !completionHandled) {
            completionHandled = true;
            window.clearInterval(interval);
            setBootstrapping(false);
            const latestDashboard = await loadDashboard();
            if (!active) {
              return;
            }
            setDashboard(latestDashboard);
            if (windowLabel === "launcher" && latestDashboard.launchExperience === "first_run") {
              setShowInstallStep(true);
            } else {
              setShowClientSetupStep(true);
            }
          }
        })
        .catch(() => {});
    }, 650);

    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [bootstrapping]);

  useEffect(() => {
    const launcherStep2Visible =
      windowLabel === "launcher" &&
      !showInstallStep &&
      !showProxyVerificationStep &&
      !showPostInstallGuide &&
      (showClientSetupStep || bootstrapProgress.complete || dashboard.bootstrapComplete);

    if (
      !launcherStep2Visible
    ) {
      return;
    }
    void refreshConnectors();
  }, [
    windowLabel,
    showClientSetupStep,
    showInstallStep,
    showProxyVerificationStep,
    showPostInstallGuide,
    bootstrapProgress.complete,
    dashboard.bootstrapComplete
  ]);

  useEffect(() => {
    if (
      windowLabel !== "launcher" ||
      !showProxyVerificationStep ||
      showInstallStep ||
      showPostInstallGuide
    ) {
      return;
    }

    let active = true;
    const interval = window.setInterval(() => {
      void (async () => {
        try {
          const [runtime, lines] = await Promise.all([
            invoke<RuntimeStatus>("get_runtime_status"),
            invoke<string[]>("get_headroom_logs", { maxLines: 220 })
          ]);

          if (!active) {
            return;
          }

          if (!runtime.proxyReachable) {
            setProxyVerificationHint(
              "Headroom proxy is not reachable yet. Start Headroom runtime, then send a test message."
            );
            return;
          }

          setProxyVerificationHint(null);
          const signature = lines.join("\n");
          if (!signature.trim() || signature === proxyVerificationLastSignatureRef.current) {
            return;
          }
          proxyVerificationLastSignatureRef.current = signature;

          const candidateLines = lines.filter(
            (line) => !proxyVerificationBaselineLinesRef.current.has(line)
          );
          if (candidateLines.length === 0) {
            return;
          }

          setProxyVerificationRows((current) => {
            const unusedLines = [...candidateLines];
            return current.map((row) => {
              if (row.state === "verified") {
                return row;
              }
              const matched = findClientVerificationLogLine(row.clientId, unusedLines);
              if (!matched) {
                return row;
              }
              const idx = unusedLines.lastIndexOf(matched);
              if (idx >= 0) {
                unusedLines.splice(idx, 1);
              }
              proxyVerificationBaselineLinesRef.current.add(matched);
              const snippet =
                matched.length > 160 ? `${matched.slice(0, 160)}...` : matched;
              return {
                ...row,
                state: "verified",
                message: snippet
              };
            });
          });
        } catch {
          if (active) {
            setProxyVerificationHint("Waiting for Headroom log activity...");
          }
        }
      })();
    }, 1000);

    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [windowLabel, showProxyVerificationStep, showInstallStep, showPostInstallGuide]);

  useEffect(() => {
    if (!showInstallProgress) {
      return;
    }

    const signature = `${bootstrapProgress.currentStep}|${bootstrapProgress.running}|${bootstrapProgress.complete}|${bootstrapProgress.failed}`;
    if (signature === stepSignature) {
      return;
    }

    setStepSignature(signature);
    setStepStartedAtMs(Date.now());
    setStepEtaSeedSeconds(bootstrapProgress.currentStepEtaSeconds);
    setStepBasePercent(bootstrapProgress.overallPercent);
  }, [bootstrapProgress, showInstallProgress, stepSignature]);


  useEffect(() => {
    if (!isLastScreen) return;
    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (!focused) triggerHide();
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, [isLastScreen]);

  useEffect(() => {
    if (windowLabel !== "main") {
      return;
    }

    void refreshRuntimeStatus();
    const interval = window.setInterval(() => {
      void refreshRuntimeStatus();
    }, 3000);

    return () => window.clearInterval(interval);
  }, [windowLabel]);

  useEffect(() => {
    if (windowLabel !== "main") {
      return;
    }

    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        const now = new Date();
        const nowDayKey = formatDayKey(now);

        if (!focused) {
          mainWindowLastBlurAtRef.current = now.getTime();
          mainWindowLastSeenDayRef.current = nowDayKey;
          return;
        }

        void refreshConnectors();

        const inactiveForMs = mainWindowLastBlurAtRef.current
          ? now.getTime() - mainWindowLastBlurAtRef.current
          : 0;
        const dayRolledOver = nowDayKey !== mainWindowLastSeenDayRef.current;
        if (inactiveForMs >= 3_600_000 || dayRolledOver) {
          setChartResetSignal((current) => current + 1);
        }

        mainWindowLastBlurAtRef.current = null;
        mainWindowLastSeenDayRef.current = nowDayKey;
      })
      .then((fn) => {
        unlisten = fn;
      });

    return () => unlisten?.();
  }, [windowLabel]);

  useEffect(() => {
    if (!startupReady) {
      return;
    }
    void refreshAppUpdateConfiguration();
  }, [startupReady]);

  useEffect(() => {
    if (
      !startupReady ||
      windowLabel !== "main" ||
      !appUpdateConfig
    ) {
      return;
    }
    if (!appUpdateConfig.enabled || appUpdateConfig.configurationError) {
      return;
    }

    const runBackgroundCheck = () => {
      if (
        appUpdateReadyToRestartRef.current ||
        appUpdateBusyRef.current ||
        appUpdateInstallBusyRef.current
      ) {
        return;
      }
      void checkForAppUpdate({
        background: true,
        knownUpdateVersion: appUpdateKnownVersionRef.current,
      });
    };

    const timer = window.setTimeout(runBackgroundCheck, APP_UPDATE_BACKGROUND_INITIAL_DELAY_MS);
    const interval = window.setInterval(runBackgroundCheck, APP_UPDATE_BACKGROUND_CHECK_INTERVAL_MS);

    return () => {
      window.clearTimeout(timer);
      window.clearInterval(interval);
    };
  }, [appUpdateConfig, startupReady, windowLabel]);

  useEffect(() => {
    appUpdateKnownVersionRef.current = appUpdateAvailable?.version ?? null;
  }, [appUpdateAvailable?.version]);

  useEffect(() => {
    appUpdateReadyToRestartRef.current = appUpdateReadyToRestart;
  }, [appUpdateReadyToRestart]);

  useEffect(() => {
    appUpdateBusyRef.current = appUpdateBusy;
  }, [appUpdateBusy]);

  useEffect(() => {
    appUpdateInstallBusyRef.current = appUpdateInstallBusy;
  }, [appUpdateInstallBusy]);

  useEffect(() => {
    if (activeView !== "settings") {
      return;
    }
    void Promise.all([
      refreshConnectors(),
      refreshRuntimeStatus(),
      appUpdateConfig ? Promise.resolve() : refreshAppUpdateConfiguration()
    ]);
  }, [activeView]);

  useEffect(() => {
    if (activeView !== "home") {
      return;
    }

    let active = true;
    const refreshDashboard = () => {
      void loadDashboard()
        .then((next) => {
          if (active) {
            setDashboard((prev) =>
              JSON.stringify(prev) === JSON.stringify(next) ? prev : next
            );
          }
        })
        .catch(() => {
          // keep last known state
        });
    };

    refreshDashboard();
    const interval = window.setInterval(refreshDashboard, 5000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [activeView]);

  useEffect(() => {
    if (activeView !== "home" || !startupReady) {
      return;
    }
    void Promise.all([refreshConnectors(), refreshRuntimeStatus()]);
  }, [activeView, startupReady]);

  useEffect(() => {
    if (claudeProjects.length === 0) {
      setSelectedClaudeProjectPath(null);
      return;
    }

    setSelectedClaudeProjectPath((current) => {
      if (current && claudeProjects.some((project) => project.projectPath === current)) {
        return current;
      }
      return claudeProjects[0].projectPath;
    });
  }, [claudeProjects]);

  useEffect(() => {
    if (activeView !== "optimization") {
      return;
    }
    void Promise.all([refreshClaudeProjects(), refreshHeadroomLearnApiKeyStatus()]);
  }, [activeView]);

  useEffect(() => {
    if (!showApiKeyDialog) {
      return;
    }
    const timer = window.setTimeout(() => {
      apiKeyInputRef.current?.focus();
      apiKeyInputRef.current?.select();
    }, 0);
    return () => window.clearTimeout(timer);
  }, [showApiKeyDialog]);

  useEffect(() => {
    if (activeView !== "optimization") {
      return;
    }

    let active = true;
    const refreshLearnStatus = () => {
      void invoke<HeadroomLearnStatus>("get_headroom_learn_status", {
        projectPath: selectedClaudeProjectPath
      })
        .then((status) => {
          if (active) {
            setHeadroomLearnStatus(status);
          }
        })
        .catch(() => {
          if (active) {
            setHeadroomLearnStatus((current) => ({
              ...current,
              running: false,
              summary: "Could not read headroom learn status."
            }));
          }
        });
    };

    refreshLearnStatus();
    const interval = window.setInterval(
      refreshLearnStatus,
      headroomLearnStatus.running ? 900 : 3200
    );
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [activeView, selectedClaudeProjectPath, headroomLearnStatus.running]);

  useEffect(() => {
    if (activeView !== "upgrade") {
      setUpgradeActionError(null);
    }
  }, [activeView]);

  useEffect(() => {
    const wasRunning = previousHeadroomLearnRunningRef.current;
    previousHeadroomLearnRunningRef.current = headroomLearnStatus.running;

    if (!wasRunning || headroomLearnStatus.running) {
      return;
    }

    if (headroomLearnStatus.success && headroomLearnStatus.projectPath) {
      const completedAt =
        headroomLearnStatus.lastRunAt ??
        headroomLearnStatus.finishedAt ??
        new Date().toISOString();
      setClaudeProjects((current) =>
        current.map((project) =>
          project.projectPath === headroomLearnStatus.projectPath
            ? {
                ...project,
                lastLearnRanAt: completedAt,
                hasPersistedLearnings: true,
                activeDaysSinceLastLearn: 0
              }
            : project
        )
      );
    }

    void refreshClaudeProjects();
  }, [
    headroomLearnStatus.finishedAt,
    headroomLearnStatus.lastRunAt,
    headroomLearnStatus.projectPath,
    headroomLearnStatus.running,
    headroomLearnStatus.success
  ]);

  // Keep connectorPhase in sync with the connector enabled state from the backend
  const claudeConnectorEnabled = getClaudeConnector(connectors)?.enabled;
  useEffect(() => {
    setConnectorPhase((prev) => {
      if (!claudeConnectorEnabled) return "disabled";
      if (prev === "disabled") return "healthy"; // externally re-enabled
      return prev; // keep "verifying" or "healthy"
    });
  }, [claudeConnectorEnabled]);

  useEffect(() => {
    void refreshPricingStatus();
    const interval = window.setInterval(() => {
      void refreshPricingStatus();
    }, 60_000);
    return () => {
      window.clearInterval(interval);
    };
  }, []);

  useEffect(() => {
    const claudeConnector = getClaudeConnector(connectors);
    if (!pricingStatus || pricingStatus.optimizationAllowed || !claudeConnector?.enabled) {
      return;
    }
    if (connectorsBusy) {
      return;
    }
    void toggleConnector(claudeConnector, false);
  }, [connectors, connectorsBusy, pricingStatus]);

  useEffect(() => {
    const runtimeHealthyNow =
      runtimeStatus?.running === true &&
      runtimeStatus?.proxyReachable === true &&
      connectorPhase === "healthy";
    if (!pricingStatus?.authenticated || !runtimeHealthyNow || desktopActivationSentRef.current) {
      return;
    }
    desktopActivationSentRef.current = true;
    void invoke<HeadroomPricingStatus>("activate_headroom_account")
      .then((status) => setPricingStatus(status))
      .catch(() => {
        desktopActivationSentRef.current = false;
      });
  }, [connectorPhase, pricingStatus?.authenticated, runtimeStatus?.proxyReachable, runtimeStatus?.running]);

  // Poll logs while verifying; dismiss when a matching line appears after the anchor
  useEffect(() => {
    if (connectorPhase !== "verifying") return;
    let active = true;
    const interval = setInterval(() => {
      void (async () => {
        const lines = await invoke<string[]>("get_headroom_logs", { maxLines: 200 });
        if (!active) return;
        const anchor = reenableLogAnchorRef.current;
        const anchorIdx = anchor ? lines.lastIndexOf(anchor) : -1;
        const newLines = anchorIdx >= 0 ? lines.slice(anchorIdx + 1) : lines;
        if (findClientVerificationLogLine("claude_code", newLines)) {
          setConnectorPhase("healthy");
        }
      })();
    }, 1000);
    return () => {
      active = false;
      clearInterval(interval);
    };
  }, [connectorPhase]);

  async function handleBootstrap() {
    bootstrapFailureSignatureRef.current = "";
    setBootstrapError(null);
    setBootstrapProgress({
      running: true,
      complete: false,
      failed: false,
      currentStep: "Preparing install",
      message: "Initializing installer workflow.",
      currentStepEtaSeconds: 3,
      overallPercent: 2
    });
    setBootstrapping(true);
    try {
      await invoke("start_bootstrap");
    } catch (error) {
      const failureReport = buildBootstrapInvokeFailureReport(error);
      const failureSignature = bootstrapFailureSignature(failureReport);
      if (bootstrapFailureSignatureRef.current !== failureSignature) {
        bootstrapFailureSignatureRef.current = failureSignature;
        reportBootstrapFailure(failureReport, error);
      }
      setBootstrapError(failureReport.message);
      setBootstrapProgress({
        running: false,
        complete: false,
        failed: true,
        currentStep: failureReport.currentStep,
        message: failureReport.message,
        currentStepEtaSeconds: failureReport.currentStepEtaSeconds,
        overallPercent: failureReport.overallPercent
      });
      setBootstrapping(false);
    } finally {
      // Most completion paths are still managed by progress polling.
    }
  }

  function stepPercentSpan(step: string) {
    switch (step) {
      case "Preparing install":
        return 13;
      case "Downloading Python":
        return 13;
      case "Creating environment":
        return 17;
      case "Installing Headroom":
        return 20;
      case "Installing RTK":
        return 11;
      case "Finalizing":
        return 4;
      default:
        return 8;
    }
  }

  function getStepProgress(progress: BootstrapProgress) {
    if (progress.complete) {
      return 1;
    }
    if (!progress.running || !stepStartedAtMs) {
      return 0;
    }

    const elapsedSeconds = Math.max(0, (Date.now() - stepStartedAtMs) / 1000);
    const eta = Math.max(8, stepEtaSeedSeconds || progress.currentStepEtaSeconds || 20);
    const linear = Math.min(0.96, elapsedSeconds / eta);

    if (elapsedSeconds <= eta) {
      return linear;
    }

    const overtime = elapsedSeconds - eta;
    const creep = Math.min(0.995, linear + overtime / (eta * 10));
    return creep;
  }

  function animatedOverallPercent(progress: BootstrapProgress) {
    if (progress.complete || progress.failed || !progress.running) {
      return progress.overallPercent;
    }

    const span = stepPercentSpan(progress.currentStep);
    const animated = stepBasePercent + span * getStepProgress(progress);
    return Math.min(99, Math.max(progress.overallPercent, animated));
  }

  function etaCopy(seconds: number, progress: BootstrapProgress) {
    if (!showInstallProgress) {
      return "ETA: starts after install";
    }
    if (progress.complete) {
      return "ETA: complete";
    }
    if (progress.failed) {
      return "ETA: unavailable";
    }

    const elapsedSeconds = stepStartedAtMs
      ? Math.max(0, Math.round((Date.now() - stepStartedAtMs) / 1000))
      : 0;
    const baselineEta = Math.max(stepEtaSeedSeconds, seconds);
    const remainingSeconds = Math.max(0, baselineEta - elapsedSeconds);

    if (remainingSeconds <= 0 && progress.running) {
      return "ETA: finishing up";
    }
    if (remainingSeconds <= 0) {
      return "ETA: --";
    }
    if (remainingSeconds < 60) {
      return `ETA: ${remainingSeconds}s`;
    }
    const mins = Math.floor(remainingSeconds / 60);
    const secs = remainingSeconds % 60;
    return `ETA: ${mins}m ${secs}s`;
  }

  function getConnectorUnavailableReason(connector: ClientConnectorStatus) {
    if (canConfigureConnectorWithoutDetection(connector)) {
      return null;
    }
    return (
      connectorUnavailableReasons[connector.clientId] ??
      "Connector is unavailable because this client is not detected on this machine."
    );
  }

  function canConfigureConnectorWithoutDetection(connector: ClientConnectorStatus) {
    return connector.installed || connector.clientId === "claude_code";
  }

  function getConnectorSupportWarning(connector: ClientConnectorStatus) {
    return connectorSupportWarnings[connector.clientId] ?? null;
  }

  function getConnectorDetectionWarning(connector: ClientConnectorStatus) {
    if (connector.installed) {
      return null;
    }
    if (connector.clientId === "claude_code") {
      return connectorUnavailableReasons[connector.clientId];
    }
    return null;
  }

  function getClaudeConnector(connectorsToCheck: ClientConnectorStatus[]) {
    return aggregateClientConnectors(connectorsToCheck).find(
      (connector) => connector.clientId === "claude_code"
    ) ?? null;
  }

  function applyAppUpdatePatch(patch: AppUpdateStatePatch) {
    if (Object.prototype.hasOwnProperty.call(patch, "config")) {
      setAppUpdateConfig(patch.config ?? null);
    }
    if (Object.prototype.hasOwnProperty.call(patch, "availableUpdate")) {
      setAppUpdateAvailable(patch.availableUpdate ?? null);
    }
    if (Object.prototype.hasOwnProperty.call(patch, "readyToRestart")) {
      setAppUpdateReadyToRestart(patch.readyToRestart ?? false);
    }
    if (Object.prototype.hasOwnProperty.call(patch, "showDialog")) {
      setShowAppUpdateDialog(patch.showDialog ?? false);
    }
    if (Object.prototype.hasOwnProperty.call(patch, "statusCopy")) {
      setAppUpdateStatusCopy(patch.statusCopy ?? null);
    }
  }

  async function refreshAppUpdateConfiguration() {
    applyAppUpdatePatch(await loadAppUpdateConfiguration());
  }

  async function checkForAppUpdate({
    background = false,
    knownUpdateVersion = null,
  }: {
    background?: boolean;
    knownUpdateVersion?: string | null;
  } = {}) {
    let config = appUpdateConfig;

    if (!config) {
      const configPatch = await loadAppUpdateConfiguration();
      applyAppUpdatePatch(configPatch);
      config = configPatch.config ?? null;
    }

    if (!config) {
      return;
    }

    const blockedPatch = getBlockedAppUpdateCheckPatch(config, background);
    if (blockedPatch) {
      applyAppUpdatePatch(blockedPatch);
      return;
    }

    setAppUpdateBusy(true);
    if (!background) {
      setAppUpdateStatusCopy("Checking for a new Headroom release…");
    }

    try {
      const patch = await runAppUpdateCheck({ background, knownUpdateVersion });
      applyAppUpdatePatch(patch);

      if (background && patch.availableUpdate) {
        const windowVisible = await getCurrentWindow().isVisible().catch(() => false);
        if (
          shouldNotifyAboutAvailableAppUpdate({
            background,
            availableUpdate: patch.availableUpdate,
            knownUpdateVersion,
            windowVisible,
          })
        ) {
          await sendAppUpdateNotification(patch.availableUpdate.version);
        }
      }
    } finally {
      setAppUpdateBusy(false);
    }
  }

  async function installAvailableUpdate() {
    if (!appUpdateAvailable) {
      return;
    }

    setAppUpdateInstallBusy(true);
    const installStatusCopy = getAppUpdateInstallStatusCopy(appUpdateAvailable);
    if (installStatusCopy) {
      setAppUpdateStatusCopy(installStatusCopy);
    }

    try {
      applyAppUpdatePatch(await runAppUpdateInstall({ availableUpdate: appUpdateAvailable }));
    } finally {
      setAppUpdateInstallBusy(false);
    }
  }

  function restartIntoInstalledUpdate() {
    void invoke("restart_app");
  }

  async function refreshConnectors() {
    try {
      setConnectorsError(null);
      const items = await invoke<ClientConnectorStatus[]>("get_client_connectors");
      setConnectors(items);
    } catch (error) {
      setConnectorsError(
        error instanceof Error ? error.message : "Could not load connector status."
      );
    }
  }

  async function refreshRuntimeStatus() {
    try {
      const runtime = await invoke<RuntimeStatus>("get_runtime_status");
      setRuntimeStatus(runtime);
    } catch (error) {
      setConnectorsError(
        error instanceof Error ? error.message : "Could not load runtime status."
      );
    }
  }

  async function refreshPricingStatus() {
    if (pricingRefreshInFlightRef.current) {
      return;
    }
    pricingRefreshInFlightRef.current = true;
    setPricingBusy(true);
    try {
      const status = await invoke<HeadroomPricingStatus>("get_headroom_pricing_status");
      setPricingStatus(status);
      void maybeFireTrialNotifications(status);
      setPricingError(null);
    } catch (error) {
      setPricingError(
        error instanceof Error ? error.message : "Could not load pricing status."
      );
    } finally {
      pricingRefreshInFlightRef.current = false;
      setPricingBusy(false);
    }
  }

  async function refreshClaudeProjects() {
    setClaudeProjectsBusy(true);
    try {
      setClaudeProjectsError(null);
      const projects = await invoke<ClaudeCodeProject[]>("get_claude_code_projects");
      setClaudeProjects(projects);
    } catch (error) {
      setClaudeProjectsError(
        error instanceof Error ? error.message : "Could not load Claude Code projects."
      );
    } finally {
      setClaudeProjectsBusy(false);
    }
  }

  async function refreshHeadroomLearnApiKeyStatus() {
    try {
      const status = await invoke<HeadroomLearnApiKeyStatus>("get_headroom_learn_api_key_status");
      setHeadroomApiKeyStatus(status);
      setApiKeyProvider(resolveApiProvider(status.provider));
    } catch {
      setHeadroomApiKeyStatus(idleHeadroomLearnApiKeyStatus);
    }
  }

  function openApiKeyDialog() {
    setApiKeyProvider(resolveApiProvider(headroomApiKeyStatus.provider));
    setShowApiKeyDialog(true);
  }

  async function autoConfigureClaudeCodeForLauncher() {
    setConnectorsBusy(true);
    setConnectorsError(null);

    try {
      let latestConnectors = await invoke<ClientConnectorStatus[]>("get_client_connectors");
      setConnectors(latestConnectors);

      const detectedClaudeConnector = getClaudeConnector(latestConnectors);
      if (!detectedClaudeConnector?.installed) {
        setShowClientSetupStep(true);
        return;
      }

      if (!detectedClaudeConnector.enabled) {
        await invoke<ClientSetupResult>("apply_client_setup", {
          clientId: detectedClaudeConnector.clientId
        });
        latestConnectors = await invoke<ClientConnectorStatus[]>("get_client_connectors");
        setConnectors(latestConnectors);
      }

      const configuredClaudeConnector = getClaudeConnector(latestConnectors);
      if (!configuredClaudeConnector?.enabled) {
        setShowClientSetupStep(true);
        return;
      }

      setShowClientSetupStep(false);
      setShowPostInstallGuide(false);
      await beginProxyVerificationStep();
    } catch (error) {
      setConnectorsError(
        error instanceof Error ? error.message : "Could not configure Claude Code automatically."
      );
      setShowClientSetupStep(true);
    } finally {
      setConnectorsBusy(false);
    }
  }

  async function handleFirstLaunchContinue() {
    setShowInstallStep(false);
    setShowPostInstallGuide(false);
    await autoConfigureClaudeCodeForLauncher();
  }

  async function runHeadroomLearn(projectPath: string) {
    const selectedProject =
      claudeProjects.find((project) => project.projectPath === projectPath) ?? null;
    const displayName = selectedProject?.displayName ?? projectPath;
    const shouldExplainKeychainRead =
      (headroomApiKeyStatus.source === "keychain" || headroomApiKeyStatus.source === "legacy_file") &&
      !hasShownLearnKeyReadNoticeRef.current;
    const startupSummary = shouldExplainKeychainRead && headroomApiKeyStatus.provider
      ? `Reading your saved ${apiProviderLabel(headroomApiKeyStatus.provider)} key from macOS Keychain to start Learn for ${displayName}.`
      : `Running headroom learn for ${displayName}.`;
    if (shouldExplainKeychainRead) {
      hasShownLearnKeyReadNoticeRef.current = true;
    }
    setHeadroomLearnBusy(true);
    setHeadroomLearnStatus((current) => ({
      ...current,
      running: true,
      projectPath,
      projectDisplayName: displayName,
      startedAt: new Date().toISOString(),
      finishedAt: null,
      progressPercent: Math.max(8, current.progressPercent || 0),
      summary: startupSummary,
      success: null,
      error: null
    }));
    try {
      await invoke("start_headroom_learn", { projectPath });
      for (const waitMs of [180, 350, 650, 900, 1200, 1800, 2400]) {
        await delay(waitMs);
        const status = await invoke<HeadroomLearnStatus>("get_headroom_learn_status", {
          projectPath
        });
        setHeadroomLearnStatus(status);
        if (!status.running) {
          break;
        }
      }
    } catch (error) {
      setHeadroomLearnStatus((current) => ({
        ...current,
        running: false,
        summary: "headroom learn could not be started.",
        error: error instanceof Error ? error.message : "Failed to start headroom learn."
      }));
    } finally {
      setHeadroomLearnBusy(false);
    }
  }

  async function handleRunHeadroomLearn(projectPath: string) {
    setSelectedClaudeProjectPath(projectPath);
    if (!headroomApiKeyStatus.hasApiKey) {
      setPendingLearnProjectPath(projectPath);
      setApiKeyError(null);
      openApiKeyDialog();
      return;
    }
    await runHeadroomLearn(projectPath);
  }

  async function openExternalLink(url: string) {
    await invoke("open_external_link", { url });
  }

  function openUpgradeAuthView(planId: UpgradePlanId | null = null) {
    setActiveView("upgradeAuth");
    setPendingUpgradePlanId(planId);
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
  }

  function resetUpgradeAuthStep() {
    setAuthCode("");
    setAuthCodeRequestedFor(null);
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
  }

  async function handleRequestAuthCode() {
    if (!authEmailValid) {
      setAuthFlowError("Enter a valid email address.");
      return;
    }
    setAuthRequestBusy(true);
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
    try {
      const result = await invoke<HeadroomAuthCodeRequest>("request_headroom_auth_code", {
        email: authEmail.trim()
      });
      setAuthCodeRequestedFor(result.email);
      setAuthCodeExpirySeconds(result.expiresInSeconds);
      setAuthFlowSuccess(`We sent a sign-in code to ${result.email}.`);
    } catch (error) {
      setAuthFlowError(describeInvokeError(error, "Could not send sign-in code."));
    } finally {
      setAuthRequestBusy(false);
    }
  }

  async function handleVerifyAuthCode() {
    if (!authEmailValid) {
      setAuthFlowError("Enter a valid email address.");
      return;
    }
    if (!authCode.trim()) {
      setAuthFlowError("Enter the authentication code from your email.");
      return;
    }
    setAuthVerifyBusy(true);
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
    try {
      const status = await invoke<HeadroomPricingStatus>("verify_headroom_auth_code", {
        email: authEmail.trim(),
        code: authCode.trim(),
        inviteCode: null
      });
      setPricingStatus(status);
      setAuthCode("");
      setAuthCodeRequestedFor(null);
      setAuthFlowSuccess("Headroom account connected.");
      setPendingUpgradePlanId(null);
      setActiveView("upgrade");
      await refreshConnectors();
    } catch (error) {
      setAuthFlowError(describeInvokeError(error, "Could not verify sign-in code."));
    } finally {
      setAuthVerifyBusy(false);
    }
  }

  async function handleSignOutHeadroomAccount() {
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
    try {
      await invoke("sign_out_headroom_account");
      setPricingStatus(await invoke<HeadroomPricingStatus>("get_headroom_pricing_status"));
      setAuthCode("");
      setAuthCodeRequestedFor(null);
      setAuthFlowSuccess("Signed out of Headroom.");
      setPendingUpgradePlanId(null);
    } catch (error) {
      setAuthFlowError(
        error instanceof Error ? error.message : "Could not sign out of Headroom."
      );
    }
  }

  async function openApiKeyGuideLink(url: string, label: string) {
    setApiKeyError(null);
    try {
      await openExternalLink(url);
    } catch (error) {
      setApiKeyError(
        error instanceof Error ? error.message : `Could not open the ${label} link.`
      );
    }
  }

  async function handleUpgradeAction(planId: UpgradePlanId) {
    const activeHeadroomPlanId =
      pricingStatus?.account?.subscriptionActive
        ? pricingStatus.account.subscriptionTier ?? null
        : null;
    const action = (() => {
      switch (planId) {
        case "free":
          return {
            kind: "internal" as const
          };
        case "pro":
          return {
            kind: activeHeadroomPlanId === planId ? "internal" as const : "checkout" as const
          };
        case "max5x":
          return {
            kind: activeHeadroomPlanId === planId ? "internal" as const : "checkout" as const
          };
        case "max20x":
          return {
            kind: activeHeadroomPlanId === planId ? "internal" as const : "checkout" as const
          };
        case "team":
          return {
            kind: "external" as const,
            url: SALES_CONTACT_URL,
            missing: "Set VITE_HEADROOM_SALES_CONTACT_URL to enable Team sales inquiries."
          };
        case "enterprise":
          return {
            kind: "external" as const,
            url: SALES_CONTACT_URL,
            missing: "Set VITE_HEADROOM_SALES_CONTACT_URL to enable Enterprise contact."
          };
        default:
          return null;
      }
    })();

    if (!action) {
      return;
    }

    trackAnalyticsEvent("upgrade_button_clicked", {
      plan_id: planId,
      action_kind: action.kind,
      email: pricingStatus?.account?.email ?? pricingStatus?.claude?.email ?? undefined,
    });

    if (action.kind === "internal") {
      setUpgradeActionError(null);
      setActiveView("home");
      return;
    }

    if (!pricingStatus?.authenticated) {
      openUpgradeAuthView(planId);
      return;
    }

    if (action.kind === "checkout") {
      setUpgradeActionBusy(planId);
      setUpgradeActionError(null);

      try {
        const url = await invoke<string>("create_headroom_checkout_session", {
          subscriptionTier: planId
        });
        await openExternalLink(url);
        window.setTimeout(() => {
          void refreshPricingStatus();
        }, 5_000);
      } catch (error) {
        setUpgradeActionError(
          error instanceof Error ? error.message : typeof error === "string" ? error : "Could not start checkout."
        );
      } finally {
        setUpgradeActionBusy(null);
      }
      return;
    }

    if (!action.url) {
      setUpgradeActionError(action.missing ?? "Could not open the selected plan link.");
      return;
    }

    setUpgradeActionBusy(planId);
    setUpgradeActionError(null);

    try {
      await openExternalLink(action.url);
    } catch (error) {
      setUpgradeActionError(
        error instanceof Error ? error.message : "Could not open the selected plan link."
      );
    } finally {
      setUpgradeActionBusy(null);
    }
  }

  async function handleContactSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();

    if (!CONTACT_FORM_URL) {
      setContactSubmitError("Set VITE_HEADROOM_CONTACT_FORM_URL to enable contact requests.");
      setContactSubmitSuccess(null);
      return;
    }

    const trimmed = contactEmail.trim();
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(trimmed)) {
      setContactSubmitError("Enter a valid email address.");
      setContactSubmitSuccess(null);
      return;
    }

    setContactSubmitBusy(true);
    setContactSubmitError(null);
    setContactSubmitSuccess(null);

    try {
      await invoke("submit_contact_request", { url: CONTACT_FORM_URL, email: trimmed });
      setContactEmail("");
      setContactSubmitSuccess("Thanks. Check your inbox for a confirmation email.");
    } catch (error) {
      setContactSubmitError(
        error instanceof Error ? error.message : "Could not submit the contact request."
      );
    } finally {
      setContactSubmitBusy(false);
    }
  }

  async function pasteApiKeyFromClipboard() {
    if (!navigator.clipboard?.readText) {
      setApiKeyError("Clipboard access is unavailable in this window. Paste manually.");
      return;
    }
    setApiKeyPasting(true);
    try {
      const text = (await navigator.clipboard.readText()).trim();
      if (!text) {
        setApiKeyError("Clipboard is empty.");
        return;
      }
      setApiKeyDraft(text);
      setApiKeyError(null);
    } catch {
      setApiKeyError("Could not read from clipboard. Try right-click paste.");
    } finally {
      setApiKeyPasting(false);
    }
  }

  async function saveApiKeyAndRunPending() {
    const trimmed = apiKeyDraft.trim();
    if (!trimmed) {
      setApiKeyError("Enter an API key first.");
      return;
    }
    setApiKeySaving(true);
    setApiKeyError(null);
    try {
      const status = await invoke<HeadroomLearnApiKeyStatus>("set_headroom_learn_api_key", {
        provider: apiKeyProvider,
        apiKey: trimmed
      });
      setHeadroomApiKeyStatus(status);
      setApiKeyDraft("");
      setShowApiKeyDialog(false);

      if (pendingLearnProjectPath) {
        await runHeadroomLearn(pendingLearnProjectPath);
      }
      setPendingLearnProjectPath(null);
    } catch (error) {
      setApiKeyError(error instanceof Error ? error.message : "Could not save API key.");
    } finally {
      setApiKeySaving(false);
    }
  }

  async function beginProxyVerificationStep() {
    let fresh = connectors;
    try {
      fresh = await invoke<ClientConnectorStatus[]>("get_client_connectors");
      setConnectors(fresh);
    } catch {
      // fall back to cached state
    }
    const enabledConnectors = aggregateClientConnectors(fresh)
      .filter((connector) => connector.enabled && connector.installed)
      .sort((left, right) => left.name.localeCompare(right.name));

    setShowClientSetupStep(false);
    setShowPostInstallGuide(false);
    setShowProxyVerificationStep(true);
    setProxyVerificationHint(null);
    setProxyVerificationRows(
      enabledConnectors.map((connector) => ({
        clientId: connector.clientId,
        name: connector.name,
        state: "processing",
        message: "Waiting for a Claude Code prompt..."
      }))
    );
    try {
      const lines = await invoke<string[]>("get_headroom_logs", { maxLines: 200 });
      proxyVerificationLastSignatureRef.current = lines.join("\n");
      proxyVerificationBaselineLinesRef.current = new Set(lines);
    } catch {
      proxyVerificationLastSignatureRef.current = "";
      proxyVerificationBaselineLinesRef.current = new Set();
    }
  }

  async function toggleConnector(connector: ClientConnectorStatus, nextEnabled: boolean) {
    setConnectorsBusy(true);
    setConnectorsError(null);
    try {
      if (nextEnabled) {
        await invoke<ClientSetupResult>("apply_client_setup", { clientId: connector.clientId });
      } else {
        await invoke("disable_client_setup", { clientId: connector.clientId });
      }

      const latestDashboard = await loadDashboard();
      setDashboard(latestDashboard);
      await refreshConnectors();
    } catch (error) {
      setConnectorsError(
        error instanceof Error ? error.message : "Failed to update connector."
      );
    } finally {
      setConnectorsBusy(false);
    }
  }


  function handleLauncherSurfaceMouseDown(event: MouseEvent<HTMLElement>) {
    if (event.button !== 0) {
      return;
    }

    const target = event.target as HTMLElement;
    if (
      target.closest(
        "button, input, textarea, select, a, [role='button'], [data-no-drag]"
      )
    ) {
      return;
    }

    void getCurrentWindow().startDragging();
  }

  const hidingRef = useRef(false);

  function triggerHide() {
    if (hidingRef.current) return;
    hidingRef.current = true;
    document.documentElement.classList.add("window-hiding");
    void invoke("hide_launcher_animated");
    setTimeout(() => {
      document.documentElement.classList.remove("window-hiding");
      hidingRef.current = false;
    }, 400);
  }

  const headroomTool = dashboard.tools.find((tool) => tool.id === "headroom");
  const headroomVersion = headroomTool?.version ?? "Unknown";
  const lifetimeTotalTokensSent = dashboard.dailySavings.reduce(
    (sum, point) => sum + point.totalTokensSent,
    0
  );
  const lifetimeTotalTokensBeforeOptimization =
    lifetimeTotalTokensSent + dashboard.lifetimeEstimatedTokensSaved;
  const headroomLifetimeSavingsPct =
    lifetimeTotalTokensBeforeOptimization > 0
      ? (dashboard.lifetimeEstimatedTokensSaved /
          lifetimeTotalTokensBeforeOptimization) *
        100
      : null;
  const rtkAvgSavingsPct =
    runtimeStatus?.rtk.installed && (runtimeStatus.rtk.totalCommands ?? 0) > 0
      ? runtimeStatus.rtk.avgSavingsPct ?? 0
      : null;
  const lifetimeDataDays = new Set(
    dashboard.dailySavings
      .map((point) => point.date)
      .filter((date) => Boolean(date))
  ).size;
  const lifetimeDataDaysLabel =
    lifetimeDataDays > 0
      ? `Based on ${lifetimeDataDays} day${lifetimeDataDays === 1 ? "" : "s"} of data`
      : "No historical savings data yet";

  useEffect(() => {
    window.dispatchEvent(
      new CustomEvent("headroom:boot-progress", {
        detail: {
          percent: startupPercent,
          status: startupCopy
        }
      })
    );
  }, [startupPercent, startupCopy]);

  useEffect(() => {
    if (!startupReady || windowLabel === null) {
      return;
    }
    window.dispatchEvent(new CustomEvent("headroom:boot-complete"));
  }, [startupReady, windowLabel]);

  if (!startupReady || windowLabel === null) {
    return null;
  }

  if (
    windowLabel === "launcher" &&
    (showInstallStep ||
      (!showPostInstallGuide &&
        !showClientSetupStep &&
        !bootstrapProgress.complete &&
        !dashboard.bootstrapComplete))
  ) {
    const stepProgress = Math.round(getStepProgress(bootstrapProgress) * 100);
    const renderPercent = animatedOverallPercent(bootstrapProgress);
    const installComplete = bootstrapProgress.complete || dashboard.bootstrapComplete;
    const statusCopy = showInstallProgress
      ? `${bootstrapProgress.message} ${
          bootstrapProgress.running && !bootstrapProgress.complete
            ? `(${stepProgress}% of this step)`
            : ""
        }`.trim()
      : "";

    return (
      <LauncherShell
        shellClassName="intro-shell"
        spinnerClassName="intro-shell__spinner"
        copyClassName="intro-shell__copy intro-shell__copy--first-run"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
        showSpinner={bootstrapping}
      >
        <h1>
          Headroom cuts Claude Code costs 
          <br />
           ~<span className="headline-highlight">50%</span> by trimming prompt bloat.
        </h1>
        <div className="intro-shell__checklist">
          <article>
            <strong>Privacy first</strong>
            <p>
              Your prompts never touch our servers — everything runs locally on your machine.
            </p>
          </article>
          <article>
            <strong>Self-contained</strong>
            <p>
              Keeps your runtime clean, never interfering with packages your
              projects depend on.
            </p>
          </article>
          <article>
            <strong>Optimize workflow</strong>
            <p>
              Run tools that improve your setup, so you become a little
              better at what you do every day.
            </p>
          </article>
        </div>
        {installComplete ? (
          <>
            <p className="launcher-install-notice">Headroom installation present</p>
            <button
              className="primary-button primary-button--large primary-button--success launcher-step1-continue"
              onClick={() => void handleFirstLaunchContinue()}
              type="button"
            >
              Continue
            </button>
          </>
        ) : (
          <button
            className="primary-button primary-button--large primary-button--install"
            disabled={bootstrapping}
            onClick={() => void handleBootstrap()}
            type="button"
          >
            {bootstrapping ? "Installing Headroom…" : "Install Headroom"}
          </button>
        )}
        <div className="install-progress-shell">
          {showInstallProgress ? (
            <div className="install-progress" aria-live="polite">
              <div className="install-progress__bar-track">
                <div
                  className="install-progress__bar-fill"
                  style={{ width: `${renderPercent}%` }}
                />
              </div>
              <div className="install-progress__meta">
                <p>{statusCopy}</p>
                <span>
                  {etaCopy(
                    bootstrapProgress.currentStepEtaSeconds,
                    bootstrapProgress
                  )}
                </span>
              </div>
              {bootstrapError ? (
                <p className="install-progress__error">{bootstrapError}</p>
              ) : null}
            </div>
          ) : null}
        </div>
      </LauncherShell>
    );
  }

  if (
    windowLabel === "launcher" &&
    !showInstallStep &&
    !showProxyVerificationStep &&
    (showClientSetupStep ||
      (bootstrapProgress.complete || dashboard.bootstrapComplete)) &&
    !showPostInstallGuide
  ) {
    const launcherConnectors =
      connectors.length > 0 ? connectors : launcherConnectorFallback;
    const sortedLauncherConnectors = sortClientConnectors(launcherConnectors);
    const availableConnectors = sortedLauncherConnectors.filter((connector) =>
      canConfigureConnectorWithoutDetection(connector)
    );
    const unavailableConnectors = sortedLauncherConnectors.filter(
      (connector) => !canConfigureConnectorWithoutDetection(connector)
    );
    const enabledConnectorCount = launcherConnectors.filter((connector) => connector.enabled).length;
    const requireSelection = availableConnectors.length > 0;

    return (
      <LauncherShell
        shellClassName="intro-shell intro-shell--post-install intro-shell--client-setup"
        spinnerClassName="intro-shell__spinner intro-shell__spinner--post-install"
        copyClassName="intro-shell__copy intro-shell__copy--post-install"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
      >
        <div className="post-install__lead">
          <h1>Connect Claude Code</h1>
          <p>Toggle to automatically configure Claude Code to route through Headroom.</p>
          <div className="connector-list">
            {availableConnectors.map((connector) => {
              const unavailableReason = getConnectorUnavailableReason(connector);
              const detectionWarning = getConnectorDetectionWarning(connector);
              const supportWarning = getConnectorSupportWarning(connector);
              const needsRestart = connector.enabled && !connector.verified;
              return (
                <article className="connector-item" key={connector.clientId}>
                  <div>
                    <h3>
                      <span className="client-logo" aria-hidden="true">
                        {renderConnectorLogo(connector.clientId)}
                      </span>
                      {connector.name}
                      {supportWarning ? (
                        <button
                          className="connector-warning-help"
                          onClick={() =>
                            setOpenConnectorWarningId((current) =>
                              current === connector.clientId ? null : connector.clientId
                            )
                          }
                          type="button"
                          aria-label={`Show warning for ${connector.name}`}
                          aria-expanded={openConnectorWarningId === connector.clientId}
                        >
                          !
                        </button>
                      ) : null}
                      <button
                        className="connector-help"
                        onClick={() =>
                          setOpenConnectorHelpId((current) =>
                            current === connector.clientId ? null : connector.clientId
                          )
                        }
                        type="button"
                        aria-label={`Show setup details for ${connector.name}`}
                        aria-expanded={openConnectorHelpId === connector.clientId}
                      >
                        i
                      </button>
                    </h3>
                    {openConnectorHelpId === connector.clientId ? (
                      <p className="connector-tooltip">
                        {connectorSetupDetails[connector.clientId] ??
                          "Headroom applies local connector configuration."}
                      </p>
                    ) : null}
                    {openConnectorWarningId === connector.clientId && supportWarning ? (
                      <p className="connector-tooltip connector-tooltip--warning">
                        {supportWarning}
                      </p>
                    ) : null}
                    {needsRestart ? (
                      <p className="connector-item__restart">
                        Restart {connector.name} to apply changes.
                      </p>
                    ) : null}
                    {detectionWarning ? (
                      <p className="connector-item__reason">{detectionWarning}</p>
                    ) : null}
                    {unavailableReason ? (
                      <p className="connector-item__reason">{unavailableReason}</p>
                    ) : null}
                  </div>
                  <div className="connector-item__controls">
                    <button
                      aria-checked={connector.enabled}
                      aria-label={`${connector.enabled ? "Disable" : "Enable"} ${connector.name} connector`}
                      className={`connector-switch${connector.enabled ? " is-on" : ""}`}
                      disabled={connectorsBusy}
                      onClick={() =>
                        void toggleConnector(connector, !connector.enabled)
                      }
                      role="switch"
                      title={unavailableReason ?? undefined}
                      type="button"
                    >
                      <span className="connector-switch__thumb" />
                    </button>
                  </div>
                </article>
              );
            })}
          </div>
          {unavailableConnectors.length > 0 ? (
            <div className="connector-list connector-list--unavailable">
              <p className="connector-list__section-label">Claude Code not detected on this machine</p>
              {unavailableConnectors.map((connector) => {
                const unavailableReason = getConnectorUnavailableReason(connector);
                const supportWarning = getConnectorSupportWarning(connector);
                return (
                  <article className="connector-item is-unavailable" key={connector.clientId}>
                    <div>
                      <h3>
                        <span className="client-logo" aria-hidden="true">
                          {renderConnectorLogo(connector.clientId)}
                        </span>
                        {connector.name}
                        {supportWarning ? (
                          <button
                            className="connector-warning-help"
                            onClick={() =>
                              setOpenConnectorWarningId((current) =>
                                current === connector.clientId ? null : connector.clientId
                              )
                            }
                            type="button"
                            aria-label={`Show warning for ${connector.name}`}
                            aria-expanded={openConnectorWarningId === connector.clientId}
                          >
                            !
                          </button>
                        ) : null}
                      </h3>
                      {openConnectorWarningId === connector.clientId && supportWarning ? (
                        <p className="connector-tooltip connector-tooltip--warning">
                          {supportWarning}
                        </p>
                      ) : null}
                      {unavailableReason ? (
                        <p className="connector-item__reason">{unavailableReason}</p>
                      ) : null}
                    </div>
                  </article>
                );
              })}
            </div>
          ) : null}
          {connectorsError ? (
            <p className="install-progress__error">{connectorsError}</p>
          ) : null}
        </div>
        <div className="post-install__actions">
          <button
            className="secondary-button post-install__reopen-setup"
            onClick={() => {
              setShowPostInstallGuide(false);
              setShowClientSetupStep(false);
              setShowProxyVerificationStep(false);
              setShowInstallStep(true);
            }}
            type="button"
          >
            Back
          </button>
          <button
            className="primary-button primary-button--large primary-button--success"
            disabled={connectorsBusy || (requireSelection && enabledConnectorCount === 0)}
            onClick={() => {
              setShowInstallStep(false);
              void beginProxyVerificationStep();
            }}
            type="button"
          >
            Continue
          </button>
        </div>
      </LauncherShell>
    );
  }

  if (
    windowLabel === "launcher" &&
    !showInstallStep &&
    showProxyVerificationStep &&
    !showPostInstallGuide
  ) {
    const hasEnabledApps = proxyVerificationRows.length > 0;
    const allVerified =
      hasEnabledApps &&
      proxyVerificationRows.every((row) => row.state === "verified");

    return (
      <LauncherShell
        shellClassName="intro-shell intro-shell--post-install"
        spinnerClassName="intro-shell__spinner intro-shell__spinner--post-install"
        copyClassName="intro-shell__copy intro-shell__copy--post-install"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
      >
        <div className="post-install__lead">
          <h1>Test your setup</h1>
          <p>
            Send a message in Claude Code to verify the connection is working. You may need to restart Claude Code first.
          </p>
          {hasEnabledApps ? (
            <div className="connector-list">
              {proxyVerificationRows.map((row) => (
                <article className="connector-item" key={row.clientId}>
                  <div>
                    <h3>
                      <span className="client-logo" aria-hidden="true">
                        {renderConnectorLogo(row.clientId)}
                      </span>
                      {row.name}
                    </h3>
                    <div className="proxy-verify-item__message">
                      <span>{row.message}</span>
                      {row.state === "verified" ? (
                        <span className="proxy-verified-pill">verified</span>
                      ) : null}
                    </div>
                  </div>
                </article>
              ))}
            </div>
          ) : (
            <p className="launcher-restart-hint">
              Claude Code is not enabled yet. Go back to the previous step to enable it.
            </p>
          )}
          {proxyVerificationHint ? (
            <p className="install-progress__error">{proxyVerificationHint}</p>
          ) : null}
        </div>
        <div className="post-install__actions">
          <button
            className="secondary-button post-install__reopen-setup"
            onClick={() => {
              setShowProxyVerificationStep(false);
              setShowClientSetupStep(true);
            }}
            type="button"
          >
            Back
          </button>
          <button
            className="primary-button primary-button--large primary-button--success"
            onClick={() => {
              setShowProxyVerificationStep(false);
              setShowPostInstallGuide(true);
            }}
            type="button"
          >
            Continue
          </button>
        </div>
      </LauncherShell>
    );
  }

  if (
    windowLabel === "launcher" &&
    !showInstallStep &&
    !showProxyVerificationStep &&
    (bootstrapProgress.complete ||
      showPostInstallGuide ||
      dashboard.bootstrapComplete)
  ) {
    return (
      <LauncherShell
        shellClassName="intro-shell intro-shell--post-install"
        spinnerClassName="intro-shell__spinner intro-shell__spinner--post-install"
        copyClassName="intro-shell__copy intro-shell__copy--post-install"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
      >
        <div className="post-install__lead">
          <h1>
            Headroom is now running
            <br />
            in the background
          </h1>
          {dashboard.launchExperience === "first_run" ? (
            <p>
              Send your first prompt and Headroom will start reducing costs automatically.
            </p>
          ) : (
            <>
              <p>
                It will trim prompt bloat whenever you use Claude Code.
              </p>
              <div className="post-install__metrics">
                <article className="soft-card stat-card">
                  <span className="stat-card__label">
                    <CurrencyDollar aria-hidden="true" className="stat-card__icon" size={15} weight="bold" />
                    Savings all-time
                  </span>
                  <strong className="stat-value--green">{currency(dashboard.lifetimeEstimatedSavingsUsd)}</strong>
                  <p>{lifetimeDataDaysLabel}</p>
                </article>
                <article className="soft-card stat-card">
                  <span className="stat-card__label">
                    <Cpu aria-hidden="true" className="stat-card__icon" size={15} weight="bold" />
                    Tokens saved all-time
                  </span>
                  <strong className="stat-value--blue">{compactNumber(dashboard.lifetimeEstimatedTokensSaved)}</strong>
                  <p>
                    Across {lifetimeDataDays > 0 ? `${lifetimeDataDays} tracked day${lifetimeDataDays === 1 ? "" : "s"}` : "all recorded usage"}
                  </p>
                </article>
              </div>
            </>
          )}
        </div>
        <div className="post-install__actions">
          <button
            className="secondary-button post-install__reopen-setup"
            onClick={() => {
              setShowPostInstallGuide(false);
              void beginProxyVerificationStep();
            }}
            type="button"
          >
            Back
          </button>
          <button
            className="primary-button primary-button--large primary-button--success"
            onClick={() => triggerHide()}
            type="button"
          >
            Get started
          </button>
          <p>Headroom stays active in your menu bar while you work.</p>
        </div>
      </LauncherShell>
    );
  }

  const runtimeIssues: string[] = [];
  if (runtimeStatus?.installed === false) {
    runtimeIssues.push("runtime not installed");
  }
  if (runtimeStatus?.running === false) {
    runtimeIssues.push("runtime offline");
  }
  if (runtimeStatus?.proxyReachable === false) {
    runtimeIssues.push("proxy unreachable");
  }
  if (runtimeStatus?.mcpConfigured === false) {
    runtimeIssues.push("MCP not configured");
  }
  if (runtimeStatus?.kompressEnabled === false) {
    runtimeIssues.push("Kompress disabled");
  }

  const runtimeHealthy = Boolean(
    runtimeStatus &&
      runtimeStatus.running &&
      runtimeStatus.proxyReachable &&
      runtimeStatus.mcpConfigured !== false &&
      runtimeStatus.kompressEnabled !== false
  );

  const claudeConnector = getClaudeConnector(connectors);

  const calloutBanner = (() => {
    if (!runtimeStatus) {
      return {
        tone: "disconnected",
        title: "Headroom status is unavailable."
      } as const;
    }

    if (runtimeStatus.paused) {
      return {
        tone: "paused",
        title: "Headroom is paused."
      } as const;
    }

    if (runtimeStatus.starting) {
      return {
        tone: "starting",
        title: "Headroom is starting up."
      } as const;
    }

    if (pricingStatus?.needsAuthentication) {
      return {
        tone: "degraded",
        title: pricingStatus.gateMessage
      } as const;
    }

    if (pricingStatus && !pricingStatus.optimizationAllowed) {
      return {
        tone: "disabled",
        title: pricingStatus.gateMessage
      } as const;
    }

    if (pricingStatus?.shouldNudge) {
      return {
        tone: "starting",
        title: pricingStatus.gateMessage
      } as const;
    }

    if (runtimeHealthy) {
      if (connectorPhase === "disabled") {
        return {
          tone: "disabled",
          title: "Claude is disconnected — Headroom isn't reducing costs."
        } as const;
      }
      if (connectorPhase === "verifying") {
        return {
          tone: "starting",
          title: "Send a message in Claude Code to verify the connection is working. You may need to restart Claude Code first."
        } as const;
      }
      return {
        tone: "healthy",
        title: "Headroom is running and trimming prompt bloat."
      } as const;
    }

    const disconnected = !runtimeStatus.installed || !runtimeStatus.running || !runtimeStatus.proxyReachable;
    return {
      tone: disconnected ? "disconnected" : "degraded",
      title: disconnected
        ? runtimeIssues.length > 0
          ? `Headroom is not hooked up right now: ${runtimeIssues.join(", ")}.`
          : "Headroom is not hooked up right now."
        : runtimeIssues.length > 0
          ? `Headroom needs attention: ${runtimeIssues.join(", ")}.`
          : "Headroom is running, but something needs attention."
    } as const;
  })();

  const calloutTitle =
    calloutBanner.title.length <= 110
      ? calloutBanner.title
      : (() => {
          const primaryIssue = runtimeIssues[0];
          if (!primaryIssue) {
            return calloutBanner.title;
          }
          if (calloutBanner.tone === "disconnected") {
            return `Headroom is not hooked up right now: ${primaryIssue}.`;
          }
          return `Headroom needs attention: ${primaryIssue}.`;
        })();
  const sortedClaudeProjects = [...claudeProjects].sort((left, right) => {
    const leftTime = Date.parse(left.lastWorkedAt);
    const rightTime = Date.parse(right.lastWorkedAt);
    return (Number.isNaN(rightTime) ? 0 : rightTime) - (Number.isNaN(leftTime) ? 0 : leftTime);
  });
  const pinnedClaudeProject =
    !showAllClaudeProjects && headroomLearnStatus.projectPath
      ? sortedClaudeProjects.find((project) => project.projectPath === headroomLearnStatus.projectPath) ?? null
      : null;
  const visibleClaudeProjects = (() => {
    if (showAllClaudeProjects) {
      return sortedClaudeProjects;
    }

    const topProjects = sortedClaudeProjects.slice(0, 3);
    if (!pinnedClaudeProject || topProjects.some((project) => project.projectPath === pinnedClaudeProject.projectPath)) {
      return topProjects;
    }
    return [...topProjects, pinnedClaudeProject];
  })();
  const hiddenClaudeProjectsCount = sortedClaudeProjects.length - visibleClaudeProjects.length;
  const trialDaysRemaining = formatRemainingDays(pricingStatus?.account?.trialEndsAt);
  const localGraceHoursRemaining = (() => {
    const target = pricingStatus?.localGraceEndsAt
      ? new Date(pricingStatus.localGraceEndsAt).getTime()
      : Number.NaN;
    if (Number.isNaN(target)) {
      return null;
    }
    return Math.max(0, Math.ceil((target - Date.now()) / 3_600_000));
  })();
  const weeklyLimitPercentLabel = formatPercentValue(
    pricingStatus?.effectiveDisableThresholdPercent ?? pricingStatus?.disableThresholdPercent
  );
  const upgradeDefaultPlanId =
    pricingAudience === "individual"
      ? (pricingStatus?.recommendedSubscriptionTier ?? cachedPricing.recommendedSubscriptionTier ?? upgradePlansState.featuredPlanId)
      : "enterprise";
  const upgradeDefaultPlan = upgradePlansState.plans.find((plan) => plan.id === upgradeDefaultPlanId) ?? null;
  const activeHeadroomPlanId =
    pricingAudience === "individual" && pricingStatus?.account?.subscriptionActive
      ? pricingStatus.account.subscriptionTier ?? null
      : null;
  const downgradePlanId = getNextLowerUpgradePlanId(activeHeadroomPlanId);
  const visibleUpgradePlans = (() => {
    if (showAllUpgradePlans || upgradePlansState.plans.length <= 2) {
      return upgradePlansState.plans;
    }

    if (pricingAudience === "individual" && activeHeadroomPlanId && downgradePlanId) {
      const visiblePlanIds = new Set<UpgradePlanId>([activeHeadroomPlanId, downgradePlanId]);
      const activeWindowPlans = upgradePlansState.plans.filter((plan) => visiblePlanIds.has(plan.id));
      if (activeWindowPlans.length === 2) {
        return activeWindowPlans;
      }
    }

    return upgradePlansState.plans.slice(0, 2);
  })();
  const hasHiddenUpgradePlans = visibleUpgradePlans.length < upgradePlansState.plans.length;
  const pendingUpgradePlanLabel = upgradePlanIntentLabel(pendingUpgradePlanId);
  const upgradeAuthMessage = pendingUpgradePlanLabel
    ? `Sign in with email to upgrade to the ${pendingUpgradePlanLabel} plan`
    : "Sign in with email to unlock your 14-day Headroom trial";
  const accountDisplayEmail = (() => {
    const enteredEmail = authEmail.trim();
    return (
      pricingStatus?.account?.email ??
      (enteredEmail || pricingStatus?.claude.email || "unknown email")
    );
  })();
  const accountPlanName = (() => {
    if (!pricingStatus?.authenticated) {
      return null;
    }
    if (!pricingStatus.account) {
      return pricingStatus.accountSyncError ? "Plan unavailable" : "Syncing plan...";
    }
    if (pricingStatus.account.subscriptionActive) {
      return subscriptionTierLabel(pricingStatus.account.subscriptionTier);
    }
    if (pricingStatus.account.trialActive) {
      if (trialDaysRemaining != null) {
        return `${trialDaysRemaining} day${trialDaysRemaining === 1 ? "" : "s"} left in trial`;
      }
      return "14-day trial";
    }
    return "Trial expired";
  })();
  const upgradeTrialCallout = (() => {
    if (pricingBusy && !pricingStatus) {
      return {
        tone: "neutral" as const,
        message: "Loading your Headroom access..."
      };
    }
    if (!pricingStatus) {
      return {
        tone: "neutral" as const,
        message: "Headroom pricing status is unavailable right now."
      };
    }
    if (!pricingStatus.authenticated) {
      if (!pricingStatus.localGraceActive) {
        return {
          tone: "expired" as const,
          message: "Your 72-hour Headroom access expired. Create an account to extend to 14 days.",
          actionLabel: "Sign up",
          onAction: openUpgradeAuthView
        };
      }
      const hoursLabel =
        localGraceHoursRemaining != null
          ? `${localGraceHoursRemaining} hour${localGraceHoursRemaining === 1 ? "" : "s"}`
          : "72 hours";
      return {
        tone: "warning" as const,
        message: `${hoursLabel} left in your 72-hour trial. Create an account to extend trial to 14 days.`,
        actionLabel: "Sign up",
        onAction: openUpgradeAuthView
      };
    }
    if (!pricingStatus.account) {
      return {
        tone: "neutral" as const,
        message:
          pricingStatus.accountSyncError ??
          "Headroom account connected. Syncing your trial and plan details..."
      };
    }
    if (pricingStatus.account?.subscriptionActive) {
      return {
        tone: "healthy" as const,
        message: `${subscriptionTierLabel(pricingStatus.account.subscriptionTier)} is active. Headroom can keep optimizing without limits.`
      };
    }
    if (pricingStatus.account?.trialActive) {
      const daysLabel =
        trialDaysRemaining != null
          ? `${trialDaysRemaining} day${trialDaysRemaining === 1 ? "" : "s"}`
          : "14 days";
      return {
        tone: "warning" as const,
        message: `${daysLabel} of trial to go. Upgrade to continue using Headroom without limits.`,
        actionLabel: "Upgrade",
        onAction: () => void handleUpgradeAction(upgradeDefaultPlanId)
      };
    }
    return {
      tone: pricingStatus.optimizationAllowed ? "warning" as const : "expired" as const,
      message: `Trial expired. You can only use Headroom for ${weeklyLimitPercentLabel} of your weekly Claude Code limits. To continue using Headroom without limits.`,
      actionLabel: "Upgrade",
      onAction: () => void handleUpgradeAction(upgradeDefaultPlanId)
    };
  })();
  const pricingAuthCard = (
    <section className="pricing-auth-card pricing-auth-card--standalone">
      <div className="pricing-auth-card__header">
        <div>
          <h2>{upgradeAuthMessage}.</h2>
        </div>
      </div>
      {!authCodeRequestedFor ? (
        <>
          <div className="pricing-auth-card__grid pricing-auth-card__grid--single">
            <label className="pricing-auth-field">
              <span>Email</span>
              <div className="pricing-auth-field__input">
                <EnvelopeSimple size={16} weight="bold" />
                <input
                  onChange={(event) => {
                    setAuthEmail(event.target.value);
                    setAuthFlowError(null);
                  }}
                  placeholder={pricingStatus?.claude.email ?? "you@example.com"}
                  type="email"
                  value={authEmail}
                />
              </div>
            </label>
          </div>
          <div className="pricing-auth-card__actions">
            <button
              className="primary-button"
              disabled={!authEmailValid || authRequestBusy}
              onClick={() => void handleRequestAuthCode()}
              type="button"
            >
              {authRequestBusy ? "Sending..." : "Sign in"}
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="pricing-auth-card__code-step">
            <p className="pricing-auth-card__step-copy">
              Enter the authentication code we sent to <strong>{authCodeRequestedFor}</strong>.
            </p>
            <button
              className="link-button pricing-auth-card__change-email"
              onClick={resetUpgradeAuthStep}
              type="button"
            >
              Use a different email
            </button>
          </div>
          <div className="pricing-auth-card__grid pricing-auth-card__grid--single">
            <label className="pricing-auth-field">
              <span>Authentication code</span>
              <div className="pricing-auth-field__input">
                <Key size={16} weight="bold" />
                <input
                  onChange={(event) => {
                    setAuthCode(event.target.value);
                    setAuthFlowError(null);
                  }}
                  placeholder={`Enter the code sent to ${authCodeRequestedFor}`}
                  type="text"
                  value={authCode}
                />
              </div>
            </label>
          </div>
          <div className="pricing-auth-card__actions">
            <button
              className="primary-button"
              disabled={!authCode.trim() || authVerifyBusy}
              onClick={() => void handleVerifyAuthCode()}
              type="button"
            >
              {authVerifyBusy ? "Verifying..." : "Verify and continue"}
            </button>
            <p className="pricing-auth-card__resend">
              Didn't receive a code?{" "}
              <button
                className="link-button"
                disabled={authRequestBusy}
                onClick={() => void handleRequestAuthCode()}
                type="button"
              >
                {authRequestBusy ? "Sending..." : "Resend code"}
              </button>
            </p>
          </div>
        </>
      )}
      {authFlowError ? (
        <p className="install-progress__error">{authFlowError}</p>
      ) : null}
      {authFlowSuccess ? (
        <p className="upgrade-plan-card__contact-status upgrade-plan-card__contact-status--success">
          {authFlowSuccess}
        </p>
      ) : null}
      {pricingError ? (
        <p className="install-progress__error">{pricingError}</p>
      ) : null}
    </section>
  );

  return (
    <main className="tray-shell">
      <aside className="tray-sidebar">
        <div className="tray-sidebar__logo">
          <img src={headroomLogo} alt="Headroom" />
        </div>
        <nav className="tray-nav" aria-label="Tray navigation">
          {navItems.map((item) => (
            <button
              key={item.id}
              className={`tray-nav__item${activeView === item.id ? " is-active" : ""}`}
              onMouseDown={() => setActiveView(item.id)}
              type="button"
            >
              <span className="tray-nav__icon" aria-hidden="true">
                <item.icon className="tray-nav__icon-svg" size={26} weight={activeView === item.id ? "fill" : "regular"} />
              </span>
              <span className="tray-nav__text">
                <strong>{item.label}</strong>
              </span>
            </button>
          ))}
        </nav>
        <div className="tray-sidebar__footer">
          <button
            className={`upgrade-pill${activeView === "upgrade" || activeView === "upgradeAuth" ? " is-active" : ""}`}
            onMouseDown={() => setActiveView("upgrade")}
            type="button"
          >
            Upgrade
          </button>
          <button
            className={`tray-nav__item${activeView === "settings" ? " is-active" : ""}`}
            onMouseDown={() => setActiveView("settings")}
            type="button"
          >
            <span className="tray-nav__icon" aria-hidden="true">
              <GearSix className="tray-nav__icon-svg" size={26} weight={activeView === "settings" ? "fill" : "regular"} />
            </span>
            <span className="tray-nav__text">
              <strong>Settings</strong>
            </span>
          </button>
        </div>
      </aside>

      <section className="tray-panel">
        <div className="tray-content" hidden={activeView !== "home"}>
            <section className={`callout-banner callout-banner--${calloutBanner.tone}`}>
              <span className={`callout-banner__dot callout-banner__dot--${calloutBanner.tone}`} aria-hidden="true" />
              <h1>{calloutTitle}</h1>
              {connectorPhase === "disabled" && claudeConnector && (
                <button
                  className="callout-banner__action"
                  disabled={connectorsBusy}
                  onClick={async () => {
                    const currentLines = await invoke<string[]>("get_headroom_logs", { maxLines: 200 }).catch(() => [] as string[]);
                    reenableLogAnchorRef.current = currentLines[currentLines.length - 1] ?? null;
                    await toggleConnector(claudeConnector, true);
                    setConnectorPhase("verifying");
                  }}
                  type="button"
                >
                  Re-enable
                </button>
              )}
            </section>

            <section className="stat-grid stat-grid--2col">
              <article
                className={`soft-card stat-card stat-card--clickable${chartMode === "usd" ? " is-active" : ""}`}
                onClick={() => setChartMode("usd")}
                role="button"
                tabIndex={0}
                onKeyDown={(e) => e.key === "Enter" && setChartMode("usd")}
              >
                <span className="stat-card__label">
                  <CurrencyCircleDollar aria-hidden="true" className="stat-card__icon" size={15} weight="bold"/>
                  Total costs saved (estimate)
                  <button
                    className="stat-card__info-button"
                    onClick={(e) => { e.stopPropagation(); setShowSavingsInfo(true); }}
                    type="button"
                    aria-label="How savings are calculated"
                  >
                    <Info size={13} weight="bold" />
                  </button>
                </span>
                <strong className="stat-value--green">{currency(dashboard.lifetimeEstimatedSavingsUsd)}</strong>
              </article>
              <article
                className={`soft-card stat-card stat-card--clickable${chartMode === "tokens" ? " is-active" : ""}`}
                onClick={() => setChartMode("tokens")}
                role="button"
                tabIndex={0}
                onKeyDown={(e) => e.key === "Enter" && setChartMode("tokens")}
              >
                <span className="stat-card__label">
                  <Cpu aria-hidden="true" className="stat-card__icon" size={15} weight="bold"/>
                  Total tokens saved
                </span>
                <strong className="stat-value--blue">
                  {compactNumber(dashboard.lifetimeEstimatedTokensSaved)}
                </strong>
              </article>
            </section>

            <DailySavingsChart
              data={dashboard.dailySavings}
              hourlyData={dashboard.hourlySavings}
              resetSignal={chartResetSignal}
              chartMode={chartMode}
              setChartMode={setChartMode}
            />

          </div>

        <div className="tray-content" hidden={activeView !== "optimization"}>
            <article className="soft-card optimize-card">
              <header className="optimize-card__head">
                <div className="optimize-card__title-row">
                  <span className="optimize-card__title-icon" aria-hidden="true">
                    <Brain weight="duotone" />
                  </span>
                  <h1>Session learnings</h1>
                </div>
                <p className="optimize-card__blurb">
                  Helps Claude Code learn from experience. Scans your recent coding sessions for mistakes and corrections, then writes those learnings to the project's memory so you spend fewer tokens on mistakes.
                </p>
              </header>
              <div className="optimize-card__body">
                {claudeProjectsBusy ? (
                  <p className="loading-copy">Loading projects…</p>
                ) : claudeProjects.length === 0 ? (
                  <p className="loading-copy">No Claude Code projects found in <code>~/.claude/projects</code>.</p>
                ) : (
                  <div className="optimize-minimal">
                    {!headroomApiKeyStatus.hasApiKey ? (
                      <p className="optimize-minimal__meta">
                        <button
                          className="optimize-minimal__inline-action"
                          onClick={openApiKeyDialog}
                          type="button"
                        >
                          Add an API key
                        </button>
                        {" "}to enable learning.
                      </p>
                    ) : null}
                    <div className="optimize-projects">
                      {visibleClaudeProjects.map((project) => {
                        const isRunning =
                          headroomLearnStatus.running &&
                          headroomLearnStatus.projectPath === project.projectPath;
                        const isLatestLearnProject =
                          headroomLearnStatus.projectPath === project.projectPath;
                        const disableLearn =
                          !headroomApiKeyStatus.hasApiKey ||
                          headroomLearnBusy ||
                          claudeProjectsBusy ||
                          (headroomLearnStatus.running && !isRunning);
                        const suggestRerun =
                          !isRunning &&
                          project.lastLearnRanAt !== null &&
                          project.activeDaysSinceLastLearn >= 2;
                        const learnMeta = (() => {
                          const base = formatLearnStatus(project);
                          const patterns =
                            project.lastLearnPatternCount != null
                              ? `${project.lastLearnPatternCount} learning${project.lastLearnPatternCount === 1 ? "" : "s"} added`
                              : null;
                          const stalePart = suggestRerun
                            ? `${project.activeDaysSinceLastLearn} active day${project.activeDaysSinceLastLearn === 1 ? "" : "s"} since · consider rerunning`
                            : null;
                          return [base, patterns, stalePart].filter(Boolean).join(" · ");
                        })();
                        const projectResultTone = headroomLearnStatus.success === true
                          ? "success"
                          : (headroomLearnStatus.success === false || headroomLearnStatus.error)
                              ? "failure"
                              : "idle";
                        const projectResultLabel = headroomLearnStatus.success === true
                          ? "Run succeeded"
                          : (headroomLearnStatus.success === false || headroomLearnStatus.error)
                              ? "Last run failed"
                              : "No completed run yet";
                        const showInlineResult =
                          isLatestLearnProject &&
                          !headroomLearnStatus.running &&
                          (
                            headroomLearnStatus.success !== null ||
                            Boolean(headroomLearnStatus.error) ||
                            headroomLearnStatus.outputTail.length > 0
                          );
                        return (
                          <div
                            className={`optimize-project-row${isRunning || showInlineResult ? " optimize-project-row--active" : ""}`}
                            key={project.id}
                          >
                            <div className="optimize-project-row__main">
                              <span className="optimize-project-row__name">
                                <strong>{project.displayName}</strong>
                                <small className={suggestRerun ? "optimize-project-row__stale" : undefined}>
                                  {learnMeta}
                                </small>
                              </span>
                              <div className="optimize-project-row__actions">
                                <div className="optimize-project-row__action-row">
                                  {showInlineResult ? (
                                    <span className={`optimize-project-row__status optimize-minimal__result--${projectResultTone}`}>
                                      {projectResultLabel}
                                    </span>
                                  ) : null}
                                  <button
                                    className="secondary-button secondary-button--small optimize-project-row__learn"
                                    disabled={disableLearn}
                                    onClick={() => void handleRunHeadroomLearn(project.projectPath)}
                                    type="button"
                                  >
                                    {isRunning ? "Learning…" : "Learn"}
                                  </button>
                                </div>
                                {isRunning ? (
                                  <div className="headroom-learn__progress optimize-project-row__progress" aria-live="polite">
                                    <div className="headroom-learn__progress-track">
                                      <span style={{ width: `${Math.max(6, headroomLearnStatus.progressPercent)}%` }} />
                                    </div>
                                    <p>
                                      {"Learning from sessions"}
                                      {typeof headroomLearnStatus.elapsedSeconds === "number"
                                        ? ` · ${headroomLearnStatus.elapsedSeconds}s`
                                        : ""}
                                    </p>
                                  </div>
                                ) : null}
                              </div>
                            </div>
                            {showInlineResult && headroomLearnStatus.outputTail.length > 0 ? (
                              <div className="optimize-project-row__details">
                                <details className="optimize-minimal__details">
                                  <summary>Recent output</summary>
                                  <pre className="optimize-minimal__mono optimize-minimal__output">
                                    {headroomLearnStatus.outputTail.join("\n")}
                                  </pre>
                                </details>
                              </div>
                            ) : null}
                            {showInlineResult && headroomLearnStatus.error ? (
                              <div className="optimize-project-row__result">
                                <p className="install-progress__error">{headroomLearnStatus.error}</p>
                              </div>
                            ) : null}
                          </div>
                        );
                      })}
                    </div>
                    {sortedClaudeProjects.length > 3 ? (
                      <button
                        className="optimize-minimal__inline-action optimize-projects__toggle"
                        onClick={() => setShowAllClaudeProjects((current) => !current)}
                        type="button"
                      >
                        {showAllClaudeProjects ? "fewer projects" : "more projects"}
                      </button>
                    ) : null}
                  </div>
                )}
                {claudeProjectsError ? (
                  <p className="install-progress__error">{claudeProjectsError}</p>
                ) : null}
                {headroomLearnStatus.error &&
                !claudeProjects.some((project) => project.projectPath === headroomLearnStatus.projectPath) ? (
                  <p className="install-progress__error">{headroomLearnStatus.error}</p>
                ) : null}
              </div>
            </article>

            {showApiKeyDialog ? (
              <div className="modal-backdrop" role="dialog" aria-modal="true">
                <div className="modal-card">
                  <h3>Add API key</h3>
                  <p>Headroom Learn uses an LLM to extract insights from past sessions and write them to your project memory.</p>
                  <p className="optimize-minimal__meta optimize-minimal__meta--notice">
                    {apiKeyDialogStorageCopy}
                  </p>
                  <label htmlFor="learn-api-provider">Provider</label>
                  <select
                    id="learn-api-provider"
                    value={apiKeyProvider}
                    onChange={(event) =>
                      setApiKeyProvider(event.target.value as ApiProvider)
                    }
                  >
                    <option value="openai">OpenAI</option>
                    <option value="anthropic">Claude (Anthropic)</option>
                    <option value="gemini">Gemini</option>
                  </select>
                  <ul className="api-key-guide">
                    <li>
                      Create key:{" "}
                      <a
                        href={apiKeyGuide.keyUrl}
                        onClick={(event) => {
                          event.preventDefault();
                          void openApiKeyGuideLink(apiKeyGuide.keyUrl, "create key");
                        }}
                      >
                        {apiKeyGuide.providerLabel}
                      </a>
                    </li>
                    <li>
                      Billing:{" "}
                      <a
                        href={apiKeyGuide.billingUrl}
                        onClick={(event) => {
                          event.preventDefault();
                          void openApiKeyGuideLink(apiKeyGuide.billingUrl, "billing");
                        }}
                      >
                        {apiKeyGuide.providerLabel} API billing
                      </a>
                    </li>
                    <li>API usage is billed separately from chat app subscriptions.</li>
                  </ul>
                  <label htmlFor="learn-api-key">API key</label>
                  <div className="api-key-input-row">
                    <input
                      id="learn-api-key"
                      ref={apiKeyInputRef}
                      type="password"
                      autoComplete="off"
                      placeholder="Paste API key"
                      value={apiKeyDraft}
                      onChange={(event) => setApiKeyDraft(event.target.value)}
                      onPaste={(event) => {
                        const pasted = event.clipboardData?.getData("text") ?? "";
                        if (!pasted) {
                          return;
                        }
                        event.preventDefault();
                        setApiKeyDraft(pasted.trim());
                        setApiKeyError(null);
                      }}
                    />
                    <button
                      className="secondary-button secondary-button--small"
                      disabled={apiKeySaving || apiKeyPasting}
                      onClick={() => void pasteApiKeyFromClipboard()}
                      type="button"
                    >
                      {apiKeyPasting ? "Pasting…" : "Paste"}
                    </button>
                  </div>
                  {apiKeyError ? <p className="install-progress__error">{apiKeyError}</p> : null}
                  <div className="modal-actions">
                    <button
                      className="secondary-button"
                      disabled={apiKeySaving}
                      onClick={() => {
                        setShowApiKeyDialog(false);
                        setPendingLearnProjectPath(null);
                      }}
                      type="button"
                    >
                      Cancel
                    </button>
                    <button
                      className="primary-button"
                      disabled={apiKeySaving}
                      onClick={() => void saveApiKeyAndRunPending()}
                      type="button"
                    >
                      {apiKeySaving ? "Saving…" : "Save and run"}
                    </button>
                  </div>
                </div>
              </div>
            ) : null}

          </div>

        <div className="tray-content tray-content--centered" hidden={activeView !== "health"}>
          <p className="loading-copy">Coming soon</p>
        </div>

        <div className="tray-content tray-content--centered" hidden={activeView !== "notifications"}>
          <p className="loading-copy">Coming soon</p>
        </div>

        <div className="tray-content tray-content--upgrade" hidden={activeView !== "upgrade"}>
          <section className="upgrade-hero">
            <h1>Plans based on your Claude subscription</h1>
            <div className="upgrade-toggle" aria-label="Upgrade audiences" role="tablist">
              {[
                { id: "individual" as const, label: "Individual" },
                { id: "teamEnterprise" as const, label: "Team & Enterprise" }
              ].map((audience) => (
                <button
                  key={audience.id}
                  aria-selected={pricingAudience === audience.id}
                  className={`upgrade-toggle__item${pricingAudience === audience.id ? " is-active" : ""}`}
                  onClick={() => {
                    setPricingAudience(audience.id);
                    setUpgradeActionError(null);
                  }}
                  role="tab"
                  type="button"
                >
                  {audience.label}
                </button>
              ))}
            </div>
          </section>

          <section
            className={`upgrade-trial-callout upgrade-trial-callout--${upgradeTrialCallout.tone}`}
          >
            <div className="upgrade-trial-callout__content">
              <p className="upgrade-trial-callout__message">
                {upgradeTrialCallout.message}
              </p>
            </div>
            {upgradeTrialCallout.actionLabel && upgradeTrialCallout.onAction ? (
              <button
                className="primary-button upgrade-trial-callout__button"
                disabled={authRequestBusy || authVerifyBusy || upgradeActionBusy !== null}
                onClick={() => upgradeTrialCallout.onAction?.()}
                type="button"
              >
                {upgradeTrialCallout.actionLabel}
              </button>
            ) : null}
          </section>

          <section className="upgrade-trial-callout upgrade-sale-banner">
            <p className="upgrade-trial-callout__message">🎉 50% off all paid plans — launch promotion</p>
          </section>

          <section
            className={`upgrade-plan-grid${visibleUpgradePlans.length === 1 ? " upgrade-plan-grid--single" : ""}`}
          >
            {visibleUpgradePlans.map((plan) => {
              const isFeatured = plan.id === upgradePlansState.featuredPlanId;
              const downgradeButtonClassName =
                plan.ctaTone === "downgrade" ? " upgrade-plan-card__button--downgrade" : "";
              const buttonClassName =
                plan.id === "free"
                  ? `primary-button upgrade-plan-card__button upgrade-plan-card__button--free${downgradeButtonClassName}`
                  : plan.ctaVariant === "primary"
                  ? `primary-button upgrade-plan-card__button${downgradeButtonClassName}`
                  : `secondary-button upgrade-plan-card__button${downgradeButtonClassName}`;

              return (
                <article
                  className={`upgrade-plan-card${isFeatured ? " upgrade-plan-card--featured" : ""}`}
                  key={plan.id}
                >
                  <div className="upgrade-plan-card__top">
                    <div className="upgrade-plan-card__title-block">
                      <span className="upgrade-plan-card__icon" aria-hidden="true">
                        <Sparkle weight={isFeatured ? "fill" : "duotone"} />
                      </span>
                      <div>
                        <h2>{plan.name}</h2>
                        <p>{plan.tagline}</p>
                      </div>
                    </div>
                    {plan.centeredPriceLabel ? (
                      <div className="upgrade-plan-card__price-note">{plan.centeredPriceLabel}</div>
                    ) : (
                      <div className="upgrade-plan-card__price-block">
                        <div>
                          {plan.originalPrice ? (
                            <div className="upgrade-plan-card__sale-row">
                              <s className="upgrade-plan-card__original-price">{plan.originalPrice}</s>
                              <span className="upgrade-plan-card__sale-badge">50% off</span>
                            </div>
                          ) : null}
                          <strong>{plan.price}</strong>
                        </div>
                        <span>
                          {plan.billingLines[0]}
                          <br />
                          {plan.billingLines[1]}
                        </span>
                      </div>
                    )}
                  </div>
                  <div className="upgrade-plan-card__action">
                    {plan.id === "enterprise" ? (
                      <form className="upgrade-plan-card__contact-form" onSubmit={(event) => void handleContactSubmit(event)}>
                        <input
                          className="upgrade-plan-card__contact-input"
                          onChange={(event) => {
                            setContactEmail(event.target.value);
                            if (contactSubmitError) {
                              setContactSubmitError(null);
                            }
                            if (contactSubmitSuccess) {
                              setContactSubmitSuccess(null);
                            }
                          }}
                          placeholder="you@company.com"
                          type="email"
                          value={contactEmail}
                        />
                        <button
                          className={`secondary-button upgrade-plan-card__button upgrade-plan-card__contact-submit${contactEmailValid ? " is-ready" : ""}`}
                          disabled={!contactEmailValid || contactSubmitBusy}
                          type="submit"
                        >
                          {contactSubmitBusy ? "Sending..." : plan.ctaLabel}
                        </button>
                      </form>
                    ) : (
                      <button
                        className={buttonClassName}
                        disabled={plan.disabled || upgradeActionBusy === plan.id}
                        onClick={() => void handleUpgradeAction(plan.id)}
                        type="button"
                      >
                        {upgradeActionBusy === plan.id ? "Opening..." : plan.ctaLabel}
                      </button>
                    )}
                  </div>

                  {plan.features.length > 0 ? (
                    <div className="upgrade-plan-card__features">
                      <ul>
                        {plan.features.map((feature) => (
                          <li key={feature}>{feature}</li>
                        ))}
                      </ul>
                    </div>
                  ) : null}
                  {plan.id === "enterprise" && contactSubmitError ? (
                    <p className="upgrade-plan-card__contact-status upgrade-plan-card__contact-status--error">
                      {contactSubmitError}
                    </p>
                  ) : null}
                  {plan.id === "enterprise" && contactSubmitSuccess ? (
                    <p className="upgrade-plan-card__contact-status upgrade-plan-card__contact-status--success">
                      {contactSubmitSuccess}
                    </p>
                  ) : null}
                </article>
              );
            })}
          </section>
          {pricingAudience === "individual" && (hasHiddenUpgradePlans || showAllUpgradePlans) ? (
            <button
              className="upgrade-plan-grid__toggle"
              onClick={() => setShowAllUpgradePlans((current) => !current)}
              type="button"
            >
              {showAllUpgradePlans ? "show fewer plans" : "show more plans"}
            </button>
          ) : null}

          {upgradeActionError ? (
            <p className="install-progress__error">{upgradeActionError}</p>
          ) : null}
        </div>

        <div className="tray-content tray-content--upgrade" hidden={activeView !== "upgradeAuth"}>
          <section className="upgrade-auth-view">
            <div className="upgrade-auth-view__header">
              <div className="upgrade-auth-view__title-row">
                <button
                  aria-label="Back to upgrade plans"
                  className="upgrade-auth-view__back"
                  onClick={() => setActiveView("upgrade")}
                  type="button"
                >
                  <CaretLeft size={16} weight="bold" />
                </button>
                <h1>Create account</h1>
              </div>
            </div>
            {pricingAuthCard}
          </section>
        </div>

        <div className="tray-content" hidden={activeView !== "settings"}>
            <section className="content-header">
              <div>
                <p>Manage how Headroom works with Claude Code.</p>
              </div>
            </section>

            <section className="panel-stack">
              <article className="soft-card panel-card settings-account-card">
                <div className="settings-account-row">
                  <p className="settings-account-copy">
                    Headroom account:{" "}
                    {pricingStatus?.authenticated ? (
                      <>
                        {accountDisplayEmail} <em>({accountPlanName})</em>
                      </>
                    ) : (
                      <em>not signed in</em>
                    )}
                  </p>
                  {pricingStatus?.authenticated ? (
                    <button
                      className="secondary-button secondary-button--small"
                      onClick={() => void handleSignOutHeadroomAccount()}
                      type="button"
                    >
                      <SignOut size={16} weight="bold" />
                      Sign out
                    </button>
                  ) : (
                    <button
                      className="secondary-button secondary-button--small"
                      onClick={() => openUpgradeAuthView()}
                      type="button"
                    >
                      Sign in
                    </button>
                  )}
                </div>
              </article>

              <article className="soft-card panel-card">
                <div className="panel-card__header">
                  <div />
                </div>
                <div className="connector-list">
                  {sortClientConnectors(aggregateClientConnectors(connectors)).map((connector) => {
                    const connectorLabel =
                      connector.clientId === "claude_code"
                        ? "Claude Code connection"
                        : connector.name;
                    const unavailableReason = getConnectorUnavailableReason(connector);
                    const detectionWarning = getConnectorDetectionWarning(connector);
                    const toggleDisabled =
                      connectorsBusy || !canConfigureConnectorWithoutDetection(connector);
                    return (
                      <article className="connector-item" key={connector.clientId}>
                        <div>
                          <h3>
                            <span className="client-logo" aria-hidden="true">
                              {renderConnectorLogo(connector.clientId)}
                            </span>
                            {connectorLabel}
                            <button
                              className="connector-help"
                              onClick={() =>
                                setOpenConnectorHelpId((current) =>
                                  current === connector.clientId ? null : connector.clientId
                                )
                              }
                              type="button"
                              aria-label={`Show setup details for ${connector.name}`}
                              aria-expanded={openConnectorHelpId === connector.clientId}
                            >
                              i
                            </button>
                          </h3>
                          {openConnectorHelpId === connector.clientId ? (
                            <p className="connector-tooltip">
                              {connectorSetupDetails[connector.clientId] ??
                                "Headroom applies local connector configuration."}
                            </p>
                          ) : null}
                          {connector.enabled && !connector.verified && connector.installed ? (
                            <p className="connector-item__restart">
                              Restart {connector.name} to start routing through Headroom.
                            </p>
                          ) : null}
                          {detectionWarning ? (
                            <p className="connector-item__reason">{detectionWarning}</p>
                          ) : null}
                          {unavailableReason ? (
                            <p className="connector-item__reason">{unavailableReason}</p>
                          ) : null}
                        </div>
                        <div className="connector-item__controls">
                          <button
                            aria-checked={connector.enabled}
                            aria-label={`${connector.enabled ? "Disable" : "Enable"} ${connector.name} connector`}
                            className={`connector-switch${connector.enabled ? " is-on" : ""}`}
                            disabled={toggleDisabled}
                            onClick={() =>
                              void toggleConnector(connector, !connector.enabled)
                            }
                            role="switch"
                            title={unavailableReason ?? undefined}
                            type="button"
                          >
                            <span className="connector-switch__thumb" />
                          </button>
                        </div>
                      </article>
                    );
                  })}
                </div>
                {connectorsError ? (
                  <p className="install-progress__error">{connectorsError}</p>
                ) : null}
              </article>

              <article className="soft-card panel-card">
                <div className="panel-card__header">
                  <div>
                    <h3>Tools status</h3>
                  </div>
                </div>
                <div className="runtime-status">
                  <div className="runtime-status__topline">
                    <span className="runtime-status__section-title">
                      Headroom app ({appSemver})
                    </span>
                  </div>
                  <div className="runtime-status__section-action-row">
                    <button
                      className="secondary-button secondary-button--small"
                      disabled={appUpdateBusy || appUpdateInstallBusy}
                      onClick={() => void checkForAppUpdate()}
                      type="button"
                    >
                      {appUpdateBusy ? "Checking…" : "Check for updates"}
                    </button>
                    {appUpdateStatusCopy ? (
                      <p className="app-update-card__summary runtime-status__summary">
                        {appUpdateStatusCopy}
                      </p>
                    ) : null}
                  </div>
                  <div className="runtime-status__meta">
                    <span className="runtime-status__section-title">
                      Headroom CLI ({headroomVersion})
                      {headroomLifetimeSavingsPct !== null ? (
                        <span className="runtime-status__section-context">
                          {" "}
                          ({percent1(headroomLifetimeSavingsPct)}% all-time savings)
                        </span>
                      ) : null}
                    </span>
                  </div>
                  <div className="runtime-status__grid runtime-status__grid--4">
                    {[
                      {
                        name: "Runtime",
                        ok: runtimeStatus?.running === true,
                      },
                      {
                        name: "Proxy",
                        ok: runtimeStatus?.proxyReachable === true,
                        suffix: "6767",
                        onClick: () => void invoke("open_headroom_dashboard"),
                      },
                      {
                        name: "MCP",
                        ok: runtimeStatus?.mcpConfigured === true,
                      },
                      {
                        name: "Kompress",
                        ok: runtimeStatus?.kompressEnabled === true,
                      },
                    ].map((s) => (
                      <span
                        key={s.name}
                        className={`runtime-status__item${s.onClick ? " runtime-status__item--clickable" : ""}`}
                        onClick={s.onClick}
                      >
                        <span className="runtime-status__label">{s.name}:</span>
                        <span className={`runtime-status__indicator ${s.ok ? "runtime-status__indicator--ok" : "runtime-status__indicator--off"}`}>
                          {s.ok ? "✔" : "✖"}
                        </span>
                        {s.suffix && <span className="runtime-status__suffix">({s.suffix})</span>}
                      </span>
                    ))}
                  </div>
                  <button
                    className="link-button runtime-status__section-action"
                    onClick={async () => {
                      const next = !showHeadroomDetails;
                      setShowHeadroomDetails(next);
                      if (next) {
                        try {
                          const lines = await invoke<string[]>("get_headroom_logs", { maxLines: 80 });
                          setHeadroomLogLines(lines);
                        } catch {
                          setHeadroomLogLines(["Failed to load headroom logs."]);
                        }
                      }
                    }}
                    type="button"
                  >
                    {showHeadroomDetails ? "Hide headroom logs" : "Show headroom logs"}
                  </button>
                  {showHeadroomDetails ? (
                    <pre className="runtime-log" ref={headroomLogRef}>
                      {headroomLogLines.length > 0 ? headroomLogLines.join("\n") : "No log output yet."}
                    </pre>
                  ) : null}
                  <div className="runtime-status__meta">
                    <span className="runtime-status__section-title">
                      RTK ({runtimeStatus?.rtk.version ?? "not installed"})
                      {rtkAvgSavingsPct !== null ? (
                        <span className="runtime-status__section-context">
                          {" "}
                          ({percent1(rtkAvgSavingsPct)}% avg savings)
                        </span>
                      ) : null}
                    </span>
                  </div>
                  <div className="runtime-status__grid runtime-status__grid--3">
                    {[
                      {
                        name: "Binary",
                        ok: runtimeStatus?.rtk.installed === true
                      },
                      {
                        name: "PATH",
                        ok: runtimeStatus?.rtk.pathConfigured === true
                      },
                      {
                        name: "Hook",
                        ok: runtimeStatus?.rtk.hookConfigured === true
                      }
                    ].map((s) => (
                      <span key={s.name} className="runtime-status__item">
                        <span className="runtime-status__label">{s.name}:</span>
                        <span
                          className={`runtime-status__indicator ${s.ok ? "runtime-status__indicator--ok" : "runtime-status__indicator--off"}`}
                        >
                          {s.ok ? "✔" : "✖"}
                        </span>
                      </span>
                    ))}
                  </div>
                  <button
                    className="link-button runtime-status__section-action"
                    onClick={async () => {
                      const next = !showRtkDetails;
                      setShowRtkDetails(next);
                      if (next) {
                        try {
                          const lines = await invoke<string[]>("get_rtk_activity", { maxLines: 80 });
                          setRtkActivityLines(lines);
                        } catch {
                          setRtkActivityLines(["Failed to load RTK activity."]);
                        }
                      }
                    }}
                    type="button"
                  >
                    {showRtkDetails ? "Hide RTK activity" : "Show RTK activity"}
                  </button>
                  {showRtkDetails ? (
                    <pre className="runtime-log" ref={rtkActivityRef}>
                      {rtkActivityLines.length > 0 ? rtkActivityLines.join("\n") : "No RTK activity yet."}
                    </pre>
                  ) : null}
                </div>
              </article>
              <a
                className="contact-link"
                href="mailto:support@extraheadroom.com"
              >
                Contact us
              </a>
              <button
                className="quit-button"
                onClick={() => void invoke("quit_headroom")}
                type="button"
              >
                Quit Headroom
              </button>
            </section>
          </div>

          {showSavingsInfo && (
            <div
              className="modal-backdrop"
              role="dialog"
              aria-modal="true"
              onClick={() => setShowSavingsInfo(false)}
            >
              <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                <h3>How savings are calculated</h3>
                <p>Headroom intercepts and prunes all inputs before sending them to Claude.</p>
                <p>Savings = tokens removed &times; API token prices.</p>
                <p>This is an optimistic estimate.</p>
                <p>Without Headroom, when tokens are sent to Claude for the first time they would be stored in their cache. Once in the cache, whenever these same tokens are sent again Claude applies a 90% discount to their cost. In our testing, this can reduce the actual savings by at most 50%.</p>
                <p>Even accounting for caching, you've likely saved at least <strong>{currency(dashboard.lifetimeEstimatedSavingsUsd * 0.5)}</strong>.</p>
                <div className="modal-actions">
                  <button
                    className="button button--primary"
                    onClick={() => setShowSavingsInfo(false)}
                    type="button"
                  >
                    Got it
                  </button>
                </div>
              </div>
            </div>
          )}

          {showAppUpdateDialog && appUpdateAvailable ? (
            <div className="modal-backdrop" role="dialog" aria-modal="true">
              <div className="modal-card">
                <h3>
                  {appUpdateReadyToRestart
                    ? `Restart to finish updating to ${appUpdateAvailable.version}`
                    : `Headroom ${appUpdateAvailable.version} is available`}
                </h3>
                <p>
                  {appUpdateReadyToRestart
                    ? "The new version has been installed. Restart Headroom when you're ready to switch over."
                    : "Headroom found a new release in the background. Nothing will install until you confirm it here."}
                </p>
                <ul className="api-key-guide">
                  <li>Current version: {appUpdateAvailable.currentVersion}</li>
                  <li>New version: {appUpdateAvailable.version}</li>
                  <li>
                    Published: {formatDateTime(appUpdateAvailable.publishedAt ?? null)}
                  </li>
                </ul>
                <div className="modal-actions">
                  <button
                    className="secondary-button"
                    disabled={appUpdateInstallBusy}
                    onClick={() => setShowAppUpdateDialog(false)}
                    type="button"
                  >
                    Later
                  </button>
                  <button
                    className="primary-button"
                    disabled={appUpdateInstallBusy}
                    onClick={() =>
                      appUpdateReadyToRestart
                        ? restartIntoInstalledUpdate()
                        : void installAvailableUpdate()
                    }
                    type="button"
                  >
                    {appUpdateInstallBusy
                      ? "Installing…"
                      : appUpdateReadyToRestart
                        ? "Restart now"
                        : `Install ${appUpdateAvailable.version}`}
                  </button>
                </div>
              </div>
            </div>
          ) : null}
      </section>
    </main>
  );
}
