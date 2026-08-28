use super::api::{
    AiBackend, AiFinishReason, AiGenerationRequest, AiGenerationResult, AiGenerationStream,
    AiLoadedModel, AiStreamEvent, AiUsage,
};
use super::error::AiBackendError;
use super::options::{AiRuntimeCapabilities, AiRuntimeOptions};
use super::prompt::render_prompt;
use crate::ai::model::{DEFAULT_MODEL_ID, InstalledModel, ModelManager};
use async_trait::async_trait;
use encoding_rs::UTF_8;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const GGUF_MAGIC: &[u8; 4] = b"GGUF";

type CancellationRegistry = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// The process-wide backend is intentionally leaked after initialization so
/// it outlives every model and context created by runtime workers. llama.cpp
/// itself treats backend initialization as process-global.
static GLOBAL_LLAMA_BACKEND: OnceLock<Result<&'static LlamaBackend, AiBackendError>> =
    OnceLock::new();

fn global_llama_backend() -> Result<&'static LlamaBackend, AiBackendError> {
    let result = GLOBAL_LLAMA_BACKEND.get_or_init(|| {
        // Native diagnostics are never allowed to write directly to stdout or
        // stderr. They can be opted into through tracing for local debugging,
        // but are always suppressed for JSON mode.
        let allow_native_logs = !crate::output::is_json_mode()
            && std::env::var("GIB_LLAMA_NATIVE_LOGS")
                .ok()
                .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(allow_native_logs));

        LlamaBackend::init()
            .map(|backend| Box::leak(Box::new(backend)) as &'static LlamaBackend)
            .map_err(|_| AiBackendError::BackendInitializationFailed)
    });

    match result {
        Ok(backend) => Ok(*backend),
        Err(error) => Err(error.clone()),
    }
}

/// Factory for the in-process backend. It is deliberately independent from
/// the command and conversation layers so those layers can later receive a
/// fake backend or another runtime implementation.
#[derive(Clone)]
pub(crate) struct AiBackendFactory {
    model_manager: ModelManager,
    options: AiRuntimeOptions,
}

impl AiBackendFactory {
    pub(crate) fn new(model_manager: ModelManager) -> Self {
        Self {
            model_manager,
            options: AiRuntimeOptions::default(),
        }
    }

    pub(crate) fn with_options(mut self, options: AiRuntimeOptions) -> Self {
        self.options = options;
        self
    }

    pub(crate) fn build(self) -> Result<Arc<dyn AiBackend>, AiBackendError> {
        Ok(Arc::new(AiRuntime::new(self.model_manager, self.options)?))
    }
}

/// Async-facing runtime service backed by one dedicated blocking worker.
/// Requests are serialized by that worker because llama contexts and samplers
/// are not safe to use concurrently. The loaded model remains warm between
/// generations until `unload_model` is called.
pub(crate) struct AiRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    commands: mpsc::Sender<WorkerCommand>,
    cancellations: CancellationRegistry,
    model_manager: ModelManager,
    options: AiRuntimeOptions,
    accepting_requests: AtomicBool,
    loaded_model_id: tokio::sync::Mutex<Option<String>>,
    lifecycle: tokio::sync::Mutex<()>,
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        cancel_all(&self.cancellations);
        // Dropping the final command sender closes the worker receiver after
        // any active operation observes its cancellation flag. The worker is
        // intentionally detached; its owned model/context are released before
        // the thread exits.
    }
}

impl AiRuntime {
    pub(crate) fn new(
        model_manager: ModelManager,
        options: AiRuntimeOptions,
    ) -> Result<Self, AiBackendError> {
        options.validate()?;
        let backend = global_llama_backend()?;
        let (commands, receiver) = mpsc::channel(options.command_capacity);
        let cancellations = Arc::new(Mutex::new(HashMap::new()));
        let worker_cancellations = Arc::clone(&cancellations);
        let worker_options = options.clone();

        thread::Builder::new()
            .name("gib-ai-llama".to_string())
            .spawn(move || worker_loop(backend, receiver, worker_cancellations, worker_options))
            .map_err(|_| AiBackendError::WorkerClosed)?;

        Ok(Self {
            inner: Arc::new(RuntimeInner {
                commands,
                cancellations,
                model_manager,
                options,
                accepting_requests: AtomicBool::new(false),
                loaded_model_id: tokio::sync::Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
            }),
        })
    }

