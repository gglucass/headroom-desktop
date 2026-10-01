import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  ActivityFeed,
  collapseDiff,
  diffLines,
  formatRequestMessages,
  groupTransforms
} from "./ActivityFeed";
import type {
  ActivityFeedResponse,
  ActivityFeedSnapshot,
  LearningsMilestoneEvent,
  RecordEvent,
  RtkTodayStats,
  TrainSuggestionEvent,
  TransformationFeedEvent,
  WeeklyRecapEvent
} from "../lib/types";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(false);
});

const emptyTiles: ActivityFeedSnapshot = {
  transformation: null,
  record: null,
  rtkToday: null,
  serenaToday: null,
  learningsMilestone: null,
  weeklyRecap: null,
  trainSuggestion: null
};

const baseFeed: ActivityFeedResponse = {
  tiles: emptyTiles,
  proxyReachable: true
};

function transformation(event: Partial<TransformationFeedEvent> = {}): TransformationFeedEvent {
  return {
    requestId: "req-1",
    timestamp: "2026-04-21T10:00:00Z",
    provider: "anthropic",
    model: "claude-sonnet-4-6",
    inputTokensOriginal: 1000,
    inputTokensOptimized: 250,
    tokensSaved: 750,
    savingsPercent: 75,
    transformsApplied: ["interceptor:ast-grep"],
    ...event
  };
}

function feedWith(partial: Partial<ActivityFeedSnapshot>): ActivityFeedResponse {
  return { ...baseFeed, tiles: { ...emptyTiles, ...partial } };
}

