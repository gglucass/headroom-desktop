import { useEffect, useMemo, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Bell, WifiSlash } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { formatDateTime, formatRelativeTime } from "../lib/dashboardHelpers";
import { estimateCostSavingsUsd, formatEstimatedUsd } from "../lib/modelPricing";
import type {
  ActivityFeedResponse,
  ActivityFeedSnapshot,
  CompressionDiffResponse,
  LearningsMilestoneEvent,
  RecordEvent,
  RecordTag,
  RtkTodayStats,
  SerenaTodayStats,
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
  // RTK and Serena are opt-in, so their tiles (and empty placeholders) only
  // render for users who installed them. Data in the slot still wins: stats
  // from a just-uninstalled addon render rather than vanish mid-session.
  rtkInstalled?: boolean;
  serenaInstalled?: boolean;
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
    badgeLabel: "Optimize",
    copy: "No scan nudge. Visit Optimize to scan any project for learnings.",
    itemModifier: "activity-feed__item--train"
  },
  transformation: {
    badgeClass: "activity-feed__badge--transformation",
    badgeLabel: "Large compression",
    copy: "No large compressions yet.",
    itemModifier: "activity-feed__item--transformation"
  },
  rtkToday: {
    badgeClass: "activity-feed__badge--rtk",
    badgeLabel: "RTK",
    copy: "No RTK commands observed yet today (UTC).",
    itemModifier: "activity-feed__item--rtk"
  },
  serenaToday: {
    badgeClass: "activity-feed__badge--serena",
    badgeLabel: "Serena",
    copy: "No Serena tool calls observed yet today.",
    itemModifier: "activity-feed__item--serena"
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
    copy: "Nothing learned yet today.",
    itemModifier: "activity-feed__item--learnings-milestone"
  },
  weeklyRecap: {
    badgeClass: "activity-feed__badge--weekly-recap",
    badgeLabel: "Weekly recap",
    copy: "No recap yet. It posts at the end of the week.",
    itemModifier: "activity-feed__item--weekly-recap"
  }
};

export function ActivityFeed({
  feed,
  error,
  loaded = true,
  onNavigateToOptimize,
  rtkInstalled = false,
  serenaInstalled = false
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
            <h1>Activity (beta)</h1>
          </div>
          <p className="activity-card__blurb">
            Large compressions, learnings and daily records.
          </p>
          <KeepDiffsSwitch />
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
          {tiles.rtkToday ? (
            <RtkTodayRow event={tiles.rtkToday} />
          ) : rtkInstalled ? (
            <EmptyTile kind="rtkToday" />
          ) : null}
          {tiles.serenaToday ? (
            <SerenaTodayRow event={tiles.serenaToday} />
          ) : serenaInstalled ? (
            <EmptyTile kind="serenaToday" />
          ) : null}
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
  // A click that ends a text selection (say, of the Request ID) is not a
  // toggle, or the row collapses and takes the selection with it.
  const onClick = () => {
    if (window.getSelection()?.toString()) return;
    toggle();
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
      onClick={onClick}
      onKeyDown={onKeyDown}
    >
      {children}
      {expanded && detail ? (
        <div className="activity-feed__detail" onClick={(e) => e.stopPropagation()}>
          {detail}
        </div>
      ) : null}
    </li>
  );
}

/**
 * "Keep before/after": opt-in because the proxy then holds the text of its
 * last 100 requests, readable from its loopback feed by any local account.
 * Flipping it restarts the backend.
 */
function KeepDiffsSwitch() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let active = true;
    void invoke<boolean>("get_compression_diffs_enabled")
      .then((value) => active && setEnabled(value))
      .catch(() => active && setEnabled(false));
    return () => {
      active = false;
    };
  }, []);
  /* v8 ignore start - click handler needs a DOM and a Tauri backend. */
  async function toggle() {
    setBusy(true);
    try {
      setEnabled(await invoke<boolean>("set_compression_diffs_enabled", { enabled: !enabled }));
    } catch (error) {
      console.error("Failed to update Keep before/after", error);
    } finally {
      setBusy(false);
    }
  }
  /* v8 ignore stop */
  return (
    <div className="activity-card__diffs">
      <div className="activity-card__diffs-text">
        <span className="activity-card__diffs-label">Keep before/after</span>
        <span className="activity-card__diffs-meta">
          {enabled
            ? "On. Expand a compression to see what was removed. The text of the last 100 requests stays in memory on this computer."
            : "Off. Turn on to see exactly what a compression removed."}
        </span>
      </div>
      <button
        aria-checked={enabled ?? false}
        aria-label={`${enabled ? "Disable" : "Enable"} keep before/after`}
        className={`connector-switch${enabled ? " is-on" : ""}`}
        disabled={enabled === null || busy}
        onClick={() => void toggle()}
        role="switch"
        type="button"
      >
        <span className="connector-switch__thumb" />
      </button>
    </div>
  );
}

