//! Drawing the TUI from App state.
use super::app::{App, Focus, PreviewMetrics, Row};
use crate::format::{exact, friendly, shorten_home_in};
use crate::model::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};

const LIST_HELP: &str = " ↵ resume  → preview  ^A source  ^R running only  ^S sort  esc quit";
const PREVIEW_HELP: &str = " ← sessions  ↑↓ line  PgUp/PgDn page  Home/End  ↵ resume  esc quit";

pub fn highlight_spans(text: &str, query: &str, base: Style) -> Vec<Span<'static>> {
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

pub fn draw(frame: &mut Frame, app: &mut App, now_ms: i64) {
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
    draw_list(frame, app, list_area, now_ms);
    draw_preview(frame, app, preview_area);

    let footer_line = match &app.status {
        Some(status) => Line::from(Span::styled(status.clone(), Style::new().fg(Color::Yellow))),
        None => Line::from(match app.focus {
            Focus::List => LIST_HELP,
            Focus::Preview => PREVIEW_HELP,
        })
        .dim(),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    if app.dialog.is_some() {
        draw_dialog(frame, app);
    }
    if app.confirm_quit {
        draw_quit_confirm(frame);
    }
}

fn draw_quit_confirm(frame: &mut Frame) {
    draw_modal(
        frame,
        " Quit ccpick ",
        vec![
            Line::from("Quit ccpick?"),
            Line::from(""),
            Line::from(" y / ↵ quit   any other key cancel").dim(),
        ],
        40,
    );
}

/// A centered, bordered box over the whole frame, sized to its wrapped contents.
fn draw_modal(frame: &mut Frame, title: &str, lines: Vec<Line<'static>>, max_width: u16) {
    let area = frame.area();
    let width = area
        .width
        .saturating_sub(4)
        .clamp(20, max_width)
        .min(area.width);
    let inner_width = width.saturating_sub(4).max(1);
    let content_height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner_width) as u16;
    let height = content_height.saturating_add(2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string())
                .padding(Padding::horizontal(1)),
        ),
        rect,
    );
}

fn draw_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.dialog else {
        return;
    };
    let source = &app.catalog.sources[dialog.source];
    let mut lines = vec![
        Line::from(format!(
            "This session lives in {}. Paste into {}:",
            source.env.display_name(),
            source.env.shell_name()
        )),
        Line::from(""),
        Line::from(Span::styled(
            dialog.command.clone(),
            Style::new().fg(Color::Cyan),
        )),
    ];
    if dialog.dir_missing {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Note: the project directory no longer exists.",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));
    if let Some(note) = &dialog.note {
        lines.push(Line::from(Span::styled(
            note.clone(),
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(" c copy   ^A source   esc close").dim());
    draw_modal(
        frame,
        &format!(
            " Resume in {} · {} ",
            source.env.display_name(),
            source.name
        ),
        lines,
        90,
    );
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect, now_ms: i64) {
    let catalog = app.catalog.clone();
    let rows = app.rows.clone();
    let block = Block::default().borders(Borders::RIGHT);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [title_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(pane_title("Sessions", app.focus == Focus::List)),
        title_area,
    );

    // The selection is drawn onto the row's own spans rather than by the list's highlight
    // style, so it starts after the running marker and still runs to the pane's edge.
    let highlight = match app.focus {
        Focus::List => Style::new().add_modifier(Modifier::REVERSED),
        Focus::Preview => Style::new().bg(Color::DarkGray),
    };
    let selected_row = app.selected_session().map(|_| app.selected);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| match row {
            Row::Divider => ListItem::new(Line::from("── in conversation text ──").dim()),
            Row::Session { idx, snippet } => {
                let idx = *idx;
                let s = &catalog.sessions[idx];
                let source_idx = app.launch_source(idx);
                let source = &catalog.sources[source_idx];
                let selected = Some(row_idx) == selected_row;
                let sel = |style: Style| {
                    if selected {
                        style.patch(highlight)
                    } else {
                        style
                    }
                };
                // Fills the rest of the row so the selection is a solid band, without
                // touching the two marker columns in front of it.
                let fill = |mut line: Line<'static>| {
                    if selected {
                        let pad = (list_area.width as usize).saturating_sub(line.width());
                        line.push_span(Span::styled(" ".repeat(pad), highlight));
                    }
                    line
                };
                let marker = if s.live.is_some() { "● " } else { "  " };
                let title = Line::from(vec![
                    Span::styled(marker, Style::new().fg(Color::Green)),
                    Span::styled(
                        s.meta.title.clone(),
                        sel(Style::new().add_modifier(Modifier::BOLD)),
                    ),
                ]);
                // The second line is indented to sit under the title, past the marker.
                let mut spans = vec![Span::raw("  ")];
                match snippet {
                    Some(text) => spans.extend(
                        highlight_spans(text, &app.query, Style::new().dim())
                            .into_iter()
                            .map(|span| Span::styled(span.content, sel(span.style))),
                    ),
                    None => {
                        let cwd = s
                            .meta
                            .cwd
                            .as_deref()
                            .map(|p| shorten_home_in(p, &source.env_home, &source.env))
                            .unwrap_or_else(|| "?".into());
                        let mut detail = format!(
                            "{cwd} · {} · {}",
                            source.name,
                            friendly(now_ms, s.meta.last_ts)
                        );
                        if s.live.is_some() {
                            detail.push_str(" [running]");
                        }
                        spans.push(Span::styled(detail, sel(Style::new().dim())));
                    }
                }
                let item = ListItem::new(vec![fill(title), fill(Line::from(spans))]);
                if app.cwd_missing(idx, source_idx) {
                    item.style(Style::new().add_modifier(Modifier::DIM))
                } else {
                    item
                }
            }
        })
        .collect();
    let mut state =
        ListState::default().with_selected(app.selected_session().map(|_| app.selected));
    frame.render_stateful_widget(List::new(items), list_area, &mut state);
}

fn pane_title(text: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    };
    Line::from(Span::styled(format!(" {text}"), style))
}

