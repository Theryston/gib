use super::error::ConversationError;
use super::model::{
    Conversation, ConversationList, ConversationMessage, ConversationMessageRole, DurableContext,
    current_timestamp,
};
use super::store::ConversationStore;
use crate::ai::model::{AiConfigStore, ModelError, ModelPaths};
use rand_core::{OsRng, TryRngCore};

/// User-facing operations over the durable conversation store.
#[derive(Debug, Clone)]
pub(crate) struct ConversationService {
    store: ConversationStore,
}

impl ConversationService {
    pub(crate) fn new(store: ConversationStore) -> Self {
        Self { store }
    }

    pub(crate) fn default_store() -> Result<Self, ConversationError> {
        Ok(Self::new(ConversationStore::new()?))
    }

    pub(crate) fn store(&self) -> &ConversationStore {
        &self.store
    }

    pub(crate) async fn create(
        &self,
        title: Option<String>,
    ) -> Result<Conversation, ConversationError> {
        let title = normalize_title(title)?;
        let conversation = Conversation::new(generate_id("conv")?, title, current_timestamp());
        let store = self.store.clone();
        run_blocking(move || store.create_blocking(&conversation)).await
    }

    pub(crate) async fn list(&self) -> Result<ConversationList, ConversationError> {
        let store = self.store.clone();
        run_blocking(move || store.list_blocking()).await
    }

    pub(crate) async fn load(
        &self,
        conversation_id: String,
    ) -> Result<Conversation, ConversationError> {
        let store = self.store.clone();
        run_blocking(move || store.load_blocking(&conversation_id)).await
    }

