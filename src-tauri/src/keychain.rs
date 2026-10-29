// Debug builds store secrets in plain files under the app data dir so that the
// keychain is never touched and macOS never shows an access prompt during development.
// Release builds use the legacy login keychain via the Security framework with
// default ACL/accessibility (kSecClass/kSecAttrService/kSecAttrAccount only —
// no kSecUseDataProtectionKeychain, which would additionally require the
// keychain-access-groups entitlement that Entitlements.plist does not carry).

// ── File-backed store ────────────────────────────────────────────────────────
// Used by every debug build, and as the release fallback on Windows/Linux when
// the OS credential store is unusable (see the keyring module below).

#[cfg(any(debug_assertions, target_os = "windows", target_os = "linux"))]
mod file_store {
    use std::path::{Path, PathBuf};

    fn secret_path(service: &str, account: &str) -> PathBuf {
        crate::storage::app_data_dir()
            .join("config")
            .join("secrets")
            .join(service)
            .join(account)
    }

    // Best effort: the dir goes 0700 before the file is written, so the brief
    // window where the tmp file still carries the umask mode is not readable
    // by other users anyway.
    #[cfg(unix)]
    fn restrict(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    #[cfg(not(unix))]
    fn restrict(_path: &Path, _mode: u32) {}

    pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
        let path = secret_path(service, account);
        match std::fs::read_to_string(&path) {
            Ok(secret) => Ok(Some(secret)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("Failed to read secret {}: {err}", path.display())),
        }
    }

    pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
        let path = secret_path(service, account);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("Failed to create secrets dir: {err}"))?;
            restrict(parent, 0o700);
        }
        crate::client_adapters::atomic_write(&path, secret.as_bytes())
            .map_err(|err| format!("Failed to write secret {}: {err}", path.display()))?;
        restrict(&path, 0o600);
        Ok(())
    }

    pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
        let path = secret_path(service, account);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("Failed to delete secret {}: {err}", path.display())),
        }
    }
}

#[cfg(debug_assertions)]
use file_store as platform;

// ── Release / macOS: login keychain (Security framework) ────────────────────

#[cfg(all(not(debug_assertions), target_os = "macos"))]
mod platform {
    use std::ffi::c_void;
    use std::os::raw::c_long;

    type OSStatus = i32;
    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFDataRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFIndex = c_long;

    const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;
    const ERR_SEC_DUPLICATE_ITEM: OSStatus = -25299;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

