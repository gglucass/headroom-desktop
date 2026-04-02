import { describe, expect, it } from "vitest";
import { cachePricingStatus } from "./pricing";
import type { HeadroomPricingStatus } from "./types";

function makePricingStatus(
  overrides: Partial<HeadroomPricingStatus> = {}
): HeadroomPricingStatus {
  return {
    authenticated: false,
    localGraceStartedAt: "2026-04-01T08:00:00Z",
    localGraceEndsAt: "2026-04-02T08:00:00Z",
    localGraceActive: true,
    accountSyncError: null,
    needsAuthentication: false,
    optimizationAllowed: true,
    shouldNudge: false,
    gateReason: null,
    gateMessage: "Headroom is active during your first local day.",
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
    ...overrides,
  };
}

describe("pricing cache snapshot", () => {
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
