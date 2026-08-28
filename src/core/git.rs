pub(crate) fn is_git_path(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .any(|component| component.eq_ignore_ascii_case(".git"))
}

#[cfg(test)]
mod tests {
    use super::is_git_path;

    #[test]
    fn recognizes_git_paths_at_any_depth_and_with_both_separators() {
        assert!(is_git_path(".git/HEAD"));
        assert!(is_git_path("projects/app/.git/objects/aa/object"));
        assert!(is_git_path(r"projects\app\.GIT\HEAD"));
        assert!(!is_git_path("projects/app/git/HEAD"));
        assert!(!is_git_path("projects/app/.gitignore"));
    }
}
