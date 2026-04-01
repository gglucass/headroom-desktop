use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{
    ClientConnectorStatus, ClientHealth, ClientSetupResult, ClientSetupVerification, ClientStatus,
};
use crate::storage::{app_data_dir, config_file};

// Raw proxy base — use provider-specific constants below when configuring client endpoints.
const HEADROOM_PROXY_URL: &str = "http://127.0.0.1:6767";
const HEADROOM_ANTHROPIC_BASE_URL: &str = "http://127.0.0.1:6767";
const HEADROOM_OPENAI_BASE_URL: &str = "http://127.0.0.1:6767/v1";
const ZSH_PROFILE_FILE: &str = ".zprofile";
const ZSH_RC_FILE: &str = ".zshrc";
const BASH_PROFILE_FILE: &str = ".bash_profile";
const BASH_LOGIN_FILE: &str = ".bash_login";
const POSIX_PROFILE_FILE: &str = ".profile";
const BASH_RC_FILE: &str = ".bashrc";
const ALL_SHELL_FILES: [&str; 6] = [
    ZSH_PROFILE_FILE,
    ZSH_RC_FILE,
    BASH_PROFILE_FILE,
    BASH_LOGIN_FILE,
    POSIX_PROFILE_FILE,
    BASH_RC_FILE,
];

#[derive(Debug, Clone, Copy)]
struct ManagedClientSpec {
    id: &'static str,
    name: &'static str,
}

const MANAGED_CLIENT_SPECS: [ManagedClientSpec; 1] = [ManagedClientSpec {
    id: "claude_code",
    name: "Claude Code",
}];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    Zsh,
    Bash,
    Posix,
}

pub fn detect_clients() -> Vec<ClientStatus> {
    let setup_state = load_setup_state();

    vec![detect_claude_code_client(is_configured(
        &setup_state,
        "claude_code",
    ))]
}

pub fn ensure_rtk_integrations(
    managed_rtk_path: &Path,
    managed_python_path: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
    ensure_rtk_integrations_for_targets(
        managed_rtk_path,
        managed_python_path,
        &resolve_default_shell_targets(),
    )
}

fn ensure_rtk_integrations_for_targets(
    managed_rtk_path: &Path,
    managed_python_path: &Path,
    shell_targets: &[PathBuf],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();

    let mut path_updates = ensure_managed_rtk_on_path(managed_rtk_path, shell_targets)?;
    let mut hook_updates = ensure_claude_code_rtk_hook(managed_rtk_path, managed_python_path)?;
    changed_files.append(&mut path_updates.0);
    backup_files.append(&mut path_updates.1);
    changed_files.append(&mut hook_updates.0);
    backup_files.append(&mut hook_updates.1);

    Ok((changed_files, backup_files))
}

pub fn rtk_integration_status() -> Result<(bool, bool)> {
    let path_configured = shell_block_contains_text_in_files(
        &resolve_default_shell_targets(),
        "managed_rtk",
        "export PATH=",
    )?;
    let hook_configured = claude_settings_hook_matches("headroom-rtk-rewrite.sh")?
        && headroom_rtk_hook_path().exists();
    Ok((path_configured, hook_configured))
}

