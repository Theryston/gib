//! Deterministic hardware-aware runtime profile resolution.
//!
//! This module contains policy only. It does not load a model, inspect a
//! conversation, or call llama.cpp. A caller supplies one hardware snapshot
//! and one model manifest, and receives one validated configuration that can
//! be passed to the runtime unchanged.

use crate::ai::hardware::HardwareSnapshot;
use crate::ai::model::ModelManifest;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub(crate) const RUNTIME_CONFIG_VERSION: u32 = 1;
pub(crate) const DEFAULT_MEMORY_BUDGET_PERCENT: u8 = 80;
pub(crate) const ALL_GPU_LAYERS: u32 = u32::MAX;

const LOW_MEMORY_CONTEXT_SIZE: u32 = 2048;
const LOW_MEMORY_BATCH_SIZE: u32 = 128;
const LOW_MEMORY_MAX_OUTPUT_TOKENS: u32 = 128;
const LOW_MEMORY_RETAINED_CONVERSATIONS: u32 = 4;

const BALANCED_CONTEXT_SIZE: u32 = 4096;
const BALANCED_BATCH_SIZE: u32 = 512;
const BALANCED_MAX_OUTPUT_TOKENS: u32 = 256;
const BALANCED_RETAINED_CONVERSATIONS: u32 = 16;

const HIGH_QUALITY_CONTEXT_SIZE: u32 = 8192;
const HIGH_QUALITY_BATCH_SIZE: u32 = 512;
const HIGH_QUALITY_MAX_OUTPUT_TOKENS: u32 = 512;
const HIGH_QUALITY_RETAINED_CONVERSATIONS: u32 = 32;

const MAX_CONTEXT_SIZE: u32 = 32_768;
const MAX_BATCH_SIZE: u32 = 4096;
const MAX_OUTPUT_TOKENS: u32 = 8192;
const MAX_AGENT_OR_SEARCH_BUDGET: u32 = 1_000_000;
const MAX_THREADS: u32 = 256;

// Conservative estimates for the current Qwen GGUF. The values are expressed
// in bytes per token and intentionally include headroom for the fp16 KV cache
// and temporary tensors. Exact model metadata can replace these constants when
// the registry grows richer, but a marketing parameter count is never used as
// a memory estimate.
const KV_CACHE_BYTES_PER_TOKEN: u64 = 128 * 1024;
const BATCH_WORKING_BYTES_PER_TOKEN: u64 = 32 * 1024;
const ALLOCATOR_OVERHEAD_PERCENT: u64 = 20;

/// The user-selectable policy family. Serde names are stable config values;
/// command-line parsing also accepts the kebab-case spellings.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeProfile {
    LowMemory,
    Balanced,
    HighQuality,
}

impl Default for RuntimeProfile {
    fn default() -> Self {
        Self::Balanced
    }
}

impl RuntimeProfile {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LowMemory => "low_memory",
            Self::Balanced => "balanced",
            Self::HighQuality => "high_quality",
        }
    }

    pub(crate) fn cli_name(self) -> &'static str {
        match self {
            Self::LowMemory => "low-memory",
            Self::Balanced => "balanced",
            Self::HighQuality => "high-quality",
        }
    }

    fn lower(self) -> Option<Self> {
        match self {
            Self::HighQuality => Some(Self::Balanced),
            Self::Balanced => Some(Self::LowMemory),
            Self::LowMemory => None,
        }
    }
}

impl fmt::Display for RuntimeProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RuntimeProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "low-memory" | "low_memory" | "low" => Ok(Self::LowMemory),
            "balanced" | "default" => Ok(Self::Balanced),
            "high-quality" | "high_quality" | "high" => Ok(Self::HighQuality),
            _ => Err(format!(
                "unknown AI runtime profile '{}'; expected low-memory, balanced, or high-quality",
                value
            )),
        }
    }
}

/// Explicit settings stored in AI config or supplied for one invocation.
/// `None` means that the selected profile remains responsible for the value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RuntimeOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) threads: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) batch_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gpu_layers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gpu_offload: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_budget: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) search_budget: Option<u32>,
    /// Percentage of the selected memory basis that the estimate may use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) memory_budget_percent: Option<u8>,
}

