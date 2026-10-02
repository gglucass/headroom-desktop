#!/usr/bin/env bash
# Runs the non-Codex half of docs/beta-smoke-test.md and prints a PASS/FAIL table.
#
# The doc stays the "why" reference - every past false-FAIL it records (the
# 0.9.1-rc.2 rtkDisabled gate, the 0.8.1-rc.2 lsof pid, the 0.7.6-rc.1 hung
# curl, the 0.9.11-rc.4 resume sleep) was someone re-implementing a step the
# doc already spelled out and getting it subtly wrong. Those steps live here
# now so they cannot drift.
#
#   ./scripts/smoke-test.sh           full pass (restarts the app twice)
#   ./scripts/smoke-test.sh --quick   skip checks 6, 9 and 12's SIGUSR1 probe
#
# Checks 4, 7 and the visual half of 5/16 cannot be driven from a shell; they
# are reported as MANUAL/PENDING with the exact follow-up.

set -uo pipefail

QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

APP="/Applications/Headroom.app"
SUP="$HOME/Library/Application Support/Headroom"
CFG="$SUP/config"
HR="$SUP/headroom"
# Wheel 0.38.0 (#3204) writes proxy-<port>.log and stops touching proxy.log, so
# an upgraded machine keeps a stale proxy.log forever; every boot-scoped count
# below would then read 0 against a file the backend no longer writes and pass
# vacuously (0.9.20-rc.3 pass). Pick the newest proxy*.log the logger writes.
PROXY_LOG=$(ls -t "$HOME"/.headroom/logs/proxy*.log 2>/dev/null | grep -v '/proxy-stdio' | head -1)
PROXY_LOG=${PROXY_LOG:-"$HOME/.headroom/logs/proxy.log"}
BASELINE="${TMPDIR:-/tmp}/hr-smoke-baseline.json"
SHOTS="${TMPDIR:-/tmp}/hr-smoke-shots"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

FAILED=0
ROWS=()
row() { # status, check, detail
  ROWS+=("$1|$2|$3")
  [ "$1" = "FAIL" ] && FAILED=1
  printf '  %-7s %-34s %s\n' "$1" "$2" "$3"
  return 0   # called as `cond && row PASS || row FAIL`; must never fire the || branch
}
have() { command -v "$1" >/dev/null 2>&1; }

AX() { osascript -e "tell application \"System Events\" to tell process \"headroom-desktop\" $1" 2>&1; }
# Since 0.9.30-rc.2 the macOS tray icon carries no menu: macOS 27 opened an
# attached menu on every click, so it is attached only for a right-click. An
# AXPress on the status item no longer opens it; a real right-click does.
open_tray_menu() {
  local p
  p=$(AX 'to get {position, size} of menu bar item 1 of menu bar 2' | tr -d ' ')
  [[ "$p" =~ ^([0-9-]+),([0-9-]+),([0-9]+),([0-9]+)$ ]] || return 1
  cliclick "rc:$(( ${BASH_REMATCH[1]} + ${BASH_REMATCH[3]} / 2 )),$(( ${BASH_REMATCH[2]} + ${BASH_REMATCH[4]} / 2 ))"
}

# The backend listens on 6768 by default but scans up to 6790 (check 9). 6767 is
# the desktop's own intercept listener, never the backend, so start the window
# above it.
backend_port() {
  lsof -iTCP -sTCP:LISTEN -nP 2>/dev/null \
    | awk '$1 ~ /(python|headroom)/ && $9 ~ /:(67[6-9][0-9]|6790)$/ {
             split($9, a, ":"); if (a[2] >= 6768) print a[2] }' \
    | sort -n | head -1
}

# Resolve the pid exactly as the doc insists: a bare `lsof -ti :PORT` also
# matches the desktop's client connection and head -1 then returns the desktop.
# Then require the proxy's argv, as pid_is_headroom_backend does: PORT falls
# back to 6768 when no backend is up, and check 12 sends this pid SIGUSR1, whose
# default action terminates whatever unrelated process holds that port.
backend_pid() {
  local pid
  pid=$(lsof -ti "TCP:${1}" -sTCP:LISTEN 2>/dev/null | head -1)
  [ -n "$pid" ] || return 0
  ps -o command= -p "$pid" 2>/dev/null | grep -i headroom | grep -qi proxy && echo "$pid"
  return 0
}

