import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { needsTermsAcceptance } from "./launcherHelpers";
import { formatDayKey } from "./dashboardHelpers";
import type { ClientConnectorStatus, DashboardState, UnroutedClient } from "./types";

// Nothing at all has come through. This is the weaker of the two signals:
// uptime is not the same as time spent coding (Headroom can autostart at
// login), so a clock on its own cannot tell "the hookup is broken" from "the
// user has not opened a terminal yet". The connector predicate below carries
// most of the confidence here; the timer is just a floor.
export const SETUP_STALL_NO_TRAFFIC_AFTER_MS = 30 * 60 * 1000;

// Requests are arriving and none of them are being optimized. The user is
// demonstrably at their keyboard routing traffic through Headroom, so this
// fires sooner and leans on request volume rather than the clock: ten requests
// with nothing trimmed does not happen by accident, and every minute of
// waiting is a minute of them getting no value while believing it works.
export const SETUP_STALL_NO_SAVINGS_AFTER_MS = 10 * 60 * 1000;
export const SETUP_STALL_NO_SAVINGS_MIN_REQUESTS = 10;

// Earliest point either branch can fire. The watchdog skips its dashboard read
// entirely before this.
export const SETUP_STALL_EARLIEST_MS = Math.min(
  SETUP_STALL_NO_TRAFFIC_AFTER_MS,
  SETUP_STALL_NO_SAVINGS_AFTER_MS
);

// Cadence of the background check. The dashboard read is local (in-memory Rust
// state), so this is cheap; 5 min just keeps it off the hot path while the tray
// is hidden. It also means an alert lands anywhere within 5 minutes of its
// threshold, which is well inside the tolerance of these signals.
export const SETUP_STALL_CHECK_INTERVAL_MS = 5 * 60 * 1000;

// Drift: the install has saved before, but the trailing daily buckets show
// traffic passing through with nothing optimized. That is the desktop-side
// mirror of the server's "on but idle" admin scope, and it means the hookup
// broke after it had been working (agent reconnected outside Headroom,
// optimization silently off) rather than never having worked.
export const SAVINGS_DRIFT_WINDOW_DAYS = 3;
// ponytail: crude evidence floor so a trickle (one stray request, health
// checks) cannot accuse a working setup; tune if support traffic says so.
export const SAVINGS_DRIFT_MIN_TOKENS_SENT = 200_000;
// The newest bucket must be at least this recent, or the user simply stopped
// coding and stale buckets prove nothing about the setup today.
export const SAVINGS_DRIFT_MAX_BUCKET_AGE_DAYS = 4;

// Local day, not UTC, for the same reason the other urgent notifications use a
// local key: a UTC key flips mid-afternoon for US users and lets two alerts
// land in one local day.
const SETUP_STALL_DAY_KEY = "headroom_setup_stall_date";

// "no_traffic": Headroom never saw a request, so the hookup itself is suspect
// (terminal still running the pre-install environment is the classic cause).
// "no_savings": requests are arriving but nothing is being trimmed, which is a
// different failure - optimization paused, gated, or misconfigured.
export type SetupStallKind = "no_traffic" | "no_savings" | "drift";

export interface SetupStallAlert {
  kind: SetupStallKind;
  title: string;
  body: string;
}

export function setupStallNoTrafficMinutes(): number {
  return Math.round(SETUP_STALL_NO_TRAFFIC_AFTER_MS / 60_000);
}

const STALL_TITLE = "Headroom hasn't saved anything yet";

/// The drift branch fires on installs that HAVE saved before, so the shared
/// "yet" title would be factually wrong there.
function stallTitle(kind: SetupStallKind): string {
  return kind === "drift" ? "Headroom has stopped saving" : STALL_TITLE;
}

function stallBody(kind: SetupStallKind): string {
  if (kind === "drift") {
    return "Headroom is running, but days of requests have passed through with nothing optimized. Your coding agent has probably disconnected from Headroom. Open Headroom to check.";
  }
  if (kind === "no_traffic") {
    return `Your coding agent is connected, but ${setupStallNoTrafficMinutes()} minutes on not one request has come back through Headroom. It is probably still running with its pre-Headroom settings. Open Headroom to check.`;
  }
  return "Requests are reaching Headroom but none are being optimized. Something is likely misconfigured. Open Headroom to check.";
}

