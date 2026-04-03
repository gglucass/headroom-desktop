import * as Sentry from "@sentry/react";

import { describeInvokeError } from "./appHelpers";
import type { BootstrapProgress } from "./types";

export type BootstrapFailurePhase =
  | "install_runtime"
  | "start_runtime"
  | "command_dispatch"
  | "unknown";

export type BootstrapFailureSource = "progress_poll" | "invoke_error";

export interface BootstrapFailureReport {
  source: BootstrapFailureSource;
  phase: BootstrapFailurePhase;
  message: string;
  currentStep: string;
  overallPercent: number;
  currentStepEtaSeconds: number;
}

export function inferBootstrapFailurePhase(message: string): BootstrapFailurePhase {
  const normalized = message.trim();

  if (normalized.startsWith("Installation failed:")) {
    return "install_runtime";
  }

  if (normalized.startsWith("Install completed but Headroom failed to start:")) {
    return "start_runtime";
  }

  return "unknown";
}

export function buildBootstrapFailureReport(
  progress: BootstrapProgress,
  source: BootstrapFailureSource = "progress_poll"
): BootstrapFailureReport {
  const message = progress.message.trim() || "Headroom bootstrap failed.";
  const currentStep = progress.currentStep.trim() || "Install failed";

  return {
    source,
    phase: inferBootstrapFailurePhase(message),
    message,
    currentStep,
    overallPercent: Math.max(0, Math.round(progress.overallPercent || 0)),
    currentStepEtaSeconds: Math.max(0, Math.round(progress.currentStepEtaSeconds || 0)),
  };
}

export function buildBootstrapInvokeFailureReport(error: unknown): BootstrapFailureReport {
  return {
    source: "invoke_error",
    phase: "command_dispatch",
    message: describeInvokeError(error, "Could not start Headroom install."),
    currentStep: "Install failed",
    overallPercent: 1,
    currentStepEtaSeconds: 0,
  };
}

export function bootstrapFailureSignature(report: BootstrapFailureReport): string {
  return [
    report.source,
    report.phase,
    report.currentStep,
    report.message,
    String(report.overallPercent),
    String(report.currentStepEtaSeconds),
  ].join("|");
}

export function reportBootstrapFailure(report: BootstrapFailureReport, cause?: unknown) {
  const error = new Error(report.message);
  error.name = "BootstrapFailedError";

  Sentry.withScope((scope) => {
    scope.setLevel("error");
    scope.setTag("flow", "bootstrap");
    scope.setTag("bootstrap_phase", report.phase);
    scope.setTag("bootstrap_source", report.source);
    scope.setFingerprint(["bootstrap_failed", report.phase, report.source]);
    scope.setContext("bootstrap", {
      currentStep: report.currentStep,
      overallPercent: report.overallPercent,
      currentStepEtaSeconds: report.currentStepEtaSeconds,
    });

    if (cause !== undefined) {
      scope.setExtra("cause", describeInvokeError(cause, report.message));
    }

    Sentry.captureException(error);
  });
}
