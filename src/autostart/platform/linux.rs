use crate::autostart::model::AutostartJob;
use crate::autostart::platform::{PlatformStatus, remove_artifact, write_artifact};
use crate::autostart::registry::RegistryPaths;
use std::path::{Path, PathBuf};
use std::process::Command;

fn unit_name(job: &AutostartJob) -> String {
    format!("gib-live-{}.service", job.id)
}

fn unit_path(job: &AutostartJob) -> Result<PathBuf, String> {
    let home =
        dirs::home_dir().ok_or_else(|| "Failed to determine the home directory".to_string())?;
    Ok(home
        .join(".config")
        .join("systemd")
        .join("user")
        .join(unit_name(job)))
}

fn systemd_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            ' ' => escaped.push_str("\\x20"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            character => escaped.push(character),
        }
    }
    escaped
}

pub(crate) fn render_systemd_unit(job: &AutostartJob, executable: &Path) -> String {
    let executable = systemd_escape(&executable.to_string_lossy());
    let root = systemd_escape(&job.root_path);
    format!(
        "[Unit]\nDescription=GIB Live job {}\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={} --mode json autostart run {}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        job.id, root, executable, job.id
    )
}

fn run_systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|error| format!("Failed to start systemctl --user: {}", error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(crate) fn enable(
    _paths: &RegistryPaths,
    job: &AutostartJob,
    executable: &Path,
    start_now: bool,
) -> Result<(), String> {
    let path = unit_path(job)?;
    write_artifact(&path, &render_systemd_unit(job, executable))?;
    let name = unit_name(job);
    if let Err(error) = run_systemctl(&["--user", "daemon-reload"]) {
        let _ = remove_artifact(&path);
        return Err(error);
    }
    if let Err(error) = run_systemctl(&["--user", "enable", &name]) {
        let _ = remove_artifact(&path);
        return Err(error);
    }
    if start_now && let Err(error) = run_systemctl(&["--user", "start", &name]) {
        return Err(error);
    }
    Ok(())
}

pub(crate) fn disable(_paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    let name = unit_name(job);
    let _ = run_systemctl(&["--user", "disable", "--now", &name]);
    Ok(())
}

pub(crate) fn remove(_paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    let name = unit_name(job);
    let _ = run_systemctl(&["--user", "disable", "--now", &name]);
    let path = unit_path(job)?;
    remove_artifact(&path)?;
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(())
}

pub(crate) fn status(_paths: &RegistryPaths, job: &AutostartJob) -> PlatformStatus {
    let name = unit_name(job);
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", &name])
        .output()
        .is_ok_and(|output| output.status.success());
    let running = Command::new("systemctl")
        .args(["--user", "is-active", &name])
        .output()
        .map(|output| output.status.success())
        .ok();
    PlatformStatus {
        platform: "linux",
        enabled,
        running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::model::{AUTOSTART_JOB_VERSION, LiveJobOverrides, SecretReferences};

    #[test]
    fn renders_a_user_systemd_service_without_shell_interpolation() {
        let job = AutostartJob {
            version: AUTOSTART_JOB_VERSION,
            id: "job-1".to_string(),
            name: "code".to_string(),
            enabled: true,
            root_path: "/tmp/project with spaces".to_string(),
            config_path: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
            overrides: LiveJobOverrides::default(),
            secrets: SecretReferences::default(),
        };
        let unit = render_systemd_unit(&job, Path::new("/usr/local/bin/gib"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("WorkingDirectory=/tmp/project\\x20with\\x20spaces"));
        assert!(unit.contains("--mode json autostart run job-1"));
        assert!(!unit.contains("sh -c"));
    }
}
