import { invoke } from "@tauri-apps/api/core";
import { listen, type Event, type UnlistenFn } from "@tauri-apps/api/event";
import * as Sentry from "@sentry/react";

import { describeInvokeError } from "./appHelpers";
import type { AppUpdateConfiguration, AvailableAppUpdate } from "./types";

export type AppUpdateInvoker = <T>(
  command: string,
  args?: Record<string, unknown>
) => Promise<T>;

export type AppUpdateProgress =
  | { phase: "downloading"; downloaded: number; total: number | null }
  | { phase: "installing" };

export type AppUpdateProgressListener = (
  event: string,
  handler: (event: Event<AppUpdateProgress>) => void
) => Promise<UnlistenFn>;

const APP_UPDATE_PROGRESS_EVENT = "app-update://progress";

// Matches the backend's empty-slot error (lib.rs `install_pending_update`).
const STALE_STAGED_UPDATE = /no longer staged/i;

// install_app_update's refusal when the app runs off the DMG or an
// App-Translocated copy (READ_ONLY_BUNDLE_MESSAGE in lib.rs). The message is
// the user's fix, not a defect to report (RUST-JK).
const READ_ONLY_BUNDLE = /running from a read-only folder/i;

// Anything that failed on the way to or from github.com rather than in our
// code: the user's network, not a defect. Covers the manifest fetch (RUST-GM,
// RUST-GW) and the bundle download the install runs (RUST-HS, a reqwest
// "error decoding response body" = the body stopped arriving mid-transfer).
const TRANSPORT_FAILURE =
  /error sending request|error decoding response body|timed out|dns error|connection|valid release JSON/i;

// Releases are quiet by default: no dialog, no notification, and (on macOS)
// a silent background install that only asks for a restart. A release that
// users must take promptly opts back into the old loud flow by carrying this
// marker anywhere in its release notes (put `<!-- headroom:loud -->` in
// .github/release-notes/<VERSION>.md; it flows into latest.json `notes`).
export const LOUD_UPDATE_MARKER = "headroom:loud";

export function isLoudAppUpdate(update: AvailableAppUpdate | null | undefined): boolean {
  return update?.notes?.includes(LOUD_UPDATE_MARKER) ?? false;
}

export function displayAppUpdateNotes(notes: string | null | undefined): string {
  if (!notes) {
    return "";
  }
  return notes.replace(/<!--[^>]*headroom:loud[^>]*-->/g, "").trim();
}

