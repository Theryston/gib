use crate::storage::FS;
use crate::utils::{decrypt_bytes, encrypt_bytes, is_encrypted};
use std::sync::Arc;

pub(crate) struct ReadDecryption {
    pub bytes: Vec<u8>,
}

pub(crate) async fn read_file_maybe_decrypt(
    fs: &Arc<dyn FS>,
    path: &str,
    password: Option<&str>,
    encrypted_without_password_error: &str,
) -> Result<ReadDecryption, String> {
    let file_bytes = fs
        .read_file(path)
        .await
        .map_err(|error| format!("Failed to read file {path}: {error}"))?;

    if file_bytes.is_empty() {
        return Ok(ReadDecryption { bytes: Vec::new() });
    }

    let was_encrypted = is_encrypted(&file_bytes);

    let decrypted_bytes = match password {
        Some(password) => {
            if was_encrypted {
                decrypt_bytes(&file_bytes, password.as_bytes())?
            } else {
                file_bytes
            }
        }
        None => {
            if was_encrypted {
                return Err(encrypted_without_password_error.to_string());
            } else {
                file_bytes
            }
        }
    };

    Ok(ReadDecryption {
        bytes: decrypted_bytes,
    })
}

pub(crate) async fn write_file_maybe_encrypt(
    fs: &Arc<dyn FS>,
    path: &str,
    data: &[u8],
    password: Option<&str>,
) -> Result<(), String> {
    let final_bytes = encode_file_bytes(data, password)?;

    fs.write_file(path, &final_bytes)
        .await
        .map_err(|e| format!("Failed to write file {}: {}", path, e))?;

    Ok(())
}

pub(crate) fn encode_file_bytes(data: &[u8], password: Option<&str>) -> Result<Vec<u8>, String> {
    match password {
        Some(password) => encrypt_bytes(data, password.as_bytes()),
        None => Ok(data.to_vec()),
    }
}
