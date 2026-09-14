//! herdr-autolabel — labels every herdr pane with a terse title describing what is happening in it.

mod config;
mod daemon;
mod fingerprint;
mod git;
mod herdr;
mod heuristics;
mod label;
mod llm;
mod logging;
mod ratelimit;
mod scrub;
mod spaces;
mod transcript;

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use daemon::{Daemon, Paths};

const USAGE: &str =
    "usage: herdr-autolabel <start|daemon|stop|once [--force]|status> [--socket PATH]

  start    spawn the detached labelling daemon (idempotent) and print status
  daemon   run the labelling loop in the foreground
  stop     SIGTERM the running daemon for this socket
  once     run a single labelling pass and print per-pane results
  status   print daemon status JSON
";

struct Args {
    command: String,
    socket: Option<PathBuf>,
    force: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut argv = std::env::args().skip(1);
    let Some(command) = argv.next() else {
        return Err(USAGE.into());
    };
    let mut socket = None;
    let mut force = false;
    while let Some(a) = argv.next() {
        match a.as_str() {
            "--socket" => socket = Some(PathBuf::from(argv.next().ok_or("--socket needs a path")?)),
            "--force" | "-f" => force = true,
            "-h" | "--help" => return Err(USAGE.into()),
            s if s.starts_with("--socket=") => {
                socket = Some(PathBuf::from(&s["--socket=".len()..]))
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(Args {
        command,
        socket,
        force,
    })
}

fn main() {
    logging::init_from_env();
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if args.command != "daemon" {
        logging::use_stderr();
    }
    let paths = Paths::resolve(args.socket.clone());
    let code = match args.command.as_str() {
        "start" => cmd_start(&paths),
        "daemon" => cmd_daemon(&paths),
        "stop" => cmd_stop(&paths),
        "once" => cmd_once(&paths, args.force),
        "status" => cmd_status(&paths),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("unknown command {other:?}\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn load_config(paths: &Paths) -> config::Config {
    match config::Config::load(&paths.config_file()) {
        Ok(c) => c,
        Err(e) => {
            logging::log_warn!("config error ({e}); using defaults");
            config::Config::default()
        }
    }
}

fn select_provider(config: &config::Config) -> Option<llm::Provider> {
    match llm::select(config) {
        Ok(p) => p,
        Err(e) => {
            logging::log_warn!("provider: {e}; heuristics only");
            None
        }
    }
}

// ---- pidfile helpers -------------------------------------------------------------------------

fn read_pid(paths: &Paths) -> Option<i32> {
    std::fs::read_to_string(paths.pidfile())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only probes for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn lock_daemon(paths: &Paths) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.pidfile().with_extension("lock"))?;
    // SAFETY: the descriptor belongs to the returned file; closing it releases the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

fn pid_is_daemon(pid: i32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(args) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        let mut args = args.split(|b| *b == 0);
        let exe = String::from_utf8_lossy(args.next().unwrap_or_default());
        PathBuf::from(exe.as_ref())
            .file_name()
            .is_some_and(|n| n == "herdr-autolabel")
            && args.next() == Some(b"daemon".as_slice())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(output) = Command::new("/bin/ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "args="])
            .output()
        else {
            return false;
        };
        let args = String::from_utf8_lossy(&output.stdout);
        let Some((exe, rest)) = args.trim().split_once(" daemon") else {
            return false;
        };
        output.status.success()
            && PathBuf::from(exe)
                .file_name()
                .is_some_and(|n| n == "herdr-autolabel")
            && (rest.is_empty() || rest.starts_with(' '))
    }
}

fn running_pid(paths: &Paths) -> Option<i32> {
    let pid = read_pid(paths).filter(|pid| pid_is_daemon(*pid))?;
    // A stale PID alone never establishes ownership of this socket's daemon.
    match lock_daemon(paths) {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Some(pid),
        Err(_) => None,
    }
}

fn remove_pidfile(paths: &Paths) {
    let _ = std::fs::remove_file(paths.pidfile());
}

fn status_json(paths: &Paths) -> serde_json::Value {
    let pid = running_pid(paths);
    let last = std::fs::read_to_string(paths.status_file())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let config = load_config(paths);
    let provider = select_provider(&config);
    json!({
        "running": pid.is_some(),
        "pid": pid,
        "socket": paths.socket,
        "state_dir": paths.state_dir,
        "config_file": paths.config_file(),
        "log_file": paths.log_file(),
        "provider": provider.as_ref().map(|p| p.kind.name()).unwrap_or("none"),
        "model": provider.as_ref().map(|p| p.model.clone()),
        "last_pass": last,
    })
}

// ---- subcommands -----------------------------------------------------------------------------

fn cmd_start(paths: &Paths) -> i32 {
    if let Some(pid) = running_pid(paths) {
        logging::log_info!("daemon already running (pid {pid})");
        println!("{}", status_json(paths));
        return 0;
    }
    if let Err(e) = std::fs::create_dir_all(&paths.state_dir) {
        eprintln!("cannot create state dir {}: {e}", paths.state_dir.display());
        return 1;
    }
    let log_path = paths.log_file();
    // Keep the log bounded.
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&log_path, paths.state_dir.join("daemon.log.1"));
    }
    let log = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open {}: {e}", log_path.display());
            return 1;
        }
    };
    let log_err = match log.try_clone() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot dup log handle: {e}");
            return 1;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot locate own binary: {e}");
            return 1;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("--socket")
        .arg(&paths.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .current_dir(&paths.state_dir);
    // SAFETY: setsid is async-signal-safe and only detaches the child from our session/tty.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("spawn failed: {e}");
            return 1;
        }
    };
    drop(child); // Not waited on: it is reparented once we exit.
    // Give the daemon a moment to write its pidfile so the printed status is accurate.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline && running_pid(paths).is_none() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let Some(pid) = running_pid(paths) else {
        eprintln!("daemon did not start; see {}", log_path.display());
        return 1;
    };
    logging::log_info!("started daemon pid {pid}, log {}", log_path.display());
    println!("{}", status_json(paths));
    0
}

