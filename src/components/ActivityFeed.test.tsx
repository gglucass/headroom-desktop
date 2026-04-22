import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ActivityFeed } from "./ActivityFeed";
import type {
  ActivityEvent,
  ActivityFeedResponse,
  LearningsMilestoneEvent,
  MemoryFeedEvent,
  MilestoneEvent,
  NewModelEvent,
  RecordEvent,
  RtkBatchEvent,
  SavingsMilestoneEvent,
  StreakEvent,
  TransformationFeedEvent,
  WeeklyRecapEvent
} from "../lib/types";

const baseFeed: ActivityFeedResponse = {
  events: [],
  logFullMessages: true,
  proxyReachable: true,
  memoryAvailable: true
};

function transformation(event: Partial<TransformationFeedEvent> = {}): ActivityEvent {
  return {
    kind: "transformation",
    data: {
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
    }
  };
}

function memory(event: Partial<MemoryFeedEvent> = {}): ActivityEvent {
  return {
    kind: "memory",
    data: {
      id: "mem-1",
      createdAt: "2026-04-21T10:01:00Z",
      scope: "user",
      content: "user prefers tabs over spaces",
      importance: 0.85,
      ...event
    }
  };
}

describe("ActivityFeed", () => {
  it("shows the error message when error is set", () => {
    const markup = renderToStaticMarkup(<ActivityFeed feed={baseFeed} error="boom" />);
    expect(markup).toContain("boom");
    expect(markup).not.toContain("activity-feed__list");
  });

  it("shows the waiting state when proxy is not reachable", () => {
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={{ ...baseFeed, proxyReachable: false }} error={null} />
    );
    expect(markup).toContain("Waiting for the Headroom proxy");
    expect(markup).not.toContain("activity-feed__list");
  });

  it("shows the empty state when proxy is up but no events", () => {
    const markup = renderToStaticMarkup(<ActivityFeed feed={baseFeed} error={null} />);
    expect(markup).toContain("No requests yet");
  });

  it("renders a transformation row with provider, model, savings, delta, and transforms", () => {
    const feed: ActivityFeedResponse = { ...baseFeed, events: [transformation()] };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Compression");
    expect(markup).toContain("anthropic");
    expect(markup).toContain("claude-sonnet-4-6");
    expect(markup).toContain("Saved 750 tokens (75.0%)");
    expect(markup).toContain("1,000");
    expect(markup).toContain("250");
    expect(markup).toContain("ast-grep");
    expect(markup).not.toContain("interceptor:ast-grep");
  });

  it("renders friendly labels for read_lifecycle transforms", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
          transformsApplied: ["read_lifecycle:stale", "read_lifecycle:superseded"]
        })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Stale Read");
    expect(markup).toContain("Superseded Read");
    expect(markup).not.toContain("read_lifecycle:stale");
    expect(markup).not.toContain("read_lifecycle:superseded");
  });

  it("renders friendly labels for parametric transforms", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
          transformsApplied: [
            "tool_crush:7",
            "router:tool_result:ast",
            "kompress:user:0.45",
            "inserted_3_cache_breakpoints",
            "cache_align"
          ]
        })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Crushed 7 tools");
    expect(markup).toContain("Tool result: ast");
    expect(markup).toContain("Kompress user (0.45x)");
    expect(markup).toContain("Inserted 3 cache breakpoints");
    expect(markup).toContain("Cache aligned");
  });

  it("falls back to the raw transform string when unknown", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [transformation({ transformsApplied: ["something:new:format"] })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("something:new:format");
  });

  it("renders a memory row with scope, importance, and content", () => {
    const feed: ActivityFeedResponse = { ...baseFeed, events: [memory()] };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Learning");
    expect(markup).toContain("user prefers tabs over spaces");
    expect(markup).toContain("importance 0.85");
    expect(markup).toContain(">user<");
  });

  it("renders both kinds in the order they appear in the feed", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        memory({ id: "mem-2", content: "second" }),
        transformation({ requestId: "req-2", provider: "openai" })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const memoryIdx = markup.indexOf("second");
    const transformationIdx = markup.indexOf("openai");
    expect(memoryIdx).toBeGreaterThan(-1);
    expect(transformationIdx).toBeGreaterThan(-1);
    expect(memoryIdx).toBeLessThan(transformationIdx);
  });

  it("renders an RTK batch row with deltas and cumulative totals", () => {
    const data: RtkBatchEvent = {
      observedAt: "2026-04-21T14:00:00Z",
      commandsDelta: 3,
      tokensSavedDelta: 1234,
      totalCommands: 2888,
      totalSaved: 12_805_724
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "rtkBatch", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">RTK<");
    expect(markup).toContain("+3 commands");
    expect(markup).toContain("1,234 tokens");
    expect(markup).toContain("2,888");
    expect(markup).toContain("12,805,724");
  });

  it("renders a milestone row with the formatted token count", () => {
    const data: MilestoneEvent = {
      observedAt: "2026-04-21T14:30:00Z",
      milestoneTokensSaved: 5_000_000,
      kind: "first_5m"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "milestone", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Milestone");
    expect(markup).toContain("5M");
  });

  it("renders a daily record row with model and savings percent", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      tokensSaved: 7500,
      savingsPercent: 82.5,
      model: "claude-opus-4-7",
      provider: "anthropic",
      requestId: "r-9",
      previousRecord: null,
      day: "2026-04-21"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "dailyRecord", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Daily record");
    expect(markup).toContain("claude-opus-4-7");
    expect(markup).toContain("Saved 7,500 tokens (82.5%)");
  });

  it("renders an all-time record row with the previous record delta", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      tokensSaved: 12000,
      savingsPercent: 91,
      model: "claude-opus-4-7",
      provider: "anthropic",
      requestId: "r-42",
      previousRecord: 9500,
      day: null
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "allTimeRecord", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("All-time record");
    expect(markup).toContain("Saved 12,000 tokens (91.0%)");
    expect(markup).toContain("previous record 9,500");
  });

  it("renders a new model row with model and provider", () => {
    const data: NewModelEvent = {
      observedAt: "2026-04-21T09:00:00Z",
      model: "claude-haiku-4-7",
      provider: "anthropic"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "newModel", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("New model");
    expect(markup).toContain("First compression on claude-haiku-4-7");
    expect(markup).toContain(">anthropic<");
  });

  it("renders a streak row without the new-record tag on a threshold event", () => {
    const data: StreakEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      days: 7,
      kind: "threshold"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "streak", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Streak<");
    expect(markup).toContain("7-day active streak");
    expect(markup).not.toContain("new longest");
  });

  it("renders a streak row with the new-record tag on a new_record event", () => {
    const data: StreakEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      days: 12,
      kind: "new_record"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "streak", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("new longest");
    expect(markup).toContain("new personal best");
  });

  it("renders a savings milestone row with the dollar amount", () => {
    const data: SavingsMilestoneEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      milestoneUsd: 100,
      kind: "first_100"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "savingsMilestone", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Savings milestone");
    expect(markup).toContain("$100");
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
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "weeklyRecap", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Weekly recap");
    expect(markup).toContain("2026-04-20");
    expect(markup).toContain("2026-04-26");
    expect(markup).toContain("12,500 tokens saved");
    expect(markup).toContain("$4.25");
    expect(markup).toContain("5 active days");
  });

  it("renders a learnings milestone row with the extracted-count copy", () => {
    const data: LearningsMilestoneEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      count: 3,
      kind: "first_3"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "learningsMilestone", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Learning milestone");
    expect(markup).toContain("3 patterns extracted");
  });

  it("renders a project badge on learnings with a project-scoped memory", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [memory({ scope: "project:/Users/u/Code/headroom-desktop" })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__project");
    expect(markup).toContain(">headroom-desktop<");
    expect(markup).not.toContain("activity-feed__scope");
  });

  it("falls back to scope display when scope is not project-prefixed", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [memory({ scope: "user" })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__scope");
    expect(markup).not.toContain("activity-feed__project");
  });

  it("renders a workspace badge on a transformation when workspace is set", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [transformation({ workspace: "/Users/u/Code/demo-repo" })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__project");
    expect(markup).toContain(">demo-repo<");
  });

  it("omits the workspace badge when workspace is missing", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [transformation()]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("activity-feed__project");
  });

  it("falls back to 'unknown' provider and 0 savings when transformation fields are null", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
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
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("unknown");
    expect(markup).toContain("Saved 0 tokens (0.0%)");
    expect(markup).not.toContain("activity-feed__delta");
    expect(markup).not.toContain("activity-feed__transforms");
  });

  it("paginates at 10 events per page and shows navigation when there are more", () => {
    const events: ActivityEvent[] = Array.from({ length: 23 }, (_, i) =>
      transformation({ requestId: `req-${i}`, timestamp: `2026-04-21T10:${String(i).padStart(2, "0")}:00Z` })
    );
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    // First page: 10 rows.
    const rowCount = (markup.match(/<li class="activity-feed__item /g) ?? []).length;
    expect(rowCount).toBe(10);
    expect(markup).toContain("Page 1 of 3");
    expect(markup).toContain("23 total");
    expect(markup).toContain("← Prev");
    expect(markup).toContain("Next →");
  });

  it("hides pagination when there are 10 or fewer events", () => {
    const events: ActivityEvent[] = Array.from({ length: 7 }, (_, i) =>
      transformation({ requestId: `req-${i}`, timestamp: `2026-04-21T10:0${i}:00Z` })
    );
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("activity-feed__pagination");
    expect(markup).not.toContain("Page 1 of");
  });
});
