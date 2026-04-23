import { useMemo, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { Bell, WifiSlash } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { formatDateTime, formatRelativeTime } from "../lib/dashboardHelpers";
import { estimateCostSavingsUsd, formatEstimatedUsd } from "../lib/modelPricing";
import type {
  ActivityEvent,
  ActivityFeedResponse,
  LearningsMilestoneEvent,
  MemoryFeedEvent,
  MilestoneEvent,
  NewModelEvent,
  RecordEvent,
  RecordTag,
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

const PAGE_SIZE = 5;
const MIN_MEMORY_EVIDENCE = 2;

export function ActivityFeed({
  feed,
  error,
  loaded = true,
  projectPaths = []
}: ActivityFeedProps) {
  const [page, setPage] = useState(0);
  // Memoize the derived feed shapes so page changes (or any re-render that
  // doesn't actually change `feed.events`) skip the O(N) filter + coalesce
  // pass. Combined with the signature-based bail in App.tsx's poll, identical
  // polls become cheap: same `feed.events` reference → memo hits → no work.
  const filteredEvents = useMemo(() => filterLowSignal(feed.events), [feed.events]);
  const visibleEvents = useMemo(() => coalesceFeed(filteredEvents), [filteredEvents]);
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
      ) : !feed.proxyReachable && visibleEvents.length === 0 ? (
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
                Page {clampedPage + 1} of {totalPages}
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
  | { type: "rtkGroup"; events: RtkBatchEvent[] }
  | { type: "memoryGroup"; events: MemoryFeedEvent[] }
  | { type: "dayHeader"; dayKey: string; label: string };

// Minimum same-kind run length required to emit a group row. Kinds absent
// from this map never coalesce (milestones/records/streaks are rare enough
// that each deserves its own row). RTK coalesces in pairs; memory needs a
// burst of 3+ because a single learning has distinct content worth seeing
// but a spammy burst (the common case when `headroom learn` extracts a
// batch of similar patterns) should fold into one group. Transformations
// are not coalesced because `filterLowSignal` keeps only the single largest
// one in the window — presence of compression is already conveyed by the
// savings graph and the big number, so one "Recent large compression" row
// is all we need.
const COALESCE_MIN_LENGTH: Partial<Record<ActivityEvent["kind"], number>> = {
  rtkBatch: 2,
  memory: 3
};

// Break a coalesced run when consecutive same-kind events are further apart
// than this. Picks a natural "work session" boundary — a morning burst and
// an afternoon burst become two groups instead of one blob.
const COALESCE_GAP_MS = 30 * 60 * 1000;

// Max different-kind events allowed to sit between two same-kind events
// without breaking the run. Handles the common case of 3 RTK batches in a
// minute with 1 learning interleaved — the RTKs still coalesce. Absorbed
// interlopers still render (as their own single row) right after the group.
const MAX_INTERLOPERS = 1;

function filterLowSignal(events: ActivityEvent[]): ActivityEvent[] {
  // Of all compressions in the window, keep only the single largest (by
  // tokens saved). The stream of individual compression events is already
  // summarised by the savings graph and the big totals number, so a long
  // chronological list of them drowns out more interesting activity. One
  // "recent large compression" card captures the only compression detail
  // worth surfacing in this view.
  let largestTransformation: ActivityEvent | null = null;
  let largestTokens = -1;
  for (const event of events) {
    if (event.kind !== "transformation") continue;
    const tokens = event.data.tokensSaved ?? 0;
    if (tokens > largestTokens) {
      largestTokens = tokens;
      largestTransformation = event;
    }
  }
  return events.filter((event) => {
    if (event.kind === "transformation") {
      return event === largestTransformation;
    }
    if (event.kind === "memory") {
      return event.data.evidenceCount >= MIN_MEMORY_EVIDENCE;
    }
    return true;
  });
}

function eventTimestampMs(event: ActivityEvent): number {
  switch (event.kind) {
    case "transformation":
      return Date.parse(event.data.timestamp ?? "") || 0;
    case "memory":
      return Date.parse(event.data.createdAt) || 0;
    case "rtkBatch":
    case "milestone":
    case "record":
    case "newModel":
    case "streak":
    case "savingsMilestone":
    case "learningsMilestone":
      return Date.parse(event.data.observedAt) || 0;
    case "weeklyRecap":
      return Date.parse(event.data.observedAt ?? event.data.weekStart) || 0;
  }
}

function localDayKey(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function dayHeaderLabel(dayKey: string, now: Date = new Date()): string {
  if (!dayKey) return "";
  const today = localDayKey(now.getTime());
  const yesterday = localDayKey(now.getTime() - 86_400_000);
  if (dayKey === today) return "Today";
  if (dayKey === yesterday) return "Yesterday";
  const [y, m, d] = dayKey.split("-").map(Number);
  const dt = new Date(y, m - 1, d);
  const sameYear = dt.getFullYear() === now.getFullYear();
  return new Intl.DateTimeFormat(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    year: sameYear ? undefined : "numeric"
  }).format(dt);
}

function coalesceFeed(events: ActivityEvent[]): FeedRow[] {
  const out: FeedRow[] = [];
  const consumed = new Set<number>();
  let lastDayKey: string | null = null;
  for (let i = 0; i < events.length; i++) {
    if (consumed.has(i)) continue;
    const event = events[i];
    const ts = eventTimestampMs(event);
    const dayKey = localDayKey(ts);
    if (dayKey && dayKey !== lastDayKey) {
      out.push({ type: "dayHeader", dayKey, label: dayHeaderLabel(dayKey) });
      lastDayKey = dayKey;
    }
    const minRun = COALESCE_MIN_LENGTH[event.kind];
    if (!minRun) {
      out.push({ type: "single", event });
      continue;
    }
    // Walk forward collecting same-kind events into a group, allowing up to
    // MAX_INTERLOPERS different-kind events to pass through without ending
    // the run. Bail on a calendar-day flip or a gap larger than
    // COALESCE_GAP_MS. Interlopers still render (as their own single rows)
    // right after the group so nothing disappears.
    const groupIndices: number[] = [i];
    const interloperIndices: number[] = [];
    let prevTs = ts;
    let interlopersUsed = 0;
    for (let j = i + 1; j < events.length; j++) {
      if (consumed.has(j)) break;
      const candidate = events[j];
      const candidateTs = eventTimestampMs(candidate);
      if (localDayKey(candidateTs) !== dayKey) break;
      if (Math.abs(prevTs - candidateTs) > COALESCE_GAP_MS) break;
      if (candidate.kind === event.kind) {
        groupIndices.push(j);
        prevTs = candidateTs;
      } else if (interlopersUsed < MAX_INTERLOPERS) {
        interloperIndices.push(j);
        interlopersUsed++;
      } else {
        break;
      }
    }
    if (groupIndices.length < minRun) {
      out.push({ type: "single", event });
      continue;
    }
    if (event.kind === "rtkBatch") {
      out.push({
        type: "rtkGroup",
        events: groupIndices.map(
          (idx) => (events[idx] as Extract<ActivityEvent, { kind: "rtkBatch" }>).data
        )
      });
    } else if (event.kind === "memory") {
      out.push({
        type: "memoryGroup",
        events: groupIndices.map(
          (idx) => (events[idx] as Extract<ActivityEvent, { kind: "memory" }>).data
        )
      });
    }
    for (const idx of groupIndices) consumed.add(idx);
    for (const idx of interloperIndices) {
      out.push({ type: "single", event: events[idx] });
      consumed.add(idx);
    }
  }
  return out;
}

function feedRowKey(row: FeedRow, index: number): string {
  if (row.type === "dayHeader") {
    return `day-${row.dayKey}`;
  }
  if (row.type === "single") {
    return singleKey(row.event, index);
  }
  if (row.type === "memoryGroup") {
    return `mg-${row.events[0].id}-${row.events.length}`;
  }
  return `rg-${row.events[0].observedAt}-${row.events.length}`;
}

function singleKey(event: ActivityEvent, index: number): string {
  switch (event.kind) {
    case "transformation":
      return `t-${event.data.requestId ?? event.data.timestamp ?? index}`;
    case "memory":
      // Include createdAt so the React key survives a theoretical ID
      // collision without bleeding state between sibling MemoryRows.
      return `m-${event.data.id}-${event.data.createdAt}`;
    case "rtkBatch":
      return `rtk-${event.data.observedAt}`;
    case "milestone":
      return `ms-${event.data.milestoneTokensSaved}-${event.data.observedAt}`;
    case "record": {
      const id =
        event.data.requestId ?? event.data.turnId ?? event.data.observedAt;
      return `rec-${event.data.tags.join(",")}-${id}`;
    }
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
  if (row.type === "dayHeader") {
    return (
      <li className="activity-feed__day-header" aria-label={`Events from ${row.label}`}>
        <span>{row.label}</span>
      </li>
    );
  }
  if (row.type === "rtkGroup") {
    return <RtkGroupRow events={row.events} />;
  }
  if (row.type === "memoryGroup") {
    return <MemoryGroupRow events={row.events} projectPaths={projectPaths} />;
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
    case "record":
      return <RecordRow event={event.data} />;
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

/**
 * Wraps a feed row and toggles an expanded detail block below the main
 * content when clicked. No-op when `detail` is null — the row renders
 * non-clickable and the caller just gets a plain `<li>` wrapper.
 */
function ExpandableRow({
  className,
  detail,
  children
}: {
  className: string;
  detail: ReactNode | null;
  children: ReactNode;
}) {
  const [expanded, setExpanded] = useState(false);
  const canExpand = detail != null;
  /* v8 ignore start — interactive handlers require a DOM; SSR tests can pin
     role/aria/class but cannot dispatch click or keyboard events. Same reason
     OptimizePanel.tsx is excluded from coverage entirely. */
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
  /* v8 ignore stop */
  return (
    <li
      className={
        className +
        (canExpand ? " activity-feed__item--clickable" : "") +
        (expanded ? " is-expanded" : "")
      }
      role={canExpand ? "button" : undefined}
      tabIndex={canExpand ? 0 : undefined}
      aria-expanded={canExpand ? expanded : undefined}
      onClick={toggle}
      onKeyDown={onKeyDown}
    >
      {children}
      {expanded && detail ? (
        <div className="activity-feed__detail">{detail}</div>
      ) : null}
    </li>
  );
}

function TimeChip({ iso }: { iso: string | null | undefined }) {
  return (
    <span className="activity-feed__time" title={formatDateTime(iso)}>
      {formatRelativeTime(iso)}
    </span>
  );
}

function RtkGroupRow({ events }: { events: RtkBatchEvent[] }) {
  const commandsDelta = events.reduce((sum, e) => sum + e.commandsDelta, 0);
  const tokensDelta = events.reduce((sum, e) => sum + e.tokensSavedDelta, 0);
  const latest = events[0];
  const detail = (
    <ul className="activity-feed__detail-list">
      {events.map((ev, i) => (
        <li key={`rg-sub-${ev.observedAt}-${i}`} className="activity-feed__detail-item">
          <TimeChip iso={ev.observedAt} />
          <span className="activity-feed__detail-primary">
            +{ev.commandsDelta.toLocaleString()} command
            {ev.commandsDelta === 1 ? "" : "s"}, saved{" "}
            {ev.tokensSavedDelta.toLocaleString()} tokens
          </span>
        </li>
      ))}
    </ul>
  );
  return (
    <ExpandableRow
      className="activity-feed__item activity-feed__item--rtk"
      detail={detail}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--rtk">
          RTK × {events.length}
        </span>
        <TimeChip iso={latest.observedAt} />
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          +{commandsDelta.toLocaleString()} command{commandsDelta === 1 ? "" : "s"}, saved{" "}
          {tokensDelta.toLocaleString()} tokens
        </strong>
      </div>
    </ExpandableRow>
  );
}

function MemoryGroupRow({
  events,
  projectPaths
}: {
  events: MemoryFeedEvent[];
  projectPaths: string[];
}) {
  const latest = events[0];
  // Resolve each learning's project and keep the value only if every learning
  // in the group agrees. Mixed groups drop the badge — otherwise the label
  // would misrepresent learnings from other projects.
  const perEventProject = events.map((ev) => {
    const scopeProject = projectBasename(ev.scope);
    if (scopeProject) return scopeProject;
    const matched = matchProjectPath(ev.content, projectPaths);
    return matched ? basenameOf(matched) : null;
  });
  const sharedProject =
    perEventProject[0] && perEventProject.every((p) => p === perEventProject[0])
      ? perEventProject[0]
      : null;
  const detail = (
    <ul className="activity-feed__detail-list">
      {events.map((ev) => (
        <li
          key={`mg-sub-${ev.id}-${ev.createdAt}`}
          className="activity-feed__detail-item"
        >
          <TimeChip iso={ev.createdAt} />
          <p className="activity-feed__content">{ev.content}</p>
        </li>
      ))}
    </ul>
  );
  return (
    <ExpandableRow
      className="activity-feed__item activity-feed__item--memory"
      detail={detail}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--memory">
          Learning × {events.length}
        </span>
        <TimeChip iso={latest.createdAt} />
        {sharedProject ? (
          <span className="activity-feed__project">{sharedProject}</span>
        ) : null}
      </div>
      <p className="activity-feed__content activity-feed__content--clamped-one">
        {latest.content}
      </p>
    </ExpandableRow>
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
  const hasExactTokens =
    event.inputTokensOriginal != null && event.inputTokensOptimized != null;
  const hasRequestId = !!event.requestId;
  const hasRawTransforms = event.transformsApplied.length > 0;
  const groups = hasRawTransforms ? groupTransforms(event.transformsApplied) : [];
  const groupsWithTargets = groups.filter((g) => g.targets.length > 0);
  const estimatedUsd = estimateCostSavingsUsd(event.model, saved);
  const hasExtra =
    hasRequestId || hasRawTransforms || event.workspace != null || estimatedUsd != null;
  const detail = hasExtra ? (
    <dl className="activity-feed__detail-grid">
      {estimatedUsd != null ? (
        <>
          <dt>Estimated cost saved</dt>
          <dd>{formatEstimatedUsd(estimatedUsd)}</dd>
        </>
      ) : null}
      {hasExactTokens ? (
        <>
          <dt>Tokens in → out</dt>
          <dd>
            {event.inputTokensOriginal!.toLocaleString()} →{" "}
            {event.inputTokensOptimized!.toLocaleString()}
          </dd>
        </>
      ) : null}
      {groupsWithTargets.length > 0 ? (
        <>
          <dt>What was touched</dt>
          <dd>
            <ul className="activity-feed__targets">
              {groupsWithTargets.map((grp) => (
                <li key={grp.label} className="activity-feed__target">
                  <span className="activity-feed__target-label">{grp.label}</span>
                  <span className="activity-feed__target-values">
                    {grp.targets.join(", ")}
                  </span>
                </li>
              ))}
            </ul>
          </dd>
        </>
      ) : null}
      {event.workspace ? (
        <>
          <dt>Workspace</dt>
          <dd className="activity-feed__detail-mono">{event.workspace}</dd>
        </>
      ) : null}
      {hasRequestId ? (
        <>
          <dt>Request ID</dt>
          <dd className="activity-feed__detail-mono">{event.requestId}</dd>
        </>
      ) : null}
    </dl>
  ) : null;
  return (
    <ExpandableRow
      className="activity-feed__item activity-feed__item--transformation"
      detail={detail}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--transformation">
          Recent large compression
        </span>
        <TimeChip iso={event.timestamp} />
        <span className="activity-feed__provider">{event.provider ?? "unknown"}</span>
        {event.model ? <span className="activity-feed__model">{event.model}</span> : null}
        {workspace ? (
          <span className="activity-feed__project">{workspace}</span>
        ) : null}
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          Saved {saved.toLocaleString()} tokens ({pct.toFixed(1)}%)
          {estimatedUsd != null ? (
            <span className="activity-feed__savings-usd">
              {" "}
              · {formatEstimatedUsd(estimatedUsd)}
            </span>
          ) : null}
        </strong>
        {hasExactTokens ? (
          <span className="activity-feed__delta">
            {event.inputTokensOriginal!.toLocaleString()} →{" "}
            {event.inputTokensOptimized!.toLocaleString()}
          </span>
        ) : null}
      </div>
      {hasRawTransforms ? (
        <ul className="activity-feed__transforms">
          {groups.map((grp) => (
            <li
              key={grp.label}
              className="activity-feed__transform"
              title={chipTitle(grp)}
            >
              {grp.count > 1 ? `${grp.label} × ${grp.count}` : grp.label}
            </li>
          ))}
        </ul>
      ) : null}
    </ExpandableRow>
  );
}

function chipTitle(grp: TransformGroup): string {
  const base = grp.count > 1 ? `${grp.title} (×${grp.count})` : grp.title;
  if (grp.targets.length === 0) return base;
  const preview = grp.targets.slice(0, 3).join(", ");
  const suffix =
    grp.targets.length > 3 ? `${preview}, +${grp.targets.length - 3} more` : preview;
  return `${base} — ${suffix}`;
}

/**
 * Collapses a transformsApplied list into one entry per friendly label with a
 * count. A single compression that fires 70 "Stale Read"s renders as one
 * "Stale Read × 70" chip instead of 70 identical chips flooding the row.
 * Preserves first-seen order so the display is stable.
 */
export function groupTransforms(
  raws: string[]
): TransformGroup[] {
  const byLabel = new Map<string, TransformGroup>();
  for (const raw of raws) {
    const { label, title, target } = formatTransform(raw);
    const existing = byLabel.get(label);
    if (existing) {
      existing.count += 1;
      if (target && !existing.targets.includes(target)) {
        existing.targets.push(target);
      }
    } else {
      byLabel.set(label, {
        label,
        title,
        count: 1,
        targets: target ? [target] : []
      });
    }
  }
  return Array.from(byLabel.values());
}

export interface TransformGroup {
  label: string;
  title: string;
  count: number;
  // Accumulated across all raws that mapped to this label (file paths for
  // read_lifecycle, tool names for tool_crush, etc.). Empty for transforms
  // emitted by older proxies that don't carry attribution, and for
  // transforms that have no meaningful target (e.g. cache_align).
  targets: string[];
}

function formatTransform(raw: string): { label: string; title: string; target?: string } {
  // Exact-match table for known labels (covers older proxies that emit the
  // un-enriched form, and transforms that have no parameter tail).
  const exact: Record<string, { label: string; title: string }> = {
    "read_lifecycle:stale": { label: "Stale Read", title: "file edited after read" },
    "read_lifecycle:superseded": { label: "Superseded Read", title: "file re-read later" },
    "interceptor:ast-grep": { label: "ast-grep", title: "semantic code search" },
    "router:excluded:tool": { label: "Tool result excluded", title: "tool output dropped" },
    "router:protected:user_message": {
      label: "Protected: user message",
      title: "user message preserved"
    },
    "router:protected:system_message": {
      label: "Protected: system message",
      title: "system message preserved"
    },
    "router:protected:recent_code": {
      label: "Protected: recent code",
      title: "recent code preserved"
    },
    "router:protected:analysis_context": {
      label: "Protected: analysis context",
      title: "analysis preserved"
    },
    cache_align: { label: "Cache aligned", title: "aligned to cache boundary" }
  };

  const hit = exact[raw];
  if (hit) return hit;

  // read_lifecycle:<state>:<file_path> — new enriched form from upstream PR.
  // Bound the split to 3 parts so paths containing `:` survive.
  if (raw.startsWith("read_lifecycle:")) {
    const parts = splitColonN(raw, 3);
    if (parts.length === 3 && parts[2]) {
      const state = parts[1];
      const target = parts[2];
      if (state === "stale") {
        return { label: "Stale Read", title: "file edited after read", target };
      }
      if (state === "superseded") {
        return { label: "Superseded Read", title: "file re-read later", target };
      }
    }
  }

  // tool_crush:<n>[:<name1,name2,...>]. The tail may be absent when the proxy
  // couldn't resolve tool names (legacy shape) — fall back to the count-only
  // label.
  const crushWithNames = /^tool_crush:(\d+):(.+)$/.exec(raw);
  if (crushWithNames) {
    const n = Number(crushWithNames[1]);
    return {
      label: `Crushed ${n} tool${n === 1 ? "" : "s"}`,
      title: "tool outputs compacted",
      target: crushWithNames[2]
    };
  }
  const crush = /^tool_crush:(\d+)$/.exec(raw);
  if (crush) {
    const n = Number(crush[1]);
    return {
      label: `Crushed ${n} tool${n === 1 ? "" : "s"}`,
      title: "tool outputs compacted"
    };
  }

  const breakpoints = /^inserted_(\d+)_cache_breakpoints$/.exec(raw);
  if (breakpoints) {
    const n = Number(breakpoints[1]);
    return {
      label: `Inserted ${n} cache breakpoint${n === 1 ? "" : "s"}`,
      title: "cache prefix tuned"
    };
  }

  const routerTool = /^router:tool_result:(.+)$/.exec(raw);
  if (routerTool) {
    return { label: `Tool result: ${routerTool[1]}`, title: "tool result compressed" };
  }

  const routerRatio = /^router:([^:]+):([\d.]+)$/.exec(raw);
  if (routerRatio) {
    return {
      label: `Compressed: ${routerRatio[1]} (${routerRatio[2]}x)`,
      title: "router compression"
    };
  }

  const kompress = /^kompress:([^:]+):([\d.]+)$/.exec(raw);
  if (kompress) {
    return {
      label: `Kompress ${kompress[1]} (${kompress[2]}x)`,
      title: "kompress compression"
    };
  }

  const cacheOpt = /^cache_optimizer:(.+)$/.exec(raw);
  if (cacheOpt) {
    return { label: `Cache optimizer: ${cacheOpt[1]}`, title: "cache tuning" };
  }

  // Unknown transform — render verbatim, tooltip shows the raw id.
  return { label: raw, title: raw };
}

function splitColonN(s: string, parts: number): string[] {
  if (parts <= 1) return [s];
  const result: string[] = [];
  let cursor = 0;
  for (let i = 0; i < parts - 1; i++) {
    const idx = s.indexOf(":", cursor);
    if (idx === -1) {
      result.push(s.slice(cursor));
      return result;
    }
    result.push(s.slice(cursor, idx));
    cursor = idx + 1;
  }
  result.push(s.slice(cursor));
  return result;
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
  /* v8 ignore start — interactive handlers require a DOM; see ExpandableRow. */
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
  /* v8 ignore stop */
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
        <TimeChip iso={event.createdAt} />
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
  const detail = (
    <dl className="activity-feed__detail-grid">
      <dt>Observed</dt>
      <dd>{formatDateTime(event.observedAt)}</dd>
      <dt>Lifetime commands</dt>
      <dd>{event.totalCommands.toLocaleString()}</dd>
      <dt>Lifetime tokens saved</dt>
      <dd>{event.totalSaved.toLocaleString()}</dd>
    </dl>
  );
  return (
    <ExpandableRow
      className="activity-feed__item activity-feed__item--rtk"
      detail={detail}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--rtk">RTK</span>
        <TimeChip iso={event.observedAt} />
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
    </ExpandableRow>
  );
}

function MilestoneRow({ event }: { event: MilestoneEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--milestone">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--milestone">Milestone</span>
        <TimeChip iso={event.observedAt} />
      </div>
      <p className="activity-feed__content">
        Crossed {formatTokensShort(event.milestoneTokensSaved)} lifetime tokens saved.
      </p>
    </li>
  );
}

const RECORD_TAG_ORDER: RecordTag[] = ["turn", "daily", "weekly", "allTime"];

const RECORD_TAG_LABEL: Record<RecordTag, string> = {
  daily: "Daily",
  weekly: "Weekly",
  allTime: "All-time",
  turn: "Turn"
};

function RecordRow({ event }: { event: RecordEvent }) {
  const workspace = workspaceBasename(event.workspace);
  const pct = event.savingsPercent;
  const callCount = event.callCount ?? null;
  const orderedTags = RECORD_TAG_ORDER.filter((tag) => event.tags.includes(tag));
  return (
    <li className="activity-feed__item activity-feed__item--record">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--record">Record</span>
        {orderedTags.map((tag) => (
          <span
            key={tag}
            className={`activity-feed__tag activity-feed__tag--${tag}`}
          >
            {RECORD_TAG_LABEL[tag]}
          </span>
        ))}
        <TimeChip iso={event.observedAt} />
        {event.model ? <span className="activity-feed__model">{event.model}</span> : null}
        {workspace ? <span className="activity-feed__project">{workspace}</span> : null}
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          Saved {event.tokensSaved.toLocaleString()} tokens
          {pct != null ? ` (${pct.toFixed(1)}%)` : ""}
          {callCount != null && callCount > 0
            ? ` across ${callCount} call${callCount === 1 ? "" : "s"}`
            : ""}
        </strong>
        {event.previousRecord != null ? (
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
        <TimeChip iso={event.observedAt} />
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
        <TimeChip iso={event.observedAt} />
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
        <TimeChip iso={event.observedAt} />
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
        <TimeChip iso={event.observedAt} />
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
        <TimeChip iso={event.observedAt} />
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
