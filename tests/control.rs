//! The control socket end to end, headless only: every request over `orrery msg` (and raw lines it would never
//! send), errors included, and the socket's life: private, one orrery per socket, stale ones replaced, removed
//! on a clean exit.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use orrery_wm::NAME;
use orrery_wm::comp::ipc_server::socket_path_in;
use orrery_wm::config;
use orrery_wm::layout::Rect;
use serde_json::Value;
use support::client::Client;
use support::{Compositor, TIMEOUT, wait_or_kill};

const SIZE: (i32, i32) = (1280, 720);

fn rects(layout: &str) -> Vec<Rect> {
    let c = config::parse(config::DEFAULT).expect("the default config parses");
    let l = c.layouts.iter().find(|l| l.name == layout).expect("layout exists");
    l.rects(Rect {
        x: 0,
        y: 0,
        w: SIZE.0,
        h: SIZE.1,
    })
}

fn size(r: Rect) -> Option<(i32, i32)> {
    Some((r.w, r.h))
}

fn start() -> (Compositor, Client) {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let client = Client::new(orrery.connect());
    (orrery, client)
}

/// Each slot's tabs, left to right, each its windows' app ids bottom to top, on the first monitor.
fn tabs(state: &Value) -> Vec<Vec<Vec<String>>> {
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
fn windows(state: &Value) -> Vec<Vec<String>> {
    tabs(state).into_iter().map(|slot| slot.concat()).collect()
}

/// The index of each slot's current tab (None for an empty slot).
fn current_tabs(state: &Value) -> Vec<Option<usize>> {
    state["monitors"][0]["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .map(|slot| slot["tabs"].as_array()?.iter().position(|t| t["current"] == true))
        .collect()
}

fn focused_slot(state: &Value) -> Option<u64> {
    state["monitors"][0]["slots"]
        .as_array()?
        .iter()
        .find(|s| s["focused"] == true)
        .and_then(|s| s["index"].as_u64())
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// a, b, c opened (and mapped) in order into slot 0.
fn open_three(client: &mut Client) -> [usize; 3] {
    let a = client.create_window("a", "A");
    let b = client.create_window("b", "B");
    let c = client.create_window("c", "C");
    assert!(client.wait_for(|s| s.windows[c].mapped() && !s.windows[a].mapped() && !s.windows[b].mapped()));
    [a, b, c]
}

#[test]
fn state_describes_monitors_slots_and_tabs() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    let state = orrery.state();
    let mon = &state["monitors"][0];
    assert_eq!(mon["name"], "headless");
    assert_eq!(mon["layout"], "wall");
    assert_eq!(mon["focused"], true);
    assert_eq!(state["config_error"], Value::Null);
    assert_eq!(mon["slots"].as_array().map(Vec::len), Some(6));
    assert_eq!(mon["slots"][3]["name"], "hero");
    assert_eq!(mon["slots"][3]["rect"]["w"], rects("wall")[3].w);
    assert_eq!(windows(&state)[0], strs(&["a", "b", "c"]));
    assert_eq!(tabs(&state)[0], [strs(&["a", "b", "c"])], "one tab, three deep");
    assert_eq!(state["monitors"][0]["slots"][0]["tabs"][0]["windows"][2]["title"], "C");
    assert_eq!(state["monitors"][0]["slots"][0]["tabs"][0]["current"], true);
    assert_eq!(state["monitors"][0]["slots"][0]["tabs"][0]["attention"], false);
    assert_eq!(state["stacking"], true);
    assert_eq!(focused_slot(&state), Some(0));
    // Pretty-printed for people: more than one line.
    let out = orrery.msg(&["state"]);
    assert!(String::from_utf8_lossy(&out.stdout).lines().count() > 10);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn focus_slot_moves_focus_and_new_windows_open_there() {
    let (orrery, mut client) = start();
    assert_eq!(orrery.msg_ok(&["focus-slot", "right"]), serde_json::json!({"ok": true}));
    assert_eq!(focused_slot(&orrery.state()), Some(3), "right of slot 0 is the hero");
    let w = client.create_window("w", "W");
    let hero = size(rects("wall")[3]);
    assert!(client.wait_for(|s| s.windows[w].mapped() && s.windows[w].size == hero));
    assert_eq!(windows(&orrery.state())[3], strs(&["w"]));
    // At the edge: no-op, still ok.
    orrery.msg_ok(&["focus-slot", "up"]);
    assert_eq!(focused_slot(&orrery.state()), Some(3));
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn move_takes_the_whole_stack_to_the_neighbour() {
    let (orrery, mut client) = start();
    let [a, b, c] = open_three(&mut client);
    orrery.msg_ok(&["move", "right"]);
    let hero = size(rects("wall")[3]);
    let ok = client.wait_for(|s| s.windows.iter().all(|w| w.size == hero) && s.windows[c].activated());
    assert!(
        ok,
        "the stack moved to the hero with focus: {:#?}",
        client.state.windows
    );
    assert!(!client.window(a).mapped() && !client.window(b).mapped());
    let state = orrery.state();
    assert!(windows(&state)[0].is_empty());
    assert_eq!(tabs(&state)[3], [strs(&["a", "b", "c"])], "one tab, the whole stack");
    assert_eq!(focused_slot(&state), Some(3));
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn move_tab_is_gone() {
    let (orrery, client) = start();
    let out = orrery.msg(&["move-tab", "right"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "nothing was sent");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown request"));
    let answer = orrery.raw(b"{\"cmd\":\"move-tab\",\"dir\":\"right\"}\n");
    assert!(answer.contains("bad request"), "{answer}");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn split_makes_the_top_window_a_tab_of_its_own() {
    let (orrery, mut client) = start();
    let [a, b, c] = open_three(&mut client);
    orrery.msg_ok(&["split"]);
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a", "b"]), strs(&["c"])]);
    assert_eq!(current_tabs(&state)[0], Some(1), "the new tab is current");
    client.roundtrip();
    assert!(client.window(c).mapped() && client.window(c).activated());
    // Back to the first tab: b is its top.
    orrery.msg_ok(&["cycle-tab", "prev"]);
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated() && !s.windows[c].mapped()));
    assert!(!client.window(a).mapped());
    // A stack of one has nothing to split; still ok.
    orrery.msg_ok(&["cycle-tab", "next"]);
    orrery.msg_ok(&["split"]);
    assert_eq!(tabs(&orrery.state())[0], [strs(&["a", "b"]), strs(&["c"])]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn with_stacking_off_a_new_window_opens_as_a_new_tab() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    orrery.msg_ok(&["toggle-stacking"]);
    assert_eq!(orrery.state()["stacking"], false);
    let d = client.create_window("d", "D");
    assert!(client.wait_for(|s| s.windows[d].mapped() && s.windows[d].activated()));
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a", "b", "c"]), strs(&["d"])]);
    assert_eq!(current_tabs(&state)[0], Some(1));
    // An empty focused slot gets its first tab either way.
    orrery.msg_ok(&["focus-slot", "right"]);
    let e = client.create_window("e", "E");
    assert!(client.wait_for(|s| s.windows[e].mapped()));
    assert_eq!(tabs(&orrery.state())[3], [strs(&["e"])]);
    // On again: on top of the current stack.
    orrery.msg_ok(&["toggle-stacking"]);
    assert_eq!(orrery.state()["stacking"], true);
    let f = client.create_window("f", "F");
    assert!(client.wait_for(|s| s.windows[f].mapped() && !s.windows[e].mapped()));
    assert_eq!(tabs(&orrery.state())[3], [strs(&["e", "f"])]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_toggled_stacking_survives_a_reload_unless_the_config_changes_it() {
    let (orrery, client) = start();
    let stacking = || orrery.state()["stacking"].clone();
    orrery.msg_ok(&["toggle-stacking"]);
    // The same config (stacking on): the toggle stays.
    orrery.write_config(config::DEFAULT);
    orrery.msg_ok(&["reload"]);
    assert_eq!(stacking(), false);
    // The config's own value changed (to off): it wins over the toggle (on again).
    orrery.msg_ok(&["toggle-stacking"]);
    assert_eq!(stacking(), true);
    let off = config::DEFAULT.replace("stacking #true", "stacking #false");
    orrery.write_config(&off);
    orrery.msg_ok(&["reload"]);
    assert_eq!(stacking(), false);
    // Unchanged again: the toggle survives.
    orrery.msg_ok(&["toggle-stacking"]);
    orrery.write_config(&format!("{off}// touched\n"));
    orrery.msg_ok(&["reload"]);
    assert_eq!(stacking(), true);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn msg_spawn_runs_the_argv_and_reports_a_failure() {
    let (orrery, client) = start();
    assert_eq!(
        orrery.msg_ok(&["spawn", "sh", "-c", "exit 0"]),
        serde_json::json!({"ok": true})
    );
    let err = orrery.msg_err(&["spawn", "/nonexistent/program", "--flag"]);
    assert!(
        err["error"].as_str().is_some_and(|e| e.contains("cannot spawn")),
        "{err}"
    );
    // No program: refused before anything is sent.
    let out = orrery.msg(&["spawn"]);
    assert!(!out.status.success() && out.stdout.is_empty());
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn swap_slot_swaps_whole_stacks() {
    let (orrery, mut client) = start();
    let [_, _, c] = open_three(&mut client);
    orrery.msg_ok(&["swap-slot", "down"]);
    let below = size(rects("wall")[1]);
    assert!(client.wait_for(|s| s.windows.iter().all(|w| w.size == below) && s.windows[c].activated()));
    let state = orrery.state();
    assert!(windows(&state)[0].is_empty());
    assert_eq!(windows(&state)[1], strs(&["a", "b", "c"]));
    assert_eq!(focused_slot(&state), Some(1), "focus follows the stack");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn cycle_tab_shows_the_next_tab_and_moves_nothing() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped() && !s.windows[a].mapped()));
    // One tab, however deep: nothing to cycle.
    orrery.msg_ok(&["cycle-tab", "next"]);
    client.roundtrip();
    assert!(client.window(b).mapped() && !client.window(a).mapped());
    // The hero gets two tabs, [a, b] and [c], the second current.
    orrery.msg_ok(&["move", "right"]);
    orrery.msg_ok(&["focus-slot", "left"]);
    let c = client.create_window("c", "C");
    assert!(client.wait_for(|s| s.windows[c].mapped()));
    orrery.msg_ok(&["move", "right"]);
    let hero = size(rects("wall")[3]);
    assert!(client.wait_for(|s| s.windows[c].size == hero && s.windows[c].activated() && !s.windows[b].mapped()));
    orrery.msg_ok(&["cycle-tab", "next"]);
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated() && !s.windows[c].mapped()));
    assert_eq!(windows(&orrery.state())[3], strs(&["a", "b", "c"]), "tabs never move");
    orrery.msg_ok(&["cycle-tab", "prev"]);
    assert!(client.wait_for(|s| s.windows[c].mapped() && s.windows[c].activated() && !s.windows[b].mapped()));
    assert!(!client.window(a).mapped());
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn layout_errors_name_the_problem_and_change_nothing() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    let before = orrery.state();

    let err = orrery.msg_err(&["layout", "nope"]);
    assert_eq!(err["ok"], false);
    assert!(
        err["error"].as_str().is_some_and(|e| e.contains("unknown layout")),
        "{err}"
    );
    let err = orrery.msg_err(&["layout", "halves", "HDMI-9"]);
    assert!(
        err["error"].as_str().is_some_and(|e| e.contains("unknown monitor")),
        "{err}"
    );
    let err = orrery.msg_err(&[r#"{"cmd":"layout","name":"halves","monitor":"nope"}"#]);
    assert!(
        err["error"].as_str().is_some_and(|e| e.contains("unknown monitor")),
        "{err}"
    );
    assert_eq!(orrery.state(), before);

    // By name, with the monitor given.
    orrery.msg_ok(&["layout", "halves", "headless"]);
    assert_eq!(orrery.state()["monitors"][0]["layout"], "halves");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn close_asks_the_focused_window_to_go() {
    let (orrery, mut client) = start();
    let [a, b, c] = open_three(&mut client);
    orrery.msg_ok(&["close"]);
    assert!(client.wait_for(|s| s.windows[c].close_requested));
    assert!(!client.window(a).close_requested && !client.window(b).close_requested);
    // Asking is not closing: c stays until the client destroys it.
    assert_eq!(windows(&orrery.state())[0], strs(&["a", "b", "c"]));
    client.destroy_window(c);
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated()));

    // Nothing focused in an empty slot: an error, nobody asked.
    orrery.msg_ok(&["focus-slot", "right"]);
    let err = orrery.msg_err(&["close"]);
    assert!(
        err["error"].as_str().is_some_and(|e| e.contains("no focused window")),
        "{err}"
    );
    client.roundtrip();
    assert!(!client.window(b).close_requested);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn reload_reports_a_broken_config_and_keeps_the_old_one() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    // A good config reloads: halves renamed to pair, and the binds follow.
    orrery.write_config(
        &config::DEFAULT
            .replace("layout \"halves\"", "layout \"pair\"")
            .replace("\"Mod+2\" layout \"halves\"", "\"Mod+2\" layout \"pair\""),
    );
    assert_eq!(orrery.msg_ok(&["reload"]), serde_json::json!({"ok": true}));
    orrery.msg_ok(&["layout", "pair"]);
    orrery.msg_ok(&["layout", "wall"]);

    orrery.write_config("mod \"Alt\"\nbogus 1\n");
    let err = orrery.msg_err(&["reload"]);
    let message = err["error"].as_str().expect("an error");
    assert!(message.contains("config.kdl") && message.contains("2:1"), "{message}");
    let state = orrery.state();
    assert_eq!(
        state["config_error"].as_str(),
        Some(message),
        "state shows it until fixed"
    );
    orrery.msg_ok(&["layout", "pair"]);
    assert_eq!(windows(&orrery.state()).concat(), strs(&["a", "b", "c"]));

    orrery.write_config(config::DEFAULT);
    orrery.msg_ok(&["reload"]);
    assert!(orrery.state()["config_error"].is_null());
    orrery.msg_err(&["layout", "pair"]);
    orrery.msg_ok(&["layout", "halves"]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn quit_stops_orrery_cleanly_and_removes_the_socket() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    assert_eq!(orrery.msg_ok(&["quit"]), serde_json::json!({"ok": true}));
    let (status, runtime) = orrery.wait_exit();
    assert!(status.success(), "quit is a clean exit: {status}");
    assert!(
        !socket_path_in(runtime.path()).exists(),
        "the control socket is removed"
    );
}

#[test]
fn sigterm_removes_the_socket_and_it_was_private() {
    let (orrery, client) = start();
    let mode = std::fs::metadata(orrery.control_socket())
        .expect("the control socket exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    drop(client);
    let (status, runtime) = orrery.stop_keeping_runtime();
    assert!(status.success());
    assert!(!socket_path_in(runtime.path()).exists());
}

#[test]
fn bad_lines_get_bad_request_and_orrery_carries_on() {
    let (orrery, client) = start();
    for line in [
        &b"not json\n"[..],
        b"{\"cmd\":\"fly\"}\n",
        b"{\"cmd\":\"state\",\"extra\":1}\n",
        b"{\"cmd\":\"focus-slot\",\"dir\":\"sideways\"}\n",
        b"{\"cmd\":\"state\"",
    ] {
        let answer = orrery.raw(line);
        let v: Value = serde_json::from_str(answer.trim_end()).expect("one JSON line back");
        assert_eq!(v["ok"], false, "{answer}");
        assert!(
            v["error"].as_str().is_some_and(|e| e.starts_with("bad request")),
            "{answer}"
        );
        assert_eq!(answer.matches('\n').count(), 1, "exactly one line: {answer:?}");
    }
    // Over 64 KiB, no newline in sight.
    let answer = orrery.raw(&vec![b' '; 70 * 1024]);
    assert!(answer.contains("bad request") && answer.contains("64 KiB"), "{answer}");
    // A request without a trailing newline, ended by EOF, is still a request.
    let answer = orrery.raw(br#"{"cmd":"state"}"#);
    assert!(answer.starts_with(r#"{"ok":true"#), "{answer}");
    // Connecting and leaving silently is fine too.
    orrery.raw(b"");
    orrery.msg_ok(&["state"]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn msg_refuses_bad_words_locally_and_reports_a_missing_orrery() {
    let (orrery, client) = start();
    let out = orrery.msg(&["focus-slot", "sideways"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "nothing was sent");
    assert!(String::from_utf8_lossy(&out.stderr).contains("direction"));
    drop(client);
    assert!(orrery.stop().success());

    let dir = tempfile::tempdir().expect("temp dir");
    let out = Command::new(support::BIN)
        .args(["msg", "--socket"])
        .arg(socket_path_in(dir.path()))
        .arg("state")
        .output()
        .expect("run orrery msg");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("cannot reach {NAME}")));
}

#[test]
fn a_second_orrery_refuses_a_live_socket() {
    let (orrery, client) = start();
    let runtime = tempfile::tempdir().expect("temp runtime dir");
    let mut second = Command::new(support::BIN)
        .args(["run", "--backend", "headless", "--socket"])
        .arg(orrery.control_socket())
        .arg("--config")
        .arg(orrery.config_dir.path().join("config.kdl"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn a second orrery");
    let status = wait_or_kill(&mut second, "the second orrery kept running");
    assert!(!status.success());
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().expect("stderr"), &mut stderr).expect("stderr");
    assert!(stderr.contains("already running"), "{stderr}");
    // It refused before opening a Wayland socket, and the first one still answers.
    let leftovers: Vec<_> = std::fs::read_dir(runtime.path()).expect("runtime dir").collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    orrery.msg_ok(&["state"]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_stale_socket_is_replaced() {
    let runtime = tempfile::tempdir().expect("temp runtime dir");
    // A socket file nobody listens on: a orrery that was killed.
    drop(UnixListener::bind(socket_path_in(runtime.path())).expect("bind"));
    let orrery = Compositor::start_in(runtime, config::DEFAULT, SIZE);
    assert_eq!(orrery.state()["monitors"][0]["name"], "headless");
    assert!(orrery.stop().success());
}

#[test]
fn a_disconnected_client_leaves_exactly_the_other_tabs() {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let mut first = Client::new(orrery.connect());
    let mut second = Client::new(orrery.connect());
    let a = first.create_window("a", "A");
    assert!(first.wait_for(|s| s.windows[a].mapped()));
    let c = second.create_window("c", "C");
    assert!(second.wait_for(|s| s.windows[c].mapped()));
    let b = first.create_window("b", "B");
    assert!(first.wait_for(|s| s.windows[b].mapped()));
    assert_eq!(windows(&orrery.state())[0], strs(&["a", "c", "b"]));

    drop(first);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let t = windows(&orrery.state());
        if t[0] == strs(&["c"]) {
            assert!(t[1..].iter().all(Vec::is_empty), "{t:?}");
            break;
        }
        assert!(Instant::now() < deadline, "still {t:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(second.wait_for(|s| s.windows[c].mapped() && s.windows[c].activated()));
    drop(second);
    assert!(orrery.stop().success());
}

/// Runs `orrery run --backend headless --socket <socket>` with its own runtime dir, expecting it to refuse at
/// once; its stderr.
fn refused_with(socket: &std::path::Path) -> String {
    let runtime = tempfile::tempdir().expect("temp runtime dir");
    let mut child = Command::new(support::BIN)
        .args(["run", "--backend", "headless", "--socket"])
        .arg(socket)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_CONFIG_HOME", runtime.path())
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orrery");
    let status = wait_or_kill(&mut child, "the compositor kept running");
    assert!(!status.success());
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("stderr"), &mut stderr).expect("stderr");
    stderr
}

#[test]
fn a_path_that_is_not_a_socket_is_refused_and_kept() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = socket_path_in(dir.path());
    std::fs::write(&path, "mine").expect("write");
    let stderr = refused_with(&path);
    assert!(stderr.contains("not a socket"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&path).expect("still there"), "mine");
}

#[test]
fn a_socket_dir_others_can_write_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let path = socket_path_in(dir.path());
    let stderr = refused_with(&path);
    assert!(stderr.contains("writable"), "{stderr}");
    assert!(!path.exists());
    // Sticky makes it acceptable (like /tmp, were it ours).
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o1777)).expect("chmod");
    let runtime = tempfile::tempdir().expect("temp runtime dir");
    let mut child = Command::new(support::BIN)
        .args(["run", "--backend", "headless", "--socket"])
        .arg(&path)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_CONFIG_HOME", runtime.path())
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn orrery");
    let deadline = Instant::now() + TIMEOUT;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{NAME} listens in a sticky dir of ours");
        assert!(child.try_wait().expect("wait").is_none(), "{NAME} exited");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_socket_replaced_under_orrery_is_not_removed_on_exit() {
    let (orrery, client) = start();
    let path = orrery.control_socket();
    std::fs::remove_file(&path).expect("remove");
    let impostor = UnixListener::bind(&path).expect("bind");
    drop(client);
    let (status, runtime) = orrery.stop_keeping_runtime();
    assert!(status.success());
    assert!(
        socket_path_in(runtime.path()).exists(),
        "not ours any more, so left alone"
    );
    drop(impostor);
}

#[test]
fn a_connection_that_never_finishes_its_line_is_dropped_at_the_deadline() {
    use std::io::{Read, Write};
    let (orrery, client) = start();
    let mut s = std::os::unix::net::UnixStream::connect(orrery.control_socket()).expect("connect");
    s.write_all(br#"{"cmd":"#).expect("write");
    s.set_read_timeout(Some(TIMEOUT)).expect("timeout");
    let begun = Instant::now();
    let mut answer = Vec::new();
    let n = s.read_to_end(&mut answer).expect("closed, not timed out");
    let waited = begun.elapsed();
    assert_eq!(n, 0, "no answer: {:?}", String::from_utf8_lossy(&answer));
    assert!(
        waited >= Duration::from_millis(1500) && waited < Duration::from_secs(8),
        "{waited:?}"
    );
    orrery.msg_ok(&["state"]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn over_sixteen_connections_the_next_is_busy() {
    let (orrery, client) = start();
    let idle: Vec<_> = (0..16)
        .map(|_| std::os::unix::net::UnixStream::connect(orrery.control_socket()).expect("connect"))
        .collect();
    let answer = orrery.raw(b"{\"cmd\":\"state\"}\n");
    assert_eq!(answer.trim_end(), r#"{"ok":false,"error":"busy"}"#);
    drop(idle);
    // Closed connections free their places once orrery sees them go.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let out = orrery.msg(&["state"]);
        if out.status.success() {
            break;
        }
        assert!(Instant::now() < deadline, "{}", String::from_utf8_lossy(&out.stdout));
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(client);
    assert!(orrery.stop().success());
}

/// CPU time (user + system, clock ticks) a process has used.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    // Fields after the parenthesised command name; utime and stime are fields 14 and 15.
    let rest = &stat[stat.rfind(')').expect("comm") + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    f[11].parse::<u64>().expect("utime") + f[12].parse::<u64>().expect("stime")
}

/// The soft limit only: the hard one stays, so the limit can be raised back.
fn set_nofile(pid: u32, limit: u64, hard: u64) {
    let lim = libc::rlimit {
        rlim_cur: limit,
        rlim_max: hard,
    };
    let pid = libc::pid_t::try_from(pid).expect("pid");
    // Safety: a valid rlimit for the call to read.
    let rc = unsafe { libc::prlimit(pid, libc::RLIMIT_NOFILE, &lim, std::ptr::null_mut()) };
    assert_eq!(rc, 0, "prlimit: {}", std::io::Error::last_os_error());
}

#[test]
fn out_of_descriptors_orrery_backs_off_instead_of_spinning() {
    use std::io::{Read, Write};
    let (orrery, client) = start();
    let pid = orrery.pid();
    let mut old = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // Safety: valid pointers for the call.
    assert_eq!(
        unsafe { libc::prlimit(pid as libc::pid_t, libc::RLIMIT_NOFILE, std::ptr::null(), &mut old) },
        0
    );
    // The lowest free descriptor number is the next one accept would use: make it just out of reach.
    let used: std::collections::HashSet<u64> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("fds")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    let lowest_free = (0..).find(|n| !used.contains(n)).expect("a free fd");
    set_nofile(pid, lowest_free, old.rlim_max);

    let mut s = std::os::unix::net::UnixStream::connect(orrery.control_socket()).expect("queued in the backlog");
    s.write_all(b"{\"cmd\":\"state\"}\n").expect("write");
    let before = cpu_ticks(pid);
    std::thread::sleep(Duration::from_millis(1000));
    let spent = cpu_ticks(pid) - before;
    // Spinning on a ready listener would burn the whole second (~100 ticks).
    assert!(spent < 60, "{NAME} used {spent} ticks in 1 s while out of descriptors");

    set_nofile(pid, old.rlim_cur, old.rlim_max);
    s.set_read_timeout(Some(TIMEOUT)).expect("timeout");
    let mut answer = String::new();
    s.read_to_string(&mut answer)
        .expect("answered once descriptors are back");
    assert!(answer.starts_with(r#"{"ok":true"#), "{answer}");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_reader_that_does_not_read_does_not_stall_orrery() {
    use std::io::{Read, Write};
    let (orrery, mut client) = start();
    // Enough title text that the state answer cannot fit the socket buffer at once.
    let title = "t".repeat(3000);
    let mut last = 0;
    for i in 0..120 {
        last = client.create_window(&format!("w{i}"), &title);
    }
    assert!(client.wait_for(|s| s.windows[last].mapped()));
    let mut slow = std::os::unix::net::UnixStream::connect(orrery.control_socket()).expect("connect");
    slow.write_all(b"{\"cmd\":\"state\"}\n").expect("write");
    std::thread::sleep(Duration::from_millis(200));
    // orrery answers others while the slow reader's answer waits for room.
    let begun = Instant::now();
    assert_eq!(
        orrery.state()["monitors"][0]["slots"][0]["tabs"][0]["windows"]
            .as_array()
            .map(Vec::len),
        Some(120)
    );
    assert!(begun.elapsed() < Duration::from_secs(2), "{:?}", begun.elapsed());
    let mut answer = Vec::new();
    slow.set_read_timeout(Some(TIMEOUT)).expect("timeout");
    slow.read_to_end(&mut answer).expect("read");
    assert!(answer.len() > 300_000, "{}", answer.len());
    let v: Value = serde_json::from_slice(&answer).expect("the whole answer arrives");
    assert_eq!(v["ok"], true);
    drop(client);
    assert!(orrery.stop().success());
}
