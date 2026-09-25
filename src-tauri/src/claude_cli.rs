use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SHELL_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(3);

pub fn detect_claude_cli() -> Option<PathBuf> {
    detect_cli("claude")
}

pub fn detect_codex_cli() -> Option<PathBuf> {
    detect_cli("codex")
}

pub fn detect_npx() -> Option<PathBuf> {
    detect_cli("npx")
}

fn detect_cli(name: &str) -> Option<PathBuf> {
    if let Some(path) = probe_known_paths(name) {
        return Some(path);
    }
    // PATH lookup before the login shell: on Windows none of the POSIX
    // candidate dirs exist and there is no `/bin/zsh` to probe, so this is the
    // only branch that can resolve `npx.cmd` / `claude.cmd` -- without it the
    // Context7 and plugin addons reported "not found on PATH" on every Windows
    // box regardless of what was installed. On Unix it is a cheap extra hit
    // ahead of the 2s interactive-shell probe.
    if let Some(path) = probe_on_path(name) {
        return Some(path);
    }
    if let Some(path) = probe_version_manager_dirs(name) {
        return Some(path);
    }
    probe_via_login_shell(name)
}

pub(crate) fn probe_on_path(name: &str) -> Option<PathBuf> {
    let path = crate::client_adapters::find_on_path(&[name])?;
    is_runnable(&path).then_some(path)
}

pub(crate) fn probe_known_paths(name: &str) -> Option<PathBuf> {
    first_runnable(known_path_candidates(home_dir(), name).into_iter())
}

fn known_path_candidates(home: PathBuf, name: &str) -> Vec<PathBuf> {
    known_path_candidates_for_platform(home, name, cfg!(windows))
}

fn known_path_candidates_for_platform(home: PathBuf, name: &str, windows: bool) -> Vec<PathBuf> {
    let base = vec![
        // The official installer (`curl -fsSL https://claude.ai/install.sh | bash`,
        // which our own "Install the Claude Code CLI" banner suggests) drops the
        // binary here. GUI launches inherit launchd's bare PATH so `find_on_path`
        // cannot see it, and the login-shell probe is a coin flip against a noisy
        // or slow `.zshrc` -- so a stock install was undetectable (issue #59).
        // First, because that is the order the user's own shell resolves it in.
        home.join(".local").join("bin").join(name),
        home.join(".claude").join("local").join(name),
        PathBuf::from(format!("/opt/homebrew/bin/{name}")),
        PathBuf::from(format!("/usr/local/bin/{name}")),
        home.join(".npm-global").join("bin").join(name),
        home.join(".volta").join("bin").join(name),
        home.join(".bun").join("bin").join(name),
        PathBuf::from(format!("/usr/bin/{name}")),
    ];
    if !windows {
        return base;
    }

    let mut candidates = Vec::with_capacity(base.len() * 5);
    for path in base {
        for extension in ["exe", "cmd", "bat", "com"] {
            candidates.push(path.with_extension(extension));
        }
        candidates.push(path);
    }
    candidates
}

/// Version-managed node trees (nvm/mise/fnm) keep binaries under
/// per-version dirs no static candidate list can name
/// (`~/.nvm/versions/node/v22.1.0/bin/claude`). GUI launches inherit
/// launchd's bare PATH and the login-shell probe is a coin flip against a
/// noisy rc file (RUST-AZ: learn ran with no usable `claude` on a machine
/// that has one) -- so enumerate the version dirs directly, newest first,
/// and smoke-test like every other candidate.
fn probe_version_manager_dirs(name: &str) -> Option<PathBuf> {
    first_runnable(version_manager_candidates(home_dir(), name).into_iter())
}

fn version_manager_candidates(home: PathBuf, name: &str) -> Vec<PathBuf> {
    let roots = [
        (
            home.join(".nvm").join("versions").join("node"),
            PathBuf::from("bin"),
        ),
        (
            home.join(".local")
                .join("share")
                .join("mise")
                .join("installs")
                .join("node"),
            PathBuf::from("bin"),
        ),
        (
            home.join(".fnm").join("node-versions"),
            PathBuf::from("installation").join("bin"),
        ),
    ];
    let mut candidates = Vec::new();
    for (root, bin) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut versions: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        sort_versions_newest_first(&mut versions);
        for version in versions {
            candidates.push(root.join(version).join(&bin).join(name));
        }
    }
    candidates
}

