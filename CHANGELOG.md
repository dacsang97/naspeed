# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.13.0](https://github.com/dacsang97/naspeed/compare/v0.12.2...v0.13.0) - 2026-11-21

### Added

- update notice in `ns dev` and an `ns update` command ([#49](https://github.com/dacsang97/naspeed/pull/49))
- *(test)* [test].bins = "scoped" keys tests on own-package binaries only ([#46](https://github.com/dacsang97/naspeed/pull/46))

### Fixed

- *(test)* make the update-notice test survive version bumps ([#50](https://github.com/dacsang97/naspeed/pull/50))
- *(store)* serialize OUT_DIR mutation and refuse entries that cannot restore their advertised paths ([#45](https://github.com/dacsang97/naspeed/pull/45))

## [0.12.2](https://github.com/dacsang97/naspeed/compare/v0.12.1...v0.12.2) - 2026-11-21

### Added

- *(cache)* carry the Go module download cache in cache transfer ([#42](https://github.com/dacsang97/naspeed/pull/42))

## [0.12.1](https://github.com/dacsang97/naspeed/compare/v0.12.0...v0.12.1) - 2026-11-21

### Added

- *(cache)* aux cache reporting + export summary for save-if-changed ([#40](https://github.com/dacsang97/naspeed/pull/40))

## [0.12.0](https://github.com/dacsang97/naspeed/compare/v0.11.0...v0.12.0) - 2026-11-18

### Added

- *(release)* support Go images and private modules ([#38](https://github.com/dacsang97/naspeed/pull/38))

### Added

- *(ci)* configure private Go repositories and proxies with separate file-backed credentials that are always cleaned up

## [0.11.0](https://github.com/dacsang97/naspeed/compare/v0.10.1...v0.11.0) - 2026-11-18

### Added

- *(dev)* support Go projects ([#37](https://github.com/dacsang97/naspeed/pull/37))

### Fixed

- *(dev)* preserve selection while wheel scrolling ([#35](https://github.com/dacsang97/naspeed/pull/35))

### Added

- *(dev)* watch, build, selectively restart and test Go modules and `go.work` workspaces

### Fixed

- *(dev)* preserve mouse text selection while wheel-scrolling beyond the TUI viewport

## [0.10.1](https://github.com/dacsang97/naspeed/compare/v0.10.0...v0.10.1) - 2026-11-16

### Fixed

- *(dev)* handle dropped file events without restarts ([#33](https://github.com/dacsang97/naspeed/pull/33))

## [0.10.0](https://github.com/dacsang97/naspeed/compare/v0.9.0...v0.10.0) - 2026-11-16

### Added

- *(jobserver)* admit rustc by learned peak memory ([#31](https://github.com/dacsang97/naspeed/pull/31))

## [0.9.0](https://github.com/dacsang97/naspeed/compare/v0.8.0...v0.9.0) - 2026-11-15

### Added

- support cargo 1.100's build-dir layout ([#30](https://github.com/dacsang97/naspeed/pull/30))
- *(dev)* sweep the project's target dir while ns dev runs ([#28](https://github.com/dacsang97/naspeed/pull/28))

## [0.8.0](https://github.com/dacsang97/naspeed/compare/v0.7.0...v0.8.0) - 2026-11-04

### Fixed

- *(wrapper)* key workspace crates on declared build script inputs ([#26](https://github.com/dacsang97/naspeed/pull/26))

## [0.7.0](https://github.com/dacsang97/naspeed/compare/v0.6.0...v0.7.0) - 2026-11-02

### Fixed

- *(dev)* stream cargo progress while checking and building ([#25](https://github.com/dacsang97/naspeed/pull/25))
- *(store)* stop filling the store's volume on repeated workspace rebuilds ([#23](https://github.com/dacsang97/naspeed/pull/23))

## [0.6.0](https://github.com/dacsang97/naspeed/compare/v0.5.0...v0.6.0) - 2026-11-02

### Added

- *(dev)* add multi-bin supervision and TUI ([#22](https://github.com/dacsang97/naspeed/pull/22))

### Other

- install steps for Windows ([#20](https://github.com/dacsang97/naspeed/pull/20))

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
