import { invoke } from "@tauri-apps/api/core";
import * as Sentry from "@sentry/react";

import { describeInvokeError } from "./appHelpers";
import type { AppUpdateConfiguration, AvailableAppUpdate } from "./types";

export type AppUpdateInvoker = <T>(
  command: string,
  args?: Record<string, unknown>
) => Promise<T>;

export interface AppUpdateStatePatch {
  config?: AppUpdateConfiguration;
  availableUpdate?: AvailableAppUpdate | null;
  readyToRestart?: boolean;
  showDialog?: boolean;
  statusCopy?: string | null;
}

export async function loadAppUpdateConfiguration(
  invokeFn: AppUpdateInvoker = invoke
): Promise<AppUpdateStatePatch> {
  try {
    const config = await invokeFn<AppUpdateConfiguration>("get_app_update_configuration");
    return {
      config,
      ...(config.configurationError ? { statusCopy: config.configurationError } : {}),
    };
  } catch (error) {
    return {
      statusCopy: describeInvokeError(error, "Could not load app update settings."),
    };
  }
}

export function getBlockedAppUpdateCheckPatch(
  config: AppUpdateConfiguration,
  background = false
): AppUpdateStatePatch | null {
  if (config.configurationError) {
    return background ? {} : { statusCopy: config.configurationError };
  }

  if (!config.enabled) {
    return background ? {} : { statusCopy: "Update checks are not configured in this build yet." };
  }

  return null;
}

export async function runAppUpdateCheck({
  background = false,
  knownUpdateVersion = null,
  invokeFn = invoke,
}: {
  background?: boolean;
  knownUpdateVersion?: string | null;
  invokeFn?: AppUpdateInvoker;
} = {}): Promise<AppUpdateStatePatch> {
  try {
    const update = await invokeFn<AvailableAppUpdate | null>("check_for_app_update");

    if (update) {
      const shouldShowDialog = !background || update.version !== knownUpdateVersion;
      return {
        availableUpdate: update,
        readyToRestart: false,
        ...(shouldShowDialog ? { showDialog: true } : {}),
        statusCopy: `Update available: ${update.version}.`,
      };
    }

    return {
      availableUpdate: null,
      readyToRestart: false,
      ...(background ? {} : { statusCopy: "Up to date." }),
    };
  } catch (error) {
    if (background) {
      Sentry.captureException(error, { tags: { flow: "app_update_check" } });
      return {};
    }
    return {
      statusCopy: describeInvokeError(error, "Could not check for updates."),
    };
  }
}

export function shouldNotifyAboutAvailableAppUpdate({
  background,
  availableUpdate,
  knownUpdateVersion,
  windowVisible,
}: {
  background: boolean;
  availableUpdate?: AvailableAppUpdate | null;
  knownUpdateVersion?: string | null;
  windowVisible: boolean;
}): boolean {
  if (!background || windowVisible || !availableUpdate) {
    return false;
  }

  return availableUpdate.version !== knownUpdateVersion;
}

export async function sendAppUpdateNotification(
  version: string,
  invokeFn: AppUpdateInvoker = invoke
): Promise<void> {
  try {
    await invokeFn("show_app_update_notification", { version });
  } catch {
    // Notification delivery is best-effort so update checks still succeed.
  }
}

const STALE_UPDATE_NOTIFIED_KEY = "headroom_stale_update_notified_version";
const STALE_UPDATE_THRESHOLD_DAYS = 5;

// Fire a nag notification when an available update has been published
// for at least 5 days and the user hasn't installed it. Deduped per version.
export async function maybeFireStaleAppUpdateNotification(
  availableUpdate: AvailableAppUpdate | null,
  invokeFn: AppUpdateInvoker = invoke
): Promise<void> {
  if (!availableUpdate?.publishedAt) return;

  const publishedMs = Date.parse(availableUpdate.publishedAt);
  if (Number.isNaN(publishedMs)) return;

  const ageDays = (Date.now() - publishedMs) / (24 * 60 * 60 * 1000);
  if (ageDays < STALE_UPDATE_THRESHOLD_DAYS) return;

  if (localStorage.getItem(STALE_UPDATE_NOTIFIED_KEY) === availableUpdate.version) {
    return;
  }

  try {
    await invokeFn("show_notification", {
      title: "Headroom update waiting",
      body: `Headroom ${availableUpdate.version} has been out for ${Math.floor(
        ageDays
      )} days. Open Headroom to install it.`,
      action: "update",
    });
    localStorage.setItem(STALE_UPDATE_NOTIFIED_KEY, availableUpdate.version);
  } catch {
    // best-effort
  }
}

export function getAppUpdateInstallStatusCopy(
  availableUpdate: AvailableAppUpdate | null
): string | null {
  return availableUpdate ? `Downloading Headroom ${availableUpdate.version}…` : null;
}

export async function runAppUpdateInstall({
  availableUpdate,
  invokeFn = invoke,
}: {
  availableUpdate: AvailableAppUpdate | null;
  invokeFn?: AppUpdateInvoker;
}): Promise<AppUpdateStatePatch> {
  if (!availableUpdate) {
    return {};
  }

  try {
    await invokeFn("install_app_update");
    return {
      readyToRestart: true,
      showDialog: true,
      statusCopy: `Headroom ${availableUpdate.version} is installed and ready to restart.`,
    };
  } catch (error) {
    Sentry.captureException(error, { tags: { flow: "app_update_install" } });
    return {
      statusCopy: describeInvokeError(error, "Could not install the update."),
    };
  }
}
