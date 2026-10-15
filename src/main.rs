mod app;
mod pty;
mod sessions;
mod transcript;
mod ui;

use std::{io::stdout, sync::atomic::Ordering, time::Duration};

use anyhow::Result;
use ratatui::{
    DefaultTerminal,
    crossterm::{
        event::{
            self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
            EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
            MouseEventKind,
        },
        execute,
    },
    layout::Position,
};

use app::{App, Focus};

/// Input poll interval; short so embedded claude output renders smoothly.
const FRAME: Duration = Duration::from_millis(16);

fn main() -> Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let mut app = App::new();
    let result = run(&mut terminal, &mut app);
    app.shutdown();
    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut needs_draw = true;
    while !app.should_quit {
        if needs_draw {
            terminal.draw(|f| ui::draw(f, app))?;
        }
        needs_draw = false;
        if event::poll(FRAME)? {
            // Drain everything that's queued (e.g. fast typing) before redrawing.
            loop {
                handle_event(app, event::read()?);
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            needs_draw = true;
        }
        needs_draw |= app.tick();
        needs_draw |= app.dirty.swap(false, Ordering::Relaxed);
    }
    Ok(())
}

/// Ctrl-] leaves an embedded session. Legacy terminals report it as Ctrl-5.
fn is_detach(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char(']' | '5'))
}

fn handle_event(app: &mut App, event: Event) {
    let pty_focused = app.focus == Focus::Conversation && app.current_pty_running();
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            if app.input.is_some() {
                handle_input_key(app, key);
            } else if pty_focused {
                if is_detach(&key) {
                    app.focus = Focus::Sidebar;
                } else if let Some(pty) = app.current_pty_mut() {
                    pty.send_key(key);
                }
            } else {
                handle_nav_key(app, key);
            }
        }
        Event::Paste(text) => {
            if let Some(input) = &mut app.input {
                input.push_str(text.trim_end_matches(['\n', '\r']));
            } else if pty_focused && let Some(pty) = app.current_pty_mut() {
                pty.paste(&text);
            }
        }
        Event::Mouse(m) => {
            if let Some(area) = app.pty_area
                && area.contains(Position::new(m.column, m.row))
                && app.current_pty_running()
            {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    app.focus = Focus::Conversation;
                }
                let (col, row) = (m.column - area.x, m.row - area.y);
                if app
                    .current_pty_mut()
                    .is_some_and(|pty| pty.send_mouse(m, col, row))
                {
                    return;
                }
            }
            let delta = match m.kind {
                MouseEventKind::ScrollUp => -3,
                MouseEventKind::ScrollDown => 3,
                _ => return,
            };
            match app.current_pty_mut() {
                Some(pty) => pty.scroll(-delta),
                None => app.scroll_by(delta),
            }
        }
        _ => {}
    }
}

fn handle_input_key(app: &mut App, key: KeyEvent) {
    let Some(input) = &mut app.input else { return };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => app.submit_new_prompt(),
        KeyCode::Esc => app.input = None,
        KeyCode::Char('c') if ctrl => app.input = None,
        KeyCode::Char('u') if ctrl => input.clear(),
        KeyCode::Backspace => {
            input.pop();
        }
        KeyCode::Char(c) if !ctrl => input.push(c),
        _ => {}
    }
}

fn handle_nav_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let is_quit =
        matches!(key.code, KeyCode::Char('q')) || (ctrl && key.code == KeyCode::Char('c'));
    if !is_quit {
        app.quit_armed = false;
    }
    let page = 15;
    match (key.code, app.focus) {
        _ if is_quit => app.request_quit(),
        (KeyCode::Esc, _) => app.focus = Focus::Sidebar,
        (KeyCode::Tab | KeyCode::BackTab, _) => {
            app.focus = match app.focus {
                Focus::Sidebar => Focus::Conversation,
                Focus::Conversation => Focus::Sidebar,
            }
        }
        (KeyCode::Left | KeyCode::Char('h'), _) => app.focus = Focus::Sidebar,
        (KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter, _) => {
            app.focus = Focus::Conversation
        }
        (KeyCode::Char('j') | KeyCode::Down, Focus::Sidebar) => app.select(1),
        (KeyCode::Char('k') | KeyCode::Up, Focus::Sidebar) => app.select(-1),
        (KeyCode::Char('j') | KeyCode::Down, Focus::Conversation) => app.scroll_by(1),
        (KeyCode::Char('k') | KeyCode::Up, Focus::Conversation) => app.scroll_by(-1),
        (KeyCode::Char('d'), _) if ctrl => app.scroll_by(page),
        (KeyCode::Char('u'), _) if ctrl => app.scroll_by(-page),
        (KeyCode::PageDown | KeyCode::Char(' '), _) => app.scroll_by(page),
        (KeyCode::PageUp, _) => app.scroll_by(-page),
        (KeyCode::Char('g') | KeyCode::Home, _) => app.scroll_to_top(),
        (KeyCode::Char('G') | KeyCode::End, _) => app.scroll_to_bottom(),
        (KeyCode::Char('n'), _) => app.open_new_prompt(),
        (KeyCode::Char('r'), _) => app.resume_selected(),
        (KeyCode::Char('x'), _) => app.close_selected(),
        (KeyCode::Char('t'), _) => app.show_thinking = !app.show_thinking,
        (KeyCode::Char('a'), _) => app.toggle_history(),
        _ => {}
    }
}
