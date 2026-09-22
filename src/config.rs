//! Project (`templatry.toml`) and source (`templatry.source.toml`) configuration schemas.
//!
//! Serde schemas, defaults, per-kind `ref` rules, and label resolution.
//! File loading and source/project context detection live in [`crate::validate`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Project configuration path, relative to the project repository root.
pub const PROJECT_CONFIG_PATH: &str = ".config/templatry.toml";

/// Source configuration filename, relative to the source root (or `root` subtree).
pub const SOURCE_CONFIG_FILENAME: &str = "templatry.source.toml";

/// Repository root for a project config path.
///
/// All project-relative paths (`path`, override dirs, generated dirs) resolve
/// against this: the grandparent when the file lives in a `.config/`
/// directory (the canonical `.config/templatry.toml` layout), else the
/// config file's own parent directory.
pub fn project_root(config_path: &Path) -> PathBuf {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    if parent.file_name().is_some_and(|name| name == ".config") {
        parent
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.to_path_buf())
    } else {
        parent.to_path_buf()
    }
}

// ---- Project config (`templatry.toml`) -------------------------------------

/// Project configuration: one `[source]` table or one `[source.<name>]` table
/// per template source (never both).
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Singular source reference (`[source]`), if the project uses one source.
    pub source: Option<SourceRef>,
    /// Named source references (`[source.<name>]`), empty for singular projects.
    pub sources: BTreeMap<String, SourceRef>,
}

impl<'de> serde::Deserialize<'de> for ProjectConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawProject {
            #[serde(default)]
            source: Option<toml::Value>,
        }

        let raw = RawProject::deserialize(deserializer)?;
        let Some(table) = raw.source else {
            return Err(serde::de::Error::missing_field(
                "project defines no `[source]`: add `[source]` (one source) or `[source.<name>]` (one table per source)",
            ));
        };
        let toml::Value::Table(map) = table else {
            return Err(serde::de::Error::custom(
                "`[source]` must be a table: set `[source]` fields or `[source.<name>]` tables",
            ));
        };
        split_project_sources(map).map_err(serde::de::Error::custom)
    }
}

/// Split a raw `[source]` table into singular vs plural form.
///
/// A `path`/`github`/`url`/`git` key holding a string means the singular
/// `[source]` table (a source literally named e.g. `path` would hold a table
/// there instead). Anything else is one `[source.<name>]` table per entry.
fn split_project_sources(
    map: toml::map::Map<String, toml::Value>,
) -> Result<ProjectConfig, String> {
    const KIND_KEYS: [&str; 4] = ["path", "github", "url", "git"];
    const FIELD_NAMES: [&str; 12] = [
        "path",
        "github",
        "url",
        "git",
        "ref",
        "root",
        "asset",
        "use_https",
        "enable_labels",
        "disable_labels",
        "include_templates",
        "exclude_templates",
    ];

    let singular = KIND_KEYS.iter().any(|key| {
        map.get(*key)
            .is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty()))
    });
    if singular {
        for (key, value) in &map {
            if !FIELD_NAMES.contains(&key.as_str()) {
                if value.is_table() {
                    return Err(format!(
                        "project mixes `[source]` fields with `[source.{key}]` tables: use either the singular `[source]` table or one table per source (`[source.<name>]`), not both"
                    ));
                }
                return Err(format!(
                    "unknown field `source.{key}`: expected one of {}",
                    FIELD_NAMES.join(", ")
                ));
            }
        }
        let source: SourceRef =
            toml::Value::Table(map)
                .try_into()
                .map_err(|err: toml::de::Error| {
                    format!("invalid `[source]` table: {}", err.message())
                })?;
        return Ok(ProjectConfig {
            source: Some(source),
            sources: BTreeMap::new(),
        });
    }

    if map.is_empty() {
        return Err(
            "project defines an empty `[source]` table: add `[source]` fields or `[source.<name>]` tables"
                .to_string(),
        );
    }
    let mut sources = BTreeMap::new();
    for (name, value) in map {
        if name.trim().is_empty() {
            return Err("source name must not be empty: rename `[source.<name>]`".to_string());
        }
        if !value.is_table() {
            return Err(format!(
                "`[source]` entry `{name}` must be a table (`[source.{name}]` with `path`, `github`, `url`, or `git`): fix the typo or move singular fields under `[source]`"
            ));
        }
        let source: SourceRef = value.try_into().map_err(|err: toml::de::Error| {
            format!("invalid `[source.{name}]` table: {}", err.message())
        })?;
        sources.insert(name, source);
    }
    Ok(ProjectConfig {
        source: None,
        sources,
    })
}

impl ProjectConfig {
    /// Configured sources in resolution order: the singular `[source]` first
    /// (with an empty name), else every `[source.<name>]` alphabetically.
    ///
    /// The empty singular name keeps single-source diagnostics and template
    /// names unqualified; every plural name qualifies its templates as
    /// `source:template`.
    pub fn project_sources(&self) -> Vec<(String, &SourceRef)> {
        if let Some(source) = self.source.as_ref() {
            return vec![(String::new(), source)];
        }
        self.sources
            .iter()
            .map(|(name, source)| (name.clone(), source))
            .collect()
    }

    /// True when the project uses the plural `[source.<name>]` form.
    pub fn is_multi_source(&self) -> bool {
        self.source.is_none()
    }
}

/// Reference to a single template source (v1: exactly one kind key is set).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// Local directory source, relative or absolute (uncached, used in place).
    #[serde(default)]
    pub path: Option<String>,
    /// GitHub release source as `org/repo` shorthand (not a URL).
    #[serde(default)]
    pub github: Option<String>,
    /// Generic URL archive source.
    #[serde(default)]
    pub url: Option<String>,
    /// Git checkout source (SSH URL by default, HTTPS with `use_https`).
    #[serde(default)]
    pub git: Option<String>,
    /// Pinned ref: release tag (GitHub, git) or full commit SHA (git only).
    #[serde(default)]
    pub r#ref: Option<String>,
    /// Subdirectory of the fetched source holding `templatry.source.toml`.
    #[serde(default)]
    pub root: Option<String>,
    /// GitHub release asset glob (e.g. `configs_*.zip`); omit to download
    /// the release's default source archive (tarball) instead.
    #[serde(default)]
    pub asset: Option<String>,
    /// Use HTTPS instead of SSH for git checkout sources.
    #[serde(default)]
    pub use_https: Option<bool>,
    /// Labels to enable on top of the source `[default]` rules.
    #[serde(default)]
    pub enable_labels: Vec<String>,
    /// Labels to disable on top of the source `[default]` rules (wins ties).
    #[serde(default)]
    pub disable_labels: Vec<String>,
    /// Templates to enable on top of label selection (absent means no restriction).
    #[serde(default)]
    pub include_templates: Option<Vec<String>>,
    /// Templates to disable after everything else (wins all ties).
    #[serde(default)]
    pub exclude_templates: Vec<String>,
}

/// The four v1 template source kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Local directory, used in place with no caching.
    LocalDir,
    /// Asset downloaded from a GitHub release.
    GitHubRelease,
    /// Archive downloaded from a plain HTTPS URL.
    UrlArchive,
    /// Git repository checked out at a tag or commit SHA.
    GitCheckout,
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalDir => write!(f, "local directory"),
            Self::GitHubRelease => write!(f, "GitHub release"),
            Self::UrlArchive => write!(f, "URL archive"),
            Self::GitCheckout => write!(f, "git checkout"),
        }
    }
}

