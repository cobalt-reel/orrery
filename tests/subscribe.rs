//! The event stream end to end, headless only: `orrery msg subscribe` gets the state, then an event per change;
//! a subscriber that does not read is dropped without holding orrery up.

mod support;

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;

use orrery_wm::config;
use serde_json::Value;
use support::client::Client;
use support::{Compositor, TIMEOUT};

const SIZE: (i32, i32) = (1280, 720);

/// The app ids of a `tabs` event's windows, tab by tab.
fn tab_apps(tabs: &Value) -> Vec<Vec<String>> {
    (tabs["tabs"].as_array().expect("tabs").iter())
        .map(|t| {
            (t["windows"].as_array().expect("windows").iter())
                .map(|w| w["app_id"].as_str().unwrap_or("?").to_owned())
                .collect()
        })
        .collect()
}

#[test]
fn a_subscriber_gets_the_state_then_every_change() {
    let orrery = Compositor::start_with(config::DEFAULT, SIZE, &["--debug-inject"]);
    // A key press orrery saw: `state` has it, the stream never does.
    orrery.msg_ok(&["inject", "key", "38", "down"]);
    orrery.msg_ok(&["inject", "key", "38", "up"]);
    assert!(orrery.state()["last_key"].is_object());

    let sub = orrery.subscribe();
    let state = sub.next("state");
    assert_eq!(state["monitors"][0]["name"], "headless");
    assert_eq!(state["colors"]["focus"], "#ff7edbff");
    assert!(state.get("last_key").is_none(), "{state}");

    let mut client = Client::new(orrery.connect());
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    let focus = sub.next("focus");
    assert_eq!(focus["screen"], "headless");
    assert_eq!(focus["workspace"], 1);
    assert_eq!(focus["slot"], 0);
    assert_eq!(focus["window"]["app_id"], "a");
    assert_eq!(focus["window"]["title"], "A");
    let tabs = sub.next("tabs");
    assert_eq!(tab_apps(&tabs), [["a"]]);
    assert_eq!(tabs["tabs"][0]["current"], true);

    // A second window as a new tab: the tabs list both, the second current.
    orrery.msg_ok(&["toggle-stacking"]);
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped()));
    let tabs = sub.wait_for("tabs with b", |v| v["event"] == "tabs" && tab_apps(v).len() == 2);
    assert_eq!(tab_apps(&tabs), [["a"], ["b"]]);
    assert_eq!(tabs["tabs"][1]["current"], true);

    // A title changes: its own event, and the tabs showing it.
    client.set_title(a, "renamed");
    let title = sub.next("title");
    assert_eq!(
        (title["app_id"].as_str(), title["title"].as_str()),
        (Some("a"), Some("renamed"))
    );
    assert_eq!(title["id"], tabs["tabs"][0]["windows"][0]["id"]);

    // Another workspace: shown on the screen, the old one still listed as occupied.
    orrery.msg_ok(&["workspace", "2"]);
    let ws = sub.next("workspace");
    assert_eq!(ws["screens"][0]["workspace"], 2);
    let marks: Vec<(u64, bool, bool)> = (ws["workspaces"].as_array().expect("workspaces").iter())
        .map(|w| {
            let n = w["number"].as_u64().expect("number");
            (
                n,
                w["shown"].as_bool().expect("shown"),
                w["occupied"].as_bool().expect("occupied"),
            )
        })
        .collect();
    assert_eq!(marks, [(1, false, true), (2, true, false)]);
    let focus = sub.next("focus");
    assert!(focus["window"].is_null(), "{focus}");

    // A reload, even of the same file, is a config event.
    orrery.msg_ok(&["reload"]);
    let cfg = sub.next("config");
    assert!(cfg["error"].is_null() && cfg["colors"]["badge"].is_string(), "{cfg}");

    orrery.msg_ok(&["layout", "halves"]);
    let layout = sub.next("layout");
    assert_eq!(layout["screens"][0]["layout"], "halves");

    // orrery stops: the stream ends, `msg subscribe` with it, and no line ever carried a key.
    drop(client);
    assert!(orrery.stop().success());
    let (rest, status) = sub.rest();
    assert!(status.success(), "{status:?}");
    assert!(rest.iter().all(|l| !l.contains("last_key")), "{rest:?}");
}

#[test]
fn a_subscriber_that_never_reads_is_dropped_and_orrery_carries_on() {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let mut stalled = UnixStream::connect(orrery.control_socket()).expect("connect");
    stalled.write_all(b"{\"cmd\":\"subscribe\"}\n").expect("subscribe");
    let reader = orrery.subscribe();
    reader.next("state");

    // Twenty windows on one stack with long titles: every title change sends all of them again (`tabs`).
    let mut client = Client::new(orrery.connect());
    let long = |n: usize| format!("{n:04}{}", "t".repeat(3800));
    let windows: Vec<usize> = (0..20).map(|n| client.create_window("w", &long(n))).collect();
    assert!(client.wait_for(|s| s.windows[windows[19]].mapped()));
    for round in 0..40 {
        client.set_title(windows[round % 20], &long(100 + round));
    }
    // The reader saw every change, to the last.
    let last = long(139);
    reader.wait_for("the last title", |v| {
        v["event"] == "title" && v["title"] == last.as_str()
    });

    // The stalled one was dropped: what was sent is there to read, then EOF.
    stalled.set_read_timeout(Some(TIMEOUT)).expect("timeout");
    let mut got = Vec::new();
    stalled.read_to_end(&mut got).expect("EOF, not a timeout");
    assert!(got.starts_with(b"{\"event\":\"state\""));
    assert!(orrery.log().contains("dropped"), "{}", orrery.log());
    assert_eq!(orrery.state()["monitors"][0]["name"], "headless");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn subscribe_is_for_this_machine_and_the_control_socket_only() {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let out = orrery.msg(&["--to", "side", "subscribe"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("control socket only"),
        "{out:?}"
    );
    let answer = orrery.raw(b"{\"cmd\":\"to\",\"machine\":\"side\",\"request\":{\"cmd\":\"subscribe\"}}\n");
    assert!(answer.contains("control socket only"), "{answer}");
    let out = orrery.msg(&["subscribe", "now"]);
    assert!(!out.status.success(), "subscribe takes no arguments");

    // At most 16 at once; the next is told so, and normal requests still go through.
    let subs: Vec<UnixStream> = (0..16)
        .map(|_| {
            let mut s = UnixStream::connect(orrery.control_socket()).expect("connect");
            s.write_all(b"{\"cmd\":\"subscribe\"}\n").expect("subscribe");
            let mut first = [0u8; 16];
            s.set_read_timeout(Some(TIMEOUT)).expect("timeout");
            s.read_exact(&mut first).expect("the state comes");
            assert_eq!(&first, b"{\"event\":\"state\"");
            s
        })
        .collect();
    let refused = orrery.raw(b"{\"cmd\":\"subscribe\"}\n");
    assert!(
        refused.contains("\"ok\":false") && refused.contains("subscribers"),
        "{refused}"
    );
    assert_eq!(orrery.state()["monitors"][0]["name"], "headless");
    // One leaves: its place is free again.
    drop(subs);
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let sub = orrery.subscribe();
        let first = sub.wait_for("anything", |_| true);
        if first["event"] == "state" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{first}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(orrery.stop().success());
}
