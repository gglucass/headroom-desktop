# Headroom Desktop - cut Claude Code & Codex token costs by ~50%

**Headroom is a desktop tray app for macOS, Windows, and Linux that cuts [Claude Code](https://www.anthropic.com/claude-code) and [ChatGPT Codex](https://openai.com/codex/) token costs by ~50% - without changing how you code.** It also routes [OpenCode](https://opencode.ai) and Grok Build through the same pipeline. It runs a local-first optimization pipeline that reversibly compresses the tool output, logs, and boilerplate that bloat every prompt, so the AI plan you already pay for stretches about 2x further. Nothing the model needs is lost - it can pull the original content back on demand.

> **Paid product.** Headroom is a paid subscription app (7-day free trial, no credit card required; plans from $4/mo, or $3/mo billed annually) - see [pricing](https://extraheadroom.com/pricing). The desktop shell in this repo is MIT-licensed and open source, but the app requires a Headroom account and an active plan to run.

[![Website](https://img.shields.io/badge/extraheadroom.com-website-blue?style=for-the-badge)](https://extraheadroom.com)&nbsp;&nbsp;[![Download](https://img.shields.io/github/v/release/gglucass/headroom-desktop?label=Download&style=for-the-badge&color=000000)](https://github.com/gglucass/headroom-desktop/releases/latest)

> **macOS:** 14 (Sonoma) or later, Apple Silicon or Intel (universal build)
>
> **Windows:** Windows 10 or later, x64
>
> **Linux (preview):** x86_64, glibc 2.39 or newer (Ubuntu 24.04, Debian 13), plus a running secret-service keyring provider (gnome-keyring or kwallet) for sign-in. Linux builds ship with every stable release, but the platform is still in preview and the app says so

### Install

**Option A - Homebrew (macOS):**

```bash
brew install --cask headroom
```

**Option B - manual download:**

1. Go to the [latest release](https://github.com/gglucass/headroom-desktop/releases/latest)
2. **macOS:** download the `.dmg` (for example `Headroom_0.9.3_mac.dmg`), open it, drag **Headroom** to Applications
3. **Windows:** download and run the `_x64-setup.exe` installer
4. **Linux:** download the `.AppImage` (has the built-in updater) or the `.deb` (same build, updates only by downloading a newer one)
5. Launch Headroom - it appears in your menu bar / system tray and walks you through setup

Headroom is signed and notarized on macOS, so it opens without Gatekeeper warnings. All install paths self-update via the app's built-in updater; the Homebrew cask stays current with each release too.

Every stable release ships all three platforms from the same tag; Linux is still in preview. The Linux runtime installs a proxy-focused subset of the Python stack, so the memory and ML extras that the macOS and Windows builds ship are absent there.

### For AI agents

If you are a coding agent installing or diagnosing Headroom for your user, follow the [agent runbook](https://extraheadroom.com/docs/for-agents) - every step that works from a shell, plus the one sign-in step that stays with the human. Detect a running install with `curl -s http://127.0.0.1:6767/stats` (JSON back = running). Machine-readable product summary: [extraheadroom.com/llms.txt](https://extraheadroom.com/llms.txt); markdown docs by appending `.md` to any docs URL.

---

![Savings dashboard - total costs and input tokens saved, with daily history](docs/app-dashboard.png)

![Activity feed - per-request compression detail](docs/app-activity.png)

![Project learnings - Headroom updates agent memory from your agents' repeated mistakes](docs/app-learnings.png)

![Addons screen - one-click token-saving addons](docs/app-addons.png)

---

> **Note:** Headroom supports **Claude Code**, **ChatGPT Codex** (CLI and desktop app), **OpenCode**, and **Grok Build**. Support for additional clients is planned.

Headroom is a local-first desktop tray app that routes your coding clients through a local optimization pipeline. Stable builds ship for macOS, Windows, and Linux (preview). It installs and manages a self-contained Python runtime, bundles proven token-saving tools, and surfaces savings analytics - all without touching your system environment.

## How it works

Headroom sits in your menu bar (system tray on Windows and Linux) and does three things:

1. **Installs a managed Python runtime** into Headroom-owned storage - isolated from your system Python, no `pip install --user` pollution.
2. **Chains token-saving tools** (`headroom` for prompt optimization, `rtk` for CLI output compression) between your client and the LLM API.
3. **Shows you the math** - daily and monthly savings charts, per-client token stats, and pipeline health.

The app ships as a slim Tauri shell (~a few MB). Heavy Python components are fetched on first launch and kept in `~/Library/Application Support/Headroom` (or the platform's app-data directory on Windows and Linux).

### Proxy topology

Headroom is a two-hop local proxy. Nothing leaves `127.0.0.1` except the final upstream call, which goes to the same API your client would have called directly.

```
Claude Code / ChatGPT Codex / OpenCode / Grok Build
    |  ANTHROPIC_BASE_URL = http://127.0.0.1:6767
    |  OPENAI_BASE_URL    = http://127.0.0.1:6767/v1
    v
:6767  Rust intercept proxy (always-on, fixed port)
    |  forwards locally
    v
:6768  managed Python backend (headroom + rtk pipeline)
    |  optimizes request/response, then calls the real API
    v
api.anthropic.com / api.openai.com
```

- **:6767** is the fixed intercept port. Clients are pointed at it, so it never moves. It is the only port your client config references.
- **:6768** is the internal hop between the intercept proxy and the Python optimization backend. It is configurable and, if the default is already bound, the proxy probes `6768-6790` and falls back to the first free port.
- Routing is set two ways so both interactive shells and GUI launches pick it up: an `env` block in `~/.claude/settings.json` (Claude Code) and an `export` in a managed shell block (fenced with `# >>> headroom:... >>>` markers). Codex uses a provider block in `~/.codex/config.toml` plus `OPENAI_BASE_URL`. OpenCode gets Headroom base URLs for its anthropic and openai providers in its config, and Grok Build gets a `GROK_CLI_CHAT_PROXY_BASE_URL` export in the managed shell block.
- On quit or uninstall, every one of these redirects is removed and clients talk to the upstream API directly again (see below).

## What Headroom changes on your system

Full disclosure of every location Headroom writes to, so you can decide before installing. The install screen in the app shows the same list, and the uninstall flow reverses every item. Paths below are the macOS locations; Windows and Linux use the platform equivalents.

**On install:**

- Downloads a self-contained Python runtime (~3 GB on disk) under `~/Library/Application Support/Headroom`. Your system Python is untouched.
- Adds a `PreToolUse` hook to `~/.claude/settings.json` and a script at `~/.claude/hooks/headroom-rtk-rewrite.sh` so Claude Code routes through Headroom. A timestamped backup of `settings.json` is written before any edit.
- For Codex, adds a Headroom provider block to `~/.codex/config.toml` and an `OPENAI_BASE_URL` export to your managed shell block so the Codex CLI and desktop app route through the local proxy. The TOML block is fenced with `# >>> headroom:... >>>` markers, a backup is written before any edit, and existing Codex threads are retagged to the managed provider.
- For OpenCode, points the anthropic and openai provider base URLs in OpenCode's config at the local proxy and installs a small transport plugin. A backup is written before any edit.
- For Grok Build, adds a `GROK_CLI_CHAT_PROXY_BASE_URL` export to the managed shell block.
- Creates `~/Library/Application Support/Headroom` for logs, caches, and per-client setup state.
- Stores your Headroom session token in the macOS Keychain (Windows Credential Manager / Linux secret-service on those platforms) under services prefixed `com.extraheadroom.headroom`.
- If you opt into "launch at login," installs a LaunchAgent plist at `~/Library/LaunchAgents/`. Never added otherwise.
- Adds a managed block to your shell profile (`.zshrc`, `.zprofile`, etc.) that prepends Headroom's managed `bin` directory (under `~/Library/Application Support/Headroom`) to `PATH` so `rtk` is available in your terminals. Every managed block is fenced with `# >>> headroom:... >>>` markers and can be removed by hand if you prefer.

**On quit (or pause):** Headroom tears down everything that would intercept your clients - the Claude Code hook entry and hook script, the `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` redirects, the Codex provider block in `~/.codex/config.toml`, the OpenCode provider URLs and plugin, and the managed shell blocks (including the Grok Build export). Codex threads are retagged back to their native provider. Claude Code and Codex behave exactly as they did before Headroom was launched. The Python runtime, logs, and keychain entries stay on disk so the next launch is fast.

**On uninstall (Settings → Uninstall Headroom):** Everything listed above is removed, including the LaunchAgent plist, `~/Library/Preferences/com.extraheadroom.headroom*`, `~/Library/Caches/com.extraheadroom.headroom`, and the keychain entries. The uninstall dialog in the app shows the full list before you confirm.

If the proxy dies unexpectedly, a watchdog restarts it; after repeated failures it auto-pauses and strips interception so your clients keep working without intervention.

## Bundled tools

| Tool | What it does | Default |
|------|-------------|---------|
| [headroom](https://pypi.org/project/headroom-ai/) | Prompt optimization pipeline (Python) | Required |
| [rtk](https://github.com/rtk-ai/rtk) | Token-optimized shell command proxy for your coding agent and your terminal | Opt-in add-on |
| [markitdown](https://github.com/microsoft/markitdown) | Converts PDFs and Office documents to clean Markdown before the agent reads them | Opt-in add-on |
| [serena](https://github.com/oraios/serena) | MCP server with symbol-level code tools, so the agent reads one function instead of a whole file | Opt-in add-on |
| [codebase memory](https://github.com/DeusData/codebase-memory-mcp) | MCP server that indexes the codebase into a persistent knowledge graph for structure questions | Opt-in add-on |
| [context7](https://github.com/upstash/context7) | MCP server that fetches current, version-specific library docs | Opt-in add-on |
| [ponytail](https://github.com/DietrichGebert/ponytail) | Plugin that nudges the agent toward leaner, less over-engineered code | Opt-in add-on |
| [caveman](https://github.com/JuliusBrussee/caveman) | Plugin that makes replies terse, cutting output tokens while keeping code and errors exact | Opt-in add-on |
| [chisle](https://github.com/JayPokale/Chisle) | Plugin that combines terse replies and minimal code (what ponytail and caveman do) and trims long command output | Opt-in add-on |

**Tool inclusion policy:** only tools that run entirely locally, inside Headroom-managed storage, with a stable CLI surface make it in. No cloud dependencies, no host profile mutations. See [`research/tool-compatibility-matrix.md`](research/tool-compatibility-matrix.md).

## Compression benchmarks

Numbers from the [headroom](https://github.com/headroomlabs-ai/headroom) open-source library that powers the optimization pipeline, summarized from the current published benchmarks page.

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
| QA preservation | SQuAD v2 + HotpotQA on raw HTML vs. extracted content | +0.02 F1, +2% exact match vs. raw HTML | - |
| Multi-tool agent test | Agno agent with 4 tools investigating a memory leak | Same findings as baseline | 76.3% |

### What compresses well vs. what doesn't

| Content type | Typical savings | Notes |
|-------------|-----------------|-------|
| JSON arrays (search results, API responses, DB rows) | 86–100% | Primary use case |
| Structured logs | 82–95% | Errors and anomalies always preserved |
| Agentic conversations (25–50 turns) | 56–81% | |
| Plain text / documentation | 43–46% | Cost savings only, adds latency |
| Source code | Mostly passthrough | Code in active messages is protected by default - see limitations |

### Limitations worth knowing

- **Code compression is intentionally conservative.** Code in recent messages (last 4 by default) and any conversation where the user is asking about code (`analyze`, `debug`, `fix`, etc.) is never compressed. The savings from code come from dropping old, no-longer-relevant messages - not from stripping function bodies.
- **Short content is skipped.** Arrays under 5 items and content under 200 tokens pass through unchanged.
- **Text compression (LLMLingua) adds latency.** It requires a ~2 GB model download on first use and doesn't break even on fast models. Useful for cost reduction, not speed.
- **Plain-text RAG results pass through.** Compression targets tool outputs and JSON; plain text in user messages is not compressed.

Full methodology and reproducible benchmarks: [headroom benchmarks](https://docs.headroomlabs.ai/docs/benchmarks) · [limitations](https://docs.headroomlabs.ai/docs/limitations)

## Interesting design decisions

- **Zero host pollution.** Headroom owns its entire dependency tree. Uninstalling the app leaves your shell, your Python, and your PATH exactly as they were (except for the optional `rtk` PATH addition, which is reversible).
- **Rust shell, Python brain.** The Tauri/Rust layer handles tray lifecycle, managed installs, client detection, and update delivery. The optimization work happens in Python, where the headroom ecosystem lives.
- **Client config with rollback.** When Headroom edits a supported client's config (e.g. Claude Code settings), it writes a backup first. Disabling or uninstalling restores the original.
- **Open source shell, private web.** The desktop app is MIT-licensed and open source. The marketing site and account backend live in a separate private repo - so contributors can build and run the full desktop experience without needing backend access.

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

## Release flow

Updates ship outside the app stores via Tauri's built-in updater. Stable releases build macOS, Windows, and Linux artifacts from the same tag. The app polls GitHub Releases in the background, prompts before installing, and requests a restart to finish. Both local builds and the GitHub Actions workflows run `./scripts/verify-release.sh` - a failing test blocks the build before anything is published.

See [`docs/macos-release.md`](docs/macos-release.md) for the full release setup.

### Branching and versioning

Use `./scripts/bump-version.sh <version>` to update all five version files at once (`package.json`, `package-lock.json`, `src-tauri/tauri.conf.json`, `Cargo.toml`, `Cargo.lock`). Accepts `X.Y.Z` or `X.Y.Z-rc.N` (leading `v` is stripped).

Two release channels are wired into CI:

- **`main`** - stable channel. Users on the default download get updates from here. Version must be plain `X.Y.Z`. Branch-protected: direct pushes are rejected; changes land via PR only.
- **`staging`** - release candidate channel. Installs via a separate build pointing at the rolling `staging` GitHub release. Version must be `X.Y.Z-rc.N`.

Work happens on `feature/*` branches, which merge into `staging` for testing. Stable promotions land on `main` via a release PR (see below).

**Release candidate flow:**

1. Merge work from a feature branch into `staging`.
2. Bump `package.json` + `src-tauri/tauri.conf.json` to `X.Y.Z-rc.N` (e.g. `0.9.4-rc.1`) and push. `.github/workflows/release-macos-staging.yml` publishes a versioned prerelease tag `vX.Y.Z-rc.N` and mirrors the artifacts to the rolling `staging` release.
3. The staging test machine auto-updates (it has both endpoints baked in and routes itself to the staging endpoint because its installed version has an `-rc` suffix).
4. If something is wrong, bump to `rc.2` and push again. Repeat until the build is good.

**Promoting to stable:**

`main` is branch-protected, so promotions go through a release PR. The merge **must** be a merge commit (not squash, not rebase) so the staging commits - including the rc tag's commit - remain in `main`'s history and the rc-ancestor check passes.

1. From the verified `staging` tip, cut a release branch: `git checkout -b release/X.Y.Z staging`.
2. On that branch, run `./scripts/bump-version.sh X.Y.Z` (strips `-rc.N`), commit, push.
3. Open a PR from `release/X.Y.Z` into `main`.
4. Merge with **"Create a merge commit"**. `.github/workflows/release-macos.yml` triggers on the push to `main` and publishes the stable release.
5. The main workflow **enforces** that a `vX.Y.Z-rc.N` prerelease exists whose commit is an ancestor of the stable (merge) commit. If not, the build fails. This guarantees stable only ships code that was tested via the staging channel.
6. After the stable build is published, the main workflow re-points the rolling `staging` release at the stable DMG. The staging machine receives that as an update, installs it, and - because the new version is plain `X.Y.Z` - automatically switches to the stable endpoint for all future checks.
7. Delete the `release/X.Y.Z` branch.

> Recommended: in **Settings → General → Pull Requests**, leave only "Allow merge commits" enabled and disable squash and rebase merges. The rc-ancestor check already rejects squashed/rebased promotions at build time, but disabling them at the repo level prevents an accidental click that lands a broken bump on `main`.

**Bypassing the rc check:**

For hotfixes where a staging cycle is impractical, include `[skip-rc-check]` in the **PR merge commit message** (the workflow reads the merge commit's message, not the bump commit's). Easiest path: put `[skip-rc-check]` in the PR title or first body line so GitHub includes it in the auto-generated merge commit. Use sparingly - the guard exists to prevent untested stable releases.

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

Clean Rust build artifacts (the `src-tauri/target/` directory grows quickly):

```bash
cargo clean --manifest-path src-tauri/Cargo.toml
```

## Dependency pinning

`headroom-ai` is installed from a specific pinned wheel on first run. Automatic upgrades are disabled - the app ships with one known-good version and only changes what it installs when the release artifact itself is updated.

Three constants in [`src-tauri/src/tool_manager.rs`](src-tauri/src/tool_manager.rs) control the pin:

- `HEADROOM_PINNED_VERSION` - the version string (e.g. `"0.37.0"`). Must match the wheel URL.
- `HEADROOM_PINNED_WHEEL_URL` - the exact PyPI wheel URL to download.
- `HEADROOM_PINNED_SHA256` - the wheel's SHA-256, verified after download.

To bump `headroom-ai`: update all three constants together, run the build, and ship a new desktop release. Users pick up the new Python dependency as part of the desktop update flow - there is no separate PyPI check or background upgrade path.

Other bundled components (`rtk`, the Python standalone runtime, the vendor wheels index) are pinned the same way - one version, one checksum, per platform.