fn draw_preview(frame: &mut Frame, app: &mut App, area: Rect) {
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
    let source = &catalog.sources[app.launch_source(idx)];
    let source_name = source.name.clone();

    // Header: its real rendered height (a long cwd can wrap), full pane width minus padding.
    let header_width = body.width.saturating_sub(2).max(1);
    let cwd = session
        .meta
        .cwd
        .as_deref()
        .map(|p| shorten_home_in(p, &source.env_home, &source.env))
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
        spans.extend(highlight_spans(text_line, query, Style::new()));
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
        terminal.draw(|f| draw(f, app, 10_000)).unwrap();
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
        let spans = highlight_spans("Fix DOCKER now", "docker", Style::new());
        let parts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, vec!["Fix ", "DOCKER", " now"]);
        assert_eq!(highlight_spans("abc", "", Style::new()).len(), 1);
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

    /// The whole screen as text, one line per row.
    fn dump(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw_to(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app, 10_000)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn foreign_rows_shorten_against_their_own_home() {
        let mut app = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        let text: String = draw_to(&mut app, 100, 30)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains(r"~\proj"));
        assert!(text.contains("win:c1"));
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
    fn the_selection_starts_after_the_running_marker_and_runs_to_the_edge() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        let buffer = draw_to(&mut app, 100, 30);
        let (x, y) = find(&buffer, "Docker build cache");
        assert!(buffer[(x, y)].modifier.contains(Modifier::REVERSED));
        // The running marker sits in the two columns before the title.
        assert!(!buffer[(x - 1, y)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(x - 2, y)].modifier.contains(Modifier::REVERSED));
        // The band runs on past the title to the pane's edge.
        let end = x + "Docker build cache".chars().count() as u16;
        assert!(buffer[(end, y)].modifier.contains(Modifier::REVERSED));
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
        crate::catalog::build_fake(p)
    }

    #[test]
    fn preview_keeps_newest_message_visible() {
        let mut app = App::new(Arc::new(catalog_with_long_tail()), "");
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app, 10_000)).unwrap();
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
        crate::catalog::build_fake(p)
    }

    #[test]
    fn list_dims_rows_whose_project_dir_is_missing() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        let buffer = draw_to(&mut app, 100, 30);
        // Session "d" ("Old notes") has cwd /nonexistent/ccpick-test; session "a" has one
        // that exists.
        assert!(
            buffer[find(&buffer, "Old notes")]
                .modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !buffer[find(&buffer, "Docker build cache")]
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn renders_the_quit_confirmation_over_everything_else() {
        let mut app = App::new(Arc::new(fake_catalog()), "");
        app.confirm_quit = true;
        let buffer = draw_to(&mut app, 100, 30);
        let text = dump(&buffer);
        assert!(text.contains("Quit ccpick?"));
        assert!(text.contains("y / ↵ quit"));
    }

    #[test]
    fn list_does_not_dim_a_session_whose_cwd_has_no_translation_mapping() {
        // A source in an environment the host can't translate to/from (unlike Windows <-> WSL)
        // means the cwd's existence is simply unknown, not missing, so it must not be dimmed.
        use crate::env::Env;
        use crate::providers::fake::FakeProvider;
        let mut p = FakeProvider::default();
        p.add_source_in("mac", "/s", Env::MacOs, "/Users/me");
        p.add_session("/s", "a", "Untranslatable cwd", 1, 1, &[]);
        let mut app = App::new(Arc::new(crate::catalog::build_fake(p)), "");
        let buffer = draw_to(&mut app, 100, 30);
        assert!(
            !buffer[find(&buffer, "Untranslatable cwd")]
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn renders_resume_dialog() {
        let mut app = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        app.selected = 1;
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let text: String = draw_to(&mut app, 100, 30)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Resume in Windows"));
        assert!(text.contains("Paste into PowerShell"));
        assert!(text.contains("Set-Location"));
        assert!(text.contains("no longer exists"));
        assert!(text.contains("c copy"));
    }

    #[test]
    fn dialog_note_keeps_the_hint_visible_on_its_own_line_above_it() {
        let mut app = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        app.selected = 1;
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        app.set_copy_result(Ok("clip.exe"));
        let text: String = draw_to(&mut app, 100, 30)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("copied (clip.exe)"));
        assert!(text.contains("c copy"));
        assert!(text.contains("esc close"));
    }

    #[test]
    fn preview_default_view_shows_end_of_wrapped_last_message() {
        let mut app = App::new(Arc::new(catalog_with_wrapped_last_message()), "");
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app, 10_000)).unwrap();
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

#[cfg(test)]
mod readme_shot {
    use super::*;
    use crate::model::{LaunchRecord, Role};
    use crate::providers::fake::FakeProvider;
    use crate::ui::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};
    use std::sync::Arc;

    fn hex(c: Color, fg: bool) -> String {
        match c {
            // Titles carry no colour of their own, so Reset is the terminal's plain
            // foreground: near-white, not grey.
            Color::Reset => if fg { "#ffffff" } else { "#16181d" }.into(),
            Color::Black => "#1b1d23".into(),
            Color::Red | Color::LightRed => "#e06c75".into(),
            Color::Green | Color::LightGreen => "#3fd07b".into(),
            Color::Yellow | Color::LightYellow => "#e5c07b".into(),
            Color::Blue | Color::LightBlue => "#61afef".into(),
            Color::Magenta | Color::LightMagenta => "#c586e0".into(),
            Color::Cyan | Color::LightCyan => "#4fc1d9".into(),
            Color::Gray => "#a9b1bd".into(),
            Color::DarkGray => "#7b8290".into(),
            Color::White => "#f3f4f6".into(),
            other => format!("{other:?}"),
        }
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// Regenerates the source HTML for `docs/screenshot.png`; see AGENTS.md.
    #[test]
    #[ignore = "run explicitly with CCPICK_SHOT_HTML set"]
    fn write_html() {
        const NOW: i64 = 1_758_000_000_000;
        const MIN: i64 = 60_000;
        const HOUR: i64 = 60 * MIN;
        const DAY: i64 = 24 * HOUR;
        let mut p = FakeProvider::default();
        p.add_source("work", "/s");
        p.add_source("personal", "/t");
        p.add_session(
            "/s",
            "a1",
            "Flaky login test on CI",
            NOW - 3 * HOUR,
            NOW - 12 * MIN,
            &[
                (Role::User, "the login test fails about one run in five"),
                (
                    Role::Assistant,
                    "It races the session cookie write: the assertion runs before the redirect \
lands, so the session is only there on a slow machine. Awaiting the redirect fixes it.",
                ),
            ],
        );
        let sessions: &[(&str, &str, &str, &str, i64, i64)] = &[
            (
                "/s",
                "b2",
                "Postgres connection pool sizing",
                "~/code/web-app",
                2 * DAY,
                26 * HOUR,
            ),
            (
                "/s",
                "c3",
                "Rate limiting the public API",
                "~/code/web-app",
                2 * DAY,
                2 * DAY,
            ),
            (
                "/t",
                "d4",
                "Blog post about the release",
                "~/notes",
                6 * DAY,
                3 * DAY,
            ),
            (
                "/s",
                "e5",
                "Terraform state migration",
                "~/code/infra",
                9 * DAY,
                4 * DAY,
            ),
            (
                "/s",
                "f6",
                "Flaky DNS in the staging cluster",
                "~/code/infra",
                9 * DAY,
                5 * DAY,
            ),
            (
                "/t",
                "g7",
                "Weekend photo import script",
                "~/code/scratch",
                11 * DAY,
                6 * DAY,
            ),
            (
                "/s",
                "h8",
                "Upgrade the build to the 2024 edition",
                "~/code/web-app",
                14 * DAY,
                8 * DAY,
            ),
            (
                "/s",
                "i9",
                "Cache invalidation on deploy",
                "~/code/web-app",
                16 * DAY,
                12 * DAY,
            ),
            (
                "/t",
                "j10",
                "Home server backup rotation",
                "~/code/scratch",
                20 * DAY,
                15 * DAY,
            ),
            (
                "/s",
                "k11",
                "Postmortem for the checkout outage",
                "~/notes",
                24 * DAY,
                20 * DAY,
            ),
            (
                "/s",
                "l12",
                "Split the monolith test suite",
                "~/code/web-app",
                30 * DAY,
                26 * DAY,
            ),
            (
                "/t",
                "m13",
                "Reading list cleanup",
                "~/notes",
                40 * DAY,
                33 * DAY,
            ),
        ];
        for (store, id, title, cwd, first, last) in sessions {
            p.add_session(store, id, title, NOW - first, NOW - last, &[]);
            p.set_cwd(store, id, Some(cwd));
        }
        p.set_cwd("/s", "a1", Some("~/code/web-app"));
        p.records.insert(
            "work".into(),
            vec![
                LaunchRecord {
                    pid: 48120,
                    session_id: "a1".into(),
                    started_at_ms: NOW - 3 * HOUR,
                    alive: true,
                },
                LaunchRecord {
                    pid: 48771,
                    session_id: "c3".into(),
                    started_at_ms: NOW - 2 * DAY,
                    alive: true,
                },
            ],
        );
        let mut catalog = crate::catalog::build_fake(p);
        // The preview labels assistant turns with the agent name; show the real one.
        for session in &mut catalog.sessions {
            session.meta.agent = "claude".into();
        }
        let mut app = App::new(Arc::new(catalog), "");
        let (w, h) = (186u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, &mut app, NOW)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let mut body = String::new();
        for y in 0..h {
            for x in 0..w {
                let cell = &buffer[(x, y)];
                let reversed = cell.modifier.contains(Modifier::REVERSED);
                let (mut fg, mut bg) = (hex(cell.fg, true), hex(cell.bg, false));
                if reversed {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let mut style = format!("color:{fg}");
                if bg != "#16181d" {
                    style.push_str(&format!(";background:{bg}"));
                }
                if cell.modifier.contains(Modifier::DIM) {
                    style.push_str(";opacity:.68");
                }
                if cell.modifier.contains(Modifier::BOLD) {
                    style.push_str(";font-weight:700");
                }
                body.push_str(&format!(
                    "<span style=\"{style}\">{}</span>",
                    esc(cell.symbol())
                ));
            }
            body.push('\n');
        }
        let html = format!(
            "<!doctype html><meta charset=utf-8><style>\
html,body{{margin:0;background:#16181d}}\
pre{{margin:0;padding:11px 12px;font:9px/1.32 'Cascadia Mono','Consolas',monospace;\
background:#16181d;color:#d6d6d6;display:inline-block;white-space:pre}}\
</style><pre>{body}</pre>"
        );
        let out =
            std::env::var("CCPICK_SHOT_HTML").unwrap_or_else(|_| "target/screenshot.html".into());
        std::fs::write(out, html).unwrap();
    }
}