pub fn apply_client_setup(client_id: &str) -> Result<ClientSetupResult> {
    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();
    let mut state = load_setup_state();
    let state_id = normalized_setup_id(client_id).to_string();

    match client_id {
        "claude_code" => {
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            let mut rtk_updates = ensure_rtk_integrations_for_targets(
                &default_headroom_rtk_path(),
                &default_headroom_managed_python_path(),
                &shell_targets,
            )?;
            let env_block = format!("export ANTHROPIC_BASE_URL={}", HEADROOM_ANTHROPIC_BASE_URL);
            let mut updates = configure_shell_block(&shell_targets, "claude_code", &env_block)?;
            let mut claude_updates =
                configure_claude_settings_env("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
            let mut legacy_updates = remove_legacy_vscode_base_url_keys()?;
            updates.0.append(&mut rtk_updates.0);
            updates.1.append(&mut rtk_updates.1);
            updates.0.append(&mut claude_updates.0);
            updates.1.append(&mut claude_updates.1);
            updates.0.append(&mut legacy_updates.0);
            updates.1.append(&mut legacy_updates.1);
            changed_files.extend(updates.0);
            backup_files.extend(updates.1);
            state
                .managed_shell_files
                .insert(state_id.clone(), serialize_paths(&shell_targets));
        }
        "vscode" => {
            let updates = configure_vscode_settings()?;
            changed_files.extend(updates.0);
            backup_files.extend(updates.1);
        }
        "codex" | "codex_cli" | "codex_gui" => {
            return Err(anyhow!(
                "Codex integration has been disabled. Headroom now focuses on Claude Code."
            ))
        }
        other => return Err(anyhow!("Automatic setup is not supported yet for {other}.",)),
    }

    let configured_at = Utc::now().to_rfc3339();
    state.configured_clients.insert(state_id, configured_at);
    write_setup_state(&state)?;

    let already_configured = changed_files.is_empty();
    let summary = if already_configured {
        "Client was already configured for Headroom.".to_string()
    } else {
        "Client configuration updated to route through Headroom.".to_string()
    };

    let verification = verify_client_setup(client_id)?;

    Ok(ClientSetupResult {
        client_id: client_id.to_string(),
        applied: true,
        already_configured,
        summary,
        changed_files,
        backup_files,
        next_steps: vec![
            "Restart your terminal/editor session to pick up environment changes.".into(),
            "Run one Claude Code prompt and verify activity appears in Headroom.".into(),
        ],
        verification,
    })
}

pub fn verify_client_setup(client_id: &str) -> Result<ClientSetupVerification> {
    let mut checks = Vec::new();
    let mut failures = Vec::new();

    match client_id {
        "claude_code" => {
            let state = load_setup_state();
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            let shell_ok = shell_block_contains_in_files(
                &shell_targets,
                "claude_code",
                "ANTHROPIC_BASE_URL",
                HEADROOM_ANTHROPIC_BASE_URL,
            )?;
            let rtk_path_ok =
                shell_block_contains_text_in_files(&shell_targets, "managed_rtk", "export PATH=")?;
            let claude_settings_ok =
                claude_settings_env_matches("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
            let rtk_hook_ok = claude_settings_hook_matches("headroom-rtk-rewrite.sh")?
                && headroom_rtk_hook_path().exists();

            if shell_ok {
                checks.push(
                    "Found Claude Code ANTHROPIC_BASE_URL export in managed shell block.".into(),
                );
            }
            if rtk_path_ok {
                checks.push("Found Headroom-managed RTK PATH export in shell profiles.".into());
            }
            if claude_settings_ok {
                checks.push(
                    "Found ~/.claude/settings.json env.ANTHROPIC_BASE_URL pointing to Headroom."
                        .into(),
                );
            }
            if rtk_hook_ok {
                checks.push(
                    "Found Headroom-managed RTK Claude hook in ~/.claude/settings.json.".into(),
                );
            }
            if !shell_ok && !claude_settings_ok {
                failures.push(
                    "Claude Code ANTHROPIC_BASE_URL was not found in shell blocks or ~/.claude/settings.json."
                        .into(),
                );
            }
            if !rtk_path_ok {
                failures.push(
                    "Headroom-managed RTK PATH export was not found in shell profiles.".into(),
                );
            }
            if !rtk_hook_ok {
                failures.push(
                    "Headroom-managed RTK Claude hook was not found in ~/.claude/settings.json."
                        .into(),
                );
            }
        }
        "vscode" => {
            let mut delegated = verify_client_setup("claude_code")?;
            delegated.client_id = "vscode".to_string();
            return Ok(delegated);
        }
        "codex" | "codex_cli" | "codex_gui" => {
            return Err(anyhow!(
                "Codex integration has been disabled. Headroom now focuses on Claude Code."
            ))
        }
        other => return Err(anyhow!("Verification is not supported yet for {other}.",)),
    }

    let proxy_reachable = is_headroom_proxy_reachable();
    if proxy_reachable {
        checks.push("Headroom proxy is reachable on 127.0.0.1:6767.".into());
    } else {
        failures.push("Headroom proxy is not reachable on 127.0.0.1:6767.".into());
    }

    Ok(ClientSetupVerification {
        client_id: client_id.to_string(),
        verified: failures.is_empty(),
        proxy_reachable,
        checks,
        failures,
    })
}

pub fn list_client_connectors(
    detected_clients: &[ClientStatus],
) -> Result<Vec<ClientConnectorStatus>> {
    let setup_state = load_setup_state();

    let connectors = MANAGED_CLIENT_SPECS
        .iter()
        .map(|spec| {
            let installed = detected_clients
                .iter()
                .find(|client| client.id == spec.id)
                .map(|client| client.installed)
                .unwrap_or(false);
            let enabled = is_configured(&setup_state, spec.id);
            let verified = if enabled {
                verify_client_setup(spec.id)
                    .map(|result| result.verified)
                    .unwrap_or(false)
            } else {
                false
            };

            ClientConnectorStatus {
                client_id: spec.id.to_string(),
                name: spec.name.to_string(),
                installed,
                enabled,
                verified,
                last_configured_at: configured_timestamp(&setup_state, spec.id),
            }
        })
        .collect();

    Ok(connectors)
}

pub fn disable_client_setup(client_id: &str) -> Result<()> {
    let mut state = load_setup_state();

    match client_id {
        "codex" | "codex_cli" => {
            disable_codex_cli()?;
            disable_codex_gui()?;
        }
        "codex_gui" => {
            disable_codex_gui()?;
        }
        "claude_code" => {
            let shell_targets = resolve_client_shell_targets_for_cleanup(&state, client_id)?;
            remove_shell_block(&shell_targets, "claude_code")?;
            remove_claude_settings_env("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
            let _ = remove_legacy_vscode_base_url_keys()?;
        }
        "vscode" => remove_vscode_connector_keys()?,
        other => {
            return Err(anyhow!(
                "Automatic setup disable is not supported yet for {other}.",
            ))
        }
    }

    match client_id {
        "codex" | "codex_cli" => {
            state.configured_clients.remove("codex");
            state.configured_clients.remove("codex_cli");
            state.configured_clients.remove("codex_gui");
            state.remembered_clients.remove("codex");
            state.remembered_clients.remove("codex_cli");
            state.remembered_clients.remove("codex_gui");
            state.managed_shell_files.remove("codex");
            state.managed_shell_files.remove("codex_cli");
            state.managed_shell_files.remove("codex_gui");
            state.remembered_shell_files.remove("codex");
            state.remembered_shell_files.remove("codex_cli");
            state.remembered_shell_files.remove("codex_gui");
        }
        _ => {
            let state_id = normalized_setup_id(client_id);
            state.configured_clients.remove(state_id);
            state.remembered_clients.remove(state_id);
            state.managed_shell_files.remove(state_id);
            state.remembered_shell_files.remove(state_id);
        }
    }
    write_setup_state(&state)?;
    Ok(())
}

pub fn clear_client_setups() -> Result<()> {
    // Snapshot before disabling — disable_client_setup removes entries from configured_clients.
    let mut state = load_setup_state();
    if !state.configured_clients.is_empty() {
        state.remembered_clients = state.configured_clients.clone();
        state.remembered_shell_files = state.managed_shell_files.clone();
        write_setup_state(&state)?;
    }

    for spec in MANAGED_CLIENT_SPECS {
        let _ = disable_client_setup(spec.id);
    }
    let _ = disable_client_setup("codex_gui");

    Ok(())
}

/// Re-applies setup for all clients that were active at the last pause or quit.
pub fn restore_client_setups() {
    let state = load_setup_state();
    let to_restore: Vec<String> = state.remembered_clients.keys().cloned().collect();
    for client_id in to_restore {
        let _ = apply_client_setup(&client_id);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ClientSetupState {
    configured_clients: BTreeMap<String, String>,
    /// Snapshot of configured_clients taken at last pause/quit, used to restore on next startup.
    #[serde(default)]
    remembered_clients: BTreeMap<String, String>,
    #[serde(default)]
    managed_shell_files: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    remembered_shell_files: BTreeMap<String, Vec<String>>,
}

fn is_configured(state: &ClientSetupState, client_id: &str) -> bool {
    configured_timestamp(state, client_id).is_some()
}

fn configured_timestamp(state: &ClientSetupState, client_id: &str) -> Option<String> {
    let primary = normalized_setup_id(client_id);
    state.configured_clients.get(primary).cloned()
}

fn load_setup_state() -> ClientSetupState {
    let path = setup_state_path();
    if !path.exists() {
        return ClientSetupState::default();
    }

    match std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClientSetupState>(&bytes).ok())
    {
        Some(state) => normalize_setup_state(state),
        None => ClientSetupState::default(),
    }
}

fn normalize_setup_state(mut state: ClientSetupState) -> ClientSetupState {
    state.configured_clients = normalize_setup_entries(state.configured_clients);
    state.remembered_clients = normalize_setup_entries(state.remembered_clients);
    state.managed_shell_files = normalize_shell_file_entries(state.managed_shell_files);
    state.remembered_shell_files = normalize_shell_file_entries(state.remembered_shell_files);
    state
}

fn normalize_setup_entries(mut entries: BTreeMap<String, String>) -> BTreeMap<String, String> {
    entries.remove("codex_cli");
    entries.remove("codex");
    entries.remove("codex_gui");

    entries
}

fn normalize_shell_file_entries(
    mut entries: BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    entries.remove("codex_cli");
    entries.remove("codex");
    entries.remove("codex_gui");

    for files in entries.values_mut() {
        dedupe_strings(files);
    }

    entries
}

fn write_setup_state(state: &ClientSetupState) -> Result<()> {
    let path = setup_state_path();
    let payload = serde_json::to_vec_pretty(state).context("serializing client setup state")?;
    std::fs::write(&path, payload).with_context(|| format!("writing {}", path.display()))
}

fn setup_state_path() -> PathBuf {
    config_file(&app_data_dir(), "client-setup.json")
}

fn default_headroom_root_dir() -> PathBuf {
    app_data_dir().join("headroom")
}

fn default_headroom_rtk_path() -> PathBuf {
    default_headroom_root_dir().join("bin").join("rtk")
}

fn default_headroom_managed_python_path() -> PathBuf {
    default_headroom_root_dir()
        .join("runtime")
        .join("venv")
        .join("bin")
        .join("python3")
}

fn resolve_client_shell_targets(state: &ClientSetupState, client_id: &str) -> Result<Vec<PathBuf>> {
    let state_id = normalized_setup_id(client_id);
    let mut targets = shell_targets_from_state(state.managed_shell_files.get(state_id));
    if targets.is_empty() {
        targets = shell_targets_from_state(state.remembered_shell_files.get(state_id));
    }
    targets.extend(discover_managed_shell_targets(&[
        "claude_code",
        "managed_rtk",
    ])?);

    let default_targets = default_shell_targets_for_family(detect_shell_family());
    if targets.is_empty() {
        targets = default_targets;
    } else {
        for file in default_targets {
            if is_profile_file(&file) {
                targets.push(file);
            }
        }
    }

    Ok(dedupe_paths(targets))
}

fn resolve_client_shell_targets_for_cleanup(
    state: &ClientSetupState,
    client_id: &str,
) -> Result<Vec<PathBuf>> {
    let mut targets = resolve_client_shell_targets(state, client_id)?;
    targets.extend(all_shell_paths());
    Ok(dedupe_paths(targets))
}

fn configure_shell_block(
    shell_targets: &[PathBuf],
    block_id: &str,
    block_body: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut changed = Vec::new();
    let mut backups = Vec::new();

    for file in shell_targets {
        let (did_change, backup) = upsert_managed_block(&file, block_id, block_body)?;
        if did_change {
            changed.push(file.display().to_string());
            if let Some(path) = backup {
                backups.push(path.display().to_string());
            }
        }
    }

    Ok((changed, backups))
}

fn ensure_managed_rtk_on_path(
    rtk_path: &Path,
    shell_targets: &[PathBuf],
) -> Result<(Vec<String>, Vec<String>)> {
    let managed_bin_dir = rtk_path.parent().ok_or_else(|| {
        anyhow!(
            "managed RTK path {} is missing a parent directory",
            rtk_path.display()
        )
    })?;
    let path_value = shell_double_quote(&managed_bin_dir.to_string_lossy());
    configure_shell_block(
        shell_targets,
        "managed_rtk",
        &format!("export PATH=\"{path_value}:$PATH\""),
    )
}

fn ensure_claude_code_rtk_hook(
    managed_rtk_path: &Path,
    managed_python_path: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
    let hook_path = headroom_rtk_hook_path();
    let hook_body = build_headroom_rtk_hook(managed_rtk_path, managed_python_path);
    let (hook_changed, hook_backup) = write_file_if_changed(&hook_path, &hook_body, true)?;
    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();

    if hook_changed {
        changed_files.push(hook_path.display().to_string());
    }
    if let Some(path) = hook_backup {
        backup_files.push(path.display().to_string());
    }

    let (settings_changed, settings_backups) = ensure_claude_settings_hook(&hook_path)?;
    changed_files.extend(settings_changed);
    backup_files.extend(settings_backups);

    Ok((changed_files, backup_files))
}

fn disable_codex_cli() -> Result<()> {
    remove_codex_provider_block()?;
    let _ = remove_codex_toml_key("openai_base_url", HEADROOM_OPENAI_BASE_URL);
    let shell_targets = all_shell_paths();
    let _ = remove_shell_block(&shell_targets, "codex_cli");
    let _ = remove_shell_block(&shell_targets, "codex");
    Ok(())
}

fn disable_codex_gui() -> Result<()> {
    clear_legacy_codex_gui_launch_env()?;
    Ok(())
}

fn clear_legacy_codex_gui_launch_env() -> Result<()> {
    remove_launchctl_env(&["OPENAI_BASE_URL", "OPENAI_API_BASE"])?;
    Ok(())
}

fn configure_vscode_settings() -> Result<(Vec<String>, Vec<String>)> {
    let (mut changed_files, mut backup_files) =
        configure_claude_settings_env("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
    let (legacy_changed, legacy_backups) = remove_legacy_vscode_base_url_keys()?;
    changed_files.extend(legacy_changed);
    backup_files.extend(legacy_backups);
    Ok((changed_files, backup_files))
}

fn remove_vscode_connector_keys() -> Result<()> {
    remove_claude_settings_env("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
    let _ = remove_legacy_vscode_base_url_keys()?;
    Ok(())
}

fn set_json_string(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    expected_value: &str,
) -> bool {
    let next = Value::String(expected_value.to_string());
    match obj.get(key) {
        Some(existing) if existing == &next => false,
        _ => {
            obj.insert(key.to_string(), next);
            true
        }
    }
}

fn remove_json_key_if_matches(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    expected_value: &str,
) -> bool {
    match obj.get(key) {
        Some(Value::String(value)) if value == expected_value => obj.remove(key).is_some(),
        _ => false,
    }
}

fn configure_claude_settings_env(
    env_key: &str,
    env_value: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let settings_path = claude_settings_path();
    let mut content = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        Value::Object(parse_json_object(&raw, &settings_path)?)
    } else {
        Value::Object(Default::default())
    };

    if !content.is_object() {
        content = Value::Object(Default::default());
    }

    let Some(root) = content.as_object_mut() else {
        return Err(anyhow!("unable to write Claude settings"));
    };

    if !root
        .get("env")
        .map(|value| value.is_object())
        .unwrap_or(false)
    {
        root.insert("env".into(), Value::Object(Default::default()));
    }

    let Some(env_obj) = root.get_mut("env").and_then(|value| value.as_object_mut()) else {
        return Err(anyhow!("unable to write Claude env settings"));
    };

    let changed = set_json_string(env_obj, env_key, env_value);
    if !changed {
        return Ok((Vec::new(), Vec::new()));
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let backup = backup_if_exists(&settings_path)?;
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&content).context("serializing Claude settings")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok((
        vec![settings_path.display().to_string()],
        backup
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
    ))
}

fn ensure_claude_settings_hook(hook_path: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let settings_path = claude_settings_path();
    let mut content = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        Value::Object(parse_json_object(&raw, &settings_path)?)
    } else {
        Value::Object(Default::default())
    };

    if !content.is_object() {
        content = Value::Object(Default::default());
    }

    let hook_command = hook_path
        .to_str()
        .ok_or_else(|| anyhow!("hook path contains invalid UTF-8: {}", hook_path.display()))?;
    let already_present = claude_hook_present_in_value(&content, hook_command);
    if already_present {
        return Ok((Vec::new(), Vec::new()));
    }

    let Some(root) = content.as_object_mut() else {
        return Err(anyhow!("unable to write Claude hook settings"));
    };

    if !root
        .get("hooks")
        .map(|value| value.is_object())
        .unwrap_or(false)
    {
        root.insert("hooks".into(), Value::Object(Default::default()));
    }

    let Some(hooks_obj) = root
        .get_mut("hooks")
        .and_then(|value| value.as_object_mut())
    else {
        return Err(anyhow!("unable to write Claude hooks settings"));
    };
    if !hooks_obj
        .get("PreToolUse")
        .map(|value| value.is_array())
        .unwrap_or(false)
    {
        hooks_obj.insert("PreToolUse".into(), Value::Array(Vec::new()));
    }

    let Some(pre_tool_use) = hooks_obj
        .get_mut("PreToolUse")
        .and_then(|value| value.as_array_mut())
    else {
        return Err(anyhow!("unable to write Claude PreToolUse hooks"));
    };

    pre_tool_use.retain(|entry| !entry_contains_hook(entry, "headroom-rtk-rewrite.sh"));
    pre_tool_use.push(serde_json::json!({
        "matcher": "Bash",
        "hooks": [{
            "type": "command",
            "command": hook_command
        }]
    }));

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let backup = backup_if_exists(&settings_path)?;
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&content).context("serializing Claude hook settings")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok((
        vec![settings_path.display().to_string()],
        backup
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
    ))
}

fn remove_claude_settings_env(env_key: &str, expected_value: &str) -> Result<()> {
    let settings_path = claude_settings_path();
    if !settings_path.exists() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut root = parse_json_object(&raw, &settings_path)?;
    let mut changed = false;

    if let Some(Value::Object(env_obj)) = root.get_mut("env") {
        changed |= remove_json_key_if_matches(env_obj, env_key, expected_value);
        if env_obj.is_empty() {
            root.remove("env");
            changed = true;
        }
    }

    if !changed {
        return Ok(());
    }

    let _ = backup_if_exists(&settings_path)?;
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&Value::Object(root))
            .context("serializing Claude settings for connector removal")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok(())
}

fn claude_hook_present_in_value(content: &Value, hook_path: &str) -> bool {
    content
        .get("hooks")
        .and_then(|value| value.get("PreToolUse"))
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|hooks| hooks.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(|command| command.as_str())
                                .map(|command| command == hook_path)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn entry_contains_hook(entry: &Value, hook_fragment: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|hooks| hooks.as_array())
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|command| command.as_str())
                    .map(|command| command.contains(hook_fragment))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn remove_legacy_vscode_base_url_keys() -> Result<(Vec<String>, Vec<String>)> {
    let settings_path = home_dir()
        .join("Library")
        .join("Application Support")
        .join("Code")
        .join("User")
        .join("settings.json");
    if !settings_path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let raw = std::fs::read_to_string(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut obj = parse_json_object(&raw, &settings_path)?;

    let mut changed = false;
    changed |= remove_json_key_if_matches(&mut obj, "openai.baseUrl", HEADROOM_PROXY_URL);
    changed |= remove_json_key_if_matches(&mut obj, "anthropic.baseUrl", HEADROOM_PROXY_URL);
    if !changed {
        return Ok((Vec::new(), Vec::new()));
    }

    let backup = backup_if_exists(&settings_path)?;
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&Value::Object(obj))
            .context("serializing VS Code settings for legacy key cleanup")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok((
        vec![settings_path.display().to_string()],
        backup
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
    ))
}

fn codex_config_toml_path() -> PathBuf {
    home_dir().join(".codex").join("config.toml")
}

fn remove_codex_provider_block() -> Result<()> {
    let path = codex_config_toml_path();
    let _ = remove_managed_block(&path, "codex_cli")?;
    Ok(())
}

fn remove_codex_toml_key(key: &str, expected_value: &str) -> Result<()> {
    let path = codex_config_toml_path();
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let target_line = format!("{key} = \"{expected_value}\"");
    let filtered: Vec<&str> = content
        .lines()
        .filter(|l| l.trim() != target_line)
        .collect();
    if filtered.len() == content.lines().count() {
        return Ok(());
    }
    let _ = backup_if_exists(&path)?;
    let mut result = filtered.join("\n");
    if !result.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    std::fs::write(&path, result).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn remove_launchctl_env(keys: &[&str]) -> Result<()> {
    for key in keys {
        let _ = run_launchctl(&["unsetenv", key]);
    }
    Ok(())
}

fn run_launchctl(args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| format!("running launchctl {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(output);
    }

    Err(anyhow!(
        "launchctl {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn normalized_setup_id(client_id: &str) -> &str {
    match client_id {
        "codex" | "codex_gui" => "codex_cli",
        "vscode" => "claude_code",
        other => other,
    }
}

fn upsert_managed_block(
    file_path: &Path,
    block_id: &str,
    block_body: &str,
) -> Result<(bool, Option<PathBuf>)> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let existing = if file_path.exists() {
        std::fs::read_to_string(file_path)
            .with_context(|| format!("reading {}", file_path.display()))?
    } else {
        String::new()
    };

    let start = format!("# >>> headroom:{block_id} >>>");
    let end = format!("# <<< headroom:{block_id} <<<");
    let block = format!("{start}\n{block_body}\n{end}\n");
    let updated =
        if let (Some(start_idx), Some(end_idx)) = (existing.find(&start), existing.find(&end)) {
            let end_with_marker = end_idx + end.len();
            let mut rebuilt = String::with_capacity(existing.len() + block.len());
            rebuilt.push_str(&existing[..start_idx]);
            rebuilt.push_str(&block);
            if end_with_marker < existing.len() {
                rebuilt.push_str(&existing[end_with_marker..]);
            }
            rebuilt
        } else if existing.trim().is_empty() {
            block
        } else {
            format!("{}\n{}", existing.trim_end(), block)
        };

    if updated == existing {
        return Ok((false, None));
    }

    let backup = backup_if_exists(file_path)?;
    std::fs::write(file_path, updated)
        .with_context(|| format!("writing {}", file_path.display()))?;
    Ok((true, backup))
}

fn write_file_if_changed(
    file_path: &Path,
    content: &str,
    executable: bool,
) -> Result<(bool, Option<PathBuf>)> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let existing = if file_path.exists() {
        Some(
            std::fs::read_to_string(file_path)
                .with_context(|| format!("reading {}", file_path.display()))?,
        )
    } else {
        None
    };

    if existing.as_deref() == Some(content) {
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(file_path)
                .with_context(|| format!("reading {}", file_path.display()))?
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(file_path, permissions)
                .with_context(|| format!("chmod {}", file_path.display()))?;
        }
        return Ok((false, None));
    }

    let backup = backup_if_exists(file_path)?;
    std::fs::write(file_path, content)
        .with_context(|| format!("writing {}", file_path.display()))?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(file_path)
            .with_context(|| format!("reading {}", file_path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(file_path, permissions)
            .with_context(|| format!("chmod {}", file_path.display()))?;
    }

    Ok((true, backup))
}

fn remove_shell_block(shell_targets: &[PathBuf], block_id: &str) -> Result<()> {
    for file in shell_targets {
        remove_managed_block(&file, block_id)?;
    }
    Ok(())
}

fn remove_managed_block(file_path: &Path, block_id: &str) -> Result<bool> {
    if !file_path.exists() {
        return Ok(false);
    }

    let existing = std::fs::read_to_string(file_path)
        .with_context(|| format!("reading {}", file_path.display()))?;
    let start = format!("# >>> headroom:{block_id} >>>");
    let end = format!("# <<< headroom:{block_id} <<<");

    let (Some(start_idx), Some(end_idx)) = (existing.find(&start), existing.find(&end)) else {
        return Ok(false);
    };

    let end_with_marker = end_idx + end.len();
    let tail = existing[end_with_marker..].trim_start_matches('\n');
    let mut rebuilt = String::with_capacity(existing.len());
    rebuilt.push_str(existing[..start_idx].trim_end());
    if !rebuilt.is_empty() && !tail.is_empty() {
        rebuilt.push('\n');
    }
    rebuilt.push_str(tail);
    if !rebuilt.is_empty() && !rebuilt.ends_with('\n') {
        rebuilt.push('\n');
    }

    let _ = backup_if_exists(file_path)?;
    std::fs::write(file_path, rebuilt)
        .with_context(|| format!("writing {}", file_path.display()))?;
    Ok(true)
}

fn backup_if_exists(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let stamp = Utc::now().format("%Y%m%d%H%M%S");
    let backup_path = PathBuf::from(format!("{}.headroom-backup-{}", path.display(), stamp));
    std::fs::copy(path, &backup_path)
        .with_context(|| format!("creating backup {}", backup_path.display()))?;

    // Prune old backups — keep only the 3 most recent for this base path.
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let headroom_prefix = format!("{}.headroom-backup-", file_name);
    let nommer_prefix = format!("{}.nommer-backup-", file_name);
    if let Some(dir) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut backups: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with(&headroom_prefix) || n.starts_with(&nommer_prefix))
                        .unwrap_or(false)
                })
                .collect();
            backups.sort();
            if backups.len() > 3 {
                for old in &backups[..backups.len() - 3] {
                    let _ = std::fs::remove_file(old);
                }
            }
        }
    }

    Ok(Some(backup_path))
}

