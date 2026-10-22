//! Machine-caused I/O failures on Headroom's own data folder.
//!
//! Permission denied, a read-only filesystem and a full disk are not bugs a
//! release can fix, and they hit every file in the folder at once. Handled per
//! call site they became one Sentry issue per file (RUST-QY/QZ/R0: one
//! read-only home, three issues; RUST-R1/R2: one blocked folder, two) and no
//! word to the user, while the state they left behind was misread downstream:
//! an unreadable `client-setup.json` loads as the empty default, which the
//! tray announces as "your coding tools were disconnected".
//!
//! So the class is handled once, here. The two choke points every own-state
//! access already goes through (`client_adapters::atomic_write` and
//! `state::read_state_file`) report each outcome with `note`. A failure is
//! classified from the `io::Error` (kind and errno, never message text); a
//! success clears that operation's fault, so a fixed machine clears its banner
//! on the next read or write. The rest of the app asks this module:
//!
//! - the Home banner: `active_issues`, carried in `RuntimeStatus`
//! - Sentry: the failing call site's message carries `NOTE`, which
//!   `logging::skip_sentry` drops, and this module captures one event per
//!   fault instead, fingerprinted by kind so every machine lands in one issue
//! - logic that would misread defaults: `state_unreadable`
//!
//! A new kind of machine fault belongs in `FaultKind`, not in another
//! `RuntimeStatus` hint field or another `skip_sentry` string.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Appended to the error message of a failed access that was classified here.
/// `logging::skip_sentry` drops any log line carrying it, since this module
/// reports the fault itself.
pub(crate) const NOTE: &str = " [machine fault on Headroom's data folder]";

/// Consecutive failures of one operation before the fault is shown and
/// reported. One failure can be a transient lock (an antivirus scan holding a
/// file on Windows); a second with no success in between is the machine.
const SUSTAINED_FAILURES: u32 = 2;

/// A fault nothing has retried for this long is dropped, so a banner never
/// outlives the evidence for it.
const FAULT_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Op {
    Read,
    Write,
}

