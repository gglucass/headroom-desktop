# Testing Notes

Current automated coverage includes:

- frontend seed-data, dashboard helper, and component render tests via Vitest
- Rust unit tests for dashboard state assembly, updater endpoint parsing, and client adapter helpers
- managed runtime path/bootstrap smoke coverage behind the existing opt-in network guard

Still valuable follow-up coverage:

- proxy routing integration tests
- client adapter fixture tests that exercise real file rewrites in temp homes
- telemetry persistence tests
- frontend onboarding flow tests
