use gib::{Client, LocalStorage, RepositoryIdentity, RepositoryKey, RepositoryOpenRequest};
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let path = argument("repository path")?;
    let storage = LocalStorage::new(&path)?;
    let mut request = RepositoryOpenRequest::new();

    if let Some(identity) = std::env::args().nth(2) {
        request = request.with_identity(RepositoryIdentity::new(identity)?);
    }
    if let Some(repository_key) = std::env::args().nth(3) {
        request = request.with_repository_key(RepositoryKey::new(repository_key)?);
    }

    let repository = Client::default().open_repository(storage, request)?;
    println!(
        "Opened repository identity={} key={} descriptor_version={} format_version={}",
        repository.identity(),
        repository.repository_key(),
        repository.descriptor_version(),
        repository.format_version()
    );
    Ok(())
}

fn argument(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}; pass it as the first argument").into())
}
