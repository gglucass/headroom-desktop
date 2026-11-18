import { invoke } from "@tauri-apps/api/core";

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
