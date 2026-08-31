use std::fmt;

use toml::Value;

use crate::domain::CURRENT_CONFIGURATION_VERSION;

/// The maximum TOML configuration document accepted by the parser.
pub(crate) const MAX_CONFIGURATION_BYTES: usize = 64 * 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigurationDocumentErrorKind {
    Parse,
    MissingField,
    UnknownField,
    InvalidType,
    InvalidValue,
    UnsupportedVersion,
    InputTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationDocumentError {
    kind: ConfigurationDocumentErrorKind,
    field: Option<String>,
    reason: String,
    version: Option<u32>,
}

impl ConfigurationDocumentError {
    fn new(
        kind: ConfigurationDocumentErrorKind,
        field: Option<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            field,
            reason: reason.into(),
            version: None,
        }
    }

    fn unsupported_version(version: u32) -> Self {
        Self {
            kind: ConfigurationDocumentErrorKind::UnsupportedVersion,
            field: Some(String::from("version")),
            reason: format!(
                "supported configuration version is {}",
                CURRENT_CONFIGURATION_VERSION
            ),
            version: Some(version),
        }
    }

    pub(crate) fn invalid_encoding() -> Self {
        Self::new(
            ConfigurationDocumentErrorKind::Parse,
            None,
            "configuration document must be valid UTF-8",
        )
    }

    pub(crate) const fn kind(&self) -> ConfigurationDocumentErrorKind {
        self.kind
    }

    pub(crate) fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }

    pub(crate) const fn version(&self) -> Option<u32> {
        self.version
    }
}

impl fmt::Display for ConfigurationDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(field) = self.field() {
            write!(formatter, "{field}: {}", self.reason())
        } else {
            formatter.write_str(self.reason())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedConfiguration {
    pub(crate) version: u32,
    pub(crate) repository: PersistedRepositoryConfiguration,
    pub(crate) backup: PersistedBackupConfiguration,
    pub(crate) live: PersistedLiveConfiguration,
    pub(crate) restore: PersistedRestoreConfiguration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistedRepositoryConfiguration {
    pub(crate) storage: Option<String>,
    pub(crate) key: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistedBackupConfiguration {
    pub(crate) root_path: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) compress: Option<i32>,
    pub(crate) chunk_size: Option<String>,
    pub(crate) concurrency: Option<usize>,
    pub(crate) ignore: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistedLiveConfiguration {
    pub(crate) message: Option<String>,
    pub(crate) debounce_ms: Option<u64>,
    pub(crate) poll_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistedRestoreConfiguration {
    pub(crate) target_path: Option<String>,
}

pub(crate) fn parse_configuration_document(
    contents: &str,
) -> Result<PersistedConfiguration, ConfigurationDocumentError> {
    if contents.len() > MAX_CONFIGURATION_BYTES {
        return Err(ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::InputTooLarge,
            None,
            format!("configuration document must be at most {MAX_CONFIGURATION_BYTES} bytes"),
        ));
    }

    let document = contents.parse::<Value>().map_err(|error| {
        ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::Parse,
            None,
            error.to_string(),
        )
    })?;
    let root = document.as_table().ok_or_else(|| {
        ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::InvalidType,
            None,
            "configuration document must be a table",
        )
    })?;

    reject_unknown_fields(
        root,
        "",
        &["version", "repository", "backup", "live", "restore"],
    )?;

    let version_value = root.get("version").ok_or_else(|| {
        ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::MissingField,
            Some(String::from("version")),
            "field is required",
        )
    })?;
    let version = required_u32(version_value, "version")?;
    if version != CURRENT_CONFIGURATION_VERSION {
        return Err(ConfigurationDocumentError::unsupported_version(version));
    }

    let repository = parse_repository(root.get("repository"))?;
    let backup = parse_backup(root.get("backup"))?;
    let live = parse_live(root.get("live"))?;
    let restore = parse_restore(root.get("restore"))?;

    Ok(PersistedConfiguration {
        version,
        repository,
        backup,
        live,
        restore,
    })
}

