use crate::fs::FS;
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3 as s3;
use aws_types::region::Region;
use bytes::Bytes;
use s3::error::ProvideErrorMetadata;
use std::io::{Error, ErrorKind};

pub struct S3FS {
    client: s3::Client,
    bucket: String,
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

        let creds = Credentials::new(access_key, secret_key, None, None, "custom");

        let shared_config = aws_config::SdkConfig::builder()
            .credentials_provider(s3::config::SharedCredentialsProvider::new(creds))
            .region(Region::new(region))
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

        Self { client, bucket }
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
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|error| s3_io_error(&error, true))?;

        let version = head.e_tag().map(ToString::to_string).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "S3 did not return an ETag for the repository reference",
            )
        })?;
        let data = self.read_file(path).await?;
        Ok((data, version))
    }

    async fn write_file_if_version(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<&str>,
    ) -> Result<(), std::io::Error> {
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

        request.send().await.map_err(|error| {
            if is_precondition_failure(&error) {
                Error::new(
                    ErrorKind::AlreadyExists,
                    format!("S3 conditional write failed: {}", s3_error_details(&error)),
                )
            } else {
                s3_io_error(&error, false)
            }
        })?;

        Ok(())
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
}
