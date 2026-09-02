use super::chunk::{ChunkId, MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES};
use super::object::{
    CURRENT_OBJECT_ENVELOPE_VERSION, ObjectCodec, ObjectEncryption, ObjectTransformOptions,
};
use super::pack::{
    MAX_PACK_SIZE_BYTES, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH, PackEntryLocation, PackId,
};
use std::fmt;

/// The version of the immutable pack-index shard format.
pub const CURRENT_PACK_INDEX_FORMAT_VERSION: u16 = 1;

/// The number of leading chunk-ID bytes used to select an index shard.
pub const PACK_INDEX_SHARD_PREFIX_BYTES: usize = 1;

/// The number of possible version-1 pack-index shards.
pub const PACK_INDEX_SHARD_COUNT: usize = 1 << (PACK_INDEX_SHARD_PREFIX_BYTES * 8);

/// The fixed byte alignment used by pack-index framing.
pub const PACK_INDEX_ALIGNMENT: u64 = 8;

/// The fixed size of a version-1 pack-index header in bytes.
pub const PACK_INDEX_HEADER_LENGTH: usize = 64;

/// The fixed size of a version-1 pack-index record in bytes.
pub const PACK_INDEX_RECORD_LENGTH: usize = 128;

/// The fixed size of a version-1 pack-index footer in bytes.
pub const PACK_INDEX_FOOTER_LENGTH: usize = 96;

/// The smallest shard that can contain one index record.
pub const MIN_PACK_INDEX_SHARD_BYTES: u64 = PACK_INDEX_HEADER_LENGTH as u64
    + PACK_INDEX_RECORD_LENGTH as u64
    + PACK_INDEX_FOOTER_LENGTH as u64;

/// The default maximum encoded size for one index shard.
pub const DEFAULT_PACK_INDEX_MAX_SHARD_BYTES: u64 = 16 * 1024 * 1024;

/// The largest encoded size accepted for one index shard.
pub const MAX_PACK_INDEX_SHARD_BYTES: u64 = 64 * 1024 * 1024;

/// The default total resident-memory budget for decoded index shards.
pub const DEFAULT_PACK_INDEX_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The default number of decoded shards retained by the lookup cache.
pub const DEFAULT_PACK_INDEX_CACHE_MAX_SHARDS: usize = 8;

/// The largest cache budget accepted by the SDK.
pub const MAX_PACK_INDEX_CACHE_BYTES: usize = 512 * 1024 * 1024;

/// The storage prefix used by the simple one-generation index layout.
pub const PACK_INDEX_STORAGE_PREFIX: &str = "indexes/pack-v1";

/// A validated policy for encoding one immutable pack-index shard.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackIndexConfiguration {
    version: u16,
    shard_prefix_bytes: usize,
    max_shard_bytes: u64,
}

impl PackIndexConfiguration {
    /// Creates the current one-byte-prefix pack-index policy.
    pub fn new(max_shard_bytes: u64) -> Result<Self, PackIndexConfigurationError> {
        Self::from_parts(
            CURRENT_PACK_INDEX_FORMAT_VERSION,
            PACK_INDEX_SHARD_PREFIX_BYTES,
            max_shard_bytes,
        )
    }

    /// Creates the default version-1 pack-index policy.
    pub const fn default_policy() -> Self {
        Self {
            version: CURRENT_PACK_INDEX_FORMAT_VERSION,
            shard_prefix_bytes: PACK_INDEX_SHARD_PREFIX_BYTES,
            max_shard_bytes: DEFAULT_PACK_INDEX_MAX_SHARD_BYTES,
        }
    }

    /// Creates a policy from explicit persisted format parameters.
    ///
    /// Unknown versions and shard layouts are rejected instead of being
    /// interpreted as the current policy.
    pub fn from_parts(
        version: u16,
        shard_prefix_bytes: usize,
        max_shard_bytes: u64,
    ) -> Result<Self, PackIndexConfigurationError> {
        if version != CURRENT_PACK_INDEX_FORMAT_VERSION {
            return Err(PackIndexConfigurationError::UnsupportedVersion);
        }
        if shard_prefix_bytes != PACK_INDEX_SHARD_PREFIX_BYTES {
            return Err(PackIndexConfigurationError::UnsupportedShardPrefix);
        }
        if max_shard_bytes < MIN_PACK_INDEX_SHARD_BYTES {
            return Err(PackIndexConfigurationError::ShardMaximumBelowMinimum);
        }
        if max_shard_bytes > MAX_PACK_INDEX_SHARD_BYTES {
            return Err(PackIndexConfigurationError::ShardMaximumExceedsLimit);
        }
        usize::try_from(max_shard_bytes)
            .map_err(|_| PackIndexConfigurationError::ShardMaximumExceedsPlatformLimit)?;
        Ok(Self {
            version,
            shard_prefix_bytes,
            max_shard_bytes,
        })
    }

