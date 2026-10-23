import { invoke } from "@tauri-apps/api/core";

import { trialSavingsLabel } from "./appHelpers";
import type { DailySavingsPoint, HeadroomPricingStatus } from "./types";
import { localDayKey, shouldNotifyInBackground } from "./urgentNotifications";

// Remaining-time thresholds at which to fire grace period notifications.
const GRACE_HOURS_LEFT_THRESHOLDS = [
  16,
  8,
  1,
];

const GRACE_THRESHOLD_KEY = "headroom_grace_notif_threshold";
const TRIAL_EXPIRY_DATE_KEY = "headroom_trial_expiry_notif_date";

export async function maybeFireTrialNotifications(
  status: HeadroomPricingStatus,
  dailySavings: DailySavingsPoint[] = []
): Promise<void> {
  if (!(await shouldNotifyInBackground())) return;

  if (status.localGraceActive && !status.authenticated) {
    await maybeFireGraceNotification(status);
  }

  const account = status.account;
  if (account?.trialActive && !account.subscriptionActive) {
    // Lead with what the trial has saved so far, as the Upgrade page does.
    const saved = account.trialStartedAt
      ? trialSavingsLabel(
          dailySavings,
          account.trialStartedAt,
          account.trialEndsAt ?? new Date().toISOString(),
          null,
          true
        )
      : null;
    await maybeFireTrialExpiryNotification(
      account.trialEndsAt ?? null,
      account.trialUsageDaysLeft ?? null,
      saved
    );
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
      ? `Less than ${hoursLeft + 1} hour(s) left. Create a Headroom account to start your 7-day trial.`
      : `${hoursLeft} hours left in your 72-hour access window. Create an account to unlock a 7-day trial.`;

  // Claim the threshold before awaiting so an overlapping tick can't send it
  // twice. sendNotification never throws, so the claim always stood anyway.
  localStorage.setItem(GRACE_THRESHOLD_KEY, String(nextIndex));
  await sendNotification("Start Your Headroom Trial", body, "signup");
}

async function maybeFireTrialExpiryNotification(
  trialEndsAt: string | null,
  usageDaysLeft: number | null,
  savedLabel: string | null
): Promise<void> {
  const now = new Date();
  let daysLeft: number;
  if (usageDaysLeft != null) {
    // Usage-day trial: the server counts saving days, not the calendar.
    daysLeft = usageDaysLeft;
    if (daysLeft <= 0 || daysLeft > 2) return;
  } else {
    const endsAt = trialEndsAt ? new Date(trialEndsAt) : null;
    // Missing, unparseable or already over.
    if (!endsAt || !(endsAt.getTime() > now.getTime())) return;
    // Local calendar days, not ceil(hours left / 24), which called a trial
    // ending at 3 PM today "tomorrow". Math.round absorbs a DST hour.
    daysLeft = Math.round((startOfLocalDay(endsAt) - startOfLocalDay(now)) / DAY_MS);
    if (daysLeft > 3) return;
  }

  const today = localDayKey(now);
  if (localStorage.getItem(TRIAL_EXPIRY_DATE_KEY) === today) return;

  const ending =
    usageDaysLeft != null
      ? `Your Headroom trial ends after ${daysLeft} more day${daysLeft === 1 ? "" : "s"} of use. Upgrade to keep optimization enabled.`
      : daysLeft === 0
        ? "Your Headroom trial ends today. Upgrade now to keep optimization enabled."
        : daysLeft === 1
          ? "Your Headroom trial ends tomorrow. Upgrade today to keep optimization enabled."
          : `Your Headroom trial ends in ${daysLeft} days. Upgrade to keep optimization enabled.`;

  // Claimed before awaiting, as in the grace path above.
  localStorage.setItem(TRIAL_EXPIRY_DATE_KEY, today);
  const body = savedLabel ? `${savedLabel} ${ending}` : ending;
  await sendNotification("Headroom Trial Ending Soon", body, "billing");
}

const DAY_MS = 24 * 60 * 60 * 1000;

function startOfLocalDay(d: Date): number {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
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
