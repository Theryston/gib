use super::api::{
    AiBackend, AiFinishReason, AiGenerationRequest, AiGenerationResult, AiGenerationStream,
    AiLoadedModel, AiStreamEvent, AiUsage,
};
use super::error::AiBackendError;
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const FAKE_MODEL_ID: &str = "fake-model";
const FAKE_VERSION: &str = "test-version";

/// Test-only implementation of the runtime boundary. Higher-level tests can
/// exercise streaming, cancellation, and serialization without loading a
/// multi-gigabyte native model.
#[derive(Clone)]
pub(crate) struct FakeAiBackend {
    loaded_model: Arc<Mutex<Option<String>>>,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    serial: Arc<tokio::sync::Mutex<()>>,
    running: Arc<AtomicUsize>,
    max_running: Arc<AtomicUsize>,
}

impl FakeAiBackend {
    pub(crate) fn new() -> Self {
        Self {
            loaded_model: Arc::new(Mutex::new(None)),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            serial: Arc::new(tokio::sync::Mutex::new(())),
            running: Arc::new(AtomicUsize::new(0)),
            max_running: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn cancel_all(&self) {
        if let Ok(active) = self.cancellations.lock() {
            for cancellation in active.values() {
                cancellation.store(true, Ordering::Release);
            }
        }
    }

    fn remove_cancellation(&self, request_id: &str) {
        if let Ok(mut active) = self.cancellations.lock() {
            active.remove(request_id);
        }
    }

    fn record_running(&self) -> RunningGuard {
        let running = self.running.fetch_add(1, Ordering::AcqRel) + 1;
        let mut maximum = self.max_running.load(Ordering::Acquire);
        while running > maximum {
            match self.max_running.compare_exchange(
                maximum,
                running,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => maximum = observed,
            }
        }
        RunningGuard {
            running: Arc::clone(&self.running),
        }
    }
}

struct RunningGuard {
    running: Arc<AtomicUsize>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.running.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait]
impl AiBackend for FakeAiBackend {
    async fn load_model(&self, model_id: &str) -> Result<AiLoadedModel, AiBackendError> {
        if model_id.trim().is_empty() {
            return Err(AiBackendError::InvalidRequest(
                "model_id cannot be empty".to_string(),
            ));
        }

        let _serial_guard = self.serial.lock().await;
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
            version: FAKE_VERSION.to_string(),
            context_size: 128,
            gpu_layers: 0,
        })
    }

