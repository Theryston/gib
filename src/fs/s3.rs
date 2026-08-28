use crate::fs::FS;
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3 as s3;
use aws_types::{region::Region, sdk_config::RequestChecksumCalculation};
use bytes::Bytes;
use s3::error::ProvideErrorMetadata;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const S3_CAPABILITY_CACHE_DIRECTORY: &str = "s3-capabilities";

pub struct S3FS {
    client: s3::Client,
    bucket: String,
    capabilities: S3CapabilityCache,
}

pub struct S3FSConfig {
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub endpoint: Option<String>,
}

impl S3FS {
    pub fn new(config: S3FSConfig) -> Self {
        let region = config.region.expect("Region is required");
        let bucket = config.bucket.expect("Bucket is required");
        let access_key = config.access_key.expect("Access key is required");
        let secret_key = config.secret_key.expect("Secret key is required");
        let capabilities = S3CapabilityCache::new(&region, &bucket, config.endpoint.as_deref());

        let creds = Credentials::new(access_key, secret_key, None, None, "custom");

        let shared_config = aws_config::SdkConfig::builder()
            .credentials_provider(s3::config::SharedCredentialsProvider::new(creds))
            .region(Region::new(region))
            // Optional request checksums are not implemented by every S3-compatible
            // server. Required checksums remain enabled, while optional checksums do
            // not make otherwise compatible PUT requests fail.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .build();

        let mut s3_config_builder = s3::config::Builder::from(&shared_config);
        if let Some(endpoint) = config.endpoint {
            if should_use_path_style(&endpoint) {
                s3_config_builder = s3_config_builder.force_path_style(true);
            }
            s3_config_builder = s3_config_builder.endpoint_url(endpoint);
        }
        let s3_config = s3_config_builder.build();

        let client = s3::Client::from_conf(s3_config);

        Self {
            client,
            bucket,
            capabilities,
        }
    }

    async fn write_file_unconditionally(
        &self,
        path: &str,
        data: &[u8],
    ) -> Result<(), std::io::Error> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(path)
            .body(Bytes::from(data.to_vec()).into())
            .send()
            .await
            .map_err(|error| s3_io_error(&error, false))?;

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
enum CapabilitySupport {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct S3Capabilities {
    #[serde(default)]
    conditional_create: CapabilitySupport,
    #[serde(default)]
    conditional_update: CapabilitySupport,
}

#[derive(Clone, Copy)]
enum ConditionalWriteKind {
    Create,
    Update,
}

struct S3CapabilityCache {
    path: Option<PathBuf>,
    state: Mutex<S3Capabilities>,
}

impl S3CapabilityCache {
    fn new(region: &str, bucket: &str, endpoint: Option<&str>) -> Self {
        let path = capability_cache_path(region, bucket, endpoint);
        let state = path
            .as_deref()
            .and_then(load_capabilities)
            .unwrap_or_default();

        Self {
            path,
            state: Mutex::new(state),
        }
    }

    fn support(&self, kind: ConditionalWriteKind) -> CapabilitySupport {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match kind {
            ConditionalWriteKind::Create => state.conditional_create,
            ConditionalWriteKind::Update => state.conditional_update,
        }
    }

    fn mark_supported(&self, kind: ConditionalWriteKind) {
        self.update(kind, CapabilitySupport::Supported);
    }

    fn mark_unsupported(&self, kind: ConditionalWriteKind) {
        self.update(kind, CapabilitySupport::Unsupported);
    }

    fn update(&self, kind: ConditionalWriteKind, support: CapabilitySupport) {
        let snapshot = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let current = match kind {
                ConditionalWriteKind::Create => &mut state.conditional_create,
                ConditionalWriteKind::Update => &mut state.conditional_update,
            };

            if *current == support {
                return;
            }

            *current = support;
            *state
        };

        persist_capabilities(self.path.as_deref(), snapshot);
    }
}

fn capability_cache_path(region: &str, bucket: &str, endpoint: Option<&str>) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let endpoint = endpoint
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(|endpoint| endpoint.trim_end_matches('/').to_ascii_lowercase())
        .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
    let identity = format!("{endpoint}\n{region}\n{bucket}");
    let digest = Sha256::digest(identity.as_bytes());

