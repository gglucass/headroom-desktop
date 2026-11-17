//! The Windows VS Code process wrapper, as a copy of the signed app binary.
//!
//! Smart App Control and App Control for Business refuse unsigned executables,
//! and the old wrapper (pip's launcher stub with a zip appended) could never
//! carry a signature, so Claude Code in the VS Code panel did not start at all
//! on those machines. A byte-for-byte copy of `headroom-desktop.exe` keeps its
//! Authenticode signature. Started under the wrapper's name, it runs the Python
//! wrapper beside it with the runtime's signed python.exe and never starts the
//! app. It is a copy, not the installed binary: a panel session holds its
//! wrapper open, and the app updater must still be able to replace the app.
#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// File stem of the wrapper copy (`.exe`) and its script (`.py`).
pub const WRAPPER_STEM: &str = "headroom-claude-wrapper";

/// The wrapper script with the interpreter on its first line, the way a
/// shebang names one. Python reads that line as a comment.
pub fn script_with_interpreter(python: &Path, source: &str) -> String {
    format!("#!\"{}\"\n{source}", python.display())
}

/// The interpreter a wrapper script names on its first line.
pub fn script_interpreter(script: &str) -> Option<PathBuf> {
    let path = script
        .lines()
        .next()?
        .strip_prefix("#!\"")?
        .strip_suffix('"')?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// When this process is the wrapper copy, run the wrapper and return its exit
/// code; `None` for the app itself.
pub fn run_as_wrapper() -> Option<i32> {
    let exe = std::env::current_exe().ok()?;
    if !exe
        .file_stem()?
        .to_str()?
        .eq_ignore_ascii_case(WRAPPER_STEM)
    {
        return None;
    }
    Some(run(
        &exe.with_extension("py"),
        std::env::args_os().skip(1).collect(),
    ))
}

/// `<python> <script> <args...>` with this process's stdio, tied to its
/// lifetime: when VS Code ends the wrapper, the CLI under it ends too, as it
/// did with pip's launcher.
fn run(script: &Path, args: Vec<OsString>) -> i32 {
    let Some(python) = std::fs::read_to_string(script)
        .ok()
        .as_deref()
        .and_then(script_interpreter)
    else {
        eprintln!(
            "Headroom's VS Code wrapper is incomplete ({} names no Python). Open Headroom to repair it.",
            script.display()
        );
        return 1;
    };
    // proc::command adds CREATE_NO_WINDOW: this binary is GUI-subsystem, so a
    // console python.exe would otherwise get a visible console window.
    let mut child = match crate::proc::command(&python).arg(script).args(args).spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("Could not start {}: {err}", python.display());
            return 1;
        }
    };
    #[cfg(windows)]
    crate::winproc::tie_to_this_process(&child);
    match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_names_its_interpreter_on_the_first_line() {
        let python =
            Path::new(r"C:\Users\A B\AppData\Local\Headroom\headroom\runtime\python\python.exe");
        let script = script_with_interpreter(python, "print(1)\n");
        assert_eq!(script_interpreter(&script).as_deref(), Some(python));
        assert!(script.ends_with("\nprint(1)\n"));
        assert_eq!(script_interpreter("print(1)\n"), None);
        assert_eq!(script_interpreter("#!\"\"\nprint(1)\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapper_runs_its_script_with_the_named_python_and_passes_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("headroom-claude-wrapper.py");
        let seen = dir.path().join("argv.txt");
        std::fs::write(
            &script,
            script_with_interpreter(
                Path::new("/usr/bin/python3"),
                &format!(
                    "import sys\nopen({:?}, 'w').write(' '.join(sys.argv[1:]))\nsys.exit(7)\n",
                    seen.display().to_string()
                ),
            ),
        )
        .unwrap();
        assert_eq!(run(&script, vec!["--confirm".into(), "a b".into()]), 7);
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "--confirm a b");

        std::fs::write(&script, "print(1)\n").unwrap();
        assert_eq!(run(&script, Vec::new()), 1);
    }
}
