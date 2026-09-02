use gib::{
    CancellationToken, ChunkId, PackBuilder, PackConfiguration, PackEntryInput, PackReader,
    SdkError, SdkResult, SealedPack,
};
use std::collections::HashMap;
use std::io::{self, Write};

const BASIC_PACK_FIXTURE: &str = include_str!("../../../tests/fixtures/packs/v1/basic.pack.hex");

fn decode_hex(value: &str) -> Vec<u8> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            assert_eq!(pair.len(), 2);
            let text = std::str::from_utf8(pair).expect("fixture should be UTF-8");
            u8::from_str_radix(text, 16).expect("fixture should be hexadecimal")
        })
        .collect()
}

fn entry(seed: u8, length: usize) -> (ChunkId, Vec<u8>, PackEntryInput) {
    let payload = (0..length)
        .map(|index| {
            seed.wrapping_add(index as u8)
                .rotate_left((index % 7) as u32)
        })
        .collect::<Vec<_>>();
    let id = ChunkId::from_content(&payload);
    let input = PackEntryInput::new(id, payload.len() as u64, payload.clone())
        .expect("test entry should be valid");
    (id, payload, input)
}

fn indexed_tiny_entry(index: u32) -> (ChunkId, Vec<u8>, PackEntryInput) {
    let payload = vec![index as u8, (index >> 8) as u8, (index >> 16) as u8];
    let id = ChunkId::from_content(&payload);
    let input = PackEntryInput::new(id, payload.len() as u64, payload.clone())
        .expect("test entry should be valid");
    (id, payload, input)
}

fn build_one_pack(configuration: PackConfiguration) -> SealedPack {
    let (_, _, first) = entry(0x11, 5);
    let (_, _, second) = entry(0x22, 6);
    let mut builder = PackBuilder::new(configuration);
    assert!(
        builder
            .add(first)
            .expect("first entry should fit")
            .is_none()
    );
    assert!(
        builder
            .add(second)
            .expect("second entry should fit")
            .is_none()
    );
    builder
        .finish()
        .expect("pack should finish")
        .expect("pack should contain entries")
}

#[test]
fn version_1_pack_fixture_has_golden_bytes_and_id() {
    let configuration = PackConfiguration::new(512, 1_024).expect("configuration should be valid");
    let pack = build_one_pack(configuration);
    let expected = decode_hex(BASIC_PACK_FIXTURE);
    assert_eq!(pack.as_bytes(), expected.as_slice());
    assert_eq!(
        pack.id().as_hex(),
        "6ce51c94ee6dd9136e32c368896d1fddb5515007120dfb5f4d14f7bca808536b"
    );
    let reader = PackReader::new(&expected).expect("fixture should verify");
    assert_eq!(reader.metadata().entry_count(), 2);
    assert_eq!(reader.entries(), pack.entries());
}

#[test]
fn identical_ordered_inputs_have_identical_bytes_and_ids() {
    let configuration = PackConfiguration::new(512, 1_024).expect("configuration should be valid");
    let first = build_one_pack(configuration);
    let second = build_one_pack(configuration);
    assert_eq!(first.id(), second.id());
    assert_eq!(first.as_bytes(), second.as_bytes());
}

#[test]
fn empty_input_does_not_publish_an_empty_pack() {
    let configuration = PackConfiguration::new(512, 1_024).expect("configuration should be valid");
    let mut builder = PackBuilder::new(configuration);
    assert!(builder.finish().expect("finish should succeed").is_none());
}

#[test]
fn pack_configuration_rejects_invalid_limits() {
    assert!(PackConfiguration::new(0, 512).is_err());
    assert!(PackConfiguration::new(512, 0).is_err());
    assert!(PackConfiguration::new(513, 512).is_err());
    assert!(PackConfiguration::from_parts(99, 256, 512).is_err());
}

#[test]
fn exact_target_seals_before_the_next_entry() {
    let configuration = PackConfiguration::new(272, 512).expect("configuration should be valid");
    let (_, _, first) = entry(0x11, 5);
    let (_, _, second) = entry(0x22, 6);
    let mut builder = PackBuilder::new(configuration);
    assert!(
        builder
            .add(first)
            .expect("first entry should fit")
            .is_none()
    );
    let completed = builder
        .add(second)
        .expect("second entry should fit")
        .expect("first pack should be sealed at the target");
    assert_eq!(completed.len(), 272);
    assert_eq!(completed.metadata().total_length(), 272);
    assert_eq!(
        builder
            .finish()
            .expect("finish should succeed")
            .expect("second pack should be present")
            .metadata()
            .entry_count(),
        1
    );
}