    /// Returns the immutable pack-index format version.
    pub const fn version(self) -> u16 {
        self.version
    }

    /// Returns the number of chunk-ID prefix bytes in this policy.
    pub const fn shard_prefix_bytes(self) -> usize {
        self.shard_prefix_bytes
    }

    /// Returns the maximum complete encoded shard size.
    pub const fn max_shard_bytes(self) -> u64 {
        self.max_shard_bytes
    }
}

impl Default for PackIndexConfiguration {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A validation failure for a pack-index encoding policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIndexConfigurationError {
    /// The persisted index format version is not supported.
    UnsupportedVersion,
    /// The persisted shard prefix layout is not supported.
    UnsupportedShardPrefix,
    /// The maximum shard size cannot hold one record.
    ShardMaximumBelowMinimum,
    /// The maximum shard size exceeds the SDK resource limit.
    ShardMaximumExceedsLimit,
    /// The maximum shard size cannot be represented by the platform.
    ShardMaximumExceedsPlatformLimit,
}

impl fmt::Display for PackIndexConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedVersion => "pack-index format version is unsupported",
            Self::UnsupportedShardPrefix => "pack-index shard prefix layout is unsupported",
            Self::ShardMaximumBelowMinimum => {
                "pack-index shard maximum is too small for one record"
            }
            Self::ShardMaximumExceedsLimit => "pack-index shard maximum exceeds the SDK limit",
            Self::ShardMaximumExceedsPlatformLimit => {
                "pack-index shard maximum exceeds the platform allocation limit"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PackIndexConfigurationError {}

/// A validated resident-memory policy for decoded pack-index shards.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackIndexCacheConfiguration {
    max_bytes: usize,
    max_shards: usize,
}

impl PackIndexCacheConfiguration {
    /// Creates an explicit cache policy.
    pub fn new(
        max_bytes: usize,
        max_shards: usize,
    ) -> Result<Self, PackIndexCacheConfigurationError> {
        if max_bytes == 0 {
            return Err(PackIndexCacheConfigurationError::MaximumBytesMustBePositive);
        }
        if max_bytes > MAX_PACK_INDEX_CACHE_BYTES {
            return Err(PackIndexCacheConfigurationError::MaximumBytesExceedsLimit);
        }
        if max_shards == 0 {
            return Err(PackIndexCacheConfigurationError::MaximumShardsMustBePositive);
        }
        Ok(Self {
            max_bytes,
            max_shards,
        })
    }

    /// Returns the default cache policy.
    pub const fn default_policy() -> Self {
        Self {
            max_bytes: DEFAULT_PACK_INDEX_CACHE_MAX_BYTES,
            max_shards: DEFAULT_PACK_INDEX_CACHE_MAX_SHARDS,
        }
    }

    /// Returns the maximum estimated resident bytes.
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }

    /// Returns the maximum number of resident shards.
    pub const fn max_shards(self) -> usize {
        self.max_shards
    }
}

