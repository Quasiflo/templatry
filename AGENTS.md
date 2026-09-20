# AGENTS.md

Rust CLI + library, Milestones 0–3 done. `validate` resolves all four source kinds (remotes fetched through the content-addressed cache); `generate` (with `--check`/`--dry-run`), `validate`, and `cache clear` work; `--watch` still returns `unimplemented` (see `ROADMAP.md`).

## Toolchain

- Rust `1.89+`, edition `2024`, pinned via `.config/mise.toml` (`core:rust 1.98.1`). Use `mise x -- <cmd>` if local cargo differs.
- Tools: `hk 2.0.0`, `rumdl`, `zizmor` (all via mise/aqua).

## Layout

- `src/lib.rs` holds `Error` (`thiserror` + `miette`, codes like `templatry::unimplemented`) and `Result`, plus modules `config`, `source`, `merge`, `generate`, `watch`, `validate`. `src/main.rs` is a thin clap shell (`generate` default, `validate`, `cache clear`, bare `--watch` alias); logic must live in lib fns (`generate::run`, `validate::run`, `source::cache_clear`) so it stays testable.
- Dependencies pinned per `ROADMAP.md` Pinned Dependencies; `deny.toml` enforces advisories/bans/licenses/sources.
- `tests/common/mod.rs` is the gold-file harness (`BLESS=1` to bless, always review the diff); suites add `tests/fixtures/<suite>/` cases per milestone.
- Release-please config lives in `.config/` (`rp-config.json` + `rp-manifest.json`), not repo root. Release type `rust`, `bump-minor-pre-major: true`.

## Commands

```sh
cargo build
cargo test
cargo test <substring>      # single / focused test
cargo clippy --all-targets -- -D warnings  # hk's clippy step skips test targets; run this manually
cargo fmt --check
hk check                    # pre-commit gate: rumdl + zizmor + cargo clippy/fmt/deny
```

## Lint / Style Gotchas

- Pre-commit gate is `.config/hk.pkl`. CI does NOT run it — only `release-please.yml` exists in `.github/workflows/`, so run `hk check` locally.
- `hk` includes a `cargo deny` step backed by `deny.toml` (cargo-deny 0.20.2 via mise/aqua). 0.20 schema note: `unmaintained`/`unsound` take scope values (`all`/`workspace`/`transitive`/`none`), not severities.
- Markdown lint is `rumdl` with `.config/rumdl.toml`: `MD013` disabled, `CHANGELOG.md` excluded. Never hard-wrap markdown (`.vscode/settings.json` uses visual wrap).
- `zizmor` lints GitHub Actions; keep `permissions: {}` at top level on new workflows (see `release-please.yml`).
