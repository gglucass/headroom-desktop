import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type { HeadroomPricingStatus, RuntimeStatus } from "./types";

const NEEDS_AUTH_KEY = "headroom_urgent_needs_auth_date";
const OPTIMIZATION_BLOCKED_KEY = "headroom_urgent_opt_blocked_date";
const RUNTIME_DOWN_KEY = "headroom_urgent_runtime_down_date";

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
  }

  if (!status.optimizationAllowed && !status.needsAuthentication) {
    await fireOncePerDay(
      OPTIMIZATION_BLOCKED_KEY,
      "Headroom optimization is off",
      status.gateMessage ||
        "Your current plan has optimization disabled. Open Headroom to review.",
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

  await fireOncePerDay(
    RUNTIME_DOWN_KEY,
    "Headroom stopped running",
    runtime.startupError
      ? `Headroom isn't running: ${runtime.startupError}`
      : "Headroom isn't running. Open the tray to restart it.",
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

async function isWindowVisible(): Promise<boolean> {
  return getCurrentWindow()
    .isVisible()
    .catch(() => true);
}
