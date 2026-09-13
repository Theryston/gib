use gib::{
    BackupBudgets, BackupRequest, ChunkingConfiguration, Client, DeltaOperation, ErrorCode,
    MemoryStorage, PackConfiguration, PackIndexConfiguration, Repository, RepositoryIdentity,
    RepositoryInitRequest, RepositoryKey, Snapshot, SnapshotId, SnapshotReference, TreeNode,
    TreeNodeKind,
};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..32 {
            let path = std::env::temp_dir().join(format!(
                "gib-path-deltas-{}-{id}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a path-delta test directory",
        )
        .into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn request(root: &Path, created_at: u64) -> Result<BackupRequest, Box<dyn Error>> {
    Ok(BackupRequest::new(root)
        .with_message("path delta test")
        .with_created_at(created_at)
        .with_budgets(BackupBudgets::with_queue_capacity(
            16 * 1024 * 1024,
            3,
            8,
            1,
            1,
        )?)
        .with_chunking(ChunkingConfiguration::new(32, 64, 128)?)
        .with_pack_configuration(PackConfiguration::new(4 * 1024, 8 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(1_024)?))
}

fn large_request(root: &Path, created_at: u64) -> Result<BackupRequest, Box<dyn Error>> {
    Ok(BackupRequest::new(root)
        .with_message("path delta test")
        .with_created_at(created_at)
        .with_budgets(BackupBudgets::with_queue_capacity(
            64 * 1024 * 1024,
            3,
            16,
            2,
            2,
        )?)
        .with_chunking(ChunkingConfiguration::new(256, 1_024, 4_096)?)
        .with_pack_configuration(PackConfiguration::new(64 * 1024, 256 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(8_192)?))
}

fn repository(client: &Client, storage: MemoryStorage) -> Result<Repository, Box<dyn Error>> {
    Ok(client.initialize_repository(
        storage,
        RepositoryInitRequest::new(
            RepositoryIdentity::new("path-delta-test")?,
            RepositoryKey::new("test")?,
        ),
    )?)
}

fn snapshot_id(reference: &SnapshotReference) -> Result<SnapshotId, Box<dyn Error>> {
    Ok(reference.snapshot_id()?)
}

/// Walks the authoritative trees of one snapshot into a path map.
fn traverse(
    storage: &MemoryStorage,
    snapshot: &SnapshotReference,
) -> Result<BTreeMap<String, (String, u64)>, Box<dyn Error>> {
    let bytes = storage.read_object(snapshot.as_str())?;
    let header = Snapshot::from_bytes(&bytes)?;
    let root = header
        .root_tree()
        .ok_or("snapshot has no root tree")?
        .as_str()
        .to_owned();
    let mut entries = BTreeMap::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((path, key)) = stack.pop() {
        let node = gib::decode_tree_node_object(&storage.read_object(&key)?)?;
        match node {
            TreeNode::Directory(directory) => {
                entries.insert(path.clone(), (String::from("directory"), 0));
                for entry in directory.entries() {
                    let child = if path.is_empty() {
                        entry.name().as_str().to_owned()
                    } else {
                        format!("{}/{}", path, entry.name().as_str())
                    };
                    let key = format!("trees/{}", entry.reference().id().as_str());
                    stack.push((child, key));
                }
            }
            TreeNode::RegularFile(file) => {
                entries.insert(path, (String::from("file"), file.size()));
            }
            TreeNode::SymbolicLink(_) => {
                entries.insert(path, (String::from("symlink"), 0));
            }
            _ => return Err("unknown tree node kind".into()),
        }
    }
    Ok(entries)
}

/// Rebuilds one snapshot through stored deltas into a comparable path map.
fn rebuild(
    repository: &Repository,
    snapshot: &SnapshotReference,
) -> Result<BTreeMap<String, (String, u64)>, Box<dyn Error>> {
    let state = repository.rebuild_path_state(snapshot)?;
    Ok(state
        .entries()
        .iter()
        .map(|(path, entry)| {
            (
                path.as_str().to_owned(),
                (entry.kind().as_str().to_owned(), entry.size()),
            )
        })
        .collect())
}

fn assert_rebuilt(
    repository: &Repository,
    storage: &MemoryStorage,
    snapshot: &SnapshotReference,
) -> Result<(), Box<dyn Error>> {
    assert_eq!(rebuild(repository, snapshot)?, traverse(storage, snapshot)?);
    Ok(())
}

fn write_file(root: &Path, name: &str, contents: &[u8]) -> Result<(), Box<dyn Error>> {
    let parent = Path::new(name).parent().unwrap_or_else(|| Path::new(""));
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(root.join(parent))?;
    }
    fs::write(root.join(name), contents)?;
    Ok(())
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut value = self.0 | 1;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

fn mutate_source(
    source: &TestDirectory,
    random: &mut XorShift,
    round: usize,
    live: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..3 {
        let name = format!("file-{:03}.bin", random.below(30));
        let contents = vec![(round & 0xff) as u8; 64 + random.below(256)];
        write_file(source.path(), &name, &contents)?;
        if !live.contains(&name) {
            live.push(name);
        }
    }
    for _ in 0..2 {
        if live.is_empty() {
            break;
        }
        let index = random.below(live.len());
        let name = live[index].clone();
        let contents = vec![(round & 0xff) as u8; 32 + random.below(512)];
        write_file(source.path(), &name, &contents)?;
    }
    for _ in 0..random.below(3) {
        if live.is_empty() {
            break;
        }
        let index = random.below(live.len());
        let name = live.remove(index);
        let _ = fs::remove_file(source.path().join(&name));
    }
    if !live.is_empty() && random.below(4) == 0 {
        let index = random.below(live.len());
        let old = live[index].clone();
        let new = format!("renamed-{old}");
        fs::rename(source.path().join(&old), source.path().join(&new))?;
        live[index] = new;
    }
    Ok(())
}

#[test]
fn delta_replay_matches_traversal_across_randomized_sequences() -> Result<(), Box<dyn Error>> {
    for seed in [7_u64, 42, 99] {
        let source = TestDirectory::new()?;
        let storage = MemoryStorage::new();
        let client = Client::default();
        let repository = repository(&client, storage.clone())?;
        let mut random = XorShift(seed);
        let mut live = Vec::new();
        let mut history = Vec::new();
        let mut previous: Option<SnapshotId> = None;
        for round in 0..10_usize {
            mutate_source(&source, &mut random, round, &mut live)?;
            let result = client.backup(
                repository.clone(),
                request(source.path(), 5_000 + round as u64)?,
            )?;
            let current = snapshot_id(result.snapshot())?;
            let delta = repository.read_path_delta(result.snapshot())?;
            assert_eq!(delta.snapshot(), &current);
            assert_eq!(delta.parent().cloned(), previous);
            assert_eq!(delta.generation(), round as u64 + 1);
            history.push(result.snapshot().clone());
            for reference in &history {
                assert_rebuilt(&repository, &storage, reference)?;
            }
            previous = Some(current);
        }
    }
    Ok(())
}

#[test]
fn rename_is_recorded_as_delete_plus_add() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    write_file(source.path(), "a.txt", b"renamed content")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    client.backup(repository.clone(), request(source.path(), 6_000)?)?;
    fs::rename(source.path().join("a.txt"), source.path().join("b.txt"))?;
    let renamed = client.backup(repository.clone(), request(source.path(), 6_001)?)?;
    let delta = repository.read_path_delta(renamed.snapshot())?;
    let operations: Vec<(String, DeltaOperation)> = delta
        .records()
        .iter()
        .map(|record| (record.path().as_str().to_owned(), record.operation()))
        .collect();
    assert!(operations.contains(&(String::from("a.txt"), DeltaOperation::Delete)));
    assert!(operations.contains(&(String::from("b.txt"), DeltaOperation::Add)));
    for (path, operation) in &operations {
        // Rebuilt directories report Modify; the renamed files must not.
        if *operation == DeltaOperation::Modify {
            assert!(path.is_empty(), "only the root may be modified");
        }
    }
    assert_rebuilt(&repository, &storage, renamed.snapshot())?;
    Ok(())
}

#[test]
fn type_changes_replay_exactly() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    write_file(source.path(), "node", b"starts as a file")?;
    write_file(source.path(), "keep.txt", b"untouched")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let first = client.backup(repository.clone(), request(source.path(), 7_000)?)?;
    assert_rebuilt(&repository, &storage, first.snapshot())?;

    fs::remove_file(source.path().join("node"))?;
    write_file(source.path(), "node/child.txt", b"now a directory")?;
    let second = client.backup(repository.clone(), request(source.path(), 7_001)?)?;
    assert_rebuilt(&repository, &storage, second.snapshot())?;
    let state = repository.rebuild_path_state(second.snapshot())?;
    assert_eq!(
        state
            .entries()
            .iter()
            .find_map(|(path, entry)| { (path.as_str() == "node").then(|| entry.kind()) }),
        Some(TreeNodeKind::Directory)
    );

    fs::remove_dir_all(source.path().join("node"))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("keep.txt", source.path().join("node"))?;
    #[cfg(not(unix))]
    write_file(source.path(), "node", b"plain fallback")?;
    let third = client.backup(repository.clone(), request(source.path(), 7_002)?)?;
    assert_rebuilt(&repository, &storage, third.snapshot())?;
    Ok(())
}

#[test]
fn branch_parent_selects_its_delta_chain() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    write_file(source.path(), "a.txt", b"v1")?;
    write_file(source.path(), "b.txt", b"v1")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let first = client.backup(repository.clone(), request(source.path(), 8_000)?)?;
    let first_id = snapshot_id(first.snapshot())?;

    write_file(source.path(), "a.txt", b"v2-main")?;
    let _second = client.backup(repository.clone(), request(source.path(), 8_001)?)?;

    write_file(source.path(), "a.txt", b"v2-branch")?;
    let branch = client.backup(
        repository.clone(),
        request(source.path(), 8_002)?.with_parent(&first_id),
    )?;
    let delta = repository.read_path_delta(branch.snapshot())?;
    assert_eq!(delta.parent(), Some(&first_id));
    assert_rebuilt(&repository, &storage, branch.snapshot())?;
    let state = repository.rebuild_path_state(branch.snapshot())?;
    let entry = state
        .entries()
        .iter()
        .find(|(path, _)| path.as_str() == "a.txt")
        .map(|(_, entry)| entry)
        .ok_or("branched file must be live")?;
    assert_eq!(entry.size(), u64::try_from(b"v2-branch".len())?);
    assert_eq!(entry.last_seen(), &branch.snapshot().snapshot_id()?);
    Ok(())
}

#[test]
fn missing_delta_is_detected_and_regenerated_byte_identical() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    write_file(source.path(), "payload.txt", b"regenerable")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    client.backup(repository.clone(), request(source.path(), 9_000)?)?;
    write_file(source.path(), "payload.txt", b"regenerable v2")?;
    let second = client.backup(repository.clone(), request(source.path(), 9_001)?)?;

    let header = Snapshot::from_bytes(&storage.read_object(second.snapshot().as_str())?)?;
    let delta_key = header
        .path_delta()
        .ok_or("snapshot must reference its delta")?
        .as_str()
        .to_owned();
    let original = storage.read_object(&delta_key)?;
    assert!(storage.remove_object(&delta_key)?);

    let missing = repository
        .read_path_delta(second.snapshot())
        .expect_err("deleted deltas must be reported");
    assert_eq!(missing.code(), ErrorCode::PathDeltaNotFound);
    let rebuild_missing = repository
        .rebuild_path_state(second.snapshot())
        .expect_err("rebuild without its delta must fail");
    assert_eq!(rebuild_missing.code(), ErrorCode::PathDeltaNotFound);

    let regenerated = repository.regenerate_path_delta(second.snapshot(), 2)?;
    assert_eq!(storage.read_object(&delta_key)?, original);
    assert_eq!(regenerated.snapshot(), &snapshot_id(second.snapshot())?);
    assert_rebuilt(&repository, &storage, second.snapshot())?;
    Ok(())
}

