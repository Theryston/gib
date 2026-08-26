use sha2::{Digest, Sha256};

pub(crate) fn normalize_relative_path(path: &str) -> Result<String, String> {
    if path.contains('\0') {
        return Err("Catalog paths cannot contain NUL bytes".to_string());
    }

    let path = path.replace('\\', "/");
    if path.starts_with('/') || path.starts_with("//") {
        return Err(format!("Catalog path must be relative: {}", path));
    }

    let first_component = path.split('/').next().unwrap_or_default();
    if first_component.len() >= 2
        && first_component.as_bytes()[1] == b':'
        && first_component.as_bytes()[0].is_ascii_alphabetic()
    {
        return Err(format!("Catalog path must be relative: {}", path));
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(format!("Catalog path cannot contain '..': {}", path));
            }
            component => components.push(component),
        }
    }

    Ok(components.join("/"))
}

pub(crate) fn normalize_file_path(path: &str) -> Result<String, String> {
    let normalized = normalize_relative_path(path)?;
    if normalized.is_empty() {
        return Err("Catalog file paths cannot be empty".to_string());
    }
    Ok(normalized)
}

pub(crate) fn lookup_path(path: &str) -> String {
    path.to_lowercase()
}

pub(crate) fn entry_id(path: &str) -> String {
    sha256_hex(path.as_bytes())
}

pub(crate) fn directory_id(path: &str) -> String {
    sha256_hex(path.as_bytes())
}

pub(crate) fn shard_id(identifier: &str) -> String {
    let digest = Sha256::digest(identifier.as_bytes());
    format!("{:02x}{:02x}", digest[0], digest[1])
}

pub(crate) fn revision_id(entry_id: &str, backup_hash: &str) -> String {
    sha256_hex(format!("{}:{}", entry_id, backup_hash).as_bytes())
}

pub(crate) fn parent_directory(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

pub(crate) fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub(crate) fn directory_paths(path: &str) -> Vec<String> {
    let components: Vec<&str> = path.split('/').collect();
    let mut directories = vec![String::new()];

    if components.len() > 1 {
        for index in 0..components.len() - 1 {
            directories.push(components[..=index].join("/"));
        }
    }

    directories
}

pub(crate) fn path_tokens(path: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for character in path.chars() {
        if character.is_alphanumeric() {
            current.extend(character.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens.sort();
    tokens.dedup();
    tokens
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_paths_without_losing_case() {
        assert_eq!(
            normalize_file_path("./Src\\Main.rs").unwrap(),
            "Src/Main.rs"
        );
        assert_eq!(lookup_path("Src/Main.rs"), "src/main.rs");
    }

    #[test]
    fn rejects_absolute_and_traversal_paths() {
        for path in ["/tmp/file", "C:/tmp/file", "../file", "a/../../file"] {
            assert!(
                normalize_file_path(path).is_err(),
                "{} should be rejected",
                path
            );
        }
    }

    #[test]
    fn uses_stable_hashes_for_ids_and_shards() {
        assert_eq!(entry_id("a.txt").len(), 64);
        assert_eq!(directory_id("").len(), 64);
        assert_eq!(shard_id(&entry_id("a.txt")).len(), 4);
        assert_eq!(entry_id("a.txt"), entry_id("a.txt"));
    }

    #[test]
    fn builds_virtual_parent_paths_and_tokens() {
        assert_eq!(directory_paths("src/bin/main.rs"), ["", "src", "src/bin"]);
        assert_eq!(path_tokens("Src/My-file.rs"), ["file", "my", "rs", "src"]);
    }
}
