import { afterEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
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

function installThrowingStorage() {
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      getItem: vi.fn(() => {
        throw new Error("storage unavailable");
      }),
      setItem: vi.fn(() => {
        throw new Error("storage unavailable");
      }),
    },
  });
}

describe("analytics helpers", () => {
  afterEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    vi.resetModules();
    Reflect.deleteProperty(globalThis, "localStorage");
  });

  it("tracks analytics events and swallows invoke failures", async () => {
    invokeMock.mockReset();
    invokeMock.mockRejectedValueOnce(new Error("bridge offline"));

    const { trackAnalyticsEvent } = await import("./analytics");

    trackAnalyticsEvent("dashboard_opened", { source: "tray", count: 2 });
    await Promise.resolve();

    expect(invokeMock).toHaveBeenCalledWith("track_analytics_event", {
      name: "dashboard_opened",
      properties: { source: "tray", count: 2 },
    });
  });

  it("records install milestones only once per name", async () => {
    invokeMock.mockResolvedValue(undefined);

    const values = installStorage();
    const { trackInstallMilestoneOnce } = await import("./analytics");

    trackInstallMilestoneOnce("desktop_setup_complete", { client: "claude_code" });
    trackInstallMilestoneOnce("desktop_setup_complete", { client: "claude_code" });

    expect(values.get("headroom.analytics.install.desktop_setup_complete")).toBe("1");
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("track_analytics_event", {
      name: "desktop_setup_complete",
      properties: { client: "claude_code" },
    });
  });

  it("skips tracking milestones that were already persisted", async () => {
    invokeMock.mockResolvedValue(undefined);

    installStorage({
      "headroom.analytics.install.desktop_setup_complete": "1",
    });

    const { trackInstallMilestoneOnce } = await import("./analytics");

    trackInstallMilestoneOnce("desktop_setup_complete");

    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("still de-dupes milestones when local storage is unavailable", async () => {
    invokeMock.mockResolvedValue(undefined);

    installThrowingStorage();
    const { trackInstallMilestoneOnce } = await import("./analytics");

    trackInstallMilestoneOnce("runtime_bootstrap_started");
    trackInstallMilestoneOnce("runtime_bootstrap_started");

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("track_analytics_event", {
      name: "runtime_bootstrap_started",
      properties: undefined,
    });
  });
});
