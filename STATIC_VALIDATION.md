# Static validation status

This project snapshot was created in an execution environment where `cargo`, `rustc`, `rustfmt`, and `rust-analyzer` were not installed and outbound DNS from the shell was unavailable.

Therefore this snapshot is **not compile-verified**.

Checks performed in the generation environment:

- Parsed workspace and crate `Cargo.toml` files with Python `tomllib`.
- Parsed `assets/catalog/builtin.toml` with Python `tomllib`.
- Parsed bundled workflow JSON with Python `json`.
- Verified referenced artifact IDs in the catalog are internally consistent using a structural validation script.
- Verified the ZIP contains the Rust workspace source rather than documentation only.

Before production use run:

```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Any compiler findings should be treated as authoritative over this static validation report.
