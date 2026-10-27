//! Linked orrerys: a headless primary and headless satellites on loopback, each with its own temp runtime dir,
//! config and secret file. Waits poll `state` and are bounded (`support::TIMEOUT`).

mod support;

use std::io::{ErrorKind, Read as _};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use orrery_wm::config;
use serde_json::Value;
use support::{Compositor, TIMEOUT};
use tempfile::TempDir;

const SIZE: (i32, i32) = (640, 480);

/// A fresh secret for one test: 24 random bytes as hex.
fn random_secret() -> String {
    let mut bytes = [0u8; 24];
    (std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes))).expect("random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A secret file (0600) in a temp dir of its own.
fn secret_file(contents: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("secret");
    write_secret(&path, contents);
    (dir, path)
}

fn write_secret(path: &Path, contents: &str) {
    std::fs::write(path, format!("{contents}\n")).expect("write the secret");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
}

/// The primary named `name`, listening on loopback `port` (0: any).
fn primary(name: &str, port: u16, secret: &Path) -> Compositor {
    let link = format!("listen \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start(&format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT), SIZE)
}

/// A satellite named `name` of the primary on loopback `port`.
fn satellite(name: &str, port: u16, secret: &Path) -> Compositor {
    let link = format!("primary \"127.0.0.1:{port}\" secret-file=\"{}\"", secret.display());
    Compositor::start(&format!("{}\nname \"{name}\"\n{link}\n", config::DEFAULT), SIZE)
}

/// The port the primary listens on.
fn port(primary: &Compositor) -> u16 {
    let listen = primary.state()["session"]["listen"].as_str().map(str::to_owned);
    let listen = listen.expect("the primary says where it listens");
    listen
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .expect("ADDR:PORT")
}

/// Polls `done` until it holds, up to TIMEOUT; panics with `what` (and the last state seen) after that.
fn wait(what: &str, o: &Compositor, done: impl Fn(&Value) -> bool) -> Value {
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

/// The session's machines as (name, role, screen numbers, workspace numbers).
fn machines(state: &Value) -> Vec<(String, String, Vec<u64>, Vec<u64>)> {
    let numbers = |v: &Value| {
        (v.as_array().expect("numbers").iter())
            .map(|n| n.as_u64().expect("number"))
            .collect()
    };
    (state["session"]["machines"].as_array().expect("machines").iter())
        .map(|m| {
            let screens = (m["monitors"].as_array().expect("monitors").iter())
                .map(|s| s["number"].as_u64().expect("number"))
                .collect();
            (
                m["name"].as_str().expect("name").to_owned(),
                m["role"].as_str().expect("role").to_owned(),
                screens,
                numbers(&m["workspaces"]),
            )
        })
        .collect()
}

fn machine(name: &str, role: &str, screens: &[u64], workspaces: &[u64]) -> (String, String, Vec<u64>, Vec<u64>) {
    (name.into(), role.into(), screens.to_vec(), workspaces.to_vec())
}

/// The machine holding the session's focus marker.
fn focused(state: &Value) -> String {
    let all = state["session"]["machines"].as_array().expect("machines");
    let m = all.iter().find(|m| m["focused"] == true).expect("one is focused");
    m["name"].as_str().expect("name").to_owned()
}

/// The workspace numbers that exist here.
fn numbers(state: &Value) -> Vec<u64> {
    (state["workspaces"].as_array().expect("workspaces").iter())
        .map(|w| w["number"].as_u64().expect("number"))
        .collect()
}

/// The first screen's (number, the workspace it shows).
fn screen(state: &Value) -> (u64, u64) {
    let m = &state["monitors"][0];
    (
        m["number"].as_u64().expect("number"),
        m["workspace"].as_u64().expect("workspace"),
    )
}

fn joined(state: &Value) -> bool {
    state["session"]["machines"].as_array().is_some_and(|m| m.len() == 2)
}

fn connected(state: &Value) -> bool {
    state["session"]["machines"][0]["connected"] == true
}

fn error(response: &Value) -> String {
    response["error"].as_str().expect("an error").to_owned()
}

#[test]
fn a_satellite_joins_and_workspaces_and_requests_are_routed_to_it() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = primary("desk", 0, &secret);
    let s = satellite("side", port(&p), &secret);

    // Both started with screen 1: the satellite's becomes the lowest free number, 2.
    let state = wait("the satellite to join", &p, joined);
    assert_eq!(
        machines(&state),
        [
            machine("desk", "primary", &[1], &[1]),
            machine("side", "satellite", &[2], &[2])
        ]
    );
    assert_eq!(state["session"]["foreign"], serde_json::json!([2]));
    assert_eq!(state["session"]["machine"], "desk");
    let sat = wait("the satellite to see it joined", &s, |st| {
        connected(st) && screen(st) == (2, 2)
    });
    assert_eq!(numbers(&sat), [2]);
    assert_eq!(sat["session"]["role"], "satellite");
    assert_eq!(sat["session"]["foreign"], serde_json::json!([1]));
    assert_eq!(machines(&sat), [machine("side", "satellite", &[2], &[2])]);
    // The link is pinged every 2 s: a round trip shows up.
    wait("a latency", &p, |st| {
        st["session"]["machines"][1]["latency_ms"].is_u64()
    });

    // `--to` runs a request there: the satellite makes workspace 3 and the primary learns it owns it.
    p.msg_ok(&["--to", "side", "workspace", "3"]);
    wait("the satellite to show 3", &s, |st| screen(st) == (2, 3));
    wait("the session to know 3", &p, |st| {
        machines(st)[1] == machine("side", "satellite", &[2], &[2, 3])
    });
    assert_eq!(focused(&p.state()), "desk", "`--to` moves no focus marker");

    // `workspace 2` on the primary: the satellite shows it, the focus marker moves there.
    p.msg_ok(&["workspace", "2"]);
    wait("the satellite to show 2", &s, |st| screen(st) == (2, 2));
    assert_eq!(focused(&p.state()), "side");
    // A new number is made on the focused machine (3, empty and hidden, goes away there).
    p.msg_ok(&["workspace", "4"]);
    wait("the satellite to show 4", &s, |st| screen(st) == (2, 4));
    wait("the session to know 4", &p, |st| machines(st)[1].3 == [2, 4]);
    assert_eq!(screen(&p.state()), (1, 1), "the primary's own screen untouched");
    p.msg_ok(&["workspace", "1"]);
    assert_eq!(focused(&p.state()), "desk");

    // Windows never cross machines, from either end.
    let refused = p.msg_err(&["move-to-workspace", "2"]);
    assert_eq!(error(&refused), "workspace 2 is on side: windows never cross machines");
    let refused = s.msg_err(&["move-to-workspace", "1"]);
    assert!(error(&refused).contains("windows never cross machines"), "{refused}");

    // `--to` with any request, spawn's argv included, answered by that machine.
    p.msg_ok(&["--to", "side", "spawn", "true", "--flag-of-true"]);
    let there = p.msg_ok(&["--to", "side", "state"]);
    assert_eq!(there["state"]["session"]["machine"], "side");
    let here = p.msg_ok(&["--to", "desk", "state"]);
    assert_eq!(here["state"]["session"]["machine"], "desk");
    let refused = p.msg_err(&["--to", "nobody", "state"]);
    assert!(error(&refused).contains("nobody"), "{refused}");
    let refused = s.msg_err(&["--to", "desk", "state"]);
    assert!(error(&refused).contains("primary"), "{refused}");

    drop(s);
    assert!(p.stop().success());
}

#[test]
fn a_wrong_secret_never_joins() {
    let (one, two) = (random_secret(), random_secret());
    let (_a, secret) = secret_file(&one);
    let (_b, other) = secret_file(&two);
    let p = primary("desk", 0, &secret);
    let s = satellite("side", port(&p), &other);
    let deadline = Instant::now() + TIMEOUT;
    while !p.log().contains("handshake failed") {
        assert!(Instant::now() < deadline, "no handshake failure logged:\n{}", p.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    // The satellite tries again (after 1 s, then 2 s): the same failure is logged once, and stays in `state`.
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(p.log().matches("handshake failed").count(), 1, "{}", p.log());
    let state = p.state();
    assert_eq!(machines(&state), [machine("desk", "primary", &[1], &[1])]);
    let why = state["session"]["error"].as_str().expect("the failure").to_owned();
    assert!(
        why.starts_with("127.0.0.1: ") && why.contains("handshake failed"),
        "{why}"
    );
    let sat = s.state();
    assert!(!connected(&sat), "{sat:#}");
    assert!(sat["session"]["error"].is_string(), "{sat:#}");
    assert!(!p.log().contains(&one) && !s.log().contains(&two), "a secret in a log");
}

#[test]
fn a_new_secret_on_reload_drops_the_links_until_both_ends_have_it() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = primary("desk", 0, &secret);
    let s = satellite("side", port(&p), &secret);
    wait("the satellite to join", &p, joined);
    wait("the satellite to see it", &s, connected);
    // Both read the same file: rewritten, then reloaded on the primary, which drops the link at once.
    write_secret(&secret, &random_secret());
    p.msg_ok(&["reload"]);
    wait("the drop", &p, |st| !joined(st));
    wait("the satellite to notice", &s, |st| !connected(st));
    assert!(p.log().contains("the secret changed"), "{}", p.log());
    // The satellite, still on the old secret, cannot rejoin until it reloads too.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!joined(&p.state()));
    s.msg_ok(&["reload"]);
    wait("the rejoin", &p, joined);
}

#[test]
fn a_name_held_by_a_silent_link_is_taken_over() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = primary("desk", 0, &secret);
    let port = port(&p);
    let old = satellite("side", port, &secret);
    wait("the first to join", &p, joined);
    // Frozen: its connection stays up, but it sends nothing more, pongs included.
    let stopped = Command::new("kill")
        .args(["-STOP", &old.pid().to_string()])
        .status()
        .expect("kill");
    assert!(stopped.success());
    std::thread::sleep(Duration::from_millis(4500));
    let new = satellite("side", port, &secret);
    wait("the new one to join", &new, connected);
    assert_eq!(machines(&p.state()).len(), 2);
    assert!(p.log().contains("replaced"), "{}", p.log());
    drop(old);
}

#[test]
fn connections_still_joining_are_capped_per_address() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = primary("desk", 0, &secret);
    let addr = format!("127.0.0.1:{}", port(&p));
    // Connected, saying nothing: two are kept waiting for their handshake, the third is closed at once.
    let conns: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect(&addr).expect("connect")).collect();
    std::thread::sleep(Duration::from_millis(300));
    let closed: Vec<bool> = (conns.iter())
        .map(|mut c| {
            c.set_read_timeout(Some(Duration::from_millis(200))).expect("timeout");
            match c.read(&mut [0u8; 64]) {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => false,
                Err(_) => true,
            }
        })
        .collect();
    assert_eq!(closed, [false, false, true]);
}

