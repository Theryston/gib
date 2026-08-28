use super::error::ConversationError;
use std::fs;
use std::path::{Path, PathBuf};

const CONVERSATIONS_DIRECTORY_NAME: &str = "conversations";
const CONFIG_FILE_NAME: &str = "config.toml";
const CONFIG_LOCK_FILE_NAME: &str = ".config.lock";
const CREATION_LOCK_FILE_NAME: &str = ".creation.lock";

/// User-level paths for AI state. This is intentionally independent of a
/// project checkout or repository configuration.
#[derive(Debug, Clone)]
pub(crate) struct ConversationPaths {
    root: PathBuf,
    conversations: PathBuf,
    config: PathBuf,
}

impl ConversationPaths {
    pub(crate) fn default() -> Result<Self, ConversationError> {
        let home = dirs::home_dir().ok_or(ConversationError::MissingHomeDirectory)?;
        Ok(Self::from_root(home.join(".gib").join("ai")))
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            conversations: root.join(CONVERSATIONS_DIRECTORY_NAME),
            config: root.join(CONFIG_FILE_NAME),
            root,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn conversations_dir(&self) -> &Path {
        &self.conversations
    }

    pub(crate) fn config_path(&self) -> &Path {
        &self.config
    }

    pub(crate) fn config_lock_path(&self) -> PathBuf {
        self.root.join(CONFIG_LOCK_FILE_NAME)
    }

    pub(crate) fn creation_lock_path(&self) -> PathBuf {
        self.conversations.join(CREATION_LOCK_FILE_NAME)
    }

    pub(crate) fn ensure_root(&self) -> Result<(), ConversationError> {
        ensure_directory(&self.root, 0o700)?;
        ensure_directory(&self.conversations, 0o700)?;
        Ok(())
    }

    pub(crate) fn conversation_path(
        &self,
        conversation_id: &str,
    ) -> Result<PathBuf, ConversationError> {
        validate_conversation_id(conversation_id)?;
        self.ensure_root()?;
        let path = self.conversations.join(format!("{}.json", conversation_id));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    pub(crate) fn conversation_lock_path(
        &self,
        conversation_id: &str,
    ) -> Result<PathBuf, ConversationError> {
        validate_conversation_id(conversation_id)?;
        self.ensure_root()?;
        Ok(self
            .conversations
            .join(format!(".{}.lock", conversation_id)))
    }
}

pub(crate) fn validate_conversation_id(value: &str) -> Result<(), ConversationError> {
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
        return Err(ConversationError::InvalidConversationId);
    }
    Ok(())
}

pub(crate) fn ensure_regular_or_missing(path: &Path) -> Result<(), ConversationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ConversationError::UnsafePath),
        Ok(metadata) if !metadata.is_file() => Err(ConversationError::UnsafePath),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ConversationError::io("inspect conversation file")),
    }
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), ConversationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ConversationError::UnsafePath),
        Ok(metadata) if !metadata.is_dir() => Err(ConversationError::UnsafePath),
        Ok(_) => {
            set_directory_mode(path, mode)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|_| ConversationError::io("create directory"))?;
            set_directory_mode(path, mode)?;
            Ok(())
        }
        Err(_) => Err(ConversationError::io("inspect directory")),
    }
}

fn set_directory_mode(path: &Path, mode: u32) -> Result<(), ConversationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|_| ConversationError::io("protect directory"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
