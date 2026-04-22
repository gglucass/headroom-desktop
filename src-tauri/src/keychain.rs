// Debug builds store secrets in plain files under the app data dir so that the
// keychain is never touched and macOS never shows an access prompt during development.
// Release builds use the macOS Data Protection Keychain (requires the keychain-access-groups
// entitlement present in Entitlements.plist).

// ── Debug: file-based store ──────────────────────────────────────────────────

#[cfg(debug_assertions)]
mod platform {
    use std::path::PathBuf;

    fn secret_path(service: &str, account: &str) -> PathBuf {
        crate::storage::app_data_dir()
            .join("config")
            .join("dev-secrets")
            .join(service)
            .join(account)
    }

    pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
        let path = secret_path(service, account);
        if !path.exists() {
            return Ok(None);
        }
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|err| format!("Failed to read dev secret {}: {err}", path.display()))
    }

    pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
        let path = secret_path(service, account);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("Failed to create dev-secrets dir: {err}"))?;
        }
        std::fs::write(&path, secret)
            .map_err(|err| format!("Failed to write dev secret {}: {err}", path.display()))
    }

    pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
        let path = secret_path(service, account);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|err| format!("Failed to delete dev secret {}: {err}", path.display()))?;
        }
        Ok(())
    }
}

// ── Release / macOS: Data Protection Keychain ────────────────────────────────

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
            Ok(())
        } else {
            Err(format!(
                "{action} failed with macOS Security status {status}."
            ))
        }
    }
}

// ── Release / non-macOS: stub ─────────────────────────────────────────────────

#[cfg(all(not(debug_assertions), not(target_os = "macos")))]
mod platform {
    pub fn read_secret(_service: &str, _account: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn write_secret(_service: &str, _account: &str, _secret: &str) -> Result<(), String> {
        Err("Secure key storage is currently only implemented for macOS builds.".into())
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
