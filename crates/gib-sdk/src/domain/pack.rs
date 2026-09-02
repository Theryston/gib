use super::chunk::{Chunk, ChunkId, MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES};
use std::fmt;

/// The version of the immutable pack file format.
pub const CURRENT_PACK_FORMAT_VERSION: u16 = 1;

/// The fixed byte alignment used for pack entries and the footer boundary.
pub const PACK_ALIGNMENT: u64 = 8;

/// The fixed size of a version-1 pack header in bytes.
pub const PACK_HEADER_LENGTH: usize = 64;

/// The fixed size of a version-1 pack entry header in bytes.
pub const PACK_ENTRY_HEADER_LENGTH: usize = 96;

/// The fixed size of a version-1 pack footer in bytes.
pub const PACK_FOOTER_LENGTH: usize = 104;

/// The default target size for a newly built pack, including framing.
pub const DEFAULT_PACK_TARGET_SIZE_BYTES: u64 = 64 * 1024 * 1024;

/// The default hard maximum for a pack, including framing.
pub const DEFAULT_PACK_MAX_SIZE_BYTES: u64 = 128 * 1024 * 1024;

/// The absolute maximum complete pack size accepted by this SDK.
///
/// The extra two MiB cover transform metadata and pack framing around the
/// largest content-defined chunk. A configured hard maximum may be lower, but
/// never higher, than this bound.
pub const MAX_PACK_SIZE_BYTES: u64 = MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES + 2 * 1024 * 1024;

const PACK_ID_HEX_LENGTH: usize = 64;

/// A validated policy for grouping transformed chunks into immutable packs.
///
/// The target is a soft boundary: a pack may exceed it when adding one entry
/// would cross it. The maximum is a hard boundary for ordinary packs. One
/// pack containing exactly one entry may exceed the configured maximum when
/// that entry itself does not fit, up to [`MAX_PACK_SIZE_BYTES`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackConfiguration {
    version: u16,
    target_size: u64,
    max_size: u64,
}

impl PackConfiguration {
    /// Creates the current version-1 pack policy.
    pub fn new(target_size: u64, max_size: u64) -> Result<Self, PackConfigurationError> {
        Self::from_parts(CURRENT_PACK_FORMAT_VERSION, target_size, max_size)
    }

    /// Creates the default pack policy.
    pub const fn default_policy() -> Self {
        Self {
            version: CURRENT_PACK_FORMAT_VERSION,
            target_size: DEFAULT_PACK_TARGET_SIZE_BYTES,
            max_size: DEFAULT_PACK_MAX_SIZE_BYTES,
        }
    }

    /// Creates a policy from an explicit persisted format version.
    ///
    /// Unknown versions are rejected instead of being interpreted as the
    /// current format.
    pub fn from_parts(
        version: u16,
        target_size: u64,
        max_size: u64,
    ) -> Result<Self, PackConfigurationError> {
        if version != CURRENT_PACK_FORMAT_VERSION {
            return Err(PackConfigurationError::UnsupportedVersion);
        }
        if target_size == 0 {
            return Err(PackConfigurationError::TargetMustBePositive);
        }
        if max_size == 0 {
            return Err(PackConfigurationError::MaximumMustBePositive);
        }
        if target_size > max_size {
            return Err(PackConfigurationError::TargetExceedsMaximum);
        }
        if max_size > MAX_PACK_SIZE_BYTES {
            return Err(PackConfigurationError::SizeExceedsLimit);
        }
        usize::try_from(max_size).map_err(|_| PackConfigurationError::SizeExceedsPlatformLimit)?;
        Ok(Self {
            version,
            target_size,
            max_size,
        })
    }

    /// Returns the immutable pack format version.
    pub const fn version(self) -> u16 {
        self.version
    }

    /// Returns the soft target size, including framing bytes.
    pub const fn target_size(self) -> u64 {
        self.target_size
    }

    /// Returns the hard maximum size, including framing bytes.
    pub const fn max_size(self) -> u64 {
        self.max_size
    }
}