/// In-app phrasing of the same two failures. `stallBody` above is written for a
/// native notification and ends with "Open Headroom to check", which reads as
/// nonsense on a banner inside Headroom.
function stallBannerBody(kind: SetupStallKind): string {
  if (kind === "drift") {
    return "Requests are passing through, but nothing has been optimized for days. Your coding agent has likely reconnected outside Headroom - restart it so it picks Headroom's settings back up.";
  }
  if (kind === "no_traffic") {
    return "No request has come through Headroom yet. Your terminal or editor is probably still running with its pre-Headroom settings - restart it and they should pick the new settings up.";
  }
  return "Requests are reaching Headroom but none are being optimized, so nothing is being saved yet. Check that your coding tool is still connected below.";
}

export interface SetupStallContext {
  /// True when the account gate has optimization switched off (unpaid plan,
  /// signed out, weekly cap hit). Zero savings is the expected outcome then,
  /// and those states already have their own daily notifications. Undefined
  /// means pricing status hasn't loaded yet, which is treated as allowed.
  optimizationBlocked?: boolean;
  /// Current connector status. Undefined means it hasn't loaded yet.
  connectors?: ClientConnectorStatus[];
  /// Test override (HEADROOM_FAKE_SETUP_STALL on an RC build, see
  /// get_debug_overrides in Rust): fire this branch immediately, ignoring
  /// uptime, savings, connector state and the account gate. The caller is
  /// responsible for not letting a forced alert repeat on every poll.
  forceKind?: SetupStallKind | null;
}

/// A connector we configured that has never seen traffic come back through
/// Headroom - the same state the Home banner surfaces as "restart it first".
/// This, not the clock, is what makes a no-traffic alert trustworthy.
///
/// Undefined connectors means status hasn't loaded; stay quiet rather than
/// guess. Empty means nothing is connected, which is not a malfunction and is
/// already covered by the banner's "No coding tools connected" state.
function hasUnverifiedConnector(connectors: ClientConnectorStatus[] | undefined): boolean {
  return (connectors ?? []).some(
    (connector) => connector.installed && connector.enabled && !connector.verified
  );
}

/// The install saved before, yet the trailing daily buckets show real traffic
/// with zero savings. Evidence-gated on purpose: an idle machine (vacation,
/// another computer) produces no fresh buckets and must not be nagged.
export function savingsDrifted(dashboard: DashboardState): boolean {
  const recent = dashboard.dailySavings.slice(-SAVINGS_DRIFT_WINDOW_DAYS);
  if (recent.length < SAVINGS_DRIFT_WINDOW_DAYS) {
    return false;
  }
  // Bucket dates are UTC day keys ("YYYY-MM-DD"), so compare in kind. Off by
  // up to a day versus local time, which the 4-day allowance absorbs.
  const newestAllowed = new Date(Date.now() - SAVINGS_DRIFT_MAX_BUCKET_AGE_DAYS * 86_400_000)
    .toISOString()
    .slice(0, 10);
  if (recent[recent.length - 1].date < newestAllowed) {
    return false;
  }
  let sent = 0;
  let saved = 0;
  for (const point of recent) {
    sent += point.totalTokensSent;
    // Tokens and USD summed together is a unit crime, but the sum is only
    // ever compared against exactly zero: any nonzero term means savings.
    saved +=
      point.estimatedTokensSaved + point.estimatedSavingsUsd + (point.outputTokensSaved ?? 0);
  }
  return saved === 0 && sent >= SAVINGS_DRIFT_MIN_TOKENS_SENT;
}

