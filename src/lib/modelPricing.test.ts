import { describe, expect, it } from "vitest";
import { estimateCostSavingsUsd, formatEstimatedUsd } from "./modelPricing";

describe("estimateCostSavingsUsd", () => {
  it("returns null for zero or negative savings", () => {
    expect(estimateCostSavingsUsd("claude-sonnet-4-6", 0)).toBeNull();
    expect(estimateCostSavingsUsd("claude-sonnet-4-6", -5)).toBeNull();
    expect(estimateCostSavingsUsd("claude-sonnet-4-6", null)).toBeNull();
  });

  it("uses the fallback rate when the model is unknown or missing", () => {
    // Fallback is Sonnet-class: $3/M, so 1M tokens = $3.
    expect(estimateCostSavingsUsd(null, 1_000_000)).toBeCloseTo(3);
    expect(estimateCostSavingsUsd("mystery-model-9000", 1_000_000)).toBeCloseTo(3);
  });

  it("applies the right rate for each model family", () => {
    // Opus @ $15/M
    expect(estimateCostSavingsUsd("claude-opus-4-7", 1_000_000)).toBeCloseTo(15);
    // Sonnet @ $3/M
    expect(estimateCostSavingsUsd("claude-sonnet-4-6", 1_000_000)).toBeCloseTo(3);
    // Haiku 4 @ $1/M
    expect(estimateCostSavingsUsd("claude-haiku-4-5", 1_000_000)).toBeCloseTo(1);
    // GPT-4o mini is cheaper than GPT-4o — ensure mini matches first.
    expect(estimateCostSavingsUsd("gpt-4o-mini", 1_000_000)).toBeCloseTo(0.15);
    expect(estimateCostSavingsUsd("gpt-4o", 1_000_000)).toBeCloseTo(2.5);
    expect(estimateCostSavingsUsd("gemini-2.5-pro", 1_000_000)).toBeCloseTo(1.25);
    expect(estimateCostSavingsUsd("gemini-2.0-flash", 1_000_000)).toBeCloseTo(0.1);
  });
});

describe("formatEstimatedUsd", () => {
  it("formats values with decreasing precision so sub-cent values stay readable", () => {
    expect(formatEstimatedUsd(12.345)).toBe("~$12.35");
    expect(formatEstimatedUsd(1.2345)).toBe("~$1.23");
    expect(formatEstimatedUsd(0.1234)).toBe("~$0.123");
    expect(formatEstimatedUsd(0.0123)).toBe("~$0.012");
    expect(formatEstimatedUsd(0.00123)).toBe("~$0.0012");
    expect(formatEstimatedUsd(0.00001)).toBe("~<$0.0001");
  });
});
