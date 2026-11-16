# Contributing to YACordis

Read [docs/design.md](docs/design.md) first.

Formatting: `dprint fmt` / `dprint check`; coverage is defined in `dprint.json`. Consult the [official docs](https://dprint.dev/config/) before editing that config.

## Style

- Use cordis v4 terminology: component (never "plugin"), fiber, effect, inject, provide, epoch, isolate, bail, waterfall, middleware, quiesce.
- Comments must not repeat what the code says; comment only genuinely obscure invariants, timing requirements, or v4 divergences, briefly.