#[test]
fn corrupt_checkpoint_falls_back_to_deltas_without_touching_restore() -> Result<(), Box<dyn Error>>
{
    let source = TestDirectory::new()?;
    write_file(source.path(), "payload.txt", b"v0")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let mut references = Vec::new();
    for generation in 1..=17_u64 {
        write_file(
            source.path(),
            "payload.txt",
            format!("v{generation}").as_bytes(),
        )?;
        let result = client.backup(
            repository.clone(),
            request(source.path(), 10_000 + generation)?,
        )?;
        references.push(result.snapshot().clone());
    }
    let sixteenth = snapshot_id(&references[15])?;
    let checkpoint_key = format!("checkpoints/{}", sixteenth.as_str());
    assert!(storage.read_object(&checkpoint_key).is_ok());
    for (index, reference) in references.iter().enumerate() {
        if index == 15 {
            continue;
        }
        let missing = repository
            .read_path_checkpoint(reference)
            .expect_err("only generation 16 has a checkpoint");
        assert_eq!(missing.code(), ErrorCode::PathDeltaNotFound);
    }

    let rebuilt = repository.rebuild_path_state(&references[16])?;
    assert_eq!(rebuilt.checkpoint(), Some(&sixteenth));
    assert_eq!(rebuilt.deltas_applied(), 1);
    let early = repository.rebuild_path_state(&references[14])?;
    assert_eq!(early.checkpoint(), None);
    assert_eq!(early.deltas_applied(), 15);
    assert_rebuilt(&repository, &storage, &references[16])?;

    let mut bytes = storage.read_object(&checkpoint_key)?;
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x80;
    storage.replace_object(&checkpoint_key, bytes)?;
    let corrupt = repository
        .read_path_checkpoint(&references[15])
        .expect_err("tampered checkpoints must fail");
    assert_eq!(corrupt.code(), ErrorCode::PathDeltaMalformed);
    let recovered = repository.rebuild_path_state(&references[16])?;
    assert_eq!(recovered.checkpoint(), None);
    assert_eq!(recovered.deltas_applied(), 17);
    assert_eq!(recovered.entries(), rebuilt.entries());
    assert_rebuilt(&repository, &storage, &references[16])?;
    Ok(())
}

