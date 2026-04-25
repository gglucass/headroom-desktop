import { aggregateClientConnectors } from "./dashboardHelpers";
import type { ClientConnectorStatus, LaunchExperience } from "./types";

export const EMAIL_ADDRESS_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

// Linear onboarding flow shown in the launcher window:
// install → client_setup → proxy_verify → post_install. Back buttons can jump
// backwards. The install step doubles as the pre-install landing.
export type LauncherStage =
  | "install"
  | "client_setup"
  | "proxy_verify"
  | "post_install";

export type LauncherAutoConfigureDecision =
  | "show_client_setup"
  | "apply_client_setup"
  | "begin_proxy_verification";

/// Step the launcher's auto-configure flow should take next, given a fresh
/// connector probe. The component is responsible for performing the IPC
/// calls; this helper isolates the decision logic so it can be unit-tested.
export type AutoConfigureStep =
  | { kind: "show_client_setup" }
  | { kind: "apply"; clientId: string }
  | { kind: "begin_proxy_verification" };

export interface ProxyVerificationRowState {
  clientId: string;
  name: string;
  state: "processing" | "waiting" | "verified";
  message: string;
}

export function isValidEmailAddress(email: string) {
  return EMAIL_ADDRESS_PATTERN.test(email.trim());
}

export function getContactRequestValidationError(
  contactFormUrl: string | undefined,
  email: string
) {
  if (!contactFormUrl) {
    return "Set VITE_HEADROOM_CONTACT_FORM_URL to enable contact requests.";
  }
  if (!isValidEmailAddress(email)) {
    return "Enter a valid email address.";
  }
  return null;
}

export function getClaudeConnector(connectors: ClientConnectorStatus[]) {
  return (
    aggregateClientConnectors(connectors).find(
      (connector) => connector.clientId === "claude_code"
    ) ?? null
  );
}

export function getLauncherAutoConfigureDecision(
  connectors: ClientConnectorStatus[]
): LauncherAutoConfigureDecision {
  const connector = getClaudeConnector(connectors);
  if (!connector?.installed) {
    return "show_client_setup";
  }
  if (!connector.enabled) {
    return "apply_client_setup";
  }
  return "begin_proxy_verification";
}

/// Given a launcher-window startup result, return the stage the launcher
/// should land on, or `null` to leave the current stage untouched (the caller
/// is in a non-launcher window, or bootstrap hasn't completed yet).
export function getInitialLauncherStage(
  windowLabel: string,
  bootstrapComplete: boolean,
  dashboardBootstrapComplete: boolean,
  launchExperience: LaunchExperience
): LauncherStage | null {
  if (windowLabel !== "launcher") {
    return null;
  }
  if (!bootstrapComplete && !dashboardBootstrapComplete) {
    return null;
  }
  return launchExperience === "first_run" ? "install" : "post_install";
}

/// First step of the launcher's auto-configure flow: decide what to do
/// given a fresh connector probe. Pre-apply only.
export function nextAutoConfigureStep(
  decision: LauncherAutoConfigureDecision,
  claudeConnector: ClientConnectorStatus | null
): AutoConfigureStep {
  if (decision === "show_client_setup") {
    return { kind: "show_client_setup" };
  }
  if (decision === "apply_client_setup") {
    if (!claudeConnector) {
      // No connector to apply against — fall back to manual setup.
      return { kind: "show_client_setup" };
    }
    return { kind: "apply", clientId: claudeConnector.clientId };
  }
  return { kind: "begin_proxy_verification" };
}

/// Second step of the launcher's auto-configure flow: after the apply IPC
/// resolved, decide whether to advance to proxy verification or bail back to
/// the manual setup screen. Reuses `nextAutoConfigureStep`'s decision branch
/// since the post-apply state is just a re-evaluation of the connector probe.
export function nextAutoConfigureStepAfterApply(
  postApplyDecision: LauncherAutoConfigureDecision
): AutoConfigureStep {
  if (postApplyDecision === "begin_proxy_verification") {
    return { kind: "begin_proxy_verification" };
  }
  return { kind: "show_client_setup" };
}

export function buildInitialProxyVerificationRows(
  connectors: ClientConnectorStatus[]
): ProxyVerificationRowState[] {
  return aggregateClientConnectors(connectors)
    .filter((connector) => connector.enabled && connector.installed)
    .sort((left, right) => left.name.localeCompare(right.name))
    .map((connector) => ({
      clientId: connector.clientId,
      name: connector.name,
      state: "processing",
      message: "Waiting for a Claude Code prompt..."
    }));
}
