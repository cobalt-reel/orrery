//! Runs a headless orrery for a test: its own temp `XDG_RUNTIME_DIR` and config, never the user's session.

#![allow(dead_code)] // each test binary uses a different part of this

pub mod client;
pub mod session;

use std::io::{BufRead, BufReader, Read as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use orrery_wm::NAME;
use orrery_wm::comp::ipc_server::socket_path_in;
use serde_json::Value;
use tempfile::TempDir;

/// The binary under test: the one place a test names it.
pub const BIN: &str = env!("CARGO_BIN_EXE_orrery");

pub const TIMEOUT: Duration = Duration::from_secs(10);
/// A orrery still running after this is killed, so a deadlocked compositor fails the test (its client's
/// roundtrip errors) instead of hanging it.
pub const WATCHDOG: Duration = Duration::from_secs(60);

pub struct Compositor {
    /// Shared with the watchdog; whoever takes it out owns stopping it. None once stopped.
    child: Arc<Mutex<Option<Child>>>,
    /// Holds the Wayland socket and the control socket.
    pub runtime: TempDir,
    pub config_dir: TempDir,
    pub display: String,
    pub size: (i32, i32),
}

fn log_of(config_dir: &TempDir) -> String {
    std::fs::read_to_string(config_dir.path().join(format!("{NAME}.log"))).unwrap_or_default()
}

/// Kills the child after WATCHDOG unless the test has taken it out first (stop or drop). It holds the `Child`
/// itself, not a pid, so it can never kill a reused pid.
fn watchdog(child: Arc<Mutex<Option<Child>>>) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + WATCHDOG;
        while Instant::now() < deadline {
            if child.lock().map(|c| c.is_none()).unwrap_or(true) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Ok(mut guard) = child.lock()
            && let Some(mut c) = guard.take()
        {
            eprintln!("watchdog: orrery still running after {WATCHDOG:?}, killing it");
            let _ = c.kill();
            let _ = c.wait();
        }
    });
}

impl Compositor {
    /// `orrery run --backend headless` with `config` as its config file, waiting for its WAYLAND_DISPLAY line.
    pub fn start(config: &str, size: (i32, i32)) -> Self {
        Self::start_in(tempfile::tempdir().expect("temp runtime dir"), config, size)
    }

    /// `start` with more `run` options (`--debug-inject`, say).
    pub fn start_with(config: &str, size: (i32, i32), args: &[&str]) -> Self {
        Self::launch(tempfile::tempdir().expect("temp runtime dir"), config, size, args, &[])
    }

    /// `start_with` and environment variables (an xkb layout, say).
    pub fn start_with_env(config: &str, size: (i32, i32), args: &[&str], env: &[(&str, &str)]) -> Self {
        Self::launch(tempfile::tempdir().expect("temp runtime dir"), config, size, args, env)
    }

    /// The same, in a runtime dir the test has prepared (a stale control socket, say).
    pub fn start_in(runtime: TempDir, config: &str, size: (i32, i32)) -> Self {
        Self::launch(runtime, config, size, &[], &[])
    }

