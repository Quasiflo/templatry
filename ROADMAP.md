# Roadmap

## Background

Templatry succeeds `smartworkspace` (`/Users/luke/Documents/Projects/smartworkspace/`), keeping its proven core (template plus override merge, per-file regeneration, watch mode with config-change restart, generated-file bookkeeping) while adding what `smartworkspace` lacks: first-class remote sources with cache, an explicit per-template merge DSL, full back-propagation including deletions, and `check` mode for CI. `smartworkspace` reference files for behavior to preserve or improve: `src/generators/data.rs` (deep merge with union-append arrays), `src/generators/configs_generator.rs` (override-missing means copy, root configs are raw append), `src/generators/vscode_generator.rs` (additive-only back-propagation with `ignoreKeys` and `ignoreValues` preservation), and `src/generators.rs` plus `src/state.rs` (watchexec watch with per-path regen, debounce, and self-trigger guard).

## Locked Decisions

- Single template source per project for v1; multi-source (`[source.<name>]`) is a future feature, so v1 uses a singular `[source]` table.
- Source types for v1: local directory, local archive, GitHub release asset, generic URL archive, and git checkout; every type accepts an optional `root` subdirectory pointer for when `templatry.source.toml` is not at the fetched root.
- Cache follows `mise` behavior (platform cache dir, e.g. `~/Library/Caches` on macOS and `$XDG_CACHE_HOME` or `~/.cache` on Linux); cache entries are keyed by a deterministic hash of the full source configuration, looked up before any fetch, with no content hashing of cached data and automatic pruning of entries older than 30 days.
- Structured merge defaults to deep merge with per-template options for array policy and deletion via `_TEMPLATRY_`-namespaced value markers.
- Back-propagation is full two-way sync including deletions (a deliberate change from `smartworkspace`, which ignored deletions), guarded by an in-memory equivalence safety check that fails loudly but never kills watch mode.
- CLI verbs are `templatry generate` (default behavior on bare run), `--watch` for the daemon, `--check` for CI diff with nonzero exit, `templatry validate` (validates project config plus the resolved source config, erroring when both configs are present in one directory), and `templatry cache clear` (manual full cache flush).
- Templatry never manages headers, gitignore rules, symlinks, or file permissions: generated-file header comments are the template author's responsibility, and reads and writes use plain platform semantics.
- Greenfield Rust, edition 2024, `src/lib.rs` plus `src/main.rs` per `Cargo.toml`; do not commit implementation work from planning sessions.

## Config Schema

- `templatry.source.toml` lives at the source root (or `root` subdirectory) and defines `[configs]` (`default_generated_dir` default `.config/generated`, `default_override_dir` default `.config/`), one `[templates.<name>]` entry per file (`template`, `override_file` defaulting to template basename, `override_dir` defaulting to `default_override_dir`, `generated_file` defaulting to template basename, `generated_dir` defaulting to `default_generated_dir`, `strategy`, array policy, `back_propagate` default `false`, `labels` set), and `[default]` with mutually exclusive `include_labels` (default-deny except listed, empty list disables everything) versus `exclude_labels` (default-allow except listed), with neither defined meaning everything enabled.
- `templatry.toml` lives at `.config/templatry.toml` in the project repo and defines a singular `[source]` table for v1 (exactly one source-kind key plus `ref`, `root`, `enable_labels` and `disable_labels` layered on top of the source `[default]`, and a `use_https` opt-in for git checkout sources that otherwise default to SSH); per-template enable, disable, or dir and strategy overrides are out of scope for v1.
- `strategy` defaults to auto-detect on file extension (structured deep merge for `json`, `jsonc`, `yaml`, `yml`, `toml`; `append_bottom` for anything else) with explicit `append_top`, `append_bottom`, and `replace` available; structured merges share one JSON-value intermediate representation.
- Labels are opaque strings per template; project `enable_labels` and `disable_labels` apply after source `[default]` resolution, with disables winning ties, and unknown labels referenced from the project file are a validation error.

## Source Retrieval and Cache

