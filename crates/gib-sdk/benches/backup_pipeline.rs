use gib::{
    BackupBudgets, BackupMetrics, BackupRequest, ChunkingConfiguration, Client, MemoryStorage,
    PackConfiguration, PackIndexConfiguration, RepositoryIdentity, RepositoryInitRequest,
    RepositoryKey,
};
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const DEFAULT_FILE_COUNT: usize = 128;
const DEFAULT_FILE_KIB: usize = 64;
const DEFAULT_RUNS: usize = 1;
const DEFAULT_MEMORY_MIB: usize = 64;

static NEXT_DATASET_ID: AtomicU64 = AtomicU64::new(1);

struct Dataset {
    path: PathBuf,
}

impl Dataset {
    fn create(file_count: usize, file_size: usize) -> std::io::Result<Self> {
        let id = NEXT_DATASET_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("gib-backup-benchmark-{}-{id}", std::process::id()));
        fs::create_dir(&path)?;
        let mut payload = vec![0_u8; file_size];
        for (offset, byte) in payload.iter_mut().enumerate() {
            *byte = ((offset as u64).wrapping_mul(31) ^ (offset as u64 / 97)) as u8;
        }
        for index in 0..file_count {
            let mut file_payload = payload.clone();
            if let Some(first) = file_payload.first_mut() {
                *first ^= index as u8;
            }
            if let Err(error) = fs::write(path.join(format!("file-{index:06}.bin")), file_payload) {
                let _ = fs::remove_dir_all(&path);
                return Err(error);
            }
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Dataset {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn main() {
    let file_count = env_usize("GIB_BACKUP_BENCH_FILES", DEFAULT_FILE_COUNT);
    let file_kib = env_usize("GIB_BACKUP_BENCH_FILE_KIB", DEFAULT_FILE_KIB);
    let runs = env_usize("GIB_BACKUP_BENCH_RUNS", DEFAULT_RUNS);
    let memory_mib = env_usize("GIB_BACKUP_BENCH_MEMORY_MIB", DEFAULT_MEMORY_MIB);
    let Some(file_size) = file_kib.checked_mul(1024) else {
        eprintln!("GIB_BACKUP_BENCH_FILE_KIB is too large");
        return;
    };
    let Some(source_bytes) = file_count.checked_mul(file_size) else {
        eprintln!("backup benchmark dataset size overflowed");
        return;
    };
    if file_count == 0 || file_size == 0 || runs == 0 || memory_mib == 0 {
        eprintln!("backup benchmark sizes must be greater than zero");
        return;
    }

    let dataset = match Dataset::create(file_count, file_size) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("could not create benchmark dataset: {error}");
            return;
        }
    };
    println!(
        "backup pipeline benchmark files={file_count} file_kib={file_kib} source_mib={:.2} runs={runs} memory_mib={memory_mib}",
        source_bytes as f64 / (1024.0 * 1024.0)
    );
    for run in 0..runs {
        match run_once(dataset.path(), file_count, source_bytes, memory_mib) {
            Ok((elapsed, metrics)) => report(run + 1, source_bytes, elapsed, metrics),
            Err(error) => {
                eprintln!("backup pipeline benchmark run={} failed: {error}", run + 1);
                return;
            }
        }
    }
}

fn run_once(
    root: &Path,
    file_count: usize,
    source_bytes: usize,
    memory_mib: usize,
) -> Result<(std::time::Duration, BackupMetrics), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = client.initialize_repository(
        storage,
        RepositoryInitRequest::new(
            RepositoryIdentity::new("backup-pipeline-benchmark")?,
            RepositoryKey::new("benchmark")?,
        ),
    )?;
    let memory_bytes = memory_mib.checked_mul(1024 * 1024).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "memory budget overflowed")
    })?;
    let budgets = BackupBudgets::with_queue_capacity(memory_bytes, 4, 32, 4, 4)?;
    let request = BackupRequest::new(root)
        .with_message("backup pipeline benchmark")
        .with_created_at(1_700_000_000)
        .with_budgets(budgets)
        .with_chunking(ChunkingConfiguration::new(
            64 * 1024,
            256 * 1024,
            1024 * 1024,
        )?)
        .with_pack_configuration(PackConfiguration::new(8 * 1024 * 1024, 16 * 1024 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(1024 * 1024)?);
    let started = Instant::now();
    let result = client.backup(repository, request)?;
    let elapsed = started.elapsed();
    if result.metrics().files() != file_count as u64
        || result.metrics().total_size() != source_bytes as u64
    {
        return Err(Box::new(gib::SdkError::InvalidRequest {
            field: "backup.benchmark_dataset",
            reason: "captured statistics do not match the generated dataset",
        }));
    }
    Ok((elapsed, result.metrics()))
}

fn report(run: usize, source_bytes: usize, elapsed: std::time::Duration, metrics: BackupMetrics) {
    let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let throughput = source_bytes as f64 / (1024.0 * 1024.0) / seconds;
    println!(
        "run={run} bytes={} chunks={} packs={} index_shards={} elapsed_ms={:.2} throughput_mib_s={throughput:.2} peak_memory_bytes={} peak_cpu={} peak_fds={} peak_network={}",
        black_box(source_bytes),
        black_box(metrics.chunks()),
        black_box(metrics.packs()),
        black_box(metrics.index_shards()),
        elapsed.as_secs_f64() * 1000.0,
        black_box(metrics.peak_memory_bytes()),
        black_box(metrics.peak_cpu_workers()),
        black_box(metrics.peak_open_file_descriptors()),
        black_box(metrics.peak_network_requests()),
    );
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}
