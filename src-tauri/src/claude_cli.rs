use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

    let mut child = Command::new(&shell)
        .arg(flags)
        .arg("command -v claude")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + SHELL_LOOKUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next()?.trim();
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
}
