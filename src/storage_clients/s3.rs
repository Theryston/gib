use crate::storage_clients::{
    ClientStorage, StorageDefinition, StorageField, StorageFields,
};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3 as s3;
use aws_types::region::Region;
use bytes::Bytes;
use std::sync::Arc;

pub struct S3ClientStorage {
    client: s3::Client,
    bucket: String,
}

pub struct S3ClientStorageConfig {
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub endpoint: Option<String>,
}

impl S3ClientStorage {
    pub fn new(config: S3ClientStorageConfig) -> Self {
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

#[async_trait]
impl ClientStorage for S3ClientStorage {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

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
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

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
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

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
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }
}

fn build_client(fields: &StorageFields) -> Result<Arc<dyn ClientStorage>, String> {
    let region = fields
        .get("region")
        .ok_or_else(|| "Missing required field: region".to_string())?
        .clone();
    let bucket = fields
        .get("bucket")
        .ok_or_else(|| "Missing required field: bucket".to_string())?
        .clone();
    let access_key = fields
        .get("access_key")
        .ok_or_else(|| "Missing required field: access_key".to_string())?
        .clone();
    let secret_key = fields
        .get("secret_key")
        .ok_or_else(|| "Missing required field: secret_key".to_string())?
        .clone();
    let endpoint = fields
        .get("endpoint")
        .cloned()
        .unwrap_or_else(|| format!("https://s3.{}.amazonaws.com", region));

    Ok(Arc::new(S3ClientStorage::new(S3ClientStorageConfig {
        region: Some(region),
        bucket: Some(bucket),
        access_key: Some(access_key),
        secret_key: Some(secret_key),
        endpoint: Some(endpoint),
    })))
}

fn default_s3_endpoint(fields: &StorageFields) -> String {
    let region = fields
        .get("region")
        .cloned()
        .unwrap_or_else(|| "us-east-1".to_string());
    format!("https://s3.{}.amazonaws.com", region)
}

const S3_FIELDS: &[StorageField] = &[
    StorageField {
        key: "region",
        arg_name: "region",
        value_name: "REGION",
        short: Some('r'),
        help: "The region for the S3 storage (only for S3 storage)",
        prompt: "Enter the S3 region",
        required: true,
        secret: false,
        default_value: None,
    },
    StorageField {
        key: "bucket",
        arg_name: "bucket",
        value_name: "BUCKET",
        short: Some('b'),
        help: "The bucket for the S3 storage (only for S3 storage)",
        prompt: "Enter the S3 bucket",
        required: true,
        secret: false,
        default_value: None,
    },
    StorageField {
        key: "access_key",
        arg_name: "access-key",
        value_name: "ACCESS_KEY",
        short: Some('a'),
        help: "The access key for the S3 storage (only for S3 storage)",
        prompt: "Enter the S3 access key",
        required: true,
        secret: true,
        default_value: None,
    },
    StorageField {
        key: "secret_key",
        arg_name: "secret-key",
        value_name: "SECRET_KEY",
        short: Some('s'),
        help: "The secret key for the S3 storage (only for S3 storage)",
        prompt: "Enter the S3 secret key",
        required: true,
        secret: true,
        default_value: None,
    },
    StorageField {
        key: "endpoint",
        arg_name: "endpoint",
        value_name: "ENDPOINT",
        short: Some('e'),
        help: "The endpoint for the S3 storage (only for S3 storage)",
        prompt: "Enter the S3 endpoint",
        required: false,
        secret: false,
        default_value: Some(default_s3_endpoint),
    },
];

pub const DEFINITION: StorageDefinition = StorageDefinition {
    id: "s3",
    label: "s3",
    legacy_type: Some(1),
    fields: S3_FIELDS,
    build_client,
    prepare: None,
};
