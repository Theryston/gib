use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

#[cfg(feature = "async")]
use futures_util::io::AsyncRead;
#[cfg(feature = "async")]
use futures_util::stream::Stream;
#[cfg(feature = "async")]
use std::pin::Pin;
#[cfg(feature = "async")]
use std::task::{Context, Poll};

use super::configuration::MAX_CHUNK_SIZE_BYTES as MAX_CONFIGURED_CHUNK_SIZE_BYTES;

/// The version of the content-defined chunking policy.
pub const CURRENT_CHUNKING_VERSION: u16 = 1;

/// The stable algorithm name recorded with the chunking policy.
pub const CONTENT_DEFINED_CHUNKING_ALGORITHM: &str = "buzhash";

/// The fixed rolling window length used by BuzHash v1.
pub const BUZHASH_WINDOW_SIZE: usize = 64;

/// The deterministic seed used to derive the BuzHash byte table.
pub const BUZHASH_TABLE_SEED: u64 = 0x4752_4942_4344_4331;

/// The buffer size used for every source read.
pub const CHUNKING_READ_BUFFER_SIZE: usize = 64 * 1024;

/// The number of buffers retained by the bounded chunk buffer pool.
pub const CHUNK_BUFFER_POOL_CAPACITY: usize = 2;

/// The default minimum content-defined chunk size.
pub const DEFAULT_MIN_CHUNK_SIZE_BYTES: u64 = 256 * 1024;

/// The default target content-defined chunk size.
pub const DEFAULT_TARGET_CHUNK_SIZE_BYTES: u64 = 1024 * 1024;

/// The default maximum content-defined chunk size.
pub const DEFAULT_MAX_CHUNK_SIZE_BYTES: u64 = 4 * 1024 * 1024;

/// The maximum content-defined chunk size accepted by the SDK.
pub const MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES: u64 = MAX_CONFIGURED_CHUNK_SIZE_BYTES;

const CHUNK_ID_DOMAIN_SEPARATOR: &[u8] = b"GIB chunk content\0";
const CHUNKING_POLICY_DOMAIN_SEPARATOR: &[u8] = b"GIB chunking policy\0";

/// A validated content-defined chunking policy.
///
/// The policy is part of backup reproducibility metadata. The minimum and
/// maximum values are hard boundaries for every non-final chunk. The target is
/// used to derive the BuzHash fingerprint mask and therefore controls the
/// expected average chunk size; it is not an exact boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkingConfiguration {
    version: u16,
    min_size: u64,
    target_size: u64,
    max_size: u64,
    max_size_usize: usize,
    boundary_mask: u64,
}

impl ChunkingConfiguration {
    /// Creates the current BuzHash policy with validated boundaries.
    pub fn new(
        min_size: u64,
        target_size: u64,
        max_size: u64,
    ) -> Result<Self, ChunkingConfigurationError> {
        Self::from_parts(
            CURRENT_CHUNKING_VERSION,
            CONTENT_DEFINED_CHUNKING_ALGORITHM,
            min_size,
            target_size,
            max_size,
        )
    }

    /// Creates the default BuzHash v1 policy.
    pub const fn default_policy() -> Self {
        Self {
            version: CURRENT_CHUNKING_VERSION,
            min_size: DEFAULT_MIN_CHUNK_SIZE_BYTES,
            target_size: DEFAULT_TARGET_CHUNK_SIZE_BYTES,
            max_size: DEFAULT_MAX_CHUNK_SIZE_BYTES,
            max_size_usize: DEFAULT_MAX_CHUNK_SIZE_BYTES as usize,
            boundary_mask: boundary_mask_for_target(DEFAULT_TARGET_CHUNK_SIZE_BYTES),
        }
    }

