use super::repository::{FormatError, decode_messagepack};
use crate::application::ports::{
    CURRENT_STORAGE_BACKEND_VERSION, CURRENT_STORAGE_CONFIGURATION_VERSION, StorageBackend,
    StorageConfigurationError,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistedStorageBackend {
    Local {
        root_path: String,
    },
    S3 {
        region: String,
        bucket: String,
        endpoint: Option<String>,
        force_path_style: bool,
        multipart_threshold: u64,
        multipart_part_size: u64,
        max_concurrency: usize,
        capability_cache_path: Option<String>,
    },
    WebDav {
        collection_url: String,
        allow_insecure_http: bool,
        max_concurrency: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedStorageConfiguration {
    pub(crate) backend: PersistedStorageBackend,
    pub(crate) credential_reference: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StorageConfigurationFormatError {
    Serialization,
    InvalidEncoding,
    InputTooLarge,
    TrailingBytes,
    UnsupportedSchemaVersion { version: u16 },
    UnsupportedBackend { kind: String },
    UnsupportedBackendVersion { kind: String, version: u16 },
}

#[derive(Serialize)]
struct StorageConfigurationWire<'a> {
    schema_version: u16,
    backend: &'a str,
    backend_version: u16,
    credential_reference: Option<&'a str>,
    root_path: Option<&'a str>,
    region: Option<&'a str>,
    bucket: Option<&'a str>,
    endpoint: Option<&'a str>,
    force_path_style: Option<bool>,
    multipart_threshold: Option<u64>,
    multipart_part_size: Option<u64>,
    max_concurrency: Option<u64>,
    capability_cache_path: Option<&'a str>,
    collection_url: Option<&'a str>,
    allow_insecure_http: Option<bool>,
}

#[derive(Deserialize)]
struct StorageConfigurationWireOwned {
    schema_version: u16,
    backend: String,
    backend_version: u16,
    credential_reference: Option<String>,
    root_path: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    endpoint: Option<String>,
    force_path_style: Option<bool>,
    multipart_threshold: Option<u64>,
    multipart_part_size: Option<u64>,
    max_concurrency: Option<u64>,
    capability_cache_path: Option<String>,
    collection_url: Option<String>,
    allow_insecure_http: Option<bool>,
}

pub(crate) fn encode_storage_configuration(
    backend: &StorageBackend,
    credential_reference: Option<&str>,
) -> Result<Vec<u8>, StorageConfigurationFormatError> {
    let mut wire = StorageConfigurationWire {
        schema_version: CURRENT_STORAGE_CONFIGURATION_VERSION,
        backend: "",
        backend_version: CURRENT_STORAGE_BACKEND_VERSION,
        credential_reference,
        root_path: None,
        region: None,
        bucket: None,
        endpoint: None,
        force_path_style: None,
        multipart_threshold: None,
        multipart_part_size: None,
        max_concurrency: None,
        capability_cache_path: None,
        collection_url: None,
        allow_insecure_http: None,
    };
    match backend {
        StorageBackend::Local(settings) => {
            wire.backend = "local";
            wire.root_path = settings.root().to_str();
            if wire.root_path.is_none() {
                return Err(StorageConfigurationFormatError::Serialization);
            }
        }
        StorageBackend::S3(settings) => {
            wire.backend = "s3";
            wire.region = Some(settings.region());
            wire.bucket = Some(settings.bucket());
            wire.endpoint = settings.endpoint();
            wire.force_path_style = Some(settings.force_path_style());
            wire.multipart_threshold = Some(settings.multipart_threshold());
            wire.multipart_part_size = Some(settings.multipart_part_size());
            wire.max_concurrency = Some(
                u64::try_from(settings.max_concurrency())
                    .map_err(|_| StorageConfigurationFormatError::Serialization)?,
            );
            if let Some(path) = settings.capability_cache_path() {
                wire.capability_cache_path = Some(
                    path.to_str()
                        .ok_or(StorageConfigurationFormatError::Serialization)?,
                );
            }
        }
        StorageBackend::WebDav(settings) => {
            wire.backend = "webdav";
            wire.collection_url = Some(settings.collection_url());
            wire.allow_insecure_http = Some(settings.allow_insecure_http());
            wire.max_concurrency = Some(
                u64::try_from(settings.max_concurrency())
                    .map_err(|_| StorageConfigurationFormatError::Serialization)?,
            );
        }
    }
    let bytes = rmp_serde::to_vec_named(&wire)
        .map_err(|_| StorageConfigurationFormatError::Serialization)?;
    if bytes.len() > crate::application::ports::MAX_STORAGE_CONFIGURATION_BYTES {
        return Err(StorageConfigurationFormatError::InputTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_storage_configuration(
    bytes: &[u8],
) -> Result<DecodedStorageConfiguration, StorageConfigurationFormatError> {
    let wire: StorageConfigurationWireOwned = decode_messagepack(
        bytes,
        crate::application::ports::MAX_STORAGE_CONFIGURATION_BYTES,
    )
    .map_err(map_format_error)?;
    if wire.schema_version != CURRENT_STORAGE_CONFIGURATION_VERSION {
        return Err(StorageConfigurationFormatError::UnsupportedSchemaVersion {
            version: wire.schema_version,
        });
    }
    let backend_kind = wire.backend.clone();
    if !matches!(backend_kind.as_str(), "local" | "s3" | "webdav") {
        return Err(StorageConfigurationFormatError::UnsupportedBackend { kind: backend_kind });
    }
    if wire.backend_version != CURRENT_STORAGE_BACKEND_VERSION {
        return Err(StorageConfigurationFormatError::UnsupportedBackendVersion {
            kind: backend_kind,
            version: wire.backend_version,
        });
    }
    let backend = match wire.backend.as_str() {
        "local" => PersistedStorageBackend::Local {
            root_path: required_string(wire.root_path)?,
        },
        "s3" => PersistedStorageBackend::S3 {
            region: required_string(wire.region)?,
            bucket: required_string(wire.bucket)?,
            endpoint: wire.endpoint,
            force_path_style: wire
                .force_path_style
                .ok_or(StorageConfigurationFormatError::InvalidEncoding)?,
            multipart_threshold: wire
                .multipart_threshold
                .ok_or(StorageConfigurationFormatError::InvalidEncoding)?,
            multipart_part_size: wire
                .multipart_part_size
                .ok_or(StorageConfigurationFormatError::InvalidEncoding)?,
            max_concurrency: usize::try_from(
                wire.max_concurrency
                    .ok_or(StorageConfigurationFormatError::InvalidEncoding)?,
            )
            .map_err(|_| StorageConfigurationFormatError::InvalidEncoding)?,
            capability_cache_path: wire.capability_cache_path,
        },
        "webdav" => PersistedStorageBackend::WebDav {
            collection_url: required_string(wire.collection_url)?,
            allow_insecure_http: wire
                .allow_insecure_http
                .ok_or(StorageConfigurationFormatError::InvalidEncoding)?,
            max_concurrency: usize::try_from(
                wire.max_concurrency
                    .ok_or(StorageConfigurationFormatError::InvalidEncoding)?,
            )
            .map_err(|_| StorageConfigurationFormatError::InvalidEncoding)?,
        },
        _ => {
            return Err(StorageConfigurationFormatError::UnsupportedBackend { kind: backend_kind });
        }
    };
    Ok(DecodedStorageConfiguration {
        backend,
        credential_reference: wire.credential_reference,
    })
}

fn required_string(value: Option<String>) -> Result<String, StorageConfigurationFormatError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or(StorageConfigurationFormatError::InvalidEncoding)
}

fn map_format_error(error: FormatError) -> StorageConfigurationFormatError {
    match error {
        FormatError::InputTooLarge => StorageConfigurationFormatError::InputTooLarge,
        FormatError::TrailingBytes => StorageConfigurationFormatError::TrailingBytes,
        FormatError::Serialization
        | FormatError::InvalidEncoding
        | FormatError::InvalidMagic
        | FormatError::InvalidRootReference
        | FormatError::InvalidField
        | FormatError::MissingRequiredFeature
        | FormatError::UnsupportedRequiredFeature
        | FormatError::UnsupportedVersion { .. }
        | FormatError::VersionMismatch
        | FormatError::InvalidChecksum
        | FormatError::InvalidObjectKind
        | FormatError::UnsupportedObjectVersion { .. }
        | FormatError::InvalidCodec
        | FormatError::UnsupportedCodec
        | FormatError::InvalidEncryption
        | FormatError::UnsupportedEncryption
        | FormatError::InvalidLength
        | FormatError::InvalidDigestLength
        | FormatError::InvalidObjectId
        | FormatError::InvalidPayloadChecksum
        | FormatError::InvalidEnvelopeChecksum => StorageConfigurationFormatError::InvalidEncoding,
    }
}

impl From<StorageConfigurationFormatError> for StorageConfigurationError {
    fn from(error: StorageConfigurationFormatError) -> Self {
        match error {
            StorageConfigurationFormatError::Serialization
            | StorageConfigurationFormatError::InvalidEncoding
            | StorageConfigurationFormatError::TrailingBytes => Self::Malformed,
            StorageConfigurationFormatError::InputTooLarge => Self::TooLarge,
            StorageConfigurationFormatError::UnsupportedSchemaVersion { version } => {
                Self::UnsupportedSchemaVersion { version }
            }
            StorageConfigurationFormatError::UnsupportedBackend { kind } => {
                Self::UnsupportedBackend { kind }
            }
            StorageConfigurationFormatError::UnsupportedBackendVersion { kind, version } => {
                Self::UnsupportedBackendVersion { kind, version }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{
        LocalStorageSettings, S3StorageSettings, StorageBackend, WebDavStorageSettings,
    };

    #[test]
    fn each_current_backend_round_trips_with_an_opaque_reference() {
        let local = StorageBackend::Local(
            LocalStorageSettings::new("/tmp/gib-storage-repository").expect("valid local path"),
        );
        let s3 = StorageBackend::S3(
            S3StorageSettings::new("us-east-1", "gib-test-bucket")
                .expect("valid S3 settings")
                .with_endpoint("https://s3.example.test")
                .expect("valid S3 endpoint")
                .with_force_path_style(true),
        );
        let webdav = StorageBackend::WebDav(
            WebDavStorageSettings::new("https://dav.example.test/collection")
                .expect("valid WebDAV URL"),
        );

        for backend in [local, s3, webdav] {
            let encoded = encode_storage_configuration(&backend, Some("opaque-ref"))
                .expect("backend should encode");
            let decoded = decode_storage_configuration(&encoded).expect("backend should decode");
            assert_eq!(decoded.credential_reference.as_deref(), Some("opaque-ref"));
            match (backend, decoded.backend) {
                (StorageBackend::Local(settings), PersistedStorageBackend::Local { root_path }) => {
                    assert_eq!(root_path, settings.root().to_str().expect("UTF-8 path"));
                }
                (
                    StorageBackend::S3(settings),
                    PersistedStorageBackend::S3 {
                        region,
                        bucket,
                        endpoint,
                        force_path_style,
                        ..
                    },
                ) => {
                    assert_eq!(region, settings.region());
                    assert_eq!(bucket, settings.bucket());
                    assert_eq!(endpoint.as_deref(), settings.endpoint());
                    assert_eq!(force_path_style, settings.force_path_style());
                }
                (
                    StorageBackend::WebDav(settings),
                    PersistedStorageBackend::WebDav { collection_url, .. },
                ) => {
                    assert_eq!(collection_url, settings.collection_url());
                }
                _ => panic!("backend variant changed during round trip"),
            }
        }
    }

    #[test]
    fn newer_schema_and_backend_versions_are_reported_explicitly() {
        let future_schema = encode_wire(99, "local", 1);
        assert_eq!(
            decode_storage_configuration(&future_schema),
            Err(StorageConfigurationFormatError::UnsupportedSchemaVersion { version: 99 })
        );

        let future_backend = encode_wire(1, "future", 1);
        assert_eq!(
            decode_storage_configuration(&future_backend),
            Err(StorageConfigurationFormatError::UnsupportedBackend {
                kind: String::from("future")
            })
        );

        let future_backend_version = encode_wire(1, "local", 99);
        assert_eq!(
            decode_storage_configuration(&future_backend_version),
            Err(StorageConfigurationFormatError::UnsupportedBackendVersion {
                kind: String::from("local"),
                version: 99
            })
        );
    }

    fn encode_wire(schema_version: u16, backend: &str, backend_version: u16) -> Vec<u8> {
        rmp_serde::to_vec_named(&StorageConfigurationWire {
            schema_version,
            backend,
            backend_version,
            credential_reference: None,
            root_path: Some("/tmp/gib-storage-repository"),
            region: None,
            bucket: None,
            endpoint: None,
            force_path_style: None,
            multipart_threshold: None,
            multipart_part_size: None,
            max_concurrency: None,
            capability_cache_path: None,
            collection_url: None,
            allow_insecure_http: None,
        })
        .expect("test wire should encode")
    }
}