    #[repr(C)]
    struct CFDictionaryCallBacks([u8; 0]);

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: CFTypeRef;
        static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
        static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithBytes(
            alloc: *const c_void,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: u32,
            is_external: u8,
        ) -> CFStringRef;
        fn CFDataCreate(alloc: *const c_void, bytes: *const u8, length: CFIndex) -> CFDataRef;
        fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
        fn CFDataGetLength(data: CFDataRef) -> CFIndex;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            num_values: CFIndex,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: CFStringRef;
        static kSecClassGenericPassword: CFStringRef;
        static kSecAttrService: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecMatchLimit: CFStringRef;
        static kSecMatchLimitOne: CFStringRef;
        fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemUpdate(query: CFDictionaryRef, attrs_to_update: CFDictionaryRef) -> OSStatus;
        fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
    }

    unsafe fn cf_string(s: &str) -> CFStringRef {
        CFStringCreateWithBytes(
            std::ptr::null(),
            s.as_ptr(),
            s.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    }

    unsafe fn callbacks_key() -> *const c_void {
        &kCFTypeDictionaryKeyCallBacks as *const CFDictionaryCallBacks as *const c_void
    }

    unsafe fn callbacks_val() -> *const c_void {
        &kCFTypeDictionaryValueCallBacks as *const CFDictionaryCallBacks as *const c_void
    }

    // Base lookup dict for the standard macOS keychain; caller must CFRelease.
    unsafe fn base_query(service: &str, account: &str) -> CFDictionaryRef {
        let svc = cf_string(service);
        let acc = cf_string(account);
        let keys: [CFTypeRef; 3] = [kSecClass, kSecAttrService, kSecAttrAccount];
        let values: [CFTypeRef; 3] = [kSecClassGenericPassword, svc, acc];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            3,
            callbacks_key(),
            callbacks_val(),
        );
        CFRelease(svc);
        CFRelease(acc);
        dict
    }

    pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
        unsafe {
            let svc = cf_string(service);
            let acc = cf_string(account);
            let keys: [CFTypeRef; 5] = [
                kSecClass,
                kSecAttrService,
                kSecAttrAccount,
                kSecReturnData,
                kSecMatchLimit,
            ];
            let values: [CFTypeRef; 5] = [
                kSecClassGenericPassword,
                svc,
                acc,
                kCFBooleanTrue,
                kSecMatchLimitOne,
            ];
            let query = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                5,
                callbacks_key(),
                callbacks_val(),
            );
            CFRelease(svc);
            CFRelease(acc);

            let mut result: CFTypeRef = std::ptr::null();
            let status = SecItemCopyMatching(query, &mut result);
            CFRelease(query);

            if status == ERR_SEC_ITEM_NOT_FOUND {
                return Ok(None);
            }
            check_status(status, "read keychain secret")?;

            let data: CFDataRef = result;
            let len = CFDataGetLength(data) as usize;
            let ptr = CFDataGetBytePtr(data);
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            CFRelease(result);

            String::from_utf8(bytes)
                .map(Some)
                .map_err(|err| format!("Keychain secret for {account} was not valid UTF-8: {err}"))
        }
    }

    pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
        write_secret_inner(service, account, secret, true)
    }

    fn write_secret_inner(
        service: &str,
        account: &str,
        secret: &str,
        allow_retry: bool,
    ) -> Result<(), String> {
        unsafe {
            let query = base_query(service, account);
            let data = CFDataCreate(std::ptr::null(), secret.as_ptr(), secret.len() as CFIndex);
            let attr_keys: [CFTypeRef; 1] = [kSecValueData];
            let attr_vals: [CFTypeRef; 1] = [data];
            let attrs = CFDictionaryCreate(
                std::ptr::null(),
                attr_keys.as_ptr(),
                attr_vals.as_ptr(),
                1,
                callbacks_key(),
                callbacks_val(),
            );
            let status = SecItemUpdate(query, attrs);
            CFRelease(attrs);
            CFRelease(data);
            CFRelease(query);

            if status != ERR_SEC_ITEM_NOT_FOUND {
                return check_status(status, "update keychain secret");
            }

            let svc = cf_string(service);
            let acc = cf_string(account);
            let data = CFDataCreate(std::ptr::null(), secret.as_ptr(), secret.len() as CFIndex);
            let keys: [CFTypeRef; 4] = [kSecClass, kSecAttrService, kSecAttrAccount, kSecValueData];
            let values: [CFTypeRef; 4] = [kSecClassGenericPassword, svc, acc, data];
            let add_dict = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                4,
                callbacks_key(),
                callbacks_val(),
            );
            let add_status = SecItemAdd(add_dict, std::ptr::null_mut());
            CFRelease(add_dict);
            CFRelease(data);
            CFRelease(svc);
            CFRelease(acc);
            if add_status == ERR_SEC_DUPLICATE_ITEM {
                // Update missed it (inaccessible/ghost item, e.g. created by a
                // prior app signature) but Add sees the primary-key collision.
                // Drop the stale item and re-add ONCE: delete_secret maps
                // not-found to Ok, so a truly undeletable ghost (iCloud sync
                // residue, cross-signature ACL) would loop Update->notFound,
                // Add->duplicate, Delete->notFound forever if we recursed
                // unboundedly.
                if allow_retry {
                    let _ = delete_secret(service, account);
                    return write_secret_inner(service, account, secret, false);
                }
                return Err(
                    "write keychain secret failed: duplicate item persists after delete \
                     (inaccessible keychain entry from another app signature?)"
                        .to_string(),
                );
            }
            check_status(add_status, "write keychain secret")
        }
    }

    pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
        unsafe {
            let query = base_query(service, account);
            let status = SecItemDelete(query);
            CFRelease(query);
            if status == ERR_SEC_ITEM_NOT_FOUND {
                return Ok(());
            }
            check_status(status, "delete keychain secret")
        }
    }

    fn check_status(status: OSStatus, action: &str) -> Result<(), String> {
        if status == 0 {
            return Ok(());
        }
        // The common failure codes on managed (MDM) or locked-down machines
        // get a hint the user can act on — a bare "status -25308" was the
        // whole error message people saw at sign-in.
        let hint = match status {
            // errSecInteractionNotAllowed: keychain locked or UI not allowed.
            -25308 => " Your macOS keychain seems to be locked. Unlock it in Keychain Access (or log out and back in) and retry.",
            // errSecAuthFailed
            -25293 => " macOS denied keychain access for Headroom. If this Mac is company-managed, your MDM profile may restrict keychain use.",
            // errSecMissingEntitlement
            -34018 => " This build is missing a keychain entitlement. Reinstalling Headroom usually fixes this.",
            _ => "",
        };
        Err(format!(
            "{action} failed with macOS Security status {status}.{hint}"
        ))
    }
}

