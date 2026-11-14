//! Fullscreen as a real client sees it, headless: `toggle-fullscreen` and `xdg_toplevel.set_fullscreen` put the
//! focused slot's top window over its whole screen (a bar's strip too), configured to it with the fullscreen state;
//! the rest of the screen is covered (unmapped, not hit); it ends by itself when the window is no longer the top of
//! the focused slot.

mod support;

use orrery_wm::config;
use serde_json::Value;
use support::Compositor;
use support::client::{Anchor, Client, KeyboardInteractivity, Layer, LayerSpec};

const SIZE: (i32, i32) = (1280, 720);
const BAR: u32 = 30;
const INJECT: &[&str] = &["--debug-inject"];
/// xkb keycodes.
const SUPER: u32 = 133;
const F: u32 = 41;

fn inject(o: &Compositor, words: &[&str]) {
    let all: Vec<&str> = std::iter::once("inject").chain(words.iter().copied()).collect();
    o.msg_ok(&all);
}

/// The pointer to (x, y): into the top-left corner, then there.
fn point(o: &Compositor, x: i32, y: i32) {
    inject(o, &["motion", "-5000", "-5000"]);
    inject(o, &["motion", &x.to_string(), &y.to_string()]);
}

/// A compositor with a bar along the top of its one screen, drawn.
fn start_with_bar() -> (Compositor, Client, usize) {
    let orrery = Compositor::start_with(config::DEFAULT, SIZE, INJECT);
    let mut client = Client::new(orrery.connect());
    let bar = client.create_layer(&LayerSpec::bar(BAR));
    drawn(&mut client, bar);
    assert_eq!(
        orrery.state()["monitors"][0]["slots"][0]["rect"]["y"],
        i64::from(BAR),
        "the bar's zone"
    );
    (orrery, client, bar)
}

/// Waits until layer surface `l` has a buffer of its configured size attached.
fn drawn(c: &mut Client, l: usize) {
    assert!(
        c.wait_for(|s| {
            let l = &s.layers[l];
            l.size
                .is_some_and(|(w, h)| l.attached == Some((w.max(1) as i32, h.max(1) as i32)))
        }),
        "layer surface {l} never drew"
    );
}

/// Draws window `w` at its configured size (the client's windows are 1x1 otherwise), so the pointer can hit it.
fn draw(c: &mut Client, w: usize) {
    let (width, height) = c.window(w).size.expect("configured");
    c.attach_buffer(w, width, height);
}

/// The ids of the fullscreen windows `state` lists, every workspace's.
fn fullscreen_ids(state: &Value) -> Vec<u64> {
    let mut ids = Vec::new();
    for ws in state["workspaces"].as_array().expect("workspaces") {
        for slot in ws["slots"].as_array().expect("slots") {
            for tab in slot["tabs"].as_array().expect("tabs") {
                for w in tab["windows"].as_array().expect("windows") {
                    if w["fullscreen"] == true {
                        ids.push(w["id"].as_u64().expect("an id"));
                    }
                }
            }
        }
    }
    ids
}

fn slot_size(state: &Value, slot: usize) -> (i32, i32) {
    let r = &state["monitors"][0]["slots"][slot]["rect"];
    let n = |k: &str| i32::try_from(r[k].as_i64().expect("a number")).expect("small");
    (n("w"), n("h"))
}

