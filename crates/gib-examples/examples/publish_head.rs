use gib::{
    Client, LocalStorage, RepositoryIdentity, RepositoryInitRequest, RepositoryKey,
    RepositoryOpenRequest, RepositoryStorage, SnapshotReference,
};
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let path = argument("repository path")?;
    let storage = LocalStorage::new(&path)?;
    let repository = Client::default().initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("head-example-repository")?,
            RepositoryKey::new("default")?,
        ),
    )?;

    println!("Snapshot construction is out of scope; using raw placeholder objects.");
    let first = SnapshotReference::new("snapshots/example-first")?;
    let second = SnapshotReference::new("snapshots/example-second")?;
    storage.create_if_absent(first.as_str(), b"minimal snapshot placeholder one")?;
    storage.create_if_absent(second.as_str(), b"minimal snapshot placeholder two")?;

    let empty_head = repository.read_head()?;
    let first_head = repository.publish_snapshot(&empty_head, first)?;
    println!(
        "Published HEAD: generation={} snapshot={:?}",
        first_head.generation(),
        first_head.snapshot()
    );

    let second_head = repository.publish_snapshot(&first_head, second)?;
    println!(
        "Published HEAD: generation={} snapshot={:?}",
        second_head.generation(),
        second_head.snapshot()
    );

    let reopened = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;
    let persisted_head = reopened.read_head()?;
    println!(
        "Reopened HEAD: generation={} snapshot={:?}",
        persisted_head.generation(),
        persisted_head.snapshot()
    );
    Ok(())
}

fn argument(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}; pass it as the first argument").into())
}
