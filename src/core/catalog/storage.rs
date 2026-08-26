use crate::core::crypto::encode_file_bytes;
use crate::core::metadata::{Backup, BackupObject};
use crate::fs::FS;
use crate::utils::{compress_bytes, decrypt_bytes, is_encrypted};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

use super::model::{
    Catalog, CatalogState, ChildrenShard, EntryShard, PendingCatalogBackup, TokenShard,
};

pub(crate) const CATALOG_ROOT: &str = "indexes/catalog/v1";
const MAX_CAS_RETRIES: usize = 5;
const MAX_PENDING_BACKUPS: usize = 128;

#[derive(Debug)]
pub(crate) struct VersionedObject<T> {
    pub(crate) value: T,
    pub(crate) version: String,
}

pub(crate) fn catalog_path(key: &str) -> String {
    format!("{}/{}/catalog", key, CATALOG_ROOT)
}

pub(crate) fn entry_shard_path(key: &str, shard: &str) -> String {
    format!("{}/{}/entries/{}", key, CATALOG_ROOT, shard)
}

pub(crate) fn children_shard_path(key: &str, shard: &str) -> String {
    format!("{}/{}/children/{}", key, CATALOG_ROOT, shard)
}

pub(crate) fn token_shard_path(key: &str, shard: &str) -> String {
    format!("{}/{}/tokens/{}", key, CATALOG_ROOT, shard)
}

pub(crate) async fn read_object<T>(
    fs: &Arc<dyn FS>,
    path: &str,
    password: Option<&str>,
    object_name: &str,
) -> Result<Option<VersionedObject<T>>, String>
where
    T: DeserializeOwned,
{
    let (raw_bytes, version) = match fs.read_file_with_version(path).await {
        Ok(result) => result,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to read {} '{}': {}",
                object_name, path, error
            ));
        }
    };

    if raw_bytes.is_empty() {
        return Err(format!("{} '{}' is empty", object_name, path));
    }

    let bytes = if is_encrypted(&raw_bytes) {
        let password = password.ok_or_else(|| {
            format!(
                "{} '{}' is encrypted but no password was provided",
                object_name, path
            )
        })?;
        decrypt_bytes(&raw_bytes, password.as_bytes())
            .map_err(|error| format!("Failed to decrypt {} '{}': {}", object_name, path, error))?
    } else {
        raw_bytes
    };

    let decompressed = zstd::decode_all(bytes.as_slice())
        .map_err(|error| format!("Failed to decompress {} '{}': {}", object_name, path, error))?;
    let value = rmp_serde::from_slice(&decompressed).map_err(|error| {
        format!(
            "Failed to deserialize {} '{}': {}",
            object_name, path, error
        )
    })?;

    Ok(Some(VersionedObject { value, version }))
}

pub(crate) async fn update_object<T, F>(
    fs: &Arc<dyn FS>,
    path: &str,
    password: Option<&str>,
    compress: i32,
    default: T,
    object_name: &str,
    mut update: F,
) -> Result<T, String>
where
    T: Serialize + DeserializeOwned + Clone,
    F: FnMut(&mut T) -> Result<(), String>,
{
    for _attempt in 0..MAX_CAS_RETRIES {
        let current = read_object(fs, path, password, object_name).await?;
        let expected_version = current.as_ref().map(|object| object.version.clone());
        let mut value = current
            .map(|object| object.value)
            .unwrap_or_else(|| default.clone());

        update(&mut value)?;

        let serialized = rmp_serde::to_vec_named(&value).map_err(|error| {
            format!("Failed to serialize {} '{}': {}", object_name, path, error)
        })?;
        let compressed = compress_bytes(&serialized, compress);
        let encoded = encode_file_bytes(&compressed, password)?;

        match fs
            .write_file_if_version(path, &encoded, expected_version.as_deref())
            .await
        {
            Ok(()) => return Ok(value),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Failed to write {} '{}': {}",
                    object_name, path, error
                ));
            }
        }
    }

    Err(format!(
        "Failed to update {} '{}' after several concurrent changes",
        object_name, path
    ))
}

