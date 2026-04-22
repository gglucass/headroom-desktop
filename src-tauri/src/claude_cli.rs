use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const SHELL_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

pub fn detect_claude_cli() -> Option<PathBuf> {
    if let Some(path) = probe_known_paths() {
        return Some(path);
    }
    probe_via_login_shell()
}

fn probe_known_paths() -> Option<PathBuf> {
    let home = home_dir();
    let candidates = [
        home.join(".claude").join("local").join("claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
        home.join(".npm-global").join("bin").join("claude"),
        home.join(".volta").join("bin").join("claude"),
        home.join(".bun").join("bin").join("claude"),
        PathBuf::from("/usr/bin/claude"),
    ];
    candidates.into_iter().find(|candidate| is_executable(candidate))
}

fn probe_via_login_shell() -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zsh");
    let flags = match shell_name {
        "fish" => "-lc",
        _ => "-ilc",
    };

    let mut command = Command::new(&shell);
    command.arg(flags).arg("command -v claude");
    read_path_from_shell(command, SHELL_LOOKUP_TIMEOUT)
}

/// Spawns `command`, reads the first non-empty line from its stdout, kills
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
            if trimmed.is_empty() {
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
    if is_executable(&path) {
        Some(path)
    } else {
        None
    }
}

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

fn home_dir() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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

    fn make_executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn is_executable_accepts_executable_files() {
        let tmp = ScopedTempDir::new("is_exec_ok");
        let path = tmp.path().join("claude");
        make_executable(&path);
        assert!(is_executable(&path));
    }

    #[test]
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
    fn read_path_from_shell_returns_path_before_shell_exits() {
        // Regression: interactive shells print the `command -v claude` output
        // immediately but keep running through `.zshrc`. Previously we waited
        // for the child to exit before reading stdout, so a slow shell init
        // would cause a timeout even when the path was already on the pipe.
        let tmp = ScopedTempDir::new("probe_slow_shell");
        let fake_claude = tmp.path().join("claude");
        make_executable(&fake_claude);
        let claude_str = fake_claude.display().to_string();

        let mut cmd = Command::new("/bin/sh");
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
    fn read_path_from_shell_times_out_when_no_output() {
        let mut cmd = Command::new("/bin/sh");
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
