//! Manual QA for immutable path deltas and checkpoints.
//!
//! ```text
//! cargo run -p gib-examples --example path_delta_qa -- inspect /tmp/gib-repository [snapshot-ref]
//! cargo run -p gib-examples --example path_delta_qa -- regenerate /tmp/gib-repository <snapshot-ref> <generation>
//! ```
//!
//! `inspect` prints the delta of one snapshot, replays stored deltas into an
//! empty map, and compares the result against a direct tree traversal.
//! `regenerate` rebuilds a missing delta from the authoritative trees and
//! republishes it under its deterministic key. Neither command restores user
//! data; traversal of the Merkle trees is the restore data path and keeps
//! working while derived objects are missing.

use gib::{Client, LocalStorage, Repository, RepositoryOpenRequest, SnapshotReference, TreeNode};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments
        .next()
        .ok_or("missing command: inspect|regenerate")?;
    match command.as_str() {
        "inspect" => {
            let repository_path = PathBuf::from(arguments.next().ok_or("missing repository")?);
            let reference = arguments.next().unwrap_or_else(|| String::from("latest"));
            inspect(&repository_path, &reference)
        }
        "regenerate" => {
            let repository_path = PathBuf::from(arguments.next().ok_or("missing repository")?);
            let reference = arguments.next().ok_or("missing snapshot reference")?;
            let generation: u64 = arguments
                .next()
                .ok_or("missing snapshot generation")?
                .parse()
                .map_err(|_| "generation must be a positive integer")?;
            regenerate(&repository_path, &reference, generation)
        }
        other => Err(format!("unknown command {other}").into()),
    }
}

fn open_repository(path: &PathBuf) -> Result<Repository, Box<dyn Error>> {
    let storage = LocalStorage::new(path)?;
    Ok(Client::default().open_repository(storage, RepositoryOpenRequest::new())?)
}

fn resolve(repository: &Repository, reference: &str) -> Result<SnapshotReference, Box<dyn Error>> {
    Ok(repository.resolve_snapshot_reference(reference)?)
}

fn traverse(
    repository: &Repository,
    snapshot: &SnapshotReference,
) -> Result<BTreeMap<String, (String, u64)>, Box<dyn Error>> {
    let storage = repository.storage();
    let bytes = storage.as_storage().read(snapshot.as_str())?;
    let header = gib::decode_snapshot_object(&bytes)?;
    let root = header
        .root_tree()
        .ok_or("snapshot has no root tree")?
        .as_str()
        .to_owned();
    let mut entries = BTreeMap::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((path, key)) = stack.pop() {
        let node = gib::decode_tree_node_object(&storage.as_storage().read(&key)?)?;
        match node {
            TreeNode::Directory(directory) => {
                entries.insert(path.clone(), (String::from("directory"), 0));
                for entry in directory.entries() {
                    let child = if path.is_empty() {
                        entry.name().as_str().to_owned()
                    } else {
                        format!("{}/{}", path, entry.name().as_str())
                    };
                    stack.push((child, format!("trees/{}", entry.reference().id().as_str())));
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

fn inspect(repository_path: &PathBuf, reference: &str) -> Result<(), Box<dyn Error>> {
    let repository = open_repository(repository_path)?;
    let snapshot = resolve(&repository, reference)?;
    println!("snapshot: {}", snapshot.as_str());
    let traversed = traverse(&repository, &snapshot)?;
    println!("traversed paths: {}", traversed.len());
    match repository.read_path_delta(&snapshot) {
        Ok(delta) => {
            println!(
                "delta: snapshot={} parent={} generation={} records={} full={}",
                delta.snapshot().as_str(),
                delta
                    .parent()
                    .map_or_else(|| String::from("-"), |parent| parent.as_str().to_owned()),
                delta.generation(),
                delta.records().len(),
                delta.is_full(),
            );
            for record in delta.records().iter().take(20) {
                println!(
                    "  {} {} {} {}",
                    record.operation(),
                    record.kind(),
                    record.path().as_str(),
                    record.size(),
                );
            }
            if delta.records().len() > 20 {
                println!("  ... ({} more)", delta.records().len() - 20);
            }
        }
        Err(error) => println!("delta unavailable: {error} ({})", error.code().as_str()),
    }
    match repository.read_path_checkpoint(&snapshot) {
        Ok(checkpoint) => println!(
            "checkpoint: snapshot={} generation={} entries={}",
            checkpoint.snapshot().as_str(),
            checkpoint.generation(),
            checkpoint.entries().len(),
        ),
        Err(error) => println!("checkpoint: {error} ({})", error.code().as_str()),
    }
    match repository.rebuild_path_state(&snapshot) {
        Ok(rebuilt) => {
            let replayed: BTreeMap<String, (String, u64)> = rebuilt
                .entries()
                .iter()
                .map(|(path, entry)| {
                    (
                        path.as_str().to_owned(),
                        (entry.kind().as_str().to_owned(), entry.size()),
                    )
                })
                .collect();
            println!(
                "rebuilt paths: {} checkpoint={} deltas_applied={}",
                replayed.len(),
                rebuilt
                    .checkpoint()
                    .map_or_else(|| String::from("-"), |id| id.as_str().to_owned()),
                rebuilt.deltas_applied(),
            );
            if replayed == traversed {
                println!("MATCH: replay equals tree traversal");
                Ok(())
            } else {
                println!(
                    "MISMATCH: replay has {} paths, traversal has {}",
                    replayed.len(),
                    traversed.len()
                );
                Err("delta replay does not match tree traversal".into())
            }
        }
        Err(error) => {
            println!("rebuild failed: {error} ({})", error.code().as_str());
            Err(error.into())
        }
    }
}

fn regenerate(
    repository_path: &PathBuf,
    reference: &str,
    generation: u64,
) -> Result<(), Box<dyn Error>> {
    let repository = open_repository(repository_path)?;
    let snapshot = resolve(&repository, reference)?;
    let delta = repository.regenerate_path_delta(&snapshot, generation)?;
    println!(
        "regenerated delta for {} with {} records",
        delta.snapshot().as_str(),
        delta.records().len(),
    );
    Ok(())
}
