import { useState } from "react";
import { Bell, WifiSlash } from "@phosphor-icons/react";
import { formatDateTime } from "../lib/dashboardHelpers";
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

interface ActivityFeedProps {
  feed: ActivityFeedResponse;
  error: string | null;
}

const PAGE_SIZE = 10;

export function ActivityFeed({ feed, error }: ActivityFeedProps) {
  const [page, setPage] = useState(0);
  const totalPages = Math.max(1, Math.ceil(feed.events.length / PAGE_SIZE));
  const clampedPage = Math.min(page, totalPages - 1);
  const start = clampedPage * PAGE_SIZE;
  const pageEvents = feed.events.slice(start, start + PAGE_SIZE);

  return (
    <>
      <header className="activity-feed__header">
        <h2>Live activity</h2>
        <p className="activity-feed__subtitle">
          Compressions, learnings, RTK saves, milestones, and records — everything Headroom is
          doing, as it happens.
        </p>
      </header>
      {error ? (
        <p className="loading-copy">{error}</p>
      ) : !feed.proxyReachable ? (
        <div className="activity-feed__empty">
          <div className="activity-feed__empty-icon activity-feed__empty-icon--waiting" aria-hidden="true">
            <WifiSlash weight="duotone" />
          </div>
          <p className="activity-feed__empty-title">Waiting for the Headroom proxy</p>
          <p className="activity-feed__empty-body">
            Headroom will reconnect as soon as the proxy is back online.
          </p>
        </div>
      ) : feed.events.length === 0 ? (
        <div className="activity-feed__empty">
          <div className="activity-feed__empty-icon" aria-hidden="true">
            <Bell weight="duotone" />
          </div>
          <p className="activity-feed__empty-title">No requests yet</p>
          <p className="activity-feed__empty-body">
            Send a message through Claude Code and you'll see compressions and learnings
            stream in here.
          </p>
        </div>
      ) : (
        <>
          <ul className="activity-feed__list">
            {pageEvents.map((event, index) => (
              <ActivityRow key={activityKey(event, start + index)} event={event} />
            ))}
          </ul>
          {totalPages > 1 ? (
            <nav className="activity-feed__pagination" aria-label="Activity pagination">
              <button
                type="button"
                className="activity-feed__page-button"
                onClick={() => setPage((p) => Math.max(0, p - 1))}
                disabled={clampedPage === 0}
              >
                ← Prev
              </button>
              <span className="activity-feed__page-indicator">
                Page {clampedPage + 1} of {totalPages} · {feed.events.length} total
              </span>
              <button
                type="button"
                className="activity-feed__page-button"
                onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
                disabled={clampedPage >= totalPages - 1}
              >
                Next →
              </button>
            </nav>
          ) : null}
        </>
      )}
    </>
  );
}

function activityKey(event: ActivityEvent, index: number): string {
  switch (event.kind) {
    case "transformation":
      return `t-${event.data.requestId ?? event.data.timestamp ?? index}`;
    case "memory":
      return `m-${event.data.id}`;
    case "rtkBatch":
      return `rtk-${event.data.observedAt}`;
    case "milestone":
      return `ms-${event.data.milestoneTokensSaved}-${event.data.observedAt}`;
    case "dailyRecord":
      return `dr-${event.data.day ?? ""}-${event.data.observedAt}`;
    case "allTimeRecord":
      return `atr-${event.data.tokensSaved}-${event.data.observedAt}`;
    case "newModel":
      return `nm-${event.data.model}-${event.data.observedAt}`;
    case "streak":
      return `streak-${event.data.days}-${event.data.kind}-${event.data.observedAt}`;
    case "savingsMilestone":
      return `smile-${event.data.milestoneUsd}-${event.data.observedAt}`;
    case "weeklyRecap":
      return `wr-${event.data.weekStart}`;
    case "learningsMilestone":
      return `lm-${event.data.count}-${event.data.observedAt}`;
  }
}