pub(crate) async fn read_catalog(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
) -> Result<Option<VersionedObject<Catalog>>, String> {
    let catalog = read_object::<Catalog>(fs, &catalog_path(key), password, "catalog").await?;
    if let Some(catalog) = &catalog
        && catalog.value.schema_version != super::model::CATALOG_SCHEMA_VERSION
    {
        return Err(format!(
            "Unsupported historical catalog schema version: {}",
            catalog.value.schema_version
        ));
    }
    Ok(catalog)
}

pub(crate) async fn mark_catalog_degraded(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    pending_backup: PendingCatalogBackup,
) -> Result<(), String> {
    let path = catalog_path(key);
    update_object(
        fs,
        &path,
        password,
        compress,
        Catalog::default(),
        "catalog",
        |catalog| {
            catalog.schema_version = super::model::CATALOG_SCHEMA_VERSION;
            catalog.state = CatalogState::Degraded;
            if !catalog
                .pending_backups
                .iter()
                .any(|pending| pending.backup_hash == pending_backup.backup_hash)
            {
                catalog.pending_backups.push(pending_backup.clone());
            }
            if catalog.pending_backups.len() > MAX_PENDING_BACKUPS {
                let remove_count = catalog.pending_backups.len() - MAX_PENDING_BACKUPS;
                catalog.pending_backups.drain(..remove_count);
            }
            Ok(())
        },
    )
    .await
    .map(|_| ())
}

pub(crate) async fn mark_catalog_degraded_state(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
) -> Result<(), String> {
    let path = catalog_path(key);
    update_object(
        fs,
        &path,
        password,
        compress,
        Catalog::default(),
        "catalog",
        |catalog| {
            catalog.schema_version = super::model::CATALOG_SCHEMA_VERSION;
            catalog.state = CatalogState::Degraded;
            Ok(())
        },
    )
    .await
    .map(|_| ())
}

pub(crate) async fn load_backup_manifest(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    backup_hash: &str,
) -> Result<Backup, String> {
    let path = format!("{}/backups/{}", key, backup_hash);
    let (raw_bytes, _) = match fs.read_file_with_version(&path).await {
        Ok(result) => result,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("Backup {} not found", backup_hash));
        }
        Err(error) => {
            return Err(format!("Failed to read backup '{}': {}", path, error));
        }
    };

    if raw_bytes.is_empty() {
        return Err(format!("Backup {} is empty", backup_hash));
    }

    let bytes = if is_encrypted(&raw_bytes) {
        let password =
            password.ok_or_else(|| "Backup is encrypted but no password provided".to_string())?;
        decrypt_bytes(&raw_bytes, password.as_bytes())?
    } else {
        raw_bytes
    };
    let decompressed = zstd::decode_all(bytes.as_slice())
        .map_err(|error| format!("Failed to decompress backup {}: {}", backup_hash, error))?;
    let backup: Backup = rmp_serde::from_slice(&decompressed)
        .map_err(|error| format!("Failed to deserialize backup {}: {}", backup_hash, error))?;

    if backup.hash != backup_hash {
        return Err(format!(
            "Backup manifest hash mismatch: expected {}, found {}",
            backup_hash, backup.hash
        ));
    }

    Ok(backup)
}

pub(crate) fn backup_object_is_restorable(
    object: &BackupObject,
    chunk_indexes: &std::collections::HashMap<String, crate::core::metadata::ChunkIndex>,
) -> bool {
    object
        .chunks
        .iter()
        .all(|chunk_hash| chunk_indexes.contains_key(chunk_hash))
}

pub(crate) fn empty_entry_shard() -> EntryShard {
    EntryShard::default()
}

pub(crate) fn empty_children_shard() -> ChildrenShard {
    ChildrenShard::default()
}

pub(crate) fn empty_token_shard() -> TokenShard {
    TokenShard::default()
}
