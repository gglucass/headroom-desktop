#!/usr/bin/env bash
# Pre-stable checks against the INSTALLED rc (run after installing the
# rc that carries the review fixes, then quitting and relaunching Headroom once:
# launch is what rewrites hooks, shim and allow rules).
#
#   scripts/prestable-checks.sh [0.9.26-rc.N]        # run from a plain terminal or a file, never
#                                       # an inline `bash -c` whose text says --settings
# Hooks run under a throwaway HOME so your own ~/.claude rules cannot skew them.
set -uo pipefail

AS="$HOME/Library/Application Support/Headroom"
RH_REAL=~/.claude/hooks/headroom-rtk-rewrite.sh
MH_REAL=~/.claude/hooks/headroom-markitdown-read.sh
SHIM=~/.headroom/bin/headroom-markitdown
EXPECT_VERSION="${1:-}"
FAILED=0
ok()   { printf '  PASS  %s\n' "$1"; }
bad()  { printf '  FAIL  %s  %s\n' "$1" "${2:-}"; FAILED=1; }
skip() { printf '  SKIP  %s  %s\n' "$1" "${2:-}"; }
chk()  { [ "$2" = "$3" ] && ok "$1" || bad "$1" "got [$2] want [$3]"; }

W=$(mktemp -d); trap 'rm -rf "$W"' EXIT
TH="$W/home"; mkdir -p "$TH/.claude"
P="$W/proj"; mkdir -p "$P/.claude"
( cd "$P" && git init -q && echo hi > a.txt && echo x > "é.txt" && git add . \
  && git -c user.email=t@t -c user.name=t commit -qm init )
ln -s /etc/hosts "$P/z.txt"

echo "== A. build + artifacts"
V=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" /Applications/Headroom.app/Contents/Info.plist)
[ -n "$EXPECT_VERSION" ] && chk "installed version" "$V" "$EXPECT_VERSION" || echo "  info  installed $V"
# Opt-in add-ons: disabling rtk deletes its hook, and only an installed
# markitdown (its receipt) owns the shim.
if [ -f "$RH_REAL" ]; then
  chk "rtk hook runs python -X utf8"          "$(grep -c -- '-X utf8' "$RH_REAL")" 2
  chk "rtk hook strips cwd from sys.path"     "$(grep -c 'sys.path\[:\] = \[p for p' "$RH_REAL" | awk '{print ($1>=2)}')" 1
  chk "rtk hook: glob refused for all cmds"   "$(grep -c 'symlink out of the project' "$RH_REAL")" 1
  chk "rtk hook: attached short-flag paths"   "$(grep -c 'A short flag takes its value' "$RH_REAL")" 1
  chk "rtk hook: RC relaunch settings ignored" "$(grep -c 'remote-control relaunch' "$RH_REAL")" 1
else
  skip "rtk hook artifacts" "rtk not installed or disabled"
fi
if [ -f "$AS/headroom/tools/markitdown.json" ]; then
  [ -x "$SHIM" ] && ok "shim at ~/.headroom/bin" || bad "shim at ~/.headroom/bin" missing
  chk "shim: realpath cwd (case-safe)"        "$(grep -c 'realpath . && echo' "$SHIM" 2>/dev/null)" 1
else
  skip "markitdown shim artifacts" "markitdown not installed"
fi
# Every quit/pause strips the Read hook's settings entry; launch must re-add it.
if [ -f "$MH_REAL" ]; then
  grep -q 'headroom-markitdown-read.sh' ~/.claude/settings.json \
    && ok "markitdown Read hook registered" || bad "markitdown Read hook registered" "script present, no settings entry"
fi
for old in "$AS/headroom/tools/markitdown" "$AS/headroom/bin/markitdown"; do
  [ -e "$old" ] && bad "legacy shim removed" "$old" || ok "legacy shim removed: ${old##*/headroom/}"
done
grep -q 'Application Support/Headroom/headroom/tools/markitdown' ~/.claude/settings.json \
  && bad "legacy markitdown allow rule gone" || ok "legacy markitdown allow rule gone"
