#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(crate) fn get_file_permissions_with_path(metadata: &std::fs::Metadata, _path: &str) -> u32 {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o777
    }

    #[cfg(not(unix))]
    {
        let is_executable = Path::new(_path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| {
                matches!(
                    ext.to_lowercase().as_str(),
                    "exe" | "bat" | "cmd" | "com" | "msi" | "ps1"
                )
            })
            .unwrap_or(false);

        if metadata.permissions().readonly() {
            if is_executable { 0o555 } else { 0o444 }
        } else {
            if is_executable { 0o755 } else { 0o644 }
        }
    }
}
