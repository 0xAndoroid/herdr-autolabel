//! LLM call budget: per-pane minimum spacing plus a global token bucket.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    per_pane_min: Duration,
    last_call: HashMap<String, Instant>,
    /// Token bucket: `tokens` refills at `per_min / 60` tokens per second up to `per_min`.
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(per_pane_min: Duration, global_per_min: u32) -> Self {
        let capacity = f64::from(global_per_min.max(1));
        Self {
            per_pane_min,
            last_call: HashMap::new(),
            capacity,
            tokens: capacity,
            refill_per_sec: capacity / 60.0,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }

    /// Reserves one LLM call for `pane` at `now`; returns false when either limit blocks it.
    pub fn try_acquire_at(&mut self, pane: &str, now: Instant) -> bool {
        if let Some(last) = self.last_call.get(pane)
            && now.saturating_duration_since(*last) < self.per_pane_min
        {
            return false;
        }
        self.refill(now);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
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
    fn global_bucket_caps_burst_and_refills() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(Duration::ZERO, 6);
        for i in 0..6 {
            assert!(rl.try_acquire_at(&format!("p{i}"), t0), "call {i}");
        }
        assert!(!rl.try_acquire_at("p9", t0));
        // 6/min → one token every 10 s.
        assert!(!rl.try_acquire_at("p9", t0 + Duration::from_secs(9)));
        assert!(rl.try_acquire_at("p9", t0 + Duration::from_secs(10)));
        assert!(!rl.try_acquire_at("p8", t0 + Duration::from_secs(10)));
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
