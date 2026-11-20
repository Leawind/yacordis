# Changelog

## [Unreleased]

## [0.2.1](https://github.com/Leawind/yacordis/compare/yacordis-v0.2.0...yacordis-v0.2.1) - 2026-11-20

### Other

- updated the following local packages: yacordis-macros

## [0.2.0] - 2026-11-20

### Added

- The leveled logging service ships with the core (previously the separate yacordis-logger crate): leveled output with pluggable sinks, component-name scopes, and level or sink changes scoped to the calling fiber that revert when it unloads.

### Changed

- `quiesce` and `Context::quiesce` share one bounded wait (30 s): a lifecycle op stuck in user code surfaces as a report — the runtime variant dumps every fiber's state — instead of hanging the caller indefinitely.

### Fixed

- Logger sinks are called outside the sink-registry lock, so a sink that re-enters the service (changing the level, adding another sink) cannot deadlock the emission that called it.
