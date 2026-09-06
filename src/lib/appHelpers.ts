import type {
  BillingPeriod,
  ClientConnectorStatus,
  RuntimeStatus,
  DailySavingsPoint,
  HeadroomAccountProfile,
  HeadroomPricingStatus,
  HeadroomSubscriptionTier,
  IntroOffer,
  PlanPrices,
  TierRecommendationSource,
} from "./types";
import { currencyExact } from "./dashboardHelpers";
// Type-only, so this stays free of setupHealthAlert's Tauri imports at runtime.
import type { SetupStallKind } from "./setupHealthAlert";

export type PricingAudience = "individual" | "teamEnterprise";
export type { BillingPeriod };

/// Fallback price table, compiled into the build. headroom-web serves the
/// live table (`planPrices`); this is what we quote when the server predates
/// that field, is unreachable, or omits a tier. Keep it in sync with
/// Polar::ProductCatalog::LIST_CENTS on the server.
const PLAN_PRICES: Record<
  "pro" | "max5x" | "max20x",
  Record<BillingPeriod, { full: string; fullCents: number }>
> = {
  pro:   { annual: { full: "$3",  fullCents: 300  }, monthly: { full: "$4",  fullCents: 400  } },
  max5x: { annual: { full: "$15",   fullCents: 1500 }, monthly: { full: "$20", fullCents: 2000 } },
  max20x:{ annual: { full: "$30",   fullCents: 3000 }, monthly: { full: "$40", fullCents: 4000 } },
};

/// The server's price table, mirrored here at render time by
/// `setServerPlanPrices`. Module-level rather than threaded through the
/// pricing helpers because all of them (plan cards, payback anchor, renewal
/// projections) need it, and it is server config rather than per-call input.
let serverPlanPrices: PlanPrices | null = null;

/// Point the price helpers at the server's table (null to fall back). Called
/// during render from the pricing status, so a price change reaches the UI
/// without an app release.
export function setServerPlanPrices(prices: PlanPrices | null | undefined): void {
  serverPlanPrices = prices ?? null;
}

/// Price for a tier/period: the server's number when it sent a usable one,
/// else the compiled-in fallback. Non-finite or negative server values are
/// rejected rather than rendered as "$NaN".
function planPrice(
  tier: "pro" | "max5x" | "max20x",
  billingPeriod: BillingPeriod
): { full: string; fullCents: number } {
  const cents = serverPlanPrices?.[tier]?.[billingPeriod];
  if (typeof cents === "number" && Number.isFinite(cents) && cents >= 0) {
    return { full: formatCents(cents), fullCents: cents };
  }
  return PLAN_PRICES[tier][billingPeriod];
}

// Discounted price for a tier, mirroring the web `sale_price_cents` rounding
// (half-up to the cent) so desktop and marketing prices never disagree.
function discountedPriceLabel(fullCents: number, percentOff: number): string {
  return formatCents(Math.round((fullCents * (100 - percentOff)) / 100));
}
const TIER_RANK: Record<HeadroomSubscriptionTier, number> = { pro: 1, max5x: 2, max20x: 3 };

/// The higher-ranked of two optional tiers: the known one when only one is
/// present, null when neither is. Used to pitch a user routing both Claude and
/// Codex the plan that covers their bigger account.
export function higherSubscriptionTier(
  a: HeadroomSubscriptionTier | null | undefined,
  b: HeadroomSubscriptionTier | null | undefined
): HeadroomSubscriptionTier | null {
  if (!a) return b ?? null;
  if (!b) return a;
  return TIER_RANK[b] > TIER_RANK[a] ? b : a;
}

/// Whether the billing-period tab being viewed is the one the subscriber
/// actually bought. An unknown/absent server value means "can't tell" - treat
/// it as a match so the active-plan chrome never vanishes on older servers.
export function matchesSubscriptionPeriod(
  billingPeriod: BillingPeriod,
  subscriptionBillingPeriod?: string | null
): boolean {
  if (subscriptionBillingPeriod !== "annual" && subscriptionBillingPeriod !== "monthly") {
    return true;
  }
  return subscriptionBillingPeriod === billingPeriod;
}

