use gib::{
    FilesystemPermissionPolicy, FilesystemScanOptions, RelativePath, local_filesystem_scanner,
};
use std::error::Error;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments
        .next()
        .ok_or("missing command; use scan [delay-ms] or read")?;
    let root = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("missing source root")?;

    match command.as_str() {
        "scan" => {
            let delay_ms = arguments
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(0);
            scan(&root, delay_ms)
        }
        "read" => {
            let relative = arguments
                .next()
                .ok_or("missing normalized relative file path")?;
            let delay_ms = arguments
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(25);
            read_verified(&root, &relative, delay_ms)
        }
        _ => Err(format!("unknown command {command}; use scan or read").into()),
    }
}

fn scan(root: &Path, delay_ms: u64) -> Result<(), Box<dyn Error>> {
    let scanner = local_filesystem_scanner().with_options(
        FilesystemScanOptions::new().with_permission_policy(FilesystemPermissionPolicy::Warn),
    );
    for item in scanner.scan(root)? {
        match item {
            Ok(entry) => {
                let path = if entry.is_root() {
                    String::from(".")
                } else {
                    entry.path().as_str().to_owned()
                };
                println!(
                    "{path}\tkind={}\tsize={}\tpermissions={:?}\tmodified_at={:?}\tidentity={:?}",
                    entry.kind(),
                    entry.metadata().size(),
                    entry
                        .metadata()
                        .permissions()
                        .map(|permissions| permissions.mode()),
                    entry.metadata().modified_at(),
                    entry.metadata().identity(),
                );
            }
            Err(error) if error.is_permission_denied() => {
                eprintln!("warning: {error}");
            }
            Err(error) => return Err(error.into()),
        }
        if delay_ms != 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    Ok(())
}

fn read_verified(root: &Path, relative: &str, delay_ms: u64) -> Result<(), Box<dyn Error>> {
    let relative = RelativePath::new(relative)?;
    let scanner = local_filesystem_scanner();
    let mut scan = scanner.scan(root)?;
    let mut selected = None;
    for item in &mut scan {
        let entry = item?;
        if entry.path() == &relative {
            selected = Some(entry);
            break;
        }
    }
    let entry = selected.ok_or_else(|| format!("file not found in scan: {relative}"))?;
    let mut reader = scan.open_file(&entry)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                let verification = reader.finish();
                return match verification {
                    Err(scan_error) => Err(scan_error.into()),
                    Ok(()) => Err(error.into()),
                };
            }
        };
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read)?);
        thread::sleep(Duration::from_millis(delay_ms));
    }
    reader.finish()?;
    println!("verified {total} bytes from {relative}");
    Ok(())
}
