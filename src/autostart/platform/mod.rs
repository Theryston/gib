use crate::autostart::model::AutostartJob;
use crate::autostart::registry::RegistryPaths;
use std::path::{Path, PathBuf};

mod linux;
#[allow(dead_code)]
mod macos;
#[allow(dead_code)]
mod windows;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlatformStatus {
    pub(crate) platform: &'static str,
    pub(crate) enabled: bool,
    pub(crate) running: Option<bool>,
}

use serde::Serialize;

pub(crate) fn platform_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        return "linux";
    }
    #[cfg(target_os = "macos")]
    {
        return "macos";
    }
    #[cfg(target_os = "windows")]
    {
        return "windows";
    }
    #[allow(unreachable_code)]
    "unsupported"
}

pub(crate) fn enable(
    paths: &RegistryPaths,
    job: &AutostartJob,
    executable: &Path,
    start_now: bool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux::enable(paths, job, executable, start_now)
    }
    #[cfg(target_os = "macos")]
    {
        macos::enable(paths, job, executable, start_now)
    }
    #[cfg(target_os = "windows")]
    {
        windows::enable(paths, job, executable, start_now)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (paths, job, executable, start_now);
        Err("Autostart is not supported on this operating system".to_string())
    }
}

pub(crate) fn disable(paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux::disable(paths, job)
    }
    #[cfg(target_os = "macos")]
    {
        macos::disable(paths, job)
    }
    #[cfg(target_os = "windows")]
    {
        windows::disable(paths, job)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (paths, job);
        Err("Autostart is not supported on this operating system".to_string())
    }
}

pub(crate) fn remove(paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux::remove(paths, job)
    }
    #[cfg(target_os = "macos")]
    {
        macos::remove(paths, job)
    }
    #[cfg(target_os = "windows")]
    {
        windows::remove(paths, job)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (paths, job);
        Err("Autostart is not supported on this operating system".to_string())
    }
}

pub(crate) fn status(paths: &RegistryPaths, job: &AutostartJob) -> PlatformStatus {
    #[cfg(target_os = "linux")]
    {
        return linux::status(paths, job);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::status(paths, job);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::status(paths, job);
    }
    #[allow(unreachable_code)]
    PlatformStatus {
        platform: platform_name(),
        enabled: false,
        running: None,
    }
}

pub(crate) fn write_artifact(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Platform artifact path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create platform artifact directory: {}", error))?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        std::process::id()
    ));
    std::fs::write(&temporary, contents)
        .map_err(|error| format!("Failed to write platform artifact: {}", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to protect platform artifact: {}", error))?;
    }
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| format!("Failed to replace platform artifact: {}", error))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("Failed to publish platform artifact: {}", error))
}

pub(crate) fn remove_artifact(path: &Path) -> Result<(), String> {
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| format!("Failed to remove platform artifact: {}", error))?;
    }
    Ok(())
}

pub(crate) fn executable_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| {
        format!(
            "Failed to determine the absolute GIB executable path: {}",
            error
        )
    })
}
