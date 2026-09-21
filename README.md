# Templatry

Write Once, Use Everywhere!

Templatry templates and distributes configuration files across your team's repositories. Linter and formatter settings, Renovate manifests, gitignores, and everything else that gets copy-pasted between repos (usually with slight per-repo tweaks) live once in a template source repository. Projects commit only small override files, and everything else generates deterministically. Bump the template version to roll out updates, while keeping full per-project control to deviate where needed.

## How It Works

Two repositories take part:

- A **template source repository** holds canonical configs plus a `templatry.source.toml` describing each template, its merge strategy, and its labels. Its only CI job is `templatry validate`.
- A **project repository** holds a `.config/templatry.toml` pointing at a source plus small override files. Running `templatry generate` merges each template with its override and writes the generated files (`.config/generated/` by default).

```text
template source repo               project repo
templates/settings.json ──┐        .config/templatry.toml  (points at the source)
templatry.source.toml     │        .config/settings.json  (override, optional)
                          │                 │ merge
                          └────────► .config/generated/settings.json
```

## Quick Start

In your template source repository, add `templatry.source.toml`:

```toml
[templates.editor]
template = "settings.json"
labels = ["rust"]
```

In your project repository, add `.config/templatry.toml`:

```toml
[source]
path = "templates"
```

Then generate, and keep a daemon running while you work:

```sh
templatry generate
templatry generate --watch
```

## CLI Reference

```sh
templatry [OPTIONS] [COMMAND]   # bare run aliases generate
templatry generate [--watch] [--check] [--dry-run] [--config <PATH>] [--offline]
templatry validate [--config <PATH>]
templatry cache clear
templatry --watch               # alias for generate --watch
```

- `generate` writes every enabled template on every run (no change detection shortcuts). `--dry-run` prints planned writes instead; `--check` prints differing paths without writing and exits `2` (for CI and pre-commit hooks); `--offline` fails on cache misses instead of fetching. `--watch` reruns on file changes (see Watch Mode). `--check`, `--dry-run`, and `--watch` are mutually exclusive.
- `validate` auto-detects context: with only `templatry.source.toml` present it validates the source; with only `.config/templatry.toml` (or `--config`) present it validates the project and the resolved source. Both files in one directory is an error.
- `cache clear` flushes the whole template cache; the next run re-pulls everything.
- Global flags: `-v`/`-vv` for more logs (`RUST_LOG` overrides), `-q` to silence non-error output.
- Exit codes: `0` success (or no diff for `--check`), `1` runtime or validation failure, `2` differences found by `--check` (or CLI usage errors).

## Project Configuration

`.config/templatry.toml` holds one singular `[source]` table (multi-source projects are future work). Exactly one source kind is set per project. All project-relative paths (`path`, override dirs, generated dirs) resolve against the repository root.

```toml
# Local directory: used in place, never cached.
[source]
path = "../templates"

# GitHub release asset: ref is the release tag, asset is a glob.
[source]
github = "myorg/team-configs"
ref = "v1.2.3"
asset = "configs_*.zip"

# Generic URL archive.
[source]
url = "https://example.com/templates.tar.gz"

# Git checkout: SSH by default, tags or full commit SHAs only.
[source]
git = "git@github.com:myorg/team-configs.git"
ref = "v1.2.3"
# use_https = true   # opt into HTTPS instead of SSH
# root = "templates" # subdirectory holding templatry.source.toml
```

Rules: `ref` is required for GitHub and git kinds and rejected as meaningless for local and URL kinds. Branches are rejected and abbreviated SHAs fail (pin a tag or full 40-character SHA). Private GitHub sources read `TEMPLATRY_GITHUB_TOKEN`, falling back to ambient `GITHUB_TOKEN`/`GH_TOKEN`, for API and download requests only. Labels layer on top of the source defaults:

```toml
[source]
path = "templates"
enable_labels = ["dart"]    # force these on
disable_labels = ["legacy"] # force these off (wins ties)
```

Unknown labels are an error. Individual templates can also be picked directly, on top of label selection: `include_templates` restricts generation to the listed templates (absent means no restriction, present-but-empty disables everything), while `exclude_templates` removes listed ones and wins all ties. Unknown template names are an error.

## Source Configuration

`templatry.source.toml` lives at the source root (or under `root`). Unknown keys are rejected everywhere, so typos fail fast.

```toml
[configs]
default_generated_dir = ".config/generated"
default_override_dir = ".config/"

[templates.editor]
template = "settings.json"       # relative to the source root, must exist
override_file = "settings.json"  # defaults to the template basename
override_dir = ".config/"        # defaults to default_override_dir
local_override_file = "settings.local.json" # optional third layer in override_dir (usually gitignored)
generated_file = "settings.json" # defaults to the template basename
generated_dir = ".config/generated" # defaults to default_generated_dir
strategy = "merge_json"          # merge_json | merge_yaml | merge_toml | append_top | append_bottom | replace | none; default: auto-detect
array_policy = "union"           # union | replace; default: union
back_propagate = false           # watch generated edits back into the override
labels = ["rust"]
# backprop_ignore = ["machine.sdk"]        # hand-edits here stay maintained (never fold, never overwritten)
# backprop_ignore_values = ["editor.font"] # adds/deletes sync, value changes ignored

[default]
# include_labels = ["rust"]  # default-deny except these (empty disables everything)
exclude_labels = ["legacy"]  # default-allow except these; mutually exclusive with include_labels
```