fn shell_block_contains_in_files(
    shell_targets: &[PathBuf],
    block_id: &str,
    var_name: &str,
    expected_value: &str,
) -> Result<bool> {
    for file in shell_targets {
        if !file.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let start = format!("# >>> headroom:{block_id} >>>");
        let end = format!("# <<< headroom:{block_id} <<<");

        if let (Some(start_idx), Some(end_idx)) = (content.find(&start), content.find(&end)) {
            let block = &content[start_idx..end_idx];
            let expected_line = format!("export {var_name}={expected_value}");
            if block.contains(&expected_line) {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn shell_block_contains_text_in_files(
    shell_targets: &[PathBuf],
    block_id: &str,
    expected_text: &str,
) -> Result<bool> {
    for file in shell_targets {
        if !file.exists() {
            continue;
        }

        let content = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let start = format!("# >>> headroom:{block_id} >>>");
        let end = format!("# <<< headroom:{block_id} <<<");

        if let (Some(start_idx), Some(end_idx)) = (content.find(&start), content.find(&end)) {
            if content[start_idx..end_idx].contains(expected_text) {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn claude_settings_env_matches(env_key: &str, expected_value: &str) -> Result<bool> {
    let path = claude_settings_path();
    if !path.exists() {
        return Ok(false);
    }

    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let content: Value = Value::Object(parse_json_object(&raw, &path)?);
    Ok(matches!(
        content.get("env").and_then(|env| env.get(env_key)),
        Some(Value::String(value)) if value == expected_value
    ))
}

fn claude_settings_hook_matches(hook_fragment: &str) -> Result<bool> {
    let path = claude_settings_path();
    if !path.exists() {
        return Ok(false);
    }

    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let content: Value = Value::Object(parse_json_object(&raw, &path)?);

    Ok(content
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(|hooks| hooks.as_array())
        .map(|entries| {
            entries
                .iter()
                .any(|entry| entry_contains_hook(entry, hook_fragment))
        })
        .unwrap_or(false))
}

fn is_headroom_proxy_reachable() -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };

    ["127.0.0.1", "localhost"].iter().any(|host| {
        client
            .get(format!("http://{host}:6767/health"))
            .send()
            .map(|response| response.status().is_success())
            .unwrap_or(false)
    })
}

fn resolve_default_shell_targets() -> Vec<PathBuf> {
    let mut targets =
        discover_managed_shell_targets(&["managed_rtk", "claude_code"]).unwrap_or_default();
    if targets.is_empty() {
        targets = default_shell_targets_for_family(detect_shell_family());
    }
    dedupe_paths(targets)
}

fn detect_shell_family() -> ShellFamily {
    if let Some(shell_name) = std::env::var_os("SHELL")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| {
            Path::new(&value)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_ascii_lowercase())
        })
    {
        if shell_name.contains("zsh") {
            return ShellFamily::Zsh;
        }
        if shell_name.contains("bash") {
            return ShellFamily::Bash;
        }
        if shell_name == "sh" {
            return ShellFamily::Posix;
        }
    }

    let has_zsh_files = [ZSH_PROFILE_FILE, ZSH_RC_FILE]
        .into_iter()
        .map(shell_path)
        .any(|path| path.exists());
    let has_bash_files = [
        BASH_PROFILE_FILE,
        BASH_LOGIN_FILE,
        POSIX_PROFILE_FILE,
        BASH_RC_FILE,
    ]
    .into_iter()
    .map(shell_path)
    .any(|path| path.exists());

    match (has_zsh_files, has_bash_files) {
        (true, false) => ShellFamily::Zsh,
        (false, true) => ShellFamily::Bash,
        _ if cfg!(target_os = "macos") => ShellFamily::Zsh,
        _ => ShellFamily::Bash,
    }
}

fn default_shell_targets_for_family(shell_family: ShellFamily) -> Vec<PathBuf> {
    match shell_family {
        ShellFamily::Zsh => {
            dedupe_paths(vec![shell_path(ZSH_PROFILE_FILE), shell_path(ZSH_RC_FILE)])
        }
        ShellFamily::Bash => dedupe_paths(vec![
            preferred_bash_profile_path(),
            shell_path(BASH_RC_FILE),
        ]),
        ShellFamily::Posix => vec![shell_path(POSIX_PROFILE_FILE)],
    }
}

fn preferred_bash_profile_path() -> PathBuf {
    [BASH_PROFILE_FILE, BASH_LOGIN_FILE, POSIX_PROFILE_FILE]
        .into_iter()
        .map(shell_path)
        .find(|path| path.exists())
        .unwrap_or_else(|| shell_path(BASH_PROFILE_FILE))
}

fn discover_managed_shell_targets(block_ids: &[&str]) -> Result<Vec<PathBuf>> {
    let mut discovered = Vec::new();
    for file in all_shell_paths() {
        for block_id in block_ids {
            if file_has_managed_block(&file, block_id)? {
                discovered.push(file.clone());
                break;
            }
        }
    }
    Ok(dedupe_paths(discovered))
}

fn shell_targets_from_state(serialized_paths: Option<&Vec<String>>) -> Vec<PathBuf> {
    serialized_paths
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect::<Vec<_>>()
}

fn serialize_paths(paths: &[PathBuf]) -> Vec<String> {
    let mut serialized = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    dedupe_strings(&mut serialized);
    serialized
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        let key = path.display().to_string();
        if seen.insert(key) {
            deduped.push(path);
        }
    }
    deduped
}

fn dedupe_strings(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn all_shell_paths() -> Vec<PathBuf> {
    ALL_SHELL_FILES.into_iter().map(shell_path).collect()
}

fn is_profile_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(ZSH_PROFILE_FILE | BASH_PROFILE_FILE | BASH_LOGIN_FILE | POSIX_PROFILE_FILE)
    )
}

fn file_has_managed_block(file_path: &Path, block_id: &str) -> Result<bool> {
    if !file_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(file_path)
        .with_context(|| format!("reading {}", file_path.display()))?;
    let start = format!("# >>> headroom:{block_id} >>>");
    let end = format!("# <<< headroom:{block_id} <<<");
    Ok(content.contains(&start) && content.contains(&end))
}

fn shell_path(name: &str) -> PathBuf {
    home_dir().join(name)
}

fn claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

fn headroom_rtk_hook_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("hooks")
        .join("headroom-rtk-rewrite.sh")
}

fn shell_double_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn build_headroom_rtk_hook(managed_rtk_path: &Path, managed_python_path: &Path) -> String {
    let rtk = shell_double_quote(&managed_rtk_path.to_string_lossy());
    let python = shell_double_quote(&managed_python_path.to_string_lossy());

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

HEADROOM_RTK="{rtk}"
HEADROOM_PYTHON="{python}"

if [ ! -x "$HEADROOM_RTK" ] || [ ! -x "$HEADROOM_PYTHON" ]; then
  exit 0
fi

INPUT="$(cat)"
if [ -z "$INPUT" ]; then
  exit 0
fi

CMD="$("$HEADROOM_PYTHON" -c 'import json, sys; data = json.load(sys.stdin); cmd = data.get("tool_input", {{}}).get("command", ""); print(cmd if isinstance(cmd, str) else "")' <<<"$INPUT" 2>/dev/null || true)"
if [ -z "$CMD" ]; then
  exit 0
fi

REWRITTEN="$("$HEADROOM_RTK" rewrite "$CMD" 2>/dev/null || true)"
if [ -z "$REWRITTEN" ] || [ "$CMD" = "$REWRITTEN" ]; then
  exit 0
fi

HEADROOM_RTK_REWRITTEN="$REWRITTEN" "$HEADROOM_PYTHON" -c 'import json, os, sys; data = json.load(sys.stdin); tool_input = data.get("tool_input"); 
if not isinstance(tool_input, dict):
    sys.exit(0)
updated = dict(tool_input)
updated["command"] = os.environ["HEADROOM_RTK_REWRITTEN"]
json.dump({{"hookSpecificOutput": {{"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "Headroom RTK auto-rewrite", "updatedInput": updated}}}}, sys.stdout)' <<<"$INPUT" 2>/dev/null || exit 0
"#
    )
}

fn home_dir() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir())
}

fn detect_claude_code_client(configured: bool) -> ClientStatus {
    let executable = claude_code_candidate_paths()
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| find_on_path(&["claude", "claude-code"]));

    if let Some(path) = executable {
        return ClientStatus {
            id: "claude_code".into(),
            name: "Claude Code".into(),
            installed: true,
            configured,
            health: if configured {
                ClientHealth::Healthy
            } else {
                ClientHealth::Attention
            },
            notes: if configured {
                vec![
                    format!("Detected at {}", path.display()),
                    "Configured by Headroom.".into(),
                ]
            } else {
                vec![
                    format!("Detected at {}", path.display()),
                    "Route Claude Code through Headroom's localhost proxy so prompts stay lean."
                        .into(),
                ]
            },
        };
    }

    if claude_code_user_state_exists(&home_dir()) {
        return ClientStatus {
            id: "claude_code".into(),
            name: "Claude Code".into(),
            installed: true,
            configured,
            health: if configured {
                ClientHealth::Healthy
            } else {
                ClientHealth::Attention
            },
            notes: if configured {
                vec![
                    "Detected Claude Code data in ~/.claude.".into(),
                    "Configured by Headroom.".into(),
                ]
            } else {
                vec![
                    "Detected Claude Code data in ~/.claude.".into(),
                    "Claude Code appears to be installed, but Headroom could not resolve the CLI from its current launch PATH. This is common when Headroom starts outside your shell and Claude was installed via nvm or another user-local toolchain.".into(),
                ]
            },
        };
    }

    ClientStatus {
        id: "claude_code".into(),
        name: "Claude Code".into(),
        installed: false,
        configured: false,
        health: ClientHealth::NotDetected,
        notes: vec!["Not detected on this machine yet.".into()],
    }
}

fn claude_code_candidate_paths() -> Vec<PathBuf> {
    let home = home_dir();
    let binary_names = ["claude", "claude-code"];
    let mut candidates = vec![
        PathBuf::from("/usr/local/bin/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude-code"),
        PathBuf::from("/opt/homebrew/bin/claude-code"),
    ];

    let user_bin_dirs = vec![
        home.join(".local").join("bin"),
        home.join("bin"),
        home.join(".npm-global").join("bin"),
        home.join(".yarn").join("bin"),
        home.join(".config")
            .join("yarn")
            .join("global")
            .join("node_modules")
            .join(".bin"),
        home.join(".volta").join("bin"),
        home.join(".bun").join("bin"),
        home.join(".asdf").join("shims"),
        home.join(".mise").join("shims"),
        home.join(".nodenv").join("shims"),
    ];

    candidates.extend(binary_candidates_in_dirs(&user_bin_dirs, &binary_names));
    candidates.extend(nvm_binary_candidates(&home, &binary_names));
    dedupe_paths(candidates)
}

fn binary_candidates_in_dirs(directories: &[PathBuf], binary_names: &[&str]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for directory in directories {
        for binary_name in binary_names {
            candidates.push(directory.join(binary_name));
            if cfg!(windows) {
                for ext in windows_path_extensions() {
                    candidates.push(directory.join(format!("{binary_name}{ext}")));
                }
            }
        }
    }
    candidates
}

fn nvm_binary_candidates(home: &Path, binary_names: &[&str]) -> Vec<PathBuf> {
    let mut candidates = binary_candidates_in_dirs(
        &[home.join(".nvm").join("current").join("bin")],
        binary_names,
    );
    let versions_dir = home.join(".nvm").join("versions").join("node");
    let Ok(entries) = std::fs::read_dir(versions_dir) else {
        return candidates;
    };

    let mut version_bins = entries
        .flatten()
        .map(|entry| entry.path().join("bin"))
        .collect::<Vec<_>>();
    version_bins.sort();
    version_bins.reverse();
    candidates.extend(binary_candidates_in_dirs(&version_bins, binary_names));
    candidates
}

fn claude_code_user_state_exists(home: &Path) -> bool {
    let claude_root = home.join(".claude");
    claude_root.join("settings.json").exists()
        || claude_root.join("projects").exists()
        || claude_root.join("sessions").exists()
        || claude_root.join("statsig").exists()
}

fn parse_json_object(raw: &str, path: &Path) -> Result<serde_json::Map<String, Value>> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => json5::from_str(raw).with_context(|| {
            format!(
                "parsing {} failed (JSON/JSON5); refusing to overwrite potentially valid user settings",
                path.display()
            )
        })?,
    };
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("{} must contain a top-level JSON object", path.display()))
}

