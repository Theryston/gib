use crate::application::ports::{
    StorageAddRequest, StorageAddResult, StorageConfigurationError,
    StorageConfigurationListRequest, StorageConfigurationMetadata, StorageConfigurationRepository,
    StorageConfigurationResult, StorageConnectivity, StorageListResult, StorageRemoveRequest,
    StorageRemoveResult,
};

pub(crate) fn add_storage<R, C>(
    repository: &R,
    connectivity: &C,
    request: StorageAddRequest,
) -> StorageConfigurationResult<StorageAddResult>
where
    R: StorageConfigurationRepository + ?Sized,
    C: StorageConnectivity + ?Sized,
{
    let (name, configuration, replace_existing) = request.into_parts();
    let exists = repository.contains(&name)?;
    if exists && !replace_existing {
        return Err(StorageConfigurationError::AlreadyExists);
    }

    let backend = configuration.backend().clone();
    let credentials_configured = configuration.credentials().is_some();
    let health = connectivity.check(&configuration)?;
    let replaced_existing = if exists {
        repository.save_replacement(&name, configuration)?
    } else {
        repository.save_new(&name, configuration)?;
        false
    };
    let metadata = StorageConfigurationMetadata::new(name, backend, credentials_configured, health);
    Ok(StorageAddResult::new(metadata, replaced_existing))
}

pub(crate) fn list_storages<R, C>(
    repository: &R,
    connectivity: &C,
    request: StorageConfigurationListRequest,
) -> StorageConfigurationResult<StorageListResult>
where
    R: StorageConfigurationRepository + ?Sized,
    C: StorageConnectivity + ?Sized,
{
    let metadata = repository.list_metadata()?;
    if !request.checks_health() {
        return Ok(StorageListResult::new(metadata));
    }

    let checked = metadata
        .into_iter()
        .map(|entry| {
            let configuration = repository.load(entry.name())?;
            let health = connectivity.check(&configuration)?;
            Ok::<StorageConfigurationMetadata, _>(entry.with_health(health))
        })
        .collect::<StorageConfigurationResult<Vec<_>>>()?;
    Ok(StorageListResult::new(checked))
}

pub(crate) fn remove_storage<R>(
    repository: &R,
    request: StorageRemoveRequest,
) -> StorageConfigurationResult<StorageRemoveResult>
where
    R: StorageConfigurationRepository + ?Sized,
{
    let name = request.into_name();
    let metadata = repository.describe(&name)?;
    repository.remove(&name)?;
    Ok(StorageRemoveResult::new(
        name,
        metadata.backend().kind(),
        metadata.credentials_configured(),
    ))
}
