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
pub fn run(catalog: Arc<Catalog>, query: &str) -> anyhow::Result<Option<LaunchPlan>> {
    let worker = SearchWorker::spawn(catalog.clone(), DEBOUNCE);
    let mut app = App::new(catalog, query);
    if !query.trim().is_empty() {
        let generation = worker.submit(query);
        app.set_text_generation(generation);
    }
    // ratatui::try_init installs a panic hook that restores the terminal.
    // Unlike ratatui::init, it reports failure (e.g. stdout isn't a TTY)
    // instead of panicking; main() maps the error to exit code 2.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, &mut app, &worker);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    worker: &SearchWorker,
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
            Action::Copy(text) => {
                let result = crate::clipboard::copy(&text, &app.catalog.host);
                app.set_copy_result(result);
            }
        }
    }
}
