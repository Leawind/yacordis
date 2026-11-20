# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/Leawind/yacordis/compare/yacordis-loader-v0.1.0...yacordis-loader-v0.1.1) - 2026-11-20

### Added

- unload component libraries for real

### Fixed

- skip the component lint when its rule tree is unwritable
- sweep stale context handles at activity and unload

### Other

- remove dead code and stale allow attributes
- version crates independently
- apply dprint
- remove repository-document references from Rust code
