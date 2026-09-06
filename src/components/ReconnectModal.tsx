import type { UnroutedClient } from "../lib/types";
import { unroutedBody, unroutedTitle } from "../lib/setupHealthAlert";

export interface ReconnectModalProps {
  clients: UnroutedClient[];
  onClose: () => void;
  onOpenSettings: () => void;
  /// Turn a switched-off connection back on. Only offered for agents whose
  /// connection is off; an enabled one was re-applied before this showed.
  onReconnect: (client: UnroutedClient) => void;
}

/// Shown when an agent ran on this machine while Headroom saw nothing from it
/// (Rust `detect_unrouted_clients`). Asks before flipping a connection the
/// user or the app switched off; a merely stale one was re-applied already.
export function ReconnectModal({
  clients,
  onClose,
  onOpenSettings,
  onReconnect,
}: ReconnectModalProps) {
  const switchedOff = clients.filter((client) => !client.enabled);

  return (
    <div
      className="modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-labelledby="reconnect-title"
      onClick={onClose}
    >
      <div className="modal-card setup-stall" onClick={(event) => event.stopPropagation()}>
        <h3 id="reconnect-title">{unroutedTitle(clients)}</h3>
        {clients.map((client) => (
          <p key={client.clientId}>{unroutedBody(client)}</p>
        ))}
        <div className="modal-actions">
          <button className="secondary-button" onClick={onOpenSettings} type="button">
            Open settings
          </button>
          {switchedOff.length > 0 ? (
            switchedOff.map((client) => (
              <button
                key={client.clientId}
                className="primary-button"
                onClick={() => onReconnect(client)}
                type="button"
              >
                Turn {client.name} connection back on
              </button>
            ))
          ) : (
            <button className="primary-button" onClick={onClose} type="button">
              Got it
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
