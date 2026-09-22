//! Template source retrieval, caching, and integrity (Milestone 2).
//!
//! Four fetchers (local directory, GitHub release, generic URL archive, git
//! checkout) behind [`resolve`], with a content-addressed cache: entries live
//! under `<platform-cache-dir>/templatry/<sha256-of-canonical-source-config>/`,
//! are looked up before any fetch, carry a sidecar with last-use timestamp
//! plus resolved git SHA, and are pruned automatically after 30 days.
//! `TEMPLATRY_GITHUB_TOKEN` (falling back to ambient `GITHUB_TOKEN`/`GH_TOKEN`)
//! authenticates GitHub API and asset-download HTTPS requests only; git
//! subprocesses inherit the parent environment untouched.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::Digest;

use crate::config::{
    SOURCE_CONFIG_FILENAME, SourceKind, SourceRef, is_full_sha, validate_git_transport,
};

/// Directory name for cached sources inside the platform cache dir.
pub const CACHE_DIR_NAME: &str = "templatry";

/// Per-entry sidecar filename: JSON with `last_used_unix` and `resolved_sha`.
pub const ENTRY_SIDECAR_FILENAME: &str = ".templatry-meta.json";

/// Cache entries older than this (by sidecar timestamp) are pruned automatically.
pub const CACHE_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Pinned GitHub API version header.
const GITHUB_API_VERSION: &str = "2022-11-28";

/// Maximum release asset names listed in glob-mismatch diagnostics.
const MAX_LISTED_ASSETS: usize = 20;

/// A resolved template source ready for validation or generation.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// Source kind that was resolved.
    pub kind: SourceKind,
    /// Directory holding `templatry.source.toml` (cache entry or local path).
    pub root_dir: PathBuf,
    /// Whether the entry came from the cache without fetching.
    pub from_cache: bool,
    /// Hex cache key over the canonical source configuration.
    pub source_id: String,
}

/// One named project source after resolution: its configured name plus the
/// resolution result. The name is empty for singular `[source]` projects.
#[derive(Debug, Clone)]
pub struct NamedSource {
    /// Configured source name (`""` for singular projects).
    pub name: String,
    /// Source kind that was resolved.
    pub kind: SourceKind,
    /// Directory holding `templatry.source.toml` (cache entry or local path).
    pub root_dir: PathBuf,
    /// Whether the entry came from the cache without fetching.
    pub from_cache: bool,
    /// Hex cache key over the canonical source configuration.
    pub source_id: String,
}

/// Resolve every source a project declares, in [`crate::config::ProjectConfig::project_sources`]
/// order (singular first, else alphabetically by name).
///
/// Each source resolves independently through [`resolve`]; failures name the
/// source they came from. Callers merge the results after per-source
/// validation (see [`crate::config::merge_active_templates`]).
pub async fn resolve_all(
    project: &crate::config::ProjectConfig,
    project_root: &Path,
    offline: bool,
) -> crate::Result<Vec<NamedSource>> {
    let mut resolved = Vec::new();
    for (name, source) in project.project_sources() {
        let single = resolve(source, project_root, offline)
            .await
            .map_err(|err| {
                if name.is_empty() {
                    return err;
                }
                match err {
                    crate::Error::Invalid { path, message } => crate::Error::Invalid {
                        path,
                        message: format!("source `{name}`: {message}"),
                    },
                    crate::Error::Parse { path, message } => crate::Error::Parse {
                        path,
                        message: format!("source `{name}`: {message}"),
                    },
                    other => other,
                }
            })?;
        resolved.push(NamedSource {
            name,
            kind: single.kind,
            root_dir: single.root_dir,
            from_cache: single.from_cache,
            source_id: single.source_id,
        });
    }
    Ok(resolved)
}

/// Resolve a source reference against the cache, fetching on miss.
///
/// `project_root` is the repository root (see [`crate::config::project_root`]):
/// local `path` sources resolve against it. Local directories resolve in
/// place with no caching. Remote kinds hit the cache by source-id hash first;
/// on miss they fetch (unless `offline`, which fails naming the missing
/// source), extract into the entry, and record the sidecar. Stale entries are
/// pruned on every call. The returned `root_dir` is guaranteed to hold
/// `templatry.source.toml`.
pub async fn resolve(
    source: &SourceRef,
    project_root: &Path,
    offline: bool,
) -> crate::Result<ResolvedSource> {
    resolve_in(source, project_root, offline, &cache_root()).await
}

/// [`resolve`] with an explicit cache root (the test seam).
pub(crate) async fn resolve_in(
    source: &SourceRef,
    project_root: &Path,
    offline: bool,
    cache_root: &Path,
) -> crate::Result<ResolvedSource> {
    let kind = source.kind()?;
    validate_git_transport(source)?;
    let source_id = source_id(source, kind)?;

    if kind == SourceKind::LocalDir {
        let local = source.path.as_deref().expect("kind() guarantees `path`");
        let base = project_root.join(local).canonicalize().map_err(|err| {
            crate::invalid(
                project_root,
                format!("source directory `{local}` cannot be resolved: {err}"),
            )
        })?;
        let root_dir = join_root(&base, source.root.as_deref());
        ensure_source_file(&root_dir, source.root.as_deref())?;
        return Ok(ResolvedSource {
            kind,
            root_dir,
            from_cache: false,
            source_id,
        });
    }

    let entry = cache_root.join(&source_id);
    prune_stale_entries(cache_root);
    if entry_is_fresh(&entry) {
        refresh_sidecar(&entry)?;
        let root_dir = join_root(&entry, source.root.as_deref());
        ensure_source_file(&root_dir, source.root.as_deref())?;
        tracing::debug!(source_id = %source_id, "cache hit");
        return Ok(ResolvedSource {
            kind,
            root_dir,
            from_cache: true,
            source_id,
        });
    }
    if offline {
        return Err(crate::invalid(
            project_root,
            format!(
                "no cached {kind} source for this configuration and `--offline` was given: re-run without `--offline` to fetch it"
            ),
        ));
    }
    tracing::info!(source_id = %source_id, kind = %kind, "fetching template source");
    let resolved_sha = fetch_to_entry(kind, source, &entry).await?;
    write_sidecar(&entry, source, kind, resolved_sha)?;
    let root_dir = join_root(&entry, source.root.as_deref());
    ensure_source_file(&root_dir, source.root.as_deref())?;
    Ok(ResolvedSource {
        kind,
        root_dir,
        from_cache: false,
        source_id,
    })
}

