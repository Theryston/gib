use crate::autostart::model::AutostartJob;
use crate::autostart::platform::{PlatformStatus, remove_artifact, write_artifact};
use crate::autostart::registry::{RegistryPaths, platform_path};
use std::path::Path;
use std::process::Command;

fn task_name(job: &AutostartJob) -> String {
    format!(r"\GIB\Live\{}", job.id)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn render_task_xml(job: &AutostartJob, executable: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  <RegistrationInfo><Description>GIB Live job {}</Description></RegistrationInfo>\n  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n  <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\n  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n    <RestartOnFailure><Interval>PT5M</Interval><Count>3</Count></RestartOnFailure>\n    <Enabled>true</Enabled>\n  </Settings>\n  <Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>--mode json autostart run {}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>\n</Task>\n",
        xml_escape(&job.id),
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&job.id),
        xml_escape(&job.root_path),
    )
}

fn run_schtasks(args: &[&str]) -> Result<(), String> {
    let output = Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|error| format!("Failed to start schtasks: {}", error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "schtasks {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(crate) fn enable(
    paths: &RegistryPaths,
    job: &AutostartJob,
    executable: &Path,
    start_now: bool,
) -> Result<(), String> {
    let artifact = platform_path(paths, &job.id)?;
    let artifact_string = artifact.to_string_lossy().to_string();
    write_artifact(&artifact, &render_task_xml(job, executable))?;
    let name = task_name(job);
    if let Err(error) = run_schtasks(&[
        "/create",
        "/tn",
        name.as_str(),
        "/xml",
        artifact_string.as_str(),
        "/f",
    ]) {
        let _ = remove_artifact(&artifact);
        return Err(error);
    }
    if start_now {
        run_schtasks(&["/run", "/tn", &name])?;
    }
    Ok(())
}

pub(crate) fn disable(_paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    let name = task_name(job);
    let _ = run_schtasks(&["/end", "/tn", name.as_str()]);
    let _ = run_schtasks(&["/change", "/tn", name.as_str(), "/disable"]);
    Ok(())
}

pub(crate) fn remove(paths: &RegistryPaths, job: &AutostartJob) -> Result<(), String> {
    let name = task_name(job);
    let _ = run_schtasks(&["/delete", "/tn", name.as_str(), "/f"]);
    remove_artifact(&platform_path(paths, &job.id)?)
}

pub(crate) fn status(paths: &RegistryPaths, job: &AutostartJob) -> PlatformStatus {
    let name = task_name(job);
    let output = Command::new("schtasks")
        .args(["/query", "/tn", name.as_str(), "/fo", "LIST", "/v"])
        .output()
        .ok();
    let running = output.as_ref().map(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.contains("Running"))
    });
    PlatformStatus {
        platform: "windows",
        enabled: platform_path(paths, &job.id).is_ok_and(|path| path.exists()),
        running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autostart::model::{AUTOSTART_JOB_VERSION, LiveJobOverrides, SecretReferences};

    #[test]
    fn renders_a_single_instance_task_with_an_absolute_runner() {
        let job = AutostartJob {
            version: AUTOSTART_JOB_VERSION,
            id: "job-1".to_string(),
            name: "code".to_string(),
            enabled: true,
            root_path: r"C:\Users\A User\code".to_string(),
            config_path: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
            overrides: LiveJobOverrides::default(),
            secrets: SecretReferences::default(),
        };
        let task = render_task_xml(&job, Path::new(r"C:\Program Files\GIB\gib.exe"));
        assert!(task.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(task.contains("autostart run job-1"));
        assert!(task.contains("Program Files"));
    }
}