#[test]
fn large_path_counts_round_trip_sorted() -> Result<(), Box<dyn Error>> {
    // Twenty directories of one hundred files keep debug runtimes sane:
    // journal checkpoints re-encode the completion set per object.
    let source = TestDirectory::new()?;
    for directory in 0..20_usize {
        for file in 0..100_usize {
            write_file(
                source.path(),
                &format!("dir-{directory:03}/file-{file:03}.bin"),
                format!("large-{directory}-{file}").as_bytes(),
            )?;
        }
    }
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let result = client.backup(repository.clone(), large_request(source.path(), 11_000)?)?;
    let delta = repository.read_path_delta(result.snapshot())?;
    assert!(delta.is_full());
    assert_eq!(delta.records().len(), 2_021);
    let mut paths: Vec<&str> = delta
        .records()
        .iter()
        .map(|record| record.path().as_str())
        .collect();
    let sorted = paths.clone();
    paths.sort_unstable();
    assert_eq!(paths, sorted);
    assert_rebuilt(&repository, &storage, result.snapshot())?;
    let header = Snapshot::from_bytes(&storage.read_object(result.snapshot().as_str())?)?;
    let delta_key = header
        .path_delta()
        .ok_or("snapshot must reference its delta")?
        .as_str()
        .to_owned();
    assert!(delta_key.starts_with("path-deltas/"));
    Ok(())
}
