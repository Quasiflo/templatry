//! Generation orchestration, check/dry-run diffing, and watch support.
//!
//! One-shot generation, `--check`, `--watch` (via [`crate::watch`]), and the
//! pure render core backing back-propagation safety replays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::{
    self, Configs, PROJECT_CONFIG_PATH, ProjectConfig, SOURCE_CONFIG_FILENAME, SourceFile,
    SourceKind, Template,
};
use crate::merge::{self, EffectiveStrategy, GroupPart, Rendered};

/// Options for [`run`], mirroring `templatry generate` flags.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Keep watching and regenerate on change.
    pub watch: bool,
    /// Diff against disk without writing; exit 2 on difference.
    pub check: bool,
    /// Print planned writes without touching disk.
    pub dry_run: bool,
    /// Project config other than [`.config/templatry.toml`](crate::config::PROJECT_CONFIG_PATH).
    pub config: Option<PathBuf>,
    /// Never touch the network; fail on cache miss.
    pub offline: bool,
}

/// One planned file write: destination plus complete new content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// Absolute or project-relative destination file.
    pub dest: PathBuf,
    /// Complete bytes to write.
    pub content: Vec<u8>,
}

/// Generate configuration files for the current project.
///
/// Always writes every enabled template (no content-hash short-circuiting);
/// `--check` diffs in memory and fails with [`crate::Error::CheckDifferences`],
/// `--dry-run` prints planned writes without touching disk.
pub async fn run(options: &Options) -> crate::Result<()> {
    if options.watch {
        if options.check || options.dry_run {
            return Err(crate::Error::Usage(
                "`--watch` cannot combine with `--check` or `--dry-run`: run them separately"
                    .to_string(),
            ));
        }
        return crate::watch::run(options).await;
    }
    let plan = plan(options).await?;
    if options.check {
        return apply_check(&plan);
    }
    if options.dry_run {
        for write in &plan {
            println!("would write {}", write.dest.display());
        }
        return Ok(());
    }
    write_plan(&plan)?;
    tracing::info!(files = plan.len(), "generated configuration files");
    Ok(())
}

/// Loaded project state shared by one-shot generation and watch cycles.
#[derive(Debug, Clone)]
pub(crate) struct ProjectContext {
    /// Project config file (change triggers a full reload in watch mode).
    pub project_file: PathBuf,
    /// Repository root: base for override, generated, and local-source paths.
    pub project_root: PathBuf,
    /// Source root holding `templatry.source.toml`.
    pub source_root: PathBuf,
    /// Validated source definition.
    pub source: SourceFile,
    /// Enabled template names, sorted.
    pub enabled: Vec<String>,
    /// Source kind (local sources also watch their source file).
    pub kind: SourceKind,
}

/// Locate, resolve, load, validate, and select: everything before rendering.
pub(crate) async fn load_context(options: &Options) -> crate::Result<ProjectContext> {
    let (project_file, project_root) = locate_project(options.config.as_deref())?;
    let content = crate::read_file(&project_file)?;
    let project: ProjectConfig = crate::parse_toml(&project_file, &content)?;
    let resolved = crate::source::resolve(&project.source, &project_root, options.offline).await?;
    let source_file = resolved.root_dir.join(SOURCE_CONFIG_FILENAME);
    let content = crate::read_file(&source_file)?;
    let mut source: SourceFile = crate::parse_toml(&source_file, &content)?;
    source.validate(&resolved.root_dir, &source_file)?;
    source.templates = source.resolved_templates(&source_file)?;
    let enabled =
        config::resolve_enabled_templates(&source.templates, &source.default, &project.source)?;
    Ok(ProjectContext {
        project_file,
        project_root,
        source_root: resolved.root_dir,
        source,
        enabled: enabled.into_iter().collect(),
        kind: resolved.kind,
    })
}

