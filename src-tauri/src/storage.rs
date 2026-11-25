use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn app_data_dir() -> PathBuf {
    // Explicit override, used for test hermeticity: nextest runs each test in
    // its own process, so an in-process env lock cannot stop parallel tests
    // from sharing (and corrupting) the real profile's Headroom dir — on
    // macOS/Windows dirs::data_local_dir() ignores every env var TestHome
    // sets. Production never sets this. Relative paths are ignored so a stray
    // value can't scatter state under an arbitrary cwd.
    if let Some(dir) = std::env::var_os("HEADROOM_DATA_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir;
        }
    }
    let base = dirs::data_local_dir()
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("Headroom")
}

pub fn ensure_data_dirs(base_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("creating app data dir {}", base_dir.display()))?;
    std::fs::create_dir_all(base_dir.join("telemetry"))
        .with_context(|| format!("creating telemetry dir under {}", base_dir.display()))?;
    std::fs::create_dir_all(base_dir.join("config"))
        .with_context(|| format!("creating config dir under {}", base_dir.display()))?;
    Ok(())
}

pub fn config_file(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("config").join(name)
}

/// The user-facing calendar day ("YYYY-MM-DD", local timezone) for an
/// instant. Canonical: every "today"/day-bucket decision that the user can
/// see goes through this, regardless of the instant's source timezone —
/// mixed UTC/local day keys gave US users mid-afternoon daily resets. UTC-
/// bucketed data from the backend is the one exception (keyed by its UTC
/// date, labeled as such). See the Persistence Rules in CLAUDE.md.
pub fn user_day_key<Tz: chrono::TimeZone>(instant: chrono::DateTime<Tz>) -> String {
    instant
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

/// Local `NaiveDate` counterpart of [`user_day_key`].
pub fn user_day<Tz: chrono::TimeZone>(instant: chrono::DateTime<Tz>) -> chrono::NaiveDate {
    instant.with_timezone(&chrono::Local).date_naive()
}

pub fn memory_db_path(base_dir: &Path) -> PathBuf {
    base_dir.join("memory.db")
}

pub fn telemetry_file(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("telemetry").join(name)
}

/// The user-state files that smoke-test check 14 guards, snapshotted raw on
/// the first launch of a new app version. A schema change in the new build
/// can fail a parse and hand the user a default, and a wiped state file
/// looks exactly like a healthy fresh one - the pre-update copy is the only
/// before/after evidence. Raw byte copies, deliberately format-agnostic.
const PRE_UPDATE_SNAPSHOT_FILES: [&str; 3] = [
    "headroom-pricing-state.json",
    "client-setup.json",
    "activity-facts.json",
];

/// Best-effort; must run BEFORE anything parses or rewrites state files.
/// Keyed on `config/last-run-version`: on any version change the previous
/// build's state files are copied raw into `config/pre-update/` (with a
/// `meta.json` naming the from/to versions), and the stamp is rewritten
/// LAST, so a crash mid-snapshot re-runs it on the next launch while the
/// state files are still untouched.
pub fn snapshot_state_on_version_change(base_dir: &Path, current_version: &str) {
    let stamp_path = config_file(base_dir, "last-run-version");
    let last_version = std::fs::read_to_string(&stamp_path)
        .map(|v| v.trim().to_string())
        .ok()
        .filter(|v| !v.is_empty());
    if last_version.as_deref() == Some(current_version) {
        return;
    }
    let snapshot_dir = base_dir.join("config").join("pre-update");
    if let Err(err) = std::fs::create_dir_all(&snapshot_dir) {
        // On a folder that exists but cannot be stat'ed (an ACL or privacy
        // block) `create_dir_all` returns mkdir's AlreadyExists, which hid
        // the block from env_fault and filed it twice (RUST-RP beside RN).
        // Any other error is mkdir's own: a stat there only says NotFound.
        let err = match err.kind() {
            std::io::ErrorKind::AlreadyExists => {
                std::fs::metadata(&snapshot_dir).err().unwrap_or(err)
            }
            _ => err,
        };
        let note = crate::env_fault::failed(&snapshot_dir, crate::env_fault::Op::Write, &err);
        log::warn!(
            "pre-update snapshot: cannot create {}: {err}{note}",
            snapshot_dir.display()
        );
        return;
    }
    for name in PRE_UPDATE_SNAPSHOT_FILES {
        let source = config_file(base_dir, name);
        if !source.exists() {
            continue;
        }
        // Retried: a scanner holding the source for a moment otherwise cost
        // this version its snapshot for good (the version stamp is still
        // written below, so the next launch never retakes it).
        if let Err(err) = crate::client_adapters::retry_transient_denied(|| {
            std::fs::copy(&source, snapshot_dir.join(name))
        }) {
            log::warn!("pre-update snapshot: copying {name} failed: {err}");
        }
    }
    let meta = serde_json::json!({
        "from_version": last_version.as_deref().unwrap_or("unknown"),
        "to_version": current_version,
        "taken_at": chrono::Utc::now().to_rfc3339(),
    });
    if let Err(err) = crate::client_adapters::atomic_write(
        &snapshot_dir.join("meta.json"),
        meta.to_string().as_bytes(),
    ) {
        log::warn!("pre-update snapshot: writing meta.json failed: {err}");
    }
    if let Err(err) = crate::client_adapters::atomic_write(&stamp_path, current_version.as_bytes())
    {
        log::warn!("pre-update snapshot: writing version stamp failed: {err}");
    } else if let Some(from) = last_version {
        log::info!(
            "pre-update snapshot: state from {from} saved before first {current_version} launch"
        );
    }
}

/// Written just before an update-restart tears the app down, cleared by the
/// launch that follows.
pub fn restart_pending_path(base_dir: &Path) -> PathBuf {
    base_dir.join("restart-pending")
}

/// Touched by the detached helper immediately before it calls `open`.
pub fn restart_attempted_path(base_dir: &Path) -> PathBuf {
    base_dir.join("restart-attempted")
}

/// Records that we are quitting expecting to come straight back.
#[cfg(target_os = "macos")]
pub fn mark_restart_pending(base_dir: &Path, from_version: &str) {
    // Clear the other half first: a leftover `attempted` from an earlier
    // restart would read as "the helper tried" on the next launch.
    let _ = std::fs::remove_file(restart_attempted_path(base_dir));
    let note = format!("{from_version} at {}", chrono::Utc::now().to_rfc3339());
    if let Err(err) =
        crate::client_adapters::atomic_write(&restart_pending_path(base_dir), note.as_bytes())
    {
        log::warn!("restart: writing the pending marker failed: {err}");
    }
}

/// Reports an update-restart that never brought the app back, then clears both
/// markers. Call once at startup.
///
/// The one thing a restart cannot check for itself: when the detached helper
/// dies, there is no process left to notice, so the app is simply gone until
/// the user opens it by hand (twice on 2026-09-21, found both times by their
/// sessions failing). The two markers separate "the helper never reached its
/// `open`" from "it tried", which a missing log line cannot, and the warn
/// carries it to Sentry so this is not per-machine archaeology next time.
pub fn report_unfinished_restart(base_dir: &Path) {
    use std::io::ErrorKind;

    let pending = restart_pending_path(base_dir);
    let note = match read_restart_marker(&pending) {
        Ok(note) => note,
        Err(err) => {
            if err.kind() != ErrorKind::NotFound {
                log::info!("restart: reading the pending marker failed: {err}");
            }
            return;
        }
    };
    let attempted = restart_attempted_path(base_dir);
    // The helper records `open`'s exit code in the marker. Empty is a helper
    // from a build that only touched it (the OLD build runs the restart).
    match read_restart_marker(&attempted) {
        Ok(rc) if !matches!(rc.trim(), "" | "0") => log::warn!(
            "restart: the previous update-restart's `open` failed (rc={}); the app \
             only came back when relaunched by hand (requested by {})",
            rc.trim(),
            note.trim()
        ),
        Ok(_) => log::info!("restart: relaunch completed (requested by {})", note.trim()),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let text = crate::logging::recent_desktop_log(&crate::logging::log_path());
            log::warn!(
                "restart: the previous update-restart never relaunched the app; \
                 its detached helper {} (requested by {})",
                restart_helper_trace(&text, &note),
                note.trim()
            )
        }
        Err(err) => {
            // An unreadable marker may still hold the result. Retry at the
            // next launch instead of losing the evidence for this restart.
            log::info!("restart: reading the attempted marker failed: {err}");
            return;
        }
    }
    let _ = std::fs::remove_file(&pending);
    let _ = std::fs::remove_file(&attempted);
}