export function isTierDowngrade(
  fromTier: HeadroomSubscriptionTier,
  toTier: HeadroomSubscriptionTier
): boolean {
  return TIER_RANK[toTier] < TIER_RANK[fromTier];
}

function projectPerMonthCents(
  toTier: HeadroomSubscriptionTier,
  billingPeriod: BillingPeriod,
  options?: { fromTier?: HeadroomSubscriptionTier; currentPaidCents?: number | null }
): number {
  // PLAN_PRICES.fullCents is per-month even on annual cycles.
  const toFullPerMonth = planPrice(toTier, billingPeriod).fullCents;
  const fromTier = options?.fromTier;
  const currentPaidCents = options?.currentPaidCents ?? null;
  if (!fromTier || currentPaidCents === null) return toFullPerMonth;
  const fromFullPerMonth = planPrice(fromTier, billingPeriod).fullCents;
  if (fromFullPerMonth <= 0) return toFullPerMonth;
  // Polar reports subscription_amount_cents per full billing cycle (12x
  // per-month for annual), so normalize to per-month before the ratio math.
  const cycleMonths = billingPeriod === "annual" ? 12 : 1;
  const currentPaidPerMonth = currentPaidCents / cycleMonths;
  return Math.round(toFullPerMonth * (currentPaidPerMonth / fromFullPerMonth));
}

export function formatCents(cents: number): string {
  const dollars = cents / 100;
  return cents % 100 === 0 ? `$${dollars}` : `$${dollars.toFixed(2)}`;
}

/// Per-month price label for the target tier (e.g. `$20 / month`), with the
/// user's current discount ratio carried forward. Matches the upgrade view
/// convention where annual prices are shown per-month for tier comparison.
/// `currentPaidCents` must be a cycle amount for `billingPeriod` - passing an
/// annual amount while asking for a monthly period reads 12x high.
export function getPlanRenewalPriceLabel(
  toTier: HeadroomSubscriptionTier,
  billingPeriod: BillingPeriod,
  options?: { fromTier?: HeadroomSubscriptionTier; currentPaidCents?: number | null }
): string {
  return `${formatCents(projectPerMonthCents(toTier, billingPeriod, options))} / month`;
}

/// The intro percent while the offer runs, else 0. Both billing periods show
/// the straight percent off the period's sticker price; on annual the charge
/// still works out identically (6 half-price + 6 full months per invoice).
export function introPercentOff(introOffer: IntroOffer | null | undefined): number {
  if (!introOffer?.active || introOffer.percentOff <= 0) return 0;
  return introOffer.percentOff;
}

/// Sale-badge copy for the intro offer, or null when it is not running.
export function introSaleBadgeLabel(introOffer: IntroOffer | null | undefined): string | null {
  const pct = introPercentOff(introOffer);
  if (pct <= 0 || !introOffer) return null;
  return `${pct}% off first ${introOffer.durationMonths} months`;
}

/// Average daily savings over the trailing `days` window (default 7), used to
/// project realized/forgone savings for upgrade copy. Returns 0 with no history.
export function recentDailySavingsUsd(daily: DailySavingsPoint[], days = 7): number {
  if (daily.length === 0) return 0;
  const window = daily.slice(-days);
  const total = window.reduce((sum, p) => sum + p.estimatedSavingsUsd, 0);
  return total / window.length;
}

/// Item 1 - "pays for itself" anchor. Compares the user's recent monthly
/// savings rate against the per-month price of `planId`. Only surfaces at a
/// genuine value-add (>= 2x the price); returns null below that so a weak claim
/// never deters an upgrade. Floors the multiple so it never overstates.
export function paybackLabel(
  recentMonthlySavingsUsd: number,
  planId: HeadroomSubscriptionTier,
  billingPeriod: BillingPeriod
): string | null {
  const monthly = planPrice(planId, billingPeriod).fullCents / 100;
  if (monthly <= 0) return null;
  const multiple = recentMonthlySavingsUsd / monthly;
  if (multiple < 2) return null;
  return `You're saving about ${currencyExact(recentMonthlySavingsUsd)} a month. Upgrading pays for itself ${Math.floor(multiple)}x over.`;
}

