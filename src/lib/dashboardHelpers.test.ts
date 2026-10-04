import { afterEach, describe, expect, it, vi } from "vitest";
import {
  aggregateClientConnectors,
  baseUrlTakeoverNotice,
  buildHourlySavingsChartData,
  buildHourlySavingsWindow,
  buildMonthlySavingsChartData,
  buildMonthlySavingsWindow,
  compressibleInputSavingsRate,
  compressibleSpend,
  readCostUsd,
  newInputSavingsRate,
  newInputTokensForBar,
  allTimeCacheHitPair,
  cacheHitPair,
  calloutBannerFor,
  outputReductionForWindow,
  compactNumber,
  connectorDashboardStatus,
  connectorGateBlocksEnable,
  connectorGateMessage,
  connectorStatusLine,
  clientSetupNotice,
  currency,
  currencyExact,
  dayOfMonthTickFormatter,
  earliestHourlyDay,
  earliestSavingsMonth,
  formatDateTime,
  formatDayKey,
  formatLearnStatus,
  hasNeverScanned,
  historyOverlayCaption,
  formatMonthLabel,
  formatSelectedDayLabel,
  getEnabledSupportedConnectors,
  hasEnabledConnector,
  hourOfDayTickFormatter,
  mergeProviderSavingsForDisplay,
  providerSpentTokens,
  percent1,
  savingsRate,
  sortClientConnectors
} from "./dashboardHelpers";
import type {
  HeadroomPricingStatus,
  RuntimeStatus,
  ClientConnectorStatus,
  ClientSetupResult,
  DailySavingsPoint,
  HourlySavingsPoint
} from "./types";

