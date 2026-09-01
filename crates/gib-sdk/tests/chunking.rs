use gib::{CHUNKING_READ_BUFFER_SIZE, ChunkId, ChunkingConfiguration, ChunkingError, chunk_reader};
use serde::Deserialize;
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const BOUNDARY_VECTOR: &str =
    include_str!("../../../tests/fixtures/chunking/v1/boundary-vector.toml");

#[derive(Debug, Deserialize)]
struct BoundaryFixture {
    version: u16,
    algorithm: String,
    window_size: usize,
    min_size: u64,
    target_size: u64,
    max_size: u64,
    input_hex: String,
    chunk_offsets: Vec<u64>,
    chunk_lengths: Vec<usize>,
    chunk_ids: Vec<String>,
    boundaries: Vec<String>,
}

#[test]
fn version_1_boundary_vector_matches_bytes_offsets_lengths_and_ids() {
    let fixture: BoundaryFixture =
        toml::from_str(BOUNDARY_VECTOR).expect("boundary fixture should parse");
    let configuration = ChunkingConfiguration::from_parts(
        fixture.version,
        &fixture.algorithm,
        fixture.min_size,
        fixture.target_size,
        fixture.max_size,
    )
    .expect("fixture policy should be supported");
    assert_eq!(configuration.window_size(), fixture.window_size);
    let input = decode_hex(&fixture.input_hex);
    let chunks = chunk_reader(input.as_slice(), configuration)
        .collect::<Result<Vec<_>, ChunkingError>>()
        .expect("fixture input should chunk");

    assert_eq!(chunks.len(), fixture.chunk_offsets.len());
    assert_eq!(chunks.len(), fixture.chunk_lengths.len());
    assert_eq!(chunks.len(), fixture.chunk_ids.len());
    assert_eq!(chunks.len(), fixture.boundaries.len());
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.offset(), fixture.chunk_offsets[index]);
        assert_eq!(chunk.len(), fixture.chunk_lengths[index]);
        assert_eq!(
            chunk.id(),
            ChunkId::from_hex(&fixture.chunk_ids[index]).expect("valid ID")
        );
        assert_eq!(chunk.boundary().to_string(), fixture.boundaries[index]);
    }
}

#[test]
fn encode_decode_properties_preserve_content_ids_and_boundaries() {
    let configuration = ChunkingConfiguration::new(8, 16, 32).expect("valid policy");
    for seed in 0..32_u64 {
        for length in [0, 1, 7, 8, 15, 16, 31, 32, 33, 64, 127, 1024, 8192] {
            let input = deterministic_bytes(seed, length);
            let first = collect_chunks(&input, configuration);
            let second = collect_chunks(&input, configuration);
            assert_eq!(first, second, "seed={seed} length={length}");

            let mut reconstructed = Vec::with_capacity(input.len());
            for (index, chunk) in first.iter().enumerate() {
                assert_eq!(chunk.id(), ChunkId::from_content(chunk.as_bytes()));
                assert_eq!(chunk.offset(), reconstructed.len() as u64);
                if index + 1 < first.len() {
                    assert!(chunk.len() >= configuration.min_size() as usize);
                    assert!(chunk.len() <= configuration.max_size() as usize);
                    assert!(!chunk.is_final());
                }
                reconstructed.extend_from_slice(chunk.as_bytes());
            }
            assert_eq!(reconstructed, input, "seed={seed} length={length}");
        }
    }
}

#[test]
fn boundaries_are_independent_of_source_read_sizes() {
    let configuration = ChunkingConfiguration::new(64, 128, 256).expect("valid policy");
    let input = deterministic_bytes(0xfeed_face, 32_000);
    let regular = collect_chunks(&input, configuration);
    let variable = chunk_reader(VariableReadReader::new(input.clone()), configuration)
        .collect::<Result<Vec<_>, ChunkingError>>()
        .expect("variable source should chunk");
    assert_eq!(regular, variable);
}

#[test]
fn one_byte_insertion_preserves_most_later_chunk_ids() {
    let configuration = ChunkingConfiguration::new(64, 128, 256).expect("valid policy");
    let original = deterministic_bytes(0x1234_5678, 100_000);
    let mut shifted = original.clone();
    shifted.insert(19, 0xa5);
    let original_chunks = collect_chunks(&original, configuration);
    let shifted_chunks = collect_chunks(&shifted, configuration);
    let original_ids = original_chunks
        .iter()
        .map(|chunk| chunk.id())
        .collect::<Vec<_>>();
    let shifted_ids = shifted_chunks
        .iter()
        .map(|chunk| chunk.id())
        .collect::<Vec<_>>();
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
fn large_logical_reader_streams_without_an_input_allocation() {
    let length = 64 * 1024 * 1024_u64;
    let configuration =
        ChunkingConfiguration::new(64 * 1024, 128 * 1024, 256 * 1024).expect("valid policy");
    let mut reader = chunk_reader(std::io::repeat(0x42).take(length), configuration);
    let mut total = 0_u64;
    let mut chunks = 0_usize;
    while let Some(chunk) = reader
        .next()
        .transpose()
        .expect("logical source should chunk")
    {
        total += chunk.len() as u64;
        chunks += 1;
    }
    assert_eq!(total, length);
    assert!(chunks > 1);
}

#[test]
fn source_read_requests_are_bounded() {
    let requested = Arc::new(AtomicUsize::new(0));
    let source = BoundedRequestReader {
        remaining: 2 * CHUNKING_READ_BUFFER_SIZE + 11,
        requested: Arc::clone(&requested),
    };
    let configuration = ChunkingConfiguration::new(32, 64, 128).expect("valid policy");
    let result = chunk_reader(source, configuration).collect::<Result<Vec<_>, ChunkingError>>();
    assert!(result.is_ok());
    assert!(requested.load(Ordering::Acquire) <= CHUNKING_READ_BUFFER_SIZE);
}

#[test]
fn cancellation_is_reported_without_reading_another_buffer() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let source = CountingReader {
        remaining: 2 * CHUNKING_READ_BUFFER_SIZE,
        reads: Arc::clone(&reads),
    };
    let callback_cancelled = Arc::clone(&cancelled);
    let mut chunker = gib::Chunker::with_cancellation(
        source,
        ChunkingConfiguration::new(32, 64, 128).expect("valid policy"),
        move || callback_cancelled.load(Ordering::Acquire),
    );
    let _ = chunker.next_chunk().expect("first step should succeed");
    cancelled.store(true, Ordering::Release);
    let error = chunker
        .next_chunk()
        .expect_err("cancelled chunking should fail");
    assert!(error.is_cancelled());
    assert!(reads.load(Ordering::Acquire) <= 1);
}

