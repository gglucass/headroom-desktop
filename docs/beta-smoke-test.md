# Beta smoke test

After installing a new beta (`-rc.N`) build, paste this file into Claude Code and ask it to run the checks. Each check has a single expected signal — if any fail, stop and investigate before promoting to stable.

## Setup

1. Quit and relaunch Headroom from Applications.
2. Confirm the tray icon appears in the menu bar.
3. Open the dashboard window once (so the proxy is fully booted).

## Checks

Claude: run each block and report PASS / FAIL with the observed value.

### 1. Version matches the new beta
```bash
ls ~/Applications/Headroom.app/Contents/Info.plist >/dev/null && \
  /usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" /Applications/Headroom.app/Contents/Info.plist
```
Expect: the `-rc.N` version you just installed.

### 2. Proxy is intercepting this conversation
Send a trivial prompt ("say hi"), then:
```bash
/usr/bin/python3 -c "import json; d=json.load(open('/Users/garmlucassen/Library/Application Support/Headroom/config/activity-facts.json')); print(d['lastTransformation']['observedAt'])"
```
Expect: a UTC timestamp within the last minute. (The proxy log file mtime is not a reliable signal — it can lag by minutes between heartbeats.)

### 3. Activity facts updated
```bash
stat -f '%Sm' ~/Library/Application\ Support/Headroom/config/activity-facts.json
```
Expect: mtime within the last minute (after step 2).

### 4. RTK is on PATH and reports savings
```bash
rtk --version && rtk gain | head -5
```
Expect: a version line and a gain summary, no "command not found".

### 5. MCP retrieve tool is available (only if memory tools are enabled)
First check whether the proxy was started with memory tools:
```bash
ls ~/Library/Application\ Support/Headroom/headroom/logs/ | grep -E 'no-memory-tools' >/dev/null && echo 'memory tools DISABLED — skip this check' || echo 'memory tools enabled — run check'
```
If enabled, have Claude call `mcp__headroom__headroom_retrieve` with any small query and expect a tool result (not "No such tool available").

### 6. Tray → Dashboard renders
Click the tray icon, open the dashboard. Expect savings chart and per-client stats render without a blank/error state.

### 7. Pause / resume cleanly strips and restores interception
In Settings, toggle Pause then Resume. After Pause, `cat ~/.claude/settings.json | grep -c headroom-rtk-rewrite` should return `0`; after Resume it should return `1`.

## Inspecting the proxy directly

When inspecting the running proxy by hand (e.g. checking `/stats`), wrap `curl` with `rtk proxy` to bypass RTK's output filtering — otherwise large JSON responses get summarized into a type-shape view that looks like a broken endpoint:

```bash
rtk proxy curl -s http://127.0.0.1:6768/stats | jq .summary
```

## When something fails

- Proxy log silent → check `~/Library/Application Support/Headroom/headroom/logs/` for a newer log file or a crash file.
- RTK missing → check the managed block in `~/.zshrc` / `~/.zprofile` is intact and the shell has been reloaded.
- MCP tool missing → restart Claude Code; the MCP server registration happens at session start.
