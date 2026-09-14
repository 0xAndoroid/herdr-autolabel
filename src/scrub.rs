//! Secret scrubbing applied to every line before it leaves the machine.

use std::sync::OnceLock;

use regex::Regex;

pub const MAX_LINES: usize = 40;
pub const MAX_LINE_CHARS: usize = 300;
const REDACTED: &str = "[redacted]";

struct Patterns {
    /// `Authorization: …` / `Cookie: …` headers: everything after the colon is replaced.
    header: Regex,
    /// `key = value` style; group 1 is the key name (kept), the value is replaced.
    keyed: Regex,
    /// Patterns whose whole match is replaced.
    whole: Vec<Regex>,
    opaque: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        header: Regex::new(r"(?i)\b((?:proxy-)?authorization|(?:set-)?cookie)(\s*:\s*).+$").unwrap(),
        keyed: Regex::new(
            r#"(?i)\b([A-Za-z0-9_-]*(?:token|api[_-]?key|secret|password|passwd|authorization|cookie)[A-Za-z0-9_-]*)(["']?\s*[=:]\s*)(?:"(?:\\.|[^"\\])*(?:"|$)|'[^']*(?:'|$)|[^\s'",&]+)"#,
        )
        .unwrap(),
        whole: [
            r#"(?i)\bbearer\s+[^\s'"]+"#,
            r"sk-ant-\S+",
            r"sk-[A-Za-z0-9_-]{8,}",
            r"\bghp_\w{20,}",
            r"\bgithub_pat_\w+",
            r"\bxox[baprs]-\S+",
            r"\bAKIA[0-9A-Z]{12,}",
            r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect(),
        opaque: Regex::new(r"[A-Za-z0-9+/=_-]{40,}").unwrap(),
    })
}

/// Scrubs one line. Also truncates to `MAX_LINE_CHARS` characters.
pub fn scrub_line(line: &str) -> String {
    let p = patterns();
    let keep_key = |caps: &regex::Captures| format!("{}{}{REDACTED}", &caps[1], &caps[2]);
    let mut out = p.header.replace_all(line, keep_key).into_owned();
    out = p.keyed.replace_all(&out, keep_key).into_owned();
    for re in &p.whole {
        if re.is_match(&out) {
            out = re.replace_all(&out, REDACTED).into_owned();
        }
    }
    out = p
        .opaque
        .replace_all(&out, |caps: &regex::Captures| {
            let value = &caps[0];
            if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                value.to_string()
            } else {
                REDACTED.to_string()
            }
        })
        .into_owned();
    truncate_chars(&out, MAX_LINE_CHARS)
}

/// Scrubs a screen: keeps the last `MAX_LINES` lines, each capped and scrubbed.
pub fn scrub_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let all: Vec<&str> = lines.into_iter().collect();
    let start = all.len().saturating_sub(MAX_LINES);
    all[start..].iter().map(|l| scrub_line(l)).collect()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(s: &str) -> bool {
        s.contains(REDACTED)
    }

    #[test]
    fn keyed_assignments_keep_key_name() {
        let out = scrub_line("export OPENAI_API_KEY=abc123xyz");
        assert_eq!(out, "export OPENAI_API_KEY=[redacted]");
        let out = scrub_line("password: hunter2");
        assert_eq!(out, "password: [redacted]");
        assert_eq!(scrub_line("token = x"), "token = [redacted]");
        assert_eq!(
            scrub_line("Authorization: Basic Zm9v"),
            "Authorization: [redacted]"
        );
        assert_eq!(scrub_line("Cookie: session=abc"), "Cookie: [redacted]");
        assert_eq!(scrub_line("passwd=  q"), "passwd=  [redacted]");
        assert!(redacted(&scrub_line("my_secret_value=deadbeef")));
    }

    #[test]
    fn quoted_secrets_and_query_tokens() {
        for line in [
            r#"export ANTHROPIC_API_KEY="short secret""#,
            r#"{"password": "hunter2"}"#,
            "password='hunter2'",
            "https://example.test/?token=hunter2&view=1",
            "Authorization: Bearer hunter2",
            r#"password="hunter2"#,
        ] {
            let clean = scrub_line(line);
            assert!(
                !clean.contains("hunter2") && !clean.contains("short secret"),
                "{clean}"
            );
            assert!(redacted(&clean));
        }
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(scrub_line(sha), sha);
        assert_eq!(scrub_line(&format!("token={sha}")), "token=[redacted]");
    }

    #[test]
    fn bearer() {
        assert_eq!(
            scrub_line("curl -H 'Bearer abcdef' x"),
            "curl -H '[redacted]' x"
        );
    }

    #[test]
    fn provider_keys() {
        assert!(redacted(&scrub_line("key sk-abcdefgh1234 here")));
        assert!(redacted(&scrub_line("sk-ant-api03-foo")));
        assert!(redacted(&scrub_line("ghp_abcdefghijklmnopqrstuvwxyz")));
        assert!(redacted(&scrub_line("github_pat_11AAAA_bbbb")));
        assert!(redacted(&scrub_line("xoxb-123-456-abc")));
        assert!(redacted(&scrub_line("AKIAIOSFODNN7EXAMPLE")));
        for s in [
            "sk-ant-api03-foo",
            "ghp_abcdefghijklmnopqrstuvwxyz",
            "xoxb-123-456-abc",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert_eq!(scrub_line(s), REDACTED, "{s}");
        }
    }

    #[test]
    fn jwt_and_private_key() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        assert_eq!(scrub_line(&format!("jwt {jwt}")), "jwt [redacted]");
        assert_eq!(scrub_line("-----BEGIN RSA PRIVATE KEY-----"), REDACTED);
        assert_eq!(scrub_line("-----BEGIN PRIVATE KEY-----"), REDACTED);
    }

    #[test]
    fn long_opaque_runs() {
        let run = "Z".repeat(40);
        assert_eq!(
            scrub_line(&format!("hash {run} end")),
            "hash [redacted] end"
        );
        let short = "A".repeat(39);
        assert_eq!(scrub_line(&short), short);
    }

    #[test]
    fn benign_lines_untouched() {
        for s in [
            "cargo build --release",
            "   Compiling herdr-autolabel v0.1.0 (/Users/me/dev/herdr-autolabel)",
            "review PR 1283 for the auth refactor",
            "git checkout feat/moving-button",
            "The token count is 42",
            "❯ ",
        ] {
            assert_eq!(scrub_line(s), s);
        }
    }

    #[test]
    fn caps_lines_and_length() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let out = scrub_lines(lines.iter().map(String::as_str));
        assert_eq!(out.len(), MAX_LINES);
        assert_eq!(out[0], "line 10");
        let long = "word ".repeat(100);
        assert_eq!(scrub_line(&long).chars().count(), MAX_LINE_CHARS);
    }
}