describe("dashboard helpers", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("formats stable numeric summaries", () => {
    expect(currencyExact(12.345)).toBe("$12.35");
    expect(currency(9999)).toBe("$9,999");
    expect(currency(0.37)).toBe("$0.37");
    expect(currency(1)).toBe("$1");
    expect(currency(15_432)).toContain("K");
    expect(compactNumber(12_345)).toBe("12.3K");
    expect(percent1(18)).toBe("18.0");
  });

  it("computes savings rate against the would-be total", () => {
    expect(savingsRate(15, 85)).toBe(15);
    expect(savingsRate(0, 0)).toBeNull();
    expect(savingsRate(0, 100)).toBe(0);
    expect(savingsRate(-5, 100)).toBe(0);
  });

  it("renders near-zero negatives as clean zero, not -$0", () => {
    expect(currency(-0.003)).toBe("$0");
    expect(currencyExact(-0.003)).toBe("$0.00");
    expect(currency(-1.5)).toBe("-$2"); // genuine negatives still show
  });

  it("builds full monthly windows with zero-filled gaps", () => {
    const data: DailySavingsPoint[] = [
      {
        date: "2024-02-02",
        estimatedSavingsUsd: 2.5,
        estimatedTokensSaved: 250,
        actualCostUsd: 1.5,
        totalTokensSent: 1000
      }
    ];

    const month = new Date(2024, 1, 18);
    const windowed = buildMonthlySavingsWindow(data, month);

    expect(windowed).toHaveLength(29);
    expect(windowed[0]).toEqual({
      date: "2024-02-01",
      estimatedSavingsUsd: 0,
      estimatedTokensSaved: 0,
      actualCostUsd: 0,
      totalTokensSent: 0,
      outputSavingsUsd: 0,
      outputTokensSaved: 0
    });
    expect(windowed[1]).toEqual(data[0]);
    expect(windowed[28].date).toBe("2024-02-29");
  });

  it("plots the backend's output estimate whether or not the bucket was sampled", () => {
    const base = { estimatedSavingsUsd: 1, estimatedTokensSaved: 100, actualCostUsd: 1, totalTokensSent: 500, byProvider: [] };
    const [unscored, sampled, none] = buildHourlySavingsChartData([
      { ...base, hour: "2026-10-03T10:00", outputTokensSaved: 8_000 },
      { ...base, hour: "2026-10-03T11:00", outputTokensSaved: 8_000, outputSampledTokensSaved: 300, outputBaselineTokens: 900 },
      { ...base, hour: "2026-10-03T12:00" }
    ]);
    expect(unscored.outputTokensSaved).toBe(8_000);
    expect(sampled.outputTokensSaved).toBe(8_000);
    expect(none.outputTokensSaved).toBe(0);
  });

  it("builds hourly windows and chart data with derived totals", () => {
    const data: HourlySavingsPoint[] = [
      {
        hour: "2024-03-05T04:00",
        estimatedSavingsUsd: 1.25,
        estimatedTokensSaved: 125,
        actualCostUsd: 0.75,
        totalTokensSent: 500,
        byProvider: [
          {
            provider: "anthropic",
            estimatedSavingsUsd: 1.25,
            estimatedTokensSaved: 125,
            actualCostUsd: 0.75,
            totalTokensSent: 500
          }
        ]
      }
    ];

    const windowed = buildHourlySavingsWindow(data, new Date(2024, 2, 5, 12));
    const chartData = buildHourlySavingsChartData(windowed);

    expect(windowed).toHaveLength(24);
    expect(windowed[4]).toEqual(data[0]);
    expect(windowed[3].hour).toBe("2024-03-05T03:00");
    expect(chartData[4]).toMatchObject({
      bucketKey: "2024-03-05T04:00",
      estimatedSavingsUsd: 1.25,
      estimatedTokensSaved: 125,
      actualCostUsd: 0.75,
      totalTokensSent: 500,
      totalCostBeforeOptimization: 2,
      totalTokensBeforeOptimization: 625
    });
    // Per-provider breakdown carries through; padded hours default to empty.
    expect(chartData[4].byProvider).toEqual(data[0].byProvider);
    expect(chartData[3].byProvider).toEqual([]);
  });

  it("plots the compressible slice of spend, not the cache-read-inflated total", () => {
    const covered: HourlySavingsPoint[] = [
      {
        hour: "2024-03-05T04:00",
        estimatedSavingsUsd: 1,
        estimatedTokensSaved: 100,
        actualCostUsd: 10,
        totalTokensSent: 1000,
        cacheReadTokens: 600,
        // $9 of discount earned => $1 of actual read cost.
        cacheSavingsUsd: 9,
        byProvider: []
      }
    ];

    const chartData = buildHourlySavingsChartData(covered);
    expect(chartData[0]).toMatchObject({
      actualCostUsd: 10,
      totalTokensSent: 1000,
      compressibleCostUsd: 9,
      // $9 compressible out of $19 of full-price input, applied to our own
      // token count -- never `totalTokensSent - cacheReadTokens`.
      compressibleTokensSent: 474
    });

    // No cache coverage: nothing to subtract, bars keep the full figure.
    const uncovered = buildHourlySavingsChartData([{ ...covered[0], cacheReadTokens: undefined, cacheSavingsUsd: undefined }]);
    expect(uncovered[0]).toMatchObject({ compressibleCostUsd: 10, compressibleTokensSent: 1000 });
  });

  // Regression guard for the 2026-09-02 spent inflation: the history chart
  // must plot NEW input as "spent", never the full forwarded count (which
  // carries the re-sent cached prefix). See the invariant on
  // `newInputSavingsRate` and the memory `savings-spent-inflation-guard`.
  it("bar_spent_never_counts_cache_reads: plots exact new input, excludes the cached prefix", () => {
    // The real shape that inflated Garm's chart: a cache-heavy hour forwarded
    // 928M tokens, 681M of them cache reads. Spent must show the ~247M of new
    // input, nowhere near 928M.
    const point = {
      hour: "2026-09-02T14:00",
      estimatedSavingsUsd: 50,
      estimatedTokensSaved: 5_000_000,
      actualCostUsd: 2451,
      totalTokensSent: 928_000_000,
      cacheReadTokens: 681_000_000,
      cacheSavingsUsd: 3651,
      newInputTokens: 247_000_000,
      byProvider: []
    };
    const [datum] = buildHourlySavingsChartData([point]);
    expect(datum.compressibleTokensSent).toBe(247_000_000);
    // The load-bearing assertion: spent is a small slice of forwarded, so the
    // 681M cache reads cannot have leaked back into the denominator.
    expect(datum.compressibleTokensSent).toBeLessThan(point.totalTokensSent / 3);
    expect(newInputTokensForBar(point)).toBe(247_000_000);
  });

  it("newInputTokensForBar strips cache reads via the dollar share when no per-bucket new-input sample exists", () => {
    // No newInputTokens yet, but cache coverage is present: fall back to the
    // stripped approximation (474 of 1000), never leave the reads in.
    const point = { actualCostUsd: 10, totalTokensSent: 1000, cacheSavingsUsd: 9 };
    const spent = newInputTokensForBar(point);
    expect(spent).toBe(474);
    expect(spent).toBeLessThan(point.totalTokensSent);
  });

  it("keeps the token bar non-zero when provider cache reads exceed forwarded input", () => {
    // Real 2026-08-21T08:00 bucket: differencing the two scales clamped this
    // to a flat zero bar while the dollar bar showed $11.78.
    const chartData = buildHourlySavingsChartData([
      {
        hour: "2026-08-21T08:00",
        estimatedSavingsUsd: 1,
        estimatedTokensSaved: 100,
        actualCostUsd: 98.46,
        totalTokensSent: 77_148_652,
        cacheReadTokens: 102_285_108,
        cacheSavingsUsd: 780.1,
        byProvider: []
      }
    ]);
    expect(chartData[0].compressibleCostUsd).toBeGreaterThan(0);
    expect(chartData[0].compressibleTokensSent).toBeGreaterThan(0);
    expect(chartData[0].compressibleTokensSent).toBeLessThan(77_148_652);
  });

  it("builds monthly chart data and finds earliest visible history", () => {
    const dailyData: DailySavingsPoint[] = [
      {
        date: "2024-01-30",
        estimatedSavingsUsd: 1,
        estimatedTokensSaved: 100,
        actualCostUsd: 3,
        totalTokensSent: 1000
      },
      {
        date: "2024-03-01",
        estimatedSavingsUsd: 2,
        estimatedTokensSaved: 200,
        actualCostUsd: 4,
        totalTokensSent: 2000
      }
    ];
    const hourlyData: HourlySavingsPoint[] = [
      {
        hour: "2024-02-14T21:00",
        estimatedSavingsUsd: 0.5,
        estimatedTokensSaved: 50,
        actualCostUsd: 1,
        totalTokensSent: 300,
        byProvider: []
      }
    ];

    const chartData = buildMonthlySavingsChartData(dailyData);

    expect(chartData[0]).toMatchObject({
      bucketKey: "2024-01-30",
      totalCostBeforeOptimization: 4,
      totalTokensBeforeOptimization: 1100
    });
    expect(formatDayKey(earliestSavingsMonth(dailyData) as Date)).toBe("2024-01-01");
    expect(formatDayKey(earliestHourlyDay(hourlyData) as Date)).toBe("2024-02-14");
  });

  it("formats chart ticks predictably", () => {
    expect(dayOfMonthTickFormatter("2024-02-01")).toBe("1");
    expect(dayOfMonthTickFormatter("2024-02-02")).toBe("");
    expect(dayOfMonthTickFormatter("2024-02-29")).toBe("29");
    expect(hourOfDayTickFormatter("2024-02-01T04:00")).toBe("04");
    expect(hourOfDayTickFormatter("2024-02-01T05:00")).toBe("");
    expect(hourOfDayTickFormatter("2024-02-01T23:00")).toBe("23");
  });

  it("filters and sorts client connectors", () => {
    const connectors: ClientConnectorStatus[] = [
      { clientId: "zed", name: "Zed", installed: false, enabled: false, verified: false },
      { clientId: "claude_code", name: "Claude Code", installed: true, enabled: true, verified: true },
      { clientId: "cursor", name: "Cursor", installed: true, enabled: false, verified: false }
    ];

    expect(aggregateClientConnectors(connectors)).toEqual([connectors[1]]);
    expect(sortClientConnectors(connectors).map((connector) => connector.clientId)).toEqual([
      "claude_code",
      "cursor",
      "zed"
    ]);
  });

  it("keeps connector setup follow-up steps visible", () => {
    const result: ClientSetupResult = {
      clientId: "codex",
      applied: true,
      alreadyConfigured: false,
      summary: "Configured",
      changedFiles: [],
      backupFiles: [],
      nextSteps: ["Restart Codex.", "Run /hooks."],
      verification: {
        clientId: "codex",
        verified: true,
        proxyReachable: true,
        checks: [],
        failures: []
      }
    };

    expect(clientSetupNotice(result)).toBe("Restart Codex. Run /hooks.");
    expect(clientSetupNotice({ ...result, nextSteps: [] })).toBeNull();
    expect(baseUrlTakeoverNotice("https://gateway.example")).toContain(
      "https://gateway.example"
    );
  });

  it("reports connector status without mistaking an unrestarted client for a broken one", () => {
    const now = Date.parse("2026-08-22T12:00:00Z");
    const base: ClientConnectorStatus = {
      clientId: "codex",
      name: "Codex",
      installed: false,
      enabled: true,
      verified: true,
      lastConfiguredAt: "2026-08-22T11:00:00Z",
      verification: {
        clientId: "codex",
        verified: true,
        proxyReachable: true,
        checks: ["Found Headroom-managed provider block in ~/.codex/config.toml."],
        failures: []
      }
    };

    // Just configured: the one thing Headroom cannot do for the user.
    expect(connectorStatusLine(base, now)).toEqual({
      text: "Restart Codex if it's already open.",
      tone: "restart"
    });
    // ...and it stops nagging a working setup the next day.
    expect(connectorStatusLine(base, now + 25 * 60 * 60 * 1000)).toBeNull();
    // Not installed is not a fault: Codex desktop/IDE share ~/.codex/config.toml.
    expect(connectorStatusLine({ ...base, lastConfiguredAt: null }, now)).toBeNull();
    expect(connectorStatusLine({ ...base, enabled: false }, now)).toBeNull();

    expect(connectorStatusLine({ ...base, verified: false }, now)).toEqual({
      text: "Setup incomplete. The info button lists what failed.",
      tone: "reason"
    });
    expect(
      connectorStatusLine({ ...base, verified: false, verification: null }, now)
    ).toEqual({
      text: "Couldn't verify setup. Re-check from the info button.",
      tone: "reason"
    });
    expect(
      connectorStatusLine(
        { ...base, verification: { ...base.verification!, proxyReachable: false } },
        now
      )
    ).toEqual({
      text: "Headroom's proxy isn't answering yet.",
      tone: "reason"
    });
    // Gated: the backend is down on purpose and the row carries the gate
    // message, so neither the proxy warning nor the restart hint shows.
    // A setup failure still does.
    expect(
      connectorStatusLine(
        { ...base, verification: { ...base.verification!, proxyReachable: false } },
        now,
        true
      )
    ).toBeNull();
    expect(connectorStatusLine(base, now, true)).toBeNull();
    expect(connectorStatusLine({ ...base, verified: false }, now, true)?.tone).toBe("reason");
  });

  it("puts the gate message on every connector the gate bypasses", () => {
    const row = (clientId: string, enabled = true) =>
      ({ clientId, name: clientId, installed: true, enabled, verified: true }) as ClientConnectorStatus;
    const open = {
      authenticated: true,
      optimizationAllowed: true,
      gateReason: null,
      gateMessage: "Headroom is active.",
      codex: { optimizationAllowed: true, gateMessage: "Codex is active." }
    } as unknown as HeadroomPricingStatus;
    const weekly = { ...open, optimizationAllowed: false, gateReason: "weekly_usage_limit_reached", gateMessage: "Weekly limit reached." } as HeadroomPricingStatus;
    const wall = { ...open, optimizationAllowed: false, gateReason: "trial_ended", gateMessage: "Trial ended." } as HeadroomPricingStatus;

    expect(connectorGateMessage(row("claude_code"), null)).toBeNull();
    expect(connectorGateMessage(row("codex"), open)).toBeNull();
    // Claude's weekly meter: only Claude Code is bypassed, and only it is blocked.
    expect(connectorGateMessage(row("claude_code"), weekly)).toBe("Weekly limit reached.");
    expect(connectorGateMessage(row("codex"), weekly)).toBeNull();
    expect(connectorGateBlocksEnable(row("claude_code", false), weekly)).toBe(true);
    expect(connectorGateBlocksEnable(row("codex", false), weekly)).toBe(false);
    // W5: behind the trial wall the intercept bypasses Codex, OpenCode and Grok
    // too, on or off, so their rows say so. They can still be enabled.
    for (const id of ["claude_code", "codex", "opencode", "grok_build"]) {
      expect(connectorGateMessage(row(id), wall)).toBe("Trial ended.");
      expect(connectorGateMessage(row(id, false), wall)).toBe("Trial ended.");
    }
    expect(connectorGateBlocksEnable(row("codex", false), wall)).toBe(false);
    expect(connectorGateBlocksEnable(row("claude_code", false), wall)).toBe(true);
    // Codex's own meter closes independently of Claude's.
    const codexPaused = {
      ...open,
      codex: { optimizationAllowed: false, gateMessage: "Codex paused." }
    } as unknown as HeadroomPricingStatus;
    expect(connectorGateMessage(row("codex"), codexPaused)).toBe("Codex paused.");
    expect(connectorGateMessage(row("claude_code"), codexPaused)).toBeNull();
    // Signed out: everything is covered and blocked.
    const signedOut = { ...weekly, authenticated: false } as HeadroomPricingStatus;
    expect(connectorGateMessage(row("opencode"), signedOut)).toBe("Weekly limit reached.");
    expect(connectorGateBlocksEnable(row("opencode", false), signedOut)).toBe(true);
  });

  it("keeps codex, grok_build and opencode alongside claude_code as supported connectors", () => {
    const connectors: ClientConnectorStatus[] = [
      { clientId: "codex", name: "Codex", installed: true, enabled: false, verified: false },
      { clientId: "grok_build", name: "Grok Build", installed: true, enabled: false, verified: false },
      { clientId: "opencode", name: "OpenCode", installed: true, enabled: true, verified: false },
      { clientId: "claude_code", name: "Claude Code", installed: true, enabled: true, verified: true },
      { clientId: "cursor", name: "Cursor", installed: true, enabled: false, verified: false }
    ];

    expect(
      aggregateClientConnectors(connectors).map((connector) => connector.clientId).sort()
    ).toEqual(["claude_code", "codex", "grok_build", "opencode"]);
  });

  it("reports enabled supported connectors regardless of which tool", () => {
    const connectors: ClientConnectorStatus[] = [
      { clientId: "claude_code", name: "Claude Code", installed: true, enabled: false, verified: false },
      { clientId: "codex", name: "Codex", installed: true, enabled: true, verified: true },
      { clientId: "cursor", name: "Cursor", installed: true, enabled: true, verified: true }
    ];

    expect(getEnabledSupportedConnectors(connectors).map((c) => c.clientId)).toEqual(["codex"]);
    expect(hasEnabledConnector(connectors)).toBe(true);
    expect(
      hasEnabledConnector([
        { clientId: "claude_code", name: "Claude Code", installed: true, enabled: false, verified: false }
      ])
    ).toBe(false);
  });

  it("derives a dashboard status label/tone per connector state", () => {
    expect(
      connectorDashboardStatus({ clientId: "codex", name: "Codex", installed: false, enabled: false, verified: false })
    ).toEqual({ label: "Not installed", tone: "off" });
    expect(
      connectorDashboardStatus({ clientId: "codex", name: "Codex", installed: true, enabled: false, verified: false })
    ).toEqual({ label: "Off", tone: "off" });
    expect(
      connectorDashboardStatus({ clientId: "codex", name: "Codex", installed: true, enabled: true, verified: false })
    ).toEqual({ label: "Verifying", tone: "pending" });
    expect(
      connectorDashboardStatus({ clientId: "codex", name: "Codex", installed: true, enabled: true, verified: true })
    ).toEqual({ label: "Active", tone: "active" });
    // Red is reserved for enabled-but-broken: proxy down while a connector is on.
    expect(
      connectorDashboardStatus(
        { clientId: "codex", name: "Codex", installed: true, enabled: true, verified: true },
        { proxyReachable: false }
      )
    ).toEqual({ label: "Proxy unreachable", tone: "idle" });
  });

  it("formats timestamps and learn recency with clear fallbacks", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-03-27T12:00:00Z"));

    expect(formatDateTime(null)).toBe("Never");
    expect(formatDateTime("not-a-date")).toBe("Unknown");
    expect(formatLearnStatus({ lastLearnRanAt: null })).toBe("never scan");
    expect(formatLearnStatus({ lastLearnRanAt: "invalid" })).toBe("never scan");
    expect(formatLearnStatus({ lastLearnRanAt: "2026-03-27T08:00:00Z" })).toBe("last scan: today");
    expect(formatLearnStatus({ lastLearnRanAt: "2026-03-26T08:00:00Z" })).toBe("last scan: yesterday");
    expect(formatLearnStatus({ lastLearnRanAt: "2026-03-22T08:00:00Z" })).toBe("last scan: 5 days ago");
    expect(hasNeverScanned({ lastLearnRanAt: null })).toBe(true);
    expect(hasNeverScanned({ lastLearnRanAt: "invalid" })).toBe(true);
    expect(hasNeverScanned({ lastLearnRanAt: "2026-03-22T08:00:00Z" })).toBe(false);
  });

  it("dates a learn scan by local calendar day, not elapsed 24h periods", () => {
    vi.useFakeTimers();
    // 00:30 local; the scan ran an hour earlier, before local midnight.
    vi.setSystemTime(new Date(2026, 2, 27, 0, 30));
    const lateYesterday = new Date(2026, 2, 26, 23, 30).toISOString();
    expect(formatLearnStatus({ lastLearnRanAt: lateYesterday })).toBe("last scan: yesterday");
    const lateTwoDaysAgo = new Date(2026, 2, 25, 23, 30).toISOString();
    expect(formatLearnStatus({ lastLearnRanAt: lateTwoDaysAgo })).toBe("last scan: 2 days ago");
    // A clock-skewed future stamp still reads as today, never "-1 days ago".
    const later = new Date(2026, 2, 27, 5, 0).toISOString();
    expect(formatLearnStatus({ lastLearnRanAt: later })).toBe("last scan: today");
  });
});

