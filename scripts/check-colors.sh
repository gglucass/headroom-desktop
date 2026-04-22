#!/usr/bin/env bash
# Flag raw hex/rgba colors in component CSS rules (outside :root and @media blocks).
# New rules should use semantic tokens (--surface-*, --text-*, --border-*, etc.)
# defined at the top of src/styles.css.

set -euo pipefail

file="${1:-src/styles.css}"

# Use awk to track whether we are inside a :root block or a @media (prefers-color-scheme) block.
# Raw colors inside those blocks are the token definitions themselves — allowed.
# Raw colors elsewhere are component-level and should be migrated to tokens.

awk '
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
    count++
  }
}
END { exit (count > 0 ? 1 : 0) }
' "$file"