    /// Creates a policy from explicit version and algorithm metadata.
    ///
    /// Unknown versions and algorithms are rejected instead of being
    /// interpreted as the current policy.
    pub fn from_parts(
        version: u16,
        algorithm: &str,
        min_size: u64,
        target_size: u64,
        max_size: u64,
    ) -> Result<Self, ChunkingConfigurationError> {
        if version != CURRENT_CHUNKING_VERSION {
            return Err(ChunkingConfigurationError::UnsupportedVersion);
        }
        if algorithm != CONTENT_DEFINED_CHUNKING_ALGORITHM {
            return Err(ChunkingConfigurationError::UnsupportedAlgorithm);
        }
        if min_size == 0 {
            return Err(ChunkingConfigurationError::MinimumMustBePositive);
        }
        if target_size == 0 {
            return Err(ChunkingConfigurationError::TargetMustBePositive);
        }
        if max_size == 0 {
            return Err(ChunkingConfigurationError::MaximumMustBePositive);
        }
        if min_size > target_size {
            return Err(ChunkingConfigurationError::MinimumExceedsTarget);
        }
        if target_size > max_size {
            return Err(ChunkingConfigurationError::TargetExceedsMaximum);
        }
        if max_size > MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES {
            return Err(ChunkingConfigurationError::SizeExceedsLimit);
        }
        let max_size_usize = usize::try_from(max_size)
            .map_err(|_| ChunkingConfigurationError::SizeExceedsPlatformLimit)?;
        Ok(Self {
            version,
            min_size,
            target_size,
            max_size,
            max_size_usize,
            boundary_mask: boundary_mask_for_target(target_size),
        })
    }

    /// Returns the policy version.
    pub const fn version(self) -> u16 {
        self.version
    }

    /// Returns the algorithm name.
    pub const fn algorithm(self) -> &'static str {
        CONTENT_DEFINED_CHUNKING_ALGORITHM
    }

    /// Returns the rolling window length in bytes.
    pub const fn window_size(self) -> usize {
        BUZHASH_WINDOW_SIZE
    }

    /// Returns the deterministic seed used for the BuzHash byte table.
    pub const fn table_seed(self) -> u64 {
        BUZHASH_TABLE_SEED
    }

    /// Returns the minimum non-final chunk size.
    pub const fn min_size(self) -> u64 {
        self.min_size
    }

    /// Alias for [`Self::min_size`].
    pub const fn min_chunk_size(self) -> u64 {
        self.min_size()
    }

    /// Returns the target average chunk size.
    pub const fn target_size(self) -> u64 {
        self.target_size
    }

    /// Alias for [`Self::target_size`].
    pub const fn target_chunk_size(self) -> u64 {
        self.target_size()
    }

    /// Returns the maximum non-final chunk size.
    pub const fn max_size(self) -> u64 {
        self.max_size
    }

    /// Alias for [`Self::max_size`].
    pub const fn max_chunk_size(self) -> u64 {
        self.max_size()
    }

    /// Returns the low-bit mask used for fingerprint boundaries.
    pub const fn boundary_mask(self) -> u64 {
        self.boundary_mask
    }

    /// Returns the maximum size as a platform allocation size.
    pub(crate) const fn max_size_usize(self) -> usize {
        self.max_size_usize
    }

    /// Returns canonical policy bytes suitable for repository metadata.
    ///
    /// These bytes contain algorithm, version, rolling-window, and boundary
    /// parameters. They do not contain any file content or runtime state.
    pub fn canonical_policy_bytes(self) -> Vec<u8> {
        let algorithm = self.algorithm().as_bytes();
        let algorithm_length = algorithm.len() as u16;
        let mut bytes = Vec::with_capacity(
            CHUNKING_POLICY_DOMAIN_SEPARATOR.len() + 2 + algorithm.len() + 2 + 2 + 8 + 24,
        );
        bytes.extend_from_slice(CHUNKING_POLICY_DOMAIN_SEPARATOR);
        bytes.extend_from_slice(&algorithm_length.to_be_bytes());
        bytes.extend_from_slice(algorithm);
        bytes.extend_from_slice(&self.version.to_be_bytes());
        bytes.extend_from_slice(&(BUZHASH_WINDOW_SIZE as u16).to_be_bytes());
        bytes.extend_from_slice(&BUZHASH_TABLE_SEED.to_be_bytes());
        bytes.extend_from_slice(&self.min_size.to_be_bytes());
        bytes.extend_from_slice(&self.target_size.to_be_bytes());
        bytes.extend_from_slice(&self.max_size.to_be_bytes());
        bytes
    }

    /// Returns the SHA-256 digest of [`Self::canonical_policy_bytes`].
    pub fn policy_digest(self) -> [u8; 32] {
        let digest = Sha256::digest(self.canonical_policy_bytes());
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        bytes
    }
}

