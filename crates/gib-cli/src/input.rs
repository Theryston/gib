use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use gib::{
    ConfigurationOverrides, ConfigurationResolutionRequest, ConfigurationSelection,
    DEFAULT_SNAPSHOT_PAGE_SIZE, SnapshotCursor,
};
#[cfg(test)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::{fmt, fmt::Formatter};

#[derive(Debug, Parser)]
#[command(
    name = "gib",
    version,
    about = "⚡ Back up your files. Keep them in sync. Travel through their history."
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = OutputMode::Interactive,
        help = "Output mode: interactive or json."
    )]
    pub mode: OutputMode,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with = "no_config",
        help = "Use a specific local gib.toml file."
    )]
    pub config: Option<PathBuf>,
    #[arg(
        long = "no-config",
        global = true,
        action = clap::ArgAction::SetTrue,
        conflicts_with = "config",
        help = "Disable local gib.toml discovery."
    )]
    pub no_config: bool,
    #[arg(
        short = 's',
        long,
        global = true,
        value_name = "NAME",
        help = "Override repository.storage."
    )]
    pub storage: Option<String>,
    #[arg(
        short = 'k',
        long,
        global = true,
        value_name = "KEY",
        help = "Override repository.key."
    )]
    pub key: Option<String>,
    #[arg(
        short = 'r',
        long = "root-path",
        global = true,
        value_name = "PATH",
        help = "Override backup.root_path."
    )]
    pub root_path: Option<String>,
    #[arg(
        short = 'm',
        long,
        global = true,
        value_name = "MESSAGE",
        help = "Override backup.message."
    )]
    pub message: Option<String>,
    #[arg(
        short = 'c',
        long = "compress",
        visible_alias = "compression",
        global = true,
        value_name = "LEVEL",
        help = "Override backup.compress."
    )]
    pub compress: Option<i32>,
    #[arg(
        short = 'z',
        long = "chunk-size",
        global = true,
        value_name = "SIZE",
        help = "Override backup.chunk_size."
    )]
    pub chunk_size: Option<String>,
    #[arg(
        short = 'i',
        long,
        global = true,
        value_name = "N",
        help = "Override backup.concurrency."
    )]
    pub concurrency: Option<usize>,
    #[arg(
        long,
        global = true,
        action = clap::ArgAction::Append,
        value_name = "PATTERN",
        help = "Add a backup.ignore rule; may be repeated."
    )]
    pub ignore: Vec<String>,
    #[arg(
        long = "no-ignore-git",
        global = true,
        action = clap::ArgAction::SetTrue,
        help = "Include .git directories and files in Backup and Live captures."
    )]
    pub no_ignore_git: bool,
    #[arg(
        long = "live-message",
        global = true,
        value_name = "MESSAGE",
        help = "Override live.message."
    )]
    pub live_message: Option<String>,
    #[arg(
        long = "debounce-ms",
        global = true,
        value_name = "MILLISECONDS",
        help = "Override live.debounce_ms."
    )]
    pub debounce_ms: Option<u64>,
    #[arg(
        long = "poll-ms",
        global = true,
        value_name = "MILLISECONDS",
        help = "Override live.poll_ms."
    )]
    pub poll_ms: Option<u64>,
    #[arg(
        long = "target-path",
        global = true,
        value_name = "PATH",
        help = "Override restore.target_path."
    )]
    pub target_path: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputMode {
    Interactive,
    Json,
}

