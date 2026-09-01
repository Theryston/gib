mod app;
mod commands;
mod input;
mod interactive;
mod output;

use std::process::ExitCode;

fn main() -> ExitCode {
    app::run()
}
