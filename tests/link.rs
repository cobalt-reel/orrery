//! Linked orrerys: a headless sun and headless planets on loopback (`support::orrery`).

mod support;

use std::io::{ErrorKind, Read as _};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use orrery_wm::config;
use serde_json::Value;
use support::Compositor;
use support::system::{
    SIZE, connected, joined, planet, port, random_secret, secret_file, sun, wait, wait_log, write_secret,
};

/// The orrery's machines as (name, role, screen numbers, workspace numbers).
fn machines(state: &Value) -> Vec<(String, String, Vec<u64>, Vec<u64>)> {
    let numbers = |v: &Value| {
        (v.as_array().expect("numbers").iter())
            .map(|n| n.as_u64().expect("number"))
            .collect()
    };
    (state["orrery"]["machines"].as_array().expect("machines").iter())
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

/// The machine holding the orrery's focus marker.
fn focused(state: &Value) -> String {
    let all = state["orrery"]["machines"].as_array().expect("machines");
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

fn error(response: &Value) -> String {
    response["error"].as_str().expect("an error").to_owned()
}

#[test]
fn a_planet_joins_and_workspaces_and_requests_are_routed_to_it() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = sun("desk", 0, &secret);
    let s = planet("side", port(&p), &secret);

    // Both started with screen 1: the planet's becomes the lowest free number, 2.
    let state = wait("the planet to join", &p, joined);
    assert_eq!(
        machines(&state),
        [
            machine("desk", "sun", &[1], &[1]),
            machine("side", "planet", &[2], &[2])
        ]
    );
    assert_eq!(state["orrery"]["foreign"], serde_json::json!([2]));
    assert_eq!(state["orrery"]["machine"], "desk");
    let planet = wait("the planet to see it joined", &s, |st| {
        connected(st) && screen(st) == (2, 2)
    });
    assert_eq!(numbers(&planet), [2]);
    assert_eq!(planet["orrery"]["role"], "planet");
    assert_eq!(planet["orrery"]["foreign"], serde_json::json!([1]));
    assert_eq!(machines(&planet), [machine("side", "planet", &[2], &[2])]);
    // The link is pinged every 2 s: a round trip shows up.
    wait("a latency", &p, |st| st["orrery"]["machines"][1]["latency_ms"].is_u64());

    // `--to` runs a request there: the planet makes workspace 3 and the sun learns it owns it.
    p.msg_ok(&["--to", "side", "workspace", "3"]);
    wait("the planet to show 3", &s, |st| screen(st) == (2, 3));
    wait("the orrery to know 3", &p, |st| {
        machines(st)[1] == machine("side", "planet", &[2], &[2, 3])
    });
    assert_eq!(focused(&p.state()), "desk", "`--to` moves no focus marker");

    // `workspace 2` on the sun: the planet shows it, the focus marker moves there.
    p.msg_ok(&["workspace", "2"]);
    wait("the planet to show 2", &s, |st| screen(st) == (2, 2));
    assert_eq!(focused(&p.state()), "side");
    // A new number is made on the focused machine (3, empty and hidden, goes away there).
    p.msg_ok(&["workspace", "4"]);
    wait("the planet to show 4", &s, |st| screen(st) == (2, 4));
    wait("the orrery to know 4", &p, |st| machines(st)[1].3 == [2, 4]);
    assert_eq!(screen(&p.state()), (1, 1), "the sun's own screen untouched");
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
    assert_eq!(there["state"]["orrery"]["machine"], "side");
    let here = p.msg_ok(&["--to", "desk", "state"]);
    assert_eq!(here["state"]["orrery"]["machine"], "desk");
    let refused = p.msg_err(&["--to", "nobody", "state"]);
    assert!(error(&refused).contains("nobody"), "{refused}");
    let refused = s.msg_err(&["--to", "desk", "state"]);
    assert!(error(&refused).contains("sun"), "{refused}");

    drop(s);
    assert!(p.stop().success());
}

#[test]
fn a_wrong_secret_never_joins() {
    let (one, two) = (random_secret(), random_secret());
    let (_a, secret) = secret_file(&one);
    let (_b, other) = secret_file(&two);
    let p = sun("desk", 0, &secret);
    let s = planet("side", port(&p), &other);
    wait_log(&p, "handshake failed");
    // The planet tries again (after 1 s, then 2 s): the same failure is logged once, and stays in `state`.
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(p.log().matches("handshake failed").count(), 1, "{}", p.log());
    let state = p.state();
    assert_eq!(machines(&state), [machine("desk", "sun", &[1], &[1])]);
    let why = state["orrery"]["error"].as_str().expect("the failure").to_owned();
    assert!(
        why.starts_with("127.0.0.1: ") && why.contains("handshake failed"),
        "{why}"
    );
    let planet = s.state();
    assert!(!connected(&planet), "{planet:#}");
    assert!(planet["orrery"]["error"].is_string(), "{planet:#}");
    assert!(!p.log().contains(&one) && !s.log().contains(&two), "a secret in a log");
}

#[test]
fn a_new_secret_on_reload_drops_the_links_until_both_ends_have_it() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = sun("desk", 0, &secret);
    let s = planet("side", port(&p), &secret);
    wait("the planet to join", &p, joined);
    wait("the planet to see it", &s, connected);
    // Both read the same file: rewritten, then reloaded on the sun, which drops the link at once.
    write_secret(&secret, &random_secret());
    p.msg_ok(&["reload"]);
    wait("the drop", &p, |st| !joined(st));
    wait("the planet to notice", &s, |st| !connected(st));
    assert!(p.log().contains("the secret changed"), "{}", p.log());
    // The planet, still on the old secret, cannot rejoin until it reloads too.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!joined(&p.state()));
    s.msg_ok(&["reload"]);
    wait("the rejoin", &p, joined);
}