// ---- Cache keys -------------------------------------------------------------

/// Deterministic cache key: SHA-256 hex over the canonical normalized config.
///
/// Normalization (trailing slashes, defaulted `root`, equivalent spellings)
/// means formatting-only edits never invalidate the cache, while any
/// fetch-affecting change yields a new key.
pub(crate) fn source_id(source: &SourceRef, kind: SourceKind) -> crate::Result<String> {
    let (tag, location) = match kind {
        SourceKind::LocalDir => ("path", source.path.as_deref()),
        SourceKind::GitHubRelease => ("github", source.github.as_deref()),
        SourceKind::UrlArchive => ("url", source.url.as_deref()),
        SourceKind::GitCheckout => ("git", source.git.as_deref()),
    };
    let location = location.expect("kind() guarantees the location field");
    let document = serde_json::json!({
        "kind": tag,
        "location": location.trim().trim_end_matches('/'),
        "ref": source.r#ref.as_deref().unwrap_or_default(),
        "root": normalize_root(source.root.as_deref()),
        "asset": source.asset.as_deref().unwrap_or_default(),
        "use_https": source.use_https.unwrap_or(false),
    });
    let bytes = serde_json::to_vec(&document).map_err(|err| {
        crate::invalid(
            Path::new("templatry.toml"),
            format!("cannot hash source configuration: {err}"),
        )
    })?;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

/// Platform cache root (`<cache-dir>/templatry`), following `mise` behavior.
pub(crate) fn cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(CACHE_DIR_NAME)
}

/// Join the optional `root` subtree onto a fetched source base.
fn join_root(base: &Path, root: Option<&str>) -> PathBuf {
    let normalized = normalize_root(root);
    if normalized.is_empty() {
        base.to_path_buf()
    } else {
        base.join(normalized)
    }
}

/// Canonical `root` form: trimmed, unslashed, with `.` meaning the base.
///
/// Shared by the cache key and path resolution so equivalent spellings hit
/// the same entry.
fn normalize_root(root: Option<&str>) -> &str {
    match root.map(str::trim).map(|root| root.trim_matches('/')) {
        None | Some("") | Some(".") => "",
        Some(root) => root,
    }
}

/// The resolved directory must hold the source config file.
fn ensure_source_file(root_dir: &Path, root: Option<&str>) -> crate::Result<()> {
    if root_dir.join(SOURCE_CONFIG_FILENAME).is_file() {
        return Ok(());
    }
    let hint = match normalize_root(root) {
        "" => String::new(),
        root => format!(" (with `root = \"{root}\"`)"),
    };
    Err(crate::invalid(
        root_dir,
        format!("no `{SOURCE_CONFIG_FILENAME}` under the resolved source{hint}: check `root`"),
    ))
}

// ---- Sidecar, prune, clear --------------------------------------------------

/// Per-entry metadata: last-use timestamp plus resolved git SHA (git only).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct EntryMeta {
    last_used_unix: u64,
    #[serde(default)]
    resolved_sha: Option<String>,
    /// What was fetched (absent on entries written before provenance).
    #[serde(default)]
    source: Option<EntrySource>,
}

/// Where a cache entry came from, for `cache list` display.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct EntrySource {
    /// Source kind tag (`github`, `url`, or `git`; local sources never cache).
    kind: String,
    /// Canonical location (org/repo, URL, or git remote).
    location: String,
    /// Release tag or commit SHA (github, git).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    r#ref: String,
    /// Source-root subdirectory holding `templatry.source.toml`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    root: String,
    /// Release asset glob (github only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    asset: String,
}

