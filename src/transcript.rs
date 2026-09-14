//! The user's last prompt to a coding agent, read from the transcript the agent keeps on disk.
//! herdr reports the session per pane (`agent_session`, fed by its agent integrations): the
//! transcript path itself, or a session id to look up under the agent's session directory.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::Value;

use crate::herdr::AgentSession;

/// Bytes read from the end of a transcript; a prompt buried deeper than this is not found.
const TAIL_BYTES: u64 = 4 << 20;
/// Characters of the prompt kept for the label request.
pub const MAX_PROMPT_CHARS: usize = 300;
/// Directory levels searched below a session root when herdr reports only an id.
const MAX_DEPTH: usize = 3;

/// Per-session cache of the last prompt, re-read only when the transcript file changed.
#[derive(Default)]
pub struct Prompts {
    sessions: HashMap<String, Cached>,
}

struct Cached {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    prompt: Option<String>,
}

impl Prompts {
    /// The user's last prompt in `session`'s transcript, when the agent's format is known and
    /// the file exists.
    pub fn last(&mut self, session: &AgentSession) -> Option<String> {
        let cached = match self
            .sessions
            .entry(format!("{}:{}", session.agent, session.value))
        {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => v.insert(Cached {
                path: locate(session)?,
                len: 0,
                modified: None,
                prompt: None,
            }),
        };
        let meta = std::fs::metadata(&cached.path).ok()?;
        let modified = meta.modified().ok();
        if meta.len() != cached.len || modified != cached.modified {
            cached.len = meta.len();
            cached.modified = modified;
            cached.prompt = tail(&cached.path).and_then(|t| last_prompt_in(&session.agent, &t));
        }
        cached.prompt.clone()
    }
}

/// Session roots searched for an id: Claude Code `~/.claude/projects/<cwd slug>/<id>.jsonl`,
/// Codex `~/.codex/sessions/<y>/<m>/<d>/rollout-<time>-<id>.jsonl` (or `archived_sessions/`),
/// Pi `~/.pi/agent/sessions/<cwd slug>/<time>_<id>.jsonl`.
fn locate(session: &AgentSession) -> Option<PathBuf> {
    if session.kind == "path" {
        return Some(PathBuf::from(&session.value)).filter(|p| p.is_file());
    }
    let roots: &[&str] = match session.agent.as_str() {
        "claude" => &[".claude/projects"],
        "codex" => &[".codex/sessions", ".codex/archived_sessions"],
        "pi" => &[".pi/agent/sessions"],
        _ => return None,
    };
    let home = crate::herdr::home_dir();
    roots
        .iter()
        .find_map(|root| find(&home.join(root), &session.value, MAX_DEPTH))
}

/// `<dir>/**/<stem>.jsonl` where the stem is `id` or ends with `-<id>` / `_<id>`, descending at
/// most `depth` levels.
fn find(dir: &Path, id: &str, depth: usize) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            dirs.push(path);
            continue;
        }
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"));
        if stem.is_some_and(|s| {
            s == id
                || s.strip_suffix(id)
                    .is_some_and(|rest| rest.ends_with(['-', '_']))
        }) {
            return Some(path);
        }
    }
    if depth == 0 {
        return None;
    }
    dirs.into_iter().find_map(|d| find(&d, id, depth - 1))
}

fn tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > TAIL_BYTES {
        f.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut buf = Vec::with_capacity(len.min(TAIL_BYTES) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The last prompt the user typed to `agent`, as one line of at most `MAX_PROMPT_CHARS`
/// characters. Lines are tried newest first; each format's non-prompt records are skipped.
pub fn last_prompt_in(agent: &str, transcript: &str) -> Option<String> {
    let (marker, prompt): (&str, fn(&str) -> Option<String>) = match agent {
        "claude" => ("\"type\":\"user\"", claude_prompt),
        "codex" => ("\"user_message\"", codex_prompt),
        "pi" => ("\"role\":\"user\"", pi_prompt),
        _ => return None,
    };
    transcript
        .lines()
        .rev()
        .filter(|l| l.contains(marker))
        .find_map(|line| {
            let text = prompt(line)?;
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            (!text.is_empty() && !is_acknowledgement(&text))
                .then(|| text.chars().take(MAX_PROMPT_CHARS).collect())
        })
}

/// Words a prompt may consist of entirely and still say nothing about the task ("continue",
/// "ok, do it"); the prompt before it is the request then.
const ACKNOWLEDGEMENTS: &[&str] = &[
    "continue", "go", "ahead", "on", "ok", "okay", "yes", "y", "yep", "yeah", "sure", "proceed",
    "next", "do", "it", "please", "resume", "carry", "fine", "cool", "good", "great", "thanks",
    "again", "retry", "k",
];

fn is_acknowledgement(text: &str) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words.len() <= 3 && words.iter().all(|w| ACKNOWLEDGEMENTS.contains(&w.as_str()))
}

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "isMeta")]
    is_meta: bool,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    role: String,
    content: Value,
}

/// Claude Code: `type: user` records. Tool results, hook feedback (`isMeta`), subagent traffic
/// (`isSidechain`) and injected blocks (`<command-name>`, `<system-reminder>`) are not prompts.
fn claude_prompt(line: &str) -> Option<String> {
    let entry: ClaudeLine = serde_json::from_str(line).ok()?;
    if entry.kind != "user" || entry.is_meta || entry.is_sidechain {
        return None;
    }
    typed_text(&entry.message.content)
}

