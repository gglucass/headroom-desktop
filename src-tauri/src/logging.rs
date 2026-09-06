// Panic-safe file logger.
//
// Background: macOS LaunchServices does not guarantee stderr is connected
// to a valid fd when it spawns the app to handle a URL scheme, file
// association, or login item. Rust's `eprintln!`/`println!` macros panic
// on write failure, and a panic that crosses an ObjC -> Rust callback
// (e.g. the deep-link handler) aborts the whole process.
//
// This logger writes to a file under the platform's log directory and
// forwards Warn/Error records to Sentry. All write failures are swallowed
// so a logging failure can never crash the app.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use log::{Level, Log, Metadata, Record, SetLoggerError};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const SENTRY_MESSAGE_CHAR_CAP: usize = 400;

struct FileLogger {
    file: Mutex<Option<File>>,
    path: PathBuf,
    records_since_rotate_check: std::sync::atomic::AtomicU64,
}

impl FileLogger {
    fn write_record(&self, record: &Record, display_level: Level) {
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let _ = writeln!(
            file,
            "{ts} {level:<5} {target}: {msg}",
            level = display_level,
            target = record.target(),
            msg = record.args(),
        );
        let _ = file.flush();
    }

    fn rotate_if_needed(&self) {
        let metadata = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return,
        };
        if metadata.len() < MAX_LOG_BYTES {
            return;
        }
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        // Drop the current handle before renaming so Windows can't hold it open;
        // also necessary on macOS for log inspection while the app runs.
        *guard = None;
        let backup = self.path.with_extension("log.old");
        let _ = fs::remove_file(&backup);
        let _ = fs::rename(&self.path, &backup);
        if let Ok(f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            *guard = Some(f);
        }
    }
}

fn is_transient_transport_error(msg: &str) -> bool {
    msg.contains("error sending request")
        || msg.contains("dns error")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("operation timed out")
        || msg.contains("network is unreachable")
        || msg.contains("os error 50") // macOS: Network is down
        || msg.contains("os error 51") // macOS: Network is unreachable
        || msg.contains("os error 65") // macOS: No route to host
}

// The user's disk is full. Every persisted file (pricing state, usage counters,
// activity facts, client configs) fails the same way, from ~50 atomic_write
// callers, so this fragments into one un-fixable issue per call site. Nothing
// in the app can free space; the write is retried on the next tick and heals
// itself once the user clears the disk.
fn is_disk_full(msg: &str) -> bool {
    msg.contains("No space left on device") // unix ENOSPC
        || (cfg!(windows) && msg.contains("os error 112")) // Windows ERROR_DISK_FULL
}

// Non-2xx response from the update endpoint. Most commonly a transient 5xx
// from GitHub releases or a 404 during a tag-publish race — not actionable.
fn is_updater_endpoint_error(msg: &str) -> bool {
    msg.contains("update endpoint did not respond with a successful status code")
}

// tao panics with this on Windows session end (sign-out, shutdown, restart): the
// event loop reaches a window whose state the OS already tore down while handling
// WM_ENDSESSION. Our own teardown has run by then and the process is exiting
// either way, so nothing is lost and no release can change the outcome. Fixed in
// tao 0.37.0, which tauri-runtime-wry cannot take yet -- 2.11.4, the latest, still
// requires `tao ^0.35.0`. RUST-84 and RUST-8D are the same panic from two more
// hosts. Drop the event; re-check when tauri moves the pin.
fn is_windows_session_end_panic(msg: &str) -> bool {
    msg.contains("cannot move state from Destroyed")
}

// The whole machine is out of file descriptors (ENFILE). Like disk-full this
// is a system-wide resource the app cannot free -- it is not our per-process
// limit, so no leak of ours is the cause and no release can change the outcome
// -- and it fails every file touch identically, so it fragments into one
// un-fixable issue per call site (RUST-A3 on the usage-counters write, RUST-5T
// on the client-setup read, same class). Every affected read/write is retried
// on its next tick and heals once the machine has descriptors again.
//
// ENFILE only. EMFILE ("Too many open files", os error 24) is the PER-PROCESS
// limit and would mean we are leaking descriptors -- that one stays reportable.
fn is_system_fd_exhaustion(msg: &str) -> bool {
    msg.contains("Too many open files in system") // unix ENFILE
}

// Environmental or otherwise unfixable-by-release: keep the local log, never
// send. One predicate so panics and log records answer it the same way.
fn is_unreportable(msg: &str) -> bool {
    is_disk_full(msg) || is_system_fd_exhaustion(msg) || is_windows_session_end_panic(msg)
}

