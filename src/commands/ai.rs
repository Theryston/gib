use crate::ai::conversation::{ConversationError, ConversationService};
use crate::ai::hardware::{HardwareSnapshot, NativeRuntimeCapabilities};
use crate::ai::model::{
    AiConfigStore, ModelError, ModelInstallCancellation, ModelManager, output_progress_sink,
};
use crate::ai::profiles::{
    RuntimeConfig, RuntimeOverrides, RuntimeProfile, RuntimeProfileError, resolve_runtime_config,
};
use crate::ai::runtime::{AiBackend, AiBackendError, AiBackendFactory, AiRuntimeOptions};
use crate::ai::{
    AiCancellation, AiPromptPolicy, AiTurnError, AiTurnEvent, AiTurnEventSink, AiTurnRequest,
    AiTurnService,
};
use crate::output::{emit_error, emit_named_event, is_json_mode};
use crate::utils::handle_error;
use clap::ArgMatches;
use indicatif::{ProgressBar, ProgressStyle};
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AiCommandRequest {
    message: Option<String>,
    conversation_id: Option<String>,
    profile: Option<RuntimeProfile>,
    runtime_overrides: RuntimeOverrides,
}

#[derive(Debug)]
enum AiCommandError {
    Input(String),
    Model(ModelError),
    Backend(AiBackendError),
    Runtime(RuntimeProfileError),
    Conversation(ConversationError),
    Turn(AiTurnError),
}

impl AiCommandError {
    fn code(&self) -> &'static str {
        match self {
            Self::Input(_) => "invalid_request",
            Self::Model(error) => model_error_code(error),
            Self::Backend(error) => error.code(),
            Self::Runtime(error) => error.code(),
            Self::Conversation(error) => error.code(),
            Self::Turn(error) => error.code(),
        }
    }

    fn json_message(&self) -> String {
        match self {
            Self::Input(message) => message.clone(),
            Self::Model(ModelError::DownloadCancelled) => {
                "AI model download was cancelled; the partial download was preserved".to_string()
            }
            Self::Model(_) => "AI model installation or verification failed".to_string(),
            Self::Backend(error) => error.to_string(),
            Self::Runtime(error) => error.to_string(),
            Self::Conversation(error) => error.to_string(),
            Self::Turn(error) => error.to_string(),
        }
    }
}

impl fmt::Display for AiCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => formatter.write_str(message),
            Self::Model(error) => write!(formatter, "{error}"),
            Self::Backend(error) => write!(formatter, "{error}"),
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Conversation(error) => write!(formatter, "{error}"),
            Self::Turn(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<ModelError> for AiCommandError {
    fn from(error: ModelError) -> Self {
        Self::Model(error)
    }
}

impl From<AiBackendError> for AiCommandError {
    fn from(error: AiBackendError) -> Self {
        Self::Backend(error)
    }
}

impl From<RuntimeProfileError> for AiCommandError {
    fn from(error: RuntimeProfileError) -> Self {
        Self::Runtime(error)
    }
}

impl From<ConversationError> for AiCommandError {
    fn from(error: ConversationError) -> Self {
        Self::Conversation(error)
    }
}

impl From<AiTurnError> for AiCommandError {
    fn from(error: AiTurnError) -> Self {
        Self::Turn(error)
    }
}

/// Run the initial direct-chat command. Repository-local configuration is
/// intentionally not loaded: AI state is global under ~/.gib/ai.
pub async fn ai(matches: &ArgMatches) {
    match matches.subcommand() {
        Some(("conversation", conversation_matches)) => {
            if matches.get_one::<String>("message").is_some()
                || matches.get_one::<String>("conversation").is_some()
                || runtime_options_present(matches)
            {
                report_failure(AiCommandError::Input(
                    "chat and runtime options cannot be used with 'gib ai conversation'"
                        .to_string(),
                ));
            }
            super::ai_conversation::run(conversation_matches).await;
            return;
        }
        Some((operation, _)) => report_failure(AiCommandError::Input(format!(
            "Unknown AI operation '{operation}'. Run 'gib ai --help' for more information."
        ))),
        None => {}
    }

    let request = match parse_request(matches) {
        Ok(request) => request,
        Err(error) => report_failure(AiCommandError::Input(error)),
    };

    if request.message.is_none() && !interactive_terminal_available() {
        report_failure(AiCommandError::Input(
            "Interactive gib ai requires a terminal; provide --mode json --message <MESSAGE> for non-interactive use".to_string(),
        ));
    }

    if let Err(error) = validate_conversation_override(request.conversation_id.clone()).await {
        report_failure(error);
    }

    let install_cancellation = ModelInstallCancellation::new();
    let signal_cancellation = install_cancellation.clone();
    let install_signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancellation.cancel();
        }
    });
    let build_result = build_turn_service(install_cancellation, request.clone()).await;
    install_signal_task.abort();

    let (service, backend, model_id, runtime_config) = match build_result {
        Ok(value) => value,
        Err(error) => report_failure(error),
    };

    let result = if let Some(message) = request.message {
        run_single_turn(&service, request.conversation_id, message, is_json_mode()).await
    } else {
        run_interactive_loop(
            &service,
            request.conversation_id,
            model_id.clone(),
            runtime_config,
        )
        .await
    };

    let _ = backend.unload_model(Some(&model_id)).await;
    if let Err(error) = result {
        report_failure(error);
    }
}

