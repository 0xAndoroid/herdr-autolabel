//! Minimal client for herdr's newline-delimited JSON socket API.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

pub const SOURCE: &str = "plugin:autolabel";
const TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug)]
pub enum Error {
    /// Could not connect to the socket (server gone).
    Connect(std::io::Error),
    /// A socket syscall failed; `step` names the failing operation (connect/write/read/…).
    Io {
        step: &'static str,
        err: std::io::Error,
    },
    Json(serde_json::Error),
    /// Server-side error response.
    Api {
        code: String,
        message: String,
    },
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "connect: {e}"),
            Error::Io { step, err } => write!(f, "io {step}: {err}"),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Api { code, message } => write!(f, "api {code}: {message}"),
            Error::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub terminal_id: String,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    /// Manual `pane rename` label — user-owned.
    pub label: Option<String>,
    /// Effective metadata title (ours or another source's).
    pub title: Option<String>,
    pub agent: Option<String>,
    pub agent_status: Option<String>,
    pub agent_session: Option<AgentSession>,
    pub display_agent: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub revision: u64,
    pub focused: bool,
    pub tokens: HashMap<String, String>,
    pub state_labels: HashMap<String, String>,
}

/// The agent's own session identity as reported to herdr (`herdr pane report-agent-session`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentSession {
    pub agent: String,
    /// `id` (a session id to look up under the agent's session directory) or `path` (the
    /// transcript file itself).
    pub kind: String,
    pub value: String,
}

impl PaneInfo {
    pub fn effective_cwd(&self) -> &str {
        self.foreground_cwd
            .as_deref()
            .filter(|c| !c.is_empty())
            .or(self.cwd.as_deref())
            .unwrap_or("")
    }

    pub fn has_manual_label(&self) -> bool {
        self.label.as_deref().is_some_and(|l| !l.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceWorktree {
    pub repo_name: String,
    pub checkout_path: String,
}

/// A sidebar space.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    /// Display name: herdr defaults it to the cwd basename; `workspace rename` overwrites it.
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub worktree: Option<WorkspaceWorktree>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub panes: Vec<PaneInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub focused_pane_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProcessInfo {
    pub pane_id: String,
    pub shell_pid: Option<u32>,
    pub tty: Option<String>,
    pub foreground_process_group_id: Option<u32>,
    pub foreground_processes: Vec<ProcessEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReadResult {
    pub text: String,
    pub truncated: bool,
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    next_id: std::cell::Cell<u64>,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            next_id: std::cell::Cell::new(1),
        }
    }

    /// One request per connection, like herdr's own CLI. A transient socket failure
    /// (EINVAL/ECONNRESET/EPIPE/EINTR or a response cut before its newline) is retried once on
    /// a fresh connection; the retry is logged at debug level only.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let id = format!("autolabel-{id}");
        let mut line =
            serde_json::to_string(&json!({"id": id, "method": method, "params": params}))?;
        line.push('\n');
        let response = match self.exchange(&line) {
            Err(e) if is_transient(&e) => {
                crate::logging::log_debug!("{method}: {e}; retrying once");
                std::thread::sleep(RETRY_DELAY);
                self.exchange(&line)?
            }
            other => other?,
        };
        let value: Value = serde_json::from_slice(&response)?;
        if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(Error::Protocol("response id mismatch".into()));
        }
        if let Some(err) = value.get("error") {
            return Err(Error::Api {
                code: err
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| Error::Protocol("missing result".into()))
    }

    /// Connects, writes one request line and reads one response frame.
    fn exchange(&self, line: &str) -> Result<Vec<u8>, Error> {
        let io = |step| move |err| Error::Io { step, err };
        let mut stream = UnixStream::connect(&self.socket).map_err(Error::Connect)?;
        // Socket options are set once, before the server can possibly have closed its end.
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(io("set_read_timeout"))?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(io("set_write_timeout"))?;
        stream.write_all(line.as_bytes()).map_err(io("write"))?;
        read_frame(&mut stream, TIMEOUT)
    }

    /// Calls `method` and unwraps the typed payload: herdr wraps results as
    /// `{"type": "<variant>", "<key>": {...}}`.
    fn call_payload<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        key: &str,
    ) -> Result<T, Error> {
        let mut result = self.call(method, params)?;
        let payload = result
            .get_mut(key)
            .ok_or_else(|| Error::Protocol(format!("missing {key} payload")))?
            .take();
        Ok(serde_json::from_value(payload)?)
    }

    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        self.call_payload("session.snapshot", json!({}), "snapshot")
    }

