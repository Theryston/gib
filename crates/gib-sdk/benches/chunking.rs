use gib::{CHUNKING_READ_BUFFER_SIZE, ChunkingConfiguration, ChunkingError, chunk_reader};
use std::hint::black_box;
use std::io::{self, Read};
use std::time::Instant;

const DEFAULT_SIZE_MIB: u64 = 16;
const FIXED_CHUNK_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
enum Dataset {
    Repetitive,
    Random,
    Shifted,
}

impl Dataset {
    const ALL: [Self; 3] = [Self::Repetitive, Self::Random, Self::Shifted];

    const fn name(self) -> &'static str {
        match self {
            Self::Repetitive => "repetitive",
            Self::Random => "random",
            Self::Shifted => "shifted",
        }
    }
}

#[derive(Clone, Copy)]
struct BenchmarkStats {
    bytes: u64,
    chunks: u64,
}

fn main() {
    let size_mib = std::env::var("GIB_CHUNK_BENCH_MIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SIZE_MIB);
    let Some(length) = size_mib.checked_mul(1024 * 1024) else {
        eprintln!("GIB_CHUNK_BENCH_MIB is too large");
        return;
    };
    let configuration = match ChunkingConfiguration::new(256 * 1024, 1024 * 1024, 4 * 1024 * 1024) {
        Ok(configuration) => configuration,
        Err(error) => {
            eprintln!("could not construct benchmark policy: {error}");
            return;
        }
    };

    println!(
        "chunking benchmark: size_mib={size_mib} read_buffer={} min={} target={} max={}",
        CHUNKING_READ_BUFFER_SIZE,
        configuration.min_size(),
        configuration.target_size(),
        configuration.max_size()
    );
    for dataset in Dataset::ALL {
        report("cdc", dataset, length, || {
            run_cdc(GeneratedReader::new(dataset, length), configuration)
        });
        report("fixed-1m", dataset, length, || {
            run_fixed(GeneratedReader::new(dataset, length))
        });
    }
}

fn report<F>(algorithm: &str, dataset: Dataset, length: u64, run: F)
where
    F: FnOnce() -> Result<BenchmarkStats, ChunkingError>,
{
    let started = Instant::now();
    let result = run();
    let elapsed = started.elapsed();
    match result {
        Ok(stats) => {
            let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
            let mib_per_second = stats.bytes as f64 / (1024.0 * 1024.0) / seconds;
            println!(
                "algorithm={algorithm} dataset={} bytes={} chunks={} elapsed_ms={} throughput_mib_s={mib_per_second:.2}",
                dataset.name(),
                black_box(stats.bytes),
                black_box(stats.chunks),
                elapsed.as_secs_f64() * 1000.0,
            );
        }
        Err(error) => eprintln!(
            "algorithm={algorithm} dataset={} bytes={length} failed={error}",
            dataset.name()
        ),
    }
}

fn run_cdc<R: Read>(
    reader: R,
    configuration: ChunkingConfiguration,
) -> Result<BenchmarkStats, ChunkingError> {
    let mut chunker = chunk_reader(reader, configuration);
    let mut bytes = 0_u64;
    let mut chunks = 0_u64;
    while let Some(chunk) = chunker.next().transpose()? {
        bytes += chunk.len() as u64;
        chunks += 1;
        black_box(chunk.id());
    }
    Ok(BenchmarkStats { bytes, chunks })
}

fn run_fixed<R: Read>(mut reader: R) -> Result<BenchmarkStats, ChunkingError> {
    let mut buffer = [0_u8; CHUNKING_READ_BUFFER_SIZE];
    let mut bytes = 0_u64;
    let mut chunks = 0_u64;
    let mut current = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            if current > 0 {
                chunks += 1;
            }
            black_box(bytes);
            return Ok(BenchmarkStats { bytes, chunks });
        }
        black_box(&buffer[..read]);
        bytes += read as u64;
        current += read as u64;
        while current >= FIXED_CHUNK_SIZE {
            chunks += 1;
            current -= FIXED_CHUNK_SIZE;
        }
    }
}

struct GeneratedReader {
    dataset: Dataset,
    length: u64,
    position: u64,
}

impl GeneratedReader {
    const fn new(dataset: Dataset, length: u64) -> Self {
        Self {
            dataset,
            length,
            position: 0,
        }
    }
}

impl Read for GeneratedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position == self.length {
            return Ok(0);
        }
        let amount = (self.length - self.position).min(buffer.len() as u64) as usize;
        for (index, byte) in buffer[..amount].iter_mut().enumerate() {
            *byte = byte_at(self.dataset, self.position + index as u64);
        }
        self.position += amount as u64;
        Ok(amount)
    }
}

fn byte_at(dataset: Dataset, position: u64) -> u8 {
    match dataset {
        Dataset::Repetitive => {
            const PATTERN: &[u8] = b"gib-content-defined-chunking";
            PATTERN[(position as usize) % PATTERN.len()]
        }
        Dataset::Random => deterministic_byte(position),
        Dataset::Shifted => {
            const INSERTION: u64 = 64 * 1024;
            if position == INSERTION {
                0xa5
            } else if position > INSERTION {
                deterministic_byte(position - 1)
            } else {
                deterministic_byte(position)
            }
        }
    }
}

fn deterministic_byte(position: u64) -> u8 {
    let mut value = position ^ 0x9e37_79b9_7f4a_7c15;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (value ^ (value >> 31)) as u8
}
