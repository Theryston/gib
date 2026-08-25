use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct LiveState {
    pub(crate) version: u32,
    pub(crate) initialized: bool,
    pub(crate) base_backup: Option<String>,
}

fn state_directory() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("gib")
        .join("live-state")
}

fn state_file_name(root: &Path, storage: &str, key: &str) -> String {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let identity = format!("{}\n{}\n{}", root.display(), storage, key);
    let digest = Sha256::digest(identity.as_bytes());
    format!("{:x}.msgpack", digest)
}

fn state_path(root: &Path, storage: &str, key: &str) -> PathBuf {
    state_directory().join(state_file_name(root, storage, key))
}

fn fallback_state_path(root: &Path, storage: &str, key: &str) -> PathBuf {
    std::env::temp_dir()
        .join("gib")
        .join("live-state")
        .join(state_file_name(root, storage, key))
}

pub(crate) fn load_live_state(root: &Path, storage: &str, key: &str) -> Result<LiveState, String> {
    let primary_path = state_path(root, storage, key);
    let path = if primary_path.exists() {
        primary_path
    } else {
        fallback_state_path(root, storage, key)
    };
    if !path.exists() {
        return Ok(LiveState {
            version: STATE_VERSION,
            ..LiveState::default()
        });
    }

    let bytes = std::fs::read(&path)
        .map_err(|error| format!("Failed to read live state '{}': {}", path.display(), error))?;
    let mut state: LiveState = rmp_serde::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse live state '{}': {}", path.display(), error))?;

    if state.version == 0 {
        state.version = STATE_VERSION;
    }
    if state.version != STATE_VERSION {
        return Err(format!(
            "Unsupported live state version {} in '{}'",
            state.version,
            path.display()
        ));
    }

    Ok(state)
}

pub(crate) fn save_live_state(
    root: &Path,
    storage: &str,
    key: &str,
    state: &LiveState,
) -> Result<(), String> {
    let bytes = rmp_serde::to_vec_named(state)
        .map_err(|error| format!("Failed to serialize live state: {}", error))?;
    let primary_path = state_path(root, storage, key);
    let fallback_path = fallback_state_path(root, storage, key);

    for path in [primary_path, fallback_path] {
        let parent = path
            .parent()
            .ok_or_else(|| "Live state path has no parent directory".to_string())?;
        if std::fs::create_dir_all(parent).is_ok() {
            if std::fs::write(&path, &bytes).is_ok() {
                return Ok(());
            }
        }
    }

    Err("Failed to write live state in the application or temporary state directory".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip_contains_only_the_base_reference() {
        let state = LiveState {
            version: STATE_VERSION,
            initialized: true,
            base_backup: Some("abc123".to_string()),
        };
        let bytes = rmp_serde::to_vec_named(&state).unwrap();
        let decoded: LiveState = rmp_serde::from_slice(&bytes).unwrap();

        assert_eq!(decoded.version, STATE_VERSION);
        assert!(decoded.initialized);
        assert_eq!(decoded.base_backup.as_deref(), Some("abc123"));
    }
}
