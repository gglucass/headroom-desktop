//! Headroom's status bar item for the Claude Code panel in VS Code and Cursor.
//!
//! The panel renders no `statusLine` (its webview has no code for one), so the
//! terminal line from claude_statusline.rs never reaches it. This packs the
//! small extension in resources/vscode-statusbar into a .vsix and installs it
//! through each editor's own CLI, into the default profile and every named
//! profile with extensions of its own; it reads the same per-conversation file
//! and hides whenever the terminal line's script is gone (pause, quit).
//!
//! Installed alongside the terminal statusline and under the same opt-out.
//! Removed only when the user turns that off or uninstalls Headroom, never on
//! quit: an editor CLI round trip per launch would be slow and churn the
//! editor's extension list. Once installed, a missing copy means the user
//! uninstalled it, and it is not put back unless they turn the feature on
//! again. Finds VS Code and Cursor where their installers put the CLI on
//! macOS, Windows and Linux (not a Cursor AppImage, which has no fixed path).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

const EXTENSION_ID: &str = "headroom.headroom-status";
const EXTENSION_JS: &str = include_str!("../resources/vscode-statusbar/extension.cjs");
const PACKAGE_JSON: &str = include_str!("../resources/vscode-statusbar/package.json");
const TRACKING_FILE: &str = "vscode-statusbar.json";

/// Serializes installs: a launch and a settings toggle can both ask at once.
static INSTALL: Mutex<()> = Mutex::new(());

struct Editor {
    id: &'static str,
    cli: PathBuf,
    extensions_dir: PathBuf,
    /// The editor's `User` folder, whose globalStorage lists named profiles.
    user_dir: PathBuf,
}

fn editors() -> Vec<Editor> {
    let home = crate::client_adapters::home_dir();
    let data_root = editor_data_root(&home);
    [
        ("vscode", ".vscode", "Code"),
        ("cursor", ".cursor", "Cursor"),
    ]
    .into_iter()
    .filter_map(|(id, dir, data)| {
        Some(Editor {
            id,
            cli: cli_candidates(id, &home)
                .into_iter()
                .find(|cli| cli.exists())?,
            extensions_dir: home.join(dir).join("extensions"),
            user_dir: data_root.join(data).join("User"),
        })
    })
    .collect()
}

/// Where VS Code and its forks keep their `User` folder on this OS.
fn editor_data_root(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
    }
}

/// The editor's CLI where its installers put it; the first that exists wins.
/// Linux: the .deb/.rpm location, its /usr/bin link, the snap.
fn cli_candidates(id: &str, home: &Path) -> Vec<PathBuf> {
    let local_programs = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Local"))
        .join("Programs");
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    match (id, std::env::consts::OS) {
        ("vscode", "macos") => {
            vec!["/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code".into()]
        }
        ("cursor", "macos") => {
            vec!["/Applications/Cursor.app/Contents/Resources/app/bin/cursor".into()]
        }
        ("vscode", "windows") => [local_programs, program_files]
            .into_iter()
            .map(|root| root.join("Microsoft VS Code").join("bin").join("code.cmd"))
            .collect(),
        ("cursor", "windows") => vec![local_programs
            .join("cursor")
            .join("resources")
            .join("app")
            .join("bin")
            .join("cursor.cmd")],
        ("vscode", "linux") => {
            vec![
                "/usr/share/code/bin/code".into(),
                "/usr/bin/code".into(),
                "/snap/bin/code".into(),
            ]
        }
        ("cursor", "linux") => vec![
            "/usr/share/cursor/resources/app/bin/cursor".into(),
            "/usr/bin/cursor".into(),
            home.join(".local").join("bin").join("cursor"),
        ],
        _ => Vec::new(),
    }
}

