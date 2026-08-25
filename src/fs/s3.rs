use crate::fs::FS;
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3 as s3;
use aws_types::region::Region;
use bytes::Bytes;

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
            s3_config_builder = s3_config_builder.endpoint_url(endpoint);
        }
        let s3_config = s3_config_builder.build();

        let client = s3::Client::from_conf(s3_config);

        Self { client, bucket }
    }
}

fn s3_io_error(message: String) -> std::io::Error {
    let kind =
        if message.contains("NoSuchKey") || message.contains("NotFound") || message.contains("404")
        {
            std::io::ErrorKind::NotFound
        } else {
            std::io::ErrorKind::Other
        };
    std::io::Error::new(kind, message)
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
            .map_err(|e| s3_io_error(e.to_string()))?;

        let data = resp
            .body
            .collect()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

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
            .map_err(|e| s3_io_error(e.to_string()))?;

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

            let resp = req.send().await.map_err(|e| s3_io_error(e.to_string()))?;

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
            .map_err(|e| s3_io_error(e.to_string()))?;
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
            .map_err(|e| s3_io_error(e.to_string()))?;

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

        request.send().await.map_err(|e| {
            let message = e.to_string();
            let kind = if message.contains("Precondition")
                || message.contains("precondition")
                || message.contains("412")
            {
                std::io::ErrorKind::AlreadyExists
            } else {
                std::io::ErrorKind::Other
            };
            std::io::Error::new(kind, message)
        })?;

        Ok(())
    }
}