/// One destination group member: everything needed to re-render it on change.
#[derive(Debug, Clone)]
pub(crate) struct GroupMember {
    pub name: String,
    pub labels: BTreeSet<String>,
    pub template: Template,
}

/// Group enabled templates by destination file.
pub(crate) fn group_members(
    context: &ProjectContext,
) -> crate::Result<BTreeMap<PathBuf, Vec<GroupMember>>> {
    let mut groups: BTreeMap<PathBuf, Vec<GroupMember>> = BTreeMap::new();
    for name in &context.enabled {
        let template = &context.source.templates[name];
        let dest = context
            .project_root
            .join(template.resolved_generated_dir(&context.source.configs))
            .join(template.resolved_generated_file()?);
        groups.entry(dest).or_default().push(GroupMember {
            name: name.clone(),
            labels: template.labels.clone(),
            template: template.clone(),
        });
    }
    Ok(groups)
}

/// Render one destination group into final bytes.
pub(crate) fn render_group(
    dest: &Path,
    members: &[GroupMember],
    context: &ProjectContext,
) -> crate::Result<Vec<u8>> {
    let mut rendered = Vec::with_capacity(members.len());
    for member in members {
        let output = render_template(
            &member.template,
            &member.name,
            &context.source_root,
            &context.project_root,
            &context.source.configs,
        )?;
        rendered.push((member.name.as_str(), &member.labels, output));
    }
    let parts: Vec<GroupPart<'_>> = rendered
        .iter()
        .map(|(name, labels, output)| GroupPart {
            template: name,
            labels,
            rendered: output,
        })
        .collect();
    merge::combine_group(dest, &parts)
}

/// Compute the full write plan without touching disk (the test seam for [`run`]).
pub async fn plan(options: &Options) -> crate::Result<Vec<PlannedWrite>> {
    let context = load_context(options).await?;
    let groups = group_members(&context)?;
    render_plan(&context, &groups)
}

/// Render every destination group with forward preservation.
///
/// Shared by one-shot planning and watch cycles so both write identical bytes.
pub(crate) fn render_plan(
    context: &ProjectContext,
    groups: &BTreeMap<PathBuf, Vec<GroupMember>>,
) -> crate::Result<Vec<PlannedWrite>> {
    let mut plan = Vec::with_capacity(groups.len());
    for (dest, members) in groups {
        let content = render_group(dest, members, context)?;
        let content = preserve_group(dest, &content, members)?;
        plan.push(PlannedWrite {
            dest: dest.clone(),
            content,
        });
    }
    Ok(plan)
}

/// Apply forward preservation for a rendered group.
///
/// Groups without ignore lists pass through untouched. Otherwise the on-disk
/// file, when present, supplies maintained state (see [`crate::backprop`]).
pub(crate) fn preserve_group(
    dest: &Path,
    combined: &[u8],
    members: &[GroupMember],
) -> crate::Result<Vec<u8>> {
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut format = None;
    for member in members {
        for pattern in &member.template.backprop_ignore {
            keys.push(parse_ignore_pattern(pattern)?);
        }
        for pattern in &member.template.backprop_ignore_values {
            values.push(parse_ignore_pattern(pattern)?);
        }
        if format.is_none() {
            let generated = member.template.resolved_generated_file()?;
            if let crate::merge::EffectiveStrategy::Structured(found) =
                crate::merge::effective_strategy(&member.template, &generated)?
            {
                format = Some(found);
            }
        }
    }
    let Some(format) = format else {
        if keys.is_empty() && values.is_empty() {
            return Ok(combined.to_vec());
        }
        return Err(crate::invalid(
            dest,
            "internal error: preserved group has no structured format".to_string(),
        ));
    };
    crate::backprop::preserve_maintained(dest, combined, format, &keys, &values)
}

/// Parse an ignore pattern (validated upstream; failures stay loud).
fn parse_ignore_pattern(pattern: &str) -> crate::Result<crate::backprop::IgnorePattern> {
    crate::backprop::IgnorePattern::parse(pattern).map_err(|reason| {
        crate::invalid(
            Path::new("templatry.source.toml"),
            format!("invalid ignore pattern `{pattern}`: {reason}"),
        )
    })
}

