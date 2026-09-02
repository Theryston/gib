use gib::{
    CompressionLevel, MemoryStorage, ObjectCodec, ObjectEncryption, ObjectKey, ObjectMetadata,
    ObjectRange, ObjectRead, PackBuilder, PackConfiguration, PackEntryInput, PackId,
    PackIndexCache, PackIndexCacheConfiguration, PackIndexConfiguration, PackIndexEntry,
    PackIndexLookup, PackIndexReader, PackIndexShardBuilder, PackIndexShardId,
    PackIndexStoragePublisher, PackIndexTransform, RepositoryStorage, SdkError, SdkResult,
    SealedPack, StorageError,
};
use std::io::Read;
use std::sync::{Arc, Mutex};

fn plain_transform() -> PackIndexTransform {
    PackIndexTransform::plain(1).expect("plain transform should be valid")
}

fn transformed_transform() -> PackIndexTransform {
    PackIndexTransform::new(
        2,
        1,
        gib::ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::XChaCha20Poly1305)
            .with_compression_level(CompressionLevel::new(7).expect("level should be valid")),
    )
    .expect("transformed metadata should be valid")
}

fn index_entry(
    shard: u8,
    discriminator: u8,
    pack_discriminator: u8,
    offset: u64,
    stored_length: u64,
    logical_length: u64,
    transform: PackIndexTransform,
) -> PackIndexEntry {
    let chunk_id = gib::ChunkId::from_digest({
        let mut bytes = [0_u8; 32];
        bytes[0] = shard;
        bytes[1] = discriminator;
        bytes[31] = discriminator.wrapping_mul(3);
        bytes
    });
    let pack_id = PackId::from_digest([pack_discriminator; 32]);
    let entry_length = (96_u64 + stored_length).div_ceil(8) * 8;
    PackIndexEntry::new(
        chunk_id,
        pack_id,
        offset,
        offset + 96,
        entry_length,
        stored_length,
        logical_length,
        transform,
    )
    .expect("index entry should be valid")
}

fn shard(entries: impl IntoIterator<Item = PackIndexEntry>) -> gib::SealedPackIndexShard {
    let entries: Vec<PackIndexEntry> = entries.into_iter().collect();
    let shard_id = PackIndexShardId::from_chunk_id(entries[0].chunk_id());
    let mut builder = PackIndexShardBuilder::new(PackIndexConfiguration::default(), shard_id)
        .expect("index builder should be valid");
    for entry in entries {
        builder.add(entry).expect("entry should be accepted");
    }
    builder.finish().expect("index shard should finish")
}

fn decode_hex(value: &str) -> Vec<u8> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(
                std::str::from_utf8(pair).expect("fixture should be UTF-8"),
                16,
            )
            .expect("fixture should be hexadecimal")
        })
        .collect()
}

#[test]
fn version_1_index_fixture_has_golden_bytes_and_id() {
    let first = index_entry(0x10, 0x11, 0xa1, 64, 5, 5, plain_transform());
    let second = index_entry(0x10, 0x22, 0xa2, 168, 7, 7, transformed_transform());
    let index = shard([first, second]);
    let expected = decode_hex(include_str!(
        "../../../tests/fixtures/pack-indexes/v1/basic.index.hex"
    ));
    assert_eq!(index.as_bytes(), expected.as_slice());
    assert_eq!(
        index.id().as_hex(),
        "4c0b3fd0b9aa210f1982f926f5fa0ad81e527aeb27c9df60cfc74c1eeab0d8bf"
    );
    let reader = PackIndexReader::new(&expected).expect("index should verify");
    assert_eq!(reader.entries(), index.entries());
    assert_eq!(reader.metadata().entry_count(), 2);
    assert_eq!(reader.metadata().shard_id().as_byte(), 0x10);
}

#[test]
fn index_records_are_sorted_and_binary_search_finds_boundaries() {
    let entries = [
        index_entry(0x20, 0x90, 0x91, 64, 1, 1, plain_transform()),
        index_entry(0x20, 0x10, 0x92, 176, 2, 2, plain_transform()),
        index_entry(0x20, 0x50, 0x93, 288, 3, 3, plain_transform()),
    ];
    let index = shard(entries);
    let reader = PackIndexReader::new(index.as_bytes()).expect("index should verify");
    assert_eq!(reader.entries()[0].chunk_id(), entries[1].chunk_id());
    assert_eq!(reader.entries()[1].chunk_id(), entries[2].chunk_id());
    assert_eq!(reader.entries()[2].chunk_id(), entries[0].chunk_id());
    assert_eq!(reader.lookup(entries[0].chunk_id()), Some(entries[0]));
    assert_eq!(reader.lookup(entries[1].chunk_id()), Some(entries[1]));
    assert_eq!(reader.lookup(entries[2].chunk_id()), Some(entries[2]));
    let missing = gib::ChunkId::from_digest([0x20; 32]);
    assert_eq!(reader.lookup(missing), None);
}

