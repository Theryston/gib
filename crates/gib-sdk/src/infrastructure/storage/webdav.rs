use crate::api::CancellationToken;
use crate::application::ports::{
    ObjectCursor, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectRange,
    ObjectRead, ObjectWriteOptions, RepositoryStorage, STORAGE_TRANSFER_BUFFER_SIZE,
    StorageCapabilities, StorageError, StorageResult, StorageVersion, StorageWriteCondition,
};
use futures_util::StreamExt;
use futures_util::stream::try_unfold;
use percent_encoding::percent_decode_str;
use quick_xml::events::{BytesCData, BytesText, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use reqwest::header::{self, HeaderValue};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{self, Cursor, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::runtime::{Builder as RuntimeBuilder, Handle};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self as tokio_mpsc, error::TrySendError};
use tokio::task::AbortHandle;
use url::Url;

/// The default number of concurrent WebDAV requests.
pub const DEFAULT_WEBDAV_MAX_CONCURRENCY: usize = 8;

/// The largest accepted WebDAV request concurrency.
pub const MAX_WEBDAV_MAX_CONCURRENCY: usize = 64;

/// The bounded transfer buffer used for WebDAV request and response streams.
pub const DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE: usize = STORAGE_TRANSFER_BUFFER_SIZE;

/// The default total timeout for one WebDAV request.
pub const DEFAULT_WEBDAV_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const WEBDAV_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WEBDAV_STREAM_CHANNEL_CAPACITY: usize = 2;
const WEBDAV_RUNTIME_CHANNEL_CAPACITY: usize = DEFAULT_WEBDAV_MAX_CONCURRENCY;
const MAX_WEBDAV_URL_LENGTH: usize = 8 * 1024;
const MAX_WEBDAV_CREDENTIAL_LENGTH: usize = 4 * 1024;
const MAX_PROPFIND_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TEMP_FILE_ATTEMPTS: usize = 32;
const DAV_NAMESPACE: &[u8] = b"DAV:";
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getetag/><d:getcontentlength/></d:prop></d:propfind>"#;

static NEXT_TEMP_UPLOAD_ID: AtomicU64 = AtomicU64::new(1);

/// Validated configuration for a WebDAV collection.
///
/// The collection URL is normalized to a trailing slash and cannot contain
/// embedded credentials, a query, or a fragment. Basic-auth credentials are
/// retained only by the adapter and are redacted from [`Debug`]. HTTP is
/// rejected by [`WebDavStorage::new`] unless
/// [`Self::with_allow_insecure_http`] is explicitly selected.
#[derive(Clone, Eq, PartialEq)]
pub struct WebDavStorageConfig {
    collection_url: String,
    username: String,
    password: String,
    allow_insecure_http: bool,
    max_concurrency: usize,
}

impl WebDavStorageConfig {
    /// Creates a WebDAV configuration with Basic authentication.
    pub fn new(
        collection_url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> StorageResult<Self> {
        let collection_url = normalize_collection_url(&collection_url.into())?;
        let config = Self {
            collection_url: collection_url.as_str().to_owned(),
            username: username.into(),
            password: password.into(),
            allow_insecure_http: false,
            max_concurrency: DEFAULT_WEBDAV_MAX_CONCURRENCY,
        };
        config.validate_syntax()?;
        Ok(config)
    }

    /// Allows an explicitly configured HTTP endpoint.
    ///
    /// HTTPS remains the default and is required unless this method is used.
    pub const fn with_allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// Sets the maximum number of concurrent network requests.
    pub const fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Returns the normalized collection URL without credentials.
    pub fn collection_url(&self) -> &str {
        &self.collection_url
    }

    /// Returns whether insecure HTTP is explicitly allowed.
    pub const fn allow_insecure_http(&self) -> bool {
        self.allow_insecure_http
    }

    /// Returns the configured request concurrency.
    pub const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    fn validate_syntax(&self) -> StorageResult<()> {
        validate_credential(&self.username, true)?;
        validate_credential(&self.password, true)?;
        if !(1..=MAX_WEBDAV_MAX_CONCURRENCY).contains(&self.max_concurrency) {
            return Err(StorageError::InvalidRequest);
        }
        Ok(())
    }
}

impl fmt::Debug for WebDavStorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebDavStorageConfig")
            .field("collection_url", &self.collection_url)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("max_concurrency", &self.max_concurrency)
            .finish()
    }
}

/// A streamed WebDAV object-storage adapter.
///
/// The adapter maps validated logical object keys to one configured WebDAV
/// collection. Network operations run on a dedicated Tokio runtime, GET
/// responses are exposed through bounded readers, and PUT sources are staged
/// in a private temporary file before the remote mutation begins. The staging
/// file bounds memory usage and ensures source or local-disk failures do not
/// start a remote PUT.
#[derive(Clone)]
pub struct WebDavStorage {
    config: Arc<WebDavStorageConfig>,
    root_url: Url,
    root_segments: Vec<String>,
    runtime: Arc<WebDavRuntime>,
}

impl WebDavStorage {
    /// Constructs a WebDAV adapter and validates that the configured URL is an
    /// accessible collection.
    pub fn new(config: WebDavStorageConfig) -> StorageResult<Self> {
        let storage = Self::build(config)?;
        storage.validate_root()?;
        Ok(storage)
    }

