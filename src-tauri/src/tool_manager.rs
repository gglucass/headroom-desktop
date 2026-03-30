use std::fs::OpenOptions;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::models::{ManagedTool, ToolStatus};

const HEADROOM_VERSION: &str = "0.5.6";
const HEADROOM_PROXY_PORT: &str = "6767";
const HEADROOM_PROXY_URL: &str = "http://127.0.0.1:6767";
const HEADROOM_STARTUP_POLL_MS: u64 = 250;
const HEADROOM_STARTUP_TIMEOUT_MS: u64 = 45_000;
const HEADROOM_WHEEL_URL: &str = "https://files.pythonhosted.org/packages/84/35/4f6f01ce76f391a378e83a99008b186b6237baf5dbfc6571d42a5595c5f4/headroom_ai-0.5.6-py3-none-any.whl";
const HEADROOM_WHEEL_SHA256: &str =
    "fd09f1e13f8b9faaab2647d968a46b26a1f7df0c4ad51e1e318bd6804f6e915b";
const HEADROOM_REQUIREMENTS_LOCK: &str = include_str!("../python/headroom-requirements.lock");
const RTK_VERSION: &str = "0.33.1";
const PYTHON_STANDALONE_RELEASE: &str = "20251014";
const PYTHON_SHA256_MACOS_AARCH64: &str =
    "84cb7acbf75264982c8bdd818bfa1ff0f1eb76007b48a5f3e01d28633b46afdf";
const PYTHON_SHA256_MACOS_X86_64: &str =
    "f76a921e71e9c8954cccd00f176b7083041527b3b4223670d05bbb2f51209d3f";

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
struct RtkGainOutput {
    summary: RtkGainSummary,
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

impl ToolManager {
    pub fn new(runtime: ManagedRuntime) -> Self {
        let manifests = vec![
            ManagedToolManifest {
                id: "headroom".into(),
                name: "Headroom".into(),
                description: "Default optimizer stage for every supported client.".into(),
                runtime: "python".into(),
                source_url: "https://pypi.org/project/headroom-ai/".into(),
                version: HEADROOM_VERSION.into(),
                checksum: Some(HEADROOM_WHEEL_SHA256.into()),
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
                checksum: None,
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
                version: manifest.version.clone(),
                checksum: manifest.checksum.clone(),
            })
            .collect()
    }

    pub fn python_runtime_installed(&self) -> bool {
        self.runtime.ready_flag().exists() && self.runtime.managed_python().exists()
    }

    pub fn headroom_entrypoint(&self) -> PathBuf {
        self.runtime.venv_dir.join("bin").join("headroom")
    }

    pub fn managed_python(&self) -> PathBuf {
        self.runtime.managed_python()
    }

