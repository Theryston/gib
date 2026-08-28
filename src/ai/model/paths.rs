use super::error::ModelError;
use super::registry::ModelManifest;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ModelPaths {
    root: PathBuf,
    models: PathBuf,
    config: PathBuf,
}

impl ModelPaths {
    pub(crate) fn default() -> Result<Self, ModelError> {
        let home = dirs::home_dir().ok_or(ModelError::MissingHomeDirectory)?;
        Ok(Self::from_root(home.join(".gib").join("ai")))
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            models: root.join("models"),
            config: root.join("config.toml"),
            root,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn models_dir(&self) -> &Path {
        &self.models
    }

    pub(crate) fn config_path(&self) -> &Path {
        &self.config
    }

    pub(crate) fn ensure_root(&self) -> Result<(), ModelError> {
        ensure_directory(&self.root, 0o700)?;
        ensure_directory(&self.models, 0o700)?;
        Ok(())
    }

    pub(crate) fn ensure_model_directory(
        &self,
        manifest: &ModelManifest,
    ) -> Result<PathBuf, ModelError> {
        validate_path_component(&manifest.id)?;
        validate_path_component(&manifest.version)?;
        self.ensure_root()?;
        let model_directory = self.models.join(&manifest.id).join(&manifest.version);
        ensure_directory(&self.models.join(&manifest.id), 0o700)?;
        ensure_directory(&model_directory, 0o700)?;
        Ok(model_directory)
    }

    pub(crate) fn model_lock_path(&self, model_id: &str) -> Result<PathBuf, ModelError> {
        validate_path_component(model_id)?;
        self.ensure_root()?;
        Ok(self.models.join(format!(".{model_id}.install.lock")))
    }

    pub(crate) fn artifact_path(&self, manifest: &ModelManifest) -> Result<PathBuf, ModelError> {
        let directory = self.ensure_model_directory(manifest)?;
        let path = directory.join(format!("{}.gguf", manifest.id));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    pub(crate) fn metadata_path(&self, manifest: &ModelManifest) -> Result<PathBuf, ModelError> {
        let directory = self.ensure_model_directory(manifest)?;
        let path = directory.join(format!("{}.metadata.json", manifest.id));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    pub(crate) fn partial_path(&self, manifest: &ModelManifest) -> Result<PathBuf, ModelError> {
        let directory = self.ensure_model_directory(manifest)?;
        let path = directory.join(format!("{}.gguf.part", manifest.id));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    pub(crate) fn partial_state_path(
        &self,
        manifest: &ModelManifest,
    ) -> Result<PathBuf, ModelError> {
        let directory = self.ensure_model_directory(manifest)?;
        let path = directory.join(format!("{}.gguf.part.json", manifest.id));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    pub(crate) fn relative_to_models(&self, path: &Path) -> Result<String, ModelError> {
        path.strip_prefix(&self.models)
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .map_err(|_| ModelError::UnsafePath(path.to_path_buf()))
    }
}

pub(crate) fn validate_path_component(value: &str) -> Result<(), ModelError> {
    if value.is_empty()
        || value.len() > 96
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
    {
        return Err(ModelError::InvalidModelId(value.to_string()));
    }
    Ok(())
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ModelError::UnsafePath(path.to_path_buf()))
        }
        Ok(metadata) if !metadata.is_dir() => Err(ModelError::UnsafePath(path.to_path_buf())),
        Ok(_) => {
            set_directory_mode(path, mode)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| ModelError::io("create directory", path, error))?;
            set_directory_mode(path, mode)?;
            Ok(())
        }
        Err(error) => Err(ModelError::io("inspect directory", path, error)),
    }
}

pub(crate) fn ensure_regular_or_missing(path: &Path) -> Result<(), ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ModelError::UnsafePath(path.to_path_buf()))
        }
        Ok(metadata) if !metadata.is_file() => Err(ModelError::UnsafePath(path.to_path_buf())),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ModelError::io("inspect file", path, error)),
    }
}

fn set_directory_mode(path: &Path, mode: u32) -> Result<(), ModelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| ModelError::io("protect directory", path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
