# Changelog

## [Unreleased]

### Changed

- Lifecycle tracing records only in debug builds, or when `YACORDIS_LIFECYCLE_TRACE` is set: its per-fiber ring otherwise retains a bucket for every fiber a long-lived release process ever installed.

### Fixed

- A fiber being disposed reports `Disposed` from the same critical section that marks it disposed, not after its teardown completes (cordis nulls the uid first, for the same reason): a direct `dispose()` — which no quiesce waits for, since it is not op-accounted — could be observed as `Active` with its dependencies already withdrawn.
- A dropped runtime is collectable: a fiber no longer stores its `Context`, which made `Core → Fiber → Context → Core` an `Arc` cycle and leaked one runtime per `Context::new()`.
- A disposed fiber retires its cascade entry on the parent, and effect batches drop spent entries: reinstalling a component no longer accumulates one entry per cycle.

## [0.2.0] - 2026-11-20

### Added

- The leveled logging service ships with the core (previously the separate yacordis-logger crate): leveled output with pluggable sinks, component-name scopes, and level or sink changes scoped to the calling fiber that revert when it unloads.

### Changed

- `quiesce` and `Context::quiesce` share one bounded wait (30 s): a lifecycle op stuck in user code surfaces as a report — the runtime variant dumps every fiber's state — instead of hanging the caller indefinitely.

### Fixed

- Logger sinks are called outside the sink-registry lock, so a sink that re-enters the service (changing the level, adding another sink) cannot deadlock the emission that called it.