- Resolve the `[source]` table into one fetcher: local directory (use in place, no cache copy), local archive (extract like a remote), GitHub release (`github = "org/repo"`, `ref` tag, `asset` glob such as `some_teams_configs_*.zip`, optional `root`), generic URL (archive URL plus `root`), or git checkout (URL plus `ref` commit, tag, or branch, plus `root`); supported archive formats to pin during implementation are at minimum `tar.gz` and `zip`.
- A GitHub asset glob matching zero assets is an error, and matching more than one asset is an error that lists the matches so the pattern gets tightened; there is no implicit newest-match selection.
- Cache entries live under `<platform-cache-dir>/templatry/<source-id-hash>/` where the hash is deterministic over every aspect of the source configuration (kind, URL, `ref`, asset pattern, `root`, and any other fetch-affecting field), so any source change naturally yields a new cache key and lookups never touch the network on a hit; local directory sources never enter the cache.
- Cached content is trusted as-is with no re-hashing: a manually modified cache is the user's responsibility to flush via `templatry cache clear`, which empties the whole cache so the next run re-pulls and re-derives entries; entries older than 30 days (by best-available timestamp, e.g. entry mtime refreshed on each hit) are pruned automatically.
- Floating refs are strongly discouraged but unenforceable for plain HTTP fetches; the cache-first default means a floating ref does not move until the user runs `templatry cache clear`, which is the accepted staleness story alongside `--offline` for cache-only operation.
- Private sources read `TEMPLATRY_GITHUB_TOKEN` with explicit precedence, pass through ambient `GITHUB_TOKEN` and `GH_TOKEN` for GitHub API and asset downloads, and forward standard git authentication environment to git subprocesses; GitHub API traffic is always HTTPS while git checkout sources default to SSH with a `use_https` config opt-in, falling back to whichever transport the chosen libraries support if both cannot be offered.

## Merge Rules

- Deep merge is the structured default: objects recurse with override winning, scalars and type mismatches replace, and arrays follow the per-template array policy (`union` append-with-dedup for scalars as in `smartworkspace`, or `replace` wholesale).
- Override-driven deletion uses a value marker: setting a key's value to `_TEMPLATRY_DELETE_` in the override file deletes that key from the merged output; all current and future markers share the `_TEMPLATRY_` prefix plus trailing underscore convention.
- `append_bottom` concatenates template bytes, a newline separator, then override bytes (the `smartworkspace` root-config behavior for `gitignore` and `gitattributes` style files); `append_top` is the mirror (override bytes, newline, then template bytes); `replace` emits the override file verbatim, ignoring the template.
- Missing override file ALWAYS means copy the template verbatim; missing template file is always an error; empty `include_labels` disabling everything must still succeed as a no-op generate.
- Generation never skips writes via content hashing: every enabled template is merged and written on every run, keeping behavior simple and predictable at the cost of redundant writes.
- Format handling must decide TOML datetime preservation, comment preservation expectations (lossy JSON intermediate representation is acceptable for v1 if documented, matching `smartworkspace`), `jsonc` comment stripping on read, and deterministic pretty-printing per format so `--check` is stable.

## Back-Propagation

- Only templates with `back_propagate = true` watch their generated file; on change, diff the current generated content against the last generated content (tracked via in-memory snapshot plus on-disk state where needed), then fold additions, modifications, and deletions back into the override file so the next template bump reproduces the hand-tuned output.
- Deletions in the generated file delete the corresponding key from the override file, or record `_TEMPLATRY_DELETE_` there when the template still defines the key, which is the v1 behavior change versus `smartworkspace`.
- Safety check before any override write: replay the candidate override through the template pipeline in memory and compare against the actual current generated state, using semantic equivalence ignoring key order for structured formats and exact comparison for text strategies; on mismatch, emit a loud error and leave both files untouched, and in watch mode log the error and keep watching rather than exiting.
- Conflicts between a template update and a pending back-propagation write resolve template-wins-then-reapply-override (regenerate forward, then fold the captured diff), with machine-specific preservation (`ignoreKeys` and `ignoreValues` style lists from `smartworkspace`) evaluated during implementation rather than assumed.
- When `back_propagate = false`, hand-edits to generated files are overwritten on the next generate and reported as dirty by `--check`.

