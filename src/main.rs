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
