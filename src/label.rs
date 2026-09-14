//! Label post-processing shared by heuristics and the LLM path.

/// Cleans a raw label: strips quotes/backticks/trailing punctuation, collapses whitespace, cuts
/// to `max_words` words and then to `max_chars` characters (at a word boundary when possible).
pub fn finalize(raw: &str, max_words: usize, max_chars: usize) -> String {
    let first_line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let cleaned: String = first_line
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    words.truncate(max_words);
    let joined = words.join(" ");
    let trimmed = joined.trim_end_matches(|c: char| {
        matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '…' | '-' | '—')
    });
    truncate_words(trimmed.trim(), max_chars)
}

/// Truncates to at most `max_chars` characters, preferring a word boundary.
pub fn truncate_words(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let hard: String = s.chars().take(max_chars).collect();
    match hard.rfind(' ') {
        // Keep at least half the budget so we don't collapse to a lone short word.
        Some(idx) if idx >= max_chars / 2 => hard[..idx].trim_end().to_string(),
        _ => hard.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_example_truncates_at_word_boundary() {
        let out = finalize("Reviewing PR 1283 for the auth refactor", 3, 24);
        assert_eq!(out, "Reviewing PR 1283");
        assert!(out.chars().count() <= 24);
        let out = finalize("Reviewing PR 1283 for the auth refactor", 10, 24);
        assert!(out.chars().count() <= 24);
        assert!(!out.ends_with(' '));
        assert_eq!(out, "Reviewing PR 1283 for");
    }

    #[test]
    fn strips_quotes_and_punctuation() {
        assert_eq!(finalize("\"review PR 1283.\"", 3, 24), "review PR 1283");
        assert_eq!(finalize("`cargo build`\n\nextra", 3, 24), "cargo build");
        assert_eq!(
            finalize("  feat/moving-button!  ", 3, 24),
            "feat/moving-button"
        );
        assert_eq!(finalize("Label: fix tests", 3, 24), "Label: fix tests");
    }

    #[test]
    fn collapses_whitespace_and_word_count() {
        assert_eq!(finalize("a   b\tc d e", 3, 24), "a b c");
    }

    #[test]
    fn hard_cut_when_no_boundary() {
        let out = finalize("supercalifragilisticexpialidocious", 3, 10);
        assert_eq!(out, "supercalif");
        // A boundary too early falls back to a hard cut.
        assert_eq!(
            truncate_words("a bcdefghijklmnopqrstuvwxyz", 10),
            "a bcdefghi"
        );
    }

    #[test]
    fn empty_input() {
        assert_eq!(finalize("", 3, 24), "");
        assert_eq!(finalize("\n\"\"\n", 3, 24), "");
    }
}
