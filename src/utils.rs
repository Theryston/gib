//! Small, side-effect-free encoding helpers shared by the library.

use argon2::Argon2;
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use rand_core::{OsRng, TryRngCore};

const MAGIC: &[u8; 4] = b"GIB1";

pub(crate) fn compress_bytes(data: &[u8], level: i32) -> Vec<u8> {
    zstd::encode_all(data, level).unwrap_or_else(|_| data.to_vec())
}

pub(crate) fn decompress_bytes(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(data).unwrap_or_else(|_| data.to_vec())
}

fn derive_key(password: &[u8], salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0_u8; 32];
    Argon2::default()
        .hash_password_into(password, salt, &mut key)
        .map_err(|_| "Argon2 failed".to_string())?;
    Ok(key)
}

pub(crate) fn encrypt_bytes(data: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    let mut rng = OsRng;
    let mut salt = [0_u8; 16];
    rng.try_fill_bytes(&mut salt)
        .map_err(|error| format!("Failed to generate encryption salt: {error}"))?;
    let key_bytes = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    let mut nonce_bytes = [0_u8; 12];
    rng.try_fill_bytes(&mut nonce_bytes)
        .map_err(|error| format!("Failed to generate encryption nonce: {error}"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), data)
        .map_err(|_| "Encryption failed".to_string())?;

    let mut output =
        Vec::with_capacity(MAGIC.len() + salt.len() + nonce_bytes.len() + ciphertext.len());
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&salt);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

pub(crate) fn decrypt_bytes(blob: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < MAGIC.len() + 16 + 12 {
        return Err("Blob too small".to_string());
    }
    if &blob[..MAGIC.len()] != MAGIC {
        return Err("Not encrypted".to_string());
    }
    let key_bytes = derive_key(password, &blob[4..20])?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    cipher
        .decrypt(Nonce::from_slice(&blob[20..32]), &blob[32..])
        .map_err(|_| "Invalid password or corrupted data".to_string())
}

pub(crate) fn is_encrypted(data: &[u8]) -> bool {
    data.len() >= MAGIC.len() && &data[..MAGIC.len()] == MAGIC
}
