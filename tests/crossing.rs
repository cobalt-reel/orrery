//! Input crossing: the primary's keyboard and pointer go where the session's focus marker is. Input a satellite is
//! sent runs through its own input path (binds included) to its clients, and is all released when it leaves; the
//! primary forwards its own devices' input there, takes it back with the escape key, on a link drop, or when the
//! pointer crosses back. `inject` (`--debug-inject` only) stands in for a machine's own devices; `inject-remote`
//! sends a satellite input messages as they are.

mod support;

use orrery_wm::config;
use serde_json::{Value, json};
use support::client::Client;
use support::session::{
    SIZE, connected, joined, port, primary_env, primary_with, random_secret, satellite_env, satellite_with,
    secret_file, wait, wait_log,
};
use support::{Compositor, TIMEOUT};
use tempfile::TempDir;

const INJECT: &[&str] = &["--debug-inject"];

/// xkb keycodes (evdev's plus 8: a client sees evdev's).
const SUPER: u32 = 133;
const CTRL: u32 = 37;
const ESCAPE: u32 = 9;
const CAPS: u32 = 66;
const A: u32 = 38;
const B: u32 = 56;
const ONE: u32 = 10;
const TWO: u32 = 11;
const LEFT_BUTTON: u32 = 272;
/// Super's bit in wl_keyboard.modifiers (Mod4).
const MOD4: u32 = 0x40;

/// A German layout, as keymap text a primary would send (includes resolved where it compiles).
const KEYMAP_DE: &str = r#"xkb_keymap {
    xkb_keycodes { include "evdev+aliases(qwertz)" };
    xkb_types { include "complete" };
    xkb_compat { include "complete" };
    xkb_symbols { include "pc+de+inet(evdev)" };
};"#;

fn evdev(xkb: u32) -> u32 {
    xkb - 8
}

/// A primary ("desk") and a satellite ("side"), both with `--debug-inject`, linked.
fn pair() -> (TempDir, Compositor, Compositor) {
    let (dir, secret) = secret_file(&random_secret());
    let p = primary_with("desk", 0, &secret, INJECT);
    let s = satellite_with("side", port(&p), &secret, INJECT);
    wait("the satellite to join", &p, joined);
    wait("the satellite to see it", &s, connected);
    (dir, p, s)
}

/// `message` (a link input message, as JSON) sent by the primary to "side".
fn send(p: &Compositor, message: Value) {
    let request = json!({"cmd": "inject-remote", "machine": "side", "message": message});
    p.msg_ok(&[&request.to_string()]);
}

/// The same, in words (`key 38 down`, `release`).
fn send_words(p: &Compositor, words: &[&str]) {
    let all: Vec<&str> = ["inject-remote", "side"].iter().chain(words).copied().collect();
    p.msg_ok(&all);
}

fn inject(o: &Compositor, words: &[&str]) {
    let all: Vec<&str> = std::iter::once("inject").chain(words.iter().copied()).collect();
    o.msg_ok(&all);
}

/// Keys through `inject`, each `(keycode, pressed)`.
fn keys(o: &Compositor, keys: &[(u32, bool)]) {
    for (key, pressed) in keys {
        inject(o, &["key", &key.to_string(), if *pressed { "down" } else { "up" }]);
    }
}