/// Descending by the numeric runs in the name ("v10.1.0" above "v9.9.9",
/// which plain string order gets backwards). Ties and non-numeric names sort
/// arbitrarily but deterministically -- every candidate is smoke-tested
/// anyway, order only decides which working install wins.
fn sort_versions_newest_first(versions: &mut [String]) {
    fn numeric_key(version: &str) -> Vec<u64> {
        let mut nums = Vec::new();
        let mut current = String::new();
        for ch in version.chars() {
            if ch.is_ascii_digit() {
                current.push(ch);
            } else if !current.is_empty() {
                nums.push(current.parse().unwrap_or(0));
                current.clear();
            }
        }
        if !current.is_empty() {
            nums.push(current.parse().unwrap_or(0));
        }
        nums
    }
    versions.sort_by_key(|v| std::cmp::Reverse(numeric_key(v)));
}

fn first_runnable<I: Iterator<Item = PathBuf>>(candidates: I) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|candidate| is_runnable(candidate))
}

fn probe_via_login_shell(name: &str) -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zsh");
    let flags = match shell_name {
        "fish" => "-lc",
        _ => "-ilc",
    };

    let mut command = crate::proc::command(&shell);
    command.arg(flags).arg(format!("command -v {name}"));
    read_path_from_shell(command, SHELL_LOOKUP_TIMEOUT)
}

/// Spawns `command`, reads the first stdout line naming an existing file, kills
/// the child, and returns the line as a validated `PathBuf`. The timeout
/// bounds how long we wait for that first line — NOT how long we wait for
/// the child to exit. Interactive shells (`-ilc`) print the `command -v`
/// result immediately but then run through `.zshrc`, so waiting for exit
/// before reading stdout was dropping valid paths on the floor.
fn read_path_from_shell(mut command: Command, timeout: Duration) -> Option<PathBuf> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim().to_string();
            // Interactive shells source the user's rc files *before* running
            // our `command -v`, so the first line on the pipe is frequently an
            // rc-file banner. Skip anything that does not name a real file
            // instead of handing the caller the banner and giving up.
            if trimmed.is_empty() || !Path::new(&trimmed).is_file() {
                continue;
            }
            let _ = tx.send(trimmed);
            return;
        }
    });

    let first_line = rx.recv_timeout(timeout).ok();
    let _ = child.kill();
    let _ = child.wait();

    let first_line = first_line?;
    if first_line.is_empty() {
        return None;
    }
    let path = PathBuf::from(first_line);
    if is_runnable(&path) {
        Some(path)
    } else {
        None
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() {
        return false;
    }
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => true,
        _ => false,
    }
}