fn read_restart_marker(path: &Path) -> std::io::Result<String> {
    String::from_utf8(crate::state::read_state_file(path)?)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.utf8_error()))
}

/// How far the last restart's helper got, read from the desktop log it writes
/// to. RUST-XM's one event could not say whether the helper never ran or died
/// mid-wait, and those point at different causes.
fn restart_helper_trace(log_text: &str, pending_note: &str) -> &'static str {
    const NO_TRACE: &str = "has no matching restart trace in the desktop log";
    let Some(requested) = pending_note
        .trim()
        .split_once(" at ")
        .and_then(|(_, stamp)| chrono::DateTime::parse_from_rfc3339(stamp).ok())
    else {
        return NO_TRACE;
    };

    // The pending marker uses UTC. Both log writers use local wall time, and
    // the shell prints only whole seconds. A short window keeps an older
    // restart or a later, unrelated helper out of this report.
    let requested = requested.with_timezone(&chrono::Local).naive_local();
    let latest = requested + chrono::Duration::seconds(30);
    let mut announced = None;
    for line in log_text.lines() {
        const ANNOUNCE: &str =
            " INFO  headroom_desktop_lib: restart_app: relaunching via `open -n`";
        if let Some((stamp, _)) = line.split_once(ANNOUNCE) {
            announced = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S%.3f")
                .ok()
                .filter(|at| {
                    // Compare at the Rust logger's millisecond precision.
                    at.and_utc().timestamp_millis() >= requested.and_utc().timestamp_millis()
                        && *at <= latest
                });
        } else if let Some((spawned, (stamp, _))) =
            announced.zip(line.split_once(" helper: detached, waiting for pid "))
        {
            if chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S").is_ok_and(|at| {
                at.and_utc().timestamp() >= spawned.and_utc().timestamp() && at <= latest
            }) {
                return "logged its start before reaching `open`";
            }
        }
    }
    if announced.is_some() {
        "has no start record in the desktop log"
    } else {
        NO_TRACE
    }
}