// ── Release / Windows + Linux: OS credential store (keyring crate) ──────────
// Windows -> Credential Manager (windows-native); Linux -> DBus Secret
// Service, i.e. gnome-keyring/kwallet (sync-secret-service). Either store can
// be missing or unusable on a given host, so both fall back to file_store.

#[cfg(all(not(debug_assertions), any(target_os = "windows", target_os = "linux")))]
mod platform {
    use super::file_store;
    use keyring::Entry;

    // The Secret Service can be reachable and still have nowhere to put a
    // secret: keyring resolves everything through the `default` collection
    // alias, and on a box where that alias was never created (headless VM,
    // XFCE/xrdp session with no login keyring) both its lookup and its create
    // path call get_default_collection() and fail with "no result found". That
    // is not recoverable from our side, and it blocked sign-in entirely, so
    // fall back to a 0600 file rather than refuse to store the token.
    fn warn_fallback(err: &str) {
        // Once per process: read_secret runs on every pricing poll, so warning
        // per call would flood Sentry the way the grace/start retries did.
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::warn!(
                "OS credential store unusable ({err}); storing Headroom secrets \
                 in a 0600 file under the app data dir instead"
            );
        });
    }

    pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
        match Entry::new(service, account).and_then(|entry| entry.get_password()) {
            Ok(secret) => Ok(Some(secret)),
            // NoEntry is not a store failure, but the secret may still be in
            // the fallback file from a run where the store was unusable.
            Err(keyring::Error::NoEntry) => file_store::read_secret(service, account),
            Err(err) => {
                warn_fallback(&err.to_string());
                file_store::read_secret(service, account)
            }
        }
    }

    pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
        match Entry::new(service, account).and_then(|entry| entry.set_password(secret)) {
            Ok(()) => {
                // Don't leave a stale plaintext copy shadowed by the store.
                let _ = file_store::delete_secret(service, account);
                Ok(())
            }
            Err(err) => {
                warn_fallback(&err.to_string());
                file_store::write_secret(service, account, secret)?;
                // read_secret asks the store first, so an older secret left
                // there (one that fit before this one hit Windows' TooLong
                // limit, say) would shadow the new file forever. File first:
                // a failed file write keeps the old secret instead of losing
                // both.
                if let Ok(entry) = Entry::new(service, account) {
                    let _ = entry.delete_credential();
                }
                Ok(())
            }
        }
    }

    pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
        // Both stores: sign-out must not leave a token behind in whichever one
        // happened to be writable when it was saved.
        if let Ok(entry) = Entry::new(service, account) {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(err) => warn_fallback(&err.to_string()),
            }
        }
        file_store::delete_secret(service, account)
    }
}

// ── Release / other platforms: stub ───────────────────────────────────────────

