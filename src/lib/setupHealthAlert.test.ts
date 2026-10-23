import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { mockDashboard } from "./mockData";
import type { ClientConnectorStatus, DailySavingsPoint, DashboardState } from "./types";
import {
  __resetSetupStallThrottle,
  connectorBadgeTip,
  evaluateSetupStall,
  maybeFireSetupStallAlert,
  setupStallBannerLine,
  mergeUnroutedBadges,
  pruneUnroutedBadges,
  setupStallNoTrafficMinutes,
  shouldShowStallBannerLine,
  unroutedAfterReconnect,
  unroutedBody,
  SETUP_STALL_NO_SAVINGS_AFTER_MS,
  SETUP_STALL_NO_SAVINGS_MIN_REQUESTS,
  SETUP_STALL_NO_TRAFFIC_AFTER_MS,
  SAVINGS_DRIFT_MIN_TOKENS_SENT,
  SAVINGS_DRIFT_WINDOW_DAYS,
} from "./setupHealthAlert";

const { invokeMock, isVisibleMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  isVisibleMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ isVisible: isVisibleMock }),
}));

function installStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      getItem: vi.fn((key: string) => values.get(key) ?? null),
      setItem: vi.fn((key: string, value: string) => {
        values.set(key, value);
      }),
      removeItem: vi.fn((key: string) => {
        values.delete(key);
      }),
    },
  });
  return values;
}

// A healthy, fully installed app that simply has nothing to show yet.
function stalledDashboard(overrides: Partial<DashboardState> = {}): DashboardState {
  return {
    ...mockDashboard,
    bootstrapComplete: true,
    pythonRuntimeInstalled: true,
    lifetimeRequests: 0,
    lifetimeEstimatedSavingsUsd: 0,
    lifetimeEstimatedTokensSaved: 0,
    ...overrides,
  };
}

// Enough traffic to rule out a quiet start, so only the savings are missing.
function busyDashboard(overrides: Partial<DashboardState> = {}): DashboardState {
  return stalledDashboard({
    lifetimeRequests: SETUP_STALL_NO_SAVINGS_MIN_REQUESTS,
    ...overrides,
  });
}

function connector(overrides: Partial<ClientConnectorStatus> = {}): ClientConnectorStatus {
  return {
    clientId: "claude_code",
    name: "Claude Code",
    installed: true,
    enabled: true,
    verified: false,
    ...overrides,
  };
}

// A connector that was configured but has never seen traffic come back. This is
// what makes the no-traffic branch fire at all.
const UNVERIFIED = [connector()];

const PAST_WINDOW = SETUP_STALL_NO_TRAFFIC_AFTER_MS + 1_000;

