import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import type { UpstreamOverrideView } from "../lib/types";

/// Settings for an Anthropic-compatible provider (GLM, Kimi, DeepSeek) that
/// Headroom should route to. The base URL is the whole switch: empty is
/// Anthropic, anything else wins over a cc-switch provider change. Saving
/// restarts the proxy -- the upstream is read at boot, so a running proxy
/// keeps serving the previous one.
export function UpstreamPanel() {
  const [baseUrl, setBaseUrl] = useState("");
  const [hasToken, setHasToken] = useState(false);
  // Empty means "leave the stored token alone", which is why the field starts
  // blank even when one is set. Only a touched field is ever sent.
  const [token, setToken] = useState("");
  const [tokenTouched, setTokenTouched] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const apply = useCallback((next: UpstreamOverrideView) => {
    setBaseUrl(next.baseUrl);
    setHasToken(next.hasToken);
    setToken("");
    setTokenTouched(false);
  }, []);

  useEffect(() => {
    let active = true;
    void invoke<UpstreamOverrideView>("get_upstream_override")
      .then((current) => {
        if (active) {
          apply(current);
        }
      })
      .catch((err) => {
        if (active) {
          setError(String(err));
        }
      })
      .finally(() => {
        if (active) {
          setLoading(false);
        }
      });
    return () => {
      active = false;
    };
  }, [apply]);

  const configured = baseUrl.trim() !== "";

  const save = useCallback(async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await invoke<UpstreamOverrideView>("save_upstream_override", {
        // Off clears the stored URL and token backend-side, so an emptied
        // field is how the user turns the provider back off.
        mode: configured ? "override" : "off",
        baseUrl,
        token: tokenTouched ? token : null,
      });
      apply(saved);
      setNotice(
        saved.mode === "off"
          ? "Provider removed. Headroom restarted on Anthropic."
          : "Saved. Headroom restarted on this provider.",
      );
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  }, [apply, baseUrl, configured, token, tokenTouched]);

  return (
    <article className="soft-card panel-card">
      <div className="panel-card__header">
        <div>
          <h3>Provider</h3>
          <p className="panel-card__subtitle">
            Route Headroom at an Anthropic-compatible endpoint instead of Anthropic.
            Leave the URL empty to use Anthropic.
          </p>
        </div>
      </div>

      {loading ? (
        <p className="upstream-panel__meta">Loading…</p>
      ) : (
        <div className="upstream-panel">
          <label className="upstream-field">
            <span>Base URL</span>
            <span className="upstream-field__input">
              <input
                aria-label="Provider base URL"
                autoComplete="off"
                disabled={busy}
                onChange={(event) => setBaseUrl(event.target.value)}
                placeholder="https://api.z.ai/api/anthropic"
                spellCheck={false}
                type="text"
                value={baseUrl}
              />
            </span>
          </label>

          <label className="upstream-field">
            <span>Auth token</span>
            <span className="upstream-field__input">
              <input
                aria-label="Provider auth token"
                autoComplete="off"
                disabled={busy}
                onChange={(event) => {
                  setToken(event.target.value);
                  setTokenTouched(true);
                }}
                placeholder={hasToken ? "Stored — type to replace" : "Paste the provider token"}
                spellCheck={false}
                type="password"
                value={token}
              />
            </span>
          </label>
          <p className="upstream-panel__meta">
            Kept in your OS keychain and written to ~/.claude/settings.json, which is
            where your client reads it from. Headroom forwards the token your client
            sends and never adds one of its own.
          </p>

          <div className="upstream-panel__actions">
            <button
              className="secondary-button secondary-button--small"
              disabled={busy}
              onClick={() => void save()}
              type="button"
            >
              {busy ? "Saving…" : "Save and restart"}
            </button>
            {hasToken && configured ? (
              <button
                className="addon-card__link"
                disabled={busy}
                onClick={() => {
                  setToken("");
                  setTokenTouched(true);
                }}
                type="button"
              >
                Remove stored token
              </button>
            ) : null}
          </div>

          {configured ? (
            <p className="upstream-panel__meta">
              Third-party endpoints run lossless compaction only, so payloads stay close
              to what your client sent.
            </p>
          ) : null}
          {error ? <p className="install-progress__error">{error}</p> : null}
          {notice ? <p className="install-progress__notice">{notice}</p> : null}
        </div>
      )}
    </article>
  );
}
