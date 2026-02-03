use crate::storage_clients::{build_storage_client, parse_storage_config, ClientStorage, StorageConfig};
use argon2::Argon2;
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use console::style;
use dirs::home_dir;
use indicatif::ProgressBar;
use rand_core::{OsRng, TryRngCore};
use std::sync::Arc;

use crate::output::{emit_error, is_json_mode};
const MAGIC: &[u8; 4] = b"GIB1";

pub fn compress_bytes(data: &[u8], level: i32) -> Vec<u8> {
    zstd::encode_all(data, level).unwrap()
}

pub fn decompress_bytes(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(data).unwrap()
}

fn derive_key(password: &[u8], salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];

    let argon2 = Argon2::default();
    argon2
        .hash_password_into(password, salt, &mut key)
        .map_err(|_| "Argon2 failed".to_string())?;

    Ok(key)
}

pub fn encrypt_bytes(data: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; 16];
    let mut rng = OsRng;

    rng.try_fill_bytes(&mut salt).unwrap();

    let key_bytes = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));

    let mut nonce_bytes = [0u8; 12];
    rng.try_fill_bytes(&mut nonce_bytes).unwrap();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|_| "Encryption failed".to_string())?;

    let mut out =
        Vec::with_capacity(MAGIC.len() + salt.len() + nonce_bytes.len() + ciphertext.len());

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);

    Ok(out)
}

pub fn decrypt_bytes(blob: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 4 + 16 + 12 {
        return Err("Blob too small".to_string());
    }

    if &blob[..4] != MAGIC {
        return Err("Not encrypted".to_string());
    }

    let salt = &blob[4..20];
    let nonce = &blob[20..32];
    let ciphertext = &blob[32..];

    let key_bytes = derive_key(password, salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));

    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| "Invalid password or corrupted data".to_string())
}

pub fn is_encrypted(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..4] == MAGIC
}

pub fn get_pwd_string() -> String {
    std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string()
}

pub fn get_storage(name: &str) -> StorageConfig {
    let home_dir = home_dir().unwrap();
    let storage_path = home_dir
        .join(".gib")
        .join("storages")
        .join(format!("{}.msgpack", name));
    let contents = std::fs::read(&storage_path).unwrap_or_else(|e| {
        handle_error(format!("Failed to read storage '{}': {}", name, e), None)
    });

    parse_storage_config(&contents)
        .unwrap_or_else(|e| handle_error(format!("Failed to parse storage '{}': {}", name, e), None))
}

pub fn handle_error(error: String, pb: Option<&ProgressBar>) -> ! {
    if let Some(pb) = pb {
        pb.finish_and_clear();
    }
    if is_json_mode() {
        emit_error(&error, "error");
    } else {
        eprintln!("{}", style(error).red());
        std::process::exit(1);
    }
}

pub fn get_storage_client(
    storage: &StorageConfig,
    pb: Option<&ProgressBar>,
) -> Arc<dyn ClientStorage> {
    build_storage_client(storage).unwrap_or_else(|e| handle_error(e, pb))
}
