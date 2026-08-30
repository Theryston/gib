use super::{
    AddStorageRequest, BackupRequest, DeleteBackupRequest, Gib, GibError, ListBackupsRequest,
    ListPendingBackupsRequest, LiveRequest, PruneRequest, RepositoryRequest, RestoreRequest,
    SearchRequest,
};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_directory(label: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("gib-api-{label}-{timestamp}"));
    fs::create_dir_all(&path).expect("temporary directory should be created");
    path
}

fn client(data_dir: &PathBuf, working_dir: &PathBuf) -> Gib {
    Gib::builder()
        .data_dir(data_dir)
        .working_dir(working_dir)
        .discover_config(false)
        .build()
        .expect("test client should build")
}

#[test]
fn public_credentials_are_redacted_from_debug_json_and_errors() {
    let repository = RepositoryRequest::new("project", "cloud").with_password("top-secret");
    let debug = format!("{repository:?}");
    let json = serde_json::to_string(&repository).expect("repository should serialize");
    assert!(!debug.contains("top-secret"));
    assert!(!json.contains("top-secret"));

    let error = GibError::new(
        super::ErrorCode::Internal,
        "provider returned password=top-secret, token='abc'",
    )
    .with_context("secret_key", "hidden")
    .with_context("path", "/tmp/project");
    assert!(!error.message().contains("top-secret"));
    assert!(!error.message().contains("abc"));
    assert!(!format!("{error:?}").contains("hidden"));
    assert_eq!(
        error.context().get("path").map(String::as_str),
        Some("/tmp/project")
    );
    assert!(
        !serde_json::to_string(&error)
            .expect("error should serialize")
            .contains("hidden")
    );
}

#[tokio::test]
async fn public_local_api_round_trip_preserves_cli_capabilities() {
    let root = temporary_directory("round-trip");
    let data_dir = root.join("data");
    let source = root.join("source");
    let storage = root.join("storage");
    let target = root.join("restore");
    fs::create_dir_all(&source).expect("source directory should be created");
    fs::write(source.join("notes.txt"), b"library API\n").expect("source file should be written");
    fs::write(source.join("ignored.tmp"), b"ignored")
        .expect("second source file should be written");

    let gib = client(&data_dir, &root);
    gib.add_storage(AddStorageRequest::local("local", &storage))
        .await
        .expect("local storage should be added");
    let repository = RepositoryRequest::new("project", "local");
    let mut backup_request = BackupRequest::new(
        repository.clone(),
        &source,
        "Library round trip",
        "Test User <test@example.com>",
    );
    backup_request.ignore_patterns = vec!["ignored.tmp".to_string()];
    let backup = gib
        .backup(backup_request)
        .await
        .expect("backup should succeed");
    assert_eq!(backup.files_total, 1);
    assert!(backup.head_published);

    let listed = gib
        .list_backups(ListBackupsRequest {
            repository: repository.clone(),
        })
        .await
        .expect("backup list should succeed");
    assert_eq!(listed.backups.len(), 1);
    assert_eq!(listed.backups[0].hash, backup.backup.hash);

    let pending = gib
        .list_pending_backups(ListPendingBackupsRequest {
            repository: repository.clone(),
        })
        .await
        .expect("pending list should succeed");
    assert!(pending.pending.is_empty());

    let search = gib
        .search(SearchRequest::new(repository.clone(), "notes").expect("search request is valid"))
        .await
        .expect("search should succeed");
    assert_eq!(search.results.len(), 1);
    assert_eq!(search.results[0].path, "notes.txt");

    let restored = gib
        .restore(RestoreRequest {
            repository: repository.clone(),
            backup: "latest".to_string(),
            target_path: target.clone(),
            only: vec!["notes.txt".to_string()],
            prune_local: false,
        })
        .await
        .expect("restore should succeed");
    assert_eq!(restored.restored, 1);
    assert_eq!(
        fs::read(target.join("notes.txt")).expect("restored file should exist"),
        b"library API\n"
    );

    let prune_plan = gib
        .plan_prune(PruneRequest {
            repository: repository.clone(),
        })
        .await
        .expect("prune plan should succeed");
    assert!(
        prune_plan
            .items
            .iter()
            .all(|item| item.kind == "pending_backup" || item.kind == "chunk")
    );

    let deleted = gib
        .delete_backup(DeleteBackupRequest {
            repository: repository.clone(),
            backup: backup.backup.hash,
        })
        .await
        .expect("backup should be deleted");
    assert_eq!(deleted.remaining_backups, 0);
    assert!(
        gib.list_backups(ListBackupsRequest { repository })
            .await
            .expect("backup list should still succeed")
            .backups
            .is_empty()
    );

    fs::remove_dir_all(root).expect("temporary directory should be removed");
}

#[tokio::test]
async fn live_handle_stops_without_process_global_signal_handling() {
    let root = temporary_directory("live-stop");
    let data_dir = root.join("data");
    let source = root.join("source");
    let storage = root.join("storage");
    fs::create_dir_all(&source).expect("live source directory should be created");
    fs::write(source.join("notes.txt"), b"live API\n").expect("live source file should be written");

    let gib = client(&data_dir, &root);
    gib.add_storage(AddStorageRequest::local("local", &storage))
        .await
        .expect("live storage should be added");
    let request = LiveRequest::new(RepositoryRequest::new("live-project", "local"), &source);
    let handle = gib
        .start_live(request)
        .await
        .expect("live operation should start");
    handle.stop().await.expect("live stop should be accepted");
    let result = handle.wait().await.expect("live operation should finish");
    assert!(result.stopped);
    assert!(result.backups_created <= 1);

    fs::remove_dir_all(root).expect("temporary live directory should be removed");
}
