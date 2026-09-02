use crate::domain::{
    CURRENT_PACK_INDEX_FORMAT_VERSION, CompressionLevel, MAX_PACK_INDEX_SHARD_BYTES, ObjectCodec,
    ObjectEncryption, PACK_INDEX_ALIGNMENT, PACK_INDEX_FOOTER_LENGTH, PACK_INDEX_HEADER_LENGTH,
    PACK_INDEX_RECORD_LENGTH, PackIndexConfiguration, PackIndexEntry, PackIndexEntryError,
    PackIndexId, PackIndexShardId, PackIndexShardMetadata, PackIndexTransform,
    SealedPackIndexShard, entry_belongs_to_shard,
};
use sha2::{Digest, Sha256};

const INDEX_MAGIC: &[u8; 4] = b"GIXS";
const FOOTER_MAGIC: &[u8; 4] = b"GIXF";
const INDEX_FLAGS: u16 = 0;
const INDEX_ID_DOMAIN_SEPARATOR: &[u8] = b"GIB pack index identity\0";

/// A failure while building or validating the private pack-index format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackIndexFormatError {
    UnsupportedVersion { version: u16 },
    InvalidMagic,
    InvalidField,
    InvalidLength,
    InvalidChecksum,
    Truncated,
    TrailingData,
    DuplicateChunkId,
    WrongShard,
    UnsupportedCodec,
    UnsupportedEncryption,
    ShardTooLarge,
    AllocationFailure,
    BuilderFinished,
    BuilderAborted,
}

pub(crate) struct PackIndexShardBuilder {
    configuration: PackIndexConfiguration,
    shard_id: PackIndexShardId,
    entries: Vec<PackIndexEntry>,
    state: BuilderState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuilderState {
    Open,
    Finished,
    Aborted,
}

impl PackIndexShardBuilder {
    pub(crate) fn new(
        configuration: PackIndexConfiguration,
        shard_id: PackIndexShardId,
    ) -> Result<Self, PackIndexFormatError> {
        Ok(Self {
            configuration,
            shard_id,
            entries: Vec::new(),
            state: BuilderState::Open,
        })
    }

    pub(crate) const fn configuration(&self) -> PackIndexConfiguration {
        self.configuration
    }

    pub(crate) const fn shard_id(&self) -> PackIndexShardId {
        self.shard_id
    }

