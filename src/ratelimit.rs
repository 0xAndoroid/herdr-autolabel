//! LLM call budget: per-pane minimum spacing plus a rolling one-minute global budget.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub struct RateLimiter {
    per_pane_min: Duration,
    last_call: HashMap<String, Instant>,
    capacity: usize,
    calls: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(per_pane_min: Duration, global_per_min: u32) -> Self {
        Self {
            per_pane_min: per_pane_min.max(Duration::from_secs(15)),
            last_call: HashMap::new(),
            capacity: global_per_min.clamp(1, 6) as usize,
            calls: VecDeque::new(),
        }
    }

    /// Reserves one LLM call for `pane` at `now`; returns false when either limit blocks it.
    pub fn try_acquire_at(&mut self, pane: &str, now: Instant) -> bool {
        if let Some(last) = self.last_call.get(pane)
            && now.saturating_duration_since(*last) < self.per_pane_min
        {
            return false;
        }
        while self
            .calls
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= Duration::from_secs(60))
        {
            self.calls.pop_front();
        }
        if self.calls.len() >= self.capacity {
            return false;
        }
        self.calls.push_back(now);
        self.last_call.insert(pane.to_string(), now);
        true
    }

    pub fn try_acquire(&mut self, pane: &str) -> bool {
        self.try_acquire_at(pane, Instant::now())
    }

    /// Drops state for panes that no longer exist.
    pub fn retain_panes(&mut self, alive: &dyn Fn(&str) -> bool) {
        self.last_call.retain(|k, _| alive(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_pane_spacing() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(Duration::from_secs(15), 60);
        assert!(rl.try_acquire_at("p1", t0));
        assert!(!rl.try_acquire_at("p1", t0 + Duration::from_secs(5)));
        assert!(rl.try_acquire_at("p2", t0 + Duration::from_secs(5)));
        assert!(rl.try_acquire_at("p1", t0 + Duration::from_secs(15)));
    }

    #[test]
    fn rolling_minute_caps_calls() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(Duration::ZERO, 6);
        for i in 0..6 {
            assert!(rl.try_acquire_at(&format!("p{i}"), t0), "call {i}");
        }
        assert!(!rl.try_acquire_at("p9", t0));
        assert!(!rl.try_acquire_at("p9", t0 + Duration::from_secs(10)));
        assert!(!rl.try_acquire_at("p9", t0 + Duration::from_secs(59)));
        assert!(rl.try_acquire_at("p9", t0 + Duration::from_secs(60)));
        // Never exceeds capacity after a long idle.
        assert!(rl.try_acquire_at("q0", t0 + Duration::from_secs(1000)));
        for i in 1..6 {
            assert!(rl.try_acquire_at(&format!("q{i}"), t0 + Duration::from_secs(1000)));
        }
        assert!(!rl.try_acquire_at("q6", t0 + Duration::from_secs(1000)));
    }

    #[test]
    fn blocked_call_does_not_consume_token() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(Duration::from_secs(15), 2);
        assert!(rl.try_acquire_at("p1", t0));
        assert!(!rl.try_acquire_at("p1", t0 + Duration::from_secs(1)));
        assert!(rl.try_acquire_at("p2", t0 + Duration::from_secs(1)));
        assert!(!rl.try_acquire_at("p3", t0 + Duration::from_secs(1)));
    }
}
