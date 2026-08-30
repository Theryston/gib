//! CLI-owned operating-system integration for autostart jobs.
//!
//! The library persists and runs jobs without invoking platform commands. The
//! binary adapter owns service-manager artifacts and commands so embedding the
//! library cannot unexpectedly modify the user's login services.

use gib::api::{AutostartJob, ErrorCode, Gib, GibError};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PlatformStatus {
    pub(crate) platform: String,
    pub(crate) enabled: bool,
    pub(crate) running: Option<bool>,
}

pub(crate) fn enable(client: &Gib, job: &AutostartJob, start_now: bool) -> Result<(), GibError> {
    let executable = std::env::current_exe().map_err(|error| {
        GibError::new(
            ErrorCode::Io,
            format!("Failed to determine the GIB executable path: {error}"),
        )
    })?;
    native::enable(client, job, &executable, start_now)
}

pub(crate) fn disable(client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
    native::disable(client, job)
}

pub(crate) fn remove(client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
    native::remove(client, job)
}

pub(crate) fn status(client: &Gib, job: &AutostartJob) -> PlatformStatus {
    native::status(client, job)
}

fn write_artifact(path: &Path, contents: &str) -> Result<(), GibError> {
    let parent = path.parent().ok_or_else(|| {
        GibError::new(
            ErrorCode::Io,
            "The autostart platform artifact has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        GibError::new(
            ErrorCode::Io,
            format!("Failed to create platform artifact directory: {error}"),
        )
    })?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        std::process::id()
    ));
    std::fs::write(&temporary, contents).map_err(|error| {
        GibError::new(
            ErrorCode::Io,
            format!("Failed to write platform artifact: {error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                GibError::new(
                    ErrorCode::Io,
                    format!("Failed to protect platform artifact: {error}"),
                )
            },
        )?;
    }
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| {
            GibError::new(
                ErrorCode::Io,
                format!("Failed to replace platform artifact: {error}"),
            )
        })?;
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        GibError::new(
            ErrorCode::Io,
            format!("Failed to publish platform artifact: {error}"),
        )
    })
}

fn remove_artifact(path: &Path) -> Result<(), GibError> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| {
            GibError::new(
                ErrorCode::Io,
                format!("Failed to remove platform artifact: {error}"),
            )
        })?;
    }
    Ok(())
}

