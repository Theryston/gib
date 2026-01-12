use crate::commands::config::Config;
use dirs::home_dir;

pub fn whoami() {
    let home_dir = home_dir().unwrap();
    let config_path = home_dir.join(".gib").join("config.msgpack");
    let config: Config = rmp_serde::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    println!("You are: {}", config.author);
}
