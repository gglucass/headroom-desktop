import { describe, expect, it, vi } from "vitest";

import type { AppUpdateConfiguration, AvailableAppUpdate } from "./types";
import {
  getAppUpdateInstallStatusCopy,
  getBlockedAppUpdateCheckPatch,
  loadAppUpdateConfiguration,
  maybeFireStaleAppUpdateNotification,
  runAppUpdateCheck,
  runAppUpdateInstall,
  sendAppUpdateNotification,
  shouldNotifyAboutAvailableAppUpdate,
} from "./appUpdate";

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

function daysAgo(n: number): string {
  return new Date(Date.now() - n * 24 * 60 * 60 * 1000).toISOString();
}

const disabledConfig: AppUpdateConfiguration = {
  enabled: false,
  currentVersion: "0.2.9",
  endpointCount: 0,
  configurationError: null,
  betaChannelEnabled: false,
};

const brokenConfig: AppUpdateConfiguration = {
  enabled: false,
  currentVersion: "0.2.9",
  endpointCount: 0,
  configurationError: "HEADROOM_UPDATER_PUBLIC_KEY is missing.",
  betaChannelEnabled: false,
};

const availableUpdate: AvailableAppUpdate = {
  currentVersion: "0.2.9",
  version: "0.3.0",
  publishedAt: "2026-04-02T12:00:00Z",
  notes: "Bug fixes.",
};

describe("app update helpers", () => {
  it("loads update configuration and surfaces config errors as status copy", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(brokenConfig);

    const result = await loadAppUpdateConfiguration(invokeFn);

    expect(invokeFn).toHaveBeenCalledWith("get_app_update_configuration");
    expect(result).toEqual({
      config: brokenConfig,
      statusCopy: "HEADROOM_UPDATER_PUBLIC_KEY is missing.",
    });
  });

  it("formats configuration load failures with the shared invoke error helper", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce({ error: "bridge offline" });

    const result = await loadAppUpdateConfiguration(invokeFn);

    expect(result).toEqual({
      statusCopy: "bridge offline",
    });
  });

  it("returns a visible manual-check message when updates are disabled", () => {
    expect(getBlockedAppUpdateCheckPatch(disabledConfig)).toEqual({
      statusCopy: "Update checks are not configured in this build yet.",
    });
  });

  it("suppresses background-check copy when configuration is invalid", () => {
    expect(getBlockedAppUpdateCheckPatch(brokenConfig, true)).toEqual({});
  });

  it("marks an available update as ready to install and opens the dialog", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(availableUpdate);

    const result = await runAppUpdateCheck({ invokeFn });

    expect(invokeFn).toHaveBeenCalledWith("check_for_app_update");
    expect(result).toEqual({
      availableUpdate,
      readyToRestart: false,
      showDialog: true,
      statusCopy: "Update available: 0.3.0.",
    });
  });

  it("keeps background checks from reopening the same update dialog every hour", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(availableUpdate);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: "0.3.0",
      invokeFn,
    });

    expect(result).toEqual({
      availableUpdate,
      readyToRestart: false,
      statusCopy: "Update available: 0.3.0.",
    });
  });

  it("surfaces an up-to-date message for manual checks", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(null);

    const result = await runAppUpdateCheck({ invokeFn });

    expect(result).toEqual({
      availableUpdate: null,
      readyToRestart: false,
      statusCopy: "Up to date.",
    });
  });

  it("suppresses background check errors instead of overwriting status copy", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce(new Error("feed unavailable"));

    const result = await runAppUpdateCheck({ background: true, invokeFn });

    expect(result).toEqual({});
  });

  it("surfaces manual check errors with invoke-style fallback parsing", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce({ message: "timed out" });

    const result = await runAppUpdateCheck({ invokeFn });

    expect(result).toEqual({
      statusCopy: "timed out",
    });
  });

  it("notifies only for newly discovered background updates while the window is hidden", () => {
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate,
        knownUpdateVersion: null,
        windowVisible: false,
      })
    ).toBe(true);
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate,
        knownUpdateVersion: "0.3.0",
        windowVisible: false,
      })
    ).toBe(false);
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate,
        knownUpdateVersion: null,
        windowVisible: true,
      })
    ).toBe(false);
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: false,
        availableUpdate,
        knownUpdateVersion: null,
        windowVisible: false,
      })
    ).toBe(false);
  });

  it("returns the install progress copy for the selected update", () => {
    expect(getAppUpdateInstallStatusCopy(availableUpdate)).toBe("Downloading Headroom 0.3.0…");
    expect(getAppUpdateInstallStatusCopy(null)).toBeNull();
  });

  it("marks updates as ready to restart after a successful install", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);

    const result = await runAppUpdateInstall({
      availableUpdate,
      invokeFn,
    });

    expect(invokeFn).toHaveBeenCalledWith("install_app_update");
    expect(result).toEqual({
      readyToRestart: true,
      showDialog: true,
      statusCopy: "Headroom 0.3.0 is installed and ready to restart.",
    });
  });

  it("surfaces install errors without mutating update state", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce("permission denied");

    const result = await runAppUpdateInstall({
      availableUpdate,
      invokeFn,
    });

    expect(result).toEqual({
      statusCopy: "permission denied",
    });
  });

  it("returns an empty patch when install is requested without an update", async () => {
    const invokeFn = vi.fn();

    const result = await runAppUpdateInstall({
      availableUpdate: null,
      invokeFn,
    });

    expect(invokeFn).not.toHaveBeenCalled();
    expect(result).toEqual({});
  });

  it("best-effort sends update notifications without surfacing delivery failures", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce(new Error("notifications disabled"));

    await expect(sendAppUpdateNotification("0.3.0", invokeFn)).resolves.toBeUndefined();
    expect(invokeFn).toHaveBeenCalledWith("show_app_update_notification", { version: "0.3.0" });
  });
});

describe("maybeFireStaleAppUpdateNotification", () => {
  it("fires when the update is at least 5 days old and has not been notified", async () => {
    installStorage();
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: daysAgo(6) },
      invokeFn
    );

    expect(invokeFn).toHaveBeenCalledWith("show_notification", {
      title: "Headroom update waiting",
      body: expect.stringContaining("0.3.0"),
      action: "update",
    });
    expect(localStorage.setItem).toHaveBeenCalledWith(
      "headroom_stale_update_notified_version",
      "0.3.0"
    );
  });

  it("does not fire when the update is fresher than 5 days", async () => {
    installStorage();
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: daysAgo(3) },
      invokeFn
    );

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("does not fire twice for the same version", async () => {
    installStorage({ headroom_stale_update_notified_version: "0.3.0" });
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: daysAgo(10) },
      invokeFn
    );

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("fires again for a new version even if a previous one was notified", async () => {
    installStorage({ headroom_stale_update_notified_version: "0.3.0" });
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, version: "0.4.0", publishedAt: daysAgo(6) },
      invokeFn
    );

    expect(invokeFn).toHaveBeenCalledOnce();
  });

  it("is a no-op when there is no available update", async () => {
    installStorage();
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(null, invokeFn);

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("is a no-op when publishedAt is missing or malformed", async () => {
    installStorage();
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: null },
      invokeFn
    );
    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: "not-a-date" },
      invokeFn
    );

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("swallows invoke errors without throwing", async () => {
    installStorage();
    const invokeFn = vi.fn().mockRejectedValueOnce(new Error("notifications disabled"));

    await expect(
      maybeFireStaleAppUpdateNotification(
        { ...availableUpdate, publishedAt: daysAgo(6) },
        invokeFn
      )
    ).resolves.toBeUndefined();
  });
});