#[test]
fn a_name_held_by_a_silent_link_is_taken_over() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = sun("desk", 0, &secret);
    let port = port(&p);
    let old = planet("side", port, &secret);
    wait("the first to join", &p, joined);
    // Frozen: its connection stays up, but it sends nothing more, pongs included.
    let stopped = Command::new("kill")
        .args(["-STOP", &old.pid().to_string()])
        .status()
        .expect("kill");
    assert!(stopped.success());
    std::thread::sleep(Duration::from_millis(4500));
    let new = planet("side", port, &secret);
    wait("the new one to join", &new, connected);
    assert_eq!(machines(&p.state()).len(), 2);
    assert!(p.log().contains("replaced"), "{}", p.log());
    drop(old);
}

#[test]
fn connections_still_joining_are_capped_per_address() {
    let (_dir, secret) = secret_file(&random_secret());
    let p = sun("desk", 0, &secret);
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
    let p = sun("desk", 0, &secret);
    let port = port(&p);
    let first = planet("side", port, &secret);
    wait("the first to join", &p, joined);
    let second = planet("side", port, &secret);
    let state = wait("the refusal", &second, |st| {
        st["orrery"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("already in the orrery"))
    });
    assert!(!connected(&state));
    // The sun's own name is taken too.
    let third = planet("desk", port, &secret);
    wait("the refusal", &third, |st| {
        st["orrery"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("already in the orrery"))
    });
    assert_eq!(machines(&p.state()).len(), 2);
    assert!(connected(&first.state()));
}

#[test]
fn a_planet_keeps_its_numbers_through_a_drop_and_rejoins() {
    let good = random_secret();
    let (_dir, secret) = secret_file(&good);
    let p = sun("desk", 0, &secret);
    let port = port(&p);
    let s = planet("side", port, &secret);
    wait("the planet to join", &p, joined);
    // Made on the planet itself: claimed by its report. Shown, so it stays.
    s.msg_ok(&["workspace", "3"]);
    wait("the orrery to know 3", &p, |st| machines(st)[1].3 == [2, 3]);

    // The sun goes: the planet keeps working, with every number it had.
    assert!(p.stop().success());
    let alone = wait("the planet to notice", &s, |st| !connected(st));
    assert_eq!(alone["orrery"]["foreign"], serde_json::json!([]));
    assert_eq!((screen(&alone), numbers(&alone)), ((2, 3), vec![2, 3]));
    s.msg_ok(&["layout", "halves"]);
    s.msg_ok(&["spawn", "true"]);
    assert_eq!(s.state()["monitors"][0]["layout"], "halves");

    // Back on the same port: the planet rejoins with its numbers.
    let p = sun("desk", port, &secret);
    let state = wait("the rejoin", &p, joined);
    assert_eq!(
        machines(&state),
        [
            machine("desk", "sun", &[1], &[1]),
            machine("side", "planet", &[2], &[2, 3])
        ]
    );
    wait("the planet to see it", &s, connected);
    assert!(p.stop().success());

    // Back again, but the planet cannot join until the secret is fixed; meanwhile the sun makes 2, the
    // planet's screen. On the rejoin that number moves to the lowest free one, 4 (3 is still the planet's).
    let (_wrong, wrong) = secret_file(&random_secret());
    let p = sun("desk", port, &wrong);
    p.msg_ok(&["workspace", "2"]);
    assert_eq!(numbers(&p.state()), [1, 2]);
    write_secret(&wrong, &good);
    p.msg_ok(&["reload"]);
    let state = wait("the rejoin", &p, joined);
    assert_eq!(
        machines(&state),
        [
            machine("desk", "sun", &[1], &[1, 2]),
            machine("side", "planet", &[4], &[3, 4])
        ]
    );
    let planet = wait("the planet renamed", &s, |st| connected(st) && screen(st).0 == 4);
    assert_eq!((screen(&planet), numbers(&planet)), ((4, 3), vec![3, 4]));
    assert_eq!(planet["orrery"]["foreign"], serde_json::json!([1, 2]));
}

#[test]
fn alone_there_is_no_system_and_no_to() {
    let o = Compositor::start(config::DEFAULT, SIZE);
    assert!(o.state().get("orrery").is_none());
    let refused = o.msg_err(&["--to", "desk", "state"]);
    assert!(error(&refused).contains("not in an orrery"), "{refused}");
}
