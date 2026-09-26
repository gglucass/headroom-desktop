import { describe, expect, it } from "vitest";

import {
  buildInitialProxyVerificationRows,
  markIdleProxyVerificationRows,
  proxyVerificationRowMessage,
  PROXY_VERIFY_IDLE_AFTER_SECONDS,
  getClaudeConnector,
  getContactRequestValidationError,
  getInitialLauncherStage,
  magicLinkScreenCopy,
  getLauncherAutoConfigureDecision,
  isValidEmailAddress,
  needsTermsAcceptance,
  nextAutoConfigureStep,
  nextAutoConfigureStepAfterApply,
  recommendedHeadroomTier,
  INSTALL_WIZARD_STEPS,
  BILLING_FUNNEL_STEPS
} from "./launcherHelpers";
import type { ClientConnectorStatus } from "./types";

describe("install-wizard funnel steps", () => {
  // MUST match DesktopFunnelStep::BILLING in headroom-web, or the server drops them.
  it("pins the billing steps shared with the server", () => {
    expect([...BILLING_FUNNEL_STEPS]).toEqual(["upgrade_view_opened", "checkout_clicked"]);
  });

  // Pins the ordered step list. MUST stay in sync with DesktopFunnelStep::ORDER
  // in the headroom-web repo — the server ignores any step not in its list, so a
  // silent drift here means silently-dropped beacons.
  it("matches the canonical order shared with the server", () => {
    expect([...INSTALL_WIZARD_STEPS]).toEqual([
      "signup_gate_shown",
      "email_code_requested",
      "email_code_verified",
      "client_setup_shown",
      "client_setup_no_clients_detected",
      "client_setup_applied",
      "proxy_verify_started",
      "proxy_verified",
      "bootstrap_started",
      "bootstrap_completed",
      "bootstrap_failed",
      "post_install_shown",
      "unrouted_usage_detected",
      "unrouted_codex_usage_detected",
      "first_optimized_request",
      "first_prompt_request",
      "first_savings_recorded"
    ]);
  });
});

