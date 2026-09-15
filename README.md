# herdr-autolabel

herdr plugin whose daemon labels every **pane** with a terse 1–2 word title describing what is happening in it — `feat/daemon`, `cargo build`, `nvim foo.rs`, `ssh host`, `review PR 1283` — and names every **space** (the workspace rows in herdr's sidebar) `<project>: <activity>` after its panes — `pika: watching CI run`, `jolt: cargo build`. Polls every second; known commands normally update pane titles within ≈1 s and space names within ≈2 s. LLM calls and rate limits can add delay.

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

- **Idle shell** → git branch if the cwd is in a repo (read from `.git/HEAD`, no subprocess) and the branch is not a default one (`main`/`master`/`trunk`/`develop`), else cwd basename.
- **Known process** (cargo, git, npm/pnpm/yarn/bun, make, just, pytest, go, python, node, editors, ssh, htop, less, man, tail, docker, kubectl, gh, …) → deterministic `<tool> <subcommand|file|host>`, no LLM. The command **typed at the prompt** decides (herdr's foreground process group leader), not what it spawned: `shop dev --app api` running `docker logs` is an unknown command, so the LLM names it with the child as context (`api docker logs`) and renames it when the child changes phase (`docker build` → `docker logs`).
- **Agent panes** (claude, codex, pi, gemini, cursor, opencode, …) **and unknown long-running processes** → the LLM writes the whole `<project>: <task>` name (≤ `max_chars`) from the scrubbed screen (its last 40 visible rows) + project/cwd/branch/PR mentions, shortening the project (`herdr` for `herdr-autolabel`) or dropping it for a home or scratch folder (`dev`, dotfiles). For agents the **user's request** heads the prompt and the task must paraphrase it rather than name the file or command the agent is on at the moment. For Claude, Codex and Pi panes the request is your **last prompt verbatim** (first 300 characters), read from the transcript the agent keeps: Claude Code's `~/.claude/projects/*/<session id>.jsonl` (skipping tool results, hook feedback and slash-command echoes), Codex's `~/.codex/sessions/<date>/rollout-*-<session id>.jsonl` (its `user_message` events; `archived_sessions/` too), Pi's session file. A prompt that is only an acknowledgement (`continue`, `ok, do it`) does not count; the one before it does. Two more lines say what the whole session is about and shape only the project: the **session summary** the agent keeps in its terminal title (Claude Code; Codex when its title is configured to carry one; Pi keeps none) and the session's **first prompt**. When they, the branch or the command name an app or sub-project inside the repository, that becomes the project: `api: switch to fable` for a Claude session on the `api` app of the `shop` repo. herdr's agent integrations (`herdr integration install claude|codex|pi`) report the session per pane, as an id or the file path. Other agents contribute the summary they keep in the **terminal title** (`Worktree path migration`), unless it starts with the agent's own name. Such a pane is relabelled when your prompt changes and holds still while the agent works on it. On failure or `provider = "none"` → `<agent> <branch|cwd>` / `<cmd> <arg>`.
- **Pika panes** (the Pika assistant TUI) are named `pika`, pane and space alike, without the LLM: its conversations are not the pane's work.
- Cerebras qwen occasionally returns an empty completion under `reasoning_effort: none`; the call is retried once with `low` reasoning (~50–80 completion tokens) before falling back.

Pane titles are display-only metadata (`pane.report_metadata`, source `plugin:autolabel`) and show in pane borders. Manual renames always win: a pane with a `herdr pane rename` label is never titled, and any title we set on it is cleared. `label_panes = false` stops writing titles; labels are still computed for the spaces.

## Space labels

Each space gets **one** name of at most `max_chars` (25) characters, taken from its **primary pane**: the first **agent pane** (claude/codex/pi/…), else the first pane **running a command** (or carrying a manual name), else the first labelled pane. Focus plays no part, so switching panes never renames the space.

- An **LLM-written** pane label is already a whole name (`herdr: fix label rules`, `relocate worktrees`) and is used as it is.
- A **heuristic** label becomes `<project>: <activity>` — where the work happens, then what it is (`jolt: cargo build`). The project is herdr's repository name when the space is a worktree, else the repo checkout name (else cwd basename) named by most panes, ties to the first pane; worktree checkouts named `<repo>.<branch>` (worktrunk's default) are cut at the first dot — `jolt.keccak-xorrotl-fusion` → `jolt` — while a leading dot stays (`.dotfiles`). The activity is cut to whole words in the room the project leaves (a cut never ends in a connective such as `to`/`for`); the project stands alone when the activity repeats it (idle shells at the repo root: `pika`) or when not even its first word fits; the activity alone when no pane has a cwd.

Manually renamed panes contribute their manual name as the activity (`pika: billing rewrite`); panes titled by another source contribute that title as a whole name. Filtered panes (`allow`/`deny`) contribute nothing, and neither does a pane still waiting for its LLM name (rate-limited, failed, or `provider = "none"`; its own title is the `<agent> <branch|cwd>` stand-in): a space whose only agent pane has no name yet keeps herdr's default, the folder name. Spaces never call the LLM.

**Hysteresis:** the first label is applied at once; afterwards a new label must be derived on 2 consecutive passes (≈1–2 s with the default interval) before the space is renamed, so commands that disappear before the second pass do not rename it. The `relabel` action (`once --force`) bypasses the window.

**Ownership:** herdr has no display-only title for workspaces (only custom `$tokens`, which need a sidebar layout change), so spaces are renamed with `workspace.rename` — the same name `herdr workspace rename` sets — under these rules:

- A space we have never seen is taken over whatever it is called — herdr's default (cwd basename), a programmatic label such as the `pika` workspaces the Pika TUI opens — and its name is remembered.
- If you rename a space we named, we let go of it immediately and never rename it again until it is set back to the remembered name or to a herdr default (the cwd/repo basename of one of its panes, the worktree name, a number).
- The pre-rename name is remembered in `spaces-<socket hash>.json` in the state dir, so a restarted daemon still recognises its own names; `stop` (SIGTERM, the `stop` action) puts every original name back. The space then shows herdr's default again. Only spaces still carrying one of our names are restored.
- Before writing, the space is re-read (`workspace.get`); a rename racing the pass skips that pass.

Sidebar space rows show `workspace` + `branch` by default, so the branch line is unchanged; the name line becomes the space name above.

## LLM provider

`provider = "auto"` picks the first with a key: **cerebras** (`qwen-3.8-27b`), else **openai** (`gpt-5.6-luna`), else none; `provider = "anthropic"` (`claude-haiku-4-5`) is available by name. Keys come from the environment or `~/.keysrc` (`export CEREBRAS_API_KEY=…` lines). Cerebras and Anthropic: 40 output tokens, temperature 0, reasoning off, 8 s timeout. OpenAI: `reasoning_effort` medium with a 2048-token completion budget for the reasoning plus the name, 30 s timeout.

The system prompt is the `prompt` config key; the built-in text is in `config.example.toml`. Put project-specific examples (your app and repository names) there rather than in the source.

## Rate limits & privacy

Polling checks for changed context; it does not call the LLM on a timer. Unchanged fingerprints reuse the previous result, and returning to a cached fingerprint reuses its label even if another context produced the same name. Cache misses are eligible immediately, subject to the rate limits below.

- Per pane ≥ `llm_per_pane_secs` (15 s) between calls; global rolling 60-second window `llm_global_per_min` (6/min); call timestamps shared through a locked state file across sockets, one-shot runs and restarts; labels cached by fingerprint (LRU 256). Unchanged panes cost one `process_info` call (fingerprint = foreground command + cwd + branch + agent + idle/working + the agent's terminal title; for Claude, Codex and Pi panes your last prompt replaces command, state and title, and the transcript is re-read only when it grew). Screen text never enters the fingerprint and is read only for a pane about to be sent to the LLM, so an LLM call happens only when a new command starts, an agent flips between working and idle, or you send Claude a new prompt. Spaces never call the LLM: they reuse pane labels.
- Only the last 40 visible rows (≤300 chars each) and, for Claude, Codex and Pi panes, the first 300 characters each of your first and last prompts plus the agent's terminal title leave the machine, after scrubbing: `key=value` secrets (token/api_key/secret/password/authorization/cookie), `Bearer …`, `sk-…`, `sk-ant-…`, `ghp_…`, `github_pat_…`, `xox?-…`, `AKIA…`, JWTs, private-key headers and any opaque 40+ char run → `[redacted]`. Pure-hex 40-character Git SHAs are preserved unless assigned to a secret key. Process arguments, agent metadata, cwd basename and branch are scrubbed too.

## Config

`$HERDR_PLUGIN_CONFIG_DIR/config.toml` (`herdr plugin config-dir andoroid.autolabel`); see `config.example.toml`. All keys optional. Unknown keys are ignored; invalid fields use defaults with a warning. Invalid TOML syntax rejects the whole file.

| key | default | meaning |
| --- | --- | --- |
| `interval_secs` | `1` | seconds between passes; minimum 1 |
| `label_panes` | `true` | write pane titles (pane borders) |
| `label_spaces` | `true` | rename sidebar spaces after their panes |
| `provider` | `"auto"` | `auto` / `cerebras` / `anthropic` / `openai` / `none` |
| `model` | provider default | model override |
| `prompt` | built-in | system prompt for the LLM; replaces the built-in text |
| `max_chars` | `25` | label length cap (cut at a word boundary): pane titles, and space names as a whole |
| `lines` | `40` | screen rows sent to the LLM |
| `llm_per_pane_secs` | `15` | min spacing between LLM calls per pane |
| `llm_global_per_min` | `6` | global LLM budget |
| `allow` | `[]` | globs on `pane_id` / `workspace_id` / cwd; non-empty = only these |
| `deny` | `[]` | globs; always win |

Globs match the entire pane ID, workspace ID or cwd. `*` includes `/`; `?` matches one Unicode character. Brackets and backslashes are literal; deny takes precedence. A space whose panes are all filtered out is left alone.

## Operating

```sh
herdr-autolabel status          # JSON: running, pid, provider/model, last pass stats (panes + spaces)
herdr-autolabel stop            # SIGTERM the daemon; restores original space names (also: herdr plugin action invoke andoroid.autolabel.stop)
herdr-autolabel once [--force]  # one pass, prints per-pane and per-space labels (relabel action = once --force)  # note: a one-shot run from a second process treats the daemon's pane titles as foreign, clears and relabels them; the daemon re-applies within one interval. Space ownership is shared through the state file, so both agree on which names are ours.
```

Add `--socket PATH` to target a named session. Logs: `$HERDR_PLUGIN_STATE_DIR/daemon.log` (`AUTOLABEL_LOG=debug` for verbose); status: `status.json`; space names: `spaces-<hash>.json`. The daemon exits by itself after 3 consecutive failed connects (server stopped). A held per-socket lock prevents duplicate daemons; PID identity is checked before stopping. SIGTERM/INT restore space names and remove the pidfile after in-flight bounded I/O finishes; SIGKILL can leave a stale file, which is ignored, and leaves our space names in place (a restarted daemon recognises them from the state file). Without herdr's env the state dir falls back to `~/.local/state/herdr-autolabel/` and the socket to `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`.

Socket client: one request per connection; socket timeouts are armed once right after connecting and never touched again — herdr closes its end as soon as the response is written, and macOS then rejects any `setsockopt` on that socket with `EINVAL` even though the unread tail of a multi-chunk response is still buffered (this was the intermittent `pass failed: io: Invalid argument (os error 22)`). Remaining transient socket errors (`EINVAL`/`ECONNRESET`/`EPIPE`/`EINTR`, a response cut before its newline) are retried once on a fresh connection at debug level. `cargo nextest run --run-ignored ignored-only stress` hammers this path.

Pane text is read with `source = visible` (the rows on screen, last `lines` of them). herdr answers a `recent`/`recent_unwrapped` text read on an idle agent's alternate screen that shows fewer rows than requested by scrolling the TUI with synthetic wheel events to harvest history and scrolling back, which redraws the pane on every pass; `visible` never touches the viewport.

macOS + Linux.

## Known limits

- A competing metadata title from another source makes the daemon skip that pane for as long as the title is present; once it disappears the pane is labelled again. Our own titles from a previous daemon run look the same (the snapshot does not expose title ownership): they are cleared once, then relabelled on the next pass; meanwhile they name their spaces as they are. Before writing, we recheck the pane; herdr has no atomic compare-and-set, so a rename racing that final write is cleared on the next pass.
- Space names are real workspace labels, not metadata: without the state file (deleted, or a different `HERDR_PLUGIN_STATE_DIR`) a name we set looks like one you typed and is left alone; `herdr workspace rename <id> <cwd basename>` hands it back. Pane titles are not restored on stop (they are metadata and cleared on the next run).
- Names typed before the daemon first saw a space are not distinguishable from herdr's defaults: they are replaced while the daemon runs and put back on `stop`. Rename the space again (while the daemon runs) to keep your name.
- Heuristics look at the command typed at the prompt (the foreground process group leader; else the first non-shell process); pipelines label by their first command.
- Coding-agent panes are always LLM-labelled (cached per fingerprint; Pika panes are named `pika` instead); with `provider = "none"` they fall back to `<agent> <branch|cwd>`.
- Labels reflect the visible screen (its last 40 rows); a long-idle agent keeps its last label until its command, state or request changes.
