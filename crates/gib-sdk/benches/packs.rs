use gib::{
    CancellationToken, ChunkId, PackBuilder, PackConfiguration, PackEntryInput, SdkResult,
    SealedPack,
};
use std::hint::black_box;
use std::time::Instant;

const DEFAULT_SIZE_MIB: u64 = 64;
const ENTRY_SIZE: usize = 256 * 1024;

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
    source_bytes: u64,
    packed_bytes: u64,
    entries: u64,
    packs: u64,
}

fn main() {
    let size_mib = std::env::var("GIB_PACK_BENCH_MIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SIZE_MIB);
    let Some(source_bytes) = size_mib.checked_mul(1024 * 1024) else {
        eprintln!("GIB_PACK_BENCH_MIB is too large");
        return;
    };
    let configuration = match PackConfiguration::new(8 * 1024 * 1024, 16 * 1024 * 1024) {
        Ok(configuration) => configuration,
        Err(error) => {
            eprintln!("could not construct benchmark configuration: {error}");
            return;
        }
    };
    println!(
        "pack benchmark: source_mib={size_mib} entry_size={} target={} max={}",
        ENTRY_SIZE,
        configuration.target_size(),
        configuration.max_size()
    );
    for dataset in Dataset::ALL {
        report(dataset, source_bytes, configuration);
    }
}

fn report(dataset: Dataset, source_bytes: u64, configuration: PackConfiguration) {
    let started = Instant::now();
    let result = run(dataset, source_bytes, configuration);
    let elapsed = started.elapsed();
    match result {
        Ok(stats) => {
            let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
            let throughput = stats.source_bytes as f64 / (1024.0 * 1024.0) / seconds;
            println!(
                "dataset={} source_bytes={} packed_bytes={} entries={} packs={} elapsed_ms={} throughput_mib_s={throughput:.2}",
                dataset.name(),
                black_box(stats.source_bytes),
                black_box(stats.packed_bytes),
                black_box(stats.entries),
                black_box(stats.packs),
                elapsed.as_secs_f64() * 1000.0,
            );
        }
        Err(error) => eprintln!("dataset={} failed={error}", dataset.name()),
    }
}

fn run(
    dataset: Dataset,
    source_bytes: u64,
    configuration: PackConfiguration,
) -> SdkResult<BenchmarkStats> {
    let cancellation = CancellationToken::new();
    let mut builder = PackBuilder::new(configuration);
    let mut stats = BenchmarkStats {
        source_bytes: 0,
        packed_bytes: 0,
        entries: 0,
        packs: 0,
    };
    let mut publisher = |pack: &SealedPack| {
        stats.packed_bytes += pack.len() as u64;
        stats.packs += 1;
        black_box(pack.id());
        Ok(())
    };
    let entries = GeneratedEntries::new(dataset, source_bytes);
    stats.entries = builder.add_stream(entries, &mut publisher, &cancellation)?;
    stats.source_bytes = stats.entries * ENTRY_SIZE as u64;
    Ok(stats)
}

struct GeneratedEntries {
    dataset: Dataset,
    remaining: u64,
    index: u64,
}

impl GeneratedEntries {
    fn new(dataset: Dataset, source_bytes: u64) -> Self {
        Self {
            dataset,
            remaining: source_bytes.div_ceil(ENTRY_SIZE as u64),
            index: 0,
        }
    }
}

impl Iterator for GeneratedEntries {
    type Item = PackEntryInput;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let index = self.index;
        self.index += 1;
        self.remaining -= 1;
        let mut payload = vec![0_u8; ENTRY_SIZE];
        for (offset, byte) in payload.iter_mut().enumerate() {
            *byte = match self.dataset {
                Dataset::Repetitive => 0x5a,
                Dataset::Random => random_byte(index, offset as u64),
                Dataset::Shifted => random_byte(index, offset as u64 + index),
            };
        }
        let id = ChunkId::from_content(&payload);
        PackEntryInput::new(id, payload.len() as u64, payload).ok()
    }
}

fn random_byte(entry: u64, offset: u64) -> u8 {
    let mut value = entry
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(offset)
        .wrapping_add(0x4752_4942);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    (value ^ (value >> 31)) as u8
}
