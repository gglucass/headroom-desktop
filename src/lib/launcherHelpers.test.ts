import { describe, expect, it } from "vitest";

import {
  buildInitialProxyVerificationRows,
  getClaudeConnector,
  getContactRequestValidationError,
  getLauncherAutoConfigureDecision,
  isValidEmailAddress
} from "./launcherHelpers";
import type { ClientConnectorStatus } from "./types";

describe("launcher helpers", () => {
  it("validates trimmed email addresses for auth and contact flows", () => {
    expect(isValidEmailAddress("  user@example.com  ")).toBe(true);
    expect(isValidEmailAddress("missing-at-symbol")).toBe(false);
    expect(isValidEmailAddress("user@example")).toBe(false);
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
    ).toBe("begin_proxy_verification");
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
});