describe("historyOverlayCaption", () => {
  const now = new Date(2026, 8, 29, 15, 0);

  it("calls the open period today / this month", () => {
    expect(historyOverlayCaption("day", new Date(2026, 8, 29), now)).toBe("saved today");
    expect(historyOverlayCaption("month", new Date(2026, 8, 1), now)).toBe("saved this month");
  });

  it("names an earlier period instead of calling it today / this month", () => {
    const pastDay = historyOverlayCaption("day", new Date(2026, 8, 25), now);
    expect(pastDay).not.toContain("today");
    expect(pastDay).toBe(`saved on ${formatSelectedDayLabel(new Date(2026, 8, 25))}`);
    const pastMonth = historyOverlayCaption("month", new Date(2026, 7, 1), now);
    expect(pastMonth).not.toContain("this month");
    expect(pastMonth).toBe(`saved in ${formatMonthLabel(new Date(2026, 7, 1))}`);
  });
});

describe("mergeProviderSavingsForDisplay", () => {
  it("folds anthropic and unknown into Claude Code, openai into Codex, and xai into Grok Build", () => {
    const merged = mergeProviderSavingsForDisplay([
      {
        provider: "openai",
        estimatedSavingsUsd: 0.04,
        estimatedTokensSaved: 40,
        actualCostUsd: 0.16,
        totalTokensSent: 80
      },
      {
        provider: "anthropic",
        estimatedSavingsUsd: 0.1,
        estimatedTokensSaved: 100,
        actualCostUsd: 0.24,
        totalTokensSent: 120
      },
      {
        provider: "unknown",
        estimatedSavingsUsd: 0.01,
        estimatedTokensSaved: 15,
        actualCostUsd: 0.03,
        totalTokensSent: 20
      },
      {
        provider: "xai",
        estimatedSavingsUsd: 0.02,
        estimatedTokensSaved: 25,
        actualCostUsd: 0.08,
        totalTokensSent: 50
      }
    ]);

    expect(merged).toEqual([
      {
        label: "Claude Code",
        estimatedSavingsUsd: 0.1 + 0.01,
        estimatedTokensSaved: 115,
        actualCostUsd: 0.24 + 0.03,
        totalTokensSent: 140,
        // No per-provider cache fields: the tooltip keeps the bucket-ratio fallback.
        compressibleCostUsd: null,
        compressibleTokensSent: null
      },
      {
        label: "ChatGPT Codex",
        estimatedSavingsUsd: 0.04,
        estimatedTokensSaved: 40,
        actualCostUsd: 0.16,
        totalTokensSent: 80,
        compressibleCostUsd: null,
        compressibleTokensSent: null
      },
      {
        label: "Grok Build",
        estimatedSavingsUsd: 0.02,
        estimatedTokensSaved: 25,
        actualCostUsd: 0.08,
        totalTokensSent: 50,
        compressibleCostUsd: null,
        compressibleTokensSent: null
      }
    ]);
  });

  it("omits a connector with no attributed providers", () => {
    const merged = mergeProviderSavingsForDisplay([
      {
        provider: "anthropic",
        estimatedSavingsUsd: 0.1,
        estimatedTokensSaved: 100,
        actualCostUsd: 0.24,
        totalTokensSent: 120
      }
    ]);

    expect(merged).toHaveLength(1);
    expect(merged[0].label).toBe("Claude Code");
  });

  it("returns nothing for an empty breakdown", () => {
    expect(mergeProviderSavingsForDisplay([])).toEqual([]);
  });

  it("drops each connector's OWN cache reads from its spend", () => {
    // Claude Code heavily cached, ChatGPT much less, in the same hour. The old
    // bucket-wide ratio gives both the hour's 0.55 share (ChatGPT $0.54,
    // Claude Code $7.92); each connector's own reads give $0.62 and $6.90,
    // so the fix lowers ChatGPT's rate and raises Claude Code's.
    const [claude, chatgpt] = mergeProviderSavingsForDisplay([
      {
        provider: "anthropic",
        estimatedSavingsUsd: 5.03,
        estimatedTokensSaved: 400_000,
        actualCostUsd: 14.5,
        totalTokensSent: 9_000_000,
        cacheSavingsUsd: 60,
        cacheReadCostUsd: 7.6
      },
      {
        provider: "openai",
        estimatedSavingsUsd: 1.98,
        estimatedTokensSaved: 200_000,
        actualCostUsd: 0.98,
        totalTokensSent: 1_000_000,
        cacheSavingsUsd: 3.24,
        cacheReadCostUsd: 0.36
      }
    ]);
    expect(claude.compressibleCostUsd).toBeCloseTo(6.9);
    expect(chatgpt.compressibleCostUsd).toBeCloseTo(0.62);
    // Tokens split by each connector's own dollar share, not the hour's.
    expect(chatgpt.compressibleTokensSent).toBe(Math.round(1_000_000 * (0.62 / (0.98 + 3.24))));
    expect(claude.compressibleTokensSent).toBe(Math.round(9_000_000 * (6.9 / (14.5 + 60))));
  });

  it("falls back for a whole group when any of its providers lacks priced reads", () => {
    // anthropic priced, "unknown" legacy: both fold into Claude Code, whose own
    // spend is then unknowable, so the tooltip must fall back rather than show
    // anthropic's slice as the whole group.
    const [claude] = mergeProviderSavingsForDisplay([
      {
        provider: "anthropic",
        estimatedSavingsUsd: 1,
        estimatedTokensSaved: 10,
        actualCostUsd: 4,
        totalTokensSent: 100,
        cacheSavingsUsd: 9,
        cacheReadCostUsd: 1
      },
      {
        provider: "unknown",
        estimatedSavingsUsd: 0.5,
        estimatedTokensSaved: 5,
        actualCostUsd: 2,
        totalTokensSent: 50
      }
    ]);
    expect(claude.compressibleCostUsd).toBeNull();
    expect(claude.compressibleTokensSent).toBeNull();
  });

});

