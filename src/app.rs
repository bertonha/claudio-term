use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    pty::Pty,
    sessions::{self, HistoryCache, Session},
    transcript::Transcript,
};

const REGISTRY_REFRESH: Duration = Duration::from_secs(1);
const HISTORY_REFRESH: Duration = Duration::from_secs(5);
const TRANSCRIPT_REFRESH: Duration = Duration::from_millis(250);
const STATUS_TTL: Duration = Duration::from_secs(4);
const HISTORY_LIMIT: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Conversation,
}

/// One entry in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// A claude hosted inside this app.
    Pty(u64),
    /// A session running elsewhere, or a past one.
    Session(String),
}

pub struct App {
    /// Sessions running anywhere on the machine (from the registry).
    pub sessions: Vec<Session>,
    /// Sessions seen running during this run that have since exited, newest first.
    /// Claude deletes its registry entry on exit, so without this they'd vanish.
    pub exited: Vec<Session>,
    /// Recent past sessions (from transcripts); only loaded when shown.
    pub history: Vec<Session>,
    pub ptys: Vec<Pty>,
    /// Raised by pty reader threads when there is new output to draw.
    pub dirty: Arc<AtomicBool>,
    pub selected: usize,
    pub transcripts: HashMap<String, Transcript>,
    pub focus: Focus,
    pub show_thinking: bool,
    pub show_history: bool,
    /// Scroll offset (in wrapped lines) of the transcript view.
    pub scroll: u16,
    /// Max scroll computed at last render; used to clamp key handling.
    pub max_scroll: u16,
    /// When true the transcript sticks to the bottom as new messages arrive.
    pub follow: bool,
    /// Directory being typed for a new session, while the prompt is open.
    pub input: Option<String>,
    /// Screen area of the embedded terminal at the last draw, for mouse forwarding.
    pub pty_area: Option<ratatui::layout::Rect>,
    pub status: Option<(String, Instant)>,
    pub quit_armed: bool,
    pub should_quit: bool,
    launch_cwd: String,
    next_pty_id: u64,
    history_cache: HistoryCache,
    last_registry: Option<Instant>,
    last_history: Option<Instant>,
    last_transcript: Option<Instant>,
}

impl App {
    pub fn new() -> Self {
        let launch_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut app = Self {
            sessions: Vec::new(),
            exited: Vec::new(),
            history: Vec::new(),
            ptys: Vec::new(),
            dirty: Arc::new(AtomicBool::new(false)),
            selected: 0,
            transcripts: HashMap::new(),
            focus: Focus::Sidebar,
            show_thinking: false,
            show_history: false,
            scroll: 0,
            max_scroll: 0,
            follow: true,
            input: None,
            pty_area: None,
            status: None,
            quit_armed: false,
            should_quit: false,
            launch_cwd: sessions::tilde(&launch_cwd),
            next_pty_id: 1,
            history_cache: HistoryCache::default(),
            last_registry: None,
            last_history: None,
            last_transcript: None,
        };
        app.tick();
        app
    }

    pub fn rows(&self) -> Vec<RowKey> {
        let mut rows: Vec<RowKey> = self.ptys.iter().map(|p| RowKey::Pty(p.id)).collect();
        let pty_pids: HashSet<i32> = self
            .ptys
            .iter()
            .filter_map(|p| p.pid)
            .map(|p| p as i32)
            .collect();
        rows.extend(
            self.sessions
                .iter()
                .filter(|s| !pty_pids.contains(&s.pid))
                .map(|s| RowKey::Session(s.session_id.clone())),
        );
        let mut owned: HashSet<&str> = self
            .ptys
            .iter()
            .filter_map(|p| p.session_id.as_deref())
            .chain(self.sessions.iter().map(|s| s.session_id.as_str()))
            .collect();
        for s in &self.exited {
            if owned.insert(&s.session_id) {
                rows.push(RowKey::Session(s.session_id.clone()));
            }
        }
        if self.show_history {
            for s in &self.history {
                if owned.insert(&s.session_id) {
                    rows.push(RowKey::Session(s.session_id.clone()));
                }
            }
        }
        rows
    }

    pub fn current(&self) -> Option<RowKey> {
        self.rows().get(self.selected).cloned()
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .chain(&self.exited)
            .chain(&self.history)
            .find(|s| s.session_id == id)
    }

