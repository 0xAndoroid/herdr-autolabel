use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

struct Session {
    root: PathBuf,
}

impl Session {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hal-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.toml"),
            "provider = \"none\"\ninterval_secs = 2",
        )
        .unwrap();
        Self { root }
    }

    fn command(&self, verb: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-autolabel"));
        command
            .arg(verb)
            .arg("--socket")
            .arg(self.root.join("absent.sock"))
            .env("HERDR_PLUGIN_STATE_DIR", &self.root)
            .env("HERDR_PLUGIN_CONFIG_DIR", &self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&self.command("status").output().unwrap().stdout).unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.command("stop").output();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn result(child: Child) -> Value {
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn concurrent_starts_share_one_detached_daemon_and_stop_cleans_pidfile() {
    let session = Session::new("concurrent");
    let a = session.command("start").spawn().unwrap();
    let b = session.command("start").spawn().unwrap();
    let a = result(a);
    let b = result(b);
    assert_eq!(a["running"], true);
    assert_eq!(a["pid"], b["pid"]);
    let pid = a["pid"].as_i64().unwrap() as i32;
    // SAFETY: getsid only queries the test-created daemon's session.
    assert_eq!(unsafe { libc::getsid(pid) }, pid);
    assert_eq!(
        result(session.command("stop").spawn().unwrap())["stopped"],
        true
    );
    assert_eq!(session.status()["running"], false);
    assert!(
        !fs::read_dir(&session.root).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|x| x == "pid"))
    );
}

#[test]
fn three_connection_failures_exit_and_reused_pid_is_not_signaled() {
    let session = Session::new("exit");
    let start = result(session.command("start").spawn().unwrap());
    assert_eq!(start["running"], true);
    let pidfile = fs::read_dir(&session.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "pid"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while pidfile.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!pidfile.exists());
    assert_eq!(
        fs::read_to_string(session.root.join("daemon.log"))
            .unwrap()
            .matches("connect failed")
            .count(),
        3
    );
    fs::write(&pidfile, std::process::id().to_string()).unwrap();
    assert_eq!(session.status()["running"], false);
    assert_eq!(
        result(session.command("stop").spawn().unwrap())["reason"],
        "not running"
    );
}