/// Pure decision: is this session's silence worth alerting about? Returns null
/// whenever the silence is expected (still installing, runtime not there yet,
/// blocked behind a gate, not enough uptime) or whenever savings have landed.
export function evaluateSetupStall(
  dashboard: DashboardState,
  uptimeMs: number,
  context: SetupStallContext = {}
): SetupStallAlert | null {
  if (context.forceKind) {
    return {
      kind: context.forceKind,
      title: stallTitle(context.forceKind),
      body: stallBody(context.forceKind),
    };
  }
  if (uptimeMs < SETUP_STALL_EARLIEST_MS) {
    return null;
  }
  // A half-finished install has its own progress UI and its own failure
  // reporting. Alerting here would just duplicate it with worse copy.
  if (!dashboard.bootstrapComplete || !dashboard.pythonRuntimeInstalled) {
    return null;
  }
  // Nothing routes through Headroom until the Terms gate is cleared, so zero
  // savings there says nothing about the setup.
  if (needsTermsAcceptance(dashboard.requiredTermsVersion, dashboard.acceptedTermsVersion)) {
    return null;
  }
  if (context.optimizationBlocked) {
    return null;
  }
  const savingsRecorded =
    dashboard.lifetimeEstimatedTokensSaved > 0 || dashboard.lifetimeEstimatedSavingsUsd > 0;
  if (savingsRecorded) {
    // Saved before: the setup branches below no longer apply, but the hookup
    // can still break later. Same daily throttle, its own evidence gate.
    if (savingsDrifted(dashboard)) {
      return { kind: "drift", title: stallTitle("drift"), body: stallBody("drift") };
    }
    return null;
  }

  if (dashboard.lifetimeRequests === 0) {
    if (uptimeMs < SETUP_STALL_NO_TRAFFIC_AFTER_MS) {
      return null;
    }
    if (!hasUnverifiedConnector(context.connectors)) {
      return null;
    }
    return { kind: "no_traffic", title: stallTitle("no_traffic"), body: stallBody("no_traffic") };
  }

  // Some traffic, but not yet enough to rule out a normal quiet start. Wait
  // for the volume rather than accusing the setup on one or two requests.
  if (
    dashboard.lifetimeRequests < SETUP_STALL_NO_SAVINGS_MIN_REQUESTS ||
    uptimeMs < SETUP_STALL_NO_SAVINGS_AFTER_MS
  ) {
    return null;
  }
  return { kind: "no_savings", title: stallTitle("no_savings"), body: stallBody("no_savings") };
}

/// Line for the always-on Home banner, which otherwise reassures a user with
/// zero savings to "check back later" - the wrong thing to tell someone whose
/// install has never seen a request. Returns null when there is nothing honest
/// to say, in which case the caller keeps the existing copy.
///
/// Deliberately NOT `evaluateSetupStall`: that one gates `no_traffic` behind
/// `hasUnverifiedConnector`, so it stays silent when a connector verifies (its
/// config is present) yet traffic never actually arrives. That silent state is
/// the one we most need to surface. A passive banner line can carry that risk
/// where the interrupting modal could not, so this drops the predicate and
/// leans on the launch/lifetime gates below instead.
///
/// `uptimeMs` is time since THIS app run started, not since install (Headroom
/// can autostart at login), so it cannot express "a while with nothing
/// happening" on its own. `launchExperience` and `lifetimeRequests` both
/// survive a restart, and together they do: not the first launch, and not one
/// request in the install's entire history.
export function setupStallBannerLine(
  dashboard: DashboardState,
  uptimeMs: number,
  context: SetupStallContext = {}
): string | null {
  if (context.forceKind) {
    return stallBannerBody(context.forceKind);
  }
  // A first run is allowed to be quiet: the user may simply not have opened a
  // terminal yet, and the install flow has its own progress UI.
  if (dashboard.launchExperience === "first_run") {
    return null;
  }
  if (!dashboard.bootstrapComplete || !dashboard.pythonRuntimeInstalled) {
    return null;
  }
  if (needsTermsAcceptance(dashboard.requiredTermsVersion, dashboard.acceptedTermsVersion)) {
    return null;
  }
  // Zero savings is the expected outcome behind a gate and says nothing about
  // the setup, same reasoning as evaluateSetupStall.
  if (context.optimizationBlocked) {
    return null;
  }
  const savingsRecorded =
    dashboard.lifetimeEstimatedTokensSaved > 0 || dashboard.lifetimeEstimatedSavingsUsd > 0;
  if (savingsRecorded) {
    return savingsDrifted(dashboard) ? stallBannerBody("drift") : null;
  }

  if (dashboard.lifetimeRequests === 0) {
    // Do not accuse anything during boot.
    return uptimeMs < SETUP_STALL_NO_TRAFFIC_AFTER_MS ? null : stallBannerBody("no_traffic");
  }

  if (
    dashboard.lifetimeRequests < SETUP_STALL_NO_SAVINGS_MIN_REQUESTS ||
    uptimeMs < SETUP_STALL_NO_SAVINGS_AFTER_MS
  ) {
    return null;
  }
  return stallBannerBody("no_savings");
}