impl Op {
    fn label(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FaultKind {
    /// The OS or security software refused the access (EPERM on macOS/Linux,
    /// ERROR_ACCESS_DENIED on Windows): endpoint protection, macOS privacy
    /// controls, a locked file.
    Blocked,
    /// File permissions refused it (EACCES): the folder belongs to another
    /// account, typically after Headroom ran once under sudo.
    Permissions,
    ReadOnly,
    DiskFull,
}

impl FaultKind {
    fn label(self) -> &'static str {
        match self {
            FaultKind::Blocked => "blocked",
            FaultKind::Permissions => "permissions",
            FaultKind::ReadOnly => "read_only",
            FaultKind::DiskFull => "disk_full",
        }
    }

    /// Disk-full stays out of Sentry entirely, as `logging::is_disk_full` has
    /// always kept it; the user still gets the banner.
    fn reported(self) -> bool {
        self != FaultKind::DiskFull
    }
}

/// The machine fault behind `err`, if it is one. Errors that say something
/// about the file itself (NotFound, bad data) or are transient are `None`.
pub(crate) fn classify(err: &std::io::Error) -> Option<FaultKind> {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::PermissionDenied => {
            // EACCES is the permission bits; EPERM, and Windows' one
            // access-denied code, is something refusing the operation.
            if cfg!(unix) && err.raw_os_error() == Some(13) {
                Some(FaultKind::Permissions)
            } else {
                Some(FaultKind::Blocked)
            }
        }
        ErrorKind::ReadOnlyFilesystem => Some(FaultKind::ReadOnly),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => Some(FaultKind::DiskFull),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct Fault {
    kind: FaultKind,
    failures: u32,
    last_seen: Instant,
}

/// Per-operation fault state. Pure: the caller supplies the clock and decides
/// what is in scope, so it tests without touching the real data folder.
#[derive(Debug, Default)]
struct Faults {
    read: Option<Fault>,
    write: Option<Fault>,
}

impl Faults {
    fn slot(&mut self, op: Op) -> &mut Option<Fault> {
        match op {
            Op::Read => &mut self.read,
            Op::Write => &mut self.write,
        }
    }

    fn get(&self, op: Op) -> Option<&Fault> {
        match op {
            Op::Read => self.read.as_ref(),
            Op::Write => self.write.as_ref(),
        }
    }

    fn ok(&mut self, op: Op) {
        *self.slot(op) = None;
    }

    /// Records a failure; true when this one makes the fault sustained.
    fn fail(&mut self, op: Op, kind: FaultKind, now: Instant) -> bool {
        let slot = self.slot(op);
        let failures = match slot {
            Some(prev) if now.duration_since(prev.last_seen) < FAULT_TTL => prev.failures + 1,
            _ => 1,
        };
        *slot = Some(Fault {
            kind,
            failures,
            last_seen: now,
        });
        failures == SUSTAINED_FAILURES
    }

    fn sustained(&self, op: Op, now: Instant) -> Option<&Fault> {
        self.get(op).filter(|fault| {
            fault.failures >= SUSTAINED_FAILURES && now.duration_since(fault.last_seen) < FAULT_TTL
        })
    }
}

static FAULTS: Mutex<Faults> = Mutex::new(Faults {
    read: None,
    write: None,
});

/// Faults already sent to Sentry by this process, so a fault that clears and
/// returns (a flapping lock) is one event, not one per flap.
static REPORTED: Mutex<Vec<(Op, FaultKind)>> = Mutex::new(Vec::new());

/// Test-only stand-in for the data folder. `HEADROOM_DATA_DIR` is swapped by
/// tests in other modules under other locks, so a test that scoped by it
/// raced them.
#[cfg(test)]
static TEST_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
pub(crate) struct ReadFaultGuard;

#[cfg(test)]
impl Drop for ReadFaultGuard {
    fn drop(&mut self) {
        *FAULTS.lock() = Faults::default();
        *TEST_ROOT.lock() = None;
    }
}

/// Holds a sustained read fault until the guard drops, for tests of the logic
/// that holds off on one. The stand-in root keeps other tests' successful
/// reads from clearing it. Process-wide, so callers are serial.
#[cfg(test)]
pub(crate) fn hold_read_fault() -> ReadFaultGuard {
    *TEST_ROOT.lock() = Some(PathBuf::from("/nonexistent/headroom-read-fault"));
    let now = Instant::now();
    let mut faults = FAULTS.lock();
    *faults = Faults::default();
    for _ in 0..SUSTAINED_FAILURES {
        faults.fail(Op::Read, FaultKind::Blocked, now);
    }
    ReadFaultGuard
}

fn in_scope(path: &Path) -> bool {
    #[cfg(test)]
    if let Some(root) = TEST_ROOT.lock().as_ref() {
        return path.starts_with(root);
    }
    path.starts_with(crate::storage::app_data_dir())
}

/// Records the outcome of one access to `path` and returns the note for its
/// error message: `NOTE` when the failure is a machine fault on Headroom's
/// data folder, empty otherwise. Paths outside the folder (client configs,
/// shell profiles) are left to their own handling.
pub(crate) fn note<T>(path: &Path, op: Op, result: &std::io::Result<T>) -> &'static str {
    match result {
        Ok(_) => {
            succeeded(path, op);
            ""
        }
        Err(err) => failed(path, op, err),
    }
}

/// `note` for a whole operation's success, when its steps report failures
/// one by one (`atomic_write`'s mkdir, write and rename): only the operation
/// as a whole clears the fault.
pub(crate) fn succeeded(path: &Path, op: Op) {
    if in_scope(path) {
        FAULTS.lock().ok(op);
    }
}

/// `note` for one failed step; returns the note for its error message.
pub(crate) fn failed(path: &Path, op: Op, err: &std::io::Error) -> &'static str {
    if !in_scope(path) {
        return "";
    }
    let Some(kind) = classify(err) else {
        return "";
    };
    let now = Instant::now();
    let (became_sustained, sustained) = {
        let mut faults = FAULTS.lock();
        let became = faults.fail(op, kind, now);
        (became, faults.sustained(op, now).is_some())
    };
    if became_sustained {
        log::info!(
            "env_fault: sustained {} fault ({}) on {}: {err}",
            op.label(),
            kind.label(),
            path.display()
        );
    }
    if sustained {
        report(op, kind, path, err);
    }
    NOTE
}