impl Default for PackIndexCacheConfiguration {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A validation failure for an index-shard cache policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIndexCacheConfigurationError {
    /// The byte budget is zero.
    MaximumBytesMustBePositive,
    /// The byte budget exceeds the SDK resource limit.
    MaximumBytesExceedsLimit,
    /// The shard count is zero.
    MaximumShardsMustBePositive,
}

impl fmt::Display for PackIndexCacheConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MaximumBytesMustBePositive => {
                "pack-index cache maximum bytes must be greater than zero"
            }
            Self::MaximumBytesExceedsLimit => {
                "pack-index cache maximum bytes exceeds the SDK limit"
            }
            Self::MaximumShardsMustBePositive => {
                "pack-index cache maximum shards must be greater than zero"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PackIndexCacheConfigurationError {}

/// The one-byte prefix selecting an immutable pack-index shard.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackIndexShardId(u8);

impl PackIndexShardId {
    /// Creates a shard ID from its raw prefix byte.
    pub const fn from_byte(value: u8) -> Self {
        Self(value)
    }

    /// Derives the shard ID from the first byte of a chunk ID.
    pub const fn from_chunk_id(chunk_id: ChunkId) -> Self {
        Self(chunk_id.as_bytes()[0])
    }

    /// Returns the raw prefix byte.
    pub const fn as_byte(self) -> u8 {
        self.0
    }

    /// Returns the two-character lowercase hexadecimal storage component.
    pub fn as_hex(self) -> String {
        format!("{:02x}", self.0)
    }
}

impl fmt::Display for PackIndexShardId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02x}", self.0)
    }
}

/// A validated transformed-payload descriptor stored with an index record.
///
/// The envelope and object versions select the payload decoder. The codec,
/// compression level, and encryption scheme select its transform decoder.
/// Per-object encryption nonce, salt, KDF parameters, and authentication data
/// remain in the authenticated transformed payload envelope; they are not
/// duplicated in every index record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PackIndexTransform {
    envelope_version: u16,
    object_version: u16,
    options: ObjectTransformOptions,
}

impl PackIndexTransform {
    /// Creates a transform descriptor from validated object options.
    pub fn new(
        envelope_version: u16,
        object_version: u16,
        options: ObjectTransformOptions,
    ) -> Result<Self, PackIndexTransformError> {
        if envelope_version == 0 {
            return Err(PackIndexTransformError::EnvelopeVersionMustBePositive);
        }
        if object_version == 0 {
            return Err(PackIndexTransformError::ObjectVersionMustBePositive);
        }
        Ok(Self {
            envelope_version,
            object_version,
            options,
        })
    }

    /// Creates the current untransformed object descriptor.
    pub fn plain(object_version: u16) -> Result<Self, PackIndexTransformError> {
        Self::new(
            CURRENT_OBJECT_ENVELOPE_VERSION,
            object_version,
            ObjectTransformOptions::new(ObjectCodec::None, ObjectEncryption::None),
        )
    }

    /// Returns the immutable-object envelope version.
    pub const fn envelope_version(self) -> u16 {
        self.envelope_version
    }

    /// Returns the kind-specific payload version.
    pub const fn object_version(self) -> u16 {
        self.object_version
    }

    /// Returns the transform options.
    pub const fn options(self) -> ObjectTransformOptions {
        self.options
    }

    /// Returns the recorded codec.
    pub const fn codec(self) -> ObjectCodec {
        self.options.codec()
    }

    /// Returns the recorded compression level.
    pub const fn compression_level(self) -> super::object::CompressionLevel {
        self.options.compression_level()
    }

    /// Returns the recorded encryption scheme.
    pub const fn encryption(self) -> ObjectEncryption {
        self.options.encryption()
    }
}

/// A validation failure for transformed-payload metadata.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIndexTransformError {
    /// The envelope version is zero and cannot select a decoder.
    EnvelopeVersionMustBePositive,
    /// The payload version is zero and cannot select a decoder.
    ObjectVersionMustBePositive,
}

impl fmt::Display for PackIndexTransformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvelopeVersionMustBePositive => {
                formatter.write_str("pack-index envelope version must be greater than zero")
            }
            Self::ObjectVersionMustBePositive => {
                formatter.write_str("pack-index object version must be greater than zero")
            }
        }
    }
}

impl std::error::Error for PackIndexTransformError {}

/// A half-open byte range in a containing pack.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackIndexRange {
    offset: u64,
    length: u64,
}

impl PackIndexRange {
    /// Creates a range after checking offset arithmetic.
    pub fn new(offset: u64, length: u64) -> Result<Self, PackIndexRangeError> {
        offset
            .checked_add(length)
            .ok_or(PackIndexRangeError::OffsetOverflow)?;
        Ok(Self { offset, length })
    }

    /// Returns the first byte offset.
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Returns the exact range length.
    pub const fn length(self) -> u64 {
        self.length
    }

    /// Returns the exclusive range end.
    pub const fn end(self) -> u64 {
        self.offset + self.length
    }
}

