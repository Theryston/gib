use crate::ai::runtime::AiBackendError;
use crate::ai::runtime::{
    AiBackend, AiFinishReason, AiGenerationRequest, AiGenerationResult, AiGenerationStream,
    AiLoadedModel, AiStreamEvent, AiUsage,
};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Test-only backend that returns scripted complete model outputs. It allows
/// structured-generation tests to exercise malformed JSON and validation
/// retries without loading a native model.
#[derive(Clone)]
pub(crate) struct ScriptedAiBackend {
    scripts: Arc<Mutex<VecDeque<String>>>,
    loaded_model: Arc<Mutex<Option<String>>>,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    attempts: Arc<AtomicUsize>,
}

impl ScriptedAiBackend {
    pub(crate) fn new(scripts: Vec<&str>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(
                scripts
                    .into_iter()
                    .map(ToString::to_string)
                    .collect::<VecDeque<_>>(),
            )),
            loaded_model: Arc::new(Mutex::new(None)),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Acquire)
    }

    fn remove_cancellation(&self, request_id: &str) {
        if let Ok(mut cancellations) = self.cancellations.lock() {
            cancellations.remove(request_id);
        }
    }
}

#[async_trait]
impl AiBackend for ScriptedAiBackend {
    async fn load_model(&self, model_id: &str) -> Result<AiLoadedModel, AiBackendError> {
        if model_id.trim().is_empty() {
            return Err(AiBackendError::InvalidRequest(
                "model_id cannot be empty".to_string(),
            ));
        }
        let mut loaded_model = self
            .loaded_model
            .lock()
            .map_err(|_| AiBackendError::WorkerClosed)?;
        if let Some(current) = loaded_model.as_deref()
            && current != model_id
        {
            return Err(AiBackendError::ModelAlreadyLoaded {
                loaded_model_id: current.to_string(),
                requested_model_id: model_id.to_string(),
            });
        }
        *loaded_model = Some(model_id.to_string());
        Ok(AiLoadedModel {
            model_id: model_id.to_string(),
            version: "scripted-test-version".to_string(),
            context_size: 4096,
            gpu_layers: 0,
        })
    }

    async fn unload_model(&self, model_id: Option<&str>) -> Result<(), AiBackendError> {
        let mut loaded_model = self
            .loaded_model
            .lock()
            .map_err(|_| AiBackendError::WorkerClosed)?;
        if let (Some(requested), Some(current)) = (model_id, loaded_model.as_deref())
            && requested != current
        {
            return Err(AiBackendError::ModelMismatch {
                loaded_model_id: current.to_string(),
                requested_model_id: requested.to_string(),
            });
        }
        *loaded_model = None;
        Ok(())
    }

    async fn generate(
        &self,
        request: AiGenerationRequest,
    ) -> Result<AiGenerationStream, AiBackendError> {
        request.validate()?;
        let script = self
            .scripts
            .lock()
            .map_err(|_| AiBackendError::WorkerClosed)?
            .pop_front()
            .ok_or_else(|| AiBackendError::GenerationFailed {
                model_id: request.model_id.clone(),
            })?;
        let cancellation = Arc::new(AtomicBool::new(false));
        {
            let mut cancellations = self
                .cancellations
                .lock()
                .map_err(|_| AiBackendError::WorkerClosed)?;
            if cancellations.contains_key(&request.request_id) {
                return Err(AiBackendError::RequestAlreadyActive {
                    request_id: request.request_id,
                });
            }
            cancellations.insert(request.request_id.clone(), Arc::clone(&cancellation));
        }
        self.attempts.fetch_add(1, Ordering::AcqRel);

        let request_id = request.request_id.clone();
        let backend = self.clone();
        let worker_cancellation = Arc::clone(&cancellation);
        let (events_sender, events_receiver) = mpsc::channel(8);
        tokio::spawn(async move {
            let started = AiStreamEvent::Started {
                request_id: request.request_id.clone(),
                model_id: request.model_id.clone(),
            };
            if !send_event(&events_sender, started, &worker_cancellation).await {
                backend.remove_cancellation(&request.request_id);
                return;
            }
            let usage = AiUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            };
            if worker_cancellation.load(Ordering::Acquire) {
                let _ = send_event(
                    &events_sender,
                    AiStreamEvent::Cancelled {
                        request_id: request.request_id.clone(),
                        usage,
                    },
                    &worker_cancellation,
                )
                .await;
                backend.remove_cancellation(&request.request_id);
                return;
            }
            if !send_event(
                &events_sender,
                AiStreamEvent::TextDelta {
                    request_id: request.request_id.clone(),
                    text: script.clone(),
                },
                &worker_cancellation,
            )
            .await
            {
                backend.remove_cancellation(&request.request_id);
                return;
            }
            let _ = send_event(
                &events_sender,
                AiStreamEvent::Usage {
                    request_id: request.request_id.clone(),
                    usage,
                },
                &worker_cancellation,
            )
            .await;
            let _ = send_event(
                &events_sender,
                AiStreamEvent::Finished {
                    result: AiGenerationResult {
                        request_id: request.request_id.clone(),
                        model_id: request.model_id.clone(),
                        text: script,
                        finish_reason: AiFinishReason::EndOfGeneration,
                        usage,
                        duration_ms: 0,
                    },
                },
                &worker_cancellation,
            )
            .await;
            backend.remove_cancellation(&request.request_id);
        });

        Ok(AiGenerationStream::new(
            request_id,
            events_receiver,
            cancellation,
        ))
    }

    fn cancel(&self, request_id: &str) -> Result<(), AiBackendError> {
        let cancellations = self
            .cancellations
            .lock()
            .map_err(|_| AiBackendError::WorkerClosed)?;
        let cancellation =
            cancellations
                .get(request_id)
                .ok_or_else(|| AiBackendError::RequestNotFound {
                    request_id: request_id.to_string(),
                })?;
        cancellation.store(true, Ordering::Release);
        Ok(())
    }
}

async fn send_event(
    events: &mpsc::Sender<AiStreamEvent>,
    event: AiStreamEvent,
    cancellation: &AtomicBool,
) -> bool {
    if cancellation.load(Ordering::Acquire) && !event.is_terminal() {
        return false;
    }
    events.send(event).await.is_ok()
}