impl SourceRef {
    /// Determine the source kind, enforcing per-kind required/forbidden fields.
    ///
    /// Structural checks only: tag-vs-branch existence is verified at fetch
    /// time (Milestone 2) via `ls-remote`.
    pub fn kind(&self) -> crate::Result<SourceKind> {
        let specified: Vec<(&str, &str)> = [
            ("path", self.path.as_deref()),
            ("github", self.github.as_deref()),
            ("url", self.url.as_deref()),
            ("git", self.git.as_deref()),
        ]
        .into_iter()
        .filter(|(_, value)| value.is_some_and(|value| !value.is_empty()))
        .map(|(key, value)| (key, value.unwrap_or_default()))
        .collect();

        if specified.is_empty() {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                "source table specifies no source kind: set exactly one of `path`, `github`, `url`, or `git`",
            ));
        }
        if specified.len() > 1 {
            let keys: Vec<&str> = specified.iter().map(|(key, _)| *key).collect();
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!(
                    "source table specifies {} source kinds ({}): set exactly one of `path`, `github`, `url`, or `git`",
                    specified.len(),
                    keys.join("`, `")
                ),
            ));
        }
        let (key, value) = specified[0];
        if value.trim().is_empty() {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!("source `{key}` must not be empty"),
            ));
        }

        match key {
            "path" => {
                self.forbid_ref("local directory sources")?;
                self.forbid_asset()?;
                self.forbid_use_https("local directory sources")?;
                Ok(SourceKind::LocalDir)
            }
            "github" => {
                self.require_ref("GitHub release sources", "release tag")?;
                self.forbid_use_https("GitHub release sources")?;
                if value.contains("://") {
                    return Err(crate::invalid(
                        Path::new("templatry.toml"),
                        format!(
                            "source `github` must use `org/repo` shorthand, not a URL (got `{value}`)"
                        ),
                    ));
                }
                if !value.contains('/') {
                    return Err(crate::invalid(
                        Path::new("templatry.toml"),
                        format!("source `github` must use `org/repo` shorthand (got `{value}`)"),
                    ));
                }
                Ok(SourceKind::GitHubRelease)
            }
            "url" => {
                self.forbid_ref("URL archive sources")?;
                self.forbid_asset()?;
                self.forbid_use_https("URL archive sources")?;
                if !value.starts_with("https://") && !value.starts_with("http://") {
                    return Err(crate::invalid(
                        Path::new("templatry.toml"),
                        format!("source `url` must be an http(s) URL (got `{value}`)"),
                    ));
                }
                Ok(SourceKind::UrlArchive)
            }
            "git" => {
                let r#ref = self.require_ref("git checkout sources", "tag or full commit SHA")?;
                self.forbid_asset()?;
                if is_full_sha(r#ref) {
                    Ok(SourceKind::GitCheckout)
                } else if is_hex(r#ref) && r#ref.len() >= 7 {
                    Err(crate::invalid(
                        Path::new("templatry.toml"),
                        format!(
                            "source `ref` `{ref}` looks like an abbreviated commit SHA: use the full 40-character SHA (branches are rejected; pin a tag or commit)"
                        ),
                    ))
                } else {
                    Ok(SourceKind::GitCheckout)
                }
            }
            _ => unreachable!("kind key comes from the fixed specified list"),
        }
    }

    /// Resolve the effective `use_https` flag (git kind only; default SSH).
    pub fn use_https_or_default(&self) -> bool {
        self.use_https.unwrap_or(false)
    }

    fn require_ref(&self, who: &str, what: &str) -> crate::Result<&str> {
        self.r#ref
            .as_deref()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| {
                crate::invalid(
                    Path::new("templatry.toml"),
                    format!("{who} require `ref` ({what})"),
                )
            })
    }

    fn forbid_ref(&self, who: &str) -> crate::Result<()> {
        if self.r#ref.as_deref().is_some_and(|r| !r.is_empty()) {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!("{who} do not use `ref`: remove it"),
            ));
        }
        Ok(())
    }

    fn forbid_asset(&self) -> crate::Result<()> {
        if self.asset.as_deref().is_some_and(|a| !a.is_empty()) {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                "only GitHub release sources use `asset`: remove it",
            ));
        }
        Ok(())
    }

    fn forbid_use_https(&self, who: &str) -> crate::Result<()> {
        if self.use_https.is_some() {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!("{who} do not use `use_https` (git checkouts only): remove it"),
            ));
        }
        Ok(())
    }
}

/// Validate `use_https`/URL consistency for git checkout sources.
///
/// Call after [`SourceRef::kind`]; errors when `use_https = true` is combined
/// with a non-HTTPS URL. SSH (or anything else) is always accepted when
/// `use_https` is unset or false.
pub fn validate_git_transport(source: &SourceRef) -> crate::Result<()> {
    if source.use_https_or_default()
        && let Some(git) = source.git.as_deref()
        && !git.starts_with("https://")
        && !git.starts_with("http://")
    {
        return Err(crate::invalid(
            Path::new("templatry.toml"),
            format!("`use_https = true` requires an http(s) git URL (got `{git}`)"),
        ));
    }
    Ok(())
}

/// True for a full 40-character commit SHA.
pub(crate) fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && is_hex(value)
}

/// True when every character is hexadecimal.
fn is_hex(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|char| char.is_ascii_hexdigit())
}

// ---- Source config (`templatry.source.toml`) -------------------------------

/// Template source definition: defaults, abstracts, templates, label rules.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    /// Default directories (all defaults apply when `[configs]` is omitted).
    #[serde(default)]
    pub configs: Configs,
    /// Inert partial templates merged into concrete templates via `extends`.
    #[serde(default, rename = "abstract")]
    pub abstracts: BTreeMap<String, AbstractTemplate>,
    /// One entry per template file, keyed by template name.
    #[serde(default)]
    pub templates: BTreeMap<String, Template>,
    /// Label selection rules (`[default]` omitted means everything enabled).
    #[serde(default)]
    pub default: Option<DefaultRules>,
}

/// Default directory conventions for templates.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configs {
    /// Where generated configs go unless a template overrides it.
    #[serde(default = "default_generated_dir")]
    pub default_generated_dir: String,
    /// Where project overrides live unless a template overrides it.
    #[serde(default = "default_override_dir")]
    pub default_override_dir: String,
}

impl Default for Configs {
    fn default() -> Self {
        Self {
            default_generated_dir: default_generated_dir(),
            default_override_dir: default_override_dir(),
        }
    }
}

fn default_generated_dir() -> String {
    ".config/generated".to_string()
}

fn default_override_dir() -> String {
    ".config/".to_string()
}

/// Label selection rules: mutually exclusive allow/deny lists.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultRules {
    /// Default-deny except these labels (empty list disables everything).
    #[serde(default)]
    pub include_labels: Option<Vec<String>>,
    /// Default-allow except these labels.
    #[serde(default)]
    pub exclude_labels: Option<Vec<String>>,
}

// jscpd:ignore-start
/// One template file: paths, merge strategy, labels.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// Template file path, relative to the source root (inheritable from an
    /// abstract; required after merging).
    #[serde(default)]
    pub template: Option<String>,
    /// Abstract template whose fields this entry inherits (`[abstract.<name>]`).
    #[serde(default)]
    pub extends: Option<String>,
    /// Override filename (defaults to the template basename).
    #[serde(default)]
    pub override_file: Option<String>,
    /// Override directory (defaults to `default_override_dir`).
    #[serde(default)]
    pub override_dir: Option<String>,
    /// Local override filename in `override_dir`, applied after the override
    /// (missing files are skipped; typically gitignored machine-local tweaks).
    #[serde(default)]
    pub local_override_file: Option<String>,
    /// Generated filename (defaults to the template basename).
    #[serde(default)]
    pub generated_file: Option<String>,
    /// Generated directory (defaults to `default_generated_dir`).
    #[serde(default)]
    pub generated_dir: Option<String>,
    /// Merge strategy (defaults to auto-detect on file extension).
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Array merge policy for structured merges (defaults to `union`).
    #[serde(default)]
    pub array_policy: Option<ArrayPolicy>,
    /// Watch the generated file and fold edits back into the override.
    #[serde(default)]
    pub back_propagate: Option<bool>,
    /// Back-propagation ignore paths: hand-edits here stay maintained
    /// (never folded, never overwritten). Requires `back_propagate`.
    #[serde(default)]
    pub backprop_ignore: Vec<String>,
    /// Back-propagation value-ignore paths: adds/deletes sync, value changes
    /// are left alone. Requires `back_propagate`.
    #[serde(default)]
    pub backprop_ignore_values: Vec<String>,
    /// Arbitrary labels for selection (e.g. `rust`, `dart`).
    #[serde(default)]
    pub labels: BTreeSet<String>,
}