/// Item 2 - counterfactual for the weekly gate. Projects the savings forgone
/// while optimization is paused until the weekly limit resets. `daysUntilReset`
/// may be fractional. Returns null below $1 so we don't nag over trivial sums.
export function forgoneSavingsLabel(
  recentDailySavingsUsd: number,
  daysUntilReset: number
): string | null {
  if (recentDailySavingsUsd <= 0 || daysUntilReset <= 0) return null;
  const forgone = recentDailySavingsUsd * daysUntilReset;
  if (forgone < 1) return null;
  return `You'll miss out on about ${currencyExact(forgone)} in savings this week unless you upgrade.`;
}

export type UpgradePlanId = "free" | "pro" | "max5x" | "max20x" | "team" | "enterprise";
type IndividualUpgradePlanId = "free" | "pro" | "max5x" | "max20x";
type PaidUpgradePlanId = HeadroomSubscriptionTier;

const INDIVIDUAL_PLAN_ORDER: IndividualUpgradePlanId[] = ["free", "pro", "max5x", "max20x"];

export interface UpgradePlanPurchaseInfo {
  renewsOn: string;
  /// What the next invoice actually charges, in the cycle it is billed in:
  /// "$360/yr" on annual, "$30/mo" on monthly.
  renewalPriceLabel: string;
  discountPct: number;
  /// Renewal price as a fraction of sticker. The percent above is rounded for
  /// display; pricing another tier off it turns an exact third into $10.05, so
  /// carry the discount across plan cards with this instead.
  renewalRatio: number;
  /// "33% off for 12 months" / "40% off forever". Absent when nothing is off.
  renewalNote?: string;
  cancelAtPeriodEnd?: boolean;
  endsOn?: string;
}

export interface UpgradePlan {
  id: UpgradePlanId;
  name: string;
  tagline: string;
  price: string;
  originalPrice?: string;
  billingLines: [string, string];
  /// Full-width line under the price row spelling out the post-intro rate.
  reversionLine?: string;
  centeredPriceLabel?: string;
  featureIntro: string;
  features: string[];
  ctaLabel: string;
  ctaVariant: "primary" | "secondary";
  ctaTone?: "default" | "downgrade";
  /// Label for the strikethrough sale row, when the card is discounted.
  saleBadge?: string;
  purchaseInfo?: UpgradePlanPurchaseInfo;
}

export function upgradePlanIntentLabel(planId: UpgradePlanId | null) {
  switch (planId) {
    case "pro":
      return "Pro";
    case "max5x":
      return "Max x5";
    case "max20x":
      return "Max x20";
    default:
      return null;
  }
}

export interface ScheduledPlanChange {
  tier: HeadroomSubscriptionTier;
  billingPeriod: BillingPeriod;
  note: string;
}

/** A plan change the server has scheduled for the next cycle. Until it lands
 * the subscription still reports the plan being paid for, so the pending
 * fields are the only sign of it. Null unless both the tier and a usable
 * effective date are known -- an unparseable date would otherwise reach the
 * billing screen as "on Invalid Date". */
export function scheduledPlanChange(
  account: HeadroomAccountProfile | null | undefined
): ScheduledPlanChange | null {
  const tier = account?.subscriptionPendingTier;
  const effectiveAt = account?.subscriptionPendingEffectiveAt;
  if (!tier || !effectiveAt) return null;
  const effective = new Date(effectiveAt);
  if (Number.isNaN(effective.getTime())) return null;
  const billingPeriod: BillingPeriod =
    account?.subscriptionPendingBillingPeriod === "monthly" ? "monthly" : "annual";
  const on = effective.toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    year: "numeric"
  });
  // Same tier on a shorter cycle is a billing switch, not a plan change.
  const note = tier === account?.subscriptionTier
    ? `Switches to ${billingPeriod} billing on ${on}`
    : `Switches to ${upgradePlanIntentLabel(tier)} (${billingPeriod}) on ${on}`;
  return { tier, billingPeriod, note };
}

// Connector(s) whose detected plan drives a tier-mismatch recommendation, for
// the upgrade banner copy.
export function tierRecommendationSourceLabel(source: TierRecommendationSource) {
  switch (source) {
    case "codex":
      return "ChatGPT";
    case "both":
      return "Claude and ChatGPT";
    default:
      return "Claude";
  }
}

