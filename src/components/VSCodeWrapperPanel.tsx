import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export function VSCodeWrapperPanel() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let active = true;
    void invoke<boolean>("get_vscode_wrapper_enabled")
      .then((value) => active && setEnabled(value))
      .catch((error) => console.error("Failed to read the VS Code process wrapper setting", error));
    return () => {
      active = false;
    };
  }, []);

  async function toggle() {
    setBusy(true);
    try {
      setEnabled(await invoke<boolean>("set_vscode_wrapper_enabled", { enabled: !enabled }));
    } catch (error) {
      console.error("Failed to update the VS Code process wrapper setting", error);
    } finally {
      setBusy(false);
    }
  }

  return (
    <article className="soft-card panel-card">
      <div className="panel-card__header">
        <div>
          <h3>VS Code panel restart</h3>
          <p className="panel-card__subtitle">
            Let Headroom restart Claude Code in the VS Code panel for Remote Control. Turn this off
            if your security policy blocks the process wrapper. Proxy routing and tool search stay
            enabled. Off by default on Windows.
          </p>
        </div>
        <button
          aria-checked={enabled ?? false}
          aria-label={`${enabled ? "Disable" : "Enable"} the VS Code panel restart`}
          className={`connector-switch${enabled ? " is-on" : ""}`}
          disabled={enabled === null || busy}
          onClick={() => void toggle()}
          role="switch"
          type="button"
        >
          <span className="connector-switch__thumb" />
        </button>
      </div>
    </article>
  );
}
