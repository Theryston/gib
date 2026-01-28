use serde::Serialize;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Interactive,
    Json,
}

impl OutputMode {}

static OUTPUT_MODE: OnceLock<OutputMode> = OnceLock::new();

pub fn detect_mode_from_args(args: &[String]) -> OutputMode {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--mode" {
            if let Some(value) = iter.next() {
                return match value.to_ascii_lowercase().as_str() {
                    "json" => OutputMode::Json,
                    "interactive" => OutputMode::Interactive,
                    _ => OutputMode::Json,
                };
            }
            return OutputMode::Json;
        } else if let Some(value) = arg.strip_prefix("--mode=") {
            return match value.to_ascii_lowercase().as_str() {
                "json" => OutputMode::Json,
                "interactive" => OutputMode::Interactive,
                _ => OutputMode::Json,
            };
        }
    }

    OutputMode::Interactive
}

pub fn set_output_mode(mode: OutputMode) {
    let _ = OUTPUT_MODE.set(mode);
}

pub fn output_mode() -> OutputMode {
    *OUTPUT_MODE.get_or_init(|| OutputMode::Interactive)
}

pub fn is_json_mode() -> bool {
    output_mode() == OutputMode::Json
}

#[derive(Serialize)]
struct Event<'a, T: Serialize> {
    #[serde(rename = "type")]
    kind: &'a str,
    data: T,
}

#[derive(Serialize)]
struct ErrorData<'a> {
    message: &'a str,
    code: &'a str,
}

#[derive(Serialize)]
struct ErrorDataOwned {
    message: String,
    code: String,
}

#[derive(Serialize)]
struct WarningData<'a> {
    message: &'a str,
    code: &'a str,
}

#[derive(Serialize)]
struct PanicData {
    message: String,
    code: &'static str,
    location: Option<String>,
}

#[derive(Serialize)]
struct TextData {
    text: String,
}

#[derive(Serialize)]
struct ProgressData {
    percent: u64,
    total: u64,
    processed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn emit_event<T: Serialize>(kind: &'static str, data: &T, to_stderr: bool) {
    let event = Event { kind, data };
    let json = serde_json::to_string(&event).unwrap_or_else(|e| {
        let fallback = Event {
            kind: "error",
            data: ErrorDataOwned {
                message: e.to_string(),
                code: "serialization_error".to_string(),
            },
        };
        serde_json::to_string(&fallback)
            .unwrap_or_else(|_| "{\"type\":\"error\",\"data\":{\"message\":\"serialization_error\",\"code\":\"serialization_error\"}}".to_string())
    });

    if to_stderr {
        eprintln!("{json}");
    } else {
        println!("{json}");
    }
}

pub fn emit_output<T: Serialize>(data: &T) {
    emit_event("output", data, false);
}

pub fn emit_help(text: String) {
    let payload = TextData { text };
    emit_event("help", &payload, false);
}

pub fn emit_version(text: String) {
    let payload = TextData { text };
    emit_event("version", &payload, false);
}

pub fn emit_progress_update(processed: u64, total: u64, message: Option<String>) {
    let percent = if total == 0 {
        0
    } else {
        (processed.saturating_mul(100)) / total
    };

    let payload = ProgressData {
        percent,
        total,
        processed,
        message,
    };
    emit_event("progress", &payload, false);
}

pub fn emit_progress_message(message: &str) {
    emit_progress_update(0, 0, Some(message.to_string()));
}

pub fn emit_error(message: &str, code: &str) -> ! {
    let payload = ErrorData { message, code };
    emit_event("error", &payload, true);
    std::process::exit(1);
}

pub fn emit_warning(message: &str, code: &str) {
    if is_json_mode() {
        let payload = WarningData { message, code };
        emit_event("warning", &payload, true);
    } else {
        eprintln!("Warning: {}", message);
    }
}

pub fn init_panic_hook_if_json() {
    if !is_json_mode() {
        return;
    }

    std::panic::set_hook(Box::new(|info| {
        let message = if let Some(value) = info.payload().downcast_ref::<&str>() {
            value.to_string()
        } else if let Some(value) = info.payload().downcast_ref::<String>() {
            value.clone()
        } else {
            "panic".to_string()
        };

        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()));

        let payload = PanicData {
            message,
            code: "panic",
            location,
        };

        emit_event("error", &payload, true);
    }));
}

pub struct JsonProgress {
    total: u64,
    processed: AtomicU64,
    message: Mutex<Option<String>>,
}

impl JsonProgress {
    pub fn new(total: u64) -> Arc<Self> {
        Arc::new(Self {
            total,
            processed: AtomicU64::new(0),
            message: Mutex::new(None),
        })
    }

    pub fn set_message(&self, message: &str) {
        let mut guard = self.message.lock().unwrap();
        *guard = Some(message.to_string());
        let processed = self.processed.load(Ordering::SeqCst);
        emit_progress_update(processed, self.total, guard.clone());
    }

    pub fn inc_by(&self, delta: u64) {
        let processed = self.processed.fetch_add(delta, Ordering::SeqCst) + delta;
        let message = self.message.lock().unwrap().clone();
        emit_progress_update(processed, self.total, message);
    }
}