describe("readCostUsd", () => {
  it("prefers the rollup-priced read cost and falls back to discount / 9", () => {
    // Fable 5.1 reads: $10/MTok list, $0.25/MTok read -> discount $9.75,
    // cost $0.25 per MTok. discount / 9 would say $1.083 (4.3x too high).
    expect(readCostUsd({ cacheSavingsUsd: 9.75, cacheReadCostUsd: 0.25 })).toBeCloseTo(0.25);
    expect(readCostUsd({ cacheSavingsUsd: 9.75 })).toBeCloseTo(9.75 / 9);
    expect(readCostUsd({ cacheSavingsUsd: null, cacheReadCostUsd: null })).toBe(0);
  });

  it("feeds the exact read cost through every rate built on it", () => {
    // $54.48 Fable spend, $7.23 saved, 1M-token-scale reads. With the rollup's
    // read cost the compressible spend is $54.48 - $8.33; with /9 it would be
    // $54.48 - $36.08 and the rate would read 28.2% instead of 13.5%.
    const bucket = {
      actualCostUsd: 54.48,
      estimatedSavingsUsd: 7.23,
      cacheSavingsUsd: 324.72,
      cacheReadCostUsd: 8.33
    };
    expect(compressibleInputSavingsRate([bucket])!.pct).toBeCloseTo(
      (7.23 / (7.23 + 54.48 - 8.33)) * 100
    );
    expect(compressibleSpend({ ...bucket, totalTokensSent: 0 }).compressibleCostUsd).toBeCloseTo(
      54.48 - 8.33
    );
    const pair = cacheHitPair([bucket])!;
    expect(pair.hitPct).toBeCloseTo(((8.33 + 324.72) / (54.48 + 324.72)) * 100);
    expect(pair.compressedPct).toBeCloseTo((7.23 / (7.23 + 54.48 - 8.33)) * 100);
  });
});