livez() { curl -sS --max-time 3 -o /dev/null -w '%{http_code}' "http://127.0.0.1:6767/livez" 2>/dev/null; }

wait_livez() { # seconds
  local code=""
  for _ in $(seq 1 "$1"); do
    code=$(livez)
    [ "$code" = "200" ] && break
    sleep 1
  done
  echo "$code"
}

# `open -a` against a still-dying instance just activates it, so poll for the
# process to actually go away. The executable is headroom-desktop, not Headroom.
quit_app() {
  osascript -e 'quit app "Headroom"' >/dev/null 2>&1
  for _ in $(seq 1 30); do pgrep -xq headroom-desktop || break; sleep 0.5; done
}

# proxy.log survives backend restarts, so every count in checks 11 and 13 has to
# be scoped to the current boot or pre-fix history keeps them non-zero forever.
# Untimestamped continuation lines inherit the state of the line above them.
since_ts() { # "YYYY-mm-dd HH:MM:SS"
  [ -f "$PROXY_LOG" ] || return 0
  awk -v B="$1" '
    /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] /{ on = (substr($0,1,19) >= B) }
    on' "$PROXY_LOG"
}
since_boot() { since_ts "$BOOT_TS"; }
count_since_boot() { since_boot | grep -c "$1"; }

set_boot_ts() {
  local pid lstart
  pid=$(backend_pid "$PORT")
  BOOT_TS="0000-00-00 00:00:00"
  [ -n "$pid" ] || return 0
  lstart=$(ps -o lstart= -p "$pid" 2>/dev/null)
  [ -n "$lstart" ] || return 0
  BOOT_TS=$(date -j -f "%a %b %e %T %Y" "$lstart" +"%Y-%m-%d %H:%M:%S" 2>/dev/null || echo "0000-00-00 00:00:00")
}

mkdir -p "$SHOTS"
PORT=$(backend_port)
[ -n "$PORT" ] || PORT=6768
set_boot_ts

echo "Headroom beta smoke test (non-Codex)"
echo "  backend port $PORT, booted $BOOT_TS$([ "$QUICK" = 1 ] && echo ', --quick')"
echo

# --- 1. version -------------------------------------------------------------
installed=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist" 2>/dev/null)
expected=$(jq -r '.version // empty' "$REPO/src-tauri/tauri.conf.json" 2>/dev/null)
if [ -z "$installed" ]; then
  row FAIL "1 version" "Info.plist unreadable"
elif [ -z "$expected" ] || [ "$installed" = "$expected" ]; then
  row PASS "1 version" "$installed${expected:+ (matches repo)}"
# A release branch bumps the repo to the stable X.Y.Z while the build under
# test is still its last X.Y.Z-rc.N, so an exact match here would FAIL every
# promotion pass (0.9.18-rc.6 against release/0.9.18, 2026-09-21). Only the
# rc's OWN stable is accepted: a stale rc from an older line still FAILs.
elif [[ "$expected" != *-rc.* && "$installed" == "$expected"-rc.* ]]; then
  row PASS "1 version" "$installed (rc of the repo's $expected; release branch)"
else
  row FAIL "1 version" "installed $installed, repo says $expected"
fi

# --- 2. proxy is intercepting ----------------------------------------------
if [ -f "$CFG/activity-facts.json" ]; then
  age=$(( $(date +%s) - $(stat -f %m "$CFG/activity-facts.json") ))
  if [ "$age" -lt 120 ]; then
    row PASS "2 intercepting" "activity-facts ${age}s old"
  else
    row FAIL "2 intercepting" "activity-facts ${age}s old (needs a live client session)"
  fi
else
  row FAIL "2 intercepting" "activity-facts.json missing"
fi

