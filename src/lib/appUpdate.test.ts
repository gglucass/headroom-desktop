import { describe, expect, it, vi } from "vitest";
import * as Sentry from "@sentry/react";

vi.mock("@sentry/react", () => ({
  captureException: vi.fn(),
}));

import type { AppUpdateConfiguration, AvailableAppUpdate } from "./types";
import {
  displayAppUpdateNotes,
  formatAppUpdateProgressCopy,
  getAppUpdateInstallStatusCopy,
  getBlockedAppUpdateCheckPatch,
  isLoudAppUpdate,
  loadAppUpdateConfiguration,
  maybeFireStaleAppUpdateNotification,
  runAppUpdateCheck,
  runAppUpdateInstall,
  sendAppUpdateNotification,
  shouldNotifyAboutAvailableAppUpdate,
  type AppUpdateProgress,
  type AppUpdateProgressListener,
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
  silentInstallSupported: false,
};

const brokenConfig: AppUpdateConfiguration = {
  enabled: false,
  currentVersion: "0.2.9",
  endpointCount: 0,
  configurationError: "HEADROOM_UPDATER_PUBLIC_KEY is missing.",
  betaChannelEnabled: false,
  silentInstallSupported: false,
};

const availableUpdate: AvailableAppUpdate = {
  currentVersion: "0.2.9",
  version: "0.3.0",
  publishedAt: "2026-04-02T12:00:00Z",
  notes: "Bug fixes.",
};

