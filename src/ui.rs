use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::{
    app::{App, Focus, RowKey},
    sessions::{Session, tilde},
    transcript::{Entry, truncate},
};

const TOOL_RESULT_LINES: usize = 4;
const ACCENT: Color = Color::LightMagenta;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Length(38), Constraint::Min(20)]).areas(main);

    app.pty_area = None;
    draw_sidebar(frame, app, left);
    match app.current() {
        Some(RowKey::Pty(id)) => draw_pty(frame, app, id, right),
        Some(RowKey::Session(id)) => draw_transcript(frame, app, &id, right),
        None => draw_empty(frame, app, right),
    }
    draw_footer(frame, app, footer);
    if let Some(input) = &app.input {
        draw_input(frame, input);
    }
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let color = if focused { ACCENT } else { Color::DarkGray };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(title, Style::default().bold()))
}

fn status_dot(session: Option<&Session>) -> Span<'static> {
    match session.and_then(|s| s.status.as_deref()) {
        Some("busy") => Span::styled("● ", Style::default().fg(Color::Yellow)),
        _ => Span::styled("● ", Style::default().fg(Color::Green)),
    }
}

fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(Color::DarkGray))
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    let width = area.width.saturating_sub(6) as usize;
    let items: Vec<ListItem> = rows
        .iter()
        .filter_map(|row| match row {
            RowKey::Pty(id) => {
                let pty = app.pty(*id)?;
                let session = app.pty_session(pty);
                let dot = if pty.exited {
                    dim("✕ ")
                } else {
                    status_dot(session)
                };
                let name = match session {
                    Some(s) => s.display_name().to_string(),
                    None if pty.exited => "exited".to_string(),
                    None => "starting…".to_string(),
                };
                Some(ListItem::new(vec![
                    Line::from(vec![
                        dot,
                        Span::raw(name).bold(),
                        Span::styled("  here", Style::default().fg(ACCENT)),
                    ]),
                    Line::from(dim(format!("  {}", truncate_left(&tilde(&pty.cwd), width)))),
                ]))
            }
            RowKey::Session(id) => {
                let s = app.session(id)?;
                let first = if s.alive {
                    vec![
                        status_dot(Some(s)),
                        Span::raw(s.display_name().to_string()).bold(),
                        dim(format!("  {}", s.pid)),
                    ]
                } else {
                    vec![
                        dim("○ "),
                        Span::raw(truncate(s.display_name(), width.saturating_sub(6))),
                        dim(format!("  {}", age(s.started_at))),
                    ]
                };
                Some(ListItem::new(vec![
                    Line::from(first),
                    Line::from(dim(format!("  {}", truncate_left(&tilde(&s.cwd), width)))),
                ]))
            }
        })
        .collect();

    let title = format!(" Instances ({}) ", rows.len());
    let list = List::new(items)
        .block(pane_block(title, app.focus == Focus::Sidebar))
        .highlight_style(Style::default().bg(Color::Rgb(50, 40, 60)))
        .highlight_symbol("▌");
    let mut state = ListState::default().with_selected((!rows.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_empty(frame: &mut Frame, app: &App, area: Rect) {
    let msg = Paragraph::new(
        "No Claude instances.\n\nPress `n` to start one here, or `a` to show past sessions.",
    )
    .block(pane_block(
        " Conversation ".into(),
        app.focus == Focus::Conversation,
    ))
    .fg(Color::DarkGray);
    frame.render_widget(msg, area);
}

fn draw_pty(frame: &mut Frame, app: &mut App, id: u64, area: Rect) {
    let focused = app.focus == Focus::Conversation;
    let name = app
        .pty(id)
        .and_then(|p| app.pty_session(p))
        .map(|s| s.display_name().to_string())
        .unwrap_or_else(|| "claude".into());
    let Some(pty) = app.ptys.iter_mut().find(|p| p.id == id) else {
        return;
    };

    let state = match (pty.exited, focused) {
        (true, _) => "exited — r resume · x close",
        (false, true) => "live — ctrl-] to leave",
        (false, false) => "live — enter to type",
    };
    let block = pane_block(format!(" {name} · {} · {state} ", tilde(&pty.cwd)), focused);
    let inner = block.inner(area);
    pty.resize(inner.height, inner.width);
    app.pty_area = Some(inner);

    let parser = pty.parser.lock().unwrap();
    let mut cursor = Cursor::default();
    if !focused || pty.exited || parser.screen().scrollback() > 0 {
        cursor = cursor.visibility(false);
    }
    frame.render_widget(
        PseudoTerminal::new(parser.screen())
            .block(block)
            .cursor(cursor),
        area,
    );
}

fn draw_transcript(frame: &mut Frame, app: &mut App, id: &str, area: Rect) {
    let focused = app.focus == Focus::Conversation;
    let Some(session) = app.session(id) else {
        return;
    };

    let hint = if session.alive {
        "read-only, running elsewhere"
    } else {
        "r to resume here"
    };
    let title = format!(
        " {} · {} · v{} · {hint} ",
        session.display_name(),
        tilde(&session.cwd),
        session.version.as_deref().unwrap_or("?")
    );
    let text = match app.transcript(id) {
        Some(t) if !t.entries.is_empty() => render_entries(&t.entries, app.show_thinking),
        Some(t) if t.path.is_none() => Text::from("Waiting for transcript…").fg(Color::DarkGray),
        _ => Text::from("Empty conversation.").fg(Color::DarkGray),
    };

    let block = pane_block(title, focused);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width) as u16;
    app.max_scroll = total.saturating_sub(inner.height);
    if app.follow {
        app.scroll = app.max_scroll;
    }
    app.scroll = app.scroll.min(app.max_scroll);

    frame.render_widget(paragraph.block(block).scroll((app.scroll, 0)), area);
}

fn render_entries(entries: &[Entry], show_thinking: bool) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        match entry {
            Entry::User(text) => {
                lines.push(Line::default());
                for (i, l) in text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(ACCENT).bold()),
                        Span::styled(l.to_string(), Style::default().fg(Color::White).bold()),
                    ]));
                }
                lines.push(Line::default());
            }
            Entry::Assistant(text) => {
                for (i, l) in text.lines().enumerate() {
                    let prefix = if i == 0 { "⏺ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::raw(prefix),
                        Span::raw(l.to_string()),
                    ]));
                }
                lines.push(Line::default());
            }
            Entry::Thinking(text) if show_thinking => {
                let style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                for (i, l) in text.lines().enumerate() {
                    let prefix = if i == 0 { "✻ " } else { "  " };
                    lines.push(Line::styled(format!("{prefix}{l}"), style));
                }
                lines.push(Line::default());
            }
            Entry::Thinking(_) => {}
            Entry::ToolUse { name, summary } => {
                lines.push(Line::from(vec![
                    Span::styled("⏺ ", Style::default().fg(Color::Green)),
                    Span::styled(name.clone(), Style::default().bold()),
                    Span::styled(format!("({summary})"), Style::default().fg(Color::Gray)),
                ]));
            }
            Entry::ToolResult { text, is_error } => {
                let color = if *is_error {
                    Color::Red
                } else {
                    Color::DarkGray
                };
                let all: Vec<&str> = text.lines().collect();
                for (i, l) in all.iter().take(TOOL_RESULT_LINES).enumerate() {
                    let prefix = if i == 0 { "  ⎿  " } else { "     " };
                    lines.push(Line::styled(
                        format!("{prefix}{}", truncate(l, 200)),
                        Style::default().fg(color),
                    ));
                }
                if all.len() > TOOL_RESULT_LINES {
                    lines.push(Line::from(dim(format!(
                        "     … +{} lines",
                        all.len() - TOOL_RESULT_LINES
                    ))));
                }
                lines.push(Line::default());
            }
        }
    }
    Text::from(lines)
}

