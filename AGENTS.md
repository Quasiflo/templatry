# AGENTS.md

Greenfield Rust CLI + library. `src/` is currently empty — nothing implemented yet.

## Toolchain

- Rust `1.89+`, edition `2024`, pinned via `.config/mise.toml` (`core:rust 1.98.1`). Use `mise x -- <cmd>` if local cargo differs.
- Tools: `hk 2.0.0`, `rumdl`, `zizmor` (all via mise/aqua).

## Layout

- `Cargo.toml` declares both targets, neither file exists yet — create first:
  - `src/lib.rs` → `templatry` library
  - `src/main.rs` → `templatry` binary
- No dependencies in `Cargo.toml`. `Cargo.lock` only lists the root package.
- `tests/` is empty (only `.DS_Store`). `docs/` only has empty `docs/assets/`.
- Release-please config lives in `.config/` (`rp-config.json` + `rp-manifest.json`), not repo root. Release type `rust`, `bump-minor-pre-major: true`.

## Commands

```sh
cargo build
cargo test
cargo test <substring>      # single / focused test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
hk check                    # pre-commit gate: rumdl + zizmor + cargo clippy/fmt/deny
```

## Lint / Style Gotchas

- Pre-commit gate is `.config/hk.pkl`. CI does NOT run it — only `release-please.yml` exists in `.github/workflows/`, so run `hk check` locally.
- `hk` includes a `cargo deny` step but there is no `deny.toml` yet — that check will fail until one is added or the step removed.
- Markdown lint is `rumdl` with `.config/rumdl.toml`: `MD013` disabled, `CHANGELOG.md` excluded. Never hard-wrap markdown (`.vscode/settings.json` uses visual wrap).
- `zizmor` lints GitHub Actions; keep `permissions: {}` at top level on new workflows (see `release-please.yml`).