impl EntrySource {
    /// Canonical provenance, normalized like the cache key.
    fn describe(source: &crate::config::SourceRef, kind: SourceKind) -> Self {
        let tag = match kind {
            SourceKind::LocalDir => "path",
            SourceKind::GitHubRelease => "github",
            SourceKind::UrlArchive => "url",
            SourceKind::GitCheckout => "git",
        };
        let location = match kind {
            SourceKind::LocalDir => source.path.as_deref(),
            SourceKind::GitHubRelease => source.github.as_deref(),
            SourceKind::UrlArchive => source.url.as_deref(),
            SourceKind::GitCheckout => source.git.as_deref(),
        };
        Self {
            kind: tag.to_string(),
            location: location
                .unwrap_or_default()
                .trim()
                .trim_end_matches('/')
                .to_string(),
            r#ref: source.r#ref.as_deref().unwrap_or_default().to_string(),
            root: normalize_root(source.root.as_deref()).to_string(),
            asset: source.asset.as_deref().unwrap_or_default().to_string(),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_sidecar(entry: &Path) -> Option<EntryMeta> {
    let bytes = std::fs::read(entry.join(ENTRY_SIDECAR_FILENAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn entry_is_fresh(entry: &Path) -> bool {
    read_sidecar(entry)
        .is_some_and(|meta| now_unix().saturating_sub(meta.last_used_unix) <= CACHE_TTL_SECS)
}

fn refresh_sidecar(entry: &Path) -> crate::Result<()> {
    let mut meta = read_sidecar(entry).unwrap_or(EntryMeta {
        last_used_unix: 0,
        resolved_sha: None,
        source: None,
    });
    meta.last_used_unix = now_unix();
    write_sidecar_inner(entry, &meta)
}

fn write_sidecar(
    entry: &Path,
    source: &crate::config::SourceRef,
    kind: SourceKind,
    resolved_sha: Option<String>,
) -> crate::Result<()> {
    write_sidecar_inner(
        entry,
        &EntryMeta {
            last_used_unix: now_unix(),
            resolved_sha,
            source: Some(EntrySource::describe(source, kind)),
        },
    )
}

fn write_sidecar_inner(entry: &Path, meta: &EntryMeta) -> crate::Result<()> {
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|err| crate::invalid(entry, format!("cannot serialize cache metadata: {err}")))?;
    std::fs::write(entry.join(ENTRY_SIDECAR_FILENAME), bytes)
        .map_err(|err| crate::invalid(entry, format!("cannot write cache metadata: {err}")))?;
    Ok(())
}

/// Remove entries with missing, unreadable, or over-TTL sidecars.
///
/// Best-effort: individual failures are logged, never fatal.
pub(crate) fn prune_stale_entries(cache_root: &Path) {
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let stale = match read_sidecar(&path) {
            Some(meta) => now_unix().saturating_sub(meta.last_used_unix) > CACHE_TTL_SECS,
            None => true,
        };
        if !stale {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::debug!(path = %path.display(), "pruned stale cache entry"),
            Err(err) => {
                tracing::warn!(path = %path.display(), "cannot prune stale cache entry: {err}");
            }
        }
    }
}

/// Flush the entire cache; the next run re-pulls all sources.
pub fn cache_clear() -> crate::Result<()> {
    cache_clear_in(&cache_root())
}

/// [`cache_clear`] with an explicit cache root (the test seam).
pub(crate) fn cache_clear_in(cache_root: &Path) -> crate::Result<()> {
    if !cache_root.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(cache_root)
        .map_err(|err| crate::invalid(cache_root, format!("cannot clear cache: {err}")))?;
    Ok(())
}

/// One cached template source for `cache list`.
///
/// Provenance (`kind` and below) is `None` on entries written before it was
/// recorded; only the entry key and size are always known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedSource {
    /// Full hex cache key (the entry directory name).
    pub source_id: String,
    /// Source kind tag (`github`, `url`, or `git`).
    pub kind: Option<String>,
    /// Canonical location (org/repo, URL, or git remote).
    pub location: Option<String>,
    /// Release tag or commit SHA (github, git).
    pub r#ref: Option<String>,
    /// Source-root subdirectory holding `templatry.source.toml`.
    pub root: Option<String>,
    /// Release asset glob (github only).
    pub asset: Option<String>,
    /// Last-use unix timestamp; `None` when the sidecar is missing.
    pub last_used_unix: Option<u64>,
    /// Resolved git SHA (git checkouts only).
    pub resolved_sha: Option<String>,
    /// Recursive entry size in bytes (best effort).
    pub size_bytes: u64,
    /// Absolute path to the cache entry directory.
    pub path: PathBuf,
}

/// List cached template sources, most recently used first.
///
/// Best effort throughout: a missing cache root lists as empty, and entries
/// with unreadable sidecars still appear with unknown provenance.
pub fn cache_list() -> Vec<CachedSource> {
    cache_list_in(&cache_root())
}

/// [`cache_list`] with an explicit cache root (the test seam).
pub(crate) fn cache_list_in(cache_root: &Path) -> Vec<CachedSource> {
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return Vec::new();
    };
    let mut listed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(source_id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let meta = read_sidecar(&path);
        let provenance = meta.as_ref().and_then(|meta| meta.source.as_ref());
        let present = |value: &str| (!value.is_empty()).then(|| value.to_string());
        listed.push(CachedSource {
            source_id: source_id.to_string(),
            kind: provenance.map(|provenance| provenance.kind.clone()),
            location: provenance.map(|provenance| provenance.location.clone()),
            r#ref: provenance.and_then(|provenance| present(&provenance.r#ref)),
            root: provenance.and_then(|provenance| present(&provenance.root)),
            asset: provenance.and_then(|provenance| present(&provenance.asset)),
            last_used_unix: meta.as_ref().map(|meta| meta.last_used_unix),
            resolved_sha: meta.as_ref().and_then(|meta| meta.resolved_sha.clone()),
            size_bytes: entry_size(&path),
            path: path.clone(),
        });
    }
    listed.sort_by(|left, right| {
        (right.last_used_unix, &left.source_id).cmp(&(left.last_used_unix, &right.source_id))
    });
    listed
}

/// Recursive directory size in bytes (symlinks counted, never followed).
fn entry_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && !meta.is_symlink() {
                stack.push(path);
            } else {
                total += meta.len();
            }
        }
    }
    total
}

// ---- Auth -------------------------------------------------------------------

/// Resolve the GitHub token: explicit override wins, ambient env is fallback.
///
/// Pure over the lookup function so precedence is unit-testable; production
/// passes [`std::env::var`]. Used for GitHub API and asset-download HTTPS
/// requests only — never injected into git subprocesses.
fn token_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["TEMPLATRY_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"]
        .into_iter()
        .find_map(|key| get(key).filter(|value| !value.is_empty()))
}

pub(crate) fn github_token() -> Option<String> {
    token_from(|key| std::env::var(key).ok())
}

// ---- Fetch dispatch ---------------------------------------------------------

/// Fetch into a cache entry, removing partial results on failure.
///
/// Returns the resolved commit SHA for git sources, `None` otherwise.
async fn fetch_to_entry(
    kind: SourceKind,
    source: &SourceRef,
    entry: &Path,
) -> crate::Result<Option<String>> {
    if let Some(parent) = entry.parent()
        && !parent.exists()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Err(crate::invalid(
            parent,
            format!("cannot create cache directory: {err}"),
        ));
    }
    let outcome = match kind {
        SourceKind::GitHubRelease => fetch_github(source, entry).await,
        SourceKind::UrlArchive => fetch_url(source, entry).await,
        SourceKind::GitCheckout => fetch_git(source, entry).await,
        SourceKind::LocalDir => unreachable!("local sources never reach the fetcher"),
    };
    if outcome.is_err() {
        let _ = tokio::fs::remove_dir_all(entry).await;
    }
    outcome
}

// ---- GitHub release ---------------------------------------------------------

/// A release asset: name plus direct download URL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseAsset {
    name: String,
    browser_url: String,
}

/// Extract asset names and download URLs from a get-release-by-tag response.
fn parse_release_assets(
    body: &serde_json::Value,
    repo: &str,
    tag: &str,
) -> crate::Result<Vec<ReleaseAsset>> {
    let context = Path::new("api.github.com");
    let assets = body
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            crate::invalid(
                context,
                format!("unexpected release response for `{repo}@{tag}`: missing `assets`"),
            )
        })?;
    let parsed: Vec<ReleaseAsset> = assets
        .iter()
        .filter_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            let browser_url = asset.get("browser_download_url")?.as_str()?;
            Some(ReleaseAsset {
                name: name.to_string(),
                browser_url: browser_url.to_string(),
            })
        })
        .collect();
    if parsed.is_empty() {
        return Err(crate::invalid(
            context,
            format!("release `{tag}` in `{repo}` has no downloadable assets"),
        ));
    }
    Ok(parsed)
}

/// Default source archive URL from a get-release-by-tag response.
///
/// The `assets` array only lists *uploaded* files: releases with none still
/// ship GitHub's auto-generated archives, advertised as `tarball_url` (and
/// `zipball_url`). The tarball (`.tar.gz`) is the conventional default.
fn release_tarball_url<'a>(
    body: &'a serde_json::Value,
    repo: &str,
    tag: &str,
) -> crate::Result<&'a str> {
    body.get("tarball_url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            crate::invalid(
                Path::new("api.github.com"),
                format!("unexpected release response for `{repo}@{tag}`: missing `tarball_url`"),
            )
        })
}

