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

// Returns true only on the send that actually fired, so callers can piggyback
// their own once-only side effects on the same dedupe.
export function trackInstallMilestoneOnce(
  name: string,
  properties?: AnalyticsProperties
): boolean {
  const storageKey = `${installMilestonePrefix}${name}`;
  if (seenInstallMilestones.has(storageKey)) {
    return false;
  }

  try {
    if (localStorage.getItem(storageKey) === "1") {
      seenInstallMilestones.add(storageKey);
      return false;
    }
    localStorage.setItem(storageKey, "1");
  } catch {
    // Fall through and at least dedupe for the current session.
  }

  seenInstallMilestones.add(storageKey);
  trackAnalyticsEvent(name, properties);
  return true;
}

// The webview's Sentry SDK sends straight to Sentry, past the Rust
// before_send that honours Settings > Usage analytics and crash reports, so its
// transport asks first: events and release-health sessions alike. Per
// envelope, not cached, because another window may have flipped the switch.
// An unreadable setting counts as on, the default the switch itself shows.
export function gateOnUsageData<Envelope, Response extends object>(transport: {
  send(envelope: Envelope): PromiseLike<Response>;
  flush(timeout?: number): PromiseLike<boolean>;
}) {
  return {
    send: async (envelope: Envelope): Promise<Response | Record<string, never>> =>
      (await invoke<boolean>("get_usage_data_enabled").catch(() => true))
        ? transport.send(envelope)
        : {},
    flush: (timeout?: number) => transport.flush(timeout),
  };
}
