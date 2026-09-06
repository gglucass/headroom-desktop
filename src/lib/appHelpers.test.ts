import { afterEach, describe, expect, it } from "vitest";

import {
  authCodeSentMessage,
  buildInstallFailureMailto,
  buildSetupStallMailto,
  describeInvokeError,
  forgoneSavingsLabel,
  getNextLowerUpgradePlanId,
  getPlanRenewalPriceLabel,
  getUpgradePlans,
  higherSubscriptionTier,
  introPercentOff,
  introSaleBadgeLabel,
  isTierDowngrade,
  paybackLabel,
  scheduledPlanChange,
  recentDailySavingsUsd,
  setServerPlanPrices,
  upgradePlanIntentLabel,
} from "./appHelpers";
import type { ClientConnectorStatus, HeadroomAccountProfile, RuntimeStatus } from "./types";
import type { DailySavingsPoint, IntroOffer } from "./types";

const daily = (usd: number): DailySavingsPoint => ({
  date: "2026-06-01",
  estimatedSavingsUsd: usd,
  estimatedTokensSaved: 0,
  actualCostUsd: 0,
  totalTokensSent: 0,
});

describe("app helpers", () => {
  it("formats upgrade intent labels for paid plans only", () => {
    expect(upgradePlanIntentLabel("pro")).toBe("Pro");
    expect(upgradePlanIntentLabel("max5x")).toBe("Max x5");
    expect(upgradePlanIntentLabel("max20x")).toBe("Max x20");
    expect(upgradePlanIntentLabel("free")).toBeNull();
    expect(upgradePlanIntentLabel(null)).toBeNull();
  });

  it("averages savings over the trailing window only", () => {
    expect(recentDailySavingsUsd([])).toBe(0);
    // 9 days present, default window 7 -> mean of the last 7 ($2 each).
    const points = [daily(100), daily(100), ...Array(7).fill(daily(2))];
    expect(recentDailySavingsUsd(points)).toBe(2);
  });

  it("shows the payback anchor only at a genuine value-add (>= 2x)", () => {
    // Pro annual is $3/mo. $30/mo savings -> 10x.
    expect(paybackLabel(30, "pro", "annual")).toContain("10x");
    // Exactly 2x -> shown.
    expect(paybackLabel(6, "pro", "annual")).toContain("2x");
    // Floors, never overstates: 2.8x -> "2x", not "3x".
    expect(paybackLabel(8.4, "pro", "annual")).toContain("2x");
    expect(paybackLabel(8.4, "pro", "annual")).not.toContain("3x");
    // Under 2x (covers price but weak) -> null, so it never deters.
    expect(paybackLabel(5.7, "pro", "annual")).toBeNull();
    expect(paybackLabel(4.5, "pro", "annual")).toBeNull();
    // Below price -> null.
    expect(paybackLabel(2, "pro", "annual")).toBeNull();
    // No em dashes in user-facing copy.
    expect(paybackLabel(30, "pro", "annual")).not.toContain("—");
  });

  it("projects forgone savings until reset, suppressing trivial sums", () => {
    expect(forgoneSavingsLabel(10, 3)).toContain("$30.00");
    expect(forgoneSavingsLabel(0, 3)).toBeNull();
    expect(forgoneSavingsLabel(10, 0)).toBeNull();
    expect(forgoneSavingsLabel(0.1, 3)).toBeNull(); // $0.30 < $1 floor
  });

  it("extracts invoke errors from common shapes before falling back", () => {
    expect(describeInvokeError(new Error("network down"), "fallback")).toBe("network down");
    expect(describeInvokeError("permission denied", "fallback")).toBe("permission denied");
    expect(describeInvokeError({ message: "typed message" }, "fallback")).toBe("typed message");
    expect(describeInvokeError({ error: "nested error" }, "fallback")).toBe("nested error");
    expect(describeInvokeError({ message: "   " }, "fallback")).toBe("fallback");
    // 20 call sites in App.tsx lean on this, several on the billing path.
    expect(describeInvokeError(new Error(""), "fallback")).toBe("fallback");
    expect(describeInvokeError(null, "fallback")).toBe("fallback");
    expect(describeInvokeError(undefined, "fallback")).toBe("fallback");
  });

  it("returns the next lower visible plan for paid subscriptions", () => {
    expect(getNextLowerUpgradePlanId("pro")).toBeNull();
    expect(getNextLowerUpgradePlanId("max5x")).toBe("pro");
    expect(getNextLowerUpgradePlanId("max20x")).toBe("max5x");
    expect(getNextLowerUpgradePlanId(null)).toBeNull();
  });

  it("prioritizes the active individual subscription plan", () => {
    const result = getUpgradePlans("individual", "max20x");

    expect(result.featuredPlanId).toBe("max20x");
    expect(result.plans.map((plan) => plan.id)).toEqual([
      "max20x",
      "pro",
      "max5x",
    ]);
  });

  it("uses recommended subscription order when no active plan exists", () => {
    const result = getUpgradePlans("individual", "free", "max5x");

    expect(result.featuredPlanId).toBe("max5x");
    expect(result.plans.map((plan) => plan.id)).toEqual([
      "max5x",
      "pro",
      "max20x",
    ]);
  });

  it("pitches the higher of the Claude-implied tier and the recommendation", () => {
    // Claude Max x5 routed next to ChatGPT Pro (Codex -> Max x20).
    expect(getUpgradePlans("individual", "max5x", "max20x").featuredPlanId).toBe("max20x");
    // Claude Max x20 routed next to ChatGPT Pro Lite (Codex -> Max x5).
    const result = getUpgradePlans("individual", "max20x", "max5x");
    expect(result.featuredPlanId).toBe("max20x");
    expect(result.plans.map((plan) => plan.id)).toEqual(["max20x", "pro", "max5x"]);
    // Lapsed subscriber with neither signal keeps their last paid tier.
    expect(getUpgradePlans("individual", "unknown", null, "pro").featuredPlanId).toBe("pro");

    expect(higherSubscriptionTier(undefined, "max5x")).toBe("max5x");
    expect(higherSubscriptionTier("pro", null)).toBe("pro");
    expect(higherSubscriptionTier("max5x", "pro")).toBe("max5x");
    expect(higherSubscriptionTier(null, undefined)).toBeNull();
  });

  it("defaults unknown individual plans toward max x5 guidance", () => {
    const result = getUpgradePlans("individual", "unknown");

    expect(result.featuredPlanId).toBe("max5x");
    expect(result.plans.map((plan) => plan.id)).toEqual([
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
      ["pro", "Stay on Pro plan"],
      ["max5x", "Upgrade to Max x5"],
      ["max20x", "Upgrade to Max x20"],
    ]);
  });

  it("marks the active plan only on the billing period it was bought on", () => {
    const forPeriod = (billingPeriod: "monthly" | "annual") =>
      getUpgradePlans(
        "individual", "pro", undefined, "pro", true, false, billingPeriod,
        2000, "monthly", "2026-09-01T00:00:00Z"
      ).plans.find((plan) => plan.id === "pro")!;

    const onMonthly = forPeriod("monthly");
    expect(onMonthly.ctaLabel).toBe("Stay on Pro plan");
    expect(onMonthly.purchaseInfo).toBeDefined();

    const onAnnual = forPeriod("annual");
    expect(onAnnual.ctaLabel).toBe("Switch to annual billing");
    expect(onAnnual.purchaseInfo).toBeUndefined();
  });

  it("prices cards as AppSumo one-time buys while the deal routes there", () => {
    const result = getUpgradePlans(
      "individual", "max20x", undefined, "pro", true, false, "annual",
      null, "lifetime", undefined, undefined, undefined, undefined, false,
      undefined, 0, null, undefined, undefined, "appsumo"
    );

    expect(result.plans.map((p) => [p.id, p.centeredPriceLabel, p.ctaLabel])).toEqual([
      ["pro", "lifetime plan • via AppSumo", "Stay on Pro plan"],
      ["max5x", "$99 one-time • on AppSumo", "Upgrade on AppSumo"],
      ["max20x", "$199 one-time • on AppSumo", "Upgrade on AppSumo"],
    ]);
  });

  describe("server-driven prices", () => {
    afterEach(() => setServerPlanPrices(null));

    it("quotes the server's prices over the compiled-in table", () => {
      setServerPlanPrices({
        pro: { annual: 500, monthly: 700 },
        max5x: { annual: 2000, monthly: 2500 },
        max20x: { annual: 4000, monthly: 5000 },
      });

      expect(getUpgradePlans("individual").plans.map((p) => [p.id, p.price])).toEqual([
        ["max5x", "$20"],
        ["pro", "$5"],
        ["max20x", "$40"],
      ]);
      // Every price consumer follows, not just the plan cards.
      expect(getPlanRenewalPriceLabel("max5x", "monthly")).toBe("$25 / month");
      expect(paybackLabel(10, "pro", "annual")).toContain("2x");
    });

    it("falls back per tier when the server omits or corrupts a price", () => {
      setServerPlanPrices({
        max5x: { annual: 2000, monthly: 2500 },
        // pro absent entirely; max20x annual unusable.
        max20x: { annual: Number.NaN, monthly: 5000 },
      });

      expect(getUpgradePlans("individual").plans.map((p) => [p.id, p.price])).toEqual([
        ["max5x", "$20"],
        ["pro", "$3"],
        ["max20x", "$30"],
      ]);
    });

    it("returns to the compiled-in table when the server sends nothing", () => {
      setServerPlanPrices({ pro: { annual: 500, monthly: 700 } });
      setServerPlanPrices(undefined);

      expect(getUpgradePlans("individual").plans.map((p) => [p.id, p.price])).toEqual([
        ["max5x", "$15"],
        ["pro", "$3"],
        ["max20x", "$30"],
      ]);
    });
  });

  it("shows full annual prices when launch discount is inactive", () => {
    const result = getUpgradePlans("individual");

    expect(result.featuredPlanId).toBe("max5x");
    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["max5x", "$15"],
      ["pro", "$3"],
      ["max20x", "$30"],
    ]);
  });

  it("shows discounted annual prices when launch discount is active", () => {
    const result = getUpgradePlans("individual", "free", undefined, undefined, undefined, true);

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["max5x", "$7.50"],
      ["pro", "$1.50"],
      ["max20x", "$15"],
    ]);
  });

  it("shows full monthly prices when launch discount is inactive", () => {
    const result = getUpgradePlans("individual", "free", undefined, undefined, undefined, false, "monthly");

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["max5x", "$20"],
      ["pro", "$4"],
      ["max20x", "$40"],
    ]);
  });

  it("shows discounted monthly prices when launch discount is active", () => {
    const result = getUpgradePlans("individual", "free", undefined, undefined, undefined, true, "monthly");

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["max5x", "$10"],
      ["pro", "$2"],
      ["max20x", "$20"],
    ]);
  });

  it("shows list prices on upgrade-target cards for a subscriber with no carried-over discount", () => {
    // change_plan swaps the Polar product and attaches no new discount, so an
    // existing subscriber is quoted list price regardless of the intro offer.
    const result = getUpgradePlans("individual", "max20x", undefined, "pro", true, true, "annual", 300);

    const byId = (id: string) => result.plans.find((plan) => plan.id === id);
    expect(byId("pro")?.price).toBe("$3");
    expect(byId("pro")?.originalPrice).toBeUndefined();
    expect([byId("max5x")?.price, byId("max5x")?.originalPrice]).toEqual(["$15", undefined]);
    expect([byId("max20x")?.price, byId("max20x")?.originalPrice]).toEqual(["$30", undefined]);
  });

  it("carries a subscriber's own surviving discount onto upgrade-target cards", () => {
    // Paying $1.50/mo on the $3 pro plan under a forever discount -> 50% off
    // survives the swap, so the other cards quote 50% off too.
    const result = getUpgradePlans(
      "individual", "max20x", undefined, "pro", true, false, "annual",
      1800, "annual", "2027-03-31T00:00:00Z", "2026-03-31T00:00:00Z", "forever"
    );

    const byId = (id: string) => result.plans.find((plan) => plan.id === id);
    expect([byId("max5x")?.price, byId("max5x")?.originalPrice]).toEqual(["$7.50", "$15"]);
    expect([byId("max20x")?.price, byId("max20x")?.originalPrice]).toEqual(["$15", "$30"]);
  });

  it("drives discounted annual prices from the active cohort percent", () => {
    // 25% off the early cohort: $3 -> $2.25, $15 -> $11.25, $30 -> $22.50.
    const result = getUpgradePlans(
      "individual", "free", undefined, undefined, undefined, true, "annual",
      undefined, undefined, undefined, undefined, undefined, undefined, false, undefined, 25
    );

    expect(result.plans.map((plan) => [plan.id, plan.price])).toEqual([
      ["max5x", "$11.25"],
      ["pro", "$2.25"],
      ["max20x", "$22.50"],
    ]);
  });

  describe("intro offer", () => {
    const intro: IntroOffer = { active: true, percentOff: 50, durationMonths: 3 };

    it("returns the straight percent while the offer runs", () => {
      expect(introPercentOff(intro)).toBe(50);
      expect(introPercentOff({ ...intro, active: false })).toBe(0);
      expect(introPercentOff(null)).toBe(0);
    });

    it("labels the sale badge with the offer duration", () => {
      expect(introSaleBadgeLabel(intro)).toBe("50% off first 3 months");
      expect(introSaleBadgeLabel(null)).toBeNull();
    });

    it("drives discounted monthly prices from the intro offer", () => {
      const result = getUpgradePlans(
        "individual", "free", undefined, undefined, undefined, false, "monthly",
        undefined, undefined, undefined, undefined, undefined, undefined, false, undefined, 0,
        intro
      );

      expect(result.plans.map((plan) => [plan.id, plan.price, plan.originalPrice])).toEqual([
        ["max5x", "$10", "$20"],
        ["pro", "$2", "$4"],
        ["max20x", "$20", "$40"],
      ]);
    });

    it("drives discounted annual prices at the straight percent off the sticker", () => {
      const result = getUpgradePlans(
        "individual", "free", undefined, undefined, undefined, false, "annual",
        undefined, undefined, undefined, undefined, undefined, undefined, false, undefined, 0,
        intro
      );

      expect(result.plans.map((plan) => [plan.id, plan.price, plan.originalPrice])).toEqual([
        ["max5x", "$7.50", "$15"],
        ["pro", "$1.50", "$3"],
        ["max20x", "$15", "$30"],
      ]);
    });

    it("spells out the reversion under the price on intro cards", () => {
      const result = getUpgradePlans(
        "individual", "free", undefined, undefined, undefined, false, "annual",
        undefined, undefined, undefined, undefined, undefined, undefined, false, undefined, 0,
        intro
      );

      const max5x = result.plans.find((p) => p.id === "max5x");
      expect(max5x?.billingLines).toEqual(["USD / month", "billed annually"]);
      expect(max5x?.reversionLine).toBe("then $15/mo after 3 months");
    });

    it("lets an account forever discount win over the intro offer", () => {
      // Subscriber pays $1.50/mo (50% off pro annual, forever). Intro also on.
      const result = getUpgradePlans(
        "individual", undefined, undefined, "pro", true, false, "annual",
        1800, "annual", "2026-12-01", "2025-12-01", "forever", null, false, undefined, 0,
        intro
      );
      const max5x = result.plans.find((p) => p.id === "max5x");
      // The account's own 50% beats the intro offer's 50%: $15 -> $7.50 either way.
      expect(max5x?.price).toBe("$7.50");
    });
  });

  it("classifies tier direction for plan changes", () => {
    expect(isTierDowngrade("pro", "max20x")).toBe(false);
    expect(isTierDowngrade("max20x", "pro")).toBe(true);
    expect(isTierDowngrade("max5x", "max20x")).toBe(false);
    expect(isTierDowngrade("max20x", "max5x")).toBe(true);
  });

  describe("getPlanRenewalPriceLabel", () => {
    it("returns the standard per-month price when no current paid amount is given", () => {
      // Max x5 annual is $15 / month (billed annually).
      expect(getPlanRenewalPriceLabel("max5x", "annual")).toBe("$15 / month");
      expect(getPlanRenewalPriceLabel("max5x", "monthly")).toBe("$20 / month");
    });

    it("carries the user's current discount ratio forward to the target plan", () => {
      // 100% off Pro annual (paid $0 vs $36/year list) -> 100% off Max x20.
      expect(
        getPlanRenewalPriceLabel("max20x", "annual", { fromTier: "pro", currentPaidCents: 0 })
      ).toBe("$0 / month");
      // 50% off Pro annual (paid $18/year = 1800 cents per cycle vs $36 list)
      // -> 50% off Max x5 annual: $15 / month list -> $7.50 / month.
      expect(
        getPlanRenewalPriceLabel("max5x", "annual", { fromTier: "pro", currentPaidCents: 1800 })
      ).toBe("$7.50 / month");
      // 50% off monthly cycle (paid $2 vs $4 list per month) -> 50% off Max x5
      // monthly: $20 / month list -> $10 / month.
      expect(
        getPlanRenewalPriceLabel("max5x", "monthly", { fromTier: "pro", currentPaidCents: 200 })
      ).toBe("$10 / month");
    });
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
      // 3600 cents = $3/mo * 12 months
      const result = getUpgradePlans(...baseArgs, 3600, "annual", null);
      expect(activePlan(result)?.purchaseInfo).toBeUndefined();
    });

    it("shows full renewal price when no discount is present", () => {
      const result = getUpgradePlans(...baseArgs, 3600, "annual", "2026-12-01");
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$36/yr",
        discountPct: 0,
      });
    });

    it("quotes a monthly subscription per month", () => {
      const result = getUpgradePlans(
        "individual", undefined, undefined, "pro", true, false, "monthly",
        300, "monthly", "2026-12-01"
      );
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$4/mo",
        discountPct: 0,
      });
    });

    it("shows full renewal price for a once-off discount", () => {
      // 100% discount this period (0 cents), but "once" so renewal is full price
      const result = getUpgradePlans(...baseArgs, 0, "annual", "2026-04-16", "2025-04-16", "once");
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$36/yr",
        discountPct: 0,
      });
    });

    it("shows discounted renewal price for a forever discount", () => {
      // 1800 cents = $1.50/mo * 12 months (50% off)
      const result = getUpgradePlans(...baseArgs, 1800, "annual", "2026-12-01", "2025-12-01", "forever");
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$18/yr",
        discountPct: 50,
      });
    });

    it("applies the account forever discount to upgrade-target cards without launch promo", () => {
      // 1800 cents = $1.50/mo (50% off pro). Launch discount inactive.
      const result = getUpgradePlans(...baseArgs, 1800, "annual", "2026-12-01", "2025-12-01", "forever");
      const max5x = result.plans.find((p) => p.id === "max5x");
      expect(max5x?.originalPrice).toBeDefined();
      expect(max5x?.price).not.toBe(max5x?.originalPrice);
    });

    it("prices target cards off the exact ratio, not the rounded percent", () => {
      // Exactly a third off pro annual (2400 = $2/mo * 12). Rounding to "33% off"
      // and re-applying it to max5x quotes $10.05; the ratio quotes $10.
      const result = getUpgradePlans(...baseArgs, 2400, "annual", "2026-12-01", "2025-12-01", "forever");
      const max5x = result.plans.find((p) => p.id === "max5x");
      expect(max5x?.price).toBe("$10");
      expect(max5x?.originalPrice).toBe("$15");
      expect(max5x?.saleBadge).toBe("33% off forever");
    });

    it("states the carried discount once: renewal line on the bought period, badge on the other", () => {
      const args = ["individual", undefined, undefined, "pro", true, false] as const;
      const onBoughtPeriod = getUpgradePlans(...args, "annual", 1800, "annual", "2026-12-01", "2025-12-01", "forever");
      const boughtCard = onBoughtPeriod.plans.find((p) => p.id === "pro");
      expect(boughtCard?.saleBadge).toBeUndefined();
      expect(boughtCard?.purchaseInfo?.renewalNote).toBe("50% off forever");

      const onOtherPeriod = getUpgradePlans(...args, "monthly", 1800, "annual", "2026-12-01", "2025-12-01", "forever");
      const switchCard = onOtherPeriod.plans.find((p) => p.id === "pro");
      expect(switchCard?.price).toBe("$2");
      expect(switchCard?.originalPrice).toBe("$4");
      expect(switchCard?.saleBadge).toBe("50% off forever");
      expect(switchCard?.purchaseInfo).toBeUndefined();
    });

    it("does not discount upgrade-target cards for a once-off discount", () => {
      const result = getUpgradePlans(...baseArgs, 1800, "annual", "2026-12-01", "2025-12-01", "once");
      const max5x = result.plans.find((p) => p.id === "max5x");
      expect(max5x?.originalPrice).toBeUndefined();
    });

    it("shows discounted renewal price when repeating discount window has not expired", () => {
      // Started 2025-04-16, 12-month discount window → expires 2026-04-16
      // Renewal at 2026-01-01 is within window → discount applies
      const result = getUpgradePlans(...baseArgs, 1800, "annual", "2026-01-01", "2025-04-16", "repeating", 12);
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$18/yr",
        discountPct: 50,
      });
    });

    it("shows full renewal price when repeating discount window has expired", () => {
      // Started 2024-01-01, 12-month window → expired 2025-01-01
      // Renewal at 2026-04-01 is outside window → full price
      const result = getUpgradePlans(...baseArgs, 1800, "annual", "2026-04-01", "2024-01-01", "repeating", 12);
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$36/yr",
        discountPct: 0,
      });
    });

    it("prefers the server's renewal price over the discount-window guess", () => {
      // The save offer attaches a fresh 12-month discount to a subscription that
      // started 2 years ago, so the window check reads it as long expired. The
      // server's own figure ($1/mo * 12 = 1200) has to win anyway.
      const result = getUpgradePlans(
        ...baseArgs, 1800, "annual", "2026-12-01", "2024-01-01", "repeating", 12,
        false, null, 0, null, 1200, "2027-12-01"
      );
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$12/yr",
        discountPct: 67,
      });
    });

    it("ignores the server's renewal price once its window has passed", () => {
      const result = getUpgradePlans(
        ...baseArgs, 1800, "annual", "2026-12-01", "2025-12-01", "forever",
        null, false, null, 0, null, 1200, "2020-01-01"
      );
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$18/yr",
        discountPct: 50,
      });
    });

    it("spells out how long the discount runs", () => {
      const forever = getUpgradePlans(...baseArgs, 1800, "annual", "2026-12-01", "2025-12-01", "forever");
      expect(activePlan(forever)?.purchaseInfo?.renewalNote).toBe("50% off forever");

      const repeating = getUpgradePlans(...baseArgs, 1800, "annual", "2026-01-01", "2025-04-16", "repeating", 12);
      expect(activePlan(repeating)?.purchaseInfo?.renewalNote).toBe("50% off for 12 months");

      // A server-supplied renewal figure (subscriptionRenewalCents) wins over
      // the local window check when the discount is dated too far back for it.
      const serverFigure = getUpgradePlans(
        ...baseArgs, 1800, "annual", "2026-12-01", "2024-01-01", "repeating", 12,
        false, null, 0, null, 1200, "2027-12-01"
      );
      expect(activePlan(serverFigure)?.purchaseInfo?.renewalNote).toBe("67% off for 12 months");

      const none = getUpgradePlans(...baseArgs, 3600, "annual", "2026-12-01");
      expect(activePlan(none)?.purchaseInfo?.renewalNote).toBeUndefined();
    });

    it("names today's rate when it is not the renewal rate", () => {
      // An annual discount repeating for 12 months covers exactly one invoice,
      // so the renewal on the window boundary bills full. Without the note the
      // card states only $3 while the billing portal shows the $2.60 in force.
      const result = getUpgradePlans(
        ...baseArgs, 3120, "annual", "2027-03-31T20:31:45Z", "2026-03-31T20:31:45Z", "repeating", 12
      );
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$36/yr",
        discountPct: 0,
        renewalNote: "$31.20/yr until then",
      });
    });

    it("shows full renewal price for repeating discount with missing window data", () => {
      // "repeating" but duration_in_months is null → treat as no discount at renewal
      const result = getUpgradePlans(...baseArgs, 1800, "annual", "2026-12-01", "2025-12-01", "repeating", null);
      expect(activePlan(result)?.purchaseInfo).toMatchObject({
        renewalPriceLabel: "$36/yr",
        discountPct: 0,
      });
    });
  });

  describe("scheduled downgrade", () => {
    const baseArgs = [
      "individual" as const,
      undefined,
      undefined,
      "max20x" as const,
      true,
      false,
      "annual" as const,
    ] as const;

    function planById(result: ReturnType<typeof getUpgradePlans>, id: string) {
      return result.plans.find((p) => p.id === id);
    }

    it("stamps cancel info on the active plan card when downgrade is scheduled", () => {
      const result = getUpgradePlans(
        ...baseArgs,
        12000, // $10/mo annual
        "annual",
        "2027-03-31",
        "2026-03-31",
        null,
        null,
        true,
        "2027-03-31T20:31:45Z"
      );
      const active = planById(result, "max20x");
      expect(active?.purchaseInfo?.cancelAtPeriodEnd).toBe(true);
      expect(active?.purchaseInfo?.endsOn).toBeDefined();
    });

    it("leaves the active plan card untouched when no downgrade is scheduled", () => {
      const result = getUpgradePlans(
        ...baseArgs,
        12000,
        "annual",
        "2027-03-31",
        "2026-03-31",
        null,
        null,
        false,
        null
      );
      const active = planById(result, "max20x");
      expect(active?.purchaseInfo?.cancelAtPeriodEnd).toBe(false);
      expect(active?.purchaseInfo?.endsOn).toBeUndefined();
    });
  });
});