#[test]
fn many_tiny_entries_are_published_without_retaining_completed_packs() -> SdkResult<()> {
    let configuration =
        PackConfiguration::new(512, 1_024).map_err(|_| SdkError::InvalidRequest {
            field: "test",
            reason: "test configuration should be valid",
        })?;
    let mut builder = PackBuilder::new(configuration);
    let mut expected = HashMap::new();
    let mut published = Vec::<(Vec<u8>, Vec<gib::PackEntryLocation>)>::new();
    let mut publisher = |pack: &SealedPack| {
        published.push((pack.as_bytes().to_vec(), pack.entries().to_vec()));
        Ok(())
    };
    for index in 0..4_096_u32 {
        let (id, payload, input) = indexed_tiny_entry(index);
        expected.insert(id, payload);
        builder.add_to(input, &mut publisher)?;
    }
    builder.finish_to(&mut publisher)?;
    assert!(published.len() > 1);
    let smallest = published.iter().map(|(bytes, _)| bytes.len()).min();
    let largest = published.iter().map(|(bytes, _)| bytes.len()).max();
    println!(
        "tiny pack QA: packs={} entries={} min_bytes={:?} max_bytes={:?}",
        published.len(),
        expected.len(),
        smallest,
        largest
    );

    let mut seen = 0_usize;
    for (bytes, locations) in &published {
        let reader = PackReader::new(bytes)?;
        assert_eq!(reader.entries(), locations.as_slice());
        for pair in locations.windows(2) {
            assert!(pair[0].end_offset() <= pair[1].entry_offset());
        }
        for location in locations {
            let payload = reader.payload(location)?;
            assert_eq!(
                payload,
                expected.get(&location.chunk_id()).expect("source entry")
            );
            seen += 1;
        }
    }
    assert_eq!(seen, expected.len());
    Ok(())
}

#[test]
fn oversized_single_entry_is_allowed_but_a_second_entry_is_not_added() {
    let configuration = PackConfiguration::new(256, 512).expect("configuration should be valid");
    let (_, payload, input) = entry(0x55, 400);
    let (_, _, second) = entry(0x66, 400);
    let mut builder = PackBuilder::new(configuration);
    assert!(
        builder
            .add(input)
            .expect("oversized entry should be accepted")
            .is_none()
    );
    let first = builder
        .add(second)
        .expect("second entry should fit in its own pack")
        .expect("oversized first pack should be sealed");
    assert_eq!(first.metadata().entry_count(), 1);
    assert!(first.metadata().is_oversized_single_entry());
    assert_eq!(first.entries()[0].payload_length(), payload.len() as u64);
    let second = builder
        .finish()
        .expect("finish should succeed")
        .expect("second pack should be present");
    assert_eq!(second.metadata().entry_count(), 1);
    assert!(second.metadata().is_oversized_single_entry());
}

#[test]
fn corrupt_truncated_and_trailing_packs_are_rejected() {
    let pack = build_one_pack(PackConfiguration::new(512, 1_024).expect("valid configuration"));
    let mut mutations = Vec::new();

    let mut header = pack.as_bytes().to_vec();
    header[0] ^= 1;
    mutations.push(header);

    let mut payload = pack.as_bytes().to_vec();
    payload[PACK_PAYLOAD_OFFSET] ^= 1;
    mutations.push(payload);

    let mut footer = pack.as_bytes().to_vec();
    let last = footer.len() - 1;
    footer[last] ^= 1;
    mutations.push(footer);

    mutations.push(pack.as_bytes()[..pack.len() - 1].to_vec());
    let mut trailing = pack.as_bytes().to_vec();
    trailing.push(0);
    mutations.push(trailing);

    for bytes in mutations {
        assert!(matches!(
            PackReader::new(&bytes),
            Err(SdkError::RepositoryMalformed { .. })
        ));
    }
}

#[test]
fn length_and_critical_flag_corruption_are_rejected() {
    let pack = build_one_pack(PackConfiguration::new(512, 1_024).expect("valid configuration"));

    let mut header_length = pack.as_bytes().to_vec();
    header_length[8..12].copy_from_slice(&63_u32.to_be_bytes());
    assert!(matches!(
        PackReader::new(&header_length),
        Err(SdkError::RepositoryMalformed { .. })
    ));

    let mut entry_length = pack.as_bytes().to_vec();
    entry_length[PACK_HEADER_LENGTH + 8..PACK_HEADER_LENGTH + 16]
        .copy_from_slice(&96_u64.to_be_bytes());
    assert!(matches!(
        PackReader::new(&entry_length),
        Err(SdkError::RepositoryMalformed { .. })
    ));

    let mut flags = pack.as_bytes().to_vec();
    flags[6..8].copy_from_slice(&1_u16.to_be_bytes());
    assert!(matches!(
        PackReader::new(&flags),
        Err(SdkError::RepositoryMalformed { .. })
    ));
}