/// Inert partial template: every field optional, merged into concrete
/// templates naming it via `extends`. Never generates on its own, and cannot
/// itself extend (single-level inheritance only).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbstractTemplate {
    /// Template file path default, relative to the source root.
    #[serde(default)]
    pub template: Option<String>,
    /// Override filename default.
    #[serde(default)]
    pub override_file: Option<String>,
    /// Override directory default.
    #[serde(default)]
    pub override_dir: Option<String>,
    /// Local override filename default.
    #[serde(default)]
    pub local_override_file: Option<String>,
    /// Generated filename default.
    #[serde(default)]
    pub generated_file: Option<String>,
    /// Generated directory default.
    #[serde(default)]
    pub generated_dir: Option<String>,
    /// Merge strategy default.
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Array merge policy default.
    #[serde(default)]
    pub array_policy: Option<ArrayPolicy>,
    /// Back-propagation default (`None` inherits as disabled).
    #[serde(default)]
    pub back_propagate: Option<bool>,
    /// Labels contributed to every extending template (unioned).
    #[serde(default)]
    pub labels: BTreeSet<String>,
    /// Ignore paths contributed to every extending template (unioned).
    #[serde(default)]
    pub backprop_ignore: Vec<String>,
    /// Value-ignore paths contributed to every extending template (unioned).
    #[serde(default)]
    pub backprop_ignore_values: Vec<String>,
}
// jscpd:ignore-end

impl AbstractTemplate {
    /// Merge into a concrete template: concrete scalar fields win when set,
    /// set-valued fields union, `back_propagate` takes the first set value.
    fn apply_to(&self, concrete: &Template) -> Template {
        let union_labels: BTreeSet<String> = self
            .labels
            .iter()
            .chain(concrete.labels.iter())
            .cloned()
            .collect();
        let mut union_ignore = self.backprop_ignore.clone();
        for pattern in &concrete.backprop_ignore {
            if !union_ignore.contains(pattern) {
                union_ignore.push(pattern.clone());
            }
        }
        let mut union_ignore_values = self.backprop_ignore_values.clone();
        for pattern in &concrete.backprop_ignore_values {
            if !union_ignore_values.contains(pattern) {
                union_ignore_values.push(pattern.clone());
            }
        }
        Template {
            template: concrete.template.clone().or_else(|| self.template.clone()),
            extends: None,
            override_file: concrete
                .override_file
                .clone()
                .or_else(|| self.override_file.clone()),
            override_dir: concrete
                .override_dir
                .clone()
                .or_else(|| self.override_dir.clone()),
            local_override_file: concrete
                .local_override_file
                .clone()
                .or_else(|| self.local_override_file.clone()),
            generated_file: concrete
                .generated_file
                .clone()
                .or_else(|| self.generated_file.clone()),
            generated_dir: concrete
                .generated_dir
                .clone()
                .or_else(|| self.generated_dir.clone()),
            strategy: concrete.strategy.or(self.strategy),
            array_policy: concrete.array_policy.or(self.array_policy),
            back_propagate: concrete.back_propagate.or(self.back_propagate),
            backprop_ignore: union_ignore,
            backprop_ignore_values: union_ignore_values,
            labels: union_labels,
        }
    }
}

/// How a template merges with its project override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Structured deep merge forced as JSON (comments stripped).
    MergeJson,
    /// Structured deep merge forced as YAML.
    MergeYaml,
    /// Structured deep merge forced as TOML.
    MergeToml,
    /// Override bytes, newline, then template bytes.
    AppendTop,
    /// Template bytes, newline, then override bytes (also the fallback for
    /// unknown extensions).
    AppendBottom,
    /// Emit the override file verbatim, ignoring the template.
    Replace,
    /// Straight copy of the template file: raw bytes plus (on Unix) its
    /// permission bits, ignoring any override.
    None,
}

/// Array merge policy for structured merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrayPolicy {
    /// Append-with-dedup for scalars (the `smartworkspace` behavior).
    #[default]
    Union,
    /// The override array wins wholesale.
    Replace,
}

impl Template {
    /// Template path (required after `extends` resolution; validated).
    pub(crate) fn template_path(&self) -> crate::Result<&str> {
        self.template.as_deref().filter(|path| !path.trim().is_empty()).ok_or_else(|| {
            crate::invalid(
                Path::new("templatry.source.toml"),
                "template entry defines no `template` path (and no abstract supplies one): set `template` or `extends`",
            )
        })
    }

    /// Override filename: explicit value or the template basename.
    pub fn resolved_override_file(&self) -> crate::Result<String> {
        if let Some(file) = self.override_file.as_deref() {
            return Ok(file.to_string());
        }
        basename(self.template_path()?).ok_or_else(|| {
            crate::invalid(
                Path::new("templatry.source.toml"),
                "template path has no filename to default `override_file` from: set it explicitly",
            )
        })
    }

    /// Generated filename: explicit value or the template basename.
    pub fn resolved_generated_file(&self) -> crate::Result<String> {
        if let Some(file) = self.generated_file.as_deref() {
            return Ok(file.to_string());
        }
        basename(self.template_path()?).ok_or_else(|| {
            crate::invalid(
                Path::new("templatry.source.toml"),
                "template path has no filename to default `generated_file` from: set it explicitly",
            )
        })
    }

    /// Generated directory: explicit value or `default_generated_dir`.
    pub fn resolved_generated_dir(&self, configs: &Configs) -> String {
        self.generated_dir
            .clone()
            .unwrap_or_else(|| configs.default_generated_dir.clone())
    }

    /// Override directory: explicit value or `default_override_dir`.
    pub fn resolved_override_dir(&self, configs: &Configs) -> String {
        self.override_dir
            .clone()
            .unwrap_or_else(|| configs.default_override_dir.clone())
    }
}

/// Final segment of a template path, if it names a file.
fn basename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

impl SourceFile {
    /// Merge every concrete template with its abstract base, if any.
    ///
    /// Templates without `extends` pass through untouched. Unknown abstract
    /// names fail listing the available ones.
    pub fn resolved_templates(
        &self,
        display_path: &Path,
    ) -> crate::Result<BTreeMap<String, Template>> {
        let mut resolved = BTreeMap::new();
        for (name, template) in &self.templates {
            let merged = match template
                .extends
                .as_deref()
                .filter(|base| !base.trim().is_empty())
            {
                None => template.clone(),
                Some(base_name) => {
                    let base = self.abstracts.get(base_name).ok_or_else(|| {
                        let mut available: Vec<&str> = self
                            .abstracts
                            .keys()
                            .map(String::as_str)
                            .collect();
                        available.sort();
                        let hint = if available.is_empty() {
                            "no `[abstract.*]` tables are defined".to_string()
                        } else {
                            format!("available: {}", available.join(", "))
                        };
                        crate::invalid(
                            display_path,
                            format!(
                                "[templates.{name}] extends unknown abstract `{base_name}` ({hint}): fix the typo, add `[abstract.{base_name}]`, or drop `extends`"
                            ),
                        )
                    })?;
                    base.apply_to(template)
                }
            };
            resolved.insert(name.clone(), merged);
        }
        Ok(resolved)
    }

    /// Validate schema consistency plus on-disk template references.
    ///
    /// `source_dir` is the directory the `template` paths resolve against;
    /// `display_path` names the source file in diagnostics. All checks run on
    /// `extends`-resolved templates.
    pub fn validate(&self, source_dir: &Path, display_path: &Path) -> crate::Result<()> {
        if self.templates.is_empty() {
            return Err(crate::invalid(
                display_path,
                "defines no templates: add at least one `[templates.<name>]` entry",
            ));
        }
        let resolved = self.resolved_templates(display_path)?;
        if let Some(default) = self.default.as_ref()
            && default.include_labels.is_some()
            && default.exclude_labels.is_some()
        {
            return Err(crate::invalid(
                display_path,
                "`[default]` sets both `include_labels` and `exclude_labels`: keep exactly one (or neither for everything-enabled)",
            ));
        }
        for (name, template) in &resolved {
            template.validate(name, source_dir, display_path)?;
        }
        Self::validate_shared_destinations(&resolved, &self.configs, display_path)?;
        Self::validate_label_refs(&resolved, self.default.as_ref(), display_path)?;
        Ok(())
    }

