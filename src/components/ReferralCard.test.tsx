import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { ReferralCard } from "./ReferralCard";

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
});
