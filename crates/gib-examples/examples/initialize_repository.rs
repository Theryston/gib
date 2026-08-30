use gib::{Client, LocalStorage, RepositoryIdentity, RepositoryInitRequest, RepositoryKey};
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let path = argument("repository path")?;
    let identity = RepositoryIdentity::new(
        std::env::args()
            .nth(2)
            .unwrap_or_else(|| String::from("manual-example-repository")),
    )?;
    let repository_key = RepositoryKey::new(
        std::env::args()
            .nth(3)
            .unwrap_or_else(|| String::from("default")),
    )?;
    let storage = LocalStorage::new(&path)?;
    let request = RepositoryInitRequest::new(identity, repository_key);
    let repository = Client::default().initialize_repository(storage, request)?;

    println!(
        "Initialized repository identity={} key={} format_version={} at {}",
        repository.identity(),
        repository.repository_key(),
        repository.format_version(),
        path.display()
    );
    Ok(())
}

fn argument(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}; pass it as the first argument").into())
}