## Generation Flow

- Order is resolve source (cache lookup by source-id hash, else fetch plus extract into cache) then load `templatry.source.toml` then load project `templatry.toml` then resolve label selection then for each enabled template read template plus optional override, merge per strategy, and write the generated file (creating parent dirs), always writing without content-hash short-circuiting.
- `bare templatry` behaves as `templatry generate`; `generate` accepts `--config <path>` (override project config location, mirroring `smartworkspace --config`), `--force` (reserved for future destructive semantics; with always-write generation it currently changes nothing), and `--dry-run` (print planned writes without touching disk).
- `--check` performs the full generation in memory, diffs against disk, prints differing paths, and exits nonzero on any difference without writing, suitable for CI and pre-commit hooks.
- Generated-file writes are atomic (temp file plus rename) with plain read and write semantics and no symlink or permission management.

## Watch Mode

- `templatry --watch` (equivalently `templatry generate --watch`, one spelling canonical after CLI design) does an initial generate then watches override files, `templatry.toml`, and generated files of `back_propagate = true` templates using `watchexec`, regenerating only the affected template on override change and doing a full reload plus resubscribe on `templatry.toml` or source-config change, reusing the `smartworkspace` pattern of per-path regen plus config-change restart with debounce and a self-trigger write guard.
- Remote source updates (new `ref` or new source config) trigger re-resolve plus full regenerate when `templatry.toml` changes; upstream movement at an unchanged floating ref is not picked up until `templatry cache clear` plus re-run.
- Watcher scope is exactly the override dirs, generated dirs, and project config file; nothing outside those roots is watched, rapid editor save bursts are debounced, and back-propagation safety failures are logged loudly without stopping the watcher.

## CLI

- Verbs and flags for v1: `templatry generate [--watch] [--check] [--dry-run] [--force] [--config <path>] [--offline]`, `templatry validate [--config <path>]`, `templatry cache clear`, `--verbose` and `--quiet` global flags, with bare `templatry` aliasing `generate`; `--watch` and `--check` are mutually exclusive.
- Exit codes are `0` success or no diff, `2` differences found by `--check`, and nonzero with a diagnostic on validation or generation failure; `--check` output is a plain differing-file list consumable by CI logs.
- Help text documents the single-source limit, the `root` parameter, floating-ref discouragement with the `cache clear` remedy, and the label resolution order with examples for each source kind.

## Validation Rules

- `templatry validate` auto-detects context and rejects ambiguity: when both `.config/templatry.toml` and `templatry.source.toml` are present for the target directory it errors instead of guessing; with only `templatry.source.toml` present it validates the source schema (required tables, unknown strategy names, `include_labels` plus `exclude_labels` mutual exclusion, every `template` path existing under `root`, label references resolving).
- With only `.config/templatry.toml` (or `--config`) present it validates the project schema (exactly one source kind specified, `ref` present for remote kinds, `enable_labels` and `disable_labels` referencing labels the source actually defines) and it resolves plus downloads the source first so the referenced `templatry.source.toml` is validated as well; template repositories run `templatry validate` in CI as their only job, while project repositories run it before generate in hooks if desired.
- All diagnostics name the file, table, and key at fault with one suggested fix, following the `smartworkspace` precedent of actionable `bail!` messages.

## Library and Binary Split

- `src/lib.rs` exposes `config` (schema plus parsing plus label resolution), `source` (fetchers plus source-id hashing plus cache plus 30-day prune), `merge` (strategies plus format bridges plus `_TEMPLATRY_` markers), `generate` (orchestration plus atomic writes plus check and dry-run diffing), `watch` (subscription plus dispatch), and `validate` modules with unit-testable pure functions at each boundary.
- `src/main.rs` stays a thin `clap` plus `tokio` shell over the library (argument parsing, logging setup, exit codes), preserving the `smartworkspace` shape of a small binary over a testable core.
- Integration tests in `tests/` cover gold-file generation (template plus override means expected output per strategy, including `append_top`, `append_bottom`, and `replace`), label selection matrices, back-propagation round-trips including deletions plus safety-check mismatch cases, asset-glob tie errors, both-configs-present validation errors, and `--check` exit codes; network fetchers get trait-gated fake or fixture-based tests so the suite runs offline.

