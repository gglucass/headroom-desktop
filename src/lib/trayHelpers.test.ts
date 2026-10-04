import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  activityFeedSignature,
  homeDashboardPoll,
  launcherVerifyGate,
  launcherVerifyTickDue,
  loadDashboard,
  runtimeStatusPollMs,
  useWindowFocused,
  whenWindowVisible
} from "./trayHelpers";
import type { ActivityFeedResponse } from "./types";

const { invokeMock, isFocusedMock, isVisibleMock, onFocusChangedMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  isFocusedMock: vi.fn(),
  isVisibleMock: vi.fn(),
  onFocusChangedMock: vi.fn()
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    isFocused: isFocusedMock,
    isVisible: isVisibleMock,
    onFocusChanged: onFocusChangedMock
  })
}));

const emptySnapshot: ActivityFeedResponse = {
  proxyReachable: true,
  tiles: {
    transformation: null,
    record: null,
    rtkToday: null,
    serenaToday: null,
    learningsMilestone: null,
    weeklyRecap: null,
    trainSuggestion: null
  }
};

// Every caller has its own answer to a failed read (the pollers keep the last
// known state). Resolving to mockDashboard instead zeroed every savings figure
// and reset the terms gate on each failing 5s tick.
describe("loadDashboard", () => {
  it("rejects when get_dashboard_state fails instead of resolving to a zeroed dashboard", async () => {
    invokeMock.mockRejectedValueOnce("JoinError: task panicked");

    await expect(loadDashboard()).rejects.toBe("JoinError: task panicked");
    expect(invokeMock).toHaveBeenCalledWith("get_dashboard_state");
  });
});

describe("activityFeedSignature", () => {
  it("returns a stable string for an empty snapshot", () => {
    const sig = activityFeedSignature(emptySnapshot);
    expect(sig).toBe("1|t:-|r:-|b:-|s:-|l:-|wr:-|ts:-");
  });

  it("differentiates proxyReachable false from proxyReachable true", () => {
    const offline = activityFeedSignature({
      ...emptySnapshot,
      proxyReachable: false
    });
    const online = activityFeedSignature(emptySnapshot);
    expect(offline).not.toBe(online);
    expect(offline.startsWith("0|")).toBe(true);
    expect(online.startsWith("1|")).toBe(true);
  });

  it("changes when a tile slot's identifier flips", () => {
    const baseline = activityFeedSignature(emptySnapshot);
    const withTransform = activityFeedSignature({
      ...emptySnapshot,
      tiles: {
        ...emptySnapshot.tiles,
        transformation: {
          requestId: "req-123",
          timestamp: "2026-04-25T12:00:00Z",
          provider: "anthropic",
          model: "claude-opus-4-7",
          workspace: null,
          tokensSavedRaw: 1000,
          tokensSavedPercent: 12.5,
          estimatedCostSavingsUsd: 0.42,
          transforms: [],
          responseText: null
        } as never
      }
    });
    expect(baseline).not.toBe(withTransform);
    expect(withTransform).toContain("t:req-123");
  });

  it("falls back to timestamp when transformation has no requestId", () => {
    const sig = activityFeedSignature({
      ...emptySnapshot,
      tiles: {
        ...emptySnapshot.tiles,
        transformation: {
          requestId: null,
          timestamp: "2026-04-25T12:00:00Z",
          provider: "anthropic",
          model: null,
          workspace: null,
          tokensSavedRaw: 0,
          tokensSavedPercent: 0,
          estimatedCostSavingsUsd: 0,
          transforms: [],
          responseText: null
        } as never
      }
    });
    expect(sig).toContain("t:2026-04-25T12:00:00Z");
  });

  it("encodes record, rtkToday, learningsMilestone, weeklyRecap, trainSuggestion slots", () => {
    const sig = activityFeedSignature({
      proxyReachable: true,
      tiles: {
        transformation: null,
        record: { observedAt: "2026-04-25T11:00:00Z" } as never,
        rtkToday: { date: "2026-04-25", savedTokens: 1234 } as never,
        serenaToday: { callsLine: "3 tool calls today", tokensLine: null } as never,
        learningsMilestone: { observedAt: "2026-04-25T10:00:00Z" } as never,
        weeklyRecap: { weekStart: "2026-04-20" } as never,
        trainSuggestion: {
          projectPath: "/Users/x/proj",
          observedAt: "2026-04-25T09:00:00Z"
        } as never
      }
    });
    expect(sig).toContain("r:2026-04-25T11:00:00Z");
    expect(sig).toContain("b:2026-04-25:1234");
    expect(sig).toContain("s:3 tool calls today:");
    expect(sig).toContain("l:2026-04-25T10:00:00Z");
    expect(sig).toContain("wr:2026-04-20");
    expect(sig).toContain("ts:/Users/x/proj:2026-04-25T09:00:00Z");
  });

});