impl RuntimeOverrides {
    /// Merge a higher-precedence invocation override over persisted settings.
    pub(crate) fn merge(&self, higher: &Self) -> Self {
        Self {
            threads: higher.threads.or(self.threads),
            context_size: higher.context_size.or(self.context_size),
            batch_size: higher.batch_size.or(self.batch_size),
            gpu_layers: higher.gpu_layers.or(self.gpu_layers),
            gpu_offload: higher.gpu_offload.or(self.gpu_offload),
            max_output_tokens: higher.max_output_tokens.or(self.max_output_tokens),
            agent_budget: higher.agent_budget.or(self.agent_budget),
            search_budget: higher.search_budget.or(self.search_budget),
            memory_budget_percent: higher.memory_budget_percent.or(self.memory_budget_percent),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), RuntimeProfileError> {
        if let Some(threads) = self.threads
            && (threads == 0 || threads > MAX_THREADS)
        {
            return Err(RuntimeProfileError::InvalidOverride(format!(
                "threads must be between 1 and {MAX_THREADS}"
            )));
        }
        if let Some(context_size) = self.context_size
            && (context_size == 0 || context_size > MAX_CONTEXT_SIZE)
        {
            return Err(RuntimeProfileError::InvalidOverride(format!(
                "context_size must be between 1 and {MAX_CONTEXT_SIZE}"
            )));
        }
        if let Some(batch_size) = self.batch_size
            && (batch_size == 0 || batch_size > MAX_BATCH_SIZE)
        {
            return Err(RuntimeProfileError::InvalidOverride(format!(
                "batch_size must be between 1 and {MAX_BATCH_SIZE}"
            )));
        }
        if let Some(max_output_tokens) = self.max_output_tokens
            && (max_output_tokens == 0 || max_output_tokens > MAX_OUTPUT_TOKENS)
        {
            return Err(RuntimeProfileError::InvalidOverride(format!(
                "max_output_tokens must be between 1 and {MAX_OUTPUT_TOKENS}"
            )));
        }
        if let (Some(context_size), Some(max_output_tokens)) =
            (self.context_size, self.max_output_tokens)
            && context_size <= max_output_tokens
        {
            return Err(RuntimeProfileError::InvalidOverride(
                "context_size must be greater than max_output_tokens".to_string(),
            ));
        }
        if let (Some(context_size), Some(batch_size)) = (self.context_size, self.batch_size)
            && batch_size > context_size
        {
            return Err(RuntimeProfileError::ContextExceedsBatch {
                context_size,
                batch_size,
            });
        }
        if self.gpu_offload == Some(true) && self.gpu_layers == Some(0) {
            return Err(RuntimeProfileError::InvalidOverride(
                "gpu_offload=on cannot be combined with gpu_layers=0".to_string(),
            ));
        }
        for (name, value) in [
            ("agent_budget", self.agent_budget),
            ("search_budget", self.search_budget),
        ] {
            if value.is_some_and(|value| value == 0 || value > MAX_AGENT_OR_SEARCH_BUDGET) {
                return Err(RuntimeProfileError::InvalidOverride(format!(
                    "{name} must be between 1 and {MAX_AGENT_OR_SEARCH_BUDGET}"
                )));
            }
        }
        if let Some(percent) = self.memory_budget_percent
            && !(1..=100).contains(&percent)
        {
            return Err(RuntimeProfileError::InvalidOverride(
                "memory_budget_percent must be between 1 and 100".to_string(),
            ));
        }
        Ok(())
    }

    fn has_memory_resource_override(&self) -> bool {
        self.context_size.is_some()
            || self.batch_size.is_some()
            || self.gpu_layers.is_some()
            || self.gpu_offload.is_some()
            || self.max_output_tokens.is_some()
            || self.memory_budget_percent.is_some()
    }
}

/// User-global AI runtime preferences persisted in ~/.gib/ai/config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RuntimePreferences {
    pub(crate) profile: RuntimeProfile,
    pub(crate) overrides: RuntimeOverrides,
}

impl RuntimePreferences {
    pub(crate) fn validate(&self) -> Result<(), RuntimeProfileError> {
        self.overrides.validate()
    }
}

/// A memory estimate included in status events and used by the resolver.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryEstimate {
    pub(crate) model_bytes: u64,
    pub(crate) kv_cache_bytes: u64,
    pub(crate) batch_working_bytes: u64,
    pub(crate) allocator_overhead_bytes: u64,
    pub(crate) concurrent_requests: u32,
    pub(crate) estimated_total_bytes: u64,
    pub(crate) available_memory_bytes: Option<u64>,
    pub(crate) memory_basis: String,
    pub(crate) memory_budget_percent: u8,
    pub(crate) budget_bytes: Option<u64>,
    pub(crate) model_minimum_bytes: Option<u64>,
    pub(crate) fits_budget: Option<bool>,
}

