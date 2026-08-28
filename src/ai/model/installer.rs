use super::config::AiConfigStore;
use super::error::ModelError;
use super::lock::{FileLock, ModelInstallCancellation};
use super::paths::ModelPaths;
use super::registry::{MODEL_MANIFEST_SCHEMA_VERSION, ModelManifest, ModelRegistry};
use super::storage::{hash_file, now_unix_seconds, quarantine, write_atomic};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PARTIAL_STATE_VERSION: u32 = 1;
const INSTALL_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EVENT_MESSAGE_BYTES: usize = 512;

pub(crate) type ProgressCallback = Arc<dyn Fn(ModelInstallEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelInstallPhase {
    Resolving,
    Checking,
    Downloading,
    Verifying,
    Installing,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelInstallStatus {
    Installed,
    Resumed,
    Downloaded,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ModelInstallEvent {
    pub(crate) model_id: String,
    pub(crate) phase: ModelInstallPhase,
    pub(crate) bytes_received: u64,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) percent: Option<u8>,
    pub(crate) resumable: bool,
    pub(crate) message: String,
    pub(crate) status: Option<ModelInstallStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstallationStatus {
    Installed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InstallationMetadata {
    pub(crate) schema_version: u32,
    pub(crate) status: InstallationStatus,
    pub(crate) manifest: ModelManifest,
    pub(crate) artifact_path: String,
    pub(crate) installed_at: u64,
    pub(crate) verified_size: u64,
    pub(crate) verified_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct InstalledModel {
    pub(crate) manifest: ModelManifest,
    pub(crate) artifact_path: PathBuf,
    pub(crate) metadata_path: PathBuf,
    pub(crate) installed_at: u64,
    pub(crate) verified_size: u64,
    pub(crate) verified_sha256: String,
}

type DownloadStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

struct DownloadResponse {
    status: StatusCode,
    content_range: Option<String>,
    content_length: Option<u64>,
    body: DownloadStream,
}

#[async_trait]
trait DownloadTransport: Send + Sync {
    async fn get(
        &self,
        url: &str,
        range_start: Option<u64>,
    ) -> Result<DownloadResponse, ModelError>;
}

struct ReqwestDownloadTransport {
    client: Client,
}

#[async_trait]
impl DownloadTransport for ReqwestDownloadTransport {
    async fn get(
        &self,
        url: &str,
        range_start: Option<u64>,
    ) -> Result<DownloadResponse, ModelError> {
        let mut request = self.client.get(url);
        if let Some(range_start) = range_start {
            request = request.header(RANGE, format!("bytes={range_start}-"));
        }
        let response = request.send().await.map_err(|error| ModelError::Http {
            url: url.to_string(),
            message: error.to_string(),
        })?;
        let status = response.status();
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let content_length = response.content_length();
        let body = Box::pin(futures::stream::unfold(
            Some(response),
            |state| async move {
                let mut response = state?;
                match response.chunk().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), Some(response))),
                    Ok(None) => None,
                    Err(error) => Some((Err(error.to_string()), None)),
                }
            },
        ));
        Ok(DownloadResponse {
            status,
            content_range,
            content_length,
            body,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PartialDownloadState {
    schema_version: u32,
    model_id: String,
    manifest_version: String,
    download_url: String,
    expected_size: u64,
    sha256: String,
}

impl PartialDownloadState {
    fn from_manifest(manifest: &ModelManifest, expected_size: u64, sha256: &str) -> Self {
        Self {
            schema_version: PARTIAL_STATE_VERSION,
            model_id: manifest.id.clone(),
            manifest_version: manifest.version.clone(),
            download_url: manifest.download_url.clone(),
            expected_size,
            sha256: sha256.to_ascii_lowercase(),
        }
    }

    fn matches(&self, manifest: &ModelManifest, expected_size: u64, sha256: &str) -> bool {
        self.schema_version == PARTIAL_STATE_VERSION
            && self.model_id == manifest.id
            && self.manifest_version == manifest.version
            && self.download_url == manifest.download_url
            && self.expected_size == expected_size
            && digests_equal(&self.sha256, sha256)
    }
}

#[derive(Clone)]
pub(crate) struct ModelManager {
    registry: ModelRegistry,
    paths: ModelPaths,
    transport: Arc<dyn DownloadTransport>,
    lock_timeout: Duration,
}

impl ModelManager {
    pub(crate) fn new(registry: ModelRegistry, paths: ModelPaths) -> Result<Self, ModelError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ModelError::Http {
                url: "client".to_string(),
                message: error.to_string(),
            })?;
        Self::with_client(registry, paths, client)
    }

    pub(crate) fn default() -> Result<Self, ModelError> {
        Self::new(ModelRegistry::default(), ModelPaths::default()?)
    }

    pub(crate) fn with_client(
        registry: ModelRegistry,
        paths: ModelPaths,
        client: Client,
    ) -> Result<Self, ModelError> {
        Self::with_transport(
            registry,
            paths,
            Arc::new(ReqwestDownloadTransport { client }),
        )
    }

    fn with_transport(
        registry: ModelRegistry,
        paths: ModelPaths,
        transport: Arc<dyn DownloadTransport>,
    ) -> Result<Self, ModelError> {
        registry.validate()?;
        Ok(Self {
            registry,
            paths,
            transport,
            lock_timeout: INSTALL_LOCK_TIMEOUT,
        })
    }

    pub(crate) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    pub(crate) fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    pub(crate) fn paths(&self) -> &ModelPaths {
        &self.paths
    }

    pub(crate) fn resolve_model(&self, model_id: &str) -> Result<&ModelManifest, ModelError> {
        self.registry.get(model_id)
    }

    pub(crate) fn active_model_id(&self) -> Result<Option<String>, ModelError> {
        AiConfigStore::new(self.paths.clone()).active_model_id()
    }

    pub(crate) fn verify_installed(&self, model_id: &str) -> Result<InstalledModel, ModelError> {
        let manifest = self.registry.get(model_id)?;
        manifest.validate()?;
        manifest.require_integrity()?;
        let artifact_path = self.paths.artifact_path(manifest)?;
        let metadata_path = self.paths.metadata_path(manifest)?;
        self.verify_manifest_files(manifest, &artifact_path, &metadata_path)
    }

    pub(crate) fn list_installed(&self) -> Result<Vec<InstalledModel>, ModelError> {
        self.paths.ensure_root()?;
        let mut installed = Vec::new();
        for manifest in self.registry.iter() {
            if manifest.require_integrity().is_err() {
                continue;
            }
            match self.verify_installed(&manifest.id) {
                Ok(model) => installed.push(model),
                Err(
                    ModelError::NotInstalled(_)
                    | ModelError::MetadataMismatch(_)
                    | ModelError::Serialization { .. }
                    | ModelError::ChecksumMismatch { .. }
                    | ModelError::SizeMismatch { .. },
                ) => {}
                Err(error) => return Err(error),
            }
        }
        installed.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
        Ok(installed)
    }

    pub(crate) async fn set_active_model(
        &self,
        model_id: &str,
    ) -> Result<InstalledModel, ModelError> {
        let installed = self.verify_installed(model_id)?;
        AiConfigStore::new(self.paths.clone())
            .set_active_model(model_id)
            .await?;
        Ok(installed)
    }

    pub(crate) async fn ensure_active_model(
        &self,
        progress: Option<ProgressCallback>,
    ) -> Result<InstalledModel, ModelError> {
        self.ensure_active_model_with_cancellation(progress, None)
            .await
    }

    pub(crate) async fn ensure_active_model_with_cancellation(
        &self,
        progress: Option<ProgressCallback>,
        cancellation: Option<ModelInstallCancellation>,
    ) -> Result<InstalledModel, ModelError> {
        if cancellation
            .as_ref()
            .is_some_and(ModelInstallCancellation::is_cancelled)
        {
            return Err(ModelError::DownloadCancelled);
        }
        let model_id = self
            .active_model_id()?
            .unwrap_or_else(|| super::registry::DEFAULT_MODEL_ID.to_string());
        let installed = self
            .ensure_installed_with_cancellation(&model_id, progress, cancellation.clone())
            .await?;
        if cancellation
            .as_ref()
            .is_some_and(ModelInstallCancellation::is_cancelled)
        {
            return Err(ModelError::DownloadCancelled);
        }
        if self.active_model_id()?.is_none() {
            AiConfigStore::new(self.paths.clone())
                .set_active_model(&model_id)
                .await?;
        }
        Ok(installed)
    }

    pub(crate) async fn ensure_installed(
        &self,
        model_id: &str,
        progress: Option<ProgressCallback>,
    ) -> Result<InstalledModel, ModelError> {
        self.ensure_installed_with_cancellation(model_id, progress, None)
            .await
    }

    pub(crate) async fn ensure_installed_with_cancellation(
        &self,
        model_id: &str,
        progress: Option<ProgressCallback>,
        cancellation: Option<ModelInstallCancellation>,
    ) -> Result<InstalledModel, ModelError> {
        let manifest = self.registry.get(model_id)?.clone();
        self.install_manifest(&manifest, progress, cancellation)
            .await
    }

    pub(crate) async fn install(
        &self,
        model_id: &str,
        progress: Option<ProgressCallback>,
    ) -> Result<InstalledModel, ModelError> {
        self.ensure_installed(model_id, progress).await
    }

    async fn install_manifest(
        &self,
        manifest: &ModelManifest,
        progress: Option<ProgressCallback>,
        cancellation: Option<ModelInstallCancellation>,
    ) -> Result<InstalledModel, ModelError> {
        if cancellation
            .as_ref()
            .is_some_and(ModelInstallCancellation::is_cancelled)
        {
            return Err(ModelError::DownloadCancelled);
        }
        manifest.validate()?;
        let (expected_size, expected_sha256) = manifest.require_integrity()?;
        let lock_path = self.paths.model_lock_path(&manifest.id)?;
        let lock = FileLock::acquire_with_cancellation(
            &lock_path,
            self.lock_timeout,
            cancellation.as_ref(),
        )
        .await?;
        if cancellation
            .as_ref()
            .is_some_and(ModelInstallCancellation::is_cancelled)
        {
            return Err(ModelError::DownloadCancelled);
        }

        let artifact_path = self.paths.artifact_path(manifest)?;
        let metadata_path = self.paths.metadata_path(manifest)?;
        emit_event(
            &progress,
            ModelInstallEvent::new(
                manifest,
                ModelInstallPhase::Resolving,
                0,
                Some(expected_size),
                false,
                "Resolved the registered model",
                None,
            ),
        );

        emit_event(
            &progress,
            ModelInstallEvent::new(
                manifest,
                ModelInstallPhase::Checking,
                0,
                Some(expected_size),
                false,
                "Checking the local model artifact",
                None,
            ),
        );
        match self.verify_manifest_files(manifest, &artifact_path, &metadata_path) {
            Ok(installed) => {
                emit_event(
                    &progress,
                    ModelInstallEvent::new(
                        manifest,
                        ModelInstallPhase::Complete,
                        installed.verified_size,
                        Some(expected_size),
                        false,
                        "The model is already installed and verified",
                        Some(ModelInstallStatus::Installed),
                    ),
                );
                return Ok(installed);
            }
            Err(
                ModelError::NotInstalled(_)
                | ModelError::MetadataMismatch(_)
                | ModelError::Serialization { .. }
                | ModelError::ChecksumMismatch { .. }
                | ModelError::SizeMismatch { .. },
            ) => {
                quarantine_if_present(&artifact_path, "invalid-artifact")?;
                quarantine_if_present(&metadata_path, "invalid-metadata")?;
            }
            Err(error) => return Err(error),
        }

        let partial_path = self.paths.partial_path(manifest)?;
        let partial_state_path = self.paths.partial_state_path(manifest)?;
        let _ = self.prepare_partial_state(
            manifest,
            expected_size,
            expected_sha256,
            &partial_path,
            &partial_state_path,
        )?;
        let mut partial_size = file_size_if_present(&partial_path)?;
        loop {
            if partial_size > expected_size {
                quarantine_if_present(&partial_path, "oversized-partial")?;
                quarantine_if_present(&partial_state_path, "oversized-partial-state")?;
                let _ = self.prepare_partial_state(
                    manifest,
                    expected_size,
                    expected_sha256,
                    &partial_path,
                    &partial_state_path,
                )?;
                partial_size = 0;
                continue;
            }

            if partial_size == expected_size {
                let (actual_size, actual_sha256) = hash_file(&partial_path)?;
                if actual_size == expected_size && digests_equal(&actual_sha256, expected_sha256) {
                    emit_event(
                        &progress,
                        ModelInstallEvent::new(
                            manifest,
                            ModelInstallPhase::Verifying,
                            actual_size,
                            Some(expected_size),
                            true,
                            "The completed partial artifact passed verification",
                            None,
                        ),
                    );
                    return self.publish_verified(
                        manifest,
                        &partial_path,
                        &partial_state_path,
                        &artifact_path,
                        &metadata_path,
                        actual_size,
                        &actual_sha256,
                        progress,
                        true,
                    );
                }
                quarantine_if_present(&partial_path, "checksum-mismatch")?;
                quarantine_if_present(&partial_state_path, "checksum-mismatch-state")?;
                let _ = self.prepare_partial_state(
                    manifest,
                    expected_size,
                    expected_sha256,
                    &partial_path,
                    &partial_state_path,
                )?;
                partial_size = 0;
                continue;
            }

            break;
        }

        let (mut hasher, mut received) = seed_hasher(&partial_path)?;
        emit_event(
            &progress,
            ModelInstallEvent::new(
                manifest,
                ModelInstallPhase::Downloading,
                received,
                Some(expected_size),
                received > 0,
                if received > 0 {
                    "Resuming the model download"
                } else {
                    "Starting the model download"
                },
                None,
            ),
        );

        let response_result = if let Some(cancellation) = cancellation.as_ref() {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    let message =
                        "The model download was cancelled; the partial download was preserved";
                    emit_event(
                        &progress,
                        ModelInstallEvent::new(
                            manifest,
                            ModelInstallPhase::Failed,
                            received,
                            Some(expected_size),
                            received > 0,
                            message,
                            Some(ModelInstallStatus::Cancelled),
                        ),
                    );
                    return Err(ModelError::DownloadCancelled);
                }
                response = self
                    .transport
                    .get(&manifest.download_url, (received > 0).then_some(received)) => response,
            }
        } else {
            self.transport
                .get(&manifest.download_url, (received > 0).then_some(received))
                .await
        };
        let mut response = match response_result {
            Ok(response) => response,
            Err(error) => {
                emit_event(
                    &progress,
                    ModelInstallEvent::new(
                        manifest,
                        ModelInstallPhase::Failed,
                        received,
                        Some(expected_size),
                        received > 0,
                        &error.to_string(),
                        Some(ModelInstallStatus::Failed),
                    ),
                );
                return Err(error);
            }
        };
        let response_mode =
            match validate_response(&response, received, expected_size, &manifest.download_url) {
                Ok(mode) => mode,
                Err(error) => {
                    emit_event(
                        &progress,
                        ModelInstallEvent::new(
                            manifest,
                            ModelInstallPhase::Failed,
                            received,
                            Some(expected_size),
                            received > 0,
                            &error.to_string(),
                            Some(ModelInstallStatus::Failed),
                        ),
                    );
                    return Err(error);
                }
            };

        let append = response_mode == ResponseMode::Append;
        if !append {
            received = 0;
            hasher = Sha256::new();
        }
        let mut file = open_partial(&partial_path, append)?;
        protect_file(&partial_path)?;

        loop {
            let next_chunk = if let Some(cancellation) = cancellation.as_ref() {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        file.flush().map_err(|error| {
                            ModelError::io("flush partial model", &partial_path, error)
                        })?;
                        file.sync_all().map_err(|error| {
                            ModelError::io("sync partial model", &partial_path, error)
                        })?;
                        let message =
                            "The model download was cancelled; the partial download was preserved";
                        emit_event(
                            &progress,
                            ModelInstallEvent::new(
                                manifest,
                                ModelInstallPhase::Failed,
                                received,
                                Some(expected_size),
                                true,
                                message,
                                Some(ModelInstallStatus::Cancelled),
                            ),
                        );
                        return Err(ModelError::DownloadCancelled);
                    }
                    chunk = response.body.next() => chunk,
                }
            } else {
                response.body.next().await
            };
            let Some(chunk) = next_chunk else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(message) => {
                    let _ = file.flush();
                    let _ = file.sync_all();
                    emit_event(
                        &progress,
                        ModelInstallEvent::new(
                            manifest,
                            ModelInstallPhase::Failed,
                            received,
                            Some(expected_size),
                            true,
                            &message,
                            Some(ModelInstallStatus::Failed),
                        ),
                    );
                    return Err(ModelError::DownloadInterrupted(message));
                }
            };
            lock.refresh()?;
            let chunk_len = chunk.len() as u64;
            if received.saturating_add(chunk_len) > expected_size {
                let _ = file.flush();
                let _ = file.sync_all();
                let actual = received.saturating_add(chunk_len);
                emit_event(
                    &progress,
                    ModelInstallEvent::new(
                        manifest,
                        ModelInstallPhase::Failed,
                        received,
                        Some(expected_size),
                        true,
                        "The server returned more bytes than the manifest allows",
                        Some(ModelInstallStatus::Failed),
                    ),
                );
                return Err(ModelError::SizeMismatch {
                    expected: expected_size,
                    actual,
                });
            }

            file.write_all(&chunk)
                .map_err(|error| ModelError::io("write partial model", &partial_path, error))?;
            hasher.update(&chunk);
            received += chunk_len;
            emit_event(
                &progress,
                ModelInstallEvent::new(
                    manifest,
                    ModelInstallPhase::Downloading,
                    received,
                    Some(expected_size),
                    received > 0,
                    "Downloading the model",
                    None,
                ),
            );
        }

        file.flush()
            .map_err(|error| ModelError::io("flush partial model", &partial_path, error))?;
        file.sync_all()
            .map_err(|error| ModelError::io("sync partial model", &partial_path, error))?;

        if received != expected_size {
            emit_event(
                &progress,
                ModelInstallEvent::new(
                    manifest,
                    ModelInstallPhase::Failed,
                    received,
                    Some(expected_size),
                    true,
                    "The download ended before the manifest size was reached",
                    Some(ModelInstallStatus::Failed),
                ),
            );
            return Err(ModelError::SizeMismatch {
                expected: expected_size,
                actual: received,
            });
        }

        let actual_sha256 = format!("{:x}", hasher.finalize());
        emit_event(
            &progress,
            ModelInstallEvent::new(
                manifest,
                ModelInstallPhase::Verifying,
                received,
                Some(expected_size),
                true,
                "Verifying the model SHA-256",
                None,
            ),
        );
        if !digests_equal(&actual_sha256, expected_sha256) {
            emit_event(
                &progress,
                ModelInstallEvent::new(
                    manifest,
                    ModelInstallPhase::Failed,
                    received,
                    Some(expected_size),
                    true,
                    "The model SHA-256 did not match the manifest",
                    Some(ModelInstallStatus::Failed),
                ),
            );
            return Err(ModelError::ChecksumMismatch {
                expected: expected_sha256.to_ascii_lowercase(),
                actual: actual_sha256,
            });
        }

        self.publish_verified(
            manifest,
            &partial_path,
            &partial_state_path,
            &artifact_path,
            &metadata_path,
            received,
            &actual_sha256,
            progress,
            append && received > 0,
        )
    }

    fn prepare_partial_state(
        &self,
        manifest: &ModelManifest,
        expected_size: u64,
        expected_sha256: &str,
        partial_path: &Path,
        state_path: &Path,
    ) -> Result<PartialDownloadState, ModelError> {
        let state = match fs::read(state_path) {
            Ok(bytes) => match serde_json::from_slice::<PartialDownloadState>(&bytes) {
                Ok(state) if state.matches(manifest, expected_size, expected_sha256) => Some(state),
                Ok(_) | Err(_) => {
                    quarantine_if_present(partial_path, "stale-partial")?;
                    quarantine_if_present(state_path, "stale-partial-state")?;
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if path_is_present(partial_path)? {
                    quarantine_if_present(partial_path, "missing-partial-state")?;
                }
                None
            }
            Err(error) => return Err(ModelError::io("read partial state", state_path, error)),
        };

        let state = state.unwrap_or_else(|| {
            PartialDownloadState::from_manifest(manifest, expected_size, expected_sha256)
        });
        if !path_is_present(state_path)? {
            let encoded = serde_json::to_vec_pretty(&state)
                .map_err(|error| ModelError::serialization("partial download state", error))?;
            write_atomic(state_path, &encoded, Some(0o600))?;
        }
        Ok(state)
    }

    fn verify_manifest_files(
        &self,
        manifest: &ModelManifest,
        artifact_path: &Path,
        metadata_path: &Path,
    ) -> Result<InstalledModel, ModelError> {
        ensure_artifact_file(manifest, artifact_path)?;
        ensure_metadata_file(manifest, metadata_path)?;
        let metadata_bytes = fs::read(metadata_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ModelError::NotInstalled(manifest.id.clone())
            } else {
                ModelError::io("read installed model metadata", metadata_path, error)
            }
        })?;
        let metadata: InstallationMetadata = serde_json::from_slice(&metadata_bytes)
            .map_err(|error| ModelError::serialization("installed model metadata", error))?;
        if metadata.schema_version != MODEL_MANIFEST_SCHEMA_VERSION {
            return Err(ModelError::MetadataMismatch(format!(
                "unsupported metadata schema version {}",
                metadata.schema_version
            )));
        }
        if metadata.status != InstallationStatus::Installed {
            return Err(ModelError::MetadataMismatch(
                "the sidecar does not describe an installed artifact".to_string(),
            ));
        }
        if !same_manifest(&metadata.manifest, manifest) {
            return Err(ModelError::MetadataMismatch(
                "the sidecar manifest does not match the registered manifest".to_string(),
            ));
        }
        let expected_path = self.paths.relative_to_models(artifact_path)?;
        if metadata.artifact_path != expected_path {
            return Err(ModelError::MetadataMismatch(
                "the sidecar artifact path does not match the registered model path".to_string(),
            ));
        }
        let (expected_size, expected_sha256) = manifest.require_integrity()?;
        if metadata.verified_size != expected_size
            || !digests_equal(&metadata.verified_sha256, expected_sha256)
        {
            return Err(ModelError::MetadataMismatch(
                "the sidecar verification values do not match the manifest".to_string(),
            ));
        }
        let (actual_size, actual_sha256) = hash_file(artifact_path)?;
        if actual_size != expected_size {
            return Err(ModelError::SizeMismatch {
                expected: expected_size,
                actual: actual_size,
            });
        }
        if !digests_equal(&actual_sha256, expected_sha256) {
            return Err(ModelError::ChecksumMismatch {
                expected: expected_sha256.to_ascii_lowercase(),
                actual: actual_sha256,
            });
        }
        Ok(InstalledModel {
            manifest: manifest.clone(),
            artifact_path: artifact_path.to_path_buf(),
            metadata_path: metadata_path.to_path_buf(),
            installed_at: metadata.installed_at,
            verified_size: actual_size,
            verified_sha256: actual_sha256,
        })
    }

    fn publish_verified(
        &self,
        manifest: &ModelManifest,
        partial_path: &Path,
        partial_state_path: &Path,
        artifact_path: &Path,
        metadata_path: &Path,
        verified_size: u64,
        verified_sha256: &str,
        progress: Option<ProgressCallback>,
        resumed: bool,
    ) -> Result<InstalledModel, ModelError> {
        emit_event(
            &progress,
            ModelInstallEvent::new(
                manifest,
                ModelInstallPhase::Installing,
                verified_size,
                Some(verified_size),
                resumed,
                "Publishing the verified model atomically",
                None,
            ),
        );
        ensure_destination_absent(artifact_path)?;
        fs::rename(partial_path, artifact_path)
            .map_err(|error| ModelError::io("publish model artifact", artifact_path, error))?;
        sync_directory(artifact_path.parent().unwrap_or_else(|| Path::new(".")))?;

        let metadata = InstallationMetadata {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            status: InstallationStatus::Installed,
            manifest: manifest.clone(),
            artifact_path: self.paths.relative_to_models(artifact_path)?,
            installed_at: now_unix_seconds(),
            verified_size,
            verified_sha256: verified_sha256.to_ascii_lowercase(),
        };
        let encoded = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| ModelError::serialization("installed model metadata", error))?;
        write_atomic(metadata_path, &encoded, Some(0o600))?;
        if path_is_present(partial_state_path)? {
            fs::remove_file(partial_state_path).map_err(|error| {
                ModelError::io("remove partial state", partial_state_path, error)
            })?;
        }
        sync_directory(metadata_path.parent().unwrap_or_else(|| Path::new(".")))?;

        let installed = InstalledModel {
            manifest: manifest.clone(),
            artifact_path: artifact_path.to_path_buf(),
            metadata_path: metadata_path.to_path_buf(),
            installed_at: metadata.installed_at,
            verified_size,
            verified_sha256: verified_sha256.to_ascii_lowercase(),
        };
        emit_event(
            &progress,
            ModelInstallEvent::new(
                manifest,
                ModelInstallPhase::Complete,
                verified_size,
                Some(verified_size),
                resumed,
                "The model was installed and verified",
                Some(if resumed {
                    ModelInstallStatus::Resumed
                } else {
                    ModelInstallStatus::Downloaded
                }),
            ),
        );
        Ok(installed)
    }
}

