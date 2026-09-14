//! Pane fingerprint: cheap change detection so unchanged panes cost nothing.

use std::hash::Hasher;

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

/// What identifies the task in a pane, hashed into a `u64`. Screen text is deliberately
/// absent: a pane changes when a new foreground command starts, when an agent flips between
/// working and idle, or when the request changes: the user's last `prompt` to Claude (from its
/// transcript), else the `title` a coding agent keeps in the terminal title. A known prompt
/// stands in for the process, the idle state and the title as well: the pane changes when the
/// user asks for something new, not while the agent works on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub process: Vec<String>,
    pub cwd: String,
    pub branch: Option<String>,
    pub agent: Option<String>,
    pub idle: bool,
    pub prompt: Option<String>,
    pub title: Option<String>,
}

impl Fingerprint {
    pub fn hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.write(self.cwd.as_bytes());
        h.write_u8(1);
        h.write(self.branch.as_deref().unwrap_or("").as_bytes());
        h.write_u8(1);
        h.write(self.agent.as_deref().unwrap_or("").as_bytes());
        h.write_u8(1);
        if let Some(prompt) = &self.prompt {
            h.write_u8(2);
            h.write(prompt.as_bytes());
            return h.finish();
        }
        for p in &self.process {
            h.write(p.as_bytes());
            h.write_u8(0);
        }
        h.write_u8(1);
        h.write_u8(u8::from(self.idle));
        h.write(self.title.as_deref().unwrap_or("").as_bytes());
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_fields_matter() {
        let base = Fingerprint {
            process: vec!["cargo".into(), "build".into()],
            cwd: "/x".into(),
            branch: Some("main".into()),
            ..Fingerprint::default()
        };
        let mut other = base.clone();
        assert_eq!(base.hash(), other.hash());
        other.branch = Some("dev".into());
        assert_ne!(base.hash(), other.hash());
        other = base.clone();
        other.idle = true;
        assert_ne!(base.hash(), other.hash());
        other = base.clone();
        other.process = vec!["cargo".into(), "test".into()];
        assert_ne!(base.hash(), other.hash());
        other = base.clone();
        other.title = Some("Review PR".into());
        assert_ne!(base.hash(), other.hash());
    }

    #[test]
    fn prompt_replaces_process_idle_state_and_title() {
        let base = Fingerprint {
            agent: Some("claude".into()),
            prompt: Some("review PR 12".into()),
            ..Fingerprint::default()
        };
        let busy = Fingerprint {
            process: vec!["claude".into()],
            title: Some("Reviewing a PR".into()),
            ..base.clone()
        };
        let done = Fingerprint {
            idle: true,
            ..busy.clone()
        };
        assert_eq!(base.hash(), busy.hash());
        assert_eq!(base.hash(), done.hash());
        let asked_again = Fingerprint {
            prompt: Some("now fix the docs".into()),
            ..base.clone()
        };
        assert_ne!(base.hash(), asked_again.hash());
        let cd = Fingerprint {
            cwd: "/elsewhere".into(),
            ..base.clone()
        };
        assert_ne!(base.hash(), cd.hash());
    }

    #[test]
    fn fnv_is_deterministic() {
        assert_eq!(hash_str("abc"), hash_str("abc"));
        assert_ne!(hash_str("abc"), hash_str("abx"));
    }
}
