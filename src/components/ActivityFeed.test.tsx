import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ActivityFeed } from "./ActivityFeed";
import type {
  ActivityEvent,
  ActivityFeedResponse,
  MemoryFeedEvent,
  TransformationFeedEvent
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
    expect(markup).toContain("interceptor:ast-grep");
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
});
