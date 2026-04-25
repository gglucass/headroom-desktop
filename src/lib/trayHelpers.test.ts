import { describe, expect, it } from "vitest";

import {
  activityFeedSignature,
  notificationActionView,
  serializeState
} from "./trayHelpers";
import type { ActivityFeedResponse } from "./types";

const emptySnapshot: ActivityFeedResponse = {
  proxyReachable: true,
  tiles: {
    transformation: null,
    record: null,
    rtkToday: null,
    learningsMilestone: null,
    weeklyRecap: null,
    trainSuggestion: null
  }
};

describe("notificationActionView", () => {
  it("routes auth-related actions to upgradeAuth", () => {
    expect(notificationActionView("signin")).toBe("upgradeAuth");
    expect(notificationActionView("signup")).toBe("upgradeAuth");
    expect(notificationActionView("billing")).toBe("upgradeAuth");
  });

  it("routes runtime/connectors actions to settings", () => {
    expect(notificationActionView("runtime")).toBe("settings");
    expect(notificationActionView("connectors")).toBe("settings");
  });

  it("routes optimize/activity actions to their respective views", () => {
    expect(notificationActionView("optimize")).toBe("optimization");
    expect(notificationActionView("activity")).toBe("notifications");
  });

  it("returns null for unknown actions and explicit null", () => {
    expect(notificationActionView(null)).toBeNull();
    expect(notificationActionView("not-a-real-action")).toBeNull();
    expect(notificationActionView("")).toBeNull();
  });
});

describe("activityFeedSignature", () => {
  it("returns a stable string for an empty snapshot", () => {
    const sig = activityFeedSignature(emptySnapshot);
    expect(sig).toBe("1|t:-|r:-|b:-|l:-|wr:-|ts:-");
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
          requestMessages: null,
          responseText: null,
          compressedMessages: null
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
          requestMessages: null,
          responseText: null,
          compressedMessages: null
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
    expect(sig).toContain("l:2026-04-25T10:00:00Z");
    expect(sig).toContain("wr:2026-04-20");
    expect(sig).toContain("ts:/Users/x/proj:2026-04-25T09:00:00Z");
  });

  it("two calls with the same input return the same signature", () => {
    expect(activityFeedSignature(emptySnapshot)).toBe(
      activityFeedSignature(emptySnapshot)
    );
  });
});

describe("serializeState", () => {
  it("round-trips primitive and object values", () => {
    expect(serializeState(null)).toBe("null");
    expect(serializeState(42)).toBe("42");
    expect(serializeState({ a: 1, b: [2, 3] })).toBe('{"a":1,"b":[2,3]}');
  });

  it("returns a string equal for two structurally equal values", () => {
    expect(serializeState({ a: 1, b: 2 })).toBe(serializeState({ a: 1, b: 2 }));
  });
});
