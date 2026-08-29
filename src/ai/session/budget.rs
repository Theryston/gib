use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) const BUDGET_SCHEMA_VERSION: u32 = 1;

const DEFAULT_MODEL_CALLS: u64 = 8;
const DEFAULT_OUTPUT_TOKENS: u64 = 4_096;
const DEFAULT_TOOL_CALLS: u64 = 32;
const DEFAULT_SEARCH_ACTIONS: u64 = 32;
const DEFAULT_CANDIDATES: u64 = 256;
const DEFAULT_CONTEXT_BYTES: u64 = 512 * 1024;
const DEFAULT_CONTEXT_TOKENS: u64 = 32_768;
const DEFAULT_RETRIES: u64 = 8;
const DEFAULT_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_EVIDENCE_BYTES: u64 = 4 * 1024 * 1024;

/// Explicit resource ceilings for one agent turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentBudgetLimits {
    /// An RFC 3339 UTC deadline. It is evaluated at every checked consume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wall_clock_deadline: Option<String>,
    pub(crate) max_model_calls: u64,
    pub(crate) max_output_tokens: u64,
    pub(crate) max_tool_calls: u64,
    pub(crate) max_search_actions: u64,
    pub(crate) max_candidates: u64,
    pub(crate) max_context_bytes: u64,
    pub(crate) max_context_tokens: u64,
    pub(crate) max_retries: u64,
    pub(crate) max_artifact_bytes: u64,
    pub(crate) max_evidence_bytes: u64,
}

impl Default for AgentBudgetLimits {
    fn default() -> Self {
        Self {
            wall_clock_deadline: None,
            max_model_calls: DEFAULT_MODEL_CALLS,
            max_output_tokens: DEFAULT_OUTPUT_TOKENS,
            max_tool_calls: DEFAULT_TOOL_CALLS,
            max_search_actions: DEFAULT_SEARCH_ACTIONS,
            max_candidates: DEFAULT_CANDIDATES,
            max_context_bytes: DEFAULT_CONTEXT_BYTES,
            max_context_tokens: DEFAULT_CONTEXT_TOKENS,
            max_retries: DEFAULT_RETRIES,
            max_artifact_bytes: DEFAULT_ARTIFACT_BYTES,
            max_evidence_bytes: DEFAULT_EVIDENCE_BYTES,
        }
    }
}

impl AgentBudgetLimits {
    pub(crate) fn with_deadline_after(mut self, duration: Duration) -> Self {
        let deadline = Utc::now()
            + chrono::Duration::from_std(duration).unwrap_or_else(|_| chrono::Duration::zero());
        self.wall_clock_deadline = Some(deadline.to_rfc3339_opts(SecondsFormat::Millis, true));
        self
    }

    pub(crate) fn with_deadline(mut self, deadline: impl Into<String>) -> Self {
        self.wall_clock_deadline = Some(deadline.into());
        self
    }

    pub(crate) fn validate(&self) -> Result<(), BudgetError> {
        if let Some(deadline) = &self.wall_clock_deadline {
            parse_deadline(deadline)?;
        }
        Ok(())
    }
}

/// A charge for one bounded operation. All dimensions are explicit so a
/// caller cannot hide a resource-consuming action in an untyped log field.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetCost {
    pub(crate) wall_clock_ms: u64,
    pub(crate) model_calls: u64,
    pub(crate) output_tokens: u64,
    pub(crate) tool_calls: u64,
    pub(crate) search_actions: u64,
    pub(crate) candidates: u64,
    pub(crate) context_bytes: u64,
    pub(crate) context_tokens: u64,
    pub(crate) retries: u64,
    pub(crate) artifact_bytes: u64,
    pub(crate) evidence_bytes: u64,
}

impl BudgetCost {
    pub(crate) fn model_call(output_tokens: u64) -> Self {
        Self {
            model_calls: 1,
            output_tokens,
            ..Self::default()
        }
    }

    pub(crate) fn tool_call() -> Self {
        Self {
            tool_calls: 1,
            ..Self::default()
        }
    }

    pub(crate) fn search_action() -> Self {
        Self {
            search_actions: 1,
            ..Self::default()
        }
    }

