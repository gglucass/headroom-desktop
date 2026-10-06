// Headroom status bar item for the Claude Code panel in VS Code and Cursor.
// Installed and kept current by Headroom Desktop (vscode_statusbar.rs); do not
// edit the installed copy.
//
// The Claude Code panel renders no statusLine, so the line Headroom shows under
// the terminal prompt never reaches it. This reads the same per-conversation
// file (claude_statusline.rs) and shows this workspace's most recently active
// Claude Code conversation in the window's status bar. A workspace's
// conversations are the transcripts under ~/.claude/projects/<its slug>/.
// After it, Claude's plan usage, which Headroom keeps in the same file. While
// Headroom's pricing gate pauses Claude traffic, its notice replaces the saving.
"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");

// Same timings as the terminal statusline script.
const FLASH_MS = 4000;
const COMPRESS_MS = 2000;
const POLL_MS = 500;
// Claude Code truncates longer project folder names and appends a hash.
const SLUG_MAX = 200;
// A window this full turns the item yellow, as in the tray and terminal.
const WARN_PERCENT = 80;
const SAVINGS_TOOLTIP =
  "Input tokens Headroom saved in this workspace's latest Claude Code conversation";

/** Claude Code's ~/.claude/projects folder name for a working directory. */
function projectSlug(dir) {
  return dir.replace(/[^a-zA-Z0-9]/g, "-");
}

/** Folders under `root` that hold transcripts for these workspace folders. */
function projectDirs(root, folders, readdir) {
  const dirs = [];
  for (const folder of folders) {
    const slug = projectSlug(folder);
    if (slug.length <= SLUG_MAX) {
      dirs.push(path.join(root, slug));
      continue;
    }
    // The hash suffix is Claude Code's own; match on the truncated prefix.
    // Case-insensitively: on Windows VS Code reports the drive as `c:` where
    // Claude Code's folder has `C--`.
    const prefix = `${slug.slice(0, SLUG_MAX)}-`.toLowerCase();
    for (const name of readdir(root)) {
      if (name.toLowerCase().startsWith(prefix)) dirs.push(path.join(root, name));
    }
  }
  return dirs;
}

function lastActive(session) {
  return Math.max(session.lastSavedAtMs || 0, session.lastRequestAtMs || 0);
}

/** The most recently active conversation that `belongs` to this workspace. */
function pickSession(sessions, belongs) {
  let best = null;
  for (const [id, session] of Object.entries(sessions || {})) {
    if (belongs(id) && (!best || lastActive(session) > lastActive(best))) best = session;
  }
  return best;
}

/** 700, 3.1k, 15k, 1.1M: the terminal script's rounding. */
function fmt(n) {
  let div;
  let unit;
  if (n >= 999500) {
    div = 1000000;
    unit = "M";
  } else if (n >= 1000) {
    div = 1000;
    unit = "k";
  } else {
    return String(n);
  }
  const tenths = Math.floor((n * 10 + div / 2) / div);
  if (tenths >= 100) return `${Math.floor((n + div / 2) / div)}${unit}`;
  return `${tenths % 10 === 0 ? tenths / 10 : (tenths / 10).toFixed(1)}${unit}`;
}

/** False once Headroom deleted its terminal statusline script, which it does
 *  whenever Claude Code stops routing through it (pause, quit, connector off):
 *  the total would belong to conversations Headroom no longer sees. A
 *  headroom.json from before scriptPath has none and keeps showing. */
function routed(scriptPath, exists) {
  return !scriptPath || exists(scriptPath);
}

/** What the item shows at `now`, or null to hide it. A fresh saving outranks
 *  "compressing", as in the terminal line. */
function view(session, now) {
  if (!session) return null;
  const total = session.tokensSaved || 0;
  const last = session.lastSaved || 0;
  if (last > 0 && now - (session.lastSavedAtMs || 0) < FLASH_MS) {
    return { text: `$(zap) Headroom saved ${fmt(total)} (+${fmt(last)})`, highlight: true };
  }
  if (now - (session.lastRequestAtMs || 0) < COMPRESS_MS) {
    return { text: "$(sync~spin) Headroom compressing", highlight: false };
  }
  if (total > 0) return { text: `$(zap) Headroom saved ${fmt(total)}`, highlight: false };
  return null;
}

/** The gate's notice ("Headroom paused: trial ended. Upgrade in the Headroom
 *  app to resume"), its first sentence on the bar and the rest as the tooltip;
 *  null when Claude traffic is not paused. */
