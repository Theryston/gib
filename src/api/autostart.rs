use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{AutostartEvent, GibEvent};
use super::live::{ConflictPolicy, LiveHandle, LiveRequest};
use super::repository::RepositoryRequest;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;

const JOB_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_patterns: Option<Vec<String>>,
    #[serde(default)]
    pub include_git: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    #[serde(default = "default_conflict")]
    pub conflict: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartJob {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub root_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub overrides: AutostartOverrides,
    #[serde(default, skip_serializing)]
    password_ref: Option<String>,
}

impl fmt::Debug for AutostartJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AutostartJob")
            .field("version", &self.version)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("root_path", &self.root_path)
            .field("config_path", &self.config_path)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("overrides", &self.overrides)
            .field(
                "password_ref",
                &self.password_ref.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl AutostartJob {
    /// Returns the non-secret credential-store reference, if one is recorded.
    /// The referenced password is never part of this value.
    pub fn password_reference(&self) -> Option<&str> {
        self.password_ref.as_deref()
    }
}

#[derive(Clone)]
pub struct AddAutostartRequest {
    pub name: String,
    pub root_path: PathBuf,
    pub config_path: Option<PathBuf>,
    pub repository: Option<RepositoryRequest>,
    pub message: Option<String>,
    pub compression: Option<i32>,
    pub chunk_size: Option<u64>,
    pub ignore_patterns: Option<Vec<String>>,
    pub include_git: bool,
    pub concurrency: Option<usize>,
    pub conflict: ConflictPolicy,
    pub password: Option<String>,
    pub start_now: bool,
}

impl fmt::Debug for AddAutostartRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddAutostartRequest")
            .field("name", &self.name)
            .field("root_path", &self.root_path)
            .field("repository", &self.repository)
            .field("message", &self.message)
            .field("compression", &self.compression)
            .field("chunk_size", &self.chunk_size)
            .field("ignore_patterns", &self.ignore_patterns)
            .field("include_git", &self.include_git)
            .field("concurrency", &self.concurrency)
            .field("conflict", &self.conflict)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("start_now", &self.start_now)
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct UpdateAutostartRequest {
    pub name: String,
    pub root_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub repository: Option<RepositoryRequest>,
    pub message: Option<String>,
    pub compression: Option<i32>,
    pub chunk_size: Option<u64>,
    pub ignore_patterns: Option<Vec<String>>,
    pub include_git: Option<bool>,
    pub concurrency: Option<usize>,
    pub conflict: Option<ConflictPolicy>,
    pub password: Option<String>,
    pub start_now: Option<bool>,
}

impl fmt::Debug for UpdateAutostartRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpdateAutostartRequest")
            .field("name", &self.name)
            .field("root_path", &self.root_path)
            .field("config_path", &self.config_path)
            .field("repository", &self.repository)
            .field("message", &self.message)
            .field("compression", &self.compression)
            .field("chunk_size", &self.chunk_size)
            .field("ignore_patterns", &self.ignore_patterns)
            .field("include_git", &self.include_git)
            .field("concurrency", &self.concurrency)
            .field("conflict", &self.conflict)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("start_now", &self.start_now)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutostartStatus {
    pub job: AutostartJob,
    pub platform_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutostartChange {
    pub job: AutostartJob,
    pub action: String,
}

pub struct AutostartRun {
    pub job: AutostartJob,
    pub handle: LiveHandle,
}

impl fmt::Debug for AutostartRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AutostartRun")
            .field("job", &self.job)
            .finish()
    }
}