#[test]
fn duplicate_entries_are_rejected_before_publication() {
    let entry = index_entry(0x30, 1, 1, 64, 1, 1, plain_transform());
    let mut builder = PackIndexShardBuilder::new(
        PackIndexConfiguration::default(),
        PackIndexShardId::from_byte(0x30),
    )
    .expect("index builder should be valid");
    builder.add(entry).expect("first entry should fit");
    builder
        .add(entry)
        .expect("duplicate is detected at sealing");
    assert_eq!(
        builder.finish().err(),
        Some(SdkError::RepositoryMalformed {
            reason: "pack-index shard contains a duplicate chunk ID",
        })
    );
}

#[test]
fn invalid_offsets_lengths_and_shards_are_rejected() {
    let chunk_id = gib::ChunkId::from_digest([0x41; 32]);
    let pack_id = PackId::from_digest([0x42; 32]);
    assert!(
        PackIndexEntry::new(
            chunk_id,
            pack_id,
            u64::MAX,
            u64::MAX,
            104,
            1,
            1,
            plain_transform(),
        )
        .is_err()
    );
    assert!(
        PackIndexEntry::new(chunk_id, pack_id, 64, 160, 105, 1, 1, plain_transform(),).is_err()
    );
    let entry = index_entry(0x41, 1, 1, 64, 1, 1, plain_transform());
    let mut builder = PackIndexShardBuilder::new(
        PackIndexConfiguration::default(),
        PackIndexShardId::from_byte(0x40),
    )
    .expect("index builder should be valid");
    assert_eq!(
        builder.add(entry),
        Err(SdkError::InvalidRequest {
            field: "pack_index_shard",
            reason: "entry does not belong to the selected chunk-ID shard",
        })
    );
}

#[test]
fn corruption_truncation_trailing_and_unknown_versions_are_rejected() {
    let index = shard([index_entry(0x50, 1, 1, 64, 1, 1, plain_transform())]);
    let mut header_corruption = index.as_bytes().to_vec();
    header_corruption[6] = 1;
    assert!(PackIndexReader::new(&header_corruption).is_err());
    let mut payload_corruption = index.as_bytes().to_vec();
    payload_corruption[64] ^= 1;
    assert!(PackIndexReader::new(&payload_corruption).is_err());
    assert!(PackIndexReader::new(&index.as_bytes()[..index.len() - 1]).is_err());
    let mut trailing = index.as_bytes().to_vec();
    trailing.push(0);
    assert!(PackIndexReader::new(&trailing).is_err());
    let mut unsupported = index.as_bytes().to_vec();
    unsupported[4..6].copy_from_slice(&2_u16.to_be_bytes());
    assert_eq!(
        PackIndexReader::new(&unsupported).err(),
        Some(SdkError::RepositoryUnsupportedVersion { version: 2 })
    );
}

#[test]
fn cache_evicts_by_shard_and_respects_the_memory_budget() {
    let first = shard([index_entry(0x60, 1, 1, 64, 1, 1, plain_transform())]);
    let second = shard([index_entry(0x61, 2, 2, 64, 1, 1, plain_transform())]);
    let configuration = PackIndexCacheConfiguration::new(first.len() * 2, 1)
        .expect("cache configuration should be valid");
    let mut cache = PackIndexCache::new(configuration);
    cache.insert(&first).expect("first shard should cache");
    assert_eq!(cache.len(), 1);
    assert!(cache.lookup(first.entries()[0].chunk_id()).is_some());
    cache.insert(&second).expect("second shard should cache");
    assert_eq!(cache.len(), 1);
    assert!(cache.lookup(first.entries()[0].chunk_id()).is_none());
    assert!(cache.lookup(second.entries()[0].chunk_id()).is_some());
    assert!(cache.resident_bytes() <= configuration.max_bytes());
}