beforeEach(() => {
  installStorage();
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  isVisibleMock.mockReset();
  isVisibleMock.mockResolvedValue(false);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("evaluateSetupStall", () => {
  it("stays quiet before either branch's window elapses", () => {
    expect(
      evaluateSetupStall(busyDashboard(), SETUP_STALL_NO_SAVINGS_AFTER_MS - 1, {
        connectors: UNVERIFIED,
      })
    ).toBeNull();
  });

  it("stays quiet while the install is still in progress", () => {
    expect(
      evaluateSetupStall(busyDashboard({ bootstrapComplete: false }), PAST_WINDOW)
    ).toBeNull();
    expect(
      evaluateSetupStall(busyDashboard({ pythonRuntimeInstalled: false }), PAST_WINDOW)
    ).toBeNull();
  });

  it("stays quiet while the Terms gate is still blocking the app", () => {
    expect(
      evaluateSetupStall(
        busyDashboard({ requiredTermsVersion: 2, acceptedTermsVersion: 1 }),
        PAST_WINDOW
      )
    ).toBeNull();
  });

  it("stays quiet when the account gate has optimization switched off", () => {
    expect(
      evaluateSetupStall(busyDashboard(), PAST_WINDOW, { optimizationBlocked: true })
    ).toBeNull();
    // Unknown gate state (pricing status not loaded yet) must not suppress it.
    expect(
      evaluateSetupStall(busyDashboard(), PAST_WINDOW, { optimizationBlocked: undefined })
    ).not.toBeNull();
  });

  it("stays quiet once any savings are on record", () => {
    expect(
      evaluateSetupStall(busyDashboard({ lifetimeEstimatedTokensSaved: 10 }), PAST_WINDOW)
    ).toBeNull();
    // Sub-cent savings still round to 0 tokens on some paths, so the USD field
    // has to retire the alert on its own.
    expect(
      evaluateSetupStall(busyDashboard({ lifetimeEstimatedSavingsUsd: 0.004 }), PAST_WINDOW)
    ).toBeNull();
  });

  describe("no_traffic branch", () => {
    it("fires once a configured connector has never seen traffic come back", () => {
      const alert = evaluateSetupStall(stalledDashboard(), PAST_WINDOW, {
        connectors: UNVERIFIED,
      });
      expect(alert?.kind).toBe("no_traffic");
      expect(alert?.body).toContain(`${setupStallNoTrafficMinutes()} minutes`);
    });

    it("holds the full no-traffic window even though no_savings fires sooner", () => {
      expect(
        evaluateSetupStall(stalledDashboard(), SETUP_STALL_NO_TRAFFIC_AFTER_MS - 1, {
          connectors: UNVERIFIED,
        })
      ).toBeNull();
    });

    // Uptime is not the same as time spent coding, so the clock alone must
    // never be enough to accuse the setup.
    it("stays quiet when nothing is connected at all", () => {
      expect(evaluateSetupStall(stalledDashboard(), PAST_WINDOW, { connectors: [] })).toBeNull();
    });

    it("stays quiet when connector status hasn't loaded yet", () => {
      expect(evaluateSetupStall(stalledDashboard(), PAST_WINDOW, {})).toBeNull();
    });

    it("stays quiet when the connector is already verified or switched off", () => {
      expect(
        evaluateSetupStall(stalledDashboard(), PAST_WINDOW, {
          connectors: [connector({ verified: true })],
        })
      ).toBeNull();
      expect(
        evaluateSetupStall(stalledDashboard(), PAST_WINDOW, {
          connectors: [connector({ enabled: false })],
        })
      ).toBeNull();
      expect(
        evaluateSetupStall(stalledDashboard(), PAST_WINDOW, {
          connectors: [connector({ installed: false })],
        })
      ).toBeNull();
    });
  });

  describe("forceKind test override", () => {
    it("fires immediately regardless of uptime, savings, gate or connectors", () => {
      const healthy = stalledDashboard({
        lifetimeEstimatedTokensSaved: 5_000_000,
        lifetimeEstimatedSavingsUsd: 42,
        lifetimeRequests: 900,
      });
      const alert = evaluateSetupStall(healthy, 0, {
        forceKind: "no_traffic",
        optimizationBlocked: true,
        connectors: [],
      });

      expect(alert?.kind).toBe("no_traffic");
    });

    it("honours the requested branch", () => {
      expect(
        evaluateSetupStall(stalledDashboard(), 0, { forceKind: "no_savings" })?.kind
      ).toBe("no_savings");
    });

    // The override exists to exercise both surfaces. A tester almost always has
    // the tray open when the forced alert fires, and the production skip would
    // then hide the notification and make it look like there isn't one.
    it("still sends the notification when the tray window is open", async () => {
      isVisibleMock.mockResolvedValue(true);

      const alert = await maybeFireSetupStallAlert(stalledDashboard(), 0, {
        forceKind: "no_savings",
      });

      expect(alert).not.toBeNull();
      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: alert?.title,
        body: alert?.body,
        action: "setup",
      });
    });

    // Eyeballing the modal must not silence the real alert for the rest of the
    // day, so a forced fire neither reads nor writes the throttle key.
    it("never consumes the once-per-day slot", async () => {
      expect(
        await maybeFireSetupStallAlert(stalledDashboard(), 0, { forceKind: "no_traffic" })
      ).not.toBeNull();
      expect(
        await maybeFireSetupStallAlert(stalledDashboard(), 0, { forceKind: "no_traffic" })
      ).not.toBeNull();
      expect(localStorage.setItem).not.toHaveBeenCalled();

      // The genuine alert still has its slot available afterwards.
      expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    });
  });

  describe("no_savings branch", () => {
    it("fires on request volume rather than waiting out the no-traffic clock", () => {
      const alert = evaluateSetupStall(busyDashboard(), SETUP_STALL_NO_SAVINGS_AFTER_MS);
      expect(alert?.kind).toBe("no_savings");
      expect(alert?.body).toContain("none are being optimized");
    });

    it("needs no connector context, since the traffic itself is the evidence", () => {
      expect(evaluateSetupStall(busyDashboard(), PAST_WINDOW, { connectors: [] })).not.toBeNull();
    });

    it("waits for enough requests to rule out a quiet start", () => {
      expect(
        evaluateSetupStall(
          busyDashboard({ lifetimeRequests: SETUP_STALL_NO_SAVINGS_MIN_REQUESTS - 1 }),
          PAST_WINDOW
        )
      ).toBeNull();
    });
  });
});

