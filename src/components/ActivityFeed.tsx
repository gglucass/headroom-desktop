import { formatDateTime } from "../lib/dashboardHelpers";
import type {
  ActivityEvent,
  ActivityFeedResponse,
  MemoryFeedEvent,
  TransformationFeedEvent
} from "../lib/types";

interface ActivityFeedProps {
  feed: ActivityFeedResponse;
  error: string | null;
}

export function ActivityFeed({ feed, error }: ActivityFeedProps) {
  return (
    <>
      <header className="activity-feed__header">
        <h2>Live activity</h2>
        <p className="activity-feed__subtitle">
          Compressions Headroom applied to recent requests, plus learnings extracted from live
          traffic.
        </p>
      </header>
      {error ? (
        <p className="loading-copy">{error}</p>
      ) : !feed.proxyReachable ? (
        <p className="loading-copy">Waiting for the Headroom proxy…</p>
      ) : feed.events.length === 0 ? (
        <p className="loading-copy">
          No requests yet. Send a message through Claude Code to see compressions and learnings
          stream in.
        </p>
      ) : (
        <ul className="activity-feed__list">
          {feed.events.map((event, index) => (
            <ActivityRow key={activityKey(event, index)} event={event} />
          ))}
        </ul>
      )}
    </>
  );
}

function activityKey(event: ActivityEvent, index: number): string {
  if (event.kind === "transformation") {
    return `t-${event.data.requestId ?? event.data.timestamp ?? index}`;
  }
  return `m-${event.data.id}`;
}

function ActivityRow({ event }: { event: ActivityEvent }) {
  if (event.kind === "transformation") {
    return <TransformationRow event={event.data} />;
  }
  return <MemoryRow event={event.data} />;
}

function TransformationRow({ event }: { event: TransformationFeedEvent }) {
  const saved = event.tokensSaved ?? 0;
  const pct = event.savingsPercent ?? 0;
  return (
    <li className="activity-feed__item activity-feed__item--transformation">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--transformation">
          Compression
        </span>
        <span className="activity-feed__time">{formatDateTime(event.timestamp)}</span>
        <span className="activity-feed__provider">{event.provider ?? "unknown"}</span>
        {event.model ? <span className="activity-feed__model">{event.model}</span> : null}
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
          {event.transformsApplied.map((t) => (
            <li key={t} className="activity-feed__transform">
              {t}
            </li>
          ))}
        </ul>
      ) : null}
    </li>
  );
}

function MemoryRow({ event }: { event: MemoryFeedEvent }) {
  return (
    <li className="activity-feed__item activity-feed__item--memory">
      <div className="activity-feed__row activity-feed__row--meta">
        <span className="activity-feed__badge activity-feed__badge--memory">Learning</span>
        <span className="activity-feed__time">{formatDateTime(event.createdAt)}</span>
        <span className="activity-feed__scope">{event.scope}</span>
        <span className="activity-feed__importance">
          importance {event.importance.toFixed(2)}
        </span>
      </div>
      <p className="activity-feed__content">{event.content}</p>
    </li>
  );
}
