import type {
  ClaudeAuthMethod,
  ClaudePlanTier,
  HeadroomPricingStatus,
  HeadroomSubscriptionTier,
} from "./types";

export interface CachedPricing {
  planTier?: ClaudePlanTier;
  recommendedSubscriptionTier?: HeadroomSubscriptionTier;
  subscriptionTier?: HeadroomSubscriptionTier;
}

export function claudePlanLabel(plan: ClaudePlanTier) {
  switch (plan) {
    case "free":
      return "Claude Free";
    case "pro":
      return "Claude Pro";
    case "max5x":
      return "Claude Max x5";
    case "max20x":
      return "Claude Max x20";
    default:
      return "Unknown Claude plan";
  }
}

export function subscriptionTierLabel(tier?: HeadroomSubscriptionTier | null) {
  switch (tier) {
    case "pro":
      return "Pro";
    case "max5x":
      return "Max x5";
    case "max20x":
      return "Max x20";
    default:
      return "No paid Headroom plan";
  }
}

export function authMethodLabel(method: ClaudeAuthMethod) {
  switch (method) {
    case "claude_ai_oauth":
      return "Claude AI OAuth";
    case "api_key":
      return "API key";
    default:
      return "Unknown";
  }
}

export function formatPercentValue(value?: number | null) {
  if (value == null || Number.isNaN(value)) {
    return "Unknown";
  }
  return `${value.toFixed(value % 1 === 0 ? 0 : 1)}%`;
}

export function formatRemainingDays(timestamp?: string | null) {
  if (!timestamp) {
    return null;
  }
  const target = new Date(timestamp).getTime();
  if (Number.isNaN(target)) {
    return null;
  }
  const diffMs = target - Date.now();
  const diffDays = Math.ceil(diffMs / 86_400_000);
  return diffDays;
}

export function pricingTone(status: HeadroomPricingStatus | null) {
  if (!status) {
    return "neutral";
  }
  if (!status.optimizationAllowed) {
    return "blocked";
  }
  if (status.needsAuthentication || status.shouldNudge) {
    return "warning";
  }
  if (status.account?.trialActive) {
    return "trial";
  }
  return "healthy";
}

export function cachePricingStatus(status: HeadroomPricingStatus): CachedPricing {
  return {
    planTier: status.claude.planTier,
    recommendedSubscriptionTier: status.recommendedSubscriptionTier ?? undefined,
    subscriptionTier: status.account?.subscriptionTier ?? undefined,
  };
}