    pub fn pane(&self, pane_id: &str) -> Result<PaneInfo, Error> {
        self.call_payload("pane.get", json!({"pane_id": pane_id}), "pane")
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<WorkspaceInfo, Error> {
        self.call_payload(
            "workspace.get",
            json!({"workspace_id": workspace_id}),
            "workspace",
        )
    }

    pub fn process_info(&self, pane_id: &str) -> Result<ProcessInfo, Error> {
        self.call_payload(
            "pane.process_info",
            json!({"pane_id": pane_id}),
            "process_info",
        )
    }

    pub fn read_visible(&self, pane_id: &str, lines: u32) -> Result<ReadResult, Error> {
        self.call_payload(
            "pane.read",
            json!({
                "pane_id": pane_id,
                "source": "visible",
                "lines": lines,
                "format": "text",
                "strip_ansi": true,
            }),
            "read",
        )
    }

    pub fn set_title(&self, pane_id: &str, title: &str) -> Result<(), Error> {
        self.call(
            "pane.report_metadata",
            json!({"pane_id": pane_id, "source": SOURCE, "title": title}),
        )?;
        Ok(())
    }

    pub fn clear_title(&self, pane_id: &str) -> Result<(), Error> {
        self.call(
            "pane.report_metadata",
            json!({"pane_id": pane_id, "source": SOURCE, "clear_title": true}),
        )?;
        Ok(())
    }

    /// Sets a workspace (sidebar space) label. This is the same user-visible name that
    /// `herdr workspace rename` sets; herdr has no display-only title for workspaces.
    pub fn rename_workspace(&self, workspace_id: &str, label: &str) -> Result<(), Error> {
        self.call(
            "workspace.rename",
            json!({"workspace_id": workspace_id, "label": label}),
        )?;
        Ok(())
    }
}

/// Errors worth one immediate retry on a fresh connection: the kernel occasionally fails a
/// socket syscall on a connection the server is tearing down (macOS reports EINVAL for it,
/// Linux ECONNRESET/EPIPE), and a response cut before its newline is the same race.
fn is_transient(err: &Error) -> bool {
    match err {
        Error::Io { err, .. } => matches!(
            err.raw_os_error(),
            Some(libc::EINVAL | libc::ECONNRESET | libc::EPIPE | libc::EINTR | libc::ECONNABORTED)
        ),
        Error::Protocol(m) => m.starts_with("response ended"),
        _ => false,
    }
}

fn read_frame(stream: &mut UnixStream, timeout: Duration) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + timeout;
    let mut response = Vec::new();
    let mut buffer = [0; 4096];
    // The caller armed SO_RCVTIMEO right after connecting; it must not be touched again:
    // herdr closes the connection as soon as it has written the response (one request per
    // connection), and once the peer has closed, macOS rejects any setsockopt on the socket
    // with EINVAL even though the unread tail of the response is still buffered. That was the
    // intermittent `pass failed: io: Invalid argument (os error 22)` on multi-chunk frames.
    loop {
        let n = stream
            .read(&mut buffer)
            .map_err(|err| Error::Io { step: "read", err })?;
        if n == 0 {
            return Err(Error::Protocol("response ended before newline".into()));
        }
        if let Some(end) = buffer[..n].iter().position(|b| *b == b'\n') {
            response.extend_from_slice(&buffer[..end]);
            return Ok(response);
        }
        response.extend_from_slice(&buffer[..n]);
        if Instant::now() >= deadline {
            return Err(Error::Io {
                step: "read",
                err: std::io::ErrorKind::TimedOut.into(),
            });
        }
    }
}