describe("useWindowFocused", () => {
  let emitFocus: (focused: boolean) => void = () => {};

  beforeEach(() => {
    isFocusedMock.mockReset();
    onFocusChangedMock.mockReset();
    onFocusChangedMock.mockImplementation(
      async (handler: (event: { payload: boolean }) => void) => {
        emitFocus = (focused) => handler({ payload: focused });
        return () => {};
      }
    );
  });

  it("reports a window that starts hidden as unfocused instead of assuming focus", async () => {
    // Both windows are created hidden; the launcher is never shown on a
    // returning launch and an autostarted main window is not either.
    isFocusedMock.mockResolvedValue(false);
    const { result } = renderHook(() => useWindowFocused());
    await act(async () => {});
    expect(result.current).toBe(false);
  });

  it("seeds a window that is already focused and then follows focus events", async () => {
    isFocusedMock.mockResolvedValue(true);
    const { result } = renderHook(() => useWindowFocused());
    await act(async () => {});
    expect(result.current).toBe(true);
    act(() => emitFocus(false));
    expect(result.current).toBe(false);
  });
});

describe("runtimeStatusPollMs", () => {
  it("keeps polling runtime status while the tray is hidden so a crash still notifies", () => {
    expect(runtimeStatusPollMs(true)).toBe(3_000);
    expect(runtimeStatusPollMs(false)).toBe(30_000);
  });
});

describe("whenWindowVisible", () => {
  beforeEach(() => {
    isVisibleMock.mockReset();
  });

  it("skips the poll while the window is hidden and runs it once shown", async () => {
    const poll = vi.fn();
    const gated = whenWindowVisible(poll);

    isVisibleMock.mockResolvedValue(false);
    await gated();
    expect(poll).not.toHaveBeenCalled();

    isVisibleMock.mockRejectedValue(new Error("ipc down"));
    await gated();
    expect(poll).not.toHaveBeenCalled();

    isVisibleMock.mockResolvedValue(true);
    await gated();
    expect(poll).toHaveBeenCalledTimes(1);
  });
});

describe("launcherVerifyTickDue", () => {
  it("polls every tick while shown, and slowly while hidden until verified", () => {
    expect(launcherVerifyTickDue(true, 3, 0, true)).toBe(true);
    // The user closed onboarding to send the test prompt: keep checking.
    expect(launcherVerifyTickDue(false, 0, 0, false)).toBe(true);
    expect(launcherVerifyTickDue(false, 3, 0, false)).toBe(false);
    expect(launcherVerifyTickDue(false, 10, 0, false)).toBe(true);
    expect(launcherVerifyTickDue(false, 10, 0, true)).toBe(false);
    // A returning launch parks the launcher hidden all session: stop.
    expect(launcherVerifyTickDue(false, 10, 30 * 60_000, false)).toBe(false);
  });
});

describe("launcherVerifyGate", () => {
  it("keeps polling hidden when the main window sets the marker mid-run", () => {
    let marker = false;
    const due = launcherVerifyGate(() => marker, 0);
    expect(due(false, false, 0)).toBe(true);
    // The main window's poller saw the test prompt first.
    marker = true;
    for (let i = 1; i < 10; i++) due(false, false, i * 1000);
    expect(due(false, false, 10_000)).toBe(true);
  });

  it("stays quiet hidden on a launch that was already verified", () => {
    const due = launcherVerifyGate(() => true, 0);
    expect(due(false, false, 0)).toBe(false);
    expect(due(true, false, 1000)).toBe(true);
  });
});

describe("homeDashboardPoll", () => {
  beforeEach(() => {
    isVisibleMock.mockReset();
  });

  it("keeps the launcher's first-run savings poll running while it is visible but unfocused", async () => {
    // The user is in their terminal sending the test prompt: the launcher
    // lost focus but still shows post_install, waiting on this poll.
    const poll = vi.fn();
    const gated = homeDashboardPoll("launcher", false, poll);
    expect(gated).not.toBeNull();

    isVisibleMock.mockResolvedValue(true);
    await gated?.();
    expect(poll).toHaveBeenCalledTimes(1);

    isVisibleMock.mockResolvedValue(false);
    await gated?.();
    expect(poll).toHaveBeenCalledTimes(1);
  });

  it("gates the tray on focus, since it hides on blur", async () => {
    const poll = vi.fn();
    expect(homeDashboardPoll("main", false, poll)).toBeNull();
    expect(homeDashboardPoll(null, false, poll)).toBeNull();
    await homeDashboardPoll("main", true, poll)?.();
    expect(poll).toHaveBeenCalledTimes(1);
    expect(isVisibleMock).not.toHaveBeenCalled();
  });
});
