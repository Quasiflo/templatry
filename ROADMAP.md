# Roadmap

## Background

Templatry succeeds `smartworkspace` (`/Users/luke/Documents/Projects/smartworkspace/`), keeping its proven core (template plus override merge, per-file regeneration, watch mode with config-change restart, generated-file bookkeeping) while adding what `smartworkspace` lacks: first-class remote sources with cache, an explicit per-template merge DSL, full back-propagation including deletions, shared-destination merging for label-split files, and `check` mode for CI. `smartworkspace` reference files for behavior to preserve or improve: `src/generators/data.rs` (deep merge with union-append arrays), `src/generators/configs_generator.rs` (override-missing means copy, root configs are raw append), `src/generators/vscode_generator.rs` (additive-only back-propagation with `ignoreKeys` and `ignoreValues` preservation), and `src/generators.rs` plus `src/state.rs` (watchexec watch with per-path regen, debounce, and self-trigger guard).

## Locked Decisions

- Single template source per project for v1; multi-source (`[source.<name>]`) is a future feature, so v1 uses a singular `[source]` table.
- Source types for v1: local directory, GitHub release asset, generic URL archive, and git checkout. Local archives are out of scope: local sources are directories only, used in place with no caching. Every type accepts an optional `root` subdirectory pointer for when `templatry.source.toml` is not at the fetched root.
- Cache follows `mise` behavior (platform cache dir, e.g. `~/Library/Caches` on macOS and `$XDG_CACHE_HOME` or `~/.cache` on Linux); entries live under `<platform-cache-dir>/templatry/<full-sha256-hex>/` where the key is SHA-256 over the canonical normalized source struct (no truncation, no human-readable prefix), looked up before any fetch, with no content hashing of cached data and automatic pruning of entries older than 30 days via a per-entry sidecar file.
- Structured merge defaults to deep merge with per-template `array_policy` (`union` default, `replace`) and override-driven deletion via the `_TEMPLATRY_DELETE_` value marker; output uses sorted keys for stability (generated files are not meant for human reading, so comment loss and reordering are acceptable).
- Multiple enabled templates may target the same destination file (e.g. per-ecosystem `settings.json` fragments selected by labels); their outputs combine additively with order-independent semantics, and any conflicting leaf values are a loud error naming the paths, templates, and labels involved.
- Back-propagation is full two-way sync including deletions (a deliberate change from `smartworkspace`, which ignored deletions), guarded by an in-memory equivalence safety check that fails loudly but never kills watch mode.
- CLI verbs are `templatry generate` (default behavior on bare run, canonical `generate --watch` with bare `--watch` accepted as an alias), `--check` for CI diff with exit code `2` on difference, `templatry validate` (validates project config plus the resolved source config, erroring when both configs are present in one directory), and `templatry cache clear` (manual full cache flush). There is no `--force` flag: every enabled template is merged and written on every run. `--offline` proceeds on cache hits and errors loudly (naming the missing source) on cache misses.
- `ref` pinning is strict: tags or commit SHAs only, branches rejected; `ref` on source kinds where it is meaningless (local directory, generic URL) is a validation error, not silently ignored.
- Auth tokens (`TEMPLATRY_GITHUB_TOKEN` with explicit precedence, ambient `GITHUB_TOKEN` and `GH_TOKEN` as fallback) are used only for GitHub HTTPS API and asset-download requests; nothing is injected into git subprocesses, which inherit the parent environment untouched.
- Templatry never manages headers, gitignore rules, symlinks, or file permissions: generated-file header comments are the template author's responsibility, and reads and writes use plain platform semantics.
- Greenfield Rust, edition 2024, `src/lib.rs` plus `src/main.rs` per `Cargo.toml`; do not commit implementation work from planning sessions.

## Config Schema

