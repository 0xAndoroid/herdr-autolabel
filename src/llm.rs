//! LLM labelling: provider selection, prompt construction, request, post-processing.

use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::label;
use crate::scrub;

pub const SYSTEM_PROMPT: &str = "You label terminal panes. Reply with ONLY a 1–2 word label (max 3 words, ≤24 chars) describing what the user is doing in this pane right now. Prefer concrete nouns: PR numbers (e.g. 'review PR 1283'), branch names (e.g. 'feat/moving-button'), file names, commands. No quotes, no punctuation at the end, no explanations.";

const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_TOKENS: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cerebras,
    Anthropic,
    OpenAi,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Cerebras => "cerebras",
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
        }
    }

    pub fn env_key(self) -> &'static str {
        match self {
            Kind::Cerebras => "CEREBRAS_API_KEY",
            Kind::Anthropic => "ANTHROPIC_API_KEY",
            Kind::OpenAi => "OPENAI_API_KEY",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Kind::Cerebras => "qwen-3.8-27b",
            Kind::Anthropic => "claude-haiku-4-5",
            Kind::OpenAi => "gpt-5-mini",
        }
    }

    fn url(self) -> &'static str {
        match self {
            Kind::Cerebras => "https://api.cerebras.ai/v1/chat/completions",
            Kind::Anthropic => "https://api.anthropic.com/v1/messages",
            Kind::OpenAi => "https://api.openai.com/v1/chat/completions",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        match s {
            "cerebras" => Some(Kind::Cerebras),
            "anthropic" => Some(Kind::Anthropic),
            "openai" => Some(Kind::OpenAi),
            _ => None,
        }
    }
}

const AUTO_ORDER: [Kind; 3] = [Kind::Cerebras, Kind::Anthropic, Kind::OpenAi];

#[derive(Debug, Clone)]
pub struct Provider {
    pub kind: Kind,
    pub model: String,
    key: String,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind.name(), self.model)
    }
}

#[derive(Debug)]
pub enum Error {
    Http(String),
    Empty,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(m) => write!(f, "{m}"),
            Error::Empty => write!(f, "empty completion"),
        }
    }
}

/// Reads an API key from the environment, else from `~/.keysrc` (`export KEY=value` lines).
pub fn find_key(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name)
        && !v.trim().is_empty()
    {
        return Some(v.trim().to_string());
    }
    let text = std::fs::read_to_string(crate::herdr::home_dir().join(".keysrc")).ok()?;
    key_from_keysrc(&text, name)
}

pub fn key_from_keysrc(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != name {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// Picks the provider per config (`auto` = first of cerebras → anthropic → openai with a key).
pub fn select(config: &Config) -> Result<Option<Provider>, String> {
    let build = |kind: Kind, key: String| Provider {
        kind,
        model: config
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| kind.default_model().to_string()),
        key,
    };
    match config.provider.as_str() {
        "none" | "off" | "disabled" => Ok(None),
        "auto" | "" => Ok(AUTO_ORDER
            .into_iter()
            .find_map(|k| find_key(k.env_key()).map(|key| build(k, key)))),
        other => {
            let kind = Kind::parse(other).ok_or_else(|| format!("unknown provider {other:?}"))?;
            match find_key(kind.env_key()) {
                Some(key) => Ok(Some(build(kind, key))),
                None => Err(format!(
                    "provider {other} configured but {} not set",
                    kind.env_key()
                )),
            }
        }
    }
}

/// Context for one labelling request.
#[derive(Debug, Clone, Default)]
pub struct Context<'a> {
    pub agent: Option<&'a str>,
    pub agent_status: Option<&'a str>,
    pub process: Option<&'a str>,
    pub cwd_basename: &'a str,
    pub branch: Option<&'a str>,
    pub lines: &'a [&'a str],
}