#[test]
fn unsupported_header_and_entry_versions_never_fallback() {
    let pack = build_one_pack(PackConfiguration::new(512, 1_024).expect("valid configuration"));
    let mut header = pack.as_bytes().to_vec();
    header[4..6].copy_from_slice(&99_u16.to_be_bytes());
    assert!(matches!(
        PackReader::new(&header),
        Err(SdkError::RepositoryUnsupportedVersion { version: 99 })
    ));

    let mut entry = pack.as_bytes().to_vec();
    entry[PACK_HEADER_LENGTH + 4..PACK_HEADER_LENGTH + 6].copy_from_slice(&99_u16.to_be_bytes());
    assert!(matches!(
        PackReader::new(&entry),
        Err(SdkError::RepositoryMalformed { .. })
    ));
}

#[test]
fn a_failed_writer_aborts_the_builder() {
    let pack = build_one_pack(PackConfiguration::new(512, 1_024).expect("valid configuration"));
    let mut writer = FailingWriter;
    assert_eq!(
        pack.write_to(&mut writer),
        Err(SdkError::RepositoryPackWriteFailed)
    );

    let configuration = PackConfiguration::new(272, 512).expect("configuration should be valid");
    let (_, _, first) = entry(1, 5);
    let (_, _, second) = entry(2, 5);
    let mut builder = PackBuilder::new(configuration);
    builder.add(first).expect("first entry should fit");
    let mut publisher = |_pack: &SealedPack| Err(SdkError::RepositoryPackWriteFailed);
    assert_eq!(
        builder.add_to(second, &mut publisher),
        Err(SdkError::RepositoryPackWriteFailed)
    );
    assert!(matches!(
        builder.finish(),
        Err(SdkError::InvalidRequest {
            field: "pack_builder",
            ..
        })
    ));
}

#[test]
fn cancellation_stops_streaming_without_final_publication() {
    let configuration = PackConfiguration::new(272, 512).expect("configuration should be valid");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut builder = PackBuilder::new(configuration);
    let (_, _, input) = entry(1, 5);
    assert!(matches!(
        builder.add_with_cancellation(input, &cancellation),
        Err(SdkError::OperationCancelled { operation_id: None })
    ));
    assert!(matches!(
        builder.finish(),
        Err(SdkError::InvalidRequest {
            field: "pack_builder",
            ..
        })
    ));
}

#[test]
fn cancellation_during_streaming_drops_the_current_unpublished_pack() {
    let configuration = PackConfiguration::new(272, 512).expect("configuration should be valid");
    let cancellation = CancellationToken::new();
    let cancellation_for_publisher = cancellation.clone();
    let mut published = 0_usize;
    let mut publisher = |_pack: &SealedPack| {
        published += 1;
        cancellation_for_publisher.cancel();
        Ok(())
    };
    let entries = (0..12_u8).map(|seed| entry(seed, 5).2);
    let mut builder = PackBuilder::new(configuration);
    assert!(matches!(
        builder.add_stream(entries, &mut publisher, &cancellation),
        Err(SdkError::OperationCancelled { operation_id: None })
    ));
    assert_eq!(published, 1);
    assert!(matches!(
        builder.finish(),
        Err(SdkError::InvalidRequest {
            field: "pack_builder",
            ..
        })
    ));
}

#[test]
fn property_style_round_trips_preserve_non_overlapping_ranges() -> SdkResult<()> {
    let configuration =
        PackConfiguration::new(512, 1_024).map_err(|_| SdkError::InvalidRequest {
            field: "test",
            reason: "test configuration should be valid",
        })?;
    for seed in 0..16_u8 {
        let mut builder = PackBuilder::new(configuration);
        let mut packs = Vec::new();
        for length in [0_usize, 1, 3, 7, 15, 31, 63, 97] {
            let (_, _, input) = entry(seed.wrapping_add(length as u8), length);
            if let Some(pack) = builder.add(input)? {
                packs.push(pack);
            }
        }
        if let Some(pack) = builder.finish()? {
            packs.push(pack);
        }
        for pack in packs {
            let reader = PackReader::new(pack.as_bytes())?;
            for pair in reader.entries().windows(2) {
                assert!(pair[0].end_offset() <= pair[1].entry_offset());
            }
            for location in reader.entries() {
                assert_eq!(
                    reader.payload(location)?.len() as u64,
                    location.payload_length()
                );
            }
        }
    }
    Ok(())
}

const PACK_HEADER_LENGTH: usize = 64;
const PACK_PAYLOAD_OFFSET: usize = PACK_HEADER_LENGTH + 96;

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test writer failure",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