/// `inject` straight to the control socket, with no process per request: for floods.
fn inject_raw(o: &Compositor, input: &Value) {
    let answer = o.raw(format!("{}\n", json!({"cmd": "inject", "input": input})).as_bytes());
    assert!(answer.contains(r#""ok":true"#), "{answer}");
}

/// Where the primary's keyboard and pointer go, and which machine holds the focus marker.
fn input(state: &Value) -> String {
    state["session"]["input"].as_str().expect("input").to_owned()
}

fn marker(state: &Value) -> String {
    let machines = state["session"]["machines"].as_array().expect("machines");
    let focused = machines
        .iter()
        .find(|m| m["focused"] == true)
        .expect("a focused machine");
    focused["name"].as_str().expect("a name").to_owned()
}

/// Input and the marker are both on `machine`.
fn at(machine: &str) -> impl Fn(&Value) -> bool {
    move |st| input(st) == machine && marker(st) == machine
}

/// The input waiting on the primary for its link to the satellite to take more.
fn queued(state: &Value) -> u64 {
    state["session"]["machines"][1]["queued"].as_u64().expect("queued")
}

fn signal(o: &Compositor, sig: &str) {
    let sent = std::process::Command::new("kill")
        .args([sig, &o.pid().to_string()])
        .status();
    assert!(sent.expect("run kill").success(), "kill {sig}");
}

/// The rectangle (x, y, w, h) of the slot holding the window with `app_id`, on the first screen.
fn slot_of(state: &Value, app_id: &str) -> (f64, f64, f64, f64) {
    let slots = state["monitors"][0]["slots"].as_array().expect("slots");
    let slot = (slots.iter())
        .find(|s| {
            (s["tabs"].as_array().expect("tabs").iter())
                .flat_map(|t| t["windows"].as_array().expect("windows"))
                .any(|w| w["app_id"] == app_id)
        })
        .expect("the window's slot");
    let n = |k: &str| slot["rect"][k].as_f64().expect("a number");
    (n("x"), n("y"), n("w"), n("h"))
}

/// A window on `o`, with keyboard focus, drawn over its whole slot (so the pointer finds it anywhere there).
fn focused_window(o: &Compositor) -> (Client, usize) {
    let mut c = Client::new(o.connect());
    let w = c.create_window("typed", "typed");
    assert!(c.wait_for(|st| st.keyboard_focus == Some(w)), "no keyboard focus");
    let (width, height) = c.window(w).size.expect("configured");
    c.attach_buffer(w, width, height);
    (c, w)
}

/// The releases since `from`, sorted.
fn released(keys: &[(u32, bool)], from: usize) -> Vec<u32> {
    let mut up: Vec<u32> = keys[from..].iter().filter(|(_, p)| !p).map(|(k, _)| *k).collect();
    up.sort_unstable();
    up
}

#[test]
fn inject_is_refused_without_the_debug_flag() {
    let o = Compositor::start(config::DEFAULT, SIZE);
    for words in [
        &["inject", "key", "38", "down"][..],
        &["inject-remote", "side", "release"],
    ] {
        let refused = o.msg_err(words);
        let why = refused["error"].as_str().expect("an error");
        assert!(why.contains("--debug-inject"), "{why}");
    }
}

#[test]
fn injected_input_runs_as_this_machines_own() {
    let o = Compositor::start_with(config::DEFAULT, SIZE, INJECT);
    let (mut c, w) = focused_window(&o);
    inject(&o, &["key", &A.to_string(), "down"]);
    inject(&o, &["key", &A.to_string(), "up"]);
    assert!(
        c.wait_for(|st| st.keys == [(evdev(A), true), (evdev(A), false)]),
        "{:?}",
        c.state.keys
    );
    assert_eq!(o.state()["last_key"]["key"], "(plain key)", "typing is never kept");

    // A bind: Super+Ctrl+2 lays the screen out in halves; its key reaches no client, Super and Ctrl do.
    for (key, state) in [
        (SUPER, "down"),
        (CTRL, "down"),
        (TWO, "down"),
        (TWO, "up"),
        (CTRL, "up"),
        (SUPER, "up"),
    ] {
        inject(&o, &["key", &key.to_string(), state]);
    }
    wait("the halves layout", &o, |st| layout(st) == "halves");
    assert!(c.wait_for(|st| st.keys.len() == 6), "{:?}", c.state.keys);
    assert!(
        !c.state.keys.iter().any(|(k, _)| *k == evdev(TWO)),
        "{:?}",
        c.state.keys
    );

    // The pointer: relative motion from the top-left corner onto the window, kept on the screen; a click there.
    let (x, y, width, height) = slot_of(&o.state(), "typed");
    let size = (width as i32, height as i32);
    assert!(
        c.wait_for(|st| st.windows[w].size == Some(size)),
        "not configured to its new slot"
    );
    c.attach_buffer(w, size.0, size.1);
    inject(&o, &["motion", "-5000", "-5000"]);
    inject(
        &o,
        &[
            "motion",
            &(x + width / 2.0).to_string(),
            &(y + height / 2.0).to_string(),
        ],
    );
    inject(&o, &["button", &LEFT_BUTTON.to_string(), "down"]);
    inject(&o, &["button", &LEFT_BUTTON.to_string(), "up"]);
    assert!(
        c.wait_for(|st| st.pointer_focus == Some(w) && st.buttons == [(LEFT_BUTTON, true), (LEFT_BUTTON, false)]),
        "{:?} {:?}",
        c.state.pointer_focus,
        c.state.buttons
    );
}

#[test]
fn a_satellite_runs_what_the_primary_sends_and_release_lets_it_all_go() {
    let (_dir, p, s) = pair();
    let (mut c, w) = focused_window(&s);
    let local = *c.state.keymaps.last().expect("a keymap");

    // The primary's keymap, then a handoff with Super held and the pointer entering beside the window.
    send(&p, json!({"msg": "keymap", "xkb": KEYMAP_DE}));
    assert!(c.wait_for(|st| st.keymaps.len() == 2), "{:?}", c.state.keymaps);
    assert_ne!(c.state.keymaps[1], local, "another layout");
    let (x, y, width, height) = slot_of(&s.state(), "typed");
    let edge = if x == 0.0 { "left" } else { "right" };
    assert!(
        x == 0.0 || x + width == f64::from(SIZE.0),
        "the window's slot is at an edge"
    );
    let entry = (y + height / 2.0) / f64::from(SIZE.1);
    send(
        &p,
        json!({"msg": "handoff", "mods": [SUPER], "pointer": {"edge": edge, "y": entry}}),
    );
    assert!(
        c.wait_for(|st| st.keys == [(evdev(SUPER), true)] && st.mods_depressed & MOD4 != 0),
        "{:?} {:#x}",
        c.state.keys,
        c.state.mods_depressed
    );
    assert!(
        c.wait_for(|st| st.pointer_focus == Some(w)),
        "the pointer entered elsewhere"
    );

    // Keys and buttons as typed and clicked here.
    send_words(&p, &["key", &A.to_string(), "down"]);
    send_words(&p, &["button", &LEFT_BUTTON.to_string(), "down"]);
    assert!(
        c.wait_for(|st| st.keys.len() == 2 && st.buttons == [(LEFT_BUTTON, true)]),
        "{:?} {:?}",
        c.state.keys,
        c.state.buttons
    );
    assert_eq!(c.state.keys[1], (evdev(A), true));

    // Release: everything it had down goes up, the modifiers clear and the local keymap is back.
    send_words(&p, &["release"]);
    assert!(
        c.wait_for(|st| released(&st.keys, 2).len() == 2
            && st.buttons.len() == 2
            && st.mods_depressed == 0
            && st.keymaps.len() == 3),
        "{:?} {:?} {:#x} {:?}",
        c.state.keys,
        c.state.buttons,
        c.state.mods_depressed,
        c.state.keymaps
    );
    assert_eq!(released(&c.state.keys, 2), [evdev(A), evdev(SUPER)]);
    assert_eq!(c.state.buttons[1], (LEFT_BUTTON, false));
    assert_eq!(c.state.keymaps[2], local);
    // A release of what is no longer down goes nowhere: the next key is the next the client sees.
    send_words(&p, &["key", &A.to_string(), "up"]);
    send_words(&p, &["handoff"]);
    send_words(&p, &["key", &B.to_string(), "down"]);
    assert!(c.wait_for(|st| st.keys.len() == 5), "{:?}", c.state.keys);
    assert_eq!(c.state.keys[4], (evdev(B), true));
    send_words(&p, &["release"]);
}

#[test]
fn binds_typed_on_the_primary_run_on_the_satellite() {
    let (_dir, p, s) = pair();
    let (mut c, _) = focused_window(&s);
    send(&p, json!({"msg": "handoff", "mods": [SUPER, CTRL], "pointer": null}));
    send_words(&p, &["key", &TWO.to_string(), "down"]);
    send_words(&p, &["key", &TWO.to_string(), "up"]);
    wait("the halves layout", &s, |st| layout(st) == "halves");
    assert_eq!(layout(&p.state()), "wall", "not on the primary");
    assert!(c.wait_for(|st| st.keys.len() == 2), "{:?}", c.state.keys);
    assert!(
        !c.state.keys.iter().any(|(k, _)| *k == evdev(TWO)),
        "{:?}",
        c.state.keys
    );
}

#[test]
fn a_link_drop_releases_what_the_primary_had_down() {
    let (_dir, p, s) = pair();
    let (mut c, _) = focused_window(&s);
    send(&p, json!({"msg": "keymap", "xkb": KEYMAP_DE}));
    send_words(&p, &["handoff", &SUPER.to_string()]);
    send_words(&p, &["key", &A.to_string(), "down"]);
    assert!(c.wait_for(|st| st.keys.len() == 2), "{:?}", c.state.keys);
    let local = c.state.keymaps[0];

    assert!(p.stop().success());
    assert!(
        c.wait_for(|st| released(&st.keys, 2) == [evdev(A), evdev(SUPER)] && st.mods_depressed == 0),
        "{:?}",
        c.state.keys
    );
    assert!(
        c.wait_for(|st| st.keymaps.last() == Some(&local)),
        "{:?}",
        c.state.keymaps
    );
    wait("the satellite to notice", &s, |st| !connected(st));
}

#[test]
fn a_keyboard_reloaded_while_the_primarys_keymap_is_in_use_comes_after_it() {
    let (_dir, p, s) = pair();
    let (mut c, _) = focused_window(&s);
    let local = c.state.keymaps[0];
    send(&p, json!({"msg": "keymap", "xkb": KEYMAP_DE}));
    send_words(&p, &["handoff"]);
    assert!(c.wait_for(|st| st.keymaps.len() == 2), "{:?}", c.state.keymaps);

    let config = std::fs::read_to_string(s.config_path()).expect("the satellite's config");
    s.write_config(&format!("{config}\nkeyboard {{\n    layout \"fr\"\n}}\n"));
    s.msg_ok(&["reload"]);
    c.roundtrip();
    assert_eq!(
        c.state.keymaps.len(),
        2,
        "the primary's keymap stays while its input is here"
    );
    send_words(&p, &["release"]);
    assert!(c.wait_for(|st| st.keymaps.len() == 3), "{:?}", c.state.keymaps);
    assert!(
        c.state.keymaps[2] != local && c.state.keymaps[2] != c.state.keymaps[1],
        "the reloaded one: {:?}",
        c.state.keymaps
    );
}

#[test]
fn a_reload_keeping_the_layouts_comes_back_on_the_one_in_use() {
    let (dir, secret) = secret_file(&random_secret());
    let p = primary_with("desk", 0, &secret, INJECT);
    let toggle = "keyboard {\n    layout \"us,de\"\n    options \"grp:caps_toggle\"\n}\n";
    let s = satellite_with("side", port(&p), &secret, INJECT);
    let config = std::fs::read_to_string(s.config_path()).expect("the satellite's config");
    s.write_config(&format!("{config}\n{toggle}"));
    s.msg_ok(&["reload"]);
    wait("the satellite to join", &p, joined);
    wait("the satellite to see it", &s, connected);
    let (mut c, _) = focused_window(&s);
    inject(&s, &["key", &CAPS.to_string(), "down"]);
    inject(&s, &["key", &CAPS.to_string(), "up"]);
    assert!(c.wait_for(|st| st.group == 1), "group {}", c.state.group);
    send(&p, json!({"msg": "keymap", "xkb": KEYMAP_DE}));
    send_words(&p, &["handoff"]);
    assert!(
        c.wait_for(|st| st.group == 0 && st.keymaps.len() == 2),
        "{:?}",
        c.state.keymaps
    );
    // Other options, the same layouts, while the primary's keymap is in use: back on de after.
    s.write_config(&format!(
        "{config}\n{}",
        toggle.replace("grp:caps_toggle", "grp:caps_toggle,compose:ralt")
    ));
    s.msg_ok(&["reload"]);
    send_words(&p, &["release"]);
    assert!(
        c.wait_for(|st| st.keymaps.len() == 3 && st.group == 1),
        "{:?} group {}",
        c.state.keymaps,
        c.state.group
    );
    drop(dir);
}

#[test]
fn a_keymap_that_does_not_compile_closes_the_link() {
    let (_dir, p, s) = pair();
    let (mut c, _) = focused_window(&s);
    send_words(&p, &["handoff"]);
    send_words(&p, &["key", &A.to_string(), "down"]);
    assert!(c.wait_for(|st| st.keys.len() == 1), "{:?}", c.state.keys);
    send(&p, json!({"msg": "keymap", "xkb": "xkb_keymap { nonsense"}));
    wait_log(&s, "a protocol error: a keymap that does not compile");
    assert!(
        c.wait_for(|st| st.keys == [(evdev(A), true), (evdev(A), false)]),
        "{:?}",
        c.state.keys
    );
    assert_eq!(c.state.keymaps.len(), 1, "the local keymap all along");
    // The satellite joins again.
    wait("the rejoin", &s, connected);
    wait("the rejoin", &p, joined);
}

#[test]
fn the_pointer_leaving_a_satellite_is_reported_and_holds_still_until_input_comes_back() {
    let (_dir, p, _s) = pair();
    let left = "side's pointer left its screens past the left edge";
    send(
        &p,
        json!({"msg": "handoff", "mods": [], "pointer": {"edge": "left", "y": 0.5}}),
    );
    send_words(&p, &["motion", "-5", "0"]);
    wait_log(&p, &format!("{left}, 50 % down"));
    // Held: this motion goes nowhere, so after the next handoff (the pointer where it was) a small push left is
    // out again.
    send_words(&p, &["motion", "50", "0"]);
    send_words(&p, &["handoff"]);
    send_words(&p, &["motion", "-1", "0"]);
    let deadline = std::time::Instant::now() + TIMEOUT;
    while p.log().matches(left).count() < 2 {
        assert!(std::time::Instant::now() < deadline, "no second exit:\n{}", p.log());
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Motion inside goes nowhere near an edge; past the right edge it is out there, at the entry's height.
    send(
        &p,
        json!({"msg": "handoff", "mods": [], "pointer": {"edge": "right", "y": 0.25}}),
    );
    send_words(&p, &["motion", "-100", "0"]);
    send_words(&p, &["motion", "101", "0"]);
    wait_log(&p, "side's pointer left its screens past the right edge, 25 % down");
    assert_eq!(p.log().matches("side's pointer left").count(), 3, "{}", p.log());
}

#[test]
fn release_and_a_link_drop_free_a_pointer_held_after_an_exit() {
    let (_dir, p, _s) = pair();
    let left = "side's pointer left its screens past the left edge";
    let exits = |n: usize| {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while p.log().matches(left).count() < n {
            assert!(std::time::Instant::now() < deadline, "no exit {n}:\n{}", p.log());
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    send(
        &p,
        json!({"msg": "handoff", "mods": [], "pointer": {"edge": "left", "y": 0.5}}),
    );
    send_words(&p, &["motion", "-5", "0"]);
    exits(1);
    // Released, it moves again with no handoff: in, then out once more.
    send_words(&p, &["release"]);
    send_words(&p, &["motion", "50", "0"]);
    send_words(&p, &["motion", "-60", "0"]);
    exits(2);
    // Held again, then the link drops (a keymap that does not compile): after the rejoin it moves again.
    send(&p, json!({"msg": "keymap", "xkb": "xkb_keymap { nonsense"}));
    wait("the rejoin", &p, |st| !joined(st));
    wait("the rejoin", &p, joined);
    send_words(&p, &["motion", "50", "0"]);
    send_words(&p, &["motion", "-60", "0"]);
    exits(3);
}

#[test]
fn the_satellites_own_layout_is_back_after_the_primarys_keymap() {
    let (dir, secret) = secret_file(&random_secret());
    let p = primary_with("desk", 0, &secret, INJECT);
    let xkb = [
        ("XKB_DEFAULT_LAYOUT", "us,de"),
        ("XKB_DEFAULT_OPTIONS", "grp:caps_toggle"),
    ];
    let s = satellite_env("side", port(&p), &secret, INJECT, &xkb);
    wait("the satellite to join", &p, joined);
    wait("the satellite to see it", &s, connected);
    let (mut c, _) = focused_window(&s);
    // Its own keyboard to its second layout.
    inject(&s, &["key", &CAPS.to_string(), "down"]);
    inject(&s, &["key", &CAPS.to_string(), "up"]);
    assert!(c.wait_for(|st| st.group == 1), "group {}", c.state.group);
    // The primary's keymap (one layout) while input is here; this machine's own, on its second layout, after.
    send(&p, json!({"msg": "keymap", "xkb": KEYMAP_DE}));
    send_words(&p, &["handoff"]);
    assert!(
        c.wait_for(|st| st.keymaps.len() == 2 && st.group == 0),
        "{}",
        c.state.group
    );
    send_words(&p, &["release"]);
    assert!(
        c.wait_for(|st| st.keymaps.len() == 3 && st.group == 1),
        "{:?} group {}",
        c.state.keymaps,
        c.state.group
    );
    drop(dir);
}

#[test]
fn the_satellite_types_on_the_layout_the_primary_is_on() {
    let (dir, secret) = secret_file(&random_secret());
    let xkb = [
        ("XKB_DEFAULT_LAYOUT", "us,de"),
        ("XKB_DEFAULT_OPTIONS", "grp:caps_toggle"),
    ];
    let p = primary_env("desk", 0, &secret, INJECT, &xkb);
    let s = satellite_with("side", port(&p), &secret, INJECT);
    wait("the satellite to join", &p, joined);
    wait("the satellite to see it", &s, connected);
    let (mut c, _) = focused_window(&s);
    // The primary's keyboard to its second layout, then input to the satellite: its keymap, on that layout.
    keys(&p, &[(CAPS, true), (CAPS, false)]);
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    assert!(
        c.wait_for(|st| st.keymaps.len() == 2 && st.group == 1),
        "{:?} group {}",
        c.state.keymaps,
        c.state.group
    );
    // Home, the primary back on its first layout, and there again: the keymap is not sent again, the layout is.
    p.msg_ok(&["workspace", "1"]);
    wait("input home", &p, at("desk"));
    keys(&p, &[(CAPS, true), (CAPS, false)]);
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    assert!(
        c.wait_for(|st| st.keymaps.len() == 4 && st.group == 0),
        "{:?} group {}",
        c.state.keymaps,
        c.state.group
    );
    drop(dir);
}

#[test]
fn keys_typed_on_the_primary_go_where_the_marker_is() {
    let (_dir, p, s) = pair();
    let (mut here, _) = focused_window(&p);
    let (mut there, _) = focused_window(&s);
    assert_eq!(input(&p.state()), "desk");
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    keys(&p, &[(A, true), (A, false)]);
    assert!(
        there.wait_for(|st| st.keys == [(evdev(A), true), (evdev(A), false)]),
        "{:?}",
        there.state.keys
    );
    assert_eq!(p.state()["last_key"], Value::Null, "typing forwarded is never kept");
    // Back: they go to the primary's client again, and no more to the satellite's.
    p.msg_ok(&["workspace", "1"]);
    wait("input home", &p, at("desk"));
    keys(&p, &[(B, true), (B, false)]);
    assert!(
        here.wait_for(|st| st.keys == [(evdev(B), true), (evdev(B), false)]),
        "{:?}",
        here.state.keys
    );
    there.roundtrip();
    assert_eq!(there.state.keys.len(), 2, "{:?}", there.state.keys);
    assert_eq!(input(&s.state()), "side", "a satellite's own devices stay on it");
}

#[test]
fn super_stays_held_across_a_workspace_handoff() {
    let (_dir, p, s) = pair();
    let (mut here, _) = focused_window(&p);
    let (mut there, _) = focused_window(&s);
    // Super+2 on the primary: workspace 2 is the satellite's, so input goes there with Super still down.
    keys(&p, &[(SUPER, true), (TWO, true), (TWO, false), (A, true)]);
    wait("input on side", &p, at("side"));
    assert!(
        there.wait_for(|st| st.keys == [(evdev(SUPER), true), (evdev(A), true)] && st.mods_depressed & MOD4 != 0),
        "{:?} {:#x}",
        there.state.keys,
        there.state.mods_depressed
    );
    keys(&p, &[(A, false), (SUPER, false)]);
    assert!(
        there.wait_for(|st| st.keys.len() == 4 && st.mods_depressed == 0),
        "{:?}",
        there.state.keys
    );
    assert_eq!(there.state.keys[2..], [(evdev(A), false), (evdev(SUPER), false)]);
    // The primary's client saw Super go down and up (released at the handoff), and neither 2 (the bind's) nor A.
    assert!(
        here.wait_for(|st| st.keys == [(evdev(SUPER), true), (evdev(SUPER), false)]),
        "{:?}",
        here.state.keys
    );
}

#[test]
fn the_escape_key_brings_input_home_and_lets_go_on_the_satellite() {
    let (_dir, p, s) = pair();
    let (mut here, _) = focused_window(&p);
    let (mut there, _) = focused_window(&s);
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    keys(&p, &[(SUPER, true), (A, true)]);
    assert!(there.wait_for(|st| st.keys.len() == 2), "{:?}", there.state.keys);
    // Super+Escape: never forwarded; the satellite lets go of everything, Super is down here now.
    keys(&p, &[(ESCAPE, true)]);
    wait("input home", &p, at("desk"));
    assert!(
        there.wait_for(|st| released(&st.keys, 2) == [evdev(A), evdev(SUPER)] && st.mods_depressed == 0),
        "{:?}",
        there.state.keys
    );
    assert!(
        here.wait_for(|st| st.keys == [(evdev(SUPER), true)]),
        "{:?}",
        here.state.keys
    );
    // Escape's release and A's go nowhere; Super's comes here.
    keys(&p, &[(ESCAPE, false), (A, false), (SUPER, false)]);
    assert!(
        here.wait_for(|st| st.keys == [(evdev(SUPER), true), (evdev(SUPER), false)]),
        "{:?}",
        here.state.keys
    );
    there.roundtrip();
    assert_eq!(there.state.keys.len(), 4, "{:?}", there.state.keys);
    assert!(!there.state.keys.iter().any(|(k, _)| *k == evdev(ESCAPE)));
    assert_eq!(p.state()["last_key"]["bind"], "Reclaim");
}

#[test]
fn the_pointer_crosses_to_the_satellite_and_back() {
    let (_dir, p, _s) = pair();
    // Past the primary's right edge: onto the satellite, which takes the marker (and the keyboard) too.
    inject(&p, &["motion", "5000", "100"]);
    wait("input on side", &p, at("side"));
    // Past the satellite's right edge is past the row's end: it stays there, free to move.
    inject(&p, &["motion", "5000", "0"]);
    wait_log(&p, "side's pointer left its screens past the right edge");
    // Back past its left edge: home, at the primary's right edge.
    inject(&p, &["motion", "-5000", "0"]);
    wait("input home", &p, at("desk"));
    // A step left is on the primary's screen; past its left edge there is nothing.
    inject(&p, &["motion", "-5000", "0"]);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(at("desk")(&p.state()));
}

#[test]
fn an_exit_from_a_satellite_input_already_left_moves_nothing() {
    let (dir, secret) = secret_file(&random_secret());
    let p = primary_with("desk", 0, &secret, INJECT);
    let _side = satellite_with("side", port(&p), &secret, INJECT);
    wait("side to join", &p, joined);
    let _far = satellite_with("far", port(&p), &secret, INJECT);
    wait("far to join", &p, |st| {
        st["session"]["machines"].as_array().is_some_and(|m| m.len() == 3)
    });
    p.msg_ok(&["workspace", "3"]);
    wait("input on far", &p, at("far"));
    // side's pointer leaves past its left edge (input is not there): that would land on the primary.
    send(
        &p,
        json!({"msg": "handoff", "mods": [], "pointer": {"edge": "left", "y": 0.5}}),
    );
    send_words(&p, &["motion", "-5", "0"]);
    wait_log(&p, "side's pointer left its screens past the left edge");
    assert!(at("far")(&p.state()), "input stays on far");
    drop(dir);
}

#[test]
fn a_drag_stays_on_its_machine() {
    let (_dir, p, _s) = pair();
    inject(&p, &["button", &LEFT_BUTTON.to_string(), "down"]);
    inject(&p, &["motion", "5000", "0"]);
    inject(&p, &["button", &LEFT_BUTTON.to_string(), "up"]);
    assert!(at("desk")(&p.state()), "not while the button is down");
    inject(&p, &["motion", "1", "0"]);
    wait("input on side", &p, at("side"));
}

#[test]
fn motion_merges_while_the_link_is_stalled_and_keys_all_arrive_in_order() {
    let (_dir, p, s) = pair();
    let (mut there, w) = focused_window(&s);
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    // The satellite's pointer onto the window: from its top-left corner (where it starts) to the middle.
    let (x, y, width, height) = slot_of(&s.state(), "typed");
    inject(
        &p,
        &[
            "motion",
            &(x + width / 2.0).to_string(),
            &(y + height / 2.0).to_string(),
        ],
    );
    assert!(
        there.wait_for(|st| st.pointer_focus == Some(w)),
        "the pointer is not on the window"
    );
    let wiggle = |i: usize| json!({"msg": "motion", "dx": 0, "dy": if i.is_multiple_of(2) { 1 } else { -1 }});
    let key = |k: u32, pressed: bool| json!({"msg": "key", "keycode": k, "pressed": pressed, "time": 0});

    // The satellite stops reading; motion fills the link until input waits on the primary.
    signal(&s, "-STOP");
    let mut sent = 0;
    while queued(&p.state()) == 0 {
        for _ in 0..10 {
            inject_raw(&p, &wiggle(sent));
            sent += 1;
        }
        // Few steps are ever on their way unanswered: never seconds of motion to replay late.
        assert!(sent < 500, "{sent} steps and the link still takes more");
    }
    // Keys between long runs of motion: the motion merges, the keys wait as they are.
    let flood = |n: usize| {
        for i in 0..n {
            inject_raw(&p, &wiggle(i));
        }
    };
    inject_raw(&p, &key(A, true));
    flood(400);
    inject_raw(&p, &key(A, false));
    flood(400);
    inject_raw(&p, &key(B, true));
    inject_raw(&p, &key(B, false));
    flood(400);
    let waiting = queued(&p.state());
    assert!(
        (4..=8).contains(&waiting),
        "{waiting} waiting: 1200 steps merged between 4 keys"
    );
    let before = there.state.motions;

    signal(&s, "-CONT");
    let typed = [(evdev(A), true), (evdev(A), false), (evdev(B), true), (evdev(B), false)];
    assert!(there.wait_for(|st| st.keys == typed), "{:?}", there.state.keys);
    wait("the outbox to empty", &p, |st| queued(st) == 0);
    keys(&p, &[(ONE, true), (ONE, false)]);
    assert!(there.wait_for(|st| st.keys.len() == 6), "{:?}", there.state.keys);
    let moved = there.state.motions - before;
    assert!(moved <= sent + 4, "{moved} motion events for {} steps", sent + 1200);
}

#[test]
fn super_1_typed_while_input_is_on_the_satellite_comes_home_through_the_primary() {
    let (_dir, p, s) = pair();
    let (mut here, _) = focused_window(&p);
    let (mut there, _) = focused_window(&s);
    keys(&p, &[(SUPER, true), (TWO, true), (TWO, false)]);
    wait("input on side", &p, at("side"));
    // Super+1 runs on the satellite, whose 1 is the primary's: it asks the primary, which takes input home.
    keys(&p, &[(ONE, true)]);
    wait("input home", &p, at("desk"));
    assert!(
        there.wait_for(|st| st.keys == [(evdev(SUPER), true), (evdev(SUPER), false)]),
        "{:?}",
        there.state.keys
    );
    keys(&p, &[(ONE, false), (SUPER, false)]);
    assert!(
        here.wait_for(|st| st.keys
            == [
                (evdev(SUPER), true),
                (evdev(SUPER), false),
                (evdev(SUPER), true),
                (evdev(SUPER), false)
            ]),
        "{:?}",
        here.state.keys
    );
    // The satellite's own `workspace 1` goes the same way.
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    s.msg_ok(&["workspace", "1"]);
    wait("input home", &p, at("desk"));
}

#[test]
fn a_link_drop_while_input_is_away_brings_it_home() {
    let (_dir, p, s) = pair();
    let (mut here, _) = focused_window(&p);
    p.msg_ok(&["workspace", "2"]);
    wait("input on side", &p, at("side"));
    keys(&p, &[(SUPER, true)]);
    drop(s);
    wait("input home", &p, |st| input(st) == "desk" && !joined(st));
    assert!(
        here.wait_for(|st| st.keys == [(evdev(SUPER), true)]),
        "Super still held, here now: {:?}",
        here.state.keys
    );
    keys(&p, &[(SUPER, false)]);
    assert!(here.wait_for(|st| st.keys.len() == 2), "{:?}", here.state.keys);
}

/// The layout the first screen shows.
fn layout(state: &Value) -> String {
    state["monitors"][0]["layout"].as_str().expect("a layout").to_owned()
}
