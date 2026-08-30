# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.46](https://github.com/Theryston/gib/compare/gib-sdk-v0.0.45...gib-sdk-v0.0.46) - 2026-08-30

### Fixed

- authorize release tag creation
- restore gib-sdk release pipeline

### Other

- Update AGENTS.md

## [0.0.45](https://github.com/Theryston/gib/compare/v0.0.44...v0.0.45) - 2026-08-30

### Added

- add cross-platform support for autostart secret protection
- add cargo test to release workflow

## [0.0.44](https://github.com/Theryston/gib/compare/v0.0.43...v0.0.44) - 2026-08-30

### Added

- install ripgrep for architecture check workflow
- lib API

## [0.0.43](https://github.com/Theryston/gib/compare/v0.0.42...v0.0.43) - 2026-08-29

### Added

- show loading spinner on backup start in interactive mode
- refine progress bar lifecycle in backup and restore commands

## [0.0.42](https://github.com/Theryston/gib/compare/v0.0.41...v0.0.42) - 2026-08-28

### Added

- parallelize catalog shard updates and optimize S3 reads

## [0.0.41](https://github.com/Theryston/gib/compare/v0.0.40...v0.0.41) - 2026-08-28

### Added

- protect .git directories from local cleanup and live sync

## [0.0.40](https://github.com/Theryston/gib/compare/v0.0.39...v0.0.40) - 2026-08-28

### Added

- replace Git sync policy with simple .git ignore flag

## [0.0.39](https://github.com/Theryston/gib/compare/v0.0.38...v0.0.39) - 2026-08-28

### Added

- add no-start flag to autostart commands

## [0.0.38](https://github.com/Theryston/gib/compare/v0.0.37...v0.0.38) - 2026-08-28

### Added

- cache S3 conditional write capability detection
- support custom S3 endpoints with path-style addressing

## [0.0.37](https://github.com/Theryston/gib/compare/v0.0.36...v0.0.37) - 2026-08-28

### Added

- remove redundant backup status lines in interactive logs
- use progress bar for live autostart logs
- add interactive renderer for live log display

## [0.0.36](https://github.com/Theryston/gib/compare/v0.0.35...v0.0.36) - 2026-08-27

### Added

- expand README with live backup options and detailed sections

### Other

- Update README-tmp.md

## [0.0.35](https://github.com/Theryston/gib/compare/v0.0.34...v0.0.35) - 2026-08-27

### Other

- add top-level quick start

## [0.0.34](https://github.com/Theryston/gib/compare/v0.0.33...v0.0.34) - 2026-08-27

### Other

- add temporary marketing README

## [0.0.33](https://github.com/Theryston/gib/compare/v0.0.32...v0.0.33) - 2026-08-26

### Added

- replace file identity fields with creation time on Windows

## [0.0.32](https://github.com/Theryston/gib/compare/v0.0.31...v0.0.32) - 2026-08-26

### Added

- add autostart log following command

## [0.0.31](https://github.com/Theryston/gib/compare/v0.0.30...v0.0.31) - 2026-08-26

### Added

- add WebDAV storage support

### Other

- Merge branch 'main' of github.com:Theryston/gib

## [0.0.30](https://github.com/Theryston/gib/compare/v0.0.29...v0.0.30) - 2026-08-26

### Added

- improve explorer status display with user-friendly wording
- rank search results by relevance and support partial token matches
- mark restorable deleted directories in explorer navigation
- support parentless snapshot correction in read-only catalog
- add explore command for browsing historical catalog

## [0.0.29](https://github.com/Theryston/gib/compare/v0.0.28...v0.0.29) - 2026-08-26

### Added

- add search command for historical catalog queries

### Other

- Fix punctuation in README description

## [0.0.28](https://github.com/Theryston/gib/compare/v0.0.27...v0.0.28) - 2026-08-26

### Added

- add automatic historical catalog for backup metadata

## [0.0.27](https://github.com/Theryston/gib/compare/v0.0.26...v0.0.27) - 2026-08-26

### Added

- sync Git history during live backups

## [0.0.26](https://github.com/Theryston/gib/compare/v0.0.25...v0.0.26) - 2026-08-26

### Fixed

- handle stale backup references in live sync cache

## [0.0.25](https://github.com/Theryston/gib/compare/v0.0.24...v0.0.25) - 2026-08-26

### Added

- add incremental backup support for live sync

## [0.0.24](https://github.com/Theryston/gib/compare/v0.0.23...v0.0.24) - 2026-08-25

### Added

- fix systemd escaping and persist effective repository values
- autostart

## [0.0.23](https://github.com/Theryston/gib/compare/v0.0.22...v0.0.23) - 2026-08-25

### Added

- add conflict resolution policy flag to live command

## [0.0.22](https://github.com/Theryston/gib/compare/v0.0.21...v0.0.22) - 2026-08-25

### Added

- rename watch command to live
- add watch mode polling and repository reconciliation

## [0.0.21](https://github.com/Theryston/gib/compare/v0.0.20...v0.0.21) - 2026-08-25

### Other

- format AGENTS.md for better readability

## [0.0.20](https://github.com/Theryston/gib/compare/v0.0.19...v0.0.20) - 2026-08-25

### Added

- support latest backup reference across commands

## [0.0.19](https://github.com/Theryston/gib/compare/v0.0.18...v0.0.19) - 2026-08-25

### Added

- add local gib.toml configuration support

### Other

- Merge branch 'main' of github.com:Theryston/gib

## [0.0.18](https://github.com/Theryston/gib/compare/v0.0.17...v0.0.18) - 2026-08-25

### Added

- add watch command for automatic incremental backups

### Other

- Merge branch 'main' of github.com:Theryston/gib

## [0.0.17](https://github.com/Theryston/gib/compare/v0.0.16...v0.0.17) - 2026-08-25

### Added

- clarify storage detection in setup command

## [0.0.16](https://github.com/Theryston/gib/compare/v0.0.15...v0.0.16) - 2026-08-25

### Added

- improve setup summary output readability
- add setup command to discover and register local repositories

## [0.0.15](https://github.com/Theryston/gib/compare/v0.0.14...v0.0.15) - 2026-05-14

### Added

- add --parent support to backups

### Other

- Update README installer URLs to trygib.org and delete install scripts

## [0.0.14](https://github.com/Theryston/gib/compare/v0.0.13...v0.0.14) - 2026-01-29

### Added

- add pending subcommand to list pending backups for a repository
- add interactive search to restore file selector
- add selective restore with --only option

### Other

- Add README tip to list pending backups with gib backup pending
- --only path selection logic to core module
- Update README with selective restore and resume features

## [0.0.13](https://github.com/Theryston/gib/compare/v0.0.12...v0.0.13) - 2026-01-29

### Added

- add configurable concurrency to backup command

## [0.0.12](https://github.com/Theryston/gib/compare/v0.0.11...v0.0.12) - 2026-01-28

### Other

- remove redundant comment in install.sh

## [0.0.11](https://github.com/Theryston/gib/compare/v0.0.10...v0.0.11) - 2026-01-28

### Other

- improve release workflow and add repo scoping for gh

## [0.0.10](https://github.com/Theryston/gib/compare/v0.0.9...v0.0.10) - 2026-01-28

### Other

- release workflow and remove release-pr

## [0.0.9](https://github.com/Theryston/gib/compare/v0.0.8...v0.0.9) - 2026-01-28

### Added

- add release-plz automation and update release workflow

### Fixed

- update release-plz action to v0.5.124
