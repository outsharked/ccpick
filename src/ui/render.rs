//! Drawing the TUI from App state.
use super::app::{App, Focus, PreviewMetrics, Row};
use crate::format::{exact, friendly, shorten_home};
use crate::model::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use std::path::Path;

const LIST_HELP: &str = " ↵ resume  → preview  ^A source  ^R running only  ^S sort  esc quit";
const PREVIEW_HELP: &str = " ← sessions  ↑↓ line  PgUp/PgDn page  Home/End  ↵ resume  esc quit";

pub fn highlight(text: &str, query: &str, base: Style) -> Vec<Span<'static>> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let lower = text.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(found) = lower[pos..].find(&needle) {
        let start = pos + found;
        let end = start + needle.len();
        if start > pos {
            spans.push(Span::styled(text[pos..start].to_string(), base));
        }
        spans.push(Span::styled(
            text[start..end].to_string(),
            base.fg(Color::Black).bg(Color::Yellow),
        ));
        pos = end;
    }
    if pos < text.len() {
        spans.push(Span::styled(text[pos..].to_string(), base));
    }
    spans
}

pub fn draw(frame: &mut Frame, app: &mut App, now_ms: i64, home: &Path) {
    let catalog = app.catalog.clone();
    let title = format!(
        " ccpick ─ {} sessions ─ {} sources ─ {} running ",
        catalog.sessions.len(),
        catalog.sources.len(),
        catalog.running_count()
    );
    let outer = Block::default().borders(Borders::ALL).title(title);
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let [query_area, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::new().fg(Color::Cyan)),
            Span::raw(app.query.clone()),
            Span::raw("▏"),
        ])),
        query_area,
    );

    let [list_area, preview_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
    draw_list(frame, app, list_area, now_ms, home);
    draw_preview(frame, app, preview_area, home);

    let footer_line = match &app.status {
        Some(status) => Line::from(Span::styled(status.clone(), Style::new().fg(Color::Yellow))),
        None => Line::from(match app.focus {
            Focus::List => LIST_HELP,
            Focus::Preview => PREVIEW_HELP,
        })
        .dim(),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect, now_ms: i64, home: &Path) {
    let catalog = &app.catalog;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| match row {
            Row::Divider => ListItem::new(Line::from("── in conversation text ──").dim()),
            Row::Session { idx, snippet } => {
                let s = &catalog.sessions[*idx];
                let marker = if s.live.is_some() { "● " } else { "  " };
                let title = Line::from(vec![
                    Span::styled(marker, Style::new().fg(Color::Green)),
                    Span::styled(
                        s.meta.title.clone(),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                ]);
                let second = match snippet {
                    Some(text) => Line::from(highlight(
                        &format!("  {text}"),
                        &app.query,
                        Style::new().dim(),
                    )),
                    None => {
                        let cwd = s
                            .meta
                            .cwd
                            .as_deref()
                            .map(|p| shorten_home(p, home))
                            .unwrap_or_else(|| "?".into());
                        let mut detail = format!(
                            "  {cwd} · {} · {}",
                            catalog.sources[app.launch_source(*idx)].name,
                            friendly(now_ms, s.meta.last_ts)
                        );
                        if s.live.is_some() {
                            detail.push_str(" [running]");
                        }
                        Line::from(detail).dim()
                    }
                };
                let item = ListItem::new(vec![title, second]);
                let cwd_missing = s.meta.cwd.as_deref().is_none_or(|p| !p.is_dir());
                if cwd_missing {
                    item.style(Style::new().add_modifier(Modifier::DIM))
                } else {
                    item
                }
            }
        })
        .collect();
    let block = Block::default().borders(Borders::RIGHT);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [title_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(pane_title("Sessions", app.focus == Focus::List)),
        title_area,
    );

    let highlight = match app.focus {
        Focus::List => Style::new().add_modifier(Modifier::REVERSED),
        Focus::Preview => Style::new().bg(Color::DarkGray),
    };
    let list = List::new(items).highlight_style(highlight);
    let mut state =
        ListState::default().with_selected(app.selected_session().map(|_| app.selected));
    frame.render_stateful_widget(list, list_area, &mut state);
}

/// A pane title: bright when its pane has focus, dimmed otherwise.
fn pane_title(text: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    };
    Line::from(Span::styled(format!(" {text}"), style))
}

