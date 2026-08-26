use clap::{Arg, Command, arg};

use crate::output::{
    detect_mode_from_args, emit_error, emit_help, emit_version, init_panic_hook_if_json,
    is_json_mode, set_output_mode,
};
use crate::utils::handle_error;

mod autostart;
mod commands;
mod config;
mod core;
mod fs;
mod output;
mod utils;

fn cli() -> Command {
    Command::new("gib")
        .about("A blazingly fast, modern backup tool with versioning, deduplication, and encryption.")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .allow_external_subcommands(true)
        .arg(
            Arg::new("mode")
                .long("mode")
                .value_name("MODE")
                .help("Output mode")
                .default_value("interactive")
                .value_parser(["interactive", "json"])
                .global(true),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("PATH")
                .help("Use a specific local gib.toml file")
                .global(true),
        )
        .arg(
            Arg::new("no-config")
                .long("no-config")
                .help("Disable local gib.toml discovery")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("config")
                .global(true),
        )
        .subcommand(
            Command::new("config")
                .about("Configure your backup tool")
                .arg(
                arg!(-a --author <AUTHOR> "Your identity like 'John Doe <john.doe@example.com>'")
                    .required(false),
            ),
        )
        .subcommand(
            Command::new("whoami")
                .about("Show your identity")
        )
        .subcommand(
            Command::new("setup")
                .about("Discover local GIB storages and configure them")
                .arg(
                    Arg::new("no-recursive")
                        .long("no-recursive")
                        .help("Only inspect storage directories directly below the current directory")
                        .action(clap::ArgAction::SetTrue)
                        .required(false),
                ),
        )
        .subcommand(
            Command::new("encrypt")
                .about("Encrypt all chunks of your repository")
                .arg(arg!(-p --password <PASSWORD> "The password to use for the encryption").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use for the encryption").required(false))
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
        )
        .subcommand(
            Command::new("log")
                .about("List all backups for a repository")
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
        )
        .subcommand(
            Command::new("search")
                .about("Search files in the historical filesystem catalog")
                .arg(
                    Arg::new("query")
                        .value_name("QUERY")
                        .help("Case-insensitive tokens to search for in file paths")
                        .required(true),
                )
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PREFIX")
                        .help("Restrict results to this relative path prefix")
                        .required(false),
                )
                .arg(
                    Arg::new("extension")
                        .long("extension")
                        .value_name("EXT")
                        .help("Restrict results to this file extension (without a leading dot)")
                        .required(false),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .help("Maximum number of results to display (default: 100)")
                        .value_parser(clap::value_parser!(usize))
                        .default_value("100")
                        .required(false),
                )
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
        )
        .subcommand(
            Command::new("explore")
                .about("Browse and restore files from the historical filesystem catalog")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .help("Relative catalog directory or file path to open")
                        .required(false),
                )
                .arg(
                    Arg::new("scope")
                        .long("scope")
                        .value_name("SCOPE")
                        .help("Catalog scope: current or all-history (default: all-history)")
                        .value_parser(["current", "all-history"])
                        .default_value("all-history")
                        .required(false),
                )
                .arg(
                    Arg::new("query")
                        .long("query")
                        .value_name("QUERY")
                        .help("Search catalog paths using the shared token index")
                        .required(false),
                )
                .arg(
                    Arg::new("history")
                        .long("history")
                        .help("Show only restorable revisions for the file at --path")
                        .action(clap::ArgAction::SetTrue)
                        .required(false),
                )
                .arg(
                    Arg::new("restore")
                        .long("restore")
                        .help("Restore selected catalog entries")
                        .action(clap::ArgAction::SetTrue)
                        .required(false),
                )
                .arg(
                    Arg::new("select")
                        .long("select")
                        .value_name("PATH")
                        .help("File or directory path to restore (repeatable; JSON also accepts --path)")
                        .action(clap::ArgAction::Append)
                        .required(false),
                )
                .arg(
                    Arg::new("revision")
                        .long("revision")
                        .value_name("PATH=BACKUP")
                        .help("Restore a specific revision (PATH=BACKUP, or BACKUP with --path)")
                        .action(clap::ArgAction::Append)
                        .required(false),
                )
                .arg(
                    Arg::new("target-path")
                        .short('t')
                        .long("target-path")
                        .value_name("TARGET_PATH")
                        .help("Destination directory for restore (default: restore.target_path or current directory)")
                        .required(false),
                )
                .arg(
                    Arg::new("cursor")
                        .long("cursor")
                        .value_name("CURSOR")
                        .help("Continue a paginated directory or search result listing")
                        .required(false),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .help("Maximum search results (default: 100)")
                        .value_parser(clap::value_parser!(usize))
                        .default_value("100")
                        .required(false),
                )
                .arg(
                    Arg::new("sort")
                        .long("sort")
                        .value_name("ORDER")
                        .help("Interactive order: name, size, status, or recent")
                        .value_parser(["name", "size", "status", "recent"])
                        .default_value("name")
                        .required(false),
                )
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
        )
        .subcommand(
            Command::new("backup")
                .about("Create a backup of a directory and store it in a storage")
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-m --message <MESSAGE> "The backup message").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use for the backup").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for the backup").required(false))
                .arg(arg!(-c --compress <COMPRESS> "The compression level to use for the backup").required(false))
                .arg(
                    Arg::new("chunk-size")
                        .short('z')
                        .long("chunk-size")
                        .value_name("CHUNK_SIZE")
                        .help("The chunk size to use for the backup (default: 5 MB)")
                        .required(false),
                )
                .arg(
                    Arg::new("root-path")
                        .short('r')
                        .long("root-path")
                        .value_name("ROOT_PATH")
                        .help("The root path to backup")
                        .required(false),
                )
                .arg(
                    Arg::new("ignore")
                        .short('i')
                        .long("ignore")
                        .value_name("IGNORE")
                        .help("File or folder names to ignore (can be used multiple times)")
                        .required(false)
                        .action(clap::ArgAction::Append),
                )
                .arg(
                    Arg::new("continue")
                        .long("continue")
                        .value_name("BACKUP")
                        .help("Continue the backup from an incomplete backup")
                        .required(false),
                )
                .arg(
                    Arg::new("parent")
                        .long("parent")
                        .value_name("BACKUP")
                        .help("Inherit the file tree from a previous backup (hash, prefix, or latest)")
                        .num_args(0..=1)
                        .required(false),
                )
                .arg(
                    Arg::new("concurrency")
                        .long("concurrency")
                        .help("How many files to process at the same time [default: the number of CPUs * 2]")
                        .value_name("CONCURRENCY")
                        .required(false),
                )
                .subcommand(
                    Command::new("pending")
                        .about("List pending backups for a repository")
                        .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                        .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                        .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                )
                .subcommand(
                    Command::new("delete")
                        .about("Delete a backup and its orphaned chunks")
                        .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                        .arg(arg!(-b --backup <BACKUP> "The backup hash, prefix, or latest to delete").required(false))
                        .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                        .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                )
        )
        .subcommand(
            Command::new("live")
                .about("Keep a directory backed up and synchronized across active devices")
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-m --message <MESSAGE> "Optional context included in automatic backup messages").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use for the backup").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for the backup").required(false))
                .arg(
                    Arg::new("conflict")
                        .long("conflict")
                        .value_name("POLICY")
                        .value_parser(["local", "remote"])
                        .help("Conflict resolution policy: keep local changes or use remote changes (required in JSON mode)")
                        .required_if_eq("mode", "json"),
                )
                .arg(arg!(-c --compress <COMPRESS> "The compression level to use for the backup").required(false))
                .arg(
                    Arg::new("chunk-size")
                        .short('z')
                        .long("chunk-size")
                        .value_name("CHUNK_SIZE")
                        .help("The chunk size to use for the backup (default: 5 MB)")
                        .required(false),
                )
                .arg(
                    Arg::new("root-path")
                        .short('r')
                        .long("root-path")
                        .value_name("ROOT_PATH")
                        .help("The root path to keep backed up and synchronized")
                        .required(false),
                )
                .arg(
                    Arg::new("ignore")
                        .short('i')
                        .long("ignore")
                        .value_name("IGNORE")
                        .help("File or folder names to ignore (can be used multiple times)")
                        .required(false)
                        .action(clap::ArgAction::Append),
                )
                .arg(
                    Arg::new("continue")
                        .long("continue")
                        .value_name("BACKUP")
                        .help("Continue the backup from an incomplete backup (not valid for live)")
                        .required(false),
                )
                .arg(
                    Arg::new("parent")
                        .long("parent")
                        .value_name("BACKUP")
                        .help("Use a parent backup (not valid for live)")
                        .num_args(0..=1)
                        .required(false),
                )
                .arg(
                    Arg::new("concurrency")
                        .long("concurrency")
                        .help("How many files to process at the same time [default: the number of CPUs * 2]")
                        .value_name("CONCURRENCY")
                        .required(false),
                ),
        )
        .subcommand(
            Command::new("autostart")
                .about("Manage per-user jobs that keep directories synchronized with gib live")
                .subcommand(
                    Command::new("add")
                        .about("Register a directory as a per-user live job")
                        .arg(
                            Arg::new("name")
                                .long("name")
                                .value_name("NAME")
                                .help("Stable human-readable job name")
                                .required(false),
                        )
                        .arg(
                            Arg::new("root-path")
                                .short('r')
                                .long("root-path")
                                .value_name("ROOT_PATH")
                                .help("Directory to keep backed up and synchronized")
                                .required(false),
                        )
                        .arg(arg!(-k --key <KEY> "Repository key override").required(false))
                        .arg(arg!(-s --storage <STORAGE> "Storage name override").required(false))
                        .arg(arg!(-p --password <PASSWORD> "Repository password (stored in the user credential store)").required(false))
                        .arg(arg!(-m --message <MESSAGE> "Automatic live backup message").required(false))
                        .arg(arg!(-c --compress <COMPRESS> "Compression level").required(false))
                        .arg(
                            Arg::new("chunk-size")
                                .short('z')
                                .long("chunk-size")
                                .value_name("CHUNK_SIZE")
                                .help("Chunk size")
                                .required(false),
                        )
                        .arg(
                            Arg::new("ignore")
                                .short('i')
                                .long("ignore")
                                .value_name("IGNORE")
                                .help("File or folder name to ignore (repeatable)")
                                .action(clap::ArgAction::Append)
                                .required(false),
                        )
                        .arg(
                            Arg::new("concurrency")
                                .long("concurrency")
                                .value_name("CONCURRENCY")
                                .help("Maximum concurrent file operations")
                                .required(false),
                        )
                        .arg(
                            Arg::new("conflict")
                                .long("conflict")
                                .value_name("POLICY")
                                .value_parser(["local", "remote"])
                                .help("Conflict policy used by the background live process")
                                .required(false),
                        )
                        .arg(
                            Arg::new("start-now")
                                .long("start-now")
                                .help("Start the job immediately after registering it")
                                .action(clap::ArgAction::SetTrue)
                                .required(false),
                        )
                        .arg(
                            Arg::new("replace")
                                .long("replace")
                                .help("Replace an existing job with the same name")
                                .action(clap::ArgAction::SetTrue)
                                .required(false),
                        ),
                )
                .subcommand(
                    Command::new("update")
                        .about("Update a registered live job")
                        .arg(Arg::new("name").value_name("NAME").required(true))
                        .arg(
                            Arg::new("root-path")
                                .short('r')
                                .long("root-path")
                                .value_name("ROOT_PATH")
                                .help("New directory to keep synchronized")
                                .required(false),
                        )
                        .arg(Arg::new("key").short('k').long("key").value_name("KEY").required(false))
                        .arg(Arg::new("storage").short('s').long("storage").value_name("STORAGE").required(false))
                        .arg(Arg::new("password").short('p').long("password").value_name("PASSWORD").required(false))
                        .arg(Arg::new("message").short('m').long("message").value_name("MESSAGE").required(false))
                        .arg(Arg::new("compress").short('c').long("compress").value_name("COMPRESS").required(false))
                        .arg(
                            Arg::new("chunk-size")
                                .short('z')
                                .long("chunk-size")
                                .value_name("CHUNK_SIZE")
                                .required(false),
                        )
                        .arg(
                            Arg::new("ignore")
                                .short('i')
                                .long("ignore")
                                .value_name("IGNORE")
                                .action(clap::ArgAction::Append)
                                .required(false),
                        )
                        .arg(Arg::new("concurrency").long("concurrency").value_name("CONCURRENCY").required(false))
                        .arg(
                            Arg::new("conflict")
                                .long("conflict")
                                .value_name("POLICY")
                                .value_parser(["local", "remote"])
                                .required(false),
                        )
                        .arg(
                            Arg::new("start-now")
                                .long("start-now")
                                .help("Enable and start the job immediately")
                                .action(clap::ArgAction::SetTrue)
                                .required(false),
                        ),
                )
                .subcommand(Command::new("list").about("List registered live jobs"))
                .subcommand(
                    Command::new("status")
                        .about("Show live job and platform status")
                        .arg(Arg::new("name").value_name("NAME").required(false)),
                )
                .subcommand(
                    Command::new("enable")
                        .about("Enable a live job")
                        .arg(Arg::new("name").value_name("NAME").required(true)),
                )
                .subcommand(
                    Command::new("disable")
                        .about("Disable a live job")
                        .arg(Arg::new("name").value_name("NAME").required(true)),
                )
                .subcommand(
                    Command::new("remove")
                        .about("Remove a live job registration")
                        .arg(Arg::new("name").value_name("NAME").required(true))
                        .arg(
                            Arg::new("yes")
                                .short('y')
                                .long("yes")
                                .help("Confirm removal without an interactive prompt")
                                .action(clap::ArgAction::SetTrue)
                                .required(false),
                        ),
                )
                .subcommand(
                    Command::new("run")
                        .about("Run one registered live job")
                        .hide(true)
                        .arg(Arg::new("job-id").value_name("JOB_ID").required(true)),
                ),
        )
        .subcommand(
            Command::new("restore")
                .about("Restore files from a backup")
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-b --backup <BACKUP> "The backup hash, prefix, or latest to restore").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                .arg(
                    Arg::new("only")
                        .long("only")
                        .value_name("PATH")
                        .help("Restore only the specified file or directory (omit value to select interactively)")
                        .num_args(0..=1)
                        .action(clap::ArgAction::Append)
                        .required(false),
                )
                .arg(
                    Arg::new("target-path")
                        .short('t')
                        .long("target-path")
                        .value_name("TARGET_PATH")
                        .help("The target directory to restore files to (default: current directory)")
                        .required(false),
                )
                .arg(
                    Arg::new("prune-local")
                        .short('d')
                        .long("prune-local")
                        .visible_alias("delete")
                        .help("Delete local files that are not in the backup tree")
                        .action(clap::ArgAction::SetTrue)
                        .required(false),
                )
        )
        .subcommand(
            Command::new("storage")
                .about("Manage your storage")
                .subcommand(
                    Command::new("add")
                        .about("Add a new storage")
                        .arg(arg!(-n --name <NAME> "The name of the storage").required(false))
                        .arg(
                            arg!(-t --type <TYPE> "The type of the storage ('local' or 's3')")
                                .required(false)
                                .value_parser(["local", "s3"]),
                        )
                        .arg(arg!(-p --path <PATH> "The path for storing backups (only for local storage)").required(false))
                        .arg(arg!(-r --region <REGION> "The region for the S3 storage (only for S3 storage)").required(false))
                        .arg(arg!(-b --bucket <BUCKET> "The bucket for the S3 storage (only for S3 storage)").required(false))
                        .arg(
                            Arg::new("access-key")
                                .short('a')
                                .long("access-key")
                                .value_name("ACCESS_KEY")
                                .help("The access key for the S3 storage (only for S3 storage)")
                                .required(false),
                        )
                        .arg(
                            Arg::new("secret-key")
                                .short('s')
                                .long("secret-key")
                                .value_name("SECRET_KEY")
                                .help("The secret key for the S3 storage (only for S3 storage)")
                                .required(false),
                        )
                        .arg(arg!(-e --endpoint <ENDPOINT> "The endpoint for the S3 storage (only for S3 storage)").required(false))
                )
                .subcommand(
                    Command::new("list")
                        .about("List all storages")
                )
                .subcommand(
                    Command::new("remove")
                        .about("Remove a storage")
                        .arg(arg!(-n --name <NAME> "The name of the storage").required(false))
                )
                .subcommand(
                    Command::new("prune")
                        .about("Prune unused chunks and incomplete backups")
                        .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                        .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                        .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                        .arg(
                            Arg::new("yes")
                                .short('y')
                                .long("yes")
                                .help("Skip confirmation prompt")
                                .action(clap::ArgAction::SetTrue)
                                .required(false),
                        )
                )
        )
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let detected_mode = detect_mode_from_args(&args);
    set_output_mode(detected_mode);
    init_panic_hook_if_json();

    let matches = match cli().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) => {
            if is_json_mode() {
                use clap::error::ErrorKind;
                match e.kind() {
                    ErrorKind::DisplayHelp => {
                        emit_help(e.to_string());
                        std::process::exit(0);
                    }
                    ErrorKind::DisplayVersion => {
                        emit_version(e.to_string());
                        std::process::exit(0);
                    }
                    _ => emit_error(&e.to_string(), "cli_error"),
                }
            } else {
                e.exit();
            }
        }
    };

    match matches.subcommand() {
        Some(("config", matches)) => commands::config(matches),
        Some(("whoami", _)) => commands::whoami(),
        Some(("setup", matches)) => commands::setup(matches),
        Some(("live", matches)) => commands::live(matches).await,
        Some(("autostart", matches)) => commands::autostart(matches).await,
        Some(("encrypt", matches)) => commands::encrypt(matches).await,
        Some(("log", matches)) => commands::log(matches).await,
        Some(("search", matches)) => commands::search(matches).await,
        Some(("explore", matches)) => commands::explore(matches).await,
        Some(("backup", matches)) => match matches.subcommand() {
            Some(("delete", matches)) => commands::delete(matches).await,
            Some(("pending", matches)) => commands::pending(matches).await,
            None => commands::backup(matches).await,
            _ => {
                handle_error(
                    "Invalid subcommand! Run 'gib backup --help' for more information.".to_string(),
                    None,
                );
            }
        },
        Some(("restore", matches)) => commands::restore(matches).await,
        Some(("storage", matches)) => match matches.subcommand() {
            Some(("add", matches)) => {
                commands::storage::add(matches);
            }
            Some(("list", _)) => {
                commands::storage::list();
            }
            Some(("remove", matches)) => {
                commands::storage::remove(matches);
            }
            Some(("prune", matches)) => commands::storage::prune(matches).await,
            _ => {
                handle_error(
                    "Invalid subcommand! Run 'gib --help' for more information.".to_string(),
                    None,
                );
            }
        },
        _ => {
            handle_error(
                "Invalid command! Run 'gib --help' for more information.".to_string(),
                None,
            );
        }
    }
}
