import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export interface QuitConfirmModalProps {
  onClose: () => void;
}

/// Shown by Quit when an agent sent a request in the last hour (Rust
/// `quit_strands_sessions`). Quitting points every client back at its provider,
/// but a running session read Headroom's address at start and keeps calling it
/// until restarted; Pause keeps answering it.
export function QuitConfirmModal({ onClose }: QuitConfirmModalProps) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function pause() {
    setBusy(true);
    setError(null);
    try {
      await invoke("pause_headroom");
      onClose();
    } catch (err) {
      setError(`Headroom could not pause: ${String(err)}`);
      setBusy(false);
    }
  }

  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="quit-confirm-title"
      onClick={() => {
        if (!busy) {
          onClose();
        }
      }}
    >
      <div className="modal-card" onClick={(event) => event.stopPropagation()}>
        <h3 id="quit-confirm-title">Quit Headroom?</h3>
        <p>
          Agents you have open, like Claude Code or ChatGPT Codex, will stop working until you
          restart them.
        </p>
        <p>Pausing turns Headroom off and keeps them working.</p>
        {error ? <p className="install-progress__error">{error}</p> : null}
        <div className="modal-actions">
          <button
            className="secondary-button"
            disabled={busy}
            onClick={() => void invoke("quit_headroom", { confirmed: true })}
            type="button"
          >
            Quit anyway
          </button>
          <button
            className="primary-button"
            disabled={busy}
            onClick={() => void pause()}
            type="button"
          >
            {busy ? "Pausing..." : "Pause instead"}
          </button>
        </div>
      </div>
    </div>
  );
}