/// Write every planned file atomically.
pub(crate) fn write_plan(plan: &[PlannedWrite]) -> crate::Result<()> {
    for write in plan {
        atomic_write(&write.dest, &write.content)?;
    }
    Ok(())
}

/// Render one template with its project override into a [`Rendered`] output.
pub(crate) fn render_template(
    template: &Template,
    name: &str,
    source_root: &Path,
    project_root: &Path,
    configs: &Configs,
) -> crate::Result<Rendered> {
    let template_path = source_root.join(template.template_path()?);
    let template_bytes = crate::read_file_bytes(&template_path)?;
    let generated_filename = template.resolved_generated_file()?;
    let strategy = merge::effective_strategy(template, &generated_filename)?;

    let override_dir = project_root.join(template.resolved_override_dir(configs));
    let override_path = override_dir.join(template.resolved_override_file()?);
    let override_text = match read_optional(&override_path, "override file")? {
        Some(bytes) => Some(decode(&bytes, &override_path)?),
        None => None,
    };
    let local_text = match template
        .local_override_file
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        Some(name) => {
            let local_path = override_dir.join(name);
            match read_optional(&local_path, "local override file")? {
                Some(bytes) => Some(decode(&bytes, &local_path)?),
                None => None,
            }
        }
        None => None,
    };

    match strategy {
        EffectiveStrategy::Replace => {
            let Some(text) = override_text else {
                return Err(crate::invalid(
                    &override_path,
                    format!(
                        "template `{name}` uses `replace` but no override file exists: create it (replace emits the override verbatim)"
                    ),
                ));
            };
            if local_text.is_some() {
                tracing::warn!(
                    template = name,
                    "local override ignored: strategy `replace` emits the override verbatim"
                );
            }
            Ok(Rendered::Text(text))
        }
        _ => {
            let template_text = decode(&template_bytes, &template_path)?;
            render_contents(
                &template_text,
                override_text.as_deref(),
                local_text.as_deref(),
                strategy,
                template.array_policy.unwrap_or_default(),
                name,
            )
        }
    }
}

/// Read an optional layer file: missing or whitespace-only means absent.
fn read_optional(path: &Path, what: &str) -> crate::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(None),
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(crate::invalid(path, format!("cannot read {what}: {err}"))),
    }
}

