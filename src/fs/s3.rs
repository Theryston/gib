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
}