    pub fn pty(&self, id: u64) -> Option<&Pty> {
        self.ptys.iter().find(|p| p.id == id)
    }

    pub fn current_pty_mut(&mut self) -> Option<&mut Pty> {
        match self.current()? {
            RowKey::Pty(id) => self.ptys.iter_mut().find(|p| p.id == id),
            RowKey::Session(_) => None,
        }
    }

    /// True when the selected row is an embedded claude that is still running.
    pub fn current_pty_running(&mut self) -> bool {
        self.current_pty_mut().is_some_and(|p| !p.exited)
    }

    /// The registry entry of an embedded claude, once it has registered itself.
    pub fn pty_session(&self, pty: &Pty) -> Option<&Session> {
        let pid = pty.pid? as i32;
        self.sessions.iter().find(|s| s.pid == pid)
    }

    pub fn transcript(&self, session_id: &str) -> Option<&Transcript> {
        self.transcripts.get(session_id)
    }

    /// Periodic work. Returns true if anything visible may have changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        let key = self.current();

        if self
            .last_registry
            .is_none_or(|t| t.elapsed() >= REGISTRY_REFRESH)
        {
            let live = sessions::discover();
            self.track_exits(&live);
            self.sessions = live;
            self.last_registry = Some(Instant::now());
            for pty in &mut self.ptys {
                if let Some(pid) = pty.pid
                    && let Some(s) = self.sessions.iter().find(|s| s.pid == pid as i32)
                {
                    pty.session_id = Some(s.session_id.clone());
                }
            }
            changed = true;
        }
        if self.show_history
            && self
                .last_history
                .is_none_or(|t| t.elapsed() >= HISTORY_REFRESH)
        {
            self.history = sessions::history(&mut self.history_cache, HISTORY_LIMIT);
            self.last_history = Some(Instant::now());
            changed = true;
        }
        for pty in &mut self.ptys {
            changed |= pty.poll_exit();
        }
        if changed {
            self.reselect(key);
        }
        if self
            .last_transcript
            .is_none_or(|t| t.elapsed() >= TRANSCRIPT_REFRESH)
        {
            changed |= self.poll_transcript();
        }
        if self
            .status
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= STATUS_TTL)
        {
            self.status = None;
            changed = true;
        }
        changed
    }

    /// Moves sessions that dropped out of the registry into `exited` so they stay
    /// selectable and resumable. Embedded sessions are skipped; their pty row covers them.
    fn track_exits(&mut self, live: &[Session]) {
        let live_ids: HashSet<&str> = live.iter().map(|s| s.session_id.as_str()).collect();
        let pty_pids: HashSet<i32> = self
            .ptys
            .iter()
            .filter_map(|p| p.pid)
            .map(|p| p as i32)
            .collect();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default();
        // A session resumed elsewhere is live again.
        self.exited
            .retain(|s| !live_ids.contains(s.session_id.as_str()));
        for gone in self
            .sessions
            .iter()
            .filter(|s| !live_ids.contains(s.session_id.as_str()) && !pty_pids.contains(&s.pid))
        {
            let mut gone = gone.clone();
            gone.alive = false;
            gone.status = None;
            gone.started_at = now;
            self.exited.retain(|s| s.session_id != gone.session_id);
            self.exited.insert(0, gone);
        }
    }

    /// Keeps `key` selected after the row list changed, if it still exists.
    fn reselect(&mut self, key: Option<RowKey>) {
        let rows = self.rows();
        self.selected = key
            .and_then(|k| rows.iter().position(|r| *r == k))
            .unwrap_or(self.selected)
            .min(rows.len().saturating_sub(1));
    }

    fn poll_transcript(&mut self) -> bool {
        self.last_transcript = Some(Instant::now());
        let Some(RowKey::Session(id)) = self.current() else {
            return false;
        };
        let known_path = self.session(&id).and_then(|s| s.transcript.clone());
        let transcript = self.transcripts.entry(id.clone()).or_default();
        if transcript.path.is_none() {
            // The transcript file is created lazily by Claude on the first message.
            transcript.path = known_path.or_else(|| sessions::find_transcript(&id));
        }
        transcript.poll()
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn select(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        if next != self.selected {
            self.selected = next;
            self.follow = true;
            self.poll_transcript();
        }
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).clamp(0, self.max_scroll as i32) as u16;
        self.scroll = next;
        self.follow = next >= self.max_scroll;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
        self.follow = false;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
    }

    pub fn toggle_history(&mut self) {
        let key = self.current();
        self.show_history = !self.show_history;
        self.last_history = None;
        self.tick();
        self.reselect(key);
        self.flash(if self.show_history {
            "Showing past sessions"
        } else {
            "Hiding past sessions"
        });
    }

    pub fn open_new_prompt(&mut self) {
        self.input = Some(self.launch_cwd.clone());
    }

    pub fn submit_new_prompt(&mut self) {
        let Some(value) = self.input.take() else {
            return;
        };
        let dir = sessions::expand_tilde(value.trim());
        if !Path::new(&dir).is_dir() {
            self.flash(format!("Not a directory: {dir}"));
            return;
        }
        self.spawn(dir, None);
    }

    fn spawn(&mut self, cwd: String, resume: Option<String>) {
        match Pty::spawn(self.next_pty_id, &cwd, resume, self.dirty.clone()) {
            Ok(pty) => {
                self.next_pty_id += 1;
                let key = RowKey::Pty(pty.id);
                self.ptys.push(pty);
                self.reselect(Some(key));
                self.focus = Focus::Conversation;
            }
            Err(e) => self.flash(format!("Failed to start claude: {e}")),
        }
    }

    /// Resumes the selected session inside the app (`claude --resume`).
    pub fn resume_selected(&mut self) {
        match self.current() {
            Some(RowKey::Session(id)) => {
                let Some(s) = self.session(&id) else { return };
                if s.alive {
                    let msg = format!(
                        "{} is running in another terminal (pid {}) — exit it there, then press r again",
                        s.display_name(),
                        s.pid
                    );
                    self.flash(msg);
                } else {
                    let cwd = s.cwd.clone();
                    self.spawn(cwd, Some(id));
                }
            }
            Some(RowKey::Pty(pty_id)) => {
                let Some(pty) = self.pty(pty_id) else { return };
                if !pty.exited {
                    self.focus = Focus::Conversation;
                } else if let Some(session_id) = pty.session_id.clone() {
                    let cwd = pty.cwd.clone();
                    self.ptys.retain(|p| p.id != pty_id);
                    self.spawn(cwd, Some(session_id));
                } else {
                    self.flash("This session never started a conversation; nothing to resume");
                }
            }
            None => {}
        }
    }

    /// Removes an exited embedded session from the list.
    pub fn close_selected(&mut self) {
        let Some(RowKey::Pty(id)) = self.current() else {
            return;
        };
        if self.pty(id).is_some_and(|p| p.exited) {
            self.ptys.retain(|p| p.id != id);
            self.reselect(None);
        } else {
            self.flash("Still running — type /exit in it first");
        }
    }

    pub fn request_quit(&mut self) {
        let running = self.ptys.iter().filter(|p| !p.exited).count();
        if running == 0 || self.quit_armed {
            self.should_quit = true;
        } else {
            self.quit_armed = true;
            self.flash(format!(
                "{running} embedded session(s) running — press q again to quit and stop them"
            ));
        }
    }

    pub fn shutdown(&mut self) {
        for pty in &mut self.ptys {
            pty.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_session(id: &str, pid: i32) -> Session {
        let mut s: Session = serde_json::from_value(
            serde_json::json!({ "pid": pid, "sessionId": id, "cwd": "/tmp", "status": "idle" }),
        )
        .unwrap();
        s.alive = true;
        s
    }

    #[test]
    fn exited_sessions_stay_listed_and_resumable() {
        let mut app = App::new();
        app.sessions = vec![
            live_session("gone", 999_001),
            live_session("still-here", 999_002),
        ];
        app.exited.clear();

        // The "gone" instance quit, so it dropped out of the registry.
        let live = vec![live_session("still-here", 999_002)];
        app.track_exits(&live);
        app.sessions = live;

        let gone = app.session("gone").expect("exited session is still known");
        assert!(!gone.alive);
        assert!(app.rows().contains(&RowKey::Session("gone".into())));

        // If it's resumed elsewhere it becomes live again instead of exited.
        let live = vec![
            live_session("gone", 999_003),
            live_session("still-here", 999_002),
        ];
        app.track_exits(&live);
        assert!(app.exited.iter().all(|s| s.session_id != "gone"));
    }
}