function ActivityRow({ event }: { event: ActivityEvent }) {
  switch (event.kind) {
    case "transformation":
      return <TransformationRow event={event.data} />;
    case "memory":
      return <MemoryRow event={event.data} />;
    case "rtkBatch":
      return <RtkBatchRow event={event.data} />;
    case "milestone":
      return <MilestoneRow event={event.data} />;
    case "dailyRecord":
      return <RecordRow event={event.data} kind="daily" />;
    case "allTimeRecord":
      return <RecordRow event={event.data} kind="allTime" />;
    case "newModel":
      return <NewModelRow event={event.data} />;
    case "streak":
      return <StreakRow event={event.data} />;
    case "savingsMilestone":
      return <SavingsMilestoneRow event={event.data} />;
    case "weeklyRecap":
      return <WeeklyRecapRow event={event.data} />;
    case "learningsMilestone":
      return <LearningsMilestoneRow event={event.data} />;
  }
}

function projectBasename(scope: string): string | null {
  if (!scope.startsWith("project:")) return null;
  const path = scope.slice("project:".length);
  const segments = path.split("/").filter(Boolean);
  return segments.length > 0 ? segments[segments.length - 1] : null;
}

function workspaceBasename(path: string | null | undefined): string | null {
  if (!path) return null;
  const segments = path.split("/").filter(Boolean);
  return segments.length > 0 ? segments[segments.length - 1] : null;
}

function TransformationRow({ event }: { event: TransformationFeedEvent }) {
  const saved = event.tokensSaved ?? 0;
  const pct = event.savingsPercent ?? 0;
  const workspace = workspaceBasename(event.workspace);
  return (
    <li className="activity-feed__item activity-feed__item--transformation">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--transformation">
          Compression
        </span>
        <span className="activity-feed__time">{formatDateTime(event.timestamp)}</span>
        <span className="activity-feed__provider">{event.provider ?? "unknown"}</span>
        {event.model ? <span className="activity-feed__model">{event.model}</span> : null}
        {workspace ? (
          <span className="activity-feed__project">{workspace}</span>
        ) : null}
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          Saved {saved.toLocaleString()} tokens ({pct.toFixed(1)}%)
        </strong>
        {event.inputTokensOriginal != null && event.inputTokensOptimized != null ? (
          <span className="activity-feed__delta">
            {event.inputTokensOriginal.toLocaleString()} →{" "}
            {event.inputTokensOptimized.toLocaleString()}
          </span>
        ) : null}
      </div>
      {event.transformsApplied.length > 0 ? (
        <ul className="activity-feed__transforms">
          {event.transformsApplied.map((t) => {
            const { label, title } = formatTransform(t);
            return (
              <li key={t} className="activity-feed__transform" title={title}>
                {label}
              </li>
            );
          })}
        </ul>
      ) : null}
    </li>
  );
}