#[cfg(all(
    not(debug_assertions),
    not(target_os = "macos"),
    not(target_os = "windows"),
    not(target_os = "linux")
))]
mod platform {
    pub fn read_secret(_service: &str, _account: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn write_secret(_service: &str, _account: &str, _secret: &str) -> Result<(), String> {
        Err("Secure key storage is not implemented for this platform build.".into())
    }

    pub fn delete_secret(_service: &str, _account: &str) -> Result<(), String> {
        Ok(())
    }
}

// ── Public interface ──────────────────────────────────────────────────────────

pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
    platform::read_secret(service, account)
}

pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
    platform::write_secret(service, account, secret)
}

pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
    platform::delete_secret(service, account)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// Snapshot HOME / XDG_DATA_HOME, point them at a fresh tempdir, and
    /// restore on drop. Mirrors the helper in client_adapters' tests so debug
    /// keychain reads/writes don't bleed into the developer's real profile.
    struct TestHome {
        _tmp: tempfile::TempDir,
        prev_home: Option<OsString>,
        prev_xdg: Option<OsString>,
        // Held for the guard's lifetime: see test_env_lock::lock_home.
        _env_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestHome {
        fn new() -> Self {
            let env_lock = crate::test_env_lock::lock_home();
            let tmp = tempfile::tempdir().expect("create temp home");
            let home: PathBuf = tmp.path().to_path_buf();
            let prev_home = std::env::var_os("HOME");
            let prev_xdg = std::env::var_os("XDG_DATA_HOME");
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_DATA_HOME", home.join(".local").join("share"));
            crate::storage::ensure_data_dirs(&crate::storage::app_data_dir())
                .expect("ensure_data_dirs in test home");
            TestHome {
                _tmp: tmp,
                prev_home,
                prev_xdg,
                _env_lock: env_lock,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_xdg.take() {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn write_then_read_round_trips_value() {
        let _home = TestHome::new();
        super::write_secret("test-svc", "acct-a", "s3cret").expect("write");
        let value = super::read_secret("test-svc", "acct-a").expect("read");
        assert_eq!(value.as_deref(), Some("s3cret"));
    }

    #[test]
    #[serial_test::serial]
    fn read_returns_none_when_missing() {
        let _home = TestHome::new();
        let value = super::read_secret("test-svc", "never-written").expect("read");
        assert!(value.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn delete_removes_secret() {
        let _home = TestHome::new();
        super::write_secret("test-svc", "acct-b", "to-be-deleted").expect("write");
        super::delete_secret("test-svc", "acct-b").expect("delete");
        let value = super::read_secret("test-svc", "acct-b").expect("read after delete");
        assert!(value.is_none(), "deleted secret should be gone");
    }

    #[test]
    #[serial_test::serial]
    fn delete_is_idempotent_when_secret_missing() {
        let _home = TestHome::new();
        super::delete_secret("test-svc", "never-existed").expect("delete on missing");
    }

    #[test]
    #[serial_test::serial]
    fn overwrite_replaces_existing_secret() {
        let _home = TestHome::new();
        super::write_secret("test-svc", "acct-c", "first").expect("first write");
        super::write_secret("test-svc", "acct-c", "second").expect("second write");
        let value = super::read_secret("test-svc", "acct-c").expect("read");
        assert_eq!(value.as_deref(), Some("second"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secret_interface_round_trips_via_debug_store() {
        // Debug builds use the file store on every platform; this guards the
        // public interface + debug-store path on Windows. The keyring-backed
        // release path is smoke-tested on a real Windows host (Phase 6).
        let service = format!("headroom-test-{}", uuid::Uuid::new_v4());
        super::write_secret(&service, "account", "value").expect("write should succeed");
        assert_eq!(
            super::read_secret(&service, "account")
                .expect("read should succeed")
                .as_deref(),
            Some("value")
        );
        super::delete_secret(&service, "account").expect("delete should succeed");
        assert_eq!(
            super::read_secret(&service, "account").expect("read should succeed"),
            None
        );
    }
}