describe("maybeFireSetupStallAlert", () => {
  it("sends a native notification and returns the alert when the tray is hidden", async () => {
    const alert = await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW);

    expect(alert?.kind).toBe("no_savings");
    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: alert?.title,
      body: alert?.body,
      action: "setup",
    });
  });

  it("carries the no_traffic branch through the same path", async () => {
    const alert = await maybeFireSetupStallAlert(stalledDashboard(), PAST_WINDOW, {
      connectors: UNVERIFIED,
    });

    expect(alert?.kind).toBe("no_traffic");
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("skips the native notification when the tray window is already open", async () => {
    isVisibleMock.mockResolvedValue(true);

    const alert = await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW);

    expect(alert).not.toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("fires at most once per local day", async () => {
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).toBeNull();
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("fires again the next local day while savings are still zero", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 0, 5, 9, 0, 0));
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();

    vi.setSystemTime(new Date(2026, 0, 6, 9, 0, 0));
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("consumes the day slot even if the notification call fails", async () => {
    invokeMock.mockRejectedValue(new Error("notification center unavailable"));

    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).toBeNull();
  });

  it("does not consume the day slot when the account gate is the cause", async () => {
    expect(
      await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW, {
        optimizationBlocked: true,
      })
    ).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
    // The slot is still available once the gate clears.
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
  });

  it("does not consume the day slot when the user simply hasn't started coding", async () => {
    expect(await maybeFireSetupStallAlert(stalledDashboard(), PAST_WINDOW)).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
    expect(
      await maybeFireSetupStallAlert(stalledDashboard(), PAST_WINDOW, {
        connectors: UNVERIFIED,
      })
    ).not.toBeNull();
  });

  it("returns null without touching storage when the stall isn't due", async () => {
    expect(await maybeFireSetupStallAlert(busyDashboard(), 1_000)).toBeNull();
    expect(localStorage.setItem).not.toHaveBeenCalled();
  });

  it("treats an unavailable localStorage as unthrottled rather than crashing", async () => {
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem: () => {
          throw new Error("blocked");
        },
        setItem: () => {
          throw new Error("blocked");
        },
        removeItem: () => {
          throw new Error("blocked");
        },
      },
    });

    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    expect(() => __resetSetupStallThrottle()).not.toThrow();
  });

  it("re-arms after the throttle is reset", async () => {
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    __resetSetupStallThrottle();
    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
  });

  it("falls back to hidden when the visibility probe rejects", async () => {
    isVisibleMock.mockRejectedValue(new Error("no window"));

    expect(await maybeFireSetupStallAlert(busyDashboard(), PAST_WINDOW)).not.toBeNull();
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });
});

