# Headroom Architecture

Headroom v1 is split into a small desktop shell and a local daemon-oriented backend:

- `src/`: Tauri frontend for the tray UI, onboarding surfaces, live activity, and research visibility.
- `src-tauri/src/lib.rs`: Tauri command entrypoints and tray wiring.
- `src-tauri/src/state.rs`: top-level application state and dashboard shaping.
- `src-tauri/src/tool_manager.rs`: bootstrap/runtime/tool installation boundary.
- `src-tauri/src/client_adapters.rs`: client detection and guided setup contract.
- `src-tauri/src/pipeline.rs`: request-stage summary model for prompt optimization flows.
- `src-tauri/src/insights.rs`: daily local recommendation generation.
- `research/tool-compatibility-matrix.md`: v1 inclusion gate for external tools.

## Bootstrap strategy

The downloadable app stays small because it ships only the Tauri shell, Rust daemon, and installer logic. Third-party Python components are fetched after first launch into a Headroom-managed application support directory.

## v1 boundaries

- macOS is the only polished target for v1.
- `headroom` is required.
- `rtk` is required.
- `vitals` is included as the primary scanner.
- Managed tools may be Python-based or standalone binaries when Headroom owns the install path.
- Client configuration changes require explicit user consent and rollback support.
