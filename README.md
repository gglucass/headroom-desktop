# Headroom Desktop

**Cut your LLM API bills by 60–90% without changing how you code.**

> **Pricing:** Headroom has a free tier and paid plans — see [pricing](https://extraheadroom.com/pricing). The desktop shell in this repo is MIT-licensed; account features and paid plans are opt-in.

[![Website](https://img.shields.io/badge/extraheadroom.com-website-blue?style=for-the-badge)](https://extraheadroom.com)&nbsp;&nbsp;[![Download for macOS](https://img.shields.io/github/v/release/gglucass/headroom-desktop?label=Download%20for%20macOS&style=for-the-badge&logo=apple&logoColor=white&color=000000)](https://github.com/gglucass/headroom-desktop/releases/latest)

> **Stable:** macOS on Apple Silicon (M1 or later)
>
> **Preview:** Linux x86_64 builds are experimental and currently support the core proxy flow only.

### Install

1. Go to the [latest release](https://github.com/gglucass/headroom-desktop/releases/latest)
2. On macOS, download the `.dmg` file (for example `Headroom_0.2.9.dmg`)
3. Open the DMG, drag **Headroom** to Applications
4. Launch Headroom — it appears in your menu bar and walks you through setup

Headroom is signed and notarized, so macOS will open it without Gatekeeper warnings.

Linux preview artifacts are published on the same release page. Today they are best treated as a preview for the core Headroom proxy, Claude Code routing, and RTK flow. `Headroom Learn` is not supported yet on Linux preview builds.

---

![Headroom dashboard showing $204 saved across 66.8M tokens](docs/screenshot-1.png)

---

> **Note:** Headroom currently supports **Claude Code** only. Support for additional clients is planned.

Headroom is a local-first desktop tray app that routes your coding clients through a local optimization pipeline. The stable target is macOS; Linux builds are currently experimental. It installs and manages a self-contained Python runtime, bundles proven token-saving tools, and surfaces savings analytics — all without touching your system environment.

## How it works

Headroom sits in your menu bar and does three things:

1. **Installs a managed Python runtime** into Headroom-owned storage — isolated from your system Python, no `pip install --user` pollution.
2. **Chains token-saving tools** (`headroom` for prompt optimization, `rtk` for CLI output compression) between your client and the LLM API.
3. **Shows you the math** — daily and monthly savings charts, per-client token stats, and pipeline health.

The app ships as a slim Tauri shell (~a few MB). Heavy Python components are fetched on first launch and kept in `~/Library/Application Support/Headroom`.

## What Headroom changes on your system

Full disclosure of every location Headroom writes to, so you can decide before installing. The install screen in the app shows the same list, and the uninstall flow reverses every item.

**On install:**

- Downloads a self-contained Python runtime (~2 GB) to `~/.headroom`. Your system Python is untouched.
- Adds a `PreToolUse` hook to `~/.claude/settings.json` and a script at `~/.claude/hooks/headroom-rtk-rewrite.sh` so Claude Code routes through Headroom. A timestamped backup of `settings.json` is written before any edit.
- Creates `~/Library/Application Support/Headroom` for logs, caches, and per-client setup state.
- Stores your Headroom session token in the macOS Keychain under services prefixed `com.extraheadroom.headroom`.
- If you opt into "launch at login," installs a LaunchAgent plist at `~/Library/LaunchAgents/`. Never added otherwise.
- Adds a managed block to your shell profile (`.zshrc`, `.zprofile`, etc.) that prepends `~/.headroom/bin` to `PATH` so `rtk` is available in your terminals. Every managed block is fenced with `# >>> headroom:... >>>` markers and can be removed by hand if you prefer.

**On quit (or pause):** Headroom tears down everything that would intercept Claude Code — the hook entry, the hook script, the `ANTHROPIC_BASE_URL` redirect, and the managed shell blocks. Claude Code behaves exactly as it did before Headroom was launched. The Python runtime, logs, and keychain entries stay on disk so the next launch is fast.

**On uninstall (Settings → Uninstall Headroom):** Everything listed above is removed, including the LaunchAgent plist, `~/Library/Preferences/com.extraheadroom.headroom*`, `~/Library/Caches/com.extraheadroom.headroom`, and the keychain entries. The uninstall dialog in the app shows the full list before you confirm.

If the proxy dies unexpectedly, a watchdog restarts it; after repeated failures it auto-pauses and strips interception so Claude Code keeps working without intervention.

## Bundled tools

| Tool | What it does | Default |
|------|-------------|---------|
| [headroom](https://pypi.org/project/headroom-ai/) | Prompt optimization pipeline (Python) | Required |
| [rtk](https://github.com/gglucass/rtk) | Rewrites Claude Code bash commands to strip noise before it reaches the context window | Auto-enabled |
| vitals | Project health scanner — flags stale deps, large files, drift | Included |

**Tool inclusion policy:** only tools that run entirely locally, inside Headroom-managed storage, with a stable CLI surface make it in. No cloud dependencies, no host profile mutations. See [`research/tool-compatibility-matrix.md`](research/tool-compatibility-matrix.md).

## Compression benchmarks

Numbers from the [headroom](https://github.com/chopratejas/headroom) open-source library that powers the optimization pipeline, summarized from the current published benchmarks page.

### Current benchmark summary

| Benchmark | What it tests | Result |
|-----------|---------------|--------|
| Scrapinghub article extraction | Extract article bodies from 181 HTML pages while removing boilerplate | 0.919 F1, 98.2% recall, **94.9% compression** |
| SmartCrusher JSON compression | Find a critical error in 100 production log entries after compression | 4/4 correct, **87.6% compression** |
| QA accuracy preservation | Ask the same questions on raw HTML vs. extracted content | 0.87 F1 vs. 0.85 baseline, 62% exact match vs. 60% |
| Multi-tool agent test | 4-tool agent investigating a memory leak with compressed tool output | 6,100 vs. 15,662 tokens sent, **76.3% compression**, same findings |

### Benchmark details

| Benchmark | Setup | Accuracy | Compression |
|-----------|-------|----------|-------------|
| HTML extraction | Scrapinghub article extraction benchmark, 181 pages | 0.919 F1, 0.879 precision, 0.982 recall | 94.9% |
| JSON compression | 100 production log entries, critical error at position 67 | 4/4 correct answers | 87.6% |
| QA preservation | SQuAD v2 + HotpotQA on raw HTML vs. extracted content | +0.02 F1, +2% exact match vs. raw HTML | — |
| Multi-tool agent test | Agno agent with 4 tools investigating a memory leak | Same findings as baseline | 76.3% |

### What compresses well vs. what doesn't

| Content type | Typical savings | Notes |
|-------------|-----------------|-------|
| JSON arrays (search results, API responses, DB rows) | 86–100% | Primary use case |
| Structured logs | 82–95% | Errors and anomalies always preserved |
| Agentic conversations (25–50 turns) | 56–81% | |
| Plain text / documentation | 43–46% | Cost savings only, adds latency |
| Source code | Mostly passthrough | Code in active messages is protected by default — see limitations |

### Limitations worth knowing

- **Code compression is intentionally conservative.** Code in recent messages (last 4 by default) and any conversation where the user is asking about code (`analyze`, `debug`, `fix`, etc.) is never compressed. The savings from code come from dropping old, no-longer-relevant messages — not from stripping function bodies.
- **Short content is skipped.** Arrays under 5 items and content under 200 tokens pass through unchanged.
- **Text compression (LLMLingua) adds latency.** It requires a ~2 GB model download on first use and doesn't break even on fast models. Useful for cost reduction, not speed.
- **Plain-text RAG results pass through.** Compression targets tool outputs and JSON; plain text in user messages is not compressed.

Full methodology and reproducible benchmarks: [chopratejas/headroom benchmarks](https://chopratejas.github.io/headroom/benchmarks/) · [limitations](https://chopratejas.github.io/headroom/LIMITATIONS/)

## Interesting design decisions

- **Zero host pollution.** Headroom owns its entire dependency tree. Uninstalling the app leaves your shell, your Python, and your PATH exactly as they were (except for the optional `rtk` PATH addition, which is reversible).
- **Rust shell, Python brain.** The Tauri/Rust layer handles tray lifecycle, managed installs, client detection, and update delivery. The optimization work happens in Python, where the headroom ecosystem lives.
- **Client config with rollback.** When Headroom edits a supported client's config (e.g. Claude Code settings), it writes a backup first. Disabling or uninstalling restores the original.
- **Open source shell, private web.** The desktop app is MIT-licensed and open source. The marketing site and account backend live in a separate private repo — so contributors can build and run the full desktop experience without needing backend access.

## Project structure

```
src/              React + Tauri frontend (tray UI, onboarding, savings dashboard)
src-tauri/        Rust backend
  state.rs        Dashboard state and data shaping
  tool_manager.rs Bootstrap, Python runtime, and tool installation
  client_adapters.rs  Client detection and guided setup
  insights.rs     Daily local recommendation engine
research/         Tool vetting artifacts and compatibility matrix
docs/             Architecture notes, release process
```

## macOS release flow

Updates ship outside the App Store via Tauri's built-in updater. The app polls GitHub Releases in the background, prompts before installing, and requests a restart to finish. Both local DMG builds and the GitHub Actions workflow run `./scripts/verify-release.sh` — a failing test blocks the build before anything is published.

See [`docs/macos-release.md`](docs/macos-release.md) for the full release setup.

### Branching and versioning

Use `./scripts/bump-version.sh <version>` to update all four version files at once (`package.json`, `package-lock.json`, `src-tauri/tauri.conf.json`, `Cargo.toml`). Accepts `X.Y.Z` or `X.Y.Z-rc.N` (leading `v` is stripped).

Two release channels are wired into CI:

- **`main`** — stable channel. Users on the default download get updates from here. Version must be plain `X.Y.Z`.
- **`staging`** — release candidate channel. Installs via a separate build pointing at the rolling `staging` GitHub release. Version must be `X.Y.Z-rc.N`.

Work happens on `feature/*` branches, which merge into `staging` for testing, which merges into `main` for promotion. `main` is fast-forward only from `staging`.

**Release candidate flow:**

1. Merge work from a feature branch into `staging`.
2. Bump `package.json` + `src-tauri/tauri.conf.json` to `X.Y.Z-rc.N` (e.g. `0.2.44-rc.1`) and push. `.github/workflows/release-macos-staging.yml` publishes a versioned prerelease tag `vX.Y.Z-rc.N` and mirrors the artifacts to the rolling `staging` release.
3. The staging test machine auto-updates (it has both endpoints baked in and routes itself to the staging endpoint because its installed version has an `-rc` suffix).
4. If something is wrong, bump to `rc.2` and push again. Repeat until the build is good.

**Promoting to stable:**

1. Fast-forward merge `staging` → `main`.
2. Bump version to plain `X.Y.Z` (strip `-rc.N`) and push. `.github/workflows/release-macos.yml` publishes the stable release.
3. The main workflow **enforces** that a `vX.Y.Z-rc.N` prerelease exists whose commit is an ancestor of the stable commit. If not, the build fails. This guarantees stable only ships code that was tested via the staging channel.
4. After the stable build is published, the main workflow re-points the rolling `staging` release at the stable DMG. The staging machine receives that as an update, installs it, and — because the new version is plain `X.Y.Z` — automatically switches to the stable endpoint for all future checks.

**Bypassing the rc check:**

For hotfixes where a staging cycle is impractical, include `[skip-rc-check]` in the commit message that bumps the version on `main`. Use sparingly — the guard exists to prevent untested stable releases.

## Development

```bash
npm install
npm run tauri dev
```

For the live auth and pricing flow, create a `.env`:

```bash
HEADROOM_ACCOUNT_API_BASE_URL="https://extraheadroom.com/api/v1"
HEADROOM_APTABASE_APP_KEY="REPLACE_WITH_APTABASE_APP_KEY"
VITE_SENTRY_DSN="REPLACE_WITH_SENTRY_DSN"
VITE_HEADROOM_SALES_CONTACT_URL="mailto:hello@extraheadroom.com"
VITE_HEADROOM_CONTACT_FORM_URL="https://extraheadroom.com/contact_request"
```

See [`.env.example`](.env.example) for the complete list, including the optional updater and macOS signing keys used for release builds. Set the same keys as GitHub Actions repository variables for production DMG builds.

Run tests:

```bash
npm run test:all          # frontend + Rust
cargo test --manifest-path src-tauri/Cargo.toml   # Rust only
```

## Dependency pinning

`headroom-ai` is installed from a specific pinned wheel on first run. Automatic upgrades are disabled — the app ships with one known-good version and only changes what it installs when the release artifact itself is updated.

Three constants in [`src-tauri/src/tool_manager.rs`](src-tauri/src/tool_manager.rs) control the pin:

- `HEADROOM_PINNED_VERSION` — the version string (e.g. `"0.8.2"`). Must match the wheel URL.
- `HEADROOM_PINNED_WHEEL_URL` — the exact PyPI wheel URL to download.
- `HEADROOM_PINNED_SHA256` — the wheel's SHA-256, verified after download.

To bump `headroom-ai`: update all three constants together, run the build, and ship a new desktop release. Users pick up the new Python dependency as part of the desktop update flow — there is no separate PyPI check or background upgrade path.

Other bundled components (`rtk`, the Python standalone runtime, the vendor wheels index) are pinned the same way — one version, one checksum, per platform.