fn draw_preview(frame: &mut Frame, app: &mut App, area: Rect, home: &Path) {
    let focused = app.focus == Focus::Preview;
    let [title_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let Some(idx) = app.selected_session() else {
        app.preview_metrics = None;
        frame.render_widget(
            Paragraph::new(pane_title("Conversation", focused)),
            title_area,
        );
        frame.render_widget(
            Paragraph::new("no sessions match")
                .dim()
                .block(Block::default().padding(Padding::horizontal(1))),
            body,
        );
        return;
    };
    let catalog = app.catalog.clone();
    let session = &catalog.sessions[idx];
    let source_name = catalog.sources[app.launch_source(idx)].name.clone();

    // Header: its real rendered height (a long cwd can wrap), full pane width minus padding.
    let header_width = body.width.saturating_sub(2).max(1);
    let cwd = session
        .meta
        .cwd
        .as_deref()
        .map(|p| shorten_home(p, home))
        .unwrap_or_else(|| "?".into());
    let header_lines = vec![
        Line::from(format!(
            "{cwd} · {} · {source_name}",
            session.meta.branch.as_deref().unwrap_or("-")
        )),
        Line::from(format!(
            "{} msgs · started {} · last {}",
            session.meta.msg_count,
            exact(session.meta.first_ts),
            exact(session.meta.last_ts),
        ))
        .dim(),
        Line::from(session.meta.id.clone()).dim(),
        Line::from("─".repeat(header_width as usize)).dim(),
    ];
    let header_height = Paragraph::new(header_lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(header_width) as u16;
    let [header_area, message_area] = Layout::vertical([
        Constraint::Length(header_height.min(body.height)),
        Constraint::Min(0),
    ])
    .areas(body);
    frame.render_widget(
        Paragraph::new(header_lines)
            .block(Block::default().padding(Padding::horizontal(1)))
            .wrap(Wrap { trim: false }),
        header_area,
    );

    // Messages: text on the left, a one-column scrollbar on the right.
    let [text_area, scrollbar_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(message_area);
    let text_width = text_area.width.saturating_sub(2).max(1);
    let key = (idx, text_width, app.query.clone());
    if app.preview_layout.as_ref().map(|l| &l.key) != Some(&key) {
        let agent = session.meta.agent.clone();
        let messages = app.preview_messages().to_vec();
        let mut layout = PreviewLayout::build(&messages, &agent, &key.2, text_width);
        layout.key = key;
        app.preview_layout = Some(layout);
    }
    let hit_index = app.text_hit(idx).map(|h| h.message_index);
    let layout = app.preview_layout.as_ref().expect("just built");
    let page = text_area.height as usize;
    let max = layout.total.saturating_sub(page);
    let anchor = match hit_index {
        Some(hit) => layout.starts.get(hit).copied().unwrap_or(max).min(max),
        None => max,
    };
    let metrics = PreviewMetrics {
        total: layout.total,
        page,
        anchor,
    };
    let overflows = layout.total > page;
    app.preview_metrics = Some(metrics);
    let pos = app.preview_position(metrics);

    let mut title = pane_title("Conversation", focused);
    if overflows {
        let percent = (pos * 100).checked_div(max).unwrap_or(100);
        title.push_span(Span::raw(format!(" · {percent}%")).dim());
    }
    frame.render_widget(Paragraph::new(title), title_area);

    let layout = app.preview_layout.as_ref().expect("built above");
    if layout.total == 0 {
        return;
    }
    let (lines, skip) = layout.visible(pos, page);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().padding(Padding::horizontal(1)))
            .wrap(Wrap { trim: false })
            .scroll((skip.min(u16::MAX as usize) as u16, 0)),
        text_area,
    );
    if overflows {
        let mut state = ScrollbarState::new(max + 1)
            .position(pos)
            .viewport_content_length(page);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollbar_area,
            &mut state,
        );
    }
}

/// A session's conversation rendered and measured once for a given width and query, so
/// scrolling only has to slice it.
pub struct PreviewLayout {
    /// (session index, text width, query) this layout was built for.
    pub(crate) key: (usize, u16, String),
    lines: Vec<Vec<Line<'static>>>,
    /// Rendered (wrapped) row count of each message, including its blank separator.
    pub(crate) heights: Vec<usize>,
    /// First row of each message.
    pub(crate) starts: Vec<usize>,
    pub(crate) total: usize,
}

