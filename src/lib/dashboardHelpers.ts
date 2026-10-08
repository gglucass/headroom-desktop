import type {
  ClientConnectorStatus,
  ClientSetupResult,
  DailySavingsPoint,
  HeadroomPricingStatus,
  HourlySavingsPoint,
  ProviderSavingsPoint,
  RuntimeStatus
} from "./types";

export interface SavingsChartDatum {
  bucketKey: string;
  bucketLabel: string;
  estimatedSavingsUsd: number;
  estimatedTokensSaved: number;
  // Full bucket spend, cache reads included. Kept for the per-provider
  // pro-rating in the tooltip; the bars plot the compressible figures below.
  actualCostUsd: number;
  totalTokensSent: number;
  // What the bars plot: spend with the provider cache-read portion removed.
  // See `compressibleSpend`.
  compressibleCostUsd: number;
  compressibleTokensSent: number;
  // Output-shaping savings, stacked on top of compression in the chart. Zero
  // for buckets predating the layer, so the bar simply shows one segment.
  outputSavingsUsd: number;
  // The backend's per-bucket estimate, shown from the first shaped request
  // and while an addon shapes replies too. It is rougher than the Output
  // chip's sampled figure (it scores traffic the local recompute cannot
  // against the user's overall mean reply length), so the chip says so when
  // it is the only output figure in a window.
  outputTokensSaved: number;
  // Tool-schema deferral, the third Headroom layer, priced upstream at the
  // cache-read rate. Zero for buckets before per-bucket sampling began
  // (2026-09-02) -- the backend only exposed a lifetime total, so those bars
  // understate this layer rather than misreport it.
  toolSchemaSavingsUsd: number;
  toolSchemaTokensSaved: number;
  totalCostBeforeOptimization: number;
  totalTokensBeforeOptimization: number;
  // Per-provider attribution, only populated for hourly buckets (day view).
  // Undefined for monthly buckets, which have no provider dimension.
  byProvider?: ProviderSavingsPoint[];
}

export function currencyExact(value: number) {
  // Avoid "-$0.00" from tiny negatives that round to zero at 2 decimals.
  if (value > -0.005 && value <= 0) value = 0;
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2
  }).format(value);
}

