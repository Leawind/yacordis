# Changelog

## [Unreleased]

### Added

- The leveled logging service ships with the core (previously the separate yacordis-logger crate): leveled output with pluggable sinks, component-name scopes, and level or sink changes scoped to the calling fiber that revert when it unloads.