impl PreviewLayout {
    fn build(messages: &[Message], agent: &str, query: &str, width: u16) -> PreviewLayout {
        let lines: Vec<Vec<Line<'static>>> = messages
            .iter()
            .map(|m| message_lines(agent, query, m))
            .collect();
        let heights: Vec<usize> = lines
            .iter()
            .map(|l| {
                Paragraph::new(l.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(width)
            })
            .collect();
        let mut starts = Vec::with_capacity(heights.len());
        let mut total = 0;
        for h in &heights {
            starts.push(total);
            total += h;
        }
        PreviewLayout {
            key: (usize::MAX, width, query.to_string()),
            lines,
            heights,
            starts,
            total,
        }
    }

    /// Lines of the messages covering rows `pos..pos + page`, plus how many rows of the first
    /// of them to scroll past.
    fn visible(&self, pos: usize, page: usize) -> (Vec<Line<'static>>, usize) {
        if self.lines.is_empty() {
            return (Vec::new(), 0);
        }
        let last = self.lines.len() - 1;
        let first = self
            .starts
            .iter()
            .zip(&self.heights)
            .position(|(start, height)| start + height > pos)
            .unwrap_or(last);
        let skip = pos.saturating_sub(self.starts[first]);
        let mut lines = Vec::new();
        let mut covered = 0;
        for i in first..self.lines.len() {
            lines.extend(self.lines[i].iter().cloned());
            covered += self.heights[i];
            if covered >= skip + page {
                break;
            }
        }
        (lines, skip.min(self.heights[first].saturating_sub(1)))
    }
}

/// Rendered lines for one message: a bold role label riding along the first
/// line, then the rest of the message text, then one blank separator line.
fn message_lines(agent: &str, query: &str, message: &Message) -> Vec<Line<'static>> {
    let (name, color) = match message.role {
        Role::User => ("you: ".to_string(), Color::Cyan),
        Role::Assistant => (format!("{agent}: "), Color::Magenta),
    };
    let mut lines = Vec::new();
    for (i, text_line) in message.text.lines().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            spans.push(Span::styled(
                name.clone(),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        spans.extend(highlight(text_line, query, Style::new()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| draw(f, app, 10_000, Path::new("/home/x")))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_header_rows_and_preview() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        let text = screen(&mut app);
        assert!(text.contains("4 sessions"));
        assert!(text.contains("1 running"));
        assert!(text.contains("Docker build cache"));
        assert!(text.contains("fix docker"));
        assert!(text.contains("resume"));
    }

    #[test]
    fn renders_status_instead_of_help() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        app.status = Some("hello status".into());
        let text = screen(&mut app);
        assert!(text.contains("hello status"));
    }

    #[test]
    fn highlight_splits_matches() {
        let spans = highlight("Fix DOCKER now", "docker", Style::new());
        let parts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, vec!["Fix ", "DOCKER", " now"]);
        assert_eq!(highlight("abc", "", Style::new()).len(), 1);
    }

    fn msg(text: &str) -> Message {
        Message {
            role: Role::User,
            text: text.into(),
            ts: None,
        }
    }

    #[test]
    fn message_lines_labels_first_line_only() {
        let m = Message {
            role: Role::Assistant,
            text: "line one\nline two".into(),
            ts: None,
        };
        let lines = message_lines("claude", "", &m);
        // 2 content lines + 1 trailing blank separator; no extra row for the label.
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn layout_measures_each_message_and_offsets() {
        let messages = vec![msg("a"), msg("b\nc"), msg("d")];
        let layout = PreviewLayout::build(&messages, "claude", "", 50);
        // 1 content row + 1 blank separator; the second message has 2 content rows.
        assert_eq!(layout.heights, vec![2, 3, 2]);
        assert_eq!(layout.starts, vec![0, 2, 5]);
        assert_eq!(layout.total, 7);
    }

    #[test]
    fn visible_lines_start_mid_message() {
        let messages = vec![msg("a"), msg("b\nc"), msg("d")];
        let layout = PreviewLayout::build(&messages, "claude", "", 50);
        // Row 3 is the second row of message 1; a 3-row page needs messages 1 and 2.
        let (lines, skip) = layout.visible(3, 3);
        assert_eq!(skip, 1);
        assert_eq!(lines.len(), 5);
        // Past the end still returns the last message rather than nothing.
        let (lines, _) = layout.visible(99, 3);
        assert_eq!(lines.len(), 2);
    }

    /// (x, y) of the first cell where `needle` starts on screen.
    fn find(buffer: &ratatui::buffer::Buffer, needle: &str) -> (u16, u16) {
        let width = buffer.area.width;
        for y in 0..buffer.area.height {
            let row: Vec<&str> = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            let chars: Vec<&str> = needle.split("").filter(|c| !c.is_empty()).collect();
            if let Some(x) = row.windows(chars.len()).position(|w| w == chars.as_slice()) {
                return (x as u16, y);
            }
        }
        panic!("{needle:?} not on screen");
    }

    fn draw_to(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| draw(f, app, 10_000, Path::new("/home/x")))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn selected_row_is_reversed_when_list_focused_and_gray_otherwise() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        let buffer = draw_to(&mut app, 100, 30);
        let cell = &buffer[find(&buffer, "Docker build cache")];
        assert!(cell.modifier.contains(Modifier::REVERSED));

        app.focus = super::super::app::Focus::Preview;
        let buffer = draw_to(&mut app, 100, 30);
        let cell = &buffer[find(&buffer, "Docker build cache")];
        assert!(!cell.modifier.contains(Modifier::REVERSED));
        assert_eq!(cell.bg, Color::DarkGray);
    }