impl Cli {
    pub fn configuration_request(
        &self,
        starting_directory: impl AsRef<Path>,
    ) -> ConfigurationResolutionRequest {
        let selection = if self.no_config {
            ConfigurationSelection::disabled()
        } else if let Some(path) = &self.config {
            ConfigurationSelection::explicit(path)
        } else {
            ConfigurationSelection::discover()
        };

        let mut overrides = ConfigurationOverrides::new();
        if let Some(value) = &self.storage {
            overrides = overrides.with_repository_storage(value.clone());
        }
        if let Some(value) = &self.key {
            overrides = overrides.with_repository_key(value.clone());
        }
        if let Some(value) = &self.root_path {
            overrides = overrides.with_backup_root_path(value);
        }
        if let Some(value) = &self.message {
            overrides = overrides.with_backup_message(value.clone());
        }
        if let Some(value) = self.compress {
            overrides = overrides.with_backup_compress(value);
        }
        if let Some(value) = &self.chunk_size {
            overrides = overrides.with_backup_chunk_size(value.clone());
        }
        if let Some(value) = self.concurrency {
            overrides = overrides.with_backup_concurrency(value);
        }
        overrides = overrides.with_ignore_rules(self.ignore.clone());
        if self.no_ignore_git {
            overrides = overrides.with_no_ignore_git();
        }
        if let Some(value) = &self.live_message {
            overrides = overrides.with_live_message(value.clone());
        }
        if let Some(value) = self.debounce_ms {
            overrides = overrides.with_live_debounce_ms(value);
        }
        if let Some(value) = self.poll_ms {
            overrides = overrides.with_live_poll_ms(value);
        }
        if let Some(value) = &self.target_path {
            overrides = overrides.with_restore_target_path(value);
        }

        ConfigurationResolutionRequest::new(starting_directory)
            .with_selection(selection)
            .with_overrides(overrides)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Configure the global author identity.")]
    Config(ConfigRequest),
    #[command(about = "List snapshot history in deterministic newest-first order.")]
    Log(LogRequest),
    #[command(about = "Resolve a full ID, unique prefix, or the `latest` alias.")]
    Resolve(ResolveRequest),
    #[command(about = "Show the configured global author identity.")]
    Whoami(WhoamiRequest),
    #[command(about = "Manage named storage configurations.")]
    #[command(subcommand)]
    Storage(Box<StorageCommand>),
}

#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    #[command(about = "Add or explicitly replace a named storage configuration.")]
    Add(Box<StorageAddCommand>),
    #[command(about = "List named storage configurations.")]
    List(StorageListCommand),
    #[command(about = "Remove a named storage configuration without deleting its data.")]
    Remove(StorageRemoveCommand),
}

#[derive(Args)]
pub struct StorageAddCommand {
    #[arg(short = 'n', long, value_name = "NAME", help = "Unique storage name.")]
    pub name: Option<String>,
    #[arg(
        short = 't',
        long = "backend",
        visible_alias = "type",
        value_name = "BACKEND",
        help = "Backend type: local, s3, or webdav."
    )]
    pub backend: Option<String>,
    #[arg(
        short = 'p',
        long,
        visible_alias = "root",
        value_name = "PATH",
        help = "Local storage root."
    )]
    pub path: Option<PathBuf>,
    #[arg(long, value_name = "REGION", help = "S3 region.")]
    pub region: Option<String>,
    #[arg(short = 'b', long, value_name = "BUCKET", help = "S3 bucket.")]
    pub bucket: Option<String>,
    #[arg(
        short = 'a',
        long = "access-key",
        value_name = "ACCESS_KEY",
        help = "S3 access key."
    )]
    pub access_key: Option<String>,
    #[arg(
        long = "secret-key",
        value_name = "SECRET_KEY",
        help = "S3 secret key."
    )]
    pub secret_key: Option<String>,
    #[arg(
        long = "session-token",
        value_name = "SESSION_TOKEN",
        help = "Optional S3 session token."
    )]
    pub session_token: Option<String>,
    #[arg(
        short = 'e',
        long,
        value_name = "URL",
        help = "Optional S3-compatible endpoint."
    )]
    pub endpoint: Option<String>,
    #[arg(long, help = "Use S3 path-style addressing.")]
    pub force_path_style: bool,
    #[arg(long, value_name = "URL", help = "WebDAV collection URL.")]
    pub url: Option<String>,
    #[arg(long, value_name = "USERNAME", help = "WebDAV username.")]
    pub username: Option<String>,
    #[arg(long, value_name = "PASSWORD", help = "WebDAV password.")]
    pub password: Option<String>,
    #[arg(long, help = "Permit an explicitly insecure HTTP WebDAV URL.")]
    pub allow_insecure_http: bool,
    #[arg(
        long,
        visible_alias = "replace-existing",
        help = "Explicitly replace an existing name."
    )]
    pub replace: bool,
}

impl fmt::Debug for StorageAddCommand {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageAddCommand")
            .field("name", &self.name)
            .field("backend", &self.backend)
            .field("path", &self.path)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field(
                "access_key",
                &self.access_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "secret_key",
                &self.secret_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("endpoint", &self.endpoint)
            .field("force_path_style", &self.force_path_style)
            .field("url", &self.url)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("replace", &self.replace)
            .finish()
    }
}

#[derive(Debug, Args)]
pub struct StorageListCommand {
    #[arg(long, help = "Perform a read-only health check for every storage.")]
    pub check_health: bool,
}

