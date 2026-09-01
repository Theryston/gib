use crate::application::ports::{
    StorageBackend, StorageConfiguration, StorageConfigurationError, StorageConfigurationResult,
    StorageConnectivity, StorageError, StorageHealth,
};

/// The SDK's read-only connectivity checker for configured storage backends.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultStorageConnectivity;

impl StorageConnectivity for DefaultStorageConnectivity {
    fn check(
        &self,
        configuration: &StorageConfiguration,
    ) -> StorageConfigurationResult<StorageHealth> {
        match configuration.backend() {
            StorageBackend::Local(settings) => {
                crate::infrastructure::storage::LocalStorage::new(settings.root())
                    .map(|_| StorageHealth::Healthy)
                    .map_err(|error| {
                        connectivity_failure(StorageBackend::Local(settings.clone()), error)
                    })
            }
            StorageBackend::S3(settings) => check_s3(configuration, settings),
            StorageBackend::WebDav(settings) => check_webdav(configuration, settings),
        }
    }
}

fn connectivity_failure(backend: StorageBackend, error: StorageError) -> StorageConfigurationError {
    StorageConfigurationError::ConnectivityFailure {
        backend: backend.kind(),
        error,
    }
}

#[cfg(feature = "s3")]
fn check_s3(
    configuration: &StorageConfiguration,
    settings: &crate::application::ports::S3StorageSettings,
) -> StorageConfigurationResult<StorageHealth> {
    use crate::application::ports::{ObjectListRequest, RepositoryStorage};
    use crate::infrastructure::storage::S3Storage;
    use crate::infrastructure::storage::S3StorageConfig;

    let credentials = configuration
        .credentials()
        .and_then(crate::application::ports::StorageCredentials::as_s3)
        .ok_or(StorageConfigurationError::InvalidConfiguration)?;
    let mut adapter_configuration = S3StorageConfig::new(
        settings.region(),
        settings.bucket(),
        credentials.access_key(),
        credentials.secret_key(),
    )
    .map_err(|error| map_adapter_configuration_error(StorageBackend::S3(settings.clone()), error))?
    .with_force_path_style(settings.force_path_style())
    .with_multipart_threshold(settings.multipart_threshold())
    .with_multipart_part_size(settings.multipart_part_size())
    .with_max_concurrency(settings.max_concurrency())
    .without_capability_cache();
    if let Some(session_token) = credentials.session_token() {
        adapter_configuration = adapter_configuration.with_session_token(session_token);
    }
    if let Some(endpoint) = settings.endpoint() {
        adapter_configuration = adapter_configuration.with_endpoint(endpoint);
        adapter_configuration =
            adapter_configuration.with_force_path_style(settings.force_path_style());
    }
    let storage = S3Storage::new(adapter_configuration).map_err(|error| {
        map_adapter_configuration_error(StorageBackend::S3(settings.clone()), error)
    })?;
    storage
        .list_page(&ObjectListRequest::root().with_limit(1))
        .map(|_| StorageHealth::Healthy)
        .map_err(|error| connectivity_failure(StorageBackend::S3(settings.clone()), error))
}

#[cfg(not(feature = "s3"))]
fn check_s3(
    _configuration: &StorageConfiguration,
    _settings: &crate::application::ports::S3StorageSettings,
) -> StorageConfigurationResult<StorageHealth> {
    Err(connectivity_failure(
        StorageBackend::S3(_settings.clone()),
        StorageError::UnsupportedCapability,
    ))
}

#[cfg(feature = "webdav")]
fn check_webdav(
    configuration: &StorageConfiguration,
    settings: &crate::application::ports::WebDavStorageSettings,
) -> StorageConfigurationResult<StorageHealth> {
    use crate::infrastructure::storage::{WebDavStorage, WebDavStorageConfig};

    let credentials = configuration
        .credentials()
        .and_then(crate::application::ports::StorageCredentials::as_webdav)
        .ok_or(StorageConfigurationError::InvalidConfiguration)?;
    let adapter_configuration = WebDavStorageConfig::new(
        settings.collection_url(),
        credentials.username(),
        credentials.password(),
    )
    .map_err(|error| {
        map_adapter_configuration_error(StorageBackend::WebDav(settings.clone()), error)
    })?
    .with_allow_insecure_http(settings.allow_insecure_http())
    .with_max_concurrency(settings.max_concurrency());
    WebDavStorage::new(adapter_configuration)
        .map(|_| StorageHealth::Healthy)
        .map_err(|error| connectivity_failure(StorageBackend::WebDav(settings.clone()), error))
}

#[cfg(not(feature = "webdav"))]
fn check_webdav(
    _configuration: &StorageConfiguration,
    settings: &crate::application::ports::WebDavStorageSettings,
) -> StorageConfigurationResult<StorageHealth> {
    Err(connectivity_failure(
        StorageBackend::WebDav(settings.clone()),
        StorageError::UnsupportedCapability,
    ))
}

#[cfg(any(feature = "s3", feature = "webdav"))]
fn map_adapter_configuration_error(
    backend: StorageBackend,
    error: StorageError,
) -> StorageConfigurationError {
    if matches!(error, StorageError::InvalidRequest) {
        StorageConfigurationError::InvalidConfiguration
    } else {
        connectivity_failure(backend, error)
    }
}
