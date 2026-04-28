import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type { HeadroomPricingStatus, RuntimeStatus } from "./types";

const NEEDS_AUTH_KEY = "headroom_urgent_needs_auth_date";
const OPTIMIZATION_BLOCKED_KEY = "headroom_urgent_opt_blocked_date";
const RUNTIME_DOWN_KEY = "headroom_urgent_runtime_down_date";
const NUDGE_KEY_PREFIX = "headroom_urgent_nudge_level";

const NUDGE_TITLES: Record<number, string> = {
  1: "Heads up: 25% of your weekly Claude usage",
  2: "Halfway there: 35% of your weekly Claude usage",
  3: "Almost paused: 45% of your weekly Claude usage",
};

export async function maybeFireUrgentPricingNotifications(
  status: HeadroomPricingStatus
): Promise<void> {
  if (await isWindowVisible()) return;

  if (status.needsAuthentication) {
    await fireOncePerDay(
      NEEDS_AUTH_KEY,
      "Headroom needs you to sign in",
      status.gateMessage ||
        "Sign in to Headroom to keep optimization running.",
      "signin"
    );
    return;
  }

  if (!status.optimizationAllowed) {
    await fireOncePerDay(
      OPTIMIZATION_BLOCKED_KEY,
      "Headroom optimization is off",
      status.gateMessage ||
        "Your current plan has optimization disabled. Open Headroom to review.",
      "billing"
    );
    return;
  }

  if (status.shouldNudge && status.nudgeLevel > 0) {
    const level = Math.min(status.nudgeLevel, 3);
    await fireOncePerWeek(
      `${NUDGE_KEY_PREFIX}_${level}`,
      NUDGE_TITLES[level] ?? "Heads up: weekly Claude usage rising",
      status.gateMessage ||
        "Headroom will pause optimization at your weekly usage cap. Upgrade to keep going.",
      "billing"
    );
  }
}

export async function maybeFireUrgentRuntimeNotification(
  runtime: RuntimeStatus
): Promise<void> {
  if (await isWindowVisible()) return;

  const runtimeDown =
    runtime.installed && !runtime.running && !runtime.starting && !runtime.paused;
  if (!runtimeDown) return;

  const body = runtime.startupErrorHint
    ? `Headroom isn't running. ${runtime.startupErrorHint}`
    : runtime.startupError
    ? `Headroom isn't running: ${runtime.startupError}`
    : "Headroom isn't running. Open the tray to restart it.";

  await fireOncePerDay(
    RUNTIME_DOWN_KEY,
    "Headroom stopped running",
    body,
    "runtime"
  );
}

async function fireOncePerDay(
  storageKey: string,
  title: string,
  body: string,
  action: string
): Promise<void> {
  const today = new Date().toISOString().slice(0, 10);
  if (localStorage.getItem(storageKey) === today) return;
  try {
    await invoke("show_notification", { title, body, action });
    localStorage.setItem(storageKey, today);
  } catch {
    // best-effort
  }
}

async function fireOncePerWeek(
  storageKey: string,
  title: string,
  body: string,
  action: string
): Promise<void> {
  const week = isoWeekKey(new Date());
  if (localStorage.getItem(storageKey) === week) return;
  try {
    await invoke("show_notification", { title, body, action });
    localStorage.setItem(storageKey, week);
  } catch {
    // best-effort
  }
}

// Returns "YYYY-Www" using ISO 8601 week numbering. Used to key
// notifications that should re-fire each new Claude weekly usage window.
function isoWeekKey(date: Date): string {
  const d = new Date(Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate()));
  const dayNum = d.getUTCDay() || 7;
  d.setUTCDate(d.getUTCDate() + 4 - dayNum);
  const yearStart = new Date(Date.UTC(d.getUTCFullYear(), 0, 1));
  const weekNum = Math.ceil(((d.getTime() - yearStart.getTime()) / 86400000 + 1) / 7);
  return `${d.getUTCFullYear()}-W${String(weekNum).padStart(2, "0")}`;
}

async function isWindowVisible(): Promise<boolean> {
  return getCurrentWindow()
    .isVisible()
    .catch(() => true);
}
