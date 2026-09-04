use gib::{
    LocalStorage, Repository, RepositoryIdentity, RepositoryKey, RepositoryObject,
    RepositoryStorage, Snapshot, SnapshotId, SnapshotPublication,
};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn interactive_log_uses_aligned_requested_columns_and_formats_time() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new();
    let repository_path = directory.path().join("repository");
    let storage = LocalStorage::new(&repository_path)?;
    let repository = Repository::initialize(
        storage.clone(),
        RepositoryIdentity::new("history-command-test")?,
        RepositoryKey::new("default")?,
    )?;

    publish(
        &repository,
        &storage,
        "1234567890abcdef",
        "base snapshot",
        0,
        1_536,
        None,
    )?;
    publish(
        &repository,
        &storage,
        "fedcba0987654321",
        "new snapshot",
        60,
        2 * 1024 * 1024,
        Some("Jane Doe"),
    )?;

    let repository_text = repository_path.display().to_string();
    let interactive = run(directory.path(), &["log", "--repo", &repository_text]);
    assert!(interactive.status.success(), "{interactive:?}");
    assert!(interactive.stderr.is_empty(), "{interactive:?}");

    let stdout = String::from_utf8(interactive.stdout)?;
    let header = stdout
        .lines()
        .find(|line| line.contains("SNAPSHOT") && line.contains("MESSAGE"))
        .ok_or("history table header should be rendered")?;
    let row = stdout
        .lines()
        .find(|line| line.contains("fedcba0987654321") && line.contains("new snapshot"))
        .ok_or("history table row should be rendered")?;

    let headers = ["SNAPSHOT", "SIZE", "AUTHOR", "TIME", "MESSAGE"];
    let header_positions = headers
        .iter()
        .map(|value| visual_position(header, value))
        .collect::<Option<Vec<_>>>()
        .ok_or("history headers should be present")?;
    assert!(
        header_positions
            .windows(2)
            .all(|positions| positions[0] < positions[1])
    );

    for (header_value, row_value) in headers.iter().zip([
        "fedcba0987654321",
        "2.0 MiB",
        "Jane Doe",
        "Jan 01, 1970 00:01 UTC",
        "new snapshot",
    ]) {
        assert_eq!(
            visual_position(header, header_value),
            visual_position(row, row_value),
            "column {header_value} should align"
        );
    }
    assert!(stdout.contains("Jan 01, 1970 00:01 UTC"));
    assert!(!stdout.contains(" 60 "));

    let json = run(
        directory.path(),
        &["--mode", "json", "log", "--repo", &repository_text],
    );
    assert!(json.status.success(), "{json:?}");
    assert!(json.stderr.is_empty(), "{json:?}");
    let json_text = String::from_utf8(json.stdout)?;
    let json_value: serde_json::Value = serde_json::from_str(json_text.trim())?;
    assert_eq!(json_value["type"], "output");
    assert_eq!(json_value["data"]["summaries"][0]["timestamp"], 60);

    Ok(())
}

fn publish(
    repository: &Repository,
    storage: &LocalStorage,
    id: &str,
    message: &str,
    timestamp: u64,
    size: u64,
    author: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let snapshot = Snapshot::new(SnapshotId::new(id)?, message, timestamp)?;
    let snapshot = match author {
        Some(author) => snapshot.with_author(author.to_owned())?,
        None => snapshot,
    }
    .with_root_tree(RepositoryObject::new(format!("trees/{id}"))?)
    .with_path_delta(RepositoryObject::new(format!("path-deltas/{id}"))?)
    .with_statistics(1, 1, size);
    let reference = snapshot.reference()?;
    storage.create_if_absent(reference.as_str(), &snapshot.to_bytes()?)?;
    repository.publish_snapshot(
        &repository.read_head()?,
        SnapshotPublication::from_snapshot(snapshot)?,
    )?;
    Ok(())
}

fn visual_position(line: &str, value: &str) -> Option<usize> {
    let start = line.find(value)?;
    Some(dialoguer::console::measure_text_width(&line[..start]))
}

fn run(home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gib"))
        .args(arguments)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("gib should run")
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gib-cli-history-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