/// The one resolved configuration passed to the in-process runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    pub(crate) schema_version: u32,
    pub(crate) model_id: String,
    pub(crate) profile: RuntimeProfile,
    pub(crate) hardware: HardwareSnapshot,
    pub(crate) threads: u32,
    pub(crate) batch_threads: u32,
    pub(crate) context_size: u32,
    pub(crate) batch_size: u32,
    pub(crate) micro_batch_size: u32,
    pub(crate) gpu_layers: u32,
    pub(crate) gpu_offload: bool,
    pub(crate) offload_kqv: bool,
    pub(crate) max_output_tokens: u32,
    pub(crate) retained_conversations: u32,
    pub(crate) agent_budget: Option<u32>,
    pub(crate) search_budget: Option<u32>,
    pub(crate) keep_model_warm: bool,
    pub(crate) memory_estimate: MemoryEstimate,
    pub(crate) downgrade_reason: Option<String>,
}

impl RuntimeConfig {
    pub(crate) fn summary(&self) -> String {
        let gpu = if self.gpu_offload {
            if self.gpu_layers == ALL_GPU_LAYERS {
                "all".to_string()
            } else {
                self.gpu_layers.to_string()
            }
        } else {
            "off".to_string()
        };
        format!(
            "profile={} threads={} context={} batch={} output={} gpu={}",
            self.profile.cli_name(),
            self.threads,
            self.context_size,
            self.batch_size,
            self.max_output_tokens,
            gpu
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProfileDefaults {
    context_size: u32,
    batch_size: u32,
    max_output_tokens: u32,
    retained_conversations: u32,
    thread_cap: u32,
    gpu_layers: u32,
    gpu_offload: bool,
    keep_model_warm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum RuntimeProfileError {
    InvalidOverride(String),
    MissingModelSize {
        model_id: String,
    },
    UnsupportedGpuOffload,
    GpuMemoryInsufficient {
        required_bytes: u64,
        available_bytes: u64,
    },
    ExcessiveThreads {
        requested: u32,
        safe_limit: u32,
    },
    ContextExceedsBatch {
        context_size: u32,
        batch_size: u32,
    },
    MemoryUnavailableForOverride,
    UnsafeResourceOverride {
        setting: String,
        estimated_bytes: u64,
        budget_bytes: u64,
    },
    InsufficientMemory {
        profile: RuntimeProfile,
        estimated_bytes: u64,
        budget_bytes: u64,
    },
}

impl RuntimeProfileError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidOverride(_) => "invalid_runtime_override",
            Self::MissingModelSize { .. } => "runtime_model_size_unavailable",
            Self::UnsupportedGpuOffload => "runtime_gpu_offload_unavailable",
            Self::GpuMemoryInsufficient { .. } => "runtime_gpu_memory_insufficient",
            Self::ExcessiveThreads { .. } => "runtime_threads_excessive",
            Self::ContextExceedsBatch { .. } => "runtime_context_exceeds_batch",
            Self::MemoryUnavailableForOverride => "runtime_memory_unavailable",
            Self::UnsafeResourceOverride { .. } => "runtime_unsafe_resource_override",
            Self::InsufficientMemory { .. } => "runtime_insufficient_memory",
        }
    }
}

impl fmt::Display for RuntimeProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOverride(message) => write!(formatter, "Invalid AI runtime override: {message}"),
            Self::MissingModelSize { model_id } => write!(
                formatter,
                "Cannot resolve an AI runtime profile for '{model_id}' because the model size is unavailable"
            ),
            Self::UnsupportedGpuOffload => formatter.write_str(
                "GPU offload was requested, but the current llama.cpp build reports no usable accelerator",
            ),
            Self::GpuMemoryInsufficient {
                required_bytes,
                available_bytes,
            } => write!(
                formatter,
                "GPU offload requested approximately {required_bytes} model bytes, but only {available_bytes} GPU bytes were reported free"
            ),
            Self::ExcessiveThreads {
                requested,
                safe_limit,
            } => write!(
                formatter,
                "AI runtime requested {requested} threads, above the safe host limit of {safe_limit}"
            ),
            Self::ContextExceedsBatch {
                context_size,
                batch_size,
            } => write!(
                formatter,
                "AI runtime context size {context_size} cannot be paired with batch size {batch_size}"
            ),
            Self::MemoryUnavailableForOverride => formatter.write_str(
                "Cannot validate an explicit AI resource override because host memory is unavailable",
            ),
            Self::UnsafeResourceOverride {
                setting,
                estimated_bytes,
                budget_bytes,
            } => write!(
                formatter,
                "AI runtime override '{setting}' would require approximately {estimated_bytes} bytes, above the safe budget of {budget_bytes} bytes"
            ),
            Self::InsufficientMemory {
                profile,
                estimated_bytes,
                budget_bytes,
            } => write!(
                formatter,
                "AI runtime profile '{profile}' needs approximately {estimated_bytes} bytes, above the safe budget of {budget_bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for RuntimeProfileError {}

/// Resolve persisted preferences plus invocation-scoped overrides. The
/// invocation values have precedence, and available memory is sampled only by
/// the caller before this function is called.
pub(crate) fn resolve_runtime_config(
    model_id: &str,
    model: &ModelManifest,
    preferences: &RuntimePreferences,
    invocation_profile: Option<RuntimeProfile>,
    invocation_overrides: &RuntimeOverrides,
    hardware: HardwareSnapshot,
) -> Result<RuntimeConfig, RuntimeProfileError> {
    preferences.validate()?;
    invocation_overrides.validate()?;
    let overrides = preferences.overrides.merge(invocation_overrides);
    overrides.validate()?;

    let model_bytes = model
        .expected_size
        .ok_or_else(|| RuntimeProfileError::MissingModelSize {
            model_id: model_id.to_string(),
        })?;
    if model_bytes == 0 {
        return Err(RuntimeProfileError::MissingModelSize {
            model_id: model_id.to_string(),
        });
    }

    let requested_profile = invocation_profile.unwrap_or(preferences.profile);
    let memory_budget_percent = overrides
        .memory_budget_percent
        .unwrap_or(DEFAULT_MEMORY_BUDGET_PERCENT);
    let explicit_memory_resource_override = overrides.has_memory_resource_override();
    let mut profile = requested_profile;
    let mut reasons = Vec::new();

    loop {
        let defaults = profile_defaults(profile, &hardware, model_bytes);
        if profile == RuntimeProfile::HighQuality
            && hardware.usable_gpu_offload()
            && hardware.gpu_can_fit_model(model_bytes) == Some(false)
        {
            reasons.push(
                "HighQuality GPU offload was disabled because the reported free accelerator memory is smaller than the GGUF model"
                    .to_string(),
            );
        }
        if profile == RuntimeProfile::HighQuality
            && !hardware.usable_gpu_offload()
            && overrides.gpu_layers.is_none()
            && overrides.gpu_offload != Some(false)
        {
            reasons.push(
                "HighQuality GPU offload was disabled because llama.cpp reported no usable accelerator"
                    .to_string(),
            );
        }
        let mut threads = defaults.thread_cap.min(hardware.cpu_count_or_one()).max(1);
        let mut context_size = defaults.context_size;
        let mut batch_size = defaults.batch_size;
        let mut max_output_tokens = defaults.max_output_tokens;
        let mut gpu_layers = defaults.gpu_layers;

        if let Some(value) = overrides.threads {
            threads = value;
        }
        if let Some(value) = overrides.context_size {
            context_size = value;
        }
        if let Some(value) = overrides.batch_size {
            batch_size = value;
        }
        if let Some(value) = overrides.max_output_tokens {
            max_output_tokens = value;
        }
        if let Some(value) = overrides.gpu_layers {
            gpu_layers = value;
        }
        if let Some(value) = overrides.gpu_offload {
            if value && overrides.gpu_layers.is_none() && gpu_layers == 0 {
                gpu_layers = ALL_GPU_LAYERS;
            }
            if !value {
                gpu_layers = 0;
            }
        }
        let gpu_offload = gpu_layers > 0;

        validate_resolved_values(
            &hardware,
            &overrides,
            threads,
            context_size,
            batch_size,
            max_output_tokens,
            gpu_layers,
            gpu_offload,
            model_bytes,
        )?;

        let estimate = estimate_memory(
            model_bytes,
            model.min_ram_bytes,
            &hardware,
            context_size,
            batch_size,
            memory_budget_percent,
        );
        match estimate.fits_budget {
            Some(true) => {
                return Ok(build_runtime_config(
                    model_id,
                    profile,
                    hardware,
                    defaults,
                    threads,
                    context_size,
                    batch_size,
                    max_output_tokens,
                    gpu_layers,
                    gpu_offload,
                    estimate,
                    join_reasons(reasons),
                    overrides.agent_budget,
                    overrides.search_budget,
                ));
            }
            Some(false) if explicit_memory_resource_override => {
                return Err(RuntimeProfileError::UnsafeResourceOverride {
                    setting: resource_setting_name(&overrides),
                    estimated_bytes: estimate.estimated_total_bytes,
                    budget_bytes: estimate.budget_bytes.unwrap_or(0),
                });
            }
            Some(false) => {
                let Some(next_profile) = profile.lower() else {
                    reasons.push(format!(
                        "LowMemory estimated approximately {} bytes, above the {}% memory budget of {} bytes; continuing with LowMemory defaults, so performance may be degraded",
                        estimate.estimated_total_bytes,
                        memory_budget_percent,
                        estimate.budget_bytes.unwrap_or(0),
                    ));
                    return Ok(build_runtime_config(
                        model_id,
                        profile,
                        hardware,
                        defaults,
                        threads,
                        context_size,
                        batch_size,
                        max_output_tokens,
                        gpu_layers,
                        gpu_offload,
                        estimate,
                        join_reasons(reasons),
                        overrides.agent_budget,
                        overrides.search_budget,
                    ));
                };
                reasons.push(format!(
                    "{} exceeded the {}% memory budget (estimated {} bytes, budget {} bytes); downgraded to {}",
                    profile,
                    memory_budget_percent,
                    estimate.estimated_total_bytes,
                    estimate.budget_bytes.unwrap_or(0),
                    next_profile
                ));
                profile = next_profile;
            }
            None if explicit_memory_resource_override => {
                return Err(RuntimeProfileError::MemoryUnavailableForOverride);
            }
            None if profile == RuntimeProfile::LowMemory => {
                reasons.push(
                    "Host available memory was unavailable; LowMemory defaults were retained"
                        .to_string(),
                );
                return Ok(build_runtime_config(
                    model_id,
                    profile,
                    hardware,
                    defaults,
                    threads,
                    context_size,
                    batch_size,
                    max_output_tokens,
                    gpu_layers,
                    gpu_offload,
                    estimate,
                    join_reasons(reasons),
                    overrides.agent_budget,
                    overrides.search_budget,
                ));
            }
            None => {
                let Some(next_profile) = profile.lower() else {
                    unreachable!("LowMemory is handled above")
                };
                reasons.push(format!(
                    "Host available memory was unavailable; downgraded to {} for conservative defaults",
                    next_profile
                ));
                profile = next_profile;
            }
        }
    }
}

fn profile_defaults(
    profile: RuntimeProfile,
    hardware: &HardwareSnapshot,
    model_bytes: u64,
) -> ProfileDefaults {
    let gpu_available =
        hardware.usable_gpu_offload() && hardware.gpu_can_fit_model(model_bytes) != Some(false);
    match profile {
        RuntimeProfile::LowMemory => ProfileDefaults {
            context_size: LOW_MEMORY_CONTEXT_SIZE,
            batch_size: LOW_MEMORY_BATCH_SIZE,
            max_output_tokens: LOW_MEMORY_MAX_OUTPUT_TOKENS,
            retained_conversations: LOW_MEMORY_RETAINED_CONVERSATIONS,
            thread_cap: 4,
            gpu_layers: 0,
            gpu_offload: false,
            keep_model_warm: false,
        },
        RuntimeProfile::Balanced => ProfileDefaults {
            context_size: BALANCED_CONTEXT_SIZE,
            batch_size: BALANCED_BATCH_SIZE,
            max_output_tokens: BALANCED_MAX_OUTPUT_TOKENS,
            retained_conversations: BALANCED_RETAINED_CONVERSATIONS,
            thread_cap: 8,
            gpu_layers: 0,
            gpu_offload: false,
            keep_model_warm: true,
        },
        RuntimeProfile::HighQuality => ProfileDefaults {
            context_size: HIGH_QUALITY_CONTEXT_SIZE,
            batch_size: HIGH_QUALITY_BATCH_SIZE,
            max_output_tokens: HIGH_QUALITY_MAX_OUTPUT_TOKENS,
            retained_conversations: HIGH_QUALITY_RETAINED_CONVERSATIONS,
            thread_cap: 16,
            gpu_layers: if gpu_available { ALL_GPU_LAYERS } else { 0 },
            gpu_offload: gpu_available,
            keep_model_warm: true,
        },
    }
}

fn validate_resolved_values(
    hardware: &HardwareSnapshot,
    overrides: &RuntimeOverrides,
    threads: u32,
    context_size: u32,
    batch_size: u32,
    max_output_tokens: u32,
    gpu_layers: u32,
    gpu_offload: bool,
    model_bytes: u64,
) -> Result<(), RuntimeProfileError> {
    let safe_thread_limit = hardware
        .cpu_count_or_one()
        .saturating_mul(2)
        .clamp(1, MAX_THREADS);
    if threads == 0 {
        return Err(RuntimeProfileError::InvalidOverride(
            "threads must be greater than zero".to_string(),
        ));
    }
    if overrides.threads.is_some() && threads > safe_thread_limit {
        return Err(RuntimeProfileError::ExcessiveThreads {
            requested: threads,
            safe_limit: safe_thread_limit,
        });
    }
    if context_size == 0 || context_size > MAX_CONTEXT_SIZE {
        return Err(RuntimeProfileError::InvalidOverride(format!(
            "context_size must be between 1 and {MAX_CONTEXT_SIZE}"
        )));
    }
    if max_output_tokens == 0
        || max_output_tokens > MAX_OUTPUT_TOKENS
        || context_size <= max_output_tokens
    {
        return Err(RuntimeProfileError::InvalidOverride(
            "context_size must be greater than max_output_tokens, and max_output_tokens must be within its supported bound"
                .to_string(),
        ));
    }
    if batch_size == 0 || batch_size > MAX_BATCH_SIZE {
        return Err(RuntimeProfileError::InvalidOverride(format!(
            "batch_size must be between 1 and {MAX_BATCH_SIZE}"
        )));
    }
    if batch_size > context_size {
        return Err(RuntimeProfileError::ContextExceedsBatch {
            context_size,
            batch_size,
        });
    }
    if gpu_offload && !hardware.usable_gpu_offload() {
        return Err(RuntimeProfileError::UnsupportedGpuOffload);
    }
    if gpu_offload
        && let Some(available) = hardware.gpu_can_fit_model(model_bytes)
        && !available
    {
        return Err(RuntimeProfileError::GpuMemoryInsufficient {
            required_bytes: model_bytes,
            available_bytes: hardware
                .runtime_capabilities
                .gpu_memory_free_bytes
                .unwrap_or(0),
        });
    }
    if gpu_layers == 0 && gpu_offload {
        return Err(RuntimeProfileError::InvalidOverride(
            "gpu_offload requires at least one GPU layer".to_string(),
        ));
    }
    Ok(())
}

fn estimate_memory(
    model_bytes: u64,
    model_minimum_bytes: Option<u64>,
    hardware: &HardwareSnapshot,
    context_size: u32,
    batch_size: u32,
    memory_budget_percent: u8,
) -> MemoryEstimate {
    let kv_cache_bytes = u64::from(context_size).saturating_mul(KV_CACHE_BYTES_PER_TOKEN);
    let batch_working_bytes = u64::from(batch_size).saturating_mul(BATCH_WORKING_BYTES_PER_TOKEN);
    let base = model_bytes
        .saturating_add(kv_cache_bytes)
        .saturating_add(batch_working_bytes);
    let allocator_overhead_bytes = base
        .saturating_mul(ALLOCATOR_OVERHEAD_PERCENT)
        .saturating_div(100);
    let estimated_total_bytes = base.saturating_add(allocator_overhead_bytes);

    let (available_memory_bytes, memory_basis) =
        if let Some(available) = hardware.available_memory_bytes {
            (Some(available), "available_memory_bytes".to_string())
        } else if let Some(total) = hardware.total_memory_bytes {
            (Some(total), "total_memory_bytes_fallback".to_string())
        } else {
            (None, "unavailable".to_string())
        };
    let budget_bytes = available_memory_bytes.map(|available| {
        available
            .saturating_mul(u64::from(memory_budget_percent))
            .saturating_div(100)
    });
    let fits_budget = budget_bytes.map(|budget| estimated_total_bytes <= budget);

    MemoryEstimate {
        model_bytes,
        kv_cache_bytes,
        batch_working_bytes,
        allocator_overhead_bytes,
        concurrent_requests: 1,
        estimated_total_bytes,
        available_memory_bytes,
        memory_basis,
        memory_budget_percent,
        budget_bytes,
        model_minimum_bytes: model_minimum_bytes,
        fits_budget,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_runtime_config(
    model_id: &str,
    profile: RuntimeProfile,
    hardware: HardwareSnapshot,
    defaults: ProfileDefaults,
    threads: u32,
    context_size: u32,
    batch_size: u32,
    max_output_tokens: u32,
    gpu_layers: u32,
    gpu_offload: bool,
    memory_estimate: MemoryEstimate,
    downgrade_reason: Option<String>,
    agent_budget: Option<u32>,
    search_budget: Option<u32>,
) -> RuntimeConfig {
    RuntimeConfig {
        schema_version: RUNTIME_CONFIG_VERSION,
        model_id: model_id.to_string(),
        profile,
        hardware,
        threads,
        batch_threads: threads,
        context_size,
        batch_size,
        micro_batch_size: batch_size,
        gpu_layers,
        gpu_offload,
        offload_kqv: gpu_offload,
        max_output_tokens,
        retained_conversations: defaults.retained_conversations,
        agent_budget,
        search_budget,
        keep_model_warm: defaults.keep_model_warm,
        memory_estimate,
        downgrade_reason,
    }
}

fn resource_setting_name(overrides: &RuntimeOverrides) -> String {
    let mut settings = Vec::new();
    if overrides.threads.is_some() {
        settings.push("threads");
    }
    if overrides.context_size.is_some() {
        settings.push("context_size");
    }
    if overrides.batch_size.is_some() {
        settings.push("batch_size");
    }
    if overrides.gpu_layers.is_some() || overrides.gpu_offload.is_some() {
        settings.push("gpu_offload");
    }
    if overrides.max_output_tokens.is_some() {
        settings.push("max_output_tokens");
    }
    if overrides.memory_budget_percent.is_some() {
        settings.push("memory_budget_percent");
    }
    settings.join(",")
}

fn join_reasons(reasons: Vec<String>) -> Option<String> {
    (!reasons.is_empty()).then(|| reasons.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::hardware::HardwareSnapshot;
    use crate::ai::model::ModelRegistry;

    fn model() -> ModelManifest {
        ModelRegistry::default()
            .get("qwen3.5-4b-q8-0")
            .expect("built-in model should exist")
            .clone()
    }

    fn resolve(
        profile: RuntimeProfile,
        hardware: HardwareSnapshot,
        overrides: RuntimeOverrides,
    ) -> Result<RuntimeConfig, RuntimeProfileError> {
        resolve_runtime_config(
            "qwen3.5-4b-q8-0",
            &model(),
            &RuntimePreferences {
                profile,
                overrides: RuntimeOverrides::default(),
            },
            None,
            &overrides,
            hardware,
        )
    }

    #[test]
    fn profile_defaults_are_deterministic_for_normal_memory() {
        let hardware = HardwareSnapshot::fixture(
            Some(16 * 1024 * 1024 * 1024),
            Some(12 * 1024 * 1024 * 1024),
            Some(8),
            Some(4),
            false,
        );
        let first = resolve(
            RuntimeProfile::Balanced,
            hardware.clone(),
            RuntimeOverrides::default(),
        )
        .expect("balanced profile should fit");
        let second = resolve(
            RuntimeProfile::Balanced,
            hardware,
            RuntimeOverrides::default(),
        )
        .expect("balanced profile should fit");
        assert_eq!(first, second);
        assert_eq!(first.context_size, BALANCED_CONTEXT_SIZE);
        assert_eq!(first.batch_size, BALANCED_BATCH_SIZE);
        assert_eq!(first.profile, RuntimeProfile::Balanced);
    }

    #[test]
    fn high_quality_downgrades_when_memory_estimate_crosses_threshold() {
        let hardware = HardwareSnapshot::fixture(
            Some(8 * 1024 * 1024 * 1024),
            Some(7_500 * 1024 * 1024),
            Some(4),
            Some(2),
            false,
        );
        let config = resolve(
            RuntimeProfile::HighQuality,
            hardware,
            RuntimeOverrides::default(),
        )
        .expect("a lower profile should fit");
        assert_eq!(config.profile, RuntimeProfile::Balanced);
        assert!(config.downgrade_reason.is_some());
        assert_eq!(config.context_size, BALANCED_CONTEXT_SIZE);
    }

    #[test]
    fn unavailable_memory_uses_low_memory_defaults_with_a_reason() {
        let hardware = HardwareSnapshot::fixture(None, None, Some(4), Some(2), false);
        let config = resolve(
            RuntimeProfile::Balanced,
            hardware,
            RuntimeOverrides::default(),
        )
        .expect("unknown memory should use a conservative profile");
        assert_eq!(config.profile, RuntimeProfile::LowMemory);
        assert!(config.memory_estimate.fits_budget.is_none());
        assert!(
            config
                .downgrade_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unavailable"))
        );
    }

    #[test]
    fn low_memory_continues_with_a_performance_warning_when_budget_is_too_small() {
        let hardware = HardwareSnapshot::fixture(
            Some(8 * 1024 * 1024 * 1024),
            Some(2 * 1024 * 1024 * 1024),
            Some(4),
            Some(2),
            false,
        );
        let config = resolve(
            RuntimeProfile::LowMemory,
            hardware,
            RuntimeOverrides::default(),
        )
        .expect("automatic LowMemory selection should remain usable");

        assert_eq!(config.profile, RuntimeProfile::LowMemory);
        assert_eq!(config.memory_estimate.fits_budget, Some(false));
        assert!(
            config
                .downgrade_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("performance may be degraded"))
        );
    }

    #[test]
    fn high_quality_uses_native_acceleration_only_when_reported_usable() {
        let hardware = HardwareSnapshot::fixture(
            Some(32 * 1024 * 1024 * 1024),
            Some(24 * 1024 * 1024 * 1024),
            Some(16),
            Some(8),
            true,
        );
        let config = resolve(
            RuntimeProfile::HighQuality,
            hardware,
            RuntimeOverrides::default(),
        )
        .expect("high quality profile should fit");
        assert!(config.gpu_offload);
        assert_eq!(config.gpu_layers, ALL_GPU_LAYERS);
    }

    #[test]
    fn high_quality_disables_all_layer_offload_when_reported_gpu_memory_is_too_small() {
        let mut hardware = HardwareSnapshot::fixture(
            Some(32 * 1024 * 1024 * 1024),
            Some(24 * 1024 * 1024 * 1024),
            Some(16),
            Some(8),
            true,
        );
        hardware.runtime_capabilities.gpu_memory_free_bytes = Some(1 * 1024 * 1024 * 1024);
        let config = resolve(
            RuntimeProfile::HighQuality,
            hardware,
            RuntimeOverrides::default(),
        )
        .expect("CPU high-quality settings should remain available");
        assert!(!config.gpu_offload);
        assert!(
            config
                .downgrade_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("accelerator memory"))
        );
    }

    #[test]
    fn explicit_unsafe_memory_override_fails_instead_of_downgrading() {
        let hardware = HardwareSnapshot::fixture(
            Some(8 * 1024 * 1024 * 1024),
            Some(2 * 1024 * 1024 * 1024),
            Some(4),
            Some(2),
            false,
        );
        let error = resolve(
            RuntimeProfile::Balanced,
            hardware,
            RuntimeOverrides {
                context_size: Some(32_768),
                ..RuntimeOverrides::default()
            },
        )
        .expect_err("an explicit overcommit must fail");
        assert!(matches!(
            error,
            RuntimeProfileError::UnsafeResourceOverride { .. }
        ));
    }

    #[test]
    fn invalid_relationships_and_unsupported_offload_are_rejected() {
        let low_memory = HardwareSnapshot::fixture(
            Some(16 * 1024 * 1024 * 1024),
            Some(12 * 1024 * 1024 * 1024),
            Some(8),
            Some(4),
            false,
        );
        let relationship = resolve(
            RuntimeProfile::Balanced,
            low_memory.clone(),
            RuntimeOverrides {
                context_size: Some(256),
                max_output_tokens: Some(256),
                ..RuntimeOverrides::default()
            },
        )
        .expect_err("context and output must have room for one another");
        assert!(matches!(
            relationship,
            RuntimeProfileError::InvalidOverride(_)
        ));

        let offload = resolve(
            RuntimeProfile::Balanced,
            low_memory,
            RuntimeOverrides {
                gpu_offload: Some(true),
                ..RuntimeOverrides::default()
            },
        )
        .expect_err("unsupported offload must fail");
        assert_eq!(offload, RuntimeProfileError::UnsupportedGpuOffload);
    }

    #[test]
    fn explicit_thread_limit_and_gpu_conflicts_are_rejected_before_memory_resolution() {
        let hardware = HardwareSnapshot::fixture(
            Some(16 * 1024 * 1024 * 1024),
            Some(12 * 1024 * 1024 * 1024),
            Some(4),
            Some(2),
            false,
        );
        let threads = resolve(
            RuntimeProfile::Balanced,
            hardware.clone(),
            RuntimeOverrides {
                threads: Some(9),
                ..RuntimeOverrides::default()
            },
        )
        .expect_err("explicit thread oversubscription must be rejected");
        assert!(matches!(
            threads,
            RuntimeProfileError::ExcessiveThreads { .. }
        ));

        let gpu_conflict = resolve(
            RuntimeProfile::Balanced,
            hardware,
            RuntimeOverrides {
                gpu_layers: Some(0),
                gpu_offload: Some(true),
                ..RuntimeOverrides::default()
            },
        )
        .expect_err("contradictory GPU settings must be rejected");
        assert!(matches!(
            gpu_conflict,
            RuntimeProfileError::InvalidOverride(_)
        ));
    }

    #[test]
    fn override_precedence_is_explicit_and_serializable() {
        let persisted = RuntimePreferences {
            profile: RuntimeProfile::LowMemory,
            overrides: RuntimeOverrides {
                threads: Some(2),
                context_size: Some(2048),
                ..RuntimeOverrides::default()
            },
        };
        let invocation = RuntimeOverrides {
            threads: Some(4),
            ..RuntimeOverrides::default()
        };
        let merged = persisted.overrides.merge(&invocation);
        assert_eq!(merged.threads, Some(4));
        assert_eq!(merged.context_size, Some(2048));
        let encoded = toml::to_string(&persisted).expect("preferences should serialize");
        let decoded: RuntimePreferences =
            toml::from_str(&encoded).expect("preferences should round-trip");
        assert_eq!(decoded, persisted);
    }
}
