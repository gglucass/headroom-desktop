import { afterEach, describe, expect, it, vi } from "vitest";

import type { HeadroomAccountProfile, HeadroomPricingStatus } from "./types";
import { maybeFireTrialNotifications } from "./trialNotifications";
import { localDayKey } from "./urgentNotifications";

const { invokeMock, isVisibleMock, windows } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  isVisibleMock: vi.fn(),
  // The calling webview's label, and whether the OTHER Headroom window is on
  // screen. isVisibleMock is the calling (main) window.
  windows: { label: "main", otherVisible: false },
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ label: windows.label, isVisible: isVisibleMock }),
  getAllWindows: async () => [
    { isVisible: isVisibleMock },
    { isVisible: async () => windows.otherVisible },
  ],
}));

function installStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      getItem: vi.fn((key: string) => values.get(key) ?? null),
      setItem: vi.fn((key: string, value: string) => {
        values.set(key, value);
      }),
      removeItem: vi.fn((key: string) => {
        values.delete(key);
      }),
    },
  });
  return values;
}

afterEach(() => {
  windows.label = "main";
  windows.otherVisible = false;
  vi.useRealTimers();
  vi.unstubAllEnvs();
});

function hoursAgo(n: number): string {
  return new Date(Date.now() - n * 60 * 60 * 1000).toISOString();
}

function hoursFromNow(n: number): string {
  return new Date(Date.now() + n * 60 * 60 * 1000).toISOString();
}

function daysFromNow(n: number): string {
  return new Date(Date.now() + n * 24 * 60 * 60 * 1000).toISOString();
}

function makeStatus(overrides: Partial<HeadroomPricingStatus> = {}): HeadroomPricingStatus {
  const now = new Date();
  const graceStart = hoursAgo(2);
  const graceEnd = new Date(new Date(graceStart).getTime() + 72 * 60 * 60 * 1000).toISOString();
  return {
    authenticated: false,
    localGraceStartedAt: graceStart,
    localGraceEndsAt: graceEnd,
    localGraceActive: true,
    accountSyncError: null,
    needsAuthentication: false,
    optimizationAllowed: true,
    shouldNudge: false,
    nudgeLevel: 0,
    gateReason: null,
    gateMessage: "",
    nudgeThresholdPercent: null,
    effectiveNudgeThresholdsPercent: null,
    disableThresholdPercent: null,
    effectiveDisableThresholdPercent: null,
    recommendedSubscriptionTier: null,
    claude: {
      authMethod: "claude_ai_oauth",
      email: null,
      displayName: null,
      planTier: "free",
      hasExtraUsageEnabled: false,
    },
    account: null,
    launchDiscountActive: false,
    ...overrides,
  };
}

function makeTrialAccount(trialEndsAt: string): HeadroomAccountProfile {
  return {
    email: "user@example.com",
    trialStartedAt: daysFromNow(-11),
    trialEndsAt,
    trialActive: true,
    subscriptionActive: false,
    subscriptionTier: null,
    inviteCode: null,
    acceptedInvitesCount: 0,
    inviteBonusPercent: 0,
  };
}

