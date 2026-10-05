import { useState } from "react";
import { CopySimple } from "@phosphor-icons/react";

// Paid referral program. Shown only to a subscriber who can refer: the server
// sends referralCode exactly then. Rewards are granted server-side
// (headroom-web ReferralRewardJob) once the friend has paid for a month.
export interface ReferralCardProps {
  code: string;
  rewardsEarned: number;
  rewardPending: boolean;
}

export const referralUrl = (code: string) => `https://extraheadroom.com/r/${code}`;

export function ReferralCard({ code, rewardsEarned, rewardPending }: ReferralCardProps) {
  const [copyState, setCopyState] = useState<"idle" | "copied" | "error">("idle");
  const url = referralUrl(code);

  async function copy() {
    try {
      await navigator.clipboard.writeText(url);
      setCopyState("copied");
    } catch {
      setCopyState("error");
    }
    window.setTimeout(() => setCopyState("idle"), 2000);
  }

  return (
    <section className="referral-card">
      <h2>Give a month, get a month</h2>
      <p>
        When a friend subscribes through your link, you both get a free month: once they've been
        subscribed for a month, your next renewal moves a month later. Every friend adds another month.
      </p>
      <div className="referral-card__link">
        <code>{url}</code>
        <button className="secondary-button secondary-button--small" onClick={() => void copy()} type="button">
          <CopySimple size={16} weight="bold" />
          {copyState === "copied" ? "Copied" : copyState === "error" ? "Copy failed" : "Copy link"}
        </button>
      </div>
      <p>
        Or they can enter code <strong>{code}</strong> when they sign in.
        {rewardsEarned > 0
          ? ` ${rewardsEarned} friend${rewardsEarned === 1 ? "" : "s"} subscribed so far.`
          : ""}
      </p>
      {rewardPending ? (
        <p>Your own free month, from the friend who invited you, arrives once you've been subscribed for a month.</p>
      ) : null}
    </section>
  );
}