    fn launch(runtime: TempDir, config: &str, size: (i32, i32), args: &[&str], env: &[(&str, &str)]) -> Self {
        let config_dir = tempfile::tempdir().expect("temp config dir");
        let config_path = config_dir.path().join("config.kdl");
        std::fs::write(&config_path, config).expect("write the temp config");

        // orrery's log, printed only when a test fails (see Drop).
        let log = std::fs::File::create(config_dir.path().join(format!("{NAME}.log"))).expect("log file");
        let mut child = Command::new(BIN)
            .args(["run", "--backend", "headless", "--size"])
            .arg(format!("{}x{}", size.0, size.1))
            .arg("--config")
            .arg(&config_path)
            .arg("--socket")
            .arg(socket_path_in(runtime.path()))
            .args(args)
            // Never the user's session: our own runtime dir, and no display to inherit.
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("XDG_CONFIG_HOME", config_dir.path())
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            // The default us layout (keycodes in tests are its), whatever the user's environment says.
            .env_remove("XKB_DEFAULT_RULES")
            .env_remove("XKB_DEFAULT_MODEL")
            .env_remove("XKB_DEFAULT_LAYOUT")
            .env_remove("XKB_DEFAULT_VARIANT")
            .env_remove("XKB_DEFAULT_OPTIONS")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("spawn orrery");
        let stdout = child.stdout.take().expect("piped stdout");
        let child = Arc::new(Mutex::new(Some(child)));
        watchdog(child.clone());

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Keep draining so orrery never blocks on a full pipe.
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(name) = line.strip_prefix("WAYLAND_DISPLAY=") {
                    let _ = tx.send(name.to_owned());
                }
            }
        });
        let display = match rx.recv_timeout(TIMEOUT) {
            Ok(d) => d,
            Err(e) => {
                let status = child.lock().ok().and_then(|mut c| c.take()).map(|mut c| {
                    let _ = c.kill();
                    c.wait()
                });
                panic!(
                    "{NAME} printed no WAYLAND_DISPLAY line ({e}); exit {status:?}\n--- {NAME} log ---\n{}--- end ---",
                    log_of(&config_dir)
                );
            }
        };
        Self {
            child,
            runtime,
            config_dir,
            display,
            size,
        }
    }

    /// orrery's log (stderr) so far.
    pub fn log(&self) -> String {
        log_of(&self.config_dir)
    }

    pub fn pid(&self) -> u32 {
        self.child
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(Child::id))
            .expect("running")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.runtime.path().join(&self.display)
    }

    /// The config file orrery was started with (and watches).
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.path().join("config.kdl")
    }

    /// Replaces the config file in one step (write aside, rename), so orrery never reads half of it.
    pub fn write_config(&self, src: &str) {
        let tmp = self.config_dir.path().join("config.kdl.new");
        std::fs::write(&tmp, src).expect("write the new config");
        std::fs::rename(&tmp, self.config_path()).expect("move the new config in place");
    }

    pub fn control_socket(&self) -> PathBuf {
        socket_path_in(self.runtime.path())
    }

    /// A raw connection to orrery's Wayland socket, by path (no environment involved).
    pub fn connect(&self) -> wayland_client::Connection {
        let stream = UnixStream::connect(self.socket_path()).expect("connect to orrery");
        wayland_client::Connection::from_socket(stream).expect("wayland connection")
    }

    /// `orrery msg --socket <ours> <words>`.
    pub fn msg(&self, words: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .arg("msg")
            .arg("--socket")
            .arg(self.control_socket())
            .args(words)
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .expect("run orrery msg")
    }

    /// A second `orrery run --backend headless` on this orrery's control socket, with a runtime dir of its own
    /// (returned, to see what it left there): its exit status, stdout and stderr.
    pub fn run_again(&self) -> (ExitStatus, String, String, TempDir) {
        let runtime = tempfile::tempdir().expect("temp runtime dir");
        let mut child = Command::new(BIN)
            .args(["run", "--backend", "headless", "--socket"])
            .arg(self.control_socket())
            .arg("--config")
            .arg(self.config_path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("XDG_CONFIG_HOME", runtime.path())
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn a second orrery");
        let status = wait_or_kill(&mut child, "the second orrery kept running");
        // It has exited, and says little: nothing is lost to a full pipe.
        let (mut stdout, mut stderr) = (String::new(), String::new());
        (child.stdout.take().expect("stdout").read_to_string(&mut stdout)).expect("read its stdout");
        (child.stderr.take().expect("stderr").read_to_string(&mut stderr)).expect("read its stderr");
        (status, stdout, stderr, runtime)
    }

    /// `msg`, which must succeed, its stdout as JSON.
    pub fn msg_ok(&self, words: &[&str]) -> serde_json::Value {
        let out = self.msg(words);
        assert!(
            out.status.success(),
            "{NAME} msg {words:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("msg prints JSON")
    }

    /// `msg`, which must fail, its stdout (the response) as JSON.
    pub fn msg_err(&self, words: &[&str]) -> serde_json::Value {
        let out = self.msg(words);
        assert!(!out.status.success(), "{NAME} msg {words:?} should fail");
        serde_json::from_slice(&out.stdout).expect("msg prints the error response as JSON")
    }

    /// `msg state`'s state.
    pub fn state(&self) -> serde_json::Value {
        self.msg_ok(&["state"])["state"].clone()
    }

    /// Raw bytes to the control socket, the answer as a string (for what `orrery msg` would never send).
    pub fn raw(&self, bytes: &[u8]) -> String {
        use std::io::{Read, Write};
        let mut s = UnixStream::connect(self.control_socket()).expect("connect to the control socket");
        s.set_read_timeout(Some(TIMEOUT)).expect("timeout");
        // A huge request may be answered (and the socket closed) before all of it is written.
        let _ = s.write_all(bytes);
        let _ = s.shutdown(std::net::Shutdown::Write);
        // Read to EOF; a reset after the answer (orrery closed with our input unread) still keeps it.
        let mut answer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => answer.extend_from_slice(&chunk[..n]),
                Err(_) if answer.contains(&b'\n') => break,
                Err(e) => panic!("read the answer: {e}"),
            }
        }
        String::from_utf8(answer).expect("a UTF-8 answer")
    }

    /// Waits for orrery to exit by itself (after `quit`), and keeps the runtime dir for inspection.
    pub fn wait_exit(mut self) -> (ExitStatus, TempDir) {
        let mut child = self.child.lock().expect("child lock").take().expect("running");
        let runtime = std::mem::replace(&mut self.runtime, tempfile::tempdir().expect("temp dir"));
        (wait_or_kill(&mut child, "the compositor did not exit on quit"), runtime)
    }

    /// SIGTERM, then wait: orrery must exit by itself, successfully.
    pub fn stop(self) -> ExitStatus {
        self.stop_keeping_runtime().0
    }

    /// `stop`, keeping the runtime dir so the test can look at what orrery left behind.
    pub fn stop_keeping_runtime(mut self) -> (ExitStatus, TempDir) {
        // Taking the child out stands the watchdog down; from here this function owns stopping it.
        let mut child = self.child.lock().expect("child lock").take().expect("running");
        let runtime = std::mem::replace(&mut self.runtime, tempfile::tempdir().expect("temp dir"));
        let pid = child.id().to_string();
        let sent = Command::new("kill").args(["-TERM", &pid]).status().expect("run kill");
        assert!(sent.success(), "kill -TERM {pid} failed");
        (
            wait_or_kill(&mut child, "the compositor did not exit on SIGTERM"),
            runtime,
        )
    }
}

