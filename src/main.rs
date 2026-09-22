//! `templatry` binary: thin `clap` + `tokio` shell over the [`templatry`] library.
//!
//! Argument parsing, logging setup, and exit codes live here; every verb
//! delegates to a library entry point so behavior stays unit-testable.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use tracing::level_filters::LevelFilter;

#[derive(Debug, Parser)]
#[command(
    name = "templatry",
    version,
    about = "Write Once, Use Everywhere: template & distribute configuration files."
)]
struct Cli {
    /// Increase log verbosity; repeat for more detail.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Silence all non-error output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Alias for `generate --watch`; only valid without a subcommand.
    #[arg(long)]
    watch: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate configuration files (also the default with no subcommand).
    Generate {
        /// Keep watching override files and regenerate on change.
        #[arg(long)]
        watch: bool,
        /// Compare generated output against disk without writing; exit 2 on diff.
        #[arg(long, conflicts_with = "watch")]
        check: bool,
        /// Print planned writes without touching disk.
        #[arg(long, conflicts_with_all = ["check", "watch"])]
        dry_run: bool,
        /// Use a project config other than `.config/templatry.toml`.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Never touch the network; fail on cache miss.
        #[arg(long)]
        offline: bool,
    },
    /// Validate project and/or source configuration.
    Validate {
        /// Validate a project config other than `.config/templatry.toml`.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Manage the template source cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
}

#[derive(Debug, Subcommand)]
enum CacheCommands {
    /// Flush the entire cache; the next run re-pulls all sources.
    Clear,
    /// List cached template sources, most recently used first.
    List,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);
    run(cli).await
}

async fn run(cli: Cli) -> miette::Result<()> {
    match cli.command {
        None => {
            let options = templatry::generate::Options {
                watch: cli.watch,
                ..Default::default()
            };
            run_generate(&options).await
        }
        Some(Commands::Generate {
            watch,
            check,
            dry_run,
            config,
            offline,
        }) => {
            reject_bare_watch_flag(cli.watch)?;
            run_generate(&templatry::generate::Options {
                watch,
                check,
                dry_run,
                config,
                offline,
            })
            .await
        }
        Some(Commands::Validate { config }) => {
            reject_bare_watch_flag(cli.watch)?;
            templatry::validate::run(config.as_deref()).await?;
            Ok(())
        }
        Some(Commands::Cache {
            command: CacheCommands::Clear,
        }) => {
            reject_bare_watch_flag(cli.watch)?;
            templatry::source::cache_clear()?;
            Ok(())
        }
        Some(Commands::Cache {
            command: CacheCommands::List,
        }) => {
            reject_bare_watch_flag(cli.watch)?;
            print_cache_list(&templatry::source::cache_list());
            Ok(())
        }
    }
}

/// Print cached template sources as a table, most recently used first.
fn print_cache_list(entries: &[templatry::source::CachedSource]) {
    if entries.is_empty() {
        println!("template source cache is empty");
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows: Vec<[String; 7]> = entries
        .iter()
        .map(|entry| {
            let mut modifiers = Vec::new();
            if let Some(root) = entry.root.as_deref().filter(|root| !root.is_empty()) {
                modifiers.push(format!("root {root}"));
            }
            if let Some(asset) = entry.asset.as_deref().filter(|asset| !asset.is_empty()) {
                modifiers.push(format!("asset {asset}"));
            }
            let source = match entry.location.clone() {
                Some(location) if !modifiers.is_empty() => {
                    format!("{} [{}]", location, modifiers.join(", "))
                }
                Some(location) => location,
                None => "(unknown)".to_string(),
            };
            [
                entry.source_id.chars().take(12).collect(),
                entry.kind.clone().unwrap_or_else(|| "-".to_string()),
                source,
                entry.r#ref.clone().unwrap_or_else(|| "-".to_string()),
                format_size(entry.size_bytes),
                format_age(now, entry.last_used_unix),
                entry.path.display().to_string(),
            ]
        })
        .collect();
    let headers = ["ID", "KIND", "SOURCE", "REF", "SIZE", "LAST USED", "PATH"];
    let mut widths = [0; 7];
    for (index, header) in headers.iter().enumerate() {
        widths[index] = header.len();
    }
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.len());
        }
    }
    let print_row = |cells: &[String; 7]| {
        println!(
            "{:<id$}  {:<kind$}  {:<source$}  {:<ref$}  {:>size$}  {:<age$}  {}",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            cells[4],
            cells[5],
            cells[6],
            id = widths[0],
            kind = widths[1],
            source = widths[2],
            r#ref = widths[3],
            size = widths[4],
            age = widths[5],
        );
    };
    print_row(&headers.map(str::to_string));
    for row in &rows {
        print_row(row);
    }
}

/// Bytes as `512 B`, `1.2 KB`, `3.4 MB`.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Unix timestamp as `45s ago`, `3m ago`, `2h ago`, `5d ago` (`-` when unknown).
fn format_age(now: u64, then: Option<u64>) -> String {
    let Some(then) = then else {
        return "-".to_string();
    };
    let age = now.saturating_sub(then);
    if age < 60 {
        format!("{age}s ago")
    } else if age < 3_600 {
        format!("{}m ago", age / 60)
    } else if age < 86_400 {
        format!("{}h ago", age / 3_600)
    } else {
        format!("{}d ago", age / 86_400)
    }
}

/// Run generation, mapping `--check` differences to exit code 2.
///
/// The differing paths are already printed; CI only needs the code.
async fn run_generate(options: &templatry::generate::Options) -> miette::Result<()> {
    match templatry::generate::run(options).await {
        Ok(()) => Ok(()),
        Err(templatry::Error::CheckDifferences { .. }) => std::process::exit(2),
        Err(other) => Err(other.into()),
    }
}

/// The top-level `--watch` alias is only meaningful without a subcommand.
fn reject_bare_watch_flag(watch: bool) -> miette::Result<()> {
    if watch {
        return Err(templatry::Error::Usage(
            "top-level `--watch` is only valid without a subcommand; use `templatry generate --watch`".to_string(),
        )
        .into());
    }
    Ok(())
}

/// `tracing` subscriber: flag-derived level by default, `RUST_LOG` overrides.
fn init_logging(verbose: u8, quiet: bool) {
    let level = if quiet {
        LevelFilter::OFF
    } else {
        match verbose {
            0 => LevelFilter::INFO,
            1 => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        }
    };
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(level.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