impl Default for ChunkingConfiguration {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A validation failure for a content-defined chunking policy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkingConfigurationError {
    /// The minimum size is zero.
    MinimumMustBePositive,
    /// The target size is zero.
    TargetMustBePositive,
    /// The maximum size is zero.
    MaximumMustBePositive,
    /// The minimum size is larger than the target size.
    MinimumExceedsTarget,
    /// The target size is larger than the maximum size.
    TargetExceedsMaximum,
    /// The maximum size exceeds the SDK resource limit.
    SizeExceedsLimit,
    /// The maximum size cannot be represented as a platform allocation size.
    SizeExceedsPlatformLimit,
    /// The persisted chunking version is not supported.
    UnsupportedVersion,
    /// The persisted chunking algorithm is not supported.
    UnsupportedAlgorithm,
}

impl fmt::Display for ChunkingConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MinimumMustBePositive => "chunking minimum size must be greater than zero",
            Self::TargetMustBePositive => "chunking target size must be greater than zero",
            Self::MaximumMustBePositive => "chunking maximum size must be greater than zero",
            Self::MinimumExceedsTarget => "chunking minimum size must not exceed target size",
            Self::TargetExceedsMaximum => "chunking target size must not exceed maximum size",
            Self::SizeExceedsLimit => "chunking maximum size exceeds the SDK limit",
            Self::SizeExceedsPlatformLimit => {
                "chunking maximum size exceeds the platform allocation limit"
            }
            Self::UnsupportedVersion => "chunking policy version is unsupported",
            Self::UnsupportedAlgorithm => "chunking algorithm is unsupported",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ChunkingConfigurationError {}

/// A stable content identifier for one plaintext chunk.
///
/// Chunk IDs are independent of chunk boundaries and chunking policy so the
/// same plaintext can be reused when a surrounding file shifts. The ID is
/// SHA-256 over `GIB chunk content\0` followed by the plaintext bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkId([u8; 32]);

impl ChunkId {
    /// Creates an ID from plaintext content.
    pub fn from_content(content: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CHUNK_ID_DOMAIN_SEPARATOR);
        hasher.update(content);
        Self::from_digest(hasher.finalize().into())
    }

    /// Creates an ID from its raw SHA-256 digest bytes.
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Parses a 64-character hexadecimal chunk ID.
    pub fn from_hex(value: &str) -> Result<Self, ChunkIdError> {
        if value.len() != 64 {
            return Err(ChunkIdError::InvalidLength);
        }
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            let offset = index * 2;
            let high = hex_value(value.as_bytes()[offset]).ok_or(ChunkIdError::InvalidCharacter)?;
            let low =
                hex_value(value.as_bytes()[offset + 1]).ok_or(ChunkIdError::InvalidCharacter)?;
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

impl fmt::Display for ChunkId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex_encode(&self.0))
    }
}

impl AsRef<[u8]> for ChunkId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A malformed hexadecimal chunk ID.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkIdError {
    /// The value does not contain exactly 64 hexadecimal characters.
    InvalidLength,
    /// The value contains a non-hexadecimal character.
    InvalidCharacter,
}

impl fmt::Display for ChunkIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => {
                formatter.write_str("chunk ID must contain 64 hexadecimal characters")
            }
            Self::InvalidCharacter => {
                formatter.write_str("chunk ID contains a non-hexadecimal character")
            }
        }
    }
}

impl std::error::Error for ChunkIdError {}

/// Explains why one chunk boundary was emitted.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChunkBoundary {
    /// The rolling fingerprint matched the configured boundary mask.
    Fingerprint,
    /// The maximum configured size was reached.
    Maximum,
    /// The source reached EOF and emitted the remaining bytes.
    EndOfInput,
}

impl fmt::Display for ChunkBoundary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fingerprint => formatter.write_str("fingerprint"),
            Self::Maximum => formatter.write_str("maximum"),
            Self::EndOfInput => formatter.write_str("end_of_input"),
        }
    }
}

/// One immutable plaintext chunk emitted by a streaming chunker.
pub struct Chunk {
    offset: u64,
    bytes: Vec<u8>,
    id: ChunkId,
    boundary: ChunkBoundary,
    pool: Option<Arc<ChunkBufferPool>>,
}

impl Chunk {
    /// Returns the zero-based byte offset of the chunk in the source.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the exclusive byte offset immediately after the chunk.
    pub fn end_offset(&self) -> u64 {
        self.offset + self.bytes.len() as u64
    }

    /// Returns the chunk length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Alias for [`Self::len`].
    pub fn size(&self) -> usize {
        self.len()
    }