    pub(crate) fn add(&mut self, entry: PackIndexEntry) -> Result<(), PackIndexFormatError> {
        self.ensure_open()?;
        if !entry_belongs_to_shard(entry, self.shard_id) {
            return Err(PackIndexFormatError::WrongShard);
        }
        let next_count = self
            .entries
            .len()
            .checked_add(1)
            .ok_or(PackIndexFormatError::ShardTooLarge)?;
        let records_length = u64::try_from(next_count)
            .ok()
            .and_then(|count| count.checked_mul(PACK_INDEX_RECORD_LENGTH as u64))
            .ok_or(PackIndexFormatError::ShardTooLarge)?;
        let total_length = (PACK_INDEX_HEADER_LENGTH as u64)
            .checked_add(records_length)
            .and_then(|body| body.checked_add(PACK_INDEX_FOOTER_LENGTH as u64))
            .ok_or(PackIndexFormatError::ShardTooLarge)?;
        if total_length > self.configuration.max_shard_bytes()
            || total_length > MAX_PACK_INDEX_SHARD_BYTES
        {
            return Err(PackIndexFormatError::ShardTooLarge);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| PackIndexFormatError::AllocationFailure)?;
        self.entries.push(entry);
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<SealedPackIndexShard, PackIndexFormatError> {
        self.ensure_open()?;
        self.state = BuilderState::Finished;
        self.entries.sort_unstable_by_key(|entry| entry.chunk_id());
        if self
            .entries
            .windows(2)
            .any(|pair| pair[0].chunk_id() == pair[1].chunk_id())
        {
            return Err(PackIndexFormatError::DuplicateChunkId);
        }

        encode_shard(self.shard_id, self.configuration, &self.entries)
    }

    pub(crate) fn abort(&mut self) {
        self.entries.clear();
        self.state = BuilderState::Aborted;
    }

    fn ensure_open(&self) -> Result<(), PackIndexFormatError> {
        match self.state {
            BuilderState::Open => Ok(()),
            BuilderState::Finished => Err(PackIndexFormatError::BuilderFinished),
            BuilderState::Aborted => Err(PackIndexFormatError::BuilderAborted),
        }
    }
}

fn encode_shard(
    shard_id: PackIndexShardId,
    configuration: PackIndexConfiguration,
    entries: &[PackIndexEntry],
) -> Result<SealedPackIndexShard, PackIndexFormatError> {
    let entry_count =
        u64::try_from(entries.len()).map_err(|_| PackIndexFormatError::ShardTooLarge)?;
    let records_length = entry_count
        .checked_mul(PACK_INDEX_RECORD_LENGTH as u64)
        .ok_or(PackIndexFormatError::ShardTooLarge)?;
    let body_length = (PACK_INDEX_HEADER_LENGTH as u64)
        .checked_add(records_length)
        .ok_or(PackIndexFormatError::ShardTooLarge)?;
    let total_length = body_length
        .checked_add(PACK_INDEX_FOOTER_LENGTH as u64)
        .ok_or(PackIndexFormatError::ShardTooLarge)?;
    if total_length > configuration.max_shard_bytes() || total_length > MAX_PACK_INDEX_SHARD_BYTES {
        return Err(PackIndexFormatError::ShardTooLarge);
    }
    let total_length_usize =
        usize::try_from(total_length).map_err(|_| PackIndexFormatError::ShardTooLarge)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve(total_length_usize)
        .map_err(|_| PackIndexFormatError::AllocationFailure)?;
    let mut header = [0_u8; PACK_INDEX_HEADER_LENGTH];
    write_header(
        &mut header,
        configuration,
        shard_id,
        entry_count,
        records_length,
        body_length,
    );
    bytes.extend_from_slice(&header);
    for entry in entries {
        encode_entry(&mut bytes, *entry)?;
    }
    if u64::try_from(bytes.len()).map_err(|_| PackIndexFormatError::ShardTooLarge)? != body_length {
        return Err(PackIndexFormatError::InvalidLength);
    }
    let body_checksum = digest(&bytes);
    let index_id = index_id_for_body(configuration.version(), &bytes);
    let mut footer = [0_u8; PACK_INDEX_FOOTER_LENGTH];
    footer[0..4].copy_from_slice(FOOTER_MAGIC);
    put_u16(&mut footer, 4, configuration.version());
    put_u16(&mut footer, 6, INDEX_FLAGS);
    put_u32(&mut footer, 8, PACK_INDEX_FOOTER_LENGTH as u32);
    put_u64(&mut footer, 12, entry_count);
    put_u64(&mut footer, 20, body_length);
    footer[28..60].copy_from_slice(&body_checksum);
    footer[60..92].copy_from_slice(&index_id.as_bytes());
    bytes.extend_from_slice(&footer);

    let metadata = PackIndexShardMetadata::from_parts(
        shard_id,
        index_id,
        configuration.version(),
        entry_count,
        PACK_INDEX_HEADER_LENGTH as u64,
        records_length,
        body_length,
        total_length,
    );
    Ok(SealedPackIndexShard::new(
        index_id,
        bytes,
        metadata,
        entries.to_vec(),
    ))
}

fn encode_entry(bytes: &mut Vec<u8>, entry: PackIndexEntry) -> Result<(), PackIndexFormatError> {
    let mut record = [0_u8; PACK_INDEX_RECORD_LENGTH];
    record[0..32].copy_from_slice(&entry.chunk_id().as_bytes());
    record[32..64].copy_from_slice(&entry.pack_id().as_bytes());
    put_u64(&mut record, 64, entry.entry_offset());
    put_u64(&mut record, 72, entry.payload_offset());
    put_u64(&mut record, 80, entry.entry_length());
    put_u64(&mut record, 88, entry.stored_length());
    put_u64(&mut record, 96, entry.logical_length());
    let transform = entry.transform();
    put_u16(&mut record, 104, transform.envelope_version());
    put_u16(&mut record, 106, transform.object_version());
    record[108] = codec_tag(transform.codec())?;
    record[109] = encryption_tag(transform.encryption())?;
    put_i32(
        &mut record,
        112,
        if transform.codec() == ObjectCodec::None {
            0
        } else {
            transform.compression_level().value()
        },
    );
    bytes.extend_from_slice(&record);
    Ok(())
}

fn write_header(
    header: &mut [u8; PACK_INDEX_HEADER_LENGTH],
    configuration: PackIndexConfiguration,
    shard_id: PackIndexShardId,
    entry_count: u64,
    records_length: u64,
    body_length: u64,
) {
    header[0..4].copy_from_slice(INDEX_MAGIC);
    put_u16(header, 4, configuration.version());
    put_u16(header, 6, INDEX_FLAGS);
    put_u32(header, 8, PACK_INDEX_HEADER_LENGTH as u32);
    put_u32(header, 12, PACK_INDEX_ALIGNMENT as u32);
    header[16] = configuration.shard_prefix_bytes() as u8;
    header[17] = shard_id.as_byte();
    put_u16(header, 18, PACK_INDEX_RECORD_LENGTH as u16);
    put_u64(header, 20, entry_count);
    put_u64(header, 28, PACK_INDEX_HEADER_LENGTH as u64);
    put_u64(header, 36, records_length);
    put_u64(header, 44, body_length);
}

pub(crate) struct VerifiedPackIndexShard {
    metadata: PackIndexShardMetadata,
    entries: Vec<PackIndexEntry>,
    estimated_memory: usize,
}

impl VerifiedPackIndexShard {
    pub(crate) fn new(bytes: &[u8]) -> Result<Self, PackIndexFormatError> {
        let input_length =
            u64::try_from(bytes.len()).map_err(|_| PackIndexFormatError::ShardTooLarge)?;
        if input_length > MAX_PACK_INDEX_SHARD_BYTES {
            return Err(PackIndexFormatError::ShardTooLarge);
        }
        let minimum_length = PACK_INDEX_HEADER_LENGTH
            .checked_add(PACK_INDEX_FOOTER_LENGTH)
            .ok_or(PackIndexFormatError::ShardTooLarge)?;
        if bytes.len() < minimum_length {
            return Err(PackIndexFormatError::Truncated);
        }
        let header = parse_header(bytes)?;
        let footer_offset = bytes
            .len()
            .checked_sub(PACK_INDEX_FOOTER_LENGTH)
            .ok_or(PackIndexFormatError::Truncated)?;
        let footer = parse_footer(bytes, footer_offset)?;
        if footer.version != header.version {
            return Err(PackIndexFormatError::InvalidField);
        }
        if footer.entry_count != header.entry_count || footer.body_length != header.body_length {
            return Err(PackIndexFormatError::InvalidLength);
        }
        if footer.body_length
            != u64::try_from(footer_offset).map_err(|_| PackIndexFormatError::InvalidLength)?
        {
            return Err(PackIndexFormatError::InvalidLength);
        }
        if header.body_length
            != header
                .records_offset
                .checked_add(header.records_length)
                .ok_or(PackIndexFormatError::InvalidLength)?
            || !header.body_length.is_multiple_of(PACK_INDEX_ALIGNMENT)
        {
            return Err(PackIndexFormatError::InvalidLength);
        }
        let body = bytes
            .get(..footer_offset)
            .ok_or(PackIndexFormatError::Truncated)?;
        if digest(body) != footer.body_checksum {
            return Err(PackIndexFormatError::InvalidChecksum);
        }
        let calculated_id = index_id_for_body(header.version, body);
        if calculated_id != footer.index_id {
            return Err(PackIndexFormatError::InvalidChecksum);
        }

        let count =
            usize::try_from(header.entry_count).map_err(|_| PackIndexFormatError::ShardTooLarge)?;
        let mut entries = Vec::new();
        entries
            .try_reserve(count)
            .map_err(|_| PackIndexFormatError::AllocationFailure)?;
        let records_start = usize::try_from(header.records_offset)
            .map_err(|_| PackIndexFormatError::InvalidLength)?;
        let records_length = usize::try_from(header.records_length)
            .map_err(|_| PackIndexFormatError::InvalidLength)?;
        let records_end = records_start
            .checked_add(records_length)
            .ok_or(PackIndexFormatError::InvalidLength)?;
        if records_start != PACK_INDEX_HEADER_LENGTH
            || records_length != count.saturating_mul(PACK_INDEX_RECORD_LENGTH)
            || records_end != footer_offset
        {
            return Err(PackIndexFormatError::InvalidLength);
        }
        let mut offset = records_start;
        let mut previous = None;
        while offset < records_end {
            let end = offset
                .checked_add(PACK_INDEX_RECORD_LENGTH)
                .ok_or(PackIndexFormatError::InvalidLength)?;
            let record = parse_entry(
                bytes
                    .get(offset..end)
                    .ok_or(PackIndexFormatError::Truncated)?,
                header.shard_id,
            )?;
            if previous.is_some_and(|chunk_id| chunk_id >= record.chunk_id()) {
                if previous == Some(record.chunk_id()) {
                    return Err(PackIndexFormatError::DuplicateChunkId);
                }
                return Err(PackIndexFormatError::InvalidField);
            }
            previous = Some(record.chunk_id());
            entries.push(record);
            offset = end;
        }
        if offset != records_end {
            return Err(PackIndexFormatError::TrailingData);
        }
        let estimated_memory = bytes
            .len()
            .checked_add(
                entries
                    .len()
                    .checked_mul(std::mem::size_of::<PackIndexEntry>())
                    .ok_or(PackIndexFormatError::ShardTooLarge)?,
            )
            .ok_or(PackIndexFormatError::ShardTooLarge)?;
        let metadata = PackIndexShardMetadata::from_parts(
            header.shard_id,
            footer.index_id,
            header.version,
            header.entry_count,
            header.records_offset,
            header.records_length,
            header.body_length,
            input_length,
        );
        Ok(Self {
            metadata,
            entries,
            estimated_memory,
        })
    }

