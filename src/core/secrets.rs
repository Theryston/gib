#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

pub(crate) fn password_reference(job_id: &str) -> String {
    format!("gib/autostart/{}/password", job_id)
}

pub(crate) fn store_password(reference: &str, password: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let mut child = Command::new("secret-tool")
            .args([
                "store",
                "--label",
                "GIB autostart password",
                "service",
                "gib-autostart",
                "reference",
                reference,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Failed to start secret-tool: {}", error))?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(password.as_bytes()).map_err(|error| {
                format!("Failed to provide the password to secret-tool: {}", error)
            })?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("Failed to store the password in the keyring: {}", error))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "secret-tool failed to store the password: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    #[cfg(target_os = "macos")]
    {
        macos::store(reference, password)
    }

    #[cfg(windows)]
    {
        windows::store(reference, password)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (reference, password);
        Err("No supported platform credential store is available".to_string())
    }
}

pub(crate) fn read_password(reference: &str) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        let output = Command::new("secret-tool")
            .args(["lookup", "service", "gib-autostart", "reference", reference])
            .output()
            .map_err(|error| format!("Failed to start secret-tool: {}", error))?;
        if output.status.success() {
            let password = String::from_utf8(output.stdout)
                .map_err(|_| "The keyring returned a non-text password".to_string())?;
            return Ok(password.trim_end_matches(['\r', '\n']).to_string());
        }
        return Err(format!(
            "The password reference '{}' is unavailable in the user keyring",
            reference
        ));
    }

    #[cfg(target_os = "macos")]
    {
        macos::read(reference)
    }

    #[cfg(windows)]
    {
        windows::read(reference)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = reference;
        Err("No supported platform credential store is available".to_string())
    }
}

