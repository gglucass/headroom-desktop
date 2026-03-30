#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_uint};
    use std::ptr;

    type OSStatus = i32;
    type SecKeychainItemRef = *mut c_void;

    const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainAddGenericPassword(
            keychain: *const c_void,
            service_name_length: c_uint,
            service_name: *const c_char,
            account_name_length: c_uint,
            account_name: *const c_char,
            password_length: c_uint,
            password_data: *const c_void,
            item_ref: *mut SecKeychainItemRef,
        ) -> OSStatus;
        fn SecKeychainFindGenericPassword(
            keychain_or_array: *const c_void,
            service_name_length: c_uint,
            service_name: *const c_char,
            account_name_length: c_uint,
            account_name: *const c_char,
            password_length: *mut c_uint,
            password_data: *mut *mut c_void,
            item_ref: *mut SecKeychainItemRef,
        ) -> OSStatus;
        fn SecKeychainItemModifyAttributesAndData(
            item_ref: SecKeychainItemRef,
            attr_list: *const c_void,
            length: c_uint,
            data: *const c_void,
        ) -> OSStatus;
        fn SecKeychainItemDelete(item_ref: SecKeychainItemRef) -> OSStatus;
        fn SecKeychainItemFreeContent(attr_list: *mut c_void, data: *mut c_void) -> OSStatus;
    }

    pub fn read_secret(service: &str, account: &str) -> Result<Option<String>, String> {
        let mut password_length = 0;
        let mut password_data = ptr::null_mut();
        let mut item_ref = ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null(),
                service.len() as c_uint,
                service.as_ptr().cast(),
                account.len() as c_uint,
                account.as_ptr().cast(),
                &mut password_length,
                &mut password_data,
                &mut item_ref,
            )
        };
        if status == ERR_SEC_ITEM_NOT_FOUND {
            return Ok(None);
        }
        check_status(status, "read keychain secret")?;

        let bytes = unsafe {
            std::slice::from_raw_parts(password_data.cast::<u8>(), password_length as usize)
        }
        .to_vec();

        unsafe {
            let _ = SecKeychainItemFreeContent(ptr::null_mut(), password_data);
            if !item_ref.is_null() {
                CFRelease(item_ref.cast());
            }
        }

        String::from_utf8(bytes)
            .map(Some)
            .map_err(|err| format!("Keychain secret for {account} was not valid UTF-8: {err}"))
    }

    pub fn write_secret(service: &str, account: &str, secret: &str) -> Result<(), String> {
        if let Some(item_ref) = find_item(service, account)? {
            let status = unsafe {
                SecKeychainItemModifyAttributesAndData(
                    item_ref,
                    ptr::null(),
                    secret.len() as c_uint,
                    secret.as_ptr().cast(),
                )
            };
            unsafe {
                CFRelease(item_ref.cast());
            }
            check_status(status, "update keychain secret")
        } else {
            let mut item_ref = ptr::null_mut();
            let status = unsafe {
                SecKeychainAddGenericPassword(
                    ptr::null(),
                    service.len() as c_uint,
                    service.as_ptr().cast(),
                    account.len() as c_uint,
                    account.as_ptr().cast(),
                    secret.len() as c_uint,
                    secret.as_ptr().cast(),
                    &mut item_ref,
                )
            };
            unsafe {
                if !item_ref.is_null() {
                    CFRelease(item_ref.cast());
                }
            }
            check_status(status, "write keychain secret")
        }
    }

    pub fn has_secret(service: &str, account: &str) -> Result<bool, String> {
        let Some(item_ref) = find_item(service, account)? else {
            return Ok(false);
        };
        unsafe {
            CFRelease(item_ref.cast());
        }
        Ok(true)
    }

    pub fn delete_secret(service: &str, account: &str) -> Result<(), String> {
        let Some(item_ref) = find_item(service, account)? else {
            return Ok(());
        };
        let status = unsafe { SecKeychainItemDelete(item_ref) };
        unsafe {
            CFRelease(item_ref.cast());
        }
        check_status(status, "delete keychain secret")
    }

    fn find_item(service: &str, account: &str) -> Result<Option<SecKeychainItemRef>, String> {
        let mut item_ref = ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null(),
                service.len() as c_uint,
                service.as_ptr().cast(),
                account.len() as c_uint,
                account.as_ptr().cast(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut item_ref,
            )
        };
        if status == ERR_SEC_ITEM_NOT_FOUND {
            return Ok(None);
        }
        check_status(status, "find keychain secret")?;
        Ok(Some(item_ref))
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