impl ModelInstallEvent {
    fn new(
        manifest: &ModelManifest,
        phase: ModelInstallPhase,
        bytes_received: u64,
        total_bytes: Option<u64>,
        resumable: bool,
        message: &str,
        status: Option<ModelInstallStatus>,
    ) -> Self {
        let percent = total_bytes.and_then(|total| {
            if total == 0 {
                None
            } else {
                Some(bytes_received.min(total).saturating_mul(100) / total)
            }
        });
        Self {
            model_id: manifest.id.clone(),
            phase,
            bytes_received,
            total_bytes,
            percent: percent.map(|value| value as u8),
            resumable,
            message: truncate_message(message),
            status,
        }
    }
}

pub(crate) fn output_progress_sink() -> ProgressCallback {
    if crate::output::is_json_mode() {
        let last_percent = Arc::new(Mutex::new(None::<u8>));
        Arc::new(move |event| {
            let should_emit =
                if event.phase == ModelInstallPhase::Downloading && event.total_bytes.is_some() {
                    let mut last = last_percent
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if *last == event.percent {
                        false
                    } else {
                        *last = event.percent;
                        true
                    }
                } else {
                    true
                };
            if should_emit {
                crate::output::emit_named_event("ai_model_install", &event);
            }
        })
    } else {
        let progress_bar = Arc::new(Mutex::new(None::<ProgressBar>));
        Arc::new(move |event| {
            if event.phase == ModelInstallPhase::Downloading {
                let mut guard = progress_bar
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if guard.is_none() {
                    let bar = event
                        .total_bytes
                        .map(ProgressBar::new)
                        .unwrap_or_else(ProgressBar::new_spinner);
                    let style = ProgressStyle::with_template(
                        "{spinner:.green} {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {percent}%",
                    )
                    .unwrap_or_else(|_| ProgressStyle::default_bar());
                    bar.set_style(style);
                    *guard = Some(bar);
                }
                if let Some(bar) = guard.as_ref() {
                    bar.set_position(event.bytes_received);
                    bar.set_message(event.message.clone());
                }
            } else if matches!(
                event.phase,
                ModelInstallPhase::Complete | ModelInstallPhase::Failed
            ) {
                let mut guard = progress_bar
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(bar) = guard.take() {
                    if event.phase == ModelInstallPhase::Complete {
                        bar.finish_with_message(event.message);
                    } else {
                        bar.abandon_with_message(event.message);
                    }
                } else {
                    eprintln!("{}", event.message);
                }
            } else {
                eprintln!("{}", event.message);
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseMode {
    Append,
    Replace,
}

fn validate_response(
    response: &DownloadResponse,
    requested_start: u64,
    expected_size: u64,
    url: &str,
) -> Result<ResponseMode, ModelError> {
    let status = response.status;
    if requested_start > 0 && status == StatusCode::OK {
        return Ok(ResponseMode::Replace);
    }
    if status == StatusCode::PARTIAL_CONTENT {
        let header =
            response
                .content_range
                .as_deref()
                .ok_or_else(|| ModelError::InvalidContentRange {
                    header: "<missing>".to_string(),
                    expected_start: requested_start,
                })?;
        let range = parse_content_range(header).ok_or_else(|| ModelError::InvalidContentRange {
            header: header.to_string(),
            expected_start: requested_start,
        })?;
        if range.start != requested_start
            || range.end < range.start
            || range.end >= expected_size
            || range.total.is_some_and(|total| total != expected_size)
        {
            return Err(ModelError::InvalidContentRange {
                header: header.to_string(),
                expected_start: requested_start,
            });
        }
        if let Some(content_length) = response.content_length
            && content_length != range.end - range.start + 1
        {
            return Err(ModelError::InvalidContentRange {
                header: format!("{header} (content length {content_length})"),
                expected_start: requested_start,
            });
        }
        return Ok(ResponseMode::Append);
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(ModelError::RangeNotSatisfiable {
            current: requested_start,
            expected: expected_size,
        });
    }
    if status != StatusCode::OK {
        return Err(ModelError::UnexpectedStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }
    Ok(ResponseMode::Replace)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let range = value.trim().strip_prefix("bytes ")?;
    let (range, total) = range.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some(ContentRange {
        start: start.parse().ok()?,
        end: end.parse().ok()?,
        total: if total == "*" {
            None
        } else {
            Some(total.parse().ok()?)
        },
    })
}

fn digests_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.bytes().zip(right.bytes()) {
        difference |= left.to_ascii_lowercase() ^ right.to_ascii_lowercase();
    }
    difference == 0
}

fn seed_hasher(path: &Path) -> Result<(Sha256, u64), ModelError> {
    if !path_is_present(path)? {
        return Ok((Sha256::new(), 0));
    }
    let mut file =
        File::open(path).map_err(|error| ModelError::io("open partial model", path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ModelError::io("read partial model", path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((hasher, size))
}

fn open_partial(path: &Path, append: bool) -> Result<File, ModelError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options
        .open(path)
        .map_err(|error| ModelError::io("open partial model", path, error))
}

fn file_size_if_present(path: &Path) -> Result<u64, ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ModelError::UnsafePath(path.to_path_buf()))
        }
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(ModelError::io("inspect partial model", path, error)),
    }
}

fn path_is_present(path: &Path) -> Result<bool, ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ModelError::UnsafePath(path.to_path_buf()))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ModelError::io("inspect model file", path, error)),
    }
}

