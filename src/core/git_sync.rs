use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Describes the parts of nested Git repositories that are safe to include in
/// a live backup. Git history is kept, while machine-local bookkeeping is
/// excluded so that Git cannot turn a remote restore into another live change.
#[derive(Debug, Default)]
pub(crate) struct GitSyncPolicy {
    root: PathBuf,
    repositories: Vec<GitRepository>,
    git_directories: BTreeSet<String>,
    git_file_markers: BTreeSet<String>,
}

#[derive(Debug)]
struct GitRepository {
    git_dir_relative: String,
    marker_is_file: bool,
}

impl GitSyncPolicy {
    pub(crate) fn discover(root: &Path, ignore_patterns: &[String]) -> Result<Self, String> {
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut repositories = Vec::new();
        let mut directories = vec![canonical_root.clone()];

        while let Some(directory) = directories.pop() {
            if !directory.is_dir() {
                continue;
            }

            let git_marker = directory.join(".git");
            if !is_ignored_by_user(&git_marker, &canonical_root, ignore_patterns) {
                match fs::symlink_metadata(&git_marker) {
                    Ok(metadata) if metadata.is_dir() || metadata.is_file() => {
                        let git_dir_relative = relative_path(&canonical_root, &git_marker)?;
                        repositories.push(GitRepository {
                            git_dir_relative,
                            marker_is_file: metadata.is_file(),
                        });
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Failed to inspect Git metadata '{}': {}",
                            git_marker.display(),
                            error
                        ));
                    }
                }
            }

            for entry in fs::read_dir(&directory).map_err(|error| {
                format!(
                    "Failed to inspect directory while discovering Git repositories '{}': {}",
                    directory.display(),
                    error
                )
            })? {
                let entry = entry.map_err(|error| {
                    format!(
                        "Failed to inspect directory entry in '{}': {}",
                        directory.display(),
                        error
                    )
                })?;
                let path = entry.path();
                let file_type = entry.file_type().map_err(|error| {
                    format!("Failed to inspect '{}': {}", path.display(), error)
                })?;

                if !file_type.is_dir() || file_type.is_symlink() {
                    continue;
                }
                if entry.file_name() == ".git"
                    || is_ignored_by_user(&path, &canonical_root, ignore_patterns)
                {
                    continue;
                }

                directories.push(path);
            }
        }

        repositories.sort_by(|left, right| {
            left.git_dir_relative
                .cmp(&right.git_dir_relative)
                .then(left.marker_is_file.cmp(&right.marker_is_file))
        });
        repositories.dedup_by(|left, right| {
            left.git_dir_relative == right.git_dir_relative
                && left.marker_is_file == right.marker_is_file
        });

        let git_directories = repositories
            .iter()
            .filter(|repository| !repository.marker_is_file)
            .map(|repository| repository.git_dir_relative.clone())
            .collect();
        let git_file_markers = repositories
            .iter()
            .filter(|repository| repository.marker_is_file)
            .map(|repository| repository.git_dir_relative.clone())
            .collect();

        Ok(Self {
            root: canonical_root,
            repositories,
            git_directories,
            git_file_markers,
        })
    }

    pub(crate) fn repository_count(&self) -> usize {
        self.repositories.len()
    }

    pub(crate) fn is_volatile_path(&self, root: &Path, path: &Path) -> bool {
        let relative = path
            .strip_prefix(root)
            .or_else(|_| path.strip_prefix(&self.root))
            .ok()
            .map(path_to_string)
            .or_else(|| {
                let canonical_path = fs::canonicalize(path).ok()?;
                canonical_path
                    .strip_prefix(&self.root)
                    .ok()
                    .map(path_to_string)
            });

        relative.is_some_and(|relative| self.is_volatile_relative(&relative))
    }

    pub(crate) fn is_volatile_relative(&self, relative: &str) -> bool {
        let relative = normalize_relative_path(relative);
        let components = relative.split('/').collect::<Vec<_>>();
        let Some(git_component_index) =
            components.iter().position(|component| *component == ".git")
        else {
            return false;
        };

        let git_dir_relative = components[..=git_component_index].join("/");
        if self.git_file_markers.contains(&git_dir_relative) {
            return relative == git_dir_relative;
        }
        if !self.git_directories.contains(&git_dir_relative) {
            return false;
        }

        let Some(within_git_dir) = components.get(git_component_index + 1..) else {
            return false;
        };
        is_volatile_git_relative(&within_git_dir.join("/"))
    }
}

