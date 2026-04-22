import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  AppliedPatterns,
  AppliedSection,
  LiveLearning,
} from "../lib/types";

interface OptimizePanelProps {
  projectPath: string;
  // Bump this to force a refetch after an external event (e.g. a Learn run
  // finishes). The value itself is ignored; only changes matter.
  refreshSignal?: number;
  // When provided, the panel uses this slice of live learnings rather than
  // invoking `list_live_learnings` itself. Lets the parent batch one Python
  // subprocess spawn across every panel instead of N.
  preloadedLive?: LiveLearning[] | null;
  // Called after the panel mutates live learnings (e.g. delete) so the parent
  // can refetch the shared aggregate. Only used when preloadedLive is set.
  onLiveMutated?: () => void;
}

const CATEGORY_LABELS: Record<string, string> = {
  environment: "Environment",
  architecture: "Architecture",
  preference: "Preference",
  error_recovery: "Error recovery",
};

type ModalKind = null | "pending" | "applied";

export function OptimizePanel({
  projectPath,
  refreshSignal,
  preloadedLive,
  onLiveMutated,
}: OptimizePanelProps) {
  const hasPreloadedLive = preloadedLive !== undefined;
  const [live, setLive] = useState<LiveLearning[] | null>(
    hasPreloadedLive ? preloadedLive ?? null : null,
  );
  const [applied, setApplied] = useState<AppliedPatterns | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [modal, setModal] = useState<ModalKind>(null);

  // Sync preloaded slice into local state whenever the parent updates it.
  useEffect(() => {
    if (hasPreloadedLive) {
      setLive(preloadedLive ?? null);
    }
  }, [hasPreloadedLive, preloadedLive]);

  const refetch = useCallback(() => {
    let active = true;
    const appliedPromise = invoke<AppliedPatterns>("list_applied_patterns", { projectPath });
    const livePromise = hasPreloadedLive
      ? Promise.resolve<LiveLearning[] | null>(null)
      : invoke<LiveLearning[]>("list_live_learnings", { projectPath });
    Promise.all([livePromise, appliedPromise])
      .then(([l, a]) => {
        if (!active) return;
        if (!hasPreloadedLive) {
          setLive(l);
        }
        setApplied(a);
        setLoadError(null);
      })
      .catch((err) => {
        if (!active) return;
        setLoadError(err instanceof Error ? err.message : "Failed to load optimize data.");
      });
    return () => {
      active = false;
    };
  }, [projectPath, hasPreloadedLive]);

  useEffect(() => {
    const cancel = refetch();
    return cancel;
  }, [refetch, refreshSignal]);

  const handleDeleteLive = async (id: string) => {
    setBusyIds((prev) => new Set(prev).add(id));
    try {
      await invoke("delete_live_learning", { memoryId: id });
      if (hasPreloadedLive) {
        onLiveMutated?.();
      } else {
        refetch();
      }
    } catch (err) {
      setLoadError(err instanceof Error ? err.message : "Delete failed.");
    } finally {
      setBusyIds((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const handleDeleteApplied = async (
    fileKind: "claude" | "memory",
    sectionTitle: string,
    bulletText: string,
  ) => {
    const key = `${fileKind}|${sectionTitle}|${bulletText}`;
    setBusyIds((prev) => new Set(prev).add(key));
    try {
      await invoke("delete_applied_pattern", {
        projectPath,
        fileKind,
        sectionTitle,
        bulletText,
      });
      refetch();
    } catch (err) {
      setLoadError(err instanceof Error ? err.message : "Delete failed.");
    } finally {
      setBusyIds((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  };

  const liveCount = live?.length ?? 0;
  const appliedCount =
    (applied?.claudeMd.reduce((n, s) => n + s.bullets.length, 0) ?? 0) +
    (applied?.memoryMd.reduce((n, s) => n + s.bullets.length, 0) ?? 0);

  const liveByCategory = groupBy(live ?? [], (l) => l.category);
  const pendingDisabled = live === null || liveCount === 0;
  const appliedDisabled = applied === null || appliedCount === 0;

  return (
    <>
      <span className="optimize-panel__pills">
        <button
          type="button"
          className={`optimize-panel__pill${pendingDisabled ? " optimize-panel__pill--empty" : ""}`}
          onClick={() => setModal("pending")}
          disabled={pendingDisabled}
        >
          {liveCount} pending learning{liveCount === 1 ? "" : "s"}
        </button>
        <button
          type="button"
          className={`optimize-panel__pill${appliedDisabled ? " optimize-panel__pill--empty" : ""}`}
          onClick={() => setModal("applied")}
          disabled={appliedDisabled}
        >
          {appliedCount} learning{appliedCount === 1 ? "" : "s"} applied
        </button>
      </span>

      {modal === "pending" ? (
        <Modal title="Pending learnings" onClose={() => setModal(null)}>
          <p className="optimize-panel__info">
            Patterns observed in recent sessions. Each must be observed at least
            twice before being written to your project's memory.
          </p>
          {loadError ? <p className="install-progress__error">{loadError}</p> : null}
          {live === null ? (
            <p className="optimize-panel__empty">Loading…</p>
          ) : live.length === 0 ? (
            <p className="optimize-panel__empty">
              No pending learnings yet for this project — use Claude Code and
              they'll appear here.
            </p>
          ) : (
            Array.from(liveByCategory.entries()).map(([category, items]) => (
              <div className="optimize-panel__subsection" key={category}>
                <h5 className="optimize-panel__subsection-title">
                  {CATEGORY_LABELS[category] ?? category}
                </h5>
                <ul className="optimize-panel__list">
                  {items.map((row) => (
                    <LiveRow
                      key={row.id}
                      row={row}
                      busy={busyIds.has(row.id)}
                      onDelete={() => void handleDeleteLive(row.id)}
                    />
                  ))}
                </ul>
              </div>
            ))
          )}
        </Modal>
      ) : null}

      {modal === "applied" ? (
        <Modal title="Applied learnings" onClose={() => setModal(null)}>
          <p className="optimize-panel__info">
            Patterns still present in pending learnings may be re-applied on the
            next flush (~10s).
          </p>
          {loadError ? <p className="install-progress__error">{loadError}</p> : null}
          {applied === null ? (
            <p className="optimize-panel__empty">Loading…</p>
          ) : appliedCount === 0 ? (
            <p className="optimize-panel__empty">
              No applied learnings yet — run Learn or let live traffic
              accumulate.
            </p>
          ) : (
            <>
              <AppliedFileView
                label="CLAUDE.md"
                fileKind="claude"
                sections={applied.claudeMd}
                busyIds={busyIds}
                onDelete={handleDeleteApplied}
              />
              <AppliedFileView
                label="MEMORY.md"
                fileKind="memory"
                sections={applied.memoryMd}
                busyIds={busyIds}
                onDelete={handleDeleteApplied}
              />
            </>
          )}
        </Modal>
      ) : null}
    </>
  );
}

function Modal({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      onClick={onClose}
    >
      <div
        className="modal-card optimize-panel__modal"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="optimize-panel__modal-header">
          <h3>{title}</h3>
          <button
            type="button"
            className="optimize-panel__modal-close"
            onClick={onClose}
            aria-label="Close"
          >
            ×
          </button>
        </div>
        <div className="optimize-panel__modal-body">{children}</div>
      </div>
    </div>
  );
}

function LiveRow({
  row,
  busy,
  onDelete,
}: {
  row: LiveLearning;
  busy: boolean;
  onDelete: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const canExpand = row.content.length > 140 || row.content.includes("\n");
  return (
    <li className="optimize-panel__row">
      <div className="optimize-panel__row-main">
        <p
          className={
            expanded
              ? "activity-feed__content"
              : "activity-feed__content activity-feed__content--clamped"
          }
          title={canExpand && !expanded ? row.content : undefined}
        >
          {row.content}
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
        <div className="optimize-panel__row-meta">
          <span>importance {row.importance.toFixed(2)}</span>
          <span>evidence ×{row.evidenceCount}</span>
        </div>
      </div>
      <button
        type="button"
        className="optimize-panel__delete"
        disabled={busy}
        onClick={onDelete}
      >
        {busy ? "…" : "Delete"}
      </button>
    </li>
  );
}

function AppliedFileView({
  label,
  fileKind,
  sections,
  busyIds,
  onDelete,
}: {
  label: string;
  fileKind: "claude" | "memory";
  sections: AppliedSection[];
  busyIds: Set<string>;
  onDelete: (
    fileKind: "claude" | "memory",
    sectionTitle: string,
    bulletText: string,
  ) => Promise<void> | void;
}) {
  if (sections.length === 0) return null;
  return (
    <div className="optimize-panel__subsection">
      <h5 className="optimize-panel__subsection-title">{label}</h5>
      {sections.map((section) => (
        <div className="optimize-panel__applied-section" key={section.title}>
          <h6 className="optimize-panel__applied-section-title">{section.title}</h6>
          <ul className="optimize-panel__list">
            {section.bullets.map((bullet) => {
              const key = `${fileKind}|${section.title}|${bullet}`;
              return (
                <AppliedBullet
                  key={key}
                  bullet={bullet}
                  busy={busyIds.has(key)}
                  onDelete={() => void onDelete(fileKind, section.title, bullet)}
                />
              );
            })}
          </ul>
        </div>
      ))}
    </div>
  );
}

function AppliedBullet({
  bullet,
  busy,
  onDelete,
}: {
  bullet: string;
  busy: boolean;
  onDelete: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const canExpand = bullet.length > 140 || bullet.includes("\n");
  return (
    <li className="optimize-panel__row">
      <div className="optimize-panel__row-main">
        <p
          className={
            expanded
              ? "activity-feed__content"
              : "activity-feed__content activity-feed__content--clamped"
          }
          title={canExpand && !expanded ? bullet : undefined}
        >
          {bullet}
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
      </div>
      <button
        type="button"
        className="optimize-panel__delete"
        disabled={busy}
        onClick={onDelete}
      >
        {busy ? "…" : "Delete"}
      </button>
    </li>
  );
}

function groupBy<T, K>(items: T[], key: (item: T) => K): Map<K, T[]> {
  const map = new Map<K, T[]>();
  for (const item of items) {
    const k = key(item);
    const list = map.get(k);
    if (list) {
      list.push(item);
    } else {
      map.set(k, [item]);
    }
  }
  return map;
}