export function describeInvokeError(error: unknown, fallback: string) {
  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }
  if (typeof error === "string" && error.trim()) {
    return error;
  }
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string" &&
    error.message.trim()
  ) {
    return error.message;
  }
  if (
    typeof error === "object" &&
    error !== null &&
    "error" in error &&
    typeof error.error === "string" &&
    error.error.trim()
  ) {
    return error.error;
  }
  return fallback;
}

export function getNextLowerUpgradePlanId(
  planId?: PaidUpgradePlanId | null
): IndividualUpgradePlanId | null {
  switch (planId) {
    case "pro":
      // No free plan to downgrade to post-trial.
      return null;
    case "max5x":
      return "pro";
    case "max20x":
      return "max5x";
    default:
      return null;
  }
}

/// The tier above `planId`, or null at the top. The companion card next to an
/// active plan: this is the upgrade view, so the nearest step up earns the slot.
export function getNextHigherUpgradePlanId(
  planId?: PaidUpgradePlanId | null
): IndividualUpgradePlanId | null {
  switch (planId) {
    case "pro":
      return "max5x";
    case "max5x":
      return "max20x";
    default:
      return null;
  }
}

export function getUpgradePlans(
  audience: PricingAudience,
  claudePlanTier?: HeadroomPricingStatus["claude"]["planTier"],
  recommendedSubscriptionTier?: HeadroomPricingStatus["recommendedSubscriptionTier"],
  headroomSubscriptionTier?: HeadroomSubscriptionTier | null,
  hasActiveHeadroomSubscription = false,
  launchDiscountActive = false,
  billingPeriod: BillingPeriod = "annual",
  subscriptionAmountCents?: number | null,
  subscriptionBillingPeriod?: string | null,
  subscriptionRenewsAt?: string | null,
  subscriptionStartedAt?: string | null,
  subscriptionDiscountDuration?: string | null,
  subscriptionDiscountDurationInMonths?: number | null,
  subscriptionCancelAtPeriodEnd: boolean = false,
  subscriptionEndsAt?: string | null,
  activePercentOff: number = 0,
  introOffer: IntroOffer | null = null,
  subscriptionRenewalCents?: number | null,
  subscriptionRenewalEndsAt?: string | null,
  // The server's upgradeAction for AppSumo-entitled accounts; "appsumo" means
  // every plan change happens on AppSumo, so Polar prices would be wrong.
  upgradeAction?: string | null
): {
  plans: UpgradePlan[];
  featuredPlanId: UpgradePlanId;
} {
  if (audience === "individual") {
    // No free plan post-trial: the upgrade sheet only offers paid plans.
    const billingLabel = billingPeriod === "annual" ? "billed annually" : "billed monthly";

    const activeHeadroomPlanId =
      hasActiveHeadroomSubscription && headroomSubscriptionTier
        ? headroomSubscriptionTier
        : null;
    // The subscription lives on one billing period. On the other tab the same
    // tier is a switch offer, not the plan you are on.
    const periodMatches = matchesSubscriptionPeriod(billingPeriod, subscriptionBillingPeriod);

    // Compute purchase info for the active plan card when data is available.
    const activePurchaseInfo = ((): UpgradePlanPurchaseInfo | undefined => {
      if (!activeHeadroomPlanId || subscriptionAmountCents == null) {
        return undefined;
      }
      const purchasePeriod = (subscriptionBillingPeriod === "annual" || subscriptionBillingPeriod === "monthly")
        ? subscriptionBillingPeriod
        : billingPeriod;
      const fullCents = planPrice(activeHeadroomPlanId, purchasePeriod).fullCents;

      // Determine if the discount will still apply at renewal time.
      const discountAppliesAtRenewal = ((): boolean => {
        if (!subscriptionDiscountDuration) return false;
        if (subscriptionDiscountDuration === "forever") return true;
        if (subscriptionDiscountDuration === "once") return false;
        // "repeating": check if renewal falls within the discount window
        if (
          subscriptionDiscountDuration === "repeating" &&
          subscriptionDiscountDurationInMonths != null &&
          subscriptionStartedAt &&
          subscriptionRenewsAt
        ) {
          const discountExpiresAt = new Date(subscriptionStartedAt);
          discountExpiresAt.setMonth(discountExpiresAt.getMonth() + subscriptionDiscountDurationInMonths);
          return new Date(subscriptionRenewsAt) < discountExpiresAt;
        }
        return false;
      })();

      // Amount is stored as per-billing-cycle cents; convert to per-month.
      const paidCentsPerMonth = purchasePeriod === "annual"
        ? subscriptionAmountCents / 12
        : subscriptionAmountCents;

      // The server's own figure wins when it has one: a discount attached
      // mid-subscription can't be dated from subscriptionStartedAt, so the
      // window check above reads it as expired. Nothing fills these fields
      // today (the save offer did, until 2026-09-02), but the contract stays
      // for the next mid-subscription discount.
      const serverRenewalCents =
        subscriptionRenewalCents != null &&
        subscriptionRenewalEndsAt != null &&
        new Date(subscriptionRenewalEndsAt) > new Date()
          ? subscriptionRenewalCents
          : null;

      // If the discount won't apply at renewal, show full price for the renewal.
      const renewalCentsPerMonth = serverRenewalCents != null
        ? (purchasePeriod === "annual" ? serverRenewalCents / 12 : serverRenewalCents)
        : discountAppliesAtRenewal ? paidCentsPerMonth : fullCents;
      const renewalRatio = fullCents > 0 ? renewalCentsPerMonth / fullCents : 1;
      const discountPct = fullCents > 0 && renewalCentsPerMonth < fullCents
        ? Math.round((1 - renewalRatio) * 100)
        : 0;
      const perMonthLabel = (cents: number) => `$${(cents / 100).toFixed(2).replace(/\.00$/, "")}`;
      // The card's sticker price is per month, but an annual subscription is
      // charged once a year - quote the renewal in the amount that will hit
      // the card, so it matches the billing portal.
      const cycleLabel = (centsPerMonth: number) =>
        purchasePeriod === "annual"
          ? `${perMonthLabel(centsPerMonth * 12)}/yr`
          : `${perMonthLabel(centsPerMonth)}/mo`;
      const renewalPriceLabel = cycleLabel(renewalCentsPerMonth);
      // How long the discount behind that price runs. The save offer needs no
      // special case: applying it attaches a repeating 12-month discount, which
      // comes back through the same two fields as any other.
      const months = subscriptionDiscountDurationInMonths ?? 0;
      const durationLabel = subscriptionDiscountDuration === "forever"
        ? "forever"
        : months > 0 ? `for ${months} month${months === 1 ? "" : "s"}` : null;
      // When today's rate is not the renewal rate, say so. Otherwise the card
      // states only the future price and reads as contradicting the billing
      // portal, which shows the amount currently being charged.
      const renewalNote = discountPct > 0
        ? `${discountPct}% off${durationLabel ? ` ${durationLabel}` : ""}`
        : Math.round(paidCentsPerMonth) !== Math.round(renewalCentsPerMonth)
        ? `${cycleLabel(paidCentsPerMonth)} until then`
        : undefined;
      const renewsOn = subscriptionRenewsAt
        ? new Date(subscriptionRenewsAt).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" })
        : null;
      if (!renewsOn) return undefined;
      const endsOn = subscriptionCancelAtPeriodEnd && subscriptionEndsAt
        ? new Date(subscriptionEndsAt).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" })
        : undefined;
      return {
        renewsOn,
        renewalPriceLabel,
        discountPct,
        renewalRatio,
        renewalNote,
        cancelAtPeriodEnd: subscriptionCancelAtPeriodEnd,
        endsOn
      };
    })();

    function paidPlan(
      id: "pro" | "max5x" | "max20x",
      name: string,
      tagline: string,
      featureIntro: string,
      features: string[],
      ctaLabel: string
    ): UpgradePlan {
      const prices = planPrice(id, billingPeriod);
      // Upgrade-target cards show the discounted price because checkout
      // attaches the matching Polar discount server-side; the active plan card
      // uses purchaseInfo (actual paid amount) instead of a generic badge.
      // The intro/launch offers only exist on a *new* checkout. An existing
      // subscriber changing tier goes through subscriptions#change_plan, which
      // swaps the Polar product and carries over only the discount already on
      // the subscription - so their own percent is the only one that can apply,
      // and quoting them the intro price promised a discount they'd never get.
      // The activePercentOff/50 fallback serves legacy cohort servers that
      // still signal launchDiscountActive.
      const accountDiscountPct = activePurchaseInfo?.discountPct ?? 0;
      const newCheckout = !hasActiveHeadroomSubscription;
      const introPct = newCheckout ? introPercentOff(introOffer) : 0;
      const legacyPct = newCheckout && launchDiscountActive
        ? (activePercentOff > 0 ? activePercentOff : 50)
        : 0;
      const effectivePercentOff = accountDiscountPct || introPct || legacyPct;
      // Each card states the discount once. On the active card of the period
      // actually bought, the renewal line under the price already does it, so
      // the badge would be the same fact twice; on the other period's tab there
      // is no renewal line, and without the badge the switch offer reads as if
      // it costs the discount.
      const showDiscount =
        effectivePercentOff > 0 && (id !== activeHeadroomPlanId || !periodMatches);
      // An account discount prices off the exact ratio it renews at, not the
      // rounded percent, so a third off $15 quotes $10 and not $10.05.
      const price = !showDiscount
        ? prices.full
        : accountDiscountPct > 0 && activePurchaseInfo
        ? formatCents(Math.round(prices.fullCents * activePurchaseInfo.renewalRatio))
        : discountedPriceLabel(prices.fullCents, effectivePercentOff);
      return {
        id,
        name,
        tagline,
        price,
        ...(showDiscount ? { originalPrice: prices.full } : {}),
        // Subscribers see no intro/launch badge, so without this their carried
        // discount showed up as a bare lowered number with nothing explaining it.
        // No duration unless it is forever: a repeating discount carries its
        // *remaining* months across a plan change, not a fresh full term.
        ...(showDiscount && accountDiscountPct > 0
          ? {
              saleBadge: `${accountDiscountPct}% off${
                subscriptionDiscountDuration === "forever" ? " forever" : ""
              }`
            }
          : {}),
        ...(id === activeHeadroomPlanId && periodMatches && activePurchaseInfo
          ? { purchaseInfo: activePurchaseInfo }
          : {}),
        billingLines: ["USD / month", billingLabel],
        // Full-width line under the price so "$10/mo billed annually" can't
        // be misread as the full-year rate; the badge names the duration.
        ...(showDiscount && introPct > 0 && introOffer
          ? { reversionLine: `then ${prices.full}/mo after ${introOffer.durationMonths} months` }
          : {}),
        featureIntro,
        features,
        ctaLabel,
        ctaVariant: "primary",
        ctaTone: "default"
      };
    }

    const paidPlans: Record<"pro" | "max5x" | "max20x", UpgradePlan> = {
      pro: paidPlan("pro", "Pro", "Unlock unlimited savings", "Everything in Free, plus:", [
        "Unlimited use with Claude Pro or ChatGPT Plus",
        "Use on all your devices with one account",
        "Email-based support"
      ], "Get Pro"),
      max5x: paidPlan("max5x", "Max x5", "For Claude Max x5 or ChatGPT Pro Lite accounts", "Includes:", [
        "Unlimited use with Claude Max x5 or ChatGPT Pro Lite",
        "Use on all your devices with one account",
        "Email-based support"
      ], "Get Max x5"),
      max20x: paidPlan("max20x", "Max x20", "For Claude Max x20 or ChatGPT Pro accounts", "Includes:", [
        "Unlimited use with Claude Max x20 or ChatGPT Pro",
        "Use on all your devices with one account",
        "Priority support"
      ], "Get Max x20"),
    };

    const withRelativeCta = (plan: UpgradePlan): UpgradePlan => {
      if (!activeHeadroomPlanId) {
        return plan;
      }

      const planRank = INDIVIDUAL_PLAN_ORDER.indexOf(plan.id as IndividualUpgradePlanId);
      const activeRank = INDIVIDUAL_PLAN_ORDER.indexOf(activeHeadroomPlanId);
      if (planRank === -1 || activeRank === -1) {
        return plan;
      }

      if (plan.id === activeHeadroomPlanId) {
        return periodMatches
          ? {
              ...plan,
              ctaLabel: `Stay on ${plan.name} plan`,
              ctaVariant: "secondary",
              ctaTone: "default"
            }
          : {
              ...plan,
              ctaLabel: `Switch to ${billingPeriod === "annual" ? "annual" : "monthly"} billing`,
              ctaVariant: "primary",
              ctaTone: "default"
            };
      }

      if (planRank < activeRank) {
        return {
          ...plan,
          ctaLabel: `Downgrade to ${plan.name} plan`,
          ctaVariant: "secondary",
          ctaTone: "downgrade"
        };
      }

      return {
        ...plan,
        ctaLabel: `Upgrade to ${plan.name}`,
        ctaVariant: "primary",
        ctaTone: "default"
      };
    };

    // While the AppSumo deal is live every plan change happens on AppSumo as a
    // one-time tier buy, so the Polar monthly/annual prices on the cards would
    // promise a price nobody pays. centeredPriceLabel replaces the whole price
    // block (sticker, billing lines, discounts) with the AppSumo figure.
    const APPSUMO_LIFETIME_PRICES: Record<"pro" | "max5x" | "max20x", string> = {
      pro: "$29",
      max5x: "$99",
      max20x: "$199"
    };
    const withAppsumoPricing = (plan: UpgradePlan): UpgradePlan => {
      if (upgradeAction !== "appsumo") {
        return plan;
      }
      if (plan.id === activeHeadroomPlanId) {
        return { ...plan, centeredPriceLabel: "lifetime plan • via AppSumo" };
      }
      const oneTime = APPSUMO_LIFETIME_PRICES[plan.id as "pro" | "max5x" | "max20x"];
      return {
        ...plan,
        centeredPriceLabel: `${oneTime} one-time • on AppSumo`,
        ...(plan.ctaLabel.startsWith("Upgrade") ? { ctaLabel: "Upgrade on AppSumo" } : {})
      };
    };

    if (activeHeadroomPlanId) {
      const orderedPaidPlans = [
        paidPlans[activeHeadroomPlanId],
        ...(["pro", "max5x", "max20x"] as const)
          .filter((planId) => planId !== activeHeadroomPlanId)
          .map((planId) => paidPlans[planId])
      ].map(withRelativeCta).map(withAppsumoPricing);
      return {
        plans: orderedPaidPlans,
        featuredPlanId: activeHeadroomPlanId
      };
    }

    // Pitch the higher of the Claude-implied tier and the recommendation (the
    // caller folds the Codex-implied tier into it). A user routing both clients
    // needs the plan that covers the bigger account - the same rule as the
    // server's recommended_headroom_tier and pricing::detect_tier_mismatch.
    // Claude Max x20 + ChatGPT Pro Lite used to be pitched Max x5 and landed in
    // the tier-mismatch clamp two weeks after paying. A lapsed subscriber with
    // neither signal is pitched their last paid tier.
    const claudePlanId = (() => {
      switch (claudePlanTier) {
        case "pro":
          return "pro" as const;
        case "max5x":
          return "max5x" as const;
        case "max20x":
          return "max20x" as const;
        default:
          return null;
      }
    })();
    const pitchedPlanId =
      higherSubscriptionTier(claudePlanId, recommendedSubscriptionTier) ??
      headroomSubscriptionTier ??
      null;

    if (pitchedPlanId) {
      const orderedPaidPlans = [
        paidPlans[pitchedPlanId],
        ...(["pro", "max5x", "max20x"] as const)
          .filter((planId) => planId !== pitchedPlanId)
          .map((planId) => paidPlans[planId])
      ];
      return {
        plans: orderedPaidPlans,
        featuredPlanId: pitchedPlanId
      };
    }

    // No recommendation (e.g. not signed in yet) or an unknown Claude plan:
    // default to Max x5 as the featured plan.
    return {
      plans: [paidPlans.max5x, paidPlans.pro, paidPlans.max20x],
      featuredPlanId: "max5x"
    };
  }

  return {
    plans: [
      {
        id: "enterprise",
        name: "Team & Enterprise",
        tagline: "Shared controls, governance, and private deployment options",
        price: "",
        billingLines: ["", ""],
        centeredPriceLabel: "custom pricing • contact us",
        featureIntro: "",
        features: [],
        ctaLabel: "Submit",
        ctaVariant: "primary",
        ctaTone: "default"
      }
    ],
    featuredPlanId: "enterprise"
  };
}