/// Builds the user message: facts block, PR mentions, then the scrubbed screen.
pub fn user_message(ctx: &Context) -> String {
    let clean = |value: &str| scrub::scrub_line(&value.replace(['\n', '\r'], " "));
    let mut out = String::new();
    if let Some(a) = ctx.agent {
        out.push_str(&format!("agent: {}", clean(a)));
        if let Some(s) = ctx.agent_status {
            out.push_str(&format!(" ({})", clean(s)));
        }
        out.push('\n');
    }
    if let Some(p) = ctx.process {
        out.push_str(&format!("foreground process: {}\n", clean(p)));
    }
    out.push_str(&format!("cwd: {}\n", clean(ctx.cwd_basename)));
    if let Some(b) = ctx.branch {
        out.push_str(&format!("git branch: {}\n", clean(b)));
    }
    let scrubbed = scrub::scrub_lines(ctx.lines.iter().copied());
    let prs = pr_mentions(&scrubbed);
    if !prs.is_empty() {
        out.push_str(&format!("PRs mentioned: {}\n", prs.join(", ")));
    }
    out.push_str("\nlast lines of the pane:\n");
    for l in &scrubbed {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn pr_mentions(lines: &[String]) -> Vec<String> {
    let re = regex::Regex::new(r"(?i)\bPR\s*#?(\d+)|/pull/(\d+)").unwrap();
    let mut seen = Vec::new();
    for l in lines {
        for c in re.captures_iter(l) {
            let n = c
                .get(1)
                .or_else(|| c.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            let s = format!("PR {n}");
            if !n.is_empty() && !seen.contains(&s) {
                seen.push(s);
            }
        }
    }
    seen.truncate(5);
    seen
}

impl Provider {
    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent()
    }

    /// `retry` relaxes the Cerebras settings after an empty completion: qwen occasionally
    /// answers with zero tokens under `reasoning_effort: none`; a little reasoning fixes it
    /// (the reasoning lands in a separate field, `content` stays the label).
    fn request_body_with(&self, user: &str, retry: bool) -> Value {
        match self.kind {
            Kind::Anthropic => json!({
                "model": self.model,
                "max_tokens": MAX_TOKENS,
                "temperature": 0,
                "system": SYSTEM_PROMPT,
                "messages": [{"role": "user", "content": user}],
            }),
            // Cerebras' qwen models reason by default and would spend the whole token budget
            // on it; `reasoning_effort: none` turns that off.
            Kind::Cerebras => json!({
                "model": self.model,
                "max_tokens": if retry { 256 } else { MAX_TOKENS },
                "temperature": 0,
                "reasoning_effort": if retry { "low" } else { "none" },
                "messages": [
                    {"role": "system", "content": SYSTEM_PROMPT},
                    {"role": "user", "content": user},
                ],
            }),
            // gpt-5 family rejects `max_tokens` and non-default temperature.
            Kind::OpenAi => json!({
                "model": self.model,
                "max_completion_tokens": MAX_TOKENS.max(64),
                "reasoning_effort": "minimal",
                "messages": [
                    {"role": "system", "content": SYSTEM_PROMPT},
                    {"role": "user", "content": user},
                ],
            }),
        }
    }

    fn extract(&self, v: &Value) -> Option<String> {
        match self.kind {
            Kind::Anthropic => v["content"]
                .as_array()?
                .iter()
                .find_map(|c| c.get("text").and_then(Value::as_str))
                .map(str::to_string),
            Kind::Cerebras | Kind::OpenAi => v["choices"][0]["message"]["content"]
                .as_str()
                .map(str::to_string),
        }
    }

    /// Raw completion text for `user` (already scrubbed by `user_message`).
    fn complete(&self, user: &str) -> Result<String, Error> {
        match self.complete_with(user, false) {
            Err(Error::Empty) if self.kind == Kind::Cerebras => {
                crate::logging::log_debug!("empty completion, retrying with reasoning");
                self.complete_with(user, true)
            }
            other => other,
        }
    }

    fn complete_with(&self, user: &str, retry: bool) -> Result<String, Error> {
        let agent = Self::agent();
        let mut req = agent
            .post(self.kind.url())
            .header("content-type", "application/json");
        req = match self.kind {
            Kind::Anthropic => req
                .header("x-api-key", &self.key)
                .header("anthropic-version", "2023-06-01"),
            _ => req.header("authorization", &format!("Bearer {}", self.key)),
        };
        let mut resp = req
            .send_json(self.request_body_with(user, retry))
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let body: Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| Error::Http(format!("status {status}, bad body: {e}")))?;
        if status >= 300 {
            let msg = body["error"]["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("");
            return Err(Error::Http(format!("status {status}: {msg}")));
        }
        let text = self.extract(&body).unwrap_or_default();
        if text.trim().is_empty() {
            // Model responses carry no pane content, so a preview is safe to log.
            let preview: String = body.to_string().chars().take(400).collect();
            crate::logging::log_debug!("empty completion, response: {preview}");
            return Err(Error::Empty);
        }
        Ok(text)
    }

    /// Full pipeline: prompt → completion → cleaned label (empty string when unusable).
    pub fn label(&self, ctx: &Context, max_chars: usize) -> Result<String, Error> {
        let raw = self.complete(&user_message(ctx))?;
        let cleaned = postprocess(&raw, max_chars);
        if cleaned.is_empty() {
            return Err(Error::Empty);
        }
        Ok(cleaned)
    }
}

/// Strips quotes/backticks/trailing punctuation, collapses whitespace, ≤3 words, ≤`max_chars`.
pub fn postprocess(raw: &str, max_chars: usize) -> String {
    let raw = raw.trim();
    // Some models prefix "Label:" or wrap in a sentence; keep the last line that has content.
    let candidate = raw
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    let candidate = candidate
        .strip_prefix("Label:")
        .or_else(|| candidate.strip_prefix("label:"))
        .unwrap_or(candidate);
    label::finalize(candidate, 3, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysrc_parsing() {
        let text = "# keys\nexport FOO=bar\nexport CEREBRAS_API_KEY=\"csk-abc\"\nOPENAI_API_KEY='sk-x'\nEMPTY=\n";
        assert_eq!(key_from_keysrc(text, "FOO").as_deref(), Some("bar"));
        assert_eq!(
            key_from_keysrc(text, "CEREBRAS_API_KEY").as_deref(),
            Some("csk-abc")
        );
        assert_eq!(
            key_from_keysrc(text, "OPENAI_API_KEY").as_deref(),
            Some("sk-x")
        );
        assert_eq!(key_from_keysrc(text, "EMPTY"), None);
        assert_eq!(key_from_keysrc(text, "NOPE"), None);
    }

    #[test]
    fn postprocess_cases() {
        assert_eq!(postprocess("\"review PR 1283\"\n", 24), "review PR 1283");
        assert_eq!(
            postprocess("`feat/moving-button`.", 24),
            "feat/moving-button"
        );
        assert_eq!(
            postprocess("Label: fixing auth tests now", 24),
            "fixing auth tests"
        );
        assert_eq!(
            postprocess("Reviewing PR 1283 for the auth refactor", 24),
            "Reviewing PR 1283"
        );
        assert_eq!(postprocess("", 24), "");
        assert!(
            postprocess("abcdefghijklmnopqrstuvwxyz0123", 24)
                .chars()
                .count()
                <= 24
        );
    }

    #[test]
    fn user_message_scrubs_and_mentions_prs() {
        let lines = [
            "export OPENAI_API_KEY=sk-abcdefghijkl",
            "Reviewing PR #1283",
            "see https://github.com/o/r/pull/77",
        ];
        let ctx = Context {
            agent: Some("claude"),
            agent_status: Some("working"),
            process: None,
            cwd_basename: "pika",
            branch: Some("main"),
            lines: &lines,
        };
        let msg = user_message(&ctx);
        assert!(msg.contains("agent: claude (working)"));
        assert!(msg.contains("cwd: pika"));
        assert!(msg.contains("git branch: main"));
        assert!(msg.contains("PRs mentioned: PR 1283, PR 77"));
        assert!(!msg.contains("sk-abcdefghijkl"));
        assert!(msg.contains("OPENAI_API_KEY=[redacted]"));
    }

    #[test]
    fn all_prompt_facts_are_scrubbed() {
        let ctx = Context {
            agent: Some("password=hunter2"),
            agent_status: Some("token=hunter2"),
            process: Some("worker password='hunter2'"),
            cwd_basename: "secret=hunter2",
            branch: Some("api_key=hunter2"),
            lines: &["safe"],
        };
        assert!(!user_message(&ctx).contains("hunter2"));
    }

    #[test]
    fn provider_selection_none_and_unknown() {
        let mut c = Config {
            provider: "none".into(),
            ..Config::default()
        };
        assert!(select(&c).unwrap().is_none());
        c.provider = "bogus".into();
        assert!(select(&c).is_err());
    }

    #[test]
    fn response_extraction() {
        let p = Provider {
            kind: Kind::Anthropic,
            model: "m".into(),
            key: "k".into(),
        };
        let v = json!({"content": [{"type": "text", "text": "hi"}]});
        assert_eq!(p.extract(&v).as_deref(), Some("hi"));
        let p = Provider {
            kind: Kind::Cerebras,
            model: "m".into(),
            key: "k".into(),
        };
        let v = json!({"choices": [{"message": {"role": "assistant", "content": "yo"}}]});
        assert_eq!(p.extract(&v).as_deref(), Some("yo"));
        assert_eq!(p.extract(&json!({})), None);
    }

    #[test]
    fn cerebras_retry_relaxes_reasoning() {
        let p = Provider {
            kind: Kind::Cerebras,
            model: "m".into(),
            key: "k".into(),
        };
        let first = p.request_body_with("ctx", false);
        assert_eq!(first["reasoning_effort"], "none");
        assert_eq!(first["max_tokens"], MAX_TOKENS);
        let retry = p.request_body_with("ctx", true);
        assert_eq!(retry["reasoning_effort"], "low");
        assert_eq!(retry["max_tokens"], 256);
        assert_eq!(retry["messages"][1]["content"], "ctx");
    }
}