describe("buildSetupStallMailto", () => {
  const context = {
    appVersion: "0.7.7-rc.6",
    lifetimeRequests: 12,
    runtime: {
      installed: true,
      running: true,
      paused: false,
      proxyReachable: false,
    } as unknown as RuntimeStatus,
    connectors: [
      {
        clientId: "claude_code",
        name: "Claude Code",
        installed: true,
        enabled: true,
        verified: false,
      },
    ] as ClientConnectorStatus[],
  };

  function decodedBody(url: string): string {
    return decodeURIComponent(url.split("&body=")[1] ?? "");
  }

  it("addresses support and names the branch in the subject", () => {
    const url = buildSetupStallMailto("no_savings", context);

    expect(url.startsWith("mailto:support@extraheadroom.com?subject=")).toBe(true);
    expect(decodeURIComponent(url.split("?subject=")[1].split("&body=")[0])).toBe(
      "Headroom is not saving anything (no_savings)"
    );
  });

  // The point of the escape hatch is that a reply doesn't open with "what
  // version are you on, is it running, what's connected?".
  it("carries the state that decided which branch fired", () => {
    const body = decodedBody(buildSetupStallMailto("no_traffic", context));

    expect(body).toContain("Alert: no_traffic");
    expect(body).toContain("App version: 0.7.7-rc.6");
    expect(body).toContain("Lifetime requests seen: 12");
    expect(body).toContain("proxyReachable=false");
    expect(body).toContain("Claude Code: installed=true enabled=true verified=false");
  });

  it("survives a missing runtime and an empty connector list", () => {
    const body = decodedBody(
      buildSetupStallMailto("no_traffic", { ...context, runtime: null, connectors: [] })
    );

    expect(body).toContain("installed=unknown");
    expect(body).toContain("(none reported)");
  });
});