impl Gib {
    pub fn list_autostart_jobs(&self) -> Result<Vec<AutostartJob>, GibError> {
        let paths = RegistryPaths::new(self)?;
        ensure_registry(&paths)?;
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&paths.jobs)
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?
        {
            let entry = entry.map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let job = read_job_file(&entry.path())?;
            jobs.push(job);
        }
        jobs.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(jobs)
    }

    pub fn add_autostart(&self, request: AddAutostartRequest) -> Result<AutostartChange, GibError> {
        validate_job_name(&request.name)?;
        let root_path = super::client::path_from_context(&self.inner.context, &request.root_path);
        if !root_path.is_dir() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "Autostart root path is not a directory",
            ));
        }
        let paths = RegistryPaths::new(self)?;
        ensure_registry(&paths)?;
        if self
            .list_autostart_jobs()?
            .iter()
            .any(|job| job.name == request.name)
        {
            return Err(GibError::new(
                ErrorCode::RepositoryConflict,
                format!("Autostart job '{}' already exists", request.name),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let id = generate_job_id(&request.name, &root_path);
        let repository = request.repository.clone().ok_or_else(|| {
            GibError::new(
                ErrorCode::InvalidRequest,
                "Autostart repository key and storage are required",
            )
        })?;
        repository.validate()?;
        let password = request
            .password
            .clone()
            .or_else(|| repository.password.clone());
        let job = AutostartJob {
            version: JOB_VERSION,
            id: id.clone(),
            name: request.name,
            enabled: true,
            root_path,
            config_path: request
                .config_path
                .as_deref()
                .map(|path| super::client::path_from_context(&self.inner.context, path)),
            created_at: now.clone(),
            updated_at: now,
            overrides: AutostartOverrides {
                storage: Some(repository.storage),
                key: Some(repository.key),
                message: request.message,
                compression: request.compression,
                chunk_size: request.chunk_size,
                ignore_patterns: request.ignore_patterns,
                include_git: request.include_git,
                concurrency: request.concurrency,
                conflict: conflict_name(request.conflict).to_string(),
            },
            password_ref: password
                .as_ref()
                .map(|_| format!("gib/autostart/{id}/password")),
        };
        write_job(&paths, &job)?;
        if let Some(password) = password {
            write_secret(&paths, &id, &password)?;
        }
        emit_event(self, "registered", Some(&job));
        Ok(AutostartChange {
            job,
            action: if request.start_now {
                "registered".to_string()
            } else {
                "registered_without_start".to_string()
            },
        })
    }

    pub fn update_autostart(
        &self,
        request: UpdateAutostartRequest,
    ) -> Result<AutostartChange, GibError> {
        let paths = RegistryPaths::new(self)?;
        let mut job = find_job(&paths, &request.name)?;
        if let Some(root) = request.root_path {
            let root = super::client::path_from_context(&self.inner.context, &root);
            if !root.is_dir() {
                return Err(GibError::new(
                    ErrorCode::InvalidRequest,
                    "Autostart root path is not a directory",
                ));
            }
            job.root_path = root;
        }
        if let Some(config_path) = request.config_path {
            job.config_path = Some(super::client::path_from_context(
                &self.inner.context,
                &config_path,
            ));
        }
        if let Some(repository) = request.repository {
            repository.validate()?;
            job.overrides.key = Some(repository.key);
            job.overrides.storage = Some(repository.storage);
            if let Some(password) = repository.password {
                job.password_ref = Some(format!("gib/autostart/{}/password", job.id));
                write_secret(&paths, &job.id, &password)?;
            }
        }
        if let Some(password) = request.password {
            job.password_ref = Some(format!("gib/autostart/{}/password", job.id));
            write_secret(&paths, &job.id, &password)?;
        }
        if request.message.is_some() {
            job.overrides.message = request.message;
        }
        if request.compression.is_some() {
            job.overrides.compression = request.compression;
        }
        if request.chunk_size.is_some() {
            job.overrides.chunk_size = request.chunk_size;
        }
        if request.ignore_patterns.is_some() {
            job.overrides.ignore_patterns = request.ignore_patterns;
        }
        if request.include_git.is_some() {
            job.overrides.include_git = request.include_git.unwrap_or(false);
        }
        if request.concurrency.is_some() {
            job.overrides.concurrency = request.concurrency;
        }
        if let Some(conflict) = request.conflict {
            job.overrides.conflict = conflict_name(conflict).to_string();
        }
        if let Some(start_now) = request.start_now {
            job.enabled = start_now;
        }
        job.updated_at = Utc::now().to_rfc3339();
        write_job(&paths, &job)?;
        emit_event(self, "updated", Some(&job));
        Ok(AutostartChange {
            job,
            action: "updated".to_string(),
        })
    }

    pub fn autostart_status(&self, name: Option<&str>) -> Result<Vec<AutostartStatus>, GibError> {
        let jobs = self.list_autostart_jobs()?;
        Ok(jobs
            .into_iter()
            .filter(|job| name.is_none_or(|name| job.name == name || job.id == name))
            .map(|job| AutostartStatus {
                platform_enabled: job.enabled,
                job,
            })
            .collect())
    }

    pub fn enable_autostart(&self, name: &str) -> Result<AutostartChange, GibError> {
        self.set_autostart_enabled(name, true)
    }

    pub fn disable_autostart(&self, name: &str) -> Result<AutostartChange, GibError> {
        self.set_autostart_enabled(name, false)
    }

    pub fn remove_autostart(&self, name: &str) -> Result<AutostartChange, GibError> {
        let paths = RegistryPaths::new(self)?;
        let job = find_job(&paths, name)?;
        let path = paths.jobs.join(format!("{}.toml", job.id));
        fs::remove_file(path).map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        let _ = fs::remove_file(paths.secrets.join(format!("{}.secret", job.id)));
        emit_event(self, "removed", Some(&job));
        Ok(AutostartChange {
            job,
            action: "removed".to_string(),
        })
    }

    pub async fn run_autostart(&self, name_or_id: &str) -> Result<AutostartRun, GibError> {
        self.run_autostart_with_password(name_or_id, None).await
    }

    /// Runs a registered job, optionally supplying a password obtained from an
    /// embedding application's credential store. The password is used only
    /// for this run and is never included in the returned job or events.
    pub async fn run_autostart_with_password(
        &self,
        name_or_id: &str,
        password: Option<String>,
    ) -> Result<AutostartRun, GibError> {
        let paths = RegistryPaths::new(self)?;
        let job = find_job_by_name_or_id(&paths, name_or_id)?;
        if !job.enabled {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "Autostart job is disabled",
            ));
        }
        let key = job.overrides.key.clone().ok_or_else(|| {
            GibError::new(
                ErrorCode::InvalidRequest,
                "Autostart job has no repository key",
            )
        })?;
        let storage = job.overrides.storage.clone().ok_or_else(|| {
            GibError::new(ErrorCode::InvalidRequest, "Autostart job has no storage")
        })?;
        let mut repository = RepositoryRequest::new(key, storage);
        let stored_password = if password.is_none() {
            read_secret(&paths, &job.id)?
        } else {
            None
        };
        match password.or(stored_password) {
            Some(password) => repository.password = Some(password),
            None if job.password_ref.is_some() => {
                return Err(GibError::new(
                    ErrorCode::PasswordRequired,
                    "The stored password for this autostart job is unavailable; update the job and provide the password again",
                ));
            }
            None => {}
        }
        let mut request = LiveRequest::new(repository, job.root_path.clone());
        request.message = job.overrides.message.clone();
        request.compression = job.overrides.compression.unwrap_or(3);
        request.chunk_size = job.overrides.chunk_size.unwrap_or(5 * 1024 * 1024);
        request.ignore_patterns = job.overrides.ignore_patterns.clone().unwrap_or_default();
        request.include_git = job.overrides.include_git;
        request.concurrency = job
            .overrides
            .concurrency
            .unwrap_or_else(|| num_cpus::get().saturating_mul(2).max(1));
        request.conflict = if job.overrides.conflict == "remote" {
            ConflictPolicy::Remote
        } else {
            ConflictPolicy::Local
        };
        let runner = self.with_config_path(job.config_path.clone());
        let handle = runner.start_live(request).await?;
        emit_event(self, "started", Some(&job));
        Ok(AutostartRun { job, handle })
    }

    pub fn follow_autostart_events(&self, name_or_id: &str) -> Result<Vec<String>, GibError> {
        let path = self.autostart_log_path(name_or_id)?;
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(path)
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        Ok(content.lines().map(ToOwned::to_owned).collect())
    }

    /// Returns the JSONL event-log path for an autostart job. The path is
    /// useful to an embedding application's own event consumer; reading it
    /// does not start or follow a background task.
    pub fn autostart_log_path(&self, name_or_id: &str) -> Result<PathBuf, GibError> {
        let paths = RegistryPaths::new(self)?;
        let job = find_job_by_name_or_id(&paths, name_or_id)?;
        Ok(paths.logs.join(format!("{}.jsonl", job.id)))
    }

    /// Returns the non-secret credential-store identifier associated with a
    /// job. It is useful for a CLI adapter migrating legacy OS keychain
    /// entries; the credential value itself is never exposed.
    pub fn autostart_password_reference(
        &self,
        name_or_id: &str,
    ) -> Result<Option<String>, GibError> {
        let paths = RegistryPaths::new(self)?;
        Ok(find_job_by_name_or_id(&paths, name_or_id)?
            .password_reference()
            .map(str::to_owned))
    }

    fn set_autostart_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<AutostartChange, GibError> {
        let paths = RegistryPaths::new(self)?;
        let mut job = find_job(&paths, name)?;
        job.enabled = enabled;
        job.updated_at = Utc::now().to_rfc3339();
        write_job(&paths, &job)?;
        emit_event(
            self,
            if enabled { "enabled" } else { "disabled" },
            Some(&job),
        );
        Ok(AutostartChange {
            job,
            action: if enabled { "enabled" } else { "disabled" }.to_string(),
        })
    }
}

