use std::path::PathBuf;
use std::process::Command;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::keychain;

const DEVICE_KEYCHAIN_SERVICE: &str = "com.extraheadroom.headroom.device";
const MACHINE_ID_DIGEST_ACCOUNT: &str = "machine-id-digest";

static CACHED: Mutex<Option<DeviceIdentity>> = Mutex::new(None);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub machine_id_digest: String,
    pub chopratejas_instance_id: Option<String>,
    pub os: String,
}

pub fn current() -> DeviceIdentity {
    if let Some(value) = CACHED.lock().clone() {
        return value;
    }
    let identity = DeviceIdentity {
        machine_id_digest: load_or_compute_machine_id_digest(),
        chopratejas_instance_id: read_chopratejas_instance_id(),
        os: describe_os(),
    };
    *CACHED.lock() = Some(identity.clone());
    identity
}

fn load_or_compute_machine_id_digest() -> String {
    if let Ok(Some(cached)) =
        keychain::read_secret(DEVICE_KEYCHAIN_SERVICE, MACHINE_ID_DIGEST_ACCOUNT)
    {
        let trimmed = cached.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let raw = read_hardware_uuid().unwrap_or_else(fallback_identifier);
    let digest = sha256_hex(&raw);

    if let Err(err) =
        keychain::write_secret(DEVICE_KEYCHAIN_SERVICE, MACHINE_ID_DIGEST_ACCOUNT, &digest)
    {
        sentry::capture_message(
            &format!("Could not persist machine id digest: {err}"),
            sentry::Level::Warning,
        );
    }
    digest
}

#[cfg(target_os = "macos")]
fn read_hardware_uuid() -> Option<String> {
    let output = Command::new("/usr/sbin/ioreg")
        .args(["-d2", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("IOPlatformUUID") {
            continue;
        }
        if let Some(uuid) = line.rsplit('"').find(|chunk| !chunk.trim().is_empty()) {
            let trimmed = uuid.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn read_hardware_uuid() -> Option<String> {
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn fallback_identifier() -> String {
    sentry::capture_message(
        "Device hardware UUID unavailable — falling back to hostname-based identifier",
        sentry::Level::Warning,
    );
    let hostname = Command::new("/bin/hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| "unknown-host".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "unknown-home".to_string());
    format!("fallback:{hostname}:{home}")
}

fn describe_os() -> String {
    let info = os_info::get();
    format!(
        "{} {} {}",
        info.os_type(),
        info.version(),
        std::env::consts::ARCH
    )
}

fn read_chopratejas_instance_id() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let headroom_dir = PathBuf::from(home).join(".headroom");
    if !headroom_dir.exists() {
        return None;
    }
    // chopratejas/headroom stores its storage root under ~/.headroom. Their
    // instance id is sha256(storage_path)[:16] when present, else
    // sha256(hostname:uid)[:16]. We mirror that exactly so both tools land on
    // the same value.
    let path_str = headroom_dir.to_string_lossy().into_owned();
    Some(truncate_hex(&sha256_hex(&path_str), 16))
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex_encode(&hasher.finalize())
}

fn truncate_hex(digest: &str, len: usize) -> String {
    digest.chars().take(len).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex("hello"), sha256_hex("hello"));
        assert_ne!(sha256_hex("hello"), sha256_hex("world"));
        assert_eq!(sha256_hex("").len(), 64);
    }

    #[test]
    fn truncate_hex_caps_length() {
        assert_eq!(truncate_hex(&sha256_hex("x"), 16).len(), 16);
    }

    #[test]
    fn chopratejas_instance_id_is_none_when_home_missing() {
        let previous = std::env::var_os("HOME");
        std::env::remove_var("HOME");
        let result = read_chopratejas_instance_id();
        if let Some(value) = previous {
            std::env::set_var("HOME", value);
        }
        assert!(result.is_none());
    }
}