# --- 3. RTK (opt-in addon; absent is the correct state) ---------------------
if [ -x "$HR/bin/rtk" ]; then
  if zsh -lc 'rtk --version' >/dev/null 2>&1; then
    row PASS "3 rtk" "$(zsh -lc 'rtk --version' 2>/dev/null | head -1)"
  else
    row FAIL "3 rtk" "installed but not on a login shell's PATH"
  fi
else
  row SKIP "3 rtk" "not installed (opt-in addon)"
fi

# --- 4. MCP retrieve --------------------------------------------------------
row MANUAL "4 mcp retrieve" "call mcp__headroom__headroom_retrieve; any structured payload passes"

# --- 8. bundled runtime -----------------------------------------------------
venv="$HR/runtime/venv"
ver=$("$venv/bin/headroom" --version 2>&1 | head -1)
mod=$("$venv/bin/python3" -c "import headroom; print(headroom.__file__)" 2>&1 | head -1)
if [[ "$ver" == *"version"* ]] && [[ "$mod" == *"/site-packages/headroom/__init__.py" ]]; then
  row PASS "8 runtime" "$ver"
else
  row FAIL "8 runtime" "$ver / $mod"
fi
rec=$(jq -r .version "$HR/tools/markitdown.json" 2>/dev/null)
art=$("$venv/bin/markitdown" --version 2>&1 | awk '{print $NF}')
if [ -n "$rec" ] && [ "$rec" = "$art" ]; then
  row PASS "8 addon receipts" "markitdown $rec == $art"
else
  row FAIL "8 addon receipts" "markitdown receipt $rec != artifact $art"
fi
# The Office nudge and the Bash rule name this shim; a path with a space never
# matched the rule (every Office read prompted until 0.9.26-rc.4), so launch
# moves both off Application Support. Full $HR paths: a bare
# "headroom/bin/markitdown" also matches the new ".headroom/bin/...".
if [ -n "$rec" ]; then
  shim="$HOME/.headroom/bin/headroom-markitdown"
  settings="$HOME/.claude/settings.json"
  stale=$(grep -oF -e "Bash($HR/bin/markitdown *)" -e "Bash($HR/tools/markitdown *)" "$settings" 2>/dev/null
    ls -d "$HR/bin/markitdown" "$HR/tools/markitdown" 2>/dev/null)
  rules=$(grep -cF "Bash($shim *)" "$settings" 2>/dev/null)
  if [ -n "$stale" ]; then
    row FAIL "8 markitdown shim" "migration left: $(echo $stale)"
  elif [ ! -x "$shim" ]; then
    row FAIL "8 markitdown shim" "$shim missing or not executable"
  # The Bash rule is a Claude Code setting; a Codex-only machine never gets one.
  elif [ "$(jq -r '.configuredClients.claude_code // empty' "$CFG/client-setup.json" 2>/dev/null)" = "" ]; then
    row PASS "8 markitdown shim" "$shim (Claude Code not configured, no rule expected)"
  elif [ "$(jq -r '.enabled' "$HR/tools/markitdown.json" 2>/dev/null)" = "false" ]; then
    if [ "${rules:-0}" -eq 0 ]; then
      row PASS "8 markitdown shim" "$shim (addon disabled, no rule)"
    else
      row FAIL "8 markitdown shim" "addon disabled but Bash($shim *) is allowed"
    fi
  elif [ "${rules:-0}" -gt 0 ]; then
    row PASS "8 markitdown shim" "$shim + allow rule"
  else
    row FAIL "8 markitdown shim" "no Bash($shim *) rule in $settings"
  fi
fi
# Plugin addons track a marketplace, so a receipt lagging is expected; they only
# have to resolve to a version string at all.
unresolved=$(jq -r '.plugins | to_entries[] | select((.value[0].version // "?") == "?") | .key' \
  "$HOME/.claude/plugins/installed_plugins.json" 2>/dev/null)
if [ -z "$unresolved" ]; then
  row PASS "8 plugins resolve" "$(jq -r '.plugins | length' "$HOME/.claude/plugins/installed_plugins.json" 2>/dev/null) plugins"
else
  row FAIL "8 plugins resolve" "unresolved: $(echo "$unresolved" | tr '\n' ' ')"
