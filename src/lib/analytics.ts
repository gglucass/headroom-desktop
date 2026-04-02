import { invoke } from "@tauri-apps/api/core";

export type AnalyticsProperties = Record<
  string,
  string | number | boolean | null | undefined
>;

const installMilestonePrefix = "headroom.analytics.install.";
const seenInstallMilestones = new Set<string>();

export function trackAnalyticsEvent(
  name: string,
  properties?: AnalyticsProperties
) {
  void invoke("track_analytics_event", { name, properties }).catch(() => {
    // Analytics should never interrupt product flows.
  });
}

export function trackInstallMilestoneOnce(
  name: string,
  properties?: AnalyticsProperties
) {
  const storageKey = `${installMilestonePrefix}${name}`;
  if (seenInstallMilestones.has(storageKey)) {
    return;
  }

  try {
    if (localStorage.getItem(storageKey) === "1") {
      seenInstallMilestones.add(storageKey);
      return;
    }
    localStorage.setItem(storageKey, "1");
  } catch {
    // Fall through and at least dedupe for the current session.
  }

  seenInstallMilestones.add(storageKey);
  trackAnalyticsEvent(name, properties);
}