    /// Returns whether the chunk has no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the plaintext bytes without copying.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Alias for [`Self::as_bytes`].
    pub fn bytes(&self) -> &[u8] {
        self.as_bytes()
    }

    /// Returns the stable content ID for the plaintext bytes.
    pub const fn id(&self) -> ChunkId {
        self.id
    }

    /// Returns the reason that ended this chunk.
    pub const fn boundary(&self) -> ChunkBoundary {
        self.boundary
    }

    /// Returns whether this chunk was emitted at source EOF.
    pub const fn is_final(&self) -> bool {
        matches!(self.boundary, ChunkBoundary::EndOfInput)
    }

    /// Consumes the chunk and returns its plaintext bytes.
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.pool = None;
        std::mem::take(&mut self.bytes)
    }

    /// Consumes the chunk and returns offset, ID, boundary, and plaintext.
    pub fn into_parts(mut self) -> (u64, ChunkId, ChunkBoundary, Vec<u8>) {
        self.pool = None;
        let bytes = std::mem::take(&mut self.bytes);
        (self.offset, self.id, self.boundary, bytes)
    }
}

impl Clone for Chunk {
    fn clone(&self) -> Self {
        Self {
            offset: self.offset,
            bytes: self.bytes.clone(),
            id: self.id,
            boundary: self.boundary,
            pool: None,
        }
    }
}

impl fmt::Debug for Chunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Chunk")
            .field("offset", &self.offset)
            .field("length", &self.bytes.len())
            .field("id", &self.id)
            .field("boundary", &self.boundary)
            .finish()
    }
}

impl PartialEq for Chunk {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
            && self.bytes == other.bytes
            && self.id == other.id
            && self.boundary == other.boundary
    }
}

impl Eq for Chunk {}

impl Drop for Chunk {
    fn drop(&mut self) {
        let Some(pool) = self.pool.take() else {
            return;
        };
        let bytes = std::mem::take(&mut self.bytes);
        pool.recycle(bytes);
    }
}

/// Errors raised while reading or chunking a source stream.
#[non_exhaustive]
#[derive(Debug)]
pub enum ChunkingError {
    /// The source returned an I/O error.
    Io(io::Error),
    /// Cooperative cancellation was observed between bounded source units.
    Cancelled,
    /// The source violated the `Read` contract by reporting too many bytes.
    InvalidSourceRead,
    /// The logical source offset exceeded the representable range.
    OffsetOverflow,
}

impl ChunkingError {
    /// Returns whether this error represents cooperative cancellation.
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for ChunkingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "chunk source I/O failed: {error}"),
            Self::Cancelled => formatter.write_str("chunking was cancelled"),
            Self::InvalidSourceRead => {
                formatter.write_str("chunk source returned an invalid byte count")
            }
            Self::OffsetOverflow => formatter.write_str("chunk source offset overflowed"),
        }
    }
}

impl std::error::Error for ChunkingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Cancelled | Self::InvalidSourceRead | Self::OffsetOverflow => None,
        }
    }
}

impl From<io::Error> for ChunkingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Result type returned by synchronous and asynchronous chunkers.
pub type ChunkingResult<T> = Result<T, ChunkingError>;

/// A bounded, deterministic content-defined chunk stream over a synchronous reader.
pub struct Chunker<R> {
    source: R,
    assembler: ChunkAssembler,
    read_buffer: [u8; CHUNKING_READ_BUFFER_SIZE],
    read_offset: usize,
    read_length: usize,
    finished: bool,
    cancellation: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

impl<R: Read> Chunker<R> {
    /// Creates a chunker that reads at most [`CHUNKING_READ_BUFFER_SIZE`] bytes
    /// from the source at a time.
    pub fn new(source: R, configuration: ChunkingConfiguration) -> Self {
        Self::with_optional_cancellation(source, configuration, None)
    }

    /// Creates a chunker with a cooperative cancellation callback.
    ///
    /// The callback is checked before every bounded source read, after each
    /// read, and while scanning the read buffer. A callback cannot interrupt a
    /// source implementation that is already blocked inside one `Read` call.
    pub fn with_cancellation<F>(
        source: R,
        configuration: ChunkingConfiguration,
        cancellation: F,
    ) -> Self
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        Self::with_optional_cancellation(source, configuration, Some(Box::new(cancellation)))
    }

