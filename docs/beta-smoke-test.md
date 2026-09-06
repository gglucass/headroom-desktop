# Beta smoke test

After installing a new beta (`-rc.N`) build, paste this file into Claude Code and ask it to run the checks. Each check has a single expected signal — if any fail, stop and investigate before promoting to stable.

## Setup

1. Quit and relaunch Headroom from Applications.
2. Confirm the tray icon appears in the menu bar.
3. Open the dashboard window once (so the proxy is fully booted).

## Checks (Claude Code pass)

Run these from a Claude Code session and report PASS / FAIL with the observed value. Check 14 has a step that must run **before** you install the rc - read it first. Checks 1, 5, 8, 9, 10, 11, 12, 14, 15, and 16 are client-agnostic — run them once in either client. Codex has very different wiring (no RTK, no `~/.claude/settings.json`, pay-per-token), so its equivalents of checks 6 and 7 live in the **Codex pass** below; run that whole section from a Codex session.

### 1. Version matches the new beta
```bash
/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" /Applications/Headroom.app/Contents/Info.plist
```
Expect: the `-rc.N` version you just installed.

### 2. Proxy is intercepting this conversation
Send a trivial prompt ("say hi"), then:
```bash
stat -f '%Sm' ~/Library/Application\ Support/Headroom/config/activity-facts.json
```
Expect: mtime within the last minute. `lastTransformation` inside the file is a "Recent large compression" tile pick (gated on >=1000 tokens saved and >20% savings, see `activity_facts.rs`), not a per-request heartbeat — don't use it as a liveness signal.

### 3. RTK is on PATH and reports savings (Claude Code only — RTK does not rewrite Codex)
First check whether RTK is installed at all. RTK is an opt-in addon (since 7a0f489, 2026-06-23): bootstrap never installs it, so a fresh install has no `rtk` until the user adds it from the Addons tab - and it can additionally be turned off from the Optimize view (`rtkDisabled` in the setup state; `is_rtk_disabled` in `client_adapters.rs`). Either way a missing `rtk` is the correct state, not a regression. Do NOT gate on `rtkDisabled` alone: `false` only means "never explicitly disabled" and says nothing about whether it was ever installed (the 0.9.1-rc.2 Windows pass false-flagged on exactly this). Gate on the binary:
```bash
ls ~/Library/Application\ Support/Headroom/headroom/bin/rtk >/dev/null 2>&1 \
  && echo "RTK installed - run check" || echo "RTK NOT INSTALLED (opt-in addon) - skip this check"
```
If enabled:
```bash
zsh -lc 'rtk --version && rtk gain | head -5'
```
Expect: a version line and a gain summary, no "command not found". The `zsh -lc` wrapper is required: `rtk` is added to PATH by the `headroom:managed_rtk` block in `~/.zprofile`, which only a login shell sources. Claude Code's Bash tool (and Codex's shell tool) spawn a non-login, non-interactive shell that does *not* source it, so a bare `rtk` here reports `command not found` on a perfectly healthy install. A login shell exercises the same PATH wiring a real terminal gets, so this confirms both that the managed block is intact and that the binary runs.

### 4. MCP retrieve tool is available (Claude Code only)
Have Claude call `mcp__headroom__headroom_retrieve` with any small query and expect a structured tool result - an "expired or incorrect hash" error payload is a PASS; only "No such tool available" fails. Do not gate this on the proxy's `no-memory-tools` flag: MCP registration is independent of it (observed on the 0.9.3-rc.5 pass - the live proxy log carried `no-memory-tools` and the tool still answered), and the old log-filename gate here also matched rotated logs from old boots. If you want the flag state anyway, read the *newest* `headroom-proxy---port-` filename - it is informational, not a skip condition.

### 5. Tray → Dashboard renders
Click the tray icon, open the dashboard. Expect savings chart and per-client stats render without a blank/error state.

### 6. Pause / resume cleanly strips and restores interception
In Settings, toggle Pause then Resume (restore runs on a background thread, so give it a second), checking after each:
```bash
grep -c 'headroom:claude_code' ~/.zprofile ~/.zshrc
```
Expect: after Pause both files print `0`; after Resume both print `2` (the `# >>> headroom:claude_code >>>` and `# <<< headroom:claude_code <<<` marker lines). Do *not* grep `~/.claude/settings.json` for `headroom-rtk-rewrite` — that hook only exists when the RTK addon is installed, so on an install with RTK off (check 3) it reads `0` in both states and the check looks like a FAIL on a healthy build. The managed shell block is the RTK-independent marker. If RTK *is* installed, `grep -c headroom-rtk-rewrite ~/.claude/settings.json` is a valid extra signal: `0` after Pause, `1` after Resume.

This verifies the Claude Code config only — Pause clears *all* clients, so check C4 in the Codex pass confirms Codex's config is stripped and restored too.