    pub(crate) async fn load_active_model(&self) -> Result<AiLoadedModel, AiBackendError> {
        let model_id = self
            .inner
            .model_manager
            .active_model_id()
            .map_err(|error| AiBackendError::from_model_error(DEFAULT_MODEL_ID, error))?
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());
        self.load_model(&model_id).await
    }

    pub(crate) fn capabilities(&self) -> Result<AiRuntimeCapabilities, AiBackendError> {
        let backend = global_llama_backend()?;
        Ok(AiRuntimeCapabilities {
            cpu: true,
            gpu_offload: backend.supports_gpu_offload(),
            mmap: backend.supports_mmap(),
            mlock: backend.supports_mlock(),
        })
    }

    pub(crate) fn options(&self) -> &AiRuntimeOptions {
        &self.inner.options
    }

    async fn resolve_verified_model(
        &self,
        model_id: &str,
    ) -> Result<VerifiedModel, AiBackendError> {
        let manager = self.inner.model_manager.clone();
        let model_id = model_id.to_string();
        tokio::task::spawn_blocking(move || resolve_verified_model(&manager, &model_id))
            .await
            .map_err(|_| AiBackendError::WorkerClosed)?
    }

    async fn send_load(&self, model: VerifiedModel) -> Result<AiLoadedModel, AiBackendError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.inner
            .commands
            .send(WorkerCommand::Load {
                model,
                response: response_sender,
            })
            .await
            .map_err(|_| AiBackendError::WorkerClosed)?;
        response_receiver
            .await
            .map_err(|_| AiBackendError::WorkerClosed)?
    }

    async fn send_unload(&self, model_id: Option<String>) -> Result<(), AiBackendError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.inner
            .commands
            .send(WorkerCommand::Unload {
                model_id,
                response: response_sender,
            })
            .await
            .map_err(|_| AiBackendError::WorkerClosed)?;
        response_receiver
            .await
            .map_err(|_| AiBackendError::WorkerClosed)?
    }
}

#[async_trait]
impl AiBackend for AiRuntime {
    async fn load_model(&self, model_id: &str) -> Result<AiLoadedModel, AiBackendError> {
        if model_id.trim().is_empty() {
            return Err(AiBackendError::InvalidRequest(
                "model_id cannot be empty".to_string(),
            ));
        }

        let _lifecycle_guard = self.inner.lifecycle.lock().await;
        let verified_model = self.resolve_verified_model(model_id).await?;
        let result = self.send_load(verified_model).await;
        if result.is_ok() {
            *self.inner.loaded_model_id.lock().await = Some(model_id.to_string());
            self.inner.accepting_requests.store(true, Ordering::Release);
        } else if matches!(&result, Err(AiBackendError::WorkerClosed)) {
            *self.inner.loaded_model_id.lock().await = None;
            self.inner
                .accepting_requests
                .store(false, Ordering::Release);
        }
        result
    }

    async fn unload_model(&self, model_id: Option<&str>) -> Result<(), AiBackendError> {
        if model_id.is_some_and(|value| value.trim().is_empty()) {
            return Err(AiBackendError::InvalidRequest(
                "model_id cannot be empty when supplied to unload_model".to_string(),
            ));
        }

        let _lifecycle_guard = self.inner.lifecycle.lock().await;
        if let Some(requested_model_id) = model_id {
            let loaded_model_id = self.inner.loaded_model_id.lock().await;
            if let Some(loaded_model_id) = loaded_model_id.as_deref()
                && loaded_model_id != requested_model_id
            {
                return Err(AiBackendError::ModelMismatch {
                    loaded_model_id: loaded_model_id.to_string(),
                    requested_model_id: requested_model_id.to_string(),
                });
            }
        }
        // This is done before enqueueing the command so an unload request
        // immediately stops new generations while an active decode winds down.
        self.inner
            .accepting_requests
            .store(false, Ordering::Release);
        cancel_all(&self.inner.cancellations);
        let result = self.send_unload(model_id.map(ToString::to_string)).await;
        match &result {
            Ok(()) => {
                *self.inner.loaded_model_id.lock().await = None;
            }
            Err(AiBackendError::WorkerClosed) => {
                *self.inner.loaded_model_id.lock().await = None;
            }
            Err(_) => {
                if self.inner.loaded_model_id.lock().await.is_some() {
                    self.inner.accepting_requests.store(true, Ordering::Release);
                }
            }
        }
        result
    }

