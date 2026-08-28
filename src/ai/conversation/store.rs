use super::error::ConversationError;
use super::lock::ConversationLock;
use super::model::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationLimits, ConversationList,
    ConversationWarning, current_timestamp,
};
use super::paths::{ConversationPaths, ensure_regular_or_missing, validate_conversation_id};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Owns conversation paths, serialization, migration, locking, and durable
/// replacement writes. User-facing operations remain in ConversationService.
#[derive(Debug, Clone)]
pub(crate) struct ConversationStore {
    paths: ConversationPaths,
    limits: ConversationLimits,
    lock_timeout: Duration,
}

impl ConversationStore {
    pub(crate) fn new() -> Result<Self, ConversationError> {
        Ok(Self::from_paths(ConversationPaths::default()?))
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        Self::from_paths(ConversationPaths::from_root(root))
    }

    pub(crate) fn from_paths(paths: ConversationPaths) -> Self {
        Self {
            paths,
            limits: ConversationLimits::default(),
            lock_timeout: Duration::from_secs(30),
        }
    }

    pub(crate) fn with_limits(mut self, limits: ConversationLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    pub(crate) fn paths(&self) -> &ConversationPaths {
        &self.paths
    }

    pub(crate) fn limits(&self) -> ConversationLimits {
        self.limits
    }

    /// Load a single document after validating its ID, file size, schema
    /// version, migration result, and persisted contract.
    pub(crate) fn load_blocking(
        &self,
        conversation_id: &str,
    ) -> Result<Conversation, ConversationError> {
        validate_conversation_id(conversation_id)?;
        let path = self.paths.conversation_path(conversation_id)?;
        self.read_document(&path, conversation_id)
    }

    /// List valid summaries while isolating malformed or future-version files
    /// as structured warnings.
    pub(crate) fn list_blocking(&self) -> Result<ConversationList, ConversationError> {
        self.paths.ensure_root()?;
        let entries = fs::read_dir(self.paths.conversations_dir())
            .map_err(|_| ConversationError::io("list conversations"))?;
        let mut conversations = Vec::new();
        let mut warnings = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    warnings.push(ConversationWarning {
                        conversation_id: "unknown".to_string(),
                        code: "io_error".to_string(),
                        message: "a conversation directory entry could not be read".to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let stem = path.file_stem().and_then(|value| value.to_str());
            let Some(conversation_id) = stem else {
                warnings.push(ConversationWarning {
                    conversation_id: "unknown".to_string(),
                    code: "invalid_conversation_id".to_string(),
                    message: "a conversation file has an invalid file name".to_string(),
                });
                continue;
            };
            if validate_conversation_id(conversation_id).is_err() {
                warnings.push(ConversationWarning {
                    conversation_id: "unknown".to_string(),
                    code: "invalid_conversation_id".to_string(),
                    message: "a conversation file has an invalid file name".to_string(),
                });
                continue;
            }

            match self.read_document(&path, conversation_id) {
                Ok(conversation) => conversations.push(conversation.summary()),
                Err(error) => warnings.push(ConversationWarning {
                    conversation_id: conversation_id.to_string(),
                    code: error.code().to_string(),
                    message: error.to_string(),
                }),
            }
        }

        conversations.sort_by(|left, right| {
            left.conversation_id
                .cmp(&right.conversation_id)
                .then_with(|| left.updated_at.cmp(&right.updated_at))
        });
        warnings.sort_by(|left, right| {
            left.conversation_id
                .cmp(&right.conversation_id)
                .then_with(|| left.code.cmp(&right.code))
        });
        Ok(ConversationList {
            conversations,
            warnings,
        })
    }

    /// Persist a newly-created document under a creation lock so even an
    /// unlikely generated-ID collision cannot overwrite an existing file.
    pub(crate) fn create_blocking(
        &self,
        conversation: &Conversation,
    ) -> Result<Conversation, ConversationError> {
        conversation.validate(self.limits)?;
        let path = self
            .paths
            .conversation_path(&conversation.conversation_id)?;
        let _lock = ConversationLock::acquire(
            &self.paths.creation_lock_path(),
            "conversation creation",
            self.lock_timeout,
        )?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ConversationError::UnsafePath);
            }
            Ok(_) => {
                return Err(ConversationError::ConversationAlreadyExists {
                    id: conversation.conversation_id.clone(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ConversationError::io("inspect conversation")),
        }
        self.persist_document(&path, conversation)?;
        Ok(conversation.clone())
    }

    /// Mutate one document under its per-conversation lock and enforce an
    /// optimistic revision check against the freshly-loaded on-disk value.
    pub(crate) fn mutate_blocking<F>(
        &self,
        conversation_id: &str,
        expected_revision: u64,
        mutation: F,
    ) -> Result<Conversation, ConversationError>
    where
        F: FnOnce(&mut Conversation) -> Result<(), ConversationError>,
    {
        validate_conversation_id(conversation_id)?;
        let path = self.paths.conversation_path(conversation_id)?;
        let lock_path = self.paths.conversation_lock_path(conversation_id)?;
        let _lock = ConversationLock::acquire(&lock_path, "conversation", self.lock_timeout)?;
        let mut conversation = self.read_document(&path, conversation_id)?;
        if conversation.revision != expected_revision {
            return Err(ConversationError::RevisionConflict {
                id: conversation_id.to_string(),
                expected: expected_revision,
                actual: conversation.revision,
            });
        }
        mutation(&mut conversation)?;
        conversation.revision = conversation
            .revision
            .checked_add(1)
            .ok_or(ConversationError::RevisionOverflow)?;
        conversation.updated_at = current_timestamp();
        self.persist_document(&path, &conversation)?;
        Ok(conversation)
    }

    /// Delete one document under its per-conversation lock. Configuration
    /// repair is deliberately handled by ConversationService.
    pub(crate) fn delete_blocking(
        &self,
        conversation_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<Conversation, ConversationError> {
        validate_conversation_id(conversation_id)?;
        let path = self.paths.conversation_path(conversation_id)?;
        let lock_path = self.paths.conversation_lock_path(conversation_id)?;
        let _lock = ConversationLock::acquire(&lock_path, "conversation", self.lock_timeout)?;
        let conversation = self.read_document(&path, conversation_id)?;
        if let Some(expected_revision) = expected_revision
            && conversation.revision != expected_revision
        {
            return Err(ConversationError::RevisionConflict {
                id: conversation_id.to_string(),
                expected: expected_revision,
                actual: conversation.revision,
            });
        }
        fs::remove_file(&path).map_err(|_| ConversationError::io("delete conversation"))?;
        Ok(conversation)
    }

    fn read_document(
        &self,
        path: &Path,
        expected_id: &str,
    ) -> Result<Conversation, ConversationError> {
        ensure_regular_or_missing(path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConversationError::ConversationNotFound {
                    id: expected_id.to_string(),
                });
            }
            Err(_) => return Err(ConversationError::io("open conversation")),
        };
        let mut bytes = Vec::new();
        let read_limit = self.limits.max_file_bytes.saturating_add(1);
        file.take(read_limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| ConversationError::io("read conversation"))?;
        if bytes.len() > self.limits.max_file_bytes {
            return Err(ConversationError::ConversationTooLarge {
                limit: self.limits.max_file_bytes,
                actual: bytes.len(),
            });
        }

        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            ConversationError::MalformedConversation {
                id: expected_id.to_string(),
            }
        })?;
        let version = schema_version(&value, expected_id)?;
        if version > CONVERSATION_SCHEMA_VERSION {
            return Err(ConversationError::FutureSchemaVersion {
                id: expected_id.to_string(),
                version,
            });
        }
        let migrated = migrate(value, version, expected_id)?;
        let conversation: Conversation = serde_json::from_value(migrated).map_err(|_| {
            ConversationError::MalformedConversation {
                id: expected_id.to_string(),
            }
        })?;
        if conversation.conversation_id != expected_id {
            return Err(ConversationError::MalformedConversation {
                id: expected_id.to_string(),
            });
        }
        conversation.validate(self.limits).map_err(|_| {
            ConversationError::MalformedConversation {
                id: expected_id.to_string(),
            }
        })?;
        Ok(conversation)
    }

    fn persist_document(
        &self,
        path: &Path,
        conversation: &Conversation,
    ) -> Result<(), ConversationError> {
        conversation.validate(self.limits)?;
        let mut encoded = serde_json::to_vec_pretty(conversation)
            .map_err(|_| ConversationError::serialization("encode conversation"))?;
        encoded.push(b'\n');
        if encoded.len() > self.limits.max_file_bytes {
            return Err(ConversationError::ConversationTooLarge {
                limit: self.limits.max_file_bytes,
                actual: encoded.len(),
            });
        }
        write_atomic(path, &encoded)
    }
}

