## Contributing flow

branch → PR → CI green (`.github/workflows/ci.yml`) → merge; no direct pushes to main.

CI runs `cargo fmt --all -- --check`, `typos`, `cargo clippy --all-targets --message-format=short -- -D warnings`, and `cargo nextest run` on `ubuntu-latest`. Run the same locally before pushing.

Lints: `[lints.rust]`/`[lints.clippy]` in `Cargo.toml` (clippy pedantic, `unwrap_used`/`expect_used`/`panic`/`allow_attributes` denied). Test modules opt out with a module-level `#![expect(clippy::unwrap_used)]`; production code restructures instead of suppressing.