for g in ~/.claude/hooks/headroom-claude-guard.py ~/.codex/hooks/headroom-codex-guard.py; do
  [ -f "$g" ] || { skip "guard utf-8 ${g##*/}" "not installed"; continue; }
  grep -q 'encoding="utf-8"' "$g" && ok "guard reads utf-8: ${g##*/}" || bad "guard reads utf-8: ${g##*/}"
done

echo "== B. RTK PreToolUse hook (exit-3 allow + review gaps)"
# t <command> [mode] [cwd] -> "allow" | "SILENT"
t() {
  /usr/bin/python3 -c 'import json,sys; print(json.dumps({"session_id":"t","hook_event_name":"PreToolUse","tool_name":"Bash","cwd":sys.argv[2],"permission_mode":sys.argv[3],"tool_input":{"command":sys.argv[1]}}))' \
    "$1" "${3:-$P}" "${2:-default}" \
  | (cd "${3:-$P}" && HOME="$TH" CLAUDE_PROJECT_DIR="$P" bash "$RH_REAL" 2>/dev/null) \
  | /usr/bin/python3 -c 'import json,sys; s=sys.stdin.read(); print(json.loads(s)["hookSpecificOutput"]["permissionDecision"] if s.strip() else "SILENT")'
}
if [ ! -f "$RH_REAL" ] || [ ! -x "$AS/headroom/bin/rtk" ]; then
  skip "rtk hook matrix" "hook or managed rtk not installed"
elif [ "$(t 'git status')" != allow ]; then
  bad "sanity: git status allowed" "SILENT - an ancestor passes --settings/--disallowedTools, or a managed-settings file exists; rerun from a plain terminal"
