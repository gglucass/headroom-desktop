/**
 * Per-model input-token prices (USD per 1M tokens) for estimating the dollar
 * value of a single compression. Consumers: the Activity feed's compression
 * detail row.
 *
 * This is an approximation on purpose:
 *   - We charge the full input rate for every saved token, even though a
 *     real request might have hit the prompt cache (cheaper) or been
 *     uncached (same rate). The dashboard's lifetime estimate uses the same
 *     simplification.
 *   - We only track input pricing. Compression saves input tokens; output
 *     pricing does not apply.
 *
 * Callers should display the result with a `~` prefix.
 */

type PriceRule = { match: RegExp; usdPerMTokens: number };

// Ordered most-specific → least-specific. First match wins.
const PRICE_RULES: PriceRule[] = [
  { match: /opus-4/i, usdPerMTokens: 15 },
  { match: /opus/i, usdPerMTokens: 15 },
  { match: /sonnet-4/i, usdPerMTokens: 3 },
  { match: /sonnet/i, usdPerMTokens: 3 },
  { match: /haiku-4/i, usdPerMTokens: 1 },
  { match: /haiku/i, usdPerMTokens: 0.8 },
  { match: /gpt-4o-mini/i, usdPerMTokens: 0.15 },
  { match: /gpt-4o/i, usdPerMTokens: 2.5 },
  { match: /gpt-4/i, usdPerMTokens: 2.5 },
  { match: /gpt-5/i, usdPerMTokens: 3 },
  { match: /gpt-3\.5/i, usdPerMTokens: 0.5 },
  { match: /gemini.*pro/i, usdPerMTokens: 1.25 },
  { match: /gemini.*flash/i, usdPerMTokens: 0.1 },
];

// Fallback when we have no idea what model this is — use Sonnet-class as a
// reasonable middle-of-the-road rate so the number isn't wildly off.
const DEFAULT_USD_PER_MTOKENS = 3;

export function estimateCostSavingsUsd(
  model: string | null | undefined,
  tokensSaved: number | null | undefined
): number | null {
  if (!tokensSaved || tokensSaved <= 0) return null;
  const rate = rateFor(model);
  return (tokensSaved / 1_000_000) * rate;
}

function rateFor(model: string | null | undefined): number {
  if (!model) return DEFAULT_USD_PER_MTOKENS;
  for (const rule of PRICE_RULES) {
    if (rule.match.test(model)) return rule.usdPerMTokens;
  }
  return DEFAULT_USD_PER_MTOKENS;
}

/**
 * Format a small per-request USD estimate. Always prefixed with `~` because
 * the rate is heuristic, and uses enough precision to show sub-cent values
 * that matter for a single compression (e.g. `~$0.0023`).
 */
export function formatEstimatedUsd(usd: number): string {
  if (usd >= 1) return `~$${usd.toFixed(2)}`;
  if (usd >= 0.01) return `~$${usd.toFixed(3)}`;
  if (usd >= 0.0001) return `~$${usd.toFixed(4)}`;
  return "~<$0.0001";
}