    /// `replace` cannot combine: flag shared destinations involving it.
    /// Structured and text strategies cannot combine either.
    fn validate_shared_destinations(
        templates: &BTreeMap<String, Template>,
        configs: &Configs,
        display_path: &Path,
    ) -> crate::Result<()> {
        let members: Vec<DestMember<'_>> = templates
            .iter()
            .map(|(name, template)| DestMember {
                display: name.clone(),
                template,
                configs,
                source: "",
            })
            .collect();
        check_shared_groups(&members, display_path, false)
    }

    /// `[default]` label lists must reference labels some template defines.
    fn validate_label_refs(
        templates: &BTreeMap<String, Template>,
        default: Option<&DefaultRules>,
        display_path: &Path,
    ) -> crate::Result<()> {
        let Some(default) = default else {
            return Ok(());
        };
        let known = Self::all_labels(templates);
        for labels in [
            default.include_labels.as_ref(),
            default.exclude_labels.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            for label in labels {
                if !known.contains(label.as_str()) {
                    return Err(crate::invalid(
                        display_path,
                        format!(
                            "`[default]` references unknown label `{label}` (no template defines it): fix the typo or add the label to a template"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Union of every label defined by the given templates.
    fn all_labels(templates: &BTreeMap<String, Template>) -> BTreeSet<&str> {
        templates
            .values()
            .flat_map(|template| template.labels.iter().map(String::as_str))
            .collect()
    }
}

impl Template {
    /// Validate one template entry: path safety, on-disk existence, and
    /// back-propagation compatibility.
    fn validate(&self, name: &str, source_dir: &Path, display_path: &Path) -> crate::Result<()> {
        let Some(template) = self
            .template
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        else {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] defines no `template` path (and no abstract supplies one): set `template` or `extends`"
                ),
            ));
        };
        let relative = Path::new(template);
        if relative.is_absolute() {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `template` must be relative to the source root (got `{template}`)"
                ),
            ));
        }
        if relative
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `template` must not escape the source root with `..` (got `{template}`)"
                ),
            ));
        }
        let on_disk = source_dir.join(relative);
        if !on_disk.is_file() {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `template` `{template}` does not exist under the source root"
                ),
            ));
        }
        if self.back_propagate == Some(true) && self.strategy == Some(Strategy::None) {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `back_propagate` needs an override to fold into, but strategy `none` copies the template verbatim: disable `back_propagate` or use another strategy"
                ),
            ));
        }
        if self.back_propagate != Some(true)
            && (!self.backprop_ignore.is_empty() || !self.backprop_ignore_values.is_empty())
        {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `backprop_ignore`/`backprop_ignore_values` need `back_propagate = true`: enable it or remove the lists"
                ),
            ));
        }
        for pattern in self
            .backprop_ignore
            .iter()
            .chain(self.backprop_ignore_values.iter())
        {
            if let Err(reason) = crate::backprop::IgnorePattern::parse(pattern) {
                return Err(crate::invalid(
                    display_path,
                    format!("[templates.{name}] invalid ignore pattern `{pattern}`: {reason}"),
                ));
            }
        }
        if (!self.backprop_ignore.is_empty() || !self.backprop_ignore_values.is_empty())
            && matches!(
                crate::merge::family_of(self, &self.resolved_generated_file()?)?,
                crate::merge::Family::Text
            )
        {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `backprop_ignore`/`backprop_ignore_values` need a structured merge strategy (key paths are meaningless for text)"
                ),
            ));
        }
        Ok(())
    }
}

// ---- Label and template resolution --------------------------------------------

/// Resolve the enabled template names for a project selection.
///
/// `templates` must already be `extends`-resolved. Base rules come from the
/// source `[default]` section (neither list means everything enabled); project
/// `enable_labels` force on, `disable_labels` force off. Project
/// `include_templates` then restricts to listed templates (absent means no
/// restriction, present-but-empty disables everything) and `exclude_templates`
/// removes listed ones, winning all ties. Unknown project labels and template
/// names are an error.
pub fn resolve_enabled_templates(
    templates: &BTreeMap<String, Template>,
    default: &Option<DefaultRules>,
    project: &SourceRef,
) -> crate::Result<BTreeSet<String>> {
    let known_labels: BTreeSet<&str> = SourceFile::all_labels(templates);
    for label in project
        .enable_labels
        .iter()
        .chain(project.disable_labels.iter())
    {
        if !known_labels.contains(label.as_str()) {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!(
                    "unknown label `{label}` (no template defines it): fix the typo or add the label to a template"
                ),
            ));
        }
    }
    if let Some(include) = project.include_templates.as_ref() {
        for name in include {
            if !templates.contains_key(name) {
                return Err(unknown_template(name, templates));
            }
        }
    }
    for name in &project.exclude_templates {
        if !templates.contains_key(name) {
            return Err(unknown_template(name, templates));
        }
    }

    let base_enabled = |template: &Template| match default.as_ref() {
        Some(default) if let Some(include) = default.include_labels.as_ref() => {
            template.labels.iter().any(|label| include.contains(label))
        }
        Some(default) if let Some(exclude) = default.exclude_labels.as_ref() => {
            !template.labels.iter().any(|label| exclude.contains(label))
        }
        _ => true,
    };

    let mut enabled = BTreeSet::new();
    for (name, template) in templates {
        let mut on = base_enabled(template);
        if template
            .labels
            .iter()
            .any(|label| project.enable_labels.contains(label))
        {
            on = true;
        }
        if template
            .labels
            .iter()
            .any(|label| project.disable_labels.contains(label))
        {
            on = false;
        }
        if let Some(include) = project.include_templates.as_ref()
            && !include.contains(name)
        {
            on = false;
        }
        if project.exclude_templates.contains(name) {
            on = false;
        }
        if on {
            enabled.insert(name.clone());
        }
    }
    Ok(enabled)
}

/// Unknown-template diagnostic listing what the source actually defines.
fn unknown_template(name: &str, templates: &BTreeMap<String, Template>) -> crate::Error {
    let mut available: Vec<&str> = templates.keys().map(String::as_str).collect();
    available.sort();
    crate::invalid(
        Path::new("templatry.toml"),
        format!(
            "unknown template `{name}` (source defines: {}): fix the typo or add the template to the source",
            available.join(", ")
        ),
    )
}

// ---- Multi-source merge -----------------------------------------------------

/// One source's contribution to the merged template set (borrowed view).
///
/// `templates` must already be `extends`-resolved within the source, so
/// abstract substitutions never bleed across sources. Views must arrive in
/// deterministic source-name order ([`ProjectConfig::project_sources`]).
pub struct SourceView<'a> {
    /// Source name (`""` for singular `[source]` projects).
    pub name: &'a str,
    /// `extends`-resolved templates (all, not just enabled).
    pub templates: &'a BTreeMap<String, Template>,
    /// Enabled template names after this source's own label filtering.
    pub enabled: &'a BTreeSet<String>,
    /// This source's `[configs]` defaults for destination resolution.
    pub configs: &'a Configs,
}

/// One active template with its owning source, in (source, template) order.
#[derive(Debug, Clone)]
pub struct ActiveTemplate {
    /// Owning source name (`""` for singular projects).
    pub source: String,
    /// Template name within its source.
    pub name: String,
    /// `extends`-resolved template definition.
    pub template: Template,
    /// Owning source's `[configs]` defaults.
    pub configs: Configs,
}

