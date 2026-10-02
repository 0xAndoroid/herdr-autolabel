//! Shared LLM call budget, including one-shot runs and daemon restarts.

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A failed or rejected call holds its target off this long, instead of `per_pane_min`.
const FAILURE_BACKOFF_MS: u64 = 15_000;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Budget {
    /// Earliest time each pane or workspace may call again.
    next_call: HashMap<String, u64>,
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
            per_pane_min: per_pane_min.max(Duration::from_secs(3)),
            capacity: global_per_min.clamp(1, 6) as usize,
            path,
        }
    }

    fn update(
        &self,
        now: u64,
        update: impl FnOnce(&mut Budget) -> bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
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
        budget.next_call.retain(|_, next| *next > now);
        while budget
            .calls
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= 60_000)
        {
            budget.calls.pop_front();
        }
        if !update(&mut budget) {
            return Ok(false);
        }
        crate::daemon::write_atomic(&self.path, &serde_json::to_vec(&budget)?)?;
        Ok(true)
    }

    fn acquire_at(&self, pane: &str, now: u64) -> Result<bool, Box<dyn std::error::Error>> {
        self.update(now, |budget| {
            if budget.next_call.contains_key(pane) || budget.calls.len() >= self.capacity {
                return false;
            }
            budget
                .next_call
                .insert(pane.to_string(), now + self.per_pane_min.as_millis() as u64);
            budget.calls.push_back(now);
            true
        })
    }

    fn back_off_at(&self, pane: &str, now: u64) -> Result<bool, Box<dyn std::error::Error>> {
        self.update(now, |budget| {
            budget
                .next_call
                .insert(pane.to_string(), now + FAILURE_BACKOFF_MS);
            true
        })
    }

    pub fn try_acquire(&self, pane: &str) -> bool {
        match self.acquire_at(pane, now_ms()) {
            Ok(allowed) => allowed,
            Err(e) => {
                crate::logging::log_warn!("LLM budget unavailable ({e}); skipping call");
                false
            }
        }
    }

    /// Call after a failed or rejected request. Retries briefly while another process holds
    /// the budget lock: a dropped backoff would let the target retry after `per_pane_min`.
    pub fn back_off(&self, pane: &str) {
        let mut attempts = 1;
        while let Err(e) = self.back_off_at(pane, now_ms()) {
            if attempts == 5 {
                crate::logging::log_warn!("LLM budget unavailable ({e}); backoff not recorded");
                return;
            }
            attempts += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used)]

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
    fn per_pane_spacing_of_three_seconds() {
        let path = std::env::temp_dir().join(format!("hal-budget-3s-{}.json", std::process::id()));
        let rl = RateLimiter::new(Duration::from_secs(3), 6, path);
        assert!(rl.acquire_at("p1", 0).unwrap());
        assert!(!rl.acquire_at("p1", 2_999).unwrap());
        assert!(rl.acquire_at("p1", 3_000).unwrap());
        clean(&rl);
    }

    #[test]
    fn failed_call_backs_off_its_pane_for_fifteen_seconds() {
        let path = std::env::temp_dir().join(format!("hal-budget-bo-{}.json", std::process::id()));
        let rl = RateLimiter::new(Duration::from_secs(3), 6, path);
        assert!(rl.acquire_at("p1", 0).unwrap());
        rl.back_off_at("p1", 1_000).unwrap();
        assert!(!rl.acquire_at("p1", 4_000).unwrap());
        assert!(rl.acquire_at("p2", 4_000).unwrap());
        assert!(!rl.acquire_at("p1", 15_999).unwrap());
        assert!(rl.acquire_at("p1", 16_000).unwrap());
        clean(&rl);
    }

    #[test]
    fn backoff_waits_out_a_briefly_held_lock() {
        let path =
            std::env::temp_dir().join(format!("hal-budget-held-{}.json", std::process::id()));
        let rl = RateLimiter::new(Duration::from_secs(3), 6, path);
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(rl.path.with_extension("lock"))
            .unwrap();
        // SAFETY: `holder` is open for the duration of the call.
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            drop(holder);
        });
        rl.back_off("p1");
        release.join().unwrap();
        let now = now_ms();
        assert!(!rl.acquire_at("p1", now + 4_000).unwrap());
        assert!(rl.acquire_at("p1", now + 15_000).unwrap());
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
