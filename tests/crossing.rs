//! Input crossing, the receiving side: input a satellite is sent runs through its own input path (binds included) to
//! its clients, and is all released when it leaves. A test-only request on the primary (`inject-remote`,
//! `--debug-inject` only) sends the satellite input messages as they are, and `inject` stands in for a machine's own
//! devices.

mod support;

use orrery_wm::config;
use serde_json::{Value, json};
use support::client::Client;
use support::session::{
    SIZE, connected, joined, port, primary_with, random_secret, satellite_with, secret_file, wait, wait_log,
};
use support::{Compositor, TIMEOUT};
use tempfile::TempDir;

const INJECT: &[&str] = &["--debug-inject"];

/// xkb keycodes (evdev's plus 8: a client sees evdev's).
const SUPER: u32 = 133;
const CTRL: u32 = 37;
const A: u32 = 38;
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
    // A release of what is no longer down goes nowhere.
    send_words(&p, &["key", &A.to_string(), "up"]);
    send_words(&p, &["handoff"]);
    send_words(&p, &["release"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    c.roundtrip();
    assert_eq!(c.state.keys.len(), 4, "{:?}", c.state.keys);
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

/// The layout the first screen shows.
fn layout(state: &Value) -> String {
    state["monitors"][0]["layout"].as_str().expect("a layout").to_owned()
}
