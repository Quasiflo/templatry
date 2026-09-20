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

/// Project configuration: a singular `[source]` table (multi-source is future work).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Template source reference plus label layering.
    pub source: SourceRef,
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
    /// GitHub release asset glob (e.g. `configs_*.zip`).
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
                self.require_asset()?;
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

    fn require_asset(&self) -> crate::Result<&str> {
        self.asset.as_deref().filter(|a| !a.is_empty()).ok_or_else(|| {
            crate::invalid(
                Path::new("templatry.toml"),
                "GitHub release sources require `asset` (release asset glob, e.g. `configs_*.zip`)",
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

/// Template source definition: defaults, templates, and label rules.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    /// Default directories (all defaults apply when `[configs]` is omitted).
    #[serde(default)]
    pub configs: Configs,
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

/// One template file: paths, merge strategy, labels.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// Template file path, relative to the source root.
    pub template: String,
    /// Override filename (defaults to the template basename).
    #[serde(default)]
    pub override_file: Option<String>,
    /// Override directory (defaults to `default_override_dir`).
    #[serde(default)]
    pub override_dir: Option<String>,
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
    pub array_policy: ArrayPolicy,
    /// Watch the generated file and fold edits back into the override.
    #[serde(default)]
    pub back_propagate: bool,
    /// Arbitrary labels for selection (e.g. `rust`, `dart`).
    #[serde(default)]
    pub labels: BTreeSet<String>,
}

/// How a template merges with its project override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Structured deep merge (also the auto-detect behavior for
    /// `json`/`jsonc`/`yaml`/`yml`/`toml`).
    Merge,
    /// Override bytes, newline, then template bytes.
    AppendTop,
    /// Template bytes, newline, then override bytes (also the fallback for
    /// unknown extensions).
    AppendBottom,
    /// Emit the override file verbatim, ignoring the template.
    Replace,
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
    /// Override filename: explicit value or the template basename.
    pub fn resolved_override_file(&self) -> crate::Result<String> {
        if let Some(file) = self.override_file.as_deref() {
            return Ok(file.to_string());
        }
        basename(&self.template).ok_or_else(|| {
            crate::invalid(
                Path::new("templatry.source.toml"),
                format!(
                    "template `{}` has no filename to default `override_file` from: set it explicitly",
                    self.template
                ),
            )
        })
    }

    /// Generated filename: explicit value or the template basename.
    pub fn resolved_generated_file(&self) -> crate::Result<String> {
        if let Some(file) = self.generated_file.as_deref() {
            return Ok(file.to_string());
        }
        basename(&self.template).ok_or_else(|| {
            crate::invalid(
                Path::new("templatry.source.toml"),
                format!(
                    "template `{}` has no filename to default `generated_file` from: set it explicitly",
                    self.template
                ),
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
    /// Validate schema consistency plus on-disk template references.
    ///
    /// `source_dir` is the directory the `template` paths resolve against;
    /// `display_path` names the source file in diagnostics.
    pub fn validate(&self, source_dir: &Path, display_path: &Path) -> crate::Result<()> {
        if self.templates.is_empty() {
            return Err(crate::invalid(
                display_path,
                "defines no templates: add at least one `[templates.<name>]` entry",
            ));
        }
        if let Some(default) = self.default.as_ref()
            && default.include_labels.is_some()
            && default.exclude_labels.is_some()
        {
            return Err(crate::invalid(
                display_path,
                "`[default]` sets both `include_labels` and `exclude_labels`: keep exactly one (or neither for everything-enabled)",
            ));
        }
        for (name, template) in &self.templates {
            template.validate(name, source_dir, display_path)?;
        }
        self.validate_shared_destinations(display_path)?;
        self.validate_label_refs(display_path)?;
        Ok(())
    }

    /// `replace` cannot combine: flag shared destinations involving it.
    /// Structured and text strategies cannot combine either.
    fn validate_shared_destinations(&self, display_path: &Path) -> crate::Result<()> {
        let mut groups: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
        for (name, template) in &self.templates {
            let key = (
                template.resolved_generated_dir(&self.configs),
                template.resolved_generated_file()?,
            );
            groups.entry(key).or_default().push(name.as_str());
        }
        for ((dir, file), names) in &groups {
            if names.len() < 2 {
                continue;
            }
            if names
                .iter()
                .any(|name| self.templates[*name].strategy == Some(Strategy::Replace))
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
            for name in names {
                let template = &self.templates[*name];
                families.insert(crate::merge::family_of(
                    template,
                    &template.resolved_generated_file()?,
                )?);
                if template.back_propagate {
                    back_propagated.push(*name);
                }
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

    /// `[default]` label lists must reference labels some template defines.
    fn validate_label_refs(&self, display_path: &Path) -> crate::Result<()> {
        let Some(default) = self.default.as_ref() else {
            return Ok(());
        };
        let known = self.all_labels();
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

    /// Union of every label defined by any template.
    fn all_labels(&self) -> BTreeSet<&str> {
        self.templates
            .values()
            .flat_map(|template| template.labels.iter().map(String::as_str))
            .collect()
    }
}

impl Template {
    /// Validate one template entry: path safety plus on-disk existence.
    fn validate(&self, name: &str, source_dir: &Path, display_path: &Path) -> crate::Result<()> {
        if self.template.trim().is_empty() {
            return Err(crate::invalid(
                display_path,
                format!("[templates.{name}] `template` must not be empty"),
            ));
        }
        let relative = Path::new(&self.template);
        if relative.is_absolute() {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `template` must be relative to the source root (got `{}`)",
                    self.template
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
                    "[templates.{name}] `template` must not escape the source root with `..` (got `{}`)",
                    self.template
                ),
            ));
        }
        let on_disk = source_dir.join(relative);
        if !on_disk.is_file() {
            return Err(crate::invalid(
                display_path,
                format!(
                    "[templates.{name}] `template` `{}` does not exist under the source root",
                    self.template
                ),
            ));
        }
        Ok(())
    }
}

// ---- Label resolution -------------------------------------------------------

/// Resolve the enabled template names for a label selection.
///
/// Base rules come from the source `[default]` section (neither list means
/// everything enabled); project `enable_labels` force on, `disable_labels`
/// force off and win ties. Unknown project labels are an error.
pub fn resolve_enabled_templates(
    source: &SourceFile,
    project_enable: &[String],
    project_disable: &[String],
) -> crate::Result<BTreeSet<String>> {
    let known: BTreeSet<&str> = source.all_labels();
    for label in project_enable.iter().chain(project_disable.iter()) {
        if !known.contains(label.as_str()) {
            return Err(crate::invalid(
                Path::new("templatry.toml"),
                format!(
                    "unknown label `{label}` (no template defines it): fix the typo or add the label to a template"
                ),
            ));
        }
    }

    let base_enabled = |template: &Template| match source.default.as_ref() {
        Some(default) if let Some(include) = default.include_labels.as_ref() => {
            template.labels.iter().any(|label| include.contains(label))
        }
        Some(default) if let Some(exclude) = default.exclude_labels.as_ref() => {
            !template.labels.iter().any(|label| exclude.contains(label))
        }
        _ => true,
    };

    let mut enabled = BTreeSet::new();
    for (name, template) in &source.templates {
        let mut on = base_enabled(template);
        if template
            .labels
            .iter()
            .any(|label| project_enable.contains(label))
        {
            on = true;
        }
        if template
            .labels
            .iter()
            .any(|label| project_disable.contains(label))
        {
            on = false;
        }
        if on {
            enabled.insert(name.clone());
        }
    }
    Ok(enabled)
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
        parse_project(&format!("[source]\n{body}")).source
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
        assert_eq!(template.array_policy, ArrayPolicy::Union);
        assert!(!template.back_propagate);
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
        assert_eq!(template.array_policy, ArrayPolicy::Replace);
        assert!(template.back_propagate);
        assert!(template.labels.contains("rust"));
    }

    #[test]
    fn all_strategy_values_parse() {
        for (value, expected) in [
            ("merge", Strategy::Merge),
            ("append_top", Strategy::AppendTop),
            ("append_bottom", Strategy::AppendBottom),
            ("replace", Strategy::Replace),
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
        let enabled = resolve_enabled_templates(&source, &[], &[]).unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn include_selects_matching_templates() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\n[default]\ninclude_labels = [\"rust\"]\n",
        );
        let enabled = resolve_enabled_templates(&source, &[], &[]).unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string()]));
    }

    #[test]
    fn empty_include_disables_everything() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[default]\ninclude_labels = []\n",
        );
        let enabled = resolve_enabled_templates(&source, &[], &[]).unwrap();
        assert!(enabled.is_empty());
    }

    #[test]
    fn exclude_removes_matching_templates() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\n[default]\nexclude_labels = [\"rust\"]\n",
        );
        let enabled = resolve_enabled_templates(&source, &[], &[]).unwrap();
        assert_eq!(enabled, BTreeSet::from(["b".to_string()]));
    }

    #[test]
    fn project_enable_adds_and_disable_wins() {
        let source = parse_source(
            "[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n[templates.b]\ntemplate = \"b.json\"\nlabels = [\"dart\"]\n[default]\ninclude_labels = [\"rust\"]\n",
        );
        let enable = ["dart".to_string()];
        let enabled = resolve_enabled_templates(&source, &enable, &[]).unwrap();
        assert_eq!(enabled, BTreeSet::from(["a".to_string(), "b".to_string()]));

        let disable = ["rust".to_string(), "dart".to_string()];
        let enabled = resolve_enabled_templates(&source, &enable, &disable).unwrap();
        assert!(enabled.is_empty());
    }

    #[test]
    fn unknown_project_label_rejected() {
        let source = parse_source("[templates.a]\ntemplate = \"a.json\"\nlabels = [\"rust\"]\n");
        let err = resolve_enabled_templates(&source, &["nope".to_string()], &[]).unwrap_err();
        assert!(err.to_string().contains("unknown label `nope`"), "{err:?}");
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

        let err = project_ref("github = \"org/repo\"\nasset = \"c_*.zip\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("require `ref`"), "{err:?}");

        let err = project_ref("github = \"org/repo\"\nref = \"v1\"\n")
            .kind()
            .unwrap_err();
        assert!(err.to_string().contains("require `asset`"), "{err:?}");

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
            source: SourceRef {
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
            },
        }
        .source
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
}
