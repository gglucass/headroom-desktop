import path from "node:path";
import { describe, expect, it } from "vitest";

// The extension ships as plain CommonJS; its pure helpers are tested here and
// `activate` (which needs the vscode API) is exercised in a real editor.
import ext from "./extension.cjs";

const { combine, fmt, pickSession, projectDirs, projectSlug, routed, usageView, view } = ext;

describe("vscode status bar extension", () => {
  it("rounds like the terminal statusline", () => {
    expect([700, 3067, 15400, 999499, 999500, 1100000].map(fmt)).toEqual([
      "700",
      "3.1k",
      "15k",
      "999k",
      "1M",
      "1.1M"
    ]);
  });

  it("finds this workspace's transcript folders the way Claude Code names them", () => {
    expect(projectSlug("/Users/garm/Code/headroom-desktop")).toBe(
      "-Users-garm-Code-headroom-desktop"
    );
    const long = `/Users/garm/${"x".repeat(250)}`;
    const truncated = `${projectSlug(long).slice(0, 200)}-abc123`;
    // path.join, like the extension: Windows separators are `\`.
    expect(projectDirs("/p", ["/a/b", long], () => [truncated, "other"])).toEqual([
      path.join("/p", "-a-b"),
      path.join("/p", truncated)
    ]);
    // Windows: VS Code says `c:\`, Claude Code's folder says `C--`.
    const win = `c:\\Users\\garm\\${"y".repeat(250)}`;
    const winFolder = `C${projectSlug(win).slice(1, 200)}-def456`;
    expect(projectDirs("/p", [win], () => [winFolder])).toEqual([path.join("/p", winFolder)]);
  });

  it("follows the workspace's most recently active conversation", () => {
    const sessions = {
      mine_old: { tokensSaved: 10, lastSavedAtMs: 100, lastRequestAtMs: 100 },
      mine_new: { tokensSaved: 20, lastSavedAtMs: 50, lastRequestAtMs: 300 },
      elsewhere: { tokensSaved: 30, lastSavedAtMs: 900, lastRequestAtMs: 900 }
    };
    expect(pickSession(sessions, (id: string) => id.startsWith("mine"))?.tokensSaved).toBe(20);
    expect(pickSession(sessions, () => false)).toBeNull();
  });

  it("flashes a new saving, then compressing, then the total, else hides", () => {
    const now = 1_000_000;
    const session = (lastSavedAtMs: number, lastRequestAtMs: number, tokensSaved = 31_000) => ({
      tokensSaved,
      lastSaved: 3_600,
      lastSavedAtMs,
      lastRequestAtMs
    });
    expect(view(session(now - 1_000, now), now)).toEqual({
      text: "$(zap) Headroom saved 31k (+3.6k)",
      highlight: true
    });
    expect(view(session(now - 60_000, now - 500), now)?.text).toBe(
      "$(sync~spin) Headroom compressing"
    );
    expect(view(session(now - 60_000, now - 60_000), now)).toEqual({
      text: "$(zap) Headroom saved 31k",
      highlight: false
    });
    expect(view(session(0, now - 60_000, 0), now)).toBeNull();
    expect(view(null, now)).toBeNull();
  });

  it("adds Claude plan usage after the savings, yellow near the cap", () => {
    const now = 1_790_000_000_000;
    const at = (seconds: number) => Math.floor(now / 1000) + seconds;
    const usage = (five: number, fiveAt: number, week: number, weekAt: number) => ({
      fiveHour: { usedPercent: five, resetsAt: fiveAt },
      sevenDay: { usedPercent: week, resetsAt: weekAt }
    });
    expect(usageView(usage(34.9, at(3_600), 62, at(300_000)), now)).toMatchObject({
      text: "usage: 5h 34%, week 62%",
      warn: false
    });
    // A window past its reset is back at 0; one at 80% or more warns.
    expect(usageView(usage(97, at(-60), 91, at(3_600)), now)).toMatchObject({
      text: "usage: 5h 0%, week 91%",
      warn: true
    });
    expect(usageView(null, now)).toBeNull();
    expect(usageView({}, now)).toBeNull();

    const saving = { text: "$(zap) Headroom saved 31k", highlight: false };
    const shown = usageView(usage(34, at(3_600), 62, at(300_000)), now);
    expect(combine(saving, shown)?.text).toBe("$(zap) Headroom saved 31k | usage: 5h 34%, week 62%");
    expect(combine(null, shown)?.text).toBe("$(zap) usage: 5h 34%, week 62%");
    expect(combine(saving, null)?.text).toBe("$(zap) Headroom saved 31k");
    expect(combine(null, null)).toBeNull();
  });

  it("hides once Headroom removed its statusline script (pause, quit, disconnect)", () => {
    const script = "/Users/x/.claude/hooks/headroom-statusline.sh";
    expect(routed(script, (p: string) => p === script)).toBe(true);
    expect(routed(script, () => false)).toBe(false);
    // A headroom.json written before scriptPath existed keeps showing.
    expect(routed(undefined, () => false)).toBe(true);
  });
});