impl Default for PackConfiguration {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A validation failure for a pack policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackConfigurationError {
    /// The persisted pack format version is not supported.
    UnsupportedVersion,
    /// The target size is zero.
    TargetMustBePositive,
    /// The maximum size is zero.
    MaximumMustBePositive,
    /// The target is larger than the maximum.
    TargetExceedsMaximum,
    /// The maximum exceeds the SDK resource limit.
    SizeExceedsLimit,
    /// The maximum cannot be represented as a platform allocation size.
    SizeExceedsPlatformLimit,
}

impl fmt::Display for PackConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedVersion => "pack format version is unsupported",
            Self::TargetMustBePositive => "pack target size must be greater than zero",
            Self::MaximumMustBePositive => "pack maximum size must be greater than zero",
            Self::TargetExceedsMaximum => "pack target size must not exceed maximum size",
            Self::SizeExceedsLimit => "pack maximum size exceeds the SDK limit",
            Self::SizeExceedsPlatformLimit => {
                "pack maximum size exceeds the platform allocation limit"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PackConfigurationError {}

/// A validation failure for a transformed chunk supplied to a pack builder.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackEntryError {
    /// The transformed payload exceeds the absolute pack resource limit.
    PayloadExceedsLimit,
    /// The logical plaintext length exceeds the chunk resource limit.
    PlaintextLengthExceedsLimit,
}

impl fmt::Display for PackEntryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::PayloadExceedsLimit => "transformed pack entry payload exceeds the SDK limit",
            Self::PlaintextLengthExceedsLimit => {
                "pack entry plaintext length exceeds the chunk size limit"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PackEntryError {}

/// One transformed chunk ready to be framed into a pack.
///
/// The payload is opaque to the pack builder. Compression and authenticated
/// encryption must already have been applied by the object transform layer;
/// the pack records the bytes and their logical plaintext length without
/// changing chunk identity.
pub struct PackEntryInput {
    chunk_id: ChunkId,
    plaintext_length: u64,
    payload: Vec<u8>,
}

impl fmt::Debug for PackEntryInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackEntryInput")
            .field("chunk_id", &self.chunk_id)
            .field("plaintext_length", &self.plaintext_length)
            .field("payload_length", &self.payload.len())
            .finish()
    }
}

impl PackEntryInput {
    /// Creates a pack entry from a chunk ID, logical plaintext length, and
    /// transformed payload bytes.
    pub fn new(
        chunk_id: ChunkId,
        plaintext_length: u64,
        payload: Vec<u8>,
    ) -> Result<Self, PackEntryError> {
        if plaintext_length > MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES {
            return Err(PackEntryError::PlaintextLengthExceedsLimit);
        }
        if payload.len() as u64 > MAX_PACK_SIZE_BYTES {
            return Err(PackEntryError::PayloadExceedsLimit);
        }
        Ok(Self {
            chunk_id,
            plaintext_length,
            payload,
        })
    }

    /// Creates an entry for a chunk using its content ID and length.
    pub fn from_chunk(chunk: &Chunk, payload: Vec<u8>) -> Result<Self, PackEntryError> {
        Self::new(chunk.id(), chunk.len() as u64, payload)
    }

    /// Returns the logical chunk ID.
    pub const fn chunk_id(&self) -> ChunkId {
        self.chunk_id
    }

    /// Returns the logical plaintext length before transforms.
    pub const fn plaintext_length(&self) -> u64 {
        self.plaintext_length
    }

    /// Returns the transformed payload without copying it.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Returns the transformed payload and metadata, consuming the input.
    pub(crate) fn into_parts(self) -> (ChunkId, u64, Vec<u8>) {
        (self.chunk_id, self.plaintext_length, self.payload)
    }
}

/// A stable SHA-256 identifier for one sealed pack file.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackId([u8; 32]);

impl PackId {
    /// Creates an ID from raw SHA-256 digest bytes.
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Parses a 64-character hexadecimal pack ID.
    pub fn from_hex(value: &str) -> Result<Self, PackIdError> {
        if value.len() != PACK_ID_HEX_LENGTH {
            return Err(PackIdError::InvalidLength);
        }
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            let offset = index * 2;
            let high = hex_value(value.as_bytes()[offset]).ok_or(PackIdError::InvalidCharacter)?;
            let low =
                hex_value(value.as_bytes()[offset + 1]).ok_or(PackIdError::InvalidCharacter)?;
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

impl fmt::Debug for PackId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PackId")
            .field(&self.as_hex())
            .finish()
    }
}

impl fmt::Display for PackId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex_encode(&self.0))
    }
}

impl AsRef<[u8]> for PackId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A validation failure while parsing a pack ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackIdError {
    /// The hexadecimal value does not contain exactly 64 characters.
    InvalidLength,
    /// The hexadecimal value contains a non-hexadecimal character.
    InvalidCharacter,
}

impl fmt::Display for PackIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => formatter.write_str("pack ID must contain 64 hexadecimal bytes"),
            Self::InvalidCharacter => formatter.write_str("pack ID contains an invalid character"),
        }
    }
}

impl std::error::Error for PackIdError {}

/// The location and lengths of one entry inside a sealed pack.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PackEntryLocation {
    pack_id: PackId,
    chunk_id: ChunkId,
    entry_offset: u64,
    payload_offset: u64,
    entry_length: u64,
    payload_length: u64,
    plaintext_length: u64,
}

impl PackEntryLocation {
    pub(crate) const fn new(
        pack_id: PackId,
        chunk_id: ChunkId,
        entry_offset: u64,
        payload_offset: u64,
        entry_length: u64,
        payload_length: u64,
        plaintext_length: u64,
    ) -> Self {
        Self {
            pack_id,
            chunk_id,
            entry_offset,
            payload_offset,
            entry_length,
            payload_length,
            plaintext_length,
        }
    }