export function currency(value: number) {
  // Avoid "-$0" from tiny negatives that round to zero at 0 decimals.
  if (value > -0.5 && value <= 0) value = 0;
  // Sub-dollar savings would render as a flat "$0"; show cents instead.
  if (value > 0 && value < 1) return currencyExact(value);
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

/** Share of the would-be total that was saved, as a whole percent. Null when there is nothing to compare. */
export function savingsRate(saved: number, spent: number) {
  const baseline = Math.max(0, saved) + Math.max(0, spent);
  if (baseline <= 0) return null;
  return Math.round((Math.max(0, saved) / baseline) * 100);
}

/** What a bucket's cache reads cost. The backend rollup's own figure when it
 * has one (priced per request by model, the same way `actualCostUsd` was);
 * otherwise recovered from the read discount as `discount / 9`, which assumes
 * reads bill at 0.1x list. That holds for most models but overstates the read
 * cost wherever the discount is steeper (claude-fable-5-1 reads bill at
 * 0.025x, so /9 reads them 4.3x too high and the compressible spend too low),
 * so it is only the fallback for buckets the rollup never priced. */
export function readCostUsd(point: {
  cacheSavingsUsd?: number | null;
  cacheReadCostUsd?: number | null;
}) {
  if (point.cacheReadCostUsd != null) return Math.max(0, point.cacheReadCostUsd);
  return Math.max(0, point.cacheSavingsUsd ?? 0) / 9;
}

/** Billable-dollar input-compression rate: the share of the COMPRESSIBLE
 * input spend Headroom removed. Cache reads are excluded from the denominator
 * (they bill at ~0.1x and Headroom deliberately never touches the cached
 * prefix); output shaping is never part of this rate.
 *
 * Superseded by `newInputSavingsRate` as the headline basis, but kept as the
 * FALLBACK for windows without sampled new-input coverage (pre-sampling
 * buckets have archived cache coverage going back months, so this rate can
 * always be computed for them). Priced in dollars because only the dollar
 * figures are on one scale: `totalTokensSent` is our own tokenizer's count
 * while `cacheReadTokens` is the provider's ("must never be differenced",
 * proxy/outcome.py; on real data reads exceed forwarded input). The read cost (`readCostUsd`) is
 * subtracted from the bucket's actual input cost -- both from one pricing
 * function, so the subtraction is sound.
 *
 * Only buckets with cache coverage count, so numerator and denominator always
 * describe the same slice; null when the window has no coverage. */
export function compressibleInputSavingsRate(
  points: Array<{
    cacheSavingsUsd?: number | null;
    cacheReadCostUsd?: number | null;
    actualCostUsd: number;
    estimatedSavingsUsd: number;
  }>
) {
  let saved = 0;
  // What survived compression and was still paid for at full input price.
  let remaining = 0;
  for (const point of points) {
    if (point.cacheSavingsUsd == null) continue;
    saved += Math.max(0, point.estimatedSavingsUsd);
    remaining += Math.max(0, point.actualCostUsd - readCostUsd(point));
  }
  const baseline = saved + remaining;
  if (baseline <= 0) return null;
  return { pct: Math.min(100, (saved / baseline) * 100), saved, remaining };
}

/**
 * INVARIANT (set with Garm, 2026-09-03; do NOT change the basis without asking
 * him first). Every input-savings number the user sees -- this rate, the
 * headline input % chip, AND the history chart's saved/spent bars -- is
 * measured against JUST the new input Headroom can compress, on BOTH sides:
 *   numerator   = tokens Headroom removed from that new input
 *   denominator = the new input that reached the model (uncached + cache-write)
 * The re-sent cached prefix and provider cache reads are excluded from both;
 * layers that ride the cached prefix (tool-schema deferral) are excluded too.
 * History: the session rate (state.rs `session_savings_pct`) has used this
 * basis since at least 0.9.2; the history chart chip used the billable-dollar
 * `compressibleInputSavingsRate` (diluted by cache-read-bearing spend) until
 * 2026-09-03, when it was aligned onto this basis. NOTE (verified, do not
 * misremember): the ~25%->~5-10% fleet drop across 0.9.3->0.9.5 was NOT a
 * denominator change -- the denominator logic is byte-identical at 0.9.2 and
 * 0.9.4 -- it was compression (the numerator) collapsing on the 0.35->0.37
 * wheel swap, shown through an unchanged formula. A denominator change is a
 * product decision, not a cleanup: ask Garm before making one.
 *
 * Canonical input-compression rate over a window of buckets, on the
 * NEW-INPUT basis: saved tokens vs the input that newly entered context
 * (provider-billed uncached + cache-write tokens, sampled locally from the
 * proxy's cumulative counters -- see `newInputTokens`). The re-sent cached
 * prefix never enters the denominator: Headroom deliberately never rewrites
 * it, so it carries no compression opportunity, and counting it drove this
 * rate toward zero as sessions grew (2026-09-02 fleet analysis: displayed ~5%
 * while compression of actually-touchable input ran ~30%). Both counts come
 * from the proxy's own tokenizer, so unlike `cacheReadTokens` they may be
 * summed and ratioed (that provider-scale ban is documented on `compressibleInputSavingsRate`).
 *
 * Only buckets with sampled coverage count -- numerator included, so both
 * sides always describe the same slice; null when the window has none.
 * Buckets from backend rollups or older builds carry 0/absent coverage and
 * are skipped: their sent count is full-forwarded and must not mix in. */
export function newInputSavingsRate(
  points: Array<{ newInputTokens?: number; estimatedTokensSaved: number }>
) {
  let saved = 0;
  let newInput = 0;
  for (const point of points) {
    if (!point.newInputTokens) continue;
    saved += Math.max(0, point.estimatedTokensSaved);
    newInput += point.newInputTokens;
  }
  const baseline = saved + newInput;
  if (baseline <= 0) return null;
  return {
    pct: Math.min(100, (saved / baseline) * 100),
    savedTokens: saved,
    newInputTokens: newInput
  };
}

/** Output-shaper reduction over a window of buckets, from the locally-sampled
 * saved/baseline deltas. Only buckets with samples count; null when the window
 * has no coverage or the sampled baseline is zero. A bucket's saved delta is
 * signed (a stretch of replies longer than baseline), so it nets against the
 * rest of the window and only the window total is floored at zero. */
export function outputReductionForWindow(
  points: Array<{
    outputSampledTokensSaved?: number | null;
    outputBaselineTokens?: number | null;
  }>
) {
  let saved = 0;
  let baseline = 0;
  for (const point of points) {
    if (point.outputSampledTokensSaved == null || point.outputBaselineTokens == null) continue;
    saved += point.outputSampledTokensSaved;
    baseline += Math.max(0, point.outputBaselineTokens);
  }
  if (baseline <= 0) return null;
  saved = Math.max(0, saved);
  return { pct: Math.min(100, (saved / baseline) * 100), savedTokens: saved, baselineTokens: baseline };
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

// Caption under the History overlay total. The total covers whichever period
// the chart shows, so only the open period may be called today / this month.
export function historyOverlayCaption(
  view: "day" | "month",
  visible: Date,
  now: Date = new Date()
): string {
  if (view === "day") {
    return visible >= startOfDay(now)
      ? "saved today"
      : `saved on ${formatSelectedDayLabel(visible)}`;
  }
  return visible >= startOfMonth(now)
    ? "saved this month"
    : `saved in ${formatMonthLabel(visible)}`;
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
      totalTokensSent: 0,
      outputSavingsUsd: 0,
      outputTokensSaved: 0
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
      totalTokensSent: 0,
      outputSavingsUsd: 0,
      outputTokensSaved: 0,
      byProvider: []
    };
  });
}