// Flatten a message/block `content` value to text. Anthropic sends a block
// list, OpenAI a string. `tool_result` blocks carry their (compressible)
// payload in `block.content`, a string or nested block list, not in
// `block.text`, so without recursing both diff sides render a bare
// `[tool_result]` and the diff shows no change. Other non-text blocks become a
// `[type]` marker rather than vanish.
function flattenContent(c: unknown): string {
  if (typeof c === "string") return c;
  if (!Array.isArray(c)) return "";
  return c
    .map((block) => {
      if (!block || typeof block !== "object") return "";
      const b = block as { type?: unknown; text?: unknown; content?: unknown };
      if (typeof b.text === "string") return b.text;
      if (b.content !== undefined) {
        const inner = flattenContent(b.content);
        if (inner.length > 0) return inner;
      }
      if (typeof b.type === "string") return `[${b.type}]`;
      return "";
    })
    .filter((s) => s.length > 0)
    .join("\n");
}

export function formatRequestMessages(messages: TransformationRequestMessage[]): string {
  return messages
    .map((m) => {
      const role = (m.role ?? "").trim() || "(unknown)";
      return `${role}:\n${flattenContent(m.content)}`;
    })
    .join("\n\n");
}

export type DiffLine = { type: "same" | "add" | "del"; text: string };

// LCS line diff. ponytail: O(n*m) flat Uint16 table. Large compressions, the
// whole point of this view, routinely run to thousands of lines, so the cap is
// on the cell product (memory), not per-side line count. ~30M cells = 60MB,
// computed once per expand. Over that we fall back to side-by-side dumps.
// Upgrade to Hirschberg/Myers if the cap ever bites.
const MAX_DIFF_CELLS = 30_000_000;

export function diffLines(a: string, b: string): DiffLine[] | null {
  const oldL = a.split("\n");
  const newL = b.split("\n");
  const n = oldL.length;
  const m = newL.length;
  // Uint16 caps LCS values at 65535; the cell cap keeps n,m well under that.
  if ((n + 1) * (m + 1) > MAX_DIFF_CELLS) return null;
  const w = m + 1;
  const dp = new Uint16Array((n + 1) * w);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i * w + j] =
        oldL[i] === newL[j]
          ? dp[(i + 1) * w + (j + 1)] + 1
          : Math.max(dp[(i + 1) * w + j], dp[i * w + (j + 1)]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (oldL[i] === newL[j]) {
      out.push({ type: "same", text: oldL[i] });
      i++;
      j++;
    } else if (dp[(i + 1) * w + j] >= dp[i * w + (j + 1)]) {
      out.push({ type: "del", text: oldL[i] });
      i++;
    } else {
      out.push({ type: "add", text: newL[j] });
      j++;
    }
  }
  while (i < n) out.push({ type: "del", text: oldL[i++] });
  while (j < m) out.push({ type: "add", text: newL[j++] });
  return out;
}

export type CollapsedDiffLine = DiffLine | { type: "skip"; text: string };

// Keep `context` unchanged lines around each change and collapse the rest, so
// the removed (red) / added (green) lines are visible the moment the row opens
// instead of buried under hundreds of identical context lines.
const DIFF_CONTEXT = 3;

