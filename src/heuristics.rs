//! Deterministic labels for well-known foreground processes; decides when the LLM is needed.

use std::path::Path;

use crate::label;

/// A foreground process as reported by `pane.process_info`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proc {
    pub name: String,
    pub argv: Vec<String>,
}

impl Proc {
    pub fn new(argv: &[&str]) -> Self {
        Self {
            name: argv.first().map(|a| basename(a)).unwrap_or_default(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Command name: basename of argv[0] (leading `-` of login shells stripped), else `name`.
    pub fn command(&self) -> String {
        let raw = self
            .argv
            .first()
            .map(|a| basename(a))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| self.name.clone());
        raw.trim_start_matches('-').to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneFacts {
    pub fg: Option<Proc>,
    pub cwd: String,
    pub branch: Option<String>,
    /// Agent kind from herdr (`claude`, `codex`, …).
    pub agent: Option<String>,
    pub agent_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Deterministic label; no LLM needed.
    Label(String),
    /// Ask the LLM; use `fallback` when it fails or is unavailable.
    Llm { fallback: String },
}

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "nu", "dash", "ksh", "tcsh", "csh", "ash", "elvish", "xonsh",
    "pwsh",
];
const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "pi",
    "gemini",
    "cursor",
    "cursor-agent",
    "opencode",
    "aider",
    "goose",
    "amp",
    "copilot",
    "droid",
    "kiro",
    "qwen",
    "crush",
];
/// Wrappers whose real command follows.
const WRAPPERS: &[&str] = &[
    "sudo",
    "env",
    "time",
    "nohup",
    "nice",
    "caffeinate",
    "doas",
    "command",
    "exec",
    "stdbuf",
    "unbuffer",
    "script",
];
/// Full-screen tools labelled by name alone.
const BY_NAME: &[&str] = &[
    "htop",
    "btop",
    "top",
    "glances",
    "k9s",
    "lazygit",
    "lazydocker",
    "tig",
    "gitui",
    "ranger",
    "yazi",
    "nnn",
    "mc",
    "ncdu",
    "bmon",
    "iftop",
    "nvtop",
    "gdb",
    "lldb",
    "irb",
    "psql",
    "mysql",
    "sqlite3",
    "redis-cli",
    "mongosh",
    "tmux",
    "screen",
    "watch",
    "journalctl",
];
const EDITORS: &[&str] = &[
    "vim", "nvim", "vi", "hx", "helix", "nano", "emacs", "micro", "kak",
];
const PAGERS: &[&str] = &["less", "more", "bat", "cat", "man", "glow", "mdcat"];
const SUBCOMMAND_TOOLS: &[&str] = &[
    "cargo",
    "git",
    "docker",
    "podman",
    "kubectl",
    "terraform",
    "gh",
    "glab",
    "brew",
    "apt",
    "apt-get",
    "pip",
    "pip3",
    "uv",
    "poetry",
    "pnpm",
    "yarn",
    "bun",
    "npm",
    "npx",
    "go",
    "just",
    "make",
    "gradle",
    "mvn",
    "dotnet",
    "swift",
    "xcodebuild",
    "flutter",
    "rustup",
    "helm",
    "aws",
    "gcloud",
    "az",
    "systemctl",
    "nix",
    "wt",
    "hunk",
    "herdr",
    "pika-cli",
    "pokernow",
    "cal",
    "gws",
    "codesign",
    "task",
];
const NETWORK_TOOLS: &[&str] = &["curl", "wget", "rsync", "scp", "sftp", "ping", "nc", "mosh"];

pub fn is_shell(cmd: &str) -> bool {
    SHELLS.contains(&cmd.trim_start_matches('-'))
}

pub fn is_agent(cmd: &str) -> bool {
    AGENTS.contains(&cmd)
}

/// Agent kind when the process is (or wraps) a coding agent, e.g. `claude`, `node …/claude`,
/// `node …/@anthropic-ai/claude-code/cli.js`.
pub fn agent_of(proc_: &Proc) -> Option<String> {
    let cmd = proc_.command();
    if is_agent(&cmd) {
        return Some(cmd);
    }
    if !matches!(
        cmd.as_str(),
        "node" | "bun" | "deno" | "tsx" | "ts-node" | "python" | "python3"
    ) {
        return None;
    }
    let script = proc_.argv.get(1)?;
    script.split('/').rev().find_map(|part| {
        let part = part.trim_end_matches(".js").trim_end_matches(".mjs");
        let base = part.strip_suffix("-code").unwrap_or(part);
        is_agent(base).then(|| base.to_string())
    })
}

pub fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Picks the process that best describes the pane from a foreground process group: the first
/// non-shell entry, or `None` when the group is empty or only shells (an idle prompt).
pub fn pick_foreground(procs: &[Proc]) -> Option<Proc> {
    procs.iter().find(|p| !is_shell(&p.command())).cloned()
}

/// Default branches say nothing about the work; the directory name is more useful then.
fn is_default_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "trunk" | "develop")
}