    pub(crate) fn for_artifact(bytes: u64) -> Self {
        Self {
            artifact_bytes: bytes,
            ..Self::default()
        }
    }

    pub(crate) fn for_evidence(bytes: u64) -> Self {
        Self {
            evidence_bytes: bytes,
            ..Self::default()
        }
    }

    fn is_zero(self) -> bool {
        self == Self::default()
    }
}

/// Consumed counters. `remaining` is represented separately in snapshots and
/// persisted budget state to make accounting auditable without recomputing it
/// from an opaque log.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetUsage {
    pub(crate) wall_clock_ms: u64,
    pub(crate) model_calls: u64,
    pub(crate) output_tokens: u64,
    pub(crate) tool_calls: u64,
    pub(crate) search_actions: u64,
    pub(crate) candidates: u64,
    pub(crate) context_bytes: u64,
    pub(crate) context_tokens: u64,
    pub(crate) retries: u64,
    pub(crate) artifact_bytes: u64,
    pub(crate) evidence_bytes: u64,
}

impl BudgetUsage {
    fn add_cost(&mut self, cost: BudgetCost) {
        self.wall_clock_ms = self.wall_clock_ms.saturating_add(cost.wall_clock_ms);
        self.model_calls = self.model_calls.saturating_add(cost.model_calls);
        self.output_tokens = self.output_tokens.saturating_add(cost.output_tokens);
        self.tool_calls = self.tool_calls.saturating_add(cost.tool_calls);
        self.search_actions = self.search_actions.saturating_add(cost.search_actions);
        self.candidates = self.candidates.saturating_add(cost.candidates);
        self.context_bytes = self.context_bytes.saturating_add(cost.context_bytes);
        self.context_tokens = self.context_tokens.saturating_add(cost.context_tokens);
        self.retries = self.retries.saturating_add(cost.retries);
        self.artifact_bytes = self.artifact_bytes.saturating_add(cost.artifact_bytes);
        self.evidence_bytes = self.evidence_bytes.saturating_add(cost.evidence_bytes);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BudgetDimension {
    ModelCalls,
    OutputTokens,
    ToolCalls,
    SearchActions,
    Candidates,
    ContextBytes,
    ContextTokens,
    Retries,
    ArtifactBytes,
    EvidenceBytes,
    WallClockDeadline,
}

impl BudgetDimension {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ModelCalls => "model_calls",
            Self::OutputTokens => "output_tokens",
            Self::ToolCalls => "tool_calls",
            Self::SearchActions => "search_actions",
            Self::Candidates => "candidates",
            Self::ContextBytes => "context_bytes",
            Self::ContextTokens => "context_tokens",
            Self::Retries => "retries",
            Self::ArtifactBytes => "artifact_bytes",
            Self::EvidenceBytes => "evidence_bytes",
            Self::WallClockDeadline => "wall_clock_deadline",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum BudgetError {
    InvalidCost {
        dimension: BudgetDimension,
    },
    Exhausted {
        dimension: BudgetDimension,
        requested: u64,
        remaining: u64,
    },
    DeadlineExceeded,
    InvalidDeadline,
    StateInconsistent,
}

impl BudgetError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidCost { .. } => "invalid_budget_cost",
            Self::Exhausted { .. } => "budget_exhausted",
            Self::DeadlineExceeded => "budget_deadline_exceeded",
            Self::InvalidDeadline => "invalid_budget_deadline",
            Self::StateInconsistent => "budget_state_inconsistent",
        }
    }
}

impl fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCost { dimension } => {
                write!(
                    formatter,
                    "the requested {dimension:?} budget cost is invalid"
                )
            }
            Self::Exhausted {
                dimension,
                requested,
                remaining,
            } => write!(
                formatter,
                "budget dimension {} cannot consume {requested}; {remaining} remains",
                dimension.as_str()
            ),
            Self::DeadlineExceeded => {
                formatter.write_str("the agent session wall-clock deadline has passed")
            }
            Self::InvalidDeadline => {
                formatter.write_str("the agent session wall-clock deadline is invalid")
            }
            Self::StateInconsistent => {
                formatter.write_str("the agent session budget state is inconsistent")
            }
        }
    }
}