describe("cacheHitPair", () => {
  it("computes both rates in dollars over covered buckets only", () => {
    const pair = cacheHitPair([
      // Read discount $9 -> read cost $1, reads' full-price value $10.
      { cacheSavingsUsd: 9, actualCostUsd: 3, estimatedSavingsUsd: 0.5 },
      // No cache coverage: excluded from BOTH rates, not just the hit rate.
      { actualCostUsd: 50, estimatedSavingsUsd: 999 },
      { cacheSavingsUsd: 4.5, actualCostUsd: 2.5, estimatedSavingsUsd: 1.5 }
    ]);
    expect(pair).not.toBeNull();
    // reads at full price 15 / full-price input (5.5 + 13.5)
    expect(pair!.hitPct).toBeCloseTo((15 / 19) * 100);
    // saved 2 / (saved 2 + rest (5.5 - 1.5))
    expect(pair!.compressedPct).toBeCloseTo((2 / 6) * 100);
  });

  it("returns null when no bucket carries cache data", () => {
    expect(cacheHitPair([{ actualCostUsd: 3, estimatedSavingsUsd: 1 }])).toBeNull();
    expect(cacheHitPair([])).toBeNull();
  });

  it("does not saturate when provider cache reads exceed our token count", () => {
    // The 2026-08-17 regression: the token form of this pair ratioed the
    // provider's cache reads against our own tokenizer's forwarded count;
    // reads exceeded input, pinning the display at "100% hits, 100% of the
    // rest compressed". The dollar form has no cross-tokenizer ratio at all.
    const pair = cacheHitPair([
      { cacheSavingsUsd: 9, actualCostUsd: 3, estimatedSavingsUsd: 0.5 }
    ]);
    expect(pair!.hitPct).toBeCloseTo((10 / 12) * 100);
    expect(pair!.compressedPct).toBeCloseTo(20);
  });

  it("reports a fully-cached window as 0% of an empty remainder", () => {
    const pair = cacheHitPair([
      // Actual cost is exactly the read cost: nothing was left to compress.
      { cacheSavingsUsd: 9, actualCostUsd: 1, estimatedSavingsUsd: 0 }
    ]);
    expect(pair!.hitPct).toBe(100);
    expect(pair!.compressedPct).toBe(0);
  });
});

