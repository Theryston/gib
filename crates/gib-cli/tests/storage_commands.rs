use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gib-cli-storage-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn local_storage_has_json_add_list_remove_flows() {
    let directory = TestDirectory::new();
    let root = directory.path().join("local-data");
    let root_text = root.display().to_string();
    let add = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "local",
            "--backend",
            "local",
            "--path",
            &root_text,
        ],
    );
    assert!(add.status.success(), "{add:?}");
    let add_json = single_json(&add.stdout);
    assert_eq!(add_json["type"], "storage");
    assert_eq!(add_json["data"]["storage"]["name"], "local");
    assert_eq!(add_json["data"]["storage"]["backend"], "local");
    assert_eq!(add_json["data"]["storage"]["path"], root_text);
    assert!(add.stderr.is_empty());

    let list = run(directory.path(), &["--mode", "json", "storage", "list"]);
    assert!(list.status.success(), "{list:?}");
    let list_json = single_json(&list.stdout);
    assert_eq!(list_json["data"]["storages"][0]["name"], "local");
    assert_eq!(list_json["data"]["storages"][0]["health"], "not_checked");

    fs::write(root.join("sentinel"), b"preserve me").expect("sentinel should be written");
    let remove = run(
        directory.path(),
        &[
            "--mode", "json", "storage", "remove", "--name", "local", "--yes",
        ],
    );
    assert!(remove.status.success(), "{remove:?}");
    let remove_json = single_json(&remove.stdout);
    assert_eq!(remove_json["data"]["action"], "removed");
    assert_eq!(remove_json["data"]["repository_data_preserved"], true);
    assert_eq!(
        fs::read(root.join("sentinel")).expect("sentinel should remain"),
        b"preserve me"
    );
}

#[test]
fn local_storage_can_be_added_interactively_with_prompts() {
    let directory = TestDirectory::new();
    let root = directory.path().join("interactive-data");
    let input = format!("interactive\nlocal\n{}\ny\n", root.display());
    let output = run_with_stdin(directory.path(), &["storage", "add"], input.as_bytes());
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains('\x1b'));
    assert!(stdout.contains("What kind of storage is this?"));
    assert!(stdout.contains("Review before saving"));
    assert!(stdout.contains("Added storage 'interactive' (local)"));
    assert!(output.stderr.is_empty());

    let listed = run(directory.path(), &["storage", "list"]);
    assert!(listed.status.success(), "{listed:?}");
    let listed_stdout = String::from_utf8_lossy(&listed.stdout);
    let header = listed_stdout
        .lines()
        .find(|line| line.contains("NAME") && line.contains("CREDENTIALS"))
        .expect("storage table header should be rendered");
    let row = listed_stdout
        .lines()
        .find(|line| line.contains("interactive") && line.contains("not checked"))
        .expect("storage table row should be rendered");
    for (header_value, row_value) in [
        ("NAME", "interactive"),
        ("BACKEND", "local"),
        ("HEALTH", "not checked"),
        ("CREDENTIALS", "not required"),
    ] {
        assert_eq!(
            visual_position(header, header_value),
            visual_position(row, row_value),
            "column {header_value} should align"
        );
    }
}

fn visual_position(line: &str, value: &str) -> Option<usize> {
    let start = line.find(value)?;
    Some(dialoguer::console::measure_text_width(&line[..start]))
}

#[test]
fn duplicate_names_fail_in_json_and_replacement_is_explicit() {
    let directory = TestDirectory::new();
    let first_root = directory.path().join("first");
    let second_root = directory.path().join("second");
    let first_text = first_root.display().to_string();
    let second_text = second_root.display().to_string();
    let first = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "same",
            "--backend",
            "local",
            "--path",
            &first_text,
        ],
    );
    assert!(first.status.success(), "{first:?}");

    let duplicate = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "same",
            "--backend",
            "local",
            "--path",
            &second_text,
        ],
    );
    assert_eq!(duplicate.status.code(), Some(3), "{duplicate:?}");
    let duplicate_json = single_json(&duplicate.stderr);
    assert_eq!(duplicate_json["data"]["code"], "storage_already_exists");

    let replacement = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "same",
            "--backend",
            "local",
            "--path",
            &second_text,
            "--replace",
        ],
    );
    assert!(replacement.status.success(), "{replacement:?}");
    let replacement_json = single_json(&replacement.stdout);
    assert_eq!(replacement_json["data"]["replaced_existing"], true);
    assert_eq!(replacement_json["data"]["storage"]["path"], second_text);
}

