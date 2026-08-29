use super::attempt::AttemptOutcome;
use super::error::SessionError;
use super::model::{AgentSession, SessionLimits, SessionSummary};
use super::redaction::hash_bytes;
use super::trace::TraceEventKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSIONS_DIRECTORY_NAME: &str = "sessions";
const CREATION_LOCK_FILE_NAME: &str = ".creation.lock";
const STALE_LOCK_AFTER: Duration = Duration::from_secs(15 * 60);
static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct SessionPaths {
    root: PathBuf,
    sessions: PathBuf,
}

impl SessionPaths {
    pub(crate) fn default() -> Result<Self, SessionError> {
        let home = dirs::home_dir().ok_or(SessionError::MissingHomeDirectory)?;
        Ok(Self::from_root(home.join(".gib").join("ai")))
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            sessions: root.join(SESSIONS_DIRECTORY_NAME),
            root,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn sessions_dir(&self) -> &Path {
        &self.sessions
    }

    pub(crate) fn creation_lock_path(&self) -> PathBuf {
        self.sessions.join(CREATION_LOCK_FILE_NAME)
    }

    pub(crate) fn session_path(&self, session_id: &str) -> Result<PathBuf, SessionError> {
        validate_session_id(session_id)?;
        self.ensure_root()?;
        let path = self.sessions.join(format!("{session_id}.json"));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    fn session_lock_path(&self, session_id: &str) -> Result<PathBuf, SessionError> {
        validate_session_id(session_id)?;
        self.ensure_root()?;
        Ok(self.sessions.join(format!(".{session_id}.lock")))
    }

    fn ensure_root(&self) -> Result<(), SessionError> {
        ensure_directory(&self.root, 0o700)?;
        ensure_directory(&self.sessions, 0o700)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionWarning {
    pub(crate) session_id: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionList {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) warnings: Vec<SessionWarning>,
}

/// Owns session paths, locks, migration, recovery, and atomic replacement
/// writes. Workflow policy remains in `SessionService`.
#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    paths: SessionPaths,
    limits: SessionLimits,
    lock_timeout: Duration,
}

impl SessionStore {
    pub(crate) fn new() -> Result<Self, SessionError> {
        Ok(Self::from_paths(SessionPaths::default()?))
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        Self::from_paths(SessionPaths::from_root(root))
    }

    pub(crate) fn from_paths(paths: SessionPaths) -> Self {
        Self {
            paths,
            limits: SessionLimits::default(),
            lock_timeout: Duration::from_secs(30),
        }
    }

    pub(crate) fn with_limits(mut self, limits: SessionLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    pub(crate) fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    pub(crate) fn limits(&self) -> SessionLimits {
        self.limits
    }

    pub(crate) fn create(&self, session: &AgentSession) -> Result<AgentSession, SessionError> {
        self.create_blocking(session)
    }

    pub(crate) fn load(&self, session_id: impl AsRef<str>) -> Result<AgentSession, SessionError> {
        self.load_blocking(session_id)
    }

    pub(crate) fn save(&self, session: &AgentSession) -> Result<(), SessionError> {
        self.save_blocking(session)
    }

    pub(crate) fn mutate<F>(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        mutation: F,
    ) -> Result<AgentSession, SessionError>
    where
        F: FnOnce(&mut AgentSession) -> Result<(), SessionError>,
    {
        self.mutate_blocking(session_id, expected_revision, mutation)
    }

    pub(crate) fn create_blocking(
        &self,
        session: &AgentSession,
    ) -> Result<AgentSession, SessionError> {
        session.validate(self.limits)?;
        let path = self.paths.session_path(session.session_id.as_str())?;
        let _lock = SessionLock::acquire(
            &self.paths.creation_lock_path(),
            "session creation",
            self.lock_timeout,
        )?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(SessionError::UnsafePath);
            }
            Ok(_) => {
                return Err(SessionError::SessionAlreadyExists {
                    id: session.session_id.to_string(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(SessionError::io("inspect session")),
        }
        self.persist_document(&path, session)?;
        Ok(session.clone())
    }

    pub(crate) fn load_blocking(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<AgentSession, SessionError> {
        let session_id = session_id.as_ref().to_string();
        let path = self.paths.session_path(&session_id)?;
        let lock_path = self.paths.session_lock_path(&session_id)?;
        let _lock = SessionLock::acquire(&lock_path, "agent session", self.lock_timeout)?;
        let mut session = self.read_document(&path, &session_id)?;
        if recover_in_flight(&mut session, self.limits)? {
            self.persist_document(&path, &session)?;
        }
        Ok(session)
    }

    pub(crate) fn save_blocking(&self, session: &AgentSession) -> Result<(), SessionError> {
        let path = self.paths.session_path(session.session_id.as_str())?;
        let lock_path = self.paths.session_lock_path(session.session_id.as_str())?;
        let _lock = SessionLock::acquire(&lock_path, "agent session", self.lock_timeout)?;
        self.persist_document(&path, session)
    }

    pub(crate) fn mutate_blocking<F>(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        mutation: F,
    ) -> Result<AgentSession, SessionError>
    where
        F: FnOnce(&mut AgentSession) -> Result<(), SessionError>,
    {
        let session_id = session_id.as_ref().to_string();
        let path = self.paths.session_path(&session_id)?;
        let lock_path = self.paths.session_lock_path(&session_id)?;
        let _lock = SessionLock::acquire(&lock_path, "agent session", self.lock_timeout)?;
        let mut session = self.read_document(&path, &session_id)?;
        let recovered = recover_in_flight(&mut session, self.limits)?;
        if session.revision != expected_revision {
            if recovered {
                self.persist_document(&path, &session)?;
            }
            return Err(SessionError::RevisionConflict {
                id: session_id,
                expected: expected_revision,
                actual: session.revision,
            });
        }
        let immutable_identity = (
            session.session_id.clone(),
            session.turn_id.clone(),
            session.conversation_id.clone(),
            session.request_fingerprint.clone(),
            session.workflow_id.clone(),
            session.workflow_version.clone(),
            session.created_at.clone(),
            session.runtime_profile,
            session.budget.limits(),
        );
        mutation(&mut session)?;
        if session.session_id != immutable_identity.0
            || session.turn_id != immutable_identity.1
            || session.conversation_id != immutable_identity.2
            || session.request_fingerprint != immutable_identity.3
            || session.workflow_id != immutable_identity.4
            || session.workflow_version != immutable_identity.5
            || session.created_at != immutable_identity.6
            || session.runtime_profile != immutable_identity.7
            || session.budget.limits() != immutable_identity.8
        {
            return Err(SessionError::InvalidWorkflow);
        }
        session.touch()?;
        session.validate(self.limits)?;
        self.persist_document(&path, &session)?;
        Ok(session)
    }

    pub(crate) fn list_blocking(&self) -> Result<SessionList, SessionError> {
        self.paths.ensure_root()?;
        let entries = fs::read_dir(self.paths.sessions_dir())
            .map_err(|_| SessionError::io("list sessions"))?;
        let mut sessions = Vec::new();
        let mut warnings = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    warnings.push(SessionWarning {
                        session_id: "unknown".to_string(),
                        code: "io_error".to_string(),
                        message: "a session directory entry could not be read".to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            match self.load_blocking(id) {
                Ok(session) => sessions.push(session.summary()),
                Err(error) => warnings.push(SessionWarning {
                    session_id: safe_warning_id(id),
                    code: error.code().to_string(),
                    message: error.to_string(),
                }),
            }
        }
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        warnings.sort_by(|left, right| {
            left.session_id
                .cmp(&right.session_id)
                .then_with(|| left.code.cmp(&right.code))
        });
        Ok(SessionList { sessions, warnings })
    }

    fn read_document(&self, path: &Path, expected_id: &str) -> Result<AgentSession, SessionError> {
        ensure_regular_or_missing(path)?;
        let file = File::open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SessionError::SessionNotFound {
                    id: expected_id.to_string(),
                }
            } else {
                SessionError::io("open session")
            }
        })?;
        let mut bytes = Vec::new();
        file.take(self.limits.max_session_file_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| SessionError::io("read session"))?;
        if bytes.len() > self.limits.max_session_file_bytes {
            return Err(SessionError::LimitExceeded {
                resource: "session file bytes".to_string(),
                limit: self.limits.max_session_file_bytes,
                actual: bytes.len(),
            });
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| SessionError::MalformedSession {
                id: expected_id.to_string(),
            })?;
        let version = schema_version(&value, expected_id)?;
        if version > super::model::SESSION_SCHEMA_VERSION {
            return Err(SessionError::FutureSchemaVersion {
                id: expected_id.to_string(),
                version,
            });
        }
        let migrated = migrate(value, version, expected_id)?;
        let session: AgentSession =
            serde_json::from_value(migrated).map_err(|_| SessionError::MalformedSession {
                id: expected_id.to_string(),
            })?;
        if session.session_id.as_str() != expected_id {
            return Err(SessionError::MalformedSession {
                id: expected_id.to_string(),
            });
        }
        session
            .validate(self.limits)
            .map_err(|_| SessionError::MalformedSession {
                id: expected_id.to_string(),
            })?;
        Ok(session)
    }

    fn persist_document(&self, path: &Path, session: &AgentSession) -> Result<(), SessionError> {
        session.validate(self.limits)?;
        let mut encoded = serde_json::to_vec_pretty(session)
            .map_err(|_| SessionError::serialization("encode session"))?;
        encoded.push(b'\n');
        if encoded.len() > self.limits.max_session_file_bytes {
            return Err(SessionError::LimitExceeded {
                resource: "session file bytes".to_string(),
                limit: self.limits.max_session_file_bytes,
                actual: encoded.len(),
            });
        }
        write_atomic(path, &encoded)
    }
}

fn safe_warning_id(value: &str) -> String {
    if validate_session_id(value).is_ok() {
        value.to_string()
    } else {
        hash_bytes("session-", value.as_bytes())
    }
}

fn validate_session_id(value: &str) -> Result<(), SessionError> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
    {
        return Err(SessionError::InvalidIdentifier {
            kind: "session ID".to_string(),
        });
    }
    Ok(())
}

fn schema_version(value: &Value, id: &str) -> Result<u32, SessionError> {
    let Some(object) = value.as_object() else {
        return Err(SessionError::MalformedSession { id: id.to_string() });
    };
    let version_value = object
        .get("schema_version")
        .or_else(|| object.get("version"));
    let version = version_value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    if version_value.is_some_and(|value| value.as_u64().is_none()) {
        return Err(SessionError::MalformedSession { id: id.to_string() });
    }
    Ok(version)
}

fn migrate(mut value: Value, version: u32, id: &str) -> Result<Value, SessionError> {
    match version {
        0 => migrate_v0(&mut value, id)?,
        super::model::SESSION_SCHEMA_VERSION => {}
        version => {
            return Err(SessionError::UnsupportedSchemaVersion {
                id: id.to_string(),
                version,
            });
        }
    }
    Ok(value)
}

fn migrate_v0(value: &mut Value, id: &str) -> Result<(), SessionError> {
    let Some(object) = value.as_object_mut() else {
        return Err(SessionError::MalformedSession { id: id.to_string() });
    };
    if !object.contains_key("session_id")
        && let Some(legacy_id) = object.remove("id")
    {
        object.insert("session_id".to_string(), legacy_id);
    }
    if !object.contains_key("turn_id")
        && let Some(turn_id) = object.remove("turn")
    {
        object.insert("turn_id".to_string(), turn_id);
    }
    object.remove("version");
    object.insert(
        "schema_version".to_string(),
        Value::from(super::model::SESSION_SCHEMA_VERSION),
    );
    object
        .entry("session_id".to_string())
        .or_insert_with(|| Value::String(id.to_string()));
    object
        .entry("turn_id".to_string())
        .or_insert_with(|| Value::String("turn-legacy".to_string()));
    object
        .entry("conversation_id".to_string())
        .or_insert_with(|| Value::String("conversation-legacy".to_string()));
    object
        .entry("request_fingerprint".to_string())
        .or_insert_with(|| {
            Value::String(super::redaction::hash_bytes(
                "sha256:",
                format!("legacy:{id}").as_bytes(),
            ))
        });
    object
        .entry("workflow_id".to_string())
        .or_insert_with(|| Value::String("legacy".to_string()));
    object
        .entry("workflow_version".to_string())
        .or_insert_with(|| Value::String("0".to_string()));
    object
        .entry("phase".to_string())
        .or_insert_with(|| Value::String("classify".to_string()));
    object
        .entry("status".to_string())
        .or_insert_with(|| Value::String("running".to_string()));
    object
        .entry("created_at".to_string())
        .or_insert_with(|| Value::String("1970-01-01T00:00:00Z".to_string()));
    let created = object
        .get("created_at")
        .cloned()
        .unwrap_or_else(|| Value::String("1970-01-01T00:00:00Z".to_string()));
    object.entry("updated_at".to_string()).or_insert(created);
    object
        .entry("runtime_profile".to_string())
        .or_insert_with(|| Value::String("balanced".to_string()));
    object.entry("budget".to_string()).or_insert_with(|| {
        serde_json::to_value(super::budget::AgentBudget::default_budget()).unwrap_or(Value::Null)
    });
    for key in ["artifact_refs", "evidence_refs", "attempts"] {
        object
            .entry(key.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    object.entry("trace".to_string()).or_insert_with(|| {
        serde_json::to_value(super::trace::TraceLog::new()).unwrap_or(Value::Null)
    });
    object
        .entry("stop_reason".to_string())
        .or_insert(Value::Null);
    object
        .entry("revision".to_string())
        .or_insert_with(|| Value::from(0_u64));
    Ok(())
}

fn recover_in_flight(
    session: &mut AgentSession,
    limits: SessionLimits,
) -> Result<bool, SessionError> {
    let mut recovered = false;
    let mut interrupted = Vec::new();
    for attempt in &mut session.attempts {
        if attempt.outcome == AttemptOutcome::Running {
            let attempt_id = attempt.attempt_id.clone();
            attempt.finish(
                AttemptOutcome::Interrupted,
                attempt.budget_delta,
                attempt.artifact_refs.clone(),
                attempt.evidence_refs.clone(),
                Some("interrupted".to_string()),
            )?;
            interrupted.push(attempt_id);
            recovered = true;
        }
    }
    for attempt_id in interrupted {
        session.record_trace(
            TraceEventKind::AttemptInterrupted,
            "in-flight attempt marked interrupted during recovery",
            Some(attempt_id),
            Vec::new(),
            Vec::new(),
            None,
            false,
        )?;
    }
    if recovered {
        session.touch()?;
        session.validate(limits)?;
    }
    Ok(recovered)
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), SessionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(SessionError::UnsafePath)
        }
        Ok(_) => set_directory_mode(path, mode),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|_| SessionError::io("create session directory"))?;
            set_directory_mode(path, mode)
        }
        Err(_) => Err(SessionError::io("inspect session directory")),
    }
}

fn ensure_regular_or_missing(path: &Path) -> Result<(), SessionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(SessionError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(SessionError::io("inspect session file")),
    }
}

