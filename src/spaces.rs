//! Workspace context, fallback names, and ownership of applied names.

use std::collections::HashMap;
use std::hash::Hasher;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fingerprint::Fnv;
use crate::label;

/// Passes a new candidate must persist before a space that already has a label is renamed.
pub const HYSTERESIS_PASSES: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneSummary {
    pub label: String,
    pub project: Option<String>,
    pub context: String,
    pub fingerprint: u64,
}

pub fn fallback(panes: &[(&str, &PaneSummary)], max_chars: usize) -> Option<String> {
    let mut labels: Vec<&str> = panes
        .iter()
        .map(|(_, p)| p.label.as_str())
        .filter(|s| !s.is_empty() && *s != "shell")
        .collect();
    labels.sort_unstable();
    labels.dedup();
    let name = match labels.as_slice() {
        [] if panes.is_empty() => return None,
        [] => "shell",
        [one] => one,
        _ => "multiple tasks",
    };
    Some(label::truncate_words(name, max_chars))
}

pub fn fingerprint(panes: &[(&str, &PaneSummary)]) -> u64 {
    let mut hash = Fnv::new();
    hash.write(b"workspace\0");
    for (id, pane) in panes {
        hash.write(id.as_bytes());
        hash.write_u8(0);
        hash.write_u64(pane.fingerprint);
        hash.write(pane.label.as_bytes());
        hash.write_u8(0);
    }
    hash.finish()
}

pub fn dominant_project<'a>(panes: &[(&str, &'a PaneSummary)]) -> Option<&'a str> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for project in panes.iter().filter_map(|(_, p)| p.project.as_deref()) {
        match counts.iter_mut().find(|(s, _)| *s == project) {
            Some((_, n)) => *n += 1,
            None => counts.push((project, 1)),
        }
    }
    // max_by_key keeps the last tie; reversing selects the first pane's project.
    counts.iter().rev().max_by_key(|(_, n)| *n).map(|(p, _)| *p)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
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
        if path == crate::herdr::home_dir() {
            push(Some("~".into()));
        }
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
    #![expect(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn fallback_and_fingerprint_include_every_pane() {
        let first = PaneSummary {
            label: "fix tests".into(),
            project: Some("jolt".into()),
            fingerprint: 1,
            ..Default::default()
        };
        let mut second = PaneSummary {
            label: "review PR 25".into(),
            project: Some("jolt".into()),
            fingerprint: 2,
            ..Default::default()
        };
        let panes = [("p1", &first), ("p2", &second)];
        assert_eq!(fallback(&panes, 25).as_deref(), Some("multiple tasks"));
        assert_eq!(dominant_project(&panes), Some("jolt"));
        let before = fingerprint(&panes);
        second.fingerprint = 3;
        assert_ne!(before, fingerprint(&[("p1", &first), ("p2", &second)]));
        assert_ne!(before, fingerprint(&[("p1", &first)]));
        assert_eq!(
            fallback(&[("p1", &first)], 25).as_deref(),
            Some("fix tests")
        );
        assert_eq!(fallback(&[], 25), None);
    }

    #[test]
    fn hysteresis_needs_two_consecutive_passes() {
        let mut s = SpaceState::default();
        assert_eq!(s.observe(Some("pika"), false).as_deref(), Some("pika"));
        s.applied = Some("pika".into());
        assert_eq!(s.observe(Some("pika"), false), None);
        assert_eq!(s.observe(Some("cargo build"), false), None);
        assert_eq!(s.observe(Some("pika"), false), None);
        assert_eq!(s.pending, None);
        assert_eq!(s.observe(Some("cargo build"), false), None);
        assert_eq!(
            s.observe(Some("cargo build"), false).as_deref(),
            Some("cargo build")
        );
        s.applied = Some("cargo build".into());
        assert_eq!(s.observe(Some("a"), false), None);
        assert_eq!(s.observe(Some("b"), false), None);
        assert_eq!(s.observe(Some("a"), false), None);
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
        assert_eq!(
            classify("my project", Some(&ours), &defaults),
            Ownership::Manual
        );
        assert_eq!(classify("pika", Some(&ours), &defaults), Ownership::Default);
        assert_eq!(classify("3", Some(&ours), &defaults), Ownership::Default);
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
    fn home_alias_is_a_default_workspace_name() {
        let home = crate::herdr::home_dir();
        let defaults = default_names([home.to_str().unwrap()].into_iter(), None, None);
        for applied in [Some("home".into()), None] {
            let state = SpaceState {
                original: "dev".into(),
                applied,
                pending: None,
            };
            assert_eq!(classify("~", Some(&state), &defaults), Ownership::Default);
            assert_eq!(
                classify("my project", Some(&state), &defaults),
                Ownership::Manual
            );
        }
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