    pub fn rtk_entrypoint(&self) -> PathBuf {
        self.runtime.bin_dir.join("rtk")
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

    pub fn headroom_learn_has_persisted_learnings(&self, project_path: &str) -> bool {
        self.headroom_learn_metadata(project_path).is_some()
    }

    pub fn headroom_learn_pattern_count(&self, project_path: &str) -> Option<usize> {
        self.headroom_learn_metadata(project_path)
            .and_then(|metadata| metadata.pattern_count)
    }

    pub fn start_headroom_background(&self) -> Result<Child> {
        let entrypoint = self.headroom_entrypoint();
        if !entrypoint.exists() {
            bail!("headroom entrypoint not found at {}", entrypoint.display());
        }

        let startup_variants: &[&[&str]] = &[&["proxy", "--port", HEADROOM_PROXY_PORT]];
        let mut errors = Vec::new();
        let logs_dir = self.runtime.logs_dir();
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("creating {}", logs_dir.display()))?;

        for args in startup_variants {
            let variant = if args.is_empty() {
                "default".to_string()
            } else {
                args.join("-")
            };
            let log_path = logs_dir.join(format!("headroom-{variant}.log"));
            let log_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .with_context(|| format!("opening {}", log_path.display()))?;

            let mut child = Command::new(&entrypoint)
                .args(*args)
                .current_dir(&self.runtime.root_dir)
                .env("PYTHONNOUSERSITE", "1")
                .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
                .env("PIP_NO_INPUT", "1")
                // Headroom's upstream HTTP/2 path can be brittle on some networks;
                // force HTTP/1.1 for proxy stability.
                .env("HEADROOM_HTTP2", "0")
                .env("HEADROOM_SDK", "headroom-desktop-proxy")
                .stdin(Stdio::null())
                .stdout(Stdio::from(
                    log_file
                        .try_clone()
                        .with_context(|| format!("cloning {}", log_path.display()))?,
                ))
                .stderr(Stdio::from(log_file))
                .spawn()
                .with_context(|| {
                    format!("starting headroom background process: {}", args.join(" "))
                })?;

            let mut startup_ok = false;
            let mut startup_error: Option<String> = None;

            let startup_polls = (HEADROOM_STARTUP_TIMEOUT_MS / HEADROOM_STARTUP_POLL_MS).max(1);
            for _ in 0..startup_polls {
                thread::sleep(Duration::from_millis(HEADROOM_STARTUP_POLL_MS));
                if is_local_proxy_reachable() {
                    startup_ok = true;
                    break;
                }

                match child.try_wait() {
                    Ok(Some(status)) => {
                        startup_error = Some(format!(
                            "headroom {} exited with status {} before opening port 6767 (log: {})",
                            args.join(" "),
                            status,
                            log_path.display()
                        ));
                        break;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        startup_error = Some(format!(
                            "headroom {} wait check failed: {} (log: {})",
                            args.join(" "),
                            err,
                            log_path.display()
                        ));
                        break;
                    }
                }
            }

            if startup_ok {
                return Ok(child);
            }

            let _ = child.kill();
            let _ = child.wait();

            if let Some(error) = startup_error {
                errors.push(error);
            } else {
                errors.push(format!(
                    "headroom {} never opened port {} within {}ms (log: {})",
                    args.join(" "),
                    HEADROOM_PROXY_PORT,
                    HEADROOM_STARTUP_TIMEOUT_MS,
                    log_path.display()
                ));
            }
        }

        Err(anyhow!(
            "unable to keep headroom running in background: {}",
            errors.join("; ")
        ))
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
        let modified = std::fs::metadata(&path).ok()?.modified().ok()?;

        {
            let cache = self
                .log_marker_cache
                .lock()
                .expect("tool log marker cache poisoned");
            if let Some(cached) = cache.as_ref() {
                if cached.tool_id == tool_id && cached.path == path && cached.modified == modified {
                    return cached.result;
                }
            }
        }

        let content = std::fs::read_to_string(&path).ok()?;

        for line in content.lines().rev() {
            let lowered = line.to_ascii_lowercase();
            if lowered.contains(enabled_marker) {
                let result = Some(true);
                let mut cache = self
                    .log_marker_cache
                    .lock()
                    .expect("tool log marker cache poisoned");
                *cache = Some(ToolLogMarkerCache {
                    tool_id: tool_id.to_string(),
                    path,
                    modified,
                    result,
                });
                return result;
            }
            if disabled_markers
                .iter()
                .any(|marker| lowered.contains(marker))
            {
                let result = Some(false);
                let mut cache = self
                    .log_marker_cache
                    .lock()
                    .expect("tool log marker cache poisoned");
                *cache = Some(ToolLogMarkerCache {
                    tool_id: tool_id.to_string(),
                    path,
                    modified,
                    result,
                });
                return result;
            }
        }

        let mut cache = self
            .log_marker_cache
            .lock()
            .expect("tool log marker cache poisoned");
        *cache = Some(ToolLogMarkerCache {
            tool_id: tool_id.to_string(),
            path,
            modified,
            result: None,
        });

        None
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

    pub fn headroom_ml_installed(&self) -> Option<bool> {
        self.read_headroom_receipt()?
            .get("ml")?
            .get("installed")?
            .as_bool()
    }

    pub fn headroom_kompress_enabled(&self) -> Option<bool> {
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
        candidates.into_iter().next().map(|candidate| candidate.metadata)
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

    pub fn rtk_installed(&self) -> bool {
        self.rtk_entrypoint().exists() && self.runtime.tools_dir.join("rtk.json").exists()
    }

    pub fn installed_rtk_version(&self) -> Option<String> {
        self.read_rtk_receipt()?
            .get("version")?
            .as_str()
            .map(|v| v.to_string())
    }

    pub fn rtk_gain_summary(&self) -> Option<RtkGainSummary> {
        if !self.rtk_installed() {
            return None;
        }

        let output = Command::new(self.rtk_entrypoint())
            .args(["gain", "--all", "--format", "json"])
            .current_dir(&self.runtime.root_dir)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        serde_json::from_slice::<RtkGainOutput>(&output.stdout)
            .ok()
            .map(|parsed| parsed.summary)
    }

    /// Returns true when the installed Headroom version doesn't match the
    /// version pinned in this build of Headroom.
    pub fn headroom_needs_upgrade(&self) -> bool {
        match self.installed_headroom_version() {
            Some(installed) => installed != HEADROOM_VERSION,
            None => false, // no receipt → not yet bootstrapped, not an upgrade case
        }
    }

    /// Re-installs Headroom (and MCP) to bring it up to the pinned version.
    /// Call this when `headroom_needs_upgrade()` returns true.
    pub fn upgrade_headroom(&self) -> Result<()> {
        let old = self
            .installed_headroom_version()
            .unwrap_or("unknown".into());
        eprintln!(
            "headroom: upgrading headroom from {} to {}",
            old, HEADROOM_VERSION
        );
        self.install_headroom()
            .context("upgrading headroom to pinned version")
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
            self.install_python_distribution()?;
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

    fn install_python_distribution(&self) -> Result<()> {
        let archive_path = self.runtime.downloads_dir.join("python-standalone.tar.gz");
        let artifact = python_distribution_artifact()?;
        download_to_path(&artifact.url, &archive_path, artifact.sha256)?;

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

    fn install_headroom(&self) -> Result<()> {
        let lock_path = self.write_headroom_requirements_lock()?;
        let artifact = headroom_distribution_artifact();
        let wheel_path = self
            .runtime
            .downloads_dir
            .join("headroom_ai-0.5.6-py3-none-any.whl");
        download_to_path(&artifact.url, &wheel_path, artifact.sha256)?;

        run_python_command(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--upgrade",
                "--requirement",
                lock_path.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
        )
        .context("installing locked Headroom dependencies into Headroom-managed virtualenv")?;

        run_python_command(
            &self.runtime.managed_python(),
            &[
                "-m",
                "pip",
                "install",
                "--no-deps",
                wheel_path.to_string_lossy().as_ref(),
            ],
            &self.runtime.root_dir,
        )
        .context("installing verified Headroom wheel into Headroom-managed virtualenv")?;

        let mcp_install = match self.install_headroom_mcp() {
            Ok(()) => json!({
                "configured": true,
                "proxyUrl": HEADROOM_PROXY_URL
            }),
            Err(err) => {
                eprintln!("headroom MCP setup skipped: {err}");
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
                "version": self.manifests[0].version,
                "artifact": {
                    "url": HEADROOM_WHEEL_URL,
                    "sha256": HEADROOM_WHEEL_SHA256,
                    "requirementsLockSha256": sha256_bytes(HEADROOM_REQUIREMENTS_LOCK.as_bytes())
                },
                "mcp": mcp_install,
                "ml": {
                    "installed": true,
                    "engine": "kompress"
                }
            }),
        )
    }

    fn install_headroom_mcp(&self) -> Result<()> {
        let entrypoint = self.headroom_entrypoint();
        run_command(
            &entrypoint,
            &["mcp", "install", "--proxy-url", HEADROOM_PROXY_URL],
            &self.runtime.root_dir,
        )
        .context("configuring Headroom MCP integration")
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
                    "sha256": Value::Null
                }
            }),
        )
    }

