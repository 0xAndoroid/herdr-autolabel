# herdr-autolabel

herdr plugin whose daemon labels every pane with a terse 1–2 word title describing what is happening in it — `feat/daemon`, `cargo build`, `nvim foo.rs`, `ssh host`, `review PR 1283` — and keeps it current (≈10 s lag) as activity changes.

## Install

```sh
cd ~/dev/herdr-autolabel && cargo build --release
herdr plugin link ~/dev/herdr-autolabel
herdr plugin action invoke andoroid.autolabel.start   # current session; auto-starts on later server starts
```

The `[[startup]]` hook runs on every herdr server start (and live handoff) and spawns the daemon detached; `plugin link` alone does not start it, hence the `start` action.

## How labels are derived

- **Idle shell** → git branch if the cwd is in a repo (read from `.git/HEAD`, no subprocess) and the branch is not a default one (`main`/`master`/`trunk`/`develop`), else cwd basename.
- **Known process** (cargo, git, npm/pnpm/yarn/bun, make, just, pytest, go, python, node, editors, ssh, htop, less, man, tail, docker, kubectl, gh, …) → deterministic `<tool> <subcommand|file|host>`, no LLM.
- **Agent panes** (claude, codex, pi, gemini, cursor, opencode, …) **and unknown long-running processes** → LLM over the scrubbed last 40 lines + cwd/branch/PR mentions; on failure or `provider = "none"` → `<agent> <branch|cwd>` / `<cmd> <arg>`.
- Cerebras qwen occasionally returns an empty completion under `reasoning_effort: none`; the call is retried once with `low` reasoning (~50–80 completion tokens) before falling back.

Manual renames always win: a pane with a `herdr pane rename` label is never titled, and any title we set on it is cleared.

## LLM provider

`provider = "auto"` picks the first with a key: **cerebras** (`qwen-3.8-27b`) → **anthropic** (`claude-haiku-4-5`) → **openai** (`gpt-5-mini`) → none. Keys come from the environment or `~/.keysrc` (`export CEREBRAS_API_KEY=…` lines). 16 output tokens, temperature 0, 8 s timeout, reasoning disabled.

## Rate limits & privacy

- Per pane ≥ `llm_per_pane_secs` (15 s) between calls; global rolling 60-second window `llm_global_per_min` (6/min); call timestamps shared through a locked state file across sockets, one-shot runs and restarts; labels cached by fingerprint (LRU 256). Unchanged panes cost nothing (fingerprint = process + cwd + branch + agent state + digit/spinner-normalised screen hash).
- Only the last 40 lines (≤300 chars each) leave the machine, after scrubbing: `key=value` secrets (token/api_key/secret/password/authorization/cookie), `Bearer …`, `sk-…`, `sk-ant-…`, `ghp_…`, `github_pat_…`, `xox?-…`, `AKIA…`, JWTs, private-key headers and any opaque 40+ char run → `[redacted]`. Pure-hex 40-character Git SHAs are preserved unless assigned to a secret key. Process arguments, agent metadata, cwd basename and branch are scrubbed too.

## Config

`$HERDR_PLUGIN_CONFIG_DIR/config.toml` (`herdr plugin config-dir andoroid.autolabel`); see `config.example.toml`. All keys optional. Unknown keys are ignored; invalid fields use defaults with a warning. Invalid TOML syntax rejects the whole file.

| key | default | meaning |
| --- | --- | --- |
| `interval_secs` | `10` | seconds between passes |
| `provider` | `"auto"` | `auto` / `cerebras` / `anthropic` / `openai` / `none` |
| `model` | provider default | model override |
| `max_chars` | `24` | label length cap (cut at a word boundary) |
| `lines` | `40` | pane lines inspected / sent |
| `llm_per_pane_secs` | `15` | min spacing between LLM calls per pane |
| `llm_global_per_min` | `6` | global LLM budget |
| `allow` | `[]` | globs on `pane_id` / `workspace_id` / cwd; non-empty = only these |
| `deny` | `[]` | globs; always win |

Globs match the entire pane ID, workspace ID or cwd. `*` includes `/`; `?` matches one Unicode character. Brackets and backslashes are literal; deny takes precedence.

## Operating

```sh
herdr-autolabel status          # JSON: running, pid, provider/model, last pass stats
herdr-autolabel stop            # SIGTERM the daemon (also: herdr plugin action invoke andoroid.autolabel.stop)
herdr-autolabel once [--force]  # one pass, prints per-pane labels (relabel action = once --force)  # note: a one-shot run from a second process treats the daemon's titles as foreign, clears and relabels them; the daemon re-applies within one interval
```

Add `--socket PATH` to target a named session. Logs: `$HERDR_PLUGIN_STATE_DIR/daemon.log` (`AUTOLABEL_LOG=debug` for verbose); status: `status.json`. The daemon exits by itself after 3 consecutive failed connects (server stopped). A held per-socket lock prevents duplicate daemons; PID identity is checked before stopping. SIGTERM/INT remove the pidfile after in-flight bounded I/O finishes; SIGKILL can leave a stale file, which is ignored. Without herdr's env the state dir falls back to `~/.local/state/herdr-autolabel/` and the socket to `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`.

macOS + Linux.

## Known limits

- A competing metadata title from another source makes the daemon skip that pane for as long as the title is present; once it disappears the pane is labelled again. Our own titles from a previous daemon run look the same (the snapshot does not expose title ownership): they are cleared once, then relabelled on the next pass. Before writing, we recheck the pane; herdr has no atomic compare-and-set, so a rename racing that final write is cleared on the next pass.
- Heuristics look at the first non-shell foreground process; pipelines label by their first command.
- Coding-agent panes are always LLM-labelled (cached per fingerprint); with `provider = "none"` they fall back to `<agent> <branch|cwd>`.
- Labels reflect the last 40 lines; a long-idle agent keeps its last label until the screen changes.