describe("allTimeCacheHitPair", () => {
  const breakdown = {
    compressionSavingsUsd: 4,
    outputSavingsUsd: 0,
    cacheSavingsUsd: 9, // read discount $9 -> read cost $1
    cacheReadTokens: 900,
    totalInputTokens: 1000,
    totalInputCostUsd: 3 // $1 reads + $2 billable
  };

  it("prices the lifetime breakdown as one synthetic bucket", () => {
    const pair = allTimeCacheHitPair(breakdown, 4);
    const direct = cacheHitPair([
      {
        cacheSavingsUsd: breakdown.cacheSavingsUsd,
        actualCostUsd: breakdown.totalInputCostUsd,
        estimatedSavingsUsd: 4
      }
    ]);
    expect(pair).toEqual(direct);
    expect(pair!.hitPct).toBeCloseTo(83.33, 2);
    // $4 saved against $4 saved + $2 paid at full price.
    expect(pair!.compressedPct).toBeCloseTo(66.67, 2);
  });

  it("prices lifetime reads at the ratio of exactly priced buckets", () => {
    // Buckets billed reads at 0.025x list: $0.25 read cost per $9.75 discount.
    const priced = [
      { cacheSavingsUsd: 9.75, cacheReadCostUsd: 0.25 },
      { cacheSavingsUsd: 5, cacheReadCostUsd: null }
    ];
    const pair = allTimeCacheHitPair(breakdown, 4, priced);
    const direct = cacheHitPair([
      {
        cacheSavingsUsd: breakdown.cacheSavingsUsd,
        cacheReadCostUsd: (9 * 0.25) / 9.75,
        actualCostUsd: breakdown.totalInputCostUsd,
        estimatedSavingsUsd: 4
      }
    ]);
    expect(pair).toEqual(direct);
    expect(pair!.compressedPct).toBeLessThan(allTimeCacheHitPair(breakdown, 4)!.compressedPct);
  });

  it("is null without cache coverage, whatever the dollars say", () => {
    // cacheReadTokens is the existence signal: no reads means no hit rate to
    // report, even though cacheSavingsUsd would divide fine.
    expect(allTimeCacheHitPair({ ...breakdown, cacheReadTokens: 0 }, 4)).toBeNull();
    expect(allTimeCacheHitPair(null, 4)).toBeNull();
    expect(allTimeCacheHitPair(undefined, 4)).toBeNull();
  });
});