/// Written by the idle update applier just before it restarts or installs.
/// Its relaunch is not a login launch, so it would raise the window (and take
/// focus) on a machine whose owner is away or typing in another app.
pub fn quiet_relaunch_path(base_dir: &Path) -> PathBuf {
    base_dir.join("quiet-relaunch")
}

/// A marker older than this belongs to a relaunch that never happened; the
/// next launch is the user's own and shows its window.
const QUIET_RELAUNCH_WINDOW_SECS: i64 = 15 * 60;

pub fn mark_quiet_relaunch(base_dir: &Path) {
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(err) =
        crate::client_adapters::atomic_write(&quiet_relaunch_path(base_dir), now.as_bytes())
    {
        log::warn!("update: writing the quiet-relaunch marker failed: {err}");
    }
}

/// True when this launch is the idle applier's relaunch. Consumes the marker,
/// so only that one launch stays in the tray.
pub fn take_quiet_relaunch(base_dir: &Path, now: chrono::DateTime<chrono::Utc>) -> bool {
    let path = quiet_relaunch_path(base_dir);
    let Ok(note) = std::fs::read_to_string(&path) else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    chrono::DateTime::parse_from_rfc3339(note.trim()).is_ok_and(|at| {
        (0..=QUIET_RELAUNCH_WINDOW_SECS).contains(&now.signed_duration_since(at).num_seconds())
    })
}

#[cfg(test)]
mod restart_marker_tests {
    use super::*;

    #[test]
    fn only_the_idle_relaunch_stays_in_the_tray() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        let now = chrono::Utc::now();

        assert!(
            !take_quiet_relaunch(base, now),
            "no marker: a normal launch"
        );

