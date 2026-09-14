//! Shared LLM call budget, including one-shot runs and daemon restarts.

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
struct Budget {
    last_call: HashMap<String, u64>,
    calls: VecDeque<u64>,
}

pub struct RateLimiter {
    per_pane_min: Duration,
    capacity: usize,
    path: PathBuf,
}

impl RateLimiter {
    pub fn new(per_pane_min: Duration, global_per_min: u32, path: PathBuf) -> Self {
        Self {
            per_pane_min: per_pane_min.max(Duration::from_secs(15)),
            capacity: global_per_min.clamp(1, 6) as usize,
            path,
        }
    }

    fn acquire_at(&self, pane: &str, now: u64) -> Result<bool, Box<dyn std::error::Error>> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        // SAFETY: this descriptor stays open until the budget update completes.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut budget: Budget = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Budget::default(),
            Err(e) => return Err(e.into()),
        };
        budget
            .last_call
            .retain(|_, last| now.saturating_sub(*last) < 86_400_000);
        while budget
            .calls
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= 60_000)
        {
            budget.calls.pop_front();
        }
        if budget.last_call.get(pane).is_some_and(|last| {
            u128::from(now.saturating_sub(*last)) < self.per_pane_min.as_millis()
        }) || budget.calls.len() >= self.capacity
        {
            return Ok(false);
        }
        budget.last_call.insert(pane.to_string(), now);
        budget.calls.push_back(now);
        crate::daemon::write_atomic(&self.path, &serde_json::to_vec(&budget)?)?;
        Ok(true)
    }

    pub fn try_acquire(&self, pane: &str) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        match self.acquire_at(pane, now) {
            Ok(allowed) => allowed,
            Err(e) => {
                crate::logging::log_warn!("LLM budget unavailable ({e}); skipping call");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(name: &str, capacity: u32) -> RateLimiter {
        RateLimiter::new(
            Duration::from_secs(15),
            capacity,
            std::env::temp_dir().join(format!("hal-budget-{name}-{}.json", std::process::id())),
        )
    }

    fn clean(rl: &RateLimiter) {
        let _ = std::fs::remove_file(&rl.path);
        let _ = std::fs::remove_file(rl.path.with_extension("lock"));
    }

    #[test]
    fn per_pane_spacing_survives_new_instances() {
        let rl = limiter("spacing", 6);
        assert!(rl.acquire_at("p1", 0).unwrap());
        let other = limiter("spacing", 6);
        assert!(!other.acquire_at("p1", 5_000).unwrap());
        assert!(other.acquire_at("p2", 5_000).unwrap());
        assert!(other.acquire_at("p1", 15_000).unwrap());
        clean(&rl);
    }

    #[test]
    fn rolling_minute_caps_calls_across_instances() {
        let rl = limiter("rolling", 6);
        for i in 0..6 {
            assert!(rl.acquire_at(&format!("p{i}"), 0).unwrap());
        }
        let other = limiter("rolling", 6);
        assert!(!other.acquire_at("p9", 10_000).unwrap());
        assert!(!other.acquire_at("p9", 59_999).unwrap());
        assert!(other.acquire_at("p9", 60_000).unwrap());
        clean(&rl);
    }

    #[test]
    fn blocked_call_does_not_consume_budget() {
        let rl = limiter("blocked", 2);
        assert!(rl.acquire_at("p1", 0).unwrap());
        assert!(!rl.acquire_at("p1", 1_000).unwrap());
        assert!(rl.acquire_at("p2", 1_000).unwrap());
        assert!(!rl.acquire_at("p3", 1_000).unwrap());
        clean(&rl);
    }

    #[test]
    fn unavailable_budget_fails_closed() {
        let rl = limiter("corrupt", 6);
        std::fs::write(&rl.path, "invalid").unwrap();
        assert!(rl.acquire_at("p1", 0).is_err());
        clean(&rl);
    }
}