// Drop transient transport errors (offline laptop, flaky wifi, upstream blip)
// from Sentry. They still hit the local log file via write_record.
fn skip_sentry(target: &str, msg: &str) -> bool {
    // Environmental and target-agnostic: keep the local log, drop the event.
    if is_disk_full(msg) || is_system_fd_exhaustion(msg) {
        return true;
    }
    // A line announcing that a Sentry capture was skipped must never itself be
    // captured -- the bridge forwarding it re-created exactly the event the
    // skip existed to prevent, minus all its structure (RUST-AR: the
    // bootstrap_failed ENOSPC skip filed 7 events). Target-agnostic on
    // purpose: every capture site uses this phrasing.
    if msg.starts_with("skipping Sentry capture for") {
        return true;
    }
    // tiktoken prefetch is best-effort warm-up, same contract as the kompress
    // prefetch below: the proxy lazy-loads the tokenizer on first request if
    // the prefetch fails, and the dominant failure is the host's own
    // network/proxy environment (RUST-AP). Local log only; the repeat
    // suppression at the emit site already demotes follow-ups to info.
    if target.starts_with("headroom_desktop_lib") && msg.starts_with("tiktoken prefetch failed") {
        return true;
    }
    if target.starts_with("tauri_plugin_updater") {
        return is_transient_transport_error(msg) || is_updater_endpoint_error(msg);
    }
    // proxy_intercept bypass forwarders (plain + websocket-upgrade variant):
    // when CC is bypassing the local Python proxy and we re-issue directly to
    // the upstream API, transient network failures aren't actionable — client
    // already gets a 502 and CC retries. The upgrade variant was missed by the
    // original prefix and accumulated as RUST-2R (393 events, all transport).
    if target.starts_with("headroom_desktop_lib::proxy_intercept")
        && (msg.starts_with("proxy_intercept bypass forward failed")
            || msg.starts_with("proxy_intercept bypass upgrade forward failed"))
    {
        return is_transient_transport_error(msg);
    }
    // The accept loop self-heals: it backs off and keeps accepting. A transient
    // EMFILE (or similar) under load isn't actionable as a Sentry event.
    if target.starts_with("headroom_desktop_lib::proxy_intercept")
        && msg.starts_with("[proxy_intercept] accept error")
    {
        return true;
    }
    // A held intercept port reaches Sentry via the explicit once-per-error
    // capture at the emit site (RUST-62); this warn repeats on every 15s bind
    // retry and only duplicated it (RUST-5R). Local log only.
    //
    // Matched on the retry marker rather than one variant's prose: the emit
    // site now says three different things about a held port (draining after a
    // restart, stuck past the drain window, held by a named process), and
    // keying on any single wording is what made this fragile before. Every
    // bind-retry warn ends in the same "retrying in 15s" and every one of them
    // has an explicit capture at the emit site, so they all belong here.
    // `skips_foreign_port_bind_retry_warns` is the guard; keep its fixtures
    // copies of the real messages.
    if target.starts_with("headroom_desktop_lib::proxy_intercept")
        && msg.starts_with("[proxy_intercept] port")
        && msg.contains("retrying in 15s")
    {
        return true;
    }
    // report_codex_upstream_error logs the RAW upstream body locally and then
    // captures a separate, status-fingerprinted event with only the structural
    // summary. The bridge defeated both halves (RUST-5Q): the raw body — which
    // quotes the user's request fields — left the machine, and because this
    // line carries no fingerprint Sentry parameterized it into one grab-bag
    // mixing 400/403/503/507. The capture at the emit site is the Sentry path.
    // Matched as "<client> upstream error " rather than a per-client prefix
    // list: report_upstream_error emits one line per client_key
    // (claude-code/codex/opencode/grok-build), and a hardcoded list here is
    // exactly what let the bypass-forward variant slip through as RUST-2R.
    if target.starts_with("headroom_desktop_lib::proxy_intercept")
        && msg
            .split_once(' ')
            .is_some_and(|(_, rest)| rest.starts_with("upstream error "))
    {
        return true;
    }
    // Kompress prefetch is best-effort; the proxy lazy-loads the model on first
    // request if this fails. These two variants carry no actionable detail (the
    // spawn error is rare and the restart self-heals on next request), so they
    // are pure noise. The "download error" variant is NOT suppressed — it
    // carries a classified cause and is the systemic signal worth tracking.
    if target.starts_with("headroom_desktop_lib::state")
        && (msg.starts_with("kompress prefetch failed")
            || msg.starts_with("kompress prefetch: restart after download failed"))
    {
        return true;
    }
    // The download-error variant reaches Sentry via an explicit
    // category-fingerprinted capture_message at the emit site (RUST-3C
    // grab-bag split); the accompanying log::warn is local-only.
    if target.starts_with("headroom_desktop_lib::state")
        && msg.starts_with("kompress prefetch download error")
    {
        return true;
    }
    // Same split for the /stats probe: the reason is in the message, so a
    // 15s timeout and an HTTP 404 grouped as one issue (RUST-6V) that no fix
    // could ever resolve. The category-fingerprinted capture at the emit site
    // is the Sentry path.
    if target.starts_with("headroom_desktop_lib::state")
        && msg.starts_with("headroom /stats fetch failed")
    {
        return true;
    }
    // The direct-wheel fallback warn embeds the full PyPI URL, so message
    // grouping opened a fresh issue per wheel version and per platform tag for
    // one condition (RUST-22). `report_wheel_download_fallback` captures it
    // fingerprinted on the cause class instead; this line stays local.
    if target.starts_with("headroom_desktop_lib::tool_manager")
        && msg.starts_with("headroom wheel download failed")
    {
        return true;
    }
    // Boot-validation failure reaches Sentry via the fully-tagged Level::Error
    // capture at the same emit site (capture_runtime_upgrade_failure, RUST-4A:
    // versions, boot diagnostics, pip tail); this bridged warn double-reports
    // the same incident as RUST-2N with none of that context.
    if target.starts_with("headroom_desktop_lib::state")
        && msg.starts_with("run_upgrade_with_ui: boot validation failed")
    {
        return true;
    }
    // Two more warns from the same incident: the new venv's spawn error and
    // the not-started short-circuit that follows it. Both already ride in the
    // tagged capture (`ensure_headroom_running_error`, outcome=not_started).
    // Bridged, the spawn warn grouped on its message, which embeds the venv
    // path and the prior-attempts wording, so one host filed RUST-CW and
    // RUST-CZ a minute apart with RUST-CX beside them.
    if target.starts_with("headroom_desktop_lib::state")
        && (msg.starts_with("run_upgrade_with_ui: new proxy failed to spawn")
            || msg.starts_with("run_upgrade_with_ui: skipping boot validation"))
    {
        return true;
    }
    // The retag summary's "unreadable" variant: when any candidate DB failed
    // to open, "no `threads` table found" proves nothing about a rename
    // (RUST-95 false-fired the rename canary on a machine whose sqlite files
    // all threw disk I/O errors). The canary itself ("has no `threads` table
    // ... renamed the table") stays reportable.
    if target.starts_with("headroom_desktop_lib::client_adapters")
        && msg.starts_with("codex retag ")
        && msg.contains("candidate(s) unreadable")
    {
        return true;
    }
    // The pip final-failure warn embeds pip's stderr tail, so message-based
    // grouping opened a fresh issue per tail for one underlying failure
    // (RUST-6M/6N/6P, all the same half-built venv). It reaches Sentry via the
    // per-category fingerprinted capture at the emit site instead.
    if target.starts_with("headroom_desktop_lib::tool_manager")
        && msg.starts_with("pip install attempt ")
        && msg.contains("failed (final)")
    {
        return true;
    }
    // Same split as the pip line above: one partial-plugin-install message
    // covered five unrelated causes under one fingerprint (RUST-6K), so it was
    // untriageable AND unresolvable -- any resolve regressed on the next
    // sibling shape. It reaches Sentry via the per-category fingerprinted
    // capture at the emit site instead.
    if target.starts_with("headroom_desktop_lib::tool_manager")
        && msg.contains("installed for some hosts but not all")
    {
        return true;
    }
    // Ad-hoc codesign of venv native extensions is best-effort (EDR nicety):
    // codesign exits non-zero when a single .so can't be re-signed, but the
    // rest are signed and the smoke test is the real gate. A per-file failure
    // isn't actionable, so keep the log line but drop the Sentry event.
    if target.starts_with("headroom_desktop_lib::tool_manager")
        && msg.starts_with("ad-hoc codesign exited")
    {
        return true;
    }
    // Uninstall/cleanup teardown is best-effort by construction. It races a
    // still-exiting backend that re-creates a file mid-walk ("Directory not
    // empty"), a venv Windows still holds open ("Access is denied", RUST-6T),
    // and settings files we deliberately leave alone when they don't parse
    // ("refusing to overwrite potentially valid user settings", RUST-6X --
    // that branch is the correct one, not a failure). The app is being removed
    // either way, so none of it is actionable in a release. Matched by prefix
    // across modules: the same teardown runs from client_adapters (files,
    // settings) and lib (plugins, MCP servers), and every sibling shape landed
    // as its own un-fixable issue.
    if msg.starts_with("cleanup: ") || msg.starts_with("uninstall: removing ") {
        return true;
    }
    // Codex thread retag is best-effort over every *.sqlite in the Codex dirs;
    // a Codex-owned DB corrupted on the user's disk ("database disk image is
    // malformed") is environmental and unfixable by a release. The retag
    // already skips the file; keep the local log, drop the Sentry event.
    // "disk I/O error" is the same environmental class (RUST-95/96: a
    // macOS-beta box failing every sqlite open); "database is locked" is NOT
    // skipped -- recurring lock contention would mean our busy_timeout
    // assumption went stale.
    if target.starts_with("headroom_desktop_lib::client_adapters")
        && msg.starts_with("codex retag")
        && (msg.contains("database disk image is malformed") || msg.contains("disk I/O error"))
    {
        return true;
    }
    // Every one of these lines is the sanitizer WORKING: it found tool_search
    // references with no matching entry in the tools array and dropped them, so
    // upstream never 400s and the session survives. Reporting a successful
    // mitigation as an error-level issue made it un-resolvable (RUST-5W kept
    // regressing, then escalated) -- any resolve reopens the moment another
    // client sends a stale reference. The rate is worth watching as a metric,
    // not as a defect. Keep the local log, which is what support threads read.
    if target.starts_with("headroom_desktop_lib::proxy_intercept")
        && msg.starts_with("[proxy_intercept] dropped ")
        && msg.contains("stale tool_search reference")
    {
        return true;
    }
    // The backend-port fallback reaches Sentry via the explicit capture at the
    // emit site (tool_manager), which carries occupant_cmd/occupant_pid tags and
    // both port numbers. This warn fires at the same instant with none of that
    // context, so one fallback landed as two issues (RUST-7E and RUST-7F, same
    // millisecond). Same split as the intercept-port line above.
    if target.starts_with("headroom_desktop_lib::tool_manager")
        && msg.starts_with("[backend_port] ")
    {
        return true;
    }
    // A host with no usable Secret Service (headless VM, xrdp session with no
    // login keyring) is the case this fallback exists FOR: the 0600 file is the
    // designed path, sign-in works, nothing is broken. It fired once per process
    // as a fresh error-level issue on every Linux box without a desktop keyring
    // (RUST-7G). Keep the local log so a support thread can see which store was
    // used; drop the Sentry event.
    if target.starts_with("headroom_desktop_lib::keychain")
        && msg.starts_with("OS credential store unusable")
    {
        return true;
    }
    // The machine-id digest is a deterministic value (sha256 of the hardware
    // UUID); the keychain write is a best-effort cache whose failure changes
    // nothing (next launch recomputes the same value). Dominant cause is a ghost
    // keychain entry from another app signature — environmental, unfixable here,
    // identical every launch. Keep the local log, drop the Sentry event (RUST-3P
    // / RUST-51: the earlier demote to log::warn still reached Sentry via this
    // logger, so it needs the explicit skip here).
    if target.starts_with("headroom_desktop_lib::device")
        && msg.starts_with("Could not persist machine id digest")
    {
        return true;
    }
    // A tray tooltip that would not apply is cosmetic: the icon, the menu and
    // every action still work, and the updater loop retries on the next tick
    // (the emit site no longer caches the tooltip on failure, so it heals
    // itself). Windows reports the busy notification area as a bare E_FAIL
    // whose text is OS-LOCALIZED -- "Error no especificado" opened RUST-7P as a
    // separate issue from the English wording, so this would fragment into one
    // un-fixable issue per language. Keep the local log, drop the Sentry event.
    if target.starts_with("headroom_desktop_lib") && msg.starts_with("tray: set_tooltip failed") {
        return true;
    }
    // tauri-runtime-wry logs a BARE `{e}` when webview.evaluate_script fails, so
    // every Tauri emit to a webview that is minimized, suspended or already torn
    // down bridges here as an error with no context and no call site. On one
    // Windows host that is a per-emit loop: RUST-6H took 988 events in 24h from
    // `Jan2022` alone, more than the entire fleet's Sentry volume over the same
    // window. Nothing is actionable -- the backend keeps proxying, only the UI
    // misses the event, and it heals when the webview comes back. Match the
    // known transient HRESULT rather than wry's generic prefix so webview
    // creation and startup failures still reach Sentry; keep the local log.
    if target.starts_with("tauri_runtime_wry")
        && msg.starts_with("WebView2 error:")
        && msg.contains("HRESULT(0x8007139F)")
    {
        return true;
    }
    // tauri-utils warns when APPDIR/APPIMAGE is set but the executable is not
    // under an AppImage mount. A .deb/.rpm install inherits those variables
    // from whatever AppImage the user launched us from (RUST-CN: 6 events in
    // 2 minutes on one CachyOS host, one per resource-dir lookup). A property
    // of the launching shell, not of anything we ship; keep the local log.
    if target.starts_with("tauri_utils") && msg.contains("not detected as an AppImage") {
        return true;
    }
    // The canary captures its own fully-scoped event at the emit site (flow
    // tag, sample/zero/strata/models extras, and the fixed `zero_savings_canary`
    // fingerprint that makes the fleet-wide event count the blast radius). This
    // log line fires in the same breath with none of that, so one detection
    // landed as two issues in the same millisecond: RUST-A5 (the capture) and
    // RUST-A4 (this warn, parameterized into its own group because the counts
    // and model list are baked into the text). Same split as the intercept-port
    // and backend-port lines above. Keep the local log -- it is what a support
    // thread reads -- and drop the Sentry twin.
    if target.starts_with("headroom_desktop_lib::savings_canary")
        && msg.starts_with("zero-savings canary:")
    {
        return true;
    }
    // Routing a missing managed runtime back to setup is the RECOVERY path, and
    // the emit site says so: `ensure_runtime_ready_for_tray` deliberately logs
    // instead of calling `capture_headroom_start_failure`, because capturing a
    // not-installed runtime as a startup crash was misleading noise (RUST-1M).
    // The log bridge defeated that intent -- the warn reached Sentry anyway as
    // RUST-8W, and because it interpolates `{err:#}` (program, full argv, log
    // tail) it fragmented per command line on top. The app re-runs bootstrap
    // from the setup window, so there is no failure left to report.
    if target.starts_with("headroom_desktop_lib")
        && msg.starts_with("ensure_runtime_ready_for_tray: managed runtime missing")
    {
        return true;
    }
    // The detached-proxy sweep in `stop_headroom` is best-effort teardown. On
    // unix a `pkill` that matched nothing (exit 1) is already treated as
    // success; on Windows the sweep script now states its own verdict, so a
    // residual "powershell exited with status" error is a crash-class exit
    // with nothing actionable in it: the app is stopping, and the next
    // launch's `reclaim_orphan_proxy` reaps whatever survived -- the same
    // reasoning that made the session-teardown exit codes an info log
    // (RUST-7N; RUST-6F/6G were the old per-command-pattern warnings).
    // Deliberately NARROW: the script's enumeration-failure verdict ("could
    // not enumerate processes") and the pkill refusal guard must still
    // report. Keep the local log either way.
    if target.starts_with("headroom_desktop_lib::state")
        && msg.starts_with(
            "failed to clean detached headroom proxy processes: powershell exited with status",
        )
    {
        return true;
    }
    // Stopping without the lifecycle lock is the DESIGNED path, not a failure:
    // `stop_headroom` deliberately caps its wait so a quit racing a launch can
    // never hang the app with the window stuck on "Restarting...". The pkill
    // sweep reaps whatever the racing spawn leaves behind. It fires on ordinary
    // quit-during-launch across unrelated hosts (RUST-7Z), and there is no fix
    // to ship -- the alternative is the unbounded wait this replaced.
    if target.starts_with("headroom_desktop_lib::state")
        && msg.starts_with("stop_headroom: lifecycle lock still held")
    {
        return true;
    }
    false
}

