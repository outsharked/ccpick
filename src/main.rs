use anyhow::Result;
use ccpick::cache::{Cache, default_cache_path};
use ccpick::{catalog, config, export, format, launch, report, ui};
use clap::Parser;
use std::path::PathBuf;

/// Find and resume coding-agent sessions (Claude Code) across config directories.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Initial search query
    query: Vec<String>,
    /// Extra config directory to include (repeatable)
    #[arg(long = "config-dir", value_name = "DIR", global = true)]
    config_dirs: Vec<PathBuf>,
    /// Config file (default: ~/.config/ccpick/config.toml)
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,
    /// Don't read or write the metadata cache
    #[arg(long, global = true)]
    no_cache: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Print resolved sources and stores
    Sources,
    /// Print matching sessions as TSV
    List {
        /// Only sessions matching this query
        query: Vec<String>,
    },
    /// Print recent sessions as JSON, for an LLM to summarise
    ///
    /// One JSON array, one session per line, most recently active first. Each session has its
    /// title, project, branch, timestamps and message count, plus `opening_prompt` (the first
    /// user message) and `tail` (the end of the conversation, role-labelled), each capped.
    Export {
        /// Only sessions matching this query
        query: Vec<String>,
        /// Only sessions active since: an age (36h, 14d, 2w) or a date (2026-09-01)
        #[arg(long, default_value = "14d", value_name = "AGE|DATE")]
        since: String,
        /// Max chars of the opening prompt (0 omits it)
        #[arg(long, default_value_t = 300, value_name = "N")]
        head_chars: usize,
        /// Max chars of the end of the conversation (0 omits it)
        #[arg(long, default_value_t = 700, value_name = "N")]
        tail_chars: usize,
    },
    /// Serve the session list as a local web page
    #[cfg(feature = "web")]
    Web {
        /// Port to listen on (default: chosen by the OS)
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Seconds between refreshes
        #[arg(long, default_value_t = 10)]
        refresh: u64,
        /// Print the URL instead of opening a browser
        #[arg(long)]
        no_open: bool,
    },
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

    #[cfg(feature = "web")]
    if let Some(Command::Web {
        port,
        refresh,
        no_open,
    }) = cli.command
    {
        ccpick::web::run(
            settings,
            cache,
            ccpick::web::WebOptions {
                port,
                refresh,
                open: !no_open,
            },
        )?;
        return Ok(0);
    }

    let catalog = catalog::build_from_settings(&settings, &mut cache)?;
    if let Err(err) = cache.save() {
        eprintln!("ccpick: warning: could not write cache: {err}");
    }

    if let Some(Command::Sources) = cli.command {
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
    match &cli.command {
        Some(Command::List { query }) => {
            print!("{}", report::list_tsv(&catalog, &query.join(" ")));
            return Ok(0);
        }
        Some(Command::Export {
            query,
            since,
            head_chars,
            tail_chars,
        }) => {
            let opts = export::ExportOptions {
                since_ms: export::parse_since(since, format::now_ms())
                    .map_err(anyhow::Error::msg)?,
                head_chars: *head_chars,
                tail_chars: *tail_chars,
            };
            print!("{}", export::export_json(&catalog, &query.join(" "), opts));
            return Ok(0);
        }
        _ => {}
    }
    let query = cli.query.join(" ");
    let rebuild = move || {
        let catalog = catalog::build_from_settings(&settings, &mut cache);
        if catalog.is_ok()
            && let Err(err) = cache.save()
        {
            eprintln!("ccpick: warning: could not write cache: {err}");
        }
        catalog
    };
    match ui::run(std::sync::Arc::new(catalog), &query, rebuild)? {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("ccpick").chain(args.iter().copied()))
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_words_are_the_tui_query() {
        let cli = parse(&["docker", "build"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.query, strings(&["docker", "build"]));
    }

    #[test]
    fn list_takes_a_query() {
        match parse(&["list", "docker", "build"]).unwrap().command {
            Some(Command::List { query }) => assert_eq!(query, strings(&["docker", "build"])),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn sources_is_a_command() {
        assert!(matches!(
            parse(&["sources"]).unwrap().command,
            Some(Command::Sources)
        ));
    }

    #[test]
    fn export_defaults() {
        match parse(&["export"]).unwrap().command {
            Some(Command::Export {
                query,
                since,
                head_chars,
                tail_chars,
            }) => {
                assert!(query.is_empty());
                assert_eq!(since, "14d");
                assert_eq!(head_chars, 300);
                assert_eq!(tail_chars, 700);
            }
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn export_options_and_query() {
        let args = [
            "export",
            "--since",
            "2026-09-01",
            "--head-chars",
            "0",
            "--tail-chars",
            "50",
            "ingress",
        ];
        match parse(&args).unwrap().command {
            Some(Command::Export {
                query,
                since,
                head_chars,
                tail_chars,
            }) => {
                assert_eq!(query, strings(&["ingress"]));
                assert_eq!(since, "2026-09-01");
                assert_eq!(head_chars, 0);
                assert_eq!(tail_chars, 50);
            }
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn shared_flags_work_after_a_command() {
        let cli = parse(&["export", "--no-cache", "--config-dir", "/x"]).unwrap();
        assert!(cli.no_cache);
        assert_eq!(cli.config_dirs, vec![PathBuf::from("/x")]);
    }

    #[test]
    fn double_dash_searches_for_a_command_name() {
        for word in ["list", "export", "sources"] {
            let cli = parse(&["--", word]).unwrap();
            assert!(cli.command.is_none(), "{word}");
            assert_eq!(cli.query, strings(&[word]));
        }
    }

    #[test]
    fn the_old_flags_are_gone() {
        assert!(parse(&["--list"]).is_err());
        assert!(parse(&["--sources"]).is_err());
    }
}
