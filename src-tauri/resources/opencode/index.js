// Headroom Desktop's OpenCode plugin: the wheel's bundle, with the proxy
// defaulting to the desktop's 127.0.0.1:6767 instead of the CLI's 8787.
process.env.HEADROOM_PROXY_URL ??= process.env.HEADROOM_BASE_URL ?? "http://127.0.0.1:6767";
export { default } from "./headroom-plugin.js";