/// What groups panes of one space together: the non-default branch, else the repository
/// checkout name, else the cwd basename. `None` without a cwd.
pub fn scope(facts: &PaneFacts) -> Option<String> {
    if let Some(b) = facts
        .branch
        .as_deref()
        .filter(|b| !b.is_empty() && !is_default_branch(b))
    {
        return Some(b.to_string());
    }
    let cwd = facts.cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return None;
    }
    crate::git::repo_basename(Path::new(cwd))
        .or_else(|| Some(basename(cwd)))
        .filter(|s| !s.is_empty())
}

fn idle_label(facts: &PaneFacts) -> String {
    match &facts.branch {
        Some(b) if !b.is_empty() && !is_default_branch(b) => b.clone(),
        _ => {
            let base = basename(facts.cwd.trim_end_matches('/'));
            if base.is_empty() {
                "shell".into()
            } else {
                base
            }
        }
    }
}

/// First argv element after the command that is not a flag, skipping `skip_with_value` flag
/// arguments.
fn first_positional<'a>(args: &'a [String], skip_with_value: &[&str]) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a.starts_with('-') {
            if skip_with_value.contains(&a.as_str()) {
                iter.next();
            }
            continue;
        }
        if a.starts_with('+') {
            continue;
        }
        return Some(a.as_str());
    }
    None
}

fn positionals(args: &[String]) -> Vec<&str> {
    args.iter()
        .filter(|a| !a.starts_with('-') && !a.starts_with('+'))
        .map(String::as_str)
        .collect()
}

fn two(a: &str, b: Option<&str>) -> String {
    match b {
        Some(b) if !b.is_empty() => format!("{a} {b}"),
        _ => a.to_string(),
    }
}