/// Validate an invocation-scoped conversation before model installation. An
/// unknown explicit ID must fail without downloading/loading a model or
/// writing a user message.
async fn validate_conversation_override(
    conversation_id: Option<String>,
) -> Result<(), AiCommandError> {
    let Some(conversation_id) = conversation_id else {
        return Ok(());
    };
    let conversations = ConversationService::default_store()?;
    conversations.load(conversation_id).await?;
    Ok(())
}

fn parse_request(matches: &ArgMatches) -> Result<AiCommandRequest, String> {
    let message = matches.get_one::<String>("message").cloned();
    let conversation_id = matches.get_one::<String>("conversation").cloned();
    let profile = matches
        .get_one::<String>("profile")
        .map(|value| value.parse::<RuntimeProfile>())
        .transpose()?;
    let runtime_overrides = parse_runtime_overrides(matches)?;
    if matches
        .get_one::<String>("mode")
        .is_some_and(|mode| mode == "json")
        && message.is_none()
    {
        return Err("Missing required argument: --message (required in --mode json)".to_string());
    }
    Ok(AiCommandRequest {
        message,
        conversation_id,
        profile,
        runtime_overrides,
    })
}

async fn build_turn_service(
    cancellation: ModelInstallCancellation,
    request: AiCommandRequest,
) -> Result<(AiTurnService, Arc<dyn AiBackend>, String, RuntimeConfig), AiCommandError> {
    let conversations = ConversationService::default_store()?;
    let model_manager = ModelManager::default()?;
    let installed = model_manager
        .ensure_active_model_with_cancellation(
            Some(output_progress_sink()),
            Some(cancellation.clone()),
        )
        .await?;
    if cancellation.is_cancelled() {
        return Err(ModelError::DownloadCancelled.into());
    }
    let model_id = installed.manifest.id.clone();
    let runtime_progress = start_runtime_profile_progress(&model_id);
    let factory = AiBackendFactory::new(model_manager.clone());
    let native_capabilities = factory.capabilities()?;
    let hardware = HardwareSnapshot::detect(NativeRuntimeCapabilities {
        cpu: native_capabilities.cpu,
        gpu_offload: native_capabilities.gpu_offload,
        mmap: native_capabilities.mmap,
        mlock: native_capabilities.mlock,
        gpu_memory_total_bytes: native_capabilities.gpu_memory_total_bytes,
        gpu_memory_free_bytes: native_capabilities.gpu_memory_free_bytes,
        accelerator_backends: native_capabilities.accelerator_backends.clone(),
    });
    let preferences = AiConfigStore::new(model_manager.paths().clone()).runtime_preferences()?;
    let runtime_config = resolve_runtime_config(
        &model_id,
        &installed.manifest,
        &preferences,
        request.profile,
        &request.runtime_overrides,
        hardware,
    )?;
    let runtime_message = runtime_config
        .downgrade_reason
        .as_deref()
        .map(|reason| {
            format!(
                "AI runtime resolved ({}): {reason}",
                runtime_config.summary()
            )
        })
        .unwrap_or_else(|| format!("AI runtime resolved ({})", runtime_config.summary()));
    runtime_progress.finish_with_message(runtime_message);
    if is_json_mode() {
        emit_named_event(
            "ai_runtime",
            &serde_json::json!({
                "status": "resolved",
                "model_id": model_id.as_str(),
                "runtime": runtime_config.clone(),
            }),
        );
    }
    let backend = factory
        .with_options(AiRuntimeOptions::from_runtime_config(&runtime_config))
        .build()?;
    let load_progress = start_model_load_progress(&model_id);
    let load_result = backend.load_model(&model_id).await;
    match &load_result {
        Ok(_) => {
            load_progress.finish_with_message("The AI model is loaded and ready");
            if is_json_mode() {
                emit_named_event(
                    "ai_model_load",
                    &serde_json::json!({
                        "model_id": model_id.as_str(),
                        "phase": "loading",
                        "status": "complete",
                        "message": "The AI model is loaded and ready"
                    }),
                );
            }
        }
        Err(error) => {
            let message = format!("Failed to load the AI model: {error}");
            load_progress.abandon_with_message(message.clone());
            if is_json_mode() {
                emit_named_event(
                    "ai_model_load",
                    &serde_json::json!({
                        "model_id": model_id.as_str(),
                        "phase": "loading",
                        "status": "failed",
                        "message": message
                    }),
                );
            }
        }
    }
    load_result?;
    if cancellation.is_cancelled() {
        let _ = backend.unload_model(Some(&model_id)).await;
        return Err(ModelError::DownloadCancelled.into());
    }
    let prompt_policy = AiPromptPolicy {
        context_limit: runtime_config.context_size,
        max_output_tokens: runtime_config.max_output_tokens,
        ..AiPromptPolicy::default()
    };
    let service = AiTurnService::new(
        conversations,
        backend.clone(),
        model_id.clone(),
        prompt_policy,
    );
    Ok((service, backend, model_id, runtime_config))
}