fi

# --- 10. auth / pricing -----------------------------------------------------
if security find-generic-password -s com.extraheadroom.headroom.account -a session-token >/dev/null 2>&1; then
  seen=$(jq -r '.first_seen_at // empty' "$CFG/headroom-pricing-state.json" 2>/dev/null)
  if [ -n "$seen" ] && [ "$seen" != "null" ]; then
    row PASS "10 auth/pricing" "signed in, first_seen_at $seen"
  else
    row FAIL "10 auth/pricing" "signed in but first_seen_at missing"
  fi
else
  row FAIL "10 auth/pricing" "not signed in (regression if this build was)"
fi

# --- 11. computed transforms reached the wire -------------------------------
if [ -f "$PROXY_LOG" ]; then
  c11a=$(count_since_boot 'ccr_streaming_retrieve_buffered[^ ]* source=passthrough')
  c11b=$(count_since_boot 'body_mutated=true.*source=passthrough')
  [ "$c11a" = "0" ] && row PASS "11a empty-200 class" "0" \
    || row FAIL "11a empty-200 class" "$c11a streaming CCR passthroughs since boot"
  if [ "$c11b" = "0" ]; then
    row PASS "11b discard rate" "0"
  else
    reasons=$(since_boot | grep 'body_mutated=true.*source=passthrough' \
      | sed -n 's/.*mutation_reasons=\([^ ]*\).*/\1/p' | tr ',' '\n' | sort | uniq -c \
      | sort -rn | head -3 | awk '{printf "%s x%s ", $2, $1}')
    row FAIL "11b discard rate" "$c11b discarded since boot: $reasons"
  fi
else
  row FAIL "11 wire truth" "proxy.log missing at $PROXY_LOG"
fi