/// Support mail for the setup-stall alert. Carries the state that decided which
/// branch fired, so a reply doesn't have to start by asking for all of it.
export function buildSetupStallMailto(
  kind: SetupStallKind,
  context: {
    appVersion: string;
    lifetimeRequests: number;
    runtime: RuntimeStatus | null;
    connectors: ClientConnectorStatus[];
  }
): string {
  const subject = `Headroom is not saving anything (${kind})`;
  const connectorLines = context.connectors.length
    ? context.connectors.map(
        (connector) =>
          `  ${connector.name}: installed=${connector.installed} enabled=${connector.enabled} verified=${connector.verified}`
      )
    : ["  (none reported)"];
  const diagnosticLines = [
    `Alert: ${kind}`,
    `App version: ${context.appVersion}`,
    `Lifetime requests seen: ${context.lifetimeRequests}`,
    `Runtime: installed=${context.runtime?.installed ?? "unknown"} running=${
      context.runtime?.running ?? "unknown"
    } paused=${context.runtime?.paused ?? "unknown"} proxyReachable=${
      context.runtime?.proxyReachable ?? "unknown"
    }`,
    "Connectors:",
    ...connectorLines,
  ];
  const body =
    "Which coding agent are you using, and how do you launch it?\n\n\n" +
    "---\n" +
    "Diagnostic info (please keep):\n" +
    diagnosticLines.join("\n");
  return `mailto:support@extraheadroom.com?subject=${encodeURIComponent(
    subject
  )}&body=${encodeURIComponent(body)}`;
}

