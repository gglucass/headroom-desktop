import { beforeEach, describe, expect, it } from "vitest";

import type { HeadroomPricingStatus } from "./types";
import { takeUpgradeLanding } from "./upgradeLanding";

function status(over: Partial<HeadroomPricingStatus> = {}, account = {}): HeadroomPricingStatus {
  return {
    authenticated: true,
    optimizationAllowed: false,
    gateReason: "trial_ended",
    account: { trialActive: false, subscriptionActive: false, ...account },
    ...over
  } as HeadroomPricingStatus;
}

beforeEach(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => void values.set(key, value),
      removeItem: (key: string) => void values.delete(key)
    }
  });
});

describe("takeUpgradeLanding", () => {
  it("lands once after the trial ends, and again after a later wall", () => {
    expect(takeUpgradeLanding(status(), null)).toBe(true);
    expect(takeUpgradeLanding(status(), null)).toBe(false);
    // Subscribed, then lapsed: the next wall lands once more.
    expect(
      takeUpgradeLanding(status({ optimizationAllowed: true, gateReason: null }, { subscriptionActive: true }), null)
    ).toBe(false);
    expect(takeUpgradeLanding(status(), null)).toBe(true);
  });

  it("lands after any billing notification, never for other gates or actions", () => {
    const ok = status({ optimizationAllowed: true, gateReason: null }, { trialActive: true });
    expect(takeUpgradeLanding(ok, "billing")).toBe(true);
    expect(takeUpgradeLanding(ok, "signin")).toBe(false);
    expect(takeUpgradeLanding(ok, null)).toBe(false);
    expect(takeUpgradeLanding(status({ gateReason: "weekly_usage_limit_reached" }), null)).toBe(false);
  });

  it("does nothing before pricing loads or while signed out", () => {
    expect(takeUpgradeLanding(null, "billing")).toBe(false);
    expect(takeUpgradeLanding(status({ authenticated: false }), "billing")).toBe(false);
    // Still armed for the first signed-in open.
    expect(takeUpgradeLanding(status(), null)).toBe(true);
  });
});
