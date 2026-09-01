use crate::application::ports::{
    CredentialReference, CredentialStore, CredentialStoreError, MAX_STORAGE_CREDENTIAL_LENGTH,
    StorageCredentials,
};
const CREDENTIAL_BLOB_VERSION: u8 = 1;
const S3_CREDENTIAL_KIND: u8 = 1;
const WEBDAV_CREDENTIAL_KIND: u8 = 2;
const MAX_CREDENTIAL_BLOB_BYTES: usize = 2 + 3 * (4 + MAX_STORAGE_CREDENTIAL_LENGTH) + 1;

/// A credential-store adapter backed by the operating system's secure store.
///
/// Linux uses Secret Service through `secret-tool`, macOS uses Keychain, and
/// Windows uses Credential Manager. The named-storage configuration file keeps
/// only the opaque references returned by this adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformCredentialStore;

impl PlatformCredentialStore {
    /// Creates an operating-system credential-store adapter.
    pub const fn new() -> Self {
        Self
    }
}

impl CredentialStore for PlatformCredentialStore {
    fn store(
        &self,
        credentials: &StorageCredentials,
    ) -> Result<CredentialReference, CredentialStoreError> {
        let mut random = [0_u8; 16];
        getrandom::getrandom(&mut random).map_err(|_| CredentialStoreError::Unavailable)?;
        let reference = CredentialReference::new(format!("gib-storage-{}", encode_hex(&random)))
            .map_err(|_| CredentialStoreError::Unavailable)?;
        let blob = encode_credentials(credentials)?;
        let value = encode_hex(&blob);
        platform_store(reference.as_str(), value.as_bytes())?;
        Ok(reference)
    }

    fn load(
        &self,
        reference: &CredentialReference,
    ) -> Result<StorageCredentials, CredentialStoreError> {
        let value = platform_load(reference.as_str())?;
        if value.trim().len() > MAX_CREDENTIAL_BLOB_BYTES.saturating_mul(2) {
            return Err(CredentialStoreError::Invalid);
        }
        let blob = decode_hex(&value).ok_or(CredentialStoreError::Invalid)?;
        decode_credentials(&blob)
    }

    fn delete(&self, reference: &CredentialReference) -> Result<(), CredentialStoreError> {
        platform_delete(reference.as_str())
    }
}

fn encode_credentials(credentials: &StorageCredentials) -> Result<Vec<u8>, CredentialStoreError> {
    let mut encoded = vec![CREDENTIAL_BLOB_VERSION];
    match credentials {
        StorageCredentials::S3(credentials) => {
            encoded.push(S3_CREDENTIAL_KIND);
            append_text(&mut encoded, credentials.access_key())?;
            append_text(&mut encoded, credentials.secret_key())?;
            match credentials.session_token() {
                Some(session_token) => {
                    encoded.push(1);
                    append_text(&mut encoded, session_token)?;
                }
                None => encoded.push(0),
            }
        }
        StorageCredentials::WebDav(credentials) => {
            encoded.push(WEBDAV_CREDENTIAL_KIND);
            append_text(&mut encoded, credentials.username())?;
            append_text(&mut encoded, credentials.password())?;
        }
    }
    Ok(encoded)
}

fn decode_credentials(encoded: &[u8]) -> Result<StorageCredentials, CredentialStoreError> {
    let mut cursor = 0;
    if take_byte(encoded, &mut cursor)? != CREDENTIAL_BLOB_VERSION {
        return Err(CredentialStoreError::Invalid);
    }
    let kind = take_byte(encoded, &mut cursor)?;
    let credentials = match kind {
        S3_CREDENTIAL_KIND => {
            let access_key = take_text(encoded, &mut cursor)?;
            let secret_key = take_text(encoded, &mut cursor)?;
            let session_token = match take_byte(encoded, &mut cursor)? {
                0 => None,
                1 => Some(take_text(encoded, &mut cursor)?),
                _ => return Err(CredentialStoreError::Invalid),
            };
            StorageCredentials::s3_with_session_token(access_key, secret_key, session_token)
                .map_err(|_| CredentialStoreError::Invalid)?
        }
        WEBDAV_CREDENTIAL_KIND => {
            let username = take_text(encoded, &mut cursor)?;
            let password = take_text(encoded, &mut cursor)?;
            StorageCredentials::webdav(username, password)
                .map_err(|_| CredentialStoreError::Invalid)?
        }
        _ => return Err(CredentialStoreError::Invalid),
    };
    if cursor != encoded.len() {
        return Err(CredentialStoreError::Invalid);
    }
    Ok(credentials)
}