struct RegistryPaths {
    root: PathBuf,
    jobs: PathBuf,
    logs: PathBuf,
    secrets: PathBuf,
}

impl RegistryPaths {
    fn new(gib: &Gib) -> Result<Self, GibError> {
        let preferred = gib.inner.context.data_dir.join("autostart");
        let root = legacy_registry_root(&gib.inner.context.data_dir)
            .filter(|legacy| !preferred.join("jobs").is_dir() && legacy.join("jobs").is_dir())
            .unwrap_or(preferred);
        Ok(Self {
            jobs: root.join("jobs"),
            logs: root.join("logs"),
            secrets: root.join("secrets"),
            root,
        })
    }
}

fn ensure_registry(paths: &RegistryPaths) -> Result<(), GibError> {
    for path in [&paths.root, &paths.jobs, &paths.logs, &paths.secrets] {
        fs::create_dir_all(path)
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
    }
    protect_directory(&paths.root)?;
    protect_directory(&paths.secrets)?;
    Ok(())
}

fn find_job(paths: &RegistryPaths, name: &str) -> Result<AutostartJob, GibError> {
    find_job_by_name_or_id(paths, name)
}

fn find_job_by_name_or_id(
    paths: &RegistryPaths,
    name_or_id: &str,
) -> Result<AutostartJob, GibError> {
    let mut jobs = Vec::new();
    if paths.jobs.is_dir() {
        for entry in fs::read_dir(&paths.jobs)
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?
        {
            let entry = entry.map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            jobs.push(read_job_file(&entry.path())?);
        }
    }
    jobs.into_iter()
        .find(|job| job.name == name_or_id || job.id == name_or_id)
        .ok_or_else(|| {
            GibError::new(
                ErrorCode::StorageNotFound,
                format!("Autostart job '{name_or_id}' was not found"),
            )
        })
}

