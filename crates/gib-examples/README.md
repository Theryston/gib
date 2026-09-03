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

The filesystem scanner QA example supports the shared Backup/Live ignore
policy. `scan` prints included entries, while `decide` prints an inclusion or
ignore reason for one normalized relative path:

```text
cargo run -p gib-examples --example filesystem_scan_qa -- \
  scan /path/to/source --ignore node_modules --ignore 'src/*.generated'
cargo run -p gib-examples --example filesystem_scan_qa -- \
  scan /path/to/source --config /path/to/gib.toml --ignore cli-only
cargo run -p gib-examples --example filesystem_scan_qa -- \
  decide nested/.git/HEAD
cargo run -p gib-examples --example filesystem_scan_qa -- \
  decide nested/.git/HEAD --no-ignore-git
```

`--ignore` may be repeated. A bare name matches at any depth; a path pattern
is source-root anchored. `--config` loads the file and merges its
`[backup].ignore` values with the command-line rules. Use
`--no-ignore-git` to opt into `.git` capture.

The local storage QA example exercises upload, prefix listing, whole-object
read, range read, deletion, and conditional-writer conflict handling. Its
`hold-write` mode intentionally slows a large streaming write so it can be
interrupted from another terminal:

```text
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa smoke
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa conflict
cargo run -p gib-examples --example local_storage_qa -- /tmp/gib-local-qa hold-write 1073741824 10
```

The S3 storage QA example requires the `s3` feature and explicit credentials.
It works with AWS S3 and S3-compatible services. The endpoint is optional for
standard AWS S3 and should be set to the service URL for MinIO or LocalStack:

```text
export GIB_S3_REGION=us-east-1
export GIB_S3_BUCKET=gib-qa
export GIB_S3_ACCESS_KEY=minioadmin
export GIB_S3_SECRET_KEY=minioadmin
export GIB_S3_ENDPOINT=http://127.0.0.1:9000
export GIB_S3_CAPABILITY_CACHE_PATH=/tmp/gib-s3-capabilities.msgpack
cargo run -p gib-examples --features s3 --example s3_storage_qa -- all
```

The commands `capabilities`, `reprobe`, `atomic`, `smoke`, `multipart`,
`paginate`, and `cancel` run checks individually. `capabilities` probes both
conditional-write forms on a cache miss and reports when a persisted result was
loaded. Run it again as a new process to verify the cache hit. `reprobe`
invalidates the configured endpoint/bucket entry and probes again. `atomic`
attempts a conditional publication and reports a safe refusal when the
endpoint does not support it. `GIB_S3_SESSION_TOKEN`,
`GIB_S3_CAPABILITY_CACHE_PATH`, `GIB_S3_MULTIPART_THRESHOLD`,
`GIB_S3_MULTIPART_PART_SIZE`, and `GIB_S3_MAX_CONCURRENCY` are optional. The
cancel command verifies that no usable object remains; use the provider CLI to
inspect multipart uploads and confirm that the abort request removed any
incomplete upload, for example:

```text
aws --endpoint-url "$GIB_S3_ENDPOINT" s3api list-multipart-uploads --bucket "$GIB_S3_BUCKET"
```

The S3 contract integration tests use the same adapter with an isolated object
namespace. Set `GIB_S3_TEST_REGION`, `GIB_S3_TEST_BUCKET`,
`GIB_S3_TEST_ACCESS_KEY`, and `GIB_S3_TEST_SECRET_KEY` (plus the optional
`GIB_S3_TEST_ENDPOINT` and `GIB_S3_TEST_SESSION_TOKEN`) before running:

```text
cargo test -p gib-sdk --features s3 --test storage_contract --no-fail-fast
```

For the capability-detection manual QA, run `capabilities` twice as separate
processes, remove or corrupt `GIB_S3_CAPABILITY_CACHE_PATH`, run
`capabilities` again, and then run `atomic` against an endpoint known not to
support native preconditions. A refusal must report
`conditional writes are unsupported`; it must not publish the object.

The WebDAV storage QA example requires the `webdav` feature, an existing
dedicated WebDAV collection, and Basic-auth credentials. HTTPS is required by
default; set `GIB_WEBDAV_ALLOW_HTTP=true` only for an intentionally insecure
local test server:

```text
docker run --rm --name gib-webdav-qa \
  -e AUTH_TYPE=Basic -e USERNAME=gib-qa -e PASSWORD=test-password \
  -e LOCATION=/dav --publish 8080:80 --detach bytemark/webdav

export GIB_WEBDAV_URL=http://127.0.0.1:8080/dav/
export GIB_WEBDAV_USERNAME=gib-qa
export GIB_WEBDAV_PASSWORD='test-password'
export GIB_WEBDAV_ALLOW_HTTP=true
cargo run -p gib-examples --features webdav --example webdav_storage_qa -- all
```

The container command is a local-only QA setup; use HTTPS for real data. The
image's `LOCATION`, Basic-auth, and persistent-volume options are documented by
[the image maintainer](https://github.com/BytemarkHosting/docker-webdav).

The commands `smoke`, `unicode`, `ranges`, `paginate`, `cancel`, `redaction`,
and `auth` run the WebDAV checks individually. `smoke` covers CRUD and a byte
range, `unicode` covers encoded nested names, `ranges` checks a large range
crossing a transfer-buffer boundary, `paginate` checks continuation cursors,
`cancel` interrupts a large staged upload, and `auth` checks the typed error for
bad credentials. The WebDAV contract integration test uses the same settings
with `GIB_WEBDAV_TEST_URL`, `GIB_WEBDAV_TEST_USERNAME`, and
`GIB_WEBDAV_TEST_PASSWORD`:

```text
export GIB_WEBDAV_TEST_URL="$GIB_WEBDAV_URL"
export GIB_WEBDAV_TEST_USERNAME="$GIB_WEBDAV_USERNAME"
export GIB_WEBDAV_TEST_PASSWORD="$GIB_WEBDAV_PASSWORD"
export GIB_WEBDAV_TEST_ALLOW_HTTP="${GIB_WEBDAV_ALLOW_HTTP:-}"
cargo test -p gib-sdk --features webdav --test storage_contract --no-fail-fast
```

The SDK also has focused adversarial tests for malformed DAV XML, encoded
traversal, cross-origin hrefs, root validation, and credential redaction:

```text
cargo test -p gib-sdk --features webdav webdav --lib --no-fail-fast
```

The secure storage-configuration QA example uses an in-memory credential-store
double and recognizable fake values. It verifies that the MessagePack record
contains only non-secret settings and an opaque reference, forces an update
failure, and removes both record and credential in one process:

```text
cargo run -p gib-examples --example storage_configuration_qa -- /tmp/gib-storage-configuration-qa all
```

To inspect each phase separately, keep the same directory and run:

```text
cargo run -p gib-examples --example storage_configuration_qa -- /tmp/gib-storage-configuration-qa add
rg -a -n 'manual-recognizable-(access-key|secret-key|session-token)' /tmp/gib-storage-configuration-qa
cargo run -p gib-examples --example storage_configuration_qa -- /tmp/gib-storage-configuration-qa inspect
cargo run -p gib-examples --example storage_configuration_qa -- /tmp/gib-storage-configuration-qa fail-update
cargo run -p gib-examples --example storage_configuration_qa -- /tmp/gib-storage-configuration-qa remove
```

The separate-process commands intentionally use the in-memory double only for
file-safety checks; a real application must inject its approved encrypted
credential-store adapter to reload credentials across processes.