fn find_on_path(binary_names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_on_path_entries(std::env::split_paths(&path_var), binary_names)
}

fn find_on_path_entries<I>(path_entries: I, binary_names: &[&str]) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    for entry in path_entries {
        for binary_name in binary_names {
            let candidate = entry.join(binary_name);
            if candidate.exists() {
                return Some(candidate);
            }

            if cfg!(windows) {
                for ext in windows_path_extensions() {
                    let with_ext = entry.join(format!("{binary_name}{ext}"));
                    if with_ext.exists() {
                        return Some(with_ext);
                    }
                }
            }
        }
    }

    None
}

fn windows_path_extensions() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD").to_os_string())
        .to_string_lossy()
        .split(';')
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.starts_with('.') {
                value.to_string()
            } else {
                format!(".{value}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{
        build_headroom_rtk_hook, claude_code_user_state_exists, claude_hook_present_in_value,
        default_shell_targets_for_family, entry_contains_hook, find_on_path_entries,
        normalize_setup_state, normalized_setup_id, nvm_binary_candidates, parse_json_object,
        serialize_paths, shell_double_quote, ClientSetupState, ShellFamily,
    };

    #[test]
    fn normalize_setup_state_removes_legacy_codex_entries() {
        let state = ClientSetupState {
            configured_clients: BTreeMap::from([
                ("claude_code".into(), "2026-03-27T10:00:00Z".into()),
                ("codex_cli".into(), "2026-03-27T10:01:00Z".into()),
                ("codex_gui".into(), "2026-03-27T10:02:00Z".into()),
            ]),
            remembered_clients: BTreeMap::from([
                ("codex".into(), "2026-03-27T10:03:00Z".into()),
                ("claude_code".into(), "2026-03-27T10:04:00Z".into()),
            ]),
            managed_shell_files: BTreeMap::from([
                ("claude_code".into(), vec!["/Users/test/.zprofile".into()]),
                ("codex_gui".into(), vec!["/Users/test/.zshrc".into()]),
            ]),
            remembered_shell_files: BTreeMap::from([
                ("codex".into(), vec!["/Users/test/.bash_profile".into()]),
                ("claude_code".into(), vec!["/Users/test/.bashrc".into()]),
            ]),
        };

        let normalized = normalize_setup_state(state);

        assert_eq!(normalized.configured_clients.len(), 1);
        assert!(normalized.configured_clients.contains_key("claude_code"));
        assert_eq!(normalized.remembered_clients.len(), 1);
        assert!(normalized.remembered_clients.contains_key("claude_code"));
        assert_eq!(normalized.managed_shell_files.len(), 1);
        assert!(normalized.managed_shell_files.contains_key("claude_code"));
        assert_eq!(normalized.remembered_shell_files.len(), 1);
        assert!(normalized
            .remembered_shell_files
            .contains_key("claude_code"));
    }

    #[test]
    fn parse_json_object_accepts_json5_but_rejects_non_objects() {
        let parsed = parse_json_object(
            "{ unquoted: 'value', trailing: true, }",
            Path::new("settings.json"),
        )
        .expect("json5 object should parse");
        assert_eq!(
            parsed.get("unquoted").and_then(|value| value.as_str()),
            Some("value")
        );
        assert_eq!(
            parsed.get("trailing").and_then(|value| value.as_bool()),
            Some(true)
        );

        let err =
            parse_json_object("[]", Path::new("settings.json")).expect_err("arrays are rejected");
        assert!(err
            .to_string()
            .contains("must contain a top-level JSON object"));
    }

    #[test]
    fn setup_aliases_map_to_current_primary_ids() {
        assert_eq!(normalized_setup_id("codex"), "codex_cli");
        assert_eq!(normalized_setup_id("codex_gui"), "codex_cli");
        assert_eq!(normalized_setup_id("vscode"), "claude_code");
        assert_eq!(normalized_setup_id("claude_code"), "claude_code");
    }

    #[test]
    fn shell_double_quote_escapes_shell_sensitive_characters() {
        let escaped = shell_double_quote("path with spaces/$HOME/\"quoted\"`cmd`\\tail");
        assert_eq!(
            escaped,
            "path with spaces/\\$HOME/\\\"quoted\\\"\\`cmd\\`\\\\tail"
        );
    }

    #[test]
    fn shell_targets_include_profile_and_rc_for_supported_shells() {
        let zsh_targets = default_shell_targets_for_family(ShellFamily::Zsh);
        let bash_targets = default_shell_targets_for_family(ShellFamily::Bash);

        assert!(zsh_targets.iter().any(|path| path.ends_with(".zprofile")));
        assert!(zsh_targets.iter().any(|path| path.ends_with(".zshrc")));
        assert!(bash_targets.iter().any(|path| {
            path.ends_with(".bash_profile")
                || path.ends_with(".bash_login")
                || path.ends_with(".profile")
        }));
        assert!(bash_targets.iter().any(|path| path.ends_with(".bashrc")));
    }

    #[test]
    fn serialize_paths_dedupes_repeated_entries() {
        let serialized = serialize_paths(&[
            PathBuf::from("/Users/test/.zprofile"),
            PathBuf::from("/Users/test/.zprofile"),
            PathBuf::from("/Users/test/.zshrc"),
        ]);

        assert_eq!(
            serialized,
            vec![
                "/Users/test/.zprofile".to_string(),
                "/Users/test/.zshrc".to_string()
            ]
        );
    }

    #[test]
    fn generated_rtk_hook_uses_escaped_paths_and_rewrite_reason() {
        let hook = build_headroom_rtk_hook(
            Path::new("/tmp/head room/bin/rtk"),
            Path::new("/tmp/head room/runtime/$python"),
        );

        assert!(hook.contains("HEADROOM_RTK=\"/tmp/head room/bin/rtk\""));
        assert!(hook.contains("HEADROOM_PYTHON=\"/tmp/head room/runtime/\\$python\""));
        assert!(hook.contains("Headroom RTK auto-rewrite"));
        assert!(hook.contains("\"updatedInput\": updated"));
    }

    #[test]
    fn hook_detection_finds_nested_hook_commands() {
        let hook_path = "/Users/test/.claude/hooks/headroom-rtk-rewrite.sh";
        let content = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "bash",
                        "hooks": [
                            { "type": "command", "command": hook_path }
                        ]
                    }
                ]
            }
        });

        assert!(claude_hook_present_in_value(&content, hook_path));
        assert!(entry_contains_hook(
            &content["hooks"]["PreToolUse"][0],
            "headroom-rtk-rewrite.sh"
        ));
        assert!(!entry_contains_hook(
            &json!({ "hooks": [] }),
            "headroom-rtk-rewrite.sh"
        ));
    }

    #[test]
    fn nvm_binary_candidates_include_installed_versions() {
        let home = unique_temp_dir("headroom-nvm-detect");
        let version_bin = home
            .join(".nvm")
            .join("versions")
            .join("node")
            .join("v22.17.1")
            .join("bin");
        fs::create_dir_all(&version_bin).expect("create nvm bin");
        fs::write(version_bin.join("claude"), "").expect("write fake claude binary");

        let candidates = nvm_binary_candidates(&home, &["claude"]);

        assert!(candidates
            .iter()
            .any(|candidate| candidate == &version_bin.join("claude")));

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn path_lookup_scans_supplied_entries() {
        let home = unique_temp_dir("headroom-path-detect");
        let bin_dir = home.join("custom-bin");
        fs::create_dir_all(&bin_dir).expect("create custom bin");
        fs::write(bin_dir.join("claude"), "").expect("write fake claude binary");

        let detected = find_on_path_entries(vec![bin_dir.clone()], &["claude"]);

        assert_eq!(detected, Some(bin_dir.join("claude")));

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn claude_user_state_detection_accepts_settings_or_projects() {
        let home = unique_temp_dir("headroom-claude-home");
        let claude_root = home.join(".claude");
        fs::create_dir_all(&claude_root).expect("create claude root");
        assert!(!claude_code_user_state_exists(&home));

        fs::write(claude_root.join("settings.json"), "{}").expect("write settings");
        assert!(claude_code_user_state_exists(&home));

        let _ = fs::remove_dir_all(home);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
