# Changelog

All notable changes to RoleForge are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-08-22

### Added

- Property-based tests for link normalization, whitespace cleaning, and truncation limits.
- A unit-test suite covering matching, scraping, storage, config, and Telegram formatting (16 to 119 tests).
- An end-to-end sweep test that drives collection, scoring, persistence, every export, the digest, and the alert outbox against an injected fetcher, with no network.
- A `profiling` build profile and a `find-hotspots` skill for ranking CPU hotspots with samply.

### Fixed

- Honoured explicit geographic lockouts for hybrid and onsite profiles; only remote matches were gated before.
- Parsed salaries written with thousands separators, so `£45,000 - £55,000` no longer reads as no salary at all.
- Committed jobs, their application records, and their review fingerprints in one transaction, and only after the alert queue is durable. A failure part-way through a sweep used to leave a posting fingerprinted as reviewed but never stored, hiding it for good on the retry.
- Saved alert state after each Telegram delivery rather than after the whole queue, so a crash mid-batch no longer resends everything already sent.

### Changed

- Split each provider adapter's payload mapping out of its HTTP call so the mapping is testable.
- Extracted the pure decisions inside `pull`, `export_latest`, `generic_html`, and `send_message` into named helpers.
- Dispatched sources with a single flat `match` instead of two nested ones.
- Dropped a completeness check in `parse_feed` that `dedupe_jobs` already applies.
- Updated the Rust dependency set to its latest compatible releases.
- Adapted Reqwest configuration for its `rustls` and `query` features.
- Adapted SHA-256 fingerprint formatting for SHA2 0.11.
- Replaced the hand-rolled temporary database path in tests with `tempfile`.
- Replaced every `OnceLock<Regex>` accessor with a `LazyLock` static.
- Merged the duplicated CDATA branch in `parse_feed` into the text branch, so CDATA now also fills `company`, `location`, and `date_posted`.
- Backfilled historical review fingerprints in one transaction instead of one per stored job.
- Defaulted `daily_digest` to unset; every reader already supplies its own fallback, so the defaults now live in one place.
- Skipped Adzuna, Reed, Jooble, and SPI-mode Workable until their API keys are set, instead of polling, warning, and eventually raising a source-failure alert.
- Cached compiled phrase patterns in `text::contains_phrase`, which was recompiling a regex on every call. Profiling put regex construction at ~70% of scoring CPU; it is now under 20% of a run five times shorter.

### Removed

- The write-only `application_links`, `application_fingerprints`, and `migrations` tables.
- `MatchResult.reasons`, `matched_role_profile`, and `match`; the candidate payload already carries them.
- `ResumeProfile.raw`, `candidate_name`, `candidate_title`, and `resume_summary`, none of which were read.
- `SalaryInfo.currency` and `cadence`; scoring reads only the annualized GBP range.
- The inert `feedback` configuration and the `feedback_metrics.json` snapshot it produced.
- The always-empty `seen_jobs_state.json` snapshot; SQLite is authoritative.
- The dead `daily_digest.statuses` default, which was never read — digest filtering reads `include_statuses`.
- The duplicated `OrElseString` traits, replaced by one `text::or_fallback`.
- Chrono's unused `serde` feature.
- `Database::append_jobs` and `Database::upsert_application`, both folded into `commit_batch`.

### Security

- Stripped the request URL from HTTP errors before they are logged or stored; Jooble's key sits in the path and Adzuna's in the query string.
- Confined the generic scraper to the hosts of its own start URLs when no `allowed_domains` is configured, and rejected any scheme but HTTP and HTTPS. It would previously follow an anchor to any host.
- Neutralised spreadsheet formulas in the CSV export by prefixing fields that open with `=`, `+`, `-`, `@`, a tab, or a carriage return.

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
