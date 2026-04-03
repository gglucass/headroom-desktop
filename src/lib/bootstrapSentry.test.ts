import { describe, expect, it } from "vitest";

import {
  bootstrapFailureSignature,
  buildBootstrapFailureReport,
  buildBootstrapInvokeFailureReport,
  inferBootstrapFailurePhase,
} from "./bootstrapSentry";
import type { BootstrapProgress } from "./types";

function makeFailedProgress(message: string): BootstrapProgress {
  return {
    running: false,
    complete: false,
    failed: true,
    currentStep: "Install failed",
    message,
    currentStepEtaSeconds: 0,
    overallPercent: 58,
  };
}

describe("bootstrap sentry helpers", () => {
  it("infers install phase failures from backend bootstrap messages", () => {
    expect(
      inferBootstrapFailurePhase("Installation failed: downloading https://example.com/python.tar.gz")
    ).toBe("install_runtime");
  });

  it("infers runtime start failures from backend bootstrap messages", () => {
    expect(
      inferBootstrapFailurePhase(
        "Install completed but Headroom failed to start: headroom exited before opening port 6768"
      )
    ).toBe("start_runtime");
  });

  it("keeps unknown messages grouped as unknown phase", () => {
    expect(inferBootstrapFailurePhase("Unexpected installer state")).toBe("unknown");
  });

  it("builds normalized progress failure reports for Sentry", () => {
    const report = buildBootstrapFailureReport(
      makeFailedProgress("Installation failed: checksum mismatch")
    );

    expect(report).toEqual({
      source: "progress_poll",
      phase: "install_runtime",
      message: "Installation failed: checksum mismatch",
      currentStep: "Install failed",
      overallPercent: 58,
      currentStepEtaSeconds: 0,
    });
  });

  it("builds invoke failure reports from bridge errors", () => {
    const report = buildBootstrapInvokeFailureReport(new Error("Bootstrap is already running."));

    expect(report).toEqual({
      source: "invoke_error",
      phase: "command_dispatch",
      message: "Bootstrap is already running.",
      currentStep: "Install failed",
      overallPercent: 1,
      currentStepEtaSeconds: 0,
    });
  });

  it("uses failure signatures that separate retry sources", () => {
    const progressReport = buildBootstrapFailureReport(
      makeFailedProgress("Installation failed: checksum mismatch")
    );
    const invokeReport = buildBootstrapInvokeFailureReport(
      new Error("Installation failed: checksum mismatch")
    );

    expect(bootstrapFailureSignature(progressReport)).not.toBe(
      bootstrapFailureSignature(invokeReport)
    );
  });
});
