use gib::{
    ConfigurationOverrides, ConfigurationResolutionRequest, ConfigurationResolver,
    FilesystemPermissionPolicy, FilesystemScanOptions, IgnorePolicy, RelativePath,
    local_filesystem_scanner,
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
        .ok_or("missing command; use scan, decide, or read")?;

    match command.as_str() {
        "scan" => {
            let root = arguments
                .next()
                .map(PathBuf::from)
                .ok_or("missing source root")?;
            let options = CaptureArguments::parse(arguments, true)?;
            scan(&root, &options)
        }
        "read" => {
            let root = arguments
                .next()
                .map(PathBuf::from)
                .ok_or("missing source root")?;
            let relative = arguments
                .next()
                .ok_or("missing normalized relative file path")?;
            let mut options = CaptureArguments::parse(arguments, true)?;
            if options.delay_ms.is_none() {
                options.delay_ms = Some(25);
            }
            read_verified(&root, &relative, &options)
        }
        "decide" => {
            let relative = arguments.next().ok_or("missing normalized relative path")?;
            let options = CaptureArguments::parse(arguments, false)?;
            explain(&relative, &options)
        }
        _ => Err(format!("unknown command {command}; use scan, decide, or read").into()),
    }
}

struct CaptureArguments {
    patterns: Vec<String>,
    config: Option<PathBuf>,
    no_ignore_git: bool,
    delay_ms: Option<u64>,
}

impl CaptureArguments {
    fn parse<I>(arguments: I, allow_delay: bool) -> Result<Self, Box<dyn Error>>
    where
        I: IntoIterator<Item = String>,
    {
        let mut parsed = Self {
            patterns: Vec::new(),
            config: None,
            no_ignore_git: false,
            delay_ms: None,
        };
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--ignore" => parsed
                    .patterns
                    .push(arguments.next().ok_or("--ignore requires a pattern")?),
                "--no-ignore-git" => parsed.no_ignore_git = true,
                "--config" => {
                    parsed.config = Some(
                        arguments
                            .next()
                            .map(PathBuf::from)
                            .ok_or("--config requires a path")?,
                    );
                }
                _ if allow_delay && parsed.delay_ms.is_none() => {
                    parsed.delay_ms = Some(argument.parse::<u64>()?);
                }
                _ => return Err(format!("unexpected capture argument: {argument}").into()),
            }
        }
        Ok(parsed)
    }

    fn policy(&self, starting_directory: &Path) -> Result<IgnorePolicy, Box<dyn Error>> {
        let policy = if let Some(config_path) = &self.config {
            let overrides = ConfigurationOverrides::new().with_ignore_rules(self.patterns.clone());
            let request = ConfigurationResolutionRequest::new(starting_directory)
                .with_config_path(config_path)
                .with_overrides(overrides);
            ConfigurationResolver::default()
                .resolve(if self.no_ignore_git {
                    request.with_no_ignore_git()
                } else {
                    request
                })?
                .ignore_policy()
                .clone()
        } else {
            IgnorePolicy::new(self.patterns.iter().map(String::as_str))?
        };
        Ok(if self.no_ignore_git {
            policy.with_no_ignore_git()
        } else {
            policy
        })
    }
}

fn scan(root: &Path, options: &CaptureArguments) -> Result<(), Box<dyn Error>> {
    let policy = options.policy(root)?;
    let scanner = local_filesystem_scanner()
        .with_options(
            FilesystemScanOptions::new().with_permission_policy(FilesystemPermissionPolicy::Warn),
        )
        .with_ignore_policy(policy);
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
        if let Some(delay_ms) = options.delay_ms
            && delay_ms != 0
        {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    Ok(())
}

fn read_verified(
    root: &Path,
    relative: &str,
    options: &CaptureArguments,
) -> Result<(), Box<dyn Error>> {
    let relative = RelativePath::new(relative)?;
    let scanner = local_filesystem_scanner().with_ignore_policy(options.policy(root)?);
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
        if let Some(delay_ms) = options.delay_ms
            && delay_ms != 0
        {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    reader.finish()?;
    println!("verified {total} bytes from {relative}");
    Ok(())
}

fn explain(relative: &str, options: &CaptureArguments) -> Result<(), Box<dyn Error>> {
    let decision = options.policy(Path::new("."))?.decision(relative)?;
    if !decision.is_ignored() {
        println!("{}\tincluded", decision.path());
        return Ok(());
    }

    let matched = decision
        .matched()
        .ok_or("ignored decision did not expose a matching rule")?;
    if matched.is_git_path() {
        println!("{}\tignored\tmatched=.git", decision.path());
    } else if let Some(pattern) = matched.pattern() {
        println!("{}\tignored\tmatched={pattern}", decision.path());
    }
    Ok(())
}