impl std::error::Error for BudgetError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetSnapshot {
    pub(crate) consumed: BudgetUsage,
    pub(crate) remaining: BudgetUsage,
    pub(crate) deadline_at: Option<String>,
}

#[derive(Debug, Clone)]
struct BudgetState {
    consumed: BudgetUsage,
    remaining: BudgetUsage,
}

/// A thread-safe checked account for one session.
///
/// Clones share the same account, so concurrent model/tool workers cannot
/// overspend a dimension by racing separate local counters. Serialization
/// takes a consistent snapshot of both consumed and remaining values.
#[derive(Debug, Clone)]
pub(crate) struct AgentBudget {
    limits: AgentBudgetLimits,
    state: Arc<Mutex<BudgetState>>,
}

impl PartialEq for AgentBudget {
    fn eq(&self, other: &Self) -> bool {
        let left = self.snapshot().ok();
        let right = other.snapshot().ok();
        self.limits == other.limits
            && left.zip(right).is_some_and(|(left, right)| {
                left.consumed == right.consumed
                    && usage_without_deadline(left.remaining)
                        == usage_without_deadline(right.remaining)
                    && left.deadline_at == right.deadline_at
            })
    }
}

fn usage_without_deadline(mut usage: BudgetUsage) -> BudgetUsage {
    usage.wall_clock_ms = 0;
    usage
}

impl Eq for AgentBudget {}

impl AgentBudget {
    pub(crate) fn new(limits: AgentBudgetLimits) -> Result<Self, BudgetError> {
        limits.validate()?;
        let remaining = initial_remaining(&limits)?;
        Ok(Self {
            limits,
            state: Arc::new(Mutex::new(BudgetState {
                consumed: BudgetUsage::default(),
                remaining,
            })),
        })
    }

    pub(crate) fn default_budget() -> Self {
        Self::new(AgentBudgetLimits::default()).expect("default agent budget must be valid")
    }

    pub(crate) fn limits(&self) -> AgentBudgetLimits {
        self.limits.clone()
    }