    async fn generate(
        &self,
        request: AiGenerationRequest,
    ) -> Result<AiGenerationStream, AiBackendError> {
        request.validate()?;
        // Admission shares the lifecycle lock with load/unload. This closes
        // the race where an unload could cancel the registry and enqueue its
        // command while a generation was still between its state check and
        // command submission.
        let _lifecycle_guard = self.inner.lifecycle.lock().await;
        if !self.inner.accepting_requests.load(Ordering::Acquire) {
            return Err(AiBackendError::ModelNotLoaded {
                model_id: request.model_id,
            });
        }

        let cancellation = Arc::new(AtomicBool::new(false));
        {
            let mut active = self
                .inner
                .cancellations
                .lock()
                .map_err(|_| AiBackendError::WorkerClosed)?;
            if active.contains_key(&request.request_id) {
                return Err(AiBackendError::RequestAlreadyActive {
                    request_id: request.request_id,
                });
            }
            active.insert(request.request_id.clone(), Arc::clone(&cancellation));
        }

        let (events_sender, events_receiver) = mpsc::channel(self.inner.options.stream_capacity);
        let request_id = request.request_id.clone();
        let command = WorkerCommand::Generate {
            request,
            events: events_sender,
            cancellation: Arc::clone(&cancellation),
        };
        if self.inner.commands.send(command).await.is_err() {
            remove_cancellation(&self.inner.cancellations, &request_id);
            self.inner
                .accepting_requests
                .store(false, Ordering::Release);
            return Err(AiBackendError::WorkerClosed);
        }

        Ok(AiGenerationStream::new(
            request_id,
            events_receiver,
            cancellation,
        ))
    }

    fn cancel(&self, request_id: &str) -> Result<(), AiBackendError> {
        let active = self
            .inner
            .cancellations
            .lock()
            .map_err(|_| AiBackendError::WorkerClosed)?;
        let cancellation =
            active
                .get(request_id)
                .ok_or_else(|| AiBackendError::RequestNotFound {
                    request_id: request_id.to_string(),
                })?;
        cancellation.store(true, Ordering::Release);
        Ok(())
    }
}

struct VerifiedModel {
    model_id: String,
    version: String,
    artifact_path: PathBuf,
}

struct LoadedModel {
    model_id: String,
    version: String,
    model: LlamaModel,
}

enum WorkerCommand {
    Load {
        model: VerifiedModel,
        response: oneshot::Sender<Result<AiLoadedModel, AiBackendError>>,
    },
    Unload {
        model_id: Option<String>,
        response: oneshot::Sender<Result<(), AiBackendError>>,
    },
    Generate {
        request: AiGenerationRequest,
        events: mpsc::Sender<AiStreamEvent>,
        cancellation: Arc<AtomicBool>,
    },
}

fn worker_loop(
    backend: &'static LlamaBackend,
    mut receiver: mpsc::Receiver<WorkerCommand>,
    cancellations: CancellationRegistry,
    options: AiRuntimeOptions,
) {
    let mut loaded_model = None;
    while let Some(command) = receiver.blocking_recv() {
        match command {
            WorkerCommand::Load { model, response } => {
                let result = load_model_on_worker(backend, &mut loaded_model, model, &options);
                let _ = response.send(result);
            }
            WorkerCommand::Unload { model_id, response } => {
                let result = unload_model_on_worker(&mut loaded_model, model_id);
                let _ = response.send(result);
            }
            WorkerCommand::Generate {
                request,
                events,
                cancellation,
            } => {
                run_generation(
                    backend,
                    loaded_model.as_ref(),
                    request,
                    events,
                    cancellation,
                    &cancellations,
                    &options,
                );
            }
        }
    }
    drop(loaded_model);
}