- `templatry.source.toml` lives at the source root (or `root` subdirectory) and defines `[configs]` (`default_generated_dir` default `.config/generated`, `default_override_dir` default `.config/`), one `[templates.<name>]` entry per file (`template`, `override_file` defaulting to template basename, `override_dir` defaulting to `default_override_dir`, `generated_file` defaulting to template basename, `generated_dir` defaulting to `default_generated_dir`, `strategy`, `array_policy` default `union`, `back_propagate` default `false`, `labels` set), and `[default]` with mutually exclusive `include_labels` (default-deny except listed, empty list disables everything) versus `exclude_labels` (default-allow except listed), with neither defined meaning everything enabled.
- `templatry.toml` lives at `.config/templatry.toml` in the project repo and defines a singular `[source]` table for v1 (exactly one source-kind key plus `ref` where applicable, `root`, `enable_labels` and `disable_labels` layered on top of the source `[default]`, `asset` glob for GitHub-release sources, and a `use_https` bool for git checkout sources that otherwise default to SSH); per-template enable, disable, or dir and strategy overrides are out of scope for v1. Concrete shapes:

  ```toml
  [source]
  path = "../my-templates"          # local directory: uncached, used in place
  # github = "org/repo"             # github release: org/repo shorthand (not a URL)
  # ref = "v1.2.3"                  # github + git only: tag name required
  # asset = "configs_*.zip"         # github only: asset glob, required
  # url = "https://example.com/templates.tar.gz"   # generic URL archive (ref rejected)
  # git = "git@github.com:org/repo.git"            # git checkout (branches rejected)
  # use_https = true                # git only, default false = SSH
  # root = "templates"              # all remote kinds, default: fetch root
  ```

- `strategy` defaults to auto-detect on file extension (structured deep merge for `json`, `jsonc`, `yaml`, `yml`, `toml`; `append_bottom` for anything else) with explicit `append_top`, `append_bottom`, and `replace` available; structured merges share one JSON-value intermediate representation (`yaml_serde`, as in `smartworkspace`).
- Labels are opaque strings per template; project `enable_labels` and `disable_labels` apply after source `[default]` resolution, with disables winning ties, and unknown labels referenced from the project file are a validation error.

## Source Retrieval and Cache

- Resolve the `[source]` table into one fetcher: local directory (use in place, no cache copy), GitHub release (`github = "org/repo"`, `ref` tag, required `asset` glob such as `some_teams_configs_*.zip`, optional `root`), generic URL (archive URL plus `root`), or git checkout (URL plus `ref` tag or commit SHA, plus `root`); supported archive formats are `tar.gz` (plus `tgz` alias), `tar`, and `zip`, extracted with `flate2` plus `tar` plus `zip`; `xz` and `zstd` variants are deferred until needed.
- GitHub access uses raw `reqwest` (already planned for downloads) against two endpoints, get-release-by-tag plus list-assets with pagination, downloading with `Accept: application/octet-stream`; no `octocrab` dependency. A glob matching zero assets is an error, and matching more than one asset is an error that lists the matches so the pattern gets tightened; there is no implicit newest-match selection and no `latest` auto-resolution.
- Git checkout shells out to system `git` (SSH, HTTPS, and credential helpers come for free; `use_https` only selects the URL form); probe `git --version` at fetch time and bail with install instructions when absent. Each new cache key gets a fresh shallow clone plus checkout of the pinned ref with no in-place `fetch`, so floating state can never move under a cached entry; the resolved commit SHA is recorded in the entry sidecar for auditability. Branch refs are rejected via `ls-remote`: a 40-char hex ref is accepted as a commit SHA outright (short SHAs are rejected with a use-the-full-SHA error); otherwise the ref must match `git ls-remote --tags` output (lightweight and annotated tags both accepted via checkout by tag name); a ref matching only `--heads` output fails with an explicit branches-are-rejected error; anything else fails as ref-not-found.
- The cache key is SHA-256 (hex, full length, no prefix) over the canonical normalized source struct — normalize first (trailing slashes, defaulted `root`, equivalent field spellings) and hash the struct, never the raw TOML text, so formatting-only edits do not invalidate the cache. Any source change naturally yields a new key and lookups never touch the network on a hit.
- Each entry carries a sidecar file, `.templatry-meta.json` (`{"last_used_unix": ..., "resolved_sha": ...}` with the SHA present for git sources), refreshed on every hit; entries whose sidecar is older than 30 days are pruned automatically during normal invocations. Cached content is trusted as-is with no re-hashing: a manually modified cache is the user's responsibility to flush via `templatry cache clear`, which empties the whole cache so the next run re-pulls and re-derives entries.
- Floating refs are rejected, not merely discouraged, wherever enforcement is possible (tags-or-SHA for git and GitHub releases); plain HTTP URLs remain inherently unenforceable, and the cache-first default (stale until `templatry cache clear`) is the accepted story there alongside `--offline` for cache-only operation.

## Merge Rules

