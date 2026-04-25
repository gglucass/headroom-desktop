import type { ActivityFeedResponse } from "./types";

/// All views the tray window can land on. Kept here (rather than in App.tsx)
/// so helpers and tests can import the union without pulling in App.tsx's
/// component tree.
export type TrayView =
  | "home"
  | "optimization"
  | "health"
  | "notifications"
  | "upgrade"
  | "upgradeAuth"
  | "settings";

/// Map a notification's `action` payload to the tray view that should open
/// when the user clicks the notification. Unknown actions return null so the
/// caller can decide whether to fall back to a default.
export function notificationActionView(action: string | null): TrayView | null {
  switch (action) {
    case "signin":
    case "billing":
    case "signup":
      return "upgradeAuth";
    case "runtime":
    case "connectors":
      return "settings";
    case "optimize":
      return "optimization";
    case "activity":
      return "notifications";
    default:
      return null;
  }
}

/// O(1) structural fingerprint of an activity feed response. Used by the
/// polling effect to skip `setActivityFeed` when the snapshot is identical
/// two polls in a row (the common case between compressions). Each tile
/// contributes a stable id for its slot — `null` when absent — so any slot
/// flip shows up in the signature.
export function activityFeedSignature(feed: ActivityFeedResponse): string {
  const { tiles } = feed;
  const parts = [
    feed.proxyReachable ? 1 : 0,
    tiles.transformation
      ? `t:${tiles.transformation.requestId ?? tiles.transformation.timestamp ?? ""}`
      : "t:-",
    tiles.record ? `r:${tiles.record.observedAt}` : "r:-",
    tiles.rtkToday ? `b:${tiles.rtkToday.date}:${tiles.rtkToday.savedTokens}` : "b:-",
    tiles.learningsMilestone ? `l:${tiles.learningsMilestone.observedAt}` : "l:-",
    tiles.weeklyRecap ? `wr:${tiles.weeklyRecap.weekStart}` : "wr:-",
    tiles.trainSuggestion
      ? `ts:${tiles.trainSuggestion.projectPath}:${tiles.trainSuggestion.observedAt}`
      : "ts:-"
  ];
  return parts.join("|");
}

/// Stable JSON serializer for diff-and-set state updates. Lifted to its own
/// helper so callers can be tested without dragging the whole component tree
/// in.
export function serializeState(value: unknown): string {
  return JSON.stringify(value);
}