fn report(op: Op, kind: FaultKind, path: &Path, err: &std::io::Error) {
    // A fault sustained before sentry::init (the launch-time setup read) waits
    // for a client rather than spending its one report on nothing.
    if !kind.reported() || sentry::Hub::current().client().is_none() {
        return;
    }
    {
        let mut reported = REPORTED.lock();
        if reported.contains(&(op, kind)) {
            return;
        }
        reported.push((op, kind));
    }
    sentry::with_scope(
        |scope| {
            scope.set_fingerprint(Some(&["env-fault", op.label(), kind.label()]));
            scope.set_tag("flow", "env_fault");
            scope.set_tag("env_fault.op", op.label());
            scope.set_tag("env_fault.kind", kind.label());
            // The file name only: the folder is always Headroom's own.
            scope.set_extra(
                "file",
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
            );
        },
        || {
            sentry::capture_message(
                &format!(
                    "Headroom's data folder: {} {} ({})",
                    op.label(),
                    kind.label(),
                    err
                ),
                sentry::Level::Warning,
            )
        },
    );
}

/// True while reads of Headroom's own state are failing, so what was loaded is
/// a default rather than the user's state. Logic that would announce a change
/// read from that state (the "disconnected" notification) holds off.
pub(crate) fn state_unreadable() -> bool {
    FAULTS.lock().sustained(Op::Read, Instant::now()).is_some()
}

/// True while a reported fault is sustained. Whatever else fails on that
/// machine meanwhile (a child process that cannot open its own files in the
/// folder) is the same fault, which `report` has already sent once. Disk-full
/// is never reported, so it mutes nothing: no echo to stand in for.
pub(crate) fn active() -> bool {
    let now = Instant::now();
    let faults = FAULTS.lock();
    [Op::Read, Op::Write].into_iter().any(|op| {
        faults
            .sustained(op, now)
            .is_some_and(|fault| fault.kind.reported())
    })
}

/// Banner prose for each sustained fault, read first: an unreadable folder
/// explains a write failure too, never the reverse.
pub(crate) fn active_issues() -> Vec<String> {
    let now = Instant::now();
    let faults = FAULTS.lock();
    let data_dir = crate::storage::app_data_dir();
    let dir = display_dir(&data_dir);
    // RUST-R1/R2/R4: a home folder on an external drive the user never let
    // Headroom use reads as `~/...`, so the volume is checked on the real path.
    let external_volume = cfg!(target_os = "macos") && data_dir.starts_with("/Volumes");
    [Op::Read, Op::Write]
        .into_iter()
        .filter_map(|op| {
            faults
                .sustained(op, now)
                .map(|fault| issue_text(op, fault.kind, &dir, external_volume))
        })
        .collect()
}

/// `~`-relative when under home, so the banner shows the path users see.
fn display_dir(dir: &Path) -> String {
    let home: PathBuf = crate::client_adapters::home_dir();
    match dir.strip_prefix(&home) {
        // The platform's separator, so Windows reads `~\AppData\Local\...`.
        Ok(rest) if !home.as_os_str().is_empty() => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        }
        _ => dir.display().to_string(),
    }
}