        mark_quiet_relaunch(base);
        assert!(take_quiet_relaunch(
            base,
            now + chrono::Duration::seconds(20)
        ));
        assert!(
            !take_quiet_relaunch(base, now + chrono::Duration::seconds(21)),
            "consumed: the user's next launch shows its window"
        );

        // A relaunch that never came: the user's own launch later is not quiet.
        mark_quiet_relaunch(base);
        assert!(!take_quiet_relaunch(base, now + chrono::Duration::hours(1)));
        assert!(!quiet_relaunch_path(base).exists());
    }

    /// A relaunch that never happened has to be distinguishable from one that
    /// worked, and neither may leave markers behind to confuse the next launch.
    #[test]
    fn unfinished_restart_is_detected_and_both_markers_are_cleared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();

        // Nothing pending: nothing to report, nothing to clean.
        report_unfinished_restart(base);

        // Helper died before `open`: pending stands alone.
        std::fs::write(restart_pending_path(base), "0.9.18 at now").unwrap();
        assert!(!restart_attempted_path(base).exists());
        report_unfinished_restart(base);
        assert!(
            !restart_pending_path(base).exists(),
            "a reported restart must not be reported again on every later launch"
        );

        // Helper tried: both markers, both cleared, whatever `open` returned.
        for rc in ["", "0\n", "1\n"] {
            std::fs::write(restart_pending_path(base), "0.9.18 at now").unwrap();
            std::fs::write(restart_attempted_path(base), rc).unwrap();
            report_unfinished_restart(base);
        }
        assert!(!restart_pending_path(base).exists());
        assert!(
            !restart_attempted_path(base).exists(),
            "a stale attempted marker reads as success on the next failure"
        );
    }

    #[test]
    fn unreadable_attempt_marker_keeps_the_restart_pending_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let pending = restart_pending_path(dir.path());
        let attempted = restart_attempted_path(dir.path());
        let note = "0.9.64 at 2026-11-23T11:52:25+00:00";
        std::fs::write(&pending, note).unwrap();
        std::fs::create_dir(&attempted).unwrap();

        report_unfinished_restart(dir.path());
        assert_eq!(std::fs::read_to_string(&pending).unwrap(), note);

        std::fs::remove_dir(&attempted).unwrap();
        std::fs::write(&attempted, "0\n").unwrap();
        report_unfinished_restart(dir.path());
        assert!(!pending.exists());
        assert!(!attempted.exists());
    }

    #[test]
    fn helper_trace_matches_the_pending_restart_time() {
        use chrono::TimeZone;

        let local = chrono::Local
            .with_ymd_and_hms(2026, 11, 23, 11, 52, 25)
            .unwrap();
        let utc = local.to_utc() + chrono::Duration::microseconds(750);
        let note = format!("0.9.64 at {}", utc.to_rfc3339());
        let spawned = "2026-11-23 11:52:25.000 INFO  headroom_desktop_lib: restart_app: relaunching via `open -n` against bundle\n";
        let started = "2026-11-23 11:52:25 helper: detached, waiting for pid 9\n";
        let trace = |log: &str| restart_helper_trace(log, &note);
        let missing = "has no matching restart trace in the desktop log";
        let no_start = "has no start record in the desktop log";
        // The app can die after writing pending but before announcing the
        // restart, leaving the previous restart as the newest one in the log.
        let older = format!("{spawned}{started}").replace("11:52:25", "11:51:25");
        assert_eq!(trace(&older), trace(""));
        assert_eq!(trace(""), missing);
        assert_eq!(trace(spawned), no_start);
        assert_eq!(
            trace(&format!("{spawned}{started}")),
            "logged its start before reaching `open`"
        );
        assert_eq!(trace(&format!("{older}{spawned}")), no_start);
        // A later helper (e.g. the bundle trasher) sheds no light on this restart.
        let later = started.replace("11:52:25", "11:53:25");
        assert_eq!(trace(&format!("{spawned}{later}")), no_start);
        assert_eq!(
            restart_helper_trace(&format!("{spawned}{started}"), "0.9.18 at now"),
            missing
        );
    }
}

#[cfg(test)]
mod pre_update_snapshot_tests {
    use super::*;

