# RoleForge

[![CI](https://github.com/gregorycarnegie/roleforge/actions/workflows/ci.yml/badge.svg)](https://github.com/gregorycarnegie/roleforge/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-2024-orange?logo=rust)](https://www.rust-lang.org/)

Resume-aware job-market intelligence in one Rust binary. RoleForge polls job feeds and company boards, scores roles against your resume and preferences, stores state in SQLite, exports inspection snapshots, and can deliver Telegram alerts.

## Quick start

```bash
git clone https://github.com/gregorycarnegie/roleforge.git
cd roleforge
cp .env.example .env       # optional API and Telegram credentials
cp resume.example.json resume.json
cargo run -- pull          # fetch, score, persist, and alert once
cargo run -- export        # write the latest matched batch to desc.json
cargo run -- run           # continuous 60-second loop plus Telegram worker
```

Edit the local `resume.json` before the first run. It is ignored by Git so personal profile data cannot be committed accidentally.

## Commands

| Command | Purpose |
|---|---|
| `roleforge pull` | Run one source sweep and update SQLite/JSON/CSV outputs |
| `roleforge export` | Export the latest matched batch to `desc.json` for OpenClaw |
| `roleforge telegram-worker` | Long-poll Telegram callback updates |
| `roleforge run` | Run the pull/export loop and callback worker continuously |

Build a standalone release binary with `cargo build --release`.

## Sources

Built-in support includes RSS/Atom feeds, eFinancialCareers pages, Remotive, The Muse, Arbeitnow, Adzuna, Reed, and Jooble. Optional `company_boards.json` entries support Greenhouse, Lever, Ashby, Workable, and JSON-LD career pages.

```json
[
  {"name":"acme","display_name":"Acme","platform":"greenhouse","board_token":"acme"},
  {"name":"other","display_name":"Other","platform":"lever","site":"other"}
]
```

Optional `job_search_config.json`:

```json
{
  "priority_companies": ["Monzo"],
  "company_blacklist": ["Example Corp"],
  "daily_digest": {"enabled": true, "hour_utc": 7, "max_items": 8, "page_size": 4},
  "feedback": {"enabled": true}
}
```

## Migration from the Python bot

RoleForge retains the existing `jobbot_state.sqlite3` schema plus the `resume.json`, `.env`, `company_boards.json`, and `job_search_config.json` formats. Stop the Python loop, copy those files into the RoleForge directory, keep a backup of the database, then run:

```bash
roleforge export
roleforge pull
```

The database remains authoritative. `jobs.csv`, `matches.json`, `desc.json`, and the other JSON files are regenerated snapshots.

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Licensed under MIT.