/// A validation failure for a pack range.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIndexRangeError {
    /// The range end overflowed `u64`.
    OffsetOverflow,
    /// The range end lies outside the containing pack.
    RangeExceedsPack,
    /// The containing pack length exceeds the SDK pack limit.
    PackLengthExceedsLimit,
}

impl fmt::Display for PackIndexRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::OffsetOverflow => "pack index range offset arithmetic overflowed",
            Self::RangeExceedsPack => "pack index range exceeds the containing pack",
            Self::PackLengthExceedsLimit => "containing pack length exceeds the SDK limit",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PackIndexRangeError {}

/// A validated location record mapping one chunk ID to a transformed pack
/// payload and its decoder metadata.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PackIndexEntry {
    chunk_id: ChunkId,
    pack_id: PackId,
    entry_offset: u64,
    payload_offset: u64,
    entry_length: u64,
    stored_length: u64,
    logical_length: u64,
    transform: PackIndexTransform,
}

impl PackIndexEntry {
    /// Creates a validated entry from explicit pack coordinates.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chunk_id: ChunkId,
        pack_id: PackId,
        entry_offset: u64,
        payload_offset: u64,
        entry_length: u64,
        stored_length: u64,
        logical_length: u64,
        transform: PackIndexTransform,
    ) -> Result<Self, PackIndexEntryError> {
        if logical_length > MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES {
            return Err(PackIndexEntryError::LogicalLengthExceedsLimit);
        }
        if entry_offset < super::pack::PACK_HEADER_LENGTH as u64
            || !entry_offset.is_multiple_of(PACK_ALIGNMENT)
        {
            return Err(PackIndexEntryError::InvalidEntryOffset);
        }
        if entry_length < PACK_ENTRY_HEADER_LENGTH as u64
            || !entry_length.is_multiple_of(PACK_ALIGNMENT)
        {
            return Err(PackIndexEntryError::InvalidEntryLength);
        }
        let entry_end = entry_offset
            .checked_add(entry_length)
            .ok_or(PackIndexEntryError::LengthOverflow)?;
        if entry_end > MAX_PACK_SIZE_BYTES {
            return Err(PackIndexEntryError::RangeExceedsLimit);
        }
        let unpadded_length = (PACK_ENTRY_HEADER_LENGTH as u64)
            .checked_add(stored_length)
            .ok_or(PackIndexEntryError::LengthOverflow)?;
        let expected_entry_length = unpadded_length
            .checked_add(PACK_ALIGNMENT - 1)
            .ok_or(PackIndexEntryError::LengthOverflow)?
            / PACK_ALIGNMENT
            * PACK_ALIGNMENT;
        if entry_length != expected_entry_length {
            return Err(PackIndexEntryError::InvalidEntryLength);
        }
        let expected_payload_offset = entry_offset
            .checked_add(PACK_ENTRY_HEADER_LENGTH as u64)
            .ok_or(PackIndexEntryError::LengthOverflow)?;
        if payload_offset != expected_payload_offset {
            return Err(PackIndexEntryError::InvalidPayloadOffset);
        }
        let payload_end = payload_offset
            .checked_add(stored_length)
            .ok_or(PackIndexEntryError::LengthOverflow)?;
        if payload_end > entry_end {
            return Err(PackIndexEntryError::PayloadExceedsEntry);
        }
        Ok(Self {
            chunk_id,
            pack_id,
            entry_offset,
            payload_offset,
            entry_length,
            stored_length,
            logical_length,
            transform,
        })
    }

    /// Creates an index entry from a verified pack location.
    pub fn from_location(
        location: PackEntryLocation,
        transform: PackIndexTransform,
    ) -> Result<Self, PackIndexEntryError> {
        Self::new(
            location.chunk_id(),
            location.pack_id(),
            location.entry_offset(),
            location.payload_offset(),
            location.entry_length(),
            location.payload_length(),
            location.plaintext_length(),
            transform,
        )
    }

    /// Returns the logical chunk ID.
    pub const fn chunk_id(self) -> ChunkId {
        self.chunk_id
    }

    /// Returns the containing pack ID.
    pub const fn pack_id(self) -> PackId {
        self.pack_id
    }

    /// Returns the entry-frame offset.
    pub const fn entry_offset(self) -> u64 {
        self.entry_offset
    }

    /// Returns the transformed-payload offset.
    pub const fn payload_offset(self) -> u64 {
        self.payload_offset
    }

    /// Returns the aligned entry-frame length.
    pub const fn entry_length(self) -> u64 {
        self.entry_length
    }

    /// Returns the transformed stored-payload length.
    pub const fn stored_length(self) -> u64 {
        self.stored_length
    }

    /// Alias for [`Self::stored_length`].
    pub const fn payload_length(self) -> u64 {
        self.stored_length()
    }

    /// Returns the logical plaintext length.
    pub const fn logical_length(self) -> u64 {
        self.logical_length
    }

    /// Alias for [`Self::logical_length`].
    pub const fn plaintext_length(self) -> u64 {
        self.logical_length()
    }

    /// Returns the recorded transform descriptor.
    pub const fn transform(self) -> PackIndexTransform {
        self.transform
    }

    /// Returns the complete entry-frame range.
    pub fn entry_range(self) -> PackIndexRange {
        PackIndexRange {
            offset: self.entry_offset,
            length: self.entry_length,
        }
    }

    /// Returns the exact transformed-payload range.
    pub fn payload_range(self) -> PackIndexRange {
        PackIndexRange {
            offset: self.payload_offset,
            length: self.stored_length,
        }
    }

    /// Validates both the entry and payload ranges against a pack length.
    pub fn validate_against_pack_length(
        self,
        pack_length: u64,
    ) -> Result<PackIndexRange, PackIndexRangeError> {
        if pack_length > MAX_PACK_SIZE_BYTES {
            return Err(PackIndexRangeError::PackLengthExceedsLimit);
        }
        if self.entry_range().end() > pack_length || self.payload_range().end() > pack_length {
            return Err(PackIndexRangeError::RangeExceedsPack);
        }
        Ok(self.payload_range())
    }
}

