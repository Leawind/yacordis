# Changelog

## [Unreleased]

## [0.1.1](https://github.com/Leawind/yacordis/compare/yacordis-v0.1.0...yacordis-v0.1.1) - 2026-11-20

### Added

- unload component libraries for real

### Fixed

- bound the quiesce wait and emit logs outside the sink lock
- let the work executor thread retire with its runtime
- serialize registry and store lock acquisition order
- keep internal/service inside its isolation group
- honor declared ordering in activation and global disposal

### Other

- record the runtime fixes in the changelog
- remove dead code and stale allow attributes
- version crates independently
- fold the logger service into the core crate
- specify readme for crate yacordis
- apply dprint
- remove repository-document references from Rust code

### Added

- The leveled logging service ships with the core (previously the separate yacordis-logger crate): leveled output with pluggable sinks, component-name scopes, and level or sink changes scoped to the calling fiber that revert when it unloads.

### Changed

- `quiesce` and `Context::quiesce` share one bounded wait (30 s): a lifecycle op stuck in user code surfaces as a report — the runtime variant dumps every fiber's state — instead of hanging the caller indefinitely.

### Fixed

- Logger sinks are called outside the sink-registry lock, so a sink that re-enters the service (changing the level, adding another sink) cannot deadlock the emission that called it.