fn schema_version(value: &Value, id: &str) -> Result<u32, ConversationError> {
    let Some(object) = value.as_object() else {
        return Err(ConversationError::MalformedConversation { id: id.to_string() });
    };
    let version = object
        .get("schema_version")
        .or_else(|| object.get("version"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    if object
        .get("schema_version")
        .or_else(|| object.get("version"))
        .is_some_and(|value| value.as_u64().is_none())
    {
        return Err(ConversationError::MalformedConversation { id: id.to_string() });
    }
    Ok(version)
}

/// Migrations are ordered and pure: reading a legacy file only transforms an
/// in-memory JSON value. The current schema is written on the next mutation.
fn migrate(mut value: Value, version: u32, id: &str) -> Result<Value, ConversationError> {
    match version {
        0 => migrate_v0(&mut value, id)?,
        CONVERSATION_SCHEMA_VERSION => {}
        version => {
            return Err(ConversationError::UnsupportedSchemaVersion {
                id: id.to_string(),
                version,
            });
        }
    }
    Ok(value)
}

fn migrate_v0(value: &mut Value, id: &str) -> Result<(), ConversationError> {
    let Some(object) = value.as_object_mut() else {
        return Err(ConversationError::MalformedConversation { id: id.to_string() });
    };

    if !object.contains_key("conversation_id")
        && let Some(legacy_id) = object.remove("id")
    {
        object.insert("conversation_id".to_string(), legacy_id);
    }
    object.remove("version");
    object.insert(
        "schema_version".to_string(),
        Value::from(CONVERSATION_SCHEMA_VERSION),
    );
    object
        .entry("title".to_string())
        .or_insert_with(|| Value::String(super::model::DEFAULT_CONVERSATION_TITLE.to_string()));
    object
        .entry("created_at".to_string())
        .or_insert_with(|| Value::String("1970-01-01T00:00:00Z".to_string()));
    let created_at = object
        .get("created_at")
        .cloned()
        .unwrap_or_else(|| Value::String("1970-01-01T00:00:00Z".to_string()));
    object.entry("updated_at".to_string()).or_insert(created_at);
    object
        .entry("revision".to_string())
        .or_insert_with(|| Value::from(0_u64));
    if !object.contains_key("durable_context") {
        let context = object
            .remove("context")
            .unwrap_or_else(|| Value::Object(Map::new()));
        object.insert("durable_context".to_string(), context);
    }
    object
        .entry("messages".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    object
        .entry("archived".to_string())
        .or_insert_with(|| Value::Bool(false));
    migrate_v0_messages(object, id)?;
    Ok(())
}

fn migrate_v0_messages(object: &mut Map<String, Value>, id: &str) -> Result<(), ConversationError> {
    let fallback_timestamp = object
        .get("created_at")
        .and_then(Value::as_str)
        .unwrap_or("1970-01-01T00:00:00Z")
        .to_string();
    let Some(Value::Array(messages)) = object.get_mut("messages") else {
        return Err(ConversationError::MalformedConversation { id: id.to_string() });
    };
    for (index, message) in messages.iter_mut().enumerate() {
        let Some(message) = message.as_object_mut() else {
            return Err(ConversationError::MalformedConversation { id: id.to_string() });
        };
        if !message.contains_key("message_id") {
            let legacy_id = message
                .remove("id")
                .unwrap_or_else(|| Value::String(format!("msg-legacy-{}", index)));
            message.insert("message_id".to_string(), legacy_id);
        }
        if !message.contains_key("timestamp") {
            let timestamp = message
                .remove("created_at")
                .unwrap_or_else(|| Value::String(fallback_timestamp.clone()));
            message.insert("timestamp".to_string(), timestamp);
        }
        message
            .entry("status".to_string())
            .or_insert_with(|| Value::String("complete".to_string()));
    }
    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), ConversationError> {
    let parent = path.parent().ok_or(ConversationError::UnsafePath)?;
    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| ConversationError::io("create temporary conversation file"))?;
        set_file_mode(&file)?;
        file.write_all(data)
            .map_err(|_| ConversationError::io("write temporary conversation file"))?;
        file.sync_all()
            .map_err(|_| ConversationError::io("sync temporary conversation file"))?;
        publish_replacement(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("conversation.json");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(
        ".{}.tmp-{}-{}-{}",
        file_name,
        std::process::id(),
        stamp,
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn publish_replacement(temporary: &Path, destination: &Path) -> Result<(), ConversationError> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|_| ConversationError::io("replace conversation file"))?;
    }
    fs::rename(temporary, destination)
        .map_err(|_| ConversationError::io("publish conversation file"))
}

fn sync_directory(path: &Path) -> Result<(), ConversationError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| ConversationError::io("sync conversation directory"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_file_mode(file: &File) -> Result<(), ConversationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ConversationError::io("protect conversation file"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::conversation::model::{
        ConversationMessage, ConversationMessageRole, ConversationMessageStatus,
    };
    use std::process::Command;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    const PROCESS_TEST_ROOT_ENV: &str = "GIB_CONVERSATION_PROCESS_TEST_ROOT";

    fn store(name: &str) -> ConversationStore {
        let root = std::env::temp_dir().join(format!(
            "gib-conversation-store-{}-{}-{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        ConversationStore::from_root(root)
    }

    fn conversation(id: &str) -> Conversation {
        Conversation::new(
            id.to_string(),
            "Test conversation".to_string(),
            "2026-08-28T12:00:00Z".to_string(),
        )
    }

    #[test]
    fn create_load_append_and_stale_revision_conflict() {
        let store = store("append");
        let initial = store
            .create_blocking(&conversation("conv-append"))
            .expect("conversation should be created");
        let updated = store
            .mutate_blocking("conv-append", initial.revision, |conversation| {
                conversation.messages.push(ConversationMessage::new(
                    "msg-one".to_string(),
                    ConversationMessageRole::User,
                    "2026-08-28T12:01:00Z".to_string(),
                    "hello".to_string(),
                ));
                Ok(())
            })
            .expect("message should be appended");
        assert_eq!(updated.revision, 1);
        assert_eq!(
            store.load_blocking("conv-append").unwrap().messages.len(),
            1
        );
        assert!(matches!(
            store.mutate_blocking("conv-append", 0, |_| Ok(())),
            Err(ConversationError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn malformed_files_are_isolated_from_valid_listing() {
        let store = store("listing");
        store
            .create_blocking(&conversation("conv-valid"))
            .expect("valid conversation should be created");
        let directory = store.paths().conversations_dir();
        fs::write(directory.join("conv-broken.json"), b"not json")
            .expect("malformed fixture should be written");
        let listing = store.list_blocking().expect("listing should succeed");
        assert_eq!(listing.conversations.len(), 1);
        assert_eq!(listing.warnings.len(), 1);
        assert_eq!(listing.warnings[0].code, "malformed_conversation");
        assert_eq!(
            store.load_blocking("conv-broken").unwrap_err().code(),
            "malformed_conversation"
        );
    }

    #[test]
    fn legacy_v0_documents_migrate_in_memory_and_write_current_schema() {
        let store = store("migration");
        store.paths().ensure_root().unwrap();
        let path = store
            .paths()
            .conversation_path("conv-legacy")
            .expect("legacy path should be safe");
        fs::write(
            &path,
            br#"{
                "id": "conv-legacy",
                "title": "Legacy",
                "created_at": "2026-08-28T12:00:00Z",
                "messages": [{
                    "id": "msg-legacy",
                    "role": "user",
                    "content": "hello"
                }]
            }"#,
        )
        .expect("legacy fixture should be written");
        let migrated = store
            .load_blocking("conv-legacy")
            .expect("legacy document should load");
        assert_eq!(migrated.schema_version, CONVERSATION_SCHEMA_VERSION);
        assert_eq!(migrated.messages[0].message_id, "msg-legacy");
        assert_eq!(
            migrated.messages[0].status,
            ConversationMessageStatus::Complete
        );

        store
            .mutate_blocking("conv-legacy", 0, |_| Ok(()))
            .expect("next write should succeed");
        let encoded = fs::read_to_string(path).expect("current document should be readable");
        assert!(encoded.contains("\"schema_version\": 1"));
        assert!(!encoded.contains("\"id\": \"conv-legacy\""));
    }

    #[test]
    fn concurrent_threads_append_without_losing_acknowledged_messages() {
        let store = store("concurrent");
        store
            .create_blocking(&conversation("conv-concurrent"))
            .expect("conversation should be created");
        let mut workers = Vec::new();
        for index in 0..8 {
            let store = store.clone();
            workers.push(thread::spawn(move || {
                loop {
                    let current = store.load_blocking("conv-concurrent").unwrap();
                    let result = store.mutate_blocking(
                        "conv-concurrent",
                        current.revision,
                        |conversation| {
                            conversation.messages.push(ConversationMessage::new(
                                format!("msg-{index}"),
                                ConversationMessageRole::Assistant,
                                current_timestamp(),
                                format!("result-{index}"),
                            ));
                            Ok(())
                        },
                    );
                    if result.is_ok() {
                        break;
                    }
                    assert!(matches!(
                        result,
                        Err(ConversationError::RevisionConflict { .. })
                    ));
                }
            }));
        }
        for worker in workers {
            worker.join().expect("worker should finish");
        }
        let final_document = store
            .load_blocking("conv-concurrent")
            .expect("conversation should remain valid");
        assert_eq!(final_document.messages.len(), 8);
        assert_eq!(final_document.revision, 8);
    }

    #[test]
    fn future_documents_are_never_overwritten() {
        let store = store("future");
        store.paths().ensure_root().unwrap();
        let path = store
            .paths()
            .conversation_path("conv-future")
            .expect("future path should be safe");
        let original = br#"{"schema_version":999,"conversation_id":"conv-future"}"#;
        fs::write(&path, original).expect("future fixture should be written");
        assert!(matches!(
            store.load_blocking("conv-future"),
            Err(ConversationError::FutureSchemaVersion { version: 999, .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn interrupted_temporary_files_do_not_hide_a_valid_document() {
        let store = store("interrupted");
        store
            .create_blocking(&conversation("conv-interrupted"))
            .expect("conversation should be created");
        fs::write(
            store
                .paths()
                .conversations_dir()
                .join(".conv-interrupted.json.tmp-crashed"),
            b"incomplete",
        )
        .expect("interrupted temporary file should be written");
        let listing = store.list_blocking().expect("listing should recover");
        assert_eq!(listing.conversations.len(), 1);
        assert!(listing.warnings.is_empty());
        assert_eq!(
            store
                .load_blocking("conv-interrupted")
                .unwrap()
                .conversation_id,
            "conv-interrupted"
        );
    }

    #[test]
    fn independently_spawned_processes_append_without_losing_messages() {
        if let Ok(root) = std::env::var(PROCESS_TEST_ROOT_ENV) {
            let store = ConversationStore::from_root(root);
            for index in 0..4 {
                loop {
                    let current = store.load_blocking("conv-process").unwrap();
                    let process_id = std::process::id();
                    let result =
                        store.mutate_blocking("conv-process", current.revision, |conversation| {
                            conversation.messages.push(ConversationMessage::new(
                                format!("msg-process-{process_id}-{index}"),
                                ConversationMessageRole::Assistant,
                                current_timestamp(),
                                format!("process-result-{process_id}-{index}"),
                            ));
                            Ok(())
                        });
                    if result.is_ok() {
                        break;
                    }
                    assert!(matches!(
                        result,
                        Err(ConversationError::RevisionConflict { .. })
                    ));
                }
            }
            return;
        }

        let store = store("process");
        store
            .create_blocking(&conversation("conv-process"))
            .expect("conversation should be created");
        let root = store.paths().root().to_string_lossy().to_string();
        let test_name = "ai::conversation::store::tests::independently_spawned_processes_append_without_losing_messages";
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                Command::new(std::env::current_exe().expect("test executable should exist"))
                    .arg("--exact")
                    .arg(test_name)
                    .arg("--nocapture")
                    .env(PROCESS_TEST_ROOT_ENV, &root)
                    .spawn()
                    .expect("child test process should start"),
            );
        }
        for mut child in children {
            let status = child.wait().expect("child test process should finish");
            assert!(status.success(), "child process exited with {status}");
        }

        let final_document = store
            .load_blocking("conv-process")
            .expect("conversation should remain valid");
        assert_eq!(final_document.messages.len(), 8);
        assert_eq!(final_document.revision, 8);
    }

    #[test]
    fn unsafe_ids_are_rejected_before_path_construction() {
        for id in ["../escape", "conv/nested", "conv\\nested", "conv\0id", ".."] {
            assert!(matches!(
                ConversationStore::from_root("/tmp/gib-test").load_blocking(id),
                Err(ConversationError::InvalidConversationId)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn conversation_directory_and_files_use_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let store = store("permissions");
        store
            .create_blocking(&conversation("conv-permissions"))
            .expect("conversation should be created");
        assert_eq!(
            fs::metadata(store.paths().root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.paths().conversations_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.paths().conversation_path("conv-permissions").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