#[test]
fn a_duplicate_name_is_refused() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = primary("desk", 0, &secret);
    let port = port(&p);
    let first = satellite("side", port, &secret);
    wait("the first to join", &p, joined);
    let second = satellite("side", port, &secret);
    let state = wait("the refusal", &second, |st| {
        st["session"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("already in the session"))
    });
    assert!(!connected(&state));
    // The primary's own name is taken too.
    let third = satellite("desk", port, &secret);
    wait("the refusal", &third, |st| {
        st["session"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("already in the session"))
    });
    assert_eq!(machines(&p.state()).len(), 2);
    assert!(connected(&first.state()));
}

#[test]
fn a_satellite_keeps_its_numbers_through_a_drop_and_rejoins() {
    let good = random_secret();
    let (_dir, secret) = secret_file(&good);
    let p = primary("desk", 0, &secret);
    let port = port(&p);
    let s = satellite("side", port, &secret);
    wait("the satellite to join", &p, joined);
    // Made on the satellite itself: claimed by its report. Shown, so it stays.
    s.msg_ok(&["workspace", "3"]);
    wait("the session to know 3", &p, |st| machines(st)[1].3 == [2, 3]);

    // The primary goes: the satellite keeps working, with every number it had.
    assert!(p.stop().success());
    let alone = wait("the satellite to notice", &s, |st| !connected(st));
    assert_eq!(alone["session"]["foreign"], serde_json::json!([]));
    assert_eq!((screen(&alone), numbers(&alone)), ((2, 3), vec![2, 3]));
    s.msg_ok(&["layout", "halves"]);
    s.msg_ok(&["spawn", "true"]);
    assert_eq!(s.state()["monitors"][0]["layout"], "halves");

    // Back on the same port: the satellite rejoins with its numbers.
    let p = primary("desk", port, &secret);
    let state = wait("the rejoin", &p, joined);
    assert_eq!(
        machines(&state),
        [
            machine("desk", "primary", &[1], &[1]),
            machine("side", "satellite", &[2], &[2, 3])
        ]
    );
    wait("the satellite to see it", &s, connected);
    assert!(p.stop().success());

    // Back again, but the satellite cannot join until the secret is fixed; meanwhile the primary makes 2, the
    // satellite's screen. On the rejoin that number moves to the lowest free one, 4 (3 is still the satellite's).
    let (_wrong, wrong) = secret_file(&random_secret());
    let p = primary("desk", port, &wrong);
    p.msg_ok(&["workspace", "2"]);
    assert_eq!(numbers(&p.state()), [1, 2]);
    write_secret(&wrong, &good);
    p.msg_ok(&["reload"]);
    let state = wait("the rejoin", &p, joined);
    assert_eq!(
        machines(&state),
        [
            machine("desk", "primary", &[1], &[1, 2]),
            machine("side", "satellite", &[4], &[3, 4])
        ]
    );
    let sat = wait("the satellite renamed", &s, |st| connected(st) && screen(st).0 == 4);
    assert_eq!((screen(&sat), numbers(&sat)), ((4, 3), vec![3, 4]));
    assert_eq!(sat["session"]["foreign"], serde_json::json!([1, 2]));
}

#[test]
fn alone_there_is_no_session_and_no_to() {
    let o = Compositor::start(config::DEFAULT, SIZE);
    assert!(o.state().get("session").is_none());
    let refused = o.msg_err(&["--to", "desk", "state"]);
    assert!(error(&refused).contains("not in a session"), "{refused}");
}