pub(crate) fn delete_password(reference: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let output = Command::new("secret-tool")
            .args(["clear", "service", "gib-autostart", "reference", reference])
            .output()
            .map_err(|error| format!("Failed to start secret-tool: {}", error))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "secret-tool failed to remove the password: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    #[cfg(target_os = "macos")]
    {
        macos::delete(reference)
    }

    #[cfg(windows)]
    {
        windows::delete(reference)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = reference;
        Err("No supported platform credential store is available".to_string())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::ptr;

    type OsStatus = i32;
    type KeychainItemRef = *mut c_void;

    const ERR_SEC_ITEM_NOT_FOUND: OsStatus = -25300;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainFindGenericPassword(
            keychain: *mut c_void,
            service_name_length: u32,
            service_name: *const c_void,
            account_name_length: u32,
            account_name: *const c_void,
            password_length: *mut u32,
            password_data: *mut *mut c_void,
            item_ref: *mut KeychainItemRef,
        ) -> OsStatus;
        fn SecKeychainAddGenericPassword(
            keychain: *mut c_void,
            service_name_length: u32,
            service_name: *const c_void,
            account_name_length: u32,
            account_name: *const c_void,
            password_length: u32,
            password_data: *const c_void,
            item_ref: *mut KeychainItemRef,
        ) -> OsStatus;
        fn SecKeychainItemModifyAttributesAndData(
            item_ref: KeychainItemRef,
            attr_list: *mut c_void,
            data_length: u32,
            data: *const c_void,
        ) -> OsStatus;
        fn SecKeychainItemFreeContent(attr_list: *mut c_void, data: *mut c_void) -> OsStatus;
        fn SecKeychainItemDelete(item_ref: KeychainItemRef) -> OsStatus;
    }

    fn failure(action: &str, status: OsStatus) -> String {
        format!("macOS Keychain failed to {action} (status {status})")
    }

    pub(super) fn store(reference: &str, password: &str) -> Result<(), String> {
        let service = reference.as_bytes();
        let account = b"gib";
        let mut password_length = 0;
        let mut password_data = ptr::null_mut();
        let mut item_ref = ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null_mut(),
                service.len() as u32,
                service.as_ptr() as *const c_void,
                account.len() as u32,
                account.as_ptr() as *const c_void,
                &mut password_length,
                &mut password_data,
                &mut item_ref,
            )
        };

        if status == 0 {
            let update_status = unsafe {
                SecKeychainItemModifyAttributesAndData(
                    item_ref,
                    ptr::null_mut(),
                    password.len() as u32,
                    password.as_ptr() as *const c_void,
                )
            };
            if !password_data.is_null() {
                unsafe {
                    SecKeychainItemFreeContent(ptr::null_mut(), password_data);
                }
            }
            if update_status == 0 {
                return Ok(());
            }
            return Err(failure("update the password", update_status));
        }

        if status != ERR_SEC_ITEM_NOT_FOUND {
            return Err(failure("find the password", status));
        }

        let add_status = unsafe {
            SecKeychainAddGenericPassword(
                ptr::null_mut(),
                service.len() as u32,
                service.as_ptr() as *const c_void,
                account.len() as u32,
                account.as_ptr() as *const c_void,
                password.len() as u32,
                password.as_ptr() as *const c_void,
                ptr::null_mut(),
            )
        };
        if add_status == 0 {
            Ok(())
        } else {
            Err(failure("store the password", add_status))
        }
    }

    pub(super) fn read(reference: &str) -> Result<String, String> {
        let service = reference.as_bytes();
        let account = b"gib";
        let mut password_length = 0;
        let mut password_data = ptr::null_mut();
        let mut item_ref = ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null_mut(),
                service.len() as u32,
                service.as_ptr() as *const c_void,
                account.len() as u32,
                account.as_ptr() as *const c_void,
                &mut password_length,
                &mut password_data,
                &mut item_ref,
            )
        };
        if status != 0 {
            return Err(format!(
                "The password reference '{}' is unavailable in macOS Keychain",
                reference
            ));
        }

        let result = unsafe {
            let bytes =
                std::slice::from_raw_parts(password_data as *const u8, password_length as usize);
            String::from_utf8(bytes.to_vec())
                .map_err(|_| "The Keychain returned a non-text password".to_string())
        };
        if !password_data.is_null() {
            unsafe {
                SecKeychainItemFreeContent(ptr::null_mut(), password_data);
            }
        }
        result
    }

    pub(super) fn delete(reference: &str) -> Result<(), String> {
        let service = reference.as_bytes();
        let account = b"gib";
        let mut password_length = 0;
        let mut password_data = ptr::null_mut();
        let mut item_ref = ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null_mut(),
                service.len() as u32,
                service.as_ptr() as *const c_void,
                account.len() as u32,
                account.as_ptr() as *const c_void,
                &mut password_length,
                &mut password_data,
                &mut item_ref,
            )
        };
        if !password_data.is_null() {
            unsafe {
                SecKeychainItemFreeContent(ptr::null_mut(), password_data);
            }
        }
        if status == ERR_SEC_ITEM_NOT_FOUND {
            return Ok(());
        }
        if status != 0 {
            return Err(failure("find the password for removal", status));
        }
        let delete_status = unsafe { SecKeychainItemDelete(item_ref) };
        if delete_status == 0 {
            Ok(())
        } else {
            Err(failure("remove the password", delete_status))
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    #[repr(C)]
    struct FileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[repr(C)]
    struct Credential {
        flags: u32,
        credential_type: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: FileTime,
        credential_blob_size: u32,
        credential_blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "Advapi32")]
    unsafe extern "system" {
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredReadW(
            target_name: *const u16,
            credential_type: u32,
            reserved: u32,
            credential: *mut *mut Credential,
        ) -> i32;
        fn CredDeleteW(target_name: *const u16, credential_type: u32, flags: u32) -> i32;
        fn CredFree(buffer: *mut c_void);
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn store(reference: &str, password: &str) -> Result<(), String> {
        let target_name = wide(reference);
        let user_name = wide("gib");
        let mut blob = password.as_bytes().to_vec();
        let credential = Credential {
            flags: 0,
            credential_type: CRED_TYPE_GENERIC,
            target_name: target_name.as_ptr() as *mut u16,
            comment: ptr::null_mut(),
            last_written: FileTime {
                low_date_time: 0,
                high_date_time: 0,
            },
            credential_blob_size: blob.len() as u32,
            credential_blob: blob.as_mut_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null_mut(),
            target_alias: ptr::null_mut(),
            user_name: user_name.as_ptr() as *mut u16,
        };

        let success = unsafe { CredWriteW(&credential, 0) };
        if success != 0 {
            Ok(())
        } else {
            Err(format!(
                "Windows Credential Manager failed to store the password: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    pub(super) fn read(reference: &str) -> Result<String, String> {
        let target_name = wide(reference);
        let mut credential = ptr::null_mut();
        let success =
            unsafe { CredReadW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if success == 0 {
            return Err(format!(
                "The password reference '{}' is unavailable in Windows Credential Manager",
                reference
            ));
        }

        let result = unsafe {
            let credential_ref = &*credential;
            let bytes = std::slice::from_raw_parts(
                credential_ref.credential_blob,
                credential_ref.credential_blob_size as usize,
            );
            String::from_utf8(bytes.to_vec())
                .map_err(|_| "Credential Manager returned a non-text password".to_string())
        };
        unsafe { CredFree(credential as *mut c_void) };
        result
    }

    pub(super) fn delete(reference: &str) -> Result<(), String> {
        let target_name = wide(reference);
        let success = unsafe { CredDeleteW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if success != 0 {
            Ok(())
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(1168) {
                Ok(())
            } else {
                Err(format!(
                    "Windows Credential Manager failed to remove the password: {}",
                    error
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_references_are_stable_and_scoped_to_a_job() {
        assert_eq!(
            password_reference("job-123"),
            "gib/autostart/job-123/password"
        );
    }
}