- Deep merge is the structured default: objects recurse with override winning, scalars and type mismatches replace, and arrays follow the per-template `array_policy` (`union` append-with-dedup for scalars as in `smartworkspace`, or `replace` wholesale). Exactly two policies for v1; merge-by-key and friends are future work.
- Override-driven deletion uses a value marker: setting a key's value to `_TEMPLATRY_DELETE_` in the override file deletes that key from the merged output; the marker works at any nesting depth, and all current and future markers share the `_TEMPLATRY_` prefix plus trailing underscore convention. The marker in array position is a validation error (use `array_policy = "replace"` to drop elements); under `append_top`, `append_bottom`, or `replace` strategies it is a literal string, and `validate` warns (not errors) when a marker-looking string appears with a non-merge strategy.
- `append_bottom` concatenates template bytes, a newline separator, then override bytes (the `smartworkspace` root-config behavior for `gitignore` and `gitattributes` style files); `append_top` is the mirror (override bytes, newline, then template bytes); `replace` emits the override file verbatim, ignoring the template.
- Missing override file with a merge or append strategy means copy the template verbatim, while missing override file with `replace` is an error because there is no content to emit; missing template file is always an error; empty `include_labels` disabling everything must still succeed as a no-op generate.
- Generation never skips writes via content hashing: every enabled template is merged and written on every run, keeping behavior simple and predictable at the cost of redundant writes.
- Format handling: lossy JSON intermediate representation is accepted for v1 (TOML datetimes are a hard error with an actionable message, per `smartworkspace` precedent), `jsonc` comments are stripped on read, and output is deterministically pretty-printed with sorted keys so `--check` is stable.

## Shared-Destination Merging

- When label selection enables more than one template resolving to the same `generated_dir` plus `generated_file`, each template is first merged with its own override per its own strategy, then the per-template outputs combine additively into the single destination file. This is the mechanism for splitting large multi-domain files (e.g. `templates.rust-vscode-settings` plus `templates.dart-vscode-settings` both emitting `settings.json`).
- Combination is order-independent by construction: objects recurse and union; a leaf path defined by more than one contributor with unequal values is a generation error naming the conflicting path, each contributing template, and their labels, so the user resolves it in the source or overrides. Equal values from multiple contributors are fine (idempotent). Arrays from multiple contributors must be exactly equal or they conflict; there is no silent ordering choice, and no contributor's `array_policy` applies across templates.
- `validate` checks shared-destination groups structurally where possible (e.g. flagging templates whose strategies cannot combine, such as `replace` targeting an already-targeted destination), but value-level conflicts surface at generate time when real content is available.
- Back-propagation scoping follows the contributor: a change in a shared destination diffs back only into the override of the `back_propagate = true` contributor whose last-output subtree matches the edit; ambiguous edits matching multiple contributors are a loud error rather than a guess. If no contributor has `back_propagate = true`, hand-edits are overwritten and reported dirty by `--check` as usual.

## Back-Propagation

- Only templates with `back_propagate = true` watch their generated file; on change, diff the current generated content against the last generated content (tracked via in-memory snapshot plus on-disk state where needed), then fold additions, modifications, and deletions back into the override file so the next template bump reproduces the hand-tuned output.
- Deletions in the generated file delete the corresponding key from the override file, or record `_TEMPLATRY_DELETE_` there when the template still defines the key, which is the v1 behavior change versus `smartworkspace`.
- Safety check before any override write: replay the candidate override through the template pipeline in memory and compare against the actual current generated state, using exact `serde_json::Value` equality after key sorting for structured formats (the `1` versus `1.0` number-representation quirk is accepted: any mismatch is an error, erring toward safety) and byte-exact comparison for text strategies; on mismatch, emit a loud error and leave both files untouched, writing the conflicting generated snapshot to system temp (`temp_dir()/templatry-conflicts/<stem>.<timestamp>.conflict.<ext>`) with the path named in the error so the hand-edit is recoverable, and in watch mode log the error and keep watching rather than exiting.
- Conflicts between a template update and a pending back-propagation write resolve template-wins-then-reapply-override (regenerate forward, then fold the captured diff), with machine-specific preservation (`ignoreKeys` and `ignoreValues` style lists from `smartworkspace`) evaluated during implementation rather than assumed.
- When `back_propagate = false`, hand-edits to generated files are overwritten on the next generate and reported as dirty by `--check`.