describe("maybeFireTrialNotifications", () => {
  afterEach(() => {
    invokeMock.mockReset();
    isVisibleMock.mockReset();
  });

  describe("window visibility gate", () => {
    it("does not fire any notification when the window is visible", async () => {
      isVisibleMock.mockResolvedValue(true);
      installStorage();
      const status = makeStatus({ localGraceStartedAt: hoursAgo(2), localGraceActive: true });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("treats isVisible errors as visible to avoid spamming on failure", async () => {
      isVisibleMock.mockRejectedValue(new Error("bridge down"));
      installStorage();
      const status = makeStatus({ localGraceStartedAt: hoursAgo(2), localGraceActive: true });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });
  });

  describe("single notifying webview", () => {
    it("fires only from the main webview, and not while any Headroom window is on screen", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({ localGraceEndsAt: hoursFromNow(15.5) });

      windows.label = "launcher";
      await maybeFireTrialNotifications(status);
      expect(invokeMock).not.toHaveBeenCalled();

      windows.label = "main";
      windows.otherVisible = true;
      await maybeFireTrialNotifications(status);
      expect(invokeMock).not.toHaveBeenCalled();

      windows.otherVisible = false;
      await maybeFireTrialNotifications(status);
      expect(invokeMock).toHaveBeenCalledOnce();
    });

    it("fires once when two overlapping ticks race for the same slot", async () => {
      isVisibleMock.mockResolvedValue(false);
      invokeMock.mockResolvedValue(undefined);
      installStorage();
      const grace = makeStatus({ localGraceEndsAt: hoursFromNow(15.5) });
      const trial = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(2)),
      });

      await Promise.all([
        maybeFireTrialNotifications(grace),
        maybeFireTrialNotifications(grace),
      ]);
      expect(invokeMock).toHaveBeenCalledOnce();

      await Promise.all([
        maybeFireTrialNotifications(trial),
        maybeFireTrialNotifications(trial),
      ]);
      expect(invokeMock).toHaveBeenCalledTimes(2);
    });
  });

  describe("grace period notifications", () => {
    it("fires the 16-hour-left notification when 16 or fewer hours remain", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const graceEnd = hoursFromNow(15.5);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Start Your Headroom Trial",
        body: expect.stringContaining("hours left"),
        action: "signup",
      });
    });

    it("fires the 8-hour-left notification when 8 or fewer hours remain", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const graceEnd = hoursFromNow(7.5);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Start Your Headroom Trial",
        body: expect.any(String),
        action: "signup",
      });
    });

    it("fires the 1-hour-left notification when 1 or fewer hours remain", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const graceEnd = hoursFromNow(0.5);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Start Your Headroom Trial",
        body: expect.any(String),
        action: "signup",
      });
    });

    it("does not fire when more than 16 hours remain", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const graceEnd = hoursFromNow(17);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("does not repeat a threshold that was already sent", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage({ headroom_grace_notif_threshold: "1" }); // 8h-left threshold already sent
      const graceEnd = hoursFromNow(7.5);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("sends the next threshold when a lower one was already sent", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage({ headroom_grace_notif_threshold: "0" }); // 16h-left sent, 8h-left not yet
      const graceEnd = hoursFromNow(7.5);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledOnce();
    });

    it("records the threshold index in localStorage after sending", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const graceEnd = hoursFromNow(15.5);
      const status = makeStatus({ localGraceEndsAt: graceEnd });

      await maybeFireTrialNotifications(status);

      expect(localStorage.setItem).toHaveBeenCalledWith(
        "headroom_grace_notif_threshold",
        "0"
      );
    });

    it("skips grace notification when user is authenticated", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        authenticated: true,
        localGraceEndsAt: hoursFromNow(7.5),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("skips grace notification when grace period has expired", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        localGraceStartedAt: hoursAgo(73),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("uses urgent copy when fewer than 3 hours remain", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage({ headroom_grace_notif_threshold: "1" }); // 8h-left sent, 1h-left eligible
      const graceEnd = hoursFromNow(0.5);
      const status = makeStatus({
        localGraceEndsAt: graceEnd,
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Start Your Headroom Trial",
        body: expect.stringContaining("hour(s) left"),
        action: "signup",
      });
    });

    it("best-effort swallows invoke errors without throwing", async () => {
      isVisibleMock.mockResolvedValue(false);
      invokeMock.mockRejectedValueOnce(new Error("notifications disabled"));
      installStorage();
      const status = makeStatus({ localGraceEndsAt: hoursFromNow(15.5) });

      await expect(maybeFireTrialNotifications(status)).resolves.toBeUndefined();
    });
  });

  describe("trial expiry notifications", () => {
    it("fires a notification when the trial ends in 3 days", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(3)),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Headroom Trial Ending Soon",
        body: "Your Headroom trial ends in 3 days. Upgrade to keep optimization enabled.",
        action: "billing",
      });
    });

    it("fires a notification when the trial ends in 2 days", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(2)),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Headroom Trial Ending Soon",
        body: "Your Headroom trial ends in 2 days. Upgrade to keep optimization enabled.",
        action: "billing",
      });
    });

    it("leads with what the trial has saved so far", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const account = makeTrialAccount(daysFromNow(2));
      const status = makeStatus({ localGraceActive: false, authenticated: true, account });
      const today = localDayKey(new Date());

      await maybeFireTrialNotifications(status, [
        {
          date: today,
          estimatedSavingsUsd: 34,
          estimatedTokensSaved: 12_300_000,
          actualCostUsd: 0,
          totalTokensSent: 0,
        },
      ]);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Headroom Trial Ending Soon",
        body:
          "So far in your trial, Headroom has saved you 12.3M tokens, about $34 at API prices. " +
          "Your Headroom trial ends in 2 days. Upgrade to keep optimization enabled.",
        action: "billing",
      });
    });

    it("uses tomorrow copy when exactly 1 day remains", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(1)),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Headroom Trial Ending Soon",
        body: "Your Headroom trial ends tomorrow. Upgrade today to keep optimization enabled.",
        action: "billing",
      });
    });

    it("says the trial ends today when it ends later the same local day", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      vi.useFakeTimers();
      vi.setSystemTime(new Date(2026, 0, 15, 9, 0, 0)); // 9 AM local
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(new Date(2026, 0, 15, 15, 0, 0).toISOString()),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Headroom Trial Ending Soon",
        body: "Your Headroom trial ends today. Upgrade now to keep optimization enabled.",
        action: "billing",
      });
    });

    it("says tomorrow for a trial ending tomorrow afternoon", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      vi.useFakeTimers();
      vi.setSystemTime(new Date(2026, 0, 15, 9, 0, 0)); // 30 hours before the end
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(new Date(2026, 0, 16, 15, 0, 0).toISOString()),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Headroom Trial Ending Soon",
        body: "Your Headroom trial ends tomorrow. Upgrade today to keep optimization enabled.",
        action: "billing",
      });
    });

    it("throttles by local calendar day, not UTC day", async () => {
      vi.stubEnv("TZ", "America/Los_Angeles");
      isVisibleMock.mockResolvedValue(false);
      vi.useFakeTimers();
      // The morning notice already fired on Jan 15. 6 PM Pacific is 02:00 UTC
      // on Jan 16, but it is still Jan 15 for the user.
      vi.setSystemTime(new Date(2026, 0, 15, 18, 0, 0));
      installStorage({ headroom_trial_expiry_notif_date: "2026-01-15" });
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(new Date(2026, 0, 17, 12, 0, 0).toISOString()),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("does not fire when more than 3 days remain", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(5)),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("does not fire when the trial has already expired", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(-1)),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("does not fire a second time on the same calendar day", async () => {
      isVisibleMock.mockResolvedValue(false);
      const today = localDayKey(new Date());
      installStorage({ headroom_trial_expiry_notif_date: today });
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(2)),
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("records today's date in localStorage after sending", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const today = localDayKey(new Date());
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: makeTrialAccount(daysFromNow(2)),
      });

      await maybeFireTrialNotifications(status);

      expect(localStorage.setItem).toHaveBeenCalledWith(
        "headroom_trial_expiry_notif_date",
        today
      );
    });

    it("skips when the trial is not active", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: {
          ...makeTrialAccount(daysFromNow(2)),
          trialActive: false,
        },
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("skips when a subscription is already active", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: true,
        account: {
          ...makeTrialAccount(daysFromNow(2)),
          subscriptionActive: true,
        },
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("skips when no account is present", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = makeStatus({
        localGraceActive: false,
        authenticated: false,
        account: null,
      });

      await maybeFireTrialNotifications(status);

      expect(invokeMock).not.toHaveBeenCalled();
    });
  });

  describe("intro offer notification", () => {
    const introOffer = { active: true, percentOff: 25, annualPercentOff: 12.5, durationMonths: 3 };
    const savedToday = () => [
      {
        date: localDayKey(new Date()),
        estimatedSavingsUsd: 4.2,
        estimatedTokensSaved: 2_100_000,
        actualCostUsd: 0,
        totalTokensSent: 0,
      },
    ];
    const usageTrial = (usageDaysLeft: number) =>
      makeStatus({
        localGraceActive: false,
        authenticated: true,
        introOffer,
        account: {
          ...makeTrialAccount(daysFromNow(13)),
          trialStartedAt: hoursAgo(5),
          trialUsageDaysLeft: usageDaysLeft,
        },
      });

    it("pitches the offer once, as soon as the trial has saved something", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();

      await maybeFireTrialNotifications(usageTrial(6), savedToday());
      await maybeFireTrialNotifications(usageTrial(5), savedToday());

      expect(invokeMock).toHaveBeenCalledTimes(1);
      expect(invokeMock).toHaveBeenCalledWith("show_notification", {
        title: "Intro offer: 25% off your first 3 months",
        body:
          "So far in your trial, Headroom has saved you 2.1M tokens, about $4 at API prices. " +
          "Upgrade to keep it running after your trial.",
        action: "billing",
      });
    });

    it("waits until the trial has saved something worth naming", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();

      await maybeFireTrialNotifications(usageTrial(6), []);

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("stays quiet without a live intro offer", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();
      const status = usageTrial(6);

      await maybeFireTrialNotifications({ ...status, introOffer: { ...introOffer, active: false } }, savedToday());

      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("leaves the last two days of use to the trial-ending notice", async () => {
      isVisibleMock.mockResolvedValue(false);
      installStorage();

      await maybeFireTrialNotifications(usageTrial(2), savedToday());

      expect(invokeMock).toHaveBeenCalledTimes(1);
      expect(invokeMock.mock.calls[0][1].title).toBe("Headroom Trial Ending Soon");
    });
  });
});