fn parse_repository(
    value: Option<&Value>,
) -> Result<PersistedRepositoryConfiguration, ConfigurationDocumentError> {
    let Some(table) = optional_table(value, "repository")? else {
        return Ok(PersistedRepositoryConfiguration::default());
    };
    reject_unknown_fields(table, "repository", &["storage", "key"])?;
    Ok(PersistedRepositoryConfiguration {
        storage: optional_string(table, "storage", "repository.storage")?,
        key: optional_string(table, "key", "repository.key")?,
    })
}

fn parse_backup(
    value: Option<&Value>,
) -> Result<PersistedBackupConfiguration, ConfigurationDocumentError> {
    let Some(table) = optional_table(value, "backup")? else {
        return Ok(PersistedBackupConfiguration::default());
    };
    reject_unknown_fields(
        table,
        "backup",
        &[
            "root_path",
            "message",
            "compress",
            "chunk_size",
            "concurrency",
            "ignore",
        ],
    )?;
    Ok(PersistedBackupConfiguration {
        root_path: optional_string(table, "root_path", "backup.root_path")?,
        message: optional_string(table, "message", "backup.message")?,
        compress: optional_i32(table, "compress", "backup.compress")?,
        chunk_size: optional_string(table, "chunk_size", "backup.chunk_size")?,
        concurrency: optional_usize(table, "concurrency", "backup.concurrency")?,
        ignore: optional_string_array(table, "ignore", "backup.ignore")?,
    })
}

fn parse_live(
    value: Option<&Value>,
) -> Result<PersistedLiveConfiguration, ConfigurationDocumentError> {
    let Some(table) = optional_table(value, "live")? else {
        return Ok(PersistedLiveConfiguration::default());
    };
    reject_unknown_fields(table, "live", &["message", "debounce_ms", "poll_ms"])?;
    Ok(PersistedLiveConfiguration {
        message: optional_string(table, "message", "live.message")?,
        debounce_ms: optional_u64(table, "debounce_ms", "live.debounce_ms")?,
        poll_ms: optional_u64(table, "poll_ms", "live.poll_ms")?,
    })
}

fn parse_restore(
    value: Option<&Value>,
) -> Result<PersistedRestoreConfiguration, ConfigurationDocumentError> {
    let Some(table) = optional_table(value, "restore")? else {
        return Ok(PersistedRestoreConfiguration::default());
    };
    reject_unknown_fields(table, "restore", &["target_path"])?;
    Ok(PersistedRestoreConfiguration {
        target_path: optional_string(table, "target_path", "restore.target_path")?,
    })
}

fn reject_unknown_fields(
    table: &toml::map::Map<String, Value>,
    prefix: &str,
    allowed: &[&str],
) -> Result<(), ConfigurationDocumentError> {
    if let Some(key) = table
        .keys()
        .find(|key| !allowed.iter().any(|allowed_key| allowed_key == key))
    {
        let field = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        return Err(ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::UnknownField,
            Some(field),
            "field is not supported",
        ));
    }
    Ok(())
}

fn optional_table<'a>(
    value: Option<&'a Value>,
    field: &str,
) -> Result<Option<&'a toml::map::Map<String, Value>>, ConfigurationDocumentError> {
    match value {
        None => Ok(None),
        Some(Value::Table(table)) => Ok(Some(table)),
        Some(_) => Err(invalid_type(field, "a table")),
    }
}

fn optional_string(
    table: &toml::map::Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<Option<String>, ConfigurationDocumentError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid_type(field, "a string")),
    }
}