fn is_volatile_git_relative(relative: &str) -> bool {
    let components = relative.split('/').collect::<Vec<_>>();
    let Some(first) = components.first().copied() else {
        return false;
    };

    if matches!(
        first,
        "HEAD"
            | "index"
            | "config"
            | "description"
            | "FETCH_HEAD"
            | "ORIG_HEAD"
            | "MERGE_HEAD"
            | "CHERRY_PICK_HEAD"
            | "REVERT_HEAD"
            | "AUTO_MERGE"
            | "MERGE_MSG"
            | "COMMIT_EDITMSG"
            | "MERGE_RR"
            | "commondir"
            | "gitdir"
            | "gc.log"
    ) {
        return true;
    }

    if matches!(
        first,
        "logs" | "hooks" | "worktrees" | "rebase-apply" | "rebase-merge" | "sequencer" | "rr-cache"
    ) {
        return true;
    }

    if first == "info" && components.get(1).copied() == Some("exclude") {
        return true;
    }

    components
        .iter()
        .any(|component| is_transient_git_name(component))
}

fn is_transient_git_name(name: &str) -> bool {
    name.ends_with(".lock")
        || name.ends_with(".new")
        || name.starts_with("tmp_")
        || name.starts_with("tmp.")
        || name.contains(".gib-")
}

fn is_ignored_by_user(path: &Path, root: &Path, ignore_patterns: &[String]) -> bool {
    if ignore_patterns.is_empty() {
        return false;
    }

    let components = path
        .strip_prefix(root)
        .ok()
        .map(|relative| {
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    components
        .iter()
        .any(|component| ignore_patterns.iter().any(|pattern| pattern == component))
}

fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(root)
        .map(path_to_string)
        .map_err(|error| {
            format!(
                "Failed to derive a relative Git metadata path for '{}': {}",
                path.display(),
                error
            )
        })
}

fn path_to_string(path: &Path) -> String {
    normalize_relative_path(&path.to_string_lossy())
}

fn normalize_relative_path(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gib-git-policy-test-{suffix}"));
        fs::create_dir_all(&path).expect("temporary directory should be created");
        path
    }

    #[test]
    fn discovers_git_repositories_at_any_depth() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("project-one/.git/objects/aa")).unwrap();
        fs::create_dir_all(root.join("group/project-two/source/.git/refs/heads")).unwrap();
        fs::create_dir_all(root.join("plain-folder/nested")).unwrap();

        let policy = GitSyncPolicy::discover(&root, &[]).unwrap();

        assert_eq!(policy.repository_count(), 2);
        assert!(!policy.is_volatile_relative("project-one/.git/objects/aa/hash"));
        assert!(!policy.is_volatile_relative("group/project-two/source/.git/refs/heads/main"));
        assert!(!policy.is_volatile_relative("plain-folder/file.txt"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_history_and_refs_but_filters_machine_local_git_state() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("app/.git")).unwrap();
        let policy = GitSyncPolicy::discover(&root, &[]).unwrap();

        for path in [
            "app/.git/objects/aa/object",
            "app/.git/objects/pack/pack-file.pack",
            "app/.git/refs/heads/main",
            "app/.git/refs/tags/v1",
            "app/.git/packed-refs",
        ] {
            assert!(
                !policy.is_volatile_relative(path),
                "{path} should be synced"
            );
        }

        for path in [
            "app/.git/HEAD",
            "app/.git/index",
            "app/.git/index.lock",
            "app/.git/logs/HEAD",
            "app/.git/hooks/pre-commit",
            "app/.git/refs/heads/main.lock",
            "app/.git/.index.gib-123-456",
            "app/.git/objects/pack/tmp_pack_123",
        ] {
            assert!(
                policy.is_volatile_relative(path),
                "{path} should be local state"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn respects_an_explicit_user_ignore_for_a_nested_repository() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("ignored/.git")).unwrap();
        let policy = GitSyncPolicy::discover(&root, &["ignored".to_string()]).unwrap();

        assert_eq!(policy.repository_count(), 0);
        assert!(!policy.is_volatile_relative("ignored/.git/index"));

        let _ = fs::remove_dir_all(root);
    }
}
