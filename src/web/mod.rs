//! A local web portal over the same catalog the TUI uses. Agent-neutral: nothing here may
//! know Claude file formats, ccs, or CLAUDE_CONFIG_DIR.
pub mod json;
pub mod route;
pub mod server;
pub mod state;

use crate::cache::Cache;
use crate::config::Settings;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The page and its assets, embedded so the binary is self-contained and the page works with no
/// network access.
pub const PAGE: &str = include_str!("assets/index.html");
pub const STYLE: &str = include_str!("assets/app.css");
pub const SCRIPT: &str = include_str!("assets/app.js");

pub struct WebOptions {
    pub port: u16,
    pub refresh: u64,
    pub open: bool,
}

/// Runs the portal until interrupted (Ctrl-C, or the process is otherwise killed): the refresh
/// loop underneath this never sets its own `stop` flag.
pub fn run(settings: Settings, mut cache: Cache, options: WebOptions) -> anyhow::Result<()> {
    let catalog = crate::catalog::build_from_settings(&settings, &mut cache)?;
    let token = server::mint_token();
    let portal = Arc::new(state::Portal::new(catalog, token.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let port = server::serve(portal.clone(), options.port, stop.clone())?;
    let url = format!("http://127.0.0.1:{port}/?t={token}");
    println!("ccpick: serving at {url}");
    if options.open {
        open_browser(&url);
    }
    // The cache is shared across refreshes on purpose: it is what makes a quiet tick cheap.
    // Rebuilding with a fresh in-memory cache every 10 seconds would rescan every transcript.
    let cache = Arc::new(std::sync::Mutex::new(cache));
    let refresh_settings = settings.clone();
    let refresh_cache = cache.clone();
    state::refresh_loop(
        portal,
        Duration::from_secs(options.refresh.max(1)),
        move || {
            let mut cache = refresh_cache.lock().unwrap();
            crate::catalog::build_from_settings(&refresh_settings, &mut cache)
        },
        stop,
    );
    // `refresh_loop` only returns once `stop` is set, and nothing above ever does that (see this
    // function's own doc comment), so this line never runs in practice: the portal keeps its
    // cache in memory for the life of the run rather than persisting it to disk on exit.
    Ok(())
}

fn open_browser(url: &str) {
    let argv: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["open", url]
    } else if cfg!(windows) {
        vec!["cmd.exe", "/c", "start", "", url]
    } else {
        vec!["xdg-open", url]
    };
    let _ = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
