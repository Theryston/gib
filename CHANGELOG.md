# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
