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
