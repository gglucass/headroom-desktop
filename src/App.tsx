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
  ArrowClockwise,
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
  Key,
  PuzzlePiece,
  SignOut,
  Sliders,
  Sparkle,
  Terminal,
} from "@phosphor-icons/react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  PREVIEW_SUPPORT_EMAIL,
  platformPreviewNoticeFor,
  platformPreviewSupportMailto,
} from "./lib/platform";
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
  formatAppUpdateProgressCopy,
  getAppUpdateInstallStatusCopy,
  getBlockedAppUpdateCheckPatch,
  loadAppUpdateConfiguration,
  runAppUpdateCheck,
  runAppUpdateInstall,
  sendAppUpdateNotification,
  shouldNotifyAboutAvailableAppUpdate,
  maybeFireStaleAppUpdateNotification,
  type AppUpdateStatePatch,
} from "./lib/appUpdate";
import { maybeFireTrialNotifications } from "./lib/trialNotifications";
import {
  fireUpsellNudge,
  maybeFireUrgentPricingNotifications,
  maybeFireUrgentRuntimeNotification,
} from "./lib/urgentNotifications";
import {
  maybeFireSetupStallAlert,
  setupStallBannerLine,
  SETUP_STALL_CHECK_INTERVAL_MS,
  SETUP_STALL_EARLIEST_MS,
  type SetupStallAlert,
  type SetupStallKind,
} from "./lib/setupHealthAlert";
import { SetupStallModal } from "./components/SetupStallModal";
import { UpstreamPanel } from "./components/UpstreamPanel";
import {
  authCodeSentMessage,
  buildInstallFailureMailto,
  buildSetupStallMailto,
  describeInvokeError,
  formatCents,
  getNextHigherUpgradePlanId,
  getNextLowerUpgradePlanId,
  getPlanRenewalPriceLabel,
  getUpgradePlans,
  type UpgradePlan,
  introSaleBadgeLabel,
  isTierDowngrade,
  matchesSubscriptionPeriod,
  forgoneSavingsLabel,
  paybackLabel,
  recentDailySavingsUsd,
  setServerPlanPrices,
  tierRecommendationSourceLabel,
  scheduledPlanChange,
  upgradePlanIntentLabel,
  type BillingPeriod,
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
  baseUrlTakeoverNotice,
  buildHourlySavingsChartData,
  buildHourlySavingsWindow,
  buildMonthlySavingsChartData,
  buildMonthlySavingsWindow,
  compressibleInputSavingsRate,
  allTimeCacheHitPair,
  cacheHitPair,
  outputReductionForWindow,
  compactNumber,
  connectorDashboardStatus,
  connectorStatusLine,
  clientSetupNotice,
  currency,
  currencyExact,
  dayOfMonthTickFormatter,
  earliestHourlyDay,
  earliestSavingsMonth,
  formatDateTime,
  formatDayKey,
  formatLearnStatus,
  formatMonthLabel,
  formatSelectedDayLabel,
  getEnabledSupportedConnectors,
  hasEnabledConnector,
  hasNeverScanned,
  hourOfDayTickFormatter,
  mergeProviderSavingsForDisplay,
  percent1,
  sortClientConnectors,
  startOfDay,
  startOfMonth,
  type SavingsChartDatum
} from "./lib/dashboardHelpers";
import {
  buildInitialProxyVerificationRows,
  formatConnectorNameList,
  getClaudeConnector,
  getContactRequestValidationError,
  getInitialLauncherStage,
  magicLinkScreenCopy,
  getLauncherAutoConfigureDecision,
  isValidEmailAddress,
  needsTermsAcceptance,
  nextAutoConfigureStep,
  nextAutoConfigureStepAfterApply,
  recommendedHeadroomTier,
  type LauncherStage,
  type MagicLinkState,
  type InstallWizardStep
} from "./lib/launcherHelpers";
import { mockDashboard } from "./lib/mockData";
import {
  cachePricingStatus,
  type CachedPricing,
  formatRemainingDays,
  readCachedPricing,
  subscriptionTierLabel,
  writeCachedPricing
} from "./lib/pricing";
import {
  activityFeedSignature,
  notificationActionView,
  serializeState,
  type TrayView
} from "./lib/trayHelpers";
import { trackAnalyticsEvent, trackInstallMilestoneOnce } from "./lib/analytics";
import { ActivityFeed } from "./components/ActivityFeed";
import { AuthCodeForm } from "./components/AuthCodeForm";
import { ConnectorIcon, hasConnectorIcon } from "./components/ConnectorIcon";
import { LauncherShell } from "./components/LauncherShell";
import { LearnScanStatusLine } from "./components/LearnScanStatusLine";
import { OptimizePanel } from "./components/OptimizePanel";
import { TermsGate } from "./components/TermsGate";
import type {
  AppUpdateConfiguration,
  AvailableAppUpdate,
  BootstrapFailureReport,
  BootstrapProgress,
  ClaudePlanTier,
  HeadroomAuthCodeRequest,
  HeadroomPricingStatus,
  LaunchFlags,
  ClaudeCodeProject,
  ClientConnectorStatus,
  ClientSetupResult,
  DailySavingsPoint,
  DashboardState,
  DebugOverrides,
  HeadroomLearnPrereqStatus,
  HeadroomLearnStatus,
  HeadroomSubscriptionTier,
  ActivityFeedResponse,
  AppliedPatterns,
  HourlySavingsPoint,
  OutputReduction,
  RuntimeStatus,
  RuntimeUpgradeFailure,
  RuntimeUpgradeProgress,
  SaveOffer,
} from "./lib/types";

interface NavItem {
  id: TrayView;
  label: string;
  icon: ElementType;
}

const navItems: NavItem[] = [
  { id: "home", label: "Home", icon: House },
  { id: "optimization", label: "Optimize", icon: Sliders },
  { id: "notifications", label: "Activity", icon: Bell },
  { id: "addons", label: "Addons", icon: PuzzlePiece },
];

interface AddonCopy {
  whatItDoes: string;
  installing?: string;
  uninstalling?: string;
  installed?: string;
  uninstalled?: string;
  enabling?: string;
  disabling?: string;
  disabled?: string;
}

const addonCopy: Record<string, AddonCopy> = {
  rtk: {
    whatItDoes:
      "Installing downloads the RTK binary into Headroom's managed runtime, adds it to your shell PATH, and turns on the bash auto-rewrite hook. Shell commands your agent runs are routed through RTK, which compacts their output so it costs far fewer tokens. Removed cleanly when you uninstall it or Headroom.",
    installing: "Downloading RTK and registering the bash hook...",
    uninstalling: "Removing RTK, its PATH entry, and the bash hook...",
    uninstalled: "RTK removed. Shell commands run normally, without output rewriting.",
    enabling: "Enabling RTK and registering the bash hook...",
    disabling: "Disabling RTK and removing the bash hook...",
    disabled: "RTK is off but still installed. Re-enable any time without re-downloading."
  },
  markitdown: {
    whatItDoes:
      "Installing adds the MarkItDown converter to Headroom's managed Python runtime and registers a document Read hook. Nothing is installed system-wide - it all lives under Headroom's app data and is removed when you uninstall Headroom.",
    installing: "Installing MarkItDown and registering the Read hook...",
    uninstalling: "Removing MarkItDown and its Read hook...",
    uninstalled: "MarkItDown removed. Your agent reads documents in their original format again.",
    enabling: "Enabling MarkItDown...",
    disabling: "Disabling MarkItDown...",
    disabled: "MarkItDown is off. It stays installed but no longer converts documents."
  },
  ponytail: {
    whatItDoes:
      "Installing registers the Ponytail marketplace and plugin in Claude Code and/or Codex (whichever CLIs are on your PATH). It nudges the agent to write the least code possible. Removed from the plugin registry when you uninstall it or Headroom.",
    installing: "Registering the Ponytail plugin with your agent...",
    uninstalling: "Removing the Ponytail plugin...",
    uninstalled: "Ponytail removed. Your agent writes code without the Ponytail nudge.",
    installed: "Ponytail installed. Run /ponytail-audit in your agent to scan this codebase for over-engineering.",
    enabling: "Enabling Ponytail...",
    disabling: "Disabling Ponytail...",
    disabled: "Ponytail is off. It stays installed but no longer nudges the agent."
  },
  caveman: {
    whatItDoes:
      "Installing registers the Caveman marketplace and plugin in Claude Code and/or Codex (whichever CLIs are on your PATH). It makes the agent reply in terse caveman-speak, cutting output tokens while keeping code, commands, and errors exact. Removed from the plugin registry when you uninstall it or Headroom.",
    installing: "Registering the Caveman plugin with your agent...",
    uninstalling: "Removing the Caveman plugin...",
    uninstalled: "Caveman removed. Your agent speaks in full sentences again.",
    installed: "Caveman installed. Run /caveman-stats in your agent to see session token savings.",
    enabling: "Enabling Caveman...",
    disabling: "Disabling Caveman...",
    disabled: "Caveman is off. It stays installed but no longer compresses replies."
  },
  serena: {
    whatItDoes:
      "Installing sets up Serena in Headroom's managed runtime and registers it as an MCP server in Claude Code and ChatGPT (Codex). Your agent gets symbol-level code tools - find a definition, read just that function, edit in place - instead of reading whole files. Its tool definitions add some tokens to every request, so the net saving is largest in bigger codebases. A serena MCP entry you configured yourself is never touched, and everything is removed cleanly when you uninstall it or Headroom.",
    installing: "Installing Serena and registering its MCP server...",
    installed: "Serena installed. Restart open agent sessions to pick up the new MCP server.",
    uninstalling: "Removing Serena and its MCP registrations...",
    uninstalled: "Serena removed. Your agent reads code as whole files again.",
    enabling: "Re-registering the Serena MCP server...",
    disabling: "Removing the Serena MCP registrations...",
    disabled: "Serena is off but still installed. Re-enable any time without re-downloading."
  },
  "codebase-memory": {
    whatItDoes:
      "Installing downloads the codebase-memory binary into Headroom's managed runtime, verifies it, and registers it as an MCP server in Claude Code and ChatGPT (Codex). It indexes a repo into a persistent knowledge graph - functions, classes, call chains - so your agent answers structure questions from the graph instead of re-reading files. Ask your agent to index a repo the first time you use it there. Indexes are stored inside Headroom's app data, a codebase-memory MCP entry you configured yourself is never touched, and everything is removed cleanly when you uninstall it or Headroom.",
    installing: "Downloading Codebase Memory and registering its MCP server...",
    installed: "Codebase Memory installed. Restart open agent sessions, then ask your agent to index the repo.",
    uninstalling: "Removing Codebase Memory, its indexes, and its MCP registrations...",
    uninstalled: "Codebase Memory removed. Your agent explores code by reading files again.",
    enabling: "Re-registering the Codebase Memory MCP server...",
    disabling: "Removing the Codebase Memory MCP registrations...",
    disabled: "Codebase Memory is off but still installed. Re-enable any time without re-downloading."
  },
  context7: {
    whatItDoes:
      "Installing verifies the Context7 MCP server runs via npx, then registers it in Claude Code and ChatGPT (Codex). Your agent can pull current, version-specific documentation for the libraries you use instead of guessing APIs from stale training data - docs are fetched only when it asks, so the idle cost is just its tool definitions. A context7 MCP entry you configured yourself is never touched, and the registration is removed cleanly when you uninstall it or Headroom. Requires Node.js on PATH.",
    installing: "Verifying Context7 with npx and registering its MCP server...",
    installed: "Context7 installed. Restart open agent sessions to pick up the new MCP server.",
    uninstalling: "Removing the Context7 MCP registrations...",
    uninstalled: "Context7 removed. Your agent relies on its training data for library docs again.",
    enabling: "Re-registering the Context7 MCP server...",
    disabling: "Removing the Context7 MCP registrations...",
    disabled: "Context7 is off but still installed. Re-enable any time."
  }
};

const connectorSetupDetails: Record<string, string> = {
  claude_code:
    "Headroom injects ANTHROPIC_BASE_URL into shell profiles and ~/.claude/settings.json so Claude Code connects through Headroom.",
  codex:
    "The ChatGPT app (previously Codex), its IDE extension, and the Codex CLI share ~/.codex/config.toml. Headroom adds a managed provider there and an OPENAI_BASE_URL shell export, plus a SessionStart guard that warns when routing breaks. In the Codex CLI, run /hooks once to review and trust the guard (and again after it changes).",
  grok_build:
    "Headroom writes a managed proxy block to ~/.grok/config.toml and exports GROK_CLI_CHAT_PROXY_BASE_URL in your shell profiles so Grok Build connects through Headroom.",
  opencode:
    "Headroom points the anthropic and openai provider base URLs in OpenCode's config file (usually ~/.config/opencode/opencode.json) at its localhost proxy and registers a transport plugin that routes every other provider through it too. Anthropic and OpenAI traffic is optimized; other providers pass through for visibility. A project-level opencode.json can override this for that project."
};

// Claude Code run inside the Claude desktop app is the one Claude Code surface
// Headroom cannot reach. Headroom routes Claude Code by pointing it at a local
// proxy through your shell profile and ~/.claude/settings.json; Anthropic's
// desktop app uses neither, so its requests never pass through Headroom.
//
// The copy names Anthropic as the source of the limitation, because a user who
// is not told whose constraint it is reasonably concludes Headroom is broken.
// It stops at attributing the decision and does not speculate about why they
// made it, which we do not know.
const CLAUDE_DESKTOP_LIMITATION =
  "Claude Code inside the Claude desktop app cannot be optimized. Headroom routes Claude Code by pointing it at a local proxy via your shell profile and ~/.claude/settings.json, and Anthropic's desktop app uses neither, so its requests never reach Headroom. That is a design decision on Anthropic's side and nothing Headroom can work around. Run Claude Code from a terminal, or use the VS Code or JetBrains extension, and Headroom picks it up automatically.";

const connectorSupportWarnings: Record<string, string> = {
  claude_code: CLAUDE_DESKTOP_LIMITATION
};

// Two-letter badges for the home-banner connector cluster: with three or
// more connectors, full names push the banner headline onto a second line.
const connectorMonograms: Record<string, string> = {
  claude_code: "CC",
  codex: "CX",
  grok_build: "GK",
  opencode: "OC"
};

const connectorUnavailableReasons: Record<string, string> = {
  // A user whose only Claude Code is the one built into the Claude desktop app
  // lands here, because the CLI genuinely isn't installed. Say so, or they
  // reasonably conclude Headroom is broken rather than inapplicable.
  claude_code:
    "Claude Code was not detected. Install the Claude Code CLI and restart Headroom. Note that Claude Code inside the Claude desktop app cannot be optimized: Anthropic's desktop app does not use the CLI's configuration, so Headroom never sees its requests. That is their design decision, not something Headroom can configure around.",
  codex:
    "The Codex CLI was not detected. Headroom can still configure the ChatGPT app (previously Codex) and its IDE extension; install the CLI only if you also want terminal use.",
  grok_build:
    "Grok Build was not detected. Install Grok Build and restart Headroom.",
  opencode:
    "OpenCode was not detected. Install OpenCode and restart Headroom."
};

// Grok routing: UA-classified in the intercept, forwarded to api.x.ai via
// the backend's per-request x-headroom-base-url selection.
const GROK_CONNECTOR_ENABLED = true;

// OpenCode is visible in RC builds for end-to-end verification; keep the
// flag so it can ship dark in a stable if the RC pass surfaces problems.
const OPENCODE_CONNECTOR_ENABLED = true;

function withoutHiddenConnectors(list: ClientConnectorStatus[]) {
  return list.filter(
    (connector) =>
      (GROK_CONNECTOR_ENABLED || connector.clientId !== "grok_build") &&
      (OPENCODE_CONNECTOR_ENABLED || connector.clientId !== "opencode")
  );
}

// Connectors the Claude pricing gate neither auto-disables nor blocks
// enabling while the user is authenticated: Codex has its own proxy-side
// gate (codex_bypass); OpenCode bills against the user's own provider API
// keys, so the Claude gate has nothing to meter (no dedicated bypass).
const GATE_EXEMPT_CONNECTOR_IDS = new Set(["codex", "opencode", "grok_build"]);

const launcherConnectorFallback: ClientConnectorStatus[] = withoutHiddenConnectors([
  {
    clientId: "claude_code",
    name: "Claude Code",
    installed: false,
    enabled: false,
    verified: false
  },
  {
    clientId: "codex",
    name: "ChatGPT",
    installed: false,
    enabled: false,
    verified: false
  },
  {
    clientId: "grok_build",
    name: "Grok Build",
    installed: false,
    enabled: false,
    verified: false
  },
  {
    clientId: "opencode",
    name: "OpenCode",
    installed: false,
    enabled: false,
    verified: false
  }
]);

const idleBootstrapProgress: BootstrapProgress = {
  running: false,
  complete: false,
  failed: false,
  currentStep: "Idle",
  message: "Installer has not started.",
  currentStepEtaSeconds: 0,
  overallPercent: 0
};

const idleRuntimeUpgradeProgress: RuntimeUpgradeProgress = {
  running: false,
  complete: false,
  failed: false,
  currentStep: "Idle",
  message: "",
  overallPercent: 0,
  fromVersion: null,
  toVersion: null
};

const MAX_UPGRADE_AUTO_RETRIES = 2;

const GATE_AUTO_DISABLED_STORAGE_KEY = "headroom:gateAutoDisabledConnectors";

// Persisted "a connected tool's request has reached the proxy" marker, shared
// by both webviews (launcher onboarding verify + main-window poller). Without
// it, verification state lives only in one webview's RAM: every app restart
// (and every tray-open, since hidden webviews get timer-throttled) replays the
// "send a message to verify" nag even though traffic flowed all along.
const CONNECTOR_TRAFFIC_VERIFIED_STORAGE_KEY = "headroom:connectorTrafficVerified";

function isConnectorTrafficVerified(): boolean {
  try {
    return window.localStorage.getItem(CONNECTOR_TRAFFIC_VERIFIED_STORAGE_KEY) !== null;
  } catch {
    return false;
  }
}

function setConnectorTrafficVerified(verified: boolean): void {
  try {
    if (verified) {
      window.localStorage.setItem(
        CONNECTOR_TRAFFIC_VERIFIED_STORAGE_KEY,
        new Date().toISOString()
      );
    } else {
      window.localStorage.removeItem(CONNECTOR_TRAFFIC_VERIFIED_STORAGE_KEY);
    }
  } catch {
    // best effort
  }
}

// Fire-and-forget install-wizard funnel beacon. Errors (offline/mid-install)
// are swallowed so tracking never affects the wizard. Dedup is server-side
// (first-write-wins), so re-emitting a step on back-nav is harmless.
function reportFunnelStep(step: InstallWizardStep): void {
  void invoke("report_funnel_step", { step }).catch(() => {});
}

// Which funnel step each launcher stage's first paint represents. `install`
// (the landing screen) and `paywall` have no beacon: for new users the
// terms+email sign-up gate renders on top of the launcher, so the first real
// screen is captured by `signup_gate_shown`, not a launcher stage.
const LAUNCHER_STAGE_STEP: Partial<Record<LauncherStage, InstallWizardStep>> = {
  client_setup: "client_setup_shown",
  proxy_verify: "proxy_verify_started",
  post_install: "post_install_shown"
};

// Concrete first prompt for the post-install checklist: blank-page paralysis
// is a real drop-off cause between "agent connected" and "first prompt sent",
// so hand the user something they can paste that works in any repo.
const STARTER_PROMPT =
  "Read this project's main source file and its README in full, then explain what it does and one thing worth improving.";

// Live first-savings checklist. Rendered on the launcher's post-install stage
// AND as a Home card in the tray window: the launcher hides on click-away and
// the menu bar icon reopens the tray window, so without the Home card there
// is no way back to the checklist once it's been dismissed.
function FirstSavingsChecklist({
  dashboard,
  onReopenSetup
}: {
  dashboard: DashboardState;
  onReopenSetup: () => void;
}) {
  const [copied, setCopied] = useState(false);
  // A step stuck gray usually means traffic isn't reaching Headroom, not that
  // the user hasn't acted yet. Offer a setup re-check, but only after a grace
  // window: proxyReachable is false for ~1min on a healthy install while the
  // backend binds, so an immediate prompt would nag on good installs.
  const [showTroubleshoot, setShowTroubleshoot] = useState(false);
  useEffect(() => {
    const timer = window.setTimeout(() => setShowTroubleshoot(true), 20000);
    return () => window.clearTimeout(timer);
  }, []);
  return (
    <div className="post-install__checklist">
      <ol className="post-install__steps">
        <li className="post-install__step">
          <span
            className={`callout-banner__dot ${dashboard.lifetimeRequests > 0 ? "callout-banner__dot--healthy" : "callout-banner__dot--disconnected"}`}
            aria-hidden="true"
          />
          <div>
            <strong>Coding agent connected</strong>
            {dashboard.lifetimeRequests <= 0 && (
              <p>Open a terminal and start your agent — Headroom picks it up automatically.</p>
            )}
          </div>
        </li>
        <li className="post-install__step">
          <span
            className={`callout-banner__dot ${dashboard.firstPromptRequestSeen ? "callout-banner__dot--healthy" : "callout-banner__dot--disconnected"}`}
            aria-hidden="true"
          />
          <div>
            <strong>First prompt sent</strong>
            {!dashboard.firstPromptRequestSeen && (
              <p>Ask it anything, or paste the starter prompt below.</p>
            )}
          </div>
        </li>
        <li className="post-install__step">
          <span className="callout-banner__dot callout-banner__dot--disconnected" aria-hidden="true" />
          <div>
            <strong>First savings recorded</strong>
            <p>Use your AI coding agent as normal or paste the starter prompt below.</p>
          </div>
        </li>
      </ol>
      {dashboard.lifetimeEstimatedTokensSaved <= 0 &&
        dashboard.lifetimeEstimatedSavingsUsd <= 0 && (
        <div className="post-install__starter">
          <code>{STARTER_PROMPT}</code>
          <button
            className="secondary-button"
            onClick={() => {
              void navigator.clipboard?.writeText(STARTER_PROMPT).then(() => {
                setCopied(true);
                window.setTimeout(() => setCopied(false), 2000);
              });
            }}
            type="button"
          >
            {copied ? "Copied" : "Copy prompt"}
          </button>
        </div>
      )}
      {showTroubleshoot && (
        <button
          type="button"
          className="post-install__troubleshoot"
          onClick={onReopenSetup}
        >
          Not turning green? Re-check setup.
        </button>
      )}
    </div>
  );
}

const idleHeadroomLearnStatus: HeadroomLearnStatus = {
  running: false,
  progressPercent: 0,
  summary: "Select a project to run headroom learn.",
  outputTail: []
};

const idleHeadroomLearnPrereqStatus: HeadroomLearnPrereqStatus = {
  claudeCliAvailable: false,
  claudeCliPath: null,
  codexCliAvailable: false,
  codexCliPath: null,
  codexLoggedIn: false
};

const CLAUDE_CODE_INSTALL_DOCS_URL = "https://docs.claude.com/en/docs/claude-code/setup";
const CLAUDE_CODE_INSTALL_CURL_CMD = "curl -fsSL https://claude.ai/install.sh | bash";
const CODEX_CLI_INSTALL_CMD = "npm install -g @openai/codex";
const CODEX_CLI_LOGIN_CMD = "codex login";
const CODEX_INSTALL_DOCS_URL = "https://developers.openai.com/codex/cli";
const CODEX_INSTALL_NPM_CMD = "npm i -g @openai/codex";

const APPSUMO_ACCOUNT_URL = "https://appsumo.com/account/products/";

const SALES_CONTACT_URL = (
  import.meta.env.VITE_HEADROOM_SALES_CONTACT_URL ??
  ""
).trim() || "mailto:hello@example.com";
const CONTACT_FORM_URL = (
  import.meta.env.VITE_HEADROOM_CONTACT_FORM_URL ??
  ""
).trim();

type StartupPhase = "window" | "dashboard" | "bootstrap" | "runtime" | "ready";

// Values must match User::CANCELLATION_REASONS server-side; anything else is
// rejected by /desktop/cancellation_intent.
const CANCELLATION_REASONS: { value: string; label: string }[] = [
  { value: "too_expensive", label: "Too expensive" },
  { value: "not_enough_savings", label: "Not saving me enough tokens" },
  { value: "not_using_it", label: "Not using it enough" },
  { value: "switched", label: "Switched to something else" },
  { value: "technical_problems", label: "Ran into technical problems" },
  { value: "other", label: "Something else" }
];

