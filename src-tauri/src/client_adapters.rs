use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{
    ClientConnectorStatus, ClientHealth, ClientSetupResult, ClientSetupVerification, ClientStatus,
};
use crate::state::{UpstreamOverride, UpstreamOverrideMode};
use crate::storage::{app_data_dir, config_file};

// Raw proxy base — use provider-specific constants below when configuring client endpoints.
const HEADROOM_PROXY_URL: &str = "http://127.0.0.1:6767";
const HEADROOM_ANTHROPIC_BASE_URL: &str = "http://127.0.0.1:6767";
// Companion to ANTHROPIC_BASE_URL. With a custom base URL and ENABLE_TOOL_SEARCH
// unset, Claude Code stops deferring MCP/system tool schemas behind its
// server-side Tool Search Tool and front-loads every tool definition into the
// local context window (issue #746). A heavy MCP setup then spends tens of
// thousands of tokens per turn on tool schemas alone, and small sessions fall
// into an auto-compact loop. `headroom wrap claude` sets this; the settings.json
// wiring must too. We write it only when unset so a user's own value wins.
const HEADROOM_ENABLE_TOOL_SEARCH_KEY: &str = "ENABLE_TOOL_SEARCH";
const HEADROOM_ENABLE_TOOL_SEARCH_VALUE: &str = "true";
// Claude Code hides /remote-control whenever ANTHROPIC_BASE_URL is not
// api.anthropic.com (pure host compare, loopback included), so a Headroom-routed
// session can never turn it on. The /remote-control command Headroom installs
// asks the session to exit, and the `claude` shell function relaunches the SAME
// session by id with this `--settings` layer, which outranks the settings.json
// env for that one process. (`ANTHROPIC_BASE_URL= claude` does not work: the
// settings.json env beats an empty process env.)
const CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE: &str =
    r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.anthropic.com"}}"#;
const CLAUDE_REMOTE_CONTROL_COMMAND_MARKER: &str =
    "<!-- managed by Headroom Desktop -- do not edit -->";
const HEADROOM_OPENAI_BASE_URL: &str = "http://127.0.0.1:6767/v1";
const HEADROOM_GROK_PROXY_BASE_URL: &str = "http://127.0.0.1:6767/v1";
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

const MANAGED_CLIENT_SPECS: [ManagedClientSpec; 4] = [
    ManagedClientSpec {
        id: "claude_code",
        name: "Claude Code",
    },
    ManagedClientSpec {
        id: "codex",
        name: "ChatGPT Codex",
    },
    ManagedClientSpec {
        id: "grok_build",
        name: "Grok Build",
    },
    ManagedClientSpec {
        id: "opencode",
        name: "OpenCode",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    Zsh,
    Bash,
    Posix,
}

pub fn detect_clients() -> Vec<ClientStatus> {
    let setup_state = load_setup_state();

    vec![
        detect_claude_code_client(is_configured(&setup_state, "claude_code")),
        detect_codex_client(is_configured(&setup_state, "codex")),
        detect_grok_build_client(is_configured(&setup_state, "grok_build")),
        detect_opencode_client(is_configured(&setup_state, "opencode")),
    ]
}

pub fn ensure_rtk_integrations(
    managed_rtk_path: &Path,
    managed_python_path: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
    let _setup = setup_write_lock();
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
    // Respect the user's opt-out so bootstrap, restore, and client setup don't
    // silently re-add the PATH export and Claude Code hook after they've been
    // turned off via the tool status toggle. Also skip when the binary is absent
    // (not installed / uninstalled) so we never write integrations pointing at a
    // missing rtk.
    if is_rtk_disabled() || !managed_rtk_path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();

    let mut path_updates =
        shell_step_best_effort(ensure_managed_rtk_on_path(managed_rtk_path, shell_targets))?
            .unwrap_or_default();
    let mut hook_updates = ensure_claude_code_rtk_hook(managed_rtk_path, managed_python_path)?;
    changed_files.append(&mut path_updates.0);
    backup_files.append(&mut path_updates.1);
    changed_files.append(&mut hook_updates.0);
    backup_files.append(&mut hook_updates.1);

    // Codex has no PreToolUse-style hook, so the auto-rewrite can't be wired the
    // way it is for Claude Code. Mirror the MarkItDown approach: drop a managed
    // `~/.codex/AGENTS.md` nudge telling Codex to route shell commands through
    // the managed `rtk` binary (which is already on PATH via the block above).
    if is_codex_enabled() {
        let agents = rtk_codex_agents_path();
        let (codex_changed, codex_backup) =
            upsert_nudge_block(&agents, "rtk", &build_rtk_codex_nudge(managed_rtk_path))?;
        if codex_changed {
            changed_files.push(agents.display().to_string());
        }
        if let Some(path) = codex_backup {
            backup_files.push(path.display().to_string());
        }
    }

    Ok((changed_files, backup_files))
}

fn rtk_codex_agents_path() -> PathBuf {
    codex_home().join("AGENTS.md")
}

/// Codex nudge: Codex has no command-rewrite hook, so it routes shell commands
/// through the managed `rtk` binary by being told to prefix them with it.
fn build_rtk_codex_nudge(managed_rtk_path: &Path) -> String {
    let bin = shell_word(managed_rtk_path);
    format!(
        "## Token-saving shell commands (Headroom RTK)\n\
         Run shell commands through RTK to get compact, token-optimized output:\n\
         prefix the command with `{bin} ` (for example `{bin} git status`,\n\
         `{bin} ls -la`, `{bin} cargo build`). RTK compacts output, so do NOT\n\
         use it when you need verbatim text: reading or grepping code you are\n\
         about to edit or patch (RTK grep strips indentation and truncates long\n\
         lines), or `git diff --check` (RTK drops its whitespace report). Run\n\
         those raw. Everything else (status, logs, builds, tests, listings) is\n\
         safe to prefix."
    )
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

/// Serialises the public writers of client-setup.json and of the client configs
/// setup rewrites (~/.claude/settings.json, shell rc files). Each one loads the
/// file, changes its part and writes the whole thing back, and they run on
/// different threads: launch restore, the warm-runtime RTK/MarkItDown refresh
/// and the UI toggles each wrote a stale copy over the others' changes (a lost
/// configured_clients stamp, preserved gateway URL, opt-out flag or hook).
/// Only the public entry points take it, never the helpers they share, so no
/// thread takes it twice.
// ponytail: one global lock held across a whole apply (up to seconds while
// Codex holds its thread DB); per-file locks if a toggle ever visibly waits.
static SETUP_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup_write_lock() -> std::sync::MutexGuard<'static, ()> {
    SETUP_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// True when the user turned RTK off via the tool status toggle.
pub fn is_rtk_disabled() -> bool {
    load_setup_state().rtk_disabled
}

/// True when the user turned auto-learning off in the Optimize view. The proxy
/// then runs without the passive traffic-learning flags; manual Learn scans are
/// unaffected.
pub fn is_auto_learn_disabled() -> bool {
    load_setup_state().auto_learn_disabled
}

/// Persist the auto-learning opt-out. Only read when the proxy is spawned, so
/// the caller restarts the backend for it to take effect.
pub fn set_auto_learn_enabled(enabled: bool) -> Result<()> {
    let _setup = setup_write_lock();
    let mut state = load_setup_state();
    state.auto_learn_disabled = !enabled;
    write_setup_state(&state)
}

/// True when the user turned usage analytics and crash reports off.
pub fn is_usage_data_disabled() -> bool {
    load_setup_state().usage_data_disabled
}

pub fn set_usage_data_enabled(enabled: bool) -> Result<()> {
    let _setup = setup_write_lock();
    let mut state = load_setup_state();
    state.usage_data_disabled = !enabled;
    write_setup_state(&state)
}

/// True when the user turned the Claude Code savings statusline off.
pub fn is_statusline_disabled() -> bool {
    load_setup_state().statusline_disabled
}

/// Persist the statusline opt-out and apply it now: install it when Claude
/// Code routes through Headroom, remove it otherwise.
pub fn set_statusline_enabled(enabled: bool) -> Result<()> {
    let _setup = setup_write_lock();
    let mut state = load_setup_state();
    let was_disabled = state.statusline_disabled;
    state.statusline_disabled = !enabled;
    write_setup_state(&state)?;
    let applied = if enabled && is_claude_code_enabled() {
        ensure_claude_statusline().map(|_| ())
    } else {
        remove_claude_statusline()
    };
    // A failed apply keeps the old flag: the toggle reverts in the UI, and the
    // flag must not claim the opposite of what is installed after a restart.
    if let Err(err) = applied {
        state.statusline_disabled = was_disabled;
        let _ = write_setup_state(&state);
        return Err(err);
    }
    // The editor status bar extension goes with it, but only here and on
    // Headroom's uninstall: remove_claude_statusline also runs on every quit.
    // Not an error to the toggle: the flag and the statusline are already off,
    // and failing here left the UI showing "on" over a disabled state.
    // Released first: the uninstall runs editor CLIs (60s timeout each), and
    // a quit or pause meanwhile blocks on this lock on the UI thread.
    drop(_setup);
    if !enabled {
        if let Err(err) = crate::vscode_statusbar::uninstall() {
            log::warn!("statusline disabled, but the editor status bar extension stayed: {err:#}");
        }
    }
    Ok(())
}

/// Enable or disable RTK from the tool status toggle. Disabling tears down the
/// RTK PATH export, the Claude Code hook, and the Codex AGENTS.md nudge (without
/// touching `ANTHROPIC_BASE_URL` routing) and persists the opt-out so bootstrap
/// won't re-add them. Enabling clears the flag and re-applies the integrations.
pub fn set_rtk_enabled(
    enabled: bool,
    managed_rtk_path: &Path,
    managed_python_path: &Path,
) -> Result<()> {
    let _setup = setup_write_lock();
    let mut state = load_setup_state();
    state.rtk_disabled = !enabled;
    write_setup_state(&state)?;

    if enabled {
        ensure_rtk_integrations_for_targets(
            managed_rtk_path,
            managed_python_path,
            &resolve_default_shell_targets(),
        )?;
    } else {
        let shell_targets = resolve_client_shell_targets_for_cleanup(&state, "claude_code")?;
        remove_shell_block(&shell_targets, "managed_rtk")?;
        // Only RTK's entry: the MarkItDown Read hook is its own add-on.
        for settings_path in claude_settings_candidates() {
            let _ = remove_pre_tool_use_markers(&settings_path, &["headroom-rtk-rewrite.sh"]);
        }
        let hook_path = headroom_rtk_hook_path();
        if hook_path.exists() {
            let _ = std::fs::remove_file(&hook_path);
        }
        let _ = remove_managed_block(&rtk_codex_agents_path(), "rtk");
    }

    Ok(())
}

/// Raw OS codes anywhere in the chain: from `io::Error` sources, and from the
/// "(os error N)" suffix `atomic_write` bakes into its message text instead of
/// carrying a source (see there, RUST-77). Without the text half, nothing an
/// `atomic_write` caller returns ever downcasts, so a Windows ERROR_ACCESS_DENIED
/// on a shell-profile tmp write reached Sentry as an Error (RUST-D2) past the
/// `is_permission_denied` exclusion built for exactly that.
fn os_error_codes(err: &anyhow::Error) -> Vec<i32> {
    err.chain()
        .flat_map(|cause| {
            let from_io = cause
                .downcast_ref::<std::io::Error>()
                .and_then(|io| io.raw_os_error());
            let from_text = cause
                .to_string()
                .rsplit_once("(os error ")
                .and_then(|(_, rest)| rest.trim_end_matches(')').trim().parse().ok());
            from_io.into_iter().chain(from_text)
        })
        .collect()
}

/// EPERM (1) and EACCES (13) on unix; ERROR_ACCESS_DENIED (5) on Windows.
#[cfg(unix)]
const PERMISSION_DENIED_OS_ERRORS: &[i32] = &[1, 13];
#[cfg(windows)]
const PERMISSION_DENIED_OS_ERRORS: &[i32] = &[5];

/// True when the error chain carries a filesystem permission denial, i.e. an
/// unwritable target -- an environment issue, not an app bug.
pub fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    }) || os_error_codes(err)
        .iter()
        .any(|code| PERMISSION_DENIED_OS_ERRORS.contains(code))
}

/// Raw OS codes for a full disk. ErrorKind::StorageFull isn't stable, so match
/// the platform's codes: ENOSPC (28) on macOS/Linux; ERROR_HANDLE_DISK_FULL (39)
/// and ERROR_DISK_FULL (112) on Windows.
#[cfg(unix)]
const NO_SPACE_OS_ERRORS: &[i32] = &[28];
#[cfg(windows)]
const NO_SPACE_OS_ERRORS: &[i32] = &[39, 112];

/// True when the error chain contains a filesystem "no space left on device" --
/// a full disk, an environment issue not an app bug, same class as
/// `is_permission_denied`.
pub fn is_no_space(err: &anyhow::Error) -> bool {
    os_error_codes(err)
        .iter()
        .any(|code| NO_SPACE_OS_ERRORS.contains(code))
}

/// True when the error chain contains an io InvalidData -- in practice a
/// `read_to_string` on a file that isn't valid UTF-8 (RUST-5X: a latin-1
/// ~/.bashrc). Rewriting such a file would mangle the user's own bytes, so the
/// step that wanted to rewrite it is skipped, same class as a locked file.
pub fn is_invalid_utf8(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::InvalidData)
    })
}

/// Runs a shell-profile write step, tolerating a profile we can't safely rewrite
/// (a read-only ~/.zshrc -> os error 13, or a non-UTF-8 ~/.bashrc). The env that
/// actually routes a client lives in app-owned config (~/.claude/settings.json,
/// ~/.codex/config.toml), so an untouchable shell file costs terminal
/// convenience, not core routing. Returns `Ok(None)` when the step was skipped
/// for that reason.
fn shell_step_best_effort(
    step: Result<(Vec<String>, Vec<String>)>,
) -> Result<Option<(Vec<String>, Vec<String>)>> {
    match step {
        Ok(updates) => Ok(Some(updates)),
        Err(err) if is_permission_denied(&err) || is_invalid_utf8(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn apply_client_setup(client_id: &str) -> Result<ClientSetupResult> {
    // Every path that wires a client lands here (Resume, a provider save's
    // restart, the Connectors page, the self-heal), and each would hand the
    // unidentified 6767 holder this user's credentials. Only
    // `rewire_clients_after_port_reclaimed` wires them back, after the port is ours.
    if clients_unwired_for_port_holder() {
        return Err(anyhow!(PORT_HOLDER_REFUSAL));
    }
    let first = apply_client_setup_once(client_id)?;
    if first.verification.verified {
        return Ok(first);
    }
    // Apply-ok-but-verify-miss is a lost-update race (RUST-3W): a concurrent
    // read-modify-write on the same file — the MCP registrar re-run at boot
    // rewrites ~/.codex/config.toml non-atomically, Codex itself rewrites it on
    // exit — can clobber a just-written block before verification reads it
    // back. Re-apply once; a persistent failure still returns unverified and
    // reaches Sentry.
    let mut second = apply_client_setup_once(client_id)?;
    for file in first.changed_files {
        if !second.changed_files.contains(&file) {
            second.changed_files.push(file);
        }
    }
    for file in first.backup_files {
        if !second.backup_files.contains(&file) {
            second.backup_files.push(file);
        }
    }
    second.already_configured &= first.already_configured;
    Ok(second)
}

fn apply_client_setup_once(client_id: &str) -> Result<ClientSetupResult> {
    let _setup = setup_write_lock();
    // Again under the lock: an apply that passed the check above and then
    // waited here while `unwire_clients_for_port_holder` ran would wire the
    // client straight back to the holder, and that unwire runs only once.
    if clients_unwired_for_port_holder() {
        return Err(anyhow!(PORT_HOLDER_REFUSAL));
    }
    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();
    let mut state = load_setup_state();
    let state_id = normalized_setup_id(client_id).to_string();
    let mut shell_unwritable = false;
    let mut replaced_base_url = None;

    match client_id {
        // One settings.json write for the whole arm; see `coalesce_writes`.
        "claude_code" => coalesce_writes(claude_settings_path(), || -> Result<()> {
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            // Critical, app-owned writes first: the ~/.claude/settings.json env is
            // what actually routes Claude Code through Headroom. Do it before the
            // shell profile so a locked ~/.zshrc can't block core setup.
            let (changed, backups, replaced) = configure_claude_base_url()?;
            let mut updates = (changed, backups);
            if let Some(original) = replaced {
                // A custom gateway/proxy URL was routing Claude before us:
                // remember it for restore-on-disable and tell the caller so
                // the UI can inform the user their routing changed.
                state
                    .preserved_base_urls
                    .insert(state_id.clone(), original.clone());
                replaced_base_url = Some(original);
                // Persist now: `coalesce_writes` lands our URL in settings.json
                // even when a later step fails, and the gateway would otherwise
                // be lost for good.
                write_setup_state(&state)?;
            }
            // Ride ENABLE_TOOL_SEARCH alongside the base URL so Claude Code keeps
            // deferring tool schemas (issue #746). If-absent so a user's own value
            // wins.
            let mut tool_search = configure_claude_settings_env_if_absent(
                HEADROOM_ENABLE_TOOL_SEARCH_KEY,
                HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
            )?;
            updates.0.append(&mut tool_search.0);
            updates.1.append(&mut tool_search.1);
            let mut legacy_updates = remove_legacy_vscode_base_url_keys();
            updates.0.append(&mut legacy_updates.0);
            updates.1.append(&mut legacy_updates.1);

            // Loud-fail guard so a closed app or lost ANTHROPIC_BASE_URL routing
            // surfaces in Claude instead of silently hitting Anthropic directly.
            let mut guard = ensure_claude_guard_hook()?;
            updates.0.append(&mut guard.0);
            updates.1.append(&mut guard.1);
            // Convenience, never a setup blocker: the /remote-control relaunch
            // command (see CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE).
            match ensure_claude_remote_control_command() {
                Ok(mut rc) => {
                    updates.0.append(&mut rc.0);
                    updates.1.append(&mut rc.1);
                }
                Err(err) => log::warn!("installing /remote-control command failed: {err}"),
            }
            // Same: the per-conversation savings statusline. Rides routing, so
            // it is never on screen while Claude Code bypasses Headroom.
            match ensure_claude_statusline() {
                Ok(mut line) => {
                    updates.0.append(&mut line.0);
                    updates.1.append(&mut line.1);
                }
                Err(err) => log::warn!("installing Claude statusline failed: {err}"),
            }
            // `disable_client_setup` (every pause and quit) strips the MarkItDown
            // Read hook with RTK's but keeps its script, which only turning the
            // add-on off deletes; so the script says to re-register it. Without
            // this the PDF hook was gone after the first quit.
            let markitdown_hook = headroom_markitdown_hook_path();
            if markitdown_hook.exists() {
                match ensure_claude_settings_hook(
                    &markitdown_hook,
                    "Read",
                    "headroom-markitdown-read.sh",
                ) {
                    Ok(mut hook) => {
                        updates.0.append(&mut hook.0);
                        updates.1.append(&mut hook.1);
                    }
                    Err(err) => log::warn!("re-registering the MarkItDown Read hook failed: {err}"),
                }
            }

            // Shell profile (RTK PATH + `claude` function) is convenience;
            // tolerate an unwritable profile rather than failing the whole setup.
            let env_block = claude_code_shell_block(crate::proxy_intercept::INTERCEPT_PORT);
            let shell_step = ensure_rtk_integrations_for_targets(
                &default_headroom_rtk_path(),
                &default_headroom_managed_python_path(),
                &shell_targets,
            )
            .and_then(|mut rtk| {
                let mut env = configure_shell_block(&shell_targets, "claude_code", &env_block)?;
                rtk.0.append(&mut env.0);
                rtk.1.append(&mut env.1);
                Ok(rtk)
            });
            match shell_step_best_effort(shell_step)? {
                Some(mut shell) => {
                    updates.0.append(&mut shell.0);
                    updates.1.append(&mut shell.1);
                }
                None => shell_unwritable = true,
            }

            changed_files.extend(updates.0);
            backup_files.extend(updates.1);
            state
                .managed_shell_files
                .insert(state_id.clone(), serialize_paths(&shell_targets));
            Ok(())
        })?,
        "vscode" => {
            let (changed, backups, replaced) = configure_vscode_settings()?;
            changed_files.extend(changed);
            backup_files.extend(backups);
            if let Some(original) = replaced {
                state
                    .preserved_base_urls
                    .insert(state_id.clone(), original.clone());
                replaced_base_url = Some(original);
            }
        }
        "codex" | "codex_cli" => {
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            // Heal (and pre-empt) the wheel's MCP registrar deleting other
            // apps' tables from inside our marker span. Best-effort.
            protect_foreign_mcp_tables_unlocked();
            // Critical, app-owned write first: the ~/.codex/config.toml provider
            // block is what routes Codex through Headroom.
            let (changed, backups, preserved) = configure_codex_provider_block()?;
            let mut updates = (changed, backups);
            let captured = !preserved.is_empty();
            for (entry, original) in preserved {
                // A custom root `model_provider` or `openai_base_url` (gateway,
                // LM Studio) was routing Codex before us: remember it for
                // restore-on-disable so we don't silently drop the user onto
                // api.openai.com. Restored silently (no takeover notice: that
                // copy is Claude/base_url specific).
                state
                    .preserved_base_urls
                    .insert(entry.to_string(), original);
            }
            // Persist now: config.toml no longer holds the user's value, so if
            // a later step fails (malformed hooks.json) it would be lost for good.
            if captured {
                write_setup_state(&state)?;
            }

            // Loud-fail guard so a closed app or clobbered config surfaces in
            // Codex instead of silently routing direct to OpenAI.
            let mut guard = ensure_codex_guard_hook()?;
            updates.0.append(&mut guard.0);
            updates.1.append(&mut guard.1);

            // The OPENAI_BASE_URL export is for other OpenAI clients, not
            // routing (config.toml routes Codex), so best-effort. It exports
            // only while the intercept answers and never over the user's own
            // value (Ollama, OpenRouter); an older build's unconditional
            // block is rewritten in place.
            let env_block = codex_shell_block(crate::proxy_intercept::INTERCEPT_PORT);
            match shell_step_best_effort(configure_shell_block(
                &shell_targets,
                "codex_cli",
                &env_block,
            ))? {
                Some(mut shell) => {
                    updates.0.append(&mut shell.0);
                    updates.1.append(&mut shell.1);
                }
                None => shell_unwritable = true,
            }
            changed_files.extend(updates.0);
            backup_files.extend(updates.1);
            state
                .managed_shell_files
                .insert(state_id.clone(), serialize_paths(&shell_targets));
            // Pull existing native threads into the headroom-provider menu so the
            // Codex history list stays whole once it routes through Headroom.
            retag_codex_thread_providers(CODEX_NATIVE_PROVIDER, CODEX_HEADROOM_PROVIDER);
        }
        "grok_build" => {
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            protect_foreign_mcp_tables_unlocked();
            let mut updates = configure_grok_proxy_block()?;
            let env_block = format!(
                "export GROK_CLI_CHAT_PROXY_BASE_URL={}",
                HEADROOM_GROK_PROXY_BASE_URL
            );
            match shell_step_best_effort(configure_shell_block(
                &shell_targets,
                "grok_build",
                &env_block,
            ))? {
                Some(mut shell) => {
                    updates.0.append(&mut shell.0);
                    updates.1.append(&mut shell.1);
                }
                None => shell_unwritable = true,
            }
            changed_files.extend(updates.0);
            backup_files.extend(updates.1);
            state
                .managed_shell_files
                .insert(state_id.clone(), serialize_paths(&shell_targets));
        }
        "opencode" => {
            // Config-file routing only: OpenCode reads provider base URLs from
            // opencode.json(c); no env vars or shell blocks are involved.
            let updates = configure_opencode_provider_block(&mut state)?;
            changed_files.extend(updates.0);
            backup_files.extend(updates.1);
        }
        other => return Err(anyhow!("Automatic setup is not supported yet for {other}.",)),
    }

    // Keep the original enable time across re-applies (launch restore, resume,
    // hourly repair). It drives the "Restart X" hint, which is meant for the
    // user's own enable, not for every app launch. A different build restamps
    // it: an update may write different config, which an already-open client
    // only picks up on restart.
    // ponytail: every update counts, config-changing or not; fingerprint the
    // managed output if the post-update hint proves noisy.
    let same_build = state
        .setup_versions
        .get(&state_id)
        .is_none_or(|version| version == env!("CARGO_PKG_VERSION"));
    let configured_at = state
        .configured_clients
        .get(&state_id)
        .or_else(|| state.remembered_clients.get(&state_id))
        .filter(|_| same_build)
        .cloned()
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    state
        .configured_clients
        .insert(state_id.clone(), configured_at);
    state
        .setup_versions
        .insert(state_id, env!("CARGO_PKG_VERSION").to_string());
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
        next_steps: {
            let mut steps = Vec::new();
            if shell_unwritable {
                steps.push(
                    "Couldn't update your shell profile (e.g. ~/.zshrc): it isn't writable or isn't UTF-8 text. The client's own config still routes through Headroom. For terminal use, fix the file and turn the connector off and on."
                        .into(),
                );
            }
            // The restart hint lives on the connector row, not here.
            if normalized_setup_id(client_id) == "codex_cli" {
                steps.push(
                    "In the Codex CLI, run /hooks and trust the Headroom guard so it can warn you if routing breaks."
                        .into(),
                );
            }
            steps
        },
        verification,
        shell_profile_unwritable: shell_unwritable,
        replaced_base_url,
    })
}

pub fn verify_client_setup(client_id: &str) -> Result<ClientSetupVerification> {
    let mut checks = Vec::new();
    let mut failures = Vec::new();

    match client_id {
        "claude_code" => {
            let state = load_setup_state();
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            let shell_ok = shell_block_contains_text_in_files(
                &shell_targets,
                "claude_code",
                &intercept_export_line("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL),
            )?;
            let rtk_path_ok =
                shell_block_contains_text_in_files(&shell_targets, "managed_rtk", "export PATH=")?;
            let claude_settings_ok =
                claude_settings_env_matches("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
            let rtk_hook_ok = claude_settings_hook_matches("headroom-rtk-rewrite.sh")?
                && headroom_rtk_hook_path().exists();

            // Informational: the export only reaches shells started while
            // Headroom runs, never VS Code or the desktop app, so settings.json
            // stays the routing check and a missing export fails nothing.
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
            if !claude_settings_ok {
                failures.push(
                    "Claude Code ANTHROPIC_BASE_URL was not found in ~/.claude/settings.json."
                        .into(),
                );
            }
            // RTK is a separate, opt-in integration (`set_rtk_enabled` tears it
            // down without touching ANTHROPIC_BASE_URL routing). Its wiring is
            // only ever added when the managed binary exists on disk (see
            // `ensure_rtk_integrations_for_targets`), so its absence must not
            // fail Claude Code verification when RTK isn't installed or the user
            // disabled it — routing is what "connected" means here.
            // The PATH export is shell convenience apply skips on an unwritable
            // profile; the hook is the RTK wiring, so only it can fail setup.
            let rtk_required = !state.rtk_disabled && default_headroom_rtk_path().exists();
            if rtk_required && !rtk_hook_ok {
                failures.push(
                    "Headroom-managed RTK Claude hook was not found in ~/.claude/settings.json."
                        .into(),
                );
            }

            // Three cases, worded apart because they are different bugs
            // (RUST-GS grouped them all as "not found"). A guard registered
            // under the other interpreter is not lost: `guard_python_command`
            // re-probes `/usr/bin/python3` every process, so installing or
            // removing the Command Line Tools, or one probe timing out, changes
            // the expected command. It still runs, so it verifies, and repair
            // re-applies it as stale without reporting it.
            if !claude_guard_hook_path().exists() {
                failures.push(CLAUDE_GUARD_SCRIPT_MISSING.into());
            } else if claude_guard_registered()? {
                checks.push(
                    "Found Headroom routing guard registered in ~/.claude/settings.json.".into(),
                );
            } else if claude_guard_registered_any_interpreter()? {
                checks.push(CLAUDE_GUARD_STALE_COMMAND.into());
            } else {
                failures.push(
                    "Headroom routing guard was not found in ~/.claude/settings.json.".into(),
                );
            }
        }
        "vscode" => {
            let mut delegated = verify_client_setup("claude_code")?;
            delegated.client_id = "vscode".to_string();
            return Ok(delegated);
        }
        "codex" | "codex_cli" => {
            let state = load_setup_state();
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            let shell_ok = shell_block_contains_text_in_files(
                &shell_targets,
                "codex_cli",
                &intercept_export_line("OPENAI_BASE_URL", HEADROOM_OPENAI_BASE_URL),
            )?;
            let toml_ok = codex_provider_block_matches()?;

            // Shell export is convenience, not routing: config.toml is what
            // routes Codex (apply tolerates an unwritable shell profile).
            if shell_ok {
                checks.push(
                    "Found ChatGPT Codex OPENAI_BASE_URL export in managed shell block.".into(),
                );
            }
            if toml_ok {
                checks
                    .push("Found Headroom-managed provider block in ~/.codex/config.toml.".into());
            }
            if !toml_ok {
                failures.push(
                    "Headroom-managed provider block in ~/.codex/config.toml is missing or stale (e.g. Codex login state changed since it was written).".into(),
                );
            }
            if codex_guard_hook_path().exists() && codex_guard_registered()? {
                checks
                    .push("Found Headroom routing guard registered in ~/.codex/hooks.json.".into());
            } else {
                failures
                    .push("Headroom routing guard was not found in ~/.codex/hooks.json.".into());
            }

            // Independent confirmation from Codex itself, run off-thread: the
            // `codex doctor` call takes seconds and `verify` is awaited by the
            // setup UI, so block it there and we stall the flow. Detached and
            // logged (its output isn't surfaced in the result today); never a
            // `verified` failure (doctor can flag unrelated issues, and an
            // untrusted-but-installed guard is expected until the user runs
            // /hooks). At most hourly: verify runs on every setup-UI poll and
            // repair pass, and each doctor run scans the rollout DB and probes
            // the proxy unauthenticated (`HEAD /v1/responses` + `GET
            // /v1/models`, both 401 by design), which read as a Codex auth
            // failure in the log (84 runs in a day on one machine).
            static LAST_DOCTOR: std::sync::Mutex<Option<std::time::Instant>> =
                std::sync::Mutex::new(None);
            let due = {
                let mut last = LAST_DOCTOR
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let due = last.is_none_or(|at| at.elapsed() >= Duration::from_secs(3600));
                if due {
                    *last = Some(std::time::Instant::now());
                }
                due
            };
            if due {
                std::thread::spawn(|| {
                    if let Some(summary) = codex_doctor_summary() {
                        log::info!("codex doctor: {summary}");
                    }
                });
            }
        }
        "grok_build" => {
            let state = load_setup_state();
            let shell_targets = resolve_client_shell_targets(&state, client_id)?;
            let shell_ok = shell_block_contains_in_files(
                &shell_targets,
                "grok_build",
                "GROK_CLI_CHAT_PROXY_BASE_URL",
                HEADROOM_GROK_PROXY_BASE_URL,
            )?;
            let toml_ok = grok_proxy_block_matches()?;

            if shell_ok {
                checks.push(
                    "Found Grok Build GROK_CLI_CHAT_PROXY_BASE_URL export in managed shell block."
                        .into(),
                );
            }
            if toml_ok {
                checks.push("Found Headroom-managed proxy block in ~/.grok/config.toml.".into());
            }
            if !toml_ok {
                failures.push(
                    "Headroom-managed proxy block was not found in ~/.grok/config.toml.".into(),
                );
            }
            if !shell_ok {
                failures.push(
                    "Grok Build GROK_CLI_CHAT_PROXY_BASE_URL export was not found in shell profiles."
                        .into(),
                );
            }
        }
        "opencode" => {
            if opencode_provider_block_matches()? {
                checks.push(
                    "Found Headroom proxy base URLs for the anthropic and openai providers in OpenCode's config."
                        .into(),
                );
            } else {
                failures.push(
                    "Headroom proxy base URLs were not found for the anthropic and openai providers in OpenCode's config."
                        .into(),
                );
            }
        }
        other => return Err(anyhow!("Verification is not supported yet for {other}.",)),
    }

    // Proxy reachability is transient runtime state — the runtime warm-up
    // can finish after this verification runs. Surface it via the
    // `proxy_reachable` field, but don't fail `verified` on it. `verified`
    // attests only to "we wrote everything we needed to write".
    let proxy_reachable = is_headroom_proxy_reachable();
    if proxy_reachable {
        checks.push("Headroom proxy is reachable on 127.0.0.1:6767.".into());
    }

    Ok(ClientSetupVerification {
        client_id: client_id.to_string(),
        verified: failures.is_empty(),
        proxy_reachable,
        checks,
        failures,
    })
}

/// Silent self-heal for drifted client configs: for every client the user has
/// enabled, if verification fails (another tool rewrote settings.json, a shell
/// block vanished), re-run `apply_client_setup` and confirm with a re-verify.
/// Returns the client ids whose broken config was repaired; a version
/// restamp re-applies without being listed.
///
/// Scans at most once per hour per process: verification reads a handful of
/// files (and the codex arm spawns a detached `codex doctor`), and a repair
/// that cannot stick (read-only fs, ancient CLI) must not churn on every
/// watchdog tick. The exception is `~/.claude/settings.json` changing on disk:
/// a Claude Code process holding a pre-wiring copy writes the whole file back
/// and drops our env (RUST-KH), and every session started before the hourly
/// scan ran went direct. That file alone is re-verified on the next tick
/// (5 minutes) after it changes; verify-first means a clean file is not
/// rewritten, so our own repair write does not loop.
pub fn repair_client_setups() -> Vec<String> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    // ponytail: process-wide hourly throttle; split per client if support
    // traffic ever shows one client's broken repair starving another's.
    static LAST_SCAN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    static CLAUDE_SETTINGS_MTIME: Mutex<Option<SystemTime>> = Mutex::new(None);
    let mtime = std::fs::metadata(claude_settings_path())
        .and_then(|meta| meta.modified())
        .ok();
    let claude_settings_changed = {
        let mut seen = CLAUDE_SETTINGS_MTIME.lock().unwrap();
        let changed = seen.is_some() && *seen != mtime;
        *seen = mtime;
        changed
    };
    {
        let mut last = LAST_SCAN.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(3600)) {
            return if claude_settings_changed
                && is_configured(&load_setup_state(), "claude_code")
                && repair_client_setup_now("claude_code")
            {
                vec!["claude_code".to_string()]
            } else {
                Vec::new()
            };
        }
        *last = Some(Instant::now());
    }

    let client_ids: Vec<String> = load_setup_state()
        .configured_clients
        .keys()
        .cloned()
        .collect();
    client_ids
        .into_iter()
        .filter(|client_id| repair_client_setup_now(client_id))
        .collect()
}

/// Codex answered a request with 401 "Missing bearer": the provider block
/// lacks `requires_openai_auth` (written by a build before 0.9.28, or edited
/// by hand), so Codex attaches no credentials at all. The hourly scan above
/// would fix it within the hour, but the user is failing NOW, on every prompt, with no hint that
/// Headroom is the cause (RUST-C1, ~16 hosts/week; the Sep 14-20 cohort of
/// activated-but-never-saved users was 80% Codex-plan). Repair immediately,
/// bounded to once per five minutes so a retry loop cannot churn config.toml,
/// and only while the connector is still enabled (the pricing gate disables it
/// on purpose and must not be fought).
/// Claim the next repair slot, or `false` if one was claimed under five
/// minutes ago. One mutex and no I/O, because this is what the forwarding task
/// calls on EVERY 401: in the missing-bearer state every prompt 401s and Codex
/// retries, so deciding this inside a spawned thread meant one OS thread per
/// failed response whose whole job was to take this lock and give up. Claim
/// first, spawn only on success.
pub fn claim_codex_missing_bearer_slot() -> bool {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let mut last = LAST.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if last.is_some_and(|at| at.elapsed() < Duration::from_secs(300)) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

/// The repair itself, unthrottled: [`claim_codex_missing_bearer_slot`] is the
/// throttle, and both the enabled check and the rewrite touch the filesystem,
/// so this belongs on a thread of its own rather than the forwarding task.
pub fn repair_codex_missing_bearer_now() -> bool {
    // Only while the connector is still enabled: the pricing gate disables it
    // on purpose and must not be fought.
    if !is_codex_enabled() {
        return false;
    }
    repair_client_setup_now("codex_cli")
}

/// The version that last wrote this client's managed files, when it is not
/// the running one. Pre-stamp installs (no entry) count as stale: they are
/// exactly the installs an update left behind.
fn stale_setup_version(client_id: &str) -> Option<String> {
    let state = load_setup_state();
    let state_id = normalized_setup_id(client_id);
    if !state.configured_clients.contains_key(state_id) {
        return None;
    }
    let written_by = state
        .setup_versions
        .get(state_id)
        .cloned()
        .unwrap_or_else(|| "an earlier build".to_string());
    (written_by != env!("CARGO_PKG_VERSION")).then_some(written_by)
}

/// One client's verify -> re-apply -> re-verify cycle, unthrottled. Returns
/// true only when a silently broken config came back clean. A version restamp
/// re-applies too but returns false: it is every client on every update, so
/// counting it would bury the real repairs in the auto-repaired metric.
fn repair_client_setup_now(client_id: &str) -> bool {
    let (mut broken, checks) = match verify_client_setup(client_id) {
        Ok(verification) => (verification.failures, verification.checks),
        // Ids verification doesn't support are ids repair can't help.
        Err(_) => (Vec::new(), Vec::new()),
    };
    // Only a failed check means a config broke silently. A version restamp
    // is every client on every update, and its text carries the version, so
    // reporting it opened four new issues per release (RUST-J5..J8).
    let silently_broken = !broken.is_empty();
    // Managed files written by another app version verify fine (the routing
    // export is still there) but are a different generation from what this
    // build's scripts and hooks expect. Re-apply so an update carries them.
    if let Some(stale) = stale_setup_version(client_id) {
        broken.push(format!(
            "Managed files were written by Headroom {stale}; running {}.",
            env!("CARGO_PKG_VERSION")
        ));
    }
    // Verifies, but the next session start runs an interpreter this build
    // would not pick; re-apply like a restamp, unreported.
    if checks
        .iter()
        .any(|check| check == CLAUDE_GUARD_STALE_COMMAND)
    {
        broken.push(CLAUDE_GUARD_STALE_COMMAND.into());
    }
    if broken.is_empty() {
        return false;
    }
    if let Err(err) = apply_client_setup(client_id) {
        log::warn!("repair_client_setups: re-apply for {client_id} failed: {err:#}");
        return false;
    }
    match verify_client_setup(client_id) {
        Ok(verification) if verification.failures.is_empty() => {
            // A successful self-repair is the only fleet-visible trace of a
            // config that was silently broken (e.g. the stale flagless
            // Codex block, which 401'd every request until repaired), so it
            // is reported -- but from here, not through the log bridge.
            // Info, not warn: the bridged warn carried no fingerprint,
            // and Sentry grouped it on the SDK's stacktrace instead of the
            // text -- so byte-identical "repaired codex_cli" lines opened
            // RUST-DK, RUST-E5, RUST-EA and RUST-E0, and a resolve on any
            // of them meant nothing. One issue per client, from here.
            log::info!("repair_client_setups: repaired {client_id} ({broken:?})");
            if !silently_broken {
                return false;
            }
            // WHICH check failed, in the fingerprint and in full as an
            // extra. Grouping on the client alone said only "codex_cli
            // drifted again" (RUST-CF, RUST-F0) -- no way to tell a Codex
            // login that restamps its own config from a shell profile
            // another installer rewrites, which are different bugs with
            // different owners. The strings are fixed sentences from
            // `verify_client_setup`, so they group across machines and
            // carry nothing of the user's.
            let cause: String = broken
                .first()
                .map(|f| f.chars().take(80).collect())
                .unwrap_or_else(|| "unknown".to_string());
            sentry::with_scope(
                |scope| {
                    scope.set_tag("flow", "repair_client_setups");
                    scope.set_extra("failures", broken.clone().into());
                    scope.set_fingerprint(Some(&[
                        "repair_client_setups",
                        client_id,
                        cause.as_str(),
                    ]));
                },
                || {
                    sentry::capture_message(
                        &format!("repair_client_setups: repaired {client_id}"),
                        sentry::Level::Warning,
                    );
                },
            );
            true
        }
        Ok(verification) => {
            log::warn!(
                "repair_client_setups: {client_id} still failing after re-apply: {:?}",
                verification.failures
            );
            false
        }
        Err(err) => {
            log::warn!("repair_client_setups: re-verify for {client_id} errored: {err:#}");
            false
        }
    }
}

/// The agent must have run this recently for its silence to mean anything.
pub const UNROUTED_ACTIVITY_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// Headroom must have been up this long: an agent used before Headroom came
/// back had nowhere to route, which is not a broken hookup.
pub const UNROUTED_MIN_UPTIME: Duration = Duration::from_secs(2 * 3600);
/// Entry cap for the artifact walk; Codex keeps years of session rollouts.
const LOCAL_ACTIVITY_WALK_CAP: usize = 20_000;

/// Newest Claude Code transcript, `<projects_root>/<project>/*.jsonl`: the
/// one artifact only a running session writes. The whole-tree walk this
/// replaced also counted `<project>/memory/MEMORY.md`, which Headroom's own
/// learn writer, memory scrubber and end-marker repair touch -- so a launch
/// that wrote one read as "Claude Code ran", and a machine that had not used
/// it through the proxy for two days fired the unrouted alert hourly
/// (RUST-2K: five hosts in the first day of 0.9.10).
pub(crate) fn newest_claude_transcript_mtime(projects_root: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut visited = 0usize;
    for project in std::fs::read_dir(projects_root).ok()?.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > LOCAL_ACTIVITY_WALK_CAP {
                return newest;
            }
            if entry.path().extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                if Some(modified) > newest {
                    newest = Some(modified);
                }
            }
        }
    }
    newest
}

/// When the agent last wrote its own session artifacts on this machine:
/// evidence it ran, independent of whether Headroom saw any of it.
pub fn client_local_activity_at(client_id: &str) -> Option<SystemTime> {
    match normalized_setup_id(client_id) {
        // Rollouts only, which every surface (CLI, exec, Desktop, the IDE
        // extension) appends on each turn. Not the state_<N>.sqlite thread
        // store: the idle `codex app-server` an IDE extension or Codex Desktop
        // keeps running rewrites it (and its -wal) with no turn at all, which
        // read as "Codex ran, nothing proxied" for users who never opened it
        // (RUST-KC, codex_rollout_fresh=false). Nor the date directories: a
        // Codex that opens a thread it never writes creates today's directory
        // with no rollout in it, which kept RUST-KC firing on 0.9.27.
        "codex_cli" => newest_jsonl_under(&codex_home().join("sessions"), LOCAL_ACTIVITY_WALK_CAP)
            .map(|(at, _)| at),
        "claude_code" => {
            newest_claude_transcript_mtime(&home_dir().join(".claude").join("projects"))
        }
        _ => None,
    }
}

/// Newest `*.jsonl` under `root` by mtime, visiting at most `cap` entries.
/// A resumed thread appends to its original day's rollout, so the date-named
/// directories cannot be trusted to hold the newest one.
fn newest_jsonl_under(root: &Path, cap: usize) -> Option<(SystemTime, PathBuf)> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > cap {
                return newest;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let path = entry.path();
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Ok(modified) = meta.modified() {
                    if newest.as_ref().is_none_or(|(at, _)| modified > *at) {
                        newest = Some((modified, path));
                    }
                }
            }
        }
    }
    newest
}

/// What a Codex rollout's first line (`session_meta`) says about the session.
#[derive(Debug, Default, PartialEq)]
struct CodexSessionMeta {
    /// Which Codex wrote it: codex_cli_rs, codex_vscode, codex_exec, the app...
    originator: Option<String>,
    cli_version: Option<String>,
    /// The provider the thread was CREATED with.
    model_provider: Option<String>,
    started_at: Option<chrono::DateTime<Utc>>,
}

fn parse_codex_session_meta(first_line: &str) -> Option<CodexSessionMeta> {
    let line: Value = serde_json::from_str(first_line.trim()).ok()?;
    if line.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = line.get("payload")?;
    let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
    Some(CodexSessionMeta {
        originator: text("originator"),
        cli_version: text("cli_version"),
        model_provider: text("model_provider"),
        started_at: text("timestamp")
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok())
            .map(|at| at.with_timezone(&Utc)),
    })
}

/// Sentry tags for "Codex ran, nothing reached the proxy". Without them every
/// report said only that it happened (RUST-DW), which cannot tell the two
/// fixes apart: a session CREATED with `model_provider = "headroom"` read our
/// config and its traffic still never arrived, while one created with
/// "openai" after Headroom started never read the config at all. `resumed`
/// marks a thread older than `app_started_at`, whose provider predates us.
/// `rollout_fresh` says whether that rollout was written this run at all:
/// activity is the newest rollout's mtime since 0.9.28, so "false" on a
/// newer build means the rollout walk and this one disagree (the walk cap).
/// Call it BEFORE re-applying the setup, or `codex_config_routed` reports
/// our own repair instead of what Codex read.
pub(crate) fn codex_unrouted_diagnostics(
    app_started_at: SystemTime,
) -> Vec<(&'static str, String)> {
    use std::io::{BufRead, Read};
    let newest = newest_jsonl_under(&codex_home().join("sessions"), LOCAL_ACTIVITY_WALK_CAP);
    let rollout_fresh = match &newest {
        Some((modified, _)) => (*modified > app_started_at).to_string(),
        None => "unknown".into(),
    };
    let meta = newest
        .and_then(|(_, path)| {
            let file = std::fs::File::open(path).ok()?;
            let mut line = String::new();
            // session_meta carries the base instructions (tens of KB); the cap
            // only stops a pathological first line.
            std::io::BufReader::new(file.take(1 << 20))
                .read_line(&mut line)
                .ok()?;
            parse_codex_session_meta(&line)
        })
        .unwrap_or_default();
    let provider = match meta.model_provider.as_deref() {
        Some("headroom") => "headroom",
        Some("openai") => "openai",
        Some(_) => "other",
        None => "unknown",
    };
    let resumed = match meta.started_at {
        Some(at) => (SystemTime::from(at) < app_started_at).to_string(),
        None => "unknown".into(),
    };
    let unknown = || "unknown".to_string();
    vec![
        ("codex_surface", meta.originator.unwrap_or_else(unknown)),
        (
            "codex_cli_version",
            meta.cli_version.unwrap_or_else(unknown),
        ),
        ("codex_session_provider", provider.into()),
        ("codex_session_resumed", resumed),
        ("codex_rollout_fresh", rollout_fresh),
        (
            "codex_config_routed",
            codex_provider_block_matches().map_or_else(|_| "error".into(), |ok| ok.to_string()),
        ),
        (
            "codex_home_env",
            std::env::var_os("CODEX_HOME")
                .is_some_and(|v| !v.is_empty())
                .to_string(),
        ),
    ]
}

/// When `client_id`'s routing took effect: the later of `started` and the
/// connector's enable time. Activity before it had nowhere to route, so it is
/// no evidence of a broken hookup: a user coding in Codex through the six
/// minutes of a first-run bootstrap read as unrouted one second after setup
/// applied (RUST-KC).
pub(crate) fn routed_since(client_id: &str, started: SystemTime) -> SystemTime {
    configured_timestamp(&load_setup_state(), client_id)
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok())
        .map_or(started, |at| started.max(at.into()))
}

/// Pure decision: the agent ran on this machine while Headroom, routing it the
/// whole time (`routed_since`), saw nothing from it. `requests_recent` is the
/// agent's proxied request count over today and yesterday
/// (usage_counters::requests_since_yesterday).
pub(crate) fn client_ran_unrouted(
    activity_at: Option<SystemTime>,
    requests_recent: u64,
    routed_since: SystemTime,
    now: SystemTime,
) -> bool {
    let Some(activity_at) = activity_at else {
        return false;
    };
    if requests_recent > 0 {
        return false;
    }
    let uptime_ok = now
        .duration_since(routed_since)
        .is_ok_and(|uptime| uptime >= UNROUTED_MIN_UPTIME);
    let recent = now
        .duration_since(activity_at)
        .is_ok_and(|age| age <= UNROUTED_ACTIVITY_WINDOW);
    uptime_ok && recent && activity_at > routed_since
}

pub fn is_claude_code_enabled() -> bool {
    is_configured(&load_setup_state(), "claude_code")
}

pub fn is_codex_enabled() -> bool {
    is_configured(&load_setup_state(), "codex_cli")
}

/// True when an enabled connector bills against the user's own provider keys
/// (or ChatGPT plan), so the Claude pricing gate must neither stop the Python
/// backend nor bypass the proxy for it.
///
/// Counts the remembered snapshot too, as `list_client_connectors` does: quit
/// empties `configured_clients`, and on relaunch the gate is enforced before
/// `restore_client_setups` re-applies them, so reading only the configured set
/// picked FULL bypass for a gated Codex user and tore the backend down until
/// the watchdog brought it back ~35s later. A deliberate pause holds the same
/// snapshot; `ensure_headroom_running` still declines the spawn while paused.
pub fn any_gate_exempt_client_enabled() -> bool {
    let state = load_setup_state();
    GATE_EXEMPT_CLIENTS
        .iter()
        .any(|id| is_configured(&state, id) || state.remembered_clients.contains_key(*id))
}

const GATE_EXEMPT_CLIENTS: [&str; 3] = ["codex_cli", "opencode", "grok_build"];

/// Whether `client_id` is one of the connectors the Claude pricing gate keeps
/// the backend up for (see `any_gate_exempt_client_enabled`).
pub fn is_gate_exempt_client(client_id: &str) -> bool {
    GATE_EXEMPT_CLIENTS.contains(&normalized_setup_id(client_id))
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
            // Fall back to the remembered snapshot while restore_client_setups
            // is still re-applying on launch, so the connector doesn't flash
            // "disabled" during the async restore window after a restart.
            let enabled = is_configured(&setup_state, spec.id)
                || setup_state
                    .remembered_clients
                    .contains_key(normalized_setup_id(spec.id));
            let verification = if enabled {
                verify_client_setup(spec.id).ok()
            } else {
                None
            };
            let verified = verification.as_ref().is_some_and(|result| result.verified);

            ClientConnectorStatus {
                client_id: spec.id.to_string(),
                name: spec.name.to_string(),
                installed,
                enabled,
                verified,
                last_configured_at: configured_timestamp(&setup_state, spec.id),
                verification,
            }
        })
        .collect();

    Ok(connectors)
}

pub fn disable_client_setup(client_id: &str) -> Result<()> {
    let _setup = setup_write_lock();
    let mut state = load_setup_state();

    match client_id {
        "codex" | "codex_cli" => {
            let preserved: Vec<(&str, String)> = CODEX_ROOT_KEYS
                .iter()
                .filter_map(|&(key, _, entry)| {
                    Some((key, state.preserved_base_urls.get(entry)?.clone()))
                })
                .collect();
            disable_codex_cli()?;
            // Restore any pre-Headroom root model_provider/openai_base_url
            // instead of leaving the key deleted -- deleting it silently drops a
            // gateway user onto api.openai.com (mirrors the Claude base_url
            // restore).
            for (key, value) in preserved {
                let _ = restore_codex_root_key(key, &value);
            }
            disable_codex_gui()?;
            // Hand the threads back to the native-provider menu so the full
            // history stays visible once Codex no longer routes through Headroom.
            retag_codex_thread_providers(CODEX_HEADROOM_PROVIDER, CODEX_NATIVE_PROVIDER);
        }
        "codex_gui" => {
            disable_codex_gui()?;
        }
        // One settings.json write for the whole arm; see `coalesce_writes`.
        "claude_code" => coalesce_writes(claude_settings_path(), || -> Result<()> {
            // Routing first, shell profiles best-effort (as codex and grok_build
            // do): the block routes nothing, and a shell cleanup failure that
            // returned early left settings.json pointing Claude Code at the
            // stopped proxy after quit. A settings.json restore error is
            // reported only after the hooks below are stripped.
            // Restore any pre-Headroom gateway/proxy URL instead of deleting
            // the key — deleting it pointed gateway users at api.anthropic.com
            // where their credentials may not even work. The reconciler stops
            // first, or it takes the restored URL straight back.
            crate::tool_manager::set_cc_switch_routed(false);
            let restored = remove_claude_settings_env(
                "ANTHROPIC_BASE_URL",
                HEADROOM_ANTHROPIC_BASE_URL,
                claude_restore_base_url(&state).as_deref(),
            );
            // Drop the ENABLE_TOOL_SEARCH we planted (no-op unless still ours).
            let _ = remove_claude_settings_env(
                HEADROOM_ENABLE_TOOL_SEARCH_KEY,
                HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
                None,
            );
            remove_legacy_vscode_base_url_keys();
            // Strip the PreToolUse hook entry and delete the hook script so CC
            // behaves exactly as it did before Headroom was launched.
            for settings_path in claude_settings_candidates() {
                let _ = strip_headroom_hook_from_settings(&settings_path);
            }
            let hook_path = headroom_rtk_hook_path();
            if hook_path.exists() {
                let _ = std::fs::remove_file(&hook_path);
            }
            let _ = remove_claude_guard_hook();
            let _ = remove_claude_remote_control_command();
            let _ = remove_claude_statusline();
            if let Ok(shell_targets) = resolve_client_shell_targets_for_cleanup(&state, client_id) {
                let _ = remove_shell_block(&shell_targets, "claude_code");
                // Also drop the managed_rtk PATH block so `rtk` isn't exported
                // from shell profiles after quit; otherwise the user's next
                // shell still has Headroom binaries shadowing whatever's on PATH.
                let _ = remove_shell_block(&shell_targets, "managed_rtk");
            }
            restored?;
            Ok(())
        })?,
        "vscode" => {
            // Same settings.json key as claude_code: stop the reconciler first.
            crate::tool_manager::set_cc_switch_routed(false);
            remove_vscode_connector_keys(claude_restore_base_url(&state).as_deref())?;
            let _ = remove_vscode_process_wrapper();
        }
        "grok_build" => disable_grok_build()?,
        "opencode" => disable_opencode(&state)?,
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
            // Consumed: the values are back in the user's config now. The next
            // apply re-captures them if Headroom is re-enabled.
            for (_, _, entry) in CODEX_ROOT_KEYS {
                state.preserved_base_urls.remove(entry);
            }
            state.setup_versions.remove("codex_cli");
        }
        "opencode" => {
            state.configured_clients.remove("opencode");
            state.remembered_clients.remove("opencode");
            state.managed_shell_files.remove("opencode");
            state.remembered_shell_files.remove("opencode");
            // Consumed: the URLs are back in the user's config now. The next
            // apply re-captures them if Headroom is re-enabled.
            state.preserved_base_urls.remove("opencode_anthropic");
            state.preserved_base_urls.remove("opencode_openai");
            state.setup_versions.remove("opencode");
        }
        _ => {
            let state_id = normalized_setup_id(client_id);
            state.configured_clients.remove(state_id);
            state.remembered_clients.remove(state_id);
            state.managed_shell_files.remove(state_id);
            state.remembered_shell_files.remove(state_id);
            state.setup_versions.remove(state_id);
            // Consumed: the URL is back in the user's config now. The next
            // apply re-captures it if Headroom is re-enabled.
            state.preserved_base_urls.remove(state_id);
        }
    }
    write_setup_state(&state)?;
    Ok(())
}

/// Set while the clients are unwired because 6767 is held by a listener that
/// is not this user's Headroom (see `unwire_clients_for_port_holder`).
static CLIENTS_UNWIRED_FOR_PORT_HOLDER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Why a wiring or provider save is refused while the port-holder unwire holds.
pub const PORT_HOLDER_REFUSAL: &str = "Another program is holding Headroom's port 6767. Headroom reconnects your coding tools once it is free; try again then.";

/// Whether the clients are off because an unidentified listener holds 6767.
pub fn clients_unwired_for_port_holder() -> bool {
    CLIENTS_UNWIRED_FOR_PORT_HOLDER.load(std::sync::atomic::Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn set_clients_unwired_for_port_holder(unwired: bool) {
    CLIENTS_UNWIRED_FOR_PORT_HOLDER.store(unwired, std::sync::atomic::Ordering::Release);
}

/// Pause and quit. Also ends a port-holder unwire: the user's decision now
/// stands, so reclaiming the port must not wire the clients back over it.
pub fn clear_client_setups() -> Result<()> {
    CLIENTS_UNWIRED_FOR_PORT_HOLDER.store(false, std::sync::atomic::Ordering::Release);
    clear_and_remember_client_setups()
}

/// The intercept's bind loop found 6767 held by a live listener that is not
/// this user's Headroom (`proxy_intercept::verdict_unwires_clients`): one it
/// cannot name, which is what another signed-in user's Headroom looks like, or
/// another program. Every wired client would keep sending this user's bearer
/// and prompts to it, so unwire them the way a pause does, remembered for
/// `rewire_clients_after_port_reclaimed`. Returns whether anything was wired;
/// with nothing wired (already paused, say) this claims nothing.
///
/// Runs once per holder: the bind loop calls it on every 15s retry, and a
/// client whose disable failed stays configured, which reran the whole
/// teardown each time. Nothing can wire a client while the flag holds
/// (`apply_client_setup` refuses), and pause and quit clear it, so a Resume
/// under the same holder is still caught on the next retry.
pub fn unwire_clients_for_port_holder() -> bool {
    if clients_unwired_for_port_holder() || load_setup_state().configured_clients.is_empty() {
        return false;
    }
    CLIENTS_UNWIRED_FOR_PORT_HOLDER.store(true, std::sync::atomic::Ordering::Release);
    if let Err(err) = clear_and_remember_client_setups() {
        log::warn!("unwiring clients from a port holder: {err:#}");
    }
    let left: Vec<String> = load_setup_state().configured_clients.into_keys().collect();
    if !left.is_empty() {
        log::warn!("unwiring clients from a port holder left {left:?} wired");
    }
    true
}

/// The intercept bound 6767: wire back what `unwire_clients_for_port_holder`
/// took off, unless a pause or quit has taken over since.
pub fn rewire_clients_after_port_reclaimed() {
    if CLIENTS_UNWIRED_FOR_PORT_HOLDER.swap(false, std::sync::atomic::Ordering::AcqRel) {
        log::info!("port reclaimed; re-wiring clients unwired from its previous holder");
        restore_client_setups();
    }
}

/// The crash guard's unwire (`handle_crash_guard_flag` in lib.rs), run once
/// the app is gone: what quit does, for an app that died without quitting, so
/// the clients connect directly instead of failing with ECONNREFUSED on the
/// dead 6767, remembered for the next launch's `restore_client_setups`.
/// Nothing wired (a quit or pause already unwired them) or an intercept that
/// answers again (the next instance is already up) means there is nothing to
/// undo. Returns the clients it unwired.
pub fn unwire_clients_after_crash(intercept_answers: impl FnOnce() -> bool) -> Vec<String> {
    let state = load_setup_state();
    if state.configured_clients.is_empty() || intercept_answers() {
        return Vec::new();
    }
    let codex = is_configured(&state, "codex_cli");
    if let Err(err) = clear_client_setups() {
        log::warn!("crash guard: unwiring clients failed: {err:#}");
    }
    if codex {
        retag_codex_threads_to_native();
    }
    state.configured_clients.into_keys().collect()
}

fn clear_and_remember_client_setups() -> Result<()> {
    // Capture snapshot before disabling. We re-apply it afterwards because
    // disable_client_setup also clears remembered_clients as a side effect,
    // which would otherwise erase the snapshot we need for restore_client_setups.
    let pre = load_setup_state();
    // Merge with any prior snapshot so a second clear is idempotent: after a
    // pause, configured_clients is already empty and only remembered_clients
    // holds the restore set — a quit-time clear must not wipe it (pause then
    // Cmd-Q used to permanently lose all connectors).
    let mut snapshot_clients = pre.remembered_clients.clone();
    snapshot_clients.extend(pre.configured_clients.clone());
    let mut snapshot_shell_files = pre.remembered_shell_files.clone();
    snapshot_shell_files.extend(pre.managed_shell_files.clone());
    // Kept so the next launch's restore can tell an update from a relaunch
    // (see configured_at in apply_client_setup).
    let snapshot_versions = pre.setup_versions.clone();

    for spec in MANAGED_CLIENT_SPECS {
        let _ = disable_client_setup(spec.id);
    }
    let _ = disable_client_setup("codex_gui");

    // Re-save the remembered snapshot so restore_client_setups works on next launch.
    if !snapshot_clients.is_empty() {
        // Only here: disable_client_setup above takes the lock itself.
        let _setup = setup_write_lock();
        let mut state = load_setup_state();
        state.remembered_clients = snapshot_clients;
        state.remembered_shell_files = snapshot_shell_files;
        state.setup_versions = snapshot_versions;
        write_setup_state(&state)?;
    }

    Ok(())
}

/// Fully uninstalls Headroom's on-disk footprint on a best-effort basis:
/// reverses every client setup, strips Headroom's hook entry from Claude Code
/// settings (both `settings.json` and `settings.local.json`), deletes the
/// managed hook script, the Headroom application-support directory, the
/// `~/.headroom` Python runtime, the macOS LaunchAgent plist, Preferences,
/// Caches, and keychain entries.
///
/// Returns the list of paths that were successfully removed (useful for
/// surfacing to the user). Per-step failures are logged and skipped.
/// `remove_dir_all`, retrying on transient `ENOTEMPTY`. A backend/proxy
/// process killed in `stop_headroom` may still flush a log line into the
/// directory tree mid-walk, re-creating an entry so the final `rmdir` fails
/// with "Directory not empty". A short backoff lets the writer finish.
///
/// A `PermissionDenied` is different: it is usually NOT transient, so retrying
/// alone never clears it. The two causes that reach us are a read-only
/// attribute somewhere in the tree -- which blocks the delete outright on
/// Windows, and blocks it via a read-only *directory* on Unix -- and a live
/// process holding an open handle to a file inside it (Sentry RUST-6T: an agent
/// session still running serena's MCP server out of the venv being removed).
/// The first is fixable here, so on the first `PermissionDenied` we clear
/// read-only bits across the tree and try again. The second is not ours to fix
/// by force; callers surface it so the user can close the session.
/// The NSIS uninstaller, which on Windows sits in the app data dir because a
/// currentUser install puts $INSTDIR at %LOCALAPPDATA%\Headroom.
///
/// Never ours to delete. It is the file `HKCU\...\Uninstall\Headroom`'s
/// `UninstallString` points at, and NSIS removes it itself at the end of a
/// successful uninstall. Delete it from under NSIS and any later abort in that
/// section -- its "Headroom is still running" check ends in one -- leaves the
/// registry entry standing with no uninstaller behind it. That machine can
/// never be uninstalled again: the installer's maintenance page reads the
/// UninstallString, `ExecWait` fails to launch it, and the run ends instantly
/// with "Unable to uninstall!" and no uninstaller window.
const NSIS_UNINSTALLER: &str = "uninstall.exe";

/// Remove everything inside `dir`, then `dir` itself, skipping past entries that
/// cannot be removed instead of stopping at the first one.
///
/// `remove_dir_all` walks the tree and returns at the first entry it fails on,
/// leaving every entry it had not reached yet. For the app data dir on Windows
/// that entry is Headroom's own running exe: a currentUser NSIS install puts
/// $INSTDIR at %LOCALAPPDATA%\Headroom, the same path as `app_data_dir()`, and
/// the `--uninstall` sweep runs *from* that exe. So the walk deleted `config`
/// and gave up before `runtime`, and the reinstall found a complete managed
/// runtime and skipped setup while re-prompting for terms.
///
/// Returns the last failure, so a caller can still tell a partial sweep from a
/// clean one.
fn purge_dir_tolerantly(dir: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().eq_ignore_ascii_case(NSIS_UNINSTALLER) {
                continue;
            }
            let path = entry.path();
            // `file_type` does not follow symlinks or reparse points, so a
            // junction is unlinked, never descended into.
            let result = match entry.file_type() {
                Ok(kind) if kind.is_dir() => remove_dir_all_retry(&path),
                _ => std::fs::remove_file(&path),
            };
            if let Err(err) = result {
                log::warn!("cleanup: removing {} failed: {err}", path.display());
                last = Err(err);
            }
        }
    }
    // A child failure is the more useful error; otherwise report the removal of
    // the dir itself, which still fails while anything is left in it.
    last.and(std::fs::remove_dir(dir))
}

/// Kill every process running out of `dir`, except this one.
#[cfg(target_os = "windows")]
fn kill_processes_under(dir: &Path) {
    kill_processes_like(dir, "\\*");
}

/// Kill every process whose image is exactly `exe`: a Headroom-installed binary
/// that must be replaced or deleted, which Windows refuses while it runs.
#[cfg(target_os = "windows")]
pub(crate) fn kill_processes_running(exe: &Path) {
    kill_processes_like(exe, "");
}

/// Kill every process whose image path is `path` followed by the `-like`
/// pattern `suffix`, except this one.
///
/// Windows keeps a running image undeletable, so anything still executing from
/// inside Headroom's footprint pins it: the backend proxy, and the MCP servers
/// (serena, codebase-memory) that Claude Code and Codex spawned from our venv
/// and that outlive us. The in-app uninstall stops the backend via
/// `stop_headroom`, but the `--uninstall` entry point the NSIS uninstaller
/// calls has no `AppState` to do that with, and neither path ever reached the
/// agents' MCP children.
///
/// Identity is the executable's own path, not a port or a name, so this can
/// only ever match a binary Headroom installed. `uninstall.exe` is exempt: it
/// lives in the same directory and is usually the process driving this sweep.
#[cfg(target_os = "windows")]
fn kill_processes_like(path: &Path, suffix: &str) {
    // `-like` metacharacters, plus `'` so a username containing one cannot
    // close the PowerShell literal early.
    let escaped = path
        .display()
        .to_string()
        .replace('`', "``")
        .replace('\'', "''")
        .replace('[', "`[")
        .replace(']', "`]");
    let me = std::process::id();
    // `$PID` is the powershell process itself: its own command line embeds the
    // pattern, and Win32_Process would hand it back as a match (RUST-6F).
    let script = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $PID -and $_.ProcessId -ne {me} -and $_.Name -ne 'uninstall.exe' -and $_.ExecutablePath -like '{escaped}{suffix}' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
    );
    let mut command = crate::proc::command("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    // Bounded: uninstall must finish even when WMI is wedged; the removal
    // below retries past whatever this sweep could not stop.
    match crate::proc::output_with_timeout(command, Duration::from_secs(20)) {
        Ok(output) if output.status.success() => {}
        Ok(output) => log::warn!("cleanup: process sweep exited {:?}", output.status.code()),
        Err(crate::proc::OutputError::TimedOut) => {
            log::warn!("cleanup: process sweep timed out after 20s")
        }
        Err(crate::proc::OutputError::Spawn(err)) => {
            log::warn!("cleanup: process sweep failed to run: {err}")
        }
    }
    // Handles are released asynchronously after the process dies.
    std::thread::sleep(Duration::from_millis(300));
}

pub(crate) fn remove_dir_all_retry(path: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    let mut cleared_readonly = false;
    for attempt in 0..5 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::PermissionDenied && !cleared_readonly {
                    // Once per call: if this does not free the tree, the cause is
                    // an open handle and further passes are wasted work.
                    cleared_readonly = true;
                    clear_readonly_recursive(path);
                }
                last = Err(e);
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    last
}

/// Best-effort: drop the read-only bit on `path` and everything under it, so a
/// following `remove_dir_all` is not blocked by it. Depth-first, because a
/// read-only directory has to stay writable until its children are gone.
///
/// `DirEntry::metadata` does not traverse symlinks or Windows reparse points, so
/// a junction or symlink is never descended into -- this cannot walk out of the
/// tree or loop. Every failure is ignored: this only ever runs as a rescue pass
/// before a delete that has already failed once.
fn clear_readonly_recursive(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(md) if md.is_dir() => clear_readonly_recursive(&entry.path()),
                Ok(md) => clear_readonly(&entry.path(), md.permissions()),
                Err(_) => {}
            }
        }
    }
    // The directory itself last: on Unix its write bit is what permits unlinking
    // the children above, so clearing it earlier would be undone by nothing but
    // is pointless before they are gone.
    if let Ok(md) = std::fs::symlink_metadata(path) {
        clear_readonly(path, md.permissions());
    }
}

fn clear_readonly(path: &Path, perms: std::fs::Permissions) {
    if !perms.readonly() {
        return;
    }
    // Deliberately NOT `set_readonly(false)`: on Unix that sets the write bit for
    // group and other as well. This runs on a delete that has already failed, so
    // the delete may fail again (an open handle is not fixable here) -- and then
    // whatever we widened is left behind permanently on the user's disk. Grant
    // the minimum that permits the unlink: owner write.
    let mut perms = perms;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = perms.mode();
        perms.set_mode(mode | 0o200);
    }
    #[cfg(not(unix))]
    {
        // Windows has no per-class write bit here; the read-only attribute is the
        // whole mechanism, and clearing it is what unblocks the delete.
        perms.set_readonly(false);
    }
    let _ = std::fs::set_permissions(path, perms);
}

/// Undo every edit Headroom made to *other* tools' state: agent settings, shell
/// rc blocks, hook scripts, MCP registrations, login-keychain credentials, the
/// backup files we left behind, and the LaunchAgent plist.
///
/// Deliberately leaves Headroom's own directories alone. Splitting it out this
/// way is what lets the Homebrew cask call `--uninstall` from its `uninstall`
/// stanza without destroying user data that belongs to `zap` — see
/// docs/macos-release.md. Idempotent: safe to run when the app is not running,
/// and safe to run twice.
fn revert_external_mutations_with_status() -> (Vec<String>, bool) {
    let mut removed: Vec<String> = Vec::new();

    // Reverse settings.json mutations and shell blocks for every known client.
    if let Err(err) = clear_client_setups() {
        log::warn!("cleanup: clear_client_setups failed: {err}");
    }

    // Strip the Headroom hook entry from both ~/.claude/settings.json and
    // ~/.claude/settings.local.json. `clear_client_setups` doesn't do this —
    // it only removes env keys — so without this step the hook entry remains,
    // points to a deleted script, and Claude Code logs errors on every call.
    for settings_path in claude_settings_candidates() {
        match strip_headroom_hook_from_settings(&settings_path) {
            Ok(true) => removed.push(settings_path.display().to_string()),
            Ok(false) => {}
            Err(err) => log::warn!(
                "cleanup: stripping hook from {} failed: {err}",
                settings_path.display()
            ),
        }
    }

    // Independently strip the ANTHROPIC_BASE_URL routing env and the Claude
    // guard hook. clear_client_setups() above also removes these via
    // disable_client_setup, but any failure there leaves both in place, and
    // each bricks Claude once the proxy is gone (stale base URL -> dead
    // 127.0.0.1:6767; guard hook errors on every prompt). Do them
    // unconditionally here. Idempotent: each only acts on Headroom's own value,
    // restoring the cc-switch capture or any preserved pre-Headroom gateway URL.
    if let Err(err) = remove_claude_settings_env(
        "ANTHROPIC_BASE_URL",
        HEADROOM_ANTHROPIC_BASE_URL,
        claude_restore_base_url(&load_setup_state()).as_deref(),
    ) {
        log::warn!("cleanup: removing ANTHROPIC_BASE_URL from Claude settings failed: {err}");
    }
    if let Err(err) = remove_claude_settings_env(
        HEADROOM_ENABLE_TOOL_SEARCH_KEY,
        HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
        None,
    ) {
        log::warn!("cleanup: removing ENABLE_TOOL_SEARCH from Claude settings failed: {err}");
    }
    if let Err(err) = remove_claude_guard_hook() {
        log::warn!("cleanup: removing Claude guard hook failed: {err}");
    }
    if let Err(err) = remove_claude_remote_control_command() {
        log::warn!("cleanup: removing /remote-control command failed: {err}");
    }
    // Disabling keeps the wrapper file for VS Code's settings-reload window;
    // uninstall removes it, but only once no settings file still points at it.
    remove_vscode_wrapper_file_if_unreferenced();
    if let Err(err) = remove_claude_statusline() {
        log::warn!("cleanup: removing Claude statusline failed: {err}");
    }
    if let Err(err) = crate::vscode_statusbar::uninstall() {
        log::warn!("cleanup: removing the editor status bar extension failed: {err}");
    }

    // Restore the open-source Claude Code plugin hook if we neutralized it.
    let (oss_hooks_pending, restored_oss_hooks) = restore_oss_plugin_hooks();
    removed.extend(restored_oss_hooks);

    for hook_path in [headroom_rtk_hook_path(), headroom_markitdown_hook_path()] {
        if hook_path.exists() {
            match std::fs::remove_file(&hook_path) {
                Ok(_) => removed.push(hook_path.display().to_string()),
                Err(err) => log::warn!("cleanup: removing {} failed: {err}", hook_path.display()),
            }
        }
    }

    // Drop the managed RTK nudge from ~/.codex/AGENTS.md (clear_client_setups
    // handles env/shell blocks but not these managed Markdown blocks).
    if let Err(err) = remove_managed_block(&rtk_codex_agents_path(), "rtk") {
        log::warn!("cleanup: removing rtk AGENTS.md block failed: {err}");
    }
    // MarkItDown's nudges, Bash rule and conversion cache: uninstall_and_quit
    // removes them through the ToolManager, which `--uninstall` does not have.
    // The unix shim path (ToolManager::markitdown_shim_path); Windows has no
    // Bash rule to match, and the rest does not depend on the path.
    let markitdown_shim = home_dir()
        .join(".headroom")
        .join("bin")
        .join("headroom-markitdown");
    if let Err(err) = disable_markitdown_integration(&markitdown_shim) {
        log::warn!("cleanup: removing the MarkItDown integration failed: {err}");
    }

    // MCP server registrations live in the agents' own configs, outside
    // Headroom's footprint. uninstall_and_quit unregisters via the Python
    // helpers first, but that needs a working runtime — strip anything left
    // that provably launches from Headroom's install dirs (plus the
    // `headroom` server itself), or every new agent session would spawn a
    // failing MCP server against the deleted entrypoint.
    removed.extend(remove_headroom_mcp_entries());

    // Credentials live in the login keychain, which no Homebrew cask stanza can
    // reach, so this has to happen here rather than being left to `zap`.
    remove_known_keychain_entries();

    // Sweep `<basename>.headroom-backup-*` and `<basename>.nommer-backup-*`
    // siblings created by `backup_if_exists` for every file we ever mutated.
    // Without this, stale backups remain in ~/.claude, ~/.claude/hooks,
    // ~/.codex, VS Code's User folder, and the user's
    // shell rc directory after uninstall.
    for target in managed_backup_targets() {
        removed.extend(sweep_managed_backups(&target));
    }

    // The LaunchAgent plist and its Linux counterpart are install side effects
    // outside Headroom's own directories, so they belong here and not with the
    // user-data removal.
    #[cfg(target_os = "macos")]
    removed.extend(remove_macos_launch_agents());
    #[cfg(target_os = "linux")]
    removed.extend(remove_linux_autostart_entries());

    (removed, oss_hooks_pending)
}

#[cfg_attr(target_os = "windows", allow(dead_code))] // Windows uninstall uses perform_full_cleanup()
pub fn revert_external_mutations() -> Vec<String> {
    revert_external_mutations_with_status().0
}

/// Full uninstall: everything `revert_external_mutations` undoes, plus every
/// directory Headroom owns (app data, `~/.headroom`, caches, logs, preferences,
/// the Kompress model snapshot). Used by the in-app "uninstall and quit".
///
/// The `--uninstall` CLI flag deliberately calls the narrower function instead:
/// a Homebrew cask's `uninstall` must not delete user data, which is what `zap`
/// is for.
pub fn perform_full_cleanup() -> Vec<String> {
    let (mut removed, oss_hooks_pending) = revert_external_mutations_with_status();

    // Also wipe the per-client setup-state file so a reinstall starts clean.
    let setup_state = setup_state_path();
    if setup_state.exists() {
        // Retried and reported: a stale setup state left by one scanner hold
        // made the reinstall think every client was already configured.
        if let Err(err) = retry_transient_denied(|| std::fs::remove_file(&setup_state)) {
            log::warn!("cleanup: removing {} failed: {err}", setup_state.display());
        }
    }

    let app_dir = app_data_dir();
    if app_dir.exists() {
        if oss_hooks_pending {
            log::warn!(
                "cleanup: preserving {} because an OSS Claude plugin hook still needs restoration",
                app_dir.display()
            );
        } else {
            // Before the sweep, not after: on Windows an open image or handle
            // inside the tree is what makes an entry undeletable.
            #[cfg(target_os = "windows")]
            kill_processes_under(&app_dir);
            match purge_dir_tolerantly(&app_dir) {
                Ok(_) => removed.push(app_dir.display().to_string()),
                Err(err) => log::warn!("cleanup: removing {} failed: {err}", app_dir.display()),
            }
        }
    }

    let dot_headroom = home_dir().join(".headroom");
    if dot_headroom.exists() {
        // Tolerant, like the app dir: one file a scanner (or a proxy that has
        // not let go yet) still holds used to abort the whole removal.
        match purge_dir_tolerantly(&dot_headroom) {
            Ok(_) => removed.push(dot_headroom.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", dot_headroom.display()),
        }
    }

    // Model snapshots the bundled runtime pulls into the shared HuggingFace hub
    // cache. This used to remove only KOMPRESS_HF_MODEL_DIR, which orphaned every
    // other model we fetch (~788MB measured: ModernBERT-base, two all-MiniLM-L6-v2
    // variants, siglip-image-encoder-onnx, technique-router-onnx).
    //
    // Sweep by prefix instead of naming each one, so a new model added upstream
    // does not silently start leaking. `chopratejas` is the author of the Python
    // package we bundle, so `models--chopratejas--*` is unambiguously ours.
    //
    // Generic third-party models we also pull (answerdotai--ModernBERT-base,
    // sentence-transformers--all-MiniLM-L6-v2, Qdrant--all-MiniLM-L6-v2-onnx) are
    // deliberately left in place: another tool on this machine may share them, and
    // re-pulling one is cheap next to breaking someone else's cache. Never the
    // cache root either, for the same reason.
    const HF_OWNED_MODEL_PREFIX: &str = "models--chopratejas--";
    // Resolve the cache the way huggingface_hub does rather than assuming the
    // default, so a relocated cache is still cleaned up.
    let hf_hub = crate::tool_manager::hf_hub_cache_dir()
        .unwrap_or_else(|| home_dir().join(".cache").join("huggingface").join("hub"));
    // `.locks` holds a same-named sibling dir per model.
    for parent in [hf_hub.clone(), hf_hub.join(".locks")] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(HF_OWNED_MODEL_PREFIX)
            {
                continue;
            }
            let dir = entry.path();
            match remove_dir_all_retry(&dir) {
                Ok(_) => removed.push(dir.display().to_string()),
                Err(err) => log::warn!("cleanup: removing {} failed: {err}", dir.display()),
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // remove_macos_launch_agents() runs in revert_external_mutations().
        removed.extend(remove_macos_preferences());
        removed.extend(remove_macos_caches());
        removed.extend(remove_macos_logs());
        removed.extend(remove_macos_bundle_dirs());
    }

    #[cfg(target_os = "windows")]
    {
        // Remove the autostart Run key tauri-plugin-autostart creates
        // (HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Headroom).
        let _ = crate::proc::command("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "Headroom",
                "/f",
            ])
            .status();

        // Windows app-data dirs not covered by app_data_dir() (which resolves
        // to %APPDATA%\Headroom already) and the huggingface cache (local).
        if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            for candidate in [base.join("Headroom"), base.join("headroom")] {
                if candidate.exists() {
                    match remove_dir_all_retry(&candidate) {
                        Ok(_) => removed.push(candidate.display().to_string()),
                        Err(err) => {
                            log::warn!("cleanup: removing {} failed: {err}", candidate.display())
                        }
                    }
                }
            }
        }
    }

    removed
}

/// Every file Headroom has ever mutated, and therefore every file that may have
/// a `.headroom-backup-*` / `.nommer-backup-*` sibling to sweep.
fn managed_backup_targets() -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = claude_settings_candidates();
    targets.push(home_dir().join(".claude.json"));
    targets.push(headroom_rtk_hook_path());
    targets.push(headroom_markitdown_hook_path());
    targets.push(claude_guard_hook_path());
    targets.push(codex_config_toml_path());
    targets.push(codex_hooks_json_path());
    targets.push(codex_guard_hook_path());
    targets.push(grok_config_toml_path());
    // Both possible opencode config names: backups are created next to
    // whichever file was active at apply/disable time.
    targets.push(opencode_config_dir().join("opencode.json"));
    targets.push(opencode_config_dir().join("opencode.jsonc"));
    targets.push(vscode_user_settings_path());
    targets.extend(all_shell_paths());
    targets
}

/// Remove sibling backup files that `backup_if_exists` (or its predecessor
/// "nommer") created next to `target`. Filenames look like
/// `<basename>.headroom-backup-<timestamp>` and `<basename>.nommer-backup-<timestamp>`.
/// Returns the paths removed.
fn sweep_managed_backups(target: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    let Some(parent) = target.parent() else {
        return removed;
    };
    let Some(file_name) = target.file_name().and_then(|n| n.to_str()) else {
        return removed;
    };
    let headroom_prefix = format!("{}.headroom-backup-", file_name);
    let nommer_prefix = format!("{}.nommer-backup-", file_name);

    let Ok(entries) = std::fs::read_dir(parent) else {
        return removed;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(&headroom_prefix) && !name.starts_with(&nommer_prefix) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_file(&path) {
            Ok(_) => removed.push(path.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", path.display()),
        }
    }
    removed
}

/// True when an MCP server command launches from inside Headroom's install
/// footprint (the app-support dir or `~/.headroom`). Uninstall deletes both,
/// so a surviving entry could only ever spawn a failing server.
fn mcp_command_in_headroom_footprint(command: &str) -> bool {
    command_under_dir(command, &app_data_dir())
        || command_under_dir(command, &home_dir().join(".headroom"))
}

/// Is `command` a path inside `dir`? A literal `format!("{dir}/")` prefix never
/// matched on Windows, where the configs hold `C:\Users\...\serena.exe` with
/// backslashes and any drive-letter/user casing, so uninstall left entries
/// pointing at deleted executables in every Claude session.
fn command_under_dir(command: &str, dir: &Path) -> bool {
    command_under_dir_for(command, &dir.display().to_string(), cfg!(windows))
}

fn command_under_dir_for(command: &str, dir: &str, windows: bool) -> bool {
    let norm = |s: &str| {
        if windows {
            s.replace('/', "\\").to_lowercase()
        } else {
            s.to_string()
        }
    };
    let sep = if windows { '\\' } else { '/' };
    let mut prefix = norm(dir);
    if !prefix.ends_with(sep) {
        prefix.push(sep);
    }
    norm(command).starts_with(&prefix)
}

/// Headroom-owned MCP entry: the `headroom` server itself (desktop owns that
/// name — install always writes it with --force), or any entry whose command
/// resolves into Headroom's install footprint (serena, codebase-memory).
/// `command` is a string in Claude's config and an array in OpenCode's.
fn mcp_json_entry_is_headroom(name: &str, entry: &Value) -> bool {
    if name == "headroom" {
        return true;
    }
    let command = match entry.get("command") {
        Some(Value::String(command)) => Some(command.as_str()),
        Some(Value::Array(items)) => items.first().and_then(Value::as_str),
        _ => None,
    };
    command.is_some_and(mcp_command_in_headroom_footprint)
}

/// Drop Headroom-owned entries from a `mcpServers`/`mcp` JSON map in place.
/// Returns whether anything was removed.
fn remove_headroom_mcp_json_entries(servers: &mut serde_json::Map<String, Value>) -> bool {
    let owned: Vec<String> = servers
        .iter()
        .filter(|(name, entry)| mcp_json_entry_is_headroom(name, entry))
        .map(|(name, _)| name.clone())
        .collect();
    for name in &owned {
        servers.remove(name);
    }
    !owned.is_empty()
}

/// Strip Headroom-owned MCP servers from `mcpServers` in `~/.claude.json`.
/// Parse failure ⇒ skip: the file holds OAuth state and per-project settings,
/// so it must never be rewritten from a state we couldn't fully read.
fn strip_headroom_mcp_from_claude_json() -> Option<String> {
    let path = home_dir().join(".claude.json");
    if !path.exists() {
        return None;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            log::warn!("cleanup: reading {} failed: {err}", path.display());
            return None;
        }
    };
    if raw.trim().is_empty() {
        return None;
    }
    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(root) => root,
        Err(err) => {
            log::warn!(
                "cleanup: parsing {} failed; leaving it untouched: {err}",
                path.display()
            );
            return None;
        }
    };
    let servers = root.get_mut("mcpServers")?.as_object_mut()?;
    if !remove_headroom_mcp_json_entries(servers) {
        return None;
    }
    let bytes = serde_json::to_vec_pretty(&root).ok()?;
    if let Err(err) = backup_if_exists(&path) {
        log::warn!("cleanup: backing up {} failed: {err}", path.display());
    }
    match atomic_write(&path, &bytes) {
        Ok(()) => Some(path.display().to_string()),
        Err(err) => {
            log::warn!("cleanup: writing {} failed: {err}", path.display());
            None
        }
    }
}

/// Strip Headroom-owned MCP servers from OpenCode's top-level `mcp` table.
/// Same parse-failure contract as the Claude variant.
fn strip_headroom_mcp_from_opencode() -> Option<String> {
    let path = opencode_config_path();
    if !path.exists() {
        return None;
    }
    let mut config = match read_opencode_config(&path) {
        Ok(config) => config,
        Err(err) => {
            log::warn!(
                "cleanup: parsing {} failed; leaving it untouched: {err}",
                path.display()
            );
            return None;
        }
    };
    let servers = config.get_mut("mcp")?.as_object_mut()?;
    if !remove_headroom_mcp_json_entries(servers) {
        return None;
    }
    if let Err(err) = backup_if_exists(&path) {
        log::warn!("cleanup: backing up {} failed: {err}", path.display());
    }
    match write_opencode_config(&path, &config) {
        Ok(()) => Some(path.display().to_string()),
        Err(err) => {
            log::warn!("cleanup: writing {} failed: {err}", path.display());
            None
        }
    }
}

/// The key path of a `[a.b]` table header line as TOML reads it, so
/// `[ a . "b" ]` is `[a.b]` too; `None` for any other line, `[[array]]`
/// headers included.
fn toml_table_header_path(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.starts_with('[') {
        return None;
    }
    let mut table = line.parse::<toml::Table>().ok()?;
    let mut path = Vec::new();
    while let Some((key, value)) = table.into_iter().next() {
        path.push(key);
        let toml::Value::Table(inner) = value else {
            return None;
        };
        table = inner;
    }
    (!path.is_empty()).then_some(path)
}

/// The server name of a `[mcp_servers.<name>]` / `[mcp_servers.<name>.<sub>]`
/// header line, however it is spelled.
fn mcp_table_name(line: &str) -> Option<String> {
    let mut path = toml_table_header_path(line)?.into_iter();
    (path.next()? == "mcp_servers").then(|| path.next())?
}

/// Pure-text removal of Headroom-owned `[mcp_servers.*]` tables (including
/// subtables) and the Python registrar's
/// `# --- [end ]Headroom MCP server[: name] ---` marker comments from a
/// Codex-style TOML config. A table is ours when its name is `headroom` or
/// its `command` launches from Headroom's install footprint; user-managed
/// servers stay untouched.
fn strip_headroom_mcp_toml(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Pass 1: which server names are Headroom-owned.
    let mut owned: BTreeSet<String> = BTreeSet::new();
    let mut current: Option<String> = None;
    for line in &lines {
        if line.trim().starts_with('[') {
            current = mcp_table_name(line);
        }
        let Some(name) = current.as_deref() else {
            continue;
        };
        if name == "headroom"
            || (line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "command")
                && toml_line_value(line)
                    .as_deref()
                    .is_some_and(mcp_command_in_headroom_footprint))
        {
            owned.insert(name.to_string());
        }
    }

    // Pass 2: rebuild without owned spans and marker comments. A span runs
    // from its `[mcp_servers.<name>]`/`[...<name>.<sub>]` header to the next
    // table header of any other name.
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut dropping = false;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("# --- Headroom MCP server")
            || trimmed.starts_with("# --- end Headroom MCP server")
        {
            continue;
        }
        if trimmed.starts_with('[') {
            dropping = mcp_table_name(line).is_some_and(|name| owned.contains(&name));
        }
        if !dropping {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Strip Headroom-owned MCP tables from a Codex/Grok `config.toml`. Returns
/// the path when the file changed.
fn strip_headroom_mcp_from_toml_file(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(err) => {
            log::warn!("cleanup: reading {} failed: {err}", path.display());
            return None;
        }
    };
    let stripped = strip_headroom_mcp_toml(&existing);
    let normalized = {
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}\n")
        }
    };
    if normalized == existing {
        return None;
    }
    if let Err(err) = backup_if_exists(path) {
        log::warn!("cleanup: backing up {} failed: {err}", path.display());
    }
    match atomic_write(path, normalized.as_bytes()) {
        Ok(()) => Some(path.display().to_string()),
        Err(err) => {
            log::warn!("cleanup: writing {} failed: {err}", path.display());
            None
        }
    }
}

/// Strip Headroom-registered MCP servers from every client config. Runs even
/// when the Python unregister helpers in uninstall_and_quit already succeeded
/// (then it's a no-op) so a broken runtime can't leave dead entries behind.
fn remove_headroom_mcp_entries() -> Vec<String> {
    let mut removed = Vec::new();
    removed.extend(strip_headroom_mcp_from_claude_json());
    removed.extend(strip_headroom_mcp_from_toml_file(&codex_config_toml_path()));
    removed.extend(strip_headroom_mcp_from_toml_file(&grok_config_toml_path()));
    removed.extend(strip_headroom_mcp_from_opencode());
    removed
}

fn claude_settings_candidates() -> Vec<PathBuf> {
    let claude_dir = home_dir().join(".claude");
    vec![
        claude_dir.join("settings.json"),
        claude_dir.join("settings.local.json"),
    ]
}

/// Remove the PreToolUse entry pointing at `headroom-rtk-rewrite.sh`. Drops
/// the `PreToolUse` array if it becomes empty, and the `hooks` object if it
/// has no remaining event arrays. Returns true if the file was modified.
fn strip_headroom_hook_from_settings(settings_path: &Path) -> Result<bool> {
    remove_pre_tool_use_markers(
        settings_path,
        &["headroom-rtk-rewrite.sh", "headroom-markitdown-read.sh"],
    )
}

/// Removes every PreToolUse hook whose command contains one of `markers` (a
/// user hook in the same matcher group stays), pruning empty `PreToolUse`/`hooks`
/// containers. Returns whether the file changed.
fn remove_pre_tool_use_markers(settings_path: &Path, markers: &[&str]) -> Result<bool> {
    if !held_or_exists(settings_path) {
        return Ok(false);
    }

    let raw = read_held_or_disk(settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let mut root = parse_json_object(&raw, settings_path)?;

    let Some(hooks_val) = root.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return Ok(false);
    };

    let mut changed = false;

    if let Some(pre_tool_use) = hooks_obj
        .get_mut("PreToolUse")
        .and_then(|value| value.as_array_mut())
    {
        changed = strip_hook_from_groups(pre_tool_use, markers);
        if pre_tool_use.is_empty() {
            hooks_obj.remove("PreToolUse");
        }
    }

    if hooks_obj.is_empty() {
        root.remove("hooks");
    }

    if !changed {
        return Ok(false);
    }

    let _ = backup_if_exists(settings_path)?;
    atomic_write(
        settings_path,
        &serde_json::to_vec_pretty(&Value::Object(root))
            .context("serializing Claude settings for hook cleanup")?,
    )?;

    Ok(true)
}

#[cfg(target_os = "macos")]
fn remove_macos_launch_agents() -> Vec<String> {
    let mut removed = Vec::new();
    let launch_agents_dir = home_dir().join("Library").join("LaunchAgents");

    // Bundle-id-style plist (tauri-plugin-autostart default) and the
    // "Headroom.plist" name some older builds shipped. Either can exist.
    let candidates = ["com.extraheadroom.headroom.plist", "Headroom.plist"];

    for name in candidates {
        let path = launch_agents_dir.join(name);
        if !path.exists() {
            continue;
        }
        // Best-effort unload before deletion so launchd forgets the job.
        let _ = crate::proc::command("launchctl")
            .args(["unload", "-w"])
            .arg(&path)
            .output();
        match std::fs::remove_file(&path) {
            Ok(_) => removed.push(path.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", path.display()),
        }
    }

    removed
}

/// tauri-plugin-autostart writes `~/.config/autostart/<product name>.desktop`
/// on Linux (auto-launch names the file after `package_info().name`). Left
/// behind, it execs a binary uninstall just deleted, on every login — the same
/// class of leftover as the macOS LaunchAgent plist.
#[cfg(target_os = "linux")]
fn remove_linux_autostart_entries() -> Vec<String> {
    let mut removed = Vec::new();
    let autostart_dir = home_dir().join(".config").join("autostart");

    // Current product name, plus the binary name in case a build ever shipped
    // the plugin's `app_name` override. Either can exist.
    for name in ["Headroom.desktop", "headroom-desktop.desktop"] {
        let path = autostart_dir.join(name);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", path.display()),
        }
    }

    removed
}

#[cfg(target_os = "macos")]
fn remove_macos_preferences() -> Vec<String> {
    let mut removed = Vec::new();
    let prefs_dir = home_dir().join("Library").join("Preferences");
    let Ok(entries) = std::fs::read_dir(&prefs_dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("com.extraheadroom.headroom") {
            continue;
        }
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(_) => removed.push(path.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", path.display()),
        }
    }
    removed
}

#[cfg(target_os = "macos")]
fn remove_macos_caches() -> Vec<String> {
    let mut removed = Vec::new();
    let caches_dir = home_dir()
        .join("Library")
        .join("Caches")
        .join("com.extraheadroom.headroom");
    if caches_dir.exists() {
        match std::fs::remove_dir_all(&caches_dir) {
            Ok(_) => removed.push(caches_dir.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", caches_dir.display()),
        }
    }
    removed
}

#[cfg(target_os = "macos")]
fn remove_macos_logs() -> Vec<String> {
    let mut removed = Vec::new();
    let logs_dir = home_dir().join("Library").join("Logs").join("Headroom");
    if logs_dir.exists() {
        match std::fs::remove_dir_all(&logs_dir) {
            Ok(_) => removed.push(logs_dir.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", logs_dir.display()),
        }
    }
    removed
}

/// Sweep the per-bundle-id directories macOS creates for a GUI app outside the
/// Caches/Preferences locations already handled above: the WKWebView data
/// store, HTTP cookie/storage caches, and saved window state.
#[cfg(target_os = "macos")]
fn remove_macos_bundle_dirs() -> Vec<String> {
    let mut removed = Vec::new();
    let lib = home_dir().join("Library");
    let targets = [
        lib.join("WebKit").join("com.extraheadroom.headroom"),
        lib.join("HTTPStorages").join("com.extraheadroom.headroom"),
        lib.join("HTTPStorages")
            .join("com.extraheadroom.headroom.binarycookies"),
        lib.join("Saved Application State")
            .join("com.extraheadroom.headroom.savedState"),
    ];
    for path in targets {
        if !path.exists() {
            continue;
        }
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(_) => removed.push(path.display().to_string()),
            Err(err) => log::warn!("cleanup: removing {} failed: {err}", path.display()),
        }
    }
    removed
}

/// Delete every keychain entry Headroom is known to write. Accounts are
/// captured alongside services because macOS keychain queries require both.
fn remove_known_keychain_entries() {
    const ENTRIES: &[(&str, &str)] = &[
        ("com.extraheadroom.headroom.account", "session-token"),
        ("com.extraheadroom.headroom.device", "machine-id-digest"),
        ("com.extraheadroom.headroom.headroom-learn", "openai"),
        ("com.extraheadroom.headroom.headroom-learn", "anthropic"),
        ("com.extraheadroom.headroom.headroom-learn", "gemini"),
    ];
    for (service, account) in ENTRIES {
        if let Err(err) = crate::keychain::delete_secret(service, account) {
            log::warn!("cleanup: deleting keychain {service}/{account} failed: {err}");
        }
    }
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
// Container-level default, not per-field: one field added or removed in a
// future build must not fail the whole parse and hand back an empty state,
// which reads as "no clients configured" and orphans every shell block we
// wrote (uninstall then can't find them to remove).
#[serde(rename_all = "camelCase", default)]
struct ClientSetupState {
    configured_clients: BTreeMap<String, String>,
    /// Snapshot of configured_clients taken at last pause/quit, used to restore on next startup.
    #[serde(default)]
    remembered_clients: BTreeMap<String, String>,
    #[serde(default)]
    managed_shell_files: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    remembered_shell_files: BTreeMap<String, Vec<String>>,
    /// Pre-existing custom base URLs (corporate gateway, LiteLLM, Bedrock
    /// proxy) that setup replaced with Headroom's, keyed by client state id.
    /// Restored verbatim on disable/uninstall — setup used to clobber these
    /// and never put them back, silently unrouting enterprise users from
    /// their gateway.
    #[serde(default)]
    preserved_base_urls: BTreeMap<String, String>,
    /// User opted RTK out via the tool status toggle. When true, bootstrap and
    /// client setup skip re-adding the RTK PATH export and Claude Code hook.
    #[serde(default)]
    rtk_disabled: bool,
    /// User turned auto-learning off in the Optimize view. When true the proxy
    /// is spawned without the passive traffic-learning flags.
    #[serde(default)]
    auto_learn_disabled: bool,
    /// User turned the Claude Code statusline off in Settings > Advanced. When
    /// true, client setup skips installing it.
    #[serde(default)]
    statusline_disabled: bool,
    /// User turned usage analytics and crash reports off in Settings
    /// (`analytics::sharing_enabled`).
    #[serde(default)]
    usage_data_disabled: bool,
    /// App version that last wrote each client's managed files (scripts,
    /// hooks, shell blocks, commands), keyed by client state id. An update
    /// changes what setup writes but nothing re-ran setup, so users kept
    /// mismatched generations (0.9.22-rc.4 wrapper with an rc.5 script refused
    /// every Remote Control restart). The self-heal re-applies on a mismatch.
    #[serde(default)]
    setup_versions: BTreeMap<String, String>,
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

    // The on-disk file is rewritten by other code paths in this module
    // (apply_client_setup, disable_client_setup, clear_client_setups). Even
    // though `write_setup_state` now publishes via tmp+rename, retry once
    // before giving up: a parse failure on an existing file is almost always
    // a transient race or a partially-written file from an older build, and
    // returning the empty default flips `is_claude_code_enabled` to false,
    // which the tray reads as "Claude Code disconnected" and notifies on.
    match try_load_setup_state(&path) {
        Ok(state) => normalize_setup_state(state),
        Err(first_err) => {
            std::thread::sleep(std::time::Duration::from_millis(15));
            match try_load_setup_state(&path) {
                Ok(state) => normalize_setup_state(state),
                Err(second_err) => {
                    // Only a parse failure is evidence that the bytes on disk
                    // are unusable. An I/O failure says nothing about them, and
                    // quarantining on one is destructive: it renames the user's
                    // real setup away, every caller gets the empty default (the
                    // tray reads that as "every client disconnected"), and the
                    // next write_setup_state persists that emptiness over the
                    // top. Worse, `quarantine_unparsable` reuses one `.corrupt`
                    // slot, so a second failure overwrites the rescue copy of
                    // the first with the now-empty file and the original is
                    // gone for good. RUST-5T is exactly that: one machine out
                    // of file descriptors system-wide (ENFILE), both attempts
                    // failing in `read`, 8 times. Leave the file alone and let
                    // the next launch read it once the machine recovers.
                    let unreadable = first_err.is_io() && second_err.is_io();
                    let verb = if unreadable { "read" } else { "read/parse" };
                    log::warn!(
                        "load_setup_state: failed to {verb} {} twice ({first_err:#}; {second_err:#}); returning default{}",
                        path.display(),
                        if unreadable { " without quarantining" } else { "" }
                    );
                    if !unreadable {
                        quarantine_unparsable(&path, "client setup state");
                    }
                    ClientSetupState::default()
                }
            }
        }
    }
}

/// Why a `client-setup.json` load failed. The two cases must never be handled
/// alike: `Parse` means the bytes on disk are unusable and moving them aside is
/// the recovery path, while `Io` means we never saw the bytes at all and have
/// no grounds to touch the file. See the quarantine decision in
/// `load_setup_state` for what conflating them cost (RUST-5T).
enum SetupStateLoadError {
    Io(anyhow::Error),
    Parse(anyhow::Error),
}

impl SetupStateLoadError {
    fn is_io(&self) -> bool {
        matches!(self, SetupStateLoadError::Io(_))
    }
}

impl std::fmt::Display for SetupStateLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `{:#}` on the inner anyhow error: the callers log with `{err:#}` and
        // the context chain ("reading <path>: <os error>") is the whole signal.
        match self {
            SetupStateLoadError::Io(err) | SetupStateLoadError::Parse(err) => write!(f, "{err:#}"),
        }
    }
}

fn try_load_setup_state(path: &Path) -> std::result::Result<ClientSetupState, SetupStateLoadError> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))
        .map_err(SetupStateLoadError::Io)?;
    serde_json::from_slice::<ClientSetupState>(&bytes)
        .with_context(|| format!("parsing {}", path.display()))
        .map_err(SetupStateLoadError::Parse)
}

fn normalize_setup_state(mut state: ClientSetupState) -> ClientSetupState {
    state.configured_clients = normalize_setup_entries(state.configured_clients);
    state.remembered_clients = normalize_setup_entries(state.remembered_clients);
    state.managed_shell_files = normalize_shell_file_entries(state.managed_shell_files);
    state.remembered_shell_files = normalize_shell_file_entries(state.remembered_shell_files);
    state
}

fn normalize_setup_entries(mut entries: BTreeMap<String, String>) -> BTreeMap<String, String> {
    // codex_gui is a removed id; codex/codex_cli are live again, keep them.
    entries.remove("codex_gui");

    entries
}

fn normalize_shell_file_entries(
    mut entries: BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    entries.remove("codex_gui");

    for files in entries.values_mut() {
        dedupe_strings(files);
    }

    entries
}

fn write_setup_state(state: &ClientSetupState) -> Result<()> {
    let path = setup_state_path();
    let payload = serde_json::to_vec_pretty(state).context("serializing client setup state")?;

    atomic_write(&path, &payload)
}

thread_local! {
    /// The file a `coalesce_writes` scope holds back, and its pending contents.
    static HELD_WRITE: std::cell::RefCell<Option<(PathBuf, Option<Vec<u8>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with every `atomic_write` of `path` held in memory, then write it
/// once (on error too, so a partial apply persists as it did step by step).
/// Claude Code writes settings.json from its in-memory copy, not the file, so
/// a session that loaded it between two of our several writes per apply
/// later wrote it back without the rest: the guard, from a snapshot taken
/// after the env write (RUST-GS), or the env, from one taken mid-teardown
/// (RUST-KH). One write leaves only whole states to snapshot. Readers of the
/// held file go through `read_held_or_disk` / `held_or_exists`.
fn coalesce_writes<T>(path: PathBuf, f: impl FnOnce() -> Result<T>) -> Result<T> {
    // Clears the hold even if `f` panics, so this thread's later writes land.
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            HELD_WRITE.with(|held| held.borrow_mut().take());
        }
    }
    if HELD_WRITE.with(|held| held.borrow().is_some()) {
        return f();
    }
    HELD_WRITE.with(|held| *held.borrow_mut() = Some((path.clone(), None)));
    let release = Release;
    let result = f();
    let pending = HELD_WRITE
        .with(|held| held.borrow_mut().take())
        .and_then(|(_, bytes)| bytes);
    drop(release);
    let flushed = pending.map_or(Ok(()), |bytes| atomic_write(&path, &bytes));
    match (result, flushed) {
        (Ok(value), flushed) => flushed.map(|()| value),
        (Err(err), flushed) => {
            if let Err(flush_err) = flushed {
                log::warn!(
                    "writing {} after a failed step: {flush_err:#}",
                    path.display()
                );
            }
            Err(err)
        }
    }
}

fn held_bytes(path: &Path) -> Option<Vec<u8>> {
    HELD_WRITE.with(|held| match held.borrow().as_ref() {
        Some((held_path, bytes)) if held_path == path => bytes.clone(),
        _ => None,
    })
}

/// `std::fs::read_to_string`, seeing a write `coalesce_writes` holds back.
fn read_held_or_disk(path: &Path) -> std::io::Result<String> {
    match held_bytes(path) {
        Some(bytes) => String::from_utf8(bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
        None => std::fs::read_to_string(path),
    }
}

/// `path.exists()`, seeing a write `coalesce_writes` holds back.
fn held_or_exists(path: &Path) -> bool {
    held_bytes(path).is_some() || path.exists()
}

/// Write via a sibling tmp file then rename. POSIX rename is atomic, so
/// concurrent readers (other apps parsing their own config, the tray-icon
/// thread calling `is_claude_code_enabled` every 2s) see either the old file
/// or the new one — never a half-written truncate. A plain `fs::write` also
/// leaves a truncated file behind on crash/power loss mid-write, which for
/// user-owned configs (settings.json, config.toml, shell rc files) breaks the
/// user's shell or client startup.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let held = HELD_WRITE.with(|held| match held.borrow_mut().as_mut() {
        Some((held_path, bytes)) if held_path == path => {
            *bytes = Some(contents.to_vec());
            true
        }
        _ => false,
    });
    if held {
        return Ok(());
    }
    // Write through a symlink, not over it. Renaming the tmp onto the link
    // replaces the link itself with a regular file, so a dotfiles-managed
    // `~/.zprofile -> ~/dotfiles/zprofile` silently forked into a copy and the
    // repo never saw the edit. Resolving first also keeps the tmp beside the
    // real file, so the rename stays on one filesystem.
    let resolved = resolve_symlink_chain(path);
    if resolved == path {
        return atomic_write_at(path, contents);
    }
    atomic_write_at(&resolved, contents).or_else(|err| {
        // A dangling link into a tree we cannot create (a dotfiles volume not
        // mounted yet): the caller built `contents` from an empty file, so
        // replacing the link would leave a copy holding only our part.
        if std::fs::symlink_metadata(&resolved).is_err() {
            return Err(err);
        }
        // A link into a tree we cannot write (Nix home-manager points
        // ~/.claude/settings.json into the read-only /nix/store) can't be
        // written through. Replacing the link is what every write did before,
        // and it beats failing a routing write outright.
        log::info!(
            "writing through symlink {} failed ({err}); replacing the link",
            path.display()
        );
        atomic_write_at(path, contents)
    })
}

fn atomic_write_at(path: &Path, contents: &[u8]) -> Result<()> {
    // Per-writer unique tmp name. A fixed `<path>.tmp` is shared by concurrent
    // writers to the same file: A renames tmp->path, then B's rename finds its
    // tmp already consumed and fails ENOENT (Sentry RUST-3W / RUST-4W). pid +
    // a process-local counter makes each write's tmp its own.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp_path = {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut s = path.as_os_str().to_os_string();
        s.push(format!(".tmp.{}.{}", std::process::id(), n));
        PathBuf::from(s)
    };
    // Create the parent before the tmp write. Most callers do this themselves
    // (150-odd `create_dir_all(parent)?` sites) but the ones that don't hit
    // ENOENT / ERROR_PATH_NOT_FOUND the moment the dir is missing or has been
    // removed under them (RUST-8M: usage-counters.json, os error 3 on Windows).
    // One guard here covers every caller instead of auditing all of them.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|err| anyhow!("creating {}: {err}", parent.display()))?;
        }
    }
    // The io error goes in the *message*, not a source: every caller logs this
    // with `{err}`, which prints only the top context and drops the chain, so
    // Sentry saw "failed to persist usage-counters.json: writing <path>.tmp.N"
    // with no reason at all (RUST-77). Baking the cause in fixes all 50-odd
    // callers at once instead of auditing each log site for `{err:#}`.
    // Write + fsync the tmp before the rename. Without the fsync the rename's
    // metadata can reach disk ahead of the data, so a crash/power loss leaves a
    // zero-length file where valid state used to be -- which is what the
    // "corrupt (expected value at line 1 column 1)" reports are (RUST-8P).
    // Keep the replaced file's mode. The tmp is created with the umask default
    // (0644), so without this a rewrite silently widened a 0600 settings.json
    // holding ANTHROPIC_AUTH_TOKEN to readable by every other local account.
    #[cfg(unix)]
    let keep_mode = std::fs::metadata(path).ok().map(|meta| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777)
    });
    let mut write_tmp = || -> std::io::Result<()> {
        // direct-write: this is atomic_write
        let mut f = std::fs::File::create(&tmp_path)?;
        // Before any byte lands, so the contents never sit under a wider mode.
        // Best effort: a filesystem without Unix modes (vfat, some network
        // mounts) rejects fchmod, and there the mode meant nothing anyway;
        // failing the write over it would break every caller.
        #[cfg(unix)]
        if let Some(perms) = &keep_mode {
            let _ = f.set_permissions(perms.clone());
        }
        std::io::Write::write_all(&mut f, contents)?;
        f.sync_all()
    };
    write_tmp().map_err(|err| {
        // A failed write still leaves the (partial) tmp behind, and the name is
        // unique per write, so nothing ever reclaims it. On a full disk that is
        // one orphan per attempt, each holding whatever bytes did land (RUST-6R).
        let _ = std::fs::remove_file(&tmp_path);
        anyhow!("writing {}: {err}", tmp_path.display())
    })?;
    // Windows: AV scanners / the search indexer briefly hold the destination
    // (or the just-written tmp) open, so the rename fails ERROR_ACCESS_DENIED
    // (os error 5) even though nothing is wrong with the state (RUST-9M,
    // pricing-state on 0.8.9). Transient by nature -- retry briefly before
    // reporting.
    // direct-write: this is atomic_write
    rename_recovering_lost_tmp(&mut || std::fs::rename(&tmp_path, path), &mut write_tmp).map_err(
        |err| {
            let _ = std::fs::remove_file(&tmp_path); // don't leak the tmp on failure
            anyhow!(
                "renaming {} -> {}: {err}",
                tmp_path.display(),
                path.display()
            )
        },
    )
}

/// Follows `path` through any symlinks to the file a write should land in.
///
/// Hand-rolled rather than `canonicalize` so a dangling link resolves to its
/// (missing) target instead of failing: writing through it creates the target,
/// as `echo >> link` would. Relative targets resolve against the link's own
/// directory. Gives up after 40 hops (the kernel's ELOOP limit) and returns the
/// last path reached, so a link cycle degrades to the old replace-the-link
/// behaviour instead of an error.
pub(crate) fn resolve_symlink_chain(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let is_link = std::fs::symlink_metadata(&current)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            break;
        }
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .map(|dir| dir.join(&target))
                .unwrap_or(target)
        };
    }
    current
}

/// Test helper: creates a file symlink, or returns false where the OS refuses.
/// Windows needs Developer Mode or an elevated shell to create one, so a local
/// Windows run skips; CI must not, since a silent skip there would hide the
/// only Windows coverage of `resolve_symlink_chain` (the runners can create
/// links, so a refusal on CI is a failure).
#[cfg(test)]
pub(crate) fn symlink_file_or_skip(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    match result {
        Ok(()) => true,
        Err(err) if cfg!(windows) && std::env::var_os("CI").is_none() => {
            eprintln!("skipping: cannot create symlinks here ({err})");
            false
        }
        Err(err) => panic!("creating symlink {}: {err}", link.display()),
    }
}

/// Renames a freshly written tmp into place, rewriting it once if it vanished.
///
/// `NotFound` from the rename means the tmp we just wrote and fsynced is gone:
/// a scanner deleted or quarantined it between close and rename (RUST-EZ, os
/// error 2 on Windows). Retrying the rename alone can only fail the same way,
/// so the tmp is written again and that one is moved. Once -- a second loss is
/// not a race, and this is the primitive every persisted file goes through.
fn rename_recovering_lost_tmp(
    rename: &mut impl FnMut() -> std::io::Result<()>,
    write_tmp: &mut impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    match retry_transient_denied(&mut *rename) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            write_tmp()?;
            retry_transient_denied(rename)
        }
        other => other,
    }
}

/// Retries `op` while it fails `PermissionDenied` (or, on Windows, a sharing
/// violation: os error 32, which std maps to `Uncategorized`, raised when the
/// client itself holds its config open mid-write - RUST-5X), sleeping
/// 50/100/200ms between attempts (4 tries total), and on Windows also 400 and
/// 800ms (6 tries, ~1.5s): Defender and the search indexer hold a freshly
/// written file for a second or more, longer than the old 350ms window, and
/// every persisted write goes through here. On Unix a denial is nearly always
/// a real permission problem, so it keeps the short window. Any other error,
/// or the final denial, is returned as-is.
pub(crate) fn retry_transient_denied<T>(
    mut op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut delay = std::time::Duration::from_millis(50);
    for _ in 0..TRANSIENT_DENIED_RETRIES {
        match op() {
            Err(err) if is_transient_denied(&err) => {
                std::thread::sleep(delay);
                delay *= 2;
            }
            other => return other,
        }
    }
    op()
}

/// Retries after the first attempt (see `retry_transient_denied`).
const TRANSIENT_DENIED_RETRIES: u32 = if cfg!(windows) { 5 } else { 3 };

fn is_transient_denied(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
        || (cfg!(windows) && err.raw_os_error() == Some(32))
}

/// Move `path` to `dest` so the next write cannot destroy it. The rename is
/// retried through the transient Windows denials a scanner causes, and when
/// it still fails the file is COPIED instead: a bare rename failing on
/// Windows let the fresh default state persist over the only copy of a user's
/// savings history, which is exactly what the backup exists to prevent.
pub(crate) fn move_aside(path: &Path, dest: &Path) -> std::io::Result<()> {
    // direct-write: moves Headroom's own unparsable state aside, never a user file
    match retry_transient_denied(|| std::fs::rename(path, dest)) {
        Ok(()) => Ok(()),
        Err(rename_err) => std::fs::copy(path, dest).map(|_| ()).map_err(|copy_err| {
            std::io::Error::new(
                copy_err.kind(),
                format!("rename failed ({rename_err}); copy failed ({copy_err})"),
            )
        }),
    }
}

/// Move an unparsable state file aside instead of letting the next write
/// silently overwrite it. Single fixed `.corrupt` slot per file, so repeated
/// failures overwrite each other rather than growing without bound.
/// Best-effort: a failure here must never block the caller's fresh start.
pub(crate) fn quarantine_unparsable(path: &Path, reason: &str) {
    if !path.exists() {
        return;
    }
    let mut s = path.as_os_str().to_os_string();
    s.push(".corrupt");
    let dest = PathBuf::from(s);
    match move_aside(path, &dest) {
        Ok(()) => log::warn!(
            "quarantined unparsable {} -> {} ({reason})",
            path.display(),
            dest.display()
        ),
        Err(err) => log::warn!(
            "could not quarantine unparsable {} ({reason}): {err}",
            path.display()
        ),
    }
}

fn setup_state_path() -> PathBuf {
    config_file(&app_data_dir(), "client-setup.json")
}

fn default_headroom_root_dir() -> PathBuf {
    app_data_dir().join("headroom")
}

// Windows layout mirrors tool_manager: `rtk.exe` in bin, venv interpreters
// under `Scripts\` with `.exe`. Without this, `managed_rtk_path.exists()` is
// always false on Windows and the RTK shell/hook integration silently skips.
fn default_headroom_rtk_path() -> PathBuf {
    let name = if cfg!(target_os = "windows") {
        "rtk.exe"
    } else {
        "rtk"
    };
    default_headroom_root_dir().join("bin").join(name)
}

fn default_headroom_managed_python_path() -> PathBuf {
    let (dir, name) = if cfg!(target_os = "windows") {
        ("Scripts", "python.exe")
    } else {
        ("bin", "python3")
    };
    default_headroom_root_dir()
        .join("runtime")
        .join("venv")
        .join(dir)
        .join(name)
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
        "codex_cli",
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

    Ok(dedupe_shell_targets(rehome_shell_targets(
        targets,
        legacy_shell_home().as_deref(),
        &shell_home(),
    )))
}

fn resolve_client_shell_targets_for_cleanup(
    state: &ClientSetupState,
    client_id: &str,
) -> Result<Vec<PathBuf>> {
    let mut targets = resolve_client_shell_targets(state, client_id)?;
    targets.extend(all_shell_paths());
    Ok(dedupe_shell_targets(targets))
}

fn configure_shell_block(
    shell_targets: &[PathBuf],
    block_id: &str,
    block_body: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut changed = Vec::new();
    let mut backups = Vec::new();

    for file in shell_targets {
        let (did_change, backup) = upsert_managed_block(file, block_id, block_body)?;
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
    let bin_dir = managed_bin_dir.to_string_lossy();
    // The block is sourced by Git Bash on Windows, where `C:\...` in a
    // colon-separated PATH splits at the drive colon into `C` and `\...`.
    let bin_dir = if cfg!(target_os = "windows") {
        msys_path(&bin_dir)
    } else {
        bin_dir.into_owned()
    };
    let d = shell_double_quote(&bin_dir);
    // Dedupe, then prepend: the dir ends up first and exactly once. Written to
    // both the profile and the rc file, so a login shell sources it twice, and
    // an rc that prepends its own dir in between (~/.grok/bin) made a
    // skip-when-first block add a second copy (rc11). Anywhere-on-PATH is not
    // enough either: in a nested macOS login shell (tmux, VS Code, `zsh -l`)
    // path_helper moves the inherited dir behind /etc/paths, and a Homebrew or
    // Rust Type Kit `rtk` would then win. Plain POSIX string surgery on
    // ":$PATH:", so the one body runs the same in zsh, bash 3.2 and sh (zsh
    // does not word-split); quoted pattern text is literal in all three.
    configure_shell_block(
        shell_targets,
        "managed_rtk",
        &format!(
            "_headroom_path=\":$PATH:\"\n\
             while case \"$_headroom_path\" in *:\"{d}\":*) true ;; *) false ;; esac; do\n\
             \x20 _headroom_path=${{_headroom_path%%:\"{d}\":*}}:${{_headroom_path#*:\"{d}\":}}\n\
             done\n\
             _headroom_path=${{_headroom_path#:}}\n\
             _headroom_path=${{_headroom_path%:}}\n\
             export PATH=\"{d}${{_headroom_path:+:$_headroom_path}}\"\n\
             unset _headroom_path"
        ),
    )
}

fn ensure_claude_code_rtk_hook(
    managed_rtk_path: &Path,
    managed_python_path: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
    let hook_path = headroom_rtk_hook_path();
    let hook_body = if rtk_rewrite_exit_is_a_verdict(managed_rtk_path) {
        build_headroom_rtk_hook(managed_rtk_path, managed_python_path)
    } else {
        // Its exit 0 is no verdict, so the hook could only auto-allow every
        // rewrite. Stand down until launch's ensure_rtk_current upgrades it.
        "#!/usr/bin/env bash\nexit 0\n".to_string()
    };
    let (hook_changed, hook_backup) = write_file_if_changed(&hook_path, &hook_body, true)?;
    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();

    if hook_changed {
        changed_files.push(hook_path.display().to_string());
    }
    if let Some(path) = hook_backup {
        backup_files.push(path.display().to_string());
    }

    let (settings_changed, settings_backups) =
        ensure_claude_settings_hook(&hook_path, "Bash", "headroom-rtk-rewrite.sh")?;
    changed_files.extend(settings_changed);
    backup_files.extend(settings_backups);

    Ok((changed_files, backup_files))
}

/// Whether `rtk rewrite`'s exit code is a permission verdict (0 only when every
/// segment is allowed). rtk before 0.37 exits 0 on every rewrite: 0.33.1 passes
/// `git status; rm -rf ~/x` as 0. Only a version read off the binary as older
/// says no; one whose `--version` cannot be read cannot rewrite either.
fn rtk_rewrite_exit_is_a_verdict(rtk: &Path) -> bool {
    let Ok(out) = crate::proc::command(rtk).arg("--version").output() else {
        return true;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let version: Vec<u32> = text
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .split('.')
        .map_while(|part| part.parse().ok())
        .collect();
    // 0.37.2 is the oldest release verified to answer 3 for an unruled command.
    version.len() < 3 || version[..3] >= [0, 37, 2][..]
}

fn markitdown_claude_md_path() -> PathBuf {
    home_dir().join(".claude").join("CLAUDE.md")
}

fn markitdown_codex_agents_path() -> PathBuf {
    codex_home().join("AGENTS.md")
}

/// A path as a shell word: quoted only when a path with whitespace (a home
/// dir with a space, macOS "Application Support") would otherwise split.
fn shell_word(path: &Path) -> String {
    let bin = path.display().to_string();
    if bin.contains(char::is_whitespace) {
        format!("'{bin}'")
    } else {
        bin
    }
}

/// Whether a `Bash(<shim> *)` rule can be trusted and can match. The Windows
/// `.cmd` shim cannot vet its arguments (cmd.exe re-parses them); a path with
/// whitespace never matched in Claude Code, quoted or not (0.9.26-rc.3 on
/// macOS, where the shim then sat under "Application Support").
fn markitdown_rule_allowed(shim_path: &Path) -> bool {
    if cfg!(windows) {
        return false;
    }
    let spaced = shim_path.to_string_lossy().contains(char::is_whitespace);
    if spaced {
        log::warn!(
            "markitdown shim path {} has whitespace; no Bash rule can match it, Office reads will prompt",
            shim_path.display()
        );
    }
    !spaced
}

/// Office-only nudge for Claude Code, where PDFs are already handled by the
/// PreToolUse(Read) hook.
fn build_markitdown_office_nudge(shim_path: &Path) -> String {
    let bin = shell_word(shim_path);
    format!(
        "## Reading Office documents (Headroom MarkItDown)\n\
         The Read tool cannot open .docx, .doc, .pptx, .ppt, .xlsx, or .xls files.\n\
         To read one, run `{bin} <path>` via Bash and use the Markdown it prints.\n\
         (PDFs are handled automatically and need no special step.)"
    )
}

/// Codex nudge: Codex has no PreToolUse-style hook, so it covers PDF *and*
/// Office formats through the `markitdown` CLI.
fn build_markitdown_codex_nudge(shim_path: &Path) -> String {
    let bin = shell_word(shim_path);
    format!(
        "## Reading documents (Headroom MarkItDown)\n\
         To read a .pdf, .docx, .doc, .pptx, .ppt, .xlsx, or .xls file, run\n\
         `{bin} <path>` in the shell and use the Markdown it prints, rather than\n\
         opening the raw file. This keeps large documents cheap to read."
    )
}

/// Enables the MarkItDown addon integration for whichever coding clients are
/// configured through Headroom: Claude Code gets the PDF Read hook plus an
/// Office nudge (managed `~/.claude/CLAUDE.md` block + scoped Bash permission);
/// Codex gets a managed `~/.codex/AGENTS.md` nudge covering PDF and Office (it
/// has no hook mechanism). Idempotent and safe to re-run.
pub fn enable_markitdown_integration(
    markitdown_entrypoint: &Path,
    markitdown_shim: &Path,
    python_path: &Path,
) -> Result<(Vec<String>, Vec<String>)> {
    let _setup = setup_write_lock();
    let mut changed_files = Vec::new();
    let mut backup_files = Vec::new();

    if is_claude_code_enabled() {
        let hook_path = headroom_markitdown_hook_path();
        let hook_body = build_headroom_markitdown_hook(markitdown_entrypoint, python_path);
        let (hook_changed, hook_backup) = write_file_if_changed(&hook_path, &hook_body, true)?;
        if hook_changed {
            changed_files.push(hook_path.display().to_string());
        }
        if let Some(path) = hook_backup {
            backup_files.push(path.display().to_string());
        }

        let (settings_changed, settings_backups) =
            ensure_claude_settings_hook(&hook_path, "Read", "headroom-markitdown-read.sh")?;
        changed_files.extend(settings_changed);
        backup_files.extend(settings_backups);

        let claude_md = markitdown_claude_md_path();
        let (md_changed, md_backup) = upsert_nudge_block(
            &claude_md,
            "markitdown_office",
            &build_markitdown_office_nudge(markitdown_shim),
        )?;
        if md_changed {
            changed_files.push(claude_md.display().to_string());
        }
        if let Some(path) = md_backup {
            backup_files.push(path.display().to_string());
        }

        let allowed = markitdown_rule_allowed(markitdown_shim);
        if set_markitdown_bash_permission(markitdown_shim, &[], |_| Some(allowed))? {
            changed_files.push(claude_settings_path().display().to_string());
        }
    }

    if is_codex_enabled() {
        let agents = markitdown_codex_agents_path();
        let (codex_changed, codex_backup) = upsert_nudge_block(
            &agents,
            "markitdown",
            &build_markitdown_codex_nudge(markitdown_shim),
        )?;
        if codex_changed {
            changed_files.push(agents.display().to_string());
        }
        if let Some(path) = codex_backup {
            backup_files.push(path.display().to_string());
        }
    }

    Ok((changed_files, backup_files))
}

/// Removes every MarkItDown integration artifact for all clients (Claude Read
/// hook + script + Office nudge + Bash permission, and the Codex AGENTS.md
/// nudge), leaving any RTK hook untouched. Cleanup runs unconditionally so a
/// client that was later disconnected is still scrubbed.
pub fn disable_markitdown_integration(markitdown_shim: &Path) -> Result<bool> {
    let _setup = setup_write_lock();
    let hook_path = headroom_markitdown_hook_path();
    if hook_path.exists() {
        let _ = std::fs::remove_file(&hook_path);
    }
    // Every step runs before the first error is returned: an unparseable
    // settings.json used to leave both nudges and the cache behind.
    let steps = [
        remove_pre_tool_use_markers(&claude_settings_path(), &["headroom-markitdown-read.sh"]),
        remove_managed_block(&markitdown_claude_md_path(), "markitdown_office"),
        set_markitdown_bash_permission(markitdown_shim, &[], |_| Some(false)),
        remove_managed_block(&markitdown_codex_agents_path(), "markitdown"),
    ];
    // Converted document text must not outlive the integration.
    let _ = std::fs::remove_dir_all(markitdown_cache_dir());
    steps
        .into_iter()
        .try_fold(false, |changed, step| Ok(changed | step?))
}

/// The Read hook's conversion cache; mirrors the path the hook computes.
fn markitdown_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join("headroom-markitdown")
}

/// Launch-time heal: rewrites an installed MarkItDown Read hook with the
/// current body so hook fixes reach existing installs on app update, points
/// installed nudges and the Bash rule at the current shim (it moved, from
/// `legacy_shims`), and drops the rule where it cannot be trusted or cannot
/// match (see `markitdown_rule_allowed`). Adds nothing the integration did not
/// already have.
pub fn refresh_markitdown_integration(
    markitdown_entrypoint: &Path,
    markitdown_shim: &Path,
    legacy_shims: &[PathBuf],
    python_path: &Path,
) -> Result<()> {
    let _setup = setup_write_lock();
    let hook_path = headroom_markitdown_hook_path();
    if hook_path.exists() {
        let hook_body = build_headroom_markitdown_hook(markitdown_entrypoint, python_path);
        write_file_if_changed(&hook_path, &hook_body, true)?;
    }
    let claude_md = markitdown_claude_md_path();
    if file_has_managed_block(&claude_md, "markitdown_office")? {
        let nudge = build_markitdown_office_nudge(markitdown_shim);
        upsert_nudge_block(&claude_md, "markitdown_office", &nudge)?;
    }
    // The Bash rule follows the shim it names, in one write, and only while the
    // Claude integration (its hook) is on: a disable that raced this launch
    // removed the current rule but not a legacy one.
    let allowed = markitdown_rule_allowed(markitdown_shim);
    set_markitdown_bash_permission(markitdown_shim, legacy_shims, |moved| {
        if !allowed {
            Some(false)
        } else {
            (moved && hook_path.exists()).then_some(true)
        }
    })?;
    let agents = markitdown_codex_agents_path();
    if file_has_managed_block(&agents, "markitdown")? {
        let nudge = build_markitdown_codex_nudge(markitdown_shim);
        upsert_nudge_block(&agents, "markitdown", &nudge)?;
    }
    Ok(())
}

/// Adds or removes a `Bash(<shim> *)` entry in `permissions.allow` so the Office
/// nudge can run `markitdown` without prompting, dropping the entry of every
/// `legacy` shim in the same write. `present` is told whether one was dropped
/// (the rule moved) and returns the wanted state, None to leave it. Returns
/// whether settings changed.
fn set_markitdown_bash_permission(
    shim_path: &Path,
    legacy: &[PathBuf],
    present: impl FnOnce(bool) -> Option<bool>,
) -> Result<bool> {
    let settings_path = claude_settings_path();
    let entry = format!("Bash({} *)", shim_path.display());
    let legacy: Vec<String> = legacy
        .iter()
        .filter(|p| p.as_path() != shim_path)
        .map(|p| format!("Bash({} *)", p.display()))
        .collect();

    let mut content = if held_or_exists(&settings_path) {
        let raw = read_held_or_disk(&settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        if raw.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            Value::Object(parse_json_object(&raw, &settings_path)?)
        }
    } else {
        Value::Object(Default::default())
    };

    let root = content
        .as_object_mut()
        .ok_or_else(|| anyhow!("unable to write Claude permissions settings"))?;
    let allow = root
        .entry("permissions")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("permissions is not an object"))?
        .entry("allow")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("permissions.allow is not an array"))?;

    let before = allow.len();
    allow.retain(|v| !v.as_str().is_some_and(|s| legacy.iter().any(|l| l == s)));
    let moved = allow.len() != before;
    let already = allow.iter().any(|v| v.as_str() == Some(entry.as_str()));
    let present = present(moved).unwrap_or(already);
    if present == already && !moved {
        return Ok(false);
    }
    if present && !already {
        allow.push(Value::String(entry));
    } else if !present {
        allow.retain(|v| v.as_str() != Some(entry.as_str()));
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let _ = backup_if_exists(&settings_path)?;
    atomic_write(
        &settings_path,
        &serde_json::to_vec_pretty(&content).context("serializing Claude permissions settings")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;
    Ok(true)
}

fn disable_codex_cli() -> Result<()> {
    remove_codex_provider_block()?;
    let _ = remove_codex_toml_key("openai_base_url", HEADROOM_OPENAI_BASE_URL);
    let _ = remove_codex_guard_hook();
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

fn configure_vscode_settings() -> Result<(Vec<String>, Vec<String>, Option<String>)> {
    let (mut changed_files, mut backup_files, replaced) = configure_claude_base_url()?;
    let (ts_changed, ts_backups, _) = configure_claude_settings_env_if_absent(
        HEADROOM_ENABLE_TOOL_SEARCH_KEY,
        HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
    )?;
    changed_files.extend(ts_changed);
    backup_files.extend(ts_backups);
    let (legacy_changed, legacy_backups) = remove_legacy_vscode_base_url_keys();
    changed_files.extend(legacy_changed);
    backup_files.extend(legacy_backups);
    Ok((changed_files, backup_files, replaced))
}

fn remove_vscode_connector_keys(restore_value: Option<&str>) -> Result<()> {
    remove_claude_settings_env(
        "ANTHROPIC_BASE_URL",
        HEADROOM_ANTHROPIC_BASE_URL,
        restore_value,
    )?;
    let _ = remove_claude_settings_env(
        HEADROOM_ENABLE_TOOL_SEARCH_KEY,
        HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
        None,
    );
    remove_legacy_vscode_base_url_keys();
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

/// Point settings.json's ANTHROPIC_BASE_URL at Headroom. The cc-switch capture
/// (`tool_manager::cc_switch_capture_path`) outlives a quit so the next backend
/// can reseed from it, which is only right while this write replaces that same
/// URL (the one the quit restored) or our own. Anything else means the user
/// moved off it, e.g. to Claude Official, so drop it before the write: the
/// reconciler reseeds as soon as it sees our URL, and Anthropic OAuth traffic
/// would follow the stale provider. A kept capture is never reported as a
/// replaced gateway: preserved_base_urls would outlive the capture (which the
/// backend drops on a switch to Official), and a later quit would restore the
/// relay over Claude Official. Once written, the reconciler may reconcile
/// again (`tool_manager::cc_switch_routed_path`).
fn configure_claude_base_url() -> Result<(Vec<String>, Vec<String>, Option<String>)> {
    let mut kept = None;
    if let Some(captured) = crate::tool_manager::cc_switch_captured_upstream() {
        let current = read_claude_settings_env("ANTHROPIC_BASE_URL")
            .ok()
            .flatten();
        match current {
            Some(url) if url == captured => kept = Some(url),
            Some(url) if url == HEADROOM_ANTHROPIC_BASE_URL => {}
            _ => crate::tool_manager::clear_cc_switch_capture(),
        }
    }
    let (changed, backups, replaced) =
        configure_claude_settings_env("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)?;
    crate::tool_manager::set_cc_switch_routed(true);
    Ok((
        changed,
        backups,
        replaced.filter(|url| Some(url) != kept.as_ref()),
    ))
}

/// The URL a Claude Code or VS Code disable puts back in place of Headroom's:
/// the provider the cc-switch reconciler last replaced (newer than anything
/// apply saw, and gone with the backend otherwise), else the pre-Headroom URL
/// apply preserved.
fn claude_restore_base_url(state: &ClientSetupState) -> Option<String> {
    crate::tool_manager::cc_switch_captured_upstream().or_else(|| {
        state
            .preserved_base_urls
            .get(normalized_setup_id("claude_code"))
            .cloned()
    })
}

/// Point `env.<env_key>` at Headroom. The third return element is a
/// pre-existing *foreign* value this write replaced (a corporate gateway,
/// LiteLLM, or Bedrock-proxy URL) — callers must preserve it and restore it
/// on disable instead of just deleting the key.
fn configure_claude_settings_env(
    env_key: &str,
    env_value: &str,
) -> Result<(Vec<String>, Vec<String>, Option<String>)> {
    configure_claude_settings_env_impl(env_key, env_value, true)
}

/// Like `configure_claude_settings_env`, but leaves a pre-existing, non-empty
/// value in place. Used for ENABLE_TOOL_SEARCH: we default it on, but a value
/// the user set themselves (e.g. `false` as the LSP tool_reference-400 fallback)
/// wins, mirroring `headroom wrap claude`'s precedence.
fn configure_claude_settings_env_if_absent(
    env_key: &str,
    env_value: &str,
) -> Result<(Vec<String>, Vec<String>, Option<String>)> {
    configure_claude_settings_env_impl(env_key, env_value, false)
}

fn configure_claude_settings_env_impl(
    env_key: &str,
    env_value: &str,
    overwrite_existing: bool,
) -> Result<(Vec<String>, Vec<String>, Option<String>)> {
    let settings_path = claude_settings_path();
    let mut content = if held_or_exists(&settings_path) {
        let raw = read_held_or_disk(&settings_path)
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

    if !overwrite_existing {
        let has_value = env_obj
            .get(env_key)
            .and_then(|value| value.as_str())
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if has_value {
            return Ok((Vec::new(), Vec::new(), None));
        }
    }

    let replaced_foreign_value = env_obj
        .get(env_key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty() && *value != env_value)
        .map(str::to_string);

    let changed = set_json_string(env_obj, env_key, env_value);
    if !changed {
        return Ok((Vec::new(), Vec::new(), None));
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let backup = backup_if_exists(&settings_path)?;
    atomic_write(
        &settings_path,
        &serde_json::to_vec_pretty(&content).context("serializing Claude settings")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok((
        vec![settings_path.display().to_string()],
        backup
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
        replaced_foreign_value,
    ))
}

/// Absolute, quoted `bash.exe` for a Windows hook command. Git for Windows
/// only adds `Git\cmd` to PATH in its default setup while `bash.exe` lives in
/// `Git\bin`, so a bare `bash` resolved on a dev box and nowhere else -- the
/// rtk and markitdown hooks installed fine and then silently never fired.
/// Install locations are probed before PATH deliberately: `System32\bash.exe`
/// is WSL, whose filesystem view cannot see the `C:\Users\...` script path.
/// Bare `bash` remains the last resort -- a hook that needs the user to fix
/// their PATH still beats no hook at all.
fn windows_bash_command() -> String {
    let git_bash = ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|root| PathBuf::from(root).join("Git"))
        .chain(
            std::env::var_os("LOCALAPPDATA")
                .map(|root| PathBuf::from(root).join("Programs").join("Git")),
        )
        .map(|root| root.join("bin").join("bash.exe"))
        .find(|candidate| candidate.exists());

    git_bash
        .or_else(|| find_on_path(&["bash"]))
        .map(|path| format!("\"{}\"", path.display()))
        .unwrap_or_else(|| "bash".to_string())
}

/// The command Claude Code runs for a Headroom PreToolUse hook. The hooks are
/// bash scripts; Claude Code launches them through bash on Windows too, so the
/// interpreter and script are quoted but carry no call operator (see
/// [`join_guard_command`]).
fn hook_shell_command(hook_path: &Path) -> Result<String> {
    if cfg!(target_os = "windows") {
        return Ok(join_guard_command(
            &windows_bash_command(),
            &hook_path.to_string_lossy(),
            true,
            false,
        ));
    }
    hook_path
        .to_str()
        .ok_or_else(|| anyhow!("hook path contains invalid UTF-8: {}", hook_path.display()))
        .map(str::to_string)
}

fn ensure_claude_settings_hook(
    hook_path: &Path,
    matcher: &str,
    marker: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let settings_path = claude_settings_path();
    let mut content = if held_or_exists(&settings_path) {
        let raw = read_held_or_disk(&settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        Value::Object(parse_json_object(&raw, &settings_path)?)
    } else {
        Value::Object(Default::default())
    };

    if !content.is_object() {
        content = Value::Object(Default::default());
    }

    let hook_command = hook_shell_command(hook_path)?;
    let already_present = claude_hook_present_in_value(&content, &hook_command);
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

    strip_hook_from_groups(pre_tool_use, &[marker]);
    pre_tool_use.push(serde_json::json!({
        "matcher": matcher,
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
    atomic_write(
        &settings_path,
        &serde_json::to_vec_pretty(&content).context("serializing Claude hook settings")?,
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

/// Undo `configure_claude_settings_env`: if `env.<env_key>` still equals
/// Headroom's value, put back `restore_value` (the user's pre-Headroom
/// gateway URL) when one was preserved, otherwise delete the key. A key that
/// no longer matches Headroom's value was changed by the user and is left
/// alone.
/// Put the configured provider token where the client will actually send it:
/// `env.ANTHROPIC_AUTH_TOKEN` in `~/.claude/settings.json`.
///
/// This is the same place cc-switch and hand-configured setups keep it, and it
/// is deliberately the only copy outside the keychain: Headroom forwards
/// whatever the client sent rather than injecting credentials of its own, so
/// there is no path that puts this token on the wire from the desktop.
///
/// `None` takes Headroom's own token (`ours`) back out -- used when the
/// override is cleared, so a stale provider token cannot outlive the endpoint
/// it belonged to.
pub fn apply_upstream_auth_token(
    token: Option<&str>,
    ours: Option<&str>,
    replaced: &mut BTreeMap<String, String>,
) -> Result<()> {
    set_or_clear_claude_settings_env(AUTH_TOKEN_ENV, token, ours, replaced)?;
    // settings.json now holds a provider credential, and Claude Code creates it
    // 0644 inside a home that other local accounts can traverse (macOS homes
    // are 0750 group staff, and every user is in staff). Only the owner, who
    // runs Claude Code, needs to read it. atomic_write keeps the mode after.
    // Logged, not returned: the token is already written by now, and failing
    // the save would leave it applied behind an error the user cannot act on.
    #[cfg(unix)]
    if token.is_some_and(|token| !token.is_empty()) {
        use std::os::unix::fs::PermissionsExt;
        let path = claude_settings_path();
        let narrowed = std::fs::metadata(&path).and_then(|meta| {
            let mode = meta.permissions().mode() & 0o700;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        });
        if let Err(err) = narrowed {
            log::warn!("could not make {} owner-only: {err}", path.display());
        }
    }
    Ok(())
}

const AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// Set one `env` key in the client's settings, or, when the value is absent or
/// empty, take out `ours`: the value Headroom itself wrote there last save. A
/// key Headroom never wrote, or one the user has since changed by hand, is left
/// alone. A set records in `replaced` the user's own value it overwrote (first
/// take only), so turning the provider off can put it back.
fn set_or_clear_claude_settings_env(
    env_key: &str,
    value: Option<&str>,
    ours: Option<&str>,
    replaced: &mut BTreeMap<String, String>,
) -> Result<()> {
    let ours = ours.filter(|ours| !ours.is_empty());
    match value.filter(|value| !value.is_empty()) {
        Some(value) => {
            let current = read_claude_settings_env(env_key)?;
            configure_claude_settings_env(env_key, value)?;
            if let Some(current) =
                current.filter(|current| !current.is_empty() && Some(current.as_str()) != ours)
            {
                replaced.entry(env_key.to_string()).or_insert(current);
            }
            Ok(())
        }
        None => match ours {
            Some(ours) => remove_claude_settings_env(env_key, ours, None),
            None => Ok(()),
        },
    }
}

/// Client settings any third-party provider needs beyond the credential, taken
/// from a working GLM setup.
///
/// Both are provider-agnostic. Anthropic-compatible endpoints are slower than
/// Anthropic and the stock client timeout aborts long turns; nonessential
/// traffic goes to Anthropic endpoints a third-party base URL does not serve,
/// so leaving it on only produces errors.
const PROVIDER_CLIENT_ENV: &[(&str, &str)] = &[
    ("API_TIMEOUT_MS", "3000000"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
];

/// The model slots Claude Code reads for its big tiers (verified against the
/// shipped binary). All are written together: leaving one unset sends that
/// slot's Claude model id to a provider that does not serve it the moment the
/// user switches model.
const PROVIDER_MODEL_SLOT_ENV: &[&str] = &[
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
];

/// The cheap tier Claude Code uses for background work (file summaries, title
/// generation). Kept separate from the big slots because every provider below
/// serves a smaller, faster model for it, and pointing it at the big model
/// costs real money and latency on work the user never reads.
const PROVIDER_SMALL_MODEL_SLOT_ENV: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";

/// A provider Headroom can configure from a token alone.
///
/// Every value is from that vendor's own Claude Code documentation, read
/// 2026-09-02. Model ids age faster than releases do, which is why the panel
/// also offers Custom: a stale preset is escapable without an app update.
pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    /// Opus, Sonnet and Fable slots.
    pub model: &'static str,
    /// Haiku slot.
    pub small_model: &'static str,
    pub context_window: &'static str,
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "glm",
        label: "GLM (Z.ai)",
        base_url: "https://api.z.ai/api/anthropic",
        model: "glm-5.3[1m]",
        small_model: "glm-4.7",
        context_window: "1000000",
    },
    ProviderPreset {
        id: "kimi",
        label: "Kimi (Moonshot)",
        base_url: "https://api.moonshot.ai/anthropic",
        model: "kimi-k3[1m]",
        small_model: "kimi-k2.7-code",
        context_window: "1000000",
    },
    ProviderPreset {
        id: "minimax",
        label: "MiniMax",
        base_url: "https://api.minimax.io/anthropic",
        model: "MiniMax-M3",
        small_model: "MiniMax-M3",
        context_window: "512000",
    },
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/anthropic",
        model: "deepseek-v4-pro[1m]",
        small_model: "deepseek-v4-flash",
        context_window: "786432",
    },
];

pub fn provider_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|preset| preset.id == id)
}

/// The client config a configured provider needs beyond the credential. An
/// empty field means "do not write that key", which is what a provider that
/// maps Claude model ids itself wants.
pub struct ProviderClientEnv<'a> {
    pub model: &'a str,
    pub small_model: &'a str,
    pub context_window: &'a str,
}

/// Write the rest of the client config a configured provider needs, or clear
/// all of it with `None` -- a stale model id must not outlive the endpoint that
/// served it, same rule as the token. `previous` is what the last save wrote,
/// the only values a clear takes back out.
pub fn apply_upstream_provider_env(
    env: Option<ProviderClientEnv<'_>>,
    previous: Option<ProviderClientEnv<'_>>,
    replaced: &mut BTreeMap<String, String>,
) -> Result<()> {
    for (env_key, value) in PROVIDER_CLIENT_ENV {
        set_or_clear_claude_settings_env(
            env_key,
            env.is_some().then_some(*value),
            previous.is_some().then_some(*value),
            replaced,
        )?;
    }
    for env_key in PROVIDER_MODEL_SLOT_ENV {
        set_or_clear_claude_settings_env(
            env_key,
            env.as_ref().map(|env| env.model),
            previous.as_ref().map(|env| env.model),
            replaced,
        )?;
    }
    set_or_clear_claude_settings_env(
        PROVIDER_SMALL_MODEL_SLOT_ENV,
        env.as_ref().map(|env| env.small_model),
        previous.as_ref().map(|env| env.small_model),
        replaced,
    )?;
    set_or_clear_claude_settings_env(
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
        env.as_ref().map(|env| env.context_window),
        previous.as_ref().map(|env| env.context_window),
        replaced,
    )
}

/// What the last save wrote for the provider beyond the token.
fn written_provider_env(previous: &UpstreamOverride) -> Option<ProviderClientEnv<'_>> {
    (previous.mode != UpstreamOverrideMode::Off).then(|| ProviderClientEnv {
        model: &previous.model,
        // Saved before `small_model` was kept: re-derive it the way that save
        // did, the preset's, or the one model a hand-entered endpoint got.
        small_model: if previous.small_model.is_empty() {
            provider_preset(&previous.provider)
                .map_or(previous.model.as_str(), |preset| preset.small_model)
        } else {
            &previous.small_model
        },
        context_window: &previous.context_window,
    })
}

/// Write a saved provider into the client config and the keychain.
///
/// `previous` is the last save: what Headroom wrote then is the only thing a
/// clear may take back out, so a key the user set themselves (a cc-switch
/// token, their own model pins, the privacy flag) is never deleted. `next`
/// arrives resolved and leaves with `has_token` and `replaced_env` set.
/// `token`: `None` keeps the stored one, `Some("")` clears it.
pub fn apply_upstream_client_config(
    previous: &UpstreamOverride,
    next: &mut UpstreamOverride,
    token: Option<&str>,
) -> Result<(), String> {
    let _setup = setup_write_lock();
    // Checked before the keychain or settings.json is touched: a rejected
    // field must not leave a provider token live in the client config.
    if !next.context_window.chars().all(|c| c.is_ascii_digit()) {
        return Err("The context window must be a whole number of tokens.".into());
    }
    let configured = next.mode != UpstreamOverrideMode::Off;
    let stored = crate::upstream_override::read_token();
    let token = match token {
        _ if !configured => Some(""),
        // Untouched: re-apply the stored one, because cc-switch or a hand edit
        // may have overwritten the copy in the client's settings -- but only on
        // the endpoint it was entered for. Another provider must never be sent
        // this one's credential.
        None if next.base_url == previous.base_url => stored.as_deref(),
        None => Some(""),
        Some(token) => Some(token),
    };
    if let Some(token) = token {
        if token.is_empty() {
            crate::upstream_override::delete_token()?;
        } else if stored.as_deref() != Some(token) {
            crate::upstream_override::write_token(token)?;
        }
        // A keychain that will not read back (locked, or an ACL from another
        // app signature) cannot say which token Headroom wrote, so the one in
        // the client config is taken to be it rather than stranded there.
        let ours = match &stored {
            Some(stored) => Some(stored.clone()),
            None if previous.has_token => {
                read_claude_settings_env(AUTH_TOKEN_ENV).map_err(|err| err.to_string())?
            }
            None => None,
        };
        let mut replaced = BTreeMap::new();
        apply_upstream_auth_token(Some(token), ours.as_deref(), &mut replaced)
            .map_err(|err| err.to_string())?;
        // The user's own token is a credential: it waits in the keychain, not
        // in launch-profile.json with the rest.
        if let Some(original) = replaced.remove(AUTH_TOKEN_ENV) {
            if crate::upstream_override::read_replaced_token().is_none() {
                crate::upstream_override::write_replaced_token(&original)?;
            }
        }
    }
    next.has_token = token.is_some_and(|token| !token.is_empty());

    let mut replaced = previous.replaced_env.clone();
    apply_upstream_provider_env(
        configured.then_some(ProviderClientEnv {
            model: &next.model,
            small_model: &next.small_model,
            context_window: &next.context_window,
        }),
        written_provider_env(previous),
        &mut replaced,
    )
    .map_err(|err| err.to_string())?;
    if !configured {
        // Back on Anthropic: put back what the user had before the provider,
        // unless they have set that key again since.
        let replaced_token = crate::upstream_override::read_replaced_token();
        replaced.extend(
            replaced_token
                .clone()
                .map(|token| (AUTH_TOKEN_ENV.to_string(), token)),
        );
        for (env_key, original) in std::mem::take(&mut replaced) {
            configure_claude_settings_env_if_absent(&env_key, &original)
                .map_err(|err| err.to_string())?;
        }
        if replaced_token.is_some() {
            crate::upstream_override::delete_replaced_token()?;
        }
    }
    next.replaced_env = replaced;
    Ok(())
}

/// Chisle's PostToolUse hook elides the middle of any tool result over 8k
/// chars, and by default that includes every `mcp__*` tool. Two of those are
/// not safe to cut: Serena's symbol and file reads feed its exact-match edit
/// tools (Chisle exempts `Read` for this reason, not Serena), and
/// `headroom_retrieve` exists to hand back the original the proxy compressed.
/// Chisle's own list minus the `mcp__` wildcard keeps its compression for the
/// tools it was built for. Claude Code passes settings.json `env` to hooks.
const CHISLE_COMPRESS_TOOLS_KEY: &str = "CHISLE_COMPRESS_TOOLS";
const CHISLE_COMPRESS_TOOLS_VALUE: &str = "Bash,Agent,WebFetch,WebSearch,Grep,Glob";

/// Plants (or, on uninstall, removes) the list above. A value the user set
/// themselves wins on install and survives the removal. Not planted without
/// `~/.claude`: Chisle went into Codex alone (Claude Code's own install leaves
/// `~/.claude/plugins`), and a settings.json written there would make Claude
/// Code read as installed (`claude_code_user_state_exists`) for good.
pub fn scope_chisle_compression(scoped: bool) -> Result<()> {
    if scoped && !home_dir().join(".claude").is_dir() {
        return Ok(());
    }
    if scoped {
        configure_claude_settings_env_if_absent(
            CHISLE_COMPRESS_TOOLS_KEY,
            CHISLE_COMPRESS_TOOLS_VALUE,
        )
        .map(|_| ())
    } else {
        remove_claude_settings_env(CHISLE_COMPRESS_TOOLS_KEY, CHISLE_COMPRESS_TOOLS_VALUE, None)
    }
}

/// Current value of one `env` key in `~/.claude/settings.json`, if any.
fn read_claude_settings_env(env_key: &str) -> Result<Option<String>> {
    let settings_path = claude_settings_path();
    if !held_or_exists(&settings_path) {
        return Ok(None);
    }
    let raw = read_held_or_disk(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let root = parse_json_object(&raw, &settings_path)?;
    Ok(root
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(env_key))
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn remove_claude_settings_env(
    env_key: &str,
    expected_value: &str,
    restore_value: Option<&str>,
) -> Result<()> {
    let settings_path = claude_settings_path();
    if !held_or_exists(&settings_path) {
        return Ok(());
    }

    let raw = read_held_or_disk(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut root = parse_json_object(&raw, &settings_path)?;
    let mut changed = false;

    if let Some(Value::Object(env_obj)) = root.get_mut("env") {
        match restore_value {
            Some(original)
                if env_obj.get(env_key).and_then(|v| v.as_str()) == Some(expected_value) =>
            {
                env_obj.insert(env_key.into(), Value::String(original.to_string()));
                changed = true;
            }
            _ => {
                changed |= remove_json_key_if_matches(env_obj, env_key, expected_value);
            }
        }
        if env_obj.is_empty() {
            root.remove("env");
            changed = true;
        }
    }

    if !changed {
        return Ok(());
    }

    let _ = backup_if_exists(&settings_path)?;
    atomic_write(
        &settings_path,
        &serde_json::to_vec_pretty(&Value::Object(root))
            .context("serializing Claude settings for connector removal")?,
    )?;

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

/// Removes the handlers whose `command` contains one of `fragments` from each
/// matcher group, and drops a group only when that empties it. Claude Code's
/// hook editor appends a user's hook to the first group with the same matcher,
/// which can be ours, so dropping the whole group deleted the user's hook too.
/// Returns whether anything was removed.
fn strip_hook_from_groups(entries: &mut Vec<Value>, fragments: &[&str]) -> bool {
    let mut changed = false;
    entries.retain_mut(|entry| {
        let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|hook| {
            !hook
                .get("command")
                .is_some_and(|c| fragments.iter().any(|f| command_contains(c, f)))
        });
        if hooks.len() == before {
            return true;
        }
        changed = true;
        !hooks.is_empty()
    });
    changed
}

fn entry_contains_hook(entry: &Value, hook_fragment: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|hooks| hooks.as_array())
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .is_some_and(|c| command_contains(c, hook_fragment))
            })
        })
        .unwrap_or(false)
}

/// Match a hook `command` against a fragment, tolerating both the Claude string
/// form (`"/usr/bin/python3 /path/guard.py"`) and the argv-array form Codex
/// normalizes to (`["python3", "/path/guard.py"]`). Callers pass the guard
/// *script path* as the fragment so a differing interpreter (system vs Homebrew
/// python3) can't leave the entry behind when the script is deleted.
fn command_contains(command: &Value, fragment: &str) -> bool {
    match command {
        Value::String(s) => s.contains(fragment),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .any(|p| p.contains(fragment)),
        _ => false,
    }
}

/// Best-effort: the keys route nothing today, and VS Code tolerates a
/// settings.json this parser refuses (a pasted shell command), so a failure
/// here aborted Claude Code connect after the routing write and disconnect
/// before the hooks were stripped. An unparseable file is left untouched.
fn remove_legacy_vscode_base_url_keys() -> (Vec<String>, Vec<String>) {
    try_remove_legacy_vscode_base_url_keys().unwrap_or_else(|err| {
        log::log!(
            vscode_settings_failure_level(&err),
            "skipping legacy VS Code base URL cleanup: {err:#}"
        );
        Default::default()
    })
}

/// A settings.json our parsers refuse but VS Code applies (a missing comma,
/// RUST-M2/M3/M4) is the user's file, and the best-effort VS Code edits leave
/// it untouched: local log only, since every distinct parse error message was
/// its own Sentry issue. So is one the OS will not let us read or write
/// (EPERM on macOS, RUST-N7/N8): no change here can grant that access. Any
/// other failure still warns.
fn vscode_settings_failure_level(err: &anyhow::Error) -> log::Level {
    if err.chain().any(|cause| cause.is::<json5::Error>()) || is_permission_denied(err) {
        log::Level::Info
    } else {
        log::Level::Warn
    }
}

fn try_remove_legacy_vscode_base_url_keys() -> Result<(Vec<String>, Vec<String>)> {
    // Deliberately the macOS path only. These keys were written into VS Code's
    // settings.json by macOS-only builds; the connector has since moved to
    // ~/.claude/settings.json, which is where every platform reads and writes
    // today. No Linux or Windows build ever wrote a key for this to clean up,
    // so there is nothing to make platform-aware here.
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
    atomic_write(
        &settings_path,
        &serde_json::to_vec_pretty(&Value::Object(obj))
            .context("serializing VS Code settings for legacy key cleanup")?,
    )?;

    Ok((
        vec![settings_path.display().to_string()],
        backup
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
    ))
}

fn codex_config_toml_path() -> PathBuf {
    codex_home().join("config.toml")
}

// The managed Codex config is split across two marker blocks so each lands in
// the correct TOML scope. `model_provider`/`openai_base_url` are root keys: a
// bare key belongs to the most recently opened `[table]` above it, so appending
// them at end-of-file (as a naive text upsert does) silently absorbs them into
// whatever table the user's config happens to end in (e.g. `[features]`, whose
// values must be booleans), producing
// `invalid type: string "headroom", expected a boolean in features`. The root
// keys therefore go in a block at the *top* of the file (nothing above ⇒ root
// scope), and the `[model_providers.headroom]` table goes in a block at the
// *end*. The table always carries `requires_openai_auth = true`, as Codex's
// built-in `openai` provider does: without it Codex attaches NO credential of
// any kind (`resolve_provider_auth` returns the unauthenticated provider), so
// API-key, `chatgptAuthTokens` and personal-access-token logins all 401'd with
// "Missing bearer" (RUST-C1, RUST-KN) while only `auth_mode: chatgpt` got it.
// The login screen it can raise only shows for a user with no credential at
// all, who could not get a request through either way (#406 predates that).
const CODEX_ROOT_BLOCK_ID: &str = "codex_cli";
const CODEX_TABLE_BLOCK_ID: &str = "codex_cli_provider";

// Codex permanently stamps every thread with the `model_provider` it ran under,
// and its history/projects menu filters threads by the *active* provider set. So
// threads created through Headroom (provider `headroom`) disappear from the menu
// when Codex runs natively (provider `openai`) and vice-versa. To keep the menu
// whole we retag threads to match whichever provider is currently active:
// `openai -> headroom` on connect, `headroom -> openai` on disconnect/quit.
const CODEX_HEADROOM_PROVIDER: &str = "headroom";
const CODEX_NATIVE_PROVIDER: &str = "openai";

/// Directories Codex is known to keep its state store in: the v148 GUI uses
/// `<codex_home>/sqlite/`, the CLI/TUI uses `<codex_home>/`.
fn codex_state_dirs() -> Vec<PathBuf> {
    let codex = codex_home();
    vec![codex.join("sqlite"), codex]
}

/// True when Codex keeps (or kept) a sqlite-backed thread store on this machine,
/// so a *missing* recognized `state_<N>.sqlite` means the store moved/renamed --
/// the case worth a signal. The only evidence we trust is a `state_*.sqlite`-shaped
/// file in `<codex_home>/sqlite/` (GUI) or `<codex_home>/` (CLI/TUI), including a
/// renamed one whose version no longer parses -- exactly the relocation we want to
/// catch. The bare `sqlite/` dir is NOT evidence: it also holds unrelated stores
/// (logs/goals/memories), so a fresh install with those but no thread store would
/// otherwise false-fire "store moved" (Sentry RUST-3R). CLI-only or pre-sqlite
/// installs with just `config.toml`/`sessions/` stay silent -- nothing to split.
fn codex_sqlite_store_expected() -> bool {
    codex_state_dirs().iter().any(|dir| {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("state_") && n.ends_with(".sqlite"))
                })
            })
            .unwrap_or(false)
    })
}

/// Discover every `*.sqlite` file under the known Codex dirs. The thread store's
/// *filename* has changed across Codex versions (`state_5.sqlite`, and whatever
/// comes next), so we no longer couple discovery to a name scheme: every sqlite
/// candidate is handed to `retag_one_codex_db`, which identifies the real store
/// by its `threads` table and no-ops on anything else (logs/goals/memories). A
/// rename can no longer silently split the history menu. A missing dir
/// (`read_dir` error) is skipped. Paths are deduped in case the two dirs ever
/// resolve to the same place.
fn discover_codex_state_dbs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for dir in codex_state_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sqlite")
                && seen.insert(path.clone())
            {
                out.push(path);
            }
        }
    }
    out
}

/// Best-effort retag of Codex thread provider tags so the history menu stays
/// whole across the Headroom proxy boundary. Never fails the caller: a missing
/// store, a missing `threads` table, or a DB locked by a running Codex is logged
/// and skipped. Only rows whose `model_provider` equals `from` are touched, so
/// third-party providers are left alone.
fn retag_codex_thread_providers(from: &str, to: &str) {
    let mut found_thread_store = false;
    let mut unreadable = 0usize;
    let mut skip_reasons: Vec<String> = Vec::new();
    for path in discover_codex_state_dbs() {
        match retag_one_codex_db(&path, from, to) {
            // No `threads` table: unrelated sqlite store (logs/goals/memories).
            Ok(None) => {}
            Ok(Some(n)) => {
                found_thread_store = true;
                if n > 0 {
                    log::info!(
                        "codex retag {from}->{to}: {n} thread(s) in {}",
                        path.display()
                    );
                }
            }
            // Corrupt/unreadable DBs (malformed image, disk I/O error --
            // Sentry RUST-95/96, one macOS-beta box) are environmental and
            // dropped from Sentry by the skip_sentry rule in logging.rs;
            // other causes (e.g. locked past busy_timeout) stay reportable.
            Err(e) => {
                unreadable += 1;
                log::warn!(
                    "codex retag {from}->{to} skipped for {}: {e}",
                    path.display()
                );
                skip_reasons.push(e.to_string());
            }
        }
    }
    report_codex_retag_skips(&skip_reasons);
    // A `state_*.sqlite`-shaped file with no `threads` table means Codex renamed
    // the table itself (discovery already survives a file rename). Only flag when
    // the store-shaped name is present, so a clean or CLI-only / pre-sqlite
    // machine -- or one with just logs/goals/memories DBs -- stays silent
    // (Sentry RUST-3R). This is the last remaining schema-drift signal worth a
    // release (Sentry RUST-43).
    if !found_thread_store && codex_sqlite_store_expected() {
        if unreadable > 0 {
            // Cannot distinguish "Codex renamed the table" from "the disk is
            // broken" when any candidate failed to open: RUST-95 false-fired
            // the rename signal on a machine whose sqlite files all threw
            // disk I/O errors. Local log only (skip_sentry rule).
            log::warn!(
                "codex retag {from}->{to}: no `threads` table found but \
                 {unreadable} candidate(s) unreadable; skipping the rename signal"
            );
        } else {
            log::warn!(
                "codex retag {from}->{to}: a state_*.sqlite is present but has no \
                 `threads` table under {dirs:?}; the history menu may split. Codex \
                 likely renamed the table.",
                dirs = codex_state_dirs(),
            );
        }
    }
}

/// One Sentry event per retag PASS, not one per file.
///
/// The per-file warn names the DB, so the log bridge grouped it by filename: a
/// running Codex holding three of its own sqlite files opened three issues in
/// the same second for one condition (RUST-EK, RUST-EM, RUST-EN). The bridged
/// twin is dropped in logging.rs and the reasons ride along as an extra.
///
/// The environmental causes stay dropped (a DB the user's disk corrupted is
/// not ours to fix, RUST-95/96), but a real one anywhere in the pass still
/// reports: a lock outliving `busy_timeout` is how we would learn that
/// assumption went stale. Capped at one event per class per session by
/// `claim_retag_skip_report_slot`.
fn codex_retag_skip_class(reasons: &[String]) -> Option<&'static str> {
    reasons.iter().find_map(|reason| {
        let lower = reason.to_ascii_lowercase();
        if lower.contains("malformed") || lower.contains("disk i/o error") {
            None
        } else if lower.contains("is locked") {
            Some("locked")
        } else {
            Some("other")
        }
    })
}

/// Skip classes already reported this session, so the event stays a HOST
/// count. A retag pass runs on every app launch and every quit, and
/// `busy_timeout` is 750ms -- which a Codex actively writing its own store
/// blows past routinely. Without this the one condition files a Warning per
/// launch, forever, on every user who keeps Codex open. Same shape as
/// `claim_transient_report_slot` in pricing.rs.
static RETAG_SKIP_REPORTED: std::sync::Mutex<std::collections::BTreeSet<&'static str>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

fn claim_retag_skip_report_slot(class: &'static str) -> bool {
    let mut seen = RETAG_SKIP_REPORTED.lock().unwrap_or_else(|e| {
        // A poisoned lock must not silence reporting outright.
        RETAG_SKIP_REPORTED.clear_poison();
        e.into_inner()
    });
    seen.insert(class)
}

fn report_codex_retag_skips(reasons: &[String]) {
    let Some(class) = codex_retag_skip_class(reasons) else {
        return;
    };
    if !claim_retag_skip_report_slot(class) {
        return;
    }
    let sample: Vec<String> = {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for reason in reasons {
            seen.insert(reason.chars().take(160).collect());
        }
        seen.into_iter().take(5).collect()
    };
    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "codex_retag");
            scope.set_extra("skipped_files", (reasons.len() as u64).into());
            scope.set_extra("reasons", sample.join(" | ").into());
            scope.set_fingerprint(Some(&["codex_retag_skipped", class]));
        },
        || {
            sentry::capture_message(
                &format!(
                    "codex retag skipped {} database(s) ({class})",
                    reasons.len()
                ),
                sentry::Level::Warning,
            );
        },
    );
}

fn retag_one_codex_db(path: &Path, from: &str, to: &str) -> rusqlite::Result<Option<usize>> {
    use rusqlite::OptionalExtension;

    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(750))?;
    // No-op (without erroring) on builds whose store lacks the threads table.
    let has_table = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'threads'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_table {
        return Ok(None);
    }
    conn.execute(
        "UPDATE threads SET model_provider = ?2 WHERE model_provider = ?1",
        rusqlite::params![from, to],
    )
    .map(Some)
}

/// Retag Codex threads back to the native provider. Exposed for the app-quit
/// hook in `lib.rs`, which covers exit paths (Cmd-Q, dock quit, signals) that
/// bypass `clear_client_setups` and therefore the disconnect retag.
pub fn retag_codex_threads_to_native() {
    retag_codex_thread_providers(CODEX_HEADROOM_PROVIDER, CODEX_NATIVE_PROVIDER);
}

/// Pull Codex threads into the headroom provider menu. Exposed for the
/// app-launch hook in `lib.rs`, which must undo the quit-time native retag on
/// the exit paths (Cmd-Q, dock quit, app-update restart) that never populate
/// `remembered_clients` and are therefore skipped by `restore_client_setups`.
pub fn retag_codex_threads_to_headroom() {
    retag_codex_thread_providers(CODEX_NATIVE_PROVIDER, CODEX_HEADROOM_PROVIDER);
}

/// The root keys the managed `codex_cli` block owns, Headroom's value for each,
/// and the `preserved_base_urls` entry that holds a user's own value until
/// disable restores it. `model_provider` keeps the bare `codex_cli` entry older
/// builds persisted.
const CODEX_ROOT_KEYS: [(&str, &str, &str); 2] = [
    ("model_provider", "headroom", "codex_cli"),
    (
        "openai_base_url",
        HEADROOM_OPENAI_BASE_URL,
        "codex_cli_openai_base_url",
    ),
];

/// Whether a comment-stripped TOML line assigns one of [`CODEX_ROOT_KEYS`].
fn is_codex_root_key_line(code: &str) -> bool {
    code.split_once('=')
        .is_some_and(|(key, _)| CODEX_ROOT_KEYS.iter().any(|(k, ..)| key.trim() == *k))
}

fn codex_root_keys_body() -> String {
    format!(
        "model_provider = \"headroom\"\n\
         openai_base_url = \"{base}\"",
        base = HEADROOM_OPENAI_BASE_URL,
    )
}

fn codex_provider_table_body() -> String {
    format!(
        "[model_providers.headroom]\n\
         name = \"Headroom persistent proxy\"\n\
         base_url = \"{base}\"\n\
         supports_websockets = false\n\
         requires_openai_auth = true",
        base = HEADROOM_OPENAI_BASE_URL,
    )
}

fn codex_marker_block(block_id: &str, body: &str) -> String {
    format!("# >>> headroom:{block_id} >>>\n{body}\n# <<< headroom:{block_id} <<<\n")
}

/// Remove every Headroom-managed artifact from Codex `config.toml` text: both
/// managed marker blocks, plus any orphan root keys an older (buggy) build may
/// have left absorbed into a preceding table. Leaves all other content intact.
fn strip_codex_managed_toml(content: &str) -> String {
    // Codex's TOML writer appends new tables *before* a trailing comment, and
    // our table block's closing marker is the last line of the file -- so
    // Codex-owned tables ([projects.*] trust, [hooks.state], [windows]) end up
    // trapped INSIDE the managed block. Pull them out before stripping, or a
    // disable/rewrite silently deletes the user's trust and sandbox state.
    let rescued = rescue_foreign_toml_from_block(content, CODEX_ROOT_BLOCK_ID, None);
    let rescued = rescue_foreign_toml_from_block(
        &rescued,
        CODEX_TABLE_BLOCK_ID,
        Some("[model_providers.headroom]"),
    );
    let without_blocks = strip_marker_block(
        &strip_marker_block(&rescued, CODEX_ROOT_BLOCK_ID),
        CODEX_TABLE_BLOCK_ID,
    );
    // Exact values only: this runs on every quit, and a loopback URL of the
    // user's own (LM Studio on :1234) is not ours to delete.
    let openai_orphan = format!("openai_base_url = \"{HEADROOM_OPENAI_BASE_URL}\"");
    without_blocks
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed == "model_provider = \"headroom\"" || trimmed == openai_orphan)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Move every TOML table we do not own out of a managed marker block, re-emitting
/// it after the closing marker (byte-preserved, order kept). `owned_table` is the
/// one table header the block legitimately contains (`None` for the root-keys
/// block). The root-keys block owns only the [`CODEX_ROOT_KEYS`] assignments:
/// any other root key in it is Codex's (see below) and is moved out the same
/// way, landing right after the end marker, still in root scope. Handles
/// repeated blocks in one pass since classification is line-state based, not
/// index based.
// ponytail: a comment line directly above a trapped table stays with the block
// (and is dropped on strip); attach comment-carrying to the following header if
// a real config ever shows up with one.
fn rescue_foreign_toml_from_block(
    content: &str,
    block_id: &str,
    owned_table: Option<&str>,
) -> String {
    rescue_foreign_toml(
        content,
        &format!("# >>> headroom:{block_id} >>>"),
        &format!("# <<< headroom:{block_id} <<<"),
        |header| owned_table == Some(header),
        owned_table.is_none(),
        false,
    )
}

/// The engine behind [`rescue_foreign_toml_from_block`] and
/// [`rescue_foreign_toml_from_mcp_spans`]: between the `start`/`end` marker
/// lines, a table is kept when `owns` accepts its header; `root_keys_only`
/// also moves out any root key that is not one of [`CODEX_ROOT_KEYS`].
/// Moved lines land after the block's end marker, or with `before_start` just
/// before its start marker.
fn rescue_foreign_toml(
    content: &str,
    start: &str,
    end: &str,
    owns: impl Fn(&str) -> bool,
    root_keys_only: bool,
    before_start: bool,
) -> String {
    fn place<'a>(out: &mut Vec<&'a str>, rescued: &mut Vec<&'a str>, at: Option<usize>) {
        if rescued.is_empty() {
            return;
        }
        if let Some(at) = at {
            let at = before_adjacent_headroom_blocks(out, at);
            while rescued.last().is_some_and(|l| l.trim().is_empty()) {
                rescued.pop();
            }
            rescued.push("");
            if at > 0 && !out[at - 1].trim().is_empty() {
                rescued.insert(0, "");
            }
            out.splice(at..at, rescued.drain(..));
        } else {
            out.push("");
            out.append(rescued);
        }
    }
    let mut out: Vec<&str> = Vec::new();
    let mut rescued: Vec<&str> = Vec::new();
    let mut in_block = false;
    let mut in_foreign_table = false;
    let mut start_at = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == start {
            in_block = true;
            in_foreign_table = false;
            start_at = out.len();
            out.push(line);
            continue;
        }
        if trimmed == end {
            in_block = false;
            out.push(line);
            place(&mut out, &mut rescued, before_start.then_some(start_at));
            continue;
        }
        if in_block {
            let code = line.split('#').next().unwrap_or("").trim();
            if code.starts_with('[') && code.ends_with(']') {
                in_foreign_table = !owns(code);
            }
            // Codex's TOML writer appends a new root key (/model's `model`,
            // `model_reasoning_effort`) after the last root key -- our
            // openai_base_url -- so it lands before our end marker too.
            let foreign_root_line =
                root_keys_only && !code.is_empty() && !is_codex_root_key_line(code);
            if in_foreign_table || foreign_root_line {
                rescued.push(line);
                continue;
            }
        }
        out.push(line);
    }
    // Unterminated block (missing end marker): don't lose what we set aside.
    place(&mut out, &mut rescued, before_start.then_some(start_at));
    out.join("\n")
}

/// Pure-text removal of every `# >>> headroom:<id> >>> ... <<<` block. Loops so
/// a config that already holds duplicate managed blocks (interrupted write,
/// older build) is fully cleaned, not left with one survivor that regenerates.
fn strip_marker_block(content: &str, block_id: &str) -> String {
    let start = format!("# >>> headroom:{block_id} >>>");
    let end = format!("# <<< headroom:{block_id} <<<");
    let mut out = content.to_string();
    while let Some(start_idx) = out.find(&start) {
        let Some(end_idx) = out[start_idx..].find(&end).map(|rel| start_idx + rel) else {
            break;
        };
        let tail = out[end_idx + end.len()..]
            .trim_start_matches('\n')
            .to_string();
        let head = out[..start_idx].trim_end().to_string();
        let mut rebuilt = String::with_capacity(out.len());
        rebuilt.push_str(&head);
        if !rebuilt.is_empty() && !tail.is_empty() {
            rebuilt.push('\n');
        }
        rebuilt.push_str(&tail);
        out = rebuilt;
    }
    // Stray markers. Codex's TOML writer keeps our start marker as the leading
    // comment of `[model_providers.headroom]`, so when it drops that table the
    // start goes with it and the trailing end marker (document trailer) stays.
    // This used to `break` on "end before start" and leave the file alone,
    // which made every render a no-op fixpoint and every verify a miss:
    // hourly "still failing after re-apply" for as long as the file lived
    // (RUST-BZ). Drop any marker line that is not part of a pair.
    if out.contains(&start) || out.contains(&end) {
        let had_newline = out.ends_with('\n');
        out = out
            .lines()
            .filter(|line| {
                let line = line.trim();
                line != start && line != end
            })
            .collect::<Vec<_>>()
            .join("\n");
        if had_newline && !out.is_empty() {
            out.push('\n');
        }
    }
    out
}

/// Root-scope lines assigning `key` in a Codex config. Root scope only: the
/// same key inside a `[profiles.x]`/`[model_providers.x]` table belongs to that
/// table, not the global route.
fn codex_root_key_lines<'a>(content: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    let mut in_root = true;
    content.lines().filter(move |raw| {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_root = false;
        }
        in_root && line.split_once('=').is_some_and(|(k, _)| k.trim() == key)
    })
}

/// The root-scope value of `key` in a Codex config when set to something other
/// than Headroom's `ours`. This is the Codex analog of a foreign
/// `ANTHROPIC_BASE_URL` -- captured on apply and restored on disable. Each line
/// is read as TOML, so a literal ('single-quoted') string or a trailing comment
/// yields the value Codex itself sees.
fn codex_foreign_root_value(content: &str, key: &str, ours: &str) -> Option<String> {
    codex_root_key_lines(content, key)
        .filter_map(|raw| {
            let line = toml::from_str::<toml::Table>(raw).ok()?;
            line.get(key)?.as_str().map(str::to_owned)
        })
        .find(|value| !value.is_empty() && value != ours)
}

/// Drop every root-scope assignment of a [`CODEX_ROOT_KEYS`] key, whatever its
/// value or spacing, so the managed block's copies aren't duplicate root keys
/// (invalid TOML: Codex refuses to load its config). The user's own values are
/// captured by [`codex_foreign_root_value`] before this runs. The same keys
/// inside a table are left untouched.
fn strip_codex_root_keys(content: &str) -> String {
    let mut in_root = true;
    content
        .lines()
        .filter(|raw| {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.starts_with('[') && line.ends_with(']') {
                in_root = false;
                return true;
            }
            !(in_root && is_codex_root_key_line(line))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drop an unmarked `[model_providers.headroom]` table so the managed block's
/// copy isn't a duplicate table key. This is the table-scope analog of
/// [`strip_codex_root_keys`]: a second `[model_providers.headroom]`
/// makes Codex refuse to load its *entire* config, so one stale table breaks
/// every `codex` invocation, not just our routing (Sentry RUST-6K).
///
/// Such a table is left behind by an OSS `pip install headroom` (which wrote the
/// provider with no marker comments) or by a user who hand-added it before
/// marker blocks existed. It is only ever removed for the `headroom` provider
/// name -- our own namespace, and byte-identical in intent to the block we are
/// about to write. Every other provider table is left untouched.
///
/// Marker-wrapped copies are already gone by the time this runs
/// ([`strip_codex_managed_toml`]), so this only sees unmarked leftovers.
fn strip_codex_headroom_provider_table(content: &str) -> String {
    let mut dropping = false;
    content
        .lines()
        .filter(|raw| {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.starts_with('[') && line.ends_with(']') {
                // A new table header always ends any drop; it starts one only
                // for our own provider name.
                dropping = line == "[model_providers.headroom]";
                return !dropping;
            }
            !dropping
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Restore a preserved pre-Headroom root key (`model_provider`,
/// `openai_base_url`) after teardown, so a gateway/alternate-provider user
/// isn't silently left on api.openai.com. No-op if the config already has that
/// root key (user re-added their own): a second one is invalid TOML.
fn restore_codex_root_key(key: &str, value: &str) -> Result<()> {
    let path = codex_config_toml_path();
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    if codex_root_key_lines(&existing, key).next().is_some() {
        return Ok(());
    }
    let line = format!("{key} = {}", toml_basic_string(value));
    let trimmed = existing.trim();
    let rebuilt = if trimmed.is_empty() {
        format!("{line}\n")
    } else {
        format!("{line}\n{trimmed}\n")
    };
    let _ = backup_if_exists(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    atomic_write(&path, rebuilt.as_bytes())?;
    Ok(())
}

/// Reconstruct `config.toml` with the managed root keys pinned to the top and
/// the provider table appended at the end, around the user's other content.
fn render_codex_config(existing: &str) -> String {
    let mid = strip_codex_managed_toml(existing);
    // Drop a foreign root model_provider/openai_base_url too, else our managed
    // copies collide with them as duplicate root keys.
    let mid = strip_codex_root_keys(&mid);
    // Same collision one scope down: an unmarked `[model_providers.headroom]`
    // table would duplicate the one in the managed block below.
    let mid = strip_codex_headroom_provider_table(&mid);
    let mid = mid.trim();

    let mut out = codex_marker_block(CODEX_ROOT_BLOCK_ID, &codex_root_keys_body());
    if !mid.is_empty() {
        out.push('\n');
        out.push_str(mid);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&codex_marker_block(
        CODEX_TABLE_BLOCK_ID,
        &codex_provider_table_body(),
    ));
    out
}

/// A [`CODEX_ROOT_KEYS`] state entry and the user's own root value it holds.
type CodexPreservedKey = (&'static str, String);

/// Returns `(changed_files, backup_files, preserved)`: the pre-existing
/// *foreign* root values this write replaced -- callers must preserve them and
/// restore them on disable instead of dropping the user onto api.openai.com
/// (mirrors [`configure_claude_settings_env`]).
fn configure_codex_provider_block() -> Result<(Vec<String>, Vec<String>, Vec<CodexPreservedKey>)> {
    let path = codex_config_toml_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };

    let preserved: Vec<CodexPreservedKey> = CODEX_ROOT_KEYS
        .iter()
        .filter_map(|&(key, ours, entry)| {
            Some((entry, codex_foreign_root_value(&existing, key, ours)?))
        })
        .collect();
    let updated = render_codex_config(&existing);
    if updated == existing {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    // Never turn a config Codex loads into one it refuses (a duplicate root key
    // or table makes Codex reject the whole file): keep the user's file.
    if existing.parse::<toml::Value>().is_ok() && updated.parse::<toml::Value>().is_err() {
        return Err(anyhow!(
            "rendered {} is not valid TOML; refusing to overwrite",
            path.display()
        ));
    }

    let backup = backup_if_exists(&path)?;
    atomic_write(&path, updated.as_bytes())?;

    let mut backup_files = Vec::new();
    if let Some(backup_path) = backup {
        backup_files.push(backup_path.display().to_string());
    }
    Ok((vec![path.display().to_string()], backup_files, preserved))
}

/// Rewrite the `command` of the `[mcp_servers.headroom]` table in
/// `~/.codex/config.toml` to the absolute `entrypoint`. The upstream Python
/// registrar writes a bare `command = "headroom"` that relies on PATH; when
/// the managed runtime relocates, `~/.local/bin/headroom` dangles and Codex
/// fails to start the MCP server with `No such file or directory`. Desktop
/// re-runs `mcp install` on every launch, so pinning the absolute path here
/// self-heals the config. No-op when the config or table is absent. Targets
/// the table by header rather than the Headroom marker block, which the
/// upstream registrar can mis-place around unrelated user tables.
pub fn pin_codex_mcp_command(entrypoint: &Path) -> Result<Option<String>> {
    pin_toml_mcp_command(&codex_config_toml_path(), entrypoint)
}

/// The shared rewrite behind [`pin_codex_mcp_command`] and
/// [`pin_grok_mcp_command`]: both CLIs read the same `[mcp_servers.headroom]`
/// table shape. Also turns the upstream beacon off in its
/// `[mcp_servers.headroom.env]`: on by default upstream, and the MCP server
/// reports to it (the proxy already runs with it off).
fn pin_toml_mcp_command(path: &Path, entrypoint: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let target_line = format!(
        "command = {}",
        toml_basic_string(&entrypoint.to_string_lossy())
    );
    // The upstream registrar may resolve the server as `<python> -m headroom.cli
    // mcp serve`. Pinning only `command` to the console script would leave
    // `args = ["-m", "headroom.cli", ...]` behind, and `headroom -m ...` fails
    // with "No such option '-m'" — so the args must be pinned together.
    let target_args_line = r#"args = ["mcp", "serve"]"#;

    let beacon_line = r#"HEADROOM_BEACON = "off""#;
    let mut in_headroom_table = false;
    let mut in_env_table = false;
    let mut replaced = false;
    // When the replaced `args` value is a multi-line array, the continuation
    // lines ("-m", / "headroom.cli", / ]) must be dropped too, or the rebuilt
    // file is invalid TOML and Codex fails to load its config entirely.
    let mut skip_array_depth: i32 = 0;
    let mut out: Vec<String> = Vec::with_capacity(content.lines().count());
    for line in content.lines() {
        if skip_array_depth > 0 {
            skip_array_depth += bracket_delta(line);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_headroom_table = trimmed == "[mcp_servers.headroom]";
            in_env_table = trimmed == "[mcp_servers.headroom.env]";
            out.push(line.to_string());
            if in_env_table {
                out.push(beacon_line.to_string());
            }
            continue;
        }
        if in_env_table
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "HEADROOM_BEACON")
        {
            continue;
        }
        if in_headroom_table {
            match trimmed
                .split_once('=')
                .map(|(key, value)| (key.trim(), value))
            {
                Some(("command", _)) => {
                    out.push(target_line.clone());
                    replaced = true;
                    continue;
                }
                Some(("args", value)) => {
                    out.push(target_args_line.to_string());
                    skip_array_depth = bracket_delta(value).max(0);
                    continue;
                }
                _ => {}
            }
        }
        out.push(line.to_string());
    }

    if !replaced {
        return Ok(None);
    }
    let mut rebuilt = out.join("\n");
    if content.ends_with('\n') {
        rebuilt.push('\n');
    }
    if rebuilt == content {
        return Ok(None);
    }
    // Never publish a config Codex can't parse — bail and leave the user's
    // file untouched instead.
    toml::from_str::<toml::Value>(&rebuilt).with_context(|| {
        format!(
            "rebuilt {} is not valid TOML; refusing to overwrite",
            path.display()
        )
    })?;
    let _ = backup_if_exists(path)?;
    atomic_write(path, rebuilt.as_bytes())?;
    Ok(Some(path.display().to_string()))
}

/// Net `[` minus `]` on a line, ignoring brackets inside basic strings.
/// Good enough for tracking whether a TOML array value has closed.
fn bracket_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => break,
            '[' if !in_string => delta += 1,
            ']' if !in_string => delta -= 1,
            _ => {}
        }
    }
    delta
}

const GROK_PROXY_BLOCK_ID: &str = "grok_build_proxy";

fn grok_config_toml_path() -> PathBuf {
    grok_home().join("config.toml")
}

fn grok_proxy_body() -> String {
    format!(
        "[model.grok-build]\nbase_url = \"{base}\"",
        base = HEADROOM_GROK_PROXY_BASE_URL
    )
}

fn strip_grok_managed_toml(content: &str) -> String {
    strip_marker_block(content, GROK_PROXY_BLOCK_ID)
}

/// Locate a `[model.grok-build]` table in `lines`: returns the header line
/// index and, when present, the index of its `base_url` line.
fn find_grok_build_table(lines: &[&str]) -> Option<(usize, Option<usize>)> {
    let mut header_idx = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let code = trimmed.split('#').next().unwrap_or("").trim_end();
        if code.starts_with('[') && code.ends_with(']') {
            if code == "[model.grok-build]" {
                header_idx = Some(idx);
            } else if let Some(header) = header_idx {
                // Next table started: the grok-build table had no base_url.
                return Some((header, None));
            }
            continue;
        }
        if let Some(header) = header_idx {
            if trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "base_url")
            {
                return Some((header, Some(idx)));
            }
        }
    }
    header_idx.map(|h| (h, None))
}

/// Extract the string value of a `key = "value"` TOML line. The line is read
/// as TOML, so escapes (`\\` in a Windows path), a literal ('single-quoted')
/// string or a trailing comment yield the value the client itself sees.
fn toml_line_value(line: &str) -> Option<String> {
    let table = toml::from_str::<toml::Table>(line).ok()?;
    table.values().next()?.as_str().map(str::to_owned)
}

/// Rewrite `base_url` inside a user-owned `[model.grok-build]` table (e.g.
/// written by `headroom wrap grok`), keeping the previous value in a trailing
/// `# was:` comment so disable can restore it. Mirrors the upstream Python
/// registrar (headroom/providers/grok_build/config.py). Returns `None` when no
/// such table exists.
fn redirect_existing_grok_build_base_url(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let (header_idx, base_url_idx) = find_grok_build_table(&lines)?;
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    match base_url_idx {
        Some(idx) => {
            let old = toml_line_value(lines[idx]);
            if old.as_deref() == Some(HEADROOM_GROK_PROXY_BASE_URL) {
                return Some(format!("{content}\n"));
            }
            let indent: String = lines[idx]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            out[idx] = match old {
                Some(old) => {
                    format!("{indent}base_url = \"{HEADROOM_GROK_PROXY_BASE_URL}\"  # was: {old}")
                }
                None => format!("{indent}base_url = \"{HEADROOM_GROK_PROXY_BASE_URL}\""),
            };
        }
        None => out.insert(
            header_idx + 1,
            format!("base_url = \"{HEADROOM_GROK_PROXY_BASE_URL}\""),
        ),
    }
    let mut rebuilt = out.join("\n");
    rebuilt.push('\n');
    Some(rebuilt)
}

/// Undo a `base_url` redirect left by [`redirect_existing_grok_build_base_url`]:
/// restore the value recorded in the `# was:` comment, or drop the line when
/// Headroom inserted it into a table that had none.
fn restore_grok_build_base_url(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let Some((_, Some(idx))) = find_grok_build_table(&lines) else {
        return content.to_string();
    };
    let line = lines[idx];
    if toml_line_value(line).as_deref() != Some(HEADROOM_GROK_PROXY_BASE_URL) {
        return content.to_string();
    }
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    if let Some((_, was)) = line.split_once("# was: ") {
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        out[idx] = format!("{indent}base_url = {}", toml_basic_string(was.trim()));
    } else {
        out.remove(idx);
    }
    out.join("\n")
}

fn render_grok_config(existing: &str) -> String {
    let mid = strip_grok_managed_toml(existing);
    let mid = mid.trim();

    // A user-owned [model.grok-build] table must not be duplicated - a second
    // table is invalid TOML. Redirect its base_url in place instead.
    if let Some(redirected) = redirect_existing_grok_build_base_url(mid) {
        return redirected;
    }

    // The managed block opens a [model.grok-build] table, so it must sit after
    // the user's content: any top-level key following the block would be
    // absorbed into the table.
    let block = codex_marker_block(GROK_PROXY_BLOCK_ID, &grok_proxy_body());
    if mid.is_empty() {
        return block;
    }
    format!("{mid}\n\n{block}")
}

fn configure_grok_proxy_block() -> Result<(Vec<String>, Vec<String>)> {
    let path = grok_config_toml_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };

    let updated = render_grok_config(&existing);
    if updated == existing {
        return Ok((Vec::new(), Vec::new()));
    }
    // A table header the text scan misses (quoted key, odd spacing) would get
    // a duplicate [model.grok-build], which Grok refuses: keep the user's file.
    if existing.parse::<toml::Value>().is_ok() && updated.parse::<toml::Value>().is_err() {
        return Err(anyhow!(
            "rendered {} is not valid TOML; refusing to overwrite",
            path.display()
        ));
    }

    let backup = backup_if_exists(&path)?;
    atomic_write(&path, updated.as_bytes())?;

    let mut backup_files = Vec::new();
    if let Some(backup_path) = backup {
        backup_files.push(backup_path.display().to_string());
    }
    Ok((vec![path.display().to_string()], backup_files))
}

fn grok_proxy_block_matches() -> Result<bool> {
    let path = grok_config_toml_path();
    if !path.exists() {
        return Ok(false);
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let base_url = format!("base_url = \"{}\"", HEADROOM_GROK_PROXY_BASE_URL);
    if marker_block_contains(&content, GROK_PROXY_BLOCK_ID, &base_url) {
        return Ok(true);
    }
    // Redirected user-owned table (no managed block).
    let lines: Vec<&str> = content.lines().collect();
    Ok(matches!(
        find_grok_build_table(&lines),
        Some((_, Some(idx)))
            if toml_line_value(lines[idx]).as_deref() == Some(HEADROOM_GROK_PROXY_BASE_URL)
    ))
}

fn remove_grok_proxy_block() -> Result<()> {
    let path = grok_config_toml_path();
    if !path.exists() {
        return Ok(());
    }
    let existing =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let stripped = restore_grok_build_base_url(&strip_grok_managed_toml(&existing));
    let normalized = {
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}\n")
        }
    };
    if normalized == existing {
        return Ok(());
    }
    let _ = backup_if_exists(&path)?;
    atomic_write(&path, normalized.as_bytes())?;
    Ok(())
}

fn disable_grok_build() -> Result<()> {
    remove_grok_proxy_block()?;
    let shell_targets = all_shell_paths();
    let _ = remove_shell_block(&shell_targets, "grok_build");
    Ok(())
}

/// Both OpenCode @ai-sdk transports append their endpoint to a `/v1` base
/// (`/messages`, `/responses`), so anthropic and openai share one proxy URL.
/// Verified against opencode 1.18.5.
const HEADROOM_OPENCODE_BASE_URL: &str = "http://127.0.0.1:6767/v1";
const OPENCODE_MANAGED_PROVIDERS: [&str; 2] = ["anthropic", "openai"];

/// OpenCode resolves its dirs with `xdg-basedir`, which has no Windows branch:
/// `%USERPROFILE%\.config\opencode` there too, never `%APPDATA%`. An
/// `%APPDATA%` config (0.7.x-0.9.25) was one OpenCode never read, so Windows
/// OpenCode was never routed (RUST-K2).
fn opencode_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("opencode")
}

/// OpenCode's global config file. Honors `$OPENCODE_CONFIG`; otherwise
/// prefers `opencode.jsonc` when it exists (OpenCode does the same).
fn opencode_config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("OPENCODE_CONFIG").filter(|v| !v.is_empty()) {
        return PathBuf::from(explicit);
    }
    let dir = opencode_config_dir();
    let jsonc = dir.join("opencode.jsonc");
    if jsonc.exists() {
        jsonc
    } else {
        dir.join("opencode.json")
    }
}

fn opencode_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("share"))
        .join("opencode")
}

fn read_opencode_config(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => {
            let value: serde_json::Value =
                serde_json::from_str(&strip_jsonc(&raw)).with_context(|| {
                    format!(
                        "parsing {} failed (JSON/JSONC); refusing to overwrite potentially valid user config",
                        path.display()
                    )
                })?;
            // Same contract as parse_json_object's JSON5 fallback: writers
            // re-serialize with serde_json (comment-free), the byte-for-byte
            // .headroom-backup keeps the original. Local info only - expected,
            // benign behavior (RUST-61 was setup refusing valid .jsonc files).
            log::info!(
                "{} contains JSONC syntax (comments/trailing commas); a Headroom rewrite will normalize it to strict JSON - the original is kept as a .headroom-backup file",
                path.display()
            );
            value
        }
    };
    if !value.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    Ok(value)
}

/// Strip `//` and `/* */` comments plus trailing commas so a JSONC config can
/// be parsed with serde_json. String contents (including escapes) survive.
fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    // Bytes, not chars: `byte as char` turned each UTF-8 byte of a non-ASCII
    // character into its own Latin-1 char. Only whole ASCII bytes and whole
    // comments are dropped, so the output stays valid UTF-8.
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(bytes[i]);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(b'"');
                i += 1;
            }
            '/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            ',' => {
                // Trailing comma: skip when the next non-whitespace,
                // non-comment character closes the container.
                let mut j = i + 1;
                loop {
                    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'/') {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                            j += 1;
                        }
                        j = (j + 2).min(bytes.len());
                        continue;
                    }
                    break;
                }
                if !matches!(bytes.get(j), Some(b'}') | Some(b']')) {
                    out.push(b',');
                }
                i += 1;
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn opencode_provider_base_url(config: &serde_json::Value, provider: &str) -> Option<String> {
    config
        .get("provider")?
        .get(provider)?
        .get("options")?
        .get("baseURL")?
        .as_str()
        .map(str::to_string)
}

fn ensure_json_object<'a>(
    value: &'a mut serde_json::Value,
    key: &str,
) -> &'a mut serde_json::Value {
    let obj = value
        .as_object_mut()
        .expect("read_opencode_config guarantees an object root");
    let entry = obj
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    entry
}

fn set_opencode_provider_base_url(config: &mut serde_json::Value, provider: &str, url: &str) {
    let options = ensure_json_object(
        ensure_json_object(ensure_json_object(config, "provider"), provider),
        "options",
    );
    options
        .as_object_mut()
        .expect("ensure_json_object returns an object")
        .insert("baseURL".into(), serde_json::json!(url));
}

/// Remove the managed `baseURL`, pruning `options`/provider/`provider` map
/// entries that end up empty so disable leaves no husks behind.
fn remove_opencode_provider_base_url(config: &mut serde_json::Value, provider: &str) {
    let Some(providers) = config.get_mut("provider").and_then(|v| v.as_object_mut()) else {
        return;
    };
    if let Some(entry) = providers.get_mut(provider) {
        if let Some(options) = entry.get_mut("options").and_then(|v| v.as_object_mut()) {
            options.remove("baseURL");
            if options.is_empty() {
                entry.as_object_mut().map(|o| o.remove("options"));
            }
        }
        if entry.as_object().is_some_and(|o| o.is_empty()) {
            providers.remove(provider);
        }
    }
    if providers.is_empty() {
        config.as_object_mut().map(|o| o.remove("provider"));
    }
}

fn write_opencode_config(path: &Path, config: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut payload = serde_json::to_string_pretty(config)
        .with_context(|| format!("serializing {}", path.display()))?;
    payload.push('\n');
    atomic_write(path, payload.as_bytes())
}

/// Self-contained OpenCode transport plugin (all-provider routing via
/// `x-headroom-base-url`), vendored from headroom-ai's `plugins/opencode`
/// built with the desktop wrapper entry (proxy default 127.0.0.1:6767).
/// Regenerate: `npx tsup --config tsup.desktop.config.ts` in the plugin dir,
/// copy `dist-desktop/entry.opencode.js` here. Replace with the wheel-shipped
/// bundle once upstream PR headroomlabs-ai/headroom#2601 lands in a release.
const OPENCODE_PLUGIN_BYTES: &[u8] = include_bytes!("../resources/opencode/entry.opencode.js");

fn opencode_plugin_install_path() -> PathBuf {
    crate::storage::app_data_dir()
        .join("opencode")
        .join("entry.opencode.js")
}

/// Write (or refresh after an app update) the vendored plugin bundle.
fn ensure_opencode_plugin_file() -> Result<PathBuf> {
    let path = opencode_plugin_install_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if std::fs::read(&path).ok().as_deref() != Some(OPENCODE_PLUGIN_BYTES) {
        atomic_write(&path, OPENCODE_PLUGIN_BYTES)?;
    }
    Ok(path)
}

fn opencode_plugin_array_contains(config: &serde_json::Value, entry: &str) -> bool {
    config
        .get("plugin")
        .and_then(|p| p.as_array())
        .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(entry)))
}

fn add_opencode_plugin_entry(config: &mut serde_json::Value, entry: &str) {
    let obj = config
        .as_object_mut()
        .expect("read_opencode_config guarantees an object root");
    let list = obj
        .entry("plugin".to_string())
        .or_insert_with(|| serde_json::json!([]));
    if !list.is_array() {
        *list = serde_json::json!([]);
    }
    list.as_array_mut()
        .expect("ensured array above")
        .push(serde_json::json!(entry));
}

fn remove_opencode_plugin_entry(config: &mut serde_json::Value, entry: &str) {
    let Some(list) = config.get_mut("plugin").and_then(|p| p.as_array_mut()) else {
        return;
    };
    list.retain(|v| v.as_str() != Some(entry));
    if list.is_empty() {
        config.as_object_mut().map(|o| o.remove("plugin"));
    }
}

fn configure_opencode_provider_block(
    state: &mut ClientSetupState,
) -> Result<(Vec<String>, Vec<String>)> {
    let path = opencode_config_path();
    let mut config = read_opencode_config(&path)?;

    let mut changed = false;

    // `headroom wrap opencode` (bundled CLI) injects its own provider block and
    // repoints the native providers at a wrap-managed proxy, restoring both when
    // it exits. A SIGKILL, a crash, or a reboot leaves that state behind, and the
    // user has no `headroom` on PATH to unwrap it with - so do the unwrap here.
    // The block names the port it hijacked, which is how a wrap-managed base URL
    // is told apart from one the user actually chose (and so must not be
    // preserved as the "original" for restore-on-disable).
    if config.pointer("/provider/headroom").is_some() {
        let wrap_url = config
            .pointer("/provider/headroom/options/baseURL")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(providers) = config.get_mut("provider").and_then(|v| v.as_object_mut()) {
            providers.remove("headroom");
        }
        for provider in OPENCODE_MANAGED_PROVIDERS {
            if wrap_url.is_some() && opencode_provider_base_url(&config, provider) == wrap_url {
                remove_opencode_provider_base_url(&mut config, provider);
            }
        }
        changed = true;
    }
    for provider in OPENCODE_MANAGED_PROVIDERS {
        let existing = opencode_provider_base_url(&config, provider);
        if existing.as_deref() == Some(HEADROOM_OPENCODE_BASE_URL) {
            continue;
        }
        if let Some(original) = existing {
            // Pre-existing custom base URL (gateway, LiteLLM, ...): preserve
            // for restore-on-disable, same contract as codex/claude.
            state
                .preserved_base_urls
                .insert(format!("opencode_{provider}"), original);
        }
        set_opencode_provider_base_url(&mut config, provider, HEADROOM_OPENCODE_BASE_URL);
        changed = true;
    }

    // Transport plugin: routes every other provider (Google, custom
    // gateways, ...) through the proxy via x-headroom-base-url. The bundle
    // defaults to 6767, so no env vars are needed.
    let plugin_path = ensure_opencode_plugin_file()?;
    let plugin_entry = plugin_path.display().to_string();
    if !opencode_plugin_array_contains(&config, &plugin_entry) {
        add_opencode_plugin_entry(&mut config, &plugin_entry);
        changed = true;
    }

    if !changed {
        return Ok((Vec::new(), Vec::new()));
    }

    let backup = backup_if_exists(&path)?;
    write_opencode_config(&path, &config)?;

    let mut backup_files = Vec::new();
    if let Some(backup_path) = backup {
        backup_files.push(backup_path.display().to_string());
    }
    Ok((vec![path.display().to_string()], backup_files))
}

fn opencode_provider_block_matches() -> Result<bool> {
    let path = opencode_config_path();
    if !path.exists() {
        return Ok(false);
    }
    let config = read_opencode_config(&path)?;
    let base_urls_ok = OPENCODE_MANAGED_PROVIDERS.iter().all(|provider| {
        opencode_provider_base_url(&config, provider).as_deref() == Some(HEADROOM_OPENCODE_BASE_URL)
    });
    let plugin_path = opencode_plugin_install_path();
    let plugin_ok = plugin_path.is_file()
        && opencode_plugin_array_contains(&config, &plugin_path.display().to_string());
    Ok(base_urls_ok && plugin_ok)
}

fn disable_opencode(state: &ClientSetupState) -> Result<()> {
    let path = opencode_config_path();
    if !path.exists() {
        return Ok(());
    }
    let mut config = read_opencode_config(&path)?;
    let mut changed = false;
    for provider in OPENCODE_MANAGED_PROVIDERS {
        if opencode_provider_base_url(&config, provider).as_deref()
            != Some(HEADROOM_OPENCODE_BASE_URL)
        {
            // Not ours (user changed it since) - leave it alone.
            continue;
        }
        match state
            .preserved_base_urls
            .get(&format!("opencode_{provider}"))
        {
            Some(original) => set_opencode_provider_base_url(&mut config, provider, original),
            None => remove_opencode_provider_base_url(&mut config, provider),
        }
        changed = true;
    }
    let plugin_entry = opencode_plugin_install_path().display().to_string();
    if opencode_plugin_array_contains(&config, &plugin_entry) {
        remove_opencode_plugin_entry(&mut config, &plugin_entry);
        changed = true;
    }
    if changed {
        let _ = backup_if_exists(&path)?;
        write_opencode_config(&path, &config)?;
    }
    let _ = std::fs::remove_file(opencode_plugin_install_path());
    Ok(())
}

fn detect_opencode_client(configured: bool) -> ClientStatus {
    let executable = opencode_candidate_paths()
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| find_on_path(&["opencode"]));

    let detected = executable
        .as_ref()
        .map(|path| format!("Detected at {}", path.display()))
        .or_else(|| {
            opencode_user_state_exists().then(|| {
                format!(
                    "Detected OpenCode data in {}.",
                    opencode_data_dir().display()
                )
            })
        });

    if let Some(detected_note) = detected {
        return ClientStatus {
            id: "opencode".into(),
            name: "OpenCode".into(),
            installed: true,
            configured,
            health: if configured {
                ClientHealth::Healthy
            } else {
                ClientHealth::Attention
            },
            notes: if configured {
                vec![detected_note, "Configured by Headroom.".into()]
            } else {
                vec![
                    detected_note,
                    "Route OpenCode through Headroom's localhost proxy so prompts stay lean."
                        .into(),
                ]
            },
        };
    }

    ClientStatus {
        id: "opencode".into(),
        name: "OpenCode".into(),
        installed: false,
        configured: false,
        health: ClientHealth::NotDetected,
        notes: vec!["Not detected on this machine yet.".into()],
    }
}

fn opencode_candidate_paths() -> Vec<PathBuf> {
    let home = home_dir();
    let mut candidates = vec![
        home.join(".opencode").join("bin").join("opencode"),
        PathBuf::from("/opt/homebrew/bin/opencode"),
        PathBuf::from("/usr/local/bin/opencode"),
    ];
    let user_bin_dirs = vec![home.join(".local").join("bin"), home.join("bin")];
    candidates.extend(binary_candidates_in_dirs(&user_bin_dirs, &["opencode"]));
    dedupe_paths(candidates)
}

/// Deliberately excludes the config file: setup itself creates one, which
/// would make detection self-fulfilling after disable (the grok_build bug).
fn opencode_user_state_exists() -> bool {
    let data = opencode_data_dir();
    data.join("auth.json").exists() || data.join("storage").exists()
}

/// Rewrite the `command` of the `[mcp_servers.headroom]` table in
/// `~/.grok/config.toml` to the absolute `entrypoint`. Mirrors
/// [`pin_codex_mcp_command`]: the upstream Python registrar writes a bare
/// `command = "headroom"` that relies on PATH, which dangles when the managed
/// runtime relocates.
pub fn pin_grok_mcp_command(entrypoint: &Path) -> Result<Option<String>> {
    pin_toml_mcp_command(&grok_config_toml_path(), entrypoint)
}

/// `(is_end, server_name)` for the wheel registrar's
/// `# --- [end ]Headroom MCP server[: name] ---` marker lines.
fn mcp_span_marker(line: &str) -> Option<(bool, &str)> {
    let (is_end, rest) = match line.strip_prefix("# --- end Headroom MCP server") {
        Some(rest) => (true, rest),
        None => (false, line.strip_prefix("# --- Headroom MCP server")?),
    };
    let rest = rest.strip_suffix(" ---")?;
    let name = if rest.is_empty() {
        "headroom"
    } else {
        rest.strip_prefix(": ")?
    };
    Some((is_end, name))
}

/// Where a table can go above the Headroom block whose start marker is
/// `lines[at]`: moved back past every Headroom MCP span or managed block
/// directly above it, so the comments toml_edit keeps as the table's prefix
/// never hold one of our markers (the wheel appends each span at EOF, and the
/// Codex provider block can sit right above them). Stops at a block that
/// does not open with a table: a table placed above root keys takes them.
fn before_adjacent_headroom_blocks(lines: &[&str], mut at: usize) -> usize {
    loop {
        let mut k = at;
        while k > 0 && lines[k - 1].trim().is_empty() {
            k -= 1;
        }
        let Some(above) = k.checked_sub(1).map(|i| lines[i].trim()) else {
            return at;
        };
        let start = match mcp_span_marker(above) {
            Some((true, "headroom")) => "# --- Headroom MCP server ---".to_string(),
            Some((true, name)) => format!("# --- Headroom MCP server: {name} ---"),
            _ => match above
                .strip_prefix("# <<< headroom:")
                .and_then(|rest| rest.strip_suffix(" <<<"))
            {
                Some(id) => format!("# >>> headroom:{id} >>>"),
                None => return at,
            },
        };
        let Some(s) = lines[..k - 1].iter().rposition(|l| l.trim() == start) else {
            return at;
        };
        let opens_with_table = lines[s + 1..k - 1]
            .iter()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .find(|code| !code.is_empty())
            .is_some_and(|code| code.starts_with('['));
        if !opens_with_table {
            return at;
        }
        at = s;
    }
}

/// Move every table a Headroom MCP span does not own (anything but
/// `[mcp_servers.<span name>(.*)]`) to just before the span's start marker,
/// byte-preserved and in order. The wheel's Codex/Grok registrar deletes
/// everything between its markers on a force re-register, and Codex/ChatGPT's
/// TOML writer appends new tables before the document's trailing comment -- so
/// with our span last, their MCP servers land inside it (rc11 lost the ChatGPT
/// app's browser-use/computer-use `node_repl` this way).
///
/// Not after the end marker: toml_edit (Codex's writer) keeps the comments
/// above a header as that table's prefix, so the end marker would belong to
/// the app's table and go when the app drops it. The wheel then finds a start
/// with no end, cannot unregister, and appends a second
/// `[mcp_servers.headroom]`: the whole config stops parsing. Before the start
/// marker, the start stays the prefix of our own header and the end stays the
/// document trailer.
fn rescue_foreign_toml_from_mcp_spans(content: &str) -> String {
    // Only spans that hold their own table. The wheel finds that table by its
    // parsed key wherever it is, deletes the span and appends a fresh one, so
    // emptying a span whose table lives elsewhere (an inline table under
    // `[mcp_servers]`) would leave the file with two definitions.
    let mut names: BTreeSet<&str> = BTreeSet::new();
    let mut span: Option<&str> = None;
    for line in content.lines() {
        if let Some((is_end, name)) = mcp_span_marker(line.trim()) {
            span = (!is_end).then_some(name);
        } else if let Some(name) = span {
            if mcp_table_name(line).as_deref() == Some(name) {
                names.insert(name);
            }
        }
    }
    let mut out = content.to_string();
    for name in names {
        let (start, end) = if name == "headroom" {
            (
                "# --- Headroom MCP server ---".to_string(),
                "# --- end Headroom MCP server ---".to_string(),
            )
        } else {
            (
                format!("# --- Headroom MCP server: {name} ---"),
                format!("# --- end Headroom MCP server: {name} ---"),
            )
        };
        out = rescue_foreign_toml(
            &out,
            &start,
            &end,
            |header| mcp_table_name(header).as_deref() == Some(name),
            false,
            true,
        );
    }
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Foreign tables that sat inside a Headroom MCP span of `content`, as
/// `(key, has_root, text)`: a server's tables are one family keyed
/// `mcp_servers.<x>`, any other table is keyed by its own path, and `has_root`
/// says the family's own header is among them. `[[array]]` entries are left
/// out: TOML takes another entry, so a duplicate would still parse.
fn mcp_span_foreign_groups(content: &str) -> Vec<(Vec<String>, bool, String)> {
    let mut groups: Vec<(Vec<String>, bool, Vec<&str>)> = Vec::new();
    let mut span: Option<&str> = None;
    let mut current: Option<usize> = None;
    for line in content.lines() {
        if let Some((is_end, name)) = mcp_span_marker(line.trim()) {
            span = (!is_end).then_some(name);
            current = None;
            continue;
        }
        let Some(name) = span else { continue };
        let code = line.split('#').next().unwrap_or("").trim();
        if code.starts_with('[') && code.ends_with(']') {
            current = None;
            if mcp_table_name(line).as_deref() == Some(name) {
                continue;
            }
            let Some(path) = toml_table_header_path(line) else {
                continue;
            };
            let family = if path[0] == "mcp_servers" {
                2
            } else {
                path.len()
            };
            let key = path[..family.min(path.len())].to_vec();
            let i = groups
                .iter()
                .position(|(k, ..)| *k == key)
                .unwrap_or_else(|| {
                    groups.push((key.clone(), false, Vec::new()));
                    groups.len() - 1
                });
            groups[i].1 |= path == key;
            current = Some(i);
        }
        if let Some(i) = current {
            groups[i].2.push(line);
        }
    }
    groups
        .into_iter()
        .map(|(key, has_root, lines)| (key, has_root, lines.join("\n").trim_end().to_string()))
        .collect()
}

/// Byte offset of the first Headroom MCP span start marker that directly
/// precedes a table header, where a table can go without taking over any key,
/// moved back past adjacent Headroom blocks (`before_adjacent_headroom_blocks`).
fn mcp_span_start_offset(text: &str) -> Option<usize> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let at = (0..lines.len()).find(|&i| {
        matches!(mcp_span_marker(lines[i].trim()), Some((false, _)))
            && lines
                .get(i + 1)
                .is_some_and(|next| next.trim_start().starts_with('['))
    })?;
    let at = before_adjacent_headroom_blocks(&lines, at);
    Some(lines[..at].iter().map(|l| l.len()).sum())
}

/// Heal configs an earlier build already damaged: re-add (before the span)
/// each foreign table family a `<file>.headroom-backup-*` held inside a
/// Headroom MCP span, newest backup first, when adding it to `live` still
/// parses -- TOML refuses a table defined twice, so a table the live file has
/// (the app re-added it, maybe with newer values) is never overwritten. A
/// family without its root header is restored only under a root `live` has:
/// a lone `[mcp_servers.x.env]` has no `command`, which Codex rejects.
fn restore_lost_mcp_span_tables(path: &Path, live: &str) -> String {
    let mut healed = live.to_string();
    let (Some(dir), Some(file_name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return healed;
    };
    let prefix = format!("{file_name}.headroom-backup-");
    let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    backups.sort_by(|a, b| b.cmp(a));
    for backup in backups {
        let Ok(text) = std::fs::read_to_string(&backup) else {
            continue;
        };
        for (key, has_root, group) in mcp_span_foreign_groups(&text) {
            let root_is_live = || {
                healed
                    .parse::<toml::Value>()
                    .ok()
                    .is_some_and(|doc| key.iter().try_fold(&doc, |v, k| v.get(k)).is_some())
            };
            if !has_root && !root_is_live() {
                continue;
            }
            // Before our span, not at EOF: the span is last, so EOF is its end
            // marker (see `rescue_foreign_toml_from_mcp_spans`).
            let candidate = match mcp_span_start_offset(&healed) {
                Some(at) => {
                    let head = healed[..at].trim_end();
                    let sep = if head.is_empty() { "" } else { "\n\n" };
                    format!("{head}{sep}{group}\n\n{}", &healed[at..])
                }
                None => format!("{}\n\n{group}\n", healed.trim_end()),
            };
            if candidate.parse::<toml::Value>().is_ok() {
                log::info!(
                    "restored [{}] to {} from {} (lost from inside the Headroom MCP span)",
                    key.join("."),
                    path.display(),
                    backup.display()
                );
                healed = candidate;
            }
        }
    }
    healed
}

/// Keep other apps' tables out of the wheel registrar's reach in `path` (a
/// Codex or Grok `config.toml`): evacuate foreign tables from Headroom MCP
/// spans and, with `heal`, restore any a backup shows were already lost. Only
/// writes a change that leaves the parsed config otherwise identical; a file
/// that does not parse is left alone. Returns whether the file changed.
pub(crate) fn protect_foreign_mcp_tables_in(path: &Path, heal: bool) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(parsed) = existing.parse::<toml::Value>() else {
        return Ok(false);
    };
    let mut evacuated = rescue_foreign_toml_from_mcp_spans(&existing);
    if evacuated.parse::<toml::Value>().ok().as_ref() != Some(&parsed) {
        // Keys after the end marker would change owner (TOML scoping): keep
        // the file as it is rather than alter the user's config.
        log::warn!(
            "{}: moving tables out of the Headroom MCP span would change the config; left in place",
            path.display()
        );
        evacuated = existing.clone();
    }
    let updated = if heal {
        restore_lost_mcp_span_tables(path, &evacuated)
    } else {
        evacuated
    };
    if updated == existing {
        return Ok(false);
    }
    backup_if_exists(path)?;
    atomic_write(path, updated.as_bytes())?;
    Ok(true)
}

/// [`protect_foreign_mcp_tables_in`] for every config the wheel's marker-span
/// registrars (Codex, Grok) write. Run it around every `headroom mcp install`
/// and MCP helper run. Best-effort: logs and carries on.
pub fn protect_foreign_mcp_tables() {
    let _setup = setup_write_lock();
    protect_foreign_mcp_tables_unlocked();
}

/// Whether this run may heal (see [`restore_lost_mcp_span_tables`]). The heal
/// runs once per machine and is recorded before it runs: every backup it reads
/// then predates this build's own writes (the evacuation backs up the file
/// with the tables still inside the span), and a server the user removes or
/// renames afterwards stays gone.
// ponytail: a server the user removed while any retained backup (the newest
// three, possibly weeks old) still held it inside our span comes back that one
// time; it cannot be told apart from one the wheel's force re-register
// deleted (0.9.26 and rc11 both ran `mcp install --force`). Date-gate the
// backups if that ever shows up in a report.
fn claim_mcp_span_heal() -> bool {
    let marker = config_file(&app_data_dir(), "mcp-span-heal-done");
    if marker.exists() {
        return false;
    }
    match atomic_write(&marker, Utc::now().to_rfc3339().as_bytes()) {
        Ok(()) => true,
        Err(err) => {
            log::warn!(
                "not healing lost MCP tables: recording {} failed: {err:#}",
                marker.display()
            );
            false
        }
    }
}

/// [`protect_foreign_mcp_tables`] for callers already holding the setup
/// write lock.
fn protect_foreign_mcp_tables_unlocked() {
    let heal = claim_mcp_span_heal();
    for path in [codex_config_toml_path(), grok_config_toml_path()] {
        if let Err(err) = protect_foreign_mcp_tables_in(&path, heal) {
            log::warn!(
                "protecting foreign MCP tables in {} failed: {err:#}",
                path.display()
            );
        }
    }
}

fn toml_basic_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn codex_provider_block_matches() -> Result<bool> {
    let path = codex_config_toml_path();
    if !path.exists() {
        return Ok(false);
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let base_url = format!("base_url = \"{}\"", HEADROOM_OPENAI_BASE_URL);
    let openai_base = format!("openai_base_url = \"{}\"", HEADROOM_OPENAI_BASE_URL);
    let root_ok = marker_block_contains(
        &content,
        CODEX_ROOT_BLOCK_ID,
        "model_provider = \"headroom\"",
    ) && marker_block_contains(&content, CODEX_ROOT_BLOCK_ID, &openai_base);
    let table_ok = marker_block_contains(&content, CODEX_TABLE_BLOCK_ID, &base_url)
        && marker_block_contains(
            &content,
            CODEX_TABLE_BLOCK_ID,
            "supports_websockets = false",
        );
    // Builds before 0.9.28 wrote the flag only for `auth_mode: chatgpt`, so
    // every other login sends no bearer and 401s with "Missing bearer";
    // failing verify here makes repair add it (see `codex_provider_table_body`).
    let auth_ok = marker_block_contains(
        &content,
        CODEX_TABLE_BLOCK_ID,
        "requires_openai_auth = true",
    );
    Ok(root_ok && table_ok && auth_ok)
}

fn marker_block_contains(content: &str, block_id: &str, needle: &str) -> bool {
    let start = format!("# >>> headroom:{block_id} >>>");
    let end = format!("# <<< headroom:{block_id} <<<");
    // The end marker is searched AFTER the start. Searched from the top, a
    // stray end marker earlier in the file read as "end before start" and the
    // intact block behind it verified as missing on every hourly repair
    // (RUST-BZ: 67 events, 8 hosts; see strip_marker_block for how the stray
    // marker gets there).
    let Some(start_idx) = content.find(&start) else {
        return false;
    };
    match content[start_idx..].find(&end) {
        Some(rel) => content[start_idx..start_idx + rel].contains(needle),
        None => false,
    }
}

fn remove_codex_provider_block() -> Result<()> {
    let path = codex_config_toml_path();
    if !path.exists() {
        return Ok(());
    }
    let existing =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let stripped = strip_codex_managed_toml(&existing);
    let normalized = {
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}\n")
        }
    };
    if normalized == existing {
        return Ok(());
    }
    let _ = backup_if_exists(&path)?;
    atomic_write(&path, normalized.as_bytes())?;
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
    // Only remove the key from the root table: an identical `key = value`
    // line inside some other table ([profiles.x], a user's own server entry)
    // belongs to that table, not to the block we installed.
    let mut in_root_table = true;
    let filtered: Vec<&str> = content
        .lines()
        .filter(|l| {
            let trimmed = l.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_root_table = false;
            }
            !(in_root_table && trimmed == target_line)
        })
        .collect();
    if filtered.len() == content.lines().count() {
        return Ok(());
    }
    let _ = backup_if_exists(&path)?;
    let mut result = filtered.join("\n");
    if !result.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    atomic_write(&path, result.as_bytes())?;
    Ok(())
}

const CODEX_GUARD_STATUS_MESSAGE: &str = "Verifying Headroom route";

fn codex_hooks_json_path() -> PathBuf {
    codex_home().join("hooks.json")
}

fn codex_guard_hook_path() -> PathBuf {
    codex_home().join("hooks").join("headroom-codex-guard.py")
}

/// Interpreter used by the Claude/Codex session-start guard hooks: the system
/// `/usr/bin/python3` when it actually runs, else the managed runtime's own
/// interpreter, which this app installs regardless of what's on PATH. Windows
/// always takes the managed one -- bare `python` on a stock box is either
/// absent from PATH or the Microsoft Store stub that opens the Store instead of
/// running -- as does a Mac without the Command Line Tools (the xcode-select
/// shim) or a Linux distro without /usr/bin/python3.
fn guard_python_command() -> String {
    guard_python_for(!cfg!(target_os = "windows") && system_python_usable())
}

/// Quoted in the fallback: the macOS path has "Application Support".
fn guard_python_for(system_python_ok: bool) -> String {
    if system_python_ok {
        return "/usr/bin/python3".to_string();
    }
    let managed =
        crate::tool_manager::ManagedRuntime::bootstrap_root(&app_data_dir()).managed_python();
    format!("\"{}\"", managed.display())
}

/// Whether `/usr/bin/python3` runs, probed once per process (the guard command
/// is rebuilt on every verify).
fn system_python_usable() -> bool {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USABLE.get_or_init(|| {
        python_usable(
            Path::new("/usr/bin/xcode-select"),
            Path::new("/usr/bin/python3"),
        )
    })
}

/// On macOS `xcode-select -p` goes first: it is the only check that does not
/// pop the "install developer tools" dialog when the Command Line Tools are
/// missing. It is not enough on its own -- a macOS upgrade can leave the
/// tools without xcrun, an Xcode license can be unaccepted -- so the
/// interpreter itself must also run.
fn python_usable(xcode_select: &Path, python: &Path) -> bool {
    let runs = |program: &Path, args: &[&str]| {
        let mut command = crate::proc::command(program);
        command.args(args);
        crate::proc::output_with_timeout(command, Duration::from_secs(10))
            .is_ok_and(|out| out.status.success())
    };
    (!cfg!(target_os = "macos") || runs(xcode_select, &["-p"])) && runs(python, &["-S", "-c", ""])
}

/// Join the guard interpreter and its script into a command string the host
/// shell can actually run. The shell is the client's choice, not ours, and the
/// two clients no longer agree on Windows:
///
/// * Codex runs hook commands through PowerShell (its shell probe list ends
///   `pwsh`/`powershell`, prefixed with `$ErrorActionPreference = 'Stop'`).
///   There a command that *starts* with a quoted path parses as a string
///   literal rather than a command ("At line:1 char:81" -- the offset lands on
///   the unquoted script path), so the call operator is required.
/// * Claude Code runs them through bash (observed on v2.1.259: `/usr/bin/bash:
///   -c: line 1: syntax error near unexpected token`), where a leading `&` is
///   that syntax error. It gets the same string minus the call operator.
///
/// Either way the script path needs quoting, because profile directories
/// contain spaces. Deliberately not `shell_double_quote`: that escapes
/// backslashes POSIX-style and would mangle every Windows path. Both shells
/// leave backslashes alone inside double quotes, and `"` and backtick are
/// invalid in Windows filenames, so bare double quotes are sufficient for both.
fn guard_command(script_path: &Path, powershell: bool) -> String {
    join_guard_command(
        &guard_python_command(),
        &script_path.to_string_lossy(),
        cfg!(target_os = "windows"),
        powershell,
    )
}

/// Pure so the Windows branches are exercised by tests on every platform.
fn join_guard_command(python: &str, script: &str, windows: bool, powershell: bool) -> String {
    match (windows, powershell) {
        (true, true) => format!("& {python} \"{script}\""),
        (true, false) => format!("{python} \"{script}\""),
        (false, _) => format!("{python} {}", shell_word(Path::new(script))),
    }
}

fn codex_guard_command() -> String {
    guard_command(&codex_guard_hook_path(), true)
}

/// Informational guard that Codex runs at session start: it checks that
/// `~/.codex/config.toml` still routes through Headroom and that the desktop app
/// is reachable, and surfaces a notification when either is off so a genuinely
/// broken route is visible. It never blocks (always exits 0): Codex is the
/// user's own OpenAI account and must keep working whether or not Headroom is
/// active -- the intercept forwards Codex direct to OpenAI when the app is down
/// or the gate trips. Runs under system `/usr/bin/python3` (>=3.9), so it
/// carries a tiny TOML fallback parser for the pre-3.11 interpreters that lack
/// `tomllib`. Deliberately does NOT inspect auth mode or `OPENAI_API_KEY`:
/// routing is decided by `base_url`, so an OpenAI-API-key Codex user is a valid
/// Headroom setup, not a failure.
fn build_codex_guard_script() -> String {
    format!(
        r##"#!/usr/bin/env python3
"""Headroom Codex routing guard (managed by Headroom Desktop -- do not edit)."""
import json
import os
import pathlib
import socket
import subprocess
import sys
import time

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

CODEX_HOME = pathlib.Path(os.environ.get("CODEX_HOME") or (pathlib.Path.home() / ".codex"))
CONFIG = CODEX_HOME / "config.toml"
BASE_URL = "{base}"
ADDR = ("127.0.0.1", 6767)
# stderr fires every invocation; the macOS notification is rate-limited so an
# app restart doesn't produce a storm of alerts.
DEBOUNCE_PATH = pathlib.Path(__file__).with_name(".headroom-guard-notified")
DEBOUNCE_SECONDS = 600
# The app cannot see what this script can: it only knows the config files it
# wrote itself, which always verify. Leave the verdict where the app can read
# it, or the real cause never leaves this process. See `read_guard_verdict`.
VERDICT_PATH = pathlib.Path(__file__).with_name(".headroom-guard-verdict.json")


def record_verdict(issues):
    # Written on every run, including the healthy one: "guard ran, route was
    # fine" and "guard never ran" are different facts and the app needs both.
    try:
        payload = json.dumps({{"at": int(time.time()), "issues": issues}})
        tmp = VERDICT_PATH.with_suffix(".tmp")
        tmp.write_text(payload)
        os.replace(str(tmp), str(VERDICT_PATH))
    except Exception:
        pass


def notify(message):
    if sys.platform == "win32":
        return
    try:
        if time.time() - DEBOUNCE_PATH.stat().st_mtime < DEBOUNCE_SECONDS:
            return
    except OSError:
        pass
    try:
        DEBOUNCE_PATH.touch()
        subprocess.run(
            [
                "/usr/bin/osascript",
                "-e",
                'display notification ' + json.dumps(message) + ' with title "Headroom Codex guard"',
            ],
            check=False,
            timeout=5,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        pass


def toml_fallback(text):
    result, current = {{}}, []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            current = [p.strip() for p in line.strip("[]").split(".") if p.strip()]
            continue
        if "=" not in line:
            continue
        key, value = (p.strip() for p in line.split("=", 1))
        if value[:1] == '"' and value[-1:] == '"':
            value = value[1:-1]
        target = result
        for part in current:
            target = target.setdefault(part, {{}})
        target[key] = value
    return result


def load_config():
    # Explicit UTF-8 (TOML's mandated encoding): the default is the locale
    # codec, and on a CP950 Windows box a non-ASCII config raised
    # UnicodeDecodeError, which escaped as "hook exited with code 1".
    try:
        text = CONFIG.read_text(encoding="utf-8")
    except (OSError, ValueError):
        return None
    if tomllib is not None:
        try:
            return tomllib.loads(text)
        except Exception:
            return toml_fallback(text)
    return toml_fallback(text)


def probe():
    # A TCP accept on the intercept port means the desktop app is up. Not an
    # HTTP round trip: /readyz is forwarded to the Python backend, which under
    # heavy multi-agent load can miss a 2s window while perfectly healthy, and
    # that false "down" surfaced as a SessionStart hook error in Claude Code.
    try:
        socket.create_connection(ADDR, timeout=2).close()
        return True
    except OSError:
        return False


def reachable():
    # One retry after a short pause so an app-relaunch blip doesn't read as "down".
    if probe():
        return True
    time.sleep(2)
    return probe()


def main():
    # Clients read hook output as UTF-8; the locale codec (cp950 on a Chinese
    # Windows) mangles non-ASCII and raises on what it cannot encode. Guarded:
    # a Linux /usr/bin/python3 can be 3.6, which has no reconfigure.
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
    issues = []
    config = load_config()
    if config is None:
        issues.append("~/.codex/config.toml is missing or unreadable")
    else:
        provider_name = config.get("model_provider")
        if provider_name != "headroom":
            issues.append('Codex model_provider is "' + str(provider_name) + '" (expected "headroom"); Codex is not being optimized by Headroom')
        else:
            provider = (config.get("model_providers") or {{}}).get("headroom") or {{}}
            base = provider.get("base_url")
            if base != BASE_URL:
                issues.append("Headroom provider base_url is " + str(base) + " (expected " + BASE_URL + ")")
    if not reachable():
        issues.append("Headroom Desktop isn't running; open it to optimize Codex")

    record_verdict(issues)
    # Never block (exit 2): Codex is the user's own OpenAI account and must keep
    # working whether or not Headroom is active. Surface issues as a once-per-
    # session notification so a genuinely broken route is visible, without
    # holding a paused or departing user's Codex hostage to the app being open.
    if issues:
        notify("; ".join(issues))
        sys.stderr.write("Headroom Codex guard:\n")
        for issue in issues:
            sys.stderr.write("- " + issue + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
"##,
        base = HEADROOM_OPENAI_BASE_URL,
    )
}

/// Merge guard entries for the given `events` into a hooks file.
/// Codex `hooks.json` and Claude `settings.json` share the identical
/// `{{"hooks": {{event: [{{matcher?, hooks: [...]}}]}}}}` shape, so both clients
/// use this. Preserves every other key in the file. Idempotent: an
/// already-registered guard command is left untouched.
fn register_guard_hook_entries(
    hooks_path: &Path,
    command: &str,
    status_message: &str,
    events: &[(&str, Option<&str>)],
) -> Result<(Vec<String>, Vec<String>)> {
    let hooks: Vec<_> = events
        .iter()
        .map(|&(event, matcher)| (event, matcher, command, status_message))
        .collect();
    register_hook_entries(hooks_path, &hooks)
}

/// `register_guard_hook_entries` for hooks that differ per event, in one write
/// and one backup. Each is `(event, matcher, command, status_message)`; an
/// empty status message is omitted.
fn register_hook_entries(
    hooks_path: &Path,
    hooks: &[(&str, Option<&str>, &str, &str)],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut content = if held_or_exists(hooks_path) {
        let raw = read_held_or_disk(hooks_path)
            .with_context(|| format!("reading {}", hooks_path.display()))?;
        Value::Object(parse_json_object(&raw, hooks_path)?)
    } else {
        Value::Object(Default::default())
    };

    let root = content
        .as_object_mut()
        .ok_or_else(|| anyhow!("unable to write hooks settings"))?;
    if !root.get("hooks").map(Value::is_object).unwrap_or(false) {
        root.insert("hooks".into(), Value::Object(Default::default()));
    }
    let hooks_obj = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("unable to write hooks settings"))?;

    let mut mutated = false;
    for &(event, matcher, command, status_message) in hooks {
        if !hooks_obj.get(event).map(Value::is_array).unwrap_or(false) {
            hooks_obj.insert(event.to_string(), Value::Array(Vec::new()));
        }
        let entries = hooks_obj
            .get_mut(event)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("unable to write hooks settings"))?;
        if entries
            .iter()
            .any(|entry| entry_contains_hook(entry, command))
        {
            continue;
        }
        let mut handler = serde_json::json!({
            "type": "command",
            "command": command,
            "timeout": 10,
        });
        if !status_message.is_empty() {
            handler["statusMessage"] = status_message.into();
        }
        let mut entry = serde_json::Map::new();
        if let Some(matcher) = matcher {
            entry.insert("matcher".into(), Value::String(matcher.to_string()));
        }
        entry.insert("hooks".into(), Value::Array(vec![handler]));
        entries.push(Value::Object(entry));
        mutated = true;
    }

    if !mutated {
        return Ok((Vec::new(), Vec::new()));
    }

    let backup = backup_if_exists(hooks_path)?;
    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    atomic_write(
        hooks_path,
        &serde_json::to_vec_pretty(&content).context("serializing hooks file")?,
    )?;

    let mut backups = Vec::new();
    if let Some(backup) = backup {
        backups.push(backup.display().to_string());
    }
    Ok((vec![hooks_path.display().to_string()], backups))
}

/// Whether `command` is registered under any event in a hooks file.
fn guard_registered_in_hooks(hooks_path: &Path, command: &str) -> Result<bool> {
    if !held_or_exists(hooks_path) {
        return Ok(false);
    }
    let raw = read_held_or_disk(hooks_path)
        .with_context(|| format!("reading {}", hooks_path.display()))?;
    let content = Value::Object(parse_json_object(&raw, hooks_path)?);
    Ok(content
        .get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks.values().any(|entries| {
                entries
                    .as_array()
                    .map(|arr| arr.iter().any(|entry| entry_contains_hook(entry, command)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false))
}

/// Strip the guard entries for `command` from a hooks file. Leaves any
/// user-authored hooks intact and drops now-empty event arrays. `delete_if_empty`
/// removes the whole file when nothing remains -- correct for Codex's standalone
/// `hooks.json`, but never for Claude's shared `settings.json`. `only_events`
/// limits the sweep to specific event names; `None` sweeps every event.
fn remove_guard_hook_entries(
    hooks_path: &Path,
    command: &str,
    delete_if_empty: bool,
    only_events: Option<&[&str]>,
) -> Result<()> {
    if !held_or_exists(hooks_path) {
        return Ok(());
    }
    let raw = read_held_or_disk(hooks_path)
        .with_context(|| format!("reading {}", hooks_path.display()))?;
    let mut content = Value::Object(parse_json_object(&raw, hooks_path)?);
    let mut changed = false;
    let mut hooks_empty = false;
    if let Some(hooks_obj) = content.get_mut("hooks").and_then(Value::as_object_mut) {
        // Sweep every event, not just the ones we register, so a guard that Codex
        // (or an older build) moved to another event is still stripped.
        let events: Vec<String> = hooks_obj.keys().cloned().collect();
        for event in events {
            if let Some(filter) = only_events {
                if !filter.contains(&event.as_str()) {
                    continue;
                }
            }
            if let Some(entries) = hooks_obj.get_mut(&event).and_then(Value::as_array_mut) {
                changed |= strip_hook_from_groups(entries, &[command]);
            }
        }
        hooks_obj.retain(|_, value| !value.as_array().map(|arr| arr.is_empty()).unwrap_or(false));
        hooks_empty = hooks_obj.is_empty();
    }
    if hooks_empty {
        if let Some(root) = content.as_object_mut() {
            root.remove("hooks");
        }
    }
    if !changed {
        return Ok(());
    }
    let _ = backup_if_exists(hooks_path)?;
    let root_empty = content.as_object().map(|o| o.is_empty()).unwrap_or(false);
    if delete_if_empty && root_empty {
        let _ = std::fs::remove_file(hooks_path);
    } else {
        atomic_write(
            hooks_path,
            &serde_json::to_vec_pretty(&content).context("serializing hooks file")?,
        )?;
    }
    Ok(())
}

/// Write the guard script and register it in `~/.codex/hooks.json` for the
/// SessionStart event only. `hooks.json` is auto-discovered by Codex (no
/// `config.toml` flag needed). The user must trust the hook once via Codex's
/// `/hooks` command before it runs (re-trust after any guard update).
///
/// SessionStart only (mirrors `ensure_claude_guard_hook`): on UserPromptSubmit a
/// nonzero exit blocks every prompt, which held a paused or departing user's own
/// OpenAI-billed Codex hostage to the desktop app being open -- the exact reason
/// users uninstalled. The guard is informational, not a gate; the intercept
/// forwards Codex direct to OpenAI when the app is down or the gate trips.
fn ensure_codex_guard_hook() -> Result<(Vec<String>, Vec<String>)> {
    let script_path = codex_guard_hook_path();
    let (script_changed, script_backup) =
        write_file_if_changed(&script_path, &build_codex_guard_script(), true)?;
    // Migration: earlier builds registered on UserPromptSubmit, where a nonzero
    // exit blocked every Codex prompt. Strip that entry from existing installs;
    // match on the script path so it lands regardless of interpreter drift.
    remove_guard_hook_entries(
        &codex_hooks_json_path(),
        &script_path.display().to_string(),
        false,
        Some(&["UserPromptSubmit"]),
    )?;
    // Same stale-command migration as `ensure_claude_guard_hook`; see there.
    if !codex_guard_registered().unwrap_or(false) {
        remove_guard_hook_entries(
            &codex_hooks_json_path(),
            &script_path.display().to_string(),
            false,
            Some(&["SessionStart"]),
        )?;
    }
    let (mut changed, mut backups) = register_guard_hook_entries(
        &codex_hooks_json_path(),
        &codex_guard_command(),
        CODEX_GUARD_STATUS_MESSAGE,
        &[("SessionStart", Some("startup|resume|clear|compact"))],
    )?;
    if script_changed {
        changed.insert(0, script_path.display().to_string());
    }
    if let Some(backup) = script_backup {
        backups.insert(0, backup.display().to_string());
    }
    Ok((changed, backups))
}

fn codex_guard_registered() -> Result<bool> {
    guard_registered_in_hooks(&codex_hooks_json_path(), &codex_guard_command())
}

fn remove_codex_guard_hook() -> Result<()> {
    let script_path = codex_guard_hook_path();
    // Match on the script path, not the full `/usr/bin/python3 <path>` command,
    // so the registration is stripped even if the interpreter differs -- otherwise
    // deleting the script below leaves a dangling hook that fails with ENOENT.
    remove_guard_hook_entries(
        &codex_hooks_json_path(),
        &script_path.display().to_string(),
        true,
        None,
    )?;
    if script_path.exists() {
        let _ = std::fs::remove_file(&script_path);
    }
    Ok(())
}

const CLAUDE_GUARD_STATUS_MESSAGE: &str = "Verifying Headroom route";

/// What the session-start guard saw from INSIDE the agent's own process, the
/// last time it ran.
///
/// This is the only honest view of the route. The app can verify the files it
/// wrote (`~/.claude/settings.json`, `~/.codex/config.toml`) and they always
/// pass, because it wrote them - which is why `detect_unrouted_clients`
/// re-applies a setup that was never wrong and reports `reapplied=true` while
/// nothing changes (434 of 436 such re-applies on the fleet over 30 days, with
/// hosts recurring across days). The break is elsewhere: a project-local
/// `.claude/settings.json` at a higher precedence, or a session env pointing
/// somewhere else. Only the guard, inheriting the agent's environment, can see
/// those - and until now it wrote its verdict to stderr and dropped it.
pub fn read_guard_verdict(client_id: &str) -> Option<Vec<String>> {
    // Explicit on both sides: a `_ => claude` fallback would hand the next
    // client id added to `detect_unrouted_clients` Claude's verdict under
    // another agent's name, which is worse than no diagnosis at all.
    let path = match client_id {
        "codex" | "codex_cli" => codex_guard_hook_path(),
        "claude_code" | "claude" => claude_guard_hook_path(),
        _ => return None,
    }
    .with_file_name(".headroom-guard-verdict.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // A verdict older than a day describes a session that has since ended; the
    // unrouted check it feeds is itself defined over a 24h window.
    let at = parsed.get("at")?.as_i64()?;
    if chrono::Utc::now().timestamp() - at > 24 * 3600 {
        return None;
    }
    Some(
        parsed
            .get("issues")?
            .as_array()?
            .iter()
            .filter_map(|issue| issue.as_str().map(str::to_owned))
            .collect(),
    )
}

fn claude_guard_hook_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("hooks")
        .join("headroom-claude-guard.py")
}

fn claude_guard_command() -> String {
    guard_command(&claude_guard_hook_path(), false)
}

/// Loud-fail guard that Claude Code runs at session start (SessionStart only:
/// exit 2 there surfaces a warning but cannot block, whereas on UserPromptSubmit
/// it blocks every prompt -- which broke Claude Desktop / Cowork VM sessions
/// that share `~/.claude/settings.json` but can never reach 127.0.0.1:6767).
/// Because the hook inherits Claude's environment, it checks the *effective*
/// routing -- `ANTHROPIC_BASE_URL` as Claude actually sees it -- rather than a
/// config file, plus that the desktop app is reachable. Pure stdlib so it runs
/// on the system `/usr/bin/python3`. Unlike Codex, Claude Code runs app-written
/// `settings.json` hooks without a manual trust step.
fn build_claude_guard_script() -> String {
    format!(
        r##"#!/usr/bin/env python3
"""Headroom Claude routing guard (managed by Headroom Desktop -- do not edit)."""
import json
import os
import pathlib
import socket
import subprocess
import sys
import time

BASE_URL = "{base}"
ADDR = ("127.0.0.1", 6767)
# stderr fires every invocation; the macOS notification is rate-limited so an
# app restart doesn't produce a storm of alerts.
DEBOUNCE_PATH = pathlib.Path(__file__).with_name(".headroom-guard-notified")
DEBOUNCE_SECONDS = 600
# The app cannot see what this script can: it only knows the config files it
# wrote itself, which always verify. Leave the verdict where the app can read
# it, or the real cause never leaves this process. See `read_guard_verdict`.
VERDICT_PATH = pathlib.Path(__file__).with_name(".headroom-guard-verdict.json")


def record_verdict(issues):
    # Written on every run, including the healthy one: "guard ran, route was
    # fine" and "guard never ran" are different facts and the app needs both.
    try:
        payload = json.dumps({{"at": int(time.time()), "issues": issues}})
        tmp = VERDICT_PATH.with_suffix(".tmp")
        tmp.write_text(payload)
        os.replace(str(tmp), str(VERDICT_PATH))
    except Exception:
        pass


def notify(message):
    if sys.platform == "win32":
        return
    try:
        if time.time() - DEBOUNCE_PATH.stat().st_mtime < DEBOUNCE_SECONDS:
            return
    except OSError:
        pass
    try:
        DEBOUNCE_PATH.touch()
        subprocess.run(
            [
                "/usr/bin/osascript",
                "-e",
                'display notification ' + json.dumps(message) + ' with title "Headroom Claude guard"',
            ],
            check=False,
            timeout=5,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        pass


def probe():
    # A TCP accept on the intercept port means the desktop app is up. Not an
    # HTTP round trip: /readyz is forwarded to the Python backend, which under
    # heavy multi-agent load can miss a 2s window while perfectly healthy, and
    # that false "down" surfaced as a SessionStart hook error in Claude Code.
    try:
        socket.create_connection(ADDR, timeout=2).close()
        return True
    except OSError:
        return False


def reachable():
    # One retry after a short pause so an app-relaunch blip doesn't read as "down".
    if probe():
        return True
    time.sleep(2)
    return probe()


def settings_base(path):
    # env.ANTHROPIC_BASE_URL from a Claude settings file, or None if absent/unreadable.
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except Exception:
        return None
    env = data.get("env") if isinstance(data, dict) else None
    if isinstance(env, dict):
        value = env.get("ANTHROPIC_BASE_URL")
        return str(value) if value is not None else None
    return None


def diagnose_route(effective):
    # A real routing break is (a) a higher-precedence project-local scope pointing
    # elsewhere, or (b) neither user settings nor the session env routing to Headroom.
    # settings.json's env is what Claude Code actually applies to its API calls, so
    # a correct user settings + unset process env (GUI / `open` launch that didn't
    # inherit the shell export) is HEALTHY, not a failure -- don't warn on it.
    shown = effective if effective else "unset"
    home = os.path.expanduser("~")
    user_val = settings_base(os.path.join(home, ".claude", "settings.json"))
    cwd = os.getcwd()
    for path in (
        os.path.join(cwd, ".claude", "settings.local.json"),
        os.path.join(cwd, ".claude", "settings.json"),
    ):
        val = settings_base(path)
        if val is not None and val != BASE_URL:
            return "ANTHROPIC_BASE_URL -- " + path + " sets it to " + val + ", which overrides Headroom's route (" + BASE_URL + "). Remove or fix that entry."
    if user_val != BASE_URL and effective != BASE_URL:
        return "ANTHROPIC_BASE_URL is not routed to Headroom (user settings: " + (str(user_val) if user_val else "no entry") + ", session env: " + shown + "). Reopen the Headroom app or re-run client setup."
    return None


def main():
    # Clients read hook output as UTF-8; the locale codec (cp950 on a Chinese
    # Windows) mangles non-ASCII and raises on what it cannot encode. Guarded:
    # a Linux /usr/bin/python3 can be 3.6, which has no reconfigure.
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
    issues = []
    route_issue = diagnose_route(os.environ.get("ANTHROPIC_BASE_URL"))
    if route_issue:
        issues.append(route_issue)
    if not reachable():
        issues.append("Headroom Desktop is not reachable on 127.0.0.1:6767 -- it may be restarting; open the app if it isn't")

    record_verdict(issues)
    if issues:
        notify("; ".join(issues))
        sys.stderr.write("Headroom Claude guard failed:\n")
        for issue in issues:
            sys.stderr.write("- " + issue + "\n")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
"##,
        base = HEADROOM_ANTHROPIC_BASE_URL,
    )
}

/// Write the Claude guard script and register it in `~/.claude/settings.json`
/// for SessionStart only. No trust step required.
///
/// Never UserPromptSubmit: exit 2 there blocks the prompt, and Claude Desktop /
/// Cowork VM sessions read the same settings.json but can never reach
/// 127.0.0.1:6767 from inside the VM, so the guard bricked every prompt in the
/// Claude desktop app while the routing it verifies didn't even apply there.
fn ensure_claude_guard_hook() -> Result<(Vec<String>, Vec<String>)> {
    let script_path = claude_guard_hook_path();
    let (script_changed, script_backup) =
        write_file_if_changed(&script_path, &build_claude_guard_script(), true)?;
    // Migration: earlier builds also registered on UserPromptSubmit; strip that
    // entry from existing installs. Match on the script path (see codex counterpart).
    remove_guard_hook_entries(
        &claude_settings_path(),
        &script_path.display().to_string(),
        false,
        Some(&["UserPromptSubmit"]),
    )?;
    // Migration: the Windows command string changed (PowerShell needs the call
    // operator), and `register_guard_hook_entries` dedupes on the exact command
    // string, so an upgrading install would keep the old unparseable entry
    // alongside the fixed one and keep erroring at every session start. Strip
    // stale forms by script path -- but only when the current command is not
    // already registered, since an unconditional remove-then-re-add would
    // rewrite settings.json on every launch.
    if !claude_guard_registered().unwrap_or(false) {
        remove_guard_hook_entries(
            &claude_settings_path(),
            &script_path.display().to_string(),
            false,
            Some(&["SessionStart"]),
        )?;
    }
    let (mut changed, mut backups) = register_guard_hook_entries(
        &claude_settings_path(),
        &claude_guard_command(),
        CLAUDE_GUARD_STATUS_MESSAGE,
        &[("SessionStart", Some("startup|resume|clear|compact"))],
    )?;
    report_unparseable_guard_command(&claude_guard_command());
    if script_changed {
        changed.insert(0, script_path.display().to_string());
    }
    if let Some(backup) = script_backup {
        backups.insert(0, backup.display().to_string());
    }
    Ok((changed, backups))
}

/// Which shell a client feeds hook commands to is the client's choice and it
/// changes without notice: the PowerShell call operator Codex still needs became
/// a bash syntax error in Claude Code v2.1.259, and every Windows install
/// errored at session start for weeks with the only evidence a screenshot.
/// `bash -n` parses without executing, so this is a free canary that turns the
/// next such switch into a Sentry warning. Windows only, once per process: on
/// macOS and Linux the command is a bare interpreter and path that always
/// parses. The command string itself is not logged -- it carries the user's
/// profile path -- only the shape that decides it.
fn report_unparseable_guard_command(command: &str) {
    use std::sync::Once;

    if !cfg!(target_os = "windows") {
        return;
    }
    static CHECKED: Once = Once::new();
    CHECKED.call_once(|| {
        let bash = windows_bash_command();
        let mut probe = crate::proc::command(bash.trim_matches('"'));
        probe.arg("-n").arg("-c").arg(command);
        // Bounded: a bash that is the WSL launcher boots the user's WSL VM to
        // parse this, and a wedged WSL never returns -- inside this Once that
        // blocked every later Claude Code setup for the life of the process.
        match crate::proc::output_with_timeout(probe, Duration::from_secs(10)) {
            Err(crate::proc::OutputError::TimedOut) => {
                log::info!("claude guard bash canary skipped: bash timed out");
            }
            Err(crate::proc::OutputError::Spawn(_)) => {}
            Ok(out) => {
                let status = out.status;
                // bash reports a syntax error as exit 2. Any other failure is the
                // resolved `bash.exe` not being a bash at all -- the WSL launcher
                // on a box without Git for Windows exits 1 without parsing
                // (RUST-C6, two hosts) -- and says nothing about the command.
                if status.code() == Some(2) {
                    log::warn!(
                    "claude guard command does not parse under bash (exit {:?}, call_operator={}); \
                     SessionStart hooks will fail until the command form is fixed",
                    status.code(),
                    command.starts_with('&')
                );
                } else if !status.success() {
                    log::info!(
                        "claude guard bash canary skipped: bash exited {:?} without parsing",
                        status.code()
                    );
                }
            }
        }
    });
}

fn claude_guard_registered() -> Result<bool> {
    guard_registered_in_hooks(&claude_settings_path(), &claude_guard_command())
}

/// Registered by script path, whatever interpreter runs it.
fn claude_guard_registered_any_interpreter() -> Result<bool> {
    guard_registered_in_hooks(
        &claude_settings_path(),
        &claude_guard_hook_path().display().to_string(),
    )
}

const CLAUDE_GUARD_SCRIPT_MISSING: &str =
    "Headroom routing guard script was missing from ~/.claude/hooks.";
const CLAUDE_GUARD_STALE_COMMAND: &str =
    "Headroom routing guard in ~/.claude/settings.json runs under a different Python; re-applying.";

/// Strip the Claude guard from every settings candidate and delete the script.
/// Never deletes settings.json (it carries other keys), so `delete_if_empty` is
/// false.
fn claude_remote_control_script_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("hooks")
        .join("headroom-remote-control.sh")
}

fn claude_remote_control_command_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("commands")
        .join("remote-control.md")
}

/// Shell function the managed `claude_code` and `codex_cli` blocks both
/// define (identically, so either block works alone): does the intercept
/// answer on 127.0.0.1:`port`? A local connect with no external command and
/// no network: bash's /dev/tcp, zsh's ztcp; a plain sh (dash reading
/// ~/.profile) has neither and says no. On macOS and Linux a closed loopback
/// port refuses at once and an open one completes the handshake in the
/// kernel, so the answer is usually instant. The start-up export runs it
/// once per shell (`intercept_export_line`); `claude` and `codex` run it per
/// call while the shell carries Headroom's URL.
///
/// On Windows (Git Bash reads these blocks too, `$OSTYPE` msys or cygwin)
/// Winsock retries a refused loopback connect, so a probe of a closed 6767
/// took about 2 s: every new terminal after a crash or a failed quit
/// cleanup, and every `claude`/`codex` call in a terminal opened while
/// Headroom ran. There the connect runs in a child bash under coreutils
/// `/usr/bin/timeout 1` (shipped with Git for Windows, MSYS2 and Cygwin):
/// 1 s closed; open took 0.15 s on the rc9 win-test VM, and 1 s leaves room
/// for a loaded or AV-scanned box to start that child. The path is explicit
/// because a PATH with System32 first finds Windows' timeout.exe, which
/// rejects the arguments and would report a live intercept as down. BASH_ENV
/// is cleared so that child never sources an rc carrying this block and
/// probes again. Without it the probe falls back to the plain connect.
///
/// Known residuals:
/// * Outside Windows it has no timeout. While the intercept is wedged with a
///   full accept queue (macOS caps the backlog at kern.ipc.somaxconn, 128 by
///   default) each probe blocks until the connect times out, measured at
///   about 8 s on macOS in bash, zsh and sh.
/// * A terminal opened while Headroom ran keeps the exported URL after quit,
///   and so does every tool started from it: a running process's env cannot
///   be changed from outside. `claude` and `codex` below re-probe per call;
///   anything else there keeps the dead URL until that terminal closes.
/// * It checks that something listens, not who. After a crash, if another
///   program holds 6767 (see `unwire_clients_for_port_holder`), a new shell's
///   probe succeeds and exports the URL at it.
fn intercept_probe_function(port: u16) -> String {
    format!(
        r#"__headroom_up() {{
  case ${{OSTYPE-}} in
    msys*|cygwin*) if [ -x /usr/bin/timeout ]; then
      BASH_ENV= /usr/bin/timeout 1 "${{BASH:-bash}}" -c ': </dev/tcp/127.0.0.1/{port}' 2>/dev/null; return
    fi ;;
  esac
  if [ -n "${{ZSH_VERSION-}}" ]; then
    local REPLY
    zmodload zsh/net/tcp 2>/dev/null && ztcp 127.0.0.1 {port} 2>/dev/null && ztcp -c "$REPLY"
  else
    (: </dev/tcp/127.0.0.1/{port}) 2>/dev/null
  fi
}}"#
    )
}

/// Exports `var` as Headroom's `url` in a shell started while the intercept
/// answers, for everything that reads it (Agent SDK scripts, other tools,
/// `CLAUDE_CONFIG_DIR` setups), unless the user already set their own. Once
/// it is down, a Headroom value inherited from an older shell (a tmux server,
/// an IDE) is dropped instead; the user's own value never is.
///
/// A login shell runs this four times (both blocks, in .zprofile and .zshrc
/// or .bash_profile and .bashrc), so the probe's verdict is kept in
/// `__headroom_live`, keyed by `$$` so a child shell that inherited it (an rc
/// under `set -a`) probes afresh. Re-sourcing an rc keeps the start-up
/// verdict; `claude` and `codex` re-probe per call anyway.
fn intercept_export_line(var: &str, url: &str) -> String {
    format!(
        r#"case ${{__headroom_live-}} in "$$:"[01]) ;; *) if __headroom_up; then __headroom_live=$$:1; else __headroom_live=$$:0; fi ;; esac
if [ "$__headroom_live" = "$$:1" ]; then export {var}="${{{var}:-{url}}}"; elif [ "${{{var}-}}" = {url} ]; then unset {var}; fi"#
    )
}

/// The managed `codex_cli` block: the probed OPENAI_BASE_URL export (Codex
/// itself is routed by config.toml) and a `codex` function that runs without
/// a Headroom OPENAI_BASE_URL the shell still carries once the intercept is
/// gone, so Codex falls back to its own provider instead of the dead port.
/// Defined through `eval` and only when `codex` is not an alias, as
/// `claude_code_shell_block` explains, nor already a function: a user's own
/// `codex` wrapper (profile, sandbox or approval flags) earlier in the rc is
/// kept, and the block's copy in the other profile finds ours already there.
fn codex_shell_block(port: u16) -> String {
    let function = r#"codex() {
  if [ "${OPENAI_BASE_URL-}" = __BASE__ ] && ! __headroom_up; then
    command env -u OPENAI_BASE_URL codex "$@"
  else
    command codex "$@"
  fi
}"#
    .replace("__BASE__", HEADROOM_OPENAI_BASE_URL);
    format!(
        "{}\n{}\nif ! alias codex >/dev/null 2>&1 && ! typeset -f codex >/dev/null 2>&1; then eval '{}'; fi",
        intercept_probe_function(port),
        intercept_export_line("OPENAI_BASE_URL", HEADROOM_OPENAI_BASE_URL),
        function
    )
}

/// The managed `claude_code` shell block: the probed ANTHROPIC_BASE_URL
/// export (`intercept_export_line`; settings.json routes Claude Code itself)
/// and a `claude` function that (a) adds the api.anthropic.com settings
/// layer whenever the user passes `--remote-control`, (b) tags the session with
/// `HEADROOM_RC_RELAUNCHER=tty` so the script only ends sessions this function
/// will bring back (an alias, `command claude` or a shell opened before setup
/// skips it), (c) after the wrapped session exits, resumes the session named
/// in the relaunch marker for this tty, (d) routes a
/// `CLAUDE_CONFIG_DIR=~/.claude-work` session, which reads that dir's
/// settings.json instead of ours, by setting ANTHROPIC_BASE_URL for that one
/// process while ~/.claude/settings.json still routes through Headroom and the
/// user set no base URL of their own; checked per call, so it ends with quit
/// (the export covers this too, but only in a shell started while Headroom
/// ran, and login often restores terminals before the app is up), and (e)
/// runs without a Headroom ANTHROPIC_BASE_URL the shell still carries once
/// the intercept is gone.
/// The marker is written by the /remote-control script
/// (`build_claude_remote_control_script`), keyed by tty so two terminals never
/// swap sessions, and ignored once stale so a terminal without the function
/// (opened before setup) cannot leave a marker that hijacks some later exit.
///
/// The function is defined through `eval`, and only when `claude` is not an
/// alias: zsh and bash alias-expand a function name at parse time, so a bare
/// `claude() {` below a user's `alias claude=...` (Claude Code's own local
/// installer writes one) is a parse error that aborts the rest of the rc file.
///
/// The relaunch carries the user's session flags (model, permission mode, dirs)
/// from an allowlist, because only a known arity tells a flag's value from a
/// prompt, and replaying the prompt would submit it again. An unlisted flag is
/// dropped, as every flag was before.
fn claude_code_shell_block(port: u16) -> String {
    let function = r#"claude() {
  local a; for a in "$@"; do [ "$a" = --remote-control ] && { set -- --settings '__OVERRIDE__' "$@"; break; }; done
  if [ -n "${CLAUDE_CONFIG_DIR-}" ] && [ "${CLAUDE_CONFIG_DIR%/}" != "$HOME/.claude" ] && [ -z "${ANTHROPIC_BASE_URL-}" ] &&
    command grep -qs '"ANTHROPIC_BASE_URL"[[:space:]]*:[[:space:]]*"__BASE__"' "$HOME/.claude/settings.json"; then
    ANTHROPIC_BASE_URL=__BASE__ HEADROOM_RC_RELAUNCHER=tty command claude "$@"
  elif [ "${ANTHROPIC_BASE_URL-}" = __BASE__ ] && ! __headroom_up; then
    command env -u ANTHROPIC_BASE_URL HEADROOM_RC_RELAUNCHER=tty claude "$@"
  else
    HEADROOM_RC_RELAUNCHER=tty command claude "$@"
  fi
  local rc=$?
  local m="$HOME/.headroom/remote-control/$(command basename "$(command tty 2>/dev/null)" 2>/dev/null)"
  if [ -s "$m" ] && [ -n "$(command find "$m" -mmin -2 2>/dev/null)" ]; then
    local sid; sid=$(command cat "$m"); command rm -f "$m"
    local n=$# v=0
    while [ "$n" -gt 0 ]; do
      a=$1; shift; n=$((n-1))
      if [ "$v" = 1 ]; then v=0; set -- "$@" "$a"; continue; fi
      case $a in
        __VALUE_FLAGS__) v=1; set -- "$@" "$a" ;;
        __FLAGS__) set -- "$@" "$a" ;;
      esac
    done
    command claude --settings '__OVERRIDE__' "$@" -r "$sid" --remote-control
    return $?
  fi
  return $rc
}"#
    .replace("__VALUE_FLAGS__", &RELAUNCH_VALUE_FLAGS.join("|"))
    .replace(
        "__FLAGS__",
        &RELAUNCH_VALUE_FLAGS
            .iter()
            .map(|flag| format!("{flag}=*"))
            .chain(RELAUNCH_SWITCHES.iter().map(|flag| flag.to_string()))
            .collect::<Vec<_>>()
            .join("|"),
    )
    .replace("__OVERRIDE__", CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE)
    .replace("__BASE__", HEADROOM_ANTHROPIC_BASE_URL);
    format!(
        "{}\n{}\n\
         # /remote-control needs api.anthropic.com; this relaunches the same session without Headroom.\n\
         if ! alias claude >/dev/null 2>&1; then eval '{}'; fi",
        intercept_probe_function(port),
        intercept_export_line("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL),
        function.replace('\'', r"'\''")
    )
}

/// POSIX sh script run by the /remote-control command (via the model's Bash
/// tool, so it inherits CLAUDE_CODE_SESSION_ID and CLAUDE_PID). Refuses when
/// Remote Control is already reachable, or when the session has no tty (IDE
/// panels: nothing would relaunch it), and otherwise writes the tty-keyed
/// marker and asks the session to exit from a detached child so the command's
/// own output lands first.
fn claude_remote_control_wrapper_path() -> PathBuf {
    // An .exe on Windows: the extension spawns the wrapper with `shell: false`
    // (`build_windows_wrapper_exe`).
    let name = if cfg!(windows) {
        "headroom-claude-wrapper.exe"
    } else {
        "headroom-claude-wrapper.py"
    };
    home_dir().join(".claude").join("hooks").join(name)
}

/// What the Windows panel command runs: the wrapper's own confirm step, since
/// Windows gets no relaunch script. `~` because Git Bash and PowerShell both
/// expand it, and an unquoted home path with a space would split.
const WINDOWS_REMOTE_CONTROL_CONFIRM: &str =
    "~/.claude/hooks/headroom-claude-wrapper.exe --confirm";

/// The VS Code extension's Claude process wrapper setting: an executable it
/// launches the CLI through as `<wrapper> <claude-binary> <args...>`.
const VSCODE_PROCESS_WRAPPER_KEY: &str = "claudeCode.claudeProcessWrapper";

/// A second command name for the VS Code panel. The panel's input box handles
/// the exact strings `/remote-control` and `/rc` itself (its own toggle, which
/// hits the same base-URL gate and fails), so the bare name can never reach a
/// user command there. Any other name goes to the CLI, which prefers user
/// commands. The terminal keeps the bare name.
const CLAUDE_REMOTE_CONTROL_PANEL_COMMAND: &str = "remote-control-headroom";

fn claude_remote_control_panel_command_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("commands")
        .join(format!("{CLAUDE_REMOTE_CONTROL_PANEL_COMMAND}.md"))
}

fn vscode_user_settings_path() -> PathBuf {
    crate::vscode_statusbar::editor_data_root(&home_dir())
        .join("Code")
        .join("User")
        .join("settings.json")
}

/// Process wrapper the VS Code extension launches Claude through. It pipes the
/// panel's stream-json traffic untouched; when the CLI exits and a fresh
/// Remote Control relaunch marker exists for its session, it respawns the CLI
/// on the SAME session with the api.anthropic.com settings layer, replays the
/// handshake the extension sent at startup (swallowing the duplicate answers),
/// and asks the new process to start Remote Control. The extension never sees
/// an exit, so no error card, no manual reopen, no toggle.
fn build_claude_remote_control_wrapper() -> String {
    r#"#!/usr/bin/python3
"""Headroom Claude process wrapper (managed by Headroom Desktop -- do not edit).

The VS Code extension runs `<wrapper> <claude-binary> <args...>` and talks to
the CLI over stdio (stream-json). This wrapper runs the CLI as a child and pipes
both directions untouched. When the child exits and a fresh Remote Control
relaunch marker exists for its session (written by headroom-remote-control.sh
after the user confirmed), the wrapper respawns the CLI on the SAME session with
the api.anthropic.com settings layer, replays the control requests that set
session state (never one-shot actions such as rewind_files, which would undo
work), asks the new child to start Remote Control, and keeps piping. The
extension never sees a process exit. Headroom is off for the swapped session.
The panel shows nothing for the swap, so the answer to that request becomes a
line in the conversation: Remote Control is active, or it did not start.
Once the extension closes stdin or signals the wrapper, nothing is respawned.

Windows has no relaunch script (its ps and kill cannot see a native pid), so
there `--confirm` records the restart and the wrapper ends its own child once
the turn's result is out, by closing the child's stdin: a stream-json CLI takes
that as the end of input and exits cleanly, transcript saved.
"""
import json
import os
import signal
import subprocess
import sys
import threading
import time
import uuid

HOME = os.path.expanduser("~")
DIR = os.path.join(HOME, ".headroom", "remote-control")
OVERRIDE = '{"env":{"ANTHROPIC_BASE_URL":"https://api.anthropic.com"}}'
MARKER_MAX_AGE = 60
# Control requests that carry session state; replayed into a respawned child.
REPLAYED = ("initialize", "mcp_set_servers", "update_settings")
RC_REQUEST = "headroom-remote-control"
ENDS_CHILD = os.name == "nt"
# How long a confirmed restart waits for its turn to end (the Stop hook's 10 min).
PENDING_MAX_AGE = 600


def confirm():
    base = os.environ.get("ANTHROPIC_BASE_URL", "")
    if not base or base.startswith("https://api.anthropic.com"):
        print("Remote Control is already available in this session: type /rc.")
        return
    sid = os.environ.get("CLAUDE_CODE_SESSION_ID", "")
    os.makedirs(DIR, exist_ok=True)
    if not sid or os.environ.get("HEADROOM_RC_RELAUNCHER") != "wrapper":
        # A settings file, not inline JSON: Windows PowerShell 5.1 strips the
        # inner quotes when it passes an argument to a native command.
        settings = os.path.join(DIR, "settings.json")
        with open(settings, "w") as f:
            f.write(OVERRIDE)
        print("Headroom cannot restart this session by itself: it was not started "
              "through Headroom's VS Code wrapper.")
        print('Exit this session, then run: claude --settings "%s" -r %s --remote-control'
              % (settings, sid or "<session-id>"))
        return
    open(os.path.join(DIR, "exit-" + sid), "w").close()
    print("Restarting this session with Remote Control. Headroom is off for the restarted session.")
    print("The restart takes up to 30 seconds; this panel stays open and picks up where "
          "it left off, and says so here once Remote Control is on.")


if sys.argv[1:] == ["--confirm"]:
    confirm()
    sys.exit(0)

if len(sys.argv) < 2:
    sys.exit("usage: headroom-claude-wrapper.py <claude-binary> [args...]")
binary, args = sys.argv[1], sys.argv[2:]

state = {"child": None, "sid": None, "swallow": set(), "recorded": [], "terminating": False}
lock = threading.Lock()
out = sys.stdout.buffer
inp = sys.stdin.buffer


def spawn(extra):
    return subprocess.Popen(
        [binary] + args + extra,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr,
        env=dict(os.environ, HEADROOM_RC_RELAUNCHER="wrapper"),
    )


def replayable(msg):
    if msg.get("type") != "control_request":
        return False
    subtype = (msg.get("request") or {}).get("subtype") or ""
    return subtype in REPLAYED or subtype.startswith("set_")


def write_child(line):
    with lock:
        child = state["child"]
    if child is not None and child.stdin is not None:
        try:
            child.stdin.write(line)
            child.stdin.flush()
        except (BrokenPipeError, OSError, ValueError):
            pass


def restart_after(result):
    # The turn that ran --confirm is over. Its record is consumed either way;
    # an interrupted turn drops the restart, as the Stop hook never runs then.
    pending = os.path.join(DIR, "exit-" + (state["sid"] or ""))
    try:
        fresh = time.time() - os.stat(pending).st_mtime < PENDING_MAX_AGE
        os.remove(pending)
    except OSError:
        return False
    if not fresh or result.get("subtype") != "success":
        return False
    open(os.path.join(DIR, "resume-" + state["sid"]), "w").close()
    return True


def pump_stdin():
    while True:
        line = inp.readline()
        if not line:
            break
        try:
            if replayable(json.loads(line)):
                state["recorded"].append(line)
        except (ValueError, AttributeError):
            pass
        write_child(line)
    state["terminating"] = True
    with lock:
        child = state["child"]
    if child is not None and child.stdin is not None:
        try:
            child.stdin.close()
        except OSError:
            pass


def announce(response):
    # The panel shows nothing for the swap, so say it in the conversation: the
    # same system/informational line the CLI emits itself, which the webview
    # renders as a meta line (display only, not part of the transcript).
    if response.get("subtype") == "success":
        url = (response.get("response") or {}).get("session_url")
        text, level = "Remote Control is now active.", "notice"
        if url:
            text += " Continue here, on your phone, or at " + url
    else:
        text = "Remote Control did not start: " + (response.get("error") or "unknown error")
        text, level = text + ". Run /remote-control-headroom to try again.", "warning"
    line = {"type": "system", "subtype": "informational", "content": text, "level": level,
            "uuid": str(uuid.uuid4()), "session_id": state["sid"]}
    return (json.dumps(line) + "\n").encode()


def pump_child(child):
    while True:
        line = child.stdout.readline()
        if not line:
            return
        end = False
        try:
            parsed = json.loads(line)
            sid = parsed.get("session_id")
            if sid:
                state["sid"] = sid
            if ENDS_CHILD and parsed.get("type") == "result":
                end = restart_after(parsed)
            if parsed.get("type") == "control_response":
                rid = (parsed.get("response") or {}).get("request_id")
                if rid in state["swallow"]:
                    state["swallow"].discard(rid)
                    if rid != RC_REQUEST:
                        continue
                    line = announce(parsed["response"])
        except ValueError:
            pass
        with lock:
            try:
                out.write(line)
                out.flush()
            except (BrokenPipeError, OSError):
                return
        if end:
            try:
                child.stdin.close()
            except (OSError, ValueError):
                pass


def resume_marker():
    sid = state["sid"]
    if not sid:
        return None
    marker = os.path.join(DIR, "resume-" + sid)
    try:
        if time.time() - os.stat(marker).st_mtime < MARKER_MAX_AGE:
            return marker
    except OSError:
        pass
    return None


def forward_signal(signum, _frame):
    state["terminating"] = True
    with lock:
        child = state["child"]
    if child is not None:
        try:
            child.send_signal(signum)
        except OSError:
            pass


# Windows has no SIGHUP.
for name in ("SIGTERM", "SIGINT", "SIGHUP"):
    if hasattr(signal, name):
        signal.signal(getattr(signal, name), forward_signal)

child = spawn([])
state["child"] = child
threading.Thread(target=pump_stdin, daemon=True).start()
while True:
    pump = threading.Thread(target=pump_child, args=(child,), daemon=True)
    pump.start()
    code = child.wait()
    pump.join(timeout=5)
    marker = None if state["terminating"] else resume_marker()
    if marker is None:
        # Daemon threads may still hold the stdio buffers; a normal interpreter
        # shutdown then aborts ("could not acquire lock ... at interpreter
        # shutdown") and on macOS the process can wedge unkillable.
        try:
            out.flush()
        except (BrokenPipeError, OSError):
            pass
        os._exit(code if code is not None else 1)
    os.remove(marker)
    child = spawn(["--resume", state["sid"], "--settings", OVERRIDE])
    with lock:
        state["child"] = child
    for line in list(state["recorded"]):
        try:
            rid = json.loads(line).get("request_id")
            if rid:
                state["swallow"].add(rid)
        except ValueError:
            pass
        write_child(line)
    state["swallow"].add(RC_REQUEST)
    request = {
        "type": "control_request",
        "request_id": RC_REQUEST,
        "request": {"subtype": "remote_control", "enabled": True},
    }
    write_child((json.dumps(request) + "\n").encode())
"#
    .to_string()
}

/// The Windows wrapper. The extension spawns it with `shell: false`, so it has
/// to be an .exe; it is built the way pip builds console scripts: distlib's
/// launcher stub, a shebang naming the interpreter, and a zip holding the
/// wrapper as `__main__.py`. The stub finds the shebang just before the zip
/// (the zip's own offsets locate its start, so they must count from it) and
/// runs `<python> <exe> <args>`, and Python runs the zip.
fn build_windows_wrapper_exe(stub: &[u8], python: &Path, source: &str) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file(
        "__main__.py",
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )?;
    zip.write_all(source.as_bytes())?;
    let archive = zip.finish()?.into_inner();
    let mut exe = stub.to_vec();
    exe.extend_from_slice(format!("#!\"{}\"\n", python.display()).as_bytes());
    exe.extend_from_slice(&archive);
    Ok(exe)
}

/// The interpreter and launcher stub for the Windows wrapper, when both are
/// there. The base interpreter, not the venv's: a running wrapper holds its
/// executable open for the whole panel session, and the venv is the part a
/// runtime repair deletes. The stub ships in the venv's pip.
fn windows_wrapper_parts() -> Option<(PathBuf, PathBuf)> {
    let runtime = crate::tool_manager::ManagedRuntime::bootstrap_root(&app_data_dir());
    let stub = if cfg!(target_arch = "aarch64") {
        "t64-arm.exe"
    } else {
        "t64.exe"
    };
    let stub = runtime
        .venv_dir
        .join("Lib")
        .join("site-packages")
        .join("pip")
        .join("_vendor")
        .join("distlib")
        .join(stub);
    (runtime.standalone_runtime_intact() && stub.is_file())
        .then(|| (runtime.standalone_python(), stub))
}

/// Windows gets the VS Code panel restart only: the terminal one needs the
/// managed zsh/bash `claude` function, and PowerShell gets none.
fn ensure_windows_remote_control_panel() -> Result<(Vec<String>, Vec<String>)> {
    let Some((python, stub)) = windows_wrapper_parts() else {
        // No runtime yet: no wrapper, and no command offering a restart
        // nothing would perform. The next setup after bootstrap adds both.
        remove_vscode_process_wrapper()?;
        return Ok((Vec::new(), Vec::new()));
    };
    let mut changed = Vec::new();
    let mut backups = Vec::new();
    let wrapper = claude_remote_control_wrapper_path();
    let exe = build_windows_wrapper_exe(
        &std::fs::read(&stub).with_context(|| format!("reading {}", stub.display()))?,
        &python,
        &build_claude_remote_control_wrapper(),
    )?;
    if std::fs::read(&wrapper).ok().as_deref() != Some(exe.as_slice()) {
        if let Some(parent) = wrapper.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        match atomic_write(&wrapper, &exe) {
            Ok(()) => changed.push(wrapper.display().to_string()),
            // A panel session running it holds the file; the old wrapper still
            // works, and the next setup replaces it.
            Err(err) => {
                log::info!("keeping the running {}: {err:#}", wrapper.display())
            }
        }
    }
    let command = claude_remote_control_panel_command_path();
    let content = build_claude_remote_control_command_with(
        CLAUDE_REMOTE_CONTROL_PANEL_DESCRIPTION,
        WINDOWS_REMOTE_CONTROL_CONFIRM,
    );
    if std::fs::read_to_string(&command)
        .is_ok_and(|existing| !existing.contains(CLAUDE_REMOTE_CONTROL_COMMAND_MARKER))
    {
        log::info!("keeping the user's own {}", command.display());
    } else {
        let (did_change, backup) = write_file_if_changed(&command, &content, false)?;
        if did_change {
            changed.push(command.display().to_string());
            backups.extend(backup.map(|p| p.display().to_string()));
        }
    }
    if wrapper.is_file() {
        match configure_vscode_process_wrapper() {
            Ok((mut c, mut b)) => {
                changed.append(&mut c);
                backups.append(&mut b);
            }
            Err(err) => log::log!(
                vscode_settings_failure_level(&err),
                "configuring the VS Code process wrapper failed: {err}"
            ),
        }
    }
    Ok((changed, backups))
}

/// Point the VS Code extension at the wrapper. On macOS and Linux only with a
/// working /usr/bin/python3 (the wrapper's interpreter): without the Command
/// Line Tools it is a stub that fails on macOS, a distro may not ship it at
/// all, and every panel session would then fail to start. Windows checks its
/// interpreter before building the .exe (`windows_wrapper_parts`).
fn configure_vscode_process_wrapper() -> Result<(Vec<String>, Vec<String>)> {
    let settings_path = vscode_user_settings_path();
    if !settings_path.exists() {
        // No VS Code user settings at all: nothing launches through us yet, and
        // creating the file would claim a config the user never made.
        return Ok((Vec::new(), Vec::new()));
    }
    if !cfg!(windows) && !system_python_usable() {
        remove_vscode_process_wrapper()?;
        return Ok((Vec::new(), Vec::new()));
    }
    let raw = std::fs::read_to_string(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let obj = parse_json_object(&raw, &settings_path)?;
    // Already ours, or a wrapper the user configured themselves: never replace.
    if obj.contains_key(VSCODE_PROCESS_WRAPPER_KEY) {
        return Ok((Vec::new(), Vec::new()));
    }
    let wrapper = claude_remote_control_wrapper_path().display().to_string();
    let Some(edited) = edit_vscode_wrapper_key(&raw, &wrapper, true, &settings_path) else {
        log::warn!(
            "not setting {VSCODE_PROCESS_WRAPPER_KEY}: {} did not take a clean text edit",
            settings_path.display()
        );
        return Ok((Vec::new(), Vec::new()));
    };
    let backup = backup_if_exists(&settings_path)?;
    atomic_write(&settings_path, edited.as_bytes())?;
    Ok((
        vec![settings_path.display().to_string()],
        backup
            .into_iter()
            .map(|p| p.display().to_string())
            .collect(),
    ))
}

fn remove_vscode_process_wrapper() -> Result<()> {
    let settings_path = vscode_user_settings_path();
    if !settings_path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut obj = parse_json_object(&raw, &settings_path)?;
    let wrapper = claude_remote_control_wrapper_path().display().to_string();
    // A text edit keeps the user's comments and key order; a full rewrite is
    // the fallback, since a setting left pointing at a deleted wrapper would
    // stop the panel from starting at all.
    let edited = match edit_vscode_wrapper_key(&raw, &wrapper, false, &settings_path) {
        Some(edited) => edited.into_bytes(),
        None => {
            if !remove_json_key_if_matches(&mut obj, VSCODE_PROCESS_WRAPPER_KEY, &wrapper) {
                return Ok(());
            }
            serde_json::to_vec_pretty(&Value::Object(obj))
                .context("serializing VS Code settings after removing the process wrapper")?
        }
    };
    backup_if_exists(&settings_path)?;
    atomic_write(&settings_path, &edited)
}

/// Delete the wrapper script once VS Code's settings no longer name it. An
/// unreadable settings file counts as naming it: a panel pointed at a missing
/// wrapper does not start at all.
fn remove_vscode_wrapper_file_if_unreferenced() {
    let wrapper = claude_remote_control_wrapper_path();
    if !wrapper.exists() {
        return;
    }
    let settings = vscode_user_settings_path();
    // As settings.json spells it: JSON-escaped, so a Windows path's
    // backslashes are doubled.
    let spelled = Value::String(wrapper.display().to_string()).to_string();
    let referenced = match std::fs::read_to_string(&settings) {
        Ok(raw) => raw.contains(&spelled[1..spelled.len() - 1]),
        Err(err) => err.kind() != std::io::ErrorKind::NotFound,
    };
    if referenced {
        return;
    }
    if let Err(err) = std::fs::remove_file(&wrapper) {
        log::warn!("cleanup: removing {} failed: {err}", wrapper.display());
    }
}

/// Add (or remove) our wrapper key in VS Code's settings.json as a text edit.
/// The file is hand-maintained JSONC: a serde round trip would sort every key
/// and strip every comment. The result is re-parsed and must equal the parsed
/// original plus (minus) exactly that key, or None is returned.
fn edit_vscode_wrapper_key(raw: &str, wrapper: &str, add: bool, path: &Path) -> Option<String> {
    let mut expected = parse_json_object(raw, path).ok()?;
    let value = Value::String(wrapper.to_string());
    let key = format!("\"{VSCODE_PROCESS_WRAPPER_KEY}\"");
    let edited = if add {
        let comma = if expected.is_empty() { "\n" } else { "," };
        expected.insert(VSCODE_PROCESS_WRAPPER_KEY.to_string(), value.clone());
        let open = raw.find('{')? + 1;
        format!(
            "{}\n    {key}: {value}{comma}{}",
            &raw[..open],
            &raw[open..]
        )
    } else {
        if expected.remove(VSCODE_PROCESS_WRAPPER_KEY)? != value {
            return None;
        }
        let at = raw.find(&key)?;
        let rest = raw[at + key.len()..]
            .trim_start()
            .strip_prefix(':')?
            .trim_start()
            .strip_prefix(value.to_string().as_str())?;
        let before = raw[..at].trim_end();
        match rest.trim_start().strip_prefix(',') {
            Some(after) => format!("{before}{after}"),
            None => format!("{}{rest}", before.strip_suffix(',').unwrap_or(before)),
        }
    };
    (parse_json_object(&edited, path).ok()? == expected).then_some(edited)
}

fn build_claude_remote_control_script() -> String {
    r#"#!/bin/sh
# Headroom Remote Control relaunch (managed by Headroom Desktop -- do not edit).
# Claude Code hides /remote-control whenever ANTHROPIC_BASE_URL is not
# api.anthropic.com, so a Headroom-routed session can never turn it on. This
# asks the session to exit; whatever launched it (the `claude` shell function
# Headroom manages, or Headroom's VS Code process wrapper, named by
# HEADROOM_RC_RELAUNCHER) then resumes the SAME session by id with the base URL
# overridden for that one process. A session nothing will relaunch is never
# ended: it gets the manual command instead.
# Three phases. Without arguments (run by the model's Bash tool after the user
# confirms): record the pending exit. With --stop (Claude Code's Stop hook,
# fired once the model's turn has ended): write the relaunch marker and perform
# it. Killing only after the turn ends keeps the transcript clean, so the
# resumed session shows no "interrupted" turn in the terminal or on the phone.
# With --cancel (UserPromptSubmit): drop it. Esc skips the Stop hook, so
# without this a restart the user interrupted would fire at the end of their
# NEXT turn. The markers are only written by --stop, so a cancelled restart
# leaves nothing that could relaunch a later exit. Must print nothing:
# UserPromptSubmit stdout is added to the prompt.
dir="$HOME/.headroom/remote-control"
case "${1:-}" in --stop|--cancel)
  sid=$(sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  exit_file="$dir/exit-$sid"
  [ -n "$sid" ] && [ -f "$exit_file" ] || exit 0
  # "<pid> <relauncher>": a tty name, or "wrapper" for the VS Code panel.
  read -r pid via < "$exit_file"
  # A record the Stop hook never saw (terminal closed mid-turn) must not end a
  # resumed session days later, when the pid may be some other claude.
  fresh=$(find "$exit_file" -mmin -10 2>/dev/null)
  rm -f "$exit_file"
  [ "$1" = "--stop" ] && [ -n "$fresh" ] || exit 0
  # Never signal a pid without checking it is still the Claude Code process.
  case "$(basename "$(ps -o comm= -p "$pid" 2>/dev/null)")" in
    claude|claude.exe) ;;
    *) exit 0;;
  esac
  case "$via" in
    wrapper) : > "$dir/resume-$sid";;
    ?*) printf '%s\n' "$sid" > "$dir/$via";;
  esac
  kill -TERM "$pid"
  exit 0;;
esac
case "${ANTHROPIC_BASE_URL:-}" in
  ""|https://api.anthropic.com*)
    echo "Remote Control is already available in this session: type /rc."
    exit 0;;
esac
override='__OVERRIDE__'
if [ -z "${CLAUDE_CODE_SESSION_ID:-}" ] || [ -z "${CLAUDE_PID:-}" ]; then
  echo "Could not identify this session. Exit it, then run: claude --settings '$override' -r <session-id> --remote-control"
  exit 0
fi
fallback="claude --settings '$override' -r $CLAUDE_CODE_SESSION_ID --remote-control"
via=
case "${HEADROOM_RC_RELAUNCHER:-}" in
  wrapper) via=wrapper;;
  tty)
    tty_name=${HEADROOM_REMOTE_CONTROL_TTY:-$(ps -o tty= -p "$CLAUDE_PID" 2>/dev/null | tr -d ' ')}
    # Linux ps says "pts/3" where the shell function's `basename $(tty)` says "3".
    tty_name=${tty_name##*/}
    case "$tty_name" in ""|"??"|"-"|"?") ;; *) via=$tty_name;; esac;;
esac
if [ -z "$via" ]; then
  echo "Headroom cannot restart this session by itself: it was not started through Headroom's claude launcher (an alias, a shell opened before Headroom was set up, or an editor panel without Headroom's wrapper)."
  echo "Exit this session, then run: $fallback"
  exit 0
fi
mkdir -p "$dir" && printf '%s %s\n' "$CLAUDE_PID" "$via" > "$dir/exit-$CLAUDE_CODE_SESSION_ID"
echo "Restarting this session with Remote Control. Headroom is off for the restarted session."
# Only when the Stop hook will NOT run: a timed exit, at the cost of an
# interrupted turn. With the hook live there is no timer at all. A timer that
# raced a slow turn killed the session mid-turn and the CLI lost every
# transcript entry after the confirmation, which is what "No response
# requested." on the phone was. The hook is live when it is registered and no
# settings layer switches hooks off: disableAllHooks anywhere, or a managed
# allowManagedHooksOnly, which ignores user hooks.
settings="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json"
managed="/Library/Application Support/ClaudeCode/managed-settings.json"
[ -f "$managed" ] || managed=/etc/claude-code/managed-settings.json
if ! grep -q 'headroom-remote-control\.sh[^"]* --stop' "$settings" 2>/dev/null \
  || grep -qs '"disableAllHooks"[[:space:]]*:[[:space:]]*true' "$settings" .claude/settings.json .claude/settings.local.json "$managed" \
  || grep -qs '"allowManagedHooksOnly"[[:space:]]*:[[:space:]]*true' "$managed"; then
  secs=${HEADROOM_REMOTE_CONTROL_FALLBACK_SECS:-15}
  # Runs the --stop phase itself, so the marker and the pid check are shared.
  nohup sh -c 'sleep "$1"; printf "{\"session_id\":\"%s\"}\n" "$2" | sh "$0" --stop' "$0" "$secs" "$CLAUDE_CODE_SESSION_ID" >/dev/null 2>&1 &
fi
if [ "$via" = wrapper ]; then
  echo "The restart takes up to 30 seconds; this panel stays open and picks up where it left off, and says so here once Remote Control is on."
  exit 0
fi
echo "The restart takes up to 30 seconds. If it does not come back by itself, run: $fallback"
"#
    .replace("__OVERRIDE__", CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE)
}

/// User-level command that shadows Claude Code's hidden built-in. Every user
/// command costs one model turn (its body is the prompt), so that turn is the
/// confirmation dialog. `disable-model-invocation` matters: a shell-only
/// "type it again to confirm" was defeated in testing by the model invoking
/// the skill itself. Only the script and the question tool are allowed.
fn build_claude_remote_control_command() -> String {
    build_claude_remote_control_command_with(
        "Restart this session with Remote Control (Headroom off for that session)",
        &claude_remote_control_script_path().display().to_string(),
    )
}

const CLAUDE_REMOTE_CONTROL_PANEL_DESCRIPTION: &str =
    "Restart this session with Remote Control from the VS Code panel (Headroom off for that session)";

fn build_claude_remote_control_panel_command() -> String {
    build_claude_remote_control_command_with(
        CLAUDE_REMOTE_CONTROL_PANEL_DESCRIPTION,
        &claude_remote_control_script_path().display().to_string(),
    )
}

/// `script` is the command the confirmed restart runs with the Bash tool.
fn build_claude_remote_control_command_with(description: &str, script: &str) -> String {
    format!(
        "---\n\
description: {description}\n\
allowed-tools: Bash({script}:*), AskUserQuestion, ToolSearch\n\
disable-model-invocation: true\n\
---\n\
{CLAUDE_REMOTE_CONTROL_COMMAND_MARKER}\n\
First write exactly this one line of plain text and nothing else before it: \
\"Remote Control is unavailable while this session runs through Headroom: Claude Code switches it off for any custom endpoint.\" \
Then call the AskUserQuestion tool (a tool call, never prose). If AskUserQuestion is not loaded yet, \
load it first with ToolSearch using the query \"select:AskUserQuestion\"; never fall back to asking in prose. \
Call it with header \"Remote Control\" and exactly one question: \
\"Headroom is incompatible with Remote Control due to design decisions by Anthropic. \
Headroom can restart this session with itself disabled so Remote Control does work. How do you want to proceed?\" \
Offer exactly two options, in this order: \
\"Restart with Remote Control\" (description: \"Exit and restart this session without Headroom. The restart replays the conversation uncached.\") and \
\"Do nothing and keep this session routed through Headroom\" (description: \"Remote Control stays unavailable here.\"). \
If the answer is \"Restart with Remote Control\", run `{script}` with the Bash tool, then reply with exactly one line: \"Restarting with Remote Control. This takes up to 30 seconds.\" \
Otherwise reply only \"Staying in this session.\"\n"
    )
}

/// Install the /remote-control relaunch command and its script. The terminal
/// relaunch needs the managed zsh/bash function, which Windows does not get,
/// so Windows gets the VS Code panel's restart alone.
fn ensure_claude_remote_control_command() -> Result<(Vec<String>, Vec<String>)> {
    if cfg!(target_os = "windows") {
        return ensure_windows_remote_control_panel();
    }
    let mut changed = Vec::new();
    let mut backups = Vec::new();
    for (path, content, executable) in [
        (
            claude_remote_control_script_path(),
            build_claude_remote_control_script(),
            true,
        ),
        (
            claude_remote_control_command_path(),
            build_claude_remote_control_command(),
            false,
        ),
        (
            claude_remote_control_panel_command_path(),
            build_claude_remote_control_panel_command(),
            false,
        ),
        (
            claude_remote_control_wrapper_path(),
            build_claude_remote_control_wrapper(),
            true,
        ),
    ] {
        // Our commands carry the marker; a same-name file without it is the
        // user's own and stays, as remove_claude_remote_control_command leaves it.
        if content.contains(CLAUDE_REMOTE_CONTROL_COMMAND_MARKER)
            && std::fs::read_to_string(&path)
                .is_ok_and(|existing| !existing.contains(CLAUDE_REMOTE_CONTROL_COMMAND_MARKER))
        {
            log::info!("keeping the user's own {}", path.display());
            continue;
        }
        let (did_change, backup) = write_file_if_changed(&path, &content, executable)?;
        if did_change {
            changed.push(path.display().to_string());
            if let Some(backup) = backup {
                backups.push(backup.display().to_string());
            }
        }
    }
    // Convenience for the VS Code panel; never a setup blocker.
    match configure_vscode_process_wrapper() {
        Ok((mut c, mut b)) => {
            changed.append(&mut c);
            backups.append(&mut b);
        }
        Err(err) => log::log!(
            vscode_settings_failure_level(&err),
            "configuring the VS Code process wrapper failed: {err}"
        ),
    }
    // The Stop hook performs the exit the script recorded, once the turn ends;
    // the next prompt drops one an interrupted turn left behind. No status
    // message on the prompt hook: it runs on every prompt and almost never acts.
    let (stop, cancel) = (
        claude_remote_control_hook_command("--stop"),
        claude_remote_control_hook_command("--cancel"),
    );
    let (mut hook_changed, mut hook_backups) = register_hook_entries(
        &claude_settings_path(),
        &[
            (
                "Stop",
                None,
                &stop,
                "Headroom: checking for a pending Remote Control restart",
            ),
            ("UserPromptSubmit", None, &cancel, ""),
        ],
    )?;
    changed.append(&mut hook_changed);
    backups.append(&mut hook_backups);
    Ok((changed, backups))
}

fn claude_remote_control_hook_command(phase: &str) -> String {
    format!(
        "{} {phase}",
        shell_double_quote(&claude_remote_control_script_path().to_string_lossy())
    )
}

/// Remove the script, and the command file only when it is ours: a user's own
/// ~/.claude/commands/remote-control.md is never touched.
fn remove_claude_remote_control_command() -> Result<()> {
    let script = claude_remote_control_script_path();
    // Match on the script path so any earlier command form is stripped too.
    let fragment = script.display().to_string();
    // A hook left registered against a deleted script errors on every prompt
    // and every turn end, so the script stays until its hooks are gone.
    let mut hooks_removed = true;
    for settings_path in claude_settings_candidates() {
        if let Err(err) = remove_guard_hook_entries(
            &settings_path,
            &fragment,
            false,
            Some(&["Stop", "UserPromptSubmit"]),
        ) {
            log::warn!("removing the Remote Control hooks failed: {err}");
            hooks_removed = false;
        }
    }
    if hooks_removed && script.exists() {
        remove_owned_script(&script);
    }
    for command in [
        claude_remote_control_command_path(),
        claude_remote_control_panel_command_path(),
    ] {
        if let Ok(content) = std::fs::read_to_string(&command) {
            if content.contains(CLAUDE_REMOTE_CONTROL_COMMAND_MARKER) {
                remove_owned_script(&command);
            }
        }
    }
    // The wrapper file is never deleted, only the setting. VS Code picks up a
    // settings.json edit seconds later (8s observed), and a panel spawn in that
    // window still launches the old path: deleting the file failed it with
    // "native binary not found". A leftover wrapper is an inert passthrough.
    if let Err(err) = remove_vscode_process_wrapper() {
        log::log!(
            vscode_settings_failure_level(&err),
            "removing the VS Code process wrapper setting failed: {err}"
        );
    }
    Ok(())
}

/// File name of the statusline script; also how our `statusLine` entry is
/// recognised in settings.json.
const CLAUDE_STATUSLINE_SCRIPT: &str = "headroom-statusline.sh";

pub(crate) fn claude_statusline_script_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("hooks")
        .join(CLAUDE_STATUSLINE_SCRIPT)
}

/// Prints this conversation's Headroom input savings from the file the
/// intercept keeps (claude_statusline.rs), looked up by the `session_id`
/// Claude Code passes on stdin: "Headroom saved 31k tokens this session", bold
/// green with the new saving appended for a few seconds after one lands, and
/// "Headroom compressing..." for a moment after each request goes out. The
/// real compression is ~100 ms (p50); the moment is stretched to a couple of
/// seconds so a 1 s render cycle cannot miss it. A saving outranks it, so a
/// follow-up request never cuts a saving's highlight short. After it, the plan
/// usage Claude Code passes in `rate_limits` (Pro and Max, once the session has
/// had a response): "| usage: 5h 34%, week 62%", a window at 80% or more in yellow
/// with its reset time, a window past its reset at 0. Silent until there is
/// either, and on any error.
///
/// Percentages are cut to whole numbers as strings: bash's float printf reads
/// "23.5" as invalid under a comma-decimal locale.
///
/// Plain bash, parsing with regexes, because it runs every second
/// (`refreshInterval`): ~4 ms per render against ~30 ms for a Python start.
/// No subshells or external commands on the common path: under Git Bash each
/// is an MSYS fork, and the six per render this used to cost took +8.3% of a
/// 2-vCPU Windows VM per Claude Code session, +3.5% without them (win-test,
/// 2026-10-01). Stays bash 3.2
/// compatible (macOS /bin/bash): EPOCHSECONDS (bash 5) falls back to `date`,
/// no EPOCHREALTIME, no printf %T.
fn build_claude_statusline_script(state_path: &Path) -> String {
    let state = shell_double_quote(&state_path.to_string_lossy());
    let warn = crate::TRAY_USAGE_RESET_SHOWN_AT_PERCENT as u32;
    format!(
        r#"#!/bin/bash
# Headroom statusline (managed by Headroom Desktop - do not edit).
state_file="{state}"
flash_secs=4
compress_secs=2
IFS= read -r -d '' input
now_s=${{EPOCHSECONDS:-$(date +%s)}}
now_ms=$(( now_s * 1000 ))
usage=
win() {{
  local pct at r f
  [[ $input =~ \"$1\"[[:space:]]*:[[:space:]]*\{{[^}}]*\"used_percentage\"[[:space:]]*:[[:space:]]*([0-9]*)(\.[0-9]*)? ]] || return 0
  pct=${{BASH_REMATCH[1]:-0}}
  [[ $input =~ \"$1\"[[:space:]]*:[[:space:]]*\{{[^}}]*\"resets_at\"[[:space:]]*:[[:space:]]*([0-9]+) ]] && at=${{BASH_REMATCH[1]}}
  if [ -n "$at" ] && [ "$now_s" -ge "$at" ]; then pct=0; fi
  if [ "$pct" -gt 100 ]; then pct=100; fi
  r="$2 $pct%"
  if [ "$pct" -ge {warn} ] && [ -n "$at" ]; then
    if [ $(( at - now_s )) -lt 86400 ]; then f=+%H:%M; else f=+%a; fi
    r=$'\033[33m'"$r (resets $(date -d "@$at" "$f" 2>/dev/null || date -r "$at" "$f" 2>/dev/null))"$'\033[0m'
  fi
  usage="${{usage:+$usage, }}$r"
}}
win five_hour 5h
win seven_day week
fmt() {{
  local n=$1 d u t
  if [ "$n" -ge 999500 ]; then d=1000000 u=M
  elif [ "$n" -ge 1000 ]; then d=1000 u=k
  else fmt_out=$n; return; fi
  t=$(( (n * 10 + d / 2) / d ))
  if [ "$t" -ge 100 ]; then fmt_out="$(( (n + d / 2) / d ))$u"
  elif [ $(( t % 10 )) -eq 0 ]; then fmt_out="$(( t / 10 ))$u"
  else fmt_out="$(( t / 10 )).$(( t % 10 ))$u"; fi
}}
saved=
if [[ $input =~ \"session_id\"[[:space:]]*:[[:space:]]*\"([A-Za-z0-9-]+)\" ]] && [ -r "$state_file" ]; then
  sid=${{BASH_REMATCH[1]}}
  IFS= read -r -d '' state < "$state_file"
  if [[ $state =~ \"$sid\":\{{\"tokensSaved\":([0-9]+),\"lastSaved\":([0-9]+),\"lastSavedAtMs\":([0-9]+)(,\"lastRequestAtMs\":([0-9]+))?\}} ]]; then
    total=${{BASH_REMATCH[1]}} last=${{BASH_REMATCH[2]}} last_at=${{BASH_REMATCH[3]}} req_at=${{BASH_REMATCH[5]:-0}}
    fmt "$total"
    line="Headroom saved $fmt_out tokens this session"
    if [ "$last" -gt 0 ] && [ $(( now_ms - last_at )) -lt $(( flash_secs * 1000 )) ]; then
      fmt "$last"
      saved=$'\033[1;32m'"$line (+$fmt_out)"$'\033[0m'
    elif [ $(( now_ms - req_at )) -lt $(( compress_secs * 1000 )) ]; then
      saved=$'\033[32mHeadroom compressing...\033[0m'
    elif [ "$total" -gt 0 ]; then
      saved=$line
    fi
  fi
fi
if [ -n "$usage" ]; then usage="usage: $usage"; fi
if [ -n "$saved" ] && [ -n "$usage" ]; then
  printf '%s | %s\n' "$saved" "$usage"
elif [ -n "$saved$usage" ]; then
  printf '%s\n' "$saved$usage"
fi
"#
    )
}

/// Session flags the /remote-control relaunch keeps: each takes exactly one
/// value (`--add-dir a b` keeps only `a`).
const RELAUNCH_VALUE_FLAGS: &[&str] = &[
    "--model",
    "--permission-mode",
    "--add-dir",
    "--agent",
    "--effort",
    "--fallback-model",
    "--append-system-prompt",
    "--mcp-config",
    "--plugin-dir",
];
/// Valueless session flags the relaunch keeps.
const RELAUNCH_SWITCHES: &[&str] = &[
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--strict-mcp-config",
    "--verbose",
    "--ide",
    "--chrome",
    "--no-chrome",
];

/// Ours only when the command is our script alone, however its path is
/// quoted: a user's composed command that merely also runs ours is theirs, and
/// must be neither replaced on setup nor deleted on removal.
fn is_our_statusline(value: &Value) -> bool {
    value
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| {
            is_our_statusline_command(command)
                || without_bash_program(command).is_some_and(is_our_statusline_command)
        })
}

/// The script path after a leading bash program, as Windows writes the
/// command: `"C:\Program Files\Git\bin\bash.exe" "<script>"`, or bare `bash`.
fn without_bash_program(command: &str) -> Option<&str> {
    let command = command.trim();
    let (program, rest) = match command.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"')?,
        None => command.split_once(char::is_whitespace)?,
    };
    let name = program.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    matches!(name.as_str(), "bash" | "bash.exe").then_some(rest)
}

fn is_our_statusline_command(command: &str) -> bool {
    let command = command.trim();
    match command.strip_prefix('"') {
        Some(rest) => rest.strip_suffix('"'),
        // Unquoted, whitespace separates commands: `~/mine.sh; <ours>`.
        None => (!command.contains(char::is_whitespace)).then_some(command),
    }
    .is_some_and(|path| {
        // Both separators: a Windows path read anywhere still ends in ours.
        !path.contains(['"', ';', '&', '|', '`', '\n'])
            && path.rsplit(['/', '\\']).next() == Some(CLAUDE_STATUSLINE_SCRIPT)
    })
}

/// `statusLine` is a single slot in ~/.claude/settings.json. Ours goes in only
/// when the slot is empty or already ours: a user's own statusline is never
/// replaced, and removal only ever deletes our entry. Returns whether the file
/// changed.
fn set_claude_statusline_setting(command: Option<&str>) -> Result<bool> {
    let settings_path = claude_settings_path();
    // Only a missing file reads as empty: an unreadable one (permissions,
    // non-UTF-8) would otherwise be replaced by a file holding just statusLine.
    let raw = match read_held_or_disk(&settings_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        other => other.with_context(|| format!("reading {}", settings_path.display()))?,
    };
    let mut root = if raw.trim().is_empty() {
        if command.is_none() {
            return Ok(false);
        }
        serde_json::Map::new()
    } else {
        parse_json_object(&raw, &settings_path)?
    };
    let current = root.get("statusLine");
    match command {
        Some(command) => {
            if current.is_some_and(|v| !is_our_statusline(v)) {
                return Ok(false);
            }
            // refreshInterval: the highlight has to switch off on time, and a
            // saving can land between Claude Code's own event-driven renders.
            let desired =
                serde_json::json!({ "type": "command", "command": command, "refreshInterval": 1 });
            if current == Some(&desired) {
                return Ok(false);
            }
            root.insert("statusLine".into(), desired);
        }
        None => {
            if !current.is_some_and(is_our_statusline) {
                return Ok(false);
            }
            root.remove("statusLine");
        }
    }
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let _ = backup_if_exists(&settings_path)?;
    atomic_write(
        &settings_path,
        &serde_json::to_vec_pretty(&Value::Object(root))
            .context("serializing Claude statusline settings")?,
    )
    .with_context(|| format!("writing {}", settings_path.display()))?;
    Ok(true)
}

/// Install the savings statusline. On Windows Claude Code runs statusline
/// commands through Git Bash, as it does the hooks, so the command names
/// bash.exe the way theirs does (`hook_shell_command`). A no-op when the user
/// turned it off or already has a statusline of their own.
fn ensure_claude_statusline() -> Result<(Vec<String>, Vec<String>)> {
    if is_statusline_disabled() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut changed = Vec::new();
    let mut backups = Vec::new();
    let script = claude_statusline_script_path();
    let (did_change, backup) = write_file_if_changed(
        &script,
        &build_claude_statusline_script(&crate::claude_statusline::state_path()),
        true,
    )?;
    if did_change {
        changed.push(script.display().to_string());
    }
    if let Some(backup) = backup {
        backups.push(backup.display().to_string());
    }
    let command = if cfg!(windows) {
        hook_shell_command(&script)?
    } else {
        format!("\"{}\"", shell_double_quote(&script.to_string_lossy()))
    };
    if set_claude_statusline_setting(Some(&command))? {
        changed.push(claude_settings_path().display().to_string());
    }
    // The Claude Code panel in VS Code/Cursor shows no statusLine; its status
    // bar gets the same numbers from a small extension (background thread).
    crate::vscode_statusbar::ensure_installed();
    Ok((changed, backups))
}

/// Remove a script we own once its settings entry is already gone. Claude Code
/// may be executing it at that moment, which Windows refuses to delete; the
/// entry is what mattered, so a file that survives the retries is logged and
/// left for next time rather than failing a disable the user already got.
fn remove_owned_script(path: &Path) {
    match retry_transient_denied(|| std::fs::remove_file(path)) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::warn!("removing {} failed: {err}", path.display()),
    }
}

/// Remove our `statusLine` entry (never a user's own) and the script.
fn remove_claude_statusline() -> Result<()> {
    set_claude_statusline_setting(None)?;
    let script = claude_statusline_script_path();
    if script.exists() {
        remove_owned_script(&script);
    }
    Ok(())
}

fn remove_claude_guard_hook() -> Result<()> {
    let script_path = claude_guard_hook_path();
    // Match on the script path, not the full interpreter command (see codex counterpart).
    let fragment = script_path.display().to_string();
    for settings_path in claude_settings_candidates() {
        let _ = remove_guard_hook_entries(&settings_path, &fragment, false, None);
    }
    if script_path.exists() {
        let _ = std::fs::remove_file(&script_path);
    }
    Ok(())
}

/// Run `codex doctor` as an independent confirmation that Codex itself accepts
/// the route (stronger than our "is the text in the file" checks). Best-effort
/// and never a hard failure: a missing CLI or a doctor error for unrelated
/// reasons must not flip `verified`.
fn codex_doctor_summary() -> Option<String> {
    let codex = find_on_path(&["codex"])?;
    let mut command = crate::proc::command(codex);
    command.arg("doctor");
    // Its reachability probe can hang on a wedged network; a detached thread
    // waiting forever would leak one thread and one codex process per run.
    let output = crate::proc::output_with_timeout(command, Duration::from_secs(60)).ok()?;
    if output.status.success() {
        Some("`codex doctor` reports the Codex CLI install is healthy.".into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("run `codex doctor` for details");
        Some(format!("`codex doctor` reported issues: {}", first.trim()))
    }
}

fn remove_launchctl_env(keys: &[&str]) -> Result<()> {
    for key in keys {
        let _ = run_launchctl(&["unsetenv", key]);
    }
    Ok(())
}

fn run_launchctl(args: &[&str]) -> Result<std::process::Output> {
    let output = crate::proc::command("launchctl")
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
    // The end marker is searched AFTER the start, as marker_block_contains does.
    // Searched from the top, a stray end marker ahead of the block (a
    // hand-deleted opener, an interrupted write) read as "end before start",
    // so every launch appended another copy of the block. A start with no end
    // after it is treated as absent and a fresh block is appended.
    let updated = match existing
        .find(&start)
        .and_then(|s| existing[s..].find(&end).map(|rel| (s, s + rel)))
    {
        Some((start_idx, end_idx)) => {
            let end_with_marker = end_idx + end.len();
            let mut rebuilt = String::with_capacity(existing.len() + block.len());
            rebuilt.push_str(&existing[..start_idx]);
            rebuilt.push_str(&block);
            if end_with_marker < existing.len() {
                // `block` already ends in `\n`; if the surviving suffix also
                // starts with `\n`, drop one to avoid blank-line padding
                // accumulating between managed blocks on repeat applies.
                let suffix = &existing[end_with_marker..];
                let suffix = suffix.strip_prefix('\n').unwrap_or(suffix);
                rebuilt.push_str(suffix);
            }
            rebuilt
        }
        _ if existing.trim().is_empty() => block,
        _ => format!("{}\n{}", existing.trim_end(), block),
    };

    if updated == existing {
        return Ok((false, None));
    }

    let backup = backup_if_exists(file_path)?;
    atomic_write(file_path, updated.as_bytes())?;
    Ok((true, backup))
}

/// `upsert_managed_block` for an instruction nudge (CLAUDE.md, AGENTS.md). A
/// file a Windows editor saved as ANSI/UTF-16 is skipped, not failed: the
/// rewrite would mangle the user's bytes, and a missing nudge must not take the
/// hook and the rest of the integration down with it.
fn upsert_nudge_block(
    file_path: &Path,
    block_id: &str,
    block_body: &str,
) -> Result<(bool, Option<PathBuf>)> {
    match upsert_managed_block(file_path, block_id, block_body) {
        Err(err) if is_invalid_utf8(&err) => {
            log::warn!("leaving {} alone: not valid UTF-8", file_path.display());
            Ok((false, None))
        }
        other => other,
    }
}

fn write_file_if_changed(
    file_path: &Path,
    content: &str,
    executable: bool,
) -> Result<(bool, Option<PathBuf>)> {
    #[cfg(not(unix))]
    let _ = executable; // only used for chmod on unix
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
    atomic_write(file_path, content.as_bytes())?;

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
        remove_managed_block(file, block_id)?;
    }
    Ok(())
}

fn remove_managed_block(file_path: &Path, block_id: &str) -> Result<bool> {
    if !file_path.exists() {
        return Ok(false);
    }

    let bytes =
        std::fs::read(file_path).with_context(|| format!("reading {}", file_path.display()))?;
    let Ok(existing) = String::from_utf8(bytes) else {
        // ponytail: a non-UTF-8 profile is left untouched -- rewriting it from a
        // lossy decode would mangle the user's own bytes. Cost: a stale managed
        // block survives uninstall on such a file. Upgrade path if that matters:
        // splice the block out at the byte level instead of via String.
        log::info!(
            "leaving {} alone: not valid UTF-8, cannot rewrite safely",
            file_path.display()
        );
        return Ok(false);
    };
    // strip_marker_block pairs each start with the end AFTER it; the two
    // independent finds here duplicated the file instead of removing the block
    // when a stray end marker came first. It also removes every copy an older
    // upsert appended behind such a marker, and the stray marker itself.
    let mut rebuilt = strip_marker_block(&existing, block_id);
    if rebuilt == existing {
        return Ok(false);
    }
    if !rebuilt.is_empty() && !rebuilt.ends_with('\n') {
        rebuilt.push('\n');
    }

    let _ = backup_if_exists(file_path)?;
    atomic_write(file_path, rebuilt.as_bytes())?;
    Ok(true)
}

pub(crate) fn backup_if_exists(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let stamp = Utc::now().format("%Y%m%d%H%M%S");
    let backup_path = PathBuf::from(format!("{}.headroom-backup-{}", path.display(), stamp));
    // One apply rewrites a file several times within a second. The first
    // backup of the second holds the user's original; a later copy would
    // replace it with our own intermediate rewrite.
    if backup_path.exists() {
        return Ok(Some(backup_path));
    }
    retry_transient_denied(|| std::fs::copy(path, &backup_path))
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
            // nommer backups predate every headroom one (the app's old name);
            // sorted by path they came last and got each new backup pruned.
            backups.sort_by_key(|p| {
                let ours = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&headroom_prefix));
                (ours, p.clone())
            });
            if backups.len() > 3 {
                for old in &backups[..backups.len() - 3] {
                    let _ = std::fs::remove_file(old);
                }
            }
        }
    }

    Ok(Some(backup_path))
}

/// Reads a file for inspection only, replacing invalid UTF-8 instead of failing
/// on it. Shell profiles can carry non-UTF-8 bytes (RUST-5X), and marker/export
/// scanning only ever looks for ASCII. Never write the result back -- the lossy
/// decode would replace the user's bytes with U+FFFD.
fn read_to_string_lossy(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn shell_block_contains_in_files(
    shell_targets: &[PathBuf],
    block_id: &str,
    var_name: &str,
    expected_value: &str,
) -> Result<bool> {
    shell_block_contains_text_in_files(
        shell_targets,
        block_id,
        &format!("export {var_name}={expected_value}"),
    )
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
        // marker_block_contains searches the end marker after the start. Two
        // independent finds sliced `content[start..end]` and panicked when a
        // stray end marker came first, killing the watchdog and tray threads.
        if marker_block_contains(&read_to_string_lossy(file)?, block_id, expected_text) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn claude_settings_env_matches(env_key: &str, expected_value: &str) -> Result<bool> {
    let path = claude_settings_path();
    if !held_or_exists(&path) {
        return Ok(false);
    }

    let raw = read_held_or_disk(&path).with_context(|| format!("reading {}", path.display()))?;
    let content: Value = Value::Object(parse_json_object(&raw, &path)?);
    Ok(matches!(
        content.get("env").and_then(|env| env.get(env_key)),
        Some(Value::String(value)) if value == expected_value
    ))
}

fn claude_settings_hook_matches(hook_fragment: &str) -> Result<bool> {
    let path = claude_settings_path();
    if !held_or_exists(&path) {
        return Ok(false);
    }

    let raw = read_held_or_disk(&path).with_context(|| format!("reading {}", path.display()))?;
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

/// Cached because a single launcher "Continue" click verifies every installed
/// client, and `apply_client_setup` re-runs the whole write+verify once when
/// verification misses -- up to eight probes. While the backend process is up
/// but not yet answering `/readyz` (Windows warm-up is the slow case) each
/// probe burns the full timeout on both hosts, so those eight probes are
/// seconds of dead click. `proxy_reachable` is transient status, never a
/// `verified` input, so a 3s-stale reading is fine.
fn is_headroom_proxy_reachable() -> bool {
    static CACHE: std::sync::Mutex<Option<(bool, std::time::Instant)>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((reachable, at)) = *cache {
        if at.elapsed() < Duration::from_secs(3) {
            return reachable;
        }
    }
    let reachable = probe_headroom_proxy();
    *cache = Some((reachable, std::time::Instant::now()));
    reachable
}

fn probe_headroom_proxy() -> bool {
    let client = match reqwest::blocking::Client::builder()
        .no_proxy()
        .tls_built_in_root_certs(false)
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };

    ["127.0.0.1", "localhost"].iter().any(|host| {
        client
            .get(format!("http://{host}:6767/readyz"))
            .send()
            // 404 = an older proxy build without the /readyz route, still up and
            // serving -- count it as reachable (Sentry RUST-2X).
            .map(|response| {
                let status = response.status();
                status.is_success() || status == reqwest::StatusCode::NOT_FOUND
            })
            .unwrap_or(false)
    })
}

/// Pure core for `detect_oss_remnants`: given the environment facts, produce the
/// operator-facing warnings. Stale open-source-install remnants coexisting with
/// the paid desktop app are the root cause of instability under concurrent
/// agents (duplicate `mcp serve`, `:8787` vs Cursor OAuth callback conflicts,
/// hooks pointing at a non-app binary). Kept pure so it is unit-testable.
fn oss_remnant_warnings(
    local_headroom_exists: bool,
    local_rtk_exists: bool,
    port_8787_listening: bool,
    claude_hook_points_at_local_bin: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if port_8787_listening {
        warnings.push(
            "An open-source Headroom proxy is listening on :8787. It conflicts with the paid \
             desktop proxy (:6767/:6768) and Cursor MCP OAuth callbacks. Stop it and remove the \
             open-source install."
                .into(),
        );
    }
    if local_headroom_exists {
        warnings.push(
            "Found a stale open-source binary at ~/.local/bin/headroom. Remove it so only the \
             app-owned runtime serves MCP."
                .into(),
        );
    }
    if local_rtk_exists {
        warnings.push(
            "Found a stale open-source binary at ~/.local/bin/rtk. Remove it so the Claude hook \
             uses the app-owned RTK binary."
                .into(),
        );
    }
    if claude_hook_points_at_local_bin {
        warnings.push(
            "The Claude hook in ~/.claude/settings.json points at ~/.local/bin (open-source \
             install) instead of the app-owned binary. Re-run client setup to repair it."
                .into(),
        );
    }
    warnings
}

/// Gather real environment facts and return OSS-remnant warnings, empty when the
/// install is clean.
pub fn detect_oss_remnants() -> Vec<String> {
    let local_bin = home_dir().join(".local").join("bin");
    let hook_points_at_local_bin = std::fs::read_to_string(claude_settings_path())
        .map(|raw| raw.contains(".local/bin/rtk") || raw.contains(".local/bin/headroom"))
        .unwrap_or(false);
    oss_remnant_warnings(
        local_bin.join("headroom").exists(),
        local_bin.join("rtk").exists(),
        port_listening(8787),
        hook_points_at_local_bin,
    )
}

/// True when something accepts a TCP connection on `127.0.0.1:<port>`.
fn port_listening(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// The open-source Claude Code plugin runs this bare command at SessionStart
/// and before Bash/PowerShell calls, where it exits 127 because the app ships
/// no `headroom` on PATH. Replace only that exact command: no global PATH
/// mutation, and the same rewrite works on Windows, macOS, and Linux.
const OSS_PLUGIN_HOOK_COMMAND: &str = "headroom init hook ensure";
/// What we put in its place: a builtin every hook host we can be launched
/// under (sh, cmd.exe, PowerShell) understands. Deliberately not a path to a
/// file we ship -- an absolute path goes dead if our app data is ever removed
/// or relocated, stranding the plugin with a hook that fails on every Bash
/// call and a restore string we can no longer match.
const OSS_PLUGIN_MANAGED_COMMAND: &str = "exit 0";
static OSS_PLUGIN_HOOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Escape hatch. `HEADROOM_ABSORB_OSS_PLUGIN=0` restores anything we already
/// rewrote and then leaves the plugin alone.
fn oss_absorb_disabled() -> bool {
    std::env::var_os("HEADROOM_ABSORB_OSS_PLUGIN").is_some_and(|v| v == "0")
}

/// True when Claude Code has the open-source `headroom` plugin installed, from
/// any marketplace. It is mirrored under several marketplace names, so match the
/// plugin half of the `<plugin>@<marketplace>` key rather than a fixed ref.
fn oss_headroom_plugin_installed() -> bool {
    let Some(plugins) = crate::tool_manager::claude_installed_plugins() else {
        return false;
    };
    let Some(map) = plugins.get("plugins").and_then(Value::as_object) else {
        return false;
    };
    map.iter().any(|(key, installs)| {
        key.split('@').next() == Some("headroom")
            && installs.as_array().is_some_and(|list| !list.is_empty())
    })
}

/// Installed plugin records carry their cache directory. Resolve the hooks file
/// from there instead of guessing where Claude or its shell looks for commands.
fn oss_headroom_plugin_hook_paths() -> Vec<PathBuf> {
    let Some(plugins) = crate::tool_manager::claude_installed_plugins() else {
        return Vec::new();
    };
    let Some(map) = plugins.get("plugins").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut hooks = Vec::new();
    for (key, installs) in map {
        if key.split('@').next() != Some("headroom") {
            continue;
        }
        let Some(installs) = installs.as_array() else {
            continue;
        };
        for install in installs {
            let Some(root) = install.get("installPath").and_then(Value::as_str) else {
                continue;
            };
            let root = PathBuf::from(root);
            hooks.push(root.join("hooks").join("hooks.json"));
            hooks.push(root.join("hooks.json"));
        }
    }
    hooks.retain(|path| path.is_file());
    dedupe_paths(hooks)
}

fn oss_plugin_hook_receipt_path() -> PathBuf {
    config_file(&app_data_dir(), "oss-plugin-hooks.json")
}

fn load_oss_plugin_hook_receipt() -> Vec<PathBuf> {
    let path = oss_plugin_hook_receipt_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice(&bytes) {
        Ok(paths) => paths,
        Err(err) => {
            // This file is the only record of which third-party hooks we
            // rewrote. Silently overwriting an unreadable one strands them
            // neutralized with nothing left pointing at them, so keep a copy.
            log::warn!(
                "oss plugin hook: unreadable receipt {}: {err}",
                path.display()
            );
            let _ = backup_if_exists(&path);
            Vec::new()
        }
    }
}

fn save_oss_plugin_hook_receipt(paths: &[PathBuf]) -> Result<()> {
    let path = oss_plugin_hook_receipt_path();
    if paths.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    atomic_write(&path, &serde_json::to_vec_pretty(paths)?)
}

fn hook_file_contains_command(path: &Path, command: &str) -> Result<bool> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading OSS plugin hooks {}", path.display()))?;
    Ok(raw.contains(&serde_json::to_string(command)?))
}

/// Exact JSON-string replacement preserves the plugin's formatting and becomes
/// a no-op if upstream changes the command or schema.
fn replace_oss_plugin_hook_command(path: &Path, from: &str, to: &str) -> Result<bool> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading OSS plugin hooks {}", path.display()))?;
    serde_json::from_str::<Value>(&raw)
        .with_context(|| format!("parsing OSS plugin hooks {}", path.display()))?;
    let from = serde_json::to_string(from)?;
    if !raw.contains(&from) {
        return Ok(false);
    }
    let updated = raw.replace(&from, &serde_json::to_string(to)?);
    atomic_write(path, updated.as_bytes())?;
    Ok(true)
}

/// What the open-source plugin/CLI look like on this machine right now.
pub struct OssPluginStatus {
    pub plugin_installed: bool,
    pub hook_absorbed: bool,
    pub cli_on_path: bool,
    /// An open-source proxy is serving on :8787 (the OSS default port).
    pub oss_proxy_8787: bool,
    /// Claude Code's `ANTHROPIC_BASE_URL` still points at our proxy.
    pub base_url_ours: bool,
}

/// True when Claude Code's `ANTHROPIC_BASE_URL` still points at our proxy.
fn claude_base_url_is_ours() -> bool {
    std::fs::read_to_string(claude_settings_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| {
            v.get("env")?
                .get("ANTHROPIC_BASE_URL")?
                .as_str()
                .map(|url| url == HEADROOM_ANTHROPIC_BASE_URL)
        })
        .unwrap_or(false)
}

/// True when a real open-source `headroom` CLI exists for the plugin hook to
/// run. `find_on_path` alone is not enough: a GUI launch inherits launchd's
/// bare PATH, which never contains `~/.local/bin` -- exactly where the OSS
/// installer puts the binary. Probing the known install locations too is what
/// keeps us from neutralizing a plugin hook that works. Same helper Claude/
/// Codex detection uses, so a broken binary counts as absent and gets absorbed.
fn oss_cli_present() -> bool {
    crate::claude_cli::probe_on_path("headroom").is_some()
        || crate::claude_cli::probe_known_paths("headroom").is_some()
}

pub fn absorb_oss_plugin() -> OssPluginStatus {
    // Probing runs `headroom --version` against every known install location,
    // so it execs whatever binary of that name happens to be on disk. Nobody
    // without the plugin needs that at every launch: the answer only decides
    // whether to leave a plugin hook alone.
    let probe = !oss_absorb_disabled() && oss_headroom_plugin_installed();
    absorb_oss_plugin_with_cli_on_path(probe && oss_cli_present())
}

/// Cheap poll for the one state a single startup pass cannot cover: Claude Code
/// updated the plugin, which re-clones into a fresh version directory that never
/// saw our rewrite, so the bare command is back and failing on every Bash call.
/// A tray app can sit for weeks between launches, so waiting for the next start
/// means weeks of 127s.
///
/// Deliberately narrow. It fires only for users we are already managing (a
/// non-empty receipt) and only for a hook path we have not rewritten, so a user
/// with a real OSS CLI -- whose receipt is empty because we restored theirs --
/// never sends us back through the exec probe on a timer.
pub fn oss_plugin_hook_needs_absorbing() -> bool {
    if oss_absorb_disabled() {
        return false;
    }
    let receipt = load_oss_plugin_hook_receipt();
    if receipt.is_empty() {
        return false;
    }
    oss_headroom_plugin_hook_paths().iter().any(|path| {
        !receipt.contains(path)
            && matches!(
                hook_file_contains_command(path, OSS_PLUGIN_HOOK_COMMAND),
                Ok(true)
            )
    })
}

fn absorb_oss_plugin_with_cli_on_path(cli_on_path: bool) -> OssPluginStatus {
    let plugin_installed = oss_headroom_plugin_installed();
    let hooks = oss_headroom_plugin_hook_paths();
    let absorb = plugin_installed && !cli_on_path && !oss_absorb_disabled();
    let hook_absorbed = {
        let _guard = OSS_PLUGIN_HOOK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if crate::SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire) {
            false
        } else {
            reconcile_oss_plugin_hooks(&hooks, absorb).0
        }
    };

    OssPluginStatus {
        plugin_installed,
        hook_absorbed,
        cli_on_path,
        oss_proxy_8787: port_listening(8787),
        base_url_ours: claude_base_url_is_ours(),
    }
}

/// Neutralize or restore the exact OSS hook command. The receipt retains cache
/// paths after plugin removal, so uninstall can still restore inactive caches.
fn reconcile_oss_plugin_hooks(current_hooks: &[PathBuf], absorb: bool) -> (bool, Vec<String>) {
    let managed = OSS_PLUGIN_MANAGED_COMMAND;
    let mut hooks = load_oss_plugin_hook_receipt();
    hooks.extend_from_slice(current_hooks);
    hooks = dedupe_paths(hooks);

    // Persist targets before touching third-party files. If the app crashes
    // after the rewrite, the next launch or uninstall can still restore them.
    if absorb {
        if let Err(err) = save_oss_plugin_hook_receipt(&hooks) {
            log::warn!("oss plugin hook: preparing receipt failed: {err:#}");
            return (false, Vec::new());
        }
    }

    let mut changed = Vec::new();
    let mut still_managed = Vec::new();
    for path in hooks {
        // The receipt outlives the files it names: a plugin update re-clones
        // into a new version dir and the old one goes away. That is the normal
        // end of an entry, not a failure -- skipping it here keeps the read
        // error out of the log (and out of Sentry) and lets the entry fall off
        // the receipt below.
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                log::warn!(
                    "oss plugin hook: inspecting {} failed: {err}",
                    path.display()
                );
                if !absorb {
                    still_managed.push(path);
                }
                continue;
            }
        }
        let result = if absorb {
            replace_oss_plugin_hook_command(&path, OSS_PLUGIN_HOOK_COMMAND, managed)
        } else {
            replace_oss_plugin_hook_command(&path, managed, OSS_PLUGIN_HOOK_COMMAND)
        };
        match result {
            Ok(true) => changed.push(path.display().to_string()),
            Ok(false) => {}
            Err(err) => log::warn!("oss plugin hook: {err:#}"),
        }
        match hook_file_contains_command(&path, managed) {
            Ok(true) => still_managed.push(path),
            Ok(false) => {}
            Err(err) => {
                log::warn!("oss plugin hook: {err:#}");
                if !absorb {
                    still_managed.push(path);
                }
            }
        }
    }

    if let Err(err) = save_oss_plugin_hook_receipt(&still_managed) {
        log::warn!("oss plugin hook: saving receipt failed: {err:#}");
    }
    (!still_managed.is_empty(), changed)
}

fn restore_oss_plugin_hooks() -> (bool, Vec<String>) {
    let _guard = OSS_PLUGIN_HOOK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let hooks = oss_headroom_plugin_hook_paths();
    reconcile_oss_plugin_hooks(&hooks, false)
}

fn resolve_default_shell_targets() -> Vec<PathBuf> {
    let mut targets =
        discover_managed_shell_targets(&["managed_rtk", "claude_code"]).unwrap_or_default();
    if targets.is_empty() {
        targets = default_shell_targets_for_family(detect_shell_family());
    }
    dedupe_shell_targets(rehome_shell_targets(
        targets,
        legacy_shell_home().as_deref(),
        &shell_home(),
    ))
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
        .any(|path| path.is_file());
    let has_bash_files = [
        BASH_PROFILE_FILE,
        BASH_LOGIN_FILE,
        POSIX_PROFILE_FILE,
        BASH_RC_FILE,
    ]
    .into_iter()
    .map(shell_path)
    .any(|path| path.is_file());

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
            dedupe_shell_targets(vec![shell_path(ZSH_PROFILE_FILE), shell_path(ZSH_RC_FILE)])
        }
        ShellFamily::Bash => dedupe_shell_targets(vec![
            preferred_bash_profile_path(),
            shell_path(BASH_RC_FILE),
        ]),
        ShellFamily::Posix => dedupe_shell_targets(vec![shell_path(POSIX_PROFILE_FILE)]),
    }
}

fn preferred_bash_profile_path() -> PathBuf {
    [BASH_PROFILE_FILE, BASH_LOGIN_FILE, POSIX_PROFILE_FILE]
        .into_iter()
        .map(shell_path)
        .find(|path| path.is_file())
        .unwrap_or_else(|| shell_path(BASH_PROFILE_FILE))
}

fn discover_managed_shell_targets(block_ids: &[&str]) -> Result<Vec<PathBuf>> {
    let mut discovered = Vec::new();
    for file in current_shell_paths() {
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
        // A buggy build could persist an unexpanded, relative path (e.g.
        // `$XDG_CONFIG_HOME/zsh/.zshrc`); re-using it would create files under
        // the Finder-launch cwd `/`. Drop non-absolute stragglers.
        .filter(|p| p.is_absolute())
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

/// Dedupe a shell-target list and drop anything that already exists as a
/// directory. Such a path is neither readable nor rewritable: `read_to_string`
/// fails with `EISDIR` ("Is a directory", os error 21), which aborted the whole
/// client setup for a user whose `~/.profile` is a directory (RUST-5X/5Y/5Z:
/// it broke claude_code, codex and grok_build alike). A file we may not open
/// (chmod 000, a root-owned copy, a dotfiles symlink macOS privacy protection
/// denies) failed the same way, and at quit stopped disable before it removed
/// Claude Code's routing, so it is dropped too. Paths that do not exist yet
/// stay eligible; we create those.
fn dedupe_shell_targets(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    dedupe_paths(
        paths
            .into_iter()
            .filter(|path| {
                if path.is_dir() {
                    return false;
                }
                match std::fs::File::open(path) {
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                        // Once per path: the status poll resolves targets every tick.
                        let mut logged = UNREADABLE_SHELL_TARGETS_LOGGED
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if logged.insert(path.clone()) {
                            log::warn!("skipping shell profile {}: {err}", path.display());
                        }
                        false
                    }
                    _ => true,
                }
            })
            .collect(),
    )
}

static UNREADABLE_SHELL_TARGETS_LOGGED: std::sync::Mutex<BTreeSet<PathBuf>> =
    std::sync::Mutex::new(BTreeSet::new());

fn dedupe_strings(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

/// Every shell file a managed block may live in, for removal and cleanup:
/// the current locations plus, on Windows, the Git Bash files under
/// `%USERPROFILE%` that builds before the `shell_home` fix wrote.
fn all_shell_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = ALL_SHELL_FILES.into_iter().map(shell_path).collect();
    if let Some(legacy) = legacy_shell_home() {
        paths.extend(
            ALL_SHELL_FILES
                .into_iter()
                .filter(|name| !is_zsh_file_name(name))
                .map(|name| legacy.join(name)),
        );
    }
    dedupe_shell_targets(paths)
}

/// The shell files the current environment reads. Discovery uses these, not
/// [`all_shell_paths`], so a stale block at the legacy location is not taken
/// as the place to keep writing.
fn current_shell_paths() -> Vec<PathBuf> {
    dedupe_shell_targets(ALL_SHELL_FILES.into_iter().map(shell_path).collect())
}

fn is_zsh_file_name(name: &str) -> bool {
    matches!(name, ZSH_PROFILE_FILE | ZSH_RC_FILE)
}

/// Home directory Git Bash (and any POSIX shell) reads its profile files from.
/// `home_dir()` deliberately ignores `HOME` on Windows, but Git for Windows
/// honors a `HOME` the user set (user or system environment variable) and only
/// falls back to `%USERPROFILE%` without one, so blocks written under
/// `%USERPROFILE%` never loaded for those users (Windows rc9 pass:
/// HOME=C:\hrhome-test). Unchanged everywhere else.
fn shell_home() -> PathBuf {
    if cfg!(windows) && !cfg!(test) {
        git_bash_home(std::env::var_os("HOME"), home_dir(), |p| p.is_dir())
    } else {
        home_dir()
    }
}

/// `%USERPROFILE%` when Git Bash reads a different home: where earlier builds
/// wrote the blocks, so disable and cleanup still reach them.
fn legacy_shell_home() -> Option<PathBuf> {
    let home = home_dir();
    let shell = shell_home();
    (shell != home).then_some(home)
}

/// Resolve Git Bash's home from its `HOME` value: the MSYS form Git Bash
/// exports (`/c/Users/x`) is mapped to `C:\Users\x`, quotes are dropped, and
/// anything that is not an existing directory falls back to `profile`.
fn git_bash_home(
    home_env: Option<std::ffi::OsString>,
    profile: PathBuf,
    is_dir: impl Fn(&Path) -> bool,
) -> PathBuf {
    let Some(raw) = home_env.and_then(|v| v.into_string().ok()) else {
        return profile;
    };
    let raw = raw.trim().trim_matches('"').trim();
    if raw.is_empty() {
        return profile;
    }
    let bytes = raw.as_bytes();
    let native = if bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes.len() == 2 || bytes[2] == b'/')
    {
        let drive = (bytes[1] as char).to_ascii_uppercase();
        let rest = raw.get(3..).unwrap_or("").replace('/', "\\");
        format!("{drive}:\\{rest}")
    } else {
        raw.to_string()
    };
    let path = PathBuf::from(native);
    if is_dir(&path) {
        path
    } else {
        profile
    }
}

/// Map persisted Git Bash targets under the legacy home onto the current one,
/// so an existing install moves its blocks rather than keep rewriting the copy
/// Git Bash never reads. zsh files resolve through `zsh_dir` and stay put.
fn rehome_shell_targets(
    paths: Vec<PathBuf>,
    legacy: Option<&Path>,
    current: &Path,
) -> Vec<PathBuf> {
    let Some(legacy) = legacy else {
        return paths;
    };
    paths
        .into_iter()
        .map(
            |path| match (path.parent(), path.file_name().and_then(|n| n.to_str())) {
                (Some(parent), Some(name)) if parent == legacy && !is_zsh_file_name(name) => {
                    current.join(name)
                }
                _ => path,
            },
        )
        .collect()
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

    let content = read_to_string_lossy(file_path)?;
    let start = format!("# >>> headroom:{block_id} >>>");
    let end = format!("# <<< headroom:{block_id} <<<");
    Ok(content.contains(&start) && content.contains(&end))
}

fn shell_path(name: &str) -> PathBuf {
    match name {
        ZSH_PROFILE_FILE | ZSH_RC_FILE => zsh_dir().join(name),
        _ => shell_home().join(name),
    }
}

/// Directory zsh reads its rc/profile files from. zsh honors `$ZDOTDIR`
/// (falling back to `$HOME`); a Finder-launched app rarely inherits `$ZDOTDIR`
/// from the login shell, so when it's absent from our own env we recover it
/// from `~/.zshenv`, the file zsh always sources from `$HOME` and the
/// conventional place users set ZDOTDIR.
fn zsh_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZDOTDIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        // A relative ZDOTDIR would create files under the (Finder-launch) cwd
        // of `/`. Only trust it if absolute; otherwise fall through to $HOME.
        if dir.is_absolute() {
            return dir;
        }
    }
    // Asked once per home: every status poll resolves shell targets, and the
    // home only changes under TestHome.
    static ASKED: std::sync::Mutex<Option<(PathBuf, Option<PathBuf>)>> =
        std::sync::Mutex::new(None);
    let home = home_dir();
    let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    if asked
        .as_ref()
        .is_none_or(|(asked_home, _)| *asked_home != home)
    {
        *asked = Some((home.clone(), zdotdir_from_zsh(&home)));
    }
    let from_zsh = asked.as_ref().and_then(|(_, dir)| dir.clone());
    drop(asked);
    from_zsh
        .or_else(|| zdotdir_from_zshenv(&home))
        .unwrap_or(home)
}

/// Ask zsh itself. A non-interactive `zsh -c` sources only the zshenv files,
/// so every way of setting ZDOTDIR there resolves as the user's shells see it
/// (`"$HOME"/.config/zsh`, `${XDG_CONFIG_HOME:-$HOME/.config}/zsh`, a value
/// built on an earlier line), which the line parser below gets wrong. Only an
/// absolute, existing directory is trusted; no zsh, a slow zshenv or odd
/// output falls back to the parser.
fn zdotdir_from_zsh(home: &Path) -> Option<PathBuf> {
    let mut command = crate::proc::command("zsh");
    command
        .args(["-c", "print -rn -- \"${ZDOTDIR:-$HOME}\""])
        .env("HOME", home)
        .env_remove("ZDOTDIR");
    let output = crate::proc::output_with_timeout(command, Duration::from_secs(3)).ok()?;
    if !output.status.success() {
        return None;
    }
    // Last line: a zshenv that prints a banner puts it ahead of the answer.
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.lines().last()?);
    (dir.is_absolute() && dir.is_dir()).then_some(dir)
}

/// Expand `$VAR` / `${VAR}` from the process env. Unset vars are left as the
/// literal `$VAR` so the caller can detect an unresolved (non-absolute) path
/// and fall back rather than creating a bogus relative dir.
fn expand_env_vars(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            // Copy up to the next `$` as a str slice: `byte as char` mangled
            // every non-ASCII character in the path.
            let end = raw[i..].find('$').map_or(raw.len(), |o| i + o);
            out.push_str(&raw[i..end]);
            i = end;
            continue;
        }
        let (name, next) = if bytes.get(i + 1) == Some(&b'{') {
            match raw[i + 2..].find('}') {
                Some(end) => (&raw[i + 2..i + 2 + end], i + 2 + end + 1),
                None => (&raw[i..i], i + 1), // unterminated `${` -> emit `$` literally
            }
        } else {
            let end = raw[i + 1..]
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .map(|o| i + 1 + o)
                .unwrap_or(raw.len());
            (&raw[i + 1..end], end)
        };
        match (!name.is_empty())
            .then(|| std::env::var(name).ok())
            .flatten()
        {
            Some(val) => out.push_str(&val),
            None => out.push_str(&raw[i..next]), // keep literal `$VAR` when unset
        }
        i = next;
    }
    out
}

fn zdotdir_from_zshenv(home: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(home.join(".zshenv")).ok()?;
    for line in content.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(value) = line.strip_prefix("ZDOTDIR=") else {
            continue;
        };
        let raw = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.split('"').next()) {
            inner
        } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.split('\'').next()) {
            inner
        } else {
            value.split([' ', '\t', '#']).next().unwrap_or("")
        };
        if raw.is_empty() {
            continue;
        }
        let expanded = if let Some(tail) = raw.strip_prefix("~/") {
            home.join(tail)
        } else if raw == "~" {
            home.to_path_buf()
        } else if let Some(tail) = raw
            .strip_prefix("$HOME/")
            .or_else(|| raw.strip_prefix("${HOME}/"))
        {
            home.join(tail)
        } else {
            PathBuf::from(expand_env_vars(raw))
        };
        // If expansion couldn't fully resolve the value (unset env var leaves a
        // literal `$`, or it's otherwise relative), returning it would create a
        // bogus dir relative to cwd (e.g. `$XDG_CONFIG_HOME/zsh` under `/`).
        // Fall back to $HOME instead.
        if !expanded.is_absolute() {
            return None;
        }
        return Some(expanded);
    }
    None
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

fn headroom_markitdown_hook_path() -> PathBuf {
    home_dir()
        .join(".claude")
        .join("hooks")
        .join("headroom-markitdown-read.sh")
}

/// A PYTHONPATH dir holding a stand-in `markitdown` CLI and an importable
/// `speech_recognition`, so only MARKITDOWN_MAIN_NO_AUDIO's block keeps the
/// CLI from reporting "transcribed". It writes to `-o` or prints
/// `converted:<argc>:<first arg>`.
#[cfg(test)]
pub(crate) fn fake_markitdown_pythonpath(root: &Path) -> PathBuf {
    let dir = root.join("pylib");
    std::fs::create_dir_all(dir.join("markitdown")).unwrap();
    std::fs::write(dir.join("speech_recognition.py"), "").unwrap();
    std::fs::write(dir.join("markitdown").join("__init__.py"), "").unwrap();
    std::fs::write(
        dir.join("markitdown").join("__main__.py"),
        "import sys\n\
         def main():\n\
         \x20   try:\n\
         \x20       import speech_recognition\n\
         \x20       text = 'transcribed'\n\
         \x20   except ImportError:\n\
         \x20       text = 'converted'\n\
         \x20   args = sys.argv[1:]\n\
         \x20   if '-o' in args:\n\
         \x20       open(args[args.index('-o') + 1], 'w').write(text)\n\
         \x20   else:\n\
         \x20       print(f'{text}:{len(args)}:{args[0]}')\n",
    )
    .unwrap();
    dir
}

/// `python -c` body that runs the markitdown CLI with speech transcription
/// disabled. `markitdown[all]` ships SpeechRecognition, and its audio converter
/// posts any audio it is handed (a `.wav`, or audio named `.pdf`) to Google's
/// speech API over plain HTTP. Both callers run with no prompt. Holds no `'`,
/// since the shim and the Read hook embed it in single quotes.
///
/// It opens, like every `python -c` Headroom runs in a project dir, by dropping
/// the `""` (cwd) that `-c` puts first on sys.path, and the absolute cwd an
/// empty PYTHONPATH entry adds: otherwise a cloned repo's `markitdown/` or
/// `json.py` runs with no prompt. Not `-P`, which needs 3.11:
/// the tests run these snippets under a 3.9 system python.
pub(crate) const MARKITDOWN_MAIN_NO_AUDIO: &str = r#"import sys; import os; _cwd = os.path.realpath(os.getcwd()); sys.path[:] = [p for p in sys.path if p and os.path.realpath(p) != _cwd]; sys.modules["speech_recognition"] = None; from markitdown.__main__ import main; sys.argv[0] = "markitdown"; sys.exit(main())"#;

/// PreToolUse(Read) hook: when Claude reads a PDF, convert it to Markdown via
/// the managed `markitdown` and redirect the read at the converted file through
/// `updatedInput.file_path`. Fails open at every step so a missing binary,
/// oversized file, or conversion error falls through to a native Read, and so
/// does any read a Read rule could cover (see HOOK_RULES_PY), Windows included.
///
/// Scoped to PDF deliberately: Claude Code's Read tool rejects unsupported
/// binary types (docx/pptx/xlsx) at input validation *before* PreToolUse hooks
/// run, so a hook can never intercept them. Office formats are handled instead
/// by the managed CLAUDE.md nudge that points Claude at the `markitdown` CLI.
fn build_headroom_markitdown_hook(markitdown_path: &Path, python_path: &Path) -> String {
    let markitdown = shell_double_quote(&markitdown_path.to_string_lossy());
    let python = shell_double_quote(&python_path.to_string_lossy());
    let no_audio = MARKITDOWN_MAIN_NO_AUDIO;
    let rules = HOOK_RULES_PY;

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

HEADROOM_MARKITDOWN="{markitdown}"
HEADROOM_PYTHON="{python}"

if [ ! -x "$HEADROOM_MARKITDOWN" ] || [ ! -x "$HEADROOM_PYTHON" ]; then
  exit 0
fi

INPUT="$(cat)"
if [ -z "$INPUT" ]; then
  exit 0
fi

# -X utf8: the hook JSON on stdin/stdout is UTF-8, not the Windows locale codepage.
"$HEADROOM_PYTHON" -X utf8 -c '{rules}import json, os, subprocess, hashlib, stat, tempfile, time
ALLOWED = {{".pdf"}}
MAX_BYTES = 25 * 1024 * 1024
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
tool_input = data.get("tool_input")
if not isinstance(tool_input, dict):
    sys.exit(0)
fp = tool_input.get("file_path")
if not isinstance(fp, str) or not fp:
    sys.exit(0)
if os.path.splitext(fp)[1].lower() not in ALLOWED:
    sys.exit(0)
# The allow below skips the prompt Claude Code shows for reads outside the
# working directories, so only answer for files inside them.
roots = [os.path.realpath(d) for d in (data.get("cwd"), os.environ.get("CLAUDE_PROJECT_DIR")) if isinstance(d, str) and d]
full = os.path.realpath(os.path.join(roots[0], fp) if roots else fp)
if not any(full == r or full.startswith(r.rstrip(os.sep) + os.sep) for r in roots):
    sys.exit(0)
# Rules match the redirected cache path, never the PDF, so the allow below would
# dodge every Read rule, and a block on reads outside the project (the cache is
# outside it). Leave the read to Claude Code wherever one could apply.
rules = settings(data, windows=True)
if rules is None or rules[2] or any(not isinstance(r, str) or r.partition("(")[0].strip() == "Read" for r in rules[0] + rules[1]):
    sys.exit(0)
try:
    st = os.stat(fp)
except OSError:
    sys.exit(0)
if st.st_size > MAX_BYTES:
    sys.exit(0)
# Per-user cache, never a shared /tmp: another local user could pre-create a
# shared dir and plant content or symlinks the conversion would write through.
cache = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.join(os.path.expanduser("~"), ".cache"), "headroom-markitdown")
try:
    os.makedirs(cache, mode=0o700, exist_ok=True)
    cst = os.lstat(cache)
    if not stat.S_ISDIR(cst.st_mode) or (hasattr(os, "getuid") and cst.st_uid != os.getuid()):
        sys.exit(0)
    if cst.st_mode & 0o077:
        os.chmod(cache, 0o700)
except OSError:
    sys.exit(0)
key = hashlib.sha256((os.path.abspath(fp) + ":" + str(st.st_mtime_ns)).encode()).hexdigest()[:16]
out = os.path.join(cache, key + ".md")
if not (os.path.isfile(out) and not os.path.islink(out) and os.path.getsize(out) > 0):
    # Convert into a fresh private file, then rename over the target, so a
    # symlink already sitting at `out` is replaced instead of written through.
    tmp = None
    try:
        fd, tmp = tempfile.mkstemp(dir=cache, suffix=".tmp")
        os.close(fd)
        subprocess.run([sys.executable, "-c", {no_audio:?}, fp, "-o", tmp], check=True, capture_output=True, timeout=120)
        os.replace(tmp, out)
    except Exception:
        if tmp and os.path.exists(tmp):
            os.unlink(tmp)
        sys.exit(0)
    # Each (path, mtime) adds a file holding a whole document: drop week-old ones.
    try:
        for name in os.listdir(cache):
            old = os.path.join(cache, name)
            if os.lstat(old).st_mtime < time.time() - 7 * 86400:
                os.unlink(old)
    except OSError:
        pass
if not (os.path.exists(out) and os.path.getsize(out) > 0):
    sys.exit(0)
updated = dict(tool_input)
updated["file_path"] = out
json.dump({{"hookSpecificOutput": {{"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "Headroom MarkItDown conversion", "updatedInput": updated}}}}, sys.stdout)' <<<"$INPUT" 2>/dev/null || exit 0
"#
    )
}

fn shell_double_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// `C:\Users\x\bin` -> `/c/Users/x/bin`, the form Git Bash (MSYS) takes
/// in PATH. Anything that isn't a drive-letter path is returned unchanged.
fn msys_path(value: &str) -> String {
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
            let rest = chars.as_str().replace('\\', "/");
            let rest = rest.trim_matches('/');
            let drive = drive.to_ascii_lowercase();
            if rest.is_empty() {
                format!("/{drive}")
            } else {
                format!("/{drive}/{rest}")
            }
        }
        _ => value.to_string(),
    }
}

/// Opens the `python -c` of every hook that answers "allow" with a rewritten
/// input: Claude Code matches its rules against that rewrite, never the
/// original, so the hook must first see which rules could apply. Never contains
/// a single quote, like the scripts it opens. First line: see
/// MARKITDOWN_MAIN_NO_AUDIO.
const HOOK_RULES_PY: &str = r##"import sys; import os; _cwd = os.path.realpath(os.getcwd()); sys.path[:] = [p for p in sys.path if p and os.path.realpath(p) != _cwd]
import glob, json, re, subprocess


def cli_rules():
    # Rules on an ancestor command line: --settings, --disallowedTools.
    try:
        rows = subprocess.run(["ps", "-A", "-ww", "-o", "pid=", "-o", "ppid=", "-o", "command="], capture_output=True, text=True, timeout=5).stdout.splitlines()
    except Exception:
        return True
    procs = {}
    for row in rows:
        parts = row.split(None, 2)
        if len(parts) > 1 and parts[0].isdigit() and parts[1].isdigit():
            procs[int(parts[0])] = (int(parts[1]), parts[2] if len(parts) > 2 else "")
    pid = os.getppid()
    for _ in range(64):
        if pid == 0:
            return False
        if pid not in procs:
            return True
        pid, command = procs[pid]
        # The Headroom remote-control relaunch (CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE) sets env only.
        command = command.replace("--settings {\"env\":{\"ANTHROPIC_BASE_URL\":\"https://api.anthropic.com\"}}", "")
        if re.search(r"--(managed-)?settings|--disallowed", command):
            return True
    return True


def policy_keys():
    # The Windows policy tier Claude Code reads (a "Settings" value under either key).
    import winreg
    for hive in (winreg.HKEY_LOCAL_MACHINE, winreg.HKEY_CURRENT_USER):
        try:
            winreg.CloseKey(winreg.OpenKey(hive, "SOFTWARE\\Policies\\ClaudeCode"))
            return True
        except FileNotFoundError:
            continue
        except OSError:
            return True
    return False


def settings(data, windows=False):
    # (ask rules, deny rules, reads outside blocked) across what Claude Code loads,
    # or None when a source cannot be read here: registry and MDM policies,
    # managed files (a policyHelper hides their rules), the server-managed cache.
    # Windows only when the caller opts in (the rtk verdict never has).
    win = sys.platform == "win32"
    if win and not windows:
        return None
    conf = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    managed = [os.path.join(os.environ.get("ProgramFiles") or "C:\\Program Files", "ClaudeCode")] if win else ["/Library/Application Support/ClaudeCode", "/etc/claude-code"]
    opaque = [os.path.join(d, n) for d in managed for n in ("managed-settings.json", "managed-settings.d")]
    opaque.append(os.path.join(conf, "remote-settings.json"))
    if os.environ.get("CLAUDE_CODE_MANAGED_SETTINGS_PATH") or os.environ.get("CLAUDE_CODE_REMOTE_SETTINGS_PATH"):
        return None
    if any(os.path.exists(p) for p in opaque):
        return None
    if policy_keys() if win else glob.glob("/Library/Managed Preferences/**/com.anthropic.claudecode.plist", recursive=True):
        return None
    files = [os.path.join(conf, "settings.json"), os.path.join(conf, "settings.local.json")]
    bases = [os.environ.get("CLAUDE_PROJECT_DIR"), data.get("cwd")]
    for base in bases[:2]:
        if isinstance(base, str) and base:
            try:
                common = subprocess.run(["git", "-C", base, "rev-parse", "--path-format=absolute", "--git-common-dir"], capture_output=True, text=True, timeout=5).stdout.strip()
            except Exception:
                return None
            if common:
                bases.append(os.path.dirname(common))
    for base in bases:
        d = os.path.abspath(base) if isinstance(base, str) and base else ""
        while d:
            files += [os.path.join(d, ".claude", n) for n in ("settings.json", "settings.local.json")]
            d = "" if os.path.dirname(d) == d else os.path.dirname(d)
    ask, deny, blocks = [], [], False
    for path in files:
        try:
            with open(path, encoding="utf-8") as f:
                perms = json.load(f).get("permissions") or {}
            more_ask, more_deny = perms.get("ask") or [], perms.get("deny") or []
            if not isinstance(more_ask, list) or not isinstance(more_deny, list):
                return None
        except (FileNotFoundError, NotADirectoryError):
            continue
        except Exception:
            return None
        ask, deny = ask + more_ask, deny + more_deny
        blocks = blocks or bool(perms.get("blockReadsOutsideWorkingDirectories"))
    # ponytail: Windows has no ps, so an ancestor --settings or
    # --disallowedTools goes unseen there; read the parent chain through
    # CreateToolhelp32Snapshot (ctypes) if that ever matters.
    return None if not win and cli_rules() else (ask, deny, blocks)
"##;

/// The rtk hook's last step, after HOOK_RULES_PY: prints allow-with-the-rewrite,
/// or nothing.
const RTK_HOOK_VERDICT_PY: &str = r##"import json, os, re, shlex

data = json.load(sys.stdin)
tool_input = data.get("tool_input")
if not isinstance(tool_input, dict):
    sys.exit(0)

# Modes that run the built-in read-only set unasked. Not auto (a read-only
# command can wait for server-side classifier review, which an allow skips) and
# not plan (with auto mode available the classifier reviews planning commands).
QUIET_MODES = ("default", "acceptEdits", "dontAsk", "bypassPermissions")
# Read-only commands from the Claude Code built-in set that rtk 0.48 rewrites.
READ_ONLY = ("git", "ls", "cat", "head", "tail", "wc", "du", "stat", "diff", "tree", "find", "grep", "rg")
GIT_READ_ONLY = ("status", "diff", "log", "show", "branch")
# Flags that write or execute: long names (an abbreviation matches too), short letters.
SEARCH_DENY = (("--pre", "--pre-glob", "--search-zip", "--hostname-bin"), "z")
FLAG_DENY = {
    "git": (("--output", "--ext-diff", "--exec", "--exec-path", "--upload-pack"), "co"),
    "rg": SEARCH_DENY,
    "grep": SEARCH_DENY,
    "tree": ((), "oR"),
}
FIND_DENY = {"-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls", "-files0-from"}
BRANCH_LIST = {"-a", "-r", "-v", "-vv", "-l", "--list", "--show-current", "--all", "--remotes", "--verbose", "--no-color"}
BRANCH_VALUE = ("--contains", "--merged", "--no-merged")
SHELL = re.compile(r"[;&|<>`$(){}\x00-\x08\x0a-\x1f\x7f]")


def lists_branches(args):
    i = 0
    while i < len(args):
        if args[i] in BRANCH_VALUE:
            i += 1
            if i == len(args) or args[i].startswith("-"):
                return False
        elif args[i] not in BRANCH_LIST and args[i].split("=", 1)[0] not in BRANCH_VALUE:
            return False
        i += 1
    return True


def inside(arg, roots):
    # Claude Code prompts for reads outside the working directories, so must we.
    if len(arg) > 512:
        return False
    if arg.startswith("--"):
        return "=" not in arg or inside(arg.split("=", 1)[1], roots)
    if arg.startswith("-"):
        # A short flag takes its value attached after any letter (-f/x, -rflink).
        return all(inside(arg[k:], roots) for k in range(2, len(arg)))
    if arg.startswith("~"):
        return False
    base = roots[0] if roots else os.getcwd()
    full = os.path.realpath(os.path.join(base, arg))
    return any(full == r or full.startswith(r.rstrip(os.sep) + os.sep) for r in roots)


def read_only(cmd, out):
    # One plain read-only command, rewritten to one plain rtk call.
    if len(cmd) > 10000 or SHELL.search(cmd) or SHELL.search(out) or not out.startswith("rtk "):
        return False
    try:
        argv = shlex.split(cmd)
    except ValueError:
        return False
    if not argv or argv[0] not in READ_ONLY:
        return False
    roots = [os.path.realpath(d) for d in (data.get("cwd"), os.environ.get("CLAUDE_PROJECT_DIR")) if isinstance(d, str) and d]
    if not roots or not all(inside(a, roots) for a in argv[1:]):
        return False
    name, args = argv[0], argv[1:]
    # An unquoted glob can expand to a file named -delete or --pre=sh, or to a
    # symlink out of the project that `inside` only saw as the literal pattern.
    # A backslash hides flags from the check and fakes quotes (\x27 z* \x27)
    # the quote strip below would pair, so it refuses outright.
    if "\\" in cmd or re.search(r"[*?\[]", re.sub(r"\x27[^\x27]*\x27|\"[^\"]*\"", "", cmd)):
        return False
    if name == "find":
        return not FIND_DENY.intersection(args)
    if name == "git":
        if not args or args[0] not in GIT_READ_ONLY or args[0] == "branch" and not lists_branches(args[1:]):
            return False
        args = args[1:]
    longs, shorts = FLAG_DENY.get(name, ((), ""))
    for a in args:
        flag = a.split("=", 1)[0]
        if flag.startswith("--"):
            if flag != "--" and any(x.startswith(flag) for x in longs):
                return False
        elif flag.startswith("-") and set(a[1:]) & set(shorts):
            return False
    return True


def hides(rule, name, asking):
    # Could the rewrite hide `name ...` from this rule? Read rules reach cat, head
    # and tail, and an ask rule naming the command stops matching `rtk ...`. rtk
    # itself answers 2 for a Bash deny rule.
    if not isinstance(rule, str):
        return True
    tool, _, arg = rule.partition("(")
    if tool.strip() == "Read":
        return True
    if not asking or tool.strip() != "Bash":
        return False
    word = (arg.rstrip(") ").split() or ["*"])[0].split(":")[0]
    return "*" in word or word == name


mode, cmd = data.get("permission_mode"), tool_input.get("command")
if os.environ.get("HEADROOM_RTK_RC") == "0":
    # rtk judged only the rules it can read. A managed, MDM or command-line
    # deny it never saw would be dodged by this allow, so stay silent there.
    # ponytail: Windows keeps the unconditional allow, since settings() cannot
    # read registry policies and gating would turn RTK off there outright;
    # close it by reading HKLM\SOFTWARE\Policies\ClaudeCode via winreg.
    if sys.platform != "win32" and settings(data) is None:
        sys.exit(0)
else:
    ro =mode in QUIET_MODES and isinstance(cmd, str) and read_only(cmd, os.environ.get("HEADROOM_RTK_OUT", ""))
    rules = settings(data) if ro or mode == "bypassPermissions" else None
    if rules is None:
        sys.exit(0)
    ask, deny, blocks = rules
    name = shlex.split(cmd)[0] if ro else ""
    bypass = mode == "bypassPermissions" and not (ask or deny or blocks)
    if not bypass and not (ro and not blocks and not any(hides(r, name, True) for r in ask) and not any(hides(r, name, True) for r in deny)):
        sys.exit(0)
updated = dict(tool_input)
updated["command"] = os.environ["HEADROOM_RTK_REWRITTEN"]
json.dump({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "Headroom RTK auto-rewrite", "updatedInput": updated}}, sys.stdout)
"##;

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

CMD="$("$HEADROOM_PYTHON" -X utf8 -c 'import sys; import os; _cwd = os.path.realpath(os.getcwd()); sys.path[:] = [p for p in sys.path if p and os.path.realpath(p) != _cwd]; import json; data = json.load(sys.stdin); cmd = data.get("tool_input", {{}}).get("command", ""); print(cmd if isinstance(cmd, str) else "")' <<<"$INPUT" 2>/dev/null || true)"
if [ -z "$CMD" ]; then
  exit 0
fi

# `rtk git diff --check` swallows the whitespace-error report the flag exists to
# produce (only the exit code survives), so any --check command must stay raw.
case " $CMD " in
  *" --check "*) exit 0 ;;
esac

# The exit code is rtk's permission verdict against the user's Claude Code
# rules: 0 = every segment is allowed, 3 = rewrite but the user must still be
# asked, 1 = no rtk equivalent, 2 = a deny rule matched. rtk knows only explicit
# rules, so it answers 3 even for `git status`. Three outcomes:
#   0: rewrite and allow.
#   3: rewrite and allow only where Claude Code would not ask either (the
#      verdict script): one plain read-only command in a mode that runs those
#      unasked, or anything in bypassPermissions, and in both cases only when
#      no rule could be dodged, since rules match the rewritten command.
#   anything else: no output, the ORIGINAL command goes to Claude Code. A
#      rewrite handed back undecided would be judged as `export PATH=...; rtk
#      ...`, which misses Claude Code's read-only allowlist and every CLI/skill
#      `Bash(...)` rule, so `git status` would prompt (or be denied headless).
#
# rtk takes project rules from the nearest `.claude/` at or above its cwd,
# while Claude Code loads only the project root's, so a `.claude/` planted in
# a parent or a vendored subdir could grant rtk an allow Claude Code never
# loaded. Run it where those rules really come from: the project root when
# it has one, else the user's own ~/.claude.
RTK_CWD="${{CLAUDE_PROJECT_DIR:-}}"
if [ -z "$RTK_CWD" ] || [ ! -d "$RTK_CWD/.claude" ]; then
  RTK_CWD="$HOME"
fi
RTK_RC=0
REWRITTEN="$(cd "$RTK_CWD" && "$HEADROOM_RTK" rewrite "$CMD" 2>/dev/null)" || RTK_RC=$?
case "$RTK_RC" in
  0|3) ;;
  *) exit 0 ;;
esac
if [ -z "$REWRITTEN" ] || [ "$CMD" = "$REWRITTEN" ]; then
  exit 0
fi
RTK_OUT="$REWRITTEN"

# `rtk rewrite` emits a bare `rtk` leading token, which only resolves if the
# managed PATH export has propagated into this session's environment. GUI apps
# (VSCode, terminals) launched before rtk was enabled inherit a stale PATH, so
# `rtk` is missing and the rewrite would fail with "command not found". Pin the
# leading token to the managed binary's absolute path so it works regardless.
#
# The path is spliced into a command the shell re-parses, so it must be quoted
# and in the shell's own form. On Windows the managed path is `C:\...\rtk.exe`:
# unquoted, Git Bash eats every backslash ("C:Users...rtk.exe: command not
# found"), and in a PATH entry the drive colon splits it in two. `cygpath -u`
# turns it into `/c/...`; `printf %q` quotes spaces and other metacharacters.
HEADROOM_RTK_SH="$HEADROOM_RTK"
if command -v cygpath >/dev/null 2>&1; then
  HEADROOM_RTK_SH="$(cygpath -u "$HEADROOM_RTK" 2>/dev/null || printf '%s' "$HEADROOM_RTK")"
fi
HEADROOM_RTK_Q="$(printf '%q' "$HEADROOM_RTK_SH")"
HEADROOM_RTK_DIR_Q="$(printf '%q' "$(dirname "$HEADROOM_RTK_SH")")"

if [ "${{REWRITTEN%% *}}" = "rtk" ]; then
  # Already checked executable above; the quoted token can't be re-tested by
  # splitting on spaces, so skip the first-token guard below.
  REWRITTEN="$HEADROOM_RTK_Q${{REWRITTEN#rtk}}"
else
  # Defense-in-depth: if the rewritten command's first token isn't resolvable
  # (e.g. a partial uninstall left `rtk` missing from PATH), fall through to the
  # original command instead of handing Claude Code a command that will fail
  # with "command not found".
  FIRST_TOKEN="${{REWRITTEN%% *}}"
  case "$FIRST_TOKEN" in
    /*)
      [ -x "$FIRST_TOKEN" ] || exit 0
      ;;
    *)
      command -v "$FIRST_TOKEN" >/dev/null 2>&1 || exit 0
      ;;
  esac
fi

# The pin above only fixes the LEADING token. `rtk rewrite` also emits `rtk`
# embedded after a `&&`, `;`, or `|` (e.g. `cd web && rtk npx ...`), and those
# stay bare -- they fail with "command not found: rtk" in the non-interactive,
# non-login shell Claude Code's Bash tool spawns, which sources only ~/.zshenv
# (never the .zprofile/.zshrc where the managed PATH export lands). Prepend the
# managed bin dir to PATH for this one invocation so every `rtk`, at any
# position, resolves regardless of which profile files the shell sourced.
REWRITTEN="export PATH=$HEADROOM_RTK_DIR_Q:\"\$PATH\"; $REWRITTEN"

HEADROOM_RTK_RC="$RTK_RC" HEADROOM_RTK_OUT="$RTK_OUT" HEADROOM_RTK_REWRITTEN="$REWRITTEN" \
  "$HEADROOM_PYTHON" -X utf8 -c '{rules}{verdict}' <<<"$INPUT" 2>/dev/null || exit 0
"#,
        rules = HOOK_RULES_PY,
        verdict = RTK_HOOK_VERDICT_PY
    )
}

/// The user's home, as the tools we configure see it. The one resolver every
/// module goes through.
///
/// On Windows that is the profile folder (`dirs::home_dir`, the known-folder
/// API), NOT `HOME`: Claude Code (Node's `os.homedir()`), Codex and the Python
/// proxy (`Path.home()`) all ignore `HOME` there. A machine with a persistent
/// `HOME` (a corporate `H:\`, some Git setups) had the desktop writing
/// `~/.claude` settings and reading `~/.headroom` ledgers and logs in one
/// place while every tool it configures used another.
///
/// `HOME` still comes first on Unix, where the two agree, and in test builds
/// on every platform: TestHome redirects through it, and without that the
/// Windows test job would write into the runner's real profile.
pub(crate) fn home_dir() -> PathBuf {
    let from_env = || {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) && !cfg!(test) {
        dirs::home_dir()
            .or_else(from_env)
            .unwrap_or_else(std::env::temp_dir)
    } else {
        from_env()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir)
    }
}

/// Codex's home directory. Mirrors the Codex CLI and the upstream Headroom
/// proxy: honor `$CODEX_HOME` when set, else `~/.codex`. Staying in sync with
/// the proxy matters — if the two layers disagree on where Codex lives, the
/// provider retag rewrites a different store than the config it edited.
pub(crate) fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

/// Grok Build's home directory. Honors `$GROK_HOME` when set, else `~/.grok`.
fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".grok"))
}

/// Claude Desktop is not a supported client: its bundled Claude Code pins
/// provider routing to the host, so nothing Headroom configures reaches it.
/// A machine that has only it installed still deserves to hear that in words
/// rather than a generic "no coding tool found". Presence only, no version.
pub(crate) fn claude_desktop_installed() -> bool {
    let home = home_dir();
    let mut candidates = vec![
        PathBuf::from("/Applications/Claude.app"),
        home.join("Applications").join("Claude.app"),
    ];
    // The Squirrel install root, which uninstall removes. Deliberately NOT the
    // Electron userData dirs (`~/Library/Application Support/Claude`,
    // `%APPDATA%\Claude`): those outlive an uninstall, so they would tell a
    // former user we cannot work with an app they already deleted. Missing an
    // install in a non-standard location is the safe direction -- the copy is
    // an extra explanation and its absence leaves the correct generic text.
    //
    // Claude Desktop now ships on Windows as MSIX only, and the migration
    // deletes the Squirrel root, so every current install is the package. Its
    // per-user data dir is keyed by the package family name and Windows removes
    // it with the package; the versioned `WindowsApps` folder is not listable.
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(base);
        candidates.push(base.join("AnthropicClaude"));
        candidates.push(base.join("Packages").join("Claude_pzs8sxrjxfjjc"));
    }
    candidates.iter().any(|path| path.exists())
}

/// Which Claude-Desktop bucket this machine is in, for the identity payload.
/// `absent` | `only` (the app and nothing we can route) | `with_agent`.
///
/// The `only` bucket cannot be served at all -- its bundled Claude Code pins
/// provider routing to the host -- so an activation funnel that counts it as a
/// drop-off is measuring a user we were never able to reach. `with_agent`
/// separates the other invisible case: a routable client is installed and
/// configured, but the user only ever prompts inside the app, which today
/// looks identical to "installed it and lost interest".
///
/// `detect_clients` only runs when the app is present, so the common answer
/// costs three `exists()` calls.
pub(crate) fn claude_desktop_verdict() -> &'static str {
    if !claude_desktop_installed() {
        return "absent";
    }
    if detect_clients().iter().any(|client| client.installed) {
        "with_agent"
    } else {
        "only"
    }
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

fn detect_codex_client(configured: bool) -> ClientStatus {
    let executable = codex_candidate_paths()
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| find_on_path(&["codex"]));

    let detected = executable
        .as_ref()
        .map(|path| format!("Detected at {}", path.display()))
        .or_else(|| {
            chatgpt_app_path()
                .map(|path| format!("Detected the ChatGPT app at {}.", path.display()))
        })
        .or_else(|| {
            codex_user_state_exists()
                .then(|| format!("Detected ChatGPT Codex data in {}.", codex_home().display()))
        });

    if let Some(detected_note) = detected {
        return ClientStatus {
            id: "codex".into(),
            name: "ChatGPT Codex".into(),
            installed: true,
            configured,
            health: if configured {
                ClientHealth::Healthy
            } else {
                ClientHealth::Attention
            },
            notes: if configured {
                vec![detected_note, "Configured by Headroom.".into()]
            } else {
                vec![
                    detected_note,
                    "Route ChatGPT Codex through Headroom's localhost proxy so prompts stay lean."
                        .into(),
                ]
            },
        };
    }

    ClientStatus {
        id: "codex".into(),
        name: "ChatGPT Codex".into(),
        installed: false,
        configured: false,
        health: ClientHealth::NotDetected,
        notes: vec!["Not detected on this machine yet.".into()],
    }
}

fn detect_grok_build_client(configured: bool) -> ClientStatus {
    let executable = grok_candidate_paths()
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| find_on_path(&["grok"]));

    let detected = executable
        .as_ref()
        .map(|path| format!("Detected at {}", path.display()))
        .or_else(|| {
            grok_user_state_exists()
                .then(|| format!("Detected Grok Build data in {}.", grok_home().display()))
        });

    if let Some(detected_note) = detected {
        return ClientStatus {
            id: "grok_build".into(),
            name: "Grok Build".into(),
            installed: true,
            configured,
            health: if configured {
                ClientHealth::Healthy
            } else {
                ClientHealth::Attention
            },
            notes: if configured {
                vec![detected_note, "Configured by Headroom.".into()]
            } else {
                vec![
                    detected_note,
                    "Route Grok Build through Headroom's localhost proxy so prompts stay lean."
                        .into(),
                ]
            },
        };
    }

    ClientStatus {
        id: "grok_build".into(),
        name: "Grok Build".into(),
        installed: false,
        configured: false,
        health: ClientHealth::NotDetected,
        notes: vec!["Not detected on this machine yet.".into()],
    }
}

fn grok_candidate_paths() -> Vec<PathBuf> {
    let home = home_dir();
    let mut candidates = vec![
        // Official installer target (verified against grok 0.2.112).
        home.join(".grok").join("bin").join("grok"),
        PathBuf::from("/usr/local/bin/grok"),
        PathBuf::from("/opt/homebrew/bin/grok"),
        home.join(".grok")
            .join("downloads")
            .join("grok-macos-aarch64"),
        home.join(".grok")
            .join("downloads")
            .join("grok-macos-x86_64"),
    ];

    let user_bin_dirs = vec![
        home.join(".local").join("bin"),
        home.join("bin"),
        home.join(".cargo").join("bin"),
    ];
    candidates.extend(binary_candidates_in_dirs(&user_bin_dirs, &["grok"]));
    dedupe_paths(candidates)
}

/// Deliberately excludes config.toml: setup itself creates one, which would
/// make detection self-fulfilling after disable (same rule as opencode).
fn grok_user_state_exists() -> bool {
    let grok_root = grok_home();
    grok_root.join("auth.json").exists()
        || grok_root.join("sessions").exists()
        || grok_root.join("downloads").exists()
        || grok_root.join("bin").exists()
}

fn codex_candidate_paths() -> Vec<PathBuf> {
    let home = home_dir();
    let binary_names = ["codex"];
    let mut candidates = vec![
        PathBuf::from("/usr/local/bin/codex"),
        PathBuf::from("/opt/homebrew/bin/codex"),
    ];

    let user_bin_dirs = vec![
        home.join(".local").join("bin"),
        home.join(".cargo").join("bin"),
        home.join("bin"),
        home.join(".npm-global").join("bin"),
        home.join(".yarn").join("bin"),
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

fn codex_user_state_exists() -> bool {
    let codex_root = codex_home();
    codex_root.join("config.toml").exists()
        || codex_root.join("auth.json").exists()
        || codex_root.join("sessions").exists()
        // Written by the unified ChatGPT app's Codex mode even before sign-in.
        || codex_root.join(".codex-global-state.json").exists()
}

/// The unified ChatGPT desktop app (the standalone Codex app was absorbed into
/// it on 2026-07-09; the bundle id stays com.openai.codex). Its Codex mode
/// reads ~/.codex/config.toml, so app presence alone makes the connector
/// configurable without the CLI binary on disk.
fn chatgpt_app_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        [
            PathBuf::from("/Applications/ChatGPT.app"),
            home_dir().join("Applications").join("ChatGPT.app"),
        ]
        .into_iter()
        .find(|path| path.exists())
    }
    #[cfg(target_os = "windows")]
    {
        let exe = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("Programs")
            .join("ChatGPT")
            .join("ChatGPT.exe");
        exe.exists().then_some(exe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Locate a Codex CLI that actually runs, for the Headroom Learn analysis
/// backend (`codex exec`). Smoke-tested like the MCP/plugin paths: the first
/// `codex` that merely exists can be an x86_64 leftover in /usr/local that
/// fails with ENOEXEC on an arm64 Mac without Rosetta.
pub(crate) fn detect_codex_cli() -> Option<PathBuf> {
    crate::claude_cli::detect_codex_cli()
}

/// True once the user has signed in to Codex with their ChatGPT account — the
/// OAuth token lands in `~/.codex/auth.json`. Required for the keyless
/// `codex exec` analysis backend.
pub(crate) fn codex_logged_in() -> bool {
    codex_home().join("auth.json").is_file()
}

fn parse_json_object(raw: &str, path: &Path) -> Result<serde_json::Map<String, Value>> {
    // An empty file (a `touch`, a writer that died mid-write) holds no settings
    // to protect, and both parsers reject it: that blocked setup until the user
    // fixed the file by hand.
    if raw.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => {
            let value = json5::from_str(raw).with_context(|| {
                format!(
                    "parsing {} failed (JSON/JSON5); refusing to overwrite potentially valid user settings",
                    path.display()
                )
            })?;
            // Writers re-serialize with serde_json, which strips the
            // comments/relaxed syntax that forced the JSON5 fallback. Log it
            // locally so the .headroom-backup is discoverable, but do NOT
            // capture to Sentry: this is expected, benign behavior (user keeps
            // comments in their settings), and the capture just inflated
            // RUST-4R with 120+ no-action events. Local info only.
            log::info!(
                "{} contains JSON5 syntax (comments/trailing commas); a Headroom rewrite will normalize it to strict JSON — the original is kept as a .headroom-backup file",
                path.display()
            );
            value
        }
    };
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("{} must contain a top-level JSON object", path.display()))
}

pub(crate) fn find_on_path(binary_names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_on_path_entries(std::env::split_paths(&path_var), binary_names)
}

fn find_on_path_entries<I>(path_entries: I, binary_names: &[&str]) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    for entry in path_entries {
        for binary_name in binary_names {
            // PATHEXT variants first on Windows: npm drops an extensionless
            // shim (`claude`, a bash script) next to `claude.cmd`, and only
            // the PATHEXT one is executable there. Matching the bare name
            // first handed callers a path Windows cannot spawn.
            if cfg!(windows) {
                for ext in windows_path_extensions() {
                    let with_ext = entry.join(format!("{binary_name}{ext}"));
                    if with_ext.exists() {
                        return Some(with_ext);
                    }
                }
            }

            let candidate = entry.join(binary_name);
            if candidate.exists() {
                return Some(candidate);
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
    #[test]
    fn codex_retag_skip_class_reports_only_what_a_release_can_fix() {
        use super::codex_retag_skip_class;
        // A disk the user's Codex corrupted is not ours (RUST-95/96).
        assert_eq!(
            codex_retag_skip_class(&["database disk image is malformed".into()]),
            None
        );
        assert_eq!(codex_retag_skip_class(&["disk I/O error".into()]), None);
        // A lock outliving busy_timeout is: one class for the whole pass,
        // however many of Codex's own DBs it happened to hold (RUST-EK/EM/EN).
        assert_eq!(
            codex_retag_skip_class(&[
                "database disk image is malformed".into(),
                "database is locked".into(),
                "database is locked".into(),
            ]),
            Some("locked")
        );
        assert_eq!(codex_retag_skip_class(&[]), None);
    }

    /// A retag pass runs on every launch AND every quit, and a Codex that is
    /// open holds its own store past `busy_timeout` routinely -- so without a
    /// per-session cap the one condition files a Warning per launch forever.
    /// The event has to stay a HOST count.
    #[test]
    fn a_retag_skip_class_reports_once_per_session() {
        use super::claim_retag_skip_report_slot;
        // Slug is deliberately not one of the real classes: the static is
        // process-global, so a real one would couple this to run order.
        assert!(claim_retag_skip_report_slot("test-only-class"));
        assert!(!claim_retag_skip_report_slot("test-only-class"));
        // A different class is still worth one event of its own.
        assert!(claim_retag_skip_report_slot("test-only-other"));
    }

    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{
        build_claude_guard_script, build_codex_guard_script, build_headroom_markitdown_hook,
        build_headroom_rtk_hook, build_markitdown_codex_nudge, build_markitdown_office_nudge,
        claude_code_user_state_exists, claude_hook_present_in_value, codex_home,
        codex_sqlite_store_expected, default_shell_targets_for_family, discover_codex_state_dbs,
        edit_vscode_wrapper_key, entry_contains_hook, find_on_path_entries, is_no_space,
        is_permission_denied, msys_path, normalize_setup_state, normalized_setup_id,
        nvm_binary_candidates, oss_remnant_warnings, parse_json_object, pin_codex_mcp_command,
        remove_managed_block, remove_pre_tool_use_markers, render_codex_config,
        retag_codex_thread_providers, retag_codex_threads_to_headroom, retag_one_codex_db,
        serialize_paths, shell_block_contains_in_files, shell_block_contains_text_in_files,
        shell_double_quote, strip_headroom_hook_from_settings, upsert_managed_block,
        vscode_settings_failure_level, write_file_if_changed, ClientSetupState, ShellFamily,
        NO_SPACE_OS_ERRORS, PERMISSION_DENIED_OS_ERRORS, VSCODE_PROCESS_WRAPPER_KEY,
    };
    use super::{build_claude_remote_control_wrapper, build_windows_wrapper_exe};
    #[cfg(unix)]
    use super::{
        build_claude_statusline_script, claude_settings_path, claude_statusline_script_path,
        ensure_claude_statusline, is_our_statusline, remove_claude_statusline,
        set_statusline_enabled, CLAUDE_STATUSLINE_SCRIPT,
    };
    #[cfg(unix)]
    use super::{
        claude_code_shell_block, claude_remote_control_command_path,
        claude_remote_control_hook_command, claude_remote_control_panel_command_path,
        claude_remote_control_script_path, claude_remote_control_wrapper_path,
        ensure_claude_remote_control_command, remove_claude_remote_control_command,
        remove_vscode_wrapper_file_if_unreferenced, vscode_user_settings_path,
        CLAUDE_REMOTE_CONTROL_COMMAND_MARKER, CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE,
        HEADROOM_ANTHROPIC_BASE_URL,
    };
    #[cfg(target_os = "windows")]
    use super::{claude_guard_command, codex_guard_command};
    use rusqlite::Connection;
    use serde_json::Value;

    #[test]
    fn strip_headroom_mcp_toml_removes_owned_tables_keeps_user_tables() {
        // Same straddled-HOME race as the JSON twin below: this reads
        // app_data_dir() to build the fixture and strip_headroom_mcp_toml
        // reads it again to match, while sibling tests flip HOME to a tempdir.
        let _env_lock = crate::test_env_lock::lock_home();
        let app_dir = crate::storage::app_data_dir().display().to_string();
        // Real config.toml files hold these paths as TOML basic strings, so a
        // Windows path's backslashes arrive escaped; the fixture must match.
        let headroom_cmd =
            super::toml_basic_string(&format!("{app_dir}/runtime/venv/bin/headroom"));
        let serena_cmd = super::toml_basic_string(&format!("{app_dir}/serena-venv/bin/serena"));
        let content = format!(
            "model = \"gpt-5\"\n\
             \n\
             # --- Headroom MCP server ---\n\
             [mcp_servers.headroom]\n\
             command = {headroom_cmd}\n\
             args = [\"mcp\", \"serve\"]\n\
             \n\
             [mcp_servers.headroom.env]\n\
             HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n\
             # --- end Headroom MCP server ---\n\
             # --- Headroom MCP server: serena ---\n\
             [mcp_servers.serena]\n\
             command = {serena_cmd}\n\
             \n\
             [mcp_servers.context7]\n\
             command = \"npx\"\n\
             \n\
             [mcp_servers.node_repl]\n\
             command = \"/Applications/ChatGPT.app/bin/node_repl\"\n"
        );
        let stripped = super::strip_headroom_mcp_toml(&content);
        assert!(!stripped.contains("mcp_servers.headroom"));
        assert!(!stripped.contains("serena"));
        assert!(!stripped.contains("Headroom MCP server"));
        assert!(stripped.contains("[mcp_servers.context7]"));
        assert!(stripped.contains("command = \"npx\""));
        assert!(stripped.contains("[mcp_servers.node_repl]"));
        assert!(stripped.contains("model = \"gpt-5\""));
    }

    #[test]
    fn strip_headroom_mcp_toml_is_noop_without_headroom_entries() {
        let content = "[mcp_servers.node_repl]\ncommand = \"/usr/local/bin/node_repl\"\n";
        assert_eq!(
            super::strip_headroom_mcp_toml(content),
            content.trim_end_matches('\n')
        );
    }

    #[test]
    fn remove_headroom_mcp_json_entries_removes_by_name_and_footprint() {
        // app_data_dir() derives from HOME, and this test reads it once to
        // build the fixture while remove_headroom_mcp_json_entries reads it
        // again to match. Sibling tests repoint HOME at a tempdir, so without
        // the lock the two reads can straddle a flip and disagree (~1 run in 6
        // of the full suite).
        let _env_lock = crate::test_env_lock::lock_home();
        let app_dir = crate::storage::app_data_dir().display().to_string();
        let mut servers = json!({
            "headroom": { "command": "python3", "args": ["mcp", "serve"] },
            "serena": { "command": format!("{app_dir}/serena-venv/bin/serena") },
            "codebase-memory": { "command": [format!("{app_dir}/runtime/bin/codebase-memory-mcp")] },
            "context7": { "command": "npx" },
        });
        let map = servers.as_object_mut().unwrap();
        assert!(super::remove_headroom_mcp_json_entries(map));
        assert!(map.get("headroom").is_none());
        assert!(map.get("serena").is_none());
        assert!(map.get("codebase-memory").is_none());
        assert!(map.get("context7").is_some());

        let mut untouched = json!({ "context7": { "command": "npx" } });
        assert!(!super::remove_headroom_mcp_json_entries(
            untouched.as_object_mut().unwrap()
        ));
    }

    #[test]
    fn is_permission_denied_matches_only_permission_errors() {
        // Construct by ErrorKind, not raw errno: 13 is EACCES on Unix but
        // ERROR_INVALID_DATA on Windows, where it does not map to
        // PermissionDenied.
        let denied = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ))
        .context("writing /Users/x/.zshrc");
        assert!(is_permission_denied(&denied));

        let not_found = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ))
        .context("writing /Users/x/.zshrc");
        assert!(!is_permission_denied(&not_found));

        assert!(!is_permission_denied(&anyhow::anyhow!("Permission denied")));
    }

    /// RUST-D2: `atomic_write` bakes the io cause into its message and carries
    /// no source, so the classifiers must read the "(os error N)" text too.
    #[test]
    fn environment_classifiers_read_atomic_write_message_text() {
        let denied_code = *PERMISSION_DENIED_OS_ERRORS.last().unwrap();
        let denied = anyhow::anyhow!(
            "writing ~/.bash_profile.tmp.9156.2330: {}",
            std::io::Error::from_raw_os_error(denied_code)
        )
        .context("client setup failed for codex");
        assert!(is_permission_denied(&denied), "{denied:#}");
        assert!(!is_no_space(&denied));

        let full = anyhow::anyhow!(
            "writing ~/.zshrc.tmp.1.2: {}",
            std::io::Error::from_raw_os_error(NO_SPACE_OS_ERRORS[0])
        );
        assert!(is_no_space(&full), "{full:#}");
        assert!(!is_permission_denied(&full));

        // A code in prose that is not the io Display suffix stays unclassified.
        assert!(!is_permission_denied(&anyhow::anyhow!(
            "os error 5 happened"
        )));
        assert!(!is_no_space(&anyhow::anyhow!("exit code 28")));
    }

    #[test]
    fn is_no_space_matches_only_disk_full_codes() {
        for &code in NO_SPACE_OS_ERRORS {
            let full = anyhow::Error::new(std::io::Error::from_raw_os_error(code))
                .context("creating backup /Users/x/.claude/settings.json.headroom-backup");
            assert!(is_no_space(&full));
        }

        let denied = anyhow::Error::new(std::io::Error::from_raw_os_error(13))
            .context("writing /Users/x/.zshrc");
        assert!(!is_no_space(&denied));

        assert!(!is_no_space(&anyhow::anyhow!("No space left on device")));
    }

    #[test]
    fn client_setup_state_survives_schema_drift_in_either_direction() {
        // A newer build's extra field must be ignored, and a build that
        // drops/renames configured_clients must still yield the rest. A parse
        // failure here returns the empty default, which reads as "nothing
        // configured": the tray reports Claude Code disconnected and uninstall
        // can no longer find the shell blocks listed in managedShellFiles.
        let newer = r#"{"configuredClients":{"claude_code":"2026-03-27T10:00:00Z"},
            "managedShellFiles":{"claude_code":["/Users/test/.zshrc"]},
            "someFutureFlag":42}"#;
        let state: ClientSetupState = serde_json::from_str(newer).unwrap();
        assert!(state.configured_clients.contains_key("claude_code"));
        assert!(state.managed_shell_files.contains_key("claude_code"));

        let dropped =
            r#"{"managedShellFiles":{"codex_cli":["/Users/test/.zshrc"]},"rtkDisabled":true}"#;
        let state: ClientSetupState = serde_json::from_str(dropped).unwrap();
        assert!(state.managed_shell_files.contains_key("codex_cli"));
        assert!(state.rtk_disabled);
    }

    #[cfg(unix)]
    #[test]
    fn upstream_auth_token_makes_claude_settings_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _home = TestHome::new();
        let path = super::claude_settings_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        super::apply_upstream_auth_token(Some("sk-provider"), None, &mut BTreeMap::new())
            .expect("apply");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("sk-provider"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A later rewrite (clearing it) must not widen it again.
        super::apply_upstream_auth_token(None, Some("sk-provider"), &mut BTreeMap::new())
            .expect("clear");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// RUST-5T: both load attempts failed in `read` (the machine was out of
    /// file descriptors), and the old code quarantined on that -- renaming the
    /// user's real setup away and handing every caller the empty default. An
    /// unreadable file must be left exactly where it is.
    #[test]
    fn an_unreadable_setup_state_is_never_quarantined() {
        let _home = TestHome::new();
        let mut state = super::ClientSetupState::default();
        state
            .configured_clients
            .insert("claude_code".into(), "2026-01-01T00:00:00+00:00".into());
        super::write_setup_state(&state).expect("write");
        let path = super::setup_state_path();
        let original = std::fs::read(&path).expect("read back");

        // Unreadable, not unparsable: a directory where the file is expected
        // makes every `fs::read` fail without saying anything about contents,
        // which is the same class of evidence as ENFILE.
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("dir in its place");
        assert!(super::try_load_setup_state(&path).is_err_and(|e| e.is_io()));
        assert!(
            super::load_setup_state().configured_clients.is_empty(),
            "callers still get the default"
        );
        assert!(
            !path.with_extension("json.corrupt").exists(),
            "an unreadable file must not be moved aside"
        );

        // A genuinely unparsable file still is: the bytes were read and are bad.
        std::fs::remove_dir(&path).expect("undo");
        std::fs::write(&path, b"{ truncated").expect("corrupt it");
        assert!(super::load_setup_state().configured_clients.is_empty());
        let corrupt = path.with_extension("json.corrupt");
        assert!(corrupt.exists(), "unparsable bytes are quarantined");
        assert_eq!(std::fs::read(&corrupt).unwrap(), b"{ truncated");

        // And the healthy path is untouched.
        std::fs::write(&path, &original).expect("restore");
        assert!(super::load_setup_state()
            .configured_clients
            .contains_key("claude_code"));
    }

    #[test]
    fn quarantine_unparsable_moves_the_file_aside_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-setup.json");
        std::fs::write(&path, b"{ truncated").unwrap();

        super::quarantine_unparsable(&path, "test");
        assert!(
            !path.exists(),
            "original is moved, not left to be overwritten"
        );
        let corrupt = dir.path().join("client-setup.json.corrupt");
        assert_eq!(std::fs::read(&corrupt).unwrap(), b"{ truncated");

        // Repeat failures reuse the one slot instead of accumulating files.
        std::fs::write(&path, b"{ again").unwrap();
        super::quarantine_unparsable(&path, "test");
        assert_eq!(std::fs::read(&corrupt).unwrap(), b"{ again");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        // Missing file is a no-op, not an error.
        super::quarantine_unparsable(&dir.path().join("absent.json"), "test");
    }

    #[test]
    fn normalize_setup_state_keeps_codex_but_drops_legacy_codex_gui() {
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
                ("codex_cli".into(), vec!["/Users/test/.zshrc".into()]),
                ("codex_gui".into(), vec!["/Users/test/.zshrc".into()]),
            ]),
            remembered_shell_files: BTreeMap::from([
                ("codex".into(), vec!["/Users/test/.bash_profile".into()]),
                ("claude_code".into(), vec!["/Users/test/.bashrc".into()]),
            ]),
            preserved_base_urls: BTreeMap::new(),
            rtk_disabled: false,
            auto_learn_disabled: false,
            statusline_disabled: false,
            usage_data_disabled: false,
            setup_versions: BTreeMap::new(),
        };

        let normalized = normalize_setup_state(state);

        // codex_cli stays configured; only the removed codex_gui id is stripped.
        assert!(normalized.configured_clients.contains_key("claude_code"));
        assert!(normalized.configured_clients.contains_key("codex_cli"));
        assert!(!normalized.configured_clients.contains_key("codex_gui"));

        assert!(normalized.remembered_clients.contains_key("claude_code"));
        assert!(normalized.remembered_clients.contains_key("codex"));

        assert!(normalized.managed_shell_files.contains_key("claude_code"));
        assert!(normalized.managed_shell_files.contains_key("codex_cli"));
        assert!(!normalized.managed_shell_files.contains_key("codex_gui"));

        assert!(normalized
            .remembered_shell_files
            .contains_key("claude_code"));
        assert!(normalized.remembered_shell_files.contains_key("codex"));
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
    fn unparseable_vscode_settings_stay_out_of_sentry() {
        // RUST-M2: the user's file, missing a comma; VS Code applies it anyway.
        let raw = "{\n    \"chat.viewSessions.orientation\": \"stacked\"\n    \"a\": 1\n}\n";
        let err = parse_json_object(raw, Path::new("settings.json")).expect_err("missing comma");
        assert_eq!(vscode_settings_failure_level(&err), log::Level::Info);

        let io = anyhow::Error::from(std::io::Error::other("denied")).context("writing settings");
        assert_eq!(vscode_settings_failure_level(&io), log::Level::Warn);

        // RUST-N7/N8: macOS refused the read with EPERM.
        let eperm = anyhow::Error::from(std::io::Error::from_raw_os_error(
            super::PERMISSION_DENIED_OS_ERRORS[0],
        ))
        .context("reading settings.json");
        assert_eq!(vscode_settings_failure_level(&eperm), log::Level::Info);
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
    #[cfg(unix)]
    fn hook_rules_read_windows_sources_only_when_asked() {
        // The rules reader under a stand-in win32 (hook tests only run on unix
        // CI): the rtk verdict still gets None there, the MarkItDown hook reads
        // the files, and either policy key or Program Files managed settings
        // makes it None again (the sources Claude Code 2.1.284 reads).
        let root = unique_temp_dir("headroom-hook-rules-win");
        let (conf, project, pf) = (root.join("conf"), root.join("project"), root.join("pf"));
        for dir in [&conf, &project, &pf] {
            fs::create_dir_all(dir).expect("dirs");
        }
        let check = r#"
import sys, types
keys = set(sys.argv[1].split(",")) - {""}
winreg = types.ModuleType("winreg")
winreg.HKEY_LOCAL_MACHINE, winreg.HKEY_CURRENT_USER = "HKLM", "HKCU"
def open_key(hive, path):
    if hive in keys and path == "SOFTWARE\\Policies\\ClaudeCode":
        return hive
    raise FileNotFoundError(path)
winreg.OpenKey, winreg.CloseKey = open_key, lambda key: None
sys.modules["winreg"] = winreg
sys.platform = "win32"
data = {"cwd": sys.argv[2]}
print(settings(data) is None, settings(data, windows=True) is None)
"#;
        let run = |keys: &str| {
            let output = crate::proc::command("/usr/bin/python3")
                .arg("-c")
                .arg(format!("{}\n{check}", super::HOOK_RULES_PY))
                .arg(keys)
                .arg(&project)
                .env("CLAUDE_CONFIG_DIR", &conf)
                .env("ProgramFiles", &pf)
                .env_remove("CLAUDE_PROJECT_DIR")
                .env_remove("CLAUDE_CODE_MANAGED_SETTINGS_PATH")
                .env_remove("CLAUDE_CODE_REMOTE_SETTINGS_PATH")
                .output()
                .expect("python");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
                + &String::from_utf8_lossy(&output.stderr)
        };
        assert_eq!(run(""), "True False");
        assert_eq!(run("HKLM"), "True True");
        assert_eq!(run("HKCU"), "True True");
        fs::create_dir_all(pf.join("ClaudeCode")).expect("managed dir");
        fs::write(pf.join("ClaudeCode").join("managed-settings.json"), "{}").expect("managed");
        assert_eq!(run(""), "True True");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn generated_markitdown_hook_escapes_paths_and_redirects_read() {
        let hook = build_headroom_markitdown_hook(
            Path::new("/tmp/head room/venv/bin/markitdown"),
            Path::new("/tmp/head room/venv/bin/$python"),
        );

        assert!(hook.contains("HEADROOM_MARKITDOWN=\"/tmp/head room/venv/bin/markitdown\""));
        assert!(hook.contains("HEADROOM_PYTHON=\"/tmp/head room/venv/bin/\\$python\""));
        // Scoped to PDF only (Office is handled by the nudge, not the hook),
        // redirects via updatedInput, and fails open.
        assert!(hook.contains("ALLOWED = {\".pdf\"}"));
        assert!(!hook.contains(".docx"));
        assert!(hook.contains("updated[\"file_path\"] = out"));
        assert!(hook.contains("\"updatedInput\": updated"));
        assert!(hook.contains("Headroom MarkItDown conversion"));
        assert!(hook.contains("sys.exit(0)"));
    }

    #[test]
    #[cfg(unix)]
    fn markitdown_hook_caches_in_a_private_home_dir_and_replaces_planted_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        // The cache used to live in a shared `${TMPDIR:-/tmp}` dir another
        // local user could pre-create and fill with symlinks the conversion
        // wrote through (e.g. into ~/.ssh/authorized_keys).
        let root = unique_temp_dir("headroom-md-hook");
        let home = root.join("home");
        fs::create_dir_all(&home).expect("home");
        // Only checked for; the hook converts through the python's markitdown.
        let fake_md = root.join("markitdown");
        fs::write(&fake_md, "#!/bin/sh\n").expect("fake md");
        fs::set_permissions(&fake_md, fs::Permissions::from_mode(0o755)).expect("chmod");
        let pythonpath = super::fake_markitdown_pythonpath(&root);
        let pdf = root.join("doc.pdf");
        fs::write(&pdf, "%PDF-1.4").expect("pdf");
        let hook_path = root.join("hook.sh");
        fs::write(
            &hook_path,
            build_headroom_markitdown_hook(&fake_md, Path::new("/usr/bin/python3")),
        )
        .expect("hook");

        let run_for = |pdf: &Path| {
            let output = crate::proc::command("bash")
                .arg(&hook_path)
                .env("HOME", &home)
                .env("PYTHONPATH", &pythonpath)
                .env_remove("XDG_CACHE_HOME")
                .env_remove("CLAUDE_PROJECT_DIR")
                .env_remove("CLAUDE_CONFIG_DIR")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    let input =
                        json!({ "cwd": root, "tool_input": { "file_path": pdf } }).to_string();
                    child
                        .stdin
                        .as_mut()
                        .unwrap()
                        .write_all(input.as_bytes())
                        .unwrap();
                    child.wait_with_output()
                })
                .expect("run hook");
            assert!(output.status.success());
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            serde_json::from_str::<Value>(&stdout).ok().and_then(|v| {
                v["hookSpecificOutput"]["updatedInput"]["file_path"]
                    .as_str()
                    .map(PathBuf::from)
            })
        };
        let run = || run_for(&pdf);

        // The allow skips Claude Code's prompt for reads outside the working
        // directories, so a PDF outside the session's cwd is left to it.
        let outside = unique_temp_dir("headroom-md-hook-outside");
        fs::create_dir_all(&outside).expect("outside dir");
        let outside = outside.join("other.pdf");
        fs::write(&outside, "%PDF-1.4").expect("outside pdf");
        assert_eq!(run_for(&outside), None);

        // A week-old entry goes on the next conversion: nothing else ever
        // deletes these copies of the user's documents.
        let cache = home.join(".cache").join("headroom-markitdown");
        fs::create_dir_all(&cache).expect("cache");
        let stale = cache.join("stale.md");
        fs::write(&stale, "old contract").expect("stale");
        fs::File::options()
            .write(true)
            .open(&stale)
            .and_then(|f| f.set_modified(SystemTime::now() - super::Duration::from_secs(8 * 86400)))
            .expect("age stale");

        let out = run().expect("hook should redirect the read");
        assert_eq!(out.parent(), Some(cache.as_path()));
        assert!(!stale.exists(), "week-old cache entry survived");
        // Audio transcription (an unprompted upload) stays off.
        assert_eq!(fs::read_to_string(&out).unwrap(), "converted");
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // A symlink planted at the cache target is replaced, never followed.
        let victim = root.join("authorized_keys");
        fs::write(&victim, "ssh-ed25519 original").expect("victim");
        fs::remove_file(&out).unwrap();
        std::os::unix::fs::symlink(&victim, &out).unwrap();
        assert_eq!(run().as_deref(), Some(out.as_path()));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "ssh-ed25519 original");
        assert!(!fs::symlink_metadata(&out).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&out).unwrap(), "converted");

        // Rules match the redirected cache path, so a Read rule anywhere
        // Claude Code loads it (or a source this hook cannot read) keeps the
        // read native. Other rules do not.
        let user_settings = home.join(".claude").join("settings.json");
        let project_settings = root.join(".claude").join("settings.local.json");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        for (path, body) in [
            (
                &user_settings,
                r#"{"permissions":{"deny":["Read(./secret/**)"]}}"#,
            ),
            (&project_settings, r#"{"permissions":{"ask":["Read"]}}"#),
            (&project_settings, "{not json"),
        ] {
            fs::write(path, body).unwrap();
            assert_eq!(run(), None, "{body} in {}", path.display());
            fs::remove_file(path).unwrap();
        }
        fs::write(&user_settings, r#"{"permissions":{"deny":["Bash(rm:*)"]}}"#).unwrap();
        assert_eq!(run().as_deref(), Some(out.as_path()));

        // A cache dir that is itself a symlink is refused outright.
        fs::remove_dir_all(&cache).unwrap();
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &cache).unwrap();
        assert_eq!(run(), None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refresh_markitdown_integration_rewrites_only_an_installed_hook() {
        // Hook fixes reach existing installs only if launch rewrites the
        // installed body; a disabled integration (no hook, no nudge, no rule)
        // stays off.
        let home = TestHome::new();
        let shim = home
            .path()
            .join(".headroom")
            .join("bin")
            .join("headroom-markitdown");
        let (md, py) = (
            Path::new("/h/venv/bin/markitdown"),
            Path::new("/h/venv/bin/python3"),
        );
        // Stable ran the shim from bin/ (on PATH), rc.2-rc.3 from tools/; both
        // under Application Support, whose space no Bash rule ever matched.
        let legacy = [
            PathBuf::from("/h/App Support/bin/markitdown"),
            PathBuf::from("/h/App Support/tools/markitdown"),
        ];
        let settings_path = super::claude_settings_path();
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            r#"{"permissions":{"allow":["Bash(ls *)"]}}"#,
        )
        .unwrap();
        let hook = super::headroom_markitdown_hook_path();
        super::refresh_markitdown_integration(md, &shim, &legacy, py).expect("refresh");
        assert!(!hook.exists());
        assert!(!super::markitdown_claude_md_path().exists());
        assert_eq!(
            fs::read_to_string(&settings_path).unwrap(),
            r#"{"permissions":{"allow":["Bash(ls *)"]}}"#
        );
        let rules = || -> Vec<String> {
            let settings = fs::read_to_string(&settings_path).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&settings).unwrap();
            parsed["permissions"]["allow"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        // A disable that raced the first launch left a legacy rule behind but
        // took the hook: the rule goes, and moves nowhere.
        super::set_markitdown_bash_permission(&legacy[0], &[], |_| Some(true)).unwrap();
        super::refresh_markitdown_integration(md, &shim, &legacy, py).expect("refresh");
        assert_eq!(rules(), ["Bash(ls *)"]);

        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(
            &hook,
            "HEADROOM_MD_CACHE=\"${TMPDIR:-/tmp}/headroom-markitdown\"\n",
        )
        .unwrap();
        let (claude_md, agents) = (
            super::markitdown_claude_md_path(),
            super::markitdown_codex_agents_path(),
        );
        upsert_managed_block(
            &claude_md,
            "markitdown_office",
            &build_markitdown_office_nudge(&legacy[1]),
        )
        .unwrap();
        upsert_managed_block(
            &agents,
            "markitdown",
            &build_markitdown_codex_nudge(&legacy[1]),
        )
        .unwrap();
        for old in &legacy {
            super::set_markitdown_bash_permission(old, &[], |_| Some(true)).unwrap();
        }
        let pre_migration = fs::read_to_string(&settings_path).unwrap();
        // The writes above share the stamp second, whose first backup is kept.
        for entry in fs::read_dir(settings_path.parent().unwrap())
            .unwrap()
            .flatten()
        {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("settings.json.headroom-backup-")
            {
                fs::remove_file(entry.path()).unwrap();
            }
        }

        super::refresh_markitdown_integration(md, &shim, &legacy, py).expect("refresh");
        // One write, so its backup is the settings from before the move (two
        // writes a second apart kept only the half-migrated copy).
        let settings_dir = settings_path.parent().unwrap();
        let newest_backup = fs::read_dir(settings_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("settings.json.headroom-backup-"))
            })
            .max()
            .expect("backup");
        assert_eq!(fs::read_to_string(newest_backup).unwrap(), pre_migration);
        assert_eq!(
            fs::read_to_string(&hook).unwrap(),
            build_headroom_markitdown_hook(md, py)
        );
        let new_path = shim.display().to_string();
        for file in [&claude_md, &agents] {
            let body = fs::read_to_string(file).unwrap();
            assert!(
                body.contains(&format!("`{new_path} <path>`")) && !body.contains("/h/"),
                "{body}"
            );
        }
        let mut expected = vec!["Bash(ls *)".to_string()];
        if !cfg!(windows) {
            expected.push(format!("Bash({new_path} *)"));
        }
        assert_eq!(rules(), expected);

        // One launch converges; the next changes nothing.
        let snapshot = |paths: &[&PathBuf]| -> Vec<String> {
            paths
                .iter()
                .map(|p| fs::read_to_string(p).unwrap())
                .collect()
        };
        let before = snapshot(&[&settings_path, &claude_md, &agents, &hook]);
        super::refresh_markitdown_integration(md, &shim, &legacy, py).expect("refresh");
        assert_eq!(
            snapshot(&[&settings_path, &claude_md, &agents, &hook]),
            before
        );
    }

    #[test]
    fn markitdown_shim_path_with_whitespace_gets_a_quoted_nudge_and_no_rule() {
        // A home dir with a space: no rule form matches, so none is written,
        // and the nudge quotes the path so the shell does not split it.
        let _home = TestHome::new();
        let (md, py) = (
            Path::new("/h/venv/bin/markitdown"),
            Path::new("/h/venv/bin/python3"),
        );
        let shim = Path::new("/Users/Jane Doe/.headroom/bin/headroom-markitdown");
        let legacy = [PathBuf::from("/h/tools/markitdown")];
        super::set_markitdown_bash_permission(&legacy[0], &[], |_| Some(true)).unwrap();
        super::set_markitdown_bash_permission(shim, &[], |_| Some(true)).unwrap();
        super::refresh_markitdown_integration(md, shim, &legacy, py).expect("refresh");
        let settings = fs::read_to_string(super::claude_settings_path()).unwrap();
        assert!(!settings.contains("markitdown"), "{settings}");
        assert!(build_markitdown_office_nudge(shim)
            .contains("`'/Users/Jane Doe/.headroom/bin/headroom-markitdown' <path>`"));
    }

    #[test]
    fn rtk_codex_nudge_quotes_a_path_with_a_space() {
        // macOS keeps rtk under "Application Support"; unquoted, the shell
        // split the example at the space and the command exited 127.
        let nudge = super::build_rtk_codex_nudge(Path::new(
            "/Users/u/Library/Application Support/Headroom/headroom/bin/rtk",
        ));
        assert!(
            nudge.contains(
                "`'/Users/u/Library/Application Support/Headroom/headroom/bin/rtk' git status`"
            ),
            "{nudge}"
        );
    }

    #[test]
    fn disabling_markitdown_deletes_the_conversion_cache() {
        // Each cached file is a whole converted document.
        let home = TestHome::new();
        let cache = home.path().join(".cache").join("headroom-markitdown");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("0123456789abcdef.md"), "contract text").unwrap();
        super::disable_markitdown_integration(Path::new("/h/tools/markitdown")).expect("disable");
        assert!(!cache.exists());
    }

    #[test]
    fn disabling_markitdown_marker_leaves_rtk_hook_intact() {
        let root = unique_temp_dir("headroom-strip-markitdown");
        fs::create_dir_all(&root).expect("create root");
        let settings = root.join("settings.json");
        fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "Bash", "hooks": [{ "type": "command", "command": "/h/headroom-rtk-rewrite.sh" }] },
                        { "matcher": "Read", "hooks": [{ "type": "command", "command": "/h/headroom-markitdown-read.sh" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .expect("write settings");

        let changed = remove_pre_tool_use_markers(&settings, &["headroom-markitdown-read.sh"])
            .expect("strip");
        assert!(changed);

        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        let entries = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entry_contains_hook(&entries[0], "headroom-rtk-rewrite.sh"));
    }

    #[test]
    fn markitdown_office_nudge_points_at_the_shim_and_skips_pdf() {
        let nudge = build_markitdown_office_nudge(Path::new("/h/bin/markitdown"));
        assert!(nudge.contains("/h/bin/markitdown <path>"));
        assert!(nudge.contains(".docx"));
        assert!(nudge.contains("PDFs are handled automatically"));
    }

    #[test]
    fn markitdown_codex_nudge_covers_pdf_and_office() {
        let nudge = build_markitdown_codex_nudge(Path::new("/h/bin/markitdown"));
        assert!(nudge.contains("/h/bin/markitdown <path>"));
        // Codex has no hook, so PDF is covered by the CLI nudge too.
        assert!(nudge.contains(".pdf"));
        assert!(nudge.contains(".docx"));
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

    /// npm installs drop an extensionless bash shim next to the `.cmd`, and
    /// only the `.cmd` is spawnable on Windows. Matching the bare name first
    /// handed every caller (`claude`, `codex`, `npx`) a dead path.
    #[test]
    #[cfg(windows)]
    fn windows_path_lookup_prefers_the_pathext_variant() {
        let home = unique_temp_dir("headroom-path-pathext");
        let bin_dir = home.join("custom-bin");
        fs::create_dir_all(&bin_dir).expect("create custom bin");
        fs::write(bin_dir.join("claude"), "").expect("write npm bash shim");
        fs::write(bin_dir.join("claude.cmd"), "").expect("write npm cmd shim");

        let detected = find_on_path_entries(vec![bin_dir.clone()], &["claude"]).expect("detected");

        // The extension's CASE comes from PATHEXT, which is uppercase on a real
        // Windows box (`.COM;.EXE;.BAT;.CMD`), so the returned path is the one
        // we constructed -- `claude.CMD` -- not the on-disk spelling. It spawns
        // either way because NTFS is case-insensitive; only a byte-compare in a
        // test can tell them apart. Assert what actually matters: the .cmd was
        // picked over the bare shim.
        assert_eq!(detected.parent(), Some(bin_dir.as_path()));
        assert!(
            detected
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("claude.cmd")),
            "{}",
            detected.display()
        );

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

    #[test]
    fn managed_block_upsert_replaces_existing_block_without_duplication() {
        let root = unique_temp_dir("headroom-managed-block");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".zshrc");
        fs::write(&path, "export PATH=/usr/bin\n").expect("write shell file");

        let first = upsert_managed_block(
            &path,
            "claude_code",
            "export ANTHROPIC_BASE_URL=http://127.0.0.1:6767",
        )
        .expect("insert managed block");
        assert!(first.0);
        assert!(first.1.is_some());

        upsert_managed_block(
            &path,
            "claude_code",
            "export ANTHROPIC_BASE_URL=http://127.0.0.1:6767\nexport HEADROOM=1",
        )
        .expect("replace managed block");

        let content = fs::read_to_string(&path).expect("read updated shell file");
        assert_eq!(content.matches("# >>> headroom:claude_code >>>").count(), 1);
        assert!(content.contains("export PATH=/usr/bin"));
        assert!(content.contains("export HEADROOM=1"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_block_upsert_treats_reordered_markers_as_absent() {
        // A stray end-before-start block (leftover from an interrupted write).
        // The old slice-based rewrite duplicated the stray fragments and left a
        // dangling opening marker at the tail; the guarded path appends a fresh,
        // well-formed block instead.
        let root = unique_temp_dir("headroom-reordered-markers");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".zshrc");
        fs::write(
            &path,
            "# <<< headroom:claude_code <<<\nstray old body\n# >>> headroom:claude_code >>>\n",
        )
        .expect("write malformed shell file");

        upsert_managed_block(
            &path,
            "claude_code",
            "export ANTHROPIC_BASE_URL=http://127.0.0.1:6767",
        )
        .expect("upsert over malformed block");

        let content = fs::read_to_string(&path).expect("read updated shell file");
        // Tail must be a well-ordered block: the last opening marker precedes the
        // last closing marker, and the file ends on the closing marker (not on a
        // dangling opener as the buggy slice produced).
        let last_start = content
            .rfind("# >>> headroom:claude_code >>>")
            .expect("start marker present");
        let last_end = content
            .rfind("# <<< headroom:claude_code <<<")
            .expect("end marker present");
        assert!(last_start < last_end, "tail block must be well-ordered");
        assert!(content
            .trim_end()
            .ends_with("# <<< headroom:claude_code <<<"));
        assert!(content.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:6767"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_managed_block_keeps_surrounding_shell_content_intact() {
        let root = unique_temp_dir("headroom-remove-block");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".zprofile");
        fs::write(
            &path,
            "export PATH=/usr/bin\n# >>> headroom:claude_code >>>\nexport ANTHROPIC_BASE_URL=http://127.0.0.1:6767\n# <<< headroom:claude_code <<<\nexport EDITOR=vim\n",
        )
        .expect("write shell file");

        let removed = remove_managed_block(&path, "claude_code").expect("remove managed block");

        assert!(removed);
        assert_eq!(
            fs::read_to_string(&path).expect("read cleaned shell file"),
            "export PATH=/usr/bin\nexport EDITOR=vim\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shell_block_helpers_only_match_content_inside_the_named_block() {
        let root = unique_temp_dir("headroom-shell-match");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".bashrc");
        fs::write(
            &path,
            "export ANTHROPIC_BASE_URL=https://example.com\n# >>> headroom:claude_code >>>\nexport ANTHROPIC_BASE_URL=http://127.0.0.1:6767\nexport PATH=/tmp/headroom:$PATH\n# <<< headroom:claude_code <<<\n",
        )
        .expect("write shell file");

        assert!(shell_block_contains_in_files(
            std::slice::from_ref(&path),
            "claude_code",
            "ANTHROPIC_BASE_URL",
            "http://127.0.0.1:6767",
        )
        .expect("detect managed export"));
        assert!(shell_block_contains_text_in_files(
            std::slice::from_ref(&path),
            "claude_code",
            "export PATH=",
        )
        .expect("detect managed text"));
        assert!(!shell_block_contains_in_files(
            &[path],
            "managed_rtk",
            "ANTHROPIC_BASE_URL",
            "http://127.0.0.1:6767",
        )
        .expect("ignore other block ids"));

        let _ = fs::remove_dir_all(root);
    }

    /// A hand-deleted opener leaves a stray end marker ahead of the block. The
    /// verify helpers sliced `content[start..end]` from two independent finds
    /// and panicked on it, killing the proxy watchdog and tray threads through
    /// rtk_integration_status. Upsert appended a fresh copy on every launch and
    /// remove duplicated the file instead of removing the block.
    #[test]
    fn shell_block_ops_tolerate_a_stray_end_marker_before_the_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".zshrc");
        let content = "# <<< headroom:managed_rtk <<<\n# >>> headroom:managed_rtk >>>\nexport PATH=/x:$PATH\n# <<< headroom:managed_rtk <<<\n";
        fs::write(&path, content).unwrap();
        let targets = std::slice::from_ref(&path);

        assert!(
            shell_block_contains_text_in_files(targets, "managed_rtk", "export PATH=").unwrap()
        );
        assert!(shell_block_contains_in_files(targets, "managed_rtk", "PATH", "/x:$PATH").unwrap());

        let (changed, _) =
            upsert_managed_block(&path, "managed_rtk", "export PATH=/x:$PATH").unwrap();
        assert!(!changed, "an intact block behind a stray end is current");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);

        assert!(remove_managed_block(&path, "managed_rtk").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn write_file_if_changed_skips_backups_when_content_is_unchanged() {
        let root = unique_temp_dir("headroom-write-file");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("headroom-rtk-rewrite.sh");
        fs::write(&path, "#!/bin/sh\necho headroom\n").expect("write hook file");

        let changed = write_file_if_changed(&path, "#!/bin/sh\necho headroom\n", false)
            .expect("skip unchanged write");

        assert_eq!(changed, (false, None));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_block_round_trip_preserves_realistic_zshrc_content() {
        let root = unique_temp_dir("headroom-zshrc-roundtrip");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".zshrc");
        let original = r#"export NVM_DIR="$HOME/.nvm"
[ -s "$NVM_DIR/nvm.sh" ] && \. "$NVM_DIR/nvm.sh"

# pnpm
export PNPM_HOME="/Users/test/Library/pnpm"
case ":$PATH:" in
  *":$PNPM_HOME:"*) ;;
  *) export PATH="$PNPM_HOME:$PATH" ;;
esac

export BUN_INSTALL="$HOME/.bun"
export PATH="$BUN_INSTALL/bin:$PATH"
"#;
        fs::write(&path, original).expect("write zshrc");

        upsert_managed_block(
            &path,
            "managed_rtk",
            "export PATH=\"/tmp/headroom/bin:$PATH\"",
        )
        .expect("add managed rtk block");
        upsert_managed_block(
            &path,
            "claude_code",
            "export ANTHROPIC_BASE_URL=http://127.0.0.1:6767",
        )
        .expect("add claude block");

        remove_managed_block(&path, "claude_code").expect("remove claude block");
        remove_managed_block(&path, "managed_rtk").expect("remove managed rtk block");

        let final_content = fs::read_to_string(&path).expect("read round-tripped zshrc");
        assert_eq!(final_content, original);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn updating_one_managed_block_does_not_touch_other_blocks_or_user_content() {
        let root = unique_temp_dir("headroom-multi-block-update");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".zprofile");
        let original = r#"eval "$(/opt/homebrew/bin/brew shellenv)"

# >>> headroom:managed_rtk >>>
export PATH="/old/headroom/bin:$PATH"
# <<< headroom:managed_rtk <<<

# >>> headroom:claude_code >>>
export ANTHROPIC_BASE_URL=http://127.0.0.1:6767
# <<< headroom:claude_code <<<

eval "$(/opt/homebrew/bin/rbenv init - zsh)"
"#;
        fs::write(&path, original).expect("write zprofile");

        upsert_managed_block(
            &path,
            "managed_rtk",
            "export PATH=\"/new/headroom/bin:$PATH\"",
        )
        .expect("update managed rtk block");

        let updated = fs::read_to_string(&path).expect("read updated zprofile");
        assert!(updated.contains("eval \"$(/opt/homebrew/bin/brew shellenv)\""));
        assert!(updated.contains("eval \"$(/opt/homebrew/bin/rbenv init - zsh)\""));
        assert!(updated.contains("export PATH=\"/new/headroom/bin:$PATH\""));
        assert!(updated.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:6767"));
        assert_eq!(updated.matches("# >>> headroom:managed_rtk >>>").count(), 1);
        assert_eq!(updated.matches("# >>> headroom:claude_code >>>").count(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removing_one_managed_block_leaves_other_managed_blocks_and_user_content() {
        let root = unique_temp_dir("headroom-remove-single-block");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join(".zshrc");
        fs::write(
            &path,
            r#"export NVM_DIR="$HOME/.nvm"
[ -s "$NVM_DIR/nvm.sh" ] && \. "$NVM_DIR/nvm.sh"

# >>> headroom:managed_rtk >>>
export PATH="/tmp/headroom/bin:$PATH"
# <<< headroom:managed_rtk <<<

# >>> headroom:claude_code >>>
export ANTHROPIC_BASE_URL=http://127.0.0.1:6767
# <<< headroom:claude_code <<<
"#,
        )
        .expect("write zshrc");

        remove_managed_block(&path, "claude_code").expect("remove claude block");

        let updated = fs::read_to_string(&path).expect("read cleaned zshrc");
        assert!(updated.contains("export NVM_DIR=\"$HOME/.nvm\""));
        assert!(updated.contains("[ -s \"$NVM_DIR/nvm.sh\" ] && \\. \"$NVM_DIR/nvm.sh\""));
        assert!(updated.contains("# >>> headroom:managed_rtk >>>"));
        assert!(updated.contains("export PATH=\"/tmp/headroom/bin:$PATH\""));
        assert!(!updated.contains("# >>> headroom:claude_code >>>"));

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn managed_rtk_path_export_is_idempotent_across_profile_and_rc() {
        // The block lands in both .zprofile and .zshrc, so a login zsh sourced
        // it twice and carried the bin dir twice. A stale unconditional block
        // is rewritten on the next apply, and verification still finds it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("Head room $x").join("bin");
        let rc = tmp.path().join(".zshrc");
        let dir = bin.to_string_lossy().into_owned();
        let stale = format!(
            "# >>> headroom:managed_rtk >>>\nexport PATH=\"{}:$PATH\"\n# <<< headroom:managed_rtk <<<\n",
            super::shell_double_quote(&dir)
        );
        fs::write(&rc, stale).unwrap();

        let (changed, _) =
            super::ensure_managed_rtk_on_path(&bin.join("rtk"), std::slice::from_ref(&rc)).unwrap();
        assert_eq!(changed.len(), 1, "stale block was not rewritten");
        assert!(super::shell_block_contains_text_in_files(
            std::slice::from_ref(&rc),
            "managed_rtk",
            "export PATH="
        )
        .unwrap());

        // A nested macOS login shell inherits PATH and path_helper moves the
        // inherited dir behind /etc/paths, so "on PATH" is not enough: the
        // block must put it back first.
        let reordered = format!("/usr/bin:/bin:{dir}");
        let cases: [(&str, Vec<&str>); 2] = [
            ("/usr/bin:/bin", vec![dir.as_str(), "/usr/bin", "/bin"]),
            (reordered.as_str(), vec![dir.as_str(), "/usr/bin", "/bin"]),
        ];
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !Path::new(shell).exists() {
                continue;
            }
            for (start, expected) in &cases {
                let out = crate::proc::command(shell)
                    .args(["-c", ". \"$1\"; . \"$1\"; printf %s \"$PATH\"", "sh"])
                    .arg(&rc)
                    .env("PATH", start)
                    // zsh -c still reads $ZDOTDIR/.zshenv, and a developer's
                    // one prepends its own PATH entries.
                    .env("ZDOTDIR", tmp.path())
                    .env_remove("BASH_ENV")
                    .output()
                    .unwrap();
                let path = String::from_utf8_lossy(&out.stdout).into_owned();
                let entries: Vec<&str> = path.split(':').collect();
                assert_eq!(
                    &entries,
                    expected,
                    "{shell} from {start}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }

    /// rc11: the block skipped only when the dir was already first, so a
    /// login zsh whose .zshrc prepends another dir (~/.grok/bin) between the
    /// .zprofile block and the .zshrc block carried the Headroom dir twice.
    /// Dedupe-then-prepend: first, exactly once, also in nested login shells.
    #[cfg(unix)]
    #[test]
    fn managed_rtk_dir_is_first_exactly_once_when_rc_prepends_another_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("Head room $x [1]*").join("bin");
        let dir = bin.to_string_lossy().into_owned();
        let profile = tmp.path().join(".zprofile");
        let rc = tmp.path().join(".zshrc");
        fs::write(&rc, "export PATH=\"/grok bin:$PATH\"\n").unwrap();
        super::ensure_managed_rtk_on_path(&bin.join("rtk"), &[profile.clone(), rc.clone()])
            .unwrap();
        let login = ". \"$1\"; . \"$2\"";
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !Path::new(shell).exists() {
                continue;
            }
            // Twice: the second pass is a nested login shell inheriting PATH.
            let out = crate::proc::command(shell)
                .args([
                    "-c",
                    &format!("{login}; {login}; printf %s \"$PATH\""),
                    "sh",
                ])
                .arg(&profile)
                .arg(&rc)
                .env("PATH", format!("/usr/bin:/bin:{dir}:/usr/bin"))
                .env("ZDOTDIR", tmp.path())
                .env_remove("BASH_ENV")
                .output()
                .unwrap();
            let path = String::from_utf8_lossy(&out.stdout).into_owned();
            let entries: Vec<&str> = path.split(':').collect();
            assert_eq!(
                entries,
                [
                    dir.as_str(),
                    "/grok bin",
                    "/grok bin",
                    "/usr/bin",
                    "/bin",
                    "/usr/bin"
                ],
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    #[test]
    fn zdotdir_from_zshenv_parses_common_forms() {
        let cases: [(&str, Option<&str>); 6] = [
            (
                "export ZDOTDIR=\"$HOME/.config/zsh\"\n",
                Some(".config/zsh"),
            ),
            ("ZDOTDIR=~/.config/zsh\n", Some(".config/zsh")),
            (
                "export ZDOTDIR='${HOME}/dotfiles/zsh'\n",
                Some("dotfiles/zsh"),
            ),
            ("# comment\nexport ZDOTDIR=$HOME/z  # trailing\n", Some("z")),
            ("export ZDOTDIR=~\n", Some("")),
            ("# no zdotdir here\nexport FOO=bar\n", None),
        ];
        for (i, (contents, expected_tail)) in cases.into_iter().enumerate() {
            let home = unique_temp_dir(&format!("headroom-zdotdir-{i}"));
            fs::create_dir_all(&home).unwrap();
            fs::write(home.join(".zshenv"), contents).unwrap();
            let got = super::zdotdir_from_zshenv(&home);
            let expected = expected_tail.map(|tail| {
                if tail.is_empty() {
                    home.clone()
                } else {
                    home.join(tail)
                }
            });
            assert_eq!(got, expected, "case {i}");
        }
        // Missing file -> None.
        let empty = unique_temp_dir("headroom-zdotdir-none");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(super::zdotdir_from_zshenv(&empty), None);
    }

    #[test]
    fn expand_env_vars_keeps_non_ascii_text() {
        assert_eq!(
            super::expand_env_vars("/Users/j\u{f6}rg/\u{65e5}\u{672c}/zsh"),
            "/Users/j\u{f6}rg/\u{65e5}\u{672c}/zsh"
        );
    }

    #[test]
    fn zdotdir_unresolved_env_var_falls_back_to_none() {
        // TestHome sets XDG_CONFIG_HOME under this lock, so without it the
        // remove_var below races those tests (~1 run in 6 of the full suite).
        let _env_lock = crate::test_env_lock::lock_home();
        // Reproduces os error 30: `$XDG_CONFIG_HOME` unset under a Finder launch
        // must NOT yield a relative `$XDG_CONFIG_HOME/zsh` path.
        std::env::remove_var("XDG_CONFIG_HOME");
        let home = unique_temp_dir("headroom-zdotdir-unresolved");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".zshenv"),
            "export ZDOTDIR=\"$XDG_CONFIG_HOME/zsh\"\n",
        )
        .unwrap();
        assert_eq!(super::zdotdir_from_zshenv(&home), None);
    }

    /// `ZDOTDIR="$HOME"/.config/zsh` (quote then bare tail) resolved to $HOME
    /// through the line parser, so the managed blocks went into a ~/.zshrc zsh
    /// never reads. zsh itself answers every form correctly.
    #[test]
    #[serial_test::serial]
    fn zsh_dir_asks_zsh_for_a_zdotdir_the_parser_cannot_read() {
        if super::find_on_path(&["zsh"]).is_none() {
            eprintln!("skipping: no zsh on PATH");
            return;
        }
        let home = TestHome::new();
        let zdotdir = home.path().join(".config").join("zsh");
        fs::create_dir_all(&zdotdir).unwrap();
        fs::write(
            home.path().join(".zshenv"),
            "export ZDOTDIR=\"$HOME\"/.config/zsh\n",
        )
        .unwrap();

        assert_eq!(super::zsh_dir(), zdotdir);
        assert_eq!(super::shell_path(".zshrc"), zdotdir.join(".zshrc"));
    }

    #[test]
    fn strip_hook_returns_false_when_file_missing() {
        let root = unique_temp_dir("headroom-strip-missing");
        let settings = root.join("does-not-exist.json");
        let changed = strip_headroom_hook_from_settings(&settings).expect("strip should succeed");
        assert!(!changed, "missing file should report no change");
        assert!(!settings.exists(), "should not create the file");
    }

    #[test]
    fn strip_hook_removes_headroom_entry_and_leaves_other_entries() {
        let root = unique_temp_dir("headroom-strip-mixed");
        fs::create_dir_all(&root).expect("create root");
        let settings = root.join("settings.json");
        let content = json!({
            "env": { "SOME_KEY": "keep-me" },
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            { "type": "command", "command": "/other/tool/script.sh" }
                        ]
                    },
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "/Users/test/.claude/hooks/headroom-rtk-rewrite.sh"
                            }
                        ]
                    }
                ]
            }
        });
        fs::write(&settings, serde_json::to_string_pretty(&content).unwrap())
            .expect("write settings");

        let changed = strip_headroom_hook_from_settings(&settings).expect("strip should succeed");
        assert!(changed, "should report change");

        let raw = fs::read_to_string(&settings).expect("read settings");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse settings");
        let entries = parsed
            .get("hooks")
            .and_then(|v| v.get("PreToolUse"))
            .and_then(|v| v.as_array())
            .expect("PreToolUse preserved");
        assert_eq!(entries.len(), 1, "only the non-headroom entry remains");
        assert!(
            entry_contains_hook(&entries[0], "other/tool/script.sh"),
            "unrelated entry preserved"
        );
        assert_eq!(
            parsed.get("env").and_then(|v| v.get("SOME_KEY")),
            Some(&json!("keep-me")),
            "unrelated top-level keys untouched"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hook_removal_keeps_a_user_hook_sharing_our_matcher_group() {
        // Claude Code's hook editor appends a new Bash hook to the first group
        // with that matcher, which can be ours. Strip, reinstall and guard
        // removal must take only our handler out of the group.
        let _home = TestHome::new();
        let settings = super::claude_settings_path();
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let user = json!({ "type": "command", "command": "/u/block-rm-rf.sh" });
        let seed = |ours: &str| {
            let content = json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [
                { "type": "command", "command": ours }, user.clone()
            ]}]}});
            fs::write(&settings, serde_json::to_string_pretty(&content).unwrap()).unwrap();
        };
        let commands = || -> Vec<String> {
            read_settings_json(&settings)["hooks"]["PreToolUse"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
                .map(|hook| hook["command"].as_str().unwrap_or_default().to_string())
                .collect()
        };

        seed("/h/.claude/hooks/headroom-rtk-rewrite.sh");
        assert!(strip_headroom_hook_from_settings(&settings).unwrap());
        assert_eq!(commands(), ["/u/block-rm-rf.sh"]);

        seed("/old/headroom-rtk-rewrite.sh");
        super::ensure_claude_settings_hook(
            Path::new("/new/headroom-rtk-rewrite.sh"),
            "Bash",
            "headroom-rtk-rewrite.sh",
        )
        .unwrap();
        let after = commands();
        assert!(after.iter().any(|c| c == "/u/block-rm-rf.sh"), "{after:?}");
        assert!(after.iter().all(|c| !c.contains("/old/")), "{after:?}");

        seed("/h/.claude/hooks/headroom-guard.py");
        super::remove_guard_hook_entries(&settings, "headroom-guard.py", false, None).unwrap();
        assert_eq!(commands(), ["/u/block-rm-rf.sh"]);
    }

    #[test]
    fn backup_keeps_the_pre_burst_copy_and_outlives_nommer_backups() {
        // One apply writes settings.json several times within a second; the
        // backup must hold the user's original, not the next-to-last rewrite.
        // Backups from the app's old name sort after ours by path, and used to
        // get each new backup pruned the moment it was made.
        let dir = tempfile::tempdir().unwrap();
        for stamp in ["20250101000000", "20250102000000", "20250103000000"] {
            let old = dir
                .path()
                .join(format!("settings.json.nommer-backup-{stamp}"));
            fs::write(old, "{}").unwrap();
        }
        let settings = dir.path().join("settings.json");
        fs::write(&settings, "{ // mine\n}").unwrap();
        // Both calls must land in one stamp second.
        while chrono::Utc::now().timestamp_subsec_millis() > 500 {
            std::thread::yield_now();
        }
        let first = super::backup_if_exists(&settings).unwrap().unwrap();
        fs::write(&settings, "{}").unwrap();
        let second = super::backup_if_exists(&settings).unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read_to_string(&first).unwrap(), "{ // mine\n}");
    }

    #[test]
    fn strip_hook_drops_empty_pre_tool_use_and_hooks_keys() {
        let root = unique_temp_dir("headroom-strip-empty");
        fs::create_dir_all(&root).expect("create root");
        let settings = root.join("settings.json");
        let content = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "/path/to/headroom-rtk-rewrite.sh"
                            }
                        ]
                    }
                ]
            }
        });
        fs::write(&settings, serde_json::to_string_pretty(&content).unwrap())
            .expect("write settings");

        let changed = strip_headroom_hook_from_settings(&settings).expect("strip should succeed");
        assert!(changed);

        let raw = fs::read_to_string(&settings).expect("read settings");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse settings");
        assert!(
            parsed.get("hooks").is_none(),
            "empty hooks object should be removed, got {parsed}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn strip_hook_leaves_file_untouched_when_no_headroom_entry_present() {
        let root = unique_temp_dir("headroom-strip-noop");
        fs::create_dir_all(&root).expect("create root");
        let settings = root.join("settings.json");
        let original = serde_json::to_string_pretty(&json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            { "type": "command", "command": "/unrelated.sh" }
                        ]
                    }
                ]
            }
        }))
        .unwrap();
        fs::write(&settings, &original).expect("write settings");

        let changed = strip_headroom_hook_from_settings(&settings).expect("strip should succeed");
        assert!(!changed, "should report no change");

        let after = fs::read_to_string(&settings).expect("read settings");
        assert_eq!(after, original, "file should be byte-identical");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn strip_hook_tolerates_empty_file() {
        let root = unique_temp_dir("headroom-strip-empty-file");
        fs::create_dir_all(&root).expect("create root");
        let settings = root.join("settings.json");
        fs::write(&settings, "").expect("write empty file");

        let changed = strip_headroom_hook_from_settings(&settings).expect("strip should succeed");
        assert!(!changed, "empty file should report no change");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_falls_through_when_rewritten_first_token_missing_from_path() {
        // The hook has an OR guard that exits 0 when the binaries are missing,
        // so we give it real paths and verify the PATH-resolution check kicks in
        // when `rtk rewrite` produces a command whose first token can't be
        // resolved. That's the regression-prone slice added this session.
        let root = unique_temp_dir("headroom-hook-bash");
        fs::create_dir_all(&root).expect("create root");

        // Fake rtk that always prepends a made-up binary name that won't be on PATH.
        let fake_rtk = root.join("fake-rtk");
        fs::write(
            &fake_rtk,
            "#!/usr/bin/env bash\nshift  # drop the 'rewrite' arg\necho \"__headroom_nonexistent_binary_xyzzy__ $*\"\n",
        )
        .expect("write fake rtk");
        fs::set_permissions(
            &fake_rtk,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod rtk");

        // Use the real system python3 so the embedded Python snippets run.
        let system_python = PathBuf::from("/usr/bin/python3");
        assert!(system_python.exists(), "this test assumes /usr/bin/python3");

        let hook_body = build_headroom_rtk_hook(&fake_rtk, &system_python);
        let hook_path = root.join("hook.sh");
        fs::write(&hook_path, &hook_body).expect("write hook");
        fs::set_permissions(
            &hook_path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod hook");

        // Hook expects a JSON object on stdin with tool_input.command.
        let stdin = r#"{"tool_input":{"command":"git status"}}"#;
        let output = crate::proc::command("bash")
            .arg(&hook_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(stdin.as_bytes())
                    .unwrap();
                child.wait_with_output()
            })
            .expect("run hook");

        assert!(output.status.success(), "hook should exit 0");
        assert!(
            output.stdout.is_empty(),
            "hook should emit no rewrite when first token isn't resolvable, got: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn hooks_never_import_a_projects_own_modules() {
        use std::os::unix::fs::PermissionsExt;
        // Hooks run in the project dir, before any permission decision, and
        // `python -c` puts that dir first on sys.path: a cloned repo's
        // `json.py` ran on every Bash call and PDF Read.
        let root = unique_temp_dir("headroom-hook-cwd");
        let (home, project) = (root.join("home"), root.join("project"));
        fs::create_dir_all(project.join("markitdown")).expect("project");
        fs::create_dir_all(&home).expect("home");
        let marker = |name: &str| format!("open({:?}, 'w').close()\n", root.join(name));
        fs::write(project.join("json.py"), marker("ran-json")).expect("json.py");
        fs::write(
            project.join("markitdown").join("__init__.py"),
            marker("ran-markitdown"),
        )
        .expect("markitdown");
        let pdf = project.join("doc.pdf");
        fs::write(&pdf, "%PDF-1.4").expect("pdf");
        let exe = |name: &str, body: &str| {
            let path = root.join(name);
            fs::write(&path, body).expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        };
        let rtk = exe("rtk", "#!/usr/bin/env bash\nshift\necho \"/bin/echo $*\"\n");
        let md = exe("markitdown", "#!/bin/sh\n");
        let python = Path::new("/usr/bin/python3");
        let run = |hook: String, input: Value| {
            let hook_path = exe("hook.sh", &hook);
            let mut child = crate::proc::command("bash")
                .arg(&hook_path)
                .current_dir(&project)
                .env("HOME", &home)
                // The leading empty entry (`$PYTHONPATH:/x` with it unset)
                // puts the cwd on sys.path as an absolute path, not as "".
                .env(
                    "PYTHONPATH",
                    format!(":{}", super::fake_markitdown_pythonpath(&root).display()),
                )
                .env_remove("XDG_CACHE_HOME")
                .env_remove("CLAUDE_PROJECT_DIR")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn hook");
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.to_string().as_bytes())
                .unwrap();
            String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).to_string()
        };

        // Both got as far as their verdict, so every `python -c` ran.
        let out = run(
            build_headroom_rtk_hook(&rtk, python),
            json!({ "tool_input": { "command": "git status" } }),
        );
        assert!(out.contains("\"allow\""), "{out}");
        let out = run(
            build_headroom_markitdown_hook(&md, python),
            json!({ "cwd": project, "tool_input": { "file_path": pdf } }),
        );
        assert!(out.contains("updatedInput"), "{out}");
        for name in ["ran-json", "ran-markitdown"] {
            assert!(
                !root.join(name).exists(),
                "the project's module ran: {name}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_passes_through_check_commands() {
        // `rtk git diff --check` swallows the whitespace report; the hook must
        // leave any --check command unrewritten even when rtk would rewrite it.
        let root = unique_temp_dir("headroom-hook-check");
        fs::create_dir_all(&root).expect("create root");

        let fake_rtk = root.join("fake-rtk");
        fs::write(
            &fake_rtk,
            "#!/usr/bin/env bash\nshift\necho \"/bin/echo $*\"\n",
        )
        .expect("write fake rtk");
        fs::set_permissions(
            &fake_rtk,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod rtk");

        let system_python = PathBuf::from("/usr/bin/python3");
        let hook_body = build_headroom_rtk_hook(&fake_rtk, &system_python);
        let hook_path = root.join("hook.sh");
        fs::write(&hook_path, &hook_body).expect("write hook");
        fs::set_permissions(
            &hook_path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod hook");

        for cmd in ["git diff --cached --check", "git diff --check"] {
            let stdin = format!(r#"{{"tool_input":{{"command":"{cmd}"}}}}"#);
            let output = crate::proc::command("bash")
                .arg(&hook_path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child
                        .stdin
                        .as_mut()
                        .unwrap()
                        .write_all(stdin.as_bytes())
                        .unwrap();
                    child.wait_with_output()
                })
                .expect("run hook");
            assert!(output.status.success(), "hook should exit 0 for {cmd}");
            assert!(
                output.stdout.is_empty(),
                "hook must not rewrite {cmd}, got: {:?}",
                String::from_utf8_lossy(&output.stdout)
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_emits_rewrite_when_first_token_is_valid_absolute_path() {
        let root = unique_temp_dir("headroom-hook-bash-ok");
        fs::create_dir_all(&root).expect("create root");

        // Pick a binary that definitely exists on macOS/Linux test hosts.
        let real_binary = "/bin/echo";
        assert!(Path::new(real_binary).exists());

        // Fake rtk rewrites to use an absolute path that *does* exist.
        let fake_rtk = root.join("fake-rtk");
        fs::write(
            &fake_rtk,
            format!("#!/usr/bin/env bash\nshift\necho \"{real_binary} $*\"\n"),
        )
        .expect("write fake rtk");
        fs::set_permissions(
            &fake_rtk,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod rtk");

        let system_python = PathBuf::from("/usr/bin/python3");
        let hook_body = build_headroom_rtk_hook(&fake_rtk, &system_python);
        let hook_path = root.join("hook.sh");
        fs::write(&hook_path, &hook_body).expect("write hook");
        fs::set_permissions(
            &hook_path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod hook");

        let stdin = r#"{"tool_input":{"command":"git status"}}"#;
        let output = crate::proc::command("bash")
            .arg(&hook_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(stdin.as_bytes())
                    .unwrap();
                child.wait_with_output()
            })
            .expect("run hook");

        assert!(output.status.success(), "hook should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(real_binary),
            "rewrite should be emitted when first token is a valid absolute path, got stdout: {stdout:?}, stderr: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("Headroom RTK auto-rewrite"),
            "should be a rewrite hookSpecificOutput payload"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_pins_bare_rtk_token_to_managed_absolute_path() {
        let root = unique_temp_dir("headroom-hook-pin-rtk");
        fs::create_dir_all(&root).expect("create root");

        // Fake rtk emits a bare `rtk` leading token, like the real binary.
        // `rtk` is NOT on PATH here, so without pinning the rewrite would be a
        // "command not found" landmine and the defense-in-depth guard would
        // drop it. Pinning to the managed absolute path must keep the rewrite.
        let fake_rtk = root.join("rtk");
        fs::write(&fake_rtk, "#!/usr/bin/env bash\nshift\necho \"rtk $*\"\n")
            .expect("write fake rtk");
        fs::set_permissions(
            &fake_rtk,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod rtk");

        let system_python = PathBuf::from("/usr/bin/python3");
        let hook_body = build_headroom_rtk_hook(&fake_rtk, &system_python);
        let hook_path = root.join("hook.sh");
        fs::write(&hook_path, &hook_body).expect("write hook");
        fs::set_permissions(
            &hook_path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod hook");

        let stdin = r#"{"tool_input":{"command":"git status"}}"#;
        let output = crate::proc::command("bash")
            .arg(&hook_path)
            .env("PATH", "/usr/bin:/bin") // ensure bare `rtk` is unresolvable
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(stdin.as_bytes())
                    .unwrap();
                child.wait_with_output()
            })
            .expect("run hook");

        assert!(output.status.success(), "hook should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Headroom RTK auto-rewrite"),
            "rewrite should survive when bare `rtk` is pinned to absolute path, got stdout: {stdout:?}, stderr: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(&fake_rtk.to_string_lossy().replace('"', "\\\"")),
            "rewritten command should invoke the managed rtk by absolute path, got: {stdout:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_rewrite_runs_with_windows_style_managed_path() {
        // Regression for #120: on Windows the managed path is `C:\...\rtk.exe`
        // and was spliced into the rewritten command unquoted, so Git Bash
        // stripped the backslashes ("C:Users...rtk.exe: command not found").
        // Simulate it on Unix: the hook is handed a Windows-style path (a
        // file of that literal name exists, so the `-x` check passes) and a fake `cygpath` maps it to a real directory
        // containing a space. The emitted command must then actually run.
        let root = unique_temp_dir("headroom-hook-windows-path");
        let bin_dir = root.join("Program Files").join("bin");
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        fs::create_dir_all(&fake_bin).expect("create fakebin");
        let exec = |path: &Path, body: &str| {
            fs::write(path, body).expect("write script");
            fs::set_permissions(
                path,
                <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .expect("chmod script");
        };

        let rtk_body = "#!/usr/bin/env bash\nif [ \"$1\" = rewrite ]; then shift; echo \"rtk $* && rtk git log\"; else echo \"rtk-ran $*\"; fi\n";
        let windows_rtk = r"C:\Program Files\bin\rtk.exe";
        // In fakebin, which is both the hook's cwd (for `-x`) and on PATH (a
        // name without `/` is executed via a PATH lookup, not from cwd).
        exec(&fake_bin.join(windows_rtk), rtk_body);
        exec(&bin_dir.join("rtk.exe"), rtk_body);
        exec(&bin_dir.join("rtk"), rtk_body);
        exec(
            &fake_bin.join("cygpath"),
            &format!(
                "#!/usr/bin/env bash\n[ \"$1\" = -u ] && shift\nif [ \"$1\" = '{windows_rtk}' ]; then printf '%s\\n' '{}'; else printf '%s\\n' \"$1\"; fi\n",
                bin_dir.join("rtk.exe").display()
            ),
        );

        let system_python = PathBuf::from("/usr/bin/python3");
        let hook_body = build_headroom_rtk_hook(Path::new(windows_rtk), &system_python);
        let hook_path = fake_bin.join("hook.sh");
        exec(&hook_path, &hook_body);

        let run = |cmd: &mut std::process::Command, stdin: &str| {
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child
                        .stdin
                        .as_mut()
                        .unwrap()
                        .write_all(stdin.as_bytes())
                        .unwrap();
                    child.wait_with_output()
                })
                .expect("run")
        };

        let output = run(
            crate::proc::command("bash")
                .arg(&hook_path)
                .current_dir(&fake_bin)
                .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display())),
            r#"{"tool_input":{"command":"git status"}}"#,
        );
        assert!(output.status.success(), "hook should exit 0");
        let payload: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
                panic!(
                    "hook must emit a rewrite ({e}), stdout: {:?}, stderr: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let rewritten = payload["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .expect("rewritten command")
            .to_string();
        assert!(
            !rewritten.contains("C:"),
            "the Windows path must reach the shell in MSYS form: {rewritten:?}"
        );

        // Execute it the way Claude Code does: from elsewhere, rtk not on PATH.
        let executed = run(
            crate::proc::command("bash")
                .arg("-c")
                .arg(&rewritten)
                .current_dir(std::env::temp_dir())
                .env("PATH", "/usr/bin:/bin"),
            "",
        );
        let stdout = String::from_utf8_lossy(&executed.stdout);
        assert!(
            executed.status.success(),
            "rewritten command {rewritten:?} failed: stdout {stdout:?}, stderr {:?}",
            String::from_utf8_lossy(&executed.stderr)
        );
        assert_eq!(stdout, "rtk-ran git status\nrtk-ran git log\n");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn msys_path_converts_drive_letter_paths_only() {
        assert_eq!(
            msys_path(r"C:\Users\me\AppData\Local\Headroom\headroom\bin"),
            "/c/Users/me/AppData/Local/Headroom/headroom/bin"
        );
        assert_eq!(msys_path(r"d:\Program Files\x\"), "/d/Program Files/x");
        assert_eq!(msys_path("E:"), "/e");
        assert_eq!(msys_path("/usr/local/bin"), "/usr/local/bin");
        assert_eq!(msys_path("relative/C:/x"), "relative/C:/x");
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_prepends_managed_path_so_embedded_rtk_resolves() {
        // Regression for compound commands: `rtk rewrite` embeds a bare `rtk`
        // after `&&`/`;`/`|`, which the leading-token pin never touches. The
        // hook must prepend the managed bin dir to PATH so the embedded token
        // resolves in the non-interactive, non-login shell Claude Code spawns.
        let root = unique_temp_dir("headroom-hook-embedded-rtk");
        fs::create_dir_all(&root).expect("create root");

        // Fake rtk emits a compound command with rtk embedded mid-chain, like
        // the real binary does for `cd x && <cmd>`. The leading token is `cd`.
        let fake_rtk = root.join("rtk");
        fs::write(
            &fake_rtk,
            "#!/usr/bin/env bash\nshift\necho \"cd /tmp && rtk $*\"\n",
        )
        .expect("write fake rtk");
        fs::set_permissions(
            &fake_rtk,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod rtk");

        let system_python = PathBuf::from("/usr/bin/python3");
        let hook_body = build_headroom_rtk_hook(&fake_rtk, &system_python);
        let hook_path = root.join("hook.sh");
        fs::write(&hook_path, &hook_body).expect("write hook");
        fs::set_permissions(
            &hook_path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod hook");

        let stdin = r#"{"tool_input":{"command":"git status"}}"#;
        let output = crate::proc::command("bash")
            .arg(&hook_path)
            .env("PATH", "/usr/bin:/bin") // bare `rtk` unresolvable without the prepend
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(stdin.as_bytes())
                    .unwrap();
                child.wait_with_output()
            })
            .expect("run hook");

        assert!(output.status.success(), "hook should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Headroom RTK auto-rewrite"),
            "compound rewrite should be emitted, got stdout: {stdout:?}, stderr: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The emitted command must export the managed bin dir onto PATH so the
        // embedded `rtk` resolves, and must preserve that embedded token.
        assert!(
            stdout.contains("export PATH="),
            "rewrite must prepend a PATH export, got: {stdout:?}"
        );
        assert!(
            stdout.contains(&root.to_string_lossy().replace('"', "\\\"")),
            "PATH export must point at the managed bin dir, got: {stdout:?}"
        );
        assert!(
            stdout.contains("&& rtk "),
            "embedded rtk token must be preserved, got: {stdout:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn rtk_hook_ignores_only_headrooms_own_remote_control_settings() {
        // The relaunch's env-only --settings must not read as user rules (it
        // turned RTK off for every remote-control session); any other does.
        assert!(super::HOOK_RULES_PY.contains(&format!(
            "--settings {}",
            super::CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE.replace('"', "\\\"")
        )));
    }

    #[test]
    #[cfg(unix)]
    fn hook_script_auto_allows_only_what_claude_code_would_not_ask() {
        // `rtk rewrite`'s exit code is its verdict against the user's Claude
        // Code permission rules. Exit 3 ("rewrite, but ask") used to be turned
        // into "allow", so `git status; rm -rf ~` ran with no prompt. rtk also
        // answers 3 for every unruled command, so 3 may allow only a plain
        // read-only command, or anything in bypassPermissions, and only while no
        // ask/deny rule could be dodged by the rewrite. 1, 2 and the rest never
        // emit anything. (Assumes no ancestor of the test runner passes
        // `--settings` or `--disallowedTools`, which also turns 3 off.)
        //
        // rtk reads project rules from the nearest `.claude/` above its cwd,
        // so it must run from the project root Claude Code loaded (or HOME),
        // never from a subdir under a planted parent `.claude/`.
        let root = unique_temp_dir("headroom-hook-bash-verdict");
        let home = root.join("home");
        let shared = root.join("shared");
        let victim = shared.join("victim");
        let project = root.join("project");
        for dir in [home.join(".claude"), shared.join(".claude"), victim.clone()] {
            fs::create_dir_all(dir).expect("create dirs");
        }
        fs::create_dir_all(project.join(".claude")).expect("create project");
        std::os::unix::fs::symlink(&victim, project.join("link_out")).expect("symlink out");
        let system_python = PathBuf::from("/usr/bin/python3");

        // The updated command when the hook allowed, None when it stayed silent.
        let run = |code: i32, project_dir: &Path, command: &str, mode: &str| {
            let fake_rtk = root.join(format!("fake-rtk-{code}"));
            fs::write(
                &fake_rtk,
                format!("#!/usr/bin/env bash\nshift\necho \"rtk $* @$PWD\"\nexit {code}\n"),
            )
            .expect("write fake rtk");
            fs::set_permissions(
                &fake_rtk,
                <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .expect("chmod rtk");
            let hook_path = root.join(format!("hook-{code}.sh"));
            fs::write(
                &hook_path,
                build_headroom_rtk_hook(&fake_rtk, &system_python),
            )
            .expect("write hook");

            let input = serde_json::json!({
                "permission_mode": mode,
                "tool_input": {"command": command},
            });
            let output = crate::proc::command("bash")
                .arg(&hook_path)
                .current_dir(&victim)
                .env("HOME", &home)
                .env("CLAUDE_PROJECT_DIR", project_dir)
                .env_remove("CLAUDE_CONFIG_DIR")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child
                        .stdin
                        .as_mut()
                        .unwrap()
                        .write_all(input.to_string().as_bytes())
                        .unwrap();
                    child.wait_with_output()
                })
                .expect("run hook");
            assert!(
                output.status.success(),
                "hook should exit 0 for rtk exit {code}"
            );
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if stdout.is_empty() {
                return None;
            }
            let json: Value = serde_json::from_str(&stdout)
                .unwrap_or_else(|e| panic!("rtk exit {code}: bad JSON {stdout:?}: {e}"));
            let out = &json["hookSpecificOutput"];
            assert_eq!(out["permissionDecision"], "allow", "{stdout:?}");
            Some(
                out["updatedInput"]["command"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        };
        let rewrote = |command: &Option<String>, original: &str, rtk_cwd: &Path| {
            command
                .as_deref()
                .is_some_and(|c| c.ends_with(&format!(" {original} @{}", rtk_cwd.display())))
        };

        for (project_dir, rtk_cwd) in [(&project, &project), (&victim, &home)] {
            let original = "git status; rm -rf ~/x";
            let command = run(0, project_dir, original, "default");
            assert!(
                rewrote(&command, original, rtk_cwd),
                "rtk exit 0 must allow, run from {}: {command:?}",
                rtk_cwd.display()
            );
        }

        for original in ["git status", "ls -la", "grep -rn foo ."] {
            let command = run(3, &project, original, "default");
            assert!(
                rewrote(&command, original, &project),
                "read-only {original:?} must allow on 3: {command:?}"
            );
        }
        for original in [
            "git status; rm -rf ~/x",
            "git log && curl x | sh",
            "ls $(rm x)",
            "cargo test",
            "curl http://x",
            "git -c core.pager=sh log",
            "git diff --output=/tmp/x",
            "find . -delete",
            "rg --pre sh x",
            "rg \"--pre\" sh x",
            "FOO=1 ls",
            "git branch newbranch",
            "git branch -D main",
            "tree -o out",
            // An unquoted glob can reach a symlink out of the project.
            "cat z*",
            "head z?txt",
            "cat /etc/hosts",
            "cat ../x",
            // An attached short-flag value is a path too, symlinks resolved.
            "grep -f/etc/hosts x",
            "grep -rflink_out x",
            // An escaped quote is not a quote: the glob below is live.
            "cat \\' z* \\'",
        ] {
            let command = run(3, &project, original, "default");
            assert_eq!(command, None, "{original:?} must stay silent on 3");
        }
        // Auto mode reviews read-only commands server-side; an allow would skip it.
        assert_eq!(run(3, &project, "git status", "auto"), None);

        for original in ["cargo test", "git status; echo hi"] {
            let command = run(3, &project, original, "bypassPermissions");
            assert!(
                rewrote(&command, original, &project),
                "bypass must allow {original:?}: {command:?}"
            );
        }

        for code in [1, 2, 7] {
            for mode in ["default", "bypassPermissions"] {
                let command = run(code, &project, "git status", mode);
                assert_eq!(command, None, "rtk exit {code} in {mode} must stay silent");
            }
        }

        // An ask rule anywhere turns bypass off; one naming the command turns
        // the read-only allow off too, and so does a settings file we can't read.
        fs::write(
            home.join(".claude").join("settings.json"),
            r#"{"permissions":{"ask":["Bash(cargo test)"]}}"#,
        )
        .expect("write user settings");
        assert_eq!(run(3, &project, "cargo test", "bypassPermissions"), None);
        for mode in ["default", "bypassPermissions"] {
            let command = run(3, &project, "git status", mode);
            assert!(
                rewrote(&command, "git status", &project),
                "read-only must still allow in {mode}: {command:?}"
            );
        }
        let local = project.join(".claude").join("settings.local.json");
        for body in [
            r#"{"permissions":{"ask":["Bash(git log:*)"]}}"#,
            "{not json",
        ] {
            fs::write(&local, body).expect("write project settings");
            assert_eq!(run(3, &project, "git status", "default"), None, "{body}");
        }
        // rtk's exit 0 covers only the rules rtk read, so a source this hook
        // cannot read (managed settings, or this one) keeps it silent too.
        assert_eq!(run(0, &project, "cargo test", "default"), None);

        let _ = fs::remove_dir_all(root);
    }

    // ── Lifecycle integration tests ──────────────────────────────────────────
    //
    // These tests drive `apply_client_setup` / `verify_client_setup` /
    // `disable_client_setup` / `clear_client_setups` against a temp $HOME so we
    // catch regressions in the user-visible setup-then-teardown flow. Tests are
    // serialized via `serial_test` because they mutate process-wide env vars
    // (HOME, XDG_DATA_HOME, SHELL).

    /// RAII-style guard that snapshots HOME / XDG_DATA_HOME / SHELL, points
    /// them at a fresh tempdir, and restores them on drop. Used to keep
    /// lifecycle tests from touching the developer's real profile.
    struct TestHome {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        prev_home: Option<std::ffi::OsString>,
        prev_xdg: Option<std::ffi::OsString>,
        prev_shell: Option<std::ffi::OsString>,
        prev_codex: Option<std::ffi::OsString>,
        prev_zdotdir: Option<std::ffi::OsString>,
        prev_xdg_config: Option<std::ffi::OsString>,
        prev_opencode_config: Option<std::ffi::OsString>,
        prev_grok_home: Option<std::ffi::OsString>,
        prev_headroom_data_dir: Option<std::ffi::OsString>,
        prev_appdata: Option<std::ffi::OsString>,
        prev_localappdata: Option<std::ffi::OsString>,
        // Held for the guard's lifetime: env vars are process-global, so two
        // TestHome tests running on parallel threads corrupt each other's HOME
        // (and can leak writes into the developer's real profile). serial_test
        // only covers tests that opted in; this lock covers every TestHome user.
        _env_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestHome {
        fn new() -> Self {
            let env_lock = crate::test_env_lock::lock_home();
            let tmp = tempfile::tempdir().expect("create temp home");
            let home = tmp.path().to_path_buf();
            let prev_home = std::env::var_os("HOME");
            let prev_xdg = std::env::var_os("XDG_DATA_HOME");
            let prev_shell = std::env::var_os("SHELL");
            let prev_codex = std::env::var_os("CODEX_HOME");
            let prev_zdotdir = std::env::var_os("ZDOTDIR");
            let prev_xdg_config = std::env::var_os("XDG_CONFIG_HOME");
            let prev_opencode_config = std::env::var_os("OPENCODE_CONFIG");
            let prev_grok_home = std::env::var_os("GROK_HOME");
            let prev_headroom_data_dir = std::env::var_os("HEADROOM_DATA_DIR");
            let prev_appdata = std::env::var_os("APPDATA");
            let prev_localappdata = std::env::var_os("LOCALAPPDATA");
            std::env::set_var("HOME", &home);
            // Pin the Windows profile dirs into the temp home: perform_full_cleanup
            // reads these on Windows, and the runner's
            // real AppData is otherwise shared across all parallel test
            // processes. No-ops on Unix (only read under cfg windows).
            std::env::set_var("APPDATA", home.join("AppData").join("Roaming"));
            std::env::set_var("LOCALAPPDATA", home.join("AppData").join("Local"));
            std::env::set_var("XDG_DATA_HOME", home.join(".local").join("share"));
            // Pin the app data dir into the temp home. dirs::data_local_dir()
            // ignores HOME/XDG on macOS and Windows, so without this the setup
            // state, seeded rtk, and cleanup sweeps all hit the REAL profile —
            // and under nextest (process per test) the env lock below cannot
            // serialize that sharing across processes.
            std::env::set_var(
                "HEADROOM_DATA_DIR",
                home.join(".local").join("share").join("Headroom"),
            );
            // Pin XDG_CONFIG_HOME into the temp home and clear the opencode /
            // grok override vars: a dev machine with any of these set would
            // otherwise have the opencode/grok tests write the developer's
            // REAL client configs.
            std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            std::env::remove_var("OPENCODE_CONFIG");
            std::env::remove_var("GROK_HOME");
            // Force a deterministic shell family so tests don't depend on the
            // dev's login shell.
            std::env::set_var("SHELL", "/bin/zsh");
            // Clear any real CODEX_HOME so codex_home() falls back to the temp
            // $HOME/.codex and the Codex tests stay hermetic on dev machines.
            std::env::remove_var("CODEX_HOME");
            // Clear any real ZDOTDIR so zsh_dir() resolves against the temp
            // $HOME and the shell-block tests stay hermetic on dev machines.
            std::env::remove_var("ZDOTDIR");
            // Clear every var huggingface_hub honours, so hf_hub_cache_dir()
            // resolves to the temp $HOME. Without this, a dev with HF_HOME or
            // HF_HUB_CACHE set would have the cleanup tests delete models out
            // of their REAL HuggingFace cache.
            for var in [
                "HF_HUB_CACHE",
                "HUGGINGFACE_HUB_CACHE",
                "HF_HOME",
                "XDG_CACHE_HOME",
            ] {
                std::env::remove_var(var);
            }
            // Mirror what the app does at startup so write_setup_state has a
            // config dir to land in.
            crate::storage::ensure_data_dirs(&crate::storage::app_data_dir())
                .expect("ensure_data_dirs in test home");
            TestHome {
                _tmp: tmp,
                home,
                prev_home,
                prev_xdg,
                prev_shell,
                prev_codex,
                prev_zdotdir,
                prev_xdg_config,
                prev_opencode_config,
                prev_grok_home,
                prev_headroom_data_dir,
                prev_appdata,
                prev_localappdata,
                _env_lock: env_lock,
            }
        }

        fn path(&self) -> &Path {
            &self.home
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_xdg.take() {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match self.prev_shell.take() {
                Some(v) => std::env::set_var("SHELL", v),
                None => std::env::remove_var("SHELL"),
            }
            match self.prev_codex.take() {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
            match self.prev_xdg_config.take() {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match self.prev_opencode_config.take() {
                Some(v) => std::env::set_var("OPENCODE_CONFIG", v),
                None => std::env::remove_var("OPENCODE_CONFIG"),
            }
            match self.prev_grok_home.take() {
                Some(v) => std::env::set_var("GROK_HOME", v),
                None => std::env::remove_var("GROK_HOME"),
            }
            match self.prev_zdotdir.take() {
                Some(v) => std::env::set_var("ZDOTDIR", v),
                None => std::env::remove_var("ZDOTDIR"),
            }
            match self.prev_headroom_data_dir.take() {
                Some(v) => std::env::set_var("HEADROOM_DATA_DIR", v),
                None => std::env::remove_var("HEADROOM_DATA_DIR"),
            }
            match self.prev_appdata.take() {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
            match self.prev_localappdata.take() {
                Some(v) => std::env::set_var("LOCALAPPDATA", v),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
        }
    }

    /// RTK is opt-in: its PATH block and Claude Code hook are only wired when the
    /// managed binary exists on disk. Drop a fake one at the default location so
    /// tests covering a fully-configured environment exercise the RTK wiring.
    fn seed_installed_rtk() {
        let rtk = super::default_headroom_rtk_path();
        fs::create_dir_all(rtk.parent().unwrap()).unwrap();
        fs::write(&rtk, "#!/bin/sh\n").unwrap();
    }

    fn read_settings_json(path: &Path) -> serde_json::Value {
        let raw = fs::read_to_string(path).expect("read settings.json");
        serde_json::from_str(&raw).expect("parse settings.json")
    }

    /// RUST-5X: a shell profile with non-UTF-8 bytes (latin-1 comment) made
    /// `read_to_string` fail and took the whole client setup down with it, so
    /// Claude Code never got routed. The profile is convenience; core routing
    /// via ~/.claude/settings.json must still land, and the user's bytes must
    /// survive untouched.
    #[test]
    #[serial_test::serial]
    fn apply_client_setup_survives_non_utf8_shell_profile() {
        let home = TestHome::new();
        // 0xFF is never valid UTF-8.
        let latin1 = b"# caf\xe9 alias\nalias ll='ls -l'\n\xff\n";
        let zshrc = home.path().join(".zshrc");
        fs::write(&zshrc, latin1).unwrap();
        fs::write(home.path().join(".zshenv"), latin1).unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks": {}}"#,
        )
        .unwrap();
        seed_installed_rtk();

        let result =
            super::apply_client_setup("claude_code").expect("setup succeeds despite bad profile");
        assert!(result.applied);
        assert!(
            result.shell_profile_unwritable,
            "shell step reported as skipped"
        );
        assert_eq!(
            fs::read(&zshrc).unwrap(),
            latin1,
            "user's non-UTF-8 profile left byte-identical"
        );

        // The part that actually routes Claude Code still happened.
        let settings = read_settings_json(&home.path().join(".claude").join("settings.json"));
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some("http://127.0.0.1:6767")
        );
        // Verification reads the same profiles and must not blow up either.
        super::verify_client_setup("claude_code").expect("verification tolerates bad profile");
    }

    /// A shell rc Headroom may not read (chmod 000, a root-owned copy, a
    /// dotfiles symlink macOS privacy protection denies) failed shell-target
    /// discovery, so every client setup aborted before settings.json was
    /// written, and a quit-time disable returned before it stripped
    /// ANTHROPIC_BASE_URL, leaving Claude Code on the stopped proxy.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn unreadable_shell_rc_blocks_neither_setup_nor_quit_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        let zshrc = home.path().join(".zshrc");
        fs::write(&zshrc, "# user zshrc\n").unwrap();
        fs::set_permissions(&zshrc, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&zshrc).is_ok() {
            eprintln!("skipping: running as root, permissions are not enforced");
            return;
        }
        let settings = home.path().join(".claude").join("settings.json");
        let base_url = |settings: &Path| {
            read_settings_json(settings)["env"]["ANTHROPIC_BASE_URL"]
                .as_str()
                .map(str::to_owned)
        };

        super::apply_client_setup("claude_code").expect("setup succeeds despite unreadable rc");
        assert_eq!(
            base_url(&settings).as_deref(),
            Some("http://127.0.0.1:6767")
        );
        super::clear_client_setups().expect("clear");
        assert_eq!(base_url(&settings), None);

        // A readable profile holds our block, but the shell cleanup still
        // fails (no backup can be written next to it). Routing is removed first.
        super::apply_client_setup("claude_code").expect("re-apply");
        assert!(fs::read_to_string(home.path().join(".zprofile"))
            .unwrap()
            .contains("# >>> headroom:claude_code >>>"));
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let disabled = super::disable_client_setup("claude_code");
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o755)).unwrap();
        disabled.expect("disable tolerates a shell cleanup failure");
        assert_eq!(base_url(&settings), None);
    }

    /// Upgrading users carry an rc7 block (an unconditional export that
    /// outlived quit) or an A-1 block (no export at all). The first apply
    /// rewrites either, in place, to the probed export, and a second apply
    /// changes nothing.
    #[test]
    #[serial_test::serial]
    fn apply_claude_code_rewrites_older_shell_blocks_to_the_probed_export() {
        let port = crate::proxy_intercept::INTERCEPT_PORT;
        let a1_block = super::claude_code_shell_block(port)
            .lines()
            .skip_while(|line| !line.starts_with("# /remote-control"))
            .collect::<Vec<_>>()
            .join("\n");
        for older in [
            "export ANTHROPIC_BASE_URL=http://127.0.0.1:6767".to_string(),
            a1_block,
        ] {
            let home = TestHome::new();
            let zshrc = home.path().join(".zshrc");
            fs::write(
                &zshrc,
                format!("# user zshrc\n# >>> headroom:claude_code >>>\n{older}\n# <<< headroom:claude_code <<<\n# tail\n"),
            )
            .unwrap();

            let result = super::apply_client_setup("claude_code").expect("apply");
            assert!(
                result.verification.verified,
                "{:?}",
                result.verification.failures
            );
            assert!(
                result
                    .verification
                    .checks
                    .iter()
                    .any(|check| check.contains("export in managed shell block")),
                "{:?}",
                result.verification.checks
            );
            let rc = fs::read_to_string(&zshrc).unwrap();
            assert_eq!(
                rc,
                format!(
                    "# user zshrc\n# >>> headroom:claude_code >>>\n{}\n# <<< headroom:claude_code <<<\n# tail\n",
                    super::claude_code_shell_block(port)
                )
            );

            let again = super::apply_client_setup("claude_code").expect("re-apply");
            assert!(
                !again.changed_files.iter().any(|f| f.ends_with(".zshrc")),
                "{:?}",
                again.changed_files
            );
            assert_eq!(fs::read_to_string(&zshrc).unwrap(), rc);
        }
    }

    /// The RTK PATH export is shell convenience that apply skips when the first
    /// profile is unwritable or not UTF-8; verification must not then fail
    /// Claude Code forever ("Setup incomplete", a useless re-apply every
    /// repair pass) while settings.json and the RTK hook route it fine.
    #[test]
    #[serial_test::serial]
    fn claude_code_verifies_when_the_rtk_path_export_cannot_be_written() {
        let home = TestHome::new();
        let zprofile = home.path().join(".zprofile");
        fs::write(&zprofile, b"# caf\xe9\n\xff\n").unwrap();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        seed_installed_rtk();

        let result = super::apply_client_setup("claude_code").expect("apply");
        assert!(result.shell_profile_unwritable, "shell step was skipped");
        assert!(
            !fs::read_to_string(home.path().join(".zshrc"))
                .unwrap()
                .contains("headroom:managed_rtk"),
            "precondition: no RTK PATH export anywhere"
        );
        assert!(
            result.verification.verified,
            "{:?}",
            result.verification.failures
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_then_verify_claude_code_writes_expected_files() {
        let home = TestHome::new();
        // Seed an empty zshrc/zshenv so the shell-block writers have files to
        // edit and don't depend on the dev's real shell config layout.
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks": {}}"#,
        )
        .unwrap();
        seed_installed_rtk();

        let result = super::apply_client_setup("claude_code").expect("apply_client_setup succeeds");
        assert!(result.applied);
        assert_eq!(result.client_id, "claude_code");

        // Hook script and settings.json hook entry must be present.
        let hook_path = home
            .path()
            .join(".claude")
            .join("hooks")
            .join("headroom-rtk-rewrite.sh");
        assert!(hook_path.exists(), "hook script written to disk");
        let hook_contents = fs::read_to_string(&hook_path).unwrap();
        assert!(
            hook_contents.starts_with("#!/usr/bin/env bash"),
            "hook has expected shebang"
        );

        let settings = read_settings_json(&home.path().join(".claude").join("settings.json"));
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some("http://127.0.0.1:6767"),
            "claude settings.json points env at headroom proxy"
        );
        assert_eq!(
            settings["env"]["ENABLE_TOOL_SEARCH"].as_str(),
            Some("true"),
            "claude settings.json keeps tool-schema deferral on (issue #746)"
        );
        let pre_tool_use = &settings["hooks"]["PreToolUse"];
        assert!(
            pre_tool_use.is_array() && !pre_tool_use.as_array().unwrap().is_empty(),
            "PreToolUse hook entry exists, got: {settings}"
        );

        // The managed shell block exports ANTHROPIC_BASE_URL only while the
        // intercept answers (settings.json routes Claude Code itself).
        let zshrc = fs::read_to_string(home.path().join(".zshrc")).unwrap();
        assert!(
            zshrc.contains(&super::intercept_export_line(
                "ANTHROPIC_BASE_URL",
                "http://127.0.0.1:6767"
            )),
            "claude_code block with the probed export, got:\n{zshrc}"
        );

        // verify_client_setup should report all the configured checks.
        // Proxy reachability is reported via `proxy_reachable` only, so a
        // missing proxy in the test environment no longer flips `verified`.
        let verification =
            super::verify_client_setup("claude_code").expect("verify_client_setup succeeds");
        assert_eq!(verification.client_id, "claude_code");
        assert!(
            verification
                .checks
                .iter()
                .any(|c| c.contains("ANTHROPIC_BASE_URL")),
            "verification reports the env check, got: {:?}",
            verification.checks
        );
        assert!(
            verification
                .checks
                .iter()
                .any(|c| c.contains("RTK Claude hook")),
            "verification reports the hook check, got: {:?}",
            verification.checks
        );
    }

    #[test]
    #[serial_test::serial]
    fn chisle_compression_scope_leaves_a_machine_without_claude_code_alone() {
        let home = TestHome::new();
        super::scope_chisle_compression(true).unwrap();
        assert!(!home.path().join(".claude").exists());
        assert!(!super::claude_code_user_state_exists(home.path()));
    }

    #[test]
    #[serial_test::serial]
    fn chisle_compression_scope_round_trips_and_keeps_other_env() {
        let home = TestHome::new();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings = home.path().join(".claude").join("settings.json");
        fs::write(
            &settings,
            r#"{"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:6767"}}"#,
        )
        .unwrap();

        super::scope_chisle_compression(true).unwrap();
        let scoped = super::read_claude_settings_env("CHISLE_COMPRESS_TOOLS")
            .unwrap()
            .expect("planted");
        assert!(!scoped.contains("mcp__"), "{scoped}");

        super::scope_chisle_compression(false).unwrap();
        assert_eq!(
            super::read_claude_settings_env("CHISLE_COMPRESS_TOOLS").unwrap(),
            None
        );
        assert_eq!(
            super::read_claude_settings_env("ANTHROPIC_BASE_URL").unwrap(),
            Some("http://127.0.0.1:6767".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn enable_tool_search_defaults_on_but_respects_user_value() {
        let home = TestHome::new();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings = home.path().join(".claude").join("settings.json");
        fs::write(
            &settings,
            r#"{"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:6767"}}"#,
        )
        .unwrap();

        // Absent -> we plant our default.
        super::configure_claude_settings_env_if_absent(
            super::HEADROOM_ENABLE_TOOL_SEARCH_KEY,
            super::HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
        )
        .unwrap();
        assert_eq!(
            super::read_claude_settings_env("ENABLE_TOOL_SEARCH").unwrap(),
            Some("true".to_string())
        );

        // User set it themselves (e.g. "false" as the LSP-400 fallback) -> untouched.
        fs::write(
            &settings,
            r#"{"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:6767", "ENABLE_TOOL_SEARCH": "false"}}"#,
        )
        .unwrap();
        super::configure_claude_settings_env_if_absent(
            super::HEADROOM_ENABLE_TOOL_SEARCH_KEY,
            super::HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
        )
        .unwrap();
        assert_eq!(
            super::read_claude_settings_env("ENABLE_TOOL_SEARCH").unwrap(),
            Some("false".to_string()),
            "a user-owned value must not be clobbered"
        );

        // Cleanup only strips our own value, so the user's "false" survives.
        super::remove_claude_settings_env(
            super::HEADROOM_ENABLE_TOOL_SEARCH_KEY,
            super::HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
            None,
        )
        .unwrap();
        assert_eq!(
            super::read_claude_settings_env("ENABLE_TOOL_SEARCH").unwrap(),
            Some("false".to_string()),
            "cleanup must not delete a user-owned value"
        );

        // Our own planted value, though, is removed on cleanup.
        super::configure_claude_settings_env("ENABLE_TOOL_SEARCH", "true").unwrap();
        super::remove_claude_settings_env(
            super::HEADROOM_ENABLE_TOOL_SEARCH_KEY,
            super::HEADROOM_ENABLE_TOOL_SEARCH_VALUE,
            None,
        )
        .unwrap();
        assert_eq!(
            super::read_claude_settings_env("ENABLE_TOOL_SEARCH").unwrap(),
            None
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_claude_writes_guard_and_disable_preserves_env_and_user_hooks() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        // Pre-existing user-authored hook that must survive apply and disable.
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        )
        .unwrap();
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("first apply");
        super::apply_client_setup("claude_code").expect("second apply");

        let script = home
            .path()
            .join(".claude")
            .join("hooks")
            .join("headroom-claude-guard.py");
        assert!(script.exists(), "guard script written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script).unwrap().permissions().mode();
            assert!(
                mode & 0o111 != 0,
                "guard script is executable, got {mode:o}"
            );
        }

        let settings_path = home.path().join(".claude").join("settings.json");
        let settings = read_settings_json(&settings_path);
        // The registered command is platform-dependent (/usr/bin/python3 vs the
        // quoted managed python.exe), so assert against the real builder.
        let command = super::claude_guard_command();
        let guard_count = |event: &str| {
            settings["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| {
                    entry["hooks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|h| h["command"] == serde_json::Value::String(command.clone()))
                })
                .count()
        };
        // Guard registered exactly once on SessionStart despite the double-apply.
        assert_eq!(
            guard_count("SessionStart"),
            1,
            "guard registered once for SessionStart, got:\n{settings:#}"
        );
        // Never on UserPromptSubmit: exit 2 there blocks every prompt in Claude
        // Desktop / Cowork VM sessions that can't reach the app.
        assert_eq!(
            guard_count("UserPromptSubmit"),
            0,
            "guard must not register on UserPromptSubmit, got:\n{settings:#}"
        );
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["matcher"],
            "startup|resume|clear|compact"
        );

        super::disable_client_setup("claude_code").expect("disable");

        assert!(!script.exists(), "guard script removed on disable");
        let after = read_settings_json(&settings_path);
        let after_str = serde_json::to_string(&after).unwrap();
        assert!(
            !after_str.contains("headroom-claude-guard.py"),
            "guard stripped from settings.json, got:\n{after:#}"
        );
        assert!(
            after_str.contains("echo mine"),
            "user-authored hook preserved, got:\n{after:#}"
        );
        // settings.json must NOT be deleted even if it were otherwise empty.
        assert!(settings_path.exists(), "settings.json preserved on disable");
    }

    #[test]
    #[serial_test::serial]
    fn apply_migrates_guard_off_user_prompt_submit() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        // An older build registered the guard on UserPromptSubmit, where exit 2
        // blocks every prompt in Claude Desktop / Cowork VM sessions. A user
        // hook on the same event must survive the migration.
        let script = home
            .path()
            .join(".claude")
            .join("hooks")
            .join("headroom-claude-guard.py");
        // Build via serde_json so the script path is JSON-escaped: raw format!
        // interpolation of a Windows path writes lone backslashes that json5
        // parsing silently eats, leaving a command the strip can never match.
        let old_command = format!("/usr/bin/python3 {}", script.display());
        let seeded = serde_json::json!({"hooks":{
            "SessionStart":[{"matcher":"startup|resume|clear|compact","hooks":[{"type":"command","command": old_command.as_str()}]}],
            "UserPromptSubmit":[
                {"hooks":[{"type":"command","command": old_command.as_str()}]},
                {"hooks":[{"type":"command","command":"echo mine"}]}
            ]
        }});
        fs::write(
            home.path().join(".claude").join("settings.json"),
            serde_json::to_string(&seeded).unwrap(),
        )
        .unwrap();
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("apply");

        let settings = read_settings_json(&home.path().join(".claude").join("settings.json"));
        let ups = serde_json::to_string(&settings["hooks"]["UserPromptSubmit"]).unwrap();
        assert!(
            !ups.contains("headroom-claude-guard.py"),
            "guard stripped from UserPromptSubmit, got:\n{settings:#}"
        );
        assert!(
            ups.contains("echo mine"),
            "user-authored UserPromptSubmit hook preserved, got:\n{settings:#}"
        );
        let ss = serde_json::to_string(&settings["hooks"]["SessionStart"]).unwrap();
        assert!(
            ss.contains("headroom-claude-guard.py"),
            "guard still registered on SessionStart, got:\n{settings:#}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn revert_external_mutations_spares_user_data_but_full_cleanup_removes_it() {
        // The Homebrew cask calls `--uninstall` (-> revert_external_mutations)
        // from its `uninstall` stanza, which runs on every `brew uninstall`.
        // Homebrew reserves user-data deletion for the opt-in `zap`, so the
        // narrow function must undo our edits to OTHER tools while leaving
        // Headroom's own directories intact. perform_full_cleanup (the in-app
        // "uninstall and quit") must still remove both.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks": {}}"#,
        )
        .unwrap();
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");

        // User data: Headroom's own directories.
        let app_dir = super::app_data_dir();
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("memory.db"), b"user data").unwrap();
        let dot_headroom = home.path().join(".headroom");
        fs::create_dir_all(&dot_headroom).unwrap();
        fs::write(dot_headroom.join("keep.json"), b"user data").unwrap();

        // An external mutation and a stray backup file, both of which the
        // narrow function is responsible for.
        let settings_path = home.path().join(".claude").join("settings.json");
        let stray_backup = home.path().join(".zshrc.headroom-backup-20260101000000");
        fs::write(&stray_backup, "# old\n").unwrap();
        assert_eq!(
            read_settings_json(&settings_path)["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some("http://127.0.0.1:6767"),
            "precondition: base url wired"
        );

        super::revert_external_mutations();

        assert!(
            read_settings_json(&settings_path)["env"]["ANTHROPIC_BASE_URL"].is_null(),
            "revert should strip the routing env"
        );
        assert!(
            !stray_backup.exists(),
            "revert should sweep stray backup files"
        );
        assert!(
            app_dir.join("memory.db").exists(),
            "revert must NOT delete Headroom's app data — that belongs to `brew zap`"
        );
        assert!(
            dot_headroom.join("keep.json").exists(),
            "revert must NOT delete ~/.headroom — that belongs to `brew zap`"
        );

        super::perform_full_cleanup();

        assert!(
            !app_dir.exists(),
            "full cleanup should remove Headroom's app data"
        );
        assert!(
            !dot_headroom.exists(),
            "full cleanup should remove ~/.headroom"
        );
    }

    #[test]
    #[serial_test::serial]
    fn revert_external_mutations_strips_markitdown_nudges_and_cache() {
        // `--uninstall` (NSIS, the Homebrew cask) has no ToolManager, so the
        // disable uninstall_and_quit runs never happens there. A settings.json
        // no parser accepts must not keep the nudges or the cache either.
        let home = TestHome::new();
        let claude_md = home.path().join(".claude").join("CLAUDE.md");
        let agents = home.path().join(".codex").join("AGENTS.md");
        fs::create_dir_all(claude_md.parent().unwrap()).unwrap();
        fs::create_dir_all(agents.parent().unwrap()).unwrap();
        fs::write(home.path().join(".claude").join("settings.json"), "{ nope").unwrap();
        fs::write(&claude_md, "# mine\n").unwrap();
        upsert_managed_block(&claude_md, "markitdown_office", "run the shim").unwrap();
        upsert_managed_block(&agents, "markitdown", "run the shim").unwrap();
        let cache = home.path().join(".cache").join("headroom-markitdown");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("0123.md"), "contract text").unwrap();

        super::revert_external_mutations();

        let md = fs::read_to_string(&claude_md).unwrap();
        assert!(
            md.contains("# mine") && !md.contains("headroom:markitdown"),
            "{md}"
        );
        assert!(!fs::read_to_string(&agents)
            .unwrap()
            .contains("headroom:markitdown"));
        assert!(!cache.exists());
    }

    #[test]
    #[serial_test::serial]
    fn full_cleanup_sweeps_our_hf_models_but_spares_shared_ones() {
        // Regression: this used to remove only models--chopratejas--kompress-v2-base
        // and orphaned every other model the runtime pulls (~788MB measured on a
        // real install). Sweep by `models--chopratejas--*` so a newly added upstream
        // model cannot silently start leaking.
        let home = TestHome::new();
        let hub = home.path().join(".cache").join("huggingface").join("hub");

        // Ours: author prefix of the bundled Python package.
        let ours = [
            "models--chopratejas--kompress-v2-base",
            "models--chopratejas--technique-router-onnx",
            "models--chopratejas--siglip-image-encoder-onnx",
        ];
        // Generic models we also pull, but which another tool may share. Removing
        // these would break that tool's cache, so they must survive.
        let shared = [
            "models--answerdotai--ModernBERT-base",
            "models--sentence-transformers--all-MiniLM-L6-v2",
            "models--Qdrant--all-MiniLM-L6-v2-onnx",
        ];

        for name in ours.iter().chain(shared.iter()) {
            for parent in [hub.join(name), hub.join(".locks").join(name)] {
                fs::create_dir_all(&parent).unwrap();
                fs::write(parent.join("blob"), b"weights").unwrap();
            }
        }

        super::perform_full_cleanup();

        for name in ours {
            assert!(
                !hub.join(name).exists(),
                "{name} is ours and should be removed"
            );
            assert!(
                !hub.join(".locks").join(name).exists(),
                "{name} lock dir should be removed"
            );
        }
        for name in shared {
            assert!(
                hub.join(name).join("blob").exists(),
                "{name} is shared with other tools and must survive uninstall"
            );
            assert!(
                hub.join(".locks").join(name).exists(),
                "{name} lock dir is shared and must survive"
            );
        }
        // The cache root itself is never ours to delete.
        assert!(hub.exists(), "hub cache root preserved");
    }

    #[test]
    #[serial_test::serial]
    fn full_cleanup_strips_base_url_and_guard_when_shell_block_removal_fails() {
        // Regression: perform_full_cleanup used to remove ANTHROPIC_BASE_URL and
        // the guard hook ONLY via clear_client_setups -> disable_client_setup,
        // where remove_shell_block runs first under `?`. A failure there left the
        // routing env and the guard hook in place, both of which brick Claude
        // once the proxy is gone. Force that failure and confirm cleanup still
        // strips them.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks": {}}"#,
        )
        .unwrap();
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("apply");

        let settings_path = home.path().join(".claude").join("settings.json");
        let guard_script = home
            .path()
            .join(".claude")
            .join("hooks")
            .join("headroom-claude-guard.py");
        assert_eq!(
            read_settings_json(&settings_path)["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some("http://127.0.0.1:6767"),
            "precondition: base url wired"
        );
        assert!(guard_script.exists(), "precondition: guard script written");

        // Sabotage a shell target: a directory where remove_managed_block expects
        // a file makes read_to_string fail, so disable_client_setup("claude_code")
        // bails before it reaches base-url / guard removal.
        let zshrc = home.path().join(".zshrc");
        fs::remove_file(&zshrc).unwrap();
        fs::create_dir(&zshrc).unwrap();

        super::perform_full_cleanup();

        assert!(settings_path.exists(), "settings.json preserved");
        let after = read_settings_json(&settings_path);
        assert!(
            after["env"]["ANTHROPIC_BASE_URL"].is_null(),
            "base url stripped despite shell-block failure, got:\n{after:#}"
        );
        assert!(
            !serde_json::to_string(&after["hooks"])
                .unwrap()
                .contains("headroom-claude-guard.py"),
            "guard hook stripped despite shell-block failure, got:\n{after:#}"
        );
        assert!(!guard_script.exists(), "guard script deleted");
    }

    #[test]
    #[serial_test::serial]
    fn apply_preserves_and_disable_restores_custom_base_url() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        // A corporate gateway already routes Claude Code before Headroom.
        let gateway = "https://gateway.corp.example/anthropic";
        fs::write(
            home.path().join(".claude").join("settings.json"),
            format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"{gateway}"}}}}"#),
        )
        .unwrap();
        seed_installed_rtk();

        let result = super::apply_client_setup("claude_code").expect("apply");
        // Setup captured the gateway and told the caller it took over routing.
        assert_eq!(result.replaced_base_url.as_deref(), Some(gateway));
        let settings_path = home.path().join(".claude").join("settings.json");
        let after_apply = read_settings_json(&settings_path);
        assert_eq!(
            after_apply["env"]["ANTHROPIC_BASE_URL"],
            serde_json::Value::String(super::HEADROOM_ANTHROPIC_BASE_URL.to_string())
        );
        assert_eq!(
            super::load_setup_state().preserved_base_urls["claude_code"],
            gateway
        );

        super::disable_client_setup("claude_code").expect("disable");
        // The gateway URL is restored, not deleted.
        let after_disable = read_settings_json(&settings_path);
        assert_eq!(
            after_disable["env"]["ANTHROPIC_BASE_URL"],
            serde_json::Value::String(gateway.to_string()),
            "custom base URL restored on disable, got:\n{after_disable:#}"
        );
        assert!(
            !super::load_setup_state()
                .preserved_base_urls
                .contains_key("claude_code"),
            "preserved entry consumed after restore"
        );
    }

    fn write_cc_switch_capture(url: &str) {
        let capture = crate::tool_manager::cc_switch_capture_path();
        fs::create_dir_all(capture.parent().unwrap()).unwrap();
        fs::write(&capture, format!(r#"{{"url":"{url}"}}"#)).unwrap();
    }

    /// The cc-switch reconciler points settings.json back at the intercept and
    /// records the provider URL it replaced. Quit and pause restore settings.json
    /// after the backend is gone, so that record is the only place the URL
    /// survives: it has to win over an older preserved gateway (deleting the key
    /// sent the provider's key to api.anthropic.com), and it has to outlive the
    /// relaunch that routes the restored URL through Headroom again, or the next
    /// backend has nothing to forward to.
    #[test]
    #[serial_test::serial]
    fn quit_restores_the_cc_switch_capture_and_relaunch_keeps_it() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings_path = home.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.corp.example/anthropic"}}"#,
        )
        .unwrap();
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");

        // Mid-session the user picks a relay in cc-switch; the reconciler puts
        // the intercept back and records the relay.
        let relay = "https://api.relay.example/anthropic";
        write_cc_switch_capture(relay);
        fs::write(
            &settings_path,
            format!(
                r#"{{"env":{{"ANTHROPIC_BASE_URL":"{}","ANTHROPIC_AUTH_TOKEN":"sk-relay"}}}}"#,
                super::HEADROOM_ANTHROPIC_BASE_URL
            ),
        )
        .unwrap();

        super::clear_client_setups().expect("quit");
        let after_quit = read_settings_json(&settings_path);
        assert_eq!(
            after_quit["env"]["ANTHROPIC_BASE_URL"], relay,
            "quit dropped the cc-switch provider, got:\n{after_quit:#}"
        );

        super::apply_client_setup("claude_code").expect("relaunch");
        assert_eq!(
            read_settings_json(&settings_path)["env"]["ANTHROPIC_BASE_URL"],
            super::HEADROOM_ANTHROPIC_BASE_URL
        );
        assert_eq!(
            crate::tool_manager::cc_switch_captured_upstream().as_deref(),
            Some(relay),
            "relaunch dropped the capture the quit restored"
        );
    }

    /// The crash guard runs quit's unwire, and only for an app that died with
    /// clients still wired. After a quit or pause nothing is wired: it must not
    /// even probe the port (a closed loopback port takes ~1s to refuse on
    /// Windows, on every quit) and must keep the snapshot the next launch
    /// restores from.
    #[test]
    #[serial_test::serial]
    fn crash_guard_unwires_only_what_a_dead_app_left_wired() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings_path = home.path().join(".claude").join("settings.json");
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");
        let base_url = || read_settings_json(&settings_path)["env"]["ANTHROPIC_BASE_URL"].clone();

        // The next instance already answers on 6767: its wiring stays.
        assert!(super::unwire_clients_after_crash(|| true).is_empty());
        assert_eq!(base_url(), super::HEADROOM_ANTHROPIC_BASE_URL);

        assert_eq!(
            super::unwire_clients_after_crash(|| false),
            vec!["claude_code".to_string()]
        );
        assert_ne!(base_url(), super::HEADROOM_ANTHROPIC_BASE_URL);
        assert!(super::load_setup_state()
            .remembered_clients
            .contains_key("claude_code"));

        assert!(
            super::unwire_clients_after_crash(|| panic!("probed with nothing wired")).is_empty()
        );
        assert!(
            super::load_setup_state()
                .remembered_clients
                .contains_key("claude_code"),
            "a second unwire lost the snapshot the next launch restores"
        );
    }

    /// The relay a quit restored is the reconciler's, not a pre-Headroom
    /// gateway. Relaunch recorded it in preserved_base_urls, where it outlived
    /// the capture: after a switch to Claude Official (the backend drops the
    /// capture) and a re-route, the next quit wrote the relay over the
    /// intercept and Claude Code sent its Anthropic OAuth token there.
    #[test]
    #[serial_test::serial]
    fn a_kept_cc_switch_capture_is_never_preserved_as_a_gateway() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings_path = home.path().join(".claude").join("settings.json");
        let relay = "https://api.relay.example/anthropic";
        write_cc_switch_capture(relay);
        fs::write(
            &settings_path,
            format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"{relay}"}}}}"#),
        )
        .unwrap();
        seed_installed_rtk();

        let relaunch = super::apply_client_setup("claude_code").expect("relaunch");
        assert!(relaunch.replaced_base_url.is_none());
        assert!(
            !super::load_setup_state()
                .preserved_base_urls
                .contains_key("claude_code"),
            "the reconciler's relay was preserved as a gateway"
        );

        // cc-switch -> Claude Official mid-session, then the hourly repair.
        crate::tool_manager::clear_cc_switch_capture();
        fs::write(&settings_path, r#"{"env":{}}"#).unwrap();
        super::apply_client_setup("claude_code").expect("repair");
        super::clear_client_setups().expect("quit");
        let after_quit = read_settings_json(&settings_path);
        assert!(
            after_quit["env"]["ANTHROPIC_BASE_URL"].is_null(),
            "quit put the relay back over Claude Official, got:\n{after_quit:#}"
        );
    }

    /// The backend's cc-switch reconciler rewrites settings.json only while
    /// Headroom routes it (`cc_switch_routed_path`). Ungated, turning the
    /// connector off handed the relay back and the reconciler took it again
    /// within 0.3s: the card said disconnected, every request still went
    /// through Headroom, and nothing short of quitting took Claude off it.
    #[test]
    #[serial_test::serial]
    fn claude_routing_gates_the_cc_switch_reconciler() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        seed_installed_rtk();
        let routed = crate::tool_manager::cc_switch_routed_path;

        for client in ["claude_code", "vscode"] {
            super::apply_client_setup(client).expect("apply");
            assert!(routed().exists(), "{client} apply left the reconciler off");
            super::disable_client_setup(client).expect("disable");
            assert!(
                !routed().exists(),
                "{client} disable left the reconciler on"
            );
        }
    }

    /// A kept capture is only valid while settings.json still names that
    /// provider. Switching cc-switch to Claude Official while Headroom is closed
    /// leaves no base URL; routing Claude through Headroom again has to drop the
    /// capture before it writes the intercept URL, or the next backend reseeds
    /// the relay and Anthropic OAuth traffic follows it there.
    #[test]
    #[serial_test::serial]
    fn routing_claude_drops_a_cc_switch_capture_the_user_moved_off() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings_path = home.path().join(".claude").join("settings.json");
        fs::write(&settings_path, r#"{"env":{}}"#).unwrap();
        write_cc_switch_capture("https://api.relay.example/anthropic");
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("apply");
        assert!(
            crate::tool_manager::cc_switch_captured_upstream().is_none(),
            "stale capture survived a re-route over Claude Official"
        );
        super::disable_client_setup("claude_code").expect("disable");
        assert!(read_settings_json(&settings_path)["env"]["ANTHROPIC_BASE_URL"].is_null());
    }

    /// Quit and pause go through disable_client_setup, whose shell-profile step
    /// ran first under `?`: a locked or immutable rc file returned before
    /// settings.json was restored, so Claude Code stayed on the dead port after
    /// Headroom exited. The shell step is now best-effort and runs last.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn disable_restores_claude_settings_even_when_the_shell_step_fails() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings_path = home.path().join(".claude").join("settings.json");
        fs::write(&settings_path, r#"{"hooks": {}}"#).unwrap();
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");

        // A read-only home makes the shell step fail (no backup can be
        // written next to the rc file; an unreadable rc is skipped instead).
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let disabled = super::disable_client_setup("claude_code");
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o755)).unwrap();
        disabled.expect("disable tolerates a shell cleanup failure");
        let after = read_settings_json(&settings_path);
        assert!(
            after["env"]["ANTHROPIC_BASE_URL"].is_null(),
            "base url left on the dead port, got:\n{after:#}"
        );
        assert!(
            !serde_json::to_string(&after["hooks"])
                .unwrap()
                .contains("headroom-claude-guard.py"),
            "guard hook left behind, got:\n{after:#}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn markitdown_read_hook_survives_pause_resume_and_rtk_toggle() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");
        let md = home.path().join("md");
        super::enable_markitdown_integration(&md, &md, &md).expect("enable markitdown");
        let settings_path = home.path().join(".claude").join("settings.json");
        let registered = || {
            read_settings_json(&settings_path)["hooks"]["PreToolUse"]
                .as_array()
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| e.to_string().contains("headroom-markitdown-read.sh"))
                })
        };
        assert!(registered());

        // Pause (and every quit) strips it; resume must put it back.
        super::disable_client_setup("claude_code").expect("disable");
        assert!(!registered());
        super::apply_client_setup("claude_code").expect("re-apply");
        assert!(registered(), "resume lost the MarkItDown Read hook");

        // Turning RTK off is not turning MarkItDown off.
        let (rtk, python) = (super::default_headroom_rtk_path(), home.path().join("py"));
        super::set_rtk_enabled(false, &rtk, &python).expect("rtk off");
        assert!(
            registered(),
            "RTK off took the MarkItDown Read hook with it"
        );

        // Turning MarkItDown off removes the script, so apply leaves it off.
        super::disable_markitdown_integration(&md).expect("disable markitdown");
        super::apply_client_setup("claude_code").expect("apply after md off");
        assert!(!registered());
    }

    #[test]
    #[serial_test::serial]
    fn claude_apply_failing_after_the_settings_write_still_persists_the_captured_base_url() {
        // Review of A-2: settings.json already holds Headroom's URL once the
        // env write lands, so a later failing step (here the guard script)
        // must not drop the captured gateway, or the next launch captures
        // nothing and quit deletes it for good.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let gateway = "https://gateway.corp.example/anthropic";
        fs::write(
            home.path().join(".claude").join("settings.json"),
            format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"{gateway}"}}}}"#),
        )
        .unwrap();
        seed_installed_rtk();
        // A directory where the guard script goes makes that step fail.
        fs::create_dir_all(super::claude_guard_hook_path().join("blocker")).unwrap();

        assert!(super::apply_client_setup("claude_code").is_err());
        assert_eq!(
            super::load_setup_state()
                .preserved_base_urls
                .get("claude_code")
                .map(String::as_str),
            Some(gateway)
        );
    }

    #[test]
    #[serial_test::serial]
    fn claude_connect_and_disconnect_survive_an_unparseable_vscode_settings_file() {
        // Audit #11: VS Code tolerates a settings.json it cannot parse (here a
        // pasted shell command), but the legacy base-URL cleanup refused it and
        // aborted connect after the routing write, and disconnect before the
        // hooks were stripped and the client was marked off.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        let vscode = home
            .path()
            .join("Library/Application Support/Code/User/settings.json");
        fs::create_dir_all(vscode.parent().unwrap()).unwrap();
        fs::write(&vscode, "cd /x\n").unwrap();

        super::apply_client_setup("claude_code").expect("apply");
        assert!(super::claude_guard_registered().unwrap());
        assert!(super::load_setup_state()
            .configured_clients
            .contains_key("claude_code"));

        super::disable_client_setup("claude_code").expect("disable");
        assert!(!super::claude_guard_registered().unwrap());
        assert!(!super::load_setup_state()
            .configured_clients
            .contains_key("claude_code"));
        assert_eq!(fs::read_to_string(&vscode).unwrap(), "cd /x\n");
    }

    #[test]
    #[serial_test::serial]
    fn claude_connect_routes_through_a_whitespace_only_settings_file() {
        // Audit #98: a 0-byte or whitespace-only settings.json (a `touch`, or
        // a writer that died mid-write) failed both parsers, so setup refused
        // it as "potentially valid user settings" until fixed by hand.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "  \n").unwrap();

        super::apply_client_setup("claude_code").expect("apply");
        assert_eq!(
            read_settings_json(&settings)["env"]["ANTHROPIC_BASE_URL"],
            super::HEADROOM_ANTHROPIC_BASE_URL
        );
    }

    /// Runs `write` on another thread while this one holds the setup write
    /// lock, as an apply in flight on the launch-restore thread does, and
    /// asserts the write waits for it instead of interleaving.
    fn assert_waits_for_setup_writes(write: impl FnOnce() + Send + 'static) {
        let in_flight = super::setup_write_lock();
        let (done_tx, done) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            write();
            let _ = done_tx.send(());
        });
        assert_eq!(
            done.recv_timeout(std::time::Duration::from_millis(300)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "wrote while another setup write was in flight"
        );
        drop(in_flight);
        writer.join().expect("writer thread");
        done.recv().expect("write finished");
    }

    #[test]
    #[serial_test::serial]
    fn setup_state_toggles_wait_for_an_apply_in_flight() {
        // Audit #86: each writer loads client-setup.json, changes one field and
        // writes the whole state back, so a toggle landing inside a launch
        // restore's apply was overwritten by the apply's stale copy (the
        // statusline or RTK opt-out silently reverted, a connector lost its
        // configured stamp).
        let home = TestHome::new();
        let tools = home.path().to_path_buf();
        assert_waits_for_setup_writes(|| super::set_statusline_enabled(false).unwrap());
        assert_waits_for_setup_writes(|| super::set_auto_learn_enabled(false).unwrap());
        assert_waits_for_setup_writes(|| super::set_usage_data_enabled(false).unwrap());
        assert_waits_for_setup_writes(move || {
            super::set_rtk_enabled(false, &tools, &tools).unwrap()
        });
        assert_waits_for_setup_writes(|| super::disable_client_setup("claude_code").unwrap());
        let state = super::load_setup_state();
        assert!(state.statusline_disabled && state.auto_learn_disabled && state.rtk_disabled);
        assert!(super::is_usage_data_disabled());
    }

    #[test]
    #[serial_test::serial]
    fn launch_refreshes_of_claude_settings_wait_for_an_apply_in_flight() {
        // Audit #134: the warm-runtime thread's RTK hook and MarkItDown writes
        // rewrote ~/.claude/settings.json from a copy read before the restore
        // thread's apply wrote its routing env and hooks, dropping them.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        seed_installed_rtk();
        let py = home.path().join("python3");
        let shim = home.path().join("markitdown");
        let (rtk, rtk_py) = (super::default_headroom_rtk_path(), py.clone());
        assert_waits_for_setup_writes(move || {
            super::ensure_rtk_integrations(&rtk, &rtk_py).unwrap();
        });
        let (md, md_shim, md_py) = (shim.clone(), shim.clone(), py.clone());
        assert_waits_for_setup_writes(move || {
            super::refresh_markitdown_integration(&md, &md_shim, &[], &md_py).unwrap()
        });
        let (md, md_shim, md_py) = (shim.clone(), shim.clone(), py.clone());
        assert_waits_for_setup_writes(move || {
            super::enable_markitdown_integration(&md, &md_shim, &md_py).unwrap();
        });
        assert_waits_for_setup_writes(move || {
            super::disable_markitdown_integration(&shim).unwrap();
        });
        assert!(super::claude_settings_hook_matches("headroom-rtk-rewrite.sh").unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn apply_without_custom_base_url_does_not_report_takeover() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();

        let result = super::apply_client_setup("claude_code").expect("apply");
        assert!(result.replaced_base_url.is_none());
        assert!(super::load_setup_state().preserved_base_urls.is_empty());

        // Disable deletes the key (nothing to restore).
        super::disable_client_setup("claude_code").expect("disable");
        let settings_path = home.path().join(".claude").join("settings.json");
        if settings_path.exists() {
            let after = read_settings_json(&settings_path);
            assert!(after["env"]["ANTHROPIC_BASE_URL"].is_null());
        }
    }

    #[test]
    #[serial_test::serial]
    fn verify_claude_code_passes_when_rtk_deliberately_disabled() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks": {}}"#,
        )
        .unwrap();

        super::apply_client_setup("claude_code").expect("apply_client_setup succeeds");

        // User turns RTK off: this strips the RTK PATH block + hook but leaves
        // ANTHROPIC_BASE_URL routing intact, and persists the opt-out.
        super::set_rtk_enabled(false, home.path(), home.path()).expect("disable RTK");

        let hook_path = home
            .path()
            .join(".claude")
            .join("hooks")
            .join("headroom-rtk-rewrite.sh");
        assert!(!hook_path.exists(), "RTK hook removed when RTK disabled");

        // Routing config is still present, so Claude Code must verify green
        // even though the RTK pieces are gone.
        let verification =
            super::verify_client_setup("claude_code").expect("verify_client_setup succeeds");
        assert!(
            verification.verified,
            "claude_code verifies on routing alone when RTK is disabled, failures: {:?}",
            verification.failures
        );
        assert!(
            verification.failures.iter().all(|f| !f.contains("RTK")),
            "no RTK failures reported when RTK is disabled, got: {:?}",
            verification.failures
        );
    }

    #[test]
    #[serial_test::serial]
    fn claude_guard_under_another_python_is_stale_not_missing() {
        // RUST-GS: the expected guard command embeds the interpreter, which
        // `guard_python_command` re-probes each process. A guard written under
        // the other one still runs: it must verify, and repair must rewrite it
        // without reporting it, while a deleted script still fails.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings = home.path().join(".claude").join("settings.json");
        fs::write(&settings, r#"{"hooks": {}}"#).unwrap();
        super::apply_client_setup("claude_code").expect("apply succeeds");

        let current = super::claude_guard_command();
        let raw = fs::read_to_string(&settings).unwrap();
        let script = super::claude_guard_hook_path().display().to_string();
        let other = super::join_guard_command("\"/other/python3\"", &script, false, false);
        let mut value: Value = serde_json::from_str(&raw).unwrap();
        for entry in value["hooks"]["SessionStart"].as_array_mut().unwrap() {
            for hook in entry["hooks"].as_array_mut().unwrap() {
                if hook["command"] == Value::String(current.clone()) {
                    hook["command"] = Value::String(other.clone());
                }
            }
        }
        fs::write(&settings, value.to_string()).unwrap();

        let stale = super::verify_client_setup("claude_code").expect("verify runs");
        assert!(stale.failures.is_empty(), "{:?}", stale.failures);
        assert!(stale
            .checks
            .iter()
            .any(|c| c == super::CLAUDE_GUARD_STALE_COMMAND));
        assert!(
            !super::repair_client_setup_now("claude_code"),
            "re-applied unreported"
        );
        assert!(
            super::claude_guard_registered().unwrap(),
            "current command back"
        );
        assert!(!fs::read_to_string(&settings)
            .unwrap()
            .contains("/other/python3"));

        fs::remove_file(super::claude_guard_hook_path()).unwrap();
        let missing = super::verify_client_setup("claude_code").expect("verify runs");
        assert_eq!(
            missing.failures,
            vec![super::CLAUDE_GUARD_SCRIPT_MISSING.to_string()]
        );
    }

    #[test]
    #[serial_test::serial]
    fn verify_claude_code_passes_when_rtk_not_installed() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude").join("settings.json"),
            r#"{"hooks": {}}"#,
        )
        .unwrap();

        // Clean install with RTK auto-install removed: routing is configured but
        // the managed RTK binary was never dropped on disk and the user never
        // toggled RTK off (rtk_disabled stays false). Claude Code must still
        // verify green on routing alone.
        super::apply_client_setup("claude_code").expect("apply_client_setup succeeds");

        assert!(
            !super::default_headroom_rtk_path().exists(),
            "RTK binary must be absent for this test"
        );
        let state = super::load_setup_state();
        assert!(
            !state.rtk_disabled,
            "rtk_disabled stays false when untoggled"
        );

        let verification =
            super::verify_client_setup("claude_code").expect("verify_client_setup succeeds");
        assert!(
            verification.verified,
            "claude_code verifies on routing alone when RTK isn't installed, failures: {:?}",
            verification.failures
        );
        assert!(
            verification.failures.iter().all(|f| !f.contains("RTK")),
            "no RTK failures reported when RTK isn't installed, got: {:?}",
            verification.failures
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn rtk_hook_stands_down_for_an_rtk_whose_exit_code_is_no_verdict() {
        // rtk 0.33.1 exits 0 on every rewrite, so a hook that trusts exit 0
        // auto-allowed `git status; rm -rf ~/x` for anyone whose upgrade failed.
        let home = TestHome::new();
        let bin_dir = home.path().join("managed-bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let rtk = bin_dir.join("rtk");
        let python = bin_dir.join("python3");
        fs::write(&python, "#!/bin/sh\n").unwrap();
        let hook = home.path().join(".claude/hooks/headroom-rtk-rewrite.sh");
        for (version, rewrites) in [("0.33.1", false), ("0.37.2", true), ("0.48.0", true)] {
            fs::write(&rtk, format!("#!/bin/sh\necho 'rtk {version}'\n")).unwrap();
            fs::set_permissions(
                &rtk,
                <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .unwrap();
            super::ensure_claude_code_rtk_hook(&rtk, &python).expect("write hook");
            let body = fs::read_to_string(&hook).expect("hook written");
            assert_eq!(
                body.contains(" rewrite "),
                rewrites,
                "rtk {version}: {body}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn ensure_rtk_integrations_writes_codex_nudge_and_disable_removes_it() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(home.path().join(".claude").join("settings.json"), "{}").unwrap();

        // Mark Codex as a configured client so the AGENTS.md nudge path runs.
        let mut state = super::load_setup_state();
        state
            .configured_clients
            .insert("codex_cli".into(), "now".into());
        super::write_setup_state(&state).unwrap();

        // Fake managed rtk + python binaries so the binary-present guard passes.
        let bin_dir = home.path().join("managed-bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let rtk = bin_dir.join("rtk");
        fs::write(&rtk, "#!/bin/sh\n").unwrap();
        let python = bin_dir.join("python3");
        fs::write(&python, "#!/bin/sh\n").unwrap();

        super::ensure_rtk_integrations(&rtk, &python).expect("ensure_rtk_integrations");

        let agents = home.path().join(".codex").join("AGENTS.md");
        let body = fs::read_to_string(&agents).expect("AGENTS.md written");
        assert!(
            body.contains("Headroom RTK"),
            "nudge heading present: {body}"
        );
        assert!(
            body.contains(&rtk.display().to_string()),
            "nudge references the managed rtk path: {body}"
        );

        // Disabling RTK must remove the managed block.
        super::set_rtk_enabled(false, &rtk, &python).expect("disable rtk");
        let after = fs::read_to_string(&agents).unwrap_or_default();
        assert!(
            !after.contains("Headroom RTK"),
            "nudge removed on disable: {after}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_claude_code_is_byte_idempotent() {
        // Regression: a second apply used to add blank-line padding between
        // managed blocks, so byte-exact idempotency now holds and is
        // asserted here.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("first apply");
        let zshrc_after_first = fs::read_to_string(home.path().join(".zshrc")).unwrap();
        let zshenv_after_first = fs::read_to_string(home.path().join(".zshenv")).unwrap();
        let settings_after_first =
            fs::read_to_string(home.path().join(".claude").join("settings.json")).unwrap();
        let hook_after_first = fs::read_to_string(
            home.path()
                .join(".claude")
                .join("hooks")
                .join("headroom-rtk-rewrite.sh"),
        )
        .unwrap();

        super::apply_client_setup("claude_code").expect("second apply");
        let zshrc_after_second = fs::read_to_string(home.path().join(".zshrc")).unwrap();
        let zshenv_after_second = fs::read_to_string(home.path().join(".zshenv")).unwrap();
        let settings_after_second =
            fs::read_to_string(home.path().join(".claude").join("settings.json")).unwrap();
        let hook_after_second = fs::read_to_string(
            home.path()
                .join(".claude")
                .join("hooks")
                .join("headroom-rtk-rewrite.sh"),
        )
        .unwrap();

        assert_eq!(zshrc_after_first, zshrc_after_second, "zshrc byte-stable");
        assert_eq!(
            zshenv_after_first, zshenv_after_second,
            "zshenv byte-stable"
        );
        assert_eq!(
            settings_after_first, settings_after_second,
            "settings.json byte-stable"
        );
        assert_eq!(
            hook_after_first, hook_after_second,
            "hook script byte-stable"
        );

        // Sanity: each managed block still appears exactly once.
        let combined = format!("{zshrc_after_second}\n{zshenv_after_second}");
        assert_eq!(
            combined.matches("# >>> headroom:claude_code >>>").count(),
            1
        );
        assert_eq!(
            combined.matches("# >>> headroom:managed_rtk >>>").count(),
            1
        );
    }

    #[test]
    #[serial_test::serial]
    fn disable_then_clear_claude_code_removes_traces() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("apply");
        let hook_path = home
            .path()
            .join(".claude")
            .join("hooks")
            .join("headroom-rtk-rewrite.sh");
        assert!(hook_path.exists(), "hook present after apply");

        super::disable_client_setup("claude_code").expect("disable");

        // Hook script removed.
        assert!(!hook_path.exists(), "hook removed after disable");

        // Shell blocks removed.
        let zshrc = fs::read_to_string(home.path().join(".zshrc")).unwrap();
        let zshenv = fs::read_to_string(home.path().join(".zshenv")).unwrap();
        let combined = format!("{zshrc}\n{zshenv}");
        assert!(
            !combined.contains("headroom:claude_code"),
            "claude_code shell block removed, got:\n{combined}"
        );

        // settings.json no longer points env at the proxy and no longer carries
        // the Headroom hook entry.
        let settings = read_settings_json(&home.path().join(".claude").join("settings.json"));
        assert!(
            settings["env"]["ANTHROPIC_BASE_URL"].is_null(),
            "ANTHROPIC_BASE_URL stripped from settings.json env, got: {settings}"
        );
        let still_has_headroom_hook =
            claude_hook_present_in_value(&settings, "headroom-rtk-rewrite.sh");
        assert!(
            !still_has_headroom_hook,
            "Headroom hook entry stripped from settings.json, got: {settings}"
        );

        // clear_client_setups runs disable across all clients without error,
        // and the setup state file is left without a `claude_code` entry.
        super::clear_client_setups().expect("clear");
        let post = super::load_setup_state();
        assert!(
            !post.configured_clients.contains_key("claude_code"),
            "claude_code dropped from configured_clients, got: {:?}",
            post.configured_clients
        );
    }

    #[test]
    #[serial_test::serial]
    fn clear_client_setups_twice_preserves_remembered_snapshot() {
        // Regression: pause (first clear) moves configured -> remembered; the
        // quit-time second clear used to wipe remembered_clients because the
        // re-save was skipped while configured was empty — so a pause
        // followed by Cmd-Q permanently lost every connector.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();

        super::apply_client_setup("claude_code").expect("apply");

        super::clear_client_setups().expect("first clear (pause)");
        let state = super::load_setup_state();
        assert!(state.configured_clients.is_empty());
        assert!(
            state.remembered_clients.contains_key("claude_code"),
            "pause snapshots the configured client, got: {:?}",
            state.remembered_clients
        );

        super::clear_client_setups().expect("second clear (quit)");
        let state = super::load_setup_state();
        assert!(
            state.remembered_clients.contains_key("claude_code"),
            "quit-time clear after a pause must keep the restore snapshot, got: {:?}",
            state.remembered_clients
        );
    }

    /// Finding 38: a 6767 holder Headroom cannot identify (another signed-in
    /// user's Headroom looks exactly like this) must stop receiving this
    /// user's credentials. The bind loop unwires every client the way a pause
    /// does, and reclaiming the port wires them back, unless the user paused
    /// in between.
    #[test]
    #[serial_test::serial]
    fn clients_unwired_for_an_unidentified_port_holder_return_only_without_a_pause() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");

        assert!(super::unwire_clients_for_port_holder(), "a wired client");
        assert!(!super::is_claude_code_enabled(), "unwired like a pause");
        assert!(
            !super::unwire_clients_for_port_holder(),
            "nothing left wired, so a repeat claims nothing"
        );
        // Resume, a provider save's restart and the Connectors page all wire
        // through apply_client_setup; none may hand the holder the bearer.
        super::restore_client_setups();
        assert!(!super::is_claude_code_enabled(), "restore wired it back");
        assert!(super::apply_client_setup("claude_code").is_err());
        assert!(!super::is_claude_code_enabled(), "a manual apply wired it");
        super::rewire_clients_after_port_reclaimed();
        assert!(super::is_claude_code_enabled(), "the bind wires it back");

        assert!(super::unwire_clients_for_port_holder());
        super::clear_client_setups().expect("user pause");
        super::rewire_clients_after_port_reclaimed();
        assert!(
            !super::is_claude_code_enabled(),
            "a pause after the unwire is the user's call; the bind must not undo it"
        );
    }

    /// The bind loop retries every 15s while the holder stays. A client whose
    /// disable failed stays in configured_clients, and that used to rerun the
    /// whole teardown of every client on each retry. Once unwired, a repeat
    /// does nothing.
    #[test]
    #[serial_test::serial]
    fn a_repeat_port_holder_unwire_does_not_redo_the_teardown() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        seed_installed_rtk();
        super::apply_client_setup("claude_code").expect("apply");

        // What a failed disable leaves: unwired, yet still configured.
        super::set_clients_unwired_for_port_holder(true);
        let repeat = super::unwire_clients_for_port_holder();
        let still_wired = super::is_claude_code_enabled();
        super::set_clients_unwired_for_port_holder(false);
        assert!(!repeat, "already unwired, so a repeat claims nothing");
        assert!(still_wired, "a repeat must not rerun the teardown");
    }

    #[test]
    #[serial_test::serial]
    fn gate_exemption_holds_across_the_quit_to_restore_window() {
        // Quit clears configured_clients into the remembered snapshot, and the
        // launch-time gate runs before restore_client_setups re-applies it.
        // Reading configured alone chose full bypass for a Codex user (W4).
        let _home = TestHome::new();
        assert!(!super::any_gate_exempt_client_enabled());
        super::apply_client_setup("codex").expect("apply");
        assert!(super::any_gate_exempt_client_enabled());

        super::clear_client_setups().expect("quit-time clear");
        assert!(super::load_setup_state().configured_clients.is_empty());
        assert!(
            super::any_gate_exempt_client_enabled(),
            "restore still pending"
        );

        // A user disable drops it from both sets, so the exemption ends.
        super::disable_client_setup("codex").expect("disable");
        assert!(!super::any_gate_exempt_client_enabled());

        for id in ["codex", "codex_gui", "codex_cli", "opencode", "grok_build"] {
            assert!(super::is_gate_exempt_client(id), "{id}");
        }
        assert!(!super::is_gate_exempt_client("claude_code"));
        assert!(!super::is_gate_exempt_client("vscode"));
    }

    #[test]
    #[serial_test::serial]
    fn list_client_connectors_carries_verification_only_for_enabled_clients() {
        // The connector panel keys its status line off these two fields: an
        // enabled client must arrive with its checks attached (the panel has
        // no other way to say what is wrong), and a disabled one must carry
        // none, so the list never implies it verified something it skipped.
        let _home = TestHome::new();
        super::apply_client_setup("codex").expect("apply_client_setup succeeds");

        // installed: false is the Codex desktop-app/IDE user -- they share
        // ~/.codex/config.toml with the CLI, so the connector is configurable
        // and verifiable without the CLI binary on disk.
        let detected = vec![crate::models::ClientStatus {
            id: "codex".to_string(),
            name: "Codex".to_string(),
            installed: false,
            configured: true,
            health: crate::models::ClientHealth::Healthy,
            notes: Vec::new(),
        }];
        let connectors = super::list_client_connectors(&detected).expect("listing succeeds");

        let codex = connectors
            .iter()
            .find(|connector| connector.client_id == "codex")
            .expect("codex connector listed");
        assert!(!codex.installed);
        assert!(codex.enabled);
        assert!(codex.verified);
        let verification = codex
            .verification
            .as_ref()
            .expect("verification attached for an enabled client");
        assert_eq!(verification.verified, codex.verified);
        assert!(
            verification
                .checks
                .iter()
                .any(|check| check.contains("config.toml")),
            "config.toml check reported, got: {:?}",
            verification.checks
        );

        let grok = connectors
            .iter()
            .find(|connector| connector.client_id == "grok_build")
            .expect("grok connector listed");
        assert!(!grok.enabled);
        assert!(grok.verification.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn apply_then_verify_then_disable_codex_round_trip() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();

        let result = super::apply_client_setup("codex").expect("apply_client_setup succeeds");
        assert!(result.applied);
        assert_eq!(result.client_id, "codex");

        // Managed provider block lands in ~/.codex/config.toml.
        let config_toml = home.path().join(".codex").join("config.toml");
        let toml = fs::read_to_string(&config_toml).expect("codex config.toml written");
        assert!(
            toml.contains("# >>> headroom:codex_cli >>>"),
            "managed marker present, got:\n{toml}"
        );
        assert!(
            toml.contains("model_provider = \"headroom\""),
            "model_provider set, got:\n{toml}"
        );
        assert!(
            toml.contains("base_url = \"http://127.0.0.1:6767/v1\""),
            "provider base_url points at proxy, got:\n{toml}"
        );
        assert!(
            toml.contains("supports_websockets = false"),
            "Codex must use the reliable HTTP Responses transport, got:\n{toml}"
        );
        assert!(
            toml.contains("requires_openai_auth = true"),
            "Codex attaches no credential without the flag, got:\n{toml}"
        );

        // OPENAI_BASE_URL exported from a managed shell block, only while the
        // intercept answers.
        let zshrc = fs::read_to_string(home.path().join(".zshrc")).unwrap();
        assert!(
            zshrc.contains(&super::intercept_export_line(
                "OPENAI_BASE_URL",
                "http://127.0.0.1:6767/v1"
            )),
            "codex_cli block with the probed export, got:\n{zshrc}"
        );

        // verify_client_setup reports the configured checks and passes.
        let verification =
            super::verify_client_setup("codex").expect("verify_client_setup succeeds");
        assert_eq!(verification.client_id, "codex");
        assert!(
            verification.failures.is_empty(),
            "no verification failures, got: {:?}",
            verification.failures
        );
        assert!(
            verification
                .checks
                .iter()
                .any(|c| c.contains("config.toml")),
            "verification reports the toml check, got: {:?}",
            verification.checks
        );

        // Disable strips both the toml block and the shell export.
        super::disable_client_setup("codex").expect("disable_client_setup succeeds");
        let toml_after = fs::read_to_string(&config_toml).unwrap_or_default();
        assert!(
            !toml_after.contains("# >>> headroom:codex_cli >>>"),
            "managed block removed on disable, got:\n{toml_after}"
        );
        let combined_after = format!(
            "{}\n{}",
            fs::read_to_string(home.path().join(".zshrc")).unwrap(),
            fs::read_to_string(home.path().join(".zshenv")).unwrap(),
        );
        assert!(
            !combined_after.contains("OPENAI_BASE_URL"),
            "shell export removed on disable, got:\n{combined_after}"
        );
    }

    /// Upgrading users carry an rc7 codex_cli block (an unconditional export
    /// that overrode the user's own OPENAI_BASE_URL and outlived quit) or none
    /// (A-1). The first apply writes the probed block in its place, leaves the
    /// user's earlier line alone (the block never exports over it), records
    /// the targets for pause and restore, and a second apply changes nothing.
    #[test]
    #[serial_test::serial]
    fn apply_codex_rewrites_its_shell_export_to_the_probed_form() {
        let user_line = "export OPENAI_BASE_URL=http://localhost:11434/v1\n";
        let block = format!(
            "# >>> headroom:codex_cli >>>\n{}\n# <<< headroom:codex_cli <<<\n",
            super::codex_shell_block(crate::proxy_intercept::INTERCEPT_PORT)
        );
        for older in [
            "# >>> headroom:codex_cli >>>\nexport OPENAI_BASE_URL=http://127.0.0.1:6767/v1\n# <<< headroom:codex_cli <<<\n",
            "",
        ] {
            let home = TestHome::new();
            let zshrc = home.path().join(".zshrc");
            fs::write(&zshrc, format!("{user_line}{older}")).unwrap();

            let result = super::apply_client_setup("codex").expect("apply");
            assert!(
                result.verification.verified,
                "{:?}",
                result.verification.failures
            );
            assert!(
                result
                    .verification
                    .checks
                    .iter()
                    .any(|check| check.contains("export in managed shell block")),
                "{:?}",
                result.verification.checks
            );
            let rc = fs::read_to_string(&zshrc).unwrap();
            assert_eq!(rc, format!("{user_line}{block}"));
            assert!(super::load_setup_state()
                .managed_shell_files
                .get("codex_cli")
                .is_some_and(|files| files.iter().any(|f| f.ends_with(".zshrc"))));

            let again = super::apply_client_setup("codex").expect("re-apply");
            assert!(
                !again.changed_files.iter().any(|f| f.ends_with(".zshrc")),
                "{:?}",
                again.changed_files
            );
            assert_eq!(fs::read_to_string(&zshrc).unwrap(), rc);
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_then_verify_then_disable_opencode_round_trip() {
        let _home = TestHome::new(); // env guard

        let result = super::apply_client_setup("opencode").expect("apply_client_setup succeeds");
        assert!(result.applied);
        assert_eq!(result.client_id, "opencode");

        // Resolve via the same function the apply path uses (XDG_CONFIG_HOME,
        // else ~/.config, on every platform).
        let config_path = super::opencode_config_path();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).expect("config written"))
                .expect("valid json");
        for provider in ["anthropic", "openai"] {
            assert_eq!(
                config["provider"][provider]["options"]["baseURL"],
                serde_json::json!("http://127.0.0.1:6767/v1"),
                "{provider} routed through proxy, got:\n{config:#}"
            );
        }

        let verification =
            super::verify_client_setup("opencode").expect("verify_client_setup succeeds");
        assert!(
            verification.failures.is_empty(),
            "{:?}",
            verification.failures
        );

        super::disable_client_setup("opencode").expect("disable_client_setup succeeds");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            after.get("provider").is_none(),
            "provider husk removed on disable, got:\n{after:#}"
        );
        assert!(!super::is_configured(
            &super::load_setup_state(),
            "opencode"
        ));
    }

    #[test]
    #[serial_test::serial]
    fn opencode_apply_preserves_existing_base_url_and_restores_on_disable() {
        let _home = TestHome::new(); // env guard
        let config_dir = super::opencode_config_dir();
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("opencode.json");
        fs::write(
            &config_path,
            r#"{
  "theme": "tokyonight",
  "provider": {
    "anthropic": {
      "options": {
        "baseURL": "https://gateway.corp.example/v1",
        "timeout": 5000
      }
    }
  }
}"#,
        )
        .unwrap();

        super::apply_client_setup("opencode").expect("apply succeeds");

        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(config["theme"], serde_json::json!("tokyonight"));
        assert_eq!(
            config["provider"]["anthropic"]["options"]["timeout"],
            serde_json::json!(5000),
            "sibling option keys preserved"
        );
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            serde_json::json!("http://127.0.0.1:6767/v1")
        );

        super::disable_client_setup("opencode").expect("disable succeeds");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            after["provider"]["anthropic"]["options"]["baseURL"],
            serde_json::json!("https://gateway.corp.example/v1"),
            "original gateway URL restored, got:\n{after:#}"
        );
        assert_eq!(after["theme"], serde_json::json!("tokyonight"));
    }

    #[test]
    #[serial_test::serial]
    fn opencode_apply_unwraps_stale_wrap_config() {
        let _home = TestHome::new(); // env guard
        let config_dir = super::opencode_config_dir();
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("opencode.json");
        // `headroom wrap opencode` killed before it could restore: its own
        // provider block, plus a native provider repointed at its proxy port.
        fs::write(
            &config_path,
            r#"{
  "theme": "tokyonight",
  "provider": {
    "headroom": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:8787/v1" }
    },
    "anthropic": {
      "options": { "baseURL": "http://127.0.0.1:8787/v1" }
    }
  }
}"#,
        )
        .unwrap();

        super::apply_client_setup("opencode").expect("apply unwraps instead of refusing");

        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            config["provider"].get("headroom").is_none(),
            "stale wrap provider removed, got:\n{config:#}"
        );
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            serde_json::json!("http://127.0.0.1:6767/v1")
        );
        assert_eq!(config["theme"], serde_json::json!("tokyonight"));

        super::disable_client_setup("opencode").expect("disable succeeds");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            after
                .pointer("/provider/anthropic/options/baseURL")
                .is_none(),
            "wrap's dead port must not be restored as the user's own, got:\n{after:#}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn opencode_apply_installs_transport_plugin_and_disable_removes_it() {
        let _home = TestHome::new(); // env guard

        super::apply_client_setup("opencode").expect("apply succeeds");

        let plugin_path = super::opencode_plugin_install_path();
        assert!(plugin_path.is_file(), "vendored plugin written to app data");
        let config_path = super::opencode_config_path();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let plugins = config["plugin"].as_array().expect("plugin array present");
        assert!(
            plugins
                .iter()
                .any(|v| v.as_str() == Some(&plugin_path.display().to_string())),
            "plugin path registered, got:\n{config:#}"
        );

        super::disable_client_setup("opencode").expect("disable succeeds");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            after.get("plugin").is_none(),
            "plugin entry removed on disable, got:\n{after:#}"
        );
        assert!(!plugin_path.exists(), "plugin file removed on disable");
    }

    #[test]
    fn strip_jsonc_removes_comments_and_trailing_commas() {
        let src = r#"{
  // line comment
  "a": "value with // not a comment",
  /* block
     comment */
  "b": [1, 2, /* inline */ 3,],
  "c": "trailing \" escape",
}"#;
        let parsed: serde_json::Value =
            serde_json::from_str(&super::strip_jsonc(src)).expect("stripped source parses");
        assert_eq!(
            parsed["a"],
            serde_json::json!("value with // not a comment")
        );
        assert_eq!(parsed["b"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn strip_jsonc_keeps_non_ascii_strings_intact() {
        // Bytes pushed as chars turned every non-ASCII character into Latin-1
        // mojibake, which the OpenCode rewrite then saved to disk.
        let src = "{\"p\": \"Pr\u{fc}fe - \u{65e5}\u{672c} // \u{e9}\", // c\u{f6}mment\n /* \u{e4} */ \"q\": 1,\n}";
        let parsed: serde_json::Value =
            serde_json::from_str(&super::strip_jsonc(src)).expect("stripped source parses");
        assert_eq!(
            parsed["p"],
            serde_json::json!("Pr\u{fc}fe - \u{65e5}\u{672c} // \u{e9}")
        );
        assert_eq!(parsed["q"], serde_json::json!(1));
    }

    #[test]
    #[serial_test::serial]
    fn opencode_apply_tolerates_jsonc_config() {
        let _home = TestHome::new(); // env guard
        let config_path = super::opencode_config_dir().join("opencode.jsonc");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            "{\n  // user comment (RUST-61: setup used to refuse this file)\n  \"theme\": \"dark\",\n}\n",
        )
        .unwrap();

        super::apply_client_setup("opencode").expect("apply succeeds on .jsonc with comments");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap())
                .expect("apply wrote strict json");
        assert_eq!(after["theme"], serde_json::json!("dark"), "user key kept");
        for provider in super::OPENCODE_MANAGED_PROVIDERS {
            assert_eq!(
                super::opencode_provider_base_url(&after, provider).as_deref(),
                Some(super::HEADROOM_OPENCODE_BASE_URL),
                "provider {provider} routed"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn opencode_disable_tolerates_comments_added_after_apply() {
        let _home = TestHome::new(); // env guard

        super::apply_client_setup("opencode").expect("apply succeeds");
        let config_path = super::opencode_config_path();
        let mut contents = fs::read_to_string(&config_path).unwrap();
        contents.insert_str(0, "// routed through headroom\n");
        fs::write(&config_path, &contents).unwrap();

        super::disable_client_setup("opencode").expect("disable succeeds despite comments");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap())
                .expect("disable wrote parseable json");
        assert!(
            after.get("provider").is_none(),
            "proxy URLs removed, got:\n{after:#}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn opencode_config_path_prefers_jsonc_when_present() {
        let _home = TestHome::new(); // env guard
        let config_dir = super::opencode_config_dir();
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("opencode.jsonc"), "{}").unwrap();

        super::apply_client_setup("opencode").expect("apply succeeds");
        let jsonc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config_dir.join("opencode.jsonc")).unwrap())
                .unwrap();
        assert_eq!(
            jsonc["provider"]["anthropic"]["options"]["baseURL"],
            serde_json::json!("http://127.0.0.1:6767/v1"),
            "jsonc file managed when it is the active config"
        );
        assert!(
            !config_dir.join("opencode.json").exists(),
            "no stray opencode.json created next to the active jsonc"
        );
    }

    #[test]
    #[serial_test::serial]
    fn grok_config_preserves_user_top_level_keys() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        let grok_dir = home.path().join(".grok");
        fs::create_dir_all(&grok_dir).unwrap();
        fs::write(grok_dir.join("config.toml"), "default_model = \"grok-4\"\n").unwrap();

        super::apply_client_setup("grok_build").expect("apply_client_setup succeeds");

        let toml = fs::read_to_string(grok_dir.join("config.toml")).unwrap();
        let key_pos = toml.find("default_model").expect("user key kept");
        let table_pos = toml
            .find("[model.grok-build]")
            .expect("managed table present");
        assert!(
            key_pos < table_pos,
            "top-level key must precede the managed table, got:\n{toml}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn grok_config_redirects_existing_grok_build_table_and_restores_on_disable() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        let grok_dir = home.path().join(".grok");
        fs::create_dir_all(&grok_dir).unwrap();
        fs::write(
            grok_dir.join("config.toml"),
            "[model.grok-build]\nbase_url = \"http://127.0.0.1:8787/v1\"\n",
        )
        .unwrap();

        super::apply_client_setup("grok_build").expect("apply_client_setup succeeds");

        let toml = fs::read_to_string(grok_dir.join("config.toml")).unwrap();
        assert_eq!(
            toml.matches("[model.grok-build]").count(),
            1,
            "no duplicate table, got:\n{toml}"
        );
        assert!(
            toml.contains(
                "base_url = \"http://127.0.0.1:6767/v1\"  # was: http://127.0.0.1:8787/v1"
            ),
            "base_url redirected in place, got:\n{toml}"
        );

        let verification =
            super::verify_client_setup("grok_build").expect("verify_client_setup succeeds");
        assert!(
            verification.failures.is_empty(),
            "{:?}",
            verification.failures
        );

        super::disable_client_setup("grok_build").expect("disable_client_setup succeeds");
        let after = fs::read_to_string(grok_dir.join("config.toml")).unwrap();
        assert!(
            after.contains("base_url = \"http://127.0.0.1:8787/v1\""),
            "original base_url restored, got:\n{after}"
        );
        assert!(
            !after.contains("# was:"),
            "redirect comment removed, got:\n{after}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn grok_config_reads_commented_header_and_literal_base_url() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        let grok_dir = home.path().join(".grok");
        fs::create_dir_all(&grok_dir).unwrap();
        let config = grok_dir.join("config.toml");
        // A trailing comment on the header hid the table (a second
        // [model.grok-build] made the file invalid TOML), and a literal
        // ('single-quoted') base_url was overwritten with no `# was:` record.
        fs::write(
            &config,
            "[model.grok-build] # my gateway\nbase_url = 'https://gw.example/v1'\n",
        )
        .unwrap();

        super::apply_client_setup("grok_build").expect("apply_client_setup succeeds");
        let toml = fs::read_to_string(&config).unwrap();
        assert_eq!(
            toml.matches("[model.grok-build]").count(),
            1,
            "no duplicate table, got:\n{toml}"
        );
        assert!(
            toml.parse::<toml::Value>().is_ok(),
            "valid TOML, got:\n{toml}"
        );
        assert!(
            toml.contains("# was: https://gw.example/v1"),
            "previous base_url recorded, got:\n{toml}"
        );

        super::disable_client_setup("grok_build").expect("disable_client_setup succeeds");
        let after = fs::read_to_string(&config).unwrap();
        assert!(
            after.contains("base_url = \"https://gw.example/v1\""),
            "original base_url restored, got:\n{after}"
        );

        // A spelling the text scan still misses must not be turned into a
        // duplicate table: refuse and keep the user's file.
        let quoted = "[model.\"grok-build\"]\nbase_url = \"https://gw.example/v1\"\n";
        fs::write(&config, quoted).unwrap();
        assert!(super::apply_client_setup("grok_build").is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), quoted);
    }

    #[test]
    #[serial_test::serial]
    fn apply_then_verify_then_disable_grok_build_round_trip() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();

        let result = super::apply_client_setup("grok_build").expect("apply_client_setup succeeds");
        assert!(result.applied);
        assert_eq!(result.client_id, "grok_build");

        let config_toml = home.path().join(".grok").join("config.toml");
        let toml = fs::read_to_string(&config_toml).expect("grok config.toml written");
        assert!(
            toml.contains("# >>> headroom:grok_build_proxy >>>"),
            "managed marker present, got:\n{toml}"
        );
        assert!(
            toml.contains("base_url = \"http://127.0.0.1:6767/v1\""),
            "proxy base_url set, got:\n{toml}"
        );

        let zshrc = fs::read_to_string(home.path().join(".zshrc")).unwrap();
        let zshenv = fs::read_to_string(home.path().join(".zshenv")).unwrap();
        let combined = format!("{zshrc}\n{zshenv}");
        assert!(
            combined.contains("GROK_CLI_CHAT_PROXY_BASE_URL=http://127.0.0.1:6767/v1"),
            "GROK_CLI_CHAT_PROXY_BASE_URL exported, got:\n{combined}"
        );

        let verification =
            super::verify_client_setup("grok_build").expect("verify_client_setup succeeds");
        assert!(
            verification.failures.is_empty(),
            "{:?}",
            verification.failures
        );

        super::disable_client_setup("grok_build").expect("disable_client_setup succeeds");
        let toml_after = fs::read_to_string(&config_toml).unwrap_or_default();
        assert!(
            !toml_after.contains("# >>> headroom:grok_build_proxy >>>"),
            "managed block removed on disable, got:\n{toml_after}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_is_byte_idempotent() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();

        super::apply_client_setup("codex").expect("first apply");
        let config_toml = home.path().join(".codex").join("config.toml");
        let toml_first = fs::read_to_string(&config_toml).unwrap();
        let zshenv_first = fs::read_to_string(home.path().join(".zshenv")).unwrap();

        super::apply_client_setup("codex").expect("second apply");
        let toml_second = fs::read_to_string(&config_toml).unwrap();
        let zshenv_second = fs::read_to_string(home.path().join(".zshenv")).unwrap();

        assert_eq!(toml_first, toml_second, "config.toml byte-stable");
        assert_eq!(zshenv_first, zshenv_second, "zshenv byte-stable");
        assert_eq!(
            toml_second.matches("# >>> headroom:codex_cli >>>").count(),
            1,
            "managed block appears exactly once"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_writes_and_registers_guard() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();

        super::apply_client_setup("codex").expect("apply");

        let script = home
            .path()
            .join(".codex")
            .join("hooks")
            .join("headroom-codex-guard.py");
        assert!(script.exists(), "guard script written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script).unwrap().permissions().mode();
            assert!(
                mode & 0o111 != 0,
                "guard script is executable, got {mode:o}"
            );
        }

        let hooks: serde_json::Value =
            read_settings_json(&home.path().join(".codex").join("hooks.json"));
        // The registered command is platform-dependent (/usr/bin/python3 vs the
        // quoted managed python.exe), so assert against the real builder.
        let command = super::codex_guard_command();
        // SessionStart only: on UserPromptSubmit a nonzero exit blocks the prompt.
        let session_registered = hooks["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["hooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|h| h["command"] == serde_json::Value::String(command.clone()))
            });
        assert!(
            session_registered,
            "guard registered on SessionStart, got:\n{hooks:#}"
        );
        assert!(
            !hooks["hooks"]["UserPromptSubmit"]
                .to_string()
                .contains("headroom-codex-guard.py"),
            "guard must not register on UserPromptSubmit, got:\n{hooks:#}"
        );
        assert_eq!(
            hooks["hooks"]["SessionStart"][0]["matcher"],
            "startup|resume|clear|compact"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn guard_commands_do_not_hardcode_unix_python() {
        assert!(claude_guard_command().contains("python.exe"));
        assert!(codex_guard_command().contains("python.exe"));
        assert!(!claude_guard_command().starts_with("/usr/bin/python3"));
        assert!(!codex_guard_command().starts_with("/usr/bin/python3"));
    }

    #[test]
    fn hook_command_is_the_bare_script_path_on_unix() {
        let path = PathBuf::from("/home/g/.claude/hooks/headroom-rtk-rewrite.sh");
        let cmd = super::hook_shell_command(&path).expect("hook command");
        if cfg!(target_os = "windows") {
            // Claude Code runs hooks through bash on Windows: quoted
            // interpreter, quoted script, NO call operator (bash rejects a
            // leading `&` as a syntax error).
            assert!(!cmd.starts_with("& "), "{cmd}");
            assert!(cmd.ends_with("\"/home/g/.claude/hooks/headroom-rtk-rewrite.sh\""));
            assert!(cmd.contains("bash"), "{cmd}");
        } else {
            assert_eq!(cmd, "/home/g/.claude/hooks/headroom-rtk-rewrite.sh");
        }
    }

    /// Regression: Codex runs SessionStart hooks through PowerShell on Windows.
    /// A command that starts with a quoted interpreter path parses as a string
    /// literal, not a command, so the guard died with
    /// "SessionStart:startup hook error / Failed with non-blocking status code:
    /// At line:1 char:81" -- char 81 being the first character of the unquoted
    /// script path that followed the 79-char quoted python path plus a space.
    /// The call operator is what makes it a command, and the script path must
    /// be quoted because profile directories contain spaces.
    #[test]
    fn windows_guard_command_is_powershell_callable() {
        let cmd = super::join_guard_command(
            "\"C:\\Users\\garm\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\python.exe\"",
            "C:\\Users\\garm space\\.claude\\hooks\\headroom-claude-guard.py",
            true,
            true,
        );
        assert!(
            cmd.starts_with("& \""),
            "PowerShell needs the call operator before a quoted path, got: {cmd}"
        );
        assert!(
            cmd.ends_with("headroom-claude-guard.py\""),
            "script path must be quoted so spaces survive, got: {cmd}"
        );
        // Backslashes stay single: PowerShell escapes with a backtick, so
        // POSIX-style doubling would break every Windows path.
        assert!(
            !cmd.contains("\\\\"),
            "path must not be POSIX-escaped: {cmd}"
        );
    }

    #[test]
    fn unix_guard_command_quotes_only_a_spaced_script_path() {
        let cmd =
            super::join_guard_command("/usr/bin/python3", "/home/g/.claude/guard.py", false, false);
        assert_eq!(cmd, "/usr/bin/python3 /home/g/.claude/guard.py");
        // A home dir with a space would split the argument and python exits 2.
        let cmd = super::join_guard_command(
            "/usr/bin/python3",
            "/Users/Jane Doe/.claude/guard.py",
            false,
            false,
        );
        assert_eq!(cmd, "/usr/bin/python3 '/Users/Jane Doe/.claude/guard.py'");
    }

    /// Regression: on a Mac without the Command Line Tools /usr/bin/python3 is
    /// the xcode-select shim, and on some Linux distros it does not exist, so a
    /// hardcoded system interpreter failed the guard at every session start.
    /// Without a usable system python the guard runs on the managed runtime's
    /// interpreter, quoted because the macOS path has "Application Support".
    #[test]
    fn guard_python_falls_back_to_the_managed_interpreter_without_system_python() {
        let _home = TestHome::new();
        let managed =
            crate::tool_manager::ManagedRuntime::bootstrap_root(&crate::storage::app_data_dir())
                .managed_python();
        assert_eq!(
            super::guard_python_for(false),
            format!("\"{}\"", managed.display())
        );
        if !cfg!(target_os = "windows") {
            assert_eq!(super::guard_python_for(true), "/usr/bin/python3");
        }
    }

    /// Regression: after a macOS upgrade left the Command Line Tools without
    /// xcrun, `xcode-select -p` still passed while the /usr/bin/python3 shim
    /// exited 1, so the VS Code wrapper (shebang /usr/bin/python3) stayed set
    /// and every panel session failed to start. The interpreter itself must run.
    #[cfg(unix)]
    #[test]
    fn system_python_probe_requires_the_interpreter_to_run() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let select_ok = script("xcode-select", "exit 0");
        let select_missing = script("xcode-select-missing", "exit 2");
        let shim = script(
            "python3-shim",
            "echo 'xcrun: error: invalid active developer path, missing xcrun' >&2; exit 1",
        );
        let python = script("python3", "exit 0");
        assert!(!super::python_usable(&select_ok, &shim));
        assert!(!super::python_usable(
            &select_ok,
            &tmp.path().join("absent")
        ));
        assert!(super::python_usable(&select_ok, &python));
        if cfg!(target_os = "macos") {
            // Without the Command Line Tools the shim is never run: running it
            // pops the "install developer tools" dialog.
            assert!(!super::python_usable(&select_missing, &python));
        }
    }

    /// Regression: Claude Code moved to bash for hook commands on Windows
    /// (v2.1.259), where the PowerShell call operator above is a syntax error --
    /// "/usr/bin/bash: -c: line 1: syntax error near unexpected token" at every
    /// session start, with the guard never running. Its command is the same
    /// quoted pair without the operator, which bash executes and PowerShell no
    /// longer has to parse.
    #[test]
    fn windows_claude_guard_command_is_bash_callable() {
        let cmd = super::join_guard_command(
            "\"C:\\Users\\garm\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\python.exe\"",
            "C:\\Users\\garm space\\.claude\\hooks\\headroom-claude-guard.py",
            true,
            false,
        );
        assert!(
            !cmd.starts_with('&'),
            "bash reads a leading & as a syntax error, got: {cmd}"
        );
        assert!(
            cmd.starts_with("\"C:"),
            "the interpreter path must stay quoted, got: {cmd}"
        );
        assert!(
            cmd.ends_with("headroom-claude-guard.py\""),
            "script path must be quoted so spaces survive, got: {cmd}"
        );
        assert!(
            !cmd.contains("\\\\"),
            "path must not be POSIX-escaped: {cmd}"
        );
    }

    /// OpenCode's `xdg-basedir` ignores `%APPDATA%` on Windows (RUST-K2).
    #[test]
    #[serial_test::serial]
    fn opencode_dirs_follow_xdg_basedir_on_every_platform() {
        let home = TestHome::new(); // restores both XDG vars on drop
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(
            super::opencode_config_dir(),
            home.path().join(".config").join("opencode")
        );
        assert_eq!(
            super::opencode_data_dir(),
            home.path().join(".local").join("share").join("opencode")
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_guard_is_idempotent_and_disable_preserves_user_hooks() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        fs::write(home.path().join(".zshenv"), "# user zshenv\n").unwrap();
        // Pre-existing user-authored hook that must survive apply and disable.
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(
            home.path().join(".codex").join("hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        )
        .unwrap();

        super::apply_client_setup("codex").expect("first apply");
        super::apply_client_setup("codex").expect("second apply");

        let hooks_path = home.path().join(".codex").join("hooks.json");
        let hooks = read_settings_json(&hooks_path);
        // Guard registered on SessionStart exactly once (not UserPromptSubmit,
        // where a nonzero exit would block every prompt).
        let guard_count = hooks["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["hooks"].as_array().unwrap().iter().any(|h| {
                    h["command"]
                        .as_str()
                        .map(|c| c.contains("headroom-codex-guard.py"))
                        .unwrap_or(false)
                })
            })
            .count();
        assert_eq!(
            guard_count, 1,
            "guard registered exactly once on SessionStart, got:\n{hooks:#}"
        );
        // The guard must NOT be on UserPromptSubmit; the pre-existing user hook
        // there survives untouched.
        let user_prompt = hooks["hooks"]["UserPromptSubmit"].to_string();
        assert!(
            !user_prompt.contains("headroom-codex-guard.py"),
            "guard must not register on UserPromptSubmit, got:\n{hooks:#}"
        );
        assert!(
            user_prompt.contains("echo mine"),
            "user's UserPromptSubmit hook preserved, got:\n{hooks:#}"
        );

        super::disable_client_setup("codex").expect("disable");

        let script = home
            .path()
            .join(".codex")
            .join("hooks")
            .join("headroom-codex-guard.py");
        assert!(!script.exists(), "guard script removed on disable");
        let after = read_settings_json(&hooks_path);
        let after_str = serde_json::to_string(&after).unwrap();
        assert!(
            !after_str.contains("headroom-codex-guard.py"),
            "guard stripped from hooks.json, got:\n{after:#}"
        );
        assert!(
            after_str.contains("echo mine"),
            "user-authored hook preserved, got:\n{after:#}"
        );
    }

    #[test]
    fn remove_guard_hook_entries_strips_stale_interpreter_and_argv_forms() {
        // Regression: an entry written by another build (different interpreter) or
        // normalized by Codex into argv-array form under an unregistered event must
        // still be stripped -- otherwise deleting the script leaves a dangling hook.
        let home = TestHome::new();
        let hooks_path = home.path().join("hooks.json");
        let script = "/Users/x/.codex/hooks/headroom-codex-guard.py";
        fs::write(
            &hooks_path,
            format!(
                r#"{{"hooks":{{
                    "SessionStart":[{{"hooks":[{{"type":"command","command":"/opt/homebrew/bin/python3 {script}"}}]}}],
                    "SessionEnd":[{{"hooks":[{{"type":"command","command":["python3","{script}"]}}]}}],
                    "UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"echo mine"}}]}}]
                }}}}"#
            ),
        )
        .unwrap();

        super::remove_guard_hook_entries(&hooks_path, script, true, None).unwrap();

        let after = read_settings_json(&hooks_path);
        let after_str = serde_json::to_string(&after).unwrap();
        assert!(
            !after_str.contains("headroom-codex-guard.py"),
            "stale guard forms stripped, got:\n{after:#}"
        );
        assert!(
            after_str.contains("echo mine"),
            "user hook preserved, got:\n{after:#}"
        );
    }

    #[test]
    fn codex_guard_script_is_informational_never_blocks() {
        // Regression: a nonzero exit on a Codex hook blocks the session, which
        // held lapsed users' own OpenAI-billed Codex hostage to the app. The
        // guard must only notify, never block.
        let script = super::build_codex_guard_script();
        assert!(
            !script.contains("return 2"),
            "codex guard must never block (exit 2)"
        );
        assert!(script.contains("return 0"));
    }

    #[test]
    fn guard_scripts_read_config_as_utf8_not_the_locale_codec() {
        // Regression: on a CP950 Windows box `read_text()` decoded a non-ASCII
        // config.toml with the locale codec and the Codex guard exited 1.
        for script in [
            super::build_codex_guard_script(),
            super::build_claude_guard_script(),
        ] {
            assert!(!script.contains("read_text()"), "{script}");
            assert!(!script.contains("open(path)"), "{script}");
        }
        assert!(super::build_codex_guard_script().contains("read_text(encoding=\"utf-8\")"));
        // Hook JSON on stdin/stdout must not go through the locale codec either.
        let (tool, py) = (Path::new("/x/tool"), Path::new("/x/python"));
        for hook in [
            super::build_headroom_markitdown_hook(tool, py),
            super::build_headroom_rtk_hook(tool, py),
        ] {
            assert!(!hook.contains("\"$HEADROOM_PYTHON\" -c"), "{hook}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn ensure_codex_guard_migrates_off_user_prompt_submit() {
        let home = TestHome::new();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(codex_dir.join("hooks")).unwrap();
        let hooks_path = codex_dir.join("hooks.json");
        let cmd = format!(
            "/usr/bin/python3 {}",
            codex_dir
                .join("hooks")
                .join("headroom-codex-guard.py")
                .display()
        );
        // Old install: guard registered on both SessionStart and UserPromptSubmit.
        // Built via serde_json so the path is JSON-escaped on Windows (raw
        // format! would write lone backslashes the parser mangles).
        let seeded = serde_json::json!({"hooks":{
            "SessionStart":[{"matcher":"startup|resume|clear|compact","hooks":[{"type":"command","command": cmd.as_str()}]}],
            "UserPromptSubmit":[{"hooks":[{"type":"command","command": cmd.as_str()}]}]
        }});
        fs::write(&hooks_path, serde_json::to_string(&seeded).unwrap()).unwrap();

        super::ensure_codex_guard_hook().unwrap();

        let after = read_settings_json(&hooks_path);
        let dump = serde_json::to_string_pretty(&after).unwrap();
        assert!(
            !after["hooks"]["UserPromptSubmit"]
                .to_string()
                .contains("headroom-codex-guard.py"),
            "guard stripped from UserPromptSubmit, got:\n{dump}"
        );
        assert!(
            after["hooks"]["SessionStart"]
                .to_string()
                .contains("headroom-codex-guard.py"),
            "guard kept on SessionStart, got:\n{dump}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_emits_requires_openai_auth_for_every_login() {
        // Without the flag Codex sends no credential at all, whatever the
        // login: these all 401'd "Missing bearer" while the flag was written
        // only for `auth_mode: chatgpt` (RUST-C1, RUST-KN).
        for auth in [
            None,
            Some("{\"auth_mode\":\"chatgpt\",\"tokens\":{\"account_id\":\"acct_123\"}}"),
            Some("{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"sk-test\"}"),
            Some("{\"auth_mode\":\"chatgptAuthTokens\",\"tokens\":{}}"),
        ] {
            let home = TestHome::new();
            fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
            let codex_dir = home.path().join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            if let Some(auth) = auth {
                fs::write(codex_dir.join("auth.json"), auth).unwrap();
            }

            super::apply_client_setup("codex").expect("apply_client_setup succeeds");
            let toml = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
            assert!(
                toml.contains("requires_openai_auth = true"),
                "auth {auth:?} needs the flag, got:\n{toml}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_keeps_root_keys_at_root_scope_when_config_ends_in_a_table() {
        // Regression for the `invalid type: string "headroom", expected a
        // boolean in features` error: a config whose last table is `[features]`
        // (boolean-only values) used to absorb the appended root keys.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_toml = codex_dir.join("config.toml");
        fs::write(
            &config_toml,
            "model = \"gpt-5.4\"\n\n[features]\njs_repl = false\n",
        )
        .unwrap();

        super::apply_client_setup("codex").expect("apply succeeds");

        let raw = fs::read_to_string(&config_toml).unwrap();
        let parsed: toml::Value = raw
            .parse()
            .unwrap_or_else(|e| panic!("valid toml: {e}\n{raw}"));

        assert_eq!(
            parsed.get("model_provider").and_then(|v| v.as_str()),
            Some("headroom"),
            "model_provider must resolve at root scope, got:\n{raw}"
        );
        assert!(
            parsed
                .get("features")
                .and_then(|f| f.get("model_provider"))
                .is_none(),
            "model_provider must not leak into [features], got:\n{raw}"
        );
        assert_eq!(
            parsed
                .get("model_providers")
                .and_then(|m| m.get("headroom"))
                .and_then(|h| h.get("base_url"))
                .and_then(|v| v.as_str()),
            Some(super::HEADROOM_OPENAI_BASE_URL),
            "provider table base_url points at the proxy, got:\n{raw}"
        );
        // The user's own content survives untouched.
        assert_eq!(
            parsed.get("model").and_then(|v| v.as_str()),
            Some("gpt-5.4"),
            "existing root key preserved, got:\n{raw}"
        );
        assert_eq!(
            parsed
                .get("features")
                .and_then(|f| f.get("js_repl"))
                .and_then(|v| v.as_bool()),
            Some(false),
            "existing [features] table preserved, got:\n{raw}"
        );
    }

    #[test]
    fn oss_remnant_warnings_clean_install_is_silent() {
        assert!(oss_remnant_warnings(false, false, false, false).is_empty());
    }

    #[test]
    fn oss_remnant_warnings_flags_each_remnant() {
        let w = oss_remnant_warnings(true, true, true, true);
        assert_eq!(w.len(), 4, "one warning per remnant, got: {w:?}");
        assert!(w.iter().any(|m| m.contains(":8787")));
        assert!(w.iter().any(|m| m.contains("~/.local/bin/headroom")));
        assert!(w.iter().any(|m| m.contains("~/.local/bin/rtk")));
        assert!(w.iter().any(|m| m.contains("settings.json")));
    }

    #[test]
    fn render_codex_config_collapses_duplicate_managed_blocks() {
        // Regression: a config left with TWO managed provider blocks (interrupted
        // write / older build) used to keep one survivor that regenerated forever,
        // surfacing as a duplicate [model_providers.headroom] the user deleted by
        // hand. render must collapse all duplicates down to exactly one.
        let dup = "# >>> headroom:codex_cli_provider >>>\n\
                   [model_providers.headroom]\n\
                   base_url = \"http://stale/v1\"\n\
                   # <<< headroom:codex_cli_provider <<<\n\
                   model = \"gpt-5.4\"\n\
                   # >>> headroom:codex_cli_provider >>>\n\
                   [model_providers.headroom]\n\
                   base_url = \"http://stale2/v1\"\n\
                   # <<< headroom:codex_cli_provider <<<\n";

        let rendered = render_codex_config(dup);

        assert_eq!(
            rendered.matches("[model_providers.headroom]").count(),
            1,
            "exactly one managed provider table after render, got:\n{rendered}"
        );
        assert!(
            rendered.parse::<toml::Value>().is_ok(),
            "rendered config is valid toml, got:\n{rendered}"
        );
        assert!(
            rendered.contains("model = \"gpt-5.4\""),
            "user content between the duplicates is preserved, got:\n{rendered}"
        );
    }

    #[test]
    fn render_codex_config_rescues_codex_tables_trapped_in_the_block() {
        // Regression (Windows repro, 2026-09-03): Codex's TOML writer appends
        // new tables before a trailing comment, and our provider block's closing
        // marker is the last line of the file -- so Codex's own [projects.*]
        // trust, [hooks.state] and [windows] tables land INSIDE the managed
        // block. A rewrite (or disable) then deleted them silently.
        let existing = "# >>> headroom:codex_cli >>>\n\
                        model_provider = \"headroom\"\n\
                        openai_base_url = \"http://127.0.0.1:6767/v1\"\n\
                        # <<< headroom:codex_cli <<<\n\
                        \n\
                        # >>> headroom:codex_cli_provider >>>\n\
                        [model_providers.headroom]\n\
                        name = \"Headroom persistent proxy\"\n\
                        base_url = \"http://127.0.0.1:6767/v1\"\n\
                        supports_websockets = false\n\
                        \n\
                        [projects.'c:\\users\\garm\\code\\headroom-desktop']\n\
                        trust_level = \"trusted\"\n\
                        \n\
                        [hooks.state]\n\
                        \n\
                        [windows]\n\
                        sandbox = \"elevated\"\n\
                        # <<< headroom:codex_cli_provider <<<\n";

        let rendered = render_codex_config(existing);

        assert!(
            rendered.parse::<toml::Value>().is_ok(),
            "rendered config is valid toml, got:\n{rendered}"
        );
        assert!(
            rendered.contains("trust_level = \"trusted\"")
                && rendered.contains("[hooks.state]")
                && rendered.contains("sandbox = \"elevated\""),
            "Codex-owned tables trapped in the block are preserved, got:\n{rendered}"
        );
        // ...and they must live OUTSIDE the regenerated block, or the next
        // rewrite faces the same trap.
        let block_start = rendered
            .find("# >>> headroom:codex_cli_provider >>>")
            .unwrap();
        let block_end = rendered
            .find("# <<< headroom:codex_cli_provider <<<")
            .unwrap();
        let block = &rendered[block_start..block_end];
        assert!(
            !block.contains("[projects") && !block.contains("[windows"),
            "rescued tables sit outside the managed block, got:\n{rendered}"
        );

        // The disable path routes through the same strip: nothing Codex owns
        // may vanish there either.
        let stripped = super::strip_codex_managed_toml(existing);
        assert!(
            stripped.contains("trust_level = \"trusted\"")
                && stripped.contains("sandbox = \"elevated\"")
                && !stripped.contains("model_providers.headroom"),
            "disable keeps Codex-owned tables and drops only ours, got:\n{stripped}"
        );
    }

    /// Turn a freshly applied block into the flagless one older builds wrote.
    fn strip_codex_auth_flag(codex_dir: &std::path::Path) {
        let path = codex_dir.join("config.toml");
        let toml = fs::read_to_string(&path).unwrap();
        fs::write(&path, toml.replace("\nrequires_openai_auth = true", "")).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn codex_block_without_the_auth_flag_is_stale_and_reapply_adds_it() {
        // Builds before 0.9.28 wrote the block without requires_openai_auth
        // for every login but `auth_mode: chatgpt`, so Codex sent no bearer
        // and every request 401'd ("Missing bearer"). Verify must fail on such
        // a block so repair rewrites it.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();

        super::apply_client_setup("codex").expect("apply_client_setup succeeds");
        assert!(super::codex_provider_block_matches().unwrap());
        strip_codex_auth_flag(&codex_dir);
        assert!(
            !super::codex_provider_block_matches().unwrap(),
            "a flagless block is stale"
        );

        super::apply_client_setup("codex").expect("re-apply succeeds");
        let toml = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        assert!(
            toml.contains("requires_openai_auth = true"),
            "re-apply upgrades the block with the flag, got:\n{toml}"
        );
        assert!(
            super::codex_provider_block_matches().unwrap(),
            "upgraded block matches again"
        );
    }

    #[test]
    fn render_codex_config_drops_an_unmarked_headroom_provider_table() {
        // Regression for Sentry RUST-6K: an OSS `pip install headroom` (or a
        // hand-added table from before marker blocks) leaves an UNMARKED
        // [model_providers.headroom]. Adding our marked copy alongside it made
        // the whole config invalid TOML ("duplicate key"), and Codex then
        // refused to load ANY of it -- breaking every `codex` invocation, not
        // just our routing.
        let existing = "model = \"gpt-5.4\"\n\
                        \n\
                        [model_providers.headroom]\n\
                        name = \"Headroom (old oss install)\"\n\
                        base_url = \"http://127.0.0.1:8787/v1\"\n\
                        \n\
                        [model_providers.other]\n\
                        base_url = \"http://elsewhere/v1\"\n";

        let rendered = render_codex_config(existing);

        assert_eq!(
            rendered.matches("[model_providers.headroom]").count(),
            1,
            "exactly one headroom provider table after render, got:\n{rendered}"
        );
        assert!(
            rendered.parse::<toml::Value>().is_ok(),
            "rendered config is valid toml, got:\n{rendered}"
        );
        // The stale table's body must go with its header, not linger as orphan
        // keys absorbed into whatever table precedes them.
        assert!(
            !rendered.contains("8787"),
            "stale provider body is removed with its header, got:\n{rendered}"
        );
        // Everything that is not ours is untouched.
        assert!(
            rendered.contains("[model_providers.other]")
                && rendered.contains("http://elsewhere/v1")
                && rendered.contains("model = \"gpt-5.4\""),
            "foreign providers and user content are preserved, got:\n{rendered}"
        );
    }

    #[test]
    fn codex_foreign_model_provider_is_root_scope_only() {
        let codex_foreign_model_provider =
            |content| super::codex_foreign_root_value(content, "model_provider", "headroom");
        assert_eq!(
            codex_foreign_model_provider("model_provider = \"gateway\"\n").as_deref(),
            Some("gateway"),
        );
        // Our own managed value is not "foreign".
        assert_eq!(
            codex_foreign_model_provider("model_provider = \"headroom\"\n"),
            None,
        );
        // A model_provider inside a table belongs to that table, not the route.
        assert_eq!(
            codex_foreign_model_provider("[profiles.work]\nmodel_provider = \"gateway\"\n"),
            None,
        );
        assert_eq!(codex_foreign_model_provider(""), None);
    }

    #[test]
    fn codex_foreign_model_provider_reads_the_value_as_toml() {
        let codex_foreign_model_provider =
            |content| super::codex_foreign_root_value(content, "model_provider", "headroom");
        // Audit #90: the value was trimmed of `"` only, so a single-quoted
        // (literal string) provider was preserved as `'azure'` and restored
        // as a provider named with quotes, which Codex cannot find.
        assert_eq!(
            codex_foreign_model_provider("model_provider = 'azure'\n").as_deref(),
            Some("azure"),
        );
        assert_eq!(
            codex_foreign_model_provider("model_provider = \"gw\" # corp gateway\n").as_deref(),
            Some("gw"),
        );
    }

    #[test]
    fn render_codex_config_does_not_duplicate_a_foreign_root_model_provider() {
        // Regression: a pre-existing root model_provider used to survive into the
        // rendered body, colliding with the managed `model_provider = "headroom"`
        // as a duplicate root key -> invalid TOML, Codex refuses to load config.
        let existing = "model_provider = \"gateway\"\n\
                        [model_providers.gateway]\n\
                        base_url = \"http://gw/v1\"\n";
        let rendered = render_codex_config(existing);
        let parsed: toml::Value = rendered
            .parse()
            .unwrap_or_else(|e| panic!("rendered config is valid toml: {e}\n{rendered}"));
        assert_eq!(
            parsed.get("model_provider").and_then(|v| v.as_str()),
            Some("headroom"),
            "managed provider wins at root, got:\n{rendered}"
        );
        assert_eq!(
            rendered.matches("model_provider =").count(),
            1,
            "exactly one root model_provider, got:\n{rendered}"
        );
        // The user's own provider table is left untouched for the restore.
        assert!(
            rendered.contains("[model_providers.gateway]"),
            "user provider table preserved, got:\n{rendered}"
        );
    }

    #[test]
    fn codex_session_meta_reads_the_newest_rollouts_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        fs::create_dir_all(root.join("2026/09/01")).unwrap();
        fs::create_dir_all(root.join("2026/09/29")).unwrap();
        // The older day's rollout is written LAST: a resumed thread. Explicit
        // mtimes, not a sleep: coarse-mtime filesystems would tie the two.
        let touch = |rel: &str, age_secs: u64| {
            let path = root.join(rel);
            fs::write(&path, b"{}").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::now() - std::time::Duration::from_secs(age_secs))
                .unwrap();
        };
        touch("2026/09/29/rollout-a.jsonl", 60);
        touch("2026/09/29/notes.txt", 0);
        touch("2026/09/01/rollout-b.jsonl", 10);
        assert_eq!(
            super::newest_jsonl_under(&root, 1_000).map(|(_, path)| path),
            Some(root.join("2026/09/01/rollout-b.jsonl"))
        );
        assert_eq!(
            super::newest_jsonl_under(&root.join("missing"), 1_000),
            None
        );

        let meta = super::parse_codex_session_meta(
            r#"{"timestamp":"x","type":"session_meta","payload":{"id":"1","timestamp":"2026-09-29T07:01:02.123Z","originator":"codex_vscode","cli_version":"0.156.1","model_provider":"openai","instructions":"long"}}"#,
        )
        .expect("session_meta parses");
        assert_eq!(meta.originator.as_deref(), Some("codex_vscode"));
        assert_eq!(meta.cli_version.as_deref(), Some("0.156.1"));
        assert_eq!(meta.model_provider.as_deref(), Some("openai"));
        assert_eq!(
            meta.started_at.map(|at| at.to_rfc3339()),
            Some("2026-09-29T07:01:02.123+00:00".into())
        );
        // Any other first line is not a session header.
        assert_eq!(
            super::parse_codex_session_meta(r#"{"type":"response_item","payload":{}}"#),
            None
        );
        assert_eq!(super::parse_codex_session_meta("not json"), None);
    }

    #[test]
    fn newest_claude_transcript_mtime_ignores_headroom_written_memory_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("-Users-x-repo");
        fs::create_dir_all(project.join("memory")).unwrap();
        // Only Headroom's learn output exists: that is not Claude Code activity.
        fs::write(project.join("memory").join("MEMORY.md"), b"x").unwrap();
        assert!(super::newest_claude_transcript_mtime(tmp.path()).is_none());
        fs::write(project.join("session.jsonl"), b"x").unwrap();
        assert!(super::newest_claude_transcript_mtime(tmp.path()).is_some());
        assert!(super::newest_claude_transcript_mtime(&tmp.path().join("missing")).is_none());
    }

    #[test]
    fn client_ran_unrouted_needs_fresh_activity_long_uptime_and_no_requests() {
        let hour = std::time::Duration::from_secs(3600);
        let now = SystemTime::now();
        let started = now - 3 * hour;
        let active = now - hour;
        assert!(super::client_ran_unrouted(Some(active), 0, started, now));
        // Headroom saw the agent: routed.
        assert!(!super::client_ran_unrouted(Some(active), 1, started, now));
        // Activity predates this app run: it had no proxy to reach.
        assert!(!super::client_ran_unrouted(
            Some(now - 4 * hour),
            0,
            started,
            now
        ));
        // App only just came up.
        assert!(!super::client_ran_unrouted(
            Some(active),
            0,
            now - hour / 2,
            now
        ));
        // Days-old activity says nothing about today's routing.
        assert!(!super::client_ran_unrouted(
            Some(now - 40 * hour),
            0,
            now - 50 * hour,
            now
        ));
        assert!(!super::client_ran_unrouted(None, 0, started, now));
    }

    /// RUST-KC: Codex used before its connector was enabled had no route to
    /// take, so the unrouted baseline is the enable time when that is later.
    #[test]
    fn routed_since_is_the_later_of_start_and_enable() {
        let _home = TestHome::new();
        let started = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        // Not enabled: app start is all there is.
        assert_eq!(super::routed_since("codex", started), started);
        let enabled = started + std::time::Duration::from_secs(360);
        let mut state = super::ClientSetupState::default();
        state.configured_clients.insert(
            "codex_cli".into(),
            chrono::DateTime::<chrono::Utc>::from(enabled).to_rfc3339(),
        );
        state
            .configured_clients
            .insert("claude_code".into(), "2020-01-01T00:00:00+00:00".into());
        super::write_setup_state(&state).expect("write");
        assert_eq!(super::routed_since("codex", started), enabled);
        // Enabled in an earlier run: this run's start still bounds it.
        assert_eq!(super::routed_since("claude_code", started), started);
    }

    // NOTE: keep this the only test that calls repair_client_setups: the
    // function carries a process-wide hourly scan throttle, so a second
    // caller in the same test binary would get an empty no-op back.
    #[test]
    #[serial_test::serial]
    fn codex_missing_bearer_repair_rewrites_flagless_block_at_once() {
        // A flagless block left by an older build: every request 401s. The
        // intercept's 401 hook must fix it now, not on the hourly scan.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        super::apply_client_setup("codex").expect("apply succeeds");
        strip_codex_auth_flag(&codex_dir);

        // Exactly what the 401 hook does: claim the slot on the caller, then
        // do the filesystem work.
        assert!(super::claim_codex_missing_bearer_slot(), "first 401 claims");
        assert!(
            super::repair_codex_missing_bearer_now(),
            "stale block is repaired"
        );
        let toml = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        assert!(toml.contains("requires_openai_auth = true"), "got:\n{toml}");
        // Five-minute throttle: a retry loop of 401s must not churn the file,
        // and must not get as far as spawning a thread to find that out.
        assert!(!super::claim_codex_missing_bearer_slot());
    }

    /// The guards are Python built through `format!`, so a single unescaped
    /// brace yields a script that parses fine as Rust and then dies at
    /// runtime inside the user's agent, where nobody sees it.
    #[test]
    fn guard_scripts_emit_the_verdict_write_with_real_braces() {
        for script in [
            super::build_claude_guard_script(),
            super::build_codex_guard_script(),
        ] {
            assert!(
                script.contains("record_verdict(issues)"),
                "verdict recorded"
            );
            // format! collapses {{ to {: the emitted dict must be real Python.
            assert!(
                script.contains(r#"json.dumps({"at": int(time.time()), "issues": issues})"#),
                "escaping collapsed to a literal dict, got:\n{script}"
            );
            assert!(
                !script.contains("{{"),
                "unescaped brace survived into output"
            );
        }
    }

    /// The whole point of the verdict file: a cause the app CANNOT see from
    /// its own config files still reaches it. Without this the app re-applies
    /// a correct config, reports success, and tells the user to restart.
    #[test]
    #[serial_test::serial]
    fn guard_verdict_carries_a_cause_the_app_cannot_see() {
        let home = TestHome::new();
        let hooks = home.path().join(".claude").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let verdict = hooks.join(".headroom-guard-verdict.json");

        assert_eq!(
            super::read_guard_verdict("claude_code"),
            None,
            "no file yet"
        );

        let fresh = chrono::Utc::now().timestamp();
        fs::write(
            &verdict,
            format!(r#"{{"at": {fresh}, "issues": ["project-local override"]}}"#),
        )
        .unwrap();
        assert_eq!(
            super::read_guard_verdict("claude_code"),
            Some(vec!["project-local override".to_string()])
        );

        // A healthy run records an empty list, which must stay distinct from
        // "the guard never ran" - the caller filters on emptiness.
        fs::write(&verdict, format!(r#"{{"at": {fresh}, "issues": []}}"#)).unwrap();
        assert_eq!(super::read_guard_verdict("claude_code"), Some(Vec::new()));

        // Yesterday's verdict describes a session that has since ended.
        let stale = fresh - 25 * 3600;
        fs::write(&verdict, format!(r#"{{"at": {stale}, "issues": ["old"]}}"#)).unwrap();
        assert_eq!(super::read_guard_verdict("claude_code"), None, "stale");

        fs::write(&verdict, "not json").unwrap();
        assert_eq!(super::read_guard_verdict("claude_code"), None, "garbage");
    }

    #[test]
    #[serial_test::serial]
    fn repair_client_setups_reapplies_a_clobbered_config() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();

        super::apply_client_setup("codex").expect("apply succeeds");
        let baseline = super::verify_client_setup("codex").expect("verify runs");
        assert!(
            baseline.failures.is_empty(),
            "clean right after apply: {:?}",
            baseline.failures
        );

        // Another tool clobbers the routing config behind our back.
        let config_toml = home.path().join(".codex").join("config.toml");
        fs::write(&config_toml, "model_provider = \"other\"\n").unwrap();
        assert!(
            !super::verify_client_setup("codex")
                .expect("verify runs")
                .failures
                .is_empty(),
            "clobber must be visible to verification"
        );

        let repaired = super::repair_client_setups();
        assert_eq!(repaired, vec!["codex_cli".to_string()]);
        let healed = super::verify_client_setup("codex").expect("verify runs");
        assert!(healed.failures.is_empty(), "healed: {:?}", healed.failures);
    }

    // The configure time drives the frontend's "Quit and reopen" hint. A
    // quit + relaunch restore must not reset it, or the hint shows for a day
    // after every launch.
    #[test]
    #[serial_test::serial]
    fn restore_keeps_the_original_configured_at() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();

        super::apply_client_setup("opencode").expect("apply succeeds");
        let original = super::configured_timestamp(&super::load_setup_state(), "opencode")
            .expect("configured");

        super::clear_client_setups().expect("clear succeeds");
        std::thread::sleep(std::time::Duration::from_millis(5));
        super::restore_client_setups();
        assert_eq!(
            super::configured_timestamp(&super::load_setup_state(), "opencode"),
            Some(original.clone())
        );

        // Relaunch into a different build: the new config needs a restart.
        super::clear_client_setups().expect("clear succeeds");
        let mut state = super::load_setup_state();
        state
            .setup_versions
            .insert("opencode".into(), "0.0.1".into());
        super::write_setup_state(&state).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        super::restore_client_setups();
        let updated = super::configured_timestamp(&super::load_setup_state(), "opencode")
            .expect("configured");
        assert_ne!(updated, original, "an update is a new configure");
        let original = updated;

        super::disable_client_setup("opencode").expect("disable succeeds");
        std::thread::sleep(std::time::Duration::from_millis(5));
        super::apply_client_setup("opencode").expect("re-enable succeeds");
        assert_ne!(
            super::configured_timestamp(&super::load_setup_state(), "opencode"),
            Some(original),
            "a user re-enable is a new configure"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_heals_a_stray_end_marker_left_by_a_codex_rewrite() {
        // RUST-BZ: Codex's TOML writer carries our start marker as the
        // leading comment of [model_providers.headroom]; dropping that table
        // takes the start with it and leaves the trailing end marker behind.
        // Render then saw "end before start", left the file untouched, and
        // verify failed on every hourly repair.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_toml = codex_dir.join("config.toml");
        fs::write(
            &config_toml,
            "# >>> headroom:codex_cli >>>\nmodel_provider = \"headroom\"\nopenai_base_url = \"http://127.0.0.1:6767/v1\"\n# <<< headroom:codex_cli <<<\n\n[projects.\"/Users/x/app\"]\ntrust_level = \"trusted\"\n# <<< headroom:codex_cli_provider <<<\n",
        )
        .unwrap();

        super::apply_client_setup("codex").expect("apply succeeds");
        let healed = super::verify_client_setup("codex").expect("verify runs");
        assert!(healed.failures.is_empty(), "healed: {:?}", healed.failures);
        let after = fs::read_to_string(&config_toml).unwrap();
        assert_eq!(
            after
                .matches("# <<< headroom:codex_cli_provider <<<")
                .count(),
            1,
            "{after}"
        );
        assert_eq!(
            after.matches("[model_providers.headroom]").count(),
            1,
            "{after}"
        );
        assert!(after.contains("trust_level = \"trusted\""), "{after}");

        super::apply_client_setup("codex").expect("second apply");
        assert_eq!(
            fs::read_to_string(&config_toml).unwrap(),
            after,
            "byte-stable"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_then_disable_codex_restores_a_foreign_model_provider() {
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_toml = codex_dir.join("config.toml");
        fs::write(
            &config_toml,
            "model_provider = \"gateway\"\n\n[model_providers.gateway]\nbase_url = \"http://gw/v1\"\n",
        )
        .unwrap();

        super::apply_client_setup("codex").expect("apply succeeds");

        let after_apply = fs::read_to_string(&config_toml).unwrap();
        let parsed: toml::Value = after_apply
            .parse()
            .unwrap_or_else(|e| panic!("valid toml after apply: {e}\n{after_apply}"));
        assert_eq!(
            parsed.get("model_provider").and_then(|v| v.as_str()),
            Some("headroom"),
            "Headroom takes over routing while enabled, got:\n{after_apply}"
        );

        super::disable_client_setup("codex").expect("disable succeeds");

        let after_disable = fs::read_to_string(&config_toml).unwrap();
        let parsed: toml::Value = after_disable
            .parse()
            .unwrap_or_else(|e| panic!("valid toml after disable: {e}\n{after_disable}"));
        assert_eq!(
            parsed.get("model_provider").and_then(|v| v.as_str()),
            Some("gateway"),
            "the pre-Headroom provider is restored on disable, got:\n{after_disable}"
        );
        assert!(
            parsed
                .get("model_providers")
                .and_then(|m| m.get("gateway"))
                .is_some(),
            "user provider table survives the round trip, got:\n{after_disable}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_then_disable_codex_restores_a_foreign_root_openai_base_url() {
        // Audit #2: the managed root block also sets openai_base_url, so a
        // user's own root value (LM Studio, a gateway) became a duplicate root
        // key and Codex refused to load its config. A loopback value was
        // instead deleted outright by the orphan filter.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_toml = codex_dir.join("config.toml");
        fs::write(
            &config_toml,
            "openai_base_url=\"http://127.0.0.1:1234/v1\" # LM Studio\nmodel = \"qwen\"\n",
        )
        .unwrap();

        super::apply_client_setup("codex").expect("apply succeeds");

        let after_apply = fs::read_to_string(&config_toml).unwrap();
        let parsed: toml::Value = after_apply
            .parse()
            .unwrap_or_else(|e| panic!("valid toml after apply: {e}\n{after_apply}"));
        assert_eq!(
            parsed.get("openai_base_url").and_then(|v| v.as_str()),
            Some(super::HEADROOM_OPENAI_BASE_URL),
            "Headroom takes over routing while enabled, got:\n{after_apply}"
        );

        super::disable_client_setup("codex").expect("disable succeeds");

        let after_disable = fs::read_to_string(&config_toml).unwrap();
        let parsed: toml::Value = after_disable
            .parse()
            .unwrap_or_else(|e| panic!("valid toml after disable: {e}\n{after_disable}"));
        assert_eq!(
            parsed.get("openai_base_url").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:1234/v1"),
            "the pre-Headroom base URL is restored on disable, got:\n{after_disable}"
        );
        assert_eq!(
            parsed.get("model").and_then(|v| v.as_str()),
            Some("qwen"),
            "other root keys survive the round trip, got:\n{after_disable}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn codex_apply_failing_after_the_config_write_still_persists_the_captured_base_url() {
        // Review of A-2: the config.toml write strips the user's root
        // openai_base_url, so a later failing step (a malformed hooks.json
        // breaks the guard hook) must not drop the captured value, or quit
        // strips with nothing to restore.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_toml = codex_dir.join("config.toml");
        fs::write(
            &config_toml,
            "openai_base_url = \"http://127.0.0.1:1234/v1\"\n",
        )
        .unwrap();
        let hooks_json = codex_dir.join("hooks.json");
        fs::write(&hooks_json, "{ not json").unwrap();

        assert!(super::apply_client_setup("codex").is_err());
        assert_eq!(
            super::load_setup_state()
                .preserved_base_urls
                .get("codex_cli_openai_base_url")
                .map(String::as_str),
            Some("http://127.0.0.1:1234/v1")
        );

        // Once hooks.json is fixed, quit hands the user's value back.
        fs::remove_file(&hooks_json).unwrap();
        super::disable_client_setup("codex").expect("disable succeeds");
        let after: toml::Value = fs::read_to_string(&config_toml).unwrap().parse().unwrap();
        assert_eq!(
            after.get("openai_base_url").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:1234/v1")
        );
    }

    #[test]
    fn strip_codex_managed_toml_keeps_root_keys_codex_appended_inside_the_root_block() {
        // Audit #18: Codex's /model writes `model` and `model_reasoning_effort`
        // through toml_edit, which appends them after the last root key --
        // our openai_base_url -- so they land before our end marker. Every
        // launch and quit then deleted the user's model choice with the block.
        let existing = "# >>> headroom:codex_cli >>>\n\
                        model_provider = \"headroom\"\n\
                        openai_base_url = \"http://127.0.0.1:6767/v1\"\n\
                        model = \"gpt-5.5\"\n\
                        model_reasoning_effort = \"high\"\n\
                        # <<< headroom:codex_cli <<<\n\
                        [projects.'/Users/me/code']\n\
                        trust_level = \"trusted\"\n\
                        \n\
                        # >>> headroom:codex_cli_provider >>>\n\
                        [model_providers.headroom]\n\
                        name = \"Headroom persistent proxy\"\n\
                        base_url = \"http://127.0.0.1:6767/v1\"\n\
                        supports_websockets = false\n\
                        # <<< headroom:codex_cli_provider <<<\n";

        for (label, out) in [
            ("render", render_codex_config(existing)),
            ("strip", super::strip_codex_managed_toml(existing)),
        ] {
            let parsed: toml::Value = out
                .parse()
                .unwrap_or_else(|e| panic!("{label}: valid toml: {e}\n{out}"));
            assert_eq!(
                parsed.get("model").and_then(|v| v.as_str()),
                Some("gpt-5.5"),
                "{label}: /model's choice stays a root key, got:\n{out}"
            );
            assert_eq!(
                parsed
                    .get("model_reasoning_effort")
                    .and_then(|v| v.as_str()),
                Some("high"),
                "{label}: reasoning effort stays a root key, got:\n{out}"
            );
            if let Some(start) = out.find("# >>> headroom:codex_cli >>>") {
                let end = out.find("# <<< headroom:codex_cli <<<").unwrap();
                assert!(
                    !out[start..end].contains("model ="),
                    "{label}: rescued keys sit outside the managed block, got:\n{out}"
                );
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn apply_codex_repairs_a_previously_corrupted_features_block() {
        // A machine upgraded mid-bug: the old single block sits at end-of-file,
        // its root keys absorbed into [features]. Re-applying must repair it so
        // the file parses and the keys resolve at root scope.
        let home = TestHome::new();
        fs::write(home.path().join(".zshrc"), "# user zshrc\n").unwrap();
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config_toml = codex_dir.join("config.toml");
        fs::write(
            &config_toml,
            "[features]\njs_repl = false\n\
             # >>> headroom:codex_cli >>>\n\
             model_provider = \"headroom\"\n\
             openai_base_url = \"http://127.0.0.1:6767/v1\"\n\n\
             [model_providers.headroom]\n\
             name = \"Headroom persistent proxy\"\n\
             base_url = \"http://127.0.0.1:6767/v1\"\n\
             supports_websockets = true\n\
             # <<< headroom:codex_cli <<<\n",
        )
        .unwrap();

        // The corrupted file is invalid against Codex's schema, but still parses
        // as TOML with the key wrongly nested under [features].
        let before: toml::Value = fs::read_to_string(&config_toml).unwrap().parse().unwrap();
        assert_eq!(
            before
                .get("features")
                .and_then(|f| f.get("model_provider"))
                .and_then(|v| v.as_str()),
            Some("headroom"),
            "precondition: corruption present"
        );

        super::apply_client_setup("codex").expect("re-apply repairs config");

        let after: toml::Value = fs::read_to_string(&config_toml).unwrap().parse().unwrap();
        assert_eq!(
            after.get("model_provider").and_then(|v| v.as_str()),
            Some("headroom")
        );
        assert!(after
            .get("features")
            .and_then(|f| f.get("model_provider"))
            .is_none());
    }

    #[test]
    fn sweep_managed_backups_removes_headroom_and_nommer_siblings_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("settings.json");
        fs::write(&target, "{}").unwrap();

        let headroom_backup = tmp
            .path()
            .join("settings.json.headroom-backup-20260101000000");
        let nommer_backup = tmp
            .path()
            .join("settings.json.nommer-backup-20250101000000");
        let unrelated = tmp.path().join("settings.json.bak");
        let other_target_backup = tmp
            .path()
            .join("config.toml.headroom-backup-20260101000000");
        fs::write(&headroom_backup, "old").unwrap();
        fs::write(&nommer_backup, "older").unwrap();
        fs::write(&unrelated, "user-owned").unwrap();
        fs::write(&other_target_backup, "different file's backup").unwrap();

        let removed = super::sweep_managed_backups(&target);

        assert_eq!(removed.len(), 2, "removed: {removed:?}");
        assert!(!headroom_backup.exists(), "headroom backup should be gone");
        assert!(!nommer_backup.exists(), "nommer backup should be gone");
        assert!(unrelated.exists(), "unrelated .bak should survive");
        assert!(
            other_target_backup.exists(),
            "another file's backup should survive"
        );
        assert!(target.exists(), "target file itself should survive");
    }

    #[test]
    fn dedupe_shell_targets_drops_directories_keeps_files_and_missing_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile_dir = tmp.path().join(".profile");
        fs::create_dir(&profile_dir).unwrap();
        let zshrc = tmp.path().join(".zshrc");
        fs::write(&zshrc, "# user config\n").unwrap();
        let not_created_yet = tmp.path().join(".bash_profile");

        let kept = super::dedupe_shell_targets(vec![
            profile_dir.clone(),
            zshrc.clone(),
            not_created_yet.clone(),
            zshrc.clone(),
        ]);

        assert_eq!(kept, vec![zshrc, not_created_yet]);
        assert!(
            !kept.contains(&profile_dir),
            "a directory named .profile must never become a shell target (RUST-5X)"
        );
    }

    #[test]
    fn upsert_managed_block_never_sees_a_directory_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile_dir = tmp.path().join(".profile");
        fs::create_dir(&profile_dir).unwrap();

        // Ground truth for the bug: reading a directory is EISDIR, and that
        // error used to abort setup for every client.
        let err = super::upsert_managed_block(&profile_dir, "claude_code", "export FOO=1")
            .expect_err("reading a directory must fail");
        // The invariant is that it errors instead of clobbering; the OS wording
        // differs (EISDIR on Unix, "Access is denied" os error 5 on Windows).
        #[cfg(unix)]
        assert!(
            format!("{err:#}").contains("Is a directory"),
            "unexpected error: {err:#}"
        );
        let _ = err;
        assert!(super::dedupe_shell_targets(vec![profile_dir]).is_empty());
    }

    #[test]
    fn sweep_managed_backups_is_quiet_when_parent_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist").join("settings.json");
        let removed = super::sweep_managed_backups(&missing);
        assert!(removed.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn write_setup_state_publishes_atomically() {
        let _home = TestHome::new();
        let mut state = super::ClientSetupState::default();
        state
            .configured_clients
            .insert("claude_code".into(), "2026-01-01T00:00:00+00:00".into());
        super::write_setup_state(&state).expect("write");

        let path = super::setup_state_path();
        assert!(path.exists(), "setup state file written");

        // No sibling .tmp* file may be left behind after a successful publish —
        // its presence would mean the rename step never happened.
        let dir = path.parent().unwrap();
        let stem = path.file_name().unwrap().to_string_lossy().into_owned();
        let leftover: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&format!("{stem}.tmp")))
            .collect();
        assert!(
            leftover.is_empty(),
            "tmp files cleaned up by rename, got: {leftover:?}"
        );

        // Round-trip survives.
        let reloaded = super::load_setup_state();
        assert!(reloaded.configured_clients.contains_key("claude_code"));
    }

    #[test]
    fn retry_transient_denied_retries_then_succeeds() {
        // RUST-9M: a rename denied by a transient AV/indexer hold must be
        // retried, not reported. Two denials then success => Ok.
        let mut calls = 0;
        let out = super::retry_transient_denied(|| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(out.unwrap(), 3);
    }

    #[test]
    fn retry_transient_denied_gives_up_and_passes_other_errors_through() {
        // Persistent denial: every attempt used (4, or 6 on Windows), then
        // the error surfaces.
        let mut calls = 0;
        let out = super::retry_transient_denied(|| -> std::io::Result<()> {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert_eq!(
            out.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(calls, super::TRANSIENT_DENIED_RETRIES + 1);
        // A non-denied error is never retried.
        let mut calls = 0;
        let out = super::retry_transient_denied(|| -> std::io::Result<()> {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert_eq!(out.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(calls, 1);
    }

    #[test]
    fn command_under_dir_matches_windows_paths_by_separator_and_case() {
        let dir = r"C:\Users\Jo\AppData\Local\Headroom";
        // What Claude's config actually holds on Windows: backslashes, and
        // whatever casing the writer used. The old `format!("{dir}/")` prefix
        // matched none of these, so uninstall left them behind.
        assert!(super::command_under_dir_for(
            r"C:\Users\Jo\AppData\Local\Headroom\tools\serena\serena.exe",
            dir,
            true
        ));
        assert!(super::command_under_dir_for(
            "c:/users/jo/appdata/local/headroom/tools/serena/serena.exe",
            dir,
            true
        ));
        // A sibling that merely shares the prefix is not inside the footprint.
        assert!(!super::command_under_dir_for(
            r"C:\Users\Jo\AppData\Local\HeadroomOther\x.exe",
            dir,
            true
        ));
        // Unix stays exact: case matters there.
        assert!(super::command_under_dir_for(
            "/Users/jo/.headroom/bin/x",
            "/Users/jo/.headroom",
            false
        ));
        assert!(!super::command_under_dir_for(
            "/Users/jo/.HEADROOM/bin/x",
            "/Users/jo/.headroom",
            false
        ));
    }

    #[test]
    fn toml_line_value_unescapes_windows_command_paths() {
        // The registrar TOML-escapes backslashes, so a Windows config.toml
        // holds `C:\\Users\\...`. Compared raw, the uninstall fallback never
        // matched Headroom-owned MCP tables and left them behind.
        let line = r#"command = "C:\\Users\\Jo\\AppData\\Local\\Headroom\\headroom\\serena-venv\\Scripts\\serena.exe""#;
        let command = super::toml_line_value(line).expect("string value");
        assert_eq!(
            command,
            r"C:\Users\Jo\AppData\Local\Headroom\headroom\serena-venv\Scripts\serena.exe"
        );
        assert!(super::command_under_dir_for(
            &command,
            r"C:\Users\Jo\AppData\Local\Headroom",
            true
        ));
        // Literal strings and trailing comments read as the client sees them.
        assert_eq!(
            super::toml_line_value("command = 'C:\\x\\y.exe'  # note").as_deref(),
            Some(r"C:\x\y.exe")
        );
    }

    #[test]
    fn move_aside_moves_the_file_and_keeps_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("savings-state.json");
        let dest = dir.path().join("savings-state.json.corrupt");
        std::fs::write(&path, b"{history}").expect("seed");
        super::move_aside(&path, &dest).expect("move aside");
        assert!(!path.exists());
        assert_eq!(std::fs::read(&dest).expect("backup"), b"{history}");
        // A missing source is an error, not a silent success.
        assert!(super::move_aside(&path, &dest).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn retry_transient_denied_retries_windows_sharing_violation() {
        // RUST-5X: os error 32 (ERROR_SHARING_VIOLATION) is `Uncategorized`
        // in std, so a kind check alone never retried it.
        let mut calls = 0;
        let out = super::retry_transient_denied(|| {
            calls += 1;
            if calls < 2 {
                Err(std::io::Error::from_raw_os_error(32))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(out.unwrap(), 2);
    }

    /// Windows names the interpreter first, as the hooks do; the entry is
    /// still ours, and a composed command that also runs ours is still not.
    #[test]
    fn our_statusline_is_recognised_behind_the_windows_bash_program() {
        use super::is_our_statusline;
        let ours =
            |command: &str| is_our_statusline(&json!({ "type": "command", "command": command }));
        assert!(ours(
            r#""C:\Program Files\Git\bin\bash.exe" "C:\Users\a b\.claude\hooks\headroom-statusline.sh""#
        ));
        assert!(ours(
            r#"bash "C:/Users/a/.claude/hooks/headroom-statusline.sh""#
        ));
        assert!(ours(r#""/Users/a/.claude/hooks/headroom-statusline.sh""#));
        assert!(!ours(
            r#""C:\Program Files\Git\bin\bash.exe" "C:\Users\a\.claude\mine.sh""#
        ));
        assert!(!ours(
            r#"node "C:\Users\a\.claude\hooks\headroom-statusline.sh""#
        ));
        assert!(!ours(
            r#"bash "C:\Users\a\mine.sh"; "C:\Users\a\.claude\hooks\headroom-statusline.sh""#
        ));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn statusline_takes_only_an_empty_slot_and_removes_only_its_own() {
        let _home = TestHome::new();
        let settings = claude_settings_path();
        let read = || -> Value {
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap()
        };

        let (changed, _) = ensure_claude_statusline().expect("install");
        assert_eq!(changed.len(), 2, "script + settings written: {changed:?}");
        assert!(is_our_statusline(&read()["statusLine"]));
        let script = std::fs::read_to_string(claude_statusline_script_path()).unwrap();
        assert!(script.contains(&crate::claude_statusline::state_path().display().to_string()));
        let (changed, _) = ensure_claude_statusline().expect("reinstall");
        assert!(changed.is_empty(), "idempotent: {changed:?}");

        // The opt-out removes it and keeps later setups from putting it back.
        set_statusline_enabled(false).expect("disable");
        assert!(read().get("statusLine").is_none());
        assert!(!claude_statusline_script_path().exists());
        ensure_claude_statusline().expect("setup while disabled");
        assert!(read().get("statusLine").is_none());
        set_statusline_enabled(true).expect("enable");

        // A user's own statusline is never replaced and never removed.
        let own = serde_json::json!({ "type": "command", "command": "~/my-line.sh" });
        std::fs::write(
            &settings,
            serde_json::to_vec(&serde_json::json!({ "statusLine": own, "model": "opus" })).unwrap(),
        )
        .unwrap();
        ensure_claude_statusline().expect("setup over a user line");
        assert_eq!(read()["statusLine"], own);
        remove_claude_statusline().expect("remove");
        assert_eq!(read()["statusLine"], own);
        assert_eq!(read()["model"], "opus");

        // Nor is a composed line that merely also runs our script.
        let composed = serde_json::json!({
            "type": "command",
            "command": format!("{}; ~/my-line.sh", claude_statusline_script_path().display()),
        });
        std::fs::write(
            &settings,
            serde_json::to_vec(&serde_json::json!({ "statusLine": composed })).unwrap(),
        )
        .unwrap();
        ensure_claude_statusline().expect("setup over a composed line");
        assert_eq!(read()["statusLine"], composed);
        remove_claude_statusline().expect("remove");
        assert_eq!(read()["statusLine"], composed);

        // Ours last is still theirs, quoted or not: only the text after the
        // final `/` names our script.
        let ours = claude_statusline_script_path().display().to_string();
        for command in [
            format!("~/my-line.sh; {ours}"),
            format!("~/my-line.sh;{ours}"),
            format!("\"~/my-line.sh\" && \"{ours}\""),
        ] {
            let line = serde_json::json!({ "type": "command", "command": command });
            assert!(!is_our_statusline(&line), "{command}");
        }
        assert!(is_our_statusline(
            &serde_json::json!({ "type": "command", "command": ours })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn statusline_script_prints_this_conversation_and_is_otherwise_silent() {
        use crate::claude_statusline::{Persisted, Session, SCHEMA_VERSION};
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("claude-statusline.json");
        let now_ms = chrono::Utc::now().timestamp_millis();
        let session = |total, last, at, req| Session {
            tokens_saved: total,
            last_saved: last,
            last_saved_at_ms: at,
            last_request_at_ms: req,
        };
        let old = now_ms - 60_000;
        // Serialized by the store itself, so the script's regex is checked
        // against the real field order, not a hand-written copy of it.
        let persisted = Persisted {
            schema_version: SCHEMA_VERSION,
            sessions: BTreeMap::from([
                // A follow-up request must not cut a fresh saving short.
                (
                    "aaaa-just-saved".into(),
                    session(1_100_000, 15_400, now_ms, now_ms),
                ),
                ("bbbb-quiet".into(), session(3_067, 2_865, old, old)),
                ("cccc-small".into(), session(700, 700, old, old)),
                ("dddd-sending".into(), session(3_067, 2_865, old, now_ms)),
                ("eeee-first-request".into(), session(0, 0, 0, now_ms)),
                ("ffff-nothing-yet".into(), session(0, 0, 0, old)),
            ]),
            // Written after the sessions; the script's regex must not care.
            plan_usage: Some(crate::models::ClaudePlanUsage {
                five_hour: Some(crate::models::PlanWindow {
                    used_percent: 50.0,
                    resets_at: 1,
                }),
                seven_day: None,
            }),
            codex_plan_usage: None,
        };
        // Plus an entry as the previous build wrote it, without lastRequestAtMs.
        let json = serde_json::to_string(&persisted).unwrap().replacen(
            r#""sessions":{"#,
            r#""sessions":{"0000-old-format":{"tokensSaved":42,"lastSaved":0,"lastSavedAtMs":1},"#,
            1,
        );
        std::fs::write(&state, json).unwrap();
        let script = dir.path().join(CLAUDE_STATUSLINE_SCRIPT);
        std::fs::write(&script, build_claude_statusline_script(&state)).unwrap();
        let render = |stdin: &str| -> String {
            use std::io::Write;
            let mut child = crate::proc::command("/bin/bash")
                .arg(&script)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "script must never fail the statusline"
            );
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            render(r#"{"session_id":"aaaa-just-saved","cwd":"/x"}"#),
            "\x1b[1;32mHeadroom saved 1.1M tokens this session (+15k)\x1b[0m\n"
        );
        // Claude Code's real payload is pretty-printed; spacing must not matter.
        assert_eq!(
            render("{\n  \"session_id\": \"bbbb-quiet\"\n}"),
            "Headroom saved 3.1k tokens this session\n"
        );
        assert_eq!(
            render(r#"{"session_id":"cccc-small"}"#),
            "Headroom saved 700 tokens this session\n"
        );
        let compressing = "\x1b[32mHeadroom compressing...\x1b[0m\n";
        assert_eq!(render(r#"{"session_id":"dddd-sending"}"#), compressing);
        assert_eq!(
            render(r#"{"session_id":"eeee-first-request"}"#),
            compressing
        );
        assert_eq!(render(r#"{"session_id":"ffff-nothing-yet"}"#), "");
        assert_eq!(
            render(r#"{"session_id":"0000-old-format"}"#),
            "Headroom saved 42 tokens this session\n"
        );
        assert_eq!(render(r#"{"session_id":"unknown"}"#), "");
        assert_eq!(render("not json"), "");

        // Plan usage from Claude Code's own `rate_limits`, after the savings,
        // pretty-printed as Claude Code sends it.
        let now = chrono::Utc::now().timestamp();
        let limits = |five: &str, five_at: i64, week: &str, week_at: i64| {
            format!(
                "{{\n  \"session_id\": \"bbbb-quiet\",\n  \"rate_limits\": {{\n    \"five_hour\": {{\n      \"used_percentage\": {five},\n      \"resets_at\": {five_at}\n    }},\n    \"seven_day\": {{\n      \"used_percentage\": {week},\n      \"resets_at\": {week_at}\n    }}\n  }}\n}}"
            )
        };
        assert_eq!(
            render(&limits("34.9", now + 3_600, "62", now + 300_000)),
            "Headroom saved 3.1k tokens this session | usage: 5h 34%, week 62%\n"
        );
        // A window past its reset is back at 0; one near its cap turns
        // yellow with its reset time.
        let near = render(&limits("97.2", now - 60, "91", now + 3_600));
        assert!(
            near.starts_with(
                "Headroom saved 3.1k tokens this session | usage: 5h 0%, \x1b[33mweek 91% (resets "
            ),
            "{near:?}"
        );
        assert!(near.ends_with(")\x1b[0m\n"), "{near:?}");
        // Usage alone, before this conversation has saved anything.
        assert_eq!(
            render(
                &limits("12", now + 3_600, "40", now + 300_000).replace("bbbb-quiet", "unknown")
            ),
            "usage: 5h 12%, week 40%\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_control_command_installs_and_removes_only_its_own_file() {
        let home = TestHome::new();
        // A VS Code settings file exists, so the wrapper setting is written too.
        let vscode = vscode_user_settings_path();
        std::fs::create_dir_all(vscode.parent().unwrap()).unwrap();
        // Hand-written JSONC: the comment and key order must survive.
        let original = "{\n    // mine\n    \"zeta\": 1,\n    \"editor.fontSize\": 13\n}\n";
        std::fs::write(&vscode, original).unwrap();
        let (changed, _) = ensure_claude_remote_control_command().expect("install");
        assert_eq!(
            changed.len(),
            6,
            "script + 2 commands + wrapper + hooks + vscode setting: {changed:?}"
        );
        let panel = std::fs::read_to_string(claude_remote_control_panel_command_path()).unwrap();
        assert!(panel.contains(CLAUDE_REMOTE_CONTROL_COMMAND_MARKER));
        assert!(panel.contains("from the VS Code panel"));
        let wrapper = std::fs::read_to_string(claude_remote_control_wrapper_path()).unwrap();
        assert!(wrapper.starts_with("#!/usr/bin/python3\n"));
        let raw = std::fs::read_to_string(&vscode).unwrap();
        let v = parse_json_object(&raw, &vscode).unwrap();
        assert_eq!(
            v[VSCODE_PROCESS_WRAPPER_KEY],
            Value::String(claude_remote_control_wrapper_path().display().to_string())
        );
        assert!(
            raw.ends_with(&original[1..]),
            "only the key is added: {raw}"
        );
        let command = std::fs::read_to_string(claude_remote_control_command_path()).unwrap();
        let script_path = claude_remote_control_script_path();
        assert!(command.contains(CLAUDE_REMOTE_CONTROL_COMMAND_MARKER));
        assert!(command.contains("disable-model-invocation: true"));
        assert!(command.contains(&format!(
            "Bash({}:*), AskUserQuestion",
            script_path.display()
        )));
        assert!(command.contains("First write exactly this one line of plain text"));
        assert!(command
            .contains("then reply with exactly one line: \"Restarting with Remote Control. This takes up to 30 seconds.\""));
        assert!(command.contains("Then call the AskUserQuestion tool"));
        assert!(command
            .contains("load it first with ToolSearch using the query \"select:AskUserQuestion\""));
        assert!(command.contains("AskUserQuestion, ToolSearch\n"));
        assert!(command.contains(
            "Headroom is incompatible with Remote Control due to design decisions by Anthropic."
        ));
        let script = std::fs::read_to_string(&script_path).unwrap();
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("--stop"));
        let settings = std::fs::read_to_string(claude_settings_path()).unwrap();
        let hooks: Value = serde_json::from_str(&settings).unwrap();
        for (event, phase) in [("Stop", "--stop"), ("UserPromptSubmit", "--cancel")] {
            assert_eq!(
                hooks["hooks"][event][0]["hooks"][0]["command"],
                claude_remote_control_hook_command(phase),
                "{settings}"
            );
        }
        // Runs on every prompt, so it must not flash a status line.
        assert!(hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]
            .get("statusMessage")
            .is_none());
        let (changed, _) = ensure_claude_remote_control_command().expect("reinstall");
        assert!(changed.is_empty(), "idempotent: {changed:?}");

        // A user-authored command of the same name survives removal.
        std::fs::write(claude_remote_control_command_path(), "my own command\n").unwrap();
        remove_claude_remote_control_command().expect("remove");
        assert!(!script_path.exists());
        // Kept: VS Code can still launch it until it notices the setting is gone.
        assert!(claude_remote_control_wrapper_path().exists());
        // Uninstall removes it, once the setting no longer names it.
        remove_vscode_wrapper_file_if_unreferenced();
        assert!(!claude_remote_control_wrapper_path().exists());
        assert!(!claude_remote_control_panel_command_path().exists());
        assert_eq!(std::fs::read_to_string(&vscode).unwrap(), original);
        let settings = std::fs::read_to_string(claude_settings_path()).unwrap();
        assert!(!settings.contains("headroom-remote-control"), "{settings}");
        assert_eq!(
            std::fs::read_to_string(claude_remote_control_command_path()).unwrap(),
            "my own command\n"
        );

        // ...and install, which used to replace it with ours.
        ensure_claude_remote_control_command().expect("install again");
        remove_claude_remote_control_command().expect("remove again");
        assert_eq!(
            std::fs::read_to_string(claude_remote_control_command_path()).unwrap(),
            "my own command\n"
        );
        std::fs::remove_file(claude_remote_control_command_path()).unwrap();

        // A wrapper the user configured themselves is neither replaced nor removed.
        if cfg!(target_os = "macos") {
            std::fs::write(
                &vscode,
                format!("{{\"{VSCODE_PROCESS_WRAPPER_KEY}\": \"/opt/mine/wrap\"}}"),
            )
            .unwrap();
            ensure_claude_remote_control_command().expect("install over user wrapper");
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&vscode).unwrap()).unwrap();
            assert_eq!(
                v[VSCODE_PROCESS_WRAPPER_KEY],
                Value::String("/opt/mine/wrap".into())
            );
            remove_claude_remote_control_command().expect("remove keeps user wrapper");
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&vscode).unwrap()).unwrap();
            assert_eq!(
                v[VSCODE_PROCESS_WRAPPER_KEY],
                Value::String("/opt/mine/wrap".into())
            );
        }
        assert!(!claude_remote_control_command_path().exists());
        assert!(!home
            .path()
            .join(".claude/commands/remote-control.md")
            .exists());
    }

    #[test]
    fn windows_wrapper_exe_puts_the_shebang_right_before_its_zip() {
        let python = Path::new(r"C:\Users\A B\AppData\Roaming\Headroom\python.exe");
        let exe = build_windows_wrapper_exe(b"MZstub", python, "print(1)\n").unwrap();
        // What distlib's launcher does: locate the end-of-central-directory
        // record, step back over the directory to the archive's start, and
        // read the shebang line that ends there.
        let eocd = exe
            .windows(4)
            .rposition(|w| w == b"PK\x05\x06")
            .expect("end of central directory");
        let u32_at = |at: usize| u32::from_le_bytes(exe[at..at + 4].try_into().unwrap()) as usize;
        let start = eocd - u32_at(eocd + 12) - u32_at(eocd + 16);
        assert_eq!(&exe[..6], b"MZstub");
        assert_eq!(
            &exe[6..start],
            "#!\"C:\\Users\\A B\\AppData\\Roaming\\Headroom\\python.exe\"\n".as_bytes()
        );
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&exe[start..])).unwrap();
        let mut main = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("__main__.py").unwrap(), &mut main).unwrap();
        assert_eq!(main, "print(1)\n");
    }

    /// The Windows flow, run by the system Python with the Windows switch on:
    /// --confirm records the restart, and the wrapper ends its child after the
    /// turn's result and resumes the same session with Remote Control on. The
    /// file is the real .exe layout minus the stub, which Python runs as a zip.
    #[cfg(unix)]
    #[test]
    fn windows_wrapper_restarts_its_child_once_the_confirmed_turn_ends() {
        use std::io::{BufRead, Write};
        let home = TestHome::new();
        let source = build_claude_remote_control_wrapper()
            .replace("ENDS_CHILD = os.name == \"nt\"", "ENDS_CHILD = True");
        assert!(source.contains("ENDS_CHILD = True"));
        let wrapper = home.path().join("wrapper.exe");
        std::fs::write(
            &wrapper,
            build_windows_wrapper_exe(b"", Path::new("/usr/bin/python3"), &source).unwrap(),
        )
        .unwrap();
        let log = home.path().join("argv.log");
        let fake = home.path().join("fake_claude.py");
        std::fs::write(
            &fake,
            r#"import json, sys
with open(sys.argv[1], "a") as f:
    f.write(" ".join(sys.argv[2:]) + "\n")
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("type") == "user":
        print(json.dumps({"type": "result", "subtype": "success", "session_id": "s1"}), flush=True)
    elif msg.get("request_id") == "headroom-remote-control":
        print(json.dumps({"type": "control_response", "response": {"subtype": "success",
            "request_id": "headroom-remote-control",
            "response": {"session_url": "https://claude.ai/code/session_x"}}}), flush=True)
"#,
        )
        .unwrap();

        let confirm = crate::proc::command("/usr/bin/python3")
            .arg(&wrapper)
            .arg("--confirm")
            .env("HOME", home.path())
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:6767")
            .env("CLAUDE_CODE_SESSION_ID", "s1")
            .env("HEADROOM_RC_RELAUNCHER", "wrapper")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&confirm.stdout);
        assert!(said.contains("Restarting this session"), "{said}");
        let dir = home.path().join(".headroom").join("remote-control");
        assert!(dir.join("exit-s1").exists());

        let mut child = crate::proc::command("/usr/bin/python3")
            .arg(&wrapper)
            .arg("/usr/bin/python3")
            .arg(&fake)
            .arg(&log)
            .env("HOME", home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let _ = tx.send(line);
            }
        });
        writeln!(
            stdin,
            r#"{{"type":"user","message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        let mut seen = Vec::new();
        let announced = loop {
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(line) if line.contains("Remote Control is now active") => break line,
                Ok(line) => seen.push(line),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("no announcement; saw {seen:?}");
                }
            }
        };
        drop(stdin);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("wrapper outlived its stdin");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            announced.contains("claude.ai/code/session_x"),
            "{announced}"
        );
        assert!(seen[0].contains(r#""type": "result""#), "{seen:?}");
        let argv = std::fs::read_to_string(&log).unwrap();
        let runs: Vec<&str> = argv.lines().collect();
        assert_eq!(runs.len(), 2, "{argv}");
        assert!(runs[1].starts_with("--resume s1 --settings "), "{argv}");
        assert!(!dir.join("exit-s1").exists() && !dir.join("resume-s1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn remote_control_script_records_the_exit_and_the_stop_hook_performs_it() {
        use std::os::unix::process::ExitStatusExt;
        let home = TestHome::new();
        ensure_claude_remote_control_command().expect("install");
        let script = claude_remote_control_script_path();
        // A real process whose executable name is `claude`, since the stop
        // phase refuses to signal anything else.
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // A symlink, not a copy: a copied platform binary wedges on macOS
        // (unkillable "UE" state) and hangs the test harness on its stdout pipe.
        std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).unwrap();
        let spawn_claude = || {
            use std::os::unix::process::CommandExt;
            let mut cmd = crate::proc::command(bin.join("claude"));
            cmd.arg("30");
            // No controlling terminal, as on CI. With an empty
            // HEADROOM_REMOTE_CONTROL_TTY the script reads this pid's tty from
            // `ps`, so run from a terminal it found one and restarted.
            // SAFETY: setsid is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            cmd.spawn().expect("spawn fake claude")
        };
        let wait_for_term = |child: &mut std::process::Child, what: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            while std::time::Instant::now() < deadline {
                if let Some(status) = child.try_wait().unwrap() {
                    assert_eq!(status.signal(), Some(libc::SIGTERM));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what}")
        };
        let mut child = spawn_claude();
        let pid = child.id().to_string();
        let run = |base_url: &str, relauncher: &str, tty: &str, sid: &str, pid: &str| -> String {
            let out = crate::proc::command("sh")
                .arg(&script)
                .env("HOME", home.path())
                .env("ANTHROPIC_BASE_URL", base_url)
                .env("CLAUDE_PID", pid)
                .env("CLAUDE_CODE_SESSION_ID", sid)
                .env("HEADROOM_RC_RELAUNCHER", relauncher)
                .env("HEADROOM_REMOTE_CONTROL_TTY", tty)
                .env("HEADROOM_REMOTE_CONTROL_FALLBACK_SECS", "1")
                .output()
                .expect("run script");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let hook = |phase: &str, session_id: &str| {
            use std::io::Write;
            let mut proc = crate::proc::command("sh")
                .arg(&script)
                .arg(phase)
                .env("HOME", home.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("run hook");
            proc.stdin
                .take()
                .unwrap()
                .write_all(format!(r#"{{"session_id": "{session_id}"}}"#).as_bytes())
                .unwrap();
            let out = proc.wait_with_output().unwrap();
            assert!(out.status.success());
            // UserPromptSubmit stdout would be injected into the prompt.
            assert!(out.stdout.is_empty(), "{phase} printed output");
        };
        let stop = |session_id: &str| hook("--stop", session_id);
        let marker_dir = home.path().join(".headroom/remote-control");
        let exit_file = marker_dir.join("exit-sid-123");
        let tty_marker = marker_dir.join("ttys999");
        let fallback = format!(
            "claude --settings '{CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE}' -r sid-123 --remote-control"
        );

        let out = run(
            "https://api.anthropic.com",
            "tty",
            "ttys999",
            "sid-123",
            &pid,
        );
        assert!(out.contains("already available"), "{out}");
        assert!(!marker_dir.exists());

        // Nothing will relaunch it: no tty, or a session not started through
        // the shell function or the wrapper (alias, shell opened before
        // setup). Never ended; the manual command carries the override.
        for (relauncher, tty) in [("tty", ""), ("", "ttys999"), ("bogus", "ttys999")] {
            let out = run(
                HEADROOM_ANTHROPIC_BASE_URL,
                relauncher,
                tty,
                "sid-123",
                &pid,
            );
            assert!(
                out.contains("cannot restart this session by itself"),
                "{out}"
            );
            assert!(out.contains(&fallback), "{out}");
            assert!(!marker_dir.exists(), "{relauncher}/{tty} recorded an exit");
        }

        // The VS Code panel through the wrapper: pending exit only; the resume
        // marker is written by the Stop hook.
        let out = run(
            HEADROOM_ANTHROPIC_BASE_URL,
            "wrapper",
            "",
            "sid-panel",
            &pid,
        );
        assert!(out.contains("this panel stays open"), "{out}");
        assert_eq!(
            std::fs::read_to_string(marker_dir.join("exit-sid-panel")).unwrap(),
            format!("{pid} wrapper\n")
        );
        assert!(!marker_dir.join("resume-sid-panel").exists());
        std::fs::remove_file(marker_dir.join("exit-sid-panel")).unwrap();

        // Confirmed in a terminal: records the pending exit, kills nothing yet.
        let out = run(
            HEADROOM_ANTHROPIC_BASE_URL,
            "tty",
            "ttys999",
            "sid-123",
            &pid,
        );
        assert!(
            out.contains("Restarting this session with Remote Control"),
            "{out}"
        );
        assert!(out.contains(&fallback), "{out}");
        assert_eq!(
            std::fs::read_to_string(&exit_file).unwrap(),
            format!("{pid} ttys999\n")
        );
        assert!(!tty_marker.exists(), "relaunch marker only at --stop");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "session must outlive the tool call"
        );
        // With the Stop hook registered there is no fallback timer at all.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "no timed exit while the Stop hook is registered"
        );

        // A new prompt means the user interrupted the confirming turn (Esc skips
        // Stop): the pending exit is dropped, no relaunch marker is left for a
        // later exit, and the next Stop kills nothing.
        hook("--cancel", "sid-123");
        assert!(!exit_file.exists());
        assert!(!tty_marker.exists());
        stop("sid-123");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            child.try_wait().unwrap().is_none(),
            "cancelled restart fired"
        );
        run(
            HEADROOM_ANTHROPIC_BASE_URL,
            "tty",
            "ttys999",
            "sid-123",
            &pid,
        );
        assert!(exit_file.exists());

        // A Stop for some other session leaves it alone.
        stop("sid-other");
        assert!(exit_file.exists());
        assert!(child.try_wait().unwrap().is_none());

        // The Stop for this session writes the relaunch marker, performs the
        // exit and consumes the record.
        stop("sid-123");
        assert!(!exit_file.exists());
        assert_eq!(std::fs::read_to_string(&tty_marker).unwrap(), "sid-123\n");
        std::fs::remove_file(&tty_marker).unwrap();
        wait_for_term(&mut child, "session was not terminated by the stop hook");

        // The stop phase never signals a pid that is not Claude Code any more.
        let mut other = crate::proc::command("sleep").arg("30").spawn().unwrap();
        std::fs::write(&exit_file, format!("{} ttys999\n", other.id())).unwrap();
        stop("sid-123");
        assert!(!exit_file.exists());
        assert!(!tty_marker.exists());
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            other.try_wait().unwrap().is_none(),
            "foreign pid must survive"
        );
        let _ = other.kill();

        // Nor a stale record (the terminal closed before the Stop hook ran):
        // resumed days later, the pid may be another claude.
        let mut stale = spawn_claude();
        std::fs::write(&exit_file, format!("{} ttys999\n", stale.id())).unwrap();
        assert!(crate::proc::command("touch")
            .args(["-t", "202001010000"])
            .arg(&exit_file)
            .status()
            .unwrap()
            .success());
        stop("sid-123");
        assert!(!exit_file.exists());
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(stale.try_wait().unwrap().is_none(), "stale record fired");
        let _ = stale.kill();
        let _ = stale.wait();

        // Without the Stop hook (settings edited by hand) the timed exit runs
        // the same stop phase.
        std::fs::write(claude_settings_path(), "{}\n").unwrap();
        let mut child = spawn_claude();
        let pid = child.id().to_string();
        let out = run(
            HEADROOM_ANTHROPIC_BASE_URL,
            "tty",
            "ttys998",
            "sid-456",
            &pid,
        );
        assert!(out.contains("Restarting"), "{out}");
        wait_for_term(&mut child, "timed exit did not fire without the Stop hook");
        assert!(!marker_dir.join("exit-sid-456").exists());
        assert_eq!(
            std::fs::read_to_string(marker_dir.join("ttys998")).unwrap(),
            "sid-456\n"
        );

        // Registered but switched off by disableAllHooks: the hook never runs,
        // so the timed exit must, for the VS Code panel too.
        std::fs::write(
            claude_settings_path(),
            format!(
                "{{\"disableAllHooks\": true, \"hooks\": {{\"Stop\": [{{\"hooks\": [{{\"type\": \"command\", \"command\": \"{} --stop\"}}]}}]}}}}\n",
                script.display()
            ),
        )
        .unwrap();
        let mut child = spawn_claude();
        let pid = child.id().to_string();
        run(HEADROOM_ANTHROPIC_BASE_URL, "wrapper", "", "sid-789", &pid);
        wait_for_term(&mut child, "timed exit did not fire with hooks disabled");
        assert!(marker_dir.join("resume-sid-789").exists());
    }

    #[cfg(unix)]
    #[test]
    fn claude_shell_function_resumes_the_marked_session_without_headroom() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = home.path().join("claude.log");
        let marker_dir = home.path().join(".headroom/remote-control");
        std::fs::create_dir_all(&marker_dir).unwrap();
        // Fake claude: logs its argv; on first launch leaves a relaunch marker
        // for this tty, exactly like the /remote-control script does.
        let fake_claude = format!(
            "#!/bin/sh\necho \"${{HEADROOM_RC_RELAUNCHER:-none}} $*\" >> '{log}'\nif [ ! -e '{first}' ]; then touch '{first}'; echo sid-123 > '{marker}'; fi\nexit 7\n",
            log = log.display(),
            first = home.path().join("first").display(),
            marker = marker_dir.join("ttys999").display(),
        );
        std::fs::write(bin.join("claude"), fake_claude).unwrap();
        std::fs::write(bin.join("tty"), "#!/bin/sh\necho /dev/ttys999\n").unwrap();
        for name in ["claude", "tty"] {
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let block = home.path().join("block.sh");
        std::fs::write(&block, claude_code_shell_block(closed_loopback_port())).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let shells: Vec<&str> = ["bash", "zsh"]
            .into_iter()
            .filter(|sh| crate::proc::command(sh).arg("-c").arg(":").status().is_ok())
            .collect();
        for shell in shells {
            // `relaunch`: the fake leaves a relaunch marker on its first launch.
            let run = |cmd: &str, relaunch: bool| {
                let _ = std::fs::remove_file(&log);
                let first = home.path().join("first");
                if relaunch {
                    let _ = std::fs::remove_file(&first);
                } else {
                    std::fs::write(&first, "").unwrap();
                }
                crate::proc::command(shell)
                    .arg("-c")
                    .arg(format!(". '{}'; {cmd}", block.display()))
                    .env("HOME", home.path())
                    .env("PATH", &path)
                    .env_remove("HEADROOM_RC_RELAUNCHER")
                    .status()
                    .expect("run shell")
            };
            let status = run("claude hello world", true);
            let lines = std::fs::read_to_string(&log).unwrap();
            let expected_resume = format!(
                "none --settings {CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE} -r sid-123 --remote-control"
            );
            assert_eq!(
                lines,
                format!("tty hello world\n{expected_resume}\n"),
                "{shell}"
            );
            assert!(!marker_dir.join("ttys999").exists(), "marker consumed");
            assert_eq!(
                status.code(),
                Some(7),
                "{shell}: relaunch exit code propagates"
            );

            // Session flags survive the relaunch; the prompt, a resume target
            // and unknown flags do not.
            run(
                "claude --model opus 'fix it' --dangerously-skip-permissions --add-dir '../a b' --effort=high -c --unknown x",
                true,
            );
            assert_eq!(
                std::fs::read_to_string(&log).unwrap().lines().nth(1),
                Some(
                    format!(
                        "none --settings {CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE} --model opus --dangerously-skip-permissions --add-dir ../a b --effort=high -r sid-123 --remote-control"
                    )
                    .as_str()
                ),
                "{shell}"
            );

            // Manual fallback: the flag alone gets the override added, but
            // only as a whole argument, never inside a prompt.
            let status = run("claude -r sid-123 --remote-control", false);
            assert_eq!(status.code(), Some(7));
            assert_eq!(
                std::fs::read_to_string(&log).unwrap(),
                format!("tty --settings {CLAUDE_REMOTE_CONTROL_SETTINGS_OVERRIDE} -r sid-123 --remote-control\n"),
                "{shell}"
            );
            run("claude -p 'what does --remote-control do'", false);
            assert_eq!(
                std::fs::read_to_string(&log).unwrap(),
                "tty -p what does --remote-control do\n",
                "{shell}"
            );
        }

        // A user's `alias claude=...` above the block (interactive shells
        // expand aliases) must not turn the block into a parse error that
        // aborts the rest of their rc file; the alias keeps winning.
        let out = crate::proc::command("bash")
            .arg("-c")
            .arg(format!(
                "shopt -s expand_aliases\nalias claude='claude --aliased'\n. '{}'\necho rc-tail-ran; type -t claude",
                block.display()
            ))
            .env("HOME", home.path())
            .env("PATH", &path)
            .output()
            .expect("run bash");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "rc-tail-ran\nalias\n",
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Claude Code under `CLAUDE_CONFIG_DIR=~/.claude-work` reads that dir's
    /// settings.json, not the one Headroom routes, and a shell started before
    /// Headroom (login restoring terminals ahead of the app) has no export.
    /// The `claude` function routes such a session for its own process only,
    /// and only while ~/.claude/settings.json still routes through Headroom and
    /// the user set no base URL of their own.
    #[cfg(unix)]
    #[test]
    fn claude_shell_function_routes_another_config_dir_only_while_headroom_routes() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            bin.join("claude"),
            "#!/bin/sh\necho \"${ANTHROPIC_BASE_URL:-unset}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let block = home.path().join("block.sh");
        // Started while the intercept was down: no export at shell start.
        std::fs::write(&block, claude_code_shell_block(closed_loopback_port())).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let work = home.path().join(".claude-work");
        let default_dir = format!("{}/", home.path().join(".claude").display());
        let shells: Vec<&str> = ["bash", "zsh"]
            .into_iter()
            .filter(|sh| crate::proc::command(sh).arg("-c").arg(":").status().is_ok())
            .collect();
        for shell in shells {
            let run = |config_dir: Option<&str>, base_url: Option<&str>| {
                let mut cmd = crate::proc::command(shell);
                // `set -u`: an rc file with `setopt nounset` / `set -u` above
                // the block must still be able to start claude.
                cmd.arg("-c")
                    .arg(format!(
                        "set -u; . '{}'; claude; echo \"after=${{ANTHROPIC_BASE_URL:-unset}}\"",
                        block.display()
                    ))
                    .env("HOME", home.path())
                    .env("PATH", &path)
                    .env_remove("CLAUDE_CONFIG_DIR")
                    .env_remove("ANTHROPIC_BASE_URL");
                if let Some(dir) = config_dir {
                    cmd.env("CLAUDE_CONFIG_DIR", dir);
                }
                if let Some(url) = base_url {
                    cmd.env("ANTHROPIC_BASE_URL", url);
                }
                String::from_utf8(cmd.output().expect("run shell").stdout).unwrap()
            };
            let work = work.to_str().unwrap();

            super::configure_claude_settings_env("ANTHROPIC_BASE_URL", HEADROOM_ANTHROPIC_BASE_URL)
                .expect("route settings.json");
            assert_eq!(
                run(Some(work), None),
                format!("{HEADROOM_ANTHROPIC_BASE_URL}\nafter=unset\n"),
                "{shell}: another config dir is routed for that process only"
            );
            assert_eq!(
                run(None, None),
                "unset\nafter=unset\n",
                "{shell}: settings.json routes the default dir"
            );
            assert_eq!(
                run(Some(&default_dir), None),
                "unset\nafter=unset\n",
                "{shell}: ~/.claude/ is the default dir"
            );
            assert_eq!(
                run(Some(work), Some("https://gateway.example")),
                "https://gateway.example\nafter=https://gateway.example\n",
                "{shell}: the user's own base URL wins"
            );

            // After quit settings.json no longer routes: never the dead port.
            std::fs::write(home.path().join(".claude/settings.json"), "{}\n").unwrap();
            assert_eq!(
                run(Some(work), None),
                "unset\nafter=unset\n",
                "{shell}: not routed once Headroom stops"
            );
        }
    }

    /// A loopback port nothing listens on (bound, then released).
    #[cfg(unix)]
    fn closed_loopback_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("ephemeral loopback port")
            .port()
    }

    /// The blocks export ANTHROPIC_BASE_URL / OPENAI_BASE_URL for Agent SDK
    /// scripts and other tools, but only in a shell that starts while the
    /// intercept answers, so a terminal opened after quit never points at the
    /// dead port. `claude` and `codex` re-probe per call and drop a Headroom
    /// URL the shell still carries once the port is closed, for that call
    /// only. A user's own value is never exported over nor stripped.
    #[cfg(unix)]
    #[test]
    fn shell_blocks_export_base_urls_only_while_the_intercept_answers() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["claude", "codex"] {
            std::fs::write(
                bin.join(name),
                format!("#!/bin/sh\necho \"{name}=${{ANTHROPIC_BASE_URL:-unset}},${{OPENAI_BASE_URL:-unset}}\"\n"),
            )
            .unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        let closed = closed_loopback_port();
        let block = |port: u16| {
            let file = home.path().join(format!("block-{port}.sh"));
            std::fs::write(
                &file,
                format!(
                    "{}\n{}\n",
                    claude_code_shell_block(port),
                    super::codex_shell_block(port)
                ),
            )
            .unwrap();
            file
        };
        let (open_block, closed_block) = (block(open), block(closed));
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let (a, o) = (HEADROOM_ANTHROPIC_BASE_URL, super::HEADROOM_OPENAI_BASE_URL);
        let shells: Vec<&str> = ["bash", "zsh"]
            .into_iter()
            .filter(|sh| crate::proc::command(sh).arg("-c").arg(":").status().is_ok())
            .collect();
        for shell in shells {
            // `inherited`: both variables as the shell's parent passed them;
            // `later`: set after the rc ran (a shell started while Headroom ran).
            let exec = |script: String, inherited: Option<(&str, &str)>| {
                let mut cmd = crate::proc::command(shell);
                cmd.arg("-c")
                    .arg(script)
                    .env("HOME", home.path())
                    .env("PATH", &path)
                    .env_remove("CLAUDE_CONFIG_DIR")
                    .env_remove("ANTHROPIC_BASE_URL")
                    .env_remove("OPENAI_BASE_URL");
                if let Some((anthropic, openai)) = inherited {
                    cmd.env("ANTHROPIC_BASE_URL", anthropic)
                        .env("OPENAI_BASE_URL", openai);
                }
                let out = cmd.output().expect("run shell");
                assert!(
                    out.stderr.is_empty(),
                    "{shell}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8(out.stdout).unwrap()
            };
            let run = |block: &Path,
                       inherited: Option<(&str, &str)>,
                       later: Option<(&str, &str)>| {
                let mut script = format!(". '{}'\n", block.display());
                if let Some((anthropic, openai)) = later {
                    script.push_str(&format!(
                        "export ANTHROPIC_BASE_URL={anthropic} OPENAI_BASE_URL={openai}\n"
                    ));
                }
                script.push_str(
                    "echo \"shell=${ANTHROPIC_BASE_URL:-unset},${OPENAI_BASE_URL:-unset}\"; claude; codex\n\
                     echo \"after=${ANTHROPIC_BASE_URL:-unset},${OPENAI_BASE_URL:-unset}\"",
                );
                exec(script, inherited)
            };
            let both = |x: &str, y: &str| {
                format!("shell={x},{y}\nclaude={x},{y}\ncodex={x},{y}\nafter={x},{y}\n")
            };

            assert_eq!(
                run(&open_block, None, None),
                both(a, o),
                "{shell}: exported while the intercept answers"
            );
            assert_eq!(
                run(&closed_block, None, None),
                both("unset", "unset"),
                "{shell}: nothing exported once it is gone"
            );
            assert_eq!(
                run(&closed_block, Some((a, o)), None),
                both("unset", "unset"),
                "{shell}: a pane inheriting the dead URL drops it"
            );
            assert_eq!(
                run(&closed_block, None, Some((a, o))),
                format!("shell={a},{o}\nclaude=unset,{o}\ncodex={a},unset\nafter={a},{o}\n"),
                "{shell}: each command runs without its dead URL, that call only"
            );
            let own = ("https://gateway.example", "http://localhost:11434/v1");
            for block in [&open_block, &closed_block] {
                assert_eq!(
                    run(block, Some(own), None),
                    both(own.0, own.1),
                    "{shell}: the user's own URLs are never replaced"
                );
                assert_eq!(
                    run(block, None, Some(own)),
                    both(own.0, own.1),
                    "{shell}: nor stripped"
                );
                // A codex() wrapper of the user's own, sourced before a block
                // (and after the .zprofile copy of it), keeps its flags.
                for before in ["", &format!(". '{}'\n", block.display())] {
                    assert_eq!(
                        exec(
                            format!(
                                "{before}codex() {{ echo mine; }}\n. '{}'\ncodex\n",
                                block.display()
                            ),
                            None
                        ),
                        "mine\n",
                        "{shell}: the user's codex function is kept"
                    );
                }
            }
        }
        drop(listener);
    }

    /// A login shell sources both blocks from both profiles (.zprofile and
    /// .zshrc, or .bash_profile sourcing .bashrc): the start-up probe runs
    /// once for all four export lines, so a slow probe (a full accept queue,
    /// Windows retrying a refused connect) stalls a new terminal once, not 4x.
    #[cfg(unix)]
    #[test]
    fn shell_start_probes_the_intercept_once() {
        let home = TestHome::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let block = home.path().join("block.sh");
        std::fs::write(
            &block,
            format!(
                "{}\n{}\n",
                claude_code_shell_block(port),
                super::codex_shell_block(port)
            ),
        )
        .unwrap();
        for shell in ["bash", "zsh"] {
            if crate::proc::command(shell)
                .arg("-c")
                .arg(":")
                .status()
                .is_err()
            {
                continue;
            }
            let out = crate::proc::command(shell)
                .arg("-c")
                .arg(format!(
                    ". '{0}'\n. '{0}'\necho \"$ANTHROPIC_BASE_URL\"",
                    block.display()
                ))
                .env("HOME", home.path())
                .env_remove("ANTHROPIC_BASE_URL")
                .env_remove("OPENAI_BASE_URL")
                .output()
                .expect("run shell");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                format!("{HEADROOM_ANTHROPIC_BASE_URL}\n"),
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let probes = std::iter::from_fn(|| listener.accept().ok()).count();
            assert_eq!(probes, 1, "{shell}: connects at shell start");
        }
    }

    /// Git for Windows reads profile files from a `HOME` the user set; the
    /// blocks must go there, not to `%USERPROFILE%` (Windows rc9 pass).
    #[test]
    fn git_bash_home_follows_a_user_set_home() {
        use std::ffi::OsString;
        let profile = PathBuf::from("PROFILE");
        let exists = |p: &Path| p == Path::new("C:\\hrhome-test") || p == Path::new("D:\\home\\x");
        let home = |v: &str| super::git_bash_home(Some(OsString::from(v)), profile.clone(), exists);
        assert_eq!(home("C:\\hrhome-test"), PathBuf::from("C:\\hrhome-test"));
        assert_eq!(
            home("\"C:\\hrhome-test\""),
            PathBuf::from("C:\\hrhome-test")
        );
        assert_eq!(home("/d/home/x"), PathBuf::from("D:\\home\\x"));
        // Not an existing directory, empty, or unset: Git Bash's fallback.
        assert_eq!(home("C:\\gone"), profile);
        assert_eq!(home("  "), profile);
        assert_eq!(super::git_bash_home(None, profile.clone(), exists), profile);
    }

    /// Persisted targets from the old location move to the new home; zsh
    /// files and targets elsewhere stay put.
    #[test]
    fn legacy_git_bash_targets_move_to_the_shell_home() {
        let legacy = PathBuf::from("/profile");
        let current = PathBuf::from("/home");
        let moved = super::rehome_shell_targets(
            vec![
                legacy.join(".bashrc"),
                legacy.join(".bash_profile"),
                legacy.join(".zshrc"),
                PathBuf::from("/elsewhere/.bashrc"),
            ],
            Some(&legacy),
            &current,
        );
        assert_eq!(
            moved,
            vec![
                current.join(".bashrc"),
                current.join(".bash_profile"),
                legacy.join(".zshrc"),
                PathBuf::from("/elsewhere/.bashrc"),
            ]
        );
        let same = vec![legacy.join(".bashrc")];
        assert_eq!(
            super::rehome_shell_targets(same.clone(), None, &current),
            same
        );
    }

    /// Winsock retries a refused loopback connect, so in Windows Git Bash a
    /// probe of a closed 6767 took about 2 s (rc9 win-test VM, 6/6 runs), on
    /// every new terminal and `claude`/`codex` call once the blocks outlive
    /// the app. There (`$OSTYPE` msys or cygwin) the probe runs under
    /// coreutils `/usr/bin/timeout 1`, with BASH_ENV cleared so the child bash
    /// never sources an rc that probes again; elsewhere it stays the builtin
    /// connect, no extra process. Simulated with OSTYPE set in the script and
    /// a fake coreutils `timeout` that logs and runs its command, substituted
    /// for /usr/bin/timeout in the block.
    ///
    /// rc12 gate: the probe ran whichever `timeout` PATH found first. With
    /// System32 ahead of /usr/bin that is Windows' timeout.exe, which rejects
    /// the arguments and exits 1, so a live intercept read as down and the
    /// block dropped the URL. The PATH `timeout` here is that impostor.
    #[cfg(unix)]
    #[test]
    fn windows_git_bash_bounds_the_intercept_probe() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        let bin = home.path().join("bin");
        let usr_bin = home.path().join("usr").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&usr_bin).unwrap();
        let log = home.path().join("timeout.log");
        let coreutils = usr_bin.join("timeout");
        for (path, script) in [
            (
                coreutils.clone(),
                format!(
                    "#!/bin/sh\necho \"$1 env=${{BASH_ENV-}}\" >> '{}'\nshift\nexec \"$@\"\n",
                    log.display()
                ),
            ),
            (bin.join("timeout"), "#!/bin/sh\nexit 1\n".to_string()),
        ] {
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (open, closed) = (
            listener.local_addr().unwrap().port(),
            closed_loopback_port(),
        );
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let shells: Vec<&str> = ["bash", "zsh", "sh"]
            .into_iter()
            .filter(|sh| crate::proc::command(sh).arg("-c").arg(":").status().is_ok())
            .collect();
        for shell in shells {
            let run = |port: u16, ostype: &str| {
                let block = home.path().join(format!("block-{port}.sh"));
                std::fs::write(
                    &block,
                    claude_code_shell_block(port)
                        .replace("/usr/bin/timeout", &coreutils.to_string_lossy()),
                )
                .unwrap();
                let _ = std::fs::remove_file(&log);
                let out = crate::proc::command(shell)
                    .arg("-c")
                    .arg(format!(
                        "{ostype}set -u; . '{}'; echo \"${{ANTHROPIC_BASE_URL:-unset}}\"",
                        block.display()
                    ))
                    .env("HOME", home.path())
                    .env("PATH", &path)
                    .env("BASH_ENV", "/dev/null")
                    .env_remove("CLAUDE_CONFIG_DIR")
                    .env_remove("ANTHROPIC_BASE_URL")
                    .output()
                    .expect("run shell");
                assert!(
                    out.stderr.is_empty(),
                    "{shell}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                (
                    String::from_utf8(out.stdout).unwrap(),
                    std::fs::read_to_string(&log).unwrap_or_default(),
                )
            };
            for ostype in ["OSTYPE=msys; ", "OSTYPE=cygwin; "] {
                assert_eq!(
                    run(open, ostype),
                    (
                        format!("{HEADROOM_ANTHROPIC_BASE_URL}\n"),
                        "1 env=\n".to_string()
                    ),
                    "{shell} {ostype}: bounded probe, intercept up"
                );
                assert_eq!(
                    run(closed, ostype),
                    ("unset\n".to_string(), "1 env=\n".to_string()),
                    "{shell} {ostype}: bounded probe, intercept down"
                );
            }
            assert_eq!(
                run(open, "").1,
                "",
                "{shell}: macOS/Linux never spawn timeout"
            );
        }
        drop(listener);
    }

    /// settings.json is hand-maintained JSONC: the wrapper key goes in and out
    /// as a text edit that keeps comments and key order, and anything that is
    /// not exactly our key is refused rather than rewritten.
    #[test]
    fn vscode_wrapper_key_edit_keeps_user_settings_byte_for_byte() {
        let path = Path::new("settings.json");
        let w = "/Users/me/.headroom/claude-wrapper";
        let user = "{\n  // font for the panel\n  \"editor.fontSize\": 13,\n  \"files.autoSave\": \"off\"\n}\n";

        let added = edit_vscode_wrapper_key(user, w, true, path).expect("add");
        assert!(added.contains("// font for the panel"));
        let obj = parse_json_object(&added, path).unwrap();
        assert_eq!(obj[VSCODE_PROCESS_WRAPPER_KEY], w);
        assert_eq!(obj["editor.fontSize"], 13);
        assert_eq!(
            edit_vscode_wrapper_key(&added, w, false, path).as_deref(),
            Some(user)
        );

        // Empty object, and the key as the last entry (no trailing comma left).
        let added = edit_vscode_wrapper_key("{}", w, true, path).expect("add to empty");
        let removed = edit_vscode_wrapper_key(&added, w, false, path).expect("remove");
        assert!(parse_json_object(&removed, path).unwrap().is_empty());
        let last = format!("{{\"a\": 1, \"{VSCODE_PROCESS_WRAPPER_KEY}\": \"{w}\"}}");
        let removed = edit_vscode_wrapper_key(&last, w, false, path).expect("remove last");
        assert_eq!(removed, "{\"a\": 1}");

        // A wrapper the user set themselves, or no key at all, is not ours.
        let theirs = format!("{{\"{VSCODE_PROCESS_WRAPPER_KEY}\": \"/opt/their-wrapper\"}}");
        assert_eq!(edit_vscode_wrapper_key(&theirs, w, false, path), None);
        assert_eq!(edit_vscode_wrapper_key(user, w, false, path), None);
        // Unparseable settings are never touched.
        assert_eq!(edit_vscode_wrapper_key("{\"a\": ", w, true, path), None);
    }

    /// Drives the wrapper the way the VS Code extension does, with a fake
    /// "claude" that speaks just enough stream-json: it echoes control
    /// requests as responses, reports a session id, and exits when told.
    #[cfg(unix)]
    #[test]
    fn remote_control_wrapper_swaps_the_session_without_the_extension_noticing() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        ensure_claude_remote_control_command().expect("install");
        let wrapper = claude_remote_control_wrapper_path();
        let fake = home.path().join("fake-claude.py");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
resumed = "--resume" in args
override = "--settings" in args and "api.anthropic.com" in " ".join(args)
sid = args[args.index("--resume") + 1] if resumed else "sid-wrap"
def emit(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
emit({"type": "system", "subtype": "init", "session_id": sid, "resumed": resumed, "override": override,
      "relauncher": os.environ.get("HEADROOM_RC_RELAUNCHER")})
for line in sys.stdin:
    d = json.loads(line)
    if d.get("type") == "control_request":
        sub = d["request"].get("subtype")
        emit({"type": "control_response", "response": {"subtype": "success", "request_id": d["request_id"],
              "response": {"echo": sub, "override": override, "session_url": "https://claude.ai/code/session_x"}}})
        if sub == "remote_control":
            emit({"type": "system", "subtype": "bridge", "enabled": True, "override": override})
        if sub == "rewind_files":
            emit({"type": "system", "subtype": "rewound", "resumed": resumed})
    elif d.get("type") == "user":
        if d["message"]["content"] == "exit":
            sys.exit(143)
        emit({"type": "assistant", "text": "seen:" + d["message"]["content"], "resumed": resumed})
sys.exit(3)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut proc = crate::proc::command(&wrapper)
            .arg(&fake)
            .arg("--output-format")
            .arg("stream-json")
            .env("HOME", home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn wrapper");
        let mut stdin = proc.stdin.take().unwrap();
        let mut lines = BufReader::new(proc.stdout.take().unwrap()).lines();
        let mut next = || -> Value {
            serde_json::from_str(&lines.next().expect("line").expect("read")).expect("json")
        };
        let mut send = |s: &str| {
            stdin.write_all(s.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
            stdin.flush().unwrap();
        };

        let init = next();
        assert_eq!(init["session_id"], "sid-wrap");
        assert_eq!(init["resumed"], false);
        assert_eq!(
            init["relauncher"], "wrapper",
            "tells the script it can relaunch"
        );
        send(
            r#"{"type":"control_request","request_id":"init-1","request":{"subtype":"initialize"}}"#,
        );
        assert_eq!(next()["response"]["request_id"], "init-1");
        send(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#);
        assert_eq!(next()["text"], "seen:hello");
        // A one-shot action: replaying it into the respawned child would undo
        // every edit made since that checkpoint.
        send(
            r#"{"type":"control_request","request_id":"rw-1","request":{"subtype":"rewind_files"}}"#,
        );
        assert_eq!(next()["response"]["request_id"], "rw-1");
        assert_eq!(next()["subtype"], "rewound");

        let marker_dir = home.path().join(".headroom/remote-control");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join("resume-sid-wrap"), "").unwrap();
        send(r#"{"type":"user","message":{"role":"user","content":"exit"}}"#);

        // The swap: the new child is resumed with the override, the replayed
        // handshake answer is swallowed, Remote Control is requested, and the
        // extension's next message lands in the resumed session.
        let reinit = next();
        assert_eq!(reinit["resumed"], true, "{reinit}");
        assert_eq!(reinit["override"], true, "{reinit}");
        assert_eq!(reinit["session_id"], "sid-wrap");
        // The swallowed Remote Control answer becomes the panel's only sign
        // that the swap finished: a meta line in the conversation.
        let active = next();
        assert_eq!(active["subtype"], "informational", "{active}");
        assert_eq!(active["level"], "notice");
        assert_eq!(active["session_id"], "sid-wrap");
        assert_eq!(
            active["content"],
            "Remote Control is now active. Continue here, on your phone, or at https://claude.ai/code/session_x"
        );
        let bridge = next();
        assert_eq!(
            bridge["subtype"], "bridge",
            "handshake answer must be swallowed, got {bridge}"
        );
        assert_eq!(bridge["override"], true);
        assert!(
            !marker_dir.join("resume-sid-wrap").exists(),
            "marker consumed"
        );
        send(r#"{"type":"user","message":{"role":"user","content":"after"}}"#);
        let after = next();
        assert_eq!(after["text"], "seen:after");
        assert_eq!(after["resumed"], true);

        // The panel closing (stdin EOF) ends the wrapper with the child's code,
        // even with a relaunch marker present: nothing may run on headless.
        std::fs::write(marker_dir.join("resume-sid-wrap"), "").unwrap();
        drop(stdin);
        let status = proc.wait().unwrap();
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn repair_reapplies_managed_files_written_by_another_app_version() {
        let _home = TestHome::new();
        super::apply_client_setup("claude_code").expect("first apply");
        let state = super::load_setup_state();
        assert_eq!(
            state.setup_versions.get("claude_code").map(String::as_str),
            Some(env!("CARGO_PKG_VERSION")),
            "apply stamps the running version: {state:?}"
        );
        assert!(super::stale_setup_version("claude_code").is_none());
        assert!(!super::repair_client_setup_now("claude_code"));

        // The stamp says an older build wrote the files. The routing export is
        // intact, so verification alone would never trigger a re-apply.
        let mut state = super::load_setup_state();
        state
            .setup_versions
            .insert("claude_code".into(), "0.9.22-rc.4".into());
        super::write_setup_state(&state).unwrap();
        assert_eq!(
            super::stale_setup_version("claude_code").as_deref(),
            Some("0.9.22-rc.4")
        );
        assert!(
            !super::repair_client_setup_now("claude_code"),
            "a restamp is not a repair of a broken config"
        );
        assert_eq!(
            super::load_setup_state()
                .setup_versions
                .get("claude_code")
                .map(String::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );

        // An install from before the stamp existed is stale too.
        let mut state = super::load_setup_state();
        state.setup_versions.clear();
        super::write_setup_state(&state).unwrap();
        assert_eq!(
            super::stale_setup_version("claude_code").as_deref(),
            Some("an earlier build")
        );

        // A client that is not configured is never stale.
        super::disable_client_setup("claude_code").unwrap();
        assert!(super::stale_setup_version("claude_code").is_none());
        assert!(super::load_setup_state().setup_versions.is_empty());
    }

    #[test]
    fn coalesce_writes_lands_every_edit_as_one_write() {
        // RUST-GS/KH: a Claude Code session that read settings.json between
        // two of our writes kept that half-applied copy and wrote it back.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let other = tmp.path().join("other.json");
        std::fs::write(&path, "{}").unwrap();
        super::coalesce_writes(path.clone(), || {
            super::atomic_write(&path, b"{\"env\":1}")?;
            super::atomic_write(&path, b"{\"env\":1,\"hooks\":2}")?;
            // Readers see the held edit; the disk does not, until the end.
            assert_eq!(
                super::read_held_or_disk(&path).unwrap(),
                "{\"env\":1,\"hooks\":2}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
            // Only the held path waits.
            super::atomic_write(&other, b"x")?;
            assert_eq!(std::fs::read(&other).unwrap(), b"x");
            Ok(())
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"env\":1,\"hooks\":2}"
        );

        // A failed step still persists what came before it, as step-by-step did.
        let fresh = tmp.path().join("fresh.json");
        let err = super::coalesce_writes(fresh.clone(), || -> anyhow::Result<()> {
            assert!(!super::held_or_exists(&fresh));
            super::atomic_write(&fresh, b"{}")?;
            assert!(super::held_or_exists(&fresh));
            Err(anyhow::anyhow!("later step failed"))
        });
        assert!(err.is_err());
        assert_eq!(std::fs::read(&fresh).unwrap(), b"{}");
        // And the hold is released: writes land directly again.
        super::atomic_write(&path, b"{}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }

    #[test]
    fn atomic_write_creates_missing_parent_dir() {
        // RUST-8M: callers that skip their own `create_dir_all` got ENOENT
        // (os error 3 on Windows) when the config dir was missing.
        let dir = std::env::temp_dir().join(format!("aw_mkparent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("state.json");
        super::atomic_write(&path, b"{}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_the_replaced_files_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o600, 0o755] {
            let path = dir.path().join(format!("f{mode:o}"));
            std::fs::write(&path, b"old").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            super::atomic_write(&path, b"new").unwrap();
            let got = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "mode {mode:o} was not kept");
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
        }
    }

    #[test]
    fn managed_block_writes_through_a_symlinked_profile() {
        // A dotfiles-managed `~/.zprofile -> dotfiles/zprofile` must stay a
        // link: the rename used to replace the link with a regular file, so the
        // block landed in a fork the dotfiles repo never saw. Relative target
        // plus a second hop covers the stow-style `../dotfiles/...` chains.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("zprofile");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "export FOO=1\n").unwrap();
        let hop = dir.path().join("hop");
        if !super::symlink_file_or_skip(&Path::new("dotfiles").join("zprofile"), &hop) {
            return;
        }
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let link = home.join(".zprofile");
        assert!(super::symlink_file_or_skip(
            &Path::new("..").join("hop"),
            &link
        ));

        let (changed, _) = super::upsert_managed_block(&link, "test", "export BAR=2").unwrap();
        assert!(changed);
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(&hop)
            .unwrap()
            .file_type()
            .is_symlink());
        let body = std::fs::read_to_string(&real).unwrap();
        assert!(body.starts_with("export FOO=1\n"), "{body}");
        assert!(body.contains("export BAR=2"), "{body}");

        // Backups stay beside the link, never inside the dotfiles repo.
        let repo: Vec<_> = std::fs::read_dir(real.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(repo, vec![std::ffi::OsString::from("zprofile")]);

        assert!(super::remove_managed_block(&link, "test").unwrap());
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "export FOO=1\n");
    }

    #[test]
    #[serial_test::serial]
    fn connector_round_trip_keeps_every_symlinked_config_a_symlink() {
        // A dotfiles repo owning every file the Claude Code and Codex connectors
        // write. Apply, pause (clear) and resume (restore) must each land in the
        // repo's files and leave every link in place.
        let home = TestHome::new();
        let repo = home.path().join("dotfiles");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        let files = [
            (".zshrc", "# user zshrc\n"),
            (".zshenv", "# user zshenv\n"),
            (".zprofile", "# user zprofile\n"),
            (".claude/settings.json", "{}"),
            (".claude.json", "{}"),
            (".codex/config.toml", "# user codex\n"),
        ];
        for (name, body) in files {
            let real = repo.join(name.replace('/', "_"));
            fs::write(&real, body).unwrap();
            if !super::symlink_file_or_skip(&real, &home.path().join(name)) {
                return;
            }
        }
        seed_installed_rtk();
        let links_intact = || {
            for (name, _) in files {
                let meta = fs::symlink_metadata(home.path().join(name)).unwrap();
                assert!(meta.file_type().is_symlink(), "{name} replaced by a file");
            }
        };
        let repo_has = |needle: &str| {
            files.iter().any(|(name, _)| {
                fs::read_to_string(repo.join(name.replace('/', "_")))
                    .unwrap()
                    .contains(needle)
            })
        };

        super::apply_client_setup("claude_code").expect("apply claude");
        super::apply_client_setup("codex").expect("apply codex");
        links_intact();
        assert!(
            repo_has("127.0.0.1:6767"),
            "routing landed in the repo files"
        );

        super::clear_client_setups().expect("pause");
        links_intact();
        assert!(!repo_has("127.0.0.1:6767"), "pause stripped the repo files");

        super::restore_client_setups();
        links_intact();
        assert!(repo_has("127.0.0.1:6767"), "resume restored the repo files");

        // Every backup stays beside its link, never inside the repo.
        let mut in_repo: Vec<_> = fs::read_dir(&repo)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        in_repo.sort();
        assert_eq!(in_repo.len(), files.len(), "{in_repo:?}");
    }

    #[test]
    fn atomic_write_through_a_dangling_symlink_creates_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("missing");
        let link = dir.path().join("link");
        if !super::symlink_file_or_skip(&target, &link) {
            return;
        }
        super::atomic_write(&link, b"x").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_through_a_read_only_target_replaces_the_link() {
        // home-manager links into the read-only /nix/store: writing through
        // cannot work, so the write falls back to replacing the link.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        let target = store.join("settings.json");
        std::fs::write(&target, b"{}").unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o555)).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = super::atomic_write(&link, b"new");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        assert!(!std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&link).unwrap(), b"new");
        assert_eq!(std::fs::read(&target).unwrap(), b"{}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_a_dangling_link_into_a_tree_it_cannot_create() {
        // ~/.zshrc -> /Volumes/Data/dotfiles/zshrc before the volume mounts:
        // the caller built the contents from an empty file, so replacing the
        // link would leave a regular file holding only Headroom's block.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let volumes = dir.path().join("Volumes");
        std::fs::create_dir_all(&volumes).unwrap();
        std::fs::set_permissions(&volumes, std::fs::Permissions::from_mode(0o555)).unwrap();
        let link = dir.path().join(".zshrc");
        std::os::unix::fs::symlink(volumes.join("Data").join("zshrc"), &link).unwrap();

        let result = super::atomic_write(&link, b"# headroom block\n");
        std::fs::set_permissions(&volumes, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn atomic_write_on_a_symlink_cycle_still_writes() {
        // A cycle cannot be followed; fall back to replacing the link rather
        // than failing the write.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        if !super::symlink_file_or_skip(&b, &a) {
            return;
        }
        assert!(super::symlink_file_or_skip(&a, &b));
        super::atomic_write(&a, b"x").unwrap();
    }

    #[test]
    fn atomic_write_concurrent_same_path_no_enoent() {
        // Regression for Sentry RUST-3W / RUST-4W: a shared `<path>.tmp` made
        // concurrent writers race — one rename consumed the tmp, the other hit
        // ENOENT. Unique per-writer tmp names must let all writers succeed.
        let dir = std::env::temp_dir().join(format!("aw_race_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let body = format!("{{\"n\":{i}}}");
                    super::atomic_write(&p, body.as_bytes())
                })
            })
            .collect();
        for h in handles {
            h.join()
                .unwrap()
                .expect("concurrent atomic_write must not ENOENT");
        }
        assert!(path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_rewrites_a_tmp_that_vanished_before_the_rename() {
        // RUST-EZ: on Windows a scanner removed the tmp between fsync and
        // rename, so the write was lost with "os error 2" and every caller of
        // this primitive silently failed to persist.
        let mut renames = 0;
        let mut writes = 0;
        let out = super::rename_recovering_lost_tmp(
            &mut || {
                renames += 1;
                if renames == 1 {
                    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                } else {
                    Ok(())
                }
            },
            &mut || {
                writes += 1;
                Ok(())
            },
        );
        assert!(out.is_ok());
        assert_eq!((renames, writes), (2, 1));

        // A tmp that keeps vanishing is not a race: report it instead of
        // looping.
        let mut renames = 0;
        let mut writes = 0;
        let out = super::rename_recovering_lost_tmp(
            &mut || {
                renames += 1;
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            },
            &mut || {
                writes += 1;
                Ok(())
            },
        );
        assert_eq!(out.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!((renames, writes), (2, 1));

        // Anything else is returned as-is, with no rewrite.
        let mut writes = 0;
        let out = super::rename_recovering_lost_tmp(
            &mut || Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
            &mut || {
                writes += 1;
                Ok(())
            },
        );
        assert_eq!(out.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(writes, 0);
    }

    #[test]
    fn atomic_write_error_names_the_io_cause() {
        // RUST-77: callers log this with `{err}`, which drops the anyhow
        // source, so Sentry only ever saw "writing <path>.tmp.N". The cause
        // must survive plain Display.
        let dir = std::env::temp_dir().join(format!("aw_cause_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Parent exists (atomic_write creates it now), so force the failure on
        // the tmp write itself: an over-long name is ENAMETOOLONG on unix and
        // ERROR_FILENAME_EXCED_RANGE on Windows, both with an "(os error N)".
        let path = dir.join("s".repeat(300));
        let err = super::atomic_write(&path, b"x").expect_err("write into a missing dir must fail");
        let shown = format!("{err}");
        assert!(shown.starts_with("writing "), "{shown}");
        // Match on the "(os error N)" suffix every platform's io::Error Display
        // carries, not the message text: ENOENT reads "No such file or
        // directory" on unix but "The system cannot find the path specified."
        // on Windows, so a unix-worded assertion fails CI on Windows while the
        // cause it checks for is present.
        assert!(
            shown.contains("os error"),
            "io cause missing from `{{err}}`: {shown}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_dir_all_retry_clears_readonly_instead_of_giving_up() {
        // Sentry RUST-6T: uninstall died on Windows "Access is denied (os error
        // 5)". Retrying cannot fix a read-only tree -- the error is deterministic,
        // so all 5 attempts fail identically. The rescue pass must clear the
        // read-only bits and get the delete through.
        //
        // Shaped like the real failure: a venv-ish tree with a read-only file
        // inside a read-only nested directory. On Unix the read-only DIRECTORY
        // blocks unlinking its children, so the rescue pass is load-bearing. On
        // Windows, std's remove_dir_all deletes read-only entries itself since
        // rust-lang/rust#129800 (FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE),
        // so there this only checks the helper handles a read-only tree.
        let root = std::env::temp_dir().join(format!("rdo_retry_{}", std::process::id()));
        // Via the helper, not plain remove: a read-only tree left by an earlier
        // run (recycled pid) would otherwise survive setup and break this test.
        super::remove_dir_all_retry(&root).ok();
        let nested = root.join("Lib").join("site-packages");
        std::fs::create_dir_all(&nested).unwrap();
        let locked_file = nested.join("RECORD");
        std::fs::write(&locked_file, b"x").unwrap();

        for p in [locked_file.as_path(), nested.as_path()] {
            let mut perms = std::fs::metadata(p).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(p, perms).unwrap();
        }
        // Precondition: a plain remove_dir_all really is blocked, else this test
        // would pass even with the rescue pass deleted. Unix-only: modern
        // Windows std ignores the read-only attribute (see header comment), so
        // no such precondition can hold there.
        #[cfg(unix)]
        assert!(
            std::fs::remove_dir_all(&root).is_err(),
            "read-only tree must block a plain remove_dir_all, or this test proves nothing"
        );

        super::remove_dir_all_retry(&root).expect("read-only tree must be removed");
        assert!(!root.exists(), "tree still present after retry helper");
    }

    #[test]
    fn remove_dir_all_retry_is_ok_on_a_missing_path() {
        let missing = std::env::temp_dir().join(format!("rdo_absent_{}", std::process::id()));
        std::fs::remove_dir_all(&missing).ok();
        assert!(super::remove_dir_all_retry(&missing).is_ok());
    }

    #[test]
    fn purge_dir_tolerantly_never_removes_the_nsis_uninstaller() {
        // Deleting it leaves the registry's UninstallString pointing at nothing
        // if anything later in the uninstall section aborts, and the installer
        // then fails instantly with "Unable to uninstall!". NSIS removes it
        // itself once its own section has finished.
        let root = std::env::temp_dir().join(format!("purge_keeps_un_{}", std::process::id()));
        super::remove_dir_all_retry(&root).ok();
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::write(root.join("runtime").join("python"), b"x").unwrap();
        let uninstaller = root.join("uninstall.exe");
        std::fs::write(&uninstaller, b"nsis").unwrap();

        let result = super::purge_dir_tolerantly(&root);

        assert!(
            uninstaller.exists(),
            "the uninstaller must survive the sweep"
        );
        assert!(!root.join("runtime").exists(), "runtime survived the sweep");
        assert!(
            result.is_err(),
            "the dir cannot go while the uninstaller is still in it"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn purge_dir_tolerantly_skips_past_an_undeletable_entry() {
        // The 0.8.8-rc.2 Windows uninstall: one `remove_dir_all` over the app
        // dir stopped at the running Headroom.exe, so `config` was gone (terms
        // re-prompted on reinstall) while `runtime` survived and the reinstall
        // reported an installation already present. One undeletable entry must
        // not strand the entries the walk had not reached yet.
        let root = std::env::temp_dir().join(format!("purge_tolerant_{}", std::process::id()));
        super::remove_dir_all_retry(&root).ok();
        for child in ["config", "runtime"] {
            std::fs::create_dir_all(root.join(child).join("nested")).unwrap();
            std::fs::write(root.join(child).join("nested").join("f"), b"x").unwrap();
        }
        // Stand-in for the running exe. A mode with no read or execute bit stays
        // undeletable through the rescue pass in remove_dir_all_retry, which
        // only ever adds owner *write* (0o200).
        #[cfg(unix)]
        let blocked = {
            use std::os::unix::fs::PermissionsExt;
            let blocked = root.join("blocked");
            std::fs::create_dir_all(&blocked).unwrap();
            std::fs::write(blocked.join("keep"), b"x").unwrap();
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert!(
                std::fs::read_dir(&blocked).is_err(),
                "entry must really be undeletable, or this test proves nothing"
            );
            blocked
        };

        let result = super::purge_dir_tolerantly(&root);

        assert!(!root.join("config").exists(), "config survived the sweep");
        assert!(!root.join("runtime").exists(), "runtime survived the sweep");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(result.is_err(), "a partial sweep must report the failure");
            assert!(
                blocked.exists(),
                "the undeletable entry should still be there"
            );
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_dir_all(&root).unwrap();
        }
        #[cfg(not(unix))]
        {
            assert!(result.is_ok(), "nothing blocked the sweep: {result:?}");
            assert!(!root.exists(), "the dir itself should be gone");
        }
    }

    #[test]
    #[serial_test::serial]
    fn load_setup_state_falls_back_to_default_on_corrupt_file() {
        let _home = TestHome::new();
        let path = super::setup_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Simulate a torn / partial write that would have happened with the
        // pre-fix non-atomic writer. The retry path inside load_setup_state
        // re-reads after a short backoff and, when the file is still bad,
        // logs a warning and returns the default rather than panicking.
        std::fs::write(&path, b"{ not json").unwrap();

        let state = super::load_setup_state();
        assert!(state.configured_clients.is_empty());
        assert!(state.remembered_clients.is_empty());
    }

    fn seed_codex_threads_db(path: &Path, rows: &[(&str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT NOT NULL)",
            [],
        )
        .unwrap();
        for (id, provider) in rows {
            conn.execute(
                "INSERT INTO threads (id, model_provider) VALUES (?, ?)",
                [id, provider],
            )
            .unwrap();
        }
    }

    fn provider_count(path: &Path, provider: &str) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE model_provider = ?1",
            [provider],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn retag_one_codex_db_moves_only_matching_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state_5.sqlite");
        seed_codex_threads_db(
            &db,
            &[
                ("a", "openai"),
                ("b", "openai"),
                ("c", "headroom"),
                ("d", "anthropic"),
            ],
        );

        let moved = retag_one_codex_db(&db, "openai", "headroom").unwrap();
        assert_eq!(moved, Some(2));
        assert_eq!(provider_count(&db, "openai"), 0);
        assert_eq!(provider_count(&db, "headroom"), 3);
        // Third-party providers are untouched.
        assert_eq!(provider_count(&db, "anthropic"), 1);

        // Reverse direction round-trips only the headroom rows.
        let back = retag_one_codex_db(&db, "headroom", "openai").unwrap();
        assert_eq!(back, Some(3));
        assert_eq!(provider_count(&db, "headroom"), 0);
        assert_eq!(provider_count(&db, "openai"), 3);
        assert_eq!(provider_count(&db, "anthropic"), 1);
    }

    #[test]
    fn retag_one_codex_db_noop_without_threads_table() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state_5.sqlite");
        // Open creates an empty DB with no `threads` table.
        Connection::open(&db).unwrap();
        assert_eq!(retag_one_codex_db(&db, "openai", "headroom").unwrap(), None);
    }

    #[test]
    #[serial_test::serial]
    fn retag_codex_thread_providers_silent_when_no_store() {
        let _home = TestHome::new();
        // No ~/.codex stores exist under the temp home: must not panic.
        retag_codex_thread_providers("openai", "headroom");
    }

    #[test]
    #[serial_test::serial]
    fn codex_sqlite_store_expected_gates_on_state_file_not_dir() {
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        // CLI-only / pre-sqlite Codex: config + sessions but no sqlite/ store.
        std::fs::create_dir_all(codex.join("sessions")).unwrap();
        std::fs::write(codex.join("config.toml"), "").unwrap();
        assert!(
            !codex_sqlite_store_expected(),
            "config/sessions alone must not trigger the moved-store warning"
        );
        // sqlite/ dir holding only unrelated stores (logs/goals/memories) but no
        // thread store must NOT fire -- the false positive behind Sentry RUST-3R.
        std::fs::create_dir_all(codex.join("sqlite")).unwrap();
        std::fs::write(codex.join("sqlite").join("logs_2.sqlite"), "").unwrap();
        std::fs::write(codex.join("sqlite").join("goals_1.sqlite"), "").unwrap();
        assert!(
            !codex_sqlite_store_expected(),
            "unrelated sqlite stores must not trigger the moved-store warning"
        );
        // CLI store renamed loose in codex_home (version no longer parses) ->
        // expected, so the relocation gets flagged.
        std::fs::write(codex.join("state_5x.sqlite"), "").unwrap();
        assert!(codex_sqlite_store_expected());
        std::fs::remove_file(codex.join("state_5x.sqlite")).unwrap();
        // GUI thread store present under sqlite/ -> expected.
        std::fs::write(codex.join("sqlite").join("state_6.sqlite"), "").unwrap();
        assert!(codex_sqlite_store_expected());
    }

    #[test]
    #[serial_test::serial]
    fn retag_codex_threads_to_headroom_pulls_native_threads_back() {
        // Reproduces the app-update restart path: the quit handler left threads
        // tagged `openai`; launch must retag them back to `headroom`.
        let home = TestHome::new();
        let db = home.path().join(".codex").join("state_5.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        seed_codex_threads_db(&db, &[("a", "openai"), ("b", "openai"), ("c", "anthropic")]);

        retag_codex_threads_to_headroom();

        assert_eq!(provider_count(&db, "headroom"), 2);
        assert_eq!(provider_count(&db, "openai"), 0);
        // Third-party threads are untouched.
        assert_eq!(provider_count(&db, "anthropic"), 1);
    }

    #[test]
    #[serial_test::serial]
    fn codex_activity_is_rollouts_not_the_thread_store() {
        // Our launch retag and an idle `codex app-server` both write the thread
        // store with no turn; either read as "Codex ran" fired RUST-KC.
        let home = TestHome::new();
        let db = home.path().join(".codex").join("state_5.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        seed_codex_threads_db(&db, &[("a", "openai")]);
        retag_codex_threads_to_headroom();
        let wal = db.with_extension("sqlite-wal");
        std::fs::write(&wal, b"idle app-server").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&wal)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(super::client_local_activity_at("codex"), None);

        // Nor does a date directory with no rollout in it yet (0.9.27 KC).
        let day = home.path().join(".codex/sessions/2026/09/30");
        std::fs::create_dir_all(&day).unwrap();
        assert_eq!(super::client_local_activity_at("codex"), None);

        // A turn appends a rollout, and that does count.
        std::fs::write(day.join("rollout-x.jsonl"), b"{}\n").unwrap();
        assert!(super::client_local_activity_at("codex").is_some());
    }

    #[test]
    #[serial_test::serial]
    fn codex_home_honors_env_else_default() {
        let home = TestHome::new();
        // TestHome clears CODEX_HOME, so we fall back to $HOME/.codex.
        assert_eq!(codex_home(), home.path().join(".codex"));

        let custom = home.path().join("custom-codex");
        std::env::set_var("CODEX_HOME", &custom);
        assert_eq!(codex_home(), custom);

        // An empty value is ignored (treated as unset).
        std::env::set_var("CODEX_HOME", "");
        assert_eq!(codex_home(), home.path().join(".codex"));
    }

    #[test]
    #[serial_test::serial]
    fn pin_codex_mcp_command_rewrites_only_headroom_table() {
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let config = codex.join("config.toml");
        std::fs::write(
            &config,
            "# --- Headroom MCP server ---\n\
             [mcp_servers.headroom]\n\
             command = \"headroom\"\n\
             args = [\"mcp\", \"serve\"]\n\
             \n\
             [mcp_servers.headroom.env]\n\
             HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n\
             \n\
             [mcp_servers.node_repl]\n\
             command = \"/Applications/Codex.app/node_repl\"\n",
        )
        .unwrap();

        let entrypoint = home.path().join("App Support/venv/bin/headroom");
        let changed = pin_codex_mcp_command(&entrypoint).unwrap();
        assert!(changed.is_some(), "config should have been rewritten");

        let after = std::fs::read_to_string(&config).unwrap();
        let abs = entrypoint.display().to_string();
        // Compare parsed values, not raw text: TOML escapes Windows path
        // backslashes on write, so the raw file never contains `abs` verbatim.
        let parsed: toml::Value = toml::from_str(&after).expect("rewritten config parses");
        assert_eq!(
            parsed["mcp_servers"]["headroom"]["command"].as_str(),
            Some(abs.as_str()),
            "headroom command pinned to absolute path, got:\n{after}"
        );
        // The unrelated server's command must be untouched.
        assert!(after.contains("command = \"/Applications/Codex.app/node_repl\""));
        // The headroom env sub-table has no `command`; nothing spurious added.
        assert_eq!(after.matches("command = ").count(), 2);
        // The upstream beacon is turned off for Headroom's MCP server only.
        assert_eq!(
            parsed["mcp_servers"]["headroom"]["env"]["HEADROOM_BEACON"].as_str(),
            Some("off")
        );
        assert!(parsed["mcp_servers"]["node_repl"].get("env").is_none());

        // Idempotent: a second run with the same entrypoint is a no-op.
        assert!(pin_codex_mcp_command(&entrypoint).unwrap().is_none());

        // A beacon the upstream registrar turned on is turned off, once.
        std::fs::write(
            &config,
            after.replace(r#"HEADROOM_BEACON = "off""#, r#"HEADROOM_BEACON = "on""#),
        )
        .unwrap();
        assert!(pin_codex_mcp_command(&entrypoint).unwrap().is_some());
        let after = std::fs::read_to_string(&config).unwrap();
        assert_eq!(after.matches("HEADROOM_BEACON").count(), 1, "{after}");
        assert!(after.contains(r#"HEADROOM_BEACON = "off""#));
    }

    #[test]
    #[serial_test::serial]
    fn pin_codex_mcp_command_normalizes_python_module_args() {
        // Upstream may register `<python> -m headroom.cli mcp serve`. Pinning
        // command to the console script must also rewrite the args, otherwise
        // `headroom -m headroom.cli ...` fails with "No such option '-m'".
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let config = codex.join("config.toml");
        std::fs::write(
            &config,
            "[mcp_servers.headroom]\n\
             command = \"/somewhere/venv/bin/python3\"\n\
             args = [\"-m\", \"headroom.cli\", \"mcp\", \"serve\"]\n\
             \n\
             [mcp_servers.headroom.env]\n\
             HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n",
        )
        .unwrap();

        let entrypoint = home.path().join("venv/bin/headroom");
        assert!(pin_codex_mcp_command(&entrypoint).unwrap().is_some());

        let after = std::fs::read_to_string(&config).unwrap();
        assert!(
            after.contains("args = [\"mcp\", \"serve\"]"),
            "python -m args must be normalized, got:\n{after}"
        );
        assert!(!after.contains("-m"), "no -m leftovers, got:\n{after}");
    }

    #[test]
    #[serial_test::serial]
    fn pin_codex_mcp_command_handles_multi_line_args_array() {
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let config = codex.join("config.toml");
        std::fs::write(
            &config,
            "[mcp_servers.headroom]\n\
             command = \"/somewhere/venv/bin/python3\"\n\
             args = [\n  \"-m\",\n  \"headroom.cli\",\n  \"mcp\",\n  \"serve\",\n]\n\
             \n\
             [mcp_servers.headroom.env]\n\
             HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n",
        )
        .unwrap();

        let entrypoint = home.path().join("venv/bin/headroom");
        assert!(pin_codex_mcp_command(&entrypoint).unwrap().is_some());

        let after = std::fs::read_to_string(&config).unwrap();
        // No orphaned continuation lines — the rebuilt file must parse.
        let parsed: toml::Value = toml::from_str(&after).expect("rebuilt config parses");
        assert_eq!(
            parsed["mcp_servers"]["headroom"]["args"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(after.contains("[mcp_servers.headroom.env]"));
        assert!(!after.contains("headroom.cli"));
    }

    /// Regression: the Grok pin rewrote only `command`, so a registrar that
    /// wrote `<python> -m headroom.cli mcp serve` left Grok spawning
    /// `headroom -m headroom.cli ...`, which click rejects, and the MCP server
    /// never started in Grok.
    #[test]
    #[serial_test::serial]
    fn pin_grok_mcp_command_normalizes_python_module_args() {
        let home = TestHome::new();
        let grok = home.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        let config = grok.join("config.toml");
        std::fs::write(
            &config,
            "[mcp_servers.headroom]\n\
             command = \"/somewhere/venv/bin/python3\"\n\
             args = [\n  \"-m\",\n  \"headroom.cli\",\n  \"mcp\",\n  \"serve\",\n]\n\
             \n\
             [mcp_servers.headroom.env]\n\
             HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n",
        )
        .unwrap();

        let entrypoint = home.path().join("venv/bin/headroom");
        assert!(super::pin_grok_mcp_command(&entrypoint).unwrap().is_some());

        let after = std::fs::read_to_string(&config).unwrap();
        let parsed: toml::Value = toml::from_str(&after).expect("rebuilt config parses");
        let server = &parsed["mcp_servers"]["headroom"];
        assert_eq!(
            server["command"].as_str(),
            Some(entrypoint.to_string_lossy().as_ref())
        );
        assert_eq!(
            server["args"],
            toml::Value::Array(vec!["mcp".into(), "serve".into()]),
            "python -m args must be normalized, got:\n{after}"
        );
        assert!(after.contains("[mcp_servers.headroom.env]"));
    }

    /// Faithful port of the wheel's `CodexRegistrar.register_server(force=True)`
    /// for a spec that differs from the file (always, once Rust pinned the
    /// command): `unregister_server` deletes everything between the markers,
    /// then `_write_block` appends a fresh span (headroom/mcp_registry/codex.py
    /// and grok.py, 0.39.0).
    fn wheel_force_register(content: &str, block: &str) -> String {
        let (ms, me) = (
            "# --- Headroom MCP server ---",
            "# --- end Headroom MCP server ---",
        );
        let mut content = content.to_string();
        if let (Some(start), Some(end)) = (content.find(ms), content.find(me)) {
            let before = content[..start].trim_end_matches('\n');
            let after = content[end + me.len()..].trim_start_matches('\n');
            content = if !before.is_empty() && !after.is_empty() {
                format!("{before}\n\n{after}")
            } else {
                let rest = if before.is_empty() { after } else { before };
                let rest = rest.trim_end_matches('\n');
                if rest.is_empty() {
                    String::new()
                } else {
                    format!("{rest}\n")
                }
            };
        }
        if content.trim().is_empty() {
            format!("{block}\n")
        } else {
            format!("{}\n\n{block}\n", content.trim_end_matches('\n'))
        }
    }

    /// The span the wheel's `_write_block` appends for its own spec.
    const WHEEL_BLOCK: &str = "# --- Headroom MCP server ---\n\
         [mcp_servers.headroom]\n\
         command = \"headroom\"\n\
         args = [\"mcp\", \"serve\"]\n\
         \n\
         [mcp_servers.headroom.env]\n\
         HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n\
         # --- end Headroom MCP server ---";

    const NODE_REPL_TABLES: &str = "[mcp_servers.node_repl]\n\
         args = []\n\
         command = \"/Applications/ChatGPT.app/node_repl\"\n\
         startup_timeout_sec = 120\n\
         \n\
         [mcp_servers.node_repl.env]\n\
         BROWSER_USE_AVAILABLE_BACKENDS = \"chrome,iab\"\n\
         NODE_REPL_TRUSTED_SERVICES = '{\"browser\":\"x.mjs\"}'\n";

    /// The ChatGPT app appends its tables before the document's trailing
    /// comment, so with our span last they land inside it.
    fn codex_config_with_trapped_node_repl(headroom_command: &str) -> String {
        format!(
            "model = \"gpt-5\"\n\
             \n\
             [tui]\n\
             screen_reader_detection_done = true\n\
             # --- Headroom MCP server ---\n\
             [mcp_servers.headroom]\n\
             command = \"{headroom_command}\"\n\
             args = [\"mcp\", \"serve\"]\n\
             \n\
             [mcp_servers.headroom.env]\n\
             HEADROOM_PROXY_URL = \"http://127.0.0.1:6767\"\n\
             \n\
             {NODE_REPL_TABLES}\
             # --- end Headroom MCP server ---\n"
        )
    }

    fn assert_node_repl_intact(after: &str) {
        assert!(
            after.contains(NODE_REPL_TABLES.trim_end()),
            "node_repl tables lost or altered:\n{after}"
        );
        let parsed: toml::Value = toml::from_str(after).expect("config parses");
        assert_eq!(
            parsed["mcp_servers"]["node_repl"]["env"]["BROWSER_USE_AVAILABLE_BACKENDS"].as_str(),
            Some("chrome,iab")
        );
        // Outside the span and not after it: the end marker stays the
        // document trailer rather than the prefix of the app's table.
        let start = after.find("# --- Headroom MCP server ---").unwrap();
        assert!(
            after.find("[mcp_servers.node_repl]").unwrap() < start,
            "node_repl not moved before the Headroom span:\n{after}"
        );
        assert_eq!(
            after.trim_end().lines().last(),
            Some("# --- end Headroom MCP server ---"),
            "end marker is no longer the trailer:\n{after}"
        );
    }

    /// What Codex's toml_edit writer does when the ChatGPT app drops a
    /// server: the table goes with its prefix decor, i.e. every blank and
    /// comment line between the previous key and its header (checked against
    /// toml_edit 0.22). Comments before EOF are the document trailer and stay.
    fn toml_edit_remove_server(content: &str, name: &str) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let header = |l: &str| l.trim_start().starts_with('[');
        let decor = |l: &str| l.trim().is_empty() || l.trim_start().starts_with('#');
        let mut keep = vec![true; lines.len()];
        for i in 0..lines.len() {
            if super::mcp_table_name(lines[i]).as_deref() != Some(name) {
                continue;
            }
            let mut from = i;
            while from > 0 && decor(lines[from - 1]) {
                from -= 1;
            }
            let mut to = i + 1;
            while to < lines.len() && !header(lines[to]) {
                to += 1;
            }
            while to > i + 1 && decor(lines[to - 1]) {
                to -= 1;
            }
            keep[from..to].iter_mut().for_each(|k| *k = false);
        }
        let mut out: Vec<&str> = lines
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(l, _)| *l)
            .collect();
        out.push("");
        out.join("\n")
    }

    /// The wheel appends each span at EOF and the Codex provider block can sit
    /// right above them, so the spot before our span's start marker can be
    /// directly under another Headroom end marker. Evacuating there made that
    /// marker node_repl's prefix, and the app dropping node_repl took it.
    #[test]
    fn an_evacuated_table_never_sits_under_another_headroom_marker() {
        let cbm_span = "# --- Headroom MCP server: codebase-memory ---\n\
             [mcp_servers.codebase-memory]\n\
             command = \"cbm\"\n\
             # --- end Headroom MCP server: codebase-memory ---";
        let provider = "# >>> headroom:codex_cli_provider >>>\n\
             [model_providers.headroom]\n\
             name = \"Headroom\"\n\
             # <<< headroom:codex_cli_provider <<<";
        let trapped = codex_config_with_trapped_node_repl("headroom");
        let (head, span) = trapped.split_at(trapped.find("# --- Headroom MCP server ---").unwrap());
        for above in [cbm_span.to_string(), format!("{provider}\n\n{cbm_span}")] {
            let config = format!("{head}{above}\n\n{span}");
            let evacuated = super::rescue_foreign_toml_from_mcp_spans(&config);
            let before_config: toml::Value = toml::from_str(&config).unwrap();
            assert_eq!(
                toml::from_str::<toml::Value>(&evacuated).unwrap(),
                before_config
            );
            let dropped = toml_edit_remove_server(&evacuated, "node_repl");
            for marker in [
                "# --- Headroom MCP server: codebase-memory ---",
                "# --- end Headroom MCP server: codebase-memory ---",
                "# --- Headroom MCP server ---",
                "# --- end Headroom MCP server ---",
            ] {
                assert!(dropped.contains(marker), "{marker} lost:\n{dropped}");
            }
            if above.contains("codex_cli_provider") {
                assert!(
                    dropped.contains("# <<< headroom:codex_cli_provider <<<"),
                    "{dropped}"
                );
            }
        }
        // A block of root keys is never jumped: a table above it takes them.
        let root = "# >>> headroom:codex_cli >>>\nopenai_base_url = \"http://127.0.0.1:6767/v1\"\n# <<< headroom:codex_cli <<<";
        let config = format!(
            "{root}\n\n{}",
            &trapped[trapped.find("# --- Headroom").unwrap()..]
        );
        let evacuated = super::rescue_foreign_toml_from_mcp_spans(&config);
        let parsed: toml::Value = toml::from_str(&evacuated).unwrap();
        assert_eq!(
            parsed["openai_base_url"].as_str(),
            Some("http://127.0.0.1:6767/v1")
        );
        assert!(evacuated.starts_with(root), "{evacuated}");
    }

    /// rc12 gate: the evacuation put node_repl right after our end marker,
    /// which toml_edit then owns as node_repl's prefix. The app dropping
    /// node_repl (browser-use turned off) took the end marker with it; the
    /// wheel's force re-register could then not unregister and appended a
    /// second `[mcp_servers.headroom]`, so Codex refused the whole config.
    #[test]
    fn the_app_dropping_an_evacuated_table_keeps_both_span_markers() {
        let pinned = "/Apps/Headroom/venv/bin/headroom";
        let evacuated =
            super::rescue_foreign_toml_from_mcp_spans(&codex_config_with_trapped_node_repl(pinned));
        assert_node_repl_intact(&evacuated);
        let dropped = toml_edit_remove_server(&evacuated, "node_repl");
        assert!(!dropped.contains("node_repl"), "{dropped}");
        assert!(dropped.contains("# --- Headroom MCP server ---\n[mcp_servers.headroom]"));
        assert!(
            dropped.contains("# --- end Headroom MCP server ---"),
            "{dropped}"
        );
        let reinstalled = wheel_force_register(&dropped, WHEEL_BLOCK);
        let parsed: toml::Value = toml::from_str(&reinstalled)
            .unwrap_or_else(|err| panic!("reinstall broke the config: {err}\n{reinstalled}"));
        assert_eq!(
            parsed["mcp_servers"]["headroom"]["command"].as_str(),
            Some("headroom")
        );

        // The heal's re-added tables sit before the span the same way.
        let _home = TestHome::new();
        let config = super::codex_config_toml_path();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            config.with_file_name("config.toml.headroom-backup-20260930060313"),
            codex_config_with_trapped_node_repl(pinned),
        )
        .unwrap();
        std::fs::write(
            &config,
            wheel_force_register("model = \"gpt-5\"\n", WHEEL_BLOCK),
        )
        .unwrap();
        let healed = super::restore_lost_mcp_span_tables(
            &config,
            &std::fs::read_to_string(&config).unwrap(),
        );
        assert_node_repl_intact(&healed);
        assert!(toml_edit_remove_server(&healed, "node_repl")
            .contains("# --- end Headroom MCP server ---"));
    }

    /// rc11 data loss: runtime maintenance ran `headroom mcp install --force`,
    /// the wheel saw the Rust-pinned command differ from its own spec, and its
    /// unregister deleted the ChatGPT app's browser-use/computer-use servers
    /// that sat inside our marker span. The desktop must move them out before
    /// (and after) every registrar run.
    #[test]
    #[serial_test::serial]
    fn foreign_tables_in_the_mcp_span_survive_the_wheels_force_reinstall() {
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let config = codex.join("config.toml");
        let pinned = "/Apps/Headroom/venv/bin/headroom";
        std::fs::write(&config, codex_config_with_trapped_node_repl(pinned)).unwrap();

        assert!(super::protect_foreign_mcp_tables_in(&config, false).unwrap());
        let evacuated = std::fs::read_to_string(&config).unwrap();
        assert_node_repl_intact(&evacuated);
        // Nothing but the move: the parsed config is unchanged.
        assert_eq!(
            toml::from_str::<toml::Value>(&evacuated).unwrap(),
            toml::from_str::<toml::Value>(&codex_config_with_trapped_node_repl(pinned)).unwrap()
        );

        std::fs::write(&config, wheel_force_register(&evacuated, WHEEL_BLOCK)).unwrap();
        pin_codex_mcp_command(Path::new(pinned)).unwrap();
        super::protect_foreign_mcp_tables_in(&config, false).unwrap();
        let after = std::fs::read_to_string(&config).unwrap();
        assert!(
            after.contains("[mcp_servers.node_repl]"),
            "wheel reinstall dropped node_repl:\n{after}"
        );
        let parsed: toml::Value = toml::from_str(&after).expect("config parses");
        assert_eq!(
            parsed["mcp_servers"]["node_repl"]["command"].as_str(),
            Some("/Applications/ChatGPT.app/node_repl")
        );
        assert_eq!(
            parsed["mcp_servers"]["headroom"]["command"].as_str(),
            Some(pinned)
        );
        // Idempotent once clean.
        assert!(!super::protect_foreign_mcp_tables_in(&config, false).unwrap());
    }

    /// Heal for machines rc11 already damaged: a `.headroom-backup-*` still
    /// holds the tables the wheel deleted from inside the span. Restore those
    /// the live file lacks, outside the span; never overwrite a table the live
    /// file has (the app may have re-added it with newer values).
    #[test]
    #[serial_test::serial]
    fn lost_mcp_span_tables_are_restored_from_a_headroom_backup() {
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let config = codex.join("config.toml");
        let pinned = "/Apps/Headroom/venv/bin/headroom";
        let damaged_backup = codex_config_with_trapped_node_repl(pinned).replace(
            "# --- end Headroom MCP server ---\n",
            "[mcp_servers.chrome]\ncommand = \"old-chrome\"\n# --- end Headroom MCP server ---\n",
        );
        std::fs::write(
            codex.join("config.toml.headroom-backup-20260930060313"),
            &damaged_backup,
        )
        .unwrap();
        // The live file after the wheel's unregister+append: node_repl gone,
        // chrome re-added by the app with a newer command.
        let live = format!(
            "model = \"gpt-5\"\n\
             \n\
             [tui]\n\
             screen_reader_detection_done = true\n\
             \n\
             [mcp_servers.chrome]\n\
             command = \"new-chrome\"\n\
             \n\
             # --- Headroom MCP server ---\n\
             [mcp_servers.headroom]\n\
             command = \"{pinned}\"\n\
             args = [\"mcp\", \"serve\"]\n\
             # --- end Headroom MCP server ---\n"
        );
        std::fs::write(&config, &live).unwrap();

        super::protect_foreign_mcp_tables();
        let after = std::fs::read_to_string(&config).unwrap();
        assert_node_repl_intact(&after);
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["chrome"]["command"].as_str(),
            Some("new-chrome")
        );
        assert_eq!(
            after.replace(&format!("{}\n\n", NODE_REPL_TABLES.trim_end()), ""),
            live,
            "live content moved"
        );

        // Review: the heal ran on every call while a backup held the table,
        // so a server the user turned off afterwards came back (three times,
        // until the backups rotated out). It runs once.
        let removed = after.replace(NODE_REPL_TABLES.trim_end(), "");
        std::fs::write(&config, &removed).unwrap();
        super::protect_foreign_mcp_tables();
        let again = std::fs::read_to_string(&config).unwrap();
        assert!(
            !again.contains("node_repl"),
            "removed server restored:\n{again}"
        );
    }

    /// Review: the heal read the backup the evacuation itself had just written
    /// (node_repl still inside the span), so a server the user removed after
    /// the evacuation came back.
    #[test]
    #[serial_test::serial]
    fn a_server_removed_after_the_evacuation_stays_removed() {
        let _home = TestHome::new();
        let config = super::codex_config_toml_path();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            codex_config_with_trapped_node_repl("/Apps/Headroom/venv/bin/headroom"),
        )
        .unwrap();
        super::protect_foreign_mcp_tables();
        let evacuated = std::fs::read_to_string(&config).unwrap();
        assert_node_repl_intact(&evacuated);

        // The user turns browser-use off; the ChatGPT app deletes node_repl.
        std::fs::write(&config, evacuated.replace(NODE_REPL_TABLES.trim_end(), "")).unwrap();
        super::protect_foreign_mcp_tables();
        let after = std::fs::read_to_string(&config).unwrap();
        assert!(
            !after.contains("node_repl"),
            "removed server restored:\n{after}"
        );
    }

    /// Review: the heal took any header inside a backup's span as restorable.
    /// A subtable whose server was since deleted came back as an orphan
    /// `[mcp_servers.gone.env]` (no `command`, which Codex rejects), and an
    /// `[[array]]` entry the live file still has was appended again (TOML
    /// allows another entry, so the parse check passed).
    #[test]
    #[serial_test::serial]
    fn the_heal_restores_no_orphan_subtable_or_duplicate_array_entry() {
        let _home = TestHome::new();
        let config = super::codex_config_toml_path();
        let dir = config.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let span = "# --- Headroom MCP server ---\n\
             [mcp_servers.headroom]\n\
             command = \"/Apps/Headroom/venv/bin/headroom\"\n";
        let end = "# --- end Headroom MCP server ---\n";
        std::fs::write(
            dir.join("config.toml.headroom-backup-20260930060313"),
            format!(
                "[mcp_servers.gone]\ncommand = \"gone\"\n\n{span}\n\
                 [mcp_servers.gone.env]\nA = \"1\"\n\n[[hooks]]\nname = \"a\"\n{end}"
            ),
        )
        .unwrap();
        let live = format!("{span}{end}\n[[hooks]]\nname = \"a\"\n");
        std::fs::write(&config, &live).unwrap();

        super::protect_foreign_mcp_tables();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), live);
    }

    /// Review: ownership compared header text, so our own table spelled
    /// `[ mcp_servers."headroom" ]` (the same TOML key) counted as foreign and
    /// was moved out of the span. The wheel's re-register then appended a
    /// second `[mcp_servers.headroom]` and Codex refused the config.
    #[test]
    #[serial_test::serial]
    fn a_respelled_headroom_table_stays_in_its_span() {
        let home = TestHome::new();
        let config = home.path().join("config.toml");
        std::fs::write(
            &config,
            codex_config_with_trapped_node_repl("/Apps/Headroom/venv/bin/headroom")
                .replace("[mcp_servers.headroom]\n", "[ mcp_servers.\"headroom\" ]\n"),
        )
        .unwrap();
        super::protect_foreign_mcp_tables_in(&config, false).unwrap();
        let evacuated = std::fs::read_to_string(&config).unwrap();
        assert_node_repl_intact(&evacuated);

        let reinstalled = wheel_force_register(&evacuated, WHEEL_BLOCK);
        let parsed: toml::Value = toml::from_str(&reinstalled)
            .unwrap_or_else(|err| panic!("reinstall broke the config: {err}\n{reinstalled}"));
        assert_eq!(
            parsed["mcp_servers"]["node_repl"]["command"].as_str(),
            Some("/Applications/ChatGPT.app/node_repl")
        );
    }

    /// Review: protect is a read-modify-write of ~/.codex/config.toml run from
    /// the maintenance and add-on threads, so it must not interleave with an
    /// apply writing the provider block to the same file.
    #[test]
    #[serial_test::serial]
    fn protecting_mcp_tables_waits_for_an_apply_in_flight() {
        let _home = TestHome::new();
        assert_waits_for_setup_writes(super::protect_foreign_mcp_tables);
    }

    /// Regression: the Learn backend took the first `codex` that merely
    /// existed, so an x86_64 leftover on an arm64 Mac without Rosetta (or any
    /// binary that cannot run) was handed to `headroom learn` and failed every
    /// run. Candidates must pass the same smoke test the MCP/plugin paths use.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn detect_codex_cli_skips_a_codex_that_does_not_run() {
        use std::os::unix::fs::PermissionsExt;
        let home = TestHome::new();
        let bin = home.path().join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let broken = bin.join("codex");
        std::fs::write(&broken, b"\x00\x01\x02\x03not a binary").unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(super::detect_codex_cli(), Some(broken));
    }

    #[test]
    #[serial_test::serial]
    fn discover_codex_state_dbs_finds_any_sqlite_regardless_of_name() {
        let home = TestHome::new();
        let codex = home.path().join(".codex");
        std::fs::create_dir_all(codex.join("sqlite")).unwrap();
        // GUI store under sqlite/, CLI store at the root, plus a renamed store
        // whose name no longer follows the `state_<N>` scheme -- discovery is
        // content-based now, so it must still be picked up (the actual fix).
        std::fs::File::create(codex.join("sqlite").join("state_6.sqlite")).unwrap();
        std::fs::File::create(codex.join("state_5.sqlite")).unwrap();
        std::fs::File::create(codex.join("sqlite").join("threads.sqlite")).unwrap();
        // A non-sqlite file in the same dir must be ignored.
        std::fs::File::create(codex.join("config.toml")).unwrap();

        let names: BTreeSet<String> = discover_codex_state_dbs()
            .into_iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "state_6.sqlite".to_owned(),
                "state_5.sqlite".to_owned(),
                "threads.sqlite".to_owned(),
            ])
        );
    }

    #[test]
    #[serial_test::serial]
    fn retag_handles_unknown_store_version() {
        // Future-proofing: a Codex store-version bump (here state_99) must still
        // retag, not silently no-op for every user at once.
        let home = TestHome::new();
        let db = home.path().join(".codex").join("state_99.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        seed_codex_threads_db(&db, &[("a", "openai"), ("b", "openai"), ("c", "anthropic")]);

        retag_codex_threads_to_headroom();

        assert_eq!(provider_count(&db, "headroom"), 2);
        assert_eq!(provider_count(&db, "openai"), 0);
        assert_eq!(provider_count(&db, "anthropic"), 1);
    }

    #[test]
    #[serial_test::serial]
    fn retag_handles_store_renamed_off_state_scheme() {
        // The regression this change fixes: Codex renames the store off the
        // `state_<N>.sqlite` scheme entirely. Content-based discovery must still
        // find and retag it by its `threads` table, not the filename.
        let home = TestHome::new();
        let db = home
            .path()
            .join(".codex")
            .join("sqlite")
            .join("threads.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        seed_codex_threads_db(&db, &[("a", "openai"), ("b", "openai"), ("c", "anthropic")]);

        retag_codex_threads_to_headroom();

        assert_eq!(provider_count(&db, "headroom"), 2);
        assert_eq!(provider_count(&db, "openai"), 0);
        assert_eq!(provider_count(&db, "anthropic"), 1);
    }

    #[test]
    fn claude_guard_script_is_diagnostic_and_reachable_tolerates_any_response() {
        let script = build_claude_guard_script();
        // reachable() is a TCP accept on the intercept port: an HTTP probe is
        // forwarded to the backend and reads "down" under load (false
        // SessionStart hook errors), and a 503-during-bypass is still "up".
        assert!(!script.contains("return response.status < 500"));
        assert!(!script.contains("urllib"));
        assert!(script.contains("socket.create_connection(ADDR, timeout=2).close()"));
        // main() explains WHY instead of the flat "is not" message.
        assert!(script.contains("def diagnose_route"));
        assert!(script.contains("overrides Headroom's route"));
        assert!(!script.contains("ANTHROPIC_BASE_URL is not \" + BASE_URL"));
        // A correct user settings + unset process env (GUI / `open` launch) is
        // healthy and must NOT trigger the old "restart Claude Code" nag.
        assert!(!script.contains("did not inherit the Headroom shell env"));
        assert!(script.contains("user_val != BASE_URL and effective != BASE_URL"));
        // Notifications are debounced and reachability retries once, so an app
        // relaunch doesn't produce a notification storm.
        assert!(script.contains("DEBOUNCE_PATH.touch()"));
        assert!(script.contains("time.sleep(2)\n    return probe()"));
    }

    #[test]
    fn codex_guard_script_names_actual_values_and_tolerates_any_response() {
        let script = build_codex_guard_script();
        assert!(!script.contains("return response.status < 500"));
        assert!(!script.contains("urllib"));
        assert!(script.contains("socket.create_connection(ADDR, timeout=2).close()"));
        // Messages include the actual found value, not just "is not headroom".
        assert!(script.contains("(expected \"headroom\")"));
        assert!(script.contains("(expected \" + BASE_URL + \")"));
        assert!(script.contains("DEBOUNCE_PATH.touch()"));
        assert!(script.contains("time.sleep(2)\n    return probe()"));
    }

    // --- Open-source plugin coexistence -------------------------------------
    //
    // The four states from the investigation. `headroom init hook ensure` (the
    // plugin's only command) never writes ANTHROPIC_BASE_URL and never picks a
    // port, so the whole surface we own is: does a `headroom` exist on PATH for
    // the hook to run, and did we avoid touching anyone who already had one.

    fn seed_oss_plugin(home: &Path, plugin_ref: &str) -> PathBuf {
        let dir = home.join(".claude").join("plugins");
        fs::create_dir_all(&dir).unwrap();
        let install = dir
            .join("cache")
            .join(plugin_ref.replace('@', "-"))
            .join("0.36.5");
        let hooks = install.join("hooks").join("hooks.json");
        fs::create_dir_all(hooks.parent().unwrap()).unwrap();
        fs::write(
            &hooks,
            json!({
                "hooks": {
                    "SessionStart": [{
                        "hooks": [{ "type": "command", "command": super::OSS_PLUGIN_HOOK_COMMAND }]
                    }],
                    "PreToolUse": [{
                        "matcher": "Bash|PowerShell",
                        "hooks": [{ "type": "command", "command": super::OSS_PLUGIN_HOOK_COMMAND }]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            dir.join("installed_plugins.json"),
            json!({
                "version": 2,
                "plugins": { plugin_ref: [{
                    "scope": "user",
                    "version": "0.36.5",
                    "installPath": install
                }] }
            })
            .to_string(),
        )
        .unwrap();
        hooks
    }

    /// Every file under `root`, with its bytes, for before/after comparison.
    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = fs::read(&path) {
                    out.insert(path, bytes);
                }
            }
        }
        out
    }

    #[test]
    fn state_1_app_alone_touches_nothing_on_disk() {
        // The guarantee for the majority of users: someone running the desktop
        // app without the open-source plugin must come out of this byte-for-byte
        // unchanged. Not "no shim" — nothing at all.
        let home = TestHome::new();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude/settings.json"),
            json!({ "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:6767" } }).to_string(),
        )
        .unwrap();
        fs::create_dir_all(home.path().join(".local/bin")).unwrap();

        let before = snapshot_tree(home.path());
        let status = super::absorb_oss_plugin_with_cli_on_path(false);
        let after = snapshot_tree(home.path());

        assert_eq!(before, after, "no-plugin users must see zero writes");
        assert!(!status.plugin_installed);
        assert!(!status.hook_absorbed);
        assert!(
            status.base_url_ours,
            "our routing is left exactly as it was"
        );
    }

    #[test]
    fn state_1_app_alone_stays_inert() {
        let _home = TestHome::new();

        let status = super::absorb_oss_plugin_with_cli_on_path(false);

        assert!(!status.plugin_installed);
        assert!(!status.hook_absorbed);
        assert!(!super::oss_plugin_hook_receipt_path().exists());
    }

    #[test]
    fn state_2_plugin_without_cli_gets_a_noop_hook() {
        let home = TestHome::new();
        let hooks = seed_oss_plugin(home.path(), "headroom@headroom-marketplace");

        let status = super::absorb_oss_plugin_with_cli_on_path(false);

        assert!(status.plugin_installed);
        assert!(status.hook_absorbed);
        let raw = fs::read_to_string(&hooks).unwrap();
        assert_eq!(
            raw.matches(&serde_json::to_string(super::OSS_PLUGIN_MANAGED_COMMAND).unwrap())
                .count(),
            2
        );
        assert!(!raw.contains(super::OSS_PLUGIN_HOOK_COMMAND));
    }

    #[test]
    fn plugin_is_recognised_from_any_marketplace_mirror() {
        // The same plugin is listed under several marketplaces (sleetish,
        // burgebj, oll4com) whose hooks.json are byte-identical.
        for plugin_ref in ["headroom@headroom-marketplace", "headroom@sleetish"] {
            let home = TestHome::new();
            seed_oss_plugin(home.path(), plugin_ref);

            assert!(
                super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed,
                "{plugin_ref} should be recognised"
            );
        }
    }

    #[test]
    fn unrelated_plugins_do_not_trigger_the_hook_rewrite() {
        let home = TestHome::new();
        seed_oss_plugin(home.path(), "ponytail@ponytail");

        assert!(!super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);
        assert!(!super::oss_plugin_hook_receipt_path().exists());
    }

    #[test]
    fn state_3_real_cli_on_path_is_never_shadowed() {
        let home = TestHome::new();
        let hooks = seed_oss_plugin(home.path(), "headroom@headroom-marketplace");
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);

        let status = super::absorb_oss_plugin_with_cli_on_path(true);

        assert!(status.cli_on_path);
        assert!(!status.hook_absorbed);
        assert_eq!(
            fs::read_to_string(hooks)
                .unwrap()
                .matches(super::OSS_PLUGIN_HOOK_COMMAND)
                .count(),
            2
        );
    }

    /// Regression: `state_3` proves the bool is honoured, but the bool itself
    /// came from `find_on_path` alone, and a GUI launch inherits launchd's bare
    /// PATH -- no `~/.local/bin`, which is where the OSS installer puts
    /// `headroom`. Every such user read as "no CLI" and had a working plugin
    /// hook neutralized. Asserts only the positive direction: a machine that
    /// really does have a `headroom` elsewhere cannot make this pass wrongly.
    #[test]
    #[cfg(unix)]
    fn an_oss_cli_in_local_bin_counts_even_when_path_cannot_see_it() {
        use std::os::unix::fs::PermissionsExt;

        let home = TestHome::new();
        let cli = home.path().join(".local/bin/headroom");
        fs::create_dir_all(cli.parent().unwrap()).unwrap();
        fs::write(&cli, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&cli).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&cli, perms).unwrap();

        assert!(super::oss_cli_present());
    }

    /// The replacement must stay a shell builtin, not a path to anything we
    /// ship. A path goes dead the moment our app data is removed or moved,
    /// and takes the restore string -- our only way back -- with it.
    #[test]
    fn the_managed_hook_command_is_not_a_path() {
        assert_eq!(super::OSS_PLUGIN_MANAGED_COMMAND, "exit 0");
    }

    #[test]
    fn removing_the_plugin_restores_its_hook_and_clears_the_receipt() {
        let home = TestHome::new();
        let hooks = seed_oss_plugin(home.path(), "headroom@headroom-marketplace");
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);

        seed_oss_plugin(home.path(), "ponytail@ponytail");
        let status = super::absorb_oss_plugin_with_cli_on_path(false);
        assert!(!status.plugin_installed);
        assert!(!status.hook_absorbed);
        assert!(!super::oss_plugin_hook_receipt_path().exists());
        assert_eq!(
            fs::read_to_string(&hooks)
                .unwrap()
                .matches(super::OSS_PLUGIN_HOOK_COMMAND)
                .count(),
            2
        );
    }

    #[test]
    fn a_foreign_headroom_binary_is_never_clobbered() {
        let home = TestHome::new();
        let shim = home.path().join(".local/bin/headroom");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        fs::write(&shim, "#!/bin/sh\necho not ours\n").unwrap();
        seed_oss_plugin(home.path(), "headroom@headroom-marketplace");

        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);
        assert_eq!(
            fs::read_to_string(&shim).unwrap(),
            "#!/bin/sh\necho not ours\n"
        );
    }

    #[test]
    fn failed_restore_preserves_the_receipt_for_a_later_retry() {
        let home = TestHome::new();
        let hooks = seed_oss_plugin(home.path(), "headroom@headroom-marketplace");
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);

        let managed = serde_json::to_string(super::OSS_PLUGIN_MANAGED_COMMAND).unwrap();
        let corrupt = format!("{} trailing", fs::read_to_string(&hooks).unwrap());
        fs::write(&hooks, corrupt).unwrap();

        super::perform_full_cleanup();

        assert!(super::app_data_dir().exists());
        assert!(super::oss_plugin_hook_receipt_path().exists());
        assert!(fs::read_to_string(hooks).unwrap().contains(&managed));
    }

    #[test]
    fn state_4_oss_proxy_bypass_is_measured_not_fought() {
        // The user ran `headroom init` themselves, so their ANTHROPIC_BASE_URL
        // points at the open-source proxy. We report it and change nothing.
        let home = TestHome::new();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let settings = home.path().join(".claude/settings.json");
        let original = json!({ "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787" } });
        fs::write(&settings, original.to_string()).unwrap();
        seed_oss_plugin(home.path(), "headroom@headroom-marketplace");

        let status = super::absorb_oss_plugin_with_cli_on_path(false);

        assert!(status.plugin_installed);
        assert!(
            !status.base_url_ours,
            "the bypass must be visible to telemetry"
        );
        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            original.to_string(),
            "we never rewrite a base URL the user set on purpose"
        );
    }

    /// A plugin update re-clones into a fresh version dir carrying the bare
    /// command, which one startup pass can never see. The poll must catch that
    /// and nothing else: a user we do not manage must not be dragged back
    /// through the exec probe every five minutes forever.
    #[test]
    fn the_recheck_fires_only_for_a_fresh_hook_we_are_already_managing() {
        let home = TestHome::new();
        seed_oss_plugin(home.path(), "headroom@headroom-marketplace");

        // Nobody managed yet: an untouched plugin is not the recheck's job.
        assert!(!super::oss_plugin_hook_needs_absorbing());

        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);
        assert!(
            !super::oss_plugin_hook_needs_absorbing(),
            "steady state must stay quiet"
        );

        // Claude Code updates the plugin: a new version dir, bare command back.
        let updated = seed_oss_plugin(home.path(), "headroom@headroom-marketplace-v2");
        assert!(super::oss_plugin_hook_needs_absorbing());
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);
        assert!(!fs::read_to_string(&updated)
            .unwrap()
            .contains(super::OSS_PLUGIN_HOOK_COMMAND));
        assert!(!super::oss_plugin_hook_needs_absorbing());

        // A real CLI appears, we hand the hook back, and the poll must not
        // immediately claim it again.
        assert!(!super::absorb_oss_plugin_with_cli_on_path(true).hook_absorbed);
        assert!(
            !super::oss_plugin_hook_needs_absorbing(),
            "a restored user must not be re-absorbed on a timer"
        );
    }

    #[test]
    fn the_recheck_respects_the_kill_switch() {
        let home = TestHome::new();
        seed_oss_plugin(home.path(), "headroom@headroom-marketplace");
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);
        seed_oss_plugin(home.path(), "headroom@headroom-marketplace-v2");
        assert!(super::oss_plugin_hook_needs_absorbing());

        std::env::set_var("HEADROOM_ABSORB_OSS_PLUGIN", "0");
        let needs = super::oss_plugin_hook_needs_absorbing();
        std::env::remove_var("HEADROOM_ABSORB_OSS_PLUGIN");

        assert!(!needs);
    }

    #[test]
    fn the_kill_switch_absorbs_nothing_and_restores_what_it_finds() {
        let home = TestHome::new();
        let hooks = seed_oss_plugin(home.path(), "headroom@headroom-marketplace");
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);

        std::env::set_var("HEADROOM_ABSORB_OSS_PLUGIN", "0");
        let status = super::absorb_oss_plugin_with_cli_on_path(false);
        std::env::remove_var("HEADROOM_ABSORB_OSS_PLUGIN");

        assert!(status.plugin_installed);
        assert!(!status.hook_absorbed);
        assert_eq!(
            fs::read_to_string(&hooks)
                .unwrap()
                .matches(super::OSS_PLUGIN_HOOK_COMMAND)
                .count(),
            2,
            "the opt-out must hand the hook back, not just stop touching it"
        );
    }

    #[test]
    fn absorbing_the_hook_does_not_hide_a_real_oss_remnant() {
        let home = TestHome::new();
        seed_oss_plugin(home.path(), "headroom@headroom-marketplace");
        assert!(super::absorb_oss_plugin_with_cli_on_path(false).hook_absorbed);

        let foreign = home.path().join(".local/bin/headroom");
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::write(&foreign, "#!/bin/sh\necho real OSS CLI\n").unwrap();

        assert!(
            super::detect_oss_remnants()
                .iter()
                .any(|w| w.contains("~/.local/bin/headroom")),
            "absorbing the plugin hook must not hide a real OSS CLI"
        );
    }
    /// The provider env keys are what make a third-party endpoint actually
    /// answer (Andrew's GLM setup). Clearing the provider must take all of
    /// them back out again rather than leaving Claude Code pinned to a model
    /// Anthropic has never heard of.
    #[test]
    #[serial_test::serial]
    fn provider_env_round_trips_through_claude_settings() {
        let home = TestHome::new();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, r#"{"env": {"USER_KEY": "keep me"}}"#).unwrap();

        let glm = super::provider_preset("glm").expect("glm preset exists");
        let glm_env = || super::ProviderClientEnv {
            model: glm.model,
            small_model: glm.small_model,
            context_window: glm.context_window,
        };
        let mut replaced = BTreeMap::new();
        super::apply_upstream_provider_env(Some(glm_env()), None, &mut replaced).unwrap();
        let written = read_settings_json(&settings);
        assert_eq!(written["env"]["API_TIMEOUT_MS"].as_str(), Some("3000000"));
        assert_eq!(
            written["env"]["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"].as_str(),
            Some("1")
        );
        assert_eq!(
            written["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"].as_str(),
            Some(glm.context_window)
        );
        for slot in super::PROVIDER_MODEL_SLOT_ENV {
            assert_eq!(written["env"][slot].as_str(), Some(glm.model), "{slot}");
        }
        // The cheap tier must NOT be pointed at the big model.
        assert_eq!(
            written["env"][super::PROVIDER_SMALL_MODEL_SLOT_ENV].as_str(),
            Some(glm.small_model)
        );

        super::apply_upstream_provider_env(None, Some(glm_env()), &mut replaced).unwrap();
        let cleared = read_settings_json(&settings);
        let env = cleared["env"].as_object().expect("env survives");
        for key in super::PROVIDER_MODEL_SLOT_ENV.iter().chain(
            [
                "API_TIMEOUT_MS",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            ]
            .iter(),
        ) {
            assert!(!env.contains_key(*key), "{key} still set after clearing");
        }
        assert_eq!(env["USER_KEY"].as_str(), Some("keep me"));
    }

    fn preset_override(id: &str) -> crate::state::UpstreamOverride {
        let preset = super::provider_preset(id).expect("preset exists");
        crate::state::UpstreamOverride {
            mode: crate::state::UpstreamOverrideMode::Override,
            base_url: preset.base_url.into(),
            provider: id.into(),
            model: preset.model.into(),
            small_model: preset.small_model.into(),
            context_window: preset.context_window.into(),
            ..Default::default()
        }
    }

    /// Env a cc-switch or gateway user keeps in ~/.claude/settings.json that
    /// Headroom never wrote.
    const USER_CLAUDE_ENV: &str = r#"{"env": {
        "ANTHROPIC_AUTH_TOKEN": "sk-user-gateway",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "API_TIMEOUT_MS": "600000",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "user-bedrock-opus"
    }}"#;

    fn assert_user_claude_env(settings: &Path) {
        let env = read_settings_json(settings)["env"].clone();
        assert_eq!(
            env["ANTHROPIC_AUTH_TOKEN"].as_str(),
            Some("sk-user-gateway")
        );
        assert_eq!(
            env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"].as_str(),
            Some("1")
        );
        assert_eq!(env["API_TIMEOUT_MS"].as_str(), Some("600000"));
        assert_eq!(
            env["ANTHROPIC_DEFAULT_OPUS_MODEL"].as_str(),
            Some("user-bedrock-opus")
        );
        for key in [
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
        ] {
            assert!(env.get(key).is_none(), "{key} left behind: {env}");
        }
    }

    /// Audit #30: saving the panel as "Anthropic (default)" with no provider
    /// ever configured deleted the user's own token and env keys, because the
    /// clear matched whatever value was there.
    #[test]
    #[serial_test::serial]
    fn saving_anthropic_from_off_keeps_the_users_own_claude_env() {
        let home = TestHome::new();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, USER_CLAUDE_ENV).unwrap();

        let mut next = crate::state::UpstreamOverride::default();
        super::apply_upstream_client_config(&Default::default(), &mut next, None).unwrap();

        assert_user_claude_env(&settings);
        assert!(!next.has_token);
    }

    /// Audit #17: a provider round trip overwrote the user's own values on the
    /// way in and deleted Headroom's on the way out, so the originals never
    /// came back.
    #[test]
    #[serial_test::serial]
    fn a_provider_round_trip_puts_the_users_claude_env_back() {
        let home = TestHome::new();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, USER_CLAUDE_ENV).unwrap();

        let mut glm = preset_override("glm");
        super::apply_upstream_client_config(&Default::default(), &mut glm, Some("sk-glm")).unwrap();
        let env = read_settings_json(&settings)["env"].clone();
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"].as_str(), Some("sk-glm"));
        assert_eq!(env["API_TIMEOUT_MS"].as_str(), Some("3000000"));
        assert_eq!(
            env["ANTHROPIC_DEFAULT_OPUS_MODEL"].as_str(),
            Some(glm.model.as_str())
        );
        // The credential it replaced is not written to launch-profile.json.
        assert!(!glm.replaced_env.contains_key("ANTHROPIC_AUTH_TOKEN"));

        let mut off = crate::state::UpstreamOverride::default();
        super::apply_upstream_client_config(&glm, &mut off, None).unwrap();

        assert_user_claude_env(&settings);
        assert!(off.replaced_env.is_empty());
        assert_eq!(crate::upstream_override::read_token(), None);
    }

    /// Audit #31: the token was written before the context window was
    /// checked, so a rejected save left a provider token live in the client
    /// config with the upstream still on Anthropic.
    #[test]
    #[serial_test::serial]
    fn a_rejected_context_window_writes_no_token() {
        let home = TestHome::new();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{}").unwrap();

        let mut custom = crate::state::UpstreamOverride {
            mode: crate::state::UpstreamOverrideMode::Override,
            base_url: "https://gateway.example.com/anthropic".into(),
            context_window: "200k".into(),
            ..Default::default()
        };
        let err =
            super::apply_upstream_client_config(&Default::default(), &mut custom, Some("sk-x"))
                .unwrap_err();

        assert!(err.contains("context window"), "{err}");
        assert!(read_settings_json(&settings)["env"]["ANTHROPIC_AUTH_TOKEN"].is_null());
        assert_eq!(crate::upstream_override::read_token(), None);
    }

    /// Audit #72: switching provider with the token field untouched re-applied
    /// the previous provider's key, which then went to the new provider.
    #[test]
    #[serial_test::serial]
    fn an_untouched_token_is_not_carried_to_another_provider() {
        let home = TestHome::new();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{}").unwrap();

        let mut glm = preset_override("glm");
        super::apply_upstream_client_config(&Default::default(), &mut glm, Some("sk-glm")).unwrap();
        // Same provider, field untouched: still re-applied.
        let mut again = preset_override("glm");
        super::apply_upstream_client_config(&glm, &mut again, None).unwrap();
        assert!(again.has_token);

        let mut kimi = preset_override("kimi");
        super::apply_upstream_client_config(&again, &mut kimi, None).unwrap();

        assert!(!kimi.has_token);
        assert!(read_settings_json(&settings)["env"]["ANTHROPIC_AUTH_TOKEN"].is_null());
        assert_eq!(crate::upstream_override::read_token(), None);
    }
}
