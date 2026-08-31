use gib::{
    LocalStorage, MemoryStorage, Repository, RepositoryIdentity, RepositoryKey, RepositoryStorage,
    SdkError, Snapshot, SnapshotId, SnapshotListRequest, SnapshotPublication, SnapshotSummary,
    StorageResult, StorageVersion, VersionedObject,
};
use std::collections::HashSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn initialize<S>(storage: S) -> Result<Repository, Box<dyn Error>>
where
    S: Into<gib::StorageHandle>,
{
    Ok(Repository::initialize(
        storage,
        RepositoryIdentity::new("snapshot-history-test")?,
        RepositoryKey::new("default")?,
    )?)
}

fn publish(
    repository: &Repository,
    storage: &dyn RepositoryStorage,
    id: &str,
    message: &str,
    timestamp: u64,
    size: u64,
) -> Result<(), Box<dyn Error>> {
    let snapshot = Snapshot::new(SnapshotId::new(id)?, message, timestamp)?
        .with_root_tree(gib::RepositoryObject::new(format!("trees/{id}"))?)
        .with_path_delta(gib::RepositoryObject::new(format!("path-deltas/{id}"))?)
        .with_statistics(3, 2, size);
    let reference = snapshot.reference()?;
    let bytes = snapshot.to_bytes()?;
    storage.create_if_absent(reference.as_str(), &bytes)?;
    let publication = SnapshotPublication::from_snapshot(snapshot)?;
    repository.publish_snapshot(&repository.read_head()?, publication)?;
    Ok(())
}

#[test]
fn resolves_full_unique_ambiguous_missing_malformed_and_latest_references()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize(storage.clone())?;

    assert_eq!(
        repository.resolve_snapshot_reference("latest"),
        Err(SdkError::RepositoryNoSnapshots)
    );

    publish(&repository, &storage, "abc-one", "first", 10, 11)?;
    publish(&repository, &storage, "abc-two", "second", 20, 22)?;
    publish(&repository, &storage, "def-three", "third", 30, 33)?;

    assert_eq!(
        repository.resolve_snapshot_reference("abc-one")?.as_str(),
        "snapshots/abc-one"
    );
    assert_eq!(
        repository.resolve_snapshot_reference("abc-on")?.as_str(),
        "snapshots/abc-one"
    );
    assert_eq!(
        repository.resolve_snapshot_reference("latest")?.as_str(),
        "snapshots/def-three"
    );
    assert_eq!(
        repository.resolve_snapshot_reference("def")?.as_str(),
        "snapshots/def-three"
    );
    assert_eq!(
        repository.resolve_snapshot_reference("missing"),
        Err(SdkError::SnapshotReferenceNotFound)
    );
    assert_eq!(
        repository.resolve_snapshot_reference("abc"),
        Err(SdkError::SnapshotReferenceAmbiguous)
    );
    assert_eq!(
        repository.resolve_snapshot_reference(""),
        Err(SdkError::SnapshotReferenceEmpty)
    );
    assert_eq!(
        repository.resolve_snapshot_reference("../abc"),
        Err(SdkError::SnapshotReferenceMalformed)
    );
    assert_eq!(
        repository.resolve_snapshot_reference("snapshots/abc-one"),
        Err(SdkError::SnapshotReferenceMalformed)
    );
    Ok(())
}

