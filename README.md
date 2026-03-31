# Headroom Desktop

Headroom is a local-first desktop tray app for coding-focused LLM workflows. It routes supported clients through a local optimization pipeline powered by `headroom`.

## What is implemented here

- Tauri 2 desktop shell scaffold with a playful dashboard-oriented UI
- Rust backend modules for:
  - local dashboard state
  - self-contained managed Python bootstrap/runtime layout
  - managed tool installation for Headroom and RTK
  - client detection
  - daily insights
  - research compatibility matrix
- Research and architecture docs aligned with the v1 plan

## Project structure

- `src/`: React/Tauri frontend
- `src-tauri/`: Rust backend and Tauri configuration
- `research/`: tool inclusion research artifacts
- `docs/`: architecture notes

The marketing/download website now lives in a separate private repo so the desktop app can stay open source without exposing the web app source.

## Client setup

- Headroom configures supported clients to route through its local optimization pipeline.
- Headroom installs `rtk` into Headroom-managed storage, adds it to the user's shell `PATH`, and enables Claude Code bash auto-rewrite by default.
- The current app UI focuses on setup, savings visibility, and runtime health.

## Next implementation steps

1. Replace the bootstrap placeholders with real managed Python downloads and package installs inside Headroom-managed storage.
2. Implement the local proxy/gateway that routes supported client traffic through Headroom.
3. Add config adapters that can safely modify supported client settings with rollback support.
4. Add optional tools after the Headroom + RTK baseline is stable.
5. Add persistent telemetry storage and real historical insights.

## Dependency pinning policy

- Headroom is pinned in-app to `headroom-ai[all]==0.5.16` from PyPI for stable releases.
- For each new Headroom app release, validate compatibility against the latest released Headroom version before deciding whether to bump the pin.

## macOS release flow

- Headroom is wired for outside-the-App-Store macOS updates using Tauri's official updater flow.
- The app checks in the background, prompts before installing, and asks the user to restart after the update is installed.
- Release setup details live in [`docs/macos-release.md`](docs/macos-release.md).

## Development

Install dependencies and then run:

```bash
npm install
npm run tauri dev
```

To enable the live Pro checkout button in the desktop app, set a Polar Checkout Link in a local `.env` file:

```bash
VITE_HEADROOM_POLAR_PRO_CHECKOUT_URL="https://polar.sh/your-organization/checkout?products=your-product-price"
```

Polar's official checkout-link API/docs:
- [Create Checkout Link](https://polar.sh/docs/api-reference/checkout-links/create)
- [Create Checkout Session](https://polar.sh/docs/api-reference/checkouts/create-session)

Run Rust tests with:

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```