#[test]
fn all_shards_can_be_built_independently() {
    let configuration = PackIndexConfiguration::default();
    let mut total_bytes = 0_u64;
    for raw_shard in 0..=u8::MAX {
        let builder =
            PackIndexShardBuilder::new(configuration, PackIndexShardId::from_byte(raw_shard))
                .expect("empty shard builder should be valid");
        let mut builder = builder;
        let shard = builder.finish().expect("empty shard should finish");
        assert_eq!(shard.metadata().shard_id().as_byte(), raw_shard);
        total_bytes += shard.len() as u64;
    }
    assert_eq!(total_bytes, 256 * (64 + 96));
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ReadCounts {
    streams: usize,
    metadata: usize,
    ranges: usize,
}

#[derive(Clone)]
struct CountingStorage {
    inner: MemoryStorage,
    counts: Arc<Mutex<ReadCounts>>,
}

impl CountingStorage {
    fn new(inner: MemoryStorage) -> Self {
        Self {
            inner,
            counts: Arc::new(Mutex::new(ReadCounts::default())),
        }
    }

    fn counts(&self) -> ReadCounts {
        self.counts
            .lock()
            .expect("counts lock should not fail")
            .to_owned()
    }
}

impl RepositoryStorage for CountingStorage {
    fn read_stream(&self, object_key: &ObjectKey) -> Result<ObjectRead, StorageError> {
        self.counts
            .lock()
            .expect("counts lock should not fail")
            .streams += 1;
        self.inner.read_stream(object_key)
    }

    fn metadata(&self, object_key: &ObjectKey) -> Result<ObjectMetadata, StorageError> {
        self.counts
            .lock()
            .expect("counts lock should not fail")
            .metadata += 1;
        self.inner.metadata(object_key)
    }

    fn read_range(
        &self,
        object_key: &ObjectKey,
        range: ObjectRange,
    ) -> Result<ObjectRead, StorageError> {
        self.counts
            .lock()
            .expect("counts lock should not fail")
            .ranges += 1;
        self.inner.read_range(object_key, range)
    }
}

fn create_pack(discriminator: u8, payload: Vec<u8>) -> SealedPack {
    let chunk_id = gib::ChunkId::from_digest({
        let mut bytes = [0_u8; 32];
        bytes[0] = 0x70;
        bytes[1] = discriminator;
        bytes
    });
    let input = PackEntryInput::new(chunk_id, payload.len() as u64, payload)
        .expect("pack entry should be valid");
    let mut builder = PackBuilder::new(
        PackConfiguration::new(512, 1_024).expect("pack configuration should be valid"),
    );
    builder.add(input).expect("pack entry should be accepted");
    builder
        .finish()
        .expect("pack should finish")
        .expect("pack should not be empty")
}

fn store_pack(storage: &MemoryStorage, pack: &SealedPack) {
    storage
        .put(
            format!("packs/{}", pack.id().as_hex()).as_str(),
            pack.as_bytes(),
        )
        .expect("pack should be stored");
}

#[test]
fn lookup_reads_one_shard_then_exact_pack_ranges() -> SdkResult<()> {
    let packs = vec![
        create_pack(1, b"first payload".to_vec()),
        create_pack(2, b"middle payload".to_vec()),
        create_pack(3, b"last payload".to_vec()),
    ];
    let storage = MemoryStorage::new();
    for pack in &packs {
        store_pack(&storage, pack);
    }
    let transform = plain_transform();
    let mut builder = PackIndexShardBuilder::new(
        PackIndexConfiguration::default(),
        PackIndexShardId::from_byte(0x70),
    )?;
    for pack in &packs {
        builder.add_pack(pack, transform)?;
    }
    let index = builder.finish()?;
    storage
        .put(
            gib::pack_index_storage_key(PackIndexShardId::from_byte(0x70)).as_str(),
            index.as_bytes(),
        )
        .expect("index should be stored");
    let counting = CountingStorage::new(storage);
    let mut lookup = PackIndexLookup::new(
        counting.clone(),
        PackIndexCacheConfiguration::new(4 * 1024 * 1024, 4)
            .expect("cache configuration should be valid"),
    );

    for (pack, expected) in packs.iter().zip([
        b"first payload".as_slice(),
        b"middle payload".as_slice(),
        b"last payload".as_slice(),
    ]) {
        let chunk_id = pack.entries()[0].chunk_id();
        let entry = lookup.lookup(chunk_id)?.expect("chunk should be indexed");
        assert_eq!(entry.pack_id(), pack.id());
        let mut read = lookup
            .read_chunk(chunk_id)?
            .expect("chunk range should be readable");
        assert_eq!(read.range().offset(), entry.payload_offset());
        assert_eq!(read.range().length(), entry.stored_length());
        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes)
            .map_err(|_| SdkError::StorageFailure {
                operation: "read_pack_range",
            })?;
        assert_eq!(bytes, expected);
    }
    let counts = counting.counts();
    assert_eq!(counts.streams, 1);
    assert_eq!(counts.metadata, 3);
    assert_eq!(counts.ranges, 3);
    assert_eq!(lookup.cache().len(), 1);
    Ok(())
}

