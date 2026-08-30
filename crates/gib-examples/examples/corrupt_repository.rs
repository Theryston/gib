use gib::{FORMAT_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| "missing repository path; pass it as the first argument".to_string())?;
    let mode = std::env::args()
        .nth(2)
        .ok_or_else(|| "missing mode; use unsupported-version or truncate".to_string())?;
    let descriptor_path = path.join(REPOSITORY_DESCRIPTOR_OBJECT_KEY);

    match mode.as_str() {
        "unsupported-version" => {
            let bytes = fs::read(&descriptor_path)?;
            let descriptor = String::from_utf8(bytes)?;
            let modified =
                descriptor.replace("\"descriptor_version\":1", "\"descriptor_version\":65535");
            if modified == descriptor {
                return Err("descriptor version field was not found".into());
            }
            fs::write(&descriptor_path, modified.as_bytes())?;
            println!("Set descriptor version to an unsupported value.");
        }
        "truncate" => {
            fs::write(&descriptor_path, b"{\"magic\":")?;
            println!("Truncated {}.", descriptor_path.display());
        }
        "show-roots" => {
            println!("format object: {FORMAT_OBJECT_KEY}");
            println!("descriptor object: {REPOSITORY_DESCRIPTOR_OBJECT_KEY}");
        }
        _ => return Err("unknown mode; use unsupported-version, truncate, or show-roots".into()),
    }
    Ok(())
}
