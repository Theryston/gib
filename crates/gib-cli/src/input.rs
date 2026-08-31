use clap::{Args, CommandFactory, Parser, Subcommand};
use gib::{DEFAULT_SNAPSHOT_PAGE_SIZE, SnapshotCursor};
#[cfg(test)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "gib",
    version,
    about = "⚡ Back up your files. Keep them in sync. Travel through their history."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "List snapshot history in deterministic newest-first order.")]
    Log(LogRequest),
    #[command(about = "Resolve a full ID, unique prefix, or the `latest` alias.")]
    Resolve(ResolveRequest),
}

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
    }
}
