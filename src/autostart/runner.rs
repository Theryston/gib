use crate::autostart::registry::{ensure_registry, log_path, read_job, registry_paths};
use crate::commands::backup::resolve_live_overrides;
use crate::commands::live::{conflict_policy_from_name, emit_live_start, run_live};
use crate::core::secrets;
use crate::output::{configure_json_log, emit_named_event};
use serde_json::json;
use std::env;
use std::path::Path;

pub(crate) async fn run(job_id: &str) -> Result<(), String> {
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let job = read_job(&paths, job_id)?;

    if !job.enabled {
        return Err(format!("Autostart job '{}' is disabled", job_id));
    }

    let log = log_path(&paths, job_id)?;
    configure_json_log(&log)?;
    emit_named_event(
        "autostart",
        &json!({
            "event": "started",
            "id": job.id,
            "name": job.name,
            "root_path": job.root_path,
            "log_path": log,
        }),
    );

    let root = Path::new(&job.root_path);
    if !root.is_dir() {
        let message = format!(
            "Autostart root '{}' is not an existing directory",
            root.display()
        );
        emit_named_event(
            "autostart",
            &json!({
                "event": "configuration_error",
                "id": job.id,
                "message": message,
                "recoverable": false,
            }),
        );
        return Err(message);
    }

    env::set_current_dir(root).map_err(|error| {
        format!(
            "Failed to use autostart root '{}' as the working directory: {}",
            root.display(),
            error
        )
    })?;

    let password = match job.secrets.password_ref.as_deref() {
        Some(reference) => match secrets::read_password(reference) {
            Ok(password) => Some(password),
            Err(error) => {
                emit_named_event(
                    "autostart",
                    &json!({
                        "event": "secret_unavailable",
                        "id": job.id,
                        "reference": reference,
                        "message": error,
                        "recoverable": true,
                    }),
                );
                return Err(format!(
                    "The password for autostart job '{}' is unavailable; update the job and provide the password again",
                    job.name
                ));
            }
        },
        None => None,
    };

    let resolved = match resolve_live_overrides(job.live_overrides(password)).await {
        Ok(resolved) => resolved,
        Err(error) => {
            emit_named_event(
                "autostart",
                &json!({
                    "event": "configuration_error",
                    "id": job.id,
                    "message": error,
                    "recoverable": true,
                }),
            );
            return Err(format!(
                "Failed to resolve autostart job '{}': {}",
                job.name, error
            ));
        }
    };

    let policy = conflict_policy_from_name(&job.overrides.conflict)?;
    emit_live_start(&resolved, policy);

    let result = run_live(resolved, policy).await;
    match &result {
        Ok(()) => emit_named_event(
            "autostart",
            &json!({
                "event": "stopped",
                "id": job.id,
                "reason": "requested",
            }),
        ),
        Err(error) => emit_named_event(
            "autostart",
            &json!({
                "event": "failed",
                "id": job.id,
                "message": error,
                "recoverable": true,
            }),
        ),
    }
    result
}
