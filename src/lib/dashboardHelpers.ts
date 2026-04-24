import type {
  ClientConnectorStatus,
  DailySavingsPoint,
  HourlySavingsPoint
} from "./types";

export interface SavingsChartDatum {
  bucketKey: string;
  bucketLabel: string;
  estimatedSavingsUsd: number;
  estimatedTokensSaved: number;
  actualCostUsd: number;
  totalTokensSent: number;
  totalCostBeforeOptimization: number;
  totalTokensBeforeOptimization: number;
}

const GENERIC_PROXY_TELEMETRY_PATTERNS = [
  /\btoin:/i,
  /\bpatterns,\s*\d+\s*compressions/i,
  /\bretrieval rate\b/i
];

const CONNECTOR_LOG_MARKERS: Record<string, RegExp[]> = {
  claude_code: [/\bclaude\b/i, /\banthropic\b/i, /\/v1\/messages/i]
};

export function currencyExact(value: number) {
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2
  }).format(value);
}

export function currency(value: number) {
  if (value >= 10_000) {
    return new Intl.NumberFormat("en-US", {
      style: "currency",
      currency: "USD",
      notation: "compact",
      maximumFractionDigits: 1
    }).format(value);
  }
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: 0
  }).format(value);
}

export function compactNumber(value: number) {
  return new Intl.NumberFormat("en-US", {
    notation: "compact",
    maximumFractionDigits: 1
  }).format(value);
}

export function percent1(value: number) {
  return new Intl.NumberFormat("en-US", {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1
  }).format(value);
}

export function formatDayLabel(dayKey: string) {
  const parsed = new Date(`${dayKey}T00:00:00`);
  if (Number.isNaN(parsed.getTime())) {
    return dayKey;
  }
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric"
  }).format(parsed);
}

export function formatHourLabel(hourKey: string) {
  const parsed = new Date(`${hourKey}:00`);
  if (Number.isNaN(parsed.getTime())) {
    return hourKey;
  }
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit"
  }).format(parsed);
}

export function formatDayKey(date: Date) {
  const year = date.getFullYear();
  const month = `${date.getMonth() + 1}`.padStart(2, "0");
  const day = `${date.getDate()}`.padStart(2, "0");
  return `${year}-${month}-${day}`;
}

export function startOfDay(date: Date) {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate());
}

export function startOfMonth(date: Date) {
  return new Date(date.getFullYear(), date.getMonth(), 1);
}

export function endOfMonth(date: Date) {
  return new Date(date.getFullYear(), date.getMonth() + 1, 0);
}

export function addMonths(date: Date, delta: number) {
  return new Date(date.getFullYear(), date.getMonth() + delta, 1);
}

export function addDays(date: Date, delta: number) {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate() + delta);
}

export function parseDayKey(dayKey: string) {
  const parsed = new Date(`${dayKey}T00:00:00`);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

export function parseHourKey(hourKey: string) {
  const parsed = new Date(`${hourKey}:00`);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

export function formatMonthLabel(date: Date) {
  return new Intl.DateTimeFormat(undefined, {
    month: "long",
    year: "numeric"
  }).format(date);
}

export function formatSelectedDayLabel(date: Date) {
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric"
  }).format(date);
}

export function buildMonthlySavingsWindow(data: DailySavingsPoint[], month: Date) {
  const monthStart = startOfMonth(month);
  const monthEnd = endOfMonth(month);
  const totalDays = monthEnd.getDate();
  const dataByDate = new Map(data.map((point) => [point.date, point]));

  return Array.from({ length: totalDays }, (_, index) => {
    const day = new Date(monthStart);
    day.setDate(index + 1);
    const date = formatDayKey(day);
    return dataByDate.get(date) ?? {
      date,
      estimatedSavingsUsd: 0,
      estimatedTokensSaved: 0,
      actualCostUsd: 0,
      totalTokensSent: 0
    };
  });
}

export function buildHourlySavingsWindow(data: HourlySavingsPoint[], day: Date) {
  const dayKey = formatDayKey(day);
  const dataByHour = new Map(data.map((point) => [point.hour, point]));

  return Array.from({ length: 24 }, (_, hour) => {
    const hourKey = `${dayKey}T${String(hour).padStart(2, "0")}:00`;
    return dataByHour.get(hourKey) ?? {
      hour: hourKey,
      estimatedSavingsUsd: 0,
      estimatedTokensSaved: 0,
      actualCostUsd: 0,
      totalTokensSent: 0
    };
  });
}

export function buildMonthlySavingsChartData(data: DailySavingsPoint[]): SavingsChartDatum[] {
  return data.map((point) => ({
    bucketKey: point.date,
    bucketLabel: formatDayLabel(point.date),
    estimatedSavingsUsd: point.estimatedSavingsUsd,
    estimatedTokensSaved: point.estimatedTokensSaved,
    actualCostUsd: point.actualCostUsd,
    totalTokensSent: point.totalTokensSent,
    totalCostBeforeOptimization: point.actualCostUsd + point.estimatedSavingsUsd,
    totalTokensBeforeOptimization: point.totalTokensSent + point.estimatedTokensSaved
  }));
}

