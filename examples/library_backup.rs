use gib::api::{BackupRequest, Gib, RepositoryRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gib = Gib::builder()
        .data_dir(".gib")
        .working_dir(".")
        .discover_config(false)
        .build()?;

    let request = BackupRequest::new(
        RepositoryRequest::new("my-project", "local"),
        ".",
        "Application backup",
        "Your Name <you@example.com>",
    );
    let result = gib.backup(request).await?;
    println!("{}", result.backup.hash);
    Ok(())
}