fn load_model_on_worker(
    backend: &'static LlamaBackend,
    loaded_model: &mut Option<LoadedModel>,
    verified_model: VerifiedModel,
    options: &AiRuntimeOptions,
) -> Result<AiLoadedModel, AiBackendError> {
    if let Some(current) = loaded_model.as_ref() {
        if current.model_id == verified_model.model_id && current.version == verified_model.version
        {
            return Ok(AiLoadedModel {
                model_id: current.model_id.clone(),
                version: current.version.clone(),
                context_size: options.context_size,
                gpu_layers: options.n_gpu_layers,
            });
        }
        return Err(AiBackendError::ModelAlreadyLoaded {
            loaded_model_id: current.model_id.clone(),
            requested_model_id: verified_model.model_id,
        });
    }

    if options.n_gpu_layers > 0 && !backend.supports_gpu_offload() {
        return Err(AiBackendError::CapabilityUnavailable {
            capability: "GPU offload was requested, but this build has no usable GPU backend"
                .to_string(),
        });
    }

    let model_params = LlamaModelParams::default()
        .with_n_gpu_layers(options.n_gpu_layers)
        .with_use_mmap(backend.supports_mmap());
    let model = LlamaModel::load_from_file(backend, &verified_model.artifact_path, &model_params)
        .map_err(|_| AiBackendError::load_native(&verified_model.model_id))?;

    let model_id = verified_model.model_id;
    let version = verified_model.version;
    *loaded_model = Some(LoadedModel {
        model_id: model_id.clone(),
        version: version.clone(),
        model,
    });
    Ok(AiLoadedModel {
        model_id,
        version,
        context_size: options.context_size,
        gpu_layers: options.n_gpu_layers,
    })
}

fn unload_model_on_worker(
    loaded_model: &mut Option<LoadedModel>,
    requested_model_id: Option<String>,
) -> Result<(), AiBackendError> {
    if let (Some(requested), Some(current)) = (requested_model_id, loaded_model.as_ref())
        && requested != current.model_id
    {
        return Err(AiBackendError::ModelMismatch {
            loaded_model_id: current.model_id.clone(),
            requested_model_id: requested,
        });
    }
    loaded_model.take();
    Ok(())
}

fn resolve_verified_model(
    model_manager: &ModelManager,
    model_id: &str,
) -> Result<VerifiedModel, AiBackendError> {
    let installed = model_manager
        .verify_installed(model_id)
        .map_err(|error| AiBackendError::from_model_error(model_id, error))?;
    validate_verified_artifact(model_id, &installed)?;
    Ok(VerifiedModel {
        model_id: installed.manifest.id,
        version: installed.manifest.version,
        artifact_path: installed.artifact_path,
    })
}