const authCodeExpiryFallbackSeconds = 900;
const APP_UPDATE_BACKGROUND_INITIAL_DELAY_MS = 12_000;
const APP_UPDATE_BACKGROUND_CHECK_INTERVAL_MS = 60 * 60 * 1000;
// Desktop activation is a one-shot "this device is live" ping to headroom-web.
// It is not worth an unbounded retry: a machine that is offline now is usually
// offline for a while, and every attempt costs a Sentry warning. Five attempts
// spread over ~8 minutes, then wait for the next launch.
const DESKTOP_ACTIVATION_MAX_ATTEMPTS = 5;
const DESKTOP_ACTIVATION_RETRY_BASE_MS = 30_000;
const DESKTOP_ACTIVATION_RETRY_MAX_MS = 5 * 60 * 1000;

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

  const providerSavings = mergeProviderSavingsForDisplay(point.byProvider ?? []);
  // The backend's by_provider rollup has no cache dimension, so the bucket's
  // compressible share is pro-rated across its providers. Exact whenever one
  // connector was active in the hour (the common case); an approximation only
  // when two ran concurrently with different cache hit rates.
  const costScale = point.actualCostUsd > 0 ? point.compressibleCostUsd / point.actualCostUsd : 1;
  const tokenScale =
    point.totalTokensSent > 0 ? point.compressibleTokensSent / point.totalTokensSent : 1;

  return (
    <div className="savings-chart__tooltip">
      <strong>{point.bucketLabel}</strong>
      {providerSavings.length > 0
        ? // Hourly buckets carry per-provider attribution: show Saved/Spent per
          // connector instead of the bucket total (which would be redundant).
          providerSavings.map((provider) => (
            <div className="savings-chart__tooltip-group" key={provider.label}>
              <span className="savings-chart__tooltip-label">{provider.label}</span>
              <span className="savings-chart__tooltip-item">
                <i
                  aria-hidden="true"
                  className={`savings-chart__tooltip-dot savings-chart__tooltip-dot--${
                    chartMode === "usd" ? "saved-usd" : "saved-tokens"
                  }`}
                />
                {/* "Input saved", not "Saved": the output-shaping group below
                    is bucket-level (upstream's by_provider rollup has no output
                    dimension), so an unqualified "Saved" here would read as the
                    connector's whole contribution. */}
                {chartMode === "usd"
                  ? `Input saved ${currencyExact(provider.estimatedSavingsUsd)}`
                  : `Input saved ${compactNumber(provider.estimatedTokensSaved)} tokens`}
              </span>
              <span className="savings-chart__tooltip-item">
                <i
                  aria-hidden="true"
                  className={`savings-chart__tooltip-dot savings-chart__tooltip-dot--${
                    chartMode === "usd" ? "actual-usd" : "actual-tokens"
                  }`}
                />
                {/* "Spent" for brevity; the figure is the compressible slice,
                    matching the bar and the chip's denominator. */}
                {chartMode === "usd"
                  ? `Spent ${currencyExact(provider.actualCostUsd * costScale)}`
                  : `Spent ${compactNumber(provider.totalTokensSent * tokenScale)} tokens`}
              </span>
            </div>
          ))
        : // Monthly buckets (and pre-attribution hourly buckets) have no provider
          // dimension: fall back to the aggregate bucket total.
        chartMode === "usd" ? (
          <div className="savings-chart__tooltip-group">
            <span className="savings-chart__tooltip-label">Dollars</span>
            <span className="savings-chart__tooltip-item">
              <i
                aria-hidden="true"
                className="savings-chart__tooltip-dot savings-chart__tooltip-dot--saved-usd"
              />
              Input saved {currencyExact(point.estimatedSavingsUsd)}
            </span>
            <span className="savings-chart__tooltip-item">
              <i
                aria-hidden="true"
                className="savings-chart__tooltip-dot savings-chart__tooltip-dot--actual-usd"
              />
              Spent {currencyExact(point.compressibleCostUsd)}
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
              Input saved {compactNumber(point.estimatedTokensSaved)} tokens
            </span>
            <span className="savings-chart__tooltip-item">
              <i
                aria-hidden="true"
                className="savings-chart__tooltip-dot savings-chart__tooltip-dot--actual-tokens"
              />
              Spent {compactNumber(point.compressibleTokensSent)} tokens
            </span>
          </div>
        )}
      {/* Output shaping has no per-provider breakdown, so it always renders as a
          single bucket-level row under whichever branch ran above. */}
      {(chartMode === "usd" ? point.outputSavingsUsd : point.outputTokensSaved) > 0 && (
        <div className="savings-chart__tooltip-group">
          <span className="savings-chart__tooltip-label">Output shaping</span>
          <span className="savings-chart__tooltip-item">
            <i
              aria-hidden="true"
              className={`savings-chart__tooltip-dot savings-chart__tooltip-dot--${
                chartMode === "usd" ? "saved-usd" : "saved-tokens"
              } savings-chart__tooltip-dot--output`}
            />
            {chartMode === "usd"
              ? `Saved ${currencyExact(point.outputSavingsUsd)}`
              : `Saved ${compactNumber(point.outputTokensSaved)} tokens`}
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

type SavingsChartView = "month" | "day";
type SavingsChartMode = "usd" | "tokens";

// Output-token reduction from the proxy's output shaper, shown as a secondary
// line inside the "Total input tokens saved" card so the two numbers (input
// tokens saved vs. output tokens not emitted) read as distinct. The line shows
// just the headline percent; clicking it opens a popover with the method
// ("estimated"/"measured"), confidence band, request count, and a note that
// output savings are counterfactual. Caller renders this only when `reduction`
// is present (the backend returns null until a verbosity baseline is seeded).
// The parent card is itself clickable, so the trigger stops event propagation.
function WindowRateChip({
  label,
  dot,
  title,
  badge,
  value,
  rows,
  note,
  popSide = "right"
}: {
  label: string;
  dot?: "input" | "output";
  title: string;
  badge: string;
  value: string;
  rows: Array<{ dt: string; dd: string }>;
  note: string;
  popSide?: "right" | "left";
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: Event) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: Event) => {
      if ((e as KeyboardEvent).key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="output-chip" ref={ref}>
      <button
        type="button"
        className={`output-chip__button${badge === "estimated" ? " output-chip__button--estimated" : ""}${open ? " is-open" : ""}`}
        aria-expanded={open}
        aria-label={`${title} details`}
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        onKeyDown={(e) => e.stopPropagation()}
      >
        {dot ? (
          <span
            className={`output-chip__dot${dot === "input" ? " output-chip__dot--input" : ""}`}
            aria-hidden="true"
          />
        ) : null}
        {label}
      </button>
      {open ? (
        <div
          className={`output-chip__popover${popSide === "left" ? " output-chip__popover--flip" : ""}`}
          role="dialog"
          aria-label={`${title} details`}
          onClick={(e) => e.stopPropagation()}
        >
          <div className="output-chip__pop-head">
            <span className="output-chip__pop-title">{title}</span>
            <span className="output-chip__pop-badge">{badge}</span>
          </div>
          <div className="output-chip__pop-value">{value}</div>
          <dl className="output-chip__pop-stats">
            {rows.map((row) => (
              <div key={row.dt}>
                <dt>{row.dt}</dt>
                <dd>{row.dd}</dd>
              </div>
            ))}
          </dl>
          <p className="output-chip__pop-note">{note}</p>
        </div>
      ) : null}
    </div>
  );
}

function OutputReductionChip({
  reduction,
  flip = false,
  allTimeFallback = false
}: {
  reduction: OutputReduction;
  flip?: boolean;
  /** Rendered because the visible window has no samples of its own, so this
   * is the lifetime figure standing in. Says so, rather than letting an
   * all-time number read as that period's. */
  allTimeFallback?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const isMeasured = reduction.method === "measured";

  useEffect(() => {
    if (!open) return;
    const onDown = (e: Event) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: Event) => {
      if ((e as KeyboardEvent).key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="output-chip" ref={ref}>
      <button
        type="button"
        className={`output-chip__button${isMeasured ? "" : " output-chip__button--estimated"}${open ? " is-open" : ""}`}
        aria-expanded={open}
        aria-label="Output token reduction details"
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        onKeyDown={(e) => e.stopPropagation()}
      >
        <span className="output-chip__dot" aria-hidden="true" />
        Output −{percent1(reduction.reductionPercent)}%
      </button>
      {open ? (
        <div
          className={`output-chip__popover${flip ? " output-chip__popover--flip" : ""}`}
          role="dialog"
          aria-label="Output reduction details"
          onClick={(e) => e.stopPropagation()}
        >
          <div className="output-chip__pop-head">
            <span className="output-chip__pop-title">
              Output token reduction{allTimeFallback ? " · all-time" : ""}
            </span>
            <span className="output-chip__pop-badge">{isMeasured ? "measured" : "estimated"}</span>
          </div>
          <div className="output-chip__pop-value">{percent1(reduction.reductionPercent)}%</div>
          <dl className="output-chip__pop-stats">
            <div>
              <dt>95% CI</dt>
              <dd>
                {percent1(reduction.ciLowPercent)}–{percent1(reduction.ciHighPercent)}%
              </dd>
            </div>
            <div>
              <dt>Requests</dt>
              <dd>{compactNumber(reduction.requests)}</dd>
            </div>
          </dl>
          <p className="output-chip__pop-note">
            {allTimeFallback ? "No output samples landed in this window, so this is the all-time figure, not this period's. " : ""}
            {isMeasured
              ? "Output tokens the model didn't emit because the shaper steered verbosity / routed effort down — measured against an unshaped A/B holdout."
              : "Output tokens the model didn't emit because the shaper steered verbosity / routed effort down. Output savings are counterfactual, so this is an estimate vs a learned baseline."}
          </p>
        </div>
      ) : null}
    </div>
  );
}

function DailySavingsChart({
  data,
  hourlyData,
  resetSignal,
  chartMode,
  setChartMode,
  outputReduction
}: {
  data: DailySavingsPoint[];
  hourlyData: HourlySavingsPoint[];
  resetSignal: number;
  chartMode: SavingsChartMode;
  setChartMode: (mode: SavingsChartMode) => void;
  outputReduction: OutputReduction | null;
}) {
  const currentMonth = startOfMonth(new Date());
  const today = startOfDay(new Date());
  const [visibleMonth, setVisibleMonth] = useState(() => currentMonth);
  const [visibleDay, setVisibleDay] = useState(() => today);
  const [view, setView] = useState<SavingsChartView>("day");
  const [savingsTodayUsd, setSavingsTodayUsd] = useState<number | null>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<number>("savings-today-updated", (event) => {
      setSavingsTodayUsd(event.payload);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);
  const firstSavingsMonth = earliestSavingsMonth(data);
  const firstHourlyDay = earliestHourlyDay(hourlyData);
  const monthlyWindow = buildMonthlySavingsWindow(data, visibleMonth);
  const hourlyWindow = buildHourlySavingsWindow(hourlyData, visibleDay);
  const monthlyData = buildMonthlySavingsChartData(monthlyWindow);
  const hourlyChartData = buildHourlySavingsChartData(hourlyWindow);
  const chartData = view === "month" ? monthlyData : hourlyChartData;
  // Canonical rate under the headline: input compression as a share of the
  // BILLABLE input for the visible window. Always priced in dollars regardless
  // of the chart's unit toggle -- a rate is unit-free, and only the dollar
  // figures share one scale (see compressibleInputSavingsRate). Output shaping
  // stays out of every percentage (it keeps its own measured reduction chip);
  // cache reads stay out of the denominator (~0.1x, deliberately untouched).
  const compressibleRate = compressibleInputSavingsRate(
    view === "month" ? monthlyWindow : hourlyWindow
  );
  // Window-scoped output reduction from the locally-sampled series, falling
  // back to the all-time figure for any window without samples of its own.
  // The fallback labels itself all-time (see OutputReductionChip) so it can't
  // read as that period's number — but it does render: since the estimator
  // only scores strata its baseline actually observed, a window of purely
  // unscored traffic has no samples at all, and showing nothing there reads
  // as "the shaper did nothing" rather than "this window can't be measured".
  const windowOutput = outputReductionForWindow(
    view === "month" ? monthlyWindow : hourlyWindow
  );
  const canViewPreviousMonth = firstSavingsMonth ? visibleMonth > firstSavingsMonth : false;
  const canViewNextMonth = visibleMonth < currentMonth;
  const canViewPreviousDay = firstHourlyDay ? visibleDay > firstHourlyDay : false;
  const canViewNextDay = visibleDay < today;
  const label = view === "month" ? formatMonthLabel(visibleMonth) : formatSelectedDayLabel(visibleDay);
  // Headline totals cover all three Headroom layers -- input compression,
  // output shaping, and tool-schema deferral -- matching the breakdown rows in
  // the savings modal and the segments stacked in the bars below. The provider
  // cache is deliberately NOT among them: it works with Headroom out of the
  // path entirely, so it is not a benefit of running Headroom. Deferral is the
  // opposite -- those definitions are re-sent on every request unless Headroom
  // holds them back -- and it is priced at the cache-read rate upstream, since
  // they would have been cache reads after the session's first request.
  // Buckets before 2026-09-02 carry no deferral figure (the backend only ever
  // exposed a lifetime total), so older bars understate that layer.
  // The live tray figure for today already sums the layers it knows, so it can
  // stand in for the bucket sum while today is still open.
  const chartSaved = Math.max(
    0,
    chartMode === "usd"
      ? view === "day" && visibleDay >= today && savingsTodayUsd !== null
        ? savingsTodayUsd
        : chartData.reduce(
            (s, d) => s + d.estimatedSavingsUsd + d.outputSavingsUsd + (d.toolSchemaSavingsUsd ?? 0),
            0
          )
      : chartData.reduce(
          (s, d) => s + d.estimatedTokensSaved + d.outputTokensSaved + (d.toolSchemaTokensSaved ?? 0),
          0
        )
  );

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
                $ costs
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
              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <path d="M10 3L5 8l5 5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
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
              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <path d="M6 3l5 5-5 5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
          </div>
        </div>
        <div className="savings-chart__canvas savings-chart__canvas--combined">
          <div className="savings-chart__overlay" aria-hidden="true">
            <span className="savings-chart__overlay-total">
              {chartMode === "usd" ? currency(chartSaved) : compactNumber(chartSaved)}
            </span>
            <span className="savings-chart__overlay-label">
              {view === "day" ? "saved today" : "saved this month"}
            </span>
            {compressibleRate !== null || windowOutput !== null || outputReduction ? (
              <span className="savings-chart__overlay-chips">
                {compressibleRate !== null && (
                  <WindowRateChip
                    dot="input"
                    label={`Input −${Math.round(compressibleRate.pct)}%`}
                    title="Input compression"
                    badge="measured"
                    value={`${Math.round(compressibleRate.pct)}%`}
                    rows={[
                      { dt: "Removed", dd: currency(compressibleRate.saved) },
                      {
                        dt: "Compressible input",
                        dd: currency(compressibleRate.saved + compressibleRate.remaining)
                      }
                    ]}
                    note="Excludes cache reads."
                  />
                )}
                {windowOutput !== null ? (
                  <WindowRateChip
                    dot="output"
                    popSide="left"
                    label={`Output −${Math.round(windowOutput.pct)}%`}
                    title="Output token reduction"
                    badge="estimated"
                    value={`${Math.round(windowOutput.pct)}%`}
                    rows={[
                      { dt: "Avoided", dd: compactNumber(windowOutput.savedTokens) },
                      { dt: "Baseline", dd: compactNumber(windowOutput.baselineTokens) },
                      ...(outputReduction
                        ? [{ dt: "All-time", dd: `${percent1(outputReduction.reductionPercent)}%` }]
                        : [])
                    ]}
                    note="Estimate vs the shaper's learned baseline, sampled while the app runs."
                  />
                ) : outputReduction ? (
                  <OutputReductionChip allTimeFallback flip reduction={outputReduction} />
                ) : null}
              </span>
            ) : null}
          </div>
          <ResponsiveContainer height="100%" width="100%">
            <BarChart
              barCategoryGap="5%"
              barGap={1}
              data={chartData}
              // Clears the overlay: total + label + the chip row added in 0.7.9
              // measure ~72px from the canvas top, so this is that plus a few
              // px of breathing room -- not slack to be reclaimed twice.
              margin={{ top: 82, right: 2, left: 2, bottom: 0 }}
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
                {/* Output shaping sits on top of compression in the same hue
                    family, one shade lighter, so it reads as a second layer of
                    the same thing rather than a separate metric. */}
                <linearGradient id="outputUsdGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#63b3a4" />
                  <stop offset="100%" stopColor="#8CCCBE" />
                </linearGradient>
                <linearGradient id="outputTokensGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#e2cf6a" stopOpacity="0.3" />
                  <stop offset="100%" stopColor="#F3E2A4" stopOpacity="0.22" />
                </linearGradient>
                {/* Tool-schema deferral, the third layer. Same family again,
                    one further step out, so the bar reads as three shades of
                    "Headroom removed this" rather than three unrelated
                    metrics. Chart-only SVG gradient stops, matching the two
                    above -- the CSS token rule covers component CSS. */}
                <linearGradient id="toolSchemaUsdGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#4f9d8f" />
                  <stop offset="100%" stopColor="#74BDB0" />
                </linearGradient>
                <linearGradient id="toolSchemaTokensGradient" x1="0" x2="0" y1="0" y2="1">
                  <stop offset="0%" stopColor="#eddfa0" stopOpacity="0.28" />
                  <stop offset="100%" stopColor="#F7EEC9" stopOpacity="0.2" />
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
              {/* Both axes are hidden, so recharts' default "nice" rounding
                  buys nothing but empty space above the tallest bar: pin the
                  domain to the data so the peak bucket fills the plot. */}
              <YAxis domain={[0, "dataMax"]} hide yAxisId="usd" />
              <YAxis domain={[0, "dataMax"]} hide yAxisId="tokens" />
              <Tooltip content={(props) => <SavingsChartTooltip {...props} chartMode={chartMode} />} cursor={{ fill: "rgba(36, 31, 29, 0.05)" }} />
              {chartMode === "usd" && (
                <>
                  <Bar
                    dataKey="compressibleCostUsd"
                    fill="url(#actualUsdGradient)"
                    maxBarSize={16}
                    stackId="usd"
                    yAxisId="usd"
                  />
                  <Bar
                    dataKey="estimatedSavingsUsd"
                    fill="url(#savingsUsdGradient)"
                    maxBarSize={16}
                    // Kept on both savings segments: the output bar is empty on
                    // buckets that predate the layer, and a 1px cap under a
                    // present output segment is invisible anyway.
                    radius={[1, 1, 0, 0]}
                    stackId="usd"
                    yAxisId="usd"
                  />
                  <Bar
                    dataKey="outputSavingsUsd"
                    fill="url(#outputUsdGradient)"
                    maxBarSize={16}
                    radius={[1, 1, 0, 0]}
                    stackId="usd"
                    yAxisId="usd"
                  />
                  <Bar
                    dataKey="toolSchemaSavingsUsd"
                    fill="url(#toolSchemaUsdGradient)"
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
                    dataKey="compressibleTokensSent"
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
                  <Bar
                    dataKey="outputTokensSaved"
                    fill="url(#outputTokensGradient)"
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
                          stroke="#F3E2A4"
                          strokeWidth={sw}
                          rx={1}
                        />
                      );
                    }}
                  />
                  <Bar
                    dataKey="toolSchemaTokensSaved"
                    fill="url(#toolSchemaTokensGradient)"
                    maxBarSize={16}
                    stackId="tokens"
                    yAxisId="tokens"
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

function AddonClientChips({
  connectors,
  savings
}: {
  connectors: ClientConnectorStatus[];
  savings?: string | null;
}) {
  const clients = sortClientConnectors(aggregateClientConnectors(connectors));
  if (clients.length === 0 && !savings) {
    return null;
  }
  return (
    <div className="addon-card__clients">
      {clients.map((connector) => {
        const status = connectorDashboardStatus(connector);
        return (
          <span
            className="callout-banner__chip"
            key={connector.clientId}
            title={status.label}
          >
            <span
              className={`callout-banner__chip-dot callout-banner__chip-dot--${status.tone}`}
              aria-hidden="true"
            />
            <span className="callout-banner__chip-name">{connector.name}</span>
            <span className="visually-hidden">{status.label}</span>
          </span>
        );
      })}
      {savings ? (
        <span className="callout-banner__chip" title="Savings">
          <span
            className="callout-banner__chip-dot callout-banner__chip-dot--active"
            aria-hidden="true"
          />
          <span className="callout-banner__chip-name">{savings}</span>
        </span>
      ) : null}
    </div>
  );
}

function formatAddonVersion(version: string): string {
  return /^\d/.test(version) ? `v${version}` : version;
}

function AddonCard({
  name,
  version,
  installed,
  enabled,
  description,
  copy,
  infoOpen,
  onToggleInfo,
  busy,
  busyLabel,
  resultMessage,
  onDismissResult,
  sourceUrl,
  onOpenSource,
  connectors,
  showClients,
  savings,
  actionsDisabled,
  onInstall,
  onToggleEnabled,
  onUninstall,
  updateAvailable,
  onUpdate,
  availableVersion,
  unavailableReason,
  children
}: {
  name: string;
  version?: string | null;
  installed: boolean;
  enabled: boolean;
  description: ReactNode;
  copy?: AddonCopy;
  infoOpen: boolean;
  onToggleInfo: () => void;
  busy: boolean;
  busyLabel: string | null;
  resultMessage: string | null;
  onDismissResult: () => void;
  sourceUrl: string;
  onOpenSource: () => void;
  connectors: ClientConnectorStatus[];
  showClients: boolean;
  savings?: string | null;
  actionsDisabled: boolean;
  onInstall: () => void;
  onToggleEnabled: () => void;
  onUninstall: () => void;
  updateAvailable?: boolean;
  onUpdate?: () => void;
  availableVersion?: string | null;
  /** Platform has no installable build: gray the card, drop the actions. */
  unavailableReason?: string | null;
  children?: ReactNode;
}) {
  return (
    <li className={`addon-card${unavailableReason ? " addon-card--unavailable" : ""}`}>
      <div className="addon-card__body">
        <div className="addon-card__heading">
          <span className="addon-card__name">{name}</span>
          {installed && version ? (
            <span className="addon-card__version">{formatAddonVersion(version)}</span>
          ) : null}
          {copy ? (
            <button
              type="button"
              className="addon-card__info"
              aria-label={`What ${name} does`}
              aria-expanded={infoOpen}
              onClick={onToggleInfo}
            >
              i
            </button>
          ) : null}
          {installed ? (
            <span
              className={`addon-card__badge addon-card__badge--${enabled ? "on" : "off"}`}
            >
              {enabled ? "Enabled" : "Disabled"}
            </span>
          ) : null}
        </div>
        {infoOpen && copy ? (
          <p className="addon-card__info-text">{copy.whatItDoes}</p>
        ) : null}
        <p className="addon-card__description">{description}</p>
        {showClients ? (
          <AddonClientChips connectors={connectors} savings={savings} />
        ) : null}
        <button type="button" className="addon-card__link" onClick={onOpenSource}>
          {sourceUrl}
        </button>
        {unavailableReason ? (
          <p className="addon-card__notice">{unavailableReason}</p>
        ) : null}
        {busy && busyLabel ? (
          <p className="addon-card__progress">{busyLabel}</p>
        ) : resultMessage ? (
          <p className="addon-card__result">
            {resultMessage}
            <button
              type="button"
              className="addon-card__result-dismiss"
              aria-label="Dismiss"
              onClick={onDismissResult}
            >
              ×
            </button>
          </p>
        ) : null}
        {children}
      </div>
      <div className="addon-card__actions">
        {unavailableReason ? (
          <button type="button" className="addon-card__action" disabled>
            Unavailable
          </button>
        ) : !installed ? (
          <button
            type="button"
            className="addon-card__action addon-card__action--primary"
            disabled={actionsDisabled}
            onClick={onInstall}
          >
            Install
          </button>
        ) : (
          <>
            {updateAvailable && onUpdate ? (
              // install_addon is idempotent and always installs the pinned
              // version, so it is also the upgrade path -- no second command.
              <button
                type="button"
                className="addon-card__action addon-card__action--primary"
                disabled={actionsDisabled}
                onClick={onUpdate}
              >
                {availableVersion
                  ? `Update to ${formatAddonVersion(availableVersion)}`
                  : "Update"}
              </button>
            ) : null}
            <button
              type="button"
              className="addon-card__action"
              disabled={actionsDisabled}
              onClick={onToggleEnabled}
            >
              {enabled ? "Disable" : "Enable"}
            </button>
            <button
              type="button"
              className="addon-card__action addon-card__action--danger"
              disabled={actionsDisabled}
              onClick={onUninstall}
            >
              Uninstall
            </button>
          </>
        )}
      </div>
    </li>
  );
}

const ADDON_DISPLAY_ORDER = [
  "ponytail",
  "serena",
  "codebase-memory",
  "context7",
  "markitdown",
  "caveman"
];

// Unknown ids land after the curated ones, before the trailing RTK card.
function addonDisplayRank(id: string): number {
  const rank = ADDON_DISPLAY_ORDER.indexOf(id);
  return rank === -1 ? ADDON_DISPLAY_ORDER.length : rank;
}

function buildAddonRequestMailto(): string {
  const subject = "Addon request";
  const body =
    "Which addon would you like to see in Headroom?\n\n\n" +
    "What would it do for you / which tool does it wrap?\n\n\n";
  return `mailto:product@extraheadroom.com?subject=${encodeURIComponent(subject)}&body=${encodeURIComponent(body)}`;
}

function buildUpgradeIssueMailto(failure: RuntimeUpgradeFailure): string {
  const subject = `Headroom update issue (${failure.targetHeadroomVersion}, ${failure.failurePhase})`;
  const diagnosticLines = [
    `App version: ${failure.appVersion}`,
    `Target Headroom: ${failure.targetHeadroomVersion}`,
    failure.fallbackHeadroomVersion
      ? `Fallback running: ${failure.fallbackHeadroomVersion}`
      : null,
    `Failure phase: ${failure.failurePhase}`,
    `Attempts: ${failure.attempts}`,
    `First attempt: ${failure.firstAttemptAt}`,
    `Last attempt: ${failure.lastAttemptAt}`,
    `Rollback restored: ${failure.rollbackRestored ? "yes" : "no"}`,
    "",
    "Error:",
    failure.errorMessage,
  ].filter((line): line is string => line !== null);
  const body =
    "What were you doing when this happened?\n\n\n" +
    "---\n" +
    "Diagnostic info (please keep):\n" +
    diagnosticLines.join("\n");
  return `mailto:support@extraheadroom.com?subject=${encodeURIComponent(subject)}&body=${encodeURIComponent(body)}`;
}

interface ProxyVerificationRow {
  clientId: string;
  name: string;
  state: "processing" | "waiting" | "verified";
  message: string;
}


export default function App() {
  const [dashboard, setDashboard] = useState<DashboardState>(mockDashboard);
  const [addonBusyId, setAddonBusyId] = useState<string | null>(null);
  const [addonBusyLabel, setAddonBusyLabel] = useState<string | null>(null);
  const [addonInfoId, setAddonInfoId] = useState<string | null>(null);
  const [addonResult, setAddonResult] = useState<{ id: string; message: string } | null>(null);
  const [addonError, setAddonError] = useState<string | null>(null);
  const [bootstrapping, setBootstrapping] = useState(false);
  const [bootstrapProgress, setBootstrapProgress] =
    useState<BootstrapProgress>(idleBootstrapProgress);
  const [runtimeUpgradeProgress, setRuntimeUpgradeProgress] =
    useState<RuntimeUpgradeProgress>(idleRuntimeUpgradeProgress);
  const [bootstrapError, setBootstrapError] = useState<string | null>(null);
  const [windowLabel, setWindowLabel] = useState<"main" | "launcher" | null>(null);
  const [startupPhase, setStartupPhase] = useState<StartupPhase>("window");
  const [startupPercent, setStartupPercent] = useState(10);
  const [startupCopy, setStartupCopy] = useState("Opening launch window…");
  const [startupReady, setStartupReady] = useState(false);
  const [activeView, setActiveView] = useState<TrayView>("home");
  const [pricingAudience, setPricingAudience] = useState<PricingAudience>("individual");
  // Annual first: it is the cheaper per-month number and the tab the "Save 25%"
  // badge points at. A subscriber's own period overrides this once it loads.
  const [billingPeriod, setBillingPeriod] = useState<BillingPeriod>("annual");
  // Launcher stage is a single source of truth for which onboarding screen
  // is showing. Only one screen can be active at a time; transitions go
  // through `setLauncherStage` so implicit renders from bootstrap/dashboard
  // flags cannot bypass the install step's readiness gate.
  const [launcherStage, setLauncherStage] = useState<LauncherStage>("install");
  // Set while a headroom://auth magic link is being redeemed. The launcher is
  // already on screen by then (the deep-link handler shows it), so without this
  // the sign-in ran invisibly behind whatever onboarding stage it landed on.
  const [magicLinkState, setMagicLinkState] = useState<MagicLinkState | null>(null);
  // Paywall-first experiment flag, served from the Rust-side cache (never
  // blocks). Gated flow applies only to fresh installs: an installed runtime
  // means the user is grandfathered and sees zero difference.
  const [launchFlags, setLaunchFlags] = useState<LaunchFlags | null>(null);
  // Set when the app has been up past the stall window with zero savings on
  // record. Held in state (not rendered immediately) so the modal is waiting
  // whenever the user next opens the tray, rather than stealing focus.
  const [setupStall, setSetupStall] = useState<SetupStallAlert | null>(null);
  // Same signals as the modal above, but for the always-on Home banner, whose
  // default copy tells a user with zero savings to check back later.
  const [stallBannerLine, setStallBannerLine] = useState<string | null>(null);
  const [debugOverrides, setDebugOverrides] = useState<DebugOverrides | null>(null);
  const [connectors, setConnectors] = useState<ClientConnectorStatus[]>([]);
  const [openConnectorHelpId, setOpenConnectorHelpId] = useState<string | null>(null);
  const [openConnectorWarningId, setOpenConnectorWarningId] = useState<string | null>(null);
  const [connectorsBusy, setConnectorsBusy] = useState(false);
  const [connectorPhase, setConnectorPhase] = useState<"disabled" | "verifying" | "healthy">(
    () => (isConnectorTrafficVerified() ? "healthy" : "verifying")
  );
  const [connectorsError, setConnectorsError] = useState<string | null>(null);
  const [connectorsNotice, setConnectorsNotice] = useState<string | null>(null);
  const [proxyVerificationRows, setProxyVerificationRows] = useState<ProxyVerificationRow[]>([]);
  const [proxyVerificationHint, setProxyVerificationHint] = useState<
    { text: string; tone: "info" | "error" } | null
  >(null);
  // Leaving the verify step unverified takes two clicks: the first arms the
  // warning, the second leaves. 86% of installs used to click straight past
  // this screen (median 26s, 45% under 15s) and the ones that did went on to
  // send a first prompt 59% of the time vs 76% for the ones that waited -- the
  // single biggest activation leak in onboarding, and invisible in support
  // reports because nothing errors.
  const [proxyVerifySkipArmed, setProxyVerifySkipArmed] = useState(false);
  const proxyVerificationRequestAnchorRef = useRef<Record<string, number> | null>(null);
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatus | null>(null);
  // Fresh install (no runtime on disk yet). Drives the onboarding email-harvest
  // step: every fresh install collects an email to start the card-free 7-day
  // trial. The old paywall-first checkout gate has been retired, so this no
  // longer depends on the launch flag.
  const paywallFirstFlow = runtimeStatus?.installed === false;
  // Verify against the always-up 6767 intercept (which counts passthrough
  // traffic) instead of the backend whenever the backend won't be optimizing:
  // pre-install, or when the pricing gate has bypassed it (e.g. ended trial).
  // Otherwise proxy_verify waits forever on a backend that never comes up.
  const interceptOnlyVerify =
    paywallFirstFlow || runtimeStatus?.bypassed === true;
  const [resuming, setResuming] = useState(false);
  const [resumeError, setResumeError] = useState<string | null>(null);
  const [appUpdateConfig, setAppUpdateConfig] = useState<AppUpdateConfiguration | null>(null);
  const [appUpdateAvailable, setAppUpdateAvailable] = useState<AvailableAppUpdate | null>(null);
  const [appUpdateBusy, setAppUpdateBusy] = useState(false);
  const [appUpdateInstallBusy, setAppUpdateInstallBusy] = useState(false);
  const [appUpdateRestartBusy, setAppUpdateRestartBusy] = useState(false);
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
  const [optimizeAppliedByProject, setOptimizeAppliedByProject] =
    useState<Record<string, AppliedPatterns> | null>(null);
  const [optimizeAppliedRefreshTick, setOptimizeAppliedRefreshTick] = useState(0);
  const previousHeadroomLearnRunningRef = useRef(false);
  const [headroomLearnBusy, setHeadroomLearnBusy] = useState(false);
  const [headroomLearnPrereq, setHeadroomLearnPrereq] =
    useState<HeadroomLearnPrereqStatus>(idleHeadroomLearnPrereqStatus);
  const [activityFeed, setActivityFeed] = useState<ActivityFeedResponse>({
    tiles: {
      transformation: null,
      record: null,
      rtkToday: null,
      serenaToday: null,
      learningsMilestone: null,
      weeklyRecap: null,
      trainSuggestion: null
    },
    proxyReachable: false
  });
  // Flipped true after the first activity feed fetch attempt resolves (success
  // OR failure). Before this the feed holds a placeholder value whose
  // `proxyReachable: false` would falsely render the "proxy unreachable"
  // empty state and make the tab feel like it's already in an error state.
  const [activityFeedLoaded, setActivityFeedLoaded] = useState(false);
  // Tray window focus proxies for visibility: the window auto-hides on blur
  // via `triggerHide`, so "not focused" ⇒ "hidden" for polling purposes.
  const [trayWindowFocused, setTrayWindowFocused] = useState(true);
  // Sticky flag: the user has visited a heavy-data tab (Activity or Optimize)
  // at least once this session. The tray-focus pre-warm is gated on this so
  // users who stay on Home don't pay its IPC/subprocess cost on every focus.
  const [heavyTabEverOpened, setHeavyTabEverOpened] = useState(false);
  const activityTabTrackedRef = useRef(false);
  const [activityFeedError, setActivityFeedError] = useState<string | null>(null);
  const [pricingStatus, setPricingStatus] = useState<HeadroomPricingStatus | null>(null);
  const [cachedPricing] = useState<CachedPricing>(() => readCachedPricing());
  const [pricingBusy, setPricingBusy] = useState(false);
  const [pricingError, setPricingError] = useState<string | null>(null);
  const pricingRefreshInFlightRef = useRef(false);
  // When the last authoritative status (verify / sign-out) was applied. A slow
  // pricing fetch issued before it must not land afterwards and overwrite it:
  // on a fresh install the very first fetch is the slowest, and it was still in
  // flight when the magic link signed the user in, so it clobbered the signed-in
  // status with its own stale signed-out one.
  const pricingStatusStampRef = useRef(0);
  const [authEmail, setAuthEmail] = useState("");
  const [authCode, setAuthCode] = useState("");
  const [authCodeRequestedFor, setAuthCodeRequestedFor] = useState<string | null>(null);
  const [authRequestBusy, setAuthRequestBusy] = useState(false);
  const [authVerifyBusy, setAuthVerifyBusy] = useState(false);
  const [authFlowError, setAuthFlowError] = useState<string | null>(null);
  const [authFlowSuccess, setAuthFlowSuccess] = useState<string | null>(null);
  const [pendingUpgradePlanId, setPendingUpgradePlanId] = useState<UpgradePlanId | null>(null);
  const [showAllUpgradePlans, setShowAllUpgradePlans] = useState(false);
  const [checkoutPollingDeadline, setCheckoutPollingDeadline] = useState<number | null>(null);
  const desktopActivationSentRef = useRef(false);
  // Activation retry bookkeeping. A failed attempt used to clear
  // `desktopActivationSentRef` immediately, so the next flip of any dep below
  // (runtime reachability flaps every time the proxy restarts) fired another
  // attempt with no delay -- offline machines sent 187 activation failures to
  // Sentry in 12 hours, two of them 3 seconds apart (RUST-7H/RUST-8X). Retry
  // on a widening backoff instead, and stop after a handful: activation is
  // best-effort, and the next app launch tries again from scratch.
  const desktopActivationAttemptsRef = useRef(0);
  const desktopActivationRetryTimerRef = useRef<number | undefined>(undefined);
  const [desktopActivationRetry, setDesktopActivationRetry] = useState(0);
  // Persisted: pricing gates last days, and an app restart mid-gate used to
  // lose this set — the auto-disabled connectors then never re-enabled when
  // the gate reopened, leaving users silently unoptimized until a manual
  // toggle.
  const [initialAutoDisabledByGate] = useState<Set<string>>(() => {
    try {
      const raw = window.localStorage.getItem(GATE_AUTO_DISABLED_STORAGE_KEY);
      return new Set(raw ? (JSON.parse(raw) as string[]) : []);
    } catch {
      return new Set<string>();
    }
  });
  const autoDisabledByGateRef = useRef<Set<string>>(initialAutoDisabledByGate);
  const persistAutoDisabledByGate = () => {
    try {
      window.localStorage.setItem(
        GATE_AUTO_DISABLED_STORAGE_KEY,
        JSON.stringify([...autoDisabledByGateRef.current])
      );
    } catch {
      // best effort
    }
  };
  const [learnInstallCopyNotice, setLearnInstallCopyNotice] = useState<string | null>(null);

  const [stepSignature, setStepSignature] = useState("");
  const [stepStartedAtMs, setStepStartedAtMs] = useState<number | null>(null);
  const [stepEtaSeedSeconds, setStepEtaSeedSeconds] = useState(0);
  // When the current ETA seed arrived. The backend re-estimates the dependency
  // install continuously, so its ETA is time-remaining-from-that-update, not a
  // budget measured from the start of the step -- counting it from
  // stepStartedAtMs double-charged the elapsed time and pinned a long install
  // at "finishing up" (RUST-9Y).
  const [stepEtaSeedAtMs, setStepEtaSeedAtMs] = useState<number | null>(null);
  // Highest fraction this step has shown. The ETA anchor above moves on every
  // estimate, so the time-based fraction restarts near zero each time -- right
  // for the ETA, wrong for a progress reading, which must never go backwards.
  // Without this the dependency step reported "0% of this step" for its whole
  // run, since a pip line (and so a re-anchor) lands more often than once a
  // second.
  const stepProgressFloorRef = useRef(0);
  const [stepBasePercent, setStepBasePercent] = useState(0);
  const [chartResetSignal, setChartResetSignal] = useState(0);
  const [chartMode, setChartMode] = useState<SavingsChartMode>("usd");
  // Safety net: if native history never loads (backend unreachable), reveal the
  // chart anyway after this delay rather than spinning forever.
  const [historyLoadTimedOut, setHistoryLoadTimedOut] = useState(false);
  const [showSavingsInfo, setShowSavingsInfo] = useState(false);
  const [showCacheInfo, setShowCacheInfo] = useState(false);
  const [autostartEnabled, setAutostartEnabled] = useState<boolean | null>(null);
  const [autostartBusy, setAutostartBusy] = useState(false);
  const [rtkBusy, setRtkBusy] = useState(false);
  const [autoLearnEnabled, setAutoLearnEnabled] = useState<boolean | null>(null);
  const [autoLearnBusy, setAutoLearnBusy] = useState(false);
  const [showUninstallDialog, setShowUninstallDialog] = useState(false);
  const [uninstallBusy, setUninstallBusy] = useState(false);
  const [uninstallError, setUninstallError] = useState<string | null>(null);
  // "cancel" is not a plan: it is the cancel-subscription action, which shares the
  // busy state so only the button that was clicked reads "Opening...".
  const [upgradeActionBusy, setUpgradeActionBusy] = useState<UpgradePlanId | "cancel" | null>(null);
  const [upgradeActionError, setUpgradeActionError] = useState<string | null>(null);
  const [pendingPlanChange, setPendingPlanChange] = useState<{
    fromTier: HeadroomSubscriptionTier;
    toTier: HeadroomSubscriptionTier;
    billingPeriod: BillingPeriod;
  } | null>(null);
  const [planChangeBusy, setPlanChangeBusy] = useState(false);
  const [planChangeError, setPlanChangeError] = useState<string | null>(null);
  const [reactivateBusy, setReactivateBusy] = useState(false);
  const [reactivateError, setReactivateError] = useState<string | null>(null);
  const [saveOffer, setSaveOffer] = useState<SaveOffer | null>(null);
  const [saveOfferBusy, setSaveOfferBusy] = useState(false);
  const [saveOfferError, setSaveOfferError] = useState<string | null>(null);
  const [saveOfferRedeemed, setSaveOfferRedeemed] = useState(false);
  const [cancelReasonOpen, setCancelReasonOpen] = useState(false);
  const [cancelReason, setCancelReason] = useState("");
  const [cancelNote, setCancelNote] = useState("");
  const [cancelBusy, setCancelBusy] = useState(false);
  const [contactEmail, setContactEmail] = useState("");
  const [contactMessage, setContactMessage] = useState("");
  const [contactSubmitBusy, setContactSubmitBusy] = useState(false);
  const [contactSubmitError, setContactSubmitError] = useState<string | null>(null);
  const [contactSubmitSuccess, setContactSubmitSuccess] = useState<string | null>(null);
  const appSemver = appUpdateConfig?.currentVersion ?? packageJson.version;
  const bootstrapFailureSignatureRef = useRef("");
  const mainWindowLastBlurAtRef = useRef<number | null>(null);
  const mainWindowLastSeenDayRef = useRef(formatDayKey(new Date()));
  // Session uptime anchor for the setup-stall check. The tray webview is
  // created at app launch and stays alive while the window is hidden, so first
  // render is a good stand-in for "when Headroom started running".
  const appStartedAtMsRef = useRef(Date.now());
  // Mirrors the account gate for the setup-stall closure: a signed-out or
  // unpaid user has zero savings by design, and both states already fire their
  // own daily notification. Undefined until pricing status first loads.
  const optimizationBlockedRef = useRef<boolean | undefined>(undefined);
  // Mirrors connector status for the same closure. Undefined until the startup
  // fetch lands, which keeps the no-traffic branch quiet rather than guessing.
  // Staleness in the "became verified" direction is harmless: that only happens
  // once traffic flows, and the no-traffic branch requires zero requests.
  const connectorsRef = useRef<ClientConnectorStatus[] | undefined>(undefined);
  // A forced setup-stall alert skips the day throttle, so this keeps it to one
  // showing per app run instead of resurrecting itself on every poll.
  const forcedSetupStallFiredRef = useRef(false);
  const appUpdateKnownVersionRef = useRef<string | null>(null);
  const appUpdateReadyToRestartRef = useRef(false);
  const appUpdateBusyRef = useRef(false);
  const appUpdateInstallBusyRef = useRef(false);
  const launcherHideAnimationMs = 320;
  const trayFocusPrewarmDelayMs = 250;
  const dashboardSignatureRef = useRef(serializeState(mockDashboard));
  const connectorsSignatureRef = useRef(serializeState([] as ClientConnectorStatus[]));
  const runtimeStatusSignatureRef = useRef(serializeState(null as RuntimeStatus | null));
  // Mirrors runtimeStatus.installed for closures (background update check) that
  // must not fire notifications while first-install bootstrap is still running.
  const runtimeInstalledRef = useRef(false);
  const bootstrapFailedRef = useRef(false);
  const claudeProjectsSignatureRef = useRef(serializeState([] as ClaudeCodeProject[]));
  // Mirror the server's price table into the pricing helpers before anything
  // reads a price this render. Idempotent and derived purely from state, so a
  // repeated render (StrictMode) is a no-op.
  setServerPlanPrices(pricingStatus?.planPrices);
  const upgradePlansState = getUpgradePlans(
    pricingAudience,
    pricingStatus?.claude.planTier ?? cachedPricing.planTier,
    pricingStatus?.recommendedSubscriptionTier ?? cachedPricing.recommendedSubscriptionTier,
    pricingStatus?.account?.subscriptionTier ?? cachedPricing.subscriptionTier,
    pricingStatus?.account?.subscriptionActive ?? false,
    pricingStatus?.launchDiscountActive ?? false,
    billingPeriod,
    pricingStatus?.account?.subscriptionAmountCents,
    pricingStatus?.account?.subscriptionBillingPeriod,
    pricingStatus?.account?.subscriptionRenewsAt,
    pricingStatus?.account?.subscriptionStartedAt,
    pricingStatus?.account?.subscriptionDiscountDuration,
    pricingStatus?.account?.subscriptionDiscountDurationInMonths,
    pricingStatus?.account?.subscriptionCancelAtPeriodEnd ?? false,
    pricingStatus?.account?.subscriptionEndsAt,
    pricingStatus?.activePercentOff ?? 0,
    pricingStatus?.introOffer ?? null,
    pricingStatus?.account?.subscriptionRenewalCents,
    pricingStatus?.account?.subscriptionRenewalEndsAt,
    pricingStatus?.account?.upgradeAction
  );
  const contactEmailValid = isValidEmailAddress(contactEmail);
  const authEmailValid = isValidEmailAddress(authEmail);
  const showInstallProgress =
    bootstrapping ||
    bootstrapProgress.running ||
    bootstrapProgress.complete ||
    bootstrapProgress.failed ||
    bootstrapProgress.overallPercent > 0;

  const isLastScreen =
    windowLabel === "launcher" && launcherStage === "post_install";
  // While the post-install screen is still waiting on the first savings,
  // a blur-hide would dismiss onboarding with no way back to it — the
  // launcher never reopens. Autohide only arms once savings have landed.
  const awaitingFirstSavings =
    dashboard.launchExperience === "first_run" &&
    dashboard.lifetimeEstimatedTokensSaved <= 0 &&
    dashboard.lifetimeEstimatedSavingsUsd <= 0;
  // Independent of launchExperience: any savings on record at all, which is
  // what retires the setup-stall watchdog below.
  const forcedSetupStall = debugOverrides?.setupStall ?? null;
  useEffect(() => {
    if (!showHeadroomDetails || !headroomLogRef.current) {
      return;
    }
    headroomLogRef.current.scrollTop = headroomLogRef.current.scrollHeight;
  }, [showHeadroomDetails, headroomLogLines]);

  useEffect(() => {
    const timer = window.setTimeout(() => setHistoryLoadTimedOut(true), 20000);
    return () => window.clearTimeout(timer);
  }, []);

  useEffect(() => {
    if (!showRtkDetails || !rtkActivityRef.current) {
      return;
    }
    rtkActivityRef.current.scrollTop = rtkActivityRef.current.scrollHeight;
  }, [showRtkDetails, rtkActivityLines]);

  useEffect(() => {
    dashboardSignatureRef.current = serializeState(dashboard);
  }, [dashboard]);

  useEffect(() => {
    connectorsSignatureRef.current = serializeState(connectors);
  }, [connectors]);

  useEffect(() => {
    runtimeStatusSignatureRef.current = serializeState(runtimeStatus);
    runtimeInstalledRef.current = runtimeStatus?.installed === true;
  }, [runtimeStatus]);

  useEffect(() => {
    bootstrapFailedRef.current = bootstrapProgress.failed === true;
  }, [bootstrapProgress.failed]);

  useEffect(() => {
    claudeProjectsSignatureRef.current = serializeState(claudeProjects);
  }, [claudeProjects]);

  function applyDashboardIfChanged(next: DashboardState) {
    const nextSignature = serializeState(next);
    if (dashboardSignatureRef.current === nextSignature) {
      return;
    }
    dashboardSignatureRef.current = nextSignature;
    setDashboard(next);
  }

  function applyConnectorsIfChanged(nextConnectors: ClientConnectorStatus[]) {
    const next = withoutHiddenConnectors(nextConnectors);
    const nextSignature = serializeState(next);
    if (connectorsSignatureRef.current === nextSignature) {
      return;
    }
    connectorsSignatureRef.current = nextSignature;
    setConnectors(next);
  }

  function applyRuntimeStatusIfChanged(next: RuntimeStatus | null) {
    const nextSignature = serializeState(next);
    if (runtimeStatusSignatureRef.current === nextSignature) {
      return;
    }
    runtimeStatusSignatureRef.current = nextSignature;
    setRuntimeStatus(next);
  }

  function applyClaudeProjectsIfChanged(next: ClaudeCodeProject[]) {
    const nextSignature = serializeState(next);
    if (claudeProjectsSignatureRef.current === nextSignature) {
      return;
    }
    claudeProjectsSignatureRef.current = nextSignature;
    setClaudeProjects(next);
  }

  useEffect(() => {
    const unlistenPromise = listen<{ action: string | null }>(
      "notification-clicked",
      (event) => {
        const action = event.payload?.action ?? null;
        if (action === "update") {
          setShowAppUpdateDialog(true);
          return;
        }
        const view = notificationActionView(action);
        if (view) {
          setActiveView(view);
        }
      }
    );
    return () => {
      void unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

  useEffect(() => {
    setShowAllUpgradePlans(false);
    if (pricingAudience !== "individual") setBillingPeriod("annual");
  }, [pricingAudience]);

  // Open on the plan the subscriber actually has, not the monthly default:
  // showing an annual subscriber monthly prices misstates what they pay. Keyed
  // on the account's period alone, so it fires on load and on a plan change but
  // never fights the toggle.
  const accountBillingPeriod = pricingStatus?.account?.subscriptionBillingPeriod;
  useEffect(() => {
    if (accountBillingPeriod === "annual" || accountBillingPeriod === "monthly") {
      setBillingPeriod(accountBillingPeriod);
    }
  }, [accountBillingPeriod]);

  useEffect(() => {
    if (!pricingStatus?.authenticated) {
      desktopActivationSentRef.current = false;
      // Signing out and back in is an explicit user action: give it a full
      // fresh budget rather than whatever the previous session burned.
      desktopActivationAttemptsRef.current = 0;
      window.clearTimeout(desktopActivationRetryTimerRef.current);
      desktopActivationRetryTimerRef.current = undefined;
    }
  }, [pricingStatus?.authenticated]);

  // Drop any pending activation retry when the app unmounts.
  useEffect(
    () => () => window.clearTimeout(desktopActivationRetryTimerRef.current),
    []
  );

  useEffect(() => {
    if (!pricingStatus) return;
    writeCachedPricing(cachePricingStatus(pricingStatus));
  }, [pricingStatus]);

  useEffect(() => {
    const STORAGE_KEY = "headroom:lastNotifiedMismatchTier";
    const mismatch = pricingStatus?.tierMismatch;
    // Don't clear the dedupe key when the mismatch reads null: the backend also
    // reports null on a poll where the Claude plan momentarily detects as
    // Unknown, and clearing there re-armed this notification to re-fire on the
    // next confident poll — that's what nagged users overnight. Leave the key;
    // it's overwritten only when we climb to a higher tier below.
    if (!mismatch) return;
    const rank: Record<string, number> = { pro: 1, max5x: 2, max20x: 3 };
    const previous = window.localStorage.getItem(STORAGE_KEY);
    // Notify on first detection and whenever the recommended tier climbs higher.
    if (previous !== null && (rank[mismatch.recommendedTier] ?? 0) <= (rank[previous] ?? 0)) {
      return;
    }
    const paidLabel = upgradePlanIntentLabel(mismatch.paidTier);
    const recommendedLabel = upgradePlanIntentLabel(mismatch.recommendedTier);
    const sourceLabel = tierRecommendationSourceLabel(mismatch.recommendedSource);
    // fireUpsellNudge self-throttles (quiet hours, twice/day, min gap); only
    // advance the dedupe key once it actually showed, or a quiet-hours skip
    // would silently mark this tier as notified and suppress it for good.
    void fireUpsellNudge(
      "Upgrade your Headroom plan",
      `Your ${sourceLabel} usage needs the Headroom ${recommendedLabel} plan, above your current ${paidLabel} plan. Upgrade to keep unlimited optimization.`
    ).then((fired) => {
      if (fired) window.localStorage.setItem(STORAGE_KEY, mismatch.recommendedTier);
    });
  }, [pricingStatus?.tierMismatch?.recommendedTier, pricingStatus?.tierMismatch]);

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
    // Analytics only. The matching funnel beacon is sent from Rust
    // (get_dashboard_state), which retries every launch until it lands —
    // this localStorage gate is once-per-install with no retry, and losing
    // one POST here permanently undercounted the funnel finish line.
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
      applyDashboardIfChanged(dashboardResult);

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
      const initialStage = getInitialLauncherStage(
        label,
        bootstrapResult.complete,
        dashboardResult.bootstrapComplete,
        dashboardResult.launchExperience
      );
      if (initialStage) {
        setLauncherStage(initialStage);
      }

      updateStartup("runtime", 80, "Preparing Headroom runtime…");
      const [runtimeResult, pricingResult, launchFlagsResult] = await Promise.all([
        invoke<RuntimeStatus>("get_runtime_status").catch(() => null),
        invoke<HeadroomPricingStatus>("get_headroom_pricing_status").catch(() => null),
        // Local cached read, never blocks on the network. Fetched before
        // startupReady so TermsGate sees the paywall-first flag on its very
        // first render (a late fetch would flicker the auth section in).
        invoke<LaunchFlags>("get_launch_flags").catch(() => null),
        refreshConnectors(),
      ]);
      if (!active) {
        return;
      }
      if (runtimeResult) {
        applyRuntimeStatusIfChanged(runtimeResult);
      }
      if (pricingResult) {
        setPricingStatus(pricingResult);
      }
      if (launchFlagsResult) {
        setLaunchFlags(launchFlagsResult);
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
    let unlisten: (() => void) | undefined;
    const detach = () => {
      const fn = unlisten;
      unlisten = undefined;
      fn?.();
    };

    const handleProgress = async (progress: BootstrapProgress) => {
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
        detach();
        return;
      }

      if (progress.complete && !completionHandled) {
        completionHandled = true;
        detach();
        setBootstrapping(false);
        const latestDashboard = await loadDashboard();
        if (!active) {
          return;
        }
        applyDashboardIfChanged(latestDashboard);
        // Always land on the install step after a bootstrap completes during
        // this session, regardless of launchExperience. The install step's
        // Continue button is gated on runtime.running, so it handles both the
        // readiness wait and the "Headroom installation present" confirmation
        // for Resume users whose launch_count > 1 (e.g., they reinstalled the
        // app without clearing ~/Library/Application Support/Headroom).
        if (windowLabel === "launcher") {
          setLauncherStage("install");
        }
      }
    };

    void listen<BootstrapProgress>("bootstrap_progress", (event) => {
      void handleProgress(event.payload);
    }).then((fn) => {
      if (!active || completionHandled) {
        fn();
        return;
      }
      unlisten = fn;
    });

    // Prime with the current state in case we subscribed mid-flight or the
    // bootstrap already completed before the listener attached.
    void invoke<BootstrapProgress>("get_bootstrap_progress")
      .then((progress) => handleProgress(progress))
      .catch(() => {});

    return () => {
      active = false;
      detach();
    };
  }, [bootstrapping]);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void listen<RuntimeUpgradeProgress>("runtime_upgrade_progress", (event) => {
      if (!active) return;
      setRuntimeUpgradeProgress(event.payload);
    }).then((fn) => {
      if (!active) {
        fn();
        return;
      }
      unlisten = fn;
    });

    void invoke<RuntimeUpgradeProgress>("get_runtime_upgrade_progress")
      .then((progress) => {
        if (active) setRuntimeUpgradeProgress(progress);
      })
      .catch(() => {});

    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  // Hand off cleanly once the runtime upgrade finishes: show the success
  // state briefly, then drop the progress object back to idle so the
  // launcher stops rendering the upgrade UI and falls through to whichever
  // window content the user should see next. We also nudge the launcher
  // stage to post_install since bootstrapComplete only gets checked at
  // startup otherwise.
  useEffect(() => {
    if (!runtimeUpgradeProgress.complete || runtimeUpgradeProgress.failed) {
      return;
    }
    const timeout = window.setTimeout(() => {
      setRuntimeUpgradeProgress(idleRuntimeUpgradeProgress);
      if (windowLabel === "launcher") {
        setLauncherStage("post_install");
      }
      // Refresh runtime status so the rest of the app picks up the
      // freshly-installed version immediately.
      void invoke<RuntimeStatus>("get_runtime_status")
        .then((status) => applyRuntimeStatusIfChanged(status))
        .catch(() => {});
    }, 2500);
    return () => window.clearTimeout(timeout);
  }, [runtimeUpgradeProgress.complete, runtimeUpgradeProgress.failed, windowLabel]);

  useEffect(() => {
    if (windowLabel !== "launcher" || launcherStage !== "client_setup") {
      return;
    }
    void refreshConnectors();
  }, [windowLabel, launcherStage]);

  // Paywall stage: poll pricing status so the tier recommendation appears as
  // soon as the first proxied request lands, and so a completed checkout
  // (deep link or the 5-minute checkout poll) flips subscriptionActive here.
  useEffect(() => {
    if (windowLabel !== "launcher" || launcherStage !== "paywall") {
      return;
    }
    trackAnalyticsEvent("paywall_shown", {});
    let active = true;
    const poll = () => {
      invoke<HeadroomPricingStatus>("get_headroom_pricing_status")
        .then((status) => {
          if (active) setPricingStatus(status);
        })
        .catch(() => {});
    };
    poll();
    const interval = window.setInterval(poll, 3000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [windowLabel, launcherStage]);

  // Post-checkout resume: the subscription is live, so move to the install
  // stage and start the (now gate-passing) bootstrap automatically.
  useEffect(() => {
    if (windowLabel !== "launcher" || launcherStage !== "paywall") {
      return;
    }
    if (pricingStatus?.account?.subscriptionActive) {
      trackAnalyticsEvent("paywall_checkout_completed", {
        plan_id: pricingStatus.account.subscriptionTier ?? undefined
      });
      setLauncherStage("install");
      void handleBootstrap();
    }
  }, [windowLabel, launcherStage, pricingStatus?.account?.subscriptionActive]);

  useEffect(() => {
    if (windowLabel !== "launcher" || launcherStage !== "proxy_verify") {
      return;
    }

    let active = true;
    const poll = () => {
      void (async () => {
        try {
          const countsCommand = interceptOnlyVerify
            ? "get_intercept_request_counts_by_agent"
            : "get_headroom_request_counts_by_agent";
          const [runtime, counts] = await Promise.all([
            interceptOnlyVerify
              ? Promise.resolve<RuntimeStatus | null>(null)
              : invoke<RuntimeStatus>("get_runtime_status").catch(() => null),
            invoke<Record<string, number> | null>(countsCommand).catch(() => null)
          ]);

          if (!active) {
            return;
          }

          if ((!interceptOnlyVerify && runtime?.proxyReachable !== true) || counts === null) {
            // On this screen the runtime is always app-managed and coming up
            // (the pre-install case routes through interceptOnlyVerify). First
            // launch synchronously downloads the compression/embedder models
            // before the backend binds, so `proxyReachable` is false for a
            // minute or more on a perfectly healthy install. Keep it calm and
            // informational — only a hard startup fault is a real error.
            const startupError = interceptOnlyVerify ? null : runtime?.startupError;
            setProxyVerificationHint(
              interceptOnlyVerify
                ? { text: "Waiting for setup traffic. Send a test message from your coding tool.", tone: "info" }
                : startupError
                ? { text: `Headroom could not finish starting: ${startupError}`, tone: "error" }
                : {
                    text: "Finishing setup - first launch downloads models and can take a minute.",
                    tone: "info"
                  }
            );
            return;
          }

          setProxyVerificationHint(null);

          // Capture the baseline on the first reachable poll. Anchoring on a
          // null/unreachable reading would let a later "proxy came up" jump
          // (0 → N) look like new traffic.
          if (proxyVerificationRequestAnchorRef.current === null) {
            proxyVerificationRequestAnchorRef.current = counts;
            return;
          }

          // Attribute traffic per client: a prompt sent to Claude Code must not
          // flip the Codex row (and vice versa). The proxy keys agents as
          // `claude-code` / `codex`; our rows use `claude_code` / `codex`.
          const anchor = proxyVerificationRequestAnchorRef.current;
          setProxyVerificationRows((current) =>
            current.map((row) => {
              if (row.state === "verified") {
                return row;
              }
              const agentKey = row.clientId.replace(/_/g, "-");
              const now = counts[agentKey] ?? 0;
              const base = anchor[agentKey] ?? 0;
              return now > base
                ? { ...row, state: "verified", message: "Request received" }
                : row;
            })
          );
        } catch {
          if (active) {
            setProxyVerificationHint({ text: "Waiting for Headroom proxy activity...", tone: "info" });
          }
        }
      })();
    };
    poll();
    const interval = window.setInterval(poll, 1000);

    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [windowLabel, launcherStage, interceptOnlyVerify]);

  // Warm the bootstrap download cache while the user is still signing up.
  // Download-only: nothing is installed until they consent on the install
  // step, and the Rust side no-ops when the runtime already exists, so this
  // is safe to fire on every launcher mount.
  useEffect(() => {
    if (windowLabel !== "launcher") return;
    void invoke("prefetch_bootstrap_artifacts").catch(() => {});
  }, [windowLabel]);

  // One beacon per launcher stage the user reaches. Single source for the
  // "stage shown" funnel steps; sub-steps (client_setup_applied, proxy_verified,
  // email_code_*) fire from their own handlers.
  useEffect(() => {
    if (windowLabel !== "launcher") return;
    const step = LAUNCHER_STAGE_STEP[launcherStage];
    if (step) reportFunnelStep(step);
  }, [windowLabel, launcherStage]);

  // signup_gate_shown: a new (unauthenticated) user is looking at the
  // terms + email sign-up gate -- the true top of the new-user funnel and the
  // "saw it, never typed an email" bucket. Gated to the paywall-first launcher
  // so existing users re-accepting terms in the main window don't count.
  const signupGateVisible =
    windowLabel === "launcher" &&
    paywallFirstFlow &&
    pricingStatus?.authenticated !== true &&
    needsTermsAcceptance(dashboard.requiredTermsVersion, dashboard.acceptedTermsVersion);
  useEffect(() => {
    if (signupGateVisible) reportFunnelStep("signup_gate_shown");
  }, [signupGateVisible]);

  // proxy_verified: every enabled client's test traffic reached the proxy.
  useEffect(() => {
    if (windowLabel !== "launcher" || launcherStage !== "proxy_verify") return;
    if (
      proxyVerificationRows.length > 0 &&
      proxyVerificationRows.every((row) => row.state === "verified")
    ) {
      // Persist for the main window: its own phase poller honors this marker,
      // so the tray doesn't ask the user to verify a second time right after
      // onboarding just did.
      setConnectorTrafficVerified(true);
      reportFunnelStep("proxy_verified");
    }
  }, [windowLabel, launcherStage, proxyVerificationRows]);

  // A connector checking in invalidates the armed skip warning: it was written
  // against the old unverified set and reads as "we still see nothing" even
  // after the user did exactly what it asked. Re-arm on the next Skip click.
  const proxyVerifiedCount = proxyVerificationRows.filter(
    (row) => row.state === "verified"
  ).length;
  useEffect(() => {
    if (proxyVerifiedCount > 0) setProxyVerifySkipArmed(false);
  }, [proxyVerifiedCount]);

  useEffect(() => {
    if (!showInstallProgress) {
      return;
    }

    const signature = `${bootstrapProgress.currentStep}|${bootstrapProgress.running}|${bootstrapProgress.complete}|${bootstrapProgress.failed}`;
    if (signature !== stepSignature) {
      setStepSignature(signature);
      setStepStartedAtMs(Date.now());
      setStepBasePercent(bootstrapProgress.overallPercent);
      stepProgressFloorRef.current = 0;
    }
    // Re-anchor on every new estimate, step change or not. "Updating
    // dependencies" is one step for the whole install, so without this the
    // seed froze at the first frame's 90s and every later, truthful estimate
    // was discarded -- which is what a slow link saw as a hang.
    setStepEtaSeedSeconds(bootstrapProgress.currentStepEtaSeconds);
    setStepEtaSeedAtMs(Date.now());
  }, [bootstrapProgress, showInstallProgress, stepSignature]);

  // Reaching the final screen IS the end of onboarding, so satisfy the tray's
  // gate here instead of on one specific exit. Every other way off this screen
  // -- the blur-autohide right below, closing the window, quitting -- used to
  // leave `setup_wizard_complete` false while the launcher kept landing here
  // (getInitialLauncherStage sends any returning user straight to
  // post_install). The tray gate then disagreed with the stage machine and
  // reopened this same screen on every click, forever.
  useEffect(() => {
    if (!isLastScreen) return;
    void invoke("complete_setup_wizard");
  }, [isLastScreen]);

  useEffect(() => {
    if (!isLastScreen || awaitingFirstSavings) return;
    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (!focused) triggerHide();
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, [isLastScreen, awaitingFirstSavings]);

  const optimizationBlocked = pricingStatus
    ? pricingStatus.needsAuthentication || !pricingStatus.optimizationAllowed
    : undefined;
  useEffect(() => {
    optimizationBlockedRef.current = optimizationBlocked;
  }, [optimizationBlocked]);

  // Test overrides (HEADROOM_FAKE_* env vars, RC builds only). Null on every
  // shipped stable build and on any RC launched without the vars, so this
  // resolves to "no overrides" in production.
  useEffect(() => {
    if (windowLabel !== "main") {
      return;
    }
    let active = true;
    void invoke<DebugOverrides>("get_debug_overrides")
      .then((overrides) => {
        if (active) setDebugOverrides(overrides);
      })
      .catch(() => {
        // Older backend or command unavailable: behave as if unset.
      });
    return () => {
      active = false;
    };
  }, [windowLabel]);

  // Setup-stall watchdog: Headroom running for a long stretch with nothing
  // saved almost always means the hookup is incomplete, not that the user was
  // idle the whole time. Runs regardless of tray visibility (the other
  // dashboard pollers are focus-gated, so without this a broken install stays
  // silent while the window is closed). It no longer retires once savings
  // land: the same tick also watches for savings DRIFT (saved before, days of
  // traffic with nothing optimized since - the server's "on but idle" cohort)
  // and drives the silent connector self-heal. `maybeFireSetupStallAlert`
  // owns the once-per-local-day throttle for both alert families.
  useEffect(() => {
    if (windowLabel !== "main") {
      return;
    }

    let active = true;
    const check = async () => {
      const uptimeMs = Date.now() - appStartedAtMsRef.current;
      if (!forcedSetupStall && uptimeMs < SETUP_STALL_EARLIEST_MS) {
        return;
      }
      // Once per app run when forced: the alert bypasses the day throttle, so
      // without this it would reappear on every tick after being dismissed.
      if (forcedSetupStall && forcedSetupStallFiredRef.current) {
        return;
      }
      // Silent self-heal first: re-apply any configured connector whose files
      // no longer verify (another tool rewrote settings.json, a shell block
      // vanished). Rust throttles the scan to once per hour and skips it
      // while paused/bypassed; failures are logged there, not surfaced here.
      void invoke("repair_client_setups").catch(() => undefined);
      const latest = await loadDashboard().catch(() => null);
      if (!active || !latest) {
        return;
      }
      applyDashboardIfChanged(latest);
      // Banner first: it is a passive line with its own (wider) gates, and it
      // must not depend on the modal's once-per-day throttle below.
      setStallBannerLine(
        setupStallBannerLine(latest, uptimeMs, {
          optimizationBlocked: optimizationBlockedRef.current,
          connectors: connectorsRef.current,
          forceKind: forcedSetupStall,
        })
      );
      const alert = await maybeFireSetupStallAlert(latest, uptimeMs, {
        optimizationBlocked: optimizationBlockedRef.current,
        connectors: connectorsRef.current,
        forceKind: forcedSetupStall,
      });
      if (active && alert) {
        if (forcedSetupStall) {
          forcedSetupStallFiredRef.current = true;
        }
        setSetupStall(alert);
      }
    };

    void check();
    const interval = window.setInterval(() => {
      void check();
    }, SETUP_STALL_CHECK_INTERVAL_MS);

    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [windowLabel, forcedSetupStall]);

  useEffect(() => {
    if (windowLabel !== "main" || !trayWindowFocused) {
      return;
    }

    void refreshRuntimeStatus();
    const interval = window.setInterval(() => {
      void refreshRuntimeStatus();
    }, 3000);

    return () => window.clearInterval(interval);
  }, [windowLabel, trayWindowFocused]);

  // Poll runtime status while the install step is visible so the Continue
  // button unlocks as soon as headroom is fully running (same signal the
  // tray uses for its solid icon: installed && !paused && proxy_reachable).
  // On a cold first install the Gatekeeper scan can finish after
  // mark_bootstrap_complete fires, and the main-window poller doesn't run
  // on the launcher.
  useEffect(() => {
    if (windowLabel !== "launcher" || launcherStage !== "install") {
      return;
    }
    if (runtimeStatus?.running === true || runtimeStatus?.bypassed === true) {
      return;
    }

    void refreshRuntimeStatus();
    const interval = window.setInterval(() => {
      void refreshRuntimeStatus();
    }, 1000);

    return () => window.clearInterval(interval);
  }, [windowLabel, launcherStage, runtimeStatus?.running, runtimeStatus?.bypassed]);

  useEffect(() => {
    if (windowLabel !== "main") {
      return;
    }

    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        setTrayWindowFocused(focused);
        const now = new Date();
        const nowDayKey = formatDayKey(now);

        if (!focused) {
          mainWindowLastBlurAtRef.current = now.getTime();
          mainWindowLastSeenDayRef.current = nowDayKey;
          return;
        }

        const inactiveForMs = mainWindowLastBlurAtRef.current
          ? now.getTime() - mainWindowLastBlurAtRef.current
          : null;
        // Skip `refreshConnectors` for quick alt-tabs: connectors only change
        // via user action (app enable/disable) or manual edits to
        // ~/.claude/settings.json — neither happens in the 30s window of a
        // fast context switch. On initial focus (`inactiveForMs === null`)
        // or after a real "came back from another app" gap, refresh to pick
        // up outside changes.
        if (inactiveForMs === null || inactiveForMs >= 30_000) {
          void refreshConnectors();
        }

        const dayRolledOver = nowDayKey !== mainWindowLastSeenDayRef.current;
        if ((inactiveForMs ?? 0) >= 3_600_000 || dayRolledOver) {
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
        appUpdateInstallBusyRef.current ||
        // Don't fire an "update available" notification while first-install
        // bootstrap is still building the runtime — it piles onto the install
        // window. Resume once the runtime is installed.
        //
        // A *failed* bootstrap is the exception, and suppressing it there was
        // a trap: when the cause is a bad pin in the lock we shipped, every
        // retry fails identically and a newer build is the only fix, so the
        // one screen that needed the updater most was the one screen that
        // never checked (Sentry RUST-1G — users on 0.8.1 retried for hours
        // while 0.8.2 sat in the manifest with the fix).
        (!runtimeInstalledRef.current && !bootstrapFailedRef.current)
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
    void invoke<boolean>("get_autostart_enabled")
      .then((enabled) => setAutostartEnabled(enabled))
      .catch(() => setAutostartEnabled(false));
  }, [activeView]);

  useEffect(() => {
    if (activeView !== "optimization") {
      return;
    }
    void invoke<boolean>("get_auto_learn_enabled")
      .then((enabled) => setAutoLearnEnabled(enabled))
      .catch(() => setAutoLearnEnabled(true));
  }, [activeView]);

  async function handleAutoLearnToggle(nextEnabled: boolean) {
    setAutoLearnBusy(true);
    try {
      // The proxy restarts inside this command, so it can take a moment.
      const enabled = await invoke<boolean>("set_auto_learn_enabled", { enabled: nextEnabled });
      setAutoLearnEnabled(enabled);
    } catch (error) {
      console.error("Failed to update auto-learning", error);
    } finally {
      setAutoLearnBusy(false);
    }
  }

  // Auto-learning gates every pattern behind minEvidence sightings, so a new
  // user sees zero learnings for a while and reads the silence as "broken"
  // (beta feedback). Show observation progress when the backend reports it,
  // and explain the gate either way.
  const learnerProgress = dashboard.learnerProgress;
  const autoLearnMeta =
    learnerProgress && learnerProgress.pendingPatterns > 0
      ? `Learning from live traffic. ${learnerProgress.pendingPatterns} pattern${
          learnerProgress.pendingPatterns === 1 ? "" : "s"
        } detected; these are saved once detected ${learnerProgress.minEvidence} times.`
      : `Learns from live traffic. First learnings may take a few sessions to appear.`;

  async function handleAutostartToggle(nextEnabled: boolean) {
    setAutostartBusy(true);
    try {
      const enabled = await invoke<boolean>("set_autostart_enabled", { enabled: nextEnabled });
      setAutostartEnabled(enabled);
    } catch (error) {
      console.error("Failed to update autostart", error);
    } finally {
      setAutostartBusy(false);
    }
  }

  async function handleRtkToggle(nextEnabled: boolean) {
    const copy = addonCopy.rtk;
    setRtkBusy(true);
    setAddonBusyId("rtk");
    setAddonBusyLabel((nextEnabled ? copy?.enabling : copy?.disabling) ?? null);
    setAddonResult(null);
    try {
      await invoke<boolean>("set_rtk_enabled", { enabled: nextEnabled });
      await refreshRuntimeStatus();
      const message = nextEnabled ? undefined : copy?.disabled;
      if (message) {
        setAddonResult({ id: "rtk", message });
      }
    } catch (error) {
      console.error("Failed to update RTK", error);
      setAddonError("RTK could not be updated.");
    } finally {
      setRtkBusy(false);
      setAddonBusyId(null);
      setAddonBusyLabel(null);
    }
  }

  async function handleUninstall() {
    setUninstallBusy(true);
    setUninstallError(null);
    try {
      await invoke<string[]>("uninstall_and_quit");
    } catch (error) {
      setUninstallError(
        typeof error === "string" ? error : "Uninstall failed. Please try again."
      );
      setUninstallBusy(false);
    }
  }

  useEffect(() => {
    if (activeView !== "home" || !trayWindowFocused) {
      return;
    }

    let active = true;
    const refreshDashboard = () => {
      void loadDashboard()
        .then((next) => {
          if (!active) return;
          applyDashboardIfChanged(next);
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
  }, [activeView, trayWindowFocused]);

  // Track whether the user has ever visited a heavy-data tab this session.
  // Once true, stays true until app restart — the pre-warm below is gated
  // on it so Home-only users don't pay its cost on every tray focus.
  useEffect(() => {
    if (activeView === "notifications" || activeView === "optimization") {
      setHeavyTabEverOpened(true);
    }
    // Once per app run, mirroring app_started's cadence so adoption ratios
    // (activity users / app_started users) compare like with like.
    if (activeView === "notifications" && !activityTabTrackedRef.current) {
      activityTabTrackedRef.current = true;
      trackAnalyticsEvent("activity_tab_opened");
    }
  }, [activeView]);

  // Pre-warm Optimize + Activity data the moment the tray gains focus, so
  // switching tabs reveals already-populated content instead of triggering
  // a fresh ~500ms Python subprocess spawn and layout flash. The tab-scoped
  // effects below still run and keep data fresh — they just hit the Rust
  // cache now instead of spawning a cold Python process. Gated on
  // `heavyTabEverOpened` so users who only use Home never trigger it.
  useEffect(() => {
    if (
      windowLabel !== "main" ||
      !trayWindowFocused ||
      !heavyTabEverOpened ||
      activeView === "notifications" ||
      activeView === "optimization"
    ) {
      return;
    }

    let active = true;
    const timeout = window.setTimeout(() => {
      if (!active) {
        return;
      }
      void refreshClaudeProjects();
      void refreshHeadroomLearnPrereq();
      invoke<ActivityFeedResponse>("get_activity_feed")
        .then((next) => {
          if (!active) return;
          setActivityFeed((prev) =>
            activityFeedSignature(prev) === activityFeedSignature(next) ? prev : next
          );
          setActivityFeedError(null);
        })
        .catch(() => {
          // Swallow: the tab-active poll will surface any real error once the
          // user opens Activity. Pre-warm failures shouldn't flash a banner.
        })
        .finally(() => {
          if (!active) return;
          setActivityFeedLoaded(true);
        });
    }, trayFocusPrewarmDelayMs);

    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, [windowLabel, trayWindowFocused, heavyTabEverOpened, activeView]);

  useEffect(() => {
    if (activeView !== "notifications" || !trayWindowFocused) {
      return;
    }
    let active = true;
    const refreshFeed = () => {
      invoke<ActivityFeedResponse>("get_activity_feed")
        .then((next) => {
          if (!active) return;
          setActivityFeed((prev) =>
            activityFeedSignature(prev) === activityFeedSignature(next) ? prev : next
          );
          setActivityFeedError(null);
        })
        .catch((err) => {
          if (!active) return;
          setActivityFeedError(
            err instanceof Error ? err.message : "Could not load activity feed."
          );
        })
        .finally(() => {
          if (!active) return;
          setActivityFeedLoaded(true);
        });
    };
    refreshFeed();
    const interval = window.setInterval(refreshFeed, 4000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [activeView, trayWindowFocused]);

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
    void Promise.all([refreshClaudeProjects(), refreshHeadroomLearnPrereq()]);
  }, [activeView]);

  useEffect(() => {
    if (activeView !== "optimization" || !trayWindowFocused) {
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
  }, [activeView, selectedClaudeProjectPath, headroomLearnStatus.running, trayWindowFocused]);

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

  const claudeProjectPathsKey = claudeProjects
    .map((project) => project.projectPath)
    .sort()
    .join("\t");
  // Batched applied-patterns fetch: one IPC instead of one per OptimizePanel.
  useEffect(() => {
    if (activeView !== "optimization") {
      return;
    }
    const paths = claudeProjectPathsKey === "" ? [] : claudeProjectPathsKey.split("\t");
    if (paths.length === 0) {
      setOptimizeAppliedByProject({});
      return;
    }
    let active = true;
    invoke<Record<string, AppliedPatterns>>("list_applied_patterns_for_projects", {
      projectPaths: paths,
    })
      .then((result) => {
        if (!active) return;
        setOptimizeAppliedByProject(result);
      })
      .catch(() => {
        if (!active) return;
        setOptimizeAppliedByProject(null);
      });
    return () => {
      active = false;
    };
  }, [
    activeView,
    claudeProjectPathsKey,
    headroomLearnStatus.finishedAt,
    optimizeAppliedRefreshTick,
  ]);

  // Keep connectorPhase in sync with the connector enabled state from the backend.
  // Any supported connector (Claude Code, Codex, ...) being enabled counts as
  // "connected" — the request-count poller below is connector-agnostic.
  const anyConnectorEnabled = hasEnabledConnector(connectors);

  // Which agents Headroom Learn should offer, driven by the enabled connectors.
  const claudeLearnEnabled = getClaudeConnector(connectors)?.enabled ?? false;
  const codexLearnEnabled = aggregateClientConnectors(connectors).some(
    (connector) => connector.clientId === "codex" && connector.enabled
  );
  const opencodeLearnEnabled = aggregateClientConnectors(connectors).some(
    (connector) => connector.clientId === "opencode" && connector.enabled
  );
  const grokLearnEnabled = aggregateClientConnectors(connectors).some(
    (connector) => connector.clientId === "grok_build" && connector.enabled
  );
  const learnAgentCount =
    Number(claudeLearnEnabled) +
    Number(codexLearnEnabled) +
    Number(opencodeLearnEnabled) +
    Number(grokLearnEnabled);
  const learnBlurb =
    learnAgentCount > 1
      ? "Headroom learns from your coding agents' sessions. When an agent repeats a mistake, Headroom updates that agent's memory so it doesn't happen again."
      : codexLearnEnabled
        ? "Headroom learns from your ChatGPT (Codex) sessions. When Codex repeats a mistake, Headroom updates your ~/.codex/AGENTS.md and instructions.md so it doesn't happen again."
        : opencodeLearnEnabled || grokLearnEnabled
          ? "Headroom learns from your agent's sessions. When it repeats a mistake, Headroom updates the agent's memory so it doesn't happen again."
          : "Headroom helps Claude Code learn from experience. When Claude makes mistakes, Headroom automatically updates the project's MEMORY.md so they don't happen again. You can also ask Headroom to scan past sessions & add token-saving learnings to CLAUDE.local.md.";
  useEffect(() => {
    // connectors === [] means get_client_connectors hasn't returned yet (the
    // Rust side always lists every managed client). Don't treat that launch
    // window as "user disabled everything", or the persisted verification
    // marker would be wiped on every start.
    if (connectors.length === 0) return;
    if (!anyConnectorEnabled) {
      // A deliberate all-off is the one case that invalidates the persisted
      // marker: re-enabling must re-verify with fresh traffic.
      setConnectorTrafficVerified(false);
      setConnectorPhase("disabled");
      return;
    }
    // Transition from "disabled" → enabled drops into verifying (unless a
    // past session already verified traffic), so the polling effect below
    // confirms via /stats before the badge flips green.
    setConnectorPhase((prev) =>
      prev === "disabled"
        ? isConnectorTrafficVerified()
          ? "healthy"
          : "verifying"
        : prev
    );
  }, [anyConnectorEnabled, connectors.length]);

  useEffect(() => {
    // Pricing status hits the remote Headroom API. When the tray is focused,
    // poll at 60s so fresh subscription/trial state is visible on demand.
    // When hidden, slow to 10 min — still fast enough for trial-expiry and
    // urgent notifications to fire, while cutting hourly API traffic by
    // ~90%. The launcher window never sets `trayWindowFocused` to false
    // (its focus listener isn't wired up), so it keeps the 60s cadence.
    const intervalMs = trayWindowFocused ? 60_000 : 600_000;
    void refreshPricingStatus();
    const interval = window.setInterval(() => {
      void refreshPricingStatus();
    }, intervalMs);
    return () => {
      window.clearInterval(interval);
    };
  }, [trayWindowFocused]);

  // headroom:// deep links from the backend trigger an immediate pricing
  // refresh — the typical case is Polar's checkout success page redirecting
  // to headroom://upgraded. Backend has already reconciled the runtime; this
  // just pulls the new status into UI state without waiting for the next
  // poll tick.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<HeadroomPricingStatus | null>("pricing-refreshed", (event) => {
      // Auth mutations ship the new status with the event. Using it beats a
      // refetch that the in-flight guard may drop outright, leaving this window
      // stale until the next poll tick (60s focused, 600s not).
      if (event.payload) {
        pricingStatusStampRef.current = Date.now();
        setPricingStatus(event.payload);
        return;
      }
      void refreshPricingStatus();
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  // After the user opens a Polar checkout URL, poll pricing status every 5s
  // for up to 5 minutes so we can flip the UI back to "active" within seconds
  // of payment confirmation, instead of waiting out the 60s baseline cadence.
  // Auto-stops once subscription_active is observed or the window expires.
  useEffect(() => {
    if (checkoutPollingDeadline === null) return;
    if (Date.now() > checkoutPollingDeadline) {
      setCheckoutPollingDeadline(null);
      return;
    }
    const interval = window.setInterval(() => {
      if (Date.now() > checkoutPollingDeadline) {
        setCheckoutPollingDeadline(null);
        return;
      }
      void refreshPricingStatus();
    }, 5_000);
    return () => {
      window.clearInterval(interval);
    };
  }, [checkoutPollingDeadline]);

  // Stop the aggressive checkout poll the moment we observe a live
  // subscription. Saves traffic and stops competing with the 60s cadence.
  useEffect(() => {
    if (checkoutPollingDeadline !== null && pricingStatus?.account?.subscriptionActive) {
      setCheckoutPollingDeadline(null);
    }
  }, [checkoutPollingDeadline, pricingStatus?.account?.subscriptionActive]);

  // When the pricing gate closes, pause optimization on enabled connectors
  // one at a time. Each disable refreshes `connectors`, re-running this
  // effect until none remain. Codex is exempt while authenticated:
  // `optimizationAllowed` reflects the *Claude* paid-plan gate, and Codex has
  // its own independent gate enforced proxy-side (codex_bypass) — a Claude
  // weekly cap must not switch off a Codex-heavy user's optimization.
  useEffect(() => {
    if (!pricingStatus || pricingStatus.optimizationAllowed || connectorsBusy) {
      return;
    }
    const target = getEnabledSupportedConnectors(connectors).find(
      (connector) =>
        !pricingStatus.authenticated || !GATE_EXEMPT_CONNECTOR_IDS.has(connector.clientId)
    );
    if (!target) {
      return;
    }
    autoDisabledByGateRef.current.add(target.clientId);
    persistAutoDisabledByGate();
    void toggleConnector(target, false);
  }, [connectors, connectorsBusy, pricingStatus]);

  // Companion to the auto-disable effect above: when the pricing gate
  // releases (e.g., user just signed up post-grace, or weekly usage
  // rolled over), bring back every connector we auto-disabled without forcing
  // a manual re-enable click. Scoped to our own prior auto-disables so a
  // user's manual disable during an ungated period is preserved.
  useEffect(() => {
    if (!pricingStatus?.optimizationAllowed || autoDisabledByGateRef.current.size === 0) {
      return;
    }
    if (connectorsBusy) {
      return;
    }
    const target = aggregateClientConnectors(connectors).find(
      (connector) =>
        autoDisabledByGateRef.current.has(connector.clientId) && !connector.enabled
    );
    if (!target) {
      autoDisabledByGateRef.current.clear();
      persistAutoDisabledByGate();
      return;
    }
    void toggleConnector(target, true);
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
      .then((status) => {
        desktopActivationAttemptsRef.current = 0;
        setPricingStatus(status);
      })
      .catch(() => {
        const attempts = (desktopActivationAttemptsRef.current += 1);
        if (attempts >= DESKTOP_ACTIVATION_MAX_ATTEMPTS) {
          // Leave the guard set: this launch is done trying. The machine is
          // offline or headroom-web is down, and neither is something more
          // requests fix.
          return;
        }
        const delay = Math.min(
          DESKTOP_ACTIVATION_RETRY_BASE_MS * 2 ** (attempts - 1),
          DESKTOP_ACTIVATION_RETRY_MAX_MS
        );
        window.clearTimeout(desktopActivationRetryTimerRef.current);
        desktopActivationRetryTimerRef.current = window.setTimeout(() => {
          desktopActivationSentRef.current = false;
          // The guard alone cannot restart the effect (no dep changed), so
          // bump a counter the effect watches. Without it a cleared guard just
          // waits for the next unrelated reachability flap.
          setDesktopActivationRetry((n) => n + 1);
        }, delay);
      });
  }, [
    connectorPhase,
    desktopActivationRetry,
    pricingStatus?.authenticated,
    runtimeStatus?.proxyReachable,
    runtimeStatus?.running,
  ]);

  // While verifying, poll the proxy's /stats request counter and flip to
  // healthy when it ticks past the anchor we captured on the first reachable
  // poll. The previous implementation scanned the python proxy log for
  // /v1/messages lines, but Claude Code traffic actually flows through the
  // Rust front proxy on 6767 — the python log only sees background activity,
  // so the regex match could hang forever even while requests were being
  // optimized normally.
  useEffect(() => {
    if (connectorPhase !== "verifying") return;
    let active = true;
    let anchor: number | null = null;
    let attempts = 0;
    let timer: number | undefined;
    // Fast feedback for the first minute after setup, then back off:
    // 'verifying' can last days if the user doesn't code, and a menubar app
    // has no business polling the proxy at 1 Hz around the clock.
    const schedule = () => {
      timer = window.setTimeout(() => void tick(), attempts < 60 ? 1000 : 10000);
    };
    const tick = async () => {
      // The launcher's onboarding verify step (a separate webview) may have
      // already observed traffic — honor its persisted marker instead of
      // demanding a second request.
      if (isConnectorTrafficVerified()) {
        setConnectorPhase("healthy");
        return;
      }
      const count = await invoke<number | null>("get_headroom_request_count").catch(
        () => null
      );
      if (!active) return;
      attempts += 1;
      // null = proxy unreachable. Don't anchor on transient
      // unreachable readings — a later reachable reading would otherwise
      // jump from 0 → N and flip the badge healthy without observing
      // any new traffic.
      if (count !== null) {
        if (anchor === null) {
          anchor = count;
        } else if (count > anchor) {
          setConnectorTrafficVerified(true);
          setConnectorPhase("healthy");
          return;
        } else if (count < anchor) {
          // Backend restart reset the /stats counter; re-anchor or the flip
          // would wait for traffic to climb past the stale pre-restart total.
          anchor = count;
        }
      }
      schedule();
    };
    schedule();
    return () => {
      active = false;
      if (timer !== undefined) window.clearTimeout(timer);
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

    // Measured from the latest estimate, not from the start of the step: the
    // backend's number is time remaining as of that update (see
    // stepEtaSeedAtMs). Falling back to the step start keeps the old behaviour
    // for steps that only ever report one, static ETA.
    const anchorMs = stepEtaSeedAtMs ?? stepStartedAtMs;
    const elapsedSeconds = Math.max(0, (Date.now() - anchorMs) / 1000);
    const eta = Math.max(8, stepEtaSeedSeconds || progress.currentStepEtaSeconds || 20);
    const linear = Math.min(0.96, elapsedSeconds / eta);

    let fraction = linear;
    if (elapsedSeconds > eta) {
      const overtime = elapsedSeconds - eta;
      fraction = Math.min(0.995, linear + overtime / (eta * 10));
    }

    stepProgressFloorRef.current = Math.max(stepProgressFloorRef.current, fraction);
    return stepProgressFloorRef.current;
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

    const anchorMs = stepEtaSeedAtMs ?? stepStartedAtMs;
    const elapsedSeconds = anchorMs
      ? Math.max(0, Math.round((Date.now() - anchorMs) / 1000))
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
    // Past an hour the seconds are noise and the minutes read as a wall of
    // digits. A slow link legitimately lands here, and "1h 20m" is the number
    // that stops someone concluding the install is wedged and quitting.
    if (mins >= 60) {
      return `ETA: ${Math.floor(mins / 60)}h ${mins % 60}m`;
    }
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
    // Codex configuration is written to ~/.codex/config.toml, which both the CLI
    // and the GUI app read, so the toggle should be usable even when the CLI
    // binary isn't on the app's PATH (same rationale as claude_code).
    // opencode's default install (~/.opencode/bin) is likewise invisible to
    // the GUI app's PATH while its config is a dotfile we can always write.
    return (
      connector.installed ||
      connector.clientId === "claude_code" ||
      connector.clientId === "codex" ||
      connector.clientId === "grok_build" ||
      connector.clientId === "opencode"
    );
  }

  function getConnectorSupportWarning(connector: ClientConnectorStatus) {
    return connectorSupportWarnings[connector.clientId] ?? null;
  }

  // Pricing gate: enabling a connector while optimization is disallowed just
  // triggers the auto-disable effect (the ON->OFF flash). Mirror that effect's
  // exemption exactly -- Codex is exempt while authenticated (its own
  // proxy-side gate). Only blocks *enabling*; an already-on connector is left
  // to the auto-disable effect.
  function connectorGateBlocksEnable(connector: ClientConnectorStatus) {
    return (
      pricingStatus != null &&
      !pricingStatus.optimizationAllowed &&
      !connector.enabled &&
      (!pricingStatus.authenticated || !GATE_EXEMPT_CONNECTOR_IDS.has(connector.clientId))
    );
  }

  function connectorGateCta() {
    if (pricingStatus?.authenticated) {
      setActiveView("upgrade");
    } else {
      openUpgradeAuthView();
    }
  }

  function getConnectorDetectionWarning(connector: ClientConnectorStatus) {
    if (connector.installed) {
      return null;
    }
    return connectorUnavailableReasons[connector.clientId] ?? null;
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
        const notifyFresh = shouldNotifyAboutAvailableAppUpdate({
          background,
          availableUpdate: patch.availableUpdate,
          knownUpdateVersion,
          windowVisible,
        });
        if (notifyFresh) {
          await sendAppUpdateNotification(patch.availableUpdate.version);
        }
        // Never stack the stale reminder onto the tick that just announced
        // the release — first discovery of an old version used to fire both
        // notifications at once.
        if (!windowVisible && !notifyFresh) {
          await maybeFireStaleAppUpdateNotification(patch.availableUpdate);
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
      const versionForCopy = appUpdateAvailable.version;
      applyAppUpdatePatch(
        await runAppUpdateInstall({
          availableUpdate: appUpdateAvailable,
          onProgress: (progress) => {
            setAppUpdateStatusCopy(formatAppUpdateProgressCopy(versionForCopy, progress));
          },
        })
      );
    } finally {
      setAppUpdateInstallBusy(false);
    }
  }

  /// One progressive button on the failed-install screen: check -> install ->
  /// restart. That screen has no other route to a newer build, and when the
  /// cause is a bad pin in the lock we shipped (RUST-1G: onnxruntime on Intel
  /// macOS) a newer build is the *only* fix -- Try again re-resolves the same
  /// impossible pin forever.
  async function handleFailedInstallUpdateAction() {
    if (appUpdateReadyToRestart) {
      restartIntoInstalledUpdate();
      return;
    }
    if (appUpdateAvailable) {
      await installAvailableUpdate();
      return;
    }
    // Foreground check, so a "Up to date."/error line always answers the click.
    await checkForAppUpdate();
  }

  async function handleFailedInstallSupportMail() {
    const report = await invoke<BootstrapFailureReport | null>(
      "get_bootstrap_failure_report"
    ).catch(() => null);
    await invoke("open_external_link", {
      url: buildInstallFailureMailto({
        kind: report?.kind ?? null,
        detail: report?.detail ?? null,
        appVersion: appSemver,
        platform: runtimeStatus?.platform ?? "unknown",
      }),
    });
  }

  function restartIntoInstalledUpdate() {
    // Never reset: restart_app tears down the backend before exiting, which
    // can take seconds on slow machines — the busy label is the only signal
    // the click registered until the window dies.
    setAppUpdateRestartBusy(true);
    void invoke("restart_app");
  }

  // Single fetch path so hidden connectors can never leak into decision
  // logic (launcher auto-configure, verification rows) via a raw invoke.
  async function fetchConnectors(): Promise<ClientConnectorStatus[]> {
    const items = await invoke<ClientConnectorStatus[]>("get_client_connectors");
    return withoutHiddenConnectors(items);
  }

  async function refreshConnectors() {
    try {
      setConnectorsError(null);
      const items = await fetchConnectors();
      // Set unconditionally, unlike the state below: an empty result is a real
      // answer ("nothing connected") that the diff-and-set helper swallows,
      // and the setup-stall watchdog has to tell it apart from "not loaded".
      connectorsRef.current = items;
      applyConnectorsIfChanged(items);
    } catch (error) {
      setConnectorsError(
        describeInvokeError(error, "Could not load connector status.")
      );
    }
  }

  async function refreshRuntimeStatus() {
    try {
      const runtime = await invoke<RuntimeStatus>("get_runtime_status");
      applyRuntimeStatusIfChanged(runtime);
      void maybeFireUrgentRuntimeNotification(runtime);
    } catch (error) {
      setConnectorsError(
        describeInvokeError(error, "Could not load runtime status.")
      );
    }
  }

  async function handleResumeRuntime() {
    if (resuming) {
      return;
    }
    setResuming(true);
    setResumeError(null);
    try {
      await invoke("force_restart_headroom");
      await refreshRuntimeStatus();
    } catch (error) {
      setResumeError(
        describeInvokeError(error, "Could not restart Headroom.")
      );
    } finally {
      setResuming(false);
    }
  }

  async function refreshPricingStatus() {
    if (pricingRefreshInFlightRef.current) {
      return;
    }
    pricingRefreshInFlightRef.current = true;
    setPricingBusy(true);
    const issuedAt = Date.now();
    try {
      const status = await invoke<HeadroomPricingStatus>("get_headroom_pricing_status");
      if (pricingStatusStampRef.current > issuedAt) {
        return;
      }
      setPricingStatus(status);
      // The code step is local UI state, so a sign-in that happened elsewhere
      // (the magic link, or the other window) leaves this one still asking for
      // a code nobody needs to enter. Every refresh path lands here.
      if (status.authenticated) {
        setAuthCode("");
        setAuthCodeRequestedFor(null);
      }
      void maybeFireTrialNotifications(status);
      void maybeFireUrgentPricingNotifications(status);
      setPricingError(null);
    } catch (error) {
      setPricingError(
        describeInvokeError(error, "Could not load pricing status.")
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
      applyClaudeProjectsIfChanged(projects);
    } catch (error) {
      setClaudeProjectsError(
        describeInvokeError(error, "Could not load Claude Code projects.")
      );
    } finally {
      setClaudeProjectsBusy(false);
    }
  }

  async function refreshHeadroomLearnPrereq(force = false) {
    try {
      const status = await invoke<HeadroomLearnPrereqStatus>("get_headroom_learn_prereq_status", {
        force,
      });
      setHeadroomLearnPrereq(status);
    } catch {
      setHeadroomLearnPrereq(idleHeadroomLearnPrereqStatus);
    }
  }

  async function copyLearnInstallCommand(command: string) {
    try {
      if (!navigator.clipboard) {
        throw new Error("Clipboard API unavailable");
      }
      await navigator.clipboard.writeText(command);
      setLearnInstallCopyNotice("Copied install command.");
      window.setTimeout(() => setLearnInstallCopyNotice(null), 2000);
    } catch {
      setLearnInstallCopyNotice("Copy failed. Select the command and copy manually.");
      window.setTimeout(() => setLearnInstallCopyNotice(null), 3000);
    }
  }

  async function autoConfigureConnectorsForLauncher() {
    setConnectorsBusy(true);
    setConnectorsError(null);

    try {
      let latestConnectors = await fetchConnectors();
      applyConnectorsIfChanged(latestConnectors);

      const decision = getLauncherAutoConfigureDecision(latestConnectors);
      const step = nextAutoConfigureStep(decision, latestConnectors);

      if (step.kind === "show_client_setup") {
        // Landing here at launch means the probe found no installed client
        // (decision "show_client_setup" is exactly installed.length === 0) --
        // the biggest unexplained drop in the Windows funnel. Name it so the
        // per-OS funnel can separate "no editor found" from "found but unused".
        if (decision === "show_client_setup") {
          reportFunnelStep("client_setup_no_clients_detected");
        }
        setLauncherStage("client_setup");
        return;
      }

      if (step.kind === "apply") {
        for (const clientId of step.clientIds) {
          const result = await invoke<ClientSetupResult>("apply_client_setup", { clientId });
          if (result.replacedBaseUrl) {
            setConnectorsNotice(baseUrlTakeoverNotice(result.replacedBaseUrl));
          }
        }
        latestConnectors = await fetchConnectors();
        applyConnectorsIfChanged(latestConnectors);

        const postApplyStep = nextAutoConfigureStepAfterApply(
          getLauncherAutoConfigureDecision(latestConnectors)
        );
        if (postApplyStep.kind !== "begin_proxy_verification") {
          setLauncherStage("client_setup");
          return;
        }
      }

      await beginProxyVerificationStep();
    } catch (error) {
      setConnectorsError(
        describeInvokeError(error, "Could not configure your coding tools automatically.")
      );
      setLauncherStage("client_setup");
    } finally {
      setConnectorsBusy(false);
    }
  }

  async function handleFirstLaunchContinue() {
    await autoConfigureConnectorsForLauncher();
  }

  async function handleInstallPrimaryAction() {
    // No checkout gate: the email-harvested trial user installs and uses
    // Headroom directly. Upgrading happens later from the in-app upgrade sheet.
    await handleBootstrap();
  }

  async function runHeadroomLearn(
    agent: "claude" | "codex" | "opencode" | "grok",
    projectPath?: string
  ) {
    if (runtimeStatus?.headroomLearnSupported === false) {
      setHeadroomLearnStatus((current) => ({
        ...current,
        running: false,
        summary: "Headroom Learn is unavailable on this platform.",
        error:
          runtimeStatus.headroomLearnDisabledReason ??
          "Headroom Learn is unavailable on this platform."
      }));
      return;
    }

    // Only Claude is project-organized; every other agent shares a stable
    // run key equal to its agent id.
    const runKey = agent === "claude" ? (projectPath ?? "") : agent;
    const displayName =
      agent === "codex"
        ? "ChatGPT sessions"
        : agent === "opencode"
          ? "OpenCode sessions"
          : agent === "grok"
            ? "Grok sessions"
            : (claudeProjects.find((project) => project.projectPath === projectPath)?.displayName ??
              projectPath ??
              "");
    const startupSummary = `Running headroom learn for ${displayName}.`;
    trackAnalyticsEvent("headroom_learn_run", { agent });
    setHeadroomLearnBusy(true);
    setHeadroomLearnStatus((current) => ({
      ...current,
      running: true,
      projectPath: runKey,
      projectDisplayName: displayName,
      startedAt: new Date().toISOString(),
      finishedAt: null,
      progressPercent: Math.max(8, current.progressPercent || 0),
      summary: startupSummary,
      success: null,
      error: null
    }));
    try {
      await invoke("start_headroom_learn", { agent, projectPath: projectPath ?? null });
      for (const waitMs of [180, 350, 650, 900, 1200, 1800, 2400]) {
        await delay(waitMs);
        const status = await invoke<HeadroomLearnStatus>("get_headroom_learn_status", {
          projectPath: runKey
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
        error: describeInvokeError(error, "Failed to start headroom learn.")
      }));
    } finally {
      setHeadroomLearnBusy(false);
    }
  }

  async function handleRunHeadroomLearn(
    agent: "claude" | "codex" | "opencode" | "grok",
    projectPath?: string
  ) {
    if (agent === "claude" && projectPath) {
      setSelectedClaudeProjectPath(projectPath);
    }
    try {
      const status = await invoke<HeadroomLearnPrereqStatus>("get_headroom_learn_prereq_status");
      setHeadroomLearnPrereq(status);
      // opencode/grok sessions are read from disk; analysis runs through
      // whichever supported CLI exists (mirrors the Rust prereq check).
      const ready =
        agent === "codex"
          ? status.codexCliAvailable && status.codexLoggedIn
          : agent === "claude"
            ? status.claudeCliAvailable
            : status.claudeCliAvailable || (status.codexCliAvailable && status.codexLoggedIn);
      if (!ready) {
        return;
      }
    } catch {
      setHeadroomLearnPrereq(idleHeadroomLearnPrereqStatus);
      return;
    }
    await runHeadroomLearn(agent, projectPath);
  }

  async function openExternalLink(url: string) {
    await invoke("open_external_link", { url });
  }

  async function runAddonAction(
    command: "install_addon" | "set_addon_enabled" | "uninstall_addon",
    id: string,
    enabled?: boolean,
    // An update reuses install_addon, so only the wording differs.
    updateLabels?: { busy: string; done: string }
  ) {
    const copy = addonCopy[id];
    const busyLabel =
      command === "install_addon"
        ? (updateLabels?.busy ?? copy?.installing)
        : command === "uninstall_addon"
          ? copy?.uninstalling
          : enabled
            ? copy?.enabling
            : copy?.disabling;
    setAddonBusyId(id);
    setAddonBusyLabel(busyLabel ?? null);
    setAddonError(null);
    setAddonResult(null);
    try {
      const next = await invoke<DashboardState>(command, { id, enabled });
      setDashboard(next);
      if (id === "rtk") {
        await refreshRuntimeStatus();
      }
      const message =
        command === "install_addon"
          ? (updateLabels?.done ?? copy?.installed)
          : command === "uninstall_addon"
            ? copy?.uninstalled
            : enabled
              ? undefined
              : copy?.disabled;
      if (message) {
        setAddonResult({ id, message });
      }
    } catch (error) {
      setAddonError(
        describeInvokeError(error, "The addon action could not be completed.")
      );
    } finally {
      setAddonBusyId(null);
      setAddonBusyLabel(null);
    }
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
      reportFunnelStep("email_code_requested");
      setAuthCodeRequestedFor(result.email);
      setAuthFlowSuccess(
        authCodeSentMessage(
          result.email,
          result.expiresInSeconds || authCodeExpiryFallbackSeconds
        )
      );
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
    await verifyAuthCode(authEmail.trim(), authCode.trim());
  }

  async function verifyAuthCode(email: string, code: string): Promise<boolean> {
    setAuthVerifyBusy(true);
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
    try {
      const status = await invoke<HeadroomPricingStatus>("verify_headroom_auth_code", {
        email,
        code,
        inviteCode: null
      });
      pricingStatusStampRef.current = Date.now();
      setPricingStatus(status);
      setAuthCode("");
      setAuthCodeRequestedFor(null);
      reportFunnelStep("email_code_verified");
      setAuthFlowSuccess("Headroom account connected.");
      setPendingUpgradePlanId(null);
      // The launcher paywall renders its own sign-in inline; switching the
      // (hidden) main-window view would be meaningless there.
      if (windowLabel !== "launcher") {
        setActiveView("upgrade");
      }
      // Not awaited: sign-in is done, and a connector rescan (filesystem walk
      // over every client config, slow on Windows) has nothing to do with it.
      // Awaiting it kept the button on "Verifying..." long after the account
      // was connected.
      void refreshConnectors();
      return true;
    } catch (error) {
      setAuthFlowError(describeInvokeError(error, "Could not verify sign-in code."));
      return false;
    } finally {
      setAuthVerifyBusy(false);
    }
  }

  // Magic sign-in link (headroom://auth). The browser deliberately cannot sign
  // anyone in -- it has none of the device fingerprints verify_code needs -- so
  // it only hands over the code and this is the ordinary typed-code flow with
  // the typing removed.
  //
  // Drained on mount as well as on the event: a cold start launched *by* the
  // link delivers the URL before this listener exists, so an event alone would
  // be lost exactly when the link was the thing that opened the app. Rust hands
  // the slot out once, so both windows can race for it harmlessly.
  // Only the launcher claims it: both windows mount this component and the slot
  // is one-shot, so the hidden main window could win the race and sign the user
  // in where nobody could see it. The deep-link handler shows the launcher
  // before parking the credentials, so it is the window on screen.
  useEffect(() => {
    if (windowLabel !== "launcher") {
      return;
    }
    let cancelled = false;
    async function claimMagicLink() {
      const pending = await invoke<[string, string] | null>("take_pending_magic_link");
      if (cancelled || !pending) {
        return;
      }
      const [email, code] = pending;
      setAuthEmail(email);
      setMagicLinkState("verifying");
      const verified = await verifyAuthCode(email, code);
      if (cancelled) {
        return;
      }
      // Success needs no screen: clearing this drops the user straight onto the
      // next onboarding step, already signed in.
      setMagicLinkState(verified ? null : "failed");
    }
    void claimMagicLink();
    const unlistenPromise = listen("magic-link-auth", () => {
      void claimMagicLink();
    });
    return () => {
      cancelled = true;
      void unlistenPromise.then((unlisten) => unlisten());
    };
  }, [windowLabel]);

  async function handleSignOutHeadroomAccount() {
    setAuthFlowError(null);
    setAuthFlowSuccess(null);
    try {
      await invoke("sign_out_headroom_account");
      const status = await invoke<HeadroomPricingStatus>("get_headroom_pricing_status");
      pricingStatusStampRef.current = Date.now();
      setPricingStatus(status);
      setAuthCode("");
      setAuthCodeRequestedFor(null);
      setAuthFlowSuccess("Signed out of Headroom.");
      setPendingUpgradePlanId(null);
    } catch (error) {
      setAuthFlowError(
        describeInvokeError(error, "Could not sign out of Headroom.")
      );
    }
  }

  async function openLearnInstallDocsLink() {
    try {
      await openExternalLink(CLAUDE_CODE_INSTALL_DOCS_URL);
    } catch (error) {
      setLearnInstallCopyNotice(
        describeInvokeError(error, "Could not open the install guide.")
      );
      window.setTimeout(() => setLearnInstallCopyNotice(null), 3000);
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
            kind: activeHeadroomPlanId ? "billing_portal" as const : "internal" as const
          };
        case "pro":
        case "max5x":
        case "max20x": {
          // Same tier on the other billing period is a real change (Polar swaps
          // the product), not the plan you are already on.
          if (
            activeHeadroomPlanId === planId &&
            matchesSubscriptionPeriod(
              billingPeriod,
              pricingStatus?.account?.subscriptionBillingPeriod
            )
          ) {
            return { kind: "internal" as const };
          }
          // AppSumo-entitled accounts have no Polar subscription to change in
          // place; the server names the route that works (their AppSumo
          // account page while the deal is live, a fresh checkout afterwards).
          const upgradeAction = pricingStatus?.account?.upgradeAction;
          if (upgradeAction === "appsumo") {
            return { kind: "external" as const, url: APPSUMO_ACCOUNT_URL };
          }
          if (upgradeAction === "checkout") {
            return { kind: "checkout" as const };
          }
          // Polar prorates the product swap with the existing discount applied,
          // so every plan change on an active subscription uses the PATCH path.
          if (activeHeadroomPlanId) {
            return { kind: "change_plan" as const };
          }
          return { kind: "checkout" as const };
        }
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

    if (action.kind === "change_plan") {
      const fromTier = pricingStatus?.account?.subscriptionTier;
      if (!fromTier) return;
      setPlanChangeError(null);
      setPendingPlanChange({
        fromTier,
        toTier: planId as HeadroomSubscriptionTier,
        billingPeriod
      });
      return;
    }

    if (action.kind === "checkout") {
      setUpgradeActionBusy(planId);
      setUpgradeActionError(null);

      try {
        const url = await invoke<string>("create_headroom_checkout_session", {
          subscriptionTier: planId,
          billingPeriod
        });
        await openExternalLink(url);
        // Aggressive poll for the next 5 minutes so the moment Polar marks
        // the subscription active we surface "Headroom is back online" without
        // making the user wait out the normal 60s pricing-refresh cadence.
        setCheckoutPollingDeadline(Date.now() + 5 * 60_000);
      } catch (error) {
        setUpgradeActionError(
          describeInvokeError(error, "Could not start checkout.")
        );
      } finally {
        setUpgradeActionBusy(null);
      }
      return;
    }

    if (action.kind === "billing_portal") {
      setUpgradeActionBusy(planId);
      setUpgradeActionError(null);

      try {
        // Plain trip to the portal. Cancelling has its own entry point that asks
        // why first; someone updating a card should not meet a retention pitch.
        await openBillingPortal();
      } catch (error) {
        setUpgradeActionError(
          describeInvokeError(error, "Could not open billing portal.")
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
        describeInvokeError(error, "Could not open the selected plan link.")
      );
    } finally {
      setUpgradeActionBusy(null);
    }
  }

  async function confirmPlanChange() {
    if (!pendingPlanChange) return;
    setPlanChangeBusy(true);
    setPlanChangeError(null);
    try {
      await invoke("change_headroom_subscription_plan", {
        subscriptionTier: pendingPlanChange.toTier,
        billingPeriod: pendingPlanChange.billingPeriod
      });
      await refreshPricingStatus();
      setPendingPlanChange(null);
      setActiveView("home");
    } catch (error) {
      setPlanChangeError(
        error instanceof Error
          ? error.message
          : typeof error === "string"
            ? error
            : "Could not change subscription plan."
      );
    } finally {
      setPlanChangeBusy(false);
    }
  }

  function cancelPlanChange() {
    if (planChangeBusy) return;
    setPendingPlanChange(null);
    setPlanChangeError(null);
  }

  async function openBillingPortal() {
    // Deep-link to the user's subscription page so they land one click away
    // from "Change plan" instead of at the portal root.
    const url = await invoke<string>("get_headroom_billing_portal_url", {
      target: "subscription"
    });
    await openExternalLink(url);
  }

  function openCancelReason() {
    setCancelReason("");
    setCancelNote("");
    setUpgradeActionError(null);
    setCancelReasonOpen(true);
  }

  async function handleCancelContinue() {
    if (!cancelReason || cancelBusy) return;
    setCancelBusy(true);
    // Fails open: the reason is a nice-to-have, being trapped in the app is not.
    const offer = await invoke<SaveOffer | null>("submit_headroom_cancellation_intent", {
      reason: cancelReason,
      note: cancelNote.trim() || null
    }).catch(() => null);
    setCancelBusy(false);
    setCancelReasonOpen(false);

    if (offer) {
      setSaveOfferError(null);
      setSaveOfferRedeemed(false);
      setSaveOffer(offer);
      return;
    }

    setUpgradeActionBusy("cancel");
    try {
      await openBillingPortal();
    } catch (error) {
      setUpgradeActionError(
        describeInvokeError(error, "Could not open billing portal.")
      );
    } finally {
      setUpgradeActionBusy(null);
    }
  }

  async function handleRedeemSaveOffer() {
    if (saveOfferBusy) return;
    setSaveOfferBusy(true);
    setSaveOfferError(null);
    try {
      await invoke("redeem_headroom_save_offer");
      setSaveOfferRedeemed(true);
      await refreshPricingStatus();
    } catch (error) {
      setSaveOfferError(
        error instanceof Error
          ? error.message
          : typeof error === "string"
            ? error
            : "Could not apply the offer."
      );
    } finally {
      setSaveOfferBusy(false);
    }
  }

  async function handleDeclineSaveOffer() {
    if (saveOfferBusy) return;
    setSaveOffer(null);
    setUpgradeActionBusy("cancel");
    try {
      await openBillingPortal();
    } catch (error) {
      setUpgradeActionError(
        describeInvokeError(error, "Could not open billing portal.")
      );
    } finally {
      setUpgradeActionBusy(null);
    }
  }

  async function handleReactivateSubscription() {
    if (reactivateBusy) return;
    setReactivateBusy(true);
    setReactivateError(null);
    try {
      await invoke("reactivate_headroom_subscription");
      await refreshPricingStatus();
    } catch (error) {
      setReactivateError(
        error instanceof Error
          ? error.message
          : typeof error === "string"
            ? error
            : "Could not reactivate subscription."
      );
    } finally {
      setReactivateBusy(false);
    }
  }

  async function handleContactSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();

    const validationError = getContactRequestValidationError(CONTACT_FORM_URL, contactEmail);
    if (validationError) {
      setContactSubmitError(validationError);
      setContactSubmitSuccess(null);
      return;
    }

    const trimmed = contactEmail.trim();
    const trimmedMessage = contactMessage.trim().slice(0, 2000);
    setContactSubmitBusy(true);
    setContactSubmitError(null);
    setContactSubmitSuccess(null);

    try {
      await invoke("submit_contact_request", {
        url: CONTACT_FORM_URL,
        email: trimmed,
        message: trimmedMessage || null,
      });
      setContactEmail("");
      setContactMessage("");
      setContactSubmitSuccess("Thanks. Check your inbox for a confirmation email.");
    } catch (error) {
      setContactSubmitError(
        describeInvokeError(error, "Could not submit the contact request.")
      );
    } finally {
      setContactSubmitBusy(false);
    }
  }

  async function beginProxyVerificationStep() {
    let fresh = connectors;
    try {
      fresh = await fetchConnectors();
      applyConnectorsIfChanged(fresh);
    } catch {
      // fall back to cached state
    }

    setLauncherStage("proxy_verify");
    setProxyVerificationHint(null);
    setProxyVerifySkipArmed(false);
    setProxyVerificationRows(buildInitialProxyVerificationRows(fresh));
    // Reset to null so the polling effect re-anchors on its first reachable
    // /stats reading. Setting it here would risk anchoring on a stale value
    // from a prior visit to this stage.
    proxyVerificationRequestAnchorRef.current = null;
  }

  async function toggleConnector(connector: ClientConnectorStatus, nextEnabled: boolean) {
    setConnectorsBusy(true);
    setConnectorsError(null);
    try {
      if (nextEnabled) {
        const result = await invoke<ClientSetupResult>("apply_client_setup", {
          clientId: connector.clientId,
        });
        setConnectorsNotice(clientSetupNotice(connector.name, result));
      } else {
        await invoke("disable_client_setup", { clientId: connector.clientId });
        setConnectorsNotice(null);
      }

      const latestDashboard = await loadDashboard();
      applyDashboardIfChanged(latestDashboard);
      await refreshConnectors();
    } catch (error) {
      setConnectorsError(
        describeInvokeError(error, "Failed to update connector.")
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
    window.setTimeout(() => {
      invoke("hide_launcher_animated").catch((error) => {
        // Surface the failure and re-arm so a second click can retry instead
        // of silently dead-ending (the 400ms reset may have been throttled
        // while the window was hidden).
        console.error("hide_launcher_animated failed", error);
        document.documentElement.classList.remove("window-hiding");
        hidingRef.current = false;
      });
    }, launcherHideAnimationMs);
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
  // Paired context for the savings headline. The headline rate dilutes as the
  // client's prompt caching improves, because cache reads sit in its
  // denominator while compression deliberately never touches the cached
  // prefix -- so a healthier cache reads as a Headroom regression. Show the
  // two forces side by side instead: how much of lifetime input the client's
  // cache served (cheap, never claimed by Headroom), and how much of the
  // REMAINING (compressible) input Headroom removed.
  // All three rows go through cacheHitPair, which prices both rates in
  // dollars (see its doc for why tokens are invalid here). The all-time row
  // feeds it the lifetime breakdown as a single synthetic bucket;
  // cacheReadTokens is used only as an existence signal for coverage, never
  // ratioed against our own token counts.
  const cachePairAllTime = allTimeCacheHitPair(
    dashboard.savingsBreakdown,
    dashboard.lifetimeEstimatedSavingsUsd
  );
  const compressionOfRestPct = cachePairAllTime?.compressedPct ?? null;
  // Same pair for the shorter windows, from the buckets that carry cache
  // coverage (backend history checkpoints; local-tracker buckets and days
  // aged out of retention are excluded from both rates). The all-time row
  // above uses the true lifetime breakdown instead, which predates coverage.
  const cachePairToday = cacheHitPair(
    buildHourlySavingsWindow(dashboard.hourlySavings, new Date())
  );
  const cachePairMonth = cacheHitPair(
    buildMonthlySavingsWindow(dashboard.dailySavings, new Date())
  );
  const rtkAvgSavingsPct =
    runtimeStatus?.rtk.installed && (runtimeStatus.rtk.totalCommands ?? 0) > 0
      ? runtimeStatus.rtk.avgSavingsPct ?? 0
      : null;
  const rtkSavingsChip =
    runtimeStatus?.rtk.installed && (runtimeStatus.rtk.totalSaved ?? 0) > 0
      ? `~${compactNumber(runtimeStatus.rtk.totalSaved ?? 0)} tokens saved`
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

  // Ahead of the terms gate: redemption is already under way by the time that
  // gate renders, and its sign-in form has no busy state of its own, so the
  // wait read as nothing happening at all.
  if (windowLabel === "launcher" && magicLinkState !== null) {
    const magicLinkCopy = magicLinkScreenCopy(
      magicLinkState,
      pricingStatus?.account?.email ?? authEmail,
      authFlowError
    );
    return (
      <LauncherShell
        shellClassName="intro-shell intro-shell--post-install"
        spinnerClassName="intro-shell__spinner intro-shell__spinner--post-install"
        copyClassName="intro-shell__copy intro-shell__copy--post-install"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
        showSpinner={magicLinkState === "verifying"}
      >
        <h1>{magicLinkCopy.title}</h1>
        <p className="launcher-install-notice">{magicLinkCopy.body}</p>
        {magicLinkState === "verifying" ? null : (
          <button
            className="primary-button primary-button--large primary-button--success"
            onClick={() => setMagicLinkState(null)}
            type="button"
          >
            Continue
          </button>
        )}
      </LauncherShell>
    );
  }

  // Block every window (launcher and main) until the user accepts the current
  // Terms of Service. New installs hit this in the launcher; updating users —
  // who may never see the launcher — hit it in the main window. Bumping the
  // backend's REQUIRED_TERMS_VERSION re-triggers it on the next launch.
  if (
    needsTermsAcceptance(
      dashboard.requiredTermsVersion,
      dashboard.acceptedTermsVersion
    )
  ) {
    return (
      <TermsGate
        requiredVersion={dashboard.requiredTermsVersion}
        termsUrl={dashboard.termsUrl}
        onAccepted={() => {
          setDashboard((prev) => ({
            ...prev,
            acceptedTermsVersion: prev.requiredTermsVersion
          }));
          // Paywall-first users stay on the landing screen; the primary CTA
          // routes them through connector setup before checkout.
        }}
        authSection={
          paywallFirstFlow && windowLabel === "launcher" ? (
            pricingStatus?.authenticated === true ? (
              <p className="paywall__account-row">
                Signed in as {pricingStatus?.account?.email ?? authEmail}
                {" • "}
                <button
                  className="link-button"
                  onClick={() => void handleSignOutHeadroomAccount()}
                  type="button"
                >
                  or use a different email
                </button>
              </p>
            ) : (
              <AuthCodeForm
                email={authEmail}
                onEmailChange={setAuthEmail}
                emailValid={authEmailValid}
                code={authCode}
                onCodeChange={setAuthCode}
                codeRequested={authCodeRequestedFor !== null}
                requestBusy={authRequestBusy}
                verifyBusy={authVerifyBusy}
                error={authFlowError}
                success={authFlowSuccess}
                onRequestCode={() => void handleRequestAuthCode()}
                onVerify={() => void handleVerifyAuthCode()}
              />
            )
          ) : undefined
        }
        authComplete={pricingStatus?.authenticated === true}
      />
    );
  }

  const upgradeFailure = runtimeStatus?.runtimeUpgradeFailure ?? null;
  const showUpgradeModal =
    runtimeUpgradeProgress.running &&
    !runtimeUpgradeProgress.complete &&
    !runtimeUpgradeProgress.failed;
  const showUpgradeSuccess =
    !runtimeUpgradeProgress.running &&
    runtimeUpgradeProgress.complete &&
    !runtimeUpgradeProgress.failed;
  const showUpgradeBanner =
    !runtimeUpgradeProgress.running && upgradeFailure !== null;
  const upgradeExhausted =
    upgradeFailure !== null && upgradeFailure.attempts >= MAX_UPGRADE_AUTO_RETRIES;
  const canDismissUpgradeFailure =
    upgradeFailure !== null &&
    upgradeFailure.rollbackRestored &&
    runtimeStatus?.proxyReachable === true;

  const upgradeOverlay = (
    <>
      {showUpgradeModal && (
        <div
          className="modal-backdrop runtime-upgrade-backdrop"
          role="dialog"
          aria-modal="true"
        >
          <div className="modal-card runtime-upgrade-modal">
            <h3>
              {runtimeUpgradeProgress.toVersion
                ? `Finishing Headroom update to ${runtimeUpgradeProgress.toVersion}…`
                : "Finishing Headroom update…"}
            </h3>
            <p className="runtime-upgrade-modal__sub">
              {runtimeUpgradeProgress.fromVersion
                ? `From ${runtimeUpgradeProgress.fromVersion}`
                : ""}
            </p>
            <div className="install-progress__bar-track">
              <div
                className="install-progress__bar-fill"
                style={{ width: `${runtimeUpgradeProgress.overallPercent}%` }}
              />
            </div>
            <p className="runtime-upgrade-modal__step">
              {runtimeUpgradeProgress.currentStep}
            </p>
            <p className="runtime-upgrade-modal__message">
              {runtimeUpgradeProgress.message}
            </p>
          </div>
        </div>
      )}
      {showUpgradeBanner && upgradeFailure && (
        <div
          className={`runtime-upgrade-banner runtime-upgrade-banner--${upgradeFailure.failurePhase}`}
          role="alert"
        >
          <div className="runtime-upgrade-banner__body">
            <strong>
              {upgradeFailure.failurePhase === "boot_validation"
                ? `headroom-ai ${upgradeFailure.targetHeadroomVersion} installed but didn't start.`
                : "Headroom update didn't finish."}
            </strong>
            <span>
              {upgradeFailure.errorHint ??
                (upgradeFailure.failurePhase === "boot_validation" &&
                upgradeFailure.fallbackHeadroomVersion
                  ? `Reverted to headroom-ai ${upgradeFailure.fallbackHeadroomVersion}.`
                  : "Running the previous headroom-ai version.")}
            </span>
            {upgradeExhausted && (
              <span className="runtime-upgrade-banner__note">
                We won't auto-retry on launch. Click Retry to try again.
              </span>
            )}
          </div>
          <div className="runtime-upgrade-banner__actions">
            <button
              type="button"
              className="primary-button primary-button--small"
              onClick={() => void invoke("retry_runtime_upgrade")}
              disabled={runtimeUpgradeProgress.running}
            >
              Retry now
            </button>
            {upgradeFailure.failurePhase === "boot_validation" && (
              <button
                type="button"
                className="secondary-button secondary-button--small"
                onClick={() =>
                  void invoke("retry_runtime_upgrade_with_rebuild")
                }
                disabled={runtimeUpgradeProgress.running}
              >
                Retry with full rebuild
              </button>
            )}
            {upgradeFailure.failurePhase === "boot_validation" && (
              <button
                type="button"
                className="secondary-button secondary-button--small"
                onClick={() =>
                  void invoke("open_external_link", {
                    url: buildUpgradeIssueMailto(upgradeFailure),
                  }).catch(() => {})
                }
              >
                Report issue
              </button>
            )}
            {canDismissUpgradeFailure && (
              <button
                type="button"
                className="secondary-button secondary-button--small"
                onClick={() =>
                  void invoke("dismiss_runtime_upgrade_failure").catch(() => {})
                }
              >
                Dismiss
              </button>
            )}
          </div>
        </div>
      )}
    </>
  );

  // While a runtime upgrade is in flight, the venv is in the middle of being
  // swapped so `bootstrapComplete` may return false. Don't render the first-
  // run install wizard in that case — render a dedicated update screen in the
  // launcher instead.
  if (
    windowLabel === "launcher" &&
    (showUpgradeModal || showUpgradeSuccess || (showUpgradeBanner && upgradeFailure))
  ) {
    return (
      <LauncherShell
        shellClassName="intro-shell intro-shell--post-install"
        spinnerClassName="intro-shell__spinner intro-shell__spinner--post-install"
        copyClassName="intro-shell__copy intro-shell__copy--post-install"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
        showSpinner={showUpgradeModal}
      >
        {showUpgradeSuccess ? (
          <>
            <h1>
              {`Headroom ${runtimeUpgradeProgress.toVersion ?? ""} is ready`}
            </h1>
            <p className="launcher-install-notice">
              {runtimeUpgradeProgress.message}
            </p>
            <div className="install-progress-shell">
              <div className="install-progress" aria-live="polite">
                <div className="install-progress__bar-track">
                  <div
                    className="install-progress__bar-fill"
                    style={{ width: "100%" }}
                  />
                </div>
              </div>
            </div>
          </>
        ) : showUpgradeModal ? (
          <>
            <h1>
              {runtimeUpgradeProgress.toVersion
                ? `Finishing Headroom ${runtimeUpgradeProgress.toVersion} update…`
                : "Finishing Headroom update…"}
            </h1>
            <p className="launcher-install-notice">
              {runtimeUpgradeProgress.message ||
                "Wrapping up the Headroom update."}
            </p>
            <div className="install-progress-shell">
              <div className="install-progress" aria-live="polite">
                <div className="install-progress__bar-track">
                  <div
                    className="install-progress__bar-fill"
                    style={{ width: `${runtimeUpgradeProgress.overallPercent}%` }}
                  />
                </div>
                <div className="install-progress__meta">
                  <p>{runtimeUpgradeProgress.currentStep}</p>
                </div>
              </div>
            </div>
          </>
        ) : upgradeFailure ? (
          <>
            <h1>
              {`Headroom ${upgradeFailure.appVersion} couldn't finish updating`}
            </h1>
            <p className="launcher-install-notice">
              {upgradeFailure.errorHint ??
                (upgradeFailure.fallbackHeadroomVersion
                  ? "Running the previous version while we wait for you to retry."
                  : "Running the previous version.")}
              {upgradeExhausted
                ? " We won't auto-retry on launch — click Retry to try again."
                : ""}
            </p>
            <div className="launcher-install-buttons">
              <button
                type="button"
                className="primary-button primary-button--large"
                onClick={() => void invoke("retry_runtime_upgrade")}
                disabled={runtimeUpgradeProgress.running}
              >
                Retry update
              </button>
              <button
                type="button"
                className="secondary-button"
                disabled={connectorsBusy}
                onClick={() => void handleFirstLaunchContinue()}
              >
                {connectorsBusy
                  ? "Connecting your coding agents…"
                  : "Continue with previous version"}
              </button>
              {upgradeFailure.failurePhase === "boot_validation" && (
                <button
                  type="button"
                  className="secondary-button"
                  onClick={() =>
                    void invoke("retry_runtime_upgrade_with_rebuild")
                  }
                  disabled={runtimeUpgradeProgress.running}
                >
                  Retry with full rebuild
                </button>
              )}
              {upgradeFailure.failurePhase === "boot_validation" && (
                <button
                  type="button"
                  className="secondary-button secondary-button--small"
                  onClick={() =>
                    void invoke("open_external_link", {
                      url: buildUpgradeIssueMailto(upgradeFailure),
                    }).catch(() => {})
                  }
                >
                  Report issue
                </button>
              )}
            </div>
          </>
        ) : null}
      </LauncherShell>
    );
  }

  if (
    windowLabel === "launcher" && launcherStage === "install"
  ) {
    const stepProgress = Math.round(getStepProgress(bootstrapProgress) * 100);
    const renderPercent = animatedOverallPercent(bootstrapProgress);
    const installComplete = bootstrapProgress.complete || dashboard.bootstrapComplete;
    const failedInstallUpdateLabel = appUpdateRestartBusy
      ? "Restarting…"
      : appUpdateInstallBusy
        ? "Installing…"
        : appUpdateBusy
          ? "Checking…"
          : appUpdateReadyToRestart
            ? "Restart now"
            : appUpdateAvailable
              ? `Install ${appUpdateAvailable.version}`
              : "Check for updates";

    const statusCopy = !showInstallProgress
      ? ""
      : bootstrapProgress.failed
        ? // The message renders in full in the error paragraph below; repeating
          // it here printed the same three sentences twice.
          bootstrapProgress.currentStep
        : `${bootstrapProgress.message} ${
            bootstrapProgress.running && !bootstrapProgress.complete
              ? `(${stepProgress}% of this step)`
              : ""
          }`.trim();

    return (
      <LauncherShell
        shellClassName="intro-shell"
        spinnerClassName="intro-shell__spinner"
        copyClassName="intro-shell__copy intro-shell__copy--first-run"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
        showSpinner={bootstrapping}
      >
        <div>
        <h1>
          Headroom cuts AI coding agents&apos; costs
           ~<span className="headline-highlight">50%</span> by trimming prompt bloat.
        </h1>
        <div className="intro-shell__agents" aria-label="Supported coding agents">
          {[
            ["claude_code", "Claude Code"],
            ["codex", "ChatGPT"],
            ["grok_build", "Grok Build"],
            ["opencode", "OpenCode"]
          ].map(([clientId, label]) => (
            <span className="intro-shell__agent" key={clientId}>
              <ConnectorIcon clientId={clientId} size={14} />
              {label}
            </span>
          ))}
        </div>
        </div>
        <div className="intro-shell__checklist">
          <article>
            <strong>Privacy first</strong>
            <p>
              Your prompts never touch our servers. Everything runs locally on your machine.
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
            <strong>Less tokens, no impact</strong>
            <p>
              Smart optimization cuts noise before your coding tool sees it, with
              no impact on the output.
            </p>
          </article>
        </div>
        {installComplete ? (
          <>
            {bootstrapProgress.running ||
            (runtimeStatus?.running !== true && runtimeStatus?.bypassed !== true) ? (
              <>
                <p className="launcher-install-notice">Starting Headroom for the first time (this can take 1-2 minutes)…</p>
                <button
                  className="primary-button primary-button--large primary-button--install launcher-step1-continue"
                  disabled
                  type="button"
                >
                  Starting Headroom…
                </button>
              </>
            ) : (
              <>
                <p className="launcher-install-notice">Headroom installation present</p>
                <button
                  className="primary-button primary-button--large primary-button--success launcher-step1-continue"
                  disabled={connectorsBusy}
                  onClick={() => void handleFirstLaunchContinue()}
                  type="button"
                >
                  {connectorsBusy ? "Connecting your coding agents…" : "Continue"}
                </button>
              </>
            )}
          </>
        ) : (
          <>
            {!bootstrapping && (
              <p className="install-pre-notice">
                Takes 2-3 minutes to install all tools and configure your installed coding
                agents to work with Headroom.
              </p>
            )}
            <button
              className="primary-button primary-button--large primary-button--install"
              disabled={bootstrapping}
              onClick={() => void handleInstallPrimaryAction()}
              type="button"
            >
              {bootstrapping
                ? "Installing Headroom…"
                : bootstrapProgress.failed
                  ? "Try again"
                  : "Install Headroom"}
            </button>
          </>
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
              {bootstrapProgress.failed ? (
                <div className="install-progress__actions">
                  <button
                    className="secondary-button secondary-button--small"
                    disabled={
                      appUpdateBusy || appUpdateInstallBusy || appUpdateRestartBusy
                    }
                    onClick={() => void handleFailedInstallUpdateAction()}
                    type="button"
                  >
                    {failedInstallUpdateLabel}
                  </button>
                  <button
                    className="secondary-button secondary-button--small"
                    onClick={() => void handleFailedInstallSupportMail()}
                    type="button"
                  >
                    Contact support
                  </button>
                </div>
              ) : null}
              {appUpdateStatusCopy && bootstrapProgress.failed ? (
                <p className="install-progress__update-status">{appUpdateStatusCopy}</p>
              ) : null}
            </div>
          ) : null}
        </div>
      </LauncherShell>
    );
  }

  if (
    windowLabel === "launcher" && launcherStage === "client_setup"
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
          <h1>Connect your coding tools</h1>
          <p>Toggle each tool to automatically route it through Headroom.</p>
          <div className="connector-list">
            {availableConnectors.map((connector) => {
              const unavailableReason = getConnectorUnavailableReason(connector);
              const detectionWarning = getConnectorDetectionWarning(connector);
              const supportWarning = getConnectorSupportWarning(connector);
              const statusLine = connectorStatusLine(connector);
              const gateBlocksEnable = connectorGateBlocksEnable(connector);
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
                    {statusLine ? (
                      <p className={`connector-item__${statusLine.tone}`}>{statusLine.text}</p>
                    ) : null}
                    {(detectionWarning ?? unavailableReason) ? (
                      <p className="connector-item__reason">
                        {detectionWarning ?? unavailableReason}
                      </p>
                    ) : null}
                    {gateBlocksEnable ? (
                      <p className="connector-item__reason">
                        {pricingStatus?.gateMessage}{" "}
                        <button
                          className="addon-card__link"
                          type="button"
                          onClick={connectorGateCta}
                        >
                          {pricingStatus?.authenticated ? "Upgrade" : "Sign in"}
                        </button>
                      </p>
                    ) : null}
                  </div>
                  <div className="connector-item__controls">
                    <button
                      aria-checked={connector.enabled}
                      aria-label={`${connector.enabled ? "Disable" : "Enable"} ${connector.name} connector`}
                      className={`connector-switch${connector.enabled ? " is-on" : ""}`}
                      disabled={connectorsBusy || gateBlocksEnable}
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
              <p className="connector-list__section-label">Not detected on this machine</p>
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
          {connectorsNotice ? (
            <p className="install-progress__notice">{connectorsNotice}</p>
          ) : null}
        </div>
        <div className="post-install__actions">
          <button
            className="secondary-button post-install__reopen-setup"
            onClick={() => {
              setLauncherStage("install");
            }}
            type="button"
          >
            Back
          </button>
          <button
            className="primary-button primary-button--large primary-button--success"
            disabled={connectorsBusy || (requireSelection && enabledConnectorCount === 0)}
            onClick={() => {
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
    windowLabel === "launcher" && launcherStage === "proxy_verify"
  ) {
    const hasEnabledApps = proxyVerificationRows.length > 0;
    const allVerified =
      hasEnabledApps &&
      proxyVerificationRows.every((row) => row.state === "verified");
    const anyVerified = proxyVerificationRows.some((row) => row.state === "verified");
    const unverified = proxyVerificationRows.filter((row) => row.state !== "verified");
    const unverifiedNames = formatConnectorNameList(unverified.map((row) => row.name));
    const finishSetup = () => {
      void invoke("complete_setup_wizard");
      setLauncherStage("post_install");
    };

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
            Restart each tool below, then send it any message to test its connection with Headroom. 
            "Say hi" is enough. If restarting doesn't work, contact{" "}
            <button
              className="install-progress__notice-link"
              onClick={() =>
                void invoke("open_external_link", { url: "mailto:support@extraheadroom.com" })}
              type="button"
            >
              support@extraheadroom.com
            </button>{" "}
            for help.
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
              No tools are enabled yet. Go back to the previous step to enable one.
            </p>
          )}
          {proxyVerificationHint ? (
            <p
              className={
                proxyVerificationHint.tone === "error"
                  ? "install-progress__error"
                  : "launcher-restart-hint"
              }
            >
              {proxyVerificationHint.text}
            </p>
          ) : null}
          {!allVerified && proxyVerifySkipArmed ? (
            <p className="install-progress__notice">
              {hasEnabledApps
                ? `We have not detected any ${unverifiedNames} activity flowing through our savings pipeline yet. You can skip and continue for now, but Headroom can only compress requests it sees, so your savings are likely to stay at zero. Restart ${unverifiedNames} to fix this.`
                : "Headroom has nothing to optimize until a coding agent is connected and its requests flow through our savings pipeline. Your savings will stay at zero. You can connect one later from within the app if you prefer."}
              <br />
              <br />
              <strong>Note:</strong> does not work with Claude Desktop app due to
              limitations by Anthropic. You need to use Claude Code{" "}
              <button
                className="install-progress__notice-link"
                onClick={() =>
                  void invoke("open_external_link", {
                    url: "https://code.claude.com/docs/en/quickstart#step-1-install-claude-code"
                  })}
                type="button"
              >
                in the CLI
              </button>{" "}
              or{" "}
              <button
                className="install-progress__notice-link"
                onClick={() =>
                  void invoke("open_external_link", {
                    url: "https://code.claude.com/docs/en/overview#vs-code"
                  })}
                type="button"
              >
                as part of VS Code
              </button>
            </p>
          ) : null}
        </div>
        <div className="post-install__actions">
          <button
            className="secondary-button post-install__reopen-setup"
            onClick={() => {
              setLauncherStage("client_setup");
            }}
            type="button"
          >
            Back
          </button>
          {allVerified ? (
            <button
              className="primary-button primary-button--large primary-button--success"
              onClick={finishSetup}
              type="button"
            >
              Continue
            </button>
          ) : anyVerified ? (
            // At least one connector is proven working, so continuing is a
            // legitimate path - no skip-arming friction needed.
            <button
              className="primary-button primary-button--large"
              onClick={finishSetup}
              type="button"
            >
              Continue
            </button>
          ) : (
            // Deliberately not a primary button: leaving without a single
            // verified connector is the path that ends in "Headroom didn't
            // work", so it should not look like the happy path.
            <button
              className="secondary-button"
              onClick={() => {
                if (proxyVerifySkipArmed) {
                  finishSetup();
                  return;
                }
                setProxyVerifySkipArmed(true);
              }}
              type="button"
            >
              {proxyVerifySkipArmed ? "Skip anyway" : "Skip for now"}
            </button>
          )}
        </div>
      </LauncherShell>
    );
  }

  if (windowLabel === "launcher" && launcherStage === "paywall") {
    // Pre-install there is no proxied traffic, so detection normally yields
    // nothing and the recommendation defaults to Max x5 — this is a self-select
    // screen. Detected tiers still win if they happen to exist.
    const recommendedTier = recommendedHeadroomTier(
      pricingStatus?.claude?.planTier ?? null,
      pricingStatus?.codexPlanTier ?? null,
      "max5x"
    );
    // Fixed cheapest-first order, independent of the upgrade view's
    // recommended-first sorting; mirrors the website pricing page.
    const paywallPlans = (["pro", "max5x", "max20x"] as const)
      .map((id) => upgradePlansState.plans.find((plan) => plan.id === id))
      .filter((plan): plan is UpgradePlan => plan !== undefined);
    const paywallPlanFit: Record<string, string> = {
      pro: "For Claude Pro or ChatGPT Plus",
      max5x: "Claude Max x5 & ChatGPT Pro Lite",
      max20x: "Claude Max x20 & ChatGPT Pro"
    };
    const signedIn = pricingStatus?.authenticated === true;
    return (
      <LauncherShell
        shellClassName="intro-shell intro-shell--paywall"
        spinnerClassName="intro-shell__spinner intro-shell__spinner--post-install"
        copyClassName="intro-shell__copy intro-shell__copy--post-install"
        onMouseDown={handleLauncherSurfaceMouseDown}
        version={appSemver}
      >
        <div className="paywall">
          <h1>Pick your Headroom plan</h1>
          <div className="upgrade-billing-toggle" role="group" aria-label="Billing period">
            {(["monthly", "annual"] as const).map((period) => (
              <button
                key={period}
                className={`upgrade-billing-toggle__item${billingPeriod === period ? " is-active" : ""}`}
                onClick={() => setBillingPeriod(period)}
                type="button"
              >
                {period === "annual" ? (
                  <>Annual <span className="upgrade-billing-toggle__save">Save 25%</span></>
                ) : "Monthly"}
              </button>
            ))}
          </div>
          {pricingStatus?.introOffer?.active ? (
            <p className="paywall__sale-banner">
              🎉 Intro offer: {pricingStatus.introOffer.percentOff}% off your first{" "}
              {pricingStatus.introOffer.durationMonths} months • on every plan
            </p>
          ) : null}
          <p className="paywall__detection">
            Pick the tier that matches your Claude or ChatGPT plan • <strong>7-day free trial</strong>.
          </p>
          {!signedIn ? (
            <AuthCodeForm
              lead="Sign in to subscribe. We'll email you a one-time code."
              email={authEmail}
              onEmailChange={setAuthEmail}
              emailValid={authEmailValid}
              code={authCode}
              onCodeChange={setAuthCode}
              codeRequested={authCodeRequestedFor !== null}
              requestBusy={authRequestBusy}
              verifyBusy={authVerifyBusy}
              error={authFlowError}
              success={authFlowSuccess}
              onRequestCode={() => void handleRequestAuthCode()}
              onVerify={() => void handleVerifyAuthCode()}
            />
          ) : null}
          <div className="paywall__plans">
            {paywallPlans.map((plan) => {
              const isRecommended = plan.id === recommendedTier;
              return (
                <article
                  className={`soft-card paywall-card${isRecommended ? " paywall-card--recommended" : ""}`}
                  key={plan.id}
                >
                  <strong className="paywall-card__name">{plan.name}</strong>
                  <span className="paywall-card__fit">
                    {paywallPlanFit[plan.id] ?? plan.tagline}
                  </span>
                  {plan.originalPrice ? (
                    <span className="paywall-card__sale-row">
                      <s className="upgrade-plan-card__original-price">
                        {plan.originalPrice}
                      </s>
                      <span className="upgrade-plan-card__sale-badge">
                        {introSaleBadgeLabel(pricingStatus?.introOffer) ??
                          `${(pricingStatus?.activePercentOff ?? 0) || 50}% off`}
                      </span>
                    </span>
                  ) : null}
                  <span className="paywall-card__price">{plan.price}</span>
                  <span className="paywall-card__billing">
                    {plan.billingLines.join(" ")}
                  </span>
                  <button
                    className={isRecommended ? "primary-button" : "secondary-button"}
                    disabled={!signedIn || upgradeActionBusy !== null}
                    onClick={() => void handleUpgradeAction(plan.id)}
                    type="button"
                  >
                    {upgradeActionBusy === plan.id
                      ? "Opening checkout…"
                      : `Start ${plan.name} trial`}
                  </button>
                </article>
              );
            })}
          </div>
          {upgradeActionError ? (
            <p className="install-progress__error">{upgradeActionError}</p>
          ) : null}
          <p className="paywall__footnote">
            <button
              className="link-button"
              onClick={() => void invoke("open_external_link", { url: "https://extraheadroom.com/features" })}
              type="button"
            >
              See all Headroom features
            </button>
            {" • "}
            Headroom finalizes installing and starts optimizing right after checkout.
          </p>
          {signedIn ? (
            <p className="paywall__footnote">
              Signed in as {pricingStatus?.account?.email ?? authEmail}
              {" • "}
              <button
                className="link-button"
                onClick={() => void handleSignOutHeadroomAccount()}
                type="button"
              >
                or use a different email
              </button>
            </p>
          ) : null}
        </div>
      </LauncherShell>
    );
  }

  if (
    windowLabel === "launcher" && launcherStage === "post_install"
  ) {
    // The tray's 5s dashboard poll keeps running under the launcher window,
    // so a first-run user who sends a prompt sees this screen flip from
    // "waiting" to their first real savings without any interaction — the
    // payoff moment stays inside onboarding instead of being deferred to a
    // later session that a third of signups never have. While waiting,
    // blur-autohide is disarmed (see awaitingFirstSavings above).
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
          {awaitingFirstSavings ? (
            <FirstSavingsChecklist
              dashboard={dashboard}
              onReopenSetup={() => setLauncherStage("client_setup")}
            />
          ) : (
            <>
              <p>
                {dashboard.launchExperience === "first_run"
                  ? "That prompt went through Headroom — your first savings are in."
                  : "It will trim prompt bloat whenever you use a connected coding agent."}
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
              void beginProxyVerificationStep();
            }}
            type="button"
          >
            Back
          </button>
          <button
            className="primary-button primary-button--large primary-button--success"
            // `complete_setup_wizard` already fired on entering this screen
            // (see the isLastScreen effect), so this is only the exit.
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

  // Cold-cache warmup: proxy is up and the ML extras are installed, but the
  // ~260MB Kompress model hasn't loaded yet (it downloads lazily on first use,
  // and the desktop prefetches it in the background after a fresh install).
  // This is normal setup, not a fault, so it must not surface as an issue.
  const kompressWarming = Boolean(
    runtimeStatus &&
      runtimeStatus.running &&
      runtimeStatus.proxyReachable &&
      runtimeStatus.mlInstalled !== false &&
      runtimeStatus.kompressEnabled === false
  );

  const runtimeIssues: string[] = [];
  if (runtimeStatus?.installed === false) {
    runtimeIssues.push("runtime not installed");
  }
  if (runtimeStatus?.running === false) {
    runtimeIssues.push(
      runtimeStatus.startupErrorHint ??
        runtimeStatus.startupError ??
        "runtime offline"
    );
  }
  if (runtimeStatus?.proxyReachable === false) {
    runtimeIssues.push("proxy unreachable");
  }
  if (runtimeStatus?.mcpConfigured === false) {
    runtimeIssues.push("MCP not configured");
  }
  if (runtimeStatus?.kompressEnabled === false && !kompressWarming) {
    runtimeIssues.push("Kompress disabled");
  }

  const runtimeHealthy = Boolean(
    runtimeStatus &&
      runtimeStatus.running &&
      runtimeStatus.proxyReachable &&
      runtimeStatus.mcpConfigured !== false &&
      (runtimeStatus.kompressEnabled !== false || kompressWarming)
  );
  const platformPreviewNotice = platformPreviewNoticeFor(
    runtimeStatus?.platform,
    runtimeStatus?.supportTier,
  );
  const headroomLearnSupported = runtimeStatus?.headroomLearnSupported !== false;
  const headroomLearnDisabledReason =
    runtimeStatus?.headroomLearnDisabledReason ??
    "Headroom Learn is unavailable on this platform.";

  const calloutBanner = (() => {
    if (!runtimeStatus) {
      return {
        tone: "disconnected",
        title: "Headroom status is unavailable."
      } as const;
    }

    if (runtimeStatus.paused) {
      if (runtimeStatus.autoPaused) {
        return {
          tone: "auto-paused",
          title: "Headroom stopped unexpectedly. Traffic is passing through unoptimized."
        } as const;
      }
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

    // Codex-only gate: surface in the top banner only when the Claude side isn't
    // itself gating/nudging (handled above), so mixed users never get a double
    // banner. Codex billing/pausing is scoped to Codex traffic.
    const codexUsage = pricingStatus?.codex;
    if (codexUsage && codexUsage.optimizationAllowed === false) {
      return {
        tone: "disabled",
        title: codexUsage.gateMessage
      } as const;
    }
    if (codexUsage?.shouldNudge) {
      return {
        tone: "starting",
        title: codexUsage.gateMessage
      } as const;
    }

    if (runtimeHealthy) {
      if (connectorPhase === "disabled") {
        return {
          tone: "disabled",
          title: "No coding tools connected — Headroom isn't reducing costs."
        } as const;
      }
      if (connectorPhase === "verifying") {
        return {
          tone: "starting",
          title: "Send a message in a connected tool to verify the connection is working. You may need to restart it first."
        } as const;
      }
      if (kompressWarming) {
        return {
          tone: "healthy",
          title: "Headroom is running while finishing setup."
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
  const tierMismatch = pricingStatus?.tierMismatch ?? null;
  // Products the clamp actually limits (per-product scope). Falls back to the
  // recommendation source for payloads cached by builds without the flags.
  const clampScopeLabel = tierMismatch
    ? [
        tierMismatch.claudeUndercovered ? "Claude" : null,
        tierMismatch.codexUndercovered ? "ChatGPT" : null,
      ]
        .filter(Boolean)
        .join(" and ") || tierRecommendationSourceLabel(tierMismatch.recommendedSource)
    : "";
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
  const upgradeDefaultPlanId =
    pricingAudience === "individual"
      ? (pricingStatus?.recommendedSubscriptionTier ??
          pricingStatus?.codex?.recommendedSubscriptionTier ??
          cachedPricing.recommendedSubscriptionTier ??
          upgradePlansState.featuredPlanId)
      : "enterprise";
  const upgradeDefaultPlan = upgradePlansState.plans.find((plan) => plan.id === upgradeDefaultPlanId) ?? null;

  // Upgrade-ask copy anchored on the user's own savings (items 1 & 2). Shown
  // only at a gate/nudge moment, and only when there's enough realized savings
  // for the numbers to land (helpers return null otherwise).
  const recentDailySavings = recentDailySavingsUsd(dashboard.dailySavings);
  const inUpgradeMoment =
    !!pricingStatus &&
    !pricingStatus.needsAuthentication &&
    !pricingStatus.account?.subscriptionActive &&
    (!pricingStatus.optimizationAllowed ||
      pricingStatus.shouldNudge ||
      pricingStatus.codex?.optimizationAllowed === false ||
      !!pricingStatus.codex?.shouldNudge);
  const paybackPlanId =
    upgradeDefaultPlanId === "pro" ||
    upgradeDefaultPlanId === "max5x" ||
    upgradeDefaultPlanId === "max20x"
      ? upgradeDefaultPlanId
      : null;
  // Item 1 - "pays for itself" anchor (recent monthly savings rate vs price).
  const upgradePaybackLabel =
    inUpgradeMoment && paybackPlanId
      ? paybackLabel(recentDailySavings * 30, paybackPlanId, billingPeriod)
      : null;
  // Item 2 - forgone-savings counterfactual until the active weekly limit resets.
  const weeklyGateForgoneLabel = (() => {
    if (!inUpgradeMoment || !pricingStatus) return null;
    const claudeWeeklyActive =
      !pricingStatus.optimizationAllowed || pricingStatus.shouldNudge;
    if (claudeWeeklyActive && pricingStatus.claude.weeklyResetsAt) {
      const days =
        (new Date(pricingStatus.claude.weeklyResetsAt).getTime() - Date.now()) / 86_400_000;
      return forgoneSavingsLabel(recentDailySavings, days);
    }
    // The metered window, not `secondary` — Plus reports its weekly window as
    // `primary`, which left this null and killed the Codex forgone-savings line.
    const codexResetSecs = pricingStatus.codex?.weeklyResetsInSeconds ?? null;
    if (codexResetSecs && codexResetSecs > 0) {
      return forgoneSavingsLabel(recentDailySavings, codexResetSecs / 86_400);
    }
    return null;
  })();
  // Show a single, strongest savings line: at a hard gate (optimization paused,
  // pain is live) lead with the forgone-savings loss; at a nudge lead with the
  // "pays for itself" gain. Fall back to the other only if the primary is null.
  const isHardGate =
    !!pricingStatus &&
    (!pricingStatus.optimizationAllowed || pricingStatus.codex?.optimizationAllowed === false);
  const upgradeSavingsLine = isHardGate
    ? (weeklyGateForgoneLabel ?? upgradePaybackLabel)
    : (upgradePaybackLabel ?? weeklyGateForgoneLabel);
  // Only show it when the pricing gate/nudge banner actually wins: a startup,
  // paused, or disconnected banner takes precedence over the upsell, so the
  // savings line must not leak under those titles.
  const showUpgradeSavingsLine =
    !!upgradeSavingsLine &&
    !!runtimeStatus &&
    !runtimeStatus.paused &&
    !runtimeStatus.starting;
  // Intro-offer nudge for unsubscribed users: surface the offer alongside the
  // savings line so the upgrade moment carries the same pitch as the pricing
  // page.
  const launchPromoLine = (() => {
    if (!inUpgradeMoment || !pricingStatus?.introOffer?.active) return null;
    const { percentOff, durationMonths } = pricingStatus.introOffer;
    return `Intro offer: ${percentOff}% off your first ${durationMonths} months.`;
  })();
  // When the banner is carrying an upgrade nudge (and isn't showing the Resume
  // control), let clicking the card jump straight to the upgrade view.
  const calloutIsUpgradeNudge =
    inUpgradeMoment &&
    !!runtimeStatus &&
    !runtimeStatus.paused &&
    !runtimeStatus.starting &&
    (showUpgradeSavingsLine || !!launchPromoLine);

  const activeHeadroomPlanId =
    pricingAudience === "individual" && pricingStatus?.account?.subscriptionActive
      ? pricingStatus.account.subscriptionTier ?? null
      : null;
  // Plan cards keep their slot on both tabs, but the "Active" chrome only
  // belongs to the period the subscriber actually pays for.
  const viewingSubscribedPeriod = matchesSubscriptionPeriod(
    billingPeriod,
    pricingStatus?.account?.subscriptionBillingPeriod
  );
  // A downgrade waits for the end of the term already paid for, so between
  // confirming it and it landing the subscription still reports the old plan.
  // Without this the change leaves no trace anywhere in the app.
  const pendingPlanChangeInfo = scheduledPlanChange(pricingStatus?.account);
  // The card beside the active plan: the nearest step up, since that is what
  // this view is for. Only the top tier has none, and there the tier below it
  // is the sole remaining move.
  const companionPlanId =
    getNextHigherUpgradePlanId(activeHeadroomPlanId) ?? getNextLowerUpgradePlanId(activeHeadroomPlanId);
  const visibleUpgradePlans = (() => {
    if (showAllUpgradePlans || upgradePlansState.plans.length <= 2) {
      return upgradePlansState.plans;
    }

    if (pricingAudience === "individual" && activeHeadroomPlanId && companionPlanId) {
      const visiblePlanIds = new Set<UpgradePlanId>([activeHeadroomPlanId, companionPlanId]);
      const activeWindowPlans = upgradePlansState.plans.filter((plan) => visiblePlanIds.has(plan.id));
      if (activeWindowPlans.length === 2) {
        return activeWindowPlans;
      }
    }

    // Lead with the plan matched to the user's Claude/ChatGPT tier; the
    // "show more plans" button reveals the rest.
    return upgradePlansState.plans.slice(0, 1);
  })();
  const hasHiddenUpgradePlans = visibleUpgradePlans.length < upgradePlansState.plans.length;
  const pendingUpgradePlanLabel = upgradePlanIntentLabel(pendingUpgradePlanId);
  const upgradeAuthMessage = pendingUpgradePlanLabel
    ? `Sign in with email to upgrade to the ${pendingUpgradePlanLabel} plan`
    : "Sign in with email";
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
      return "7-day trial";
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
          message: "Your 72-hour Headroom access expired. Create an account to extend to 7 days.",
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
        message: `${hoursLabel} left in your 72-hour trial. Create an account to extend trial to 7 days.`,
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
          : "7 days";
      return {
        tone: "warning" as const,
        message: `${daysLabel} of trial to go. Upgrade to continue using Headroom without limits.`,
        actionLabel: "Upgrade",
        onAction: () => void handleUpgradeAction(upgradeDefaultPlanId)
      };
    }
    return {
      tone: "expired" as const,
      message:
        "Your 7-day trial has ended. Upgrade to keep Headroom optimizing your prompts.",
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
                  placeholder="you@example.com"
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
          <p className="pricing-auth-card__legal">
            {"By signing in, you agree to our "}
            <button className="link-button" onClick={() => void invoke("open_external_link", { url: "https://extraheadroom.com/terms" })} type="button">Terms of Service</button>
            {" and "}
            <button className="link-button" onClick={() => void invoke("open_external_link", { url: "https://extraheadroom.com/privacy" })} type="button">Privacy Policy</button>
            {"."}
          </p>
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
      {upgradeOverlay}
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
            {tierMismatch ? (
              <section className="tier-mismatch-banner" role="alert">
                <div className="tier-mismatch-banner__body">
                  <h2 className="tier-mismatch-banner__title">Upgrade your Headroom plan</h2>
                  <p className="tier-mismatch-banner__message">
                    {tierMismatch.clamped
                      ? `Your ${tierRecommendationSourceLabel(tierMismatch.recommendedSource)} usage needs the Headroom ${upgradePlanIntentLabel(tierMismatch.recommendedTier)} plan, above your current Headroom ${upgradePlanIntentLabel(tierMismatch.paidTier)} plan, so weekly usage limits now apply to ${clampScopeLabel}. Upgrade${pricingStatus?.account?.upgradeAction === "appsumo" ? " on AppSumo" : ""} to restore unlimited optimization.`
                      : `You're on the Headroom ${upgradePlanIntentLabel(tierMismatch.paidTier)} plan but your ${tierRecommendationSourceLabel(tierMismatch.recommendedSource)} usage needs the Headroom ${upgradePlanIntentLabel(tierMismatch.recommendedTier)} plan. Upgrade${pricingStatus?.account?.upgradeAction === "appsumo" ? " on AppSumo" : ""} to match.`}
                  </p>
                  {upgradeActionError && upgradeActionBusy === null ? (
                    <p className="tier-mismatch-banner__error" role="status">
                      {upgradeActionError}
                    </p>
                  ) : null}
                </div>
                <button
                  type="button"
                  className="tier-mismatch-banner__action"
                  disabled={upgradeActionBusy === tierMismatch.recommendedTier}
                  onClick={() => void handleUpgradeAction(tierMismatch.recommendedTier)}
                >
                  {upgradeActionBusy === tierMismatch.recommendedTier
                    ? "Updating…"
                    : `Upgrade to ${upgradePlanIntentLabel(tierMismatch.recommendedTier)}`}
                </button>
              </section>
            ) : null}
            <section
              className={`callout-banner callout-banner--${calloutBanner.tone}${
                calloutIsUpgradeNudge ? " callout-banner--clickable" : ""
              }`}
              {...(calloutIsUpgradeNudge
                ? {
                    role: "button",
                    tabIndex: 0,
                    onClick: () => setActiveView("upgrade"),
                    onKeyDown: (e: React.KeyboardEvent) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        setActiveView("upgrade");
                      }
                    },
                  }
                : {})}
            >
              <span className={`callout-banner__dot callout-banner__dot--${calloutBanner.tone}`} aria-hidden="true" />
              <div className="callout-banner__body">
                <h1>{calloutTitle}</h1>
                {platformPreviewNotice ? (
                  <p className="callout-banner__subtitle">
                    {platformPreviewNotice} Please report any issues to{" "}
                    <button
                      type="button"
                      className="link-button"
                      onClick={() =>
                        void invoke("open_external_link", {
                          url: platformPreviewSupportMailto({
                            platform: runtimeStatus?.platform,
                            appVersion: appSemver,
                            headroomVersion,
                          }),
                        }).catch(() => {})
                      }
                    >
                      {PREVIEW_SUPPORT_EMAIL}
                    </button>
                    .
                  </p>
                ) : null}
                {showUpgradeSavingsLine ? (
                  <p className="callout-banner__subtitle">{upgradeSavingsLine}</p>
                ) : null}
                {calloutBanner.tone === "healthy" && dashboard.lifetimeEstimatedTokensSaved < 1_000_000 && (
                  stallBannerLine ? (
                    // Nothing has ever been saved on this install, so the
                    // reassuring "check back later" below would be a lie.
                    <p className="callout-banner__subtitle">{stallBannerLine}</p>
                  ) : (
                    <p className="callout-banner__subtitle">Use your AI coding agents as normal, and check back later to see what Headroom is saving you.</p>
                  )
                )}
                {(calloutBanner.tone === "auto-paused" || calloutBanner.tone === "paused") && (
                  <div className="callout-banner__resume">
                    <button
                      type="button"
                      className="callout-banner__action"
                      onClick={() => void handleResumeRuntime()}
                      disabled={resuming}
                    >
                      {resuming ? "Restarting…" : "Resume"}
                    </button>
                    {resumeError ? (
                      <p className="callout-banner__subtitle callout-banner__error" role="status">
                        {resumeError}
                      </p>
                    ) : null}
                  </div>
                )}
              </div>
              {(() => {
                const homeConnectors = sortClientConnectors(aggregateClientConnectors(connectors))
                  .filter((connector) => connector.installed || connector.enabled);
                if (homeConnectors.length === 0) {
                  return null;
                }
                return (
                  <div className="callout-banner__connectors">
                    {homeConnectors.map((connector) => {
                      const status = connectorDashboardStatus(connector, {
                        proxyReachable: runtimeStatus?.proxyReachable
                      });
                      return (
                        <span
                          className={`callout-banner__badge callout-banner__badge--${status.tone}`}
                          key={connector.clientId}
                          data-tip={`${connector.name}: ${status.label}`}
                        >
                          {hasConnectorIcon(connector.clientId) ? (
                            <ConnectorIcon clientId={connector.clientId} />
                          ) : (
                            <span aria-hidden="true">
                              {connectorMonograms[connector.clientId] ?? connector.name.slice(0, 2)}
                            </span>
                          )}
                          <span className="visually-hidden">{`${connector.name}: ${status.label}`}</span>
                        </span>
                      );
                    })}
                  </div>
                );
              })()}
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
                  Total costs saved
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
                  Total input tokens saved
                  <button
                    className="stat-card__info-button"
                    onClick={(e) => { e.stopPropagation(); setShowCacheInfo(true); }}
                    type="button"
                    aria-label="Cache hits and compression by period"
                  >
                    <Info size={13} weight="bold" />
                  </button>
                </span>
                <div className="stat-value-row">
                  <strong className="stat-value--blue">
                    {compactNumber(dashboard.lifetimeEstimatedTokensSaved)}
                  </strong>
                </div>
              </article>
            </section>

            {/* The 20s timeout is an escape hatch for a backend that is not
                coming up (paused, failed) - while it is actively STARTING,
                keep the skeleton instead of flashing the tracker-only
                archive chart, whose today figure reads far below the real
                one until /stats-history loads (rc.5 Windows startup showed
                $0.26 against a real $1). */}
            {dashboard.savingsHistoryLoaded ||
            (historyLoadTimedOut && !runtimeStatus?.starting) ? (
              <DailySavingsChart
                data={dashboard.dailySavings}
                hourlyData={dashboard.hourlySavings}
                resetSignal={chartResetSignal}
                chartMode={chartMode}
                setChartMode={setChartMode}
                outputReduction={dashboard.outputReduction}
              />
            ) : (
              <div className="savings-chart__skeleton" role="status">
                <p className="loading-copy">Loading savings history…</p>
              </div>
            )}

          </div>

        <div className="tray-content" hidden={activeView !== "optimization"}>
            <article className="soft-card optimize-card">
              <header className="optimize-card__head">
                <div className="optimize-card__title-row">
                  <span className="optimize-card__title-icon" aria-hidden="true">
                    <Brain weight="duotone" />
                  </span>
                  <h1>Project learnings</h1>
                </div>
                <p className="optimize-card__blurb">{learnBlurb}</p>
                {headroomLearnSupported ? (
                  <div className="optimize-card__auto-learn">
                    <div className="optimize-card__auto-learn-text">
                      <span className="optimize-card__auto-learn-label">
                        Auto-learning
                      </span>
                      <span className="optimize-card__auto-learn-meta">
                        {autoLearnEnabled === false
                          ? "Off — only manual scans add learnings."
                          : autoLearnMeta}
                      </span>
                    </div>
                    <button
                      aria-checked={autoLearnEnabled ?? true}
                      aria-label={`${autoLearnEnabled === false ? "Enable" : "Disable"} auto-learning`}
                      className={`connector-switch${autoLearnEnabled === false ? "" : " is-on"}`}
                      disabled={autoLearnEnabled === null || autoLearnBusy}
                      onClick={() =>
                        void handleAutoLearnToggle(autoLearnEnabled === false)
                      }
                      role="switch"
                      type="button"
                    >
                      <span className="connector-switch__thumb" />
                    </button>
                  </div>
                ) : null}
              </header>
              <div className="optimize-card__body">
                {!headroomLearnSupported ? (
                  <div className="optimize-minimal">
                    <p className="optimize-minimal__meta">
                      {headroomLearnDisabledReason}
                    </p>
                  </div>
                ) : !claudeLearnEnabled && !codexLearnEnabled ? (
                  <p className="loading-copy">
                    Enable a coding-agent connector to scan sessions for learnings.
                  </p>
                ) : (
                  <div className="optimize-minimal">
                    {claudeLearnEnabled && claudeProjectsBusy && claudeProjects.length === 0 ? (
                      <p className="loading-copy">Loading projects…</p>
                    ) : claudeLearnEnabled && claudeProjects.length === 0 ? (
                      <p className="loading-copy">No Claude Code projects found in <code>~/.claude/projects</code>.</p>
                    ) : claudeLearnEnabled ? (
                      <>
                    {!headroomLearnPrereq.claudeCliAvailable ? (
                      <div className="install-prompt" role="status">
                        <header className="install-prompt__head">
                          <span className="install-prompt__icon" aria-hidden="true">
                            <Terminal weight="duotone" />
                          </span>
                          <div className="install-prompt__head-text">
                            <h2 className="install-prompt__title">
                              Install the Claude Code CLI
                            </h2>
                            <p className="install-prompt__body">
                              Headroom Learn uses the <code>claude</code> CLI to analyze
                              your sessions.
                            </p>
                          </div>
                        </header>
                        <div className="install-prompt__cmd">
                          <code className="install-prompt__cmd-text">
                            {CLAUDE_CODE_INSTALL_CURL_CMD}
                          </code>
                          <button
                            className="install-prompt__cmd-copy"
                            type="button"
                            onClick={() => void copyLearnInstallCommand(CLAUDE_CODE_INSTALL_CURL_CMD)}
                          >
                            Copy
                          </button>
                        </div>
                        <div className="install-prompt__foot">
                          <button
                            className="install-prompt__link"
                            type="button"
                            onClick={() => void openLearnInstallDocsLink()}
                          >
                            Open install docs
                          </button>
                          <span className="install-prompt__foot-sep" aria-hidden="true">·</span>
                          <button
                            className="install-prompt__link install-prompt__link--recheck"
                            type="button"
                            onClick={() => void refreshHeadroomLearnPrereq(true)}
                          >
                            <ArrowClockwise weight="bold" size={12} aria-hidden="true" />
                            Re-check
                          </button>
                          {learnInstallCopyNotice ? (
                            <span className="install-prompt__notice">
                              {learnInstallCopyNotice}
                            </span>
                          ) : null}
                        </div>
                      </div>
                    ) : null}
                    <div className="optimize-projects">
                      {visibleClaudeProjects.map((project) => {
                        const isRunning =
                          headroomLearnStatus.running &&
                          headroomLearnStatus.projectPath === project.projectPath;
                        const isLatestLearnProject =
                          headroomLearnStatus.projectPath === project.projectPath;
                        const disableLearn =
                          !headroomLearnPrereq.claudeCliAvailable ||
                          headroomLearnBusy ||
                          claudeProjectsBusy ||
                          (headroomLearnStatus.running && !isRunning);
                        const learnMeta = formatLearnStatus(project);
                        const refreshLabel = isRunning
                          ? "Scanning…"
                          : "Scan now";
                        const showInlineResult =
                          isLatestLearnProject &&
                          !headroomLearnStatus.running &&
                          (headroomLearnStatus.success === false ||
                            Boolean(headroomLearnStatus.error));
                        return (
                          <div
                            className={`optimize-project-row${isRunning || showInlineResult ? " optimize-project-row--active" : ""}`}
                            key={project.id}
                          >
                            <div className="optimize-project-row__main">
                              <span className="optimize-project-row__name">
                                <strong>{project.displayName}</strong>
                                <small>
                                  <span className="optimize-project-row__training" aria-live="polite">
                                    {isRunning ? (
                                      <LearnScanStatusLine
                                        step={headroomLearnStatus.currentStep}
                                        elapsedSeconds={headroomLearnStatus.elapsedSeconds}
                                      />
                                    ) : (
                                      learnMeta
                                    )}
                                    <button
                                      type="button"
                                      className={`optimize-project-row__refresh${isRunning ? " is-spinning" : ""}`}
                                      onClick={() => void handleRunHeadroomLearn("claude", project.projectPath)}
                                      disabled={disableLearn}
                                      aria-label={refreshLabel}
                                      title={refreshLabel}
                                    >
                                      <ArrowClockwise weight="bold" size={12} aria-hidden="true" />
                                    </button>
                                  </span>
                                  {/* Hidden during this project's scan so the status line
                                      keeps the row to itself; remounts with fresh counts
                                      when the run finishes. */}
                                  {!isRunning ? (
                                    <OptimizePanel
                                      projectPath={project.projectPath}
                                      neverScanned={hasNeverScanned(project)}
                                      refreshSignal={
                                        isLatestLearnProject && !headroomLearnStatus.running
                                          ? Date.parse(headroomLearnStatus.finishedAt ?? "") || 0
                                          : 0
                                      }
                                      preloadedApplied={
                                        optimizeAppliedByProject
                                          ? optimizeAppliedByProject[project.projectPath] ?? {
                                              claudeMd: [],
                                              memoryMd: [],
                                            }
                                          : undefined
                                      }
                                      onAppliedMutated={() =>
                                        setOptimizeAppliedRefreshTick((tick) => tick + 1)
                                      }
                                    />
                                  ) : null}
                                </small>
                              </span>
                              <div className="optimize-project-row__actions">
                                {showInlineResult ? (
                                  <span className="optimize-project-row__status optimize-minimal__result--failure">
                                    Last run failed
                                  </span>
                                ) : null}
                              </div>
                            </div>
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
                      </>
                    ) : null}
                    {codexLearnEnabled
                      ? (() => {
                          const codexReady =
                            headroomLearnPrereq.codexCliAvailable &&
                            headroomLearnPrereq.codexLoggedIn;
                          const codexRunning =
                            headroomLearnStatus.running &&
                            headroomLearnStatus.projectPath === "codex";
                          const codexIsLatest = headroomLearnStatus.projectPath === "codex";
                          const codexDisable =
                            !codexReady ||
                            headroomLearnBusy ||
                            (headroomLearnStatus.running && !codexRunning);
                          const codexShowResult =
                            codexIsLatest &&
                            !headroomLearnStatus.running &&
                            (headroomLearnStatus.success === false ||
                              Boolean(headroomLearnStatus.error));
                          if (!codexReady) {
                            const codexCmd = headroomLearnPrereq.codexCliAvailable
                              ? CODEX_CLI_LOGIN_CMD
                              : CODEX_CLI_INSTALL_CMD;
                            return (
                              <div className="install-prompt" role="status">
                                <header className="install-prompt__head">
                                  <span className="install-prompt__icon" aria-hidden="true">
                                    <Terminal weight="duotone" />
                                  </span>
                                  <div className="install-prompt__head-text">
                                    <h2 className="install-prompt__title">
                                      {headroomLearnPrereq.codexCliAvailable
                                        ? "Sign in to the Codex CLI"
                                        : "Install the Codex CLI"}
                                    </h2>
                                    <p className="install-prompt__body">
                                      Headroom Learn analyzes your Codex sessions with the{" "}
                                      <code>codex</code> CLI on your ChatGPT subscription.
                                      {headroomLearnPrereq.codexCliAvailable
                                        ? " Sign in to continue."
                                        : ""}
                                    </p>
                                  </div>
                                </header>
                                <div className="install-prompt__cmd">
                                  <code className="install-prompt__cmd-text">{codexCmd}</code>
                                  <button
                                    className="install-prompt__cmd-copy"
                                    type="button"
                                    onClick={() => void copyLearnInstallCommand(codexCmd)}
                                  >
                                    Copy
                                  </button>
                                </div>
                                <div className="install-prompt__foot">
                                  <button
                                    className="install-prompt__link"
                                    type="button"
                                    onClick={() => void openExternalLink(CODEX_INSTALL_DOCS_URL)}
                                  >
                                    Open install docs
                                  </button>
                                  <span className="install-prompt__foot-sep" aria-hidden="true">
                                    ·
                                  </span>
                                  <button
                                    className="install-prompt__link install-prompt__link--recheck"
                                    type="button"
                                    onClick={() => void refreshHeadroomLearnPrereq(true)}
                                  >
                                    <ArrowClockwise weight="bold" size={12} aria-hidden="true" />
                                    Re-check
                                  </button>
                                  {learnInstallCopyNotice ? (
                                    <span className="install-prompt__notice">
                                      {learnInstallCopyNotice}
                                    </span>
                                  ) : null}
                                </div>
                              </div>
                            );
                          }
                          return (
                            <div className="optimize-projects">
                              <div
                                className={`optimize-project-row${codexRunning || codexShowResult ? " optimize-project-row--active" : ""}`}
                              >
                                <div className="optimize-project-row__main">
                                  <span className="optimize-project-row__name">
                                    <strong>ChatGPT sessions</strong>
                                    <small>
                                      <span
                                        className="optimize-project-row__training"
                                        aria-live="polite"
                                      >
                                        {codexRunning ? (
                                          <LearnScanStatusLine
                                            step={headroomLearnStatus.currentStep}
                                            elapsedSeconds={headroomLearnStatus.elapsedSeconds}
                                          />
                                        ) : (
                                          "Scans ~/.codex/sessions into AGENTS.md"
                                        )}
                                        <button
                                          type="button"
                                          className={`optimize-project-row__refresh${codexRunning ? " is-spinning" : ""}`}
                                          onClick={() => void handleRunHeadroomLearn("codex")}
                                          disabled={codexDisable}
                                          aria-label={codexRunning ? "Scanning…" : "Scan now"}
                                          title={codexRunning ? "Scanning…" : "Scan now"}
                                        >
                                          <ArrowClockwise
                                            weight="bold"
                                            size={12}
                                            aria-hidden="true"
                                          />
                                        </button>
                                      </span>
                                    </small>
                                  </span>
                                  <div className="optimize-project-row__actions">
                                    {codexShowResult ? (
                                      <span className="optimize-project-row__status optimize-minimal__result--failure">
                                        Last run failed
                                      </span>
                                    ) : null}
                                  </div>
                                </div>
                                {codexShowResult && headroomLearnStatus.error ? (
                                  <div className="optimize-project-row__result">
                                    <p className="install-progress__error">
                                      {headroomLearnStatus.error}
                                    </p>
                                  </div>
                                ) : null}
                              </div>
                            </div>
                          );
                        })()
                      : null}
                    {[
                      {
                        key: "opencode" as const,
                        enabled: opencodeLearnEnabled,
                        title: "OpenCode sessions",
                        subtitle: "Scans OpenCode's session database into agent memory"
                      },
                      {
                        key: "grok" as const,
                        enabled: grokLearnEnabled,
                        title: "Grok sessions",
                        subtitle: "Scans ~/.grok/sessions into agent memory"
                      }
                    ].map((row) => {
                      if (!row.enabled) {
                        return null;
                      }
                      const ready =
                        headroomLearnPrereq.claudeCliAvailable ||
                        (headroomLearnPrereq.codexCliAvailable &&
                          headroomLearnPrereq.codexLoggedIn);
                      const running =
                        headroomLearnStatus.running &&
                        headroomLearnStatus.projectPath === row.key;
                      const isLatest = headroomLearnStatus.projectPath === row.key;
                      const disable =
                        !ready || headroomLearnBusy || (headroomLearnStatus.running && !running);
                      const showResult =
                        isLatest &&
                        !headroomLearnStatus.running &&
                        (headroomLearnStatus.success === false ||
                          Boolean(headroomLearnStatus.error));
                      return (
                        <div className="optimize-projects" key={row.key}>
                          <div
                            className={`optimize-project-row${running || showResult ? " optimize-project-row--active" : ""}`}
                          >
                            <div className="optimize-project-row__main">
                              <span className="optimize-project-row__name">
                                <strong>{row.title}</strong>
                                <small>
                                  <span
                                    className="optimize-project-row__training"
                                    aria-live="polite"
                                  >
                                    {running ? (
                                      <LearnScanStatusLine
                                        step={headroomLearnStatus.currentStep}
                                        elapsedSeconds={headroomLearnStatus.elapsedSeconds}
                                      />
                                    ) : ready
                                      ? row.subtitle
                                      : "Needs the Claude Code or a signed-in Codex CLI for analysis"}
                                    <button
                                      type="button"
                                      className={`optimize-project-row__refresh${running ? " is-spinning" : ""}`}
                                      onClick={() => void handleRunHeadroomLearn(row.key)}
                                      disabled={disable}
                                      aria-label={running ? "Scanning…" : "Scan now"}
                                      title={running ? "Scanning…" : "Scan now"}
                                    >
                                      <ArrowClockwise weight="bold" size={12} aria-hidden="true" />
                                    </button>
                                  </span>
                                </small>
                              </span>
                              <div className="optimize-project-row__actions">
                                {showResult ? (
                                  <span className="optimize-project-row__status optimize-minimal__result--failure">
                                    Last run failed
                                  </span>
                                ) : null}
                              </div>
                            </div>
                            {showResult && headroomLearnStatus.error ? (
                              <div className="optimize-project-row__result">
                                <p className="install-progress__error">
                                  {headroomLearnStatus.error}
                                </p>
                              </div>
                            ) : null}
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
                {claudeProjectsError ? (
                  <p className="install-progress__error">{claudeProjectsError}</p>
                ) : null}
                {headroomLearnStatus.error &&
                !["codex", "opencode", "grok"].includes(headroomLearnStatus.projectPath ?? "") &&
                !claudeProjects.some((project) => project.projectPath === headroomLearnStatus.projectPath) ? (
                  <p className="install-progress__error">{headroomLearnStatus.error}</p>
                ) : null}
              </div>
            </article>

          </div>

        <div className="tray-content" hidden={activeView !== "notifications"}>
          <ActivityFeed
            feed={activityFeed}
            error={activityFeedError}
            loaded={activityFeedLoaded}
            onNavigateToOptimize={() => setActiveView("optimization")}
            rtkInstalled={runtimeStatus?.rtk.installed === true}
            serenaInstalled={dashboard.tools.some(
              (tool) => tool.id === "serena" && tool.status !== "not_installed"
            )}
          />
        </div>

        <div className="tray-content" hidden={activeView !== "addons"}>
          <article className="soft-card addons-card">
            <header className="addons-card__head">
              <div className="addons-card__title-row">
                <span className="addons-card__title-icon" aria-hidden="true">
                  <PuzzlePiece weight="duotone" />
                </span>
                <h1>Addons</h1>
              </div>
              <p className="addons-card__blurb">
                Additional tools that reduce token usage. Missing an addon you
                want?{" "}
                <button
                  type="button"
                  className="addon-card__link"
                  onClick={() => void openExternalLink(buildAddonRequestMailto())}
                >
                  Request an addon
                </button>
              </p>
            </header>
          </article>
          {addonError ? <p className="addons__error">{addonError}</p> : null}
          <ul className="addons__list">
              {dashboard.tools
                .filter((tool) => !tool.required && tool.id !== "rtk")
                .sort((a, b) => addonDisplayRank(a.id) - addonDisplayRank(b.id))
                .map((tool) => {
                  const installed = tool.status !== "not_installed";
                  return (
                    <AddonCard
                      key={tool.id}
                      name={tool.name}
                      version={tool.version}
                      installed={installed}
                      enabled={tool.enabled}
                      description={tool.description}
                      copy={addonCopy[tool.id]}
                      infoOpen={addonInfoId === tool.id}
                      onToggleInfo={() =>
                        setAddonInfoId(addonInfoId === tool.id ? null : tool.id)
                      }
                      busy={addonBusyId === tool.id}
                      busyLabel={addonBusyLabel}
                      resultMessage={
                        addonResult?.id === tool.id ? addonResult.message : null
                      }
                      onDismissResult={() => setAddonResult(null)}
                      sourceUrl={tool.sourceUrl}
                      onOpenSource={() => void openExternalLink(tool.sourceUrl)}
                      connectors={connectors}
                      showClients={installed && tool.enabled}
                      savings={tool.savingsLabel ?? null}
                      actionsDisabled={addonBusyId === tool.id}
                      updateAvailable={tool.updateAvailable ?? false}
                      availableVersion={tool.availableVersion ?? null}
                      unavailableReason={tool.unavailableReason ?? null}
                      onUpdate={() =>
                        void runAddonAction("install_addon", tool.id, undefined, {
                          busy: `Updating ${tool.name}...`,
                          done: `${tool.name} updated.`
                        })
                      }
                      onInstall={() => void runAddonAction("install_addon", tool.id)}
                      onToggleEnabled={() =>
                        void runAddonAction("set_addon_enabled", tool.id, !tool.enabled)
                      }
                      onUninstall={() => void runAddonAction("uninstall_addon", tool.id)}
                    />
                  );
                })}
              <AddonCard
                key="rtk"
                name="RTK"
                version={runtimeStatus?.rtk.version}
                installed={runtimeStatus?.rtk.installed === true}
                enabled={runtimeStatus?.rtk.enabled === true}
                description={
                  <>
                    Token-optimizing proxy that auto-rewrites your agent's bash commands.
                    {rtkAvgSavingsPct !== null
                      ? ` ${percent1(rtkAvgSavingsPct)}% avg savings.`
                      : ""}
                  </>
                }
                copy={addonCopy.rtk}
                infoOpen={addonInfoId === "rtk"}
                onToggleInfo={() => setAddonInfoId(addonInfoId === "rtk" ? null : "rtk")}
                busy={addonBusyId === "rtk"}
                busyLabel={addonBusyLabel}
                resultMessage={addonResult?.id === "rtk" ? addonResult.message : null}
                onDismissResult={() => setAddonResult(null)}
                sourceUrl={
                  dashboard.tools.find((tool) => tool.id === "rtk")?.sourceUrl ??
                  "https://github.com/rtk-ai/rtk"
                }
                onOpenSource={() =>
                  void openExternalLink(
                    dashboard.tools.find((tool) => tool.id === "rtk")?.sourceUrl ??
                      "https://github.com/rtk-ai/rtk"
                  )
                }
                connectors={connectors}
                showClients={
                  runtimeStatus?.rtk.installed === true && runtimeStatus.rtk.enabled === true
                }
                savings={rtkSavingsChip}
                actionsDisabled={rtkBusy || addonBusyId === "rtk" || !runtimeStatus}
                unavailableReason={
                  dashboard.tools.find((tool) => tool.id === "rtk")?.unavailableReason ??
                  null
                }
                onInstall={() => void runAddonAction("install_addon", "rtk")}
                onToggleEnabled={() => void handleRtkToggle(!runtimeStatus?.rtk.enabled)}
                onUninstall={() => void runAddonAction("uninstall_addon", "rtk")}
              >
                {runtimeStatus?.rtk.installed ? (
                  <>
                    <button
                      type="button"
                      className="addon-card__link"
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
                    >
                      {showRtkDetails ? "Hide RTK activity" : "Show RTK activity"}
                    </button>
                    {showRtkDetails ? (
                      <pre className="runtime-log" ref={rtkActivityRef}>
                        {rtkActivityLines.length > 0 ? rtkActivityLines.join("\n") : "No RTK activity yet."}
                      </pre>
                    ) : null}
                  </>
                ) : null}
              </AddonCard>
            </ul>
        </div>

        <div className="tray-content tray-content--upgrade" hidden={activeView !== "upgrade"}>
          <section className="upgrade-hero">
            <h1>Plans based on your AI subscription</h1>
            {pricingAudience === "individual" &&
              pricingStatus?.account?.upgradeAction !== "appsumo" ? (
              <div className="upgrade-billing-toggle" role="group" aria-label="Billing period">
                {(["monthly", "annual"] as const).map((period) => (
                  <button
                    key={period}
                    className={`upgrade-billing-toggle__item${billingPeriod === period ? " is-active" : ""}`}
                    onClick={() => setBillingPeriod(period)}
                    type="button"
                  >
                    {period === "annual" ? (
                      <>Annual <span className="upgrade-billing-toggle__save">Save 25%</span></>
                    ) : "Monthly"}
                  </button>
                ))}
              </div>
            ) : null}
          </section>

          {!pricingStatus?.account?.subscriptionActive ? (
            <>
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

            </>
          ) : null}

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

              const isActivePlan = plan.id === activeHeadroomPlanId && viewingSubscribedPeriod;
              // The plan a scheduled change is already headed for. Its CTA
              // still reads "Downgrade to X" otherwise, inviting a second
              // click at a change that is done.
              const isPendingTarget =
                !!pendingPlanChangeInfo &&
                plan.id === pendingPlanChangeInfo.tier &&
                billingPeriod === pendingPlanChangeInfo.billingPeriod &&
                !isActivePlan;
              return (
                <article
                  className={`upgrade-plan-card${isFeatured ? " upgrade-plan-card--featured" : ""}${isActivePlan ? " upgrade-plan-card--active" : ""}`}
                  key={plan.id}
                >
                  <div className="upgrade-plan-card__top">
                    <div className="upgrade-plan-card__title-block">
                      <span className="upgrade-plan-card__icon" aria-hidden="true">
                        <Sparkle weight={isFeatured ? "fill" : "duotone"} />
                      </span>
                      <div>
                        <h2>
                          {plan.name}
                          {isActivePlan ? (
                            <span className="upgrade-plan-card__active-badge">Active</span>
                          ) : null}
                        </h2>
                        <p>{plan.tagline}</p>
                      </div>
                    </div>
                    {plan.centeredPriceLabel ? (
                      <div className="upgrade-plan-card__price-note">{plan.centeredPriceLabel}</div>
                    ) : (
                      <div>
                        {plan.originalPrice && (plan.saleBadge || !activeHeadroomPlanId) ? (
                          <div className="upgrade-plan-card__sale-row">
                            <s className="upgrade-plan-card__original-price">{plan.originalPrice}</s>
                            <span className="upgrade-plan-card__sale-badge">
                              {plan.saleBadge ??
                                introSaleBadgeLabel(pricingStatus?.introOffer) ??
                                `${pricingStatus?.activePercentOff ?? 50}% off`}
                            </span>
                          </div>
                        ) : null}
                        <div className="upgrade-plan-card__price-block">
                          <strong>{plan.price}</strong>
                          <span>
                            {plan.billingLines[0]}
                            <br />
                            {plan.billingLines[1]}
                          </span>
                        </div>
                        {plan.reversionLine && !activeHeadroomPlanId ? (
                          <p className="upgrade-plan-card__reversion">{plan.reversionLine}</p>
                        ) : null}
                      </div>
                    )}
                    {plan.purchaseInfo ? (
                      <p className="upgrade-plan-card__purchase-info">
                        {plan.purchaseInfo.cancelAtPeriodEnd && plan.purchaseInfo.endsOn
                          ? `Ends on ${plan.purchaseInfo.endsOn}`
                          : isActivePlan && pendingPlanChangeInfo
                          ? // The stored renewal price is this plan's, and this
                            // plan is not the one that renews.
                            pendingPlanChangeInfo.note
                          : isActivePlan ? (
                            <>
                              Renews at{" "}
                              <span className="upgrade-plan-card__renewal-price">
                                {plan.purchaseInfo.renewalPriceLabel}
                              </span>{" "}
                              on {plan.purchaseInfo.renewsOn}
                              {plan.purchaseInfo.renewalNote
                                ? ` (${plan.purchaseInfo.renewalNote})`
                                : ""}
                            </>
                          ) : null}
                      </p>
                    ) : null}
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
                        <textarea
                          className="upgrade-plan-card__contact-textarea"
                          maxLength={2000}
                          onChange={(event) => {
                            setContactMessage(event.target.value);
                            if (contactSubmitError) {
                              setContactSubmitError(null);
                            }
                            if (contactSubmitSuccess) {
                              setContactSubmitSuccess(null);
                            }
                          }}
                          placeholder="Tell us about your team and what you're looking for (optional)"
                          rows={4}
                          value={contactMessage}
                        />
                        <button
                          className={`secondary-button upgrade-plan-card__button upgrade-plan-card__contact-submit${contactEmailValid ? " is-ready" : ""}`}
                          disabled={!contactEmailValid || contactSubmitBusy}
                          type="submit"
                        >
                          {contactSubmitBusy ? "Sending..." : plan.ctaLabel}
                        </button>
                      </form>
                    ) : isActivePlan && plan.purchaseInfo?.cancelAtPeriodEnd ? (
                      <button
                        className={buttonClassName}
                        disabled={reactivateBusy}
                        onClick={() => void handleReactivateSubscription()}
                        type="button"
                      >
                        {reactivateBusy ? "Resuming..." : `Resume ${plan.name} plan`}
                      </button>
                    ) : isActivePlan ? (
                      <div className="upgrade-plan-card__action-stack">
                        <button
                          className={buttonClassName}
                          disabled={upgradeActionBusy === plan.id}
                          onClick={() => void handleUpgradeAction(plan.id)}
                          type="button"
                        >
                          {upgradeActionBusy === plan.id ? "Opening..." : plan.ctaLabel}
                        </button>
                        {/* Only in-app entry to the billing portal (card, invoices,
                            downgrade) now that the Free card is gone for active
                            subscribers. Cancelling is deliberately its own action so
                            updating a card does not come with a retention pitch. */}
                        <div className="upgrade-plan-card__manage-row">
                          <button
                            className="upgrade-plan-card__manage-link"
                            disabled={upgradeActionBusy === "free"}
                            onClick={() => void handleUpgradeAction("free")}
                            type="button"
                          >
                            {upgradeActionBusy === "free" ? "Opening..." : "Manage billing"}
                          </button>
                          <button
                            className="upgrade-plan-card__manage-link"
                            disabled={upgradeActionBusy === "cancel"}
                            onClick={openCancelReason}
                            type="button"
                          >
                            {upgradeActionBusy === "cancel" ? "Opening..." : "Cancel subscription"}
                          </button>
                        </div>
                      </div>
                    ) : isPendingTarget ? (
                      <button className={buttonClassName} disabled type="button">
                        {plan.id === activeHeadroomPlanId ? "Switch scheduled" : "Change scheduled"}
                      </button>
                    ) : (
                      <button
                        className={buttonClassName}
                        disabled={upgradeActionBusy === plan.id}
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
          {reactivateError ? (
            <p className="install-progress__error">{reactivateError}</p>
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
                {pricingStatus?.claude?.profileFetchError ? (
                  <p className="settings-account-notice">
                    {pricingStatus.claude.profileFetchError}
                  </p>
                ) : null}
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
                        : connector.clientId === "codex"
                          ? "ChatGPT connection"
                          : connector.name;
                    const unavailableReason = getConnectorUnavailableReason(connector);
                    const detectionWarning = getConnectorDetectionWarning(connector);
                    const gateBlocksEnable = connectorGateBlocksEnable(connector);
                    const statusLine = connectorStatusLine(connector);
                    const toggleDisabled =
                      connectorsBusy ||
                      !canConfigureConnectorWithoutDetection(connector) ||
                      gateBlocksEnable;
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
                            <div className="connector-tooltip">
                              <p>
                                {connectorSetupDetails[connector.clientId] ??
                                  "Headroom applies local connector configuration."}
                              </p>
                              {connector.enabled ? (
                                <div className="connector-diagnostics">
                                  <strong>Connection checks</strong>
                                  {connector.verification ? (
                                    <ul>
                                      {connector.verification.checks.map((check) => (
                                        <li className="is-good" key={check}>
                                          ✓ {check}
                                        </li>
                                      ))}
                                      {connector.verification.failures.map((failure) => (
                                        <li className="is-bad" key={failure}>
                                          × {failure}
                                        </li>
                                      ))}
                                      {!connector.verification.proxyReachable ? (
                                        <li className="is-waiting">
                                          … Headroom proxy is not answering on 127.0.0.1:6767.
                                        </li>
                                      ) : null}
                                    </ul>
                                  ) : (
                                    <p>Verification details are not available yet.</p>
                                  )}
                                  <button
                                    className="addon-card__link connector-diagnostics__refresh"
                                    disabled={connectorsBusy}
                                    onClick={() => void refreshConnectors()}
                                    type="button"
                                  >
                                    {connectorsBusy ? "Checking…" : "Re-check"}
                                  </button>
                                </div>
                              ) : null}
                            </div>
                          ) : null}
                          {statusLine ? (
                            <p className={`connector-item__${statusLine.tone}`}>
                              {statusLine.text}
                            </p>
                          ) : null}
                          {(detectionWarning ?? unavailableReason) ? (
                            <p className="connector-item__reason">
                              {detectionWarning ?? unavailableReason}
                            </p>
                          ) : null}
                          {gateBlocksEnable ? (
                            <p className="connector-item__reason">
                              {pricingStatus?.gateMessage}{" "}
                              <button
                                className="addon-card__link"
                                type="button"
                                onClick={connectorGateCta}
                              >
                                {pricingStatus?.authenticated ? "Upgrade" : "Sign in"}
                              </button>
                            </p>
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
                {connectorsNotice ? (
                  <p className="install-progress__notice">{connectorsNotice}</p>
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
                      {appUpdateConfig?.betaChannelEnabled ? (
                        <span className="runtime-status__channel-pill">beta channel</span>
                      ) : null}
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
                      {(compressionOfRestPct ?? headroomLifetimeSavingsPct) !== null ? (
                        <span className="runtime-status__section-context">
                          {" "}
                          ({percent1((compressionOfRestPct ?? headroomLifetimeSavingsPct)!)}% of
                          billable input removed all-time)
                        </span>
                      ) : null}
                    </span>
                  </div>
                  <div className="runtime-status__grid runtime-status__grid--4">
                    {([
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
                        ok:
                          runtimeStatus?.mcpConfigured === true
                            ? true
                            : runtimeStatus?.mcpConfigured === false
                              ? false
                              : null,
                      },
                      {
                        name: "Kompress",
                        ok: kompressWarming
                          ? null
                          : runtimeStatus?.kompressEnabled === true
                            ? true
                            : runtimeStatus?.kompressEnabled === false
                              ? false
                              : null,
                        suffix: kompressWarming ? "warming up" : undefined,
                      },
                    ] as { name: string; ok: boolean | null; suffix?: string; onClick?: () => void }[]).map((s) => {
                      const indicatorClass =
                        s.ok === true
                          ? "runtime-status__indicator--ok"
                          : s.ok === false
                            ? "runtime-status__indicator--off"
                            : "runtime-status__indicator--unknown";
                      const indicatorSymbol = s.ok === true ? "✔" : s.ok === false ? "✖" : "–";
                      return (
                        <span
                          key={s.name}
                          className={`runtime-status__item${s.onClick ? " runtime-status__item--clickable" : ""}`}
                          onClick={s.onClick}
                          title={s.ok === null ? `${s.name} status unknown` : undefined}
                        >
                          <span className="runtime-status__label">{s.name}:</span>
                          <span className={`runtime-status__indicator ${indicatorClass}`}>
                            {indicatorSymbol}
                          </span>
                          {s.suffix && <span className="runtime-status__suffix">({s.suffix})</span>}
                        </span>
                      );
                    })}
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
                </div>
              </article>
              <article className="soft-card panel-card">
                <div className="panel-card__header">
                  <div>
                    <h3>Open on login</h3>
                  </div>
                  <div>
                    <p>
                      Automatically launch Headroom whenever you login or restart.
                    </p>
                  </div>
                  <div className="connector-item__controls">
                    <button
                      aria-checked={autostartEnabled === true}
                      aria-label={`${autostartEnabled ? "Disable" : "Enable"} open on login`}
                      className={`connector-switch${autostartEnabled ? " is-on" : ""}`}
                      disabled={autostartBusy || autostartEnabled === null}
                      onClick={() => void handleAutostartToggle(!autostartEnabled)}
                      role="switch"
                      type="button"
                    >
                      <span className="connector-switch__thumb" />
                    </button>
                  </div>
                </div>
              </article>

              {/* Power-user settings almost nobody needs, collapsed so they do
                  not crowd out the ones people came here for. New ones go
                  inside; the disclosure state is the browser's own. */}
              <details className="advanced-section">
                <summary>Advanced</summary>
                <div className="advanced-section__body">
                  <UpstreamPanel />
                </div>
              </details>

              <article className="soft-card panel-card">
                <div className="panel-card__header">
                  <div>
                    <h3>Uninstall</h3>
                  </div>
                </div>
                <p>
                  Removes Headroom and everything it changed: the runtime, its addons, your
                  coding agent configs, and the app itself. You will see the full list first.
                </p>
                <button
                  className="secondary-button secondary-button--small"
                  onClick={() => {
                    setUninstallError(null);
                    setShowUninstallDialog(true);
                  }}
                  type="button"
                >
                  Uninstall Headroom
                </button>
              </article>

              <button
                className="contact-link"
                onClick={() => void invoke("open_external_link", { url: "mailto:support@extraheadroom.com" })}
                type="button"
              >
                Contact us
              </button>
<button
                className="quit-button"
                onClick={() => void invoke("quit_headroom")}
                type="button"
              >
                Quit Headroom
              </button>
            </section>
          </div>

          {setupStall && (
            <SetupStallModal
              kind={setupStall.kind}
              onClose={() => setSetupStall(null)}
              onOpenSettings={() => {
                setSetupStall(null);
                setActiveView("settings");
              }}
              onContact={() => {
                void invoke("open_external_link", {
                  url: buildSetupStallMailto(setupStall.kind, {
                    appVersion: appSemver,
                    lifetimeRequests: dashboard.lifetimeRequests,
                    runtime: runtimeStatus,
                    connectors
                  })
                });
              }}
            />
          )}

          {showSavingsInfo && (
            <div
              className="modal-backdrop"
              role="dialog"
              aria-modal="true"
              onClick={() => setShowSavingsInfo(false)}
            >
              <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                <h3>How savings are calculated</h3>
                <p>Headroom intercepts and prunes all inputs before sending them to Claude or ChatGPT.</p>
                <p>Savings = tokens removed &times; API token prices.</p>
                {dashboard.savingsBreakdown ? (
                  <div className="savings-breakdown">
                    <div className="savings-breakdown__row">
                      <span>Input compression (Headroom)</span>
                      <strong>{currency(dashboard.savingsBreakdown.compressionSavingsUsd)}</strong>
                    </div>
                    {dashboard.savingsBreakdown.outputSavingsUsd >= 0.005 ? (
                      <>
                        <div className="savings-breakdown__row">
                          <span>Output shaping (Headroom)</span>
                          <strong>{currency(dashboard.savingsBreakdown.outputSavingsUsd)}</strong>
                        </div>
                        {/* Lifetime figure, recomputed from the shaper's ledger
                            (see output_savings.rs) and predating per-day
                            tracking of the layer -- so the daily bars can add
                            up to less. `requests` counts what the estimate
                            actually covers, not every shaped request: strata
                            the baseline never observed are excluded rather
                            than scored against a global mean. */}
                        <p className="savings-breakdown__note">
                          {dashboard.outputReduction?.method === "measured"
                            ? `Output savings are measured: a small share of conversations run unshaped as a control group, and Headroom compares your shaped replies against them across ${compactNumber(dashboard.outputReduction.requests)} requests.`
                            : `Output savings are counterfactual: Headroom compares each reply against a baseline learned from your past replies${
                                dashboard.outputReduction
                                  ? `, over the ${compactNumber(dashboard.outputReduction.requests)} requests that baseline covers`
                                  : ""
                              }.`}
                        </p>
                      </>
                    ) : null}
                    {(dashboard.savingsBreakdown.toolSchemaTokensSaved ?? 0) > 0 ? (
                      <>
                        <div className="savings-breakdown__row">
                          <span>Additional costs saved (tool schemas deferred)</span>
                          <strong>{currencyExact(dashboard.savingsBreakdown.toolSchemaSavingsUsd ?? 0)}</strong>
                        </div>
                        {/* Priced at the cache-read rate, not the input rate --
                            see tool_schema_savings_usd in state.rs. */}
                        <p className="savings-breakdown__note">
                          {compactNumber(dashboard.savingsBreakdown.toolSchemaTokensSaved ?? 0)} tokens
                          of tool definitions Headroom kept out of your requests until they were
                          needed. These sit at the front of the cached prefix, so they are priced at
                          the provider's cache-read rate rather than the full input rate. Counted in
                          addition to the Total costs saved figure, not inside it.
                        </p>
                      </>
                    ) : null}
                    {dashboard.savingsBreakdown.cacheSavingsUsd >= 0.005 ? (
                      <>
                        <div className="savings-breakdown__row savings-breakdown__row--context">
                          <span>Prompt caching (your AI client)</span>
                          <strong>{currency(dashboard.savingsBreakdown.cacheSavingsUsd)}</strong>
                        </div>
                        <p className="savings-breakdown__note">
                          Cache discounts are earned by your coding agent's own prompt caching. Headroom
                          never counts them as its savings. Headroom compression is cache-aligned:
                          it only touches content outside the cache.
                        </p>
                      </>
                    ) : null}
                    {(dashboard.savingsBreakdown.modelRates?.length ?? 0) > 1 ? (
                      <details className="savings-breakdown__models">
                        <summary>Compression rate by model</summary>
                        <div className="savings-breakdown__models-body">
                          {dashboard.savingsBreakdown.modelRates?.map((row) => (
                            <div className="savings-breakdown__row" key={row.model}>
                              <span>
                                {row.model}{" "}
                                <span className="savings-breakdown__sample">
                                  {compactNumber(row.requests)} requests
                                </span>
                              </span>
                              <strong>{percent1(row.savingsPercent)}%</strong>
                            </div>
                          ))}
                          {/* Rates only -- by_model covers a fraction of lifetime
                              history, so its dollars would not add up to the rows
                              above. See ModelSavingsRate in models.rs. */}
                          <p className="savings-breakdown__note">
                            How much of each model's input Headroom removed. The spread is mostly
                            workload, not model: long tool output and logs compress far better than
                            prose, so the blended figure tracks whichever models you use most rather
                            than how hard Headroom is working. Models with under 100 requests are
                            left out.
                          </p>
                        </div>
                      </details>
                    ) : null}
                  </div>
                ) : null}
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

          {showCacheInfo && (
            <div
              className="modal-backdrop"
              role="dialog"
              aria-modal="true"
              onClick={() => setShowCacheInfo(false)}
            >
              <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                <h3>Cache hits &amp; compression</h3>
                <p>
                  Most of your input is re-sent conversation history that your AI client serves
                  from the provider&apos;s prompt cache at ~10% of the input price. Headroom
                  deliberately leaves that cached prefix untouched (compressing it would break the
                  discount), so its compression works on the rest. A healthier cache makes blended
                  savings rates look smaller while your actual bill shrinks.
                </p>
                <div className="savings-breakdown">
                  {[
                    { label: "Today", pair: cachePairToday },
                    { label: "This month", pair: cachePairMonth },
                    { label: "All time", pair: cachePairAllTime }
                  ].map(({ label, pair }) => (
                    <div className="savings-breakdown__row" key={label}>
                      <span>{label}</span>
                      <strong>
                        {pair
                          ? `${Math.round(pair.hitPct)}% cache hits · ${Math.round(
                              pair.compressedPct
                            )}% of the rest compressed`
                          : "No cache data"}
                      </strong>
                    </div>
                  ))}
                </div>
                <p className="savings-breakdown__note">
                  Today and this month cover the part of the period with cache data (the backend
                  keeps a limited history of cache checkpoints). Output shaping is a separate
                  layer and is not part of these rates.
                </p>
                <div className="modal-actions">
                  <button
                    className="button button--primary"
                    onClick={() => setShowCacheInfo(false)}
                    type="button"
                  >
                    Got it
                  </button>
                </div>
              </div>
            </div>
          )}

          {showUninstallDialog ? (
            <div
              className="modal-backdrop"
              role="dialog"
              aria-modal="true"
              onClick={() => {
                if (!uninstallBusy) {
                  setShowUninstallDialog(false);
                }
              }}
            >
              <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                <h3>Uninstall Headroom?</h3>
                <p>This will:</p>
                <ul className="api-key-guide">
                  <li>
                    Restore the original routing config for every agent Headroom set
                    up (Claude Code, ChatGPT, Grok Build, OpenCode) and remove the export
                    block from your shell profile
                  </li>
                  <li>
                    Remove Headroom's addons and MCP servers (ponytail, caveman, serena,
                    context7, codebase-memory, MarkItDown)
                  </li>
                  <li>
                    Delete Headroom's data: logs, caches, setup state, the Python runtime,
                    and saved credentials
                  </li>
                  <li>Disable the login item</li>
                  <li>Remove the app itself</li>
                </ul>
                <p className="uninstall-note">
                  Terminals you already have open keep{" "}
                  <code>ANTHROPIC_BASE_URL</code> until you restart them, or run{" "}
                  <code>unset ANTHROPIC_BASE_URL</code>.
                </p>
                <p>You can reinstall at any time by launching Headroom again.</p>
                {uninstallError ? (
                  <p className="install-progress__error">{uninstallError}</p>
                ) : null}
                <div className="modal-actions">
                  <button
                    className="secondary-button"
                    disabled={uninstallBusy}
                    onClick={() => setShowUninstallDialog(false)}
                    type="button"
                  >
                    Cancel
                  </button>
                  <button
                    className="primary-button"
                    disabled={uninstallBusy}
                    onClick={() => void handleUninstall()}
                    type="button"
                  >
                    {uninstallBusy ? "Uninstalling…" : "Uninstall and quit"}
                  </button>
                </div>
              </div>
            </div>
          ) : null}

          {pendingPlanChange ? (() => {
            const isDowngrade = isTierDowngrade(
              pendingPlanChange.fromTier,
              pendingPlanChange.toTier
            );
            // Same tier, different billing period: a switch, not an upgrade.
            const isPeriodSwitch = pendingPlanChange.fromTier === pendingPlanChange.toTier;
            const action = isPeriodSwitch ? "switch" : isDowngrade ? "downgrade" : "upgrade";
            // The current plan is priced on the period it was bought on, not
            // the one being switched to.
            const currentBillingPeriod =
              pricingStatus?.account?.subscriptionBillingPeriod === "annual"
                ? "annual"
                : pricingStatus?.account?.subscriptionBillingPeriod === "monthly"
                ? "monthly"
                : pendingPlanChange.billingPeriod;
            const currentPriceLabel = getPlanRenewalPriceLabel(
              pendingPlanChange.fromTier,
              currentBillingPeriod,
              {
                fromTier: pendingPlanChange.fromTier,
                currentPaidCents: pricingStatus?.account?.subscriptionAmountCents
              }
            );
            // The target card already prices this tier for this period with the
            // account discount carried at its exact ratio. Re-deriving it from
            // the amount currently paid quoted $0 to anyone mid 100%-off period,
            // and 12x the real figure on an annual -> monthly switch (an annual
            // cycle amount divided by one month).
            const targetCard = upgradePlansState.plans.find(
              (plan) => plan.id === pendingPlanChange.toTier
            );
            const newPriceLabel = targetCard
              ? `${targetCard.price} / month`
              : getPlanRenewalPriceLabel(pendingPlanChange.toTier, pendingPlanChange.billingPeriod);
            // Mirrors the server's rule: a step down the ladder, or the same
            // tier on a shorter cycle, waits for the term already paid for
            // rather than crediting back the unused part of it.
            const isDeferred =
              isDowngrade ||
              (isPeriodSwitch &&
                currentBillingPeriod === "annual" &&
                pendingPlanChange.billingPeriod === "monthly");
            const renewsOnLabel = pricingStatus?.account?.subscriptionRenewsAt
              ? new Date(pricingStatus.account.subscriptionRenewsAt).toLocaleDateString(undefined, {
                  year: "numeric",
                  month: "long",
                  day: "numeric"
                })
              : null;
            return (
              <div
                className="modal-backdrop"
                role="dialog"
                aria-modal="true"
                onClick={cancelPlanChange}
              >
                <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                  <h3>Confirm your {action}</h3>
                  <p>
                    You'll {action} from your{" "}
                    <strong>{currentPriceLabel}</strong>{" "}
                    <strong>{upgradePlanIntentLabel(pendingPlanChange.fromTier)}</strong>{" "}
                    plan to the{" "}
                    <strong>{newPriceLabel}</strong>{" "}
                    <strong>{upgradePlanIntentLabel(pendingPlanChange.toTier)}</strong>{" "}
                    plan, billed{" "}
                    {pendingPlanChange.billingPeriod === "annual" ? "annually" : "monthly"}.
                  </p>
                  <p>
                    {isDeferred
                      ? `Nothing changes today: you keep the ${
                          upgradePlanIntentLabel(pendingPlanChange.fromTier) ?? "current"
                        } plan you have paid for${
                          renewsOnLabel ? ` until ${renewsOnLabel}` : " until the end of the term"
                        }, and the new plan starts then. No charge and no credit today.`
                      : "You'll be charged a prorated amount today for the remaining time in your current billing period, with your existing discount applied."}
                  </p>
                  {/* A period switch moves the renewal date, so the stored one
                      would be stale here; a deferred change already named it. */}
                  {!isPeriodSwitch && !isDeferred && pricingStatus?.account?.subscriptionRenewsAt ? (
                    <p>
                      Your subscription will then renew on{" "}
                      <strong>
                        {new Date(
                          pricingStatus.account.subscriptionRenewsAt
                        ).toLocaleDateString(undefined, {
                          year: "numeric",
                          month: "long",
                          day: "numeric"
                        })}
                      </strong>
                      .
                    </p>
                  ) : null}
                  {planChangeError ? (
                    <p className="install-progress__error">{planChangeError}</p>
                  ) : null}
                  <div className="modal-actions">
                    <button
                      className="secondary-button"
                      disabled={planChangeBusy}
                      onClick={cancelPlanChange}
                      type="button"
                    >
                      Cancel
                    </button>
                    <button
                      className="primary-button"
                      disabled={planChangeBusy}
                      onClick={() => void confirmPlanChange()}
                      type="button"
                    >
                      {planChangeBusy
                        ? isPeriodSwitch
                          ? "Switching…"
                          : isDowngrade
                          ? "Downgrading…"
                          : "Upgrading…"
                        : `Confirm ${action}`}
                    </button>
                  </div>
                </div>
              </div>
            );
          })() : null}

          {cancelReasonOpen ? (
            <div
              className="modal-backdrop"
              role="dialog"
              aria-modal="true"
              onClick={() => {
                if (!cancelBusy) setCancelReasonOpen(false);
              }}
            >
              <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                <h3>Why are you cancelling?</h3>
                <p>
                  This goes straight to us and takes one click. It is the only way
                  we find out what is not working.
                </p>
                <div className="reason-list">
                  {CANCELLATION_REASONS.map((reason) => (
                    <label className="reason-list__item" key={reason.value}>
                      <input
                        checked={cancelReason === reason.value}
                        disabled={cancelBusy}
                        name="cancellation-reason"
                        onChange={() => setCancelReason(reason.value)}
                        type="radio"
                        value={reason.value}
                      />
                      <span>{reason.label}</span>
                    </label>
                  ))}
                </div>
                <textarea
                  className="reason-note"
                  disabled={cancelBusy}
                  onChange={(event) => setCancelNote(event.target.value)}
                  placeholder="Anything else you want to add? (optional)"
                  rows={3}
                  value={cancelNote}
                />
                <div className="modal-actions">
                  <button
                    className="secondary-button"
                    disabled={cancelBusy}
                    onClick={() => setCancelReasonOpen(false)}
                    type="button"
                  >
                    Never mind
                  </button>
                  <button
                    className="primary-button"
                    disabled={!cancelReason || cancelBusy}
                    onClick={() => void handleCancelContinue()}
                    type="button"
                  >
                    {cancelBusy ? "One moment..." : "Continue"}
                  </button>
                </div>
              </div>
            </div>
          ) : null}

          {saveOffer ? (
            <div
              className="modal-backdrop"
              role="dialog"
              aria-modal="true"
              onClick={() => {
                if (!saveOfferBusy) setSaveOffer(null);
              }}
            >
              <div className="modal-card" onClick={(e) => e.stopPropagation()}>
                {saveOfferRedeemed ? (
                  <>
                    <h3>You're all set</h3>
                    <p>
                      Your plan stays active at{" "}
                      <strong>{formatCents(saveOffer.offerMonthlyCents)} / month</strong>{" "}
                      for the next {saveOffer.durationMonths} months. The new price
                      takes effect{" "}
                      {saveOffer.startsOn ? `on ${saveOffer.startsOn}` : "at your next renewal"}.
                    </p>
                    <div className="modal-actions">
                      <button
                        className="primary-button"
                        onClick={() => setSaveOffer(null)}
                        type="button"
                      >
                        Done
                      </button>
                    </div>
                  </>
                ) : (
                  <>
                    <h3>Before you go: {saveOffer.percentOff}% off for {saveOffer.durationMonths} months</h3>
                    <p>
                      Stay on your plan and pay{" "}
                      <strong>{formatCents(saveOffer.offerMonthlyCents)} / month</strong>{" "}
                      instead of{" "}
                      <strong>{formatCents(saveOffer.currentMonthlyCents)} / month</strong>{" "}
                      for the next {saveOffer.durationMonths} months
                      {saveOffer.billingPeriod === "annual" ? ", billed annually" : ""}.
                      That is {saveOffer.percentOff}% off the price your plan renews
                      at.
                    </p>
                    <p>
                      The new price starts{" "}
                      {saveOffer.startsOn ? `on ${saveOffer.startsOn}` : "at your next renewal"}
                      , and any discount you are on until then is unaffected. Nothing
                      else about your plan changes, and you can still cancel any time.
                    </p>
                    {saveOfferError ? (
                      <p className="install-progress__error">{saveOfferError}</p>
                    ) : null}
                    <div className="modal-actions">
                      <button
                        className="secondary-button"
                        disabled={saveOfferBusy}
                        onClick={() => void handleDeclineSaveOffer()}
                        type="button"
                      >
                        Continue to cancel
                      </button>
                      <button
                        className="primary-button"
                        disabled={saveOfferBusy}
                        onClick={() => void handleRedeemSaveOffer()}
                        type="button"
                      >
                        {saveOfferBusy
                          ? "Applying..."
                          : `Keep it at ${formatCents(saveOffer.offerMonthlyCents)} / mo`}
                      </button>
                    </div>
                  </>
                )}
              </div>
            </div>
          ) : null}

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
                {appUpdateAvailable.notes && appUpdateAvailable.notes.trim() ? (
                  <div className="release-notes">
                    <h4>What&apos;s new</h4>
                    <pre>{appUpdateAvailable.notes.trim()}</pre>
                  </div>
                ) : null}
                {appUpdateStatusCopy ? (
                  <p className="app-update-card__summary">{appUpdateStatusCopy}</p>
                ) : null}
                <div className="modal-actions">
                  <button
                    className="secondary-button"
                    disabled={appUpdateInstallBusy || appUpdateRestartBusy}
                    onClick={() => setShowAppUpdateDialog(false)}
                    type="button"
                  >
                    Later
                  </button>
                  <button
                    className="primary-button"
                    disabled={appUpdateInstallBusy || appUpdateRestartBusy}
                    onClick={() =>
                      appUpdateReadyToRestart
                        ? restartIntoInstalledUpdate()
                        : void installAvailableUpdate()
                    }
                    type="button"
                  >
                    {appUpdateRestartBusy
                      ? "Restarting…"
                      : appUpdateInstallBusy
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
