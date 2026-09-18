//! TUI state and key handling, independent of the terminal.
use crate::catalog::Catalog;
use crate::model::{LaunchPlan, Message};
use crate::search::{self, TextHit};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Rows moved by PgUp/PgDn in the session list.
const LIST_PAGE: isize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Preview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    LastActivity,
    Created,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Session { idx: usize, snippet: Option<String> },
    Divider,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    Launch(LaunchPlan),
    Search(String),
    Copy(String),
    /// Focus the terminal running this session: its pid, the source it was started from, and its
    /// title, which terminals show on the tab.
    Focus {
        pid: u32,
        source: usize,
        title: String,
    },
}

/// Shown instead of launching when a session belongs to another environment.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeDialog {
    pub session: usize,
    pub source: usize,
    pub command: String,
    pub dir_missing: bool,
    /// Result of the last copy attempt.
    pub note: Option<String>,
}

/// Preview geometry reported by the renderer each frame, used to clamp scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewMetrics {
    /// Total rendered rows of the conversation at the current width.
    pub total: usize,
    /// Rows visible in the message area.
    pub page: usize,
    /// Row the view shows when the user hasn't scrolled (end, or a search hit).
    pub anchor: usize,
}

impl PreviewMetrics {
    pub fn max_scroll(&self) -> usize {
        self.total.saturating_sub(self.page)
    }
}

pub struct App {
    pub catalog: Arc<Catalog>,
    pub query: String,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub focus: Focus,
    pub sort: SortMode,
    pub running_only: bool,
    /// Top row of the preview; None follows the anchor (end of conversation or search hit).
    pub preview_scroll: Option<usize>,
    /// Set by the renderer on every draw.
    pub preview_metrics: Option<PreviewMetrics>,
    /// Renderer's measured layout for the previewed session.
    pub(crate) preview_layout: Option<super::render::PreviewLayout>,
    pub status: Option<String>,
    pub dialog: Option<ResumeDialog>,
    /// Esc asks before quitting, so a stray Esc while typing doesn't drop the session list.
    pub confirm_quit: bool,
    source_override: HashMap<usize, usize>,
    text_hits: Vec<TextHit>,
    text_generation: u64,
    preview: Option<(usize, Vec<Message>)>,
    /// `cwd_missing` results, keyed by (session index, source index): an `is_dir()` stat is a
    /// filesystem call, so it's computed once per pair rather than on every draw.
    cwd_missing_cache: HashMap<(usize, usize), bool>,
}

impl App {
    pub fn new(catalog: Arc<Catalog>, query: &str) -> App {
        let mut app = App {
            catalog,
            query: query.to_string(),
            rows: Vec::new(),
            selected: 0,
            focus: Focus::List,
            sort: SortMode::LastActivity,
            running_only: false,
            preview_scroll: None,
            preview_metrics: None,
            preview_layout: None,
            status: None,
            dialog: None,
            confirm_quit: false,
            source_override: HashMap::new(),
            text_hits: Vec::new(),
            text_generation: 0,
            preview: None,
            cwd_missing_cache: HashMap::new(),
        };
        app.recompute_rows(false);
        // Surface discovery problems (e.g. an unreadable source config) until the first keypress clears them.
        app.status = app
            .catalog
            .warnings
            .first()
            .map(|w| format!("warning: {w}"));
        app
    }

    pub fn selected_session(&self) -> Option<usize> {
        match self.rows.get(self.selected)? {
            Row::Session { idx, .. } => Some(*idx),
            Row::Divider => None,
        }
    }

    pub fn launch_source(&self, idx: usize) -> usize {
        self.source_override
            .get(&idx)
            .copied()
            .unwrap_or(self.catalog.sessions[idx].default_source)
    }

    pub fn text_hit(&self, idx: usize) -> Option<&TextHit> {
        self.text_hits.iter().find(|h| h.session == idx)
    }

    pub fn set_text_generation(&mut self, generation: u64) {
        self.text_generation = generation;
    }

    pub fn apply_text_hits(&mut self, generation: u64, hits: Vec<TextHit>) {
        if generation != self.text_generation {
            return;
        }
        self.text_hits = hits;
        self.recompute_rows(true);
    }

