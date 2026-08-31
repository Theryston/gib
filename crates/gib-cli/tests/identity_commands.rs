use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn interactive_config_and_whoami_share_the_configured_identity_result() {
    let directory = TestDirectory::new();
    let configured = run(
        directory.path(),
        &["config", "--author", "Jane Doe <jane@example.com>"],
    );
    assert!(configured.status.success(), "{configured:?}");
    assert!(
        configured
            .stdout
            .contains("Configured author: Jane Doe <jane@example.com>")
    );
    assert!(configured.stderr.is_empty());

    let whoami = run(directory.path(), &["whoami"]);
    assert!(whoami.status.success(), "{whoami:?}");
    assert!(
        whoami
            .stdout
            .contains("You are: Jane Doe <jane@example.com>")
    );
    assert!(whoami.stderr.is_empty());
}

#[test]
fn json_config_and_whoami_emit_only_the_author_result() {
    let directory = TestDirectory::new();
    let configured = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "config",
            "--author",
            "Jane Doe <jane@example.com>",
        ],
    );
    assert!(configured.status.success(), "{configured:?}");
    let configured_json = parse_single_json(&configured.stdout);
    assert_eq!(configured_json["type"], "output");
    assert_eq!(
        configured_json["data"]["author"],
        "Jane Doe <jane@example.com>"
    );
    assert!(
        configured_json["data"]
            .as_object()
            .is_some_and(|data| data.len() == 1)
    );
    assert!(!configured.stdout.contains("secret"));
    assert!(configured.stderr.is_empty());

    let whoami = run(directory.path(), &["whoami", "--mode", "json"]);
    assert!(whoami.status.success(), "{whoami:?}");
    let whoami_json = parse_single_json(&whoami.stdout);
    assert_eq!(whoami_json, configured_json);
    assert!(whoami.stderr.is_empty());
}

#[test]
fn malformed_config_attempts_fail_without_replacing_the_previous_identity() {
    let directory = TestDirectory::new();
    let configured = run(
        directory.path(),
        &["config", "--author", "Jane Doe <jane@example.com>"],
    );
    assert!(configured.status.success(), "{configured:?}");

    let invalid = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "config",
            "--author",
            "Jane Doe jane@example.com",
        ],
    );
    assert_eq!(invalid.status.code(), Some(2));
    let invalid_json = parse_single_json(&invalid.stderr);
    assert_eq!(invalid_json["type"], "error");
    assert_eq!(invalid_json["data"]["code"], "invalid_request");

    let whoami = run(directory.path(), &["whoami"]);
    assert!(whoami.status.success(), "{whoami:?}");
    assert!(whoami.stdout.contains("Jane Doe <jane@example.com>"));
}

#[test]
fn json_whoami_reports_a_typed_not_configured_error() {
    let directory = TestDirectory::new();
    let whoami = run(directory.path(), &["whoami", "--mode", "json"]);
    assert_eq!(whoami.status.code(), Some(2));
    assert!(whoami.stdout.is_empty());
    let error = parse_single_json(&whoami.stderr);
    assert_eq!(error["type"], "error");
    assert_eq!(error["data"]["code"], "identity_not_configured");
    assert!(!whoami.stderr.contains("config.msgpack"));
}

fn run(home: &Path, arguments: &[&str]) -> OutputText {
    let output = Command::new(env!("CARGO_BIN_EXE_gib"))
        .args(arguments)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("gib binary should run");
    OutputText::from(output)
}

fn parse_single_json(value: &str) -> Value {
    let lines = value.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "expected one JSON line, got {value:?}");
    serde_json::from_str(lines[0]).expect("output should be valid JSON")
}

struct OutputText {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl From<Output> for OutputText {
    fn from(output: Output) -> Self {
        Self {
            status: output.status,
            stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
        }
    }
}

impl std::fmt::Debug for OutputText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputText")
            .field("status", &self.status)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let suffix = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gib-cli-identity-test-{}-{suffix}",
            std::process::id()
        ));
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