/** Bucket spend with the provider cache-read portion stripped out: the slice
 * of the bill Headroom can actually act on. Cache reads bill at ~0.1x and the
 * cached prefix is deliberately left intact, so counting them makes every bar
 * dwarf its own savings segment and contradicts the compression-rate chip
 * above it (which uses the same denominator, see `compressibleInputSavingsRate`).
 * Read cost comes from `readCostUsd`.
 *
 * The TOKEN figure is priced the same way rather than differenced, for the
 * reason spelled out on `compressibleInputSavingsRate`: `cacheReadTokens` is
 * the provider's count and `totalTokensSent` is our tokenizer's count of the
 * forwarded prompt, and on real data the reads routinely exceed the forwarded
 * input -- `totalTokensSent - cacheReadTokens` clamped 27 of 36 live hourly
 * buckets to a flat zero bar while the same buckets' dollar bars were fine.
 * So we split our own token count by the compressible share the dollar pair
 * implies (full-price input is `actualCostUsd + cacheSavingsUsd`: what was
 * paid plus the discount the reads earned).
 *
 * Falls back to the full figure on buckets with no cache coverage - local
 * tracker buckets, and days aged out of the backend's checkpoint history. */
export function compressibleSpend(point: {
  cacheSavingsUsd?: number | null;
  cacheReadCostUsd?: number | null;
  actualCostUsd: number;
  totalTokensSent: number;
}) {
  if (point.cacheSavingsUsd == null) {
    return {
      compressibleCostUsd: Math.max(0, point.actualCostUsd),
      compressibleTokensSent: Math.max(0, point.totalTokensSent)
    };
  }
  const cacheSavingsUsd = Math.max(0, point.cacheSavingsUsd);
  const compressibleCostUsd = Math.max(0, point.actualCostUsd - readCostUsd(point));
  const fullPriceInput = Math.max(0, point.actualCostUsd) + cacheSavingsUsd;
  const compressibleShare = fullPriceInput > 0 ? compressibleCostUsd / fullPriceInput : 1;
  return {
    compressibleCostUsd,
    compressibleTokensSent: Math.round(Math.max(0, point.totalTokensSent) * compressibleShare)
  };
}

/**
 * Tokens the "spent" bar plots, on the NEW-INPUT basis (see the invariant on
 * `newInputSavingsRate`). Exact new input (uncached + cache-write) when the
 * bucket was sampled for it; otherwise the cache-read-stripped dollar-share
 * approximation from `compressibleSpend`.
 *
 * The one thing it must NEVER return is the full forwarded count: that puts the
 * re-sent cached prefix into the denominator, which is the 2026-09-02
 * inflation (a cache-heavy bucket forwarded 928M tokens, 681M of them cache
 * reads Headroom never touches -- see `bar_spent_never_counts_cache_reads`).
 * `compressibleSpend`'s no-coverage branch returns the full count on purpose
 * for buckets with genuinely no caching, where full == new input; the guard
 * test pins that the moment cache reads exist, they are excluded.
 */