fn start_runtime_profile_progress(model_id: &str) -> ProgressBar {
    let message =
        format!("Detecting hardware and selecting an AI runtime profile for '{model_id}'");
    if is_json_mode() {
        emit_named_event(
            "ai_runtime",
            &serde_json::json!({
                "status": "detecting",
                "model_id": model_id,
                "message": message,
            }),
        );
        return ProgressBar::hidden();
    }

    let progress = ProgressBar::new_spinner();
    progress.enable_steady_tick(Duration::from_millis(100));
    progress.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    progress.set_message(message);
    progress
}

fn start_model_load_progress(model_id: &str) -> ProgressBar {
    let message = format!("Loading AI model '{model_id}' into memory (this may take a while)");
    if is_json_mode() {
        emit_named_event(
            "ai_model_load",
            &serde_json::json!({
                "model_id": model_id,
                "phase": "loading",
                "status": "started",
                "message": message
            }),
        );
        return ProgressBar::hidden();
    }

    let progress = ProgressBar::new_spinner();
    progress.enable_steady_tick(Duration::from_millis(100));
    progress.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    progress.set_message(message);
    progress
}

async fn run_single_turn(
    service: &AiTurnService,
    conversation_id: Option<String>,
    message: String,
    json_mode: bool,
) -> Result<(), AiCommandError> {
    let cancellation = AiCancellation::new();
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancellation.cancel();
        }
    });
    let sink = if json_mode {
        Some(json_event_sink())
    } else {
        Some(interactive_event_sink())
    };
    let result = run_turn_with_sink(service, conversation_id, message, cancellation, sink).await;
    signal_task.abort();
    result.map(|_| ())
}

async fn run_turn_with_sink(
    service: &AiTurnService,
    conversation_id: Option<String>,
    message: String,
    cancellation: AiCancellation,
    sink: Option<AiTurnEventSink>,
) -> Result<crate::ai::AiTurnResponse, AiCommandError> {
    let request = AiTurnRequest::new(conversation_id, message)?;
    service
        .run_turn(request, cancellation, sink)
        .await
        .map_err(Into::into)
}

