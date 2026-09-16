//! Drawing the TUI from App state.
use super::app::{App, Row};
use crate::format::{relative, shorten_home};
use crate::model::Role;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap};
use std::path::Path;

const HELP: &str = " ↵ resume  ^A source  ^R running only  ^S sort  tab preview  esc quit";
/// Header lines drawn above the message list in the preview pane (cwd/branch,
/// msg count/id, separator).
const PREVIEW_HEADER_LINES: usize = 3;

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

    let cwd = session
        .meta
        .cwd
        .as_deref()
        .map(|p| shorten_home(p, home))
        .unwrap_or_else(|| "?".into());
    let mut lines = vec![
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
        Line::from("─".repeat(area.width.saturating_sub(2) as usize)).dim(),
    ];

    if !messages.is_empty() {
        // Inner content width: pane width minus the 1-column padding on each side.
        let content_width = area.width.saturating_sub(2) as usize;
        // Rows available for messages: pane height minus the header lines above.
        let available = (area.height as usize).saturating_sub(PREVIEW_HEADER_LINES);
        let heights: Vec<usize> = messages
            .iter()
            .map(|m| message_height(&m.text, content_width))
            .collect();
        let last = messages.len() - 1;
        // Default anchor keeps the newest message visible by walking backwards
        // from the end; a text-search hit anchors at the hit instead.
        let anchor = hit_index
            .unwrap_or_else(|| tail_start(&heights, available))
            .min(last) as isize;
        let start = (anchor + offset).clamp(0, last as isize) as usize;
        // Bound how many messages we build lines for, so an early hit in a long
        // conversation doesn't render every message up to the end each frame.
        let end = forward_end(&heights, start, available);

        let label = format!("{}: ", session.meta.agent);
        for message in &messages[start..end] {
            let (name, color) = match message.role {
                Role::User => ("you: ".to_string(), Color::Cyan),
                Role::Assistant => (label.clone(), Color::Magenta),
            };
            for (i, text_line) in message.text.lines().enumerate() {
                let mut spans = Vec::new();
                if i == 0 {
                    spans.push(Span::styled(
                        name.clone(),
                        Style::new().fg(color).add_modifier(Modifier::BOLD),
                    ));
                }
                spans.extend(highlight(text_line, &query, Style::new()));
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().padding(Padding::horizontal(1)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Estimated wrapped row count of one message at `content_width`: each
/// source line wraps to `max(1, ceil(chars / content_width))` rows (a label
/// on the first line rides along in that row's width, so it adds no extra
/// row), plus one blank separator row rendered after the message.
fn message_height(text: &str, content_width: usize) -> usize {
    let width = content_width.max(1);
    let text_height: usize = text
        .lines()
        .map(|line| line.chars().count().div_ceil(width).max(1))
        .sum();
    text_height + 1
}

/// Index of the first message (walking backwards from the last one) whose
/// accumulated height still fits within `available` rows. Always includes
/// the last message, even if its height alone exceeds `available`.
fn tail_start(heights: &[usize], available: usize) -> usize {
    let Some(last) = heights.len().checked_sub(1) else {
        return 0;
    };
    let mut start = last;
    let mut total = heights[last];
    while start > 0 && total + heights[start - 1] <= available {
        start -= 1;
        total += heights[start];
    }
    start
}

/// Index one past the last message (walking forward from `start`) whose
/// accumulated height still fits within `available` rows. Always includes
/// the message at `start`, even if its height alone exceeds `available`;
/// bounds how many messages get built into lines per frame.
fn forward_end(heights: &[usize], start: usize, available: usize) -> usize {
    if start >= heights.len() {
        return start;
    }
    let mut end = start + 1;
    let mut total = heights[start];
    while end < heights.len() && total + heights[end] <= available {
        total += heights[end];
        end += 1;
    }
    end
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

    #[test]
    fn tail_start_all_fit() {
        let heights = vec![2, 2, 2];
        assert_eq!(tail_start(&heights, 10), 0);
    }

    #[test]
    fn tail_start_last_alone_exceeds() {
        let heights = vec![2, 2, 5];
        assert_eq!(tail_start(&heights, 3), 2);
    }

    #[test]
    fn tail_start_mixed() {
        let heights = vec![3, 4, 2, 5];
        // From the end: 5 (total 5), + 2 (total 7, fits in 8), then + 4 would be 11 (doesn't fit).
        assert_eq!(tail_start(&heights, 8), 2);
    }

    #[test]
    fn forward_end_includes_as_much_as_fits() {
        let heights = vec![3, 4, 2, 5];
        assert_eq!(forward_end(&heights, 0, 100), 4);
        assert_eq!(forward_end(&heights, 0, 5), 1);
        assert_eq!(forward_end(&heights, 2, 10), 4);
    }

    #[test]
    fn message_height_wraps_and_pads() {
        assert_eq!(message_height("hello", 10), 2); // 1 wrapped row + 1 blank separator
        assert_eq!(message_height("", 10), 1); // no content rows, just the separator
        assert_eq!(message_height(&"x".repeat(25), 10), 4); // ceil(25/10) = 3, + 1 separator
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
}