    fn with_optional_cancellation(
        source: R,
        configuration: ChunkingConfiguration,
        cancellation: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        Self {
            source,
            assembler: ChunkAssembler::new(configuration),
            read_buffer: [0_u8; CHUNKING_READ_BUFFER_SIZE],
            read_offset: 0,
            read_length: 0,
            finished: false,
            cancellation,
        }
    }

    /// Returns the immutable policy used by this chunker.
    pub const fn configuration(&self) -> ChunkingConfiguration {
        self.assembler.configuration
    }

    /// Returns the next chunk, or `None` after source EOF.
    pub fn next_chunk(&mut self) -> ChunkingResult<Option<Chunk>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            if self.cancellation_requested() {
                self.finished = true;
                return Err(ChunkingError::Cancelled);
            }

            if self.read_offset < self.read_length {
                let input = &self.read_buffer[self.read_offset..self.read_length];
                let (consumed, chunk) =
                    match self.assembler.consume(input, self.cancellation.as_deref()) {
                        Ok(value) => value,
                        Err(error) => {
                            self.finished = true;
                            return Err(error);
                        }
                    };
                self.read_offset += consumed;
                if let Some(chunk) = chunk {
                    return Ok(Some(chunk));
                }
                continue;
            }

            self.read_offset = 0;
            self.read_length = 0;
            let read = match self.source.read(&mut self.read_buffer) {
                Ok(read) => read,
                Err(error) => {
                    self.finished = true;
                    return Err(ChunkingError::Io(error));
                }
            };
            if read > self.read_buffer.len() {
                self.finished = true;
                return Err(ChunkingError::InvalidSourceRead);
            }
            if self.cancellation_requested() {
                self.finished = true;
                return Err(ChunkingError::Cancelled);
            }
            if read == 0 {
                self.finished = true;
                return Ok(self.assembler.finish().map(Some).unwrap_or(None));
            }
            self.read_length = read;
        }
    }

    /// Consumes the chunker and returns the underlying source.
    pub fn into_inner(self) -> R {
        self.source
    }

    fn cancellation_requested(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|callback| callback())
    }
}

impl<R: Read> Iterator for Chunker<R> {
    type Item = ChunkingResult<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_chunk() {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

/// Compatibility name for [`Chunker`].
pub type ChunkStream<R> = Chunker<R>;

/// Creates a bounded synchronous content-defined chunk stream.
pub fn chunk_reader<R: Read>(source: R, configuration: ChunkingConfiguration) -> Chunker<R> {
    Chunker::new(source, configuration)
}

/// Creates a bounded synchronous content-defined chunk stream with cancellation.
pub fn chunk_reader_with_cancellation<R, F>(
    source: R,
    configuration: ChunkingConfiguration,
    cancellation: F,
) -> Chunker<R>
where
    R: Read,
    F: Fn() -> bool + Send + Sync + 'static,
{
    Chunker::with_cancellation(source, configuration, cancellation)
}

#[cfg(feature = "async")]
/// A bounded content-defined chunk stream over a futures-compatible async reader.
pub struct AsyncChunker<R> {
    source: R,
    assembler: ChunkAssembler,
    read_buffer: [u8; CHUNKING_READ_BUFFER_SIZE],
    read_offset: usize,
    read_length: usize,
    finished: bool,
    cancellation: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

#[cfg(feature = "async")]
impl<R: AsyncRead + Unpin> AsyncChunker<R> {
    /// Creates an asynchronous chunker with bounded source reads.
    pub fn new(source: R, configuration: ChunkingConfiguration) -> Self {
        Self::with_optional_cancellation(source, configuration, None)
    }

    /// Creates an asynchronous chunker with cooperative cancellation.
    pub fn with_cancellation<F>(
        source: R,
        configuration: ChunkingConfiguration,
        cancellation: F,
    ) -> Self
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        Self::with_optional_cancellation(source, configuration, Some(Box::new(cancellation)))
    }

    fn with_optional_cancellation(
        source: R,
        configuration: ChunkingConfiguration,
        cancellation: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        Self {
            source,
            assembler: ChunkAssembler::new(configuration),
            read_buffer: [0_u8; CHUNKING_READ_BUFFER_SIZE],
            read_offset: 0,
            read_length: 0,
            finished: false,
            cancellation,
        }
    }

    /// Returns the immutable policy used by this chunker.
    pub const fn configuration(&self) -> ChunkingConfiguration {
        self.assembler.configuration
    }

    /// Awaits the next chunk, or returns `None` after source EOF.
    pub async fn next_chunk(&mut self) -> ChunkingResult<Option<Chunk>> {
        match futures_util::future::poll_fn(|context| Pin::new(&mut *self).poll_next(context)).await
        {
            Some(result) => result.map(Some),
            None => Ok(None),
        }
    }

    fn cancellation_requested(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|callback| callback())
    }
}

#[cfg(feature = "async")]
impl<R: AsyncRead + Unpin> Stream for AsyncChunker<R> {
    type Item = ChunkingResult<Chunk>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        if this.finished {
            return Poll::Ready(None);
        }
        loop {
            if this.cancellation_requested() {
                this.finished = true;
                return Poll::Ready(Some(Err(ChunkingError::Cancelled)));
            }

            if this.read_offset < this.read_length {
                let input = &this.read_buffer[this.read_offset..this.read_length];
                let result = this.assembler.consume(input, this.cancellation.as_deref());
                let (consumed, chunk) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        this.finished = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                };
                this.read_offset += consumed;
                if let Some(chunk) = chunk {
                    return Poll::Ready(Some(Ok(chunk)));
                }
                continue;
            }