/// Render template, override, and local override text into a [`Rendered`] output.
///
/// Pure over file contents: file IO, missing-file handling, and strategy
/// resolution stay in [`render_template`]. Precedence is template, then
/// override, then local. Back-propagation safety replays call this directly.
pub(crate) fn render_contents(
    template_text: &str,
    override_text: Option<&str>,
    local_text: Option<&str>,
    strategy: EffectiveStrategy,
    policy: crate::config::ArrayPolicy,
    name: &str,
) -> crate::Result<Rendered> {
    match strategy {
        EffectiveStrategy::Replace => {
            let Some(text) = override_text else {
                return Err(crate::invalid(
                    Path::new("templatry.source.toml"),
                    format!(
                        "template `{name}` uses `replace` but no override content exists: create the override file (replace emits it verbatim)"
                    ),
                ));
            };
            if local_text.is_some() {
                tracing::warn!(
                    template = name,
                    "local override ignored: strategy `replace` emits the override verbatim"
                );
            }
            Ok(Rendered::Text(text.to_string()))
        }
        EffectiveStrategy::AppendTop | EffectiveStrategy::AppendBottom => {
            for text in [override_text, local_text].into_iter().flatten() {
                if text.contains(merge::DELETE_MARKER) {
                    tracing::warn!(
                        template = name,
                        "override contains `{}` but the strategy is not a structured merge: kept literally",
                        merge::DELETE_MARKER
                    );
                    break;
                }
            }
            // Segments stay precedence-ordered (highest first for top,
            // lowest first for bottom); missing layers vanish with no
            // stray separators. Emptied layers vanish too: forward reads
            // never produce them (whitespace-only files count as missing),
            // but back-propagation replays a reverted override as `Some("")`
            // and must reproduce the file exactly.
            let mut segments: Vec<&str> = Vec::with_capacity(3);
            if strategy == EffectiveStrategy::AppendTop {
                segments.extend(local_text.filter(|text| !text.is_empty()));
                segments.extend(override_text.filter(|text| !text.is_empty()));
                segments.push(template_text);
            } else {
                segments.push(template_text);
                segments.extend(override_text.filter(|text| !text.is_empty()));
                segments.extend(local_text.filter(|text| !text.is_empty()));
            }
            Ok(Rendered::Text(segments.join("\n")))
        }
        EffectiveStrategy::Structured(format) => {
            let base = merge::parse_doc(template_text, format, &format!("template `{name}`"))?;
            let merged = match override_text {
                Some(text) => {
                    let over =
                        merge::parse_doc(text, format, &format!("override for template `{name}`"))?;
                    merge::merge_structured(base, over, policy, name)?
                }
                None => base,
            };
            let merged = match local_text {
                Some(text) => {
                    let local = merge::parse_doc(
                        text,
                        format,
                        &format!("local override for template `{name}`"),
                    )?;
                    merge::merge_structured(merged, local, policy, name)?
                }
                None => merged,
            };
            Ok(Rendered::Structured {
                value: merged,
                format,
            })
        }
        EffectiveStrategy::None => {
            if override_text.is_some() || local_text.is_some() {
                tracing::warn!(
                    template = name,
                    "overrides ignored: strategy `none` always copies the template verbatim"
                );
            }
            Ok(Rendered::Text(template_text.to_string()))
        }
    }
}

/// Decode file bytes as UTF-8 for merging.
fn decode(bytes: &[u8], path: &Path) -> crate::Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|err| crate::invalid(path, format!("file is not valid UTF-8: {err}")))
}

/// Locate the project file and repository root.
///
/// Explicit `--config` wins (made absolute against the working directory);
/// otherwise the default path must exist, with a hint when run inside a
/// template repository by mistake. The root follows
/// [`config::project_root`], so canonical `.config/templatry.toml` layouts
/// resolve `.config/`-relative paths against the repository.
fn locate_project(config: Option<&Path>) -> crate::Result<(PathBuf, PathBuf)> {
    if let Some(path) = config {
        let cwd = std::env::current_dir().map_err(|err| {
            crate::invalid(
                Path::new("."),
                format!("cannot determine working directory: {err}"),
            )
        })?;
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        if !path.is_file() {
            return Err(crate::invalid(
                &path,
                "project config does not exist".to_string(),
            ));
        }
        let root = config::project_root(&path);
        return Ok((path, root));
    }
    let cwd = std::env::current_dir().map_err(|err| {
        crate::invalid(
            Path::new("."),
            format!("cannot determine working directory: {err}"),
        )
    })?;
    let path = cwd.join(PROJECT_CONFIG_PATH);
    if !path.is_file() {
        if cwd.join(SOURCE_CONFIG_FILENAME).is_file() {
            return Err(crate::invalid(
                &cwd,
                format!(
                    "this looks like a template repository (`{SOURCE_CONFIG_FILENAME}` present): `generate` runs in project repositories, template repositories use `templatry validate`"
                ),
            ));
        }
        return Err(crate::invalid(
            &cwd,
            format!(
                "no `{PROJECT_CONFIG_PATH}` found: run from a project repository or pass `--config <PATH>`"
            ),
        ));
    }
    Ok((path.clone(), config::project_root(&path)))
}

