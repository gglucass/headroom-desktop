import type {
  HeadroomPricingStatus,
  HeadroomSubscriptionTier,
} from "./types";

export type PricingAudience = "individual" | "teamEnterprise";
export type UpgradePlanId = "free" | "pro" | "max5x" | "max20x" | "team" | "enterprise";

export interface UpgradePlan {
  id: UpgradePlanId;
  name: string;
  tagline: string;
  price: string;
  billingLines: [string, string];
  centeredPriceLabel?: string;
  featureIntro: string;
  features: string[];
  ctaLabel: string;
  ctaVariant: "primary" | "secondary";
  disabled?: boolean;
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

export function getUpgradePlans(
  audience: PricingAudience,
  claudePlanTier?: HeadroomPricingStatus["claude"]["planTier"],
  recommendedSubscriptionTier?: HeadroomPricingStatus["recommendedSubscriptionTier"],
  headroomSubscriptionTier?: HeadroomSubscriptionTier | null
): {
  plans: UpgradePlan[];
  featuredPlanId: UpgradePlanId;
} {
  if (audience === "individual") {
    const freePlan: UpgradePlan = {
      id: "free",
      name: "Free",
      tagline: "Limited usage with Claude",
      price: "$0",
      billingLines: ["/ month", "free"],
      featureIntro: "Includes:",
      features: [
        "Unlocks cost savings and stats",
        "Use with 25% of your Claude plan",
        "Optimize Claude Code practices"
      ],
      ctaLabel: "Stay on Free plan",
      ctaVariant: "secondary"
    };

    const paidPlans: Record<"pro" | "max5x" | "max20x", UpgradePlan> = {
      pro: {
        id: "pro",
        name: "Pro",
        tagline: "Unlock unlimited savings",
        price: "$5",
        billingLines: ["USD / month", "billed annually"],
        featureIntro: "Everything in Free, plus:",
        features: [
          "Unlimited use with Claude Pro",
          "Track sessions across devices",
          "Email-based support"
        ],
        ctaLabel: "Get Pro",
        ctaVariant: "primary"
      },
      max5x: {
        id: "max5x",
        name: "Max x5",
        tagline: "For Claude Max x5 accounts",
        price: "$25",
        billingLines: ["USD / month", "billed annually"],
        featureIntro: "Includes:",
        features: [
          "Unlimited use with Claude Max x5",
          "Track sessions across devices",
          "Email-based support"
        ],
        ctaLabel: "Get Max x5",
        ctaVariant: "primary"
      },
      max20x: {
        id: "max20x",
        name: "Max x20",
        tagline: "For Claude Max x20 accounts",
        price: "$50",
        billingLines: ["USD / month", "billed annually"],
        featureIntro: "Includes:",
        features: [
          "Unlimited use with Claude Max x20",
          "Track sessions across devices",
          "Priority support"
        ],
        ctaLabel: "Get Max x20",
        ctaVariant: "primary"
      }
    };

    const activePaidPlanId = (() => {
      switch (claudePlanTier) {
        case "pro":
          return "pro" as const;
        case "max5x":
          return "max5x" as const;
        case "max20x":
          return "max20x" as const;
        default:
          return headroomSubscriptionTier ?? null;
      }
    })();

    if (activePaidPlanId) {
      const orderedPaidPlans = [
        paidPlans[activePaidPlanId],
        ...(["pro", "max5x", "max20x"] as const)
          .filter((planId) => planId !== activePaidPlanId)
          .map((planId) => paidPlans[planId])
      ];
      return {
        plans: [freePlan, ...orderedPaidPlans],
        featuredPlanId: activePaidPlanId
      };
    }

    if (recommendedSubscriptionTier) {
      const orderedPaidPlans = [
        paidPlans[recommendedSubscriptionTier],
        ...(["pro", "max5x", "max20x"] as const)
          .filter((planId) => planId !== recommendedSubscriptionTier)
          .map((planId) => paidPlans[planId])
      ];
      return {
        plans: [freePlan, ...orderedPaidPlans],
        featuredPlanId: recommendedSubscriptionTier
      };
    }

    if (claudePlanTier === "unknown") {
      return {
        plans: [
          freePlan,
          paidPlans.max5x,
          paidPlans.pro,
          paidPlans.max20x
        ],
        featuredPlanId: "max5x"
      };
    }

    return {
      plans: [
        freePlan,
        paidPlans.pro,
        paidPlans.max5x,
        paidPlans.max20x
      ],
      featuredPlanId: "pro"
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
        ctaVariant: "primary"
      }
    ],
    featuredPlanId: "enterprise"
  };
}
