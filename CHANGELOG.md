# Changelog

All notable changes to RoleForge are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Property-based tests for link normalization, whitespace cleaning, and truncation limits.
- A `profiling` build profile and a `find-hotspots` skill for ranking CPU hotspots with samply.

### Changed

- Updated the Rust dependency set to its latest compatible releases.
- Adapted Reqwest configuration for its `rustls` and `query` features.
- Adapted SHA-256 fingerprint formatting for SHA2 0.11.
- Replaced the hand-rolled temporary database path in tests with `tempfile`.

## [0.1.0] - 2026-08-21

### Added

- Initial Rust migration of the job-market intelligence bot.
- Resume-aware scoring, salary normalization, and location filtering.
- RSS, Atom, API, and company job-board integrations.
- SQLite-compatible persistence and JSON/CSV snapshots.
- Telegram alerts, daily digests, and interactive digest pagination.
- OpenClaw-compatible latest-match export.
- Continuous polling and standalone CLI commands.
- Automated formatting, linting, testing, and release-build checks.

### Security

- Replaced personal resume data and credential-like values with safe examples.
- Ignored local resume, environment, database, and generated runtime files.
