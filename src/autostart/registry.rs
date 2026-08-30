use crate::autostart::model::{AUTOSTART_JOB_VERSION, AutostartJob, validate_job_id};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct RegistryPaths {
    pub(crate) root: PathBuf,
    pub(crate) jobs: PathBuf,
    pub(crate) logs: PathBuf,
    pub(crate) platform: PathBuf,
}

pub(crate) fn registry_paths() -> Result<RegistryPaths, String> {
    let root = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| "Failed to determine the user data directory".to_string())?
        .join("gib")
        .join("autostart");
    Ok(RegistryPaths {
        jobs: root.join("jobs"),
        logs: root.join("logs"),
        platform: root.join("platform"),
        root,
    })
}

pub(crate) fn ensure_registry(paths: &RegistryPaths) -> Result<(), String> {
    for directory in [&paths.root, &paths.jobs, &paths.logs, &paths.platform] {
        fs::create_dir_all(directory).map_err(|error| {
            format!(
                "Failed to create autostart directory '{}': {}",
                directory.display(),
                error
            )
        })?;
    }
    Ok(())
}

pub(crate) fn log_path(paths: &RegistryPaths, job_id: &str) -> Result<PathBuf, String> {
    validate_job_id(job_id)?;
    Ok(paths.logs.join(format!("{}.jsonl", job_id)))
}

#[allow(dead_code)]
pub(crate) fn platform_path(paths: &RegistryPaths, job_id: &str) -> Result<PathBuf, String> {
    validate_job_id(job_id)?;
    Ok(paths.platform.join(format!("{}.xml", job_id)))
}

pub(crate) fn job_path(paths: &RegistryPaths, job_id: &str) -> Result<PathBuf, String> {
    validate_job_id(job_id)?;
    Ok(paths.jobs.join(format!("{}.toml", job_id)))
}

pub(crate) fn read_job(paths: &RegistryPaths, job_id: &str) -> Result<AutostartJob, String> {
    let path = job_path(paths, job_id)?;
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read autostart job '{}': {}", job_id, error))?;
    let job: AutostartJob = toml::from_str(&contents)
        .map_err(|error| format!("Failed to parse autostart job '{}': {}", job_id, error))?;
    validate_job(&job)?;
    Ok(job)
}

pub(crate) fn list_jobs(paths: &RegistryPaths) -> Result<Vec<AutostartJob>, String> {
    if !paths.jobs.exists() {
        return Ok(Vec::new());
    }

    let mut jobs = Vec::new();
    for entry in fs::read_dir(&paths.jobs)
        .map_err(|error| format!("Failed to read autostart jobs: {}", error))?
    {
        let entry = entry.map_err(|error| format!("Failed to read autostart entry: {}", error))?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let contents = fs::read_to_string(entry.path())
            .map_err(|error| format!("Failed to read autostart job: {}", error))?;
        let job: AutostartJob = toml::from_str(&contents)
            .map_err(|error| format!("Failed to parse autostart job: {}", error))?;
        validate_job(&job)?;
        jobs.push(job);
    }
    jobs.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(jobs)
}

pub(crate) fn find_job_by_name(
    paths: &RegistryPaths,
    name: &str,
) -> Result<Option<AutostartJob>, String> {
    Ok(list_jobs(paths)?.into_iter().find(|job| job.name == name))
}

pub(crate) fn write_job(paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    ensure_registry(paths)?;
    validate_job(job)?;
    let path = job_path(paths, &job.id)?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("job.toml"),
        std::process::id()
    ));
    let encoded = toml::to_string_pretty(job)
        .map_err(|error| format!("Failed to serialize autostart job: {}", error))?;
    fs::write(&temporary, encoded)
        .map_err(|error| format!("Failed to write autostart job: {}", error))?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|error| format!("Failed to replace autostart job: {}", error))?;
    }
    fs::rename(&temporary, &path)
        .map_err(|error| format!("Failed to publish autostart job: {}", error))
}

pub(crate) fn remove_job(paths: &RegistryPaths, job_id: &str) -> Result<(), String> {
    let path = job_path(paths, job_id)?;
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|error| format!("Failed to remove autostart job '{}': {}", job_id, error))?;
    }
    Ok(())
}

pub(crate) fn generate_job_id(name: &str, root: &Path) -> String {
    let identity = format!(
        "{}\n{}\n{}",
        name,
        root.display(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let digest = Sha256::digest(identity.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("job-{suffix}")
}

pub(crate) fn validate_job(job: &AutostartJob) -> Result<(), String> {
    if job.version != AUTOSTART_JOB_VERSION {
        return Err(format!(
            "Unsupported autostart job version {} for '{}'",
            job.version, job.id
        ));
    }
    validate_job_id(&job.id)?;
    crate::autostart::model::validate_name(&job.name)?;
    if job.root_path.trim().is_empty() {
        return Err(format!("Autostart job '{}' has an empty root path", job.id));
    }
    if job.overrides.conflict != "local" && job.overrides.conflict != "remote" {
        return Err(format!(
            "Autostart job '{}' has an invalid conflict policy",
            job.id
        ));
    }
    Ok(())
}

pub(crate) fn touch_updated_at(job: &mut AutostartJob) {
    job.updated_at = Utc::now().to_rfc3339();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::model::{LiveJobOverrides, SecretReferences};

    fn paths() -> RegistryPaths {
        let root = std::env::temp_dir().join(format!(
            "gib-autostart-registry-test-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        RegistryPaths {
            jobs: root.join("jobs"),
            logs: root.join("logs"),
            platform: root.join("platform"),
            root,
        }
    }

    #[test]
    fn writes_and_reads_one_job_atomically() {
        let paths = paths();
        let job = AutostartJob {
            version: AUTOSTART_JOB_VERSION,
            id: "job-test".to_string(),
            name: "test".to_string(),
            enabled: true,
            root_path: "/tmp/project".to_string(),
            config_path: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            overrides: LiveJobOverrides::default(),
            secrets: SecretReferences::default(),
        };
        write_job(&paths, &job).unwrap();
        assert_eq!(read_job(&paths, "job-test").unwrap().name, "test");
        fs::remove_dir_all(paths.root).unwrap();
    }
}
