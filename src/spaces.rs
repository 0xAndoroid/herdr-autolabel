//! Space (workspace) labels: one label per sidebar space derived from the labels of its panes,
//! with hysteresis so a space does not flip on a transient command, and ownership tracking so
//! a name the user typed is never overwritten and our names are restored on stop.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::label;

/// Passes a new candidate must persist before a space that already has a label is renamed.
pub const HYSTERESIS_PASSES: u32 = 2;

/// What a pane contributes to its space's label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSummary {
    pub label: String,
    /// A coding agent (claude/codex/pika/…) runs in the pane.
    pub agent: bool,
    pub focused: bool,
    /// Branch when on a non-default branch, else the repo/cwd basename.
    pub scope: Option<String>,
}

/// Derives the space label from its panes:
/// 1. one pane → that pane's label verbatim;
/// 2. an agent pane → that pane's label (the focused agent pane when several);
/// 3. else the scope (branch, else repo/cwd) shared by most panes, when at least two share it;
/// 4. else the focused pane's label, else the first labelled pane's.
pub fn aggregate(panes: &[PaneSummary], max_chars: usize) -> Option<String> {
    let labelled: Vec<&PaneSummary> = panes.iter().filter(|p| !p.label.is_empty()).collect();
    let pick = |s: &str| Some(label::truncate_words(s, max_chars)).filter(|l| !l.is_empty());
    match labelled.as_slice() {
        [] => return None,
        [only] => return pick(&only.label),
        _ => {}
    }
    if let Some(agent) = labelled
        .iter()
        .find(|p| p.agent && p.focused)
        .or_else(|| labelled.iter().find(|p| p.agent))
    {
        return pick(&agent.label);
    }
    // Dominant scope: highest count wins, first occurrence breaks ties.
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for scope in labelled.iter().filter_map(|p| p.scope.as_deref()) {
        match counts.iter_mut().find(|(s, _)| *s == scope) {
            Some((_, n)) => *n += 1,
            None => counts.push((scope, 1)),
        }
    }
    let mut best: Option<(&str, usize)> = None;
    for (scope, n) in &counts {
        if best.is_none_or(|(_, m)| *n > m) {
            best = Some((scope, *n));
        }
    }
    if let Some((scope, n)) = best
        && n >= 2
    {
        return pick(scope);
    }
    let focused = labelled.iter().find(|p| p.focused).unwrap_or(&labelled[0]);
    pick(&focused.label)
}

/// Per-space state, persisted so a restarted daemon still recognises its own names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpaceState {
    /// The label the space had before we first renamed it (restored on stop).
    pub original: String,
    /// The label we last applied, if the space currently carries one of ours.
    pub applied: Option<String>,
    /// Candidate label waiting out the hysteresis window and the passes it has been seen.
    #[serde(skip)]
    pub pending: Option<(String, u32)>,
}

impl SpaceState {
    /// Feeds one pass's candidate; returns the label to apply now, if any. A space without one
    /// of our labels yet is renamed immediately; afterwards a new candidate must be seen on
    /// `HYSTERESIS_PASSES` consecutive passes (or `force`) before the space is renamed again.
    pub fn observe(&mut self, candidate: Option<&str>, force: bool) -> Option<String> {
        let candidate = candidate?;
        if self.applied.as_deref() == Some(candidate) {
            self.pending = None;
            return None;
        }
        if force || self.applied.is_none() {
            self.pending = None;
            return Some(candidate.to_string());
        }
        let seen = match &self.pending {
            Some((label, n)) if label == candidate => n + 1,
            _ => 1,
        };
        if seen >= HYSTERESIS_PASSES {
            self.pending = None;
            Some(candidate.to_string())
        } else {
            self.pending = Some((candidate.to_string(), seen));
            None
        }
    }
}

/// Who owns the space's current label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Our label from this or a previous run.
    Ours,
    /// herdr's default (the cwd basename) or a name we may replace.
    Default,
    /// A name the user typed; never touched.
    Manual,
}

