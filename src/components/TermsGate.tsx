import { useState, type ReactNode } from "react";

import { invoke } from "@tauri-apps/api/core";

import { openLinkFromClick } from "../lib/externalLink";

import headroomLogo from "../assets/headroom-logo.svg";

export interface TermsGateProps {
  /// The terms version the user is accepting (DashboardState.requiredTermsVersion).
  requiredVersion: number;
  /// Canonical Terms URL to open in the browser.
  termsUrl: string;
  /// Called after acceptance is persisted so the host can drop the gate.
  onAccepted: () => void;
  /// Paywall-first onboarding: sign-in form rendered inside the gate. When
  /// present, Continue also requires `authComplete`, and acceptance is only
  /// persisted then — an abandoned sign-in re-shows the whole gate next launch.
  authSection?: ReactNode;
  authComplete?: boolean;
}

/// Full-window blocking gate shown until the user accepts the current Terms of
/// Service. Rendered in both the launcher and the main window, so new installs
/// and updating users alike must accept before reaching any other UI.
export function TermsGate({
  requiredVersion,
  termsUrl,
  onAccepted,
  authSection,
  authComplete
}: TermsGateProps) {
  const [checked, setChecked] = useState(false);
  const [accepting, setAccepting] = useState(false);
  const authSatisfied = authSection === undefined || authComplete === true;
  const signInPresentation = authSection !== undefined;

  async function handleAccept() {
    if (!checked || accepting || !authSatisfied) {
      return;
    }
    setAccepting(true);
    try {
      await invoke("accept_terms", { version: requiredVersion });
      onAccepted();
    } catch {
      // Local acceptance failing is unexpected; re-enable the button so the
      // user can retry rather than getting stuck behind the gate.
      setAccepting(false);
    }
  }

  return (
    <div
      className="terms-gate"
      role="dialog"
      aria-modal="true"
      aria-labelledby="terms-gate-title"
    >
      <div className="terms-gate__panel">
        <img className="terms-gate__logo" src={headroomLogo} alt="" aria-hidden="true" />
        <h1 id="terms-gate-title" className="terms-gate__title">
          {signInPresentation ? "Sign in" : "Terms of Service"}
        </h1>
        {!signInPresentation && (
          <p className="terms-gate__copy">
            Please review and accept our Terms of Service to continue using
            Headroom.
          </p>
        )}
        {authSection}
        <label className="terms-gate__consent">
          <input
            type="checkbox"
            checked={checked}
            onChange={(event) => setChecked(event.target.checked)}
          />
          <span>
            I have read and accept the{" "}
            <button
              type="button"
              className="link-button"
              onClick={() => openLinkFromClick(termsUrl)}
            >
              Terms of Service
            </button>
            .
          </span>
        </label>
        <button
          type="button"
          className="primary-button primary-button--large terms-gate__accept"
          disabled={!checked || accepting || !authSatisfied}
          onClick={() => void handleAccept()}
        >
          {accepting ? "Saving…" : "Accept & Continue"}
        </button>
      </div>
    </div>
  );
}