function pausedView(paused) {
  if (!paused) return null;
  const [head, ...rest] = paused.split(". ");
  return { text: `$(debug-pause) ${head}`, highlight: false, warn: true, tooltip: rest.join(". ") || head };
}

/** "usage: 5h 34%, week 62%" from the plan usage Headroom keeps, or null without
 *  any. A window past its reset is back at 0, as in the tray. */
function usageView(usage, now) {
  const parts = [];
  const resets = [];
  let warn = false;
  for (const [key, label] of [
    ["fiveHour", "5h"],
    ["sevenDay", "week"]
  ]) {
    const window = usage && usage[key];
    if (!window || typeof window.usedPercent !== "number") continue;
    const resetsAtMs = (window.resetsAt || 0) * 1000;
    const used = now >= resetsAtMs ? 0 : Math.trunc(Math.min(100, Math.max(0, window.usedPercent)));
    warn = warn || used >= WARN_PERCENT;
    parts.push(`${label} ${used}%`);
    if (now < resetsAtMs) resets.push(`${label} resets ${new Date(resetsAtMs).toLocaleString()}`);
  }
  if (!parts.length) return null;
  const tooltip = `Claude plan usage${resets.length ? ` (${resets.join(", ")})` : ""}`;
  return { text: `usage: ${parts.join(", ")}`, warn, tooltip };
}

/** The item for a saving view and a usage view, savings first; null hides. */
function combine(saving, usage) {
  if (!saving && !usage) return null;
  return {
    text: saving ? (usage ? `${saving.text} | ${usage.text}` : saving.text) : `$(zap) ${usage.text}`,
    highlight: Boolean(saving && saving.highlight),
    warn: Boolean((usage && usage.warn) || (saving && saving.warn)),
    tooltip: [(saving && saving.tooltip) || SAVINGS_TOOLTIP, usage && usage.tooltip]
      .filter(Boolean)
      .join("\n")
  };
}

function activate(context) {
  const vscode = require("vscode");
  let statePath;
  let scriptPath;
  try {
    ({ statePath, scriptPath } = JSON.parse(
      fs.readFileSync(path.join(__dirname, "headroom.json"), "utf8")
    ));
  } catch {
    return;
  }
  const root = path.join(os.homedir(), ".claude", "projects");
  const readdir = (dir) => {
    try {
      return fs.readdirSync(dir);
    } catch {
      return [];
    }
  };
  const item = vscode.window.createStatusBarItem(
    "headroom.status",
    vscode.StatusBarAlignment.Right,
    100
  );
  item.name = "Headroom savings";
  const green = new vscode.ThemeColor("charts.green");
  const yellow = new vscode.ThemeColor("charts.yellow");

  let dirs = [];
  // Only positives are cached: a new conversation's transcript can appear a
  // moment after its first request, so a miss is checked again next time.
  const known = new Set();
  let mtime = -1;
  let session = null;
  let planUsage = null;
  let paused = null;
  const belongs = (id) => {
    if (known.has(id)) return true;
    if (!dirs.some((dir) => fs.existsSync(path.join(dir, `${id}.jsonl`)))) return false;
    known.add(id);
    return true;
  };
  const refreshDirs = () => {
    const folders = (vscode.workspace.workspaceFolders || []).map((f) => f.uri.fsPath);
    dirs = projectDirs(root, folders, readdir);
    known.clear();
    mtime = -1;
  };
  const tick = () => {
    try {
      const current = fs.statSync(statePath).mtimeMs;
      if (current !== mtime) {
        mtime = current;
        const state = JSON.parse(fs.readFileSync(statePath, "utf8"));
        session = pickSession(state.sessions, belongs);
        planUsage = state.planUsage || null;
        paused = state.paused || null;
      }
    } catch {
      mtime = -1;
      session = null;
      planUsage = null;
      paused = null;
    }
    const now = Date.now();
    const shown = routed(scriptPath, fs.existsSync)
      ? combine(pausedView(paused) || view(session, now), usageView(planUsage, now))
      : null;
    if (!shown) {
      item.hide();
      return;
    }
    item.text = shown.text;
    item.tooltip = shown.tooltip;
    item.color = shown.highlight ? green : shown.warn ? yellow : undefined;
    item.show();
  };

  refreshDirs();
  tick();
  const timer = setInterval(tick, POLL_MS);
  context.subscriptions.push(
    item,
    { dispose: () => clearInterval(timer) },
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      refreshDirs();
      tick();
    })
  );
}

function deactivate() {}

module.exports = {
  activate,
  deactivate,
  combine,
  fmt,
  pausedView,
  pickSession,
  projectDirs,
  projectSlug,
  routed,
  usageView,
  view
};
