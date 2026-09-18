pub mod app;
pub mod render;

use crate::catalog::Catalog;
use crate::format::now_ms;
use crate::launch::find_in_path;
use crate::model::LaunchPlan;
use crate::search::SearchWorker;
use app::{Action, App};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::sync::Arc;
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(150);
const TICK: Duration = Duration::from_millis(50);

/// Runs the TUI. Returns the plan to exec, or None if the user quit.
/// `rebuild` produces a fresh catalog for Ctrl-R. It is passed in rather than built here so the
/// UI layer stays ignorant of settings, caches and providers.
pub fn run(
    catalog: Arc<Catalog>,
    query: &str,
    mut rebuild: impl FnMut() -> anyhow::Result<Catalog>,
) -> anyhow::Result<Option<LaunchPlan>> {
    let mut worker = SearchWorker::spawn(catalog.clone(), DEBOUNCE);
    let mut app = App::new(catalog, query);
    if !query.trim().is_empty() {
        let generation = worker.submit(query);
        app.set_text_generation(generation);
    }
    // ratatui::try_init installs a panic hook that restores the terminal.
    // Unlike ratatui::init, it reports failure (e.g. stdout isn't a TTY)
    // instead of panicking; main() maps the error to exit code 2.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, &mut app, &mut worker, &mut rebuild);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    worker: &mut SearchWorker,
    rebuild: &mut impl FnMut() -> anyhow::Result<Catalog>,
) -> anyhow::Result<Option<LaunchPlan>> {
    loop {
        while let Ok((generation, hits)) = worker.results.try_recv() {
            app.apply_text_hits(generation, hits);
        }
        terminal.draw(|frame| render::draw(frame, app, now_ms()))?;
        if !event::poll(TICK)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.handle_key(key) {
            Action::None => {}
            Action::Quit => return Ok(None),
            Action::Search(query) => {
                let generation = worker.submit(&query);
                app.set_text_generation(generation);
            }
            Action::Launch(mut plan) => match find_in_path(&plan.argv[0]) {
                Some(path) => {
                    plan.argv[0] = path.display().to_string();
                    return Ok(Some(plan));
                }
                None => app.status = Some(format!("{} not found on PATH", plan.argv[0])),
            },
            Action::Refresh => {
                // The rebuild blocks — a second or so when it has to ask Windows what is running
                // — so say what is happening before the UI goes still.
                app.status = Some("refreshing…".into());
                terminal.draw(|frame| render::draw(frame, app, now_ms()))?;
                match rebuild() {
                    Ok(catalog) => {
                        let catalog = Arc::new(catalog);
                        app.replace_catalog(catalog.clone());
                        // The old worker holds an Arc of the stale catalog; dropping it ends its
                        // thread, and the new one searches what the user can now see.
                        *worker = SearchWorker::spawn(catalog, DEBOUNCE);
                        let query = app.query.clone();
                        if !query.trim().is_empty() {
                            let generation = worker.submit(&query);
                            app.set_text_generation(generation);
                        }
                        app.status = None;
                    }
                    Err(err) => app.status = Some(format!("could not refresh: {err}")),
                }
            }
            Action::Focus { pid, source, title } => {
                let env = app.catalog.sources[source].env.clone();
                let result = match crate::process::PidDomain::of(&env) {
                    Some(domain) => crate::focus::focus_session(
                        pid,
                        domain,
                        &env,
                        &app.catalog.host,
                        Some(&title),
                    ),
                    None => Err("no process model for this environment".into()),
                };
                // Success clears the status; when no terminal can be found, fall back to
                // reporting that the session is running.
                app.status = match result {
                    Ok(message) if message.is_empty() => None,
                    Ok(message) => Some(message),
                    Err(error) => Some(format!(
                        "running in {} (pid {pid}) — {error}",
                        app.catalog.sources[source].name
                    )),
                };
            }
            Action::Copy(text) => {
                let result = crate::clipboard::copy(&text, &app.catalog.host);
                app.set_copy_result(result);
            }
        }
    }
}
