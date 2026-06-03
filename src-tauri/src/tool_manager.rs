use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
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
/// Starting with 0.20.x upstream switched to a maturin/Rust-native single-wheel
/// build (upstream #355), so the wheel URL is per-Python-version and per-platform.
/// The pin below is for cp312 + macOS arm64 — the only target our release
/// workflow builds today (release-macos.yml). If `PYTHON_STANDALONE_RELEASE`
/// ever moves to a different cpython major (3.13+), re-pick the matching wheel
/// from https://pypi.org/pypi/headroom-ai/<version>/json. If Linux is ever
/// added to the release matrix, a per-platform wheel-picker (mirroring
/// `python_distribution_artifact`) is required.
pub(crate) const HEADROOM_PINNED_VERSION: &str = "0.21.39";
const HEADROOM_PINNED_WHEEL_URL: &str = "https://files.pythonhosted.org/packages/cd/fe/9874c65b5f66f1dbc8cdf5ca0977e3f25e6e3ccbee091bd0849dad6bd041/headroom_ai-0.21.39-cp312-cp312-macosx_11_0_arm64.whl";
const HEADROOM_PINNED_SHA256: &str =
    "1b55875f63e27f059f65365a927d388ff6f56dd32f471d9f1b328011098f2aea";
const HEADROOM_SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Index of pre-built wheels for sdist-only PyPI packages (e.g. hnswlib).
/// GitHub's expanded_assets endpoint serves HTML anchors pip can consume via --find-links.
const VENDOR_WHEELS_INDEX_URL: &str =
    "https://github.com/gglucass/headroom-desktop/releases/expanded_assets/vendor-wheels-v1";
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
    // 0.21.39 freeze diverges from every prior shipment. Upstream 0.20.x moved
    // headroom-ai to a maturin/Rust-native wheel (upstream #355) and reshaped
    // its transitive dep set as parts of the old Python implementation moved
    // into the Rust `headroom_core` crate. There is no clean lockfile overlap
    // with the 0.19.0 freeze, so users on any older receipt must do a real
    // reinstall (and the 0.10.x–0.19.x cohort is forced down the atomic
    // rebuild path by ATOMIC_REBUILD_FLOOR_VERSION below). The legacy
    // migration list stays empty until the next no-op cosmetic lock change.
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
const ATOMIC_REBUILD_FLOOR_VERSION: (u32, u32, u32) = (0, 20, 0);

/// Parse the leading `major.minor.patch` from a version string, tolerating
/// pre-release/build suffixes (`-rc.1`, `+build`, `.dev0`, etc.). Returns
/// None when the prefix isn't a numeric `major.minor`. `patch` defaults to
/// 0 when missing or unparseable, so `"0.19"` and `"0.19.0"` compare equal.
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
const RTK_VERSION: &str = "0.42.0";
const RTK_SHA256_MACOS_AARCH64: &str =
    "cddc9cd11cdf80b3342eebaba0e6ab26d9c8dec45295ea44cf98062987185724";
const RTK_SHA256_MACOS_X86_64: &str =
    "3b1b9f13548599ae9d920f5e3169cc402db1930044ea24e0be4e236b7f072a99";
const RTK_SHA256_LINUX_AARCH64: &str =
    "62bb749df1ed64f09149998c31de864932f047a1be4e0f882a8ceada849e0871";
const RTK_SHA256_LINUX_X86_64: &str =
    "cdd4f87ac97ce958f71b53a991880d6adcc41cc5bca1044175a64630980152be";
const PYTHON_STANDALONE_RELEASE: &str = "20251014";
const PYTHON_SHA256_MACOS_AARCH64: &str =
    "84cb7acbf75264982c8bdd818bfa1ff0f1eb76007b48a5f3e01d28633b46afdf";
const PYTHON_SHA256_MACOS_X86_64: &str =
    "f76a921e71e9c8954cccd00f176b7083041527b3b4223670d05bbb2f51209d3f";
const PYTHON_SHA256_LINUX_X86_64: &str =
    "c74addcd1b033a6e4d60ead3ab47fcc995569027e01d3061c4a934f363c4a0cf";
const PYTHON_SHA256_LINUX_AARCH64: &str =
    "d2a6c0d4ceea088f635b309a59d5d700a256656423225f96ddfb71d532adb1aa";

#[derive(Debug, Clone)]
pub struct BootstrapStepUpdate {
    pub step: &'static str,
    pub message: String,
    pub eta_seconds: u64,
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
        self.python_dir.join("bin").join("python3")
    }

    pub fn managed_python(&self) -> PathBuf {
        self.venv_dir.join("bin").join("python3")
    }

    pub fn managed_pip(&self) -> PathBuf {
        self.venv_dir.join("bin").join("pip")
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
}

