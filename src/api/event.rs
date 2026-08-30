use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The kind of operation that generated an event.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Backup,
    Restore,
    Search,
    Explore,
    Encrypt,
    Delete,
    Prune,
    Setup,
    Storage,
    Identity,
    Live,
    Autostart,
    Catalog,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OperationStarted {
    pub operation: OperationKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    pub operation: OperationKind,
    pub phase: String,
    pub processed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WarningEvent {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackupEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RestoreEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_remote: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_text: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutostartEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

/// Typed events emitted by library operations.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GibEvent {
    OperationStarted(OperationStarted),
    Progress(ProgressEvent),
    Warning(WarningEvent),
    Backup(BackupEvent),
    Restore(RestoreEvent),
    Live(LiveEvent),
    Autostart(AutostartEvent),
}

impl GibEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::OperationStarted(_) => "operation_started",
            Self::Progress(_) => "progress",
            Self::Warning(_) => "warning",
            Self::Backup(_) => "backup",
            Self::Restore(_) => "restore",
            Self::Live(_) => "live",
            Self::Autostart(_) => "autostart",
        }
    }

    pub fn to_json_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            json!({
                "type": "error",
                "data": { "code": "serialization_error", "message": "Failed to serialize GIB event" }
            })
        })
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"type\":\"error\",\"data\":{\"code\":\"serialization_error\",\"message\":\"Failed to serialize GIB event\"}}".to_string()
        })
    }
}

impl Serialize for GibEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut event = serializer.serialize_struct("GibEvent", 2)?;
        event.serialize_field("type", self.event_type())?;
        match self {
            Self::OperationStarted(value) => event.serialize_field("data", value)?,
            Self::Progress(value) => event.serialize_field("data", value)?,
            Self::Warning(value) => event.serialize_field("data", value)?,
            Self::Backup(value) => event.serialize_field("data", value)?,
            Self::Restore(value) => event.serialize_field("data", value)?,
            Self::Live(value) => event.serialize_field("data", value)?,
            Self::Autostart(value) => event.serialize_field("data", value)?,
        }
        event.end()
    }
}

pub type EventCallback = Arc<dyn Fn(GibEvent) + Send + Sync + 'static>;

#[derive(Clone, Default)]
pub(crate) struct EventDispatcher {
    callback: Option<EventCallback>,
    state: Arc<Mutex<DispatchState>>,
}

#[derive(Default)]
struct DispatchState {
    queue: VecDeque<GibEvent>,
    dispatching: bool,
}

impl EventDispatcher {
    pub(crate) fn new(callback: Option<EventCallback>) -> Self {
        Self {
            callback,
            state: Arc::new(Mutex::new(DispatchState::default())),
        }
    }

    pub(crate) fn emit(&self, event: GibEvent) {
        let Some(callback) = &self.callback else {
            return;
        };
        let callback = Arc::clone(callback);
        let should_dispatch = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.queue.push_back(event);
            if state.dispatching {
                false
            } else {
                state.dispatching = true;
                true
            }
        };
        if !should_dispatch {
            return;
        }

        // The queue is protected only while adding/removing events. The user
        // callback is invoked outside the lock, so callbacks may synchronously
        // invoke another operation on the same client without deadlocking.
        loop {
            let next = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match state.queue.pop_front() {
                    Some(event) => Some(event),
                    None => {
                        state.dispatching = false;
                        None
                    }
                }
            };
            let Some(event) = next else {
                break;
            };
            callback(event);
        }
    }

    pub(crate) fn has_callback(&self) -> bool {
        self.callback.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_typed_events_as_cli_compatible_envelopes() {
        let event = GibEvent::Progress(ProgressEvent {
            operation: OperationKind::Backup,
            phase: "files".to_string(),
            processed: 2,
            total: Some(4),
            percentage: Some(50),
            message: Some("Processed file".to_string()),
        });

        assert_eq!(
            event.to_json_line(),
            r#"{"type":"progress","data":{"operation":"backup","phase":"files","processed":2,"total":4,"percentage":50,"message":"Processed file"}}"#
        );
    }

    #[test]
    fn nested_callback_events_are_delivered_in_order_without_deadlock() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let dispatcher_slot = Arc::new(Mutex::new(None::<EventDispatcher>));
        let callback_seen = Arc::clone(&seen);
        let callback_slot = Arc::clone(&dispatcher_slot);
        let callback = Arc::new(move |event: GibEvent| {
            let event_name = event.event_type();
            callback_seen
                .lock()
                .expect("callback state should not be poisoned")
                .push(event_name);
            if event_name == "operation_started" {
                if let Some(dispatcher) = callback_slot
                    .lock()
                    .expect("dispatcher slot should not be poisoned")
                    .clone()
                {
                    dispatcher.emit(GibEvent::Warning(WarningEvent {
                        code: "nested".to_string(),
                        message: "nested event".to_string(),
                    }));
                }
            }
        });
        let dispatcher = EventDispatcher::new(Some(callback));
        *dispatcher_slot
            .lock()
            .expect("dispatcher slot should not be poisoned") = Some(dispatcher.clone());

        dispatcher.emit(GibEvent::OperationStarted(OperationStarted {
            operation: OperationKind::Backup,
        }));

        assert_eq!(
            *seen.lock().expect("callback state should not be poisoned"),
            vec!["operation_started", "warning"]
        );
    }
}