    /// Returns the containing pack ID.
    pub const fn pack_id(self) -> PackId {
        self.pack_id
    }

    /// Returns the logical chunk ID.
    pub const fn chunk_id(self) -> ChunkId {
        self.chunk_id
    }

    /// Returns the byte offset of the entry header.
    pub const fn entry_offset(self) -> u64 {
        self.entry_offset
    }

    /// Returns the byte offset of the transformed payload.
    pub const fn payload_offset(self) -> u64 {
        self.payload_offset
    }

    /// Returns the aligned entry frame length, including its padding.
    pub const fn entry_length(self) -> u64 {
        self.entry_length
    }

    /// Returns the transformed payload length.
    pub const fn payload_length(self) -> u64 {
        self.payload_length
    }

    /// Returns the logical plaintext length.
    pub const fn plaintext_length(self) -> u64 {
        self.plaintext_length
    }

    /// Returns the exclusive end offset of the entry frame.
    pub const fn end_offset(self) -> u64 {
        self.entry_offset + self.entry_length
    }
}

/// Summary metadata for a structurally verified pack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackMetadata {
    pack_id: PackId,
    version: u16,
    target_size: u64,
    max_size: u64,
    entry_count: u64,
    payload_bytes: u64,
    body_length: u64,
    total_length: u64,
    oversized_single_entry: bool,
}

impl PackMetadata {
    pub(crate) const fn from_parts(parts: PackMetadataParts) -> Self {
        Self {
            pack_id: parts.pack_id,
            version: parts.version,
            target_size: parts.target_size,
            max_size: parts.max_size,
            entry_count: parts.entry_count,
            payload_bytes: parts.payload_bytes,
            body_length: parts.body_length,
            total_length: parts.total_length,
            oversized_single_entry: parts.oversized_single_entry,
        }
    }

    /// Returns the pack ID.
    pub const fn pack_id(self) -> PackId {
        self.pack_id
    }

    /// Returns the immutable pack format version.
    pub const fn version(self) -> u16 {
        self.version
    }

    /// Returns the configured soft target size.
    pub const fn target_size(self) -> u64 {
        self.target_size
    }

    /// Returns the configured hard maximum size.
    pub const fn max_size(self) -> u64 {
        self.max_size
    }

    /// Returns the number of entries.
    pub const fn entry_count(self) -> u64 {
        self.entry_count
    }

    /// Returns the sum of transformed payload lengths.
    pub const fn payload_bytes(self) -> u64 {
        self.payload_bytes
    }

    /// Returns the body length before the footer.
    pub const fn body_length(self) -> u64 {
        self.body_length
    }

    /// Returns the complete sealed pack length.
    pub const fn total_length(self) -> u64 {
        self.total_length
    }

    /// Returns whether this pack uses the documented single-entry exception.
    pub const fn is_oversized_single_entry(self) -> bool {
        self.oversized_single_entry
    }
}

pub(crate) struct PackMetadataParts {
    pub(crate) pack_id: PackId,
    pub(crate) version: u16,
    pub(crate) target_size: u64,
    pub(crate) max_size: u64,
    pub(crate) entry_count: u64,
    pub(crate) payload_bytes: u64,
    pub(crate) body_length: u64,
    pub(crate) total_length: u64,
    pub(crate) oversized_single_entry: bool,
}

/// A complete immutable pack produced by the pack builder.
///
/// A sealed pack owns exactly one bounded byte buffer. It can be handed to a
/// publisher, indexed using [`Self::entries`], and then dropped; the builder
/// does not retain completed packs.
pub struct SealedPack {
    id: PackId,
    bytes: Vec<u8>,
    metadata: PackMetadata,
    entries: Vec<PackEntryLocation>,
}

impl SealedPack {
    pub(crate) fn new(
        id: PackId,
        bytes: Vec<u8>,
        metadata: PackMetadata,
        entries: Vec<PackEntryLocation>,
    ) -> Self {
        Self {
            id,
            bytes,
            metadata,
            entries,
        }
    }

    /// Returns the pack ID.
    pub const fn id(&self) -> PackId {
        self.id
    }

    /// Returns the complete sealed bytes without copying them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Alias for [`Self::as_bytes`].
    pub fn bytes(&self) -> &[u8] {
        self.as_bytes()
    }

    /// Returns the complete pack length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the pack contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns verified summary metadata for this pack.
    pub const fn metadata(&self) -> PackMetadata {
        self.metadata
    }

    /// Returns the entry locations in file order.
    pub fn entries(&self) -> &[PackEntryLocation] {
        &self.entries
    }

    /// Consumes the pack and returns its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for SealedPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedPack")
            .field("id", &self.id)
            .field("length", &self.bytes.len())
            .field("entry_count", &self.entries.len())
            .finish()
    }
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
