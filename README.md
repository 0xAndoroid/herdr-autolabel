# herdr-autolabel

herdr plugin that names panes by their tasks and spaces by the combined work in all their panes. Folder, agent, and branch identity live on separate sidebar rows. Polls once per second; LLM requests run only for changed tasks, subject to a shared rate limit.

## Sidebar layout

Add to herdr's `config.toml`, then run `herdr server reload-config`:

```toml
[ui.sidebar.agents]
rows = [["state_icon", "pane"], ["agent", "$autolabel_folder"]]

[ui.sidebar.spaces]
rows = [["state_icon", "workspace"], ["$autolabel_folder", "$autolabel_branch"]]
```

Herdr inserts centered-dot separators between tokens and omits missing values. Agent entries show the pane's task above `codex · dotfiles`; spaces show their description above `dotfiles · main` (or just `home` without a branch). The plugin reports `autolabel_folder` on panes and workspaces, and `autolabel_branch` on workspaces. No agent aliases or semantic agent state are changed. Pika is the exception: its agent rows are `pika` / `pika TUI`, and its space is just `pika`, with a blank second row. Spaces keep two rows even when folder and branch are absent.

Folder identity comes from the main repository checkout, following a linked worktree's Git `commondir`, else the current folder. Leading dots are omitted (`.dotfiles` becomes `dotfiles`); the home directory becomes `home`. Spaces use the most common folder among their panes, ties to the first; a herdr-managed worktree supplies its repository and checkout. The branch belongs to that folder's first pane (or the explicit worktree checkout).

## Install

```sh
cd ~/dev/herdr-autolabel && cargo build --release
herdr plugin link ~/dev/herdr-autolabel
herdr server reload-config
herdr plugin action invoke andoroid.autolabel.start   # current session; auto-starts on later server starts
```

Update: `git pull && cargo build --release && herdr plugin link ~/dev/herdr-autolabel && herdr server reload-config && herdr plugin action invoke andoroid.autolabel.stop && herdr plugin action invoke andoroid.autolabel.start` (a running daemon keeps the old binary until restarted).

The `[[startup]]` hook runs on every herdr server start (and live handoff) and spawns the daemon detached; `plugin link` alone does not start it, hence the `start` action.

## Pane labels

- **Idle shell:** `shell`. Folder and branch are separate metadata, never the task name.
- **Known process:** a deterministic command such as `cargo build`, `nvim foo.rs`, or `SSH host`; no pane LLM request. The SSH alias `macmini` displays as `Mac Mini`. Uses the foreground process group leader, not a child it spawned.
- **New agent session:** `New session`, with no LLM request, for a Claude, Codex or Pi pane that is not working and has neither a transcript prompt nor a terminal-title summary yet. Folder names, the current directory path, and shell `user@host:path` titles do not count as task summaries. The first prompt, task title or working state switches it to the LLM path below; a pane that already had a task label keeps it.
- **Coding agent or unknown process:** the LLM describes the task only, without a project prefix. Context includes the latest substantive user prompt, first prompt, session title, command, repository, branch, and last 40 visible rows. Claude, Codex, and Pi prompts come from their local transcripts; Codex supports both `user_message` events and user `response_item` records, excluding injected instructions. Other agents use terminal titles. Unchanged tasks retain their names while the agent works. Pika TUI panes use `pika` directly.
- **Manual pane name:** preserved. Other sources' titles are also respected. Filtered panes contribute no context to workspace requests.

Empty, numeric-only, character-count (`25`, `23 characters`), and overlong replies are rejected. Overlong model names are rewritten, not cut mid-description. The first nonempty response line supplies the name, so a trailing count cannot replace it. A rejected reply gets one retry; Cerebras retries with low reasoning. If both attempts fail, keep the previous valid name or a task-only fallback (`working`, `ready`, or the command). Failures are not cached as successful tasks; later passes retry within the budget.

Titles use `pane.report_metadata` with source `plugin:autolabel`. `label_panes = false` stops title writes but still computes context for spaces and maintains folder metadata.

## Space labels

A space containing Pika is always named `pika` automatically, even with other panes present. A space whose only non-shell task is SSH uses its exact task name, such as `SSH Mac Mini`. An all-shell space is named `shell`, ignoring cached model names and old terminal output. These cases do not call the workspace LLM.

Other workspaces get an LLM-written description of **all allowed panes**, not the first or focused agent. Every included pane supplies its task label, request/session context, command, folder, branch, and fresh visible output when available. The prompt asks for a shared purpose, or a short description covering distinct tasks; it gives active tasks more weight than idle shells. Names omit the folder and branch shown on the second row.

