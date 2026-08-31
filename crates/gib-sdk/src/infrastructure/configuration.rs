use crate::application::ports::{ConfigurationError, ConfigurationResult, ConfigurationStorage};
use crate::format::MAX_IDENTITY_CONFIGURATION_BYTES;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The directory used by the global Gib configuration.
pub const GLOBAL_CONFIGURATION_DIRECTORY: &str = ".gib";

/// The file used for the global author identity configuration.
pub const IDENTITY_CONFIGURATION_FILE_NAME: &str = "config.msgpack";

static NEXT_CONFIGURATION_TEMP_ID: AtomicU64 = AtomicU64::new(1);
static CONFIGURATION_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

/// A filesystem-backed global configuration store.
///
/// The path is injected by the caller so tests and embedded applications never
/// need to read or modify a developer's real global configuration. Writes use a
/// unique sibling temporary file, synchronization, and platform-appropriate
/// atomic replacement.
#[derive(Clone)]
pub struct LocalConfiguration {
    state: Arc<LocalConfigurationState>,
}

struct LocalConfigurationState {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl LocalConfiguration {
    /// Creates a configuration store for an explicit file path.
    pub fn new(path: impl AsRef<Path>) -> ConfigurationResult<Self> {
        let path = path.as_ref().to_path_buf();
        validate_configuration_path(&path)?;
        let lock = lock_for_path(&path)?;
        Ok(Self {
            state: Arc::new(LocalConfigurationState { path, lock }),
        })
    }

    /// Creates a store for `config.msgpack` below an explicit directory.
    pub fn in_directory(directory: impl AsRef<Path>) -> ConfigurationResult<Self> {
        Self::new(directory.as_ref().join(IDENTITY_CONFIGURATION_FILE_NAME))
    }

    /// Resolves the current user's global `.gib/config.msgpack` path.
    pub fn global() -> ConfigurationResult<Self> {
        let home = home_directory().ok_or(ConfigurationError::Unavailable)?;
        Self::in_directory(home.join(GLOBAL_CONFIGURATION_DIRECTORY))
    }

    /// Returns the injected configuration file path.
    pub fn path(&self) -> &Path {
        &self.state.path
    }
}

impl std::fmt::Debug for LocalConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalConfiguration(..)")
    }
}

impl ConfigurationStorage for LocalConfiguration {
    fn read(&self) -> ConfigurationResult<Vec<u8>> {
        let _guard = self
            .state
            .lock
            .lock()
            .map_err(|_| ConfigurationError::Unavailable)?;
        let metadata = match fs::symlink_metadata(&self.state.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigurationError::NotFound);
            }
            Err(_) => return Err(ConfigurationError::Io),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ConfigurationError::InvalidPath);
        }
        let length = usize::try_from(metadata.len()).map_err(|_| ConfigurationError::TooLarge)?;
        if length > MAX_IDENTITY_CONFIGURATION_BYTES {
            return Err(ConfigurationError::TooLarge);
        }
        fs::read(&self.state.path).map_err(map_io_error)
    }

    fn write_atomically(&self, contents: &[u8]) -> ConfigurationResult<()> {
        if contents.len() > MAX_IDENTITY_CONFIGURATION_BYTES {
            return Err(ConfigurationError::TooLarge);
        }

        let _guard = self
            .state
            .lock
            .lock()
            .map_err(|_| ConfigurationError::Unavailable)?;
        let parent = configuration_parent(&self.state.path);
        ensure_parent_directory(parent)?;
        ensure_destination_is_safe(&self.state.path)?;

        let temporary_path = write_temporary_file(&self.state.path, contents)?;
        if let Err(error) = replace_file(&temporary_path, &self.state.path) {
            let _ = fs::remove_file(&temporary_path);
            return Err(map_io_error(error));
        }
        sync_parent(parent)?;
        Ok(())
    }
}

/// An in-memory configuration store for SDK callers and deterministic tests.
#[derive(Clone, Default)]
pub struct MemoryConfiguration {
    state: Arc<Mutex<Option<Vec<u8>>>>,
}

