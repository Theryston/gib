use clap::{Arg, Command, arg};

use crate::utils::handle_error;

mod commands;
mod core;
mod fs;
mod utils;

fn cli() -> Command {
    Command::new("gib")
        .about("A blazingly fast, modern backup tool with versioning, deduplication, and encryption.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .allow_external_subcommands(true)
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
                .subcommand(
                    Command::new("delete")
                        .about("Delete a backup and its orphaned chunks")
                        .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                        .arg(arg!(-b --backup <BACKUP> "The backup hash to delete (full hash or first 8 chars)").required(false))
                        .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                        .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                )
        )
        .subcommand(
            Command::new("restore")
                .about("Restore files from a backup")
                .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                .arg(arg!(-b --backup <BACKUP> "The backup hash to restore (full hash or first 8 chars)").required(false))
                .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                .arg(
                    Arg::new("target-path")
                        .short('t')
                        .long("target-path")
                        .value_name("TARGET_PATH")
                        .help("The target directory to restore files to (default: current directory)")
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
                        .about("Prune not used chunks")
                        .arg(arg!(-k --key <KEY> "An unique key for your repository (example: 'my-repository')").required(false))
                        .arg(arg!(-s --storage <STORAGE> "The storage to use").required(false))
                        .arg(arg!(-p --password <PASSWORD> "The password to use for encrypted repositories").required(false))
                )
        )
}

#[tokio::main]
async fn main() {
    let matches = cli().get_matches();

    match matches.subcommand() {
        Some(("config", matches)) => commands::config(matches),
        Some(("whoami", _)) => commands::whoami(),
        Some(("encrypt", matches)) => commands::encrypt(matches).await,
        Some(("log", matches)) => commands::log(matches).await,
        Some(("backup", matches)) => match matches.subcommand() {
            Some(("delete", matches)) => commands::delete(matches).await,
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