describe("setupStallBannerLine", () => {
  // The banner only speaks up on a return launch: a first run is allowed to be
  // quiet because the user may simply not have opened a terminal yet.
  function returning(overrides: Partial<DashboardState> = {}): DashboardState {
    return stalledDashboard({ launchExperience: "dashboard", ...overrides });
  }

  it("stays quiet on a first run no matter how long the app has been up", () => {
    expect(setupStallBannerLine(stalledDashboard(), PAST_WINDOW)).toBeNull();
  });

  it("stays quiet during boot on a return launch", () => {
    expect(setupStallBannerLine(returning(), SETUP_STALL_NO_TRAFFIC_AFTER_MS - 1)).toBeNull();
  });

  it("names the pre-Headroom environment when no request has ever arrived", () => {
    expect(setupStallBannerLine(returning(), PAST_WINDOW)).toContain("picks up Headroom's settings");
  });

  // The modal requires an unverified connector, which keeps it silent when a
  // connector's config verifies but traffic never actually flows. The banner
  // must still speak, so a fully verified connector cannot suppress it.
  it("speaks even when every connector reports verified", () => {
    const verified = [connector({ verified: true })];
    expect(evaluateSetupStall(returning(), PAST_WINDOW, { connectors: verified })).toBeNull();
    expect(setupStallBannerLine(returning(), PAST_WINDOW, { connectors: verified })).not.toBeNull();
  });

  it("reports unoptimized traffic once enough requests have arrived", () => {
    const line = setupStallBannerLine(
      returning({ lifetimeRequests: SETUP_STALL_NO_SAVINGS_MIN_REQUESTS }),
      SETUP_STALL_NO_SAVINGS_AFTER_MS + 1_000
    );
    expect(line).toContain("none are being optimized");
  });

  it("waits for volume before blaming the setup on a couple of requests", () => {
    expect(
      setupStallBannerLine(
        returning({ lifetimeRequests: SETUP_STALL_NO_SAVINGS_MIN_REQUESTS - 1 }),
        PAST_WINDOW
      )
    ).toBeNull();
  });

  it("retires once any savings exist", () => {
    expect(
      setupStallBannerLine(returning({ lifetimeEstimatedTokensSaved: 1 }), PAST_WINDOW)
    ).toBeNull();
    expect(
      setupStallBannerLine(returning({ lifetimeEstimatedSavingsUsd: 0.01 }), PAST_WINDOW)
    ).toBeNull();
  });

  it("says nothing when a gate is what is blocking optimization", () => {
    expect(
      setupStallBannerLine(returning(), PAST_WINDOW, { optimizationBlocked: true })
    ).toBeNull();
  });

  it("says nothing while the install is still half-finished", () => {
    expect(setupStallBannerLine(returning({ bootstrapComplete: false }), PAST_WINDOW)).toBeNull();
    expect(
      setupStallBannerLine(returning({ pythonRuntimeInstalled: false }), PAST_WINDOW)
    ).toBeNull();
  });

  it("honours the debug override so the copy can be eyeballed", () => {
    expect(setupStallBannerLine(returning(), 0, { forceKind: "no_savings" })).toContain(
      "none are being optimized"
    );
  });
});

// --- drift branch: saved before, traffic still flowing, nothing optimized ---

function driftDay(daysAgo: number, overrides: Partial<DailySavingsPoint> = {}): DailySavingsPoint {
  return {
    date: new Date(Date.now() - daysAgo * 86_400_000).toISOString().slice(0, 10),
    estimatedSavingsUsd: 0,
    estimatedTokensSaved: 0,
    actualCostUsd: 1,
    totalTokensSent: SAVINGS_DRIFT_MIN_TOKENS_SENT,
    ...overrides,
  };
}

// An install that has clearly earned savings before, with a full window of
// fresh buckets showing traffic and zero optimization.
function driftedDashboard(overrides: Partial<DashboardState> = {}): DashboardState {
  return stalledDashboard({
    launchExperience: "dashboard",
    lifetimeRequests: 5_000,
    lifetimeEstimatedTokensSaved: 1_000_000,
    lifetimeEstimatedSavingsUsd: 12,
    dailySavings: [driftDay(2), driftDay(1), driftDay(0)],
    ...overrides,
  });
}

