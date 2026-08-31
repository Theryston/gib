# Gib SDK manual examples

These examples exercise the repository lifecycle through the public `gib-sdk`
API. Each file is an independent manual check:

```text
cargo run -p gib-examples --example initialize_repository -- /tmp/gib-repository
cargo run -p gib-examples --example open_repository -- /tmp/gib-repository
cargo run -p gib-examples --example inspect_repository -- /tmp/gib-repository
cargo run -p gib-examples --example inspect_configuration -- tests/fixtures/configuration/minimal.toml
cargo run -p gib-examples --example inspect_configuration -- tests/fixtures/configuration/complete.toml
cargo run -p gib-examples --example publish_head -- /tmp/gib-head-example
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa smoke
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa conflict
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

The configuration example only reads the supplied `gib.toml` and prints the
validated SDK configuration. It also prints the process directory so relative
path resolution can be checked from a nested directory. To run it from a
different directory, pass the workspace manifest explicitly:

```text
cd /tmp
cargo run --manifest-path /home/theryston/code/gib/Cargo.toml \
  -p gib-examples --example inspect_configuration -- \
  /home/theryston/code/gib/tests/fixtures/configuration/complete.toml
```

For negative checks, replace `complete.toml` with `unknown-field.toml`,
`unsupported-version.toml`, or `malformed.toml` and inspect the typed error
context printed by the example.

The local storage QA example exercises upload, prefix listing, whole-object
read, range read, deletion, and conditional-writer conflict handling. Its
`hold-write` mode intentionally slows a large streaming write so it can be
interrupted from another terminal:

```text
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa smoke
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa conflict
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa hold-write 1073741824 10
```
