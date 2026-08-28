#![allow(dead_code)]

mod config;
mod error;
mod installer;
mod lock;
mod paths;
mod registry;
mod storage;

#[allow(unused_imports)]
pub(crate) use config::{AiConfig, AiConfigStore};
#[allow(unused_imports)]
pub(crate) use error::ModelError;
#[allow(unused_imports)]
pub(crate) use installer::{
    InstallationMetadata, InstallationStatus, InstalledModel, ModelInstallEvent, ModelInstallPhase,
    ModelInstallStatus, ModelManager, ProgressCallback, output_progress_sink,
};
#[allow(unused_imports)]
pub(crate) use lock::ModelInstallCancellation;
#[allow(unused_imports)]
pub(crate) use paths::ModelPaths;
#[allow(unused_imports)]
pub(crate) use registry::{
    DEFAULT_MODEL_ID, DEFAULT_MODEL_URL, MODEL_MANIFEST_SCHEMA_VERSION, MODEL_REGISTRY_VERSION,
    ModelManifest, ModelRegistry,
};
