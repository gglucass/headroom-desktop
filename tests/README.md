# Testing Notes

Current automated coverage includes:

- frontend seed-data, dashboard helper, and component render tests via Vitest
- frontend app-helper tests for upgrade-plan selection and invoke error handling
- frontend pricing helper tests plus a coverage gate via `npm run test:coverage`
- Rust unit tests for dashboard state assembly, updater endpoint parsing, and client adapter helpers
- Rust dashboard state tests isolated to temp app-data directories instead of the real user profile
- managed runtime path/bootstrap smoke coverage behind the existing opt-in network guard

Release gating:

- `./scripts/verify-release.sh` runs frontend coverage thresholds and the Rust suite together
- `npm run build:mac:dmg` runs that gate before building artifacts locally
- `.github/workflows/release-macos.yml` runs the same gate before publishing a release

Still valuable follow-up coverage:

- proxy routing integration tests
- client adapter fixture tests that exercise real file rewrites in temp homes
- telemetry persistence tests
- frontend onboarding flow tests
