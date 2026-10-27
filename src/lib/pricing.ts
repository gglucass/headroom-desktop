import type {
  ClaudeAuthMethod,
  ClaudePlanTier,
  CodexPlanTier,
  HeadroomPricingStatus,
  HeadroomSubscriptionTier,
} from "./types";

export interface CachedPricing {
  planTier?: ClaudePlanTier;
  recommendedSubscriptionTier?: HeadroomSubscriptionTier;
  subscriptionTier?: HeadroomSubscriptionTier;
  codexPlanTier?: CodexPlanTier;
}

/// The ChatGPT plan this status names, else the last one remembered. The
/// status only knows it from Codex's auth.json or a live Codex request, so a
/// keychain-stored Codex login reads "unknown" after every restart until Codex
/// sends one; "unknown" is no evidence and never replaces a known plan.
export function resolvedCodexPlanTier(
  status: HeadroomPricingStatus | null | undefined,
  cached: CachedPricing
): CodexPlanTier | undefined {
  const live = status?.codexPlanTier;
  return live && live !== "unknown" ? live : cached.codexPlanTier;
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
    case "api":
      return "Claude API";
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

export function cachePricingStatus(
  status: HeadroomPricingStatus,
  previous: CachedPricing = {}
): CachedPricing {
  return {
    planTier: status.claude.planTier,
    recommendedSubscriptionTier: status.recommendedSubscriptionTier ?? undefined,
    subscriptionTier: status.account?.subscriptionTier ?? undefined,
    codexPlanTier: resolvedCodexPlanTier(status, previous),
  };
}

export const PRICING_CACHE_KEY = "headroom.cachedPricing";

/// Read the cached pricing snapshot used to render the tray before the
/// pricing IPC has resolved. Tolerates missing storage and corrupt JSON.
export function readCachedPricing(): CachedPricing {
  try {
    const raw = localStorage.getItem(PRICING_CACHE_KEY);
    if (raw) return JSON.parse(raw) as CachedPricing;
  } catch {}
  return {};
}

/// Persist the latest pricing snapshot. Best-effort: silently swallows
/// storage failures (private mode, quota exceeded) so a launcher render can't
/// fail just because localStorage is unavailable.
export function writeCachedPricing(pricing: CachedPricing) {
  try {
    localStorage.setItem(PRICING_CACHE_KEY, JSON.stringify(pricing));
  } catch {}
}

/// Orders one window's pricing-status writes so a slow fetch cannot overwrite
/// a newer status. Every write (sign-in, sign-out, plan change, reactivation,
/// a status pushed with `pricing-refreshed`) bumps a counter, and a fetch lands
/// only if the counter has not moved since it was issued. A counter, not a
/// Date.now() stamp: after a backward clock step the stamp made every poll look
/// older than the last write, and dropped them all until the clock caught up.
export function createPricingStatusOrder() {
  let writes = 0;
  let pollInFlight = false;
  return {
    /// Records a status applied from outside fetch().
    wrote() {
      writes += 1;
    },
    /// Resolves to the status to apply, or null when a newer write landed while
    /// it was in flight. Returns null at once, fetching nothing, for a poll
    /// while another poll is in flight. An authoritative fetch, made right after
    /// a mutation, runs anyway (the poll in flight read the account before the
    /// change) and counts as a write, so that older poll is dropped.
    fetch(
      load: () => Promise<HeadroomPricingStatus>,
      authoritative = false
    ): Promise<HeadroomPricingStatus | null> | null {
      const issued = writes;
      if (authoritative) {
        return load().then((status) => {
          if (writes !== issued) return null;
          writes += 1;
          return status;
        });
      }
      if (pollInFlight) return null;
      pollInFlight = true;
      return load()
        .then((status) => (writes === issued ? status : null))
        .finally(() => {
          pollInFlight = false;
        });
    },
  };
}