describe("drift branch", () => {
  it("fires for a saved-before install with a window of unoptimized traffic", () => {
    const alert = evaluateSetupStall(driftedDashboard(), PAST_WINDOW);
    expect(alert?.kind).toBe("drift");
    expect(alert?.title).toBe("Headroom has stopped saving");
  });

  it("stays quiet when any bucket in the window recorded savings", () => {
    const dashboard = driftedDashboard({
      dailySavings: [driftDay(2), driftDay(1), driftDay(0, { estimatedTokensSaved: 1 })],
    });
    expect(evaluateSetupStall(dashboard, PAST_WINDOW)).toBeNull();
  });

  it("stays quiet below the traffic evidence floor", () => {
    const dashboard = driftedDashboard({
      dailySavings: [
        driftDay(2, { totalTokensSent: 1_000 }),
        driftDay(1, { totalTokensSent: 1_000 }),
        driftDay(0, { totalTokensSent: 1_000 }),
      ],
    });
    expect(evaluateSetupStall(dashboard, PAST_WINDOW)).toBeNull();
  });

  it("stays quiet when the buckets are stale (user stopped coding)", () => {
    const dashboard = driftedDashboard({
      dailySavings: [driftDay(8), driftDay(7), driftDay(6)],
    });
    expect(evaluateSetupStall(dashboard, PAST_WINDOW)).toBeNull();
  });

  it("stays quiet without a full window of buckets", () => {
    const dashboard = driftedDashboard({
      dailySavings: [driftDay(1), driftDay(0)].slice(0, SAVINGS_DRIFT_WINDOW_DAYS - 1),
    });
    expect(evaluateSetupStall(dashboard, PAST_WINDOW)).toBeNull();
  });

  it("stays quiet while the account gate has optimization off", () => {
    expect(
      evaluateSetupStall(driftedDashboard(), PAST_WINDOW, { optimizationBlocked: true })
    ).toBeNull();
  });

  it("carries drift onto the Home banner line", () => {
    const line = setupStallBannerLine(driftedDashboard(), PAST_WINDOW);
    expect(line).toContain("nothing has been optimized for days");
  });

  it("keeps the banner quiet for a healthy saved-before install", () => {
    const dashboard = driftedDashboard({
      dailySavings: [driftDay(1, { estimatedTokensSaved: 500 }), driftDay(0)],
    });
    expect(setupStallBannerLine(dashboard, PAST_WINDOW)).toBeNull();
  });
});

describe("shouldShowStallBannerLine", () => {
  // The state every never-routed install is actually in: runtime fine, a
  // connector enabled, and connectorPhase pinned at "verifying" because the
  // request count never climbs. The Home banner renders that as tone
  // "starting", which is why a healthy-only gate hid the line forever.
  const verifying = {
    stallBannerLine: "No request has come through Headroom yet.",
    tone: "starting",
    connectorPhase: "verifying" as const,
    runtimeHealthy: true,
  };

  it("shows the diagnosis on an install stuck verifying", () => {
    expect(shouldShowStallBannerLine(verifying)).toBe(true);
  });

  it("shows the diagnosis on a healthy install that never saved", () => {
    expect(
      shouldShowStallBannerLine({ ...verifying, tone: "healthy", connectorPhase: "healthy" })
    ).toBe(true);
  });

  it("stays quiet when there is no line to show", () => {
    expect(shouldShowStallBannerLine({ ...verifying, stallBannerLine: null })).toBe(false);
  });

  // "Restart your terminal" is advice that cannot work while Headroom itself
  // is down, paused or still booting - those banners own the moment instead.
  it("stays quiet while Headroom itself is the problem", () => {
    expect(shouldShowStallBannerLine({ ...verifying, runtimeHealthy: false })).toBe(false);
    expect(shouldShowStallBannerLine({ ...verifying, paused: true })).toBe(false);
    expect(shouldShowStallBannerLine({ ...verifying, starting: true })).toBe(false);
  });

  it("stays quiet behind an account gate, which owns the banner", () => {
    expect(
      shouldShowStallBannerLine({ ...verifying, tone: "disabled", connectorPhase: "disabled" })
    ).toBe(false);
    expect(
      shouldShowStallBannerLine({ ...verifying, tone: "degraded", connectorPhase: "disabled" })
    ).toBe(false);
  });
});

describe("unroutedBody", () => {
  const base = { clientId: "claude_code", name: "Claude Code", activeAt: "2026-09-21T13:07:33Z" };

  it("leads with the guard's verdict when it has one", () => {
    const body = unroutedBody({
      ...base,
      enabled: true,
      reapplied: true,
      diagnosis:
        "ANTHROPIC_BASE_URL -- /repo/.claude/settings.json sets it to http://other, which overrides Headroom's route.",
    });
    expect(body).toContain("/repo/.claude/settings.json");
    // The restart advice is wrong here: the file is still there after a restart.
    expect(body).not.toContain("quit and reopen");
  });

  it("falls back to the restart ask when the guard saw nothing", () => {
    const body = unroutedBody({ ...base, enabled: true, reapplied: true });
    expect(body).toContain("quit and reopen Claude Code");
    // Says why, and that settings hold no fix (a customer searched them for weeks).
    expect(body).toContain("without going through Headroom");
    expect(body).toContain("Nothing to change in settings");
    // Names the order: "open it first, or the other way around?" (2026-10-15).
    expect(body).toContain("Always open Headroom first, then Claude Code.");
    expect(unroutedBody({ ...base, enabled: true, reapplied: true, diagnosis: null })).toBe(body);
  });

  it("asks a switched-off connector to be turned back on", () => {
    expect(unroutedBody({ ...base, enabled: false, reapplied: false })).toContain(
      "Reconnect it"
    );
  });
});

