use std::process::{exit, Command};

fn main() {
    let python = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("python.exe")));
    let Some(python) = python else {
        eprintln!("Could not locate the Headroom Python runtime");
        exit(1);
    };
    let status = Command::new(python)
        .arg("-c")
        .arg(
            "import sys; sys.argv[0] = 'headroom'; from headroom.cli import main; sys.exit(main())",
        )
        .args(std::env::args_os().skip(1))
        .status();
    match status {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("Could not start Headroom: {err}");
            exit(1);
        }
    }
}