            this.read_offset = 0;
            this.read_length = 0;
            let read = match Pin::new(&mut this.source).poll_read(context, &mut this.read_buffer) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(read)) => read,
                Poll::Ready(Err(error)) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(ChunkingError::Io(error))));
                }
            };
            if read > this.read_buffer.len() {
                this.finished = true;
                return Poll::Ready(Some(Err(ChunkingError::InvalidSourceRead)));
            }
            if this.cancellation_requested() {
                this.finished = true;
                return Poll::Ready(Some(Err(ChunkingError::Cancelled)));
            }
            if read == 0 {
                this.finished = true;
                return Poll::Ready(this.assembler.finish().map(Ok));
            }
            this.read_length = read;
        }
    }
}

#[cfg(feature = "async")]
/// Compatibility name for [`AsyncChunker`].
pub type AsyncChunkStream<R> = AsyncChunker<R>;

#[cfg(feature = "async")]
/// Creates a bounded asynchronous content-defined chunk stream.
pub fn async_chunk_reader<R: AsyncRead + Unpin>(
    source: R,
    configuration: ChunkingConfiguration,
) -> AsyncChunker<R> {
    AsyncChunker::new(source, configuration)
}

#[derive(Debug)]
struct ChunkAssembler {
    configuration: ChunkingConfiguration,
    rolling: BuzHash,
    pool: Arc<ChunkBufferPool>,
    current: Vec<u8>,
    chunk_offset: u64,
    total_offset: u64,
}

impl ChunkAssembler {
    fn new(configuration: ChunkingConfiguration) -> Self {
        let pool = Arc::new(ChunkBufferPool::new(configuration));
        let current = pool.acquire();
        Self {
            configuration,
            rolling: BuzHash::default(),
            pool,
            current,
            chunk_offset: 0,
            total_offset: 0,
        }
    }

    fn consume(
        &mut self,
        input: &[u8],
        cancellation: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> ChunkingResult<(usize, Option<Chunk>)> {
        for (index, byte) in input.iter().copied().enumerate() {
            if index % 1_024 == 0 && cancellation.is_some_and(|callback| callback()) {
                return Err(ChunkingError::Cancelled);
            }
            self.current.push(byte);
            self.rolling.push(byte);
            self.total_offset = self
                .total_offset
                .checked_add(1)
                .ok_or(ChunkingError::OffsetOverflow)?;
            let boundary = if self.current.len() >= self.configuration.max_size_usize() {
                Some(ChunkBoundary::Maximum)
            } else if self.current.len() as u64 >= self.configuration.min_size()
                && self.rolling.matches(self.configuration.boundary_mask())
            {
                Some(ChunkBoundary::Fingerprint)
            } else {
                None
            };
            if let Some(boundary) = boundary {
                return Ok((index + 1, Some(self.take_chunk(boundary))));
            }
        }
        Ok((input.len(), None))
    }

    fn finish(&mut self) -> Option<Chunk> {
        if self.current.is_empty() {
            None
        } else {
            Some(self.take_chunk(ChunkBoundary::EndOfInput))
        }
    }

    fn take_chunk(&mut self, boundary: ChunkBoundary) -> Chunk {
        let bytes = std::mem::replace(&mut self.current, self.pool.acquire());
        let offset = self.chunk_offset;
        self.chunk_offset = self.total_offset;
        let id = ChunkId::from_content(&bytes);
        Chunk {
            offset,
            bytes,
            id,
            boundary,
            pool: Some(Arc::clone(&self.pool)),
        }
    }
}

#[derive(Debug)]
struct ChunkBufferPool {
    buffers: Mutex<Vec<Vec<u8>>>,
    initial_capacity: usize,
    maximum_capacity: usize,
}

impl ChunkBufferPool {
    fn new(configuration: ChunkingConfiguration) -> Self {
        Self {
            buffers: Mutex::new(Vec::with_capacity(CHUNK_BUFFER_POOL_CAPACITY)),
            initial_capacity: configuration.target_size().min(configuration.max_size()) as usize,
            maximum_capacity: configuration.max_size_usize(),
        }
    }