export function newInputTokensForBar(point: {
  newInputTokens?: number;
  cacheSavingsUsd?: number | null;
  cacheReadCostUsd?: number | null;
  actualCostUsd: number;
  totalTokensSent: number;
}) {
  if (point.newInputTokens && point.newInputTokens > 0) return point.newInputTokens;
  return compressibleSpend(point).compressibleTokensSent;
}

export function buildMonthlySavingsChartData(data: DailySavingsPoint[]): SavingsChartDatum[] {
  return data.map((point) => ({
    bucketKey: point.date,
    bucketLabel: formatDayLabel(point.date),
    estimatedSavingsUsd: point.estimatedSavingsUsd,
    estimatedTokensSaved: point.estimatedTokensSaved,
    actualCostUsd: point.actualCostUsd,
    totalTokensSent: point.totalTokensSent,
    ...compressibleSpend(point),
    // New-input basis: what the bar plots is the new input, never the
    // cache-read-inflated forwarded count. Overrides compressibleSpend's
    // token figure with the exact per-bucket count when it was sampled.
    compressibleTokensSent: newInputTokensForBar(point),
    outputSavingsUsd: point.outputSavingsUsd ?? 0,
    outputTokensSaved: point.outputTokensSaved ?? 0,
    toolSchemaSavingsUsd: point.toolSchemaSavingsUsd ?? 0,
    toolSchemaTokensSaved: point.toolSchemaTokensSaved ?? 0,
    totalCostBeforeOptimization:
      point.actualCostUsd + point.estimatedSavingsUsd + (point.toolSchemaSavingsUsd ?? 0),
    totalTokensBeforeOptimization:
      point.totalTokensSent + point.estimatedTokensSaved + (point.toolSchemaTokensSaved ?? 0)
  }));
}

export interface ProviderSavingsDisplay {
  label: string;
  estimatedSavingsUsd: number;
  estimatedTokensSaved: number;
  actualCostUsd: number;
  totalTokensSent: number;
  // The group's own compressible spend (its input cost minus its own cache
  // reads) and the matching token figure, when every provider folded into it
  // reported its reads. Null when any did not, and the tooltip falls back to
  // the bucket-wide ratio. Tokens are split by the dollar share, never
  // differenced against provider read counts (see `compressibleSpend`).
  compressibleCostUsd: number | null;
  compressibleTokensSent: number | null;
}