/// Lift a lone top-level directory's contents into `entry`.
///
/// GitHub source archives wrap the tree in one directory (`{repo}-{tag}/`):
/// unwrapping it puts `templatry.source.toml` at the entry root. Anything
/// else (several entries, a lone file) is left for downstream checks.
fn lift_single_top_level(entry: &Path) -> crate::Result<()> {
    let mut members = std::fs::read_dir(entry)
        .map_err(|err| crate::invalid(entry, format!("cannot list extracted archive: {err}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| crate::invalid(entry, format!("cannot list extracted archive: {err}")))?;
    if members.len() == 1 && members[0].path().is_dir() {
        let top = members.pop().expect("one directory entry");
        let children = std::fs::read_dir(top.path())
            .map_err(|err| crate::invalid(entry, format!("cannot list extracted archive: {err}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                crate::invalid(entry, format!("cannot list extracted archive: {err}"))
            })?;
        for child in children {
            let target = entry.join(child.file_name());
            std::fs::rename(child.path(), &target).map_err(|err| {
                crate::invalid(
                    entry,
                    format!("cannot unwrap archived top-level directory: {err}"),
                )
            })?;
        }
        std::fs::remove_dir(top.path()).map_err(|err| {
            crate::invalid(
                entry,
                format!("cannot unwrap archived top-level directory: {err}"),
            )
        })?;
    }
    Ok(())
}
/// Select the single asset matching the `asset` glob.
///
/// Zero matches and multi-matches both fail; the multi-match error lists the
/// candidates so the pattern gets tightened.
fn select_asset<'a>(assets: &'a [ReleaseAsset], pattern: &str) -> crate::Result<&'a ReleaseAsset> {
    let context = Path::new("templatry.toml");
    let matcher = glob::Pattern::new(pattern).map_err(|err| {
        crate::invalid(context, format!("invalid `asset` glob `{pattern}`: {err}"))
    })?;
    let matched: Vec<&ReleaseAsset> = assets
        .iter()
        .filter(|asset| matcher.matches(&asset.name))
        .collect();
    match matched.len() {
        1 => Ok(matched[0]),
        0 => {
            let available: Vec<&str> = assets.iter().map(|asset| asset.name.as_str()).collect();
            Err(crate::invalid(
                context,
                format!(
                    "`asset` glob `{pattern}` matched no release assets. Available: {}",
                    asset_names(&available)
                ),
            ))
        }
        count => {
            let candidates: Vec<&str> = matched.iter().map(|asset| asset.name.as_str()).collect();
            Err(crate::invalid(
                context,
                format!(
                    "`asset` glob `{pattern}` matched {count} assets ({}): tighten the pattern to match exactly one",
                    asset_names(&candidates)
                ),
            ))
        }
    }
}

fn asset_names(names: &[&str]) -> String {
    let mut listed: Vec<String> = names
        .iter()
        .take(MAX_LISTED_ASSETS + 1)
        .map(|name| format!("`{name}`"))
        .collect();
    if names.len() > MAX_LISTED_ASSETS {
        listed.pop();
        listed.push(format!("and {} more", names.len() - MAX_LISTED_ASSETS));
    }
    listed.join(", ")
}

/// Percent-encode a URL path segment (release tags may contain slashes).
fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn http_client() -> crate::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("templatry/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| {
            crate::invalid(
                Path::new("https"),
                format!("cannot build HTTP client: {err}"),
            )
        })
}

async fn fetch_github(source: &SourceRef, entry: &Path) -> crate::Result<Option<String>> {
    let repo = source
        .github
        .as_deref()
        .expect("kind() guarantees `github`");
    let tag = source.r#ref.as_deref().expect("kind() guarantees `ref`");
    let pattern = source.asset.as_deref().filter(|asset| !asset.is_empty());
    let token = github_token();
    let client = http_client()?;

    let api_url = format!(
        "https://api.github.com/repos/{repo}/releases/tags/{}",
        encode_path_segment(tag)
    );
    let mut request = client
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
    if let Some(token) = token.clone() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|err| {
        crate::invalid(
            Path::new("api.github.com"),
            format!("cannot reach GitHub API for `{repo}@{tag}`: {err}"),
        )
    })?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(crate::invalid(
            Path::new("api.github.com"),
            format!(
                "release `{tag}` not found in `{repo}`: check `ref` (private repositories need TEMPLATRY_GITHUB_TOKEN, GITHUB_TOKEN, or GH_TOKEN)"
            ),
        ));
    }
    let response = response.error_for_status().map_err(|err| {
        crate::invalid(
            Path::new("api.github.com"),
            format!("GitHub API request for `{repo}@{tag}` failed: {err}"),
        )
    })?;
    let body: serde_json::Value = response.json().await.map_err(|err| {
        crate::invalid(
            Path::new("api.github.com"),
            format!("cannot parse GitHub release response: {err}"),
        )
    })?;
    let (display, url, kind, accept) = match pattern {
        Some(pattern) => {
            let assets = parse_release_assets(&body, repo, tag)?;
            let chosen = select_asset(&assets, pattern)?;
            tracing::info!(asset = %chosen.name, "downloading release asset");
            (
                format!("asset `{}`", chosen.name),
                chosen.browser_url.clone(),
                ArchiveKind::detect(&chosen.name)?,
                // Required by the release-asset download endpoint.
                "application/octet-stream",
            )
        }
        // No `asset` glob: the release's default source archive. Uploaded
        // assets are optional on GitHub, so `assets` may legitimately be
        // empty here — the tarball always exists.
        None => {
            let tarball_url = release_tarball_url(&body, repo, tag)?;
            tracing::info!(%repo, %tag, "downloading default source archive (tarball)");
            (
                format!("source archive for `{repo}@{tag}`"),
                tarball_url.to_string(),
                ArchiveKind::TarGz,
                // Versioned API media type: `application/octet-stream`
                // answers 415 on this endpoint.
                "application/vnd.github+json",
            )
        }
    };

    let mut request = client.get(&url).header("Accept", accept);
    if pattern.is_none() {
        request = request.header("X-GitHub-Api-Version", GITHUB_API_VERSION);
    }
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|err| {
        crate::invalid(
            Path::new("api.github.com"),
            format!("cannot download {display}: {err}"),
        )
    })?;
    let response = response.error_for_status().map_err(|err| {
        crate::invalid(
            Path::new("api.github.com"),
            format!("{display} download failed: {err}"),
        )
    })?;
    let bytes = response.bytes().await.map_err(|err| {
        crate::invalid(
            Path::new("api.github.com"),
            format!("cannot read {display}: {err}"),
        )
    })?;

    extract_archive(kind, bytes.to_vec(), entry).await?;
    if pattern.is_none() {
        lift_single_top_level(entry)?;
    }
    Ok(None)
}