    pub(crate) const fn metadata(&self) -> PackIndexShardMetadata {
        self.metadata
    }

    pub(crate) fn entries(&self) -> &[PackIndexEntry] {
        &self.entries
    }

    pub(crate) fn lookup(&self, chunk_id: crate::domain::ChunkId) -> Option<PackIndexEntry> {
        self.entries
            .binary_search_by_key(&chunk_id, |entry| entry.chunk_id())
            .ok()
            .map(|index| self.entries[index])
    }

    pub(crate) const fn estimated_memory(&self) -> usize {
        self.estimated_memory
    }
}

struct ParsedHeader {
    version: u16,
    shard_id: PackIndexShardId,
    entry_count: u64,
    records_offset: u64,
    records_length: u64,
    body_length: u64,
}

struct ParsedFooter {
    version: u16,
    entry_count: u64,
    body_length: u64,
    body_checksum: [u8; 32],
    index_id: PackIndexId,
}

fn parse_header(bytes: &[u8]) -> Result<ParsedHeader, PackIndexFormatError> {
    let header = bytes
        .get(..PACK_INDEX_HEADER_LENGTH)
        .ok_or(PackIndexFormatError::Truncated)?;
    if header[0..4] != *INDEX_MAGIC {
        return Err(PackIndexFormatError::InvalidMagic);
    }
    let version = get_u16(header, 4)?;
    if version != CURRENT_PACK_INDEX_FORMAT_VERSION {
        return Err(PackIndexFormatError::UnsupportedVersion { version });
    }
    if get_u16(header, 6)? != INDEX_FLAGS
        || get_u32(header, 8)? != PACK_INDEX_HEADER_LENGTH as u32
        || get_u32(header, 12)? != PACK_INDEX_ALIGNMENT as u32
        || header[16] as usize != crate::domain::PACK_INDEX_SHARD_PREFIX_BYTES
        || get_u16(header, 18)? != PACK_INDEX_RECORD_LENGTH as u16
        || header[52..64].iter().any(|byte| *byte != 0)
    {
        return Err(PackIndexFormatError::InvalidField);
    }
    let shard_id = PackIndexShardId::from_byte(header[17]);
    let entry_count = get_u64(header, 20)?;
    let records_offset = get_u64(header, 28)?;
    let records_length = get_u64(header, 36)?;
    let body_length = get_u64(header, 44)?;
    let expected_records_length = entry_count
        .checked_mul(PACK_INDEX_RECORD_LENGTH as u64)
        .ok_or(PackIndexFormatError::InvalidLength)?;
    if records_length != expected_records_length {
        return Err(PackIndexFormatError::InvalidLength);
    }
    Ok(ParsedHeader {
        version,
        shard_id,
        entry_count,
        records_offset,
        records_length,
        body_length,
    })
}

fn parse_footer(bytes: &[u8], offset: usize) -> Result<ParsedFooter, PackIndexFormatError> {
    let end = offset
        .checked_add(PACK_INDEX_FOOTER_LENGTH)
        .ok_or(PackIndexFormatError::Truncated)?;
    let footer = bytes
        .get(offset..end)
        .ok_or(PackIndexFormatError::Truncated)?;
    if footer[0..4] != *FOOTER_MAGIC {
        return Err(PackIndexFormatError::InvalidMagic);
    }
    let version = get_u16(footer, 4)?;
    if version != CURRENT_PACK_INDEX_FORMAT_VERSION {
        return Err(PackIndexFormatError::UnsupportedVersion { version });
    }
    if get_u16(footer, 6)? != INDEX_FLAGS
        || get_u32(footer, 8)? != PACK_INDEX_FOOTER_LENGTH as u32
        || footer[92..96].iter().any(|byte| *byte != 0)
    {
        return Err(PackIndexFormatError::InvalidField);
    }
    Ok(ParsedFooter {
        version,
        entry_count: get_u64(footer, 12)?,
        body_length: get_u64(footer, 20)?,
        body_checksum: read_array(footer, 28)?,
        index_id: PackIndexId::from_digest(read_array(footer, 60)?),
    })
}

fn parse_entry(
    record: &[u8],
    shard_id: PackIndexShardId,
) -> Result<PackIndexEntry, PackIndexFormatError> {
    if record.len() != PACK_INDEX_RECORD_LENGTH {
        return Err(PackIndexFormatError::Truncated);
    }
    let chunk_id = crate::domain::ChunkId::from_digest(read_array(record, 0)?);
    if !entry_belongs_to_shard_from_chunk(chunk_id, shard_id) {
        return Err(PackIndexFormatError::WrongShard);
    }
    let pack_id = crate::domain::PackId::from_digest(read_array(record, 32)?);
    let envelope_version = get_u16(record, 104)?;
    let object_version = get_u16(record, 106)?;
    let codec = parse_codec(record[108])?;
    let encryption = parse_encryption(record[109])?;
    if get_u16(record, 110)? != 0 || record[116..128].iter().any(|byte| *byte != 0) {
        return Err(PackIndexFormatError::InvalidField);
    }
    let compression_level = get_i32(record, 112)?;
    let compression_level = if codec == ObjectCodec::None {
        if compression_level != 0 {
            return Err(PackIndexFormatError::InvalidField);
        }
        CompressionLevel::DEFAULT
    } else {
        CompressionLevel::new(compression_level).map_err(|_| PackIndexFormatError::InvalidField)?
    };
    let options = crate::domain::ObjectTransformOptions::new(codec, encryption)
        .with_compression_level(compression_level);
    let transform = PackIndexTransform::new(envelope_version, object_version, options)
        .map_err(|_| PackIndexFormatError::InvalidField)?;
    PackIndexEntry::new(
        chunk_id,
        pack_id,
        get_u64(record, 64)?,
        get_u64(record, 72)?,
        get_u64(record, 80)?,
        get_u64(record, 88)?,
        get_u64(record, 96)?,
        transform,
    )
    .map_err(map_entry_error)
}

fn entry_belongs_to_shard_from_chunk(
    chunk_id: crate::domain::ChunkId,
    shard_id: PackIndexShardId,
) -> bool {
    PackIndexShardId::from_chunk_id(chunk_id) == shard_id
}

fn parse_codec(value: u8) -> Result<ObjectCodec, PackIndexFormatError> {
    match value {
        0 => Ok(ObjectCodec::None),
        1 => Ok(ObjectCodec::Zstd),
        _ => Err(PackIndexFormatError::UnsupportedCodec),
    }
}

fn parse_encryption(value: u8) -> Result<ObjectEncryption, PackIndexFormatError> {
    match value {
        0 => Ok(ObjectEncryption::None),
        1 => Ok(ObjectEncryption::XChaCha20Poly1305),
        _ => Err(PackIndexFormatError::UnsupportedEncryption),
    }
}

fn codec_tag(value: ObjectCodec) -> Result<u8, PackIndexFormatError> {
    match value {
        ObjectCodec::None => Ok(0),
        ObjectCodec::Zstd => Ok(1),
    }
}

fn encryption_tag(value: ObjectEncryption) -> Result<u8, PackIndexFormatError> {
    match value {
        ObjectEncryption::None => Ok(0),
        ObjectEncryption::XChaCha20Poly1305 => Ok(1),
    }
}

fn map_entry_error(error: PackIndexEntryError) -> PackIndexFormatError {
    match error {
        PackIndexEntryError::InvalidEntryOffset
        | PackIndexEntryError::InvalidEntryLength
        | PackIndexEntryError::InvalidPayloadOffset
        | PackIndexEntryError::LengthOverflow
        | PackIndexEntryError::RangeExceedsLimit
        | PackIndexEntryError::PayloadExceedsEntry
        | PackIndexEntryError::LogicalLengthExceedsLimit => PackIndexFormatError::InvalidLength,
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn index_id_for_body(version: u16, body: &[u8]) -> PackIndexId {
    let mut hasher = Sha256::new();
    hasher.update(INDEX_ID_DOMAIN_SEPARATOR);
    hasher.update(version.to_be_bytes());
    hasher.update(body);
    PackIndexId::from_digest(hasher.finalize().into())
}

fn get_u16(bytes: &[u8], offset: usize) -> Result<u16, PackIndexFormatError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(PackIndexFormatError::Truncated)?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn get_u32(bytes: &[u8], offset: usize) -> Result<u32, PackIndexFormatError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(PackIndexFormatError::Truncated)?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn get_u64(bytes: &[u8], offset: usize) -> Result<u64, PackIndexFormatError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(PackIndexFormatError::Truncated)?;
    Ok(u64::from_be_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn get_i32(bytes: &[u8], offset: usize) -> Result<i32, PackIndexFormatError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(PackIndexFormatError::Truncated)?;
    Ok(i32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_array<const N: usize>(
    bytes: &[u8],
    offset: usize,
) -> Result<[u8; N], PackIndexFormatError> {
    bytes
        .get(offset..offset + N)
        .ok_or(PackIndexFormatError::Truncated)
        .and_then(|value| {
            value
                .try_into()
                .map_err(|_| PackIndexFormatError::Truncated)
        })
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