    pub(crate) async fn append_message(
        &self,
        conversation_id: String,
        expected_revision: u64,
        role: ConversationMessageRole,
        content: String,
    ) -> Result<Conversation, ConversationError> {
        let message_id = generate_id("msg")?;
        let timestamp = current_timestamp();
        let store = self.store.clone();
        run_blocking(move || {
            store.mutate_blocking(&conversation_id, expected_revision, |conversation| {
                conversation.messages.push(ConversationMessage::new(
                    message_id, role, timestamp, content,
                ));
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn rename(
        &self,
        conversation_id: String,
        expected_revision: u64,
        title: String,
    ) -> Result<Conversation, ConversationError> {
        let title = normalize_title(Some(title))?;
        let store = self.store.clone();
        run_blocking(move || {
            store.mutate_blocking(&conversation_id, expected_revision, |conversation| {
                conversation.title = title;
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn replace_durable_context(
        &self,
        conversation_id: String,
        expected_revision: u64,
        durable_context: DurableContext,
    ) -> Result<Conversation, ConversationError> {
        let store = self.store.clone();
        run_blocking(move || {
            store.mutate_blocking(&conversation_id, expected_revision, |conversation| {
                conversation.durable_context = durable_context;
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn delete(
        &self,
        conversation_id: String,
        expected_revision: Option<u64>,
    ) -> Result<Conversation, ConversationError> {
        let config = self.config_store();
        let store = self.store.clone();
        let deleted = {
            let id = conversation_id.clone();
            run_blocking(move || store.delete_blocking(&id, expected_revision)).await?
        };

        let active_id = {
            let config = config.clone();
            run_blocking(move || {
                config
                    .active_conversation_id()
                    .map_err(|_| ConversationError::ConfigUnavailable)
            })
            .await?
        };
        if active_id.as_deref() == Some(conversation_id.as_str()) {
            config
                .set_active_conversation_id(None)
                .await
                .map_err(|_| ConversationError::ConfigUnavailable)?;
        }
        Ok(deleted)
    }

    pub(crate) async fn select_active(
        &self,
        conversation_id: String,
    ) -> Result<Conversation, ConversationError> {
        let conversation = self.load(conversation_id.clone()).await?;
        self.config_store()
            .set_active_conversation_id(Some(&conversation_id))
            .await
            .map_err(|_| ConversationError::ConfigUnavailable)?;
        match self.load(conversation_id.clone()).await {
            Ok(_) => Ok(conversation),
            Err(ConversationError::ConversationNotFound { .. }) => {
                self.config_store()
                    .set_active_conversation_id(None)
                    .await
                    .map_err(|_| ConversationError::ConfigUnavailable)?;
                Err(ConversationError::NoActiveConversation)
            }
            Err(_) => Err(ConversationError::ActiveConversationUnavailable {
                id: conversation_id,
            }),
        }
    }

    pub(crate) async fn active_conversation_id(&self) -> Result<Option<String>, ConversationError> {
        let config = self.config_store();
        run_blocking(move || {
            config
                .active_conversation_id()
                .map_err(|_| ConversationError::ConfigUnavailable)
        })
        .await
    }

    /// Resolve an explicit ID without changing global state, or resolve the
    /// configured active ID. A stale active ID is cleared deterministically.
    pub(crate) async fn resolve(
        &self,
        explicit_id: Option<String>,
    ) -> Result<Conversation, ConversationError> {
        if let Some(conversation_id) = explicit_id {
            return self.load(conversation_id).await;
        }

        let Some(active_id) = self.active_conversation_id().await? else {
            return Err(ConversationError::NoActiveConversation);
        };
        match self.load(active_id.clone()).await {
            Ok(conversation) => Ok(conversation),
            Err(ConversationError::ConversationNotFound { .. }) => {
                self.config_store()
                    .set_active_conversation_id(None)
                    .await
                    .map_err(|_| ConversationError::ConfigUnavailable)?;
                Err(ConversationError::NoActiveConversation)
            }
            Err(_) => Err(ConversationError::ActiveConversationUnavailable { id: active_id }),
        }
    }

    /// Return the active conversation when one is valid. A missing selected
    /// file is repaired by clearing the selection; a malformed file remains
    /// actionable and is never selected.
    pub(crate) async fn active(&self) -> Result<Option<Conversation>, ConversationError> {
        match self.resolve(None).await {
            Ok(conversation) => Ok(Some(conversation)),
            Err(ConversationError::NoActiveConversation) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn config_store(&self) -> AiConfigStore {
        AiConfigStore::new(ModelPaths::from_root(
            self.store.paths().root().to_path_buf(),
        ))
    }
}

fn normalize_title(title: Option<String>) -> Result<String, ConversationError> {
    let title = title.unwrap_or_else(|| super::model::DEFAULT_CONVERSATION_TITLE.to_string());
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(ConversationError::InvalidTitle);
    }
    Ok(title)
}

fn generate_id(prefix: &str) -> Result<String, ConversationError> {
    let mut random = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| ConversationError::io("generate conversation identifier"))?;
    let mut encoded = String::with_capacity(prefix.len() + 1 + random.len() * 2);
    encoded.push_str(prefix);
    encoded.push('-');
    for byte in random {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    super::paths::validate_conversation_id(&encoded)?;
    Ok(encoded)
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + value - 10),
        _ => unreachable!("hex digit input is limited to four bits"),
    }
}

async fn run_blocking<T, F>(function: F) -> Result<T, ConversationError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ConversationError> + Send + 'static,
{
    tokio::task::spawn_blocking(function)
        .await
        .map_err(|_| ConversationError::io("finish conversation operation"))?
}

impl From<ModelError> for ConversationError {
    fn from(_: ModelError) -> Self {
        Self::ConfigUnavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::conversation::model::{ConversationMessageRole, DurableContext};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn service(name: &str) -> (ConversationService, PathBuf) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gib-conversation-service-{name}-{}-{stamp}",
            std::process::id()
        ));
        (
            ConversationService::new(ConversationStore::from_root(root.clone())),
            root,
        )
    }

    #[tokio::test]
    async fn create_append_and_rename_keep_a_stable_file_id() {
        let (service, root) = service("lifecycle");
        let conversation = service
            .create(Some("Project notes".to_string()))
            .await
            .expect("conversation should be created");
        let id = conversation.conversation_id.clone();
        let appended = service
            .append_message(
                id.clone(),
                conversation.revision,
                ConversationMessageRole::User,
                "Remember this".to_string(),
            )
            .await
            .expect("message should be appended");
        let renamed = service
            .rename(id.clone(), appended.revision, "Renamed notes".to_string())
            .await
            .expect("conversation should be renamed");
        assert_eq!(renamed.conversation_id, id);
        assert_eq!(renamed.title, "Renamed notes");
        assert_eq!(renamed.revision, 2);
        assert!(
            root.join("conversations")
                .join(format!("{id}.json"))
                .is_file()
        );
        assert!(matches!(
            service
                .append_message(
                    id,
                    appended.revision,
                    ConversationMessageRole::Assistant,
                    "stale".to_string(),
                )
                .await,
            Err(ConversationError::RevisionConflict { .. })
        ));
        fs::remove_dir_all(root).expect("temporary state should be removed");
    }

    #[tokio::test]
    async fn active_selection_and_explicit_resolution_are_independent() {
        let (service, root) = service("active");
        let first = service
            .create(Some("First".to_string()))
            .await
            .expect("first conversation should be created");
        let second = service
            .create(Some("Second".to_string()))
            .await
            .expect("second conversation should be created");
        service
            .select_active(first.conversation_id.clone())
            .await
            .expect("first conversation should become active");
        let explicit = service
            .resolve(Some(second.conversation_id.clone()))
            .await
            .expect("explicit conversation should resolve");
        assert_eq!(explicit.conversation_id, second.conversation_id);
        assert_eq!(
            service.active_conversation_id().await.unwrap(),
            Some(first.conversation_id.clone())
        );
        let active = service
            .active()
            .await
            .expect("active conversation should load")
            .expect("an active conversation should exist");
        assert_eq!(active.conversation_id, first.conversation_id);
        fs::remove_dir_all(root).expect("temporary state should be removed");
    }

    #[tokio::test]
    async fn deleting_the_active_conversation_clears_global_state() {
        let (service, root) = service("delete-active");
        let conversation = service
            .create(None)
            .await
            .expect("conversation should be created");
        service
            .select_active(conversation.conversation_id.clone())
            .await
            .expect("conversation should become active");
        service
            .delete(
                conversation.conversation_id.clone(),
                Some(conversation.revision),
            )
            .await
            .expect("conversation should be deleted");
        assert_eq!(service.active_conversation_id().await.unwrap(), None);
        assert!(service.active().await.unwrap().is_none());
        assert!(
            !root
                .join("conversations")
                .join(format!("{}.json", conversation.conversation_id))
                .exists()
        );
        fs::remove_dir_all(root).expect("temporary state should be removed");
    }

    #[tokio::test]
    async fn durable_context_is_persisted_as_explicit_bounded_data() {
        let (service, root) = service("context");
        let conversation = service
            .create(None)
            .await
            .expect("conversation should be created");
        let context = DurableContext {
            summary: Some("A short summary".to_string()),
            user_preferences: BTreeMap::from([("language".to_string(), "English".to_string())]),
            artifact_refs: vec!["artifact-1".to_string()],
            evidence_refs: vec!["evidence-1".to_string()],
            facts: vec!["The user approved the plan.".to_string()],
        };
        let updated = service
            .replace_durable_context(
                conversation.conversation_id.clone(),
                conversation.revision,
                context,
            )
            .await
            .expect("durable context should be written");
        let loaded = service
            .load(updated.conversation_id.clone())
            .await
            .expect("conversation should be loadable");
        assert_eq!(
            loaded.durable_context.summary.as_deref(),
            Some("A short summary")
        );
        let encoded = fs::read_to_string(
            root.join("conversations")
                .join(format!("{}.json", updated.conversation_id)),
        )
        .expect("conversation document should be readable");
        assert!(!encoded.contains("hidden chain-of-thought"));
        assert!(!encoded.contains("tool_payload"));
        fs::remove_dir_all(root).expect("temporary state should be removed");
    }
}
