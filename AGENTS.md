# claudio-term

A ratatui terminal app that aggregates the Claude Code instances on this machine: a sidebar
of instances on the left, the selected conversation on the right. Embedded instances run in a
pty and accept input; external ones are shown read-only from their transcripts.

## Layout

- `src/main.rs`: terminal setup, event loop, key/mouse/paste routing
- `src/app.rs`: app state, sidebar rows, actions (new, resume, close, quit)
- `src/pty.rs`: embedded `claude` in a pseudo-terminal, key/mouse encoding, query responder
- `src/sessions.rs`: running sessions (`~/.claude/sessions/*.json`) and past ones (transcripts)
- `src/transcript.rs`: incremental JSONL transcript parsing
- `src/ui.rs`: rendering

## After every code change

Run these at the end of each change that adds or modifies Rust code, and fix everything they
report before considering the change done:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Do not silence clippy with `#[allow(...)]` unless the lint is a false positive; if you do,
add a comment explaining why.

## Testing notes

- `cargo test` runs unit tests, including a pty round-trip that uses `cat` as a stand-in
  for claude (via `CLAUDIO_CLAUDE_BIN`).
- `cargo test real_claude -- --ignored --nocapture` boots the real `claude`. Use it
  sparingly: killing claude during startup counts as a failed fullscreen start, and after
  repeated failures Claude Code disables its fullscreen renderer machine-wide (re-enable
  with `/tui fullscreen`).
- `CLAUDIO_PTY_LOG=<file>` appends raw child output to a file for debugging rendering.
