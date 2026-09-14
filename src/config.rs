//! Plugin configuration (`$HERDR_PLUGIN_CONFIG_DIR/config.toml`). Every key is optional.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub interval_secs: u64,
    pub provider: String,
    pub model: Option<String>,
    pub max_chars: usize,
    pub lines: u32,
    pub llm_per_pane_secs: u64,
    pub llm_global_per_min: u32,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_secs: 10,
            provider: "auto".into(),
            model: None,
            max_chars: 24,
            lines: 40,
            llm_per_pane_secs: 15,
            llm_global_per_min: 6,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut values: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut config = Self::default();
        macro_rules! read_fields {
            ($($field:ident),+ $(,)?) => {$(
                if let Some(value) = values.remove(stringify!($field)) {
                    match value.try_into() {
                        Ok(value) => config.$field = value,
                        Err(e) => crate::logging::log_warn!("config {}: {e}; using default", stringify!($field)),
                    }
                }
            )+};
        }
        read_fields!(
            interval_secs,
            provider,
            model,
            max_chars,
            lines,
            llm_per_pane_secs,
            llm_global_per_min,
            allow,
            deny
        );
        config.sanitize();
        Ok(config)
    }

    /// Loads the config file; a missing file yields defaults, a broken one an error.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    fn sanitize(&mut self) {
        let defaults = Self::default();
        macro_rules! validate {
            ($field:ident, $range:expr) => {
                if !$range.contains(&self.$field) {
                    crate::logging::log_warn!(
                        "config {} out of range; using default",
                        stringify!($field)
                    );
                    self.$field = defaults.$field;
                }
            };
        }
        validate!(interval_secs, 2..=86400);
        validate!(max_chars, 4..=80);
        validate!(lines, 1..=200);
        validate!(llm_per_pane_secs, 15..=86400);
        validate!(llm_global_per_min, 1..=6);
        self.provider = self.provider.trim().to_ascii_lowercase();
    }

    /// True when the pane passes the allow/deny globs. `candidates` are the strings a glob may
    /// match: pane id, workspace id and cwd.
    pub fn permits(&self, candidates: &[&str]) -> bool {
        let matches = |globs: &[String]| {
            globs
                .iter()
                .any(|g| candidates.iter().any(|c| glob_match(g, c)))
        };
        if matches(&self.deny) {
            return false;
        }
        self.allow.is_empty() || matches(&self.allow)
    }
}

/// Minimal glob: `*` matches any run (including `/`), `?` one char, everything else literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_yields_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.interval_secs, 10);
        assert_eq!(c.provider, "auto");
        assert_eq!(c.model, None);
        assert_eq!(c.max_chars, 24);
        assert_eq!(c.lines, 40);
        assert_eq!(c.llm_per_pane_secs, 15);
        assert_eq!(c.llm_global_per_min, 6);
        assert!(c.allow.is_empty() && c.deny.is_empty());
    }

    #[test]
    fn partial_file_overrides_only_given_keys() {
        let c = Config::parse("interval_secs = 30\nprovider = \"Anthropic\"\ndeny = [\"w9*\"]\n")
            .unwrap();
        assert_eq!(c.interval_secs, 30);
        assert_eq!(c.provider, "anthropic");
        assert_eq!(c.deny, vec!["w9*".to_string()]);
        assert_eq!(c.max_chars, 24);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let c = Config::parse("bogus = 1\nprovider = \"none\"\n").unwrap();
        assert_eq!(c.provider, "none");
    }

    #[test]
    fn invalid_fields_preserve_valid_settings() {
        let c = Config::parse("provider = \"none\"\nmax_chars = \"bad\"\ninterval_secs = 0\nllm_per_pane_secs = 0\nllm_global_per_min = 100").unwrap();
        assert_eq!(c.provider, "none");
        assert_eq!(c.max_chars, 24);
        assert_eq!(c.interval_secs, 10);
        assert_eq!(c.llm_per_pane_secs, 15);
        assert_eq!(c.llm_global_per_min, 6);
    }

    #[test]
    fn missing_file_is_defaults() {
        let c = Config::load(Path::new("/nonexistent/herdr-autolabel/config.toml")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn allow_deny_globs() {
        let c = Config::parse("allow = [\"w1*\", \"*/dev/*\"]\ndeny = [\"w1:p3\"]\n").unwrap();
        assert!(c.permits(&["w1:p1", "w1", "/Users/me/dev/x"]));
        assert!(!c.permits(&["w1:p3", "w1", "/Users/me/dev/x"]));
        assert!(c.permits(&["w2:p1", "w2", "/Users/me/dev/x"]));
        assert!(!c.permits(&["w2:p1", "w2", "/Users/me/other"]));
        let open = Config::default();
        assert!(open.permits(&["w7:p7", "w7", "/"]));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", ""));
        assert!(glob_match("w?:p1", "w1:p1"));
        assert!(!glob_match("w?:p1", "w10:p1"));
        assert!(glob_match("*.log", "a/b/c.log"));
        assert!(!glob_match("abc", "abcd"));
    }
}
