import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type { ActivityFeedResponse, DashboardState } from "./types";

/// All views the tray window can land on. Kept here (rather than in App.tsx)
/// so helpers and tests can import the union without pulling in App.tsx's
/// component tree.
export type TrayView =
  | "home"
  | "optimization"
  | "health"
  | "notifications"
  | "addons"
  | "upgrade"
  | "upgradeAuth"
  | "settings";

/// The dashboard read. It rejects on failure on purpose: each caller decides
/// what a failed read means (the pollers keep the last known state). Falling
/// back to mockDashboard here zeroed every savings figure and reset the terms
/// gate on each failing 5s tick.
export function loadDashboard(): Promise<DashboardState> {
  return invoke<DashboardState>("get_dashboard_state");
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
    tiles.serenaToday
      ? `s:${tiles.serenaToday.callsLine ?? ""}:${tiles.serenaToday.tokensLine ?? ""}`
      : "s:-",
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

/// Whether this webview's window has focus, for both windows. Both are created
/// hidden (tauri.conf.json), so this starts false and is seeded from the window
/// once the listener is live. A hard-coded `true` that only the main window
/// ever updated kept the never-shown launcher, and an autostarted main window
/// until its first focus change, on the focused poll cadence all session.
export function useWindowFocused(): boolean {
  const [focused, setFocused] = useState(false);
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const win = getCurrentWindow();
      let sawEvent = false;
      const fn = await win.onFocusChanged(({ payload }) => {
        sawEvent = true;
        if (active) setFocused(payload);
      });
      if (!active) return fn();
      unlisten = fn;
      // Queried only after the listener is up, so no later change is missed
      // and a change that races this read is not overwritten by it.
      const value = await win.isFocused();
      if (active && !sawEvent) setFocused(value);
    })().catch(() => {});
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);
  return focused;
}

/// Runtime status poll cadence for the main window. It keeps a slow poll while
/// hidden: the "Headroom stopped running" notification only fires while the
/// window is hidden, so a focused-only poll could never raise it.
export function runtimeStatusPollMs(focused: boolean): number {
  return focused ? 3_000 : 30_000;
}

/// Gate for launcher-stage pollers. The launcher webview lives hidden all
/// session on every returning launch, parked on post_install, so each tick
/// checks the window is actually showing before spawning ps/tasklist or
/// polling the runtime for a screen nobody can see.
export function whenWindowVisible(poll: () => void | Promise<void>): () => Promise<void> {
  return async () => {
    if (!(await getCurrentWindow().isVisible().catch(() => false))) return;
    await poll();
  };
}

/// Gate for the post_install traffic-verification poll (1s ticks). Closing the
/// launcher only hides it, and a first-run user often closes it and goes off to
/// send the test prompt; skipping every hidden tick (0.9.27) meant their rows
/// never verified, so proxy_verified and the main window's verified marker
/// never landed (23% -> 7% of new signups). Hidden, it checks every 10th tick
/// until verified, for 30 minutes only: returning launches park this webview
/// hidden on post_install all session.
export function launcherVerifyTickDue(
  visible: boolean,
  tick: number,
  elapsedMs: number,
  verified: boolean
): boolean {
  if (visible) return true;
  return !verified && elapsedMs < 30 * 60_000 && tick % 10 === 0;
}

/// Home dashboard poll gate, or null when it should not run. The tray hides on
/// blur, so focus stands in for visibility there. The launcher stays visible
/// while a first-run user is off in their terminal sending the test prompt,
/// and its post_install screen waits on this poll for their first savings, so
/// it gates on visibility instead: shown keeps polling, hidden skips.
export function homeDashboardPoll(
  windowLabel: string | null,
  focused: boolean,
  poll: () => void | Promise<void>
): (() => Promise<void>) | null {
  if (windowLabel === "launcher") return whenWindowVisible(poll);
  return focused ? async () => poll() : null;
}