export function buildHourlySavingsChartData(data: HourlySavingsPoint[]): SavingsChartDatum[] {
  return data.map((point) => ({
    bucketKey: point.hour,
    bucketLabel: formatHourLabel(point.hour),
    estimatedSavingsUsd: point.estimatedSavingsUsd,
    estimatedTokensSaved: point.estimatedTokensSaved,
    actualCostUsd: point.actualCostUsd,
    totalTokensSent: point.totalTokensSent,
    totalCostBeforeOptimization: point.actualCostUsd + point.estimatedSavingsUsd,
    totalTokensBeforeOptimization: point.totalTokensSent + point.estimatedTokensSaved
  }));
}

export function dayOfMonthTickFormatter(value: string) {
  const parsed = parseDayKey(value);
  if (!parsed) {
    return value;
  }
  const dayOfMonth = parsed.getDate();
  const lastDay = endOfMonth(parsed).getDate();
  return dayOfMonth === 1 || dayOfMonth === lastDay || dayOfMonth % 2 === 1
    ? String(dayOfMonth)
    : "";
}

export function hourOfDayTickFormatter(value: string) {
  const parsed = parseHourKey(value);
  if (!parsed) {
    return value;
  }
  const hour = parsed.getHours();
  return hour === 23 || hour % 4 === 0 ? String(hour).padStart(2, "0") : "";
}

export function earliestSavingsMonth(data: DailySavingsPoint[]) {
  let earliest: Date | null = null;

  for (const point of data) {
    const parsed = parseDayKey(point.date);
    if (!parsed) {
      continue;
    }
    const monthStart = startOfMonth(parsed);
    if (!earliest || monthStart < earliest) {
      earliest = monthStart;
    }
  }

  return earliest;
}

export function earliestHourlyDay(data: HourlySavingsPoint[]) {
  let earliest: Date | null = null;

  for (const point of data) {
    const parsed = parseHourKey(point.hour);
    if (!parsed) {
      continue;
    }
    const dayStart = startOfDay(parsed);
    if (!earliest || dayStart < earliest) {
      earliest = dayStart;
    }
  }

  return earliest;
}

export function formatDateTime(timestamp?: string | null) {
  if (!timestamp) {
    return "Never";
  }
  const parsed = new Date(timestamp);
  if (Number.isNaN(parsed.getTime())) {
    return "Unknown";
  }
  return new Intl.DateTimeFormat(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit"
  }).format(parsed);
}

/**
 * Relative time for high-frequency events in the activity feed. Recent events
 * read as "just now" / "10m ago" / "6h ago" / "3 days ago"; anything older
 * than a week falls back to an absolute date. `now` is injectable so callers
 * with a mocked clock (tests) can get deterministic output.
 */
export function formatRelativeTime(
  timestamp?: string | null,
  now: Date = new Date()
): string {
  if (!timestamp) return "Never";
  const ms = new Date(timestamp).getTime();
  if (Number.isNaN(ms)) return "Unknown";
  const diff = now.getTime() - ms;
  if (diff < 45_000) return "just now";
  if (diff < 60 * 60_000) return `${Math.max(1, Math.floor(diff / 60_000))}m ago`;
  if (diff < 24 * 60 * 60_000) return `${Math.floor(diff / (60 * 60_000))}h ago`;
  if (diff < 7 * 24 * 60 * 60_000) {
    const days = Math.floor(diff / (24 * 60 * 60_000));
    return `${days} day${days === 1 ? "" : "s"} ago`;
  }
  // Older than a week: absolute date.
  const d = new Date(ms);
  const sameYear = d.getFullYear() === now.getFullYear();
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    year: sameYear ? undefined : "numeric"
  }).format(d);
}

export function formatLearnStatus(project: {
  lastLearnRanAt: string | null;
}): string {
  if (!project.lastLearnRanAt) {
    return "never trained";
  }
  const parsed = new Date(project.lastLearnRanAt);
  if (Number.isNaN(parsed.getTime())) {
    return "never trained";
  }
  const diffMs = Date.now() - parsed.getTime();
  const diffDays = Math.floor(diffMs / 86_400_000);
  if (diffDays === 0) return "last training: today";
  if (diffDays === 1) return "last training: yesterday";
  return `last training: ${diffDays} days ago`;
}

export function aggregateClientConnectors(connectors: ClientConnectorStatus[]) {
  return connectors.filter((connector) => connector.clientId === "claude_code");
}

export function sortClientConnectors(connectors: ClientConnectorStatus[]) {
  return [...connectors].sort((left, right) => {
    if (left.installed !== right.installed) {
      return left.installed ? -1 : 1;
    }
    return left.name.localeCompare(right.name);
  });
}

export function isGenericProxyTelemetry(line: string) {
  return GENERIC_PROXY_TELEMETRY_PATTERNS.some((pattern) => pattern.test(line));
}

export function findClientVerificationLogLine(clientId: string, lines: string[]) {
  const markers = CONNECTOR_LOG_MARKERS[clientId] ?? [];
  if (markers.length === 0) {
    return null;
  }

  return [...lines]
    .reverse()
    .find(
      (line) =>
        line.trim().length > 0 &&
        !isGenericProxyTelemetry(line) &&
        markers.some((marker) => marker.test(line))
    ) ?? null;
}