/// Default socket: `HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_SOCKET_PATH").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    home_dir().join(".config/herdr/herdr.sock")
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    fn serve_once(sock: PathBuf, reply: impl Fn(&Value) -> String + Send + 'static) {
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let mut out = reply(&req);
            out.push('\n');
            let mut w = stream;
            w.write_all(out.as_bytes()).unwrap();
        });
    }

    fn sock(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hal-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn snapshot_roundtrip_lenient() {
        let s = sock("snap");
        serve_once(s.clone(), |req| {
            assert_eq!(req["method"], "session.snapshot");
            json!({"id": req["id"], "result": {"type": "session_snapshot", "snapshot": {
                "unknown": 1,
                "panes": [{"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "t1",
                    "terminal_id": "term-1", "focused": true, "agent_status": "idle",
                    "revision": 3, "cwd": "/x", "label": null, "extra_field": [1,2]}]}}})
            .to_string()
        });
        let c = Client::new(&s);
        let snap = c.snapshot().unwrap();
        assert_eq!(snap.panes.len(), 1);
        assert_eq!(snap.panes[0].pane_id, "w1:p1");
        assert_eq!(snap.panes[0].effective_cwd(), "/x");
        assert!(!snap.panes[0].has_manual_label());
        let _ = std::fs::remove_file(&s);
    }

    #[test]
    fn api_error_is_surfaced() {
        let s = sock("err");
        serve_once(s.clone(), |req| {
            json!({"id": req["id"], "error": {"code": "pane_not_found", "message": "nope"}})
                .to_string()
        });
        let c = Client::new(&s);
        match c.process_info("w9:p9") {
            Err(Error::Api { code, .. }) => assert_eq!(code, "pane_not_found"),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_file(&s);
    }

    #[test]
    fn frame_requires_newline_and_obeys_deadline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        server.write_all(b"{}").unwrap();
        assert!(matches!(
            read_frame(&mut client, Duration::from_millis(20)),
            Err(Error::Io { .. })
        ));
        drop(server);
        assert!(read_frame(&mut client, TIMEOUT).is_err());
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        let writer = std::thread::spawn(move || {
            server.write_all(b"{\"ok\":").unwrap();
            std::thread::sleep(Duration::from_millis(10));
            server.write_all(b"true}\n").unwrap();
        });
        assert_eq!(read_frame(&mut client, TIMEOUT).unwrap(), b"{\"ok\":true}");
        writer.join().unwrap();
    }

    #[test]
    fn multi_chunk_frame_survives_peer_closing_first() {
        // herdr writes the response and closes immediately; a frame larger than one read
        // buffer must still be assembled (re-arming SO_RCVTIMEO here fails with EINVAL on
        // macOS once the peer is gone).
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        let body = format!("{{\"pad\":\"{}\"}}", "x".repeat(6_000));
        server.write_all(body.as_bytes()).unwrap();
        server.write_all(b"\n").unwrap();
        drop(server);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(read_frame(&mut client, TIMEOUT).unwrap(), body.as_bytes());
    }

    #[test]
    fn typed_requests_and_clear_are_source_scoped() {
        let s = sock("typed");
        serve_once(s.clone(), |req| {
            assert_eq!(req["method"], "pane.read");
            assert_eq!(req["params"]["source"], "visible");
            assert_eq!(req["params"]["lines"], 40);
            json!({"id": req["id"], "result": {"type": "pane_read", "read": {"text": "ready"}}})
                .to_string()
        });
        assert_eq!(
            Client::new(&s).read_visible("p1", 40).unwrap().text,
            "ready"
        );
        std::fs::remove_file(&s).unwrap();
        serve_once(s.clone(), |req| {
            assert_eq!(
                req["params"],
                json!({"pane_id": "p1", "source": SOURCE, "clear_title": true})
            );
            json!({"id": req["id"], "result": {"type": "ok"}}).to_string()
        });
        Client::new(&s).clear_title("p1").unwrap();
        std::fs::remove_file(&s).unwrap();
    }

    #[test]
    fn missing_payload_is_not_an_empty_snapshot() {
        let s = sock("payload");
        serve_once(s.clone(), |req| {
            json!({"id": req["id"], "result": {"type": "ok"}}).to_string()
        });
        assert!(matches!(
            Client::new(&s).snapshot(),
            Err(Error::Protocol(_))
        ));
        std::fs::remove_file(&s).unwrap();
    }

    #[test]
    fn missing_socket_is_connect_error() {
        let c = Client::new("/nonexistent/herdr.sock");
        assert!(matches!(c.snapshot(), Err(Error::Connect(_))));
    }
}

#[cfg(test)]
mod stress {
    //! `cargo nextest run --run-ignored ignored-only stress` — hammers one-shot exchanges against
    //! a local server to surface intermittent socket errors (the EINVAL papercut).
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    #[test]
    #[ignore]
    fn one_shot_exchanges_under_load() {
        let sock = std::env::temp_dir().join(format!("hal-stress-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap_or_default();
                    let mut stream = stream;
                    let body = "x".repeat(20_000);
                    let _ = writeln!(
                        stream,
                        "{}",
                        json!({"id": req["id"], "result": {"type": "ok", "pad": body}})
                    );
                });
            }
        });
        let rounds: usize = std::env::var("STRESS_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        let workers: Vec<_> = (0..8)
            .map(|w| {
                let sock = sock.clone();
                std::thread::spawn(move || {
                    let client = Client::new(&sock);
                    let mut errors = Vec::new();
                    for i in 0..rounds {
                        let line = format!(
                            "{}\n",
                            json!({"id": format!("s{w}-{i}"), "method": "ping", "params": {}})
                        );
                        if let Err(e) = client.exchange(&line) {
                            errors.push(e.to_string());
                        }
                    }
                    errors
                })
            })
            .collect();
        let errors: Vec<String> = workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        let _ = std::fs::remove_file(&sock);
        eprintln!(
            "stress: {} errors over {} exchanges",
            errors.len(),
            rounds * 8
        );
        for e in errors.iter().take(20) {
            eprintln!("  {e}");
        }
        assert!(errors.is_empty(), "{errors:?}");
    }
}
