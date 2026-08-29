use super::error::AiBackendError;
use crate::ai::profiles::RuntimeConfig;

const DEFAULT_CONTEXT_SIZE: u32 = 4096;
const DEFAULT_BATCH_SIZE: u32 = 512;
const DEFAULT_QUEUE_CAPACITY: usize = 8;
const DEFAULT_STREAM_CAPACITY: usize = 64;

/// Runtime parameters translated into llama.cpp model and context settings.
///
/// Hardware profiles in Task 08 can build this value without importing any
/// llama-cpp types. A zero GPU layer count is an explicit CPU configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiRuntimeOptions {
    pub(crate) context_size: u32,
    pub(crate) batch_size: u32,
    pub(crate) micro_batch_size: u32,
    pub(crate) threads: u32,
    pub(crate) batch_threads: u32,
    pub(crate) n_gpu_layers: u32,
    pub(crate) offload_kqv: bool,
    pub(crate) command_capacity: usize,
    pub(crate) stream_capacity: usize,
}

impl Default for AiRuntimeOptions {
    fn default() -> Self {
        let threads = num_cpus::get().clamp(1, i32::MAX as usize) as u32;
        Self {
            context_size: DEFAULT_CONTEXT_SIZE,
            batch_size: DEFAULT_BATCH_SIZE,
            micro_batch_size: DEFAULT_BATCH_SIZE,
            threads,
            batch_threads: threads,
            n_gpu_layers: 0,
            offload_kqv: false,
            command_capacity: DEFAULT_QUEUE_CAPACITY,
            stream_capacity: DEFAULT_STREAM_CAPACITY,
        }
    }
}

impl AiRuntimeOptions {
    pub(crate) fn from_runtime_config(config: &RuntimeConfig) -> Self {
        Self::default()
            .with_context_size(config.context_size)
            .with_batch_size(config.batch_size)
            .with_micro_batch_size(config.micro_batch_size)
            .with_threads(config.threads)
            .with_batch_threads(config.batch_threads)
            .with_gpu_layers(config.gpu_layers)
            .with_offload_kqv(config.offload_kqv)
    }

    pub(crate) fn with_context_size(mut self, value: u32) -> Self {
        self.context_size = value;
        self
    }

    pub(crate) fn with_batch_size(mut self, value: u32) -> Self {
        self.batch_size = value;
        self
    }

    pub(crate) fn with_micro_batch_size(mut self, value: u32) -> Self {
        self.micro_batch_size = value;
        self
    }

    pub(crate) fn with_threads(mut self, value: u32) -> Self {
        self.threads = value;
        self
    }

    pub(crate) fn with_batch_threads(mut self, value: u32) -> Self {
        self.batch_threads = value;
        self
    }

    pub(crate) fn with_gpu_layers(mut self, value: u32) -> Self {
        self.n_gpu_layers = value;
        self
    }

    pub(crate) fn with_offload_kqv(mut self, value: bool) -> Self {
        self.offload_kqv = value;
        self
    }

    pub(crate) fn with_command_capacity(mut self, value: usize) -> Self {
        self.command_capacity = value;
        self
    }

    pub(crate) fn with_stream_capacity(mut self, value: usize) -> Self {
        self.stream_capacity = value;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), AiBackendError> {
        if self.context_size == 0 {
            return Err(AiBackendError::InvalidRequest(
                "runtime context_size must be greater than zero".to_string(),
            ));
        }
        if self.context_size > i32::MAX as u32 {
            return Err(AiBackendError::InvalidRequest(
                "runtime context_size must fit llama.cpp position limits".to_string(),
            ));
        }
        if self.batch_size == 0 {
            return Err(AiBackendError::InvalidRequest(
                "runtime batch_size must be greater than zero".to_string(),
            ));
        }
        if self.batch_size > i32::MAX as u32 {
            return Err(AiBackendError::InvalidRequest(
                "runtime batch_size must fit llama.cpp batch limits".to_string(),
            ));
        }
        if self.micro_batch_size == 0 || self.micro_batch_size > self.batch_size {
            return Err(AiBackendError::InvalidRequest(
                "runtime micro_batch_size must be between one and batch_size".to_string(),
            ));
        }
        if self.threads == 0 || self.threads > i32::MAX as u32 {
            return Err(AiBackendError::InvalidRequest(
                "runtime threads must fit a positive llama.cpp thread count".to_string(),
            ));
        }
        if self.batch_threads == 0 || self.batch_threads > i32::MAX as u32 {
            return Err(AiBackendError::InvalidRequest(
                "runtime batch_threads must fit a positive llama.cpp thread count".to_string(),
            ));
        }
        if self.command_capacity == 0 || self.stream_capacity == 0 {
            return Err(AiBackendError::InvalidRequest(
                "runtime channel capacities must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn context_size_for(&self, requested: u32) -> u32 {
        requested.min(self.context_size)
    }
}

/// Capabilities visible to higher layers without exposing native types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiRuntimeCapabilities {
    pub(crate) cpu: bool,
    pub(crate) gpu_offload: bool,
    pub(crate) mmap: bool,
    pub(crate) mlock: bool,
    pub(crate) gpu_memory_total_bytes: Option<u64>,
    pub(crate) gpu_memory_free_bytes: Option<u64>,
    pub(crate) accelerator_backends: Vec<String>,
}