fn optional_string_array(
    table: &toml::map::Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<Vec<String>, ConfigurationDocumentError> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(invalid_type(field, "an array of strings"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| match value {
            Value::String(value) => Ok(value.clone()),
            _ => Err(invalid_type(&format!("{field}[{index}]"), "a string")),
        })
        .collect()
}

fn required_u32(value: &Value, field: &str) -> Result<u32, ConfigurationDocumentError> {
    let Some(value) = value.as_integer() else {
        return Err(invalid_type(field, "an integer"));
    };
    u32::try_from(value).map_err(|_| {
        ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::InvalidValue,
            Some(field.to_owned()),
            "must be a non-negative 32-bit integer",
        )
    })
}

fn optional_i32(
    table: &toml::map::Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<Option<i32>, ConfigurationDocumentError> {
    optional_integer(table, key, field, "a 32-bit integer", i32::try_from)
}

fn optional_usize(
    table: &toml::map::Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<Option<usize>, ConfigurationDocumentError> {
    optional_integer(table, key, field, "a non-negative integer", usize::try_from)
}

fn optional_u64(
    table: &toml::map::Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<Option<u64>, ConfigurationDocumentError> {
    optional_integer(table, key, field, "a non-negative integer", u64::try_from)
}

fn optional_integer<T, F>(
    table: &toml::map::Map<String, Value>,
    key: &str,
    field: &str,
    expected: &str,
    convert: F,
) -> Result<Option<T>, ConfigurationDocumentError>
where
    F: FnOnce(i64) -> Result<T, std::num::TryFromIntError>,
{
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(value) = value.as_integer() else {
        return Err(invalid_type(field, "an integer"));
    };
    convert(value).map(Some).map_err(|_| {
        ConfigurationDocumentError::new(
            ConfigurationDocumentErrorKind::InvalidValue,
            Some(field.to_owned()),
            format!("must be {expected}"),
        )
    })
}

fn invalid_type(field: &str, expected: &str) -> ConfigurationDocumentError {
    ConfigurationDocumentError::new(
        ConfigurationDocumentErrorKind::InvalidType,
        Some(field.to_owned()),
        format!("must be {expected}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_minimal_versioned_document() {
        let document = parse_configuration_document("version = 1\n")
            .expect("minimal configuration should parse");
        assert_eq!(document.version, CURRENT_CONFIGURATION_VERSION);
        assert_eq!(
            document,
            PersistedConfiguration {
                version: 1,
                repository: PersistedRepositoryConfiguration::default(),
                backup: PersistedBackupConfiguration::default(),
                live: PersistedLiveConfiguration::default(),
                restore: PersistedRestoreConfiguration::default(),
            }
        );
    }

    #[test]
    fn rejects_missing_version_and_unknown_fields_with_context() {
        let missing = parse_configuration_document("[backup]\nmessage = \"x\"\n")
            .expect_err("version should be required");
        assert_eq!(missing.kind(), ConfigurationDocumentErrorKind::MissingField);
        assert_eq!(missing.field(), Some("version"));

        let unknown = parse_configuration_document("version = 1\n[backup]\nother = true\n")
            .expect_err("unknown fields should be rejected");
        assert_eq!(unknown.kind(), ConfigurationDocumentErrorKind::UnknownField);
        assert_eq!(unknown.field(), Some("backup.other"));
    }

    #[test]
    fn rejects_wrong_types_and_unsupported_versions() {
        let wrong_type = parse_configuration_document("version = \"1\"\n")
            .expect_err("version type should be checked");
        assert_eq!(
            wrong_type.kind(),
            ConfigurationDocumentErrorKind::InvalidType
        );
        assert_eq!(wrong_type.field(), Some("version"));

        let unsupported = parse_configuration_document("version = 2\n")
            .expect_err("future versions should be rejected");
        assert_eq!(
            unsupported.kind(),
            ConfigurationDocumentErrorKind::UnsupportedVersion
        );
        assert_eq!(unsupported.field(), Some("version"));
        assert_eq!(unsupported.version(), Some(2));
    }

    #[test]
    fn parses_all_persisted_fields_without_applying_domain_defaults() {
        let document = parse_configuration_document(
            r#"
version = 1

[repository]
storage = "backup-store"
key = "project.v1"

[backup]
root_path = "source"
message = "backup"
compress = 22
chunk_size = "5 MiB"
concurrency = 16
ignore = ["target", "dist"]

[live]
message = "live"
debounce_ms = 1
poll_ms = 2

[restore]
target_path = "restore"
"#,
        )
        .expect("complete configuration should parse");

        assert_eq!(document.repository.storage.as_deref(), Some("backup-store"));
        assert_eq!(document.repository.key.as_deref(), Some("project.v1"));
        assert_eq!(document.backup.root_path.as_deref(), Some("source"));
        assert_eq!(document.backup.message.as_deref(), Some("backup"));
        assert_eq!(document.backup.compress, Some(22));
        assert_eq!(document.backup.chunk_size.as_deref(), Some("5 MiB"));
        assert_eq!(document.backup.concurrency, Some(16));
        assert_eq!(document.backup.ignore, ["target", "dist"]);
        assert_eq!(document.live.message.as_deref(), Some("live"));
        assert_eq!(document.live.debounce_ms, Some(1));
        assert_eq!(document.live.poll_ms, Some(2));
        assert_eq!(document.restore.target_path.as_deref(), Some("restore"));
    }

    #[test]
    fn rejects_unknown_sections_and_every_wrong_section_type() {
        for contents in [
            "version = 1\nunknown = true\n",
            "version = 1\n[repository]\nunknown = true\n",
            "version = 1\n[backup]\nunknown = true\n",
            "version = 1\n[live]\nunknown = true\n",
            "version = 1\n[restore]\nunknown = true\n",
        ] {
            let error = parse_configuration_document(contents)
                .expect_err("unknown configuration fields should fail");
            assert_eq!(error.kind(), ConfigurationDocumentErrorKind::UnknownField);
        }

        for (contents, field) in [
            ("version = 1\nrepository = []\n", "repository"),
            ("version = 1\nbackup = []\n", "backup"),
            ("version = 1\nlive = []\n", "live"),
            ("version = 1\nrestore = []\n", "restore"),
        ] {
            let error = parse_configuration_document(contents)
                .expect_err("configuration sections must be tables");
            assert_eq!(error.kind(), ConfigurationDocumentErrorKind::InvalidType);
            assert_eq!(error.field(), Some(field));
        }
    }

    #[test]
    fn rejects_wrong_types_for_each_optional_field() {
        for (contents, field) in [
            (
                "version = 1\n[repository]\nstorage = 1\n",
                "repository.storage",
            ),
            ("version = 1\n[repository]\nkey = 1\n", "repository.key"),
            ("version = 1\n[backup]\nroot_path = 1\n", "backup.root_path"),
            ("version = 1\n[backup]\nmessage = 1\n", "backup.message"),
            (
                "version = 1\n[backup]\ncompress = \"3\"\n",
                "backup.compress",
            ),
            (
                "version = 1\n[backup]\nchunk_size = 1\n",
                "backup.chunk_size",
            ),
            (
                "version = 1\n[backup]\nconcurrency = \"8\"\n",
                "backup.concurrency",
            ),
            (
                "version = 1\n[backup]\nignore = \"dist\"\n",
                "backup.ignore",
            ),
            ("version = 1\n[live]\nmessage = 1\n", "live.message"),
            (
                "version = 1\n[live]\ndebounce_ms = \"1\"\n",
                "live.debounce_ms",
            ),
            ("version = 1\n[live]\npoll_ms = \"2\"\n", "live.poll_ms"),
            (
                "version = 1\n[restore]\ntarget_path = 1\n",
                "restore.target_path",
            ),
        ] {
            let error =
                parse_configuration_document(contents).expect_err("field type should be rejected");
            assert_eq!(error.kind(), ConfigurationDocumentErrorKind::InvalidType);
            assert_eq!(error.field(), Some(field));
        }
    }
}
