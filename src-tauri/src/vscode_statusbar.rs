//! Headroom's status bar item for the Claude Code panel in VS Code and Cursor.
//!
//! The panel renders no `statusLine` (its webview has no code for one), so the
//! terminal line from claude_statusline.rs never reaches it. This packs the
//! small extension in resources/vscode-statusbar into a .vsix and installs it
//! through each editor's own CLI; it reads the same per-conversation file.
//!
//! Installed alongside the terminal statusline and under the same opt-out.
//! Removed only when the user turns that off or uninstalls Headroom, never on
//! quit: an editor CLI round trip per launch would be slow and churn the
//! editor's extension list. Once installed, a missing copy means the user
//! uninstalled it, and it is not put back unless they turn the feature on
//! again. macOS only for now, like the terminal line's untested Windows path.

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
}

fn editors() -> Vec<Editor> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let home = crate::client_adapters::home_dir();
    [
        (
            "vscode",
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
            ".vscode",
        ),
        (
            "cursor",
            "/Applications/Cursor.app/Contents/Resources/app/bin/cursor",
            ".cursor",
        ),
    ]
    .into_iter()
    .map(|(id, cli, dir)| Editor {
        id,
        cli: PathBuf::from(cli),
        extensions_dir: home.join(dir).join("extensions"),
    })
    .filter(|editor| editor.cli.exists())
    .collect()
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

/// Install when the editor has never had it, or has an older build of it.
/// Nothing present after an earlier install means the user removed it.
fn should_install(current: &str, present: &[String], installed_before: bool) -> bool {
    if present.iter().any(|v| v == current) {
        return false;
    }
    !present.is_empty() || !installed_before
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Tracking {
    /// Editors Headroom has installed the extension into.
    installed: BTreeSet<String>,
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
/// where this install keeps the per-conversation savings file.
fn build_vsix(state_path: &Path) -> Result<Vec<u8>> {
    let version = extension_version();
    let config = serde_json::json!({ "statePath": state_path }).to_string();
    let content_types = r#"<?xml version="1.0" encoding="utf-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension=".json" ContentType="application/json"/><Default Extension=".js" ContentType="application/javascript"/><Default Extension=".vsixmanifest" ContentType="text/xml"/></Types>"#;
    let manifest = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011" xmlns:d="http://schemas.microsoft.com/developer/vsx-schema-design/2011">
  <Metadata>
    <Identity Language="en-US" Id="headroom-status" Version="{version}" Publisher="headroom"/>
    <DisplayName>Headroom</DisplayName>
    <Description xml:space="preserve">Shows what Headroom saved in your Claude Code conversation, in the status bar.</Description>
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

fn run_cli(editor: &Editor, args: &[&std::ffi::OsStr]) -> Result<()> {
    let mut command = crate::proc::command(&editor.cli);
    command.args(args);
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
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn install(editor: &Editor, state_path: &Path) -> Result<()> {
    let vsix = std::env::temp_dir().join(format!("headroom-status-{}.vsix", std::process::id()));
    // direct-write: a throwaway CLI input, deleted right after; not persisted state.
    std::fs::write(&vsix, build_vsix(state_path)?)
        .with_context(|| format!("writing {}", vsix.display()))?;
    let result = run_cli(
        editor,
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
    std::thread::spawn(move || {
        let _serial = INSTALL.lock().unwrap_or_else(|p| p.into_inner());
        let current = extension_version();
        let state_path = crate::claude_statusline::state_path();
        let mut tracking = load_tracking();
        let before = tracking.installed.clone();
        for editor in &editors {
            let present = installed_versions(&editor.extensions_dir);
            if present.contains(&current) {
                tracking.installed.insert(editor.id.to_string());
                continue;
            }
            let removed_by_user = tracking.installed.contains(editor.id)
                && !orphaned_by_our_uninstall(&editor.extensions_dir);
            if !should_install(&current, &present, removed_by_user) {
                continue;
            }
            match install(editor, &state_path) {
                Ok(()) => {
                    log::info!("installed Headroom status bar extension in {}", editor.id);
                    tracking.installed.insert(editor.id.to_string());
                }
                Err(err) => {
                    log::warn!("installing Headroom status bar extension failed: {err:#}")
                }
            }
        }
        if tracking.installed != before {
            save_tracking(&tracking);
        }
    });
}

/// Uninstall from every editor and forget earlier installs, so turning the
/// feature back on installs it again. Synchronous: callers are a settings
/// toggle and Headroom's own uninstall.
pub fn uninstall() -> Result<()> {
    let _serial = INSTALL.lock().unwrap_or_else(|p| p.into_inner());
    let mut failures = Vec::new();
    for editor in editors() {
        if installed_versions(&editor.extensions_dir).is_empty() {
            continue;
        }
        if let Err(err) = run_cli(
            &editor,
            &["--uninstall-extension".as_ref(), EXTENSION_ID.as_ref()],
        ) {
            failures.push(format!("{err:#}"));
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
    fn installs_fresh_and_upgrades_but_respects_a_user_uninstall() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(should_install("0.2.0", &[], false), "fresh editor");
        assert!(!should_install("0.2.0", &v(&["0.2.0"]), true), "up to date");
        assert!(should_install("0.2.0", &v(&["0.1.0"]), true), "older build");
        assert!(!should_install("0.2.0", &[], true), "user removed it");
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
        let bytes = build_vsix(state).unwrap();
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
}