fn write_job(paths: &RegistryPaths, job: &AutostartJob) -> Result<(), GibError> {
    ensure_registry(paths)?;
    validate_job(job)?;
    let persisted = PersistedJob::from(job);
    let text = toml::to_string_pretty(&persisted)
        .map_err(|error| GibError::new(ErrorCode::Serialization, error.to_string()))?;
    let path = paths.jobs.join(format!("{}.toml", job.id));
    let temporary = path.with_file_name(format!(".{}.tmp-{}", job.id, std::process::id()));
    fs::write(&temporary, text).map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(&path).map_err(|error| {
            GibError::new(
                ErrorCode::Io,
                format!("Failed to replace autostart job: {error}"),
            )
        })?;
    }
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        GibError::new(ErrorCode::Io, error.to_string())
    })
}

fn write_secret(paths: &RegistryPaths, id: &str, password: &str) -> Result<(), GibError> {
    let path = paths.secrets.join(format!("{id}.secret"));
    fs::write(&path, password).map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
    protect_secret_path(&path)
}

fn read_secret(paths: &RegistryPaths, id: &str) -> Result<Option<String>, GibError> {
    match fs::read_to_string(paths.secrets.join(format!("{id}.secret"))) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(GibError::new(ErrorCode::Io, error.to_string())),
    }
}