impl MemoryConfiguration {
    /// Creates an empty in-memory configuration store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the raw encoded bytes for test inspection.
    pub fn read_bytes(&self) -> ConfigurationResult<Vec<u8>> {
        self.read()
    }

    /// Replaces the raw bytes for test setup.
    pub fn replace_bytes(&self, contents: impl AsRef<[u8]>) -> ConfigurationResult<()> {
        if contents.as_ref().len() > MAX_IDENTITY_CONFIGURATION_BYTES {
            return Err(ConfigurationError::TooLarge);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigurationError::Unavailable)?;
        *state = Some(contents.as_ref().to_vec());
        Ok(())
    }
}

impl std::fmt::Debug for MemoryConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let configured = self.state.lock().is_ok_and(|state| state.is_some());
        formatter
            .debug_struct("MemoryConfiguration")
            .field("configured", &configured)
            .finish()
    }
}

impl ConfigurationStorage for MemoryConfiguration {
    fn read(&self) -> ConfigurationResult<Vec<u8>> {
        self.state
            .lock()
            .map_err(|_| ConfigurationError::Unavailable)?
            .clone()
            .ok_or(ConfigurationError::NotFound)
    }

    fn write_atomically(&self, contents: &[u8]) -> ConfigurationResult<()> {
        if contents.len() > MAX_IDENTITY_CONFIGURATION_BYTES {
            return Err(ConfigurationError::TooLarge);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigurationError::Unavailable)?;
        *state = Some(contents.to_vec());
        Ok(())
    }
}

fn validate_configuration_path(path: &Path) -> ConfigurationResult<()> {
    if path.as_os_str().is_empty()
        || path.file_name().is_none()
        || path
            .file_name()
            .is_some_and(|name| name == "." || name == "..")
    {
        return Err(ConfigurationError::InvalidPath);
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(value) if value.to_string_lossy().contains('\0')
        )
    }) {
        return Err(ConfigurationError::InvalidPath);
    }
    Ok(())
}

fn lock_for_path(path: &Path) -> ConfigurationResult<Arc<Mutex<()>>> {
    let locks = CONFIGURATION_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().map_err(|_| ConfigurationError::Unavailable)?;
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn ensure_parent_directory(parent: &Path) -> ConfigurationResult<()> {
    fs::create_dir_all(parent).map_err(map_io_error)?;
    let metadata = fs::symlink_metadata(parent).map_err(map_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ConfigurationError::InvalidPath);
    }
    Ok(())
}

fn configuration_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn ensure_destination_is_safe(path: &Path) -> ConfigurationResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                Err(ConfigurationError::InvalidPath)
            } else {
                Ok(())
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ConfigurationError::Io),
    }
}

fn write_temporary_file(path: &Path, contents: &[u8]) -> ConfigurationResult<PathBuf> {
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ConfigurationError::InvalidPath)?;
    let temporary_path = path.with_file_name(format!(
        ".{filename}.gib-tmp-{}-{}",
        std::process::id(),
        NEXT_CONFIGURATION_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options.open(&temporary_path).map_err(map_io_error)?;
    if let Err(error) = set_private_permissions(&file) {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if file.write_all(contents).is_err() || file.sync_all().is_err() {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(ConfigurationError::Io);
    }
    drop(file);
    Ok(temporary_path)
}

fn set_private_permissions(file: &File) -> ConfigurationResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(map_io_error)?;
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

fn map_io_error(error: std::io::Error) -> ConfigurationError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ConfigurationError::NotFound,
        std::io::ErrorKind::AlreadyExists => ConfigurationError::Io,
        _ => ConfigurationError::Io,
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive
    // for this synchronous call. MoveFileExW does not retain either pointer.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;

#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

fn sync_parent(parent: &Path) -> ConfigurationResult<()> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| ConfigurationError::Io)?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                Some(PathBuf::from(drive).join(path))
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}
