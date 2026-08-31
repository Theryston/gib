use gib::{ProjectConfiguration, ProjectConfigurationError, load_configuration};
use std::error::Error;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let path = argument("configuration file")?;
    let current_directory = std::env::current_dir()?;
    let configuration = match load_configuration(&path) {
        Ok(configuration) => configuration,
        Err(error) => {
            print_error(&error);
            return Err(error.into());
        }
    };

    println!("configuration file: {}", path.display());
    println!("process directory: {}", current_directory.display());
    print_configuration(&configuration);
    Ok(())
}

fn argument(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}; pass it as the first argument").into())
}

fn print_configuration(configuration: &ProjectConfiguration) {
    println!("version: {}", configuration.version());

    println!("[repository]");
    print_optional("storage", configuration.repository().storage());
    print_optional("key", configuration.repository().key());

    println!("[backup]");
    print_optional_path("root_path", configuration.backup().root_path());
    print_optional("message", configuration.backup().message());
    print_optional_display("compress", configuration.backup().compress());
    print_optional_display("chunk_size_bytes", configuration.backup().chunk_size());
    print_optional_display("concurrency", configuration.backup().concurrency());
    println!("ignore: {:?}", configuration.backup().ignore());

    println!("[live]");
    print_optional("message", configuration.live().message());
    print_optional_display("debounce_ms", configuration.live().debounce_ms());
    print_optional_display("poll_ms", configuration.live().poll_ms());

    println!("[restore]");
    print_optional_path("target_path", configuration.restore().target_path());
}

fn print_error(error: &ProjectConfigurationError) {
    eprintln!("configuration failed");
    eprintln!("  kind: {:?}", error.kind());
    if let Some(file) = error.file() {
        eprintln!("  file: {}", file.display());
    }
    if let Some(field) = error.field() {
        eprintln!("  field: {field}");
    }
    if let Some(version) = error.version() {
        eprintln!("  version: {version}");
    }
    eprintln!("  reason: {}", error.reason());
    eprintln!("  message: {error}");
}

fn print_optional(name: &str, value: Option<&str>) {
    match value {
        Some(value) => println!("{name}: {value:?}"),
        None => println!("{name}: <absent>"),
    }
}

fn print_optional_display<T: std::fmt::Display>(name: &str, value: Option<T>) {
    match value {
        Some(value) => println!("{name}: {value}"),
        None => println!("{name}: <absent>"),
    }
}

fn print_optional_path(name: &str, value: Option<&Path>) {
    match value {
        Some(value) => println!("{name}: {}", value.display()),
        None => println!("{name}: <absent>"),
    }
}