fn ensure_artifact_file(manifest: &ModelManifest, path: &Path) -> Result<(), ModelError> {
    if path_is_present(path)? {
        Ok(())
    } else {
        Err(ModelError::NotInstalled(manifest.id.clone()))
    }
}

fn ensure_metadata_file(manifest: &ModelManifest, path: &Path) -> Result<(), ModelError> {
    if path_is_present(path)? {
        Ok(())
    } else {
        Err(ModelError::NotInstalled(manifest.id.clone()))
    }
}

fn ensure_destination_absent(path: &Path) -> Result<(), ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ModelError::UnsafePath(path.to_path_buf()))
        }
        Ok(_) => Err(ModelError::MetadataMismatch(
            "the final model path unexpectedly exists".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ModelError::io("inspect final model path", path, error)),
    }
}

fn quarantine_if_present(path: &Path, reason: &str) -> Result<(), ModelError> {
    let _ = quarantine(path, reason)?;
    Ok(())
}

fn same_manifest(left: &ModelManifest, right: &ModelManifest) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.sha256 = left.sha256.map(|value| value.to_ascii_lowercase());
    right.sha256 = right.sha256.map(|value| value.to_ascii_lowercase());
    left.schema_version == right.schema_version
        && left.id == right.id
        && left.display_name == right.display_name
        && left.version == right.version
        && left.family == right.family
        && left.parameter_count == right.parameter_count
        && left.quantization == right.quantization
        && left.format == right.format
        && left.intended_use == right.intended_use
        && left.download_url == right.download_url
        && left.source == right.source
        && left.license == right.license
        && left.license_url == right.license_url
        && left.expected_size == right.expected_size
        && left.min_ram_bytes == right.min_ram_bytes
        && left.runtime_features == right.runtime_features
        && match (left.sha256.as_deref(), right.sha256.as_deref()) {
            (None, None) => true,
            (Some(left), Some(right)) => digests_equal(left, right),
            _ => false,
        }
}