    fn build(config: WebDavStorageConfig) -> StorageResult<Self> {
        config.validate_syntax()?;
        let root_url = normalize_collection_url(&config.collection_url)?;
        if root_url.scheme() == "http" && !config.allow_insecure_http {
            return Err(StorageError::InvalidRequest);
        }
        let root_segments = decode_url_segments(root_url.path())?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(WEBDAV_CONNECT_TIMEOUT)
            .timeout(DEFAULT_WEBDAV_REQUEST_TIMEOUT)
            .user_agent(format!("gib/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(map_request_error)?;
        let runtime = Arc::new(WebDavRuntime::new(client, config.max_concurrency)?);
        Ok(Self {
            config: Arc::new(config),
            root_url,
            root_segments,
            runtime,
        })
    }

    /// Returns the redacted adapter configuration.
    pub fn config(&self) -> &WebDavStorageConfig {
        &self.config
    }

    /// Returns the normalized collection URL.
    pub fn collection_url(&self) -> &str {
        self.root_url.as_str()
    }

    /// Performs a read-only validation that the configured URL is a WebDAV
    /// collection and that the supplied credentials can access it.
    pub fn validate_root(&self) -> StorageResult<()> {
        let client_config = self.client_config();
        let root_url = self.root_url.clone();
        let root_segments = self.root_segments.clone();
        self.runtime.run(move |client| async move {
            let resources = propfind_async(
                &client,
                &client_config,
                root_url.clone(),
                0,
                root_url,
                root_segments,
            )
            .await?;
            match resources
                .into_iter()
                .find(|resource| resource.key.is_none())
            {
                Some(resource) if resource.is_collection => Ok(()),
                Some(_) => Err(StorageError::InvalidRequest),
                None => Err(StorageError::InvalidRequest),
            }
        })
    }

    /// Writes an object while checking cooperative cancellation while the
    /// caller's source is staged. Once the remote PUT begins, it is allowed to
    /// finish because a remote WebDAV server cannot safely roll back an
    /// ambiguous request from the client side.
    pub fn write_stream_with_cancellation(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<ObjectMetadata> {
        check_cancelled(cancellation)?;
        let condition = WriteCondition::from_storage_condition(options.condition())?;
        let staged = stage_source(source, options.expected_size(), cancellation)?;
        check_cancelled(cancellation)?;

        let url = self.resource_url(object_key, false)?;
        let key = object_key.clone();
        let root_url = self.root_url.clone();
        let root_segments = self.root_segments.clone();
        let client_config = self.client_config();
        let specification = PutStreamSpecification {
            client_config,
            root_url,
            root_segments,
            object_url: url,
            object_key: key,
            path: staged.path.clone(),
            size: staged.size,
            condition,
        };
        let result = self
            .runtime
            .run(move |client| async move { put_staged_async(&client, specification).await });
        drop(staged);
        result
    }

    fn client_config(&self) -> WebDavClientConfig {
        WebDavClientConfig {
            username: self.config.username.clone(),
            password: self.config.password.clone(),
        }
    }

    fn resource_url(&self, object_key: &ObjectKey, collection: bool) -> StorageResult<Url> {
        let components = object_key.as_str().split('/').collect::<Vec<_>>();
        url_for_segments(&self.root_url, &components, collection)
    }

    fn root_resource_url(&self) -> Url {
        self.root_url.clone()
    }

    fn open_get_stream(
        &self,
        object_key: &ObjectKey,
        range: Option<ObjectRange>,
        metadata: ObjectMetadata,
    ) -> StorageResult<ObjectRead> {
        let url = self.resource_url(object_key, false)?;
        let specification = GetStreamSpecification {
            url,
            object_key: object_key.clone(),
            expected_size: metadata.size(),
            expected_version: metadata.version().cloned(),
            range,
            client_config: self.client_config(),
        };
        let (stream_metadata, receiver, abort_handle) =
            self.runtime.spawn_get_stream(specification)?;
        let remaining = range.map_or(stream_metadata.size(), ObjectRange::length);
        Ok(ObjectRead::new(
            stream_metadata,
            WebDavObjectReader::new(receiver, abort_handle, remaining),
        ))
    }

    fn metadata_for_key(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        let url = self.resource_url(object_key, false)?;
        let client_config = self.client_config();
        let root_url = self.root_resource_url();
        let root_segments = self.root_segments.clone();
        let object_key = object_key.clone();
        self.runtime.run(move |client| async move {
            metadata_async(
                &client,
                &client_config,
                url,
                object_key,
                root_url,
                root_segments,
            )
            .await
        })
    }

    fn list_page_inner(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        request.validate()?;
        let client_config = self.client_config();
        let root_url = self.root_resource_url();
        let root_segments = self.root_segments.clone();
        let prefix = request.prefix().as_str().to_owned();
        let cursor = request.cursor().map(|cursor| cursor.as_str().to_owned());
        let limit = request.limit();
        self.runtime.run(move |client| async move {
            list_page_async(
                &client,
                &client_config,
                root_url,
                root_segments,
                prefix,
                cursor,
                limit,
            )
            .await
        })
    }
}

impl fmt::Debug for WebDavStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebDavStorage")
            .field("collection_url", &self.root_url)
            .field("config", &self.config)
            .finish()
    }
}

impl RepositoryStorage for WebDavStorage {
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::ALL
    }

    fn read_stream(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        let metadata = self.metadata_for_key(object_key)?;
        self.open_get_stream(object_key, None, metadata)
    }

    fn read_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        let metadata = self.metadata_for_key(object_key)?;
        if range.end() > metadata.size() {
            return Err(StorageError::InvalidRange);
        }
        if range.is_empty() {
            return Ok(ObjectRead::new(metadata, Cursor::new(Vec::new())));
        }
        self.open_get_stream(object_key, Some(range), metadata)
    }

    fn metadata(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        self.metadata_for_key(object_key)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        self.write_stream_with_cancellation(object_key, source, options, None)
    }

    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        self.metadata_for_key(object_key)?;
        let url = self.resource_url(object_key, false)?;
        let client_config = self.client_config();
        self.runtime.run(move |client| async move {
            let response = authenticated(client.delete(url), &client_config)
                .send()
                .await
                .map_err(map_request_error)?;
            ensure_success(response, RequestKind::General, "DELETE").map(|_| ())
        })
    }

    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        self.list_page_inner(request)
    }
}

#[derive(Clone)]
struct WebDavClientConfig {
    username: String,
    password: String,
}

type RuntimeTask = Box<dyn FnOnce(&Client, &Handle, Arc<Semaphore>) + Send + 'static>;

struct WebDavRuntime {
    tasks: tokio_mpsc::Sender<RuntimeTask>,
}

impl WebDavRuntime {
    fn new(client: Client, max_concurrency: usize) -> StorageResult<Self> {
        let (task_sender, mut task_receiver) = tokio_mpsc::channel::<RuntimeTask>(
            WEBDAV_RUNTIME_CHANNEL_CAPACITY.max(max_concurrency),
        );
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("gib-webdav-runtime".to_owned())
            .spawn(move || {
                let runtime = RuntimeBuilder::new_current_thread().enable_all().build();
                let Ok(runtime) = runtime else {
                    let _ = ready_sender.send(false);
                    return;
                };
                let semaphore = Arc::new(Semaphore::new(max_concurrency));
                let handle = runtime.handle().clone();
                if ready_sender.send(true).is_err() {
                    return;
                }
                runtime.block_on(async move {
                    while let Some(task) = task_receiver.recv().await {
                        task(&client, &handle, semaphore.clone());
                    }
                });
            })
            .map_err(|_| StorageError::Unavailable)?;
        match ready_receiver.recv() {
            Ok(true) => Ok(Self { tasks: task_sender }),
            Ok(false) | Err(_) => Err(StorageError::Unavailable),
        }
    }

    fn run<T, F, Fut>(&self, operation: F) -> StorageResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = StorageResult<T>> + Send + 'static,
    {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        let task = Box::new(
            move |client: &Client, handle: &Handle, semaphore: Arc<Semaphore>| {
                let client = client.clone();
                handle.spawn(async move {
                    let result = match semaphore.acquire_owned().await {
                        Ok(permit) => {
                            let result = operation(client).await;
                            drop(permit);
                            result
                        }
                        Err(_) => Err(StorageError::Unavailable),
                    };
                    let _ = reply_sender.send(result);
                });
            },
        ) as RuntimeTask;
        self.send_task(task)?;
        reply_receiver
            .recv()
            .map_err(|_| StorageError::Unavailable)?
    }

    fn spawn_get_stream(
        &self,
        specification: GetStreamSpecification,
    ) -> StorageResult<(
        ObjectMetadata,
        tokio_mpsc::Receiver<StreamMessage>,
        AbortHandle,
    )> {
        let (data_sender, data_receiver) = tokio_mpsc::channel(WEBDAV_STREAM_CHANNEL_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (abort_sender, abort_receiver) = mpsc::sync_channel(1);
        let task = Box::new(
            move |client: &Client, handle: &Handle, semaphore: Arc<Semaphore>| {
                let client = client.clone();
                let task = handle.spawn(async move {
                    let permit = match semaphore.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            let _ = ready_sender.send(Err(StorageError::Unavailable));
                            return;
                        }
                    };
                    match open_get_response_async(&client, specification).await {
                        Ok((metadata, response)) => {
                            let _ = ready_sender.send(Ok(metadata));
                            forward_response_body(response, data_sender).await;
                        }
                        Err(error) => {
                            let _ = ready_sender.send(Err(error));
                        }
                    }
                    drop(permit);
                });
                let _ = abort_sender.send(task.abort_handle());
            },
        ) as RuntimeTask;
        self.send_task(task)?;
        let abort_handle = abort_receiver
            .recv()
            .map_err(|_| StorageError::Unavailable)?;
        let metadata = ready_receiver
            .recv()
            .map_err(|_| StorageError::Unavailable)??;
        Ok((metadata, data_receiver, abort_handle))
    }

    fn send_task(&self, mut task: RuntimeTask) -> StorageResult<()> {
        loop {
            match self.tasks.try_send(task) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(_)) => return Err(StorageError::Unavailable),
                Err(TrySendError::Full(returned_task)) => {
                    task = returned_task;
                    thread::yield_now();
                }
            }
        }
    }
}

