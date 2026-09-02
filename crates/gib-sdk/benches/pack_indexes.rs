use gib::{
    PackId, PackIndexCache, PackIndexCacheConfiguration, PackIndexConfiguration, PackIndexEntry,
    PackIndexReader, PackIndexShardBuilder, PackIndexShardId, PackIndexTransform,
};
use std::hint::black_box;
use std::time::Instant;

const DEFAULT_ENTRIES: usize = 50_000;
const PROBES: usize = 10_000;
const SHARD: u8 = 0x7a;

fn main() {
    let entry_count = std::env::var("GIB_PACK_INDEX_BENCH_ENTRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ENTRIES);
    let configuration = match PackIndexConfiguration::new(64 * 1024 * 1024) {
        Ok(configuration) => configuration,
        Err(error) => {
            eprintln!("could not construct benchmark configuration: {error}");
            return;
        }
    };
    let transform = match PackIndexTransform::plain(1) {
        Ok(transform) => transform,
        Err(error) => {
            eprintln!("could not construct transform metadata: {error}");
            return;
        }
    };
    let started = Instant::now();
    let mut builder =
        match PackIndexShardBuilder::new(configuration, PackIndexShardId::from_byte(SHARD)) {
            Ok(builder) => builder,
            Err(error) => {
                eprintln!("could not construct index builder: {error}");
                return;
            }
        };
    let mut chunk_ids = Vec::with_capacity(entry_count);
    for index in 0..entry_count {
        let Some(entry) = make_entry(index, transform) else {
            eprintln!("GIB_PACK_INDEX_BENCH_ENTRIES is too large for the pack limit");
            return;
        };
        chunk_ids.push(entry.chunk_id());
        if let Err(error) = builder.add(entry) {
            eprintln!("could not add benchmark entry: {error}");
            return;
        }
    }
    let shard = match builder.finish() {
        Ok(shard) => shard,
        Err(error) => {
            eprintln!("could not seal benchmark shard: {error}");
            return;
        }
    };
    let build_elapsed = started.elapsed();
    let started = Instant::now();
    let reader = match PackIndexReader::new(shard.as_bytes()) {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("could not verify benchmark shard: {error}");
            return;
        }
    };
    let parse_elapsed = started.elapsed();
    let started = Instant::now();
    let mut found = 0_usize;
    for probe in 0..PROBES {
        if reader.lookup(chunk_ids[probe % chunk_ids.len()]).is_some() {
            found += 1;
        }
    }
    let binary_search_elapsed = started.elapsed();

    let cache_configuration = match PackIndexCacheConfiguration::new(128 * 1024 * 1024, 2) {
        Ok(configuration) => configuration,
        Err(error) => {
            eprintln!("could not construct cache configuration: {error}");
            return;
        }
    };
    let mut cache = PackIndexCache::new(cache_configuration);
    let started = Instant::now();
    if let Err(error) = cache.insert(&shard) {
        eprintln!("could not populate benchmark cache: {error}");
        return;
    }
    let cache_insert_elapsed = started.elapsed();
    let started = Instant::now();
    let mut cached_found = 0_usize;
    for probe in 0..PROBES {
        if cache.lookup(chunk_ids[probe % chunk_ids.len()]).is_some() {
            cached_found += 1;
        }
    }
    let cache_lookup_elapsed = started.elapsed();
    println!(
        "pack-index benchmark entries={} shard_bytes={} records_bytes={} build_ms={:.2} parse_ms={:.2} binary_search_ms={:.2} cache_insert_ms={:.2} cache_lookup_ms={:.2} cold_shard_bytes={} hot_shard_bytes=0 probes={} found={} cached_found={} resident_bytes={}",
        entry_count,
        black_box(shard.len()),
        black_box(reader.metadata().records_length()),
        build_elapsed.as_secs_f64() * 1000.0,
        parse_elapsed.as_secs_f64() * 1000.0,
        binary_search_elapsed.as_secs_f64() * 1000.0,
        cache_insert_elapsed.as_secs_f64() * 1000.0,
        cache_lookup_elapsed.as_secs_f64() * 1000.0,
        shard.len(),
        PROBES,
        found,
        cached_found,
        cache.resident_bytes(),
    );
}

fn make_entry(index: usize, transform: PackIndexTransform) -> Option<PackIndexEntry> {
    let index = u64::try_from(index).ok()?;
    let mut chunk_digest = [0_u8; 32];
    chunk_digest[0] = SHARD;
    chunk_digest[8..16].copy_from_slice(&index.to_be_bytes());
    let pack_digest = [0x91_u8; 32];
    let offset = 64_u64.checked_add(index.checked_mul(104)?)?;
    PackIndexEntry::new(
        gib::ChunkId::from_digest(chunk_digest),
        PackId::from_digest(pack_digest),
        offset,
        offset.checked_add(96)?,
        104,
        1,
        1,
        transform,
    )
    .ok()
}