fn extension_version() -> String {
    serde_json::from_str::<serde_json::Value>(PACKAGE_JSON)
        .ok()
        .and_then(|v| v["version"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Versions of our extension an editor has installed. The editor's own
/// registry (`extensions.json`) decides: `--uninstall-extension` drops the
/// entry but can leave the folder behind unmarked, and reading the folder then
/// made re-enabling skip the install, so the item stayed gone. Folders are
/// only the fallback for an editor without a registry.
fn installed_versions(extensions_dir: &Path) -> Vec<String> {
    registry_versions(extensions_dir).unwrap_or_else(|| folder_versions(extensions_dir))
}

fn registry_versions(extensions_dir: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(extensions_dir.join("extensions.json")).ok()?;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap_or_default();
    Some(
        entries
            .iter()
            .filter(|e| {
                e["identifier"]["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(EXTENSION_ID))
            })
            .filter_map(|e| e["version"].as_str().map(str::to_owned))
            .collect(),
    )
}

/// Our extension's folders, skipping ones the editor marked obsolete
/// (uninstalled, awaiting deletion on its next start).
fn folder_versions(extensions_dir: &Path) -> Vec<String> {
    let obsolete: BTreeMap<String, serde_json::Value> =
        std::fs::read(extensions_dir.join(".obsolete"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    let prefix = format!("{EXTENSION_ID}-");
    let Ok(entries) = std::fs::read_dir(extensions_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| !obsolete.contains_key(name))
        .filter_map(|name| name.strip_prefix(&prefix).map(str::to_owned))
        .collect()
}

/// A live folder the registry does not list: the trace of our own uninstall
/// under 0.9.22, whose re-enable then skipped the install. A user's uninstall
/// marks the folder obsolete or deletes it, so this never reads as theirs.
fn orphaned_by_our_uninstall(extensions_dir: &Path) -> bool {
    registry_versions(extensions_dir).is_some_and(|listed| listed.is_empty())
        && !folder_versions(extensions_dir).is_empty()
}

/// A profile to install into. The default one lists its extensions in the
/// extensions folder; a named one (the editor's Profiles feature) in its own
/// `extensions.json`, and never loads what only the default one has.
struct Profile {
    /// `--profile` for the editor CLI; None for the default profile.
    name: Option<String>,
    /// Folder holding this profile's `extensions.json`.
    registry_dir: PathBuf,
    /// Tracking key: the editor id for the default profile, as before profiles.
    key: String,
}

fn profiles(editor: &Editor) -> Vec<Profile> {
    let mut out = vec![Profile {
        name: None,
        registry_dir: editor.extensions_dir.clone(),
        key: editor.id.to_string(),
    }];
    let storage: serde_json::Value =
        std::fs::read(editor.user_dir.join("globalStorage").join("storage.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    for profile in storage["userDataProfiles"].as_array().into_iter().flatten() {
        // Sharing the default profile's extensions: already covered.
        if profile["useDefaultFlags"]["extensions"].as_bool() == Some(true) {
            continue;
        }
        let Some(name) = profile["name"].as_str() else {
            continue;
        };
        // Stored relative to User/profiles; older editors stored a file URI.
        let location = &profile["location"];
        let registry_dir = match (location.as_str(), location["path"].as_str()) {
            (Some(relative), _) => editor.user_dir.join("profiles").join(relative),
            (None, Some(absolute)) => PathBuf::from(absolute),
            (None, None) => continue,
        };
        out.push(Profile {
            name: Some(name.to_string()),
            key: format!("{}:{}", editor.id, registry_dir.display()),
            registry_dir,
        });
    }
    out
}

/// Install when the editor has never had it, or has an older build of it.
/// Nothing present after an earlier install means the user removed it.
fn should_install(current: &str, present: &[String], installed_before: bool) -> bool {
    if present.iter().any(|v| v == current) {
        return false;
    }
    !present.is_empty() || !installed_before
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Tracking {
    /// Profiles Headroom has installed the extension into (`Profile::key`).
    installed: BTreeSet<String>,
    /// Profile -> extension version whose failed install was already reported.
    reported_failures: BTreeMap<String, String>,
}

/// True the first time `key` fails to install `version`. An editor that
/// cannot install (a `.vscode` junction to a missing drive: ENOENT on its own
/// extensions folder, RUST-KX) fails identically every launch, and each retry
/// filed another Sentry event. Retries continue; only the report is once.
fn first_failure(tracking: &mut Tracking, key: &str, version: &str) -> bool {
    tracking
        .reported_failures
        .insert(key.to_string(), version.to_string())
        .is_none_or(|reported| reported != version)
}

fn tracking_path() -> PathBuf {
    crate::storage::config_file(&crate::storage::app_data_dir(), TRACKING_FILE)
}

fn load_tracking() -> Tracking {
    let path = tracking_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return Tracking::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|err| {
        log::warn!("{TRACKING_FILE} is corrupt ({err}); backing up and starting fresh");
        // direct-write: moves Headroom's own unparsable state aside, never a user file
        let _ = std::fs::rename(&path, path.with_extension("json.bak"));
        Tracking::default()
    })
}

fn save_tracking(tracking: &Tracking) {
    let bytes = serde_json::to_vec(tracking).unwrap_or_default();
    if let Err(err) = crate::client_adapters::atomic_write(&tracking_path(), &bytes) {
        log::warn!("failed to persist {TRACKING_FILE}: {err}");
    }
}

/// The .vsix: a zip of the extension plus `headroom.json`, which tells it
/// where this install keeps the per-conversation savings file, and the
/// terminal statusline script whose absence means Claude Code is not routed.
fn build_vsix(state_path: &Path, script_path: &Path) -> Result<Vec<u8>> {
    let version = extension_version();
    let config =
        serde_json::json!({ "statePath": state_path, "scriptPath": script_path }).to_string();
    let content_types = r#"<?xml version="1.0" encoding="utf-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension=".json" ContentType="application/json"/><Default Extension=".js" ContentType="application/javascript"/><Default Extension=".vsixmanifest" ContentType="text/xml"/></Types>"#;
    let manifest = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011" xmlns:d="http://schemas.microsoft.com/developer/vsx-schema-design/2011">
  <Metadata>
    <Identity Language="en-US" Id="headroom-status" Version="{version}" Publisher="headroom"/>
    <DisplayName>Headroom</DisplayName>
    <Description xml:space="preserve">Shows what Headroom saved in your Claude Code conversation, and your Claude plan usage, in the status bar.</Description>
  </Metadata>
  <Installation><InstallationTarget Id="Microsoft.VisualStudio.Code"/></Installation>
  <Dependencies/>
  <Assets><Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true"/></Assets>
</PackageManifest>
"#
    );
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, body) in [
        ("[Content_Types].xml", content_types),
        ("extension.vsixmanifest", manifest.as_str()),
        ("extension/package.json", PACKAGE_JSON),
        ("extension/extension.js", EXTENSION_JS),
        ("extension/headroom.json", config.as_str()),
    ] {
        writer.start_file(name, options)?;
        writer.write_all(body.as_bytes())?;
    }
    Ok(writer.finish()?.into_inner())
}

/// A CLI that hangs (a lock, a first-run prompt) would otherwise hold INSTALL
/// forever, freezing the statusline toggle and Headroom's uninstall cleanup.
const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn run_cli(editor: &Editor, profile: &Profile, args: &[&std::ffi::OsStr]) -> Result<()> {
    let mut command = crate::proc::command(&editor.cli);
    command.args(args);
    if let Some(name) = &profile.name {
        command.arg("--profile").arg(name);
    }
    let out = crate::proc::output_with_timeout(command, CLI_TIMEOUT).map_err(|err| match err {
        crate::proc::OutputError::Spawn(err) => {
            anyhow!(err).context(format!("running {}", editor.cli.display()))
        }
        crate::proc::OutputError::TimedOut => {
            anyhow!("{} timed out after {}s", editor.id, CLI_TIMEOUT.as_secs())
        }
    })?;
    if !out.status.success() {
        bail!(
            "{} exited {}: {}",
            editor.id,
            out.status,
            cli_stderr(&out.stderr)
        );
    }
    Ok(())
}

/// The CLI's stderr without Node's deprecation warnings, which older editors
/// print first and which filled the whole Sentry message cap (RUST-KP).
fn cli_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !line.starts_with("(node:") && !line.starts_with("(Use `"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn install(editor: &Editor, profile: &Profile, vsix_bytes: &[u8]) -> Result<()> {
    let vsix = std::env::temp_dir().join(format!("headroom-status-{}.vsix", std::process::id()));
    // direct-write: a throwaway CLI input, deleted right after; not persisted state.
    std::fs::write(&vsix, vsix_bytes).with_context(|| format!("writing {}", vsix.display()))?;
    let result = run_cli(
        editor,
        profile,
        &[
            "--install-extension".as_ref(),
            vsix.as_os_str(),
            "--force".as_ref(),
        ],
    );
    let _ = std::fs::remove_file(&vsix);
    result
}

/// Install or upgrade the extension in every editor that needs it. Returns at
/// once; the editor CLIs take seconds, so the work runs on its own thread.
pub fn ensure_installed() {
    // Unit tests reach this through client setup; never drive a real editor.
    if cfg!(test) {
        return;
    }
    let editors = editors();
    if editors.is_empty() {
        return;
    }
    std::thread::spawn(move || install_where_needed(&editors));
}

fn install_where_needed(editors: &[Editor]) {
    let _serial = INSTALL.lock().unwrap_or_else(|p| p.into_inner());
    // Turning the statusline off writes the flag, then uninstalls under
    // INSTALL; an install queued behind that must not put it back.
    if crate::client_adapters::is_statusline_disabled() {
        return;
    }
    let current = extension_version();
    let vsix = match build_vsix(
        &crate::claude_statusline::state_path(),
        &crate::client_adapters::claude_statusline_script_path(),
    ) {
        Ok(vsix) => vsix,
        Err(err) => {
            log::warn!("building the Headroom status bar extension failed: {err:#}");
            return;
        }
    };
    let mut tracking = load_tracking();
    let before = tracking.clone();
    for editor in editors {
        for profile in profiles(editor) {
            let present = installed_versions(&profile.registry_dir);
            if present.contains(&current) {
                tracking.installed.insert(profile.key);
                continue;
            }
            let removed_by_user = tracking.installed.contains(&profile.key)
                && !orphaned_by_our_uninstall(&profile.registry_dir);
            if !should_install(&current, &present, removed_by_user) {
                continue;
            }
            match install(editor, &profile, &vsix) {
                Ok(()) => {
                    log::info!("installed Headroom status bar extension in {}", profile.key);
                    tracking.reported_failures.remove(&profile.key);
                    tracking.installed.insert(profile.key);
                }
                // An editor older than the extension's `engines.vscode` cannot run
                // the Claude Code extension either; nothing to fix on our side.
                Err(err) if format!("{err:#}").contains("not compatible with") => {
                    log::info!(
                        "{} is too old for the Headroom status bar: {err:#}",
                        profile.key
                    )
                }
                Err(err) if first_failure(&mut tracking, &profile.key, &current) => {
                    log::warn!("installing Headroom status bar extension failed: {err:#}")
                }
                Err(err) => log::info!(
                    "installing Headroom status bar extension failed again in {}: {err:#}",
                    profile.key
                ),
            }
        }
    }
    if tracking != before {
        save_tracking(&tracking);
    }
}

/// Uninstall from every editor and forget earlier installs, so turning the
/// feature back on installs it again. Synchronous: callers are a settings
/// toggle and Headroom's own uninstall.
pub fn uninstall() -> Result<()> {
    uninstall_from(&editors())
}

fn uninstall_from(editors: &[Editor]) -> Result<()> {
    let _serial = INSTALL.lock().unwrap_or_else(|p| p.into_inner());
    let mut failures = Vec::new();
    for editor in editors {
        for profile in profiles(editor) {
            if installed_versions(&profile.registry_dir).is_empty() {
                continue;
            }
            if let Err(err) = run_cli(
                editor,
                &profile,
                &["--uninstall-extension".as_ref(), EXTENSION_ID.as_ref()],
            ) {
                failures.push(format!("{err:#}"));
            }
        }
    }
    let path = tracking_path();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_failed_install_once_per_profile_and_version() {
        let mut tracking = Tracking::default();
        assert!(first_failure(&mut tracking, "vscode", "0.2.0"));
        assert!(
            !first_failure(&mut tracking, "vscode", "0.2.0"),
            "same failure, next launch"
        );
        assert!(
            first_failure(&mut tracking, "cursor", "0.2.0"),
            "another profile"
        );
        assert!(
            first_failure(&mut tracking, "vscode", "0.3.0"),
            "new extension build"
        );
    }

    #[test]
    fn installs_fresh_and_upgrades_but_respects_a_user_uninstall() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(should_install("0.2.0", &[], false), "fresh editor");
        assert!(!should_install("0.2.0", &v(&["0.2.0"]), true), "up to date");
        assert!(should_install("0.2.0", &v(&["0.1.0"]), true), "older build");
        assert!(!should_install("0.2.0", &[], true), "user removed it");
    }

    #[test]
    fn cli_errors_drop_node_deprecation_noise() {
        let stderr = b"(node:9001) [DEP0005] DeprecationWarning: Buffer() is deprecated.\n\
            (Use `Electron --trace-deprecation ...` to show where the warning was created)\n\
            Unable to install extension 'headroom.headroom-status' as it is not compatible with VS Code '1.74.3'.\n";
        assert_eq!(
            cli_stderr(stderr),
            "Unable to install extension 'headroom.headroom-status' as it is not compatible with VS Code '1.74.3'."
        );
    }

    #[test]
    fn obsolete_folders_do_not_count_as_installed() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "headroom.headroom-status-0.1.0",
            "headroom.headroom-status-0.2.0",
            "anthropic.claude-code-2.1.281-darwin-arm64",
        ] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(
            dir.path().join(".obsolete"),
            r#"{"headroom.headroom-status-0.1.0":true}"#,
        )
        .unwrap();
        assert_eq!(installed_versions(dir.path()), vec!["0.2.0".to_string()]);
        assert!(installed_versions(&dir.path().join("missing")).is_empty());

        // With a registry, it decides: an uninstall that left its folder
        // behind is not installed, so re-enabling installs again.
        std::fs::write(
            dir.path().join("extensions.json"),
            r#"[{"identifier":{"id":"anthropic.claude-code"},"version":"2.1.281"}]"#,
        )
        .unwrap();
        assert!(installed_versions(dir.path()).is_empty());
        std::fs::write(
            dir.path().join("extensions.json"),
            r#"[{"identifier":{"id":"Headroom.headroom-status"},"version":"0.1.0"}]"#,
        )
        .unwrap();
        assert_eq!(installed_versions(dir.path()), vec!["0.1.0".to_string()]);
        assert!(!orphaned_by_our_uninstall(dir.path()));

        // Folder alive but unlisted: 0.9.22's own uninstall, not the user's.
        std::fs::write(dir.path().join("extensions.json"), "[]").unwrap();
        assert!(orphaned_by_our_uninstall(dir.path()));
        // A user's uninstall marks every folder obsolete.
        std::fs::write(
            dir.path().join(".obsolete"),
            r#"{"headroom.headroom-status-0.1.0":true,"headroom.headroom-status-0.2.0":true}"#,
        )
        .unwrap();
        assert!(!orphaned_by_our_uninstall(dir.path()));
    }

    #[test]
    fn vsix_carries_the_extension_and_this_install_s_state_path() {
        let state = Path::new("/Users/x/Library/Application Support/Headroom/config/s.json");
        let bytes = build_vsix(state, Path::new("/Users/x/.claude/hooks/h.sh")).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let read = |zip: &mut zip::ZipArchive<_>, name: &str| {
            let mut out = String::new();
            std::io::Read::read_to_string(&mut zip.by_name(name).unwrap(), &mut out).unwrap();
            out
        };
        let config: serde_json::Value =
            serde_json::from_str(&read(&mut zip, "extension/headroom.json")).unwrap();
        assert_eq!(config["statePath"], state.to_str().unwrap());
        assert_eq!(read(&mut zip, "extension/extension.js"), EXTENSION_JS);
        let manifest = read(&mut zip, "extension.vsixmanifest");
        assert!(manifest.contains(&format!(r#"Version="{}""#, extension_version())));
        assert!(!extension_version().is_empty());
        zip.by_name("[Content_Types].xml").unwrap();
    }

    /// HOME and the data dir in a temp dir, and a fake editor CLI there that
    /// logs its arguments and keeps the last .vsix it was handed.
    #[cfg(unix)]
    struct Sandbox {
        dir: tempfile::TempDir,
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _env_lock: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl Sandbox {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let env_lock = crate::test_env_lock::lock_home();
            let dir = tempfile::tempdir().unwrap();
            let prev = ["HOME", "HEADROOM_DATA_DIR"]
                .map(|key| (key, std::env::var_os(key)))
                .to_vec();
            std::env::set_var("HOME", dir.path());
            std::env::set_var("HEADROOM_DATA_DIR", dir.path().join("data"));
            crate::storage::ensure_data_dirs(&crate::storage::app_data_dir()).unwrap();
            let cli = dir.path().join("code");
            std::fs::write(
                &cli,
                format!(
                    "#!/bin/sh\necho \"$*\" >> '{0}/cli.log'\n\
                     [ \"$1\" = --install-extension ] && cp \"$2\" '{0}/last.vsix'\nexit 0\n",
                    dir.path().display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                dir,
                prev,
                _env_lock: env_lock,
            }
        }

        fn editor(&self) -> Editor {
            Editor {
                id: "vscode",
                cli: self.dir.path().join("code"),
                extensions_dir: self.dir.path().join(".vscode").join("extensions"),
                user_dir: self.dir.path().join("User"),
            }
        }

        fn cli_log(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("cli.log")).unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for Sandbox {
        fn drop(&mut self) {
            for (key, value) in self.prev.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_extension_knows_where_the_statusline_script_lives() {
        let sandbox = Sandbox::new();
        install_where_needed(&[sandbox.editor()]);
        let vsix = std::fs::read(sandbox.dir.path().join("last.vsix")).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(vsix)).unwrap();
        let mut config = String::new();
        std::io::Read::read_to_string(
            &mut zip.by_name("extension/headroom.json").unwrap(),
            &mut config,
        )
        .unwrap();
        let config: serde_json::Value = serde_json::from_str(&config).unwrap();
        // Headroom deletes that script on every pause and quit, and the item
        // hides with it instead of showing a stale total.
        let script = crate::client_adapters::claude_statusline_script_path();
        assert_eq!(config["scriptPath"], script.to_str().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn an_install_queued_behind_turning_the_statusline_off_stays_off() {
        let sandbox = Sandbox::new();
        let editors = [sandbox.editor()];
        // The toggle writes the flag, then uninstalls under INSTALL; a launch's
        // install that was already waiting on INSTALL runs after it.
        crate::client_adapters::set_statusline_enabled(false).unwrap();
        install_where_needed(&editors);
        assert_eq!(sandbox.cli_log(), "", "reinstalled while the flag says off");

        crate::client_adapters::set_statusline_enabled(true).unwrap();
        install_where_needed(&editors);
        assert!(sandbox.cli_log().starts_with("--install-extension "));
    }

    #[cfg(unix)]
    #[test]
    fn named_profiles_get_the_extension_and_lose_it_on_uninstall() {
        let sandbox = Sandbox::new();
        let editor = sandbox.editor();
        let storage = editor.user_dir.join("globalStorage");
        std::fs::create_dir_all(&storage).unwrap();
        let profiles = serde_json::json!({ "userDataProfiles": [
            { "location": "-5c2f1a", "name": "Work" },
            // Shares the default profile's extensions: nothing of its own.
            { "location": "builtin/agents", "name": "Agents",
              "useDefaultFlags": { "extensions": true } },
            // Older editors stored the location as a file URI.
            { "location": { "$mid": 1, "scheme": "file",
                "path": editor.user_dir.join("profiles").join("-9e01").to_str().unwrap() },
              "name": "Old Box" },
        ]});
        std::fs::write(storage.join("storage.json"), profiles.to_string()).unwrap();

        install_where_needed(std::slice::from_ref(&editor));
        let log = sandbox.cli_log();
        let calls: Vec<&str> = log.lines().collect();
        assert_eq!(calls.len(), 3, "{log}");
        assert!(calls[0].ends_with(" --force"), "default profile: {log}");
        assert!(calls[1].ends_with(" --force --profile Work"), "{log}");
        assert!(calls[2].ends_with(" --force --profile Old Box"), "{log}");

        // Each named profile keeps its own registry; only Work lists it.
        let work = editor.user_dir.join("profiles").join("-5c2f1a");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("extensions.json"),
            format!(
                r#"[{{"identifier":{{"id":"{EXTENSION_ID}"}},"version":"{}"}}]"#,
                extension_version()
            ),
        )
        .unwrap();
        std::fs::remove_file(sandbox.dir.path().join("cli.log")).unwrap();
        uninstall_from(std::slice::from_ref(&editor)).unwrap();
        assert_eq!(
            sandbox.cli_log(),
            format!("--uninstall-extension {EXTENSION_ID} --profile Work\n")
        );
    }
}
