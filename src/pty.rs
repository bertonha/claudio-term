//! A `claude` process hosted inside a pseudo-terminal, rendered via a vt100 emulator.

use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

const SCROLLBACK: usize = 5000;

/// Per-session variables a parent claude exports to its children. If claudio-term itself
/// runs under claude (e.g. via `!`), passing these on would make each embedded claude think it
/// is a nested child session and disable its transcript.
const SESSION_ENV_MARKERS: [&str; 10] = [
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

/// The terminal emulator state for one embedded claude.
pub type Term = vt100::Parser<Responder>;

/// Answers the terminal queries a program sends on startup. Claude Code waits for these
/// replies (DA1 especially) before enabling its fullscreen renderer.
#[derive(Default)]
pub struct Responder {
    replies: Vec<u8>,
}

impl vt100::Callbacks for Responder {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let (row, col) = screen.cursor_position();
        let reply = match (i1, c, first) {
            // Primary device attributes: claim to be a VT220-class terminal.
            (None, 'c', 0) => "\x1b[?62;22c".to_string(),
            // Device status report: terminal OK.
            (None, 'n', 5) => "\x1b[0n".to_string(),
            // Cursor position report (plain and DEC private).
            (None, 'n', 6) => format!("\x1b[{};{}R", row + 1, col + 1),
            (Some(b'?'), 'n', 6) => format!("\x1b[?{};{};1R", row + 1, col + 1),
            _ => return,
        };
        self.replies.extend_from_slice(reply.as_bytes());
    }
}

pub struct Pty {
    pub id: u64,
    pub cwd: String,
    /// Session id, known up front when resuming, otherwise learned from the registry.
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub parser: Arc<Mutex<Term>>,
    pub exited: bool,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
}

impl Pty {
    /// Spawns `claude` (optionally `--resume <id>`) in `cwd`. `dirty` is raised on new output.
    pub fn spawn(
        id: u64,
        cwd: &str,
        resume: Option<String>,
        dirty: Arc<AtomicBool>,
    ) -> Result<Self> {
        let size = (24, 80);
        let pair = native_pty_system().openpty(PtySize {
            rows: size.0,
            cols: size.1,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // CLAUDIO_CLAUDE_BIN lets you point at a specific claude build (or a stand-in for tests).
        let program = std::env::var("CLAUDIO_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
        let mut cmd = CommandBuilder::new(program);
        cmd.cwd(cwd);
        if let Some(session) = &resume {
            cmd.args(["--resume", session]);
        }
        cmd.env("TERM", "xterm-256color");
        for var in SESSION_ENV_MARKERS {
            cmd.env_remove(var);
        }
        let child = pair.slave.spawn_command(cmd)?;
        // Drop our copy of the slave so the reader sees EOF when claude exits.
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(Term::new_with_callbacks(
            size.0,
            size.1,
            SCROLLBACK,
            Responder::default(),
        )));
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;
        let (sink, responder) = (parser.clone(), writer.clone());
        // CLAUDIO_PTY_LOG=<file> appends the raw child output there, for debugging rendering.
        let mut log = std::env::var_os("CLAUDIO_PTY_LOG").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if let Some(log) = &mut log {
                    let _ = log.write_all(&buf[..n]);
                }
                let replies = {
                    let mut term = sink.lock().unwrap();
                    term.process(&buf[..n]);
                    std::mem::take(&mut term.callbacks_mut().replies)
                };
                if !replies.is_empty() {
                    let mut w = responder.lock().unwrap();
                    let _ = w.write_all(&replies).and_then(|_| w.flush());
                }
                dirty.store(true, Ordering::Relaxed);
            }
            dirty.store(true, Ordering::Relaxed);
        });

