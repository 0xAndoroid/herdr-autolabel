use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::{Daemon, PassStats, SHUTDOWN, Source, SpaceOutcome};
use crate::herdr::{self, PaneInfo, Snapshot};
use crate::logging::{log_debug, log_info, log_warn};
use crate::{git, llm, scrub, spaces};

impl Daemon {
    pub(super) fn handle_spaces(
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
        self.space_fps.retain(|id, _| alive.contains(id.as_str()));
        let mut dirty = states.len() != before;
        stats.spaces = snapshot.workspaces.len();
        let mut outcomes = Vec::with_capacity(snapshot.workspaces.len());
        let mut failed = None;

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
            let summaries: Vec<(&str, &spaces::PaneSummary)> = panes
                .iter()
                .filter_map(|p| self.labels.get(&p.pane_id).map(|s| (p.pane_id.as_str(), s)))
                .collect();
            let pika = panes.iter().any(|pane| {
                pane.agent.as_deref() == Some("pika")
                    && self
                        .config
                        .permits(&[&pane.pane_id, id, pane.effective_cwd()])
            });
            if !pika
                && panes.iter().any(|pane| {
                    self.config
                        .permits(&[&pane.pane_id, id, pane.effective_cwd()])
                        && !self.labels.contains_key(&pane.pane_id)
                })
            {
                outcomes.push(outcome(None, Source::Pending, false));
                continue;
            }
            let folder = if pika {
                ""
            } else {
                ws.worktree
                    .as_ref()
                    .map(|w| w.repo_name.trim_start_matches('.'))
                    .or_else(|| spaces::dominant_project(&summaries))
                    .unwrap_or("")
            };
            let cwd = ws
                .worktree
                .as_ref()
                .map(|w| w.checkout_path.as_str())
                .or_else(|| {
                    panes
                        .iter()
                        .find(|p| {
                            self.labels
                                .get(&p.pane_id)
                                .is_some_and(|s| s.project.as_deref() == Some(folder))
                        })
                        .map(|p| p.effective_cwd())
                })
                .unwrap_or("");
            let branch = if pika {
                String::new()
            } else {
                git::branch_for(Path::new(cwd)).unwrap_or_default()
            };
            // Herdr drops tokenless rows and trims whitespace. A zero-width space keeps the row.
            let folder_text = if folder.is_empty() && branch.is_empty() {
                "\u{200b}"
            } else {
                folder
            };
            let location_changed = ws
                .tokens
                .get(herdr::FOLDER_TOKEN)
                .map_or("", String::as_str)
                != folder_text
                || ws
                    .tokens
                    .get(herdr::BRANCH_TOKEN)
                    .map_or("", String::as_str)
                    != branch;
            if location_changed
                && let Err(e) = self.client.set_workspace_location(id, folder_text, &branch)
            {
                failed = Some(e);
                break;
            }
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
                        self.space_fps.remove(id);
                        state.original.clone_from(&ws.label);
                        dirty = true;
                    }
                }
                spaces::Ownership::Ours => {}
            }
            let fp = spaces::fingerprint(&summaries);
            let previous = state
                .pending
                .as_ref()
                .map(|(s, _)| s.clone())
                .or_else(|| state.applied.clone())
                .filter(|s| !llm::postprocess(s, self.config.max_chars).is_empty());
            let fallback = || {
                previous
                    .clone()
                    .or_else(|| spaces::fallback(&summaries, self.config.max_chars))
            };
            let direct = if pika {
                Some("pika".to_string())
            } else {
                spaces::fallback(&summaries, self.config.max_chars)
                    .filter(|s| s == "shell" || s.starts_with("SSH "))
            };
            let (candidate, source) = if let Some(name) = direct {
                self.space_fps.remove(id);
                (Some(name), Source::Heuristic)
            } else if summaries.is_empty() {
                (None, Source::SkippedFilter)
            } else if !force && self.space_fps.get(id) == Some(&fp) {
                (previous, Source::Unchanged)
            } else if let Some(cached) = self.cache.get(fp) {
                self.space_fps.insert(id.clone(), fp);
                (Some(cached), Source::Cache)
            } else if let Some(provider) = &self.provider {
                let budget_key = format!("{}:space:{id}", self.paths.socket.display());
                if self.limiter.try_acquire(&budget_key) {
                    let mut context = String::new();
                    for (pane_id, summary) in &summaries {
                        context.push_str(&format!(
                            "\npane {pane_id}\ntask: {}\n{}",
                            scrub::scrub_line(&summary.label),
                            summary.context
                        ));
                        match self.client.read_visible(pane_id, self.config.lines) {
                            Ok(screen) => {
                                context
                                    .push_str(&scrub::scrub_lines(screen.text.lines()).join("\n"));
                                context.push('\n');
                            }
                            Err(e) => log_debug!("{id}: {pane_id} screen unavailable: {e}"),
                        }
                    }
                    stats.llm_calls += 1;
                    self.total_llm_calls += 1;
                    match provider.label_space(&context, self.config.max_chars) {
                        Ok(label) => {
                            log_info!(
                                "{id}: workspace llm {provider} -> {label:?} ({} panes)",
                                summaries.len()
                            );
                            self.cache.insert(fp, label.clone());
                            self.space_fps.insert(id.clone(), fp);
                            (Some(label), Source::Llm)
                        }
                        Err(e) => {
                            stats.llm_errors += 1;
                            self.limiter.back_off(&budget_key);
                            self.last_error = Some(format!("{id}: {e}"));
                            log_warn!("{id}: workspace llm failed: {e}");
                            (fallback(), Source::Fallback)
                        }
                    }
                } else {
                    log_debug!("{id}: workspace rate-limited; keeping previous name");
                    (fallback(), Source::RateLimited)
                }
            } else {
                (
                    spaces::fallback(&summaries, self.config.max_chars),
                    Source::Aggregate,
                )
            };
            log_debug!("{id}: fingerprint={fp:016x} source={source:?} candidate={candidate:?}");
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
                state.applied = Some(next.clone());
                dirty = true;
                outcomes.push(outcome(Some(next), source, false));
                continue;
            }
            // Pane labelling (and its LLM calls) ran after the snapshot; recheck the name.
            let current = match self.client.workspace(id) {
                Ok(w) => w,
                Err(herdr::Error::Api { .. }) => continue, // closed meanwhile
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            if current.label != ws.label {
                log_debug!("{id}: space renamed during the pass; skipping");
                outcomes.push(outcome(None, Source::SkippedManual, false));
                continue;
            }
            if let Err(e) = self.client.rename_workspace(id, &next) {
                failed = Some(e);
                break;
            }
            state.applied = Some(next.clone());
            dirty = true;
            stats.spaces_labeled += 1;
            self.total_spaces_applied += 1;
            log_debug!("{id}: space ← {next:?}");
            outcomes.push(outcome(Some(next), source, true));
        }
        if dirty && let Err(e) = spaces::save_states(&self.paths.spaces_file(), &states) {
            log_warn!("spaces state write failed: {e}");
        }
        self.spaces = states;
        failed.map_or(Ok(outcomes), Err)
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
}