#[test]
fn toggle_fullscreen_covers_the_whole_screen_and_gives_it_back() {
    let (orrery, mut client, bar) = start_with_bar();
    let a = client.create_window("a", "a");
    assert!(client.wait_for(|s| s.windows[a].mapped() && s.keyboard_focus == Some(a)));
    let state = orrery.state();
    let focus = usize::try_from(support::focused_slot(&state).expect("a focused slot")).expect("small");
    let slot = slot_size(&state, focus);
    assert_eq!(client.window(a).size, Some(slot));
    assert!(!client.window(a).fullscreen() && client.window(a).tiled());
    // The bar is under the pointer in its strip.
    point(&orrery, 200, 10);
    assert!(client.wait_for(|s| s.pointer_layer == Some(bar)));

    orrery.msg_ok(&["toggle-fullscreen"]);
    assert!(
        client.wait_for(|s| s.windows[a].fullscreen() && s.windows[a].size == Some(SIZE)),
        "configured to the screen, fullscreen: {:?}",
        client.window(a)
    );
    assert!(client.window(a).activated());
    assert_eq!(fullscreen_ids(&orrery.state()).len(), 1);
    // It is what the pointer is over now, the bar's strip included, at the screen's origin.
    draw(&mut client, a);
    point(&orrery, 200, 10);
    assert!(client.wait_for(|s| s.pointer_focus == Some(a) && s.pointer_layer.is_none()));
    point(&orrery, 200, 10);
    assert!(
        client.wait_for(|s| s.pointer_at == Some((200.0, 10.0))),
        "{:?}",
        client.state.pointer_at
    );
    // The layout still divides what the bar leaves: nothing moved beneath.
    assert_eq!(slot_size(&orrery.state(), focus), slot);

    // The bind does the same, back.
    for (key, down) in [(SUPER, "down"), (F, "down"), (F, "up"), (SUPER, "up")] {
        inject(&orrery, &["key", &key.to_string(), down]);
    }
    assert!(client.wait_for(|s| !s.windows[a].fullscreen() && s.windows[a].size == Some(slot)));
    assert!(fullscreen_ids(&orrery.state()).is_empty());
    point(&orrery, 200, 10);
    assert!(client.wait_for(|s| s.pointer_layer == Some(bar)), "the bar is back");
}

#[test]
fn toggle_fullscreen_needs_a_focused_window() {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let err = orrery.msg_err(&["toggle-fullscreen"]);
    assert!(
        err["error"].as_str().expect("an error").contains("no focused window"),
        "{err}"
    );
}

#[test]
fn a_client_asks_for_fullscreen_and_leaves_it() {
    let (orrery, mut client, _bar) = start_with_bar();
    let a = client.create_window("a", "a");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    let slot = client.window(a).size.expect("configured");
    client.set_fullscreen(a, true);
    assert!(client.wait_for(|s| s.windows[a].fullscreen() && s.windows[a].size == Some(SIZE)));
    assert_eq!(fullscreen_ids(&orrery.state()).len(), 1);
    // Asking again is answered (a configure) and changes nothing.
    let before = client.window(a).configures;
    client.set_fullscreen(a, true);
    assert!(
        client.wait_for(|s| s.windows[a].configures > before),
        "every request gets its configure"
    );
    assert!(client.window(a).fullscreen());
    client.set_fullscreen(a, false);
    assert!(client.wait_for(|s| !s.windows[a].fullscreen() && s.windows[a].size == Some(slot)));
    assert!(fullscreen_ids(&orrery.state()).is_empty());
}

#[test]
fn a_window_opens_fullscreen_when_it_asks_before_its_first_commit() {
    let (orrery, mut client, _bar) = start_with_bar();
    let a = client.create_fullscreen_window("player");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    assert_eq!(client.window(a).size, Some(SIZE), "its first configure is the screen's");
    assert!(client.window(a).fullscreen());
    assert_eq!(fullscreen_ids(&orrery.state()).len(), 1);
}

