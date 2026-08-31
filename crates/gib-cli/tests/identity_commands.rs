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

#[test]
fn json_config_selection_reports_explicit_and_disabled_sources() {
    let directory = TestDirectory::new();
    let parent = directory.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).expect("test tree should be created");
    let parent_config = parent.join("gib.toml");
    fs::write(
        &parent_config,
        "version = 1\n[backup]\nmessage = \"parent\"\n",
    )
    .expect("parent config should be written");
    fs::write(
        child.join("gib.toml"),
        "version = 1\n[backup]\nmessage = \"child\"\n",
    )
    .expect("child config should be written");

    let explicit_arguments = vec![
        String::from("--mode"),
        String::from("json"),
        String::from("--config"),
        parent_config.display().to_string(),
        String::from("config"),
        String::from("--author"),
        String::from("Jane Doe <jane@example.com>"),
    ];
    let explicit = run_in(child.as_path(), &explicit_arguments);
    assert!(explicit.status.success(), "{explicit:?}");
    let explicit_lines = parse_json_lines(&explicit.stdout);
    assert_eq!(explicit_lines[0]["type"], "config");
    assert_eq!(explicit_lines[0]["data"]["loaded"], true);
    assert_eq!(explicit_lines[0]["data"]["source"], "explicit");
    assert_eq!(
        explicit_lines[0]["data"]["path"],
        fs::canonicalize(&parent_config)
            .expect("config path should canonicalize")
            .display()
            .to_string()
    );
    assert_eq!(explicit_lines[1]["type"], "output");

    let disabled_arguments = vec![
        String::from("--mode"),
        String::from("json"),
        String::from("--no-config"),
        String::from("config"),
        String::from("--author"),
        String::from("Jane Doe <jane@example.com>"),
    ];
    let disabled = run_in(child.as_path(), &disabled_arguments);
    assert!(disabled.status.success(), "{disabled:?}");
    let disabled_lines = parse_json_lines(&disabled.stdout);
    assert_eq!(disabled_lines[0]["type"], "config");
    assert_eq!(disabled_lines[0]["data"]["loaded"], false);
    assert_eq!(disabled_lines[0]["data"]["source"], "disabled");
    assert!(disabled_lines[0]["data"]["path"].is_null());
    assert_eq!(disabled_lines[1]["type"], "output");
}

#[test]
fn json_discovery_reports_the_nearest_configuration_file() {
    let directory = TestDirectory::new();
    let parent = directory.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).expect("test tree should be created");
    fs::write(
        parent.join("gib.toml"),
        "version = 1\n[backup]\nmessage = \"parent\"\n",
    )
    .expect("parent config should be written");
    let child_config = child.join("gib.toml");
    fs::write(
        &child_config,
        "version = 1\n[backup]\nmessage = \"child\"\n",
    )
    .expect("child config should be written");

    let arguments = vec![
        String::from("--mode"),
        String::from("json"),
        String::from("config"),
        String::from("--author"),
        String::from("Jane Doe <jane@example.com>"),
    ];
    let output = run_in(child.as_path(), &arguments);
    assert!(output.status.success(), "{output:?}");
    let lines = parse_json_lines(&output.stdout);
    assert_eq!(lines[0]["type"], "config");
    assert_eq!(
        lines[0]["data"]["path"],
        fs::canonicalize(child_config)
            .expect("config path should canonicalize")
            .display()
            .to_string()
    );
}

fn run(home: &Path, arguments: &[&str]) -> OutputText {
    let arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    run_in(home, &arguments)
}

fn run_in(directory: &Path, arguments: &[String]) -> OutputText {
    let output = Command::new(env!("CARGO_BIN_EXE_gib"))
        .args(arguments)
        .current_dir(directory)
        .env("HOME", directory)
        .env("USERPROFILE", directory)
        .output()
        .expect("gib binary should run");
    OutputText::from(output)
}

fn parse_single_json(value: &str) -> Value {
    let lines = value.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "expected one JSON line, got {value:?}");
    serde_json::from_str(lines[0]).expect("output should be valid JSON")
}

fn parse_json_lines(value: &str) -> Vec<Value> {
    value
        .lines()
        .map(|line| serde_json::from_str(line).expect("output should be valid JSON"))
        .collect()
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