fn validate_verified_artifact(
    model_id: &str,
    installed: &InstalledModel,
) -> Result<(), AiBackendError> {
    let metadata = std::fs::symlink_metadata(&installed.artifact_path).map_err(|_| {
        AiBackendError::ModelNotInstalled {
            model_id: model_id.to_string(),
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AiBackendError::InvalidGguf {
            model_id: model_id.to_string(),
        });
    }
    if metadata.len() != installed.verified_size {
        return Err(AiBackendError::ModelIntegrity {
            model_id: model_id.to_string(),
        });
    }

    let mut file =
        File::open(&installed.artifact_path).map_err(|_| AiBackendError::InvalidGguf {
            model_id: model_id.to_string(),
        })?;
    let mut magic = [0_u8; GGUF_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|_| AiBackendError::InvalidGguf {
            model_id: model_id.to_string(),
        })?;
    if &magic != GGUF_MAGIC {
        return Err(AiBackendError::InvalidGguf {
            model_id: model_id.to_string(),
        });
    }
    Ok(())
}

fn run_generation(
    backend: &'static LlamaBackend,
    loaded_model: Option<&LoadedModel>,
    request: AiGenerationRequest,
    events: mpsc::Sender<AiStreamEvent>,
    cancellation: Arc<AtomicBool>,
    cancellations: &CancellationRegistry,
    options: &AiRuntimeOptions,
) {
    let request_id = request.request_id.clone();
    let model_id = request.model_id.clone();
    if !send_event(
        &events,
        AiStreamEvent::Started {
            request_id: request_id.clone(),
            model_id: model_id.clone(),
        },
        &cancellation,
    ) {
        remove_cancellation(cancellations, &request_id);
        return;
    }

    let started_at = Instant::now();
    let end = match loaded_model {
        None => GenerationEnd::Failed {
            error: AiBackendError::ModelNotLoaded { model_id },
            usage: AiUsage::default(),
        },
        Some(loaded_model) if loaded_model.model_id != request.model_id => GenerationEnd::Failed {
            error: AiBackendError::ModelMismatch {
                loaded_model_id: loaded_model.model_id.clone(),
                requested_model_id: request.model_id.clone(),
            },
            usage: AiUsage::default(),
        },
        Some(loaded_model) => generate_tokens(
            backend,
            loaded_model,
            &request,
            &events,
            &cancellation,
            options,
        ),
    };

    match end {
        GenerationEnd::Finished(mut result) => {
            result.duration_ms = elapsed_millis(started_at);
            let usage = result.usage;
            let _ = send_event(
                &events,
                AiStreamEvent::Usage {
                    request_id: request_id.clone(),
                    usage,
                },
                &cancellation,
            );
            let _ = send_event(&events, AiStreamEvent::Finished { result }, &cancellation);
        }
        GenerationEnd::Cancelled { usage } => {
            let _ = send_event(
                &events,
                AiStreamEvent::Usage {
                    request_id: request_id.clone(),
                    usage,
                },
                &cancellation,
            );
            let _ = send_event(
                &events,
                AiStreamEvent::Cancelled {
                    request_id: request_id.clone(),
                    usage,
                },
                &cancellation,
            );
        }
        GenerationEnd::Failed { error, usage } => {
            let _ = send_event(
                &events,
                AiStreamEvent::Usage {
                    request_id: request_id.clone(),
                    usage,
                },
                &cancellation,
            );
            let _ = send_event(
                &events,
                AiStreamEvent::Failed {
                    request_id: request_id.clone(),
                    error,
                },
                &cancellation,
            );
        }
    }
    remove_cancellation(cancellations, &request_id);
}

enum GenerationEnd {
    Finished(AiGenerationResult),
    Cancelled {
        usage: AiUsage,
    },
    Failed {
        error: AiBackendError,
        usage: AiUsage,
    },
}

fn generate_tokens(
    backend: &'static LlamaBackend,
    loaded_model: &LoadedModel,
    request: &AiGenerationRequest,
    events: &mpsc::Sender<AiStreamEvent>,
    cancellation: &AtomicBool,
    options: &AiRuntimeOptions,
) -> GenerationEnd {
    if cancellation.load(Ordering::Acquire) {
        return GenerationEnd::Cancelled {
            usage: AiUsage::default(),
        };
    }
    if request.grammar.is_some() {
        return GenerationEnd::Failed {
            error: AiBackendError::UnsupportedFeature {
                feature: "grammar-constrained generation".to_string(),
            },
            usage: AiUsage::default(),
        };
    }

    let context_limit = options.context_size_for(request.context_limit);
    let prompt = match render_prompt(&loaded_model.model, &request.messages) {
        Ok(prompt) => prompt,
        Err(error) => {
            return GenerationEnd::Failed {
                error,
                usage: AiUsage::default(),
            };
        }
    };
    let prompt_tokens = match loaded_model.model.str_to_token(&prompt, AddBos::Always) {
        Ok(tokens) => tokens,
        Err(_) => {
            return GenerationEnd::Failed {
                error: AiBackendError::GenerationFailed {
                    model_id: request.model_id.clone(),
                },
                usage: AiUsage::default(),
            };
        }
    };
    if prompt_tokens.is_empty() {
        return GenerationEnd::Failed {
            error: AiBackendError::GenerationFailed {
                model_id: request.model_id.clone(),
            },
            usage: AiUsage::default(),
        };
    }
    let prompt_token_count = match u32::try_from(prompt_tokens.len()) {
        Ok(value) => value,
        Err(_) => {
            return GenerationEnd::Failed {
                error: AiBackendError::ContextExhausted {
                    context_limit,
                    prompt_tokens: u32::MAX,
                    requested_output_tokens: request.max_output_tokens,
                },
                usage: AiUsage::default(),
            };
        }
    };
    if u64::from(prompt_token_count) + u64::from(request.max_output_tokens)
        > u64::from(context_limit)
    {
        return GenerationEnd::Failed {
            error: AiBackendError::ContextExhausted {
                context_limit,
                prompt_tokens: prompt_token_count,
                requested_output_tokens: request.max_output_tokens,
            },
            usage: AiUsage {
                prompt_tokens: prompt_token_count,
                ..AiUsage::default()
            },
        };
    }

    let context_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(context_limit))
        .with_n_batch(options.batch_size.min(context_limit))
        .with_n_ubatch(options.micro_batch_size.min(context_limit))
        .with_n_seq_max(1)
        .with_n_threads(options.threads as i32)
        .with_n_threads_batch(options.batch_threads as i32)
        .with_offload_kqv(options.offload_kqv && options.n_gpu_layers > 0)
        .with_no_perf(true);
    let mut context = match loaded_model.model.new_context(backend, context_params) {
        Ok(context) => context,
        Err(_) => {
            return GenerationEnd::Failed {
                error: AiBackendError::context_native(&request.model_id),
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    ..AiUsage::default()
                },
            };
        }
    };

    let batch_size = options.batch_size.min(context_limit) as usize;
    let mut batch = LlamaBatch::new(batch_size, 1);
    for (chunk_index, chunk) in prompt_tokens.chunks(batch_size).enumerate() {
        if cancellation.load(Ordering::Acquire) {
            return GenerationEnd::Cancelled {
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    ..AiUsage::default()
                },
            };
        }
        batch.clear();
        let chunk_start = chunk_index * batch_size;
        for (offset, token) in chunk.iter().enumerate() {
            let position = chunk_start + offset;
            let position = match i32::try_from(position) {
                Ok(value) => value,
                Err(_) => {
                    return GenerationEnd::Failed {
                        error: AiBackendError::ContextExhausted {
                            context_limit,
                            prompt_tokens: prompt_token_count,
                            requested_output_tokens: request.max_output_tokens,
                        },
                        usage: AiUsage {
                            prompt_tokens: prompt_token_count,
                            ..AiUsage::default()
                        },
                    };
                }
            };
            if batch
                .add(
                    *token,
                    position,
                    &[0],
                    position as usize == prompt_tokens.len() - 1,
                )
                .is_err()
            {
                return GenerationEnd::Failed {
                    error: AiBackendError::GenerationFailed {
                        model_id: request.model_id.clone(),
                    },
                    usage: AiUsage {
                        prompt_tokens: prompt_token_count,
                        ..AiUsage::default()
                    },
                };
            }
        }
        if context.decode(&mut batch).is_err() {
            return GenerationEnd::Failed {
                error: AiBackendError::GenerationFailed {
                    model_id: request.model_id.clone(),
                },
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    ..AiUsage::default()
                },
            };
        }
    }

    let mut sampler = build_sampler(&request.sampling);
    let max_stop_bytes = request
        .stop_sequences
        .iter()
        .map(String::len)
        .max()
        .unwrap_or(0);
    let mut pending_text = String::new();
    let mut final_text = String::new();
    let mut decoder = UTF_8.new_decoder();
    let mut completion_tokens = 0_u32;
    let mut next_position = prompt_tokens.len();
    let finish_reason;

    loop {
        if cancellation.load(Ordering::Acquire) {
            return GenerationEnd::Cancelled {
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    completion_tokens,
                    total_tokens: prompt_token_count.saturating_add(completion_tokens),
                },
            };
        }

        let token = sampler.sample(&context, batch.n_tokens() - 1);
        sampler.accept(token);
        if loaded_model.model.is_eog_token(token) {
            finish_reason = AiFinishReason::EndOfGeneration;
            break;
        }

        completion_tokens = completion_tokens.saturating_add(1);
        let piece = match loaded_model
            .model
            .token_to_piece(token, &mut decoder, true, None)
        {
            Ok(piece) => piece,
            Err(_) => {
                return GenerationEnd::Failed {
                    error: AiBackendError::GenerationFailed {
                        model_id: request.model_id.clone(),
                    },
                    usage: AiUsage {
                        prompt_tokens: prompt_token_count,
                        completion_tokens,
                        total_tokens: prompt_token_count.saturating_add(completion_tokens),
                    },
                };
            }
        };
        pending_text.push_str(&piece);

        if let Some(stop_length) = matching_stop_length(&pending_text, &request.stop_sequences) {
            let text_without_stop = pending_text.len() - stop_length;
            let prefix = pending_text[..text_without_stop].to_string();
            pending_text.clear();
            if !send_text_delta(
                events,
                &request.request_id,
                &prefix,
                &mut final_text,
                cancellation,
            ) {
                return GenerationEnd::Cancelled {
                    usage: AiUsage {
                        prompt_tokens: prompt_token_count,
                        completion_tokens,
                        total_tokens: prompt_token_count.saturating_add(completion_tokens),
                    },
                };
            }
            finish_reason = AiFinishReason::StopSequence;
            break;
        }

        if !flush_safe_text(
            &mut pending_text,
            max_stop_bytes,
            events,
            &request.request_id,
            &mut final_text,
            cancellation,
        ) {
            return GenerationEnd::Cancelled {
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    completion_tokens,
                    total_tokens: prompt_token_count.saturating_add(completion_tokens),
                },
            };
        }

        if completion_tokens >= request.max_output_tokens {
            finish_reason = AiFinishReason::MaxOutputTokens;
            break;
        }

        batch.clear();
        let next_position_i32 = match i32::try_from(next_position) {
            Ok(value) => value,
            Err(_) => {
                return GenerationEnd::Failed {
                    error: AiBackendError::ContextExhausted {
                        context_limit,
                        prompt_tokens: prompt_token_count,
                        requested_output_tokens: request.max_output_tokens,
                    },
                    usage: AiUsage {
                        prompt_tokens: prompt_token_count,
                        completion_tokens,
                        total_tokens: prompt_token_count.saturating_add(completion_tokens),
                    },
                };
            }
        };
        if batch.add(token, next_position_i32, &[0], true).is_err() {
            return GenerationEnd::Failed {
                error: AiBackendError::GenerationFailed {
                    model_id: request.model_id.clone(),
                },
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    completion_tokens,
                    total_tokens: prompt_token_count.saturating_add(completion_tokens),
                },
            };
        }
        if context.decode(&mut batch).is_err() {
            return GenerationEnd::Failed {
                error: AiBackendError::GenerationFailed {
                    model_id: request.model_id.clone(),
                },
                usage: AiUsage {
                    prompt_tokens: prompt_token_count,
                    completion_tokens,
                    total_tokens: prompt_token_count.saturating_add(completion_tokens),
                },
            };
        }
        next_position = next_position.saturating_add(1);
    }

    if !flush_all_text(
        &mut pending_text,
        events,
        &request.request_id,
        &mut final_text,
        cancellation,
    ) {
        return GenerationEnd::Cancelled {
            usage: AiUsage {
                prompt_tokens: prompt_token_count,
                completion_tokens,
                total_tokens: prompt_token_count.saturating_add(completion_tokens),
            },
        };
    }

    let usage = AiUsage {
        prompt_tokens: prompt_token_count,
        completion_tokens,
        total_tokens: prompt_token_count.saturating_add(completion_tokens),
    };
    GenerationEnd::Finished(AiGenerationResult {
        request_id: request.request_id.clone(),
        model_id: request.model_id.clone(),
        text: final_text,
        finish_reason,
        usage,
        duration_ms: 0,
    })
}

