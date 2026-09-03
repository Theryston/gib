# Filesystem scanning

`gib::local_filesystem_scanner()` creates a scanner for the host filesystem.
The selected root must be a real directory; a symbolic link or Windows
reparse-point root is rejected. The scanner emits the root first and then
returns a depth-first iterator of validated `FilesystemEntry` values.

Each emitted path is a slash-separated `RelativePath`. Every component is
checked against the portable tree-name rules before any child filesystem
operation is attempted. Invalid names, non-UTF-8 names, disappearing entries,
and other adapter failures are returned as iterator errors. They are never
silently omitted.

Symbolic links are inspected with no-follow metadata and their raw targets are
stored in `SymlinkTarget`. Links are emitted but never traversed. The local
adapter also treats Windows reparse points as links. Directory enumeration is
incremental; the scanner keeps only the active directory stack, bounded by
`FilesystemScanOptions::max_open_directories`.

## Ignore policy

Backup and Live capture use the same `gib::IgnorePolicy`. It evaluates only
normalized paths relative to the capture root, always using `/` as the
separator. A policy can be installed with
`FilesystemScanner::with_ignore_policy`, or built from repeated patterns with
`FilesystemScanner::with_ignore_patterns`.

Patterns have two forms:

- A pattern without `/` is a name pattern. It matches that name at any depth.
- A pattern containing `/` is anchored at the capture root and matches the
  named path and its descendants.

Within one component, `*` matches zero or more characters and `?` matches one
character. A complete `**` component matches zero or more complete path
components, so `**/*.tmp` matches temporary files at any depth. Backslashes in
patterns and diagnostic paths are normalized to `/`; absolute paths, traversal
components, empty components, and invalid characters are rejected during
validation. Rules are canonicalized, sorted, and deduplicated when a policy is
built, including separator-equivalent duplicates. The configuration resolver
combines `[backup].ignore` with repeated `--ignore` request values before
building this policy; the resulting order does not depend on source or input
order.

The built-in `.git` rule is enabled by default and matches an exact `.git`
component, case-insensitively, at every depth. `.gitignore`, `git`, and similar
names are not covered. `--no-ignore-git` disables only this built-in capture
rule; an explicit user pattern such as `.git` still excludes those paths.
Ignored directories are filtered before metadata inspection and before
`read_dir`, so their subtrees are not traversed.

`IgnorePolicy::decision` and the scanner's `ignore_decision` methods expose the
matching rule for diagnostics. They contain only the normalized relative path
and never the absolute source root. This capture-selection policy is separate
from destructive cleanup protection: including `.git` in a snapshot must not
authorize later removal of local `.git` paths.

The default permission policy is fail-closed. With
`FilesystemPermissionPolicy::Warn`, a permission error is yielded to the
caller and reachable siblings continue. Other errors stop the scan. A caller
that receives `FilesystemScanError::FileChanged` or
`FilesystemScanError::DirectoryChanged` should discard the affected partial
result and restart the scan or file read; `is_retryable()` identifies these
races.

Regular files should be read through `FilesystemScan::open_file`. The reader
checks the scanned metadata before opening, counts bytes while streaming, and
checks both the opened handle and the path after EOF. Callers must read to EOF
and call `VerifiedFileReader::finish`; a failed verification invalidates the
partial content. The checks detect observable type, identity, size, and
timestamp changes. Detecting an in-place change that leaves every supported
metadata value unchanged would require content hashing, which is outside the
scanner's scope.

The `Filesystem` and `FilesystemClock` traits are public injection points for
deterministic tests and alternate adapters. With the `async` feature,
`FilesystemScanner::scan_async` runs each blocking scan step on Tokio's
blocking pool.
