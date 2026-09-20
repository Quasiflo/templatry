//! Generation orchestration, check/dry-run diffing, and back-propagation.
//!
//! One-shot generation and `--check` (Milestone 3); watch mode (Milestone 4)
//! and full two-way sync (Milestone 5) plug into [`run`] next.

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
    /// Keep watching and regenerate on change (Milestone 4).
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

/// Conflict snapshot directory name inside the system temp dir.
///
/// On back-propagation safety-check mismatch, the conflicting generated
/// content is recoverable here at `<stem>.<timestamp>.conflict.<ext>`.
pub const CONFLICT_SNAPSHOT_DIR_NAME: &str = "templatry-conflicts";

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
    let source: SourceFile = crate::parse_toml(&source_file, &content)?;
    source.validate(&resolved.root_dir, &source_file)?;
    let enabled = config::resolve_enabled_templates(
        &source,
        &project.source.enable_labels,
        &project.source.disable_labels,
    )?;
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
    let mut plan = Vec::with_capacity(groups.len());
    for (dest, members) in &groups {
        plan.push(PlannedWrite {
            dest: dest.clone(),
            content: render_group(dest, members, &context)?,
        });
    }
    Ok(plan)
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
    let template_path = source_root.join(&template.template);
    let template_bytes = crate::read_file_bytes(&template_path)?;
    let generated_filename = template.resolved_generated_file()?;
    let strategy = merge::effective_strategy(template, &generated_filename)?;

    let override_path = project_root
        .join(template.resolved_override_dir(configs))
        .join(template.resolved_override_file()?);
    let override_bytes = match std::fs::read(&override_path) {
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => None,
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(crate::invalid(
                &override_path,
                format!("cannot read override file: {err}"),
            ));
        }
    };

    match strategy {
        EffectiveStrategy::Replace => {
            let Some(bytes) = override_bytes else {
                return Err(crate::invalid(
                    &override_path,
                    format!(
                        "template `{name}` uses `replace` but no override file exists: create it (replace emits the override verbatim)"
                    ),
                ));
            };
            Ok(Rendered::Text(decode(&bytes, &override_path)?))
        }
        EffectiveStrategy::AppendTop | EffectiveStrategy::AppendBottom => {
            let template_text = decode(&template_bytes, &template_path)?;
            let Some(bytes) = override_bytes else {
                return Ok(Rendered::Text(template_text));
            };
            let override_text = decode(&bytes, &override_path)?;
            if override_text.contains(merge::DELETE_MARKER) {
                tracing::warn!(
                    template = name,
                    "override contains `{}` but the strategy is not a structured merge: kept literally",
                    merge::DELETE_MARKER
                );
            }
            match strategy {
                EffectiveStrategy::AppendTop => {
                    Ok(Rendered::Text(format!("{override_text}\n{template_text}")))
                }
                _ => Ok(Rendered::Text(format!("{template_text}\n{override_text}"))),
            }
        }
        EffectiveStrategy::Structured(format) => {
            let template_text = decode(&template_bytes, &template_path)?;
            let base = merge::parse_doc(&template_text, format, &format!("template `{name}`"))?;
            let merged = match override_bytes {
                Some(bytes) => {
                    let override_text = decode(&bytes, &override_path)?;
                    let over = merge::parse_doc(
                        &override_text,
                        format,
                        &format!("override for template `{name}`"),
                    )?;
                    merge::merge_structured(base, over, template.array_policy, name)?
                }
                None => base,
            };
            Ok(Rendered::Structured {
                value: merged,
                format,
            })
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
fn atomic_write(dest: &Path, content: &[u8]) -> crate::Result<()> {
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
