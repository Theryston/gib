mod autostart_platform;
mod autostart_secrets;
mod controller;
mod definition;
mod render;

use clap::error::ErrorKind;
use definition::command;
use render::{CliOutput, OutputMode};

pub async fn run() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = OutputMode::from_args(&args);
    let output = CliOutput::new(mode);
    install_json_panic_hook(&output);
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => {
            match error.kind() {
                ErrorKind::DisplayHelp => output.help(error.to_string()),
                ErrorKind::DisplayVersion => output.version(error.to_string()),
                _ => output.error_with_code(&error.to_string(), "cli_error"),
            }
            if !matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                std::process::exit(1);
            }
            return;
        }
    };

    let client = match controller::client(&matches, &output) {
        Ok(client) => client,
        Err(error) => {
            output.error(&error);
            std::process::exit(1);
        }
    };
    if let Err(error) = controller::dispatch(&client, &matches, &output).await {
        output.error(&error);
        std::process::exit(1);
    }
}

fn install_json_panic_hook(output: &CliOutput) {
    if !output.is_json() {
        return;
    }
    let output = output.clone();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|value| (*value).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_string());
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()));
        output.json_stderr(&render::json_envelope(
            "error",
            serde_json::json!({
                "message": message,
                "code": "panic",
                "location": location,
            }),
        ));
    }));
}