fn append_text(output: &mut Vec<u8>, value: &str) -> Result<(), CredentialStoreError> {
    let length = u32::try_from(value.len()).map_err(|_| CredentialStoreError::Invalid)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn take_byte(encoded: &[u8], cursor: &mut usize) -> Result<u8, CredentialStoreError> {
    let value = *encoded.get(*cursor).ok_or(CredentialStoreError::Invalid)?;
    *cursor += 1;
    Ok(value)
}

fn take_text(encoded: &[u8], cursor: &mut usize) -> Result<String, CredentialStoreError> {
    let length_end = cursor.checked_add(4).ok_or(CredentialStoreError::Invalid)?;
    let length = encoded
        .get(*cursor..length_end)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_be_bytes)
        .ok_or(CredentialStoreError::Invalid)?;
    *cursor = length_end;
    let length = usize::try_from(length).map_err(|_| CredentialStoreError::Invalid)?;
    if length > MAX_STORAGE_CREDENTIAL_LENGTH {
        return Err(CredentialStoreError::Invalid);
    }
    let end = cursor
        .checked_add(length)
        .ok_or(CredentialStoreError::Invalid)?;
    let value = encoded
        .get(*cursor..end)
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .ok_or(CredentialStoreError::Invalid)?;
    *cursor = end;
    Ok(value)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut decoded = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    for pair in pairs {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        decoded.push((high << 4) | low);
    }
    Some(decoded)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn platform_store(reference: &str, value: &[u8]) -> Result<(), CredentialStoreError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("secret-tool")
        .args([
            "store",
            "--label",
            "Gib storage credential",
            "service",
            "gib-storage",
            "reference",
            reference,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| CredentialStoreError::Unavailable)?;
    child
        .stdin
        .take()
        .ok_or(CredentialStoreError::Unavailable)?
        .write_all(value)
        .map_err(|_| CredentialStoreError::Io)?;
    let status = child.wait().map_err(|_| CredentialStoreError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(CredentialStoreError::PermissionDenied)
    }
}

#[cfg(target_os = "linux")]
fn platform_load(reference: &str) -> Result<String, CredentialStoreError> {
    use std::process::Command;

    let output = Command::new("secret-tool")
        .args(["lookup", "service", "gib-storage", "reference", reference])
        .output()
        .map_err(|_| CredentialStoreError::Unavailable)?;
    if !output.status.success() {
        return Err(CredentialStoreError::NotFound);
    }
    String::from_utf8(output.stdout).map_err(|_| CredentialStoreError::Invalid)
}

#[cfg(target_os = "linux")]
fn platform_delete(reference: &str) -> Result<(), CredentialStoreError> {
    use std::process::Command;

    let output = Command::new("secret-tool")
        .args(["clear", "service", "gib-storage", "reference", reference])
        .output()
        .map_err(|_| CredentialStoreError::Unavailable)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CredentialStoreError::NotFound)
    }
}

#[cfg(target_os = "macos")]
fn platform_store(reference: &str, value: &[u8]) -> Result<(), CredentialStoreError> {
    use std::process::{Command, Stdio};

    let value = String::from_utf8(value.to_vec()).map_err(|_| CredentialStoreError::Invalid)?;
    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-a",
            "gib",
            "-s",
            reference,
            "-w",
            value.as_str(),
            "-U",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| CredentialStoreError::Unavailable)?;
    if status.success() {
        Ok(())
    } else {
        Err(CredentialStoreError::PermissionDenied)
    }
}

#[cfg(target_os = "macos")]
fn platform_load(reference: &str) -> Result<String, CredentialStoreError> {
    use std::process::Command;

    let output = Command::new("security")
        .args(["find-generic-password", "-a", "gib", "-s", reference, "-w"])
        .output()
        .map_err(|_| CredentialStoreError::Unavailable)?;
    if !output.status.success() {
        return Err(CredentialStoreError::NotFound);
    }
    String::from_utf8(output.stdout).map_err(|_| CredentialStoreError::Invalid)
}