describe("launcher helpers", () => {
  it("validates trimmed email addresses for auth and contact flows", () => {
    expect(isValidEmailAddress("  user@example.com  ")).toBe(true);
    expect(isValidEmailAddress("missing-at-symbol")).toBe(false);
    expect(isValidEmailAddress("user@example")).toBe(false);
  });

  it("gates on terms acceptance until the accepted version catches up", () => {
    // New install: nothing accepted yet.
    expect(needsTermsAcceptance(1, 0)).toBe(true);
    // Already accepted the current version.
    expect(needsTermsAcceptance(1, 1)).toBe(false);
    // Terms bumped in an update: previously-accepted version is now stale.
    expect(needsTermsAcceptance(2, 1)).toBe(true);
    // Accepted version somehow ahead (downgrade): do not block.
    expect(needsTermsAcceptance(1, 2)).toBe(false);
  });

  it("returns contact validation errors before submit work begins", () => {
    expect(getContactRequestValidationError(undefined, "user@example.com")).toBe(
      "Set VITE_HEADROOM_CONTACT_FORM_URL to enable contact requests."
    );
    expect(getContactRequestValidationError("https://example.com/form", "invalid")).toBe(
      "Enter a valid email address."
    );
    expect(
      getContactRequestValidationError("https://example.com/form", "user@example.com")
    ).toBeNull();
  });

  it("finds the managed Claude connector from mixed connector lists", () => {
    const connectors: ClientConnectorStatus[] = [
      { clientId: "cursor", name: "Cursor", installed: true, enabled: false, verified: false },
      {
        clientId: "claude_code",
        name: "Claude Code",
        installed: true,
        enabled: true,
        verified: true
      }
    ];

    expect(getClaudeConnector(connectors)).toEqual(connectors[1]);
  });

  it("decides whether launcher auto-setup should wait, apply setup, or continue", () => {
    expect(getLauncherAutoConfigureDecision([])).toBe("show_client_setup");
    expect(
      getLauncherAutoConfigureDecision([
        {
          clientId: "claude_code",
          name: "Claude Code",
          installed: true,
          enabled: false,
          verified: false
        }
      ])
    ).toBe("apply_client_setup");
    expect(
      getLauncherAutoConfigureDecision([
        {
          clientId: "claude_code",
          name: "Claude Code",
          installed: true,
          enabled: true,
          verified: false
        }
      ])
    ).toBe("begin_post_install");
    // Codex-only: a non-Claude tool drives the same auto-configure decision.
    expect(
      getLauncherAutoConfigureDecision([
        { clientId: "codex", name: "Codex", installed: true, enabled: false, verified: false }
      ])
    ).toBe("apply_client_setup");
  });

  it("builds initial proxy verification rows from enabled installed Claude connectors", () => {
    const rows = buildInitialProxyVerificationRows([
      { clientId: "cursor", name: "Cursor", installed: true, enabled: true, verified: false },
      {
        clientId: "claude_code",
        name: "Claude Code",
        installed: true,
        enabled: true,
        verified: false
      },
      {
        clientId: "claude_code",
        name: "Claude Code Beta",
        installed: true,
        enabled: false,
        verified: false
      }
    ]);

    expect(rows).toEqual([
      {
        clientId: "claude_code",
        name: "Claude Code",
        state: "processing",
        message: "Waiting for a Claude Code prompt..."
      }
    ]);
  });

  describe("magicLinkScreenCopy", () => {
    it("asks before signing in, naming the link's account", () => {
      const copy = magicLinkScreenCopy("confirm", "a@b.com", null);
      expect(copy.title).toContain("?");
      expect(copy.body).toContain("a@b.com");
    });

    it("names the account while verifying", () => {
      expect(magicLinkScreenCopy("verifying", "a@b.com", null).body).toContain("a@b.com");
    });

    it("surfaces the verify error, falling back to a retry hint", () => {
      expect(magicLinkScreenCopy("failed", "a@b.com", "Code expired.").body).toBe(
        "Code expired."
      );
      expect(magicLinkScreenCopy("failed", "a@b.com", null).body).toContain(
        "Request a new sign-in link"
      );
    });
  });

  describe("getInitialLauncherStage", () => {
    it("returns null in non-launcher windows regardless of bootstrap state", () => {
      expect(getInitialLauncherStage("dashboard", true, true, "first_run")).toBeNull();
      expect(getInitialLauncherStage("tray", true, true, "resume")).toBeNull();
    });

    it("returns null in the launcher window until bootstrap is complete", () => {
      expect(getInitialLauncherStage("launcher", false, false, "first_run")).toBeNull();
    });

    it("lands first-run users on install when bootstrap completed during startup", () => {
      expect(getInitialLauncherStage("launcher", true, false, "first_run")).toBe("install");
    });

    it("lands first-run users on install when bootstrap was already complete", () => {
      expect(getInitialLauncherStage("launcher", false, true, "first_run")).toBe("install");
    });

    it("lands returning users on post_install once bootstrap is complete", () => {
      expect(getInitialLauncherStage("launcher", true, true, "resume")).toBe("post_install");
      expect(getInitialLauncherStage("launcher", false, true, "dashboard")).toBe(
        "post_install"
      );
    });
  });

  describe("nextAutoConfigureStep", () => {
    const claude: ClientConnectorStatus = {
      clientId: "claude_code",
      name: "Claude Code",
      installed: true,
      enabled: false,
      verified: false
    };

    const codex: ClientConnectorStatus = {
      clientId: "codex",
      name: "Codex",
      installed: true,
      enabled: false,
      verified: false
    };

    it("routes show_client_setup decisions to manual setup", () => {
      expect(nextAutoConfigureStep("show_client_setup", [claude])).toEqual({
        kind: "show_client_setup"
      });
    });

    it("routes apply_client_setup to an apply step for every installed, not-yet-enabled connector", () => {
      expect(nextAutoConfigureStep("apply_client_setup", [claude, codex])).toEqual({
        kind: "apply",
        clientIds: ["claude_code", "codex"]
      });
    });

    it("only applies connectors that are installed and not already enabled", () => {
      expect(
        nextAutoConfigureStep("apply_client_setup", [
          { ...claude, enabled: true },
          codex,
          { clientId: "codex", name: "Codex", installed: false, enabled: false, verified: false }
        ])
      ).toEqual({ kind: "apply", clientIds: ["codex"] });
    });

    it("falls back to manual setup when apply_client_setup has no detected connector", () => {
      expect(nextAutoConfigureStep("apply_client_setup", [])).toEqual({
        kind: "show_client_setup"
      });
    });

    it("routes begin_post_install straight to the post-install screen", () => {
      expect(nextAutoConfigureStep("begin_post_install", [])).toEqual({
        kind: "begin_post_install"
      });
    });
  });

  describe("nextAutoConfigureStepAfterApply", () => {
    it("advances to proxy verification when apply produced a verified setup", () => {
      expect(nextAutoConfigureStepAfterApply("begin_post_install")).toEqual({
        kind: "begin_post_install"
      });
    });

    it("falls back to manual setup when post-apply state still needs attention", () => {
      expect(nextAutoConfigureStepAfterApply("show_client_setup")).toEqual({
        kind: "show_client_setup"
      });
      expect(nextAutoConfigureStepAfterApply("apply_client_setup")).toEqual({
        kind: "show_client_setup"
      });
    });
  });

  describe("recommendedHeadroomTier", () => {
    it("maps Claude tiers directly", () => {
      expect(recommendedHeadroomTier("pro", null)).toBe("pro");
      expect(recommendedHeadroomTier("max5x", null)).toBe("max5x");
      expect(recommendedHeadroomTier("max20x", null)).toBe("max20x");
    });

    it("maps Codex tiers per the models.rs table", () => {
      expect(recommendedHeadroomTier(null, "go")).toBe("pro");
      expect(recommendedHeadroomTier(null, "plus")).toBe("pro");
      // Standard Business/Team seats carry a Plus-level Codex allowance.
      expect(recommendedHeadroomTier(null, "team")).toBe("pro");
      expect(recommendedHeadroomTier(null, "business")).toBe("pro");
      // ChatGPT Pro Lite is ~$100/mo, the Claude Max x5 price point.
      expect(recommendedHeadroomTier(null, "prolite")).toBe("max5x");
      expect(recommendedHeadroomTier(null, "self_serve_business_usage_based")).toBe("max5x");
      expect(recommendedHeadroomTier(null, "self_serve_business_prolite")).toBe("max5x");
      expect(recommendedHeadroomTier(null, "edu")).toBe("max5x");
      // OpenAI mints both spellings of the ChatGPT Edu claim.
      expect(recommendedHeadroomTier(null, "education")).toBe("max5x");
      expect(recommendedHeadroomTier(null, "pro")).toBe("max20x");
      expect(recommendedHeadroomTier(null, "enterprise")).toBe("max20x");
      expect(recommendedHeadroomTier(null, "enterprise_cbp_usage_based")).toBe("max20x");
    });

    it("takes the higher of the two detected tiers", () => {
      expect(recommendedHeadroomTier("pro", "pro")).toBe("max20x");
      expect(recommendedHeadroomTier("max20x", "plus")).toBe("max20x");
      expect(recommendedHeadroomTier("max5x", "go")).toBe("max5x");
    });

    it("defaults to pro when nothing is confidently detected", () => {
      expect(recommendedHeadroomTier(null, null)).toBe("pro");
      expect(recommendedHeadroomTier(undefined, undefined)).toBe("pro");
      expect(recommendedHeadroomTier("free", "free")).toBe("pro");
      expect(recommendedHeadroomTier("unknown", "unknown")).toBe("pro");
    });
  });
});