## Suggested Dependencies

- Evaluate `clap` (CLI), `serde` plus `toml` plus `serde_json` plus `serde_yaml` plus `json-strip-comments` (config and merge formats, mirroring `smartworkspace`), `tokio` (async runtime), `watchexec` (watch mode), `sha2` plus `hex` (source-id hashing), `dirs` (platform cache dir), `tempfile` (atomic writes), `glob` (asset pattern matching), `diff` or `similar` (check output), `tracing` plus `tracing-subscriber` (logging), `thiserror` plus `color-eyre` or `miette` (errors), and `reqwest` with rustls (URL and GitHub downloads); pin choices in the scaffolding milestone and record them in `deny.toml` (currently missing, which breaks `hk check`).

## Milestones

- Milestone 0, scaffolding: create `src/lib.rs` and `src/main.rs`, add `deny.toml` (or remove the `cargo deny` hk step), choose and pin dependencies, establish `tracing` plus error conventions and gold-file test harness.
- Milestone 1, schemas plus `validate`: implement both TOML schemas with serde (including `append_top`, `append_bottom`, `replace`, array policies, and the `_TEMPLATRY_DELETE_` value marker), label resolution, and `templatry validate` diagnostics (both-configs-present error, project mode resolving the source) plus tests; wire template-repo CI to run only `validate`.
- Milestone 2, sources plus cache: implement the five fetchers with `root` support, source-id-hash cache keys with lookup-before-fetch, full `cache clear`, 30-day auto-prune, `--offline`, GitHub asset globs with tie errors, SSH default with `use_https` opt-in, and token handling (`TEMPLATRY_GITHUB_TOKEN` precedence plus ambient GitHub and git auth env); cover with fixture-based tests.
- Milestone 3, merge plus `generate` plus `--check`: implement deep merge with array policies and the deletion marker, format bridges with `append_bottom` unknown-extension fallback, always-write atomic generation, `--dry-run`, and `--check` with exit code `2` on diff.
- Milestone 4, `watch`: implement watchexec subscription, per-path regen, config-change restart, debounce, and self-trigger guard.
- Milestone 5, back-propagation: implement full sync including deletions with the in-memory equivalence safety check (loud non-fatal errors in watch mode) and conflict rule template-wins-then-reapply, plus round-trip and mismatch tests.
- Milestone 6, polish and dogfood: adopt templatry for its own `.config`, document source authoring (including header-comment conventions for template authors), add CI workflows beyond `release-please.yml`, and cut the first release via release-please.

## Future Work

- Multi-source projects (`[source.<name>]`) with deterministic conflict rules and per-template project overrides.
- Additional merge primitives (JSON patch, strategic merge by key, comment-preserving rewrites) and per-template `ignoreKeys` and `ignoreValues` preservation generalized beyond settings files.
- Source lockfile (`templatry.lock`) recording resolved digests, provenance (SLSA or Sigstore) verification, signed sources, and version-update automation (e.g. Renovate).
- `templatry init`, `templatry update`, `templatry diff`, `templatry cache list` and `prune`, and `templatry migrate-from-smartworkspace` conveniences.

## Open Implementation Questions

- Source-id hash algorithm and canonical serialization of the source configuration, plus the best timestamp signal for 30-day pruning (entry mtime refreshed on hit versus a dedicated metadata file).
- TOML datetime preservation and deterministic pretty-printing per format.
- Exact `[source]` field names per kind (`asset` glob key, `use_https` flag naming) and the supported archive format list.
- Whether the git fetcher shells out to system git (getting SSH and credential helpers for free) versus a pure-Rust git library, and graceful degradation if only one transport is supportable.