    Some(
        home.join(".gib")
            .join(S3_CAPABILITY_CACHE_DIRECTORY)
            .join(format!("{digest:x}.msgpack")),
    )
}

fn load_capabilities(path: &Path) -> Option<S3Capabilities> {
    let bytes = std::fs::read(path).ok()?;
    rmp_serde::from_slice(&bytes).ok()
}

fn persist_capabilities(path: Option<&Path>, capabilities: S3Capabilities) {
    let Some(path) = path else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }

    let Ok(bytes) = rmp_serde::to_vec_named(&capabilities) else {
        return;
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}-{stamp}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("capabilities"),
        std::process::id()
    ));

    if std::fs::write(&temporary, bytes).is_err() {
        return;
    }

    if std::fs::rename(&temporary, path).is_err() {
        // Windows does not replace an existing file during rename. The cache is
        // recoverable state, so replacing it explicitly is safe here.
        let _ = std::fs::remove_file(path);
        let _ = std::fs::rename(&temporary, path);
        let _ = std::fs::remove_file(temporary);
    }
}

fn should_use_path_style(endpoint: &str) -> bool {
    !endpoint.to_ascii_lowercase().contains("amazonaws.com")
}

fn is_missing_object_error(code: Option<&str>, status: Option<u16>) -> bool {
    if let Some(code) = code {
        let normalized = code
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect::<String>();

        return matches!(
            normalized.as_str(),
            "nosuchkey" | "notfound" | "nosuchobject" | "objectnotfound"
        );
    }

    status == Some(404)
}

fn is_precondition_failure<E>(error: &s3::error::SdkError<E>) -> bool
where
    E: ProvideErrorMetadata,
{
    let code = error.code().map(|value| {
        value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect::<String>()
    });
    let message = error.message().unwrap_or_default().to_ascii_lowercase();
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());

    status == Some(412)
        || code.as_deref() == Some("preconditionfailed")
        || message.contains("precondition")
}

fn is_conditional_write_unsupported<E>(error: &s3::error::SdkError<E>) -> bool
where
    E: ProvideErrorMetadata,
{
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = error.code().map(normalize_error_value);
    let message = error.message().unwrap_or_default().to_ascii_lowercase();

    status == Some(501)
        || code.as_deref() == Some("notimplemented")
        || message.contains("not implemented")
        || message.contains("unsupported")
        || message.contains("unknown header")
}

