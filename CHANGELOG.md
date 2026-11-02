# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- add multi-binary `ns dev` with selective restarts, captured and prefixed app output, per-app logs and a searchable TUI

## [0.5.0](https://github.com/dacsang97/naspeed/compare/v0.4.0...v0.5.0) - 2026-11-01

### Added

- support Windows hosts ([#17](https://github.com/dacsang97/naspeed/pull/17))

## [0.4.0](https://github.com/dacsang97/naspeed/compare/v0.3.0...v0.4.0) - 2026-10-31

### Added

- *(test)* run every binary by default and say why binaries ran ([#14](https://github.com/dacsang97/naspeed/pull/14))

### Fixed

- *(test)* start system binaries without the tracer inserted ([#15](https://github.com/dacsang97/naspeed/pull/15))

## [0.3.0](https://github.com/dacsang97/naspeed/compare/v0.2.0...v0.3.0) - 2026-10-31

### Added

- *(test)* track test inputs at run time and close dependency gaps ([#12](https://github.com/dacsang97/naspeed/pull/12))
- *(release)* make dist/ ignore itself ([#13](https://github.com/dacsang97/naspeed/pull/13))

### Other

- add GitHub Actions and AI agents guides ([#10](https://github.com/dacsang97/naspeed/pull/10))

## [0.2.0](https://github.com/dacsang97/naspeed/compare/v0.1.2...v0.2.0) - 2026-10-31

### Added

- add ns check and ns fmt ([#8](https://github.com/dacsang97/naspeed/pull/8))

## [0.1.2](https://github.com/dacsang97/naspeed/compare/v0.1.1...v0.1.2) - 2026-10-31

### Other

- build and test naspeed through naspeed-action ([#6](https://github.com/dacsang97/naspeed/pull/6))

## [0.1.1](https://github.com/dacsang97/naspeed/compare/v0.1.0...v0.1.1) - 2026-10-31

### Other

- add CODEOWNERS and FUNDING files; update status command for color output

## [0.1.0](https://github.com/dacsang97/naspeed/releases/tag/v0.1.0) - 2026-10-31

### Other

- update CI workflows, add release management and Homebrew support ([#3](https://github.com/dacsang97/naspeed/pull/3))
- add vitepress ([#2](https://github.com/dacsang97/naspeed/pull/2))
- add fmt, clippy and check workflow ([#1](https://github.com/dacsang97/naspeed/pull/1))
- Add end-to-end tests for naspeed setup and uninstall functionality
