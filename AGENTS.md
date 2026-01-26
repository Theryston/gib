# Repository Guidelines

## General rules
- After making changes to the codebase, run `cargo check` to ensure there are no errors, if you find errors please fix them.

## Project Structure & Module Organization
- `src/main.rs` defines the CLI entry point and subcommand wiring.
- `src/commands/` holds command handlers (backup, restore, storage, etc.).
- `src/core/` contains backup and repository logic.
- `src/fs/` provides filesystem and path helpers.
- `src/output.rs` and `src/utils.rs` manage output modes and shared utilities.
- `benchmarks/` stores benchmark assets used by the README.
- `sample/` is reserved for local/demo data when testing features manually.

## Build, Test, and Development Commands
```bash
cargo build            # Debug build
cargo run -- --help    # Run the CLI with arguments
cargo build --release  # Optimized release build
```

If you add formatting or lint checks, use the Rust defaults:
```bash
cargo fmt
cargo check
```

## Coding Style & Naming Conventions
- Follow Rust 2024 edition conventions and default `rustfmt` style.
- Use `snake_case` for modules/functions, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants.
- Keep CLI flags in kebab-case and align with existing `clap` patterns.
- Favor clear, descriptive names for storage keys and backup identifiers (e.g., `dev-projects`).

## Commit & Pull Request Guidelines
- Commit messages in this repo use logn an very described messages with an short title and a summary, and should start with prefixes like `feat:` or `fix:` (e.g., `feat: add ignore flag to backup`).
- Keep commits focused and scoped to a single behavior change.

## Release & CI Notes
- The release workflow is tag-driven. Tags matching `v*` (e.g., `v0.0.8`) trigger multi-target release builds.
- Use `cargo build --release` locally to validate the release binary before tagging.