fn generate_job_id(name: &str, root: &Path) -> String {
    let now = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let digest = Sha256::digest(format!("{name}\n{}\n{now}", root.display()).as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("job-{suffix}")
}

fn validate_job(job: &AutostartJob) -> Result<(), GibError> {
    if job.version != JOB_VERSION {
        return Err(GibError::new(
            ErrorCode::InvalidConfiguration,
            "Unsupported autostart job version",
        ));
    }
    if job.id.is_empty()
        || !job
            .id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(GibError::new(
            ErrorCode::InvalidConfiguration,
            "Autostart job has an invalid identifier",
        ));
    }
    validate_job_name(&job.name)?;
    if job.root_path.as_os_str().is_empty() {
        return Err(GibError::new(
            ErrorCode::InvalidConfiguration,
            "Autostart job has an empty root path",
        ));
    }
    Ok(())
}

fn emit_event(gib: &Gib, event: &str, job: Option<&AutostartJob>) {
    gib.events().emit(GibEvent::Autostart(AutostartEvent {
        event: event.to_string(),
        name: job.map(|job| job.name.clone()),
        job_id: job.map(|job| job.id.clone()),
    }));
}

fn legacy_registry_root(data_dir: &Path) -> Option<PathBuf> {
    let default_data_dir = dirs::home_dir().map(|home| home.join(".gib"))?;
    if data_dir != default_data_dir {
        return None;
    }
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .map(|path| path.join("gib").join("autostart"))
}

#[derive(Default, Deserialize)]
struct StoredOverrides {
    #[serde(default)]
    storage: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    compression: Option<i32>,
    #[serde(default)]
    compress: Option<i32>,
    #[serde(default)]
    chunk_size: Option<TomlValue>,
    #[serde(default)]
    ignore_patterns: Option<Vec<String>>,
    #[serde(default)]
    ignore: Option<Vec<String>>,
    #[serde(default)]
    include_git: Option<bool>,
    #[serde(default)]
    no_ignore_git: Option<bool>,
    #[serde(default)]
    concurrency: Option<usize>,
    #[serde(default = "default_conflict")]
    conflict: String,
}

#[derive(Default, Deserialize, Serialize)]
struct StoredSecrets {
    #[serde(default)]
    password_ref: Option<String>,
}

#[derive(Deserialize)]
struct StoredJob {
    version: u32,
    id: String,
    name: String,
    enabled: bool,
    root_path: PathBuf,
    #[serde(default)]
    config_path: Option<PathBuf>,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    overrides: StoredOverrides,
    #[serde(default)]
    secrets: Option<StoredSecrets>,
    #[serde(default)]
    password_ref: Option<String>,
}

/// The TOML representation is deliberately kept separate from the public
/// response DTO. This lets the on-disk shape retain legacy secret-reference
/// fields without making those fields part of the serialized public API.
#[derive(Serialize)]
struct PersistedJob {
    version: u32,
    id: String,
    name: String,
    enabled: bool,
    root_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config_path: Option<PathBuf>,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    overrides: AutostartOverrides,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secrets: Option<StoredSecrets>,
}

impl From<&AutostartJob> for PersistedJob {
    fn from(job: &AutostartJob) -> Self {
        Self {
            version: job.version,
            id: job.id.clone(),
            name: job.name.clone(),
            enabled: job.enabled,
            root_path: job.root_path.clone(),
            config_path: job.config_path.clone(),
            created_at: job.created_at.clone(),
            updated_at: job.updated_at.clone(),
            overrides: job.overrides.clone(),
            secrets: job.password_ref.as_ref().map(|password_ref| StoredSecrets {
                password_ref: Some(password_ref.clone()),
            }),
        }
    }
}

