//! The labelling loop: snapshot → per-pane facts → fingerprint → heuristics/LLM → report title.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::config::Config;
use crate::fingerprint::{self, Fingerprint};
use crate::herdr::{self, Client, PaneInfo};
use crate::heuristics::{self, Decision, PaneFacts, Proc};
use crate::llm;
use crate::logging::{log_debug, log_info, log_warn};
use crate::ratelimit::RateLimiter;

const MAX_CONNECT_FAILURES: u32 = 3;
const CACHE_CAPACITY: usize = 256;

pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Where the daemon keeps its files.
#[derive(Debug, Clone)]
pub struct Paths {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Paths {
    pub fn resolve(socket_override: Option<PathBuf>) -> Self {
        let socket = socket_override.unwrap_or_else(herdr::default_socket_path);
        let home = herdr::home_dir();
        let state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state/herdr-autolabel"));
        let config_dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/herdr-autolabel"));
        Self {
            socket,
            state_dir,
            config_dir,
        }
    }

    pub fn pidfile(&self) -> PathBuf {
        let h = fingerprint::hash_str(&self.socket.to_string_lossy());
        self.state_dir.join(format!("daemon-{h:016x}.pid"))
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    pub fn status_file(&self) -> PathBuf {
        self.state_dir.join("status.json")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Heuristic,
    Llm,
    Cache,
    Fallback,
    RateLimited,
    Unchanged,
    SkippedManual,
    SkippedFilter,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub pane_id: String,
    pub label: Option<String>,
    pub source: Source,
    pub applied: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PassStats {
    pub panes: usize,
    pub labeled: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub llm_calls: usize,
    pub llm_errors: usize,
    pub duration_ms: u128,
}

struct LabelCache {
    map: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl LabelCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: u64) -> Option<String> {
        let v = self.map.get(&key).cloned()?;
        // Move to back (most recently used).
        if let Some(pos) = self.order.iter().position(|k| *k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
        Some(v)
    }

    fn insert(&mut self, key: u64, value: String) {
        if self.map.insert(key, value).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

pub struct Daemon {
    pub paths: Paths,
    pub config: Config,
    pub provider: Option<llm::Provider>,
    client: Client,
    limiter: RateLimiter,
    cache: LabelCache,
    /// Last processed fingerprint per pane.
    last_fp: HashMap<String, u64>,
    /// Title we last reported per pane.
    applied: HashMap<String, String>,
    /// Label we already re-applied once after seeing a mismatching snapshot title.
    reapplied: HashMap<String, String>,
    /// Panes whose title we cleared because of a manual label.
    cleared: HashSet<String>,
    started_at: String,
    pass_count: u64,
    total_llm_calls: u64,
    total_applied: u64,
    last_error: Option<String>,
}

impl Daemon {
    pub fn new(paths: Paths, config: Config, provider: Option<llm::Provider>) -> Self {
        let limiter = RateLimiter::new(
            Duration::from_secs(config.llm_per_pane_secs),
            config.llm_global_per_min,
        );
        Self {
            client: Client::new(paths.socket.clone()),
            paths,
            limiter,
            cache: LabelCache::new(),
            last_fp: HashMap::new(),
            applied: HashMap::new(),
            reapplied: HashMap::new(),
            cleared: HashSet::new(),
            started_at: crate::logging::timestamp(),
            pass_count: 0,
            total_llm_calls: 0,
            total_applied: 0,
            last_error: None,
            config,
            provider,
        }
    }

    /// Main loop; returns when the server is gone or SHUTDOWN is set.
    pub fn run(&mut self) {
        let interval = Duration::from_secs(self.config.interval_secs);
        let mut connect_failures = 0u32;
        log_info!(
            "daemon pid {} socket {} provider {} interval {}s",
            std::process::id(),
            self.paths.socket.display(),
            self.provider
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".into()),
            self.config.interval_secs
        );
        loop {
            if SHUTDOWN.load(Ordering::Relaxed) {
                log_info!("shutdown requested");
                break;
            }
            match self.pass(false) {
                Ok((stats, _)) => {
                    connect_failures = 0;
                    log_debug!("pass {} {:?}", self.pass_count, stats);
                }
                Err(herdr::Error::Connect(e)) => {
                    connect_failures += 1;
                    log_warn!("connect failed ({connect_failures}/{MAX_CONNECT_FAILURES}): {e}");
                    if connect_failures >= MAX_CONNECT_FAILURES {
                        log_info!("server gone; exiting");
                        break;
                    }
                }
                Err(e) => {
                    self.last_error = Some(e.to_string());
                    log_warn!("pass failed: {e}");
                }
            }
            self.write_status(true);
            // Sleep in slices so SIGTERM is honoured promptly.
            let deadline = Instant::now() + interval;
            while Instant::now() < deadline && !SHUTDOWN.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        self.write_status(false);
    }

    /// One pass over all panes. `force` ignores fingerprints and re-applies every label.
    pub fn pass(&mut self, force: bool) -> Result<(PassStats, Vec<Outcome>), herdr::Error> {
        let started = Instant::now();
        let snapshot = self.client.snapshot()?;
        self.pass_count += 1;
        let mut stats = PassStats {
            panes: snapshot.panes.len(),
            ..Default::default()
        };
        let mut outcomes = Vec::with_capacity(snapshot.panes.len());
        let alive: HashSet<String> = snapshot.panes.iter().map(|p| p.pane_id.clone()).collect();
        self.last_fp.retain(|k, _| alive.contains(k));
        self.applied.retain(|k, _| alive.contains(k));
        self.reapplied.retain(|k, _| alive.contains(k));
        self.cleared.retain(|k| alive.contains(k));
        self.limiter.retain_panes(&|k| alive.contains(k));

        for pane in &snapshot.panes {
            let outcome = self.handle_pane(pane, force, &mut stats)?;
            outcomes.push(outcome);
        }
        stats.duration_ms = started.elapsed().as_millis();
        Ok((stats, outcomes))
    }

    fn handle_pane(
        &mut self,
        pane: &PaneInfo,
        force: bool,
        stats: &mut PassStats,
    ) -> Result<Outcome, herdr::Error> {
        let id = pane.pane_id.clone();
        let cwd = pane.effective_cwd().to_string();
        let outcome = |label: Option<String>, source: Source, applied: bool| Outcome {
            pane_id: id.clone(),
            label,
            source,
            applied,
        };

        if !self
            .config
            .permits(&[&pane.pane_id, &pane.workspace_id, &cwd])
        {
            stats.skipped += 1;
            self.clear_if_ours(pane)?;
            return Ok(outcome(None, Source::SkippedFilter, false));
        }
        if pane.has_manual_label() {
            stats.skipped += 1;
            self.clear_if_ours(pane)?;
            return Ok(outcome(None, Source::SkippedManual, false));
        }
        self.cleared.remove(&id);

        let procs: Vec<Proc> = match self.client.process_info(&id) {
            Ok(info) => info
                .foreground_processes
                .into_iter()
                .map(|p| Proc {
                    argv: p
                        .argv
                        .unwrap_or_else(|| p.argv0.clone().map(|a| vec![a]).unwrap_or_default()),
                    name: p.name,
                })
                .collect(),
            Err(herdr::Error::Connect(e)) => return Err(herdr::Error::Connect(e)),
            Err(e) => {
                log_debug!("{id}: process_info failed: {e}");
                Vec::new()
            }
        };
        let screen = match self.client.read_recent(&id, self.config.lines) {
            Ok(r) => r.text,
            Err(herdr::Error::Connect(e)) => return Err(herdr::Error::Connect(e)),
            Err(e) => {
                log_debug!("{id}: pane.read failed: {e}");
                String::new()
            }
        };
        let lines: Vec<&str> = screen.lines().collect();
        let fg = heuristics::pick_foreground(&procs);
        let branch = if cwd.is_empty() {
            None
        } else {
            crate::git::branch_for(Path::new(&cwd))
        };
        let agent_status = pane
            .agent_status
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let facts = PaneFacts {
            fg: fg.clone(),
            cwd: cwd.clone(),
            branch: branch.clone(),
            agent: pane.agent.clone().filter(|a| !a.is_empty()),
            agent_status: agent_status.clone(),
        };
        let fp = Fingerprint {
            process: fg
                .as_ref()
                .map(|p| {
                    let mut v = vec![p.command()];
                    v.extend(p.argv.iter().skip(1).take(2).cloned());
                    v
                })
                .unwrap_or_default(),
            cwd: cwd.clone(),
            branch: branch.clone(),
            agent: facts.agent.clone(),
            agent_status: agent_status.clone(),
            screen: fingerprint::screen_hash(lines.iter().copied()),
        }
        .hash();

        if !force && self.last_fp.get(&id) == Some(&fp) {
            stats.unchanged += 1;
            // Our title vanished (server-side reset or another source won): re-apply once.
            if let Some(ours) = self.applied.get(&id).cloned()
                && pane.title.as_deref() != Some(ours.as_str())
                && self.reapplied.get(&id) != Some(&ours)
            {
                log_debug!("{id}: title {:?} != ours {ours:?}; re-applying", pane.title);
                self.apply(&id, &ours, stats)?;
                self.reapplied.insert(id.clone(), ours.clone());
                return Ok(outcome(Some(ours), Source::Unchanged, true));
            }
            return Ok(outcome(
                self.applied.get(&id).cloned(),
                Source::Unchanged,
                false,
            ));
        }

        let (label, source) = match heuristics::decide(&facts, self.config.max_chars) {
            Decision::Label(l) => (l, Source::Heuristic),
            Decision::Llm { fallback } => {
                if let Some(cached) = self.cache.get(fp) {
                    (cached, Source::Cache)
                } else if let Some(provider) = self.provider.clone() {
                    if !self.limiter.try_acquire(&id) {
                        log_debug!("{id}: llm rate-limited; keeping previous label");
                        // Don't record the fingerprint so the next pass retries.
                        if !self.applied.contains_key(&id) {
                            self.apply(&id, &fallback, stats)?;
                            return Ok(outcome(Some(fallback), Source::RateLimited, true));
                        }
                        return Ok(outcome(
                            self.applied.get(&id).cloned(),
                            Source::RateLimited,
                            false,
                        ));
                    }
                    stats.llm_calls += 1;
                    self.total_llm_calls += 1;
                    let fg_cmdline = fg.as_ref().map(|p| p.argv.join(" "));
                    let cwd_base = heuristics::basename(cwd.trim_end_matches('/'));
                    let ctx = llm::Context {
                        agent: facts.agent.as_deref(),
                        agent_status: facts.agent.as_ref().map(|_| agent_status.as_str()),
                        process: fg_cmdline.as_deref(),
                        cwd_basename: &cwd_base,
                        branch: branch.as_deref(),
                        lines: &lines,
                    };
                    let t0 = Instant::now();
                    match provider.label(&ctx, self.config.max_chars) {
                        Ok(l) => {
                            log_info!(
                                "{id}: llm {} → {l:?} ({} ms)",
                                provider,
                                t0.elapsed().as_millis()
                            );
                            self.cache.insert(fp, l.clone());
                            (l, Source::Llm)
                        }
                        Err(e) => {
                            stats.llm_errors += 1;
                            self.last_error = Some(format!("llm: {e}"));
                            log_warn!("{id}: llm failed ({e}); fallback {fallback:?}");
                            (fallback, Source::Fallback)
                        }
                    }
                } else {
                    (fallback, Source::Fallback)
                }
            }
        };

        self.last_fp.insert(id.clone(), fp);
        if label.is_empty() {
            return Ok(outcome(None, source, false));
        }
        let changed = self.applied.get(&id) != Some(&label);
        if changed || force {
            self.apply(&id, &label, stats)?;
            self.reapplied.remove(&id);
            return Ok(outcome(Some(label), source, true));
        }
        Ok(outcome(Some(label), source, false))
    }

    fn apply(&mut self, id: &str, label: &str, stats: &mut PassStats) -> Result<(), herdr::Error> {
        match self.client.set_title(id, label) {
            Ok(()) => {
                stats.labeled += 1;
                self.total_applied += 1;
                self.applied.insert(id.to_string(), label.to_string());
                log_debug!("{id}: title ← {label:?}");
                Ok(())
            }
            Err(herdr::Error::Connect(e)) => Err(herdr::Error::Connect(e)),
            Err(e) => {
                self.last_error = Some(e.to_string());
                log_warn!("{id}: report_metadata failed: {e}");
                Ok(())
            }
        }
    }

    /// Clears our title on a pane we must not label (manual rename / filtered), once.
    fn clear_if_ours(&mut self, pane: &PaneInfo) -> Result<(), herdr::Error> {
        let id = &pane.pane_id;
        let had_ours = self.applied.remove(id).is_some();
        self.last_fp.remove(id);
        self.reapplied.remove(id);
        // A title we didn't set this run may still be ours from a previous daemon.
        if had_ours || (pane.title.is_some() && !self.cleared.contains(id)) {
            match self.client.clear_title(id) {
                Ok(()) => log_debug!("{id}: cleared our title"),
                Err(herdr::Error::Connect(e)) => return Err(herdr::Error::Connect(e)),
                Err(e) => log_warn!("{id}: clear_title failed: {e}"),
            }
        }
        self.cleared.insert(id.clone());
        Ok(())
    }

    pub fn status_value(&self, running: bool, last_pass: Option<&PassStats>) -> serde_json::Value {
        json!({
            "running": running,
            "pid": std::process::id(),
            "socket": self.paths.socket,
            "provider": self.provider.as_ref().map(|p| p.kind.name()),
            "model": self.provider.as_ref().map(|p| p.model.clone()),
            "started_at": self.started_at,
            "updated_at": crate::logging::timestamp(),
            "pass_count": self.pass_count,
            "panes_labeled": self.applied.len(),
            "llm_calls": self.total_llm_calls,
            "titles_applied": self.total_applied,
            "last_pass": last_pass,
            "last_error": self.last_error,
        })
    }

    pub fn write_status(&self, running: bool) {
        let value = self.status_value(running, None);
        let path = self.paths.status_file();
        if let Err(e) = write_atomic(
            &path,
            &serde_json::to_vec_pretty(&value).unwrap_or_default(),
        ) {
            log_warn!("status write failed: {e}");
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_lru_bounded() {
        let mut c = LabelCache::new();
        for i in 0..(CACHE_CAPACITY as u64 + 10) {
            c.insert(i, format!("l{i}"));
        }
        assert_eq!(c.map.len(), CACHE_CAPACITY);
        assert_eq!(c.get(0), None);
        assert_eq!(c.get(CACHE_CAPACITY as u64 + 9).as_deref(), Some("l265"));
        // Touching an entry keeps it alive past the next eviction.
        assert!(c.get(10).is_some());
        c.insert(9999, "x".into());
        assert!(c.get(10).is_some());
        assert_eq!(c.get(11), None);
    }

    #[test]
    fn pidfile_is_keyed_by_socket() {
        let a = Paths {
            socket: "/a.sock".into(),
            state_dir: "/s".into(),
            config_dir: "/c".into(),
        };
        let b = Paths {
            socket: "/b.sock".into(),
            ..a.clone()
        };
        assert_ne!(a.pidfile(), b.pidfile());
        assert!(a.pidfile().starts_with("/s"));
    }
}
