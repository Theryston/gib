use std::sync::Arc;

/// A failure returned by a global configuration storage adapter.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigurationError {
    /// The configuration file is not present.
    NotFound,
    /// The configured file or parent path is not safe to use.
    InvalidPath,
    /// The configuration object exceeds the adapter's bounded size.
    TooLarge,
    /// The adapter could not complete a filesystem or persistence operation.
    Io,
    /// The adapter could not provide a consistent result.
    Unavailable,
}

impl std::fmt::Display for ConfigurationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotFound => "configuration was not found",
            Self::InvalidPath => "configuration path is invalid",
            Self::TooLarge => "configuration is too large",
            Self::Io => "configuration I/O operation failed",
            Self::Unavailable => "configuration storage is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ConfigurationError {}

/// Result type returned by global configuration storage adapters.
pub type ConfigurationResult<T> = std::result::Result<T, ConfigurationError>;

/// Storage capability for one bounded, atomically replaced configuration file.
///
/// The SDK supplies already-encoded binary configuration bytes. Implementors
/// must make [`Self::write_atomically`] durable before returning success and
/// must leave the previous file untouched when preparation or replacement
/// fails.
pub trait ConfigurationStorage: Send + Sync {
    /// Reads the complete bounded configuration object.
    fn read(&self) -> ConfigurationResult<Vec<u8>>;

    /// Replaces the configuration object atomically.
    fn write_atomically(&self, contents: &[u8]) -> ConfigurationResult<()>;

    /// Alias for [`Self::read`] using configuration terminology.
    fn read_configuration(&self) -> ConfigurationResult<Vec<u8>> {
        self.read()
    }

    /// Alias for [`Self::write_atomically`] using replacement terminology.
    fn replace_atomically(&self, contents: &[u8]) -> ConfigurationResult<()> {
        self.write_atomically(contents)
    }
}

impl<T> ConfigurationStorage for Arc<T>
where
    T: ConfigurationStorage + ?Sized,
{
    fn read(&self) -> ConfigurationResult<Vec<u8>> {
        self.as_ref().read()
    }

    fn write_atomically(&self, contents: &[u8]) -> ConfigurationResult<()> {
        self.as_ref().write_atomically(contents)
    }

    fn read_configuration(&self) -> ConfigurationResult<Vec<u8>> {
        self.as_ref().read_configuration()
    }

    fn replace_atomically(&self, contents: &[u8]) -> ConfigurationResult<()> {
        self.as_ref().replace_atomically(contents)
    }
}