    /// The preview's top row for the given geometry.
    pub fn preview_position(&self, metrics: PreviewMetrics) -> usize {
        self.preview_scroll
            .unwrap_or(metrics.anchor)
            .min(metrics.max_scroll())
    }

    /// Whether the session's project dir, resolved through `source`, doesn't exist on disk.
    /// `None` from `host_cwd` (no path mapping between environments, as opposed to a definite
    /// missing directory) falls back to whether the session recorded a cwd at all, and is never
    /// treated as "missing" on its own. Cached per (session, source) since it's a filesystem
    /// stat, not a pure computation.
    pub fn cwd_missing(&mut self, idx: usize, source: usize) -> bool {
        if let Some(&missing) = self.cwd_missing_cache.get(&(idx, source)) {
            return missing;
        }
        let missing = match self.catalog.host_cwd(idx, source) {
            Some(p) => !p.is_dir(),
            None => self.catalog.sessions[idx].meta.cwd.is_none(),
        };
        self.cwd_missing_cache.insert((idx, source), missing);
        missing
    }

    pub fn preview_messages(&mut self) -> &[Message] {
        let Some(idx) = self.selected_session() else {
            return &[];
        };
        if self.preview.as_ref().map(|p| p.0) != Some(idx) {
            self.preview = Some((idx, self.catalog.messages(idx)));
        }
        &self.preview.as_ref().expect("just set").1
    }

    fn candidates(&self) -> Vec<usize> {
        let sessions = &self.catalog.sessions;
        let mut v: Vec<usize> = (0..sessions.len())
            .filter(|&i| !self.running_only || sessions[i].live.is_some())
            .collect();
        match self.sort {
            SortMode::LastActivity => {
                v.sort_by(|a, b| sessions[*b].meta.last_ts.cmp(&sessions[*a].meta.last_ts))
            }
            SortMode::Created => {
                v.sort_by(|a, b| sessions[*b].meta.first_ts.cmp(&sessions[*a].meta.first_ts))
            }
        }
        v
    }

    fn recompute_rows(&mut self, keep_selection: bool) {
        let previous = if keep_selection {
            self.selected_session()
        } else {
            None
        };
        let candidates = self.candidates();
        let fuzzy = search::fuzzy(&self.catalog, &candidates, &self.query);
        let in_fuzzy: HashSet<usize> = fuzzy.iter().copied().collect();
        let allowed: HashSet<usize> = candidates.iter().copied().collect();

        let mut rows: Vec<Row> = fuzzy
            .into_iter()
            .map(|idx| Row::Session { idx, snippet: None })
            .collect();
        if !self.query.trim().is_empty() {
            let extra: Vec<Row> = self
                .text_hits
                .iter()
                .filter(|h| allowed.contains(&h.session) && !in_fuzzy.contains(&h.session))
                .map(|h| Row::Session {
                    idx: h.session,
                    snippet: Some(h.snippet.clone()),
                })
                .collect();
            if !extra.is_empty() {
                rows.push(Row::Divider);
                rows.extend(extra);
            }
        }
        self.rows = rows;
        self.selected = previous
            .and_then(|p| {
                self.rows
                    .iter()
                    .position(|r| matches!(r, Row::Session { idx, .. } if *idx == p))
            })
            .unwrap_or(0);
        if matches!(self.rows.get(self.selected), Some(Row::Divider))
            && self.selected + 1 < self.rows.len()
        {
            self.selected += 1;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        let mut next = (self.selected as isize + delta).clamp(0, max);
        if matches!(self.rows[next as usize], Row::Divider) {
            next = (next + delta.signum()).clamp(0, max);
            if matches!(self.rows[next as usize], Row::Divider) {
                next = self.selected as isize;
            }
        }
        if next as usize != self.selected {
            self.selected = next as usize;
            self.preview_scroll = None;
            self.status = None;
        }
    }

    fn scroll_preview(&mut self, delta: isize) {
        let Some(metrics) = self.preview_metrics else {
            return;
        };
        let pos = self.preview_position(metrics) as isize;
        let max = metrics.max_scroll() as isize;
        self.preview_scroll = Some((pos + delta).clamp(0, max) as usize);
    }

    /// Up/Down/PgUp/PgDn: move the list selection or scroll the preview. `rows` is a count of
    /// lines, or of pages when `pages` is true.
    fn scroll_or_move(&mut self, rows: isize, pages: bool) {
        match self.focus {
            Focus::List => self.move_selection(if pages { rows * LIST_PAGE } else { rows }),
            Focus::Preview => {
                let page = self.preview_metrics.map_or(1, |m| m.page.max(1)) as isize;
                self.scroll_preview(if pages { rows * page } else { rows });
            }
        }
    }

    /// Home/End: jump to the first/last list row or the top/bottom of the preview.
    fn jump(&mut self, to_end: bool) {
        match self.focus {
            Focus::List => {
                let len = self.rows.len() as isize;
                self.move_selection(if to_end { len } else { -len });
            }
            Focus::Preview => {
                if let Some(metrics) = self.preview_metrics {
                    self.preview_scroll = Some(if to_end { metrics.max_scroll() } else { 0 });
                }
            }
        }
    }

    fn query_changed(&mut self) -> Action {
        self.text_hits.clear();
        self.preview_scroll = None;
        self.status = None;
        self.recompute_rows(false);
        Action::Search(self.query.clone())
    }

    fn cycle_source(&mut self) {
        let Some(idx) = self.selected_session() else {
            return;
        };
        self.status = Some(match self.cycle_source_for(idx) {
            Ok(next) => format!("resume via {}", self.catalog.sources[next].name),
            Err(msg) => msg.into(),
        });
    }

    /// Moves `idx`'s resume source to the next one in its list. Doesn't touch `status`, so it
    /// can be reused by the in-dialog Ctrl-A path, which reports the outcome in the dialog's own
    /// `note` instead.
    fn cycle_source_for(&mut self, idx: usize) -> Result<usize, &'static str> {
        let sources = &self.catalog.sessions[idx].sources;
        if sources.len() < 2 {
            return Err("no other source can resume this session");
        }
        let current = self.launch_source(idx);
        let pos = sources.iter().position(|s| *s == current).unwrap_or(0);
        let next = sources[(pos + 1) % sources.len()];
        self.source_override.insert(idx, next);
        Ok(next)
    }