describe("compressibleInputSavingsRate", () => {
  const bucket = {
    cacheReadTokens: 900,
    cacheSavingsUsd: 9, // read discount $9 -> read cost $1
    totalTokensSent: 1000,
    estimatedTokensSaved: 25,
    actualCostUsd: 3, // $1 reads + $2 billable
    estimatedSavingsUsd: 0.5
  };

  it("prices reads via the discount and excludes them", () => {
    // 0.5 / (0.5 + (3 - 9/9))
    expect(compressibleInputSavingsRate([bucket])!.pct).toBeCloseTo(20);
  });

  it("skips buckets without the dollar counter", () => {
    const uncovered = { ...bucket, cacheReadTokens: null, cacheSavingsUsd: null };
    expect(compressibleInputSavingsRate([uncovered])).toBeNull();
    expect(compressibleInputSavingsRate([bucket, uncovered])!.pct).toBeCloseTo(20);
  });

  it("stays sane when provider cache reads exceed our own input count", () => {
    // Real 2026-08-14 data: 195.8M derived cache reads against 163.4M forwarded
    // input tokens, because the two counters use different tokenizers. The old
    // token-based form clamped the denominator to 0 and reported 100%.
    const skewed = { ...bucket, cacheReadTokens: 1_200, totalTokensSent: 1_000 };
    expect(compressibleInputSavingsRate([skewed])!.pct).toBeCloseTo(20);
  });
});

describe("newInputSavingsRate", () => {
  const covered = { newInputTokens: 800, estimatedTokensSaved: 200 };

  it("rates saved tokens against sampled new input", () => {
    expect(newInputSavingsRate([covered])!.pct).toBeCloseTo(20);
    expect(newInputSavingsRate([covered])!.newInputTokens).toBe(800);
  });

  it("skips uncovered buckets on both sides of the ratio", () => {
    // Backend rollups and old buckets carry 0/absent newInputTokens; their
    // full-forwarded sent count -- and their saved tokens -- must stay out,
    // or the covered slice's numerator and denominator drift apart.
    const uncovered = { newInputTokens: 0, estimatedTokensSaved: 999_999 };
    const legacy = { estimatedTokensSaved: 5 };
    expect(newInputSavingsRate([uncovered, legacy])).toBeNull();
    expect(newInputSavingsRate([covered, uncovered, legacy])!.pct).toBeCloseTo(20);
  });
});

describe("outputReductionForWindow", () => {
  it("computes reduction over sampled buckets only", () => {
    const pct = outputReductionForWindow([
      { outputSampledTokensSaved: 300, outputBaselineTokens: 1000 },
      { outputSampledTokensSaved: null, outputBaselineTokens: null },
      { outputSampledTokensSaved: 100, outputBaselineTokens: 1000 }
    ]);
    expect(pct!.pct).toBeCloseTo(20);
  });

  it("nets a negative bucket against the window and floors only the total", () => {
    const window = outputReductionForWindow([
      { outputSampledTokensSaved: -100, outputBaselineTokens: 1000 },
      { outputSampledTokensSaved: 300, outputBaselineTokens: 1000 }
    ]);
    expect(window!.pct).toBeCloseTo(10);
    const negative = outputReductionForWindow([
      { outputSampledTokensSaved: -100, outputBaselineTokens: 1000 }
    ]);
    expect(negative!.pct).toBe(0);
    expect(negative!.savedTokens).toBe(0);
  });

  it("returns null without coverage or baseline", () => {
    expect(outputReductionForWindow([{}])).toBeNull();
    expect(
      outputReductionForWindow([{ outputSampledTokensSaved: 0, outputBaselineTokens: 0 }])
    ).toBeNull();
  });
});

describe("providerSpentTokens", () => {
  it("splits the bar's new-input tokens by each connector's own estimate", () => {
    // Sampled hour: the bar is exact new input (900), not the dollar-share sum (1200).
    const rows = providerSpentTokens(
      [
        { totalTokensSent: 10_000, compressibleTokensSent: 800 },
        { totalTokensSent: 2_000, compressibleTokensSent: 400 }
      ],
      { totalTokensSent: 12_000, compressibleTokensSent: 900 }
    );
    expect(rows).toEqual([600, 300]);
  });

  it("falls back to the bucket share when a connector has no per-provider reads", () => {
    const rows = providerSpentTokens(
      [
        { totalTokensSent: 10_000, compressibleTokensSent: 800 },
        { totalTokensSent: 2_000, compressibleTokensSent: null }
      ],
      { totalTokensSent: 12_000, compressibleTokensSent: 1_200 }
    );
    expect(rows).toEqual([1_000, 200]);
  });
});

