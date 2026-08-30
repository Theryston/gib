#!/usr/bin/env bash

set -euo pipefail

library_roots=(src/api src/core src/config src/storage)
failed=0

for root in "${library_roots[@]}"; do
    if matches=$(rg -n --glob '*.rs' \
        '(^|::)(clap|dialoguer|indicatif|console|tabled|crossterm)::|extern crate (clap|dialoguer|indicatif|console|tabled|crossterm)|println!|eprintln!|print!|std::process::(exit|abort)|std::env::set_current_dir|tokio::signal::ctrl_c|crate::(cli|commands|output)' \
        "$root" 2>/dev/null); then
        printf 'Forbidden CLI or process behavior found under %s:\n%s\n' "$root" "$matches" >&2
        failed=1
    fi
done

if rg -n --glob '*.rs' '(^|\s)(mod|pub mod) cli|use .*cli' src/lib.rs 2>/dev/null; then
    echo 'src/lib.rs must not declare or import the CLI module' >&2
    failed=1
fi

if ! rg -n '^mod cli;' src/main.rs >/dev/null; then
    echo 'src/main.rs must be the binary target that declares mod cli' >&2
    failed=1
fi

exit "$failed"