#[derive(Debug, Args)]
pub struct StorageRemoveCommand {
    #[arg(
        short = 'n',
        long,
        value_name = "NAME",
        help = "Storage name to remove."
    )]
    pub name: Option<String>,
    #[arg(
        long = "yes",
        visible_alias = "force",
        help = "Confirm removal without an interactive prompt."
    )]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ConfigRequest {
    #[arg(
        long = "author",
        value_name = "AUTHOR",
        help = "Author identity in the form `Name <email>`."
    )]
    pub author: Option<String>,
}

#[derive(Debug, Args)]
pub struct WhoamiRequest {}

#[derive(Debug, Args)]
pub struct LogRequest {
    #[arg(
        value_name = "REPOSITORY",
        help = "The local repository root. Defaults to the current directory.",
        conflicts_with = "repository_option"
    )]
    pub repository: Option<PathBuf>,
    #[arg(
        long = "repository",
        visible_alias = "repo",
        value_name = "PATH",
        help = "The local repository root (the `--repo` alias is retained for scripts).",
        conflicts_with = "repository"
    )]
    pub repository_option: Option<PathBuf>,
    #[arg(
        long = "limit",
        short = 'n',
        default_value_t = DEFAULT_SNAPSHOT_PAGE_SIZE,
        value_name = "N",
        help = "The number of summaries requested from each SDK page."
    )]
    pub page_size: usize,
    #[arg(
        long,
        value_name = "CURSOR",
        help = "Continue after an SDK history cursor.",
        value_parser = parse_cursor
    )]
    pub after: Option<SnapshotCursor>,
}

impl LogRequest {
    pub fn repository_path(&self) -> &Path {
        self.repository_option
            .as_deref()
            .or(self.repository.as_deref())
            .unwrap_or(Path::new("."))
    }
}

#[derive(Debug, Args)]
pub struct ResolveRequest {
    #[arg(
        value_name = "REFERENCE",
        help = "A full snapshot ID, unique prefix, or `latest`."
    )]
    pub reference: String,
    #[arg(
        value_name = "REPOSITORY",
        help = "The local repository root. Defaults to the current directory.",
        conflicts_with = "repository_option"
    )]
    pub repository: Option<PathBuf>,
    #[arg(
        long = "repository",
        visible_alias = "repo",
        value_name = "PATH",
        help = "The local repository root (the `--repo` alias is retained for scripts).",
        conflicts_with = "repository"
    )]
    pub repository_option: Option<PathBuf>,
}

impl ResolveRequest {
    pub fn repository_path(&self) -> &Path {
        self.repository_option
            .as_deref()
            .or(self.repository.as_deref())
            .unwrap_or(Path::new("."))
    }
}

fn parse_cursor(value: &str) -> Result<SnapshotCursor, String> {
    SnapshotCursor::new(value.to_owned()).map_err(|error| error.to_string())
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
pub fn parse_from<I, T>(arguments: I) -> Result<Cli, clap::error::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(arguments)
}