describe("calloutBannerFor", () => {
  const runtime = (overrides: Partial<RuntimeStatus> = {}): RuntimeStatus => ({
    platform: "darwin",
    supportTier: "supported",
    installed: true,
    running: true,
    starting: false,
    paused: false,
    autoPaused: false,
    bypassed: false,
    proxyReachable: true,
    headroomLearnSupported: true,
    rtk: { installed: true, enabled: true, pathConfigured: true, hookConfigured: true },
    ...overrides
  });
  const pricing = (overrides: Partial<HeadroomPricingStatus> = {}) =>
    ({
      needsAuthentication: false,
      optimizationAllowed: true,
      shouldNudge: false,
      gateMessage: "Headroom is active.",
      codex: null,
      ...overrides
    }) as unknown as HeadroomPricingStatus;
  const healthy = {
    runtimeIssues: [],
    runtimeHealthy: true,
    kompressWarming: false,
    connectorPhase: "healthy" as const
  };
  const bindHint =
    "Port 6767 is in use by python3.12 (PID 4242). Quit that program, or end it in Task Manager, and Headroom reconnects on its own.";

  it("names a failed 6767 bind ahead of any pricing banner, since no client can connect", () => {
    const status = runtime({
      running: false,
      proxyReachable: false,
      startupErrorHint: bindHint,
      interceptBindFailed: true
    });
    const input = {
      runtimeStatus: status,
      runtimeIssues: [bindHint, "proxy unreachable"],
      runtimeHealthy: false,
      kompressWarming: false,
      connectorPhase: "healthy" as const
    };
    for (const gate of [
      pricing({ optimizationAllowed: false, gateMessage: "Your Headroom trial ended." }),
      pricing({ needsAuthentication: true, gateMessage: "Sign in to keep going." }),
      pricing({ shouldNudge: true, gateMessage: "You've used 80% of this week." })
    ]) {
      expect(calloutBannerFor({ ...input, pricingStatus: gate })).toEqual({
        tone: "disconnected",
        title: "Headroom is not hooked up right now: Port 6767 is in use by python3.12 (PID 4242)."
      });
    }
    // Only a bind failure outranks the gate; a backend that is merely down
    // behind a gated account keeps the gate message.
    expect(
      calloutBannerFor({
        ...input,
        runtimeStatus: { ...status, interceptBindFailed: false },
        pricingStatus: pricing({ optimizationAllowed: false, gateMessage: "Your Headroom trial ended." })
      })
    ).toEqual({ tone: "disabled", title: "Your Headroom trial ended." });
  });

  it("names a failed 6767 bind while the 6768 backend is healthy", () => {
    // `running` tracks the backend only, so the bind hint never reaches
    // runtimeIssues here; the banner must still name the port holder.
    const status = runtime({ running: true, startupErrorHint: bindHint, interceptBindFailed: true });
    for (const pricingStatus of [null, pricing({ shouldNudge: true, gateMessage: "You've used 80% of this week." })]) {
      expect(calloutBannerFor({ ...healthy, runtimeStatus: status, pricingStatus })).toEqual({
        tone: "disconnected",
        title: "Headroom is not hooked up right now: Port 6767 is in use by python3.12 (PID 4242)."
      });
    }
  });

  it("keeps the rest of the precedence order", () => {
    const codexGated = pricing({
      codex: { optimizationAllowed: false, shouldNudge: false, gateMessage: "Codex limit reached." }
    } as unknown as Partial<HeadroomPricingStatus>);
    const codexNudge = pricing({
      codex: { optimizationAllowed: true, shouldNudge: true, gateMessage: "Codex nearly out." }
    } as unknown as Partial<HeadroomPricingStatus>);
    const cases: [Parameters<typeof calloutBannerFor>[0], string, string][] = [
      [{ ...healthy, runtimeStatus: null, pricingStatus: null }, "disconnected", "Headroom status is unavailable."],
      [{ ...healthy, runtimeStatus: runtime({ paused: true, autoPaused: true }), pricingStatus: null }, "auto-paused", "Headroom stopped unexpectedly. Traffic is passing through unoptimized."],
      [{ ...healthy, runtimeStatus: runtime({ paused: true }), pricingStatus: null }, "paused", "Headroom is paused."],
      [{ ...healthy, runtimeStatus: runtime({ starting: true, interceptBindFailed: true }), pricingStatus: null }, "starting", "Headroom is starting up."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: pricing({ needsAuthentication: true, gateMessage: "Sign in." }) }, "degraded", "Sign in."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: pricing({ shouldNudge: true, gateMessage: "Nudge." }) }, "starting", "Nudge."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: codexGated }, "disabled", "Codex limit reached."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: codexNudge }, "starting", "Codex nearly out."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: null, connectorPhase: "disabled" }, "disabled", "No coding tools connected, so Headroom isn't saving anything."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: null, connectorPhase: "verifying" }, "starting", "Send a message in a connected tool to verify the connection is working. You may need to restart it first."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: null, kompressWarming: true }, "healthy", "Headroom is running while finishing setup."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: pricing() }, "healthy", "Headroom is running and trimming prompt bloat."],
      [{ ...healthy, runtimeStatus: runtime({ running: false }), pricingStatus: null, runtimeHealthy: false, runtimeIssues: ["runtime offline", "proxy unreachable"] }, "disconnected", "Headroom is not hooked up right now: runtime offline, proxy unreachable."],
      [{ ...healthy, runtimeStatus: runtime({ running: false }), pricingStatus: null, runtimeHealthy: false }, "disconnected", "Headroom is not hooked up right now."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: null, runtimeHealthy: false, runtimeIssues: ["MCP not configured"] }, "degraded", "Headroom needs attention: MCP not configured."],
      [{ ...healthy, runtimeStatus: runtime(), pricingStatus: null, runtimeHealthy: false }, "degraded", "Headroom is running, but something needs attention."]
    ];
    for (const [input, tone, title] of cases) {
      expect(calloutBannerFor(input)).toEqual({ tone, title });
    }
  });
});
