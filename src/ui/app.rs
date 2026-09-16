//! TUI state and key handling, independent of the terminal.
use crate::catalog::Catalog;
use crate::model::{LaunchPlan, Message};
use crate::search::{self, TextHit};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
}

pub struct App {
    pub catalog: Arc<Catalog>,
    pub query: String,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub focus: Focus,
    pub sort: SortMode,
    pub running_only: bool,
    /// Preview scroll in messages, relative to the default anchor.
    pub preview_offset: isize,
    pub status: Option<String>,
    source_override: HashMap<usize, usize>,
    text_hits: Vec<TextHit>,
    text_generation: u64,
    preview: Option<(usize, Vec<Message>)>,
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
            preview_offset: 0,
            status: None,
            source_override: HashMap::new(),
            text_hits: Vec::new(),
            text_generation: 0,
            preview: None,
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
            self.preview_offset = 0;
            self.status = None;
        }
    }

    fn scroll_or_move(&mut self, delta: isize) {
        match self.focus {
            Focus::List => self.move_selection(delta),
            Focus::Preview => self.preview_offset += delta,
        }
    }

    fn query_changed(&mut self) -> Action {
        self.text_hits.clear();
        self.preview_offset = 0;
        self.status = None;
        self.recompute_rows(false);
        Action::Search(self.query.clone())
    }

    fn cycle_source(&mut self) {
        let Some(idx) = self.selected_session() else {
            return;
        };
        let sources = &self.catalog.sessions[idx].sources;
        if sources.len() < 2 {
            self.status = Some("no other source can resume this session".into());
            return;
        }
        let current = self.launch_source(idx);
        let pos = sources.iter().position(|s| *s == current).unwrap_or(0);
        let next = sources[(pos + 1) % sources.len()];
        self.source_override.insert(idx, next);
        self.status = Some(format!("resume via {}", self.catalog.sources[next].name));
    }

    fn enter(&mut self) -> Action {
        let Some(idx) = self.selected_session() else {
            return Action::None;
        };
        let session = &self.catalog.sessions[idx];
        if let Some((pid, source)) = session.live {
            self.status = Some(format!(
                "running in {} (pid {pid})",
                self.catalog.sources[source].name
            ));
            return Action::None;
        }
        match &session.meta.cwd {
            Some(cwd) if cwd.is_dir() => {
                Action::Launch(self.catalog.launch_plan(idx, self.launch_source(idx)))
            }
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

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => Action::Quit,
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
            (KeyCode::Up, _) => {
                self.scroll_or_move(-1);
                Action::None
            }
            (KeyCode::Down, _) => {
                self.scroll_or_move(1);
                Action::None
            }
            (KeyCode::PageUp, _) => {
                self.scroll_or_move(-10);
                Action::None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_or_move(10);
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
    fn running_session_is_blocked() {
        let mut a = app("");
        a.selected = 2;
        assert_eq!(a.handle_key(key(KeyCode::Enter)), Action::None);
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
        assert_eq!(a.preview_offset, -1);
        assert_eq!(a.handle_key(key(KeyCode::Esc)), Action::Quit);
        assert_eq!(a.handle_key(ctrl('c')), Action::Quit);
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
