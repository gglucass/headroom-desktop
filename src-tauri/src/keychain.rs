#[cfg(target_os = "macos")]
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
    const ERR_SEC_MISSING_ENTITLEMENT: OSStatus = -34018;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

    // Opaque type for taking addresses of CoreFoundation callback structs.
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
        static kSecUseDataProtectionKeychain: CFStringRef;
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

    // Builds a base lookup dict for a service+account item.
    // When use_data_protection is true, includes kSecUseDataProtectionKeychain (requires entitlement).
    // The returned CFDictionaryRef retains the strings; caller must CFRelease the dict.
    unsafe fn base_query(service: &str, account: &str, use_data_protection: bool) -> CFDictionaryRef {
        let svc = cf_string(service);
        let acc = cf_string(account);
        let dict = if use_data_protection {
            let keys: [CFTypeRef; 4] = [kSecClass, kSecAttrService, kSecAttrAccount, kSecUseDataProtectionKeychain];
            let values: [CFTypeRef; 4] = [kSecClassGenericPassword, svc, acc, kCFBooleanTrue];
            CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 4, callbacks_key(), callbacks_val())
        } else {
            let keys: [CFTypeRef; 3] = [kSecClass, kSecAttrService, kSecAttrAccount];
            let values: [CFTypeRef; 3] = [kSecClassGenericPassword, svc, acc];
            CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 3, callbacks_key(), callbacks_val())
        };
        CFRelease(svc);
        CFRelease(acc);
        dict
    }

    unsafe fn read_secret_inner(service: &str, account: &str, use_data_protection: bool) -> Result<Option<String>, OSStatus> {
        let svc = cf_string(service);
        let acc = cf_string(account);
        let query = if use_data_protection {
            let keys: [CFTypeRef; 6] = [kSecClass, kSecAttrService, kSecAttrAccount, kSecUseDataProtectionKeychain, kSecReturnData, kSecMatchLimit];
            let values: [CFTypeRef; 6] = [kSecClassGenericPassword, svc, acc, kCFBooleanTrue, kCFBooleanTrue, kSecMatchLimitOne];
            CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 6, callbacks_key(), callbacks_val())
        } else {
            let keys: [CFTypeRef; 5] = [kSecClass, kSecAttrService, kSecAttrAccount, kSecReturnData, kSecMatchLimit];
            let values: [CFTypeRef; 5] = [kSecClassGenericPassword, svc, acc, kCFBooleanTrue, kSecMatchLimitOne];
            CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 5, callbacks_key(), callbacks_val())
        };
        CFRelease(svc);
        CFRelease(acc);

        let mut result: CFTypeRef = std::ptr::null();
        let status = SecItemCopyMatching(query, &mut result);
        CFRelease(query);

        if status == ERR_SEC_ITEM_NOT_FOUND {
            return Ok(None);
        }
        if status != 0 {
            return Err(status);
        }

        let data: CFDataRef = result;
        let len = CFDataGetLength(data) as usize;
        let ptr = CFDataGetBytePtr(data);
        let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
        CFRelease(result);

        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| -50) // paramErr
    }

    pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
        unsafe {
            match read_secret_inner(service, account, true) {
                Err(ERR_SEC_MISSING_ENTITLEMENT) => read_secret_inner(service, account, false)
                    .map_err(|s| format!("read keychain secret failed with macOS Security status {s}.")),
                other => other.map_err(|s| format!("read keychain secret failed with macOS Security status {s}.")),
            }
        }
    }

    unsafe fn write_secret_inner(service: &str, account: &str, secret: &str, use_data_protection: bool) -> OSStatus {
        // Try update first.
        let query = base_query(service, account, use_data_protection);
        let data = CFDataCreate(std::ptr::null(), secret.as_ptr(), secret.len() as CFIndex);
        let attr_keys: [CFTypeRef; 1] = [kSecValueData];
        let attr_vals: [CFTypeRef; 1] = [data];
        let attrs = CFDictionaryCreate(std::ptr::null(), attr_keys.as_ptr(), attr_vals.as_ptr(), 1, callbacks_key(), callbacks_val());
        let status = SecItemUpdate(query, attrs);
        CFRelease(attrs);
        CFRelease(data);
        CFRelease(query);

        if status != ERR_SEC_ITEM_NOT_FOUND {
            return status;
        }

        // Item doesn't exist yet — add it.
        let svc = cf_string(service);
        let acc = cf_string(account);
        let data = CFDataCreate(std::ptr::null(), secret.as_ptr(), secret.len() as CFIndex);
        let add_status = if use_data_protection {
            let keys: [CFTypeRef; 5] = [kSecClass, kSecAttrService, kSecAttrAccount, kSecUseDataProtectionKeychain, kSecValueData];
            let values: [CFTypeRef; 5] = [kSecClassGenericPassword, svc, acc, kCFBooleanTrue, data];
            let add_dict = CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 5, callbacks_key(), callbacks_val());
            let s = SecItemAdd(add_dict, std::ptr::null_mut());
            CFRelease(add_dict);
            s
        } else {
            let keys: [CFTypeRef; 4] = [kSecClass, kSecAttrService, kSecAttrAccount, kSecValueData];
            let values: [CFTypeRef; 4] = [kSecClassGenericPassword, svc, acc, data];
            let add_dict = CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), 4, callbacks_key(), callbacks_val());
            let s = SecItemAdd(add_dict, std::ptr::null_mut());
            CFRelease(add_dict);
            s
        };
        CFRelease(data);
        CFRelease(svc);
        CFRelease(acc);
        add_status
    }

    pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
        unsafe {
            let status = write_secret_inner(service, account, secret, true);
            if status == ERR_SEC_MISSING_ENTITLEMENT {
                let fallback = write_secret_inner(service, account, secret, false);
                return check_status(fallback, "write keychain secret");
            }
            check_status(status, "write keychain secret")
        }
    }

    pub fn has_secret(service: &str, account: &str) -> Result<bool, String> {
        read_secret(service, account).map(|opt| opt.is_some())
    }

    unsafe fn delete_secret_inner(service: &str, account: &str, use_data_protection: bool) -> OSStatus {
        let query = base_query(service, account, use_data_protection);
        let status = SecItemDelete(query);
        CFRelease(query);
        status
    }

    pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
        unsafe {
            let status = delete_secret_inner(service, account, true);
            if status == ERR_SEC_MISSING_ENTITLEMENT {
                let fallback = delete_secret_inner(service, account, false);
                if fallback == ERR_SEC_ITEM_NOT_FOUND {
                    return Ok(());
                }
                return check_status(fallback, "delete keychain secret");
            }
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

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn read_secret(_service: &str, _account: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn has_secret(_service: &str, _account: &str) -> Result<bool, String> {
        Ok(false)
    }

    pub fn write_secret(_service: &str, _account: &str, _secret: &str) -> Result<(), String> {
        Err("Secure key storage is currently only implemented for macOS builds.".into())
    }

    pub fn delete_secret(_service: &str, _account: &str) -> Result<(), String> {
        Ok(())
    }
}

pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
    platform::read_secret(service, account)
}

pub fn has_secret(service: &str, account: &str) -> Result<bool, String> {
    platform::has_secret(service, account)
}

pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
    platform::write_secret(service, account, secret)
}

pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
    platform::delete_secret(service, account)
}