function formatTransform(raw: string): { label: string; title: string } {
  // Exact-match table for known labels.
  const exact: Record<string, { label: string; title: string }> = {
    "read_lifecycle:stale": {
      label: "Stale Read",
      title: "Read output replaced — file was edited after this Read"
    },
    "read_lifecycle:superseded": {
      label: "Superseded Read",
      title: "Read output replaced — file was re-Read later in the conversation"
    },
    "interceptor:ast-grep": {
      label: "ast-grep",
      title: "Tool-result interceptor applied semantic code search"
    },
    "router:excluded:tool": {
      label: "Tool result excluded",
      title: "Content router excluded a tool result from the prompt"
    },
    "router:protected:user_message": {
      label: "Protected: user message",
      title: "Content router preserved a user message from compression"
    },
    "router:protected:system_message": {
      label: "Protected: system message",
      title: "Content router preserved a system message from compression"
    },
    "router:protected:recent_code": {
      label: "Protected: recent code",
      title: "Content router preserved recent code from compression"
    },
    "router:protected:analysis_context": {
      label: "Protected: analysis context",
      title: "Content router preserved analysis context from compression"
    },
    cache_align: {
      label: "Cache aligned",
      title: "Conversation aligned to a stable cache boundary"
    }
  };

  const hit = exact[raw];
  if (hit) return hit;

  // Prefix patterns with variable tails.
  const crush = /^tool_crush:(\d+)$/.exec(raw);
  if (crush) {
    const n = Number(crush[1]);
    return {
      label: `Crushed ${n} tool${n === 1 ? "" : "s"}`,
      title: "Compacted tool-use blocks into a shorter form"
    };
  }

  const breakpoints = /^inserted_(\d+)_cache_breakpoints$/.exec(raw);
  if (breakpoints) {
    const n = Number(breakpoints[1]);
    return {
      label: `Inserted ${n} cache breakpoint${n === 1 ? "" : "s"}`,
      title: "Added cache breakpoints to improve prompt-cache hit rate"
    };
  }

  const routerTool = /^router:tool_result:(.+)$/.exec(raw);
  if (routerTool) {
    return {
      label: `Tool result: ${routerTool[1]}`,
      title: `Content router compressed a tool result with strategy ${routerTool[1]}`
    };
  }

  const routerRatio = /^router:([^:]+):([\d.]+)$/.exec(raw);
  if (routerRatio) {
    return {
      label: `Compressed: ${routerRatio[1]} (${routerRatio[2]}x)`,
      title: `Content router used ${routerRatio[1]} at ${routerRatio[2]}x compression`
    };
  }

  const kompress = /^kompress:([^:]+):([\d.]+)$/.exec(raw);
  if (kompress) {
    return {
      label: `Kompress ${kompress[1]} (${kompress[2]}x)`,
      title: `Kompress compressed ${kompress[1]} messages at ${kompress[2]}x`
    };
  }

  const cacheOpt = /^cache_optimizer:(.+)$/.exec(raw);
  if (cacheOpt) {
    return {
      label: `Cache optimizer: ${cacheOpt[1]}`,
      title: "Cache optimizer adjusted prompt for better caching"
    };
  }

  // Unknown transform — render verbatim, tooltip shows the raw id.
  return { label: raw, title: raw };
}

function MemoryRow({ event }: { event: MemoryFeedEvent }) {
  const [expanded, setExpanded] = useState(false);
  // Heuristic: show the toggle if the content is long enough to plausibly
  // wrap beyond two lines at typical widths.
  const canExpand = event.content.length > 140 || event.content.includes("\n");
  const project = projectBasename(event.scope);
  return (
    <li className="activity-feed__item activity-feed__item--memory">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--memory">Learning</span>
        <span className="activity-feed__time">{formatDateTime(event.createdAt)}</span>
        {project ? (
          <span className="activity-feed__project">{project}</span>
        ) : (
          <span className="activity-feed__scope">{event.scope}</span>
        )}
        <span className="activity-feed__importance">
          importance {event.importance.toFixed(2)}
        </span>
      </div>
      <p
        className={
          expanded
            ? "activity-feed__content"
            : "activity-feed__content activity-feed__content--clamped"
        }
        title={canExpand && !expanded ? event.content : undefined}
      >
        {event.content}
      </p>
      {canExpand ? (
        <button
          type="button"
          className="activity-feed__expand"
          onClick={() => setExpanded((prev) => !prev)}
        >
          {expanded ? "Show less" : "Show more"}
        </button>
      ) : null}
    </li>
  );
}

function RtkBatchRow({ event }: { event: RtkBatchEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--rtk">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--rtk">RTK</span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          +{event.commandsDelta.toLocaleString()} command
          {event.commandsDelta === 1 ? "" : "s"}, saved{" "}
          {event.tokensSavedDelta.toLocaleString()} tokens
        </strong>
        <span className="activity-feed__delta">
          lifetime {event.totalCommands.toLocaleString()} ·{" "}
          {event.totalSaved.toLocaleString()} tokens
        </span>
      </div>
    </li>
  );
}

function MilestoneRow({ event }: { event: MilestoneEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--milestone">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--milestone">Milestone</span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
      </div>
      <p className="activity-feed__content">
        Crossed {formatTokensShort(event.milestoneTokensSaved)} lifetime tokens saved.
      </p>
    </li>
  );
}