struct GetStreamSpecification {
    url: Url,
    object_key: ObjectKey,
    expected_size: u64,
    expected_version: Option<StorageVersion>,
    range: Option<ObjectRange>,
    client_config: WebDavClientConfig,
}

enum StreamMessage {
    Data(Vec<u8>),
    End,
    Error(StorageError),
}

struct WebDavObjectReader {
    receiver: tokio_mpsc::Receiver<StreamMessage>,
    abort_handle: Option<AbortHandle>,
    current: Vec<u8>,
    current_offset: usize,
    remaining: u64,
    done: bool,
}

impl WebDavObjectReader {
    fn new(
        receiver: tokio_mpsc::Receiver<StreamMessage>,
        abort_handle: AbortHandle,
        remaining: u64,
    ) -> Self {
        Self {
            receiver,
            abort_handle: Some(abort_handle),
            current: Vec::new(),
            current_offset: 0,
            remaining,
            done: false,
        }
    }

    fn abort(&mut self) {
        self.receiver.close();
        if let Some(abort_handle) = self.abort_handle.take() {
            abort_handle.abort();
        }
        self.done = true;
    }
}

impl Read for WebDavObjectReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.done {
            return Ok(0);
        }
        loop {
            if self.current_offset < self.current.len() {
                let available = self.current.len() - self.current_offset;
                let amount = available.min(buffer.len());
                if (amount as u64) > self.remaining {
                    self.abort();
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        StorageError::InvalidRange.to_string(),
                    ));
                }
                buffer[..amount].copy_from_slice(
                    &self.current[self.current_offset..self.current_offset + amount],
                );
                self.current_offset += amount;
                self.remaining -= amount as u64;
                return Ok(amount);
            }

            self.current.clear();
            self.current_offset = 0;
            match self.receiver.blocking_recv() {
                Some(StreamMessage::Data(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    self.current = data;
                }
                Some(StreamMessage::End) => {
                    self.done = true;
                    self.abort_handle = None;
                    if self.remaining != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "WebDAV response ended before the advertised length",
                        ));
                    }
                    return Ok(0);
                }
                Some(StreamMessage::Error(error)) => {
                    self.done = true;
                    self.abort_handle = None;
                    return Err(io::Error::other(error.to_string()));
                }
                None => {
                    self.done = true;
                    self.abort_handle = None;
                    if self.remaining == 0 {
                        return Ok(0);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "WebDAV response stream was disconnected",
                    ));
                }
            }
        }
    }
}

impl Drop for WebDavObjectReader {
    fn drop(&mut self) {
        self.abort();
    }
}

struct StagedUpload {
    path: PathBuf,
    size: u64,
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stage_source(
    source: &mut dyn Read,
    expected_size: Option<u64>,
    cancellation: Option<&CancellationToken>,
) -> StorageResult<StagedUpload> {
    let path = create_temp_upload_path()?;
    let mut file = match OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => return Err(StorageError::from_io_error(&error)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file
            .metadata()
            .map_err(|error| StorageError::from_io_error(&error))?
            .permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)
            .map_err(|error| StorageError::from_io_error(&error))?;
    }

    let result = (|| {
        let size = copy_source(source, &mut file, expected_size, cancellation)?;
        file.flush()
            .map_err(|error| StorageError::from_io_error(&error))?;
        file.sync_all()
            .map_err(|error| StorageError::from_io_error(&error))?;
        Ok(size)
    })();
    match result {
        Ok(size) => Ok(StagedUpload { path, size }),
        Err(error) => {
            drop(file);
            let _ = fs::remove_file(&path);
            Err(error)
        }
    }
}

fn create_temp_upload_path() -> StorageResult<PathBuf> {
    let directory = std::env::temp_dir();
    for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
        let id = NEXT_TEMP_UPLOAD_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".gib-webdav-upload-{}-{id}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&path);
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(StorageError::from_io_error(&error)),
        }
    }
    Err(StorageError::Unavailable)
}

fn copy_source(
    source: &mut dyn Read,
    target: &mut dyn Write,
    expected_size: Option<u64>,
    cancellation: Option<&CancellationToken>,
) -> StorageResult<u64> {
    let mut buffer = [0_u8; DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE];
    let mut total = 0_u64;
    loop {
        check_cancelled(cancellation)?;
        let read = source
            .read(&mut buffer)
            .map_err(|error| StorageError::from_io_error(&error))?;
        if read > buffer.len() {
            return Err(StorageError::InvalidRequest);
        }
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(StorageError::InvalidRequest)?;
        if expected_size.is_some_and(|expected| total > expected) {
            return Err(StorageError::InvalidRequest);
        }
        target
            .write_all(&buffer[..read])
            .map_err(|error| StorageError::from_io_error(&error))?;
    }
    if expected_size.is_some_and(|expected| total != expected) {
        return Err(StorageError::InvalidRequest);
    }
    Ok(total)
}

struct PutStreamSpecification {
    client_config: WebDavClientConfig,
    root_url: Url,
    root_segments: Vec<String>,
    object_url: Url,
    object_key: ObjectKey,
    path: PathBuf,
    size: u64,
    condition: WriteCondition,
}

async fn put_staged_async(
    client: &Client,
    specification: PutStreamSpecification,
) -> StorageResult<ObjectMetadata> {
    let PutStreamSpecification {
        client_config,
        root_url,
        root_segments,
        object_url,
        object_key,
        path,
        size,
        condition,
    } = specification;
    ensure_parent_collections_async(
        client,
        &client_config,
        &root_url,
        &root_segments,
        &object_key,
    )
    .await?;

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| StorageError::from_io_error(&error))?;
    let stream = try_unfold(file, |mut file| async move {
        let mut buffer = vec![0_u8; DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE];
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| StorageError::from_io_error(&error))?;
        if read == 0 {
            Ok::<_, StorageError>(None)
        } else {
            buffer.truncate(read);
            Ok(Some((buffer, file)))
        }
    });
    let content_length =
        HeaderValue::from_str(&size.to_string()).map_err(|_| StorageError::InvalidRequest)?;
    let request_kind = condition.request_kind();
    let mut request = authenticated(client.put(object_url.clone()), &client_config)
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(reqwest::Body::wrap_stream(stream));
    request = match &condition {
        WriteCondition::Any => request,
        WriteCondition::IfAbsent => request.header(header::IF_NONE_MATCH, "*"),
        WriteCondition::IfVersion(version) => request.header(header::IF_MATCH, version),
    };
    let response = request.send().await.map_err(map_request_error)?;
    let response = ensure_success(response, request_kind, "PUT")
        .map_err(|error| map_condition_error(error, &condition))?;
    let version = response_etag(&response)?;
    let version = match version {
        Some(version) => version,
        None => {
            let metadata = metadata_async(
                client,
                &client_config,
                object_url,
                object_key.clone(),
                root_url,
                root_segments,
            )
            .await?;
            metadata
                .version()
                .cloned()
                .ok_or(StorageError::InvalidVersion)?
        }
    };
    Ok(ObjectMetadata::new(object_key, size, Some(version)))
}