    async fn unload_model(&self, model_id: Option<&str>) -> Result<(), AiBackendError> {
        if model_id.is_some_and(|value| value.trim().is_empty()) {
            return Err(AiBackendError::InvalidRequest(
                "model_id cannot be empty when supplied to unload_model".to_string(),
            ));
        }

        self.cancel_all();
        let _serial_guard = self.serial.lock().await;
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
        {
            let loaded_model = self
                .loaded_model
                .lock()
                .map_err(|_| AiBackendError::WorkerClosed)?;
            let Some(loaded_model) = loaded_model.as_deref() else {
                return Err(AiBackendError::ModelNotLoaded {
                    model_id: request.model_id,
                });
            };
            if loaded_model != request.model_id {
                return Err(AiBackendError::ModelMismatch {
                    loaded_model_id: loaded_model.to_string(),
                    requested_model_id: request.model_id,
                });
            }
        }

        let cancellation = Arc::new(AtomicBool::new(false));
        {
            let mut active = self
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

        let request_id = request.request_id.clone();
        let (events_sender, events_receiver) = mpsc::channel(4);
        let backend = self.clone();
        let worker_cancellation = Arc::clone(&cancellation);
        tokio::spawn(async move {
            let _serial_guard = backend.serial.lock().await;
            let _running_guard = backend.record_running();

            let started = AiStreamEvent::Started {
                request_id: request.request_id.clone(),
                model_id: request.model_id.clone(),
            };
            if !send_event(&events_sender, started, &worker_cancellation).await {
                backend.remove_cancellation(&request.request_id);
                return;
            }

            let mut completion_tokens = 0_u32;
            for text in ["hello", " world"] {
                if worker_cancellation.load(Ordering::Acquire) {
                    let usage = fake_usage(completion_tokens);
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

                completion_tokens += 1;
                if !send_event(
                    &events_sender,
                    AiStreamEvent::TextDelta {
                        request_id: request.request_id.clone(),
                        text: text.to_string(),
                    },
                    &worker_cancellation,
                )
                .await
                {
                    backend.remove_cancellation(&request.request_id);
                    return;
                }
                tokio::task::yield_now().await;
            }

            let usage = fake_usage(completion_tokens);
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
                        text: "hello world".to_string(),
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
        let active = self
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

fn fake_usage(completion_tokens: u32) -> AiUsage {
    AiUsage {
        prompt_tokens: 3,
        completion_tokens,
        total_tokens: 3 + completion_tokens,
    }
}

async fn send_event(
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
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
}

fn request(request_id: &str) -> AiGenerationRequest {
    AiGenerationRequest::new(request_id, FAKE_MODEL_ID, "test prompt")
}

async fn collect_events(mut stream: AiGenerationStream) -> Vec<AiStreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

fn terminal_count(events: &[AiStreamEvent]) -> usize {
    events.iter().filter(|event| event.is_terminal()).count()
}

#[tokio::test]
async fn fake_backend_reconstructs_streaming_output_through_trait_object() {
    let fake = FakeAiBackend::new();
    let backend: Arc<dyn AiBackend> = Arc::new(fake);
    let loaded = backend
        .load_model(FAKE_MODEL_ID)
        .await
        .expect("fake model should load");
    assert_eq!(loaded.model_id, FAKE_MODEL_ID);

    let events = collect_events(
        backend
            .generate(request("streaming"))
            .await
            .expect("fake generation should start"),
    )
    .await;

    assert!(matches!(
        events.first(),
        Some(AiStreamEvent::Started { .. })
    ));
    assert_eq!(terminal_count(&events), 1);
    assert!(matches!(
        events.last(),
        Some(AiStreamEvent::Finished { .. })
    ));

    let text = events
        .iter()
        .filter_map(|event| match event {
            AiStreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let AiStreamEvent::Finished { result } = events.last().expect("terminal event") else {
        panic!("expected a finished event");
    };
    assert_eq!(text, result.text);
    assert_eq!(result.usage.total_tokens, 5);
}

#[tokio::test]
async fn fake_backend_cancellation_has_one_cancelled_terminal_event() {
    let fake = FakeAiBackend::new();
    let backend: Arc<dyn AiBackend> = Arc::new(fake);
    backend
        .load_model(FAKE_MODEL_ID)
        .await
        .expect("fake model should load");
    let mut stream = backend
        .generate(request("cancelled"))
        .await
        .expect("fake generation should start");
    assert!(matches!(
        stream.next().await,
        Some(AiStreamEvent::Started { .. })
    ));

    backend
        .cancel("cancelled")
        .expect("active fake request should be cancellable");
    let events = collect_events(stream).await;
    assert_eq!(terminal_count(&events), 1);
    assert!(matches!(
        events.last(),
        Some(AiStreamEvent::Cancelled { .. })
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiStreamEvent::Finished { .. }))
    );
}

#[tokio::test]
async fn fake_backend_serializes_concurrent_generations() {
    let fake = FakeAiBackend::new();
    let backend: Arc<dyn AiBackend> = Arc::new(fake.clone());
    backend
        .load_model(FAKE_MODEL_ID)
        .await
        .expect("fake model should load");

    let first = backend
        .generate(request("first"))
        .await
        .expect("first generation should start");
    let second = backend
        .generate(request("second"))
        .await
        .expect("second generation should start");
    let (first_events, second_events) = tokio::join!(collect_events(first), collect_events(second));

    assert_eq!(terminal_count(&first_events), 1);
    assert_eq!(terminal_count(&second_events), 1);
    assert_eq!(fake.max_running.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn fake_backend_load_and_unload_are_deterministic() {
    let fake = FakeAiBackend::new();
    let backend: Arc<dyn AiBackend> = Arc::new(fake);

    assert!(matches!(
        backend.generate(request("before-load")).await,
        Err(AiBackendError::ModelNotLoaded { .. })
    ));
    backend
        .load_model(FAKE_MODEL_ID)
        .await
        .expect("fake model should load");
    assert!(matches!(
        backend.load_model("other-model").await,
        Err(AiBackendError::ModelAlreadyLoaded { .. })
    ));
    assert!(matches!(
        backend.unload_model(Some("other-model")).await,
        Err(AiBackendError::ModelMismatch { .. })
    ));
    backend
        .unload_model(Some(FAKE_MODEL_ID))
        .await
        .expect("loaded fake model should unload");
    assert!(matches!(
        backend.generate(request("after-unload")).await,
        Err(AiBackendError::ModelNotLoaded { .. })
    ));
}