#[test]
fn json_storage_add_never_prompts_for_missing_fields() {
    let directory = TestDirectory::new();
    let output = run(
        directory.path(),
        &["--mode", "json", "storage", "add", "--name", "only-name"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty());
    let error = single_json(&output.stderr);
    assert_eq!(error["data"]["code"], "invalid_request");
    assert_eq!(error["data"]["field"], "backend");
}

#[test]
fn remote_backend_argument_sets_fail_safely_without_echoing_secrets() {
    let directory = TestDirectory::new();
    let s3 = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "s3",
            "--backend",
            "s3",
            "--region",
            "us-east-1",
            "--bucket",
            "bucket",
            "--access-key",
            "visible-access",
            "--secret-key",
            "hidden-secret",
            "--endpoint",
            "http://127.0.0.1:1",
            "--force-path-style",
        ],
    );
    assert!(!s3.status.success(), "{s3:?}");
    assert!(!String::from_utf8_lossy(&s3.stdout).contains("hidden-secret"));
    assert!(!String::from_utf8_lossy(&s3.stderr).contains("hidden-secret"));
    assert_eq!(
        single_json(&s3.stderr)["data"]["code"],
        "storage_connectivity_failure"
    );
    let s3_interactive = run(
        directory.path(),
        &[
            "storage",
            "add",
            "--name",
            "s3-interactive",
            "--backend",
            "s3",
            "--region",
            "us-east-1",
            "--bucket",
            "bucket",
            "--access-key",
            "visible-access",
            "--secret-key",
            "hidden-secret",
            "--endpoint",
            "http://127.0.0.1:1",
            "--force-path-style",
        ],
    );
    assert!(!s3_interactive.status.success(), "{s3_interactive:?}");
    assert!(!String::from_utf8_lossy(&s3_interactive.stdout).contains("hidden-secret"));
    assert!(!String::from_utf8_lossy(&s3_interactive.stderr).contains("hidden-secret"));

    let webdav = run(
        directory.path(),
        &[
            "--mode",
            "json",
            "storage",
            "add",
            "--name",
            "webdav",
            "--backend",
            "webdav",
            "--url",
            "http://127.0.0.1:1/dav",
            "--username",
            "visible-user",
            "--password",
            "hidden-password",
            "--allow-insecure-http",
        ],
    );
    assert!(!webdav.status.success(), "{webdav:?}");
    assert!(!String::from_utf8_lossy(&webdav.stdout).contains("hidden-password"));
    assert!(!String::from_utf8_lossy(&webdav.stderr).contains("hidden-password"));
    assert_eq!(
        single_json(&webdav.stderr)["data"]["code"],
        "storage_connectivity_failure"
    );
    let webdav_interactive = run(
        directory.path(),
        &[
            "storage",
            "add",
            "--name",
            "webdav-interactive",
            "--backend",
            "webdav",
            "--url",
            "http://127.0.0.1:1/dav",
            "--username",
            "visible-user",
            "--password",
            "hidden-password",
            "--allow-insecure-http",
        ],
    );
    assert!(
        !webdav_interactive.status.success(),
        "{webdav_interactive:?}"
    );
    assert!(!String::from_utf8_lossy(&webdav_interactive.stdout).contains("hidden-password"));
    assert!(!String::from_utf8_lossy(&webdav_interactive.stderr).contains("hidden-password"));
}

fn run(home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gib"))
        .args(arguments)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("gib should run")
}

fn run_with_stdin(home: &Path, arguments: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gib"))
        .args(arguments)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gib should start");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(input)
        .expect("test input should be written");
    child.wait_with_output().expect("gib should finish")
}

fn single_json(bytes: &[u8]) -> Value {
    let text = std::str::from_utf8(bytes).expect("output should be UTF-8");
    assert!(!text.contains('\x1b'));
    let mut lines = text.lines();
    let value = serde_json::from_str(lines.next().expect("one JSON line should be emitted"))
        .expect("output should be JSON");
    assert!(
        lines.next().is_none(),
        "expected one JSON line, got {text:?}"
    );
    value
}