# --- 13. billed for an unusable response ------------------------------------
if [ -f "$PROXY_LOG" ]; then
  c13a=$(count_since_boot 'PERF model=claude-[^ ]* .*tok_out=0 ')
  c13b=$(count_since_boot 'response_cache_store_refused')
  c13c=$(since_boot | grep 'PERF model=claude-' | awk '
    { for (i = 1; i <= NF; i++) {
        if ($i ~ /^total_ms=/) t = substr($i, 10) + 0
        if ($i ~ /^tok_out=/)  o = substr($i, 9) + 0 }
      if (t < 20 && o == 0) n++ } END { print n + 0 }')
  # tok_out=0 carries no status, so an upstream error passed through looks the
  # same. Non-zero is a tripwire to go read the matching inbound_response line.
  [ "$c13a" = "0" ] && row PASS "13 tok_out=0" "0" \
    || row FAIL "13 tok_out=0" "$c13a since boot - join to inbound_response by timestamp; 200 = bug, 4xx/5xx = benign"
  [ "$c13b" = "0" ] && row PASS "13 cache store guard" "0" \
    || row FAIL "13 cache store guard" "$c13b refusals - treat as a wheel-bump regression"
  [ "$c13c" = "0" ] && row PASS "13 poisoned replay" "0" \
    || row FAIL "13 poisoned replay" "$c13c sub-20ms empty 200s (only a restart clears one)"
fi

# --- 14. user state survived the upgrade ------------------------------------
PRE="$CFG/pre-update"
if [ -f "$PRE/meta.json" ]; then
  from=$(jq -r '.from_version // "?"' "$PRE/meta.json")
  to=$(jq -r '.to_version // "?"' "$PRE/meta.json")
  if [ "$to" != "$installed" ]; then
    row FAIL "14 snapshot freshness" "snapshot is $from -> $to but $installed is installed"
  else
    row PASS "14 snapshot freshness" "$from -> $to"
    a=$(jq -c '.first_seen_at' "$CFG/headroom-pricing-state.json" 2>/dev/null)
    b=$(jq -c '.first_seen_at' "$PRE/headroom-pricing-state.json" 2>/dev/null)
    [ "$a" = "$b" ] && row PASS "14 first_seen_at" "$a" \
      || row FAIL "14 first_seen_at" "now $a, was $b"
    # The auto-snapshot is taken post-quit, where clear_client_setups() has
    # already emptied configuredClients - rememberedClients is the surviving set.
    a=$(jq -c '.rememberedClients|keys' "$CFG/client-setup.json" 2>/dev/null)
    b=$(jq -c '.rememberedClients|keys' "$PRE/client-setup.json" 2>/dev/null)
    [ "$a" = "$b" ] && row PASS "14 remembered clients" "$a" \
      || row FAIL "14 remembered clients" "now $a, was $b"
    # A schemaVersion bump intentionally drops tile slots, so diff only the two
    # fields that must survive one: wiping them re-fires the weekly recap and
    # resets all-time records for every user.
    a=$(jq -c '{t:.allTimeRecordTokens,r:.lastWeeklyRecapWeekKey}' "$CFG/activity-facts.json" 2>/dev/null)
    b=$(jq -c '{t:.allTimeRecordTokens,r:.lastWeeklyRecapWeekKey}' "$PRE/activity-facts.json" 2>/dev/null)
    [ "$a" = "$b" ] && row PASS "14 records/recap" "$a" \
      || row FAIL "14 records/recap" "now $a, was $b"
  fi
else
  row FAIL "14 snapshot freshness" "no $PRE/meta.json (build <0.9.3, or first launch never happened)"
fi
# grep -c, not `ls *.corrupt`: zsh aborts the line on an empty glob, which is
# the healthy case.
corrupt=$(ls "$CFG" 2>/dev/null | grep -c '\.corrupt$')
[ "$corrupt" = "0" ] && row PASS "14 quarantine files" "0" \
  || row FAIL "14 quarantine files" "$corrupt - jq the .corrupt file, fix goes on the struct"

# --- 15. CLAUDE.md intact ---------------------------------------------------
for f in "$HOME/.claude/CLAUDE.md" "$REPO/CLAUDE.md"; do
  [ -f "$f" ] || continue
  ms=$(grep -c '^# >>> headroom:markitdown_office >>>' "$f")
  me=$(grep -c '^# <<< headroom:markitdown_office <<<' "$f")
  ls_=$(grep -c '<!-- headroom:learn:start -->' "$f")
  le=$(grep -c '<!-- headroom:learn:end -->' "$f")
  bytes=$(awk '/headroom:(learn:start|markitdown_office >>>)/{skip=1} !skip{n+=length($0)+1} /headroom:(learn:end|markitdown_office <<<)/{skip=0} END{print n+0}' "$f")
  label="15 $(basename "$(dirname "$f")")/CLAUDE.md"
  if [ "$ms" = "$me" ] && [ "$ls_" = "$le" ] && [ "$ms" -le 1 ] && [ "$ls_" -le 1 ]; then
    row PASS "$label" "markitdown $ms/$me, learn $ls_/$le, ${bytes}B user content"
  else
    row FAIL "$label" "markitdown $ms/$me, learn $ls_/$le (2/2 = duplicated block, 1/0 = truncated write)"
  fi
done

# --- 16. lifetime card covers saved today -----------------------------------
ring=$(curl -s "http://127.0.0.1:6767/stats-history" \
  | jq -r --arg d "$(date -u +%Y-%m-%d)" \
      '[.series.daily[] | select(.timestamp | startswith($d)) | .compression_savings_usd_delta] | add // 0' 2>/dev/null)
if [ -n "$ring" ]; then
  row PASS "16 ring today (UTC)" "\$$ring compression - chart's 'saved today' must be this plus shaping, not a fraction of it"
else
  row FAIL "16 ring today (UTC)" "/stats-history unreadable"
fi

# --- 5. dashboard opens -----------------------------------------------------
open -a Headroom >/dev/null 2>&1
sleep 2
open_tray_menu
sleep 1
menu=$(AX 'to get name of every menu item of menu 1 of menu bar item 1 of menu bar 2')
AX 'to click menu item "Show Headroom" of menu 1 of menu bar item 1 of menu bar 2' >/dev/null
# The main window is a tray popover: it hides 150ms after losing focus
# (MAIN_WINDOW_BLUR_HIDE_DELAY_MS), so anyone clicking elsewhere during a fixed
# sleep scores a healthy build as "no window" (0.9.11-rc.5 quick re-run). Read
# the geometry and shoot the moment it is there (measured +0.3s), poll up to 5s.
geom=""
for _ in $(seq 1 20); do
  geom=$(AX 'to get {position, size} of window 1' | tr -d ' ')
  [[ "$geom" =~ ^([0-9-]+),([0-9-]+),([0-9]+),([0-9]+)$ ]] && break
  sleep 0.25
done
if [[ "$menu" == *"Headroom"* ]] && [[ "$geom" =~ ^([0-9-]+),([0-9-]+),([0-9]+),([0-9]+)$ ]]; then
  x=${BASH_REMATCH[1]}; y=${BASH_REMATCH[2]}; w=${BASH_REMATCH[3]}; h=${BASH_REMATCH[4]}
  shot="$SHOTS/dashboard-$(date +%H%M%S).png"
  screencapture -x -o -R"$x,$y,$w,$h" "$shot" 2>/dev/null
  row PASS "5 dashboard opens" "${w}x${h} window; eyeball $shot"
  row MANUAL "16 lifetime >= today" "in that screenshot: 'Total costs saved' >= chart's 'saved today'"
else
  row FAIL "5 dashboard opens" "no window after Show Headroom (menu: $menu)"
fi

# --- disruptive block -------------------------------------------------------
if [ "$QUICK" = 1 ]; then
  row SKIP "9 port fallback" "--quick"
  row SKIP "6 pause/resume" "--quick"
  row SKIP "12 sitecustomize imported" "--quick"
else
  # --- 9. backend port fallback when 6768 is held ---------------------------
  quit_app
  python3 -c "import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',6768)); s.listen(16); time.sleep(180)" &
  blocker=$!
  sleep 1
  open -a Headroom >/dev/null 2>&1
  # A fallback port boots cold (memory tools, model load), so poll rather than
  # sleep, and keep --max-time on every curl: one hung curl against a
  # half-booted intercept strands the whole script.
  code=$(wait_livez 90)
  fb=$(backend_port)
  kill "$blocker" 2>/dev/null
  wait "$blocker" 2>/dev/null
  if [ "$code" = "200" ] && [ -n "$fb" ] && [ "$fb" != "6768" ]; then
    row PASS "9 port fallback" "6768 held, backend took $fb"
  else
    row FAIL "9 port fallback" "livez=$code, backend port=${fb:-none} (wanted != 6768)"
  fi
  quit_app
  open -a Headroom >/dev/null 2>&1
  code=$(wait_livez 90)
  PORT=$(backend_port); [ -n "$PORT" ] || PORT=6768
  set_boot_ts
  [ "$code" = "200" ] && [ "$PORT" = "6768" ] \
    && row PASS "9 port restored" "back on 6768" \
    || row FAIL "9 port restored" "livez=$code, port=$PORT"

  # --- 6. pause / resume ----------------------------------------------------
  before=$(cat "$HOME/.zprofile" "$HOME/.zshrc" 2>/dev/null | grep -c 'headroom:claude_code')
  open_tray_menu; sleep 1
  AX 'to click menu item "Pause Headroom" of menu 1 of menu bar item 1 of menu bar 2' >/dev/null
  sleep 5
  paused=$(cat "$HOME/.zprofile" "$HOME/.zshrc" 2>/dev/null | grep -c 'headroom:claude_code')
  open_tray_menu; sleep 1
  AX 'to click menu item "Resume Headroom" of menu 1 of menu bar item 1 of menu bar 2' >/dev/null
  # Resume is start_headroom -> resume_runtime -> ensure_headroom_running, and
  # only then the restore thread, so this waits on a cold Python boot (15-20s
  # measured). A fixed sleep here scores a healthy build as a FAIL.
  for _ in $(seq 1 60); do
    [ "$(cat "$HOME/.zprofile" "$HOME/.zshrc" 2>/dev/null | grep -c 'headroom:claude_code')" = "$before" ] && break
    sleep 1
  done
  resumed=$(cat "$HOME/.zprofile" "$HOME/.zshrc" 2>/dev/null | grep -c 'headroom:claude_code')
  if [ "$before" -gt 0 ] && [ "$paused" = "0" ] && [ "$resumed" = "$before" ]; then
    row PASS "6 pause/resume" "$before -> 0 -> $resumed markers"
  else
    row FAIL "6 pause/resume" "$before -> $paused -> $resumed markers (wanted N -> 0 -> N)"
  fi
  wait_livez 60 >/dev/null
  PORT=$(backend_port); [ -n "$PORT" ] || PORT=6768
  set_boot_ts
fi

# --- 12. the running proxy has the configured flags and patches -------------
pid=$(backend_pid "$PORT")
if [ -z "$pid" ]; then
  row FAIL "12 backend pid" "no headroom proxy listening on $PORT"
else
  noccr=$(ps -o args= -p "$pid" | tr ' ' '\n' | grep -c -- '--no-ccr')
  inj=$(ps eww -o command= -p "$pid" | grep -c 'pyinject')
  guard=$(grep -c '_hd_sc_cacheable' "$HR/pyinject/sitecustomize.py" 2>/dev/null)
  # 0.9.6 re-enabled CCR, so --no-ccr is opt-in now; a 1 means the kill switch
  # is set for this session, not that the build is broken.
  [ "$noccr" = "0" ] && row PASS "12 ccr enabled" "no --no-ccr (default)" \
    || row FAIL "12 ccr enabled" "--no-ccr present, CCR is OFF this session"
  [ "$inj" = "1" ] && row PASS "12 pyinject on path" "PYTHONPATH set" \
    || row FAIL "12 pyinject on path" "pyinject not on the backend's env"
  [ "${guard:-0}" -gt 0 ] && row PASS "12 cache guard on disk" "$guard hits" \
    || row FAIL "12 cache guard on disk" "_hd_sc_cacheable absent from sitecustomize.py"

  if [ "$QUICK" = 1 ]; then
    : # SIGUSR1 already reported as skipped above
  else
    # Definitive proof sitecustomize was IMPORTED, not merely present. The dump
    # lands in the per-boot log, not ~/.headroom/logs/proxy.log. Destructive
    # when injection did not happen: Python has no handler and the OS default
    # terminates the proxy. Acceptable on a beta box, cheap to restart.
    name=$(ls -t "$HR/logs" 2>/dev/null | grep -m1 "headroom-proxy---port-${PORT}-")
    perboot="$HR/logs/$name"
    was=$(grep -c 'Thread 0x' "$perboot" 2>/dev/null); was=${was:-0}
    kill -USR1 "$pid" 2>/dev/null
    sleep 2
    now=$(grep -c 'Thread 0x' "$perboot" 2>/dev/null); now=${now:-0}
    if [ -z "$name" ]; then
      row FAIL "12 sitecustomize imported" "no per-boot log for port $PORT"
    elif kill -0 "$pid" 2>/dev/null && [ "$now" -gt "$was" ]; then
      row PASS "12 sitecustomize imported" "survived SIGUSR1, dumped $((now - was)) thread lines"
    elif kill -0 "$pid" 2>/dev/null; then
      row FAIL "12 sitecustomize imported" "survived SIGUSR1 but no thread dump in $(basename "$perboot")"
    else
      row FAIL "12 sitecustomize imported" "proxy terminated - sitecustomize was never imported"
    fi
  fi
fi

# --- 7. actively optimizing (needs a large Read between two runs) -----------
# Runs after the disruptive block on purpose: /stats is in-memory per backend
# boot, so a baseline taken before checks 6/9 restart it scores the --quick
# re-run against reset counters (0.9.11-rc.5 pass).
snap=$(curl -s "http://127.0.0.1:6767/stats" | jq -c --arg ts "$(date +"%Y-%m-%d %H:%M:%S")" '{
  ts: $ts,
  frozen: (.summary.uncompressed_requests.prefix_frozen // 0),
  compressed: (.summary.compression.requests_compressed // 0),
  before: (.summary.compression.total_tokens_before // 0),
  claude: (.requests.by_model // {} | with_entries(select(.key|startswith("claude-"))) | to_entries | map(.value) | add // 0)
}' 2>/dev/null)
stale=0
if [ -n "$snap" ] && [ "$snap" != "null" ] && [ -f "$BASELINE" ]; then
  o=$(cat "$BASELINE")
  d() { echo "$1" | jq -r ".$2"; }
  dc=$(( $(d "$snap" claude) - $(d "$o" claude) ))
  db=$(( $(d "$snap" before) - $(d "$o" before) ))
  dr=$(( ($(d "$snap" frozen) + $(d "$snap" compressed)) - ($(d "$o" frozen) + $(d "$o" compressed)) ))
  # /stats counters only grow within one backend boot, so a negative delta
  # means the baseline predates a restart (a leftover file from an earlier pass
  # scored -28 reqs / -3.1M tok_before on 0.9.22-rc.6). Re-baseline, not FAIL.
  [ "$dc" -lt 0 ] || [ "$db" -lt 0 ] || [ "$dr" -lt 0 ] && stale=1
fi
if [ -z "$snap" ] || [ "$snap" = "null" ]; then
  row FAIL "7 optimizing" "/stats unreadable"
elif [ "$stale" = 1 ]; then
  echo "$snap" > "$BASELINE"
  row PENDING "7 optimizing" "baseline predates a backend restart; re-saved. Do a ~1350-line Read, then re-run with --quick"
elif [ -f "$BASELINE" ]; then
  # Cache side of the trade, per request from the proxy log since the baseline.
  # cost.py's cache_savings_usd is net of the write premium over a window and
  # moves both ways by design, so it cannot be a gate (doc, check 7).
  read -r pn ptb pts pcr pcw <<<"$(since_ts "$(d "$o" ts)" | grep 'PERF model=claude-' \
    | awk '{ for (i = 1; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] }
             n++; tb += v["tok_before"]; sv += v["tok_saved"]; cr += v["cache_read"]; cw += v["cache_write"] }
           END { printf "%d %d %d %d %d\n", n, tb, sv, cr, cw }')"
  dropped=$(since_ts "$(d "$o" ts)" | grep -c 'event=cache_breakpoints.*dropped=true')
  cache_ok=1; [ -f "$PROXY_LOG" ] && [ "${pcr:-0}" -eq 0 ] && cache_ok=0
  if [ "$dc" -gt 0 ] && [ "$db" -gt 0 ] && [ "$dr" -ge 1 ] && [ "$cache_ok" = 1 ]; then
    row PASS "7 optimizing" "+$dc claude reqs, +$db tok_before, +$dr compressed/frozen"
  else
    row FAIL "7 optimizing" "+$dc claude reqs, +$db tok_before, +$dr compressed/frozen, cache_read since baseline=${pcr:-0}"
  fi
  if [ "${pn:-0}" -gt 0 ]; then
    row NOTE "7 cache side" "$pn claude reqs: cache_read/req=$(( pcr / pn )), cache_write/req=$(( pcw / pn )), tok_saved/tok_before=$(( pts * 100 / (ptb > 0 ? ptb : 1) ))%, tail breakpoint dropped on $dropped (held Read parks it: read_maturation, see doc)"
  fi
  rm -f "$BASELINE"
else
  echo "$snap" > "$BASELINE"
  row PENDING "7 optimizing" "baseline saved; do a ~1350-line Read (the tool, not cat), then re-run with --quick"
fi

echo
n_pass=$(printf '%s\n' "${ROWS[@]}" | grep -c '^PASS|')
n_fail=$(printf '%s\n' "${ROWS[@]}" | grep -c '^FAIL|')
n_other=$(( ${#ROWS[@]} - n_pass - n_fail ))
echo "  $n_pass pass, $n_fail fail, $n_other skipped/manual/pending"
[ "$FAILED" = "1" ] && echo "  Do not promote. See docs/beta-smoke-test.md for what each check means."
exit "$FAILED"
