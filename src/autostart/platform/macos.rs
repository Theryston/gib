use crate::autostart::model::AutostartJob;
use crate::autostart::platform::{PlatformStatus, remove_artifact, write_artifact};
use crate::autostart::registry::RegistryPaths;
use std::path::{Path, PathBuf};
use std::process::Command;

fn label(job: &AutostartJob) -> String {
    format!("org.trygib.live.{}", job.id)
}

fn plist_path(job: &AutostartJob) -> Result<PathBuf, String> {
    let home =
        dirs::home_dir().ok_or_else(|| "Failed to determine the home directory".to_string())?;
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

pub(crate) fn render_launch_agent(job: &AutostartJob, executable: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>--mode</string>\n    <string>json</string>\n    <string>autostart</string>\n    <string>run</string>\n    <string>{}</string>\n  </array>\n  <key>WorkingDirectory</key>\n  <string>{}</string>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ThrottleInterval</key>\n  <integer>5</integer>\n</dict>\n</plist>\n",
        xml_escape(&label(job)),
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&job.id),
        xml_escape(&job.root_path),
    )
}

fn uid() -> Result<String, String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| format!("Failed to determine the macOS user ID: {}", error))?;
    if !output.status.success() {
        return Err("The macOS user ID command failed".to_string());
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "The macOS user ID was not valid UTF-8".to_string())
}

fn gui_target() -> Result<String, String> {
    Ok(format!("gui/{}", uid()?))
}

pub(crate) fn enable(
    _paths: &RegistryPaths,
    job: &AutostartJob,
    executable: &Path,
    start_now: bool,
) -> Result<(), String> {
    let path = plist_path(job)?;
    let path_string = path.to_string_lossy().to_string();
    write_artifact(&path, &render_launch_agent(job, executable))?;
    if !start_now {
        return Ok(());
    }
    let target = gui_target()?;
    let _ = Command::new("launchctl")
        .args(["bootout", target.as_str(), path_string.as_str()])
        .output();
    let output = Command::new("launchctl")
        .args(["bootstrap", target.as_str(), path_string.as_str()])
        .output()
        .map_err(|error| format!("Failed to start launchctl: {}", error))?;
    if !output.status.success() {
        let _ = remove_artifact(&path);
        return Err(format!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if start_now {
        let target_label = format!("{}/{}", target, label(job));
        let output = Command::new("launchctl")
            .args(["kickstart", "-k", &target_label])
            .output()
            .map_err(|error| format!("Failed to start the LaunchAgent: {}", error))?;
        if !output.status.success() {
            return Err(format!(
                "launchctl kickstart failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

pub(crate) fn disable(_paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    let path = plist_path(job)?;
    let path_string = path.to_string_lossy().to_string();
    if let Ok(target) = gui_target() {
        let _ = Command::new("launchctl")
            .args(["bootout", target.as_str(), path_string.as_str()])
            .output();
    }
    remove_artifact(&path)
}

pub(crate) fn remove(_paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    disable(_paths, job)?;
    remove_artifact(&plist_path(job)?)
}

pub(crate) fn status(_paths: &RegistryPaths, job: &AutostartJob) -> PlatformStatus {
    let target = gui_target().ok();
    let running = target.and_then(|target| {
        let service = format!("{}/{}", target, label(job));
        Command::new("launchctl")
            .args(["print", &service])
            .output()
            .ok()
            .map(|output| output.status.success())
    });
    PlatformStatus {
        platform: "macos",
        enabled: plist_path(job).is_ok_and(|path| path.exists()),
        running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::model::{AUTOSTART_JOB_VERSION, LiveJobOverrides, SecretReferences};

    #[test]
    fn renders_a_user_launch_agent_with_an_argument_array() {
        let job = AutostartJob {
            version: AUTOSTART_JOB_VERSION,
            id: "job-1".to_string(),
            name: "code".to_string(),
            enabled: true,
            root_path: "/tmp/project & copy".to_string(),
            config_path: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
            overrides: LiveJobOverrides::default(),
            secrets: SecretReferences::default(),
        };
        let plist = render_launch_agent(&job, Path::new("/Applications/GIB/gib"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("project &amp; copy"));
        assert!(plist.contains("<string>autostart</string>"));
    }
}