/// Merge per-source active templates into one ordered set.
///
/// A template name enabled in two sources is an error (the outputs would
/// collide with no owner). A name defined in several sources but enabled in
/// exactly one is returned as a warning: the other copies are ignored.
/// Names disabled everywhere stay silent.
pub fn merge_active_templates(
    views: &[SourceView<'_>],
) -> crate::Result<(Vec<ActiveTemplate>, Vec<String>)> {
    let mut defined: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut active: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, view) in views.iter().enumerate() {
        for name in view.templates.keys() {
            defined.entry(name.as_str()).or_default().push(view.name);
        }
        for name in view.enabled {
            active.entry(name.as_str()).or_default().push(index);
        }
    }
    for (name, holders) in &active {
        if holders.len() > 1 {
            let sources: Vec<String> = holders
                .iter()
                .map(|&index| format!("`{}`", views[index].name))
                .collect();
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!(
                    "template `{name}` is defined by sources {} and enabled in more than one: rename one template or disable one side (`disable_labels` or `exclude_templates`)",
                    sources.join(", ")
                ),
            ));
        }
    }
    let mut warnings = Vec::new();
    for (name, holders) in &defined {
        if holders.len() > 1 && active.get(*name).is_some_and(|live| live.len() == 1) {
            let live = views[active[*name][0]].name;
            let all: Vec<String> = holders.iter().map(|source| format!("`{source}`")).collect();
            warnings.push(format!(
                "template `{name}` is defined by sources {} but only enabled in `{live}`: the other copies are ignored (rename to silence this warning)",
                all.join(", ")
            ));
        }
    }
    let mut merged = Vec::new();
    for view in views {
        for name in view.enabled {
            merged.push(ActiveTemplate {
                source: view.name.to_string(),
                name: name.clone(),
                template: view.templates[name].clone(),
                configs: view.configs.clone(),
            });
        }
    }
    Ok((merged, warnings))
}

/// Validate merged active templates for destination conflicts.
///
/// Same rules as the single-source shared-destination check (no `replace` in
/// a group, one strategy family per destination), plus: a shared destination
/// spanning sources with `back_propagate` set is an error, since the fold
/// target would be ambiguous. Template names render `source:template`
/// whenever the owning source is named.
pub fn validate_merged_destinations(
    members: &[ActiveTemplate],
    display_path: &Path,
) -> crate::Result<()> {
    let borrowed: Vec<DestMember<'_>> = members
        .iter()
        .map(|member| DestMember {
            display: qualified_name(&member.source, &member.name),
            template: &member.template,
            configs: &member.configs,
            source: member.source.as_str(),
        })
        .collect();
    check_shared_groups(&borrowed, display_path, true)
}

/// Display name for a template: bare for singular projects, `source:template`
/// once sources are named.
pub fn qualified_name(source: &str, template: &str) -> String {
    if source.is_empty() {
        template.to_string()
    } else {
        format!("{source}:{template}")
    }
}

/// One destination-group candidate for the shared-destination check.
struct DestMember<'a> {
    /// Display name (bare or `source:template`).
    display: String,
    template: &'a Template,
    configs: &'a Configs,
    source: &'a str,
}