/// Label for a known process, or `None` when unknown.
fn known_process_label(proc_: &Proc) -> Option<String> {
    let cmd = proc_.command();
    let args: &[String] = proc_.argv.get(1..).unwrap_or(&[]);

    if WRAPPERS.contains(&cmd.as_str()) {
        // Recurse into the wrapped command (skip env assignments / wrapper flags).
        let inner: Vec<&str> = args
            .iter()
            .map(String::as_str)
            .skip_while(|a| a.starts_with('-') || (cmd == "env" && a.contains('=')))
            .collect();
        return if inner.is_empty() {
            Some(cmd)
        } else {
            let p = Proc::new(&inner);
            known_process_label(&p).or_else(|| Some(generic_label(&p)))
        };
    }
    if BY_NAME.contains(&cmd.as_str()) {
        return Some(if cmd == "watch" {
            two(
                "watch",
                first_positional(args, &["-n", "--interval", "-d"])
                    .map(basename)
                    .as_deref(),
            )
        } else {
            cmd
        });
    }
    if EDITORS.contains(&cmd.as_str()) {
        let file = first_positional(args, &["-c", "-u", "-S", "--cmd", "-w", "-s"]).map(basename);
        return Some(two(&cmd, file.as_deref()));
    }
    if PAGERS.contains(&cmd.as_str()) {
        let target = positionals(args).last().map(|s| basename(s));
        return Some(two(&cmd, target.as_deref()));
    }
    match cmd.as_str() {
        "ssh" => {
            let host = first_positional(
                args,
                &[
                    "-p", "-i", "-l", "-L", "-R", "-o", "-J", "-F", "-D", "-W", "-b", "-c", "-e",
                    "-m", "-O", "-Q", "-S", "-w", "-E", "-B", "-I",
                ],
            )?;
            let host = host.rsplit('@').next().unwrap_or(host);
            let host = host.strip_prefix("ssh://").unwrap_or(host);
            let host = host.split(':').next().unwrap_or(host);
            Some(two("ssh", Some(host)))
        }
        "tail" => {
            let file = positionals(args).last().map(|s| basename(s));
            let follow = args
                .iter()
                .any(|a| a == "-f" || a == "-F" || a.starts_with("--follow"));
            Some(match (follow, file) {
                (true, Some(f)) => format!("tail -f {f}"),
                (true, None) => "tail -f".into(),
                (_, f) => two("tail", f.as_deref()),
            })
        }
        "python" | "python3" | "python2" | "node" | "ruby" | "perl" | "deno" | "lua" | "php"
        | "tsx" | "ts-node" => {
            let base = cmd
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string();
            let base = if base == "python" || base == "node" || base == "ruby" {
                base
            } else {
                cmd.clone()
            };
            if let Some(idx) = args.iter().position(|a| a == "-m")
                && let Some(module) = args.get(idx + 1)
            {
                return Some(if module == "pytest" {
                    "pytest".into()
                } else {
                    two(&base, Some(module.rsplit('.').next().unwrap_or(module)))
                });
            }
            if cmd == "bun"
                && args
                    .first()
                    .is_some_and(|a| !a.ends_with(".ts") && !a.ends_with(".js"))
            {
                return Some(two("bun", first_positional(args, &[])));
            }
            let script =
                first_positional(args, &["-c", "-W", "-X", "-e", "-r", "--require"]).map(basename);
            Some(two(&base, script.as_deref()))
        }
        "pytest" | "py.test" => Some("pytest".into()),
        "jest" | "vitest" | "mocha" => Some(cmd),
        "docker-compose" => Some("docker compose".into()),
        "npm" | "pnpm" | "yarn" | "bun" | "npx" | "bunx" => {
            let mut pos = positionals(args).into_iter();
            let first = pos.next();
            match first {
                Some("run") | Some("exec") | Some("x") | Some("dlx") => {
                    Some(two(&cmd, pos.next().map(basename).as_deref()))
                }
                Some(sub) if cmd == "npx" || cmd == "bunx" => Some(two(&cmd, Some(&basename(sub)))),
                Some(sub) => Some(two(&cmd, Some(sub))),
                None => Some(cmd),
            }
        }
        "uv" => {
            let mut pos = positionals(args).into_iter();
            match pos.next() {
                Some("run") => {
                    let run = args.iter().position(|a| a == "run").unwrap();
                    let rest: Vec<&str> = args[run + 1..].iter().map(String::as_str).collect();
                    if rest.is_empty() {
                        Some("uv run".into())
                    } else {
                        let p = Proc::new(&rest);
                        known_process_label(&p).or_else(|| Some(generic_label(&p)))
                    }
                }
                Some(sub) => Some(two("uv", Some(sub))),
                None => Some("uv".into()),
            }
        }
        "cargo" => Some(two(
            "cargo",
            first_positional(
                args,
                &["--manifest-path", "-p", "--package", "-Z", "--config"],
            ),
        )),
        "git" => Some(two(
            "git",
            first_positional(args, &["-C", "-c", "--git-dir", "--work-tree"]),
        )),
        "make" => Some(two(
            "make",
            first_positional(args, &["-C", "-f", "-j", "-o", "-W", "-I"])
                .filter(|a| !a.contains('=')),
        )),
        "just" => Some(two(
            "just",
            first_positional(
                args,
                &["-f", "--justfile", "-d", "--working-directory", "--set"],
            ),
        )),
        "go" => Some(two("go", first_positional(args, &["-C"]))),
        "docker" | "podman" => {
            let pos = positionals(args);
            match pos.as_slice() {
                ["compose", ..] => Some(format!("{cmd} compose")),
                [sub, ..] => Some(two(&cmd, Some(sub))),
                [] => Some(cmd),
            }
        }
        _ if SUBCOMMAND_TOOLS.contains(&cmd.as_str()) => {
            Some(two(&cmd, first_positional(args, &["-R", "--repo", "-C"])))
        }
        _ if NETWORK_TOOLS.contains(&cmd.as_str()) => Some(cmd),
        _ => None,
    }
}

/// Best-effort label for an unknown process: `<cmd> <first non-numeric positional basename>`.
pub fn generic_label(proc_: &Proc) -> String {
    let cmd = proc_.command();
    let args: &[String] = proc_.argv.get(1..).unwrap_or(&[]);
    let arg = positionals(args)
        .into_iter()
        .find(|a| a.parse::<f64>().is_err())
        .map(basename);
    two(&cmd, arg.as_deref())
}