pub(crate) async fn run_turn_for_interactive(
    service: &AiTurnService,
    conversation_id: Option<String>,
    message: String,
    cancellation: AiCancellation,
    sink: Option<AiTurnEventSink>,
) -> Result<crate::ai::AiTurnResponse, AiTurnError> {
    let request = AiTurnRequest::new(conversation_id, message)?;
    service.run_turn(request, cancellation, sink).await
}

async fn run_interactive_loop(
    service: &AiTurnService,
    conversation_id: Option<String>,
    model_id: String,
    runtime_config: RuntimeConfig,
) -> Result<(), AiCommandError> {
    super::ai_interactive::run(service, conversation_id, model_id, runtime_config)
        .await
        .map_err(AiCommandError::Input)
}

fn parse_runtime_overrides(matches: &ArgMatches) -> Result<RuntimeOverrides, String> {
    let gpu_offload = matches
        .get_one::<String>("gpu-offload")
        .and_then(|mode| match mode.as_str() {
            "auto" => None,
            "on" => Some(Ok(true)),
            "off" => Some(Ok(false)),
            _ => Some(Err("--gpu-offload must be auto, on, or off".to_string())),
        })
        .transpose()?;
    Ok(RuntimeOverrides {
        threads: matches.get_one::<u32>("threads").copied(),
        context_size: matches.get_one::<u32>("context-size").copied(),
        batch_size: matches.get_one::<u32>("batch-size").copied(),
        gpu_layers: matches.get_one::<u32>("gpu-layers").copied(),
        gpu_offload,
        max_output_tokens: matches.get_one::<u32>("max-output-tokens").copied(),
        agent_budget: matches.get_one::<u32>("agent-budget").copied(),
        search_budget: matches.get_one::<u32>("search-budget").copied(),
        memory_budget_percent: matches.get_one::<u8>("memory-budget-percent").copied(),
    })
}

fn runtime_options_present(matches: &ArgMatches) -> bool {
    [
        "profile",
        "threads",
        "context-size",
        "batch-size",
        "gpu-layers",
        "gpu-offload",
        "max-output-tokens",
        "agent-budget",
        "search-budget",
        "memory-budget-percent",
    ]
    .iter()
    .any(|name| matches.contains_id(name) && matches.get_raw(name).is_some())
}

fn json_event_sink() -> AiTurnEventSink {
    Arc::new(|event| match event {
        AiTurnEvent::Finished { response } => emit_named_event("ai_response", response),
        _ => emit_named_event("ai_turn", event),
    })
}

fn interactive_event_sink() -> AiTurnEventSink {
    Arc::new(|event| match event {
        AiTurnEvent::Started { .. } => {
            print!("assistant> ");
            let _ = io::stdout().flush();
        }
        AiTurnEvent::Delta { text, .. } => {
            print!("{text}");
            let _ = io::stdout().flush();
        }
        AiTurnEvent::Finished { .. } => println!(),
        AiTurnEvent::Cancelled { .. } => println!("\n[AI response cancelled]"),
        AiTurnEvent::Failed { error, .. } => println!("\n[AI response unavailable: {error}]"),
        AiTurnEvent::Progress { .. } => {}
    })
}

fn interactive_terminal_available() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()
}

fn report_failure(error: AiCommandError) -> ! {
    let code = error.code();
    if is_json_mode() {
        let message = error.json_message();
        emit_error(&message, code);
    }
    handle_error(error.to_string(), None)
}