const loudUpdate: AvailableAppUpdate = {
  ...availableUpdate,
  notes: "Critical fix.\n\n<!-- headroom:loud -->",
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
      showDialog: true,
      statusCopy: "Update available: 0.3.0.",
    });
  });

  it("keeps quiet background updates out of the dialog entirely", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(availableUpdate);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: null,
      invokeFn,
    });

    expect(result).toEqual({
      availableUpdate,
      statusCopy: "Update available: 0.3.0.",
    });
  });

  it("opens the dialog for newly discovered loud background updates", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(loudUpdate);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: null,
      invokeFn,
    });

    expect(result).toEqual({
      availableUpdate: loudUpdate,
      showDialog: true,
      statusCopy: "Update available: 0.3.0.",
    });
  });

  it("keeps background checks from reopening the same loud update dialog every hour", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(loudUpdate);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: "0.3.0",
      invokeFn,
    });

    expect(result).toEqual({
      availableUpdate: loudUpdate,
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
      statusCopy: "Update available: 0.3.0.",
    });
  });

  it("ignores the staged version echoing back while an update waits for restart", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(availableUpdate);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: "0.3.0",
      stagedVersion: "0.3.0",
      invokeFn,
    });

    expect(result).toEqual({});
  });

  it("holds the staged restart state when the manifest stops offering an update", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(null);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: "0.3.0",
      stagedVersion: "0.3.0",
      invokeFn,
    });

    expect(result).toEqual({});
  });

  it("lets a newer release through while an older update is staged", async () => {
    const newerLoudUpdate: AvailableAppUpdate = { ...loudUpdate, version: "0.3.1" };
    const invokeFn = vi.fn().mockResolvedValueOnce(newerLoudUpdate);

    const result = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: "0.3.0",
      stagedVersion: "0.3.0",
      invokeFn,
    });

    expect(result).toEqual({
      availableUpdate: newerLoudUpdate,
      showDialog: true,
      statusCopy: "Update available: 0.3.1.",
    });
  });

  it("surfaces an up-to-date message for manual checks", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(null);

    const result = await runAppUpdateCheck({ invokeFn });

    expect(result).toEqual({
      availableUpdate: null,
      statusCopy: "Up to date.",
    });
  });

  it("keeps the installed version restartable when its replacement fails, then retries it", async () => {
    const newer = { ...availableUpdate, version: "0.3.1" };
    let state: { availableUpdate: AvailableAppUpdate | null; stagedVersion: string } = { availableUpdate, stagedVersion: "0.3.0" };
    state = { ...state, ...await runAppUpdateCheck({
      background: true,
      stagedVersion: state.stagedVersion,
      knownUpdateVersion: state.availableUpdate!.version,
      invokeFn: vi.fn().mockResolvedValue(newer),
    }) };
    expect(state.availableUpdate!.version).toBe("0.3.1");
    expect(state.stagedVersion).toBe("0.3.0");

    state = { ...state, ...await runAppUpdateInstall({
      availableUpdate: state.availableUpdate,
      invokeFn: vi.fn().mockRejectedValue("download failed"),
    }) };
    expect(state.stagedVersion).toBe("0.3.0");

    const retry = await runAppUpdateCheck({
      background: true,
      knownUpdateVersion: state.availableUpdate!.version,
      stagedVersion: state.stagedVersion,
      invokeFn: vi.fn().mockResolvedValue(newer),
    });
    expect(retry.availableUpdate).toEqual(newer);
    state = { ...state, ...await runAppUpdateInstall({
      availableUpdate: retry.availableUpdate!,
      invokeFn: vi.fn().mockResolvedValue(undefined),
    }) };
    expect(state.stagedVersion).toBe("0.3.1");
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

  it("notifies only for newly discovered loud background updates while the window is hidden", () => {
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate: loudUpdate,
        knownUpdateVersion: null,
        windowVisible: false,
      })
    ).toBe(true);
    // Quiet releases never fire the fresh-update notification.
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate,
        knownUpdateVersion: null,
        windowVisible: false,
      })
    ).toBe(false);
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate: loudUpdate,
        knownUpdateVersion: "0.3.0",
        windowVisible: false,
      })
    ).toBe(false);
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: true,
        availableUpdate: loudUpdate,
        knownUpdateVersion: null,
        windowVisible: true,
      })
    ).toBe(false);
    expect(
      shouldNotifyAboutAvailableAppUpdate({
        background: false,
        availableUpdate: loudUpdate,
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
      stagedVersion: "0.3.0",
      showDialog: true,
      statusCopy: "Headroom 0.3.0 is installed and ready to restart.",
    });
  });

  it("skips the dialog when a quiet install finishes in the background", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);

    const result = await runAppUpdateInstall({
      availableUpdate,
      quiet: true,
      invokeFn,
    });

    expect(result).toEqual({
      stagedVersion: "0.3.0",
      statusCopy: "Headroom 0.3.0 is installed and ready to restart.",
    });
  });

  it("detects the loud marker and strips it from displayed notes", () => {
    expect(isLoudAppUpdate(loudUpdate)).toBe(true);
    expect(isLoudAppUpdate(availableUpdate)).toBe(false);
    expect(isLoudAppUpdate(null)).toBe(false);
    expect(isLoudAppUpdate({ ...availableUpdate, notes: null })).toBe(false);
    expect(displayAppUpdateNotes(loudUpdate.notes)).toBe("Critical fix.");
    expect(displayAppUpdateNotes("Bug fixes.")).toBe("Bug fixes.");
    expect(displayAppUpdateNotes(null)).toBe("");
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

  it("replaces a dropped-download error with copy the user can act on", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce("error decoding response body");

    const result = await runAppUpdateInstall({ availableUpdate, invokeFn });

    expect(result).toEqual({
      statusCopy: "Could not download the update: the connection dropped. Try again.",
    });
  });

  it("shows the read-only-bundle refusal without reporting it (RUST-JK)", async () => {
    const readOnly =
      "Headroom cannot update itself because it is running from a read-only folder. " +
      "If you opened it straight from the disk image, drag Headroom to your " +
      "Applications folder and open it from there, then check for updates again.";
    vi.mocked(Sentry.captureException).mockClear();

    const result = await runAppUpdateInstall({
      availableUpdate,
      quiet: true,
      invokeFn: vi.fn().mockRejectedValueOnce(readOnly),
    });

    expect(result).toEqual({ statusCopy: readOnly });
    expect(Sentry.captureException).not.toHaveBeenCalled();

    await runAppUpdateInstall({
      availableUpdate,
      invokeFn: vi.fn().mockRejectedValueOnce("permission denied"),
    });
    expect(Sentry.captureException).toHaveBeenCalledTimes(1);
  });

  it("re-checks and retries once when the staged update was already consumed", async () => {
    const invokeFn = vi
      .fn()
      .mockRejectedValueOnce(
        "The downloaded update is no longer staged. Check for updates again, then retry."
      )
      .mockResolvedValueOnce(availableUpdate)
      .mockResolvedValueOnce(undefined);

    const result = await runAppUpdateInstall({ availableUpdate, invokeFn });

    expect(invokeFn.mock.calls.map((call) => call[0])).toEqual([
      "install_app_update",
      "check_for_app_update",
      "install_app_update",
    ]);
    expect(result).toEqual({
      stagedVersion: "0.3.0",
      showDialog: true,
      statusCopy: "Headroom 0.3.0 is installed and ready to restart.",
    });
  });

  it("keeps the stale-slot error when the re-check no longer offers that version", async () => {
    const staleError =
      "The downloaded update is no longer staged. Check for updates again, then retry.";
    const invokeFn = vi
      .fn()
      .mockRejectedValueOnce(staleError)
      .mockResolvedValueOnce({ ...availableUpdate, version: "0.4.0" });

    const result = await runAppUpdateInstall({ availableUpdate, invokeFn });

    expect(invokeFn).toHaveBeenCalledTimes(2);
    expect(result).toEqual({ statusCopy: staleError });
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

  it("formats download progress with byte counts and percent when total is known", () => {
    expect(
      formatAppUpdateProgressCopy("0.3.0", {
        phase: "downloading",
        downloaded: 5_500_000,
        total: 22_000_000,
      })
    ).toBe("Downloading Headroom 0.3.0: 5.5 MB of 22.0 MB (25%)…");
  });

  it("formats download progress without a percent when total is unknown", () => {
    expect(
      formatAppUpdateProgressCopy("0.3.0", {
        phase: "downloading",
        downloaded: 1_200_000,
        total: null,
      })
    ).toBe("Downloading Headroom 0.3.0: 1.2 MB…");
  });

  it("formats the installing phase as a separate copy string", () => {
    expect(
      formatAppUpdateProgressCopy("0.3.0", { phase: "installing" })
    ).toBe("Installing Headroom 0.3.0…");
  });

  it("does not subscribe to progress events when no onProgress callback is given", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);
    const listenFn = vi.fn();

    await runAppUpdateInstall({ availableUpdate, invokeFn, listenFn });

    expect(listenFn).not.toHaveBeenCalled();
  });

  it("forwards progress events to onProgress and unsubscribes after install resolves", async () => {
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);
    const unlisten = vi.fn();
    type EmittedHandler = Parameters<AppUpdateProgressListener>[1];
    const handlerRef: { current: EmittedHandler | null } = { current: null };
    const listenFn: AppUpdateProgressListener = vi.fn(async (_event, handler) => {
      handlerRef.current = handler;
      return unlisten;
    });
    const onProgress = vi.fn();

    const installPromise = runAppUpdateInstall({
      availableUpdate,
      invokeFn,
      listenFn,
      onProgress,
    });

    await Promise.resolve();
    expect(listenFn).toHaveBeenCalledTimes(1);
    expect((listenFn as ReturnType<typeof vi.fn>).mock.calls[0]?.[0]).toBe(
      "app-update://progress"
    );

    handlerRef.current?.({
      event: "app-update://progress",
      id: 1,
      payload: { phase: "downloading", downloaded: 100, total: 500 },
    });
    handlerRef.current?.({
      event: "app-update://progress",
      id: 2,
      payload: { phase: "installing" },
    });

    await installPromise;

    expect(onProgress).toHaveBeenCalledTimes(2);
    expect(onProgress.mock.calls[0]?.[0]).toEqual({
      phase: "downloading",
      downloaded: 100,
      total: 500,
    });
    expect(onProgress.mock.calls[1]?.[0]).toEqual({ phase: "installing" });
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("unsubscribes from progress events even when install fails", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce("permission denied");
    const unlisten = vi.fn();
    const listenFn: AppUpdateProgressListener = vi.fn(async () => unlisten);

    await runAppUpdateInstall({
      availableUpdate,
      invokeFn,
      listenFn,
      onProgress: vi.fn(),
    });

    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("best-effort sends update notifications without surfacing delivery failures", async () => {
    const invokeFn = vi.fn().mockRejectedValueOnce(new Error("notifications disabled"));

    await expect(sendAppUpdateNotification("0.3.0", invokeFn)).resolves.toBeUndefined();
    expect(invokeFn).toHaveBeenCalledWith("show_app_update_notification", { version: "0.3.0" });
  });
});

describe("maybeFireStaleAppUpdateNotification", () => {
  function waiting(days: number, extra: Record<string, unknown> = {}) {
    return {
      headroom_update_waiting: JSON.stringify({
        onVersion: "0.2.9",
        since: Date.now() - days * 24 * 60 * 60 * 1000,
        ...extra,
      }),
    };
  }

  it("fires once this build has had an update waiting for 5 days", async () => {
    installStorage(waiting(6));
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);

    await maybeFireStaleAppUpdateNotification(availableUpdate, invokeFn);

    expect(invokeFn).toHaveBeenCalledWith("show_notification", {
      title: "Headroom update waiting",
      body: expect.stringContaining("0.3.0"),
      action: "update",
    });
    expect(JSON.parse(localStorage.getItem("headroom_update_waiting") as string)).toMatchObject({
      onVersion: "0.2.9",
      notified: true,
    });
  });

  it("fires on a fortnight-old build even when the release on offer is brand new", async () => {
    // The regression this replaced: publish-date gating made every fresh
    // release reset the clock, so the users furthest behind were never nagged.
    installStorage(waiting(14));
    const invokeFn = vi.fn().mockResolvedValueOnce(undefined);

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: daysAgo(0) },
      invokeFn
    );

    expect(invokeFn).toHaveBeenCalledOnce();
  });

  it("starts the clock on first sighting instead of firing", async () => {
    const values = installStorage();
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, publishedAt: daysAgo(30) },
      invokeFn
    );

    expect(invokeFn).not.toHaveBeenCalled();
    expect(JSON.parse(values.get("headroom_update_waiting") as string)).toMatchObject({
      onVersion: "0.2.9",
    });
  });

  it("does not fire while the update has waited less than 5 days", async () => {
    installStorage(waiting(3));
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(availableUpdate, invokeFn);

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("does not fire twice on the same build, whatever version is offered", async () => {
    installStorage(waiting(10, { notified: true }));
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification({ ...availableUpdate, version: "0.4.0" }, invokeFn);

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("restarts the clock once the user is running a newer build", async () => {
    const values = installStorage(waiting(10, { notified: true }));
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(
      { ...availableUpdate, currentVersion: "0.3.0", version: "0.4.0" },
      invokeFn
    );

    expect(invokeFn).not.toHaveBeenCalled();
    const restarted = JSON.parse(values.get("headroom_update_waiting") as string);
    expect(restarted).toMatchObject({ onVersion: "0.3.0" });
    expect(restarted.notified).toBeUndefined();
  });

  it("is a no-op when there is no available update", async () => {
    installStorage();
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(null, invokeFn);

    expect(invokeFn).not.toHaveBeenCalled();
  });

  it("restarts the clock on a corrupt record instead of throwing", async () => {
    const values = installStorage({ headroom_update_waiting: "{not json" });
    const invokeFn = vi.fn();

    await maybeFireStaleAppUpdateNotification(availableUpdate, invokeFn);

    expect(invokeFn).not.toHaveBeenCalled();
    expect(JSON.parse(values.get("headroom_update_waiting") as string)).toMatchObject({
      onVersion: "0.2.9",
    });
  });

  it("swallows invoke errors without throwing", async () => {
    installStorage(waiting(6));
    const invokeFn = vi.fn().mockRejectedValueOnce(new Error("notifications disabled"));

    await expect(
      maybeFireStaleAppUpdateNotification(availableUpdate, invokeFn)
    ).resolves.toBeUndefined();
  });
});
