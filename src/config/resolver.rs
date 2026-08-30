use super::model::LocalConfigContext;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) fn resolve_path(
    explicit: Option<&Path>,
    configured: Option<&str>,
    context: &LocalConfigContext,
    working_dir: &Path,
) -> PathBuf {
    let (value, base) = match (explicit, configured) {
        (Some(value), _) => (value, working_dir),
        (None, Some(value)) => (Path::new(value), context.base_dir.as_path()),
        (None, None) => return working_dir.to_path_buf(),
    };
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    }
}

pub(crate) fn merge_ignore_patterns(configured: &[String], explicit: &[String]) -> Vec<String> {
    configured
        .iter()
        .chain(explicit)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