Shared settings collapse into inert `[abstract.*]` tables that concrete templates pull in with `extends` (single abstract only; concrete fields add to or override the base, label sets union, `back_propagate` takes the first set value):

```toml
[abstract.gitignore]
generated_dir = "."
generated_file = ".gitignore"
override_dir = "."
override_file = "template.gitignore"
strategy = "append_bottom"

[templates.gitignore-apps]
template = "apps/template.gitignore"
extends = "gitignore"
labels = ["apps"]
```

Abstracts may define any template field (including `template` itself as a default) but never generate on their own; the merged result must still define a `template` path. Unknown `extends` names fail listing the available abstracts.

## Merge Strategies

Omitted `strategy` auto-detects on the generated filename: `json`, `jsonc`, `yaml`, `yml`, and `toml` merge structurally, everything else appends the override at the bottom. The `merge_*` strategies force a format regardless of filename, for structured files with no usable extension (like `.somethingrc`).

| Strategy        | Behavior |
| --------------- | -------- |
| (auto)          | Structured deep merge for known extensions, else `append_bottom` |
| `merge_json`    | Structured deep merge as JSON (comments stripped) |
| `merge_yaml`    | Structured deep merge as YAML |
| `merge_toml`    | Structured deep merge as TOML |
| `append_top`    | Precedence-ordered segments: local, override, template (missing layers skipped) |
| `append_bottom` | Precedence-ordered segments: template, override, local (missing layers skipped) |
| `replace`       | Override file verbatim (missing override is an error; local warns and is ignored) |
| `none`          | Template file verbatim, ignoring any override (for licenses etc) |

Structured merge recurses through objects with the override winning; scalars and type mismatches replace. A `local_override_file` (just a filename, resolved in the template's `override_dir`) adds a third layer on top for machine-local tweaks that are usually gitignored: it wins over the shared override, is skipped silently when missing, and is never written by back-propagation (which folds into the main override only). Arrays follow `array_policy`: `union` appends override items (scalars deduped, objects always appended, as in `smartworkspace`) while `replace` takes the override array wholesale. Setting a key to `_TEMPLATRY_DELETE_` in the override deletes it (value position only; document-root and in-array uses are errors). A missing or empty override file means the template passes through (except `replace`, which errors).

Structured formats share a JSON intermediate representation: comments are dropped, output keys are sorted for stable diffs, JSONC comments are stripped on read, and TOML datetimes are rejected with the offending key path. Output is deterministic pretty-printed text with one trailing newline.

## Labels

Templates carry opaque labels (`rust`, `dart`, …). The source `[default]` section picks the base: `include_labels` means default-deny except listed, `exclude_labels` means default-allow except listed, neither means everything on. The project's `enable_labels`/`disable_labels` layer on top, with disables winning ties.

## Shared Destinations

Several templates may target the same generated file, so large multi-domain files (like `settings.json`) split across per-ecosystem fragments selected by labels. Combination is order-independent and additive: disjoint keys union, equal values pass, and any conflicting leaf fails naming the path, templates, labels, and both values. Arrays from several contributors must match exactly. `replace` and mixed structured/text strategies cannot share a destination (both are validation errors), as is back-propagation into a shared text destination.

## Back-Propagation

Templates with `back_propagate = true` are two-way in watch mode: hand-edits to the generated file fold back into the override, so the next template bump reproduces them. Additions and changes pin into the override (reverting to the template value cleans the pin back out); deleting a template-held key records `_TEMPLATRY_DELETE_`, while deleting an override-only key removes it. With `local_override_file` set, folds target the main override only (cleanup decisions compare against template-plus-local), and safety replays include the local layer — edits to locally pinned keys fail loudly with a pointer to edit the local file instead.

Per-template ignore lists carve out exceptions (both require `back_propagate`, structured strategies, and dot-separated paths where `*` matches one segment and every pattern covers its subtree): `backprop_ignore` keeps hand-edits fully maintained — never folded, never overwritten or removed on regen; `backprop_ignore_values` syncs existence (adds and deletes propagate) while leaving values alone.

Every fold replays the template pipeline in memory and must reproduce the hand-edited file (key order ignored for structured formats, byte-exact for text) before anything is written. Mismatches fail loudly, leave both files untouched, and save the conflicting content under the system temp directory (`templatry-conflicts/`) for recovery; watch mode logs and keeps running. Known limits: array edits under `union` usually mismatch (the policy re-adds template items — use `replace` or edit the override), and text appends can only fold edits confined to the override portion. When a template update lands with a pending hand-edit, the template wins first and the captured diff reapplies onto the fresh output. One-shot `generate` never folds back: it overwrites hand-edits (and `--check` reports them).

## Watch Mode

`templatry generate --watch` writes the full plan once, then watches override files, the project config, and (for local sources) the source config. Override changes regenerate only their destination group; config changes reload everything and resubscribe (broken configs log and retry, never kill the watcher); `Ctrl-C` quits. Our own writes are guarded against echo loops.

## Cache

Downloaded sources live under the platform cache directory (`~/Library/Caches/templatry` on macOS, `$XDG_CACHE_HOME`/`~/.cache` on Linux) keyed by the SHA-256 of the normalized source configuration, so any source change fetches side by side with older pins and cache hits never touch the network. Entries older than 30 days prune automatically; `templatry cache clear` flushes everything (the remedy for stale plain-HTTP URLs, which cannot observe upstream movement). Cached content is trusted as-is: if you hand-modify it, clear and re-pull.
