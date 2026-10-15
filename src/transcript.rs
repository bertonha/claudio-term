//! Incremental parsing of a Claude Code JSONL transcript.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    ToolUse { name: String, summary: String },
    ToolResult { text: String, is_error: bool },
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub path: Option<PathBuf>,
    pub entries: Vec<Entry>,
    offset: u64,
    partial: String,
}

impl Transcript {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            ..Default::default()
        }
    }

    /// Reads any bytes appended since the last poll. Returns true if new entries arrived.
    pub fn poll(&mut self) -> bool {
        let Some(path) = &self.path else { return false };
        let Ok(mut file) = File::open(path) else {
            return false;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // File was truncated/rewritten: start over.
            *self = Self::new(self.path.take());
            return self.poll();
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return false;
        }
        self.offset += buf.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&buf));

        let before = self.entries.len();
        let text = std::mem::take(&mut self.partial);
        let mut lines = text.split('\n').peekable();
        while let Some(line) = lines.next() {
            if lines.peek().is_none() {
                // Last segment has no trailing newline yet; keep it for next poll.
                self.partial = line.to_string();
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                parse_record(&v, &mut self.entries);
            }
        }
        self.entries.len() != before
    }
}

fn parse_record(v: &Value, out: &mut Vec<Entry>) {
    let kind = v["type"].as_str().unwrap_or_default();
    if !matches!(kind, "user" | "assistant") || v["isMeta"].as_bool() == Some(true) {
        return;
    }
    let content = &v["message"]["content"];
    if let Some(s) = content.as_str() {
        push_text(kind, s, out);
        return;
    }
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str().unwrap_or_default() {
            "text" => push_text(kind, block["text"].as_str().unwrap_or_default(), out),
            "thinking" => {
                let t = block["thinking"].as_str().unwrap_or_default().trim();
                if !t.is_empty() {
                    out.push(Entry::Thinking(t.to_string()));
                }
            }
            "tool_use" => out.push(Entry::ToolUse {
                name: block["name"].as_str().unwrap_or("?").to_string(),
                summary: summarize_input(&block["input"]),
            }),
            "tool_result" => out.push(Entry::ToolResult {
                text: result_text(&block["content"]),
                is_error: block["is_error"].as_bool().unwrap_or(false),
            }),
            _ => {}
        }
    }
}

fn push_text(kind: &str, text: &str, out: &mut Vec<Entry>) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    out.push(if kind == "user" {
        Entry::User(text.to_string())
    } else {
        Entry::Assistant(text.to_string())
    });
}

fn summarize_input(input: &Value) -> String {
    const KEYS: [&str; 7] = [
        "command",
        "file_path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ];
    let s = KEYS
        .iter()
        .find_map(|k| input[*k].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    let first = s.lines().next().unwrap_or_default();
    truncate(first, 120)
}

fn result_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
