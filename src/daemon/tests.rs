#![expect(clippy::unwrap_used)]

use super::*;
use crate::herdr::{Snapshot, WorkspaceInfo};
use crate::spaces;

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
                let source = if request["params"].get("tokens").is_some() {
                    herdr::IDENTITY_SOURCE
                } else {
                    herdr::SOURCE
                };
                assert_eq!(request["params"]["source"], source);
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
fn old_numeric_titles_do_not_become_workspace_context() {
    let (mut daemon, server) = mock_daemon(
        "old-count",
        vec![("pane.report_metadata", json!({"result": {"type": "ok"}}))],
    );
    let pane = PaneInfo {
        pane_id: "p1".into(),
        title: Some("25".into()),
        ..Default::default()
    };
    daemon
        .handle_pane(&pane, false, &mut PassStats::default())
        .unwrap();
    assert!(daemon.labels.is_empty());
    let seen = finish(&daemon, server);
    assert_eq!(seen[0]["params"]["clear_title"], true);
}

#[test]
fn denied_manual_panes_do_not_enter_workspace_context() {
    let config = Config {
        deny: vec!["p1".into()],
        ..Default::default()
    };
    let (mut daemon, server) = mock_daemon_with("denied-manual", config, vec![]);
    let pane = PaneInfo {
        pane_id: "p1".into(),
        label: Some("private task".into()),
        ..Default::default()
    };
    let result = daemon
        .handle_pane(&pane, false, &mut PassStats::default())
        .unwrap();
    assert_eq!(result.source, Source::SkippedFilter);
    assert!(daemon.labels.is_empty());
    finish(&daemon, server);
}