#[test]
fn lists_compact_summaries_newest_first_with_stable_timestamp_ties() -> Result<(), Box<dyn Error>> {
    let storage = CountingStorage::new();
    let repository = initialize(storage.clone())?;
    publish(&repository, &storage, "tie-first", "first", 42, 10)?;
    publish(&repository, &storage, "tie-second", "second", 42, 20)?;
    publish(&repository, &storage, "tie-third", "third", 42, 30)?;
    storage.clear_reads();

    let first_page =
        repository.list_snapshot_summaries(SnapshotListRequest::new().with_limit(2))?;
    assert_eq!(
        first_page
            .summaries()
            .iter()
            .map(|summary| summary.id().as_str())
            .collect::<Vec<_>>(),
        vec!["tie-third", "tie-second"]
    );
    assert_eq!(first_page.summaries()[0].message(), "third");
    assert_eq!(first_page.summaries()[0].timestamp(), Some(42));
    assert_eq!(first_page.summaries()[0].size(), Some(30));
    assert_eq!(first_page.summaries()[0].file_count(), Some(3));
    assert_eq!(first_page.summaries()[0].directory_count(), Some(2));
    assert_eq!(first_page.summaries()[0].total_size(), Some(30));
    assert_eq!(
        first_page.summaries()[0]
            .root_tree()
            .map(gib::RepositoryObject::as_str),
        Some("trees/tie-third")
    );

    let cursor = first_page
        .next_cursor()
        .cloned()
        .ok_or_else(|| std::io::Error::other("first page should have a continuation"))?;
    let second_page = repository
        .list_snapshot_summaries(SnapshotListRequest::new().with_limit(2).after(cursor))?;
    assert_eq!(
        second_page
            .summaries()
            .iter()
            .map(|summary| summary.id().as_str())
            .collect::<Vec<_>>(),
        vec!["tie-first"]
    );
    assert!(!second_page.has_more());

    let reads = storage.reads();
    assert!(
        reads
            .iter()
            .all(|key| { key == "refs/latest" || key.starts_with("refs/history/") })
    );
    Ok(())
}

#[test]
fn paginates_a_large_synthetic_history_without_duplicates() -> Result<(), Box<dyn Error>> {
    const SNAPSHOT_COUNT: usize = 257;
    const PAGE_SIZE: usize = 19;
    let storage = MemoryStorage::new();
    let repository = initialize(storage.clone())?;
    for index in 0..SNAPSHOT_COUNT {
        let id = format!("snapshot-{index:04}");
        let message = format!("message-{index:04}");
        publish(
            &repository,
            &storage,
            &id,
            &message,
            index as u64,
            index as u64 + 1,
        )?;
    }

    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let request = SnapshotListRequest::new().with_limit(PAGE_SIZE);
        let request = match cursor.take() {
            Some(cursor) => request.after(cursor),
            None => request,
        };
        let page = repository.list_snapshot_summaries(request)?;
        ids.extend(page.summaries().iter().map(|summary| summary.id().clone()));
        cursor = page.next_cursor().cloned();
        if cursor.is_none() {
            break;
        }
    }

    let unique_ids = ids.iter().cloned().collect::<HashSet<_>>();
    assert_eq!(ids.len(), SNAPSHOT_COUNT);
    assert_eq!(unique_ids.len(), SNAPSHOT_COUNT);
    assert_eq!(ids.first().map(SnapshotId::as_str), Some("snapshot-0256"));
    assert_eq!(ids.last().map(SnapshotId::as_str), Some("snapshot-0000"));
    Ok(())
}

#[test]
fn rebuilds_summary_fields_from_authoritative_snapshot_objects() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize(storage.clone())?;
    publish(&repository, &storage, "rebuild-first", "first", 100, 101)?;
    publish(&repository, &storage, "rebuild-second", "second", 200, 202)?;

    let history_keys = storage
        .objects()?
        .into_iter()
        .filter(|key| key.starts_with("refs/history/"))
        .collect::<Vec<_>>();
    for key in history_keys {
        assert!(storage.remove_object(&key)?);
    }

    let summaries = repository.rebuild_snapshot_summaries()?;
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id().as_str(), "rebuild-second");
    assert_eq!(summaries[0].message(), "second");
    assert_eq!(summaries[0].timestamp(), Some(200));
    assert_eq!(summaries[0].size(), Some(202));
    assert_eq!(summaries[1].id().as_str(), "rebuild-first");

    let listed = repository.list_history(())?;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.summaries()[0].id().as_str(), "rebuild-second");
    Ok(())
}

#[test]
fn local_storage_persists_and_lists_history_records() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new();
    let storage = LocalStorage::new(directory.path())?;
    let repository = initialize(storage.clone())?;
    publish(
        &repository,
        &storage,
        "local-history",
        "local message",
        12,
        34,
    )?;

    let page = repository.list_history(())?;
    assert_eq!(page.len(), 1);
    assert_eq!(page.summaries()[0].message(), "local message");
    assert!(
        directory
            .path()
            .join("refs/history/00000000000000000001")
            .is_file()
    );
    Ok(())
}

