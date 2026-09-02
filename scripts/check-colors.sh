#!/usr/bin/env bash
# Flag raw hex/rgba colors in component CSS rules (outside :root and @media blocks).
# New rules should use semantic tokens (--surface-*, --text-*, --border-*, etc.)
# defined at the top of src/styles.css.

set -euo pipefail

file="${1:-src/styles.css}"

# Ratchet, not a clean-tree gate. src/styles.css carries a pre-existing backlog
# (the Stage 4 token migration), so requiring zero would mean either never
# running this in CI or blocking every unrelated PR on that migration. Instead
# the count is only allowed to shrink. Lower this number as you migrate rules.
# ponytail: a stale baseline lets offenders regrow up to it after a migration;
# tighten by failing on count < BASELINE if that ever actually happens.
BASELINE="${CHECK_COLORS_BASELINE:-478}"

# Use awk to track whether we are inside a :root block or a @media (prefers-color-scheme) block.
# Raw colors inside those blocks are the token definitions themselves — allowed.
# Raw colors elsewhere are component-level and should be migrated to tokens.

offenders="$(awk '
BEGIN { depth = 0; in_token_block = 0 }
/^:root \{/                           { in_token_block = 1; depth = 1; next }
/^@media \(prefers-color-scheme:/     { in_token_block = 1; depth = 0; next }
in_token_block {
  n = gsub(/\{/, "{"); depth += n
  n = gsub(/\}/, "}"); depth -= n
  if (depth <= 0) { in_token_block = 0; depth = 0 }
  next
}
# Only flag properties whose value is a theme-sensitive color.
/^[[:space:]]*(color|background|background-color|border|border-color|border-top|border-bottom|border-left|border-right|border-top-color|border-bottom-color|border-left-color|border-right-color|outline|outline-color|fill|stroke)[[:space:]]*:/ {
  # Skip pure white/black (often theme-invariant).
  line = $0
  if (line ~ /#(fff|FFF|ffffff|FFFFFF|000|000000)[;[:space:]]/) next
  if (line ~ /#[0-9a-fA-F]{3,8}|rgba?\(|hsla?\(/) {
    printf "%s:%d: %s\n", FILENAME, NR, $0
  }
}
' "$file")"

if [ -z "$offenders" ]; then
  count=0
else
  printf '%s\n' "$offenders"
  count=$(printf '%s\n' "$offenders" | wc -l | tr -d ' ')
fi

echo "check-colors: $count raw color(s) in component rules (baseline $BASELINE)"

if [ "$count" -gt "$BASELINE" ]; then
  echo
  echo "FAIL: $((count - BASELINE)) new offender(s) above the baseline."
  echo "Use the semantic tokens defined in :root in src/styles.css (--surface-*,"
  echo "--text-*, --border-*, --fill-*, --accent*, --warning*, --danger*, --chip-*)."
  echo "If a needed color has no token, add it to BOTH :root and the"
  echo "@media (prefers-color-scheme: dark) override rather than inlining it."
  exit 1
fi

if [ "$count" -lt "$BASELINE" ]; then
  echo "Baseline is stale: lower BASELINE in this script to $count."
fi
