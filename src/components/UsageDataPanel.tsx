import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/// On/off for anonymous usage analytics and crash reports. On by default. Off
/// stops them at once; the account and license checks keep running.
export function UsageDataPanel() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [infoOpen, setInfoOpen] = useState(false);

  useEffect(() => {
    let active = true;
    void invoke<boolean>("get_usage_data_enabled")
      .then((value) => active && setEnabled(value))
      .catch(() => active && setEnabled(true));
    return () => {
      active = false;
    };
  }, []);

  async function toggle() {
    setBusy(true);
    try {
      setEnabled(await invoke<boolean>("set_usage_data_enabled", { enabled: enabled === false }));
    } catch (error) {
      console.error("Failed to update the usage data setting", error);
    } finally {
      setBusy(false);
    }
  }

  return (
    <article className="soft-card panel-card">
      <div className="panel-card__header">
        <div>
          <h3 className="usage-data-title">
            Usage analytics and crash reports
            <button
              aria-expanded={infoOpen}
              aria-label="Show details for usage analytics and crash reports"
              className="connector-help"
              onClick={() => setInfoOpen((open) => !open)}
              type="button"
            >
              i
            </button>
          </h3>
          {infoOpen ? (
            <p className="connector-tooltip">
              Send anonymous usage events and crash reports so we can find and fix problems. Turning
              this off does not affect your account or license checks.
            </p>
          ) : null}
        </div>
        <button
          aria-checked={enabled ?? true}
          aria-label={`${enabled === false ? "Enable" : "Disable"} usage analytics and crash reports`}
          className={`connector-switch${enabled === false ? "" : " is-on"}`}
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