### 7. Proxy is actively optimizing this conversation (not just a heartbeat)
The proxy always runs in `token` mode now (`HEADROOM_MODE=token`, hardcoded — cache mode and the old auth-based mode auto-switch were removed; see `tool_manager.rs`). So `.summary.mode` reports `token` for *every* session, including a Claude Code subscription/OAuth one — don't branch on it. What actually differs per request is the **compression policy**, chosen by the auth-mode classifier (`classify_auth_mode` in the proxy) from the client `User-Agent`:

- **Claude Code subscription/OAuth** (UA `claude-code/`, the normal desktop case) is classified `SUBSCRIPTION` → conservative policy (`live_zone_only=True`, cache-aligner off). The proxy only compresses the **uncached live zone** and freezes the already-cached prefix. Which counter moves depends on the live zone: a request with any live-zone savings counts in `requests_compressed`, while `uncompressed_requests.prefix_frozen` only counts requests the pipeline returned fully unchanged — zero savings (see `build_session_summary` in the proxy's `cost.py`). The right liveness signal here is `cache_savings_usd` plus movement in *either* counter.
- **Pay-per-token API-key / Codex traffic** is classified `PAYG`/`OAUTH` → aggressive policy, so `requests_compressed` and `total_tokens_removed` move directly.

This policy gate is itself guarded by `HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT=enabled` (pinned explicitly in `tool_manager.rs`). If that ever reads disabled, subscription traffic silently falls back to the PAYG-aggressive policy and starts busting the prefix cache — a net loss on cache-billed sessions. Pick the sub-check matching the traffic you're driving.

Timing matters either way: a `Read` result becomes part of Claude's *next* outgoing prompt, not the one currently being composed. So the baseline capture, the large Read, and the re-check cannot all happen in one turn — the re-check will still show the old numbers.

Generate the payload with a real `Read` tool call. Dumping the file through Bash (`cat`, `sed`) does not work: the harness persists oversized command output to disk and only a ~2KB preview enters the next prompt, so the proxy never sees the bulk (observed on the 0.9.3-rc.5 Windows pass).

**Claude Code subscription/OAuth traffic** (UA `claude-code/`, classified `SUBSCRIPTION`):
1. Capture the baseline:
   ```bash
   rtk proxy curl -s http://127.0.0.1:6767/stats | jq '{primary_model: .summary.primary_model, prefix_frozen: .summary.uncompressed_requests.prefix_frozen, requests_compressed: .summary.compression.requests_compressed, cache_savings_usd: .summary.cost.breakdown.cache_savings_usd, total_tokens_before: .summary.compression.total_tokens_before}'
   ```
2. End the turn with a large Read in flight — e.g. ask Claude to read a long file like `src-tauri/src/lib.rs` with as large an offset/limit window as the Read tool allows (the 25k-token cap means you cannot read it whole; ~1300-1500 lines is plenty).
3. On the *next* turn, re-run the same `jq` command.

Expect: `primary_model` is a `claude-*` model, `cache_savings_usd` is strictly greater (the cached prefix was preserved, not busted), `total_tokens_before` jumped by at least the size of the Read, and `prefix_frozen` + `requests_compressed` together increased by at least 1. The large Read all but guarantees live-zone savings, so in practice the increment lands in `requests_compressed`; `prefix_frozen` only counts requests returned fully unchanged, so it can legitimately stay flat for a whole session (observed on a healthy 0.6.9-rc.1: `prefix_frozen` flat at 17 while cache savings climbed). A bumped mtime on `activity-facts.json` is not enough — interception alone would still touch that file without delivering savings.

**Pay-per-token API-key traffic** (classified `PAYG`/`OAUTH` — this is also the branch Codex hits; the Codex pass below adds a Codex-attributed version):
1. Capture the baseline:
   ```bash
   rtk proxy curl -s http://127.0.0.1:6767/stats | jq '.summary.compression.requests_compressed, .summary.compression.total_tokens_removed'
   ```
2. End the turn with the same large Read in flight (~1300-1500 lines clears the compression threshold).
3. On the *next* turn, re-run the same `jq` command.

Expect: `requests_compressed` increased by at least 1, and `total_tokens_removed` is strictly greater.

### 8. Bundled runtime is healthy
The desktop ships its own Python venv and `headroom` CLI; if either is broken, the proxy can't start cleanly on a fresh install.
```bash
~/Library/Application\ Support/Headroom/headroom/runtime/venv/bin/headroom --version && \
  ~/Library/Application\ Support/Headroom/headroom/runtime/venv/bin/python3 -c "import headroom; print(headroom.__file__)"
```
Expect: a `headroom, version X.Y.Z` line and a path under `.../runtime/venv/lib/python3.12/site-packages/headroom/__init__.py`. No `ModuleNotFoundError`, no `pydantic-core` mismatch traceback (see `extract_required_pydantic_core_version` in `tool_manager.rs` for the exact failure mode).

Addons share that venv, so a wheel bump can resolve a transitive dependency out from under one of them without touching `headroom` itself. Each pinned addon's receipt must still match what its artifact actually reports - the update logic compares the receipt against the pin, so a stale receipt makes it offer (or withhold) an update on a false premise:
```bash
R=~/Library/Application\ Support/Headroom/headroom
echo "receipt: $(jq -r .version "$R/tools/markitdown.json")  artifact: $("$R/bin/markitdown" --version)"
jq -r '.plugins | to_entries[] | "\(.key): \(.value[0].version // "?")"' ~/.claude/plugins/installed_plugins.json
```
Expect: the two markitdown versions are equal, and each plugin resolves to a version string. Plugin addons (ponytail, caveman) are the deliberate exception - `PLUGIN_DISPLAY_VERSION` is the literal `latest`, they track a marketplace rather than a pin, and `installed_addon_version` reads `installed_plugins.json` rather than the receipt. A plugin receipt lagging the installed version is therefore expected and not a failure; only the pinned addons (markitdown, serena, context7, codebase-memory) must agree.

### 9. Backend port fallback when 6768 is held
The desktop's internal proxy port (default `6768`) can be claimed by other macOS processes — most often `rapportd` at login. The desktop should scan `6769..=6790` and pick a free one instead of failing.

First, confirm the live port and verify the proxy answers there:
```bash
lsof -iTCP -sTCP:LISTEN -nP 2>/dev/null | awk '$1 ~ /(headroom|python)/ && $9 ~ /:(67[6-9][0-9]|6790)/ { print $9 }'
curl -sS --max-time 5 -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:6767/livez"
```
Expect: at least one `127.0.0.1:67XX` line in the 6768-6790 range, and the curl returns `200`.

Then, force a fallback. Quit Headroom, hold 6768 with a Python blocker (`nc -l` exits after one connection, so the proxy's first probe frees the port before fallback can trigger), relaunch, and confirm the proxy comes up on a different port. Three timing traps here. First: the quit must wait for the process to actually die — teardown takes 2s+ (`stop_headroom`'s bounded SIGTERM wait plus Codex thread retagging), and `open -a` against a still-dying instance just activates it, so nothing relaunches and the check strands with no app running (a fixed `sleep 2` loses this race; the executable is `headroom-desktop`, not `Headroom`, so poll with `pgrep -x headroom-desktop`). Second: the proxy on a fallback port boots cold (memory tools / model load), so poll `/livez` for up to 90s instead of a fixed sleep. Third: every curl in the poll loop needs `--max-time` — against a half-booted intercept a timeout-less curl can hang for minutes, and with the backgrounded blocker still holding the shell's stdout open, one hung curl strands the whole script past any outer timeout even after the fallback has already succeeded (observed on the 0.7.6-rc.1 pass):
```bash
osascript -e 'quit app "Headroom"' 2>/dev/null
for _ in $(seq 1 30); do pgrep -xq headroom-desktop || break; sleep 0.5; done
python3 -c "import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',6768)); s.listen(16); time.sleep(180)" &
BLOCK_PID=$!
sleep 1
open -a Headroom
for _ in $(seq 1 90); do
  code=$(curl -sS --max-time 3 -o /dev/null -w '%{http_code}' "http://127.0.0.1:6767/livez" 2>/dev/null)
  [ "$code" = "200" ] && break
  sleep 1
done
echo "livez=$code"
lsof -iTCP -sTCP:LISTEN -nP 2>/dev/null | awk -v IGNORECASE=1 '$1 ~ /(headroom|python)/ && $9 ~ /:(67[6-9][0-9]|6790)/ { print $9 }'
kill $BLOCK_PID 2>/dev/null
```
Expect: `livez=200`, a `127.0.0.1:67XX` line where `XX` is NOT `68` (the fallback worked). A second confirmation is the proxy log *filename*, which embeds the chosen port — a successful fallback leaves a `headroom-proxy---port-6769---....log` next to the usual `...port-6768...` one:
```bash
ls -t ~/Library/Application\ Support/Headroom/headroom/logs/ | grep -m3 'headroom-proxy---port-'
```
After the test, restore the default port with the same wait-for-exit pattern (this relaunch loses the teardown race even more often, because the fallback instance was just cold-booted):
```bash
osascript -e 'quit app "Headroom"' 2>/dev/null
for _ in $(seq 1 30); do pgrep -xq headroom-desktop || break; sleep 0.5; done
open -a Headroom
```

If the fallback is missing, check the desktop log (`logging::log_path()`) for a `[backend_port]` warning line naming the occupant and the chosen fallback port. Note that the *proxy* logs under `.../headroom/logs/` never carry that line — it is emitted by the Rust side, so grepping the proxy log directory for `backend_port` comes back empty even on a successful fallback.

### 10. Auth / pricing state is intact
The session token lives in the macOS keychain under service `com.extraheadroom.headroom.account`, account `session-token`; the local pricing state lives next to `activity-facts.json`.
```bash
security find-generic-password -s com.extraheadroom.headroom.account -a session-token >/dev/null 2>&1 && echo 'signed in' || echo 'not signed in'
test -f ~/Library/Application\ Support/Headroom/config/headroom-pricing-state.json && jq -e '.first_seen_at' ~/Library/Application\ Support/Headroom/config/headroom-pricing-state.json
```
Expect: if the build is supposed to be signed in, line 1 reports `signed in`; line 2 prints a non-null `first_seen_at` timestamp. A signed-in build that flips to `not signed in` after relaunch is a regression — keychain access is broken or the token was wiped.

### 11. Computed transforms actually reached the wire

Checks 2 and 7 confirm the proxy *reports* savings. They cannot tell you the optimized body was the one sent - the proxy can compress a request, log the savings, and then forward the client's original bytes. This is the failure class behind the `API returned an empty or malformed response (HTTP 200)` incident on 2026-08-13 (upstream #2952/#2953) and the wider silent-discard bug (#2990). It is invisible in `/stats`, on the dashboard, and in `activity-facts.json`, because every one of those reads the *pre-send* accounting.

The proxy log settles it on a single line. `source=` is the bytes actually forwarded and `mutation_reasons=` is what the pipeline changed, so `body_mutated=true ... source=passthrough` is a literal contradiction: work was done and the original bytes went out anyway.

**11a. The empty-200 class must be gone (hard FAIL).** Scope to the *current* log - rotated logs still hold pre-fix history and will report non-zero forever.
```bash
grep -c 'ccr_streaming_retrieve_buffered[^ ]* source=passthrough' \
  ~/.headroom/logs/proxy.log
```
Expect: `0`. Anything above zero means a streaming CCR request forwarded `stream:true` bytes on a buffered path, and the client is about to receive an unparseable, unretryable 200. Verified discriminating: `0` on the current log, `6` across the pre-fix rotated logs.

**11b. General discard rate (hard FAIL since the headroom-ai 0.37.0 wheel).**
```bash
grep -c 'body_mutated=true.*source=passthrough' ~/.headroom/logs/proxy.log
grep 'body_mutated=true.*source=passthrough' ~/.headroom/logs/proxy.log \
  | sed -n 's/.*mutation_reasons=\([^ ]*\).*/\1/p' | tr ',' '\n' | sort | uniq -c
```
Expect: `0` on the current wheel - the signed-thinking discard fix (upstream #3015, merged upstream in v0.36.0) ships here since the headroom-ai 0.37.0 bump in 0.9.4-rc.1. Scope the count to lines timestamped after the current backend booted: `proxy.log` persists across backend restarts, so pre-bump history keeps the whole-file grep non-zero forever (observed on the 0.9.4-rc.1 pass: 148 `output_shaper` discards from the same morning's 0.35.0 run, 0 since the rc boot). Boot time via `ps -o lstart= -p $(lsof -ti TCP:6768 -sTCP:LISTEN | head -1)`, then filter on the log timestamp before counting.

`structural_diff_vs_original` is the one to read first, not last. The core compression pipeline never calls `mark_mutated` - grep the package and the reason vocabulary has no entry for it - so compression is only ever noticed by the structural safety net at the end of `handle_anthropic_messages`, which fires when no transform reported a mutation but the final body differs from the parsed original bytes. That makes this reason the label core compression lands under by omission, and a discard under it is the pipeline's own work being thrown away, which costs more than losing a shaping pass. It also means the share is not fixed: measured 7.6%-60% of mutated requests across six logs on 0.8.5-rc.1, tracking how much real compression happened. Do not read a ~3% reading as the `HEADROOM_OUTPUT_HOLDOUT=0.03` control arm - the arm gate and this one are unrelated, and the resemblance is a coincidence of whichever subset you counted.

Do not try to derive this from PERF `transforms=` instead. That field includes detector-only and no-op entries (`router:noop`, `router:protected:error_output`), so joining it against `source=` reports ~1,100 false positives per log file. `mutation_reasons` is only written when the body actually changed.

### 12. The running proxy has the flags and patches the desktop configured

The same class as check 11, one layer down: the desktop can *decide* on a flag and the live proxy never receive it. `--no-ccr` is deliberately excluded from `expected_proxy_arg_signature` in `tool_manager.rs` (a runtime too old to accept the flag would restart-loop, the same reason `--no-http2` is excluded), which means **adding it does not restart an already-running backend**. A build can ship the mitigation and run all day without it.

```bash
PID=$(lsof -ti TCP:6768 -sTCP:LISTEN | head -1)
ps -o args= -p $PID | tr ' ' '\n' | grep -c -- '--no-ccr'
ps eww -o command= -p $PID | grep -c 'pyinject'
grep -c '_hd_sc_cacheable' \
  ~/Library/Application\ Support/Headroom/headroom/pyinject/sitecustomize.py
```
Expect: `0` on line 1 by default - 0.9.6 re-enabled CCR and `--no-ccr` is now opt-in (`desktop_forces_no_ccr` in `tool_manager.rs` only pushes it when the app was launched with `HEADROOM_DESKTOP_NO_CCR=1`), so a `1` there means the kill switch is set and CCR is OFF for this session. Then `1` (PYTHONPATH points at the injection dir), and non-zero (the response-cache guard is in the file on disk, not just in the Rust literal). The general rule still holds for any flag that IS expected: a `0` with the flag present in `tool_manager.rs` means the backend predates the change - restart it and re-run, rather than trusting the source.

Resolve the pid exactly as written. A bare `lsof -ti :6768` matches two processes - the backend that *listens* on 6768 and the desktop that holds a client connection to it - and `head -1` returns the lower pid, which is the desktop on any launch where it started first. Both counts then come back `0` and a healthy build reads as a hard FAIL (observed on the 0.8.1-rc.2 pass). `-sTCP:LISTEN` narrows it to the backend, but only when the protocol and port are a *single* selection (`-ti TCP:6768`): lsof ORs multiple `-i` arguments, so the plausible-looking `lsof -iTCP -sTCP:LISTEN -ti :6768` selects every TCP listener on the machine and `head -1` picks whatever unrelated daemon sorts first.

Note the port: use the live backend port from check 9 if it fell back off `6768`.

The definitive probe of whether `sitecustomize.py` was actually *imported* (rather than merely present and on the path) is `kill -USR1 $PID`, which the injected code turns into a faulthandler thread dump on the proxy's stderr - that is the per-boot `headroom-proxy---port-<port>---...log` under `~/Library/Application Support/Headroom/headroom/logs/`, NOT `~/.headroom/logs/proxy.log` (grep the latter for `Thread 0x` and a successful probe looks like it did nothing; observed on the 0.9.9-rc.1 pass). A proxy that survives the signal is already the pass; the dump is the corroboration. **It is destructive when it fails**: if injection did not happen, Python has no SIGUSR1 handler and the OS default terminates the proxy. That is an acceptable trade on a beta box - the signal is unambiguous either way and the restart is cheap - but do not run it against a session you care about.

### 13. No request was billed for a response the client could not use

The response-side half of check 11. A request can be optimized, accounted, and still hand the client something unusable - the proxy records a 200 and moves on. Two signatures, both one-liners, both scoped to the current log:

```bash
grep -c 'PERF model=claude-[^ ]* .*tok_out=0 ' ~/.headroom/logs/proxy.log
grep -c 'response_cache_store_refused' ~/.headroom/logs/proxy.log
```
Expect: `0` and `0`.

Line 1 is a real generating model that produced no output tokens - the client-visible shape of the empty-200 bug. Requiring `model=claude-` is what makes it usable: `passthrough:count_tokens` requests legitimately report `tok_out=0` and would otherwise swamp the count. Verified discriminating: `0` on the current log, `20` on the pre-fix rotated log, 28 across all history.

A non-zero line 1 is a tripwire, not yet a verdict: the PERF line carries no status, and an upstream error passed through produces the same shape (a session-start 429 logged `tok_out=0 ttfb_ms=0` on the 0.9.3-rc.5 Windows pass). For each matching PERF line, find the `event=proxy_inbound_response ... path=/v1/messages status=` line at the same timestamp - its `duration_ms` tracks the PERF `total_ms` when several requests share the second, and the two id namespaces (`hr_...` vs `id=inbound-...`) never join, so the timestamp is the key. `status=200` with zero output tokens is the bug class and a hard FAIL; a 4xx/5xx is the proxy passing an upstream error through - report it as benign.

Line 2 is the desktop's own `SemanticCache.set` guard in `SITECUSTOMIZE_PY` refusing to store a body that is empty, non-JSON, or an error envelope. It has never fired in ~6,400 requests, which is the point: `0` is healthy, and any non-zero means the runtime is producing bodies bad enough that the semantic cache would have replayed them for the full 1h TTL. Treat a hit as a wheel-bump regression and read the surrounding request, not as the guard doing routine work.

If the cache ever does replay a poisoned body, the signature is a `/v1/messages` 200 in under 20ms with `tok_out=0`:
```bash
grep 'PERF model=claude-' ~/.headroom/logs/proxy.log | awk '{for(i=1;i<=NF;i++){if($i~/^total_ms=/)t=substr($i,10)+0; if($i~/^tok_out=/)o=substr($i,9)+0} if(t<20&&o==0)n++} END{print n+0}'
```
Expect: `0`. Only a proxy restart clears a poisoned entry, so this stays non-zero until the backend is bounced.

### 14. User state survived the upgrade

Checks 1-13 all describe a working install. None of them notice that the upgrade silently reset it, because a wiped state file looks exactly like a healthy fresh one. Every persisted file here is read back through `serde`, so one field added or renamed in the new build is enough to fail a parse and hand the user a default: a restarted grace clock, an empty savings history, or a client-setup record that no longer knows which shell files we wrote (which is also what uninstall reads to clean up).

From 0.9.3 on, the app takes this snapshot itself: on the first launch of a new version, `snapshot_state_on_version_change` (storage.rs) copies the three state files raw into `config/pre-update/` - before anything parses them - with a `meta.json` naming the from/to versions. When the build being replaced is >= 0.9.3, verify `meta.json`'s `from_version` is that build and diff against `config/pre-update/` instead of a manual snapshot. The manual block below remains for upgrades from older builds and as a cross-check. One expected asymmetry: the auto-snapshot's `client-setup.json` is captured in the post-quit state, where `clear_client_setups()` has already emptied `configuredClients`/`managedShellFiles` - the surviving client set lives under `rememberedClients` there. Compare that key, not the configured sets (observed and verified on the rc.5 -> rc.7 pass).

**Run this block BEFORE installing the rc**, on the build you are upgrading from:
```bash
S=~/Library/Application\ Support/Headroom
mkdir -p /tmp/hr-preupgrade
jq '{first_seen_at,paywall_first}' "$S/config/headroom-pricing-state.json" > /tmp/hr-preupgrade/pricing.json
jq '{configured:(.configuredClients|keys),shell:(.managedShellFiles|keys)}' "$S/config/client-setup.json" > /tmp/hr-preupgrade/setup.json
jq '{tokens:.allTimeRecordTokens,recap:.lastWeeklyRecapWeekKey,schema:.schemaVersion}' "$S/config/activity-facts.json" > /tmp/hr-preupgrade/facts.json
# For check 15: the user's own CLAUDE.md content, excluding our managed blocks.
awk '/headroom:(learn:start|markitdown_office >>>)/{skip=1} !skip{n+=length($0)+1} /headroom:(learn:end|markitdown_office <<<)/{skip=0} END{print FILENAME, n+0}' \
  ~/.claude/CLAUDE.md > /tmp/hr-preupgrade/claude-md.txt
cat /tmp/hr-preupgrade/*.json /tmp/hr-preupgrade/claude-md.txt
```

**After installing and launching the rc**, re-run the same three `jq` expressions and diff:
```bash
S=~/Library/Application\ Support/Headroom
stat -f '%Sm %N' /tmp/hr-preupgrade/*   # must predate THIS install, not an older one
[ /tmp/hr-preupgrade -nt /Applications/Headroom.app ] && echo 'NOT RUN - snapshot is NEWER than the installed app; it was taken after this install'
diff <(jq '{first_seen_at,paywall_first}' "$S/config/headroom-pricing-state.json") /tmp/hr-preupgrade/pricing.json
diff <(jq '{configured:(.configuredClients|keys),shell:(.managedShellFiles|keys)}' "$S/config/client-setup.json") /tmp/hr-preupgrade/setup.json
jq '{tokens:.allTimeRecordTokens,recap:.lastWeeklyRecapWeekKey,schema:.schemaVersion}' "$S/config/activity-facts.json"
ls "$S/config/" | grep -c '\.corrupt$'
```
Expect: `first_seen_at` byte-identical (`paywall_first` may legitimately change - the server owns it), the configured-client and shell-file key sets unchanged, and `0` quarantine files. (Use `grep -c`, not `ls *.corrupt`: zsh aborts the whole line with `no matches found` when the glob is empty, which is the healthy case.)

Check the snapshot's mtime before trusting a clean diff. `/tmp/hr-preupgrade` survives across rcs, so a run that forgot the pre-install step silently diffs against a snapshot from two builds ago - which passes, but tests the wrong upgrade. If the mtime predates the build you just replaced, say so in the report rather than claiming this rc preserved state.

`activity-facts.json` is the deliberate exception: a `schemaVersion` bump intentionally drops the tile slots, so it needs its own comparison rather than a `diff`. What must survive a bump is `allTimeRecordTokens` and `lastWeeklyRecapWeekKey` - wiping those re-fires the weekly recap and resets all-time records for every user, which has happened on four bumps so far.

A non-empty `*.corrupt` listing is the highest-signal failure in this doc. `quarantine_unparsable` only creates one when a state file failed to parse and was about to be overwritten, so the file itself is the evidence: `jq . <the .corrupt file>` to see which field the new build could not read. The fix belongs on the struct (`#[serde(default)]`, per the Persistence Rules in CLAUDE.md), not on the file.

### 15. CLAUDE.md files are intact after the upgrade

Two independent writers edit the user's CLAUDE.md, and neither is a Headroom-owned file - a bad write damages the user's own instructions. The desktop's `upsert_managed_block` maintains `# >>> headroom:markitdown_office >>>` in `~/.claude/CLAUDE.md`; the Python `headroom learn` command maintains `<!-- headroom:learn:start -->` blocks in both the global and every project CLAUDE.md. Both find their markers by literal string search on the whole file, so an interrupted write, a hand-edited half-block, or a second writer racing the first duplicates markers instead of replacing the block.

```bash
for f in ~/.claude/CLAUDE.md ~/Code/headroom-desktop/CLAUDE.md; do
  echo "== $f"
  echo "  markitdown: $(grep -c '^# >>> headroom:markitdown_office >>>' "$f")/$(grep -c '^# <<< headroom:markitdown_office <<<' "$f")"
  echo "  learn:      $(grep -c '<!-- headroom:learn:start -->' "$f")/$(grep -c '<!-- headroom:learn:end -->' "$f")"
  awk '/headroom:(learn:start|markitdown_office >>>)/{skip=1} !skip{n+=length($0)+1} /headroom:(learn:end|markitdown_office <<<)/{skip=0} END{print "  user bytes outside managed blocks: " n+0}' "$f"
done
```
Expect: every pair is `0/0` or `1/1` - never `2/2` (duplicated block) and never `1/0` (truncated mid-write). The user-bytes figure has no fixed value; capture it in the check 14 pre-install snapshot and confirm it does not shrink across the upgrade. On this machine it is 81 for the global file (which is nearly all managed blocks) and ~3,000 for the desktop project file.

A `2/2` is the duplicate-block bug: `strip_marker_block` loops for exactly this reason, and `upsert_managed_block` treats reordered `end`-before-`start` markers as absent and appends fresh rather than rebuilding around them. Both behaviours have unit tests (`managed_block_upsert_replaces_existing_block_without_duplication`, `managed_block_upsert_treats_reordered_markers_as_absent`, `updating_one_managed_block_does_not_touch_other_blocks_or_user_content`), so a failure here means a new writer, not a regression in those.

Note what this check does **not** cover: the CLAUDE.md damage users reported in 0.34.0 was never on disk. Upstream's user-turn compression split the file's content mid-tag as it was sent to the model, so the file was fine and the model saw mangled instructions. That class is invisible to any filesystem check - it is caught by check 11 (mutations reaching the wire) and by reading a `/v1/messages` request body, not here. Fixed upstream in #2887, shipped in 0.35.0.

### 16. Lifetime card covers "saved today" (rollup backfill regression)

The two Home figures come from different bucket series: "Total costs saved" sums UTC-day buckets, the chart's "saved today" sums local-hour buckets. On 2026-08-27 a fresh backend data dir under an older local tracker made `drop_rollup_backfill` discard the ring's entire live-day daily bucket, so the lifetime card read $0.50 against a $0.91 "saved today" (the real day was $1.24). Fixed in 0.9.2-rc.3 by `settle_rollup_backfill` (subtract the ring's first-checkpoint cumulative instead of dropping the bucket); unit coverage is `settle_rollup_backfill_keeps_a_real_day_minus_the_ring_start` and `lifetime_card_never_reads_below_saved_today_on_a_fresh_ring`.

With the dashboard open and the backend up for at least a minute (history fetched, tray tick fired):

1. Visual: Home -> "Total costs saved" must be >= the chart's "saved today" figure (day view, today). Strictly less is a FAIL - lifetime is a superset of today.
2. Cross-check today's magnitude against the backend's own ring:
```bash
curl -s 127.0.0.1:6767/stats-history | jq -r --arg d "$(date -u +%Y-%m-%d)"   '[.series.daily[] | select(.timestamp | startswith($d)) | .compression_savings_usd_delta] | add // 0'
```
Expect: the chart's "saved today" is in the same ballpark as this number plus any output-shaping dollars (it may run slightly lower - the ring-start remainder and the live tray cadence - but must not be a small fraction of it, and the lifetime card must not sit below either).

This regression only reproduces when the local tracker's history predates the backend ring (reset/recreated backend data dir). A truly fresh install cannot show it; if this machine's install is fresh, report the visual invariant only and note the scenario did not apply.

## Codex checks (Codex pass)

Run these from a Codex CLI session (or with Codex configured and at least one Codex prompt sent this session). Codex routes through Headroom via an `OPENAI_BASE_URL` shell export plus a managed provider block in `~/.codex/config.toml` — not `~/.claude/settings.json` and not RTK — and its traffic is pay-per-token, so the proxy runs it in `token` mode.

### C1. Codex is configured to route through Headroom
```bash
grep -q 'model_provider = "headroom"' ~/.codex/config.toml && \
  grep -q 'openai_base_url = "http://127.0.0.1:6767/v1"' ~/.codex/config.toml && \
  grep -qF '[model_providers.headroom]' ~/.codex/config.toml && \
  grep -q 'supports_websockets = false' ~/.codex/config.toml && \
  grep -q 'export OPENAI_BASE_URL=http://127.0.0.1:6767/v1' ~/.zshrc ~/.zprofile 2>/dev/null && \
  echo PASS || echo FAIL
```
Expect: `PASS`. `~/.codex/config.toml` carries both managed marker blocks — `# >>> headroom:codex_cli >>>` with the root `model_provider`/`openai_base_url` keys, and `# >>> headroom:codex_cli_provider >>>` with the `[model_providers.headroom]` table — and a managed shell block exports `OPENAI_BASE_URL`. Headroom deliberately keeps `supports_websockets = false` so Codex uses the reliable HTTP Responses stream instead of failing the whole turn when an upstream WebSocket closes before `response.completed`. A `FAIL` means setup didn't write one of them (see `configure_codex_provider_block` / `configure_shell_block` in `client_adapters.rs`).

### C2. Codex traffic is actively optimized (token mode)
Codex is billed per token, so unlike a Claude Code subscription it runs in `token` mode and `requests_compressed` *does* move. Run this from inside Codex.
1. Capture the baseline:
   ```bash
   rtk proxy curl -s http://127.0.0.1:6767/stats | jq '{mode: .summary.mode, primary_model: .summary.primary_model, requests_compressed: .summary.compression.requests_compressed, total_tokens_removed: .summary.compression.total_tokens_removed}'
   ```
2. End the turn with a large file read in flight from Codex (~1300-1500 lines clears the compression threshold). As in check 7, the read lands in Codex's *next* prompt, so the re-check must be on a later turn.
3. On the next turn, re-run the same command.

Expect: `mode` is `token`, `primary_model` is a `gpt-*` model (confirms Codex — not Claude — is the traffic being measured), `requests_compressed` increased by at least 1, and `total_tokens_removed` is strictly greater. If `primary_model` is a `claude-*` model, the proxy is dominated by Claude traffic — confirm the prompt actually ran through Codex before trusting this check.

### C3. Codex savings are attributed on the dashboard
Open the dashboard and confirm a **Codex** group appears in the per-provider savings with non-zero values. Provider `openai` maps to the Codex group (`mergeProviderSavingsForDisplay` in `dashboardHelpers.ts`); a missing Codex group after Codex traffic means per-provider attribution isn't tagging OpenAI requests.

### C4. Pause / resume cleanly strips and restores Codex routing
The Claude equivalent is check 6; Pause clears *all* client setups, so it must remove Codex's config too. In Settings, toggle Pause then Resume (restore runs on a background thread, so give it a second), checking after each:
```bash
grep -c 'headroom:codex_cli' ~/.codex/config.toml
cat ~/.zshrc ~/.zprofile 2>/dev/null | grep -c 'OPENAI_BASE_URL=http://127.0.0.1:6767'
```
Expect: after Pause both print `0`; after Resume both are non-zero (config.toml back to `4` marker lines, shell back to one export per managed profile). Pause routes through `disable_codex_cli` — strips both TOML blocks, the `openai_base_url` root key, and the shell blocks; Resume re-applies them via `restore_client_setups`.

## Inspecting the proxy directly

When inspecting the running proxy by hand (e.g. checking `/stats`), wrap `curl` with `rtk proxy` to bypass RTK's output filtering — otherwise large JSON responses get summarized into a type-shape view that looks like a broken endpoint:

```bash
rtk proxy curl -s http://127.0.0.1:6767/stats | jq .summary
```

Every `rtk` invocation in this doc (checks 3, 7, C2, and above) has the same PATH caveat as check 3: when Claude Code or Codex runs them through their shell tool, `rtk` is not on PATH because the non-login shell never sources `~/.zprofile`. Either wrap the command in `zsh -lc '...'`, or call the binary by its managed path:

```bash
"$HOME/Library/Application Support/Headroom/headroom/bin/rtk" proxy curl -s http://127.0.0.1:6767/stats | jq .summary
```

When RTK is disabled (check 3's gate), the managed binary path above does not exist either — but no rewrite hook is active, so a plain `curl -s http://127.0.0.1:6767/stats | jq ...` is unfiltered and correct. Drop the `rtk proxy` wrapper entirely in that case.

## When something fails

- Proxy log silent → check `~/Library/Application Support/Headroom/headroom/logs/` for a newer log file or a crash file.
- Check 11 or 13 non-zero after a wheel bump → the regression is in the bundled `headroom-ai`, not in desktop code. Confirm the version with check 8, then search upstream before assuming it is ours: `gh api "search/issues?q=repo:headroomlabs-ai/headroom+<symptom>"`. Checks 11 and 13 exist because this class is silent in `/stats` and on the dashboard - the log is the only place the wire truth appears.
- Check 14 or 15 non-zero → this one is ours, and it is data loss, so stop the promotion. A `.corrupt` file names the field the new build could not read; a duplicated managed block names the writer that raced. The fix goes on the struct (`#[serde(default)]`) or in the writer, never by hand-repairing the user's file - the same build will do it again to everyone else.
- RTK missing → check the managed block in `~/.zshrc` / `~/.zprofile` is intact and the shell has been reloaded.
- MCP tool missing → restart Claude Code; the MCP server registration happens at session start.
