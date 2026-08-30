use std::{fs, process::Command};

#[test]
fn release_configuration_separates_package_and_cli_tags() {
    let root = env!("CARGO_MANIFEST_DIR");
    let release_config = fs::read_to_string(format!("{root}/release-plz.toml"))
        .expect("release-plz configuration should be readable");
    let release_workflow = fs::read_to_string(format!("{root}/.github/workflows/release.yml"))
        .expect("release workflow should be readable");

    assert!(
        release_config
            .lines()
            .any(|line| line.trim() == "git_tag_name = \"gib-sdk-v{{version}}\"")
    );
    assert!(release_workflow.contains("BASELINE_TAG=\"gib-sdk-v0.0.45\""));
    assert!(release_workflow.contains("TAG=\"v${VERSION}\""));
    assert!(release_workflow.contains("PACKAGE_TAG=\"gib-sdk-v${VERSION}\""));
    assert!(release_workflow.contains("git push origin \"${TAG}\" \"${PACKAGE_TAG}\""));
}

#[test]
fn cli_reports_the_public_library_version_in_both_output_modes() {
    let interactive = Command::new(env!("CARGO_BIN_EXE_gib"))
        .arg("--version")
        .output()
        .expect("interactive version command should run");

    assert!(interactive.status.success());
    assert!(interactive.stderr.is_empty());
    assert_eq!(
        String::from_utf8(interactive.stdout).expect("version output should be UTF-8"),
        format!("gib {}\n", gib::VERSION)
    );

    let json = Command::new(env!("CARGO_BIN_EXE_gib"))
        .args(["--mode", "json", "--version"])
        .output()
        .expect("JSON version command should run");

    assert!(json.status.success());
    assert!(json.stderr.is_empty());
    let envelope: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("version output should be valid JSON");
    assert_eq!(envelope["type"], "version");
    assert_eq!(envelope["data"]["text"], format!("gib {}\n", gib::VERSION));
}