fn run_command(program: &str, args: &[&str]) -> Result<(), GibError> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|error| {
            GibError::new(
                ErrorCode::Unsupported,
                format!("Failed to start {program}: {error}"),
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(GibError::new(
        ErrorCode::Unsupported,
        if message.is_empty() {
            format!("{program} failed")
        } else {
            format!("{program} failed: {message}")
        },
    ))
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;

    pub(crate) fn platform_name() -> &'static str {
        "linux"
    }

    fn unit_name(job: &AutostartJob) -> String {
        format!("gib-live-{}.service", job.id)
    }

    fn unit_path(job: &AutostartJob) -> Result<PathBuf, GibError> {
        let home = dirs::home_dir().ok_or_else(|| {
            GibError::new(
                ErrorCode::ConfigurationNotFound,
                "Failed to determine the home directory",
            )
        })?;
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

    fn render_unit(job: &AutostartJob, executable: &Path) -> String {
        format!(
            "[Unit]\nDescription=GIB Live job {}\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={} --mode json autostart run {}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            job.id,
            systemd_escape(&job.root_path.to_string_lossy()),
            systemd_escape(&executable.to_string_lossy()),
            job.id,
        )
    }

    pub(crate) fn enable(
        _client: &Gib,
        job: &AutostartJob,
        executable: &Path,
        start_now: bool,
    ) -> Result<(), GibError> {
        let path = unit_path(job)?;
        write_artifact(&path, &render_unit(job, executable))?;
        let name = unit_name(job);
        if let Err(error) = run_command("systemctl", &["--user", "daemon-reload"]) {
            let _ = remove_artifact(&path);
            return Err(error);
        }
        if let Err(error) = run_command("systemctl", &["--user", "enable", &name]) {
            let _ = remove_artifact(&path);
            return Err(error);
        }
        if start_now {
            run_command("systemctl", &["--user", "start", &name])?;
        }
        Ok(())
    }

    pub(crate) fn disable(_client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
        let name = unit_name(job);
        let _ = run_command("systemctl", &["--user", "disable", "--now", &name]);
        Ok(())
    }

    pub(crate) fn remove(_client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
        let name = unit_name(job);
        let _ = run_command("systemctl", &["--user", "disable", "--now", &name]);
        remove_artifact(&unit_path(job)?)?;
        let _ = run_command("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }

    pub(crate) fn status(_client: &Gib, job: &AutostartJob) -> PlatformStatus {
        let name = unit_name(job);
        let enabled = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", &name])
            .output()
            .is_ok_and(|output| output.status.success());
        let running = std::process::Command::new("systemctl")
            .args(["--user", "is-active", &name])
            .output()
            .map(|output| output.status.success())
            .ok();
        PlatformStatus {
            platform: platform_name().to_string(),
            enabled,
            running,
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;

    pub(crate) fn platform_name() -> &'static str {
        "macos"
    }

    fn label(job: &AutostartJob) -> String {
        format!("org.trygib.live.{}", job.id)
    }

    fn plist_path(job: &AutostartJob) -> Result<PathBuf, GibError> {
        let home = dirs::home_dir().ok_or_else(|| {
            GibError::new(
                ErrorCode::ConfigurationNotFound,
                "Failed to determine the home directory",
            )
        })?;
        Ok(home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", label(job))))
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn render_agent(job: &AutostartJob, executable: &Path) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>--mode</string>\n    <string>json</string>\n    <string>autostart</string>\n    <string>run</string>\n    <string>{}</string>\n  </array>\n  <key>WorkingDirectory</key>\n  <string>{}</string>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ThrottleInterval</key>\n  <integer>5</integer>\n</dict>\n</plist>\n",
            xml_escape(&label(job)),
            xml_escape(&executable.to_string_lossy()),
            xml_escape(&job.id),
            xml_escape(&job.root_path.to_string_lossy()),
        )
    }

    fn uid() -> Result<String, GibError> {
        let output = std::process::Command::new("id")
            .arg("-u")
            .output()
            .map_err(|error| GibError::new(ErrorCode::Unsupported, error.to_string()))?;
        if !output.status.success() {
            return Err(GibError::new(
                ErrorCode::Unsupported,
                "The macOS user ID command failed",
            ));
        }
        String::from_utf8(output.stdout)
            .map(|value| value.trim().to_string())
            .map_err(|_| {
                GibError::new(
                    ErrorCode::Unsupported,
                    "The macOS user ID was not valid UTF-8",
                )
            })
    }

    pub(crate) fn enable(
        _client: &Gib,
        job: &AutostartJob,
        executable: &Path,
        start_now: bool,
    ) -> Result<(), GibError> {
        let path = plist_path(job)?;
        let path_string = path.to_string_lossy().to_string();
        write_artifact(&path, &render_agent(job, executable))?;
        if !start_now {
            return Ok(());
        }
        let target = format!("gui/{}", uid()?);
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", target.as_str(), path_string.as_str()])
            .output();
        run_command(
            "launchctl",
            &["bootstrap", target.as_str(), path_string.as_str()],
        )?;
        let service = format!("{target}/{}", label(job));
        run_command("launchctl", &["kickstart", "-k", &service])
    }

    pub(crate) fn disable(_client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
        let path = plist_path(job)?;
        let path_string = path.to_string_lossy().to_string();
        if let Ok(user_id) = uid() {
            let target = format!("gui/{user_id}");
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", target.as_str(), path_string.as_str()])
                .output();
        }
        remove_artifact(&path)
    }

    pub(crate) fn remove(client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
        disable(client, job)
    }

    pub(crate) fn status(_client: &Gib, job: &AutostartJob) -> PlatformStatus {
        let running = uid().ok().and_then(|user_id| {
            let service = format!("gui/{user_id}/{}", label(job));
            std::process::Command::new("launchctl")
                .args(["print", &service])
                .output()
                .ok()
                .map(|output| output.status.success())
        });
        PlatformStatus {
            platform: platform_name().to_string(),
            enabled: plist_path(job).is_ok_and(|path| path.exists()),
            running,
        }
    }
}

#[cfg(target_os = "windows")]
mod native {
    use super::*;

    pub(crate) fn platform_name() -> &'static str {
        "windows"
    }

    fn task_name(job: &AutostartJob) -> String {
        format!(r"\GIB\Live\{}", job.id)
    }

    fn artifact_path(client: &Gib, job: &AutostartJob) -> PathBuf {
        client
            .context()
            .data_dir
            .join("autostart")
            .join("platform")
            .join(format!("{}.xml", job.id))
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn render_task(job: &AutostartJob, executable: &Path) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  <RegistrationInfo><Description>GIB Live job {}</Description></RegistrationInfo>\n  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n  <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\n  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n    <RestartOnFailure><Interval>PT5M</Interval><Count>3</Count></RestartOnFailure>\n    <Enabled>true</Enabled>\n  </Settings>\n  <Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>--mode json autostart run {}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>\n</Task>\n",
            xml_escape(&job.id),
            xml_escape(&executable.to_string_lossy()),
            xml_escape(&job.id),
            xml_escape(&job.root_path.to_string_lossy()),
        )
    }

    pub(crate) fn enable(
        client: &Gib,
        job: &AutostartJob,
        executable: &Path,
        start_now: bool,
    ) -> Result<(), GibError> {
        let artifact = artifact_path(client, job);
        let artifact_string = artifact.to_string_lossy().to_string();
        write_artifact(&artifact, &render_task(job, executable))?;
        let name = task_name(job);
        if let Err(error) = run_command(
            "schtasks",
            &["/create", "/tn", &name, "/xml", &artifact_string, "/f"],
        ) {
            let _ = remove_artifact(&artifact);
            return Err(error);
        }
        if start_now {
            run_command("schtasks", &["/run", "/tn", &name])?;
        }
        Ok(())
    }

    pub(crate) fn disable(_client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
        let name = task_name(job);
        let _ = run_command("schtasks", &["/end", "/tn", &name]);
        let _ = run_command("schtasks", &["/change", "/tn", &name, "/disable"]);
        Ok(())
    }

    pub(crate) fn remove(client: &Gib, job: &AutostartJob) -> Result<(), GibError> {
        let name = task_name(job);
        let _ = run_command("schtasks", &["/delete", "/tn", &name, "/f"]);
        remove_artifact(&artifact_path(client, job))
    }

    pub(crate) fn status(client: &Gib, job: &AutostartJob) -> PlatformStatus {
        let name = task_name(job);
        let output = std::process::Command::new("schtasks")
            .args(["/query", "/tn", &name, "/fo", "LIST", "/v"])
            .output()
            .ok();
        let running = output.as_ref().map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains("Running"))
        });
        PlatformStatus {
            platform: platform_name().to_string(),
            enabled: artifact_path(client, job).exists(),
            running,
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod native {
    use super::*;

    pub(crate) fn platform_name() -> &'static str {
        "unsupported"
    }

    pub(crate) fn enable(
        _client: &Gib,
        _job: &AutostartJob,
        _executable: &Path,
        _start_now: bool,
    ) -> Result<(), GibError> {
        Err(GibError::new(
            ErrorCode::Unsupported,
            "Autostart is not supported on this operating system",
        ))
    }

    pub(crate) fn disable(_client: &Gib, _job: &AutostartJob) -> Result<(), GibError> {
        Ok(())
    }

    pub(crate) fn remove(_client: &Gib, _job: &AutostartJob) -> Result<(), GibError> {
        Ok(())
    }

    pub(crate) fn status(_client: &Gib, _job: &AutostartJob) -> PlatformStatus {
        PlatformStatus {
            platform: platform_name().to_string(),
            enabled: false,
            running: None,
        }
    }
}
