# Testing Notes

Current automated coverage includes:

- frontend seed-data, dashboard helper, and component render tests via Vitest
- frontend app-helper tests for upgrade-plan selection and invoke error handling
- frontend pricing helper tests plus a coverage gate via `npm run test:coverage`
- frontend OptimizePanel interaction tests (modal open/close, delete IPC, busy state, error surfacing) via `@testing-library/react`
- frontend launcher transition helper tests covering `getInitialLauncherStage`, `nextAutoConfigureStep`, and the post-apply step
- Rust unit tests for dashboard state assembly, updater endpoint parsing, and client adapter helpers
- Rust dashboard state tests isolated to temp app-data directories instead of the real user profile
- Rust client_adapters lifecycle tests (apply → verify → disable → clear) under a temp `$HOME` via `serial_test`
- Rust proxy_intercept end-to-end tests for bearer-token capture + backend forwarding and the 502 path on unreachable backends
- Rust keychain debug-store round-trip tests under a temp `$HOME`
- Rust lib.rs decoder tests for `classify_bootstrap_failure` and `read_applied_patterns_for_project` / `delete_applied_pattern`
- managed runtime path/bootstrap smoke coverage behind the existing opt-in network guard

Release gating:

- `./scripts/verify-release.sh` runs frontend coverage thresholds and the Rust suite together
- `npm run build:mac:dmg` runs that gate before building artifacts locally
- `.github/workflows/release-macos.yml` runs the same gate before publishing a release

Known gaps / follow-up coverage:

- App.tsx (4846 lines) still has no direct render tests — only the launcher
  transition helpers it now consumes are covered. Adding `@testing-library/react`
  render tests for `LauncherShell` would close this.
- Telemetry JSONL append failure modes (disk full, partial write) — the
  snapshot path is well-covered but the append path is not.
