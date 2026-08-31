use gib::{
    CancellationInfo, ClientBuilder, ErrorCode, EventConsumer, EventDispatcher, EventEnvelope,
    EventKind, EventPayload, EventPhase, OperationKind, OperationRequest, OperationStatus,
    Progress, RecoveryPoint, Request, SdkError,
};
use std::error::Error;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

fn operation_id() -> Result<gib::OperationId, Box<dyn Error>> {
    gib::OperationId::from_u64(9_000)
        .ok_or_else(|| std::io::Error::other("test operation identifier must be non-zero"))
        .map_err(Into::into)
}

fn event(
    operation_id: gib::OperationId,
    sequence: u64,
    kind: EventKind,
    phase: EventPhase,
    payload: EventPayload,
) -> Result<EventEnvelope, Box<dyn Error>> {
    EventEnvelope::new(operation_id, sequence, kind, phase, payload).map_err(Into::into)
}

fn receive_event(receiver: &Receiver<EventEnvelope>) -> Result<EventEnvelope, Box<dyn Error>> {
    receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(Into::into)
}

struct ExternalConsumer {
    sender: mpsc::Sender<EventEnvelope>,
}

impl EventConsumer for ExternalConsumer {
    fn on_event(&mut self, event: EventEnvelope) {
        let _ = self.sender.send(event);
    }
}

#[test]
fn external_consumer_uses_only_the_public_sdk_surface() -> Result<(), Box<dyn Error>> {
    fn assert_request<R: Request<Response = gib::OperationHandle>>() {}
    assert_request::<OperationRequest>();

    let client = gib::Client::builder().event_buffer_capacity(4).build()?;
    let (sender, receiver) = mpsc::channel();
    let subscription = client.register_event_consumer(ExternalConsumer { sender })?;

    let operation = client.start_operation(OperationRequest::new(OperationKind::Backup))?;
    let started = receive_event(&receiver)?;
    assert_eq!(started.operation_id(), operation.id());
    assert_eq!(started.schema_version(), gib::EVENT_SCHEMA_VERSION);
    assert_eq!(started.kind(), EventKind::Started);
    assert_eq!(started.phase(), EventPhase::Starting);

    let cancellation = operation.cancellation_handle();
    let result = operation.cancel()?;
    assert_eq!(result.status(), OperationStatus::Cancelled);
    assert!(result.is_cancelled());
    assert!(cancellation.is_cancelled());

    let cancelled = receive_event(&receiver)?;
    assert_eq!(cancelled.operation_id(), operation.id());
    assert_eq!(cancelled.sequence(), 2);
    assert_eq!(cancelled.kind(), EventKind::Cancelled);
    assert_eq!(
        cancelled.payload(),
        &EventPayload::Cancellation(CancellationInfo::new(
            true,
            RecoveryPoint::OperationBoundary,
        ))
    );
    assert_eq!(operation.status(), OperationStatus::Cancelled);

    let repeated = operation.cancel()?;
    assert_eq!(repeated, result);
    assert!(matches!(
        operation.complete(),
        Err(SdkError::OperationStateConflict { .. })
    ));

    subscription.close();
    assert_eq!(client.events().consumer_count(), 0);
    Ok(())
}

#[cfg(feature = "s3")]
#[test]
fn s3_backend_is_exposed_through_provider_neutral_sdk_types() -> Result<(), Box<dyn Error>> {
    let config =
        gib::S3StorageConfig::new("us-east-1", "gib-public-api", "access-key", "secret-key")?
            .with_endpoint("http://127.0.0.1:9000")
            .without_capability_cache();
    let storage = gib::S3Storage::new(config)?;
    let handle: gib::StorageHandle = (&storage).into();
    assert_eq!(
        storage.conditional_write_capabilities().create_if_absent(),
        gib::S3ConditionalWriteStatus::Inconclusive
    );
    assert!(
        handle
            .as_storage()
            .capabilities()
            .contains(gib::StorageCapabilities::ALL)
    );
    Ok(())
}

#[test]
fn builder_rejects_an_unbounded_by_zero_event_queue() {
    let error = ClientBuilder::new().event_buffer_capacity(0).build().err();
    assert!(matches!(
        error,
        Some(SdkError::InvalidConfiguration {
            field: "event_buffer_capacity",
            ..
        })
    ));
    assert_eq!(
        error.as_ref().map(SdkError::code),
        Some(ErrorCode::InvalidConfiguration)
    );
}

#[test]
fn operation_progress_is_structured_and_cancellation_is_cooperative() -> Result<(), Box<dyn Error>>
{
    let client = ClientBuilder::new().event_buffer_capacity(8).build()?;
    let (sender, receiver) = mpsc::channel();
    let _subscription = client.register_event_consumer(move |event| {
        let _ = sender.send(event);
    })?;
    let operation = client.create_operation(OperationKind::Search)?;
    let _ = receive_event(&receiver)?;

    let progress = Progress::new(3, Some(10))?;
    let delivery = operation.report_progress(progress)?;
    assert_eq!(delivery.consumer_count(), 1);
    assert!(delivery.delivered_to_all());

    let progress_event = receive_event(&receiver)?;
    assert_eq!(progress_event.kind(), EventKind::Progress);
    assert_eq!(progress_event.phase(), EventPhase::Running);
    assert_eq!(progress_event.payload(), &EventPayload::Progress(progress));
    assert_eq!(progress.fraction(), Some(0.3));

    let invalid_progress = Progress::new(11, Some(10));
    assert!(matches!(
        invalid_progress,
        Err(SdkError::InvalidRequest { .. })
    ));
    Ok(())
}

