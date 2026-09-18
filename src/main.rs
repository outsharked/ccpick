use anyhow::Result;
use ccpick::cache::{Cache, default_cache_path};
use ccpick::{catalog, config, launch, report, ui};
use clap::Parser;
use std::path::PathBuf;

/// Find and resume coding-agent sessions (Claude Code) across config directories.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Initial search query
    query: Vec<String>,
    /// Print matching sessions as TSV instead of opening the TUI
    #[arg(long)]
    list: bool,
    /// Print resolved sources and stores
    #[arg(long)]
    sources: bool,
    /// Extra config directory to include (repeatable)
    #[arg(long = "config-dir", value_name = "DIR")]
    config_dirs: Vec<PathBuf>,
    /// Config file (default: ~/.config/ccpick/config.toml)
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Don't read or write the metadata cache
    #[arg(long)]
    no_cache: bool,
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("ccpick: {err:#}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().or_else(config::default_config_path);
    let settings = config::load_settings(config_path.as_deref(), cli.config_dirs.clone())?;
    let mut cache = match (cli.no_cache, default_cache_path()) {
        (false, Some(path)) => Cache::load(path),
        _ => Cache::in_memory(),
    };
    let catalog = catalog::build_from_settings(&settings, &mut cache)?;
    if let Err(err) = cache.save() {
        eprintln!("ccpick: warning: could not write cache: {err}");
    }

    if cli.sources {
        print!("{}", report::sources_report(&catalog));
        return Ok(0);
    }
    if catalog.sources.is_empty() {
        eprintln!("ccpick: no session sources found");
        for warning in &catalog.warnings {
            eprintln!("  {warning}");
        }
        return Ok(1);
    }
    let query = cli.query.join(" ");
    if cli.list {
        print!("{}", report::list_tsv(&catalog, &query));
        return Ok(0);
    }
    match ui::run(std::sync::Arc::new(catalog), &query)? {
        Some(plan) => launch_session(&plan),
        None => Ok(0),
    }
}

#[cfg(unix)]
fn launch_session(plan: &ccpick::model::LaunchPlan) -> Result<i32> {
    let err = launch::exec(plan);
    anyhow::bail!("failed to launch {}: {err}", plan.argv.join(" "))
}

#[cfg(windows)]
fn launch_session(plan: &ccpick::model::LaunchPlan) -> Result<i32> {
    launch::run_and_wait(plan)
        .map_err(|err| anyhow::anyhow!("failed to launch {}: {err}", plan.argv.join(" ")))
}
