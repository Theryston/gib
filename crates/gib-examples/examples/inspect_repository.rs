use gib::{Client, LocalStorage, RepositoryOpenRequest};
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| "missing repository path; pass it as the first argument".to_string())?;
    let storage = LocalStorage::new(&path)?;
    let repository = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;

    println!("repository root: {}", path.display());
    println!("identity: {}", repository.identity());
    println!("repository key: {}", repository.repository_key());
    println!("format version: {}", repository.format_version());
    println!("descriptor version: {}", repository.descriptor_version());
    println!(
        "published snapshot: {}",
        repository.has_published_snapshot()
    );
    let head = repository.read_head()?;
    println!("HEAD generation: {}", head.generation());
    if let Some(snapshot) = head.snapshot() {
        println!("HEAD snapshot: {}", snapshot);
    }
    println!("root objects:");
    println!("  {}", repository.roots().format());
    println!("  {}", repository.roots().descriptor());
    Ok(())
}
