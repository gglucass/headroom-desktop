import { afterEach, describe, expect, it, vi } from "vitest";

import type { HeadroomPricingStatus, RuntimeStatus } from "./types";
import {
  maybeFireUrgentPricingNotifications,
  maybeFireUrgentRuntimeNotification,
} from "./urgentNotifications";

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
    },
  });
  return values;
}

function makePricing(
  overrides: Partial<HeadroomPricingStatus> = {}
): HeadroomPricingStatus {
  return {
    authenticated: true,
    localGraceStartedAt: new Date().toISOString(),
    localGraceEndsAt: new Date().toISOString(),
    localGraceActive: false,
    accountSyncError: null,
    needsAuthentication: false,
    optimizationAllowed: true,
    shouldNudge: false,
    gateReason: null,
    gateMessage: "",
    nudgeThresholdPercent: null,
    disableThresholdPercent: null,
    effectiveDisableThresholdPercent: null,
    recommendedSubscriptionTier: null,
    recommendedSubscriptionPriceUsd: null,
    claude: {
      authMethod: "claude_ai_oauth",
      email: null,
      displayName: null,
      planTier: "free",
      hasExtraUsageEnabled: false,
    },
    account: null,
    launchDiscountActive: false,
    ...overrides,
  };
}

function makeRuntime(overrides: Partial<RuntimeStatus> = {}): RuntimeStatus {
  return {
    platform: "darwin",
    supportTier: "supported",
    installed: true,
    running: true,
    starting: false,
    paused: false,
    proxyReachable: true,
    headroomLearnSupported: true,
    rtk: {
      installed: true,
      pathConfigured: true,
      hookConfigured: true,
    },
    ...overrides,
  };
}

describe("maybeFireUrgentPricingNotifications", () => {
  afterEach(() => {
    invokeMock.mockReset();
    isVisibleMock.mockReset();
  });

  it("does not fire when the window is visible", async () => {
    isVisibleMock.mockResolvedValue(true);
    installStorage();

    await maybeFireUrgentPricingNotifications(
      makePricing({ needsAuthentication: true })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("treats isVisible failures as visible to avoid spamming", async () => {
    isVisibleMock.mockRejectedValue(new Error("bridge down"));
    installStorage();

    await maybeFireUrgentPricingNotifications(
      makePricing({ needsAuthentication: true })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("fires the needs-auth notification with the signin action", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentPricingNotifications(
      makePricing({ needsAuthentication: true, gateMessage: "Sign in required." })
    );

    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: "Headroom needs you to sign in",
      body: "Sign in required.",
      action: "signin",
    });
  });

  it("falls back to default copy when gateMessage is empty", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentPricingNotifications(
      makePricing({ needsAuthentication: true, gateMessage: "" })
    );

    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: "Headroom needs you to sign in",
      body: "Sign in to Headroom to keep optimization running.",
      action: "signin",
    });
  });

  it("fires the optimization-blocked notification when the plan gate is on", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentPricingNotifications(
      makePricing({
        optimizationAllowed: false,
        gateMessage: "Plan does not allow optimization.",
      })
    );

    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: "Headroom optimization is off",
      body: "Plan does not allow optimization.",
      action: "billing",
    });
  });

  it("prefers needs-auth over the plan gate when both are active", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentPricingNotifications(
      makePricing({
        needsAuthentication: true,
        optimizationAllowed: false,
      })
    );

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith(
      "show_notification",
      expect.objectContaining({ action: "signin" })
    );
  });

  it("does not repeat a notification already fired today", async () => {
    isVisibleMock.mockResolvedValue(false);
    const today = new Date().toISOString().slice(0, 10);
    installStorage({ headroom_urgent_needs_auth_date: today });

    await maybeFireUrgentPricingNotifications(
      makePricing({ needsAuthentication: true })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("records today's date after sending", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();
    const today = new Date().toISOString().slice(0, 10);

    await maybeFireUrgentPricingNotifications(
      makePricing({ needsAuthentication: true })
    );

    expect(localStorage.setItem).toHaveBeenCalledWith(
      "headroom_urgent_needs_auth_date",
      today
    );
  });

  it("swallows invoke errors without throwing", async () => {
    isVisibleMock.mockResolvedValue(false);
    invokeMock.mockRejectedValueOnce(new Error("notifications disabled"));
    installStorage();

    await expect(
      maybeFireUrgentPricingNotifications(
        makePricing({ needsAuthentication: true })
      )
    ).resolves.toBeUndefined();
  });

  it("does not fire when pricing is healthy", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentPricingNotifications(makePricing());

    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("maybeFireUrgentRuntimeNotification", () => {
  afterEach(() => {
    invokeMock.mockReset();
    isVisibleMock.mockReset();
  });

  it("fires when the runtime is installed but not running", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ running: false })
    );

    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: "Headroom stopped running",
      body: "Headroom isn't running. Open the tray to restart it.",
      action: "runtime",
    });
  });

  it("surfaces the startup error when one is present", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ running: false, startupError: "port 6767 busy" })
    );

    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: "Headroom stopped running",
      body: "Headroom isn't running: port 6767 busy",
      action: "runtime",
    });
  });

  it("prefers the resolution hint over the raw startup error", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({
        running: false,
        startupError: "never opened port 6768 within 60000ms",
        startupErrorHint: "Wait a moment and click Retry.",
      })
    );

    expect(invokeMock).toHaveBeenCalledWith("show_notification", {
      title: "Headroom stopped running",
      body: "Headroom isn't running. Wait a moment and click Retry.",
      action: "runtime",
    });
  });

  it("does not fire while the runtime is starting", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ running: false, starting: true })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("does not fire while the runtime is paused", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ running: false, paused: true })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("does not fire when the runtime isn't installed", async () => {
    isVisibleMock.mockResolvedValue(false);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ installed: false, running: false })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("does not fire while the window is visible", async () => {
    isVisibleMock.mockResolvedValue(true);
    installStorage();

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ running: false })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("does not repeat within the same day", async () => {
    isVisibleMock.mockResolvedValue(false);
    const today = new Date().toISOString().slice(0, 10);
    installStorage({ headroom_urgent_runtime_down_date: today });

    await maybeFireUrgentRuntimeNotification(
      makeRuntime({ running: false })
    );

    expect(invokeMock).not.toHaveBeenCalled();
  });
});
