import { describe, expect, it } from "vitest";

import {
  describeInvokeError,
  getNextLowerUpgradePlanId,
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

  it("returns the next lower visible plan for paid subscriptions", () => {
    expect(getNextLowerUpgradePlanId("pro")).toBe("free");
    expect(getNextLowerUpgradePlanId("max5x")).toBe("pro");
    expect(getNextLowerUpgradePlanId("max20x")).toBe("max5x");
    expect(getNextLowerUpgradePlanId(null)).toBeNull();
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

  it("makes individual plan buttons relative to the active paid Headroom plan", () => {
    const result = getUpgradePlans("individual", "max20x", undefined, "pro", true);

    expect(result.featuredPlanId).toBe("pro");
    expect(result.plans.map((plan) => [plan.id, plan.ctaLabel])).toEqual([
      ["free", "Downgrade to Free plan"],
      ["pro", "Stay on Pro plan"],
      ["max5x", "Upgrade to Max x5"],
      ["max20x", "Upgrade to Max x20"],
    ]);
  });

  it("shows full annual prices when launch discount is inactive", () => {
    const result = getUpgradePlans("individual");

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["free", "$0"],
      ["pro", "$5"],
      ["max5x", "$20"],
      ["max20x", "$40"],
    ]);
  });

  it("shows discounted annual prices when launch discount is active", () => {
    const result = getUpgradePlans("individual", "free", undefined, undefined, undefined, true);

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["free", "$0"],
      ["pro", "$2.50"],
      ["max5x", "$10"],
      ["max20x", "$20"],
    ]);
  });

  it("shows full monthly prices when launch discount is inactive", () => {
    const result = getUpgradePlans("individual", "free", undefined, undefined, undefined, false, "monthly");

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["free", "$0"],
      ["pro", "$7.50"],
      ["max5x", "$30"],
      ["max20x", "$60"],
    ]);
  });

  it("shows discounted monthly prices when launch discount is active", () => {
    const result = getUpgradePlans("individual", "free", undefined, undefined, undefined, true, "monthly");

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["free", "$0"],
      ["pro", "$3.75"],
      ["max5x", "$15"],
      ["max20x", "$30"],
    ]);
  });

  describe("active plan purchase info", () => {
    const baseArgs = [
      "individual" as const,
      undefined,
      undefined,
      "pro" as const,
      true,
      false,
      "annual" as const,
    ] as const;

    function activePlan(result: ReturnType<typeof getUpgradePlans>) {
      return result.plans.find((p) => p.id === "pro");
    }

    it("omits purchase info when subscription amount is missing", () => {
      const result = getUpgradePlans(...baseArgs, null, "annual", "2026-12-01");
      expect(activePlan(result)?.purchaseInfo).toBeUndefined();
    });

    it("omits purchase info when renewal date is missing", () => {
      // 6000 cents = $5/mo * 12 months
      const result = getUpgradePlans(...baseArgs, 6000, "annual", null);
      expect(activePlan(result)?.purchaseInfo).toBeUndefined();
    });

    it("shows full renewal price when no discount is present", () => {
      const result = getUpgradePlans(...baseArgs, 6000, "annual", "2026-12-01");
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        paidPerMonthLabel: "$5",
        discountPct: 0,
      });
    });

    it("shows full renewal price for a once-off discount", () => {
      // 100% discount this period (0 cents), but "once" so renewal is full price
      const result = getUpgradePlans(...baseArgs, 0, "annual", "2026-04-16", "2025-04-16", "once");
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        paidPerMonthLabel: "$5",
        discountPct: 0,
      });
    });

    it("shows discounted renewal price for a forever discount", () => {
      // 3000 cents = $2.50/mo * 12 months (50% off)
      const result = getUpgradePlans(...baseArgs, 3000, "annual", "2026-12-01", "2025-12-01", "forever");
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        paidPerMonthLabel: "$2.50",
        discountPct: 50,
      });
    });

    it("shows discounted renewal price when repeating discount window has not expired", () => {
      // Started 2025-04-16, 12-month discount window → expires 2026-04-16
      // Renewal at 2026-01-01 is within window → discount applies
      const result = getUpgradePlans(...baseArgs, 3000, "annual", "2026-01-01", "2025-04-16", "repeating", 12);
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        paidPerMonthLabel: "$2.50",
        discountPct: 50,
      });
    });

    it("shows full renewal price when repeating discount window has expired", () => {
      // Started 2024-01-01, 12-month window → expired 2025-01-01
      // Renewal at 2026-04-01 is outside window → full price
      const result = getUpgradePlans(...baseArgs, 3000, "annual", "2026-04-01", "2024-01-01", "repeating", 12);
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        paidPerMonthLabel: "$5",
        discountPct: 0,
      });
    });

    it("shows full renewal price for repeating discount with missing window data", () => {
      // "repeating" but duration_in_months is null → treat as no discount at renewal
      const result = getUpgradePlans(...baseArgs, 3000, "annual", "2026-12-01", "2025-12-01", "repeating", null);
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        paidPerMonthLabel: "$5",
        discountPct: 0,
      });
    });
  });
});
