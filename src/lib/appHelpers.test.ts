import { describe, expect, it } from "vitest";

import {
  describeInvokeError,
  getUpgradePlans,
  upgradePlanIntentLabel,
} from "./appHelpers";

describe("app helpers", () => {
  it("formats upgrade intent labels for paid plans only", () => {
    expect(upgradePlanIntentLabel("pro")).toBe("Pro");
    expect(upgradePlanIntentLabel("max5x")).toBe("Max x5");
    expect(upgradePlanIntentLabel("max20x")).toBe("Max x20");
    expect(upgradePlanIntentLabel("free")).toBeNull();
    expect(upgradePlanIntentLabel(null)).toBeNull();
  });

  it("extracts invoke errors from common shapes before falling back", () => {
    expect(describeInvokeError(new Error("network down"), "fallback")).toBe("network down");
    expect(describeInvokeError("permission denied", "fallback")).toBe("permission denied");
    expect(describeInvokeError({ message: "typed message" }, "fallback")).toBe("typed message");
    expect(describeInvokeError({ error: "nested error" }, "fallback")).toBe("nested error");
    expect(describeInvokeError({ message: "   " }, "fallback")).toBe("fallback");
  });

  it("prioritizes the active individual subscription plan", () => {
    const result = getUpgradePlans("individual", "max20x");

    expect(result.featuredPlanId).toBe("max20x");
    expect(result.plans.map((plan) => plan.id)).toEqual([
      "free",
      "max20x",
      "pro",
      "max5x",
    ]);
  });

  it("uses recommended subscription order when no active plan exists", () => {
    const result = getUpgradePlans("individual", "free", "max5x");

    expect(result.featuredPlanId).toBe("max5x");
    expect(result.plans.map((plan) => plan.id)).toEqual([
      "free",
      "max5x",
      "pro",
      "max20x",
    ]);
  });

  it("defaults unknown individual plans toward max x5 guidance", () => {
    const result = getUpgradePlans("individual", "unknown");

    expect(result.featuredPlanId).toBe("max5x");
    expect(result.plans.map((plan) => plan.id)).toEqual([
      "free",
      "max5x",
      "pro",
      "max20x",
    ]);
  });

  it("returns the enterprise contact card for team audiences", () => {
    const result = getUpgradePlans("teamEnterprise");

    expect(result.featuredPlanId).toBe("enterprise");
    expect(result.plans).toHaveLength(1);
    expect(result.plans[0]).toMatchObject({
      id: "enterprise",
      ctaLabel: "Submit",
    });
  });
});
