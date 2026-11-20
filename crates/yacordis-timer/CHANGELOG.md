# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/Leawind/yacordis/compare/yacordis-timer-v0.1.0...yacordis-timer-v0.1.1) - 2026-11-20

### Fixed

- keep timer callbacks out of the wheel's locks
- drain in-flight timer callbacks before rollback

### Other

- version crates independently
- apply dprint
