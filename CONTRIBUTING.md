# Contributing

RoleForge requires the stable Rust toolchain.

```bash
git clone https://github.com/gregorycarnegie/roleforge.git
cd roleforge
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Copy `.env.example` to `.env` and `resume.example.json` to the ignored `resume.json`, then run one local sweep with `cargo run -- pull`.

Source integrations live in `src/sources.rs`. Add behavior-focused tests beside the implementation and document new configuration keys in the README.
