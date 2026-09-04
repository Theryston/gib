use gib::{
    BackupBudgets, BackupRequest, ChunkingConfiguration, Client, LocalStorage, PackConfiguration,
    PackIndexConfiguration, RepositoryIdentity, RepositoryInitRequest, RepositoryKey,
    RepositoryOpenRequest, SdkError,
};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1).peekable();
    let source = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("missing source directory")?;
    let repository_path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("missing local repository directory")?;
    let mut options = QaOptions::default();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--memory-mib" => options.memory_mib = parse_next(&mut arguments, "memory MiB")?,
            "--cpu" => options.cpu_workers = parse_next(&mut arguments, "CPU workers")?,
            "--fds" => options.file_descriptors = parse_next(&mut arguments, "file descriptors")?,
            "--network" => {
                options.network_requests = parse_next(&mut arguments, "network requests")?
            }
            "--queue" => options.queue_capacity = parse_next(&mut arguments, "queue capacity")?,
            "--slow-events-ms" => {
                options.slow_events_ms = parse_next(&mut arguments, "event delay")?
            }
            "--cancel-after-ms" => {
                options.cancel_after_ms = Some(parse_next(&mut arguments, "cancellation delay")?)
            }
            "--message" => options.message = arguments.next().ok_or("missing message")?,
            "--parent" => {
                options.parent = Some(
                    arguments
                        .next_if(|value| !value.starts_with("--"))
                        .unwrap_or_else(|| String::from("latest")),
                )
            }
            _ => return Err(format!("unknown argument {argument}").into()),
        }
    }
    if !source.is_dir() {
        return Err(format!("source is not a directory: {}", source.display()).into());
    }

    let client = Client::builder().event_buffer_capacity(1).build()?;
    let observed_events = Arc::new(AtomicUsize::new(0));
    let _subscription = if options.slow_events_ms > 0 {
        let observed_events = Arc::clone(&observed_events);
        let delay = Duration::from_millis(options.slow_events_ms);
        Some(client.register_event_consumer(move |_event| {
            observed_events.fetch_add(1, Ordering::AcqRel);
            thread::sleep(delay);
        })?)
    } else {
        None
    };

    let storage = LocalStorage::new(&repository_path)?;
    let repository = match client.open_repository(storage.clone(), RepositoryOpenRequest::new()) {
        Ok(repository) => repository,
        Err(SdkError::RepositoryMissing) => client.initialize_repository(
            storage,
            RepositoryInitRequest::new(
                RepositoryIdentity::new("backup-pipeline-manual-qa")?,
                RepositoryKey::new("manual")?,
            ),
        )?,
        Err(error) => return Err(error.into()),
    };
    let budgets = BackupBudgets::with_queue_capacity(
        options.memory_mib.saturating_mul(1024 * 1024),
        options.cpu_workers,
        options.file_descriptors,
        options.network_requests,
        options.queue_capacity,
    )?;
    let mut request = BackupRequest::new(&source)
        .with_message(options.message)
        .with_budgets(budgets)
        .with_chunking(ChunkingConfiguration::new(
            64 * 1024,
            256 * 1024,
            1024 * 1024,
        )?)
        .with_pack_configuration(PackConfiguration::new(8 * 1024 * 1024, 16 * 1024 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(1024 * 1024)?);
    if let Some(parent) = options.parent {
        request = request.with_parent_reference(parent)?;
    }
    let handle = client.start_backup(repository, request)?;
    let cancellation = options.cancel_after_ms.map(|delay| {
        let cancellation = handle.cancellation_handle();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(delay));
            let _ = cancellation.cancel();
        })
    });
    let result = handle.join();
    if let Some(cancellation) = cancellation {
        let _ = cancellation.join();
    }
    match result {
        Ok(result) => {
            let metrics = result.metrics();
            println!(
                "backup completed snapshot={} files={} bytes={} logical_bytes={} new_stored_bytes={} reused_bytes={} chunks={} packs={} peak_memory={} peak_cpu={} peak_fds={} peak_network={} events={}",
                result.snapshot(),
                metrics.files(),
                metrics.total_size(),
                metrics.logical_bytes(),
                metrics.new_stored_bytes(),
                metrics.reused_bytes(),
                metrics.chunks(),
                metrics.packs(),
                metrics.peak_memory_bytes(),
                metrics.peak_cpu_workers(),
                metrics.peak_open_file_descriptors(),
                metrics.peak_network_requests(),
                observed_events.load(Ordering::Acquire),
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

struct QaOptions {
    memory_mib: usize,
    cpu_workers: usize,
    file_descriptors: usize,
    network_requests: usize,
    queue_capacity: usize,
    slow_events_ms: u64,
    cancel_after_ms: Option<u64>,
    message: String,
    parent: Option<String>,
}

impl QaOptions {
    fn default() -> Self {
        Self {
            memory_mib: 64,
            cpu_workers: 4,
            file_descriptors: 16,
            network_requests: 2,
            queue_capacity: 2,
            slow_events_ms: 0,
            cancel_after_ms: None,
            message: String::from("manual bounded backup"),
            parent: None,
        }
    }
}

fn parse_next<T>(
    arguments: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<T, Box<dyn Error>>
where
    T: std::str::FromStr,
    T::Err: Error + 'static,
{
    arguments
        .next()
        .ok_or_else(|| format!("missing {name}"))?
        .parse::<T>()
        .map_err(|error| format!("invalid {name}: {error}").into())
}