async fn ensure_parent_collections_async(
    client: &Client,
    client_config: &WebDavClientConfig,
    root_url: &Url,
    root_segments: &[String],
    object_key: &ObjectKey,
) -> StorageResult<()> {
    let components = object_key.as_str().split('/').collect::<Vec<_>>();
    if components.len() <= 1 {
        return Ok(());
    }
    for end in 1..components.len() {
        let collection_url = url_for_segments(root_url, &components[..end], true)?;
        let response = authenticated(
            client.request(
                Method::from_bytes(b"MKCOL").map_err(|_| StorageError::InvalidRequest)?,
                collection_url.clone(),
            ),
            client_config,
        )
        .send()
        .await
        .map_err(map_request_error)?;
        let status = response.status();
        if status.is_success() {
            continue;
        }
        if status == StatusCode::METHOD_NOT_ALLOWED {
            let resources = propfind_async(
                client,
                client_config,
                collection_url.clone(),
                0,
                root_url.clone(),
                root_segments.to_vec(),
            )
            .await
            .map_err(|error| {
                if error == StorageError::NotFound {
                    StorageError::UnsupportedCapability
                } else {
                    error
                }
            })?;
            let expected = components[..end].join("/");
            match resources.into_iter().find(|resource| {
                resource
                    .key
                    .as_ref()
                    .is_some_and(|key| key.as_str() == expected)
            }) {
                Some(resource) if resource.is_collection => continue,
                Some(_) => return Err(StorageError::Conflict),
                None => return Err(StorageError::UnsupportedCapability),
            }
        }
        return Err(map_status(status, RequestKind::Mkcol, "MKCOL"));
    }
    Ok(())
}

async fn metadata_async(
    client: &Client,
    client_config: &WebDavClientConfig,
    url: Url,
    object_key: ObjectKey,
    root_url: Url,
    root_segments: Vec<String>,
) -> StorageResult<ObjectMetadata> {
    let resources = propfind_async(
        client,
        client_config,
        url.clone(),
        0,
        root_url,
        root_segments,
    )
    .await?;
    let resource = resources
        .into_iter()
        .find(|resource| resource.key.as_ref() == Some(&object_key))
        .ok_or(StorageError::NotFound)?;
    if resource.is_collection {
        return Err(StorageError::InvalidRequest);
    }
    if let (Some(size), Some(version)) = (resource.size, resource.version.clone()) {
        return Ok(ObjectMetadata::new(object_key, size, Some(version)));
    }

    let response = authenticated(client.head(url), client_config)
        .send()
        .await
        .map_err(map_request_error)?;
    let status = response.status();
    if !status.is_success()
        && !matches!(
            status,
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        )
    {
        return Err(map_status(status, RequestKind::General, "HEAD"));
    }
    let size = response
        .content_length()
        .or(resource.size)
        .ok_or(StorageError::InvalidRequest)?;
    let version = response_etag(&response)?.or(resource.version);
    let version = version.ok_or(StorageError::InvalidVersion)?;
    Ok(ObjectMetadata::new(object_key, size, Some(version)))
}

async fn open_get_response_async(
    client: &Client,
    specification: GetStreamSpecification,
) -> StorageResult<(ObjectMetadata, Response)> {
    let mut request = authenticated(client.get(specification.url), &specification.client_config);
    if let Some(range) = specification.range {
        let end = range
            .end()
            .checked_sub(1)
            .ok_or(StorageError::InvalidRange)?;
        request = request.header(header::RANGE, format!("bytes={}-{}", range.start(), end));
    }
    let response = request.send().await.map_err(map_request_error)?;
    let status = response.status();
    if specification.range.is_some() {
        if status == StatusCode::OK {
            return Err(StorageError::UnsupportedCapability);
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(map_status(status, RequestKind::Range, "GET range"));
        }
    } else if status != StatusCode::OK {
        return Err(map_status(status, RequestKind::General, "GET"));
    }

    let actual_version = response_etag(&response)?;
    if let (Some(expected), Some(actual)) = (&specification.expected_version, &actual_version)
        && !etag_versions_match(expected, actual)
    {
        return Err(StorageError::Conflict);
    }
    let version = specification
        .expected_version
        .or(actual_version)
        .ok_or(StorageError::InvalidVersion)?;
    let expected_length = specification
        .range
        .map_or(specification.expected_size, ObjectRange::length);
    if let Some(content_length) = response.content_length()
        && content_length != expected_length
    {
        return Err(StorageError::InvalidRange);
    }
    if let Some(range) = specification.range {
        let content_range = response
            .headers()
            .get(header::CONTENT_RANGE)
            .ok_or(StorageError::UnsupportedCapability)?;
        let content_range = content_range
            .to_str()
            .map_err(|_| StorageError::InvalidRange)?;
        let parsed = parse_content_range(content_range)?;
        if parsed.start != range.start()
            || parsed.end.saturating_add(1) != range.end()
            || parsed.total != specification.expected_size
        {
            return Err(StorageError::InvalidRange);
        }
    }
    Ok((
        ObjectMetadata::new(
            specification.object_key,
            specification.expected_size,
            Some(version),
        ),
        response,
    ))
}