/// Diff the plan against disk: print differing paths, fail on any difference.
fn apply_check(plan: &[PlannedWrite]) -> crate::Result<()> {
    let mut differing = Vec::new();
    for write in plan {
        let current = std::fs::read(&write.dest).unwrap_or_default();
        if current != write.content {
            differing.push(write.dest.clone());
        }
    }
    if differing.is_empty() {
        return Ok(());
    }
    for dest in &differing {
        println!("{}", dest.display());
    }
    Err(crate::Error::CheckDifferences {
        count: differing.len(),
    })
}

/// Write a file atomically via temp-file-plus-rename in the same directory.
pub(crate) fn atomic_write(dest: &Path, content: &[u8]) -> crate::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|err| crate::invalid(dest, format!("cannot create parent directory: {err}")))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .map_err(|err| crate::invalid(dest, format!("cannot stage write: {err}")))?;
    std::io::Write::write_all(&mut staged, content)
        .map_err(|err| crate::invalid(dest, format!("cannot stage write: {err}")))?;
    staged
        .persist(dest)
        .map_err(|err| crate::invalid(dest, format!("cannot replace file: {}", err.error)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::DocFormat;

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("test json parses")
    }

    fn rendered_value(rendered: Rendered) -> serde_json::Value {
        match rendered {
            Rendered::Structured { value, .. } => value,
            Rendered::Text(text) => panic!("expected structured, got text: {text}"),
        }
    }

    fn rendered_text(rendered: Rendered) -> String {
        match rendered {
            Rendered::Text(text) => text,
            Rendered::Structured { .. } => panic!("expected text, got structured"),
        }
    }

    const POLICY: crate::config::ArrayPolicy = crate::config::ArrayPolicy::Union;

    #[test]
    fn structured_local_wins_over_override_wins_over_template() {
        let rendered = render_contents(
            "{\"a\": 1, \"b\": 1, \"c\": 1}",
            Some("{\"b\": 2, \"c\": 2}"),
            Some("{\"c\": 3}"),
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(
            rendered_value(rendered),
            json("{\"a\": 1, \"b\": 2, \"c\": 3}")
        );
    }

    #[test]
    fn structured_layers_skip_missing() {
        // Template plus local, no override.
        let rendered = render_contents(
            "{\"a\": 1}",
            None,
            Some("{\"c\": 3}"),
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_value(rendered), json("{\"a\": 1, \"c\": 3}"));

        // Template plus override, no local.
        let rendered = render_contents(
            "{\"a\": 1}",
            Some("{\"b\": 2}"),
            None,
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_value(rendered), json("{\"a\": 1, \"b\": 2}"));

        // Template alone.
        let rendered = render_contents(
            "{\"a\": 1}",
            None,
            None,
            EffectiveStrategy::Structured(DocFormat::Json),
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_value(rendered), json("{\"a\": 1}"));
    }

    #[test]
    fn append_orders_segments_by_precedence() {
        // Bottom: template, override, local.
        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T\nO\nL");

        // Top: local, override, template.
        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::AppendTop,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "L\nO\nT");

        // Missing layers vanish with no stray separators (legacy behavior).
        let rendered = render_contents(
            "T",
            Some("O"),
            None,
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T\nO");
        let rendered = render_contents(
            "T",
            None,
            None,
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T");
        let rendered = render_contents(
            "T",
            Some("O"),
            None,
            EffectiveStrategy::AppendTop,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "O\nT");

        // Local without override still applies.
        let rendered = render_contents(
            "T",
            None,
            Some("L"),
            EffectiveStrategy::AppendBottom,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T\nL");
        let rendered = render_contents(
            "T",
            None,
            Some("L"),
            EffectiveStrategy::AppendTop,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "L\nT");
    }

    #[test]
    fn replace_and_none_ignore_local() {
        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::Replace,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "O");

        let rendered = render_contents(
            "T",
            Some("O"),
            Some("L"),
            EffectiveStrategy::None,
            POLICY,
            "t",
        )
        .unwrap();
        assert_eq!(rendered_text(rendered), "T");
    }
}