/// A validation failure for a pack-index entry.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIndexEntryError {
    /// The entry offset is before pack entries or is not aligned.
    InvalidEntryOffset,
    /// The entry length is too small or is not aligned.
    InvalidEntryLength,
    /// The payload offset is not immediately after the entry header.
    InvalidPayloadOffset,
    /// The entry or payload range arithmetic overflowed.
    LengthOverflow,
    /// The entry range exceeds the absolute pack bound.
    RangeExceedsLimit,
    /// The stored payload extends beyond its entry frame.
    PayloadExceedsEntry,
    /// The logical plaintext length exceeds the chunk limit.
    LogicalLengthExceedsLimit,
}

impl fmt::Display for PackIndexEntryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidEntryOffset => "pack-index entry offset is invalid",
            Self::InvalidEntryLength => "pack-index entry length is invalid",
            Self::InvalidPayloadOffset => "pack-index payload offset is invalid",
            Self::LengthOverflow => "pack-index entry length arithmetic overflowed",
            Self::RangeExceedsLimit => "pack-index entry range exceeds the SDK limit",
            Self::PayloadExceedsEntry => "pack-index payload exceeds its entry frame",
            Self::LogicalLengthExceedsLimit => {
                "pack-index logical chunk length exceeds the SDK limit"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PackIndexEntryError {}

/// A stable SHA-256 identifier for one immutable index shard.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackIndexId([u8; 32]);

impl PackIndexId {
    /// Creates an index ID from raw SHA-256 digest bytes.
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Parses a 64-character hexadecimal index ID.
    pub fn from_hex(value: &str) -> Result<Self, PackIndexIdError> {
        if value.len() != 64 {
            return Err(PackIndexIdError::InvalidLength);
        }
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            let offset = index * 2;
            let high =
                hex_value(value.as_bytes()[offset]).ok_or(PackIndexIdError::InvalidCharacter)?;
            let low = hex_value(value.as_bytes()[offset + 1])
                .ok_or(PackIndexIdError::InvalidCharacter)?;
            *byte = (high << 4) | low;
        }
        Ok(Self(digest))
    }

    /// Returns the raw digest bytes.
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Returns the lowercase hexadecimal representation.
    pub fn as_hex(self) -> String {
        hex_encode(&self.0)
    }
}

impl fmt::Debug for PackIndexId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PackIndexId")
            .field(&self.as_hex())
            .finish()
    }
}

impl fmt::Display for PackIndexId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex_encode(&self.0))
    }
}