#[test]
fn missing_chunks_and_packs_are_distinguished() -> SdkResult<()> {
    let pack = create_pack(9, b"payload".to_vec());
    let storage = MemoryStorage::new();
    let mut builder = PackIndexShardBuilder::new(
        PackIndexConfiguration::default(),
        PackIndexShardId::from_byte(0x70),
    )?;
    builder.add_pack(&pack, plain_transform())?;
    let index = builder.finish()?;
    storage
        .put(
            gib::pack_index_storage_key(PackIndexShardId::from_byte(0x70)).as_str(),
            index.as_bytes(),
        )
        .expect("index should be stored");
    let mut lookup = PackIndexLookup::with_defaults(storage.clone());
    let absent = gib::ChunkId::from_digest([0x70; 32]);
    assert_eq!(lookup.lookup(absent)?, None);
    assert_eq!(lookup.lookup(gib::ChunkId::from_digest([0x71; 32]))?, None);
    assert_eq!(
        lookup.read_chunk(pack.entries()[0].chunk_id()).err(),
        Some(SdkError::RepositoryRequiredObjectMissing)
    );
    Ok(())
}

#[test]
fn invalid_pack_range_is_rejected_before_range_read() -> SdkResult<()> {
    let pack = create_pack(10, b"payload".to_vec());
    let storage = MemoryStorage::new();
    let entry = PackIndexEntry::new(
        pack.entries()[0].chunk_id(),
        pack.id(),
        1_024,
        1_120,
        104,
        1,
        1,
        plain_transform(),
    )
    .expect("corrupt range record should still be a valid index record");
    let mut builder = PackIndexShardBuilder::new(
        PackIndexConfiguration::default(),
        PackIndexShardId::from_byte(0x70),
    )?;
    builder.add(entry)?;
    let index = builder.finish()?;
    storage
        .put(
            gib::pack_index_storage_key(PackIndexShardId::from_byte(0x70)).as_str(),
            index.as_bytes(),
        )
        .expect("index should be stored");
    store_pack(&storage, &pack);
    let counting = CountingStorage::new(storage);
    let mut lookup = PackIndexLookup::with_defaults(counting.clone());
    assert_eq!(
        lookup.read_chunk(pack.entries()[0].chunk_id()).err(),
        Some(SdkError::RepositoryMalformed {
            reason: "pack-index range exceeds the containing pack",
        })
    );
    assert_eq!(lookup.cache().len(), 1);
    let counts = counting.counts();
    assert_eq!(counts.streams, 1);
    assert_eq!(counts.metadata, 1);
    assert_eq!(counts.ranges, 0);
    Ok(())
}

#[test]
fn storage_publisher_uses_immutable_index_ids() -> SdkResult<()> {
    let index = shard([index_entry(0x75, 1, 1, 64, 1, 1, plain_transform())]);
    let storage = MemoryStorage::new();
    let mut publisher = PackIndexStoragePublisher::new(storage.clone());
    gib::PackIndexPublisher::publish(&mut publisher, &index)?;
    let key = gib::pack_index_object_key(index.id());
    assert_eq!(
        storage.objects().expect("objects should be readable"),
        vec![key.clone()]
    );
    assert_eq!(
        storage.read_object(&key).expect("index should be readable"),
        index.as_bytes()
    );
    assert_eq!(
        gib::PackIndexPublisher::publish(&mut publisher, &index).err(),
        Some(SdkError::RepositoryPublicationConflict)
    );
    Ok(())
}

#[test]
fn immutable_publication_keys_keep_generations_separate() -> SdkResult<()> {
    let first = shard([index_entry(0x80, 1, 1, 64, 1, 1, plain_transform())]);
    let second = shard([index_entry(0x80, 2, 2, 64, 1, 1, plain_transform())]);
    assert_ne!(first.id(), second.id());
    let first_key = ObjectKey::new(gib::pack_index_object_key(first.id()))
        .expect("first immutable index key should be valid");
    let second_key = ObjectKey::new(gib::pack_index_object_key(second.id()))
        .expect("second immutable index key should be valid");
    let storage = MemoryStorage::new();
    storage
        .put(first_key.as_str(), first.as_bytes())
        .expect("first index");
    storage
        .put(second_key.as_str(), second.as_bytes())
        .expect("second index");
    let mut lookup = PackIndexLookup::with_defaults(storage);
    assert!(
        lookup
            .lookup_at(first.entries()[0].chunk_id(), &first_key)?
            .is_some()
    );
    assert!(
        lookup
            .lookup_at(second.entries()[0].chunk_id(), &second_key)?
            .is_some()
    );
    assert_eq!(lookup.cache().len(), 2);
    Ok(())
}

#[test]
fn range_validation_catches_overflow() {
    let range = gib::PackIndexRange::new(u64::MAX, 1);
    assert!(range.is_err());
}
