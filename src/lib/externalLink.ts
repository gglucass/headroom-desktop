import { invoke } from "@tauri-apps/api/core";

// For a click with nowhere to show an error. A machine with no default
// browser (RUST-N1: ShellExecuteW code 31) or a broken xdg-open (RUST-N4)
// rejects every open, and an unhandled rejection was all the click did. The
// link goes to the clipboard instead, so it can be pasted into a browser.
export function openLinkFromClick(url: string): void {
  invoke("open_external_link", { url }).catch(() =>
    navigator.clipboard?.writeText(url).catch(() => undefined)
  );
}