async fn forward_response_body(response: Response, sender: tokio_mpsc::Sender<StreamMessage>) {
    let mut stream = response.bytes_stream();
    while let Some(result) = stream.next().await {
        match result {
            Ok(bytes) => {
                for chunk in bytes.chunks(DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE) {
                    if sender
                        .send(StreamMessage::Data(chunk.to_vec()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = sender
                    .send(StreamMessage::Error(map_request_error(error)))
                    .await;
                return;
            }
        }
    }
    let _ = sender.send(StreamMessage::End).await;
}

async fn list_page_async(
    client: &Client,
    client_config: &WebDavClientConfig,
    root_url: Url,
    root_segments: Vec<String>,
    prefix: String,
    cursor: Option<String>,
    limit: usize,
) -> StorageResult<ObjectListPage> {
    let mut pending = vec![(root_url.clone(), None::<String>)];
    let mut visited = BTreeSet::new();
    let mut objects = BTreeMap::<String, ObjectMetadata>::new();
    while let Some((collection_url, collection_key)) = pending.pop() {
        let identifier = collection_key.clone().unwrap_or_default();
        if !visited.insert(identifier.clone()) {
            continue;
        }
        let resources = propfind_async(
            client,
            client_config,
            collection_url,
            1,
            root_url.clone(),
            root_segments.clone(),
        )
        .await?;
        for resource in resources {
            let Some(resource_key) = resource.key.as_ref() else {
                continue;
            };
            if !is_descendant_key(resource_key.as_str(), collection_key.as_deref()) {
                return Err(StorageError::InvalidRequest);
            }
            if resource_key.as_str() == identifier {
                continue;
            }
            if resource.is_collection {
                pending.push((resource.url, Some(resource_key.as_str().to_owned())));
                continue;
            }
            if !matches_object_prefix(resource_key.as_str(), &prefix) {
                continue;
            }
            let size = match resource.size {
                Some(size) => size,
                None => metadata_async(
                    client,
                    client_config,
                    resource.url.clone(),
                    resource_key.clone(),
                    root_url.clone(),
                    root_segments.clone(),
                )
                .await?
                .size(),
            };
            let metadata = ObjectMetadata::new(resource_key.clone(), size, resource.version);
            objects
                .entry(resource_key.as_str().to_owned())
                .and_modify(|existing| {
                    if existing.version().is_none() {
                        *existing = metadata.clone();
                    }
                })
                .or_insert(metadata);
        }
    }

    let mut page = Vec::with_capacity(limit);
    let mut has_more = false;
    for (key, metadata) in objects.iter() {
        if cursor
            .as_deref()
            .is_some_and(|cursor| key.as_str() <= cursor)
        {
            continue;
        }
        if page.len() == limit {
            has_more = true;
            break;
        }
        page.push(metadata.clone());
    }
    let next_cursor = if has_more {
        page.last()
            .map(|metadata| ObjectCursor::new(metadata.key().as_str()))
            .transpose()?
    } else {
        None
    };
    Ok(ObjectListPage::new(page, next_cursor))
}

async fn propfind_async(
    client: &Client,
    client_config: &WebDavClientConfig,
    collection_url: Url,
    depth: u8,
    root_url: Url,
    root_segments: Vec<String>,
) -> StorageResult<Vec<DavResource>> {
    let method = Method::from_bytes(b"PROPFIND").map_err(|_| StorageError::InvalidRequest)?;
    let response = authenticated(
        client.request(method, collection_url.clone()),
        client_config,
    )
    .header("Depth", depth.to_string())
    .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
    .body(PROPFIND_BODY)
    .send()
    .await
    .map_err(map_request_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(map_status(status, RequestKind::Propfind, "PROPFIND"));
    }
    let body = read_response_limited(response).await?;
    let parsed = parse_multistatus(&body)?;
    let mut resources = BTreeMap::new();
    for resource in parsed {
        let resource = canonicalize_resource(resource, &collection_url, &root_url, &root_segments)?;
        resources
            .entry(resource.url.as_str().to_owned())
            .and_modify(|existing: &mut DavResource| existing.merge(&resource))
            .or_insert(resource);
    }
    Ok(resources.into_values().collect())
}

async fn read_response_limited(response: Response) -> StorageResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROPFIND_RESPONSE_BYTES)
    {
        return Err(StorageError::InvalidRequest);
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_PROPFIND_RESPONSE_BYTES) as usize,
    );
    let mut stream = response.bytes_stream();
    while let Some(result) = stream.next().await {
        let bytes = result.map_err(map_request_error)?;
        if (body.len() as u64)
            .checked_add(bytes.len() as u64)
            .is_none_or(|length| length > MAX_PROPFIND_RESPONSE_BYTES)
        {
            return Err(StorageError::InvalidRequest);
        }
        body.extend_from_slice(&bytes);
    }
    Ok(body)
}

#[derive(Clone)]
struct DavResource {
    url: Url,
    key: Option<ObjectKey>,
    is_collection: bool,
    size: Option<u64>,
    version: Option<StorageVersion>,
}

