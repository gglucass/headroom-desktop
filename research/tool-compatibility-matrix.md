# Headroom Tool Compatibility Matrix

This file is the starting point for the v1 research gate. Headroom only ships adapters for tools that fit the local-only, Python-runtime-only policy.

| Tool | Category | Runtime | Local-only fit | Install method | Maintenance view | Decision | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Headroom | Prompt optimization | Python | Excellent | Managed Python install | Core dependency | Include | Mandatory default stage for every client. |
| Vitals | Project health / code analysis | Python | Excellent | Managed Python install | Core dependency | Include | Primary daily scanner in v1. |
| claw-compactor | Prompt optimization | Python | Promising | Optional managed install | Medium | Research | Include only if adapter IO is stable enough for reliable orchestration. |
| rtk | Token optimization | Rust binary | Strong | Managed binary install + Claude hook setup | Core dependency | Include | Install by default so Claude Code bash commands are auto-rewritten through RTK. |
| claude-cognitive | Workflow enhancement | Outside v1 policy | Weak | Manual external setup | Medium | Defer | Deferred because it breaks the Python-only boundary and assumes user profile edits. |

## Research checklist

- Confirm license compatibility and pin exact versions before bundling.
- Verify each tool can run inside Headroom-managed storage without relying on host-global installs.
- Verify installation/update flow can be fully local after download.
- Verify tooling has a stable CLI or library surface for adapter integration.
- Reject candidates that require profile mutation, cloud services, or non-Python runtime dependencies.