    pub fn set_copy_result(&mut self, result: Result<&'static str, String>) {
        if let Some(dialog) = &mut self.dialog {
            dialog.note = Some(match result {
                Ok(how) => format!("copied ({how})"),
                Err(e) => e,
            });
        }
    }

    fn open_dialog(&mut self, idx: usize) {
        let source = self.launch_source(idx);
        let plan = self.catalog.launch_plan(idx, source);
        let command = crate::shell::resume_command(&plan, &self.catalog.sources[source].env);
        let dir_missing = self.cwd_missing(idx, source);
        self.dialog = Some(ResumeDialog {
            session: idx,
            source,
            command,
            dir_missing,
            note: None,
        });
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => Action::Quit,
            (KeyCode::Esc, _) => {
                self.dialog = None;
                Action::None
            }
            (KeyCode::Char('c'), false) => match &self.dialog {
                Some(d) => Action::Copy(d.command.clone()),
                None => Action::None,
            },
            (KeyCode::Char('a'), true) => {
                if let Some(idx) = self.dialog.as_ref().map(|d| d.session) {
                    let note = match self.cycle_source_for(idx) {
                        Ok(next) => format!("resume via {}", self.catalog.sources[next].name),
                        Err(msg) => msg.to_string(),
                    };
                    self.open_dialog(idx);
                    if let Some(dialog) = &mut self.dialog {
                        dialog.note = Some(note);
                    }
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn enter(&mut self) -> Action {
        let Some(idx) = self.selected_session() else {
            return Action::None;
        };
        let session = &self.catalog.sessions[idx];
        if let Some((pid, source)) = session.live {
            // A running session can't be resumed a second time; focus its terminal instead.
            self.status = Some(format!(
                "running in {} (pid {pid})",
                self.catalog.sources[source].name
            ));
            let title = session.meta.title.clone();
            return match u32::try_from(pid) {
                Ok(pid) => Action::Focus { pid, source, title },
                Err(_) => Action::None,
            };
        }
        let source = self.launch_source(idx);
        if !self.catalog.is_launchable(source) {
            self.open_dialog(idx);
            return Action::None;
        }
        let cwd = self.catalog.host_cwd(idx, source);
        let missing = self.cwd_missing(idx, source);
        match cwd {
            Some(_) if !missing => Action::Launch(self.catalog.launch_plan(idx, source)),
            Some(cwd) => {
                self.status = Some(format!("project dir no longer exists: {}", cwd.display()));
                Action::None
            }
            None => {
                self.status = Some("session has no recorded project dir".into());
                Action::None
            }
        }
    }

    /// While the quit prompt is up, only "yes" quits; Ctrl-C still quits outright and every
    /// other key dismisses the prompt without acting, so a typo can't resume the wrong session.
    fn handle_confirm_quit_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => Action::Quit,
            (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, false) => Action::Quit,
            _ => {
                self.confirm_quit = false;
                Action::None
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.confirm_quit {
            return self.handle_confirm_quit_key(key);
        }
        if self.dialog.is_some() {
            return self.handle_dialog_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => Action::Quit,
            (KeyCode::Esc, _) => {
                self.confirm_quit = true;
                Action::None
            }
            (KeyCode::Enter, _) => self.enter(),
            (KeyCode::Tab, _) => {
                self.focus = match self.focus {
                    Focus::List => Focus::Preview,
                    Focus::Preview => Focus::List,
                };
                Action::None
            }
            (KeyCode::Char('a'), true) => {
                self.cycle_source();
                Action::None
            }
            (KeyCode::Char('r'), true) => {
                self.running_only = !self.running_only;
                self.recompute_rows(true);
                Action::None
            }
            (KeyCode::Char('s'), true) => {
                self.sort = match self.sort {
                    SortMode::LastActivity => SortMode::Created,
                    SortMode::Created => SortMode::LastActivity,
                };
                self.recompute_rows(true);
                Action::None
            }
            (KeyCode::Right, _) => {
                self.focus = Focus::Preview;
                Action::None
            }
            (KeyCode::Left, _) => {
                self.focus = Focus::List;
                Action::None
            }
            (KeyCode::Up, _) => {
                self.scroll_or_move(-1, false);
                Action::None
            }
            (KeyCode::Down, _) => {
                self.scroll_or_move(1, false);
                Action::None
            }
            (KeyCode::PageUp, _) => {
                self.scroll_or_move(-1, true);
                Action::None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_or_move(1, true);
                Action::None
            }
            (KeyCode::Home, _) => {
                self.jump(false);
                Action::None
            }
            (KeyCode::End, _) => {
                self.jump(true);
                Action::None
            }
            (KeyCode::Backspace, _) => {
                if self.query.pop().is_some() {
                    self.query_changed()
                } else {
                    Action::None
                }
            }
            (KeyCode::Char(c), false) => {
                self.query.push(c);
                self.query_changed()
            }
            _ => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;

    fn app(query: &str) -> App {
        App::new(Arc::new(fake_catalog()), query)
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn session_ids(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                Row::Session { idx, .. } => Some(app.catalog.sessions[*idx].meta.id.clone()),
                Row::Divider => None,
            })
            .collect()
    }

    #[test]
    fn initial_rows_by_last_activity() {
        let a = app("");
        assert_eq!(session_ids(&a), vec!["a", "b", "c", "d"]);
        assert_eq!(a.selected_session(), Some(0));
    }

    #[test]
    fn typing_filters_and_requests_search() {
        let mut a = app("");
        assert_eq!(
            a.handle_key(key(KeyCode::Char('k'))),
            Action::Search("k".into())
        );
        a.handle_key(key(KeyCode::Char('u')));
        assert_eq!(
            a.handle_key(key(KeyCode::Char('b'))),
            Action::Search("kub".into())
        );
        assert_eq!(session_ids(&a)[0], "b");
        assert_eq!(
            a.handle_key(key(KeyCode::Backspace)),
            Action::Search("ku".into())
        );
    }

    #[test]
    fn text_hits_add_divider_section_for_current_generation_only() {
        let mut a = app("pineapple");
        assert!(session_ids(&a).is_empty());
        let all: Vec<usize> = (0..a.catalog.sessions.len()).collect();
        let hits = crate::search::full_text(&a.catalog, &all, "pineapple", None).unwrap();
        a.set_text_generation(7);
        a.apply_text_hits(6, hits.clone());
        assert!(a.rows.is_empty());
        a.apply_text_hits(7, hits);
        assert_eq!(a.rows[0], Row::Divider);
        assert!(
            matches!(&a.rows[1], Row::Session { idx: 1, snippet: Some(s) } if s.contains("PINEAPPLE"))
        );
        assert_eq!(a.selected, 1);
        assert_eq!(a.text_hit(1).unwrap().message_index, 0);
    }

    #[test]
    fn enter_launches_with_default_source() {
        let mut a = app("");
        match a.handle_key(key(KeyCode::Enter)) {
            Action::Launch(plan) => assert_eq!(plan.argv, vec!["fake", "one", "--resume", "a"]),
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_a_cycles_source() {
        let mut a = app("");
        a.handle_key(ctrl('a'));
        assert_eq!(a.status.as_deref(), Some("resume via two"));
        match a.handle_key(key(KeyCode::Enter)) {
            Action::Launch(plan) => assert_eq!(plan.argv[1], "two"),
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_a_single_source_explains() {
        let mut a = app("");
        a.selected = 3;
        a.handle_key(ctrl('a'));
        assert_eq!(
            a.status.as_deref(),
            Some("no other source can resume this session")
        );
    }

    #[test]
    fn running_session_is_never_resumed() {
        let mut a = app("");
        a.selected = 2;
        // Focusing is fine; resuming a second copy of a live session is not.
        assert!(!matches!(
            a.handle_key(key(KeyCode::Enter)),
            Action::Launch(_)
        ));
        assert_eq!(a.status.as_deref(), Some("running in two (pid 4242)"));
    }

    #[test]
    fn enter_on_a_running_session_focuses_its_terminal() {
        let mut a = app("");
        a.selected = 2; // session "c", running as pid 4242 via source "two"
        assert_eq!(
            a.handle_key(key(KeyCode::Enter)),
            Action::Focus {
                pid: 4242,
                source: 1,
                title: "Running thing".into()
            }
        );
        assert_eq!(a.status.as_deref(), Some("running in two (pid 4242)"));
    }

    #[test]
    fn missing_cwd_is_blocked() {
        let mut a = app("");
        a.selected = 3;
        assert_eq!(a.handle_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(
            a.status.as_deref(),
            Some("project dir no longer exists: /nonexistent/ccpick-test")
        );
    }

    fn foreign_app() -> App {
        let mut a = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        a.selected = 1; // session "w"
        a
    }

    #[test]
    fn enter_on_foreign_session_opens_resume_dialog() {
        let mut a = foreign_app();
        assert_eq!(a.handle_key(key(KeyCode::Enter)), Action::None);
        let d = a.dialog.clone().expect("dialog open");
        assert_eq!(
            d.command,
            r"Set-Location 'C:\Users\me\proj'; fake win:c1 --resume w"
        );
        assert!(d.dir_missing);
        assert_eq!(d.note, None);
    }

    #[test]
    fn dialog_keys_copy_close_and_capture_input() {
        let mut a = foreign_app();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert_eq!(a.query, "");
        let command = a.dialog.as_ref().unwrap().command.clone();
        assert_eq!(a.handle_key(key(KeyCode::Char('c'))), Action::Copy(command));
        a.set_copy_result(Ok("clip.exe"));
        assert_eq!(
            a.dialog.as_ref().unwrap().note.as_deref(),
            Some("copied (clip.exe)")
        );
        a.set_copy_result(Err("could not copy: nope".into()));
        assert_eq!(
            a.dialog.as_ref().unwrap().note.as_deref(),
            Some("could not copy: nope")
        );
        assert_eq!(a.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(a.dialog.is_none());
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.handle_key(ctrl('c')), Action::Quit);
    }

    #[test]
    fn native_session_in_wsl_host_still_launches() {
        let mut a = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        a.selected = 0; // session "n", whose cwd exists
        assert!(matches!(
            a.handle_key(key(KeyCode::Enter)),
            Action::Launch(_)
        ));
        assert!(a.dialog.is_none());
    }

    /// Like `fake_catalog_with_foreign`, but the Windows session has two Windows sources so
    /// Ctrl-A inside the dialog has something to cycle between.
    fn foreign_two_source_app() -> App {
        use crate::env::{Env, HostContext};
        use crate::model::Role;
        use crate::providers::fake::FakeProvider;
        use std::path::PathBuf;

        let ubuntu = Env::Wsl {
            distro: "Ubuntu".into(),
        };
        let mut p = FakeProvider::default();
        p.add_source_in("one", "/s", ubuntu.clone(), "/fake");
        p.add_source_in("win:c1", "/w", Env::Windows, r"C:\Users\me");
        p.add_source_in("win:c2", "/w", Env::Windows, r"C:\Users\me");
        p.add_session(
            "/s",
            "n",
            "Native session",
            2000,
            2000,
            &[(Role::User, "hello")],
        );
        p.add_session(
            "/w",
            "w",
            "Windows session",
            1000,
            1000,
            &[(Role::User, "from windows")],
        );
        p.set_cwd("/w", "w", Some(r"C:\Users\me\proj"));
        let catalog = crate::catalog::build_fake_on(
            p,
            HostContext {
                env: ubuntu,
                wsl_mount_root: PathBuf::from("/nonexistent-root/"),
            },
        );
        let mut a = App::new(Arc::new(catalog), "");
        a.selected = 1; // session "w"
        a
    }

    #[test]
    fn dialog_ctrl_a_cycles_source_and_notes_it_without_touching_status() {
        let mut a = foreign_two_source_app();
        a.status = Some("untouched".into());
        a.handle_key(key(KeyCode::Enter));
        let before_status = a.status.clone();
        assert_eq!(a.handle_key(ctrl('a')), Action::None);
        let d = a.dialog.as_ref().expect("dialog still open");
        assert!(d.command.contains("win:c2"));
        assert_eq!(d.note.as_deref(), Some("resume via win:c2"));
        assert_eq!(a.status, before_status);
    }

    #[test]
    fn dialog_ctrl_a_single_source_notes_it_without_touching_status() {
        let mut a = foreign_app();
        a.status = Some("untouched".into());
        a.handle_key(key(KeyCode::Enter));
        let command_before = a.dialog.as_ref().unwrap().command.clone();
        assert_eq!(a.handle_key(ctrl('a')), Action::None);
        let d = a.dialog.as_ref().expect("dialog still open");
        assert_eq!(d.command, command_before);
        assert_eq!(
            d.note.as_deref(),
            Some("no other source can resume this session")
        );
        assert_eq!(a.status.as_deref(), Some("untouched"));
    }

    #[test]
    fn running_only_and_sort_toggles() {
        let mut a = app("");
        a.handle_key(ctrl('r'));
        assert_eq!(session_ids(&a), vec!["c"]);
        a.handle_key(ctrl('r'));
        a.handle_key(ctrl('s'));
        assert_eq!(a.sort, SortMode::Created);
        assert_eq!(session_ids(&a)[0], "d");
    }

    #[test]
    fn navigation_focus_and_quit() {
        let mut a = app("");
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.selected, 1);
        a.handle_key(key(KeyCode::Up));
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.selected, 0);
        a.handle_key(key(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Preview);
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.selected, 0);
        // Esc asks first; Ctrl-C doesn't.
        assert_eq!(a.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(a.confirm_quit);
        assert_eq!(a.handle_key(ctrl('c')), Action::Quit);
    }

    #[test]
    fn arrows_switch_panes() {
        let mut a = app("");
        a.handle_key(key(KeyCode::Right));
        assert_eq!(a.focus, Focus::Preview);
        a.handle_key(key(KeyCode::Right));
        assert_eq!(a.focus, Focus::Preview);
        a.handle_key(key(KeyCode::Left));
        assert_eq!(a.focus, Focus::List);
        a.handle_key(key(KeyCode::Left));
        assert_eq!(a.focus, Focus::List);
    }

    #[test]
    fn preview_scrolls_by_line_page_and_ends() {
        let mut a = app("");
        a.handle_key(key(KeyCode::Right));
        a.preview_metrics = Some(PreviewMetrics {
            total: 100,
            page: 10,
            anchor: 90,
        });
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.preview_scroll, Some(89));
        a.handle_key(key(KeyCode::PageUp));
        assert_eq!(a.preview_scroll, Some(79));
        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.preview_scroll, Some(0));
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.preview_scroll, Some(0));
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.preview_scroll, Some(90));
        a.handle_key(key(KeyCode::PageDown));
        assert_eq!(a.preview_scroll, Some(90));
        // Selection in the list is untouched while the preview has focus.
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn preview_scroll_without_metrics_is_a_no_op() {
        let mut a = app("");
        a.handle_key(key(KeyCode::Right));
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.preview_scroll, None);
    }

    #[test]
    fn preview_position_follows_anchor_until_scrolled_and_clamps() {
        let mut a = app("");
        let m = PreviewMetrics {
            total: 50,
            page: 20,
            anchor: 30,
        };
        assert_eq!(a.preview_position(m), 30);
        a.preview_scroll = Some(45);
        assert_eq!(a.preview_position(m), 30);
        a.preview_scroll = Some(3);
        assert_eq!(a.preview_position(m), 3);
    }

    #[test]
    fn changing_selection_or_query_resets_preview_scroll() {
        let mut a = app("");
        a.preview_scroll = Some(5);
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.preview_scroll, None);
        a.preview_scroll = Some(5);
        a.handle_key(key(KeyCode::Char('x')));
        assert_eq!(a.preview_scroll, None);
    }

    #[test]
    fn home_and_end_move_list_selection() {
        let mut a = app("");
        a.handle_key(key(KeyCode::End));
        assert_eq!(a.selected, 3);
        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn esc_asks_before_quitting() {
        let mut a = app("");
        assert_eq!(a.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(a.confirm_quit);
        assert_eq!(a.handle_key(key(KeyCode::Char('y'))), Action::Quit);

        let mut a = app("");
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.handle_key(key(KeyCode::Enter)), Action::Quit);
    }

    #[test]
    fn anything_else_cancels_the_quit_prompt_without_acting() {
        let mut a = app("");
        a.handle_key(key(KeyCode::Esc));
        // Enter is the one key that could resume a session by accident, so the cancelling key
        // must not fall through to the normal handler.
        assert_eq!(a.handle_key(key(KeyCode::Char('n'))), Action::None);
        assert!(!a.confirm_quit);
        assert_eq!(
            a.query, "",
            "the cancelling key is not typed into the query"
        );

        let mut a = app("");
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(!a.confirm_quit);
    }

    #[test]
    fn the_quit_prompt_takes_priority_over_the_resume_dialog() {
        let mut a = foreign_app();
        a.handle_key(key(KeyCode::Enter));
        assert!(a.dialog.is_some());
        a.confirm_quit = true;
        assert_eq!(a.handle_key(key(KeyCode::Char('c'))), Action::None);
        assert!(!a.confirm_quit);
        assert!(a.dialog.is_some(), "the dialog is left alone");
    }

    #[test]
    fn cwd_missing_is_cached_per_session_and_source() {
        use crate::providers::fake::FakeProvider;
        let tmp = tempfile::tempdir().unwrap();
        let will_appear = tmp.path().join("will-appear");
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        p.add_session("/s", "a", "T", 1, 1, &[]);
        p.set_cwd("/s", "a", Some(will_appear.to_str().unwrap()));
        let mut a = App::new(Arc::new(crate::catalog::build_fake(p)), "");
        assert!(a.cwd_missing(0, 0), "doesn't exist yet");
        std::fs::create_dir_all(&will_appear).unwrap();
        assert!(
            a.cwd_missing(0, 0),
            "still cached as missing even though it now exists"
        );
    }

    #[test]
    fn foreign_session_with_untranslatable_cwd_is_not_marked_missing() {
        // A source whose env has no path-translation mapping from the host (unlike Windows <->
        // WSL, which do translate) means `host_cwd` returns None even though the session has a
        // recorded cwd; that's "unknown", not "missing", so it must not be flagged.
        use crate::env::Env;
        use crate::providers::fake::FakeProvider;
        let mut p = FakeProvider::default();
        p.add_source_in("mac", "/s", Env::MacOs, "/Users/me");
        p.add_session("/s", "a", "T", 1, 1, &[]);
        let a = App::new(Arc::new(crate::catalog::build_fake(p)), "");
        assert_eq!(a.catalog.host_cwd(0, 0), None);
        assert!(a.catalog.sessions[0].meta.cwd.is_some());
        let mut a = a;
        assert!(!a.cwd_missing(0, 0));
    }

    #[test]
    fn preview_messages_load_for_selection() {
        let mut a = app("");
        assert_eq!(a.preview_messages().len(), 2);
        a.handle_key(key(KeyCode::Down));
        assert_eq!(
            a.preview_messages()[0].text,
            "the ingress needs a PINEAPPLE annotation"
        );
    }
}