impl DavResource {
    fn merge(&mut self, other: &Self) {
        self.is_collection |= other.is_collection;
        if self.size.is_none() {
            self.size = other.size;
        }
        if self.version.is_none() {
            self.version = other.version.clone();
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedDavResource {
    href: String,
    is_collection: bool,
    etag: Option<String>,
    content_length: Option<u64>,
    content_length_text: String,
    status: Option<String>,
    statuses: Vec<u16>,
}

#[derive(Clone, Copy)]
enum ResponseField {
    Href,
    Etag,
    ContentLength,
    Status,
}

fn parse_multistatus(xml: &[u8]) -> StorageResult<Vec<ParsedDavResource>> {
    let mut reader = NsReader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut current = None::<ParsedDavResource>;
    let mut field = None::<ResponseField>;
    let mut in_resource_type = false;
    let mut saw_multistatus = false;
    let mut closed_multistatus = false;
    let mut depth = 0_usize;
    let mut resources = Vec::new();
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| StorageError::InvalidRequest)?;
        match event {
            Event::Start(element) => {
                let local = element.local_name();
                let is_multistatus =
                    is_dav_name(&namespace, local.as_ref()) && local.as_ref() == b"multistatus";
                if depth == 0 && !is_multistatus {
                    return Err(StorageError::InvalidRequest);
                }
                depth = depth.checked_add(1).ok_or(StorageError::InvalidRequest)?;
                if is_multistatus {
                    if depth != 1 || saw_multistatus || closed_multistatus {
                        return Err(StorageError::InvalidRequest);
                    }
                    saw_multistatus = true;
                } else if is_dav_name(&namespace, local.as_ref()) && local.as_ref() == b"response" {
                    if !saw_multistatus || closed_multistatus || current.is_some() {
                        return Err(StorageError::InvalidRequest);
                    }
                    current = Some(ParsedDavResource::default());
                } else if current.is_some() && is_dav_name(&namespace, local.as_ref()) {
                    match local.as_ref() {
                        b"href" => field = Some(ResponseField::Href),
                        b"getetag" => field = Some(ResponseField::Etag),
                        b"getcontentlength" => {
                            field = Some(ResponseField::ContentLength);
                            if let Some(resource) = current.as_mut() {
                                resource.content_length_text.clear();
                            }
                        }
                        b"status" => field = Some(ResponseField::Status),
                        b"resourcetype" => in_resource_type = true,
                        b"collection" if in_resource_type => {
                            if let Some(resource) = current.as_mut() {
                                resource.is_collection = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Empty(element) => {
                let local = element.local_name();
                if depth == 0 {
                    return Err(StorageError::InvalidRequest);
                }
                if is_dav_name(&namespace, local.as_ref())
                    && matches!(local.as_ref(), b"multistatus" | b"response")
                {
                    return Err(StorageError::InvalidRequest);
                }
                if current.is_some()
                    && is_dav_name(&namespace, local.as_ref())
                    && local.as_ref() == b"collection"
                    && in_resource_type
                    && let Some(resource) = current.as_mut()
                {
                    resource.is_collection = true;
                }
            }
            Event::Text(text) => append_text(&mut current, field, text)?,
            Event::CData(text) => append_cdata(&mut current, field, text)?,
            Event::End(element) => {
                let local = element.local_name();
                if depth == 0 {
                    return Err(StorageError::InvalidRequest);
                }
                if is_dav_name(&namespace, local.as_ref()) {
                    match local.as_ref() {
                        b"href" => field = None,
                        b"getetag" => field = None,
                        b"getcontentlength" => {
                            if let Some(resource) = current.as_mut() {
                                let value = std::mem::take(&mut resource.content_length_text);
                                if !value.trim().is_empty() {
                                    let parsed = value
                                        .trim()
                                        .parse::<u64>()
                                        .map_err(|_| StorageError::InvalidRequest)?;
                                    if resource
                                        .content_length
                                        .is_some_and(|existing| existing != parsed)
                                    {
                                        return Err(StorageError::InvalidRequest);
                                    }
                                    resource.content_length = Some(parsed);
                                }
                            }
                            field = None;
                        }
                        b"status" => {
                            if let Some(resource) = current.as_mut()
                                && let Some(status) = resource.status.take()
                            {
                                if let Some(code) = status
                                    .split_whitespace()
                                    .find_map(|token| token.parse::<u16>().ok())
                                {
                                    resource.statuses.push(code);
                                } else if !status.trim().is_empty() {
                                    return Err(StorageError::InvalidRequest);
                                }
                            }
                            field = None;
                        }
                        b"resourcetype" => in_resource_type = false,
                        b"response" => {
                            if !saw_multistatus || depth < 2 {
                                return Err(StorageError::InvalidRequest);
                            }
                            let resource = current.take().ok_or(StorageError::InvalidRequest)?;
                            if resource.href.trim().is_empty() {
                                return Err(StorageError::InvalidRequest);
                            }
                            let skip = !resource.statuses.is_empty()
                                && resource
                                    .statuses
                                    .iter()
                                    .all(|status| !(200..300).contains(status));
                            if !skip {
                                resources.push(resource);
                            }
                        }
                        b"multistatus" => {
                            if depth != 1 || current.is_some() {
                                return Err(StorageError::InvalidRequest);
                            }
                            closed_multistatus = true;
                        }
                        _ => {}
                    }
                }
                depth = depth.checked_sub(1).ok_or(StorageError::InvalidRequest)?;
            }
            Event::DocType(_) => return Err(StorageError::InvalidRequest),
            Event::Eof => {
                if depth != 0 || !saw_multistatus || !closed_multistatus || current.is_some() {
                    return Err(StorageError::InvalidRequest);
                }
                return Ok(resources);
            }
            _ => {}
        }
        buffer.clear();
    }
}

fn append_text(
    current: &mut Option<ParsedDavResource>,
    field: Option<ResponseField>,
    text: BytesText<'_>,
) -> StorageResult<()> {
    let text = text.unescape().map_err(|_| StorageError::InvalidRequest)?;
    append_field_value(current, field, text.as_ref())
}

fn append_cdata(
    current: &mut Option<ParsedDavResource>,
    field: Option<ResponseField>,
    text: BytesCData<'_>,
) -> StorageResult<()> {
    let text = std::str::from_utf8(text.as_ref()).map_err(|_| StorageError::InvalidRequest)?;
    append_field_value(current, field, text)
}

fn append_field_value(
    current: &mut Option<ParsedDavResource>,
    field: Option<ResponseField>,
    value: &str,
) -> StorageResult<()> {
    let Some(resource) = current.as_mut() else {
        return Ok(());
    };
    match field {
        Some(ResponseField::Href) => resource.href.push_str(value),
        Some(ResponseField::Etag) => resource
            .etag
            .get_or_insert_with(String::new)
            .push_str(value),
        Some(ResponseField::ContentLength) => resource.content_length_text.push_str(value),
        Some(ResponseField::Status) => {
            let status = resource.status.get_or_insert_with(String::new);
            status.push_str(value);
        }
        None => {}
    }
    Ok(())
}

fn canonicalize_resource(
    resource: ParsedDavResource,
    base_url: &Url,
    root_url: &Url,
    root_segments: &[String],
) -> StorageResult<DavResource> {
    let href = resource.href.trim();
    validate_href_syntax(href)?;
    let candidate = match Url::parse(href) {
        Ok(url) => url,
        Err(_) => base_url
            .join(href)
            .map_err(|_| StorageError::InvalidRequest)?,
    };
    if candidate.query().is_some()
        || candidate.fragment().is_some()
        || !same_origin(root_url, &candidate)
        || !candidate.username().is_empty()
        || candidate.password().is_some()
    {
        return Err(StorageError::InvalidRequest);
    }
    let candidate_segments = decode_url_segments(candidate.path())?;
    if candidate_segments.len() < root_segments.len()
        || candidate_segments[..root_segments.len()] != root_segments[..]
    {
        return Err(StorageError::InvalidRequest);
    }
    let relative_segments = &candidate_segments[root_segments.len()..];
    let key = if relative_segments.is_empty() {
        None
    } else {
        Some(ObjectKey::new(relative_segments.join("/"))?)
    };
    let version = resource
        .etag
        .as_deref()
        .and_then(|etag| usable_etag(Some(etag)))
        .map(storage_version_from_etag)
        .transpose()?;
    let url = if resource.is_collection {
        collection_url(candidate)?
    } else {
        candidate
    };
    Ok(DavResource {
        url,
        key,
        is_collection: resource.is_collection,
        size: resource.content_length,
        version,
    })
}

fn validate_href_syntax(href: &str) -> StorageResult<()> {
    if href.is_empty() || href.len() > MAX_WEBDAV_URL_LENGTH || href.contains('\\') {
        return Err(StorageError::InvalidRequest);
    }
    let raw_path = raw_href_path(href)?;
    validate_raw_path_segments(raw_path)
}

fn raw_href_path(href: &str) -> StorageResult<&str> {
    let end = href.find(['?', '#']).unwrap_or(href.len());
    let without_suffix = &href[..end];
    if let Some(authority_start) = without_suffix.find("://") {
        let authority = &without_suffix[authority_start + 3..];
        let path_start = authority.find('/').unwrap_or(authority.len());
        return Ok(&authority[path_start..]);
    }
    if let Some(stripped) = without_suffix.strip_prefix("//") {
        let path_start = stripped
            .find('/')
            .map_or(without_suffix.len(), |offset| offset + 2);
        return Ok(&without_suffix[path_start..]);
    }
    Ok(without_suffix)
}

fn validate_raw_path_segments(raw_path: &str) -> StorageResult<()> {
    for raw_segment in raw_path.split('/') {
        let decoded = percent_decode_str(raw_segment)
            .decode_utf8()
            .map_err(|_| StorageError::InvalidRequest)?;
        if decoded == "."
            || decoded == ".."
            || decoded.contains('/')
            || decoded.contains('\\')
            || decoded.contains('\0')
        {
            return Err(StorageError::InvalidRequest);
        }
    }
    Ok(())
}

fn normalize_collection_url(value: &str) -> StorageResult<Url> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_WEBDAV_URL_LENGTH {
        return Err(StorageError::InvalidRequest);
    }
    let mut url = Url::parse(value).map_err(|_| StorageError::InvalidRequest)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(StorageError::InvalidRequest);
    }
    let raw_path = raw_href_path(value)?;
    validate_raw_path_segments(raw_path)?;
    let _ = decode_url_segments(url.path())?;
    let path = url.path().to_owned();
    if path.is_empty() {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| StorageError::InvalidRequest)?;
        segments.push("");
    } else if path != "/" {
        let trailing_empty_segments = path
            .split('/')
            .rev()
            .take_while(|segment| segment.is_empty())
            .count();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| StorageError::InvalidRequest)?;
        if trailing_empty_segments == 0 {
            segments.push("");
        } else {
            for _ in 1..trailing_empty_segments {
                segments.pop_if_empty();
            }
        }
    }
    Ok(url)
}

fn decode_url_segments(path: &str) -> StorageResult<Vec<String>> {
    let remainder = path.strip_prefix('/').ok_or(StorageError::InvalidRequest)?;
    let raw_segments = remainder.split('/').collect::<Vec<_>>();
    let mut segments = Vec::new();
    for (index, raw_segment) in raw_segments.iter().enumerate() {
        if raw_segment.is_empty() {
            if raw_segments[index..]
                .iter()
                .all(|segment| segment.is_empty())
            {
                continue;
            }
            return Err(StorageError::InvalidRequest);
        }
        let decoded = percent_decode_str(raw_segment)
            .decode_utf8()
            .map_err(|_| StorageError::InvalidRequest)?
            .into_owned();
        if decoded == "."
            || decoded == ".."
            || decoded.contains('/')
            || decoded.contains('\\')
            || decoded.contains('\0')
        {
            return Err(StorageError::InvalidRequest);
        }
        segments.push(decoded);
    }
    Ok(segments)
}

fn url_for_segments(root_url: &Url, segments: &[&str], collection: bool) -> StorageResult<Url> {
    let mut url = root_url.clone();
    let mut path_segments = url
        .path_segments_mut()
        .map_err(|_| StorageError::InvalidRequest)?;
    path_segments.pop_if_empty();
    for segment in segments {
        path_segments.push(segment);
    }
    if collection {
        path_segments.push("");
    }
    drop(path_segments);
    Ok(url)
}

fn collection_url(mut url: Url) -> StorageResult<Url> {
    if !url.path().ends_with('/') {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| StorageError::InvalidRequest)?;
        segments.push("");
    }
    Ok(url)
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_descendant_key(key: &str, collection: Option<&str>) -> bool {
    match collection {
        None => true,
        Some(collection) => {
            key == collection
                || key
                    .strip_prefix(collection)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }
    }
}

fn matches_object_prefix(key: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn authenticated(request: RequestBuilder, config: &WebDavClientConfig) -> RequestBuilder {
    request.basic_auth(&config.username, Some(&config.password))
}

fn response_etag(response: &Response) -> StorageResult<Option<StorageVersion>> {
    let Some(value) = response.headers().get(header::ETAG) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| StorageError::InvalidVersion)?;
    usable_etag(Some(value))
        .map(storage_version_from_etag)
        .transpose()
}

fn usable_etag(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value == "*" {
        None
    } else {
        Some(value.to_owned())
    }
}

fn etag_versions_match(expected: &StorageVersion, actual: &StorageVersion) -> bool {
    fn without_weak_validator_prefix(value: &[u8]) -> &[u8] {
        value.strip_prefix(b"W/").unwrap_or(value)
    }

    without_weak_validator_prefix(expected.as_bytes())
        == without_weak_validator_prefix(actual.as_bytes())
}

fn storage_version_from_etag(etag: String) -> StorageResult<StorageVersion> {
    if etag.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(StorageError::InvalidVersion);
    }
    StorageVersion::from_bytes(etag.into_bytes())
}

#[derive(Clone, Copy)]
enum RequestKind {
    General,
    Conditional,
    Range,
    Propfind,
    Mkcol,
}

impl WriteCondition {
    fn request_kind(&self) -> RequestKind {
        match self {
            Self::Any => RequestKind::General,
            Self::IfAbsent | Self::IfVersion(_) => RequestKind::Conditional,
        }
    }
}

enum WriteCondition {
    Any,
    IfAbsent,
    IfVersion(String),
}

fn map_condition_error(error: StorageError, condition: &WriteCondition) -> StorageError {
    match condition {
        WriteCondition::IfAbsent
            if matches!(
                error,
                StorageError::AlreadyExists
                    | StorageError::Conflict
                    | StorageError::ConditionNotMet
            ) =>
        {
            StorageError::AlreadyExists
        }
        _ => error,
    }
}

impl WriteCondition {
    fn from_storage_condition(condition: &StorageWriteCondition) -> StorageResult<Self> {
        match condition {
            StorageWriteCondition::Any => Ok(Self::Any),
            StorageWriteCondition::IfAbsent => Ok(Self::IfAbsent),
            StorageWriteCondition::IfVersion(version) => {
                let value = String::from_utf8(version.as_bytes().to_vec())
                    .map_err(|_| StorageError::InvalidVersion)?;
                if value.is_empty()
                    || value == "*"
                    || value.bytes().any(|byte| byte.is_ascii_control())
                    || HeaderValue::from_str(&value).is_err()
                {
                    return Err(StorageError::InvalidVersion);
                }
                Ok(Self::IfVersion(value))
            }
        }
    }
}

fn ensure_success(
    response: Response,
    kind: RequestKind,
    operation: &str,
) -> StorageResult<Response> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(map_status(response.status(), kind, operation))
    }
}

