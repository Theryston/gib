//! CLI-side access to legacy operating-system credential stores.
//!
//! New library jobs use a protected per-client secret file. This adapter only
//! reads or removes legacy keychain entries when an existing job points at one;
//! the secret value never enters a public DTO or event.

use gib::api::{ErrorCode, GibError};

#[cfg(target_os = "linux")]
pub(crate) fn read(reference: &str) -> Result<Option<String>, GibError> {
    let output = std::process::Command::new("secret-tool")
        .args(["lookup", "service", "gib-autostart", "reference", reference])
        .output()
        .map_err(|error| GibError::new(ErrorCode::Unsupported, error.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let password = String::from_utf8(output.stdout).map_err(|_| {
        GibError::new(
            ErrorCode::Unsupported,
            "The keyring returned a non-text password",
        )
    })?;
    Ok(Some(password.trim_end_matches(['\r', '\n']).to_string()))
}

#[cfg(target_os = "linux")]
pub(crate) fn remove(reference: &str) -> Result<(), GibError> {
    let output = std::process::Command::new("secret-tool")
        .args(["clear", "service", "gib-autostart", "reference", reference])
        .output()
        .map_err(|error| GibError::new(ErrorCode::Unsupported, error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GibError::new(
            ErrorCode::Unsupported,
            "The legacy keyring entry could not be removed",
        ))
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn read(reference: &str) -> Result<Option<String>, GibError> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", reference, "-a", "gib", "-w"])
        .output()
        .map_err(|error| GibError::new(ErrorCode::Unsupported, error.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let password = String::from_utf8(output.stdout).map_err(|_| {
        GibError::new(
            ErrorCode::Unsupported,
            "The Keychain returned a non-text password",
        )
    })?;
    Ok(Some(password.trim_end_matches(['\r', '\n']).to_string()))
}

#[cfg(target_os = "macos")]
pub(crate) fn remove(reference: &str) -> Result<(), GibError> {
    let output = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", reference, "-a", "gib"])
        .output()
        .map_err(|error| GibError::new(ErrorCode::Unsupported, error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GibError::new(
            ErrorCode::Unsupported,
            "The legacy Keychain entry could not be removed",
        ))
    }
}

#[cfg(windows)]
pub(crate) fn read(_reference: &str) -> Result<Option<String>, GibError> {
    // Windows Credential Manager is intentionally handled through the native
    // API by the platform-specific implementation below.
    native::read(_reference).map(Some)
}

#[cfg(windows)]
pub(crate) fn remove(reference: &str) -> Result<(), GibError> {
    native::remove(reference)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn read(_reference: &str) -> Result<Option<String>, GibError> {
    Ok(None)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn remove(_reference: &str) -> Result<(), GibError> {
    Ok(())
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::ffi::c_void;
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;

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

    pub(super) fn read(reference: &str) -> Result<String, GibError> {
        let target_name = wide(reference);
        let mut credential = ptr::null_mut();
        let success =
            unsafe { CredReadW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if success == 0 {
            return Err(GibError::new(
                ErrorCode::PasswordRequired,
                "The legacy Credential Manager entry is unavailable",
            ));
        }
        let result = unsafe {
            let credential_ref = &*credential;
            let bytes = std::slice::from_raw_parts(
                credential_ref.credential_blob,
                credential_ref.credential_blob_size as usize,
            );
            String::from_utf8(bytes.to_vec()).map_err(|_| {
                GibError::new(
                    ErrorCode::Unsupported,
                    "Credential Manager returned a non-text password",
                )
            })
        };
        unsafe { CredFree(credential as *mut c_void) };
        result
    }

    pub(super) fn remove(reference: &str) -> Result<(), GibError> {
        let target_name = wide(reference);
        let success = unsafe { CredDeleteW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if success != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(1168) {
            Ok(())
        } else {
            Err(GibError::new(
                ErrorCode::Unsupported,
                "The legacy Credential Manager entry could not be removed",
            ))
        }
    }
}
