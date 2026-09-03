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
