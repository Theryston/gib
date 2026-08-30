use crate::commands::config::Config;
use crate::output::{emit_output, is_json_mode};
use crate::utils::handle_error;
use dirs::home_dir;

pub fn whoami() {
    let home_dir = home_dir().unwrap();
    let config_path = home_dir.join(".gib").join("config.msgpack");
    let config_bytes = std::fs::read(&config_path)
        .unwrap_or_else(|e| handle_error(format!("Failed to read config: {}", e), None));
    let config: Config = rmp_serde::from_slice(&config_bytes)
        .unwrap_or_else(|e| handle_error(format!("Failed to parse config: {}", e), None));

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct WhoamiOutput {
            author: String,
        }
        let payload = WhoamiOutput {
            author: config.author,
        };
        emit_output(&payload);
    } else {
        println!("You are: {}", config.author);
    }
}