#[test]
fn bounded_dispatcher_coalesces_progress_and_preserves_critical_events()
-> Result<(), Box<dyn Error>> {
    let dispatcher = EventDispatcher::new(2)?;
    let operation_id = operation_id()?;
    let (first_started_sender, first_started_receiver) = mpsc::channel();
    let (release_sender, release_receiver): (SyncSender<()>, Receiver<()>) = mpsc::sync_channel(0);
    let (received_sender, received_receiver) = mpsc::channel();

    let subscription = dispatcher.register_consumer(move |event: EventEnvelope| {
        if event.sequence() == 1 {
            let _ = first_started_sender.send(());
            let _ = release_receiver.recv();
        }
        let _ = received_sender.send(event);
    })?;

    let started = event(
        operation_id,
        1,
        EventKind::Started,
        EventPhase::Starting,
        EventPayload::Empty,
    )?;
    let _ = dispatcher.publish(started);
    first_started_receiver.recv_timeout(Duration::from_secs(2))?;

    let progress_two = event(
        operation_id,
        2,
        EventKind::Progress,
        EventPhase::Running,
        EventPayload::Progress(Progress::indeterminate(2)),
    )?;
    let progress_three = event(
        operation_id,
        3,
        EventKind::Progress,
        EventPhase::Running,
        EventPayload::Progress(Progress::indeterminate(3)),
    )?;
    let progress_four = event(
        operation_id,
        4,
        EventKind::Progress,
        EventPhase::Running,
        EventPayload::Progress(Progress::indeterminate(4)),
    )?;
    assert_eq!(dispatcher.publish(progress_two).delivered_count(), 1);
    assert_eq!(dispatcher.publish(progress_three).delivered_count(), 1);
    assert_eq!(dispatcher.publish(progress_four).coalesced_count(), 1);

    let completed = event(
        operation_id,
        5,
        EventKind::Completed,
        EventPhase::Completed,
        EventPayload::Empty,
    )?;
    let completion_delivery = dispatcher.publish(completed);
    assert_eq!(completion_delivery.delivered_count(), 1);
    assert_eq!(completion_delivery.evicted_progress_count(), 1);

    release_sender.send(())?;
    let first = receive_event(&received_receiver)?;
    let second = receive_event(&received_receiver)?;
    let third = receive_event(&received_receiver)?;
    assert_eq!(first.kind(), EventKind::Started);
    assert_eq!(second.kind(), EventKind::Progress);
    assert_eq!(second.sequence(), 4);
    assert_eq!(third.kind(), EventKind::Completed);
    assert!(third.is_critical());

    subscription.close();
    Ok(())
}

#[test]
fn full_critical_queue_drops_only_progress() -> Result<(), Box<dyn Error>> {
    let dispatcher = EventDispatcher::new(2)?;
    let operation_id = operation_id()?;
    let (first_started_sender, first_started_receiver) = mpsc::channel();
    let (release_sender, release_receiver): (SyncSender<()>, Receiver<()>) = mpsc::sync_channel(0);
    let (received_sender, received_receiver) = mpsc::channel();
    let subscription = dispatcher.register_consumer(move |event: EventEnvelope| {
        if event.sequence() == 1 {
            let _ = first_started_sender.send(());
            let _ = release_receiver.recv();
        }
        let _ = received_sender.send(event);
    })?;

    let _ = dispatcher.publish(event(
        operation_id,
        1,
        EventKind::Started,
        EventPhase::Starting,
        EventPayload::Empty,
    )?);
    first_started_receiver.recv_timeout(Duration::from_secs(2))?;
    let _ = dispatcher.publish(event(
        operation_id,
        2,
        EventKind::Warning,
        EventPhase::Running,
        EventPayload::Message(gib::EventMessage::new("notice", "still running")?),
    )?);
    let _ = dispatcher.publish(event(
        operation_id,
        3,
        EventKind::Conflict,
        EventPhase::Running,
        EventPayload::Message(gib::EventMessage::new("conflict", "attention required")?),
    )?);

    let progress_delivery = dispatcher.publish(event(
        operation_id,
        4,
        EventKind::Progress,
        EventPhase::Running,
        EventPayload::Progress(Progress::indeterminate(4)),
    )?);
    assert_eq!(progress_delivery.dropped_progress_count(), 1);
    assert_eq!(progress_delivery.delivered_count(), 0);

    release_sender.send(())?;
    assert_eq!(
        receive_event(&received_receiver)?.kind(),
        EventKind::Started
    );
    assert_eq!(
        receive_event(&received_receiver)?.kind(),
        EventKind::Warning
    );
    assert_eq!(
        receive_event(&received_receiver)?.kind(),
        EventKind::Conflict
    );
    subscription.close();
    Ok(())
}