#[test]
fn invalid_source_read_count_is_rejected() {
    let source = InvalidReadCount;
    let configuration = ChunkingConfiguration::new(8, 16, 32).expect("valid policy");
    let error = chunk_reader(source, configuration)
        .next()
        .expect("source should yield an error")
        .expect_err("invalid source count should fail");
    assert!(matches!(error, ChunkingError::InvalidSourceRead));
}

#[test]
#[ignore = "multi-gigabyte logical-stream stress test"]
fn multi_gigabyte_logical_stream_does_not_allocate_the_input() {
    let length = 2 * 1024 * 1024 * 1024_u64;
    let configuration = ChunkingConfiguration::default();
    let mut reader = chunk_reader(std::io::repeat(0x42).take(length), configuration);
    let mut total = 0_u64;
    while let Some(chunk) = reader
        .next()
        .transpose()
        .expect("logical source should chunk")
    {
        total += chunk.len() as u64;
    }
    assert_eq!(total, length);
}

#[cfg(feature = "async")]
#[test]
fn async_reader_uses_the_same_boundaries() {
    use futures_util::io::Cursor;
    use futures_util::stream::Stream;
    use futures_util::task::noop_waker_ref;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    let input = deterministic_bytes(0xabcdef, 10_000);
    let configuration = ChunkingConfiguration::new(64, 128, 256).expect("valid policy");
    let expected = collect_chunks(&input, configuration);
    let mut stream = gib::AsyncChunker::new(Cursor::new(input), configuration);
    let mut pinned = Pin::new(&mut stream);
    let mut context = Context::from_waker(noop_waker_ref());
    let mut actual = Vec::new();
    loop {
        match pinned.as_mut().poll_next(&mut context) {
            Poll::Ready(Some(Ok(chunk))) => actual.push(chunk),
            Poll::Ready(Some(Err(error))) => panic!("async chunking failed: {error}"),
            Poll::Ready(None) => break,
            Poll::Pending => panic!("cursor unexpectedly returned pending"),
        }
    }
    assert_eq!(actual, expected);
}

fn collect_chunks(input: &[u8], configuration: ChunkingConfiguration) -> Vec<gib::Chunk> {
    chunk_reader(input, configuration)
        .collect::<Result<Vec<_>, ChunkingError>>()
        .expect("input should chunk")
}

fn deterministic_bytes(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state =
                state.wrapping_add(0x9e37_79b9_7f4a_7c15).rotate_left(17) ^ 0xa076_1d64_78bd_642f;
            (state ^ (state >> 29) ^ (state >> 47)) as u8
        })
        .collect()
}

fn decode_hex(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len() % 2, 0, "fixture hex should have even length");
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (hex_value(pair[0]) << 4) | hex_value(pair[1]))
        .collect()
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("fixture contains non-hexadecimal input"),
    }
}

struct VariableReadReader {
    bytes: Vec<u8>,
    offset: usize,
    next_sizes: &'static [usize],
    next_index: usize,
}

impl VariableReadReader {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            offset: 0,
            next_sizes: &[1, 7, 4096, 3, 65_535, 17],
            next_index: 0,
        }
    }
}

impl Read for VariableReadReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset == self.bytes.len() {
            return Ok(0);
        }
        let requested = self.next_sizes[self.next_index % self.next_sizes.len()];
        self.next_index += 1;
        let amount = requested
            .min(buffer.len())
            .min(self.bytes.len() - self.offset);
        buffer[..amount].copy_from_slice(&self.bytes[self.offset..self.offset + amount]);
        self.offset += amount;
        Ok(amount)
    }
}

struct BoundedRequestReader {
    remaining: usize,
    requested: Arc<AtomicUsize>,
}

impl Read for BoundedRequestReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.requested.fetch_max(buffer.len(), Ordering::AcqRel);
        let amount = self.remaining.min(buffer.len()).min(4096);
        buffer[..amount].fill(0x42);
        self.remaining -= amount;
        Ok(amount)
    }
}

struct CountingReader {
    remaining: usize,
    reads: Arc<AtomicUsize>,
}

impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        let amount = self.remaining.min(buffer.len());
        buffer[..amount].fill(0x42);
        self.remaining -= amount;
        Ok(amount)
    }
}

struct InvalidReadCount;

impl Read for InvalidReadCount {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        Ok(buffer.len() + 1)
    }
}