fn set_directory_mode(path: &Path, mode: u32) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|_| SessionError::io("protect session directory"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), SessionError> {
    let parent = path.parent().ok_or(SessionError::UnsafePath)?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.json"),
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| SessionError::io("create temporary session file"))?;
        set_file_mode(&file)?;
        file.write_all(data)
            .map_err(|_| SessionError::io("write temporary session file"))?;
        file.sync_all()
            .map_err(|_| SessionError::io("sync temporary session file"))?;
        fs::rename(&temporary, path).map_err(|_| SessionError::io("publish session file"))?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_directory(path: &Path) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| SessionError::io("sync session directory"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_file_mode(file: &File) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| SessionError::io("protect session file"))?;
    }
    Ok(())
}

struct SessionLock {
    path: PathBuf,
    token: String,
}

impl SessionLock {
    fn acquire(path: &Path, scope: &str, timeout: Duration) -> Result<Self, SessionError> {
        let parent = path.parent().ok_or(SessionError::UnsafePath)?;
        ensure_directory(parent, 0o700)?;
        let token = format!(
            "pid={}\ncreated_at={}\nnonce={}\n",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    set_file_mode(&file)?;
                    file.write_all(token.as_bytes())
                        .map_err(|_| SessionError::io("write session lock"))?;
                    file.sync_all()
                        .map_err(|_| SessionError::io("sync session lock"))?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        token,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure_regular_or_missing(path)?;
                    if is_stale(path) {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if started.elapsed() >= timeout {
                        return Err(SessionError::LockTimeout {
                            scope: scope.to_string(),
                        });
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return Err(SessionError::io("create session lock")),
            }
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let owns_lock = fs::read_to_string(&self.path)
            .map(|contents| contents == self.token)
            .unwrap_or(false);
        if owns_lock {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn is_stale(path: &Path) -> bool {
    if let Ok(contents) = fs::read_to_string(path)
        && let Some(created_at) = contents.lines().find_map(|line| {
            line.strip_prefix("created_at=")
                .and_then(|value| value.parse::<u64>().ok())
        })
    {
        return SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|now| now.as_secs().saturating_sub(created_at) > STALE_LOCK_AFTER.as_secs())
            .unwrap_or(false);
    }
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK_AFTER)
}