/** Pre-filled support mail for a failed first install.
 *
 *  The install screen shows only the friendly message, so an unaided report
 *  quotes back copy we wrote ("couldn't download a required file") and names
 *  nothing we can act on — every one of the RUST-1G reports looked identical
 *  regardless of whether the cause was a bad pin, a proxy, or a full disk.
 *  `kind` is the same vocabulary as the `failure_kind` Sentry tag, so a mail
 *  can be matched to its issue; `detail` is pip's stderr tail. */
export function buildInstallFailureMailto(context: {
  kind: string | null;
  detail: string | null;
  appVersion: string;
  platform: string;
}): string {
  const subject = `Headroom install failed (${context.kind ?? "unknown"})`;
  const diagnosticLines = [
    `Failure kind: ${context.kind ?? "unknown"}`,
    `App version: ${context.appVersion}`,
    `Platform: ${context.platform}`,
    "",
    "Technical detail:",
    context.detail ?? "(none captured)",
  ];
  const body =
    "What happened, and what have you already tried?\n\n\n" +
    "---\n" +
    "Diagnostic info (please keep):\n" +
    diagnosticLines.join("\n");
  return `mailto:support@extraheadroom.com?subject=${encodeURIComponent(
    subject
  )}&body=${encodeURIComponent(body)}`;
}

// The server keeps exactly one live sign-in code per user: `issue_sign_in_code!`
// overwrites the previous digest, so a resend silently kills the code in the
// email still in flight. A stale code then fails with the same "Invalid email
// or code." as a typo, which is what sends people around the resend loop
// (identity 3922 asked for eight codes in an hour before one worked). Saying
// both facts up front is the whole fix.
export function authCodeSentMessage(email: string, expirySeconds: number): string {
  const minutes = Math.max(1, Math.round(expirySeconds / 60));
  const unit = minutes === 1 ? "minute" : "minutes";
  return (
    `We sent a sign-in code to ${email}. It expires in ${minutes} ${unit}. ` +
    "If you ask for another, only the newest code works."
  );
}