describe("ActivityFeed", () => {
  it("shows the error message when error is set", () => {
    const markup = renderToStaticMarkup(<ActivityFeed feed={baseFeed} error="boom" />);
    expect(markup).toContain("boom");
    expect(markup).not.toContain("activity-feed__list");
  });

  it("shows the waiting state when proxy is not reachable and no events", () => {
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={{ ...baseFeed, proxyReachable: false }} error={null} />
    );
    expect(markup).toContain("Waiting for the Headroom proxy");
    expect(markup).not.toContain("activity-feed__list");
  });

  it("surfaces persisted tiles even when the proxy is unreachable", () => {
    // ActivityFacts carries persisted slots across restarts. When the proxy
    // is briefly unreachable but we still have state to show, render the
    // tiles — not the "Waiting" empty — so a restart doesn't look blank.
    const feed: ActivityFeedResponse = {
      ...feedWith({ transformation: transformation({ requestId: "from-history" }) }),
      proxyReachable: false
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__list");
    expect(markup).not.toContain("Waiting for the Headroom proxy");
  });

  it("renders a placeholder card for every kind when proxy is up but no events", () => {
    // RTK is opt-in: without rtkInstalled its tile is absent entirely, not
    // an empty placeholder.
    const markup = renderToStaticMarkup(<ActivityFeed feed={baseFeed} error={null} />);
    expect(markup).not.toContain("No requests yet");
    expect(markup).toContain("activity-feed__list");
    const emptyClassCount = (markup.match(/activity-feed__item--empty/g) ?? []).length;
    expect(emptyClassCount).toBe(5);
    for (const cls of [
      "activity-feed__item--train",
      "activity-feed__item--transformation",
      "activity-feed__item--record",
      "activity-feed__item--learnings-milestone",
      "activity-feed__item--weekly-recap"
    ]) {
      expect(markup).toContain(cls);
    }
    expect(markup).not.toContain("activity-feed__item--rtk");
    expect(markup).not.toContain("activity-feed__item--serena");
    expect(markup).toContain("No large compressions yet");
    expect(markup).toContain("No recap yet");
  });

  it("includes the RTK placeholder only when rtkInstalled is set", () => {
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={baseFeed} error={null} rtkInstalled />
    );
    const emptyClassCount = (markup.match(/activity-feed__item--empty/g) ?? []).length;
    expect(emptyClassCount).toBe(6);
    expect(markup).toContain("activity-feed__item--rtk");
    expect(markup).toContain("No RTK commands observed yet today (UTC).");
    expect(markup).not.toContain("activity-feed__item--serena");
  });

  it("includes the Serena placeholder only when serenaInstalled is set", () => {
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={baseFeed} error={null} serenaInstalled />
    );
    const emptyClassCount = (markup.match(/activity-feed__item--empty/g) ?? []).length;
    expect(emptyClassCount).toBe(6);
    expect(markup).toContain("activity-feed__item--serena");
    expect(markup).toContain("No Serena tool calls observed yet today.");
    expect(markup).not.toContain("activity-feed__item--rtk");
  });

  it("keeps placeholders for other kinds when one live event is present", () => {
    const feed = feedWith({ transformation: transformation() });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const emptyClassCount = (markup.match(/activity-feed__item--empty/g) ?? []).length;
    expect(emptyClassCount).toBe(4);
    expect(markup).toContain("Recent large compression");
    expect(markup).not.toContain("No compressions yet");
  });

  it("marks the empty trainSuggestion card as clickable when a navigate handler is supplied", () => {
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={baseFeed} error={null} onNavigateToOptimize={() => {}} />
    );
    const trainSegment = markup.match(
      /<li[^>]*activity-feed__item--train[^>]*>[\s\S]*?<\/li>/
    );
    expect(trainSegment).not.toBeNull();
    expect(trainSegment![0]).toContain("activity-feed__item--clickable");
    expect(trainSegment![0]).toContain('role="button"');
    expect(trainSegment![0]).toContain("No scan nudge");
  });

  it("leaves the empty trainSuggestion card non-interactive when no handler is supplied", () => {
    const markup = renderToStaticMarkup(<ActivityFeed feed={baseFeed} error={null} />);
    const trainSegment = markup.match(
      /<li[^>]*activity-feed__item--train[^>]*>[\s\S]*?<\/li>/
    );
    expect(trainSegment).not.toBeNull();
    expect(trainSegment![0]).not.toContain("activity-feed__item--clickable");
    expect(trainSegment![0]).not.toContain('role="button"');
  });

  it("renders a transformation row with provider, model, savings, delta, and transforms", () => {
    const feed = feedWith({ transformation: transformation() });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Recent large compression");
    expect(markup).toContain("claude-sonnet-4-6");
    expect(markup).toContain("Saved 750 tokens (75.0%)");
    expect(markup).toContain("1,000");
    expect(markup).toContain("250");
    expect(markup).toContain("ast-grep");
    expect(markup).not.toContain("interceptor:ast-grep");
  });

  it("renders friendly labels for read_lifecycle transforms", () => {
    const feed = feedWith({
      transformation: transformation({
        transformsApplied: ["read_lifecycle:stale", "read_lifecycle:superseded"]
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Stale Read");
    expect(markup).toContain("Superseded Read");
    expect(markup).not.toContain("read_lifecycle:stale");
    expect(markup).not.toContain("read_lifecycle:superseded");
  });

  it("renders friendly labels for parametric transforms", () => {
    const feed = feedWith({
      transformation: transformation({
        transformsApplied: [
          "tool_crush:7",
          "router:tool_result:ast",
          "kompress:user:0.45",
          "inserted_3_cache_breakpoints",
          "cache_align"
        ]
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Crushed 7 tools");
    expect(markup).toContain("Tool result: ast");
    expect(markup).toContain("Kompress user (0.45x)");
    expect(markup).toContain("Inserted 3 cache breakpoints");
    expect(markup).toContain("Cache aligned");
  });

  it("suppresses the percent and in/out pair on clamped legacy records", () => {
    // Backends without the upstream denominator fix (headroom#3106) emit
    // saved > in with out clamped to 0 and a >100% rate on schema-heavy
    // responses turns. The saved count is real; the pair and rate are not.
    const feed = feedWith({
      transformation: transformation({
        inputTokensOriginal: 4436,
        inputTokensOptimized: 0,
        tokensSaved: 9245,
        savingsPercent: 208.4
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Saved 9,245 tokens");
    expect(markup).not.toContain("208.4%");
    expect(markup).not.toContain("4,436");
    expect(markup).not.toContain("Tokens in → out");
  });

  it("keeps the in/out pair on a legitimate full wipe (saved == in, out == 0)", () => {
    const feed = feedWith({
      transformation: transformation({
        inputTokensOriginal: 4436,
        inputTokensOptimized: 0,
        tokensSaved: 4436,
        savingsPercent: 100
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Saved 4,436 tokens (100.0%)");
    expect(markup).toContain("4,436");
  });

  it("shows an estimated dollar savings alongside tokens saved", () => {
    const feed = feedWith({
      transformation: transformation({
        model: "claude-sonnet-4-6",
        tokensSaved: 750_000,
        savingsPercent: 75
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    // sonnet: $3/M × 0.75M = $2.25
    expect(markup).toContain("~$2.25");
  });

  it("surfaces file paths from enriched read_lifecycle tags in the detail view", () => {
    const feed = feedWith({
      transformation: transformation({
        transformsApplied: [
          "read_lifecycle:stale:/src/App.tsx",
          "read_lifecycle:stale:/src/lib/foo.ts",
          "tool_crush:2:Bash,Grep"
        ]
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("/src/App.tsx");
    expect(markup).toContain("/src/lib/foo.ts");
    expect(markup).toContain("Bash,Grep");
    // Chip row still collapses to per-label count regardless of target count.
    expect(markup).toContain("Stale Read × 2");
    expect(markup).toContain("Crushed 2 tools");
  });

  it("renders smart_crush tags (current proxy) like the legacy tool_crush tags", () => {
    const feed = feedWith({
      transformation: transformation({
        transformsApplied: ["smart_crush:7", "smart_crush:2:Bash,Grep"]
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Crushed 7 tools");
    expect(markup).toContain("Crushed 2 tools");
    expect(markup).toContain("Bash,Grep");
  });

  it("falls back to the raw transform string when unknown", () => {
    const feed = feedWith({
      transformation: transformation({ transformsApplied: ["something:new:format"] })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("something:new:format");
  });

  it("collapses repeated transforms into a single count chip", () => {
    // Before: 70 identical stale-read transforms rendered 70 separate chips
    // and flooded the row. Now: one chip "Stale Read × 70".
    const feed = feedWith({
      transformation: transformation({
        transformsApplied: [
          ...Array(70).fill("read_lifecycle:stale"),
          ...Array(42).fill("router:excluded:tool"),
          "cache_align"
        ]
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const chipCount = (markup.match(/<li class="activity-feed__transform"/g) ?? []).length;
    expect(chipCount).toBe(3);
    expect(markup).toContain("Stale Read × 70");
    expect(markup).toContain("Tool result excluded × 42");
    expect(markup).toContain(">Cache aligned<");
  });

  it("shows the newest compression in the transformation tile (slot holds the latest)", () => {
    // The transformation slot is populated by the newest-observed
    // transformation — the backend writes in timestamp-asc order during its
    // observation pass, so the last write wins and the frontend just reads
    // that slot. This test seeds what ends up in the slot.
    const feed = feedWith({
      transformation: transformation({
        requestId: "latest",
        timestamp: "2026-04-21T10:02:00Z",
        tokensSaved: 100,
        savingsPercent: 10
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const liveBadgeCount = (markup.match(/Recent large compression/g) ?? []).length;
    expect(liveBadgeCount).toBe(1);
    expect(markup).toContain("Saved 100 tokens (10.0%)");
    expect(markup).not.toContain("9,999");
    expect(markup).not.toContain("Compression × ");
  });

  it("renders time chips using relative time with an absolute-date tooltip", () => {
    const now = new Date("2026-04-21T10:00:00Z");
    vi.useFakeTimers();
    vi.setSystemTime(now);
    try {
      const feed = feedWith({
        transformation: transformation({
          requestId: "relnow",
          timestamp: "2026-04-21T09:50:00Z"
        })
      });
      const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
      expect(markup).toContain("10m ago");
      expect(markup).toMatch(/title="[^"]*2026[^"]*"/);
    } finally {
      vi.useRealTimers();
    }
  });

  it("exposes transformation detail (request ID + raw transforms) in an expandable row", () => {
    const feed = feedWith({
      transformation: transformation({
        requestId: "req-abc-123",
        transformsApplied: ["interceptor:ast-grep", "cache_align"],
        workspace: "/Users/u/Code/demo"
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    // Row is marked clickable and carries the detail block so client render
    // can toggle it. SSR renders it with expanded=false, so the detail text
    // isn't visible, but the button role + aria wiring are pinned here.
    expect(markup).toContain("activity-feed__item--clickable");
    expect(markup).toContain('role="button"');
    expect(markup).toContain('aria-expanded="false"');
  });

  // The detail exists so support IDs can be copied: a click that ends a text
  // selection there must not bubble to the row toggle and collapse it.
  it("keeps an expanded row open when its detail is clicked", async () => {
    const feed = feedWith({ transformation: transformation({ requestId: "req-abc-123" }) });
    const { container } = render(<ActivityFeed feed={feed} error={null} />);
    const row = container.querySelector(".activity-feed__item--clickable") as HTMLElement;

    await userEvent.click(row);
    await userEvent.click(screen.getByText("req-abc-123"));

    expect(row).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("req-abc-123")).toBeInTheDocument();
  });

  it("renders tiles in the fixed TILE_ORDER (record → transformation → learningsMilestone → trainSuggestion → rtkToday → serenaToday → weeklyRecap)", () => {
    // With all kinds as empty placeholders on an empty feed, the TILE_ORDER
    // dictates DOM order: the record card lands first, then transformation,
    // etc. Asserts the contract that tile positions never reshuffle based on
    // event arrival.
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={baseFeed} error={null} rtkInstalled serenaInstalled />
    );
    const order = [
      "activity-feed__item--record",
      "activity-feed__item--transformation",
      "activity-feed__item--learnings-milestone",
      "activity-feed__item--train",
      "activity-feed__item--rtk",
      "activity-feed__item--serena",
      "activity-feed__item--weekly-recap"
    ];
    const positions = order.map((cls) => markup.indexOf(cls));
    for (let i = 1; i < positions.length; i++) {
      expect(positions[i]).toBeGreaterThan(positions[i - 1]);
    }
  });

  it("renders a Serena row with calls leading and tokens as the delta", () => {
    const feed = feedWith({
      serenaToday: {
        callsLine: "231 tool calls today",
        tokensLine: "~48.2k tokens returned in 2h 5m"
      }
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Serena<");
    expect(markup).toContain("231 tool calls today");
    expect(markup).toContain("~48.2k tokens returned in 2h 5m");
  });

  it("renders an RTK today row when the slot has data, even without rtkInstalled", () => {
    const data: RtkTodayStats = {
      date: "2026-04-21",
      savedTokens: 1234,
      commands: 3
    };
    const feed = feedWith({ rtkToday: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">RTK<");
    // rtk buckets by UTC date; the tile must not call it the local day.
    expect(markup).toContain("1,234 tokens saved today (UTC)");
    expect(markup).toContain("3 commands");
  });

  it("renders a daily record row with model and savings percent", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      tags: ["daily"],
      tokensSaved: 7500,
      savingsPercent: 82.5,
      model: "claude-opus-4-7",
      provider: "anthropic",
      requestId: "r-9",
      previousRecord: null,
      day: "2026-04-21"
    };
    const feed = feedWith({ record: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Record<");
    expect(markup).toContain(">Daily<");
    expect(markup).not.toContain(">All-time<");
    expect(markup).toContain("claude-opus-4-7");
    expect(markup).toContain("Saved 7,500 tokens (82.5%)");
  });

  it("renders an all-time record row with the previous record delta", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      tags: ["allTime"],
      tokensSaved: 12000,
      savingsPercent: 91,
      model: "claude-opus-4-7",
      provider: "anthropic",
      requestId: "r-42",
      previousRecord: 9500,
      day: null
    };
    const feed = feedWith({ record: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Record<");
    expect(markup).toContain(">All-time<");
    expect(markup).toContain("Saved 12,000 tokens (91.0%)");
    expect(markup).toContain("previous record 9,500");
  });

  it("renders a record row that qualifies for both daily and all-time with both tags", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      tags: ["daily", "allTime"],
      tokensSaved: 15000,
      savingsPercent: 88.2,
      model: "claude-opus-4-7",
      provider: "anthropic",
      requestId: "r-77",
      previousRecord: 10000,
      day: "2026-04-21"
    };
    const feed = feedWith({ record: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Record<");
    expect(markup).toContain(">Daily<");
    expect(markup).toContain(">All-time<");
    expect(markup).toContain("Saved 15,000 tokens (88.2%)");
    expect(markup).toContain("previous record 10,000");
  });

  it("marks the record row clickable when only requestId / exact tokens are present (no messages)", () => {
    // The record row now carries forward cost, exact tokens, and request ID
    // from the source transformation. Any one of those should make the row
    // expandable even when full-message logging is off. SSR renders detail
    // collapsed, so the signal is the `--clickable` class + aria wiring.
    const event: RecordEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      tags: ["allTime"],
      tokensSaved: 39901,
      savingsPercent: 40.9,
      model: "claude-opus-4-7",
      provider: "anthropic",
      requestId: "hr_1777038929_000202",
      previousRecord: 30000,
      day: null,
      inputTokensOriginal: 97571,
      inputTokensOptimized: 57670
    };
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={feedWith({ record: event })} error={null} />
    );
    expect(markup).toContain("activity-feed__item--record");
    expect(markup).toContain("activity-feed__item--clickable");
    expect(markup).toContain('role="button"');
    expect(markup).toContain('aria-expanded="false"');
  });

  it("renders a weekly recap row with the week range and totals", () => {
    const data: WeeklyRecapEvent = {
      observedAt: "2026-04-27T09:00:00Z",
      weekStart: "2026-04-20",
      weekEnd: "2026-04-26",
      totalTokensSaved: 12500,
      totalSavingsUsd: 4.25,
      activeDays: 5
    };
    const feed = feedWith({ weeklyRecap: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Weekly recap");
    expect(markup).toContain("2026-04-20");
    expect(markup).toContain("2026-04-26");
    expect(markup).toContain("12,500 tokens saved");
    expect(markup).toContain("$4.25");
    expect(markup).toContain("5 active days");
  });

  it("renders a learnings milestone row with the today-scoped copy and project chip", () => {
    const data: LearningsMilestoneEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      patternsToday: 4,
      remindersToday: 2,
      learningsToday: 3,
      projectPath: "/Users/u/Code/demo-repo",
      projectDisplayName: "demo-repo"
    };
    const feed = feedWith({ learningsMilestone: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Learnings<");
    expect(markup).toContain("4 patterns identified today");
    expect(markup).toContain("2 reminders");
    expect(markup).toContain("3 learnings written to memory");
    expect(markup).toContain("demo-repo");
  });

  it("omits the project chip when no project is active today", () => {
    const data: LearningsMilestoneEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      patternsToday: 0,
      remindersToday: 0,
      learningsToday: 0,
      projectPath: null,
      projectDisplayName: null
    };
    const feed = feedWith({ learningsMilestone: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("0 patterns identified today");
    expect(markup).not.toContain("activity-feed__project");
  });

  it("singularises the learnings copy for single-count values", () => {
    const data: LearningsMilestoneEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      patternsToday: 1,
      remindersToday: 1,
      learningsToday: 1,
      projectPath: null,
      projectDisplayName: null
    };
    const feed = feedWith({ learningsMilestone: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("1 pattern identified today");
    expect(markup).toContain("1 reminder and 1 learning written to memory");
  });

  it("renders a never-trained TrainSuggestion row with project + session count", () => {
    const data: TrainSuggestionEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      projectPath: "/Users/u/Code/demo-repo",
      projectDisplayName: "demo-repo",
      sessionCount: 7,
      activeDaysSinceLastLearn: 0,
      kind: "never_trained"
    };
    const feed = feedWith({ trainSuggestion: data });
    const markup = renderToStaticMarkup(
      <ActivityFeed
        feed={feed}
        error={null}
        onNavigateToOptimize={() => {}}
      />
    );
    expect(markup).toContain("activity-feed__item--train");
    expect(markup).toContain("activity-feed__badge--train");
    expect(markup).toContain("Try Optimize");
    expect(markup).toContain("demo-repo");
    expect(markup).toContain("7 sessions");
    // Clickable affordance present when navigation callback was provided.
    expect(markup).toContain("activity-feed__item--clickable");
    expect(markup).toContain('role="button"');
  });

  it("renders a stale TrainSuggestion row with the retrain copy", () => {
    const data: TrainSuggestionEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      projectPath: "/Users/u/Code/demo-repo",
      projectDisplayName: "demo-repo",
      sessionCount: 20,
      activeDaysSinceLastLearn: 4,
      kind: "stale"
    };
    const feed = feedWith({ trainSuggestion: data });
    const markup = renderToStaticMarkup(
      <ActivityFeed
        feed={feed}
        error={null}
        onNavigateToOptimize={() => {}}
      />
    );
    expect(markup).toContain("Rescan");
    expect(markup).toContain("4 active days");
    expect(markup).toContain("demo-repo");
  });

  it("omits the clickable affordance when onNavigateToOptimize is not provided", () => {
    const data: TrainSuggestionEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      projectPath: "/Users/u/Code/demo-repo",
      projectDisplayName: "demo-repo",
      sessionCount: 7,
      activeDaysSinceLastLearn: 0,
      kind: "never_trained"
    };
    const feed = feedWith({ trainSuggestion: data });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__item--train");
    expect(markup).not.toContain("activity-feed__item--clickable");
    expect(markup).not.toContain('role="button"');
  });

  it("renders a workspace badge on a transformation when workspace is set", () => {
    const feed = feedWith({
      transformation: transformation({ workspace: "/Users/u/Code/demo-repo" })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__project");
    expect(markup).toContain(">demo-repo<");
  });

  it("omits the workspace badge when workspace is missing", () => {
    const feed = feedWith({ transformation: transformation() });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("activity-feed__project");
  });

  it("falls back to 0 savings when transformation fields are null", () => {
    const feed = feedWith({
      transformation: transformation({
        requestId: null,
        timestamp: null,
        provider: null,
        model: null,
        inputTokensOriginal: null,
        inputTokensOptimized: null,
        tokensSaved: null,
        savingsPercent: null,
        transformsApplied: []
      })
    });
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Saved 0 tokens (0.0%)");
    expect(markup).not.toContain("activity-feed__delta");
    expect(markup).not.toContain("activity-feed__transforms");
  });

});

describe("groupTransforms", () => {
  it("returns an empty array for an empty input", () => {
    expect(groupTransforms([])).toEqual([]);
  });

  it("returns a single entry with count 1 for a unique raw", () => {
    const result = groupTransforms(["cache_align"]);
    expect(result).toEqual([
      { label: "Cache aligned", title: expect.any(String), count: 1, targets: [] }
    ]);
  });

  it("collapses duplicates, preserves first-seen order, and counts each group", () => {
    const raws = [
      "read_lifecycle:stale",
      "router:excluded:tool",
      "read_lifecycle:stale",
      "read_lifecycle:stale",
      "router:excluded:tool"
    ];
    const result = groupTransforms(raws);
    expect(result.map((g) => g.label)).toEqual(["Stale Read", "Tool result excluded"]);
    expect(result.map((g) => g.count)).toEqual([3, 2]);
  });

  it("groups by friendly label even when the raw strings differ", () => {
    // Two distinct raws that both map to the same friendly label would
    // collapse into one chip — pin that so future formatTransform changes
    // don't silently break the UX.
    const result = groupTransforms(["cache_align", "cache_align"]);
    expect(result).toHaveLength(1);
    expect(result[0].count).toBe(2);
  });

  it("accumulates unique file paths from enriched read_lifecycle tags", () => {
    // New proxy format: read_lifecycle:<state>:<file_path>. Two stale reads
    // on the same file dedupe to one target; different files accumulate.
    const result = groupTransforms([
      "read_lifecycle:stale:/src/App.tsx",
      "read_lifecycle:stale:/src/App.tsx",
      "read_lifecycle:stale:/src/lib/foo.ts"
    ]);
    expect(result).toHaveLength(1);
    expect(result[0].label).toBe("Stale Read");
    expect(result[0].count).toBe(3);
    expect(result[0].targets).toEqual(["/src/App.tsx", "/src/lib/foo.ts"]);
  });

  it("preserves colons in file paths when parsing read_lifecycle tags", () => {
    // A 3-part split ensures paths containing ':' aren't truncated.
    const result = groupTransforms(["read_lifecycle:stale:/tmp/has:colon/x.py"]);
    expect(result[0].targets).toEqual(["/tmp/has:colon/x.py"]);
  });

  it("groups legacy and enriched read_lifecycle tags together", () => {
    // During a rolling proxy upgrade both forms may appear in the same
    // request — both should land in the same "Stale Read" group.
    const result = groupTransforms([
      "read_lifecycle:stale",
      "read_lifecycle:stale:/src/App.tsx"
    ]);
    expect(result).toHaveLength(1);
    expect(result[0].label).toBe("Stale Read");
    expect(result[0].count).toBe(2);
    expect(result[0].targets).toEqual(["/src/App.tsx"]);
  });

  it("extracts tool names from enriched tool_crush tags", () => {
    const result = groupTransforms(["tool_crush:3:Bash,Read,Grep"]);
    expect(result).toHaveLength(1);
    expect(result[0].label).toBe("Crushed 3 tools");
    expect(result[0].targets).toEqual(["Bash,Read,Grep"]);
  });

  it("leaves targets empty for legacy tool_crush tags without names", () => {
    const result = groupTransforms(["tool_crush:5"]);
    expect(result[0].label).toBe("Crushed 5 tools");
    expect(result[0].targets).toEqual([]);
  });
});

describe("before/after", () => {
  const diffResponse = (overrides: object) =>
    invokeMock.mockImplementation(async (command: string) =>
      command === "get_compression_diff"
        ? { requestMessages: null, compressedMessages: null, ...overrides }
        : false
    );
  const expand = async () => {
    render(<ActivityFeed feed={feedWith({ transformation: transformation() })} error={null} />);
    await userEvent.click(screen.getByRole("button", { name: /Recent large compression/ }));
  };

  it("fetches the expanded request and marks what compression removed", async () => {
    diffResponse({
      requestMessages: [{ role: "user", content: "keep\nnoise" }],
      compressedMessages: [{ role: "user", content: "keep" }]
    });
    await expand();
    expect(await screen.findByText("- noise")).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith("get_compression_diff", { requestId: "req-1" });
  });

  it("does not claim nothing was removed when the proxy kept no compressed side", async () => {
    // OpenAI/Codex paths log only the sent body, with no pre-compression snapshot.
    diffResponse({ requestMessages: [{ role: "user", content: "sent" }] });
    await expand();
    expect(await screen.findByText(/does not record the text before compression/)).toBeInTheDocument();
    expect(screen.queryByText(/unchanged line/)).not.toBeInTheDocument();
  });

  it("says so when the desktop holds no text for the request", async () => {
    diffResponse({});
    await expand();
    expect(await screen.findByText(/Not available for this request/)).toBeInTheDocument();
  });
});

describe("formatRequestMessages", () => {
  it("emits role + plain string content (OpenAI shape)", () => {
    expect(
      formatRequestMessages([
        { role: "user", content: "please refactor parseFoo" },
        { role: "assistant", content: "ok - reading it now" }
      ])
    ).toBe("user:\nplease refactor parseFoo\n\nassistant:\nok - reading it now");
  });

  it("flattens Anthropic content-block lists, keeping text verbatim", () => {
    expect(
      formatRequestMessages([
        {
          role: "assistant",
          content: [
            { type: "text", text: "let me check" },
            { type: "text", text: "reading the file" }
          ]
        }
      ])
    ).toBe("assistant:\nlet me check\nreading the file");
  });

  it("marks non-text blocks with [type] so they are not silently dropped", () => {
    // A tool_use or tool_result block has no surfaced `text` - rather than
    // show nothing, the formatter inserts a `[tool_use]` marker so the
    // reader knows something non-text was in the message.
    expect(
      formatRequestMessages([
        {
          role: "assistant",
          content: [
            { type: "text", text: "done, running it:" },
            { type: "tool_use", name: "Bash" }
          ]
        }
      ])
    ).toBe("assistant:\ndone, running it:\n[tool_use]");
  });

  it("surfaces tool_result payload from block.content so compression is diffable", () => {
    // kompress shrinks the *content* of tool_result blocks. That payload lives
    // in `block.content` (string or nested text blocks), not `block.text`, so
    // it must be flattened - otherwise both diff sides render a bare
    // [tool_result] and the diff shows no change.
    expect(
      formatRequestMessages([
        {
          role: "user",
          content: [
            { type: "tool_result", tool_use_id: "x", content: "huge tool output here" }
          ]
        }
      ])
    ).toBe("user:\nhuge tool output here");
    expect(
      formatRequestMessages([
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "x",
              content: [
                { type: "text", text: "line one" },
                { type: "text", text: "line two" }
              ]
            }
          ]
        }
      ])
    ).toBe("user:\nline one\nline two");
  });

  it("labels a missing role as (unknown) instead of rendering a bare newline", () => {
    expect(formatRequestMessages([{ content: "orphan content" }])).toBe(
      "(unknown):\norphan content"
    );
  });

  it("survives block shapes the proxy log can hand it", () => {
    // Everything here comes off the wire, so no shape is guaranteed. The rule
    // is the same throughout: render what is legible, drop what is not, never
    // throw -- a malformed block must not take the whole detail view down.
    const cases: Array<[unknown, string]> = [
      [null, ""], // content absent entirely
      [42, ""], // neither string nor block list
      [{ type: "text", text: "not in a list" }, ""], // a bare block, unwrapped
      [[null, "raw string entry", { type: "text", text: "kept" }], "kept"],
      [[{ tool_use_id: "x" }], ""], // no text, no content, no type
      [[{ type: "tool_result", content: [] }], "[tool_result]"] // empty payload
    ];
    for (const [content, expected] of cases) {
      expect(
        formatRequestMessages([{ role: "user", content } as never])
      ).toBe(`user:\n${expected}`);
    }
  });
});

describe("diffLines", () => {
  it("marks removed, kept, and added lines", () => {
    expect(diffLines("a\nb\nc", "a\nINSERTED\nc")).toEqual([
      { type: "same", text: "a" },
      { type: "del", text: "b" },
      { type: "add", text: "INSERTED" },
      { type: "same", text: "c" }
    ]);
  });

  it("diffs large inputs that the old per-side line cap would have rejected", () => {
    const original = Array.from({ length: 2000 }, (_, i) => String(i)).join("\n");
    const compressed = Array.from({ length: 2000 }, (_, i) =>
      i === 1000 ? "CHANGED" : String(i)
    ).join("\n");
    const diff = diffLines(original, compressed);
    expect(diff).not.toBeNull();
    expect(diff!.some((l) => l.type === "del" && l.text === "1000")).toBe(true);
    expect(diff!.some((l) => l.type === "add" && l.text === "CHANGED")).toBe(true);
  });

  it("returns null only when the cell product exceeds the memory cap", () => {
    const huge = Array.from({ length: 6000 }, (_, i) => String(i)).join("\n");
    expect(diffLines(huge, huge)).toBeNull(); // 6001^2 ≈ 36M > 30M cap
  });
});

describe("collapseDiff", () => {
  it("collapses long unchanged runs while keeping context around changes", () => {
    const original = Array.from({ length: 2000 }, (_, i) => String(i)).join("\n");
    const compressed = Array.from({ length: 2000 }, (_, i) =>
      i === 1000 ? "CHANGED" : String(i)
    ).join("\n");
    const collapsed = collapseDiff(diffLines(original, compressed)!, 3);
    // The removed line survives and sits near the top of the collapsed output.
    const delIdx = collapsed.findIndex((l) => l.type === "del" && l.text === "1000");
    expect(delIdx).toBeGreaterThanOrEqual(0);
    expect(delIdx).toBeLessThan(10);
    // Thousands of identical lines are reduced to two skip markers.
    expect(collapsed.filter((l) => l.type === "skip").length).toBe(2);
    expect(collapsed.some((l) => l.type === "same" && l.text === "0")).toBe(false);
    // Context lines immediately around the change are preserved.
    expect(collapsed.some((l) => l.type === "same" && l.text === "999")).toBe(true);
    expect(collapsed.some((l) => l.type === "same" && l.text === "1001")).toBe(true);
  });
});