// ---- Generic URL ------------------------------------------------------------

async fn fetch_url(source: &SourceRef, entry: &Path) -> crate::Result<Option<String>> {
    let url = source.url.as_deref().expect("kind() guarantees `url`");
    let client = http_client()?;
    let response =
        client.get(url).send().await.map_err(|err| {
            crate::invalid(Path::new(url), format!("cannot download archive: {err}"))
        })?;
    let response = response
        .error_for_status()
        .map_err(|err| crate::invalid(Path::new(url), format!("archive download failed: {err}")))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|err| crate::invalid(Path::new(url), format!("cannot read archive: {err}")))?;

    let filename = url
        .split(['?', '#'])
        .next()
        .and_then(|base| base.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            crate::invalid(
                Path::new(url),
                "cannot determine an archive filename from the URL to detect its format"
                    .to_string(),
            )
        })?;
    extract_archive(ArchiveKind::detect(filename)?, bytes.to_vec(), entry).await?;
    Ok(None)
}

// ---- Git checkout -----------------------------------------------------------

/// Run git, mapping spawn failures to install instructions.
async fn run_git(args: &[&str], context: &str) -> crate::Result<std::process::Output> {
    tokio::process::Command::new("git")
        .args(args)
        .output()
        .await
        .map_err(|err| {
            crate::invalid(
                Path::new("git"),
                format!(
                    "cannot run git ({context}): {err} (install git to use git checkout sources)"
                ),
            )
        })
}

/// Fail with git's stderr when a git invocation exits nonzero.
fn ensure_git_success(output: &std::process::Output, context: &str) -> crate::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(crate::invalid(
        Path::new("git"),
        format!("git {context} failed: {stderr}"),
    ))
}