Workspace fingerprints include every pane's identity, task fingerprint, and label. Adding/removing a pane or changing any task invalidates the name; focus, scrolling, and ordinary screen changes do not. Cached descriptions skip screen reads and LLM requests. Pane and workspace requests share the same global budget and per-target cooldown. Failed/rate-limited requests retain the previous name; without a previous name, one task is used directly, distinct tasks become `multiple tasks`, and empty shells become `shell`. `provider = "none"` uses these fallbacks.

**Hysteresis:** the first name is applied immediately. A replacement must remain the candidate for two passes; `once --force` bypasses this window, but not the rate limit.

**Ownership:** herdr has no display-only title for workspaces (only custom `$tokens`, which need a sidebar layout change), so spaces are renamed with `workspace.rename` — the same name `herdr workspace rename` sets — under these rules:

- A space we have never seen is taken over whatever it is called — herdr's default (cwd basename), a programmatic label such as the `pika` workspaces the Pika TUI opens — and its name is remembered.
- If you rename a space we named, we let go of it immediately and never rename it again until it is set back to the remembered name or to a herdr default (the cwd/repo basename of one of its panes, the worktree name, a number).
- The pre-rename name is remembered in `spaces-<socket hash>.json` in the state dir, so a restarted daemon still recognises its own names; `stop` (SIGTERM, the `stop` action) puts every original name back. The space then shows herdr's default again. Only spaces still carrying one of our names are restored.
- Before writing, the space is re-read (`workspace.get`); a rename racing the pass skips that pass.

Use the sidebar layout above to display folder and branch independently of the generated description.

## LLM provider

`provider = "auto"` picks the first with a key: **cerebras** (`qwen-3.8-27b`), else **openai** (`gpt-5.6-luna`), else none; `provider = "anthropic"` (`claude-haiku-4-5`) is available by name. Keys come from the environment or `~/.keysrc` (`export CEREBRAS_API_KEY=…` lines). Cerebras and Anthropic: 40 output tokens, temperature 0, reasoning off, 8 s timeout. OpenAI: `reasoning_effort` medium with a 2048-token completion budget for the reasoning plus the name, 30 s timeout.

Override the pane instructions with `prompt`, or the workspace instructions with `space_prompt`. Both receive the same enforced output-length instruction; keep overrides task-only.

## Rate limits & privacy

Every pass (see Operating) re-checks every pane's state; the LLM is called only when a fingerprint changed, not on a timer. Unchanged fingerprints reuse the previous result, and returning to a cached fingerprint reuses its label even if another context produced the same name. Cache misses are eligible immediately, subject to the rate limits below.

