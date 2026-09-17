use anyhow::Result;
use ccpick::cache::{Cache, default_cache_path};
use ccpick::catalog::Catalog;
use ccpick::{config, homes, launch, providers, report, ui};
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
    let providers = providers::all();
    let markers: Vec<&str> = providers
        .iter()
        .flat_map(|p| p.home_markers().iter().copied())
        .collect();
    let (homes, home_warnings) = homes::discover_homes(&settings, &markers, homes::running_distros);
    let mut catalog = Catalog::build(providers, &settings, &homes, &mut cache)?;
    catalog.warnings.extend(home_warnings);
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
        Some(plan) => {
            let err = launch::exec(&plan);
            anyhow::bail!("failed to launch {}: {err}", plan.argv.join(" "))
        }
        None => Ok(0),
    }
}