/// Classifies a space's current label. A space we have never seen is taken over whatever it is
/// called (the name is remembered and restored on stop); afterwards a name that is neither the
/// one we applied nor the remembered original was typed by the user. `defaults` are the names
/// herdr gives spaces itself (basenames of pane cwds and their repositories, the worktree
/// name): renaming a released space back to one of those hands it back to us.
pub fn classify(current: &str, state: Option<&SpaceState>, defaults: &[String]) -> Ownership {
    let Some(state) = state.filter(|s| !s.original.is_empty()) else {
        return Ownership::Default;
    };
    if state.applied.as_deref() == Some(current) {
        return Ownership::Ours;
    }
    let current = current.trim();
    if state.applied.is_none() && state.original == current {
        return Ownership::Default;
    }
    if current.is_empty()
        || current.chars().all(|c| c.is_ascii_digit())
        || defaults.iter().any(|d| d == current)
    {
        return Ownership::Default;
    }
    Ownership::Manual
}
/// Names herdr would have given a space on its own, for `classify`.
pub fn default_names<'a>(
    cwds: impl Iterator<Item = &'a str>,
    worktree_repo: Option<&str>,
    worktree_checkout: Option<&str>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: Option<String>| {
        if let Some(n) = name.filter(|n| !n.is_empty())
            && !names.contains(&n)
        {
            names.push(n);
        }
    };
    for cwd in cwds.filter(|c| !c.is_empty()) {
        let path = Path::new(cwd.trim_end_matches('/'));
        push(path.file_name().map(|n| n.to_string_lossy().into_owned()));
        push(crate::git::repo_basename(path));
    }
    push(worktree_repo.map(str::to_string));
    push(
        worktree_checkout
            .map(|c| Path::new(c.trim_end_matches('/')))
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
    );
    names
}

/// Persisted map workspace_id → state.
pub type SpaceStates = HashMap<String, SpaceState>;

