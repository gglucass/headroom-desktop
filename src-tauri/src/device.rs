#[cfg(windows)]
use std::path::PathBuf;

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
        // Best-effort cache only: `digest` is deterministic (sha256 of the
        // hardware UUID, or a persisted fallback file), so the next launch
        // recomputes the SAME value whether or not this write lands. The
        // dominant failure is "duplicate item persists after delete" — a
        // keychain entry owned by a different app signature that we can't
        // read or delete but Add still collides with; that's the machine's
        // environment, unfixable from here and identical every launch. Log
        // locally instead of firing a fleet Sentry warning on every boot.
        log::warn!("Could not persist machine id digest (non-fatal, using computed value): {err}");
    }
    digest
}

#[cfg(target_os = "macos")]
fn read_hardware_uuid() -> Option<String> {
    let output = crate::proc::command("/usr/sbin/ioreg")
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

#[cfg(target_os = "windows")]
fn read_hardware_uuid() -> Option<String> {
    // Absolute path, mirroring /usr/sbin/ioreg on macOS: a `reg` shim earlier
    // in PATH would silently change every device id in the fleet.
    let reg = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("reg.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("reg"));
    let output = crate::proc::command(reg)
        .args([
            "query",
            "HKLM\\SOFTWARE\\Microsoft\\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("MachineGuid") {
            continue;
        }
        // Sample line:
        //   MachineGuid    REG_SZ    cbb05f42-c573-4037-b9c2-4a1a8b0e9a1b
        if let Some(guid) = line.split_whitespace().last() {
            let trimmed = guid.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn read_hardware_uuid() -> Option<String> {
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn fallback_identifier() -> String {
    // A hostname-derived id churns whenever DHCP renames the machine,
    // fragmenting the server-side trial record across "devices". Persist a
    // random id in app support instead; hostname remains only as the last
    // resort when even that write fails.
    let path = crate::storage::config_file(&crate::storage::app_data_dir(), "device-fallback-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let fresh = format!("fallback:{}", uuid::Uuid::new_v4());
    if crate::client_adapters::atomic_write(&path, fresh.as_bytes()).is_ok() {
        sentry::capture_message(
            "Device hardware UUID unavailable — using persisted random fallback id",
            sentry::Level::Warning,
        );
        return fresh;
    }
    sentry::capture_message(
        "Device hardware UUID unavailable — falling back to hostname-based identifier",
        sentry::Level::Warning,
    );
    let hostname = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| {
            crate::proc::command("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
        })
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
    // Their Python storage root is `Path.home()`: the profile folder on
    // Windows, where `HOME` is usually unset and this used to bail.
    let headroom_dir = crate::client_adapters::home_dir().join(".headroom");
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
    fn chopratejas_instance_id_is_none_without_a_headroom_dir() {
        // An empty home, not an unset one: with HOME unset the resolver falls
        // back to the real profile, which on a dev machine has a ~/.headroom.
        let _home_lock = crate::test_env_lock::lock_home();
        let home = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let result = read_chopratejas_instance_id();
        if let Some(value) = previous {
            std::env::set_var("HOME", value);
        }
        assert!(result.is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_machine_guid_is_readable() {
        let uuid = read_hardware_uuid();
        assert!(uuid.is_some(), "MachineGuid should exist on Windows");
        assert_eq!(uuid.as_deref().unwrap().len(), 36);
    }
}
