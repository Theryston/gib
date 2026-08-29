use crate::ai::conversation::{Conversation, ConversationMessageRole, ConversationMessageStatus};
use crate::ai::session::{
    AgentSession, ArtifactId, ArtifactKind, ArtifactRecord, AttemptLog, BudgetSnapshot,
    EvidenceRecord, EvidenceSourceKind, StopReason, canonical_bytes, hash_bytes, redact_text,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub(crate) const CONTEXT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ContextRole {
    Conversation,
    Routing,
    Search,
    HistoryExplanation,
    Restore,
}

impl fmt::Display for ContextRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conversation => "conversation",
            Self::Routing => "routing",
            Self::Search => "search",
            Self::HistoryExplanation => "history-explanation",
            Self::Restore => "restore",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ContextSourceType {
    UserRequest,
    ConversationMessage,
    DurableSummary,
    UserPreference,
    Capability,
    Artifact,
    Evidence,
    Attempt,
    Budget,
    RestoreState,
    Catalog,
    Limitation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TrustClass {
    Authoritative,
    UserProvided,
    Derived,
    ModelGenerated,
    Limitation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextLimits {
    pub(crate) max_bytes: usize,
    pub(crate) max_tokens: usize,
    pub(crate) max_items: usize,
    pub(crate) max_item_bytes: usize,
    pub(crate) max_messages: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_bytes: 32 * 1024,
            max_tokens: 8 * 1024,
            max_items: 128,
            max_item_bytes: 8 * 1024,
            max_messages: 12,
        }
    }
}

impl ContextLimits {
    fn validate(self) -> Result<(), ContextError> {
        if self.max_bytes == 0
            || self.max_tokens == 0
            || self.max_items == 0
            || self.max_item_bytes == 0
            || self.max_messages == 0
        {
            return Err(ContextError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextItem {
    pub(crate) schema_version: u32,
    pub(crate) item_id: String,
    pub(crate) role: ContextRole,
    pub(crate) source_type: ContextSourceType,
    pub(crate) source_id: String,
    pub(crate) trust: TrustClass,
    pub(crate) value: Value,
    pub(crate) byte_size: u64,
    pub(crate) token_estimate: u64,
    #[serde(default)]
    pub(crate) truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<crate::ai::session::EvidenceId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextWarning {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextBuildResult {
    pub(crate) schema_version: u32,
    pub(crate) role: ContextRole,
    pub(crate) items: Vec<ContextItem>,
    pub(crate) byte_size: u64,
    pub(crate) token_estimate: u64,
    pub(crate) omitted_item_count: usize,
    #[serde(default)]
    pub(crate) truncated: bool,
    #[serde(default)]
    pub(crate) warnings: Vec<ContextWarning>,
}

impl ContextBuildResult {
    pub(crate) fn validate(&self, limits: ContextLimits) -> Result<(), ContextError> {
        limits.validate()?;
        if self.schema_version != CONTEXT_SCHEMA_VERSION
            || self.items.len() > limits.max_items
            || self.byte_size as usize > limits.max_bytes
            || self.token_estimate as usize > limits.max_tokens
        {
            return Err(ContextError::InvalidResult);
        }
        let mut item_ids = BTreeSet::new();
        let mut bytes = 0_u64;
        let mut tokens = 0_u64;
        for item in &self.items {
            let encoded_item_bytes = encoded_size(&item.value)? as u64;
            if item.schema_version != CONTEXT_SCHEMA_VERSION
                || item.role != self.role
                || item.item_id.is_empty()
                || item.source_id.is_empty()
                || item.source_id.len() > 256
                || item.source_id.chars().any(char::is_control)
                || !item_ids.insert(item.item_id.clone())
                || item.byte_size != encoded_item_bytes
                || item.byte_size as usize > limits.max_item_bytes
                || item.token_estimate != token_estimate(item.byte_size as usize) as u64
                || item.truncated != item.truncation_reason.is_some()
                || duplicate_ids(&item.artifact_refs)
                || duplicate_ids(&item.evidence_refs)
            {
                return Err(ContextError::InvalidResult);
            }
            bytes = bytes.saturating_add(item.byte_size);
            tokens = tokens.saturating_add(item.token_estimate);
        }
        if bytes != self.byte_size || tokens != self.token_estimate {
            return Err(ContextError::InvalidResult);
        }
        let expected_truncated = self.omitted_item_count > 0
            || self.items.iter().any(|item| item.truncated)
            || self
                .warnings
                .iter()
                .any(|warning| warning.code == "context_item_truncated");
        if self.truncated != expected_truncated {
            return Err(ContextError::InvalidResult);
        }
        if self.warnings.iter().any(|warning| {
            warning.code.is_empty()
                || warning.code.len() > 96
                || warning.code.chars().any(char::is_control)
                || warning.message.is_empty()
                || warning.message.len() > 512
                || warning.message.chars().any(char::is_control)
                || warning.source_id.as_deref().is_some_and(|source_id| {
                    source_id.is_empty()
                        || source_id.len() > 256
                        || source_id.chars().any(char::is_control)
                })
        }) {
            return Err(ContextError::InvalidResult);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogContextRecord {
    pub(crate) source_id: String,
    pub(crate) status: Option<String>,
    pub(crate) value: Value,
    pub(crate) authoritative: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ContextInputs {
    /// This object is read only while building a context and is never put in
    /// WorkflowState or an OrchestratorEvent.
    pub(crate) conversation: Option<Conversation>,
    pub(crate) session: Option<AgentSession>,
    pub(crate) current_request: Option<String>,
    pub(crate) normalized_goal: Option<String>,
    pub(crate) previous_turn_context: Vec<String>,
    pub(crate) available_capabilities: Vec<String>,
    pub(crate) hypotheses: Vec<String>,
    pub(crate) artifacts: Vec<ArtifactRecord>,
    pub(crate) evidence: Vec<EvidenceRecord>,
    pub(crate) attempts: Vec<AttemptLog>,
    pub(crate) remaining_budget: Option<BudgetSnapshot>,
    pub(crate) catalog: Vec<CatalogContextRecord>,
    pub(crate) selected_revisions: Vec<String>,
    pub(crate) restore_preview: Option<ArtifactId>,
    pub(crate) risk_state: Option<String>,
    pub(crate) confirmation_state: Option<String>,
    pub(crate) verification_requirements: Vec<String>,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContextBuilder {
    limits: ContextLimits,
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self::new(ContextLimits::default()).expect("default context limits are valid")
    }
}

impl ContextBuilder {
    pub(crate) fn new(limits: ContextLimits) -> Result<Self, ContextError> {
        limits.validate()?;
        Ok(Self { limits })
    }

    pub(crate) fn limits(&self) -> ContextLimits {
        self.limits
    }

    pub(crate) fn build(
        &self,
        role: ContextRole,
        inputs: &ContextInputs,
    ) -> Result<ContextBuildResult, ContextError> {
        self.limits.validate()?;
        let mut candidates = Vec::new();
        match role {
            ContextRole::Conversation => self.build_conversation(inputs, &mut candidates),
            ContextRole::Routing => self.build_routing(inputs, &mut candidates),
            ContextRole::Search => self.build_search(inputs, &mut candidates),
            ContextRole::HistoryExplanation => self.build_history(inputs, &mut candidates),
            ContextRole::Restore => self.build_restore(inputs, &mut candidates),
        }
        let mut warnings = Vec::new();
        for limitation in &inputs.limitations {
            let limitation_index = candidates.len();
            add_candidate(
                &mut candidates,
                Candidate::new(
                    240,
                    role,
                    ContextSourceType::Limitation,
                    TrustClass::Limitation,
                    format!("limitation-{limitation_index}"),
                    Value::String(safe_text(limitation)),
                ),
            );
        }
        for record in &inputs.evidence {
            if record.status.is_limitation() {
                warnings.push(ContextWarning {
                    code: "evidence_limitation".to_string(),
                    message: "evidence source is degraded or unavailable".to_string(),
                    source_id: Some(record.evidence_id.to_string()),
                });
            }
        }
        for record in &inputs.catalog {
            if record.status.as_deref().is_some_and(|status| {
                let status = status.to_ascii_lowercase();
                status.contains("degraded")
                    || status.contains("pending")
                    || status.contains("unavailable")
            }) {
                warnings.push(ContextWarning {
                    code: "catalog_limitation".to_string(),
                    message: "catalog source status limits the available evidence".to_string(),
                    source_id: Some(safe_text(&record.source_id)),
                });
            }
        }

        candidates.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        let mut items = Vec::new();
        let mut omitted_item_count = 0_usize;
        let mut byte_size = 0_usize;
        let mut token_estimate_total = 0_usize;
        let mut seen = BTreeSet::new();

        for candidate in candidates {
            let item_id = candidate.item_id();
            if !seen.insert(item_id.clone()) {
                continue;
            }
            if items.len() >= self.limits.max_items {
                omitted_item_count = omitted_item_count.saturating_add(1);
                continue;
            }
            let (value, truncated) = match fit_value(&candidate.value, self.limits.max_item_bytes) {
                Some(value) => value,
                None => {
                    omitted_item_count = omitted_item_count.saturating_add(1);
                    warnings.push(ContextWarning {
                        code: "context_item_too_large".to_string(),
                        message: "a context item could not fit within the item limit".to_string(),
                        source_id: Some(candidate.source_id.clone()),
                    });
                    continue;
                }
            };
            let bytes = encoded_size(&value)?;
            let tokens = token_estimate(bytes);
            if byte_size.saturating_add(bytes) > self.limits.max_bytes
                || token_estimate_total.saturating_add(tokens) > self.limits.max_tokens
            {
                omitted_item_count = omitted_item_count.saturating_add(1);
                continue;
            }
            if truncated {
                warnings.push(ContextWarning {
                    code: "context_item_truncated".to_string(),
                    message: "a context item was deterministically truncated".to_string(),
                    source_id: Some(candidate.source_id.clone()),
                });
            }
            let item = ContextItem {
                schema_version: CONTEXT_SCHEMA_VERSION,
                item_id,
                role,
                source_type: candidate.source_type,
                source_id: candidate.source_id,
                trust: candidate.trust,
                value,
                byte_size: bytes as u64,
                token_estimate: tokens as u64,
                truncated,
                truncation_reason: truncated.then(|| "context_item_limit".to_string()),
                artifact_refs: candidate.artifact_refs,
                evidence_refs: candidate.evidence_refs,
            };
            byte_size = byte_size.saturating_add(bytes);
            token_estimate_total = token_estimate_total.saturating_add(tokens);
            items.push(item);
        }

        if omitted_item_count > 0 {
            warnings.push(ContextWarning {
                code: "context_items_omitted".to_string(),
                message: format!(
                    "{omitted_item_count} context item(s) were omitted by bounded packing"
                ),
                source_id: None,
            });
        }
        warnings.sort_by(|left, right| {
            (left.code.as_str(), left.source_id.as_deref().unwrap_or("")).cmp(&(
                right.code.as_str(),
                right.source_id.as_deref().unwrap_or(""),
            ))
        });
        let result = ContextBuildResult {
            schema_version: CONTEXT_SCHEMA_VERSION,
            role,
            items,
            byte_size: byte_size as u64,
            token_estimate: token_estimate_total as u64,
            omitted_item_count,
            truncated: omitted_item_count > 0
                || warnings
                    .iter()
                    .any(|warning| warning.code == "context_item_truncated"),
            warnings,
        };
        result.validate(self.limits)?;
        Ok(result)
    }

    fn build_conversation(&self, inputs: &ContextInputs, candidates: &mut Vec<Candidate>) {
        add_request(
            candidates,
            ContextRole::Conversation,
            inputs.current_request.as_deref(),
            0,
        );
        if let Some(conversation) = &inputs.conversation {
            if let Some(summary) = &conversation.durable_context.summary {
                add_candidate(
                    candidates,
                    Candidate::new(
                        5,
                        ContextRole::Conversation,
                        ContextSourceType::DurableSummary,
                        TrustClass::Derived,
                        "durable-summary",
                        Value::String(safe_text(summary)),
                    ),
                );
            }
            for (key, value) in &conversation.durable_context.user_preferences {
                add_candidate(
                    candidates,
                    Candidate::new(
                        10,
                        ContextRole::Conversation,
                        ContextSourceType::UserPreference,
                        TrustClass::UserProvided,
                        format!("preference-{key}"),
                        json!({ "key": safe_text(key), "value": safe_text(value) }),
                    ),
                );
            }
            let start = conversation
                .messages
                .len()
                .saturating_sub(self.limits.max_messages);
            for (offset, message) in conversation.messages[start..].iter().enumerate() {
                let trust = match message.role {
                    ConversationMessageRole::User => TrustClass::UserProvided,
                    ConversationMessageRole::Assistant => TrustClass::ModelGenerated,
                };
                let status = match message.status {
                    ConversationMessageStatus::Complete => "complete",
                    ConversationMessageStatus::Interrupted => "interrupted",
                    ConversationMessageStatus::Pending => "pending",
                };
                let content = match message.role {
                    ConversationMessageRole::User => message.content.clone(),
                    ConversationMessageRole::Assistant => safe_text(&message.content),
                };
                add_candidate(
                    candidates,
                    Candidate::new(
                        20 + offset,
                        ContextRole::Conversation,
                        ContextSourceType::ConversationMessage,
                        trust,
                        message.message_id.clone(),
                        json!({
                            "role": message.role,
                            "status": status,
                            "timestamp": message.timestamp,
                            "content": content,
                        }),
                    ),
                );
            }
            add_string_list(
                candidates,
                ContextRole::Conversation,
                ContextSourceType::Evidence,
                TrustClass::Derived,
                180,
                "durable-evidence",
                &conversation.durable_context.evidence_refs,
            );
            add_string_list(
                candidates,
                ContextRole::Conversation,
                ContextSourceType::Artifact,
                TrustClass::Derived,
                181,
                "durable-artifacts",
                &conversation.durable_context.artifact_refs,
            );
            add_string_list(
                candidates,
                ContextRole::Conversation,
                ContextSourceType::Evidence,
                TrustClass::Derived,
                182,
                "durable-facts",
                &conversation.durable_context.facts,
            );
        }
    }

    fn build_routing(&self, inputs: &ContextInputs, candidates: &mut Vec<Candidate>) {
        add_request(
            candidates,
            ContextRole::Routing,
            inputs.current_request.as_deref(),
            0,
        );
        add_text(
            candidates,
            ContextRole::Routing,
            ContextSourceType::Artifact,
            TrustClass::Derived,
            1,
            "normalized-goal",
            inputs.normalized_goal.as_deref(),
        );
        for (index, value) in inputs.previous_turn_context.iter().take(4).enumerate() {
            add_candidate(
                candidates,
                Candidate::new(
                    10 + index,
                    ContextRole::Routing,
                    ContextSourceType::ConversationMessage,
                    TrustClass::Derived,
                    format!("prior-{index}"),
                    Value::String(safe_text(value)),
                ),
            );
        }
        add_capabilities(
            candidates,
            ContextRole::Routing,
            &inputs.available_capabilities,
        );
    }

    fn build_search(&self, inputs: &ContextInputs, candidates: &mut Vec<Candidate>) {
        add_text(
            candidates,
            ContextRole::Search,
            ContextSourceType::UserRequest,
            TrustClass::UserProvided,
            0,
            "normalized-goal",
            inputs
                .normalized_goal
                .as_deref()
                .or(inputs.current_request.as_deref()),
        );
        for (index, hypothesis) in inputs.hypotheses.iter().enumerate() {
            add_candidate(
                candidates,
                Candidate::new(
                    10 + index,
                    ContextRole::Search,
                    ContextSourceType::Artifact,
                    TrustClass::Derived,
                    format!("hypothesis-{index}"),
                    Value::String(safe_text(hypothesis)),
                ),
            );
        }
        add_attempts(candidates, ContextRole::Search, &inputs.attempts);
        add_artifacts(candidates, ContextRole::Search, &inputs.artifacts, 80);
        add_catalog(candidates, ContextRole::Search, &inputs.catalog, 100);
        add_budget(
            candidates,
            ContextRole::Search,
            inputs.remaining_budget.as_ref(),
            200,
        );
    }

    fn build_history(&self, inputs: &ContextInputs, candidates: &mut Vec<Candidate>) {
        add_text(
            candidates,
            ContextRole::HistoryExplanation,
            ContextSourceType::UserRequest,
            TrustClass::UserProvided,
            0,
            "request",
            inputs
                .current_request
                .as_deref()
                .or(inputs.normalized_goal.as_deref()),
        );
        add_evidence(
            candidates,
            ContextRole::HistoryExplanation,
            &inputs.evidence,
            20,
        );
        add_artifacts(
            candidates,
            ContextRole::HistoryExplanation,
            &inputs.artifacts,
            80,
        );
        add_catalog(
            candidates,
            ContextRole::HistoryExplanation,
            &inputs.catalog,
            100,
        );
        add_attempts(
            candidates,
            ContextRole::HistoryExplanation,
            &inputs.attempts,
        );
        add_budget(
            candidates,
            ContextRole::HistoryExplanation,
            inputs.remaining_budget.as_ref(),
            200,
        );
    }

    fn build_restore(&self, inputs: &ContextInputs, candidates: &mut Vec<Candidate>) {
        add_request(
            candidates,
            ContextRole::Restore,
            inputs.current_request.as_deref(),
            0,
        );
        add_string_list(
            candidates,
            ContextRole::Restore,
            ContextSourceType::RestoreState,
            TrustClass::Authoritative,
            20,
            "selected-revision",
            &inputs.selected_revisions,
        );
        if let Some(preview) = &inputs.restore_preview {
            add_candidate(
                candidates,
                Candidate::new(
                    40,
                    ContextRole::Restore,
                    ContextSourceType::RestoreState,
                    TrustClass::Authoritative,
                    "restore-preview",
                    json!({ "artifact_id": preview }),
                )
                .with_artifact(preview.clone()),
            );
        }
        add_text(
            candidates,
            ContextRole::Restore,
            ContextSourceType::RestoreState,
            TrustClass::Derived,
            60,
            "risk-state",
            inputs.risk_state.as_deref(),
        );
        add_text(
            candidates,
            ContextRole::Restore,
            ContextSourceType::RestoreState,
            TrustClass::Authoritative,
            65,
            "confirmation-state",
            inputs.confirmation_state.as_deref(),
        );
        add_string_list(
            candidates,
            ContextRole::Restore,
            ContextSourceType::RestoreState,
            TrustClass::Authoritative,
            70,
            "verification-requirement",
            &inputs.verification_requirements,
        );
        add_artifacts(candidates, ContextRole::Restore, &inputs.artifacts, 100);
        add_evidence(candidates, ContextRole::Restore, &inputs.evidence, 120);
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    priority: usize,
    role: ContextRole,
    source_type: ContextSourceType,
    trust: TrustClass,
    source_id: String,
    value: Value,
    artifact_refs: Vec<ArtifactId>,
    evidence_refs: Vec<crate::ai::session::EvidenceId>,
}

impl Candidate {
    fn new(
        priority: usize,
        role: ContextRole,
        source_type: ContextSourceType,
        trust: TrustClass,
        source_id: impl Into<String>,
        value: Value,
    ) -> Self {
        Self {
            priority,
            role,
            source_type,
            trust,
            source_id: safe_text(&source_id.into()),
            value,
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
        }
    }

    fn with_artifact(mut self, artifact_id: ArtifactId) -> Self {
        self.artifact_refs.push(artifact_id);
        self
    }

    fn with_evidence(mut self, evidence_id: crate::ai::session::EvidenceId) -> Self {
        self.evidence_refs.push(evidence_id);
        self
    }

    fn item_id(&self) -> String {
        let value = json!({
            "role": self.role,
            "source_type": self.source_type,
            "source_id": self.source_id,
            "trust": self.trust,
            "value": self.value,
        });
        hash_bytes("ctx-", &canonical_bytes(&value))
    }

    fn sort_key(&self) -> (usize, ContextSourceType, String, String) {
        (
            self.priority,
            self.source_type,
            self.source_id.clone(),
            self.item_id(),
        )
    }
}

fn add_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) {
    candidates.push(candidate);
}

fn add_request(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    value: Option<&str>,
    priority: usize,
) {
    add_text(
        candidates,
        role,
        ContextSourceType::UserRequest,
        TrustClass::UserProvided,
        priority,
        "current-request",
        value,
    );
}

fn add_text(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    source_type: ContextSourceType,
    trust: TrustClass,
    priority: usize,
    source_id: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        let value = if trust == TrustClass::UserProvided {
            value.to_string()
        } else {
            safe_text(value)
        };
        add_candidate(
            candidates,
            Candidate::new(
                priority,
                role,
                source_type,
                trust,
                source_id,
                Value::String(value),
            ),
        );
    }
}

fn add_string_list(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    source_type: ContextSourceType,
    trust: TrustClass,
    priority: usize,
    prefix: &str,
    values: &[String],
) {
    for (index, value) in values.iter().enumerate() {
        add_candidate(
            candidates,
            Candidate::new(
                priority + index,
                role,
                source_type,
                trust,
                format!("{prefix}-{index}"),
                Value::String(safe_text(value)),
            ),
        );
    }
}

fn add_capabilities(candidates: &mut Vec<Candidate>, role: ContextRole, capabilities: &[String]) {
    let mut capabilities = capabilities.to_vec();
    capabilities.sort();
    capabilities.dedup();
    add_string_list(
        candidates,
        role,
        ContextSourceType::Capability,
        TrustClass::Authoritative,
        20,
        "capability",
        &capabilities,
    );
}

fn add_attempts(candidates: &mut Vec<Candidate>, role: ContextRole, attempts: &[AttemptLog]) {
    let mut grouped: BTreeMap<String, (BTreeSet<String>, usize)> = BTreeMap::new();
    for attempt in attempts {
        let entry = grouped
            .entry(attempt.canonical_fingerprint.clone())
            .or_default();
        entry.0.insert(attempt.outcome_string().to_string());
        entry.1 = entry.1.saturating_add(1);
    }
    for (index, (fingerprint, (outcomes, count))) in grouped.into_iter().enumerate() {
        add_candidate(
            candidates,
            Candidate::new(
                140 + index,
                role,
                ContextSourceType::Attempt,
                TrustClass::Derived,
                format!("attempt-{fingerprint}"),
                json!({
                    "fingerprint": fingerprint,
                    "count": count,
                    "outcomes": outcomes.into_iter().collect::<Vec<_>>(),
                }),
            ),
        );
    }
}

fn add_artifacts(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    artifacts: &[ArtifactRecord],
    priority: usize,
) {
    let mut artifacts = artifacts.to_vec();
    artifacts.sort_by(|left, right| {
        left.header
            .artifact_id
            .cmp(&right.header.artifact_id)
            .then_with(|| artifact_sort_key(left).cmp(&artifact_sort_key(right)))
    });
    artifacts.dedup_by(|left, right| left.header.artifact_id == right.header.artifact_id);
    for (index, artifact) in artifacts.into_iter().enumerate() {
        let trust = match artifact.header.kind {
            ArtifactKind::CatalogPage | ArtifactKind::CatalogSummary => TrustClass::Authoritative,
            _ => TrustClass::Derived,
        };
        add_candidate(
            candidates,
            Candidate::new(
                priority + index,
                role,
                ContextSourceType::Artifact,
                trust,
                artifact.header.artifact_id.to_string(),
                json!({
                    "artifact_id": artifact.header.artifact_id,
                    "kind": artifact.header.kind,
                    "created_at": artifact.header.created_at,
                    "content_hash": artifact.header.content_hash,
                    "payload": safe_context_value(&artifact.payload),
                }),
            )
            .with_artifact(artifact.header.artifact_id),
        );
    }
}

fn artifact_sort_key(record: &ArtifactRecord) -> Vec<u8> {
    serde_json::to_vec(record).unwrap_or_default()
}

fn add_evidence(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    evidence: &[EvidenceRecord],
    priority: usize,
) {
    let mut evidence = evidence.to_vec();
    evidence.sort_by(|left, right| {
        left.evidence_id.cmp(&right.evidence_id).then_with(|| {
            evidence_rank(right)
                .cmp(&evidence_rank(left))
                .then_with(|| evidence_sort_key(left).cmp(&evidence_sort_key(right)))
        })
    });
    evidence.dedup_by(|left, right| left.evidence_id == right.evidence_id);
    for (index, record) in evidence.into_iter().enumerate() {
        let trust = if record.status.is_limitation() {
            TrustClass::Limitation
        } else {
            match record.source.kind {
                EvidenceSourceKind::Catalog
                | EvidenceSourceKind::Filesystem
                | EvidenceSourceKind::Backup
                | EvidenceSourceKind::Restore => TrustClass::Authoritative,
                EvidenceSourceKind::User => TrustClass::UserProvided,
                EvidenceSourceKind::Model => TrustClass::ModelGenerated,
                EvidenceSourceKind::Tool
                | EvidenceSourceKind::Conversation
                | EvidenceSourceKind::Unknown => TrustClass::Derived,
            }
        };
        let mut value = Map::new();
        value.insert("evidence_id".to_string(), json!(record.evidence_id));
        value.insert("kind".to_string(), json!(record.kind));
        value.insert("status".to_string(), json!(record.status));
        value.insert("confidence".to_string(), json!(record.confidence));
        value.insert(
            "fact_or_inference".to_string(),
            json!(record.fact_or_inference),
        );
        value.insert("created_at".to_string(), json!(record.created_at));
        if let Some(observed_at) = record.observed_at {
            value.insert("observed_at".to_string(), json!(observed_at));
        }
        if let Some(statement) = record.statement {
            value.insert(
                "statement".to_string(),
                Value::String(safe_text(&statement)),
            );
        }
        if let Some(payload) = record.payload {
            value.insert("payload".to_string(), safe_context_value(&payload));
        }
        value.insert("source_kind".to_string(), json!(record.source.kind));
        value.insert(
            "source_id".to_string(),
            Value::String(safe_text(&record.source.source_id)),
        );
        value.insert("artifact_refs".to_string(), json!(record.artifact_refs));
        value.insert("attempt_refs".to_string(), json!(record.attempt_refs));
        value.insert(
            "supporting_evidence_ids".to_string(),
            json!(record.supporting_evidence_ids),
        );
        if record.status.is_limitation() {
            value.insert("limitation".to_string(), Value::Bool(true));
        }
        let candidate = Candidate::new(
            priority + index,
            role,
            ContextSourceType::Evidence,
            trust,
            record.evidence_id.to_string(),
            Value::Object(value),
        )
        .with_evidence(record.evidence_id.clone());
        add_candidate(candidates, candidate);
    }
}

fn evidence_rank(record: &EvidenceRecord) -> (u8, u8) {
    let authoritative = u8::from(matches!(
        record.source.kind,
        EvidenceSourceKind::Catalog
            | EvidenceSourceKind::Filesystem
            | EvidenceSourceKind::Backup
            | EvidenceSourceKind::Restore
    ));
    let limitation = u8::from(record.status.is_limitation());
    (authoritative, limitation)
}

fn evidence_sort_key(record: &EvidenceRecord) -> Vec<u8> {
    serde_json::to_vec(record).unwrap_or_default()
}

fn add_catalog(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    catalog: &[CatalogContextRecord],
    priority: usize,
) {
    let mut catalog = catalog.to_vec();
    catalog.sort_by(|left, right| {
        left.source_id.cmp(&right.source_id).then_with(|| {
            u8::from(right.authoritative)
                .cmp(&u8::from(left.authoritative))
                .then_with(|| catalog_sort_key(left).cmp(&catalog_sort_key(right)))
        })
    });
    catalog.dedup_by(|left, right| left.source_id == right.source_id);
    for (index, record) in catalog.into_iter().enumerate() {
        let mut value = Map::new();
        value.insert(
            "source_id".to_string(),
            Value::String(safe_text(&record.source_id)),
        );
        if let Some(status) = record.status {
            value.insert("status".to_string(), Value::String(safe_text(&status)));
        }
        value.insert("value".to_string(), safe_context_value(&record.value));
        add_candidate(
            candidates,
            Candidate::new(
                priority + index,
                role,
                ContextSourceType::Catalog,
                if record.authoritative {
                    TrustClass::Authoritative
                } else {
                    TrustClass::Derived
                },
                safe_text(&record.source_id),
                Value::Object(value),
            ),
        );
    }
}

fn catalog_sort_key(record: &CatalogContextRecord) -> Vec<u8> {
    let value = json!({
        "status": record.status,
        "value": record.value,
        "authoritative": record.authoritative,
    });
    canonical_bytes(&value)
}

fn add_budget(
    candidates: &mut Vec<Candidate>,
    role: ContextRole,
    budget: Option<&BudgetSnapshot>,
    priority: usize,
) {
    if let Some(budget) = budget {
        add_candidate(
            candidates,
            Candidate::new(
                priority,
                role,
                ContextSourceType::Budget,
                TrustClass::Authoritative,
                "remaining-budget",
                json!({
                    "remaining": budget.remaining,
                    "deadline_at": budget.deadline_at,
                }),
            ),
        );
    }
}

fn fit_value(value: &Value, limit: usize) -> Option<(Value, bool)> {
    let bytes = encoded_size(value).ok()?;
    if bytes <= limit {
        return Some((value.clone(), false));
    }
    let original_bytes = bytes;
    let preview = serde_json::to_string(value).ok()?;
    let mut low = 0_usize;
    let mut high = preview.chars().count();
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let mut marker = Map::new();
        marker.insert("truncated".to_string(), Value::Bool(true));
        marker.insert(
            "original_bytes".to_string(),
            Value::Number((original_bytes as u64).into()),
        );
        marker.insert(
            "preview".to_string(),
            Value::String(preview.chars().take(middle).collect()),
        );
        let candidate = Value::Object(marker);
        let candidate_bytes = encoded_size(&candidate).ok()?;
        if candidate_bytes <= limit {
            best = Some(candidate);
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best.map(|value| (value, true))
}

fn encoded_size(value: &Value) -> Result<usize, ContextError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| ContextError::Serialization)
}

fn token_estimate(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

fn duplicate_ids<T: Ord + Clone>(values: &[T]) -> bool {
    let mut values = values.to_vec();
    values.sort();
    values.windows(2).any(|window| window[0] == window[1])
}

fn safe_text(value: &str) -> String {
    redact_text(value).chars().take(4 * 1024).collect()
}

fn safe_context_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut safe = Map::new();
            for (key, value) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized.as_str(),
                    "prompt"
                        | "prompt_body"
                        | "prompt_content"
                        | "hidden_reasoning"
                        | "chain_of_thought"
                        | "scratchpad"
                        | "raw_output"
                        | "native_diagnostic"
                        | "native_log"
                ) || normalized.contains("reasoning")
                {
                    continue;
                }
                safe.insert(key.clone(), safe_context_value(value));
            }
            Value::Object(safe)
        }
        Value::Array(values) => Value::Array(values.iter().map(safe_context_value).collect()),
        Value::String(value) => Value::String(safe_text(value)),
        _ => value.clone(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum ContextError {
    InvalidLimits,
    InvalidResult,
    Serialization,
}

impl ContextError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidLimits => "invalid_context_limits",
            Self::InvalidResult => "invalid_context_result",
            Self::Serialization => "context_serialization_error",
        }
    }
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "context limits are invalid",
            Self::InvalidResult => "context build result is invalid",
            Self::Serialization => "context item could not be sized",
        })
    }
}

impl std::error::Error for ContextError {}

#[allow(dead_code)]
fn _stop_reason_is_safe(reason: StopReason) -> bool {
    matches!(
        reason,
        StopReason::GoalSatisfied
            | StopReason::NoCandidate
            | StopReason::Ambiguous
            | StopReason::EvidenceInsufficient
            | StopReason::BudgetExhausted
            | StopReason::UserCancelled
            | StopReason::SafetyConfirmationRequired
            | StopReason::DependencyFailed
            | StopReason::InternalError
    )
}