fn model_error_code(error: &ModelError) -> &'static str {
    match error {
        ModelError::MissingHomeDirectory => "missing_home_directory",
        ModelError::InvalidModelId(_) => "invalid_model_id",
        ModelError::InvalidManifest(_) | ModelError::ManifestIntegrityMissing { .. } => {
            "invalid_model_manifest"
        }
        ModelError::UnknownModel(_) => "unknown_model",
        ModelError::InvalidUrl(_) => "invalid_model_url",
        ModelError::Io { .. } => "model_io_error",
        ModelError::Serialization { .. } => "model_metadata_error",
        ModelError::LockTimeout(_) => "model_lock_timeout",
        ModelError::LockLost(_) => "model_lock_lost",
        ModelError::Http { .. } => "model_download_error",
        ModelError::UnexpectedStatus { .. }
        | ModelError::InvalidContentRange { .. }
        | ModelError::RangeNotSatisfiable { .. }
        | ModelError::DownloadInterrupted(_) => "model_download_error",
        ModelError::DownloadCancelled => "model_download_cancelled",
        ModelError::SizeMismatch { .. } => "model_size_mismatch",
        ModelError::NotInstalled(_) => "model_not_installed",
        ModelError::MetadataMismatch(_) => "model_metadata_error",
        ModelError::UnsafePath(_) => "unsafe_model_path",
        ModelError::ActiveModel(_) => "invalid_active_model",
        ModelError::InvalidRuntime(_) => "invalid_runtime_config",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_matches(arguments: &[&str]) -> clap::ArgMatches {
        let mut values = vec!["gib"];
        values.extend(arguments.iter().copied());
        crate::cli()
            .try_get_matches_from(values)
            .expect("arguments should parse")
    }

    #[test]
    fn parses_interactive_and_json_message_forms() {
        let interactive = command_matches(&["ai"]);
        let interactive = interactive.subcommand().expect("AI command should exist").1;
        assert_eq!(parse_request(interactive).unwrap().message, None);

        let json_before = command_matches(&["--mode", "json", "ai", "--message", "hello"]);
        let json_before = json_before.subcommand().expect("AI command should exist").1;
        assert_eq!(
            parse_request(json_before).unwrap().message.as_deref(),
            Some("hello")
        );

        let json_after = command_matches(&["ai", "--mode", "json", "--message", "hello"]);
        let json_after = json_after.subcommand().expect("AI command should exist").1;
        assert_eq!(
            parse_request(json_after).unwrap().message.as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn parses_conversation_override_without_repository_arguments() {
        let matches =
            command_matches(&["ai", "--conversation", "conv-example", "--message", "hello"]);
        let matches = matches.subcommand().expect("AI command should exist").1;
        let request = parse_request(matches).expect("request should parse");
        assert_eq!(request.conversation_id.as_deref(), Some("conv-example"));
        assert_eq!(request.message.as_deref(), Some("hello"));
    }

    #[test]
    fn json_mode_without_message_is_rejected_before_execution() {
        let matches = crate::cli()
            .try_get_matches_from(["gib", "--mode", "json", "ai"])
            .expect("clap should pass the global mode to the command");
        let matches = matches.subcommand().expect("AI command should exist").1;
        assert!(parse_request(matches).is_err());
    }

    #[test]
    fn parses_runtime_profile_and_invocation_overrides() {
        let matches = command_matches(&[
            "ai",
            "--profile",
            "high-quality",
            "--threads",
            "4",
            "--context-size",
            "8192",
            "--batch-size",
            "512",
            "--gpu-layers",
            "12",
            "--gpu-offload",
            "on",
            "--max-output-tokens",
            "512",
            "--agent-budget",
            "100",
            "--search-budget",
            "200",
            "--memory-budget-percent",
            "75",
            "--message",
            "hello",
        ]);
        let matches = matches.subcommand().expect("AI command should exist").1;
        let request = parse_request(matches).expect("runtime options should parse");
        assert_eq!(request.profile, Some(RuntimeProfile::HighQuality));
        assert_eq!(request.runtime_overrides.threads, Some(4));
        assert_eq!(request.runtime_overrides.context_size, Some(8192));
        assert_eq!(request.runtime_overrides.batch_size, Some(512));
        assert_eq!(request.runtime_overrides.gpu_layers, Some(12));
        assert_eq!(request.runtime_overrides.gpu_offload, Some(true));
        assert_eq!(request.runtime_overrides.max_output_tokens, Some(512));
        assert_eq!(request.runtime_overrides.agent_budget, Some(100));
        assert_eq!(request.runtime_overrides.search_budget, Some(200));
        assert_eq!(request.runtime_overrides.memory_budget_percent, Some(75));
    }
}