fn draw_footer(frame: &mut Frame, app: &mut App, area: Rect) {
    if let Some((msg, _)) = &app.status {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Yellow),
            )),
            area,
        );
        return;
    }
    let key = |k: &'static str| Span::styled(k, Style::default().fg(ACCENT).bold());
    let on_off = |b: bool| if b { "on" } else { "off" };

    let line = if app.focus == Focus::Conversation && app.current_pty_running() {
        Line::from(vec![
            key(" ctrl-]"),
            dim(" back to sidebar  "),
            dim("· all other keys go to claude · wheel scrolls"),
        ])
    } else {
        let mut spans = vec![
            key(" j/k"),
            dim(" select  "),
            key("enter"),
            dim(" focus  "),
            key("n"),
            dim(" new  "),
            key("r"),
            dim(" resume  "),
            key("x"),
            dim(" close  "),
            key("a"),
            dim(format!(" past:{}  ", on_off(app.show_history))),
            key("t"),
            dim(format!(" thinking:{}  ", on_off(app.show_thinking))),
            key("g/G"),
            dim(" top/end  "),
            key("q"),
            dim(" quit"),
        ];
        if matches!(app.current(), Some(RowKey::Session(_))) && app.follow {
            spans.push(Span::styled(
                "  ● following",
                Style::default().fg(Color::Green),
            ));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_input(frame: &mut Frame, value: &str) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(80);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(3) / 2,
        width,
        3,
    );
    let block = pane_block(
        " New claude session in… (enter start · esc cancel) ".into(),
        true,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(value.to_string()).block(block), popup);
    let cursor_x = popup.x + 1 + (value.chars().count() as u16).min(width.saturating_sub(3));
    frame.set_cursor_position((cursor_x, popup.y + 1));
}

fn age(ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let mins = (now - ms).max(0) / 60_000;
    match mins {
        0..60 => format!("{mins}m"),
        60..1440 => format!("{}h", mins / 60),
        _ => format!("{}d", mins / 1440),
    }
}

fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max < 2 {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(count - (max - 1)).collect();
        format!("…{tail}")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn dump(terminal: &Terminal<TestBackend>) {
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            println!("{row}");
        }
    }

    /// Renders one frame against the real ~/.claude data; run with `--nocapture` to eyeball it.
    #[test]
    fn renders_a_frame() {
        let mut app = App::new();
        app.toggle_history();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        dump(&terminal);
    }
}