/// Decides how to label a pane.
pub fn decide(facts: &PaneFacts, max_chars: usize) -> Decision {
    let fin = |s: &str| {
        let cleaned = label::finalize(s, 3, max_chars);
        if cleaned.is_empty() {
            label::truncate_words("shell", max_chars)
        } else {
            cleaned
        }
    };
    let agent_kind = facts
        .agent
        .clone()
        .filter(|a| !a.is_empty())
        .or_else(|| facts.fg.as_ref().and_then(agent_of));

    if let Some(agent) = agent_kind {
        let fallback = fin(&format!("{agent} {}", idle_label(facts)));
        return Decision::Llm { fallback };
    }
    let Some(proc_) = &facts.fg else {
        return Decision::Label(fin(&idle_label(facts)));
    };
    if is_shell(&proc_.command()) {
        return Decision::Label(fin(&idle_label(facts)));
    }
    match known_process_label(proc_) {
        Some(l) => Decision::Label(fin(&l)),
        None => Decision::Llm {
            fallback: fin(&generic_label(proc_)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(argv: &[&str], cwd: &str, branch: Option<&str>) -> PaneFacts {
        PaneFacts {
            fg: if argv.is_empty() {
                None
            } else {
                Some(Proc::new(argv))
            },
            cwd: cwd.into(),
            branch: branch.map(String::from),
            agent: None,
            agent_status: "unknown".into(),
        }
    }

    fn label_of(argv: &[&str], cwd: &str, branch: Option<&str>) -> String {
        match decide(&facts(argv, cwd, branch), 24) {
            Decision::Label(l) => l,
            Decision::Llm { fallback } => {
                panic!("expected heuristic label, got LLM (fallback {fallback})")
            }
        }
    }

    #[test]
    fn table() {
        let cases: &[(&[&str], &str, Option<&str>, &str)] = &[
            (
                &[],
                "/Users/me/dev/herdr-autolabel",
                Some("feat/daemon"),
                "feat/daemon",
            ),
            (
                &["-zsh"],
                "/Users/me/dev/herdr-autolabel",
                Some("feat/daemon"),
                "feat/daemon",
            ),
            (&["zsh"], "/Users/me/Downloads", None, "Downloads"),
            (&["zsh"], "/Users/me/dev/pika", Some("main"), "pika"),
            (&["fish"], "/Users/me/dev/pika", Some("master"), "pika"),
            (&["/bin/bash"], "/", None, "shell"),
            (
                &["cargo", "build", "--release"],
                "/r",
                Some("main"),
                "cargo build",
            ),
            (
                &["cargo", "+nightly", "test", "-p", "foo", "--", "x"],
                "/r",
                None,
                "cargo test",
            ),
            (&["nvim", "src/foo.rs"], "/r", None, "nvim foo.rs"),
            (
                &["vim", "-u", "NONE", "/etc/hosts"],
                "/r",
                None,
                "vim hosts",
            ),
            (&["ssh", "andoroid@host"], "/r", None, "ssh host"),
            (
                &["ssh", "-p", "2222", "-i", "~/.ssh/id", "user@mini.local"],
                "/r",
                None,
                "ssh mini.local",
            ),
            (&["vim"], "/r", None, "vim"),
            (&["node"], "/r", None, "node"),
            (&["bun", "run", "dev"], "/r", None, "bun dev"),
            (&["tail", "-f"], "/r", None, "tail -f"),
            (&["less"], "/r", None, "less"),
            (&["man"], "/r", None, "man"),
            (&["fish"], "/r", None, "r"),
            (&["nu"], "/r", None, "r"),
            (&["nvim", "/src/claude/main.rs"], "/r", None, "nvim main.rs"),
            (
                &["uv", "run", "python", "-m", "pytest"],
                "/r",
                None,
                "pytest",
            ),
            (&[], "/...", None, "shell"),
            (&["htop"], "/r", None, "htop"),
            (&["git", "rebase", "-i", "HEAD~3"], "/r", None, "git rebase"),
            (&["git", "-C", "/x", "status"], "/r", None, "git status"),
            (&["npm", "test"], "/r", None, "npm test"),
            (&["npm", "run", "dev"], "/r", None, "npm dev"),
            (&["pnpm", "run", "build"], "/r", None, "pnpm build"),
            (&["pytest", "tests/"], "/r", None, "pytest"),
            (&["python", "-m", "pytest", "-x"], "/r", None, "pytest"),
            (&["just", "check"], "/r", None, "just check"),
            (
                &["docker", "compose", "up", "-d"],
                "/r",
                None,
                "docker compose",
            ),
            (&["docker", "build", "."], "/r", None, "docker build"),
            (
                &["tail", "-f", "/var/log/x.log"],
                "/r",
                None,
                "tail -f x.log",
            ),
            (&["tail", "-n", "5", "x.log"], "/r", None, "tail x.log"),
            (
                &["python3", "scripts/script.py", "--flag"],
                "/r",
                None,
                "python script.py",
            ),
            (&["node", "server.js"], "/r", None, "node server.js"),
            (&["make"], "/r", None, "make"),
            (&["make", "-j8", "install"], "/r", None, "make install"),
            (&["go", "test", "./..."], "/r", None, "go test"),
            (&["less", "README.md"], "/r", None, "less README.md"),
            (&["man", "zshall"], "/r", None, "man zshall"),
            (&["sudo", "htop"], "/r", None, "htop"),
            (
                &["uv", "run", "python", "tool.py"],
                "/r",
                None,
                "python tool.py",
            ),
            (
                &["env", "FOO=1", "cargo", "clippy"],
                "/r",
                None,
                "cargo clippy",
            ),
            (&["docker-compose", "logs"], "/r", None, "docker compose"),
            (&["kubectl", "get", "pods"], "/r", None, "kubectl get"),
            (&["gh", "pr", "view", "1283"], "/r", None, "gh pr"),
            (&["watch", "-n", "2", "ls"], "/r", None, "watch ls"),
            (&["npx", "vitest"], "/r", None, "npx vitest"),
        ];
        for (argv, cwd, branch, expected) in cases {
            assert_eq!(
                label_of(argv, cwd, branch.as_deref()),
                *expected,
                "{argv:?}"
            );
        }
    }

    #[test]
    fn labels_respect_max_chars() {
        let l = label_of(
            &["nvim", "a-very-long-file-name-for-testing.rs"],
            "/r",
            None,
        );
        assert!(l.chars().count() <= 24, "{l}");
    }

    #[test]
    fn agent_panes_go_to_llm_with_fallback() {
        let mut f = facts(&["claude"], "/Users/me/dev/pika", Some("main"));
        assert_eq!(
            decide(&f, 24),
            Decision::Llm {
                fallback: "claude pika".into()
            }
        );
        f.fg = Some(Proc::new(&["zsh"]));
        f.agent = Some("codex".into());
        f.branch = None;
        assert_eq!(
            decide(&f, 24),
            Decision::Llm {
                fallback: "codex pika".into()
            }
        );
        // node running the claude bundle.
        let f = facts(
            &[
                "node",
                "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            ],
            "/x",
            None,
        );
        assert!(matches!(decide(&f, 24), Decision::Llm { .. }));
        let f = facts(&["node", "/usr/local/bin/claude"], "/x/y", None);
        assert_eq!(
            decide(&f, 24),
            Decision::Llm {
                fallback: "claude y".into()
            }
        );
        assert_eq!(
            agent_of(&Proc::new(&["codex", "--full-auto"])).as_deref(),
            Some("codex")
        );
        assert_eq!(agent_of(&Proc::new(&["node", "server.js"])), None);
    }

    #[test]
    fn unknown_process_goes_to_llm_with_generic_fallback() {
        let f = facts(
            &["./my-server", "--port", "8080", "config.toml"],
            "/r",
            None,
        );
        assert_eq!(
            decide(&f, 24),
            Decision::Llm {
                fallback: "my-server config.toml".into()
            }
        );
    }

    #[test]
    fn pick_foreground_skips_shells() {
        let procs = vec![Proc::new(&["-zsh"])];
        assert_eq!(pick_foreground(&procs), None);
        let procs = vec![
            Proc::new(&["zsh"]),
            Proc::new(&["cargo", "build"]),
            Proc::new(&["rustc"]),
        ];
        assert_eq!(pick_foreground(&procs).unwrap().command(), "cargo");
        assert_eq!(pick_foreground(&[]), None);
    }
}