/// Fire the alert at most once per local day, and never once savings exist.
/// Returns the alert when this call consumed the day's slot (the caller should
/// then show the modal), null when throttled or not due.
///
/// The native notification is skipped when the tray window is already visible
/// - the modal is the better surface in that case - but the day slot is
/// consumed either way so the user gets one interruption, not two.
export async function maybeFireSetupStallAlert(
  dashboard: DashboardState,
  uptimeMs: number,
  context: SetupStallContext = {}
): Promise<SetupStallAlert | null> {
  const alert = evaluateSetupStall(dashboard, uptimeMs, context);
  if (!alert) {
    return null;
  }

  // A forced alert bypasses the day slot entirely rather than consuming it:
  // testing the modal must not silence the real alert for the rest of the day.
  if (!context.forceKind) {
    const today = formatDayKey(new Date());
    if (readDayKey() === today) {
      return null;
    }
    writeDayKey(today);
  }

  // In production the notification is redundant when the user is already
  // looking at the tray, so it is skipped there. A forced run always sends it:
  // the point of the override is to see both surfaces, and the forced alert
  // fires about a second after launch, which is exactly when a tester is most
  // likely to have the tray open and would otherwise see the modal only.
  if (context.forceKind || !(await isWindowVisible())) {
    try {
      await invoke("show_notification", {
        title: alert.title,
        body: alert.body,
        action: "setup",
      });
    } catch {
      // Best effort. The modal still carries the message.
    }
  }

  return alert;
}

const UNROUTED_ALERT_DAY_KEY = "headroom_unrouted_alert_date";

/// Title and per-agent line for the agent-ran-without-Headroom alert. An
/// enabled agent had its connection re-applied before this shows, so the ask
/// is only a restart; a switched-off one needs the user's say-so.
export function unroutedTitle(clients: UnroutedClient[]): string {
  return `${clients.map((client) => client.name).join(" and ")} ran without Headroom`;
}

export function unroutedBody(client: UnroutedClient): string {
  return client.enabled
    ? `${client.name} was used on this machine, but none of its requests reached Headroom. Its connection was just re-applied. Quit and reopen ${client.name} so it picks the settings up.`
    : `${client.name} was used on this machine, but its Headroom connection is switched off, so nothing was optimized. Turn the connection back on to resume saving.`;
}

/// Once per local day, on its own slot so this and the stall alert cannot
/// starve each other. Native notification only when the window is hidden;
/// the modal is the in-app surface either way.
export async function maybeFireUnroutedAlert(
  clients: UnroutedClient[]
): Promise<UnroutedClient[] | null> {
  if (clients.length === 0) {
    return null;
  }
  const today = formatDayKey(new Date());
  if (readDayKey(UNROUTED_ALERT_DAY_KEY) === today) {
    return null;
  }
  writeDayKey(today, UNROUTED_ALERT_DAY_KEY);
  if (!(await isWindowVisible())) {
    try {
      await invoke("show_notification", {
        title: unroutedTitle(clients),
        body: unroutedBody(clients[0]),
        action: "setup",
      });
    } catch {
      // Best effort. The modal still carries the message.
    }
  }
  return clients;
}

function readDayKey(key: string = SETUP_STALL_DAY_KEY): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeDayKey(day: string, key: string = SETUP_STALL_DAY_KEY): void {
  try {
    localStorage.setItem(key, day);
  } catch {
    // Private-mode / quota failures shouldn't suppress the alert itself.
  }
}

/// Test affordance: clear the once-per-day throttle.
export function __resetSetupStallThrottle(): void {
  try {
    localStorage.removeItem(SETUP_STALL_DAY_KEY);
  } catch {
    // no-op
  }
}

async function isWindowVisible(): Promise<boolean> {
  return getCurrentWindow()
    .isVisible()
    .catch(() => false);
}
