use super::repository::FormatError;
use crate::domain::{
    ARGON2ID_MEMORY_COST_KIB, ARGON2ID_PARALLELISM, ARGON2ID_TIME_COST,
    REPOSITORY_ENCRYPTION_KEY_LENGTH, REPOSITORY_ENCRYPTION_SALT_LENGTH, RepositorySalt,
    XCHACHA20_POLY1305_NONCE_LENGTH, XCHACHA20_POLY1305_TAG_LENGTH,
};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{Key, KeyInit, Tag, XChaCha20Poly1305, XNonce, aead::AeadInPlace};
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use zeroize::{Zeroize, Zeroizing};

const MAX_ZSTD_WINDOW_LOG: u32 = 26;
const TRANSFORM_BUFFER_SIZE: usize = 64 * 1024;

/// The repository key held by the public encryption context.
#[derive(Clone)]
pub(crate) struct EncryptionKey([u8; REPOSITORY_ENCRYPTION_KEY_LENGTH]);

impl EncryptionKey {
    pub(crate) fn as_bytes(&self) -> &[u8; REPOSITORY_ENCRYPTION_KEY_LENGTH] {
        &self.0
    }
}

impl fmt::Debug for EncryptionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncryptionKey")
            .finish_non_exhaustive()
    }
}

impl Drop for EncryptionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Repository encryption material kept out of persisted wire models.
#[derive(Clone)]
pub(crate) struct EncryptionContext {
    salt: RepositorySalt,
    key: EncryptionKey,
}

impl EncryptionContext {
    pub(crate) const fn salt(&self) -> RepositorySalt {
        self.salt
    }

    pub(crate) fn key(&self) -> &EncryptionKey {
        &self.key
    }
}

impl fmt::Debug for EncryptionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncryptionContext")
            .field("salt", &"<redacted>")
            .field("key", &self.key)
            .finish()
    }
}

pub(crate) fn derive_encryption_context(
    password: &[u8],
    salt: RepositorySalt,
) -> Result<EncryptionContext, FormatError> {
    Ok(EncryptionContext {
        salt,
        key: derive_encryption_key(password, salt)?,
    })
}

pub(crate) fn generate_encryption_context(
    password: &[u8],
) -> Result<EncryptionContext, FormatError> {
    let mut bytes = [0u8; REPOSITORY_ENCRYPTION_SALT_LENGTH];
    getrandom::getrandom(&mut bytes).map_err(|_| FormatError::RandomnessFailure)?;
    derive_encryption_context(password, RepositorySalt::from_bytes(bytes))
}

pub(crate) fn derive_encryption_key(
    password: &[u8],
    salt: RepositorySalt,
) -> Result<EncryptionKey, FormatError> {
    let params = Params::new(
        ARGON2ID_MEMORY_COST_KIB,
        ARGON2ID_TIME_COST,
        ARGON2ID_PARALLELISM,
        Some(REPOSITORY_ENCRYPTION_KEY_LENGTH),
    )
    .map_err(|_| FormatError::KdfFailure)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; REPOSITORY_ENCRYPTION_KEY_LENGTH];
    if argon2
        .hash_password_into(password, salt.as_bytes(), &mut key)
        .is_err()
    {
        key.zeroize();
        return Err(FormatError::KdfFailure);
    }
    Ok(EncryptionKey(key))
}

pub(crate) fn random_nonce() -> Result<[u8; XCHACHA20_POLY1305_NONCE_LENGTH], FormatError> {
    let mut nonce = [0u8; XCHACHA20_POLY1305_NONCE_LENGTH];
    getrandom::getrandom(&mut nonce).map_err(|_| FormatError::RandomnessFailure)?;
    Ok(nonce)
}

pub(crate) fn compress(
    plaintext: &[u8],
    level: i32,
    max_output_length: usize,
) -> Result<Vec<u8>, FormatError> {
    let mut encoder =
        zstd::stream::write::Encoder::new(BoundedOutput::with_capacity(max_output_length), level)
            .map_err(|_| FormatError::CompressionFailure)?;
    encoder
        .include_checksum(true)
        .map_err(|_| FormatError::CompressionFailure)?;
    encoder
        .window_log(MAX_ZSTD_WINDOW_LOG)
        .map_err(|_| FormatError::CompressionFailure)?;
    encoder
        .write_all(plaintext)
        .map_err(|_| FormatError::CompressionFailure)?;
    let output = encoder
        .finish()
        .map_err(|_| FormatError::CompressionFailure)?;
    Ok(output.into_inner())
}

pub(crate) fn decompress(
    compressed: &[u8],
    expected_length: usize,
) -> Result<Vec<u8>, FormatError> {
    let mut buffer = [0u8; TRANSFORM_BUFFER_SIZE];
    let result = (|| {
        let mut decoder = zstd::stream::read::Decoder::new(Cursor::new(compressed))
            .map_err(|_| FormatError::DecompressionFailure)?;
        decoder
            .window_log_max(MAX_ZSTD_WINDOW_LOG)
            .map_err(|_| FormatError::DecompressionFailure)?;
        let mut decoder = decoder.single_frame();
        let mut output = BoundedOutput::with_capacity(expected_length);
        loop {
            let read = decoder
                .read(&mut buffer)
                .map_err(|_| FormatError::DecompressionFailure)?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|_| FormatError::DecompressionFailure)?;
        }

        let input = decoder.finish();
        if input.get_ref().position() != compressed.len() as u64 || !input.buffer().is_empty() {
            return Err(FormatError::DecompressionFailure);
        }
        if output.bytes.len() != expected_length {
            return Err(FormatError::DecompressionFailure);
        }
        Ok(std::mem::take(&mut *output.bytes))
    })();
    buffer.zeroize();
    result
}

