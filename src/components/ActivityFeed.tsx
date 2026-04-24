import { useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { Bell, WifiSlash } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { formatDateTime, formatRelativeTime } from "../lib/dashboardHelpers";
import { estimateCostSavingsUsd, formatEstimatedUsd } from "../lib/modelPricing";
import type {
  ActivityFeedResponse,
  ActivityFeedSnapshot,
  LearningsMilestoneEvent,
  RecordEvent,
  RecordTag,
  RtkBatchEvent,
  TrainSuggestionEvent,
  TransformationFeedEvent,
  TransformationRequestMessage,
  WeeklyRecapEvent
} from "../lib/types";

interface ActivityFeedProps {
  feed: ActivityFeedResponse;
  error: string | null;
  // True after the first fetch attempt resolves. Before that the `feed`
  // prop is a default placeholder whose `proxyReachable: false` would
  // otherwise render the "proxy unreachable" state on initial load.
  loaded?: boolean;
  // Invoked when a TrainSuggestion row is clicked, so the parent can switch
  // tabs. Left optional so the component keeps rendering in contexts that
  // can't navigate (tests, embedded previews).
  onNavigateToOptimize?: () => void;
}

// One entry per tile kind. `kind` matches the `ActivityFeedSnapshot` slot name
// so the backend shape and frontend render stay in lockstep.
type TileKind = keyof ActivityFeedSnapshot;

const EMPTY_TILE_COPY: Record<
  TileKind,
  { badgeClass: string; badgeLabel: string; copy: string; itemModifier: string }
> = {
  trainSuggestion: {
    badgeClass: "activity-feed__badge--train",
    badgeLabel: "Train",
    copy: "No training nudge. Visit Optimize to train any project.",
    itemModifier: "activity-feed__item--train"
  },
  transformation: {
    badgeClass: "activity-feed__badge--transformation",
    badgeLabel: "Compression",
    copy: "No compressions yet — send a message through Claude Code.",
    itemModifier: "activity-feed__item--transformation"
  },
  rtkBatch: {
    badgeClass: "activity-feed__badge--rtk",
    badgeLabel: "RTK",
    copy: "No RTK commands observed yet.",
    itemModifier: "activity-feed__item--rtk"
  },
  record: {
    badgeClass: "activity-feed__badge--record",
    badgeLabel: "Record",
    copy: "No new records yet.",
    itemModifier: "activity-feed__item--record"
  },
  learningsMilestone: {
    badgeClass: "activity-feed__badge--learnings-milestone",
    badgeLabel: "Learnings",
    copy: "No learnings yet.",
    itemModifier: "activity-feed__item--learnings-milestone"
  },
  weeklyRecap: {
    badgeClass: "activity-feed__badge--weekly-recap",
    badgeLabel: "Weekly recap",
    copy: "No recap yet — posts at the end of the week.",
    itemModifier: "activity-feed__item--weekly-recap"
  }
};

export function ActivityFeed({
  feed,
  error,
  loaded = true,
  onNavigateToOptimize
}: ActivityFeedProps) {
  const { tiles } = feed;
  // "Waiting for proxy" only fires when we've got nothing to show AND the
  // proxy isn't answering. If any slot is populated (persisted state from a
  // prior session), fall through to render it even while the proxy is down.
  const hasAnyTile = Object.values(tiles).some((v) => v != null);

  return (
    <>
      <article className="soft-card activity-card">
        <header className="activity-card__head">
          <div className="activity-card__title-row">
            <span className="activity-card__title-icon" aria-hidden="true">
              <Bell weight="duotone" />
            </span>
            <h1>Activity</h1>
          </div>
          <p className="activity-card__blurb">
            Compressions, learnings, RTK saves, milestones, and records — everything Headroom is
            doing.
          </p>
        </header>
      </article>
      {error ? (
        <p className="loading-copy">{error}</p>
      ) : !loaded ? (
        <div className="activity-feed__skeleton" aria-hidden="true">
          <div className="activity-feed__skeleton-row" />
          <div className="activity-feed__skeleton-row" />
          <div className="activity-feed__skeleton-row" />
        </div>
      ) : !feed.proxyReachable && !hasAnyTile ? (
        <div className="activity-feed__empty">
          <div className="activity-feed__empty-icon activity-feed__empty-icon--waiting" aria-hidden="true">
            <WifiSlash weight="duotone" />
          </div>
          <p className="activity-feed__empty-title">Waiting for the Headroom proxy</p>
          <p className="activity-feed__empty-body">
            Headroom will reconnect as soon as the proxy is back online.
          </p>
        </div>
      ) : (
        <ul className="activity-feed__list">
          {tiles.record ? <RecordRow event={tiles.record} /> : <EmptyTile kind="record" />}
          {tiles.transformation ? (
            <TransformationRow event={tiles.transformation} />
          ) : (
            <EmptyTile kind="transformation" />
          )}
          {tiles.learningsMilestone ? (
            <LearningsMilestoneRow event={tiles.learningsMilestone} />
          ) : (
            <EmptyTile kind="learningsMilestone" />
          )}
          {tiles.trainSuggestion ? (
            <TrainSuggestionRow
              event={tiles.trainSuggestion}
              onNavigate={onNavigateToOptimize}
            />
          ) : (
            <EmptyTile kind="trainSuggestion" onNavigateToOptimize={onNavigateToOptimize} />
          )}
          {tiles.rtkBatch ? (
            <RtkBatchRow event={tiles.rtkBatch} />
          ) : (
            <EmptyTile kind="rtkBatch" />
          )}
          {tiles.weeklyRecap ? (
            <WeeklyRecapRow event={tiles.weeklyRecap} />
          ) : (
            <EmptyTile kind="weeklyRecap" />
          )}
        </ul>
      )}
    </>
  );
}

function EmptyTile({
  kind,
  onNavigateToOptimize
}: {
  kind: TileKind;
  onNavigateToOptimize?: () => void;
}) {
  const { badgeClass, badgeLabel, copy, itemModifier } = EMPTY_TILE_COPY[kind];
  const itemClass = `activity-feed__item activity-feed__item--empty ${itemModifier}`;
  const canNavigate = kind === "trainSuggestion" && typeof onNavigateToOptimize === "function";
  const handleActivate = () => {
    if (canNavigate) onNavigateToOptimize?.();
  };
  /* v8 ignore start — keyboard activation requires a DOM; see ExpandableRow. */
  const onKeyDown = (e: ReactKeyboardEvent<HTMLLIElement>) => {
    if (!canNavigate) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      handleActivate();
    }
  };
  /* v8 ignore stop */
  return (
    <li
      className={canNavigate ? `${itemClass} activity-feed__item--clickable` : itemClass}
      role={canNavigate ? "button" : undefined}
      tabIndex={canNavigate ? 0 : undefined}
      onClick={canNavigate ? handleActivate : undefined}
      onKeyDown={onKeyDown}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className={`activity-feed__badge ${badgeClass} activity-feed__badge--empty`}>
          {badgeLabel}
        </span>
      </div>
      <p className="activity-feed__content activity-feed__content--empty">{copy}</p>
    </li>
  );
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

function workspaceBasename(path: string | null | undefined): string | null {
  if (!path) return null;
  const segments = path.split("/").filter(Boolean);
  return segments.length > 0 ? segments[segments.length - 1] : null;
}

// Extract displayable text from a single proxy-logged message. Anthropic sends
// `content` as a block list (text / tool_use / tool_result / ...); OpenAI sends
// a plain string. Text blocks are flattened verbatim; other block types are
// shown as a short `[type]` marker rather than dropped, so the reader sees
// that something was there.
function messageText(msg: TransformationRequestMessage): string {
  const c = msg.content;
  if (typeof c === "string") return c;
  if (Array.isArray(c)) {
    return c
      .map((block) => {
        if (!block || typeof block !== "object") return "";
        if (typeof block.text === "string") return block.text;
        if (typeof block.type === "string") return `[${block.type}]`;
        return "";
      })
      .filter((s) => s.length > 0)
      .join("\n");
  }
  return "";
}

export function formatRequestMessages(messages: TransformationRequestMessage[]): string {
  return messages
    .map((m) => {
      const role = (m.role ?? "").trim() || "(unknown)";
      return `${role}:\n${messageText(m)}`;
    })
    .join("\n\n");
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
  const hasRequestMessages = !!event.requestMessages && event.requestMessages.length > 0;
  const hasCompressedMessages =
    !!event.compressedMessages && event.compressedMessages.length > 0;
  const hasExtra =
    hasRequestId ||
    hasRawTransforms ||
    event.workspace != null ||
    estimatedUsd != null ||
    hasRequestMessages ||
    hasCompressedMessages;
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
      {hasRequestMessages && hasCompressedMessages ? (
        // New proxy shape: both sides present. Render the pre/post pair with
        // token counts so the N → M savings on the headline become legible
        // in terms of the actual message content below.
        <>
          <dt>
            Request (original
            {event.inputTokensOriginal != null
              ? `, ${event.inputTokensOriginal.toLocaleString()} tokens`
              : ""}
            )
          </dt>
          <dd>
            <pre className="activity-feed__message-dump">
              {formatRequestMessages(event.requestMessages!)}
            </pre>
          </dd>
          <dt>
            Request (compressed
            {event.inputTokensOptimized != null
              ? `, ${event.inputTokensOptimized.toLocaleString()} tokens`
              : ""}
            )
          </dt>
          <dd>
            <pre className="activity-feed__message-dump">
              {formatRequestMessages(event.compressedMessages!)}
            </pre>
          </dd>
        </>
      ) : hasRequestMessages ? (
        // Legacy proxy shape: only `requestMessages` exists. Its content may
        // actually be the post-compression list (field was inconsistent
        // across sites before the upstream split) — we can't tell, so label
        // it neutrally and keep today's behaviour.
        <>
          <dt>Request</dt>
          <dd>
            <pre className="activity-feed__message-dump">
              {formatRequestMessages(event.requestMessages!)}
            </pre>
          </dd>
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

const RECORD_TAG_ORDER: RecordTag[] = ["daily", "weekly", "allTime"];

const RECORD_TAG_LABEL: Record<RecordTag, string> = {
  daily: "Daily",
  weekly: "Weekly",
  allTime: "All-time"
};

function RecordRow({ event }: { event: RecordEvent }) {
  const workspace = workspaceBasename(event.workspace);
  const pct = event.savingsPercent;
  const orderedTags = RECORD_TAG_ORDER.filter((tag) => event.tags.includes(tag));
  const hasRequestMessages = !!event.requestMessages && event.requestMessages.length > 0;
  const hasCompressedMessages =
    !!event.compressedMessages && event.compressedMessages.length > 0;
  const detail =
    hasRequestMessages || hasCompressedMessages ? (
      <dl className="activity-feed__detail-grid">
        {hasRequestMessages && hasCompressedMessages ? (
          <>
            <dt>Request (original)</dt>
            <dd>
              <pre className="activity-feed__message-dump">
                {formatRequestMessages(event.requestMessages!)}
              </pre>
            </dd>
            <dt>Request (compressed)</dt>
            <dd>
              <pre className="activity-feed__message-dump">
                {formatRequestMessages(event.compressedMessages!)}
              </pre>
            </dd>
          </>
        ) : hasRequestMessages ? (
          <>
            <dt>Request</dt>
            <dd>
              <pre className="activity-feed__message-dump">
                {formatRequestMessages(event.requestMessages!)}
              </pre>
            </dd>
          </>
        ) : null}
      </dl>
    ) : null;
  return (
    <ExpandableRow
      className="activity-feed__item activity-feed__item--record"
      detail={detail}
    >
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
        </strong>
        {event.previousRecord != null ? (
          <span className="activity-feed__delta">
            previous record {event.previousRecord.toLocaleString()}
          </span>
        ) : null}
      </div>
    </ExpandableRow>
  );
}

function TrainSuggestionRow({
  event,
  onNavigate
}: {
  event: TrainSuggestionEvent;
  onNavigate?: () => void;
}) {
  const isNeverTrained = event.kind === "never_trained";
  const badgeLabel = isNeverTrained ? "Try Train" : "Retrain";
  const copy = isNeverTrained
    ? `${event.sessionCount} session${event.sessionCount === 1 ? "" : "s"} on ${event.projectDisplayName} and no Train run yet. Extract learnings into CLAUDE.md and MEMORY.md.`
    : `${event.activeDaysSinceLastLearn} active day${event.activeDaysSinceLastLearn === 1 ? "" : "s"} on ${event.projectDisplayName} since the last Train run. Consider rerunning to pick up new patterns.`;
  const canNavigate = typeof onNavigate === "function";
  const handleActivate = () => {
    if (canNavigate) onNavigate?.();
  };
  /* v8 ignore start — keyboard activation requires a DOM; see ExpandableRow. */
  const onKeyDown = (e: ReactKeyboardEvent<HTMLLIElement>) => {
    if (!canNavigate) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      handleActivate();
    }
  };
  /* v8 ignore stop */
  return (
    <li
      className={`activity-feed__item activity-feed__item--train${canNavigate ? " activity-feed__item--clickable" : ""}`}
      role={canNavigate ? "button" : undefined}
      tabIndex={canNavigate ? 0 : undefined}
      onClick={canNavigate ? handleActivate : undefined}
      onKeyDown={onKeyDown}
    >
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--train">
          {badgeLabel}
        </span>
        <TimeChip iso={event.observedAt} />
        <span className="activity-feed__project">{event.projectDisplayName}</span>
      </div>
      <p className="activity-feed__content">{copy}</p>
    </li>
  );
}

function LearningsMilestoneRow({ event }: { event: LearningsMilestoneEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--learnings-milestone">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--learnings-milestone">
          Learnings
        </span>
        <TimeChip iso={event.observedAt} />
      </div>
      <p className="activity-feed__content">
        {event.count} patterns extracted from your work so far.
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

