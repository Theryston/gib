use gib::{
    CompressionLevel, ObjectCodec, ObjectEncryption, ObjectKind, ObjectTransformOptions,
    RepositoryEncryption, RepositorySalt, SdkError, decode_immutable_object,
    decode_immutable_object_from_reader_with_encryption,
    decode_immutable_object_from_reader_with_password, decode_immutable_object_with_encryption,
    decode_immutable_object_with_password, encode_immutable_object,
    encode_immutable_object_with_options,
};
use std::io::Cursor;

fn encrypted_options() -> ObjectTransformOptions {
    ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::XChaCha20Poly1305)
        .with_compression_level(CompressionLevel::new(3).unwrap_or_default())
}

#[test]
fn zstd_objects_round_trip_and_keep_the_plaintext_identity() {
    let payload = b"repeated payload repeated payload repeated payload";
    let bytes = encode_immutable_object(
        ObjectKind::Pack,
        1,
        ObjectCodec::Zstd,
        ObjectEncryption::None,
        payload,
    )
    .expect("zstd object should encode");
    let object = decode_immutable_object(&bytes).expect("zstd object should decode");
    assert_eq!(object.codec(), ObjectCodec::Zstd);
    assert_eq!(object.encryption(), ObjectEncryption::None);
    assert_eq!(object.payload(), payload);
    assert_eq!(object.plaintext_length(), payload.len() as u64);
    assert!(object.payload_length() < object.plaintext_length());
}

#[test]
fn encrypted_objects_round_trip_with_context_and_password() {
    let payload = vec![0x5au8; 32 * 1024];
    let context = RepositoryEncryption::from_password(
        b"correct horse battery staple",
        RepositorySalt::from_bytes([9u8; 16]),
    )
    .expect("test key should derive");
    let bytes = encode_immutable_object_with_options(
        ObjectKind::Pack,
        1,
        encrypted_options(),
        Some(&context),
        &payload,
    )
    .expect("encrypted object should encode");

    assert_eq!(
        decode_immutable_object(&bytes),
        Err(SdkError::RepositoryEncryptionKeyRequired)
    );
    let decoded = decode_immutable_object_with_encryption(&bytes, &context)
        .expect("context should decrypt object");
    assert_eq!(decoded.payload(), payload);
    assert_eq!(decoded.codec(), ObjectCodec::Zstd);
    assert_eq!(decoded.encryption(), ObjectEncryption::XChaCha20Poly1305);

    let decoded = decode_immutable_object_with_password(&bytes, b"correct horse battery staple")
        .expect("password should decrypt object");
    assert_eq!(decoded.payload(), payload);
    assert_eq!(
        decode_immutable_object_with_password(&bytes, b"wrong password"),
        Err(SdkError::RepositoryAuthenticationFailed)
    );
}

#[test]
fn encrypted_reader_decoders_round_trip_without_unbounded_reads() {
    let password = b"reader password";
    let context =
        RepositoryEncryption::from_password(password, RepositorySalt::from_bytes([0x2au8; 16]))
            .expect("test key should derive");
    let payload = vec![0x91u8; 64 * 1024];
    let bytes = encode_immutable_object_with_options(
        ObjectKind::Pack,
        1,
        encrypted_options(),
        Some(&context),
        &payload,
    )
    .expect("encrypted object should encode");

    let decoded = decode_immutable_object_from_reader_with_encryption(
        Cursor::new(bytes.as_slice()),
        &context,
    )
    .expect("encrypted reader should decode");
    assert_eq!(decoded.payload(), payload);

    let decoded =
        decode_immutable_object_from_reader_with_password(Cursor::new(bytes.as_slice()), password)
            .expect("password reader should decode");
    assert_eq!(decoded.payload(), payload);
}