// Fold the upstream per-provider breakdown into the two connectors the desktop
// supports. Anything that isn't OpenAI/Codex is attributed to Claude Code,
// including legacy "unknown" buckets from before per-provider attribution
// existed (a period when Codex wasn't supported, so all traffic was Claude).
// Claude Code is listed first. A group is shown only if at least one source
// provider mapped into it.
export function mergeProviderSavingsForDisplay(
  byProvider: ProviderSavingsPoint[]
): ProviderSavingsDisplay[] {
  // Backend rollups attribute savings by upstream provider only, so OpenCode
  // (and grok until provider "xai" ships) savings silently blend into these
  // rows. Deliberately NOT surfaced in the labels: the honest per-connector
  // split arrives with the backend's by_agent rollups (upstream #2627), at
  // which point display rows switch to agent-keyed.
  const groups = {
    claude: {
      label: "Claude Code",
      count: 0,
      estimatedSavingsUsd: 0,
      estimatedTokensSaved: 0,
      actualCostUsd: 0,
      totalTokensSent: 0,
      cacheSavingsUsd: 0,
      compressibleCostUsd: 0 as number | null
    },
    codex: {
      label: "ChatGPT Codex",
      count: 0,
      estimatedSavingsUsd: 0,
      estimatedTokensSaved: 0,
      actualCostUsd: 0,
      totalTokensSent: 0,
      cacheSavingsUsd: 0,
      compressibleCostUsd: 0 as number | null
    },
    grok: {
      label: "Grok Build",
      count: 0,
      estimatedSavingsUsd: 0,
      estimatedTokensSaved: 0,
      actualCostUsd: 0,
      totalTokensSent: 0,
      cacheSavingsUsd: 0,
      compressibleCostUsd: 0 as number | null
    }
  };
  for (const point of byProvider) {
    const provider = point.provider.toLowerCase();
    const group =
      provider === "openai"
        ? groups.codex
        : provider === "xai"
          ? groups.grok
          : groups.claude;
    group.count += 1;
    group.estimatedSavingsUsd += point.estimatedSavingsUsd;
    group.estimatedTokensSaved += point.estimatedTokensSaved;
    group.actualCostUsd += point.actualCostUsd;
    group.totalTokensSent += point.totalTokensSent;
    group.cacheSavingsUsd += Math.max(0, point.cacheSavingsUsd ?? 0);
    // Exact only when the rollup priced THIS provider's reads; one provider
    // without it makes the group's figure unknowable.
    group.compressibleCostUsd =
      group.compressibleCostUsd == null || point.cacheReadCostUsd == null
        ? null
        : group.compressibleCostUsd +
          Math.max(0, point.actualCostUsd - Math.max(0, point.cacheReadCostUsd));
  }
  return [groups.claude, groups.codex, groups.grok]
    .filter((group) => group.count > 0)
    .map(({ count: _count, cacheSavingsUsd, ...display }) => {
      const cost = display.compressibleCostUsd;
      const fullPriceInput = Math.max(0, display.actualCostUsd) + cacheSavingsUsd;
      return {
        ...display,
        compressibleTokensSent:
          cost == null
            ? null
            : Math.round(
                Math.max(0, display.totalTokensSent) *
                  (fullPriceInput > 0 ? cost / fullPriceInput : 1)
              )
      };
    });
}

/**
 * Per-connector "Spent" tokens for the hourly hover. They must add up to the
 * bar, which is the Input chip's new-input denominator (exact on sampled
 * hours): each connector's own cache-stripped estimate only decides the split.
 * When any connector lacks per-provider reads, every row falls back to the
 * bucket's share, which adds up to the bar too.
 */
export function providerSpentTokens(
  providers: ReadonlyArray<{ totalTokensSent: number; compressibleTokensSent: number | null }>,
  bar: { totalTokensSent: number; compressibleTokensSent: number }
): number[] {
  const estimates = providers.map((provider) => provider.compressibleTokensSent);
  const sum = estimates.reduce<number>((acc, tokens) => acc + (tokens ?? NaN), 0);
  if (sum > 0) {
    return estimates.map((tokens) => ((tokens ?? 0) / sum) * bar.compressibleTokensSent);
  }
  const share = bar.totalTokensSent > 0 ? bar.compressibleTokensSent / bar.totalTokensSent : 1;
  return providers.map((provider) => provider.totalTokensSent * share);
}

