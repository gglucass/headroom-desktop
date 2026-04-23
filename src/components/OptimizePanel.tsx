import {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AppliedPatterns, AppliedSection } from "../lib/types";

interface OptimizePanelProps {
  projectPath: string;
  // Bump this to force a refetch after an external event (e.g. a Learn run
  // finishes). The value itself is ignored; only changes matter.
  refreshSignal?: number;
  // When provided, the panel uses this slice of applied patterns rather than
  // invoking `list_applied_patterns` itself. Lets the parent batch one IPC
  // across every panel instead of N.
  preloadedApplied?: AppliedPatterns | null;
  // Called after the panel mutates applied patterns (delete) so the parent
  // can refetch the shared aggregate. Only used when preloadedApplied is set.
  onAppliedMutated?: () => void;
}

type ModalKind = null | "claude" | "memory";

export function OptimizePanel({
  projectPath,
  refreshSignal,
  preloadedApplied,
  onAppliedMutated,
}: OptimizePanelProps) {
  const hasPreloadedApplied = preloadedApplied !== undefined;
  const [applied, setApplied] = useState<AppliedPatterns | null>(
    hasPreloadedApplied ? preloadedApplied ?? null : null,
  );
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [modal, setModal] = useState<ModalKind>(null);

  // Reading `hasPreloadedApplied` via a ref inside `refetch` keeps its identity
  // stable when the parent flips from "still loading the aggregate" to
  // "aggregate resolved". Without this, refetch's useCallback dep churn
  // re-fires the refetch effect and causes a duplicate IPC round per panel.
  const hasPreloadedAppliedRef = useRef(hasPreloadedApplied);
  hasPreloadedAppliedRef.current = hasPreloadedApplied;

  useEffect(() => {
    if (hasPreloadedApplied) {
      setApplied(preloadedApplied ?? null);
    }
  }, [hasPreloadedApplied, preloadedApplied]);

  const refetch = useCallback(() => {
    if (hasPreloadedAppliedRef.current) return () => {};
    let active = true;
    invoke<AppliedPatterns>("list_applied_patterns", { projectPath })
      .then((a) => {
        if (!active) return;
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
  }, [projectPath]);

  useEffect(() => {
    const cancel = refetch();
    return cancel;
  }, [refetch, refreshSignal]);

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
      if (hasPreloadedApplied) {
        onAppliedMutated?.();
      } else {
        refetch();
      }
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

  const claudeCount = applied?.claudeMd.reduce((n, s) => n + s.bullets.length, 0) ?? 0;
  const memoryCount = applied?.memoryMd.reduce((n, s) => n + s.bullets.length, 0) ?? 0;
  const claudeDisabled = applied === null || claudeCount === 0;
  const memoryDisabled = applied === null || memoryCount === 0;

  return (
    <>
      <span className="optimize-panel__pills">
        <button
          type="button"
          className={`optimize-panel__pill${claudeDisabled ? " optimize-panel__pill--empty" : ""}`}
          onClick={() => setModal("claude")}
          disabled={claudeDisabled}
        >
          {claudeCount} learning{claudeCount === 1 ? "" : "s"} in CLAUDE.md
        </button>
        <button
          type="button"
          className={`optimize-panel__pill${memoryDisabled ? " optimize-panel__pill--empty" : ""}`}
          onClick={() => setModal("memory")}
          disabled={memoryDisabled}
        >
          {memoryCount} reminder{memoryCount === 1 ? "" : "s"} in MEMORY.md
        </button>
      </span>

      {modal === "claude" ? (
        <Modal title="Learnings in CLAUDE.md" onClose={() => setModal(null)}>
          {loadError ? <p className="install-progress__error">{loadError}</p> : null}
          {applied === null ? (
            <p className="optimize-panel__empty">Loading…</p>
          ) : claudeCount === 0 ? (
            <p className="optimize-panel__empty">
              No learnings in CLAUDE.md yet — run Learn or let live traffic
              accumulate.
            </p>
          ) : (
            <AppliedSections
              fileKind="claude"
              sections={applied.claudeMd}
              busyIds={busyIds}
              onDelete={handleDeleteApplied}
            />
          )}
        </Modal>
      ) : null}

      {modal === "memory" ? (
        <Modal title="Reminders in MEMORY.md" onClose={() => setModal(null)}>
          {loadError ? <p className="install-progress__error">{loadError}</p> : null}
          {applied === null ? (
            <p className="optimize-panel__empty">Loading…</p>
          ) : memoryCount === 0 ? (
            <p className="optimize-panel__empty">
              No reminders in MEMORY.md yet — run Learn or let live traffic
              accumulate.
            </p>
          ) : (
            <AppliedSections
              fileKind="memory"
              sections={applied.memoryMd}
              busyIds={busyIds}
              onDelete={handleDeleteApplied}
            />
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

function AppliedSections({
  fileKind,
  sections,
  busyIds,
  onDelete,
}: {
  fileKind: "claude" | "memory";
  sections: AppliedSection[];
  busyIds: Set<string>;
  onDelete: (
    fileKind: "claude" | "memory",
    sectionTitle: string,
    bulletText: string,
  ) => Promise<void> | void;
}) {
  return (
    <>
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
    </>
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