#[test]
fn encryption_uses_a_fresh_nonce_for_each_encoding() {
    let context = RepositoryEncryption::from_password(
        b"nonce test password",
        RepositorySalt::from_bytes([4u8; 16]),
    )
    .expect("test key should derive");
    let first = encode_immutable_object_with_options(
        ObjectKind::Tree,
        1,
        encrypted_options(),
        Some(&context),
        b"same logical object",
    )
    .expect("first object should encode");
    let second = encode_immutable_object_with_options(
        ObjectKind::Tree,
        1,
        encrypted_options(),
        Some(&context),
        b"same logical object",
    )
    .expect("second object should encode");
    assert_ne!(first, second);
    assert_eq!(
        decode_immutable_object_with_encryption(&first, &context)
            .expect("first object should decode")
            .object_id(),
        decode_immutable_object_with_encryption(&second, &context)
            .expect("second object should decode")
            .object_id()
    );
}

#[test]
fn compressed_and_encrypted_corruption_never_returns_plaintext() {
    let context = RepositoryEncryption::from_password(
        b"corruption test password",
        RepositorySalt::from_bytes([8u8; 16]),
    )
    .expect("test key should derive");
    let bytes = encode_immutable_object_with_options(
        ObjectKind::Index,
        1,
        encrypted_options(),
        Some(&context),
        &vec![0x33u8; 16 * 1024],
    )
    .expect("object should encode");
    for index in [0, bytes.len() / 2, bytes.len() - 1] {
        let mut corrupted = bytes.clone();
        corrupted[index] ^= 1;
        assert!(decode_immutable_object_with_encryption(&corrupted, &context).is_err());
    }
}

#[test]
fn round_trips_edge_sizes_at_each_supported_compression_level() {
    let sizes = [0usize, 1, 15, 16, 257, 4 * 1024];
    for level in [1, 3, 22] {
        let level = CompressionLevel::new(level).expect("level should be valid");
        let options = ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::None)
            .with_compression_level(level);
        for size in sizes {
            let payload: Vec<u8> = (0..size)
                .map(|index| (index as u8).wrapping_mul(29))
                .collect();
            let bytes =
                encode_immutable_object_with_options(ObjectKind::Tree, 1, options, None, &payload)
                    .expect("compressed object should encode");
            let decoded = decode_immutable_object(&bytes).unwrap_or_else(|error| {
                panic!(
                    "compressed object should decode at level {} and size {size}: {error:?}",
                    level.value()
                )
            });
            assert_eq!(decoded.payload(), payload);
            assert_eq!(decoded.plaintext_length(), size as u64);
        }
    }
}

#[test]
fn invalid_compression_levels_are_rejected_before_encoding() {
    assert!(CompressionLevel::new(0).is_err());
    assert!(CompressionLevel::new(23).is_err());
    assert_eq!(
        CompressionLevel::new(1).expect("minimum is valid").value(),
        1
    );
    assert_eq!(
        CompressionLevel::new(22).expect("maximum is valid").value(),
        22
    );
}

#[test]
fn encryption_debug_output_redacts_password_derived_material() {
    let context = RepositoryEncryption::from_password(
        b"redaction-password",
        RepositorySalt::from_bytes([6u8; 16]),
    )
    .expect("test key should derive");
    let debug = format!("{context:?}");
    assert!(!debug.contains("redaction-password"));
    assert!(!debug.contains("06060606"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn version_2_encrypted_fixture_is_read_by_the_explicit_decoder() {
    let bytes = decode_hex_fixture(include_str!(
        "../../../tests/fixtures/repository/v2/objects/tree-encrypted-envelope.hex"
    ));
    let context = RepositoryEncryption::from_password(
        b"envelope-test-password",
        RepositorySalt::from_bytes([0xabu8; 16]),
    )
    .expect("fixture key should derive");
    let object = decode_immutable_object_with_encryption(&bytes, &context)
        .expect("version-2 fixture should decode");
    assert_eq!(object.kind(), ObjectKind::Tree);
    assert_eq!(object.codec(), ObjectCodec::Zstd);
    assert_eq!(object.encryption(), ObjectEncryption::XChaCha20Poly1305);
    assert_eq!(
        object.payload(),
        b"deterministic transformed envelope payload"
    );
}

fn decode_hex_fixture(value: &str) -> Vec<u8> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    (0..value.len() / 2)
        .map(|index| {
            u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .expect("fixture must contain hexadecimal bytes")
        })
        .collect()
}