    pub(crate) fn consumed(&self) -> Result<BudgetUsage, BudgetError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BudgetError::StateInconsistent)?;
        Ok(state.consumed)
    }

    pub(crate) fn remaining(&self) -> Result<BudgetUsage, BudgetError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BudgetError::StateInconsistent)?;
        Ok(current_remaining(&self.limits, &state.remaining))
    }

    pub(crate) fn snapshot(&self) -> Result<BudgetSnapshot, BudgetError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BudgetError::StateInconsistent)?;
        Ok(BudgetSnapshot {
            consumed: state.consumed,
            remaining: current_remaining(&self.limits, &state.remaining),
            deadline_at: self.limits.wall_clock_deadline.clone(),
        })
    }

    /// Atomically check and charge every requested dimension.
    pub(crate) fn consume(&self, cost: BudgetCost) -> Result<BudgetSnapshot, BudgetError> {
        validate_cost(cost)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| BudgetError::StateInconsistent)?;
        if deadline_expired(self.limits.wall_clock_deadline.as_deref())? {
            return Err(BudgetError::DeadlineExceeded);
        }

        check_dimension(
            BudgetDimension::ModelCalls,
            cost.model_calls,
            state.remaining.model_calls,
        )?;
        check_dimension(
            BudgetDimension::OutputTokens,
            cost.output_tokens,
            state.remaining.output_tokens,
        )?;
        check_dimension(
            BudgetDimension::ToolCalls,
            cost.tool_calls,
            state.remaining.tool_calls,
        )?;
        check_dimension(
            BudgetDimension::SearchActions,
            cost.search_actions,
            state.remaining.search_actions,
        )?;
        check_dimension(
            BudgetDimension::Candidates,
            cost.candidates,
            state.remaining.candidates,
        )?;
        check_dimension(
            BudgetDimension::ContextBytes,
            cost.context_bytes,
            state.remaining.context_bytes,
        )?;
        check_dimension(
            BudgetDimension::ContextTokens,
            cost.context_tokens,
            state.remaining.context_tokens,
        )?;
        check_dimension(
            BudgetDimension::Retries,
            cost.retries,
            state.remaining.retries,
        )?;
        check_dimension(
            BudgetDimension::ArtifactBytes,
            cost.artifact_bytes,
            state.remaining.artifact_bytes,
        )?;
        check_dimension(
            BudgetDimension::EvidenceBytes,
            cost.evidence_bytes,
            state.remaining.evidence_bytes,
        )?;

        state.consumed.add_cost(cost);
        state.remaining.model_calls -= cost.model_calls;
        state.remaining.output_tokens -= cost.output_tokens;
        state.remaining.tool_calls -= cost.tool_calls;
        state.remaining.search_actions -= cost.search_actions;
        state.remaining.candidates -= cost.candidates;
        state.remaining.context_bytes -= cost.context_bytes;
        state.remaining.context_tokens -= cost.context_tokens;
        state.remaining.retries -= cost.retries;
        state.remaining.artifact_bytes -= cost.artifact_bytes;
        state.remaining.evidence_bytes -= cost.evidence_bytes;

        Ok(BudgetSnapshot {
            consumed: state.consumed,
            remaining: current_remaining(&self.limits, &state.remaining),
            deadline_at: self.limits.wall_clock_deadline.clone(),
        })
    }

    pub(crate) fn checked_consume(&self, cost: BudgetCost) -> Result<BudgetSnapshot, BudgetError> {
        self.consume(cost)
    }

    pub(crate) fn is_exhausted(&self) -> Result<bool, BudgetError> {
        let remaining = self.remaining()?;
        Ok(remaining.model_calls == 0
            || remaining.output_tokens == 0
            || remaining.tool_calls == 0
            || remaining.search_actions == 0
            || remaining.candidates == 0
            || remaining.context_bytes == 0
            || remaining.context_tokens == 0
            || remaining.retries == 0
            || remaining.artifact_bytes == 0
            || remaining.evidence_bytes == 0)
    }

    pub(crate) fn validate(&self) -> Result<(), BudgetError> {
        self.limits.validate()?;
        let state = self
            .state
            .lock()
            .map_err(|_| BudgetError::StateInconsistent)?;
        validate_state(&self.limits, &state)
    }

    fn from_wire(wire: BudgetWire) -> Result<Self, BudgetError> {
        wire.limits.validate()?;
        let expected = initial_remaining(&wire.limits)?;
        if !remaining_matches(&wire.limits, wire.consumed, wire.remaining, expected) {
            return Err(BudgetError::StateInconsistent);
        }
        let budget = Self {
            limits: wire.limits,
            state: Arc::new(Mutex::new(BudgetState {
                consumed: wire.consumed,
                remaining: wire.remaining,
            })),
        };
        budget.validate()?;
        Ok(budget)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetWire {
    schema_version: u32,
    limits: AgentBudgetLimits,
    consumed: BudgetUsage,
    remaining: BudgetUsage,
}

impl Serialize for AgentBudget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let snapshot = self.snapshot().map_err(serde::ser::Error::custom)?;
        BudgetWire {
            schema_version: BUDGET_SCHEMA_VERSION,
            limits: self.limits.clone(),
            consumed: snapshot.consumed,
            remaining: snapshot.remaining,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentBudget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BudgetWire::deserialize(deserializer)?;
        if wire.schema_version != BUDGET_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported budget schema version",
            ));
        }
        Self::from_wire(wire).map_err(serde::de::Error::custom)
    }
}

fn initial_remaining(limits: &AgentBudgetLimits) -> Result<BudgetUsage, BudgetError> {
    Ok(BudgetUsage {
        wall_clock_ms: remaining_deadline_ms(limits.wall_clock_deadline.as_deref())?,
        model_calls: limits.max_model_calls,
        output_tokens: limits.max_output_tokens,
        tool_calls: limits.max_tool_calls,
        search_actions: limits.max_search_actions,
        candidates: limits.max_candidates,
        context_bytes: limits.max_context_bytes,
        context_tokens: limits.max_context_tokens,
        retries: limits.max_retries,
        artifact_bytes: limits.max_artifact_bytes,
        evidence_bytes: limits.max_evidence_bytes,
    })
}