fn build_sampler(settings: &super::api::AiSamplingSettings) -> LlamaSampler {
    let mut samplers = Vec::new();
    if settings.top_k > 0 {
        samplers.push(LlamaSampler::top_k(
            settings.top_k.min(i32::MAX as u32) as i32
        ));
    }
    if settings.top_p < 1.0 {
        samplers.push(LlamaSampler::top_p(settings.top_p, 1));
    }
    if settings.min_p > 0.0 {
        samplers.push(LlamaSampler::min_p(settings.min_p, 1));
    }
    if settings.temperature == 0.0 {
        samplers.push(LlamaSampler::greedy());
    } else {
        samplers.push(LlamaSampler::temp(settings.temperature));
        samplers.push(LlamaSampler::dist(settings.seed.unwrap_or(0)));
    }
    LlamaSampler::chain(samplers, true)
}

fn matching_stop_length(text: &str, stop_sequences: &[String]) -> Option<usize> {
    stop_sequences
        .iter()
        .filter(|stop| text.ends_with(stop.as_str()))
        .map(String::len)
        .max()
}

fn flush_safe_text(
    pending: &mut String,
    max_stop_bytes: usize,
    events: &mpsc::Sender<AiStreamEvent>,
    request_id: &str,
    final_text: &mut String,
    cancellation: &AtomicBool,
) -> bool {
    let bytes_to_hold = max_stop_bytes.saturating_sub(1);
    if pending.len() <= bytes_to_hold {
        return true;
    }
    let mut split_at = pending.len() - bytes_to_hold;
    while split_at > 0 && !pending.is_char_boundary(split_at) {
        split_at -= 1;
    }
    if split_at == 0 {
        return true;
    }
    let prefix: String = pending.drain(..split_at).collect();
    send_text_delta(events, request_id, &prefix, final_text, cancellation)
}