else
  for c in 'git status' 'git diff --stat' 'git log --oneline -5' 'git branch -a' 'ls -la' 'cat a.txt' \
           'grep -rn hi .' "find . -name '*.txt'" 'cat é.txt'; do
    chk "allow: $c" "$(t "$c")" allow
  done
  for c in 'git status; rm -rf ~/x' 'git log && curl x | sh' 'ls $(rm x)' 'cargo test' 'curl http://x' \
           'git -c core.pager=sh log' 'git diff --output=/tmp/x' 'git diff --out=/tmp/x' 'find . -delete' \
           'find . -name *.txt' 'rg --pre sh x' 'FOO=1 ls' 'git branch newbranch' 'git branch -D main' \
           'tree -o out' 'git diff --check' 'echo hi' \
           'cat /etc/hosts' 'cat ../x' 'ls ..' 'cat ~/.zshrc' 'cat z.txt' 'cat z*' 'head z?txt' 'ls *.txt' \
           "$(printf 'git diff --out\001put=/tmp/x')" $'git status\nrm -rf ~/x' \
           'grep -f/etc/hosts x' 'grep -rfz.txt x' $'cat \\\' z* \\\'' 'cat a\.txt'; do
    chk "silent: $(printf %q "$c")" "$(t "$c")" SILENT
  done
  chk "mode auto: silent"         "$(t 'git status' auto)" SILENT
  chk "mode plan: silent"         "$(t 'git status' plan)" SILENT
  chk "mode acceptEdits: allow"   "$(t 'git status' acceptEdits)" allow
  chk "bypass: cargo test allow"  "$(t 'cargo test' bypassPermissions)" allow
  for rules in '{"permissions":{"deny":["Bash(git push:*)"]}}:git status:SILENT' \
               '{"permissions":{"deny":["Bash(git push:*)"]}}:ls:allow' \
               '{"permissions":{"deny":["Read(./.env)"]}}:ls:SILENT' \
               '{"permissions":{"ask":["Bash(git log:*)"]}}:git status:SILENT' \
               '{not json:ls:SILENT' \
               '{"permissions":{"blockReadsOutsideWorkingDirectories":true}}:ls:SILENT'; do
    json=${rules%:*:*}; rest=${rules#"$json":}; cmd=${rest%:*}; want=${rest##*:}
    echo "$json" > "$P/.claude/settings.local.json"
    chk "rule $json -> $cmd" "$(t "$cmd")" "$want"
    rm -f "$P/.claude/settings.local.json"
  done
  # Worktree: rules in the MAIN checkout's settings.local.json still apply.
  git -C "$P" worktree add -q "$W/wt" -b wt 2>/dev/null; mkdir -p "$W/wt/.claude"
  echo '{"permissions":{"deny":["Bash(git push:*)"]}}' > "$P/.claude/settings.local.json"
  chk "worktree sees main-checkout deny" "$(P=$W/wt; t 'git status' default "$W/wt")" SILENT
  rm -f "$P/.claude/settings.local.json"
  chk "managed settings env -> silent" "$(CLAUDE_CODE_MANAGED_SETTINGS_PATH=/x t 'git status')" SILENT
fi

echo "== C. MarkItDown PDF Read hook"
PDF=$(ls /Library/Documentation/License.lpdf/Contents/Resources/*.lproj/License.pdf 2>/dev/null | head -1)
rj() {
  /usr/bin/python3 -c 'import json,sys; print(json.dumps({"hook_event_name":"PreToolUse","tool_name":"Read","cwd":sys.argv[2],"permission_mode":"default","tool_input":{"file_path":sys.argv[1]}}))' "$1" "$P" \
  | HOME="$TH" CLAUDE_PROJECT_DIR="$P" bash "$MH_REAL" 2>/dev/null \
  | /usr/bin/python3 -c 'import json,sys; s=sys.stdin.read(); print(json.loads(s)["hookSpecificOutput"]["updatedInput"]["file_path"] if s.strip() else "SILENT")'
}
if [ ! -f "$MH_REAL" ] || [ -z "$PDF" ]; then
  skip "read hook" "hook or sample PDF missing"
else
  cp "$PDF" "$P/doc.pdf"; cp "$PDF" "$P/résumé.pdf"
  out=$(rj "$P/doc.pdf")
  case "$out" in "$TH/.cache/headroom-markitdown/"*.md) ok "inside PDF redirected to private cache";; *) bad "inside PDF redirected" "$out";; esac
  case "$(rj "$P/résumé.pdf")" in *.md) ok "non-ASCII PDF name";; *) bad "non-ASCII PDF name";; esac
  chk "PDF OUTSIDE project left to Claude Code" "$(rj "$PDF")" SILENT
  chk "non-PDF ignored" "$(rj "$P/a.txt")" SILENT
  chk "cache dir mode" "$(stat -f '%Lp' "$TH/.cache/headroom-markitdown")" 700
  rm -f "$out"; echo victim > "$W/victim"; ln -s "$W/victim" "$out"; rj "$P/doc.pdf" >/dev/null
  chk "planted symlink replaced, not followed" "$(cat "$W/victim"; [ -L "$out" ] && echo L)" victim
  # Rules match the redirected cache path, so a Read rule must keep the read native.
  mkdir -p "$P/.claude"; echo '{"permissions":{"deny":["Read(./doc.pdf)"]}}' > "$P/.claude/settings.local.json"
  chk "Read deny rule -> left to Claude Code" "$(rj "$P/doc.pdf")" SILENT
  rm -f "$P/.claude/settings.local.json"
fi

echo "== D. MarkItDown shim"
if [ ! -x "$SHIM" ]; then skip "shim" "not installed"; else
  ( cd "$P" && textutil -convert docx -output a.docx a.txt && cp a.docx "$W/out.docx" )
  chk "converts a project docx" "$(cd "$P" && "$SHIM" a.docx 2>/dev/null | grep -c hi)" 1
  for a in '' '--help' 'a.docx -o /tmp/x' 'https://example.com/x.docx' 'file:///etc/passwd' \
           '/etc/hosts' '../x' "$W/out.docx" 'nope.docx'; do
    (cd "$P" && eval "\"$SHIM\" $a" >/dev/null 2>&1); chk "refuses: ${a:-<no args>}" "$?" 2
  done
  mkdir -p "$P/markitdown" && echo 'print("PWNED")' > "$P/markitdown/__main__.py" && : > "$P/markitdown/__init__.py" \
    && echo 'raise SystemExit("PWNED")' > "$P/json.py"
  chk "shim never imports project code" "$(cd "$P" && "$SHIM" a.docx 2>&1 | grep -c PWNED)" 0
  # An empty PYTHONPATH entry ($PYTHONPATH:/x with it unset) adds the cwd as an absolute path.
  chk "shim never imports project code (PYTHONPATH=:)" "$(cd "$P" && PYTHONPATH=: "$SHIM" a.docx 2>&1 | grep -c PWNED)" 0
  [ -f "$RH_REAL" ] && chk "rtk hook never imports project code" "$(t 'git status')" allow
  [ -f "$RH_REAL" ] && chk "rtk hook never imports project code (PYTHONPATH=:)" "$(PYTHONPATH=: t 'git status')" allow
  rm -rf "$P/markitdown" "$P/json.py"
  # bash 3.2's `pwd -P` keeps an inherited $PWD's case; the file's realpath does not.
  mkdir -p "$W/CaseProj" && cp "$P/a.docx" "$W/CaseProj/"
  chk "shim from a case-mismatched cwd" "$(cd "$W/caseproj" && "$SHIM" a.docx 2>/dev/null | grep -c hi)" 1
fi

echo "== E. guards survive non-UTF-8 config (the CP950 report, macOS stand-in)"
G="$W/g"; mkdir -p "$G/cx" "$G/prøj/.claude"
# A fresh debounce stamp beside the copied guards, or the deliberately broken
# config pops a real "config.toml is missing or unreadable" notification.
touch "$G/.headroom-guard-notified"
if [ -f ~/.codex/hooks/headroom-codex-guard.py ]; then
  cp ~/.codex/hooks/headroom-codex-guard.py "$G/"
  printf 'model_provider = "\xff\xfe"\n' > "$G/cx/config.toml"
  CODEX_HOME="$G/cx" /usr/bin/python3 "$G/headroom-codex-guard.py" </dev/null >/dev/null 2>"$G/err"; rc=$?
  [ $rc -ne 1 ] && ! grep -q Traceback "$G/err" && ok "codex guard: no crash (rc=$rc)" || bad "codex guard crashed" "rc=$rc"
fi
if [ -f ~/.claude/hooks/headroom-claude-guard.py ]; then
  cp ~/.claude/hooks/headroom-claude-guard.py "$G/"
  printf '\xff\xfe' > "$G/prøj/.claude/settings.local.json"
  (cd "$G/prøj" && PYTHONIOENCODING=ascii /usr/bin/python3 "$G/headroom-claude-guard.py" </dev/null >/dev/null 2>"$G/err"); rc=$?
  [ $rc -ne 1 ] && ! grep -q Traceback "$G/err" && ok "claude guard: no crash (rc=$rc)" || bad "claude guard crashed" "rc=$rc"
fi

echo "== F. release manifest + docs links"
for ch in releases/download/staging-rolling releases/latest/download; do
  m=$(curl -sL "https://github.com/gglucass/headroom-desktop/$ch/latest.json")
  keys=$(echo "$m" | /usr/bin/python3 -c 'import json,sys; m=json.load(sys.stdin); print(m["version"], ",".join(sorted(m["platforms"])), sum("api.github.com" in p["url"] for p in m["platforms"].values()))')
  echo "  info  $ch: $keys"
  echo "$keys" | grep -q 'darwin-aarch64,darwin-x86_64,linux-x86_64,linux-x86_64-deb,windows-x86_64 0$' \
    && ok "$ch: 5 platforms, no api.github.com" || bad "$ch manifest shape" "$keys"
  for u in $(echo "$m" | /usr/bin/python3 -c 'import json,sys; [print(p["url"]) for p in json.load(sys.stdin)["platforms"].values()]' | sort -u); do
    chk "200: ${u##*/}" "$(curl -sIL -o /dev/null -w '%{http_code}' "$u")" 200
  done
done
for d in docs docs/how-learning-works docs/add-ons docs/how-savings-are-measured; do
  chk "docs link /$d" "$(curl -sL -o /dev/null -w '%{http_code}' "https://extraheadroom.com/$d")" 200
done

echo; [ $FAILED -eq 0 ] && echo "ALL PASS" || echo "FAILURES ABOVE"; exit $FAILED