fn normalize_error_value(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn s3_error_details<E>(error: &s3::error::SdkError<E>) -> String
where
    E: ProvideErrorMetadata,
{
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = error.code();
    let message = error.message();
    let mut details = Vec::new();

    if let Some(status) = status {
        details.push(format!("status {status}"));
    }
    if let Some(code) = code {
        details.push(format!("code {code}"));
    }
    if let Some(message) = message.filter(|message| !message.trim().is_empty()) {
        details.push(format!("message {message}"));
    }

    if details.is_empty() {
        error.to_string()
    } else {
        details.join(", ")
    }
}

fn s3_io_error<E>(error: &s3::error::SdkError<E>, object_lookup: bool) -> Error
where
    E: ProvideErrorMetadata,
{
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = error.code();
    let kind = if object_lookup && is_missing_object_error(code, status) {
        ErrorKind::NotFound
    } else {
        ErrorKind::Other
    };

    Error::new(
        kind,
        format!("S3 request failed: {}", s3_error_details(error)),
    )
}

#[async_trait]
impl FS for S3FS {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|error| s3_io_error(&error, true))?;

        let data = resp.body.collect().await.map_err(|error| {
            Error::new(
                ErrorKind::Other,
                format!("S3 response body read failed: {error}"),
            )
        })?;

        Ok(data.into_bytes().to_vec())
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error> {
        self.write_file_unconditionally(path, data).await
    }

    async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        let mut files = Vec::new();
        let mut continuation_token = None;
        let prefix = if path.is_empty() {
            "".to_string()
        } else if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{}/", path)
        };

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .map_err(|error| s3_io_error(&error, false))?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    files.push(key.to_string());
                }
            }

            continuation_token = resp.next_continuation_token().map(|ct| ct.to_string());

            if continuation_token.is_none() {
                break;
            }
        }

        Ok(files)
    }

    async fn delete_file(&self, path: &str) -> Result<(), std::io::Error> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|error| s3_io_error(&error, false))?;
        Ok(())
    }

    async fn read_file_with_version(
        &self,
        path: &str,
    ) -> Result<(Vec<u8>, String), std::io::Error> {
        // GetObject already returns the ETag, so avoid the extra HEAD request
        // that used to precede every versioned read. Catalog shard updates can
        // read hundreds of objects during a backup finalization.
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|error| s3_io_error(&error, true))?;

        let version = response.e_tag().map(ToString::to_string).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "S3 did not return an ETag for the repository reference",
            )
        })?;
        let data = response.body.collect().await.map_err(|error| {
            Error::new(
                ErrorKind::Other,
                format!("S3 response body read failed: {error}"),
            )
        })?;

        Ok((data.into_bytes().to_vec(), version))
    }

    async fn write_file_if_version(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<&str>,
    ) -> Result<(), std::io::Error> {
        let kind = if expected_version.is_some() {
            ConditionalWriteKind::Update
        } else {
            ConditionalWriteKind::Create
        };

        if self.capabilities.support(kind) == CapabilitySupport::Unsupported {
            return self.write_file_unconditionally(path, data).await;
        }

        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(path)
            .body(Bytes::from(data.to_vec()).into());

        if let Some(expected_version) = expected_version {
            request = request.if_match(expected_version);
        } else {
            request = request.if_none_match("*");
        }

        match request.send().await {
            Ok(_) => {
                self.capabilities.mark_supported(kind);
                Ok(())
            }
            Err(error) if is_conditional_write_unsupported(&error) => {
                self.capabilities.mark_unsupported(kind);
                self.write_file_unconditionally(path, data).await
            }
            Err(error) if is_precondition_failure(&error) => {
                self.capabilities.mark_supported(kind);
                Err(Error::new(
                    ErrorKind::AlreadyExists,
                    format!("S3 conditional write failed: {}", s3_error_details(&error)),
                ))
            }
            Err(error) => Err(s3_io_error(&error, false)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_path_style_for_non_aws_endpoints() {
        assert!(should_use_path_style(
            "https://s3.us-east-005.backblazeb2.com"
        ));
        assert!(!should_use_path_style("https://s3.us-east-1.amazonaws.com"));
    }

    #[test]
    fn recognizes_provider_object_not_found_codes() {
        assert!(is_missing_object_error(Some("NoSuchKey"), Some(404)));
        assert!(is_missing_object_error(Some("not_found"), Some(404)));
        assert!(is_missing_object_error(Some("ObjectNotFound"), None));
        assert!(is_missing_object_error(None, Some(404)));
    }

    #[test]
    fn does_not_treat_a_missing_bucket_as_a_missing_object() {
        assert!(!is_missing_object_error(Some("NoSuchBucket"), Some(404)));
    }

    #[test]
    fn capability_cache_identity_is_specific_to_the_remote_storage() {
        let first = capability_cache_path("us-east-1", "backups", Some("https://s3.example.test"));
        let same = capability_cache_path("us-east-1", "backups", Some("https://s3.example.test/"));
        let different_bucket = capability_cache_path(
            "us-east-1",
            "other-backups",
            Some("https://s3.example.test"),
        );

        assert_eq!(first, same);
        assert_ne!(first, different_bucket);
    }

    #[test]
    fn capabilities_are_backward_compatible_with_an_empty_cache() {
        let capabilities = S3Capabilities::default();
        let encoded = rmp_serde::to_vec_named(&capabilities).unwrap();
        let decoded: S3Capabilities = rmp_serde::from_slice(&encoded).unwrap();

        assert_eq!(decoded.conditional_create, CapabilitySupport::Unknown);
        assert_eq!(decoded.conditional_update, CapabilitySupport::Unknown);
    }
}
