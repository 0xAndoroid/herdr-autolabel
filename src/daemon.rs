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
use crate::spaces::{PaneSummary, SpaceStates};
use crate::transcript;

mod workspace;

const MAX_CONNECT_FAILURES: u32 = 3;
const CACHE_CAPACITY: usize = 256;

pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

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
            .map_or_else(|| home.join(".local/state/herdr-autolabel"), PathBuf::from);
        let config_dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
            .filter(|p| !p.is_empty())
            .map_or_else(|| home.join(".config/herdr-autolabel"), PathBuf::from);
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

    pub fn spaces_file(&self) -> PathBuf {
        let h = fingerprint::hash_str(&self.socket.to_string_lossy());
        self.state_dir.join(format!("spaces-{h:016x}.json"))
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
    SkippedTitle,
    Aggregate,
    Pending,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub pane_id: String,
    pub label: Option<String>,
    pub source: Source,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpaceOutcome {
    pub workspace_id: String,
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
    pub spaces: usize,
    pub spaces_labeled: usize,
    pub spaces_skipped: usize,
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
    last_fp: HashMap<String, u64>,
    /// Title we last reported per pane.
    applied: HashMap<String, String>,
    /// Current label per pane, written or not (`label_panes = false`, manual names, …).
    labels: HashMap<String, PaneSummary>,
    prompts: transcript::Prompts,
    /// Space ownership + hysteresis, mirrored in `Paths::spaces_file`.
    spaces: SpaceStates,
    space_fps: HashMap<String, u64>,
    cleared: HashSet<String>,
    started_at: String,
    pass_count: u64,
    total_llm_calls: u64,
    total_applied: u64,
    total_spaces_applied: u64,
    last_error: Option<String>,
}

impl Daemon {
    pub fn new(paths: Paths, config: Config, provider: Option<llm::Provider>) -> Self {
        let limiter = RateLimiter::new(
            Duration::from_secs(config.llm_per_pane_secs),
            config.llm_global_per_min,
            paths.state_dir.join("llm-budget.json"),
        );
        Self {
            client: Client::new(paths.socket.clone()),
            paths,
            limiter,
            cache: LabelCache::new(),
            last_fp: HashMap::new(),
            applied: HashMap::new(),
            labels: HashMap::new(),
            prompts: transcript::Prompts::default(),
            spaces: SpaceStates::new(),
            space_fps: HashMap::new(),
            cleared: HashSet::new(),
            started_at: crate::logging::timestamp(),
            pass_count: 0,
            total_llm_calls: 0,
            total_applied: 0,
            total_spaces_applied: 0,
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
                .map_or_else(|| "none".into(), |p| p.to_string()),
            self.config.interval_secs
        );
        loop {
            if SHUTDOWN.load(Ordering::Relaxed) {
                log_info!("shutdown requested");
                break;
            }
            let deadline = Instant::now() + interval;
            match self.pass(false) {
                Ok((stats, _, _)) => {
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
                    connect_failures = 0;
                    self.last_error = Some(e.to_string());
                    log_warn!("pass failed: {e}");
                }
            }
            self.write_status(true);
            // Sleep in slices so SIGTERM is honoured promptly.
            while Instant::now() < deadline && !SHUTDOWN.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        if self.config.label_spaces {
            self.restore_spaces();
        }
        self.write_status(false);
    }

    /// One pass over all panes. `force` ignores fingerprints and re-applies every label.
    pub fn pass(
        &mut self,
        force: bool,
    ) -> Result<(PassStats, Vec<Outcome>, Vec<SpaceOutcome>), herdr::Error> {
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
        self.labels.retain(|k, _| alive.contains(k));
        self.cleared.retain(|k| alive.contains(k));

        for pane in &snapshot.panes {
            if SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            let allowed =
                self.config
                    .permits(&[&pane.pane_id, &pane.workspace_id, pane.effective_cwd()]);
            let pika = allowed && pane.agent.as_deref() == Some("pika");
            let folder = if allowed && !pika {
                heuristics::project(pane.effective_cwd()).unwrap_or_default()
            } else {
                String::new()
            };
            if pane
                .tokens
                .get(herdr::FOLDER_TOKEN)
                .map_or("", String::as_str)
                != folder
                || (pika && pane.display_agent.as_deref() != Some("pika TUI"))
                || (!pika && pane.display_agent.as_deref() == Some("pika TUI"))
            {
                self.client
                    .set_pane_identity(&pane.pane_id, &folder, pika)?;
            }
            let outcome = self.handle_pane(pane, force, &mut stats)?;
            outcomes.push(outcome);
        }
        let spaces = if self.config.label_spaces && !SHUTDOWN.load(Ordering::Relaxed) {
            self.handle_spaces(&snapshot, force, &mut stats)?
        } else {
            Vec::new()
        };
        stats.duration_ms = started.elapsed().as_millis();
        Ok((stats, outcomes, spaces))
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

        if let Some(source) = self.skip_pane(pane, stats)? {
            let shown = match source {
                Source::SkippedManual => pane.label.clone(),
                Source::SkippedTitle => pane.title.clone(),
                _ => None,
            };
            match shown.filter(|s| {
                !s.trim().is_empty()
                    && (source == Source::SkippedManual || llm::postprocess(s, usize::MAX).is_ok())
            }) {
                Some(label) => {
                    let project = heuristics::project(&cwd);
                    let branch = crate::git::branch_for(Path::new(&cwd));
                    let session = pane.agent_session.as_ref();
                    let request = session.and_then(|s| self.prompts.last(s));
                    let first = session.and_then(|s| self.prompts.first(s));
                    let context = llm::user_message(&llm::Context {
                        agent: pane.agent.as_deref(),
                        agent_status: pane.agent_status.as_deref(),
                        project: project.as_deref(),
                        cwd_basename: &heuristics::directory_name(&cwd),
                        branch: branch.as_deref(),
                        topic: pane.terminal_title_stripped.as_deref(),
                        request: request.as_deref(),
                        first_request: first.as_deref(),
                        ..llm::Context::default()
                    });
                    self.labels.insert(
                        id.clone(),
                        PaneSummary {
                            label: label.trim().to_string(),
                            project,
                            fingerprint: fingerprint::hash_str(&context),
                            context,
                            task_session: None,
                        },
                    );
                }
                None => {
                    self.labels.remove(&id);
                }
            }
            return Ok(outcome(None, source, false));
        }

        let (procs, leader): (Vec<Proc>, Option<u32>) = match self.client.process_info(&id) {
            Ok(info) => (
                info.foreground_processes
                    .into_iter()
                    .map(|p| Proc {
                        pid: p.pid,
                        argv: p.argv.unwrap_or_else(|| {
                            p.argv0.clone().map(|a| vec![a]).unwrap_or_default()
                        }),
                        name: p.name,
                    })
                    .collect(),
                info.foreground_process_group_id,
            ),
            Err(herdr::Error::Connect(e)) => return Err(herdr::Error::Connect(e)),
            Err(e) => {
                log_debug!("{id}: process_info failed: {e}");
                (Vec::new(), None)
            }
        };
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(outcome(None, Source::Unchanged, false));
        }
        let fg = heuristics::pick_foreground(&procs, leader);
        let child = fg
            .as_ref()
            .and_then(|typed| heuristics::running_child(&procs, typed));
        let branch = if cwd.is_empty() {
            None
        } else {
            crate::git::branch_for(Path::new(&cwd))
        };
        let agent_status = pane
            .agent_status
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let agent = pane.agent.clone().filter(|a| !a.is_empty());
        let session = pane.agent_session.as_ref().filter(|s| !s.value.is_empty());
        let session_value = session.map_or("", |s| s.value.as_str());
        let prompt = session.and_then(|s| self.prompts.last(s));
        let first_prompt = session
            .filter(|_| prompt.is_some())
            .and_then(|s| self.prompts.first(s))
            .filter(|f| Some(f) != prompt.as_ref());
        let project = heuristics::project(&cwd);
        let cwd_base = heuristics::directory_name(&cwd);
        let agent_kind = agent
            .clone()
            .or_else(|| fg.as_ref().and_then(heuristics::agent_of));
        let title = agent_kind.as_deref().and_then(|kind| {
            pane.terminal_title_stripped
                .as_deref()
                .map(str::trim)
                .filter(|t| {
                    !t.is_empty()
                        && !t.to_ascii_lowercase().starts_with(kind)
                        && *t != cwd
                        && *t != cwd_base
                        && Some(*t) != project.as_deref()
                        && !t.split_once(':').is_some_and(|(host, path)| {
                            host.contains('@') && path.starts_with(['/', '~'])
                        })
                })
        });
        let request = prompt.as_deref().or(title);
        let facts = PaneFacts {
            fg: fg.clone(),
            cwd: cwd.clone(),
            branch: branch.clone(),
            agent,
            agent_status: agent_status.clone(),
            has_request: request.is_some(),
        };
        let fp = Fingerprint {
            process: fg
                .as_ref()
                .map(|p| {
                    let mut v = vec![p.command()];
                    v.extend(p.argv.iter().skip(1).take(2).cloned());
                    // A command's phases (`docker build`, then `docker logs`) relabel it; an
                    // agent's tool calls do not.
                    if let Some(c) = child.as_ref().filter(|_| agent_kind.is_none()) {
                        v.push(c.command());
                        v.extend(c.argv.iter().skip(1).take(1).cloned());
                    }
                    v
                })
                .unwrap_or_default(),
            cwd: facts.cwd.clone(),
            branch: facts.branch.clone(),
            agent: facts.agent.clone(),
            session: session.map(|s| s.value.clone()),
            idle: agent_status == "idle",
            working: agent_status == "working",
            prompt: prompt.clone(),
            title: title.map(str::to_string),
        }
        .hash();

        let fg_cmdline = fg.as_ref().map(|p| p.argv.join(" "));
        let child_cmdline = child.as_ref().map(|p| p.argv.join(" "));
        let ctx = llm::Context {
            agent: agent_kind.as_deref(),
            agent_status: agent_kind.as_ref().map(|_| agent_status.as_str()),
            process: fg_cmdline.as_deref(),
            running: child_cmdline.as_deref(),
            project: project.as_deref(),
            cwd_basename: &cwd_base,
            branch: branch.as_deref(),
            request,
            topic: title,
            first_request: first_prompt.as_deref(),
            lines: &[],
        };
        let summary = self.labels.entry(id.clone()).or_default();
        summary.project.clone_from(&project);
        summary.context = llm::user_message(&ctx);
        summary.fingerprint = fp;

        if !force && self.last_fp.get(&id) == Some(&fp) {
            stats.unchanged += 1;
            if self.config.label_panes
                && let Some(ours) = self.applied.get(&id).cloned()
                && pane.title.is_none()
            {
                let applied = self.apply(&id, &ours, stats)?;
                return Ok(outcome(Some(ours), Source::Unchanged, applied));
            }
            return Ok(outcome(
                self.labels.get(&id).map(|l| l.label.clone()),
                Source::Unchanged,
                false,
            ));
        }

        let decision = heuristics::decide(&facts, self.config.max_chars);
        let (label, source, record_fp) = match decision {
            Decision::Label(l) => (l, Source::Heuristic, true),
            Decision::NewSession(l) => {
                match self
                    .labels
                    .get(&id)
                    .filter(|p| p.task_session.as_deref() == Some(session_value))
                {
                    Some(previous) => (previous.label.clone(), Source::Unchanged, true),
                    None => (l, Source::Heuristic, true),
                }
            }
            Decision::Llm { fallback } => {
                if let Some(cached) = self.cache.get(fp) {
                    (cached, Source::Cache, true)
                } else if let Some(provider) = self.provider.clone() {
                    let budget_key = format!("{}:{id}", self.paths.socket.display());
                    if self.limiter.try_acquire(&budget_key) {
                        // The screen is read only now: it never enters the fingerprint, so
                        // unchanged panes cost one process_info call and herdr's read-time
                        // side effects (alternate-screen history harvest) stay off idle panes.
                        let screen = match self.client.read_visible(&id, self.config.lines) {
                            Ok(r) => r.text,
                            Err(herdr::Error::Connect(e)) => return Err(herdr::Error::Connect(e)),
                            Err(e) => {
                                log_debug!("{id}: pane.read failed: {e}");
                                String::new()
                            }
                        };
                        let lines: Vec<&str> = screen.lines().collect();
                        stats.llm_calls += 1;
                        self.total_llm_calls += 1;
                        let ctx = llm::Context {
                            lines: &lines,
                            ..ctx
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
                                (l, Source::Llm, true)
                            }
                            Err(e) => {
                                stats.llm_errors += 1;
                                self.limiter.back_off(&budget_key);
                                self.last_error = Some(format!("llm: {e}"));
                                log_warn!("{id}: llm failed ({e}); fallback {fallback:?}");
                                (
                                    self.labels
                                        .get(&id)
                                        .filter(|p| {
                                            agent_kind.is_none()
                                                || p.task_session.as_deref() == Some(session_value)
                                        })
                                        .map(|p| p.label.clone())
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or(fallback),
                                    Source::Fallback,
                                    false,
                                )
                            }
                        }
                    } else {
                        log_debug!("{id}: llm rate-limited");
                        // Leave the fingerprint unrecorded so the next pass retries.
                        match self
                            .labels
                            .get(&id)
                            .filter(|p| {
                                agent_kind.is_none()
                                    || p.task_session.as_deref() == Some(session_value)
                            })
                            .map(|l| l.label.clone())
                            .filter(|s| !s.is_empty())
                        {
                            Some(previous) => (previous, Source::RateLimited, false),
                            None => (fallback, Source::Fallback, false),
                        }
                    }
                } else {
                    (fallback, Source::Fallback, true)
                }
            }
        };

        if label.is_empty() {
            self.labels.remove(&id);
            return Ok(outcome(None, source, false));
        }
        if let Some(summary) = self.labels.get_mut(&id) {
            summary.label.clone_from(&label);
            match source {
                Source::Llm | Source::Cache => summary.task_session = Some(session_value.into()),
                Source::Heuristic => summary.task_session = None,
                _ => {}
            }
        }
        log_debug!("{id}: fingerprint={fp:016x} source={source:?} label={label:?}");
        if !self.config.label_panes {
            if record_fp {
                self.last_fp.insert(id.clone(), fp);
            }
            return Ok(outcome(Some(label), source, false));
        }
        let changed = self.applied.get(&id) != Some(&label);
        if changed || force {
            let applied = self.apply(&id, &label, stats)?;
            if applied && record_fp {
                self.last_fp.insert(id.clone(), fp);
            }
            return Ok(outcome(Some(label), source, applied));
        }
        if record_fp {
            self.last_fp.insert(id.clone(), fp);
        }
        Ok(outcome(Some(label), source, false))
    }

    fn skip_pane(
        &mut self,
        pane: &PaneInfo,
        stats: &mut PassStats,
    ) -> Result<Option<Source>, herdr::Error> {
        let id = &pane.pane_id;
        let source = if !self
            .config
            .permits(&[id, &pane.workspace_id, pane.effective_cwd()])
        {
            Some(Source::SkippedFilter)
        } else if pane.has_manual_label() {
            Some(Source::SkippedManual)
        } else if pane
            .title
            .as_ref()
            .is_some_and(|title| !title.is_empty() && self.applied.get(id) != Some(title))
        {
            // A title we did not apply: either ours from a previous daemon run (cleared
            // once via `clear_if_ours`, after which the pane is labelled again) or another
            // source's, which keeps winning for as long as it is present.
            Some(Source::SkippedTitle)
        } else {
            None
        };
        if source.is_some() {
            stats.skipped += 1;
            self.clear_if_ours(pane)?;
        } else {
            self.cleared.remove(id);
        }
        Ok(source)
    }

    fn apply(
        &mut self,
        id: &str,
        label: &str,
        stats: &mut PassStats,
    ) -> Result<bool, herdr::Error> {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(false);
        }
        // An LLM call can outlive a rename or another source's title update.
        let pane = self.client.pane(id)?;
        if self.skip_pane(&pane, stats)?.is_some() || SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(false);
        }
        self.client.set_title(id, label)?;
        stats.labeled += 1;
        self.total_applied += 1;
        self.applied.insert(id.to_string(), label.to_string());
        log_debug!("{id}: title ← {label:?}");
        Ok(true)
    }

    /// Clears only our source, recording completion only after the server acknowledges it.
    fn clear_if_ours(&mut self, pane: &PaneInfo) -> Result<(), herdr::Error> {
        let id = &pane.pane_id;
        if !self.cleared.contains(id) && (self.applied.contains_key(id) || pane.title.is_some()) {
            self.client.clear_title(id)?;
            self.cleared.insert(id.clone());
            log_debug!("{id}: cleared our title");
        }
        self.applied.remove(id);
        self.last_fp.remove(id);
        Ok(())
    }

    pub fn status_value(&self, running: bool, last_pass: Option<&PassStats>) -> serde_json::Value {
        json!({
            "running": running,
            "pid": std::process::id(),
            "socket": self.paths.socket,
            "provider": self.provider.as_ref().map(|p| p.kind.name()),
            "model": self.provider.as_ref().map(|p| p.model.clone()),
            "debug": crate::logging::debug_enabled(),
            "started_at": self.started_at,
            "updated_at": crate::logging::timestamp(),
            "pass_count": self.pass_count,
            "panes_labeled": self.applied.len(),
            "llm_calls": self.total_llm_calls,
            "titles_applied": self.total_applied,
            "spaces_labeled": self.spaces.values().filter(|s| s.applied.is_some()).count(),
            "space_labels_applied": self.total_spaces_applied,
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
mod tests;