function RecordRow({
  event,
  kind
}: {
  event: RecordEvent;
  kind: "daily" | "allTime";
}) {
  const badgeLabel = kind === "daily" ? "Daily record" : "All-time record";
  const badgeClass =
    kind === "daily"
      ? "activity-feed__badge--daily-record"
      : "activity-feed__badge--all-time-record";
  const itemClass =
    kind === "daily"
      ? "activity-feed__item--daily-record"
      : "activity-feed__item--all-time-record";
  const pct = event.savingsPercent;
  const workspace = workspaceBasename(event.workspace);
  return (
    <li className={`activity-feed__item ${itemClass}`}>
      <div className="activity-feed__row activity-feed__row--meta">
        <span className={`activity-feed__badge ${badgeClass}`}>{badgeLabel}</span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
        {event.model ? <span className="activity-feed__model">{event.model}</span> : null}
        {workspace ? (
          <span className="activity-feed__project">{workspace}</span>
        ) : null}
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          Saved {event.tokensSaved.toLocaleString()} tokens
          {pct != null ? ` (${pct.toFixed(1)}%)` : ""}
        </strong>
        {kind === "allTime" && event.previousRecord != null ? (
          <span className="activity-feed__delta">
            previous record {event.previousRecord.toLocaleString()}
          </span>
        ) : null}
      </div>
    </li>
  );
}

function NewModelRow({ event }: { event: NewModelEvent }) {
  const workspace = workspaceBasename(event.workspace);
  return (
    <li className="activity-feed__item activity-feed__item--new-model">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--new-model">New model</span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
        {event.provider ? (
          <span className="activity-feed__provider">{event.provider}</span>
        ) : null}
        {workspace ? (
          <span className="activity-feed__project">{workspace}</span>
        ) : null}
      </div>
      <p className="activity-feed__content">First compression on {event.model}.</p>
    </li>
  );
}

function LearningsMilestoneRow({ event }: { event: LearningsMilestoneEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--learnings-milestone">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--learnings-milestone">
          Learning milestone
        </span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
      </div>
      <p className="activity-feed__content">
        {event.count} patterns extracted from your work so far.
      </p>
    </li>
  );
}

function StreakRow({ event }: { event: StreakEvent }) {
  const isRecord = event.kind === "new_record";
  return (
    <li className="activity-feed__item activity-feed__item--streak">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--streak">Streak</span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
        {isRecord ? (
          <span className="activity-feed__streak-record">new longest</span>
        ) : null}
      </div>
      <p className="activity-feed__content">
        {event.days}-day active streak
        {isRecord ? " — new personal best!" : "!"}
      </p>
    </li>
  );
}

function SavingsMilestoneRow({ event }: { event: SavingsMilestoneEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--savings-milestone">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--savings-milestone">
          Savings milestone
        </span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
      </div>
      <p className="activity-feed__content">
        Lifetime savings crossed ${event.milestoneUsd.toLocaleString()}.
      </p>
    </li>
  );
}

function WeeklyRecapRow({ event }: { event: WeeklyRecapEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--weekly-recap">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--weekly-recap">
          Weekly recap
        </span>
        <span className="activity-feed__time">{formatDateTime(event.observedAt)}</span>
        <span className="activity-feed__week-range">
          {event.weekStart} – {event.weekEnd}
        </span>
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          {event.totalTokensSaved.toLocaleString()} tokens saved, $
          {event.totalSavingsUsd.toFixed(2)}
        </strong>
        <span className="activity-feed__delta">
          {event.activeDays} active day{event.activeDays === 1 ? "" : "s"}
        </span>
      </div>
    </li>
  );
}

function formatTokensShort(tokens: number): string {
  if (tokens >= 1_000_000_000) {
    return `${(tokens / 1_000_000_000).toFixed(tokens % 1_000_000_000 === 0 ? 0 : 1)}B`;
  }
  if (tokens >= 1_000_000) {
    return `${(tokens / 1_000_000).toFixed(tokens % 1_000_000 === 0 ? 0 : 1)}M`;
  }
  if (tokens >= 1_000) {
    return `${(tokens / 1_000).toFixed(0)}K`;
  }
  return tokens.toLocaleString();
}