/// Replace the user's home directory with `~` wherever it appears.
pub(crate) fn scrub_home(msg: &str) -> String {
    match dirs::home_dir() {
        Some(home) => {
            let home = home.to_string_lossy();
            let home = home.trim_end_matches('/');
            if home.is_empty() {
                msg.to_string()
            } else {
                msg.replace(home, "~")
            }
        }
        None => msg.to_string(),
    }
}

/// Last gate before an event leaves the process: drop what is environmental,
/// scrub what is personal.
///
/// `skip_sentry` and `scrub_home` are both reachable only from the `Log` impl,
/// so the ~40 direct `sentry::capture_message` sites bypassed both. RUST-6R is
/// what that costs: a disk-full warning that the `log::warn!` twin of the same
/// failure (RUST-7R) correctly suppresses, carrying an unscrubbed
/// `/Users/<name>/...` path. Only the target-agnostic rule is applied here -
/// the target-scoped ones need a `Record` that a direct capture does not have.
pub(crate) fn sanitize_event(
    event: sentry::protocol::Event<'static>,
) -> Option<sentry::protocol::Event<'static>> {
    let environmental = event.message.as_deref().is_some_and(is_unreportable)
        || event
            .logentry
            .as_ref()
            .is_some_and(|entry| is_unreportable(&entry.message))
        || event
            .exception
            .values
            .iter()
            .filter_map(|exception| exception.value.as_deref())
            .any(is_unreportable);
    if environmental {
        return None;
    }
    Some(scrub_event(event))
}

