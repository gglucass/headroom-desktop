import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ActivityFeed, groupTransforms } from "./ActivityFeed";
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
  TrainSuggestionEvent,
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
      evidenceCount: 2,
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

  it("shows the waiting state when proxy is not reachable and no events", () => {
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={{ ...baseFeed, proxyReachable: false }} error={null} />
    );
    expect(markup).toContain("Waiting for the Headroom proxy");
    expect(markup).not.toContain("activity-feed__list");
  });

  it("surfaces persisted events even when the proxy is unreachable", () => {
    // Rust merges persisted compression history in when the live proxy fetch
    // fails, sending them with proxyReachable=false. The feed must render
    // them instead of the "Waiting" empty state — otherwise restarts look
    // blank until the proxy re-comes online.
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      proxyReachable: false,
      events: [transformation({ requestId: "from-history" })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__list");
    expect(markup).not.toContain("Waiting for the Headroom proxy");
  });

  it("shows the empty state when proxy is up but no events", () => {
    const markup = renderToStaticMarkup(<ActivityFeed feed={baseFeed} error={null} />);
    expect(markup).toContain("No requests yet");
  });

  it("renders a transformation row with provider, model, savings, delta, and transforms", () => {
    const feed: ActivityFeedResponse = { ...baseFeed, events: [transformation()] };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Recent large compression");
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

  it("shows an estimated dollar savings alongside tokens saved", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
          model: "claude-sonnet-4-6",
          tokensSaved: 750_000,
          savingsPercent: 75
        })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    // sonnet: $3/M × 0.75M = $2.25
    expect(markup).toContain("~$2.25");
  });

  it("surfaces file paths from enriched read_lifecycle tags in the detail view", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
          transformsApplied: [
            "read_lifecycle:stale:/src/App.tsx",
            "read_lifecycle:stale:/src/lib/foo.ts",
            "tool_crush:2:Bash,Grep"
          ]
        })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("/src/App.tsx");
    expect(markup).toContain("/src/lib/foo.ts");
    expect(markup).toContain("Bash,Grep");
    // Chip row still collapses to per-label count regardless of target count.
    expect(markup).toContain("Stale Read × 2");
    expect(markup).toContain("Crushed 2 tools");
  });

  it("falls back to the raw transform string when unknown", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [transformation({ transformsApplied: ["something:new:format"] })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("something:new:format");
  });

  it("collapses repeated transforms into a single count chip", () => {
    // Before: 70 identical stale-read transforms rendered 70 separate chips
    // and flooded the row. Now: one chip "Stale Read × 70".
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
          transformsApplied: [
            ...Array(70).fill("read_lifecycle:stale"),
            ...Array(42).fill("router:excluded:tool"),
            "cache_align"
          ]
        })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const chipCount = (markup.match(/<li class="activity-feed__transform"/g) ?? []).length;
    expect(chipCount).toBe(3);
    expect(markup).toContain("Stale Read × 70");
    expect(markup).toContain("Tool result excluded × 42");
    expect(markup).toContain(">Cache aligned<");
  });

  it("renders a memory row with content", () => {
    const feed: ActivityFeedResponse = { ...baseFeed, events: [memory()] };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Learning");
    expect(markup).toContain("user prefers tabs over spaces");
  });

  it("hides learnings below the evidence threshold", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [memory({ id: "weak", content: "noisy one-off", evidenceCount: 1 })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("noisy one-off");
    expect(markup).toContain("No requests yet");
  });

  it("keeps only the single largest compression and drops the rest", () => {
    // Presence of compression is already conveyed by the savings graph and
    // totals; a chronological list of every compression drowns out rarer
    // events. The feed surfaces one "Recent large compression" row sourced
    // from the biggest compression (by tokens saved) in the window.
    const events: ActivityEvent[] = [
      transformation({ requestId: "a", timestamp: "2026-04-21T10:02:00Z", tokensSaved: 100, savingsPercent: 10 }),
      transformation({ requestId: "biggest", timestamp: "2026-04-21T10:01:00Z", tokensSaved: 9_999, savingsPercent: 90 }),
      transformation({ requestId: "c", timestamp: "2026-04-21T10:00:00Z", tokensSaved: 300, savingsPercent: 30 })
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const rowCount = (markup.match(/<li class="activity-feed__item /g) ?? []).length;
    expect(rowCount).toBe(1);
    expect(markup).toContain("Recent large compression");
    expect(markup).toContain("Saved 9,999 tokens (90.0%)");
    expect(markup).not.toContain("Compression × ");
  });

  it("renders time chips using relative time with an absolute-date tooltip", () => {
    const now = new Date("2026-04-21T10:00:00Z");
    vi.useFakeTimers();
    vi.setSystemTime(now);
    try {
      const feed: ActivityFeedResponse = {
        ...baseFeed,
        events: [
          transformation({
            requestId: "relnow",
            timestamp: "2026-04-21T09:50:00Z"
          })
        ]
      };
      const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
      expect(markup).toContain("10m ago");
      expect(markup).toMatch(/title="[^"]*2026[^"]*"/);
    } finally {
      vi.useRealTimers();
    }
  });

  it("exposes transformation detail (request ID + raw transforms) in an expandable row", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        transformation({
          requestId: "req-abc-123",
          transformsApplied: ["interceptor:ast-grep", "cache_align"],
          workspace: "/Users/u/Code/demo"
        })
      ]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    // Row is marked clickable and carries the detail block so client render
    // can toggle it. SSR renders it with expanded=false, so the detail text
    // isn't visible, but the button role + aria wiring are pinned here.
    expect(markup).toContain("activity-feed__item--clickable");
    expect(markup).toContain('role="button"');
    expect(markup).toContain('aria-expanded="false"');
  });

  it("inserts a day-header row when a single compression on an earlier day survives the filter", () => {
    // Only the largest compression across all days survives filterLowSignal;
    // here that's the one from 2026-04-21. A memory on 2026-04-22 provides a
    // second day so the renderer emits two day-headers, confirming the
    // header logic still fires even though transformations no longer group.
    // Pin "now" so neither fixture date is "today" (today's header is hidden).
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-04-25T10:00:00Z"));
    try {
      const events: ActivityEvent[] = [
        memory({ id: "mem-late", createdAt: "2026-04-22T12:00:00Z" }),
        transformation({ requestId: "earlier", timestamp: "2026-04-21T12:00:00Z", tokensSaved: 9_999, savingsPercent: 90 })
      ];
      const feed: ActivityFeedResponse = { ...baseFeed, events };
      const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
      const headerCount = (markup.match(/<li class="activity-feed__day-header"/g) ?? []).length;
      expect(headerCount).toBe(2);
      expect(markup).toContain("Recent large compression");
      expect(markup).not.toContain("Compression × ");
    } finally {
      vi.useRealTimers();
    }
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
      tags: ["daily"],
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
      events: [{ kind: "record", data }]
    };
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
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "record", data }]
    };
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
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "record", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Record<");
    expect(markup).toContain(">Daily<");
    expect(markup).toContain(">All-time<");
    expect(markup).toContain("Saved 15,000 tokens (88.2%)");
    expect(markup).toContain("previous record 10,000");
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

  it("renders a never-trained TrainSuggestion row with project + session count", () => {
    const data: TrainSuggestionEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      projectPath: "/Users/u/Code/demo-repo",
      projectDisplayName: "demo-repo",
      sessionCount: 7,
      activeDaysSinceLastLearn: 0,
      kind: "never_trained"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "trainSuggestion", data }]
    };
    const markup = renderToStaticMarkup(
      <ActivityFeed
        feed={feed}
        error={null}
        onNavigateToOptimize={() => {}}
      />
    );
    expect(markup).toContain("activity-feed__item--train");
    expect(markup).toContain("activity-feed__badge--train");
    expect(markup).toContain("Try Train");
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
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "trainSuggestion", data }]
    };
    const markup = renderToStaticMarkup(
      <ActivityFeed
        feed={feed}
        error={null}
        onNavigateToOptimize={() => {}}
      />
    );
    expect(markup).toContain("Retrain");
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
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "trainSuggestion", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__item--train");
    expect(markup).not.toContain("activity-feed__item--clickable");
    expect(markup).not.toContain('role="button"');
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

  it("omits the scope chip when scope is not project-prefixed", () => {
    // Memory scope/entity_refs are absent from the Python export today, so the
    // fallback chip was always literal "unknown" noise. We drop it entirely and
    // only render a project chip when there's actually a project path.
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [memory({ scope: "user" })]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("activity-feed__scope");
    expect(markup).not.toContain("activity-feed__project");
  });

  it("attributes a memory to a known project by substring-matching content", () => {
    // Mirrors `pattern_matches_project` in the Rust backend: memories carry no
    // formal project link, so the project chip is inferred from a path match
    // on the memory's content.
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        memory({
          scope: "user",
          content:
            "File `/Users/u/Code/demo-repo/src/main.rs` does not exist. The correct path is ..."
        })
      ]
    };
    const markup = renderToStaticMarkup(
      <ActivityFeed
        feed={feed}
        error={null}
        projectPaths={["/Users/u/Code/other", "/Users/u/Code/demo-repo"]}
      />
    );
    expect(markup).toContain("activity-feed__project");
    expect(markup).toContain(">demo-repo<");
  });

  it("picks the longest matching project path when multiple match", () => {
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [
        memory({
          scope: "user",
          content: "Error in `/Users/u/Code/demo/packages/web/src/index.ts`"
        })
      ]
    };
    const markup = renderToStaticMarkup(
      <ActivityFeed
        feed={feed}
        error={null}
        projectPaths={["/Users/u/Code/demo", "/Users/u/Code/demo/packages/web"]}
      />
    );
    expect(markup).toContain(">web<");
    expect(markup).not.toContain(">demo<");
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

  it("paginates at 5 activity rows per page and shows navigation when there are more", () => {
    // Memory rows coalesce into groups when 3+ land within COALESCE_GAP_MS of
    // each other, so this fixture spaces them 35 minutes apart to keep every
    // entry as its own row. Day headers do not count toward PAGE_SIZE — they
    // render in addition to the 5 activity rows. All 13 entries share the
    // same local day (2026-04-21, pinned below as not "today" so the header
    // emits), so page 1 shows 5 item rows plus 1 day header = 6 rows total.
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-04-25T10:00:00Z"));
    try {
      const events: ActivityEvent[] = Array.from({ length: 13 }, (_, i) => {
        const minutes = i * 35;
        const hh = String(Math.floor(minutes / 60)).padStart(2, "0");
        const mm = String(minutes % 60).padStart(2, "0");
        return memory({ id: `mem-${i}`, createdAt: `2026-04-21T${hh}:${mm}:00Z` });
      });
      const feed: ActivityFeedResponse = { ...baseFeed, events };
      const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
      const itemCount = (markup.match(/<li class="activity-feed__item /g) ?? []).length;
      const headerCount = (markup.match(/<li class="activity-feed__day-header"/g) ?? []).length;
      expect(itemCount).toBe(5);
      expect(headerCount).toBe(1);
      expect(markup).toContain("Page 1 of 3");
      expect(markup).toContain("← Prev");
      expect(markup).toContain("Next →");
    } finally {
      vi.useRealTimers();
    }
  });

  it("hides pagination when there are 5 or fewer events", () => {
    const events: ActivityEvent[] = Array.from({ length: 4 }, (_, i) =>
      memory({ id: `mem-${i}`, createdAt: `2026-04-21T${10 + i}:00:00Z` })
    );
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("activity-feed__pagination");
    expect(markup).not.toContain("Page 1 of");
  });

  it("coalesces consecutive rtkBatch events into a single RTK group row", () => {
    const batches: ActivityEvent[] = [
      {
        kind: "rtkBatch",
        data: {
          observedAt: "2026-04-21T10:05:00Z",
          commandsDelta: 3,
          tokensSavedDelta: 1200,
          totalCommands: 10,
          totalSaved: 5000
        }
      },
      {
        kind: "rtkBatch",
        data: {
          observedAt: "2026-04-21T10:04:00Z",
          commandsDelta: 1,
          tokensSavedDelta: 400,
          totalCommands: 7,
          totalSaved: 3800
        }
      }
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events: batches };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("activity-feed__item--rtk");
    expect(markup).toContain("RTK × 2");
    expect(markup).toContain("+4 commands");
    expect(markup).toContain("1,600 tokens");
  });

  it("coalesces RTK events across a single non-RTK interloper", () => {
    const events: ActivityEvent[] = [
      {
        kind: "rtkBatch",
        data: {
          observedAt: "2026-04-21T10:05:00Z",
          commandsDelta: 4,
          tokensSavedDelta: 2000,
          totalCommands: 20,
          totalSaved: 10000
        }
      },
      memory({
        id: "mem-between",
        createdAt: "2026-04-21T10:04:30Z",
        content: "a learning dropped in between the RTK batches"
      }),
      {
        kind: "rtkBatch",
        data: {
          observedAt: "2026-04-21T10:04:00Z",
          commandsDelta: 3,
          tokensSavedDelta: 1000,
          totalCommands: 17,
          totalSaved: 8000
        }
      }
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("RTK × 2");
    expect(markup).toContain("+7 commands");
    // The interloper still renders — absorbed events are never dropped.
    expect(markup).toContain("a learning dropped in between");
    // Group row precedes the absorbed interloper in the rendered order.
    const groupIdx = markup.indexOf("RTK × 2");
    const interIdx = markup.indexOf("a learning dropped in between");
    expect(groupIdx).toBeLessThan(interIdx);
  });

  it("does not coalesce two RTK events split by two interlopers", () => {
    const events: ActivityEvent[] = [
      {
        kind: "rtkBatch",
        data: {
          observedAt: "2026-04-21T10:05:00Z",
          commandsDelta: 2,
          tokensSavedDelta: 500,
          totalCommands: 12,
          totalSaved: 6000
        }
      },
      memory({
        id: "mem-a",
        createdAt: "2026-04-21T10:04:40Z",
        content: "first interloper learning"
      }),
      memory({
        id: "mem-b",
        createdAt: "2026-04-21T10:04:20Z",
        content: "second interloper learning"
      }),
      {
        kind: "rtkBatch",
        data: {
          observedAt: "2026-04-21T10:04:00Z",
          commandsDelta: 1,
          tokensSavedDelta: 300,
          totalCommands: 10,
          totalSaved: 5500
        }
      }
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    // MAX_INTERLOPERS = 1, so two interlopers force both RTKs to stand alone.
    expect(markup).not.toContain("RTK × 2");
  });

  it("coalesces a burst of 3+ memories into a Learning group", () => {
    const events: ActivityEvent[] = [
      memory({ id: "m1", createdAt: "2026-04-21T10:03:00Z", content: "first learning" }),
      memory({ id: "m2", createdAt: "2026-04-21T10:02:00Z", content: "second learning" }),
      memory({ id: "m3", createdAt: "2026-04-21T10:01:00Z", content: "third learning" })
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Learning × 3");
    // The group collapses the three memory rows into one — no individual row
    // should render. Preview line shows the latest learning's content.
    expect(markup).toContain("first learning");
    expect(markup).toContain("activity-feed__item--clickable");
    expect(markup).toContain('aria-expanded="false"');
    const itemCount = (markup.match(/<li class="activity-feed__item /g) ?? []).length;
    expect(itemCount).toBe(1);
  });

  it("does not coalesce a pair of memories (threshold is 3)", () => {
    const events: ActivityEvent[] = [
      memory({ id: "m1", createdAt: "2026-04-21T10:02:00Z", content: "alpha learning" }),
      memory({ id: "m2", createdAt: "2026-04-21T10:01:00Z", content: "beta learning" })
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).not.toContain("Learning × 2");
    expect(markup).toContain("alpha learning");
    expect(markup).toContain("beta learning");
  });

  it("shows a shared project badge on a memory group when all learnings match", () => {
    const projectPaths = ["/Users/u/Code/demo"];
    const content = "touched /Users/u/Code/demo/file.ts in this run";
    const events: ActivityEvent[] = [
      memory({ id: "m1", createdAt: "2026-04-21T10:03:00Z", content }),
      memory({ id: "m2", createdAt: "2026-04-21T10:02:00Z", content }),
      memory({ id: "m3", createdAt: "2026-04-21T10:01:00Z", content })
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(
      <ActivityFeed feed={feed} error={null} projectPaths={projectPaths} />
    );
    expect(markup).toContain("Learning × 3");
    expect(markup).toContain(">demo<");
  });

  it("renders a turn record row with totals, call count, and previous record", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      tags: ["turn"],
      tokensSaved: 3_210,
      savingsPercent: null,
      model: "claude-opus-4-7",
      provider: null,
      requestId: null,
      previousRecord: 2_500,
      day: null,
      turnId: "turn-X",
      callCount: 4,
      workspace: "/Users/u/Code/demo"
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "record", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain(">Record<");
    expect(markup).toContain(">Turn<");
    expect(markup).toContain("Saved 3,210 tokens across 4 calls");
    expect(markup).toContain("previous record 2,500");
    expect(markup).toContain(">demo<");
    expect(markup).toContain(">claude-opus-4-7<");
  });

  it("renders a turn record row without previous/workspace when null", () => {
    const data: RecordEvent = {
      observedAt: "2026-04-22T10:00:00Z",
      tags: ["turn"],
      tokensSaved: 100,
      savingsPercent: null,
      model: null,
      provider: null,
      requestId: null,
      previousRecord: null,
      day: null,
      turnId: "turn-Y",
      callCount: 1,
      workspace: null
    };
    const feed: ActivityFeedResponse = {
      ...baseFeed,
      events: [{ kind: "record", data }]
    };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    expect(markup).toContain("Saved 100 tokens across 1 call");
    expect(markup).not.toContain("previous record");
    expect(markup).not.toContain("activity-feed__project");
    expect(markup).not.toContain("activity-feed__model");
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