/// Shared-destination rules over pre-grouped candidates.
///
/// With `cross_source_backprop`, groups spanning sources with
/// `back_propagate` set fail even for structured strategies; otherwise only
/// the text-family attribution rule applies.
fn check_shared_groups(
    members: &[DestMember<'_>],
    display_path: &Path,
    cross_source_backprop: bool,
) -> crate::Result<()> {
    let mut groups: BTreeMap<(String, String), Vec<&DestMember<'_>>> = BTreeMap::new();
    for member in members {
        let key = (
            member.template.resolved_generated_dir(member.configs),
            member.template.resolved_generated_file()?,
        );
        groups.entry(key).or_default().push(member);
    }
    for ((dir, file), group) in &groups {
        if group.len() < 2 {
            continue;
        }
        let names: Vec<&str> = group.iter().map(|member| member.display.as_str()).collect();
        if group
            .iter()
            .any(|member| member.template.strategy == Some(Strategy::Replace))
        {
            return Err(crate::invalid(
                display_path,
                format!(
                    "templates {} all target `{dir}/{file}`, but `replace` emits one override verbatim and cannot combine: give them distinct destinations or drop `replace`",
                    names.join("`, `")
                ),
            ));
        }
        let mut families = BTreeSet::new();
        let mut back_propagated = Vec::new();
        let mut sources = BTreeSet::new();
        for member in group {
            families.insert(crate::merge::family_of(
                member.template,
                &member.template.resolved_generated_file()?,
            )?);
            if member.template.back_propagate == Some(true) {
                back_propagated.push(member.display.as_str());
            }
            sources.insert(member.source);
        }
        if families.len() > 1 {
            return Err(crate::invalid(
                display_path,
                format!(
                    "templates {} all target `{dir}/{file}`, but mix structured merges with text strategies: align them to one family",
                    names.join("`, `")
                ),
            ));
        }
        if cross_source_backprop && sources.len() > 1 && !back_propagated.is_empty() {
            let spanning: Vec<String> =
                sources.iter().map(|source| format!("`{source}`")).collect();
            return Err(crate::invalid(
                display_path,
                format!(
                    "templates {} all target `{dir}/{file}`, but back propagation into a shared destination spanning sources ({}) cannot attribute edits to one override ({}): keep back-propagated destinations within one source",
                    names.join("`, `"),
                    spanning.join(", "),
                    back_propagated.join("`, `")
                ),
            ));
        }
        if !back_propagated.is_empty() && families.contains(&crate::merge::Family::Text) {
            return Err(crate::invalid(
                display_path,
                format!(
                    "templates {} all target `{dir}/{file}`, but back propagation into a shared text destination cannot attribute edits to one override ({}): use structured merge strategies for label-split files",
                    names.join("`, `"),
                    back_propagated.join("`, `")
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn parse_source(toml: &str) -> SourceFile {
        toml::from_str(toml).expect("source fixture parses")
    }

    fn parse_project(toml: &str) -> ProjectConfig {
        toml::from_str(toml).expect("project fixture parses")
    }

    fn project_ref(body: &str) -> SourceRef {
        parse_project(&format!("[source]\n{body}"))
            .source
            .expect("singular test project")
    }

    fn empty_project() -> SourceRef {
        project_ref("path = \"x\"\n")
    }

    /// Temp source dir containing one `{}` file per listed relative path.
    fn source_dir_with(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
            std::fs::write(&path, "{}\n").expect("write fixture");
        }
        dir
    }

    fn display() -> PathBuf {
        PathBuf::from("templatry.source.toml")
    }

    #[test]
    fn minimal_source_applies_defaults() {
        let source = parse_source("[templates.app]\ntemplate = \"app.json\"\n");
        assert_eq!(source.configs.default_generated_dir, ".config/generated");
        assert_eq!(source.configs.default_override_dir, ".config/");
        assert!(source.default.is_none());
        let template = &source.templates["app"];
        assert_eq!(template.strategy, None);
        assert_eq!(template.array_policy, None);
        assert_eq!(template.back_propagate, None);
        assert!(template.labels.is_empty());
    }

    #[test]
    fn full_template_entry_parses() {
        let source = parse_source(
            r#"
[configs]
default_generated_dir = "gen"
default_override_dir = "over"

[templates.app]
template = "sub/app.yaml"
override_file = "custom.yaml"
override_dir = "odir"
generated_file = "out.yaml"
generated_dir = "gdir"
strategy = "append_bottom"
array_policy = "replace"
back_propagate = true
labels = ["rust", "dart"]

[default]
exclude_labels = ["dart"]
"#,
        );
        let template = &source.templates["app"];
        assert_eq!(template.resolved_override_file().unwrap(), "custom.yaml");
        assert_eq!(template.resolved_generated_file().unwrap(), "out.yaml");
        assert_eq!(template.resolved_generated_dir(&source.configs), "gdir");
        assert_eq!(template.strategy, Some(Strategy::AppendBottom));
        assert_eq!(template.array_policy, Some(ArrayPolicy::Replace));
        assert_eq!(template.back_propagate, Some(true));
        assert!(template.labels.contains("rust"));
    }

    #[test]
    fn all_strategy_values_parse() {
        for (value, expected) in [
            ("merge_json", Strategy::MergeJson),
            ("merge_yaml", Strategy::MergeYaml),
            ("merge_toml", Strategy::MergeToml),
            ("append_top", Strategy::AppendTop),
            ("append_bottom", Strategy::AppendBottom),
            ("replace", Strategy::Replace),
            ("none", Strategy::None),
        ] {
            let source = parse_source(&format!(
                "[templates.app]\ntemplate = \"a.json\"\nstrategy = \"{value}\"\n"
            ));
            assert_eq!(source.templates["app"].strategy, Some(expected));
        }
    }

    #[test]
    fn unknown_fields_and_strategies_rejected() {
        assert!(toml::from_str::<SourceFile>("bogus = 1\n").is_err());
        assert!(
            toml::from_str::<SourceFile>("[templates.app]\ntemplate = \"a\"\nbogus = 1\n").is_err()
        );
        assert!(
            toml::from_str::<SourceFile>(
                "[templates.app]\ntemplate = \"a\"\nstrategy = \"fancy\"\n"
            )
            .is_err()
        );
        assert!(toml::from_str::<ProjectConfig>("[source]\npath = \"x\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn empty_templates_rejected() {
        let source = parse_source("[configs]\n");
        let err = source.validate(Path::new("."), &display()).unwrap_err();
        assert!(err.to_string().contains("no templates"), "{err:?}");
    }

    #[test]
    fn include_exclude_mutex_rejected() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\n[default]\ninclude_labels = [\"x\"]\nexclude_labels = [\"y\"]\n",
        );
        let dir = source_dir_with(&[]);
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("sets both"), "{err:?}");
    }

    #[test]
    fn template_path_rules() {
        let dir = source_dir_with(&["ok.json", "sub/app.yaml"]);
        let valid = parse_source(
            "[templates.a]\ntemplate = \"ok.json\"\n[templates.b]\ntemplate = \"sub/app.yaml\"\n",
        );
        valid.validate(dir.path(), &display()).expect("valid paths");

        let missing = parse_source("[templates.a]\ntemplate = \"nope.json\"\n");
        let err = missing.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err:?}");

        let absolute = parse_source("[templates.a]\ntemplate = \"/abs.json\"\n");
        let err = absolute.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("must be relative"), "{err:?}");

        let escape = parse_source("[templates.a]\ntemplate = \"../x.json\"\n");
        let err = escape.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("must not escape"), "{err:?}");
    }

    #[test]
    fn filenames_default_to_template_basename() {
        let source = parse_source("[templates.a]\ntemplate = \"sub/app.yaml\"\n");
        let template = &source.templates["a"];
        assert_eq!(template.resolved_override_file().unwrap(), "app.yaml");
        assert_eq!(template.resolved_generated_file().unwrap(), "app.yaml");
        assert_eq!(
            template.resolved_generated_dir(&source.configs),
            ".config/generated"
        );
    }

    #[test]
    fn shared_replace_destination_rejected() {
        let dir = source_dir_with(&["a.json", "b.json"]);
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\ngenerated_file = \"settings.json\"\nstrategy = \"replace\"\n[templates.b]\ntemplate = \"b.json\"\ngenerated_file = \"settings.json\"\n",
        );
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("cannot combine"), "{err:?}");
    }

    #[test]
    fn shared_text_backprop_rejected() {
        let dir = source_dir_with(&["a.txt", "b.txt"]);
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.txt\"\ngenerated_file = \"out.txt\"\nback_propagate = true\n[templates.b]\ntemplate = \"b.txt\"\ngenerated_file = \"out.txt\"\n",
        );
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("cannot attribute"), "{err:?}");

        let single = parse_source("[templates.a]\ntemplate = \"a.txt\"\nback_propagate = true\n");
        single
            .validate(dir.path(), &display())
            .expect("single text backprop");
    }

    #[test]
    fn shared_merge_destination_allowed() {
        let dir = source_dir_with(&["settings.json"]);
        let source = parse_source(
            "[templates.rust]\ntemplate = \"settings.json\"\nlabels = [\"rust\"]\n[templates.dart]\ntemplate = \"settings.json\"\nlabels = [\"dart\"]\n",
        );
        source
            .validate(dir.path(), &display())
            .expect("shared merge");
    }

    #[test]
    fn backpropagate_none_rejected() {
        let dir = source_dir_with(&["license.txt"]);
        let source = parse_source(
            "[templates.license]\ntemplate = \"license.txt\"\nstrategy = \"none\"\nback_propagate = true\n",
        );
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(
            err.to_string().contains("needs an override to fold into"),
            "{err:?}"
        );
    }

    #[test]
    fn unknown_default_label_rejected() {
        let dir = source_dir_with(&["a.json"]);
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[default]\ninclude_labels = [\"nope\"]\n",
        );
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("unknown label `nope`"), "{err:?}");
    }

    #[test]
    fn no_rules_enables_everything() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\n",
        );
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &empty_project())
                .unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn include_selects_matching_templates() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\n[default]\ninclude_labels = [\"rust\"]\n",
        );
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &empty_project())
                .unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string()]));
    }

    #[test]
    fn empty_include_disables_everything() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[default]\ninclude_labels = []\n",
        );
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &empty_project())
                .unwrap();
        assert!(enabled.is_empty());
    }

    #[test]
    fn exclude_removes_matching_templates() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\n[default]\nexclude_labels = [\"rust\"]\n",
        );
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &empty_project())
                .unwrap();
        assert_eq!(enabled, BTreeSet::from(["b".to_string()]));
    }

    #[test]
    fn project_enable_adds_and_disable_wins() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\nlabels = [\"dart\"]\n[default]\ninclude_labels = [\"rust\"]\n",
        );
        let project = project_ref("path = \"x\"\nenable_labels = [\"dart\"]\n");
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string(), "b".to_string()]));

        let project = project_ref(
            "path = \"x\"\nenable_labels = [\"dart\"]\ndisable_labels = [\"rust\", \"dart\"]\n",
        );
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap();
        assert!(enabled.is_empty());
    }

    #[test]
    fn unknown_project_label_rejected() {
        let source = parse_source("[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n");
        let project = project_ref("path = \"x\"\nenable_labels = [\"nope\"]\n");
        let err =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap_err();
        assert!(err.to_string().contains("unknown label `nope`"), "{err:?}");
    }

    #[test]
    fn include_templates_restricts_to_listed() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\nlabels = [\"dart\"]\n",
        );
        let project = project_ref("path = \"x\"\ninclude_templates = [\"b\"]\n");
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap();
        assert_eq!(enabled, BTreeSet::from(["b".to_string()]));
    }

    #[test]
    fn empty_include_templates_disables_everything() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\n",
        );
        let project = project_ref("path = \"x\"\ninclude_templates = []\n");
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap();
        assert!(enabled.is_empty());
    }

    #[test]
    fn exclude_templates_wins_all_ties() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\nlabels = [\"dart\"]\n[default]\ninclude_labels = [\"rust\", \"dart\"]\n",
        );
        // Excludes a label-enabled template and an include-listed one alike.
        let project = project_ref(
            "path = \"x\"\ninclude_templates = [\"a\", \"b\"]\nexclude_templates = [\"b\"]\n",
        );
        let enabled =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string()]));
    }

    #[test]
    fn unknown_template_name_rejected() {
        let source = parse_source("[templates.a]\ntemplate = \"a.json\"\n");
        let project = project_ref("path = \"x\"\ninclude_templates = [\"nope\"]\n");
        let err =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown template `nope`"), "{message}");
        assert!(message.contains('a'), "{message}");

        let project = project_ref("path = \"x\"\nexclude_templates = [\"nope\"]\n");
        let err =
            resolve_enabled_templates(&source.templates, &source.default, &project).unwrap_err();
        assert!(
            err.to_string().contains("unknown template `nope`"),
            "{err:?}"
        );
    }

    fn resolved(source: &SourceFile) -> BTreeMap<String, Template> {
        source
            .resolved_templates(Path::new("templatry.source.toml"))
            .expect("extends resolve")
    }

    #[test]
    fn extends_merges_concrete_over_abstract() {
        let source = parse_source(
            "[abstract.base]\ngenerated_dir = \".\"\nstrategy = \"append_bottom\"\nlabels = [\"shared\"]\narray_policy = \"replace\"\n[templates.app]\ntemplate = \"a.json\"\nextends = \"base\"\nlabels = [\"apps\"]\n",
        );
        let merged = resolved(&source);
        let app = &merged["app"];
        assert_eq!(app.template.as_deref(), Some("a.json"));
        assert_eq!(app.generated_dir.as_deref(), Some("."));
        assert_eq!(app.strategy, Some(Strategy::AppendBottom));
        assert_eq!(app.array_policy, Some(ArrayPolicy::Replace));
        assert_eq!(
            app.labels,
            BTreeSet::from(["shared".to_string(), "apps".to_string()])
        );
        // No extends: passes through untouched, keeps its own values.
        let plain = parse_source("[templates.app]\ntemplate = \"a.json\"\n");
        let merged = resolved(&plain);
        assert_eq!(merged["app"].generated_dir, None);
        assert_eq!(merged["app"].extends, None);
    }

    #[test]
    fn extends_backpropagate_prefers_explicit() {
        for (concrete, abstracted, expected) in [
            (None, None, None),
            (None, Some(true), Some(true)),
            (None, Some(false), Some(false)),
            (Some(true), None, Some(true)),
            (Some(false), Some(true), Some(false)),
            (Some(true), Some(false), Some(true)),
        ] {
            let mut concrete_toml =
                "[templates.app]\ntemplate = \"a.json\"\nextends = \"base\"\n".to_string();
            if let Some(value) = concrete {
                concrete_toml.push_str(&format!("back_propagate = {value}\n"));
            }
            let mut abstract_toml = "[abstract.base]\n".to_string();
            if let Some(value) = abstracted {
                abstract_toml.push_str(&format!("back_propagate = {value}\n"));
            }
            let source = parse_source(&format!("{abstract_toml}{concrete_toml}"));
            let merged = resolved(&source);
            assert_eq!(
                merged["app"].back_propagate, expected,
                "concrete {concrete:?} over abstract {abstracted:?}"
            );
        }
    }

    #[test]
    fn extends_unknown_abstract_rejected() {
        let source = parse_source("[templates.app]\ntemplate = \"a.json\"\nextends = \"nope\"\n");
        let err = source
            .resolved_templates(Path::new("templatry.source.toml"))
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown abstract `nope`"), "{message}");
    }

    #[test]
    fn extends_unknown_abstract_lists_available() {
        let source = parse_source(
            "[abstract.base]\n[templates.app]\ntemplate = \"a.json\"\nextends = \"nope\"\n",
        );
        let err = source
            .resolved_templates(Path::new("templatry.source.toml"))
            .unwrap_err();
        assert!(err.to_string().contains("available: base"), "{err:?}");
    }

    #[test]
    fn extends_missing_template_rejected_at_validate() {
        let dir = source_dir_with(&[]);
        let source = parse_source("[templates.app]\nextends = \"base\"\n[abstract.base]\n");
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("defines no `template`"), "{err:?}");
    }

    #[test]
    fn extends_template_default_used() {
        let dir = source_dir_with(&["shared.json"]);
        let source = parse_source(
            "[abstract.base]\ntemplate = \"shared.json\"\n[templates.a]\nextends = \"base\"\n[templates.b]\ntemplate = \"other.json\"\n",
        );
        // Concrete `b` names a missing file: resolution succeeds, validation
        // fails on `b` only. Resolve first to prove the abstract default lands.
        let merged = resolved(&source);
        assert_eq!(merged["a"].template.as_deref(), Some("shared.json"));
        assert_eq!(merged["b"].template.as_deref(), Some("other.json"));
        let err = source.validate(dir.path(), &display()).unwrap_err();
        assert!(err.to_string().contains("[templates.b]"), "{err:?}");
    }

    #[test]
    fn extends_in_abstract_rejected_at_parse() {
        let err = toml::from_str::<SourceFile>(
            "[abstract.base]\nextends = \"other\"\n[templates.a]\ntemplate = \"a.json\"\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field `extends`"),
            "{err:?}"
        );
    }

    #[test]
    fn extends_empty_string_means_absent() {
        let source = parse_source("[templates.app]\ntemplate = \"a.json\"\nextends = \"\"\n");
        let merged = resolved(&source);
        assert_eq!(merged["app"].template.as_deref(), Some("a.json"));
    }

    #[test]
    fn extends_labels_union_resolves() {
        let source = parse_source(
            "[abstract.base]\nlabels = [\"shared\"]\n[templates.a]\ntemplate = \"a.json\"\nextends = \"base\"\nlabels = [\"apps\"]\n[templates.b]\ntemplate = \"b.json\"\n[default]\ninclude_labels = []\n",
        );
        let merged = resolved(&source);
        let project = project_ref("path = \"x\"\nenable_labels = [\"shared\"]\n");
        let enabled = resolve_enabled_templates(&merged, &source.default, &project).unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string()]));
    }

    #[test]
    fn local_kind_rules() {
        assert_eq!(
            project_ref("path = \"../templates\"\n").kind().unwrap(),
            SourceKind::LocalDir
        );
        let err = project_ref("path = \"x\"\nref = \"v1\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("do not use `ref`"), "{err:?}");
    }

    #[test]
    fn github_kind_rules() {
        let source = project_ref("github = \"org/repo\"\nref = \"v1\"\nasset = \"c_*.zip\"\n");
        assert_eq!(source.kind().unwrap(), SourceKind::GitHubRelease);

        // `asset` is optional: omitting it downloads the release's default
        // source archive (tarball) instead of matching an uploaded asset.
        let source = project_ref("github = \"org/repo\"\nref = \"v1\"\n");
        assert_eq!(source.kind().unwrap(), SourceKind::GitHubRelease);

        let err = project_ref("github = \"org/repo\"\nasset = \"c_*.zip\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("require `ref`"), "{err:?}");

        let err =
            project_ref("github = \"https://github.com/org/repo\"\nref = \"v1\"\nasset = \"c\"\n")
                .kind()
                .unwrap_err();
        assert!(err.to_string().contains("shorthand"), "{err:?}");

        let err = project_ref("github = \"just-a-name\"\nref = \"v1\"\nasset = \"c\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("shorthand"), "{err:?}");
    }

    #[test]
    fn url_kind_rules() {
        let source = project_ref("url = \"https://example.com/t.tar.gz\"\n");
        assert_eq!(source.kind().unwrap(), SourceKind::UrlArchive);

        let err = project_ref("url = \"ftp://example.com/t.tar.gz\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("http(s)"), "{err:?}");

        let err = project_ref("url = \"https://example.com/t.tar.gz\"\nref = \"v1\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("do not use `ref`"), "{err:?}");
    }

    #[test]
    fn git_kind_rules() {
        let tag = project_ref("git = \"git@github.com:org/repo.git\"\nref = \"v1.0.0\"\n");
        assert_eq!(tag.kind().unwrap(), SourceKind::GitCheckout);

        let sha = project_ref(
            "git = \"git@github.com:org/repo.git\"\nref = \"0123456789abcdef0123456789abcdef01234567\"\n",
        );
        assert_eq!(sha.kind().unwrap(), SourceKind::GitCheckout);

        let err = project_ref("git = \"git@github.com:org/repo.git\"\nref = \"abc1234\"\n")
            .kind()
            .unwrap_err();
        assert!(
            err.to_string().contains("abbreviated commit SHA"),
            "{err:?}"
        );

        let err = project_ref("git = \"git@github.com:org/repo.git\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("require `ref`"), "{err:?}");
    }

    #[test]
    fn kind_count_and_use_https_rules() {
        let err = ProjectConfig {
            source: Some(SourceRef {
                path: None,
                github: None,
                url: None,
                git: None,
                r#ref: None,
                root: None,
                asset: None,
                use_https: None,
                enable_labels: vec![],
                disable_labels: vec![],
                include_templates: None,
                exclude_templates: vec![],
            }),
            sources: BTreeMap::new(),
        }
        .source
        .expect("singular")
        .kind()
        .unwrap_err();
        assert!(err.to_string().contains("no source kind"), "{err:?}");

        let err = project_ref("path = \"a\"\nurl = \"https://x/y\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err:?}");

        let err = project_ref("github = \"o/r\"\nref = \"v\"\nasset = \"a\"\nuse_https = true\n")
            .kind()
            .unwrap_err();
        assert!(
            err.to_string().contains("do not use `use_https`"),
            "{err:?}"
        );

        let https =
            project_ref("git = \"git@github.com:o/r.git\"\nref = \"v1\"\nuse_https = true\n");
        validate_git_transport(&https).expect_err("https flag with ssh url");
        let ssh = project_ref("git = \"git@github.com:o/r.git\"\nref = \"v1\"\n");
        validate_git_transport(&ssh).expect("ssh default");
    }

    #[test]
    fn singular_source_stays_singular() {
        let project = parse_project("[source]\npath = \"templates\"\n");
        assert!(!project.is_multi_source());
        let sources = project.project_sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "");
        assert_eq!(sources[0].1.path.as_deref(), Some("templates"));
    }

    #[test]
    fn plural_sources_parse_named() {
        let project = parse_project(
            "[source.b]\npath = \"tb\"\n[source.a]\npath = \"ta\"\ninclude_templates = [\"x\"]\n",
        );
        assert!(project.is_multi_source());
        assert!(project.source.is_none());
        let sources = project.project_sources();
        let names: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(sources[0].1.path.as_deref(), Some("ta"));
        assert_eq!(sources[0].1.include_templates, Some(vec!["x".to_string()]));
    }

    #[test]
    fn mixed_source_forms_rejected() {
        let err = toml::from_str::<ProjectConfig>(
            "[source]\npath = \"templates\"\n[source.extra]\npath = \"templates\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("mixes"), "{err:?}");
    }

    #[test]
    fn empty_and_scalar_source_entries_rejected() {
        let err = toml::from_str::<ProjectConfig>("[source]\n").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err:?}");

        let err =
            toml::from_str::<ProjectConfig>("[source]\nenable_labels = [\"x\"]\n").unwrap_err();
        assert!(err.to_string().contains("must be a table"), "{err:?}");

        let err = toml::from_str::<ProjectConfig>("").unwrap_err();
        assert!(err.to_string().contains("[source]"), "{err:?}");
    }

    /// Minimal template targeting `generated` under `out/` (no disk access).
    fn merged_template(generated: &str, backprop: bool) -> Template {
        Template {
            template: Some("t.json".to_string()),
            extends: None,
            override_file: None,
            override_dir: None,
            local_override_file: None,
            generated_file: Some(generated.to_string()),
            generated_dir: Some("out".to_string()),
            strategy: None,
            array_policy: None,
            back_propagate: if backprop { Some(true) } else { None },
            backprop_ignore: Vec::new(),
            backprop_ignore_values: Vec::new(),
            labels: BTreeSet::new(),
        }
    }

    fn test_view(
        templates: &[&str],
        enabled: &[&str],
    ) -> (BTreeMap<String, Template>, BTreeSet<String>, Configs) {
        let map: BTreeMap<String, Template> = templates
            .iter()
            .map(|template| ((*template).to_string(), merged_template("out.json", false)))
            .collect();
        let set: BTreeSet<String> = enabled.iter().map(|name| (*name).to_string()).collect();
        (map, set, Configs::default())
    }

    #[test]
    fn merge_active_orders_by_source_then_template() {
        let (a_map, a_on, a_configs) = test_view(&["z", "m"], &["z", "m"]);
        let (b_map, b_on, b_configs) = test_view(&["a"], &["a"]);
        let views = [
            SourceView {
                name: "a",
                templates: &a_map,
                enabled: &a_on,
                configs: &a_configs,
            },
            SourceView {
                name: "b",
                templates: &b_map,
                enabled: &b_on,
                configs: &b_configs,
            },
        ];
        let (merged, warnings) = merge_active_templates(&views).expect("merge");
        let order: Vec<(&str, &str)> = merged
            .iter()
            .map(|member| (member.source.as_str(), member.name.as_str()))
            .collect();
        assert_eq!(order, [("a", "m"), ("a", "z"), ("b", "a")]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn merge_active_conflict_errors_when_both_enabled() {
        let (a_map, a_on, a_configs) = test_view(&["app"], &["app"]);
        let (b_map, b_on, b_configs) = test_view(&["app"], &["app"]);
        let views = [
            SourceView {
                name: "a",
                templates: &a_map,
                enabled: &a_on,
                configs: &a_configs,
            },
            SourceView {
                name: "b",
                templates: &b_map,
                enabled: &b_on,
                configs: &b_configs,
            },
        ];
        let err = merge_active_templates(&views).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("template `app`"), "{message}");
        assert!(message.contains("more than one"), "{message}");
    }

    #[test]
    fn merge_active_shadow_warns_when_one_enabled() {
        let (a_map, a_on, a_configs) = test_view(&["app"], &[]);
        let (b_map, b_on, b_configs) = test_view(&["app"], &["app"]);
        let views = [
            SourceView {
                name: "a",
                templates: &a_map,
                enabled: &a_on,
                configs: &a_configs,
            },
            SourceView {
                name: "b",
                templates: &b_map,
                enabled: &b_on,
                configs: &b_configs,
            },
        ];
        let (merged, warnings) = merge_active_templates(&views).expect("shadow passes");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, "b");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("template `app`"), "{}", warnings[0]);
        assert!(warnings[0].contains("`b`"), "{}", warnings[0]);
    }

    #[test]
    fn merge_active_silent_when_all_disabled() {
        let (a_map, a_on, a_configs) = test_view(&["app"], &[]);
        let (b_map, b_on, b_configs) = test_view(&["app"], &[]);
        let views = [
            SourceView {
                name: "a",
                templates: &a_map,
                enabled: &a_on,
                configs: &a_configs,
            },
            SourceView {
                name: "b",
                templates: &b_map,
                enabled: &b_on,
                configs: &b_configs,
            },
        ];
        let (merged, warnings) = merge_active_templates(&views).expect("all off passes");
        assert!(merged.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn merged_destinations_cross_backprop_rejected() {
        let members = [
            ActiveTemplate {
                source: "a".to_string(),
                name: "a".to_string(),
                template: merged_template("shared.json", true),
                configs: Configs::default(),
            },
            ActiveTemplate {
                source: "b".to_string(),
                name: "b".to_string(),
                template: merged_template("shared.json", false),
                configs: Configs::default(),
            },
        ];
        let err = validate_merged_destinations(&members, Path::new(".config/templatry.toml"))
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("spanning sources"), "{message}");
        assert!(message.contains("a:a"), "{message}");
        assert!(message.contains("b:b"), "{message}");
    }

    #[test]
    fn merged_destinations_compatible_union_passes() {
        let members = [
            ActiveTemplate {
                source: "a".to_string(),
                name: "a".to_string(),
                template: merged_template("shared.json", false),
                configs: Configs::default(),
            },
            ActiveTemplate {
                source: "b".to_string(),
                name: "b".to_string(),
                template: merged_template("shared.json", false),
                configs: Configs::default(),
            },
        ];
        validate_merged_destinations(&members, Path::new(".config/templatry.toml"))
            .expect("structured union across sources");
    }

    #[test]
    fn qualified_name_bare_for_singular() {
        assert_eq!(qualified_name("", "app"), "app");
        assert_eq!(qualified_name("a", "app"), "a:app");
    }
}
