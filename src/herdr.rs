//! Minimal client for herdr's newline-delimited JSON socket API.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

pub const SOURCE: &str = "plugin:autolabel";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Error {
    /// Could not connect to the socket (server gone).
    Connect(std::io::Error),
    Io(std::io::Error),
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
            Error::Io(e) => write!(f, "io: {e}"),
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
    pub display_agent: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub revision: u64,
    pub focused: bool,
    pub tokens: HashMap<String, String>,
    pub state_labels: HashMap<String, String>,
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
pub struct Snapshot {
    pub panes: Vec<PaneInfo>,
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

    /// One request per connection, like herdr's own CLI.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let id = format!("autolabel-{id}");
        let mut stream = UnixStream::connect(&self.socket).map_err(Error::Connect)?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(Error::Io)?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(Error::Io)?;
        let mut line =
            serde_json::to_string(&json!({"id": id, "method": method, "params": params}))?;
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(Error::Io)?;
        stream.flush().map_err(Error::Io)?;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        let n = reader.read_line(&mut response).map_err(Error::Io)?;
        if n == 0 {
            return Err(Error::Protocol("empty response".into()));
        }
        let value: Value = serde_json::from_str(response.trim_end())?;
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
        if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(Error::Protocol("response id mismatch".into()));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| Error::Protocol("missing result".into()))
    }

    /// Calls `method` and unwraps the typed payload: herdr wraps results as
    /// `{"type": "<variant>", "<key>": {...}}`; older/other shapes are accepted as-is.
    fn call_payload<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        key: &str,
    ) -> Result<T, Error> {
        let mut result = self.call(method, params)?;
        let payload = match result.get_mut(key) {
            Some(inner) => inner.take(),
            None => result,
        };
        Ok(serde_json::from_value(payload)?)
    }

    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        self.call_payload("session.snapshot", json!({}), "snapshot")
    }

    pub fn process_info(&self, pane_id: &str) -> Result<ProcessInfo, Error> {
        self.call_payload(
            "pane.process_info",
            json!({"pane_id": pane_id}),
            "process_info",
        )
    }

    pub fn read_recent(&self, pane_id: &str, lines: u32) -> Result<ReadResult, Error> {
        self.call_payload(
            "pane.read",
            json!({
                "pane_id": pane_id,
                "source": "recent_unwrapped",
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
    fn missing_socket_is_connect_error() {
        let c = Client::new("/nonexistent/herdr.sock");
        assert!(matches!(c.snapshot(), Err(Error::Connect(_))));
    }
}