describe("buildInstallFailureMailto", () => {
  it("carries the failure kind and pip's stderr tail, not just our own copy", () => {
    const url = buildInstallFailureMailto({
      kind: "unsupported_pin",
      detail: "exit=1; stderr tail: No matching distribution found for onnxruntime==1.27.0",
      appVersion: "0.8.1",
      platform: "macos",
    });
    expect(url.startsWith("mailto:support@extraheadroom.com?subject=")).toBe(true);
    const decoded = decodeURIComponent(url);
    expect(decoded).toContain("unsupported_pin");
    expect(decoded).toContain("onnxruntime==1.27.0");
    expect(decoded).toContain("0.8.1");
    expect(decoded).toContain("macos");
  });

  it("stays sendable when no report was captured", () => {
    const decoded = decodeURIComponent(
      buildInstallFailureMailto({
        kind: null,
        detail: null,
        appVersion: "0.8.4",
        platform: "windows",
      })
    );
    expect(decoded).toContain("unknown");
    expect(decoded).toContain("(none captured)");
  });
});

describe("scheduledPlanChange", () => {
  const account = (over: Partial<HeadroomAccountProfile> = {}): HeadroomAccountProfile => ({
    email: "dev@example.com",
    trialActive: false,
    subscriptionActive: true,
    subscriptionTier: "pro",
    acceptedInvitesCount: 0,
    inviteBonusPercent: 0,
    ...over,
  });

  // TZ-independent: the runner's local rendering of the same instant.
  const on = (iso: string) =>
    new Date(iso).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });

  it("calls a same-tier change a billing switch, not a plan change", () => {
    const info = scheduledPlanChange(
      account({
        subscriptionTier: "pro",
        subscriptionPendingTier: "pro",
        subscriptionPendingBillingPeriod: "monthly",
        subscriptionPendingEffectiveAt: "2026-09-01T12:00:00Z",
      })
    );
    expect(info).toEqual({
      tier: "pro",
      billingPeriod: "monthly",
      note: `Switches to monthly billing on ${on("2026-09-01T12:00:00Z")}`,
    });
  });

  it("names the incoming plan when the tier actually changes", () => {
    const info = scheduledPlanChange(
      account({
        subscriptionTier: "max20x",
        subscriptionPendingTier: "max5x",
        subscriptionPendingBillingPeriod: "annual",
        subscriptionPendingEffectiveAt: "2026-09-01T12:00:00Z",
      })
    );
    expect(info?.tier).toBe("max5x");
    expect(info?.note).toBe(`Switches to Max x5 (annual) on ${on("2026-09-01T12:00:00Z")}`);
  });

  it("defaults an unknown billing period to annual", () => {
    const info = scheduledPlanChange(
      account({
        subscriptionPendingTier: "max5x",
        subscriptionPendingBillingPeriod: null,
        subscriptionPendingEffectiveAt: "2026-09-01T12:00:00Z",
      })
    );
    expect(info?.billingPeriod).toBe("annual");
  });

  it("reports nothing without both a tier and a usable date", () => {
    expect(scheduledPlanChange(null)).toBeNull();
    expect(scheduledPlanChange(undefined)).toBeNull();
    expect(scheduledPlanChange(account())).toBeNull();
    expect(
      scheduledPlanChange(account({ subscriptionPendingTier: "max5x" }))
    ).toBeNull();
    expect(
      scheduledPlanChange(account({ subscriptionPendingEffectiveAt: "2026-09-01T12:00:00Z" }))
    ).toBeNull();
    // An unparseable date must not reach the billing screen as "Invalid Date".
    expect(
      scheduledPlanChange(
        account({ subscriptionPendingTier: "max5x", subscriptionPendingEffectiveAt: "soon" })
      )
    ).toBeNull();
  });
});

describe("authCodeSentMessage", () => {
  it("states the expiry and that a resend invalidates the older code", () => {
    const message = authCodeSentMessage("dev@example.com", 900);
    expect(message).toContain("dev@example.com");
    expect(message).toContain("15 minutes");
    expect(message).toContain("only the newest code works");
  });

  it("never rounds a short expiry down to zero minutes", () => {
    expect(authCodeSentMessage("dev@example.com", 20)).toContain("1 minute.");
  });
});