/// `orrery msg subscribe` running against an orrery: its lines as they come, read on a thread of their own.
pub struct Subscription {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Subscription {
    /// The next line, as JSON, for which `want` holds (lines before it are skipped); panics after TIMEOUT.
    pub fn wait_for(&self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
                    if want(&v) {
                        return v;
                    }
                }
                Err(e) => panic!("no {what} event ({e})"),
            }
        }
    }

    /// The next event named `name`.
    pub fn next(&self, name: &str) -> Value {
        self.wait_for(name, |v| v["event"] == name)
    }

    /// Every line left until EOF, which must come within TIMEOUT; then the client's exit status.
    pub fn rest(mut self) -> (Vec<String>, ExitStatus) {
        let deadline = Instant::now() + TIMEOUT;
        let mut rest = Vec::new();
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => rest.push(line),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(e) => panic!("the stream did not end ({e})"),
            }
        }
        (
            rest,
            wait_or_kill(&mut self.child, "msg subscribe kept running after EOF"),
        )
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Compositor {
    /// `orrery msg --socket <ours> subscribe`, its stdout piped but not read: it blocks once the pipe is full.
    pub fn subscribe_unread(&self) -> Child {
        Command::new(BIN)
            .arg("msg")
            .arg("--socket")
            .arg(self.control_socket())
            .arg("subscribe")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run orrery msg subscribe")
    }

    /// `orrery msg --socket <ours> subscribe`, running.
    pub fn subscribe(&self) -> Subscription {
        let mut child = Command::new(BIN)
            .arg("msg")
            .arg("--socket")
            .arg(self.control_socket())
            .arg("subscribe")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run orrery msg subscribe");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Subscription { child, lines }
    }
}

/// Each slot's tabs, left to right, each its windows' app ids bottom to top, on the first monitor.
pub fn tabs(state: &Value) -> Vec<Vec<Vec<String>>> {
    state["monitors"][0]["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .map(|slot| {
            slot["tabs"]
                .as_array()
                .expect("tabs")
                .iter()
                .map(|t| {
                    t["windows"]
                        .as_array()
                        .expect("windows")
                        .iter()
                        .map(|w| w["app_id"].as_str().unwrap_or("?").to_owned())
                        .collect()
                })
                .collect()
        })
        .collect()
}

/// Each slot's windows, tab by tab, bottom to top.
pub fn windows(state: &Value) -> Vec<Vec<String>> {
    tabs(state).into_iter().map(|slot| slot.concat()).collect()
}

/// The index of each slot's current tab (None for an empty slot).
pub fn current_tabs(state: &Value) -> Vec<Option<usize>> {
    state["monitors"][0]["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .map(|slot| slot["tabs"].as_array()?.iter().position(|t| t["current"] == true))
        .collect()
}

/// Each slot's tabs' `attention`, left to right, on the first monitor.
pub fn attention(state: &Value) -> Vec<Vec<bool>> {
    state["monitors"][0]["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .map(|slot| {
            (slot["tabs"].as_array().expect("tabs").iter())
                .map(|t| t["attention"].as_bool().expect("attention"))
                .collect()
        })
        .collect()
}

pub fn focused_slot(state: &Value) -> Option<u64> {
    state["monitors"][0]["slots"]
        .as_array()?
        .iter()
        .find(|s| s["focused"] == true)
        .and_then(|s| s["index"].as_u64())
}

pub fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// Waits up to TIMEOUT for the child, killing it and panicking with `why` after that.
pub fn wait_or_kill(child: &mut Child, why: &str) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("wait for orrery") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{why}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // A failed test must not leave a compositor running.
        if let Some(mut child) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::thread::panicking() {
            eprintln!("--- orrery log ---\n{}--- end ---", log_of(&self.config_dir));
        }
    }
}