    #[test]
    fn focused_pane_title_is_highlighted() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        let buffer = draw_to(&mut app, 100, 30);
        assert_eq!(buffer[find(&buffer, "Sessions")].fg, Color::Cyan);
        assert!(
            buffer[find(&buffer, "Conversation")]
                .modifier
                .contains(Modifier::DIM)
        );

        app.focus = super::super::app::Focus::Preview;
        let buffer = draw_to(&mut app, 100, 30);
        assert_eq!(buffer[find(&buffer, "Conversation")].fg, Color::Cyan);
        assert!(
            buffer[find(&buffer, "Sessions")]
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn scrollbar_and_position_shown_only_when_conversation_overflows() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        let text: String = draw_to(&mut app, 100, 30)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains('▼'));
        assert!(!text.contains('%'));

        let mut app = App::new(Arc::new(catalog_with_long_tail()), "");
        let text: String = draw_to(&mut app, 100, 20)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains('▼'));
        assert!(text.contains("100%"));
    }

    #[test]
    fn renderer_reports_metrics_and_scrolling_to_top_hides_the_tail() {
        let mut app = App::new(Arc::new(catalog_with_long_tail()), "");
        draw_to(&mut app, 100, 20);
        let metrics = app.preview_metrics.expect("metrics reported");
        assert!(metrics.total > metrics.page);
        assert_eq!(metrics.anchor, metrics.max_scroll());

        app.preview_scroll = Some(0);
        let text: String = draw_to(&mut app, 100, 20)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains("LAST-MESSAGE-MARKER"));
        assert!(text.contains("0%"));
    }

    fn catalog_with_long_tail() -> crate::catalog::Catalog {
        use crate::cache::Cache;
        use crate::catalog::Catalog;
        use crate::config::Settings;
        use crate::model::Role as MsgRole;
        use crate::providers::fake::FakeProvider;

        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        let long = "x".repeat(400);
        let messages = [
            (MsgRole::User, long.as_str()),
            (MsgRole::Assistant, long.as_str()),
            (MsgRole::User, long.as_str()),
            (MsgRole::Assistant, long.as_str()),
            (MsgRole::User, "LAST-MESSAGE-MARKER"),
        ];
        p.add_session("/s", "a", "Long convo", 1000, 1000, &messages);
        Catalog::build(
            vec![Box::new(p)],
            &Settings::default(),
            &mut Cache::in_memory(),
        )
        .unwrap()
    }

    #[test]
    fn preview_keeps_newest_message_visible() {
        let mut app = App::new(Arc::new(catalog_with_long_tail()), "");
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|f| draw(f, &mut app, 10_000, Path::new("/home/x")))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("LAST-MESSAGE-MARKER"));
    }

    fn catalog_with_wrapped_last_message() -> crate::catalog::Catalog {
        use crate::cache::Cache;
        use crate::catalog::Catalog;
        use crate::config::Settings;
        use crate::model::Role as MsgRole;
        use crate::providers::fake::FakeProvider;

        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        // Several hundred chars of ordinary space-separated words: at a narrow
        // preview width this word-wraps across many more rows than a
        // char-count / pane-width estimate would predict (word boundaries
        // waste columns that a naive division doesn't account for).
        let prose = "the quick brown fox jumps over the lazy dog while a \
            second sentence keeps going with more ordinary words so that \
            the paragraph wraps across a good double digit number of rows \
            at a narrow preview pane width before finally ending right \
            here at WRAPPED-TAIL-MARKER";
        let messages = [
            (MsgRole::User, "short earlier message"),
            (MsgRole::Assistant, prose),
        ];
        p.add_session("/s", "a", "Wrapped convo", 1000, 1000, &messages);
        Catalog::build(
            vec![Box::new(p)],
            &Settings::default(),
            &mut Cache::in_memory(),
        )
        .unwrap()
    }

    #[test]
    fn preview_default_view_shows_end_of_wrapped_last_message() {
        let mut app = App::new(Arc::new(catalog_with_wrapped_last_message()), "");
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|f| draw(f, &mut app, 10_000, Path::new("/home/x")))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("WRAPPED-TAIL-MARKER"));
    }
}
