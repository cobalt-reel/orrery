//! Popup grabs: a menu opened on a click (an xdg_popup grabbed with the click's serial) closes on a click outside its
//! client, and when the focus moves away from its window.

mod support;

use orrery_wm::config;
use serde_json::Value;
use support::Compositor;
use support::client::Client;

const SIZE: (i32, i32) = (1280, 720);
const LEFT_BUTTON: u32 = 272;

fn inject(o: &Compositor, words: &[&str]) {
    let all: Vec<&str> = std::iter::once("inject").chain(words.iter().copied()).collect();
    o.msg_ok(&all);
}

/// The centre of slot `i` of the first screen.
fn slot_centre(state: &Value, i: usize) -> (f64, f64) {
    let rect = &state["monitors"][0]["slots"][i]["rect"];
    let n = |k: &str| rect[k].as_f64().expect("a number");
    (n("x") + n("w") / 2.0, n("y") + n("h") / 2.0)
}

/// The index of the first slot of the first screen holding no tab.
fn empty_slot(state: &Value) -> usize {
    let slots = state["monitors"][0]["slots"].as_array().expect("slots");
    (slots.iter())
        .position(|s| s["tabs"].as_array().is_none_or(|t| t.is_empty()))
        .expect("an empty slot")
}

/// Moves the pointer to `(x, y)` from the top-left corner and clicks there.
fn click_at(o: &Compositor, (x, y): (f64, f64)) {
    inject(o, &["motion", "-5000", "-5000"]);
    inject(o, &["motion", &x.to_string(), &y.to_string()]);
    inject(o, &["button", &LEFT_BUTTON.to_string(), "down"]);
    inject(o, &["button", &LEFT_BUTTON.to_string(), "up"]);
}

/// A focused window drawn over its slot, clicked; then a popup on it grabbed with that click's serial.
fn window_with_menu(o: &Compositor) -> (Client, usize) {
    let mut c = Client::new(o.connect());
    let w = c.create_window("menus", "menus");
    assert!(c.wait_for(|st| st.keyboard_focus == Some(w)), "no keyboard focus");
    let (width, height) = c.window(w).size.expect("configured");
    c.attach_buffer(w, width, height);
    let state = o.state();
    let slot = (0..).find(|&i| {
        state["monitors"][0]["slots"][i]["tabs"]
            .as_array()
            .is_some_and(|t| !t.is_empty())
    });
    click_at(o, slot_centre(&state, slot.expect("the window's slot")));
    assert!(
        c.wait_for(|st| st.press_serial.is_some()),
        "the click reached the window"
    );
    let serial = c.state.press_serial.expect("a press");
    let p = c.create_grabbed_popup(w, serial);
    assert!(c.wait_for(|st| st.popups[p].configures > 0), "the menu is configured");
    assert!(!c.state.popups[p].done, "the menu stays open after its grab");
    (c, p)
}

#[test]
fn a_click_outside_its_client_closes_a_grabbed_menu() {
    let o = Compositor::start_with(config::DEFAULT, SIZE, &["--debug-inject"]);
    let (mut c, p) = window_with_menu(&o);
    click_at(&o, slot_centre(&o.state(), empty_slot(&o.state())));
    assert!(c.wait_for(|st| st.popups[p].done), "popup_done after a click outside");
    assert!(o.stop().success());
}

#[test]
fn moving_the_focus_away_closes_a_grabbed_menu() {
    let o = Compositor::start_with(config::DEFAULT, SIZE, &["--debug-inject"]);
    let (mut c, p) = window_with_menu(&o);
    o.msg_ok(&["workspace", "2"]);
    assert!(
        c.wait_for(|st| st.popups[p].done),
        "popup_done when its window is no longer focused"
    );
    assert!(o.stop().success());
}
