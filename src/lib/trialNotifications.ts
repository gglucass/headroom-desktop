import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type { HeadroomPricingStatus } from "./types";

// Remaining-time thresholds at which to fire grace period notifications.
const GRACE_HOURS_LEFT_THRESHOLDS = [
  16,
  8,
  1,
];

const GRACE_THRESHOLD_KEY = "headroom_grace_notif_threshold";
const TRIAL_EXPIRY_DATE_KEY = "headroom_trial_expiry_notif_date";

export async function maybeFireTrialNotifications(
  status: HeadroomPricingStatus
): Promise<void> {
  const windowVisible = await getCurrentWindow()
    .isVisible()
    .catch(() => true);
  if (windowVisible) return;

  if (status.localGraceActive && !status.authenticated) {
    await maybeFireGraceNotification(status);
  }

  const account = status.account;
  if (account?.trialActive && !account.subscriptionActive) {
    await maybeFireTrialExpiryNotification(account.trialEndsAt ?? null);
  }
}

async function maybeFireGraceNotification(
  status: HeadroomPricingStatus
): Promise<void> {
  const graceEndsAt = new Date(status.localGraceEndsAt).getTime();
  const now = Date.now();
  const hoursLeftRaw = (graceEndsAt - now) / (60 * 60 * 1000);
  const lastSent = parseInt(localStorage.getItem(GRACE_THRESHOLD_KEY) ?? "-1", 10);

  let nextIndex = -1;
  for (let i = GRACE_HOURS_LEFT_THRESHOLDS.length - 1; i >= 0; i--) {
    if (hoursLeftRaw <= GRACE_HOURS_LEFT_THRESHOLDS[i] && i > lastSent) {
      nextIndex = i;
      break;
    }
  }
  if (nextIndex === -1) return;

  const hoursLeft = Math.max(0, Math.round((graceEndsAt - now) / (60 * 60 * 1000)));
  const body =
    hoursLeft <= 2
      ? `Less than ${hoursLeft + 1} hour(s) left. Create a Headroom account to start your 14-day trial.`
      : `${hoursLeft} hours left in your 72-hour access window. Create an account to unlock a 14-day trial.`;

  await sendNotification("Start Your Headroom Trial", body, "signup");
  localStorage.setItem(GRACE_THRESHOLD_KEY, String(nextIndex));
}

async function maybeFireTrialExpiryNotification(
  trialEndsAt: string | null
): Promise<void> {
  if (!trialEndsAt) return;
  const daysLeft = Math.ceil(
    (new Date(trialEndsAt).getTime() - Date.now()) / (24 * 60 * 60 * 1000)
  );
  if (daysLeft > 3 || daysLeft <= 0) return;

  const today = new Date().toISOString().slice(0, 10);
  if (localStorage.getItem(TRIAL_EXPIRY_DATE_KEY) === today) return;

  const body =
    daysLeft === 1
      ? "Your Headroom trial ends tomorrow. Upgrade today to keep optimization enabled."
      : `Your Headroom trial ends in ${daysLeft} days. Upgrade to keep optimization enabled.`;

  await sendNotification("Headroom Trial Ending Soon", body, "billing");
  localStorage.setItem(TRIAL_EXPIRY_DATE_KEY, today);
}

async function sendNotification(
  title: string,
  body: string,
  action?: string
): Promise<void> {
  try {
    await invoke("show_notification", { title, body, action });
  } catch {
    // best-effort
  }
}