    fn write_headroom_requirements_lock(&self) -> Result<PathBuf> {
        let lock_path = self
            .runtime
            .downloads_dir
            .join("headroom-requirements.lock");
        std::fs::write(&lock_path, HEADROOM_REQUIREMENTS_LOCK)
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

fn is_local_proxy_reachable() -> bool {
    let address: SocketAddr = match "127.0.0.1:6767".parse() {
        Ok(address) => address,
        Err(_) => return false,
    };

    TcpStream::connect_timeout(&address, Duration::from_millis(180)).is_ok()
}

struct DownloadArtifact {
    url: String,
    sha256: Option<&'static str>,
}

fn headroom_distribution_artifact() -> DownloadArtifact {
    DownloadArtifact {
        url: HEADROOM_WHEEL_URL.into(),
        sha256: Some(HEADROOM_WHEEL_SHA256),
    }
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
        (os, arch) => bail!("unsupported Headroom managed Python target: {os}/{arch}"),
    }
}

fn rtk_distribution_artifact() -> Result<DownloadArtifact> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        (os, arch) => bail!("unsupported RTK target: {os}/{arch}"),
    };

    Ok(DownloadArtifact {
        url: format!(
            "https://github.com/rtk-ai/rtk/releases/download/v{}/rtk-{}.tar.gz",
            RTK_VERSION, target
        ),
        sha256: None,
    })
}

fn download_to_path(url: &str, destination: &Path, expected_sha256: Option<&str>) -> Result<()> {
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

    let response = reqwest::blocking::get(url)
        .with_context(|| format!("downloading {}", url))?
        .error_for_status()
        .with_context(|| format!("downloading {}", url))?;

    let bytes = response.bytes().context("reading download body")?;
    if let Some(expected_sha256) = expected_sha256 {
        let actual_checksum = sha256_bytes(&bytes);
        if actual_checksum != expected_sha256 {
            bail!(
                "checksum mismatch for {}: expected {}, got {}",
                url,
                expected_sha256,
                actual_checksum
            );
        }
    }
    std::fs::write(destination, &bytes)
        .with_context(|| format!("writing {}", destination.display()))?;
    Ok(())
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

fn claude_project_memory_file(project_path: &str) -> PathBuf {
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
        project_path
            .trim_start_matches('/')
            .replace('-', "--")
            .replace('/', "-")
    )
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn run_python_command(python: &Path, args: &[&str], cwd: &Path) -> Result<()> {
    run_command(python, args, cwd)
}

fn run_command(binary: &Path, args: &[&str], cwd: &Path) -> Result<()> {
    let output = Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        .output()
        .with_context(|| format!("starting {} {}", binary.display(), args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "command failed: {} {}\nstdout:\n{}\nstderr:\n{}",
            binary.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ManagedRuntime, ToolManager};

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
}