## Generation Flow

- Order is resolve source (cache lookup by source-id hash, else fetch plus extract into cache) then load `templatry.source.toml` then load project `templatry.toml` then resolve label selection then for each enabled template read template plus optional override, merge per strategy, combine shared-destination groups additively with conflict errors, and write each generated file (creating parent dirs), always writing without content-hash short-circuiting.
- `bare templatry` behaves as `templatry generate`; `generate` accepts `--config <path>` (override project config location, mirroring `smartworkspace --config`), `--dry-run` (print planned writes without touching disk), and `--offline` (cache-only); `--watch` and `--check` are mutually exclusive.
- `--check` performs the full generation in memory, diffs against disk, prints differing paths, and exits `2` on any difference without writing, suitable for CI and pre-commit hooks.
- Generated-file writes are atomic (temp file plus rename) with plain read and write semantics and no symlink or permission management.

## Watch Mode

- `templatry generate --watch` (with bare `templatry --watch` accepted as an alias; one code path, documented once) does an initial generate then watches override files, `templatry.toml`, and generated files of `back_propagate = true` templates using `watchexec`, regenerating only the affected template on override change and doing a full reload plus resubscribe on `templatry.toml` or source-config change, reusing the `smartworkspace` pattern of per-path regen plus config-change restart with debounce and a self-trigger write guard.
- Remote source updates (new `ref` or new source config) trigger re-resolve plus full regenerate when `templatry.toml` changes; upstream movement at an unchanged pin is impossible by construction (fresh clone per key, branches rejected), except for plain HTTP URLs, which require `templatry cache clear` plus re-run.
- Watcher scope is exactly the override dirs, generated dirs, and project config file; nothing outside those roots is watched, rapid editor save bursts are debounced, and back-propagation safety failures are logged loudly without stopping the watcher.

## CLI

- Verbs and flags for v1: `templatry generate [--watch] [--check] [--dry-run] [--config <path>] [--offline]`, `templatry validate [--config <path>]`, `templatry cache clear`, `--verbose` and `--quiet` global flags, with bare `templatry` aliasing `generate`; `--watch` and `--check` are mutually exclusive.
- Exit codes are `0` success or no diff, `2` differences found by `--check`, and nonzero with a diagnostic on validation or generation failure; `--check` output is a plain differing-file list consumable by CI logs; `--offline` on a cache miss errors naming the missing source.
- Help text documents the single-source limit, the `root` parameter, tags-or-SHA pinning with branch rejection, the `cache clear` remedy for HTTP staleness, and the label resolution order with examples for each source kind.

## Validation Rules

- `templatry validate` auto-detects context and rejects ambiguity: when both `.config/templatry.toml` and `templatry.source.toml` are present for the target directory it errors instead of guessing; with only `templatry.source.toml` present it validates the source schema (required tables, unknown strategy or array-policy names, `include_labels` plus `exclude_labels` mutual exclusion, every `template` path existing under `root`, label references resolving, marker-in-array misuse, `replace` combined with shared destinations flagged where detectable).
- With only `.config/templatry.toml` (or `--config`) present it validates the project schema (exactly one source kind specified, `ref` present for git and GitHub-release kinds and rejected as meaningless for local directory and generic URL kinds, git SHAs must be full 40-char hex with short SHAs rejected, branch-like refs rejected for git via the tags-then-heads `ls-remote` check, `enable_labels` and `disable_labels` referencing labels the source actually defines) and it resolves plus downloads the source first so the referenced `templatry.source.toml` is validated as well; template repositories run `templatry validate` in CI as their only job, while project repositories run it before generate in hooks if desired.
- All diagnostics name the file, table, and key at fault with one suggested fix, following the `smartworkspace` precedent of actionable errors, implemented with `thiserror` plus `miette` diagnostics.

## Library and Binary Split

