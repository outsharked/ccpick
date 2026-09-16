//! Drawing the TUI from App state.
use super::app::{App, Row};
use crate::format::{relative, shorten_home};
use crate::model::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap};
use std::path::Path;

const HELP: &str = " ↵ resume  ^A source  ^R running only  ^S sort  tab preview  esc quit";

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
    draw_preview(frame, app, preview_area, now_ms, home);

    let footer_line = match &app.status {
        Some(status) => Line::from(Span::styled(status.clone(), Style::new().fg(Color::Yellow))),
        None => Line::from(HELP).dim(),
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
                            relative(now_ms, s.meta.last_ts)
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
    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state =
        ListState::default().with_selected(app.selected_session().map(|_| app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_preview(frame: &mut Frame, app: &mut App, area: Rect, now_ms: i64, home: &Path) {
    let Some(idx) = app.selected_session() else {
        frame.render_widget(Paragraph::new("no sessions match").dim(), area);
        return;
    };
    let catalog = app.catalog.clone();
    let session = &catalog.sessions[idx];
    let source_name = catalog.sources[app.launch_source(idx)].name.clone();
    let hit_index = app.text_hit(idx).map(|h| h.message_index);
    let query = app.query.clone();
    let offset = app.preview_offset;
    let messages = app.preview_messages();

    // Inner content width: pane width minus the 1-column padding on each
    // side (shared by the header and message sub-areas below, since a
    // vertical split keeps the same width).
    let content_width = area.width.saturating_sub(2).max(1);
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
            "{} msgs · last {} · {}",
            session.meta.msg_count,
            relative(now_ms, session.meta.last_ts),
            session.meta.id
        ))
        .dim(),
        Line::from("─".repeat(content_width as usize)).dim(),
    ];

    // Measure the header's real rendered height (it can wrap too, e.g. a
    // long cwd) rather than assuming a fixed row count, then give the
    // message area whatever is left.
    let header_height = Paragraph::new(header_lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(content_width) as u16;
    let header_height = header_height.min(area.height);
    let [header_area, message_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(header_lines)
            .block(Block::default().padding(Padding::horizontal(1)))
            .wrap(Wrap { trim: false }),
        header_area,
    );

    if messages.is_empty() {
        return;
    }
    let available = message_area.height as usize;
    let last = messages.len() - 1;
    let agent = session.meta.agent.clone();

    // Default view is bottom-anchored (the end of the newest message is
    // visible): build a bounded window backward from the anchor message and
    // scroll it so the window's bottom sits at the pane's bottom. A
    // text-search hit anchors at the hit instead, top-aligned, so the match
    // is visible without needing a scroll offset.
    let (msg_lines, scroll) = if let Some(hit) = hit_index {
        let top_ref = (hit as isize + offset).clamp(0, last as isize) as usize;
        let (lines, ..) =
            extend_forward(messages, &agent, &query, top_ref, available, content_width);
        (lines, 0)
    } else {
        let bottom_ref = (last as isize + offset).clamp(0, last as isize) as usize;
        let (lines, _start, height) = extend_backward(
            messages,
            &agent,
            &query,
            bottom_ref,
            available,
            content_width,
        );
        (lines, height.saturating_sub(available))
    };

    frame.render_widget(
        Paragraph::new(msg_lines)
            .block(Block::default().padding(Padding::horizontal(1)))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        message_area,
    );
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

/// Builds lines for a window of messages ending at `end_inclusive`, walking
/// backward and measuring the *real* rendered height with
/// `Paragraph::line_count` (word-aware wrapping, not a char-count estimate)
/// after each message is prepended. Stops once the measured height reaches
/// `available` or there are no earlier messages, so a long conversation
/// never has more than `available`-worth of messages built per frame.
/// Always includes at least the message at `end_inclusive`, even if its
/// height alone exceeds `available`. Returns (lines, start index, height).
fn extend_backward(
    messages: &[Message],
    agent: &str,
    query: &str,
    end_inclusive: usize,
    available: usize,
    width: u16,
) -> (Vec<Line<'static>>, usize, usize) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut start = end_inclusive;
    loop {
        let mut prefix = message_lines(agent, query, &messages[start]);
        prefix.extend(lines);
        lines = prefix;
        let height = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        if height >= available || start == 0 {
            return (lines, start, height);
        }
        start -= 1;
    }
}

/// Builds lines for a window of messages starting at `start`, walking
/// forward and measuring the real rendered height the same way as
/// `extend_backward`. Always includes at least the message at `start`.
/// Returns (lines, end index exclusive, height).
fn extend_forward(
    messages: &[Message],
    agent: &str,
    query: &str,
    start: usize,
    available: usize,
    width: u16,
) -> (Vec<Line<'static>>, usize, usize) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut end = start;
    loop {
        lines.extend(message_lines(agent, query, &messages[end]));
        let height = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        end += 1;
        if height >= available || end >= messages.len() {
            return (lines, end, height);
        }
    }
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
    fn extend_backward_includes_all_when_they_fit() {
        let messages = vec![msg("a"), msg("b"), msg("c")];
        let (lines, start, height) = extend_backward(&messages, "assistant", "", 2, 10, 50);
        assert_eq!(start, 0);
        assert_eq!(height, 6); // 3 messages * (1 content row + 1 blank)
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn extend_backward_always_includes_the_anchor_even_if_it_overflows() {
        let messages = vec![msg("a"), msg("b"), msg("c")];
        let (_, start, height) = extend_backward(&messages, "assistant", "", 2, 1, 50);
        assert_eq!(start, 2);
        assert_eq!(height, 2);
    }

    #[test]
    fn extend_backward_stops_once_it_fills_available() {
        let messages = vec![msg("a"), msg("b"), msg("c")];
        // Anchor (2 rows) alone doesn't fill 3; one more message (4 rows)
        // does, so it stops there rather than pulling in the earliest too.
        let (_, start, height) = extend_backward(&messages, "assistant", "", 2, 3, 50);
        assert_eq!(start, 1);
        assert_eq!(height, 4);
    }

    #[test]
    fn extend_forward_includes_all_when_they_fit() {
        let messages = vec![msg("a"), msg("b"), msg("c")];
        let (lines, end, height) = extend_forward(&messages, "assistant", "", 0, 10, 50);
        assert_eq!(end, 3);
        assert_eq!(height, 6);
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn extend_forward_stops_once_it_fills_available() {
        let messages = vec![msg("a"), msg("b"), msg("c")];
        let (_, end, height) = extend_forward(&messages, "assistant", "", 0, 3, 50);
        assert_eq!(end, 2);
        assert_eq!(height, 4);
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