#[cfg(target_os = "macos")]
fn platform_delete(reference: &str) -> Result<(), CredentialStoreError> {
    use std::process::Command;

    let output = Command::new("security")
        .args(["delete-generic-password", "-a", "gib", "-s", reference])
        .output()
        .map_err(|_| CredentialStoreError::Unavailable)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CredentialStoreError::NotFound)
    }
}

#[cfg(windows)]
fn platform_store(reference: &str, value: &[u8]) -> Result<(), CredentialStoreError> {
    windows::store(reference, value)
}

#[cfg(windows)]
fn platform_load(reference: &str) -> Result<String, CredentialStoreError> {
    windows::load(reference)
        .and_then(|value| String::from_utf8(value).map_err(|_| CredentialStoreError::Invalid))
}

#[cfg(windows)]
fn platform_delete(reference: &str) -> Result<(), CredentialStoreError> {
    windows::delete(reference)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_store(_reference: &str, _value: &[u8]) -> Result<(), CredentialStoreError> {
    Err(CredentialStoreError::Unavailable)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_load(_reference: &str) -> Result<String, CredentialStoreError> {
    Err(CredentialStoreError::Unavailable)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_delete(_reference: &str) -> Result<(), CredentialStoreError> {
    Err(CredentialStoreError::Unavailable)
}

#[cfg(windows)]
mod windows {
    use super::CredentialStoreError;
    use std::ffi::c_void;
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    const ERROR_NOT_FOUND: i32 = 1168;

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

    pub(super) fn store(reference: &str, value: &[u8]) -> Result<(), CredentialStoreError> {
        let target_name = wide(reference);
        let user_name = wide("gib");
        let mut blob = value.to_vec();
        let blob_size = u32::try_from(blob.len()).map_err(|_| CredentialStoreError::Invalid)?;
        let credential = Credential {
            flags: 0,
            credential_type: CRED_TYPE_GENERIC,
            target_name: target_name.as_ptr() as *mut u16,
            comment: ptr::null_mut(),
            last_written: FileTime {
                low_date_time: 0,
                high_date_time: 0,
            },
            credential_blob_size: blob_size,
            credential_blob: blob.as_mut_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null_mut(),
            target_alias: ptr::null_mut(),
            user_name: user_name.as_ptr() as *mut u16,
        };

        // SAFETY: all pointers reference live, correctly laid-out buffers for
        // the duration of the OS call, and Windows does not retain them.
        let success = unsafe { CredWriteW(&credential, 0) };
        if success != 0 {
            Ok(())
        } else {
            Err(CredentialStoreError::PermissionDenied)
        }
    }

    pub(super) fn load(reference: &str) -> Result<Vec<u8>, CredentialStoreError> {
        let target_name = wide(reference);
        let mut credential = ptr::null_mut();
        // SAFETY: `target_name` is a NUL-terminated UTF-16 buffer that lives
        // through the call, and Windows initializes the out pointer.
        let success =
            unsafe { CredReadW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if success == 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NOT_FOUND) {
                Err(CredentialStoreError::NotFound)
            } else {
                Err(CredentialStoreError::PermissionDenied)
            };
        }
        if credential.is_null() {
            return Err(CredentialStoreError::Unavailable);
        }

        // SAFETY: a successful `CredReadW` returns a valid credential buffer
        // owned by Windows; the bytes are copied before `CredFree` releases it.
        let value = unsafe {
            let credential_ref = &*credential;
            let bytes = std::slice::from_raw_parts(
                credential_ref.credential_blob,
                credential_ref.credential_blob_size as usize,
            );
            bytes.to_vec()
        };
        // SAFETY: `credential` is the allocation returned by `CredReadW` and
        // has not been freed elsewhere.
        unsafe { CredFree(credential as *mut c_void) };
        Ok(value)
    }

    pub(super) fn delete(reference: &str) -> Result<(), CredentialStoreError> {
        let target_name = wide(reference);
        // SAFETY: `target_name` is a NUL-terminated UTF-16 buffer that lives
        // through the OS call.
        let success = unsafe { CredDeleteW(target_name.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if success != 0 {
            Ok(())
        } else if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_NOT_FOUND) {
            Err(CredentialStoreError::NotFound)
        } else {
            Err(CredentialStoreError::PermissionDenied)
        }
    }
}
