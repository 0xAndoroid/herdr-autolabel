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
    /// A command runs in the pane, or someone named it: the label says what is done, not only
    /// where.
    pub busy: bool,
    /// The label is a whole space name already (LLM-written, project included as far as it
    /// helps); no project is put in front of it.
    pub whole: bool,
    /// Repository checkout name, else cwd basename.
    pub project: Option<String>,
}

/// Derives the space label from the primary pane — the first agent pane, else the first busy
/// pane, else the first labelled pane; focus plays no part, so switching panes never renames
/// the space — within `max_chars` in total:
/// - a whole label (LLM-written) is used as is;
/// - otherwise `<project>: <activity>`, the project being `worktree` (herdr's repository name
///   for a worktree space) when set, else the project named by most panes (ties go to the
///   first pane), and the activity the primary pane's label cut to whole words in the room
///   the project leaves.
///
/// The project alone when the activity repeats it (idle shells at the repository root) or when
/// not even its first word fits; the activity alone when no pane has a cwd.
pub fn aggregate(
    panes: &[PaneSummary],
    worktree: Option<&str>,
    max_chars: usize,
) -> Option<String> {
    let labelled: Vec<&PaneSummary> = panes.iter().filter(|p| !p.label.is_empty()).collect();
    let primary = labelled
        .iter()
        .find(|p| p.agent)
        .or_else(|| labelled.iter().find(|p| p.busy))
        .or(labelled.first())?;
    if primary.whole {
        return Some(label::truncate_words(&primary.label, max_chars));
    }
    let project = worktree
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .or_else(|| dominant_project(&labelled));
    let Some(project) = project else {
        return Some(label::truncate_words(&primary.label, max_chars));
    };
    let room = max_chars.saturating_sub(project.chars().count() + 2);
    let first_word_fits = primary
        .label
        .split_whitespace()
        .next()
        .is_some_and(|w| w.chars().count() <= room);
    let activity = label::truncate_words(&primary.label, room);
    Some(if !first_word_fits || activity == project {
        label::truncate_words(&project, max_chars)
    } else {
        format!("{project}: {activity}")
    })
}

/// The project named by most panes; ties go to the first pane seen.
fn dominant_project(panes: &[&PaneSummary]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for project in panes.iter().filter_map(|p| p.project.as_deref()) {
        match counts.iter_mut().find(|(s, _)| *s == project) {
            Some((_, n)) => *n += 1,
            None => counts.push((project, 1)),
        }
    }
    // `max_by_key` keeps the last maximum; reversed, that is the first project seen.
    counts
        .iter()
        .rev()
        .max_by_key(|(_, n)| *n)
        .map(|(p, _)| p.to_string())
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

    fn pane(label: &str, agent: bool, busy: bool, project: Option<&str>) -> PaneSummary {
        PaneSummary {
            label: label.into(),
            agent,
            busy,
            whole: false,
            project: project.map(str::to_string),
        }
    }

    #[test]
    fn whole_labels_are_used_as_they_are() {
        let panes = [
            pane("nvim foo.rs", false, true, Some("herdr-autolabel")),
            PaneSummary {
                whole: true,
                ..pane(
                    "herdr: fix label rules",
                    true,
                    true,
                    Some("herdr-autolabel"),
                )
            },
        ];
        assert_eq!(
            aggregate(&panes, Some("herdr-autolabel"), 25).as_deref(),
            Some("herdr: fix label rules")
        );
        let panes = [PaneSummary {
            whole: true,
            ..pane("relocate worktrees to wt", true, true, Some("dev"))
        }];
        assert_eq!(
            aggregate(&panes, None, 25).as_deref(),
            Some("relocate worktrees to wt")
        );
    }

    #[test]
    fn project_then_activity() {
        let panes = [pane("cargo build", false, true, Some("jolt"))];
        assert_eq!(
            aggregate(&panes, None, 24).as_deref(),
            Some("jolt: cargo build")
        );
        // The activity alone without a cwd; the project alone when the activity repeats it.
        let panes = [pane("ssh mini", false, true, None)];
        assert_eq!(aggregate(&panes, None, 24).as_deref(), Some("ssh mini"));
        let panes = [pane("pika", false, false, Some("pika"))];
        assert_eq!(aggregate(&panes, None, 24).as_deref(), Some("pika"));
        assert_eq!(aggregate(&[], None, 24), None);
        assert_eq!(
            aggregate(&[pane("", false, true, Some("pika"))], None, 24),
            None
        );
    }

    #[test]
    fn worktree_names_the_project() {
        let panes = [pane("cargo test", false, true, Some("feat-a"))];
        assert_eq!(
            aggregate(&panes, Some("jolt"), 24).as_deref(),
            Some("jolt: cargo test")
        );
        assert_eq!(
            aggregate(&panes, Some(""), 24).as_deref(),
            Some("feat-a: cargo test")
        );
    }

    #[test]
    fn first_agent_pane_sets_the_activity() {
        let panes = [
            pane("feat/x", false, false, Some("pika")),
            pane("nvim foo.rs", false, true, Some("pika")),
            pane("fixing auth tests", true, true, Some("pika")),
        ];
        assert_eq!(
            aggregate(&panes, None, 24).as_deref(),
            Some("pika: fixing auth tests")
        );
        // Several agents: the first one, whatever is focused.
        let panes = [
            pane("wt switch", true, true, Some("pika")),
            pane("watching CI run", true, true, Some("pika")),
        ];
        assert_eq!(
            aggregate(&panes, None, 24).as_deref(),
            Some("pika: wt switch")
        );
    }

    #[test]
    fn dominant_project_then_busy_pane_then_first() {
        let panes = [
            pane("crates", false, false, Some("pika")),
            pane("htop", false, true, Some("jolt")),
            pane("cargo test", false, true, Some("pika")),
        ];
        assert_eq!(aggregate(&panes, None, 24).as_deref(), Some("pika: htop"));
        // Ties resolve to the first project seen; idle shells fall back to the first label.
        let panes = [
            pane("sub", false, false, Some("jolt")),
            pane("crates", false, false, Some("pika")),
        ];
        assert_eq!(aggregate(&panes, None, 24).as_deref(), Some("jolt: sub"));
    }

    #[test]
    fn whole_label_is_capped_at_max_chars() {
        let panes = [pane("delegating keccak PR review", true, true, Some("web"))];
        assert_eq!(
            aggregate(&panes, None, 22).as_deref(),
            Some("web: delegating keccak")
        );
        // A project that leaves no room for the activity's first word stands alone.
        let panes = [pane(
            "cargo build",
            false,
            true,
            Some("herdr-autolabel-plugin"),
        )];
        assert_eq!(
            aggregate(&panes, None, 22).as_deref(),
            Some("herdr-autolabel-plugin")
        );
        let panes = [pane("cargo build", false, true, Some("herdr-autolabel"))];
        assert_eq!(
            aggregate(&panes, None, 22).as_deref(),
            Some("herdr-autolabel: cargo")
        );
        let panes = [pane(
            "x",
            false,
            true,
            Some("a-project-name-past-the-cap-x"),
        )];
        let out = aggregate(&panes, None, 22).unwrap();
        assert!(out.chars().count() <= 22, "{out}");
        let panes = [pane(
            "a long label without a cwd anywhere",
            false,
            true,
            None,
        )];
        assert_eq!(
            aggregate(&panes, None, 22).as_deref(),
            Some("a long label without")
        );
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