fn read_job_file(path: &Path) -> Result<AutostartJob, GibError> {
    let text = fs::read_to_string(path)
        .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
    let stored: StoredJob = toml::from_str(&text)
        .map_err(|error| GibError::new(ErrorCode::Serialization, error.to_string()))?;
    let overrides = stored.overrides;
    let job = AutostartJob {
        version: stored.version,
        id: stored.id,
        name: stored.name,
        enabled: stored.enabled,
        root_path: stored.root_path,
        config_path: stored.config_path,
        created_at: stored.created_at,
        updated_at: stored.updated_at,
        overrides: AutostartOverrides {
            storage: overrides.storage,
            key: overrides.key,
            message: overrides.message,
            compression: overrides.compression.or(overrides.compress),
            chunk_size: overrides.chunk_size.and_then(parse_chunk_size),
            ignore_patterns: overrides.ignore_patterns.or(overrides.ignore),
            include_git: overrides
                .include_git
                .or(overrides.no_ignore_git)
                .unwrap_or(false),
            concurrency: overrides.concurrency,
            conflict: overrides.conflict,
        },
        password_ref: stored
            .password_ref
            .or_else(|| stored.secrets.and_then(|secrets| secrets.password_ref)),
    };
    validate_job(&job)?;
    Ok(job)
}

fn parse_chunk_size(value: TomlValue) -> Option<u64> {
    match value {
        TomlValue::Integer(value) => u64::try_from(value).ok(),
        TomlValue::String(value) => parse_size::parse_size(&value).ok(),
        _ => None,
    }
}

fn protect_secret_path(path: &Path) -> Result<(), GibError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        // std does not expose portable ACL management. Windows inherits the
        // permissions of the protected secrets directory created below.
        let _ = path;
    }
    Ok(())
}

fn protect_directory(path: &Path) -> Result<(), GibError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        // Keep this helper explicit on Windows, where ACLs are inherited from
        // the user's profile and are not configurable through std alone.
        let _ = path;
    }
    Ok(())
}

fn validate_job_name(name: &str) -> Result<(), GibError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Autostart names may contain only ASCII letters, numbers, hyphens, and underscores",
        ));
    }
    Ok(())
}

fn default_conflict() -> String {
    "local".to_string()
}

fn conflict_name(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Local => "local",
        ConflictPolicy::Remote => "remote",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_legacy_autostart_job_shape() {
        let path = std::env::temp_dir().join(format!(
            "gib-autostart-legacy-{}.toml",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let legacy = r#"
version = 1
id = "job-legacy"
name = "legacy"
enabled = true
root_path = "/tmp/legacy-project"
created_at = "2025-01-01T00:00:00Z"
updated_at = "2025-01-01T00:00:00Z"

[overrides]
compress = 7
chunk_size = "2 MiB"
ignore = ["target"]
no_ignore_git = true
conflict = "remote"

[secrets]
password_ref = "gib/autostart/job-legacy/password"
"#;
        fs::write(&path, legacy).expect("legacy job should be written");

        let job = read_job_file(&path).expect("legacy job should be readable");
        assert_eq!(job.overrides.compression, Some(7));
        assert_eq!(job.overrides.chunk_size, Some(2 * 1024 * 1024));
        assert_eq!(
            job.overrides.ignore_patterns,
            Some(vec!["target".to_string()])
        );
        assert!(job.overrides.include_git);
        assert_eq!(
            job.password_ref.as_deref(),
            Some("gib/autostart/job-legacy/password")
        );
        assert!(!format!("{job:?}").contains("gib/autostart/job-legacy/password"));

        fs::remove_file(path).expect("legacy fixture should be removed");
    }

    #[test]
    fn preserves_password_reference_when_a_job_is_rewritten() {
        let root = std::env::temp_dir().join(format!(
            "gib-autostart-write-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let paths = RegistryPaths {
            jobs: root.join("jobs"),
            logs: root.join("logs"),
            secrets: root.join("secrets"),
            root: root.clone(),
        };
        let job = AutostartJob {
            version: JOB_VERSION,
            id: "job-reference".to_string(),
            name: "reference".to_string(),
            enabled: true,
            root_path: PathBuf::from("/tmp/reference"),
            config_path: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            overrides: AutostartOverrides::default(),
            password_ref: Some("gib/autostart/job-reference/password".to_string()),
        };

        write_job(&paths, &job).expect("job should be written");
        let loaded = read_job_file(&paths.jobs.join("job-reference.toml"))
            .expect("rewritten job should be readable");
        assert_eq!(
            loaded.password_reference(),
            Some("gib/autostart/job-reference/password")
        );

        fs::remove_dir_all(root).expect("temporary registry should be removed");
    }
}
