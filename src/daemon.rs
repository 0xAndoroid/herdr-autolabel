//! The labelling loop: snapshot → per-pane facts → fingerprint → heuristics/LLM → report title.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::config::Config;
use crate::fingerprint::{self, Fingerprint};
use crate::herdr::{self, Client, PaneInfo, Snapshot};
use crate::heuristics::{self, Decision, PaneFacts, Proc};
use crate::llm;
use crate::logging::{log_debug, log_info, log_warn};
use crate::ratelimit::RateLimiter;
use crate::spaces::{self, PaneSummary, SpaceStates};
use crate::transcript;

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

    /// Space ownership state, per socket like the pidfile.
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
    /// Space label derived from its panes.
    Aggregate,
    /// Space candidate waiting out the hysteresis window.
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
    /// Current label per pane, written or not (`label_panes = false`, manual names, …).
    labels: HashMap<String, PaneSummary>,
    /// The user's last prompt per Claude session, from its transcript.
    prompts: transcript::Prompts,
    /// Space ownership + hysteresis, mirrored in `Paths::spaces_file`.
    spaces: SpaceStates,
    /// A different title owner was observed; leave the pane alone until it closes.
    /// Panes whose title we cleared because of a manual label.
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
            let deadline = Instant::now() + interval;
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
            // Skipped panes still tell their space what they show. A title from another source
            // (or from a previous run of ours) is already a whole name; a hand-typed pane name
            // is the activity.
            let shown = match source {
                Source::SkippedManual => pane.label.clone(),
                Source::SkippedTitle => pane.title.clone(),
                _ => None,
            };
            match shown.filter(|s| !s.trim().is_empty()) {
                Some(label) => {
                    self.labels.insert(
                        id.clone(),
                        PaneSummary {
                            label: label.trim().to_string(),
                            agent: pane.agent.as_deref().is_some_and(|a| !a.is_empty()),
                            busy: true,
                            whole: source == Source::SkippedTitle,
                            project: heuristics::project(&cwd),
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
        let facts = PaneFacts {
            fg: fg.clone(),
            cwd: cwd.clone(),
            branch: branch.clone(),
            agent: pane.agent.clone().filter(|a| !a.is_empty()),
            agent_status: agent_status.clone(),
        };
        // The agent's transcript has the user's prompts verbatim; herdr reports the session.
        let session = pane.agent_session.as_ref().filter(|s| !s.value.is_empty());
        let prompt = session.and_then(|s| self.prompts.last(s));
        let first_prompt = session
            .filter(|_| prompt.is_some())
            .and_then(|s| self.prompts.first(s))
            .filter(|f| Some(f) != prompt.as_ref());
        let project = heuristics::project(&cwd);
        let agent_kind = facts
            .agent
            .clone()
            .or_else(|| fg.as_ref().and_then(heuristics::agent_of));
        let agent = agent_kind.is_some();
        // The summary a coding agent keeps in its terminal title — unless that is a status line
        // of the agent's own ("pika — idle").
        let title = agent_kind.as_deref().and_then(|kind| {
            pane.terminal_title_stripped
                .as_deref()
                .filter(|t| !t.trim().is_empty() && !t.to_ascii_lowercase().starts_with(kind))
        });
        // The user's request: the last prompt itself, else that title.
        let request = prompt.as_deref().or(title);
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
            cwd: cwd.clone(),
            branch: branch.clone(),
            agent: facts.agent.clone(),
            idle: agent_status == "idle",
            prompt: prompt.clone(),
            title: title.map(str::to_string),
        }
        .hash();

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
        // LLM-written names already carry the project as far as it helps.
        let whole = !matches!(decision, Decision::Label(_));
        let (label, source, record_fp) = match decision {
            Decision::Label(l) | Decision::Whole(l) => (l, Source::Heuristic, true),
            Decision::Llm { fallback } => {
                if let Some(cached) = self.cache.get(fp) {
                    (cached, Source::Cache, true)
                } else if let Some(provider) = self.provider.clone() {
                    if !self
                        .limiter
                        .try_acquire(&format!("{}:{id}", self.paths.socket.display()))
                    {
                        log_debug!("{id}: llm rate-limited; keeping previous label");
                        // Keep the previous label (or the fallback when there is none) and don't
                        // record the fingerprint so the next pass retries.
                        match self.labels.get(&id).map(|l| l.label.clone()) {
                            Some(previous) => (previous, Source::RateLimited, false),
                            None => (fallback, Source::Fallback, false),
                        }
                    } else {
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
                        let fg_cmdline = fg.as_ref().map(|p| p.argv.join(" "));
                        let child_cmdline = child.as_ref().map(|p| p.argv.join(" "));
                        let cwd_base = heuristics::basename(cwd.trim_end_matches('/'));
                        let ctx = llm::Context {
                            agent: facts.agent.as_deref(),
                            agent_status: facts.agent.as_ref().map(|_| agent_status.as_str()),
                            process: fg_cmdline.as_deref(),
                            running: child_cmdline.as_deref(),
                            project: project.as_deref(),
                            cwd_basename: &cwd_base,
                            branch: branch.as_deref(),
                            request,
                            topic: title,
                            first_request: first_prompt.as_deref(),
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
                                (l, Source::Llm, true)
                            }
                            Err(e) => {
                                stats.llm_errors += 1;
                                self.last_error = Some(format!("llm: {e}"));
                                log_warn!("{id}: llm failed ({e}); fallback {fallback:?}");
                                (fallback, Source::Fallback, true)
                            }
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
        // A stand-in name (no LLM, or still waiting for its budget) titles the pane only; its
        // space keeps herdr's default until a real name exists.
        if source == Source::Fallback {
            self.labels.remove(&id);
        } else {
            self.labels.insert(
                id.clone(),
                PaneSummary {
                    label: label.clone(),
                    agent,
                    busy: fg.is_some(),
                    whole,
                    project,
                },
            );
        }
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

    /// Labels every sidebar space from the labels its panes got in this pass.
    fn handle_spaces(
        &mut self,
        snapshot: &Snapshot,
        force: bool,
        stats: &mut PassStats,
    ) -> Result<Vec<SpaceOutcome>, herdr::Error> {
        // The state file is shared with one-shot runs (`once`), which may have renamed spaces
        // since our last pass: take the file as truth and keep only the in-memory hysteresis.
        let mut states = spaces::load_states(&self.paths.spaces_file());
        for (id, state) in &mut states {
            state.pending = self.spaces.get(id).and_then(|s| s.pending.clone());
        }
        let alive: HashSet<&str> = snapshot
            .workspaces
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect();
        let before = states.len();
        states.retain(|id, _| alive.contains(id.as_str()));
        let mut dirty = states.len() != before;
        stats.spaces = snapshot.workspaces.len();
        let mut outcomes = Vec::with_capacity(snapshot.workspaces.len());

        for ws in &snapshot.workspaces {
            if SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            let id = &ws.workspace_id;
            let outcome = |label: Option<String>, source: Source, applied: bool| SpaceOutcome {
                workspace_id: id.clone(),
                label,
                source,
                applied,
            };
            let panes: Vec<&PaneInfo> = snapshot
                .panes
                .iter()
                .filter(|p| p.workspace_id == *id)
                .collect();
            let defaults = spaces::default_names(
                panes
                    .iter()
                    .flat_map(|p| [p.cwd.as_deref().unwrap_or(""), p.effective_cwd()]),
                ws.worktree.as_ref().map(|w| w.repo_name.as_str()),
                ws.worktree.as_ref().map(|w| w.checkout_path.as_str()),
            );
            let ownership = spaces::classify(&ws.label, states.get(id), &defaults);
            let state = states.entry(id.clone()).or_default();
            match ownership {
                spaces::Ownership::Manual => {
                    if state.applied.take().is_some() {
                        dirty = true;
                        log_info!("{id}: space renamed by hand to {:?}; leaving it", ws.label);
                    }
                    state.pending = None;
                    stats.spaces_skipped += 1;
                    outcomes.push(outcome(
                        Some(ws.label.clone()),
                        Source::SkippedManual,
                        false,
                    ));
                    continue;
                }
                spaces::Ownership::Default => {
                    if state.applied.is_some() || state.original != ws.label {
                        state.applied = None;
                        state.original = ws.label.clone();
                        dirty = true;
                    }
                }
                spaces::Ownership::Ours => {}
            }
            let summaries: Vec<PaneSummary> = panes
                .iter()
                .filter_map(|p| self.labels.get(&p.pane_id).cloned())
                .collect();
            let worktree = ws
                .worktree
                .as_ref()
                .map(|w| heuristics::project_name(&w.repo_name));
            let candidate = spaces::aggregate(&summaries, worktree, self.config.max_chars);
            let Some(next) = state.observe(candidate.as_deref(), force) else {
                let source = if state.pending.is_some() {
                    Source::Pending
                } else {
                    Source::Unchanged
                };
                outcomes.push(outcome(state.applied.clone(), source, false));
                continue;
            };
            if next == ws.label {
                // herdr's own name already says it (single `pika` pane in a `pika` space).
                state.applied = Some(next.clone());
                dirty = true;
                outcomes.push(outcome(Some(next), Source::Aggregate, false));
                continue;
            }
            // Pane labelling (and its LLM calls) ran after the snapshot; recheck the name.
            let current = match self.client.workspace(id) {
                Ok(w) => w,
                Err(herdr::Error::Api { .. }) => continue, // closed meanwhile
                Err(e) => return Err(e),
            };
            if current.label != ws.label {
                log_debug!("{id}: space renamed during the pass; skipping");
                outcomes.push(outcome(None, Source::SkippedManual, false));
                continue;
            }
            self.client.rename_workspace(id, &next)?;
            state.applied = Some(next.clone());
            dirty = true;
            stats.spaces_labeled += 1;
            self.total_spaces_applied += 1;
            log_debug!("{id}: space ← {next:?}");
            outcomes.push(outcome(Some(next), Source::Aggregate, true));
        }
        if dirty && let Err(e) = spaces::save_states(&self.paths.spaces_file(), &states) {
            log_warn!("spaces state write failed: {e}");
        }
        self.spaces = states;
        Ok(outcomes)
    }

    /// Puts back the names spaces had before we renamed them; only spaces still carrying one
    /// of our labels are touched.
    pub fn restore_spaces(&mut self) {
        let mut states = spaces::load_states(&self.paths.spaces_file());
        if states.values().all(|s| s.applied.is_none()) {
            return;
        }
        let snapshot = match self.client.snapshot() {
            Ok(s) => s,
            Err(e) => {
                log_warn!("cannot restore space names: {e}");
                return;
            }
        };
        for ws in &snapshot.workspaces {
            let Some(state) = states.get_mut(&ws.workspace_id) else {
                continue;
            };
            if state.applied.as_deref() != Some(ws.label.as_str()) || state.original.is_empty() {
                continue;
            }
            if state.original == ws.label {
                state.applied = None;
                continue;
            }
            match self
                .client
                .rename_workspace(&ws.workspace_id, &state.original)
            {
                Ok(()) => {
                    log_debug!(
                        "{}: space restored to {:?}",
                        ws.workspace_id,
                        state.original
                    );
                    state.applied = None;
                }
                Err(e) => log_warn!("{}: restore failed: {e}", ws.workspace_id),
            }
        }
        if let Err(e) = spaces::save_states(&self.paths.spaces_file(), &states) {
            log_warn!("spaces state write failed: {e}");
        }
        self.spaces = states;
    }

    fn skip_pane(
        &mut self,
        pane: &PaneInfo,
        stats: &mut PassStats,
    ) -> Result<Option<Source>, herdr::Error> {
        let id = &pane.pane_id;
        let source = if pane.has_manual_label() {
            Some(Source::SkippedManual)
        } else if !self
            .config
            .permits(&[id, &pane.workspace_id, pane.effective_cwd()])
        {
            Some(Source::SkippedFilter)
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
mod tests {
    use super::*;

    /// Joins the mock server and removes its state dir (only once the daemon is done with it).
    fn finish(
        daemon: &Daemon,
        server: std::thread::JoinHandle<Vec<serde_json::Value>>,
    ) -> Vec<serde_json::Value> {
        let seen = server.join().unwrap();
        let _ = std::fs::remove_dir_all(&daemon.paths.state_dir);
        seen
    }

    fn mock_daemon(
        name: &str,
        replies: Vec<(&'static str, serde_json::Value)>,
    ) -> (Daemon, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        mock_daemon_with(name, Config::default(), replies)
    }

    /// Serves `replies` in order, one connection each, asserting the method of every request;
    /// joining the server yields the requests it saw.
    fn mock_daemon_with(
        name: &str,
        config: Config,
        replies: Vec<(&'static str, serde_json::Value)>,
    ) -> (Daemon, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        // Unique per invocation so concurrent tests never share sockets or budget files.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("hal-{name}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let paths = Paths {
            socket: socket.clone(),
            state_dir: dir.clone(),
            config_dir: dir.clone(),
        };
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (method, response) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method, "request {}", seen.len());
                if method == "pane.report_metadata" {
                    assert_eq!(request["params"]["source"], herdr::SOURCE);
                }
                let mut response = response;
                response["id"] = request["id"].clone();
                writeln!(stream, "{response}").unwrap();
                seen.push(request);
            }
            seen
        });
        (Daemon::new(paths, config, None), server)
    }

    #[test]
    fn manual_and_competing_titles_clear_once_and_relabel_when_title_vanishes() {
        for manual in [false, true] {
            let mut replies = vec![("pane.report_metadata", json!({"result": {"type": "ok"}}))];
            if !manual {
                // Once the competing title is gone (e.g. it was ours from a previous daemon
                // run), the pane is labelled again.
                replies.extend([
                    ("pane.process_info", json!({"result": {"process_info": {}}})),
                    ("pane.get", json!({"result": {"pane": {"pane_id": "p1"}}})),
                    ("pane.report_metadata", json!({"result": {"type": "ok"}})),
                ]);
            }
            let (mut daemon, server) =
                mock_daemon(if manual { "manual" } else { "competing" }, replies);
            let mut pane = PaneInfo {
                pane_id: "p1".into(),
                title: Some("other title".into()),
                ..Default::default()
            };
            if manual {
                pane.label = Some("mine".into());
            }
            daemon.applied.insert("p1".into(), "ours".into());
            let mut stats = PassStats::default();
            assert!(
                !daemon
                    .handle_pane(&pane, false, &mut stats)
                    .unwrap()
                    .applied
            );
            assert!(!daemon.handle_pane(&pane, true, &mut stats).unwrap().applied);
            assert!(daemon.applied.is_empty());
            if !manual {
                pane.title = None;
                let outcome = daemon.handle_pane(&pane, true, &mut stats).unwrap();
                assert_ne!(outcome.source, Source::SkippedTitle);
                assert!(outcome.applied);
                assert!(daemon.applied.contains_key("p1"));
            }
            finish(&daemon, server);
        }
    }

    #[test]
    fn failed_clear_is_retried_until_acknowledged() {
        let (mut daemon, server) = mock_daemon(
            "clear-retry",
            vec![
                (
                    "pane.report_metadata",
                    json!({"error": {"code": "busy", "message": "retry"}}),
                ),
                ("pane.report_metadata", json!({"result": {"type": "ok"}})),
            ],
        );
        let pane = PaneInfo {
            pane_id: "p1".into(),
            label: Some("mine".into()),
            title: Some("ours".into()),
            ..Default::default()
        };
        let mut stats = PassStats::default();
        assert!(daemon.handle_pane(&pane, false, &mut stats).is_err());
        assert!(daemon.handle_pane(&pane, false, &mut stats).is_ok());
        assert!(daemon.handle_pane(&pane, false, &mut stats).is_ok());
        finish(&daemon, server);
    }

    #[test]
    fn failed_apply_does_not_cache_fingerprint() {
        let mut replies = Vec::new();
        for fail in [true, false] {
            replies.extend([
                ("pane.process_info", json!({"result": {"process_info": {}}})),
                ("pane.get", json!({"result": {"pane": {"pane_id": "p1"}}})),
                (
                    "pane.report_metadata",
                    if fail {
                        json!({"error": {"code": "busy"}})
                    } else {
                        json!({"result": {"type": "ok"}})
                    },
                ),
            ]);
        }
        let (mut daemon, server) = mock_daemon("apply-retry", replies);
        let pane = PaneInfo {
            pane_id: "p1".into(),
            ..Default::default()
        };
        let mut stats = PassStats::default();
        assert!(daemon.handle_pane(&pane, false, &mut stats).is_err());
        assert!(daemon.last_fp.is_empty());
        assert!(
            daemon
                .handle_pane(&pane, false, &mut stats)
                .unwrap()
                .applied
        );
        finish(&daemon, server);
    }

    #[test]
    fn rename_during_labeling_prevents_write() {
        let (mut daemon, server) = mock_daemon(
            "rename",
            vec![
                ("pane.process_info", json!({"result": {"process_info": {}}})),
                (
                    "pane.get",
                    json!({"result": {"pane": {"pane_id": "p1", "label": "mine"}}}),
                ),
            ],
        );
        let pane = PaneInfo {
            pane_id: "p1".into(),
            ..Default::default()
        };
        assert!(
            !daemon
                .handle_pane(&pane, false, &mut PassStats::default())
                .unwrap()
                .applied
        );
        assert!(daemon.last_fp.is_empty());
        finish(&daemon, server);
    }

    fn ws(id: &str, label: &str, focused: bool) -> serde_json::Value {
        json!({"workspace_id": id, "label": label, "focused": focused, "pane_count": 1})
    }

    fn pane_json(
        id: &str,
        ws: &str,
        cwd: &str,
        focused: bool,
        title: Option<&str>,
    ) -> serde_json::Value {
        json!({"pane_id": id, "workspace_id": ws, "cwd": cwd, "focused": focused, "title": title})
    }

    fn snapshot(
        panes: Vec<serde_json::Value>,
        workspaces: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        json!({"result": {"snapshot": {"panes": panes, "workspaces": workspaces}}})
    }

    fn labelled_pane(id: &str) -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("pane.process_info", json!({"result": {"process_info": {}}})),
            ("pane.get", json!({"result": {"pane": {"pane_id": id}}})),
            ("pane.report_metadata", json!({"result": {"type": "ok"}})),
        ]
    }

    fn unchanged_pane() -> Vec<(&'static str, serde_json::Value)> {
        vec![("pane.process_info", json!({"result": {"process_info": {}}}))]
    }

    #[test]
    fn spaces_follow_their_panes_with_hysteresis_and_restore_on_stop() {
        // Two fake repos (a `.git` dir is enough for repo/branch detection).
        let root = std::env::temp_dir().join(format!("hal-spaces-repos-{}", std::process::id()));
        for d in [
            "pika/.git",
            "pika/crates",
            "pika/src",
            "jolt/.git",
            "jolt/sub",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let p = |rel: &str| root.join(rel).to_string_lossy().into_owned();
        // w1: one idle pane in pika/crates → "pika: crates" (herdr named the space "pika").
        // w2: two idle panes in the jolt repo, the first at its root → "jolt", which is
        // already the space's own name → recorded, not renamed.
        let mut replies = vec![(
            "session.snapshot",
            snapshot(
                vec![
                    pane_json("w1:p1", "w1", &p("pika/crates"), false, None),
                    pane_json("w2:p1", "w2", &p("jolt"), true, None),
                    pane_json("w2:p2", "w2", &p("jolt/sub"), false, None),
                ],
                vec![ws("w1", "pika", false), ws("w2", "jolt", true)],
            ),
        )];
        replies.extend(labelled_pane("w1:p1"));
        replies.extend(labelled_pane("w2:p1"));
        replies.extend(labelled_pane("w2:p2"));
        replies.extend([
            (
                "workspace.get",
                json!({"result": {"workspace": {"workspace_id": "w1", "label": "pika"}}}),
            ),
            ("workspace.rename", json!({"result": {"type": "ok"}})),
        ]);
        // Pass 2: w1's pane moved to pika/src → pane relabelled at once, space waits.
        let pass2_panes = vec![
            pane_json("w1:p1", "w1", &p("pika/src"), false, Some("crates")),
            pane_json("w2:p1", "w2", &p("jolt"), true, Some("jolt")),
            pane_json("w2:p2", "w2", &p("jolt/sub"), false, Some("sub")),
        ];
        let pass2_ws = vec![ws("w1", "pika: crates", false), ws("w2", "jolt", true)];
        replies.push(("session.snapshot", snapshot(pass2_panes, pass2_ws.clone())));
        replies.extend(labelled_pane("w1:p1"));
        replies.extend(unchanged_pane());
        replies.extend(unchanged_pane());
        // Pass 3: same picture again → the space follows.
        let pass3_panes = vec![
            pane_json("w1:p1", "w1", &p("pika/src"), false, Some("src")),
            pane_json("w2:p1", "w2", &p("jolt"), true, Some("jolt")),
            pane_json("w2:p2", "w2", &p("jolt/sub"), false, Some("sub")),
        ];
        replies.push(("session.snapshot", snapshot(pass3_panes.clone(), pass2_ws)));
        for _ in 0..3 {
            replies.extend(unchanged_pane());
        }
        replies.extend([
            (
                "workspace.get",
                json!({"result": {"workspace": {"workspace_id": "w1", "label": "pika: crates"}}}),
            ),
            ("workspace.rename", json!({"result": {"type": "ok"}})),
        ]);
        // Stop: w1 goes back to "pika"; w2 never changed name.
        replies.push((
            "session.snapshot",
            snapshot(
                pass3_panes,
                vec![ws("w1", "pika: src", false), ws("w2", "jolt", true)],
            ),
        ));
        replies.push(("workspace.rename", json!({"result": {"type": "ok"}})));

        let (mut daemon, server) = mock_daemon("spaces", replies);
        let (stats, panes, spaces) = daemon.pass(false).unwrap();
        assert_eq!(stats.spaces, 2);
        assert_eq!(stats.spaces_labeled, 1);
        assert_eq!(panes.iter().filter(|o| o.applied).count(), 3);
        let by_id = |v: &[SpaceOutcome], id: &str| {
            v.iter().find(|o| o.workspace_id == id).cloned().unwrap()
        };
        assert_eq!(by_id(&spaces, "w1").label.as_deref(), Some("pika: crates"));
        assert!(by_id(&spaces, "w1").applied);
        assert_eq!(by_id(&spaces, "w2").label.as_deref(), Some("jolt"));
        assert!(!by_id(&spaces, "w2").applied);
        let states = spaces::load_states(&daemon.paths.spaces_file());
        assert_eq!(states["w1"].original, "pika");
        assert_eq!(states["w1"].applied.as_deref(), Some("pika: crates"));
        assert_eq!(states["w2"].applied.as_deref(), Some("jolt"));

        let (stats, _, spaces) = daemon.pass(false).unwrap();
        assert_eq!(stats.spaces_labeled, 0);
        assert_eq!(by_id(&spaces, "w1").source, Source::Pending);
        assert_eq!(by_id(&spaces, "w1").label.as_deref(), Some("pika: crates"));

        let (stats, _, spaces) = daemon.pass(false).unwrap();
        assert_eq!(stats.spaces_labeled, 1);
        assert_eq!(by_id(&spaces, "w1").label.as_deref(), Some("pika: src"));
        assert!(by_id(&spaces, "w1").applied);

        daemon.restore_spaces();
        let states = spaces::load_states(&daemon.paths.spaces_file());
        assert_eq!(states["w1"].applied, None);
        assert_eq!(states["w2"].applied, None);

        let seen = finish(&daemon, server);
        let renames: Vec<(String, String)> = seen
            .iter()
            .filter(|r| r["method"] == "workspace.rename")
            .map(|r| {
                (
                    r["params"]["workspace_id"].as_str().unwrap().to_string(),
                    r["params"]["label"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            renames,
            vec![
                ("w1".to_string(), "pika: crates".to_string()),
                ("w1".to_string(), "pika: src".to_string()),
                ("w1".to_string(), "pika".to_string()),
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hand_named_spaces_are_never_renamed() {
        // w1 was released earlier (the user renamed it away from "pika" and we let go); w2 was
        // renamed by the user after we named it → released now. Neither is touched.
        let mut replies = vec![(
            "session.snapshot",
            snapshot(
                vec![
                    pane_json("w1:p1", "w1", "/x/pika", true, None),
                    pane_json("w2:p1", "w2", "/x/jolt", false, None),
                ],
                vec![ws("w1", "my project", true), ws("w2", "handpicked", false)],
            ),
        )];
        replies.extend(labelled_pane("w1:p1"));
        replies.extend(labelled_pane("w2:p1"));
        let (mut daemon, server) = mock_daemon("manual-spaces", replies);
        let mut states = spaces::SpaceStates::new();
        states.insert(
            "w1".into(),
            spaces::SpaceState {
                original: "pika".into(),
                applied: None,
                pending: None,
            },
        );
        states.insert(
            "w2".into(),
            spaces::SpaceState {
                original: "jolt".into(),
                applied: Some("cargo build".into()),
                pending: None,
            },
        );
        spaces::save_states(&daemon.paths.spaces_file(), &states).unwrap();
        let (stats, _, spaces) = daemon.pass(false).unwrap();
        assert_eq!(stats.spaces_skipped, 2);
        assert_eq!(stats.spaces_labeled, 0);
        assert!(
            spaces
                .iter()
                .all(|o| o.source == Source::SkippedManual && !o.applied)
        );
        let states = spaces::load_states(&daemon.paths.spaces_file());
        assert_eq!(
            states["w2"].applied, None,
            "released after the user's rename"
        );
        // Nothing to restore either.
        daemon.restore_spaces();
        assert!(
            finish(&daemon, server)
                .iter()
                .all(|r| r["method"] != "workspace.rename")
        );
    }

    #[test]
    fn panes_off_still_feed_spaces() {
        let root = std::env::temp_dir().join(format!("hal-panes-off-{}", std::process::id()));
        std::fs::create_dir_all(root.join("pika/.git")).unwrap();
        std::fs::create_dir_all(root.join("pika/crates")).unwrap();
        let cwd = root.join("pika/crates").to_string_lossy().into_owned();
        let mut replies = vec![(
            "session.snapshot",
            snapshot(
                vec![pane_json("w1:p1", "w1", &cwd, true, None)],
                vec![ws("w1", "pika", true)],
            ),
        )];
        replies.extend(unchanged_pane());
        replies.extend([
            (
                "workspace.get",
                json!({"result": {"workspace": {"workspace_id": "w1", "label": "pika"}}}),
            ),
            ("workspace.rename", json!({"result": {"type": "ok"}})),
        ]);
        let config = Config::parse("label_panes = false\n").unwrap();
        let (mut daemon, server) = mock_daemon_with("panes-off", config, replies);
        let (stats, panes, spaces) = daemon.pass(false).unwrap();
        assert_eq!(stats.labeled, 0);
        assert_eq!(panes[0].label.as_deref(), Some("crates"));
        assert!(!panes[0].applied);
        assert!(daemon.applied.is_empty());
        assert_eq!(spaces[0].label.as_deref(), Some("pika: crates"));
        assert!(spaces[0].applied);
        let seen = finish(&daemon, server);
        assert!(seen.iter().all(|r| r["method"] != "pane.report_metadata"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn manual_pane_labels_count_for_their_space() {
        let replies = vec![
            (
                "session.snapshot",
                snapshot(
                    vec![
                        json!({"pane_id": "w1:p1", "workspace_id": "w1", "cwd": "/x/pika",
                        "focused": true, "label": "billing rewrite"}),
                    ],
                    vec![ws("w1", "pika", true)],
                ),
            ),
            (
                "workspace.get",
                json!({"result": {"workspace": {"workspace_id": "w1", "label": "pika"}}}),
            ),
            ("workspace.rename", json!({"result": {"type": "ok"}})),
        ];
        let (mut daemon, server) = mock_daemon("manual-pane", replies);
        let (stats, panes, spaces) = daemon.pass(false).unwrap();
        assert_eq!(panes[0].source, Source::SkippedManual);
        assert_eq!(stats.spaces_labeled, 1);
        assert_eq!(spaces[0].label.as_deref(), Some("pika: billing rewrite"));
        let seen = finish(&daemon, server);
        assert_eq!(
            seen.last().unwrap()["params"]["label"],
            "pika: billing rewrite"
        );
    }

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