- Per pane ≥ `llm_per_pane_secs` (3 s) between calls, 15 s after a failed or rejected call; global rolling 60-second window `llm_global_per_min` (6/min); call timestamps shared through a locked state file across sockets, one-shot runs and restarts; labels cached by fingerprint (LRU 256). Unchanged panes cost one `process_info` call (fingerprint = foreground command + cwd + branch + agent + agent session + idle/working + the agent's terminal title; for Claude, Codex and Pi panes your last prompt replaces command, state and title, and the transcript is re-read only when it grew). Screen text never enters the fingerprint and is read only for a pane about to be sent to the LLM, so an LLM call happens only when a new command starts, an agent flips between working and idle, or you send Claude a new prompt. Workspace requests send the context and visible rows for every included pane, and share this budget. A request may retry once after an empty or invalid reply.
- Only the last 40 visible rows (≤300 chars each) and, for Claude, Codex and Pi panes, the first 300 characters each of your first and last prompts plus the agent's terminal title leave the machine, after scrubbing: `key=value` secrets (token/api_key/secret/password/authorization/cookie), `Bearer …`, `sk-…`, `sk-ant-…`, `ghp_…`, `github_pat_…`, `xox?-…`, `AKIA…`, JWTs, private-key headers and any opaque 40+ char run → `[redacted]`. Pure-hex 40-character Git SHAs are preserved unless assigned to a secret key. Process arguments, agent metadata, cwd basename and branch are scrubbed too.

## Config

`$HERDR_PLUGIN_CONFIG_DIR/config.toml` (`herdr plugin config-dir andoroid.autolabel`); see `config.example.toml`. All keys optional. Unknown keys are ignored; invalid fields use defaults with a warning. Invalid TOML syntax rejects the whole file.

| key | default | meaning |
| --- | --- | --- |
| `interval_secs` | `1` | seconds between pass starts; minimum 1 |
| `label_panes` | `true` | write pane titles (pane borders) |
| `label_spaces` | `true` | rename sidebar spaces after their panes |
| `provider` | `"auto"` | `auto` / `cerebras` / `anthropic` / `openai` / `none` |
| `model` | provider default | model override |
| `prompt` | built-in | pane naming instructions |
| `space_prompt` | built-in | all-pane workspace naming instructions |
| `debug` | `false` | scrubbed request/reply and decision logs; restart to apply |
| `max_chars` | `25` | label length cap (cut at a word boundary): pane titles, and space names as a whole |
| `lines` | `40` | screen rows sent to the LLM |
| `llm_per_pane_secs` | `3` | min spacing between requests per pane or workspace; minimum 3 |
| `llm_global_per_min` | `6` | shared pane/workspace request budget |
| `allow` | `[]` | globs on `pane_id` / `workspace_id` / cwd; non-empty = only these |
| `deny` | `[]` | globs; always win |

Globs match the entire pane ID, workspace ID or cwd. `*` includes `/`; `?` matches one Unicode character. Brackets and backslashes are literal; deny takes precedence. A space whose panes are all filtered out is left alone.

## Operating

```sh
herdr-autolabel status          # JSON: running, pid, provider/model, last pass stats (panes + spaces)
herdr-autolabel stop            # SIGTERM the daemon; restores original space names (also: herdr plugin action invoke andoroid.autolabel.stop)
herdr-autolabel once [--force]  # one pass, prints per-pane and per-space labels (relabel action = once --force)  # note: a one-shot run from a second process treats the daemon's pane titles as foreign, clears and relabels them; the daemon re-applies within one interval. Space ownership is shared through the state file, so both agree on which names are ours.
```

A pass starts `interval_secs` (1 s) after the previous pass started, or as soon as it ends when it ran longer. Model calls run inside the pass, so a pass that makes them lasts as long as those calls (up to the provider timeout each). Each pass re-checks every pane and calls the LLM only for panes whose fingerprint changed, at most once per `llm_per_pane_secs` (3 s) per pane, within the `llm_global_per_min` budget.

Add `--socket PATH` to target a named session. Logs: `$HERDR_PLUGIN_STATE_DIR/daemon.log` (`debug = true` in the plugin config, or `AUTOLABEL_LOG=debug`, for scrubbed context/replies, retries, cache/rate-limit decisions, and writes); status: `status.json`; space names: `spaces-<hash>.json`. The daemon exits by itself after 3 consecutive failed connects (server stopped). A held per-socket lock prevents duplicate daemons; PID identity is checked before stopping. SIGTERM/INT restore space names and remove the pidfile after in-flight bounded I/O finishes; SIGKILL can leave a stale file, which is ignored, and leaves our space names in place (a restarted daemon recognises them from the state file). Without herdr's env the state dir falls back to `~/.local/state/herdr-autolabel/` and the socket to `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`.

Socket client: one request per connection; socket timeouts are armed once right after connecting and never touched again — herdr closes its end as soon as the response is written, and macOS then rejects any `setsockopt` on that socket with `EINVAL` even though the unread tail of a multi-chunk response is still buffered (this was the intermittent `pass failed: io: Invalid argument (os error 22)`). Remaining transient socket errors (`EINVAL`/`ECONNRESET`/`EPIPE`/`EINTR`, a response cut before its newline) are retried once on a fresh connection at debug level. `cargo nextest run --run-ignored ignored-only stress` hammers this path.

Pane text is read with `source = visible` (the rows on screen, last `lines` of them). herdr answers a `recent`/`recent_unwrapped` text read on an idle agent's alternate screen that shows fewer rows than requested by scrolling the TUI with synthetic wheel events to harvest history and scrolling back, which redraws the pane on every pass; `visible` never touches the viewport.

macOS + Linux.

## Known limits

- A competing metadata title from another source makes the daemon skip that pane for as long as the title is present; once it disappears the pane is labelled again. Our own titles from a previous daemon run look the same (the snapshot does not expose title ownership): they are cleared once, then relabelled on the next pass; meanwhile valid titles contribute to workspace context. Before writing, we recheck the pane; herdr has no atomic compare-and-set, so a rename racing that final write is cleared on the next pass.
- Space names are real workspace labels, not metadata: without the state file (deleted, or a different `HERDR_PLUGIN_STATE_DIR`) a name we set looks like one you typed and is left alone; `herdr workspace rename <id> <cwd basename>` hands it back. Pane titles are not restored on stop (they are metadata and cleared on the next run).
- Names typed before the daemon first saw a space are not distinguishable from herdr's defaults: they are replaced while the daemon runs and put back on `stop`. Rename the space again (while the daemon runs) to keep your name.
- Heuristics look at the command typed at the prompt (the foreground process group leader; else the first non-shell process); pipelines label by their first command.
- Coding-agent panes use task-only `working`/`ready` fallbacks without a provider; Pika panes use `pika`.
- Labels reflect the visible screen (its last 40 rows); a long-idle agent keeps its last label until its command, state or request changes.
