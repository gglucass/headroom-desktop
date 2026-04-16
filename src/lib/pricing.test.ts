import { afterEach, describe, expect, it, vi } from "vitest";
import {
  authMethodLabel,
  cachePricingStatus,
  claudePlanLabel,
  formatPercentValue,
  formatRemainingDays,
  pricingTone,
  subscriptionTierLabel,
} from "./pricing";
import type { HeadroomPricingStatus } from "./types";

function makePricingStatus(
  overrides: Partial<HeadroomPricingStatus> = {}
): HeadroomPricingStatus {
  return {
    authenticated: false,
    localGraceStartedAt: "2026-04-01T08:00:00Z",
    localGraceEndsAt: "2026-04-04T08:00:00Z",
    localGraceActive: true,
    accountSyncError: null,
    needsAuthentication: false,
    optimizationAllowed: true,
    shouldNudge: false,
    gateReason: null,
    gateMessage: "Headroom is active during your first 72 hours.",
    nudgeThresholdPercent: null,
    disableThresholdPercent: null,
    effectiveDisableThresholdPercent: null,
    recommendedSubscriptionTier: null,
    recommendedSubscriptionPriceUsd: null,
    claude: {
      authMethod: "unknown",
      email: null,
      displayName: null,
      accountUuid: null,
      organizationUuid: null,
      billingType: null,
      accountCreatedAt: null,
      subscriptionCreatedAt: null,
      hasExtraUsageEnabled: false,
      planTier: "free",
      planDetectionSource: null,
      weeklyUtilizationPct: null,
      fiveHourUtilizationPct: null,
      extraUsageMonthlyLimit: null,
    },
    account: null,
    launchDiscountActive: false,
    ...overrides,
  };
}

describe("pricing cache snapshot", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("formats plan and auth labels for known and unknown states", () => {
    expect(claudePlanLabel("free")).toBe("Claude Free");
    expect(claudePlanLabel("pro")).toBe("Claude Pro");
    expect(claudePlanLabel("max5x")).toBe("Claude Max x5");
    expect(claudePlanLabel("max20x")).toBe("Claude Max x20");
    expect(claudePlanLabel("unknown")).toBe("Unknown Claude plan");

    expect(subscriptionTierLabel("pro")).toBe("Pro");
    expect(subscriptionTierLabel("max5x")).toBe("Max x5");
    expect(subscriptionTierLabel("max20x")).toBe("Max x20");
    expect(subscriptionTierLabel(null)).toBe("No paid Headroom plan");

    expect(authMethodLabel("claude_ai_oauth")).toBe("Claude AI OAuth");
    expect(authMethodLabel("api_key")).toBe("API key");
    expect(authMethodLabel("unknown")).toBe("Unknown");
  });

  it("formats percentages and remaining days with stable fallbacks", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-04-02T12:00:00Z"));

    expect(formatPercentValue(undefined)).toBe("Unknown");
    expect(formatPercentValue(Number.NaN)).toBe("Unknown");
    expect(formatPercentValue(18)).toBe("18%");
    expect(formatPercentValue(18.25)).toBe("18.3%");

    expect(formatRemainingDays(null)).toBeNull();
    expect(formatRemainingDays("not-a-date")).toBeNull();
    expect(formatRemainingDays("2026-04-03T12:00:00Z")).toBe(1);
    expect(formatRemainingDays("2026-04-03T12:00:01Z")).toBe(2);
  });

  it("derives pricing tone from access, nudges, and trial state", () => {
    expect(pricingTone(null)).toBe("neutral");
    expect(pricingTone(makePricingStatus({ optimizationAllowed: false }))).toBe("blocked");
    expect(pricingTone(makePricingStatus({ needsAuthentication: true }))).toBe("warning");
    expect(pricingTone(makePricingStatus({ shouldNudge: true }))).toBe("warning");
    expect(
      pricingTone(
        makePricingStatus({
          account: {
            email: "hello@example.com",
            trialStartedAt: "2026-04-01T08:00:00Z",
            trialEndsAt: "2026-04-08T08:00:00Z",
            trialActive: true,
            subscriptionActive: false,
            subscriptionTier: null,
            inviteCode: null,
            acceptedInvitesCount: 0,
            inviteBonusPercent: 0,
          },
        })
      )
    ).toBe("trial");
    expect(pricingTone(makePricingStatus())).toBe("healthy");
  });

  it("does not crash when pricing status is signed out", () => {
    expect(cachePricingStatus(makePricingStatus())).toEqual({
      planTier: "free",
      recommendedSubscriptionTier: undefined,
      subscriptionTier: undefined,
    });
  });

  it("preserves subscription tier when account data is present", () => {
    expect(
      cachePricingStatus(
        makePricingStatus({
          recommendedSubscriptionTier: "max5x",
          account: {
            email: "hello@example.com",
            trialStartedAt: null,
            trialEndsAt: null,
            trialActive: false,
            subscriptionActive: true,
            subscriptionTier: "pro",
            inviteCode: null,
            acceptedInvitesCount: 0,
            inviteBonusPercent: 0,
          },
        })
      )
    ).toEqual({
      planTier: "free",
      recommendedSubscriptionTier: "max5x",
      subscriptionTier: "pro",
    });
  });
});