fn map_status(status: StatusCode, _kind: RequestKind, _operation: &str) -> StorageError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            StorageError::Authentication
        }
        StatusCode::FORBIDDEN => StorageError::PermissionDenied,
        StatusCode::NOT_FOUND => StorageError::NotFound,
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED | StatusCode::LOCKED => {
            StorageError::Conflict
        }
        StatusCode::RANGE_NOT_SATISFIABLE => StorageError::InvalidRange,
        StatusCode::TOO_MANY_REQUESTS => StorageError::RateLimited,
        StatusCode::REQUEST_TIMEOUT
        | StatusCode::BAD_GATEWAY
        | StatusCode::GATEWAY_TIMEOUT
        | StatusCode::INTERNAL_SERVER_ERROR => StorageError::Transient,
        StatusCode::SERVICE_UNAVAILABLE => StorageError::Unavailable,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            StorageError::UnsupportedCapability
        }
        status if status.is_redirection() => StorageError::InvalidRequest,
        _ => StorageError::Io,
    }
}

fn map_request_error(error: reqwest::Error) -> StorageError {
    if error.is_timeout() {
        StorageError::Transient
    } else if error.is_connect() {
        StorageError::Unavailable
    } else if error.is_builder() {
        StorageError::InvalidRequest
    } else {
        StorageError::Transient
    }
}