    fn acquire(&self) -> Vec<u8> {
        let mut buffers = match self.buffers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buffers
            .pop()
            .map(|mut buffer| {
                buffer.clear();
                buffer
            })
            .unwrap_or_else(|| Vec::with_capacity(self.initial_capacity))
    }

    fn recycle(&self, mut buffer: Vec<u8>) {
        if buffer.capacity() > self.maximum_capacity {
            return;
        }
        buffer.clear();
        let mut buffers = match self.buffers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if buffers.len() < CHUNK_BUFFER_POOL_CAPACITY {
            buffers.push(buffer);
        }
    }
}

#[derive(Clone, Debug)]
struct BuzHash {
    window: [u8; BUZHASH_WINDOW_SIZE],
    position: usize,
    length: usize,
    hash: u64,
}

impl Default for BuzHash {
    fn default() -> Self {
        Self {
            window: [0_u8; BUZHASH_WINDOW_SIZE],
            position: 0,
            length: 0,
            hash: 0,
        }
    }
}

impl BuzHash {
    fn push(&mut self, byte: u8) {
        if self.length < BUZHASH_WINDOW_SIZE {
            self.window[self.position] = byte;
            self.position = (self.position + 1) % BUZHASH_WINDOW_SIZE;
            self.length += 1;
            self.hash = self.hash.rotate_left(1) ^ BUZHASH_TABLE[byte as usize];
            return;
        }

        let outgoing = self.window[self.position];
        self.window[self.position] = byte;
        self.position = (self.position + 1) % BUZHASH_WINDOW_SIZE;
        self.hash = self.hash.rotate_left(1)
            ^ BUZHASH_TABLE[byte as usize]
            ^ BUZHASH_TABLE[outgoing as usize].rotate_left(BUZHASH_WINDOW_SIZE as u32);
    }