describe("unroutedAfterReconnect", () => {
  const client = (clientId: string, enabled: boolean) => ({
    clientId,
    name: clientId,
    activeAt: "2026-09-21T13:07:33Z",
    enabled,
    reapplied: enabled,
  });

  // The alert is once per day, so closing the modal on the first click left
  // every other switched-off agent unrouted until tomorrow.
  it("keeps the other switched-off agents listed after one is reconnected", () => {
    const rest = unroutedAfterReconnect(
      [client("claude_code", false), client("codex", false)],
      "claude_code"
    );
    expect(rest?.map((entry) => entry.clientId)).toEqual(["codex"]);
  });

  it("closes once no switched-off agent is left to reconnect", () => {
    expect(
      unroutedAfterReconnect([client("claude_code", false), client("codex", true)], "claude_code")
    ).toBeNull();
    expect(unroutedAfterReconnect([client("codex", false)], "codex")).toBeNull();
  });
});

describe("unrouted badges", () => {
  const finding = (clientId: string, enabled = true, diagnosis: string | null = null) => ({
    clientId,
    name: clientId === "codex" ? "Codex" : "Claude Code",
    activeAt: "2026-10-22T04:00:00Z",
    enabled,
    reapplied: enabled,
    diagnosis,
  });
  const connector = (clientId: string, requestsSinceYesterday: number): ClientConnectorStatus => ({
    clientId,
    name: clientId === "codex" ? "Codex" : "Claude Code",
    installed: true,
    enabled: true,
    verified: true,
    requestsSinceYesterday,
  });

  it("keeps earlier findings, replaces one per agent, and skips switched-off agents", () => {
    const first = finding("claude_code");
    const merged = mergeUnroutedBadges([first], [finding("codex"), finding("claude_code", false)]);
    expect(merged.map((client) => client.clientId)).toEqual(["claude_code", "codex"]);
    expect(merged[0]).toBe(first);
    const again = finding("claude_code");
    expect(mergeUnroutedBadges(merged, [again])[1]).toBe(again);
  });

  it("drops a finding once that agent's requests reach Headroom", () => {
    const current = [finding("claude_code"), finding("codex")];
    expect(
      pruneUnroutedBadges(current, [connector("claude_code", 4), connector("codex", 0)]).map(
        (client) => client.clientId
      )
    ).toEqual(["codex"]);
    // Unchanged: the same array, so the state setter does not re-render.
    expect(pruneUnroutedBadges(current, [connector("codex", 0)])).toBe(current);
  });

  it("shows the request count behind Active and the diagnosis behind an unrouted badge", () => {
    expect(connectorBadgeTip(connector("claude_code", 1420), "Active")).toBe(
      "Claude Code: Active, 1,420 requests since yesterday"
    );
    expect(connectorBadgeTip(connector("codex", 1), "Active")).toBe(
      "Codex: Active, 1 request since yesterday"
    );
    expect(connectorBadgeTip(connector("codex", 0), "Connected, no requests yet")).toBe(
      "Codex: Connected, no requests yet"
    );
    const desktop = finding("claude_code", true, "Sessions in the Claude desktop app go straight to Anthropic.");
    expect(connectorBadgeTip(connector("claude_code", 0), "Ran without Headroom", desktop)).toBe(
      "Claude Code: Sessions in the Claude desktop app go straight to Anthropic."
    );
    // A finding never overrides another verdict's text.
    expect(connectorBadgeTip(connector("claude_code", 0), "Proxy unreachable", desktop)).toBe(
      "Claude Code: Proxy unreachable"
    );
  });
});