#[derive(Debug, Clone)]
struct ToolLogMarkerCache {
    tool_id: String,
    path: PathBuf,
    modified: std::time::SystemTime,
    result: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct RtkDailyGainOutput {
    #[serde(default)]
    summary: Option<RtkGainSummary>,
    #[serde(default)]
    daily: Vec<RtkDailyEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RtkGainSummary {
    pub total_commands: u64,
    pub total_saved: u64,
    pub avg_savings_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct RtkDailyEntry {
    date: String,
    #[serde(default)]
    commands: u64,
    #[serde(default)]
    saved_tokens: u64,
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
                    "Token-optimized shell command proxy for Claude Code and your terminal.".into(),
                runtime: "binary".into(),
                source_url: "https://github.com/rtk-ai/rtk".into(),
                version: RTK_VERSION.into(),
                checksum: rtk_checksum,
                required: true,
            },
        ];

        Self {
            runtime,
            manifests,
            log_marker_cache: Arc::new(Mutex::new(None)),
        }
    }

    pub fn list_tools(&self) -> Vec<ManagedTool> {
        self.manifests
            .iter()
            .map(|manifest| ManagedTool {
                id: manifest.id.clone(),
                name: manifest.name.clone(),
                description: manifest.description.clone(),
                runtime: manifest.runtime.clone(),
                required: manifest.required,
                enabled: true,
                status: self.detect_status(&manifest.id),
                source_url: manifest.source_url.clone(),
                version: if manifest.id == "headroom" {
                    self.installed_headroom_version()
                        .unwrap_or_else(|| manifest.version.clone())
                } else {
                    manifest.version.clone()
                },
                checksum: manifest.checksum.clone(),
            })
            .collect()
    }

    pub fn python_runtime_installed(&self) -> bool {
        self.runtime.ready_flag().exists() && self.runtime.managed_python().exists()
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.runtime.logs_dir()
    }

    pub fn headroom_entrypoint(&self) -> PathBuf {
        self.runtime.venv_dir.join("bin").join("headroom")
    }

    pub fn managed_python(&self) -> PathBuf {
        self.runtime.managed_python()
    }