/// One issue as "<lead>. <detail>", the shape the dashboard's `splitIssue`
/// cuts into a headline and the remedy under it.
fn issue_text(op: Op, kind: FaultKind, dir: &str, external_volume: bool) -> String {
    let lead = match op {
        Op::Read => "Headroom can't read its data folder, so your settings and connected tools may look reset.",
        Op::Write => "Headroom can't save to its data folder, so settings and savings history aren't being kept.",
    };
    let locked = if cfg!(target_os = "macos") {
        " (Finder: Get Info > Locked)"
    } else {
        ""
    };
    let detail = match kind {
        // EPERM on another drive is macOS privacy control, not security software.
        FaultKind::Blocked if external_volume => format!(
            "The folder {dir} is on an external drive macOS isn't letting Headroom use. Turn Headroom on under System Settings > Privacy & Security > Files and Folders (Removable Volumes, or Network Volumes for a network drive), or give it Full Disk Access, then quit and reopen Headroom."
        ),
        FaultKind::Blocked => format!(
            "Something on this computer is blocking access to {dir}, usually security software or a locked file{locked}. Allow Headroom in that software or unlock the folder; Headroom picks up again on its own."
        ),
        FaultKind::Permissions => format!(
            "Your user account doesn't have permission to use {dir}, usually because another account owns it (for example after running Headroom with sudo). Give the folder back to your account; Headroom picks up again on its own."
        ),
        FaultKind::ReadOnly => format!(
            "The folder {dir} is on a read-only filesystem. Run Headroom where that folder is writable."
        ),
        FaultKind::DiskFull => format!(
            "The disk holding {dir} is full. Free up some space; Headroom picks up again on its own."
        ),
    };
    format!("{lead} {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_err(code: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code)
    }

    #[cfg(unix)]
    #[test]
    fn classifies_by_errno_not_message() {
        assert_eq!(classify(&os_err(1)), Some(FaultKind::Blocked)); // EPERM
        assert_eq!(classify(&os_err(13)), Some(FaultKind::Permissions)); // EACCES
        assert_eq!(classify(&os_err(30)), Some(FaultKind::ReadOnly)); // EROFS
        assert_eq!(classify(&os_err(28)), Some(FaultKind::DiskFull)); // ENOSPC
        assert_eq!(classify(&os_err(2)), None); // ENOENT says nothing about the machine
        assert_eq!(classify(&os_err(4)), None); // EINTR is transient
                                                // Prose that merely mentions a permission problem is not one.
        assert_eq!(
            classify(&std::io::Error::other(
                "Operation not permitted (os error 1)"
            )),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn classifies_windows_codes() {
        assert_eq!(classify(&os_err(5)), Some(FaultKind::Blocked)); // ERROR_ACCESS_DENIED
        assert_eq!(classify(&os_err(112)), Some(FaultKind::DiskFull)); // ERROR_DISK_FULL
        assert_eq!(classify(&os_err(2)), None);
    }

    #[test]
    fn one_failure_is_not_a_fault() {
        let mut faults = Faults::default();
        let now = Instant::now();
        assert!(!faults.fail(Op::Write, FaultKind::Blocked, now));
        assert!(faults.sustained(Op::Write, now).is_none());
        assert!(faults.fail(Op::Write, FaultKind::Blocked, now));
        assert!(faults.sustained(Op::Write, now).is_some());
        // Only the transition reports; a third failure does not again.
        assert!(!faults.fail(Op::Write, FaultKind::Blocked, now));
    }

    #[test]
    fn a_success_in_between_resets_the_count() {
        let mut faults = Faults::default();
        let now = Instant::now();
        faults.fail(Op::Write, FaultKind::Blocked, now);
        faults.ok(Op::Write);
        assert!(!faults.fail(Op::Write, FaultKind::Blocked, now));
        assert!(faults.sustained(Op::Write, now).is_none());
    }

    #[test]
    fn a_success_clears_a_sustained_fault() {
        let mut faults = Faults::default();
        let now = Instant::now();
        faults.fail(Op::Read, FaultKind::Blocked, now);
        faults.fail(Op::Read, FaultKind::Blocked, now);
        faults.ok(Op::Read);
        assert!(faults.sustained(Op::Read, now).is_none());
    }

    #[test]
    fn operations_are_tracked_apart() {
        let mut faults = Faults::default();
        let now = Instant::now();
        faults.fail(Op::Read, FaultKind::Blocked, now);
        faults.fail(Op::Read, FaultKind::Blocked, now);
        faults.ok(Op::Write);
        assert!(faults.sustained(Op::Read, now).is_some());
    }

    #[test]
    fn a_fault_nothing_retries_ages_out() {
        let mut faults = Faults::default();
        let then = Instant::now();
        faults.fail(Op::Write, FaultKind::DiskFull, then);
        faults.fail(Op::Write, FaultKind::DiskFull, then);
        let later = then + FAULT_TTL;
        assert!(faults.sustained(Op::Write, later).is_none());
        // And a failure after the gap starts a new count.
        assert!(!faults.fail(Op::Write, FaultKind::DiskFull, later));
    }

    #[test]
    #[serial_test::serial]
    fn only_a_reported_fault_mutes_sentry() {
        // The guard's stand-in root keeps other tests' reads and writes under
        // the data folder from clearing these faults mid-test.
        let _guard = hold_read_fault();
        let now = Instant::now();
        let mut faults = Faults::default();
        faults.fail(Op::Write, FaultKind::DiskFull, now);
        faults.fail(Op::Write, FaultKind::DiskFull, now);
        *FAULTS.lock() = faults;
        assert!(
            !active(),
            "disk-full is never reported, so nothing is its echo"
        );
        FAULTS.lock().fail(Op::Read, FaultKind::Blocked, now);
        FAULTS.lock().fail(Op::Read, FaultKind::Blocked, now);
        assert!(active());
    }

    /// End to end through the real choke points: a data folder the account
    /// cannot read or write reaches the banner and the notification gate, the
    /// error messages carry the Sentry note, and access coming back clears it.
    #[cfg(unix)]
    #[test]
    // Serial: it makes state_unreadable() true process-wide for a moment.
    #[serial_test::serial]
    fn an_inaccessible_data_folder_reaches_the_banner_and_clears_on_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path, bits| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap()
        };
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("Headroom").join("config");
        std::fs::create_dir_all(&config).unwrap();
        let setup = config.join("client-setup.json");
        std::fs::write(&setup, "{}").unwrap();
        mode(&setup, 0o000);
        if std::fs::read(&setup).is_ok() {
            // Root ignores file modes, so nothing here can fail (CI is not root).
            mode(&setup, 0o644);
            eprintln!("skipped: file modes do not apply to this user");
            return;
        }
        struct ScopedRoot;
        impl Drop for ScopedRoot {
            fn drop(&mut self) {
                *TEST_ROOT.lock() = None;
            }
        }
        *TEST_ROOT.lock() = Some(temp.path().join("Headroom"));
        let _root = ScopedRoot;
        *FAULTS.lock() = Faults::default();

        // The setup loader's retry is one access: first tries alone never count.
        for _ in 0..2 {
            assert!(crate::client_adapters::setup_first_try_fails(&setup));
        }
        assert!(!state_unreadable(), "first tries are not failures");

        let err = crate::state::read_state_file(&setup).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains(NOTE), "{err}");
        assert!(!state_unreadable(), "one failure is not yet a fault");
        let _ = crate::state::read_state_file(&setup);
        assert!(state_unreadable());

        mode(&config, 0o555);
        let counters = config.join("usage-counters.json");
        for _ in 0..2 {
            let err = crate::client_adapters::atomic_write(&counters, b"{}").unwrap_err();
            assert!(err.to_string().contains(NOTE), "{err}");
        }
        let issues = active_issues();
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(
            issues[0].starts_with("Headroom can't read"),
            "{}",
            issues[0]
        );
        assert!(
            issues[1].starts_with("Headroom can't save"),
            "{}",
            issues[1]
        );
        assert!(
            issues[0].contains("permission"),
            "EACCES names permissions: {}",
            issues[0]
        );

        mode(&config, 0o755);
        mode(&setup, 0o644);
        crate::state::read_state_file(&setup).unwrap();
        crate::client_adapters::atomic_write(&counters, b"{}").unwrap();
        let issues = active_issues();
        assert!(issues.is_empty(), "{issues:?}");
        assert!(!state_unreadable());
    }

    /// The pre-update snapshot looks past mkdir's AlreadyExists to the stat
    /// error behind it (RUST-RP); a folder it may not create keeps mkdir's own
    /// error, not the stat's NotFound for the folder that was never made.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn a_refused_snapshot_folder_counts_as_a_write_fault() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("Headroom");
        let config = base.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::create_dir(config.join("probe")).is_ok() {
            eprintln!("skipped: file modes do not apply to this user");
            return;
        }
        *TEST_ROOT.lock() = Some(base.clone());
        let _guard = ReadFaultGuard; // resets FAULTS and TEST_ROOT on drop
        *FAULTS.lock() = Faults::default();

        crate::storage::snapshot_state_on_version_change(&base, "0.9.47");

        let kind = FAULTS.lock().write.as_ref().map(|fault| fault.kind);
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(kind, Some(FaultKind::Permissions));
    }

    #[test]
    fn issue_text_splits_into_a_lead_and_a_remedy() {
        // The dashboard's splitIssue cuts at the first ". " before a capital.
        for op in [Op::Read, Op::Write] {
            for kind in [
                FaultKind::Blocked,
                FaultKind::Permissions,
                FaultKind::ReadOnly,
                FaultKind::DiskFull,
            ] {
                for external_volume in [false, true] {
                    let text = issue_text(op, kind, "~/Headroom", external_volume);
                    let cut = text.find(". ").expect("lead and detail");
                    let detail = &text[cut + 2..];
                    assert!(
                        detail.starts_with(|c: char| c.is_ascii_uppercase()),
                        "{text}"
                    );
                    assert!(text.contains("~/Headroom"), "{text}");
                    assert!(text.is_ascii(), "plain ASCII only: {text}");
                }
            }
        }
        assert!(issue_text(Op::Read, FaultKind::Blocked, "~/Headroom", true)
            .contains("Removable Volumes"));
    }
}
