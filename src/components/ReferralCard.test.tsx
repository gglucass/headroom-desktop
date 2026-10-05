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
    render(<ReferralCard code="AB12CD34" signups={0} subscribed={0} freeMonths={0} rewardPending={false} />);

    expect(screen.getByText("https://extraheadroom.com/r/AB12CD34")).toBeInTheDocument();
    // Sign-in has no code field since rc7; the code goes in Upgrade afterwards.
    expect(screen.getByText(/once they've signed in to Headroom, they can add code/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Copy link/ }));

    expect(writeText).toHaveBeenCalledWith("https://extraheadroom.com/r/AB12CD34");
    expect(await screen.findByRole("button", { name: /Copied/ })).toBeInTheDocument();
  });

  it("shows the referral overview and mentions the user's own pending month", () => {
    render(<ReferralCard code="AB12CD34" signups={5} subscribed={3} freeMonths={4} rewardPending />);

    expect(screen.getByText("Friends signed up").nextSibling).toHaveTextContent("5");
    expect(screen.getByText("Subscribed").nextSibling).toHaveTextContent("3");
    expect(screen.getByText("Free months earned").nextSibling).toHaveTextContent("4");
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