/// `is_executable` only checks the POSIX exec bit; on dual-architecture Macs
/// an Intel-only Homebrew remnant in `/usr/local/bin` will satisfy the bit but
/// the kernel returns `ENOEXEC` when we try to run it. Smoke-test by spawning
/// `<path> --version` with a short timeout and rejecting anything that fails
/// to spawn or exits non-zero.
///
/// PATH augmentation: the candidate's parent directory is prepended to PATH
/// for the smoke test. nvm/volta/bun/asdf-managed `claude` installs are
/// `#!/usr/bin/env node` scripts with `node` colocated in the same bin dir;
/// without this, GUI launches inherit launchd's bare PATH and `env` fails to
/// resolve `node`, exit 127, and we'd reject a perfectly working `claude`.
fn is_runnable(path: &Path) -> bool {
    if !is_executable(path) {
        return false;
    }
    let mut command = crate::proc::command(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = path.parent() {
        command.env("PATH", crate::proc::path_with_dir_prepended(dir));
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    matches!(wait_with_timeout(child, SMOKE_TEST_TIMEOUT), Some(status) if status.success())
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// See `client_adapters::home_dir`: one resolver for every module.
fn home_dir() -> PathBuf {
    crate::client_adapters::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    struct ScopedTempDir(PathBuf);
    impl ScopedTempDir {
        fn new(label: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "headroom_claude_cli_{}_{}",
                label,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self(base)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for ScopedTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(windows)]
    fn make_executable(path: &Path) {
        fs::write(path, "").unwrap();
    }

    #[test]
    fn is_executable_accepts_executable_files() {
        let tmp = ScopedTempDir::new("is_exec_ok");
        let path = tmp.path().join("claude");
        make_executable(&path);
        assert!(is_executable(&path));
    }

    #[test]
    #[cfg(unix)]
    fn is_executable_rejects_non_executable_files() {
        let tmp = ScopedTempDir::new("is_exec_no");
        let path = tmp.path().join("not_exec");
        fs::write(&path, "").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();
        assert!(!is_executable(&path));
    }

    #[test]
    fn is_executable_rejects_missing_path() {
        assert!(!is_executable(Path::new("/nonexistent/claude")));
    }

    #[test]
    fn is_executable_rejects_directories() {
        let tmp = ScopedTempDir::new("is_exec_dir");
        assert!(!is_executable(tmp.path()));
    }

    #[test]
    // On Windows make_executable writes a plain file with no runnable format,
    // so the spawn inside is_runnable can only fail; the positive path is
    // covered by the real-Windows smoke test instead.
    #[cfg(unix)]
    fn is_runnable_accepts_working_executable() {
        let tmp = ScopedTempDir::new("runnable_ok");
        let path = tmp.path().join("claude");
        make_executable(&path);
        assert!(is_runnable(&path));
    }

    #[test]
    #[cfg(unix)]
    fn is_runnable_rejects_executable_that_fails_to_spawn() {
        // Regression: an x86_64-only Homebrew leftover at /usr/local/bin/claude
        // on an arm64 Mac satisfied the POSIX exec bit but the kernel returned
        // ENOEXEC when we tried to run it. The Python `mcp install` then hit
        // the same ENOEXEC via shutil.which, raising an uncaught OSError and
        // surfacing as "Headroom MCP install exited non-zero" in Sentry.
        let tmp = ScopedTempDir::new("runnable_enoexec");
        let path = tmp.path().join("claude");
        fs::write(&path, b"\x00\x01\x02\x03not a binary").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        assert!(!is_runnable(&path));
    }

    #[test]
    #[cfg(unix)]
    fn is_runnable_rejects_executable_that_exits_non_zero() {
        let tmp = ScopedTempDir::new("runnable_exit1");
        let path = tmp.path().join("claude");
        fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        assert!(!is_runnable(&path));
    }

    #[test]
    fn is_runnable_rejects_non_executable_file() {
        let tmp = ScopedTempDir::new("runnable_no_x");
        let path = tmp.path().join("claude");
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        // Exec bit not set — must short-circuit without spawning.
        assert!(!is_runnable(&path));
    }

    #[test]
    #[cfg(unix)]
    fn probe_on_path_rejects_an_existing_but_broken_entry() {
        let _env_lock = crate::test_env_lock::lock_home();
        let tmp = ScopedTempDir::new("path_broken");
        fs::write(tmp.path().join("headroom"), "not executable\n").unwrap();
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", tmp.path());

        let found = probe_on_path("headroom");

        match saved_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        assert!(found.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn first_runnable_walks_past_broken_candidates() {
        // Reproduces the production scenario: an Intel-only `/usr/local/bin/claude`
        // remnant on an arm64 Mac is the second candidate examined; the first
        // candidate (`~/.claude/local/claude`) does not exist; we want detection
        // to skip the broken candidate and find the working one further down.
        let tmp = ScopedTempDir::new("first_runnable_walk");
        let broken = tmp.path().join("usr_local_claude");
        fs::write(&broken, b"\x00\x01\x02\x03not a binary").unwrap();
        let mut perms = fs::metadata(&broken).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&broken, perms).unwrap();

        let working = tmp.path().join("npm_global_claude");
        make_executable(&working);

        let candidates = vec![
            tmp.path().join("does_not_exist"),
            broken.clone(),
            working.clone(),
            tmp.path().join("never_reached"), // would fail if we kept walking
        ];

        assert_eq!(
            first_runnable(candidates.into_iter()).as_deref(),
            Some(working.as_path())
        );
    }

    #[test]
    #[cfg(unix)]
    fn first_runnable_returns_none_when_all_candidates_broken() {
        let tmp = ScopedTempDir::new("first_runnable_none");
        let broken = tmp.path().join("broken");
        fs::write(&broken, b"\x00\x01\x02\x03").unwrap();
        let mut perms = fs::metadata(&broken).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&broken, perms).unwrap();

        let candidates = vec![tmp.path().join("missing"), broken];
        assert!(first_runnable(candidates.into_iter()).is_none());
    }

    #[test]
    fn version_sort_is_numeric_not_lexicographic() {
        let mut versions = vec![
            "v9.9.9".to_string(),
            "v22.1.0".to_string(),
            "v10.0.0".to_string(),
        ];
        sort_versions_newest_first(&mut versions);
        assert_eq!(versions, ["v22.1.0", "v10.0.0", "v9.9.9"]);
    }

    #[test]
    fn version_manager_candidates_walk_nvm_newest_first() {
        let dir = ScopedTempDir::new("vm-candidates");
        let home = dir.path().to_path_buf();
        for version in ["v9.0.0", "v22.1.0"] {
            std::fs::create_dir_all(
                home.join(".nvm")
                    .join("versions")
                    .join("node")
                    .join(version)
                    .join("bin"),
            )
            .unwrap();
        }
        let candidates = version_manager_candidates(home.clone(), "claude");
        let nvm = home.join(".nvm").join("versions").join("node");
        assert_eq!(
            candidates,
            [
                nvm.join("v22.1.0").join("bin").join("claude"),
                nvm.join("v9.0.0").join("bin").join("claude"),
            ]
        );
    }

    #[test]
    fn known_path_candidates_includes_apple_silicon_and_intel_homebrew_in_order() {
        // Apple Silicon Homebrew (/opt/homebrew/bin) must be examined before
        // Intel Homebrew (/usr/local/bin) so that arm64 Macs that ALSO have an
        // Intel-only `claude` left behind in /usr/local/bin reach the working
        // arm64 binary first. The bug we fixed only surfaced because all
        // earlier candidates were missing.
        let candidates = known_path_candidates(PathBuf::from("/Users/test"), "claude");
        let opt = candidates
            .iter()
            .position(|p| p == Path::new("/opt/homebrew/bin/claude"));
        let usr = candidates
            .iter()
            .position(|p| p == Path::new("/usr/local/bin/claude"));
        assert!(opt.is_some() && usr.is_some());
        assert!(opt.unwrap() < usr.unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn is_runnable_finds_colocated_interpreter_via_augmented_path() {
        // Regression: nvm/volta/bun/asdf installs of `claude` are
        // `#!/usr/bin/env <interp>` scripts with the interpreter colocated in
        // the same bin/. GUI launches inherit launchd's bare PATH, so without
        // augmenting PATH with the candidate's parent, `env` exits 127 and we
        // reject a working `claude`. Simulate by writing a script that shebangs
        // a colocated fake interpreter, then strip PATH so the test inherits
        // nothing useful.
        let tmp = ScopedTempDir::new("runnable_colocated_interp");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();

        let interp = bin.join("fakenode");
        fs::write(&interp, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&interp).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&interp, perms).unwrap();

        let claude = bin.join("claude");
        fs::write(&claude, "#!/usr/bin/env fakenode\n").unwrap();
        let mut perms = fs::metadata(&claude).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&claude, perms).unwrap();

        // Strip PATH so the only way `env fakenode` can resolve is via the
        // augmentation `is_runnable` adds.
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", "/usr/bin:/bin");
        let result = is_runnable(&claude);
        match saved {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        assert!(result, "is_runnable must augment PATH with the candidate's bin dir so colocated interpreters resolve");
    }

    #[test]
    #[cfg(unix)]
    fn is_runnable_kills_and_rejects_a_hung_executable() {
        // A binary that hangs forever must not stall detection. We override
        // the timeout indirectly by invoking wait_with_timeout directly with
        // a short bound.
        let tmp = ScopedTempDir::new("runnable_hang");
        let path = tmp.path().join("claude");
        fs::write(&path, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();

        let child = crate::proc::command(&path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let start = Instant::now();
        let result = wait_with_timeout(child, Duration::from_millis(200));
        let elapsed = start.elapsed();
        assert!(result.is_none());
        assert!(
            elapsed < Duration::from_secs(1),
            "timeout should bound the wait; took {elapsed:?}",
        );
    }

    #[test]
    #[cfg(unix)]
    fn read_path_from_shell_returns_path_before_shell_exits() {
        // Regression: interactive shells print the `command -v claude` output
        // immediately but keep running through `.zshrc`. Previously we waited
        // for the child to exit before reading stdout, so a slow shell init
        // would cause a timeout even when the path was already on the pipe.
        let tmp = ScopedTempDir::new("probe_slow_shell");
        let fake_claude = tmp.path().join("claude");
        make_executable(&fake_claude);
        let claude_str = fake_claude.display().to_string();

        let mut cmd = crate::proc::command("/bin/sh");
        cmd.arg("-c").arg(format!("echo {claude_str}; sleep 30"));

        let start = Instant::now();
        let got = read_path_from_shell(cmd, Duration::from_secs(2));
        let elapsed = start.elapsed();

        assert_eq!(got.as_deref(), Some(fake_claude.as_path()));
        assert!(
            elapsed < Duration::from_secs(2),
            "should return as soon as the first line arrives, not wait for the sleep; took {elapsed:?}",
        );
    }

    #[test]
    fn known_path_candidates_probe_the_official_installer_target_first() {
        // Issue #59: `curl -fsSL https://claude.ai/install.sh | bash` installs to
        // ~/.local/bin, which GUI-launched processes cannot see (launchd hands us
        // PATH=/usr/bin:/bin:/usr/sbin:/sbin). Absent from this list, a stock
        // install was invisible whenever the login-shell probe also failed.
        // Assert the directory, not the filename: on Windows the first
        // candidate is `claude.exe` in that same directory (the extension
        // sweep is pinned by known_windows_paths_probe_executable_extensions).
        let candidates = known_path_candidates(PathBuf::from("/Users/test"), "claude");
        assert_eq!(
            candidates.first().and_then(|path| path.parent()),
            Some(Path::new("/Users/test/.local/bin")),
        );
    }

    #[test]
    fn known_windows_paths_probe_executable_extensions() {
        let candidates =
            known_path_candidates_for_platform(PathBuf::from("/Users/test"), "headroom", true);
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(Path::new("/Users/test/.local/bin/headroom.exe")),
        );
        assert!(candidates
            .iter()
            .any(|path| path == Path::new("/Users/test/.local/bin/headroom.cmd")));
    }

    #[test]
    #[cfg(unix)]
    fn read_path_from_shell_skips_rc_file_banner_lines() {
        // Regression: `zsh -ilc` sources .zshrc before running `command -v`, so
        // anything the user's rc file prints lands on the pipe first. Taking the
        // first non-empty line handed us the banner and reported "not installed".
        let tmp = ScopedTempDir::new("probe_noisy_rc");
        let fake_claude = tmp.path().join("claude");
        make_executable(&fake_claude);
        let claude_str = fake_claude.display().to_string();

        let mut cmd = crate::proc::command("/bin/sh");
        cmd.arg("-c")
            .arg(format!("echo 'Welcome back!'; echo ''; echo {claude_str}"));

        assert_eq!(
            read_path_from_shell(cmd, Duration::from_secs(2)).as_deref(),
            Some(fake_claude.as_path()),
        );
    }

    #[test]
    // On Windows the /bin/sh spawn fails outright, returning None immediately
    // and passing both asserts without exercising the timeout at all.
    #[cfg(unix)]
    fn read_path_from_shell_times_out_when_no_output() {
        let mut cmd = crate::proc::command("/bin/sh");
        cmd.arg("-c").arg("sleep 30");

        let start = Instant::now();
        let got = read_path_from_shell(cmd, Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(got.is_none());
        assert!(
            elapsed < Duration::from_secs(1),
            "timeout should bound the wait; took {elapsed:?}",
        );
    }
}
