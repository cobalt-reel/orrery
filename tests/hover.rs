//! focus-follows-mouse: moving the pointer into a slot focuses it; off by default; a grab (an open menu) holds it.

mod support;

use orrery_wm::config;
use serde_json::Value;
use support::client::Client;
use support::{Compositor, focused_slot};

const SIZE: (i32, i32) = (1280, 720);
const LEFT_BUTTON: u32 = 272;

fn inject(o: &Compositor, words: &[&str]) {
    let all: Vec<&str> = std::iter::once("inject").chain(words.iter().copied()).collect();
    o.msg_ok(&all);
}

fn with_hover_focus() -> String {
    format!("{}\nfocus-follows-mouse #true\n", config::DEFAULT)
}

/// How many slots the first screen shows.
fn slot_count(state: &Value) -> usize {
    state["monitors"][0]["slots"].as_array().expect("slots").len()
}

/// The centre of slot `i` of the first screen.
fn slot_centre(state: &Value, i: usize) -> (f64, f64) {
    let rect = &state["monitors"][0]["slots"][i]["rect"];
    let n = |k: &str| rect[k].as_f64().expect("a number");
    (n("x") + n("w") / 2.0, n("y") + n("h") / 2.0)
}

/// Moves the pointer to `(x, y)` from the top-left corner (no click).
fn move_to(o: &Compositor, (x, y): (f64, f64)) {
    inject(o, &["motion", "-5000", "-5000"]);
    inject(o, &["motion", &x.to_string(), &y.to_string()]);
}

/// A slot other than the focused one(s) given.
fn other_slot(state: &Value, not: &[Option<u64>]) -> usize {
    (0..slot_count(state))
        .find(|&i| !not.contains(&Some(i as u64)))
        .expect("another slot")
}

#[test]
fn moving_into_a_slot_focuses_it() {
    let o = Compositor::start_with(&with_hover_focus(), SIZE, &["--debug-inject"]);
    let state = o.state();
    assert!(slot_count(&state) >= 3, "a layout with several slots");
    let first = other_slot(&state, &[focused_slot(&state)]);
    move_to(&o, slot_centre(&state, first));
    support::system::wait("the hovered slot focused", &o, |s| {
        focused_slot(s) == Some(first as u64)
    });
    let second = other_slot(&state, &[focused_slot(&state), Some(first as u64)]);
    move_to(&o, slot_centre(&state, second));
    support::system::wait("the next hovered slot focused", &o, |s| {
        focused_slot(s) == Some(second as u64)
    });
    assert!(o.stop().success());
}

#[test]
fn off_by_default_moving_focuses_nothing() {
    let o = Compositor::start_with(config::DEFAULT, SIZE, &["--debug-inject"]);
    let state = o.state();
    let before = focused_slot(&state);
    move_to(&o, slot_centre(&state, other_slot(&state, &[before])));
    // Requests are answered in order, so this state comes after the motion was handled.
    assert_eq!(focused_slot(&o.state()), before, "a click is still needed");
    assert!(o.stop().success());
}

#[test]
fn an_open_menu_holds_the_focus_where_it_is() {
    let o = Compositor::start_with(&with_hover_focus(), SIZE, &["--debug-inject"]);
    let mut c = Client::new(o.connect());
    let w = c.create_window("menus", "menus");
    assert!(c.wait_for(|st| st.keyboard_focus == Some(w)), "no keyboard focus");
    let (width, height) = c.window(w).size.expect("configured");
    c.attach_buffer(w, width, height);
    let state = o.state();
    let home = focused_slot(&state).expect("a focused slot") as usize;
    move_to(&o, slot_centre(&state, home));
    inject(&o, &["button", &LEFT_BUTTON.to_string(), "down"]);
    inject(&o, &["button", &LEFT_BUTTON.to_string(), "up"]);
    assert!(
        c.wait_for(|st| st.press_serial.is_some()),
        "the click reached the window"
    );
    let p = c.create_grabbed_popup(w, c.state.press_serial.expect("a press"));
    assert!(c.wait_for(|st| st.popups[p].configures > 0), "the menu is configured");

    let elsewhere = other_slot(&state, &[Some(home as u64)]);
    inject(&o, &["motion", "-5000", "-5000"]);
    let (x, y) = slot_centre(&state, elsewhere);
    inject(&o, &["motion", &x.to_string(), &y.to_string()]);
    assert_eq!(
        focused_slot(&o.state()),
        Some(home as u64),
        "the menu's slot keeps the focus"
    );
    c.roundtrip();
    assert!(!c.state.popups[p].done, "the menu stays open");
    assert!(o.stop().success());
}