fn flush_all_text(
    pending: &mut String,
    events: &mpsc::Sender<AiStreamEvent>,
    request_id: &str,
    final_text: &mut String,
    cancellation: &AtomicBool,
) -> bool {
    if pending.is_empty() {
        return true;
    }
    let text = std::mem::take(pending);
    send_text_delta(events, request_id, &text, final_text, cancellation)
}

fn send_text_delta(
    events: &mpsc::Sender<AiStreamEvent>,
    request_id: &str,
    text: &str,
    final_text: &mut String,
    cancellation: &AtomicBool,
) -> bool {
    if text.is_empty() {
        return true;
    }
    final_text.push_str(text);
    send_event(
        events,
        AiStreamEvent::TextDelta {
            request_id: request_id.to_string(),
            text: text.to_string(),
        },
        cancellation,
    )
}

fn send_event(
    events: &mpsc::Sender<AiStreamEvent>,
    event: AiStreamEvent,
    cancellation: &AtomicBool,
) -> bool {
    let terminal = event.is_terminal();
    let mut event = event;
    let mut cancelled_terminal_deadline = None;

    loop {
        match events.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                cancellation.store(true, Ordering::Release);
                return false;
            }
            Err(mpsc::error::TrySendError::Full(returned_event)) => {
                event = returned_event;
                if cancellation.load(Ordering::Acquire) {
                    if !terminal {
                        return false;
                    }
                    let deadline = cancelled_terminal_deadline
                        .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
                    if Instant::now() >= *deadline {
                        return false;
                    }
                }
                // This worker is deliberately blocking, but it must still be
                // able to observe cancellation while a consumer is slow or
                // has stopped draining a bounded stream.
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn cancel_all(cancellations: &CancellationRegistry) {
    if let Ok(active) = cancellations.lock() {
        for cancellation in active.values() {
            cancellation.store(true, Ordering::Release);
        }
    }
}

fn remove_cancellation(cancellations: &CancellationRegistry, request_id: &str) {
    if let Ok(mut active) = cancellations.lock() {
        active.remove(request_id);
    }
}

fn elapsed_millis(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::model::{ModelPaths, ModelRegistry};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_root() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("gib-ai-runtime-{suffix}"))
    }

    #[test]
    fn stop_matching_prefers_the_longest_suffix() {
        let stops = vec!["<|eot|>".to_string(), "<|eot_id|>".to_string()];
        assert_eq!(matching_stop_length("hello<|eot_id|>", &stops), Some(10));
        assert_eq!(matching_stop_length("hello", &stops), None);
    }

    #[test]
    fn cancelled_bounded_event_send_does_not_wait_for_a_slow_consumer() {
        let (events, _receiver) = mpsc::channel(1);
        events
            .try_send(AiStreamEvent::Started {
                request_id: "request".to_string(),
                model_id: "model".to_string(),
            })
            .expect("the test channel should accept its first event");
        let cancellation = AtomicBool::new(true);
        let started_at = Instant::now();
        assert!(!send_event(
            &events,
            AiStreamEvent::TextDelta {
                request_id: "request".to_string(),
                text: "delayed".to_string(),
            },
            &cancellation,
        ));
        assert!(started_at.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn runtime_rejects_a_missing_verified_model_without_downloading() {
        let root = unique_test_root();
        let paths = ModelPaths::from_root(&root);
        let manager = ModelManager::new(ModelRegistry::default(), paths)
            .expect("model manager should be constructible");
        let runtime = AiRuntime::new(
            manager,
            AiRuntimeOptions::default()
                .with_context_size(128)
                .with_batch_size(32)
                .with_micro_batch_size(32),
        )
        .expect("runtime worker should start");
        let error = runtime
            .load_model(DEFAULT_MODEL_ID)
            .await
            .expect_err("missing model should be rejected");
        assert!(matches!(error, AiBackendError::ModelNotInstalled { .. }));
        let _ = std::fs::remove_dir_all(root);
    }
}