#[derive(Deserialize)]
struct CodexLine {
    #[serde(rename = "type")]
    kind: String,
    payload: CodexPayload,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

/// Codex: the `event_msg` record with a `user_message` payload holds the typed text alone; the
/// `response_item` user messages also carry injected AGENTS.md and environment context.
fn codex_prompt(line: &str) -> Option<String> {
    let entry: CodexLine = serde_json::from_str(line).ok()?;
    (entry.kind == "event_msg" && entry.payload.kind == "user_message")
        .then_some(entry.payload.message)
}

#[derive(Deserialize)]
struct PiLine {
    #[serde(rename = "type")]
    kind: String,
    message: Message,
}

/// Pi: `message` records with role `user`; tool results have a role of their own.
fn pi_prompt(line: &str) -> Option<String> {
    let entry: PiLine = serde_json::from_str(line).ok()?;
    if entry.kind != "message" || entry.message.role != "user" {
        return None;
    }
    typed_text(&entry.message.content)
}

/// The typed text of a message: a string, or the `text` blocks of a block list. Text opening
/// with `<` is injected context, not typing.
fn typed_text(content: &Value) -> Option<String> {
    let typed = |s: &str| (!s.trim_start().starts_with('<')).then(|| s.to_string());
    match content {
        Value::String(s) => typed(s),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str().and_then(typed))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str, extra: &str) -> String {
        format!(r#"{{"type":"user","message":{{"role":"user","content":{content}}}{extra}}}"#)
    }

    #[test]
    fn claude_last_typed_prompt_wins_over_tool_results_meta_and_injected_blocks() {
        let lines = [
            user(r#""fix the flaky   test\nin auth""#, ""),
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"on it"}]}}"#.to_string(),
            user(r#"[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]"#, ""),
            user(r#""Stop hook feedback: rewrite""#, r#","isMeta":true"#),
            user(r#""subagent says hi""#, r#","isSidechain":true"#),
            user(r#""<command-name>/clear</command-name>""#, ""),
            "not json at all".to_string(),
            user(r#""ok, continue!""#, ""),
        ];
        assert_eq!(
            last_prompt_in("claude", &lines.join("\n")).as_deref(),
            Some("fix the flaky test in auth")
        );
        // A prompt with an injected reminder block keeps only the typed block.
        let mixed = user(
            r#"[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"text","text":"review PR 12"}]"#,
            "",
        );
        assert_eq!(
            last_prompt_in("claude", &mixed).as_deref(),
            Some("review PR 12")
        );
        assert_eq!(last_prompt_in("claude", ""), None);
        assert_eq!(
            last_prompt_in(
                "claude",
                &user(r#"[{"type":"tool_result","content":"x"}]"#, "")
            ),
            None
        );
    }

    #[test]
    fn codex_prompt_comes_from_user_message_events_not_injected_user_items() {
        let lines = [
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"continue to find  another one.","images":[]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context><cwd>/x</cwd></environment_context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"user_message mentioned"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
        ];
        assert_eq!(
            last_prompt_in("codex", &lines.join("\n")).as_deref(),
            Some("continue to find another one.")
        );
        assert_eq!(last_prompt_in("codex", lines[1]), None);
    }

    #[test]
    fn pi_prompt_is_the_last_user_message() {
        let lines = [
            r#"{"type":"session","version":3,"id":"s1","cwd":"/x"}"#,
            r#"{"type":"message","id":"a","message":{"role":"user","content":[{"type":"text","text":"make the status bar\ncolourful"}]}}"#,
            r#"{"type":"message","id":"b","message":{"role":"assistant","content":[{"type":"text","text":"role\":\"user"}]}}"#,
            r#"{"type":"message","id":"c","message":{"role":"toolResult","content":[{"type":"text","text":"ok"}]}}"#,
        ];
        assert_eq!(
            last_prompt_in("pi", &lines.join("\n")).as_deref(),
            Some("make the status bar colourful")
        );
        assert_eq!(last_prompt_in("gemini", &lines.join("\n")), None);
    }

    #[test]
    fn prompt_is_capped() {
        let long = "word ".repeat(200);
        let out = last_prompt_in("claude", &user(&format!("\"{long}\""), "")).unwrap();
        assert_eq!(out.chars().count(), MAX_PROMPT_CHARS);
    }

    #[test]
    fn find_matches_exact_and_suffixed_stems_within_depth() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("hal-find-{}-{nonce}", std::process::id()));
        let day = root.join("2026/09/09");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(root.join("slug")).unwrap();
        let codex = day.join("rollout-2026-09-09T11-47-39-id1.jsonl");
        let pi = root.join("slug/2026-09-09T15-48-10-404Z_id2.jsonl");
        let claude = root.join("slug/id3.jsonl");
        for p in [&codex, &pi, &claude, &root.join("slug/notid3.jsonl")] {
            std::fs::write(p, "").unwrap();
        }
        assert_eq!(find(&root, "id1", MAX_DEPTH), Some(codex));
        assert_eq!(find(&root, "id2", MAX_DEPTH), Some(pi));
        assert_eq!(find(&root, "id3", MAX_DEPTH), Some(claude));
        assert_eq!(find(&root, "id1", 1), None);
        assert_eq!(find(&root, "id9", MAX_DEPTH), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
