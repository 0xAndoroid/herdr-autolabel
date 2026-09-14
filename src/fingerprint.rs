//! Pane fingerprint: cheap change detection so unchanged panes cost nothing.

use std::hash::Hasher;
use std::sync::OnceLock;

use regex::Regex;

/// FNV-1a 64-bit; stable across runs (unlike `DefaultHasher`).
#[derive(Default)]
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

pub fn hash_str(s: &str) -> u64 {
    let mut h = Fnv::new();
    h.write(s.as_bytes());
    h.finish()
}

/// Everything that identifies "what is happening" in a pane. Hashed into a `u64`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub process: Vec<String>,
    pub cwd: String,
    pub branch: Option<String>,
    pub agent: Option<String>,
    pub agent_status: String,
    pub screen: u64,
}

impl Fingerprint {
    pub fn hash(&self) -> u64 {
        let mut h = Fnv::new();
        for p in &self.process {
            h.write(p.as_bytes());
            h.write_u8(0);
        }
        h.write_u8(1);
        h.write(self.cwd.as_bytes());
        h.write_u8(1);
        h.write(self.branch.as_deref().unwrap_or("").as_bytes());
        h.write_u8(1);
        h.write(self.agent.as_deref().unwrap_or("").as_bytes());
        h.write_u8(1);
        h.write(self.agent_status.as_bytes());
        h.write_u64(self.screen);
        h.finish()
    }
}

fn noise_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Lines that are only spinner/progress glyphs, digits (already `#`), units and
        // punctuation, or agent status chatter whose verb rotates each tick.
        let glyphs = concat!(
            r"[\s#%.,:;/\\|_=+*~<>()\[\]{}'`",
            "─-╿▀-▟⠀-⣿◐◓◑◒●○◉◎•·∙⋯…✻✽✶✳✢✦✧⏳⌛↑↓→←▸▹►▶⏵✓✔✗✘-]"
        );
        let units = r"\b(?:ms|s|m|h|eta|[kmgt]i?b|k|kb|mb|gb|tokens?)\b";
        Regex::new(&format!(
            r"(?i)^(?:{glyphs}|{units})*$|esc to interrupt|ctrl\+c to interrupt"
        ))
        .unwrap()
    })
}

/// Braille spinners, block/progress elements and the star spinners agents draw.
fn is_spinner_glyph(c: char) -> bool {
    matches!(c, '\u{2800}'..='\u{28FF}' | '\u{2580}'..='\u{259F}')
        || "◐◓◑◒✻✽✶✳✢✦✧·∙⏳⌛".contains(c)
}

/// Normalises one screen line: digits → `#`, whitespace collapsed. Returns `None` when the line is
/// noise (empty, only progress glyphs/counters, rotating agent status chatter).
pub fn normalize_line(line: &str) -> Option<String> {
    let digits_folded: String = line
        .chars()
        .filter(|c| !is_spinner_glyph(*c))
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect();
    let collapsed = digits_folded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() || noise_re().is_match(&collapsed) {
        None
    } else {
        Some(collapsed)
    }
}

/// Hash of the normalised screen text.
pub fn screen_hash<'a>(lines: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut h = Fnv::new();
    for line in lines.into_iter().filter_map(normalize_line) {
        h.write(line.as_bytes());
        h.write_u8(b'\n');
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_and_whitespace_are_folded() {
        assert_eq!(
            normalize_line("  Compiling   foo v1.2.3 (12/40) ").as_deref(),
            Some("Compiling foo v#.#.# (##/##)")
        );
    }

    #[test]
    fn spinner_and_progress_lines_are_noise() {
        for l in [
            "",
            "   ",
            "⠋",
            "⠙ ",
            "[=====>      ] 45%",
            "████████░░░░░░ 62%",
            "12.5 MB / 40 MB  eta 3s",
            "✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)",
            "· Brewing… (3s · esc to interrupt)",
            "----------------",
            "|/-\\",
        ] {
            assert_eq!(normalize_line(l), None, "{l:?}");
        }
    }

    #[test]
    fn real_content_survives() {
        assert!(normalize_line("error[E0308]: mismatched types").is_some());
        assert!(normalize_line("❯ cargo build").is_some());
        assert!(normalize_line("Reviewing PR #1283").is_some());
    }

    #[test]
    fn hash_stable_under_churn() {
        let a = [
            "❯ cargo test",
            "running 12 tests",
            "⠋ building",
            "[==>   ] 20%",
        ];
        let b = [
            "❯ cargo test",
            "running 17 tests",
            "⠸ building",
            "[=====>] 90%",
        ];
        assert_eq!(screen_hash(a), screen_hash(b));
    }

    #[test]
    fn hash_changes_on_real_change() {
        let a = ["❯ cargo test", "running 12 tests"];
        let b = ["❯ cargo test", "test result: FAILED"];
        assert_ne!(screen_hash(a), screen_hash(b));
    }

    #[test]
    fn fingerprint_fields_matter() {
        let base = Fingerprint {
            process: vec!["cargo".into(), "build".into()],
            cwd: "/x".into(),
            branch: Some("main".into()),
            agent: None,
            agent_status: "unknown".into(),
            screen: 7,
        };
        let mut other = base.clone();
        assert_eq!(base.hash(), other.hash());
        other.branch = Some("dev".into());
        assert_ne!(base.hash(), other.hash());
        other = base.clone();
        other.agent_status = "working".into();
        assert_ne!(base.hash(), other.hash());
        other = base.clone();
        other.process = vec!["cargo".into(), "test".into()];
        assert_ne!(base.hash(), other.hash());
    }

    #[test]
    fn fnv_is_deterministic() {
        assert_eq!(hash_str("abc"), hash_str("abc"));
        assert_ne!(hash_str("abc"), hash_str("abx"));
    }
}
