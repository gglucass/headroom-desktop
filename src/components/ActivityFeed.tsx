import { useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
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
  // True after the first fetch attempt resolves. Before that the `feed`
  // prop is a default placeholder whose `proxyReachable: false` would
  // otherwise render the "proxy unreachable" state on initial load.
  loaded?: boolean;
  // Paths of known Claude projects, used to heuristically associate a memory
  // row with a project by substring match on the memory's content. Mirrors
  // the logic in `pattern_matches_project` in the Rust backend. Memories do
  // not carry an explicit project link today (scope/entity_refs are absent
  // from the Python export), so content-matching is the only signal.
  projectPaths?: string[];
}

const PAGE_SIZE = 10;
const MIN_MEMORY_EVIDENCE = 2;

export function ActivityFeed({
  feed,
  error,
  loaded = true,
  projectPaths = []
}: ActivityFeedProps) {
  const [page, setPage] = useState(0);
  const visibleEvents = coalesceFeed(filterLowSignal(feed.events));
  const totalPages = Math.max(1, Math.ceil(visibleEvents.length / PAGE_SIZE));
  const clampedPage = Math.min(page, totalPages - 1);
  const start = clampedPage * PAGE_SIZE;
  const pageEvents = visibleEvents.slice(start, start + PAGE_SIZE);

  return (
    <>
      <header className="activity-feed__header">
        <h2>Activity</h2>
        <p className="activity-feed__subtitle">
          Compressions, learnings, RTK saves, milestones, and records — everything Headroom is
          doing.
        </p>
      </header>
      {error ? (
        <p className="loading-copy">{error}</p>
      ) : !loaded ? (
        <div className="activity-feed__skeleton" aria-hidden="true">
          <div className="activity-feed__skeleton-row" />
          <div className="activity-feed__skeleton-row" />
          <div className="activity-feed__skeleton-row" />
        </div>
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
      ) : visibleEvents.length === 0 ? (
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
            {pageEvents.map((row, index) => (
              <FeedRowItem
                key={feedRowKey(row, start + index)}
                row={row}
                projectPaths={projectPaths}
              />
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
                Page {clampedPage + 1} of {totalPages} · {visibleEvents.length} total
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

type FeedRow =
  | { type: "single"; event: ActivityEvent }
  | { type: "transformationGroup"; events: TransformationFeedEvent[] }
  | { type: "rtkGroup"; events: RtkBatchEvent[] };

// Memory is intentionally NOT coalesced — each entry has distinct content
// worth seeing individually (and is clamped to one line with click-to-expand).
// Celebratory/summary events (milestones, records, streaks, recaps) are rare
// enough that coalescing them isn't worth the lost specificity.
const COALESCE_KINDS: ReadonlySet<ActivityEvent["kind"]> = new Set([
  "transformation",
  "rtkBatch"
]);

function filterLowSignal(events: ActivityEvent[]): ActivityEvent[] {
  return events.filter((event) => {
    if (event.kind === "memory") {
      return event.data.evidenceCount >= MIN_MEMORY_EVIDENCE;
    }
    return true;
  });
}

function coalesceFeed(events: ActivityEvent[]): FeedRow[] {
  const out: FeedRow[] = [];
  let i = 0;
  while (i < events.length) {
    const event = events[i];
    if (!COALESCE_KINDS.has(event.kind)) {
      out.push({ type: "single", event });
      i++;
      continue;
    }
    let j = i + 1;
    while (j < events.length && events[j].kind === event.kind) {
      j++;
    }
    const runLength = j - i;
    if (runLength === 1) {
      out.push({ type: "single", event });
    } else if (event.kind === "transformation") {
      out.push({
        type: "transformationGroup",
        events: events.slice(i, j).map((e) => (e as Extract<ActivityEvent, { kind: "transformation" }>).data)
      });
    } else if (event.kind === "rtkBatch") {
      out.push({
        type: "rtkGroup",
        events: events.slice(i, j).map((e) => (e as Extract<ActivityEvent, { kind: "rtkBatch" }>).data)
      });
    } else {
      out.push({ type: "single", event });
    }
    i = j;
  }
  return out;
}

function feedRowKey(row: FeedRow, index: number): string {
  if (row.type === "single") {
    return singleKey(row.event, index);
  }
  if (row.type === "transformationGroup") {
    const first = row.events[0];
    return `tg-${first.requestId ?? first.timestamp ?? index}-${row.events.length}`;
  }
  return `rg-${row.events[0].observedAt}-${row.events.length}`;
}

function singleKey(event: ActivityEvent, index: number): string {
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

function FeedRowItem({ row, projectPaths }: { row: FeedRow; projectPaths: string[] }) {
  if (row.type === "transformationGroup") {
    return <TransformationGroupRow events={row.events} />;
  }
  if (row.type === "rtkGroup") {
    return <RtkGroupRow events={row.events} />;
  }
  const event = row.event;
  switch (event.kind) {
    case "transformation":
      return <TransformationRow event={event.data} />;
    case "memory":
      return <MemoryRow event={event.data} projectPaths={projectPaths} />;
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

function TransformationGroupRow({ events }: { events: TransformationFeedEvent[] }) {
  const totalSaved = events.reduce((sum, e) => sum + (e.tokensSaved ?? 0), 0);
  const avgPct =
    events.reduce((sum, e) => sum + (e.savingsPercent ?? 0), 0) / events.length;
  const latest = events[0];
  return (
    <li className="activity-feed__item activity-feed__item--transformation">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--transformation">
          Compression × {events.length}
        </span>
        <span className="activity-feed__time">{formatDateTime(latest.timestamp)}</span>
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          Saved {totalSaved.toLocaleString()} tokens ({avgPct.toFixed(1)}% avg)
        </strong>
      </div>
    </li>
  );
}

function RtkGroupRow({ events }: { events: RtkBatchEvent[] }) {
  const commandsDelta = events.reduce((sum, e) => sum + e.commandsDelta, 0);
  const tokensDelta = events.reduce((sum, e) => sum + e.tokensSavedDelta, 0);
  const latest = events[0];
  return (
    <li className="activity-feed__item activity-feed__item--rtk">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--rtk">
          RTK × {events.length}
        </span>
        <span className="activity-feed__time">{formatDateTime(latest.observedAt)}</span>
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          +{commandsDelta.toLocaleString()} command{commandsDelta === 1 ? "" : "s"}, saved{" "}
          {tokensDelta.toLocaleString()} tokens
        </strong>
      </div>
    </li>
  );
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

const basenameOf = workspaceBasename;

// Heuristic project attribution for memories without explicit scope/entity_refs.
// Mirrors `pattern_matches_project` in src-tauri/src/lib.rs: a memory "belongs
// to" a project if its content contains the project's root path followed by a
// boundary character (/, ", or `). Longest match wins so `/foo/bar` beats `/foo`
// when both are candidates.
function matchProjectPath(content: string, projectPaths: string[]): string | null {
  let best: string | null = null;
  for (const path of projectPaths) {
    const root = path.replace(/\/+$/, "");
    if (!root) continue;
    if (
      content.includes(root + "/") ||
      content.includes(root + "\"") ||
      content.includes(root + "`")
    ) {
      if (!best || root.length > best.length) best = root;
    }
  }
  return best;
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

function MemoryRow({
  event,
  projectPaths
}: {
  event: MemoryFeedEvent;
  projectPaths: string[];
}) {
  const [expanded, setExpanded] = useState(false);
  // Prefer an explicit project scope (`project:/path`) if the backend ever
  // emits one; fall back to a substring match against known project paths.
  // Mirrors `pattern_matches_project` in the Rust backend — memories today
  // carry no formal project link, so this content-based heuristic is the
  // only signal available.
  const scopeProject = projectBasename(event.scope);
  const matchedPath = scopeProject ? null : matchProjectPath(event.content, projectPaths);
  const project = scopeProject ?? (matchedPath ? basenameOf(matchedPath) : null);
  const canExpand = event.content.length > 60 || event.content.includes("\n");
  const toggle = () => {
    if (canExpand) setExpanded((prev) => !prev);
  };
  const onKeyDown = (e: ReactKeyboardEvent<HTMLLIElement>) => {
    if (!canExpand) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      toggle();
    }
  };
  return (
    <li
      className={
        "activity-feed__item activity-feed__item--memory" +
        (canExpand ? " activity-feed__item--clickable" : "") +
        (expanded ? " is-expanded" : "")
      }
      role={canExpand ? "button" : undefined}
      tabIndex={canExpand ? 0 : undefined}
      aria-expanded={canExpand ? expanded : undefined}
      onClick={toggle}
      onKeyDown={onKeyDown}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--memory">Learning</span>
        <span className="activity-feed__time">{formatDateTime(event.createdAt)}</span>
        {project ? (
          <span className="activity-feed__project">{project}</span>
        ) : null}
      </div>
      <p
        className={
          expanded
            ? "activity-feed__content"
            : "activity-feed__content activity-feed__content--clamped-one"
        }
        title={canExpand && !expanded ? event.content : undefined}
      >
        {event.content}
      </p>
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
