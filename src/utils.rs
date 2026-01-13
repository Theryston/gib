use crate::commands::storage::add::Storage;
use argon2::Argon2;
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use dirs::home_dir;
use rand_core::{OsRng, TryRngCore};
use walkdir;

const MAGIC: &[u8; 4] = b"GIB1";

pub fn compress_bytes(data: &[u8], level: i32) -> Vec<u8> {
    zstd::encode_all(data, level).unwrap()
}

pub fn decompress_bytes(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(data).unwrap()
}

fn derive_key(password: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];

    let argon2 = Argon2::default();
    argon2
        .hash_password_into(password, salt, &mut key)
        .unwrap_or_else(|_| {
            eprintln!("Argon2 failed");
            std::process::exit(1);
        });

    key
}

pub fn encrypt_bytes(data: &[u8], password: &[u8]) -> Vec<u8> {
    let mut salt = [0u8; 16];
    let mut rng = OsRng;

    rng.try_fill_bytes(&mut salt).unwrap();

    let key_bytes = derive_key(password, &salt);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));

    let mut nonce_bytes = [0u8; 12];
    rng.try_fill_bytes(&mut nonce_bytes).unwrap();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher.encrypt(nonce, data).unwrap_or_else(|_| {
        eprintln!("Encryption failed");
        std::process::exit(1);
    });

    let mut out =
        Vec::with_capacity(MAGIC.len() + salt.len() + nonce_bytes.len() + ciphertext.len());

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);

    out
}

pub fn decrypt_bytes(blob: &[u8], password: &[u8]) -> Result<Vec<u8>, &'static str> {
    if blob.len() < 4 + 16 + 12 {
        eprintln!("Blob too small");
        std::process::exit(1);
    }

    if &blob[..4] != MAGIC {
        eprintln!("Not encrypted");
        std::process::exit(1);
    }

    let salt = &blob[4..20];
    let nonce = &blob[20..32];
    let ciphertext = &blob[32..];

    let key_bytes = derive_key(password, salt);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));

    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| {
            eprintln!("Invalid password or corrupted data");
            std::process::exit(1);
        })
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

pub fn get_storage(name: &str) -> Storage {
    let home_dir = home_dir().unwrap();
    let storage_path = home_dir
        .join(".gib")
        .join("storages")
        .join(format!("{}.msgpack", name));
    let contents = std::fs::read(storage_path).unwrap();

    rmp_serde::from_slice(&contents).unwrap()
}

pub fn list_files(path: &str) -> Vec<String> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file());

    for entry in walker {
        files.push(entry.path().display().to_string());
    }

    files
}