/// Enforce tags-or-SHA: full SHAs skip the check, tags must match
/// `refs/tags/*`, branch-only refs fail loudly, the rest is ref-not-found.
async fn check_tag_or_branch(url: &str, r#ref: &str) -> crate::Result<()> {
    let tags = run_git(&["ls-remote", "--tags", "--", url, r#ref], "tag lookup").await?;
    ensure_git_success(&tags, "tag lookup")?;
    let is_tag = String::from_utf8_lossy(&tags.stdout)
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .map(|name| name.strip_suffix("^{}").unwrap_or(name))
        .any(|name| name == format!("refs/tags/{ref}"));
    if is_tag {
        return Ok(());
    }

    let heads = run_git(&["ls-remote", "--heads", "--", url, r#ref], "branch lookup").await?;
    ensure_git_success(&heads, "branch lookup")?;
    let is_branch = String::from_utf8_lossy(&heads.stdout)
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .any(|name| name == format!("refs/heads/{ref}"));
    if is_branch {
        return Err(crate::invalid(
            Path::new("templatry.toml"),
            format!(
                "source `ref` `{ref}` is a branch: branches are rejected, pin a tag or full commit SHA"
            ),
        ));
    }
    Err(crate::invalid(
        Path::new("templatry.toml"),
        format!("source `ref` `{ref}` matches no tag in the repository: check the value"),
    ))
}

async fn fetch_git(source: &SourceRef, entry: &Path) -> crate::Result<Option<String>> {
    run_git(&["--version"], "version probe").await?;
    let url = source.git.as_deref().expect("kind() guarantees `git`");
    let r#ref = source.r#ref.as_deref().expect("kind() guarantees `ref`");

    if !is_full_sha(r#ref) {
        check_tag_or_branch(url, r#ref).await?;
    }
    if is_full_sha(r#ref) {
        // Shallow fetch of the exact commit, then detach at it.
        let output = run_git(&["init", &entry.to_string_lossy()], "init").await?;
        ensure_git_success(&output, "init")?;
        let entry_arg = entry.to_string_lossy();
        let output = run_git(
            &["-C", &entry_arg, "remote", "add", "origin", "--", url],
            "remote add",
        )
        .await?;
        ensure_git_success(&output, "remote add")?;
        let output = run_git(
            &["-C", &entry_arg, "fetch", "--depth", "1", "origin", r#ref],
            "fetch",
        )
        .await?;
        ensure_git_success(&output, "fetch")?;
        let output = run_git(&["-C", &entry_arg, "checkout", "FETCH_HEAD"], "checkout").await?;
        ensure_git_success(&output, "checkout")?;
    } else {
        let output = run_git(
            &[
                "clone",
                "--depth",
                "1",
                "--branch",
                r#ref,
                "--",
                url,
                &entry.to_string_lossy(),
            ],
            "clone",
        )
        .await?;
        ensure_git_success(&output, "clone")?;
    }

    let entry_arg = entry.to_string_lossy();
    let output = run_git(&["-C", &entry_arg, "rev-parse", "HEAD"], "rev-parse").await?;
    ensure_git_success(&output, "rev-parse")?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(Some(sha))
}

// ---- Archives ---------------------------------------------------------------

/// Supported archive formats (`.tar.xz`/`.tar.zst` deferred until needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveKind {
    TarGz,
    Tar,
    Zip,
}

impl ArchiveKind {
    /// Detect the format from a filename (`.tgz` aliases `.tar.gz`).
    pub(crate) fn detect(filename: &str) -> crate::Result<Self> {
        let lower = filename.to_lowercase();
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Ok(Self::TarGz)
        } else if lower.ends_with(".tar") {
            Ok(Self::Tar)
        } else if lower.ends_with(".zip") {
            Ok(Self::Zip)
        } else {
            Err(crate::invalid(
                Path::new(filename),
                format!(
                    "cannot determine archive format of `{filename}`: expected .tar.gz, .tgz, .tar, or .zip"
                ),
            ))
        }
    }
}

/// Extract an archive into `dest` (blocking work runs on the blocking pool).
pub(crate) async fn extract_archive(
    kind: ArchiveKind,
    bytes: Vec<u8>,
    dest: &Path,
) -> crate::Result<()> {
    let dest = dest.to_path_buf();
    let failed_at = dest.clone();
    tokio::task::spawn_blocking(move || extract_blocking(kind, &bytes, &dest))
        .await
        .map_err(|err| {
            crate::invalid(&failed_at, format!("archive extraction task failed: {err}"))
        })?
}

fn extract_blocking(kind: ArchiveKind, bytes: &[u8], dest: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(dest)
        .map_err(|err| crate::invalid(dest, format!("cannot create cache entry: {err}")))?;
    match kind {
        ArchiveKind::TarGz => {
            let decoder = flate2::read::GzDecoder::new(bytes);
            let mut archive = tar::Archive::new(decoder);
            archive.unpack(dest).map_err(|err| {
                crate::invalid(dest, format!("cannot unpack tar.gz archive: {err}"))
            })?;
        }
        ArchiveKind::Tar => {
            let mut archive = tar::Archive::new(bytes);
            archive
                .unpack(dest)
                .map_err(|err| crate::invalid(dest, format!("cannot unpack tar archive: {err}")))?;
        }
        ArchiveKind::Zip => {
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
                .map_err(|err| crate::invalid(dest, format!("cannot open zip archive: {err}")))?;
            archive
                .extract(dest)
                .map_err(|err| crate::invalid(dest, format!("cannot unpack zip archive: {err}")))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;

    fn parse_project_ref(body: &str) -> SourceRef {
        let project: ProjectConfig =
            toml::from_str(&format!("[source]\n{body}")).expect("project fixture parses");
        project.source.expect("singular test project")
    }

    fn temp_cache() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn source_id_is_stable_and_unique() {
        let first = parse_project_ref("github = \"org/repo\"\nref = \"v1\"\nasset = \"c_*.zip\"\n");
        let kind = first.kind().unwrap();
        let one = source_id(&first, kind).unwrap();
        let two = source_id(&first, kind).unwrap();
        assert_eq!(one, two);
        assert_eq!(one.len(), 64);

        let changed =
            parse_project_ref("github = \"org/repo\"\nref = \"v2\"\nasset = \"c_*.zip\"\n");
        let other = source_id(&changed, changed.kind().unwrap()).unwrap();
        assert_ne!(one, other);
    }

    #[test]
    fn source_id_normalizes_equivalent_configs() {
        let plain = parse_project_ref("url = \"https://example.com/t.tar.gz\"\n");
        let slashed = parse_project_ref("url = \"https://example.com/t.tar.gz/\"\n");
        let rooted =
            parse_project_ref("url = \"https://example.com/t.tar.gz\"\nroot = \"/templates/\"\n");
        let bare =
            parse_project_ref("url = \"https://example.com/t.tar.gz\"\nroot = \"templates\"\n");
        let plain_id = source_id(&plain, plain.kind().unwrap()).unwrap();
        assert_eq!(
            plain_id,
            source_id(&slashed, slashed.kind().unwrap()).unwrap()
        );
        assert_eq!(rooted.kind().unwrap(), bare.kind().unwrap());
        assert_eq!(
            source_id(&rooted, rooted.kind().unwrap()).unwrap(),
            source_id(&bare, bare.kind().unwrap()).unwrap()
        );
    }

    #[test]
    fn token_precedence_prefers_explicit_override() {
        let get = |key: &str| match key {
            "TEMPLATRY_GITHUB_TOKEN" => Some("explicit".to_string()),
            "GITHUB_TOKEN" => Some("ambient".to_string()),
            "GH_TOKEN" => Some("fallback".to_string()),
            _ => None,
        };
        assert_eq!(token_from(get).as_deref(), Some("explicit"));

        let get = |key: &str| match key {
            "GITHUB_TOKEN" => Some("ambient".to_string()),
            "GH_TOKEN" => Some("fallback".to_string()),
            _ => None,
        };
        assert_eq!(token_from(get).as_deref(), Some("ambient"));

        let get = |key: &str| match key {
            "TEMPLATRY_GITHUB_TOKEN" => Some(String::new()),
            "GH_TOKEN" => Some("fallback".to_string()),
            _ => None,
        };
        assert_eq!(token_from(get).as_deref(), Some("fallback"));
        assert!(token_from(|_| None).is_none());
    }

    #[test]
    fn asset_selection_single_zero_multi() {
        let assets = [
            ReleaseAsset {
                name: "configs_v1.zip".to_string(),
                browser_url: "https://example.com/a".to_string(),
            },
            ReleaseAsset {
                name: "configs_v2.zip".to_string(),
                browser_url: "https://example.com/b".to_string(),
            },
        ];
        let chosen = select_asset(&assets, "configs_v2.zip").unwrap();
        assert_eq!(chosen.browser_url, "https://example.com/b");

        let err = select_asset(&assets, "missing_*.zip").unwrap_err();
        assert!(
            err.to_string().contains("matched no release assets"),
            "{err:?}"
        );
        assert!(err.to_string().contains("configs_v1.zip"), "{err:?}");

        let err = select_asset(&assets, "configs_*.zip").unwrap_err();
        assert!(err.to_string().contains("matched 2 assets"), "{err:?}");

        let err = select_asset(&assets, "[broken").unwrap_err();
        assert!(err.to_string().contains("invalid `asset` glob"), "{err:?}");
    }

    #[test]
    fn release_assets_parse_and_reject() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"assets": [
                {"name": "a.zip", "browser_download_url": "https://x/a"},
                {"name": "b.zip", "browser_download_url": "https://x/b"},
                {"name": "broken"}
            ]}"#,
        )
        .unwrap();
        let assets = parse_release_assets(&body, "org/repo", "v1").unwrap();
        assert_eq!(assets.len(), 2);

        let body: serde_json::Value = serde_json::from_str(r#"{"nope": []}"#).unwrap();
        let err = parse_release_assets(&body, "org/repo", "v1").unwrap_err();
        assert!(err.to_string().contains("missing `assets`"), "{err:?}");

        let body: serde_json::Value = serde_json::from_str(r#"{"assets": []}"#).unwrap();
        let err = parse_release_assets(&body, "org/repo", "v1").unwrap_err();
        assert!(
            err.to_string().contains("no downloadable assets"),
            "{err:?}"
        );
    }

    #[test]
    fn default_archive_uses_tarball_url() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"assets": [], "tarball_url": "https://api.github.com/repos/o/r/tarball/v1"}"#,
        )
        .unwrap();
        assert_eq!(
            release_tarball_url(&body, "org/repo", "v1").unwrap(),
            "https://api.github.com/repos/o/r/tarball/v1"
        );

        // Empty-asset releases are the norm this path serves, not an error.
        let body: serde_json::Value = serde_json::from_str(r#"{"assets": []}"#).unwrap();
        let err = release_tarball_url(&body, "org/repo", "v1").unwrap_err();
        assert!(err.to_string().contains("missing `tarball_url`"), "{err:?}");
    }

    #[test]
    fn single_top_level_directory_lifts() {
        // GitHub source archives wrap the tree once: unwrap it.
        let dir = tempfile::tempdir().expect("tempdir");
        let top = dir.path().join("repo-v1");
        std::fs::create_dir_all(top.join("nested")).expect("mkdirs");
        std::fs::write(top.join("templatry.source.toml"), "[x]\n").expect("write");
        std::fs::write(top.join("nested").join("f.txt"), "f").expect("write");
        lift_single_top_level(dir.path()).expect("lift");
        assert!(dir.path().join("templatry.source.toml").is_file());
        assert!(dir.path().join("nested").join("f.txt").is_file());
        assert!(!top.exists(), "wrapper removed");

        // Anything else stays put for downstream checks.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "a").expect("write");
        std::fs::write(dir.path().join("b.txt"), "b").expect("write");
        lift_single_top_level(dir.path()).expect("no lift");
        assert!(dir.path().join("a.txt").is_file());

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lone.txt"), "l").expect("write");
        lift_single_top_level(dir.path()).expect("no lift");
        assert!(dir.path().join("lone.txt").is_file());
    }

    #[test]
    fn entry_provenance_describes_sources() {
        let github = parse_project_ref("github = \"o/r\"\nref = \"v1\"\nasset = \"c_*.zip\"\n");
        let described = EntrySource::describe(&github, github.kind().unwrap());
        assert_eq!(described.kind, "github");
        assert_eq!(described.location, "o/r");
        assert_eq!(described.r#ref, "v1");
        assert_eq!(described.asset, "c_*.zip");
        assert_eq!(described.root, "");

        // Omitted asset normalizes to empty, like the cache key.
        let tarball = parse_project_ref("github = \"o/r/\"\nref = \"v1\"\n");
        let described = EntrySource::describe(&tarball, tarball.kind().unwrap());
        assert_eq!(described.location, "o/r");
        assert_eq!(described.asset, "");

        let url = parse_project_ref("url = \"https://example.com/t.tar.gz\"\n");
        let described = EntrySource::describe(&url, url.kind().unwrap());
        assert_eq!(described.kind, "url");
        assert_eq!(described.location, "https://example.com/t.tar.gz");
        assert_eq!(described.r#ref, "");

        let git = parse_project_ref(
            "git = \"git@example.com:o/r.git\"\nref = \"0123456789abcdef0123456789abcdef01234567\"\nroot = \".\"\n",
        );
        let described = EntrySource::describe(&git, git.kind().unwrap());
        assert_eq!(described.kind, "git");
        assert_eq!(described.r#ref, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(described.root, "", "`.` root normalizes away");
    }

    #[test]
    fn sidecar_roundtrips_provenance() {
        let dir = temp_cache();
        let github = parse_project_ref("github = \"o/r\"\nref = \"v1\"\n");
        write_sidecar(
            dir.path(),
            &github,
            github.kind().unwrap(),
            Some("abc".to_string()),
        )
        .expect("write sidecar");
        let meta = read_sidecar(dir.path()).expect("sidecar round-trips");
        assert_eq!(meta.resolved_sha.as_deref(), Some("abc"));
        let source = meta.source.expect("provenance recorded");
        assert_eq!(source.kind, "github");
        assert_eq!(source.location, "o/r");
    }

    #[test]
    fn cache_list_reports_entries_newest_first() {
        let root = temp_cache();
        let github = parse_project_ref("github = \"o/r\"\nref = \"v1\"\n");
        let provenance = Some(EntrySource::describe(&github, github.kind().unwrap()));

        // Fresh entry with provenance plus sized content.
        let fresh = root.path().join("ff");
        std::fs::create_dir_all(&fresh).expect("mkdirs");
        std::fs::write(fresh.join("a.bin"), vec![0u8; 100]).expect("write");
        std::fs::write(fresh.join("b.bin"), vec![0u8; 24]).expect("write");
        write_sidecar_inner(
            &fresh,
            &EntryMeta {
                last_used_unix: 200,
                resolved_sha: None,
                source: provenance,
            },
        )
        .expect("sidecar");

        // Legacy entry: sidecar without provenance sorts by its timestamp.
        let legacy = root.path().join("aa");
        std::fs::create_dir_all(&legacy).expect("mkdirs");
        write_sidecar_inner(
            &legacy,
            &EntryMeta {
                last_used_unix: 300,
                resolved_sha: None,
                source: None,
            },
        )
        .expect("legacy sidecar");

        // Entry without any sidecar still lists, provenance unknown, last.
        let bare = root.path().join("mm");
        std::fs::create_dir_all(&bare).expect("mkdirs");
        std::fs::write(bare.join("x.txt"), "hi").expect("write");

        // Stray files at the root are not entries.
        std::fs::write(root.path().join("stray.txt"), "x").expect("write");

        let listed = cache_list_in(root.path());
        let [legacy_entry, fresh_entry, bare_entry] = listed.try_into().expect("exactly 3 entries");
        assert_eq!(legacy_entry.source_id, "aa");
        assert_eq!(fresh_entry.source_id, "ff");
        assert_eq!(bare_entry.source_id, "mm");

        assert_eq!(legacy_entry.kind, None);
        assert_eq!(legacy_entry.last_used_unix, Some(300));

        assert_eq!(fresh_entry.kind.as_deref(), Some("github"));
        assert_eq!(fresh_entry.location.as_deref(), Some("o/r"));
        assert_eq!(fresh_entry.r#ref.as_deref(), Some("v1"));
        let sidecar_len = std::fs::metadata(fresh.join(ENTRY_SIDECAR_FILENAME))
            .expect("stat")
            .len();
        assert_eq!(fresh_entry.size_bytes, 100 + 24 + sidecar_len);

        assert_eq!(bare_entry.kind, None);
        assert_eq!(bare_entry.last_used_unix, None);
        assert_eq!(bare_entry.size_bytes, 2);

        assert_eq!(legacy_entry.path, legacy);
        assert_eq!(fresh_entry.path, fresh);
        assert_eq!(bare_entry.path, bare);
    }

    #[test]
    fn archive_format_detection() {
        assert_eq!(ArchiveKind::detect("a.tar.gz").unwrap(), ArchiveKind::TarGz);
        assert_eq!(ArchiveKind::detect("a.tgz").unwrap(), ArchiveKind::TarGz);
        assert_eq!(ArchiveKind::detect("a.TAR.GZ").unwrap(), ArchiveKind::TarGz);
        assert_eq!(ArchiveKind::detect("a.tar").unwrap(), ArchiveKind::Tar);
        assert_eq!(ArchiveKind::detect("a.ZIP").unwrap(), ArchiveKind::Zip);
        let err = ArchiveKind::detect("a.rar").unwrap_err();
        assert!(
            err.to_string().contains("cannot determine archive format"),
            "{err:?}"
        );
    }

    #[test]
    fn path_segment_encoding() {
        assert_eq!(encode_path_segment("v1.2.3"), "v1.2.3");
        assert_eq!(encode_path_segment("release/v1"), "release%2Fv1");
        assert_eq!(encode_path_segment("a b"), "a%20b");
    }

    /// Build a tar.gz archive in memory: `{name: content}` entries.
    fn tar_gz_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            for (name, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, content.as_bytes())
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        encoder.finish().unwrap()
    }

    /// Build a zip archive in memory: `{name: content}` entries.
    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, content) in entries {
                writer.start_file(*name, options).unwrap();
                std::io::Write::write_all(&mut writer, content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[tokio::test]
    async fn extraction_roundtrips_archives() {
        let dir = temp_cache();
        let dest = dir.path().join("tar-entry");
        extract_archive(
            ArchiveKind::TarGz,
            tar_gz_bytes(&[("app.json", "{}"), ("nested/deep.json", "[]")]),
            &dest,
        )
        .await
        .expect("tar.gz extracts");
        assert_eq!(
            std::fs::read_to_string(dest.join("app.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("nested/deep.json")).unwrap(),
            "[]"
        );

        let dest = dir.path().join("zip-entry");
        extract_archive(ArchiveKind::Zip, zip_bytes(&[("app.json", "{}")]), &dest)
            .await
            .expect("zip extracts");
        assert_eq!(
            std::fs::read_to_string(dest.join("app.json")).unwrap(),
            "{}"
        );
    }

    fn write_meta(entry: &Path, last_used_unix: u64) {
        let meta = EntryMeta {
            last_used_unix,
            resolved_sha: None,
            source: None,
        };
        std::fs::create_dir_all(entry).unwrap();
        std::fs::write(
            entry.join(ENTRY_SIDECAR_FILENAME),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sidecar_freshness_and_prune() {
        let cache = temp_cache();
        let fresh = cache.path().join("fresh");
        let stale = cache.path().join("stale");
        let orphan = cache.path().join("orphan");
        write_meta(&fresh, now_unix());
        write_meta(&stale, now_unix().saturating_sub(CACHE_TTL_SECS + 1));
        std::fs::create_dir_all(&orphan).unwrap();

        assert!(entry_is_fresh(&fresh));
        assert!(!entry_is_fresh(&stale));
        assert!(!entry_is_fresh(&cache.path().join("missing")));

        prune_stale_entries(cache.path());
        assert!(fresh.is_dir());
        assert!(!stale.exists());
        assert!(!orphan.exists());
    }

    #[test]
    fn cache_clear_handles_present_and_missing() {
        let cache = temp_cache();
        let nested = cache.path().join("some").join("entry");
        std::fs::create_dir_all(&nested).unwrap();
        cache_clear_in(&cache.path().join("some")).expect("clear present");
        assert!(!cache.path().join("some").exists());
        cache_clear_in(&cache.path().join("absent")).expect("clear missing is ok");
    }

    #[tokio::test]
    async fn local_source_resolves_without_cache() {
        let project = temp_cache();
        let templates = project.path().join("templates");
        std::fs::create_dir_all(&templates).unwrap();
        std::fs::write(
            templates.join(SOURCE_CONFIG_FILENAME),
            "[templates.a]\ntemplate = \"a.json\"\n",
        )
        .unwrap();
        std::fs::write(templates.join("a.json"), "{}\n").unwrap();

        let source = parse_project_ref("path = \"../templates\"\n");
        let project_dir = project.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let cache = temp_cache();
        let resolved = resolve_in(&source, &project_dir, true, cache.path())
            .await
            .expect("local resolves offline");
        assert!(!resolved.from_cache);
        assert_eq!(resolved.kind, SourceKind::LocalDir);
        assert_eq!(resolved.root_dir, templates.canonicalize().unwrap());

        let missing = parse_project_ref("path = \"../nope\"\n");
        let err = resolve_in(&missing, &project_dir, true, cache.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be resolved"), "{err:?}");
    }

    #[tokio::test]
    async fn offline_miss_names_the_source() {
        let source =
            parse_project_ref("github = \"org/repo\"\nref = \"v1\"\nasset = \"c_*.zip\"\n");
        let cache = temp_cache();
        let err = resolve_in(&source, Path::new("."), true, cache.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`--offline`"), "{err:?}");
    }

    #[tokio::test]
    async fn cache_hit_serves_without_fetch() {
        let source =
            parse_project_ref("github = \"org/repo\"\nref = \"v1\"\nasset = \"c_*.zip\"\n");
        let kind = source.kind().unwrap();
        let cache = temp_cache();
        let entry = cache.path().join(source_id(&source, kind).unwrap());
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(
            entry.join(SOURCE_CONFIG_FILENAME),
            "[templates.a]\ntemplate = \"a.json\"\n",
        )
        .unwrap();
        std::fs::write(entry.join("a.json"), "{}\n").unwrap();
        write_meta(&entry, now_unix());

        // Offline: a hit must serve; anything else would fail.
        let resolved = resolve_in(&source, Path::new("."), true, cache.path())
            .await
            .expect("cache hit");
        assert!(resolved.from_cache);
        assert_eq!(resolved.root_dir, entry);

        // A stale entry prunes, then the offline miss fires.
        write_meta(&entry, now_unix().saturating_sub(CACHE_TTL_SECS + 1));
        let err = resolve_in(&source, Path::new("."), true, cache.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`--offline`"), "{err:?}");
        assert!(!entry.exists());
    }
}