fn protect_file(path: &Path) -> Result<(), ModelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ModelError::io("protect partial model", path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ModelError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| ModelError::io("sync model directory", path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn emit_event(progress: &Option<ProgressCallback>, event: ModelInstallEvent) {
    if let Some(progress) = progress {
        progress(event);
    }
}

fn truncate_message(message: &str) -> String {
    if message.len() <= MAX_EVENT_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = MAX_EVENT_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = message[..end].to_string();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::model::paths::validate_path_component;
    use crate::ai::model::registry::{DEFAULT_MODEL_ID, DEFAULT_MODEL_URL, ModelRegistry};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream;
    use sha2::Digest;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct FakeResponse {
        status: StatusCode,
        content_range: Option<String>,
        content_length: Option<u64>,
        chunks: Vec<Result<Bytes, String>>,
    }

    struct FakeTransport {
        responses: Mutex<Vec<FakeResponse>>,
        requests: Arc<Mutex<Vec<Option<u64>>>>,
    }

    struct SlowTransport {
        body: Arc<Vec<u8>>,
        first_chunk_sent: Arc<Notify>,
    }

    impl FakeTransport {
        fn new(responses: Vec<FakeResponse>) -> (Arc<Self>, Arc<Mutex<Vec<Option<u64>>>>) {
            let requests = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    responses: Mutex::new(responses),
                    requests: Arc::clone(&requests),
                }),
                requests,
            )
        }
    }

    #[async_trait]
    impl DownloadTransport for FakeTransport {
        async fn get(
            &self,
            _url: &str,
            range_start: Option<u64>,
        ) -> Result<DownloadResponse, ModelError> {
            self.requests.lock().unwrap().push(range_start);
            let response =
                self.responses
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| ModelError::Http {
                        url: "fake".to_string(),
                        message: "no scripted response remained".to_string(),
                    })?;
            Ok(DownloadResponse {
                status: response.status,
                content_range: response.content_range,
                content_length: response.content_length,
                body: Box::pin(stream::iter(response.chunks)),
            })
        }
    }

    #[async_trait]
    impl DownloadTransport for SlowTransport {
        async fn get(
            &self,
            _url: &str,
            _range_start: Option<u64>,
        ) -> Result<DownloadResponse, ModelError> {
            let body = Arc::clone(&self.body);
            let first_chunk_sent = Arc::clone(&self.first_chunk_sent);
            let body_length = body.len() as u64;
            let stream = stream::unfold(0_u8, move |state| {
                let body = Arc::clone(&body);
                let first_chunk_sent = Arc::clone(&first_chunk_sent);
                async move {
                    match state {
                        0 => {
                            let split = body.len() / 2;
                            first_chunk_sent.notify_one();
                            Some((Ok(Bytes::copy_from_slice(&body[..split])), 1))
                        }
                        1 => {
                            std::future::pending::<()>().await;
                            None
                        }
                        _ => None,
                    }
                }
            });
            Ok(DownloadResponse {
                status: StatusCode::OK,
                content_range: None,
                content_length: Some(body_length),
                body: Box::pin(stream),
            })
        }
    }

    fn full_response(body: &[u8]) -> FakeResponse {
        FakeResponse {
            status: StatusCode::OK,
            content_range: None,
            content_length: Some(body.len() as u64),
            chunks: vec![Ok(Bytes::copy_from_slice(body))],
        }
    }

    fn range_response(body: &[u8], start: u64) -> FakeResponse {
        FakeResponse {
            status: StatusCode::PARTIAL_CONTENT,
            content_range: Some(format!(
                "bytes {}-{}/{}",
                start,
                body.len() as u64 - 1,
                body.len()
            )),
            content_length: Some(body.len() as u64 - start),
            chunks: vec![Ok(Bytes::copy_from_slice(&body[start as usize..]))],
        }
    }

    fn model_manager_with_transport(
        manifest: ModelManifest,
        root: PathBuf,
        transport: Arc<dyn DownloadTransport>,
    ) -> ModelManager {
        let registry =
            ModelRegistry::from_models([manifest]).expect("test manifest should validate");
        ModelManager::with_transport(registry, ModelPaths::from_root(root), transport)
            .expect("test model manager should be created")
    }

    fn temporary_root(label: &str) -> PathBuf {
        static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "gib-ai-model-{label}-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temporary model root should be created");
        root
    }

    fn manifest(url: String, body: &[u8]) -> ModelManifest {
        ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            id: "test-model".to_string(),
            display_name: "Test model".to_string(),
            version: "v1".to_string(),
            family: "Test".to_string(),
            parameter_count: Some("1B".to_string()),
            quantization: "Q8_0".to_string(),
            format: "GGUF".to_string(),
            intended_use: "Tests".to_string(),
            download_url: url,
            source: "Local test server".to_string(),
            license: "Test license".to_string(),
            license_url: None,
            sha256: Some(format!("{:x}", Sha256::digest(body))),
            expected_size: Some(body.len() as u64),
            min_ram_bytes: None,
            runtime_features: vec!["text-generation".to_string()],
        }
    }

    fn manager(
        manifest: ModelManifest,
        root: PathBuf,
        responses: Vec<FakeResponse>,
    ) -> (ModelManager, Arc<Mutex<Vec<Option<u64>>>>) {
        let (transport, requests) = FakeTransport::new(responses);
        (
            model_manager_with_transport(manifest, root, transport),
            requests,
        )
    }

    fn event_collector() -> (ProgressCallback, Arc<Mutex<Vec<ModelInstallEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_callback = Arc::clone(&events);
        let callback: ProgressCallback = Arc::new(move |event| {
            events_for_callback.lock().unwrap().push(event);
        });
        (callback, events)
    }

    fn write_partial_state(manager: &ModelManager, manifest: &ModelManifest, prefix: &[u8]) {
        let partial_path = manager
            .paths()
            .partial_path(manifest)
            .expect("partial path should resolve");
        std::fs::write(&partial_path, prefix).expect("partial model should be written");
        protect_file(&partial_path).expect("partial model should be protected");
        let (expected_size, expected_sha256) = manifest
            .require_integrity()
            .expect("test manifest should have integrity");
        let state = PartialDownloadState::from_manifest(manifest, expected_size, expected_sha256);
        let state_path = manager
            .paths()
            .partial_state_path(manifest)
            .expect("partial state path should resolve");
        let encoded = serde_json::to_vec_pretty(&state).expect("partial state should serialize");
        write_atomic(&state_path, &encoded, Some(0o600)).expect("partial state should be written");
    }

    #[test]
    fn built_in_registry_uses_the_gib_url_and_published_integrity() {
        let registry = ModelRegistry::default();
        let manifest = registry
            .get(DEFAULT_MODEL_ID)
            .expect("the default model should be registered");
        assert_eq!(manifest.download_url, DEFAULT_MODEL_URL);
        assert_eq!(manifest.license, "Apache-2.0");
        assert_eq!(
            manifest.license_url.as_deref(),
            Some("https://www.apache.org/licenses/LICENSE-2.0")
        );
        assert_eq!(manifest.source, "GIB public model bucket");
        assert_eq!(
            manifest.require_integrity().unwrap(),
            (
                4_482_402_656,
                "c3fc7bcaf6f75b8f7ceeead9a769f5a7a9f86a8180af1cfb2b72958dcad8e028"
            )
        );
        assert!(matches!(
            registry.get("missing-model"),
            Err(ModelError::UnknownModel(id)) if id == "missing-model"
        ));
    }

    #[test]
    fn rejects_incomplete_or_invalid_manifest_integrity() {
        let body = b"fixture";
        let mut manifest = manifest("https://example.com/model.gguf".to_string(), body);
        manifest.expected_size = None;
        assert!(matches!(
            manifest.validate(),
            Err(ModelError::InvalidManifest(message)) if message.contains("either both")
        ));

        manifest.expected_size = Some(body.len() as u64);
        manifest.sha256 = Some("not-a-sha".to_string());
        assert!(matches!(
            manifest.validate(),
            Err(ModelError::InvalidManifest(message)) if message.contains("64 hexadecimal")
        ));
    }

    #[test]
    fn rejects_invalid_registry_versions_and_unsafe_model_identifiers() {
        let body = b"fixture";
        let manifest = manifest("https://example.com/model.gguf".to_string(), body);
        let mut registry = ModelRegistry::from_models([manifest]).unwrap();
        registry.schema_version += 1;
        assert!(matches!(
            registry.validate(),
            Err(ModelError::InvalidManifest(message)) if message.contains("registry version")
        ));
        assert!(matches!(
            validate_path_component("../outside"),
            Err(ModelError::InvalidModelId(_))
        ));
    }

    #[tokio::test]
    async fn downloads_verifies_and_publishes_metadata_atomically() {
        let body = b"GGUF test fixture payload".to_vec();
        let root = temporary_root("full");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (manager, _) = manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        let (callback, events) = event_collector();

        let installed = manager
            .ensure_installed(&manifest.id, Some(callback))
            .await
            .expect("model should install");

        assert_eq!(std::fs::read(&installed.artifact_path).unwrap(), body);
        assert!(installed.metadata_path.is_file());
        let metadata: InstallationMetadata =
            serde_json::from_slice(&std::fs::read(&installed.metadata_path).unwrap())
                .expect("installation metadata should be valid JSON");
        assert_eq!(metadata.status, InstallationStatus::Installed);
        assert_eq!(metadata.manifest, manifest);
        assert_eq!(metadata.artifact_path, "test-model/v1/test-model.gguf");
        assert_eq!(metadata.verified_size, body.len() as u64);
        assert!(digests_equal(
            &metadata.verified_sha256,
            manifest.sha256.as_deref().unwrap()
        ));
        assert_eq!(
            manager
                .verify_installed(&manifest.id)
                .expect("installed model should verify"),
            installed
        );
        let event_list = events.lock().unwrap();
        assert!(
            event_list
                .iter()
                .any(|event| event.phase == ModelInstallPhase::Downloading)
        );
        assert_eq!(
            event_list.last().and_then(|event| event.status),
            Some(ModelInstallStatus::Downloaded)
        );
        assert!(!manager.paths().partial_path(&manifest).unwrap().exists());
        assert!(
            !manager
                .paths()
                .partial_state_path(&manifest)
                .unwrap()
                .exists()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn resumes_a_matching_partial_download_with_range() {
        let body = b"0123456789abcdef".to_vec();
        let root = temporary_root("resume");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let prefix = &body[..5];
        let (manager, requests) = manager(
            manifest.clone(),
            root.clone(),
            vec![range_response(&body, prefix.len() as u64)],
        );
        write_partial_state(&manager, &manifest, prefix);

        let (callback, events) = event_collector();
        manager
            .ensure_installed(&manifest.id, Some(callback))
            .await
            .expect("partial model should resume");

        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[Some(prefix.len() as u64)]
        );
        let installed = manager
            .verify_installed(&manifest.id)
            .expect("resumed model should verify");
        assert_eq!(std::fs::read(installed.artifact_path).unwrap(), body);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.status == Some(ModelInstallStatus::Resumed))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn restarts_safely_when_server_ignores_range() {
        let body = b"server ignores range".to_vec();
        let root = temporary_root("ignore-range");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (manager, requests) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        write_partial_state(&manager, &manifest, &body[..4]);

        manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("ignored range should restart safely");

        assert_eq!(requests.lock().unwrap().as_slice(), &[Some(4)]);
        let installed = manager
            .verify_installed(&manifest.id)
            .expect("restarted model should verify");
        assert_eq!(std::fs::read(installed.artifact_path).unwrap(), body);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_content_range_without_appending() {
        let body = b"invalid content range".to_vec();
        let root = temporary_root("invalid-range");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let prefix = &body[..3];
        let (manager, _) = manager(
            manifest.clone(),
            root.clone(),
            vec![FakeResponse {
                status: StatusCode::PARTIAL_CONTENT,
                content_range: Some(format!("bytes 0-{}/{}", body.len() - 1, body.len())),
                content_length: Some(body.len() as u64),
                chunks: vec![Ok(Bytes::copy_from_slice(&body[prefix.len()..]))],
            }],
        );
        write_partial_state(&manager, &manifest, prefix);
        let partial_path = manager.paths().partial_path(&manifest).unwrap();

        let error = manager
            .ensure_installed(&manifest.id, None)
            .await
            .unwrap_err();
        assert!(matches!(error, ModelError::InvalidContentRange { .. }));
        assert_eq!(std::fs::read(partial_path).unwrap(), prefix);
        assert!(!manager.paths().artifact_path(&manifest).unwrap().exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reports_an_unsatisfied_range_without_discarding_the_partial_file() {
        let body = b"range no longer available".to_vec();
        let root = temporary_root("range-416");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let prefix = &body[..4];
        let (manager, requests) = manager(
            manifest.clone(),
            root.clone(),
            vec![FakeResponse {
                status: StatusCode::RANGE_NOT_SATISFIABLE,
                content_range: None,
                content_length: None,
                chunks: Vec::new(),
            }],
        );
        write_partial_state(&manager, &manifest, prefix);

        let error = manager
            .ensure_installed(&manifest.id, None)
            .await
            .unwrap_err();
        assert!(matches!(error, ModelError::RangeNotSatisfiable { .. }));
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[Some(prefix.len() as u64)]
        );
        assert_eq!(
            std::fs::read(manager.paths().partial_path(&manifest).unwrap()).unwrap(),
            prefix
        );
        assert!(!manager.paths().artifact_path(&manifest).unwrap().exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn preserves_a_truncated_download_for_a_later_resume() {
        let body = b"truncated response body".to_vec();
        let root = temporary_root("truncated");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (manager, requests) = manager(
            manifest.clone(),
            root.clone(),
            vec![
                range_response(&body, 2),
                FakeResponse {
                    status: StatusCode::OK,
                    content_range: None,
                    content_length: Some(body.len() as u64),
                    chunks: vec![
                        Ok(Bytes::copy_from_slice(&body[..2])),
                        Err("connection closed before the response completed".to_string()),
                    ],
                },
            ],
        );

        let error = manager
            .ensure_installed(&manifest.id, None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ModelError::DownloadInterrupted(_) | ModelError::SizeMismatch { .. }
        ));
        let partial_path = manager.paths().partial_path(&manifest).unwrap();
        assert!(std::fs::read(partial_path).unwrap().len() < body.len());
        assert!(!manager.paths().artifact_path(&manifest).unwrap().exists());
        assert!(
            manager
                .paths()
                .partial_state_path(&manifest)
                .unwrap()
                .is_file()
        );

        manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("the interrupted download should resume");
        assert_eq!(requests.lock().unwrap().as_slice(), &[None, Some(2)]);
        let installed = manager
            .verify_installed(&manifest.id)
            .expect("the resumed model should verify");
        assert_eq!(std::fs::read(installed.artifact_path).unwrap(), body);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancellation_preserves_partial_download_and_releases_the_lock() {
        let body = b"cancellable model download".to_vec();
        let root = temporary_root("cancelled");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let first_chunk_sent = Arc::new(Notify::new());
        let manager = model_manager_with_transport(
            manifest.clone(),
            root.clone(),
            Arc::new(SlowTransport {
                body: Arc::new(body.clone()),
                first_chunk_sent: Arc::clone(&first_chunk_sent),
            }) as Arc<dyn DownloadTransport>,
        );
        let cancellation = ModelInstallCancellation::new();
        let first_chunk_waiter = first_chunk_sent.notified();
        let install = tokio::spawn({
            let manager = manager.clone();
            let cancellation = cancellation.clone();
            let task_manifest = manifest.clone();
            async move {
                manager
                    .ensure_installed_with_cancellation(&task_manifest.id, None, Some(cancellation))
                    .await
            }
        });
        first_chunk_waiter.await;
        cancellation.cancel();

        let error = install
            .await
            .expect("installation task should finish")
            .expect_err("cancelled installation should fail with a resumable error");
        assert!(matches!(error, ModelError::DownloadCancelled));
        let partial_path = manager.paths().partial_path(&manifest).unwrap();
        assert!(std::fs::metadata(&partial_path).unwrap().len() > 0);
        assert!(
            manager
                .paths()
                .partial_state_path(&manifest)
                .unwrap()
                .is_file()
        );
        assert!(
            !manager
                .paths()
                .model_lock_path(&manifest.id)
                .unwrap()
                .exists()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_wrong_checksum_without_publishing_a_final_artifact() {
        let body = b"checksum mismatch".to_vec();
        let root = temporary_root("checksum");
        let mut manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        manifest.sha256 = Some("0".repeat(64));
        let (manager, _) = manager(manifest.clone(), root.clone(), vec![full_response(&body)]);

        let error = manager
            .ensure_installed(&manifest.id, None)
            .await
            .unwrap_err();
        assert!(matches!(error, ModelError::ChecksumMismatch { .. }));
        assert!(!manager.paths().artifact_path(&manifest).unwrap().exists());
        assert!(manager.paths().partial_path(&manifest).unwrap().is_file());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_wrong_size_without_publishing_a_final_artifact() {
        let body = b"size mismatch".to_vec();
        let root = temporary_root("size");
        let mut manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        manifest.expected_size = Some(body.len() as u64 + 1);
        let (manager, _) = manager(manifest.clone(), root.clone(), vec![full_response(&body)]);

        let error = manager
            .ensure_installed(&manifest.id, None)
            .await
            .unwrap_err();
        assert!(matches!(error, ModelError::SizeMismatch { .. }));
        assert!(!manager.paths().artifact_path(&manifest).unwrap().exists());
        assert_eq!(
            std::fs::metadata(manager.paths().partial_path(&manifest).unwrap())
                .unwrap()
                .len(),
            body.len() as u64
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn serializes_concurrent_installers_with_one_download() {
        let body = vec![b'x'; 128];
        let root = temporary_root("concurrent");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (manager, requests) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        let first = manager.clone();
        let second = manager.clone();

        let (first_result, second_result) = tokio::join!(
            first.ensure_installed(&manifest.id, None),
            second.ensure_installed(&manifest.id, None)
        );
        assert!(first_result.is_ok());
        assert!(second_result.is_ok());
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(
            first_result.unwrap().artifact_path,
            second_result.unwrap().artifact_path
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reinstalls_after_a_missing_metadata_sidecar_and_quarantines_the_artifact() {
        let body = b"sidecar recovery".to_vec();
        let root = temporary_root("missing-sidecar");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (first_manager, _) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        let installed = first_manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("the initial model should install");
        let artifact_parent = installed
            .artifact_path
            .parent()
            .expect("the artifact should have a parent")
            .to_path_buf();
        std::fs::remove_file(&installed.metadata_path).unwrap();

        let (second_manager, requests) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        second_manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("the missing sidecar should be recovered");
        assert_eq!(requests.lock().unwrap().as_slice(), &[None]);
        assert!(second_manager.verify_installed(&manifest.id).is_ok());
        assert!(
            std::fs::read_dir(artifact_parent)
                .unwrap()
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("invalid-artifact"))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reinstalls_when_the_metadata_sidecar_references_a_different_url() {
        let body = b"metadata URL recovery".to_vec();
        let root = temporary_root("metadata-url");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (first_manager, _) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        let installed = first_manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("the initial model should install");
        let mut metadata: InstallationMetadata =
            serde_json::from_slice(&std::fs::read(&installed.metadata_path).unwrap()).unwrap();
        metadata.manifest.download_url = "https://different.example/model.gguf".to_string();
        write_atomic(
            &installed.metadata_path,
            &serde_json::to_vec_pretty(&metadata).unwrap(),
            Some(0o600),
        )
        .unwrap();

        let (second_manager, requests) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        second_manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("the changed sidecar should be recovered");
        assert_eq!(requests.lock().unwrap().as_slice(), &[None]);
        assert!(second_manager.verify_installed(&manifest.id).is_ok());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn keeps_previous_versions_when_a_manifest_version_changes() {
        let first_body = b"version one".to_vec();
        let second_body = b"version two".to_vec();
        let root = temporary_root("versions");
        let first_manifest = manifest("https://example.com/model-v1.gguf".to_string(), &first_body);
        let (first_manager, _) = manager(
            first_manifest.clone(),
            root.clone(),
            vec![full_response(&first_body)],
        );
        let first_installed = first_manager
            .ensure_installed(&first_manifest.id, None)
            .await
            .expect("the first model version should install");

        let mut second_manifest = manifest(
            "https://example.com/model-v2.gguf".to_string(),
            &second_body,
        );
        second_manifest.version = "v2".to_string();
        let (second_manager, _) = manager(
            second_manifest.clone(),
            root.clone(),
            vec![full_response(&second_body)],
        );
        let second_installed = second_manager
            .ensure_installed(&second_manifest.id, None)
            .await
            .expect("the second model version should install");

        assert_ne!(
            first_installed.artifact_path,
            second_installed.artifact_path
        );
        assert_eq!(
            std::fs::read(first_installed.artifact_path).unwrap(),
            first_body
        );
        assert_eq!(
            std::fs::read(second_installed.artifact_path).unwrap(),
            second_body
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn replaces_stale_partial_state_without_appending_untrusted_bytes() {
        let body = b"fresh bytes".to_vec();
        let root = temporary_root("stale-state");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (manager, requests) =
            manager(manifest.clone(), root.clone(), vec![full_response(&body)]);
        let partial_path = manager.paths().partial_path(&manifest).unwrap();
        let state_path = manager.paths().partial_state_path(&manifest).unwrap();
        std::fs::write(&partial_path, b"untrusted prefix").unwrap();
        let stale = PartialDownloadState {
            schema_version: PARTIAL_STATE_VERSION,
            model_id: manifest.id.clone(),
            manifest_version: manifest.version.clone(),
            download_url: "https://different.example/model.gguf".to_string(),
            expected_size: body.len() as u64,
            sha256: manifest.sha256.clone().unwrap(),
        };
        write_atomic(
            &state_path,
            &serde_json::to_vec(&stale).unwrap(),
            Some(0o600),
        )
        .unwrap();

        manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("stale partial should be replaced");
        let installed = manager.verify_installed(&manifest.id).unwrap();
        assert_eq!(std::fs::read(installed.artifact_path).unwrap(), body);
        assert_eq!(requests.lock().unwrap().as_slice(), &[None]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn persists_and_validates_the_active_model_selection() {
        let body = b"active model".to_vec();
        let root = temporary_root("active");
        let manifest = manifest("https://example.com/model.gguf".to_string(), &body);
        let (manager, _) = manager(manifest.clone(), root.clone(), vec![full_response(&body)]);

        manager
            .ensure_installed(&manifest.id, None)
            .await
            .expect("model should install");
        manager
            .set_active_model(&manifest.id)
            .await
            .expect("active model should be persisted");

        assert_eq!(
            manager.active_model_id().unwrap(),
            Some(manifest.id.clone())
        );
        let config = AiConfigStore::new(ModelPaths::from_root(root.clone()))
            .load()
            .expect("AI config should load");
        assert_eq!(config.model.active, Some(manifest.id));
        assert_eq!(config.version, super::super::config::AI_CONFIG_VERSION);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn protects_global_model_store_and_artifact_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_root("permissions");
        let paths = ModelPaths::from_root(root.clone());
        paths
            .ensure_root()
            .expect("the model root should be created");
        let manifest = manifest("https://example.com/model.gguf".to_string(), b"permissions");
        let artifact_path = paths.artifact_path(&manifest).unwrap();
        std::fs::write(&artifact_path, b"permissions").unwrap();
        protect_file(&artifact_path).unwrap();
        write_atomic(paths.config_path(), b"version = 1\n", Some(0o600)).unwrap();

        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(paths.models_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&artifact_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(paths.config_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn serializes_progress_events_without_terminal_control_sequences() {
        let body = b"event";
        let manifest = manifest("https://example.com/model.gguf".to_string(), body);
        let event = ModelInstallEvent::new(
            &manifest,
            ModelInstallPhase::Downloading,
            3,
            Some(5),
            true,
            "Downloading",
            None,
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(!encoded.contains('\u{1b}'));
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["model_id"], "test-model");
        assert_eq!(value["percent"], 60);
    }

    #[test]
    fn parses_content_ranges_strictly() {
        assert_eq!(
            parse_content_range("bytes 5-9/10"),
            Some(ContentRange {
                start: 5,
                end: 9,
                total: Some(10)
            })
        );
        assert_eq!(
            parse_content_range("bytes 5-9/*"),
            Some(ContentRange {
                start: 5,
                end: 9,
                total: None
            })
        );
        assert!(parse_content_range("items 5-9/10").is_none());
        assert!(parse_content_range("bytes 9-5/10").is_some());
    }
}