    pub fn rtk_entrypoint(&self) -> PathBuf {
        find_rtk_on_path().unwrap_or_else(|| self.runtime.bin_dir.join("rtk"))
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

    pub fn start_headroom_background(&self) -> Result<Child> {
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
            //   * HeadroomRunning → bail (a previous headroom proxy is alive;
            //     we want a fresh one).
            //   * ForeignOccupant → try to fall back to a port in
            //     6769..=6790. Only bail if every fallback is also taken.
            // The chosen port is stored in `backend_port` so the intercept,
            // health probes, and spawn args all pick it up.
            let initial_state = diagnose_proxy_port(backend_port::DEFAULT_BACKEND_PORT);
            match initial_state {
                PortState::Free => {
                    backend_port::set(backend_port::DEFAULT_BACKEND_PORT);
                }
                PortState::HeadroomRunning => {
                    bail!(
                        "{}",
                        format_already_running_bail(backend_port::DEFAULT_BACKEND_PORT)
                    );
                }
                PortState::ForeignOccupant(detail) => {
                    let pid = parse_pid_from_lsof_detail(&detail);
                    let try_bind = |port: u16| {
                        TcpListener::bind(("127.0.0.1", port)).is_ok()
                    };
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
                    (entrypoint, headroom_entrypoint_startup_args()),
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
                let log_file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .with_context(|| format!("opening {}", log_path.display()))?;

                // Wrap with `nice` so headroom yields CPU to foreground apps
                // (Claude Code, terminal, etc.) when the machine is contended.
                // On idle systems headroom still runs at full speed.
                let mut child = Command::new("/usr/bin/nice")
                    .arg("-n")
                    .arg("5")
                    .arg(executable)
                    .args(args)
                    .current_dir(&self.runtime.root_dir)
                    .process_group(0)
                    .env("PYTHONNOUSERSITE", "1")
                    .env("PYTHONUNBUFFERED", "1")
                    .env("PYTHONFAULTHANDLER", "1")
                    .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
                    .env("PIP_NO_INPUT", "1")
                    .env("HEADROOM_SDK", "headroom-desktop-proxy")
                    .env("HEADROOM_HTTP2", "false")
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(
                        log_file
                            .try_clone()
                            .with_context(|| format!("cloning {}", log_path.display()))?,
                    ))
                    .stderr(Stdio::from(log_file))
                    .spawn()
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
                                status, headroom_proxy_port()
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
                    let _ = Command::new("/bin/kill")
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
                    log_tail: tail_log_file(&log_path, 80),
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

            let last = failures
                .pop()
                .expect("at least one startup variant attempted");
            let prior_summary = if failures.is_empty() {
                String::new()
            } else {
                let joined = failures
                    .iter()
                    .map(|f| format!("{} {} {}", f.program, f.args.join(" "), f.reason))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!(" (prior attempts: {})", joined)
            };
            return Err(anyhow::Error::from(last).context(format!(
                "unable to keep headroom running in background{}",
                prior_summary
            )));
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

        let output = Command::new(self.rtk_entrypoint())
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
        enabled_marker: &str,
        disabled_markers: &[&str],
    ) -> Option<bool> {
        let path = self.latest_tool_log_path(tool_id)?;
        self.scan_file_for_marker_state_cached(tool_id, &path, enabled_marker, disabled_markers)
    }

    fn scan_file_for_marker_state_cached(
        &self,
        cache_key: &str,
        path: &Path,
        enabled_marker: &str,
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
            if lowered.contains(enabled_marker) {
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
        if let Some(path) = headroom_propagated_proxy_log_path() {
            if let Some(state) = self.scan_file_for_marker_state_cached(
                "headroom-proxy-log",
                &path,
                "kompress: enabled",
                &["kompress: not installed", "kompress: disabled"],
            ) {
                return Some(state);
            }
        }

        self.latest_tool_log_marker_state(
            "headroom",
            "kompress: enabled",
            &["kompress: not installed", "kompress: disabled"],
        )
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
        self.rtk_entrypoint().exists()
            && (find_rtk_on_path().is_some() || self.runtime.tools_dir.join("rtk.json").exists())
    }

    pub fn installed_rtk_version(&self) -> Option<String> {
        if let Some(r) = self.read_rtk_receipt() {
            if let Some(v) = r.get("version").and_then(|v| v.as_str()) {
                return Some(v.to_string());
            }
        }
        if let Some(rtk_path) = find_rtk_on_path() {
            if let Ok(output) = Command::new(rtk_path).arg("--version").output() {
                if output.status.success() {
                    let out_str = String::from_utf8_lossy(&output.stdout);
                    let version = out_str.trim().trim_start_matches("rtk").trim().to_string();
                    if !version.is_empty() {
                        return Some(version);
                    }
                }
            }
        }
        None
    }

    pub fn rtk_needs_install(&self) -> bool {
        if find_rtk_on_path().is_some() {
            return false;
        }
        !self.rtk_entrypoint().exists()
            || self.installed_rtk_version().as_deref() != Some(RTK_VERSION)
    }

    /// Reinstall rtk if the on-disk version doesn't match the pinned version.
    /// Returns Ok(true) if work was done, Ok(false) if already current.
    pub fn ensure_rtk_current(&self) -> Result<bool> {
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
        let output = Command::new(self.rtk_entrypoint())
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
        Some(HeadroomRelease {
            version: HEADROOM_PINNED_VERSION.into(),
            wheel_url: HEADROOM_PINNED_WHEEL_URL.into(),
            sha256: HEADROOM_PINNED_SHA256.into(),
        })
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
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)
            .with_context(|| format!("writing {}", receipt_path.display()))?;
        Ok(())
    }

    pub fn repair_stale_requirements_with_progress<F>(&self, mut progress: F) -> Result<()>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        let requirements_lock = bootstrap_requirements_lock();
        let lock_path = self.write_headroom_requirements_lock(requirements_lock)?;

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
                if let Some(update) =
                    pip_line_to_progress(line, deps_start.elapsed(), &mut dep_counter, 40, 82)
                {
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

    pub fn bootstrap_all(&self) -> Result<ManagedRuntime> {
        self.bootstrap_all_with_progress(|_| {})
    }

    pub fn bootstrap_all_with_progress<F>(&self, mut progress: F) -> Result<ManagedRuntime>
    where
        F: FnMut(BootstrapStepUpdate),
    {
        progress(BootstrapStepUpdate {
            step: "Preparing install",
            message: "Setting up managed directories.".into(),
            eta_seconds: 3,
            percent: 5,
        });
        self.runtime.ensure_layout()?;

        if !self.runtime.standalone_python().exists() {
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

        if !self.runtime.managed_python().exists() {
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

        progress(BootstrapStepUpdate {
            step: "Installing Headroom",
            message: "Installing Headroom and required dependencies.".into(),
            eta_seconds: 95,
            percent: 58,
        });
        self.install_headroom()?;

        progress(BootstrapStepUpdate {
            step: "Installing RTK",
            message: "Installing RTK for shell commands and Claude Code auto-rewrite.".into(),
            eta_seconds: 15,
            percent: 79,
        });
        self.install_rtk()?;

        progress(BootstrapStepUpdate {
            step: "Finalizing",
            message: "Writing managed runtime receipts and completion markers.".into(),
            eta_seconds: 6,
            percent: 90,
        });
        self.write_ready_flag()?;
        self.write_bootstrap_receipt()?;
        progress(BootstrapStepUpdate {
            step: "Install complete",
            message: "Headroom runtime installed successfully.".into(),
            eta_seconds: 0,
            percent: 100,
        });
        Ok(self.runtime.clone())
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
        archive
            .unpack(&self.runtime.runtime_dir)
            .with_context(|| format!("extracting into {}", self.runtime.runtime_dir.display()))?;

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
            let _ = std::process::Command::new("xattr")
                .args([
                    "-rd",
                    "com.apple.quarantine",
                    self.runtime.runtime_dir.to_string_lossy().as_ref(),
                ])
                .output();
        }

        Ok(())
    }

    fn create_managed_venv(&self) -> Result<()> {
        run_python_command(
            &self.runtime.standalone_python(),
            &[
                "-m",
                "venv",
                self.runtime.venv_dir.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
        )
        .context("creating Headroom-managed virtualenv")?;

        run_python_command(
            &self.runtime.managed_python(),
            &["-m", "pip", "--version"],
            &self.runtime.root_dir,
        )
        .context("verifying Headroom-managed pip is available")?;

        Ok(())
    }

    /// Bootstrap path: installs the pinned headroom release.
    fn install_headroom(&self) -> Result<()> {
        // Bootstrap path runs at first launch where there is no boot
        // validation yet — no caller will read the captured pip output, so
        // skip the buffer to avoid allocating it.
        self.install_headroom_release(
            &HeadroomRelease {
                version: HEADROOM_PINNED_VERSION.into(),
                wheel_url: HEADROOM_PINNED_WHEEL_URL.into(),
                sha256: HEADROOM_PINNED_SHA256.into(),
            },
            |_| {},
            None,
        )
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
        let wheel_path = self
            .runtime
            .downloads_dir
            .join(format!("headroom_ai-{}-py3-none-any.whl", release.version));

        progress(BootstrapStepUpdate {
            step: "Downloading update",
            message: "Fetching Headroom update bundle.".into(),
            eta_seconds: 15,
            percent: 40,
        });

        // Try direct wheel download (with retries). If it fails, fall back to PyPI index.
        let use_wheel =
            match download_to_path(&release.wheel_url, &wheel_path, Some(&release.sha256)) {
                Ok(()) => true,
                Err(download_err) => {
                    log::warn!(
                        "headroom wheel download failed (will fall back to pip index): {download_err}"
                    );
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
                if let Some(update) =
                    pip_line_to_progress(line, deps_start.elapsed(), &mut dep_counter, 55, 80)
                {
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
                std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)
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
        let output = Command::new("codesign")
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
        std::fs::write(&marker, serde_json::to_vec_pretty(&body)?)
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
            log::warn!(
                "recover_from_interrupted_upgrade: in-place upgrade was in progress; \
                 reinstalling previous headroom-ai {previous_version}"
            );
            if let Some(ref backup) = previous_lock_backup {
                let _ = self.pip_restore_deps_from_backup(backup);
                let _ = std::fs::copy(backup, self.active_lock_path());
                let _ = std::fs::remove_file(backup);
            }
            let _ = self.pip_force_reinstall_headroom_version(&previous_version);
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

        log::warn!(
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
                    if let Some(update) =
                        pip_line_to_progress(line, deps_start.elapsed(), &mut dep_counter, 15, 55)
                    {
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
                Err(download_err) => {
                    log::warn!(
                        "headroom wheel download failed (will fall back to pip index): {download_err}"
                    );
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
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)
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
                let _ = std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?);
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
                    let existing = std::env::var("PATH").unwrap_or_default();
                    let augmented = if existing.is_empty() {
                        dir.display().to_string()
                    } else {
                        format!("{}:{}", dir.display(), existing)
                    };
                    cmd.env("PATH", augmented);
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
            if !stdout.contains("not detected on this system") {
                let detected = detected_claude
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<not detected>".into());
                sentry::with_scope(
                    |scope| {
                        scope.set_extra("claude_cli_detected", detected.clone().into());
                        scope.set_extra("exit_code", exit_code.map(|c| c.into()).unwrap_or(serde_json::Value::Null));
                        scope.set_extra("signal", signal.map(|s| s.into()).unwrap_or(serde_json::Value::Null));
                        scope.set_extra(
                            "stdout_tail",
                            stdout[stdout.char_indices().rev().nth(2047).map_or(0, |(i, _)| i)..].into(),
                        );
                        scope.set_extra(
                            "stderr_tail",
                            stderr[stderr.char_indices().rev().nth(2047).map_or(0, |(i, _)| i)..].into(),
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
        if let Ok(()) = write_headroom_to_claude_json(&entrypoint, HEADROOM_PROXY_URL) {
            if claude_code_has_headroom_mcp_server() {
                return Ok(McpInstallMethod::DirectClaudeJson);
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detected = detected_claude
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<not detected>".into());
        sentry::with_scope(
            |scope| {
                scope.set_extra("claude_cli_detected", detected.clone().into());
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

    fn install_rtk(&self) -> Result<()> {
        let artifact = rtk_distribution_artifact()?;
        let archive_path = self.runtime.downloads_dir.join(format!(
            "rtk-v{}-{}-{}.tar.gz",
            RTK_VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        download_to_path(&artifact.url, &archive_path, artifact.sha256)?;

        let extract_dir = self.runtime.downloads_dir.join("rtk-extract");
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

        let extracted_binary = extract_dir.join("rtk");
        if !extracted_binary.exists() {
            bail!(
                "rtk extraction completed but {} was not found",
                extracted_binary.display()
            );
        }

        let destination = self.rtk_entrypoint();
        std::fs::copy(&extracted_binary, &destination)
            .with_context(|| format!("writing {}", destination.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&destination)
                .with_context(|| format!("reading {}", destination.display()))?
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&destination, permissions)
                .with_context(|| format!("chmod {}", destination.display()))?;
        }

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
        std::fs::write(&lock_path, contents)
            .with_context(|| format!("writing {}", lock_path.display()))?;
        Ok(lock_path)
    }

    fn write_bootstrap_receipt(&self) -> Result<()> {
        let receipt = self.runtime.root_dir.join("bootstrap-receipt.json");
        std::fs::write(
            &receipt,
            serde_json::to_vec_pretty(&json!({
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
        std::fs::write(
            &ready_flag,
            json!({
                "managedPython": self.runtime.managed_python(),
                "managedPip": self.runtime.managed_pip(),
                "scope": "self-contained"
            })
            .to_string(),
        )
        .with_context(|| format!("writing {}", ready_flag.display()))?;
        Ok(())
    }

    fn write_tool_receipt(&self, tool_id: &str, payload: serde_json::Value) -> Result<()> {
        let path = self.runtime.tools_dir.join(format!("{tool_id}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&payload).context("serializing managed tool receipt")?,
        )
        .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn detect_status(&self, tool_id: &str) -> ToolStatus {
        let installed_path = self.runtime.tools_dir.join(format!("{tool_id}.json"));
        if installed_path.exists() && self.python_runtime_installed() {
            ToolStatus::Healthy
        } else {
            ToolStatus::NotInstalled
        }
    }
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
    let path = home.join(".claude.json");

    let mut config: Value = if path.exists() {
        std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };

    let root = config
        .as_object_mut()
        .context("~/.claude.json root is not a JSON object")?;

    root.entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("~/.claude.json mcpServers is not a JSON object")?
        .insert(
            "headroom".into(),
            json!({
                "command": entrypoint,
                "args": ["mcp", "serve"],
                "env": { "HEADROOM_PROXY_URL": proxy_url },
            }),
        );

    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("writing {}", path.display()))
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
        PortState::HeadroomRunning
    } else {
        PortState::ForeignOccupant(lsof_listener(port).unwrap_or_else(|| "unknown process".into()))
    }
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

fn lsof_listener(port: u16) -> Option<String> {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-iTCP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().nth(1)?;
    let mut fields = line.split_whitespace();
    let cmd = fields.next()?;
    let pid = fields.next()?;
    Some(format!("{cmd} pid {pid}"))
}

/// Extract the numeric pid from a `"cmd pid 1234"` string returned by
/// [`lsof_listener`]. Returns None for the `"unknown process"` placeholder
/// or any unparseable shape. Companion to `port_conflict::parse_occupant`,
/// which works on the full bail string instead of the lsof detail.
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
fn redact_sensitive(line: &str) -> String {
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
                    b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
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

fn headroom_entrypoint_startup_args() -> Vec<String> {
    // The CLI `proxy` command does not expose --no-http2; HTTP/2 is controlled
    // via the HEADROOM_HTTP2 env var when using the entrypoint.
    // --log-messages stores full request/response bodies so the desktop's
    // Activity tab can render the live transformations feed.
    let mut args = vec![
        "proxy".to_string(),
        "--port".to_string(),
        headroom_proxy_port(),
        "--log-messages".to_string(),
    ];
    args.extend(headroom_learn_startup_args());
    args
}

/// Flags whose presence in the running proxy's argv we treat as proof that it
/// was started by this build. If any of these are missing, the proxy was
/// spawned by an older desktop (or by something else) and we restart it.
fn expected_proxy_arg_signature() -> Vec<&'static str> {
    vec![
        "--port",
        "--log-messages",
        "--learn",
        "--no-memory-tools",
        "--no-memory-context",
        "--memory-db-path",
    ]
}

/// Returns the full command line of whatever process is currently listening on
/// the proxy port, or `None` if we couldn't determine it.
pub fn running_proxy_argv() -> Option<String> {
    let pid = lsof_listener_pid(backend_port::get())?;
    ps_command(pid)
}

/// True if the running proxy's argv contains every flag we expect this build
/// to pass. Used to detect proxies left over from an older desktop version.
pub fn running_proxy_matches_expected_args() -> bool {
    let Some(argv) = running_proxy_argv() else {
        return false;
    };
    proxy_argv_contains_expected_flags(&argv)
}

fn proxy_argv_contains_expected_flags(argv: &str) -> bool {
    expected_proxy_arg_signature()
        .iter()
        .all(|flag| argv_contains_flag(argv, flag))
}

/// Whitespace-aware containment check so `--port` doesn't match `--port-foo`
/// and `--learn` doesn't match `--no-learn`.
fn argv_contains_flag(argv: &str, flag: &str) -> bool {
    argv.split_whitespace().any(|tok| tok == flag)
}

fn lsof_listener_pid(port: u16) -> Option<u32> {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix('p').and_then(|n| n.trim().parse().ok()))
}

fn ps_command(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
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
        (os, arch) => bail!("unsupported Headroom managed Python target: {os}/{arch}"),
    }
}

fn rtk_distribution_artifact() -> Result<DownloadArtifact> {
    let (target, sha256) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("aarch64-apple-darwin", RTK_SHA256_MACOS_AARCH64),
        ("macos", "x86_64") => ("x86_64-apple-darwin", RTK_SHA256_MACOS_X86_64),
        ("linux", "aarch64") => ("aarch64-unknown-linux-gnu", RTK_SHA256_LINUX_AARCH64),
        ("linux", "x86_64") => ("x86_64-unknown-linux-musl", RTK_SHA256_LINUX_X86_64),
        (os, arch) => bail!("unsupported RTK target: {os}/{arch}"),
    };

    Ok(DownloadArtifact {
        url: format!(
            "https://github.com/rtk-ai/rtk/releases/download/v{}/rtk-{}.tar.gz",
            RTK_VERSION, target
        ),
        sha256: Some(sha256),
    })
}

fn download_to_path(url: &str, destination: &Path, expected_sha256: Option<&str>) -> Result<()> {
    download_to_path_with_progress(url, destination, expected_sha256, |_, _| {})
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
                    last_emit = Instant::now();
                }
            }
            file.flush().context("flushing download")?;
            drop(file);
            on_progress(downloaded, total_bytes);

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
    let start = content.find("<!-- headroom:learn:start -->")?;
    let end = content.find("<!-- headroom:learn:end -->")?;
    if end <= start {
        return None;
    }

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

/// Parse sections and bullets inside the managed `<!-- headroom:learn -->`
/// block. Returns an empty Vec if no block is present.
pub fn parse_headroom_learn_block(file_content: &str) -> Vec<crate::models::AppliedSection> {
    use crate::models::AppliedSection;
    let Some(start) = file_content.find("<!-- headroom:learn:start -->") else {
        return Vec::new();
    };
    let Some(end_rel) = file_content[start..].find("<!-- headroom:learn:end -->") else {
        return Vec::new();
    };
    let block = &file_content[start..start + end_rel];

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
    let Some(start) = file_content.find("<!-- headroom:learn:start -->") else {
        return file_content.to_string();
    };
    let end_marker = "<!-- headroom:learn:end -->";
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
    while out_lines.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
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
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
    {
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
        _ => HEADROOM_REQUIREMENTS_LOCK,
    }
}

fn run_python_command(python: &Path, args: &[&str], cwd: &Path) -> Result<()> {
    run_command(python, args, cwd)
}

fn build_command(binary: &Path, args: &[&str], cwd: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONSTARTUP")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("LC_ALL", "C.UTF-8")
        .env("LANG", "C.UTF-8")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1");
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

/// Translate a pip stdout/stderr line into a progress update, or None for
/// noise. Counter-based monotonic advance inside `[base_percent, max_percent-1]`:
/// we don't know the final dep count up-front, so each interesting line nudges
/// the bar forward and it saturates just below the parent step's ceiling.
fn pip_line_to_progress(
    line: &str,
    elapsed: Duration,
    counter: &mut u32,
    base_percent: u8,
    max_percent: u8,
) -> Option<BootstrapStepUpdate> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let message = if let Some(rest) = trimmed.strip_prefix("Collecting ") {
        let spec = rest.split_whitespace().next().unwrap_or(rest);
        let pkg = spec
            .split(|c: char| matches!(c, '=' | '<' | '>' | '!' | '~' | ';' | '['))
            .next()
            .unwrap_or(spec);
        format!("Fetching {}...", pkg)
    } else if let Some(rest) = trimmed.strip_prefix("Downloading ") {
        let file = rest.split_whitespace().next().unwrap_or(rest);
        let name = file.rsplit('/').next().unwrap_or(file);
        let pkg = name.split('-').next().unwrap_or(name);
        format!("Downloading {}...", pkg)
    } else if trimmed.starts_with("Installing collected packages") {
        "Installing packages...".to_string()
    } else if let Some(rest) = trimmed.strip_prefix("Successfully installed ") {
        let count = rest.split_whitespace().count();
        format!("Installed {} packages.", count)
    } else {
        return None;
    };

    *counter = counter.saturating_add(1);
    let span = max_percent.saturating_sub(base_percent).max(1) as u32;
    let advance = (*counter).min(span.saturating_sub(1));
    let percent = (base_percent as u32 + advance).min(max_percent as u32 - 1) as u8;

    let remaining = 90_u64.saturating_sub(elapsed.as_secs()).max(5);
    Some(BootstrapStepUpdate {
        step: "Updating dependencies",
        message,
        eta_seconds: remaining,
        percent,
    })
}

// Compact representation of a pip-install failure for log/Sentry. The full
// CommandFailure Display dumps program + args + stdout + stderr, which the
// 400-char Sentry cap eats before any stderr lines appear. Pip's actual
// reason lives on stderr, so prefer the tail of stderr (or stdout if stderr
// is empty) plus exit code.
fn compact_pip_failure(err: &anyhow::Error) -> String {
    const TAIL_BUDGET: usize = 300;
    let Some(failure) = err.chain().find_map(|c| c.downcast_ref::<CommandFailure>()) else {
        return err.to_string();
    };
    let source = if !failure.stderr.trim().is_empty() {
        failure.stderr.as_str()
    } else {
        failure.stdout.as_str()
    };
    let trimmed = source.trim_end();
    let tail = if trimmed.len() > TAIL_BUDGET {
        let start = trimmed.len() - TAIL_BUDGET;
        let aligned = trimmed[start..]
            .find('\n')
            .map(|i| start + i + 1)
            .unwrap_or(start);
        &trimmed[aligned..]
    } else {
        trimmed
    };
    let exit = failure
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    format!("exit={exit}; stderr tail: {tail}")
}

/// Streaming variant of `run_pip_install_with_retries`. Each line emitted by
/// pip on stdout/stderr is piped through `on_line` as it arrives, so callers
/// can translate noteworthy pip events ("Collecting X", "Downloading Y",
/// "Installing collected packages", "Successfully installed") into
/// user-facing progress updates instead of staring at a static message for
/// the 60–90 seconds a large pip install takes.
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
        match run_command_streaming(python, args, cwd, &mut on_line) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt < MAX_ATTEMPTS {
                    log::info!(
                        "pip install attempt {}/{} failed (will retry): {}",
                        attempt, MAX_ATTEMPTS, err
                    );
                } else {
                    log::warn!(
                        "pip install attempt {}/{} failed (final): {}",
                        attempt, MAX_ATTEMPTS, compact_pip_failure(&err)
                    );
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
fn run_command_streaming<F>(binary: &Path, args: &[&str], cwd: &Path, on_line: &mut F) -> Result<()>
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

    while let Ok(streamed) = rx.recv() {
        on_line(&streamed.line);
        let sink = if streamed.is_stderr {
            &mut stderr_buf
        } else {
            &mut stdout_buf
        };
        sink.push_str(&streamed.line);
        sink.push('\n');
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

impl std::fmt::Display for HeadroomStartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} (log: {}){}",
            self.program,
            self.args.join(" "),
            self.reason,
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

pub(crate) fn find_rtk_on_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("rtk");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            return metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0);
        }
    }
    #[cfg(not(unix))]
    {
        if let Ok(metadata) = std::fs::metadata(path) {
            return metadata.is_file();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use chrono::Local;

    use super::{
        bootstrap_requirements_lock_for_target, extract_required_pydantic_core_version,
        format_all_foreign_bail, format_already_running_bail,
        headroom_entrypoint_startup_args, headroom_python_startup_args,
        looks_like_corrupt_venv_error, parse_major_minor_patch, parse_pid_from_lsof_detail,
        proxy_argv_contains_expected_flags, read_headroom_learn_metadata_from_path,
        receipt_requires_atomic_rebuild, redact_sensitive, requirements_lock_sha,
        rtk_distribution_artifact, run_command, sanitize_log_variant, sha256_bytes,
        verify_sha256_file, CommandFailure, HeadroomRelease, ManagedRuntime,
        PipOutputCapture, ToolManager, UpgradeOutcome, ATOMIC_REBUILD_FLOOR_VERSION,
        RTK_VERSION,
    };
    use crate::backend_port;
    use crate::port_conflict;

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
        let (root, runtime, manager) = seed_test_runtime("rtk-installed");

        assert!(!manager.rtk_installed(), "no binary or receipt yet");

        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
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
        let (root, runtime, manager) = seed_test_runtime("rtk-version");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
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
        let (root, runtime, manager) = seed_test_runtime("rtk-needs-install-current");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
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
    fn read_rtk_activity_reports_not_installed_when_missing() {
        let (root, _runtime, manager) = seed_test_runtime("rtk-not-installed");

        let lines = manager
            .read_rtk_activity(10)
            .expect("not-installed fallback");
        assert_eq!(lines, vec!["RTK is not installed yet.".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_rtk_activity_returns_last_lines_from_session_output() {
        let (root, runtime, manager) = seed_test_runtime("rtk-activity");
        write_executable(
            &runtime.bin_dir.join("rtk"),
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
    fn read_rtk_activity_surfaces_session_failures() {
        let (root, runtime, manager) = seed_test_runtime("rtk-activity-fail");
        write_executable(
            &runtime.bin_dir.join("rtk"),
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
    fn rtk_today_stats_returns_none_on_command_failure() {
        let (root, runtime, manager) = seed_test_runtime("rtk-today-fail");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo 'boom' 1>&2;\n  exit 4\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        assert!(manager.rtk_today_stats().is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rtk_today_stats_returns_none_on_invalid_json() {
        let (root, runtime, manager) = seed_test_runtime("rtk-today-invalid-json");
        write_executable(
            &runtime.bin_dir.join("rtk"),
            "#!/usr/bin/env bash\nif [ \"$1\" = \"gain\" ]; then\n  echo 'not-json';\n  exit 0\nfi\nexit 9\n",
        );
        manager
            .write_tool_receipt("rtk", serde_json::json!({ "version": RTK_VERSION }))
            .expect("rtk receipt");

        assert!(manager.rtk_today_stats().is_none());

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

        manager.bootstrap_all().expect("bootstrap succeeds");

        assert!(runtime.managed_python().exists());
        assert!(runtime.tools_dir.join("headroom.json").exists());
        assert!(runtime.bin_dir.join("rtk").exists());
    }

    #[test]
    fn proxy_argv_matches_when_all_expected_flags_present() {
        let argv = "/usr/bin/nice -n 5 /Users/x/headroom proxy --port 6768 --log-messages \
                    --learn --no-memory-tools --no-memory-context --memory-db-path /tmp/m.db";
        assert!(proxy_argv_contains_expected_flags(argv));
    }

    #[test]
    fn proxy_argv_mismatch_when_log_messages_missing() {
        // The exact orphan-from-old-build case: a v0.2.x proxy still running
        // with just `proxy --port 6768`.
        let argv = "/Users/x/headroom proxy --port 6768";
        assert!(!proxy_argv_contains_expected_flags(argv));
    }

    #[test]
    fn proxy_argv_mismatch_when_learn_missing() {
        let argv = "headroom proxy --port 6768 --log-messages --no-memory-tools \
                    --no-memory-context --memory-db-path /tmp/m.db";
        assert!(!proxy_argv_contains_expected_flags(argv));
    }

    #[test]
    fn proxy_argv_match_does_not_get_fooled_by_negated_flag_substring() {
        // `--no-learn` contains `--learn` as a substring; whitespace tokenizing
        // ensures we don't false-positive on it.
        let argv = "headroom proxy --port 6768 --log-messages --no-learn \
                    --no-memory-tools --no-memory-context --memory-db-path /tmp/m.db";
        assert!(!proxy_argv_contains_expected_flags(argv));
    }

    #[test]
    fn proxy_argv_match_works_for_python_module_invocation() {
        let argv = "/Users/x/venv/bin/python3 -m headroom.proxy.server --port 6768 \
                    --no-http2 --log-messages --learn --no-memory-tools --no-memory-context \
                    --memory-db-path /tmp/m.db";
        assert!(proxy_argv_contains_expected_flags(argv));
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
    fn parse_pid_from_lsof_detail_extracts_numeric_pid() {
        assert_eq!(parse_pid_from_lsof_detail("rapportd pid 594"), Some(594));
        assert_eq!(parse_pid_from_lsof_detail("python3.12 pid 1073"), Some(1073));
        assert_eq!(
            parse_pid_from_lsof_detail("Google Chrome Helper pid 4242"),
            Some(4242)
        );
    }

    #[test]
    fn parse_pid_from_lsof_detail_returns_none_for_unknown_or_malformed() {
        assert_eq!(parse_pid_from_lsof_detail("unknown process"), None);
        assert_eq!(parse_pid_from_lsof_detail(""), None);
        assert_eq!(parse_pid_from_lsof_detail("rapportd pid not-a-number"), None);
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
    fn managed_headroom_startup_uses_supported_proxy_args() {
        backend_port::reset_for_tests();
        let default_port = backend_port::DEFAULT_BACKEND_PORT.to_string();
        let entrypoint_args = headroom_entrypoint_startup_args();
        assert!(entrypoint_args.starts_with(&[
            "proxy".to_string(),
            "--port".to_string(),
            default_port.clone(),
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

    /// Regression: `start_headroom_background` previously built `startup_variants`
    /// before pre-flight ran, so when fallback called `backend_port::set(6769)`
    /// the variants still spawned with `--port 6768` and both failed with
    /// EADDRINUSE. The arg helpers read the atomic at call time, so as long as
    /// the helpers are invoked AFTER fallback has updated the atomic, the
    /// chosen fallback port flows through.
    #[test]
    fn startup_args_reflect_fallback_port_set_after_default() {
        backend_port::reset_for_tests();
        backend_port::set(6770);

        let entrypoint_args = headroom_entrypoint_startup_args();
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
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod");
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
        assert!(paths.iter().any(|p| p.ends_with("mmh3.cpython-312-darwin.so")));
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
    fn recover_from_interrupted_upgrade_handles_wheel_only_marker() {
        // In-place marker without a lock backup (wheel-only interrupted
        // upgrade). The pip reinstall inside recovery fails harmlessly without
        // a real python; we only assert the file-manipulation side: receipt
        // restored, marker cleared.
        let (root, runtime, manager) = seed_test_runtime("recover-wheel-only");
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
    fn recover_from_interrupted_upgrade_handles_in_place_marker_with_lock_backup() {
        // In-place marker with a lock backup. Recovery should: copy the lock
        // backup back to the active lock path, remove the backup, restore the
        // receipt, and clear the marker. Pip calls fail harmlessly without a
        // real python.
        let (root, runtime, manager) = seed_test_runtime("recover-lock-backup");
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

    #[test]
    fn repair_stale_requirements_updates_receipt_and_emits_progress() {
        let (root, runtime, manager) = seed_test_runtime("repair-requirements");
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
    fn smoke_test_headroom_succeeds_with_executable_python() {
        let (root, runtime, manager) = seed_test_runtime("smoke-ok");
        write_executable(&runtime.managed_python(), "#!/bin/sh\nexit 0\n");

        manager
            .smoke_test_headroom_with_timeout(Duration::from_secs(2))
            .expect("smoke test succeeds");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
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
}
