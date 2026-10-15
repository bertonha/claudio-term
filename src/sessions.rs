//! Discovery of Claude Code sessions.
//!
//! Every running interactive `claude` registers itself in `~/.claude/sessions/<pid>.json`
//! (the file is removed on exit). Transcripts live in
//! `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` and outlive the process, so they
//! are the source for past, resumable sessions.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::Value;

use crate::transcript::truncate;

/// How much of a transcript's tail to scan for its title.
const TITLE_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(default)]
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    /// Start time (live sessions) or last-modified time (past sessions), in ms since epoch.
    #[serde(default)]
    pub started_at: i64,
    pub name: Option<String>,
    pub status: Option<String>,
    pub version: Option<String>,
    #[serde(skip)]
    pub alive: bool,
    #[serde(skip)]
    pub transcript: Option<PathBuf>,
}

impl Session {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or(&self.session_id[..8.min(self.session_id.len())])
    }
}

pub fn claude_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude")
}

/// Replaces the home directory prefix with `~`.
pub fn tilde(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(home.to_string_lossy().as_ref()) {
            Some(rest) => format!("~{rest}"),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

pub fn expand_tilde(path: &str) -> String {
    match (path.strip_prefix('~'), dirs::home_dir()) {
        (Some(rest), Some(home)) => format!("{}{rest}", home.to_string_lossy()),
        _ => path.to_string(),
    }
}

/// Lists running sessions, newest first.
pub fn discover() -> Vec<Session> {
    let Ok(entries) = fs::read_dir(claude_dir().join("sessions")) else {
        return Vec::new();
    };
    let mut sessions: Vec<Session> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|p| fs::read_to_string(p).ok())
        .filter_map(|s| serde_json::from_str::<Session>(&s).ok())
        .filter(|s| pid_alive(s.pid))
        .map(|mut s| {
            s.alive = true;
            s
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    sessions
}

fn pid_alive(pid: i32) -> bool {
    // Signal 0 performs error checking only; EPERM still means the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Finds the transcript file for a session id by scanning the project dirs.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    fs::read_dir(claude_dir().join("projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(&file))
        .find(|p| p.is_file())
}

/// Caches parsed transcript metadata keyed by path and mtime.
#[derive(Default)]
pub struct HistoryCache(HashMap<PathBuf, (SystemTime, Session)>);

/// The `limit` most recently modified transcripts, as (not running) sessions.
pub fn history(cache: &mut HistoryCache, limit: usize) -> Vec<Session> {
    let Ok(projects) = fs::read_dir(claude_dir().join("projects")) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, SystemTime)> = projects
        .flatten()
        .filter_map(|dir| fs::read_dir(dir.path()).ok())
        .flat_map(|entries| entries.flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|p| Some((p.clone(), p.metadata().ok()?.modified().ok()?)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.truncate(limit);

    files
        .into_iter()
        .filter_map(|(path, mtime)| {
            if let Some((cached_mtime, session)) = cache.0.get(&path)
                && *cached_mtime == mtime
            {
                return Some(session.clone());
            }
            let session = read_meta(&path, mtime)?;
            cache.0.insert(path, (mtime, session.clone()));
            Some(session)
        })
        .collect()
}

fn read_meta(path: &Path, mtime: SystemTime) -> Option<Session> {
    let session_id = path.file_stem()?.to_str()?.to_string();
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let (mut cwd, mut version, mut first_prompt, mut title) = (None, None, None, None);
    let mut visit = |v: &Value| {
        if cwd.is_none() {
            cwd = v["cwd"].as_str().map(String::from);
        }
        if version.is_none() {
            version = v["version"].as_str().map(String::from);
        }
        match v["type"].as_str() {
            Some("agent-name") => {
                title = v["agentName"].as_str().map(String::from).or(title.take())
            }
            Some("ai-title") => title = v["aiTitle"].as_str().map(String::from).or(title.take()),
            Some("user") if first_prompt.is_none() && v["isMeta"].as_bool() != Some(true) => {
                first_prompt = v["message"]["content"]
                    .as_str()
                    .filter(|s| !s.starts_with('<'))
                    .and_then(|s| s.lines().next())
                    .map(|s| truncate(s, 40));
            }
            _ => {}
        }
    };

    for line in BufReader::new(&file)
        .lines()
        .take(200)
        .map_while(Result::ok)
    {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            visit(&v);
        }
    }
    // Titles are appended as the session evolves; the latest one is near the end.
    file.seek(SeekFrom::Start(len.saturating_sub(TITLE_TAIL_BYTES)))
        .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    for line in String::from_utf8_lossy(&tail).lines() {
        if (line.contains("\"ai-title\"") || line.contains("\"agent-name\""))
            && let Ok(v) = serde_json::from_str::<Value>(line)
        {
            visit(&v);
        }
    }

    let modified_ms = mtime.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
    Some(Session {
        pid: 0,
        session_id,
        cwd: cwd?,
        started_at: modified_ms,
        name: title.or(first_prompt),
        status: None,
        version,
        alive: false,
        transcript: Some(path.to_path_buf()),
    })
}