        Ok(Self {
            id,
            cwd: cwd.to_string(),
            session_id: resume,
            pid: child.process_id(),
            parser,
            exited: false,
            writer,
            master: pair.master,
            child,
            size,
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 || (rows, cols) == self.size {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Returns true the first time the child is observed to have exited.
    pub fn poll_exit(&mut self) -> bool {
        if self.exited {
            return false;
        }
        self.exited = matches!(self.child.try_wait(), Ok(Some(_)));
        self.exited
    }

    pub fn kill(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
        }
    }

    pub fn send_key(&mut self, key: KeyEvent) {
        let app_cursor = self.parser.lock().unwrap().screen().application_cursor();
        if let Some(bytes) = encode_key(key, app_cursor) {
            self.write(&bytes);
        }
    }

    pub fn paste(&mut self, text: &str) {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        let mut bytes = Vec::new();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.write(&bytes);
    }

    /// Scrolls the emulator's scrollback view (positive = further back in history).
    pub fn scroll(&self, delta: i32) {
        let mut parser = self.parser.lock().unwrap();
        let current = parser.screen().scrollback() as i32;
        parser
            .screen_mut()
            .set_scrollback((current + delta).max(0) as usize);
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.exited {
            return;
        }
        // Typing jumps back to the live screen.
        self.parser.lock().unwrap().screen_mut().set_scrollback(0);
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.write_all(bytes).and_then(|_| writer.flush());
    }

    /// Forwards a mouse event at (`col`, `row`) relative to the pty's top-left corner.
    /// Returns false when the child hasn't asked for this kind of mouse event.
    pub fn send_mouse(&mut self, event: MouseEvent, col: u16, row: u16) -> bool {
        let (mode, encoding) = {
            let term = self.parser.lock().unwrap();
            (
                term.screen().mouse_protocol_mode(),
                term.screen().mouse_protocol_encoding(),
            )
        };
        match encode_mouse(event, col, row, mode, encoding) {
            Some(bytes) => {
                let mut writer = self.writer.lock().unwrap();
                let _ = writer.write_all(&bytes).and_then(|_| writer.flush());
                true
            }
            None => false,
        }
    }
}

/// Translates a crossterm key event into the byte sequence a terminal would send.
fn encode_key(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let modifier = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;

    let csi_letter = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let csi_tilde = |n: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{n};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 & 0x1f,
                ' ' | '@' | '2' => 0x00,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '/' | '7' => 0x1f,
                _ => return None,
            };
            with_alt(alt, vec![b])
        }
        KeyCode::Char(c) => with_alt(alt, c.to_string().into_bytes()),
        // Claude Code treats meta+enter as "insert newline".
        KeyCode::Enter if shift || alt => b"\x1b\r".to_vec(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => with_alt(alt, vec![0x7f]),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => csi_letter('A'),
        KeyCode::Down => csi_letter('B'),
        KeyCode::Right => csi_letter('C'),
        KeyCode::Left => csi_letter('D'),
        KeyCode::Home => csi_letter('H'),
        KeyCode::End => csi_letter('F'),
        KeyCode::Insert => csi_tilde(2),
        KeyCode::Delete => csi_tilde(3),
        KeyCode::PageUp => csi_tilde(5),
        KeyCode::PageDown => csi_tilde(6),
        KeyCode::F(n @ 1..=4) => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
        KeyCode::F(n @ 5..=12) => csi_tilde([15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
        _ => return None,
    };
    Some(bytes)
}

fn encode_mouse(
    event: MouseEvent,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use MouseProtocolMode as Mode;
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match event.kind {
        _ if mode == Mode::None => return None,
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) if mode != Mode::Press => (button(b), true),
        MouseEventKind::Drag(b) if matches!(mode, Mode::ButtonMotion | Mode::AnyMotion) => {
            (32 + button(b), false)
        }
        MouseEventKind::Moved if mode == Mode::AnyMotion => (35, false),
        _ => return None,
    };
    let mods = event.modifiers;
    let code =
        code + if mods.contains(KeyModifiers::SHIFT) {
            4
        } else {
            0
        } + if mods.contains(KeyModifiers::ALT) {
            8
        } else {
            0
        } + if mods.contains(KeyModifiers::CONTROL) {
            16
        } else {
            0
        };
    let (x, y) = (col + 1, row + 1);
    Some(match encoding {
        MouseProtocolEncoding::Sgr => {
            format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes()
        }
        // Legacy X10 encoding can't name the released button and caps coordinates at 223.
        _ => {
            let code = if release { 3 + (code & !3) } else { code };
            let clamp = |v: u16| (v.min(223) + 32) as u8;
            vec![0x1b, b'[', b'M', 32 + code as u8, clamp(x), clamp(y)]
        }
    })
}

fn with_alt(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, modifiers), false).unwrap()
    }

    #[test]
    fn encodes_common_keys() {
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![0x03]);
        assert_eq!(key(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
        assert_eq!(key(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(key(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true).unwrap(),
            b"\x1bOA"
        );
    }

    #[test]
    fn answers_terminal_queries() {
        let mut term = Term::new_with_callbacks(10, 40, 0, Responder::default());
        term.process(b"ab\x1b[c\x1b[6n\x1b[?6n");
        assert_eq!(
            term.callbacks().replies,
            b"\x1b[?62;22c\x1b[1;3R\x1b[?1;3;1R"
        );
    }

    #[test]
    fn encodes_mouse() {
        let ev = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let sgr = |kind, mode| encode_mouse(ev(kind), 4, 2, mode, MouseProtocolEncoding::Sgr);
        assert_eq!(
            sgr(MouseEventKind::ScrollUp, MouseProtocolMode::PressRelease).unwrap(),
            b"\x1b[<64;5;3M"
        );
        assert_eq!(
            sgr(
                MouseEventKind::Up(MouseButton::Left),
                MouseProtocolMode::PressRelease
            )
            .unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            sgr(MouseEventKind::Moved, MouseProtocolMode::ButtonMotion),
            None
        );
        assert_eq!(sgr(MouseEventKind::ScrollUp, MouseProtocolMode::None), None);
    }

    fn wait_for(pty: &Pty, needle: &str) -> String {
        for _ in 0..100 {
            let contents = pty.parser.lock().unwrap().screen().contents();
            if contents.contains(needle) {
                return contents;
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        pty.parser.lock().unwrap().screen().contents()
    }

    /// Round-trips keystrokes through a real pty, using `cat` as a stand-in for claude.
    #[test]
    fn keys_reach_the_child_and_output_renders() {
        // SAFETY: test-only; no other test in this module reads the variable concurrently.
        unsafe { std::env::set_var("CLAUDIO_CLAUDE_BIN", "cat") };
        let mut pty = Pty::spawn(1, "/", None, Arc::new(AtomicBool::new(false))).unwrap();
        pty.resize(10, 40);
        for c in "hello pty".chars() {
            pty.send_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        pty.send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let screen = wait_for(&pty, "hello pty\nhello pty");
        assert!(
            screen.contains("hello pty\nhello pty"),
            "screen was: {screen:?}"
        );

        pty.send_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        for _ in 0..100 {
            if pty.poll_exit() || pty.exited {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(pty.exited);
    }
}

/// Boots the real `claude` in a pty and prints the emulated screen.
/// Run manually: `cargo test real_claude -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn real_claude_renders() {
    let cwd = env!("CARGO_MANIFEST_DIR");
    let mut pty = Pty::spawn(1, cwd, None, Arc::new(AtomicBool::new(false))).unwrap();
    pty.resize(30, 110);
    thread::sleep(std::time::Duration::from_secs(8));
    for c in "typed from claudio".chars() {
        pty.send_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    thread::sleep(std::time::Duration::from_secs(1));
    let (screen, alt) = {
        let term = pty.parser.lock().unwrap();
        (term.screen().contents(), term.screen().alternate_screen())
    };
    println!("{screen}\n--- alternate screen (fullscreen renderer): {alt}");
    assert!(
        screen.contains("typed from claudio"),
        "typed text not echoed"
    );
    pty.kill();
    assert!(!screen.trim().is_empty(), "claude rendered nothing");
}
