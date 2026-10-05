// Email + one-time-code sign-in block. Controlled by App state/handlers so the
// same form renders inside TermsGate (paywall-first onboarding) and the
// launcher paywall stage without duplicating auth logic.
export interface AuthCodeFormProps {
  lead?: string;
  email: string;
  onEmailChange: (value: string) => void;
  emailValid: boolean;
  code: string;
  onCodeChange: (value: string) => void;
  codeRequested: boolean;
  requestBusy: boolean;
  verifyBusy: boolean;
  error: string | null;
  success: string | null;
  onRequestCode: () => void;
  onVerify: () => void;
}

export function AuthCodeForm({
  lead,
  email,
  onEmailChange,
  emailValid,
  code,
  onCodeChange,
  codeRequested,
  requestBusy,
  verifyBusy,
  error,
  success,
  onRequestCode,
  onVerify
}: AuthCodeFormProps) {
  return (
    <div className="paywall__auth soft-card">
      {lead ? <p className="paywall__auth-lead">{lead}</p> : null}
      <div className="paywall__auth-row">
        <input
          className="paywall__auth-input"
          onChange={(event) => onEmailChange(event.target.value)}
          placeholder="you@example.com"
          type="email"
          value={email}
        />
        <button
          className="secondary-button"
          disabled={!emailValid || requestBusy}
          onClick={onRequestCode}
          type="button"
        >
          {requestBusy ? "Sending…" : codeRequested ? "Resend code" : "Send code"}
        </button>
      </div>
      {codeRequested ? (
        <div className="paywall__auth-row paywall__auth-reveal">
          <input
            autoFocus
            className="paywall__auth-input"
            onChange={(event) => onCodeChange(event.target.value)}
            placeholder="6-digit code"
            value={code}
          />
          <button
            className="primary-button"
            disabled={!code.trim() || verifyBusy}
            onClick={onVerify}
            type="button"
          >
            {verifyBusy ? "Verifying…" : "Verify"}
          </button>
        </div>
      ) : null}
      {error ? <p className="install-progress__error">{error}</p> : null}
      {success && !error ? (
        <p className="paywall__auth-success paywall__auth-reveal">{success}</p>
      ) : null}
    </div>
  );
}