pub fn load_states(path: &Path) -> SpaceStates {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_states(path: &Path, states: &SpaceStates) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(states).unwrap_or_default();
    crate::daemon::write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(label: &str, agent: bool, focused: bool, scope: Option<&str>) -> PaneSummary {
        PaneSummary {
            label: label.into(),
            agent,
            focused,
            scope: scope.map(str::to_string),
        }
    }

    #[test]
    fn single_pane_space_takes_the_pane_label_verbatim() {
        let panes = [pane("cargo build", false, false, Some("jolt"))];
        assert_eq!(aggregate(&panes, 24).as_deref(), Some("cargo build"));
        assert_eq!(aggregate(&[], 24), None);
        assert_eq!(aggregate(&[pane("", false, true, None)], 24), None);
    }

    #[test]
    fn agent_pane_wins_over_everything() {
        let panes = [
            pane("feat/x", false, true, Some("feat/x")),
            pane("nvim foo.rs", false, false, Some("feat/x")),
            pane("fixing auth tests", true, false, Some("feat/x")),
        ];
        assert_eq!(aggregate(&panes, 24).as_deref(), Some("fixing auth tests"));
        // Several agents: the focused one.
        let panes = [
            pane("codex reviewing", true, false, None),
            pane("claude writing docs", true, true, None),
        ];
        assert_eq!(
            aggregate(&panes, 24).as_deref(),
            Some("claude writing docs")
        );
    }

    #[test]
    fn dominant_scope_across_panes() {
        let panes = [
            pane("cargo test", false, false, Some("feat/moving-button")),
            pane("nvim app.rs", false, true, Some("feat/moving-button")),
            pane("htop", false, false, Some("pika")),
        ];
        assert_eq!(aggregate(&panes, 24).as_deref(), Some("feat/moving-button"));
        // Ties resolve to the first scope seen.
        let panes = [
            pane("a", false, false, Some("pika")),
            pane("b", false, true, Some("pika")),
            pane("c", false, false, Some("jolt")),
            pane("d", false, false, Some("jolt")),
        ];
        assert_eq!(aggregate(&panes, 24).as_deref(), Some("pika"));
    }

    #[test]
    fn falls_back_to_focused_then_first_pane() {
        let panes = [
            pane("cargo test", false, false, Some("jolt")),
            pane("ssh mini", false, true, None),
            pane("htop", false, false, Some("pika")),
        ];
        assert_eq!(aggregate(&panes, 24).as_deref(), Some("ssh mini"));
        let panes = [
            pane("cargo test", false, false, Some("jolt")),
            pane("htop", false, false, Some("pika")),
        ];
        assert_eq!(aggregate(&panes, 24).as_deref(), Some("cargo test"));
    }

    #[test]
    fn labels_are_capped_at_max_chars() {
        let panes = [pane(
            "extraordinarily long agent label here",
            true,
            true,
            None,
        )];
        let out = aggregate(&panes, 24).unwrap();
        assert!(out.chars().count() <= 24, "{out}");
        assert_eq!(out, "extraordinarily long");
    }

    #[test]
    fn hysteresis_needs_two_consecutive_passes() {
        let mut s = SpaceState::default();
        // First label applies immediately.
        assert_eq!(s.observe(Some("pika"), false).as_deref(), Some("pika"));
        s.applied = Some("pika".into());
        assert_eq!(s.observe(Some("pika"), false), None);
        // A transient change is ignored …
        assert_eq!(s.observe(Some("cargo build"), false), None);
        assert_eq!(s.observe(Some("pika"), false), None);
        assert_eq!(s.pending, None);
        // … a change seen twice in a row is applied.
        assert_eq!(s.observe(Some("cargo build"), false), None);
        assert_eq!(
            s.observe(Some("cargo build"), false).as_deref(),
            Some("cargo build")
        );
        // Alternating candidates never settle.
        s.applied = Some("cargo build".into());
        assert_eq!(s.observe(Some("a"), false), None);
        assert_eq!(s.observe(Some("b"), false), None);
        assert_eq!(s.observe(Some("a"), false), None);
        // Force bypasses the window; no candidate leaves the label alone.
        assert_eq!(s.observe(Some("b"), true).as_deref(), Some("b"));
        assert_eq!(s.observe(None, false), None);
    }

    #[test]
    fn ownership_classification() {
        let defaults = default_names(
            ["/Users/x/dev/pika/crates", "/Users/x/dev/pika"].into_iter(),
            Some("jolt"),
            Some("/Users/x/wt/jolt/feat-a"),
        );
        assert!(defaults.contains(&"crates".to_string()));
        assert!(defaults.contains(&"pika".to_string()));
        assert!(defaults.contains(&"jolt".to_string()));
        assert!(defaults.contains(&"feat-a".to_string()));
        // Never seen: taken over whatever the name (herdr's default or a programmatic label).
        assert_eq!(classify("pika", None, &defaults), Ownership::Default);
        assert_eq!(classify("my project", None, &defaults), Ownership::Default);
        let blank = SpaceState::default();
        assert_eq!(
            classify("my project", Some(&blank), &[]),
            Ownership::Default
        );
        let ours = SpaceState {
            original: "pika".into(),
            applied: Some("fixing tests".into()),
            pending: None,
        };
        assert_eq!(
            classify("fixing tests", Some(&ours), &defaults),
            Ownership::Ours
        );
        // Renamed by the user after we labelled it.
        assert_eq!(
            classify("my project", Some(&ours), &defaults),
            Ownership::Manual
        );
        // Reset to a default name: ours to take again.
        assert_eq!(classify("pika", Some(&ours), &defaults), Ownership::Default);
        assert_eq!(classify("3", Some(&ours), &defaults), Ownership::Default);
        // Released earlier (applied cleared, original kept): stays the user's until it is set
        // back to the original or a default name.
        let released = SpaceState {
            original: "old-dir".into(),
            ..Default::default()
        };
        assert_eq!(
            classify("my project", Some(&released), &[]),
            Ownership::Manual
        );
        assert_eq!(
            classify("old-dir", Some(&released), &[]),
            Ownership::Default
        );
        assert_eq!(
            classify("crates", Some(&released), &defaults),
            Ownership::Default
        );
    }

    #[test]
    fn state_roundtrip_drops_pending() {
        let dir = std::env::temp_dir().join(format!("hal-spaces-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spaces.json");
        let mut states = SpaceStates::new();
        states.insert(
            "w1".into(),
            SpaceState {
                original: "pika".into(),
                applied: Some("cargo build".into()),
                pending: Some(("x".into(), 1)),
            },
        );
        save_states(&path, &states).unwrap();
        let loaded = load_states(&path);
        assert_eq!(loaded["w1"].original, "pika");
        assert_eq!(loaded["w1"].applied.as_deref(), Some("cargo build"));
        assert_eq!(loaded["w1"].pending, None);
        assert!(load_states(&dir.join("missing.json")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