export function buildHourlySavingsChartData(data: HourlySavingsPoint[]): SavingsChartDatum[] {
  return data.map((point) => ({
    bucketKey: point.hour,
    bucketLabel: formatHourLabel(point.hour),
    estimatedSavingsUsd: point.estimatedSavingsUsd,
    estimatedTokensSaved: point.estimatedTokensSaved,
    actualCostUsd: point.actualCostUsd,
    totalTokensSent: point.totalTokensSent,
    ...compressibleSpend(point),
    // New-input basis (see the monthly builder and newInputSavingsRate).
    compressibleTokensSent: newInputTokensForBar(point),
    outputSavingsUsd: point.outputSavingsUsd ?? 0,
    outputTokensSaved: point.outputTokensSaved ?? 0,
    toolSchemaSavingsUsd: point.toolSchemaSavingsUsd ?? 0,
    toolSchemaTokensSaved: point.toolSchemaTokensSaved ?? 0,
    totalCostBeforeOptimization:
      point.actualCostUsd + point.estimatedSavingsUsd + (point.toolSchemaSavingsUsd ?? 0),
    totalTokensBeforeOptimization:
      point.totalTokensSent + point.estimatedTokensSaved + (point.toolSchemaTokensSaved ?? 0),
    byProvider: point.byProvider ?? []
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

function parsedLearnDate(project: { lastLearnRanAt: string | null }): Date | null {
  if (!project.lastLearnRanAt) {
    return null;
  }
  const parsed = new Date(project.lastLearnRanAt);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

// True when Learn has never produced a usable timestamp for this project, so
// empty pattern counts mean "not scanned yet" rather than "scanned, found none".
export function hasNeverScanned(project: { lastLearnRanAt: string | null }): boolean {
  return parsedLearnDate(project) === null;
}

export function formatLearnStatus(project: {
  lastLearnRanAt: string | null;
}): string {
  const parsed = parsedLearnDate(project);
  if (!parsed) {
    return "never scan";
  }
  // Local calendar days, not elapsed 24h periods: a scan at 23:30 is
  // "yesterday" at 09:00. Math.round absorbs 23h/25h DST days; the clamp keeps
  // a clock-skewed future stamp at "today".
  const diffDays = Math.max(
    0,
    Math.round((startOfDay(new Date()).getTime() - startOfDay(parsed).getTime()) / 86_400_000)
  );
  if (diffDays === 0) return "last scan: today";
  if (diffDays === 1) return "last scan: yesterday";
  return `last scan: ${diffDays} days ago`;
}

const SUPPORTED_CONNECTOR_IDS = new Set([
  "claude_code",
  "codex",
  "grok_build",
  "opencode"
]);

export function baseUrlTakeoverNotice(replaced: string): string {
  return `This client was routed through ${replaced}. Headroom puts it back when you turn the connector off.`;
}

// Null when there is nothing beyond the switch itself: the row's status line
// already carries the restart hint.
export function clientSetupNotice(result: ClientSetupResult): string | null {
  const parts = [
    ...(result.replacedBaseUrl ? [baseUrlTakeoverNotice(result.replacedBaseUrl)] : []),
    ...result.nextSteps
  ];
  return parts.length > 0 ? parts.join(" ") : null;
}

export type ConnectorStatusLine = {
  text: string;
  tone: "reason" | "restart";
};

// A client picks up routing only when it restarts, and nothing local tells us
// whether the user did. Rather than nag forever, the hint rides the configure
// timestamp: relevant right after enabling, gone by the next day.
const RESTART_HINT_WINDOW_MS = 24 * 60 * 60 * 1000;

// Connectors the Claude pricing gate does not cover while the user is
// authenticated: Codex has its own proxy-side gate (codex_bypass); OpenCode and
// Grok bill against the user's own provider keys, so the Claude gate has
// nothing to meter. Plan-usage metering only, see `connectorGateMessage`.
const GATE_EXEMPT_CONNECTOR_IDS = new Set(["codex", "opencode", "grok_build"]);

function claudeGateCovers(
  connector: ClientConnectorStatus,
  pricing: HeadroomPricingStatus | null
): boolean {
  return (
    pricing != null &&
    !pricing.optimizationAllowed &&
    (!pricing.authenticated || !GATE_EXEMPT_CONNECTOR_IDS.has(connector.clientId))
  );
}

// Why this connector's traffic is not optimized right now, or null. An
// already-on connector stays on and the intercept keeps it unoptimized; its row
// shows this message with the upgrade/sign-in CTA. The exemption above does not
// reach the account wall (trial ended / sign-in required), which bypasses every
// connector's traffic (`account_gate` in proxy_intercept.rs), and Codex's own
// gate can close while Claude's is open.
export function connectorGateMessage(
  connector: ClientConnectorStatus,
  pricing: HeadroomPricingStatus | null
): string | null {
  if (pricing == null) {
    return null;
  }
  const accountWall =
    !pricing.optimizationAllowed &&
    (pricing.gateReason === "trial_ended" || pricing.gateReason === "sign_in_required");
  if (claudeGateCovers(connector, pricing) || accountWall) {
    return pricing.gateMessage;
  }
  if (connector.clientId === "codex" && pricing.codex?.optimizationAllowed === false) {
    return pricing.codex.gateMessage;
  }
  return null;
}

// Only blocks *enabling*, which is steered to the CTA instead. Behind the
// account wall the exempt connectors can still be enabled.
export function connectorGateBlocksEnable(
  connector: ClientConnectorStatus,
  pricing: HeadroomPricingStatus | null
): boolean {
  return claudeGateCovers(connector, pricing) && !connector.enabled;
}

// `gated`: the pricing gate covers this connector. A full bypass stops the
// backend on purpose, so an unanswering proxy is expected, not a fault; the
// row shows the gate message instead.
export function connectorStatusLine(
  connector: ClientConnectorStatus,
  now: number = Date.now(),
  gated = false
): ConnectorStatusLine | null {
  if (!connector.enabled) {
    return null;
  }
  // `verified` attests only that Headroom wrote what it needed to write, so it
  // is a setup failure, never "the client has not restarted yet".
  if (!connector.verified) {
    return {
      text: connector.verification
        ? "Setup incomplete. The info button lists what failed."
        : "Couldn't verify setup. Re-check from the info button.",
      tone: "reason"
    };
  }
  if (gated) {
    return null;
  }
  if (connector.verification && !connector.verification.proxyReachable) {
    return {
      text: "Headroom's proxy isn't answering yet.",
      tone: "reason"
    };
  }
  const configuredAt = connector.lastConfiguredAt
    ? Date.parse(connector.lastConfiguredAt)
    : Number.NaN;
  if (Number.isFinite(configuredAt) && now - configuredAt < RESTART_HINT_WINDOW_MS) {
    return {
      text: `Restart ${connector.name} if it's already open.`,
      tone: "restart"
    };
  }
  return null;
}

export function aggregateClientConnectors(connectors: ClientConnectorStatus[]) {
  return connectors.filter((connector) =>
    SUPPORTED_CONNECTOR_IDS.has(connector.clientId)
  );
}

export function sortClientConnectors(connectors: ClientConnectorStatus[]) {
  return [...connectors].sort((left, right) => {
    if (left.installed !== right.installed) {
      return left.installed ? -1 : 1;
    }
    return left.name.localeCompare(right.name);
  });
}

export function getEnabledSupportedConnectors(
  connectors: ClientConnectorStatus[]
) {
  return aggregateClientConnectors(connectors).filter(
    (connector) => connector.enabled
  );
}

export function hasEnabledConnector(connectors: ClientConnectorStatus[]) {
  return getEnabledSupportedConnectors(connectors).length > 0;
}

export type ConnectorDashboardTone = "active" | "pending" | "idle" | "off";

export function connectorDashboardStatus(
  connector: ClientConnectorStatus,
  opts?: { proxyReachable?: boolean }
): {
  label: string;
  tone: ConnectorDashboardTone;
} {
  if (!connector.enabled) {
    // Deliberately off, nothing wrong: gray, not red. Red ("idle") is
    // reserved for enabled-but-broken (see the proxyReachable override).
    return connector.installed
      ? { label: "Off", tone: "off" }
      : { label: "Not installed", tone: "off" };
  }
  if (opts?.proxyReachable === false) {
    return { label: "Proxy unreachable", tone: "idle" };
  }
  if (!connector.verified) {
    return connector.installed
      ? { label: "Verifying", tone: "pending" }
      : { label: "Restart needed", tone: "pending" };
  }
  return { label: "Active", tone: "active" };
}

export type CalloutTone =
  | "disconnected"
  | "auto-paused"
  | "paused"
  | "starting"
  | "degraded"
  | "disabled"
  | "healthy";

export interface CalloutBanner {
  tone: CalloutTone;
  title: string;
}

// A startup hint is prose: "what is wrong. What to do." The headline
// carries its first sentence and the rest renders underneath it. Short
// issue fragments ("proxy unreachable") have no sentence break and stay
// inline, joined as before.
export function splitIssue(issue: string): { lead: string; detail: string } {
  const cut = issue.search(/[.!?] (?=[A-Z])/);
  return cut === -1
    ? { lead: issue, detail: "" }
    : { lead: issue.slice(0, cut + 1), detail: issue.slice(cut + 2) };
}

export function endSentence(text: string): string {
  return /[.!?]$/.test(text) ? text : `${text}.`;
}

// The Home callout banner. Branch order is precedence: the first state that
// applies owns the headline.
export function calloutBannerFor({
  runtimeStatus,
  pricingStatus,
  runtimeIssues,
  runtimeHealthy,
  kompressWarming,
  connectorPhase
}: {
  runtimeStatus: RuntimeStatus | null;
  pricingStatus: HeadroomPricingStatus | null;
  runtimeIssues: string[];
  runtimeHealthy: boolean;
  kompressWarming: boolean;
  connectorPhase: "disabled" | "verifying" | "healthy";
}): CalloutBanner {
  const primaryIssue = runtimeIssues.length > 0 ? splitIssue(runtimeIssues[0]) : null;
  const issueSummary = primaryIssue?.detail ? primaryIssue.lead : runtimeIssues.join(", ");

  if (!runtimeStatus) {
    return {
      tone: "disconnected",
      title: "Headroom status is unavailable."
    };
  }

  if (runtimeStatus.paused) {
    if (runtimeStatus.autoPaused) {
      // A known cause (App Control, antivirus) leads; its remedy renders
      // underneath. "Stopped unexpectedly" plus Resume hid it, and Resume
      // cannot get past a machine that refuses the runtime.
      return {
        tone: "auto-paused",
        title: primaryIssue?.detail
          ? primaryIssue.lead
          : "Headroom stopped unexpectedly. Traffic is passing through unoptimized."
      };
    }
    return {
      tone: "paused",
      title: "Headroom is paused."
    };
  }

  if (runtimeStatus.starting) {
    return {
      tone: "starting",
      title: "Headroom is starting up."
    };
  }

  // Every client is wired to 127.0.0.1:6767, so a failed intercept bind means
  // nothing reaches Headroom and the bypass cannot pass traffic through either.
  // A pricing banner here would claim traffic still flows unoptimized and hide
  // the one remedy that works (freeing the port).
  // Keyed on the hint itself, not runtimeIssues: `running` only reflects the
  // backend on 6768, so a healthy backend behind a foreign-held 6767 never
  // puts the hint in runtimeIssues.
  if (runtimeStatus.interceptBindFailed && runtimeStatus.startupErrorHint) {
    return {
      tone: "disconnected",
      title: endSentence(
        `Headroom is not hooked up right now: ${splitIssue(runtimeStatus.startupErrorHint).lead}`
      )
    };
  }

  if (pricingStatus?.needsAuthentication) {
    return {
      tone: "degraded",
      title: pricingStatus.gateMessage
    };
  }

  if (pricingStatus && !pricingStatus.optimizationAllowed) {
    return {
      tone: "disabled",
      title: pricingStatus.gateMessage
    };
  }

  if (pricingStatus?.shouldNudge) {
    return {
      tone: "starting",
      title: pricingStatus.gateMessage
    };
  }

  // Codex-only gate: surface in the top banner only when the Claude side isn't
  // itself gating/nudging (handled above), so mixed users never get a double
  // banner. Codex billing/pausing is scoped to Codex traffic.
  const codexUsage = pricingStatus?.codex;
  if (codexUsage && codexUsage.optimizationAllowed === false) {
    return {
      tone: "disabled",
      title: codexUsage.gateMessage
    };
  }
  if (codexUsage?.shouldNudge) {
    return {
      tone: "starting",
      title: codexUsage.gateMessage
    };
  }

  if (runtimeHealthy) {
    if (connectorPhase === "disabled") {
      return {
        tone: "disabled",
        title: "No coding tools connected, so Headroom isn't saving anything."
      };
    }
    if (connectorPhase === "verifying") {
      return {
        tone: "starting",
        title: "Send a message in a connected tool to verify the connection is working. You may need to restart it first."
      };
    }
    if (kompressWarming) {
      return {
        tone: "healthy",
        title: "Headroom is running while finishing setup."
      };
    }
    return {
      tone: "healthy",
      title: "Headroom is running and trimming prompt bloat."
    };
  }

  const disconnected = !runtimeStatus.installed || !runtimeStatus.running || !runtimeStatus.proxyReachable;
  return {
    tone: disconnected ? "disconnected" : "degraded",
    title: disconnected
      ? runtimeIssues.length > 0
        ? endSentence(`Headroom is not hooked up right now: ${issueSummary}`)
        : "Headroom is not hooked up right now."
      : runtimeIssues.length > 0
        ? endSentence(`Headroom needs attention: ${issueSummary}`)
        : "Headroom is running, but something needs attention."
  };
}