pub fn print_help() {
    let mut command = Cli::command();
    let _ = command.print_help();
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn defaults_log_to_the_current_repository_and_sdk_page_size() {
        let cli = parse_from(arguments(&["gib", "log"])).expect("valid CLI input");
        assert_eq!(cli.mode, OutputMode::Interactive);
        let Some(Command::Log(request)) = cli.command else {
            panic!("expected log command");
        };
        assert_eq!(request.repository_path(), Path::new("."));
        assert_eq!(request.page_size, DEFAULT_SNAPSHOT_PAGE_SIZE);
        assert!(request.after.is_none());
    }

    #[test]
    fn parses_log_options_without_reimplementing_sdk_bounds() {
        let cli = parse_from(arguments(&[
            "gib",
            "log",
            "--repository",
            "/tmp/repository",
            "--limit",
            "7",
            "--after",
            "g:00000000000000000007:abc",
        ]))
        .expect("valid CLI input");
        let Some(Command::Log(request)) = cli.command else {
            panic!("expected log command");
        };
        assert_eq!(request.repository_path(), Path::new("/tmp/repository"));
        assert_eq!(request.page_size, 7);
        assert_eq!(
            request.after.as_ref().map(SnapshotCursor::as_str),
            Some("g:00000000000000000007:abc")
        );
    }

    #[test]
    fn resolve_requires_one_reference_and_accepts_an_optional_repository() {
        let cli = parse_from(arguments(&["gib", "resolve", "abc", "/tmp/repository"]))
            .expect("valid CLI input");
        let Some(Command::Resolve(request)) = cli.command else {
            panic!("expected resolve command");
        };
        assert_eq!(request.repository_path(), Path::new("/tmp/repository"));
        assert_eq!(request.reference, "abc");
        assert!(parse_from(arguments(&["gib", "resolve"])).is_err());
    }

    #[test]
    fn clap_reports_conflicting_repository_arguments() {
        let error = parse_from(arguments(&[
            "gib",
            "log",
            "./repository",
            "--repository",
            "./other",
        ]))
        .expect_err("repository arguments must conflict");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn clap_generates_help_for_commands() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("Usage:"));
        assert!(help.contains("log"));
        assert!(help.contains("resolve"));
        assert!(help.contains("config"));
        assert!(help.contains("whoami"));
    }

    #[test]
    fn parses_identity_commands_and_both_mode_positions() {
        let cli = parse_from(arguments(&[
            "gib",
            "--mode",
            "json",
            "config",
            "--author",
            "Jane Doe <jane@example.com>",
        ]))
        .expect("valid JSON config input");
        assert_eq!(cli.mode, OutputMode::Json);
        let Some(Command::Config(request)) = cli.command else {
            panic!("expected config command");
        };
        assert_eq!(
            request.author.as_deref(),
            Some("Jane Doe <jane@example.com>")
        );

        let cli = parse_from(arguments(&["gib", "whoami", "--mode", "interactive"]))
            .expect("valid interactive whoami input");
        assert_eq!(cli.mode, OutputMode::Interactive);
        assert!(matches!(cli.command, Some(Command::Whoami(_))));
    }

    #[test]
    fn parses_storage_backend_aliases_and_non_secret_argument_shapes() {
        let cli = parse_from(arguments(&[
            "gib",
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "remote",
            "--type",
            "s3",
            "--region",
            "us-east-1",
            "--bucket",
            "bucket",
            "--access-key",
            "access",
            "--secret-key",
            "top-secret",
            "--endpoint",
            "https://s3.example.test",
            "--replace-existing",
        ]))
        .expect("valid storage input");
        let Some(Command::Storage(command)) = cli.command else {
            panic!("expected storage command");
        };
        let StorageCommand::Add(request) = *command else {
            panic!("expected storage add command");
        };
        assert_eq!(request.backend.as_deref(), Some("s3"));
        assert_eq!(request.secret_key.as_deref(), Some("top-secret"));
        assert!(request.replace);
        let debug = format!("{request:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("top-secret"));
    }

    #[test]
    fn parses_global_configuration_selection_and_all_override_values() {
        let cli = parse_from(arguments(&[
            "gib",
            "--config",
            "/tmp/project/gib.toml",
            "--storage",
            "cli-storage",
            "--key",
            "cli-key",
            "--root-path",
            "source",
            "--message",
            "backup",
            "--compress",
            "22",
            "--chunk-size",
            "8 KiB",
            "--concurrency",
            "8",
            "--ignore",
            ".git",
            "--ignore",
            "target",
            "--no-ignore-git",
            "--live-message",
            "live",
            "--debounce-ms",
            "100",
            "--poll-ms",
            "200",
            "--target-path",
            "restore",
            "whoami",
        ]))
        .expect("valid configuration overrides");

        let request = cli.configuration_request(Path::new("/tmp/project"));
        assert!(request.selection().is_explicit());
        assert_eq!(
            request.selection().path(),
            Some(Path::new("/tmp/project/gib.toml"))
        );
        assert_eq!(
            request.overrides().repository_storage(),
            Some("cli-storage")
        );
        assert_eq!(request.overrides().repository_key(), Some("cli-key"));
        assert_eq!(
            request.overrides().backup_root_path(),
            Some(Path::new("source"))
        );
        assert_eq!(request.overrides().backup_message(), Some("backup"));
        assert_eq!(request.overrides().backup_compress(), Some(22));
        assert_eq!(request.overrides().backup_chunk_size(), Some("8 KiB"));
        assert_eq!(request.overrides().backup_concurrency(), Some(8));
        assert_eq!(
            request.overrides().backup_ignore_rules(),
            [String::from(".git"), String::from("target")].as_slice()
        );
        assert!(request.overrides().no_ignore_git());
        assert_eq!(request.overrides().live_message(), Some("live"));
        assert_eq!(request.overrides().live_debounce_ms(), Some(100));
        assert_eq!(request.overrides().live_poll_ms(), Some(200));
        assert_eq!(
            request.overrides().restore_target_path(),
            Some(Path::new("restore"))
        );
    }

    #[test]
    fn clap_rejects_config_and_no_config_together() {
        let error = parse_from(arguments(&[
            "gib",
            "--config",
            "/tmp/project/gib.toml",
            "--no-config",
            "whoami",
        ]))
        .expect_err("configuration selection flags must conflict");
        assert_eq!(error.exit_code(), 2);
    }
}