describe("idle marking and row order", () => {
  const row = (clientId: string, state: "processing" | "verified" | "idle" = "processing") => ({
    clientId,
    name: clientId === "codex" ? "ChatGPT Codex" : "Claude Code",
    state,
    message: ""
  });

  // The 2026-09-08 support case in miniature: an installed-but-dormant agent
  // beside a live one kept the whole screen red, so the user concluded the
  // product was broken when it was not.
  it("marks a connector with no local activity as idle and stops testing it", () => {
    const rows = markIdleProxyVerificationRows([row("codex"), row("claude_code")], {
      codex: 60
    });

    expect(rows.map((entry) => entry.state)).toEqual(["processing", "idle"]);
  });

  it("sinks idle rows below the ones the user still has to restart", () => {
    const rows = markIdleProxyVerificationRows([row("codex"), row("claude_code")], {
      claude_code: 60
    });

    expect(rows.map((entry) => entry.clientId)).toEqual(["claude_code", "codex"]);
  });

  it("treats activity older than the window as idle", () => {
    const rows = markIdleProxyVerificationRows([row("codex")], {
      codex: PROXY_VERIFY_IDLE_AFTER_SECONDS + 1
    });

    expect(rows[0].state).toBe("idle");
  });

  // Idle is a display state, not a lockout: a row that has already proven
  // itself must never be demoted by a stale artifact walk.
  it("never downgrades a verified row", () => {
    const rows = markIdleProxyVerificationRows([row("codex", "verified")], {});

    expect(rows[0].state).toBe("verified");
  });
});

describe("what the verify row says while it waits", () => {
  const row = {
    clientId: "claude_code",
    name: "Claude Code",
    state: "processing" as const,
    message: ""
  };

  it("says the tool is holding old settings without counting sessions", () => {
    const message = proxyVerificationRowMessage(row, 10);
    expect(message).toContain("Quit and reopen Claude Code");
    expect(message).not.toMatch(/\d/);
  });

  it("asks the user to start the tool when nothing is running", () => {
    expect(proxyVerificationRowMessage(row, 0)).toBe(
      "Open Claude Code and send it a message."
    );
  });

  it("asks the user to launch an idle tool rather than failing it", () => {
    const message = proxyVerificationRowMessage({ ...row, state: "idle" }, 0);
    expect(message).toContain("haven't used Claude Code recently");
    expect(message).toContain("Launch it");
  });

  it("confirms a verified row", () => {
    expect(proxyVerificationRowMessage({ ...row, state: "verified" }, 0)).toBe("Request received");
  });
});