pub(crate) fn encrypt_in_place(
    key: &EncryptionKey,
    nonce: &[u8; XCHACHA20_POLY1305_NONCE_LENGTH],
    associated_data: &[u8],
    payload: &mut Vec<u8>,
) -> Result<(), FormatError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    let tag =
        match cipher.encrypt_in_place_detached(XNonce::from_slice(nonce), associated_data, payload)
        {
            Ok(tag) => tag,
            Err(_) => {
                payload.zeroize();
                return Err(FormatError::AuthenticationFailure);
            }
        };
    payload.extend_from_slice(&tag);
    Ok(())
}

pub(crate) fn decrypt_in_place(
    key: &EncryptionKey,
    nonce: &[u8; XCHACHA20_POLY1305_NONCE_LENGTH],
    associated_data: &[u8],
    payload: &mut Vec<u8>,
) -> Result<(), FormatError> {
    if payload.len() < XCHACHA20_POLY1305_TAG_LENGTH {
        payload.zeroize();
        return Err(FormatError::AuthenticationFailure);
    }
    let tag_offset = payload.len() - XCHACHA20_POLY1305_TAG_LENGTH;
    let (ciphertext, tag_bytes) = payload.split_at_mut(tag_offset);
    let tag = Tag::from_slice(tag_bytes);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    if cipher
        .decrypt_in_place_detached(XNonce::from_slice(nonce), associated_data, ciphertext, tag)
        .is_err()
    {
        payload.zeroize();
        return Err(FormatError::AuthenticationFailure);
    }
    payload.truncate(tag_offset);
    Ok(())
}

struct BoundedOutput {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
}

impl BoundedOutput {
    fn with_capacity(limit: usize) -> Self {
        Self {
            bytes: Zeroizing::new(Vec::with_capacity(limit.min(TRANSFORM_BUFFER_SIZE))),
            limit,
        }
    }

    fn into_inner(mut self) -> Vec<u8> {
        std::mem::take(&mut *self.bytes)
    }
}

impl Write for BoundedOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(new_length) = self.bytes.len().checked_add(buffer.len()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed output length overflow",
            ));
        };
        if new_length > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed output exceeds its declared length",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES;

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            output.push(DIGITS[usize::from(byte >> 4)] as char);
            output.push(DIGITS[usize::from(byte & 0x0f)] as char);
        }
        output
    }

    #[test]
    fn kdf_and_aead_known_answers_are_stable() {
        let key = derive_encryption_key(
            b"known-answer-password",
            RepositorySalt::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
        )
        .expect("KDF should succeed");
        assert_eq!(
            hex(&key.0),
            "1c49b3cda34446e1c9be94f9eabbfb9dddcff8271909db861b44f86b2a238ab2"
        );

        let mut payload = b"known-answer-plaintext".to_vec();
        let nonce = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
        ];
        encrypt_in_place(&key, &nonce, b"known-answer-aad", &mut payload)
            .expect("AEAD should succeed");
        assert_eq!(
            hex(&payload),
            "d9103222320aeba32c9e403259b17f25a2ce8d0a1fa7bbd7d127ebf5a196d215c49122197a36"
        );
        decrypt_in_place(&key, &nonce, b"known-answer-aad", &mut payload)
            .expect("AEAD should verify");
        assert_eq!(payload, b"known-answer-plaintext");
    }

    #[test]
    fn decompression_rejects_trailing_data_and_wrong_lengths() {
        let plaintext = b"strict zstandard payload";
        let compressed = compress(plaintext, 3, MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES)
            .expect("compression should succeed");
        assert_eq!(
            decompress(&compressed, plaintext.len() + 1),
            Err(FormatError::DecompressionFailure)
        );
        assert_eq!(
            decompress(&compressed, plaintext.len() - 1),
            Err(FormatError::DecompressionFailure)
        );

        let mut trailing = compressed;
        trailing.push(0);
        assert_eq!(
            decompress(&trailing, plaintext.len()),
            Err(FormatError::DecompressionFailure)
        );
    }

    #[test]
    fn failed_authentication_zeroizes_the_mutated_payload() {
        let key = derive_encryption_key(
            b"zeroize-password",
            RepositorySalt::from_bytes([0x44; REPOSITORY_ENCRYPTION_SALT_LENGTH]),
        )
        .expect("KDF should succeed");
        let nonce = [0x55; XCHACHA20_POLY1305_NONCE_LENGTH];
        let mut payload = b"authentication failure must not escape".to_vec();
        encrypt_in_place(&key, &nonce, b"aad", &mut payload).expect("encryption should succeed");
        let last = payload.len() - 1;
        payload[last] ^= 1;

        assert_eq!(
            decrypt_in_place(&key, &nonce, b"aad", &mut payload),
            Err(FormatError::AuthenticationFailure)
        );
        assert!(payload.iter().all(|byte| *byte == 0));
    }
}
