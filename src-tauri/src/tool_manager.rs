use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use parking_lot::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::backend_port::{self, AllForeign, SelectedFallback};
use crate::models::{ManagedTool, RtkTodayStats, ToolStatus};

/// Pinned headroom-ai version. Upgrade logic is disabled; this exact version
/// will be installed if the currently-installed version differs.
///
/// 0.20.x–0.24.x upstream shipped a maturin/Rust-native wheel that was both
/// per-Python-version and per-platform (e.g. `cp312-cp312-macosx_11_0_arm64`,
/// upstream #355). Starting with 0.25.0 the native module is built against the
/// CPython stable ABI (abi3, upstream #516), so a single `cp310-abi3` wheel per
/// platform now covers every CPython >= 3.10 — the pin below
/// (`cp310-abi3-macosx_11_0_arm64`) installs cleanly on our bundled cp312 and
/// stays valid if `PYTHON_STANDALONE_RELEASE` later moves to 3.13+. Only the
/// per-platform axis still matters, which `headroom_wheel_artifact` handles —
/// when bumping this pin, re-pick every platform's wheel URL/sha256 from
/// https://pypi.org/pypi/headroom-ai/<version>/json.
pub(crate) const HEADROOM_PINNED_VERSION: &str = "0.37.0";
const HEADROOM_SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Kill the RUST-9F onnxruntime import probe after this long: a native
/// DLL-init DEADLOCK (as opposed to the usual crash) must read as "timed
/// out", not hang the already-failing startup path the probe reports on.
const ONNX_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// markitdown's `--help` cold-imports a much heavier converter stack
/// (onnxruntime, magika, pdfminer, …) than the core `import headroom`. On
/// macOS 26 the first run of freshly-installed *unsigned* wheel binaries is
/// scanned by Gatekeeper/EDR, which routinely pushes that first import past
/// 15s and trips the smoke-test SIGKILL (RUST-22). It is warn-only, so a too
/// tight bound just produces Sentry noise and false "addon broken" signals.
const MARKITDOWN_SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound on the one-time `learn --verbosity` baseline seed run before
/// proxy start. Typical runs are a few seconds (a ~100MB transcript project
/// seeds in ~3s); the cap only trips on pathological corpora, after which the
/// proxy starts anyway and seeding retries next launch.
const HEADROOM_BASELINE_SEED_TIMEOUT: Duration = Duration::from_secs(30);
/// Index of pre-built wheels for sdist-only PyPI packages (e.g. hnswlib).
/// GitHub's expanded_assets endpoint serves HTML anchors pip can consume via --find-links.
const VENDOR_WHEELS_INDEX_URL: &str =
    "https://github.com/gglucass/headroom-desktop/releases/expanded_assets/vendor-wheels-v1";
/// Never let pip build a dependency from source. A user's machine is not
/// guaranteed to have Xcode Command Line Tools (nor the Rust toolchain
/// pydantic-core's sdist needs), and requiring either to install a desktop app
/// is not a bargain we get to make. Every pin we install has a wheel for every
/// platform we ship -- the one sdist-only dependency, hnswlib, is served as a
/// prebuilt wheel from `VENDOR_WHEELS_INDEX_URL` -- so in the healthy case this
/// flag changes nothing. It only bites when a wheel is *missing*: without it
/// pip silently falls back to the sdist and spends minutes compiling before
/// dying on a clang/cargo error no user can act on; with it pip fails at once
/// with "no matching distribution found", which `classify_bootstrap_failure`
/// already recognises and explains. Deliberately NOT applied to the optional
/// markitdown/serena addons: their transitive sets are unpinned and unaudited,
/// and a failed addon leaves a working Headroom behind.
const PIP_ONLY_BINARY: &str = "--only-binary=:all:";
// headroom binds on the backend port chosen at spawn time (default 6768);
// the intercept layer on 6767 forwards to it. The backend port is dynamic
// because something else on the machine (e.g. rapportd) can claim 6768 at
// login — see `backend_port` for the selection logic.
fn headroom_proxy_port() -> String {
    backend_port::get().to_string()
}
const HEADROOM_PROXY_URL: &str = "http://127.0.0.1:6767";
const MCP_METHOD_CLAUDE_CLI: &str = "claude_cli";
const MCP_METHOD_FALLBACK_JSON: &str = "fallback_json";
const MCP_METHOD_DIRECT_CLAUDE_JSON: &str = "direct_claude_json";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum McpInstallMethod {
    ClaudeCli,
    FallbackJson,
    DirectClaudeJson,
}

impl McpInstallMethod {
    fn as_str(self) -> &'static str {
        match self {
            McpInstallMethod::ClaudeCli => MCP_METHOD_CLAUDE_CLI,
            McpInstallMethod::FallbackJson => MCP_METHOD_FALLBACK_JSON,
            McpInstallMethod::DirectClaudeJson => MCP_METHOD_DIRECT_CLAUDE_JSON,
        }
    }
}
const HEADROOM_STARTUP_POLL_MS: u64 = 250;
const HEADROOM_STARTUP_TIMEOUT_MS: u64 = 300_000;

const HEADROOM_REQUIREMENTS_LOCK: &str = include_str!("../python/headroom-requirements.lock");
const HEADROOM_LINUX_REQUIREMENTS_LOCK: &str =
    include_str!("../python/headroom-linux-requirements.lock");
const HEADROOM_WINDOWS_REQUIREMENTS_LOCK: &str =
    include_str!("../python/headroom-windows-requirements.lock");

/// Full-file SHA-256 values of historical headroom-requirements.lock shipments
/// whose pinned versions are byte-for-byte identical to the current lock.
/// Receipts holding one of these shas are treated as up-to-date; the receipt is
/// silently migrated to the comment-insensitive sha on next launch so the
/// entry can be dropped the next time the lock file actually changes.
///
/// When modifying the lock, re-evaluate: compare the stripped (comments and
/// blank lines removed) form of each legacy lock against the stripped current
/// lock. Drop any entry that no longer matches — those users need a real
/// reinstall.
const LEGACY_REQUIREMENTS_LOCK_SHAS: &[&str] = &[
    // 0.28.0 re-resolved the lock from scratch ([all,vector], torch 2.12.1,
    // benchmark tail dropped, broad point-version churn), so its stripped sha
    // differs from every prior shipment. The 0.27.0 cohort's lock receipt is now
    // genuinely stale and SHOULD trigger a dep reinstall — do not whitelist the
    // old sha here. The list stays empty until a future no-op cosmetic lock edit
    // (comments/blank lines only, no pin moves) needs to be treated as
    // up-to-date.
];

/// Receipts strictly below this version cannot be safely upgraded in place to
/// the currently bundled headroom-ai — pip's in-place upgrade leaves stale
/// `.so`/`.dylib` files from old native-extension pins (onnxruntime,
/// tokenizers, cryptography, mmh3, py_rust_stemmers, uvloop/httptools)
/// alongside the new ones, which surfaces as "smoke test passes, boot
/// validation fails with no log lines and no port bound" — the python
/// process segfaults on import before reaching logging setup.
///
/// Bumping this floor is a release-by-release decision: when a new lock
/// adds native deps or bumps native pins ABI-incompatibly, raise the floor
/// to the previous bundled version. When the new lock only churns pure-Python
/// pins, leave the floor where it is.
///
/// Floor history:
/// - 0.3.7: set to 0.10.0. 0.3.6's lock jump from 0.8.2 → 0.19.0 added
///   fastembed/mmh3/py_rust_stemmers and bumped tokenizers/cryptography/
///   uvicorn; the failing Sentry users all had `fallback: 0.8.2` (from
///   0.2.50-era desktop). 0.3.0-rc.26 onward shipped headroom-ai 0.10.x
///   against the same lock as 0.8.2 — these users have the same dep set
///   on disk and have not produced upgrade-failure events, so we let them
///   take the cheap in-place path. If 0.10.x fallbacks start appearing in
///   Sentry, raise the floor.
/// - 0.3.8: a single `fallback: 0.10.12` boot-validation stall appeared in
///   Sentry, but a clean-VM 0.3.5 → 0.3.7 upgrade reproduced the same
///   0.10.12 → 0.19.0 in-place delta and succeeded. The N=1 failure looks
///   environmental, not universal to the 0.10.x cohort. With the new
///   "Retry with full rebuild" button as a recovery path for the
///   environmental cases, we keep the floor at 0.10.0 rather than penalize
///   the (probably ~99%) of 0.10.x users who succeed in-place. Re-evaluate
///   if multi-machine 0.10.x failures show up in 0.3.8 telemetry.
/// - 0.4.0: raised to 0.20.0. Upstream 0.20.x switched headroom-ai to a
///   maturin/Rust-native single-wheel build (upstream #355) — wheels are now
///   per-Python-version and per-platform and ship a compiled `headroom_core`
///   `.so`. 0.19.0 venvs were built against a `py3-none-any` wheel with no
///   native extension; an in-place pip upgrade onto the new wheel would
///   layer the new `.so` on top of stale transitive native pins from the
///   old lock, which is the exact segfault-on-import pattern this floor
///   exists to prevent. Atomic rebuild is the only safe path for the
///   0.10.x–0.19.x cohort on this bump.
/// - 0.4.2 (0.24.0 → 0.25.0 bundle): floor stays at 0.20.0. The lock delta is
///   two pure-Python pins (litellm, importlib-metadata); no native pin moves
///   and no native dep is added or removed. The headroom-ai wheel itself goes
///   from a per-version `cp312-cp312` native wheel to a stable-ABI `cp310-abi3`
///   wheel (upstream #516), but pip uninstalls the old wheel (clearing its
///   `cpython-312` `.so` via RECORD) before unpacking the new `abi3` `.so`, so
///   no stale headroom_core extension is layered. The 0.20.x+ cohort — which
///   includes every 0.24.0-shipping (desktop 0.4.1) user — upgrades in place
///   with no big-wheel rebuilds. Raise the floor only if a future lock adds or
///   ABI-bumps a native transitive dep.
/// - 0.4.x (0.25.0 → 0.26.0 bundle): floor stays at 0.20.0. headroom-ai's
///   `requires_dist` is byte-identical between 0.25.0 and 0.26.0, so the lock
///   is reused unchanged — no pin moves at all. pip uninstalls the 0.25.0 abi3
///   wheel (clearing its `.so` via RECORD) and unpacks the 0.26.0 abi3 wheel;
///   no stale native extension is layered. The 0.20.x+ cohort upgrades in
///   place with no wheel rebuilds.
/// - 0.4.x (0.26.0 → 0.27.0 bundle): floor stays at 0.20.0. The lock moves three
///   pins: tree-sitter-language-pack 1.8.1 → 0.13.0 (native, ships compiled
///   grammar `.so`s) and the new spreadsheet extra (et-xmlfile/openpyxl/xlrd,
///   pure-Python). The language-pack move is a version *change*, so pip
///   uninstalls 1.8.1 via its RECORD (removing the old grammar `.so`s) before
///   unpacking 0.13.0 — no stale native extension is layered, unlike the
///   same-version in-place rebuilds this floor guards against. headroom-ai's own
///   abi3 wheel is likewise uninstalled-then-reinstalled. The 0.20.x+ cohort
///   upgrades in place. Raise the floor only if a future lock adds or
///   ABI-bumps a native transitive dep without a version bump.
/// - 0.5.x (0.27.0 → 0.28.0 bundle): floor stays at 0.20.0. Native pin moves are
///   all version *changes* (torch 2.11.0 → 2.12.1, onnxruntime 1.23.2 → 1.27.0),
///   so pip uninstalls the old wheel via RECORD (clearing its `.so`) before
///   unpacking the new one — no same-version relayering. The new tree-sitter
///   grammar packages (tree-sitter-c-sharp/embedded-template/yaml) are fresh
///   installs, not in-place rebuilds, so they carry no stale-`.so` risk either.
///   hnswlib stays pinned at 0.8.0 (unchanged). The benchmark-tail packages
///   dropped from the lock are pure-Python orphans left on disk by the in-place
///   `--upgrade` (pip does not prune removed requirements); harmless. The
///   0.20.x+ cohort upgrades in place.
/// - 0.7.x (0.32.1 → 0.33.0 bundle): floor stays at 0.20.0. The lock is
///   unchanged — 0.33.0's requires_dist only *tightens* three existing
///   constraints (ast-grep-cli !=0.44.1, mcp >=1.28.1,<2.0.0 on the mcp/proxy
///   extras, ruff on the dev extra), and the shipped lock already satisfies all
///   of them (ast-grep-cli==0.44.0, mcp==1.28.1; dev isn't installed). No
///   package is added or removed from the core/extra set we install, so the
///   in-place path is a single `pip install --no-deps --force-reinstall` of the
///   abi3 wheel with zero dep churn. The new Rust ports (CodeCompressor,
///   Kompress — upstream #1154/#1153) live inside headroom-ai's own
///   `headroom_core` abi3 extension, which pip uninstalls via RECORD before
///   unpacking the replacement, so no stale `.so` is layered.
/// - 0.7.x (0.33.0 → 0.34.0 bundle): floor stays at 0.20.0. requires_dist is
///   byte-identical to 0.33.0. The lock tops up two transitive pins to match
///   upstream's #2753 CVE clearance (aiohttp 3.14.1 → 3.14.3, cryptography
///   49.0.0 → 50.0.0). Both are version *changes* of native wheels, so pip
///   uninstalls the old wheel via RECORD before unpacking the new one — no
///   same-version relayering. The lock-sha change triggers a dep top-up for
///   existing users, which is intended (they need the CVE-fixed wheels).
/// - 0.8.x (0.34.0 → 0.35.0 bundle): floor stays at 0.20.0. requires_dist
///   only moves the ruff dev pin (dev extra isn't installed). The lock tops
///   up one transitive pin to match upstream's #2839 CVE clearance: h2
///   4.3.0 → 4.4.1 (CVE-2026-71554). h2 is pure-Python, and its hpack/
///   hyperframe requirements are already satisfied by the shipped pins, so
///   the top-up is a wheel swap with no native or resolution churn.
const ATOMIC_REBUILD_FLOOR_VERSION: (u32, u32, u32) = (0, 20, 0);

/// Parse the leading `major.minor.patch` from a version string, tolerating
/// pre-release/build suffixes (`-rc.1`, `+build`, `.dev0`, etc.). Returns
/// None when the prefix isn't a numeric `major.minor`. `patch` defaults to
/// 0 when missing or unparseable, so `"0.19"` and `"0.19.0"` compare equal.
/// Injected onto the backend's PYTHONPATH so the `site` module imports it at
/// interpreter startup. `faulthandler.register` needs Python-side code — the
/// PYTHONFAULTHANDLER env only covers fatal signals — and the upstream proxy
/// has no dump hook of its own.
const SITECUSTOMIZE_PY: &str = r#""""Headroom Desktop injection (managed -- do not edit).

Registers SIGUSR1 to dump all Python thread stacks to stderr (the proxy
log). The desktop watchdog sends SIGUSR1 before force-killing a wedged
backend so the log shows what the event loop was stuck on.

Also keeps user-turn text out of lossy compression: from headroom-ai
0.34.0 the "coding" persona sets compress_user_messages=True, and the
profile kwargs force it on per request (HEADROOM_COMPRESS_USER_MESSAGES
can only force-enable, never disable), so the persona field itself is
flipped back. Verbatim user turns are deliberate desktop posture, not a
bug workaround: user messages carry the coding working set (code, errors,
paths) and Claude Code's <system-reminder> blocks (CLAUDE.md), which the
model must see verbatim. The 0.34.0 tag-split bug that made this urgent
(open/close tags landing in different router sections, so the pair never
matched and CLAUDE.md arrived word-dropped) was fixed upstream in 0.35.0
(#2887), but that only protects TAGGED blocks -- plain user text would
still compress -- so the flip stays. tool_result blocks compress
regardless of this flag (the role gate only guards text blocks), so the
coding token mass is unaffected.

Also ports eight fixes owed upstream (remove each once a wheel ships it),
gated on HEADROOM_SDK=headroom-desktop-proxy so only the backend process
pays the proxy import cost:
Context-limit guard (upstream PR #2942): compression under-reports
usage to the client, so Claude Code's proactive auto-compaction never
fires; once even the compressed request exceeds the model's real
window, every turn 400s with "prompt is too long" and the client
force-compacts on each error -- a compact-every-other-prompt loop
(churn report 2026-08-12). The guard nudges the reported message_start
input_tokens once the forwarded total nears the real window, learns
the real window from prompt-too-long 400 bodies, and makes
get_context_limit honor the context-1m beta so 1M sessions stop being
max-crushed by compression pressure. Kill switch:
HEADROOM_CONTEXT_GUARD=0.
Response-cache poisoning guard: SemanticCache.set stores any body gated
only on status_code == 200, so an empty/unparseable/error body is
replayed for the whole TTL (1h) and only a proxy restart clears it. The
guard refuses to store anything that is not a JSON object or that
carries an error payload. Kill switch: HEADROOM_RESPONSE_CACHE_GUARD=0.
Responses savings denominator guard (upstream PR #3106): /v1/responses
HTTP outcomes derive optimized = max(0, original - saved) from a
messages-only original while tokens_saved includes tool-schema
compaction measured against the tools array, so schema-heavy turns
clamp optimized to 0 and record savings rates above 100% ("4,436 -> 0,
208.4%" in the desktop feed, 2026-08-18). The guard counts the
request's tools schema once per compressed HTTP request and widens the
recorded pair at the outcome funnel so original - optimized == saved.
Kill switch: HEADROOM_RESPONSES_DENOMINATOR_GUARD=0.
Tool-schema dollar unfold (upstream PR #3170): since the 0.36.0
attribution unification, record_request folds the priced tool-schema
bucket into compression_savings_usd (lifetime, display_session,
per-model, per-project, and every history checkpoint) while the token
fields beside them stay message-only, so any $/token read on the
persisted state is inflated by 1 + tool/message -- 5.59x tool/message
measured on one real install, an implied $32.88/M next to models that
list at $10/M input. The desktop accumulates lifetime savings from
these dollar fields, so the fold would contaminate the headline the
product is trusted for (the savings-rate canary in state.rs is the
runtime tripwire for exactly this). The guard zeroes the tool_schema
bucket before the fold, restoring the 0.35.0 meaning of every
persisted dollar field; tool-schema TOKENS are untouched and the
desktop keeps pricing those itself at the cache-read rate.
Self-neutralizes once a wheel ships #3170's disjoint fields. Kill
switch: HEADROOM_SAVINGS_FOLD_GUARD=0.
Chained-read protection (upstream PR #2668): _is_read_command
inspects only the FIRST program and applies its write/redirect check
to the whole string, so a read batched behind other work
(`wc -l a.py && sed -n '1,60p' a.py`) classifies as a non-read and the
file content is lossy-compressed despite read protection -- the agent
then re-reads the file to recover exact bytes (turn inflation) or
fails the edit outright (resolve loss). The guard splits on ;/&&/||
(never on a single |: downstream pipeline stages consume output, not
files), judges redirects and tee per segment so a sibling write does
not unprotect the read beside it, keeps the heredoc whole-string
bailout (a heredoc body may contain ; or &&), and delegates each
segment's program parsing to the original function so wrapper peeling,
bash -c recursion and the lockfile carve-out stay upstream's. Kill
switch: HEADROOM_READ_CHAIN_GUARD=0.
Prefix-floor vendor (upstream PR #3380): 0.37.0 plus the full-replay
guard below resolve every compressed-vs-replayed conflict in favor of
the previously-forwarded bytes, so background/cold-start Kompress
landings are discarded forever and compression sits at ~2 percent while
the cache stays healthy (measured 2026-09-02, avg_compression_pct 1.9,
request cache-hit 98.8). The vendor execs the PR's overlay_cached_prefix
and finalize_turn into the wheel verbatim: inside the provider-confirmed
floor replay is unconditional, beyond it the size bound arbitrates each
turn, so a background compression improvement lands ONCE, is recorded as
the new replay source, and replays stably after -- the 0.35.0
one-time-bust economics that measured 26 percent input savings WITH 90
percent dollar cache hits. This is NOT the 0.9.4-rc.4 splice, which
stitched replayed-bytes-to-the-floor onto this turn's fresh pipeline
output and so shipped every turn's beyond-floor drift (22 percent fleet
cache coverage lost, 1.20 -> 0.94 reads/sent, n=17, p=0.007); the
vendored overlay keeps upstream's full alignment and bound arbitration
beyond the floor. The confirmed floor is bridged from prepare_turn's
keyword-only tracker_frozen -- the Anthropic token path is the only
0.37.0 caller that passes it, with the same pre-clamp value the PR
stashes in handler locals -- and floorless calls (OpenAI paths, cache
mode) get len(prev_returned), which through the PR's own mechanism
reproduces the 0.9.5 full replay: no path gets less cache protection
than the guard below gave. Gated on wheel version exactly 0.37.0 AND
the fix parameter absent; the first wheel shipping #3380 keeps its own
policy. Kill switch: HEADROOM_PR3380_VENDOR=0.
Prefix-replay inflation-skip guard (upstream issue #3379): since the
0.36.x non-inflation bound (#3052), overlay_cached_prefix declines to
replay the previously-forwarded prefix whenever background compression
lands a SMALLER form of already-forwarded history, so the forwarded
bytes change mid-conversation and the provider prompt cache busts from
the first changed byte -- measured 2026-09-01 as a 160-210k-token cache
re-write every 1-2 requests, dollar cache-hit rate 90% -> 52%, billable
input $/M up 2.2x, across the 0.35.0 -> 0.37.0 wheel swap. The guard
restores the 0.35.0 replay policy wholesale: v0.35.0's prefix_tracker
carries NO size bound at all (`_compact_json_bytes` does not exist
there), so it replayed the cached prefix whenever alignment allowed.
The bound arrived with #3052 and is what declines -- and a decline over
already-cached bytes IS the bust. A floor-limited port was shipped in
0.9.4 and measured WORSE than no guard: replaying only up to the
confirmed floor leaves every byte past it free to bust, and the fleet
lost 22% of cache coverage (1.20 -> 0.94 reads/sent, n=17, p=0.007)
against 4% for users who stayed on 0.9.3. Full replay is the state that
measured 26% input savings on 0.35.0, so the guard reproduces exactly
that and nothing cleverer. Gated on the
runtime NOT having the fix's parameter (enforce_non_inflation in the
first PR shape, confirmed_frozen_count in the reworked #3380 shape), so
the first wheel that ships either keeps its own replay policy and this
block goes inert. Kill switch: HEADROOM_PREFIX_REPLAY_GUARD=0. Normally inert
while the prefix-floor vendor above binds (the vendored signature trips
this guard's fixed-parameter gate); it is the fallback when the vendor
is killed or fails.
cc-switch Official-branch upstream reset (upstream PR #3166): the
reconciler captures the third-party endpoint cc-switch selected (Kimi,
DeepSeek, GLM) as this proxy's Anthropic upstream, but switching back to
Claude Official only stops it rewriting settings.json -- the captured
endpoint stays live on HeadroomProxy.ANTHROPIC_API_URL, a process-wide
class attr, so every Anthropic client still routed through this proxy
keeps reaching the old provider while sending Anthropic OAuth
credentials. The guard resets the upstream to the default when
settings.json goes back to an empty env, and only when this reconciler
is the one that captured a non-default upstream (an operator-configured
upstream is left alone). Unlike the other four this one is load-bearing,
not corrective: the desktop sets HEADROOM_CC_SWITCH_RECONCILE=1 only
because the guard binds here, so every failure path -- kill switch,
missing module, renamed method -- clears that env before the proxy reads
it, and the reconciler stays off instead of running unfixed. No version
gate: it self-neutralizes once a wheel ships #3166 (the fixed tick has
already reset the upstream by the time the wrapper looks). Kill switch:
HEADROOM_CC_SWITCH_RESET_GUARD=0, which turns the reconciler off with
it.
The same guard pins the URL the reconciler advertises to clients.
Upstream builds it from the port this proxy bound -- the internal port
between the desktop intercept and this process -- so every provider
switch rewrote the client onto that port and out of the intercept, where
the activity feed, request counts and savings accounting live. The
desktop passes the intercept URL in HEADROOM_CC_SWITCH_PROXY_URL and the
guard writes it onto every reconciler instance.
"""
import faulthandler
import signal

# No chain=True: SIGUSR1's default disposition is terminate, and chaining
# falls through to it after the dump -- the process must survive the dump so
# the watchdog controls the actual kill.
try:
    faulthandler.register(signal.SIGUSR1, all_threads=True)
except Exception:
    pass

# Protect user-turn text (CLAUDE.md system-reminders) from lossy Kompress:
# flip the coding persona back to compress_user_messages=False. Guarded so
# fallback runtimes without the persona (< 0.30.0) and half-installed venvs
# no-op cleanly.
try:
    from dataclasses import replace as _hd_replace

    import headroom.agent_savings as _hd_savings

    _hd_coding = _hd_savings._PROFILES.get("coding")
    if _hd_coding is not None and _hd_coding.compress_user_messages:
        _hd_savings._PROFILES["coding"] = _hd_replace(
            _hd_coding, compress_user_messages=False
        )
except Exception:
    pass

import os as _hd_os

if _hd_os.environ.get("HEADROOM_SDK") == "headroom-desktop-proxy":
    # Context-limit guard (upstream PR #2942; remove once a wheel ships it).
    # See the module docstring for the failure mode this breaks.
    try:
        if _hd_os.environ.get("HEADROOM_CONTEXT_GUARD", "1").strip().lower() not in (
            "0",
            "false",
            "no",
            "off",
        ):
            import contextvars as _hd_cg_cvars
            import json as _hd_cg_json
            import logging as _hd_cg_logging
            import re as _hd_cg_re

            import headroom.providers.anthropic as _hd_cg_prov
            import headroom.proxy.handlers.anthropic as _hd_cg_anth
            import headroom.proxy.handlers.streaming as _hd_cg_stream

            _hd_cg_log = _hd_cg_logging.getLogger("headroom.proxy")
            # Beta header of the in-flight request; set by the entry wrapper
            # so the get_context_limit patch can see it.
            _hd_cg_beta = _hd_cg_cvars.ContextVar("headroom_cg_beta", default=None)
            # (model, has_1m_beta) -> real window learned from a 400 body.
            _hd_cg_learned = {}
            _HD_CG_RE = _hd_cg_re.compile(
                r"prompt is too long:\s*(\d+)\s*tokens?\s*>\s*(\d+)\s*maximum",
                _hd_cg_re.IGNORECASE,
            )
            # Nudge arms at 90% of the real window; reports 95% of the window
            # the client believes in (Claude Code compacts around 92%).
            _HD_CG_TRIGGER = 0.90
            _HD_CG_REPORT = 0.95

            def _hd_cg_has_1m(beta):
                if not beta:
                    return False
                return any(
                    t.strip().lower().startswith("context-1m") for t in beta.split(",")
                )

            def _hd_cg_believed(base, beta):
                return max(base, 1_000_000) if _hd_cg_has_1m(beta) else base

            def _hd_cg_effective(model, base, beta):
                believed = _hd_cg_believed(base, beta)
                learned = _hd_cg_learned.get((model, _hd_cg_has_1m(beta)))
                return min(believed, learned) if learned is not None else believed

            def _hd_cg_learn(model, beta, text):
                if isinstance(text, (bytes, bytearray)):
                    text = bytes(text).decode("utf-8", errors="replace")
                m = _HD_CG_RE.search(text or "")
                if m:
                    _hd_cg_learned[(model, _hd_cg_has_1m(beta))] = int(m.group(2))
                    _hd_cg_log.warning(
                        "event=context_guard_learned_limit model=%s context_1m=%s limit=%s",
                        model,
                        _hd_cg_has_1m(beta),
                        m.group(2),
                    )

            _hd_cg_orig_handle = _hd_cg_anth.AnthropicHandlerMixin.handle_anthropic_messages

            async def _hd_cg_handle(self, request, *args, **kwargs):
                token = None
                try:
                    token = _hd_cg_beta.set(request.headers.get("anthropic-beta"))
                except Exception:
                    token = None
                try:
                    return await _hd_cg_orig_handle(self, request, *args, **kwargs)
                finally:
                    if token is not None:
                        _hd_cg_beta.reset(token)

            _hd_cg_anth.AnthropicHandlerMixin.handle_anthropic_messages = _hd_cg_handle

            # Compression pressure sees the window the request is really
            # subject to (context-1m raised, learned-capped) instead of a
            # flat 200k that pins context_pressure at 1.0 for 1M sessions.
            _hd_cg_orig_limit = _hd_cg_prov.AnthropicProvider.get_context_limit

            def _hd_cg_limit(self, model):
                base = _hd_cg_orig_limit(self, model)
                try:
                    return _hd_cg_effective(model, base, _hd_cg_beta.get())
                except Exception:
                    return base

            _hd_cg_prov.AnthropicProvider.get_context_limit = _hd_cg_limit

            class _HdCgGuard:
                # Mirrors upstream StreamUsageGuard (PR #2942): rewrite the
                # first message_start's input_tokens when the forwarded total
                # nears the real window, AND the final cumulative-usage
                # message_delta -- verified live 2026-08-12 that Claude Code
                # merges the latter over the former, so nudging only the
                # first event loses the merge. Below the trigger the guard
                # goes inert on the first event (steady-state untouched).
                def __init__(self, believed, effective):
                    self.believed = believed
                    self.effective = effective
                    self.buf = bytearray()
                    self.seen_start = False
                    self.target = None
                    self.start_cr = 0
                    self.start_cw = 0
                    self.done = believed <= 0 or effective <= 0

                def flush(self):
                    self.done = True
                    out = bytes(self.buf)
                    self.buf = bytearray()
                    return out

                def feed(self, chunk):
                    if self.done:
                        return chunk
                    self.buf.extend(chunk)
                    if len(self.buf) > 262144:
                        return self.flush()
                    out = bytearray()
                    while not self.done:
                        cut = self.buf.find(b"\n\n")
                        if cut == -1:
                            break
                        event = bytes(self.buf[: cut + 2])
                        del self.buf[: cut + 2]
                        out += self._event(event)
                    if self.done:
                        out += self.flush()
                    return bytes(out)

                def _event(self, event):
                    if b"event: ping" in event or event.strip() == b"":
                        return event
                    if not self.seen_start:
                        self.seen_start = True
                        if b"message_start" not in event:
                            self.done = True
                            return event
                        try:
                            rewritten = self._rewrite_start(event)
                        except Exception:
                            self.done = True
                            return event
                        if self.target is None:
                            self.done = True
                        return rewritten
                    if b"message_delta" not in event:
                        return event
                    self.done = True
                    try:
                        return self._rewrite_delta(event)
                    except Exception:
                        return event

                def _rewrite_start(self, event):
                    lines = event.split(b"\n")
                    for i, line in enumerate(lines):
                        if not line.startswith(b"data:"):
                            continue
                        payload = _hd_cg_json.loads(line[5:].strip())
                        if payload.get("type") != "message_start":
                            return event
                        usage = payload.get("message", {}).get("usage")
                        if not isinstance(usage, dict):
                            return event
                        cr = int(usage.get("cache_read_input_tokens") or 0)
                        cw = int(usage.get("cache_creation_input_tokens") or 0)
                        total = int(usage.get("input_tokens") or 0) + cr + cw
                        if total < _HD_CG_TRIGGER * self.effective:
                            return event
                        target = int(self.believed * _HD_CG_REPORT)
                        if total >= target:
                            return event
                        self.target = target
                        self.start_cr = cr
                        self.start_cw = cw
                        usage["input_tokens"] = target - cr - cw
                        _hd_cg_log.warning(
                            "event=context_guard_nudge forwarded_total=%s "
                            "effective_limit=%s reported_total=%s believed_limit=%s",
                            total,
                            self.effective,
                            target,
                            self.believed,
                        )
                        lines[i] = b"data: " + _hd_cg_json.dumps(
                            payload, separators=(",", ":")
                        ).encode()
                        return b"\n".join(lines)
                    return event

                def _rewrite_delta(self, event):
                    lines = event.split(b"\n")
                    for i, line in enumerate(lines):
                        if not line.startswith(b"data:"):
                            continue
                        payload = _hd_cg_json.loads(line[5:].strip())
                        if payload.get("type") != "message_delta":
                            return event
                        usage = payload.get("usage")
                        # A delta without cumulative input usage cannot
                        # override the already-nudged message_start.
                        if not isinstance(usage, dict) or "input_tokens" not in usage:
                            return event
                        cr = int(usage.get("cache_read_input_tokens", self.start_cr) or 0)
                        cw = int(
                            usage.get("cache_creation_input_tokens", self.start_cw) or 0
                        )
                        new_input = self.target - cr - cw
                        if int(usage.get("input_tokens") or 0) >= new_input:
                            return event
                        usage["input_tokens"] = new_input
                        lines[i] = b"data: " + _hd_cg_json.dumps(
                            payload, separators=(",", ":")
                        ).encode()
                        return b"\n".join(lines)
                    return event

            _hd_cg_orig_stream = _hd_cg_stream.StreamingMixin._stream_response

            async def _hd_cg_stream_response(
                self, url, headers, body, provider, model, request_id, *args, **kwargs
            ):
                resp = await _hd_cg_orig_stream(
                    self, url, headers, body, provider, model, request_id, *args, **kwargs
                )
                if provider != "anthropic":
                    return resp
                try:
                    beta = (
                        headers.get("anthropic-beta") if isinstance(headers, dict) else None
                    )
                    if getattr(resp, "status_code", 200) == 400:
                        _hd_cg_learn(model, beta, getattr(resp, "body", b"") or b"")
                        return resp
                    inner = getattr(resp, "body_iterator", None)
                    if inner is None:
                        return resp
                    base = _hd_cg_orig_limit(self.anthropic_provider, model)
                    guard = _HdCgGuard(
                        _hd_cg_believed(base, beta),
                        _hd_cg_effective(model, base, beta),
                    )

                    async def _hd_cg_guarded():
                        async for chunk in inner:
                            if guard.done:
                                yield chunk
                                continue
                            if not isinstance(chunk, (bytes, bytearray)):
                                tail = guard.flush()
                                if tail:
                                    yield tail
                                yield chunk
                                continue
                            out = guard.feed(bytes(chunk))
                            if out:
                                yield out
                        tail = guard.flush()
                        if tail:
                            yield tail

                    resp.body_iterator = _hd_cg_guarded()
                except Exception:
                    pass
                return resp

            _hd_cg_stream.StreamingMixin._stream_response = _hd_cg_stream_response
    except Exception:
        pass

    # Response-cache poisoning guard (owed upstream). SemanticCache.set is
    # gated only on `status_code == 200`, so an empty body, an unparseable
    # one, or an error payload that some path returned as 200 is stored and
    # replayed verbatim for the full TTL (default 1h) to every matching
    # non-streaming request. Observed 2026-08-13: three /v1/messages replays
    # served in 6-10ms from one poisoned entry, and the only recovery was
    # restarting the proxy. Provider-generic (Anthropic and OpenAI both use a
    # top-level "error"), so it protects every handler, not just the CCR path
    # that surfaced it. Kill switch: HEADROOM_RESPONSE_CACHE_GUARD=0.
    try:
        if _hd_os.environ.get("HEADROOM_RESPONSE_CACHE_GUARD", "1").strip().lower() not in (
            "0",
            "false",
            "no",
            "off",
        ):
            import json as _hd_sc_json
            import logging as _hd_sc_logging

            import headroom.proxy.semantic_cache as _hd_sc

            _hd_sc_log = _hd_sc_logging.getLogger("headroom.proxy")
            _hd_sc_orig_set = _hd_sc.SemanticCache.set

            def _hd_sc_cacheable(body):
                if not body:
                    return False
                try:
                    parsed = _hd_sc_json.loads(body)
                except Exception:
                    return False
                if not isinstance(parsed, dict):
                    return False
                # An error body stored under a 200 is the worst case: every
                # replay re-serves it and the client never retries.
                return not (parsed.get("type") == "error" or "error" in parsed)

            async def _hd_sc_set(self, messages, model, response_body, *args, **kwargs):
                if not _hd_sc_cacheable(response_body):
                    _hd_sc_log.warning(
                        "event=response_cache_store_refused model=%s bytes=%d",
                        model,
                        len(response_body or b""),
                    )
                    return None
                return await _hd_sc_orig_set(
                    self, messages, model, response_body, *args, **kwargs
                )

            _hd_sc.SemanticCache.set = _hd_sc_set
    except Exception:
        pass

    # Responses savings denominator guard (upstream PR #3106; remove once a
    # wheel ships it -- see the module docstring for the failure mode). WS
    # outcomes already build original as optimized + saved and are excluded
    # twice over: the stash skips executor calls carrying the WS-only timeout
    # kwarg (both WS call sites pass it on 0.35.0, the HTTP site does not),
    # and the repair refuses endpoint=responses_ws. Version-gated below 0.36
    # so a wheel that ships #3106 natively cannot be widened twice. Kill
    # switch: HEADROOM_RESPONSES_DENOMINATOR_GUARD=0.
    try:
        if _hd_os.environ.get(
            "HEADROOM_RESPONSES_DENOMINATOR_GUARD", "1"
        ).strip().lower() not in ("0", "false", "no", "off"):
            import asyncio as _hd_rd_asyncio
            import collections as _hd_rd_collections
            import dataclasses as _hd_rd_dc
            import importlib.metadata as _hd_rd_meta

            import headroom.proxy.handlers.openai as _hd_rd_openai
            import headroom.proxy.server as _hd_rd_server

            _hd_rd_ver = tuple(
                int(p) for p in _hd_rd_meta.version("headroom-ai").split(".")[:2]
            )
            if _hd_rd_ver < (0, 36):
                # request_id -> pre-compression tools-schema token count.
                # Entries pop at the outcome funnel; stragglers (upstream
                # errors, killed requests) age out FIFO.
                _hd_rd_tools = _hd_rd_collections.OrderedDict()
                _HD_RD_MAX = 512

                _hd_rd_orig_compress = (
                    _hd_rd_openai.OpenAIHandlerMixin._compress_openai_responses_payload_in_executor
                )

                async def _hd_rd_compress(self, payload, **kwargs):
                    result = await _hd_rd_orig_compress(self, payload, **kwargs)
                    try:
                        # The pass is copy-on-write, so `payload` still holds
                        # the pre-compression tools. result[1] = modified.
                        tools = (
                            payload.get("tools") if isinstance(payload, dict) else None
                        )
                        if tools and "timeout" not in kwargs and result[1]:
                            counter = self.openai_provider.get_token_counter(
                                kwargs.get("model")
                            )
                            count = await _hd_rd_asyncio.to_thread(
                                counter.count_text,
                                _hd_rd_openai._json_debug_dumps(tools),
                            )
                            rid = kwargs.get("request_id")
                            if rid and count > 0:
                                _hd_rd_tools[rid] = int(count)
                                while len(_hd_rd_tools) > _HD_RD_MAX:
                                    _hd_rd_tools.popitem(last=False)
                    except Exception:
                        pass
                    return result

                _hd_rd_openai.OpenAIHandlerMixin._compress_openai_responses_payload_in_executor = (
                    _hd_rd_compress
                )

                _hd_rd_orig_record = (
                    _hd_rd_server.HeadroomProxy._record_request_outcome
                )

                async def _hd_rd_record(self, outcome):
                    try:
                        tools_tokens = _hd_rd_tools.pop(outcome.request_id, None)
                        # Repair only the old HTTP derivation shape; anything
                        # else (WS deltas, future wheels) passes untouched.
                        if (
                            tools_tokens
                            and outcome.tokens_saved > 0
                            and (outcome.tags or {}).get("endpoint") != "responses_ws"
                            and outcome.optimized_tokens
                            == max(0, outcome.original_tokens - outcome.tokens_saved)
                        ):
                            widened = outcome.original_tokens + tools_tokens
                            outcome = _hd_rd_dc.replace(
                                outcome,
                                original_tokens=widened,
                                optimized_tokens=max(0, widened - outcome.tokens_saved),
                            )
                    except Exception:
                        pass
                    return await _hd_rd_orig_record(self, outcome)

                _hd_rd_server.HeadroomProxy._record_request_outcome = _hd_rd_record
    except Exception:
        pass

    # Tool-schema dollar unfold (upstream PR #3170; remove once a wheel ships
    # it -- see the module docstring for the contamination this prevents). The
    # wrapper strikes at the single choke point every caller funnels through:
    # SavingsTracker.record_request is keyword-only, and the fold is
    # priced["compression"] + priced["tool_schema"] inside it. Zeroing the
    # tool_schema bucket on a COPY (the caller's mapping is never mutated)
    # restores message-only dollars everywhere the tracker persists them,
    # while tool_search_saved -- the token side -- passes through untouched.
    # Gated on the runtime NOT having #3170's disjoint fields, so the first
    # wheel that ships them keeps its proper split and this block goes inert.
    # Kill switch: HEADROOM_SAVINGS_FOLD_GUARD=0.
    try:
        if _hd_os.environ.get(
            "HEADROOM_SAVINGS_FOLD_GUARD", "1"
        ).strip().lower() not in ("0", "false", "no", "off"):
            import headroom.proxy.savings_tracker as _hd_sf_st

            if "tool_schema_savings_usd" not in _hd_sf_st._empty_display_session():
                _hd_sf_orig_record = _hd_sf_st.SavingsTracker.record_request

                def _hd_sf_record(self, **kwargs):
                    priced = kwargs.get("estimated_savings_usd")
                    if priced is not None:
                        try:
                            unfolded = dict(priced)
                            unfolded["tool_schema"] = 0.0
                            kwargs["estimated_savings_usd"] = unfolded
                        except Exception:
                            pass
                    return _hd_sf_orig_record(self, **kwargs)

                _hd_sf_st.SavingsTracker.record_request = _hd_sf_record
    except Exception:
        pass

    # Chained-read protection (upstream PR #2668; remove once a wheel ships it
    # -- see the module docstring for the re-read/resolve-loss failure this
    # prevents). Rebinding the module-level name covers both in-module call
    # sites: Python resolves it via module globals at call time, which also
    # routes the original's own bash -c recursion through the new version.
    # Kill switch: HEADROOM_READ_CHAIN_GUARD=0.
    try:
        if _hd_os.environ.get(
            "HEADROOM_READ_CHAIN_GUARD", "1"
        ).strip().lower() not in ("0", "false", "no", "off"):
            import re as _hd_rc_re

            import headroom.transforms.content_router as _hd_rc_cr

            _hd_rc_orig = _hd_rc_cr._is_read_command
            # ; && and || start a NEW command; a single | deliberately does
            # not: downstream pipeline stages consume the previous stage's
            # output, not a file, so `grep -n x a.py | head -40` stays derived
            # (compressible) while `cat a.py | head -40` reads via stage one.
            _hd_rc_sep = _hd_rc_re.compile(r"\|\||&&|;")
            _hd_rc_write = _hd_rc_re.compile(r"(^|\s)(>>?|tee\b)")
            _hd_rc_heredoc = _hd_rc_re.compile(r"(^|\s)<<")

            def _hd_rc_segment_is_read(seg):
                # A redirect or tee in THIS segment means it writes a file;
                # judged on the whole segment -- pipeline included, so
                # `cat a.py | tee b.py` stays a write -- before reducing to
                # the stage that touches the file. Sibling segments never
                # see each other, so `cat a.py && echo done > marker` keeps
                # its read protected.
                if _hd_rc_write.search(seg):
                    return False
                first = seg.split("|", 1)[0].strip()
                return bool(first) and _hd_rc_orig(first)

            def _hd_rc_is_read(command):
                if not command or not isinstance(command, str):
                    return False
                c = _hd_rc_cr._strip_cd_prefix(command)
                # A heredoc is the one WHOLE-STRING bailout: its body can
                # contain ; or &&, which would split into bogus segments that
                # look like reads. The command as a whole writes a file.
                if _hd_rc_heredoc.search(c):
                    return False
                return any(
                    _hd_rc_segment_is_read(seg)
                    for seg in (s.strip() for s in _hd_rc_sep.split(c))
                    if seg
                )

            _hd_rc_cr._is_read_command = _hd_rc_is_read
    except Exception:
        pass

    # Prefix-floor vendor (upstream PR #3380, still open; remove once a wheel
    # ships it -- see the module docstring for the compression collapse this
    # ends). Vendors the PR verbatim against the 0.37.0 pin: the patched
    # overlay_cached_prefix and finalize_turn are exec'd wholesale into their
    # modules (no reimplementation -- the 0.9.4-rc.4 splice was one, and lost
    # 22 percent of fleet cache coverage). The provider-confirmed floor the
    # PR stashes in handler locals is bridged from the one seam sitecustomize
    # can reach: prepare_turn's keyword-only tracker_frozen, passed by exactly
    # one caller in 0.37.0 (the Anthropic token path) with the same pre-clamp
    # value. Calls with no bridged floor in scope (OpenAI paths, cache mode)
    # get confirmed_frozen_count=len(prev_returned): through the PR's own
    # mechanism that floors every replayable position, which IS the full
    # replay this desktop shipped in 0.9.5 -- no path gets less cache
    # protection than the prefix-replay guard gave. Binding order matters:
    # the vendored signature carries confirmed_frozen_count, which flips the
    # prefix-replay guard below to inert via its own gate. Kill switch:
    # HEADROOM_PR3380_VENDOR=0 (the guard below then binds as before).
    try:
        if _hd_os.environ.get(
            "HEADROOM_PR3380_VENDOR", "1"
        ).strip().lower() not in ("0", "false", "no", "off"):
            from importlib import metadata as _hd_v_meta

            import headroom.cache.prefix_tracker as _hd_v_pt
            import headroom.proxy.session_engine as _hd_v_se

            _hd_v_fixed = any(
                name in _hd_v_pt.overlay_cached_prefix.__code__.co_varnames
                for name in ("enforce_non_inflation", "confirmed_frozen_count")
            )
            # Exact-pin gate: the vendored bodies are v0.37.0 + PR #3380 and
            # nothing else; any other wheel keeps its own replay policy (a
            # fixed wheel also trips _hd_v_fixed and needs no vendor).
            if _hd_v_meta.version("headroom-ai") == "0.37.0" and not _hd_v_fixed:
                _hd_v_overlay_src = '''
def overlay_cached_prefix(
    optimized_messages: list[dict[str, Any]],
    current_original_messages: list[dict[str, Any]],
    previous_original_messages: list[dict[str, Any]] | None,
    previous_forwarded_messages: list[dict[str, Any]] | None,
    *,
    confirmed_frozen_count: int | None = None,
) -> list[dict[str, Any]]:
    """Replay a positional, non-inflating cached prefix when it is safe.

    Provider-agnostic cache-safety guard for the freeze path. When a message is
    "frozen", the compression pipeline may emit the agent's ORIGINAL bytes for
    it — but the provider cached whatever we FORWARDED last turn (the compressed
    form). Forwarding the original then mismatches the cached prefix and busts
    the prompt cache from that point (100% of observed misses were this
    ``prefix_change``). This overlays the exact previously-forwarded prefix onto
    the corresponding leading messages so the forwarded prefix stays byte-for-byte
    what the provider hashed for its cache key.

    Safe only when this turn extends the previous turn in a proven positional
    shape: either whole-message append or pure block append inside one message.
    There must be exactly one previous forwarded message per original. Otherwise
    the previous bytes may not correspond to the same positions, so we return
    ``optimized_messages`` unchanged (accept a possible bust rather than forward
    wrong content).

    The optimized and current-original lists must be positionally aligned, and
    compact UTF-8 JSON for the replayed result must not exceed the optimized
    candidate. These bounds prefer a cache miss to corrupting or inflating a
    client's live history.

    ``confirmed_frozen_count`` bounds UNCONDITIONAL replay. Leading positions
    the provider has already confirmed cached (a message count derived from
    ``cache_read_input_tokens``) are always replayed byte-identical: the
    replay source there is exactly what the provider hashed, so changing
    those bytes can only bust the cache. Beyond the floor the size bound
    still arbitrates each turn: a shrinking replay (the pipeline emitted
    original bytes for a frozen message) is repaired, while an inflating one
    (fresh compression improved on the forwarded form) is declined so the
    improvement reaches the wire. When the provider count collapses (cold
    cache, TTL lapse) the floor collapses with it and every accumulated
    improvement lands at once - the natural re-baselining that keeps
    long-session growth bounded (#3026). Callers with no provider-confirmed
    count pass None and keep the fully size-bounded behavior.
    """
    prev_orig = previous_original_messages
    prev_fwd = previous_forwarded_messages
    if not prev_orig or not prev_fwd:
        return optimized_messages
    if len(optimized_messages) != len(current_original_messages):
        logger.debug(
            "overlay: optimized/current-original length mismatch (optimized=%d, current=%d) "
            "— skipping positional cached-prefix replay",
            len(optimized_messages),
            len(current_original_messages),
        )
        return optimized_messages
    n = len(prev_orig)
    # Positional 1:1 correspondence between prev_orig[i] and prev_fwd[i] holds
    # only when last turn forwarded exactly one message per original (the
    # append-only, no-injection shape update_from_response records). If the
    # counts differ, an injected / dropped / merged message shifted the
    # mapping, so replaying prev_fwd[i] at position i could forward the wrong
    # content — bail (leave this turn's output untouched) rather than risk it.
    if len(prev_fwd) != n:
        logger.debug(
            "overlay: forwarded/original count mismatch (prev_fwd=%d, prev_orig=%d) "
            "— skipping cached-prefix replay (possible bust)",
            len(prev_fwd),
            n,
        )
        return optimized_messages

    relation = classify_history_relation(current_original_messages, prev_orig)
    if relation.kind == RELATION_BLOCK_APPEND and relation.message_index is not None:
        message_index = relation.message_index
        if message_index < len(optimized_messages):
            previous_message = prev_fwd[message_index]
            previous_original_message = prev_orig[message_index]
            current_message = optimized_messages[message_index]
            previous_content = (
                previous_message.get("content") if isinstance(previous_message, dict) else None
            )
            previous_original_content = (
                previous_original_message.get("content")
                if isinstance(previous_original_message, dict)
                else None
            )
            current_content = (
                current_message.get("content") if isinstance(current_message, dict) else None
            )
            split = (
                len(previous_original_content)
                if isinstance(previous_original_content, list)
                else -1
            )
            if (
                isinstance(previous_content, list)
                and isinstance(previous_original_content, list)
                and isinstance(current_content, list)
                and len(previous_content) == split
                and len(current_content) >= split
                and _canonicalize_for_prefix_compare(current_content[:split])
                == _canonicalize_for_prefix_compare(previous_original_content)
            ):
                merged = copy.deepcopy(previous_message)
                merged["content"] = copy.deepcopy(previous_content) + copy.deepcopy(
                    current_content[split:]
                )
                logger.debug(
                    "overlay: replayed %d forwarded blocks and appended %d new blocks "
                    "inside message %d",
                    split,
                    len(current_content) - split,
                    message_index,
                )
                replayed = (
                    list(prev_fwd[:message_index])
                    + [merged]
                    + list(optimized_messages[message_index + 1 :])
                )
                if message_index >= max(confirmed_frozen_count or 0, 0):
                    replayed_bytes = _compact_json_bytes(replayed)
                    optimized_bytes = _compact_json_bytes(optimized_messages)
                    if (
                        replayed_bytes is None
                        or optimized_bytes is None
                        or len(replayed_bytes) > len(optimized_bytes)
                    ):
                        logger.debug("overlay: block replay inflated compact JSON — skipping")
                        return optimized_messages
                return replayed
    # Append-only guard on CONTENT ONLY, message-by-message. Replay the
    # previously-forwarded (cached, compressed) bytes for the longest LEADING
    # run of messages that is byte-for-byte (content-canonical) identical to
    # what we forwarded last turn, and stop at the first divergence.
    #
    # This is the cache-safety centerpiece for token mode (which relies solely
    # on this replay; cache mode is already byte-stable by construction). The
    # prior all-or-nothing guard busted the ENTIRE cached prefix the moment any
    # single leading message failed to canonicalize-equal last turn — most
    # commonly the just-added assistant turn, whose client-resent form can
    # differ trivially from the copy we reconstructed and recorded. Stopping at
    # the first divergence instead keeps the (much larger) cache-hit region
    # up to that point and only re-forwards from the changed message onward.
    #
    # Comparison uses the shared canonicalizer (not just cache_control
    # stripping) so it is robust to ALL per-turn transport / annotation churn —
    # cache_control movement (Anthropic), litellm `caller`,
    # provider_specific_fields, streaming `index`, string<->block content shape,
    # etc. Content stability is what the provider's prefix cache actually keys
    # on. Safe by construction: we only replay prev_fwd[k] where
    # current_original[k] canonicalize-equals prev_orig[k], and prev_fwd[k]
    # positionally corresponds to prev_orig[k] (guaranteed by the count check
    # above), so no wrong bytes are ever forwarded.
    limit = min(n, len(current_original_messages))
    k = 0
    while k < limit and _canonicalize_for_prefix_compare(
        current_original_messages[k]
    ) == _canonicalize_for_prefix_compare(prev_orig[k]):
        k += 1
    if k == 0:
        logger.debug(
            "overlay: prefix diverged at message 0 — no cached-prefix replay "
            "(cold prefix or client rewrote history head)"
        )
        return optimized_messages
    if k < n:
        logger.debug(
            "overlay: cached-prefix replay for %d/%d leading messages "
            "(diverged at %d — re-forwarding tail fresh)",
            k,
            n,
            k,
        )
    # Replay the cached (compressed) prefix byte-identical up to the first
    # divergence; keep this turn's freshly-produced output for the rest.
    replayed = list(prev_fwd[:k]) + list(optimized_messages[k:])
    replayed_bytes = _compact_json_bytes(replayed)
    optimized_bytes = _compact_json_bytes(optimized_messages)
    if (
        replayed_bytes is None
        or optimized_bytes is None
        or len(replayed_bytes) > len(optimized_bytes)
    ):
        # Something in the replay is byte-larger than this turn's fresh form:
        # fresh compression improved on already-forwarded bytes. Landing the
        # improvement is only safe OUTSIDE the provider-confirmed prefix -
        # inside it, the improvement would change bytes the provider has
        # already cached and bust the whole suffix. Split at the confirmed
        # floor: replay the confirmed region unconditionally, forward
        # everything beyond it fresh so the improvement reaches the wire.
        floor = min(k, max(confirmed_frozen_count or 0, 0))
        if floor <= 0:
            logger.debug("overlay: replay inflated compact JSON — skipping cached-prefix replay")
            return optimized_messages
        logger.debug(
            "overlay: replay inflated beyond the confirmed floor — replaying %d/%d "
            "confirmed messages, forwarding the rest fresh",
            floor,
            k,
        )
        return list(prev_fwd[:floor]) + list(optimized_messages[floor:])
    return replayed
'''
                exec(
                    compile(_hd_v_overlay_src, "<hd-pr3380-overlay>", "exec"),
                    _hd_v_pt.__dict__,
                )
                # session_engine binds overlay_cached_prefix by value at
                # module import; repoint it so the vendored finalize_turn
                # (exec'd below into the same namespace) calls the vendored
                # overlay.
                _hd_v_se.overlay_cached_prefix = _hd_v_pt.overlay_cached_prefix

                _hd_v_finalize_src = '''
def finalize_turn(
    result_messages: list[dict[str, Any]],
    original_messages: list[dict[str, Any]],
    prev_original: list[dict[str, Any]] | None,
    prev_returned: list[dict[str, Any]] | None,
    *,
    count_tokens: Callable[[list[dict[str, Any]]], int] | None = None,
    confirmed_frozen_count: int | None = None,
) -> TurnFinal:
    """Replay last turn's exact forwarded/returned prefix over pipeline drift.

    ``overlay_cached_prefix`` self-guards (positional alignment, append-only
    shape, non-inflation), so calling this is always safe: when replay is not
    provably correct it returns the pipeline's own output unchanged.

    ``confirmed_frozen_count`` is forwarded to ``overlay_cached_prefix`` as
    the unconditional-replay floor: positions the provider has confirmed
    cached are always replayed byte-identical, while beyond the floor the
    size bound decides between drift repair (a shrinking replay) and letting
    a fresh improvement through (an inflating one). Callers with no
    provider-confirmed count pass None and keep the fully size-bounded
    behavior.

    ``count_tokens`` is invoked only when the overlay actually replaced
    bytes — the pipeline's own token count is still accurate otherwise. A
    failing hook falls back to "no recount" rather than failing the turn.
    """
    final = overlay_cached_prefix(
        result_messages,
        original_messages,
        prev_original,
        prev_returned,
        confirmed_frozen_count=confirmed_frozen_count,
    )
    replayed = final != result_messages
    tokens: int | None = None
    if replayed and count_tokens is not None:
        try:
            tokens = count_tokens(final)
        except Exception as e:
            # Fail-open: the turn still forwards, but the caller keeps the
            # pipeline's count of messages that are NOT being forwarded —
            # tokens_saved accounting is stale for this turn. Loud, not
            # silent: a tokenizer that cannot count the replayed form is a
            # bug worth surfacing even though it must not fail the request.
            logger.warning(
                "finalize_turn: token recount of replayed prefix failed "
                "(%s: %s); keeping the pipeline's pre-overlay count",
                type(e).__name__,
                e,
            )
            tokens = None
    return TurnFinal(messages=final, replayed=replayed, tokens=tokens)
'''
                exec(
                    compile(_hd_v_finalize_src, "<hd-pr3380-finalize>", "exec"),
                    _hd_v_se.__dict__,
                )

                import contextvars as _hd_v_cv

                _hd_v_floor = _hd_v_cv.ContextVar("hd_pr3380_floor", default=None)

                _hd_v_orig_prepare = _hd_v_se.prepare_turn

                def _hd_v_prepare(*args, **kwargs):
                    # The PR stashes max(int(frozen_message_count or 0), 0)
                    # in the handler BEFORE prepare_turn clamps it against
                    # the local byte-replay cache; tracker_frozen receives
                    # that same pre-clamp value. Stash-before-call so a
                    # failing prepare_turn cannot leave this turn floorless.
                    if "tracker_frozen" in kwargs:
                        try:
                            _hd_v_floor.set(
                                max(int(kwargs["tracker_frozen"] or 0), 0)
                            )
                        except Exception:
                            _hd_v_floor.set(0)
                    return _hd_v_orig_prepare(*args, **kwargs)

                _hd_v_se.prepare_turn = _hd_v_prepare

                _hd_v_vendored_finalize = _hd_v_se.finalize_turn

                def _hd_v_finalize(
                    result_messages,
                    original_messages,
                    prev_original,
                    prev_returned,
                    **kwargs,
                ):
                    if kwargs.get("confirmed_frozen_count") is None:
                        _hd_v_f = _hd_v_floor.get()
                        # One-shot: consumed by the finalize of the same
                        # turn that stashed it, never a later one (keep-alive
                        # connections run sequential requests on one task
                        # context).
                        _hd_v_floor.set(None)
                        if _hd_v_f is None:
                            _hd_v_f = len(prev_returned or [])
                        kwargs["confirmed_frozen_count"] = _hd_v_f
                    return _hd_v_vendored_finalize(
                        result_messages,
                        original_messages,
                        prev_original,
                        prev_returned,
                        **kwargs,
                    )

                _hd_v_finalize.__wrapped__ = _hd_v_vendored_finalize
                _hd_v_se.finalize_turn = _hd_v_finalize
    except Exception:
        pass
    # Prefix-replay guard (upstream issue #3379 / PR #3380; remove once a
    # wheel ships the fix -- see the module docstring for the cache-bust loop
    # this prevents). Restores v0.35.0's policy: that release has no size
    # bound at all, so it replayed the cached prefix whenever alignment
    # allowed. The bound (#3052) declines instead, and declining over
    # already-cached bytes is the bust. Probed by calling the original twice
    # on DECLINED turns only: once bound-on (a shrinking repair is accepted
    # as-is), once with _compact_json_bytes stubbed to equal-length bytes so
    # the inflation compare goes false while every alignment guard still
    # runs. Overlay is synchronous on the proxy's single event loop, so the
    # stub swap cannot interleave. Gated on the runtime
    # NOT having the fix's parameter under either name shipped on #3380
    # (enforce_non_inflation / confirmed_frozen_count), so the first wheel
    # that ships the fix keeps its own replay policy and this block goes
    # inert. Kill switch: HEADROOM_PREFIX_REPLAY_GUARD=0. Normally inert
    # already: the prefix-floor vendor above installs the parameter; this
    # binds only when the vendor is killed or fails.
    try:
        if _hd_os.environ.get(
            "HEADROOM_PREFIX_REPLAY_GUARD", "1"
        ).strip().lower() not in ("0", "false", "no", "off"):
            import headroom.cache.prefix_tracker as _hd_pr_pt

            _hd_pr_fixed = any(
                name in _hd_pr_pt.overlay_cached_prefix.__code__.co_varnames
                for name in ("enforce_non_inflation", "confirmed_frozen_count")
            )
            if hasattr(_hd_pr_pt, "_compact_json_bytes") and not _hd_pr_fixed:
                _hd_pr_orig_overlay = _hd_pr_pt.overlay_cached_prefix

                def _hd_pr_overlay(*args, **kwargs):
                    optimized = args[0] if args else kwargs.get("optimized_messages")
                    # Pass 1, bound enforced -- identical to stock. A repair
                    # that shrinks (freeze drift, #1850) is accepted as-is.
                    r1 = _hd_pr_orig_overlay(*args, **kwargs)
                    if r1 is not optimized:
                        return r1
                    # Pass 2, bound neutered; every alignment guard still
                    # runs, so a replay that is not provably correct still
                    # returns optimized untouched. Reached only on declined
                    # turns (post-kompress, a few per minute at most).
                    _hd_pr_saved = _hd_pr_pt._compact_json_bytes
                    _hd_pr_pt._compact_json_bytes = lambda value: b""
                    try:
                        return _hd_pr_orig_overlay(*args, **kwargs)
                    finally:
                        _hd_pr_pt._compact_json_bytes = _hd_pr_saved

                _hd_pr_pt.overlay_cached_prefix = _hd_pr_overlay
    except Exception:
        pass

    # cc-switch Official-branch upstream reset (upstream PR #3166, still open;
    # remove once a wheel ships it -- see the module docstring for the misroute
    # this prevents). Load-bearing: the desktop opts into the reconciler ONLY
    # because this binds, so every failure path below clears
    # HEADROOM_CC_SWITCH_RECONCILE, which the proxy reads later
    # (reconciler_enabled(), at app creation) -- fail closed, not unfixed.
    try:
        if _hd_os.environ.get(
            "HEADROOM_CC_SWITCH_RESET_GUARD", "1"
        ).strip().lower() in ("0", "false", "no", "off"):
            raise RuntimeError("cc-switch reset guard disabled by env")

        import inspect as _hd_ccs_inspect
        import json as _hd_ccs_json
        import logging as _hd_ccs_logging

        import headroom.proxy.cc_switch_reconciler as _hd_ccs_mod

        _hd_ccs_log = _hd_ccs_logging.getLogger("headroom.proxy")
        _hd_ccs_orig_tick = _hd_ccs_mod.CCSwitchReconciler.tick
        # The wrapper reads four instance attributes the constructor names.
        # Checking them here means a runtime that renamed any of them fails
        # closed at bind time, instead of binding a wrapper whose every tick
        # raises into the swallow below and silently leaves the upstream stale.
        _hd_ccs_params = _hd_ccs_inspect.signature(
            _hd_ccs_mod.CCSwitchReconciler.__init__
        ).parameters
        for _hd_ccs_needed in ("proxy_url", "default_upstream", "set_upstream", "path"):
            if _hd_ccs_needed not in _hd_ccs_params:
                raise RuntimeError(
                    "cc-switch reconciler no longer takes %r" % (_hd_ccs_needed,)
                )
        _hd_ccs_warned = False

        # Advertise the intercept port, not the port this process bound.
        # server.py builds the reconciler with
        # proxy_url=f"http://127.0.0.1:{config.port}", which is the INTERNAL
        # port between the desktop's intercept and this proxy (6768, or
        # 6769-6790 when something else already holds 6768). Clients belong on
        # the fixed intercept port: everything the desktop measures -- activity
        # feed, request counts, savings accounting, stale-tool-ref sanitisation
        # -- lives in the intercept, so a settings.json rewritten to the
        # internal port silently drops that client out of all of it, and breaks
        # outright on the next launch that has to take a fallback port. The
        # desktop passes the URL it wants advertised; no upstream knob exists
        # for this yet.
        _hd_ccs_url = _hd_os.environ.get("HEADROOM_CC_SWITCH_PROXY_URL", "").strip()
        if not _hd_ccs_url.startswith(("http://", "https://")):
            # Missing or malformed while the reconciler is enabled is an
            # inconsistent spawn, and guessing would write a bad base_url into
            # the user's settings.json. Fail closed with everything else.
            raise RuntimeError(
                "HEADROOM_CC_SWITCH_PROXY_URL missing or malformed: %r" % (_hd_ccs_url,)
            )
        _hd_ccs_url = _hd_ccs_url.rstrip("/")

        # Explicit upstream override (Override mode in the app). The user named
        # this endpoint, so a cc-switch capture must not move it; the
        # reconciler still rewrites settings.json, which is what keeps the
        # client on the intercept. Empty unless the desktop set both, so this
        # is inert for everyone else.
        _hd_ccs_pinned = ""
        if _hd_os.environ.get("HEADROOM_CC_SWITCH_PIN_UPSTREAM", "").strip().lower() in (
            "1",
            "true",
            "yes",
            "on",
        ):
            _hd_ccs_pinned = _hd_os.environ.get("ANTHROPIC_TARGET_API_URL", "").strip()
            if not _hd_ccs_pinned.startswith(("http://", "https://")):
                raise RuntimeError(
                    "pinned upstream missing or malformed: %r" % (_hd_ccs_pinned,)
                )

        _hd_ccs_orig_init = _hd_ccs_mod.CCSwitchReconciler.__init__

        def _hd_ccs_init(self, *args, **kwargs):
            _hd_ccs_orig_init(self, *args, **kwargs)
            # Already rstripped, which is what the loop guard compares against.
            self.proxy_url = _hd_ccs_url

        _hd_ccs_mod.CCSwitchReconciler.__init__ = _hd_ccs_init

        def _hd_ccs_selects_official(reconciler):
            # True when settings.json names no base URL at all -- what cc-switch
            # writes for "Claude Official" ({"env": {}}). Raises on a partial
            # read (caught mid atomic-replace); the caller must not consume the
            # mtime in that case.
            data = _hd_ccs_json.loads(reconciler.path.read_text(encoding="utf-8"))
            if not isinstance(data, dict):
                return False
            env = data.get("env")
            url = env.get("ANTHROPIC_BASE_URL") if isinstance(env, dict) else None
            return not (isinstance(url, str) and url.strip())

        def _hd_ccs_tick(self):
            global _hd_ccs_warned
            rewrote = _hd_ccs_orig_tick(self)
            try:
                # With an override configured, the user's endpoint IS the
                # default this reconciler returns to -- both when cc-switch
                # captures something else and when it goes back to Official.
                target = _hd_ccs_pinned or self.default_upstream
                if _hd_ccs_pinned:
                    if self.current_upstream != target:
                        self.current_upstream = target
                        self._set_upstream(target)
                        _hd_ccs_log.info(
                            "event=cc_switch_upstream_pinned upstream=%s", target
                        )
                    return rewrote
                # Only a captured third-party upstream can go stale, and only a
                # changed settings.json can end it. Both checks keep this off
                # the hot path of a 0.3s poll -- an Anthropic-only user never
                # gets past the first one. That first check is also what makes
                # the guard self-neutralizing: a wheel carrying #3166 has
                # already reset current_upstream by the time we look.
                if self.current_upstream in (None, target):
                    return rewrote
                mtime_ns = self.path.stat().st_mtime_ns
                if mtime_ns == getattr(self, "_hd_ccs_mtime_ns", None):
                    return rewrote
                official = _hd_ccs_selects_official(self)
                # Read succeeded: only now is this mtime processed.
                self._hd_ccs_mtime_ns = mtime_ns
                if official:
                    self.current_upstream = target
                    self._set_upstream(target)
                    _hd_ccs_log.info(
                        "event=cc_switch_official_upstream_reset upstream=%s",
                        target,
                    )
            except Exception as exc:  # noqa: BLE001 - the watcher must not die
                # Logged once: the reconciler is running and the reset that
                # makes it safe just did not happen, so a captured third-party
                # endpoint may still be live. Never silent.
                if not _hd_ccs_warned:
                    _hd_ccs_warned = True
                    _hd_ccs_log.warning(
                        "event=cc_switch_official_reset_failed err=%s "
                        "(set HEADROOM_CC_SWITCH_RECONCILE=0 to leave cc-switch alone)",
                        exc,
                    )
            return rewrote

        _hd_ccs_mod.CCSwitchReconciler.tick = _hd_ccs_tick
    except Exception:
        # Fail closed: without the reset, a switch back to Claude Official
        # leaves the captured third-party endpoint live process-wide and
        # Anthropic OAuth traffic follows it. Off is always the safe answer for
        # an opt-in flag.
        _hd_os.environ["HEADROOM_CC_SWITCH_RECONCILE"] = "0"


# ─── Read-maturation first-appearance accounting (desktop-only vendor) ───
# The pipeline books tokens_saved per request as the raw-vs-forwarded token
# diff, so a matured Read's removal is re-booked on EVERY later turn when its
# recorded marker is replayed (read_maturation._handle_read replay branch;
# the client re-sends the raw conversation each turn). Measured 2026-09-02 on
# one machine, same day: new_input_savings_percent 31.8 percent before
# maturation, 78.15 percent after - the gap is recounting, not new removal.
#
# This section subtracts the replayed-marker share at the ONE metrics seam,
# PrometheusMetrics.record_request: it owns tokens_saved_total (which feeds
# /stats tokens.saved and new_input_savings_percent) and forwards to
# SavingsTracker (checkpoints, per-model, web rollups), so every book gets
# first-appearance counting coherently. Newly-matured and fresh-tail savings
# are untouched; matured content books exactly once, on the turn it matures.
#
# Bridge: a lock-guarded module-global pending counter, drained (clamped at
# the request's own tokens_saved) by the next record_request. Deliberately
# NOT task/context-scoped: the transform may run on a different task or
# thread than the recording, and a context-scoped bridge silently no-ops
# there. Cost of the global: with two sessions in flight, one request's
# replay share can drain against the other's record, smearing per-model and
# per-request attribution slightly - the cumulative books stay exact, which
# is what the bars, checkpoints and rollups sum. A request that dies between
# transform and record leaves its share pending; the next request drains it.
#
# Traffic neutrality: the wrapped transform method delegates unchanged and
# only OBSERVES (the transform itself skips the frozen prefix, so observed
# replay events align 1:1 with router-visible diffs); the subtraction happens
# after the response on numbers no request-path decision reads. The token
# delta uses the wheel's EstimatingTokenCounter (~90 percent of the booked
# scale; drift moves only the subtracted share and the per-request clamp
# keeps books non-negative). The upstream counterpart is PR #3414
# (fix(read-maturation): book matured Read savings once) - token-exact at
# the handler seam, no bridge needed. Drop this section when a wheel ships
# it; until then re-pin on every bump like the #3380 vendor above.
# Exact-pin gated to wheel 0.37.0. Kill switch: HEADROOM_MATURATION_FIRST_APPEARANCE=0.
_hd_fa_flag = _hd_os.environ.get("HEADROOM_MATURATION_FIRST_APPEARANCE", "1")
if _hd_fa_flag.strip().lower() not in ("", "0", "false", "no", "off"):
    try:
        import importlib.metadata as _hd_fa_meta
        import threading as _hd_fa_threading

        if _hd_fa_meta.version("headroom-ai") == "0.37.0":
            from headroom.proxy import prometheus_metrics as _hd_fa_pm
            from headroom.transforms import read_maturation as _hd_fa_rm

            _hd_fa_lock = _hd_fa_threading.Lock()
            # Replayed-but-not-yet-drained token total, in a one-slot list so
            # the probe (and this module's wrappers) share one mutable cell.
            _hd_fa_pending = [0]
            # Per-tool-call replay deltas, computed once (content and marker
            # are stable per tc_id). Cleared wholesale at 4096 entries so a
            # long-lived proxy cannot grow it without bound.
            _hd_fa_deltas = {}

            try:
                from headroom.tokenizers.estimator import (
                    EstimatingTokenCounter as _hd_fa_ETC,
                )

                _hd_fa_counter = _hd_fa_ETC()

                def _hd_fa_count(text):
                    return int(_hd_fa_counter.count_text(text))

            except Exception:

                def _hd_fa_count(text):
                    return max(1, len(text) // 4)

            _hd_fa_orig_handle = _hd_fa_rm.ReadMaturationManager._handle_read
            _hd_fa_orig_record = _hd_fa_pm.PrometheusMetrics.record_request

            def _hd_fa_handle(self, tc_id, content, activity, result):
                matured_before = self._matured.get(tc_id)
                out = _hd_fa_orig_handle(self, tc_id, content, activity, result)
                try:
                    # Replay branch only: matured on an EARLIER request and
                    # replaced again now. Newly-matured stays fully booked.
                    if matured_before is not None and out[0] is not None:
                        delta = _hd_fa_deltas.get(tc_id)
                        if delta is None:
                            if len(_hd_fa_deltas) >= 4096:
                                _hd_fa_deltas.clear()
                            delta = max(
                                0,
                                _hd_fa_count(content)
                                - _hd_fa_count(matured_before.marker),
                            )
                            _hd_fa_deltas[tc_id] = delta
                        if delta > 0:
                            with _hd_fa_lock:
                                _hd_fa_pending[0] += delta
                except Exception:
                    # Accounting-only: never let bookkeeping break a request.
                    pass
                return out

            async def _hd_fa_record(self, *args, **kwargs):
                try:
                    if "tokens_saved" in kwargs:
                        with _hd_fa_lock:
                            take = min(
                                _hd_fa_pending[0], max(0, int(kwargs["tokens_saved"]))
                            )
                            if take > 0:
                                _hd_fa_pending[0] -= take
                        if take > 0:
                            kwargs = dict(kwargs)
                            kwargs["tokens_saved"] = int(kwargs["tokens_saved"]) - take
                except Exception:
                    pass
                # Late-bound module-global lookup on purpose: the functional
                # probe swaps _hd_fa_orig_record to verify subtract-once.
                return await _hd_fa_orig_record(self, *args, **kwargs)

            _hd_fa_rm.ReadMaturationManager._handle_read = _hd_fa_handle
            _hd_fa_pm.PrometheusMetrics.record_request = _hd_fa_record
    except Exception:
        # Accounting-only vendor: any binding failure leaves the books
        # exactly as upstream writes them. The request path is never touched
        # from this section, so there is nothing to fail closed FOR.
        pass


# --- Tool-search history repair: both block shapes, keyed on absence (vendor) --
# With ENABLE_TOOL_SEARCH=true the Claude Code client runs its OWN tool search and
# persists discovered-tool references into the transcript. Upstream validates every
# tool_reference in history against THIS request's tools array and 400s ("Tool
# reference 'X' not found in available tools") when the referenced tool is ABSENT
# from the array -- e.g. an MCP server that did not start, or a side-request (Stop
# hook / compact) carrying a smaller tools array. Per Anthropic's docs a referenced
# tool is normally defer_loading=true, so DEFERRED IS VALID; only ABSENCE is the
# fault. Claude Code writes references in two shapes:
#   * server-side: a `tool_search_tool_result` block (nested tool_references)
#   * client-side: a plain `tool_result` whose content is a list of tool_reference
#     blocks (this is how MCP tools like mcp__headroom__* come through)
# The wheel's strip_unsupported_tool_search_blocks only scans the FIRST shape, so a
# stale client-side reference sails through and 400s even though the repair "fired"
# (confirmed 2026-09-06 on Windows: dropped 4 server-side blocks, still 400 on
# mcp__headroom__headroom_compress carried in a client-side block).
#
# This vendor (a) adds a client-side pass that neutralizes tool_reference entries
# whose tool is absent -- keeping present (incl. deferred) ones, and replacing an
# emptied search result with a text note so its tool_use pairing stays intact --
# and (b) delegates the server-side shape to the wheel's own repair with tools
# UNFILTERED (this reverts 0.9.8-rc.5, which wrongly hid defer_loading tools from
# the availability check: it dropped healthy blocks AND missed the real cause).
# Only body["messages"] is rewritten; body["tools"] is untouched. Self-heals a
# session poisoned before this shipped, on its next request. The handler
# late-imports this symbol per request, so a module-level reassign is picked up.
# Upstream fix owed; drop this section when a wheel ships it.
# Exact-pin gated to wheel 0.37.0. Kill switch: HEADROOM_TOOL_SEARCH_REPAIR=0.
_hd_tsr_flag = _hd_os.environ.get("HEADROOM_TOOL_SEARCH_REPAIR", "1")
if _hd_tsr_flag.strip().lower() not in ("", "0", "false", "no", "off"):
    try:
        import importlib.metadata as _hd_tsr_meta

        if _hd_tsr_meta.version("headroom-ai") == "0.37.0":
            from headroom.proxy import helpers as _hd_tsr_helpers

            _hd_tsr_orig = _hd_tsr_helpers.strip_unsupported_tool_search_blocks

            def _hd_tsr_client_side(messages, tools):
                # Neutralize client-side tool_result+tool_reference blocks whose
                # referenced tool is absent from `tools`. Present tools stay,
                # deferred or not. Returns the ORIGINAL messages object when
                # nothing changed so the caller's identity check still skips the
                # write-back.
                if not isinstance(messages, list):
                    return messages, 0
                if isinstance(tools, list):
                    available = {
                        str(t["name"])
                        for t in tools
                        if isinstance(t, dict) and t.get("name")
                    }
                else:
                    available = set()
                removed = 0
                changed = False
                out = []
                for msg in messages:
                    content = msg.get("content") if isinstance(msg, dict) else None
                    if not isinstance(content, list):
                        out.append(msg)
                        continue
                    new_content = []
                    msg_changed = False
                    for block in content:
                        if (
                            isinstance(block, dict)
                            and block.get("type") == "tool_result"
                            and isinstance(block.get("content"), list)
                            and any(
                                isinstance(b, dict)
                                and b.get("type") == "tool_reference"
                                for b in block["content"]
                            )
                        ):
                            kept = []
                            dropped = 0
                            for b in block["content"]:
                                if (
                                    isinstance(b, dict)
                                    and b.get("type") == "tool_reference"
                                ):
                                    name = b.get("tool_name") or b.get("name")
                                    if name is not None and str(name) not in available:
                                        dropped += 1
                                        continue
                                kept.append(b)
                            if dropped:
                                removed += dropped
                                msg_changed = True
                                nb = dict(block)
                                nb["content"] = kept or (
                                    "[headroom: referenced tool(s) no longer available]"
                                )
                                new_content.append(nb)
                                continue
                        new_content.append(block)
                    if msg_changed:
                        changed = True
                        nm = dict(msg)
                        nm["content"] = new_content
                        out.append(nm)
                    else:
                        out.append(msg)
                return (out, removed) if changed else (messages, 0)

            def _hd_tsr_wrapped(messages, tools):
                messages, removed_client = _hd_tsr_client_side(messages, tools)
                repaired, removed_server = _hd_tsr_orig(messages, tools)
                return repaired, removed_client + removed_server

            _hd_tsr_helpers.strip_unsupported_tool_search_blocks = _hd_tsr_wrapped
    except Exception:
        # Request-path wrapper: on any binding failure fall back to the wheel's
        # own repair unchanged. Worst case is the pre-vendor behavior (server-side
        # shape only), never a new failure mode.
        pass


# --- Tool-reference 400: append a "start a new session" hint (vendor) ----------
# When an MCP server disconnects mid-session (Claude Code does not auto-reconnect
# stdio MCP servers), its tools vanish while the transcript still references them,
# and upstream 400s "Tool reference 'X' not found in available tools". The repair
# strips most of these on the wire; for the residual that still reaches the user,
# append a Headroom hint with the actionable remedy (a fresh session re-spawns the
# MCP server). The streaming handler buffers the upstream error via aread() and
# returns a plain Response, so we post-process that Response only: a raw substring
# insert inside the message (valid whether the body is JSON or an SSE error event),
# guarded so it never touches a StreamingResponse, a non-400, or an already-hinted
# body. Fail-open: any error returns the original response untouched.
# Exact-pin gated to wheel 0.37.0. Kill switch: HEADROOM_TOOL_REF_HINT=0.
_hd_hint_flag = _hd_os.environ.get("HEADROOM_TOOL_REF_HINT", "1")
if _hd_hint_flag.strip().lower() not in ("", "0", "false", "no", "off"):
    try:
        import importlib.metadata as _hd_hint_meta

        if _hd_hint_meta.version("headroom-ai") == "0.37.0":
            from headroom.proxy.handlers import streaming as _hd_hint_mod

            _hd_hint_sig = b"not found in available tools"
            _hd_hint_add = (
                b"not found in available tools. Headroom: this error is caused by a "
                b"disconnected MCP server. Close this session and start a new one to fix it."
            )
            _hd_hint_orig = _hd_hint_mod.StreamingMixin._stream_response

            def _hd_hint_apply(result):
                # Only a buffered 400 Response carrying the signature; never a
                # StreamingResponse (no .body), never double-hinted.
                body = getattr(result, "body", None)
                if (
                    getattr(result, "status_code", 200) == 400
                    and isinstance(body, (bytes, bytearray))
                    and _hd_hint_sig in body
                    and b"Headroom:" not in body
                ):
                    from starlette.responses import Response as _hd_hint_resp

                    new_body = bytes(body).replace(_hd_hint_sig, _hd_hint_add, 1)
                    headers = dict(result.headers)
                    headers.pop("content-length", None)
                    return _hd_hint_resp(
                        content=new_body,
                        status_code=result.status_code,
                        headers=headers,
                        media_type=getattr(result, "media_type", None),
                    )
                return result

            async def _hd_hint_wrapped(self, *args, **kwargs):
                result = await _hd_hint_orig(self, *args, **kwargs)
                try:
                    return _hd_hint_apply(result)
                except Exception:
                    return result

            _hd_hint_mod.StreamingMixin._stream_response = _hd_hint_wrapped
    except Exception:
        # Response-shaping only: on any binding failure the client sees the
        # upstream error verbatim (the pre-vendor behavior), never a new failure.
        pass
"#;
/// Default-on passthrough for the rollout registry's `read_maturation` feature.
///
/// Requested for every install since 0.9.7-rc.1 (2026-09-02): the 0.37.0
/// freeze policy caps tail-only compression at ~1-2% on big sessions, and
/// read_maturation is the cache-safe recovery leg (holds Read results out of
/// the provider cache until they quiesce; never mutates a cached byte). It is
/// still cache-breakpoint machinery -- the class that cost 89 installs ~17pp
/// on 0.9.4 -- so the falsey spellings of `HEADROOM_READ_MATURATION` stay a
/// no-rebuild kill switch, and the rc does not promote to stable until
/// `bin/rails savings:did` has judged a full staging day on BOTH tok_saved
/// AND cache_read.
fn read_maturation_env() -> Vec<(String, String)> {
    read_maturation_env_from(std::env::var("HEADROOM_READ_MATURATION").ok().as_deref())
}

fn read_maturation_env_from(raw: Option<&str>) -> Vec<(String, String)> {
    // Absent means on; an explicit value still passes through verbatim.
    let value = raw.map_or("1", str::trim);
    // Mirror the wheel's own truthiness so "0"/"false"/"off" mean off rather
    // than "the variable is present, therefore on".
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    ) {
        return Vec::new();
    }
    vec![("HEADROOM_READ_MATURATION".to_string(), value.to_string())]
}

/// Pre-upstream concurrency passed to the backend: 2x logical cores,
/// clamped to [8, 64]. See the spawn-site comment for why the proxy's own
/// 8-cap is safe to exceed under the desktop's env. An explicit
/// `HEADROOM_ANTHROPIC_PRE_UPSTREAM_CONCURRENCY` in the environment wins, so a
/// power user with 30+ concurrent sessions can raise it without a rebuild.
fn pre_upstream_concurrency() -> usize {
    if let Some(n) = std::env::var("HEADROOM_ANTHROPIC_PRE_UPSTREAM_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores * 2).clamp(8, 64)
}

/// Interval estimate of OpenAI's prompt-cache TTL from the backend's
/// `cache_ttl_observations.jsonl` (written under HEADROOM_CACHE_TTL_LEARN):
/// hit idles bound the TTL from below (cache proven alive), `ttl_expiry` miss
/// idles bound it from above (cache proven dead). Returns the smallest
/// observed death-idle beyond the largest observed life-idle, so an estimation
/// error can only skip a recompaction, never bust a warm cache; None when the
/// bounds overlap or samples are thin (< 3 hits or < 3 expiry misses).
/// Mirrors the upstream `headroom-cache-ttl` estimator (headroom PR #2670);
/// delete once a wheel ships it and a scheduled run replaces this.
fn learned_openai_ttl_seconds(obs_path: &Path) -> Option<u64> {
    let data = std::fs::read_to_string(obs_path).ok()?;
    let mut hit_idles: Vec<f64> = Vec::new();
    let mut expiry_idles: Vec<f64> = Vec::new();
    for line in data.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("provider").and_then(|v| v.as_str()) != Some("openai") {
            continue;
        }
        let idle = row
            .get("idle_seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if idle <= 0.0 {
            continue;
        }
        if row.get("is_miss").and_then(|v| v.as_bool()) == Some(true) {
            if row.get("reason").and_then(|v| v.as_str()) == Some("ttl_expiry") {
                expiry_idles.push(idle);
            }
        } else {
            hit_idles.push(idle);
        }
    }
    if hit_idles.len() < 3 || expiry_idles.len() < 3 {
        return None;
    }
    let max_hit_idle = hit_idles.into_iter().fold(0.0f64, f64::max);
    let learned = expiry_idles
        .into_iter()
        .filter(|&idle| idle > max_hit_idle)
        .fold(f64::INFINITY, f64::min);
    if !learned.is_finite() {
        return None;
    }
    Some((learned as u64).clamp(300, 3600))
}

fn parse_major_minor_patch(s: &str) -> Option<(u32, u32, u32)> {
    let head = s.split(|c: char| c == '-' || c == '+').next()?;
    let mut parts = head.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// True when the previously-installed receipt is too old to safely apply an
/// in-place pip upgrade against — caller should fall through to the atomic
/// venv rebuild path. Unparseable versions are treated as too old (be
/// conservative: a rebuild is always safe, an unsafe in-place is not).
fn receipt_requires_atomic_rebuild(previous_version: &str) -> bool {
    match parse_major_minor_patch(previous_version) {
        Some(v) => v < ATOMIC_REBUILD_FLOOR_VERSION,
        None => true,
    }
}
const RTK_VERSION: &str = "0.45.0";
const MARKITDOWN_PINNED_VERSION: &str = "0.1.7";
const SERENA_PINNED_VERSION: &str = "1.7.0";
const CONTEXT7_PINNED_VERSION: &str = "4.0.2";
/// First run downloads the package into the npx cache; slow networks need
/// headroom over the usual smoke-test budget.
const CONTEXT7_INSTALL_TIMEOUT: Duration = Duration::from_secs(180);
const CODEBASE_MEMORY_VERSION: &str = "0.10.3";
const CODEBASE_MEMORY_SHA256_MACOS_AARCH64: &str =
    "0ebf02328207d4c3d862c837b5e973de5bac808df92b0941737721d467287f7f";
const CODEBASE_MEMORY_SHA256_MACOS_X86_64: &str =
    "1107fea28285823e1436e4f38a4e00a0b472d8a43c379da7dfd200c914a4b9dd";
const CODEBASE_MEMORY_SHA256_LINUX_AARCH64: &str =
    "967b9eababfdbd2ef1987c571d55bc7c028cd1db7f99279830634c58db311e32";
const CODEBASE_MEMORY_SHA256_LINUX_X86_64: &str =
    "74997fb0934e70a22f20c2e112fb4d883867dc1f01a7bcdc94cf86d13b5cbd31";
/// Serena's CLI cold-imports its full LSP stack; first run on a slow disk can
/// take tens of seconds.
const SERENA_SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Registers/unregisters the managed Serena MCP entry with every detected
/// agent, reusing the bundled headroom package's registrars and install
/// ledger instead of reimplementing Claude Code JSON / Codex TOML handling in
/// Rust. Mirrors upstream `_setup_serena_mcp` / `_remove_headroom_installed_serena_mcp`
/// (headroom.cli.wrap): only entries the ledger proves Headroom installed are
/// ever overwritten or removed — a user-managed serena entry is left alone.
/// argv: `register <serena-bin>` | `unregister`.
const SERENA_MCP_HELPER: &str = r#"
import sys

from headroom.mcp_registry import (
    ClaudeRegistrar,
    CodexRegistrar,
    GrokRegistrar,
    OpencodeRegistrar,
    ServerSpec,
)
from headroom.mcp_registry.base import RegisterStatus
from headroom.mcp_registry.ledger import (
    clear_install,
    headroom_installed_matching,
    record_install,
)

action = sys.argv[1]
failures = []
# Claude/Codex only: serena's --context values are named profiles and no
# grok/opencode context has been validated against serena yet.
for registrar, context in ((ClaudeRegistrar(), "claude-code"), (CodexRegistrar(), "codex")):
    if not registrar.detect():
        print(f"{registrar.name}: not detected, skipping")
        continue
    if action == "register":
        spec = ServerSpec(
            name="serena",
            command=sys.argv[2],
            args=(
                "start-mcp-server",
                "--project-from-cwd",
                "--context",
                context,
                "--open-web-dashboard",
                "False",
            ),
        )
        result = registrar.register_server(spec)
        if result.status == RegisterStatus.MISMATCH and headroom_installed_matching(
            registrar.name, registrar.get_server("serena")
        ):
            result = registrar.register_server(spec, force=True)
        if result.status == RegisterStatus.REGISTERED:
            record_install(registrar.name, spec)
        elif result.status == RegisterStatus.FAILED:
            failures.append(f"{registrar.name}: {result.detail}")
        print(f"{registrar.name}: {result.status.value}")
    else:
        current = registrar.get_server("serena")
        if current is None:
            print(f"{registrar.name}: no serena entry")
            continue
        if not headroom_installed_matching(registrar.name, current):
            print(f"{registrar.name}: serena entry is user-managed, leaving it")
            continue
        if registrar.unregister_server("serena"):
            clear_install(registrar.name, "serena")
            print(f"{registrar.name}: removed")
        else:
            failures.append(f"{registrar.name}: removal failed")
if failures:
    sys.exit("; ".join(failures))
"#;
/// Same ledger-guarded register/unregister flow as `SERENA_MCP_HELPER`, for
/// the Context7 MCP entry. The registered command is a bare `npx` (resolved
/// from the agent session's own PATH, so nvm version switches don't strand an
/// absolute path) running the pinned package. argv: `register <package-spec>`
/// | `unregister`.
const CONTEXT7_MCP_HELPER: &str = r#"
import sys

from headroom.mcp_registry import (
    ClaudeRegistrar,
    CodexRegistrar,
    GrokRegistrar,
    OpencodeRegistrar,
    ServerSpec,
)
from headroom.mcp_registry.base import RegisterStatus
from headroom.mcp_registry.ledger import (
    clear_install,
    headroom_installed_matching,
    record_install,
)

action = sys.argv[1]
failures = []
for registrar in (ClaudeRegistrar(), CodexRegistrar(), GrokRegistrar(), OpencodeRegistrar()):
    if not registrar.detect():
        print(f"{registrar.name}: not detected, skipping")
        continue
    if action == "register":
        spec = ServerSpec(
            name="context7",
            command="npx",
            args=("-y", sys.argv[2]),
        )
        result = registrar.register_server(spec)
        if result.status == RegisterStatus.MISMATCH and headroom_installed_matching(
            registrar.name, registrar.get_server("context7")
        ):
            result = registrar.register_server(spec, force=True)
        if result.status == RegisterStatus.REGISTERED:
            record_install(registrar.name, spec)
        elif result.status == RegisterStatus.FAILED:
            failures.append(f"{registrar.name}: {result.detail}")
        print(f"{registrar.name}: {result.status.value}")
    else:
        current = registrar.get_server("context7")
        if current is None:
            print(f"{registrar.name}: no context7 entry")
            continue
        if not headroom_installed_matching(registrar.name, current):
            print(f"{registrar.name}: context7 entry is user-managed, leaving it")
            continue
        if registrar.unregister_server("context7"):
            clear_install(registrar.name, "context7")
            print(f"{registrar.name}: removed")
        else:
            failures.append(f"{registrar.name}: removal failed")
if failures:
    sys.exit("; ".join(failures))
"#;
/// Same ledger-guarded register/unregister flow as `SERENA_MCP_HELPER`, for
/// the codebase-memory MCP entry. `CBM_CACHE_DIR` points its index databases
/// into Headroom's managed tools dir so uninstalling removes them too.
/// argv: `register <binary> <cache-dir>` | `unregister`.
const CODEBASE_MEMORY_MCP_HELPER: &str = r#"
import sys

from headroom.mcp_registry import (
    ClaudeRegistrar,
    CodexRegistrar,
    GrokRegistrar,
    OpencodeRegistrar,
    ServerSpec,
)
from headroom.mcp_registry.base import RegisterStatus
from headroom.mcp_registry.ledger import (
    clear_install,
    headroom_installed_matching,
    record_install,
)

action = sys.argv[1]
failures = []
for registrar in (ClaudeRegistrar(), CodexRegistrar(), GrokRegistrar(), OpencodeRegistrar()):
    if not registrar.detect():
        print(f"{registrar.name}: not detected, skipping")
        continue
    if action == "register":
        spec = ServerSpec(
            name="codebase-memory",
            command=sys.argv[2],
            env={"CBM_CACHE_DIR": sys.argv[3]},
        )
        result = registrar.register_server(spec)
        if result.status == RegisterStatus.MISMATCH and headroom_installed_matching(
            registrar.name, registrar.get_server("codebase-memory")
        ):
            result = registrar.register_server(spec, force=True)
        if result.status == RegisterStatus.REGISTERED:
            record_install(registrar.name, spec)
        elif result.status == RegisterStatus.FAILED:
            failures.append(f"{registrar.name}: {result.detail}")
        print(f"{registrar.name}: {result.status.value}")
    else:
        current = registrar.get_server("codebase-memory")
        if current is None:
            print(f"{registrar.name}: no codebase-memory entry")
            continue
        if not headroom_installed_matching(registrar.name, current):
            print(f"{registrar.name}: codebase-memory entry is user-managed, leaving it")
            continue
        if registrar.unregister_server("codebase-memory"):
            clear_install(registrar.name, "codebase-memory")
            print(f"{registrar.name}: removed")
        else:
            failures.append(f"{registrar.name}: removal failed")
if failures:
    sys.exit("; ".join(failures))
"#;
/// A marketplace plugin addon installed through the host CLIs' own
/// `<cli> plugin ...` managers. The install/enable/uninstall plumbing is
/// shared; addons differ only in these identifiers.
struct PluginAddon {
    id: &'static str,
    /// `owner/repo` passed to `plugin marketplace add`.
    marketplace: &'static str,
    /// Marketplace name passed to `plugin marketplace remove`.
    marketplace_name: &'static str,
    /// `plugin@marketplace` ref used by install/enable/disable/uninstall.
    plugin_ref: &'static str,
}

static PLUGIN_ADDONS: [PluginAddon; 2] = [
    PluginAddon {
        id: "ponytail",
        marketplace: "DietrichGebert/ponytail",
        marketplace_name: "ponytail",
        plugin_ref: "ponytail@ponytail",
    },
    PluginAddon {
        id: "caveman",
        marketplace: "JuliusBrussee/caveman",
        marketplace_name: "caveman",
        plugin_ref: "caveman@caveman",
    },
];
const PLUGIN_DISPLAY_VERSION: &str = "latest";

fn plugin_addon(id: &str) -> Option<&'static PluginAddon> {
    PLUGIN_ADDONS.iter().find(|plugin| plugin.id == id)
}

/// Whether the card offers an Update action, and what it would move to.
///
/// The pinned version is a *minimum*, not a target: a user already ahead of the
/// pin (they updated the plugin themselves, or a pin has not caught up yet) is
/// current, and must never be prompted into a downgrade. An unparseable version
/// on either side is treated as current for the same reason — better a missed
/// prompt than a wrong one.
///
/// `None` means no Update button: nothing installed, already at or past the
/// pin, or an addon that maintains itself (headroom rides the runtime upgrade,
/// rtk is refreshed at launch from its own pin).
fn pending_addon_update(id: &str, installed: Option<&str>, pinned: &str) -> Option<String> {
    let installed = installed?;
    match id {
        // Plugins track a moving marketplace, not a pin, so there is no local
        // signal for "newer exists" — the Update action is the check. It always
        // shows for an installed plugin, and the button says just "Update".
        _ if plugin_addon(id).is_some() => Some(String::new()),
        "markitdown" | "serena" | "context7" | "codebase-memory" => {
            let on_disk = parse_major_minor_patch(installed)?;
            let target = parse_major_minor_patch(pinned)?;
            (on_disk < target).then(|| pinned.to_string())
        }
        _ => None,
    }
}
const RTK_SHA256_MACOS_AARCH64: &str =
    "064151cfc2d50b24d810b06a0af2e41b9c945e83534e4c438c3d3eae607fc3f4";
const RTK_SHA256_MACOS_X86_64: &str =
    "9ea02f889d5a2779e4fb700df4587824303c5a57cda22e903e30058079fca0ef";
const RTK_SHA256_LINUX_AARCH64: &str =
    "80a746dd305ef944ff50ef011ae4ce3878dd5ba88dfe35d859d05498191637c3";
const RTK_SHA256_LINUX_X86_64: &str =
    "c4c036fbf181fc55ef329786c8c17e0d427972b053b825944d968a6aafef1ba4";
const RTK_SHA256_WINDOWS_X86_64: &str =
    "34cea9009a8099acdaf85147b971d95f65efabfa63fb3aea7d3e2b73e6f517c3";
const PYTHON_STANDALONE_RELEASE: &str = "20251014";
const PYTHON_SHA256_MACOS_AARCH64: &str =
    "84cb7acbf75264982c8bdd818bfa1ff0f1eb76007b48a5f3e01d28633b46afdf";
const PYTHON_SHA256_MACOS_X86_64: &str =
    "f76a921e71e9c8954cccd00f176b7083041527b3b4223670d05bbb2f51209d3f";
const PYTHON_SHA256_LINUX_X86_64: &str =
    "c74addcd1b033a6e4d60ead3ab47fcc995569027e01d3061c4a934f363c4a0cf";
const PYTHON_SHA256_LINUX_AARCH64: &str =
    "d2a6c0d4ceea088f635b309a59d5d700a256656423225f96ddfb71d532adb1aa";
const PYTHON_SHA256_WINDOWS_X86_64: &str =
    "3c8b9b10a933909c98b9916297e2093b24a9c2abaa23df1c2622c2bfe052cb94";

/// torch and onnxruntime cannot load on a Windows box without the MSVC
/// redistributable, which a bare install (notably Server 2022) does not ship
/// (RUST-7W warning half, RUST-8V/8W fatal half). Installing vc_redist.exe
/// needs admin elevation our per-user install does not have, so instead the
/// runtime DLLs are vendored from the sha256-pinned `msvc-runtime` wheel next
/// to python.exe, where the loader's application-directory search finds them.
/// The cp-tag on the wheel is irrelevant: only its `.data/data/Scripts/*.dll`
/// payload is extracted, never the .pyd. Windows is x86_64-only (see the
/// PYTHON pins above), so one wheel covers the platform.
const MSVC_RUNTIME_WHEEL_URL: &str = "https://files.pythonhosted.org/packages/21/3b/134d04268ab8e35853cd007582076429b45d60d6abb1036d159be9c50342/msvc_runtime-14.44.35112-cp312-cp312-win_amd64.whl";
const MSVC_RUNTIME_WHEEL_SHA256: &str =
    "32f9c706009e16ccc319d6947ce3bffe20e5192bee52b18cf48313f9e7bedfbe";

/// Venv layout differs per platform: Unix venvs place interpreters and
/// console-script entrypoints in `bin/` (python3, no extension); Windows venvs
/// use `Scripts/` with `.exe`-suffixed names. The standalone python extracted
/// by `install_python_distribution` is the exception: on Windows the
/// interpreter is `python.exe` at the extraction root.
fn python_exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "python.exe"
    } else {
        "python3"
    }
}

fn pip_exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "pip.exe"
    } else {
        "pip"
    }
}

fn bin_subdir() -> &'static str {
    if cfg!(target_os = "windows") {
        "Scripts"
    } else {
        "bin"
    }
}

/// The wheel ships every DLL twice (`.data/data/` root and `.data/data/Scripts/`);
/// only the Scripts set is taken -- it is the superset (it adds vcruntime140
/// and vcruntime140_1) and taking one set keeps extraction single-pass.
fn msvc_runtime_dll_name(entry_path: &str) -> Option<&str> {
    let (dirs, name) = entry_path.rsplit_once('/')?;
    (dirs.ends_with(".data/data/Scripts") && name.to_ascii_lowercase().ends_with(".dll"))
        .then_some(name)
}

/// Extract the MSVC runtime DLLs from the pinned wheel into every target dir.
/// atomic_write per DLL: a crash mid-write must leave absence (retried next
/// launch), never a truncated msvcp140.dll that torch then fails on weirdly.
fn extract_msvc_runtime_dlls(wheel_path: &Path, targets: &[&Path]) -> Result<usize> {
    let file = std::fs::File::open(wheel_path)
        .with_context(|| format!("opening {}", wheel_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", wheel_path.display()))?;
    let mut extracted = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(name) = msvc_runtime_dll_name(entry.name()).map(str::to_owned) else {
            continue;
        };
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        for target in targets {
            std::fs::create_dir_all(target)
                .with_context(|| format!("creating {}", target.display()))?;
            crate::client_adapters::atomic_write(&target.join(&name), &bytes)?;
        }
        extracted += 1;
    }
    Ok(extracted)
}

#[derive(Debug, Clone)]
pub struct BootstrapStepUpdate {
    pub step: &'static str,
    pub message: String,
    pub eta_seconds: u64,
    pub percent: u8,
}

/// Last progress milestone of a bootstrap attempt that never reached a
/// verdict. Persisted by `note_bootstrap_attempt` on every progress update,
/// cleared when the attempt succeeds or fails with a classification, and
/// consumed on the next launch by `take_abandoned_bootstrap` -- the silent
/// half of install failures (app quit, crash, kill mid-install) that the
/// error branch can never see.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AbandonedBootstrap {
    pub step: String,
    pub percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedRuntime {
    pub root_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub python_dir: PathBuf,
    pub venv_dir: PathBuf,
    pub tools_dir: PathBuf,
    pub downloads_dir: PathBuf,
}

impl ManagedRuntime {
    pub fn bootstrap_root(base_dir: &Path) -> Self {
        let root_dir = base_dir.join("headroom");
        let runtime_dir = root_dir.join("runtime");
        let bin_dir = root_dir.join("bin");
        let python_dir = runtime_dir.join("python");
        let venv_dir = runtime_dir.join("venv");
        let tools_dir = root_dir.join("tools");
        let downloads_dir = root_dir.join("downloads");

        Self {
            root_dir,
            runtime_dir,
            bin_dir,
            python_dir,
            venv_dir,
            tools_dir,
            downloads_dir,
        }
    }

    pub fn ensure_layout(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root_dir)
            .with_context(|| format!("creating {}", self.root_dir.display()))?;
        std::fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("creating {}", self.runtime_dir.display()))?;
        std::fs::create_dir_all(&self.bin_dir)
            .with_context(|| format!("creating {}", self.bin_dir.display()))?;
        std::fs::create_dir_all(&self.tools_dir)
            .with_context(|| format!("creating {}", self.tools_dir.display()))?;
        std::fs::create_dir_all(&self.downloads_dir)
            .with_context(|| format!("creating {}", self.downloads_dir.display()))?;
        Ok(())
    }

    pub fn standalone_python(&self) -> PathBuf {
        if cfg!(target_os = "windows") {
            self.python_dir.join("python.exe")
        } else {
            self.python_dir.join("bin").join("python3")
        }
    }

    /// True when the base interpreter's stdlib is where CPython's own getpath
    /// looks for it (`Lib\os.py` on Windows, `lib/python3.X/os.py` elsewhere).
    /// A base whose `python.exe` survived but whose `Lib` tree did not (AV
    /// quarantine, disk cleanup: RUST-C8) passes the executable check, then
    /// every spawn dies in `init_fs_encoding` with `No module named
    /// 'encodings'`; getpath falls back to the child's cwd as prefix, which
    /// is why that log blames `<root>\Lib`. Any `python3.*` dir counts, so a
    /// pinned-Python bump cannot flip every install to "not installed".
    pub fn standalone_stdlib_present(&self) -> bool {
        if cfg!(target_os = "windows") {
            return self.python_dir.join("Lib").join("os.py").is_file();
        }
        let Ok(entries) = std::fs::read_dir(self.python_dir.join("lib")) else {
            return false;
        };
        entries.flatten().any(|entry| {
            entry.file_name().to_string_lossy().starts_with("python3")
                && entry.path().join("os.py").is_file()
        })
    }

    /// Base interpreter present and able to boot (see
    /// [`Self::standalone_stdlib_present`]). This, not `standalone_python()`
    /// alone, is the gate for both "installed" and "skip the download":
    /// bootstrap used to skip the only repair path for `runtime/python` as
    /// soon as `python.exe` existed, so a stdlib-less base stayed broken until
    /// someone deleted the directory by hand.
    pub fn standalone_runtime_intact(&self) -> bool {
        self.standalone_python().exists() && self.standalone_stdlib_present()
    }

    pub fn managed_python(&self) -> PathBuf {
        self.venv_dir.join(bin_subdir()).join(python_exe_name())
    }

    pub fn managed_pip(&self) -> PathBuf {
        self.venv_dir.join(bin_subdir()).join(pip_exe_name())
    }

    pub fn ready_flag(&self) -> PathBuf {
        self.venv_dir.join("READY")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root_dir.join("logs")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedToolManifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub runtime: String,
    pub source_url: String,
    pub version: String,
    pub checksum: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct ToolManager {
    runtime: ManagedRuntime,
    manifests: Vec<ManagedToolManifest>,
    log_marker_cache: Arc<Mutex<Option<ToolLogMarkerCache>>>,
    serena_calls_cache: Arc<Mutex<Option<SerenaCallsCache>>>,
    serena_live_stats_cache: Arc<Mutex<Option<(Instant, Option<(u64, Option<Instant>)>)>>>,
    /// False once this app process has tried to start the backend at least
    /// once. See its use in `start_headroom_background`.
    first_backend_start: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone)]
struct ToolLogMarkerCache {
    tool_id: String,
    path: PathBuf,
    modified: std::time::SystemTime,
    result: Option<bool>,
}

/// Cached result of scanning today's serena logs for tool-call lines.
#[derive(Debug)]
struct SerenaCallsCache {
    day: String,
    at: Instant,
    count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RtkDailyGainOutput {
    #[serde(default)]
    summary: Option<RtkGainSummary>,
    #[serde(default)]
    daily: Vec<RtkDailyEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct RtkDailyEntry {
    date: String,
    #[serde(default)]
    commands: u64,
    #[serde(default)]
    saved_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RtkGainSummary {
    pub total_commands: u64,
    pub total_saved: u64,
    pub avg_savings_pct: f64,
}

#[derive(Debug, Clone)]
struct HeadroomLearnMetadata {
    learned_at: Option<String>,
    pattern_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct HeadroomLearnProjectSummary {
    pub last_run_at: Option<String>,
    pub has_persisted_learnings: bool,
    pub pattern_count: Option<usize>,
}

/// HuggingFace hub cache directory name for the Kompress model. Uninstall
/// cleanup sweeps the whole `models--chopratejas--` prefix instead of this one
/// name, so the two can drift apart without leaking.
pub(crate) const KOMPRESS_HF_MODEL_DIR: &str = "models--chopratejas--kompress-v2-base";

/// HuggingFace hub cache directory, resolved the way `huggingface_hub` itself
/// resolves it. Precedence mirrors its `constants.py`: `HF_HUB_CACHE`, then the
/// legacy `HUGGINGFACE_HUB_CACHE`, then `$HF_HOME/hub`, then
/// `${XDG_CACHE_HOME:-~/.cache}/huggingface/hub`.
///
/// Reading it from our own env is correct by construction: the bundled runtime
/// is spawned as our child and inherits this env, so wherever we resolve to is
/// where it actually writes. Hardcoding the default bit us twice — the prefetch
/// guard re-downloaded an already-cached model, and uninstall left it behind.
///
/// Two deliberate deviations from python: no `$VAR` expansion inside the values
/// (`os.path.expandvars`), and an empty value is treated as unset where
/// `os.getenv` would return `""` and resolve to a CWD-relative path. Both cases
/// are already broken upstream; falling back to the default beats guessing.
/// ponytail: add expansion if anyone reports a `$`-containing value.
pub(crate) fn hf_hub_cache_dir() -> Option<PathBuf> {
    fn var(key: &str) -> Option<PathBuf> {
        std::env::var_os(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    var("HF_HUB_CACHE")
        .or_else(|| var("HUGGINGFACE_HUB_CACHE"))
        .or_else(|| var("HF_HOME").map(|home| home.join("hub")))
        .or_else(|| {
            // `HOME` before `dirs::home_dir()`: on Windows the dirs crate reads the
            // profile known folder and ignores `$HOME`, so a redirected home (tests,
            // Git Bash) would resolve the sweep against the REAL profile instead.
            let cache = var("XDG_CACHE_HOME").or_else(|| {
                var("HOME")
                    .or_else(dirs::home_dir)
                    .map(|h| h.join(".cache"))
            })?;
            Some(cache.join("huggingface").join("hub"))
        })
}

/// Result of a best-effort kompress model prefetch.
pub enum KompressPrefetchOutcome {
    /// Model successfully downloaded and cached.
    Downloaded,
    /// Subprocess exited non-zero. `cause` is a coarse category plus the last
    /// meaningful line of `kompress-prefetch.log`, suitable for Sentry.
    Failed { cause: String },
}

/// Build a short, Sentry-friendly cause from the tail of the prefetch log.
/// The leading `[category]` keeps related failures grouped; the trailing line
/// carries the specific error for triage.
/// httpx (used by huggingface_hub 1.x) reads `SSL_CERT_FILE`/`SSL_CERT_DIR` but
/// ignores `REQUESTS_CA_BUNDLE`. When a user behind TLS inspection has set
/// `REQUESTS_CA_BUNDLE` but not `SSL_CERT_FILE`, mirror it so the model download
/// trusts their corporate root. No-op if `SSL_CERT_FILE` is already set or no
/// bundle is configured -- the child otherwise inherits the parent env unchanged.
fn httpx_ca_bundle_bridge() -> Vec<(String, String)> {
    httpx_ca_bundle_bridge_from(
        std::env::var_os("SSL_CERT_FILE").is_some(),
        std::env::var("REQUESTS_CA_BUNDLE").ok().as_deref(),
    )
}

fn httpx_ca_bundle_bridge_from(
    ssl_cert_file_set: bool,
    requests_ca_bundle: Option<&str>,
) -> Vec<(String, String)> {
    if ssl_cert_file_set {
        return Vec::new();
    }
    match requests_ca_bundle {
        Some(bundle) if !bundle.trim().is_empty() => {
            vec![("SSL_CERT_FILE".to_string(), bundle.to_string())]
        }
        _ => Vec::new(),
    }
}

fn summarize_kompress_prefetch_failure(log_path: &Path) -> String {
    let contents = std::fs::read_to_string(log_path).unwrap_or_default();
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(40);
    let tail = lines[start..].join("\n");

    let category = classify_kompress_prefetch_failure(&tail);
    let detail: String = tail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .next_back()
        .unwrap_or("(no output in kompress-prefetch.log)")
        .chars()
        .take(200)
        .collect();

    // The auto backend tries ONNX first and only then falls back to PyTorch, so
    // when both are broken the last line is the SECOND failure and the ONNX
    // cause that actually explains it scrolls past unreported. RUST-75 arrived
    // as a torch `c10.dll` load error with no hint that onnxruntime -- which
    // needs the same MSVC redistributable -- had already failed for the same
    // reason. Carry the first cause too when the loader logged one.
    let onnx_cause: Option<String> = tail
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| line.contains("ONNX load failed"))
        .map(|line| line.chars().take(160).collect());

    // The Sentry fingerprint is the bracketed category, so widening the detail
    // does not regroup the issue.
    match onnx_cause {
        Some(onnx) => format!("[{category}] {detail} (after {onnx})"),
        None => format!("[{category}] {detail}"),
    }
}

/// Bucket a prefetch-log tail into a coarse, stable failure category.
fn classify_kompress_prefetch_failure(tail: &str) -> &'static str {
    let t = tail.to_lowercase();
    if t.is_empty() {
        "no output"
    } else if t.contains("sigabrt") || t.contains("aborted") {
        "native abort"
    } else if t.contains("no space left") || t.contains("disk full") || t.contains("errno 28") {
        "disk full"
    } else if t.contains("connection")
        || t.contains("timed out")
        || t.contains("timeout")
        || t.contains("name resolution")
        || t.contains("failed to resolve")
        || t.contains("max retries exceeded")
        || t.contains("ssl")
        || t.contains("httperror")
        // huggingface_hub 1.x transient: shared httpx client torn down mid-pull
        // (RUST-3C). A fresh subprocess gets a fresh client, so it's retriable.
        || t.contains("client has been closed")
    {
        "network"
    } else if t.contains("permission denied") {
        "permission denied"
    // torch's DLLs need the MSVC redistributable, which a fresh Windows box
    // (notably Server 2022) does not ship. Nothing retriable and nothing the
    // app can repair, but it must not sit in the "other" grab-bag: that bucket
    // is what made RUST-3C/RUST-45 unresolvable.
    //
    // 126 is "the DLL is absent", 1114 is "it is there and its init routine
    // failed" (RUST-75, c10.dll) -- different Windows errnos, same broken
    // native stack, same non-answer for us, so one bucket. Match the number,
    // not the sentence after it: Windows localizes that text, and 1114 landed
    // in the grab-bag as Korean.
    } else if t.contains("winerror 126")
        || t.contains("winerror 1114")
        || t.contains("importerror: dll load failed")
    {
        "missing native dep"
    } else {
        "other"
    }
}

/// Which of the two causes a timed-out tiktoken prefetch actually hit.
///
/// The alarm fires only when the cache dir was empty on entry (the gate in
/// [`ToolManager::prefetch_tiktoken_encodings`] returns early otherwise), so
/// whether anything landed in it separates the two: nothing at all means the
/// vocab host never answered (blocked egress, DNS, captive portal), some bytes
/// means a slow link that only needed longer. tiktoken prints nothing while it
/// blocks on the GET, so the log tail is empty either way -- RUST-2K carried
/// 326 events over four months with no payload beyond the words "stalled vocab
/// download", which is an alarm that teaches nothing when it goes off.
///
/// Two fixed phrases, never a byte count: a number in the message would
/// fragment the fingerprint the way per-tail messages did in RUST-6M/6N/6P.
fn stalled_prefetch_cause(cache_dir: &Path) -> &'static str {
    let reached = std::fs::read_dir(cache_dir)
        .map(|mut dir| dir.next().is_some())
        .unwrap_or(false);
    if reached {
        "vocab host reachable but slow"
    } else {
        "vocab host never answered"
    }
}

impl ToolManager {
    pub fn new(runtime: ManagedRuntime) -> Self {
        let rtk_checksum = rtk_distribution_artifact()
            .ok()
            .and_then(|artifact| artifact.sha256.map(str::to_owned));
        let manifests = vec![
            ManagedToolManifest {
                id: "headroom".into(),
                name: "Headroom".into(),
                description: "Default optimizer stage for every supported client.".into(),
                runtime: "python".into(),
                source_url: "https://pypi.org/project/headroom-ai/".into(),
                version: HEADROOM_PINNED_VERSION.into(),
                checksum: None,
                required: true,
            },
            ManagedToolManifest {
                id: "rtk".into(),
                name: "RTK".into(),
                description:
                    "Token-optimized shell command proxy for your coding agent and your terminal.".into(),
                runtime: "binary".into(),
                source_url: "https://github.com/rtk-ai/rtk".into(),
                version: RTK_VERSION.into(),
                checksum: rtk_checksum,
                required: false,
            },
            ManagedToolManifest {
                id: "markitdown".into(),
                name: "MarkItDown".into(),
                description:
                    "Converts PDF and Office documents to Markdown so they cost far fewer tokens when your agent reads them."
                        .into(),
                runtime: "python".into(),
                source_url: "https://github.com/microsoft/markitdown".into(),
                version: MARKITDOWN_PINNED_VERSION.into(),
                checksum: None,
                required: false,
            },
            ManagedToolManifest {
                id: "serena".into(),
                name: "Serena".into(),
                description:
                    "MCP server that gives your agent symbol-level code tools, so it reads one function instead of a whole file. Saves most in large repos; adds its tool definitions to every request."
                        .into(),
                runtime: "python".into(),
                source_url: "https://github.com/oraios/serena".into(),
                version: SERENA_PINNED_VERSION.into(),
                checksum: None,
                required: false,
            },
            ManagedToolManifest {
                id: "codebase-memory".into(),
                name: "Codebase Memory".into(),
                description:
                    "MCP server that indexes your codebase into a persistent knowledge graph - call chains, classes, routes - so your agent answers structure questions from the graph instead of re-reading files. Complements Serena: pre-built map vs live symbol tools."
                        .into(),
                runtime: "binary".into(),
                source_url: "https://github.com/DeusData/codebase-memory-mcp".into(),
                version: CODEBASE_MEMORY_VERSION.into(),
                checksum: None,
                required: false,
            },
            ManagedToolManifest {
                id: "context7".into(),
                name: "Context7".into(),
                description:
                    "MCP server that fetches current, version-specific documentation for the libraries you use, so your agent stops burning tokens on guessed or outdated APIs. Requires Node.js 20.18.1 or newer on PATH."
                        .into(),
                runtime: "node".into(),
                source_url: "https://github.com/upstash/context7".into(),
                version: CONTEXT7_PINNED_VERSION.into(),
                checksum: None,
                required: false,
            },
            ManagedToolManifest {
                id: "ponytail".into(),
                name: "Ponytail".into(),
                description:
                    "Plugin that nudges the agent to write the least code possible. Installs into Claude Code and Codex. Requires their CLI and Node.js on PATH."
                        .into(),
                runtime: "plugin".into(),
                source_url: "https://github.com/DietrichGebert/ponytail".into(),
                version: PLUGIN_DISPLAY_VERSION.into(),
                checksum: None,
                required: false,
            },
            ManagedToolManifest {
                id: "caveman".into(),
                name: "Caveman".into(),
                description:
                    "Plugin that makes the agent reply in terse caveman-speak, cutting output tokens while keeping code, commands, and errors exact. Installs into Claude Code and Codex. Requires their CLI and Node.js on PATH."
                        .into(),
                runtime: "plugin".into(),
                source_url: "https://github.com/JuliusBrussee/caveman".into(),
                version: PLUGIN_DISPLAY_VERSION.into(),
                checksum: None,
                required: false,
            },
        ];

        Self {
            runtime,
            manifests,
            log_marker_cache: Arc::new(Mutex::new(None)),
            serena_calls_cache: Arc::new(Mutex::new(None)),
            serena_live_stats_cache: Arc::new(Mutex::new(None)),
            first_backend_start: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub fn list_tools(&self) -> Vec<ManagedTool> {
        self.manifests
            .iter()
            .map(|manifest| {
                let installed = self.installed_addon_version(&manifest.id);
                let enabled = self.tool_enabled(&manifest.id);
                // Never offer Update on a disabled addon: every installer
                // writes `enabled: true`, so the update would silently switch
                // it back on. Enable first, then update.
                let pending = enabled
                    .then(|| {
                        pending_addon_update(&manifest.id, installed.as_deref(), &manifest.version)
                    })
                    .flatten();
                let update_available = pending.is_some();
                ManagedTool {
                    id: manifest.id.clone(),
                    name: manifest.name.clone(),
                    description: manifest.description.clone(),
                    runtime: manifest.runtime.clone(),
                    required: manifest.required,
                    enabled,
                    status: self.detect_status(&manifest.id),
                    source_url: manifest.source_url.clone(),
                    version: installed.unwrap_or_else(|| manifest.version.clone()),
                    checksum: manifest.checksum.clone(),
                    savings_label: self.tool_savings_label(&manifest.id),
                    update_available,
                    available_version: pending.filter(|version| !version.is_empty()),
                    unavailable_reason: addon_unavailable_reason(&manifest.id),
                }
            })
            .collect()
    }

    /// The version actually on disk, from whichever record the addon's own
    /// installer writes: the headroom receipt, the host CLI's plugin registry,
    /// or the `<id>.json` tool receipt every other installer writes. None when
    /// the addon is not installed.
    fn installed_addon_version(&self, tool_id: &str) -> Option<String> {
        if tool_id == "headroom" {
            return self.installed_headroom_version();
        }
        if let Some(plugin) = plugin_addon(tool_id) {
            return installed_plugin_version(plugin);
        }
        if !self.addon_installed(tool_id) {
            return None;
        }
        self.read_tool_receipt(tool_id)?
            .get("version")?
            .as_str()
            .map(str::to_string)
    }

    /// Whether an installed addon has the artifact its receipt claims. A
    /// receipt without its payload (interrupted install, user-deleted venv) is
    /// not an install, and must not produce a version or an update prompt.
    fn addon_installed(&self, tool_id: &str) -> bool {
        match tool_id {
            "rtk" => self.rtk_installed(),
            "markitdown" => self.markitdown_installed(),
            "serena" => self.serena_installed(),
            "context7" => self.context7_installed(),
            "codebase-memory" => self.codebase_memory_installed(),
            _ => false,
        }
    }

    /// Chip text for the Addons tab. markitdown and serena are measured
    /// (shim counter / serena's logs plus its live dashboard stats); ponytail
    /// and caveman are the plugins' published benchmark medians — their skills
    /// forbid inventing per-repo figures, so the labels say "benchmark". rtk's
    /// figure comes from `rtk gain` via RuntimeStatus, not from here.
    fn tool_savings_label(&self, tool_id: &str) -> Option<String> {
        match tool_id {
            "markitdown" => self.markitdown_conversion_count().map(|count| {
                if count == 1 {
                    "1 doc converted".to_string()
                } else {
                    format!("{count} docs converted")
                }
            }),
            "serena" => {
                let live = self.serena_live_stats().map(|(tokens, session_start)| {
                    (tokens, session_start.map(|start| start.elapsed()))
                });
                serena_savings_label(self.serena_tool_calls_local_today(), live)
            }
            "ponytail" => Some("47-77% lower cost (benchmark)".to_string()),
            "caveman" => Some("~65% fewer output tokens (benchmark)".to_string()),
            _ => None,
        }
    }

    /// Serena slot for the Activity-tab feed: the same two measures as the
    /// Addons-tab chip, pre-formatted by the shared `serena_savings_parts`.
    /// None when serena is not installed or neither measure has data yet.
    pub fn serena_today_stats(&self) -> Option<crate::models::SerenaTodayStats> {
        if !self.serena_installed() {
            return None;
        }
        let live = self
            .serena_live_stats()
            .map(|(tokens, session_start)| (tokens, session_start.map(|start| start.elapsed())));
        let (calls_line, tokens_line) =
            serena_savings_parts(self.serena_tool_calls_local_today(), live);
        if calls_line.is_none() && tokens_line.is_none() {
            return None;
        }
        Some(crate::models::SerenaTodayStats {
            calls_line,
            tokens_line,
        })
    }

    /// Estimated tokens serena's tools have returned in the live session(s),
    /// summed from each running MCP process's dashboard `/get_tool_stats`
    /// (v1.7.0 records unconditionally, CHAR_COUNT estimator; our registration
    /// only suppresses the browser popup, not the dashboard server). In-memory
    /// upstream, so the figure resets when the session ends — by design.
    /// Paired with the oldest matching MCP process's start (from `ps` etime),
    /// so the chip can say over what span those tokens accumulated.
    /// 60s cache; closed local ports refuse instantly, so a miss is cheap.
    fn serena_live_stats(&self) -> Option<(u64, Option<Instant>)> {
        if !self.serena_installed() {
            return None;
        }
        {
            let cache = self.serena_live_stats_cache.lock();
            if let Some((at, stats)) = cache.as_ref() {
                if at.elapsed() < Duration::from_secs(60) {
                    return *stats;
                }
            }
        }
        // The dashboard scans upward from the base port when it's taken, one
        // process per MCP session; sum whatever responds in the first few.
        let mut total: u64 = 0;
        let mut any_responder = false;
        for offset in 0..SERENA_DASHBOARD_PORT_SCAN {
            let port = SERENA_DASHBOARD_BASE_PORT + offset;
            if let Some(tokens) = fetch_serena_output_tokens(&format!("http://127.0.0.1:{port}")) {
                any_responder = true;
                total = total.saturating_add(tokens);
            }
        }
        let stats = (any_responder && total > 0).then(|| {
            let session_start = self
                .serena_oldest_session_age()
                .and_then(|age| Instant::now().checked_sub(age));
            (total, session_start)
        });
        *self.serena_live_stats_cache.lock() = Some((Instant::now(), stats));
        stats
    }

    /// Age of the oldest running serena MCP session, from `ps` elapsed time.
    /// Matched on our managed entrypoint path plus the `start-mcp-server`
    /// subcommand — same argv-identity idea as `pid_is_headroom_backend`.
    /// With several sessions the tokens above span all of them, so the oldest
    /// is the honest window. `-ww`: unlimited width, argv must not truncate.
    fn serena_oldest_session_age(&self) -> Option<Duration> {
        let output = crate::proc::command("/bin/ps")
            .args(["-axww", "-o", "etime=,args="])
            .output()
            .ok()?;
        let marker = self.serena_entrypoint().display().to_string();
        oldest_serena_session_age(&String::from_utf8_lossy(&output.stdout), &marker)
    }

    /// Today's serena tool calls, counted from `~/.serena/logs/<local day>/`.
    /// Serena's usage stats are in-memory only (analytics.py), so its log
    /// files are the only persisted trace: one line containing
    /// "; session_id: " per tool application (tools_base.py, pinned v1.7.0).
    /// Local day matches serena's own log-dir bucketing (datetime.now()).
    /// 60s cache: Result lines make these files large enough that scanning on
    /// every dashboard poll would be wasteful.
    fn serena_tool_calls_local_today(&self) -> Option<u64> {
        let day = Local::now().format("%Y-%m-%d").to_string();
        {
            let cache = self.serena_calls_cache.lock();
            if let Some(cached) = cache.as_ref() {
                if cached.day == day && cached.at.elapsed() < Duration::from_secs(60) {
                    return cached.count;
                }
            }
        }
        // ponytail: default ~/.serena only; honor SERENA_HOME if a user ever
        // actually overrides it.
        let count = dirs::home_dir()
            .map(|home| home.join(".serena").join("logs").join(&day))
            .and_then(|dir| count_serena_tool_calls_in_dir(&dir));
        *self.serena_calls_cache.lock() = Some(SerenaCallsCache {
            day,
            at: Instant::now(),
            count,
        });
        count
    }

    pub fn python_runtime_installed(&self) -> bool {
        // The base interpreter counts too, not just the venv. A venv's
        // `Scripts/python.exe` is a redirector stub that execs the interpreter
        // recorded in `pyvenv.cfg`; deleting `runtime/python` (AV quarantine,
        // disk cleanup) leaves the stub and the READY flag on disk, so a
        // venv-only gate reads "installed" while every spawn dies with exit 103
        // / `No Python at '...'` (RUST-8E). That matters because this gate is
        // what routes a missing runtime back to setup, and bootstrap already
        // re-downloads the distribution and rebuilds the venv from it -- the
        // only thing standing between the user and a self-repair was this
        // check. Same blind spot as RUST-66/6M one level down.
        // pyvenv.cfg is the same blind spot one file over: the venv's python
        // stub resolves the interpreter through it, so a venv missing only
        // this file passes the three checks below while every spawn (and
        // every pip) dies with exit 106 / `No pyvenv.cfg file` (RUST-6S,
        // third shape). Checking it here is what routes that machine back to
        // bootstrap's rebuild instead of a permanent pip-retry loop.
        // RUST-C8: the base can also lose its stdlib while keeping python.exe
        // (same routing, one directory deeper), hence `intact`, not `exists`.
        self.runtime.ready_flag().exists()
            && self.runtime.managed_python().exists()
            && self.runtime.standalone_runtime_intact()
            && self.runtime.venv_dir.join("pyvenv.cfg").exists()
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.runtime.logs_dir()
    }

    pub fn headroom_entrypoint(&self) -> PathBuf {
        let name = if cfg!(target_os = "windows") {
            "headroom.exe"
        } else {
            "headroom"
        };
        self.runtime.venv_dir.join(bin_subdir()).join(name)
    }

    pub fn managed_python(&self) -> PathBuf {
        self.runtime.managed_python()
    }

    pub fn rtk_entrypoint(&self) -> PathBuf {
        let name = if cfg!(target_os = "windows") {
            "rtk.exe"
        } else {
            "rtk"
        };
        self.runtime.bin_dir.join(name)
    }

    /// Seed the output-shaper savings baseline by mining the user's Claude Code
    /// transcripts once. The proxy's `/stats` `output_shaping` estimate stays
    /// `available: false` until this baseline exists, so without it the
    /// dashboard would never show an output-reduction number. Heuristic-only
    /// (no `--llm-judge`), so it needs no API key or network, and writes the
    /// baseline into `~/.headroom/output_savings.json` (the same `workspace_dir`
    /// the proxy's recorder reads).
    ///
    /// Targets a single transcript-rich project rather than `--all`: upstream's
    /// `_run_verbosity` writes the ledger *inside* its per-project loop
    /// (last-project-wins), so `--apply --all` overwrites the baseline with
    /// whatever project sorts last — often a near-empty one. We instead pick the
    /// project with the most transcript bytes and pass its real path via
    /// `--project`. Baseline strata (model / turn kind / size / tools) are
    /// project-independent, so one busy project yields a usable baseline. (A
    /// proper cross-project aggregate belongs upstream; tracked separately.)
    ///
    /// Best-effort and idempotent: skips when a baseline is already present.
    ///
    /// MUST run *before* the proxy starts. The proxy's `SavingsRecorder` loads
    /// the baseline once at boot and, on its periodic flush, writes its
    /// in-memory ledger back to disk — so a baseline written after the proxy is
    /// running is both invisible (never reloaded) and eventually clobbered by an
    /// empty-baseline flush. Seeding first means the recorder boots with the
    /// real baseline and the number shows without an app relaunch. Synchronous
    /// but bounded by `HEADROOM_BASELINE_SEED_TIMEOUT`; callers already run it on
    /// a background thread, so the one-time ~3s scan never blocks the UI.
    ///
    /// The learned verbosity level it also writes is intentionally ignored: the
    /// proxy spawn pins `HEADROOM_VERBOSITY_LEVEL=2`, the manual-override tier.
    pub fn seed_verbosity_baseline_if_needed(&self) {
        if verbosity_baseline_present() {
            log::debug!("verbosity baseline seeding skipped: baseline already present");
            return;
        }
        let entrypoint = self.headroom_entrypoint();
        if !entrypoint.exists() {
            log::debug!(
                "verbosity baseline seeding skipped: entrypoint not yet installed at {}",
                entrypoint.display()
            );
            return;
        }
        log::info!("seeding output-shaper verbosity baseline (no baseline present yet)");
        let Some(project_cwd) = busiest_claude_project_cwd() else {
            log::info!("verbosity baseline seeding skipped: no Claude transcripts found");
            return;
        };
        let args = [
            "learn",
            "--verbosity",
            "--apply",
            "--project",
            project_cwd.as_str(),
        ];
        // Bounded so a pathological transcript corpus can never hang launch:
        // typical runs are a few seconds; the cap only trips on outliers, after
        // which we proceed and retry next launch.
        match run_command_with_timeout(
            &entrypoint,
            &args,
            &self.runtime.root_dir,
            HEADROOM_BASELINE_SEED_TIMEOUT,
        ) {
            Ok(()) => log::info!("seeded output-shaper verbosity baseline from {project_cwd}"),
            Err(err) => log::info!("verbosity baseline seeding failed: {err:#}"),
        }
    }

    pub fn headroom_learn_log_path(&self, project_path: &str) -> PathBuf {
        let logs_dir = self.runtime.logs_dir();
        let project_name = Path::new(project_path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("project");
        let safe_name: String = project_name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let mut hasher = Sha256::new();
        hasher.update(project_path.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let short_hash = &digest[..12];
        logs_dir.join(format!("headroom-learn-{safe_name}-{short_hash}.log"))
    }

    pub fn headroom_learn_last_run_at(&self, project_path: &str) -> Option<String> {
        let path = self.headroom_learn_log_path(project_path);
        if let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) {
            let timestamp: DateTime<Utc> = modified.into();
            return Some(timestamp.to_rfc3339());
        }

        self.headroom_learn_metadata(project_path)
            .and_then(|metadata| metadata.learned_at)
    }

    /// Bundled metadata used to populate a `ClaudeCodeProject` row. Reads
    /// CLAUDE.md + MEMORY.md once instead of three times, which collapses
    /// 6 file reads per project down to 2 during the project list scan.
    pub fn headroom_learn_project_summary(
        &self,
        project_path: &str,
    ) -> HeadroomLearnProjectSummary {
        let metadata = self.headroom_learn_metadata(project_path);
        let log_last_run_at = std::fs::metadata(self.headroom_learn_log_path(project_path))
            .and_then(|meta| meta.modified())
            .ok()
            .map(|m| {
                let t: DateTime<Utc> = m.into();
                t.to_rfc3339()
            });
        HeadroomLearnProjectSummary {
            last_run_at: log_last_run_at
                .or_else(|| metadata.as_ref().and_then(|m| m.learned_at.clone())),
            has_persisted_learnings: metadata.is_some(),
            pattern_count: metadata.and_then(|m| m.pattern_count),
        }
    }

    /// `reclaim_healthy_orphan`: forwarded to `reclaim_orphan_proxy` so an
    /// upgrade boot validation replaces even a still-healthy old proxy squatting
    /// on 6768. Pass `false` for normal launch (leave a live backend alone).
    pub fn start_headroom_background(&self, reclaim_healthy_orphan: bool) -> Result<Child> {
        // First backend start of this app process: we hold no Child handle, so
        // a backend already on 6768 that `pid_is_headroom_backend` vouches for
        // is an orphan from a previous instance -- classically the one the
        // Windows updater leaves running when it exits the old app with no
        // teardown at all. Take the port back even though the orphan answers
        // /readyz: leaving it alone bails the whole start with "already
        // running", so the new build serves its traffic through the OLD
        // version's backend behind a startup error. Blast radius is the same
        // one the upgrade path already accepts -- one port, one process that
        // had to pass the identity gate.
        let first_start = self
            .first_backend_start
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        let mut allow_repair = true;
        'attempt: loop {
            let python = self.managed_python();
            if !python.exists() {
                bail!("headroom managed python not found at {}", python.display());
            }

            let entrypoint = self.headroom_entrypoint();

            let mut failures: Vec<HeadroomStartupFailure> = Vec::new();
            let logs_dir = self.runtime.logs_dir();
            std::fs::create_dir_all(&logs_dir)
                .with_context(|| format!("creating {}", logs_dir.display()))?;

            // Pre-flight: 6768 may already be held. Three cases:
            //   * Free → spawn on 6768.
            //   * HeadroomRunning → an orphaned proxy from a prior session is
            //     squatting on the port (a healthy one would have satisfied
            //     `is_headroom_proxy_reachable` upstream, so `ensure_headroom_running`
            //     would never have reached the spawn path). Reclaim the port by
            //     terminating it, then spawn the fresh runtime. Only bail if it
            //     turns out to be genuinely health-serving or we can't free it.
            //   * ForeignOccupant → try to fall back to a port in
            //     6769..=6790. Only bail if every fallback is also taken.
            // The chosen port is stored in `backend_port` so the intercept,
            // health probes, and spawn args all pick it up.
            let initial_state = diagnose_proxy_port_settled(backend_port::DEFAULT_BACKEND_PORT);
            match initial_state {
                PortState::Free => {
                    backend_port::set(backend_port::DEFAULT_BACKEND_PORT);
                }
                PortState::HeadroomRunning => {
                    reclaim_orphan_proxy(
                        backend_port::DEFAULT_BACKEND_PORT,
                        reclaim_healthy_orphan || first_start,
                    )?;
                    backend_port::set(backend_port::DEFAULT_BACKEND_PORT);
                }
                PortState::ForeignOccupant(detail) => {
                    let pid = parse_pid_from_lsof_detail(&detail);
                    let try_bind = |port: u16| TcpListener::bind(("127.0.0.1", port)).is_ok();
                    match backend_port::select_fallback(detail.clone(), pid, try_bind) {
                        Ok(SelectedFallback {
                            port,
                            original_occupant,
                            original_pid,
                        }) => {
                            backend_port::set(port);
                            log::warn!(
                                "[backend_port] {} held by {}; falling back to {}",
                                backend_port::DEFAULT_BACKEND_PORT,
                                original_occupant,
                                port,
                            );
                            sentry::with_scope(
                                |scope| {
                                    scope.set_tag("flow", "backend_port_fallback");
                                    scope.set_tag(
                                        "occupant_cmd",
                                        original_occupant
                                            .split(" pid ")
                                            .next()
                                            .unwrap_or("unknown"),
                                    );
                                    scope.set_extra(
                                        "original_port",
                                        backend_port::DEFAULT_BACKEND_PORT.into(),
                                    );
                                    scope.set_extra("chosen_port", port.into());
                                    if let Some(p) = original_pid {
                                        scope.set_extra("occupant_pid", p.into());
                                    }
                                },
                                || {
                                    sentry::capture_message(
                                        &format!(
                                            "backend_port_fallback: {} held by {}, using {}",
                                            backend_port::DEFAULT_BACKEND_PORT,
                                            original_occupant,
                                            port,
                                        ),
                                        sentry::Level::Info,
                                    );
                                },
                            );
                        }
                        Err(AllForeign {
                            original_occupant,
                            fallback_range,
                            ..
                        }) => {
                            bail!(
                                "{}",
                                format_all_foreign_bail(
                                    backend_port::DEFAULT_BACKEND_PORT,
                                    &original_occupant,
                                    fallback_range,
                                )
                            );
                        }
                    }
                }
            }

            // Construct spawn variants AFTER pre-flight so `--port` reflects any
            // fallback chosen above. The arg helpers read `backend_port::get()`
            // eagerly; building them earlier bakes in the stale default and the
            // proxy ends up trying to bind the foreign-held port.
            // Use the console_scripts entrypoint when available to avoid the Python
            // -m double-import RuntimeWarning. Fall back to -m if missing.
            let startup_variants: Vec<(PathBuf, Vec<String>)> = if entrypoint.exists() {
                vec![
                    (
                        entrypoint,
                        headroom_entrypoint_startup_args(
                            self.installed_headroom_version().as_deref(),
                            !crate::client_adapters::is_auto_learn_disabled(),
                        ),
                    ),
                    (python.clone(), headroom_python_startup_args()),
                ]
            } else {
                vec![(python.clone(), headroom_python_startup_args())]
            };

            for (executable, args) in &startup_variants {
                let variant = if args.is_empty() {
                    "default".to_string()
                } else {
                    sanitize_log_variant(&args.join("-"))
                };
                let log_path = logs_dir.join(format!("headroom-{variant}.log"));
                rotate_log_if_large(&log_path);
                let log_file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .with_context(|| format!("opening {}", log_path.display()))?;

                // SIGUSR1 -> faulthandler dump of all Python threads into the
                // proxy log (see SITECUSTOMIZE_PY). The dir holds nothing but
                // sitecustomize.py, so PYTHONPATH can't shadow real imports.
                // A failed write used to cost only the wedge diagnostics; it
                // now also costs the cc-switch Official-branch reset, so the
                // outcome gates HEADROOM_CC_SWITCH_RECONCILE below.
                let inject_dir = self.runtime.root_dir.join("pyinject");
                // Read once per spawn: the env below has to see one consistent
                // override, not three separate reads of a cache another thread
                // could republish in between.
                let upstream_env = upstream_spawn_env(&crate::upstream_override::get());
                let sitecustomize_injected =
                    match std::fs::create_dir_all(&inject_dir).and_then(|_| {
                        std::fs::write(inject_dir.join("sitecustomize.py"), SITECUSTOMIZE_PY)
                    }) {
                        Ok(()) => true,
                        Err(err) => {
                            log::warn!(
                                "[tool_manager] writing pyinject/sitecustomize.py failed: {err}"
                            );
                            false
                        }
                    };

                // Drop control samples left by the abandoned 1% holdout before
                // the 3% one starts filling the arm. One shot, stamped: from
                // here on the control arm is live data and clearing it every
                // spawn would empty it as fast as it fills.
                purge_legacy_output_savings_control_arm_once();

                // Cross-turn dedup + cold-prefix recompaction (headroom-ai
                // 0.33.0; older fallback runtimes ignore unknown envs).
                // DEDUPE is prefix-monotonic/cache-safe in every handler, so
                // it is unconditional. COLD_RECOMPACT rewrites the prefix only
                // when the prompt cache is confirmed dead. The anthropic
                // handler reads Claude Code's real TTL from cache_control; the
                // openai-format handler (Codex/OpenCode/Grok) would fall back
                // to a static 300s guess that busts still-warm caches on
                // 6-60min resumes, so we seed its learned-TTL table (below)
                // with OpenAI's documented worst-case instead of gating the
                // flag off for those connectors. CACHE_TTL_LEARN is
                // observation-only (size-capped local JSONL) and feeds the
                // future TTL learner that can replace the seed with measured
                // values. Connector state is read once per spawn; toggles
                // apply on the next backend restart.
                let cold_recompact = crate::client_adapters::is_claude_code_enabled();
                let cache_ttl_learn = crate::client_adapters::is_codex_enabled();

                // Seed value: a TTL learned from this user's own cache
                // observations when enough have accumulated, else 3600s =
                // OpenAI's "caches are always evicted within 1h" upper bound.
                // Either way Codex-path recompaction only fires on a provably
                // dead cache. Best-effort like sitecustomize.py: if the write
                // fails, resolve_learned_ttl returns None and the proxy uses
                // its 300s static guess — recompact still works, just with
                // the aggressive threshold.
                let ttl_seed_path = self.runtime.root_dir.join("cache_ttl_seed.json");
                let openai_ttl = dirs::home_dir()
                    .map(|h| h.join(".headroom").join("cache_ttl_observations.jsonl"))
                    .and_then(|p| learned_openai_ttl_seconds(&p))
                    .unwrap_or(3600);
                if let Err(err) = crate::client_adapters::atomic_write(
                    &ttl_seed_path,
                    format!(r#"{{"openai": {{"ttl_seconds": {openai_ttl}}}}}"#).as_bytes(),
                ) {
                    log::warn!("[tool_manager] writing cache_ttl_seed.json failed: {err}");
                }

                // Runs at default priority, deliberately. This used to be
                // wrapped in `nice` (+5, then +2 after the first round of
                // starvation) on the theory that the backend is background
                // work that should yield to foreground apps. It isn't: every
                // Claude Code request blocks on it, so deprioritizing it
                // deprioritizes exactly what the user is waiting for. Worse,
                // it inverted under load -- the case it existed for. A niced
                // backend on a contended machine misses the watchdog's health
                // probes, gets force-killed, truncates in-flight SSE streams,
                // and the restart costs more CPU than the nicing ever saved
                // (2026-08-17: 34 restarts in one morning at load 55 on 8
                // cores). The Windows path never niced at all.
                let mut command = {
                    let mut c = crate::proc::command(executable);
                    c.args(args);
                    c
                };
                command.current_dir(&self.runtime.root_dir);
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }
                strip_unsupported_proxy_env(&mut command);
                override_unsupported_registry_proxy(&mut command);
                strip_unusable_sslkeylogfile(&mut command);
                command
                    .env("PYTHONNOUSERSITE", "1")
                    .env("PYTHONPATH", &inject_dir)
                    .env("PYTHONUNBUFFERED", "1")
                    .env("PYTHONFAULTHANDLER", "1")
                    .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
                    .env("PIP_NO_INPUT", "1")
                    // Force huggingface_hub off the native `hf_xet` downloader.
                    // Its Rust extension can SIGABRT ("Fatal Python error: Aborted")
                    // inside xet_get while pulling kompress-int8.onnx during
                    // eager_load_compressors, killing the interpreter before it
                    // binds the port (Sentry: never opened port within timeout).
                    // The SIGABRT is uncatchable in Python; disabling xet falls
                    // back to the stable HTTPS download path.
                    .env("HF_HUB_DISABLE_XET", "1")
                    // Persistent vocab cache. tiktoken defaults to
                    // $TMPDIR/data-gym-cache, which macOS purges, so the backend
                    // re-downloads vocab files; the fetch (requests.get, no
                    // timeout) can stall on a blocked network and wedge backend
                    // boot on the main thread (RUST-5D). A stable dir survives
                    // reboots and runtime reinstalls; prefetch_tiktoken_encodings
                    // seeds it so boot never needs the network for vocab.
                    .env("TIKTOKEN_CACHE_DIR", self.tiktoken_cache_dir())
                    .env("HEADROOM_SDK", "headroom-desktop-proxy")
                    // Anonymous aggregate telemetry (opt-in in the package,
                    // off by default). Desktop opts in on the user's behalf.
                    // This is LOCAL collection only (feeds /stats); keep it on.
                    .env("HEADROOM_TELEMETRY", "on")
                    // headroom-ai 0.34.0 added an upstream phone-home beacon
                    // (session summaries uploaded to Headroom Labs), on by
                    // default. Desktop has its own telemetry; keep the
                    // upstream upload off.
                    .env("HEADROOM_BEACON", "off")
                    .env("HEADROOM_HTTP2", "false")
                    // Disable the HTTP/1.1 keep-alive pool for the upstream
                    // (proxy -> api.anthropic.com) client. Claude Code cancels
                    // streaming requests constantly (ESC, aborted tool calls,
                    // subagent cancellations), which can leave a pooled TLS
                    // connection desynced; reusing it surfaces as
                    // "SSLV3_ALERT_BAD_RECORD_MAC" on the next request. The
                    // proxy's retry path does not catch SSL/RemoteProtocolError,
                    // so the raw error leaks back to the client. Fresh
                    // connection per request avoids reuse of a poisoned socket.
                    .env("HEADROOM_MAX_KEEPALIVE", "0")
                    // Optimization mode. token: compress the frozen tool_result
                    // history for real raw-token savings. cache mode was tested and
                    // reverted -- on Claude Code subscription traffic it is ~a no-op
                    // (CacheAligner ships enabled=False, so the prefix-cache discount
                    // is 100% client-driven; cache mode only avoids busting it and
                    // adds no compression), leaving the savings chart flat.
                    .env("HEADROOM_MODE", "token")
                    .env("HEADROOM_DEDUPE", "1")
                    .env(
                        "HEADROOM_COLD_RECOMPACT",
                        if cold_recompact { "1" } else { "0" },
                    )
                    .env(
                        "HEADROOM_CACHE_TTL_LEARN",
                        if cache_ttl_learn { "1" } else { "0" },
                    )
                    .env("HEADROOM_CACHE_TTL_LEARNED_PATH", &ttl_seed_path)
                    // Off-path background compression (#1171). The Kompress ML pass
                    // over the stable prefix is CPU-bound Rust that releases the GIL,
                    // so 3+ concurrent Claude Code sessions run their passes in true
                    // parallel and saturate CPU. Each pass then stretches past the 30s
                    // COMPRESSION_TIMEOUT; asyncio cancels the awaiter but the Rust
                    // worker is non-preemptible and keeps burning a core to completion
                    // (a "leaked thread"), so the machine never drains and every
                    // session stalls ~30s/request -> apparent freeze. With this on, a
                    // cold-start-large request forwards uncompressed immediately and
                    // compresses on a dedicated single-thread background pool that
                    // never contends with the request path, breaking the cascade.
                    // Costs first-turn savings per session; steady state recovers once
                    // the prefix is compressed once.
                    .env("HEADROOM_BACKGROUND_COMPRESSION", "1")
                    // Pin per-request auth-mode policy enforcement ON. This is what
                    // keeps OAuth/subscription traffic (Claude Code, classified
                    // SUBSCRIPTION by User-Agent) on the conservative
                    // live-zone-only + cache-aligner-off policy so prior-turn
                    // compression never rewrites the frozen, already-cached prefix
                    // and busts Anthropic's prefix cache. Upstream defaults this to
                    // "enabled", but we set it explicitly so a future headroom-ai
                    // bump can't silently flip the default and fall subscription
                    // traffic back to the PAYG-aggressive policy (resolve_policy()
                    // returns policy_default_payg() when enforcement is off) -- a
                    // net loss on cache-billed subscription sessions.
                    .env("HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT", "enabled")
                    // User-message text compression is intentionally OFF: user
                    // turns carry the coding working set (code, errors, paths)
                    // and Claude Code's <system-reminder> blocks (CLAUDE.md),
                    // which the model must see verbatim; the token mass is
                    // tool_results, which compress regardless. As of headroom-ai
                    // 0.34.0 the "coding" persona flipped to
                    // compress_user_messages=True and the profile kwargs force
                    // it on per request (HEADROOM_COMPRESS_USER_MESSAGES can
                    // only force-ENABLE, never disable), so the OFF posture is
                    // enforced by SITECUSTOMIZE_PY flipping the persona field
                    // back. (The 0.34.0 tag-split bug that also mangled
                    // CLAUDE.md system-reminders was fixed upstream in 0.35.0
                    // by #2887; the flip stays because plain user text would
                    // still compress.) Trade-off: gives up the modest
                    // Codex/OpenAI user-text savings (0.5 read-discount, 0.0
                    // write-penalty) that HEADROOM_COMPRESS_USER_MESSAGES=1 enabled.
                    // Output-token shaping (new in headroom-ai 0.27.0). The proxy
                    // never emits output tokens, so this works request-side: it
                    // appends a byte-stable verbosity instruction to the TAIL of
                    // the system prompt (after the cache_control breakpoint, so the
                    // provider prefix cache is preserved) and lowers an
                    // already-present output_config.effort on mechanically-classified
                    // turns. Off by default upstream; enabled here. Effort router
                    // and mechanical-effort use upstream defaults (on, "low"). The
                    // shaper only ever lowers an effort the client already sent and
                    // never toggles thinking.type, so it cannot 400 a model that
                    // lacks effort support.
                    .env("HEADROOM_OUTPUT_SHAPER", "1")
                    // The 0.37.0 wheel added a rollout registry that gates
                    // proxy_output_shaper to the beta channel and defaults the
                    // channel to stable, which silently disabled the shaper on
                    // every install despite the request above (/stats showed
                    // decision: blocked_by_channel). Declaring the beta ring is
                    // the sanctioned lever: this app pins and qualifies the
                    // exact wheel it ships through its own rc pipeline, which
                    // is what the wheel's "beta" ring means. Verified on the
                    // pinned wheel: flips exactly proxy_output_shaper to
                    // enabled (decision: legacy_alias); every other feature
                    // stays not_requested, and qualification_eligible stays
                    // true (unlike HEADROOM_UNSAFE_ALLOW_UNSTABLE_FEATURES,
                    // which marks the install ineligible). Wheel Bump Rules:
                    // diff the FEATURES registry on every bump - a new feature
                    // with default_enabled_in <= beta would auto-enable for all
                    // users because of this declaration.
                    .env("HEADROOM_ROLLOUT_CHANNEL", "beta")
                    // read_maturation is the other beta-ring feature the
                    // registry offers, requested by DEFAULT since 0.9.7-rc.1.
                    // The 0.37.0 freeze policy discards background compression
                    // of already-forwarded history, capping compression at the
                    // fresh tail (~1-2% on big sessions); read_maturation is
                    // the cache-safe recovery leg (holds Read results out of
                    // the provider cache until they quiesce, then relocates
                    // the cache breakpoint; invariant: never mutates a cached
                    // byte). It is still cache-breakpoint machinery -- the
                    // class that cost 89 installs ~17pp on 0.9.4 -- so two
                    // guards stay: HEADROOM_READ_MATURATION=0 is a no-rebuild
                    // kill switch, and promotion to stable waits for
                    // `bin/rails savings:did` on a full staging day measuring
                    // BOTH tok_saved AND cache_read (measuring only the
                    // benefit side is what shipped the 0.9.4 regression).
                    .envs(read_maturation_env())
                    // Pin the steering level explicitly. An explicit env is the
                    // manual-override tier in the shaper's level resolution, so it
                    // wins over the per-user learned level written to verbosity.json
                    // by the baseline-seeding `learn --verbosity` run. That keeps
                    // steering uniform/predictable across users while the seeded
                    // baseline still feeds the /stats savings estimate. Level 2 =
                    // skip pre/postamble, don't restate in-context code/tool output.
                    .env("HEADROOM_VERBOSITY_LEVEL", "2")
                    // 3% of conversations run unshaped, as the control arm of a
                    // standing A/B. This is the only way the output-shaping
                    // number ever stops being a counterfactual: the seeded
                    // baseline is frozen at install time and cannot be relearned
                    // (every transcript written since is already shaped, so a
                    // re-learn would collapse the baseline onto the treatment
                    // mean and report ~0 savings). Control conversations are the
                    // only unshaped replies we will ever see again.
                    //
                    // Assignment is per conversation (`assign_arm` hashes the
                    // conversation key), so a conversation never flips mid-stream
                    // and the prefix cache stays stable.
                    //
                    // 1% was tried first and abandoned, but the fraction was
                    // never the bug: `best_estimate` prefers the measured number
                    // as soon as ONE stratum holds a sample in both arms, which
                    // is how three stale control samples reported -1439.9%. The
                    // desktop no longer reads that figure -- `output_savings`
                    // recomputes both estimators from the ledger and only shows
                    // the measured one once it covers real traffic at a usable
                    // band -- so collecting the arm is now safe. At 1% a heavy
                    // user needs ~90 days to reach a +/-13pp band and a light one
                    // never gets there; 3% reaches the same precision in ~30 days
                    // and still costs only 3% of conversations.
                    //
                    // Invisible to the compression figures: the arm gates the
                    // `shape_request` call alone, so control conversations are
                    // compressed, memory-augmented and cache-aligned exactly like
                    // any other, and the input-savings rate is priced off
                    // `cost.total_input_cost_usd`, which no output token enters.
                    .env("HEADROOM_OUTPUT_HOLDOUT", "0.03")
                    // Agent savings persona (new in headroom-ai 0.30.0). The
                    // `proxy` entrypoint reads HEADROOM_SAVINGS_PROFILE into
                    // config.savings_profile, and proxy_pipeline_kwargs() applies
                    // the persona's compression knobs per request across all
                    // handlers. The "coding" persona holds the active code working
                    // set verbatim (protect_recent=2, protect_analysis_context,
                    // smart_crusher_with_compaction) with a low min_tokens so
                    // compression stays visible, and target_ratio unset so savings
                    // emerge from lossless + relevance rather than a forced keep.
                    // The persona set compress_user_messages=False through
                    // 0.33.x; 0.34.0 flipped it to True, so user-turn
                    // protection is now restored by the SITECUSTOMIZE_PY
                    // persona patch (see that constant).
                    // Version-gated: "coding" only exists in _PROFILES from 0.30.0.
                    // When 0.30.0 boot-validation times out the app falls back to
                    // 0.28.0, whose profile set is {agent-90, balanced} only —
                    // passing "coding" there makes get_agent_savings_profile raise
                    // and the proxy exit 1 before opening the port (Sentry RUST-1M).
                    // Fall back to the runtime default that exists in every version.
                    .env(
                        "HEADROOM_SAVINGS_PROFILE",
                        savings_profile_for_runtime(self.installed_headroom_version().as_deref()),
                    )
                    // cc-switch reconciler. Off upstream by default; the desktop
                    // opts in so a user who points Claude Code at a third-party
                    // Anthropic-compatible endpoint (Kimi, DeepSeek, GLM) keeps
                    // Headroom in the path instead of dropping out of it. The
                    // reconciler captures that endpoint as the proxy upstream and
                    // rewrites env.ANTHROPIC_BASE_URL back to us, leaving the
                    // user's token untouched -- so their traffic is still
                    // compressed, and token-priced providers are exactly where
                    // compression is worth the most. Safe only alongside the
                    // Official-branch upstream reset SITECUSTOMIZE_PY carries:
                    // see cc_switch_reconcile_for_spawn.
                    .env(
                        "HEADROOM_CC_SWITCH_RECONCILE",
                        cc_switch_reconcile_for_spawn(sitecustomize_injected),
                    )
                    // The URL the reconciler writes into the client's
                    // settings.json. Upstream advertises the port this proxy
                    // bound, which is the internal one -- see the guard in
                    // SITECUSTOMIZE_PY. Derived from INTERCEPT_PORT rather than
                    // written out, because a port mismatch here is exactly the
                    // bug being fixed.
                    .env("HEADROOM_CC_SWITCH_PROXY_URL", cc_switch_proxy_url())
                    // User-configured upstream (GLM, Kimi, DeepSeek). Empty
                    // for everyone who has not set one, and an empty env is
                    // the same as unset to the runtime's _get_env_str, so this
                    // is inert by default. In Fallback mode this is only the
                    // boot default and a later cc-switch capture wins, which
                    // is the runtime's own behaviour; Override additionally
                    // pins it (see HEADROOM_CC_SWITCH_PIN_UPSTREAM).
                    .env("ANTHROPIC_TARGET_API_URL", &upstream_env.target_api_url)
                    .env("HEADROOM_CC_SWITCH_PIN_UPSTREAM", upstream_env.pin_upstream)
                    // Lossless-only for a third-party endpoint: no lossy
                    // Kompress, no CCR, so the payload shape stays close to
                    // what the client sent while still saving tokens. These
                    // endpoints are Anthropic-COMPATIBLE, not Anthropic, and
                    // each one's tolerance for a rewritten payload is unknown
                    // until it is checked. Promote a provider to the full
                    // pipeline once it has been.
                    .env("HEADROOM_LOSSLESS", upstream_env.lossless)
                    // Pre-upstream concurrency. The proxy's own auto is
                    // max(2, min(8, cpu_count)) — hard-capped at 8 to protect the
                    // event loop from CPU-bound compression. The desktop runs with
                    // HEADROOM_BACKGROUND_COMPRESSION=1 (above), which moves that
                    // CPU work off the request path, so the semaphore slots are
                    // mostly I/O-bound and the 8-cap just queues heavy multi-agent
                    // load (30+ sessions) until acquire timeouts degrade /readyz
                    // and the watchdog force-kills. Scale with cores instead.
                    .env(
                        "HEADROOM_ANTHROPIC_PRE_UPSTREAM_CONCURRENCY",
                        pre_upstream_concurrency().to_string(),
                    )
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(
                        log_file
                            .try_clone()
                            .with_context(|| format!("cloning {}", log_path.display()))?,
                    ))
                    .stderr(Stdio::from(log_file));
                // Windows: AV/Defender briefly holds the just-installed (or
                // just-scanned) exe open and CreateProcess fails ACCESS_DENIED
                // (os error 5) even though nothing is wrong -- the spawn twin
                // of RUST-9M's rename (Sentry RUST-9X, launch auto-start on
                // 0.8.8). Transient by nature; retry briefly before failing
                // the launch.
                let mut child = crate::client_adapters::retry_transient_denied(|| command.spawn())
                    .with_context(|| {
                        format!(
                            "starting headroom background process: {} {}",
                            executable.display(),
                            args.join(" ")
                        )
                    })?;

                let mut startup_ok = false;
                let mut reason: Option<String> = None;

                let startup_polls = (HEADROOM_STARTUP_TIMEOUT_MS / HEADROOM_STARTUP_POLL_MS).max(1);
                for _ in 0..startup_polls {
                    thread::sleep(Duration::from_millis(HEADROOM_STARTUP_POLL_MS));
                    if is_local_proxy_reachable() {
                        startup_ok = true;
                        break;
                    }

                    match child.try_wait() {
                        Ok(Some(status)) => {
                            reason = Some(format!(
                                "exited with status {} before opening port {}",
                                status,
                                headroom_proxy_port()
                            ));
                            break;
                        }
                        Ok(None) => {}
                        Err(err) => {
                            reason = Some(format!("wait check failed: {}", err));
                            break;
                        }
                    }
                }

                if startup_ok {
                    return Ok(child);
                }

                // Timeout path (process still alive, port never opened): send SIGABRT
                // so PYTHONFAULTHANDLER=1 dumps all-thread tracebacks to the log file
                // before the process dies. Skip if the process already exited on its own.
                if reason.is_none() {
                    let _ = crate::proc::command("/bin/kill")
                        .arg("-ABRT")
                        .arg(child.id().to_string())
                        .status();
                    thread::sleep(Duration::from_millis(500));
                }

                let _ = child.kill();
                let _ = child.wait();

                let reason = reason.unwrap_or_else(|| {
                    format!(
                        "never opened port {} within {}ms",
                        headroom_proxy_port(),
                        HEADROOM_STARTUP_TIMEOUT_MS
                    )
                });
                failures.push(HeadroomStartupFailure {
                    program: executable.display().to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    log_path: log_path.display().to_string(),
                    log_tail: crash_log_excerpt(&log_path),
                    reason,
                });
            }

            // All variants failed. If the proxy crashed because the venv has a
            // pydantic / pydantic-core skew (e.g. a partial upgrade left
            // pydantic-core ahead of pydantic), pin pydantic-core back to the
            // version pydantic asks for and retry once. The error message itself
            // tells us the required version — see extract_required_pydantic_core_version.
            if allow_repair {
                if let Some(target) = failures
                    .iter()
                    .find_map(|f| extract_required_pydantic_core_version(&f.log_tail))
                {
                    log::warn!(
                        "headroom proxy failed with pydantic-core/pydantic skew; \
                     reinstalling pydantic-core=={target} and retrying"
                    );
                    match self.repair_pydantic_core(&target) {
                        Ok(()) => {
                            log::warn!("pydantic-core repair succeeded; retrying headroom startup");
                            allow_repair = false;
                            continue 'attempt;
                        }
                        Err(repair_err) => {
                            log::error!("pydantic-core repair failed: {repair_err:#}");
                        }
                    }
                }
            }

            // Report the variant that actually captured a log tail (a traceback)
            // as the error whose tail is carried into Sentry `extra`. Otherwise
            // an empty-tailed variant can be reported as `last` while a prior
            // variant holds the real traceback — which `prior_summary` then drops
            // (it keeps only program/args/reason). Fall back to the last attempt.
            let chosen = failures
                .iter()
                .rposition(|f| !f.log_tail.is_empty())
                .unwrap_or(failures.len() - 1);
            let last = failures.remove(chosen);
            let prior_summary = prior_attempts_summary(&failures);
            // Silent-crash instrumentation (RUST-9F / RUST-9T): on Windows,
            // python dying with exit code 0xffffffff before opening the port
            // leaves no traceback -- faulthandler never runs when a native DLL
            // kills the process during import. A second distinct host hit this
            // on 2026-08-27, so probe the prime suspect (onnxruntime's native
            // init, pulled in by the ml extras) in a bare interpreter and carry
            // the verdict in the error chain Sentry already captures.
            let onnx_note = if cfg!(windows)
                && (last.reason.contains("0xffffffff")
                    || failures.iter().any(|f| f.reason.contains("0xffffffff")))
            {
                format!(" (onnx probe: {})", self.probe_onnx_import())
            } else {
                String::new()
            };
            // Probe verdict before the attempt list: the log bridge caps a
            // Sentry message at 400 chars, and RUST-BX/RUST-BV arrived as
            // "(onnx probe: o" -- the one diagnostic the whole instrumentation
            // exists for, cut off behind a Windows venv path.
            return Err(anyhow::Error::from(last).context(format!(
                "unable to keep headroom running in background{}{}",
                onnx_note, prior_summary
            )));
        }
    }

    /// Diagnostic for the silent 0xffffffff exit class (RUST-9F / RUST-9T):
    /// imports onnxruntime in a bare interpreter so a native DLL-init crash
    /// reproduces in isolation. Best-effort -- every outcome, including a
    /// crash or a missing module, IS the diagnosis. Only called on the
    /// already-failed startup path, so the extra subprocess costs nothing in
    /// the happy path.
    fn probe_onnx_import(&self) -> String {
        let python = self.managed_python();
        if !python.exists() {
            return "managed python missing".to_string();
        }
        // Bounded: an unbounded `.output()` here meant a DLL that deadlocks
        // during import would hang this thread forever -- turning a host that
        // used to fail fast into one that hangs silently. 15s then kill; the
        // "timed out" stderr line is itself the diagnosis. Going through
        // `run_command_with_timeout` also gives the probe the full child-env
        // isolation of `build_command` (PYTHONNOUSERSITE, PYTHONPATH removal)
        // that the ad-hoc spawn only partially set. The pinned venv fixes the
        // onnxruntime version, so the happy-path message doesn't need it.
        match run_command_with_timeout(
            &python,
            &["-c", "import onnxruntime"],
            &self.runtime.root_dir,
            ONNX_PROBE_TIMEOUT,
        ) {
            Ok(()) => "onnxruntime imports cleanly".to_string(),
            Err(err) => match err.downcast_ref::<CommandFailure>() {
                Some(failure) => {
                    let status = match failure.exit_code {
                        Some(code) => format!("exit {code}"),
                        None => "killed".to_string(),
                    };
                    let last_line = failure
                        .stderr
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("<no stderr>");
                    format!("import onnxruntime failed ({status}): {}", last_line.trim())
                }
                None => format!("probe spawn failed: {err:#}"),
            },
        }
    }

    pub fn latest_tool_log_path(&self, tool_id: &str) -> Option<PathBuf> {
        let logs_dir = self.runtime.logs_dir();
        let entries = std::fs::read_dir(&logs_dir).ok()?;
        let prefix = format!("{tool_id}-");
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with(&prefix) && name.ends_with(".log"))
                    .unwrap_or(false)
            })
            .filter_map(|path| {
                let modified = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()?;
                Some((modified, path))
            })
            .collect();

        candidates.sort_by_key(|(modified, _)| *modified);
        candidates.last().map(|(_, path)| path.clone())
    }

    pub fn read_headroom_log_tail(&self, max_lines: usize) -> Result<Vec<String>> {
        self.read_tool_log_tail("headroom", max_lines)
    }

    pub fn read_rtk_activity(&self, max_lines: usize) -> Result<Vec<String>> {
        if !self.rtk_installed() {
            return Ok(vec!["RTK is not installed yet.".into()]);
        }

        let output = crate::proc::command(self.rtk_entrypoint())
            .arg("session")
            .current_dir(&self.runtime.root_dir)
            .output()
            .with_context(|| format!("starting {} session", self.rtk_entrypoint().display()))?;

        if !output.status.success() {
            return Err(anyhow!(
                "command failed: {} session\nstdout:\n{}\nstderr:\n{}",
                self.rtk_entrypoint().display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines: Vec<String> = stdout.lines().map(|line| line.to_string()).collect();
        if lines.len() > max_lines {
            lines = lines.split_off(lines.len() - max_lines);
        }
        Ok(lines)
    }

    pub fn read_tool_log_tail(&self, tool_id: &str, max_lines: usize) -> Result<Vec<String>> {
        let Some(path) = self.latest_tool_log_path(tool_id) else {
            return Ok(Vec::new());
        };

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let lines = content
            .lines()
            .rev()
            .take(max_lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| line.to_string())
            .collect();
        Ok(lines)
    }

    fn latest_tool_log_marker_state(
        &self,
        tool_id: &str,
        enabled_markers: &[&str],
        disabled_markers: &[&str],
    ) -> Option<bool> {
        let path = self.latest_tool_log_path(tool_id)?;
        self.scan_file_for_marker_state_cached(tool_id, &path, enabled_markers, disabled_markers)
    }

    fn scan_file_for_marker_state_cached(
        &self,
        cache_key: &str,
        path: &Path,
        enabled_markers: &[&str],
        disabled_markers: &[&str],
    ) -> Option<bool> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;

        {
            let cache = self.log_marker_cache.lock();
            if let Some(cached) = cache.as_ref() {
                if cached.tool_id == cache_key && cached.path == path && cached.modified == modified
                {
                    return cached.result;
                }
            }
        }

        let content = std::fs::read_to_string(path).ok()?;

        let mut result: Option<bool> = None;
        for line in content.lines().rev() {
            let lowered = line.to_ascii_lowercase();
            if enabled_markers
                .iter()
                .any(|marker| lowered.contains(marker))
            {
                result = Some(true);
                break;
            }
            if disabled_markers
                .iter()
                .any(|marker| lowered.contains(marker))
            {
                result = Some(false);
                break;
            }
        }

        let mut cache = self.log_marker_cache.lock();
        *cache = Some(ToolLogMarkerCache {
            tool_id: cache_key.to_string(),
            path: path.to_path_buf(),
            modified,
            result,
        });

        result
    }

    pub fn headroom_mcp_configured(&self) -> Option<bool> {
        self.read_headroom_receipt()?
            .get("mcp")?
            .get("configured")?
            .as_bool()
    }

    pub fn headroom_mcp_error(&self) -> Option<String> {
        self.read_headroom_receipt()?
            .get("mcp")?
            .get("error")?
            .as_str()
            .map(|value| value.to_string())
    }

    pub fn headroom_mcp_install_method(&self) -> Option<String> {
        self.read_headroom_receipt()?
            .get("mcp")?
            .get("installMethod")?
            .as_str()
            .map(|value| value.to_string())
    }

    pub fn headroom_ml_installed(&self) -> Option<bool> {
        self.read_headroom_receipt()?
            .get("ml")?
            .get("installed")?
            .as_bool()
    }

    pub fn headroom_kompress_enabled(&self) -> Option<bool> {
        // The `headroom` Python package attaches a RotatingFileHandler to its
        // `headroom` root logger with `propagate = False` (see helpers.py:
        // `_setup_file_logging`). Proxy-logger INFO lines — including the
        // `Kompress: ENABLED/not installed/disabled` startup markers — go to
        // `~/.headroom/logs/proxy.log` only, never to the stderr stream that
        // our Tauri-spawned log captures. Probe that file first; fall back to
        // the spawn-time tool log (covers older headroom versions that do
        // propagate to stderr).
        // Positive markers: the startup `Kompress: ENABLED` line (cache hit at
        // eager-preload) AND the lazy-load success lines emitted on first use
        // when the model was downloaded after a cold-cache startup. The scan
        // returns the most recent marker, so a lazy load flips the status to
        // enabled without waiting for a backend restart.
        const KOMPRESS_ENABLED_MARKERS: &[&str] = &[
            "kompress: enabled",
            "kompress onnx loaded",
            "kompress pytorch loaded",
        ];
        const KOMPRESS_DISABLED_MARKERS: &[&str] =
            &["kompress: not installed", "kompress: disabled"];
        if let Some(path) = headroom_propagated_proxy_log_path() {
            if let Some(state) = self.scan_file_for_marker_state_cached(
                "headroom-proxy-log",
                &path,
                KOMPRESS_ENABLED_MARKERS,
                KOMPRESS_DISABLED_MARKERS,
            ) {
                return Some(state);
            }
        }

        self.latest_tool_log_marker_state(
            "headroom",
            KOMPRESS_ENABLED_MARKERS,
            KOMPRESS_DISABLED_MARKERS,
        )
    }

    /// True if the Kompress model snapshot is already present in the
    /// HuggingFace hub cache (`<hf_hub_cache_dir>/<KOMPRESS_HF_MODEL_DIR>/
    /// snapshots/<rev>`). Used as the prefetch idempotency guard so we never
    /// re-download an existing model.
    pub fn kompress_model_cached(&self) -> bool {
        let Some(hub) = hf_hub_cache_dir() else {
            return false;
        };
        let snapshots = hub.join(KOMPRESS_HF_MODEL_DIR).join("snapshots");
        std::fs::read_dir(&snapshots)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    }

    /// Download the Kompress model (~850MB measured, not the ~260MB this doc
    /// used to claim) into the HF cache by running the
    /// bundled venv python's loader with network enabled. Blocks until the
    /// download finishes — call this on a background thread. Output is captured
    /// to `logs/kompress-prefetch.log`.
    ///
    /// This front-loads the download the proxy would otherwise do lazily on
    /// first request, so a fresh install has ML compression ready before any
    /// traffic. It is best-effort: on failure the proxy's own lazy-load path
    /// still downloads on first use. On a non-zero exit the returned
    /// [`KompressPrefetchOutcome::Failed`] carries a short, Sentry-friendly
    /// cause read from the tail of the prefetch log.
    pub fn prefetch_kompress_model(&self) -> Result<KompressPrefetchOutcome> {
        let python = self.managed_python();
        if !python.exists() {
            bail!("headroom managed python not found at {}", python.display());
        }
        let logs_dir = self.runtime.logs_dir();
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("creating {}", logs_dir.display()))?;
        let log_path = logs_dir.join("kompress-prefetch.log");

        // hf_hub_download resumes from its `.incomplete` blob via range
        // requests, so re-invoking the subprocess continues a partial pull
        // rather than restarting it. Retry only network-category failures
        // (RUST-3C): a dropped connection or TLS blip on the ~315MB model is
        // exactly what a second attempt fixes; permission/other failures won't
        // improve on retry.
        //
        // Why retries at the subprocess level at all: huggingface_hub 1.21.0's
        // own in-process backoff self-destructs on the first ConnectError --
        // `_http_backoff_base` caches `client = get_session()` before its retry
        // loop, the ConnectError handler calls `close_session()`, and the next
        // iteration reuses the closed client, raising "RuntimeError: Cannot
        // send a request, as the client has been closed" (the exact RUST-3C
        // message). A fresh subprocess gets a fresh client and resumes the
        // blob. ponytail: 5 attempts, linear backoff -- raised from 3 after
        // 0.5.9 telemetry showed hosts burning all 3 (RUST-45 -> RUST-3C).
        const MAX_ATTEMPTS: u32 = 5;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.run_kompress_prefetch_once(&python, &log_path)? {
                KompressPrefetchOutcome::Downloaded => {
                    return Ok(KompressPrefetchOutcome::Downloaded);
                }
                KompressPrefetchOutcome::Failed { cause } => {
                    if !cause.starts_with("[network]") || attempt == MAX_ATTEMPTS {
                        return Ok(KompressPrefetchOutcome::Failed { cause });
                    }
                    // Info: a retried transient is not a fleet signal (RUST-45
                    // spam); the final Failed outcome carries the cause.
                    log::info!(
                        "kompress prefetch attempt {attempt}/{MAX_ATTEMPTS} failed (retrying): {cause}"
                    );
                    std::thread::sleep(std::time::Duration::from_secs(3 * attempt as u64));
                }
            }
        }
        unreachable!("kompress prefetch loop returns on the final attempt")
    }

    /// Runs a single kompress preload subprocess, appending its output to
    /// `kompress-prefetch.log`. Retry/backoff across network failures lives in
    /// the caller, [`Self::prefetch_kompress_model`].
    fn run_kompress_prefetch_once(
        &self,
        python: &Path,
        log_path: &Path,
    ) -> Result<KompressPrefetchOutcome> {
        rotate_log_if_large(log_path);
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;

        let status = crate::proc::command(python)
            .arg("-c")
            .arg(
                "from headroom.transforms.kompress_compressor import KompressCompressor; \
                 KompressCompressor().preload(allow_download=True)",
            )
            .current_dir(&self.runtime.root_dir)
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONUNBUFFERED", "1")
            // Same xet guard as the proxy spawn: the native hf_xet downloader
            // can SIGABRT mid-pull; the HTTPS fallback is stable.
            .env("HF_HUB_DISABLE_XET", "1")
            // huggingface_hub 1.x downloads over httpx, which reads SSL_CERT_FILE
            // but NOT REQUESTS_CA_BUNDLE. Users behind corporate TLS inspection
            // who set REQUESTS_CA_BUNDLE (per our bootstrap remediation) got pip
            // working but the model pull still failed cert verification (Sentry
            // RUST-3C). Bridge their bundle into SSL_CERT_FILE so httpx honors it.
            .envs(httpx_ca_bundle_bridge())
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                log_file
                    .try_clone()
                    .with_context(|| format!("cloning {}", log_path.display()))?,
            ))
            .stderr(Stdio::from(log_file))
            .status()
            .with_context(|| format!("running kompress prefetch via {}", python.display()))?;

        if status.success() {
            Ok(KompressPrefetchOutcome::Downloaded)
        } else {
            Ok(KompressPrefetchOutcome::Failed {
                cause: summarize_kompress_prefetch_failure(log_path),
            })
        }
    }

    /// Stable on-disk tiktoken vocab cache, passed to the backend as
    /// TIKTOKEN_CACHE_DIR (see the proxy spawn for why the default tmp
    /// location is not good enough).
    pub fn tiktoken_cache_dir(&self) -> PathBuf {
        self.runtime.root_dir.join("tiktoken-cache")
    }

    /// Best-effort pre-download of the tiktoken vocabularies the backend
    /// loads at startup (RUST-5D: tiktoken's vocab fetch has no network
    /// timeout, so a stalled download wedges backend boot until the watchdog
    /// auto-pauses). Seeds [`Self::tiktoken_cache_dir`] once; with the cache
    /// populated the backend never touches the network for vocab. Output goes
    /// to `logs/tiktoken-prefetch.log`. The subprocess is killed after a
    /// deadline for the same no-timeout reason.
    pub fn prefetch_tiktoken_encodings(&self) -> Result<()> {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
        let cache_dir = self.tiktoken_cache_dir();
        // ponytail: coarse skip — any cached entry counts as seeded. A partial
        // cache (one of two vocabs) still self-completes lazily because the
        // backend writes to the same persistent dir.
        if std::fs::read_dir(&cache_dir)
            .map(|mut dir| dir.next().is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }
        let python = self.managed_python();
        if !python.exists() {
            bail!("headroom managed python not found at {}", python.display());
        }
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating {}", cache_dir.display()))?;

        let logs_dir = self.runtime.logs_dir();
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("creating {}", logs_dir.display()))?;
        let log_path = logs_dir.join("tiktoken-prefetch.log");
        rotate_log_if_large(&log_path);
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;

        let mut child = crate::proc::command(&python)
            .arg("-c")
            // cl100k_base: the proxy's default/fallback encoding.
            // o200k_base: current OpenAI model family, loaded for codex traffic.
            .arg(
                "import tiktoken; \
                 tiktoken.get_encoding('cl100k_base'); \
                 tiktoken.get_encoding('o200k_base')",
            )
            .current_dir(&self.runtime.root_dir)
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONUNBUFFERED", "1")
            .env("TIKTOKEN_CACHE_DIR", &cache_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                log_file
                    .try_clone()
                    .with_context(|| format!("cloning {}", log_path.display()))?,
            ))
            .stderr(Stdio::from(log_file))
            .spawn()
            .with_context(|| format!("running tiktoken prefetch via {}", python.display()))?;

        let started = std::time::Instant::now();
        loop {
            match child.try_wait().context("waiting for tiktoken prefetch")? {
                Some(status) if status.success() => return Ok(()),
                Some(status) => {
                    let tail = log_tail(&log_path, 1024);
                    bail!("tiktoken prefetch exited with {status}: {tail}");
                }
                None if started.elapsed() >= DEADLINE => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let tail = log_tail(&log_path, 1024);
                    bail!(
                        "tiktoken prefetch timed out after {}s (stalled vocab download, {}){}",
                        DEADLINE.as_secs(),
                        stalled_prefetch_cause(&cache_dir),
                        if tail.is_empty() {
                            String::new()
                        } else {
                            format!(": {tail}")
                        }
                    );
                }
                None => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
    }

    fn read_headroom_receipt(&self) -> Option<Value> {
        let path = self.runtime.tools_dir.join("headroom.json");
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn read_rtk_receipt(&self) -> Option<Value> {
        let path = self.runtime.tools_dir.join("rtk.json");
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn headroom_learn_metadata(&self, project_path: &str) -> Option<HeadroomLearnMetadata> {
        let mut candidates = self
            .headroom_learn_memory_paths(project_path)
            .into_iter()
            .filter_map(|path| read_headroom_learn_metadata_from_path(&path))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.sort_key.cmp(&left.sort_key));
        candidates
            .into_iter()
            .next()
            .map(|candidate| candidate.metadata)
    }

    fn headroom_learn_memory_paths(&self, project_path: &str) -> Vec<PathBuf> {
        vec![
            // Current learn versions write CLAUDE.local.md; older ones wrote
            // CLAUDE.md. The freshest candidate wins, so list both.
            Path::new(project_path).join("CLAUDE.local.md"),
            Path::new(project_path).join("CLAUDE.md"),
            claude_project_memory_file(project_path),
        ]
    }

    /// Returns the installed Headroom version from the tool receipt, if any.
    pub fn installed_headroom_version(&self) -> Option<String> {
        self.read_headroom_receipt()?
            .get("version")?
            .as_str()
            .map(|v| v.to_string())
    }

    fn installed_requirements_lock_sha(&self) -> Option<String> {
        self.read_headroom_receipt()?
            .get("artifact")?
            .get("requirementsLockSha256")?
            .as_str()
            .map(|v| v.to_string())
    }

    pub fn rtk_installed(&self) -> bool {
        self.rtk_entrypoint().exists() && self.runtime.tools_dir.join("rtk.json").exists()
    }

    pub fn installed_rtk_version(&self) -> Option<String> {
        self.read_rtk_receipt()?
            .get("version")?
            .as_str()
            .map(|v| v.to_string())
    }

    pub fn rtk_needs_install(&self) -> bool {
        !self.rtk_entrypoint().exists()
            || self.installed_rtk_version().as_deref() != Some(RTK_VERSION)
    }

    /// Refresh an *already installed* rtk to the pinned version. Never creates a
    /// fresh install: RTK is opt-in, so a missing binary means the user has not
    /// installed it (or uninstalled it) and launch must leave it absent.
    /// Returns Ok(true) if work was done, Ok(false) if already current or absent.
    pub fn ensure_rtk_current(&self) -> Result<bool> {
        if !self.rtk_entrypoint().exists() {
            return Ok(false);
        }
        if !self.rtk_needs_install() {
            return Ok(false);
        }
        self.install_rtk()?;
        Ok(true)
    }

    fn rtk_gain_output(&self) -> Option<RtkDailyGainOutput> {
        if !self.rtk_installed() {
            return None;
        }
        let output = crate::proc::command(self.rtk_entrypoint())
            .args(["gain", "--daily", "--format", "json"])
            .current_dir(&self.runtime.root_dir)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    pub fn rtk_gain_summary(&self) -> Option<RtkGainSummary> {
        self.rtk_gain_output()?.summary
    }

    pub fn rtk_today_stats(&self) -> Option<RtkTodayStats> {
        let today = Local::now().date_naive().to_string();
        self.rtk_gain_output()?
            .daily
            .into_iter()
            .find(|entry| entry.date == today)
            .map(|entry| RtkTodayStats {
                date: entry.date,
                saved_tokens: entry.saved_tokens,
                commands: entry.commands,
            })
    }

    /// Returns the pinned release if the installed version differs from the pin.
    pub fn check_headroom_upgrade(&self) -> Option<HeadroomRelease> {
        let installed = self.installed_headroom_version()?;
        if installed == HEADROOM_PINNED_VERSION {
            return None;
        }
        Some(pinned_headroom_release().ok()?)
    }

    /// Returns true if the compiled requirements lock differs from what was
    /// used during the last headroom install.
    ///
    /// As a side effect, if the stored sha is a known legacy value whose
    /// pinned versions are byte-identical to the current lock, rewrites the
    /// receipt with the new-format sha and returns false. This avoids a
    /// purely cosmetic reinstall on the 0.2.50 → 0.3.0 jump.
    pub fn requirements_are_stale(&self) -> bool {
        let Some(stored) = self.installed_requirements_lock_sha() else {
            return true;
        };
        let current = requirements_lock_sha(bootstrap_requirements_lock());
        if stored == current {
            return false;
        }
        if LEGACY_REQUIREMENTS_LOCK_SHAS.contains(&stored.as_str()) {
            if let Err(err) = self.write_requirements_lock_sha_to_receipt(&current) {
                log::warn!("failed to migrate legacy requirementsLockSha256: {err}");
            }
            return false;
        }
        true
    }

    fn write_requirements_lock_sha_to_receipt(&self, sha: &str) -> Result<()> {
        let receipt_path = self.runtime.tools_dir.join("headroom.json");
        let bytes = std::fs::read(&receipt_path)
            .with_context(|| format!("reading {}", receipt_path.display()))?;
        let mut receipt: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", receipt_path.display()))?;
        if let Some(artifact) = receipt.get_mut("artifact").and_then(|a| a.as_object_mut()) {
            artifact.insert("requirementsLockSha256".into(), json!(sha));
        } else {
            return Ok(());
        }
        crate::client_adapters::atomic_write(&receipt_path, &serde_json::to_vec(&receipt)?)
            .with_context(|| format!("writing {}", receipt_path.display()))?;
        Ok(())
    }

    pub fn repair_stale_requirements_with_progress<F>(&self, mut progress: F) -> Result<()>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        let requirements_lock = bootstrap_requirements_lock();
        let lock_path = self.write_headroom_requirements_lock(requirements_lock)?;
        let dep_total = requirements_lock_package_count(requirements_lock);

        progress(BootstrapStepUpdate {
            step: "Repairing dependencies",
            message: "Repairing Headroom's bundled dependencies.".into(),
            eta_seconds: 60,
            percent: 40,
        });

        let deps_start = Instant::now();
        let progress_ref = std::cell::RefCell::new(&mut progress);
        let mut dep_counter: u32 = 0;
        run_pip_install_with_retries_streaming(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--find-links",
                VENDOR_WHEELS_INDEX_URL,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--upgrade",
                "--requirement",
                lock_path.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
            |line| {
                if let Some(update) = pip_line_to_progress(
                    line,
                    deps_start.elapsed(),
                    &mut dep_counter,
                    40,
                    82,
                    dep_total,
                ) {
                    if let Ok(mut cb) = progress_ref.try_borrow_mut() {
                        (cb)(BootstrapStepUpdate {
                            step: "Repairing dependencies",
                            message: update.message,
                            eta_seconds: update.eta_seconds,
                            percent: update.percent,
                        });
                    }
                }
            },
        )
        .context("repairing stale headroom requirements")?;

        progress(BootstrapStepUpdate {
            step: "Configuring integrations",
            message: "Setting up Headroom MCP integration.".into(),
            eta_seconds: 5,
            percent: 88,
        });

        let mcp_install = match self.install_headroom_mcp() {
            Ok(method) => json!({
                "configured": true,
                "proxyUrl": HEADROOM_PROXY_URL,
                "installMethod": method.as_str(),
            }),
            Err(err) => {
                log::info!("headroom MCP setup skipped during repair: {err:#}");
                json!({ "configured": false, "proxyUrl": HEADROOM_PROXY_URL, "error": err.to_string() })
            }
        };

        self.update_headroom_receipt_after_requirements_repair(
            requirements_lock_sha(requirements_lock),
            mcp_install,
        )?;

        progress(BootstrapStepUpdate {
            step: "Repair complete",
            message: "Headroom dependency repair finished.".into(),
            eta_seconds: 0,
            percent: 95,
        });

        Ok(())
    }

    pub fn bootstrap_all_with_progress<F>(&self, progress: F) -> Result<ManagedRuntime>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        // Persist every milestone to the attempt marker. If this attempt dies
        // without a verdict (quit, crash, kill), the marker survives and the
        // next launch reports it as bootstrap_abandoned with the phase it
        // died in.
        let mut caller_progress = progress;
        let mut progress = |update: BootstrapStepUpdate| {
            self.note_bootstrap_attempt(update.step, update.percent);
            caller_progress(update);
        };
        // Milestone logs mirror the UI progress steps into the file log. The
        // bootstrap is otherwise near-silent on disk (only the codesign line),
        // so a stuck/slow first install is hard to diagnose from logs alone.
        log::info!("bootstrap: starting managed runtime install");
        progress(BootstrapStepUpdate {
            step: "Preparing install",
            message: "Setting up managed directories.".into(),
            eta_seconds: 3,
            percent: 5,
        });
        self.runtime.ensure_layout()?;

        if !self.runtime.standalone_runtime_intact() {
            log::info!("bootstrap: downloading standalone Python runtime");
            progress(BootstrapStepUpdate {
                step: "Downloading Python",
                message: "Fetching pinned standalone Python runtime.".into(),
                eta_seconds: 75,
                percent: 18,
            });
            self.install_python_distribution(|update| progress(update))?;
        } else {
            progress(BootstrapStepUpdate {
                step: "Python runtime",
                message: "Pinned Python runtime already available locally.".into(),
                eta_seconds: 3,
                percent: 18,
            });
        }

        if !self.managed_venv_has_pip() {
            log::info!("bootstrap: creating managed virtualenv");
            progress(BootstrapStepUpdate {
                step: "Creating environment",
                message: "Creating isolated Headroom virtual environment.".into(),
                eta_seconds: 25,
                percent: 35,
            });
            self.create_managed_venv()?;
        } else {
            progress(BootstrapStepUpdate {
                step: "Environment",
                message: "Isolated runtime already present.".into(),
                eta_seconds: 3,
                percent: 35,
            });
        }

        log::info!("bootstrap: installing Headroom + dependencies via pip");
        progress(BootstrapStepUpdate {
            step: "Installing Headroom",
            message: "Installing Headroom and required dependencies.".into(),
            eta_seconds: 95,
            percent: 58,
        });
        // Forward pip's per-package chatter instead of dropping it. Without
        // this the whole multi-minute dependency install sits on the single
        // static frame above, which reads as a hang once its ETA lapses.
        // Clamped so the sub-step percents (which start at 40 for the
        // standalone upgrade path) never walk the bar backwards from 58.
        self.install_headroom(|update| {
            progress(BootstrapStepUpdate {
                percent: update.percent.clamp(58, 89),
                ..update
            })
        })?;

        // RTK is opt-in: bootstrap no longer installs it. Users add it from the
        // Addons tab, which calls install_addon("rtk").
        progress(BootstrapStepUpdate {
            step: "Finalizing",
            message: "Writing managed runtime receipts and completion markers.".into(),
            eta_seconds: 6,
            percent: 90,
        });
        // A bare Windows box lacks the MSVC redistributable torch/onnxruntime
        // need (RUST-7W/8V/8W). Non-fatal: on failure the box keeps today's
        // behavior and the launch-path ensure retries next boot.
        if let Err(err) = self.ensure_msvc_runtime_dlls() {
            log::warn!("MSVC runtime DLL vendoring failed during bootstrap: {err:#}");
        }

        self.clear_bootstrap_attempt();
        self.write_ready_flag()?;
        self.write_bootstrap_receipt()?;
        log::info!("bootstrap: managed runtime install complete (ready flag written)");
        progress(BootstrapStepUpdate {
            step: "Install complete",
            message: "Headroom runtime installed successfully.".into(),
            eta_seconds: 0,
            percent: 100,
        });
        Ok(self.runtime.clone())
    }

    /// Download-only warm-up for the consented bootstrap: pulls the two big
    /// network artifacts (standalone Python tarball, pinned headroom wheel)
    /// into `downloads_dir` while the user is still in signup/client-setup.
    /// Nothing is extracted or installed and nothing outside `downloads_dir`
    /// changes — `python_runtime_installed()` and every other installed-gate
    /// stays false, so consent semantics remain with bootstrap. Destinations
    /// and sha checks are identical to bootstrap's own download calls, which
    /// therefore skip instantly when they find these files.
    /// ponytail: pip's dependency downloads (the ~60-90s "Updating
    /// dependencies" step) can't be prefetched without extracting Python
    /// first — revisit only if the remaining install time still hurts.
    pub fn prefetch_bootstrap_artifacts(&self) -> Result<()> {
        self.runtime.ensure_layout()?;
        if !self.runtime.standalone_python().exists() {
            let artifact = python_distribution_artifact()?;
            let archive_path = self.runtime.downloads_dir.join("python-standalone.tar.gz");
            download_to_path(&artifact.url, &archive_path, artifact.sha256)?;
        }
        if !self.runtime.managed_python().exists() {
            let release = pinned_headroom_release()?;
            download_to_path(
                &release.wheel_url,
                &self.wheel_download_path(&release.wheel_url),
                Some(&release.sha256),
            )?;
        }
        Ok(())
    }

    /// Shared by the prefetch and the install/upgrade paths so a prefetched
    /// wheel always lands exactly where the installer looks for it. Keeps
    /// PyPI's own filename (platform tags and all) so pip's "not a supported
    /// wheel on this platform" check still backstops a mis-picked wheel — a
    /// `py3-none-any` rename made pip install a macOS wheel on Windows.
    /// Vendor the MSVC runtime DLLs into the managed runtime on Windows boxes
    /// that lack the system-wide redistributable (RUST-7W/8V/8W). Both
    /// python.exe locations get the DLLs -- the venv `Scripts/` stub and the
    /// standalone interpreter it execs -- so the application-directory DLL
    /// search succeeds whichever one is the process. No-op when System32
    /// already has the redist (the common case) or the DLLs are in place.
    pub fn ensure_msvc_runtime_dlls(&self) -> Result<bool> {
        if !cfg!(target_os = "windows") {
            return Ok(false);
        }
        let system32 =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()))
                .join("System32");
        if system32.join("msvcp140.dll").exists() && system32.join("vcruntime140_1.dll").exists() {
            return Ok(false);
        }
        let targets = [
            self.runtime.venv_dir.join(bin_subdir()),
            self.runtime.python_dir.clone(),
        ];
        // Presence probe on the three DLLs torch/onnxruntime actually import;
        // extraction still lands the wheel's full Scripts set.
        const CORE_DLLS: [&str; 3] = ["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"];
        if targets
            .iter()
            .all(|dir| CORE_DLLS.iter().all(|dll| dir.join(dll).exists()))
        {
            return Ok(false);
        }
        let wheel_path = self.wheel_download_path(MSVC_RUNTIME_WHEEL_URL);
        download_to_path(
            MSVC_RUNTIME_WHEEL_URL,
            &wheel_path,
            Some(MSVC_RUNTIME_WHEEL_SHA256),
        )?;
        let target_refs: Vec<&Path> = targets.iter().map(PathBuf::as_path).collect();
        let count = extract_msvc_runtime_dlls(&wheel_path, &target_refs)?;
        log::info!(
            "vendored {count} MSVC runtime DLLs into the managed runtime \
             (system redistributable missing)"
        );
        Ok(true)
    }

    fn wheel_download_path(&self, wheel_url: &str) -> PathBuf {
        self.runtime.downloads_dir.join(
            wheel_url
                .rsplit('/')
                .next()
                .filter(|name| name.ends_with(".whl"))
                .unwrap_or("headroom_ai.whl"),
        )
    }

    /// Run a directory operation that Windows can transiently deny.
    ///
    /// `DeleteFile`/`RemoveDirectory` only MARK a name for deletion: it stays
    /// on disk until the last handle closes, and a rename onto it meanwhile
    /// returns ACCESS_DENIED. Defender opens every file we just unpacked for
    /// the same second or two. RUST-8K is that race hitting `rename` on a
    /// fresh 0.8.6 install (os error 5, localized so the text is not even
    /// greppable) -- and it dead-ends the bootstrap, so the user never gets a
    /// runtime at all.
    ///
    /// Retries every error, not a classified subset: on a genuinely broken
    /// install this costs ~2.5s on a path that already failed, and the alt is
    /// a per-platform errno table (5/32/33 mean something else entirely on
    /// Unix) guarding a branch no test on a Mac can reach.
    /// ponytail: linear backoff, no jitter -- one process, no contention.
    fn retry_fs<T>(what: &str, mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
        const ATTEMPTS: u32 = 5;
        let mut attempt = 1;
        loop {
            match op() {
                Ok(value) => return Ok(value),
                Err(err) if attempt < ATTEMPTS => {
                    // info, not warn: a retry that then succeeds is a
                    // non-event, and warn bridges to Sentry. The caller
                    // reports the final failure as bootstrap_failed.
                    log::info!("{what} failed ({err}), retry {attempt}/{ATTEMPTS}");
                    std::thread::sleep(std::time::Duration::from_millis(250 * u64::from(attempt)));
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn install_python_distribution<F>(&self, mut emit_step: F) -> Result<()>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        let archive_path = self.runtime.downloads_dir.join("python-standalone.tar.gz");
        let artifact = python_distribution_artifact()?;
        // Sub-progress maps the download to bootstrap percents 18..=34 (next
        // step starts at 35). Keeps the progress bar moving on slow networks
        // so users don't assume the app has frozen.
        let started_at = Instant::now();
        download_to_path_with_progress(
            &artifact.url,
            &archive_path,
            artifact.sha256,
            |downloaded, total| {
                let downloaded_mb = downloaded as f64 / 1_048_576.0;
                let (message, percent, eta_seconds) = match total {
                    Some(total) if total > 0 => {
                        let total_mb = total as f64 / 1_048_576.0;
                        let frac = (downloaded as f64 / total as f64).clamp(0.0, 1.0);
                        let percent = (18.0 + frac * 16.0).round().clamp(18.0, 34.0) as u8;
                        let elapsed = started_at.elapsed().as_secs_f64().max(0.1);
                        let rate = downloaded as f64 / elapsed;
                        let remaining = (total.saturating_sub(downloaded)) as f64;
                        let eta = if rate > 1.0 {
                            (remaining / rate).ceil() as u64
                        } else {
                            75
                        };
                        (
                            format!(
                                "Downloading Python runtime: {:.1} / {:.1} MB",
                                downloaded_mb, total_mb
                            ),
                            percent,
                            eta,
                        )
                    }
                    _ => (
                        format!("Downloading Python runtime: {:.1} MB", downloaded_mb),
                        18,
                        75,
                    ),
                };
                emit_step(BootstrapStepUpdate {
                    step: "Downloading Python",
                    message,
                    eta_seconds,
                    percent,
                });
            },
        )?;

        let file = std::fs::File::open(&archive_path)
            .with_context(|| format!("opening {}", archive_path.display()))?;
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);
        // Extract to a staging dir and rename into place. Unpacking straight
        // into runtime_dir used to be a permanent trap: a crash mid-unpack
        // after bin/python3 landed made every later launch's `exists()` gate
        // read "installed" while the stdlib was incomplete — and no repair
        // path covers runtime/python, so it stayed broken until manual
        // deletion.
        let staging_dir = self.runtime.runtime_dir.join("python.extracting");
        if staging_dir.exists() {
            Self::retry_fs("clearing stale staging dir", || {
                std::fs::remove_dir_all(&staging_dir)
            })
            .with_context(|| format!("clearing stale {}", staging_dir.display()))?;
        }
        std::fs::create_dir_all(&staging_dir)
            .with_context(|| format!("creating {}", staging_dir.display()))?;
        archive
            .unpack(&staging_dir)
            .with_context(|| format!("extracting into {}", staging_dir.display()))?;

        // The tarball's single root is `python/`. On Windows the interpreter is
        // `python.exe` at the root; on Unix it is `bin/python3`.
        let extracted_root = staging_dir.join("python");
        let expected_python = if cfg!(target_os = "windows") {
            extracted_root.join("python.exe")
        } else {
            extracted_root.join("bin").join("python3")
        };
        if !expected_python.exists() {
            bail!(
                "standalone python extraction completed but {} was not found",
                expected_python.display()
            );
        }
        if self.runtime.python_dir.exists() {
            Self::retry_fs("removing partial python dir", || {
                std::fs::remove_dir_all(&self.runtime.python_dir)
            })
            .with_context(|| format!("removing partial {}", self.runtime.python_dir.display()))?;
        }
        Self::retry_fs("publishing extracted python", || {
            std::fs::rename(&extracted_root, &self.runtime.python_dir)
        })
        .with_context(|| {
            format!(
                "publishing extracted python into {}",
                self.runtime.python_dir.display()
            )
        })?;
        let _ = std::fs::remove_dir_all(&staging_dir);

        if !self.runtime.standalone_python().exists() {
            bail!(
                "standalone python extraction completed but {} was not found",
                self.runtime.standalone_python().display()
            );
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            let python = self.runtime.standalone_python();
            if let Ok(metadata) = std::fs::metadata(&python) {
                let mut perms = metadata.permissions();
                if perms.mode() & 0o111 == 0 {
                    perms.set_mode(0o755);
                    let _ = std::fs::set_permissions(&python, perms);
                }
            }
        }

        // Strip the quarantine attribute from the extracted runtime so macOS
        // Gatekeeper doesn't scan it on first execution (which can hang the
        // machine for 20-30 seconds).
        #[cfg(target_os = "macos")]
        {
            let _ = crate::proc::command("xattr")
                .args([
                    "-rd",
                    "com.apple.quarantine",
                    self.runtime.runtime_dir.to_string_lossy().as_ref(),
                ])
                .output();
        }

        Ok(())
    }

    /// `python -m venv` copies the interpreter into place *before* it runs
    /// ensurepip, so an attempt killed mid-way (quit, reboot, AV quarantine)
    /// leaves a `python` with no pip. Gating bootstrap on the interpreter alone
    /// made that state permanent: `create_managed_venv` — which carries the pip
    /// verification — was skipped on every later launch and the install died
    /// with "No module named pip" forever. Re-running venv creation re-runs
    /// ensurepip, so treating a pip-less venv as absent self-heals it.
    fn managed_venv_has_pip(&self) -> bool {
        self.runtime.managed_python().exists()
            && run_python_command(
                &self.runtime.managed_python(),
                &["-m", "pip", "--version"],
                &self.runtime.root_dir,
            )
            .is_ok()
    }

    /// Creates the venv, then installs pip into it as a separate step.
    ///
    /// `python -m venv` runs ensurepip through `subprocess.check_output` and
    /// re-raises only the exit status, so the child's own stderr is captured
    /// into an exception venv never prints. A failure there reached us as the
    /// bare string "Command '[... -m ensurepip ...]' returned non-zero exit
    /// status 1" -- no cause, no keyword for `pip_failure_category` to match,
    /// so it landed in the `other` grab-bag (Sentry RUST-82, an install-blocking
    /// dead end on 0.8.4 with nothing in it to act on).
    ///
    /// Running the two steps ourselves is what venv does internally, but now
    /// ensurepip's stderr comes back as a first-class `CommandFailure` that the
    /// categoriser can bucket. `build_command` already clears
    /// PYTHONHOME/PYTHONPATH and sets PYTHONNOUSERSITE, which is the isolation
    /// venv applies to this call, so the explicit invocation is equivalent.
    ///
    /// A venv left pip-less by a failure here is not a dead end: it fails
    /// `managed_venv_has_pip`, so the next launch treats it as absent and
    /// re-runs this whole function.
    fn create_managed_venv(&self) -> Result<()> {
        run_python_command(
            &self.runtime.standalone_python(),
            &[
                "-m",
                "venv",
                "--without-pip",
                self.runtime.venv_dir.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
        )
        .context("creating Headroom-managed virtualenv")?;

        run_python_command(
            &self.runtime.managed_python(),
            &["-m", "ensurepip", "--upgrade", "--default-pip"],
            &self.runtime.root_dir,
        )
        .context("installing pip into the Headroom-managed virtualenv")?;

        run_python_command(
            &self.runtime.managed_python(),
            &["-m", "pip", "--version"],
            &self.runtime.root_dir,
        )
        .context("verifying Headroom-managed pip is available")?;

        Ok(())
    }

    /// Bootstrap path: installs the pinned headroom release.
    fn install_headroom<F>(&self, progress: F) -> Result<()>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        // Bootstrap path runs at first launch where there is no boot
        // validation yet — no caller will read the captured pip output, so
        // skip the buffer to avoid allocating it.
        self.install_headroom_release(&pinned_headroom_release()?, progress, None)
    }

    fn install_headroom_release<F>(
        &self,
        release: &HeadroomRelease,
        mut progress: F,
        pip_capture: Option<&std::cell::RefCell<PipOutputCapture>>,
    ) -> Result<()>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        let requirements_lock = bootstrap_requirements_lock();
        let lock_path = self.write_headroom_requirements_lock(requirements_lock)?;
        let dep_total = requirements_lock_package_count(requirements_lock);
        let wheel_path = self.wheel_download_path(&release.wheel_url);

        progress(BootstrapStepUpdate {
            step: "Downloading update",
            message: "Fetching Headroom update bundle.".into(),
            eta_seconds: 15,
            percent: 40,
        });

        // Try direct wheel download (with retries). If the transfer fails,
        // fall back to the PyPI index; a checksum mismatch fails hard instead
        // — the fallback has no hash verification, so downgrading on mismatch
        // would bypass the integrity check exactly when it fires.
        let use_wheel =
            match download_to_path(&release.wheel_url, &wheel_path, Some(&release.sha256)) {
                Ok(()) => true,
                Err(download_err) if is_checksum_mismatch(&download_err) => {
                    return Err(download_err.context(
                        "Headroom wheel failed checksum verification; refusing unverified fallback",
                    ));
                }
                Err(download_err) => {
                    report_wheel_download_fallback(&release.wheel_url, &download_err);
                    false
                }
            };

        progress(BootstrapStepUpdate {
            step: "Updating dependencies",
            message: "Updating Headroom's bundled dependencies.".into(),
            eta_seconds: 90,
            percent: 55,
        });

        // Stream pip's stdout/stderr and translate noteworthy lines into
        // user-facing step updates so the progress UI actually changes
        // during the ~60-90s dependency install instead of staring at a
        // single "Updating dependencies" frame. Also funnel each line into
        // the diagnostic capture so a later boot-validation failure can
        // forensic the pip run that produced the broken venv.
        let deps_start = std::time::Instant::now();
        let deps_progress_ref = std::cell::RefCell::new(&mut progress);
        let mut dep_counter: u32 = 0;
        run_pip_install_with_retries_streaming(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--find-links",
                VENDOR_WHEELS_INDEX_URL,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--upgrade",
                "--requirement",
                lock_path.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
            |line| {
                if let Some(cap) = pip_capture {
                    cap.borrow_mut().push(line);
                }
                if let Some(update) = pip_line_to_progress(
                    line,
                    deps_start.elapsed(),
                    &mut dep_counter,
                    55,
                    80,
                    dep_total,
                ) {
                    if let Ok(mut cb) = deps_progress_ref.try_borrow_mut() {
                        (cb)(update);
                    }
                }
            },
        )
        .context("installing locked Headroom dependencies into Headroom-managed virtualenv")?;

        progress(BootstrapStepUpdate {
            step: "Applying update",
            message: "Applying the Headroom update.".into(),
            eta_seconds: 15,
            percent: 80,
        });

        let headroom_spec = format!("headroom-ai=={}", release.version);
        let headroom_arg = if use_wheel {
            wheel_path.to_string_lossy().into_owned()
        } else {
            headroom_spec.clone()
        };
        run_pip_install_with_retries_streaming(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--no-deps",
                &headroom_arg,
            ],
            &self.runtime.root_dir,
            |line| {
                if let Some(cap) = pip_capture {
                    cap.borrow_mut().push(line);
                }
            },
        )
        .with_context(|| {
            if use_wheel {
                "installing verified Headroom wheel into Headroom-managed virtualenv".into()
            } else {
                format!("installing {headroom_spec} from PyPI into Headroom-managed virtualenv")
            }
        })?;

        // Ad-hoc sign every native extension pip just dropped into the venv.
        // PyPI wheels are unsigned; some EDR tooling stalls or blocks on
        // first-execution of unsigned binaries. Best-effort — failures are
        // logged and ignored, the smoke test downstream is the real gate.
        self.ad_hoc_sign_venv_natives();

        progress(BootstrapStepUpdate {
            step: "Configuring integrations",
            message: "Setting up Headroom MCP integration.".into(),
            eta_seconds: 5,
            percent: 90,
        });

        let mcp_install = match self.install_headroom_mcp() {
            Ok(method) => json!({
                "configured": true,
                "proxyUrl": HEADROOM_PROXY_URL,
                "installMethod": method.as_str(),
            }),
            Err(err) => {
                log::info!("headroom MCP setup skipped: {err:#}");
                json!({
                    "configured": false,
                    "proxyUrl": HEADROOM_PROXY_URL,
                    "error": err.to_string()
                })
            }
        };

        self.write_tool_receipt(
            "headroom",
            json!({
                "status": "healthy",
                "installedBy": "Headroom",
                "scope": "self-contained",
                "runtime": "python",
                "pythonExecutable": self.runtime.managed_python(),
                "pipExecutable": self.runtime.managed_pip(),
                "entrypoint": self.runtime.venv_dir.join("bin").join("headroom"),
                "source": self.manifests[0].source_url,
                "version": release.version,
                "artifact": {
                    "url": release.wheel_url,
                    "sha256": release.sha256,
                    "requirementsLockSha256": requirements_lock_sha(requirements_lock)
                },
                "mcp": mcp_install,
                "ml": {
                    "installed": true,
                    "engine": "kompress"
                }
            }),
        )
    }

    fn update_headroom_receipt_after_requirements_repair(
        &self,
        requirements_lock_sha256: String,
        mcp_install: Value,
    ) -> Result<()> {
        let receipt_path = self.runtime.tools_dir.join("headroom.json");
        if let Ok(bytes) = std::fs::read(&receipt_path) {
            if let Ok(mut receipt) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(artifact) = receipt.get_mut("artifact").and_then(|a| a.as_object_mut())
                {
                    artifact.insert(
                        "requirementsLockSha256".into(),
                        json!(requirements_lock_sha256),
                    );
                }
                receipt["mcp"] = mcp_install;
                crate::client_adapters::atomic_write(&receipt_path, &serde_json::to_vec(&receipt)?)
                    .with_context(|| format!("writing {}", receipt_path.display()))?;
            }
        }
        Ok(())
    }

    /// Cheap post-install sanity check: can the new venv import the top-level
    /// headroom package and its proxy entrypoint? Catches import errors, syntax
    /// errors, and missing transitive dependencies introduced by a new version
    /// before we try to actually boot the proxy.
    ///
    /// If the failure is a pydantic / pydantic-core skew (pip's `--upgrade -r
    /// lock` left the two out of sync), reinstall pydantic-core at the version
    /// pydantic asks for and retry the smoke test once. Mirrors the
    /// proxy-startup repair in `start_headroom_proxy_with_repair` so the same
    /// recoverable failure doesn't fail an in-flight upgrade and force a
    /// rollback.
    pub fn smoke_test_headroom(&self) -> Result<()> {
        match self.smoke_test_headroom_with_timeout(HEADROOM_SMOKE_TEST_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(err) => {
                let target = err
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<CommandFailure>())
                    .and_then(|f| extract_required_pydantic_core_version(&f.stderr));
                let Some(target) = target else {
                    return Err(err);
                };
                log::warn!(
                    "smoke test failed with pydantic-core/pydantic skew; \
                     reinstalling pydantic-core=={target} and retrying"
                );
                if let Err(repair_err) = self.repair_pydantic_core(&target) {
                    log::error!("pydantic-core repair failed: {repair_err:#}");
                    return Err(err);
                }
                self.smoke_test_headroom_with_timeout(HEADROOM_SMOKE_TEST_TIMEOUT)
            }
        }
    }

    fn smoke_test_headroom_with_timeout(&self, timeout: Duration) -> Result<()> {
        let python = self.runtime.managed_python();
        if let Err(err) = run_command_with_timeout(
            &python,
            &["-c", "import headroom; import headroom.proxy.server"],
            &self.runtime.root_dir,
            timeout,
        )
        .with_context(|| format!("running smoke test with {}", python.display()))
        {
            return Err(anyhow::Error::new(CommandFailure {
                program: python.display().to_string(),
                args: vec![
                    "-c".into(),
                    "import headroom; import headroom.proxy.server".into(),
                ],
                stdout: err
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<CommandFailure>())
                    .map(|failure| failure.stdout.clone())
                    .unwrap_or_default(),
                stderr: err
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<CommandFailure>())
                    .map(|failure| failure.stderr.clone())
                    .unwrap_or_else(|| format!("{err:#}")),
                exit_code: err
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<CommandFailure>())
                    .and_then(|failure| failure.exit_code),
                signal: err
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<CommandFailure>())
                    .and_then(|failure| failure.signal),
            }))
            .context("Headroom smoke test failed — the new version cannot be imported");
        }
        Ok(())
    }

    /// Verifies the managed `markitdown` console script actually executes (its
    /// base converters and their dependencies import). No-op when the addon
    /// isn't installed, so it can be called unconditionally from a smoke pass.
    pub fn smoke_test_markitdown(&self) -> Result<()> {
        // First execution after an upgrade pays Gatekeeper/EDR scanning of the
        // freshly-written venv plus cold imports, which can blow the 60s
        // timeout (RUST-22). A retry runs with warm caches; only a repeat
        // failure is a real signal.
        if self
            .smoke_test_markitdown_with_timeout(MARKITDOWN_SMOKE_TEST_TIMEOUT)
            .is_ok()
        {
            return Ok(());
        }
        self.smoke_test_markitdown_with_timeout(MARKITDOWN_SMOKE_TEST_TIMEOUT)
    }

    fn smoke_test_markitdown_with_timeout(&self, timeout: Duration) -> Result<()> {
        if !self.markitdown_installed() {
            return Ok(());
        }
        let bin = self.markitdown_entrypoint();
        run_command_with_timeout(&bin, &["--help"], &self.runtime.root_dir, timeout)
            .with_context(|| format!("running markitdown smoke test with {}", bin.display()))?;
        Ok(())
    }

    /// Plugin addons are host plugins, not binaries we own, so "smoke test"
    /// means confirming the plugin is still registered with a host's plugin
    /// registry. No-op when our receipt says it was never installed, or when
    /// it says the user disabled it — hosts without a disable verb (Codex)
    /// drop the registration entirely on disable, so absence is expected
    /// there, not a failure (RUST-22 false positive).
    pub fn smoke_test_plugin(&self, id: &str) -> Result<()> {
        let plugin = plugin_addon(id).with_context(|| format!("unknown plugin addon: {id}"))?;
        let Some(receipt) = self.read_tool_receipt(plugin.id) else {
            return Ok(());
        };
        let enabled = receipt
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            return Ok(());
        }
        if !PluginHost::ALL
            .iter()
            .any(|host| host.plugin_present(plugin))
        {
            // The plugin was removed behind our back (host-native `/plugin`
            // uninstall or a host registry migration). Drop the stale receipt
            // so this warns once instead of on every future upgrade;
            // `detect_status` already reports NotInstalled for this state.
            let receipt_path = self.runtime.tools_dir.join(format!("{}.json", plugin.id));
            let removed = std::fs::remove_file(&receipt_path).is_ok();
            bail!(
                "{id} receipt exists but the plugin is no longer registered \
                 with any host{}",
                if removed {
                    " (stale receipt removed)"
                } else {
                    " (stale receipt could not be removed)"
                }
            );
        }
        Ok(())
    }

    /// Apply an ad-hoc codesign signature to every native extension (.so /
    /// .dylib) under the venv's site-packages. PyPI wheels arrive unsigned,
    /// and some endpoint protection (EDR) tooling either blocks unsigned
    /// freshly-extracted binaries outright or makes them slower to load on
    /// first execution. An ad-hoc signature (`codesign --force --sign -`)
    /// satisfies macOS Gatekeeper's "signed" check and clears at least one
    /// class of EDR heuristic without us shipping a Developer ID at runtime.
    ///
    /// Best-effort: failures are logged and ignored. The install must not
    /// fail because codesign couldn't sign one file — the smoke test that
    /// follows is the real gate.
    fn ad_hoc_sign_venv_natives(&self) -> usize {
        if !cfg!(target_os = "macos") {
            return 0;
        }
        let site_packages = self
            .runtime
            .venv_dir
            .join("lib")
            .join("python3.12")
            .join("site-packages");
        if !site_packages.exists() {
            return 0;
        }
        let mut native_paths: Vec<PathBuf> = Vec::new();
        if let Err(err) = collect_native_extensions(&site_packages, &mut native_paths) {
            log::warn!(
                "ad-hoc codesign skipped: failed to walk {}: {err:#}",
                site_packages.display()
            );
            return 0;
        }
        if native_paths.is_empty() {
            return 0;
        }
        let total = native_paths.len();
        // One codesign invocation can accept many file arguments; ARG_MAX
        // (~256KB on macOS) is well above what we'd hit even with 1000+
        // long paths, so we avoid the per-file fork-exec overhead.
        let output = crate::proc::command("codesign")
            .args(["--force", "--sign", "-"])
            .args(&native_paths)
            .output();
        match output {
            Ok(out) if out.status.success() => {
                log::info!("ad-hoc codesign signed {total} venv native extensions");
                total
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                log::warn!(
                    "ad-hoc codesign exited {:?} for {total} files: {}",
                    out.status.code(),
                    stderr.trim()
                );
                0
            }
            Err(err) => {
                log::warn!("ad-hoc codesign failed to spawn: {err:#}");
                0
            }
        }
    }

    fn venv_backup_dir(&self) -> PathBuf {
        let mut dir = self.runtime.venv_dir.clone();
        let file_name = format!(
            "{}.backup",
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("venv")
        );
        dir.set_file_name(file_name);
        dir
    }

    fn headroom_receipt_path(&self) -> PathBuf {
        self.runtime.tools_dir.join("headroom.json")
    }

    fn headroom_receipt_backup_path(&self) -> PathBuf {
        self.runtime.tools_dir.join("headroom.json.backup")
    }

    fn upgrade_marker_path(&self) -> PathBuf {
        self.runtime.runtime_dir.join("upgrade.in_progress.json")
    }

    fn write_upgrade_marker(
        &self,
        target_version: &str,
        in_place_previous_version: Option<&str>,
        in_place_previous_lock_backup: Option<&Path>,
    ) -> Result<()> {
        let marker = self.upgrade_marker_path();
        let mut body = json!({
            "target_version": target_version,
            "started_at": Utc::now().to_rfc3339(),
        });
        if let Some(previous) = in_place_previous_version {
            body["in_place"] = json!(true);
            body["previous_version"] = json!(previous);
        }
        if let Some(backup) = in_place_previous_lock_backup {
            body["previous_lock_backup"] = json!(backup);
        }
        crate::client_adapters::atomic_write(&marker, &serde_json::to_vec_pretty(&body)?)
            .with_context(|| format!("writing {}", marker.display()))?;
        Ok(())
    }

    /// Read the in-progress upgrade marker and, if it records an in-place
    /// upgrade, return (previous_version, target_version, previous_lock_backup).
    /// Returns None for missing markers and for full-venv-rebuild markers.
    fn read_in_place_marker(&self) -> Option<(String, String, Option<PathBuf>)> {
        let bytes = std::fs::read(self.upgrade_marker_path()).ok()?;
        let body: Value = serde_json::from_slice(&bytes).ok()?;
        if body.get("in_place").and_then(|v| v.as_bool()) != Some(true) {
            return None;
        }
        let previous = body.get("previous_version")?.as_str()?.to_string();
        let target = body.get("target_version")?.as_str()?.to_string();
        let lock_backup = body
            .get("previous_lock_backup")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        Some((previous, target, lock_backup))
    }

    fn clear_upgrade_marker(&self) {
        let _ = std::fs::remove_file(self.upgrade_marker_path());
    }

    /// Inspect disk state for the signature of an interrupted previous upgrade
    /// and restore the backup venv as the live venv if so.
    ///
    /// Interrupted = upgrade marker file present. The backup venv is treated
    /// as the canonical "old, working" one; the live venv (if any) is whatever
    /// partial state was left behind. Safe to call at every upgrade entry.
    ///
    /// Returns true if recovery was performed.
    pub fn recover_from_interrupted_upgrade(&self) -> bool {
        let marker = self.upgrade_marker_path();
        if !marker.exists() {
            return false;
        }

        // In-place recovery: pip install is atomic per-package, so the venv
        // may be in a mixed state (some packages at target pins, others at
        // previous pins). Restore deps from the lock snapshot (if the
        // interrupted upgrade took that path) and force-reinstall the prior
        // headroom-ai so the next launch starts from a known-good state.
        // `check_headroom_upgrade` will then retry the swap fresh.
        if let Some((previous_version, _target, previous_lock_backup)) = self.read_in_place_marker()
        {
            log::info!(
                "recover_from_interrupted_upgrade: in-place upgrade was in progress; \
                 reinstalling previous headroom-ai {previous_version}"
            );
            // Both pip helpers need PyPI. Offline (laptop died mid-upgrade,
            // reopened on a plane) they fail — and discarding those failures
            // used to clear the marker anyway, leaving a mixed venv (new dep
            // pins, unknown headroom-ai version) that the restored receipt
            // declared healthy. On failure keep the marker AND the lock
            // backup untouched so the next launch retries recovery; mirror
            // `rollback_headroom_upgrade`, which propagates the same errors.
            if let Some(ref backup) = previous_lock_backup {
                if let Err(err) = self.pip_restore_deps_from_backup(backup) {
                    log::warn!(
                        "recover_from_interrupted_upgrade: dep restore failed ({err:#}); \
                         keeping upgrade marker for retry"
                    );
                    return false;
                }
            }
            if let Err(err) = self.pip_force_reinstall_headroom_version(&previous_version) {
                log::warn!(
                    "recover_from_interrupted_upgrade: reinstalling headroom-ai \
                     {previous_version} failed ({err:#}); keeping upgrade marker for retry"
                );
                return false;
            }
            if let Some(ref backup) = previous_lock_backup {
                let _ = std::fs::copy(backup, self.active_lock_path());
                let _ = std::fs::remove_file(backup);
            }
            let receipt_backup = self.headroom_receipt_backup_path();
            let receipt_path = self.headroom_receipt_path();
            if receipt_backup.exists() {
                let _ = std::fs::copy(&receipt_backup, &receipt_path);
                let _ = std::fs::remove_file(&receipt_backup);
            }
            self.clear_upgrade_marker();
            return true;
        }

        let backup_dir = self.venv_backup_dir();
        let venv_dir = &self.runtime.venv_dir;
        let receipt_backup = self.headroom_receipt_backup_path();
        let receipt_path = self.headroom_receipt_path();

        log::info!(
            "recover_from_interrupted_upgrade: found stale marker at {}; restoring backup",
            marker.display()
        );

        if backup_dir.exists() {
            // The live venv (if present) is a partial/unknown new install.
            // Blow it away and put the backup back in its place.
            if venv_dir.exists() {
                if let Err(err) = std::fs::remove_dir_all(venv_dir) {
                    log::error!(
                        "recover_from_interrupted_upgrade: failed to remove partial venv at {}: {err}",
                        venv_dir.display()
                    );
                    // Leave everything in place; clearing the marker would be
                    // worse than leaving it for a later manual intervention.
                    return false;
                }
            }
            if let Err(err) = std::fs::rename(&backup_dir, venv_dir) {
                log::error!(
                    "recover_from_interrupted_upgrade: failed to restore venv from {}: {err}",
                    backup_dir.display()
                );
                return false;
            }
            if receipt_backup.exists() {
                let _ = std::fs::copy(&receipt_backup, &receipt_path);
                let _ = std::fs::remove_file(&receipt_backup);
            }
        } else {
            // No backup to restore from. Rare — the user (or a script) deleted
            // the backup dir while the marker was still live. Best we can do
            // is clear the marker so we don't loop on this state.
            log::warn!(
                "recover_from_interrupted_upgrade: no backup at {}; clearing marker",
                backup_dir.display()
            );
        }
        self.clear_upgrade_marker();
        true
    }

    /// Atomic runtime upgrade. Moves the current venv aside, creates a fresh
    /// venv at the original path, installs the new release, runs a smoke test.
    ///
    /// On success: returns `InstalledPendingValidation` — the backup is **still
    /// on disk** and the caller must call either [`commit_headroom_upgrade`] (if
    /// the new proxy boots) or [`rollback_headroom_upgrade`] (if it doesn't).
    ///
    /// On failure in any install step: rolls back internally, restoring the
    /// previous venv + receipt byte-for-byte, and returns `InstallFailed`.
    ///
    /// `force_rebuild` skips the in-place upgrade attempt and goes straight
    /// to the move-aside-and-rebuild path. Used by the user-facing "Retry
    /// with full rebuild" recovery flow when an in-place upgrade installed
    /// cleanly but boot validation failed (typically an ABI mismatch in
    /// native deps that pip can't detect).
    pub fn atomic_upgrade_headroom<F>(
        &self,
        release: &HeadroomRelease,
        mut progress: F,
        force_rebuild: bool,
    ) -> UpgradeOutcome
    where
        F: FnMut(BootstrapStepUpdate),
    {
        progress(BootstrapStepUpdate {
            step: "Preparing update",
            message: "Checking for previous upgrade state.".into(),
            eta_seconds: 2,
            percent: 5,
        });

        // Windows: anything still running from the venv (IDE-spawned MCP
        // servers, stray pythons) holds file locks that fail both the wheel
        // install and its rollback with permission errors (RUST-6Z/70).
        crate::state::kill_venv_lock_holders(&self.runtime.venv_dir);

        // If a prior upgrade was interrupted (process killed between
        // move-aside and success-commit), the backup is the REAL venv.
        // Restore it before doing anything destructive.
        let _recovered = self.recover_from_interrupted_upgrade();

        // In-place path: mutate the live venv rather than rebuilding it.
        // Covers both the wheel-only case (lock unchanged) and the lock-churn
        // case (`pip install --upgrade -r lock` reinstalls only the pins that
        // actually differ). Skipped when `force_rebuild` is set (user-
        // initiated recovery from a botched in-place upgrade) or when
        // `prepare_in_place_upgrade` decides the receipt isn't safe to
        // mutate in-place.
        if !force_rebuild {
            if let Some(ctx) = self.prepare_in_place_upgrade() {
                return self.in_place_upgrade_headroom(release, ctx, progress);
            }
        }

        let venv_dir = self.runtime.venv_dir.clone();
        let backup_dir = self.venv_backup_dir();
        let receipt_path = self.headroom_receipt_path();
        let receipt_backup = self.headroom_receipt_backup_path();

        // Best-effort: purge any leftover backup from a cleanly-completed
        // previous upgrade. recover_from_interrupted_upgrade above has
        // already handled any backup that belongs to an in-flight upgrade.
        if backup_dir.exists() {
            if let Err(err) = std::fs::remove_dir_all(&backup_dir) {
                return UpgradeOutcome::InstallFailed {
                    restored: false,
                    error: anyhow!(
                        "failed to remove stale venv backup at {}: {err}",
                        backup_dir.display()
                    ),
                };
            }
        }
        let _ = std::fs::remove_file(&receipt_backup);

        // Disk-space pre-check: building a fresh venv doubles space usage
        // momentarily. Refuse if less than 1 GB is free on the root volume.
        if let Some(avail) = available_disk_bytes(&self.runtime.root_dir) {
            const ONE_GB: u64 = 1_024 * 1_024 * 1_024;
            if avail < ONE_GB {
                return UpgradeOutcome::InstallFailed {
                    restored: false,
                    error: anyhow!(
                        "insufficient disk space for runtime upgrade: {} MB free, 1024 MB required",
                        avail / (1024 * 1024)
                    ),
                };
            }
        }

        // Move current venv + receipt aside. Write the in-progress marker
        // FIRST so that if we're killed between the rename and
        // commit/rollback, the next launch can recognize and recover.
        let had_live_venv = venv_dir.exists();
        if had_live_venv {
            if let Err(err) = self.write_upgrade_marker(&release.version, None, None) {
                return UpgradeOutcome::InstallFailed {
                    restored: false,
                    error: err.context("writing upgrade-in-progress marker"),
                };
            }
            if let Err(err) = std::fs::rename(&venv_dir, &backup_dir) {
                self.clear_upgrade_marker();
                return UpgradeOutcome::InstallFailed {
                    restored: false,
                    error: anyhow!("failed to move {} aside: {err}", venv_dir.display()),
                };
            }
        }
        let had_receipt = receipt_path.exists();
        if had_receipt {
            if let Err(err) = std::fs::copy(&receipt_path, &receipt_backup) {
                let restored = self.restore_venv_from_backup(had_live_venv);
                return UpgradeOutcome::InstallFailed {
                    restored,
                    error: anyhow!("failed to snapshot {}: {err}", receipt_path.display()),
                };
            }
        }

        progress(BootstrapStepUpdate {
            step: "Creating environment",
            message: "Creating isolated Headroom virtual environment.".into(),
            eta_seconds: 20,
            percent: 15,
        });

        if let Err(err) = self.create_managed_venv() {
            let restored = self.rollback_partial_upgrade(had_live_venv, had_receipt);
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err.context("creating replacement Headroom virtualenv"),
            };
        }

        // install_headroom_release emits its own granular progress from ~40-90%.
        let pip_capture = std::cell::RefCell::new(PipOutputCapture::new(100));
        if let Err(err) = self.install_headroom_release(release, &mut progress, Some(&pip_capture))
        {
            let restored = self.rollback_partial_upgrade(had_live_venv, had_receipt);
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err,
            };
        }

        // The swap built a fresh venv, so re-vendor the MSVC runtime DLLs
        // before the smoke test runs against the final state (RUST-7W/8V/8W).
        // Non-fatal for the same reason as in bootstrap_all_with_progress.
        if let Err(err) = self.ensure_msvc_runtime_dlls() {
            log::warn!("MSVC runtime DLL vendoring failed during upgrade: {err:#}");
        }

        progress(BootstrapStepUpdate {
            step: "Verifying install",
            message: "Running Headroom import smoke test.".into(),
            eta_seconds: 3,
            percent: 95,
        });

        if let Err(err) = self.smoke_test_headroom() {
            let restored = self.rollback_partial_upgrade(had_live_venv, had_receipt);
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err,
            };
        }

        // Re-stamp the READY flag on the fresh venv. Without this,
        // `python_runtime_installed()` returns false (the flag lives inside
        // venv_dir, which was replaced during the swap), which would make
        // `ensure_headroom_running()` early-return without spawning the
        // new proxy — silently breaking boot validation.
        if let Err(err) = self.write_ready_flag() {
            let restored = self.rollback_partial_upgrade(had_live_venv, had_receipt);
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err.context("writing READY flag on upgraded venv"),
            };
        }

        progress(BootstrapStepUpdate {
            step: "Install complete",
            message: "Install finished. Verifying Headroom boot…".into(),
            eta_seconds: 0,
            percent: 97,
        });

        UpgradeOutcome::InstalledPendingValidation {
            pip_output_tail: pip_capture.into_inner().into_string(),
        }
    }

    /// Tear down the new venv and restore the previous one. Called by the
    /// `state.rs` upgrade coordinator when boot validation fails.
    /// Idempotent — no-op if no backup exists.
    pub fn rollback_headroom_upgrade(&self) -> Result<()> {
        // Windows: clear venv file locks first — this path pip-reinstalls
        // (in-place) or renames the venv (swap), and both fail on locked
        // files (RUST-6Z/70).
        crate::state::kill_venv_lock_holders(&self.runtime.venv_dir);
        // In-place rollback: no venv backup. Restore deps from the lock
        // snapshot (if the upgrade touched the lock), then pip-reinstall the
        // previous headroom-ai and restore the receipt.
        if let Some((previous_version, _target, previous_lock_backup)) = self.read_in_place_marker()
        {
            if let Some(ref backup) = previous_lock_backup {
                self.pip_restore_deps_from_backup(backup).with_context(|| {
                    format!(
                        "rollback failed — could not restore dependencies from {}",
                        backup.display()
                    )
                })?;
                let _ = std::fs::copy(backup, self.active_lock_path());
                let _ = std::fs::remove_file(backup);
            }
            self.pip_force_reinstall_headroom_version(&previous_version)
                .with_context(|| {
                    format!(
                        "rollback failed — could not reinstall previous Headroom version {previous_version}"
                    )
                })?;
            let receipt_backup = self.headroom_receipt_backup_path();
            let receipt_path = self.headroom_receipt_path();
            if receipt_backup.exists() {
                std::fs::copy(&receipt_backup, &receipt_path)
                    .with_context(|| format!("restoring {}", receipt_path.display()))?;
                let _ = std::fs::remove_file(&receipt_backup);
            }
            self.clear_upgrade_marker();
            return Ok(());
        }

        let backup_dir = self.venv_backup_dir();
        if !backup_dir.exists() {
            return Ok(());
        }
        let had_live_venv = true; // by definition, if we have a backup
        let had_receipt = self.headroom_receipt_backup_path().exists();
        let restored = self.rollback_partial_upgrade(had_live_venv, had_receipt);
        if !restored {
            bail!(
                "rollback failed — venv.backup is present but could not be restored to {}",
                self.runtime.venv_dir.display()
            );
        }
        Ok(())
    }

    /// Returns true if the bundled requirements lock's dep pins differ from
    /// what was installed (ignoring comment/whitespace churn). Conservative:
    /// if we can't determine the installed sha, assume it differs.
    fn lock_pins_differ_from_installed(&self) -> bool {
        let Some(stored) = self.installed_requirements_lock_sha() else {
            return true;
        };
        let current = requirements_lock_sha(bootstrap_requirements_lock());
        stored != current && !LEGACY_REQUIREMENTS_LOCK_SHAS.contains(&stored.as_str())
    }

    fn active_lock_path(&self) -> PathBuf {
        self.runtime
            .downloads_dir
            .join("headroom-requirements.lock")
    }

    fn lock_backup_path(&self) -> PathBuf {
        self.runtime
            .downloads_dir
            .join("headroom-requirements.lock.backup")
    }

    /// Prepare to upgrade the runtime in place (no venv rebuild). Returns
    /// `None` when the caller should fall back to the full atomic rebuild:
    /// either there is no prior install to upgrade, the previously-installed
    /// version is below `ATOMIC_REBUILD_FLOOR_VERSION` (in-place pip across
    /// that delta leaves stale native libs), or the lock churned but the
    /// active lock file is missing on disk so we can't safely snapshot for
    /// rollback.
    ///
    /// When `Some`, the caller owns `previous_lock_backup` (if set): on
    /// success, `commit_headroom_upgrade` deletes it; on failure, rollback
    /// uses it to restore the prior pin set.
    fn prepare_in_place_upgrade(&self) -> Option<InPlaceUpgradeContext> {
        let previous_version = self.installed_headroom_version()?;
        if receipt_requires_atomic_rebuild(&previous_version) {
            log::info!(
                "prepare_in_place_upgrade: receipt {} predates atomic-rebuild floor {:?}; \
                 forcing full venv rebuild",
                previous_version,
                ATOMIC_REBUILD_FLOOR_VERSION
            );
            return None;
        }
        let previous_lock_backup = if self.lock_pins_differ_from_installed() {
            let active = self.active_lock_path();
            if !active.exists() {
                return None;
            }
            let backup = self.lock_backup_path();
            let _ = std::fs::remove_file(&backup);
            std::fs::copy(&active, &backup).ok()?;
            Some(backup)
        } else {
            None
        };
        Some(InPlaceUpgradeContext {
            previous_version,
            previous_lock_backup,
        })
    }

    /// In-place upgrade: mutate the live venv rather than rebuilding it.
    /// When `ctx.previous_lock_backup` is set, runs
    /// `pip install --upgrade -r lock` first so only churned dep pins are
    /// reinstalled (pip skips packages already at the pinned version). Then
    /// force-reinstalls the new `headroom-ai` wheel.
    ///
    /// On any failure, attempts to restore the prior version and (if
    /// applicable) the prior lock via the same pip mechanism.
    fn in_place_upgrade_headroom<F>(
        &self,
        release: &HeadroomRelease,
        ctx: InPlaceUpgradeContext,
        mut progress: F,
    ) -> UpgradeOutcome
    where
        F: FnMut(BootstrapStepUpdate),
    {
        let receipt_path = self.headroom_receipt_path();
        let receipt_backup = self.headroom_receipt_backup_path();

        // Purge stale receipt backup from any cleanly-completed prior upgrade.
        let _ = std::fs::remove_file(&receipt_backup);

        // Snapshot the receipt so rollback can restore the old artifact pointers.
        if receipt_path.exists() {
            if let Err(err) = std::fs::copy(&receipt_path, &receipt_backup) {
                if let Some(ref p) = ctx.previous_lock_backup {
                    let _ = std::fs::remove_file(p);
                }
                return UpgradeOutcome::InstallFailed {
                    restored: true,
                    error: anyhow!("failed to snapshot {}: {err}", receipt_path.display()),
                };
            }
        }

        // Write marker so an interrupted upgrade can be recovered on next launch.
        if let Err(err) = self.write_upgrade_marker(
            &release.version,
            Some(&ctx.previous_version),
            ctx.previous_lock_backup.as_deref(),
        ) {
            let _ = std::fs::remove_file(&receipt_backup);
            if let Some(ref p) = ctx.previous_lock_backup {
                let _ = std::fs::remove_file(p);
            }
            return UpgradeOutcome::InstallFailed {
                restored: true,
                error: err.context("writing upgrade-in-progress marker"),
            };
        }

        // Bounded ring buffer collecting pip stdout/stderr across both
        // install steps. Attached to the boot-validation Sentry event when
        // it later fails — pip can return exit 0 while leaving the venv
        // broken (skipped packages, ABI-mismatched native deps), and the
        // tail is the only forensic record of what pip actually did.
        let pip_capture = std::cell::RefCell::new(PipOutputCapture::new(100));

        // Dep-lock upgrade (only when pins changed).
        if ctx.previous_lock_backup.is_some() {
            progress(BootstrapStepUpdate {
                step: "Updating dependencies",
                message: "Updating Headroom's bundled dependencies.".into(),
                eta_seconds: 45,
                percent: 15,
            });

            let requirements_lock = bootstrap_requirements_lock();
            let dep_total = requirements_lock_package_count(requirements_lock);
            let lock_path = match self.write_headroom_requirements_lock(requirements_lock) {
                Ok(p) => p,
                Err(err) => {
                    let restored = self.rollback_in_place_upgrade_inner(&ctx);
                    return UpgradeOutcome::InstallFailed {
                        restored,
                        error: err,
                    };
                }
            };

            let deps_start = std::time::Instant::now();
            let deps_progress_ref = std::cell::RefCell::new(&mut progress);
            let mut dep_counter: u32 = 0;
            if let Err(err) = run_pip_install_with_retries_streaming(
                &self.runtime.managed_python(),
                &[
                    "-m",
                    "pip",
                    "install",
                    "--timeout",
                    "180",
                    "--retries",
                    "10",
                    PIP_ONLY_BINARY,
                    "--find-links",
                    VENDOR_WHEELS_INDEX_URL,
                    "--extra-index-url",
                    "https://pypi.org/simple",
                    "--upgrade",
                    "--requirement",
                    lock_path.to_string_lossy().as_ref(),
                ],
                &self.runtime.root_dir,
                |line| {
                    pip_capture.borrow_mut().push(line);
                    if let Some(update) = pip_line_to_progress(
                        line,
                        deps_start.elapsed(),
                        &mut dep_counter,
                        15,
                        55,
                        dep_total,
                    ) {
                        if let Ok(mut cb) = deps_progress_ref.try_borrow_mut() {
                            (cb)(update);
                        }
                    }
                },
            ) {
                let restored = self.rollback_in_place_upgrade_inner(&ctx);
                return UpgradeOutcome::InstallFailed {
                    restored,
                    error: err.context("upgrading Headroom's bundled dependencies in place"),
                };
            }
        }

        progress(BootstrapStepUpdate {
            step: "Downloading update",
            message: "Fetching Headroom update bundle.".into(),
            eta_seconds: 10,
            percent: 60,
        });

        let wheel_path = self
            .runtime
            .downloads_dir
            .join(format!("headroom_ai-{}-py3-none-any.whl", release.version));
        let use_wheel =
            match download_to_path(&release.wheel_url, &wheel_path, Some(&release.sha256)) {
                Ok(()) => true,
                Err(download_err) if is_checksum_mismatch(&download_err) => {
                    // Never downgrade a failed integrity check to an
                    // unverified pip-index install.
                    let restored = self.rollback_in_place_upgrade_inner(&ctx);
                    return UpgradeOutcome::InstallFailed {
                    restored,
                    error: download_err.context(
                        "Headroom wheel failed checksum verification; refusing unverified fallback",
                    ),
                };
                }
                Err(download_err) => {
                    report_wheel_download_fallback(&release.wheel_url, &download_err);
                    false
                }
            };

        progress(BootstrapStepUpdate {
            step: "Applying update",
            message: "Installing the new Headroom wheel.".into(),
            eta_seconds: 10,
            percent: 75,
        });

        let headroom_spec = format!("headroom-ai=={}", release.version);
        let headroom_arg = if use_wheel {
            wheel_path.to_string_lossy().into_owned()
        } else {
            headroom_spec.clone()
        };
        if let Err(err) = run_pip_install_with_retries_streaming(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--no-deps",
                "--force-reinstall",
                &headroom_arg,
            ],
            &self.runtime.root_dir,
            |line| {
                pip_capture.borrow_mut().push(line);
            },
        ) {
            let restored = self.rollback_in_place_upgrade_inner(&ctx);
            let context_msg = if use_wheel {
                "installing verified Headroom wheel into Headroom-managed virtualenv"
            } else {
                "installing headroom-ai from PyPI into Headroom-managed virtualenv"
            };
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err.context(context_msg),
            };
        }

        // Ad-hoc sign every native extension pip just dropped in. Failures
        // are logged and ignored; the smoke test below is the real gate.
        self.ad_hoc_sign_venv_natives();

        progress(BootstrapStepUpdate {
            step: "Verifying install",
            message: "Running Headroom import smoke test.".into(),
            eta_seconds: 3,
            percent: 85,
        });

        if let Err(err) = self.smoke_test_headroom() {
            let restored = self.rollback_in_place_upgrade_inner(&ctx);
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err,
            };
        }

        // Optional addon: an in-place upgrade keeps the venv, so markitdown
        // should still run. Warn-only — a broken optional addon must not fail
        // the core Headroom upgrade.
        if let Err(err) = self.smoke_test_markitdown() {
            log::warn!("markitdown smoke test failed after upgrade: {err:#}");
        }
        for plugin in &PLUGIN_ADDONS {
            if let Err(err) = self.smoke_test_plugin(plugin.id) {
                log::warn!("{} smoke test failed after upgrade: {err:#}", plugin.id);
            }
        }

        progress(BootstrapStepUpdate {
            step: "Configuring integrations",
            message: "Setting up Headroom MCP integration.".into(),
            eta_seconds: 5,
            percent: 92,
        });

        let mcp_install = match self.install_headroom_mcp() {
            Ok(method) => json!({
                "configured": true,
                "proxyUrl": HEADROOM_PROXY_URL,
                "installMethod": method.as_str(),
            }),
            Err(err) => {
                log::info!("headroom MCP setup skipped: {err:#}");
                json!({
                    "configured": false,
                    "proxyUrl": HEADROOM_PROXY_URL,
                    "error": err.to_string(),
                })
            }
        };

        if let Err(err) = self.update_headroom_receipt_after_in_place_upgrade(release, mcp_install)
        {
            let restored = self.rollback_in_place_upgrade_inner(&ctx);
            return UpgradeOutcome::InstallFailed {
                restored,
                error: err,
            };
        }

        progress(BootstrapStepUpdate {
            step: "Install complete",
            message: "Install finished. Verifying Headroom boot…".into(),
            eta_seconds: 0,
            percent: 97,
        });

        UpgradeOutcome::InstalledPendingValidation {
            pip_output_tail: pip_capture.into_inner().into_string(),
        }
    }

    fn pip_force_reinstall_headroom_version(&self, version: &str) -> Result<()> {
        let spec = format!("headroom-ai=={version}");
        run_pip_install_with_retries(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--no-deps",
                "--force-reinstall",
                &spec,
            ],
            &self.runtime.root_dir,
        )
        .with_context(|| format!("reinstalling Headroom version {version}"))
    }

    /// Recover from a pydantic / pydantic-core version skew by reinstalling
    /// pydantic-core at the version pydantic wants. Triggered when the proxy
    /// log shows the SystemError pydantic raises during import. `--no-deps`
    /// keeps the rest of the venv untouched.
    fn repair_pydantic_core(&self, target_version: &str) -> Result<()> {
        // Reinstall pydantic itself first (no version pin) to rewrite its
        // dist-info. A failed prior upgrade can leave two `pydantic-X.Y.dist-info`
        // dirs in site-packages; `importlib.metadata.metadata('pydantic')` then
        // returns either one non-deterministically, producing flip-flopping
        // "requires N.N.N" errors across attempts. Force-reinstalling pydantic
        // collapses the duplicates so the next pin we apply actually matches
        // what pydantic asks for.
        run_pip_install_with_retries(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--no-deps",
                "--force-reinstall",
                "pydantic",
            ],
            &self.runtime.root_dir,
        )
        .with_context(|| "reinstalling pydantic to clear duplicate dist-info")?;

        let spec = format!("pydantic-core=={target_version}");
        run_pip_install_with_retries(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--no-deps",
                "--force-reinstall",
                &spec,
            ],
            &self.runtime.root_dir,
        )
        .with_context(|| format!("reinstalling pydantic-core=={target_version}"))
    }

    /// Restore deps from `previous_lock_backup` via
    /// `pip install --upgrade -r <backup>` — packages already at the old pin
    /// are skipped by pip, only packages that were actually churned by the
    /// failed upgrade get reinstalled.
    fn pip_restore_deps_from_backup(&self, backup_lock: &Path) -> Result<()> {
        run_pip_install_with_retries(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                PIP_ONLY_BINARY,
                "--find-links",
                VENDOR_WHEELS_INDEX_URL,
                "--extra-index-url",
                "https://pypi.org/simple",
                "--upgrade",
                "--requirement",
                backup_lock.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
        )
        .with_context(|| {
            format!(
                "restoring Headroom dependencies from {}",
                backup_lock.display()
            )
        })
    }

    fn rollback_in_place_upgrade_inner(&self, ctx: &InPlaceUpgradeContext) -> bool {
        // Re-kill venv lock holders: an IDE can respawn its MCP server
        // between the failed install and this rollback, and a rollback that
        // hits the same Windows file locks reports restored=false and leaves
        // the runtime bricked (RUST-70).
        crate::state::kill_venv_lock_holders(&self.runtime.venv_dir);
        // Restore deps first so headroom-ai lands on a consistent dep set.
        let deps_ok = match ctx.previous_lock_backup.as_deref() {
            Some(backup) => {
                let ok = self.pip_restore_deps_from_backup(backup).is_ok();
                let active = self.active_lock_path();
                let _ = std::fs::copy(backup, &active);
                let _ = std::fs::remove_file(backup);
                ok
            }
            None => true,
        };
        let wheel_ok = self
            .pip_force_reinstall_headroom_version(&ctx.previous_version)
            .is_ok();
        let receipt_backup = self.headroom_receipt_backup_path();
        let receipt_path = self.headroom_receipt_path();
        let receipt_ok = if receipt_backup.exists() {
            let copy_ok = std::fs::copy(&receipt_backup, &receipt_path).is_ok();
            let _ = std::fs::remove_file(&receipt_backup);
            copy_ok
        } else {
            true
        };
        self.clear_upgrade_marker();
        deps_ok && wheel_ok && receipt_ok
    }

    fn update_headroom_receipt_after_in_place_upgrade(
        &self,
        release: &HeadroomRelease,
        mcp_install: Value,
    ) -> Result<()> {
        let receipt_path = self.headroom_receipt_path();
        let bytes = std::fs::read(&receipt_path)
            .with_context(|| format!("reading {}", receipt_path.display()))?;
        let mut receipt: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", receipt_path.display()))?;
        receipt["version"] = json!(release.version);
        if let Some(artifact) = receipt.get_mut("artifact").and_then(|a| a.as_object_mut()) {
            artifact.insert("url".into(), json!(release.wheel_url));
            artifact.insert("sha256".into(), json!(release.sha256));
            artifact.insert(
                "requirementsLockSha256".into(),
                json!(requirements_lock_sha(bootstrap_requirements_lock())),
            );
        }
        receipt["mcp"] = mcp_install;
        crate::client_adapters::atomic_write(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)
            .with_context(|| format!("writing {}", receipt_path.display()))?;
        Ok(())
    }

    /// Finalize a successful atomic upgrade. Deletes the backup venv and
    /// receipt snapshot. Non-fatal if cleanup fails — a future upgrade's
    /// "purge stale backup" step will clean up whatever we left behind.
    pub fn commit_headroom_upgrade(&self) -> Result<()> {
        let backup_dir = self.venv_backup_dir();
        if backup_dir.exists() {
            if let Err(err) = std::fs::remove_dir_all(&backup_dir) {
                log::warn!(
                    "commit_headroom_upgrade: non-fatal: failed to remove {}: {err}",
                    backup_dir.display()
                );
            }
        }
        let _ = std::fs::remove_file(self.headroom_receipt_backup_path());
        let _ = std::fs::remove_file(self.lock_backup_path());
        // Clear the in-progress marker last, so a mid-commit crash (e.g.,
        // between the remove_dir_all of the backup and the marker cleanup)
        // still looks like an interrupted upgrade on the next launch and
        // triggers recovery rather than a potentially-unsafe purge.
        self.clear_upgrade_marker();
        Ok(())
    }

    /// Restore both venv + receipt from their backups. Used from the atomic
    /// upgrade failure path and from the post-boot-validation rollback path.
    /// Returns true if the restore succeeded.
    fn rollback_partial_upgrade(&self, had_live_venv: bool, had_receipt: bool) -> bool {
        // Remove any partial new venv.
        if self.runtime.venv_dir.exists() {
            if let Err(err) = std::fs::remove_dir_all(&self.runtime.venv_dir) {
                log::error!(
                    "rollback: failed to remove partial venv at {}: {err}",
                    self.runtime.venv_dir.display()
                );
                return false;
            }
        }
        let venv_restored = self.restore_venv_from_backup(had_live_venv);
        if !venv_restored {
            return false;
        }
        if had_receipt {
            let receipt_path = self.headroom_receipt_path();
            let receipt_backup = self.headroom_receipt_backup_path();
            if let Err(err) = std::fs::copy(&receipt_backup, &receipt_path) {
                log::error!(
                    "rollback: failed to restore {}: {err}",
                    receipt_path.display()
                );
                return false;
            }
            let _ = std::fs::remove_file(&receipt_backup);
        }
        // Rollback complete — clear the marker so we don't trigger recovery
        // on the next launch.
        self.clear_upgrade_marker();
        true
    }

    fn restore_venv_from_backup(&self, had_live_venv: bool) -> bool {
        if !had_live_venv {
            return true;
        }
        let backup_dir = self.venv_backup_dir();
        if !backup_dir.exists() {
            return true;
        }
        match std::fs::rename(&backup_dir, &self.runtime.venv_dir) {
            Ok(()) => true,
            Err(err) => {
                log::error!(
                    "rollback: failed to restore venv from {}: {err}",
                    backup_dir.display()
                );
                false
            }
        }
    }

    /// Runs MCP install if the receipt shows it is not configured, or was
    /// configured via the legacy `~/.claude/mcp.json` fallback (which Claude
    /// Code ≥2.x ignores). Safe to call at every launch — no-ops when the
    /// server is already registered via `claude mcp add` or direct json write.
    ///
    /// If the install fails with a Python `ModuleNotFoundError`/`ImportError`
    /// in stderr, the venv is missing one or more pinned dependencies despite
    /// the receipt's `requirementsLockSha256` saying otherwise (seen on
    /// upgrades from very-old desktop versions where a partial install left
    /// the receipt stamped but the venv incomplete). Self-heal by running the
    /// requirements repair, which re-installs the full lock file and retries
    /// MCP install internally.
    pub fn ensure_mcp_configured(&self) -> Result<()> {
        if self.headroom_mcp_configured() == Some(true)
            && matches!(
                self.headroom_mcp_install_method().as_deref(),
                Some(MCP_METHOD_CLAUDE_CLI) | Some(MCP_METHOD_DIRECT_CLAUDE_JSON)
            )
        {
            return Ok(());
        }
        let method = match self.install_headroom_mcp() {
            Ok(method) => method,
            Err(err) if looks_like_corrupt_venv_error(&err) => {
                log::warn!(
                    "MCP install hit a Python import error; running requirements repair: {err:#}"
                );
                sentry::capture_message(
                    "MCP install hit corrupt-venv signal; auto-running requirements repair",
                    sentry::Level::Info,
                );
                self.repair_stale_requirements_with_progress(|_| {})
                    .context("auto-repairing venv after MCP install import error")?;
                // repair_stale_requirements_with_progress runs install_headroom_mcp
                // and writes the mcp section of the receipt itself, so we're done.
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let receipt_path = self.runtime.tools_dir.join("headroom.json");
        if let Ok(bytes) = std::fs::read(&receipt_path) {
            if let Ok(mut receipt) = serde_json::from_slice::<Value>(&bytes) {
                receipt["mcp"] = json!({
                    "configured": true,
                    "proxyUrl": HEADROOM_PROXY_URL,
                    "installMethod": method.as_str(),
                });
                let _ = crate::client_adapters::atomic_write(
                    &receipt_path,
                    &serde_json::to_vec(&receipt)?,
                );
            }
        }
        Ok(())
    }

    fn install_headroom_mcp(&self) -> Result<McpInstallMethod> {
        let entrypoint = self.headroom_entrypoint();
        let detected_claude = crate::claude_cli::detect_claude_cli();

        // GUI apps launched from Finder/Dock inherit a minimal PATH that
        // excludes /opt/homebrew/bin, /usr/local/bin, ~/.claude/local/bin,
        // etc. Without augmentation, `shutil.which("claude")` inside the
        // Python CLI returns None and it falls back to writing
        // ~/.claude/mcp.json — a legacy path Claude Code ≥2.x does not read.
        let run_install = |force: bool| -> Result<(std::process::Output, Vec<&'static str>)> {
            let mut args: Vec<&'static str> =
                vec!["mcp", "install", "--proxy-url", HEADROOM_PROXY_URL];
            if force {
                args.push("--force");
            }
            let mut cmd = build_command(&entrypoint, &args[..], &self.runtime.root_dir);
            if let Some(claude_path) = detected_claude.as_ref() {
                if let Some(dir) = claude_path.parent() {
                    cmd.env("PATH", crate::proc::path_with_dir_prepended(dir));
                }
            }
            let output = cmd
                .output()
                .with_context(|| format!("starting {} {}", entrypoint.display(), args.join(" ")))
                .context("configuring Headroom MCP integration")?;
            Ok((output, args))
        };

        // Always pass --force so that stale entrypoints left over from a
        // previous Headroom version (e.g. venv python3 path → headroom CLI)
        // are overwritten without a separate retry. --force is a no-op when
        // the config is already correct or absent. Desktop owns this config.
        let (output, args) = run_install(true)?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let exit_code = output.status.code();
            let signal = exit_status_signal(&output.status);

            // If the Python CLI exited non-zero only because no supported tool
            // (claude, codex, etc.) was detected on PATH, it wrote nothing --
            // but the direct JSON write path below can still configure the
            // integration. Fall through instead of surfacing a Sentry warning.
            //
            // A CLI that died importing its own runtime because the machine's
            // security policy blocked a DLL is the proxy-start failure seen
            // from a second angle (RUST-B9: same host, same minute, same
            // `_sqlite3` block as RUST-BB). That block is reported once per
            // session by `capture_headroom_start_failure`; a second issue
            // here adds nothing to act on, and the error still propagates so
            // the caller records the integration as not configured.
            let blocked_by_policy = crate::is_endpoint_protection_signal(&stderr);
            if blocked_by_policy {
                log::warn!(
                    "headroom mcp install died loading the runtime (endpoint protection); \
                     reported via the proxy-start capture"
                );
            }
            if !stdout.contains("not detected on this system") && !blocked_by_policy {
                let detected = detected_claude
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<not detected>".into());
                sentry::with_scope(
                    |scope| {
                        scope.set_extra("claude_cli_detected", detected.clone().into());
                        scope.set_extra(
                            "exit_code",
                            exit_code
                                .map(|c| c.into())
                                .unwrap_or(serde_json::Value::Null),
                        );
                        scope.set_extra(
                            "signal",
                            signal.map(|s| s.into()).unwrap_or(serde_json::Value::Null),
                        );
                        scope.set_extra(
                            "stdout_tail",
                            stdout[stdout.char_indices().rev().nth(2047).map_or(0, |(i, _)| i)..]
                                .into(),
                        );
                        scope.set_extra(
                            "stderr_tail",
                            stderr[stderr.char_indices().rev().nth(2047).map_or(0, |(i, _)| i)..]
                                .into(),
                        );
                    },
                    || {
                        sentry::capture_message(
                            "Headroom MCP install exited non-zero",
                            sentry::Level::Warning,
                        );
                    },
                );
                return Err(anyhow::Error::new(CommandFailure {
                    program: entrypoint.display().to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    stdout,
                    stderr,
                    exit_code,
                    signal,
                }))
                .context("configuring Headroom MCP integration");
            }
        }

        // Codex stores the MCP server command as a bare `headroom` (PATH-based,
        // via ~/.local/bin/headroom). When the managed runtime relocates that
        // symlink dangles and Codex fails to start the server. Pin it to the
        // absolute entrypoint so it survives runtime moves. Best-effort: a
        // failure here must not break the Claude integration below.
        let _ = crate::client_adapters::pin_codex_mcp_command(&entrypoint);
        let _ = crate::client_adapters::pin_grok_mcp_command(&entrypoint);

        // Ground truth: did Claude Code actually see the server? The Python
        // CLI's fallback branch writes ~/.claude/mcp.json (legacy, ignored by
        // Claude Code ≥2.x) and exits 0, so the subprocess succeeding is not
        // a reliable proxy for "integration works". Read the file Claude Code
        // actually reads and confirm the registration landed there.
        if claude_code_has_headroom_mcp_server() {
            return Ok(McpInstallMethod::ClaudeCli);
        }

        // The Python CLI couldn't find `claude` (e.g. GUI launch with bare
        // PATH) and wrote ~/.claude/mcp.json instead. Write the entry
        // directly to ~/.claude.json, which is what Claude Code ≥2.x reads.
        let direct_write = write_headroom_to_claude_json(&entrypoint, HEADROOM_PROXY_URL);
        if direct_write.is_ok() && claude_code_has_headroom_mcp_server() {
            return Ok(McpInstallMethod::DirectClaudeJson);
        }

        // Neither we nor the Python CLI found a `claude` anywhere: Claude Code
        // is not installed on this machine (RUST-D1: a Codex-only Windows
        // host), so "does not see the server" is the expected state, not a
        // registration that silently missed. The ~/.claude.json entry above
        // still waits for a later install. Only a detected CLI that still
        // cannot see the server is worth a report.
        let Some(detected) = detected_claude.as_ref().map(|p| p.display().to_string()) else {
            log::info!(
                "Headroom MCP install: claude CLI not detected on this machine; \
                 Claude Code registration left in ~/.claude.json"
            );
            return Ok(McpInstallMethod::FallbackJson);
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let claude_json_write_error = direct_write
            .err()
            .map(|err| format!("{err:#}"))
            .unwrap_or_default();
        sentry::with_scope(
            |scope| {
                scope.set_extra("claude_cli_detected", detected.clone().into());
                scope.set_extra(
                    "claude_json_write_error",
                    claude_json_write_error.clone().into(),
                );
                scope.set_extra(
                    "stdout_tail",
                    stdout[stdout.char_indices().rev().nth(511).map_or(0, |(i, _)| i)..].into(),
                );
                scope.set_extra(
                    "stderr_tail",
                    stderr[stderr.char_indices().rev().nth(511).map_or(0, |(i, _)| i)..].into(),
                );
            },
            || {
                sentry::capture_message(
                    "Headroom MCP install exited 0 but Claude Code does not see the server \
                     (fell back to ~/.claude/mcp.json which Claude Code ≥2.x ignores).",
                    sentry::Level::Warning,
                );
            },
        );
        Ok(McpInstallMethod::FallbackJson)
    }

    pub fn install_rtk(&self) -> Result<()> {
        let artifact = rtk_distribution_artifact()?;
        let extension = if cfg!(target_os = "windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let archive_path = self.runtime.downloads_dir.join(format!(
            "rtk-v{}-{}-{}.{}",
            RTK_VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH,
            extension
        ));
        download_to_path(&artifact.url, &archive_path, artifact.sha256)?;

        let extract_dir = self.runtime.downloads_dir.join("rtk-extract");
        if extract_dir.exists() {
            std::fs::remove_dir_all(&extract_dir)
                .with_context(|| format!("removing {}", extract_dir.display()))?;
        }
        std::fs::create_dir_all(&extract_dir)
            .with_context(|| format!("creating {}", extract_dir.display()))?;

        #[cfg(target_os = "windows")]
        {
            let file = std::fs::File::open(&archive_path)
                .with_context(|| format!("opening {}", archive_path.display()))?;
            let mut archive = zip::ZipArchive::new(file)
                .with_context(|| format!("reading zip {}", archive_path.display()))?;
            archive
                .extract(&extract_dir)
                .with_context(|| format!("extracting into {}", extract_dir.display()))?;
        }
        #[cfg(not(target_os = "windows"))]
        {
            let file = std::fs::File::open(&archive_path)
                .with_context(|| format!("opening {}", archive_path.display()))?;
            let decoder = GzDecoder::new(file);
            let mut archive = Archive::new(decoder);
            archive
                .unpack(&extract_dir)
                .with_context(|| format!("extracting into {}", extract_dir.display()))?;
        }

        let binary_name = if cfg!(target_os = "windows") {
            "rtk.exe"
        } else {
            "rtk"
        };
        let extracted_binary = extract_dir.join(binary_name);
        if !extracted_binary.exists() {
            bail!(
                "rtk extraction completed but {} was not found",
                extracted_binary.display()
            );
        }

        // Stage next to the destination, then rename into place: rtk is
        // exec'd by the PreToolUse hook on nearly every agent Bash command,
        // so copying over the live binary opens a truncated-exec window (and
        // fails with ETXTBSY on Linux while an rtk is running). Rename is
        // atomic for exec.
        let destination = self.rtk_entrypoint();
        let staged = {
            let mut s = destination.as_os_str().to_os_string();
            s.push(".new");
            PathBuf::from(s)
        };
        std::fs::copy(&extracted_binary, &staged)
            .with_context(|| format!("writing {}", staged.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&staged)
                .with_context(|| format!("reading {}", staged.display()))?
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&staged, permissions)
                .with_context(|| format!("chmod {}", staged.display()))?;
        }

        std::fs::rename(&staged, &destination)
            .with_context(|| format!("renaming {} into place", staged.display()))?;

        self.write_tool_receipt(
            "rtk",
            json!({
                "status": "healthy",
                "installedBy": "Headroom",
                "scope": "self-contained",
                "runtime": "binary",
                "entrypoint": destination,
                "source": "https://github.com/rtk-ai/rtk",
                "version": RTK_VERSION,
                "artifact": {
                    "url": artifact.url,
                    "sha256": artifact.sha256
                }
            }),
        )
    }

    fn write_headroom_requirements_lock(&self, contents: &str) -> Result<PathBuf> {
        let lock_path = self
            .runtime
            .downloads_dir
            .join("headroom-requirements.lock");
        crate::client_adapters::atomic_write(&lock_path, contents.as_bytes())
            .with_context(|| format!("writing {}", lock_path.display()))?;
        Ok(lock_path)
    }

    /// Marker recording the newest bootstrap progress milestone. Present
    /// without the READY flag = an attempt died without a verdict.
    fn bootstrap_attempt_marker_path(&self) -> PathBuf {
        self.runtime.root_dir.join("bootstrap-attempt.json")
    }

    /// Best-effort: a failed marker write must never fail the install itself.
    pub fn note_bootstrap_attempt(&self, step: &str, percent: u8) {
        let marker = AbandonedBootstrap {
            step: step.to_string(),
            percent,
        };
        if let Ok(json) = serde_json::to_vec(&marker) {
            let _ =
                crate::client_adapters::atomic_write(&self.bootstrap_attempt_marker_path(), &json);
        }
    }

    /// Remove the marker once the attempt has a verdict: success writes the
    /// READY flag, and a classified failure already reported to Sentry, so
    /// either way the next launch has nothing abandoned to report.
    pub fn clear_bootstrap_attempt(&self) {
        let _ = std::fs::remove_file(self.bootstrap_attempt_marker_path());
    }

    /// Consume the marker left by a bootstrap attempt that never reached a
    /// verdict. Reports at most once: the marker is deleted on read. A READY
    /// runtime means a later attempt succeeded, so nothing was abandoned.
    pub fn take_abandoned_bootstrap(&self) -> Option<AbandonedBootstrap> {
        let path = self.bootstrap_attempt_marker_path();
        let raw = std::fs::read(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        if self.python_runtime_installed() {
            return None;
        }
        serde_json::from_slice(&raw).ok()
    }

    /// Marker recording the last bootstrap failure reported to Sentry. Policy
    /// verdicts (Application Control, WDAC) fail identically on every launch,
    /// and each launch re-captured them: RUST-AN was one blocked laptop filing
    /// 21 identical events in a day, which escalated the issue with zero new
    /// information.
    fn bootstrap_failure_capture_marker_path(&self) -> PathBuf {
        self.runtime.root_dir.join("bootstrap-failure-capture.json")
    }

    /// True when this bootstrap failure should reach Sentry: first occurrence,
    /// a different `key` than the last capture, or the last capture is over
    /// 24h old -- so a machine stuck on the same wall still reports once a day
    /// (keeping the issue's lastSeen honest) instead of once per relaunch.
    /// Best-effort on I/O: an unreadable marker reports rather than
    /// suppresses.
    pub fn should_capture_bootstrap_failure(&self, key: &str) -> bool {
        #[derive(Default, Serialize, Deserialize)]
        #[serde(default)]
        struct CaptureMarker {
            key: String,
            unix_ts: u64,
        }
        const DEDUPE_WINDOW_SECS: u64 = 24 * 60 * 60;
        let path = self.bootstrap_failure_capture_marker_path();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let is_repeat = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<CaptureMarker>(&raw).ok())
            .is_some_and(|m| m.key == key && now.saturating_sub(m.unix_ts) < DEDUPE_WINDOW_SECS);
        if is_repeat {
            return false;
        }
        // Written only on capture, not refreshed on suppressed repeats, so the
        // window is fixed rather than sliding: a permanently stuck machine
        // reports daily instead of going silent forever.
        if let Ok(json) = serde_json::to_vec(&CaptureMarker {
            key: key.to_string(),
            unix_ts: now,
        }) {
            let _ = crate::client_adapters::atomic_write(&path, &json);
        }
        true
    }

    fn write_bootstrap_receipt(&self) -> Result<()> {
        let receipt = self.runtime.root_dir.join("bootstrap-receipt.json");
        // Receipts/flags gate bootstrap and upgrade decisions: a truncated one
        // reads as "not installed" and triggers a multi-minute rebuild, so all
        // of them go through tmp+rename.
        crate::client_adapters::atomic_write(
            &receipt,
            &serde_json::to_vec_pretty(&json!({
                "managedBy": "Headroom",
                "runtime": "python",
                "scope": "self-contained",
                "downloadsDir": self.runtime.downloads_dir,
                "managedBinDir": self.runtime.bin_dir,
                "pythonDistribution": self.runtime.standalone_python(),
                "managedPython": self.runtime.managed_python(),
                "managedPip": self.runtime.managed_pip(),
                "toolsDir": self.runtime.tools_dir
            }))
            .context("serializing bootstrap receipt")?,
        )
        .with_context(|| format!("writing {}", receipt.display()))?;
        Ok(())
    }

    fn write_ready_flag(&self) -> Result<()> {
        let ready_flag = self.runtime.ready_flag();
        crate::client_adapters::atomic_write(
            &ready_flag,
            json!({
                "managedPython": self.runtime.managed_python(),
                "managedPip": self.runtime.managed_pip(),
                "scope": "self-contained"
            })
            .to_string()
            .as_bytes(),
        )
        .with_context(|| format!("writing {}", ready_flag.display()))?;
        Ok(())
    }

    fn write_tool_receipt(&self, tool_id: &str, payload: serde_json::Value) -> Result<()> {
        let path = self.runtime.tools_dir.join(format!("{tool_id}.json"));
        crate::client_adapters::atomic_write(
            &path,
            &serde_json::to_vec_pretty(&payload).context("serializing managed tool receipt")?,
        )
        .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn read_tool_receipt(&self, tool_id: &str) -> Option<Value> {
        let path = self.runtime.tools_dir.join(format!("{tool_id}.json"));
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Optional tools persist an `enabled` flag in their receipt. Required core
    /// tools (headroom, rtk) are always enabled. Missing flag defaults to true.
    fn tool_enabled(&self, tool_id: &str) -> bool {
        self.read_tool_receipt(tool_id)
            .and_then(|receipt| receipt.get("enabled").and_then(Value::as_bool))
            .unwrap_or(true)
    }

    pub fn markitdown_entrypoint(&self) -> PathBuf {
        let name = if cfg!(target_os = "windows") {
            "markitdown.exe"
        } else {
            "markitdown"
        };
        self.runtime.venv_dir.join(bin_subdir()).join(name)
    }

    /// Shim in the Headroom-managed bin dir. The Office nudge and the Bash
    /// permission both reference this absolute path, so it works whether or not
    /// the bin dir is on PATH (RTK, which exports it, is now opt-in).
    pub fn markitdown_shim_path(&self) -> PathBuf {
        let name = if cfg!(target_os = "windows") {
            "markitdown.cmd"
        } else {
            "markitdown"
        };
        self.runtime.bin_dir.join(name)
    }

    fn markitdown_conversion_counter_path(&self) -> PathBuf {
        self.runtime.tools_dir.join("markitdown-conversions")
    }

    /// Lifetime conversions recorded by the shim. None until the first one.
    pub fn markitdown_conversion_count(&self) -> Option<u64> {
        let raw = std::fs::read_to_string(self.markitdown_conversion_counter_path()).ok()?;
        raw.trim().parse::<u64>().ok().filter(|count| *count > 0)
    }

    /// Wrapper script (previously a bare symlink) so each real conversion bumps
    /// a counter the Addons tab can show. Flag-only invocations (--help) are
    /// not counted. Re-run on every launch so pre-wrapper installs pick it up.
    pub fn ensure_markitdown_shim(&self) -> Result<()> {
        let shim = self.markitdown_shim_path();
        if shim.exists() || shim.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&shim);
        }
        #[cfg(unix)]
        {
            // ponytail: single-quoting is enough - both paths live under
            // Application Support (spaces, no quotes).
            let script = format!(
                "#!/bin/sh\n\
                 # Headroom-managed markitdown shim. Counts conversions, then runs the real binary.\n\
                 case \"$1\" in\n\
                   \"\"|-*) ;;\n\
                   *)\n\
                     C='{counter}'\n\
                     n=$(cat \"$C\" 2>/dev/null)\n\
                     case \"$n\" in ''|*[!0-9]*) n=0;; esac\n\
                     printf '%s' $((n+1)) > \"$C.tmp\" 2>/dev/null && mv -f \"$C.tmp\" \"$C\" 2>/dev/null\n\
                     ;;\n\
                 esac\n\
                 exec '{real}' \"$@\"\n",
                counter = self.markitdown_conversion_counter_path().display(),
                real = self.markitdown_entrypoint().display(),
            );
            crate::client_adapters::atomic_write(&shim, script.as_bytes())
                .with_context(|| format!("writing markitdown shim {}", shim.display()))?;
            let mut perms = std::fs::metadata(&shim)?.permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&shim, perms).with_context(|| {
                format!("marking markitdown shim executable {}", shim.display())
            })?;
        }
        #[cfg(target_os = "windows")]
        {
            let script = format!(
                "@echo off\r\n\
                 setlocal\r\n\
                 rem Headroom-managed markitdown shim. Counts conversions, then runs the real binary.\r\n\
                 if \"%~1\"==\"\" goto :run\r\n\
                 if \"%~1\"==\"--help\" goto :run\r\n\
                 set \"C={counter}\"\r\n\
                 set /p n=<\"%C%\" 2>nul\r\n\
                 if not defined n set n=0\r\n\
                 set /a n+=1 >nul 2>nul\r\n\
                 >\"%C%.tmp\" echo %n%\r\n\
                 move /y \"%C%.tmp\" \"%C%\" >nul 2>nul\r\n\
                 :run\r\n\
                 \"{real}\" %*\r\n",
                counter = self.markitdown_conversion_counter_path().display(),
                real = self.markitdown_entrypoint().display(),
            );
            crate::client_adapters::atomic_write(&shim, script.as_bytes())
                .with_context(|| format!("writing markitdown shim {}", shim.display()))?;
        }
        Ok(())
    }

    pub fn markitdown_installed(&self) -> bool {
        self.runtime.tools_dir.join("markitdown.json").exists()
            && self.markitdown_entrypoint().exists()
    }

    pub fn install_markitdown(&self) -> Result<()> {
        run_pip_install_with_retries_streaming(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                &format!("markitdown[all]=={MARKITDOWN_PINNED_VERSION}"),
            ],
            &self.runtime.root_dir,
            |line| log_pip_line("markitdown pip", line),
        )?;
        if !self.markitdown_entrypoint().exists() {
            bail!(
                "markitdown install completed but {} was not found",
                self.markitdown_entrypoint().display()
            );
        }
        run_command_with_timeout(
            &self.markitdown_entrypoint(),
            &["--help"],
            &self.runtime.root_dir,
            HEADROOM_SMOKE_TEST_TIMEOUT,
        )
        .context("markitdown installed but failed its smoke test")?;
        self.ensure_markitdown_shim()?;
        self.write_tool_receipt(
            "markitdown",
            json!({ "version": MARKITDOWN_PINNED_VERSION, "enabled": true }),
        )?;
        Ok(())
    }

    pub fn set_markitdown_enabled(&self, enabled: bool) -> Result<()> {
        if !self.markitdown_installed() {
            bail!("markitdown is not installed");
        }
        self.write_tool_receipt(
            "markitdown",
            json!({ "version": MARKITDOWN_PINNED_VERSION, "enabled": enabled }),
        )?;
        Ok(())
    }

    pub fn uninstall_markitdown(&self) -> Result<()> {
        let _ = run_command_streaming(
            &self.runtime.managed_python(),
            &["-m", "pip", "uninstall", "-y", "markitdown"],
            &self.runtime.root_dir,
            Some(PIP_OUTPUT_SILENCE_TIMEOUT),
            &mut |line: &str| log_pip_line("markitdown pip uninstall", line),
        );
        let shim = self.markitdown_shim_path();
        if shim.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&shim);
        }
        let receipt = self.runtime.tools_dir.join("markitdown.json");
        if receipt.exists() {
            std::fs::remove_file(&receipt)
                .with_context(|| format!("removing {}", receipt.display()))?;
        }
        Ok(())
    }

    /// Serena lives in its own venv: its LSP dependency tree must never
    /// up/downgrade headroom-ai's pins in the shared venv (a broken optional
    /// addon must not take down the proxy). Outside runtime/ so in-place and
    /// atomic runtime upgrades can't wipe it.
    pub fn serena_venv_dir(&self) -> PathBuf {
        self.runtime.root_dir.join("serena-venv")
    }

    pub fn serena_entrypoint(&self) -> PathBuf {
        let name = if cfg!(target_os = "windows") {
            "serena.exe"
        } else {
            "serena"
        };
        self.serena_venv_dir().join(bin_subdir()).join(name)
    }

    pub fn serena_installed(&self) -> bool {
        self.runtime.tools_dir.join("serena.json").exists() && self.serena_entrypoint().exists()
    }

    pub fn install_serena(&self) -> Result<()> {
        // --clear so a retry after a partial install starts from a clean venv.
        run_command_with_timeout(
            &self.runtime.standalone_python(),
            &[
                "-m",
                "venv",
                "--clear",
                &self.serena_venv_dir().to_string_lossy(),
            ],
            &self.runtime.root_dir,
            Duration::from_secs(120),
        )
        .context("creating serena venv")?;
        let serena_python = self
            .serena_venv_dir()
            .join(bin_subdir())
            .join(python_exe_name());
        run_pip_install_with_retries_streaming(
            &serena_python,
            &[
                "-m",
                "pip",
                "install",
                "--timeout",
                "180",
                "--retries",
                "10",
                &format!("serena-agent=={SERENA_PINNED_VERSION}"),
            ],
            &self.runtime.root_dir,
            |line| log_pip_line("serena pip", line),
        )?;
        if !self.serena_entrypoint().exists() {
            bail!(
                "serena install completed but {} was not found",
                self.serena_entrypoint().display()
            );
        }
        run_command_with_timeout(
            &self.serena_entrypoint(),
            &["--help"],
            &self.runtime.root_dir,
            SERENA_SMOKE_TEST_TIMEOUT,
        )
        .context("serena installed but failed its smoke test")?;
        self.register_serena_mcp()?;
        set_serena_global_gitignore(true);
        self.write_tool_receipt(
            "serena",
            json!({ "version": SERENA_PINNED_VERSION, "enabled": true }),
        )?;
        Ok(())
    }

    pub fn set_serena_enabled(&self, enabled: bool) -> Result<()> {
        if !self.serena_installed() {
            bail!("serena is not installed");
        }
        if enabled {
            self.register_serena_mcp()?;
        } else {
            self.unregister_serena_mcp()?;
        }
        self.write_tool_receipt(
            "serena",
            json!({ "version": SERENA_PINNED_VERSION, "enabled": enabled }),
        )?;
        Ok(())
    }

    pub fn uninstall_serena(&self) -> Result<()> {
        // Unregister before deleting the venv, and fail if it doesn't land: a
        // leftover MCP entry pointing at a deleted binary would make every new
        // agent session spawn a failing server.
        self.unregister_serena_mcp()?;
        set_serena_global_gitignore(false);
        let venv = self.serena_venv_dir();
        if venv.exists() {
            // Retrying helper, not a bare remove_dir_all: it clears read-only
            // bits across the tree, which is the half of Windows "Access is
            // denied" we can actually fix (Sentry RUST-6T).
            crate::client_adapters::remove_dir_all_retry(&venv).map_err(|err| {
                // The other half is an open handle -- an agent session still
                // running serena's MCP server out of this venv. We will not kill
                // a user's editor to win a delete, so name the cause instead: a
                // bare "Access is denied" leaves them with nothing to act on.
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    anyhow!(
                        "removing {} was denied. Serena may still be running as an MCP server \
                         in an open Claude Code or Codex session -- close those and uninstall \
                         again. Underlying error: {err}",
                        venv.display()
                    )
                } else {
                    anyhow::Error::new(err).context(format!("removing {}", venv.display()))
                }
            })?;
        }
        let receipt = self.runtime.tools_dir.join("serena.json");
        if receipt.exists() {
            std::fs::remove_file(&receipt)
                .with_context(|| format!("removing {}", receipt.display()))?;
        }
        Ok(())
    }

    fn register_serena_mcp(&self) -> Result<()> {
        set_serena_browser_dashboard();
        let entrypoint = self.serena_entrypoint().to_string_lossy().into_owned();
        self.run_mcp_helper(&["-c", SERENA_MCP_HELPER, "register", &entrypoint])
            .context("registering serena MCP server")
    }

    fn unregister_serena_mcp(&self) -> Result<()> {
        self.run_mcp_helper(&["-c", SERENA_MCP_HELPER, "unregister"])
            .context("unregistering serena MCP server")
    }

    pub fn context7_installed(&self) -> bool {
        self.runtime.tools_dir.join("context7.json").exists()
    }

    /// No managed venv or binary: the agent runs the pinned package through
    /// its own `npx`. Install just proves that works once (warming the npx
    /// cache), then registers the MCP entry — a broken entry would make every
    /// new agent session spawn a failing server.
    pub fn install_context7(&self) -> Result<()> {
        let npx = crate::claude_cli::detect_npx().context(
            "npx was not found. Context7 runs through Node.js -- install Node.js, then try again.",
        )?;
        run_command_with_timeout(
            &npx,
            &["-y", &context7_package_spec(), "--help"],
            &self.runtime.root_dir,
            CONTEXT7_INSTALL_TIMEOUT,
        )
        // ponytail: no separate Node version probe. Context7 4.x declares
        // node >=20.18.1 but npm only warns on an engines mismatch, so the
        // --help run above is what actually proves this Node can run it.
        .context(
            "context7 failed its smoke test (npx download or startup). Context7 needs Node.js 20.18.1 or newer",
        )?;
        self.register_context7_mcp()?;
        self.write_tool_receipt(
            "context7",
            json!({ "version": CONTEXT7_PINNED_VERSION, "enabled": true }),
        )?;
        Ok(())
    }

    pub fn set_context7_enabled(&self, enabled: bool) -> Result<()> {
        if !self.context7_installed() {
            bail!("context7 is not installed");
        }
        if enabled {
            self.register_context7_mcp()?;
        } else {
            self.unregister_context7_mcp()?;
        }
        self.write_tool_receipt(
            "context7",
            json!({ "version": CONTEXT7_PINNED_VERSION, "enabled": enabled }),
        )?;
        Ok(())
    }

    pub fn uninstall_context7(&self) -> Result<()> {
        self.unregister_context7_mcp()?;
        let receipt = self.runtime.tools_dir.join("context7.json");
        if receipt.exists() {
            std::fs::remove_file(&receipt)
                .with_context(|| format!("removing {}", receipt.display()))?;
        }
        Ok(())
    }

    fn register_context7_mcp(&self) -> Result<()> {
        self.run_mcp_helper(&[
            "-c",
            CONTEXT7_MCP_HELPER,
            "register",
            &context7_package_spec(),
        ])
        .context("registering context7 MCP server")
    }

    fn unregister_context7_mcp(&self) -> Result<()> {
        self.run_mcp_helper(&["-c", CONTEXT7_MCP_HELPER, "unregister"])
            .context("unregistering context7 MCP server")
    }

    pub fn codebase_memory_entrypoint(&self) -> PathBuf {
        self.runtime.bin_dir.join("codebase-memory-mcp")
    }

    /// Index databases live here (via `CBM_CACHE_DIR`) instead of the
    /// binary's default `~/.cache/codebase-memory-mcp`, so uninstalling the
    /// addon or Headroom removes them too.
    fn codebase_memory_cache_dir(&self) -> PathBuf {
        self.runtime.tools_dir.join("codebase-memory-cache")
    }

    pub fn codebase_memory_installed(&self) -> bool {
        self.runtime.tools_dir.join("codebase-memory.json").exists()
            && self.codebase_memory_entrypoint().exists()
    }

    pub fn install_codebase_memory(&self) -> Result<()> {
        let artifact = codebase_memory_distribution_artifact()?;
        let archive_path = self.runtime.downloads_dir.join(format!(
            "codebase-memory-mcp-v{}-{}-{}.tar.gz",
            CODEBASE_MEMORY_VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        download_to_path(&artifact.url, &archive_path, artifact.sha256)?;

        let extract_dir = self.runtime.downloads_dir.join("codebase-memory-extract");
        if extract_dir.exists() {
            std::fs::remove_dir_all(&extract_dir)
                .with_context(|| format!("removing {}", extract_dir.display()))?;
        }
        std::fs::create_dir_all(&extract_dir)
            .with_context(|| format!("creating {}", extract_dir.display()))?;

        let file = std::fs::File::open(&archive_path)
            .with_context(|| format!("opening {}", archive_path.display()))?;
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);
        archive
            .unpack(&extract_dir)
            .with_context(|| format!("extracting into {}", extract_dir.display()))?;

        let extracted_binary = extract_dir.join("codebase-memory-mcp");
        if !extracted_binary.exists() {
            bail!(
                "codebase-memory extraction completed but {} was not found",
                extracted_binary.display()
            );
        }

        // Stage then rename: live agent sessions may be running the old
        // binary as an MCP server, and copying over it risks ETXTBSY /
        // truncated-exec. Rename is atomic.
        let destination = self.codebase_memory_entrypoint();
        let staged = {
            let mut s = destination.as_os_str().to_os_string();
            s.push(".new");
            PathBuf::from(s)
        };
        std::fs::rename(&extracted_binary, &staged)
            .with_context(|| format!("staging {}", staged.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&staged)
                .with_context(|| format!("reading permissions of {}", staged.display()))?
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&staged, permissions)
                .with_context(|| format!("marking {} executable", staged.display()))?;
        }
        std::fs::rename(&staged, &destination)
            .with_context(|| format!("installing {}", destination.display()))?;

        run_command_with_timeout(
            &destination,
            &["--version"],
            &self.runtime.root_dir,
            Duration::from_secs(15),
        )
        .context("codebase-memory installed but failed its smoke test")?;
        std::fs::create_dir_all(self.codebase_memory_cache_dir())
            .with_context(|| format!("creating {}", self.codebase_memory_cache_dir().display()))?;
        self.register_codebase_memory_mcp()?;
        self.write_tool_receipt(
            "codebase-memory",
            json!({ "version": CODEBASE_MEMORY_VERSION, "enabled": true }),
        )?;
        Ok(())
    }

    pub fn set_codebase_memory_enabled(&self, enabled: bool) -> Result<()> {
        if !self.codebase_memory_installed() {
            bail!("codebase-memory is not installed");
        }
        if enabled {
            self.register_codebase_memory_mcp()?;
        } else {
            self.unregister_codebase_memory_mcp()?;
        }
        self.write_tool_receipt(
            "codebase-memory",
            json!({ "version": CODEBASE_MEMORY_VERSION, "enabled": enabled }),
        )?;
        Ok(())
    }

    pub fn uninstall_codebase_memory(&self) -> Result<()> {
        // Unregister first: a leftover MCP entry pointing at a deleted binary
        // would make every new agent session spawn a failing server.
        self.unregister_codebase_memory_mcp()?;
        let binary = self.codebase_memory_entrypoint();
        if binary.exists() {
            std::fs::remove_file(&binary)
                .with_context(|| format!("removing {}", binary.display()))?;
        }
        let cache = self.codebase_memory_cache_dir();
        if cache.exists() {
            std::fs::remove_dir_all(&cache)
                .with_context(|| format!("removing {}", cache.display()))?;
        }
        let receipt = self.runtime.tools_dir.join("codebase-memory.json");
        if receipt.exists() {
            std::fs::remove_file(&receipt)
                .with_context(|| format!("removing {}", receipt.display()))?;
        }
        Ok(())
    }

    fn register_codebase_memory_mcp(&self) -> Result<()> {
        let binary = self
            .codebase_memory_entrypoint()
            .to_string_lossy()
            .into_owned();
        let cache_dir = self
            .codebase_memory_cache_dir()
            .to_string_lossy()
            .into_owned();
        self.run_mcp_helper(&[
            "-c",
            CODEBASE_MEMORY_MCP_HELPER,
            "register",
            &binary,
            &cache_dir,
        ])
        .context("registering codebase-memory MCP server")
    }

    fn unregister_codebase_memory_mcp(&self) -> Result<()> {
        self.run_mcp_helper(&["-c", CODEBASE_MEMORY_MCP_HELPER, "unregister"])
            .context("unregistering codebase-memory MCP server")
    }

    fn run_mcp_helper(&self, args: &[&str]) -> Result<()> {
        // ClaudeRegistrar may shell out to the `claude` CLI, which can take a
        // few seconds per agent; 60s covers both registrars comfortably.
        run_command_with_timeout(
            &self.managed_python(),
            args,
            &self.runtime.root_dir,
            Duration::from_secs(60),
        )
    }

    /// Remove the managed rtk binary and its receipt. Shell PATH and Claude Code
    /// hook teardown is handled separately by `client_adapters::set_rtk_enabled`.
    pub fn uninstall_rtk(&self) -> Result<()> {
        let binary = self.rtk_entrypoint();
        if binary.exists() {
            std::fs::remove_file(&binary)
                .with_context(|| format!("removing {}", binary.display()))?;
        }
        let receipt = self.runtime.tools_dir.join("rtk.json");
        if receipt.exists() {
            std::fs::remove_file(&receipt)
                .with_context(|| format!("removing {}", receipt.display()))?;
        }
        Ok(())
    }

    /// A plugin install is genuine only when our receipt exists AND at least
    /// one host (Claude Code or Codex) still has the plugin registered, so a
    /// user who removes it via `/plugin` doesn't leave the card stuck on
    /// "Enabled".
    #[cfg(test)]
    pub fn plugin_installed(&self, id: &str) -> bool {
        let Some(plugin) = plugin_addon(id) else {
            return false;
        };
        self.plugin_receipt_exists(plugin)
            && PluginHost::ALL
                .iter()
                .any(|host| host.plugin_present(plugin))
    }

    fn plugin_receipt_exists(&self, plugin: &PluginAddon) -> bool {
        self.runtime
            .tools_dir
            .join(format!("{}.json", plugin.id))
            .exists()
    }

    fn run_plugin_cmd(
        &self,
        plugin: &PluginAddon,
        cli: &Path,
        host: PluginHost,
        args: &[&str],
    ) -> Result<()> {
        let id = plugin.id;
        let label = host.label();
        run_command_streaming(
            cli,
            args,
            &self.runtime.root_dir,
            None,
            &mut |line: &str| log::info!("{id} [{label}]: {line}"),
        )
    }

    /// Registers the marketplace (best-effort) and installs the plugin into a
    /// single host. Used for first install, re-enable, and Update.
    ///
    /// Already present on this host means this is an Update: both hosts install
    /// from a local marketplace checkout, so the snapshot has to be refreshed
    /// first or the "update" reinstalls the same commit. Plain `install`/`add`
    /// on an installed plugin is a no-op, which is why Update cannot just be
    /// the install path replayed.
    fn install_plugin_into(&self, plugin: &'static PluginAddon, host: PluginHost) -> Result<()> {
        let cli = host.cli().context("CLI not found on PATH")?;
        if host.plugin_present(plugin) {
            let _ = self.run_plugin_cmd(plugin, &cli, host, &host.marketplace_update_args(plugin));
            self.run_plugin_cmd(plugin, &cli, host, &host.update_args(plugin))?;
        } else {
            // Re-adding an already-known marketplace is a benign error, so its
            // failure is not fatal on its own -- but it must not be discarded
            // either. When `marketplace add` fails for a real reason (offline,
            // git failure, unwritable snapshot dir) the install that follows
            // reports only "plugin <x> was not found in marketplace <y>", which
            // names a consequence and hides every cause (Sentry RUST-6K). Carry
            // the add error and attach it if the install then fails.
            let mut marketplace_err = self
                .run_plugin_cmd(plugin, &cli, host, &host.marketplace_add_args(plugin))
                .err();
            // "marketplace 'x' is already added from a different source": the
            // host has our marketplace recorded under another spelling of the
            // same repo (RUST-AG: both of ours on one host at once, so a Codex
            // normalisation change, not a user fork) and the install below then
            // fails "not found in marketplace". The name is ours; re-register
            // it from the canonical source and let the install decide.
            if marketplace_err.as_ref().is_some_and(|err| {
                format!("{err:#}").contains("already added from a different source")
            }) {
                log::info!(
                    "{} [{}]: marketplace registered from another source; re-adding",
                    plugin.id,
                    host.label()
                );
                let _ =
                    self.run_plugin_cmd(plugin, &cli, host, &host.marketplace_remove_args(plugin));
                marketplace_err = self
                    .run_plugin_cmd(plugin, &cli, host, &host.marketplace_add_args(plugin))
                    .err();
            }
            self.run_plugin_cmd(plugin, &cli, host, &host.install_args(plugin))
                .map_err(|err| match marketplace_err {
                    Some(add_err) => {
                        err.context(format!("marketplace add failed first: {add_err:#}"))
                    }
                    None => err,
                })?;
        }
        if !host.plugin_present(plugin) {
            bail!("install completed but the plugin was not registered");
        }
        Ok(())
    }

    /// Installs a plugin addon into every host that has a CLI on PATH. Returns
    /// `Ok(true)` when at least one host succeeded but Codex was skipped because
    /// it is too old to support `plugin add` -- the caller nudges the user to
    /// update Codex. A too-old Codex is not a real error (no Sentry warning); it
    /// is a version skew the user can only fix by updating Codex.
    pub fn install_plugin(&self, id: &str) -> Result<bool> {
        let plugin = plugin_addon(id).with_context(|| format!("unknown plugin addon: {id}"))?;
        let hosts: Vec<PluginHost> = PluginHost::ALL
            .into_iter()
            .filter(|host| host.cli().is_some())
            .collect();
        if hosts.is_empty() {
            bail!(
                "Neither the Claude Code CLI ('claude') nor the Codex CLI ('codex') was found on PATH. Install one, then try again."
            );
        }
        let mut errors: Vec<String> = Vec::new();
        let mut installed_any = false;
        let mut codex_outdated = false;
        for host in hosts {
            match self.install_plugin_into(plugin, host) {
                Ok(()) => installed_any = true,
                Err(err) if matches!(host, PluginHost::Codex) && is_outdated_codex(&err) => {
                    codex_outdated = true;
                }
                Err(err) => errors.push(format!("{}: {err:#}", host.label())),
            }
        }
        if !installed_any {
            if codex_outdated && errors.is_empty() {
                bail!(
                    "Your Codex CLI is too old to install the {id} plugin. Update Codex, then try again."
                );
            }
            bail!("installing the {id} plugin failed: {}", errors.join("; "));
        }
        if !errors.is_empty() {
            let detail = errors.join("; ");
            // Explicit per-category fingerprint; the bridged warn is local-only
            // (skip_sentry rule) so this doesn't double-report. Mirrors the pip
            // install path -- see `plugin_install_failure_category`.
            let category = plugin_install_failure_category(&detail);
            sentry::with_scope(
                |scope| {
                    scope.set_fingerprint(Some(&["plugin-install-partial", category]));
                },
                || {
                    sentry::capture_message(
                        &format!(
                            "{id} installed for some hosts but not all [{category}]: {detail}"
                        ),
                        sentry::Level::Warning,
                    );
                },
            );
            log::warn!("{id} installed for some hosts but not all: {detail}");
        }
        let version =
            installed_plugin_version(plugin).unwrap_or_else(|| PLUGIN_DISPLAY_VERSION.into());
        self.write_tool_receipt(plugin.id, json!({ "version": version, "enabled": true }))?;
        Ok(codex_outdated)
    }

    pub fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let plugin = plugin_addon(id).with_context(|| format!("unknown plugin addon: {id}"))?;
        // Guard on the receipt, not host presence: disabling on a host without a
        // disable verb (Codex) removes the plugin, so `plugin_installed()`
        // would be false and re-enabling could never get past this check.
        if !self.plugin_receipt_exists(plugin) {
            bail!("{id} is not installed");
        }
        let mut errors: Vec<String> = Vec::new();
        let mut changed_any = false;
        for host in PluginHost::ALL {
            let Some(cli) = host.cli() else { continue };
            // Codex has no enable/disable verb, so enabling re-installs and
            // disabling removes. Skip disabling a host that isn't present.
            let result = if enabled {
                self.install_plugin_into(plugin, host)
            } else if host.plugin_present(plugin) {
                self.run_plugin_cmd(plugin, &cli, host, &host.disable_args(plugin))
            } else {
                continue;
            };
            match result {
                Ok(()) => changed_any = true,
                Err(err) => errors.push(format!("{}: {err:#}", host.label())),
            }
        }
        if !changed_any && !errors.is_empty() {
            bail!("toggling {id} failed: {}", errors.join("; "));
        }
        let version =
            installed_plugin_version(plugin).unwrap_or_else(|| PLUGIN_DISPLAY_VERSION.into());
        self.write_tool_receipt(plugin.id, json!({ "version": version, "enabled": enabled }))?;
        Ok(())
    }

    pub fn uninstall_plugin(&self, id: &str) -> Result<()> {
        let plugin = plugin_addon(id).with_context(|| format!("unknown plugin addon: {id}"))?;
        // No receipt means Headroom never installed it. Don't touch the user's
        // plugin config or marketplace registration (which they may own).
        if !self.plugin_receipt_exists(plugin) {
            return Ok(());
        }
        for host in PluginHost::ALL {
            if let Some(cli) = host.cli() {
                let _ = self.run_plugin_cmd(plugin, &cli, host, &host.uninstall_args(plugin));
                let _ =
                    self.run_plugin_cmd(plugin, &cli, host, &host.marketplace_remove_args(plugin));
            }
        }
        let receipt = self.runtime.tools_dir.join(format!("{}.json", plugin.id));
        if receipt.exists() {
            std::fs::remove_file(&receipt)
                .with_context(|| format!("removing {}", receipt.display()))?;
        }
        Ok(())
    }

    fn detect_status(&self, tool_id: &str) -> ToolStatus {
        if let Some(plugin) = plugin_addon(tool_id) {
            let Some(receipt) = self.read_tool_receipt(plugin.id) else {
                return ToolStatus::NotInstalled;
            };
            // Intentionally disabled via the app: the plugin may be gone from
            // hosts that lack a disable verb (Codex), but the receipt means it's
            // still installed -- report Healthy so the card shows Enable, not Install.
            let enabled = receipt
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !enabled {
                return ToolStatus::Healthy;
            }
            // Enabled per our receipt: require it still be registered with a host,
            // so a manual `/plugin` removal surfaces as not-installed.
            return if PluginHost::ALL
                .iter()
                .any(|host| host.plugin_present(plugin))
            {
                ToolStatus::Healthy
            } else {
                ToolStatus::NotInstalled
            };
        }
        let installed_path = self.runtime.tools_dir.join(format!("{tool_id}.json"));
        if installed_path.exists() && self.python_runtime_installed() {
            ToolStatus::Healthy
        } else {
            ToolStatus::NotInstalled
        }
    }
}

/// Plugin addons ship marketplace plugins that both Claude Code and Codex can
/// install through their own `<cli> plugin ...` managers. Their verbs differ
/// (Claude has enable/disable/install/uninstall; Codex only add/remove), so
/// each host carries its own argument vectors.
#[derive(Clone, Copy)]
enum PluginHost {
    ClaudeCode,
    Codex,
}

impl PluginHost {
    const ALL: [PluginHost; 2] = [PluginHost::ClaudeCode, PluginHost::Codex];

    fn label(self) -> &'static str {
        match self {
            PluginHost::ClaudeCode => "Claude Code",
            PluginHost::Codex => "Codex",
        }
    }

    fn cli(self) -> Option<PathBuf> {
        match self {
            PluginHost::ClaudeCode => crate::claude_cli::detect_claude_cli(),
            PluginHost::Codex => crate::claude_cli::detect_codex_cli(),
        }
    }

    fn marketplace_add_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        vec!["plugin", "marketplace", "add", plugin.marketplace]
    }

    fn marketplace_remove_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        vec!["plugin", "marketplace", "remove", plugin.marketplace_name]
    }

    /// Pull the marketplace's newest commit into the host's local snapshot.
    /// Claude calls it `update`, Codex calls it `upgrade`.
    fn marketplace_update_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        match self {
            PluginHost::ClaudeCode => {
                vec!["plugin", "marketplace", "update", plugin.marketplace_name]
            }
            PluginHost::Codex => vec!["plugin", "marketplace", "upgrade", plugin.marketplace_name],
        }
    }

    /// Move an installed plugin onto the refreshed snapshot. Codex has no
    /// update verb; re-adding from the upgraded snapshot is the equivalent.
    fn update_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        match self {
            PluginHost::ClaudeCode => vec!["plugin", "update", plugin.plugin_ref],
            PluginHost::Codex => vec!["plugin", "add", plugin.plugin_ref],
        }
    }

    fn install_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        match self {
            // No `--scope user`: it is Claude Code's default, and CLIs older
            // than the flag reject the whole command with "unknown option".
            PluginHost::ClaudeCode => vec!["plugin", "install", plugin.plugin_ref],
            PluginHost::Codex => vec!["plugin", "add", plugin.plugin_ref],
        }
    }

    fn disable_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        match self {
            PluginHost::ClaudeCode => vec!["plugin", "disable", plugin.plugin_ref],
            PluginHost::Codex => vec!["plugin", "remove", plugin.plugin_ref],
        }
    }

    fn uninstall_args(self, plugin: &PluginAddon) -> Vec<&'static str> {
        match self {
            PluginHost::ClaudeCode => vec!["plugin", "uninstall", plugin.plugin_ref],
            PluginHost::Codex => vec!["plugin", "remove", plugin.plugin_ref],
        }
    }

    fn plugin_present(self, plugin: &PluginAddon) -> bool {
        match self {
            PluginHost::ClaudeCode => claude_plugin_present(plugin),
            PluginHost::Codex => codex_plugin_present(plugin),
        }
    }
}

pub(crate) fn claude_installed_plugins() -> Option<Value> {
    // Not `dirs::home_dir()`: on Windows that reads the profile known folder
    // and ignores `$HOME`, so a redirected home (tests, Git Bash) resolves
    // against the REAL profile. Every OSS-plugin test failed on Windows CI
    // that way -- reading the runner's own ~/.claude and finding no plugin.
    let path = crate::client_adapters::home_dir()
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Claude Code records installs in `~/.claude/plugins/installed_plugins.json`
/// under `plugins["<plugin>@<marketplace>"]` as a non-empty array of install
/// records.
fn claude_plugin_present(plugin: &PluginAddon) -> bool {
    claude_installed_plugins()
        .and_then(|v| v.get("plugins")?.get(plugin.plugin_ref).cloned())
        .and_then(|entry| entry.as_array().map(|installs| !installs.is_empty()))
        .unwrap_or(false)
}

/// Codex records installs in `~/.codex/config.toml` under a
/// `[plugins."<plugin>@<marketplace>"]` table. Keys containing `@` are always
/// quoted, so a header substring match is reliable and avoids a TOML parse
/// dependency (matching how client_adapters edits this file).
fn codex_plugin_present(plugin: &PluginAddon) -> bool {
    let Some(path) = dirs::home_dir().map(|h| h.join(".codex").join("config.toml")) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let header = format!("[plugins.\"{}\"]", plugin.plugin_ref);
    text.lines().any(|line| line.trim_start() == header)
}

/// One serena tool application logs exactly one line containing this marker
/// (`tools_base.py` `_log_tool_application`, stable through pinned v1.7.0).
const SERENA_TOOL_CALL_LOG_MARKER: &str = "; session_id: ";

/// Serena's dashboard API binds the first free port scanning upward from here
/// (`constants.py` 0x5EDA, `dashboard.py` `_find_first_free_port`).
const SERENA_DASHBOARD_BASE_PORT: u16 = 24282;
const SERENA_DASHBOARD_PORT_SCAN: u16 = 4;

/// GET `{base_url}/get_tool_stats` and sum `output_tokens` across tools.
/// Response shape: `{"stats": {tool: {num_times_called, input_tokens,
/// output_tokens}}}`; empty stats yield Some(0), no/invalid responder None.
fn fetch_serena_output_tokens(base_url: &str) -> Option<u64> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .ok()?;
    let value: Value = client
        .get(format!("{base_url}/get_tool_stats"))
        .send()
        .ok()?
        .json()
        .ok()?;
    let stats = value.get("stats")?.as_object()?;
    let mut total: u64 = 0;
    for entry in stats.values() {
        total = total.saturating_add(
            entry
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    }
    Some(total)
}

/// "231 tool calls today, ~48k tokens returned in 2h 14m" — either half may
/// be absent (and the duration within the second), None only when both are.
/// `(calls_line, tokens_line)` — the two independently-optional halves of the
/// serena activity phrasing, shared by the Addons-tab chip and the feed tile.
fn serena_savings_parts(
    calls_today: Option<u64>,
    live: Option<(u64, Option<Duration>)>,
) -> (Option<String>, Option<String>) {
    let calls_line = calls_today.map(|count| {
        if count == 1 {
            "1 tool call today".to_string()
        } else {
            format!("{count} tool calls today")
        }
    });
    let tokens_line = match live {
        Some((tokens, Some(age))) => Some(format!(
            "~{} tokens returned in {}",
            compact_token_count(tokens),
            compact_duration(age)
        )),
        // No session age (e.g. serena running from a non-managed install):
        // still name the window, or the figure reads as all-time.
        Some((tokens, None)) => Some(format!(
            "~{} tokens returned this session",
            compact_token_count(tokens)
        )),
        None => None,
    };
    (calls_line, tokens_line)
}

fn serena_savings_label(
    calls_today: Option<u64>,
    live: Option<(u64, Option<Duration>)>,
) -> Option<String> {
    let (calls_line, tokens_line) = serena_savings_parts(calls_today, live);
    let parts: Vec<String> = [calls_line, tokens_line].into_iter().flatten().collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Parse `ps` etime, `[[dd-]hh:]mm:ss`.
fn parse_ps_etime(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    let (days, clock) = match raw.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, raw),
    };
    let fields: Vec<&str> = clock.split(':').collect();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [m, s] => (0, m.parse::<u64>().ok()?, s.parse::<u64>().ok()?),
        [h, m, s] => (
            h.parse::<u64>().ok()?,
            m.parse::<u64>().ok()?,
            s.parse::<u64>().ok()?,
        ),
        _ => return None,
    };
    Some(Duration::from_secs(
        ((days * 24 + hours) * 60 + minutes) * 60 + seconds,
    ))
}

/// Oldest `start-mcp-server` session matching our entrypoint in
/// `ps -axww -o etime=,args=` output.
fn oldest_serena_session_age(ps_output: &str, entrypoint_marker: &str) -> Option<Duration> {
    ps_output
        .lines()
        .filter_map(|line| {
            let (etime, argv) = line.trim_start().split_once(char::is_whitespace)?;
            (argv.contains(entrypoint_marker) && argv.contains("start-mcp-server"))
                .then(|| parse_ps_etime(etime))
                .flatten()
        })
        .max()
}

fn compact_duration(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    if minutes >= 60 {
        let hours = minutes / 60;
        let rest = minutes % 60;
        if rest == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {rest}m")
        }
    } else if minutes == 0 {
        "under a minute".to_string()
    } else {
        format!("{minutes}m")
    }
}

fn compact_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Count tool-call lines across every `*.txt` log in one day's serena log dir.
/// None when the dir is missing or holds no calls, so the chip stays hidden.
// ponytail: a tool result that itself quotes serena log text would inflate the
// count; switch to per-line prefix matching if that ever shows up in practice.
fn count_serena_tool_calls_in_dir(dir: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut count: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        count += String::from_utf8_lossy(&bytes)
            .matches(SERENA_TOOL_CALL_LOG_MARKER)
            .count() as u64;
    }
    (count > 0).then_some(count)
}

/// Serena writes a `.serena/` dir (config, cache, memories) into the root of
/// every project it is pointed at. It self-ignores the noisy parts but leaves
/// `project.yml` and its own `.gitignore` tracked, so every repo the user
/// touches picks up two unrequested files.
const SERENA_GITIGNORE_MARKER: &str =
    "# Headroom-managed: serena writes .serena/ into every project root";
const SERENA_GITIGNORE_PATTERN: &str = ".serena/";

/// Git reads `$XDG_CONFIG_HOME/git/ignore` (default `~/.config/git/ignore`)
/// when `core.excludesfile` is unset, so the pattern can be added without
/// rewriting the user's global git config.
fn global_git_excludes_path() -> Option<PathBuf> {
    let configured = crate::proc::command("git")
        .args(["config", "--global", "--get", "core.excludesfile"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|path| !path.is_empty());
    let home = dirs::home_dir()?;
    Some(match configured {
        Some(path) => match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(path),
        },
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("git")
            .join("ignore"),
    })
}

/// `None` when the file already says what it should - nothing to write.
/// Removal only strips the block Headroom added: a hand-written `.serena/`
/// with no marker above it is the user's, not ours.
fn apply_serena_gitignore(existing: &str, present: bool) -> Option<String> {
    if present {
        if existing
            .lines()
            .any(|line| line.trim() == SERENA_GITIGNORE_PATTERN)
        {
            return None;
        }
        let mut updated = existing.to_string();
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(SERENA_GITIGNORE_MARKER);
        updated.push('\n');
        updated.push_str(SERENA_GITIGNORE_PATTERN);
        updated.push('\n');
        return Some(updated);
    }
    if !existing.lines().any(|line| line == SERENA_GITIGNORE_MARKER) {
        return None;
    }
    let mut updated = String::new();
    let mut after_marker = false;
    for line in existing.lines() {
        if line == SERENA_GITIGNORE_MARKER {
            after_marker = true;
            continue;
        }
        if after_marker {
            after_marker = false;
            if line.trim() == SERENA_GITIGNORE_PATTERN {
                continue;
            }
        }
        updated.push_str(line);
        updated.push('\n');
    }
    Some(updated)
}

/// Best-effort: a user with an unwritable git config still gets a working
/// serena, so failures are logged at info (never Sentry-escalated) and the
/// install continues.
fn set_serena_global_gitignore(present: bool) {
    let Some(path) = global_git_excludes_path() else {
        return;
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let Some(updated) = apply_serena_gitignore(&existing, present) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            log::info!("serena: creating {} failed: {err:#}", parent.display());
            return;
        }
    }
    if let Err(err) = crate::client_adapters::atomic_write(&path, updated.as_bytes()) {
        log::info!("serena: updating {} failed: {err:#}", path.display());
    } else {
        log::info!(
            "serena: {} .serena/ in {}",
            if present { "ignoring" } else { "un-ignoring" },
            path.display()
        );
    }
}

/// Serena 1.7 defaults its dashboard "interface" on macOS to a menu-bar tray
/// app (`DashboardManager.Mode.from_platform` returns tray_manager on Darwin).
/// `--open-web-dashboard False` only suppresses the browser tab, and there is
/// no CLI flag or env var for the interface, so the config key is the only
/// lever. `browser` keeps the dashboard HTTP API up (our savings chip polls
/// it) while spawning nothing visible. An explicit user-chosen interface is
/// left alone; only the unset platform default is replaced. `None` = nothing
/// to write.
fn apply_serena_dashboard_interface(existing: &str) -> Option<String> {
    const KEY: &str = "web_dashboard_interface:";
    const WANT: &str = "web_dashboard_interface: browser";
    let mut out = String::new();
    let mut found = false;
    for line in existing.lines() {
        if !found && line.starts_with(KEY) {
            found = true;
            let value = line[KEY.len()..].split('#').next().unwrap_or("").trim();
            match value {
                "" | "null" | "~" => out.push_str(WANT),
                _ => return None,
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !found {
        if existing.trim().is_empty() {
            // A file serena has never written: `projects` is the one key its
            // loader hard-requires; everything else falls back to defaults
            // (serena fills the rest in on its next config migration save).
            out.push_str("projects: []\n");
        }
        out.push_str(WANT);
        out.push('\n');
    }
    Some(out)
}

/// Best-effort like the gitignore nudge above: serena works either way, the
/// tray icon is just noise, so failures log at info and never block install.
fn set_serena_browser_dashboard() {
    let home_dir = std::env::var_os("SERENA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".serena")));
    let Some(dir) = home_dir else {
        return;
    };
    let path = dir.join("serena_config.yml");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            // An existing file we cannot read must not be clobbered with a
            // minimal one - that would drop the user's projects list.
            log::info!("serena: reading {} failed: {err:#}", path.display());
            return;
        }
    };
    let Some(updated) = apply_serena_dashboard_interface(&existing) else {
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::info!("serena: creating {} failed: {err:#}", dir.display());
        return;
    }
    if let Err(err) = crate::client_adapters::atomic_write(&path, updated.as_bytes()) {
        log::info!("serena: updating {} failed: {err:#}", path.display());
    } else {
        log::info!(
            "serena: set web_dashboard_interface to browser in {}",
            path.display()
        );
    }
}

fn installed_plugin_version(plugin: &PluginAddon) -> Option<String> {
    let plugins = claude_installed_plugins()?;
    let installs = plugins.get("plugins")?.get(plugin.plugin_ref)?.as_array()?;
    installs
        .first()?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// Claude Code ≥2.x stores user-scope MCP servers in `~/.claude.json` under
/// `mcpServers.<name>`. The legacy `~/.claude/mcp.json` path written by our
/// Python CLI's fallback branch is ignored. Reading the file Claude Code
/// actually reads is the only reliable way to confirm the registration
/// landed where `/mcp` and `claude mcp list` will see it.
fn claude_code_has_headroom_mcp_server() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".claude.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    value
        .get("mcpServers")
        .and_then(|v| v.get("headroom"))
        .is_some()
}

/// Writes the headroom MCP server entry directly to `~/.claude.json`.
/// Used when `claude mcp add` is unavailable (e.g. bare GUI PATH). Preserves
/// all existing keys; only merges `mcpServers.headroom`.
fn write_headroom_to_claude_json(entrypoint: &Path, proxy_url: &str) -> Result<()> {
    let Some(home) = dirs::home_dir() else {
        anyhow::bail!("home directory not available");
    };
    write_headroom_to_claude_json_at(&home.join(".claude.json"), entrypoint, proxy_url)
}

fn write_headroom_to_claude_json_at(path: &Path, entrypoint: &Path, proxy_url: &str) -> Result<()> {
    let desired = json!({
        "command": entrypoint,
        "args": ["mcp", "serve"],
        "env": { "HEADROOM_PROXY_URL": proxy_url },
    });

    let modified_time = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();

    // ~/.claude.json holds OAuth state and per-project settings, and Claude
    // Code rewrites it frequently — often while this runs (bootstrap,
    // upgrade, requirements repair). Two defenses against reverting a
    // concurrent Claude Code write with our stale snapshot: skip the publish
    // entirely when our entry is already present and correct (the common
    // case on every repair), and re-check the file's mtime just before the
    // rename, retrying the whole read-modify-write if it moved.
    const MAX_ATTEMPTS: u32 = 3;
    for attempt in 0..MAX_ATTEMPTS {
        let seen_modified = modified_time(path);

        // A read or parse failure (e.g. a mid-write partial file) must never
        // degrade to an empty object, or the write below replaces the user's
        // entire config with just our entry.
        let mut config: Value = if path.exists() {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                json!({})
            } else {
                serde_json::from_slice(&bytes).with_context(|| {
                    format!(
                        "parsing {} failed; refusing to overwrite potentially valid user config",
                        path.display()
                    )
                })?
            }
        } else {
            json!({})
        };

        if config
            .get("mcpServers")
            .and_then(|servers| servers.get("headroom"))
            == Some(&desired)
        {
            return Ok(());
        }

        let root = config
            .as_object_mut()
            .context("~/.claude.json root is not a JSON object")?;

        root.entry("mcpServers")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("~/.claude.json mcpServers is not a JSON object")?
            .insert("headroom".into(), desired.clone());

        let _ = crate::client_adapters::backup_if_exists(path)?;

        // Publish atomically (tmp + rename) so a crash mid-write can never
        // leave a truncated ~/.claude.json behind.
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".headroom-tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, serde_json::to_vec_pretty(&config)?)
            .with_context(|| format!("writing {}", tmp.display()))?;

        if modified_time(path) != seen_modified && attempt + 1 < MAX_ATTEMPTS {
            // Claude Code wrote while we worked — merge against the new
            // contents instead of reverting them.
            let _ = std::fs::remove_file(&tmp);
            continue;
        }
        return std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} into place", tmp.display()));
    }
    unreachable!("loop always returns")
}

fn is_local_proxy_reachable() -> bool {
    // Check headroom's actual backend port, not the intercept port (6767),
    // because the intercept starts before headroom and would always be reachable.
    let address: SocketAddr = ([127, 0, 0, 1], backend_port::get()).into();
    TcpStream::connect_timeout(&address, Duration::from_millis(180)).is_ok()
}

enum PortState {
    Free,
    HeadroomRunning,
    ForeignOccupant(String),
}

/// Occupant string used when the port is held but no owning pid can be found.
/// `diagnose_proxy_port_settled` keys off this exact shape, so it is a const
/// rather than three loose copies of the same literal.
const UNKNOWN_OCCUPANT: &str = "unknown process";

fn diagnose_proxy_port(port: u16) -> PortState {
    // If we can bind the port, nothing is there.
    if TcpListener::bind(("127.0.0.1", port)).is_ok() {
        return PortState::Free;
    }

    // Port is held. Probe it: headroom's proxy speaks HTTP and, for an
    // unrecognized path, responds with an HTTP status line. A foreign
    // non-HTTP service (SSH, Redis, etc.) will not.
    let headroom_like = probe_headroom_http(port, Duration::from_millis(400));
    if headroom_like {
        // "Speaks HTTP" alone is not identity. HeadroomRunning occupants get
        // killed by the reclaim path, so verify the pid's argv actually looks
        // like our managed backend — an unrelated local HTTP server (dev
        // server, docker forward) squatting the port must route to the
        // foreign fallback-port path instead of being SIGKILLed.
        match listener_process(port) {
            Some((_, pid)) if pid_is_headroom_backend(pid) => PortState::HeadroomRunning,
            Some((command, pid)) => PortState::ForeignOccupant(format!("{command} pid {pid}")),
            None => PortState::ForeignOccupant(UNKNOWN_OCCUPANT.into()),
        }
    } else {
        PortState::ForeignOccupant(listener_detail(port).unwrap_or_else(|| UNKNOWN_OCCUPANT.into()))
    }
}

/// [`diagnose_proxy_port`], but waits out a socket the process we just replaced
/// left closing.
///
/// An updater relaunch tears down the old backend and starts the new one
/// immediately. For a few seconds the kernel still holds :6768 while nothing
/// accepts on it and no pid owns it -- a shape `diagnose_proxy_port` can only
/// read as a foreign occupant, so the backend abandoned its default port for
/// 6770 on every update (RUST-7F: one event per release, i.e. one per update,
/// not one per launch). Only that exact unowned shape is waited on; a named
/// occupant is a real conflict and falls back immediately.
fn diagnose_proxy_port_settled(port: u16) -> PortState {
    settle_unowned_port(|| diagnose_proxy_port(port), 6, Duration::from_millis(500))
}

/// Who holds `port`, as a short label for boot-validation failure diagnostics.
///
/// `proxy_port_bound=true` on its own cannot tell "our freshly spawned child
/// is wedged and never answered /livez" apart from "a foreign process squats
/// the port so the child never got it": both burn the full boot budget and
/// look identical in Sentry (RUST-7Y, RUST-4A). Called once, on the failure
/// path, before `stop_headroom` tears the occupant down.
pub(crate) fn describe_proxy_port_occupant(port: u16) -> String {
    match diagnose_proxy_port(port) {
        PortState::Free => "free".to_string(),
        PortState::HeadroomRunning => "headroom".to_string(),
        PortState::ForeignOccupant(detail) => format!("foreign: {detail}"),
    }
}

/// `Some(detail)` when `port` is held by a NAMED process that is not our
/// backend. `None` for free, ours, or held-by-nobody -- the unowned shape is
/// the updater-relaunch race `diagnose_proxy_port_settled` waits out
/// (RUST-7F), so a fail-fast caller must never act on it.
pub(crate) fn proxy_port_held_by_named_foreign(port: u16) -> Option<String> {
    named_foreign_occupant(diagnose_proxy_port(port))
}

fn named_foreign_occupant(state: PortState) -> Option<String> {
    match state {
        PortState::ForeignOccupant(detail) if detail != UNKNOWN_OCCUPANT => Some(detail),
        _ => None,
    }
}

/// Re-`diagnose` while the port reads as held-by-nobody, up to `attempts`
/// times. Split from [`diagnose_proxy_port_settled`] so the retry rule is
/// testable without binding real sockets.
fn settle_unowned_port(
    mut diagnose: impl FnMut() -> PortState,
    attempts: usize,
    pause: Duration,
) -> PortState {
    let mut last = diagnose();
    for _ in 1..attempts.max(1) {
        match last {
            PortState::ForeignOccupant(ref detail) if detail == UNKNOWN_OCCUPANT => {
                std::thread::sleep(pause);
                last = diagnose();
            }
            settled => return settled,
        }
    }
    last
}

/// True when `pid`'s full command line looks like Headroom's managed backend
/// (venv python under the Headroom app-support tree, or anything
/// headroom-branded). Guards port reclaim from killing an unrelated process
/// that merely answers HTTP on our port.
fn pid_is_headroom_backend(pid: u32) -> bool {
    // Windows has no `ps`, and `tasklist` reports only the image name, which is
    // a bare `python.exe` for every venv on the machine. The executable PATH is
    // both obtainable and a stricter claim: anything running out of our managed
    // runtime is ours by construction, whatever its argv says.
    #[cfg(windows)]
    {
        let Ok(output) = crate::proc::command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).Path"),
            ])
            .output()
        else {
            return false;
        };
        let path = String::from_utf8_lossy(&output.stdout);
        let runtime_dir =
            ManagedRuntime::bootstrap_root(&crate::storage::app_data_dir()).runtime_dir;
        return exe_path_is_under(&path, &runtime_dir);
    }
    #[cfg(not(windows))]
    {
        let Ok(output) = crate::proc::command("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
        else {
            return false;
        };
        let argv = String::from_utf8_lossy(&output.stdout).to_lowercase();
        // A bare "headroom" substring also matches unrelated dev processes whose
        // path merely contains it (e.g. `python /Users/x/headroom/serve.py 6768`).
        // Require the `proxy` subcommand as well: every version of the managed
        // backend runs as `... headroom proxy ...` (or `-m headroom.proxy.server`),
        // so this still recognizes old orphans the upgrade path must reclaim while
        // excluding a random headroom-pathed process. Only ever consulted for the
        // exact port being reclaimed, so the blast radius is one port either way.
        argv.contains("headroom") && argv.contains("proxy")
    }
}

/// True when `exe_path` sits inside `runtime_dir`.
///
/// The Windows half of [`pid_is_headroom_backend`], split out so the rule is
/// testable without a live pid. Match the runtime DIRECTORY rather than
/// substrings: on Windows the backend runs as the base interpreter
/// (`runtime\python\python.exe`), NOT the venv one
/// (`runtime\venv\Scripts\python.exe`) that `managed_python()` composes, so a
/// first cut of this requiring "venv" in the path vouched for neither and the
/// identity gate could never pass. Both layouts live under `runtime`.
#[cfg_attr(not(windows), allow(dead_code))]
fn exe_path_is_under(exe_path: &str, runtime_dir: &Path) -> bool {
    let exe = exe_path.trim();
    // Empty means the pid was gone by the time PowerShell looked, or it is a
    // process whose image we may not read. Either way: not provably ours.
    if exe.is_empty() {
        return false;
    }
    let Some(runtime) = runtime_dir.to_str().filter(|r| !r.is_empty()) else {
        return false;
    };
    // Windows paths are case-insensitive and PowerShell need not echo our
    // casing back.
    let exe = exe.to_lowercase();
    let mut prefix = runtime.to_lowercase();
    // The prefix has to end on a separator, or `...\runtime-old\python.exe`
    // would pass for `...\runtime` and the kill would land on a stranger.
    if !prefix.ends_with('\\') && !prefix.ends_with('/') {
        prefix.push('\\');
    }
    exe.starts_with(&prefix)
}

fn probe_headroom_http(port: u16, timeout: Duration) -> bool {
    use std::io::{Read, Write};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    if stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 16];
    match stream.read(&mut buf) {
        Ok(n) if n >= 5 => buf[..5].eq_ignore_ascii_case(b"HTTP/"),
        _ => false,
    }
}

/// The process listening on `port`, as `(command, pid)`.
///
/// lsof lives in /usr/sbin on macOS but /usr/bin on Linux, and Debian-family
/// images often omit it entirely, so try both paths and then fall back to `ss`
/// (iproute2, present on every Linux we ship to).
///
/// `None` means "could not identify the listener" — never "nothing is
/// listening" and never "it is not ours". Both the port-reclaim kill path and
/// the stale-argv check hang off this, so neither may treat None as evidence.
pub(crate) fn listener_process(port: u16) -> Option<(String, u32)> {
    for lsof in ["/usr/sbin/lsof", "/usr/bin/lsof"] {
        // Only `-iTCP:{port}` — a bare `-iTCP` here would OR with the port
        // selector (lsof ORs `-i` options) and match every listening socket on
        // the machine, so the first row would be an unrelated daemon.
        let Ok(output) = crate::proc::command(lsof)
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
            .output()
        else {
            continue; // not installed at this path
        };
        if !output.status.success() {
            continue;
        }
        if let Some(found) = parse_lsof_listener(&String::from_utf8_lossy(&output.stdout)) {
            return Some(found);
        }
    }
    #[cfg(target_os = "linux")]
    return ss_listener(port);
    #[cfg(windows)]
    return windows_listener(port);
    #[cfg(not(any(target_os = "linux", windows)))]
    return None;
}

/// Windows has neither `lsof` nor `ss`, so `listener_process` returned `None`
/// for every port -- which is why `diagnose_proxy_port` could only ever say
/// "unknown process" there (RUST-7F) and why `reclaim_orphan_proxy` bailed
/// before it could reclaim anything. `netstat` and `tasklist` ship with every
/// Windows since XP and need no elevation for our own processes.
#[cfg(windows)]
fn windows_listener(port: u16) -> Option<(String, u32)> {
    let output = crate::proc::command("netstat")
        .args(["-ano"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pid = parse_netstat_listener(&String::from_utf8_lossy(&output.stdout), port)?;

    // The image name is cosmetic (it goes into the occupant string); a pid we
    // could not name is still a pid worth reporting and gating a kill on.
    let image = crate::proc::command("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_tasklist_image(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_else(|| "unnamed process".to_string());
    Some((image, pid))
}

/// The pid LISTENING on `port` in `netstat -ano` output.
///
/// Matches the port exactly rather than by suffix: `:16768` ends with `6768`,
/// and picking that row would point a kill at an unrelated process.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netstat_listener(text: &str, port: u16) -> Option<u32> {
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Proto, Local Address, Foreign Address, State, PID
        if fields.len() < 5 || !fields[0].eq_ignore_ascii_case("TCP") {
            return None;
        }
        if !fields[3].eq_ignore_ascii_case("LISTENING") {
            return None;
        }
        // rsplit: IPv6 rows are `[::1]:6768`, so only the last colon separates
        // the port.
        let (_, found) = fields[1].rsplit_once(':')?;
        (found.parse::<u16>().ok()? == port).then(|| fields[4].parse().ok())?
    })
}

/// The image name from one `tasklist /NH /FO CSV` row (`"python.exe","123",...`).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_tasklist_image(text: &str) -> Option<String> {
    let row = text.lines().find(|line| line.starts_with('"'))?;
    let name = row.strip_prefix('"')?.split('"').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// `(command, pid)` from the first data row of `lsof -nP -iTCP -sTCP:LISTEN`,
/// whose columns start `COMMAND PID`. Row 0 is the header.
fn parse_lsof_listener(text: &str) -> Option<(String, u32)> {
    let mut fields = text.lines().nth(1)?.split_whitespace();
    let command = fields.next()?.to_string();
    let pid = fields.next()?.parse().ok()?;
    Some((command, pid))
}

/// lsof-less fallback. `ss` only fills the `users:` field for processes the
/// caller owns, which covers the case that matters — our own backend. Someone
/// else's daemon on our port stays "unknown process", which is the honest
/// answer and routes to the fallback port instead of the kill path.
#[cfg(target_os = "linux")]
fn ss_listener(port: u16) -> Option<(String, u32)> {
    let output = crate::proc::command("ss").arg("-ltnp").output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ss_listener(&String::from_utf8_lossy(&output.stdout), port)
}

/// Pick the LISTEN row whose local address ends in `:{port}` and pull
/// `(command, pid)` from its `users:(("python",pid=1234,fd=7))` field.
///
/// Filtered here rather than with an `ss` filter expression: the column layout
/// is stable across iproute2 versions, and a filter expression we got subtly
/// wrong would hand a *different* process's pid to the kill path.
// Only called on Linux, but compiled everywhere so the parser stays testable.
#[allow(dead_code)]
fn parse_ss_listener(text: &str, port: u16) -> Option<(String, u32)> {
    let suffix = format!(":{port}");
    let row = text.lines().find(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some("LISTEN") && fields.nth(2).is_some_and(|addr| addr.ends_with(&suffix))
    })?;
    let users = row.split("users:((").nth(1)?;
    let command = users.strip_prefix('"')?.split('"').next()?.to_string();
    let pid = users
        .split("pid=")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((command, pid))
}

/// `listener_process` formatted as the `"cmd pid 1234"` detail string the
/// port-conflict marker and its bail messages carry.
fn listener_detail(port: u16) -> Option<String> {
    listener_process(port).map(|(command, pid)| format!("{command} pid {pid}"))
}

/// Extract the numeric pid from a `"cmd pid 1234"` string returned by
/// [`listener_detail`]. Returns None for the `"unknown process"` placeholder
/// or any unparseable shape. Companion to `port_conflict::parse_occupant`,
/// which works on the full bail string instead of the occupant detail.
fn parse_pid_from_lsof_detail(detail: &str) -> Option<u32> {
    let idx = detail.rfind(" pid ")?;
    detail[idx + " pid ".len()..].trim().parse().ok()
}

/// Bail message when a previous (still-alive) headroom proxy holds the port.
/// Extracted as a function so the exact format is testable against
/// `port_conflict::is_port_conflict` and `state::classify_startup_error`.
fn format_already_running_bail(port: u16) -> String {
    format!(
        "headroom proxy already running on port {port} (likely a stale process from a prior session). \
         Run `lsof -iTCP:{port} -sTCP:LISTEN` to find and kill it, then retry."
    )
}

/// True when `/readyz` on the backend `port` answers with a 2xx — i.e. a
/// genuinely healthy headroom proxy is serving there. Used to avoid killing a
/// live backend during port reclaim. Short timeout: a hung orphan won't answer
/// in time and a healthy one answers in milliseconds.
fn probe_backend_readyz_ok(port: u16) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
    else {
        return false;
    };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/readyz")).send(),
        Ok(resp) if resp.status().is_success()
    )
}

/// Poll until `port` is bindable or `timeout` elapses. Returns true once the
/// port is free. A killed listener's socket is released as soon as the owning
/// process dies, so this normally returns within a couple of poll intervals.
fn wait_for_port_free(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Reclaim `port` from an orphaned headroom proxy left behind by a prior
/// session. We only get here from the `HeadroomRunning` spawn pre-flight, which
/// is unreachable when a healthy proxy is up (its `/readyz` would have
/// satisfied `is_headroom_proxy_reachable` and short-circuited
/// `ensure_headroom_running`). Still, re-confirm health on the backend port
/// directly before killing — if it answers 2xx the backend is live (e.g. the
/// 6767 intercept is wedged while 6768 is fine) and we leave it alone. On any
/// failure to reclaim (no pid, refuses to die, healthy) we fall back to the
/// original bail so the caller's classification and user guidance are
/// unchanged.
///
/// `force_unhealthy_too`: during upgrade boot validation the orphan on 6768 is
/// the *old* version we are replacing — a still-healthy old worker (left when
/// `stop_headroom`'s argv pattern-kill missed the real socket holder) must be
/// killed anyway, or the new venv can't bind and the upgrade rolls back as
/// `not_started`. When set, skip the readyz health guard and reclaim regardless.
/// Signal a single pid, gracefully or forcefully.
///
/// Deliberately not `state::terminate_process_tree`: that one signals a process
/// GROUP on unix and refuses any pid we did not spawn, which is exactly what an
/// orphan from a previous app instance is. Callers must have passed
/// `pid_is_headroom_backend` first.
fn kill_pid(pid: u32, force: bool) {
    #[cfg(windows)]
    {
        // `/T` takes the subtree with it: the backend spawns helpers, and a
        // survivor holds the port just as well as its parent did.
        let mut command = crate::proc::command("taskkill");
        command.args(["/PID", &pid.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        let _ = command.status();
    }
    #[cfg(not(windows))]
    {
        let mut command = crate::proc::command("/bin/kill");
        if force {
            command.arg("-KILL");
        }
        crate::state::note_app_kill(
            "kill_pid",
            format!("{} pid {pid}", if force { "-KILL" } else { "-TERM" }),
        );
        let _ = command.arg(pid.to_string()).status();
    }
}

fn reclaim_orphan_proxy(port: u16, force_unhealthy_too: bool) -> Result<()> {
    if !force_unhealthy_too && probe_backend_readyz_ok(port) {
        bail!("{}", format_already_running_bail(port));
    }
    let Some((_, pid)) = listener_process(port) else {
        bail!("{}", format_already_running_bail(port));
    };
    // Belt-and-braces identity gate (diagnose_proxy_port already filters, but
    // the upgrade flow reaches here with force set): never kill a pid that
    // doesn't look like our own backend.
    if !pid_is_headroom_backend(pid) {
        bail!("{}", format_already_running_bail(port));
    }

    log::warn!("[backend_port] reclaiming orphaned headroom proxy pid {pid} on port {port}");
    kill_pid(pid, false);
    if !wait_for_port_free(port, Duration::from_secs(3)) {
        kill_pid(pid, true);
        if !wait_for_port_free(port, Duration::from_secs(2)) {
            bail!("{}", format_already_running_bail(port));
        }
    }

    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "orphan_proxy_reclaimed");
            scope.set_extra("port", port.into());
            scope.set_extra("occupant_pid", pid.into());
            // Fixed fingerprint: the pid and port in the message opened one
            // issue per reclaim (RUST-88, RUST-CQ) for a single condition.
            scope.set_fingerprint(Some(&["orphan_proxy_reclaimed"]));
        },
        || {
            sentry::capture_message(
                &format!("orphan_proxy_reclaimed: killed pid {pid} holding port {port}"),
                sentry::Level::Info,
            );
        },
    );
    Ok(())
}

/// Kill a stranded prior Headroom DESKTOP instance holding `port` (the
/// intercept port). A restart or updater relaunch can leave the old
/// headroom-desktop process holding 6767 while no longer serving on it, and
/// nothing else ever reclaims that port -- the intercept's bind loop just
/// retries forever, so the app stays "not hooked up" until the user kills
/// the process by hand (RUST-7M; kill-and-restart is the confirmed field
/// fix). Identity gate: the listener must be running THIS same executable
/// (exact current_exe path match) and not be this process. A foreign
/// squatter or a reserved range never matches, and nothing is killed.
/// Returns true when a kill was issued and the port came free.
pub(crate) fn reclaim_stranded_intercept_holder(port: u16) -> bool {
    let Some((_, pid)) = listener_process(port) else {
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    if !pid_is_headroom_desktop_twin(pid) {
        return false;
    }
    log::warn!(
        "[proxy_intercept] reclaiming stranded Headroom desktop instance pid {pid} on port {port}"
    );
    kill_pid(pid, false);
    if !wait_for_port_free(port, Duration::from_secs(3)) {
        kill_pid(pid, true);
        if !wait_for_port_free(port, Duration::from_secs(2)) {
            return false;
        }
    }
    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "intercept_stranded_instance_reclaimed");
            scope.set_extra("port", port.into());
            scope.set_extra("occupant_pid", pid.into());
        },
        || {
            sentry::capture_message(
                &format!(
                    "intercept_stranded_instance_reclaimed: killed pid {pid} holding port {port}"
                ),
                sentry::Level::Info,
            );
        },
    );
    true
}

/// True when `pid` runs the same executable as this process. The strictest
/// identity claim available: an updater-stranded old instance runs from the
/// exact same install path as us, while any foreign process cannot.
fn pid_is_headroom_desktop_twin(pid: u32) -> bool {
    let Some(me) = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
    else {
        return false;
    };
    #[cfg(windows)]
    let theirs = {
        let Ok(output) = crate::proc::command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).Path"),
            ])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    #[cfg(not(windows))]
    let theirs = {
        let Ok(output) = crate::proc::command("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    exe_identity_matches(&theirs, &me)
}

/// True when `listener_exe` -- a bare exe path (Windows `Get-Process .Path`)
/// or an argv line (unix `ps -o command=`) -- names exactly `my_exe`.
/// Case-insensitive (Windows and default macOS filesystems are), and the
/// match must end at a boundary so `...\Headroom-old.exe` cannot pass for
/// `...\Headroom.exe`.
fn exe_identity_matches(listener_exe: &str, my_exe: &str) -> bool {
    let theirs = listener_exe.trim().to_lowercase();
    let mine = my_exe.trim().to_lowercase();
    if mine.is_empty() || !theirs.starts_with(&mine) {
        return false;
    }
    match theirs[mine.len()..].chars().next() {
        None => true,        // exact path match
        Some(c) => c == ' ', // argv: path followed by arguments
    }
}

/// Bail message when 6768 is foreign-held AND every port in the fallback
/// range is also taken. Must contain `"is occupied by a non-headroom process"`
/// so `port_conflict::is_port_conflict` continues to match, and the
/// `(occupant)` parenthetical so `port_conflict::parse_occupant` can extract
/// the cmd/pid for the persistent-conflict marker.
fn format_all_foreign_bail(default_port: u16, occupant: &str, range: (u16, u16)) -> String {
    let (start, end) = range;
    format!(
        "port {default_port} is occupied by a non-headroom process ({occupant}) and fallback ports {start}-{end} are also unavailable; cannot start proxy. \
         Reboot to clear stuck listeners, then relaunch Headroom."
    )
}

/// The lines that name the fatal condition behind a faulthandler dump. The
/// all-threads dump that follows them runs well past the 80-line tail Sentry
/// gets (RUST-C7: seven idle threads plus a 100-frame import chain), so the one
/// line that says access violation / Aborted / OMP error was exactly what got
/// cut. Keeps the last few matches: the log is append-only across attempts and
/// the newest dump is the one being reported.
fn fatal_header_lines(path: &Path) -> Vec<String> {
    const MARKERS: &[&str] = &[
        "Fatal Python error",
        "Windows fatal exception",
        "OMP: Error",
        "Current thread 0x",
    ];
    const KEEP: usize = 6;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if MARKERS.iter().any(|m| line.contains(m)) {
            if kept.len() == KEEP {
                kept.pop_front();
            }
            kept.push_back(redact_sensitive(&line));
        }
    }
    kept.into_iter().collect()
}

/// The 80-line tail a startup failure carries to Sentry, led by the fatal
/// marker lines when the log holds a crash dump (see [`fatal_header_lines`]).
fn crash_log_excerpt(path: &Path) -> String {
    let tail = tail_log_file(path, 80);
    let header = fatal_header_lines(path);
    if header.is_empty() {
        tail
    } else {
        format!(
            "--- fatal markers ---\n{}\n--- tail ---\n{tail}",
            header.join("\n")
        )
    }
}

pub(crate) fn tail_log_file(path: &Path, max_lines: usize) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut lines: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(max_lines);
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if lines.len() == max_lines {
            lines.pop_front();
        }
        lines.push_back(redact_sensitive(&line));
    }
    lines.into_iter().collect::<Vec<_>>().join("\n")
}

/// Strip Anthropic API keys and bearer tokens from log content before it gets
/// handed to Sentry. Without this, Sentry's default PII scrubber sees one
/// `sk-ant-…` and replaces the entire `proxy_log_tail` field with `[Filtered]`,
/// which is the single most diagnostic field in `proxy_unreachable_post_boot`.
/// Pre-redact so the rest of the line survives the scrubber.
pub(crate) fn redact_sensitive(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &line[i..];
        if let Some(consumed) = match_redactable(rest) {
            out.push_str("[REDACTED]");
            i += consumed;
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// If `rest` starts with a redactable token, return the byte length to skip.
fn match_redactable(rest: &str) -> Option<usize> {
    if let Some(after) = rest.strip_prefix("sk-ant-") {
        let token_len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
            .count();
        return Some("sk-ant-".len() + token_len);
    }
    for prefix in ["Bearer ", "bearer "] {
        if let Some(after) = rest.strip_prefix(prefix) {
            let token_len = after
                .bytes()
                .take_while(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(*b, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
                })
                .count();
            if token_len >= 8 {
                return Some(prefix.len() + token_len);
            }
        }
    }
    None
}

/// Newest `headroom-proxy*.log` in the logs directory, if any.
pub(crate) fn newest_proxy_log_path(logs_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(logs_dir).ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("headroom-proxy") || !name_str.ends_with(".log") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                let path = entry.path();
                newest = Some(match newest {
                    Some((prev_time, prev_path)) if prev_time > mtime => (prev_time, prev_path),
                    _ => (mtime, path),
                });
            }
        }
    }
    newest.map(|(_, p)| p)
}

fn headroom_python_startup_args() -> Vec<String> {
    // The `python -m headroom.proxy.server` argparse does NOT define the learn
    // flags (--learn, --no-memory-tools, --no-memory-context, --memory-db-path);
    // those live only on the `headroom proxy` click entrypoint. Passing them
    // here makes argparse exit 2, so the fallback would always fail and mask
    // the real entrypoint failure under spurious noise. Keep this variant to
    // server-supported flags only.
    vec![
        "-m".to_string(),
        "headroom.proxy.server".to_string(),
        "--port".to_string(),
        headroom_proxy_port(),
        "--no-http2".to_string(),
        "--log-messages".to_string(),
    ]
}

/// The `headroom proxy` click entrypoint only defines `--no-http2` from
/// 0.28.0 (upstream e06b6167); 0.26.0/0.27.0 exit 2 with "No such option",
/// which made boot validation on the 0.26.0 fallback runtime fail every
/// attempt and time out (Sentry RUST-4A). Unknown/unparseable version means
/// the receipt is from a current install, so assume the pinned (>= 0.28.0)
/// runtime and keep the flag.
fn runtime_supports_no_http2(installed_version: Option<&str>) -> bool {
    let Some(version) = installed_version else {
        return true;
    };
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= (0, 28),
        _ => true,
    }
}

/// Opt-in restore of `--no-ccr`, for reverting the 0.9.6 CCR re-enable without
/// a rebuild. Empty unless the app was launched with a truthy
/// `HEADROOM_DESKTOP_NO_CCR`; falsey spellings mean "leave CCR on" rather than
/// "the variable is present, therefore off".
fn desktop_forces_no_ccr() -> bool {
    desktop_forces_no_ccr_from(std::env::var("HEADROOM_DESKTOP_NO_CCR").ok().as_deref())
}

fn desktop_forces_no_ccr_from(raw: Option<&str>) -> bool {
    let Some(value) = raw.map(str::trim) else {
        return false;
    };
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// The unified `--no-ccr` flag replaced the split `--no-ccr-marker` /
/// `--no-ccr-inject-tool` pair in headroom-ai 0.31.0 (upstream ecc93991);
/// 0.30.0 and earlier exit 2 with "No such option", which would fail boot
/// validation on the 0.28.0 fallback runtime exactly as --no-http2 did on
/// 0.26.0 (Sentry RUST-4A). Verified against the tagged CLI: v0.30.0 carries
/// the split pair, v0.31.0+ the unified flag. Unknown/unparseable version means
/// the receipt is from a current install, so assume the pinned runtime.
fn runtime_supports_no_ccr(installed_version: Option<&str>) -> bool {
    let Some(version) = installed_version else {
        return true;
    };
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= (0, 31),
        _ => true,
    }
}

/// The "coding" savings persona only exists in `agent_savings._PROFILES` from
/// headroom-ai 0.30.0. On the 0.28.0 fallback runtime (chosen when 0.30.0 boot
/// validation times out) the set is {agent-90, balanced}, so `coding` makes the
/// proxy raise on startup and exit before opening the port (Sentry RUST-1M).
/// Below 0.30.0, fall back to "agent-90" — the runtime's own default, valid in
/// every version. Unknown/unparseable version = current install, keep "coding".
fn savings_profile_for_runtime(installed_version: Option<&str>) -> &'static str {
    let Some(version) = installed_version else {
        return "coding";
    };
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) if (major, minor) < (0, 30) => "agent-90",
        _ => "coding",
    }
}

/// Whether to opt this spawn into the cc-switch reconciler.
///
/// The reconciler is only safe alongside the Official-branch upstream reset:
/// without it, a switch back to Claude Official leaves the previously captured
/// third-party endpoint live on `HeadroomProxy.ANTHROPIC_API_URL` -- a
/// process-wide class attr -- so every Anthropic client on this proxy keeps
/// reaching e.g. api.deepseek.com while sending Anthropic OAuth credentials.
///
/// That reset is upstream PR #3166, still unmerged, so this used to be gated on
/// the wheel version the fix was expected to land in. That gate failed OPEN: it
/// was set to 0.36.3 as a guess, 0.36.3/0.36.4/0.36.5 all shipped without the
/// fix, and the next pin bump would have switched the reconciler on against a
/// runtime that still misroutes. SITECUSTOMIZE_PY now carries the reset itself,
/// so the question is no longer "which wheel is this" but "is the patch
/// actually in place", which the desktop can answer exactly.
///
/// Fail-closed on both sides: the flag is only set when this spawn wrote
/// pyinject/sitecustomize.py, and if the patch cannot bind in-process (kill
/// switch, module renamed, import failure) the injection clears
/// HEADROOM_CC_SWITCH_RECONCILE before `reconciler_enabled()` reads it. Neither
/// side needs a version bump when a wheel finally ships #3166: the patch
/// self-neutralizes against a runtime that already resets.
/// The env a configured upstream contributes to the backend spawn.
struct UpstreamSpawnEnv {
    target_api_url: String,
    pin_upstream: &'static str,
    lossless: &'static str,
}

/// Translate the user's override into that env.
///
/// All three are always set, so a spawn after the override was cleared cannot
/// inherit the previous one from the environment. Empty is what the runtime's
/// `_get_env_str` already treats as unset, so the default case is inert.
///
/// `HEADROOM_LOSSLESS` rides along with any configured upstream: these
/// endpoints are Anthropic-COMPATIBLE rather than Anthropic, and lossless
/// compaction keeps the payload shape close to what the client sent while
/// still saving tokens. It is deliberately keyed on "an upstream is
/// configured", not on the mode -- a Fallback upstream serves the same
/// third-party endpoint.
fn upstream_spawn_env(upstream: &crate::state::UpstreamOverride) -> UpstreamSpawnEnv {
    let configured = upstream.configured_upstream();
    UpstreamSpawnEnv {
        target_api_url: configured.unwrap_or_default().to_string(),
        pin_upstream: if upstream.pins_upstream() { "1" } else { "0" },
        lossless: if configured.is_some() { "1" } else { "0" },
    }
}

/// The URL cc-switch users' clients must be pointed at: the desktop's intercept,
/// never the Python proxy's own port. See the guard in `SITECUSTOMIZE_PY`.
fn cc_switch_proxy_url() -> String {
    format!(
        "http://127.0.0.1:{}",
        crate::proxy_intercept::INTERCEPT_PORT
    )
}

fn cc_switch_reconcile_for_spawn(sitecustomize_injected: bool) -> &'static str {
    if sitecustomize_injected {
        "1"
    } else {
        "0"
    }
}

fn headroom_entrypoint_startup_args(
    installed_version: Option<&str>,
    learn_enabled: bool,
) -> Vec<String> {
    // HTTP/2 to upstream is disabled both ways: the explicit --no-http2 flag
    // AND the HEADROOM_HTTP2=false env var (set in the spawn env). Either alone
    // suffices, but older bundled runtimes ignored the env var and ran HTTP/2
    // unconditionally, which surfaced as SSLV3_ALERT_BAD_RECORD_MAC under
    // multi-tab concurrency. The flag is belt-and-suspenders against a future
    // runtime regressing on the env var — but only on runtimes whose click
    // entrypoint defines it (see runtime_supports_no_http2). --log-messages
    // stores full request/response bodies so the desktop's Activity tab can
    // render the live transformations feed.
    let mut args = vec![
        "proxy".to_string(),
        "--port".to_string(),
        headroom_proxy_port(),
    ];
    if runtime_supports_no_http2(installed_version) {
        args.push("--no-http2".to_string());
    }
    args.push("--log-messages".to_string());
    // CCR: both reasons it was disabled are fixed in the 0.37.0 pin, so it is
    // back ON by default here.
    //
    // It was turned off because `headroom_retrieve` in the tools array routes
    // every stream:true turn through a buffered stream:false upstream call
    // resynthesized as SSE, and that wrapper committed 200 + text/event-stream
    // before the upstream outcome was known. A real 429/500/529 then reached
    // the client as an unretryable 200 ("empty or malformed response"). Both
    // halves have since landed upstream:
    //   * #2952/#2953 -- byte-faithful passthrough discarded the stream:false
    //     flip, which was the actual root cause. Merged, ships from 0.36.0.
    //   * #2465/#3079 -- liveness vs status fidelity. 0.37.0 carries
    //     proxy/buffered_ccr_response.py, which holds the response uncommitted
    //     for DEFAULT_BUFFERED_CCR_GRACE_SECONDS (5.0s) so fast failures keep
    //     their real status and headers, then commits SSE with a heartbeat so a
    //     first byte always precedes the client's stream-idle watchdog, and
    //     translates a post-commit failure into the provider's own typed SSE
    //     error so client backoff still fires.
    //
    // Why turn it back on rather than leave a working setting alone: CCR is
    // what makes lossy compression RECOVERABLE. Compressed tool output carries
    // a retrieval marker, so an agent that needs the exact bytes can ask for
    // them instead of being handed a plausible-but-wrong reconstruction --
    // upstream #1307, the correctness incident that read protection exists to
    // prevent. Without recovery there is no safe route to compressing older
    // reads, and read protection is the largest single cap on compression in
    // long agentic sessions.
    //
    // Known costs, to be measured on staging before this reaches stable:
    //   * The reversibility guard applies again (content_router.py), so lossy
    //     compressions that cannot be made recoverable are SKIPPED rather than
    //     kept. Measured 218 `lossy_unrecoverable_skipped` in ~19h with CCR
    //     off, all of which come back. tok_saved may fall.
    //   * The buffered path bypasses StreamingMixin._stream_response, one of
    //     the three seams SITECUSTOMIZE_PY's context guard (#2942) attaches to.
    //     Context-limit LEARNING still works (it hangs off
    //     handle_anthropic_messages and get_context_limit), but the streamed
    //     usage nudge does not run on those turns.
    //
    // Kill switch, no rebuild required: launch the app with
    // HEADROOM_DESKTOP_NO_CCR=1 to restore the flag.
    if runtime_supports_no_ccr(installed_version) && desktop_forces_no_ccr() {
        args.push("--no-ccr".to_string());
    }
    if learn_enabled {
        args.extend(headroom_learn_startup_args());
    }
    args
}

/// Flags whose presence in the running proxy's argv we treat as proof that it
/// was started by this build. If any of these are missing, the proxy was
/// spawned by an older desktop (or by something else) and we restart it.
/// With auto-learning off the learn flags are not passed, so they drop out of
/// the signature too.
fn expected_proxy_arg_signature(learn_enabled: bool) -> Vec<&'static str> {
    let mut flags = vec!["--port", "--log-messages"];
    if learn_enabled {
        flags.extend([
            "--learn",
            "--no-memory-tools",
            "--no-memory-context",
            "--memory-db-path",
        ]);
    }
    flags
}

/// Returns the full command line of whatever process is currently listening on
/// the proxy port, or `None` if we couldn't determine it.
pub fn running_proxy_argv() -> Option<String> {
    let (_, pid) = listener_process(backend_port::get())?;
    ps_command(pid)
}

/// Identity of whatever is listening on `port`, plus whether it is one of ours.
///
/// Diagnostics need to tell a foreign squatter from an orphaned old Headroom: a
/// port that answers HTTP without the backend's routes is invisible to the
/// readyz gate (a 404 there deliberately counts as reachable), so the listener
/// is the one fact that resolves the ambiguity. The identity string alone
/// cannot: "python3.12 (pid 7)" is our managed runtime on one host and an
/// unrelated venv on the next, so ownership goes through
/// `pid_is_headroom_backend`, which checks argv (or the executable path on
/// Windows) rather than the process name.
///
/// Best-effort: `None` where lsof is unavailable (Windows) or no listener could
/// be resolved. That is "unknown", not "foreign" -- callers must not read it as
/// proof of either, and the caller's message must read fine without it.
pub(crate) fn listener_identity_and_ownership(port: u16) -> Option<(String, bool)> {
    let (cmd, pid) = listener_process(port)?;
    let identity = format_listener_identity(&cmd, pid, ps_command(pid).as_deref());
    Some((identity, pid_is_headroom_backend(pid)))
}

fn format_listener_identity(cmd: &str, pid: u32, argv: Option<&str>) -> String {
    const MAX_ARGV: usize = 160;
    let argv = argv.map(str::trim).unwrap_or_default();
    if argv.is_empty() {
        return format!("{cmd} (pid {pid})");
    }
    let mut argv = argv.to_string();
    if argv.len() > MAX_ARGV {
        let cut = (0..=MAX_ARGV)
            .rev()
            .find(|i| argv.is_char_boundary(*i))
            .unwrap_or(0);
        argv.truncate(cut);
        argv.push_str("...");
    }
    format!("{cmd} (pid {pid}): {argv}")
}

/// True if the running proxy's argv contains every flag we expect this build
/// to pass. Used to detect proxies left over from an older desktop version.
pub fn running_proxy_matches_expected_args() -> bool {
    let Some(argv) = running_proxy_argv() else {
        // Fail open. The caller kills the backend when this is false, and
        // "we could not read the listener's argv" is not evidence that it is
        // stale. Returning false here meant every ensure-running pass on a
        // host we cannot introspect killed and respawned a healthy backend.
        return true;
    };
    proxy_argv_contains_expected_flags(&argv, !crate::client_adapters::is_auto_learn_disabled())
}

fn proxy_argv_contains_expected_flags(argv: &str, learn_enabled: bool) -> bool {
    // A proxy still carrying --learn after the user turned auto-learning off is
    // as stale as one missing a flag: restart it so the opt-out takes effect.
    if !learn_enabled && argv_contains_flag(argv, "--learn") {
        return false;
    }
    expected_proxy_arg_signature(learn_enabled)
        .iter()
        .all(|flag| argv_contains_flag(argv, flag))
}

/// Whitespace-aware containment check so `--port` doesn't match `--port-foo`
/// and `--learn` doesn't match `--no-learn`.
fn argv_contains_flag(argv: &str, flag: &str) -> bool {
    argv.split_whitespace().any(|tok| tok == flag)
}

fn ps_command(pid: u32) -> Option<String> {
    let output = crate::proc::command("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// If `log_tail` shows pydantic refusing to import because the installed
/// `pydantic-core` doesn't match what the bundled pydantic wants, return the
/// version pydantic wants. The error message is the source of truth — pydantic
/// prints the exact pinned version it expects.
///
/// Example line we match:
///     SystemError: The installed pydantic-core version (2.46.3) is
///     incompatible with the current pydantic version, which requires 2.41.5.
fn extract_required_pydantic_core_version(log_tail: &str) -> Option<String> {
    if !log_tail.contains("pydantic-core") {
        return None;
    }
    let marker = "which requires ";
    let idx = log_tail.find(marker)?;
    let after = &log_tail[idx + marker.len()..];
    let version: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let trimmed = version.trim_end_matches('.');
    if trimmed.is_empty() || !trimmed.contains('.') {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Child-process logs are opened append-mode on every launch and otherwise
/// grow unbounded. Rename to `.log.old` (keeping one prior generation, same
/// scheme as logging.rs) once the file exceeds the cap, before reopening.
/// Read the last `max_bytes` of a log file, for folding into an error message.
/// Returns an empty string if the file is missing or unreadable — best-effort
/// diagnostics, never a hard failure.
fn log_tail(path: &Path, max_bytes: u64) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max_bytes {
        use std::io::Seek;
        let _ = f.seek(std::io::SeekFrom::Start(len - max_bytes));
    }
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    buf.trim().to_string()
}

fn rotate_log_if_large(path: &Path) {
    const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    if too_big {
        let backup = path.with_extension("log.old");
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::rename(path, &backup);
    }
}

/// Make a string safe to use as part of a filename: replace path separators
/// (`/`, `\`) and other characters that have meaning to the filesystem with
/// `_`, then truncate so absurdly long argv strings don't blow past
/// per-component name limits (255 bytes on most filesystems).
fn sanitize_log_variant(raw: &str) -> String {
    const MAX_LEN: usize = 80;
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' | '\n' | '\r' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if out.len() > MAX_LEN {
        out.truncate(MAX_LEN);
    }
    out
}

/// Args that enable passive learning: the proxy extracts patterns from live
/// traffic into the memory store, but does not inject memory tools or context
/// into requests (so the model's view of the conversation is unchanged).
fn headroom_learn_startup_args() -> Vec<String> {
    vec![
        "--learn".to_string(),
        "--no-memory-tools".to_string(),
        "--no-memory-context".to_string(),
        "--memory-db-path".to_string(),
        crate::headroom_memory_db_path().display().to_string(),
    ]
}

fn headroom_propagated_proxy_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home)
        .join(".headroom")
        .join("logs")
        .join("proxy.log");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

struct DownloadArtifact {
    url: String,
    sha256: Option<&'static str>,
}

/// Metadata for a specific headroom-ai release fetched from PyPI.
pub(crate) struct HeadroomRelease {
    version: String,
    wheel_url: String,
    sha256: String,
}

impl HeadroomRelease {
    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeMaintenanceKind {
    Upgrade,
    RequirementsRepair,
}

/// Outcome of [`ToolManager::atomic_upgrade_headroom`].
///
/// `InstalledPendingValidation` means install + smoke test succeeded but the
/// backup is still on disk. The caller must either commit or rollback.
pub enum UpgradeOutcome {
    InstalledPendingValidation {
        /// Last ~100 lines of pip stdout/stderr from this install. Attached
        /// to the boot-validation Sentry event when it later fails — pip
        /// can return exit 0 while leaving the venv in a broken state
        /// (skipped packages, downgraded native deps with mismatched ABI,
        /// etc.), and without the tail there's no record of what actually
        /// happened. Empty string when capture was skipped (e.g., bootstrap).
        pip_output_tail: String,
    },
    InstallFailed {
        /// True if we successfully restored the old venv + receipt.
        restored: bool,
        error: anyhow::Error,
    },
}

/// Bounded ring buffer collecting pip stdout/stderr lines for post-mortem
/// diagnostics. Keeps the LAST `max_lines` (drops oldest when full) so
/// warnings, "Skipping X", "Successfully installed ..." lines that pip
/// prints near the end of a run survive. Sentry extras cap at ~16KB; 100
/// lines at the typical ~120-char pip line averages ~12KB.
pub(crate) struct PipOutputCapture {
    lines: std::collections::VecDeque<String>,
    max_lines: usize,
}

impl PipOutputCapture {
    pub(crate) fn new(max_lines: usize) -> Self {
        Self {
            lines: std::collections::VecDeque::with_capacity(max_lines),
            max_lines,
        }
    }

    pub(crate) fn push(&mut self, line: &str) {
        // pip's `--progress-bar raw` byte counter fires ~4x/second, so 100 of
        // them is 25 seconds of a single wheel -- it would evict every
        // Collecting/ERROR line this ring exists to carry into Sentry.
        if line.starts_with("Progress ") {
            return;
        }
        if self.lines.len() >= self.max_lines {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_string());
    }

    pub(crate) fn into_string(self) -> String {
        let parts: Vec<String> = self.lines.into_iter().collect();
        parts.join("\n")
    }
}

/// State required to perform (and roll back) an in-place upgrade — i.e. an
/// upgrade that mutates the live venv instead of rebuilding it. When
/// `previous_lock_backup` is `Some`, the dep lock has churned and the file at
/// that path is the pre-upgrade lock content, used by rollback and recovery
/// to `pip install --upgrade -r <backup>` back to the prior pin set.
pub(crate) struct InPlaceUpgradeContext {
    pub(crate) previous_version: String,
    pub(crate) previous_lock_backup: Option<PathBuf>,
}

/// Best-effort free-bytes query for the volume backing `path`. Returns None
/// on error — callers should treat that as "don't block on disk space".
#[cfg(unix)]
fn available_disk_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if ret != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
fn available_disk_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(path)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();

    let mut free_bytes_available: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            existing.as_ptr(),
            &mut free_bytes_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(free_bytes_available)
}

/// Pinned headroom-ai wheel for the running platform. The wheel carries a
/// native `_core` extension, so a macOS wheel installed on Windows/Linux
/// yields `ModuleNotFoundError: No module named 'headroom._core'` at proxy
/// start (RUST-6E: every 0.7.7 Windows install). Keep this in step with
/// `python_distribution_artifact`'s platform matrix.
fn pinned_headroom_release() -> Result<HeadroomRelease> {
    let (url, sha256) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => (
            "https://files.pythonhosted.org/packages/47/21/8a87b66e83498da89404cdba4ced6397e84331047df9e11a9ea6f3510b29/headroom_ai-0.37.0-cp310-abi3-macosx_11_0_arm64.whl",
            "b4392f68a8d02d74c62c1734cf5bf327511dcc72678f01669f44f0612944d59c",
        ),
        ("macos", "x86_64") => (
            "https://files.pythonhosted.org/packages/56/cc/385712352911b7a482514902745cba802e03947850689a784b2d40764e06/headroom_ai-0.37.0-cp310-abi3-macosx_10_12_x86_64.whl",
            "d89fd5858e701ada53d01849f73039d891fad84d9eb370f952d56581962d9cf8",
        ),
        ("linux", "aarch64") => (
            "https://files.pythonhosted.org/packages/c6/2e/8d1c60683c74ae2871270789e0af1acc93727a51189799e74529679d795c/headroom_ai-0.37.0-cp310-abi3-manylinux_2_28_aarch64.whl",
            "bc30d31a6b9336155d62bbdd99f3c2f6c5a1ed3882a8730ea0cd8ede4c40fa19",
        ),
        ("linux", "x86_64") => (
            "https://files.pythonhosted.org/packages/72/b8/16878cf4fe6fc390a0d22025b671468619db690ff14c1b103ace4b5e35f9/headroom_ai-0.37.0-cp310-abi3-manylinux_2_28_x86_64.whl",
            "2efc5cdf681a10c5fc7a2a271a471179c409074537045f682b10e4d724976f46",
        ),
        ("windows", "x86_64") => (
            "https://files.pythonhosted.org/packages/c9/84/6803f3cc069dc8a6843c7ed8b155d1cf0c603a7467f58ffa24c5c399b8c9/headroom_ai-0.37.0-cp310-abi3-win_amd64.whl",
            "e961f892786f7577e75f2c26229f11e2609fc007083dae24d336e76fc4c72e58",
        ),
        (os, arch) => bail!("unsupported headroom-ai wheel target: {os}/{arch}"),
    };

    Ok(HeadroomRelease {
        version: HEADROOM_PINNED_VERSION.into(),
        wheel_url: url.into(),
        sha256: sha256.into(),
    })
}

fn python_distribution_artifact() -> Result<DownloadArtifact> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(DownloadArtifact {
            url: format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{}/cpython-3.12.12+20251014-aarch64-apple-darwin-install_only_stripped.tar.gz",
                PYTHON_STANDALONE_RELEASE
            ),
            sha256: Some(PYTHON_SHA256_MACOS_AARCH64),
        }),
        ("macos", "x86_64") => Ok(DownloadArtifact {
            url: format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{}/cpython-3.12.12+20251014-x86_64-apple-darwin-install_only_stripped.tar.gz",
                PYTHON_STANDALONE_RELEASE
            ),
            sha256: Some(PYTHON_SHA256_MACOS_X86_64),
        }),
        ("linux", "x86_64") => Ok(DownloadArtifact {
            url: format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{}/cpython-3.12.12+20251014-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
                PYTHON_STANDALONE_RELEASE
            ),
            sha256: Some(PYTHON_SHA256_LINUX_X86_64),
        }),
        ("linux", "aarch64") => Ok(DownloadArtifact {
            url: format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{}/cpython-3.12.12+20251014-aarch64-unknown-linux-gnu-install_only_stripped.tar.gz",
                PYTHON_STANDALONE_RELEASE
            ),
            sha256: Some(PYTHON_SHA256_LINUX_AARCH64),
        }),
        ("windows", "x86_64") => Ok(DownloadArtifact {
            url: format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{}/cpython-3.12.12+20251014-x86_64-pc-windows-msvc-install_only_stripped.tar.gz",
                PYTHON_STANDALONE_RELEASE
            ),
            sha256: Some(PYTHON_SHA256_WINDOWS_X86_64),
        }),
        (os, arch) => bail!("unsupported Headroom managed Python target: {os}/{arch}"),
    }
}

fn rtk_distribution_artifact() -> Result<DownloadArtifact> {
    let (target, sha256, extension) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("aarch64-apple-darwin", RTK_SHA256_MACOS_AARCH64, "tar.gz"),
        ("macos", "x86_64") => ("x86_64-apple-darwin", RTK_SHA256_MACOS_X86_64, "tar.gz"),
        ("linux", "aarch64") => (
            "aarch64-unknown-linux-gnu",
            RTK_SHA256_LINUX_AARCH64,
            "tar.gz",
        ),
        ("linux", "x86_64") => (
            "x86_64-unknown-linux-musl",
            RTK_SHA256_LINUX_X86_64,
            "tar.gz",
        ),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", RTK_SHA256_WINDOWS_X86_64, "zip"),
        (os, arch) => bail!("unsupported RTK target: {os}/{arch}"),
    };

    Ok(DownloadArtifact {
        url: format!(
            "https://github.com/rtk-ai/rtk/releases/download/v{}/rtk-{}.{}",
            RTK_VERSION, target, extension
        ),
        sha256: Some(sha256),
    })
}

/// Why this addon cannot be installed on the current OS/arch, in one sentence
/// the Addons tab shows in place of an Install button that could only ever
/// error. Keyed off the same artifact resolvers the installers call, so a newly
/// published target re-enables the card with no edit here.
fn addon_unavailable_reason(id: &str) -> Option<String> {
    let platform = match std::env::consts::OS {
        "windows" => "Windows",
        "linux" => "Linux",
        "macos" => "macOS",
        other => other,
    };
    match id {
        "rtk" if rtk_distribution_artifact().is_err() => Some(format!(
            "Not available on {platform} {}: RTK publishes no build for this architecture yet.",
            std::env::consts::ARCH
        )),
        "codebase-memory" if codebase_memory_distribution_artifact().is_err() => Some(format!(
            "Not available on {platform}: codebase-memory publishes macOS and Linux binaries only."
        )),
        _ => None,
    }
}

fn codebase_memory_distribution_artifact() -> Result<DownloadArtifact> {
    let (target, sha256) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin-arm64", CODEBASE_MEMORY_SHA256_MACOS_AARCH64),
        ("macos", "x86_64") => ("darwin-amd64", CODEBASE_MEMORY_SHA256_MACOS_X86_64),
        ("linux", "aarch64") => ("linux-arm64", CODEBASE_MEMORY_SHA256_LINUX_AARCH64),
        ("linux", "x86_64") => ("linux-amd64", CODEBASE_MEMORY_SHA256_LINUX_X86_64),
        (os, arch) => bail!("unsupported codebase-memory target: {os}/{arch}"),
    };

    Ok(DownloadArtifact {
        url: format!(
            "https://github.com/DeusData/codebase-memory-mcp/releases/download/v{}/codebase-memory-mcp-{}.tar.gz",
            CODEBASE_MEMORY_VERSION, target
        ),
        sha256: Some(sha256),
    })
}

fn download_to_path(url: &str, destination: &Path, expected_sha256: Option<&str>) -> Result<()> {
    download_to_path_with_progress(url, destination, expected_sha256, |_, _| {})
}

/// True when a `download_to_path` failure was a sha256 mismatch (see the
/// `bail!` in `download_to_path_with_progress`) rather than a transfer error.
/// A mismatch means the bytes were tampered with or corrupted in transit —
/// exactly the case the hash exists for — so callers must never respond to it
/// by falling back to an unverified pip-index install.
fn is_checksum_mismatch(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("checksum mismatch")
}

static ARTIFACT_DOWNLOAD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `(url, downloaded, total)` of the download holding `ARTIFACT_DOWNLOAD_LOCK`,
/// for a waiter on the same URL to mirror. RUST-CR: bootstrap was consented
/// while the pre-consent prefetch was mid-way through the Python tarball,
/// blocked on the lock behind a frozen "Downloading Python 18%" frame for
/// minutes on a slow link, and the user quit. The bytes were arriving the
/// whole time; only the bar was static. Never cleared: a stale entry can only
/// be read by a same-URL waiter, whose own attempt then finds the file.
static INFLIGHT_ARTIFACT_DOWNLOAD: std::sync::Mutex<Option<(String, u64, Option<u64>)>> =
    std::sync::Mutex::new(None);

fn publish_inflight_download(url: &str, downloaded: u64, total: Option<u64>) {
    if let Ok(mut slot) = INFLIGHT_ARTIFACT_DOWNLOAD.lock() {
        *slot = Some((url.to_string(), downloaded, total));
    }
}

/// Take the artifact lock, relaying the holder's progress for `url` into
/// `on_progress` every 250ms until it is free.
fn acquire_artifact_download_lock<F>(
    url: &str,
    on_progress: &mut F,
) -> std::sync::MutexGuard<'static, ()>
where
    F: FnMut(u64, Option<u64>),
{
    loop {
        match ARTIFACT_DOWNLOAD_LOCK.try_lock() {
            Ok(guard) => return guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let mirrored = INFLIGHT_ARTIFACT_DOWNLOAD.lock().ok().and_then(|slot| {
            slot.as_ref()
                .filter(|(inflight_url, _, _)| inflight_url == url)
                .map(|(_, downloaded, total)| (*downloaded, *total))
        });
        if let Some((downloaded, total)) = mirrored {
            on_progress(downloaded, total);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Download `url` to `destination` with an optional progress callback.
///
/// The callback receives `(downloaded_bytes, total_bytes)` and is called at
/// most every 250ms during a streaming download. `total_bytes` is `None` when
/// the server does not provide a Content-Length header.
fn download_to_path_with_progress<F>(
    url: &str,
    destination: &Path,
    expected_sha256: Option<&str>,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    // One artifact download at a time, process-wide. The pre-consent prefetch
    // and the consented bootstrap can otherwise race the same `.partial`
    // file; holding the lock across the exists+sha check below means
    // whichever caller runs second sees the finished file and skips. While
    // waiting, the holder's progress for the same URL is relayed into
    // `on_progress`, so a bootstrap that joins a prefetch keeps its bar moving.
    let _download_guard = acquire_artifact_download_lock(url, &mut on_progress);

    if destination.exists() {
        if let Some(expected_sha256) = expected_sha256 {
            match verify_sha256_file(destination, expected_sha256) {
                Ok(()) => return Ok(()),
                Err(_) => {
                    std::fs::remove_file(destination)
                        .with_context(|| format!("removing {}", destination.display()))?;
                }
            }
        } else {
            return Ok(());
        }
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("headroom-desktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(30 * 60))
        .tcp_keepalive(Duration::from_secs(60))
        .build()
        .context("building download client")?;

    let tmp_path = destination.with_extension("partial");
    const MAX_ATTEMPTS: u32 = 5;
    let mut last_err = anyhow::anyhow!("no attempts made");

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            // 2s, 4s, 8s, 16s between attempts.
            std::thread::sleep(Duration::from_secs(1u64 << attempt));
        }
        let _ = std::fs::remove_file(&tmp_path);

        let result = (|| -> Result<()> {
            let mut response = client
                .get(url)
                .send()
                .with_context(|| format!("downloading {}", url))?
                .error_for_status()
                .with_context(|| format!("downloading {}", url))?;

            let total_bytes = response.content_length();
            let mut file = std::fs::File::create(&tmp_path)
                .with_context(|| format!("creating {}", tmp_path.display()))?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut downloaded: u64 = 0;
            on_progress(0, total_bytes);
            publish_inflight_download(url, 0, total_bytes);
            let mut last_emit = Instant::now();

            loop {
                let n = response.read(&mut buf).context("reading download body")?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .with_context(|| format!("writing {}", tmp_path.display()))?;
                hasher.update(&buf[..n]);
                downloaded += n as u64;
                if last_emit.elapsed() >= Duration::from_millis(250) {
                    on_progress(downloaded, total_bytes);
                    publish_inflight_download(url, downloaded, total_bytes);
                    last_emit = Instant::now();
                }
            }
            file.flush().context("flushing download")?;
            drop(file);
            on_progress(downloaded, total_bytes);
            publish_inflight_download(url, downloaded, total_bytes);

            if let Some(expected_sha256) = expected_sha256 {
                let actual_checksum = format!("{:x}", hasher.finalize());
                if actual_checksum != expected_sha256 {
                    bail!(
                        "checksum mismatch for {}: expected {}, got {}",
                        url,
                        expected_sha256,
                        actual_checksum
                    );
                }
            }

            std::fs::rename(&tmp_path, destination).with_context(|| {
                format!(
                    "renaming {} to {}",
                    tmp_path.display(),
                    destination.display()
                )
            })?;
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }

    let _ = std::fs::remove_file(&tmp_path);
    Err(last_err)
}

fn verify_sha256_file(path: &Path, expected_sha256: &str) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let actual_checksum = sha256_bytes(&bytes);
    if actual_checksum != expected_sha256 {
        bail!(
            "checksum mismatch for {}: expected {}, got {}",
            path.display(),
            expected_sha256,
            actual_checksum
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct HeadroomLearnMetadataCandidate {
    metadata: HeadroomLearnMetadata,
    sort_key: Option<DateTime<Utc>>,
}

fn read_headroom_learn_metadata_from_path(path: &Path) -> Option<HeadroomLearnMetadataCandidate> {
    let content = std::fs::read_to_string(path).ok()?;
    let (start, end, _) = headroom_learn_block_bounds(&content)?;
    let block = &content[start..end];
    let pattern_count = count_headroom_learn_patterns(block);
    let learned_at = parse_headroom_learn_timestamp(block);
    let modified_at = std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(DateTime::<Utc>::from);

    Some(HeadroomLearnMetadataCandidate {
        metadata: HeadroomLearnMetadata {
            learned_at: learned_at
                .map(|timestamp| timestamp.to_rfc3339())
                .or_else(|| modified_at.map(|timestamp| timestamp.to_rfc3339())),
            pattern_count,
        },
        sort_key: learned_at.or(modified_at),
    })
}

fn count_headroom_learn_patterns(block: &str) -> Option<usize> {
    let count = block
        .lines()
        .filter(|line| line.trim_start().starts_with("- "))
        .count();

    if count > 0 {
        Some(count)
    } else {
        None
    }
}

fn parse_headroom_learn_timestamp(block: &str) -> Option<DateTime<Utc>> {
    const PREFIX: &str = "*Auto-generated by `headroom learn` on ";

    block.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix(PREFIX)?;
        let token: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '-' | ':' | 'T' | 'Z' | '+'))
            .collect();
        if token.is_empty() {
            return None;
        }

        DateTime::parse_from_rfc3339(&token)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDate::parse_from_str(&token, "%Y-%m-%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|timestamp| DateTime::<Utc>::from_naive_utc_and_offset(timestamp, Utc))
            })
    })
}

const LEARN_START: &str = "<!-- headroom:learn:start -->";
const LEARN_END: &str = "<!-- headroom:learn:end -->";
const LEARN_BLOCK_TITLE: &str = "## Headroom Learned Patterns";

/// Byte range of the managed learn block (start marker inclusive, end marker
/// exclusive) plus whether the end marker was actually present. A block with
/// no end marker runs to the next `## ` heading (other than the block's own
/// title) or EOF. That shape is real: on 2026-09-02 the headroom-desktop
/// MEMORY.md lost its end marker (cause never found), every reader here
/// treated it as "no block", and the wheel's writer - which only replaces
/// start..end - silently wrote nothing until the marker was restored by hand.
fn headroom_learn_block_bounds(content: &str) -> Option<(usize, usize, bool)> {
    let start = content.find(LEARN_START)?;
    if let Some(rel) = content[start..].find(LEARN_END) {
        return Some((start, start + rel, true));
    }
    let mut from = start + LEARN_START.len();
    let end = loop {
        match content[from..].find("\n## ") {
            None => break content.len(),
            Some(rel) => {
                let heading = from + rel + 1;
                if content[heading..].starts_with(LEARN_BLOCK_TITLE) {
                    from = heading + LEARN_BLOCK_TITLE.len();
                    continue;
                }
                break heading;
            }
        }
    };
    Some((start, end, false))
}

/// Re-insert a missing end marker. `None` when the content has no block or
/// the block is already intact, so callers can treat `Some` as "changed".
pub fn repair_headroom_learn_block(content: &str) -> Option<String> {
    let (_, end, has_end) = headroom_learn_block_bounds(content)?;
    if has_end {
        return None;
    }
    let head = content[..end].trim_end_matches('\n');
    let tail = content[end..].trim_start_matches('\n');
    let mut out = String::with_capacity(content.len() + LEARN_END.len() + 3);
    out.push_str(head);
    out.push('\n');
    out.push_str(LEARN_END);
    out.push('\n');
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(tail);
    }
    Some(out)
}

/// Heal a start-only block on disk. Runs before `headroom learn --apply` so
/// the wheel's writer has a start..end span to replace. Warns (Sentry, via
/// the log bridge) with the file's mtime: the 2026-09-02 loss was never
/// dated, and the mtime plus a fleet count is what separates a code bug
/// (many hosts) from a local hand edit (one host).
pub fn repair_headroom_learn_block_file(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(repaired) = repair_headroom_learn_block(&content) else {
        return false;
    };
    let modified = std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(|m| DateTime::<Utc>::from(m).to_rfc3339())
        .unwrap_or_default();
    match crate::client_adapters::atomic_write(path, repaired.as_bytes()) {
        Ok(()) => {
            log::warn!(
                "learn block in {} had no end marker (file mtime {modified}, {} bytes); end marker restored",
                path.display(),
                content.len()
            );
            true
        }
        Err(err) => {
            log::warn!(
                "learn block in {} has no end marker and repair failed: {err:#}",
                path.display()
            );
            false
        }
    }
}

/// Parse sections and bullets inside the managed `<!-- headroom:learn -->`
/// block. Returns an empty Vec if no block is present.
pub fn parse_headroom_learn_block(file_content: &str) -> Vec<crate::models::AppliedSection> {
    use crate::models::AppliedSection;
    let Some((start, end, _)) = headroom_learn_block_bounds(file_content) else {
        return Vec::new();
    };
    let block = &file_content[start..end];

    let mut sections: Vec<AppliedSection> = Vec::new();
    let mut current: Option<AppliedSection> = None;

    for line in block.lines() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("### ") {
            if let Some(sec) = current.take() {
                sections.push(sec);
            }
            current = Some(AppliedSection {
                title: title.trim().to_string(),
                bullets: Vec::new(),
            });
        } else if let Some(sec) = current.as_mut() {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                let bullet = rest.trim();
                if !bullet.is_empty() {
                    sec.bullets.push(bullet.to_string());
                }
            }
        }
    }
    if let Some(sec) = current {
        sections.push(sec);
    }
    sections
}

/// Delete one bullet from the managed block and return the updated file
/// contents. No-op (returns the original) when section or bullet is missing.
///
/// If a section's bullets are all removed, the whole `### <section>` block is
/// dropped. If the entire managed block becomes empty, the whole block
/// including its markers is removed.
pub fn delete_applied_bullet(file_content: &str, section_title: &str, bullet_text: &str) -> String {
    // A start-only block (see `headroom_learn_block_bounds`) is healed first
    // so the rewrite below always has a real end marker to anchor on.
    let repaired;
    let file_content: &str = match repair_headroom_learn_block(file_content) {
        Some(fixed) => {
            repaired = fixed;
            &repaired
        }
        None => file_content,
    };
    let Some(start) = file_content.find(LEARN_START) else {
        return file_content.to_string();
    };
    let end_marker = LEARN_END;
    let Some(end_rel) = file_content[start..].find(end_marker) else {
        return file_content.to_string();
    };
    let end = start + end_rel + end_marker.len();

    let before = &file_content[..start];
    let block = &file_content[start..end];
    let after = &file_content[end..];

    let mut out_lines: Vec<String> = Vec::new();
    let mut current_section_start: Option<usize> = None;
    let mut current_section_has_bullets = false;
    let mut in_target_section = false;
    let mut bullet_removed = false;

    fn flush(
        out_lines: &mut Vec<String>,
        section_start: &mut Option<usize>,
        has_bullets: &mut bool,
    ) {
        if let Some(idx) = section_start.take() {
            if !*has_bullets {
                out_lines.truncate(idx);
            }
        }
        *has_bullets = false;
    }

    for line in block.lines() {
        // Skip the end-of-block marker so the section-flush truncation
        // can never drop it. We re-append it during reassembly below.
        if line.trim_end() == end_marker {
            continue;
        }

        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("### ") {
            flush(
                &mut out_lines,
                &mut current_section_start,
                &mut current_section_has_bullets,
            );
            current_section_start = Some(out_lines.len());
            in_target_section = title.trim() == section_title;
            out_lines.push(line.to_string());
            continue;
        }

        if current_section_start.is_some() {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                let bullet = rest.trim();
                if in_target_section && !bullet_removed && bullet == bullet_text {
                    bullet_removed = true;
                    continue;
                }
                if !bullet.is_empty() {
                    current_section_has_bullets = true;
                }
            }
        }

        out_lines.push(line.to_string());
    }
    flush(
        &mut out_lines,
        &mut current_section_start,
        &mut current_section_has_bullets,
    );

    if !bullet_removed {
        return file_content.to_string();
    }

    let any_sections = out_lines.iter().any(|l| l.trim_start().starts_with("### "));
    if !any_sections {
        let mut rewritten = String::with_capacity(before.len() + after.len());
        rewritten.push_str(before.trim_end_matches('\n'));
        let after_trimmed = after.trim_start_matches('\n');
        if !rewritten.is_empty() && !after_trimmed.is_empty() {
            rewritten.push_str("\n\n");
        }
        rewritten.push_str(after_trimmed);
        return rewritten;
    }

    // Drop trailing blank lines so removing the last bullet of the last
    // section doesn't leave a `\n\n<!-- end -->` gap behind.
    while out_lines
        .last()
        .map(|s| s.trim().is_empty())
        .unwrap_or(false)
    {
        out_lines.pop();
    }

    let mut rewritten = String::with_capacity(file_content.len());
    rewritten.push_str(before);
    rewritten.push_str(&out_lines.join("\n"));
    rewritten.push('\n');
    rewritten.push_str(end_marker);
    rewritten.push_str(after);
    rewritten
}

pub fn claude_project_memory_file(project_path: &str) -> PathBuf {
    let home = dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    home.join(".claude")
        .join("projects")
        .join(encode_claude_project_folder_name(project_path))
        .join("memory")
        .join("MEMORY.md")
}

fn encode_claude_project_folder_name(project_path: &str) -> String {
    format!(
        "-{}",
        project_path.trim_start_matches('/').replace('/', "-")
    )
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Recursively collect every `.so` / `.dylib` under `dir`. Used by
/// `ad_hoc_sign_venv_natives` to enumerate the native extensions pip
/// dropped into the venv. Symlinks are followed via `read_dir`'s default
/// behavior on macOS, but `file_type` is checked so we don't recurse into
/// non-directories. Errors propagate so the caller can log + skip.
fn collect_native_extensions(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_native_extensions(&path, out)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "so" || ext == "dylib" {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// Hash a requirements lock file ignoring comments and blank lines, so that
/// header/comment churn does not force a full `pip install` on upgrade.
fn requirements_lock_sha(lock: &str) -> String {
    let mut hasher = Sha256::new();
    for line in lock.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        hasher.update(trimmed.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

fn bootstrap_requirements_lock() -> &'static str {
    bootstrap_requirements_lock_for_target(std::env::consts::OS)
}

fn bootstrap_requirements_lock_for_target(os: &str) -> &'static str {
    match os {
        // Linux bootstrap only needs the proxy runtime. Installing the full
        // headroom-ai[all] stack pulls optional native packages like hnswlib
        // that fail on many fresh Linux systems.
        "linux" => HEADROOM_LINUX_REQUIREMENTS_LOCK,
        // Windows uses the full stack minus hnswlib (sdist-only on PyPI, no
        // vendored win_amd64 wheel; a source build needs MSVC and bricked
        // bootstrap — RUST-65/66). sqlite-vec covers the vector backend. The
        // pin set is its own file so a Windows-only resolution change never
        // perturbs the macOS lock.
        "windows" => HEADROOM_WINDOWS_REQUIREMENTS_LOCK,
        _ => HEADROOM_REQUIREMENTS_LOCK,
    }
}

fn run_python_command(python: &Path, args: &[&str], cwd: &Path) -> Result<()> {
    run_command(python, args, cwd)
}

/// Path to the output-shaper savings ledger, which `output_savings` also reads
/// to recompute the estimate the dashboard shows.
fn output_savings_ledger_path() -> Option<PathBuf> {
    crate::output_savings::ledger_path()
}

/// Core of [`purge_legacy_output_savings_control_arm_once`]: given the ledger bytes, return
/// rewritten bytes when a non-empty `control` arm was cleared, else `None`
/// (missing/empty control, or unparseable input we must not clobber).
fn ledger_bytes_without_control(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut ledger = serde_json::from_slice::<Value>(bytes).ok()?;
    let has_control = ledger
        .get("control")
        .and_then(Value::as_object)
        .is_some_and(|c| !c.is_empty());
    if !has_control {
        return None;
    }
    ledger
        .as_object_mut()?
        .insert("control".to_string(), json!({}));
    serde_json::to_vec(&ledger).ok()
}

/// Drop the output-shaper A/B control arm left over from the abandoned 1%
/// holdout, exactly once.
///
/// Those samples predate the current shaper and were collected under a policy
/// that never gathered enough of them to mean anything, so folding them into
/// the 3% arm would poison it from the first request. Clearing them on every
/// spawn is not an option either: the arm is live data now, and this runs each
/// time the proxy starts.
///
/// The stamp sits beside the ledger on purpose. A reset that removes
/// `~/.headroom` takes the control samples with it, so the stamp going too is
/// correct — there is nothing left to purge either way.
///
/// Best-effort throughout: never touch a missing or unparseable ledger, and
/// only rewrite when there is control data to drop. Uses `atomic_write` so a
/// crash mid-write cannot truncate the ledger.
fn purge_legacy_output_savings_control_arm_once() {
    let Some(path) = output_savings_ledger_path() else {
        return;
    };
    let stamp = path.with_file_name(".legacy-control-arm-purged");
    if stamp.exists() {
        return;
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Some(out) = ledger_bytes_without_control(&bytes) {
        if let Err(err) = crate::client_adapters::atomic_write(&path, &out) {
            log::warn!("[tool_manager] purging output_savings control arm failed: {err}");
            // Leave the stamp unwritten so the next spawn retries; a failed
            // purge must not be recorded as a done one.
            return;
        }
        log::info!("[tool_manager] cleared legacy output-shaper control arm");
    }
    if let Err(err) = crate::client_adapters::atomic_write(&stamp, b"") {
        log::warn!("[tool_manager] stamping legacy control-arm purge failed: {err}");
    }
}

/// True once the verbosity baseline has been seeded (non-empty sample count).
/// The persisted baseline does not carry a `total_samples` field — that is a
/// computed property of the in-memory model. On disk the total observation
/// count lives in `baseline.glob.n` (the global accumulator), so that is what
/// we gate on. The proxy reports the savings estimate as unavailable until a
/// baseline with observations exists.
fn verbosity_baseline_present() -> bool {
    let Some(path) = output_savings_ledger_path() else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|json| {
            json.get("baseline")
                .and_then(|b| b.get("glob"))
                .and_then(|g| g.get("n"))
                .and_then(|n| n.as_u64())
        })
        .is_some_and(|n| n > 0)
}

/// Real project root (the transcript `cwd`) of the Claude Code project with the
/// most transcript bytes under `~/.claude/projects`. Reading `cwd` from a
/// transcript avoids lossily decoding the mangled `~/.claude/projects` dir name
/// — it is exactly the path headroom's plugin resolves to, so `--project <cwd>`
/// matches. Returns `None` when no non-empty transcript exists.
fn busiest_claude_project_cwd() -> Option<String> {
    // client_adapters::home_dir, not a bare $HOME: a Windows GUI process has
    // no HOME env var, and bailing here silently skipped verbosity-baseline
    // seeding on every Windows install (output savings stayed $0 forever).
    let projects_dir = crate::client_adapters::home_dir()
        .join(".claude")
        .join("projects");

    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&projects_dir).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let mut bytes = 0u64;
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Ok(meta) = f.metadata() {
                        bytes += meta.len();
                    }
                }
            }
        }
        if bytes > 0 && best.as_ref().is_none_or(|(b, _)| bytes > *b) {
            best = Some((bytes, dir));
        }
    }

    project_cwd_from_transcript_dir(&best?.1)
}

/// Pull the `cwd` field from the first transcript line that has one. Reads at
/// most a few lines so a multi-hundred-MB transcript dir stays cheap.
fn project_cwd_from_transcript_dir(dir: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    for f in std::fs::read_dir(dir).ok()?.flatten() {
        let p = f.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = std::fs::File::open(&p) else {
            continue;
        };
        for line in BufReader::new(file).lines().take(50).map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                    if !cwd.is_empty() {
                        return Some(cwd.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Prepend a binary's own directory to PATH so an `#!/usr/bin/env node`
/// shebang (or similar) resolves the interpreter that nvm installs alongside
/// it. Falls back to the existing PATH when the binary has no parent.
fn context7_package_spec() -> String {
    format!("@upstash/context7-mcp@{CONTEXT7_PINNED_VERSION}")
}

/// PATH directories that would load a foreign OpenSSL into a bundled
/// interpreter. Computed once -- our own PATH does not change under us, and
/// this stats every entry.
fn foreign_openssl_path_dirs() -> &'static [String] {
    static DIRS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    DIRS.get_or_init(|| crate::conflicting_openssl_dirs(&std::env::var("PATH").unwrap_or_default()))
}

/// `path_var` with every entry in `drop` removed. Pure and unconditional so it
/// stays testable off Windows; the caller decides when it applies.
fn path_without_dirs(path_var: &std::ffi::OsStr, drop: &[String]) -> std::ffi::OsString {
    if drop.is_empty() {
        return path_var.to_os_string();
    }
    let kept: Vec<PathBuf> = std::env::split_paths(path_var)
        .filter(|dir| !drop.iter().any(|d| Path::new(d) == dir))
        .collect();
    std::env::join_paths(kept).unwrap_or_else(|_| path_var.to_os_string())
}

/// True for our managed CPython, whatever the platform names it
/// (`python3`, `python.exe`, `pythonw.exe`).
fn is_python_interpreter(binary: &Path) -> bool {
    binary
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.to_ascii_lowercase().starts_with("python"))
}

/// PATH for a child of `binary`, with the binary's own directory first.
///
/// For our bundled interpreter the inherited PATH is filtered first. Windows
/// resolves a DLL by base name along PATH, so a `libcrypto-3-x64.dll` left in
/// some unrelated program's directory is loaded into our interpreter ahead of
/// the one we ship; `_ssl.pyd` does not export `OPENSSL_Applink`, so that
/// libcrypto aborts the instant it is used and ensurepip dies before pip speaks
/// a word. RUST-8K was 25 relaunches into that wall on one host, RUST-A0 the
/// same abort with a WAMP PHP directory on PATH. Until now all we could do was
/// name the directory in the failure report and ask the user to change their
/// own PATH; dropping it from the *child's* environment fixes it for them.
/// (Refuted for the ensurepip abort, 2026-09-05: RUST-A0 recurred on 0.9.7
/// with this filter applied and the VanDyke dir named. That abort is the
/// SSLKEYLOGFILE keylog path, see `strip_unusable_sslkeylogfile`. The filter
/// stays: harmless, and still right for a DLL genuinely resolved by PATH.)
///
/// Scoped deliberately. Only the child's PATH changes -- the user's is
/// untouched -- and only for the interpreter, because a directory is dropped
/// here purely for holding an OpenSSL DLL and a non-Python child (a node CLI,
/// say) may legitimately need to exec something else out of it. Our runtime is
/// never on PATH, so every entry dropped is foreign by construction, and the
/// interpreter's own directory is prepended *after* the filter so a DLL we ship
/// beside it can never be the thing removed. Off Windows the scan finds nothing
/// (it looks for `.dll` names) and this is a no-op.
fn path_with_binary_dir(binary: &Path) -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let base = if is_python_interpreter(binary) {
        let foreign = foreign_openssl_path_dirs();
        if !foreign.is_empty() {
            // info, not warn: this is the mitigation working. A warn would
            // bridge to Sentry (see logging.rs) once per spawn on every machine
            // that has any OpenSSL on PATH, which is a lot of them.
            log::info!(
                "dropping {} foreign OpenSSL dir(s) from the interpreter's PATH: {}",
                foreign.len(),
                foreign.join(", ")
            );
        }
        path_without_dirs(&inherited, foreign)
    } else {
        inherited
    };
    match binary.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            crate::proc::path_with_dir_prepended_to(dir, &base)
        }
        _ => base,
    }
}

/// True when httpx can actually use `value` as a proxy URL. httpx parses the
/// standard proxy env vars at client construction (trust_env) and raises
/// `ValueError: Unknown scheme for proxy URL` for anything outside
/// http/https/socks5/socks5h -- socks4 in particular (v2rayN-style local
/// proxies advertise socks4://127.0.0.1:10808). An empty value is how users
/// disable a proxy; httpx ignores it, so it passes.
fn httpx_supports_proxy_url(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    v.is_empty()
        || ["http://", "https://", "socks5://", "socks5h://"]
            .iter()
            .any(|scheme| v.starts_with(scheme))
}

/// Remove proxy env vars the backend's HTTP stack cannot use. A scheme httpx
/// rejects kills the Python backend inside AsyncClient() before it opens the
/// port (RUST-AS/RUST-AT: exit 3, `Unknown scheme for proxy URL
/// URL('socks4://127.0.0.1:10808')`). Strip only the offending vars: a
/// supported proxy (socksio is vendored, so socks5 works) passes through
/// untouched, and going direct is strictly better than a guaranteed boot
/// crash -- httpx could never have used the value anyway. Name matching is
/// case-insensitive because Windows envs carry arbitrary casings.
fn strip_unsupported_proxy_env(command: &mut Command) {
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        let is_proxy_var = ["http_proxy", "https_proxy", "all_proxy"]
            .iter()
            .any(|p| name.eq_ignore_ascii_case(p));
        if !is_proxy_var {
            continue;
        }
        let value = value.to_string_lossy();
        if httpx_supports_proxy_url(&value) {
            continue;
        }
        // Scheme only -- proxy URLs can embed credentials, keep them out of logs.
        let scheme = value.split("://").next().unwrap_or("").trim().to_string();
        // info, not warn: warn is bridged to Sentry and would re-report on
        // every launch of an affected machine, forever.
        log::info!(
            "[tool_manager] dropping {name} from backend env: scheme {scheme:?} unsupported by httpx (would crash backend boot)"
        );
        command.env_remove(name);
    }
}

/// Windows keeps a second proxy source the env strip above cannot reach:
/// the WinINET registry proxy (`ProxyEnable`/`ProxyServer` under HKCU).
/// Python's `urllib.getproxies()` falls back to it whenever the process env
/// carries not a single non-empty `*_proxy` var, and httpx wraps every
/// http/https entry in `Proxy(url=...)` at AsyncClient() construction -- so a
/// v2rayN-style `socks4://127.0.0.1:10808` system proxy kills backend boot
/// with the exact ValueError of RUST-AS/RUST-AT even on builds that ship
/// `strip_unsupported_proxy_env` (RUST-AY, stable 0.9.3). Mirror urllib's
/// expansion; if the entries httpx would mount include a scheme it cannot
/// parse, override the child env: usable entries become explicit
/// http_proxy/https_proxy vars (any env proxy var makes urllib skip the
/// registry), and when nothing is usable NO_PROXY="*" sends the backend
/// direct -- strictly better than a guaranteed boot crash.
fn override_unsupported_registry_proxy(command: &mut Command) {
    if !cfg!(windows) {
        return;
    }
    // urllib consults the registry only when the child sees zero non-empty
    // *_proxy env vars. Vars the strip above removes are gone from the child,
    // so they do not count; anything else (ftp_proxy, no_proxy, a supported
    // http_proxy) suppresses the registry on its own.
    let child_keeps_a_proxy_var = std::env::vars_os().any(|(name, value)| {
        let Some(name) = name.to_str() else {
            return false;
        };
        let lower = name.to_ascii_lowercase();
        let value = value.to_string_lossy();
        if value.is_empty() || !lower.ends_with("_proxy") {
            return false;
        }
        let stripped_above = ["http_proxy", "https_proxy", "all_proxy"].contains(&lower.as_str())
            && !httpx_supports_proxy_url(&value);
        !stripped_above
    });
    if child_keeps_a_proxy_var {
        return;
    }
    let Some(output) = crate::proc::command("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output()
        .ok()
        .filter(|out| out.status.success())
    else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let enabled = parse_reg_value(&text, "ProxyEnable")
        .map(|v| reg_dword_is_set(&v))
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let Some(server) = parse_reg_value(&text, "ProxyServer") else {
        return;
    };
    let Some(overrides) = registry_proxy_env_overrides(&server) else {
        return;
    };
    // info, not warn: warn is bridged to Sentry and would re-report on every
    // launch of an affected machine, forever. Values stay out of the log --
    // proxy URLs can embed credentials.
    log::info!(
        "[tool_manager] WinINET registry proxy carries a scheme httpx cannot parse; overriding {} child proxy env var(s) so backend boot survives",
        overrides.len()
    );
    for (name, value) in overrides {
        command.env(name, value);
    }
}

/// The data row for `value_name` in `reg query` output:
/// `    ProxyServer    REG_SZ    socks4://127.0.0.1:10808`.
/// Localized Windows translates headers and INFO lines but never the value
/// rows' REG_* type token, so matching name + type token is locale-safe.
fn parse_reg_value(reg_output: &str, value_name: &str) -> Option<String> {
    for line in reg_output.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some(value_name) {
            continue;
        }
        let Some(ty) = parts.next() else { continue };
        if !ty.starts_with("REG_") {
            continue;
        }
        let rest = parts.collect::<Vec<_>>().join(" ");
        if !rest.is_empty() {
            return Some(rest);
        }
    }
    None
}

/// `reg query` prints REG_DWORD as hex (`0x1`).
fn reg_dword_is_set(value: &str) -> bool {
    let v = value.trim().trim_start_matches("0x");
    u64::from_str_radix(v, 16).map(|n| n != 0).unwrap_or(false)
}

/// urllib's `getproxies_registry` expansion of the `ProxyServer` string,
/// restricted to the keys httpx mounts (http/https). A value without `=`
/// applies to every protocol; `proto=addr` pairs are per-protocol; an address
/// without a scheme inherits its protocol name as the scheme. A keyed
/// `socks=` entry is NOT harmless: CPython backfills missing http/https keys
/// with `socks4://addr` ("the default SOCKS proxy type of Windows is SOCKS4",
/// urllib/request.py getproxies_registry) -- the RUST-B3 crash was exactly
/// that backfill reaching httpx via the https mount. Returns None when no
/// override is needed (nothing mounted, or every mounted entry parses),
/// otherwise the env vars to set on the child.
fn registry_proxy_env_overrides(proxy_server: &str) -> Option<Vec<(String, String)>> {
    let server = proxy_server.trim();
    if server.is_empty() {
        return None;
    }
    let expanded = if server.contains('=') {
        server.to_string()
    } else {
        format!("http={server};https={server}")
    };
    let mut mounted: Vec<(String, String)> = Vec::new();
    let mut socks_url: Option<String> = None;
    for pair in expanded.split(';') {
        let Some((proto, addr)) = pair.split_once('=') else {
            continue;
        };
        let proto = proto.trim().to_ascii_lowercase();
        if proto != "http" && proto != "https" && proto != "socks" {
            continue;
        }
        let addr = addr.trim();
        if addr.is_empty() {
            continue;
        }
        // urllib keeps an existing scheme (`^([^/:]+)://`) and otherwise
        // prefixes the protocol name.
        let has_scheme = addr
            .split_once("://")
            .is_some_and(|(scheme, _)| !scheme.contains('/') && !scheme.contains(':'));
        let url = if has_scheme {
            addr.to_string()
        } else {
            format!("{proto}://{addr}")
        };
        if proto == "socks" {
            // urllib's backfill rewrites a bare `socks://` to `socks4://`;
            // an explicit scheme (e.g. socks5://) is kept as-is.
            socks_url = Some(match url.strip_prefix("socks://") {
                Some(rest) => format!("socks4://{rest}"),
                None => url,
            });
        } else {
            mounted.push((proto, url));
        }
    }
    // CPython: `proxies['http'] = proxies.get('http') or socks_address` (same
    // for https), so the socks entry becomes the mount wherever no explicit
    // http/https pair exists.
    if let Some(socks) = socks_url {
        for proto in ["http", "https"] {
            if !mounted.iter().any(|(p, _)| p == proto) {
                mounted.push((proto.to_string(), socks.clone()));
            }
        }
    }
    if mounted.iter().all(|(_, url)| httpx_supports_proxy_url(url)) {
        return None;
    }
    let usable: Vec<(String, String)> = mounted
        .into_iter()
        .filter(|(_, url)| httpx_supports_proxy_url(url))
        .map(|(proto, url)| (format!("{proto}_proxy"), url))
        .collect();
    if usable.is_empty() {
        return Some(vec![("NO_PROXY".to_string(), "*".to_string())]);
    }
    Some(usable)
}

/// True when a proxy env value names a socks-scheme proxy (any variant -
/// socks4/socks4a/socks5/socks5h), which pip's vendored requests cannot use
/// without the optional pysocks package.
fn is_socks_proxy_value(value: &str) -> bool {
    value.trim().to_ascii_lowercase().starts_with("socks")
}

/// Remove socks-scheme proxy env vars from tool children. The managed venv
/// does not ship pysocks, so every pip run under `all_proxy=socks5://...`
/// dies with "Missing dependencies for SOCKS support" (RUST-6S) - going
/// direct is strictly better than that guaranteed failure. http/https
/// proxies pass through untouched, and the backend spawn is unaffected: it
/// keeps socks5 via its own narrower `strip_unsupported_proxy_env`, because
/// httpx vendors socksio and can actually use it.
fn strip_socks_proxy_env(command: &mut Command) {
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        let is_proxy_var = ["http_proxy", "https_proxy", "all_proxy"]
            .iter()
            .any(|p| name.eq_ignore_ascii_case(p));
        if !is_proxy_var {
            continue;
        }
        if !is_socks_proxy_value(&value.to_string_lossy()) {
            continue;
        }
        // info, not warn: warn is bridged to Sentry and would re-report on
        // every launch of an affected machine, forever.
        log::info!(
            "[tool_manager] dropping {name} from child env: socks proxies need pysocks, which the managed venv does not ship (pip would fail on it)"
        );
        command.env_remove(name);
    }
}

/// True when the SSLKEYLOGFILE path can actually be opened for append, which
/// is exactly what CPython's `SSLContext.keylog_filename` setter does the
/// moment a TLS context is created. Probing with create matches those
/// semantics: a not-yet-existing but creatable file is fine (Python would
/// create it the same way), so only a genuinely unopenable path is unusable.
fn sslkeylogfile_is_usable(path: &str) -> bool {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .is_ok()
}

/// Why `value` must not reach a managed-interpreter child, or `None` when it
/// may. Pure so the Windows rule is testable from any platform.
fn sslkeylogfile_strip_reason(value: &str, windows: bool) -> Option<&'static str> {
    if value.trim().is_empty() {
        return None;
    }
    if windows {
        return Some(
            "python-build-standalone's python.exe lacks OPENSSL_Applink, so a keylog file aborts pip/backend at TLS-context creation (RUST-A0)",
        );
    }
    if sslkeylogfile_is_usable(value) {
        return None;
    }
    Some("its path cannot be opened (would crash pip/backend at TLS-context creation)")
}

/// Remove SSLKEYLOGFILE from the child env when the interpreter cannot honor
/// it. urllib3 (vendored in pip), truststore and httpx all assign
/// `context.keylog_filename` at TLS-context creation, and CPython opens the
/// file eagerly there -- so a machine-wide SSLKEYLOGFILE pointing at an
/// inaccessible path (RUST-A8: `\\?\Volume{...}\virtual_file.log`, set by a
/// TLS-inspection tool) kills ensurepip/pip during bootstrap and would kill
/// the backend's AsyncClient the same way.
///
/// On Windows the variable is dropped even when the path opens fine. CPython
/// hands the opened FILE* to OpenSSL's `BIO_new_fp`, which on Windows routes
/// through OpenSSL's uplink table and needs the host EXE to export
/// `OPENSSL_Applink`; python-build-standalone's `python.exe` does not, so the
/// first TLS context dies with `OPENSSL_Uplink(...,08): no OPENSSL_Applink`
/// and exit 1 -- pip's vendored truststore does this at import. RUST-8K and
/// RUST-A0 (3 hosts, 11 events, all Windows) are that abort; the 0.9.7 event
/// had the foreign-DLL PATH filter applied, so the DLL theory was wrong. Key
/// logging can never work from that interpreter, so nothing is lost. Off
/// Windows a path that opens fine passes through: deliberate Wireshark-style
/// key logging keeps working. Empty values pass too -- Python skips them. Name
/// matching is case-insensitive because Windows envs carry arbitrary casings.
fn strip_unusable_sslkeylogfile(command: &mut Command) {
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        if !name.eq_ignore_ascii_case("SSLKEYLOGFILE") {
            continue;
        }
        let value = value.to_string_lossy();
        let Some(reason) = sslkeylogfile_strip_reason(&value, cfg!(windows)) else {
            continue;
        };
        // info, not warn: warn is bridged to Sentry and would re-report on
        // every launch of an affected machine, forever.
        log::info!("[tool_manager] dropping {name} from child env: {reason}");
        command.env_remove(name);
    }
}

fn build_command(binary: &Path, args: &[&str], cwd: &Path) -> Command {
    let mut command = crate::proc::command(binary);
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("PYTHONHOME")
        // GUI apps inherit a minimal PATH lacking the nvm/homebrew bin dir, so a
        // CLI with a `#!/usr/bin/env node` shebang (e.g. codex) fails with exit
        // 127 / "env: node: No such file or directory". node lives alongside the
        // CLI in nvm's bin, so prepend the binary's own dir to PATH.
        .env("PATH", path_with_binary_dir(binary))
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONSTARTUP")
        .env("PYTHONNOUSERSITE", "1")
        // `backslashreplace`, matching `proc::command` -- plain "utf-8" here
        // silently overrode it and re-armed the RUST-7C UnicodeEncodeError kill
        // for every child this builds (a lone surrogate must not be fatal).
        .env("PYTHONIOENCODING", "utf-8:backslashreplace")
        .env("LC_ALL", "C.UTF-8")
        .env("LANG", "C.UTF-8")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        // pip's default rich progress bar renders nothing at all to a pipe
        // until a wheel finishes, so a large one is pure silence: torch is
        // 123 MB and the RUST-9Y host was pulling at ~60 kB/s, i.e. 34 minutes
        // with no output. That froze the wizard (reads as a hang, the user
        // quits) and starved `PIP_OUTPUT_SILENCE_TIMEOUT`, which killed a
        // download that was progressing fine. `raw` prints
        // "Progress <bytes> of <total>" ~4x/second instead; `pip_line_to_progress`
        // turns it into the MB counter and `PipOutputCapture` filters it out.
        //
        // `raw` needs pip >= 24.1 and an older pip rejects the value outright,
        // failing every install. Safe here because every pip we run lives in a
        // venv built from `PYTHON_STANDALONE_RELEASE`, whose ensurepip bundles
        // pip 25.0.1. Re-check that if the interpreter pin ever moves back.
        .env("PIP_PROGRESS_BAR", "raw")
        // A host-level pip config (`user = true` in pip.conf, or PIP_USER in
        // the environment) leaks into the managed venv's pip and fails every
        // install with "Can not perform a '--user' install. User site-packages
        // are not visible in this virtualenv." (RUST-6S). Pin the switch off
        // and aim pip's config lookup at the null device so no user/site/
        // global pip.conf is read at all.
        .env("PIP_USER", "0")
        .env(
            "PIP_CONFIG_FILE",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
    strip_unusable_sslkeylogfile(&mut command);
    strip_socks_proxy_env(&mut command);
    command
}

/// Runs a pip install invocation with retries on transient failures.
///
/// pip's own `--retries` flag only covers connection establishment, not
/// mid-stream read timeouts, so a single TCP stall during a wheel download
/// can fail the whole bootstrap (see Sentry bootstrap_failed reports). We
/// retry the full invocation; pip's cachecontrol layer persists partial
/// responses so retries resume cheaply instead of redownloading from zero.
fn run_pip_install_with_retries(python: &Path, args: &[&str], cwd: &Path) -> Result<()> {
    run_pip_install_with_retries_streaming(python, args, cwd, |_| {})
}

/// Number of requirement lines in a lock, i.e. how many packages pip will
/// resolve. `requirements_lock_sha` already defines what counts as a
/// requirement line (non-blank, non-comment); this is the same rule, so the
/// two can never disagree about the file's contents.
fn requirements_lock_package_count(lock: &str) -> u32 {
    lock.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Translate a pip stdout/stderr line into a progress update, or None for
/// noise. Monotonic advance inside `[base_percent, max_percent-1]`.
///
/// `total_packages` is the requirement count from the lock we handed pip, so
/// the bar and the ETA track real progress. Pass 0 when it isn't known and the
/// old counter heuristic applies: each interesting line nudges the bar forward
/// and it saturates just below the parent step's ceiling.
///
/// Both numbers used to be fictional, and on a slow link that read as a hang.
/// `remaining` was `90 - elapsed`, floored at 5, so past 90 seconds the UI said
/// "5 seconds left" forever; the percent saturated at `max_percent - 1` after
/// `span` lines, which ~170 packages cross in the first minute. The Windows
/// host in RUST-9Y was pulling torch and opencv at ~60 kB/s -- hours of install
/// behind a bar frozen at 79% claiming it was finishing up -- and quit. The
/// funnel calls that `bootstrap_abandoned`; the user was told the wrong thing.
fn pip_line_to_progress(
    line: &str,
    elapsed: Duration,
    counter: &mut u32,
    base_percent: u8,
    max_percent: u8,
    total_packages: u32,
) -> Option<BootstrapStepUpdate> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // `resolved` marks the lines that mean every package has been fetched, so
    // the fraction can jump to 1.0 instead of extrapolating a download rate
    // through a phase that no longer downloads anything.
    let (message, collected, resolved) = if let Some(rest) = trimmed.strip_prefix("Collecting ") {
        let spec = rest.split_whitespace().next().unwrap_or(rest);
        let pkg = spec
            .split(|c: char| matches!(c, '=' | '<' | '>' | '!' | '~' | ';' | '['))
            .next()
            .unwrap_or(spec);
        (format!("Fetching {}...", pkg), true, false)
    } else if let Some(rest) = trimmed.strip_prefix("Downloading ") {
        let file = rest.split_whitespace().next().unwrap_or(rest);
        let name = file.rsplit('/').next().unwrap_or(file);
        let pkg = name.split('-').next().unwrap_or(name);
        (format!("Downloading {}...", pkg), false, false)
    } else if let Some(message) = trimmed
        .strip_prefix("Progress ")
        .and_then(raw_progress_message)
    {
        // The only output pip produces while a single wheel downloads. It does
        // not advance the package counter -- the same wheel is still in flight
        // -- but it keeps the message moving and lets the ETA below re-project
        // off the growing elapsed time, which is what turns a 34-minute torch
        // download from "hung at 79%" into "slow".
        (message, false, false)
    } else if trimmed.starts_with("Installing collected packages") {
        ("Installing packages...".to_string(), false, true)
    } else if let Some(rest) = trimmed.strip_prefix("Successfully installed ") {
        let count = rest.split_whitespace().count();
        (format!("Installed {} packages.", count), false, true)
    } else {
        return None;
    };

    // Counts packages, not lines: pip prints a `Collecting` and a `Downloading`
    // for each one, and counting both made the bar move at twice the true rate.
    if collected {
        *counter = counter.saturating_add(1);
    }
    let span = max_percent.saturating_sub(base_percent).max(1) as u32;

    // Fraction of the dependency set pip has reached. Transitive deps that are
    // not in our lock (pip resolves those too) can push the count past the
    // total, hence the clamp.
    let fraction = if resolved {
        1.0
    } else if total_packages > 0 {
        (f64::from(*counter) / f64::from(total_packages)).clamp(0.0, 1.0)
    } else {
        // Unknown total: the old counter heuristic, which saturates just below
        // the ceiling once enough packages have gone by.
        f64::from((*counter).min(span.saturating_sub(1))) / f64::from(span.max(1))
    };

    let advance = (f64::from(span) * fraction) as u32;
    let percent = (base_percent as u32 + advance).min(max_percent as u32 - 1) as u8;

    // Extrapolate from the rate this machine is actually achieving. A host on a
    // 60 kB/s link gets an ETA in the hours, which is the truth and reads as
    // "slow", where the old fixed 90-second budget read as "hung". Needs a real
    // sample first: the first few packages land before any large wheel and
    // would project absurdly low.
    const INITIAL_ETA_SECS: u64 = 90;
    const MIN_SAMPLE_SECS: u64 = 10;
    const MIN_SAMPLE_FRACTION: f64 = 0.02;
    // A whole day, so a genuinely stalled transfer still produces a finite
    // number rather than saturating into nonsense.
    const MAX_ETA_SECS: u64 = 24 * 3600;
    let elapsed_secs = elapsed.as_secs();
    let remaining = if resolved {
        // Unpack and install of what is already on disk. Not machine-
        // independent at all: Windows Defender scans every extracted file, so
        // the ~424 MB payload takes minutes there (a full pip step measured
        // 6m35s on a healthy Win11 box, most of it this phase) while an
        // AV-free SSD finishes in well under a minute. The old flat 15 here
        // put the wizard on "finishing up" for the whole unpack and read as a
        // hang.
        // ponytail: fixed per-platform seed, derive from payload size if the
        // lock ever changes materially.
        if cfg!(windows) {
            240
        } else {
            60
        }
    } else if elapsed_secs >= MIN_SAMPLE_SECS && fraction >= MIN_SAMPLE_FRACTION {
        let projected_total = elapsed.as_secs_f64() / fraction;
        (projected_total - elapsed.as_secs_f64()).clamp(5.0, MAX_ETA_SECS as f64) as u64
    } else {
        // Too early to measure. Never floor at a near-zero value the way the
        // old `.max(5)` did once the budget ran out -- that is what turned a
        // slow install into an apparently finished one.
        INITIAL_ETA_SECS.saturating_sub(elapsed_secs).max(30)
    };
    Some(BootstrapStepUpdate {
        step: "Updating dependencies",
        message,
        eta_seconds: remaining,
        percent,
    })
}

/// A pip line into the app log, minus the `PIP_PROGRESS_BAR=raw` byte counter.
/// That fires ~4x/second per wheel, and the app log's tail is the evidence
/// `bootstrap_abandoned` ships to Sentry -- one addon install would flush it.
fn log_pip_line(label: &str, line: &str) {
    if line.starts_with("Progress ") {
        return;
    }
    log::info!("{label}: {line}");
}

/// Body of a pip `--progress-bar raw` line ("<bytes> of <total>") as a
/// user-facing MB counter. `None` for anything that does not parse, so an
/// unrelated line starting with "Progress " cannot fake download progress.
/// pip reports 0 as the total when the server sends no Content-Length.
fn raw_progress_message(rest: &str) -> Option<String> {
    let (current, total) = rest.split_once(" of ")?;
    let current: u64 = current.trim().parse().ok()?;
    let total: u64 = total.trim().parse().ok()?;
    let mb = |bytes: u64| bytes as f64 / 1_048_576.0;
    Some(if total > 0 {
        format!("Downloading {:.1} / {:.1} MB...", mb(current), mb(total))
    } else {
        format!("Downloading {:.1} MB...", mb(current))
    })
}

// Compact representation of a pip-install failure for log/Sentry. The full
// CommandFailure Display dumps program + args + stdout + stderr, which the
// 400-char Sentry cap eats before any stderr lines appear. Pip's actual
// reason lives on stderr, so prefer the tail of stderr (or stdout if stderr
// is empty) plus exit code.
pub(crate) fn compact_pip_failure(err: &anyhow::Error) -> String {
    const TAIL_BUDGET: usize = 300;
    let Some(failure) = err.chain().find_map(|c| c.downcast_ref::<CommandFailure>()) else {
        // No CommandFailure means the command never ran (spawn failed).
        // `to_string()` prints only the top context ("starting <python> -m
        // pip ...") and drops the io cause — RUST-6S shipped 15 events of
        // exactly that, unreadable. `{:#}` keeps the whole chain.
        return format!("{err:#}");
    };
    let source = if !failure.stderr.trim().is_empty() {
        failure.stderr.as_str()
    } else {
        failure.stdout.as_str()
    };
    let trimmed = source.trim_end();
    // "The reason lives at the end" holds for pip run directly, but not when it
    // is reached through ensurepip: pip prints its `ERROR:` diagnosis, then a
    // Python traceback follows, so the tail is `CalledProcessError ... returned
    // non-zero exit status 1` -- the one line in the whole stream with no cause
    // in it. That is how RUST-82 reached triage: an install-blocking venv
    // failure classified `other`, with nothing in it to act on. Where pip named
    // a reason, start from it; otherwise the tail is still the best guess.
    let from_pip_error = if trimmed.starts_with("ERROR: ") {
        Some(trimmed)
    } else {
        trimmed.find("\nERROR: ").map(|i| &trimmed[i + 1..])
    };
    let tail = match from_pip_error {
        // Byte offsets, so walk to a char boundary before slicing: pip on a
        // non-English Windows locale emits multi-byte stderr and slicing
        // mid-character would panic.
        Some(head) if head.len() > TAIL_BUDGET => {
            let mut end = TAIL_BUDGET;
            while !head.is_char_boundary(end) {
                end -= 1;
            }
            &head[..end]
        }
        Some(head) => head,
        None if trimmed.len() > TAIL_BUDGET => {
            let mut start = trimmed.len() - TAIL_BUDGET;
            while !trimmed.is_char_boundary(start) {
                start += 1;
            }
            let aligned = trimmed[start..]
                .find('\n')
                .map(|i| start + i + 1)
                .unwrap_or(start);
            &trimmed[aligned..]
        }
        None => trimmed,
    };
    let exit = failure
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    format!("exit={exit}; stderr tail: {tail}")
}

/// True when pip's resolution failure is really a starved index: every
/// index/find-links fetch died, so pip saw "(from versions: none)" for a pin
/// that exists everywhere (RUST-90/91: TLS-broken middleware on one machine).
/// Requires BOTH signals so a genuinely bad pin with an incidental fetch
/// warning keeps its no-matching-dist verdict (RUST-6S listed real versions).
pub(crate) fn pip_index_fetch_failed(lower: &str) -> bool {
    lower.contains("could not fetch url") && lower.contains("from versions: none")
}

/// Coarse cause class for a pip failure, used as the Sentry fingerprint.
///
/// The compact message embeds pip's stderr tail, and Sentry groups bridged log
/// lines by message text — so every distinct tail opened its own issue for what
/// is one failure (RUST-6M/6N/6P were all the same machine's half-built venv).
/// One flat bucket would be the opposite mistake: resolving a shipped fix would
/// regress the instant an unrelated cause reappeared (RUST-5Q). These classes
/// match the buckets triage already sorts these into by hand.
pub(crate) fn pip_failure_category(compact: &str) -> &'static str {
    pip_failure_category_with_evidence(compact, compact)
}

/// pip's whole output for a failure, for the classifiers that need evidence
/// `compact_pip_failure` throws away. That tail starts at pip's FIRST
/// `ERROR:` line, and pip prints its `Could not fetch URL` / `Retrying`
/// warnings well before that -- so a starved index arrives at
/// `pip_failure_category` looking exactly like a bad pin. RUST-90 is the
/// result: `colorama==0.4.6`, a wheel that exists for every platform we ship,
/// filed under `no-matching-dist`, the bucket whose whole point is "a bad pin
/// in *our* lock". Falls back to the compact string when the error carries no
/// `CommandFailure` (spawn failure -- there was no pip output to read).
pub(crate) fn pip_failure_evidence(err: &anyhow::Error, compact: &str) -> String {
    match err.chain().find_map(|c| c.downcast_ref::<CommandFailure>()) {
        Some(failure) => format!("{}\n{}", failure.stderr, failure.stdout),
        None => compact.to_string(),
    }
}

/// `pip_failure_category`, but with pip's full output available separately for
/// the index-starvation check. `compact` still decides everything else, so the
/// buckets keep matching the message text triage reads.
pub(crate) fn pip_failure_category_with_evidence(compact: &str, evidence: &str) -> &'static str {
    let lower = compact.to_ascii_lowercase();
    let evidence_lower = evidence.to_ascii_lowercase();
    if lower.contains("no module named pip") {
        "no-pip"
    } else if (lower.contains("no matching distribution found")
        || lower.contains("could not find a version that satisfies"))
        && !pip_index_fetch_failed(&lower)
        && !pip_index_fetch_failed(&evidence_lower)
    {
        // Our lock asked for a version PyPI has no wheel for on that
        // interpreter/platform (RUST-6S: onnxruntime==1.27.0 on Intel macOS,
        // where releases stop at 1.23.2). That is a bad pin in *our* lock, not
        // the user's machine -- it must never sit in the "other" grab-bag.
        "no-matching-dist"
    } else if lower.contains("no openssl_applink") {
        // The bundled interpreter's OpenSSL refuses to run: `ensurepip` dies
        // before pip ever speaks (RUST-8K, host GIDI, 24 retries -- a hard
        // install dead end, and the inner pip's own stderr is swallowed by
        // CalledProcessError, so this line is the ONLY signal that survives).
        // Its own bucket because nothing else about it looks like the network
        // and permission causes it was sharing "other" with.
        "openssl-applink"
    } else if lower.contains("application control policy has blocked")
        || lower.contains("(os error 4551)")
    {
        // Windows Application Control (Smart App Control / WDAC / AppLocker)
        // blocked a freshly-extracted file (RUST-8K, third cause). Windows
        // localizes the prose; the numeric code is the locale-independent
        // handle. Shares its strings with `is_app_control_signal`.
        "app-control"
    } else if lower.contains("cannot import name 'httpshandler'") {
        // The bundled interpreter's ssl stack would not load, so urllib has
        // no HTTPSHandler and ensurepip dies importing pip (RUST-8K, fourth
        // cause: the App Control machine on retry -- python.exe was allowed
        // through, but _ssl's DLLs were still blocked).
        "ssl-missing"
    } else if lower.contains("no pyvenv.cfg") {
        // The venv launcher stub found no pyvenv.cfg next to it (exit 106):
        // the venv was damaged in place (AV quarantine, disk cleanup) while
        // the READY flag survived, so pip dies before it ever runs. A broken
        // install of OURS, not a user-machine problem -- and the
        // `python_runtime_installed` gate now checks pyvenv.cfg, so the next
        // launch routes to bootstrap self-repair (RUST-6S third shape, 0.9.1;
        // same blind-spot family as RUST-8E's missing base interpreter).
        "venv-broken"
    } else if lower.contains("no usable temporary directory") {
        "no-tempdir"
    } else if crate::is_disk_full_signal(&lower) {
        "disk-full"
    } else if lower.contains("permission denied")
        || lower.contains("check the permissions")
        || lower.contains("access is denied")
        || lower.contains("errno 13")
        // Windows LOCALIZES its error text: RUST-8K arrived as
        // "액세스가 거부되었습니다. (os error 5)" and fell through to the
        // grab-bag this function exists to prevent. The numeric code is the
        // one locale-independent handle. Not gated to Windows: errno 5 is EIO
        // on Unix, but a mislabelled bucket on an all-but-unseen pip EIO is a
        // cheaper bug than a Windows-only branch no test here can reach. Keep
        // the closing paren -- os error 50/51 are macOS network errors.
        || lower.contains("(os error 5)")
    {
        "permission"
    } else if lower.contains("no such file or directory") || lower.contains("errno 2") {
        "missing-file"
    } else if lower.contains("failed building wheel")
        || lower.contains("microsoft visual c++")
        || lower.contains("error: subprocess-exited-with-error")
    {
        "build"
    } else if lower.contains("could not fetch url")
        || lower.contains("connection")
        || lower.contains("timed out")
        || lower.contains("temporary failure in name resolution")
        // Same truncation as above, milder symptom: pip retried through a
        // network fault, then printed an ERROR: line that names only the
        // package. Read the evidence before shrugging it into `other` -- but
        // hold the fetch-warning evidence to the same two-signal rule as the
        // no-matching-dist guard, so an incidental (recovered) warning cannot
        // relabel an unclassified failure as environmental.
        || evidence_lower.contains("temporary failure in name resolution")
        || pip_index_fetch_failed(&evidence_lower)
    {
        "network"
    } else {
        "other"
    }
}

/// Cause class for a direct-wheel download that fell back to the pip index.
///
/// The failure's top context is `downloading <url>`, and that URL carries the
/// wheel version, the platform tag and PyPI's content hash -- so a
/// message-grouped report opens a NEW issue on every wheel bump and a separate
/// one per platform for one underlying condition (RUST-22, reopened at 0.37.0
/// after the same fallback was already triaged at earlier pins). The cause
/// class is what triage acts on: a proxy blocking files.pythonhosted.org is a
/// different problem from a 30-minute transfer timeout on a slow link, and
/// neither changes when the pin does.
fn wheel_download_failure_category(detail: &str) -> &'static str {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("operation timed out") || lower.contains("timed out") {
        "timeout"
    } else if let Some(code) = http_status_code_in(&lower) {
        // A status the CDN actually answered with: 403 is a corporate proxy
        // or a geo block, 404 a pin whose wheel PyPI never published. Keep
        // them apart -- only one of the two is ours to fix.
        match code {
            403 => "http-403",
            404 => "http-404",
            _ => "http-other",
        }
    } else if lower.contains("dns error")
        || lower.contains("failed to lookup address")
        || lower.contains("nodename nor servname")
    {
        "dns"
    } else if lower.contains("certificate")
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("self-signed")
    {
        // A TLS-terminating corporate proxy without its CA in our trust store.
        "tls"
    } else if lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("error sending request")
        || lower.contains("channel closed")
    {
        "connection"
    } else if crate::is_disk_full_signal(&lower) {
        "disk-full"
    } else if lower.contains("permission denied") || lower.contains("access is denied") {
        "permission"
    } else {
        "other"
    }
}

/// First HTTP status code in a reqwest `error_for_status` message, which
/// renders as `HTTP status client error (403 Forbidden) for url (...)`.
fn http_status_code_in(lower: &str) -> Option<u16> {
    let idx = lower.find("http status")?;
    lower[idx..]
        .split(|c: char| !c.is_ascii_digit())
        .find(|tok| tok.len() == 3)
        .and_then(|tok| tok.parse::<u16>().ok())
}

/// Report a direct-wheel download that fell back to the pip index.
///
/// Warning, not Error: the fallback install that follows normally succeeds, so
/// this is a degraded path rather than a broken one. It still reports, because
/// a category that shows up fleet-wide (a blocked CDN, an expired cert) is the
/// early signal that the *next* release's bootstrap will fail outright.
fn report_wheel_download_fallback(url: &str, err: &anyhow::Error) {
    let detail = format!("{err:#}");
    let category = wheel_download_failure_category(&detail);
    sentry::with_scope(
        |scope| {
            scope.set_tag("wheel_download_failure", category);
            scope.set_extra("wheel_url", url.to_string().into());
            scope.set_extra(
                "detail",
                detail.chars().take(2000).collect::<String>().into(),
            );
            scope.set_fingerprint(Some(["wheel-download-fallback", category].as_slice()));
        },
        || {
            sentry::capture_message(
                &format!(
                    "headroom wheel download failed ({category}); falling back to the pip index"
                ),
                sentry::Level::Warning,
            );
        },
    );
    // Local only: the fingerprinted capture above is the Sentry path, and the
    // bridged warn would re-open the URL-grouped issue this replaced. The full
    // URL and error chain stay in the file log, where triage can still read
    // them per-machine.
    log::warn!("headroom wheel download failed (will fall back to pip index): {detail}");
}

/// Cause class for a partial plugin install, so each shape gets its own Sentry
/// issue instead of one grab-bag. Same rationale as [`pip_failure_category`]:
/// RUST-6K collected five unrelated causes (a missing marketplace, a config the
/// host CLI refuses to parse, no CLI on PATH, and CLI version skew) under one
/// title, which made it untriageable -- and unresolvable, since any resolve
/// regresses the moment a sibling shape reappears.
fn plugin_install_failure_category(compact: &str) -> &'static str {
    let lower = compact.to_ascii_lowercase();
    if lower.contains("not found in marketplace") {
        // Our marketplace registration did not take. Cause now travels with it
        // (see install_plugin_into), so this bucket carries the real reason.
        "marketplace-missing"
    } else if lower.contains("cli not found on path") {
        "cli-missing"
    } else if lower.contains("unknown option")
        || lower.contains("unrecognized subcommand")
        || lower.contains("is no longer supported")
    {
        "cli-version-skew"
    } else if lower.contains("failed to load configuration")
        || lower.contains("duplicate key")
        || lower.contains("toml parse error")
    {
        // The host CLI cannot read its own config, so nothing we do lands.
        "host-config-invalid"
    } else if crate::is_disk_full_signal(&lower) {
        "disk-full"
    } else if lower.contains("permission denied")
        || lower.contains("access is denied")
        || lower.contains("errno 13")
    {
        "permission"
    } else {
        "other"
    }
}

/// Streaming variant of `run_pip_install_with_retries`. Each line emitted by
/// pip on stdout/stderr is piped through `on_line` as it arrives, so callers
/// can translate noteworthy pip events ("Collecting X", "Downloading Y",
/// "Installing collected packages", "Successfully installed") into
/// user-facing progress updates instead of staring at a static message for
/// the 60–90 seconds a large pip install takes.
/// Kill a pip child that has produced no output at all for this long. pip
/// streams constantly when healthy (Collecting/Downloading/Installing lines,
/// plus the `PIP_PROGRESS_BAR=raw` byte counter ~4x/second while a wheel is in
/// flight), so total silence this long means wedged, not slow. Generous enough
/// for the quietest legitimate gap we ship (large wheel extract on a slow disk).
///
/// That env var is load-bearing here, not cosmetic: without it pip renders
/// nothing to a pipe for the whole of a wheel transfer, so any wheel bigger
/// than 600s of link -- opencv's 40 MB at the 60 kB/s of RUST-9Y, 11 minutes --
/// tripped this watchdog and failed an install that was working. Do not drop
/// the flag without raising this timeout.
const PIP_OUTPUT_SILENCE_TIMEOUT: Duration = Duration::from_secs(600);

/// Silence window once pip has printed "Installing collected packages". From
/// that line until "Successfully installed" pip emits nothing at all, and the
/// unpack of the ~424 MB dependency payload under Windows Defender is minutes
/// of legitimate silence -- a box only slightly slower than the 6m35s one
/// measured 2026-09-03 would have had a healthy install killed mid-unpack by
/// the default window above.
const PIP_UNPACK_SILENCE_TIMEOUT: Duration = Duration::from_secs(1800);

/// Widen `limit` for the rest of the run once `line` marks the start of pip's
/// silent unpack phase. `None` (wait forever) stays `None`.
fn widen_silence_for_unpack(limit: Option<Duration>, line: &str) -> Option<Duration> {
    if line
        .trim_start()
        .starts_with("Installing collected packages")
    {
        limit.map(|l| l.max(PIP_UNPACK_SILENCE_TIMEOUT))
    } else {
        limit
    }
}

fn run_pip_install_with_retries_streaming<F>(
    python: &Path,
    args: &[&str],
    cwd: &Path,
    mut on_line: F,
) -> Result<()>
where
    F: FnMut(&str),
{
    const MAX_ATTEMPTS: u32 = 3;
    const BACKOFFS_SECS: &[u64] = &[2, 5];
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match run_command_streaming(
            python,
            args,
            cwd,
            Some(PIP_OUTPUT_SILENCE_TIMEOUT),
            &mut on_line,
        ) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt < MAX_ATTEMPTS {
                    log::info!(
                        "pip install attempt {}/{} failed (will retry): {}",
                        attempt,
                        MAX_ATTEMPTS,
                        err
                    );
                } else {
                    let compact = compact_pip_failure(&err);
                    if crate::is_disk_full_signal(&compact)
                        || crate::is_disk_full_signal(&format!("{err:#}"))
                    {
                        // ENOSPC is environmental and already surfaced + Sentry-
                        // suppressed by the caller's runtime_upgrade_failed /
                        // bootstrap_failed guard. Drop this per-attempt warn to
                        // info so the log->Sentry bridge doesn't recapture it
                        // (RUST-4C).
                        log::info!(
                            "pip install attempt {}/{} failed (final): disk full (ENOSPC)",
                            attempt,
                            MAX_ATTEMPTS
                        );
                    } else {
                        // Explicit per-category fingerprint; the bridged warn is
                        // local-only (skip_sentry rule) so this doesn't double-
                        // report. See `pip_failure_category`.
                        let category = pip_failure_category_with_evidence(
                            &compact,
                            &pip_failure_evidence(&err, &compact),
                        );
                        sentry::with_scope(
                            |scope| {
                                scope.set_fingerprint(Some(&["pip-install-failed", category]));
                            },
                            || {
                                sentry::capture_message(
                                    &format!(
                                        "pip install failed after {MAX_ATTEMPTS} attempts \
                                         [{category}]: {compact}"
                                    ),
                                    sentry::Level::Warning,
                                );
                            },
                        );
                        log::warn!(
                            "pip install attempt {}/{} failed (final): {}",
                            attempt,
                            MAX_ATTEMPTS,
                            compact
                        );
                    }
                }
                last_err = Some(err);
                if attempt < MAX_ATTEMPTS {
                    let idx = (attempt as usize - 1).min(BACKOFFS_SECS.len() - 1);
                    std::thread::sleep(std::time::Duration::from_secs(BACKOFFS_SECS[idx]));
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt was made"))
}

/// Like `run_command` but streams stdout + stderr line-by-line through
/// `on_line` in real time. Captures everything for the structured failure
/// payload so error reporting is unchanged.
/// Upper bound on each captured stream. Every consumer (`CommandFailure`'s
/// Display, which the retry path dumps into the app log, and
/// `compact_pip_failure`) reads the tail, and pip's raw progress counter emits
/// ~4 lines a second, so an unbounded buffer would hold -- and log -- megabytes
/// of "Progress N of M" after an hour on a slow link.
const STREAM_CAPTURE_CAP: usize = 64 * 1024;

/// Drop the head of `sink` down to `STREAM_CAPTURE_CAP`, cutting at a line
/// boundary (always a char boundary, so this can never split a UTF-8 sequence).
fn cap_capture(sink: &mut String) {
    if sink.len() <= STREAM_CAPTURE_CAP {
        return;
    }
    let cut = sink.len() - STREAM_CAPTURE_CAP;
    let start = sink.as_bytes()[cut..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(sink.len(), |offset| cut + offset + 1);
    sink.drain(..start);
}

fn run_command_streaming<F>(
    binary: &Path,
    args: &[&str],
    cwd: &Path,
    silence_timeout: Option<Duration>,
    on_line: &mut F,
) -> Result<()>
where
    F: FnMut(&str),
{
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let mut cmd = build_command(binary, args, cwd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {} {}", binary.display(), args.join(" ")))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (tx, rx) = mpsc::channel::<StreamedLine>();
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();
    drop(tx);

    let stdout_handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx_stdout.send(StreamedLine {
                line,
                is_stderr: false,
            });
        }
    });
    let stderr_handle = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx_stderr.send(StreamedLine {
                line,
                is_stderr: true,
            });
        }
    });

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    // Silence watchdog. A child that stops producing output entirely (AV
    // scan wedge, stuck filesystem, hung post-download step) used to hang
    // this loop -- and the install wizard -- forever with no verdict; pip's
    // own `--timeout` only bounds socket reads, not the process (Aug 26/27
    // stall cohort). No output for the whole window => kill and fail the
    // attempt so the caller's retry/verdict machinery runs. `None` keeps the
    // old wait-forever behavior for callers whose children are legitimately
    // quiet for long stretches.
    let mut last_output = Instant::now();
    // Every caller of this function runs pip, so the unpack-phase check lives
    // here rather than behind a caller knob.
    let mut silence_limit = silence_timeout;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(streamed) => {
                last_output = Instant::now();
                silence_limit = widen_silence_for_unpack(silence_limit, &streamed.line);
                on_line(&streamed.line);
                let sink = if streamed.is_stderr {
                    &mut stderr_buf
                } else {
                    &mut stdout_buf
                };
                sink.push_str(&streamed.line);
                sink.push('\n');
                cap_capture(sink);
            }
            // Pipes closed: the child is exiting; fall through to wait().
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let Some(limit) = silence_limit else {
                    continue;
                };
                if last_output.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Do NOT join the reader threads here: an orphaned
                    // grandchild (sh's `sleep`, a wedged pip subprocess) can
                    // hold the pipe open indefinitely after the kill, and a
                    // blocked join would re-create the very hang this branch
                    // exists to end. The buffers already hold everything
                    // received; the readers exit on their own when the pipe
                    // finally closes.
                    stderr_buf.push_str(&format!(
                        "\n[headroom] killed: no output for {}s (stalled installer)\n",
                        limit.as_secs()
                    ));
                    return Err(anyhow::Error::new(CommandFailure {
                        program: binary.display().to_string(),
                        args: args.iter().map(|s| s.to_string()).collect(),
                        stdout: stdout_buf,
                        stderr: stderr_buf,
                        exit_code: None,
                        signal: None,
                    }));
                }
            }
        }
    }

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    let status = child
        .wait()
        .with_context(|| format!("waiting for {} {}", binary.display(), args.join(" ")))?;

    if !status.success() {
        return Err(anyhow::Error::new(CommandFailure {
            program: binary.display().to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdout: stdout_buf,
            stderr: stderr_buf,
            exit_code: status.code(),
            signal: exit_status_signal(&status),
        }));
    }

    Ok(())
}

struct StreamedLine {
    line: String,
    is_stderr: bool,
}

fn run_command_with_timeout(
    binary: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<()> {
    use std::io::Read;
    use std::sync::mpsc;

    let mut cmd = build_command(binary, args, cwd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {} {}", binary.display(), args.join(" ")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>();
    let stdout_handle = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().with_context(|| {
                        format!("waiting for {} {}", binary.display(), args.join(" "))
                    })?;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err).with_context(|| {
                    format!("waiting for {} {}", binary.display(), args.join(" "))
                });
            }
        }
    };

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    let stdout = String::from_utf8_lossy(&stdout_rx.recv().unwrap_or_default()).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_rx.recv().unwrap_or_default()).into_owned();

    if timed_out {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "command timed out after {}ms",
            timeout.as_millis()
        ));
        return Err(anyhow::Error::new(CommandFailure {
            program: binary.display().to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdout,
            stderr,
            exit_code: None,
            signal: exit_status_signal(&status),
        }));
    }

    if !status.success() {
        return Err(anyhow::Error::new(CommandFailure {
            program: binary.display().to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdout,
            stderr,
            exit_code: status.code(),
            signal: exit_status_signal(&status),
        }));
    }

    Ok(())
}

fn run_command(binary: &Path, args: &[&str], cwd: &Path) -> Result<()> {
    let output = build_command(binary, args, cwd)
        .output()
        .with_context(|| format!("starting {} {}", binary.display(), args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow::Error::new(CommandFailure {
            program: binary.display().to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code(),
            signal: exit_status_signal(&output.status),
        }));
    }

    Ok(())
}

/// Structured failure from a shell-out. Carried through `anyhow::Error` so callers
/// can `.context()` as usual, and capture sites (e.g. Sentry) can downcast to pull
/// stdout/stderr into structured fields instead of a truncated message string.
#[derive(Debug)]
pub struct CommandFailure {
    pub program: String,
    pub args: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// Unix signal number when the child was killed by a signal (`exit_code` is
    /// `None` in that case). Lets us tell SIGKILL (9 — likely parent shutdown,
    /// OOM, or launchd) from SIGTERM (15 — graceful kill) in failure reports.
    pub signal: Option<i32>,
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = match (self.exit_code, self.signal) {
            (Some(code), _) => format!("exit {}", code),
            (None, Some(sig)) => format!("killed by signal {}", sig),
            (None, None) => "killed by signal".to_string(),
        };
        write!(
            f,
            "command failed ({}): {} {}\nstdout:\n{}\nstderr:\n{}",
            status,
            self.program,
            self.args.join(" "),
            self.stdout,
            self.stderr
        )
    }
}

impl std::error::Error for CommandFailure {}

/// True when a failure is an older Codex CLI that predates `codex plugin add`
/// (`error: unrecognized subcommand 'add'`). Not retryable -- the user must
/// update Codex -- so install treats it as a soft skip + nudge, not an error.
fn is_outdated_codex(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CommandFailure>()
        .is_some_and(|failure| failure.stderr.contains("unrecognized subcommand"))
}

/// Extract the Unix signal number that killed a child, or `None` on non-Unix
/// or when the process exited normally. Used to populate `CommandFailure.signal`
/// so failure reports distinguish SIGKILL from SIGTERM.
fn exit_status_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// Returns true when an `anyhow::Error` from a `headroom <subcommand>` shell-out
/// looks like the venv is missing pinned dependencies — i.e. Python died with
/// `ModuleNotFoundError` or `ImportError` before the CLI could run. This is the
/// recovery signal for a partial install that left the receipt's
/// `requirementsLockSha256` stamped but the venv contents incomplete.
fn looks_like_corrupt_venv_error(err: &anyhow::Error) -> bool {
    let Some(failure) = err.downcast_ref::<CommandFailure>() else {
        return false;
    };
    let stderr = failure.stderr.as_str();
    stderr.contains("ModuleNotFoundError") || stderr.contains("ImportError")
}

/// Structured error emitted when the headroom proxy subprocess fails to open
/// its port. Capture sites downcast to pull the log tail into Sentry `extra`
/// fields, which are not subject to the 8KB message cap.
#[derive(Debug)]
pub struct HeadroomStartupFailure {
    pub program: String,
    pub args: Vec<String>,
    pub log_path: String,
    pub log_tail: String,
    pub reason: String,
}

/// The attempts that failed before the reported one, as "<exe>: <reason>"
/// pairs. Program basename and reason only: the args are the same spawn line
/// every variant shares, and a Windows venv path plus that line is ~210 chars
/// of noise per attempt, which is what pushed the reason and the onnx verdict
/// past the 400-char Sentry message cap (RUST-BX, RUST-BV). Full command
/// lines stay in the local log.
fn prior_attempts_summary(failures: &[HeadroomStartupFailure]) -> String {
    if failures.is_empty() {
        return String::new();
    }
    let joined = failures
        .iter()
        .map(|f| {
            let exe = std::path::Path::new(&f.program)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| f.program.clone());
            format!("{exe}: {}", f.reason)
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(" (prior attempts: {joined})")
}

impl std::fmt::Display for HeadroomStartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Reason first: this Display is the tail of the Sentry message and
        // the program path plus args alone can exhaust the 400-char cap.
        write!(
            f,
            "{} ({} {}; log: {}){}",
            self.reason,
            self.program,
            self.args.join(" "),
            self.log_path,
            if self.log_tail.is_empty() {
                String::new()
            } else {
                format!("\n--- log tail ---\n{}\n--- end log ---", self.log_tail)
            }
        )
    }
}

impl std::error::Error for HeadroomStartupFailure {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use chrono::Local;

    #[cfg(windows)]
    use super::python_distribution_artifact;
    use super::rotate_log_if_large;
    use super::stalled_prefetch_cause;
    use super::{
        acquire_artifact_download_lock, publish_inflight_download, ARTIFACT_DOWNLOAD_LOCK,
    };
    use super::{
        addon_unavailable_reason, apply_serena_dashboard_interface, apply_serena_gitignore,
        bootstrap_requirements_lock_for_target, build_command, cc_switch_proxy_url,
        cc_switch_reconcile_for_spawn, classify_kompress_prefetch_failure,
        codebase_memory_distribution_artifact, compact_pip_failure, describe_proxy_port_occupant,
        diagnose_proxy_port, exe_path_is_under, extract_required_pydantic_core_version,
        format_all_foreign_bail, format_already_running_bail, headroom_entrypoint_startup_args,
        headroom_python_startup_args, httpx_ca_bundle_bridge_from, is_checksum_mismatch,
        is_outdated_codex, learned_openai_ttl_seconds, ledger_bytes_without_control,
        looks_like_corrupt_venv_error, parse_lsof_listener, parse_major_minor_patch,
        parse_netstat_listener, parse_pid_from_lsof_detail, parse_ss_listener,
        parse_tasklist_image, path_with_binary_dir, pending_addon_update, pinned_headroom_release,
        pip_failure_category, pip_line_to_progress, plugin_install_failure_category,
        pre_upstream_concurrency, probe_backend_readyz_ok, proxy_argv_contains_expected_flags,
        purge_legacy_output_savings_control_arm_once, read_headroom_learn_metadata_from_path,
        receipt_requires_atomic_rebuild, reclaim_orphan_proxy, redact_sensitive,
        requirements_lock_package_count, requirements_lock_sha, rtk_distribution_artifact,
        run_command, sanitize_log_variant, savings_profile_for_runtime, settle_unowned_port,
        sha256_bytes, summarize_kompress_prefetch_failure, upstream_spawn_env, verify_sha256_file,
        wait_for_port_free, wheel_download_failure_category, widen_silence_for_unpack,
        CommandFailure, HeadroomRelease, ManagedRuntime, PipOutputCapture, PortState, ToolManager,
        UpgradeOutcome, ATOMIC_REBUILD_FLOOR_VERSION, HEADROOM_LINUX_REQUIREMENTS_LOCK,
        HEADROOM_PINNED_VERSION, HEADROOM_REQUIREMENTS_LOCK, HEADROOM_WINDOWS_REQUIREMENTS_LOCK,
        MARKITDOWN_PINNED_VERSION, PIP_UNPACK_SILENCE_TIMEOUT, PLUGIN_ADDONS,
        PLUGIN_DISPLAY_VERSION, RTK_VERSION, UNKNOWN_OCCUPANT,
    };
    use super::{is_python_interpreter, log_tail, path_without_dirs};
    use crate::backend_port;
    use crate::models::ManagedTool;
    use crate::port_conflict;
    use std::net::TcpListener;

    fn write_ttl_obs(dir: &Path, rows: &[(&str, f64, bool, &str)]) -> PathBuf {
        let path = dir.join("cache_ttl_observations.jsonl");
        let mut lines: Vec<String> = rows
            .iter()
            .map(|(provider, idle, is_miss, reason)| {
                format!(
                    r#"{{"ts":1000,"provider":"{provider}","model":"gpt-5.5","reason":"{reason}","idle_seconds":{idle},"ttl_assumed":300,"is_miss":{is_miss},"cache_read":0,"expected_cached":1000}}"#
                )
            })
            .collect();
        lines.push("not-json".into());
        fs::write(&path, lines.join("\n")).expect("write obs");
        path
    }

    #[test]
    fn learned_openai_ttl_picks_smallest_death_beyond_largest_life() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_ttl_obs(
            dir.path(),
            &[
                ("openai", 120.0, false, "hit"),
                ("openai", 300.0, false, "hit"),
                ("openai", 480.0, false, "hit"),
                // Death below the max hit says nothing; 600 is the safe bound.
                ("openai", 400.0, true, "ttl_expiry"),
                ("openai", 600.0, true, "ttl_expiry"),
                ("openai", 900.0, true, "ttl_expiry"),
                // Non-openai rows and non-ttl_expiry misses are ignored.
                ("anthropic", 2000.0, false, "hit"),
                ("openai", 550.0, true, "prefix_change"),
            ],
        );
        assert_eq!(learned_openai_ttl_seconds(&path), Some(600));
    }

    #[test]
    fn learned_openai_ttl_none_without_safe_upper_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Every observed death overlaps the life range.
        let overlap = write_ttl_obs(
            dir.path(),
            &[
                ("openai", 120.0, false, "hit"),
                ("openai", 480.0, false, "hit"),
                ("openai", 900.0, false, "hit"),
                ("openai", 400.0, true, "ttl_expiry"),
                ("openai", 450.0, true, "ttl_expiry"),
                ("openai", 600.0, true, "ttl_expiry"),
            ],
        );
        assert_eq!(learned_openai_ttl_seconds(&overlap), None);
        // Thin samples (2 hits < 3) and a missing file also yield None.
        let thin = write_ttl_obs(
            dir.path(),
            &[
                ("openai", 120.0, false, "hit"),
                ("openai", 480.0, false, "hit"),
                ("openai", 600.0, true, "ttl_expiry"),
                ("openai", 700.0, true, "ttl_expiry"),
                ("openai", 800.0, true, "ttl_expiry"),
            ],
        );
        assert_eq!(learned_openai_ttl_seconds(&thin), None);
        assert_eq!(
            learned_openai_ttl_seconds(&dir.path().join("missing.jsonl")),
            None
        );
    }

    #[test]
    fn learned_openai_ttl_clamps_to_documented_bounds() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Observed death beyond OpenAI's documented 1h max: clamp to 3600.
        let path = write_ttl_obs(
            dir.path(),
            &[
                ("openai", 120.0, false, "hit"),
                ("openai", 300.0, false, "hit"),
                ("openai", 480.0, false, "hit"),
                ("openai", 5000.0, true, "ttl_expiry"),
                ("openai", 6000.0, true, "ttl_expiry"),
                ("openai", 7000.0, true, "ttl_expiry"),
            ],
        );
        assert_eq!(learned_openai_ttl_seconds(&path), Some(3600));
    }

    #[test]
    fn serena_gitignore_add_remove_roundtrip() {
        // Empty file -> block appended; missing trailing newline is repaired.
        let added = apply_serena_gitignore("", true).expect("append to empty");
        assert!(added.ends_with(".serena/\n"));
        let from_existing = apply_serena_gitignore("*.log", true).expect("append after content");
        assert!(from_existing.starts_with("*.log\n"));
        assert!(from_existing.ends_with(".serena/\n"));

        // Idempotent: already present -> no rewrite.
        assert!(apply_serena_gitignore(&added, true).is_none());
        assert!(apply_serena_gitignore(".serena/\n", true).is_none());

        // Removal restores the original bytes exactly.
        assert_eq!(
            apply_serena_gitignore(&from_existing, false).expect("remove block"),
            "*.log\n"
        );

        // A hand-written pattern with no marker is the user's - left alone.
        assert!(apply_serena_gitignore(".serena/\n*.log\n", false).is_none());
    }

    #[test]
    fn apply_serena_dashboard_interface_replaces_only_unset_default() {
        // Unset key (serena's generated default) -> forced to browser.
        assert_eq!(
            apply_serena_dashboard_interface("projects: []\nweb_dashboard_interface:\n")
                .expect("replace unset"),
            "projects: []\nweb_dashboard_interface: browser\n"
        );
        assert_eq!(
            apply_serena_dashboard_interface("web_dashboard_interface: null # default\n")
                .expect("replace null"),
            "web_dashboard_interface: browser\n"
        );

        // Missing file -> minimal config with the required projects key.
        assert_eq!(
            apply_serena_dashboard_interface("").expect("create minimal"),
            "projects: []\nweb_dashboard_interface: browser\n"
        );

        // Existing config without the key -> appended, projects not duplicated.
        assert_eq!(
            apply_serena_dashboard_interface("projects: []\nlog_level: 20\n")
                .expect("append to existing"),
            "projects: []\nlog_level: 20\nweb_dashboard_interface: browser\n"
        );

        // Explicit user choice (or already browser) -> left alone.
        assert!(apply_serena_dashboard_interface("web_dashboard_interface: app\n").is_none());
        assert!(apply_serena_dashboard_interface("web_dashboard_interface: browser\n").is_none());
        // Commented-out template line is not the key.
        assert_eq!(
            apply_serena_dashboard_interface("projects: []\n# web_dashboard_interface:\n")
                .expect("append despite comment"),
            "projects: []\n# web_dashboard_interface:\nweb_dashboard_interface: browser\n"
        );
    }

    #[test]
    fn ledger_purge_clears_nonempty_control_only() {
        // Non-empty control -> rewrite with control emptied; baseline/treatment kept.
        let with_control =
            br#"{"baseline":{"glob":{"n":5}},"treatment":{"k":{"n":3}},"control":{"k":{"n":2}}}"#;
        let out = ledger_bytes_without_control(with_control).expect("rewrite when control present");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["control"].as_object().unwrap().len(), 0);
        assert_eq!(v["treatment"]["k"]["n"], 3);
        assert_eq!(v["baseline"]["glob"]["n"], 5);

        // Empty control, absent control, and unparseable input -> no rewrite.
        assert!(ledger_bytes_without_control(br#"{"control":{}}"#).is_none());
        assert!(ledger_bytes_without_control(br#"{"treatment":{"k":{"n":1}}}"#).is_none());
        assert!(ledger_bytes_without_control(b"{not json").is_none());
    }

    #[test]
    fn legacy_control_arm_is_purged_once_then_left_to_fill() {
        let root = std::env::temp_dir().join(format!("headroom-purge-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _guard = HomeGuard::new(&root);
        let ledger = root.join(".headroom").join("output_savings.json");
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        let with_control =
            br#"{"baseline":{"glob":{"n":5}},"treatment":{"k":{"n":3}},"control":{"k":{"n":2}}}"#;

        // Samples from the abandoned 1% holdout go.
        std::fs::write(&ledger, with_control).unwrap();
        purge_legacy_output_savings_control_arm_once();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(v["control"].as_object().unwrap().len(), 0);

        // The 3% arm now fills on every proxy spawn. Purging again would empty
        // it as fast as it fills, so the stamp has to hold.
        std::fs::write(&ledger, with_control).unwrap();
        purge_legacy_output_savings_control_arm_once();
        assert_eq!(std::fs::read(&ledger).unwrap(), with_control);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_checksum_mismatch_detects_bail_through_context_layers() {
        // Pins the string coupling with download_to_path_with_progress's
        // bail! — reword one side and the unverified-fallback guard dies.
        let err = anyhow::anyhow!("checksum mismatch for https://x: expected aa, got bb")
            .context("downloading wheel");
        assert!(is_checksum_mismatch(&err));
        assert!(!is_checksum_mismatch(&anyhow::anyhow!(
            "connection reset by peer"
        )));
    }

    #[test]
    fn sitecustomize_registers_nonchaining_sigusr1_dump() {
        assert!(super::SITECUSTOMIZE_PY
            .contains("faulthandler.register(signal.SIGUSR1, all_threads=True)"));
        // chain=True would fall through to SIGUSR1's default terminate action.
        assert!(!super::SITECUSTOMIZE_PY.contains("chain=True)"));
    }

    #[test]
    fn sitecustomize_keeps_user_turn_text_out_of_lossy_compression() {
        // From headroom-ai 0.34.0 the coding persona compresses user-role
        // text blocks and no env can disable it. Verbatim user turns are
        // deliberate desktop posture (see the SITECUSTOMIZE_PY docstring);
        // the injection must flip the persona field back.
        assert!(super::SITECUSTOMIZE_PY.contains("compress_user_messages=False"));
        assert!(super::SITECUSTOMIZE_PY.contains(r#"_PROFILES.get("coding")"#));
        // Must stay guarded: fallback runtimes (< 0.30.0) lack the persona.
        assert!(super::SITECUSTOMIZE_PY.contains("except Exception:"));
    }

    #[test]
    fn sitecustomize_dropped_patches_upstreamed_in_0_35_0() {
        // The breakpoint-diagnostics / payload-preview / expansion-guard /
        // marker-mirroring patches shipped upstream in 0.35.0 (#2919);
        // keeping the desktop copies would double-log cache_breakpoints
        // and shadow the native implementations.
        assert!(!super::SITECUSTOMIZE_PY.contains("event=cache_breakpoints"));
        assert!(!super::SITECUSTOMIZE_PY.contains("HEADROOM_LOG_PAYLOAD_PREVIEW"));
        assert!(!super::SITECUSTOMIZE_PY.contains("normalize_message_cache_control"));
    }

    /// SemanticCache.set stores any body gated only on `status_code == 200`,
    /// so one empty/unparseable/error response is replayed for the full TTL
    /// (1h) and only a proxy restart clears it — the amplifier that turned a
    /// single bad upstream response into a wedged session on 2026-08-13.
    #[test]
    fn sitecustomize_guards_response_cache_against_poisoning() {
        let py = super::SITECUSTOMIZE_PY;
        // Must patch the class the proxy actually instantiates
        // (headroom.proxy.semantic_cache.SemanticCache, not cache.semantic).
        assert!(py.contains("import headroom.proxy.semantic_cache as _hd_sc"));
        assert!(py.contains("_hd_sc.SemanticCache.set = _hd_sc_set"));
        // The wrapper must stay async: callers `await self.cache.set(...)`.
        assert!(py.contains("async def _hd_sc_set("));
        assert!(py.contains("await _hd_sc_orig_set("));
        // Error payloads under a 200 are the worst case to cache.
        assert!(py.contains(r#"parsed.get("type") == "error""#));
        assert!(py.contains("HEADROOM_RESPONSE_CACHE_GUARD"));
        // Backend process only: it imports the proxy stack.
        assert!(py.contains(r#"environ.get("HEADROOM_SDK") == "headroom-desktop-proxy""#));
    }

    /// Upstream PR #3106: /v1/responses HTTP outcomes derive optimized from
    /// a messages-only original while tokens_saved includes tool-schema
    /// compaction, clamping optimized to 0 and recording >100% savings
    /// rates. Seams verified against the installed 0.35.0 wheel by
    /// scratchpad verify script on 2026-08-18 (both WS executor call sites
    /// pass `timeout=`, the HTTP site does not; streaming and buffered HTTP
    /// both record through HeadroomProxy._record_request_outcome).
    #[test]
    fn sitecustomize_ports_responses_denominator_guard() {
        let py = super::SITECUSTOMIZE_PY;
        // Both seams patched on the classes the proxy actually uses.
        assert!(py.contains(
            "_hd_rd_openai.OpenAIHandlerMixin._compress_openai_responses_payload_in_executor = ("
        ));
        assert!(py.contains("_hd_rd_server.HeadroomProxy._record_request_outcome = _hd_rd_record"));
        // WS exclusion, both layers: stash-time kwarg check + repair-time tag.
        assert!(py.contains(r#""timeout" not in kwargs"#));
        assert!(py.contains(r#"("endpoint") != "responses_ws""#));
        // Coherence precondition: only the old derivation shape is repaired,
        // so a wheel shipping #3106 natively (also version-gated) and WS
        // delta records pass through untouched.
        assert!(py.contains("== max(0, outcome.original_tokens - outcome.tokens_saved)"));
        assert!(py.contains("if _hd_rd_ver < (0, 36):"));
        assert!(py.contains("HEADROOM_RESPONSES_DENOMINATOR_GUARD"));
        // The stash must be bounded: entries for failed requests never pop.
        assert!(py.contains("_HD_RD_MAX"));
    }

    #[test]
    fn sitecustomize_unfolds_tool_schema_dollars() {
        // Upstream PR #3170 not yet in a wheel: 0.36.0's attribution
        // unification folds priced tool-schema dollars into
        // compression_savings_usd while every token field beside it stays
        // message-only, inflating any $/token read on the persisted state by
        // 1 + tool/message (5.59x measured; an implied $32.88/M next to
        // models listing at $10/M). The guard zeroes the tool_schema bucket
        // before the fold so the desktop's lifetime dollars keep their
        // 0.35.0 meaning on the 0.37.0 wheel.
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_SAVINGS_FOLD_GUARD"));
        assert!(py.contains("_hd_sf_st.SavingsTracker.record_request = _hd_sf_record"));
        // The unfold works on a copy -- the caller's mapping is not mutated.
        assert!(py.contains("unfolded = dict(priced)"));
        assert!(py.contains(r#"unfolded["tool_schema"] = 0.0"#));
        // Self-neutralizes on the first wheel shipping #3170's disjoint
        // fields, so a proper upstream split is never zeroed.
        assert!(
            py.contains(r#""tool_schema_savings_usd" not in _hd_sf_st._empty_display_session()"#)
        );
        // Only the dollar fold is undone; the token side must pass through.
        assert!(!py.contains(r#"kwargs["tool_search_saved"]"#));
        // The 0.37.0 wheel handles Codex additional_tools natively (#3186 +
        // #3194), so the desktop lift is gone with the pin bump.
        assert!(!py.contains("HEADROOM_ADDITIONAL_TOOLS_GUARD"));
        assert!(!py.contains("additional_tools"));
    }

    #[test]
    fn sitecustomize_ports_read_chain_guard() {
        // Upstream PR #2668 not yet in a wheel: _is_read_command matches only
        // the first program and applies its write check whole-string, so a
        // read batched behind other work (`wc -l a.py && sed -n '1,60p'
        // a.py`) is lossy-compressed despite read protection -- the exact
        // re-read/resolve-loss failure the protection exists to prevent.
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_READ_CHAIN_GUARD"));
        assert!(py.contains("upstream PR #2668"));
        // Split on ; && and || -- never on a single | (pipelines reduce to
        // their first stage instead).
        assert!(py.contains(r#"_hd_rc_re.compile(r"\|\||&&|;")"#));
        assert!(py.contains(r#"seg.split("|", 1)[0].strip()"#));
        // Redirect/tee are judged per SEGMENT so a sibling write cannot
        // unprotect the read next to it; the heredoc bailout stays
        // whole-string because its body may contain separators.
        assert!(py.contains(r#"(^|\s)(>>?|tee\b)"#));
        assert!(py.contains(r#"(^|\s)<<"#));
        // Each segment delegates to the ORIGINAL parser (wrapper peeling,
        // bash -c recursion, lockfile carve-out stay upstream's), and the
        // rebind covers both in-module call sites via module globals.
        assert!(py.contains("_hd_rc_orig(first)"));
        assert!(py.contains("_hd_rc_cr._is_read_command = _hd_rc_is_read"));
    }

    #[test]
    fn sitecustomize_ports_prefix_replay_guard() {
        // Upstream issue #3379: the 0.36.x non-inflation bound declines the
        // cached-prefix replay exactly when background compression shrinks
        // already-forwarded history, busting the provider prompt cache every
        // time kompress lands. The guard stubs the sizing helper per call so
        // the bound never fires, and goes inert once a wheel ships the
        // enforce_non_inflation parameter.
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_PREFIX_REPLAY_GUARD"));
        assert!(py.contains("upstream issue #3379"));
        // Gated on the fix's parameter being ABSENT under either name the
        // upstream PR shipped (v1 flag, reworked floor), so a fixed wheel
        // keeps its own replay policy.
        assert!(py.contains("in _hd_pr_pt.overlay_cached_prefix.__code__.co_varnames"));
        assert!(py.contains(r#""enforce_non_inflation", "confirmed_frozen_count""#));
        // Two-pass probe: bound-on first (a shrinking repair is accepted
        // as-is), then stubbed sizing so the replay always wins -- v0.35.0's
        // policy, which has no bound at all. The floor-limited variant
        // shipped in 0.9.4 measured WORSE than no guard (fleet cache
        // coverage 1.20 -> 0.94) because bytes past the floor still bust, so
        // there must be no partial splice here.
        assert!(py.contains("if r1 is not optimized:"));
        assert!(py.contains(r#"_hd_pr_pt._compact_json_bytes = lambda value: b"""#));
        assert!(py.contains("_hd_pr_pt._compact_json_bytes = _hd_pr_saved"));
        assert!(py.contains("_hd_pr_pt.overlay_cached_prefix = _hd_pr_overlay"));
        assert!(
            !py.contains("_hd_pr_floors"),
            "floor splice must not come back"
        );
        assert!(
            !py.contains("r2[:floor]"),
            "partial replay leaves busts past the floor"
        );
    }

    #[test]
    fn sitecustomize_vendors_pr3380_floor_semantics() {
        // Upstream PR #3380 vendored verbatim against the 0.37.0 pin: the
        // patched overlay_cached_prefix + finalize_turn are exec'd wholesale,
        // never reimplemented (the 0.9.4-rc.4 splice was a reimplementation
        // and lost 22% of fleet cache coverage). Functional evidence:
        // scratchpad test_vendor.py T1-T6 against the installed 0.37.0 venv,
        // 2026-09-02.
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_PR3380_VENDOR"));
        // Exact-pin + fixed-parameter gates: any other wheel keeps its own
        // replay policy.
        assert!(py.contains(r#"_hd_v_meta.version("headroom-ai") == "0.37.0""#));
        assert!(py.contains("in _hd_v_pt.overlay_cached_prefix.__code__.co_varnames"));
        // The vendored signature is the PR's, which is also what flips the
        // prefix-replay guard's own gate to inert.
        assert!(py.contains("confirmed_frozen_count: int | None = None"));
        // The vendor must BIND before the replay guard reads the signature.
        let vendor_code = py.rfind("HEADROOM_PR3380_VENDOR").unwrap();
        let guard_code = py.rfind("HEADROOM_PREFIX_REPLAY_GUARD").unwrap();
        assert!(
            vendor_code < guard_code,
            "vendor block must precede the replay guard"
        );
        // Floor bridge: pre-clamp tracker_frozen in, one-shot consume out,
        // full-replay fallback (the 0.9.5 posture) for floorless callers.
        assert!(py.contains(r#"if "tracker_frozen" in kwargs:"#));
        assert!(py.contains("_hd_v_floor.set(None)"));
        assert!(py.contains("_hd_v_f = len(prev_returned or [])"));
    }

    #[test]
    fn sitecustomize_vendors_tool_search_history_repair() {
        // The tool_reference 400 ("... not found in available tools") is caused
        // by a referenced tool being ABSENT from the request's tools array; a
        // deferred-but-present tool is valid. The vendor keys on absence and
        // covers the client-side tool_result+tool_reference shape the wheel
        // repair does not scan, delegating the server-side shape to the wheel
        // with tools UNFILTERED (reverting rc.5's defer_loading filter).
        // Behaviour is proven by
        // tool_search_history_repair_behaves_against_the_installed_wheel; this
        // pins the shape.
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_TOOL_SEARCH_REPAIR"));
        // rc.5's wrong defer_loading filter must be gone.
        assert!(!py.contains("HEADROOM_TOOL_SEARCH_DEFER_REPAIR"));
        assert!(!py.contains("_hd_tsr_orig(messages, filtered)"));
        // Exact-pin gated: any other wheel keeps its own repair.
        assert!(py.contains(r#"_hd_tsr_meta.version("headroom-ai") == "0.37.0""#));
        // Client-side pass keyed on absence, delegating server-side unfiltered.
        assert!(py.contains("def _hd_tsr_client_side(messages, tools):"));
        assert!(py.contains("removed_client + removed_server"));
        // It must reassign the module symbol the handler late-imports.
        assert!(
            py.contains("_hd_tsr_helpers.strip_unsupported_tool_search_blocks = _hd_tsr_wrapped")
        );
    }

    #[test]
    fn sitecustomize_vendors_tool_ref_hint() {
        // The residual tool_reference 400 gets a "start a new session" hint.
        // Behaviour is proven by tool_ref_hint_behaves_against_the_installed_wheel;
        // this pins the shape.
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_TOOL_REF_HINT"));
        assert!(py.contains(r#"_hd_hint_meta.version("headroom-ai") == "0.37.0""#));
        // Wraps the buffered-error seam and post-processes its Response.
        assert!(py.contains("_hd_hint_mod.StreamingMixin._stream_response = _hd_hint_wrapped"));
        assert!(py.contains("def _hd_hint_apply(result):"));
        // Assert the load-bearing marker the idempotency guard keys on, not the
        // user-facing copy (which is free to change without breaking this test).
        assert!(py.contains(r#"b"Headroom:" not in body"#));
    }

    #[test]
    fn sitecustomize_ports_context_limit_guard() {
        // Upstream PR #2942: without the guard, long sessions degrade into a
        // compact-every-other-prompt loop once the compressed request hits
        // the model's real window (churn report 2026-08-12). Verified against
        // installed 0.34.0 by scratchpad/verify_sitecustomize.py on 2026-08-12;
        // all three patched seams re-verified present with unchanged
        // signatures in the 0.35.0 wheel on 2026-08-13 (#2942 still open).
        let py = super::SITECUSTOMIZE_PY;
        // The guard imports the full proxy stack; it must only run in the
        // backend process, never in markitdown or other venv Pythons.
        assert!(py.contains(r#"environ.get("HEADROOM_SDK") == "headroom-desktop-proxy""#));
        // All three seams are patched...
        assert!(
            py.contains("_hd_cg_stream.StreamingMixin._stream_response = _hd_cg_stream_response")
        );
        assert!(py.contains("_hd_cg_prov.AnthropicProvider.get_context_limit = _hd_cg_limit"));
        assert!(py.contains(
            "_hd_cg_anth.AnthropicHandlerMixin.handle_anthropic_messages = _hd_cg_handle"
        ));
        // ...the real window is learned from prompt-too-long 400 bodies,
        // keyed by 1m-beta presence so a clamped account never poisons one
        // with real 1M access...
        assert!(py.contains("prompt is too long"));
        assert!(py.contains("_hd_cg_learned.get((model, _hd_cg_has_1m(beta)))"));
        // ...the nudge covers BOTH usage-bearing events -- Claude Code merges
        // the final cumulative-usage message_delta over message_start
        // (verified live 2026-08-12), so rewriting only the first loses the
        // merge...
        assert!(py.contains("_rewrite_start"));
        assert!(py.contains("_rewrite_delta"));
        // ...and the kill switch is honored.
        assert!(py.contains("HEADROOM_CONTEXT_GUARD"));
    }

    /// Behavioural check of the vendored #3380 prefix floor, against the REAL
    /// installed wheel rather than the string of the file.
    ///
    /// Every other test of this machinery asserts `py.contains(...)`. That is
    /// what the 0.9.4 splice had, and it passed while the change cost 89
    /// installs ~17pp of their savings rate: presence is not behaviour. This
    /// writes the SITECUSTOMIZE_PY we are about to ship into a temp dir, points
    /// the managed venv at it, and asserts the floor's actual properties --
    /// including the ContextVar bridge from prepare_turn to finalize_turn,
    /// which is the novel part and the part with no upstream coverage.
    ///
    /// Self-skips where the managed runtime is absent (CI, a fresh clone), so
    /// it costs nothing there and runs automatically on any machine that has
    /// the backend installed. That is deliberate: an `#[ignore]` test only
    /// protects you when somebody remembers to type `--ignored`.
    #[test]
    fn vendored_prefix_floor_behaves_against_the_installed_wheel() {
        let python =
            ManagedRuntime::bootstrap_root(&crate::storage::app_data_dir()).managed_python();
        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("verify-prefix-floor.py");
        if !python.exists() || !probe.exists() {
            eprintln!("skipping: no managed runtime at {}", python.display());
            return;
        }

        let dir = std::env::temp_dir().join(format!("hd-prefix-floor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp inject dir");
        std::fs::write(dir.join("sitecustomize.py"), super::SITECUSTOMIZE_PY)
            .expect("write sitecustomize");

        let out = crate::proc::command(&python)
            .arg(&probe)
            .env("PYTHONPATH", &dir)
            .env("HEADROOM_SDK", "headroom-desktop-proxy")
            .output()
            .expect("run prefix-floor probe");
        let _ = std::fs::remove_dir_all(&dir);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A wheel that already ships #3380 leaves the vendor inert by design;
        // that is a pass upstream, not a regression here.
        if stdout.contains("FAIL vendor bound") && stderr.is_empty() {
            eprintln!("skipping: vendor did not bind (wheel is not the 0.37.0 pin)");
            return;
        }
        assert!(
            out.status.success(),
            "vendored prefix floor misbehaved against the installed wheel.\n\
             This is the machinery that caused the 0.9.4 regression -- do not \
             silence it.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn first_appearance_accounting_behaves_against_the_installed_wheel() {
        let python =
            ManagedRuntime::bootstrap_root(&crate::storage::app_data_dir()).managed_python();
        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("verify-first-appearance.py");
        if !python.exists() || !probe.exists() {
            eprintln!("skipping: no managed runtime at {}", python.display());
            return;
        }

        let dir = std::env::temp_dir().join(format!("hd-first-appear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp inject dir");
        std::fs::write(dir.join("sitecustomize.py"), super::SITECUSTOMIZE_PY)
            .expect("write sitecustomize");

        let out = crate::proc::command(&python)
            .arg(&probe)
            .env("PYTHONPATH", &dir)
            .env("HEADROOM_SDK", "headroom-desktop-proxy")
            .output()
            .expect("run first-appearance probe");
        let _ = std::fs::remove_dir_all(&dir);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A wheel that ships first-appearance counting upstream (or a bump
        // past the 0.37.0 pin) leaves this vendor inert by design.
        if stdout.contains("FAIL fa bound") && stderr.is_empty() {
            eprintln!("skipping: first-appearance vendor did not bind (not the 0.37.0 pin)");
            return;
        }
        assert!(
            out.status.success() && stdout.contains("OK first-appearance"),
            "first-appearance accounting misbehaved against the installed wheel.\n\
             If traffic neutrality failed, do NOT ship: that is the 0.9.4 class\n\
             of mistake.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn tool_search_history_repair_behaves_against_the_installed_wheel() {
        // The tool_reference 400 ("... not found in available tools") lived in
        // the WHEEL's history repair, not the string blob. This runs the shipped
        // sitecustomize against the installed wheel and asserts absence-keyed
        // handling across BOTH block shapes: client-side absent neutralized,
        // deferred-but-present kept (both shapes), server-side absent dropped,
        // kill switch reverts client-side coverage.
        let python =
            ManagedRuntime::bootstrap_root(&crate::storage::app_data_dir()).managed_python();
        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("verify-tool-search-repair.py");
        if !python.exists() || !probe.exists() {
            eprintln!("skipping: no managed runtime at {}", python.display());
            return;
        }

        let dir =
            std::env::temp_dir().join(format!("hd-tool-search-repair-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp inject dir");
        std::fs::write(dir.join("sitecustomize.py"), super::SITECUSTOMIZE_PY)
            .expect("write sitecustomize");

        let out = crate::proc::command(&python)
            .arg(&probe)
            .env("PYTHONPATH", &dir)
            .env("HEADROOM_SDK", "headroom-desktop-proxy")
            .output()
            .expect("run tool-search-repair probe");
        let _ = std::fs::remove_dir_all(&dir);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A wheel that ships the fix upstream (or a bump past the 0.37.0 pin)
        // leaves this vendor inert by design.
        if stdout.contains("FAIL tsr bound") && stderr.is_empty() {
            eprintln!("skipping: tool-search repair vendor did not bind (not the 0.37.0 pin)");
            return;
        }
        assert!(
            out.status.success() && stdout.contains("OK tool-search repair"),
            "tool-search history repair misbehaved against the installed wheel.\n\
             If a client-side absent reference survived, the tool_reference 400 is\n\
             back; if a deferred+present reference was dropped, rc.5's regression\n\
             is back.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn tool_ref_hint_behaves_against_the_installed_wheel() {
        // The residual tool_reference 400 that reaches the user gets a Headroom
        // "start a new session" hint appended. Runs the shipped sitecustomize
        // against the installed wheel's starlette and asserts the transform is
        // valid, content-length-correct, idempotent, and scoped to 400s carrying
        // the signature.
        let python =
            ManagedRuntime::bootstrap_root(&crate::storage::app_data_dir()).managed_python();
        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("verify-tool-ref-hint.py");
        if !python.exists() || !probe.exists() {
            eprintln!("skipping: no managed runtime at {}", python.display());
            return;
        }

        let dir = std::env::temp_dir().join(format!("hd-tool-ref-hint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp inject dir");
        std::fs::write(dir.join("sitecustomize.py"), super::SITECUSTOMIZE_PY)
            .expect("write sitecustomize");

        let out = crate::proc::command(&python)
            .arg(&probe)
            .env("PYTHONPATH", &dir)
            .env("HEADROOM_SDK", "headroom-desktop-proxy")
            .output()
            .expect("run tool-ref-hint probe");
        let _ = std::fs::remove_dir_all(&dir);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("FAIL hint bound") && stderr.is_empty() {
            eprintln!("skipping: tool-ref hint vendor did not bind (not the 0.37.0 pin)");
            return;
        }
        assert!(
            out.status.success() && stdout.contains("OK tool-ref hint"),
            "tool-ref hint vendor misbehaved against the installed wheel.\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn read_maturation_defaults_on_with_falsey_kill_switch() {
        // Default-on since 0.9.7-rc.1: an untouched install requests the
        // feature from the wheel's beta ring.
        assert_eq!(
            super::read_maturation_env_from(None),
            vec![("HEADROOM_READ_MATURATION".to_string(), "1".to_string())]
        );

        // The no-rebuild kill switch: falsey spellings mean off. This is
        // cache-breakpoint machinery (the class that cost 89 installs ~17pp
        // of their savings rate on 0.9.4), so disabling it must never
        // require an update.
        for off in ["", "  ", "0", "false", "FALSE", "no", "off", "Off"] {
            assert!(
                super::read_maturation_env_from(Some(off)).is_empty(),
                "{off:?} should disable read maturation"
            );
        }

        // Explicit values still pass through verbatim.
        assert_eq!(
            super::read_maturation_env_from(Some("1")),
            vec![("HEADROOM_READ_MATURATION".to_string(), "1".to_string())]
        );
        assert_eq!(
            super::read_maturation_env_from(Some(" true ")),
            vec![("HEADROOM_READ_MATURATION".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn pre_upstream_concurrency_stays_within_bounds() {
        let value = pre_upstream_concurrency();
        assert!((8..=32).contains(&value), "got {value}");
    }

    #[test]
    fn httpx_ca_bundle_bridge_mirrors_requests_bundle() {
        // Bundle set, SSL_CERT_FILE unset -> mirror it for httpx.
        assert_eq!(
            httpx_ca_bundle_bridge_from(false, Some("/etc/corp/ca.pem")),
            vec![("SSL_CERT_FILE".to_string(), "/etc/corp/ca.pem".to_string())]
        );
        // SSL_CERT_FILE already set -> don't override.
        assert!(httpx_ca_bundle_bridge_from(true, Some("/etc/corp/ca.pem")).is_empty());
        // No bundle, or blank -> nothing to bridge.
        assert!(httpx_ca_bundle_bridge_from(false, None).is_empty());
        assert!(httpx_ca_bundle_bridge_from(false, Some("  ")).is_empty());
    }

    /// RUST-A0/RUST-8K: a foreign `libcrypto-3-x64.dll` on PATH aborts our
    /// interpreter before pip runs. Drop those directories from the child's
    /// PATH -- but only for the interpreter, and never the one we spawn from.
    #[test]
    fn foreign_openssl_dirs_are_dropped_from_the_interpreter_path() {
        let dir = tempfile::tempdir().unwrap();
        let wamp = dir.path().join("wamp64").join("php");
        let innocent = dir.path().join("tools");
        let runtime = dir.path().join("runtime").join("Scripts");
        for d in [&wamp, &innocent, &runtime] {
            std::fs::create_dir_all(d).unwrap();
        }
        // The interpreter ships its own copy beside itself; it must survive.
        std::fs::write(wamp.join("libcrypto-3-x64.dll"), b"foreign").unwrap();
        std::fs::write(runtime.join("libcrypto-3-x64.dll"), b"ours").unwrap();

        let path_var = std::env::join_paths([&wamp, &innocent, &runtime]).unwrap();
        let foreign = crate::conflicting_openssl_dirs(&path_var.to_string_lossy());
        assert!(foreign.contains(&wamp.display().to_string()));

        let filtered = path_without_dirs(&path_var, &foreign);
        let kept: Vec<PathBuf> = std::env::split_paths(&filtered).collect();
        assert!(!kept.contains(&wamp), "the foreign dir is dropped");
        assert!(kept.contains(&innocent), "unrelated dirs are kept");

        // And prepending puts the interpreter's own dir back at the front even
        // when the scan flagged it, so a DLL we ship is never the casualty.
        let python = runtime.join("python.exe");
        let restored = crate::proc::path_with_dir_prepended_to(python.parent().unwrap(), &filtered);
        assert_eq!(
            std::env::split_paths(&restored).next(),
            Some(runtime.clone())
        );

        // Nothing to drop leaves PATH byte-identical.
        assert_eq!(path_without_dirs(&path_var, &[]), path_var);
    }

    #[test]
    fn only_the_interpreter_gets_the_openssl_filter() {
        // Built with `join`, the way the real caller does: a hardcoded
        // "C:\\...\\python.exe" literal is one string with no separators to a
        // non-Windows `Path`, so it would assert nothing off Windows.
        let venv: PathBuf = ["Headroom", "runtime", "venv", "Scripts"].iter().collect();
        assert!(is_python_interpreter(&venv.join("python.exe")));
        assert!(is_python_interpreter(&venv.join("pythonw.exe")));
        assert!(is_python_interpreter(Path::new("/usr/bin/python3")));
        assert!(is_python_interpreter(Path::new("python")));
        // A node CLI may legitimately need something out of a dropped dir.
        assert!(!is_python_interpreter(&venv.join("codex")));
        assert!(!is_python_interpreter(Path::new("/opt/nvm/bin/node")));
    }

    #[test]
    fn path_with_binary_dir_prepends_parent() {
        let path =
            path_with_binary_dir(&PathBuf::from("/Users/x/.nvm/versions/node/v22/bin/codex"));
        let first = std::env::split_paths(&path).next().expect("non-empty PATH");
        assert_eq!(first, PathBuf::from("/Users/x/.nvm/versions/node/v22/bin"));
        // A bare binary name has no usable parent; PATH is left unchanged.
        let existing = std::env::var_os("PATH").unwrap_or_default();
        assert_eq!(path_with_binary_dir(&PathBuf::from("codex")), existing);
    }

    #[test]
    fn classify_kompress_prefetch_failure_buckets_known_causes() {
        assert_eq!(classify_kompress_prefetch_failure(""), "no output");
        assert_eq!(
            classify_kompress_prefetch_failure("python3 abort trap: 6 (SIGABRT)"),
            "native abort"
        );
        assert_eq!(
            classify_kompress_prefetch_failure("OSError: [Errno 28] No space left on device"),
            "disk full"
        );
        assert_eq!(
            classify_kompress_prefetch_failure(
                "requests.exceptions.ConnectionError: Max retries exceeded with url"
            ),
            "network"
        );
        assert_eq!(
            classify_kompress_prefetch_failure(
                "RuntimeError: Cannot send a request, as the client has been closed."
            ),
            "network"
        );
        assert_eq!(
            classify_kompress_prefetch_failure("PermissionError: [Errno 13] Permission denied"),
            "permission denied"
        );
        assert_eq!(
            classify_kompress_prefetch_failure(
                "OSError: [WinError 126] The specified module could not be found. \
                 Error loading \"...\\torch\\lib\\c10.dll\" or one of its dependencies"
            ),
            "missing native dep"
        );
        // RUST-75, verbatim from the event: only the errno survives the locale.
        assert_eq!(
            classify_kompress_prefetch_failure(
                "OSError: [WinError 1114] DLL 초기화 루틴을 실행할 수 없습니다. \
                 Error loading \"...\\torch\\lib\\c10.dll\" or one of its dependencies."
            ),
            "missing native dep"
        );
        assert_eq!(
            classify_kompress_prefetch_failure("ValueError: something unexpected"),
            "other"
        );
    }

    #[test]
    fn summarize_kompress_prefetch_failure_uses_last_meaningful_line() {
        let dir = std::env::temp_dir().join(format!(
            "kompress-prefetch-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("kompress-prefetch.log");
        fs::write(
            &log,
            "Downloading model...\nTraceback (most recent call last):\n  File x\nConnectionError: Max retries exceeded\n\n",
        )
        .unwrap();

        let cause = summarize_kompress_prefetch_failure(&log);
        assert_eq!(cause, "[network] ConnectionError: Max retries exceeded");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn summarize_kompress_prefetch_failure_handles_missing_log() {
        let cause = summarize_kompress_prefetch_failure(&PathBuf::from("/no/such/prefetch.log"));
        assert_eq!(cause, "[no output] (no output in kompress-prefetch.log)");
    }

    /// RUST-75 arrived as a bare torch DLL error. The ONNX failure that
    /// preceded it -- same missing MSVC redistributable, and the actual first
    /// cause -- was dropped, so the report pointed at the wrong library.
    #[test]
    fn crash_log_excerpt_leads_with_the_dump_header_the_tail_drops() {
        // RUST-C7 shape: header, then a dump long enough that an 80-line
        // tail starts mid-traceback.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("headroom-proxy.log");
        let mut body = String::from("banner line\nFatal Python error: Aborted\n\n");
        body.push_str("Current thread 0x00001234 (most recent call first):\n");
        for i in 0..150 {
            body.push_str(&format!(
                "  File \"sklearn/__init__.py\", line {i} in <module>\n"
            ));
        }
        std::fs::write(&log, body).unwrap();
        let excerpt = super::crash_log_excerpt(&log);
        assert!(
            excerpt.starts_with(
                "--- fatal markers ---\nFatal Python error: Aborted\nCurrent thread 0x00001234"
            ),
            "{excerpt}"
        );
        assert!(excerpt.contains("--- tail ---\n"), "{excerpt}");
        assert!(
            !excerpt.contains("banner line"),
            "tail must stay the last 80 lines: {excerpt}"
        );
        // No dump: plain tail, no marker section.
        std::fs::write(&log, "just\na\nlog\n").unwrap();
        assert_eq!(super::crash_log_excerpt(&log), "just\na\nlog");
    }

    #[test]
    fn startup_failure_summary_keeps_reason_and_probe_inside_the_sentry_cap() {
        // RUST-BX / RUST-BV: a Windows venv path plus the spawn args ran the
        // message past the 400-char log-bridge cap before the reason or the
        // onnx verdict appeared.
        let exe = r"C:\Users\jao\AppData\Local\Headroom\headroom\runtime\venv\Scripts\headroom.exe";
        let args: Vec<String> = [
            "proxy",
            "--port",
            "6768",
            "--no-http2",
            "--log-messages",
            "--learn",
            "--no-memory-tools",
            "--no-memory-context",
            "--memory-db-path",
            r"C:\Users\jao\AppData\Local\Headroom\memory.db",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let failure = |reason: &str| super::HeadroomStartupFailure {
            program: exe.to_string(),
            args: args.clone(),
            log_path: r"C:\Users\jao\AppData\Local\Headroom\logs\proxy.log".to_string(),
            log_tail: String::new(),
            reason: reason.to_string(),
        };
        let prior = vec![failure(
            "exited with status exit code: 0xffffffff before opening port 6768",
        )];
        let last = failure("never opened port 6768 within 300000ms");
        let err = anyhow::Error::from(last).context(format!(
            "unable to keep headroom running in background{}{}",
            " (onnx probe: import onnxruntime failed (exit 0xffffffff): <no stderr>)",
            super::prior_attempts_summary(&prior)
        ));
        let message = format!("watchdog: hung-kill restart failed: {err:#}");
        let head: String = message.chars().take(400).collect();
        assert!(
            head.contains("onnx probe: import onnxruntime failed"),
            "{head}"
        );
        assert!(
            head.contains("headroom.exe: exited with status exit code: 0xffffffff"),
            "{head}"
        );
        assert!(
            head.contains("never opened port 6768 within 300000ms"),
            "{head}"
        );
        assert!(
            !head.contains("--memory-db-path"),
            "args must not lead: {head}"
        );
    }

    #[test]
    fn summarize_kompress_prefetch_failure_carries_the_earlier_onnx_cause() {
        let dir = std::env::temp_dir().join(format!(
            "kompress-onnx-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("kompress-prefetch.log");
        fs::write(
            &log,
            "Downloading Kompress ONNX model ...\n             WARNING ONNX load failed for kompress-v2-base, trying PyTorch: DLL load failed while importing onnxruntime_pybind11_state\n             Traceback (most recent call last):\n             OSError: [WinError 126] The specified module could not be found. Error loading \"C:\\venv\\torch\\lib\\c10.dll\"\n",
        )
        .unwrap();

        let cause = summarize_kompress_prefetch_failure(&log);
        // Category still comes from the last line, so the fingerprint is stable.
        assert!(
            cause.starts_with("[missing native dep] OSError: [WinError 126]"),
            "{cause}"
        );
        // ...but the ONNX cause that explains it now rides along.
        assert!(cause.contains("(after WARNING ONNX load failed"), "{cause}");
        assert!(cause.contains("onnxruntime_pybind11_state"), "{cause}");

        fs::remove_dir_all(&dir).ok();
    }

    /// An updater relaunch leaves :6768 held by nobody for a moment. Treating
    /// that as a foreign occupant moved the backend to 6770 on every update
    /// (RUST-7F), so the unowned shape is waited out before falling back.
    #[test]
    fn settle_unowned_port_waits_for_a_closing_socket_then_takes_the_port() {
        let calls = std::cell::Cell::new(0);
        let state = settle_unowned_port(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    PortState::ForeignOccupant(UNKNOWN_OCCUPANT.into())
                } else {
                    PortState::Free
                }
            },
            6,
            Duration::from_millis(0),
        );
        assert!(matches!(state, PortState::Free));
        assert_eq!(calls.get(), 3);
    }

    /// The boot-validation failure path reports the occupant, not just a
    /// bound/unbound bit. A free port and a held one must not read alike.
    #[test]
    fn named_foreign_occupant_only_fires_on_a_named_foreigner() {
        use super::{named_foreign_occupant, PortState, UNKNOWN_OCCUPANT};
        assert_eq!(
            named_foreign_occupant(PortState::ForeignOccupant("nginx pid 42".into())),
            Some("nginx pid 42".into())
        );
        // The unowned shape is the updater-relaunch race (RUST-7F): a
        // fail-fast caller must keep waiting on it.
        assert!(
            named_foreign_occupant(PortState::ForeignOccupant(UNKNOWN_OCCUPANT.into())).is_none()
        );
        assert!(named_foreign_occupant(PortState::Free).is_none());
        assert!(named_foreign_occupant(PortState::HeadroomRunning).is_none());
    }

    #[test]
    fn pip_index_fetch_failed_requires_both_signals() {
        use super::pip_index_fetch_failed;
        // RUST-90: no index was readable, so the pin verdict is meaningless.
        assert!(pip_index_fetch_failed(
            "could not fetch url https://pypi.org/simple/x/: ssl eof\n\
             error: could not find a version that satisfies x==1 (from versions: none)"
        ));
        // A real bad pin lists the versions pip DID see (RUST-6S).
        assert!(!pip_index_fetch_failed(
            "could not fetch url https://github.com/x: rate limited\n\
             no matching distribution found for onnxruntime==1.27.0 (from versions: 1.23.0)"
        ));
        assert!(!pip_index_fetch_failed(
            "no matching distribution found for onnxruntime==1.27.0 (from versions: none)"
        ));
        // And a starved index must knock the shape out of no-matching-dist.
        assert_eq!(
            super::pip_failure_category(
                "exit=1; stderr tail: Could not fetch URL https://pypi.org/simple/x/: ssl eof\n\
                 ERROR: No matching distribution found for x==1 (from versions: none)"
            ),
            "network"
        );
    }

    #[test]
    fn describe_proxy_port_occupant_separates_free_from_held() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
        let port = listener.local_addr().expect("addr").port();
        assert_ne!(describe_proxy_port_occupant(port), "free");
        drop(listener);
        assert_eq!(describe_proxy_port_occupant(port), "free");
    }

    #[test]
    fn settle_unowned_port_does_not_wait_on_a_named_occupant() {
        let calls = std::cell::Cell::new(0);
        let state = settle_unowned_port(
            || {
                calls.set(calls.get() + 1);
                PortState::ForeignOccupant("rapportd pid 594".into())
            },
            6,
            Duration::from_millis(0),
        );
        // A real conflict must fall back immediately, not sit through the wait.
        assert!(matches!(state, PortState::ForeignOccupant(ref d) if d == "rapportd pid 594"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn settle_unowned_port_gives_up_after_the_attempt_budget() {
        let calls = std::cell::Cell::new(0);
        let state = settle_unowned_port(
            || {
                calls.set(calls.get() + 1);
                PortState::ForeignOccupant(UNKNOWN_OCCUPANT.into())
            },
            6,
            Duration::from_millis(0),
        );
        assert!(matches!(state, PortState::ForeignOccupant(ref d) if d == UNKNOWN_OCCUPANT));
        assert_eq!(calls.get(), 6, "budget must be spent, not exceeded");
    }

    #[test]
    fn redact_sensitive_strips_anthropic_keys() {
        let line = "POST /v1/messages x-api-key: sk-ant-api03-AbCdEf-12_34 done";
        let out = redact_sensitive(line);
        assert!(!out.contains("sk-ant-"), "leak: {out}");
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("done"));
    }

    #[test]
    fn redact_sensitive_strips_bearer_tokens() {
        let line = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig trailing";
        let out = redact_sensitive(line);
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "leak: {out}");
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("trailing"));
    }

    #[test]
    fn redact_sensitive_passes_through_clean_lines() {
        let line = "2026-05-03T20:31:34Z proxy started on 127.0.0.1:6767";
        assert_eq!(redact_sensitive(line), line);
    }

    #[test]
    fn redact_sensitive_ignores_short_bearer_word() {
        // "Bearer" followed by something too short to be a real token shouldn't
        // be redacted — we don't want to nuke unrelated prose.
        let line = "the Bearer of the message is fine";
        assert_eq!(redact_sensitive(line), line);
    }

    fn cmd_failure_with_stderr(stderr: &str) -> anyhow::Error {
        anyhow::Error::new(CommandFailure {
            program: "/runtime/venv/bin/headroom".into(),
            args: vec!["mcp".into(), "install".into()],
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code: Some(1),
            signal: None,
        })
    }

    #[test]
    fn looks_like_corrupt_venv_error_matches_module_not_found() {
        let err = cmd_failure_with_stderr(
            "Traceback (most recent call last):\n\
             ...\n\
             ModuleNotFoundError: No module named 'opentelemetry'\n",
        );
        assert!(looks_like_corrupt_venv_error(&err));
    }

    #[test]
    fn looks_like_corrupt_venv_error_matches_import_error() {
        let err = cmd_failure_with_stderr(
            "Traceback (most recent call last):\n\
             ImportError: cannot import name 'X' from partially initialized module 'Y'\n",
        );
        assert!(looks_like_corrupt_venv_error(&err));
    }

    #[test]
    fn looks_like_corrupt_venv_error_ignores_unrelated_failures() {
        let err = cmd_failure_with_stderr("error: invalid --proxy-url\n");
        assert!(!looks_like_corrupt_venv_error(&err));
    }

    #[test]
    fn looks_like_corrupt_venv_error_ignores_non_command_errors() {
        let err = anyhow::anyhow!("some other failure with ModuleNotFoundError in the message");
        // Only CommandFailure errors carry the structured stderr we trust as a
        // corrupt-venv signal — a bare anyhow message could be anything.
        assert!(!looks_like_corrupt_venv_error(&err));
    }

    #[test]
    fn looks_like_corrupt_venv_error_survives_anyhow_context() {
        use anyhow::Context as _;
        let err = cmd_failure_with_stderr("ModuleNotFoundError: No module named 'opentelemetry'\n");
        let wrapped = Err::<(), _>(err)
            .context("configuring Headroom MCP integration")
            .unwrap_err();
        assert!(looks_like_corrupt_venv_error(&wrapped));
    }

    #[test]
    fn requirements_lock_package_count_counts_only_requirement_lines() {
        let lock =
            "# header\n\nabsl-py==2.4.0\n# inline note\naiohttp==3.13.5\n  \ntorch==2.12.1\n";
        assert_eq!(requirements_lock_package_count(lock), 3);
        // Same rule as requirements_lock_sha, so the two never disagree about
        // what a requirement line is.
        assert_eq!(requirements_lock_package_count(""), 0);
        assert_eq!(requirements_lock_package_count("# only comments\n\n"), 0);
        // The shipped locks must produce a usable total; a zero here silently
        // reverts pip progress to the old counter heuristic.
        for lock in [
            HEADROOM_REQUIREMENTS_LOCK,
            HEADROOM_LINUX_REQUIREMENTS_LOCK,
            HEADROOM_WINDOWS_REQUIREMENTS_LOCK,
        ] {
            assert!(requirements_lock_package_count(lock) > 10);
        }
    }

    #[test]
    fn pip_progress_eta_tracks_a_slow_link_instead_of_claiming_five_seconds() {
        // RUST-9Y: 168 packages over a ~60 kB/s link. Ten minutes in, a tenth
        // of the way through, the old code said "5 seconds" and pinned the bar
        // just under the ceiling. Both numbers now follow the real rate.
        let mut counter = 0u32;
        let total = 168;
        let mut last = None;
        for _ in 0..17 {
            last = pip_line_to_progress(
                "Collecting torch==2.12.1",
                Duration::from_secs(600),
                &mut counter,
                55,
                80,
                total,
            );
        }
        let update = last.expect("Collecting is a progress line");
        // ~10% done in 600s projects ~90 minutes remaining, not 5 seconds.
        assert!(
            update.eta_seconds > 3000 && update.eta_seconds < 7200,
            "eta was {}",
            update.eta_seconds
        );
        // And the bar reflects the tenth actually done, not the ceiling.
        assert!(
            (56..=60).contains(&update.percent),
            "percent was {}",
            update.percent
        );
    }

    #[test]
    fn pip_progress_never_reports_a_near_zero_eta_before_it_can_measure() {
        // The old floor was `.max(5)`, which is what made an install that had
        // outrun its 90s budget look finished.
        let mut counter = 0u32;
        let update = pip_line_to_progress(
            "Collecting absl-py==2.4.0",
            Duration::from_secs(300),
            &mut counter,
            55,
            80,
            0,
        )
        .expect("Collecting is a progress line");
        assert!(update.eta_seconds >= 30, "eta was {}", update.eta_seconds);
    }

    #[test]
    fn pip_progress_counts_packages_not_lines() {
        // pip prints Collecting AND Downloading per package; counting both
        // advanced the bar at twice the true rate.
        let mut counter = 0u32;
        let elapsed = Duration::from_secs(60);
        pip_line_to_progress(
            "Collecting torch==2.12.1",
            elapsed,
            &mut counter,
            55,
            80,
            100,
        );
        pip_line_to_progress(
            "Downloading torch-2.12.1-cp312-cp312-win_amd64.whl (31 kB)",
            elapsed,
            &mut counter,
            55,
            80,
            100,
        );
        assert_eq!(counter, 1);
    }

    #[test]
    fn pip_progress_resolves_to_the_ceiling_once_downloads_are_done() {
        let mut counter = 0u32;
        let update = pip_line_to_progress(
            "Installing collected packages: absl-py, aiohttp",
            Duration::from_secs(4000),
            &mut counter,
            55,
            80,
            168,
        )
        .expect("Installing is a progress line");
        assert_eq!(update.percent, 79);
        // No more downloading, so the download rate must not be extrapolated
        // into a multi-hour estimate for an unpack. The seed is the fixed
        // per-platform unpack budget, never elapsed-derived.
        let expected = if cfg!(windows) { 240 } else { 60 };
        assert_eq!(update.eta_seconds, expected);
    }

    #[test]
    fn unpack_phase_widens_the_silence_window_and_only_then() {
        let base = Some(Duration::from_secs(600));
        assert_eq!(
            widen_silence_for_unpack(base, "Installing collected packages: absl-py, torch"),
            Some(PIP_UNPACK_SILENCE_TIMEOUT)
        );
        // Anything else leaves the window alone.
        assert_eq!(
            widen_silence_for_unpack(base, "Collecting torch==2.12.1"),
            base
        );
        // A caller that opted out of the watchdog stays opted out.
        assert_eq!(
            widen_silence_for_unpack(None, "Installing collected packages: absl-py"),
            None
        );
        // Never narrow a window that is already wider.
        let wide = Some(Duration::from_secs(3600));
        assert_eq!(
            widen_silence_for_unpack(wide, "Installing collected packages: absl-py"),
            wide
        );
    }

    #[test]
    fn pip_progress_keeps_moving_while_one_big_wheel_downloads() {
        // torch is 123 MB; at the ~60 kB/s RUST-9Y measured that is 34 minutes
        // in which pip prints no Collecting/Downloading line at all. The raw
        // byte counter is the only thing keeping the wizard -- and the silence
        // watchdog -- alive through it.
        let mut counter = 84u32;
        let first = pip_line_to_progress(
            "Progress 47185920 of 128974848",
            Duration::from_secs(1200),
            &mut counter,
            55,
            80,
            168,
        )
        .expect("a raw progress line is a progress line");
        assert_eq!(first.message, "Downloading 45.0 / 123.0 MB...");
        // The same wheel is still in flight, so the package count must not move.
        assert_eq!(counter, 84);

        let later = pip_line_to_progress(
            "Progress 52428800 of 128974848",
            Duration::from_secs(1800),
            &mut counter,
            55,
            80,
            168,
        )
        .expect("a raw progress line is a progress line");
        assert!(
            later.eta_seconds > first.eta_seconds,
            "eta must grow while the same wheel drags on: {} -> {}",
            first.eta_seconds,
            later.eta_seconds
        );
    }

    #[test]
    fn pip_progress_ignores_prose_that_merely_starts_with_progress() {
        let mut counter = 0u32;
        assert!(pip_line_to_progress(
            "Progress report written",
            Duration::from_secs(10),
            &mut counter,
            55,
            80,
            168,
        )
        .is_none());
        assert_eq!(counter, 0);
    }

    #[test]
    fn stream_capture_keeps_the_tail_within_the_cap() {
        // 4 lines/second for an hour would otherwise be held in memory and
        // dumped into the app log by the first retry's CommandFailure Display.
        let mut sink = String::new();
        for i in 0..20_000 {
            sink.push_str(&format!("Progress {i} of 128974848 \u{2713}\n"));
            super::cap_capture(&mut sink);
        }
        assert!(sink.len() <= super::STREAM_CAPTURE_CAP);
        assert!(sink.ends_with("Progress 19999 of 128974848 \u{2713}\n"));
        // Cut on a line boundary, so a multi-byte char is never split.
        assert!(sink.starts_with("Progress "));
    }

    #[test]
    fn pip_output_capture_drops_raw_progress_lines() {
        let mut capture = super::PipOutputCapture::new(3);
        capture.push("Collecting torch==2.12.1");
        for i in 0..50 {
            capture.push(&format!("Progress {i} of 128974848"));
        }
        capture.push("ERROR: No matching distribution found for torch==2.12.1");
        let out = capture.into_string();
        assert!(out.contains("Collecting torch"), "{out}");
        assert!(out.contains("ERROR: No matching"), "{out}");
        assert!(!out.contains("Progress "), "{out}");
    }

    #[test]
    fn wheel_download_failure_category_splits_causes_not_urls() {
        // RUST-22: one condition, a new Sentry issue per wheel version and
        // platform, because the URL was the message.
        let cases = [
            ("downloading https://files.pythonhosted.org/a.whl: operation timed out", "timeout"),
            (
                "downloading https://files.pythonhosted.org/a.whl: HTTP status client error (403 Forbidden) for url (https://files.pythonhosted.org/a.whl)",
                "http-403",
            ),
            (
                "downloading https://files.pythonhosted.org/a.whl: HTTP status client error (404 Not Found) for url (https://files.pythonhosted.org/a.whl)",
                "http-404",
            ),
            ("downloading https://x/a.whl: dns error: failed to lookup address information", "dns"),
            ("downloading https://x/a.whl: invalid peer certificate: UnknownIssuer", "tls"),
            ("downloading https://x/a.whl: error sending request", "connection"),
            ("writing /tmp/a.partial: Permission denied (os error 13)", "permission"),
            ("downloading https://x/a.whl: something new", "other"),
        ];
        for (detail, expected) in cases {
            assert_eq!(
                wheel_download_failure_category(detail),
                expected,
                "for: {detail}"
            );
        }
        // Two different pins of the same platform wheel must land on one
        // category -- that is the whole point.
        assert_eq!(
            wheel_download_failure_category(
                "downloading https://files.pythonhosted.org/47/21/headroom_ai-0.37.0-cp310-abi3-macosx_11_0_arm64.whl: operation timed out"
            ),
            wheel_download_failure_category(
                "downloading https://files.pythonhosted.org/99/aa/headroom_ai-0.38.0-cp310-abi3-win_amd64.whl: operation timed out"
            )
        );
    }

    #[test]
    fn requirements_lock_sha_ignores_comments_and_blank_lines() {
        let a = "# header one\n\nabsl-py==2.4.0\naiohttp==3.13.5\n";
        let b = "# header two — different\naiohttp==3.13.5\nabsl-py==2.4.0\n";
        let c = "absl-py==2.4.0\naiohttp==3.13.6\n";
        // Same pinned versions, different comments/whitespace → same hash.
        assert_eq!(requirements_lock_sha(a), requirements_lock_sha(a));
        // Order still matters (pip resolution order), so (a) and (b) differ.
        assert_ne!(requirements_lock_sha(a), requirements_lock_sha(b));
        // A real version bump changes the hash.
        assert_ne!(requirements_lock_sha(a), requirements_lock_sha(c));
        // Adding/removing a comment or blank line must not change the hash.
        let a_more_comments =
            "# header one\n# extra note\n\n\nabsl-py==2.4.0\n# inline\naiohttp==3.13.5\n";
        assert_eq!(
            requirements_lock_sha(a),
            requirements_lock_sha(a_more_comments)
        );
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn run_command_failure_carries_structured_output() {
        let tmp = std::env::temp_dir();
        let err = run_command(
            std::path::Path::new("/bin/sh"),
            &["-c", "echo hi-out; echo hi-err 1>&2; exit 7"],
            &tmp,
        )
        .expect_err("command should have failed");

        let failure = err
            .chain()
            .find_map(|e| e.downcast_ref::<CommandFailure>())
            .expect("CommandFailure should be in the error chain");

        assert_eq!(failure.exit_code, Some(7));
        assert!(
            failure.stdout.contains("hi-out"),
            "stdout: {}",
            failure.stdout
        );
        assert!(
            failure.stderr.contains("hi-err"),
            "stderr: {}",
            failure.stderr
        );
        assert_eq!(failure.program, "/bin/sh");
    }

    #[test]
    fn managed_python_paths_live_inside_headroom_root() {
        let root = std::env::temp_dir().join("headroom-tool-manager-test");
        let runtime = ManagedRuntime::bootstrap_root(&root);

        assert!(runtime.managed_python().starts_with(&runtime.root_dir));
        assert!(runtime.standalone_python().starts_with(&runtime.root_dir));
        assert!(runtime.managed_pip().starts_with(&runtime.root_dir));
        assert!(runtime.bin_dir.starts_with(&runtime.root_dir));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn managed_runtime_uses_windows_layout() {
        let runtime =
            ManagedRuntime::bootstrap_root(&std::env::temp_dir().join("headroom-layout-test"));
        assert!(runtime.standalone_python().ends_with("python\\python.exe"));
        assert!(runtime.managed_python().ends_with("Scripts\\python.exe"));
        assert!(runtime.managed_pip().ends_with("Scripts\\pip.exe"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn tool_manager_entrypoints_use_windows_layout() {
        let runtime =
            ManagedRuntime::bootstrap_root(&std::env::temp_dir().join("headroom-entrypoints-test"));
        let manager = ToolManager::new(runtime);
        assert!(manager
            .headroom_entrypoint()
            .ends_with("Scripts\\headroom.exe"));
        assert!(manager.rtk_entrypoint().ends_with("bin\\rtk.exe"));
        assert!(manager
            .markitdown_shim_path()
            .ends_with("bin\\markitdown.cmd"));
    }

    /// The wheel carries a native `_core` extension, so the platform tag in
    /// its filename must match the platform we're installing on. Runs on every
    /// target: a macOS wheel shipped to Windows is what broke RUST-6E.
    #[test]
    fn pinned_headroom_wheel_matches_running_platform() {
        let release = pinned_headroom_release().expect("pinned wheel for this platform");
        let expected_tag = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "macosx_11_0_arm64",
            ("macos", "x86_64") => "macosx_10_12_x86_64",
            ("linux", "aarch64") => "manylinux_2_28_aarch64",
            ("linux", "x86_64") => "manylinux_2_28_x86_64",
            ("windows", "x86_64") => "win_amd64",
            (os, arch) => panic!("test needs a tag for {os}/{arch}"),
        };
        assert!(
            release.wheel_url.ends_with(&format!("{expected_tag}.whl")),
            "wheel {} is not built for {}/{}",
            release.wheel_url,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        assert!(
            release
                .wheel_url
                .contains(&format!("headroom_ai-{HEADROOM_PINNED_VERSION}-")),
            "wheel url {} does not carry the pinned version",
            release.wheel_url
        );
        assert_eq!(release.sha256.len(), 64, "sha256 must be pinned");
    }

    #[test]
    fn abandoned_bootstrap_marker_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ToolManager::new(ManagedRuntime::bootstrap_root(dir.path()));
        assert!(manager.take_abandoned_bootstrap().is_none());

        manager.note_bootstrap_attempt("Downloading Python", 18);
        let taken = manager.take_abandoned_bootstrap().expect("marker reported");
        assert_eq!(taken.step, "Downloading Python");
        assert_eq!(taken.percent, 18);
        // Consumed on read: a second launch must not double-report.
        assert!(manager.take_abandoned_bootstrap().is_none());

        // An attempt with a verdict (success or classified failure) clears
        // the marker, so the next launch reports nothing.
        manager.note_bootstrap_attempt("Updating dependencies", 60);
        manager.clear_bootstrap_attempt();
        assert!(manager.take_abandoned_bootstrap().is_none());
    }

    /// One machine relaunching into the same wall must not re-file the same
    /// Sentry event every launch (RUST-AN: 21 identical app_control_blocked
    /// events from one laptop in a day). First capture and a changed cause
    /// always report; a same-key repeat inside 24h does not; a stale marker
    /// (over 24h) reports again.
    #[test]
    fn bootstrap_failure_capture_dedupes_same_key_within_a_day() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ToolManager::new(ManagedRuntime::bootstrap_root(dir.path()));

        assert!(manager.should_capture_bootstrap_failure("app_control_blocked"));
        assert!(!manager.should_capture_bootstrap_failure("app_control_blocked"));
        // A different cause is a different story.
        assert!(manager.should_capture_bootstrap_failure("permission"));
        // ... and it replaced the marker, so the original kind reports again.
        assert!(manager.should_capture_bootstrap_failure("app_control_blocked"));

        // Backdate the marker past the window: the daily heartbeat reports.
        let path = manager.bootstrap_failure_capture_marker_path();
        std::fs::write(&path, r#"{"key":"app_control_blocked","unix_ts":1}"#)
            .expect("backdate marker");
        assert!(manager.should_capture_bootstrap_failure("app_control_blocked"));

        // A corrupt marker reports rather than suppresses.
        std::fs::write(&path, b"not json").expect("corrupt marker");
        assert!(manager.should_capture_bootstrap_failure("app_control_blocked"));
    }

    /// Downloads keep PyPI's filename so pip's own platform check stays a
    /// backstop; a `py3-none-any` rename is what let a macOS wheel install on
    /// Windows.
    #[test]
    fn wheel_download_path_keeps_platform_tagged_filename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ToolManager::new(ManagedRuntime::bootstrap_root(dir.path()));
        let release = pinned_headroom_release().expect("pinned wheel for this platform");
        let path = manager.wheel_download_path(&release.wheel_url);
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            release.wheel_url.rsplit('/').next()
        );
        assert!(!path.to_string_lossy().contains("py3-none-any"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn python_distribution_artifact_supports_windows_x86_64() {
        let artifact = python_distribution_artifact().expect("windows python target");
        assert!(artifact.url.contains("x86_64-pc-windows-msvc"));
        assert!(artifact.url.ends_with(".tar.gz"));
        assert!(
            artifact.sha256.is_some(),
            "python checksum should be pinned"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rtk_distribution_artifact_supports_windows_x86_64() {
        let artifact = rtk_distribution_artifact().expect("windows rtk target");
        assert!(artifact.url.contains("x86_64-pc-windows-msvc"));
        assert!(artifact.url.ends_with(".zip"));
        assert!(artifact.sha256.is_some(), "rtk checksum should be pinned");
    }

    #[test]
    fn bootstrap_requirements_lock_targets_windows() {
        assert_eq!(
            bootstrap_requirements_lock_for_target("windows"),
            HEADROOM_WINDOWS_REQUIREMENTS_LOCK
        );
        assert_eq!(
            bootstrap_requirements_lock_for_target("linux"),
            HEADROOM_LINUX_REQUIREMENTS_LOCK
        );
        assert_eq!(
            bootstrap_requirements_lock_for_target("macos"),
            HEADROOM_REQUIREMENTS_LOCK
        );
        // hnswlib is sdist-only on PyPI with no vendored win_amd64 wheel; a
        // pin here bricks bootstrap on any Windows box without MSVC (RUST-65).
        assert!(!HEADROOM_WINDOWS_REQUIREMENTS_LOCK.contains("hnswlib=="));
        assert!(HEADROOM_WINDOWS_REQUIREMENTS_LOCK.contains("sqlite-vec=="));
    }

    #[test]
    fn serena_venv_lives_outside_runtime_dir_so_upgrades_keep_it() {
        let root = std::env::temp_dir().join("headroom-tool-manager-test");
        let runtime = ManagedRuntime::bootstrap_root(&root);
        let runtime_dir = runtime.runtime_dir.clone();
        let manager = ToolManager::new(runtime);

        assert!(manager
            .serena_venv_dir()
            .starts_with(&manager.runtime.root_dir));
        assert!(!manager.serena_venv_dir().starts_with(&runtime_dir));
        assert!(manager
            .serena_entrypoint()
            .starts_with(manager.serena_venv_dir()));
    }

    #[test]
    fn kompress_marker_scan_treats_lazy_load_as_enabled() {
        let (root, runtime, manager) = seed_test_runtime("kompress-marker");
        fs::create_dir_all(runtime.logs_dir()).expect("logs dir");
        let enabled: &[&str] = &[
            "kompress: enabled",
            "kompress onnx loaded",
            "kompress pytorch loaded",
        ];
        let disabled: &[&str] = &["kompress: not installed", "kompress: disabled"];

        // Cold-cache startup logs "not installed"; a later first-use lazy load
        // logs "Kompress ONNX loaded". The most-recent marker (lazy load) wins,
        // so the desktop reports enabled without a restart.
        let log = runtime.logs_dir().join("kompress-lazy.log");
        fs::write(
            &log,
            "2026-06-12 10:00:00 - headroom.proxy - INFO - Kompress: not installed (pip install headroom-ai[ml])\n\
             2026-06-12 10:05:00 - headroom.proxy - INFO - Kompress ONNX loaded: chopratejas/kompress-v2-base backend=onnx\n",
        )
        .expect("write lazy log");
        assert_eq!(
            manager.scan_file_for_marker_state_cached("k-lazy", &log, enabled, disabled),
            Some(true),
            "a lazy-load line after a not-installed line should report enabled"
        );

        // A pure cold-cache log (no lazy load yet) still reports disabled.
        let log2 = runtime.logs_dir().join("kompress-cold.log");
        fs::write(
            &log2,
            "2026-06-12 10:00:00 - headroom.proxy - INFO - Kompress: not installed (pip install headroom-ai[ml])\n",
        )
        .expect("write cold log");
        assert_eq!(
            manager.scan_file_for_marker_state_cached("k-cold", &log2, enabled, disabled),
            Some(false),
            "a not-installed line with no later load should report disabled"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_distribution_artifact_is_pinned_to_current_release_with_checksum() {
        let artifact = rtk_distribution_artifact().expect("supported RTK target");

        assert!(artifact.url.contains(&format!("/v{RTK_VERSION}/")));
        assert!(
            artifact.sha256.is_some(),
            "RTK artifact checksum should be pinned"
        );
    }

    #[test]
    fn tool_manifest_exposes_platform_rtk_checksum() {
        let root = std::env::temp_dir().join("headroom-tool-manager-manifest-test");
        let runtime = ManagedRuntime::bootstrap_root(&root);
        let manager = ToolManager::new(runtime);

        let rtk = manager
            .list_tools()
            .into_iter()
            .find(|tool| tool.id == "rtk")
            .expect("rtk manifest should exist");
        assert_eq!(rtk.version, RTK_VERSION);
        assert!(rtk.checksum.is_some(), "RTK checksum should be exposed");
    }

    #[test]
    fn rtk_installed_requires_binary_and_receipt() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-installed");

        assert!(!manager.rtk_installed(), "no binary or receipt yet");

        write_executable(&manager.rtk_entrypoint(), "#!/usr/bin/env bash\nexit 0\n");
        assert!(
            !manager.rtk_installed(),
            "binary alone should not count as installed"
        );

        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");
        assert!(manager.rtk_installed(), "binary + receipt should count");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installed_rtk_version_reads_receipt() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-version");
        write_executable(&manager.rtk_entrypoint(), "#!/usr/bin/env bash\nexit 0\n");
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": "0.37.2-test" }))
            .expect("rtk receipt");

        assert_eq!(
            manager.installed_rtk_version().as_deref(),
            Some("0.37.2-test")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_needs_install_true_when_binary_missing() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-needs-install-missing");
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");
        assert!(manager.rtk_needs_install());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_needs_install_true_when_version_stale() {
        let (root, runtime, manager) = seed_test_runtime("rtk-needs-install-stale");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": "0.0.1-old" }))
            .expect("rtk receipt");
        assert!(manager.rtk_needs_install());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_needs_install_false_when_current() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-needs-install-current");
        write_executable(&manager.rtk_entrypoint(), "#!/usr/bin/env bash\nexit 0\n");
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");
        assert!(!manager.rtk_needs_install());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ensure_rtk_current_is_noop_when_already_current() {
        let (root, runtime, manager) = seed_test_runtime("rtk-ensure-current-noop");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");
        let did_work = manager.ensure_rtk_current().expect("ensure_rtk_current");
        assert!(!did_work, "should skip install when already current");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ensure_rtk_current_is_noop_when_binary_absent() {
        // RTK is opt-in: a missing binary means uninstalled/never-installed, so
        // launch must not create a fresh install.
        let (root, _runtime, manager) = seed_test_runtime("rtk-ensure-current-absent");
        let did_work = manager.ensure_rtk_current().expect("ensure_rtk_current");
        assert!(!did_work, "should not install rtk when binary is absent");
        assert!(!manager.rtk_installed(), "rtk must remain uninstalled");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_rtk_activity_reports_not_installed_when_missing() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-not-installed");

        let lines = manager
            .read_rtk_activity(10)
            .expect("not-installed fallback");
        assert_eq!(lines, vec!["RTK is not installed yet.".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // read_rtk_activity spawns `rtk session`; the fake is a shell script
    fn read_rtk_activity_returns_last_lines_from_session_output() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-activity");
        write_executable(
            &manager.rtk_entrypoint(),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"session\" ]; then\n  printf 'line-1\\nline-2\\nline-3\\nline-4\\n';\n  exit 0\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        let lines = manager.read_rtk_activity(2).expect("session output");
        assert_eq!(lines, vec!["line-3".to_string(), "line-4".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // read_rtk_activity spawns `rtk session`; the fake is a shell script
    fn read_rtk_activity_surfaces_session_failures() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-activity-fail");
        write_executable(
            &manager.rtk_entrypoint(),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"session\" ]; then\n  echo 'session stdout';\n  echo 'session stderr' 1>&2;\n  exit 7\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        let err = manager
            .read_rtk_activity(10)
            .expect_err("failing session should surface an error");
        let msg = err.to_string();
        assert!(msg.contains("session stdout"), "stdout preserved: {msg}");
        assert!(msg.contains("session stderr"), "stderr preserved: {msg}");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn rtk_gain_summary_parses_summary_json() {
        let (root, runtime, manager) = seed_test_runtime("rtk-gain-summary");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo '{\"summary\":{\"total_commands\":7,\"total_saved\":1234,\"avg_savings_pct\":61.5},\"daily\":[]}';\n  exit 0\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        let summary = manager.rtk_gain_summary().expect("gain summary");
        assert_eq!(summary.total_commands, 7);
        assert_eq!(summary.total_saved, 1234);
        assert_eq!(summary.avg_savings_pct, 61.5);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn rtk_today_stats_returns_matching_daily_row() {
        let (root, runtime, manager) = seed_test_runtime("rtk-today");
        let today = Local::now().date_naive().to_string();
        let script = format!(
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  cat <<'EOF'\n{{\"daily\":[{{\"date\":\"1999-01-01\",\"commands\":1,\"saved_tokens\":2}},{{\"date\":\"{today}\",\"commands\":7,\"saved_tokens\":1234}}]}}\nEOF\n  exit 0\nfi\nexit 9\n",
        );
        write_executable(&runtime.bin_dir.join("rtk"), &script);
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        let stats = manager.rtk_today_stats().expect("today stats");
        assert_eq!(stats.date, today);
        assert_eq!(stats.commands, 7);
        assert_eq!(stats.saved_tokens, 1234);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_today_stats_returns_none_when_today_absent() {
        let (root, runtime, manager) = seed_test_runtime("rtk-today-missing");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo '{\"daily\":[{\"date\":\"1999-01-01\",\"commands\":1,\"saved_tokens\":2}]}';\n  exit 0\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        assert!(manager.rtk_today_stats().is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_gain_summary_returns_none_when_summary_absent() {
        let (root, runtime, manager) = seed_test_runtime("rtk-gain-missing");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo '{\"daily\":[]}';\n  exit 0\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        assert!(manager.rtk_gain_summary().is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_gain_summary_returns_none_on_command_failure() {
        let (root, runtime, manager) = seed_test_runtime("rtk-gain-fail");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo 'boom' 1>&2;\n  exit 4\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        assert!(manager.rtk_gain_summary().is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_gain_summary_returns_none_on_invalid_json() {
        let (root, runtime, manager) = seed_test_runtime("rtk-gain-invalid-json");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo 'not-json';\n  exit 0\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        assert!(manager.rtk_gain_summary().is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bootstrap_all_installs_into_temp_root_when_enabled() {
        if std::env::var("HEADROOM_RUN_NETWORK_TESTS").is_err() {
            return;
        }

        let root = std::env::temp_dir().join(format!("headroom-e2e-{}", uuid::Uuid::new_v4()));
        let runtime = ManagedRuntime::bootstrap_root(&root);
        let manager = ToolManager::new(runtime.clone());

        manager
            .bootstrap_all_with_progress(|_| {})
            .expect("bootstrap succeeds");

        assert!(runtime.managed_python().exists());
        assert!(runtime.tools_dir.join("headroom.json").exists());
        assert!(runtime.bin_dir.join("rtk").exists());
    }

    #[test]
    fn listener_identity_formats_and_truncates_on_char_boundaries() {
        assert_eq!(
            super::format_listener_identity("nginx", 42, None),
            "nginx (pid 42)"
        );
        assert_eq!(
            super::format_listener_identity("python3.12", 7, Some("  headroom proxy --port 6767 ")),
            "python3.12 (pid 7): headroom proxy --port 6767"
        );
        // A multibyte char straddling the cap must not panic the truncate.
        let argv = format!("{}é", "x".repeat(159));
        let got = super::format_listener_identity("app", 1, Some(&argv));
        assert!(got.ends_with("..."), "{got}");
        assert!(got.len() < 200);
    }

    #[test]
    fn proxy_argv_matches_when_all_expected_flags_present() {
        let argv = "/Users/x/headroom proxy --port 6768 --log-messages \
                    --learn --no-memory-tools --no-memory-context --memory-db-path /tmp/m.db";
        assert!(proxy_argv_contains_expected_flags(argv, true));
    }

    #[test]
    fn proxy_argv_matches_legacy_nice_wrapped_orphan() {
        // Builds before 2026-08-17 spawned the backend under `nice`. Upgrading
        // users still have one of those running, and it must be recognized as
        // ours rather than treated as a foreign occupant of the port.
        let argv = "/usr/bin/nice -n 2 /Users/x/headroom proxy --port 6768 --log-messages \
                    --learn --no-memory-tools --no-memory-context --memory-db-path /tmp/m.db";
        assert!(proxy_argv_contains_expected_flags(argv, true));
    }

    #[test]
    fn proxy_argv_matches_without_learn_flags_when_auto_learn_off() {
        let argv = "/Users/x/headroom proxy --port 6768 --no-http2 --log-messages";
        assert!(proxy_argv_contains_expected_flags(argv, false));
    }

    #[test]
    fn proxy_argv_mismatch_when_learn_present_but_auto_learn_off() {
        // Leftover learn-enabled proxy from before the toggle flipped: restart.
        let argv = "/Users/x/headroom proxy --port 6768 --log-messages --learn \
                    --no-memory-tools --no-memory-context --memory-db-path /tmp/m.db";
        assert!(!proxy_argv_contains_expected_flags(argv, false));
    }

    #[test]
    fn proxy_argv_mismatch_when_log_messages_missing() {
        // The exact orphan-from-old-build case: a v0.2.x proxy still running
        // with just `proxy --port 6768`.
        let argv = "/Users/x/headroom proxy --port 6768";
        assert!(!proxy_argv_contains_expected_flags(argv, true));
    }

    #[test]
    fn proxy_argv_mismatch_when_learn_missing() {
        let argv = "headroom proxy --port 6768 --log-messages --no-memory-tools \
                    --no-memory-context --memory-db-path /tmp/m.db";
        assert!(!proxy_argv_contains_expected_flags(argv, true));
    }

    #[test]
    fn proxy_argv_match_does_not_get_fooled_by_negated_flag_substring() {
        // `--no-learn` contains `--learn` as a substring; whitespace tokenizing
        // ensures we don't false-positive on it.
        let argv = "headroom proxy --port 6768 --log-messages --no-learn \
                    --no-memory-tools --no-memory-context --memory-db-path /tmp/m.db";
        assert!(!proxy_argv_contains_expected_flags(argv, true));
    }

    #[test]
    fn proxy_argv_match_works_for_python_module_invocation() {
        let argv = "/Users/x/venv/bin/python3 -m headroom.proxy.server --port 6768 \
                    --no-http2 --log-messages --learn --no-memory-tools --no-memory-context \
                    --memory-db-path /tmp/m.db";
        assert!(proxy_argv_contains_expected_flags(argv, true));
    }

    #[test]
    fn sanitize_log_variant_replaces_path_separators() {
        let raw = "proxy---memory-db-path-/Users/x/Library/Application Support/Headroom/memory.db";
        let cleaned = sanitize_log_variant(raw);
        assert!(
            !cleaned.contains('/'),
            "expected no slashes, got: {cleaned}"
        );
        assert!(!cleaned.contains('\\'));
        assert!(cleaned.contains("memory-db-path"));
    }

    #[test]
    fn sanitize_log_variant_truncates_long_input() {
        let raw = "a".repeat(500);
        let cleaned = sanitize_log_variant(&raw);
        assert_eq!(cleaned.len(), 80);
    }

    #[test]
    fn sanitize_log_variant_keeps_short_safe_input_unchanged() {
        let raw = "proxy---port-6768---log-messages---learn";
        let cleaned = sanitize_log_variant(raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn prefetch_bootstrap_artifacts_is_a_no_op_when_runtime_is_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = ManagedRuntime::bootstrap_root(dir.path());
        for marker in [runtime.standalone_python(), runtime.managed_python()] {
            fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
            fs::write(&marker, b"").expect("marker");
        }
        let downloads_dir = runtime.downloads_dir.clone();

        // Both gates satisfied: must succeed without attempting any network
        // download (an attempt would be a gate bug, and flaky offline).
        ToolManager::new(runtime)
            .prefetch_bootstrap_artifacts()
            .expect("no-op prefetch succeeds");

        let leftovers: Vec<_> = fs::read_dir(&downloads_dir)
            .expect("downloads dir exists")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no-op prefetch must not write into downloads: {leftovers:?}"
        );
    }

    /// RUST-CR: a bootstrap that joins an in-flight prefetch must see that
    /// download's progress while it waits for the lock, not a frozen frame.
    #[test]
    fn artifact_lock_waiter_mirrors_the_holders_progress_for_its_url() {
        let held = ARTIFACT_DOWNLOAD_LOCK.lock().expect("lock");
        publish_inflight_download("https://example/python.tar.gz", 5, Some(10));
        let same = std::thread::spawn(|| {
            let mut seen = Vec::new();
            let _guard =
                acquire_artifact_download_lock("https://example/python.tar.gz", &mut |d, t| {
                    seen.push((d, t))
                });
            seen
        });
        let other = std::thread::spawn(|| {
            let mut seen = Vec::new();
            let _guard =
                acquire_artifact_download_lock("https://example/wheel.whl", &mut |d, t| {
                    seen.push((d, t))
                });
            seen
        });
        std::thread::sleep(Duration::from_millis(700));
        drop(held);
        assert!(same.join().expect("join").contains(&(5, Some(10))));
        assert!(
            other.join().expect("join").is_empty(),
            "a different URL must not inherit progress"
        );
    }

    /// RUST-2K: the timeout message must say WHICH stall it was, or the alarm
    /// is unactionable however many times it fires.
    #[test]
    fn stalled_prefetch_cause_separates_blocked_from_slow() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nothing landed: the vocab host never answered at all.
        assert_eq!(
            stalled_prefetch_cause(dir.path()),
            "vocab host never answered"
        );
        // One vocab through, the other still coming: a slow link, not a wall.
        std::fs::write(
            dir.path().join("9b5ad71b2ce5302211f9c61530b329a4922fc6a4"),
            b"x",
        )
        .expect("write cached vocab");
        assert_eq!(
            stalled_prefetch_cause(dir.path()),
            "vocab host reachable but slow"
        );
        // An unreadable/absent dir must not panic; treat it as nothing cached.
        assert_eq!(
            stalled_prefetch_cause(&dir.path().join("does-not-exist")),
            "vocab host never answered"
        );
    }

    /// RUST-7W: only the `.data/data/Scripts/*.dll` payload may extract -- the
    /// root-level duplicates, the .pyd, and dist-info must all be skipped.
    #[test]
    fn msvc_runtime_dll_name_selects_scripts_dlls_only() {
        assert_eq!(
            super::msvc_runtime_dll_name("msvc_runtime-14.44.35112.data/data/Scripts/msvcp140.dll"),
            Some("msvcp140.dll")
        );
        assert_eq!(
            super::msvc_runtime_dll_name(
                "msvc_runtime-14.44.35112.data/data/Scripts/VCRUNTIME140_1.DLL"
            ),
            Some("VCRUNTIME140_1.DLL")
        );
        // Root-level duplicate of the same DLL: not the Scripts set.
        assert_eq!(
            super::msvc_runtime_dll_name("msvc_runtime-14.44.35112.data/data/msvcp140.dll"),
            None
        );
        assert_eq!(
            super::msvc_runtime_dll_name("msvc_runtime.cp312-win_amd64.pyd"),
            None
        );
        assert_eq!(
            super::msvc_runtime_dll_name("msvc_runtime-14.44.35112.dist-info/RECORD"),
            None
        );
    }

    /// Every DLL must land in every target: the venv Scripts dir and the
    /// standalone python root are both candidate application directories.
    #[test]
    fn extract_msvc_runtime_dlls_lands_in_every_target() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let wheel_path = dir.path().join("msvc.whl");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&wheel_path).expect("create"));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in [
            (
                "msvc_runtime-14.44.35112.data/data/Scripts/msvcp140.dll",
                b"dll-bytes".as_slice(),
            ),
            (
                "msvc_runtime-14.44.35112.data/data/msvcp140.dll",
                b"root-duplicate".as_slice(),
            ),
            ("msvc_runtime.cp312-win_amd64.pyd", b"pyd".as_slice()),
        ] {
            writer.start_file(name, stored).expect("start_file");
            writer.write_all(bytes).expect("write entry");
        }
        writer.finish().expect("finish zip");

        let scripts = dir.path().join("venv-scripts");
        let python_root = dir.path().join("python");
        let extracted = super::extract_msvc_runtime_dlls(
            &wheel_path,
            &[scripts.as_path(), python_root.as_path()],
        )
        .expect("extract");
        assert_eq!(extracted, 1, "only the Scripts DLL extracts");
        for target in [&scripts, &python_root] {
            assert_eq!(
                std::fs::read(target.join("msvcp140.dll")).expect("dll present"),
                b"dll-bytes"
            );
            assert!(!target.join("msvc_runtime.cp312-win_amd64.pyd").exists());
        }
    }

    #[test]
    fn prefetch_tiktoken_encodings_gates_before_spawning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = ManagedRuntime::bootstrap_root(dir.path());
        let manager = ToolManager::new(runtime);

        // Empty cache but no managed python: must bail, not hang or succeed.
        let err = manager
            .prefetch_tiktoken_encodings()
            .expect_err("missing python must error");
        assert!(err.to_string().contains("managed python not found"));

        // Seeded cache: no-op success without spawning python (which does not
        // exist here) or touching the network.
        let cache = manager.tiktoken_cache_dir();
        fs::create_dir_all(&cache).expect("mkdir cache");
        fs::write(cache.join("seed"), b"x").expect("seed cache");
        manager
            .prefetch_tiktoken_encodings()
            .expect("seeded cache is a no-op");
    }

    #[test]
    fn rotate_log_if_large_rotates_only_past_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("headroom-default.log");

        fs::write(&log, b"small").expect("write small log");
        rotate_log_if_large(&log);
        assert!(log.exists(), "small log must not rotate");
        assert!(!log.with_extension("log.old").exists());

        fs::write(&log, vec![b'x'; 5 * 1024 * 1024 + 1]).expect("write big log");
        rotate_log_if_large(&log);
        assert!(!log.exists(), "oversized log must be renamed away");
        assert_eq!(
            fs::metadata(log.with_extension("log.old"))
                .expect("old gen")
                .len(),
            5 * 1024 * 1024 + 1
        );
    }

    #[test]
    fn log_tail_returns_last_bytes_and_handles_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("t.log");

        assert_eq!(log_tail(&log, 1024), "", "missing file -> empty");

        fs::write(&log, b"  hello  ").expect("write");
        assert_eq!(log_tail(&log, 1024), "hello", "trims whitespace");

        fs::write(&log, b"0123456789").expect("write");
        assert_eq!(log_tail(&log, 4), "6789", "seeks to last N bytes");
    }

    #[test]
    fn parse_pid_from_lsof_detail_extracts_numeric_pid() {
        assert_eq!(parse_pid_from_lsof_detail("rapportd pid 594"), Some(594));
        assert_eq!(
            parse_pid_from_lsof_detail("python3.12 pid 1073"),
            Some(1073)
        );
        assert_eq!(
            parse_pid_from_lsof_detail("Google Chrome Helper pid 4242"),
            Some(4242)
        );
    }

    /// The real path observed on a Windows box was
    /// `...\Headroom\headroom\runtime\python\python.exe` -- the BASE
    /// interpreter. A first cut of this gate required "venv" in the path, which
    /// that never matches, so the identity check could never pass and every
    /// reclaim stayed dead.
    #[test]
    fn exe_identity_matches_twin_paths_and_argv_only() {
        // RUST-7M reclaim gate: a stranded old instance is the same binary.
        let me = "/Applications/Headroom.app/Contents/MacOS/headroom-desktop";
        // Bare path (Windows Get-Process .Path), case-insensitive.
        assert!(super::exe_identity_matches(me, me));
        assert!(super::exe_identity_matches(&me.to_uppercase(), me));
        // Argv line (unix ps): path plus arguments.
        assert!(super::exe_identity_matches(
            "/Applications/Headroom.app/Contents/MacOS/headroom-desktop --flag",
            me
        ));
        // Prefix collisions and strangers must NOT pass.
        assert!(!super::exe_identity_matches(
            "/Applications/Headroom.app/Contents/MacOS/headroom-desktop-old",
            me
        ));
        assert!(!super::exe_identity_matches(
            "/usr/bin/python3 serve.py",
            me
        ));
        assert!(!super::exe_identity_matches("", me));
        assert!(!super::exe_identity_matches(me, ""));
    }

    #[test]
    fn exe_path_is_under_accepts_both_windows_python_layouts() {
        let runtime = PathBuf::from(r"C:\Users\garm\AppData\Local\Headroom\headroom\runtime");
        assert!(exe_path_is_under(
            r"C:\Users\garm\AppData\Local\Headroom\headroom\runtime\python\python.exe",
            &runtime
        ));
        assert!(exe_path_is_under(
            r"C:\Users\garm\AppData\Local\Headroom\headroom\runtime\venv\Scripts\headroom.exe",
            &runtime
        ));
        // PowerShell output arrives with a trailing newline and need not match
        // our casing.
        assert!(exe_path_is_under(
            "c:\\users\\garm\\appdata\\local\\headroom\\headroom\\runtime\\python\\python.exe\r\n",
            &runtime
        ));
    }

    /// A prefix match that does not land on a separator would point a kill at a
    /// stranger.
    #[test]
    fn exe_path_is_under_rejects_neighbours_and_strangers() {
        let runtime = PathBuf::from(r"C:\Users\garm\AppData\Local\Headroom\headroom\runtime");
        assert!(!exe_path_is_under(
            r"C:\Users\garm\AppData\Local\Headroom\headroom\runtime-old\python\python.exe",
            &runtime
        ));
        assert!(!exe_path_is_under(r"C:\Python312\python.exe", &runtime));
        assert!(!exe_path_is_under(
            r"C:\Users\garm\AppData\Local\Headroom\headroom-desktop.exe",
            &runtime
        ));
        // Pid already gone: PowerShell prints nothing. Never provably ours.
        assert!(!exe_path_is_under("", &runtime));
        assert!(!exe_path_is_under("   \r\n", &runtime));
    }

    /// Windows could not name a port holder at all before this (no lsof, no
    /// ss), so every occupant read as "unknown process" and no reclaim could
    /// pass its identity gate.
    #[test]
    fn parse_netstat_listener_finds_the_listening_pid() {
        let out = "\r\nActive Connections\r\n\r\n  Proto  Local Address          Foreign Address        State           PID\r\n  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1044\r\n  TCP    127.0.0.1:6768         0.0.0.0:0              LISTENING       9876\r\n  TCP    [::1]:6767             [::]:0                 LISTENING       4321\r\n";
        assert_eq!(parse_netstat_listener(out, 6768), Some(9876));
        // IPv6 rows are `[::1]:6767`; only the LAST colon separates the port.
        assert_eq!(parse_netstat_listener(out, 6767), Some(4321));
        assert_eq!(parse_netstat_listener(out, 7000), None);
    }

    /// Suffix matching would point a kill at whatever holds :16768.
    #[test]
    fn parse_netstat_listener_does_not_match_a_port_by_suffix() {
        let out = "  TCP    127.0.0.1:16768        0.0.0.0:0              LISTENING       5555\r\n";
        assert_eq!(parse_netstat_listener(out, 6768), None);
    }

    /// An ESTABLISHED row for the same port is a client, not the holder.
    #[test]
    fn parse_netstat_listener_ignores_non_listening_rows() {
        let out = "  TCP    127.0.0.1:6768         127.0.0.1:52100        ESTABLISHED     7777\r\n";
        assert_eq!(parse_netstat_listener(out, 6768), None);
    }

    #[test]
    fn parse_tasklist_image_reads_the_csv_image_name() {
        let out = "\"python.exe\",\"9876\",\"Console\",\"1\",\"45,678 K\"\r\n";
        assert_eq!(parse_tasklist_image(out), Some("python.exe".to_string()));
        // `/FI` with no match prints an INFO line, not a CSV row.
        assert_eq!(
            parse_tasklist_image("INFO: No tasks are running which match the specified criteria."),
            None
        );
    }

    #[test]
    fn parse_lsof_listener_reads_the_row_below_the_header() {
        let out = "COMMAND   PID USER   FD   TYPE DEVICE SIZE/OFF NODE NAME\n\
                   python  12345 garm    7u  IPv4 0x1234      0t0  TCP 127.0.0.1:6768 (LISTEN)\n";
        assert_eq!(parse_lsof_listener(out), Some(("python".into(), 12345)));
        // Header only: lsof found nothing, which is not a listener.
        assert_eq!(
            parse_lsof_listener("COMMAND   PID USER   FD   TYPE DEVICE\n"),
            None
        );
    }

    #[test]
    fn parse_ss_listener_picks_the_row_for_the_requested_port() {
        let out = "State  Recv-Q Send-Q Local Address:Port  Peer Address:Port Process\n\
             LISTEN 0      4096       127.0.0.1:16768      0.0.0.0:*     users:((\"decoy\",pid=1,fd=3))\n\
             LISTEN 0      4096       127.0.0.1:6768       0.0.0.0:*     users:((\"python\",pid=12345,fd=7))\n";
        // :16768 must not satisfy a :6768 lookup.
        assert_eq!(parse_ss_listener(out, 6768), Some(("python".into(), 12345)));
        assert_eq!(parse_ss_listener(out, 16768), Some(("decoy".into(), 1)));
        assert_eq!(parse_ss_listener(out, 9999), None);
    }

    #[test]
    fn parse_ss_listener_returns_none_without_process_ownership() {
        // ss omits `users:` for processes the caller does not own. Guessing a
        // pid here would point the kill path at the wrong process.
        let out = "State  Recv-Q Send-Q Local Address:Port Peer Address:Port\n\
                   LISTEN 0      4096       0.0.0.0:6768       0.0.0.0:*\n";
        assert_eq!(parse_ss_listener(out, 6768), None);
    }

    #[test]
    fn parse_pid_from_lsof_detail_returns_none_for_unknown_or_malformed() {
        assert_eq!(parse_pid_from_lsof_detail("unknown process"), None);
        assert_eq!(parse_pid_from_lsof_detail(""), None);
        assert_eq!(
            parse_pid_from_lsof_detail("rapportd pid not-a-number"),
            None
        );
        // Missing the " pid " separator entirely.
        assert_eq!(parse_pid_from_lsof_detail("rapportd 594"), None);
    }

    /// Round-trip: the bail string produced by `format_all_foreign_bail` must
    /// be matched by `port_conflict::is_port_conflict` (so the persistent-
    /// conflict marker keeps tracking it) AND the occupant must be parseable
    /// by `port_conflict::parse_occupant` (so analytics/Sentry get the
    /// process name and pid).
    #[test]
    fn all_foreign_bail_round_trips_through_port_conflict_helpers() {
        let bail = format_all_foreign_bail(6768, "rapportd pid 594", (6769, 6790));
        assert!(
            port_conflict::is_port_conflict(&bail),
            "bail must match is_port_conflict so the marker keeps tracking; got: {bail}"
        );
        let (cmd, pid) = port_conflict::parse_occupant(&bail);
        assert_eq!(cmd.as_deref(), Some("rapportd"), "bail: {bail}");
        assert_eq!(pid, Some(594), "bail: {bail}");
    }

    /// Mirror round-trip for the unknown-occupant path (lsof returned nothing
    /// useful). `parse_occupant` should return None/None instead of inventing
    /// a fake cmd from "unknown process".
    #[test]
    fn all_foreign_bail_with_unknown_occupant_round_trips() {
        let bail = format_all_foreign_bail(6768, "unknown process", (6769, 6790));
        assert!(port_conflict::is_port_conflict(&bail));
        let (cmd, pid) = port_conflict::parse_occupant(&bail);
        assert!(cmd.is_none(), "got cmd: {cmd:?} from bail: {bail}");
        assert!(pid.is_none(), "got pid: {pid:?} from bail: {bail}");
    }

    /// The "stale headroom proxy holding the port" bail must NOT trigger
    /// the foreign-process port-conflict path — those are separate
    /// fingerprints in Sentry. Verifies the boundary stays intact.
    #[test]
    fn already_running_bail_is_not_classified_as_foreign_conflict() {
        let bail = format_already_running_bail(6768);
        assert!(
            !port_conflict::is_port_conflict(&bail),
            "stale-proxy bail must not match foreign-port classifier; got: {bail}"
        );
        // But the lib.rs port-conflict-failure classifier (which fingerprints
        // both shapes the same way) still catches it via its second condition.
        assert!(crate::is_port_conflict_failure(&bail));
    }

    #[test]
    fn wait_for_port_free_detects_release() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            !wait_for_port_free(port, Duration::from_millis(200)),
            "port held by a live listener must not report free"
        );
        drop(listener);
        assert!(
            wait_for_port_free(port, Duration::from_secs(2)),
            "port must report free shortly after the listener is dropped"
        );
    }

    /// Reproduces the upgrade-rollback scenario from the Sentry report: a
    /// *healthy* orphaned proxy (answers /readyz) squatting on the backend
    /// port. Normal launch (`force_unhealthy_too=false`) must leave it alone
    /// and bail; an upgrade boot validation (`true`) must reclaim it anyway so
    /// the new venv can bind. Ignored by default — spawns a child process,
    /// binds a port, and kills the process. Run locally:
    /// `cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored reclaim_orphan`
    #[test]
    #[ignore]
    fn reclaim_orphan_proxy_respects_upgrade_override() {
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };

        // Stand-in for a live old-version proxy: answers 200 on every path so
        // `/readyz` reads healthy. argv is `python3 -c ...`, which deliberately
        // does NOT match `stop_headroom`'s pattern-kill — exactly the orphan
        // that survives into the spawn pre-flight.
        let script = r#"
import http.server, socketserver, sys
class S(socketserver.TCPServer):
    allow_reuse_address = True
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'ok')
    def log_message(self, *a):
        pass
S(('127.0.0.1', int(sys.argv[1])), H).serve_forever()
"#;

        // The trailing marker arg makes the stand-in pass the
        // pid_is_headroom_backend argv identity gate (needs both a "headroom"
        // and a "proxy" token), like a real orphan running as
        // `.../headroom proxy ...` from the app-support venv path would.
        let mut child = crate::proc::command("/usr/bin/python3")
            .arg("-c")
            .arg(script)
            .arg(port.to_string())
            .arg("--headroom-proxy-test-standin")
            .spawn()
            .expect("spawn stand-in proxy");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !probe_backend_readyz_ok(port) {
            assert!(
                std::time::Instant::now() < deadline,
                "stand-in proxy never became healthy on port {port}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        // Normal launch: a healthy occupant is left alone, reclaim bails.
        assert!(
            reclaim_orphan_proxy(port, false).is_err(),
            "force=false must bail on a healthy occupant"
        );
        assert!(
            probe_backend_readyz_ok(port),
            "force=false must NOT kill the healthy occupant"
        );

        // Upgrade validation: the healthy old proxy is reclaimed regardless.
        assert!(
            reclaim_orphan_proxy(port, true).is_ok(),
            "force=true must reclaim even a healthy occupant"
        );
        assert!(
            wait_for_port_free(port, Duration::from_secs(3)),
            "force=true must free the port"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn reclaim_orphan_proxy_never_kills_foreign_http_server() {
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };

        // An unrelated local HTTP server (no headroom marker in argv) that
        // happens to hold the port: reclaim must bail — even with force — and
        // leave the process alive.
        let script = r#"
import http.server, socketserver, sys
class S(socketserver.TCPServer):
    allow_reuse_address = True
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'ok')
    def log_message(self, *a):
        pass
S(('127.0.0.1', int(sys.argv[1])), H).serve_forever()
"#;
        let mut child = crate::proc::command("/usr/bin/python3")
            .arg("-c")
            .arg(script)
            .arg(port.to_string())
            .spawn()
            .expect("spawn foreign http server");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !probe_backend_readyz_ok(port) {
            assert!(
                std::time::Instant::now() < deadline,
                "foreign server never came up on port {port}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        assert!(
            reclaim_orphan_proxy(port, true).is_err(),
            "reclaim must refuse to kill a non-headroom occupant"
        );
        assert!(
            probe_backend_readyz_ok(port),
            "foreign occupant must still be alive after reclaim bails"
        );
        assert!(matches!(
            diagnose_proxy_port(port),
            PortState::ForeignOccupant(_)
        ));

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn probe_backend_readyz_ok_false_when_nothing_listening() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(
            !probe_backend_readyz_ok(port),
            "no server listening means /readyz is not healthy"
        );
    }

    #[test]
    fn extract_required_pydantic_core_version_pulls_pin_from_systemerror() {
        let log = "Traceback (most recent call last):\n  File \"<frozen runpy>\", line 189, in _run_module_as_main\n  ...\nSystemError: The installed pydantic-core version (2.46.3) is incompatible with the current pydantic version, which requires 2.41.5. If you encounter this error, make sure that you haven't upgraded pydantic-core manually.\n";
        assert_eq!(
            extract_required_pydantic_core_version(log),
            Some("2.41.5".into())
        );
    }

    #[test]
    fn extract_required_pydantic_core_version_returns_none_on_unrelated_traceback() {
        let log = "Traceback (most recent call last):\n  File \"x.py\", line 1, in <module>\nImportError: No module named 'foo'\n";
        assert!(extract_required_pydantic_core_version(log).is_none());
    }

    #[test]
    fn extract_required_pydantic_core_version_returns_none_when_marker_missing_version() {
        // Future-proof: if pydantic ever changes the message format and there's
        // no version after "which requires ", we must not return an empty pin.
        let log = "pydantic-core mismatch: which requires nothing useful here";
        assert!(extract_required_pydantic_core_version(log).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn managed_headroom_startup_uses_supported_proxy_args() {
        backend_port::reset_for_tests();
        let default_port = backend_port::DEFAULT_BACKEND_PORT.to_string();
        let entrypoint_args = headroom_entrypoint_startup_args(Some("0.28.0"), true);
        assert!(entrypoint_args.starts_with(&[
            "proxy".to_string(),
            "--port".to_string(),
            default_port.clone(),
            "--no-http2".to_string(),
            "--log-messages".to_string(),
        ]));
        assert!(entrypoint_args.contains(&"--learn".to_string()));
        assert!(entrypoint_args.contains(&"--no-memory-tools".to_string()));
        assert!(entrypoint_args.contains(&"--no-memory-context".to_string()));
        assert!(entrypoint_args.contains(&"--memory-db-path".to_string()));

        let python_args = headroom_python_startup_args();
        assert_eq!(
            python_args,
            vec![
                "-m".to_string(),
                "headroom.proxy.server".to_string(),
                "--port".to_string(),
                default_port,
                "--no-http2".to_string(),
                "--log-messages".to_string(),
            ]
        );
        // The python -m fallback must not pass learn flags; argparse on
        // headroom.proxy.server doesn't define them and would exit 2.
        assert!(!python_args.contains(&"--learn".to_string()));
        assert!(!python_args.contains(&"--no-memory-tools".to_string()));
        assert!(!python_args.contains(&"--no-memory-context".to_string()));
        assert!(!python_args.contains(&"--memory-db-path".to_string()));

        backend_port::reset_for_tests();
    }

    /// Regression (Sentry RUST-4A): the 0.26.0 fallback runtime's click
    /// entrypoint has no `--no-http2` option, so passing it made every boot
    /// validation attempt exit 2 and the upgrade time out. The flag must be
    /// gated on runtime >= 0.28.0; unknown versions assume the pinned runtime.
    #[test]
    #[serial_test::serial]
    fn entrypoint_args_gate_no_http2_on_runtime_version() {
        backend_port::reset_for_tests();

        assert!(!headroom_entrypoint_startup_args(Some("0.26.0"), true)
            .contains(&"--no-http2".to_string()));
        assert!(!headroom_entrypoint_startup_args(Some("0.27.0"), true)
            .contains(&"--no-http2".to_string()));
        assert!(headroom_entrypoint_startup_args(Some("0.28.0"), true)
            .contains(&"--no-http2".to_string()));
        assert!(headroom_entrypoint_startup_args(Some("1.0.0"), true)
            .contains(&"--no-http2".to_string()));
        // Unknown or malformed receipt version: assume pinned runtime.
        assert!(headroom_entrypoint_startup_args(None, true).contains(&"--no-http2".to_string()));
        assert!(headroom_entrypoint_startup_args(Some("garbage"), true)
            .contains(&"--no-http2".to_string()));
        // The python -m argparse variant has defined --no-http2 since 0.10.0
        // and must keep it unconditionally.
        assert!(headroom_python_startup_args().contains(&"--no-http2".to_string()));

        backend_port::reset_for_tests();
    }

    /// CCR is ON by default from 0.9.6: both reasons it was disabled landed
    /// upstream (#2953 for the discarded stream flip, and 0.37.0's
    /// buffered_ccr_response grace window for #2465/#3079). `--no-ccr` is now
    /// only emitted when a machine explicitly asks for it, and the version gate
    /// still applies because the unified flag only exists from 0.31.0 -- on the
    /// 0.28.0 fallback runtime click would exit 2 and boot validation would
    /// fail like RUST-4A.
    #[test]
    #[serial_test::serial]
    fn entrypoint_args_omit_no_ccr_unless_explicitly_forced() {
        backend_port::reset_for_tests();

        // Default: CCR stays enabled, so the flag is absent at every version.
        for v in [
            None,
            Some("0.28.0"),
            Some("0.31.0"),
            Some("0.37.0"),
            Some("garbage"),
        ] {
            assert!(
                !headroom_entrypoint_startup_args(v, true).contains(&"--no-ccr".to_string()),
                "CCR must be on by default (version {v:?})"
            );
        }

        // The opt-in restore honours falsey spellings rather than treating the
        // variable's mere presence as "off".
        assert!(!super::desktop_forces_no_ccr_from(None));
        for off in ["", "  ", "0", "false", "FALSE", "no", "off"] {
            assert!(
                !super::desktop_forces_no_ccr_from(Some(off)),
                "{off:?} should keep CCR on"
            );
        }
        for on in ["1", "true", " yes "] {
            assert!(
                super::desktop_forces_no_ccr_from(Some(on)),
                "{on:?} should restore --no-ccr"
            );
        }
        // The `python -m headroom.proxy.server` argparse defines no CCR
        // option at all; passing it there would exit 2 on every fallback boot.
        assert!(!headroom_python_startup_args().contains(&"--no-ccr".to_string()));
        // Version-gated flags stay out of the staleness signature: a runtime
        // that cannot take the flag would otherwise never match and the
        // desktop would stop/restart the proxy on every check.
        assert!(!super::expected_proxy_arg_signature(true).contains(&"--no-ccr"));

        backend_port::reset_for_tests();
    }

    /// Regression (Sentry RUST-1M): the "coding" savings persona only exists in
    /// _PROFILES from 0.30.0. On the 0.28.0 fallback runtime, passing it makes
    /// the proxy raise on startup and exit before opening the port. Must gate on
    /// runtime >= 0.30.0; unknown versions assume the pinned (current) runtime.
    #[test]
    #[serial_test::serial]
    fn savings_profile_gated_on_runtime_version() {
        assert_eq!(savings_profile_for_runtime(Some("0.28.0")), "agent-90");
        assert_eq!(savings_profile_for_runtime(Some("0.29.9")), "agent-90");
        assert_eq!(savings_profile_for_runtime(Some("0.30.0")), "coding");
        assert_eq!(savings_profile_for_runtime(Some("1.0.0")), "coding");
        // Unknown or malformed receipt version: assume pinned runtime.
        assert_eq!(savings_profile_for_runtime(None), "coding");
        assert_eq!(savings_profile_for_runtime(Some("garbage")), "coding");
    }

    /// The cc-switch reconciler must never run without the Official-branch
    /// upstream reset (upstream PR #3166): enabling it there leaves a captured
    /// third-party endpoint live process-wide after the user switches back to
    /// Claude Official, so Anthropic OAuth traffic goes to the old provider.
    /// The desktop ships that reset in SITECUSTOMIZE_PY, so the flag tracks
    /// whether the injection landed -- not a wheel version, which is what the
    /// previous gate guessed wrong (it named 0.36.3; 0.36.3, 0.36.4 and 0.36.5
    /// all shipped without the fix).
    #[test]
    fn cc_switch_reconcile_gated_on_the_injected_reset() {
        assert_eq!(cc_switch_reconcile_for_spawn(true), "1");
        // Injection write failed: the reset is not in the interpreter, so the
        // reconciler stays off rather than running unfixed.
        assert_eq!(cc_switch_reconcile_for_spawn(false), "0");
    }

    /// The Python half of the same invariant. The guard is load-bearing, not
    /// corrective like the other four ports: whenever it cannot bind it must
    /// clear the env the desktop just set, because `reconciler_enabled()` reads
    /// it later in the same process.
    #[test]
    fn sitecustomize_cc_switch_reset_guard_fails_closed() {
        let py = super::SITECUSTOMIZE_PY;
        // Patches the real entry point, on the class (server.py imports the
        // class object, so a module-level shim would be bypassed).
        assert!(py.contains("import headroom.proxy.cc_switch_reconciler as _hd_ccs_mod"));
        assert!(py.contains("_hd_ccs_mod.CCSwitchReconciler.tick = _hd_ccs_tick"));
        // Resets through the reconciler's own setter, to the effective default
        // (the user's endpoint when one is configured, Anthropic's otherwise).
        assert!(py.contains("self._set_upstream(target)"));
        assert!(py.contains("target = _hd_ccs_pinned or self.default_upstream"));
        // Every failure path -- kill switch included -- disables the watcher.
        assert!(py.contains("HEADROOM_CC_SWITCH_RESET_GUARD"));
        assert!(py.contains(r#"raise RuntimeError("cc-switch reset guard disabled by env")"#));
        assert!(py.contains(r#"_hd_os.environ["HEADROOM_CC_SWITCH_RECONCILE"] = "0""#));
        // Self-neutralizing: a wheel carrying #3166 has already reset
        // current_upstream by the time the wrapper looks.
        assert!(py.contains("if self.current_upstream in (None, target):"));
        // A renamed instance attribute fails closed at bind time rather than
        // binding a wrapper that raises on every tick and resets nothing.
        assert!(py.contains(
            r#"for _hd_ccs_needed in ("proxy_url", "default_upstream", "set_upstream", "path"):"#
        ));
        // And a wrapper that does fail at runtime says so once.
        assert!(py.contains("event=cc_switch_official_reset_failed"));
    }

    /// A user-configured upstream reaches the backend as env, and clearing it
    /// has to reach the backend too: all three vars are always set, so a spawn
    /// after "Off" cannot inherit the previous endpoint from the environment.
    #[test]
    fn upstream_spawn_env_carries_the_configured_endpoint() {
        use crate::state::{UpstreamOverride, UpstreamOverrideMode};

        let off = upstream_spawn_env(&UpstreamOverride::default());
        assert_eq!(off.target_api_url, "");
        assert_eq!(off.pin_upstream, "0");
        // No third-party endpoint, so the full pipeline stays on for the
        // Anthropic users who are the overwhelming majority.
        assert_eq!(off.lossless, "0");

        let fallback = upstream_spawn_env(&UpstreamOverride {
            mode: UpstreamOverrideMode::Fallback,
            base_url: "https://api.z.ai/api/anthropic".into(),
            has_token: true,
            ..Default::default()
        });
        assert_eq!(fallback.target_api_url, "https://api.z.ai/api/anthropic");
        // Fallback boots at the endpoint but lets a cc-switch capture win.
        assert_eq!(fallback.pin_upstream, "0");
        assert_eq!(fallback.lossless, "1");

        let overridden = upstream_spawn_env(&UpstreamOverride {
            mode: UpstreamOverrideMode::Override,
            base_url: "https://api.z.ai/api/anthropic".into(),
            has_token: true,
            ..Default::default()
        });
        assert_eq!(overridden.pin_upstream, "1");
        assert_eq!(overridden.lossless, "1");

        // A mode with no URL is not an upstream: booting the proxy at an empty
        // target would be worse than not configuring one.
        let empty = upstream_spawn_env(&UpstreamOverride {
            mode: UpstreamOverrideMode::Override,
            base_url: String::new(),
            has_token: false,
            ..Default::default()
        });
        assert_eq!(empty.target_api_url, "");
        assert_eq!(empty.pin_upstream, "0");
        assert_eq!(empty.lossless, "0");
    }

    /// Override mode has to survive a cc-switch provider switch, and the
    /// Official branch has to return to the user's endpoint rather than
    /// Anthropic's -- otherwise "override" would last until the next switch.
    #[test]
    fn sitecustomize_pins_the_configured_upstream() {
        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains("HEADROOM_CC_SWITCH_PIN_UPSTREAM"));
        assert!(py.contains(
            r#"_hd_ccs_pinned = _hd_os.environ.get("ANTHROPIC_TARGET_API_URL", "").strip()"#
        ));
        assert!(py.contains("event=cc_switch_upstream_pinned"));
        // The pinned endpoint becomes the default the reset returns to.
        assert!(py.contains("target = _hd_ccs_pinned or self.default_upstream"));
        // Pin on with nothing usable to pin to is a broken spawn, not a
        // reason to fall back to capturing.
        assert!(py.contains("pinned upstream missing or malformed"));
    }

    /// The reconciler must point clients at the intercept, not at the port the
    /// Python proxy bound. Upstream builds proxy_url from `config.port` -- the
    /// internal hop -- so without this pin every cc-switch provider switch
    /// rewrote the user's settings.json onto 6768 (or a 6769-6790 fallback),
    /// dropping that client out of the intercept's activity feed, request
    /// counts and savings accounting, and stranding it on the next launch that
    /// had to move ports.
    #[test]
    fn cc_switch_advertises_the_intercept_port() {
        assert_eq!(
            cc_switch_proxy_url(),
            format!(
                "http://127.0.0.1:{}",
                crate::proxy_intercept::INTERCEPT_PORT
            )
        );
        // The port clients are configured with app-wide, spelled out here so a
        // change to either constant has to be deliberate.
        assert_eq!(cc_switch_proxy_url(), "http://127.0.0.1:6767");
        assert_ne!(
            cc_switch_proxy_url(),
            format!(
                "http://127.0.0.1:{}",
                crate::backend_port::DEFAULT_BACKEND_PORT
            )
        );

        let py = super::SITECUSTOMIZE_PY;
        assert!(py.contains(r#"_hd_os.environ.get("HEADROOM_CC_SWITCH_PROXY_URL", "")"#));
        assert!(py.contains("_hd_ccs_mod.CCSwitchReconciler.__init__ = _hd_ccs_init"));
        assert!(py.contains("self.proxy_url = _hd_ccs_url"));
        // Missing or malformed falls into the same fail-closed except as the
        // reset guard: no reconciler rather than one writing a bad base_url.
        assert!(py.contains("HEADROOM_CC_SWITCH_PROXY_URL missing or malformed"));
    }

    /// Regression: `start_headroom_background` previously built `startup_variants`
    /// before pre-flight ran, so when fallback called `backend_port::set(6769)`
    /// the variants still spawned with `--port 6768` and both failed with
    /// EADDRINUSE. The arg helpers read the atomic at call time, so as long as
    /// the helpers are invoked AFTER fallback has updated the atomic, the
    /// chosen fallback port flows through.
    #[test]
    #[serial_test::serial]
    fn startup_args_reflect_fallback_port_set_after_default() {
        backend_port::reset_for_tests();
        backend_port::set(6770);

        let entrypoint_args = headroom_entrypoint_startup_args(None, true);
        let port_idx = entrypoint_args
            .iter()
            .position(|a| a == "--port")
            .expect("entrypoint args contain --port");
        assert_eq!(entrypoint_args[port_idx + 1], "6770");

        let python_args = headroom_python_startup_args();
        let port_idx = python_args
            .iter()
            .position(|a| a == "--port")
            .expect("python args contain --port");
        assert_eq!(python_args[port_idx + 1], "6770");

        backend_port::reset_for_tests();
    }

    #[test]
    fn linux_bootstrap_requirements_skip_optional_memory_and_ml_packages() {
        let linux_requirements = bootstrap_requirements_lock_for_target("linux");

        assert!(linux_requirements.contains("ast-grep-cli=="));
        assert!(!linux_requirements.contains("hnswlib=="));
        assert!(linux_requirements.contains("opentelemetry-api=="));
        assert!(!linux_requirements.contains("torch=="));
        assert!(!linux_requirements.contains("sentence-transformers=="));
        assert!(linux_requirements.contains("mcp=="));
        assert!(linux_requirements.contains("onnxruntime=="));
        assert!(linux_requirements.contains("transformers=="));
    }

    #[test]
    fn parse_headroom_learn_timestamp_accepts_generated_date_lines() {
        let block = r#"
<!-- headroom:learn:start -->
## Headroom Learned Patterns
*Auto-generated by `headroom learn` on 2026-03-26 — do not edit manually*
- First pattern
<!-- headroom:learn:end -->
"#;

        let timestamp = super::parse_headroom_learn_timestamp(block).expect("timestamp");

        assert_eq!(timestamp.to_rfc3339(), "2026-03-26T00:00:00+00:00");
    }

    #[test]
    fn count_headroom_learn_patterns_counts_bullets_inside_block() {
        let block = r#"
<!-- headroom:learn:start -->
- First pattern
*Auto-generated by `headroom learn` on 2026-03-26 — do not edit manually*
- Second pattern
<!-- headroom:learn:end -->
"#;

        assert_eq!(super::count_headroom_learn_patterns(block), Some(2));
    }

    #[test]
    fn count_headroom_learn_patterns_returns_none_for_block_with_no_bullets() {
        let block = r#"
<!-- headroom:learn:start -->
*Auto-generated by `headroom learn` on 2026-03-26 — do not edit manually*
<!-- headroom:learn:end -->
"#;

        assert_eq!(super::count_headroom_learn_patterns(block), None);
    }

    #[test]
    fn count_headroom_learn_patterns_ignores_non_bullet_lines() {
        let block = r#"
<!-- headroom:learn:start -->
## Heading
Plain text without a dash
- Real pattern
<!-- headroom:learn:end -->
"#;

        assert_eq!(super::count_headroom_learn_patterns(block), Some(1));
    }

    #[test]
    fn parse_headroom_learn_timestamp_returns_none_when_no_timestamp_line() {
        let block = r#"
<!-- headroom:learn:start -->
- Some pattern
<!-- headroom:learn:end -->
"#;

        assert!(super::parse_headroom_learn_timestamp(block).is_none());
    }

    #[test]
    fn parse_headroom_learn_timestamp_accepts_rfc3339_datetime() {
        let block = r#"
<!-- headroom:learn:start -->
*Auto-generated by `headroom learn` on 2026-03-26T14:30:00Z — do not edit manually*
- Pattern
<!-- headroom:learn:end -->
"#;

        let timestamp = super::parse_headroom_learn_timestamp(block).expect("timestamp");

        assert_eq!(timestamp.to_rfc3339(), "2026-03-26T14:30:00+00:00");
    }

    #[test]
    fn parse_block_extracts_sections_and_bullets() {
        let content = r#"# Prior heading

<!-- headroom:learn:start -->
## Headroom Learned Patterns
*Auto-generated by `headroom learn` on 2026-04-22 — do not edit manually*

### Large Files
*~15,000 tokens/session saved*
- `src/App.tsx` is very large
- Also `lib.rs`

### Learned: environment
- Use uv run python
<!-- headroom:learn:end -->
"#;

        let sections = super::parse_headroom_learn_block(content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "Large Files");
        assert_eq!(
            sections[0].bullets,
            vec!["`src/App.tsx` is very large", "Also `lib.rs`"]
        );
        assert_eq!(sections[1].title, "Learned: environment");
        assert_eq!(sections[1].bullets, vec!["Use uv run python"]);
    }

    #[test]
    fn parse_block_returns_empty_when_no_block_present() {
        let content = "Just some CLAUDE.md content without markers.\n- a bullet";
        assert!(super::parse_headroom_learn_block(content).is_empty());
    }

    #[test]
    fn delete_applied_bullet_removes_one_bullet() {
        let content = "\
before
<!-- headroom:learn:start -->
### Foo
- alpha
- beta
- gamma
<!-- headroom:learn:end -->
after
";
        let out = super::delete_applied_bullet(content, "Foo", "beta");
        assert!(out.contains("- alpha"));
        assert!(!out.contains("- beta"));
        assert!(out.contains("- gamma"));
        assert!(out.contains("### Foo"));
    }

    #[test]
    fn delete_applied_bullet_drops_section_when_last_bullet_removed() {
        let content = "\
<!-- headroom:learn:start -->
### Foo
- only
### Bar
- keep
<!-- headroom:learn:end -->
";
        let out = super::delete_applied_bullet(content, "Foo", "only");
        assert!(!out.contains("### Foo"));
        assert!(out.contains("### Bar"));
        assert!(out.contains("- keep"));
    }

    #[test]
    fn delete_applied_bullet_drops_last_section_and_keeps_end_marker() {
        // Regression: previously the final flush truncated the trailing
        // `<!-- headroom:learn:end -->` marker when the last section was
        // emptied, which left the block unparseable on the next read.
        let content = "\
<!-- headroom:learn:start -->
### Foo
- keep
### Bar
- removeme
<!-- headroom:learn:end -->
";
        let out = super::delete_applied_bullet(content, "Bar", "removeme");
        assert!(out.contains("### Foo"), "earlier section preserved");
        assert!(out.contains("- keep"), "earlier bullet preserved");
        assert!(!out.contains("### Bar"), "emptied last section dropped");
        assert!(!out.contains("- removeme"), "removed bullet absent");
        assert!(
            out.contains("<!-- headroom:learn:end -->"),
            "end marker preserved, got:\n{out}"
        );
        assert!(
            !super::parse_headroom_learn_block(&out).is_empty(),
            "block still parseable after deletion"
        );
    }

    #[test]
    fn delete_applied_bullet_removes_whole_block_when_empty() {
        let content = "prefix\n\n<!-- headroom:learn:start -->\n### Foo\n- only\n<!-- headroom:learn:end -->\n\nsuffix\n";
        let out = super::delete_applied_bullet(content, "Foo", "only");
        assert!(!out.contains("headroom:learn:start"));
        assert!(!out.contains("headroom:learn:end"));
        assert!(out.contains("prefix"));
        assert!(out.contains("suffix"));
    }

    #[test]
    fn delete_applied_bullet_is_noop_when_bullet_missing() {
        let content =
            "<!-- headroom:learn:start -->\n### Foo\n- alpha\n<!-- headroom:learn:end -->\n";
        let out = super::delete_applied_bullet(content, "Foo", "not-there");
        assert_eq!(out, content);
    }

    const START_ONLY: &str = "<!-- headroom:learn:start -->\n\
## Headroom Learned Patterns\n\
*Auto-generated by `headroom learn` on 2026-09-02 - do not edit manually*\n\
\n\
### Foo\n\
- alpha\n\
- beta\n\
\n\
## Manual memory index\n\
\n\
- [x](y.md)\n";

    #[test]
    fn learn_block_parse_tolerates_missing_end_marker() {
        let sections = super::parse_headroom_learn_block(START_ONLY);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Foo");
        assert_eq!(sections[0].bullets, vec!["alpha", "beta"]);
    }

    #[test]
    fn learn_block_repair_inserts_end_marker_and_is_idempotent() {
        let fixed = super::repair_headroom_learn_block(START_ONLY).expect("repaired");
        assert!(fixed.contains("- beta\n<!-- headroom:learn:end -->\n\n## Manual memory index\n"));
        assert!(fixed.ends_with("- [x](y.md)\n"));
        assert_eq!(
            super::parse_headroom_learn_block(&fixed),
            super::parse_headroom_learn_block(START_ONLY)
        );
        assert!(super::repair_headroom_learn_block(&fixed).is_none());
        // EOF case: no heading after the block.
        let eof =
            super::repair_headroom_learn_block("<!-- headroom:learn:start -->\n### Foo\n- a\n")
                .expect("repaired");
        assert_eq!(
            eof,
            "<!-- headroom:learn:start -->\n### Foo\n- a\n<!-- headroom:learn:end -->\n"
        );
        assert!(super::repair_headroom_learn_block("no block here\n").is_none());
    }

    #[test]
    fn learn_block_delete_heals_start_only_block() {
        let out = super::delete_applied_bullet(START_ONLY, "Foo", "alpha");
        assert!(out.contains("<!-- headroom:learn:end -->"));
        assert!(out.contains("## Manual memory index"));
        let sections = super::parse_headroom_learn_block(&out);
        assert_eq!(sections[0].bullets, vec!["beta"]);
    }

    #[test]
    fn learn_block_repair_file_writes_once() {
        let root = unique_temp_dir("headroom-learn-repair");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("MEMORY.md");
        fs::write(&path, START_ONLY).expect("write");
        assert!(super::repair_headroom_learn_block_file(&path));
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("<!-- headroom:learn:end -->"));
        assert!(!super::repair_headroom_learn_block_file(&path));
        assert!(!super::repair_headroom_learn_block_file(
            &root.join("missing.md")
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_headroom_learn_timestamp_returns_none_for_malformed_date() {
        let block = r#"
<!-- headroom:learn:start -->
*Auto-generated by `headroom learn` on not-a-date — do not edit manually*
- Pattern
<!-- headroom:learn:end -->
"#;

        assert!(super::parse_headroom_learn_timestamp(block).is_none());
    }

    #[test]
    fn encode_claude_project_folder_name_replaces_slashes_preserving_hyphens() {
        // Claude Code's on-disk encoding only substitutes '/' with '-'; literal
        // hyphens in the path are preserved verbatim. Verified against real
        // ~/.claude/projects/ folder names.
        assert_eq!(
            super::encode_claude_project_folder_name("/Users/alice/my-project"),
            "-Users-alice-my-project"
        );
    }

    #[test]
    fn encode_claude_project_folder_name_handles_root_slash() {
        assert_eq!(super::encode_claude_project_folder_name("/foo"), "-foo");
    }

    #[test]
    fn read_headroom_learn_metadata_from_path_falls_back_to_file_metadata() {
        let root = unique_temp_dir("headroom-learn-metadata");
        fs::create_dir_all(&root).expect("create root");
        let memory = root.join("MEMORY.md");
        fs::write(
            &memory,
            r#"
<!-- headroom:learn:start -->
- First pattern
- Second pattern
<!-- headroom:learn:end -->
"#,
        )
        .expect("write memory file");

        let metadata = read_headroom_learn_metadata_from_path(&memory).expect("metadata");

        assert_eq!(metadata.metadata.pattern_count, Some(2));
        assert!(metadata.metadata.learned_at.is_some());
        assert!(metadata.sort_key.is_some());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verify_sha256_file_accepts_matching_content_and_rejects_mismatches() {
        let root = unique_temp_dir("headroom-sha256");
        fs::create_dir_all(&root).expect("create root");
        let artifact = root.join("artifact.bin");
        fs::write(&artifact, b"headroom").expect("write artifact");

        let checksum = sha256_bytes(b"headroom");
        verify_sha256_file(&artifact, &checksum).expect("matching checksum");

        let err = verify_sha256_file(&artifact, "not-the-right-checksum")
            .expect_err("mismatched checksum should fail");
        assert!(err.to_string().contains("checksum mismatch"));

        let _ = fs::remove_dir_all(root);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    fn write_executable(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, body).expect("write script");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("chmod");
        }
    }

    fn seed_test_runtime(prefix: &str) -> (PathBuf, ManagedRuntime, ToolManager) {
        let root = unique_temp_dir(prefix);
        let runtime = ManagedRuntime::bootstrap_root(&root);
        runtime.ensure_layout().expect("layout");
        fs::create_dir_all(&runtime.venv_dir).expect("venv dir");
        fs::write(runtime.venv_dir.join("marker"), b"live-v1").expect("marker");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            br#"{"version":"0.0.1"}"#,
        )
        .expect("receipt");
        let manager = ToolManager::new(runtime.clone());
        (root, runtime, manager)
    }

    /// RUST-8E: a venv orphaned from its base interpreter keeps its redirector
    /// stub and READY flag, so the venv-only gate reported "installed" and the
    /// launch dead-ended in a Sentry start failure (exit 103, `No Python at`)
    /// instead of routing to setup, which re-downloads the runtime.
    #[test]
    fn python_runtime_installed_requires_the_standalone_base() {
        let (_root, runtime, manager) = seed_test_runtime("orphaned-venv-base");
        for marker in [runtime.ready_flag(), runtime.managed_python()] {
            fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
            fs::write(&marker, b"").expect("marker");
        }
        assert!(
            !manager.python_runtime_installed(),
            "a venv whose standalone base is gone must not read as installed"
        );

        let base = runtime.standalone_python();
        fs::create_dir_all(base.parent().expect("parent")).expect("mkdir");
        fs::write(&base, b"").expect("base");

        // RUST-6S third shape: same blind spot one file over. A venv whose
        // pyvenv.cfg was deleted in place keeps all three markers, but every
        // spawn dies with exit 106 / `No pyvenv.cfg file`.
        assert!(
            !manager.python_runtime_installed(),
            "a venv missing pyvenv.cfg must not read as installed"
        );
        fs::write(runtime.venv_dir.join("pyvenv.cfg"), b"").expect("pyvenv.cfg");
        // RUST-C8: python.exe survived but the stdlib next to it did not.
        // Every spawn dies with `No module named 'encodings'`, and bootstrap
        // skipped the reinstall because the executable existed.
        assert!(
            !manager.python_runtime_installed(),
            "a base whose stdlib is gone must not read as installed"
        );
        let landmark = seed_stdlib_landmark(&runtime);
        assert!(
            manager.python_runtime_installed(),
            "all markers present must read as installed"
        );
        fs::remove_file(&landmark).expect("remove landmark");
        assert!(!runtime.standalone_runtime_intact());
    }

    /// Creates the `os.py` landmark CPython's getpath looks for next to the
    /// base interpreter, and returns its path.
    fn seed_stdlib_landmark(runtime: &ManagedRuntime) -> PathBuf {
        let landmark = if cfg!(target_os = "windows") {
            runtime.python_dir.join("Lib").join("os.py")
        } else {
            runtime
                .python_dir
                .join("lib")
                .join("python3.12")
                .join("os.py")
        };
        fs::create_dir_all(landmark.parent().expect("parent")).expect("mkdir");
        fs::write(&landmark, b"").expect("landmark");
        landmark
    }

    /// RUST-82: `python -m venv` runs ensurepip through `check_output` and
    /// re-raises only its exit status, so an install-blocking failure reached
    /// Sentry as a bare "returned non-zero exit status 1" with the cause
    /// discarded. Creating the venv without pip and running ensurepip ourselves
    /// is what venv does internally, but keeps ensurepip's stderr. If anyone
    /// folds these back into one step, that blind spot returns.
    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn create_managed_venv_runs_ensurepip_as_its_own_step() {
        let (root, runtime, manager) = seed_test_runtime("venv-ensurepip-split");
        let log = root.join("argv.log");
        let script = format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display());
        write_executable(&runtime.standalone_python(), &script);
        write_executable(&runtime.managed_python(), &script);

        manager.create_managed_venv().expect("fake python succeeds");

        let calls = fs::read_to_string(&log).expect("argv log");
        let mut lines = calls.lines();
        assert_eq!(
            lines.next().map(str::to_string),
            Some(format!(
                "-m venv --without-pip {}",
                runtime.venv_dir.to_string_lossy()
            )),
            "venv is created without pip: {calls}"
        );
        assert_eq!(
            lines.next(),
            Some("-m ensurepip --upgrade --default-pip"),
            "ensurepip runs as its own command, so its stderr survives: {calls}"
        );
        assert_eq!(
            lines.next(),
            Some("-m pip --version"),
            "pip is still verified afterwards: {calls}"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// `build_command` set plain "utf-8", silently overriding the
    /// `utf-8:backslashreplace` that `proc::command` applies on purpose -- which
    /// re-armed the RUST-7C UnicodeEncodeError kill for every python child
    /// built here (a lone surrogate in a log line must not be fatal).
    #[test]
    fn build_command_keeps_the_backslashreplace_stdio_encoding() {
        let cmd = build_command(Path::new("python3"), &["-V"], Path::new("."));
        let encoding = cmd
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("PYTHONIOENCODING"))
            .and_then(|(_, value)| value)
            .expect("PYTHONIOENCODING is set");
        assert_eq!(encoding, std::ffi::OsStr::new("utf-8:backslashreplace"));
    }

    /// A host pip.conf with `user = true` (or PIP_USER in the environment)
    /// reaches the managed venv's pip and fails every install with "Can not
    /// perform a '--user' install" (RUST-6S). `build_command` must pin the
    /// switch off and aim pip's config lookup at the null device.
    #[test]
    fn build_command_isolates_pip_from_host_pip_config() {
        let cmd = build_command(Path::new("python3"), &["-V"], Path::new("."));
        let env_of = |name: &str| {
            cmd.get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .and_then(|(_, value)| value)
        };
        assert_eq!(env_of("PIP_USER"), Some(std::ffi::OsStr::new("0")));
        let devnull = if cfg!(windows) { "NUL" } else { "/dev/null" };
        assert_eq!(
            env_of("PIP_CONFIG_FILE"),
            Some(std::ffi::OsStr::new(devnull))
        );
    }

    /// socks4 (and anything else httpx cannot parse) must be treated as
    /// unusable so the spawn path strips it (RUST-AS/RUST-AT); supported
    /// schemes and the empty "proxy disabled" value must pass through.
    #[test]
    fn httpx_proxy_url_support_matches_httpx_scheme_map() {
        for ok in [
            "",
            "  ",
            "http://127.0.0.1:8080",
            "https://proxy.corp:3128",
            "socks5://127.0.0.1:10808",
            "SOCKS5h://127.0.0.1:10808",
        ] {
            assert!(
                super::httpx_supports_proxy_url(ok),
                "{ok:?} should be supported"
            );
        }
        for bad in [
            "socks4://127.0.0.1:10808",
            "socks4a://127.0.0.1:1080",
            "socks://127.0.0.1:1080",
            "127.0.0.1:8080",
        ] {
            assert!(
                !super::httpx_supports_proxy_url(bad),
                "{bad:?} should be stripped"
            );
        }
    }

    #[test]
    fn parse_reg_value_reads_typed_rows() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    socks4://127.0.0.1:10808\r\n";
        assert_eq!(
            super::parse_reg_value(out, "ProxyEnable").as_deref(),
            Some("0x1")
        );
        assert_eq!(
            super::parse_reg_value(out, "ProxyServer").as_deref(),
            Some("socks4://127.0.0.1:10808")
        );
        assert!(super::parse_reg_value(out, "ProxyOverride").is_none());
        assert!(super::reg_dword_is_set("0x1"));
        assert!(!super::reg_dword_is_set("0x0"));
    }

    /// RUST-AY: v2rayN "set system proxy" writes a single socks4 URL into
    /// `ProxyServer`; urllib mounts it under http AND https, httpx's
    /// AsyncClient() raises, and the backend dies before opening the port.
    /// Nothing is usable -> the child goes direct.
    #[test]
    fn registry_socks4_single_value_goes_direct() {
        assert_eq!(
            super::registry_proxy_env_overrides("socks4://127.0.0.1:10808"),
            Some(vec![("NO_PROXY".to_string(), "*".to_string())])
        );
    }

    #[test]
    fn registry_clean_proxies_need_no_override() {
        assert_eq!(super::registry_proxy_env_overrides("1.2.3.4:8080"), None);
        assert_eq!(
            super::registry_proxy_env_overrides("http=1.2.3.4:8080;https=1.2.3.4:8080"),
            None
        );
        // A keyed socks entry with an explicit httpx-supported scheme is kept
        // verbatim by urllib's backfill, so both mounts parse: no override.
        assert_eq!(
            super::registry_proxy_env_overrides("socks=socks5://127.0.0.1:1080"),
            None
        );
        assert_eq!(super::registry_proxy_env_overrides(""), None);
        assert_eq!(super::registry_proxy_env_overrides("   "), None);
    }

    /// RUST-B3: CPython backfills missing http/https keys from a keyed
    /// `socks=` entry as `socks4://addr`, so `http=...;socks=...` mounts
    /// socks4 on https and crashed the backend despite RUST-AY. The usable
    /// http half survives as an explicit env var.
    #[test]
    fn registry_keyed_socks_backfills_and_overrides() {
        assert_eq!(
            super::registry_proxy_env_overrides("http=1.2.3.4:8080;socks=127.0.0.1:1080"),
            Some(vec![(
                "http_proxy".to_string(),
                "http://1.2.3.4:8080".to_string()
            )])
        );
        // socks alone backfills BOTH mounts with socks4: nothing usable.
        assert_eq!(
            super::registry_proxy_env_overrides("socks=127.0.0.1:1080"),
            Some(vec![("NO_PROXY".to_string(), "*".to_string())])
        );
        // Explicit http/https pairs win over the backfill (CPython uses
        // `proxies.get(proto) or socks`), so a fully-specified config with a
        // stray socks entry keeps its usable halves.
        assert_eq!(
            super::registry_proxy_env_overrides(
                "http=1.2.3.4:8080;https=1.2.3.4:8080;socks=127.0.0.1:1080"
            ),
            None
        );
    }

    /// A corporate box with a usable http proxy next to a broken socks4
    /// https entry keeps the working half as an explicit env var (which also
    /// makes urllib skip the registry entirely).
    #[test]
    fn registry_mixed_keeps_the_usable_entry() {
        assert_eq!(
            super::registry_proxy_env_overrides("http=socks4://127.0.0.1:10808;https=1.2.3.4:8080"),
            Some(vec![(
                "https_proxy".to_string(),
                "https://1.2.3.4:8080".to_string()
            )])
        );
    }

    /// RUST-6S: pip cannot use socks proxies without pysocks, so any socks
    /// scheme must read as strippable for pip-class children; http/https and
    /// the empty "proxy disabled" value pass through.
    #[test]
    fn socks_proxy_detection_covers_every_socks_scheme() {
        for socks in [
            "socks5://127.0.0.1:10808",
            "SOCKS4://127.0.0.1:1080",
            "socks5h://proxy.local:1080",
            "  socks4a://127.0.0.1:9 ",
        ] {
            assert!(
                super::is_socks_proxy_value(socks),
                "{socks:?} should be stripped for pip children"
            );
        }
        for other in ["", "http://127.0.0.1:8080", "https://proxy.corp:3128"] {
            assert!(
                !super::is_socks_proxy_value(other),
                "{other:?} should pass through"
            );
        }
    }

    /// RUST-A8: SSLKEYLOGFILE pointing at an unopenable path must read as
    /// unusable so the spawn paths strip it; a creatable path must pass
    /// through (python would create it the same way).
    #[test]
    fn sslkeylogfile_usability_matches_python_eager_open_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let creatable = dir.path().join("keylog.txt");
        assert!(
            super::sslkeylogfile_is_usable(creatable.to_str().unwrap()),
            "creatable path should be usable"
        );
        let unopenable = dir.path().join("no-such-dir").join("keylog.txt");
        assert!(
            !super::sslkeylogfile_is_usable(unopenable.to_str().unwrap()),
            "path in a missing directory fails python's eager open and should be stripped"
        );
    }

    /// RUST-A0 / RUST-8K: on Windows the variable is dropped even when the
    /// path opens, because the standalone python.exe cannot honor it (no
    /// OPENSSL_Applink); elsewhere only an unopenable path is stripped.
    #[test]
    fn sslkeylogfile_is_always_stripped_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let creatable = dir.path().join("keylog.txt");
        let creatable = creatable.to_str().unwrap();
        assert!(super::sslkeylogfile_strip_reason(creatable, true).is_some());
        assert!(super::sslkeylogfile_strip_reason(creatable, false).is_none());
        let unopenable = dir.path().join("no-such-dir").join("keylog.txt");
        assert!(super::sslkeylogfile_strip_reason(unopenable.to_str().unwrap(), false).is_some());
        assert!(super::sslkeylogfile_strip_reason("  ", true).is_none());
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn managed_venv_has_pip_rejects_a_venv_whose_pip_is_missing() {
        // A venv interrupted during ensurepip keeps its interpreter but has no
        // pip. Bootstrap used to gate on the interpreter alone, so it skipped
        // venv creation forever and every install died with "No module named
        // pip" (RUST-66 / RUST-6M).
        let (_root, runtime, manager) = seed_test_runtime("venv-pip-probe");
        assert!(!manager.managed_venv_has_pip(), "no interpreter yet");

        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 1\n");
        assert!(
            !manager.managed_venv_has_pip(),
            "interpreter present but `-m pip --version` fails"
        );

        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 0\n");
        assert!(manager.managed_venv_has_pip(), "pip answers");
    }

    #[test]
    fn claude_plugin_install_args_omit_the_scope_flag() {
        // `--scope user` is Claude Code's default and older CLIs reject the
        // flag outright, failing the whole install (RUST-6K).
        let plugin = PLUGIN_ADDONS
            .iter()
            .find(|p| p.id == "caveman")
            .expect("caveman addon");
        assert_eq!(
            crate::tool_manager::PluginHost::ClaudeCode.install_args(plugin),
            vec!["plugin", "install", plugin.plugin_ref]
        );
    }

    #[test]
    fn tool_enabled_reads_receipt_flag_and_defaults_true() {
        let (_root, runtime, manager) = seed_test_runtime("tool-enabled");
        // No receipt -> default enabled.
        assert!(manager.tool_enabled("markitdown"));

        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"0.1.6","enabled":false}"#,
        )
        .expect("receipt");
        assert!(!manager.tool_enabled("markitdown"));

        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"0.1.6","enabled":true}"#,
        )
        .expect("receipt");
        assert!(manager.tool_enabled("markitdown"));
    }

    /// Every addon keeps its card on every platform; the ones with no artifact
    /// for this target carry a reason instead (codebase-memory ships no Windows
    /// binary, rtk none for windows-aarch64). A card without an artifact and
    /// without a reason would offer an Install button that only ever errors.
    #[test]
    fn addons_without_an_artifact_for_this_target_carry_a_reason() {
        let (_root, _runtime, manager) = seed_test_runtime("manifest-artifacts");
        let tools = manager.list_tools();
        let reason_for = |id: &str| {
            tools
                .iter()
                .find(|tool| tool.id == id)
                .unwrap_or_else(|| panic!("{id} card must exist on every platform"))
                .unavailable_reason
                .clone()
        };

        assert_eq!(
            reason_for("rtk").is_none(),
            rtk_distribution_artifact().is_ok(),
            "rtk is installable exactly when its artifact resolves"
        );
        assert_eq!(
            reason_for("codebase-memory").is_none(),
            codebase_memory_distribution_artifact().is_ok(),
            "codebase-memory is installable exactly when its artifact resolves"
        );
        // Nothing platform-gated about the Python addons: never gray them.
        assert!(reason_for("markitdown").is_none());
        assert!(reason_for("serena").is_none());
    }

    #[test]
    fn unavailable_reason_names_the_platform_and_the_reason() {
        // The message is user-facing copy on a grayed card, so it has to say
        // both which platform is missing and why, without a bare target triple.
        let Some(reason) = addon_unavailable_reason("codebase-memory") else {
            return; // this target has a build; nothing to phrase
        };
        assert!(reason.starts_with("Not available on "), "{reason}");
        assert!(reason.contains("codebase-memory"), "{reason}");
    }

    fn listed_tool(manager: &ToolManager, id: &str) -> ManagedTool {
        manager
            .list_tools()
            .into_iter()
            .find(|tool| tool.id == id)
            .expect("tool in manifest")
    }

    #[test]
    fn list_tools_reports_the_installed_version_and_offers_the_pinned_one() {
        let (_root, runtime, manager) = seed_test_runtime("addon-update");

        // Not installed: the card shows what an install would give you.
        let absent = listed_tool(&manager, "markitdown");
        assert_eq!(absent.version, MARKITDOWN_PINNED_VERSION);
        assert!(!absent.update_available);
        assert!(absent.available_version.is_none());

        let entrypoint = manager.markitdown_entrypoint();
        fs::create_dir_all(entrypoint.parent().expect("bin parent")).expect("bin dir");
        fs::write(&entrypoint, b"#!/bin/sh\n").expect("entrypoint");
        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"0.1.5","enabled":true}"#,
        )
        .expect("receipt");

        // Installed and behind: report what is on disk, offer the pin.
        let stale = listed_tool(&manager, "markitdown");
        assert_eq!(stale.version, "0.1.5");
        assert!(stale.update_available);
        assert_eq!(
            stale.available_version.as_deref(),
            Some(MARKITDOWN_PINNED_VERSION)
        );

        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            format!(r#"{{"version":"{MARKITDOWN_PINNED_VERSION}","enabled":true}}"#).as_bytes(),
        )
        .expect("receipt");
        let current = listed_tool(&manager, "markitdown");
        assert_eq!(current.version, MARKITDOWN_PINNED_VERSION);
        assert!(!current.update_available);

        // The pin is a minimum: someone ahead of it is current, not overdue.
        // Prompting here would be an offer to downgrade.
        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"99.0.0","enabled":true}"#,
        )
        .expect("receipt");
        let ahead = listed_tool(&manager, "markitdown");
        assert_eq!(ahead.version, "99.0.0");
        assert!(!ahead.update_available);

        // An unreadable version is treated as current for the same reason.
        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"main","enabled":true}"#,
        )
        .expect("receipt");
        assert!(!listed_tool(&manager, "markitdown").update_available);

        // Disabled and behind: no Update button. Every installer writes
        // `enabled: true`, so updating here would switch the addon back on
        // behind the user's back.
        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"0.1.5","enabled":false}"#,
        )
        .expect("receipt");
        let disabled = listed_tool(&manager, "markitdown");
        assert_eq!(disabled.version, "0.1.5");
        assert!(!disabled.update_available);

        // A receipt whose payload is gone is not an install: no version claim,
        // no update prompt.
        fs::remove_file(&entrypoint).expect("remove entrypoint");
        let orphaned = listed_tool(&manager, "markitdown");
        assert_eq!(orphaned.version, MARKITDOWN_PINNED_VERSION);
        assert!(!orphaned.update_available);
    }

    #[test]
    fn pending_addon_update_treats_the_pin_as_a_minimum() {
        assert_eq!(
            pending_addon_update("serena", Some("1.6.1"), "1.7.0").as_deref(),
            Some("1.7.0")
        );
        assert_eq!(pending_addon_update("serena", Some("1.7.0"), "1.7.0"), None);
        // Ahead of the pin: current, not overdue. Never offer a downgrade.
        assert_eq!(pending_addon_update("serena", Some("1.8.0"), "1.7.0"), None);
        assert_eq!(pending_addon_update("serena", Some("2.0.0"), "1.7.0"), None);
        assert_eq!(pending_addon_update("serena", None, "1.7.0"), None);
        assert_eq!(
            pending_addon_update("serena", Some("nightly"), "1.7.0"),
            None
        );

        // Plugins track a marketplace, so the action is the check: offered
        // whenever installed, with no version to advertise.
        assert_eq!(
            pending_addon_update("ponytail", Some("4.7.0"), PLUGIN_DISPLAY_VERSION).as_deref(),
            Some("")
        );
        assert_eq!(
            pending_addon_update("caveman", None, PLUGIN_DISPLAY_VERSION),
            None
        );

        // Self-maintaining: rtk is refreshed at launch, headroom rides the
        // runtime upgrade. Neither gets an Update button, stale or not.
        assert_eq!(
            pending_addon_update("rtk", Some("0.1.0"), RTK_VERSION),
            None
        );
        assert_eq!(
            pending_addon_update("headroom", Some("0.1.0"), HEADROOM_PINNED_VERSION),
            None
        );
    }

    #[test]
    fn rtk_is_refreshed_on_launch_so_it_never_advertises_a_manual_update() {
        let (_root, runtime, manager) = seed_test_runtime("addon-update-rtk");
        fs::create_dir_all(&runtime.bin_dir).expect("bin dir");
        fs::write(manager.rtk_entrypoint(), b"#!/bin/sh\n").expect("entrypoint");
        fs::write(
            runtime.tools_dir.join("rtk.json"),
            br#"{"version":"0.1.0","enabled":true}"#,
        )
        .expect("receipt");

        let rtk = listed_tool(&manager, "rtk");
        assert_eq!(rtk.version, "0.1.0");
        assert!(manager.rtk_needs_install());
        assert!(!rtk.update_available);
    }

    #[test]
    fn markitdown_conversion_count_reads_counter_file() {
        let (_root, runtime, manager) = seed_test_runtime("markitdown-count");
        assert_eq!(manager.markitdown_conversion_count(), None);

        fs::write(runtime.tools_dir.join("markitdown-conversions"), b"7\n").expect("counter");
        assert_eq!(manager.markitdown_conversion_count(), Some(7));

        fs::write(runtime.tools_dir.join("markitdown-conversions"), b"junk").expect("counter");
        assert_eq!(manager.markitdown_conversion_count(), None);

        fs::write(runtime.tools_dir.join("markitdown-conversions"), b"0").expect("counter");
        assert_eq!(manager.markitdown_conversion_count(), None);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn markitdown_shim_counts_file_conversions_but_not_flag_calls() {
        let (_root, _runtime, manager) = seed_test_runtime("markitdown-shim");
        write_executable(
            &manager.markitdown_entrypoint(),
            "#!/bin/sh\necho converted:$1\n",
        );
        manager.ensure_markitdown_shim().expect("shim");

        let run = |arg: &str| {
            let out = crate::proc::command(manager.markitdown_shim_path())
                .arg(arg)
                .output()
                .expect("run shim");
            assert!(out.status.success(), "shim exited non-zero for {arg}");
            String::from_utf8_lossy(&out.stdout).to_string()
        };

        assert!(run("/tmp/a.docx").contains("converted:/tmp/a.docx"));
        run("/tmp/b.xlsx");
        run("--help"); // flag-only invocation must not count
        assert_eq!(manager.markitdown_conversion_count(), Some(2));
    }

    #[test]
    fn count_serena_tool_calls_counts_marker_lines_in_txt_logs_only() {
        let dir = unique_temp_dir("serena-log-count");
        assert_eq!(
            super::count_serena_tool_calls_in_dir(&dir),
            None,
            "missing dir"
        );

        fs::create_dir_all(&dir).expect("log dir");
        fs::write(
            dir.join("mcp_20260723_1.txt"),
            "INFO find_symbol: {\"name\": \"foo\"}; session_id: abc\n\
             INFO Result: 42 lines\n\
             INFO read_file: {\"path\": \"x.rs\"}; session_id: abc\n",
        )
        .expect("log file");
        fs::write(dir.join("ignored.log"), "x: {}; session_id: zzz\n").expect("non-txt");
        assert_eq!(super::count_serena_tool_calls_in_dir(&dir), Some(2));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn serena_savings_label_combines_calls_live_tokens_and_timing() {
        assert_eq!(super::serena_savings_label(None, None), None);
        assert_eq!(
            super::serena_savings_label(Some(1), None).as_deref(),
            Some("1 tool call today")
        );
        assert_eq!(
            super::serena_savings_label(None, Some((500, None))).as_deref(),
            Some("~500 tokens returned this session")
        );
        let age = std::time::Duration::from_secs(2 * 3600 + 14 * 60 + 5);
        assert_eq!(
            super::serena_savings_label(Some(231), Some((48_200, Some(age)))).as_deref(),
            Some("231 tool calls today, ~48k tokens returned in 2h 14m")
        );
        assert_eq!(super::compact_token_count(1_234_000), "1.2M");
    }

    #[test]
    fn parse_ps_etime_handles_all_ps_formats() {
        use std::time::Duration;
        assert_eq!(
            super::parse_ps_etime("05:33"),
            Some(Duration::from_secs(333))
        );
        assert_eq!(
            super::parse_ps_etime(" 02:14:33"),
            Some(Duration::from_secs(2 * 3600 + 14 * 60 + 33))
        );
        assert_eq!(
            super::parse_ps_etime("3-01:02:03"),
            Some(Duration::from_secs(3 * 86400 + 3600 + 2 * 60 + 3))
        );
        assert_eq!(super::parse_ps_etime("garbage"), None);
    }

    #[test]
    fn oldest_serena_session_age_matches_entrypoint_and_subcommand_only() {
        let marker = "/tmp/hr/serena-venv/bin/serena";
        let ps = "\
  01:00 /usr/bin/python other-tool start-mcp-server\n\
  45:00 /tmp/hr/serena-venv/bin/python /tmp/hr/serena-venv/bin/serena start-mcp-server --context claude-code\n\
2-00:00:00 /tmp/hr/serena-venv/bin/serena --help\n\
  02:14:33 /tmp/hr/serena-venv/bin/serena start-mcp-server --context codex\n";
        assert_eq!(
            super::oldest_serena_session_age(ps, marker),
            Some(std::time::Duration::from_secs(2 * 3600 + 14 * 60 + 33)),
            "picks oldest matching session; ignores other tools and non-server invocations"
        );
        assert_eq!(super::oldest_serena_session_age("", marker), None);
    }

    #[test]
    fn compact_duration_formats() {
        use std::time::Duration;
        assert_eq!(
            super::compact_duration(Duration::from_secs(30)),
            "under a minute"
        );
        assert_eq!(super::compact_duration(Duration::from_secs(47 * 60)), "47m");
        assert_eq!(super::compact_duration(Duration::from_secs(2 * 3600)), "2h");
        assert_eq!(
            super::compact_duration(Duration::from_secs(2 * 3600 + 14 * 60)),
            "2h 14m"
        );
    }

    #[test]
    fn fetch_serena_output_tokens_sums_stats_across_tools() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let body = r#"{"stats":{"find_symbol":{"num_times_called":3,"input_tokens":10,"output_tokens":4500},"read_file":{"num_times_called":1,"input_tokens":5,"output_tokens":500}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write");
        });

        let total = super::fetch_serena_output_tokens(&format!("http://127.0.0.1:{port}"));
        handle.join().expect("server thread");
        assert_eq!(total, Some(5000));

        // Nothing listening: must be None, not Some(0).
        let unused = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead_port = unused.local_addr().expect("addr").port();
        drop(unused);
        assert_eq!(
            super::fetch_serena_output_tokens(&format!("http://127.0.0.1:{dead_port}")),
            None
        );
    }

    #[test]
    fn list_tools_exposes_savings_labels() {
        let (_root, runtime, manager) = seed_test_runtime("savings-labels");
        let label_of = |id: &str| {
            manager
                .list_tools()
                .into_iter()
                .find(|tool| tool.id == id)
                .expect("tool listed")
                .savings_label
        };

        assert_eq!(label_of("markitdown"), None);
        fs::write(runtime.tools_dir.join("markitdown-conversions"), b"3").expect("counter");
        assert_eq!(label_of("markitdown").as_deref(), Some("3 docs converted"));

        let ponytail = label_of("ponytail").expect("ponytail label");
        assert!(
            ponytail.contains("benchmark"),
            "must cite benchmark: {ponytail}"
        );
        assert_eq!(label_of("rtk"), None, "rtk figure comes from RuntimeStatus");
    }

    #[test]
    fn commit_headroom_upgrade_removes_backup() {
        let (root, runtime, manager) = seed_test_runtime("commit-backup");
        let backup = manager.venv_backup_dir();
        fs::create_dir_all(&backup).expect("backup dir");
        fs::write(backup.join("old-marker"), b"old").expect("old marker");
        fs::write(
            manager.headroom_receipt_backup_path(),
            br#"{"version":"0.0.0"}"#,
        )
        .expect("receipt backup");

        manager.commit_headroom_upgrade().expect("commit ok");

        assert!(!backup.exists(), "backup should be removed");
        assert!(!manager.headroom_receipt_backup_path().exists());
        assert!(
            runtime.venv_dir.join("marker").exists(),
            "live venv untouched"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn commit_headroom_upgrade_is_noop_without_backup() {
        let (root, _runtime, manager) = seed_test_runtime("commit-noop");
        manager.commit_headroom_upgrade().expect("noop ok");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn collect_native_extensions_walks_recursively_and_filters_by_extension() {
        let root = unique_temp_dir("collect-natives");
        let sp = root.join("site-packages");
        let pkg = sp.join("torch").join("_C");
        fs::create_dir_all(&pkg).expect("nested dirs");
        // Should be collected:
        fs::write(sp.join("mmh3.cpython-312-darwin.so"), b"").expect("so file");
        fs::write(pkg.join("libtorch_python.dylib"), b"").expect("dylib file");
        fs::write(
            sp.join("hnswlib").join("hnswlib.cpython-312-darwin.so"),
            b"",
        )
        .or_else(|_| {
            fs::create_dir_all(sp.join("hnswlib")).and_then(|_| {
                fs::write(
                    sp.join("hnswlib").join("hnswlib.cpython-312-darwin.so"),
                    b"",
                )
            })
        })
        .expect("nested so");
        // Should NOT be collected:
        fs::write(sp.join("README.md"), b"docs").expect("md file");
        fs::write(sp.join("module.py"), b"code").expect("py file");
        fs::write(pkg.join("_C.pyi"), b"stubs").expect("pyi file");

        let mut paths = Vec::new();
        super::collect_native_extensions(&sp, &mut paths).expect("walk ok");
        paths.sort();

        assert_eq!(paths.len(), 3, "expected 3 native files, got {paths:?}");
        assert!(paths
            .iter()
            .any(|p| p.ends_with("mmh3.cpython-312-darwin.so")));
        assert!(paths.iter().any(|p| p.ends_with("libtorch_python.dylib")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with("hnswlib.cpython-312-darwin.so")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ad_hoc_sign_venv_natives_returns_zero_when_site_packages_missing() {
        // Fresh venv dir with no lib/python3.12/site-packages subtree — the
        // helper must silently return 0 rather than error. This is the path
        // exercised on every install before pip has populated the venv.
        let (root, _runtime, manager) = seed_test_runtime("codesign-no-sitepackages");
        assert_eq!(manager.ad_hoc_sign_venv_natives(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn commit_headroom_upgrade_removes_lock_backup() {
        let (root, _runtime, manager) = seed_test_runtime("commit-lock-backup");
        let lock_backup = manager.lock_backup_path();
        fs::write(&lock_backup, b"old-lock==1.0\n").expect("seed lock backup");

        manager.commit_headroom_upgrade().expect("commit ok");

        assert!(
            !lock_backup.exists(),
            "in-place lock backup should be removed on commit"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_and_read_in_place_marker_roundtrip() {
        let (root, _runtime, manager) = seed_test_runtime("marker-roundtrip");
        let lock_backup = manager.lock_backup_path();
        fs::write(&lock_backup, b"old-lock\n").expect("seed lock backup");

        manager
            .write_upgrade_marker("0.11.0", Some("0.10.8"), Some(&lock_backup))
            .expect("marker");

        let (prev, target, backup) = manager
            .read_in_place_marker()
            .expect("marker should parse as in-place");
        assert_eq!(prev, "0.10.8");
        assert_eq!(target, "0.11.0");
        assert_eq!(backup.as_deref(), Some(lock_backup.as_path()));

        // Atomic-rebuild markers (no previous_version) must not parse as in-place.
        manager
            .write_upgrade_marker("0.11.0", None, None)
            .expect("atomic marker");
        assert!(manager.read_in_place_marker().is_none());

        // Wheel-only shape (previous_version but no lock backup).
        manager
            .write_upgrade_marker("0.10.8", Some("0.10.7"), None)
            .expect("wheel-only marker");
        let (prev, target, backup) = manager.read_in_place_marker().expect("parse");
        assert_eq!(prev, "0.10.7");
        assert_eq!(target, "0.10.8");
        assert!(backup.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_headroom_receipt_after_in_place_upgrade_rewrites_artifact() {
        // Guards the legacy-sha migration path. When LEGACY_REQUIREMENTS_LOCK_SHAS
        // is empty (current state after the 0.19.0 lock regen), there is no
        // legacy fixture to inject — re-enable when a future cosmetic-only lock
        // change re-populates the list.
        if super::LEGACY_REQUIREMENTS_LOCK_SHAS.is_empty() {
            return;
        }
        let (root, runtime, manager) = seed_test_runtime("receipt-rewrite");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.10.4",
                "artifact": {
                    "url": "https://old.example/headroom_ai-0.10.4.whl",
                    "sha256": "oldoldold",
                    "requirementsLockSha256": super::LEGACY_REQUIREMENTS_LOCK_SHAS[0],
                },
                "mcp": { "configured": false },
            }))
            .unwrap(),
        )
        .expect("seed receipt");

        let release = HeadroomRelease {
            version: "0.10.8".into(),
            wheel_url: "https://new.example/headroom_ai-0.10.8.whl".into(),
            sha256: "newnewnew".into(),
        };
        let mcp = serde_json::json!({ "configured": true, "proxyUrl": "http://127.0.0.1:6767" });
        manager
            .update_headroom_receipt_after_in_place_upgrade(&release, mcp.clone())
            .expect("receipt update ok");

        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(runtime.tools_dir.join("headroom.json")).expect("read receipt"),
        )
        .expect("parse receipt");
        assert_eq!(receipt["version"], "0.10.8");
        assert_eq!(receipt["artifact"]["url"], release.wheel_url);
        assert_eq!(receipt["artifact"]["sha256"], release.sha256);
        assert_eq!(
            receipt["artifact"]["requirementsLockSha256"],
            requirements_lock_sha(super::bootstrap_requirements_lock()),
            "legacy sha must be migrated to the comment-insensitive form"
        );
        assert_eq!(receipt["mcp"], mcp);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_headroom_upgrade_restores_from_backup() {
        // Simulate state after a boot-validation failure: a NEW venv is live
        // at venv_dir, the previous one is at venv_dir.backup, and the old
        // receipt is snapshotted.
        let (root, runtime, manager) = seed_test_runtime("rollback");
        let backup = manager.venv_backup_dir();

        // "Move" the current live venv to backup and create a fake "new" venv.
        fs::rename(&runtime.venv_dir, &backup).expect("move aside");
        fs::create_dir_all(&runtime.venv_dir).expect("new venv dir");
        fs::write(runtime.venv_dir.join("new-marker"), b"new").expect("new marker");
        fs::copy(
            runtime.tools_dir.join("headroom.json"),
            manager.headroom_receipt_backup_path(),
        )
        .expect("snapshot receipt");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            br#"{"version":"9.9.9"}"#,
        )
        .expect("new receipt");

        manager
            .rollback_headroom_upgrade()
            .expect("rollback succeeds");

        // The live venv should now be the original (contains "marker", not "new-marker").
        assert!(
            runtime.venv_dir.join("marker").exists(),
            "restored marker present"
        );
        assert!(
            !runtime.venv_dir.join("new-marker").exists(),
            "new venv wiped"
        );
        assert!(!backup.exists(), "backup consumed");
        let receipt = fs::read(runtime.tools_dir.join("headroom.json")).expect("receipt");
        assert!(
            String::from_utf8_lossy(&receipt).contains("0.0.1"),
            "receipt restored to previous: {}",
            String::from_utf8_lossy(&receipt)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_headroom_upgrade_is_noop_without_backup() {
        let (root, _runtime, manager) = seed_test_runtime("rollback-noop");
        manager.rollback_headroom_upgrade().expect("noop ok");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recover_from_interrupted_upgrade_restores_backup_as_live() {
        // Simulate an interrupted upgrade: marker present, venv.backup has
        // the real old venv, venv has some partial new content.
        let (root, runtime, manager) = seed_test_runtime("interrupted");
        let backup = manager.venv_backup_dir();

        // Move original venv aside (as atomic_upgrade would).
        fs::rename(&runtime.venv_dir, &backup).expect("move aside");
        // Simulate a partial new venv left by an interrupted pip install.
        fs::create_dir_all(&runtime.venv_dir).expect("partial venv");
        fs::write(runtime.venv_dir.join("partial-marker"), b"interrupted").expect("partial");
        // Marker file and receipt backup (written by atomic_upgrade).
        manager
            .write_upgrade_marker("0.8.2", None, None)
            .expect("marker");
        fs::copy(
            runtime.tools_dir.join("headroom.json"),
            manager.headroom_receipt_backup_path(),
        )
        .expect("receipt snapshot");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            br#"{"version":"9.9.9-partial"}"#,
        )
        .expect("new receipt");

        let recovered = manager.recover_from_interrupted_upgrade();
        assert!(recovered, "recovery should fire");

        // The live venv should be the restored original.
        assert!(
            runtime.venv_dir.join("marker").exists(),
            "original restored"
        );
        assert!(
            !runtime.venv_dir.join("partial-marker").exists(),
            "partial new venv discarded"
        );
        assert!(!backup.exists(), "backup consumed");
        assert!(
            !manager.upgrade_marker_path().exists(),
            "marker cleared after recovery"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recover_from_interrupted_upgrade_is_noop_without_marker() {
        let (root, _runtime, manager) = seed_test_runtime("interrupted-noop");
        assert!(!manager.recover_from_interrupted_upgrade());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recover_from_interrupted_upgrade_keeps_marker_when_pip_fails() {
        // Offline / broken-python recovery: pip fails, so the marker and the
        // receipt backup must survive for a retry on the next launch —
        // clearing them used to leave a mixed venv that the restored receipt
        // declared healthy.
        let (root, runtime, manager) = seed_test_runtime("recover-pip-fails");
        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 1\n");
        manager
            .write_upgrade_marker("0.10.8", Some("0.10.7"), None)
            .expect("marker");
        fs::write(
            manager.headroom_receipt_backup_path(),
            br#"{"version":"0.10.7"}"#,
        )
        .expect("receipt snapshot");

        assert!(!manager.recover_from_interrupted_upgrade());
        assert!(
            manager.upgrade_marker_path().exists(),
            "marker kept for retry when pip fails"
        );
        assert!(
            manager.headroom_receipt_backup_path().exists(),
            "receipt backup kept for retry when pip fails"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn recover_from_interrupted_upgrade_handles_wheel_only_marker() {
        // In-place marker without a lock backup (wheel-only interrupted
        // upgrade). A stub python stands in for a successful pip reinstall;
        // we assert the file-manipulation side: receipt restored, marker
        // cleared.
        let (root, runtime, manager) = seed_test_runtime("recover-wheel-only");
        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 0\n");
        manager
            .write_upgrade_marker("0.10.8", Some("0.10.7"), None)
            .expect("marker");
        fs::write(
            manager.headroom_receipt_backup_path(),
            br#"{"version":"0.10.7"}"#,
        )
        .expect("receipt snapshot");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            br#"{"version":"0.10.8-partial"}"#,
        )
        .expect("partial receipt");

        assert!(manager.recover_from_interrupted_upgrade());

        assert!(
            !manager.upgrade_marker_path().exists(),
            "marker cleared after recovery"
        );
        assert!(
            !manager.headroom_receipt_backup_path().exists(),
            "receipt backup consumed"
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(runtime.tools_dir.join("headroom.json")).expect("read receipt"),
        )
        .expect("parse receipt");
        assert_eq!(receipt["version"], "0.10.7", "receipt restored to previous");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn recover_from_interrupted_upgrade_handles_in_place_marker_with_lock_backup() {
        // In-place marker with a lock backup. Recovery should: copy the lock
        // backup back to the active lock path, remove the backup, restore the
        // receipt, and clear the marker. A stub python stands in for
        // successful pip calls.
        let (root, runtime, manager) = seed_test_runtime("recover-lock-backup");
        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 0\n");
        let active_lock = manager.active_lock_path();
        let lock_backup = manager.lock_backup_path();
        fs::write(&active_lock, b"new-lock==2.0\n").expect("seed active lock");
        fs::write(&lock_backup, b"old-lock==1.0\n").expect("seed lock backup");

        manager
            .write_upgrade_marker("0.11.0", Some("0.10.8"), Some(&lock_backup))
            .expect("marker");
        fs::write(
            manager.headroom_receipt_backup_path(),
            br#"{"version":"0.10.8"}"#,
        )
        .expect("receipt snapshot");

        assert!(manager.recover_from_interrupted_upgrade());

        assert!(!manager.upgrade_marker_path().exists(), "marker cleared");
        assert!(!lock_backup.exists(), "lock backup consumed");
        assert_eq!(
            fs::read(&active_lock).expect("read active lock"),
            b"old-lock==1.0\n",
            "active lock rolled back to snapshot content"
        );
        assert!(!manager.headroom_receipt_backup_path().exists());
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(runtime.tools_dir.join("headroom.json")).expect("read receipt"),
        )
        .expect("parse receipt");
        assert_eq!(receipt["version"], "0.10.8");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn atomic_upgrade_purges_stale_backup_and_reports_failure_without_python() {
        // Without a real standalone python available, create_managed_venv()
        // will fail. We still want to verify that a stale backup from a
        // previous aborted upgrade is removed before the attempt, and that
        // the live venv is restored byte-for-byte after the failure.
        let (root, runtime, manager) = seed_test_runtime("atomic-stale");

        // Pre-seed a stale backup (simulating a previous aborted upgrade).
        let stale_backup = manager.venv_backup_dir();
        fs::create_dir_all(&stale_backup).expect("stale backup");
        fs::write(stale_backup.join("stale-marker"), b"stale").expect("stale marker");

        // Fake release — bogus URL ensures download/install would fail even
        // if we somehow reached that step.
        let release = HeadroomRelease {
            version: "0.0.0-test".into(),
            wheel_url: "https://example.invalid/headroom.whl".into(),
            sha256: "deadbeef".into(),
        };

        let outcome = manager.atomic_upgrade_headroom(&release, |_| {}, false);

        match outcome {
            UpgradeOutcome::InstallFailed { restored, .. } => {
                assert!(restored, "old venv should be restored after failure");
            }
            UpgradeOutcome::InstalledPendingValidation { .. } => {
                panic!("unexpected success without python");
            }
        }

        // Live venv is back with its original content.
        assert!(
            runtime.venv_dir.join("marker").exists(),
            "original marker restored"
        );
        // Stale backup purged (either consumed during restore or cleaned at start).
        assert!(!stale_backup.exists(), "stale backup removed");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn requirements_are_stale_recognizes_legacy_sha_and_migrates_receipt() {
        if super::LEGACY_REQUIREMENTS_LOCK_SHAS.is_empty() {
            return;
        }
        let (root, runtime, manager) = seed_test_runtime("legacy-sha-migrate");
        let legacy_sha = super::LEGACY_REQUIREMENTS_LOCK_SHAS[0];
        let receipt_path = runtime.tools_dir.join("headroom.json");
        fs::write(
            &receipt_path,
            serde_json::to_vec(&serde_json::json!({
                "version": "0.2.50",
                "artifact": { "requirementsLockSha256": legacy_sha },
            }))
            .unwrap(),
        )
        .expect("receipt");

        assert!(
            !manager.requirements_are_stale(),
            "legacy sha should be treated as current"
        );

        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt_path).expect("receipt read")).expect("json");
        assert_eq!(
            receipt["artifact"]["requirementsLockSha256"],
            requirements_lock_sha(super::bootstrap_requirements_lock()),
            "receipt should be migrated to the new-format sha"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn requirements_are_stale_flags_unknown_sha() {
        let (root, runtime, manager) = seed_test_runtime("unknown-sha");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.2.45",
                "artifact": { "requirementsLockSha256": "deadbeef".repeat(8) },
            }))
            .unwrap(),
        )
        .expect("receipt");

        assert!(
            manager.requirements_are_stale(),
            "unknown sha must force a reinstall"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_in_place_skips_lock_snapshot_when_sha_matches() {
        let (root, runtime, manager) = seed_test_runtime("in-place-current");
        let current_sha = requirements_lock_sha(super::bootstrap_requirements_lock());
        // Receipt must be ≥ ATOMIC_REBUILD_FLOOR_VERSION or `prepare_in_place_upgrade`
        // forces an atomic rebuild before reaching the lock-snapshot logic.
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.20.0",
                "artifact": { "requirementsLockSha256": current_sha },
            }))
            .unwrap(),
        )
        .expect("receipt");

        let ctx = manager
            .prepare_in_place_upgrade()
            .expect("eligible for in-place");
        assert_eq!(ctx.previous_version, "0.20.0");
        assert!(
            ctx.previous_lock_backup.is_none(),
            "lock unchanged => no snapshot"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_in_place_skips_lock_snapshot_when_stored_sha_is_legacy() {
        if super::LEGACY_REQUIREMENTS_LOCK_SHAS.is_empty() {
            return;
        }
        let (root, runtime, manager) = seed_test_runtime("in-place-legacy");
        let legacy_sha = super::LEGACY_REQUIREMENTS_LOCK_SHAS[0];
        // Receipt must be ≥ ATOMIC_REBUILD_FLOOR_VERSION; this test exercises
        // the legacy-sha path, not the version-floor path.
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.20.0",
                "artifact": { "requirementsLockSha256": legacy_sha },
            }))
            .unwrap(),
        )
        .expect("receipt");

        let ctx = manager
            .prepare_in_place_upgrade()
            .expect("eligible for in-place");
        assert_eq!(ctx.previous_version, "0.20.0");
        assert!(ctx.previous_lock_backup.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_in_place_snapshots_lock_when_pins_differ() {
        let (root, runtime, manager) = seed_test_runtime("in-place-lock-churn");
        // Receipt must be ≥ ATOMIC_REBUILD_FLOOR_VERSION; this test is about
        // the lock-snapshot path, not the version-floor path (covered by
        // `receipt_requires_atomic_rebuild_below_floor`).
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.20.0",
                "artifact": { "requirementsLockSha256": "deadbeef".repeat(8) },
            }))
            .unwrap(),
        )
        .expect("receipt");
        fs::write(manager.active_lock_path(), b"old-lock-content==1.0\n")
            .expect("seed active lock");

        let ctx = manager
            .prepare_in_place_upgrade()
            .expect("eligible for in-place");
        assert_eq!(ctx.previous_version, "0.20.0");
        let backup = ctx
            .previous_lock_backup
            .as_ref()
            .expect("lock changed => snapshot taken");
        assert_eq!(
            fs::read(backup).expect("backup readable"),
            b"old-lock-content==1.0\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_in_place_falls_back_to_atomic_when_lock_missing() {
        // Lock pins differ AND the active lock is missing on disk => caller
        // should fall through to the full atomic rebuild so rollback stays
        // safe. Receipt must be ≥ ATOMIC_REBUILD_FLOOR_VERSION so the
        // version-floor early-return doesn't pre-empt the assertion target.
        let (root, runtime, manager) = seed_test_runtime("in-place-no-lock-on-disk");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.20.0",
                "artifact": { "requirementsLockSha256": "deadbeef".repeat(8) },
            }))
            .unwrap(),
        )
        .expect("receipt");
        // no active lock written
        assert!(manager.prepare_in_place_upgrade().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_in_place_skipped_without_installed_version() {
        let (root, runtime, manager) = seed_test_runtime("in-place-no-receipt");
        fs::remove_file(runtime.tools_dir.join("headroom.json")).expect("drop receipt");
        assert!(manager.prepare_in_place_upgrade().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_in_place_falls_back_to_atomic_when_receipt_predates_floor() {
        // 0.8.2 (shipped in headroom-desktop 0.2.50-rc.1 and the fallback
        // version on every Sentry boot-validation stall observed for 0.3.6)
        // is below the 0.10.0 floor. Force the rebuild even when the lock
        // snapshot is takeable.
        let (root, runtime, manager) = seed_test_runtime("in-place-pre-floor");
        let current_sha = requirements_lock_sha(super::bootstrap_requirements_lock());
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": "0.8.2",
                "artifact": { "requirementsLockSha256": current_sha },
            }))
            .unwrap(),
        )
        .expect("receipt");
        fs::write(manager.active_lock_path(), b"old-lock-content==1.0\n")
            .expect("seed active lock");
        assert!(
            manager.prepare_in_place_upgrade().is_none(),
            "0.8.2 receipt must force atomic rebuild even when lock snapshot is takeable"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Points $HOME and $CODEX_HOME at the test root for the test's lifetime.
    /// repair/bootstrap paths reach install_headroom_mcp, which writes MCP
    /// registrations into ~/.claude.json and ~/.codex/config.toml — without
    /// this guard a test run corrupts the developer's real agent configs with
    /// a temp-dir entrypoint that macOS later deletes.
    struct HomeGuard {
        prev_home: Option<std::ffi::OsString>,
        prev_codex: Option<std::ffi::OsString>,
        _env_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new(root: &Path) -> Self {
            let env_lock = crate::test_env_lock::lock_home();
            let prev_home = std::env::var_os("HOME");
            let prev_codex = std::env::var_os("CODEX_HOME");
            std::env::set_var("HOME", root);
            std::env::remove_var("CODEX_HOME");
            HomeGuard {
                prev_home,
                prev_codex,
                _env_lock: env_lock,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_codex.take() {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }

    /// Snapshots and clears every var `hf_hub_cache_dir` reads, so the
    /// precedence test is hermetic on a dev machine that has any of them set.
    struct HfEnvGuard {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    const HF_CACHE_VARS: [&str; 4] = [
        "HF_HUB_CACHE",
        "HUGGINGFACE_HUB_CACHE",
        "HF_HOME",
        "XDG_CACHE_HOME",
    ];

    impl HfEnvGuard {
        fn new() -> Self {
            let prev = HF_CACHE_VARS
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            for key in HF_CACHE_VARS {
                std::env::remove_var(key);
            }
            HfEnvGuard { prev }
        }
    }

    impl Drop for HfEnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.prev.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn hf_hub_cache_dir_follows_huggingface_precedence() {
        let root = std::env::temp_dir().join("headroom-hf-precedence");
        let _home = HomeGuard::new(&root);
        let _hf = HfEnvGuard::new();

        // Default: ${XDG_CACHE_HOME:-$HOME/.cache}/huggingface/hub
        assert_eq!(
            super::hf_hub_cache_dir(),
            Some(root.join(".cache").join("huggingface").join("hub"))
        );

        // An empty value is treated as unset rather than resolving to a
        // relative path.
        std::env::set_var("HF_HUB_CACHE", "");
        assert_eq!(
            super::hf_hub_cache_dir(),
            Some(root.join(".cache").join("huggingface").join("hub"))
        );
        std::env::remove_var("HF_HUB_CACHE");

        std::env::set_var("XDG_CACHE_HOME", "/x/xdg");
        assert_eq!(
            super::hf_hub_cache_dir(),
            Some(PathBuf::from("/x/xdg/huggingface/hub"))
        );

        // $HF_HOME/hub beats XDG_CACHE_HOME.
        std::env::set_var("HF_HOME", "/x/hfhome");
        assert_eq!(
            super::hf_hub_cache_dir(),
            Some(PathBuf::from("/x/hfhome/hub"))
        );

        // The legacy var beats HF_HOME and is itself the full hub path, no
        // `hub` suffix appended.
        std::env::set_var("HUGGINGFACE_HUB_CACHE", "/x/legacy");
        assert_eq!(super::hf_hub_cache_dir(), Some(PathBuf::from("/x/legacy")));

        // HF_HUB_CACHE wins outright.
        std::env::set_var("HF_HUB_CACHE", "/x/win");
        assert_eq!(super::hf_hub_cache_dir(), Some(PathBuf::from("/x/win")));
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn repair_stale_requirements_updates_receipt_and_emits_progress() {
        let (root, runtime, manager) = seed_test_runtime("repair-requirements");
        let _home = HomeGuard::new(&root);
        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 0\n");
        write_executable(&manager.headroom_entrypoint(), "#!/bin/sh\nexit 0\n");
        fs::write(
            runtime.tools_dir.join("headroom.json"),
            br#"{
                "version":"0.8.2",
                "artifact":{"requirementsLockSha256":"stale"},
                "mcp":{"configured":false}
            }"#,
        )
        .expect("seed receipt");

        let mut steps = Vec::new();
        manager
            .repair_stale_requirements_with_progress(|step| steps.push(step.step.to_string()))
            .expect("repair succeeds");

        assert!(steps.iter().any(|step| step == "Repairing dependencies"));
        assert!(steps.iter().any(|step| step == "Configuring integrations"));
        assert!(steps.iter().any(|step| step == "Repair complete"));

        let receipt = fs::read(runtime.tools_dir.join("headroom.json")).expect("receipt");
        let receipt: serde_json::Value = serde_json::from_slice(&receipt).expect("receipt json");
        assert_eq!(
            receipt["artifact"]["requirementsLockSha256"],
            requirements_lock_sha(super::bootstrap_requirements_lock())
        );
        assert_eq!(receipt["mcp"]["configured"], true);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn smoke_test_headroom_succeeds_with_executable_python() {
        let (root, runtime, manager) = seed_test_runtime("smoke-ok");
        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 0\n");

        manager
            .smoke_test_headroom_with_timeout(Duration::from_secs(2))
            .expect("smoke test succeeds");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn smoke_test_headroom_returns_command_failure_output_on_nonzero_exit() {
        let (root, runtime, manager) = seed_test_runtime("smoke-fail");
        write_executable(
            &runtime.managed_python(),
            "#!/bin/sh\necho failure-stdout\necho failure-stderr >&2\nexit 7\n",
        );

        let err = manager
            .smoke_test_headroom_with_timeout(Duration::from_secs(2))
            .expect_err("smoke test should fail");
        let failure = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<CommandFailure>())
            .expect("command failure");
        assert_eq!(failure.exit_code, Some(7));
        assert!(failure.stdout.contains("failure-stdout"));
        assert!(failure.stderr.contains("failure-stderr"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn smoke_test_markitdown_is_noop_when_not_installed() {
        let (root, _runtime, manager) = seed_test_runtime("markitdown-smoke-absent");
        manager
            .smoke_test_markitdown_with_timeout(Duration::from_secs(2))
            .expect("no-op when markitdown is absent");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn smoke_test_plugin_is_noop_when_not_installed() {
        let (root, _runtime, manager) = seed_test_runtime("plugin-smoke-absent");
        for plugin in &PLUGIN_ADDONS {
            manager
                .smoke_test_plugin(plugin.id)
                .expect("no-op when plugin receipt is absent");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[serial_test::serial]
    fn smoke_test_plugin_skips_disabled_and_self_heals_stale_receipt() {
        let (root, runtime, manager) = seed_test_runtime("plugin-smoke-receipt");
        // Host registries resolve under $HOME; point it at the empty test
        // root so no plugin reads as registered.
        let _home = HomeGuard::new(&root);
        for plugin in &PLUGIN_ADDONS {
            let receipt = runtime.tools_dir.join(format!("{}.json", plugin.id));

            // Disabled by the user: hosts without a disable verb (Codex) hold
            // no registration, so absence is not a failure (RUST-22).
            fs::write(&receipt, br#"{"version":"latest","enabled":false}"#).expect("receipt");
            manager
                .smoke_test_plugin(plugin.id)
                .expect("disabled plugin is not a smoke failure");
            assert!(receipt.exists(), "disabled receipt must be kept");

            // Enabled but deregistered behind our back: warn once, then
            // self-heal by dropping the stale receipt.
            fs::write(&receipt, br#"{"version":"latest","enabled":true}"#).expect("receipt");
            let err = manager
                .smoke_test_plugin(plugin.id)
                .expect_err("enabled but unregistered must fail");
            assert!(err.to_string().contains("no longer registered"));
            assert!(!receipt.exists(), "stale receipt must be removed");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_disabled_receipt_reports_installed_not_missing() {
        // A receipt with enabled:false means the user disabled it via the app.
        // On hosts without a disable verb the plugin is gone, but the card must
        // still show "installed" (Enable), not "not installed" (Install).
        let (root, runtime, manager) = seed_test_runtime("plugin-disabled");
        for plugin in &PLUGIN_ADDONS {
            fs::write(
                runtime.tools_dir.join(format!("{}.json", plugin.id)),
                br#"{"version":"latest","enabled":false}"#,
            )
            .expect("receipt");
            assert!(matches!(
                manager.detect_status(plugin.id),
                crate::models::ToolStatus::Healthy
            ));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_plugin_is_noop_without_receipt() {
        // Cleanup must not touch plugin/marketplace config Headroom never wrote.
        let (root, _runtime, manager) = seed_test_runtime("plugin-uninstall-noreceipt");
        for plugin in &PLUGIN_ADDONS {
            manager
                .uninstall_plugin(plugin.id)
                .expect("no-op when plugin receipt is absent");
        }
        let _ = fs::remove_dir_all(root);
    }

    /// End-to-end round trip against the real `claude`/`codex` plugin CLIs:
    /// install, confirm both presence checks + smoke test flip on, then
    /// uninstall and confirm they flip off. Ignored by default — it needs at
    /// least one CLI on PATH plus network, and mutates the real ~/.claude and
    /// ~/.codex plugin config. Run locally:
    /// `cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored ponytail_install_roundtrip`
    #[test]
    #[ignore]
    fn ponytail_install_roundtrip() {
        let (root, _runtime, manager) = seed_test_runtime("ponytail-roundtrip");

        if crate::claude_cli::detect_claude_cli().is_none()
            && crate::claude_cli::detect_codex_cli().is_none()
        {
            eprintln!("skipping ponytail_install_roundtrip: no claude/codex CLI on PATH");
            let _ = fs::remove_dir_all(&root);
            return;
        }

        // Capture every result and always run uninstall before asserting, so a
        // failed assertion never leaves the plugin behind on the real machine.
        let install = manager.install_plugin("ponytail");
        let installed = manager.plugin_installed("ponytail");
        let smoke_while_installed = manager.smoke_test_plugin("ponytail");
        let uninstall = manager.uninstall_plugin("ponytail");
        let gone = !manager.plugin_installed("ponytail");
        let _ = fs::remove_dir_all(&root);

        install.expect("install_plugin should succeed");
        assert!(installed, "plugin_installed() should be true after install");
        smoke_while_installed.expect("smoke_test_plugin should pass while installed");
        uninstall.expect("uninstall_plugin should succeed");
        assert!(gone, "plugin_installed() should be false after uninstall");
    }

    #[test]
    fn is_outdated_codex_detects_unrecognized_subcommand() {
        let outdated = anyhow::Error::new(CommandFailure {
            program: "codex".into(),
            args: vec!["plugin".into(), "add".into()],
            stdout: String::new(),
            stderr: "error: unrecognized subcommand 'add'\n".into(),
            exit_code: Some(2),
            signal: None,
        });
        assert!(is_outdated_codex(&outdated));

        let other = anyhow::Error::new(CommandFailure {
            program: "codex".into(),
            args: vec!["plugin".into(), "add".into()],
            stdout: String::new(),
            stderr: "error: network unreachable\n".into(),
            exit_code: Some(1),
            signal: None,
        });
        assert!(!is_outdated_codex(&other));

        // A non-CommandFailure error must not be misclassified.
        assert!(!is_outdated_codex(&anyhow::anyhow!(
            "unrecognized subcommand"
        )));
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn smoke_test_markitdown_succeeds_when_entrypoint_runs() {
        let (root, runtime, manager) = seed_test_runtime("markitdown-smoke-ok");
        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"0.1.6","enabled":true}"#,
        )
        .expect("receipt");
        write_executable(&manager.markitdown_entrypoint(), "#!/bin/sh\nexit 0\n");

        manager
            .smoke_test_markitdown_with_timeout(Duration::from_secs(2))
            .expect("smoke test succeeds");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn smoke_test_markitdown_fails_on_nonzero_exit() {
        let (root, runtime, manager) = seed_test_runtime("markitdown-smoke-fail");
        fs::write(
            runtime.tools_dir.join("markitdown.json"),
            br#"{"version":"0.1.6","enabled":true}"#,
        )
        .expect("receipt");
        write_executable(&manager.markitdown_entrypoint(), "#!/bin/sh\nexit 3\n");

        manager
            .smoke_test_markitdown_with_timeout(Duration::from_secs(2))
            .expect_err("smoke test should fail");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn smoke_test_headroom_repairs_pydantic_core_skew_and_retries() {
        let (root, runtime, manager) = seed_test_runtime("smoke-pydantic-skew");
        let state_file = root.join("smoke-attempts");
        let pip_log = root.join("pip-args");
        let script = format!(
            r#"#!/bin/sh
case "$1" in
  -c)
    if [ -f '{state}' ]; then
      exit 0
    fi
    touch '{state}'
    cat >&2 <<'EOF'
Traceback (most recent call last):
  File "<string>", line 1, in <module>
SystemError: The installed pydantic-core version (2.41.5) is incompatible with the current pydantic version, which requires 2.46.3. If you encounter this error, make sure that you haven't upgraded pydantic-core manually.
EOF
    exit 1
    ;;
  -m)
    echo "$@" >> '{pip_log}'
    exit 0
    ;;
esac
exit 0
"#,
            state = state_file.display(),
            pip_log = pip_log.display(),
        );
        write_executable(&runtime.managed_python(), &script);

        manager
            .smoke_test_headroom()
            .expect("repair should let smoke retry succeed");

        let pip_args = fs::read_to_string(&pip_log).expect("pip log written");
        assert!(
            pip_args.contains("pydantic-core==2.46.3"),
            "expected repair to install pydantic-core==2.46.3, got: {pip_args}"
        );
        // pydantic itself must also be force-reinstalled to collapse any
        // duplicate dist-info dirs that cause the flip-flop skew.
        let pydantic_invocations = pip_args
            .lines()
            .filter(|line| {
                line.contains("--force-reinstall")
                    && line.split_whitespace().any(|tok| tok == "pydantic")
            })
            .count();
        assert_eq!(
            pydantic_invocations, 1,
            "expected exactly one force-reinstall of pydantic, got: {pip_args}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn smoke_test_headroom_does_not_repair_unrelated_failures() {
        let (root, runtime, manager) = seed_test_runtime("smoke-unrelated-fail");
        let state_file = root.join("attempts");
        let script = format!(
            "#!/bin/sh\necho >> '{state}'\necho boom >&2\nexit 1\n",
            state = state_file.display(),
        );
        write_executable(&runtime.managed_python(), &script);

        let err = manager
            .smoke_test_headroom()
            .expect_err("smoke should fail without retry");
        let failure = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<CommandFailure>())
            .expect("command failure");
        assert_eq!(failure.exit_code, Some(1));

        let attempts = fs::read_to_string(&state_file).expect("attempts log");
        assert_eq!(
            attempts.lines().count(),
            1,
            "non-skew failures should not retry"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)] // exercises a fake shell-script binary; Windows cannot exec it
    fn smoke_test_headroom_times_out() {
        let (root, runtime, manager) = seed_test_runtime("smoke-timeout");
        write_executable(&runtime.managed_python(), "#!/bin/sh\nsleep 1\n");

        let err = manager
            .smoke_test_headroom_with_timeout(Duration::from_millis(100))
            .expect_err("smoke test should time out");
        let failure = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<CommandFailure>())
            .expect("command failure");
        assert_eq!(failure.exit_code, None);
        assert!(failure.stderr.contains("command timed out after 100ms"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_major_minor_patch_handles_clean_and_pre_release() {
        assert_eq!(parse_major_minor_patch("0.19.0"), Some((0, 19, 0)));
        assert_eq!(parse_major_minor_patch("1.2.3"), Some((1, 2, 3)));
        // Patch defaults to 0.
        assert_eq!(parse_major_minor_patch("0.19"), Some((0, 19, 0)));
        // Pre-release / build suffixes are stripped.
        assert_eq!(parse_major_minor_patch("0.19.0-rc.1"), Some((0, 19, 0)));
        assert_eq!(parse_major_minor_patch("0.19.0+build.5"), Some((0, 19, 0)));
        assert_eq!(parse_major_minor_patch("0.19.0.dev0"), Some((0, 19, 0)));
        // Nonsense returns None — caller treats as "rebuild" to be safe.
        assert_eq!(parse_major_minor_patch(""), None);
        assert_eq!(parse_major_minor_patch("not-a-version"), None);
        assert_eq!(parse_major_minor_patch("0"), None);
    }

    #[test]
    fn receipt_requires_atomic_rebuild_below_floor() {
        // Floor raised to 0.20.0 in 0.4.0: upstream 0.20.x switched
        // headroom-ai to a maturin/Rust-native single-wheel build (upstream
        // #355). The 0.10.x–0.19.x cohort was built against the old
        // py3-none-any wheel with no `headroom_core` `.so`; an in-place
        // upgrade onto the new native wheel would layer a fresh extension
        // on top of stale transitive native pins, which is the exact
        // segfault-on-import pattern this floor exists to prevent.
        assert_eq!(ATOMIC_REBUILD_FLOOR_VERSION, (0, 20, 0));

        // Pre-floor: every desktop shipment up to and including 0.3.x
        // (which bundled headroom-ai 0.19.0). Both the original 0.8.2
        // fallback cohort and the 0.10.x → 0.19.x cohort now fall here.
        assert!(receipt_requires_atomic_rebuild("0.5.18"));
        assert!(receipt_requires_atomic_rebuild("0.8.2"));
        assert!(receipt_requires_atomic_rebuild("0.9.7"));
        assert!(receipt_requires_atomic_rebuild("0.10.4"));
        assert!(receipt_requires_atomic_rebuild("0.10.12"));
        assert!(receipt_requires_atomic_rebuild("0.19.0"));

        // At-or-above the floor: in-place is allowed (0.20.x cohort + future).
        assert!(!receipt_requires_atomic_rebuild("0.20.0"));
        assert!(!receipt_requires_atomic_rebuild("0.21.39"));
        assert!(!receipt_requires_atomic_rebuild("1.0.0"));

        // Pre-release suffixes don't change the comparison.
        assert!(!receipt_requires_atomic_rebuild("0.20.0-rc.1"));
        assert!(receipt_requires_atomic_rebuild("0.19.99-rc.1"));

        // Unparseable receipts are treated as too-old (conservative).
        assert!(receipt_requires_atomic_rebuild(""));
        assert!(receipt_requires_atomic_rebuild("garbage"));
    }

    #[test]
    fn pip_output_capture_keeps_last_n_lines() {
        let mut cap = PipOutputCapture::new(3);
        cap.push("first");
        cap.push("second");
        cap.push("third");
        // Buffer is now full; the next push must evict "first".
        cap.push("fourth");
        cap.push("fifth");
        let out = cap.into_string();
        // We keep the LAST 3 lines because the tail (warnings, "Successfully
        // installed", "Skipping X") is the diagnostically interesting part.
        assert_eq!(out, "third\nfourth\nfifth");
    }

    #[test]
    fn pip_output_capture_handles_empty_and_partial_fill() {
        let cap = PipOutputCapture::new(10);
        assert_eq!(cap.into_string(), "");

        let mut cap = PipOutputCapture::new(10);
        cap.push("only line");
        assert_eq!(cap.into_string(), "only line");

        let mut cap = PipOutputCapture::new(10);
        cap.push("a");
        cap.push("b");
        assert_eq!(cap.into_string(), "a\nb");
    }

    #[test]
    fn claude_json_write_preserves_existing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude.json");
        fs::write(
            &path,
            r#"{"oauthAccount":{"id":"abc"},"projects":{"/x":{}}}"#,
        )
        .unwrap();

        super::write_headroom_to_claude_json_at(&path, Path::new("/bin/headroom"), "http://p")
            .unwrap();

        let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["oauthAccount"]["id"], "abc");
        assert!(after["projects"]["/x"].is_object());
        assert_eq!(after["mcpServers"]["headroom"]["command"], "/bin/headroom");
    }

    #[test]
    fn claude_json_write_refuses_to_clobber_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude.json");
        fs::write(&path, r#"{"oauthAccount":{"id":"ab"#).unwrap(); // truncated mid-write

        let err =
            super::write_headroom_to_claude_json_at(&path, Path::new("/bin/headroom"), "http://p")
                .unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
        // Original bytes untouched.
        assert_eq!(fs::read(&path).unwrap(), br#"{"oauthAccount":{"id":"ab"#);
    }

    #[test]
    fn claude_json_write_treats_empty_file_as_fresh_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude.json");
        fs::write(&path, "  \n").unwrap();

        super::write_headroom_to_claude_json_at(&path, Path::new("/bin/headroom"), "http://p")
            .unwrap();

        let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(after["mcpServers"]["headroom"].is_object());
        // Backup taken, no tmp file left behind.
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.contains(".headroom-backup-")));
        assert!(!names.iter().any(|n| n.ends_with(".headroom-tmp")));
    }

    fn pip_failure(stderr: &str) -> anyhow::Error {
        anyhow::Error::new(CommandFailure {
            program: "python".into(),
            args: vec!["-m".into(), "pip".into(), "install".into()],
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code: Some(1),
            signal: None,
        })
    }

    /// RUST-82: an install-blocking venv failure that reached triage with no
    /// cause in it. Two defects stacked. `python -m venv` runs ensurepip via
    /// `check_output` and re-raises only the exit status, so all Sentry ever saw
    /// was "Command '[... -m ensurepip ...]' returned non-zero exit status 1";
    /// and even once ensurepip is run directly, pip's `ERROR:` line is followed
    /// by a traceback, so the 300-byte tail kept the traceback and dropped the
    /// diagnosis. Verified against the real stderr of a failing ensurepip.
    #[test]
    #[cfg(unix)] // exercises /bin/sh; Windows cannot exec it
    fn run_command_streaming_kills_silent_child() {
        let err = super::run_command_streaming(
            std::path::Path::new("/bin/sh"),
            &["-c", "echo hi; sleep 30"],
            &std::env::temp_dir(),
            Some(Duration::from_millis(700)),
            &mut |_| {},
        )
        .expect_err("silent child must be killed");
        let failure = err
            .downcast_ref::<CommandFailure>()
            .expect("stall reports as CommandFailure");
        assert!(
            failure.stderr.contains("no output for"),
            "stderr should name the stall: {}",
            failure.stderr
        );
        assert!(
            failure.stdout.contains("hi"),
            "output before the stall is preserved"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_command_streaming_spares_slow_but_talking_child() {
        super::run_command_streaming(
            std::path::Path::new("/bin/sh"),
            &["-c", "for i in 1 2 3; do echo tick; sleep 0.3; done"],
            &std::env::temp_dir(),
            Some(Duration::from_secs(5)),
            &mut |_| {},
        )
        .expect("child that keeps talking must not be killed");
    }

    /// RUST-90: `colorama==0.4.6` -- a pure-python wheel that exists for every
    /// platform we ship -- filed under `no-matching-dist`, the bucket reserved
    /// for a bad pin in our own lock. The machine's index was unreachable; pip
    /// said so, but `compact_pip_failure` tails from the first `ERROR:` line
    /// and pip prints `Could not fetch URL` above it. Classify against pip's
    /// whole output, not the tail that survives into the Sentry message.
    #[test]
    fn a_starved_index_is_not_blamed_on_our_lock() {
        let stderr = concat!(
            "WARNING: Retrying (Retry(total=4, connect=None, read=None, redirect=None, status=None)) ",
            "after connection broken by 'SSLError(SSLCertVerificationError(1, ",
            "'[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed'))': /simple/colorama/\n",
            "WARNING: Could not fetch URL https://pypi.org/simple/colorama/: ",
            "There was a problem confirming the ssl certificate - skipping\n",
            "ERROR: Could not find a version that satisfies the requirement colorama==0.4.6 ",
            "(from versions: none)\n",
            "ERROR: No matching distribution found for colorama==0.4.6\n",
        );
        let err = pip_failure(stderr);
        let compact = compact_pip_failure(&err);

        // The evidence really is gone from the message Sentry groups on --
        // that is the whole bug, so pin it.
        assert!(
            !compact.to_ascii_lowercase().contains("could not fetch url"),
            "test no longer reproduces the truncation: {compact}"
        );
        assert_eq!(
            pip_failure_category(&compact),
            "no-matching-dist",
            "the tail alone is genuinely ambiguous; that is why evidence is needed"
        );

        let evidence = super::pip_failure_evidence(&err, &compact);
        assert_eq!(
            super::pip_failure_category_with_evidence(&compact, &evidence),
            "network",
            "a starved index must not be filed as a bad pin in our lock"
        );
    }

    /// The counter-case the two-signal rule exists for (RUST-6S): a pin our
    /// lock really did get wrong. pip reached the index and listed what it
    /// found, so nothing here may be excused as a network fault.
    #[test]
    fn a_genuinely_bad_pin_keeps_its_verdict() {
        let stderr = concat!(
            "ERROR: Could not find a version that satisfies the requirement onnxruntime==1.27.0 ",
            "(from versions: 1.13.1, 1.22.0, 1.23.2)\n",
            "ERROR: No matching distribution found for onnxruntime==1.27.0\n",
        );
        let err = pip_failure(stderr);
        let compact = compact_pip_failure(&err);
        let evidence = super::pip_failure_evidence(&err, &compact);
        assert_eq!(
            super::pip_failure_category_with_evidence(&compact, &evidence),
            "no-matching-dist"
        );
    }

    #[test]
    fn compact_pip_failure_keeps_pips_diagnosis_ahead_of_a_traceback() {
        let stderr = concat!(
            "ERROR: Could not install packages due to an OSError: ",
            "[Errno 13] Permission denied: '/opt/venv/lib/python3.14/site-packages/pip'\n",
            "Check the permissions.\n",
            "\n",
            "Traceback (most recent call last):\n",
            "  File \"<frozen runpy>\", line 203, in _run_module_as_main\n",
            "  File \"/usr/lib/python3.14/ensurepip/__init__.py\", line 88, in _run_pip\n",
            "    return subprocess.run(cmd, check=True).returncode\n",
            "  File \"/usr/lib/python3.14/subprocess.py\", line 578, in run\n",
            "    raise CalledProcessError(retcode, process.args,\n",
            "subprocess.CalledProcessError: Command '['/opt/venv/bin/python', '-W', ",
            "'ignore::DeprecationWarning', '-c', 'import runpy...']' ",
            "returned non-zero exit status 1.\n",
        );
        let compact = compact_pip_failure(&pip_failure(stderr));
        assert!(
            compact.contains("Permission denied"),
            "pip named the cause and it must survive: {compact}"
        );
        assert_eq!(
            pip_failure_category(&compact),
            "permission",
            "a named cause must not sit in the `other` grab-bag: {compact}"
        );
    }

    /// The tail is still right when pip prints no `ERROR:` line at all, so the
    /// RUST-82 fix must not cost us the ordinary case.
    #[test]
    fn compact_pip_failure_still_tails_when_pip_named_no_error() {
        let stderr = format!(
            "{}\nno matching distribution found for onnxruntime",
            "x".repeat(400)
        );
        let compact = compact_pip_failure(&pip_failure(&stderr));
        assert!(compact.ends_with("no matching distribution found for onnxruntime"));
        assert_eq!(pip_failure_category(&compact), "no-matching-dist");
    }

    #[test]
    fn compact_pip_failure_survives_multibyte_stderr() {
        // Non-English Windows locale: the 300-byte tail offset lands mid-
        // character, which used to panic on the slice.
        let stderr = "エラー: パッケージをインストールできませんでした。".repeat(20);
        let compact = compact_pip_failure(&pip_failure(&stderr));
        assert!(compact.starts_with("exit=1; stderr tail: "));
        assert!(compact.contains("エラー"));
    }

    #[test]
    fn every_index_resolving_pip_install_is_wheels_only() {
        // A dependency built from source needs a compiler the user was never
        // asked to have, and fails minutes later with a clang/cargo error they
        // cannot act on. `PIP_ONLY_BINARY` prevents that -- but only on the
        // call sites that carry it, and these arg lists are copy-pasted across
        // nine of them. Any install that resolves from an index must be
        // wheels-only; the optional markitdown/serena addons deliberately do
        // not resolve from `--extra-index-url` and so are not covered here.
        let source = include_str!("tool_manager.rs");
        let indexed = source.matches("\"--extra-index-url\",").count();
        // Split so this needle does not count itself in the scan above.
        let only_binary = source.matches(concat!("PIP_ONLY_", "BINARY,")).count();
        assert!(indexed > 0, "sanity: expected index-resolving installs");
        assert_eq!(
            indexed, only_binary,
            "an index-resolving pip install is missing PIP_ONLY_BINARY \
             ({indexed} indexed installs vs {only_binary} wheels-only)"
        );
    }

    #[test]
    fn pip_failure_category_splits_the_buckets_triage_uses() {
        // One issue per cause class, not per stderr tail (RUST-6M/6N/6P) and
        // not one flat grab-bag (RUST-5Q).
        let cases = [
            ("exit=1; stderr tail: No module named pip", "no-pip"),
            (
                "exit=1; stderr tail: FileNotFoundError: [Errno 2] No usable temporary directory",
                "no-tempdir",
            ),
            (
                "exit=1; stderr tail: OSError: [Errno 28] No space left on device",
                "disk-full",
            ),
            ("exit=1; stderr tail: Check the permissions.", "permission"),
            (
                "exit=1; stderr tail: ERROR: Could not find a version that satisfies the \
                 requirement onnxruntime==1.27.0 (from versions: 1.23.2)\nERROR: No matching \
                 distribution found for onnxruntime==1.27.0",
                "no-matching-dist",
            ),
            (
                "exit=1; stderr tail: ERROR: Could not install packages due to an OSError: \
                 [Errno 2] No such file or directory: 'C:\\\\...\\\\INSTALLERvp0i8uew.tmp'",
                "missing-file",
            ),
            (
                "exit=1; stderr tail: ERROR: Failed building wheel for hnswlib",
                "build",
            ),
            (
                "exit=1; stderr tail: Could not fetch URL https://pypi.org/simple/",
                "network",
            ),
            ("exit=1; stderr tail: something new", "other"),
            // RUST-6S third shape: venv damaged in place, launcher stub can't
            // resolve the interpreter, pip never runs.
            ("exit=106; stderr tail: No pyvenv.cfg file", "venv-broken"),
            // RUST-8K, verbatim: Windows access-denied on a Korean install.
            // Only the numeric code survives translation.
            (
                "publishing extracted python into ~\\AppData\\Local\\Headroom: \
                 액세스가 거부되었습니다. (os error 5)",
                "permission",
            ),
            // RUST-8K again, second cause under the same title: the bundled
            // interpreter's OpenSSL dies inside `ensurepip`, before pip runs.
            (
                "installing pip into the Headroom-managed virtualenv: command failed (exit 1): \
                 python.exe -m ensurepip --upgrade --default-pip\nstderr:\n\
                 OPENSSL_Uplink(00007FF926407C58,08): no OPENSSL_Applink",
                "openssl-applink",
            ),
            // RUST-8K, third cause: Windows Application Control blocked the
            // venv python before it could run. Localized prose, so the
            // numeric code carries the match.
            (
                "creating Headroom-managed virtualenv: starting python.exe -m venv: \
                 An Application Control policy has blocked this file. (os error 4551)",
                "app-control",
            ),
            // RUST-8K, fourth cause: same machine on retry -- python ran,
            // but _ssl's DLLs stayed blocked, so ensurepip dies importing
            // pip and only this ImportError survives.
            (
                "installing pip into the Headroom-managed virtualenv: command failed (exit 1): \
                 python.exe -m ensurepip --upgrade --default-pip\nstderr:\n\
                 ImportError: cannot import name 'HTTPSHandler' from 'urllib.request'",
                "ssl-missing",
            ),
            // The macOS network errnos must not read as a denial: the
            // closing paren in the needle is what keeps 51 out of "permission".
            (
                "exit=1; stderr tail: error sending request (os error 51)",
                "other",
            ),
        ];
        for (compact, expected) in cases {
            assert_eq!(pip_failure_category(compact), expected, "for: {compact}");
        }
    }

    #[test]
    fn plugin_install_failure_category_splits_the_rust_6k_grab_bag() {
        // Every string below is a real RUST-6K event body. They arrived under ONE
        // fingerprint, which is why that issue could never be resolved: a resolve
        // regressed on the next sibling shape. One bucket per cause class.
        let cases = [
            (
                "Codex: command failed (exit 1): /opt/homebrew/bin/codex plugin add \
                 ponytail@ponytail\nstdout:\n\nstderr:\nError: plugin `ponytail` was not found \
                 in marketplace `ponytail`",
                "marketplace-missing",
            ),
            (
                "Codex: command failed (exit 1): /opt/homebrew/bin/codex plugin add \
                 ponytail@ponytail\nstdout:\n\nstderr:\nError: failed to load configuration\n\
                 Caused by:\n    0: ~/.codex/config.toml:646:18: duplicate key",
                "host-config-invalid",
            ),
            ("Codex: CLI not found on PATH", "cli-missing"),
            (
                "Claude Code: command failed (exit 1): ~/.local/bin/claude plugin install \
                 caveman@caveman --scope user\nstdout:\n\nstderr:\nerror: unknown option '--scope'",
                "cli-version-skew",
            ),
            (
                "Codex: command failed (exit 1): codex plugin add caveman@caveman\nstderr:\n\
                 Error: failed to load configuration\n\nCaused by:\n    0: \
                 ~/.codex/config.toml:209:12: `wire_api = \"chat\"` is no longer supported.",
                "cli-version-skew",
            ),
            ("Codex: something we have not seen", "other"),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (detail, expected) in cases {
            assert_eq!(
                plugin_install_failure_category(detail),
                expected,
                "for: {detail}"
            );
            seen.insert(expected);
        }
        assert!(
            seen.len() >= 5,
            "the five RUST-6K shapes must land in distinct buckets, got: {seen:?}"
        );
    }

    #[test]
    fn retry_fs_survives_a_transient_denial() {
        // RUST-8K: Windows kept the old python dir alive after remove_dir_all
        // and denied the rename onto it. The op succeeds once the handle
        // closes, so the bootstrap must not dead-end on the first refusal.
        let attempts = Cell::new(0u32);
        let result = ToolManager::retry_fs("unit", || {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                Ok(attempts.get())
            }
        });
        assert_eq!(result.expect("third attempt succeeds"), 3);
        assert_eq!(
            attempts.get(),
            3,
            "must retry, not give up on the first Err"
        );
    }

    #[test]
    fn retry_fs_gives_up_and_returns_the_last_error() {
        // The bound matters as much as the retry: an unbounded loop would hang
        // the bootstrap on a genuinely broken install instead of reporting it.
        let attempts = Cell::new(0u32);
        let result = ToolManager::retry_fs("unit", || {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert_eq!(
            result.expect_err("exhausted").kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(attempts.get(), 5, "bounded at ATTEMPTS");
    }
}
