//! Linked orrerys for a test: a headless primary and headless satellites on loopback, each with its own temp runtime
//! dir, config and secret file. Waits poll `state` and are bounded (`TIMEOUT`).

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orrery_wm::config;
use serde_json::Value;
use tempfile::TempDir;

use super::{Compositor, TIMEOUT};

pub const SIZE: (i32, i32) = (640, 480);

/// A fresh secret for one test: 24 random bytes as hex.
pub fn random_secret() -> String {
    let mut bytes = [0u8; 24];
    (std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes))).expect("random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A secret file (0600) in a temp dir of its own.
pub fn secret_file(contents: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("secret");
    write_secret(&path, contents);
    (dir, path)
}

pub fn write_secret(path: &Path, contents: &str) {
    std::fs::write(path, format!("{contents}\n")).expect("write the secret");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
}

/// The primary named `name`, listening on loopback `port` (0: any).
pub fn primary(name: &str, port: u16, secret: &Path) -> Compositor {
    primary_with(name, port, secret, &[])
}

/// `primary` with more `run` options.
pub fn primary_with(name: &str, port: u16, secret: &Path, args: &[&str]) -> Compositor {
    let link = format!("listen \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with(&format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT), SIZE, args)
}

/// `primary_with` and environment variables.
pub fn primary_env(name: &str, port: u16, secret: &Path, args: &[&str], env: &[(&str, &str)]) -> Compositor {
    let link = format!("listen \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with_env(
        &format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT),
        SIZE,
        args,
        env,
    )
}

/// A satellite named `name` of the primary on loopback `port`.
pub fn satellite(name: &str, port: u16, secret: &Path) -> Compositor {
    satellite_with(name, port, secret, &[])
}

/// `satellite` with more `run` options.
pub fn satellite_with(name: &str, port: u16, secret: &Path, args: &[&str]) -> Compositor {
    let link = format!("primary \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with(&format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT), SIZE, args)
}

/// `satellite_with` and environment variables.
pub fn satellite_env(name: &str, port: u16, secret: &Path, args: &[&str], env: &[(&str, &str)]) -> Compositor {
    let link = format!("primary \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with_env(
        &format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT),
        SIZE,
        args,
        env,
    )
}

/// The port the primary listens on.
pub fn port(primary: &Compositor) -> u16 {
    let listen = primary.state()["session"]["listen"].as_str().map(str::to_owned);
    let listen = listen.expect("the primary says where it listens");
    listen
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .expect("ADDR:PORT")
}

/// Polls `done` until it holds, up to TIMEOUT; panics with `what` (and the last state seen) after that.
pub fn wait(what: &str, o: &Compositor, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = o.state();
        if done(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {state:#}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Polls `o`'s log until it holds `text`, up to TIMEOUT.
pub fn wait_log(o: &Compositor, text: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !o.log().contains(text) {
        assert!(Instant::now() < deadline, "no {text:?} logged:\n{}", o.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The primary sees two machines.
pub fn joined(state: &Value) -> bool {
    state["session"]["machines"].as_array().is_some_and(|m| m.len() == 2)
}

/// A satellite's link is up.
pub fn connected(state: &Value) -> bool {
    state["session"]["machines"][0]["connected"] == true
}