/// Replace the home directory with `~` in every free-text field, including the
/// ones `scope.set_extra`/`set_tag` fill in. The username is both a privacy leak
/// and a grouping key, so leaving it in splits one failure into one Sentry issue
/// per user.
pub(crate) fn scrub_event(
    mut event: sentry::protocol::Event<'static>,
) -> sentry::protocol::Event<'static> {
    if let Some(message) = event.message.as_deref() {
        event.message = Some(scrub_home(message));
    }
    if let Some(entry) = event.logentry.as_mut() {
        entry.message = scrub_home(&entry.message);
    }
    for exception in &mut event.exception.values {
        if let Some(value) = exception.value.as_deref() {
            exception.value = Some(scrub_home(value));
        }
    }
    // Tags carry paths too: `occupant_cmd` on the port-conflict events is a
    // process command line.
    for value in event.tags.values_mut() {
        *value = scrub_home(value);
    }
    for value in event.extra.values_mut() {
        scrub_json(value);
    }
    event
}

fn scrub_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = scrub_home(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(scrub_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(scrub_json),
        _ => {}
    }
}

impl Log for FileLogger {
    fn enabled(&self, _meta: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let msg = format!("{}", record.args());
        let demote = record.level() <= Level::Warn && skip_sentry(record.target(), &msg);
        let display_level = if demote && record.level() == Level::Error {
            Level::Warn
        } else {
            record.level()
        };

        // Rotation must not depend on level: an info-heavy session can blow
        // past MAX_LOG_BYTES without ever logging a warning. Warn+ checks
        // every record; info/debug check every 64th to keep the stat off the
        // hot path.
        if display_level <= Level::Warn
            || self
                .records_since_rotate_check
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                % 64
                == 0
        {
            self.rotate_if_needed();
        }
        self.write_record(record, display_level);

        if record.level() <= Level::Warn {
            if demote {
                return;
            }
            let level = match record.level() {
                Level::Error => sentry::Level::Error,
                _ => sentry::Level::Warning,
            };
            // Home paths embed the local username; replace with ~ so it
            // never leaves the machine.
            let scrubbed = scrub_home(&msg);
            let truncated: String = scrubbed.chars().take(SENTRY_MESSAGE_CHAR_CAP).collect();
            sentry::capture_message(&truncated, level);
        }
    }

