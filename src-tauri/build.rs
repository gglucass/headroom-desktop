use std::env;
use std::fs;
use std::path::Path;

const FORWARDED_ENV_VARS: &[&str] = &[
    "HEADROOM_ACCOUNT_API_BASE_URL",
    "HEADROOM_APTABASE_APP_KEY",
    "HEADROOM_UPDATER_PUBLIC_KEY",
    "HEADROOM_UPDATER_ENDPOINTS",
    "HEADROOM_SENTRY_DSN",
];

fn main() {
    for key in FORWARDED_ENV_VARS {
        println!("cargo:rerun-if-env-changed={key}");
        if env::var_os(key).is_none() {
            if let Some(value) = env_file_value(key) {
                println!("cargo:rustc-env={key}={value}");
            }
        }
    }

    println!("cargo:rerun-if-changed=../.env");
    println!("cargo:rerun-if-changed=../.env.local");
    tauri_build::build()
}

fn env_file_value(key: &str) -> Option<String> {
    ["../.env.local", "../.env"]
        .iter()
        .find_map(|path| read_env_file_value(Path::new(path), key))
}

fn read_env_file_value(path: &Path, key: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (name, raw_value) = trimmed.split_once('=')?;
        if name.trim() != key {
            continue;
        }

        return Some(parse_env_value(raw_value.trim()));
    }

    None
}

fn parse_env_value(raw_value: &str) -> String {
    let unquoted = match raw_value.as_bytes() {
        [quote, middle @ .., end] if (*quote == b'"' || *quote == b'\'') && quote == end => {
            String::from_utf8_lossy(middle).to_string()
        }
        _ => raw_value
            .split(" #")
            .next()
            .unwrap_or(raw_value)
            .trim()
            .to_string(),
    };
    unquoted
}