- `src/lib.rs` exposes `config` (schema plus parsing plus label resolution), `source` (fetchers plus source-id hashing plus cache plus sidecar plus 30-day prune), `merge` (strategies plus array policies plus format bridges plus `_TEMPLATRY_` markers plus shared-destination combination), `generate` (orchestration plus atomic writes plus check and dry-run diffing), `watch` (subscription plus dispatch), and `validate` modules with unit-testable pure functions at each boundary.
- `src/main.rs` stays a thin `clap` plus `tokio` shell over the library (argument parsing, logging setup, exit codes), preserving the `smartworkspace` shape of a small binary over a testable core.
- Integration tests in `tests/` cover gold-file generation (template plus override means expected output per strategy, including `append_top`, `append_bottom`, and `replace`), label selection matrices, shared-destination combination (idempotent overlap, conflict errors naming paths plus templates plus labels, array conflicts), back-propagation round-trips including deletions plus safety-check mismatch cases with snapshot recovery, asset-glob zero- and multi-match errors, both-configs-present validation errors, meaningless-`ref` errors, and `--check` exit codes; network fetchers get trait-gated fake or fixture-based tests so the suite runs offline.

## Pinned Dependencies

- `clap` (CLI), `serde` plus `toml` plus `serde_json` plus `yaml_serde` plus `json-strip-comments` (config and merge formats, mirroring `smartworkspace`), `tokio` (async runtime), `watchexec` (watch mode), `sha2` plus `hex` (source-id hashing), `dirs` (platform cache dir), `tempfile` (atomic writes and conflict snapshots), `glob` or `wildmatch` (asset pattern matching), `flate2` plus `tar` plus `zip` (archive extraction), `diff` or `similar` (check output), `tracing` plus `tracing-subscriber` (logging), `thiserror` plus `miette` (errors), and `reqwest` with rustls (URL and GitHub downloads); record them in `deny.toml` (currently missing, which breaks `hk check`).

## Milestones

- Milestone 0, scaffolding: create `src/lib.rs` and `src/main.rs`, add `deny.toml` (or remove the `cargo deny` hk step), pin the dependencies above, establish `tracing` plus `miette` error conventions and gold-file test harness.
- Milestone 1, schemas plus `validate`: implement both TOML schemas with serde (including `append_top`, `append_bottom`, `replace`, array policies, the `_TEMPLATRY_DELETE_` value marker, `asset`, `use_https`), label resolution, strict `ref` rules (required for git and GitHub-release, rejected elsewhere, branches rejected for git), and `templatry validate` diagnostics (both-configs-present error, project mode resolving the source) plus tests; wire template-repo CI to run only `validate`.
- Milestone 2, sources plus cache: implement the four fetchers with `root` support, SHA-256 source-id cache keys with lookup-before-fetch, sidecar files with resolved SHAs, full `cache clear`, 30-day auto-prune, `--offline`, GitHub asset globs with zero- and multi-match errors, SSH default with `use_https` opt-in, and GitHub-only token handling (`TEMPLATRY_GITHUB_TOKEN` precedence plus ambient `GITHUB_TOKEN` and `GH_TOKEN`); cover with fixture-based tests.
- Milestone 3, merge plus `generate` plus `--check`: implement deep merge with array policies and the deletion marker (including array-position errors and non-merge literal-plus-warning behavior), format bridges with `append_bottom` unknown-extension fallback and sorted-key output, shared-destination additive combination with conflict errors, always-write atomic generation, `--dry-run`, and `--check` with exit code `2` on diff.
- Milestone 4, `watch`: implement watchexec subscription, per-path regen, config-change restart, debounce, and self-trigger guard.
- Milestone 5, back-propagation: implement full sync including deletions with the in-memory equivalence safety check (exact `Value` equality post-sort, byte-exact text, conflict snapshot to temp, loud non-fatal errors in watch mode), shared-destination contributor scoping with ambiguity errors, and conflict rule template-wins-then-reapply, plus round-trip and mismatch tests.
- Milestone 6, polish and dogfood: adopt templatry for its own `.config`, document source authoring (including header-comment conventions for template authors and the label-split shared-destination pattern), add CI workflows beyond `release-please.yml`, and cut the first release via release-please.

## Future Work

- Multi-source projects (`[source.<name>]`) with deterministic conflict rules and per-template project overrides.
- Additional merge primitives (JSON patch, strategic merge by key, comment-preserving rewrites) and per-template `ignoreKeys` and `ignoreValues` preservation generalized beyond settings files.
- Source lockfile (`templatry.lock`) recording resolved digests, provenance (SLSA or Sigstore) verification, signed sources, and version-update automation (e.g. Renovate).
- `templatry init`, `templatry update`, `templatry diff`, `templatry cache list` and `prune`, and `templatry migrate-from-smartworkspace` conveniences.
- Extended archive formats (`.tar.xz`, `.tar.zst`) on demand.