#[test]
fn manual_and_competing_titles_clear_once_and_relabel_when_title_vanishes() {
    for manual in [false, true] {
        let mut replies = vec![("pane.report_metadata", json!({"result": {"type": "ok"}}))];
        if !manual {
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

#[test]
fn matching_context_reuses_label_even_when_different_contexts_name_it_the_same() {
    let mut replies = labelled_pane("p1");
    replies.extend((0..4).flat_map(|_| unchanged_pane()));
    let (mut daemon, server) = mock_daemon("context-cache", replies);
    let label = "herdr: fix labels";
    for title in ["Task A", "Task B"] {
        let fp = Fingerprint {
            agent: Some("claude".into()),
            title: Some(title.into()),
            ..Default::default()
        }
        .hash();
        daemon.cache.insert(fp, label.into());
    }
    let mut pane = PaneInfo {
        pane_id: "p1".into(),
        agent: Some("claude".into()),
        ..Default::default()
    };
    let mut stats = PassStats::default();
    for (title, source) in [
        ("Task A", Source::Cache),
        ("Task A", Source::Unchanged),
        ("Task B", Source::Cache),
        ("Task B", Source::Unchanged),
        ("Task A", Source::Cache),
    ] {
        pane.terminal_title_stripped = Some(title.into());
        let outcome = daemon.handle_pane(&pane, false, &mut stats).unwrap();
        assert_eq!(outcome.source, source);
        assert_eq!(outcome.label.as_deref(), Some(label));
        pane.title = outcome.label;
    }
    assert_eq!(stats.labeled, 1);
    assert_eq!(stats.llm_calls, 0);
    finish(&daemon, server);
}

#[test]
fn new_session_until_first_prompt_then_one_model_label_per_prompt() {
    let mut replies = labelled_pane("p1");
    replies.extend((0..3).flat_map(|_| unchanged_pane()));
    replies.extend(labelled_pane("p1"));
    replies.extend((0..3).flat_map(|_| unchanged_pane()));
    replies.extend(labelled_pane("p1"));
    let (mut daemon, server) = mock_daemon("new-session", replies);
    let transcript = daemon.paths.state_dir.join("session.jsonl");
    let mut pane = PaneInfo {
        pane_id: "p1".into(),
        agent: Some("claude".into()),
        agent_status: Some("idle".into()),
        agent_session: Some(herdr::AgentSession {
            agent: "claude".into(),
            kind: "path".into(),
            value: transcript.to_string_lossy().into(),
        }),
        ..Default::default()
    };
    let mut stats = PassStats::default();
    let mut step = |daemon: &mut Daemon, pane: &mut PaneInfo, source, label: &str| {
        let outcome = daemon.handle_pane(pane, false, &mut stats).unwrap();
        assert_eq!(
            (outcome.source, outcome.label.as_deref()),
            (source, Some(label))
        );
        pane.title = outcome.label;
    };
    step(&mut daemon, &mut pane, Source::Heuristic, "New session");
    for _ in 0..3 {
        step(&mut daemon, &mut pane, Source::Unchanged, "New session");
    }
    assert_eq!(daemon.labels["p1"].label, "New session");

    let user =
        |text: &str| format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#);
    std::fs::write(&transcript, user("fix the sidebar labels") + "\n").unwrap();
    step(&mut daemon, &mut pane, Source::Fallback, "ready");
    for _ in 0..3 {
        step(&mut daemon, &mut pane, Source::Unchanged, "ready");
    }

    let next = "add a new session label";
    let fp = Fingerprint {
        agent: Some("claude".into()),
        prompt: Some(next.into()),
        ..Default::default()
    }
    .hash();
    daemon.cache.insert(fp, "New session label".into());
    std::fs::write(
        &transcript,
        user("fix the sidebar labels") + "\n" + &user(next) + "\n",
    )
    .unwrap();
    step(&mut daemon, &mut pane, Source::Cache, "New session label");

    let seen = finish(&daemon, server);
    assert!(seen.iter().all(|r| r["method"] != "pane.read"));
}

fn ws(id: &str, label: &str, focused: bool) -> serde_json::Value {
    json!({"workspace_id": id, "label": label, "focused": focused, "pane_count": 1, "tokens": {herdr::FOLDER_TOKEN: if id == "w1" { "pika" } else { "jolt" }}})
}

fn pane_json(
    id: &str,
    ws: &str,
    cwd: &str,
    focused: bool,
    title: Option<&str>,
) -> serde_json::Value {
    json!({"pane_id": id, "workspace_id": ws, "cwd": cwd, "focused": focused, "title": title, "tokens": {herdr::FOLDER_TOKEN: heuristics::project(cwd)}})
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
fn pika_identity_has_no_folder_and_clears_when_replaced() {
    let ok = json!({"result": {"type": "ok"}});
    let mut pika = pane_json("w1:p1", "w1", "/x/.dotfiles", true, Some("pika"));
    pika["agent"] = json!("pika");
    let mut codex = pika.clone();
    codex["agent"] = json!("codex");
    codex["display_agent"] = json!("pika TUI");
    codex["tokens"] = json!({});
    let replies = vec![
        ("session.snapshot", snapshot(vec![pika], vec![])),
        ("pane.report_metadata", ok.clone()),
        ("pane.process_info", json!({"result": {"process_info": {}}})),
        ("session.snapshot", snapshot(vec![codex.clone()], vec![])),
        ("pane.report_metadata", ok.clone()),
        ("pane.process_info", json!({"result": {"process_info": {}}})),
        ("pane.get", json!({"result": {"pane": codex}})),
        ("pane.report_metadata", ok),
    ];
    let config = Config {
        label_spaces: false,
        ..Default::default()
    };
    let (mut daemon, server) = mock_daemon_with("pika-identity", config, replies);
    daemon.applied.insert("w1:p1".into(), "pika".into());
    assert_eq!(
        daemon.pass(false).unwrap().1[0].label.as_deref(),
        Some("pika")
    );
    assert_eq!(
        daemon.pass(false).unwrap().1[0].label.as_deref(),
        Some("New session")
    );
    let seen = finish(&daemon, server);
    assert_eq!(seen[1]["params"]["display_agent"], "pika TUI");
    assert_ne!(seen[1]["params"]["source"], seen[7]["params"]["source"]);
    assert_eq!(seen[1]["params"]["tokens"][herdr::FOLDER_TOKEN], "");
    assert_eq!(seen[4]["params"]["clear_display_agent"], true);
    assert_eq!(seen[4]["params"]["tokens"][herdr::FOLDER_TOKEN], "dotfiles");
}

#[test]
fn pika_space_overrides_other_panes_and_clears_location() {
    let ok = json!({"result": {"type": "ok"}});
    let mut workspace: WorkspaceInfo =
        serde_json::from_value(ws("w1", "3D printing", true)).unwrap();
    workspace
        .tokens
        .insert(herdr::BRANCH_TOKEN.into(), "main".into());
    let snapshot = Snapshot {
        panes: vec![
            PaneInfo {
                pane_id: "w1:p1".into(),
                workspace_id: "w1".into(),
                agent: Some("pika".into()),
                ..Default::default()
            },
            PaneInfo {
                pane_id: "w1:p2".into(),
                workspace_id: "w1".into(),
                agent: Some("codex".into()),
                ..Default::default()
            },
        ],
        workspaces: vec![workspace],
        ..Default::default()
    };
    let (mut daemon, server) = mock_daemon(
        "pika-space",
        vec![
            ("workspace.report_metadata", ok.clone()),
            (
                "workspace.get",
                json!({"result": {"workspace": {"label": "3D printing"}}}),
            ),
            ("workspace.rename", ok),
        ],
    );
    let mut stats = PassStats::default();
    let result = daemon.handle_spaces(&snapshot, false, &mut stats).unwrap();
    assert_eq!(result[0].label.as_deref(), Some("pika"));
    assert_eq!(result[0].source, Source::Heuristic);
    assert_eq!(stats.llm_calls, 0);
    let seen = finish(&daemon, server);
    assert_eq!(
        seen[0]["params"]["tokens"],
        json!({herdr::FOLDER_TOKEN: "\u{200b}", herdr::BRANCH_TOKEN: ""})
    );
}

#[test]
fn ssh_space_keeps_destination_instead_of_cached_generic_name() {
    let snapshot: Snapshot = serde_json::from_value(
        snapshot(
            vec![
                pane_json("w1:p1", "w1", "/x/pika", true, None),
                pane_json("w1:p2", "w1", "/x/pika", false, None),
            ],
            vec![ws("w1", "Remote SSH session", true)],
        )["result"]["snapshot"]
            .clone(),
    )
    .unwrap();
    let (mut daemon, server) = mock_daemon(
        "ssh-space",
        vec![
            (
                "workspace.get",
                json!({"result": {"workspace": {"label": "Remote SSH session"}}}),
            ),
            ("workspace.rename", json!({"result": {"type": "ok"}})),
        ],
    );
    for (id, label) in [("w1:p1", "SSH Mac Mini"), ("w1:p2", "shell")] {
        daemon.labels.insert(
            id.into(),
            PaneSummary {
                label: label.into(),
                project: Some("pika".into()),
                ..Default::default()
            },
        );
    }
    let fp = spaces::fingerprint(&[
        ("w1:p1", &daemon.labels["w1:p1"]),
        ("w1:p2", &daemon.labels["w1:p2"]),
    ]);
    daemon.cache.insert(fp, "Remote SSH session".into());
    let mut stats = PassStats::default();
    let result = daemon.handle_spaces(&snapshot, false, &mut stats).unwrap();
    assert_eq!(result[0].label.as_deref(), Some("SSH Mac Mini"));
    assert_eq!(result[0].source, Source::Heuristic);
    assert_eq!(stats.llm_calls, 0);
    let seen = finish(&daemon, server);
    assert_eq!(seen.last().unwrap()["params"]["label"], "SSH Mac Mini");
}

#[test]
fn space_metadata_hysteresis_and_restore() {
    let ok = json!({"result": {"type": "ok"}});
    let current = |label| json!({"result": {"workspace": {"workspace_id": "w1", "label": label}}});
    let replies = vec![
        ("workspace.report_metadata", ok.clone()),
        ("workspace.get", current("pika")),
        ("workspace.rename", ok.clone()),
        ("workspace.get", current("cargo build")),
        ("workspace.rename", ok.clone()),
        (
            "session.snapshot",
            snapshot(vec![], vec![ws("w1", "cargo check", false)]),
        ),
        ("workspace.rename", ok),
    ];
    let (mut daemon, server) = mock_daemon("space-metadata", replies);
    daemon.labels.insert(
        "w1:p1".into(),
        PaneSummary {
            label: "cargo build".into(),
            project: Some("pika".into()),
            ..Default::default()
        },
    );
    let mut snapshot = Snapshot {
        panes: vec![PaneInfo {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            cwd: Some("/x/pika".into()),
            ..Default::default()
        }],
        workspaces: vec![WorkspaceInfo {
            workspace_id: "w1".into(),
            label: "pika".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut stats = PassStats::default();
    snapshot.panes.push(PaneInfo {
        pane_id: "w1:p2".into(),
        workspace_id: "w1".into(),
        ..Default::default()
    });
    assert_eq!(
        daemon.handle_spaces(&snapshot, false, &mut stats).unwrap()[0].source,
        Source::Pending
    );
    snapshot.panes.pop();
    assert!(daemon.handle_spaces(&snapshot, false, &mut stats).unwrap()[0].applied);
    snapshot.workspaces[0]
        .tokens
        .insert(herdr::FOLDER_TOKEN.into(), "pika".into());
    snapshot.workspaces[0].label = "cargo build".into();
    daemon.labels.get_mut("w1:p1").unwrap().label = "cargo check".into();
    assert!(!daemon.handle_spaces(&snapshot, false, &mut stats).unwrap()[0].applied);
    assert!(daemon.handle_spaces(&snapshot, false, &mut stats).unwrap()[0].applied);
    daemon.restore_spaces();
    let state = spaces::load_states(&daemon.paths.spaces_file());
    assert_eq!(state["w1"].applied, None);
    let seen = finish(&daemon, server);
    assert_eq!(
        seen[0]["params"]["tokens"],
        json!({herdr::FOLDER_TOKEN: "pika", herdr::BRANCH_TOKEN: ""})
    );
    let renames: Vec<_> = seen
        .iter()
        .filter(|r| r["method"] == "workspace.rename")
        .map(|r| r["params"]["label"].as_str().unwrap())
        .collect();
    assert_eq!(renames, ["cargo build", "cargo check", "pika"]);
}

#[test]
fn hand_named_spaces_are_never_renamed() {
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
    assert_eq!(panes[0].label.as_deref(), Some("shell"));
    assert!(!panes[0].applied);
    assert!(daemon.applied.is_empty());
    assert_eq!(spaces[0].label.as_deref(), Some("shell"));
    assert!(spaces[0].applied);
    let seen = finish(&daemon, server);
    assert!(seen.iter().all(|r| r["method"] != "pane.report_metadata"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn failed_space_rename_keeps_earlier_renames_in_state() {
    let root = std::env::temp_dir().join(format!("hal-rename-fail-{}", std::process::id()));
    for d in ["pika/.git", "pika/crates", "jolt/.git", "jolt/sub"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let p = |rel: &str| root.join(rel).to_string_lossy().into_owned();
    let mut replies = vec![(
        "session.snapshot",
        snapshot(
            vec![
                pane_json("w1:p1", "w1", &p("pika/crates"), true, None),
                pane_json("w2:p1", "w2", &p("jolt/sub"), false, None),
            ],
            vec![ws("w1", "pika", true), ws("w2", "jolt", false)],
        ),
    )];
    replies.extend(unchanged_pane());
    replies.extend(unchanged_pane());
    replies.extend([
        (
            "workspace.get",
            json!({"result": {"workspace": {"workspace_id": "w1", "label": "pika"}}}),
        ),
        ("workspace.rename", json!({"result": {"type": "ok"}})),
        (
            "workspace.get",
            json!({"result": {"workspace": {"workspace_id": "w2", "label": "jolt"}}}),
        ),
        (
            "workspace.rename",
            json!({"error": {"code": "busy", "message": "retry"}}),
        ),
    ]);
    let config = Config::parse("label_panes = false\n").unwrap();
    let (mut daemon, server) = mock_daemon_with("rename-fail", config, replies);
    assert!(daemon.pass(false).is_err());
    let states = spaces::load_states(&daemon.paths.spaces_file());
    assert_eq!(states["w1"].original, "pika");
    assert_eq!(states["w1"].applied.as_deref(), Some("shell"));
    finish(&daemon, server);
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
                        "focused": true, "label": "billing rewrite", "tokens": {herdr::FOLDER_TOKEN: "pika"}}),
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
    assert_eq!(spaces[0].label.as_deref(), Some("billing rewrite"));
    let seen = finish(&daemon, server);
    assert_eq!(seen.last().unwrap()["params"]["label"], "billing rewrite");
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
