## Contributing flow

branch → PR → CI green (`.github/workflows/ci.yml`) → merge; no direct pushes to main.

CI runs `cargo fmt --all -- --check`, `typos`, `cargo clippy --all-targets -- -D warnings`, and `cargo nextest run` on `ubuntu-latest`.
