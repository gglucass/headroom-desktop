import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ActivityFeed, formatRequestMessages, groupTransforms } from "./ActivityFeed";
import type {
  ActivityEvent,
  ActivityFeedResponse,
  LearningsMilestoneEvent,
  MemoryFeedEvent,
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

  it("makes the row expandable when request/response messages are logged, and skips it when null", () => {
    // Static markup keeps the detail pane collapsed, so we can't assert on
    // the <pre> contents here. But the row gains `activity-feed__item--clickable`
    // iff any detail field is populated — that's the signal the row is
    // render-wise aware of the extra data. Per-field rendering is covered by
    // the formatRequestMessages unit tests below.
    const withMessages = renderToStaticMarkup(
      <ActivityFeed
        feed={{
          ...baseFeed,
          events: [
            transformation({
              // Strip every other detail-triggering field so clickability
              // can only come from request/response.
              requestId: null,
              workspace: null,
              transformsApplied: [],
              tokensSaved: 0,
              model: null,
              requestMessages: [{ role: "user", content: "hi" }]
            })
          ]
        }}
        error={null}
      />
    );
    expect(withMessages).toContain("activity-feed__item--clickable");

    const withoutMessages = renderToStaticMarkup(
      <ActivityFeed
        feed={{
          ...baseFeed,
          logFullMessages: false,
          events: [
            transformation({
              requestId: null,
              workspace: null,
              transformsApplied: [],
              tokensSaved: 0,
              model: null,
              requestMessages: null,
              responseContent: null
            })
          ]
        }}
        error={null}
      />
    );
    expect(withoutMessages).not.toContain("activity-feed__item--clickable");
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

  it("surfaces only the most recent compression in the transformation tile", () => {
    // One tile per kind — the compression tile updates its content to reflect
    // the most recent transformation in the window. Older compressions are
    // implied by the savings graph and totals elsewhere.
    const events: ActivityEvent[] = [
      transformation({ requestId: "latest", timestamp: "2026-04-21T10:02:00Z", tokensSaved: 100, savingsPercent: 10 }),
      transformation({ requestId: "middle", timestamp: "2026-04-21T10:01:00Z", tokensSaved: 9_999, savingsPercent: 90 }),
      transformation({ requestId: "oldest", timestamp: "2026-04-21T10:00:00Z", tokensSaved: 300, savingsPercent: 30 })
    ];
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const rowCount = (markup.match(/<li class="activity-feed__item /g) ?? []).length;
    expect(rowCount).toBe(1);
    expect(markup).toContain("Recent large compression");
    expect(markup).toContain("Saved 100 tokens (10.0%)");
    expect(markup).not.toContain("9,999");
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

  it("renders both kinds in the fixed tile order (transformation before memory)", () => {
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
    // Tiles render in TILE_ORDER: transformation before memory.
    expect(transformationIdx).toBeLessThan(memoryIdx);
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

  it("collapses many memories into a single Learning tile (latest wins)", () => {
    const events: ActivityEvent[] = Array.from({ length: 13 }, (_, i) => {
      const minutes = i * 35;
      const hh = String(Math.floor(minutes / 60)).padStart(2, "0");
      const mm = String(minutes % 60).padStart(2, "0");
      return memory({
        id: `mem-${i}`,
        createdAt: `2026-04-21T${hh}:${mm}:00Z`,
        content: `learning #${i}`
      });
    });
    const feed: ActivityFeedResponse = { ...baseFeed, events };
    const markup = renderToStaticMarkup(<ActivityFeed feed={feed} error={null} />);
    const itemCount = (markup.match(/<li class="activity-feed__item /g) ?? []).length;
    // One tile per kind; all 13 memories are the same kind.
    expect(itemCount).toBe(1);
    // Latest memory (highest index) wins the tile.
    expect(markup).toContain("learning #12");
    // No pagination UI remains.
    expect(markup).not.toContain("activity-feed__pagination");
    expect(markup).not.toContain("Page 1 of");
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

describe("formatRequestMessages", () => {
  it("emits role + plain string content (OpenAI shape)", () => {
    expect(
      formatRequestMessages([
        { role: "user", content: "please refactor parseFoo" },
        { role: "assistant", content: "ok — reading it now" }
      ])
    ).toBe("user:\nplease refactor parseFoo\n\nassistant:\nok — reading it now");
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
    // A tool_use or tool_result block has no surfaced `text` — rather than
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

  it("labels a missing role as (unknown) instead of rendering a bare newline", () => {
    expect(formatRequestMessages([{ content: "orphan content" }])).toBe(
      "(unknown):\norphan content"
    );
  });
});