impl AsRef<[u8]> for PackIndexId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A validation failure while parsing an index ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIndexIdError {
    /// The hexadecimal value does not contain exactly 64 characters.
    InvalidLength,
    /// The hexadecimal value contains a non-hexadecimal character.
    InvalidCharacter,
}

impl fmt::Display for PackIndexIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => {
                formatter.write_str("pack index ID must contain 64 hexadecimal bytes")
            }
            Self::InvalidCharacter => {
                formatter.write_str("pack index ID contains an invalid character")
            }
        }
    }
}

impl std::error::Error for PackIndexIdError {}

/// Summary metadata for a verified immutable index shard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackIndexShardMetadata {
    shard_id: PackIndexShardId,
    index_id: PackIndexId,
    version: u16,
    entry_count: u64,
    records_offset: u64,
    records_length: u64,
    body_length: u64,
    total_length: u64,
}

impl PackIndexShardMetadata {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn from_parts(
        shard_id: PackIndexShardId,
        index_id: PackIndexId,
        version: u16,
        entry_count: u64,
        records_offset: u64,
        records_length: u64,
        body_length: u64,
        total_length: u64,
    ) -> Self {
        Self {
            shard_id,
            index_id,
            version,
            entry_count,
            records_offset,
            records_length,
            body_length,
            total_length,
        }
    }

    /// Returns the shard selected by the leading chunk-ID byte.
    pub const fn shard_id(self) -> PackIndexShardId {
        self.shard_id
    }

    /// Returns the content ID of this immutable shard.
    pub const fn index_id(self) -> PackIndexId {
        self.index_id
    }

    /// Returns the index format version.
    pub const fn version(self) -> u16 {
        self.version
    }

    /// Returns the number of sorted records.
    pub const fn entry_count(self) -> u64 {
        self.entry_count
    }

    /// Returns the first record offset.
    pub const fn records_offset(self) -> u64 {
        self.records_offset
    }

    /// Returns the encoded records length.
    pub const fn records_length(self) -> u64 {
        self.records_length
    }

    /// Returns the body length before the footer.
    pub const fn body_length(self) -> u64 {
        self.body_length
    }

    /// Returns the complete encoded shard length.
    pub const fn total_length(self) -> u64 {
        self.total_length
    }
}

/// A complete immutable pack-index shard produced by a shard builder.
pub struct SealedPackIndexShard {
    id: PackIndexId,
    bytes: Vec<u8>,
    metadata: PackIndexShardMetadata,
    entries: Vec<PackIndexEntry>,
}

impl SealedPackIndexShard {
    pub(crate) fn new(
        id: PackIndexId,
        bytes: Vec<u8>,
        metadata: PackIndexShardMetadata,
        entries: Vec<PackIndexEntry>,
    ) -> Self {
        Self {
            id,
            bytes,
            metadata,
            entries,
        }
    }

    /// Returns the immutable index-shard ID.
    pub const fn id(&self) -> PackIndexId {
        self.id
    }

    /// Alias for [`Self::id`].
    pub const fn index_id(&self) -> PackIndexId {
        self.id()
    }

    /// Returns the shard's complete encoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the complete encoded shard length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the encoded shard is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns verified shard metadata.
    pub const fn metadata(&self) -> PackIndexShardMetadata {
        self.metadata
    }

    /// Returns records in canonical chunk-ID order.
    pub fn entries(&self) -> &[PackIndexEntry] {
        &self.entries
    }

    /// Consumes the shard and returns its encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for SealedPackIndexShard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedPackIndexShard")
            .field("id", &self.id)
            .field("shard_id", &self.metadata.shard_id())
            .field("length", &self.bytes.len())
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

/// Derives the storage key component for a static version-1 shard.
pub fn pack_index_storage_key(shard_id: PackIndexShardId) -> String {
    format!("{PACK_INDEX_STORAGE_PREFIX}/{}", shard_id.as_hex())
}

/// Derives the storage key for an immutable index shard ID.
pub fn pack_index_object_key(index_id: PackIndexId) -> String {
    format!("indexes/{}", index_id.as_hex())
}

/// Returns whether a location is assigned to the expected shard.
pub(crate) fn entry_belongs_to_shard(entry: PackIndexEntry, shard_id: PackIndexShardId) -> bool {
    PackIndexShardId::from_chunk_id(entry.chunk_id()) == shard_id
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}