    fn write_config(base: &Path, name: &str, contents: &str) {
        let path = config_file(base, name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
    }

    fn snapshot_file(base: &Path, name: &str) -> PathBuf {
        base.join("config").join("pre-update").join(name)
    }

    #[test]
    fn version_change_snapshots_raw_bytes_and_records_versions() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // Deliberately not-parseable content: the copy must be raw bytes,
        // indifferent to what the new build can read.
        write_config(base, "headroom-pricing-state.json", "{\"first_seen_at\":1}");
        write_config(base, "client-setup.json", "not json at all");
        // activity-facts.json absent: missing files are skipped, not errors.

        snapshot_state_on_version_change(base, "0.9.2");
        snapshot_state_on_version_change(base, "0.9.3");

        assert_eq!(
            std::fs::read_to_string(snapshot_file(base, "headroom-pricing-state.json")).unwrap(),
            "{\"first_seen_at\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(snapshot_file(base, "client-setup.json")).unwrap(),
            "not json at all"
        );
        assert!(!snapshot_file(base, "activity-facts.json").exists());
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(snapshot_file(base, "meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["from_version"], "0.9.2");
        assert_eq!(meta["to_version"], "0.9.3");
        assert_eq!(
            std::fs::read_to_string(config_file(base, "last-run-version")).unwrap(),
            "0.9.3"
        );
    }

    /// `user_day` buckets by the user's local midnight, not UTC's. The
    /// regression this guards is a call site reverting to
    /// `Utc::now().date_naive()`, which shifts every "today" counter by up to a
    /// full day for anyone off UTC (build_claude_code_project's sessions_today
    /// did exactly that).
    ///
    /// ponytail: inert on a UTC host (the two midnights coincide there), so CI
    /// does not catch the revert -- only a developer or user off UTC does.
    /// Making it deterministic everywhere needs an injectable clock timezone,
    /// which is more machinery than the one-line call site is worth.
    #[test]
    fn user_day_buckets_on_local_midnight() {
        use chrono::{Duration, Local, TimeZone, Timelike};

        let local_now = Local::now();
        let midnight = Local
            .from_local_datetime(&local_now.date_naive().and_hms_opt(0, 0, 0).unwrap())
            .single()
            .expect("local midnight is unambiguous");
        assert_eq!(midnight.hour(), 0);

        // One second either side of local midnight lands on different local days.
        let before = midnight - Duration::seconds(1);
        let after = midnight + Duration::seconds(1);
        assert_eq!(user_day(after), midnight.date_naive());
        assert_eq!(user_day(before), midnight.date_naive() - Duration::days(1));
        // And the key property: the bucket follows Local, not the instant's own
        // timezone. `before.with_timezone(&Utc).date_naive()` is the buggy form.
        assert_eq!(
            user_day(before.with_timezone(&chrono::Utc)),
            user_day(before)
        );
        assert_eq!(user_day_key(after), midnight.format("%Y-%m-%d").to_string());
    }

    #[test]
    fn same_version_relaunch_leaves_snapshot_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_config(base, "client-setup.json", "old");
        snapshot_state_on_version_change(base, "0.9.3");
        // The build now rewrites its state; a same-version relaunch must not
        // overwrite the pre-update copy with post-update state.
        write_config(base, "client-setup.json", "rewritten by 0.9.3");
        snapshot_state_on_version_change(base, "0.9.3");
        assert_eq!(
            std::fs::read_to_string(snapshot_file(base, "client-setup.json")).unwrap(),
            "old"
        );
    }

    #[test]
    fn next_version_change_replaces_snapshot_with_the_build_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_config(base, "activity-facts.json", "state written by 0.9.2");
        snapshot_state_on_version_change(base, "0.9.3");
        write_config(base, "activity-facts.json", "state written by 0.9.3");
        snapshot_state_on_version_change(base, "0.9.4");
        assert_eq!(
            std::fs::read_to_string(snapshot_file(base, "activity-facts.json")).unwrap(),
            "state written by 0.9.3"
        );
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(snapshot_file(base, "meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["from_version"], "0.9.3");
        assert_eq!(meta["to_version"], "0.9.4");
    }
}
