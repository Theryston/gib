use gib::{
    BackupBudgets, BackupRequest, ChunkingConfiguration, Client, MemoryStorage, PackConfiguration,
    PackIndexConfiguration, RepositoryIdentity, RepositoryInitRequest, RepositoryKey, TreeNode,
};
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const DEFAULT_FILES: usize = 256;
const DEFAULT_FILE_KIB: usize = 64;
const DEFAULT_ROUNDS: usize = 20;
const DEFAULT_MEMORY_MIB: usize = 64;

static NEXT_DATASET_ID: AtomicU64 = AtomicU64::new(1);

struct Dataset {
    path: PathBuf,
}

impl Dataset {
    fn create(file_count: usize, file_size: usize) -> std::io::Result<Self> {
        let id = NEXT_DATASET_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("gib-delta-benchmark-{}-{id}", std::process::id()));
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

fn traverse_count(
    storage: &MemoryStorage,
    snapshot: &gib::SnapshotReference,
) -> Result<(usize, u64), Box<dyn Error>> {
    let bytes = storage.read_object(snapshot.as_str())?;
    let header = gib::decode_snapshot_object(&bytes)?;
    let root = header
        .root_tree()
        .ok_or("snapshot has no root tree")?
        .as_str()
        .to_owned();
    let mut files = 0_usize;
    let mut bytes_total = 0_u64;
    let mut stack = vec![root];
    while let Some(key) = stack.pop() {
        match gib::decode_tree_node_object(&storage.read_object(&key)?)? {
            TreeNode::Directory(directory) => {
                for entry in directory.entries() {
                    stack.push(format!("trees/{}", entry.reference().id().as_str()));
                }
            }
            TreeNode::RegularFile(file) => {
                files += 1;
                bytes_total += file.size();
            }
            TreeNode::SymbolicLink(_) => {}
            _ => return Err("unknown tree node kind".into()),
        }
    }
    Ok((files, bytes_total))
}

fn main() {
    let file_count = env_usize("GIB_DELTA_BENCH_FILES", DEFAULT_FILES);
    let file_kib = env_usize("GIB_DELTA_BENCH_FILE_KIB", DEFAULT_FILE_KIB);
    let rounds = env_usize("GIB_DELTA_BENCH_ROUNDS", DEFAULT_ROUNDS);
    let memory_mib = env_usize("GIB_DELTA_BENCH_MEMORY_MIB", DEFAULT_MEMORY_MIB);
    let Some(file_size) = file_kib.checked_mul(1024) else {
        eprintln!("GIB_DELTA_BENCH_FILE_KIB is too large");
        return;
    };
    if file_count == 0 || file_size == 0 || rounds == 0 || memory_mib == 0 {
        eprintln!("delta benchmark sizes must be greater than zero");
        return;
    }
    let dataset = match Dataset::create(file_count, file_size) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("could not create benchmark dataset: {error}");
            return;
        }
    };
    match run(dataset.path(), file_count, rounds, memory_mib) {
        Ok(()) => {}
        Err(error) => eprintln!("path delta benchmark failed: {error}"),
    }
}

fn run(
    root: &Path,
    file_count: usize,
    rounds: usize,
    memory_mib: usize,
) -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = client.initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("path-delta-benchmark")?,
            RepositoryKey::new("benchmark")?,
        ),
    )?;
    let memory_bytes = memory_mib
        .checked_mul(1024 * 1024)
        .ok_or("memory budget overflowed")?;
    let base = BackupRequest::new(root)
        .with_message("path delta benchmark")
        .with_budgets(BackupBudgets::with_queue_capacity(
            memory_bytes,
            4,
            32,
            4,
            4,
        )?)
        .with_chunking(ChunkingConfiguration::new(
            64 * 1024,
            256 * 1024,
            1024 * 1024,
        )?)
        .with_pack_configuration(PackConfiguration::new(8 * 1024 * 1024, 16 * 1024 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(1024 * 1024)?);
    let mut references = Vec::new();
    let mut backup_ms = 0_f64;
    for round in 0..rounds {
        fs::write(
            root.join(format!("file-{:06}.bin", round % file_count)),
            vec![round as u8; 1024],
        )?;
        let started = Instant::now();
        let result = client.backup(
            repository.clone(),
            base.clone().with_created_at(1_700_000_000 + round as u64),
        )?;
        backup_ms += started.elapsed().as_secs_f64() * 1000.0;
        black_box(result.metrics());
        references.push(result.snapshot().clone());
    }
    let latest = references.last().ok_or("no snapshots published")?.clone();
    let delta_bytes: u64 = storage
        .objects()?
        .iter()
        .filter(|key| key.starts_with("path-deltas/"))
        .try_fold(0_u64, |total, key| {
            Ok::<u64, Box<dyn Error>>(total + storage.read_object(key)?.len() as u64)
        })?;
    let checkpoint_bytes: u64 = storage
        .objects()?
        .iter()
        .filter(|key| key.starts_with("checkpoints/"))
        .try_fold(0_u64, |total, key| {
            Ok::<u64, Box<dyn Error>>(total + storage.read_object(key)?.len() as u64)
        })?;
    let started = Instant::now();
    let rebuilt = repository.rebuild_path_state(&latest)?;
    let rebuild_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let (files, bytes_total) = traverse_count(&storage, &latest)?;
    let traverse_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        rebuilt.entries().len(),
        files + 1,
        "rebuild must match traversal"
    );
    let _ = black_box(bytes_total);
    println!(
        "path delta benchmark files={file_count} rounds={rounds} backups_ms={backup_ms:.2} delta_bytes={delta_bytes} checkpoint_bytes={checkpoint_bytes} rebuild_ms={rebuild_ms:.2} traverse_ms={traverse_ms:.2} rebuilt_paths={} deltas_applied={} checkpoint_used={}",
        black_box(rebuilt.entries().len()),
        black_box(rebuilt.deltas_applied()),
        black_box(rebuilt.checkpoint().is_some()),
    );
    Ok(())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}