extern "C" fn on_term(_sig: libc::c_int) {
    daemon::SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn cmd_daemon(paths: &Paths) -> i32 {
    if let Err(e) = std::fs::create_dir_all(&paths.state_dir) {
        eprintln!("cannot create state dir {}: {e}", paths.state_dir.display());
        return 1;
    }
    let _lock = match lock_daemon(paths) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("cannot acquire daemon lock: {e}");
            return 1;
        }
    };
    // SAFETY: installing a minimal handler that only flips an atomic flag.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_term as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_term as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    if let Err(e) =
        daemon::write_atomic(&paths.pidfile(), std::process::id().to_string().as_bytes())
    {
        eprintln!("cannot write pidfile: {e}");
        return 1;
    }
    let config = load_config(paths);
    let provider = select_provider(&config);
    let mut d = Daemon::new(paths.clone(), config, provider);
    d.run();
    remove_pidfile(paths);
    0
}

fn cmd_stop(paths: &Paths) -> i32 {
    match running_pid(paths) {
        Some(pid) => {
            // SAFETY: plain SIGTERM to a pid we recorded ourselves.
            let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
            if rc != 0 {
                eprintln!("kill {pid} failed: {}", std::io::Error::last_os_error());
                return 1;
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && running_pid(paths) == Some(pid) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let stopped = running_pid(paths) != Some(pid);
            println!("{}", json!({"stopped": stopped, "pid": pid}));
            i32::from(!stopped)
        }
        None => {
            println!("{}", json!({"stopped": false, "reason": "not running"}));
            0
        }
    }
}

fn cmd_once(paths: &Paths, force: bool) -> i32 {
    let config = load_config(paths);
    let provider = select_provider(&config);
    let mut d = Daemon::new(paths.clone(), config, provider);
    match d.pass(force) {
        Ok((stats, outcomes, spaces)) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "socket": paths.socket,
                    "provider": d.provider.as_ref().map(|p| p.to_string()),
                    "stats": stats,
                    "panes": outcomes,
                    "spaces": spaces,
                }))
                .unwrap_or_default()
            );
            0
        }
        Err(e) => {
            eprintln!(
                "{}",
                json!({"error": e.to_string(), "socket": paths.socket})
            );
            1
        }
    }
}

fn cmd_status(paths: &Paths) -> i32 {
    println!(
        "{}",
        serde_json::to_string_pretty(&status_json(paths)).unwrap_or_default()
    );
    0
}