fn current_remaining(limits: &AgentBudgetLimits, stored: &BudgetUsage) -> BudgetUsage {
    let mut remaining = *stored;
    remaining.wall_clock_ms =
        remaining_deadline_ms(limits.wall_clock_deadline.as_deref()).unwrap_or(0);
    remaining
}

fn remaining_matches(
    _limits: &AgentBudgetLimits,
    consumed: BudgetUsage,
    remaining: BudgetUsage,
    expected: BudgetUsage,
) -> bool {
    remaining.model_calls == expected.model_calls.saturating_sub(consumed.model_calls)
        && remaining.output_tokens
            == expected
                .output_tokens
                .saturating_sub(consumed.output_tokens)
        && remaining.tool_calls == expected.tool_calls.saturating_sub(consumed.tool_calls)
        && remaining.search_actions
            == expected
                .search_actions
                .saturating_sub(consumed.search_actions)
        && remaining.candidates == expected.candidates.saturating_sub(consumed.candidates)
        && remaining.context_bytes
            == expected
                .context_bytes
                .saturating_sub(consumed.context_bytes)
        && remaining.context_tokens
            == expected
                .context_tokens
                .saturating_sub(consumed.context_tokens)
        && remaining.retries == expected.retries.saturating_sub(consumed.retries)
        && remaining.artifact_bytes
            == expected
                .artifact_bytes
                .saturating_sub(consumed.artifact_bytes)
        && remaining.evidence_bytes
            == expected
                .evidence_bytes
                .saturating_sub(consumed.evidence_bytes)
    // Wall-clock remaining time is a live value, not a consumable counter.
    // It is refreshed on every snapshot and therefore is intentionally not
    // compared with the value persisted by a previous process.
    // The wall-clock field is intentionally excluded from equality.
}

fn validate_state(limits: &AgentBudgetLimits, state: &BudgetState) -> Result<(), BudgetError> {
    let expected = initial_remaining(limits)?;
    if state.consumed.model_calls > limits.max_model_calls
        || state.consumed.output_tokens > limits.max_output_tokens
        || state.consumed.tool_calls > limits.max_tool_calls
        || state.consumed.search_actions > limits.max_search_actions
        || state.consumed.candidates > limits.max_candidates
        || state.consumed.context_bytes > limits.max_context_bytes
        || state.consumed.context_tokens > limits.max_context_tokens
        || state.consumed.retries > limits.max_retries
        || state.consumed.artifact_bytes > limits.max_artifact_bytes
        || state.consumed.evidence_bytes > limits.max_evidence_bytes
        || !remaining_matches(limits, state.consumed, state.remaining, expected)
    {
        return Err(BudgetError::StateInconsistent);
    }
    Ok(())
}

fn validate_cost(cost: BudgetCost) -> Result<(), BudgetError> {
    // The explicit u64 representation already excludes negative values. Keep
    // a checked operation here so future cost fields cannot silently be added
    // without validation.
    if cost.is_zero() {
        return Ok(());
    }
    Ok(())
}

fn check_dimension(
    dimension: BudgetDimension,
    requested: u64,
    remaining: u64,
) -> Result<(), BudgetError> {
    if requested > remaining {
        return Err(BudgetError::Exhausted {
            dimension,
            requested,
            remaining,
        });
    }
    Ok(())
}

fn parse_deadline(value: &str) -> Result<DateTime<Utc>, BudgetError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| BudgetError::InvalidDeadline)
}

fn deadline_expired(value: Option<&str>) -> Result<bool, BudgetError> {
    let Some(value) = value else {
        return Ok(false);
    };
    Ok(parse_deadline(value)? <= Utc::now())
}

fn remaining_deadline_ms(value: Option<&str>) -> Result<u64, BudgetError> {
    let Some(value) = value else {
        return Ok(u64::MAX);
    };
    let duration = parse_deadline(value)?.signed_duration_since(Utc::now());
    Ok(u64::try_from(duration.num_milliseconds().max(0)).unwrap_or(0))
}
