import type { HeadroomPricingStatus } from "./types";

// Most users past the trial wall with the app running never opened Upgrade
// (40 of 58, walls Sep 28 - Oct 4), the one view where they can act. So a
// window open lands there the first time after the trial ends, and when it
// follows a billing notification: no platform reports the click itself, so
// lib.rs keeps the last notification's action for a few minutes instead.
const WALL_LANDED_KEY = "headroom_upgrade_landing_wall_shown";

/** Whether this open of the main window should show Upgrade. `notificationAction`
 *  is the action of a notification shown in the last minutes, if any. */
export function takeUpgradeLanding(
  status: HeadroomPricingStatus | null,
  notificationAction: string | null
): boolean {
  if (!status?.authenticated) return false;
  const account = status.account;
  // Re-armed by a trial or subscription, so a later wall (a lapsed
  // subscription) lands once more.
  if (account?.trialActive || account?.subscriptionActive) {
    localStorage.removeItem(WALL_LANDED_KEY);
  }
  if (notificationAction === "billing") return true;
  if (status.optimizationAllowed || status.gateReason !== "trial_ended") return false;
  if (localStorage.getItem(WALL_LANDED_KEY)) return false;
  localStorage.setItem(WALL_LANDED_KEY, "1");
  return true;
}
