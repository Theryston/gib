use gib::{FORMAT_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| "missing repository path; pass it as the first argument".to_string())?;
    let mode = std::env::args().nth(2).ok_or_else(|| {
        "missing mode; use unsupported-version, truncate, show-bytes, or show-roots".to_string()
    })?;
    let descriptor_path = path.join(REPOSITORY_DESCRIPTOR_OBJECT_KEY);

    match mode.as_str() {
        "unsupported-version" => {
            let bytes = fs::read(&descriptor_path)?;
            let modified = replace_u16_field(&bytes, b"descriptor_version", u16::MAX)?;
            fs::write(&descriptor_path, modified)?;
            println!("Set descriptor version to an unsupported value.");
        }
        "truncate" => {
            fs::write(&descriptor_path, [0x87])?;
            println!("Truncated {}.", descriptor_path.display());
        }
        "show-bytes" => {
            let format_path = path.join(FORMAT_OBJECT_KEY);
            println!("format bytes: {}", hex(&fs::read(format_path)?));
            println!("descriptor bytes: {}", hex(&fs::read(&descriptor_path)?));
        }
        "show-roots" => {
            println!("format object: {FORMAT_OBJECT_KEY}");
            println!("descriptor object: {REPOSITORY_DESCRIPTOR_OBJECT_KEY}");
        }
        _ => {
            return Err(
                "unknown mode; use unsupported-version, truncate, show-bytes, or show-roots".into(),
            );
        }
    }
    Ok(())
}

fn replace_u16_field(bytes: &[u8], field: &[u8], value: u16) -> Result<Vec<u8>, Box<dyn Error>> {
    let field_start = bytes
        .windows(field.len())
        .position(|window| window == field)
        .ok_or("descriptor field was not found")?;
    let value_start = field_start + field.len();
    let value_end = encoded_integer_end(bytes, value_start)?;
    let mut modified = Vec::with_capacity(bytes.len() + 2);
    modified.extend_from_slice(&bytes[..value_start]);
    modified.extend_from_slice(&[0xcd, (value >> 8) as u8, value as u8]);
    modified.extend_from_slice(&bytes[value_end..]);
    Ok(modified)
}

fn encoded_integer_end(bytes: &[u8], start: usize) -> Result<usize, Box<dyn Error>> {
    let marker = *bytes
        .get(start)
        .ok_or("descriptor version value is missing")?;
    let width = match marker {
        0x00..=0x7f | 0xe0..=0xff => 1,
        0xcc => 2,
        0xcd => 3,
        0xce => 5,
        0xcf => 9,
        0xd0 => 2,
        0xd1 => 3,
        0xd2 => 5,
        0xd3 => 9,
        _ => return Err("descriptor version is not an integer".into()),
    };
    start
        .checked_add(width)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| "descriptor version value is truncated".into())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}
