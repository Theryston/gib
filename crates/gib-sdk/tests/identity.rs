use gib::{
    AuthorIdentity, ConfigurationError, ConfigurationResult, ConfigurationStorage, ErrorCode,
    LocalConfiguration, MemoryConfiguration, SdkError,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn local_identity_first_write_update_and_read_round_trip() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = TestDirectory::new();
    let configuration = LocalConfiguration::new(directory.path().join("config.msgpack"))?;
    let client = gib::Client::default();

    assert_eq!(
        client.read_identity(&configuration)?,
        None,
        "an absent file is not a configured identity"
    );
    assert_eq!(
        client.get_identity(&configuration),
        Err(SdkError::IdentityNotConfigured)
    );

    let first = client.set_identity(&configuration, "Jane Doe <jane@example.com>")?;
    assert_eq!(first.as_str(), "Jane Doe <jane@example.com>");
    let configuration_path = directory.path().join("config.msgpack");
    assert!(configuration_path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&configuration_path)?.permissions().mode() & 0o777,
            0o600
        );
    }

    let second = client.set_identity(&configuration, "O'Connor <oconnor@example.org>")?;
    assert_eq!(second.as_str(), "O'Connor <oconnor@example.org>");
    assert_eq!(client.get_identity(&configuration)?, second);
    assert_eq!(client.read_identity(&configuration)?, Some(second));
    Ok(())
}

#[test]
fn invalid_identity_is_rejected_before_any_write() -> Result<(), Box<dyn std::error::Error>> {
    let storage = MemoryConfiguration::new();
    let client = gib::Client::default();

    for value in [
        "Jane Doe jane@example.com",
        "Jane Doe <jane@example.com",
        "Jane Doe <jane@example>",
        "Jane Doe <jane@@example.com>",
        "Jane  Doe <jane@example.com>",
    ] {
        let error = client
            .set_identity(&storage, value)
            .expect_err("invalid identity must fail");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
        assert_eq!(storage.read_bytes(), Err(ConfigurationError::NotFound));
    }
    Ok(())
}

#[test]
fn an_injected_atomic_write_failure_preserves_the_previous_identity() {
    let memory = MemoryConfiguration::new();
    let client = gib::Client::default();
    client
        .set_identity(&memory, "Jane Doe <jane@example.com>")
        .expect("initial identity should be written");
    let previous_bytes = memory.read_bytes().expect("initial bytes should exist");

    let failing = FailingConfiguration::new(previous_bytes.clone());
    let error = client
        .set_identity(failing.clone(), "John Doe <john@example.com>")
        .expect_err("injected write failure should be returned");
    assert_eq!(
        error,
        SdkError::ConfigurationFailure {
            operation: "write_identity"
        }
    );
    assert_eq!(failing.read_bytes(), previous_bytes);
    assert_eq!(
        client
            .get_identity(failing)
            .expect("previous identity should still decode")
            .as_str(),
        "Jane Doe <jane@example.com>"
    );
}

#[test]
fn author_identity_exposes_structured_name_and_email_without_rewriting() {
    let identity = AuthorIdentity::new("Jane Q. Doe <jane+backup@example.co.uk>")
        .expect("identity should be valid");
    assert_eq!(identity.name(), "Jane Q. Doe");
    assert_eq!(identity.email(), "jane+backup@example.co.uk");
    assert_eq!(identity.as_str(), "Jane Q. Doe <jane+backup@example.co.uk>");
}

#[derive(Clone)]
struct FailingConfiguration {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl FailingConfiguration {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(bytes)),
        }
    }

    fn read_bytes(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .expect("test lock should not be poisoned")
            .clone()
    }
}

impl ConfigurationStorage for FailingConfiguration {
    fn read(&self) -> ConfigurationResult<Vec<u8>> {
        Ok(self.read_bytes())
    }

    fn write_atomically(&self, _contents: &[u8]) -> ConfigurationResult<()> {
        Err(ConfigurationError::Io)
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let suffix = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("gib-identity-test-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
