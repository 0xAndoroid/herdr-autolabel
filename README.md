# herdr-autolabel

herdr plugin whose daemon labels every **pane** with a terse 1–2 word title describing what is happening in it — `feat/daemon`, `cargo build`, `nvim foo.rs`, `ssh host`, `review PR 1283` — and names every **space** (the workspace rows in herdr's sidebar) after its panes. Both follow activity with ≈10–20 s lag.

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
- **Known process** (cargo, git, npm/pnpm/yarn/bun, make, just, pytest, go, python, node, editors, ssh, htop, less, man, tail, docker, kubectl, gh, …) → deterministic `<tool> <subcommand|file|host>`, no LLM.
- **Agent panes** (claude, codex, pi, gemini, cursor, opencode, …) **and unknown long-running processes** → LLM over the scrubbed last 40 lines + cwd/branch/PR mentions; on failure or `provider = "none"` → `<agent> <branch|cwd>` / `<cmd> <arg>`.
- Cerebras qwen occasionally returns an empty completion under `reasoning_effort: none`; the call is retried once with `low` reasoning (~50–80 completion tokens) before falling back.

Pane titles are display-only metadata (`pane.report_metadata`, source `plugin:autolabel`) and show in pane borders. Manual renames always win: a pane with a `herdr pane rename` label is never titled, and any title we set on it is cleared. `label_panes = false` stops writing titles; labels are still computed for the spaces.

## Space labels

Each space gets **one** label (≤ `max_chars`) derived from the labels of its panes, in this order:

1. **One pane** → that pane's label verbatim (same fingerprint, no extra LLM call).
2. **An agent pane** (claude/codex/pi/…) → that pane's activity label; the focused agent pane when there are several.
3. **Dominant scope** → the non-default branch, else repo checkout name, else cwd basename shared by at least two panes (`feat/moving-button`, `pika`, `jolt`); ties go to the first pane.
4. Else the **focused pane's** label, else the first labelled pane's.

Manually renamed panes contribute their manual name; panes titled by another source contribute that title; filtered panes (`allow`/`deny`) contribute nothing.

**Hysteresis:** the first label is applied at once; afterwards a new label must be derived on 2 consecutive passes (≈20 s) before the space is renamed, so a `cargo build` in one pane does not flip the space. The `relabel` action (`once --force`) bypasses the window.

**Ownership:** herdr has no display-only title for workspaces (only custom `$tokens`, which need a sidebar layout change), so spaces are renamed with `workspace.rename` — the same name `herdr workspace rename` sets — under these rules:

- A space we have never seen is taken over whatever it is called — herdr's default (cwd basename), a programmatic label such as the `pika` workspaces the Pika TUI opens — and its name is remembered.
- If you rename a space we named, we let go of it immediately and never rename it again until it is set back to the remembered name or to a herdr default (the cwd/repo basename of one of its panes, the worktree name, a number).
- The pre-rename name is remembered in `spaces-<socket hash>.json` in the state dir, so a restarted daemon still recognises its own names; `stop` (SIGTERM, the `stop` action) puts every original name back. The space then shows herdr's default again. Only spaces still carrying one of our names are restored.
- Before writing, the space is re-read (`workspace.get`); a rename racing the pass skips that pass.

Sidebar space rows show `workspace` + `branch` by default, so the branch line is unchanged; the name line becomes the activity label.

## LLM provider

`provider = "auto"` picks the first with a key: **cerebras** (`qwen-3.8-27b`) → **anthropic** (`claude-haiku-4-5`) → **openai** (`gpt-5-mini`) → none. Keys come from the environment or `~/.keysrc` (`export CEREBRAS_API_KEY=…` lines). 16 output tokens, temperature 0, 8 s timeout, reasoning disabled.

## Rate limits & privacy

- Per pane ≥ `llm_per_pane_secs` (15 s) between calls; global rolling 60-second window `llm_global_per_min` (6/min); call timestamps shared through a locked state file across sockets, one-shot runs and restarts; labels cached by fingerprint (LRU 256). Unchanged panes cost nothing (fingerprint = process + cwd + branch + agent state + digit/spinner-normalised screen hash). Spaces never call the LLM: they reuse pane labels.
- Only the last 40 lines (≤300 chars each) leave the machine, after scrubbing: `key=value` secrets (token/api_key/secret/password/authorization/cookie), `Bearer …`, `sk-…`, `sk-ant-…`, `ghp_…`, `github_pat_…`, `xox?-…`, `AKIA…`, JWTs, private-key headers and any opaque 40+ char run → `[redacted]`. Pure-hex 40-character Git SHAs are preserved unless assigned to a secret key. Process arguments, agent metadata, cwd basename and branch are scrubbed too.

## Config

`$HERDR_PLUGIN_CONFIG_DIR/config.toml` (`herdr plugin config-dir andoroid.autolabel`); see `config.example.toml`. All keys optional. Unknown keys are ignored; invalid fields use defaults with a warning. Invalid TOML syntax rejects the whole file.

| key | default | meaning |
| --- | --- | --- |
| `interval_secs` | `10` | seconds between passes |
| `label_panes` | `true` | write pane titles (pane borders) |
| `label_spaces` | `true` | rename sidebar spaces after their panes |
| `provider` | `"auto"` | `auto` / `cerebras` / `anthropic` / `openai` / `none` |
| `model` | provider default | model override |
| `max_chars` | `24` | label length cap (cut at a word boundary), panes and spaces |
| `lines` | `40` | pane lines inspected / sent |
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

macOS + Linux.

## Known limits

- A competing metadata title from another source makes the daemon skip that pane for as long as the title is present; once it disappears the pane is labelled again. Our own titles from a previous daemon run look the same (the snapshot does not expose title ownership): they are cleared once, then relabelled on the next pass. Before writing, we recheck the pane; herdr has no atomic compare-and-set, so a rename racing that final write is cleared on the next pass.
- Space names are real workspace labels, not metadata: without the state file (deleted, or a different `HERDR_PLUGIN_STATE_DIR`) a name we set looks like one you typed and is left alone; `herdr workspace rename <id> <cwd basename>` hands it back. Pane titles are not restored on stop (they are metadata and cleared on the next run).
- Names typed before the daemon first saw a space are not distinguishable from herdr's defaults: they are replaced while the daemon runs and put back on `stop`. Rename the space again (while the daemon runs) to keep your name.
- Heuristics look at the first non-shell foreground process; pipelines label by their first command.
- Coding-agent panes are always LLM-labelled (cached per fingerprint); with `provider = "none"` they fall back to `<agent> <branch|cwd>`.
- Labels reflect the last 40 lines; a long-idle agent keeps its last label until the screen changes.
