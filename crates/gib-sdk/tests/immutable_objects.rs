use gib::{
    CURRENT_TREE_OBJECT_VERSION, MAX_IMMUTABLE_OBJECT_BYTES, ObjectCodec, ObjectEncryption,
    ObjectKind, SdkError, decode_immutable_object, decode_immutable_object_from_reader,
    decode_snapshot_object, encode_immutable_object, encode_object,
};
use std::error::Error;
use std::io::Cursor;

const TREE_OBJECT_ID: &str = "c740fdadc5c20672e5a77b27f10e71b119606dee4f2cb45267e416df3d0e0063";
const SNAPSHOT_OBJECT_ID: &str = "33ddee3a503f8088923ceba97fd979e83309ccb93d706a836029262e4574fd52";

fn decode_hex_fixture(value: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !value.len().is_multiple_of(2) {
        return Err("fixture hex has an odd number of digits".into());
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for index in 0..(value.len() / 2) {
        let offset = index * 2;
        let pair = &value.as_bytes()[offset..offset + 2];
        let pair = std::str::from_utf8(pair)?;
        bytes.push(u8::from_str_radix(pair, 16)?);
    }
    Ok(bytes)
}

#[test]
fn version_1_tree_fixture_has_golden_bytes_and_id() -> Result<(), Box<dyn Error>> {
    let bytes = decode_hex_fixture(include_str!(
        "../../../tests/fixtures/repository/v1/objects/tree-envelope.hex"
    ))?;
    let payload = b"canonical tree payload";
    let object = decode_immutable_object(&bytes)?;

    assert_eq!(object.kind(), ObjectKind::Tree);
    assert_eq!(object.version(), CURRENT_TREE_OBJECT_VERSION);
    assert_eq!(object.codec(), ObjectCodec::None);
    assert_eq!(object.encryption(), ObjectEncryption::None);
    assert_eq!(object.payload(), payload);
    assert_eq!(object.object_id().as_str(), TREE_OBJECT_ID);
    assert_eq!(
        encode_object(ObjectKind::Tree, CURRENT_TREE_OBJECT_VERSION, payload)?,
        bytes
    );
    Ok(())
}

#[test]
fn version_1_snapshot_fixtures_preserve_legacy_decode_compatibility() -> Result<(), Box<dyn Error>>
{
    let enveloped = decode_hex_fixture(include_str!(
        "../../../tests/fixtures/repository/v1/objects/snapshot-envelope.hex"
    ))?;
    let legacy = decode_hex_fixture(include_str!(
        "../../../tests/fixtures/repository/v1/objects/snapshot-legacy.hex"
    ))?;

    let enveloped_snapshot = decode_snapshot_object(&enveloped)?;
    let legacy_snapshot = decode_snapshot_object(&legacy)?;
    assert_eq!(enveloped_snapshot, legacy_snapshot);
    assert_eq!(enveloped_snapshot.id().as_str(), "fixture-snapshot");
    assert_eq!(
        enveloped_snapshot.parent().map(|id| id.as_str()),
        Some("fixture-parent")
    );
    assert_eq!(enveloped_snapshot.file_count(), 3);
    assert_eq!(enveloped_snapshot.directory_count(), 2);
    assert_eq!(enveloped_snapshot.total_size(), 1_024);
    assert_eq!(enveloped_snapshot.object_id()?.as_str(), SNAPSHOT_OBJECT_ID);
    assert_eq!(enveloped_snapshot.to_bytes()?, enveloped);
    Ok(())
}

#[test]
fn identical_logical_objects_have_identical_ids_and_bytes() -> Result<(), Box<dyn Error>> {
    let first = encode_immutable_object(
        ObjectKind::Pack,
        1,
        ObjectCodec::None,
        ObjectEncryption::None,
        b"same logical pack",
    )?;
    let second = encode_immutable_object(
        ObjectKind::Pack,
        1,
        ObjectCodec::None,
        ObjectEncryption::None,
        b"same logical pack",
    )?;
    assert_eq!(first, second);
    assert_eq!(
        decode_immutable_object(&first)?,
        decode_immutable_object(&second)?
    );
    Ok(())
}

#[test]
fn the_reader_rejects_an_object_that_exceeds_the_bound() {
    let bytes = vec![0u8; MAX_IMMUTABLE_OBJECT_BYTES + 1];
    let error = decode_immutable_object_from_reader(Cursor::new(bytes));
    assert_eq!(
        error,
        Err(SdkError::RepositoryMalformed {
            reason: "immutable object exceeds the supported size limit",
        })
    );
}