#[test]
fn fullscreen_covers_the_other_slots_and_ends_when_the_focus_leaves() {
    let (orrery, mut client, _bar) = start_with_bar();
    let a = client.create_window("a", "a");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["focus-slot", "right"]);
    let b = client.create_window("b", "b");
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.keyboard_focus == Some(b)));
    assert!(client.window(a).mapped(), "two slots, two windows drawn");
    let slot = client.window(b).size.expect("configured");

    orrery.msg_ok(&["toggle-fullscreen"]);
    assert!(client.wait_for(|s| s.windows[b].fullscreen() && !s.windows[a].mapped()));
    assert!(
        !client.window(a).fullscreen(),
        "the covered window keeps its slot's size"
    );
    // The pointer where the other slot is: the fullscreen window is there, and hovering keeps it so.
    draw(&mut client, b);
    point(&orrery, 20, 100);
    assert!(client.wait_for(|s| s.pointer_focus == Some(b)));
    assert_eq!(client.state.keyboard_focus, Some(b));

    // A request from the covered window is refused, with its configure.
    let before = client.window(a).configures;
    client.set_fullscreen(a, true);
    assert!(client.wait_for(|s| s.windows[a].configures > before));
    assert!(!client.window(a).fullscreen());
    assert!(client.window(b).fullscreen());

    // The focus to the other slot: back to tiles.
    orrery.msg_ok(&["focus-slot", "left"]);
    assert!(
        client.wait_for(|s| { !s.windows[b].fullscreen() && s.windows[b].size == Some(slot) && s.windows[a].mapped() })
    );
    assert_eq!(client.state.keyboard_focus, Some(a));
    assert!(fullscreen_ids(&orrery.state()).is_empty());

    // A window opening on top of a fullscreen one ends it too.
    orrery.msg_ok(&["toggle-fullscreen"]);
    assert!(client.wait_for(|s| s.windows[a].fullscreen()));
    let c = client.create_window("c", "c");
    assert!(client.wait_for(|s| s.windows[c].mapped() && !s.windows[a].fullscreen()));
    assert!(!client.window(c).fullscreen());
}

#[test]
fn fullscreen_stays_on_its_workspace_and_only_what_takes_the_keyboard_shows_over_it() {
    let (orrery, mut client, bar) = start_with_bar();
    let a = client.create_window("a", "a");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["toggle-fullscreen"]);
    assert!(client.wait_for(|s| s.windows[a].fullscreen()));

    // An overlay shows (and is hit) over it; so does a launcher on the top layer, which takes the keyboard.
    let corner = |namespace, layer, keyboard| LayerSpec {
        anchor: Anchor::Top | Anchor::Left,
        keyboard,
        ..LayerSpec::centred(namespace, layer, (100, 100))
    };
    let toast = client.create_layer(&corner("toast", Layer::Overlay, KeyboardInteractivity::None));
    drawn(&mut client, toast);
    point(&orrery, 50, 50);
    assert!(client.wait_for(|s| s.pointer_layer == Some(toast)));
    client.destroy_layer(toast);
    draw(&mut client, a);
    point(&orrery, 50, 10);
    assert!(
        client.wait_for(|s| s.pointer_focus == Some(a)),
        "the bar beneath stays covered"
    );
    let launcher = client.create_layer(&corner("launcher", Layer::Top, KeyboardInteractivity::Exclusive));
    drawn(&mut client, launcher);
    assert!(client.wait_for(|s| s.keyboard_layer == Some(launcher)));
    point(&orrery, 50, 50);
    assert!(client.wait_for(|s| s.pointer_layer == Some(launcher)));
    client.destroy_layer(launcher);
    assert!(client.wait_for(|s| s.keyboard_focus == Some(a)));

    // Another workspace is drawn as ever, bar and all; back, the window is fullscreen still.
    orrery.msg_ok(&["workspace", "4"]);
    assert!(client.wait_for(|s| !s.windows[a].mapped()));
    point(&orrery, 200, 10);
    assert!(client.wait_for(|s| s.pointer_layer == Some(bar)));
    assert_eq!(fullscreen_ids(&orrery.state()).len(), 1, "fullscreen while hidden");
    orrery.msg_ok(&["workspace", "1"]);
    assert!(client.wait_for(|s| s.windows[a].mapped() && s.pointer_focus == Some(a)));
    assert!(client.window(a).fullscreen());
    assert_eq!(client.window(a).size, Some(SIZE));

    // Unmapped, it is no longer fullscreen when it maps again.
    client.unmap(a);
    client.remap(a);
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    assert!(!client.window(a).fullscreen());
}
