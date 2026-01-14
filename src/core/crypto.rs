use crate::fs::FS;
use crate::utils::handle_error;
use crate::utils::{decrypt_bytes, encrypt_bytes, is_encrypted};
use dialoguer::Password;
use std::sync::Arc;

pub struct ReadDecryption {
    pub bytes: Vec<u8>,
    pub was_encrypted: bool,
}

pub(crate) async fn read_file_maybe_decrypt(
    fs: &Arc<dyn FS>,
    path: &str,
    password: Option<&str>,
    encrypted_without_password_error: &str,
) -> Result<ReadDecryption, String> {
    let file_bytes = fs.read_file(path).await.unwrap_or_else(|_| Vec::new());

    if file_bytes.is_empty() {
        return Ok(ReadDecryption {
            bytes: Vec::new(),
            was_encrypted: false,
        });
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
        was_encrypted,
    })
}

pub(crate) async fn write_file_maybe_encrypt(
    fs: &Arc<dyn FS>,
    path: &str,
    data: &[u8],
    password: Option<&str>,
) -> Result<(), String> {
    let final_bytes = match password {
        Some(password) => encrypt_bytes(data, password.as_bytes()).unwrap_or_else(|_| Vec::new()),
        None => data.to_vec(),
    };

    fs.write_file(path, &final_bytes)
        .await
        .map_err(|e| format!("Failed to write file {}: {}", path, e))?;

    Ok(())
}

pub(crate) fn get_password(is_required: bool) -> Option<String> {
    let password = Password::new()
        .allow_empty_password(!is_required)
        .with_prompt("Enter your repository password (leave empty to skip encryption)")
        .interact()
        .unwrap();

    let password = if !password.is_empty() {
        let confirm = Password::new()
            .with_prompt("Repeat password")
            .allow_empty_password(false)
            .interact()
            .unwrap();

        if password != confirm {
            handle_error("Error: the passwords don't match.".to_string(), None);
        }

        Some(password)
    } else {
        None
    };

    password
}