#[test]
fn snapshot_objects_round_trip_all_summary_fields_without_tree_contents()
-> Result<(), Box<dyn Error>> {
    let parent = SnapshotId::new("parent")?;
    let snapshot = Snapshot::new(SnapshotId::new("round-trip")?, "message", 77)?
        .with_parent(Some(parent.clone()))
        .with_author("author")?
        .with_root_tree(gib::RepositoryObject::new("trees/root")?)
        .with_path_delta(gib::RepositoryObject::new("path-deltas/change")?)
        .with_statistics(8, 4, 1234);
    let decoded = Snapshot::from_bytes(&snapshot.to_bytes()?)?;
    assert_eq!(decoded, snapshot);
    assert_eq!(decoded.parent(), Some(&parent));
    assert_eq!(decoded.author(), Some("author"));
    assert_eq!(decoded.total_size(), 1234);
    let summary = SnapshotSummary::new("summary-id", "summary", Some(88), Some(99))?;
    assert_eq!(summary.id().as_str(), "summary-id");
    assert_eq!(summary.reference().as_str(), "snapshots/summary-id");
    Ok(())
}

#[test]
fn history_prefers_authoritative_snapshot_metadata_over_supplied_summary()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize(storage.clone())?;
    let snapshot = Snapshot::new(SnapshotId::new("authoritative")?, "authoritative", 7)?
        .with_statistics(1, 1, 10);
    let reference = snapshot.reference()?;
    storage.create_if_absent(reference.as_str(), &snapshot.to_bytes()?)?;
    let supplied = SnapshotSummary::new(reference.clone(), "stale", Some(1), Some(1))?;
    let publication = SnapshotPublication::with_summary(reference, supplied)?;
    repository.publish_snapshot(&repository.read_head()?, publication)?;

    let page = repository.list_history(())?;
    assert_eq!(page.len(), 1);
    assert_eq!(page.summaries()[0].message(), "authoritative");
    assert_eq!(page.summaries()[0].timestamp(), Some(7));
    assert_eq!(page.summaries()[0].size(), Some(10));
    Ok(())
}

#[test]
fn repeated_publication_of_one_snapshot_does_not_make_its_id_ambiguous()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize(storage.clone())?;
    let snapshot = Snapshot::new(SnapshotId::new("published-twice")?, "message", 7)?;
    let reference = snapshot.reference()?;
    storage.create_if_absent(reference.as_str(), &snapshot.to_bytes()?)?;

    let first = SnapshotPublication::from_snapshot(snapshot.clone())?;
    repository.publish_snapshot(&repository.read_head()?, first)?;
    let second = SnapshotPublication::from_snapshot(snapshot)?;
    repository.publish_snapshot(&repository.read_head()?, second)?;

    assert_eq!(
        repository.resolve_snapshot_reference("published-twice")?,
        reference
    );
    assert_eq!(
        repository.resolve_snapshot_reference("published")?,
        reference
    );
    Ok(())
}

#[derive(Clone)]
struct CountingStorage {
    inner: MemoryStorage,
    reads: Arc<Mutex<Vec<String>>>,
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("gib-snapshot-history-{}-{id}", std::process::id()));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl CountingStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            reads: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn clear_reads(&self) {
        if let Ok(mut reads) = self.reads.lock() {
            reads.clear();
        }
    }

    fn reads(&self) -> Vec<String> {
        self.reads
            .lock()
            .map_or_else(|_| Vec::new(), |reads| reads.clone())
    }
}

impl RepositoryStorage for CountingStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        self.inner.create_if_absent(object_key, contents)
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        if let Ok(mut reads) = self.reads.lock() {
            reads.push(object_key.to_owned());
        }
        self.inner.read(object_key)
    }

    fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.inner.list_objects(prefix)
    }

    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        self.inner.read_with_version(object_key)
    }

    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        self.inner.compare_and_swap(object_key, expected, contents)
    }
}