    fn matches(&self, mask: u64) -> bool {
        self.hash & mask == 0
    }
}

const fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut result = value;
    result = (result ^ (result >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    result = (result ^ (result >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    result ^ (result >> 31)
}

const fn build_buzhash_table() -> [u64; 256] {
    let mut table = [0_u64; 256];
    let mut index = 0;
    while index < table.len() {
        table[index] = splitmix64(BUZHASH_TABLE_SEED.wrapping_add(index as u64));
        index += 1;
    }
    table
}

const BUZHASH_TABLE: [u64; 256] = build_buzhash_table();

const fn boundary_mask_for_target(target: u64) -> u64 {
    let mut bits = 0_u32;
    let mut power = 1_u64;
    while power < target && bits < 63 {
        power <<= 1;
        bits += 1;
    }
    if bits == 0 { 0 } else { (1_u64 << bits) - 1 }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn collect_chunks(bytes: &[u8], configuration: ChunkingConfiguration) -> Vec<Chunk> {
        Chunker::new(Cursor::new(bytes.to_vec()), configuration)
            .collect::<ChunkingResult<Vec<_>>>()
            .expect("chunking should succeed")
    }

    #[test]
    fn validates_policy_boundaries_and_rejects_future_metadata() {
        assert_eq!(
            ChunkingConfiguration::new(4, 8, 16)
                .expect("valid policy should construct")
                .boundary_mask(),
            7
        );
        assert_eq!(
            ChunkingConfiguration::from_parts(2, CONTENT_DEFINED_CHUNKING_ALGORITHM, 4, 8, 16),
            Err(ChunkingConfigurationError::UnsupportedVersion)
        );
        assert_eq!(
            ChunkingConfiguration::from_parts(1, "other", 4, 8, 16),
            Err(ChunkingConfigurationError::UnsupportedAlgorithm)
        );
        assert_eq!(
            ChunkingConfiguration::new(8, 4, 16),
            Err(ChunkingConfigurationError::MinimumExceedsTarget)
        );
        assert_eq!(
            ChunkingConfiguration::new(4, 16, 8),
            Err(ChunkingConfigurationError::TargetExceedsMaximum)
        );
    }

    #[test]
    fn empty_and_tiny_sources_have_only_the_allowed_final_chunk() {
        let configuration = ChunkingConfiguration::new(8, 16, 32).expect("valid policy");
        assert!(collect_chunks(&[], configuration).is_empty());
        let chunks = collect_chunks(b"tiny", configuration);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_final());
        assert_eq!(chunks[0].as_bytes(), b"tiny");
    }

    #[test]
    fn non_final_chunks_obey_minimum_and_maximum_boundaries() {
        let configuration = ChunkingConfiguration::new(32, 64, 96).expect("valid policy");
        let input: Vec<u8> = (0..100_000_u32).map(|value| value as u8).collect();
        let chunks = collect_chunks(&input, configuration);
        assert!(!chunks.is_empty());
        for (index, chunk) in chunks.iter().enumerate() {
            if index + 1 != chunks.len() {
                assert!(chunk.len() >= configuration.min_size() as usize);
                assert!(chunk.len() <= configuration.max_size() as usize);
            }
            assert_eq!(
                chunk.offset(),
                chunks[..index].iter().map(Chunk::len).sum::<usize>() as u64
            );
        }
        let reconstructed = chunks
            .iter()
            .flat_map(|chunk| chunk.as_bytes().iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, input);
    }

    #[test]
    fn deterministic_boundaries_and_ids_are_independent_of_source_read_sizes() {
        let configuration = ChunkingConfiguration::new(64, 128, 256).expect("valid policy");
        let input: Vec<u8> = (0..32_000_u32)
            .map(|value| value.wrapping_mul(31) as u8)
            .collect();
        let first = collect_chunks(&input, configuration);
        let second = collect_chunks(&input, configuration);
        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|chunk| chunk.offset()).collect::<Vec<_>>(),
            second
                .iter()
                .map(|chunk| chunk.offset())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn one_byte_insertion_realigns_after_the_rolling_window() {
        let configuration = ChunkingConfiguration::new(64, 128, 256).expect("valid policy");
        let original: Vec<u8> = (0..100_000_u32)
            .map(|value| value.wrapping_mul(17).wrapping_add(11) as u8)
            .collect();
        let mut shifted = original.clone();
        shifted.insert(19, 0xa5);
        let original_chunks = collect_chunks(&original, configuration);
        let shifted_chunks = collect_chunks(&shifted, configuration);
        let original_ids = original_chunks.iter().map(Chunk::id).collect::<Vec<_>>();
        let shifted_ids = shifted_chunks.iter().map(Chunk::id).collect::<Vec<_>>();
        let aligned = original_ids
            .iter()
            .zip(shifted_ids.iter())
            .filter(|(left, right)| left == right)
            .count();
        assert!(
            aligned + 3 >= original_ids.len().min(shifted_ids.len()),
            "aligned={aligned} original={} shifted={}",
            original_ids.len(),
            shifted_ids.len()
        );
    }

    #[test]
    fn cancellation_is_observed_before_another_bounded_read() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let source = CountingReader {
            remaining: 2 * CHUNKING_READ_BUFFER_SIZE,
            reads: Arc::clone(&reads),
            cancelled: Arc::clone(&cancelled),
        };
        let callback_cancelled = Arc::clone(&cancelled);
        let mut chunker = Chunker::with_cancellation(
            source,
            ChunkingConfiguration::new(32, 64, 128).expect("valid policy"),
            move || callback_cancelled.load(Ordering::Acquire),
        );
        let _ = chunker
            .next_chunk()
            .expect("first chunking step should succeed");
        cancelled.store(true, Ordering::Release);
        let error = chunker
            .next_chunk()
            .expect_err("cancellation should stop the next step");
        assert!(error.is_cancelled());
        assert!(reads.load(Ordering::Acquire) <= 1);
    }

    struct CountingReader {
        remaining: usize,
        reads: Arc<AtomicUsize>,
        cancelled: Arc<AtomicBool>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(0);
            }
            let amount = self.remaining.min(buffer.len());
            buffer[..amount].fill(0x42);
            self.remaining -= amount;
            Ok(amount)
        }
    }
}
