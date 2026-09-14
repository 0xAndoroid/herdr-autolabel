//! Tiny timestamped logger to stdout (redirected to `daemon.log` by `start`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn init_from_env() {
    let level = std::env::var("AUTOLABEL_LOG").unwrap_or_default();
    DEBUG.store(
        level.eq_ignore_ascii_case("debug") || level.eq_ignore_ascii_case("trace"),
        Ordering::Relaxed,
    );
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

pub fn emit(level: &str, msg: &str) {
    println!("{} {level:<5} {msg}", timestamp());
}

macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logging::emit("INFO", &format!($($arg)*)) };
}
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logging::emit("WARN", &format!($($arg)*)) };
}
macro_rules! log_debug {
    ($($arg:tt)*) => { if $crate::logging::debug_enabled() { $crate::logging::emit("DEBUG", &format!($($arg)*)) } };
}
pub(crate) use {log_debug, log_info, log_warn};

/// RFC 3339 UTC timestamp without external crates.
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// Howard Hinnant's days → civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(20_710), (2026, 9, 14));
    }
}