export function collapseDiff(diff: DiffLine[], context = DIFF_CONTEXT): CollapsedDiffLine[] {
  const keep = new Array(diff.length).fill(false);
  for (let i = 0; i < diff.length; i++) {
    if (diff[i].type === "same") continue;
    for (let j = Math.max(0, i - context); j <= Math.min(diff.length - 1, i + context); j++) {
      keep[j] = true;
    }
  }
  const out: CollapsedDiffLine[] = [];
  let i = 0;
  while (i < diff.length) {
    if (diff[i].type !== "same" || keep[i]) {
      out.push(diff[i]);
      i++;
      continue;
    }
    let j = i;
    while (j < diff.length && diff[j].type === "same" && !keep[j]) j++;
    const n = j - i;
    out.push({ type: "skip", text: `... ${n} unchanged line${n === 1 ? "" : "s"}` });
    i = j;
  }
  return out;
}

/**
 * dt/dd pair for the detail grid: the request's messages before and after
 * compression as a unified line diff, fetched from the proxy when the row is
 * expanded (the detail only mounts then). Shared by the compression and
 * record rows.
 */
function CompressionDiff({ requestId }: { requestId: string }) {
  const [result, setResult] = useState<CompressionDiffResponse | null>(null);
  const [error, setError] = useState(false);
  useEffect(() => {
    let active = true;
    invoke<CompressionDiffResponse>("get_compression_diff", { requestId })
      .then((value) => active && setResult(value))
      .catch(() => active && setError(true));
    return () => {
      active = false;
    };
  }, [requestId]);
  // Computed once per fetch, not on every feed poll that re-renders the row.
  const view = useMemo(() => {
    if (!result?.requestMessages?.length) return null;
    const original = formatRequestMessages(result.requestMessages);
    // No compressed list: the proxy sent the messages as they were.
    const compressed = result.compressedMessages
      ? formatRequestMessages(result.compressedMessages)
      : original;
    const diff = diffLines(original, compressed);
    return { original, compressed, lines: diff ? collapseDiff(diff) : null };
  }, [result]);

  let body;
  if (error) {
    body = "Could not reach Headroom to load the request.";
  } else if (!result) {
    body = "Loading...";
  } else if (!result.logFullMessages) {
    body = "Turn on Keep before/after above to see what this compression removed.";
  } else if (!view) {
    body = "No longer held. Headroom keeps the text of its last 100 requests only, and not across restarts.";
  } else if (!view.lines) {
    // Too large to diff: show both sides whole.
    return (
      <>
        <dt>Before</dt>
        <dd>
          <pre className="activity-feed__message-dump">{view.original}</pre>
        </dd>
        <dt>After</dt>
        <dd>
          <pre className="activity-feed__message-dump">{view.compressed}</pre>
        </dd>
      </>
    );
  } else {
    body = (
      <pre className="activity-feed__message-dump activity-feed__diff">
        {view.lines.map((line, idx) => (
          <div
            key={idx}
            className={`activity-feed__diff-line activity-feed__diff-line--${line.type}`}
          >
            {line.type === "del"
              ? "- "
              : line.type === "add"
                ? "+ "
                : line.type === "skip"
                  ? ""
                  : "  "}
            {line.text}
          </div>
        ))}
      </pre>
    );
  }
  return (
    <>
      <dt>Before / after</dt>
      <dd>{body}</dd>
    </>
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

// Backends predating the upstream denominator fix (headroom#3106) measure
// tokensSaved across all layers (tool schemas included) but count "in" over
// messages only, so on schema-heavy turns "out" clamps to 0 and the percent
// tops 100 ("4,436 -> 0, 208.4%"). Those two displays are meaningless for
// such records - suppress them; the saved count itself is real and stays.
function isClampedTokenPair(
  saved: number,
  original: number | null | undefined,
  optimized: number | null | undefined
): boolean {
  return optimized === 0 && original != null && saved > original;
}

function TransformationRow({ event }: { event: TransformationFeedEvent }) {
  const saved = event.tokensSaved ?? 0;
  const pct = event.savingsPercent ?? 0;
  const showPct = pct <= 100;
  const workspace = workspaceBasename(event.workspace);
  const hasExactTokens =
    event.inputTokensOriginal != null &&
    event.inputTokensOptimized != null &&
    !isClampedTokenPair(saved, event.inputTokensOriginal, event.inputTokensOptimized);
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
          <CompressionDiff requestId={event.requestId!} />
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
        {event.model ? <span className="activity-feed__model">{event.model}</span> : null}
        {workspace ? (
          <span className="activity-feed__project">{workspace}</span>
        ) : null}
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          Saved {saved.toLocaleString()} tokens
          {showPct ? ` (${pct.toFixed(1)}%)` : ""}
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
  return `${base}: ${suffix}`;
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
    if (raw === "router:noop") continue;
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

  // smart_crush:<n>[:<name1,name2,...>] — the current proxy tag (SmartCrusher
  // replaced the retired ToolCrusher). Same shape/labels as tool_crush above,
  // which is kept for older activity records.
  const smartCrushWithNames = /^smart_crush:(\d+):(.+)$/.exec(raw);
  if (smartCrushWithNames) {
    const n = Number(smartCrushWithNames[1]);
    return {
      label: `Crushed ${n} tool${n === 1 ? "" : "s"}`,
      title: "tool outputs compacted",
      target: smartCrushWithNames[2]
    };
  }
  const smartCrush = /^smart_crush:(\d+)$/.exec(raw);
  if (smartCrush) {
    const n = Number(smartCrush[1]);
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

function RtkTodayRow({ event }: { event: RtkTodayStats }) {
  return (
    <li className="activity-feed__item activity-feed__item--rtk">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--rtk">RTK</span>
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">
          {event.savedTokens.toLocaleString()} tokens saved today (UTC)
        </strong>
        <span className="activity-feed__delta">
          {event.commands.toLocaleString()} command{event.commands === 1 ? "" : "s"}
        </span>
      </div>
    </li>
  );
}

function SerenaTodayRow({ event }: { event: SerenaTodayStats }) {
  // The backend guarantees at least one line; calls lead when present.
  const strong = event.callsLine ?? event.tokensLine;
  const delta = event.callsLine ? event.tokensLine : null;
  return (
    <li className="activity-feed__item activity-feed__item--serena">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--serena">Serena</span>
      </div>
      <div className="activity-feed__row activity-feed__row--savings">
        <strong className="activity-feed__savings">{strong}</strong>
        {delta ? <span className="activity-feed__delta">{delta}</span> : null}
      </div>
    </li>
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
  const hasExactTokens =
    event.inputTokensOriginal != null &&
    event.inputTokensOptimized != null &&
    !isClampedTokenPair(
      event.tokensSaved,
      event.inputTokensOriginal,
      event.inputTokensOptimized
    );
  const hasRequestId = !!event.requestId;
  const estimatedUsd = estimateCostSavingsUsd(event.model, event.tokensSaved);
  const hasExtra = estimatedUsd != null || hasExactTokens || hasRequestId;
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
      {hasRequestId ? (
        <>
          <dt>Request ID</dt>
          <dd className="activity-feed__detail-mono">{event.requestId}</dd>
          <CompressionDiff requestId={event.requestId!} />
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
          {pct != null && pct <= 100 ? ` (${pct.toFixed(1)}%)` : ""}
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
  const badgeLabel = isNeverTrained ? "Try Optimize" : "Rescan";
  const copy = isNeverTrained
    ? `${event.sessionCount} session${event.sessionCount === 1 ? "" : "s"} on ${event.projectDisplayName} and no Scan run yet. Extract learnings into CLAUDE.local.md and MEMORY.md.`
    : `${event.activeDaysSinceLastLearn} active day${event.activeDaysSinceLastLearn === 1 ? "" : "s"} on ${event.projectDisplayName} since the last Scan run. Consider rerunning to pick up new patterns.`;
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
  const { patternsToday, remindersToday, learningsToday, projectDisplayName } = event;
  return (
    <li className="activity-feed__item activity-feed__item--learnings-milestone">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--learnings-milestone">
          Learnings
        </span>
        <TimeChip iso={event.observedAt} />
        {projectDisplayName ? (
          <span className="activity-feed__project">{projectDisplayName}</span>
        ) : null}
      </div>
      <p className="activity-feed__content">
        {patternsToday} pattern{patternsToday === 1 ? "" : "s"} identified today,{" "}
        {remindersToday} reminder{remindersToday === 1 ? "" : "s"} and {learningsToday}{" "}
        learning{learningsToday === 1 ? "" : "s"} written to memory.
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