    fn flush(&self) {
        if let Ok(mut g) = self.file.lock() {
            if let Some(f) = g.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// Initialize the global logger. Safe to call once at startup. Subsequent
/// calls return Err but do not panic.
pub fn init() -> Result<PathBuf, SetLoggerError> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let logger = FileLogger {
        file: Mutex::new(file),
        path: path.clone(),
        records_since_rotate_check: std::sync::atomic::AtomicU64::new(0),
    };
    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(log::LevelFilter::Debug);
    Ok(path)
}

#[cfg(target_os = "macos")]
pub(crate) fn log_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join("Library/Logs/Headroom/headroom-desktop.log"))
        .unwrap_or_else(|| PathBuf::from("/tmp/headroom-desktop.log"))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn log_path() -> PathBuf {
    dirs::data_local_dir()
        .map(|d| d.join("headroom/headroom-desktop.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("headroom-desktop.log"))
}

#[cfg(test)]
mod tests {
    use super::skip_sentry;

    #[test]
    fn skips_capture_skip_announcements_and_tiktoken_prefetch() {
        // RUST-AR: the skip announcement itself was bridged to Sentry.
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "skipping Sentry capture for bootstrap_failed (other): disk full (ENOSPC)"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "skipping Sentry capture for runtime_upgrade_failed (install): disk full (ENOSPC)"
        ));
        // RUST-AP: best-effort prefetch, lazy-load fallback exists.
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "tiktoken prefetch failed: tiktoken prefetch exited with exit code: 1"
        ));
        // A real bootstrap failure capture is not the skip announcement.
        assert!(!skip_sentry(
            "headroom_desktop_lib",
            "bootstrap_failed (install_runtime)"
        ));
    }

    #[test]
    fn skips_updater_transport_errors() {
        assert!(skip_sentry(
            "tauri_plugin_updater::updater",
            "failed to check for updates: error sending request for url (https://github.com/...)"
        ));
        assert!(skip_sentry(
            "tauri_plugin_updater",
            "dns error: failed to lookup address"
        ));
        assert!(skip_sentry(
            "tauri_plugin_updater::updater",
            "operation timed out"
        ));
    }

    #[test]
    fn skips_updater_endpoint_status_errors() {
        assert!(skip_sentry(
            "tauri_plugin_updater::updater",
            "update endpoint did not respond with a successful status code"
        ));
    }

    #[test]
    fn keeps_updater_non_transport_errors() {
        assert!(!skip_sentry(
            "tauri_plugin_updater::updater",
            "signature verification failed"
        ));
        assert!(!skip_sentry(
            "tauri_plugin_updater",
            "invalid release manifest"
        ));
    }

    #[test]
    fn skips_codex_retag_per_db_and_unreadable_warns_keeps_rename_canary() {
        // Per-DB skip (RUST-95/96: one machine's disk I/O errors): local only.
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "codex retag headroom->openai skipped for ~/.codex/goals_1.sqlite: disk I/O error"
        ));
        // Summary downgraded because candidates were unreadable: local only.
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "codex retag headroom->openai: no `threads` table found but 3 candidate(s) unreadable; skipping the rename signal"
        ));
        // The true schema-drift canary must still reach Sentry.
        assert!(!skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "codex retag headroom->openai: a state_*.sqlite is present but has no `threads` table under [\"~/.codex/sqlite\"]; the history menu may split. Codex likely renamed the table."
        ));
    }

    /// One detection, two issues: the fully-scoped capture at the emit site
    /// (RUST-A5) and this bridged log line (RUST-A4). The capture is the Sentry
    /// path; the warn is local only.
    #[test]
    fn skips_the_zero_savings_canary_log_twin() {
        assert!(skip_sentry(
            "headroom_desktop_lib::savings_canary",
            "zero-savings canary: 32/32 requests over 10000 tokens saved nothing (models: anthropic/glm-5.3; strata: other|new_user_ask|xl|tools)"
        ));
        // The emit-site capture's own wording must still reach Sentry -- it is
        // the half that carries the fingerprint and the extras.
        assert!(!skip_sentry(
            "headroom_desktop_lib::savings_canary",
            "zero_savings_canary: 32/32 large requests compressed to nothing (models: anthropic/glm-5.3; strata: other|new_user_ask|xl|tools)"
        ));
    }

    /// ENFILE is a machine-wide resource we cannot free, and it fails every
    /// file touch identically (RUST-A3 writing, RUST-5T reading). EMFILE is the
    /// per-process limit and would mean we leak descriptors -- keep reporting it.
    #[test]
    fn skips_system_wide_fd_exhaustion_but_not_our_own() {
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "failed to persist usage-counters.json: writing ~/Library/Application Support/Headroom/config/usage-counters.json.tmp.22503.30753: Too many open files in system (os error 23)"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "load_setup_state: could not read ~/Library/Application Support/Headroom/config/client-setup.json twice (reading: Too many open files in system (os error 23))"
        ));
        assert!(!skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "failed to persist usage-counters.json: writing /tmp/x: Too many open files (os error 24)"
        ));
    }

    #[test]
    fn skips_foreign_port_bind_retry_warns() {
        // Named foreign holder -- captured once at the emit site.
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "[proxy_intercept] port 6767 is held by Affinity (pid 54915); retrying in 15s (Address already in use (os error 48))"
        ));
        // Stuck past the drain window -- also captured once at the emit site.
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "[proxy_intercept] port 6767 still in use with nothing listening after 420s; retrying in 15s (Only one usage of each socket address (protocol/network address/port) is normally permitted. (os error 10048))"
        ));
        // Other bind/loop errors from proxy_intercept stay in Sentry.
        assert!(!skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "[proxy_intercept] error: some other failure; retrying in 15s"
        ));
    }

    #[test]
    fn skips_machine_id_digest_persist_failures() {
        assert!(skip_sentry(
            "headroom_desktop_lib::device",
            "Could not persist machine id digest (non-fatal, using computed value): duplicate item"
        ));
        // A different device.rs warning is not blanket-skipped.
        assert!(!skip_sentry(
            "headroom_desktop_lib::device",
            "hardware UUID unavailable"
        ));
    }

    #[test]
    fn skips_codex_retag_malformed_db() {
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "codex retag openai->headroom skipped for ~/.codex/logs_2.sqlite: database disk image is malformed"
        ));
        // Other retag skip causes (locked DB, schema drift) stay in Sentry.
        assert!(!skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "codex retag openai->headroom skipped for ~/.codex/state_5.sqlite: database is locked"
        ));
    }

    #[test]
    fn keeps_other_targets() {
        assert!(!skip_sentry(
            "headroom_desktop_lib::pricing",
            "error sending request: timeout"
        ));
        assert!(!skip_sentry("reqwest", "error sending request"));
    }

    #[test]
    fn skips_proxy_intercept_bypass_transport_errors() {
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "proxy_intercept bypass forward failed: error sending request for url (https://api.anthropic.com/v1/messages?beta=true)"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "proxy_intercept bypass forward failed: dns error: failed to lookup address"
        ));
    }

    #[test]
    fn keeps_proxy_intercept_non_transport_errors() {
        assert!(!skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "proxy_intercept bypass forward failed: invalid header value"
        ));
        assert!(!skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "some other proxy_intercept warning"
        ));
    }

    #[test]
    fn skips_kompress_prefetch_best_effort_warnings() {
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "kompress prefetch failed: some error"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "kompress prefetch: restart after download failed: boom"
        ));
    }

    #[test]
    fn skips_uninstall_cleanup_removal_warnings() {
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "cleanup: removing /Users/x/Library/Application Support/Headroom failed: Directory not empty (os error 66)"
        ));
        // RUST-6X: the parse failed, so we left the file alone -- the safe
        // branch, reported as if it were a defect.
        assert!(skip_sentry(
            "headroom_desktop_lib::client_adapters",
            "cleanup: stripping hook from ~/.claude/settings.local.json failed: parsing \
             ~/.claude/settings.local.json failed (JSON/JSON5); refusing to overwrite \
             potentially valid user settings"
        ));
        // RUST-6T: same teardown, different module -- the venv is still open on
        // Windows when uninstall_and_quit deletes it.
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "uninstall: removing serena failed: removing ~\\AppData\\Local\\Headroom\\headroom\\serena-venv: Access is denied. (os error 5)"
        ));
    }

    #[test]
    fn skips_wry_evaluate_script_failures_but_not_other_wry_errors() {
        // RUST-6H: 988 events in 24h from one Windows host, every one of them
        // this bare wry Display bridged from tauri-runtime-wry's `log::error!("{e}")`.
        assert!(skip_sentry(
            "tauri_runtime_wry",
            "WebView2 error: WindowsError(Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" })"
        ));
        // Other bare WebView2 errors may be creation/startup failures.
        assert!(!skip_sentry(
            "tauri_runtime_wry",
            "WebView2 error: WindowsError(Error { code: HRESULT(0x80004005), message: \"Unspecified error.\" })"
        ));
        assert!(!skip_sentry(
            "tauri_runtime_wry",
            "failed to navigate to url http://localhost:1420: some error"
        ));
        // And the string alone is not a licence to drop it from our own modules.
        assert!(!skip_sentry(
            "headroom_desktop_lib::state",
            "WebView2 error: something we emitted ourselves"
        ));
    }

    #[test]
    fn skips_tray_missing_runtime_route_to_setup() {
        // RUST-8W: the emit site already decided this is a recovery, not a
        // crash (RUST-1M); the log bridge sent it to Sentry regardless, with
        // the whole argv inlined so it fragmented per command line too.
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "ensure_runtime_ready_for_tray: managed runtime missing; routing to setup: unable to keep headroom running in background (prior attempts: ~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\headroom.exe proxy --port 6768 exited with status exit code: 1)"
        ));
        // A real tray-path start failure still reports.
        assert!(!skip_sentry(
            "headroom_desktop_lib",
            "ensure_runtime_ready_for_tray failed: unable to keep headroom running in background"
        ));
    }

    #[test]
    fn skips_detached_proxy_sweep_failures() {
        // RUST-6F/6G: one issue per command pattern, from a best-effort sweep
        // during teardown that unix already treats as success when it matches
        // nothing.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "failed to clean detached headroom proxy processes: powershell exited with status Some(1) for exe '~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\python.exe' args '-m headroom.proxy.server'"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "failed to clean detached headroom proxy processes: powershell exited with status Some(1) for exe '~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\headroom.exe' args 'proxy --port'"
        ));
        // The refusal guard is a real bug (an unresolved runtime path would
        // turn the sweep into a loose substring kill), so it must still
        // report -- as the caller actually bridges it, prefix and all.
        assert!(!skip_sentry(
            "headroom_desktop_lib::state",
            "failed to clean detached headroom proxy processes: refusing to pkill with an unresolved executable path \"\""
        ));
        // A failed Win32_Process enumeration is the one sweep outcome worth a
        // report; the script's own verdict separates it from a clean run.
        assert!(!skip_sentry(
            "headroom_desktop_lib::state",
            "failed to clean detached headroom proxy processes: powershell could not enumerate processes (Win32_Process query failed) for exe '~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\headroom.exe' args 'proxy --port'"
        ));
    }

    #[test]
    fn skips_stop_headroom_lifecycle_lock_fallback() {
        // RUST-7Z: the capped wait IS the fix for the unbounded one; quitting
        // during a launch takes this branch by design, on any platform.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "stop_headroom: lifecycle lock still held after 2s; stopping without it"
        ));
        assert!(!skip_sentry(
            "headroom_desktop_lib::state",
            "stop_headroom: something else entirely"
        ));
    }

    #[test]
    fn drops_the_windows_session_end_panic() {
        use super::is_unreportable;
        // RUST-84/8D: tao's WM_ENDSESSION panic. Arrives as a panic through
        // sentry's own integration, so skip_sentry never sees it -- sanitize_event
        // is the only gate, and it reads this predicate.
        assert!(is_unreportable("cannot move state from Destroyed"));
        assert!(!is_unreportable("cannot move state from Running"));
        // The disk-full rule this predicate absorbed still holds.
        assert!(is_unreportable("write failed: No space left on device"));
        assert!(!is_unreportable("write failed: permission denied"));
    }

    #[test]
    fn skips_codex_upstream_error_raw_body_warning() {
        // RUST-5Q: this line carries the raw upstream body (user request fields)
        // and no fingerprint, so Sentry grab-bagged 400/403/503/507 together.
        // The status-fingerprinted capture at the emit site is the Sentry path.
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "codex upstream error 400 on /v1/responses: {\"error\":{\"message\":\"Unsupported value\"}}"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "codex upstream error 503 on /v1/responses: upstream connect error"
        ));
        // The Claude-side sibling logs the same shape under its client key.
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "claude-code upstream error 400 on /v1/messages: {\"error\":{\"message\":\"max_tokens\"}}"
        ));
        assert!(!skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "some other proxy_intercept warning"
        ));
    }

    #[test]
    fn skips_bridged_pip_final_failure_warning() {
        // RUST-6M/6N/6P: the stderr tail is in the message, so message-based
        // grouping opened a new issue per tail. The per-category fingerprinted
        // capture at the emit site is the Sentry path.
        assert!(skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "pip install attempt 3/3 failed (final): exit=1; stderr tail: Check the permissions."
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "pip install attempt 3/3 failed (final): exit=1; stderr tail: No module named pip"
        ));
        // The retry line is log::info (never bridged) and any other pip warn
        // still reports.
        assert!(!skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "pip install produced no usable venv"
        ));
    }

    #[test]
    fn partial_plugin_install_warn_is_local_only() {
        // RUST-6K: the bridged warn grouped five causes under one fingerprint.
        // It now reaches Sentry only via the fingerprinted capture at the emit
        // site, so the warn itself must be demoted to local-only.
        assert!(skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "ponytail installed for some hosts but not all: Codex: command failed (exit 1): \
             codex plugin add ponytail@ponytail"
        ));
        // Unrelated plugin warns still report.
        assert!(!skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "caveman smoke test failed after upgrade: stale receipt removed"
        ));
    }

    #[test]
    fn skips_adhoc_codesign_best_effort_warning() {
        assert!(skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "ad-hoc codesign exited Some(1) for 633 files: /path/_http_writer.so: replacing existing signature"
        ));
        // A genuine signing regression surfaces via the smoke-test gate, not
        // this best-effort line; an unrelated tool_manager warn still reports.
        assert!(!skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "some other tool_manager warning"
        ));
    }

    #[test]
    fn skips_kompress_prefetch_download_error_warn() {
        // Sentry now gets this via the explicit category-fingerprinted
        // capture_message at the emit site (RUST-3C grab-bag split); the
        // bridged warn would double-report.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "kompress prefetch download error: [network] Max retries exceeded"
        ));
    }

    #[test]
    fn skips_tauri_utils_appimage_env_warning() {
        // RUST-CN: APPDIR leaked from the launching shell into a .deb install.
        assert!(skip_sentry(
            "tauri_utils",
            "`APPDIR` or `APPIMAGE` environment variable found but this application was not \
             detected as an AppImage; this might be a security issue."
        ));
        assert!(!skip_sentry(
            "tauri_utils",
            "some other tauri_utils warning"
        ));
    }

    #[test]
    fn skips_stats_fetch_failed_warn() {
        // RUST-6V: timeout and HTTP 404 shared one message shape, so Sentry
        // grouped two different bugs together. The fingerprinted capture at
        // the emit site is the Sentry path now.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "headroom /stats fetch failed (HTTP 404 Not Found); dashboard loses the layers"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "headroom /stats fetch failed (timed out after 15s); dashboard loses the layers"
        ));
        assert!(!skip_sentry(
            "headroom_desktop_lib::state",
            "some other state warning"
        ));
    }

    #[test]
    fn skips_wheel_download_fallback_warn() {
        // RUST-22: the full PyPI URL was the message, so every wheel bump and
        // every platform tag opened a new issue for one condition.
        assert!(skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "headroom wheel download failed (will fall back to pip index): downloading https://files.pythonhosted.org/packages/47/21/headroom_ai-0.37.0-cp310-abi3-macosx_11_0_arm64.whl"
        ));
        // The checksum-mismatch path is a hard failure, not this warn, and
        // must keep its own reporting.
        assert!(!skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "Headroom wheel failed checksum verification; refusing unverified fallback"
        ));
    }

    #[test]
    fn skips_bypass_upgrade_forward_transport_errors() {
        // The websocket-upgrade forwarder variant (RUST-2R) gets the same
        // transient-transport treatment as the plain bypass forwarder.
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "proxy_intercept bypass upgrade forward failed: error sending request for url (https://api.openai.com/v1/responses)"
        ));
        // Non-transport failures on the same path still report.
        assert!(!skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "proxy_intercept bypass upgrade forward failed: builder error"
        ));
    }

    #[test]
    fn skips_boot_validation_failed_rollback_warn() {
        // capture_runtime_upgrade_failure at the same site carries the
        // fully-tagged event (RUST-4A); the bridged warn duplicated it as
        // RUST-2N.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "run_upgrade_with_ui: boot validation failed (timed_out); rolling back to Some(\"0.30.0\")"
        ));
    }

    #[test]
    fn skips_upgrade_spawn_failure_and_not_started_warns() {
        // RUST-CZ: the spawn error, message-grouped on the venv path.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "run_upgrade_with_ui: new proxy failed to spawn: unable to keep headroom running in \
             background (prior attempts: headroom.exe: exited with status exit code: 1 before \
             opening port 6768): exited with status exit code: 1 before opening port 6768 \
             (~\\AppData\\Local\\Headroom\\headroom\\runtime\\venv\\Scripts\\python.exe -m headroom.proxy.server)"
        ));
        // RUST-CX: the short-circuit that follows it.
        assert!(skip_sentry(
            "headroom_desktop_lib::state",
            "run_upgrade_with_ui: skipping boot validation: no tracked child and no reachable proxy"
        ));
        // Same prefix from another module is not ours to drop.
        assert!(!skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "run_upgrade_with_ui: new proxy failed to spawn: x"
        ));
    }

    #[test]
    fn keeps_other_state_warnings() {
        assert!(!skip_sentry(
            "headroom_desktop_lib::state",
            "some other state warning"
        ));
    }

    #[test]
    fn skips_successful_stale_tool_reference_sanitisation() {
        assert!(skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "[proxy_intercept] dropped 3 stale tool_search reference(s) [\"TaskCreate\"] from a \
             direct-forwarded request — absent from the tools array, upstream would 400 the \
             session permanently"
        ));
        // A genuine failure in the same module still reports.
        assert!(!skip_sentry(
            "headroom_desktop_lib::proxy_intercept",
            "[proxy_intercept] dropped connection while forwarding"
        ));
    }

    #[test]
    fn skips_backend_port_fallback_warn_but_not_siblings() {
        // The emit-site capture_message is the Sentry path for this event.
        assert!(skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "[backend_port] 6768 held by unknown process; falling back to 6770"
        ));
        // Other tool_manager warnings still report.
        assert!(!skip_sentry(
            "headroom_desktop_lib::tool_manager",
            "managed headroom exited unexpectedly"
        ));
    }

    #[test]
    fn skips_tray_tooltip_failures_in_any_locale() {
        // English and Spanish renderings of the same E_FAIL both drop.
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "tray: set_tooltip failed: tray icon error: Unspecified error (os error -2147467259)"
        ));
        assert!(skip_sentry(
            "headroom_desktop_lib",
            "tray: set_tooltip failed: tray icon error: Error no especificado (os error -2147467259)"
        ));
        // Other tray failures are not cosmetic and still report.
        assert!(!skip_sentry(
            "headroom_desktop_lib",
            "tray: set_icon failed: tray icon error: Unspecified error"
        ));
    }

    #[test]
    fn skips_keyring_fallback_but_not_other_keychain_failures() {
        assert!(skip_sentry(
            "headroom_desktop_lib::keychain",
            "OS credential store unusable (Couldn't access platform secure storage: \
             Secret Service: no result found); storing Headroom secrets in a 0600 file \
             under the app data dir instead"
        ));
        // A real keychain failure that is NOT the designed fallback still reports.
        assert!(!skip_sentry(
            "headroom_desktop_lib::keychain",
            "failed to write Headroom secret to the file store"
        ));
    }

    #[test]
    fn skips_disk_full_from_any_target() {
        assert!(skip_sentry(
            "headroom_desktop_lib::pricing",
            "Could not persist reconciled grace state: Failed to write pricing state \
             ~/config/headroom-pricing-state.json: writing \
             ~/config/headroom-pricing-state.json.tmp.47276.4810: \
             No space left on device (os error 28)"
        ));
        // Any other write failure from the same path still reports.
        assert!(!skip_sentry(
            "headroom_desktop_lib::pricing",
            "Could not persist reconciled grace state: Failed to write pricing state \
             ~/config/headroom-pricing-state.json: writing \
             ~/config/headroom-pricing-state.json.tmp.47276.4810: \
             Permission denied (os error 13)"
        ));
    }

    #[test]
    fn scrub_home_replaces_home_dir_with_tilde() {
        // scrub_home reads $HOME again internally, so a TestHome elsewhere
        // swapping it between our read and theirs makes the scrub a no-op.
        let _home_lock = crate::test_env_lock::lock_home();

        let home = dirs::home_dir().unwrap();
        let msg = format!(
            "cleanup: removing {}/Library/Application Support/x",
            home.display()
        );
        let scrubbed = super::scrub_home(&msg);
        assert_eq!(
            scrubbed,
            "cleanup: removing ~/Library/Application Support/x"
        );
        assert_eq!(super::scrub_home("no paths here"), "no paths here");
    }

    #[test]
    fn scrub_event_covers_message_tags_and_extras() {
        // scrub_home reads $HOME again internally, so a TestHome elsewhere
        // swapping it between our read and theirs makes the scrub a no-op.
        let _home_lock = crate::test_env_lock::lock_home();

        let home = dirs::home_dir().unwrap();
        let home = home.display().to_string();
        let mut event = sentry::protocol::Event::new();
        event.message = Some(format!("Failed to write {home}/Library/x.json"));
        event
            .tags
            .insert("occupant_cmd".into(), format!("{home}/bin/thing"));
        event.extra.insert(
            "error_chain".into(),
            serde_json::json!({"stderr": [format!("no such file: {home}/y")]}),
        );

        let scrubbed = super::scrub_event(event);

        assert_eq!(
            scrubbed.message.as_deref(),
            Some("Failed to write ~/Library/x.json")
        );
        assert_eq!(
            scrubbed.tags.get("occupant_cmd").map(String::as_str),
            Some("~/bin/thing")
        );
        let chain = serde_json::to_string(&scrubbed.extra["error_chain"]).unwrap();
        assert!(!chain.contains(&home), "home leaked in extras: {chain}");
        assert!(chain.contains("no such file: ~/y"), "{chain}");
    }

    #[test]
    fn sanitize_event_drops_disk_full_from_direct_captures() {
        // The shape of RUST-6R: a direct capture_message that never passed
        // through skip_sentry.
        let mut full = sentry::protocol::Event::new();
        full.message = Some(
            "Could not persist reconciled grace state: Failed to write pricing state \
             ~/config/headroom-pricing-state.json: No space left on device (os error 28)"
                .into(),
        );
        assert!(super::sanitize_event(full).is_none());

        let mut other = sentry::protocol::Event::new();
        other.message = Some("Could not persist reconciled grace state: Permission denied".into());
        assert!(super::sanitize_event(other).is_some());
    }
}
