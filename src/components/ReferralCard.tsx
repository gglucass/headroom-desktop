import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CopySimple } from "@phosphor-icons/react";

// Paid referral program. Shown only to a subscriber who can refer: the server
// sends referralCode exactly then. Rewards are granted server-side
// (headroom-web ReferralRewardJob) once the friend has paid for a month.
export interface ReferralCardProps {
  code: string;
  signups: number;
  subscribed: number;
  freeMonths: number;
  rewardPending: boolean;
}

export const referralUrl = (code: string) => `https://extraheadroom.com/r/${code}`;

export function ReferralCard({ code, signups, subscribed, freeMonths, rewardPending }: ReferralCardProps) {
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
        Or, once they've signed in to Headroom, they can add code <strong>{code}</strong> under Upgrade.
      </p>
      <dl className="referral-card__stats">
        <div>
          <dt>Friends signed up</dt>
          <dd>{signups}</dd>
        </div>
        <div>
          <dt>Subscribed</dt>
          <dd>{subscribed}</dd>
        </div>
        <div>
          <dt>Free months earned</dt>
          <dd>{freeMonths}</dd>
        </div>
      </dl>
      {rewardPending ? (
        <p>Your own free month, from the friend who invited you, arrives once you've been subscribed for a month.</p>
      ) : null}
    </section>
  );
}

// For a signed-in user who hasn't paid and wasn't referred at sign-in: the
// one place to add a friend's code later. The server decides validity.
export function ReferralCodeEntry({ onApplied }: { onApplied: () => void }) {
  const [open, setOpen] = useState(false);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!open) {
    return (
      <button className="link-button paywall__referral-toggle" onClick={() => setOpen(true)} type="button">
        Have a referral code from a friend?
      </button>
    );
  }

  async function apply() {
    setBusy(true);
    setError(null);
    try {
      await invoke("apply_headroom_referral_code", { code });
      onApplied();
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="referral-card__link">
        <input
          aria-label="Referral code"
          className="paywall__auth-input"
          onChange={(event) => {
            setCode(event.target.value);
            setError(null);
          }}
          placeholder="Referral code"
          value={code}
        />
        <button
          className="secondary-button secondary-button--small"
          disabled={!code.trim() || busy}
          onClick={() => void apply()}
          type="button"
        >
          {busy ? "Adding..." : "Add code"}
        </button>
      </div>
      {error ? <p className="install-progress__error">{error}</p> : null}
    </>
  );
}