export interface AppUpdateStatePatch {
  config?: AppUpdateConfiguration;
  availableUpdate?: AvailableAppUpdate | null;
  stagedVersion?: string;
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
  stagedVersion = null,
  invokeFn = invoke,
}: {
  background?: boolean;
  knownUpdateVersion?: string | null;
  stagedVersion?: string | null;
  invokeFn?: AppUpdateInvoker;
} = {}): Promise<AppUpdateStatePatch> {
  try {
    const update = await invokeFn<AvailableAppUpdate | null>("check_for_app_update");

    // A staged update leaves the running process reporting its *old* version,
    // so the backend keeps offering the build we already installed. Swallow
    // that echo (installing it again would re-download every hour) and hold
    // the "restart to finish" state if the manifest stops offering anything.
    // A genuinely newer version still falls through, so a loud hotfix reaches
    // users who have not restarted yet instead of waiting behind the staged
    // one.
    if (stagedVersion && (!update || update.version === stagedVersion)) {
      return {};
    }

    if (update) {
      // Background-found updates only interrupt when the release is marked
      // loud; quiet releases surface passively (Settings copy, stale nag,
      // and on macOS a silent install). Manual checks always show the dialog.
      const shouldShowDialog =
        !background || (isLoudAppUpdate(update) && update.version !== knownUpdateVersion);
      return {
        availableUpdate: update,
        ...(shouldShowDialog ? { showDialog: true } : {}),
        statusCopy: `Update available: ${update.version}.`,
      };
    }

    return {
      availableUpdate: null,
      ...(background ? {} : { statusCopy: "Up to date." }),
    };
  } catch (error) {
    if (background) {
      // A transport failure reaching github.com is the user's network
      // (RUST-GM), and "Could not fetch a valid release JSON" is github.com
      // answering with something other than latest.json (RUST-GW: two hosts
      // on two OSes in the same minute), not a defect; keep both visible but
      // below Error.
      const transport = TRANSPORT_FAILURE.test(describeInvokeError(error, ""));
      Sentry.captureException(error, {
        level: transport ? "warning" : "error",
        tags: { flow: "app_update_check" },
      });
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

  // Quiet releases never fire the "new version" notification; the 5-day
  // stale nag remains the fallback on platforms without silent install.
  if (!isLoudAppUpdate(availableUpdate)) {
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

const STALE_UPDATE_WAITING_KEY = "headroom_update_waiting";
const STALE_UPDATE_THRESHOLD_DAYS = 5;

interface WaitingUpdateRecord {
  // The build that was RUNNING when the clock started, not the one on offer.
  onVersion: string;
  since: number;
  notified?: boolean;
}

function readWaitingUpdate(onVersion: string): WaitingUpdateRecord | null {
  try {
    const raw = localStorage.getItem(STALE_UPDATE_WAITING_KEY);
    if (!raw) return null;
    const record = JSON.parse(raw) as WaitingUpdateRecord;
    // A record left by an older build is spent: upgrading is the exact thing
    // the nag asks for, so the clock restarts on whatever is running now.
    if (record?.onVersion !== onVersion || !Number.isFinite(record.since)) return null;
    return record;
  } catch {
    return null;
  }
}

function writeWaitingUpdate(record: WaitingUpdateRecord): void {
  try {
    localStorage.setItem(STALE_UPDATE_WAITING_KEY, JSON.stringify(record));
  } catch {
    // Storage is best-effort; a failed write just restarts the clock.
  }
}

// Fire a nag notification when THIS install has had an update waiting for at
// least 5 days, deduped per running build.
//
// It used to measure the age of the newest release instead, which our cadence
// made unfireable: a user sitting on 0.9.15 for a fortnight is offered 0.9.19,
// published yesterday, so `ageDays < 5` and nothing ever fired. Every release
// reset the clock for everyone, including the people furthest behind (gaps
// between 0.9.15 and 0.9.19 were 7, 4, 0 and 1 days). Since the quiet-update
// default landed in 0.9.8 this nag is the only thing that reaches Windows and
// Linux at all -- neither can install without the user, so a quiet release is
// otherwise invisible there.
export async function maybeFireStaleAppUpdateNotification(
  availableUpdate: AvailableAppUpdate | null,
  invokeFn: AppUpdateInvoker = invoke
): Promise<void> {
  if (!availableUpdate?.currentVersion) return;

  const onVersion = availableUpdate.currentVersion;
  const record = readWaitingUpdate(onVersion);

  // First sighting on this build starts the clock; it cannot also fire, or a
  // fresh install one release behind would be nagged on its first check.
  if (!record) {
    writeWaitingUpdate({ onVersion, since: Date.now() });
    return;
  }

  if (record.notified) return;

  const waitingDays = (Date.now() - record.since) / (24 * 60 * 60 * 1000);
  if (waitingDays < STALE_UPDATE_THRESHOLD_DAYS) return;

  try {
    await invokeFn("show_notification", {
      title: "Headroom update waiting",
      body: `Headroom ${availableUpdate.version} is ready, and ${onVersion} has had an update waiting for ${Math.floor(
        waitingDays
      )} days. Open Headroom to install it.`,
      action: "update",
    });
    writeWaitingUpdate({ ...record, notified: true });
  } catch {
    // best-effort
  }
}

export function getAppUpdateInstallStatusCopy(
  availableUpdate: AvailableAppUpdate | null
): string | null {
  return availableUpdate ? `Downloading Headroom ${availableUpdate.version}…` : null;
}

export function formatAppUpdateProgressCopy(
  version: string,
  progress: AppUpdateProgress
): string {
  if (progress.phase === "installing") {
    return `Installing Headroom ${version}…`;
  }

  const downloadedMb = progress.downloaded / 1_000_000;
  if (progress.total && progress.total > 0) {
    const totalMb = progress.total / 1_000_000;
    const pct = Math.min(100, Math.round((progress.downloaded / progress.total) * 100));
    return `Downloading Headroom ${version}: ${downloadedMb.toFixed(1)} MB of ${totalMb.toFixed(1)} MB (${pct}%)…`;
  }
  return `Downloading Headroom ${version}: ${downloadedMb.toFixed(1)} MB…`;
}

export async function runAppUpdateInstall({
  availableUpdate,
  quiet = false,
  invokeFn = invoke,
  listenFn = listen as AppUpdateProgressListener,
  onProgress,
}: {
  availableUpdate: AvailableAppUpdate | null;
  quiet?: boolean;
  invokeFn?: AppUpdateInvoker;
  listenFn?: AppUpdateProgressListener;
  onProgress?: (progress: AppUpdateProgress) => void;
}): Promise<AppUpdateStatePatch> {
  if (!availableUpdate) {
    return {};
  }

  let unlisten: UnlistenFn | null = null;
  if (onProgress) {
    try {
      unlisten = await listenFn(APP_UPDATE_PROGRESS_EVENT, (event) => {
        onProgress(event.payload);
      });
    } catch (error) {
      Sentry.captureException(error, { tags: { flow: "app_update_progress_listen" } });
    }
  }

  try {
    try {
      await invokeFn("install_app_update");
    } catch (error) {
      // `install` consumes the staged handle even when it FAILS (RUST-HA's
      // read-only mount, then RUST-HP), so the retry the error text asks for
      // dead-ends on an empty slot until the user also runs a check by hand.
      // Run that check here instead, and only install again when the feed
      // still offers the exact version this call was asked to install - a
      // mismatch means the slot moved on and re-installing would be a
      // different build than the one the UI named.
      if (!STALE_STAGED_UPDATE.test(describeInvokeError(error, ""))) {
        throw error;
      }
      const rechecked = await invokeFn<AvailableAppUpdate | null>("check_for_app_update").catch(
        () => null
      );
      if (rechecked?.version !== availableUpdate.version) {
        throw error;
      }
      await invokeFn("install_app_update");
    }
    return {
      stagedVersion: availableUpdate.version,
      ...(quiet ? {} : { showDialog: true }),
      statusCopy: `Headroom ${availableUpdate.version} is installed and ready to restart.`,
    };
  } catch (error) {
    // The download runs inside `install`, so a dropped connection surfaces here
    // as raw reqwest text ("error decoding response body", RUST-HS) at Error
    // level. Same class the check flow already keeps below Error, and the same
    // recovery: the failed install consumed the staged handle, so the retry is
    // a fresh check plus install - which the branch above now does for them.
    const detail = describeInvokeError(error, "");
    const transport = TRANSPORT_FAILURE.test(detail);
    if (!READ_ONLY_BUNDLE.test(detail)) {
      Sentry.captureException(error, {
        level: transport ? "warning" : "error",
        tags: { flow: "app_update_install" },
      });
    }
    return {
      statusCopy: transport
        ? "Could not download the update: the connection dropped. Try again."
        : describeInvokeError(error, "Could not install the update."),
    };
  } finally {
    unlisten?.();
  }
}
