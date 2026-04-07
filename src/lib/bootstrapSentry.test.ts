import { describe, expect, it, vi, beforeEach } from "vitest";
import * as Sentry from "@sentry/react";

import {
  bootstrapFailureSignature,
  buildBootstrapFailureReport,
  buildBootstrapInvokeFailureReport,
  inferBootstrapFailurePhase,
  reportBootstrapFailure,
} from "./bootstrapSentry";
import type { BootstrapProgress } from "./types";

const mockScope = vi.hoisted(() => ({
  setLevel: vi.fn(),
  setTag: vi.fn(),
  setFingerprint: vi.fn(),
  setContext: vi.fn(),
  setExtra: vi.fn(),
}));

vi.mock("@sentry/react", () => ({
  captureException: vi.fn(),
  withScope: vi.fn((cb: (scope: unknown) => void) => cb(mockScope)),
}));

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

describe("reportBootstrapFailure", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("calls withScope and captureException", () => {
    const report = buildBootstrapFailureReport(
      makeFailedProgress("Installation failed: disk full")
    );

    reportBootstrapFailure(report);

    expect(Sentry.withScope).toHaveBeenCalledOnce();
    expect(Sentry.captureException).toHaveBeenCalledOnce();
    const err = vi.mocked(Sentry.captureException).mock.calls[0][0] as Error;
    expect(err).toBeInstanceOf(Error);
    expect(err.name).toBe("BootstrapFailedError");
    expect(err.message).toBe("Installation failed: disk full");
  });

  it("includes cause as extra when provided", () => {
    const report = buildBootstrapFailureReport(makeFailedProgress("Installation failed: disk full"));

    reportBootstrapFailure(report, new Error("underlying cause"));

    expect(mockScope.setExtra).toHaveBeenCalledWith("cause", expect.any(String));
  });

  it("does not set extra when cause is not provided", () => {
    const report = buildBootstrapFailureReport(makeFailedProgress("Installation failed: disk full"));

    reportBootstrapFailure(report);

    expect(mockScope.setExtra).not.toHaveBeenCalled();
  });
});
