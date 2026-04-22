import { afterEach, describe, expect, it, vi } from "vitest";
import {
  aggregateClientConnectors,
  buildHourlySavingsChartData,
  buildHourlySavingsWindow,
  buildMonthlySavingsChartData,
  buildMonthlySavingsWindow,
  compactNumber,
  currency,
  currencyExact,
  dayOfMonthTickFormatter,
  earliestHourlyDay,
  earliestSavingsMonth,
  findClientVerificationLogLine,
  formatDateTime,
  formatDayKey,
  formatLearnStatus,
  hourOfDayTickFormatter,
  percent1,
  sortClientConnectors
} from "./dashboardHelpers";
import type {
  ClientConnectorStatus,
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
    expect(currency(15_432)).toContain("K");
    expect(compactNumber(12_345)).toBe("12.3K");
    expect(percent1(18)).toBe("18.0");
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
      totalTokensSent: 0
    });
    expect(windowed[1]).toEqual(data[0]);
    expect(windowed[28].date).toBe("2024-02-29");
  });

  it("builds hourly windows and chart data with derived totals", () => {
    const data: HourlySavingsPoint[] = [
      {
        hour: "2024-03-05T04:00",
        estimatedSavingsUsd: 1.25,
        estimatedTokensSaved: 125,
        actualCostUsd: 0.75,
        totalTokensSent: 500
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
        totalTokensSent: 300
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

  it("filters generic proxy noise when locating verification lines", () => {
    const lines = [
      "patterns, 42 compressions",
      "claude request POST /v1/messages completed",
      "anthropic handshake",
      "retrieval rate 0.2"
    ];

    expect(findClientVerificationLogLine("claude_code", lines)).toBe("anthropic handshake");
    expect(findClientVerificationLogLine("unknown", lines)).toBeNull();
  });

  it("formats timestamps and learn recency with clear fallbacks", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-03-27T12:00:00Z"));

    expect(formatDateTime(null)).toBe("Never");
    expect(formatDateTime("not-a-date")).toBe("Unknown");
    expect(formatLearnStatus({ lastLearnRanAt: null })).toBe("never analyzed");
    expect(formatLearnStatus({ lastLearnRanAt: null, hasPersistedLearnings: true })).toBe("existing learnings found");
    expect(formatLearnStatus({ lastLearnRanAt: "invalid" })).toBe("never analyzed");
    expect(formatLearnStatus({ lastLearnRanAt: "invalid", hasPersistedLearnings: true })).toBe("existing learnings found");
    expect(formatLearnStatus({ lastLearnRanAt: "2026-03-27T08:00:00Z" })).toBe("analyzed today");
    expect(formatLearnStatus({ lastLearnRanAt: "2026-03-26T08:00:00Z" })).toBe("analyzed yesterday");
    expect(formatLearnStatus({ lastLearnRanAt: "2026-03-22T08:00:00Z" })).toBe("analyzed 5 days ago");
  });
});