fn validate_credential(value: &str, required: bool) -> StorageResult<()> {
    if (required && value.is_empty())
        || value.len() > MAX_WEBDAV_CREDENTIAL_LENGTH
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(StorageError::InvalidRequest);
    }
    Ok(())
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> StorageResult<()> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(StorageError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

fn parse_content_range(value: &str) -> StorageResult<ContentRange> {
    let (unit, range) = value.split_once(' ').ok_or(StorageError::InvalidRange)?;
    if unit != "bytes" {
        return Err(StorageError::UnsupportedCapability);
    }
    let (range, total) = range.split_once('/').ok_or(StorageError::InvalidRange)?;
    let (start, end) = range.split_once('-').ok_or(StorageError::InvalidRange)?;
    let total = total
        .parse::<u64>()
        .map_err(|_| StorageError::InvalidRange)?;
    let start = start
        .parse::<u64>()
        .map_err(|_| StorageError::InvalidRange)?;
    let end = end.parse::<u64>().map_err(|_| StorageError::InvalidRange)?;
    if end < start {
        return Err(StorageError::InvalidRange);
    }
    Ok(ContentRange { start, end, total })
}

fn is_dav_name(namespace: &ResolveResult<'_>, local: &[u8]) -> bool {
    matches!(namespace, ResolveResult::Bound(namespace) if namespace.as_ref() == DAV_NAMESPACE)
        && !local.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_collection_urls_without_double_encoding() -> StorageResult<()> {
        assert_eq!(
            normalize_collection_url("https://example.test/dav/root///")?.as_str(),
            "https://example.test/dav/root/"
        );
        assert_eq!(
            normalize_collection_url("https://example.test/space%20name")?.as_str(),
            "https://example.test/space%20name/"
        );
        assert_eq!(
            normalize_collection_url("https://example.test")?.as_str(),
            "https://example.test/"
        );
        Ok(())
    }

    #[test]
    fn rejects_unsafe_collection_urls() {
        for url in [
            "ftp://example.test/dav/",
            "https://example.test/dav/?tenant=one",
            "https://example.test/dav/#fragment",
            "https://user:password@example.test/dav/",
            "https://example.test/dav/../outside",
            "https://example.test/dav/%2e%2e/outside",
            "https://example.test/dav/a//b",
        ] {
            assert_eq!(
                WebDavStorageConfig::new(url, "user", "secret").map(|_| ()),
                Err(StorageError::InvalidRequest)
            );
        }
    }

    #[test]
    fn requires_explicit_http_opt_in() -> StorageResult<()> {
        let config = WebDavStorageConfig::new("http://example.test/dav", "user", "secret")?;
        assert!(matches!(
            WebDavStorage::build(config.clone()),
            Err(StorageError::InvalidRequest)
        ));
        let config = config.with_allow_insecure_http(true);
        assert!(WebDavStorage::build(config).is_ok());
        Ok(())
    }

    #[test]
    fn encodes_resource_segments_and_decodes_returned_hrefs() -> StorageResult<()> {
        let root = normalize_collection_url("https://example.test/dav/root/")?;
        let url = url_for_segments(&root, &["space name", "café.txt"], false)?;
        assert_eq!(
            url.as_str(),
            "https://example.test/dav/root/space%20name/caf%C3%A9.txt"
        );
        let parsed = ParsedDavResource {
            href: "/dav/root/space%20name/caf%C3%A9.txt".to_owned(),
            is_collection: false,
            etag: Some("\"v1\"".to_owned()),
            content_length: Some(4),
            content_length_text: String::new(),
            status: None,
            statuses: vec![200],
        };
        let resource =
            canonicalize_resource(parsed, &root, &root, &decode_url_segments(root.path())?)?;
        assert_eq!(
            resource.key.as_ref().map(ObjectKey::as_str),
            Some("space name/café.txt")
        );
        Ok(())
    }

    #[test]
    fn rejects_cross_origin_and_traversal_hrefs() -> StorageResult<()> {
        let root = normalize_collection_url("https://example.test/dav/root/")?;
        for href in [
            "https://other.test/dav/root/secret",
            "/dav/other/secret",
            "../root/secret",
            "/dav/root/%2e%2e/secret",
            "/dav/root/a%2Fb",
        ] {
            let parsed = ParsedDavResource {
                href: href.to_owned(),
                is_collection: false,
                etag: None,
                content_length: Some(1),
                content_length_text: String::new(),
                status: None,
                statuses: vec![200],
            };
            assert_eq!(
                canonicalize_resource(parsed, &root, &root, &decode_url_segments(root.path())?)
                    .map(|_| ()),
                Err(StorageError::InvalidRequest)
            );
        }
        Ok(())
    }

    #[test]
    fn parses_namespace_qualified_multistatus_variants() -> StorageResult<()> {
        let xml = br#"
            <?xml version="1.0"?>
            <d:multistatus xmlns:d="DAV:">
              <d:response>
                <d:href>/dav/root/</d:href>
                <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
                  <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
              </d:response>
              <d:response>
                <d:href>/dav/root/hello%20world.txt</d:href>
                <d:propstat><d:prop><d:resourcetype/><d:getetag>&quot;v1&quot;</d:getetag>
                  <d:getcontentlength>1<!--split-->2</d:getcontentlength></d:prop>
                  <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
              </d:response>
            </d:multistatus>
        "#;
        let parsed = parse_multistatus(xml)?;
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_collection);
        assert_eq!(parsed[1].etag.as_deref(), Some("\"v1\""));
        assert_eq!(parsed[1].content_length, Some(12));
        Ok(())
    }

    #[test]
    fn parses_apache_dav_propstat_variants() -> StorageResult<()> {
        let xml = br#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:">
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/" xmlns:g0="DAV:">
<D:href>/dav/</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype><D:collection/></lp1:resourcetype>
<lp1:getetag>"1000-65a5c545e903d"</lp1:getetag>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
<D:propstat>
<D:prop>
<g0:getcontentlength/>
</D:prop>
<D:status>HTTP/1.1 404 Not Found</D:status>
</D:propstat>
</D:response>
</D:multistatus>"#;
        let parsed = parse_multistatus(xml)?;
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_collection);
        assert_eq!(parsed[0].statuses, vec![200, 404]);
        Ok(())
    }

    #[test]
    fn rejects_malformed_or_non_dav_xml() {
        for xml in [
            b"<multistatus><response><href>/root".as_slice(),
            b"<d:multistatus xmlns:d=\"wrong\"><d:response/></d:multistatus>".as_slice(),
            b"<d:multistatus xmlns:d=\"DAV:\"><d:response/></d:multistatus>".as_slice(),
            b"<wrapper><d:multistatus xmlns:d=\"DAV:\"></d:multistatus></wrapper>".as_slice(),
            b"<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>x</d:href></d:multistatus>"
                .as_slice(),
        ] {
            assert_eq!(parse_multistatus(xml), Err(StorageError::InvalidRequest));
        }
    }

    #[test]
    fn parses_ranges_defensively() -> StorageResult<()> {
        let parsed = parse_content_range("bytes 10-19/100")?;
        assert_eq!(parsed.start, 10);
        assert_eq!(parsed.end, 19);
        assert_eq!(parsed.total, 100);
        assert_eq!(
            parse_content_range("bytes 19-10/100"),
            Err(StorageError::InvalidRange)
        );
        Ok(())
    }

    #[test]
    fn treats_a_collection_resource_as_its_own_descendant() {
        assert!(is_descendant_key("manual", Some("manual")));
        assert!(is_descendant_key("manual/object", Some("manual")));
        assert!(!is_descendant_key("manual-other", Some("manual")));
    }

    #[test]
    fn accepts_weak_and_strong_etags_for_the_same_read_version() -> StorageResult<()> {
        let strong = StorageVersion::new(b"\"v1\"".to_vec())?;
        let weak = StorageVersion::new(b"W/\"v1\"".to_vec())?;
        let different = StorageVersion::new(b"\"v2\"".to_vec())?;
        assert!(etag_versions_match(&strong, &weak));
        assert!(!etag_versions_match(&strong, &different));
        Ok(())
    }

    #[test]
    fn maps_conditional_statuses_without_downgrading_the_condition() {
        assert_eq!(
            map_condition_error(StorageError::Conflict, &WriteCondition::IfAbsent,),
            StorageError::AlreadyExists
        );
        assert_eq!(
            map_condition_error(StorageError::ConditionNotMet, &WriteCondition::IfAbsent,),
            StorageError::AlreadyExists
        );
        assert_eq!(
            map_condition_error(
                StorageError::Conflict,
                &WriteCondition::IfVersion(String::from("\"v1\"")),
            ),
            StorageError::Conflict
        );
        assert_eq!(
            map_status(
                StatusCode::METHOD_NOT_ALLOWED,
                RequestKind::General,
                "DELETE",
            ),
            StorageError::UnsupportedCapability
        );
        assert_eq!(
            map_status(StatusCode::UNAUTHORIZED, RequestKind::General, "PROPFIND",),
            StorageError::Authentication
        );
        let wildcard = StorageVersion::new(b"*".to_vec()).expect("wildcard is bounded");
        assert!(matches!(
            WriteCondition::from_storage_condition(&StorageWriteCondition::IfVersion(wildcard)),
            Err(StorageError::InvalidVersion)
        ));
    }

    #[test]
    fn redacts_basic_auth_from_debug() -> StorageResult<()> {
        let config = WebDavStorageConfig::new(
            "https://example.test/dav",
            "sensitive-user",
            "sensitive-password",
        )?;
        let debug = format!("{config:?}");
        assert!(!debug.contains("sensitive-user"));
        assert!(!debug.contains("sensitive-password"));
        assert!(debug.contains("<redacted>"));
        Ok(())
    }
}
