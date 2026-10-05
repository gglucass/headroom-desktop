import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { ReferralCard, ReferralCodeEntry } from "./ReferralCard";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));

describe("ReferralCard", () => {
  it("shows the share link and copies it", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    render(<ReferralCard code="AB12CD34" rewardsEarned={0} rewardPending={false} />);

    expect(screen.getByText("https://extraheadroom.com/r/AB12CD34")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Copy link/ }));

    expect(writeText).toHaveBeenCalledWith("https://extraheadroom.com/r/AB12CD34");
    expect(await screen.findByRole("button", { name: /Copied/ })).toBeInTheDocument();
    expect(screen.queryByText(/subscribed so far/)).not.toBeInTheDocument();
  });

  it("counts converted friends and mentions the user's own pending month", () => {
    render(<ReferralCard code="AB12CD34" rewardsEarned={2} rewardPending />);

    expect(screen.getByText(/2 friends subscribed so far/)).toBeInTheDocument();
    expect(screen.getByText(/arrives once you\x27ve been subscribed for a month/)).toBeInTheDocument();
  });

  it("adds a friend's code after sign-in and shows the server's refusal", async () => {
    const onApplied = vi.fn();
    invokeMock.mockRejectedValueOnce("Referral codes are for new subscribers.").mockResolvedValueOnce(undefined);
    render(<ReferralCodeEntry onApplied={onApplied} />);

    await userEvent.click(screen.getByRole("button", { name: "Have a referral code from a friend?" }));
    await userEvent.type(screen.getByLabelText("Referral code"), "AB12CD34");
    await userEvent.click(screen.getByRole("button", { name: "Add code" }));
    expect(await screen.findByText("Referral codes are for new subscribers.")).toBeInTheDocument();
    expect(onApplied).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("button", { name: "Add code" }));
    expect(invokeMock).toHaveBeenLastCalledWith("apply_headroom_referral_code", { code: "AB12CD34" });
    expect(onApplied).toHaveBeenCalledTimes(1);
  });
});
