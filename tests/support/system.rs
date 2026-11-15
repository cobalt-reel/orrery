//! Linked orrerys for a test: a headless sun and headless planets on loopback, each with its own temp runtime
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

/// The sun named `name`, listening on loopback `port` (0: any).
pub fn sun(name: &str, port: u16, secret: &Path) -> Compositor {
    sun_with(name, port, secret, &[])
}

/// `sun` with more `run` options.
pub fn sun_with(name: &str, port: u16, secret: &Path, args: &[&str]) -> Compositor {
    let link = format!("listen \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with(&format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT), SIZE, args)
}

/// `sun_with` and environment variables.
pub fn sun_env(name: &str, port: u16, secret: &Path, args: &[&str], env: &[(&str, &str)]) -> Compositor {
    let link = format!("listen \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with_env(
        &format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT),
        SIZE,
        args,
        env,
    )
}

/// A planet named `name` of the sun on loopback `port`.
pub fn planet(name: &str, port: u16, secret: &Path) -> Compositor {
    planet_with(name, port, secret, &[])
}

/// `planet` with more `run` options.
pub fn planet_with(name: &str, port: u16, secret: &Path, args: &[&str]) -> Compositor {
    let link = format!("sun \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with(&format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT), SIZE, args)
}

/// `planet_with` and environment variables.
pub fn planet_env(name: &str, port: u16, secret: &Path, args: &[&str], env: &[(&str, &str)]) -> Compositor {
    let link = format!("sun \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start_with_env(
        &format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT),
        SIZE,
        args,
        env,
    )
}

/// The port the sun listens on.
pub fn port(sun: &Compositor) -> u16 {
    let listen = sun.state()["orrery"]["listen"].as_str().map(str::to_owned);
    let listen = listen.expect("the sun says where it listens");
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

/// The sun sees two machines.
pub fn joined(state: &Value) -> bool {
    state["orrery"]["machines"].as_array().is_some_and(|m| m.len() == 2)
}

/// A planet's link is up.
pub fn connected(state: &Value) -> bool {
    state["orrery"]["machines"][0]["connected"] == true
}
