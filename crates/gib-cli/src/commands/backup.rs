use super::{CommandError, open_repository};
use crate::input::{BackupAction, BackupCommand, BackupPendingCommand, OutputMode};
use crate::output;
use gib::{
    BackupBudgets, BackupPipeline, BackupRequest, Client, CompressionLevel, ObjectCodec,
    ObjectTransformOptions, PendingOperationListRequest, SdkError, local_filesystem_scanner,
};
use std::path::{Path, PathBuf};

pub fn run(
    command: BackupCommand,
    configuration: &gib::ResolvedConfiguration,
    mode: OutputMode,
) -> Result<(), CommandError> {
    let BackupCommand {
        repository,
        repository_option,
        continue_id,
        action,
    } = command;
    let repository_path = repository_option
        .as_deref()
        .or(repository.as_deref())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    if continue_id.is_some() && action.is_some() {
        return Err(CommandError::Sdk(SdkError::InvalidRequest {
            field: "backup",
            reason: "--continue cannot be combined with a backup action",
        }));
    }
    match action {
        Some(BackupAction::Pending(request)) => run_pending(&repository_path, request, mode),
        None => run_backup(
            BackupCommand {
                repository: Some(repository_path),
                repository_option: None,
                continue_id,
                action: None,
            },
            configuration,
            mode,
        ),
    }
}

fn run_backup(
    command: BackupCommand,
    configuration: &gib::ResolvedConfiguration,
    mode: OutputMode,
) -> Result<(), CommandError> {
    let repository_path = command.repository_path().to_path_buf();
    let repository = open_repository(&repository_path)?;
    let request = build_request(configuration)?;
    let client = Client::default();
    let ignore_policy = configuration.ignore_policy().clone();
    let scanner = local_filesystem_scanner().with_ignore_policy(ignore_policy);
    let pipeline = BackupPipeline::new(repository, scanner, client.events());
    output::render_backup_start(
        &repository_path,
        request.root(),
        command.continue_id.as_deref(),
        mode,
    );
    let handle = match command.continue_id {
        Some(identifier) => pipeline
            .resume(identifier, request)
            .map_err(CommandError::Sdk)?,
        None => pipeline.start(request).map_err(CommandError::Sdk)?,
    };
    let pending_identifier = handle.pending_identifier().to_owned();
    let result = handle.join().map_err(CommandError::Sdk)?;
    output::render_backup_result(&result, &pending_identifier, mode);
    Ok(())
}

fn run_pending(
    repository_path: &Path,
    command: BackupPendingCommand,
    mode: OutputMode,
) -> Result<(), CommandError> {
    let repository = open_repository(repository_path)?;
    let mut cursor = command.after;
    let mut count = 0_usize;
    loop {
        let mut request = PendingOperationListRequest::new().with_limit(command.page_size);
        if let Some(cursor_value) = cursor.take() {
            request = request.with_cursor(cursor_value);
        }
        let page = repository
            .list_pending_operations(request)
            .map_err(CommandError::Sdk)?;
        count = count.saturating_add(page.operations().len());
        output::render_pending_operations(repository_path, &page, mode);
        cursor = page.next_cursor().cloned();
        if cursor.is_none() {
            break;
        }
    }
    output::render_pending_complete(count, mode);
    Ok(())
}

fn build_request(
    configuration: &gib::ResolvedConfiguration,
) -> Result<BackupRequest, CommandError> {
    let backup = configuration.configuration().backup();
    let root = backup.root_path().unwrap_or(Path::new("."));
    let message = backup.message().unwrap_or("backup");
    let concurrency = backup
        .concurrency()
        .unwrap_or(gib::DEFAULT_BACKUP_CPU_WORKERS);
    let budgets = BackupBudgets::with_queue_capacity(
        gib::DEFAULT_BACKUP_MEMORY_BYTES,
        concurrency,
        gib::DEFAULT_BACKUP_FILE_DESCRIPTORS,
        gib::DEFAULT_BACKUP_NETWORK_REQUESTS,
        gib::DEFAULT_BACKUP_QUEUE_CAPACITY,
    )
    .map_err(|_| invalid_backup_request("backup.concurrency"))?;
    let transforms = match backup.compress() {
        Some(level) => {
            let level = CompressionLevel::new(level)
                .map_err(|_| invalid_backup_request("backup.compress"))?;
            ObjectTransformOptions::new(ObjectCodec::Zstd, gib::ObjectEncryption::None)
                .with_compression_level(level)
        }
        None => ObjectTransformOptions::new(ObjectCodec::None, gib::ObjectEncryption::None),
    };
    Ok(BackupRequest::new(PathBuf::from(root))
        .with_message(message)
        .with_budgets(budgets)
        .with_chunking(backup.chunking())
        .with_transform_options(transforms))
}

fn invalid_backup_request(field: &'static str) -> CommandError {
    CommandError::Sdk(SdkError::InvalidRequest {
        field,
        reason: "value is outside the supported backup range",
    })
}
