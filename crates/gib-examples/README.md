# Gib SDK manual examples

These examples exercise the repository lifecycle through the public `gib-sdk`
API. Each file is an independent manual check:

```text
cargo run -p gib-examples --example initialize_repository -- /tmp/gib-repository
cargo run -p gib-examples --example open_repository -- /tmp/gib-repository
cargo run -p gib-examples --example inspect_repository -- /tmp/gib-repository
cargo run -p gib-examples --example publish_head -- /tmp/gib-head-example
cargo run -p gib-examples --example corrupt_repository -- /tmp/gib-repository show-bytes
cargo run -p gib-examples --example corrupt_repository -- /tmp/gib-repository unsupported-version
cargo run -p gib-examples --example corrupt_repository -- /tmp/gib-repository truncate
```

The `format` and `config/repository` files contain binary MessagePack bytes.
When snapshots are published, `refs/latest` contains the versioned,
integrity-checked HEAD record.
The `publish_head` example creates two raw placeholder objects and publishes
them in sequence; it demonstrates HEAD publication only, not snapshot
construction. Run it with a new repository path.
The corruption example intentionally edits the descriptor for manual negative
tests. Recreate the temporary directory before running the next successful
open check.
