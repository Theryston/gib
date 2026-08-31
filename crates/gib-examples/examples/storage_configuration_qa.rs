use gib::{
    MemoryCredentialStore, S3StorageSettings, StorageBackend, StorageConfiguration,
    StorageConfigurationError, StorageConfigurationOperation, StorageConfigurationStore,
    StorageCredentials,
};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const ACCESS_KEY: &str = "manual-recognizable-access-key";
const SECRET_KEY: &str = "manual-recognizable-secret-key";
const SESSION_TOKEN: &str = "manual-recognizable-session-token";

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let directory = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("missing configuration directory")?;
    let command = arguments.next().unwrap_or_else(|| String::from("all"));

    match command.as_str() {
        "all" => all(&directory),
        "add" => add(&directory),
        "inspect" => inspect(&directory),
        "fail-update" => fail_update(&directory),
        "remove" => remove(&directory),
        _ => Err(format!(
            "unknown command {command}; use all, add, inspect, fail-update, or remove"
        )
        .into()),
    }
}

fn all(directory: &Path) -> Result<(), Box<dyn Error>> {
    let credential_store = MemoryCredentialStore::new();
    let store = StorageConfigurationStore::new(directory, credential_store.clone())?;
    add_with_store(&store)?;
    inspect_with_store(&store)?;
    fail_update_with_store(&store)?;

    store.delete("manual-remote")?;
    if store.record_path("manual-remote")?.exists() || !credential_store.is_empty() {
        return Err("configuration or credential remained after removal".into());
    }
    println!("removed configuration and credential reference");
    Ok(())
}

fn add(directory: &Path) -> Result<(), Box<dyn Error>> {
    let store = StorageConfigurationStore::new(directory, MemoryCredentialStore::new())?;
    add_with_store(&store)
}

fn add_with_store(store: &StorageConfigurationStore) -> Result<(), Box<dyn Error>> {
    store.save("manual-remote", configuration("one")?)?;
    let path = store.record_path("manual-remote")?;
    println!("stored non-secret configuration at {}", path.display());
    println!("configuration bytes: {}", fs::metadata(path)?.len());
    Ok(())
}

fn inspect(directory: &Path) -> Result<(), Box<dyn Error>> {
    let store = StorageConfigurationStore::new(directory, MemoryCredentialStore::new())?;
    inspect_with_store(&store)
}

fn inspect_with_store(store: &StorageConfigurationStore) -> Result<(), Box<dyn Error>> {
    let path = store.record_path("manual-remote")?;
    let bytes = fs::read(&path)?;
    for secret in [ACCESS_KEY, SECRET_KEY, SESSION_TOKEN] {
        if bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
        {
            return Err("configuration file contains a credential value".into());
        }
    }
    println!(
        "secret scan passed for {} ({} bytes)",
        path.display(),
        bytes.len()
    );
    Ok(())
}

fn fail_update(directory: &Path) -> Result<(), Box<dyn Error>> {
    let store = StorageConfigurationStore::new(directory, MemoryCredentialStore::new())?;
    fail_update_with_store(&store)
}

fn fail_update_with_store(store: &StorageConfigurationStore) -> Result<(), Box<dyn Error>> {
    let path = store.record_path("manual-remote")?;
    let before = fs::read(&path)?;
    store.inject_failure(
        StorageConfigurationOperation::Rename,
        StorageConfigurationError::Io,
    );
    let result = store.save("manual-remote", configuration("two")?);
    if result != Err(StorageConfigurationError::Io) {
        return Err(format!("expected injected failure, got {result:?}").into());
    }
    let after = fs::read(path)?;
    if before != after {
        return Err("failed update changed the previous configuration".into());
    }
    println!("forced update failure preserved the previous file");
    Ok(())
}

fn remove(directory: &Path) -> Result<(), Box<dyn Error>> {
    let store = StorageConfigurationStore::new(directory, MemoryCredentialStore::new())?;
    match store.delete("manual-remote") {
        Ok(()) => {
            println!("removed configuration and credential reference");
            Ok(())
        }
        Err(StorageConfigurationError::NotFound) => {
            Err("configuration was not found; run the add command first".into())
        }
        Err(error) => Err(error.into()),
    }
}

fn configuration(label: &str) -> Result<StorageConfiguration, Box<dyn Error>> {
    let settings = S3StorageSettings::new("us-east-1", "gib-test-bucket")?;
    let credentials = StorageCredentials::s3_with_session_token(
        ACCESS_KEY,
        format!("{SECRET_KEY}-{label}"),
        Some(String::from(SESSION_TOKEN)),
    )?;
    Ok(StorageConfiguration::new(
        StorageBackend::S3(settings),
        Some(credentials),
    )?)
}
