//! Layer-shell (`zwlr_layer_shell_v1`) as a real client sees it, headless: exclusive zones shrinking the area the
//! layout divides, the four layers in hit-testing, keyboard interactivity, popups, and a removed screen closing its
//! layer surfaces. `inject` (`--debug-inject`) moves the pointer, clicks and types.

mod support;

use orrery_wm::config;
use serde_json::{Value, json};
use support::Compositor;
use support::client::{Anchor, Client, KeyboardInteractivity, Layer, LayerSpec};

const SIZE: (i32, i32) = (1280, 720);
const INJECT: &[&str] = &["--debug-inject"];

/// xkb keycodes (evdev's plus 8: a client sees evdev's).
const SUPER: u32 = 133;
const CTRL: u32 = 37;
const A: u32 = 38;
const ONE: u32 = 10;
const TWO: u32 = 11;
const LEFT_BUTTON: u32 = 272;

fn evdev(xkb: u32) -> u32 {
    xkb - 8
}

fn start() -> (Compositor, Client) {
    let orrery = Compositor::start_with(config::DEFAULT, SIZE, INJECT);
    let client = Client::new(orrery.connect());
    (orrery, client)
}

fn inject(o: &Compositor, words: &[&str]) {
    let all: Vec<&str> = std::iter::once("inject").chain(words.iter().copied()).collect();
    o.msg_ok(&all);
}

/// The pointer to (x, y): into the top-left corner, then there.
fn point(o: &Compositor, x: i32, y: i32) {
    inject(o, &["motion", "-5000", "-5000"]);
    inject(o, &["motion", &x.to_string(), &y.to_string()]);
}

fn click(o: &Compositor) {
    inject(o, &["button", &LEFT_BUTTON.to_string(), "down"]);
    inject(o, &["button", &LEFT_BUTTON.to_string(), "up"]);
}

/// Presses then releases `keys` in order (a chord: all down, then all up in reverse).
fn chord(o: &Compositor, keys: &[u32]) {
    for k in keys {
        inject(o, &["key", &k.to_string(), "down"]);
    }
    for k in keys.iter().rev() {
        inject(o, &["key", &k.to_string(), "up"]);
    }
}

fn rect(v: &Value) -> (i64, i64, i64, i64) {
    let n = |k: &str| v[k].as_i64().expect("a number");
    (n("x"), n("y"), n("w"), n("h"))
}

/// Every slot rectangle of monitor `m`.
fn slot_rects(state: &Value, m: usize) -> Vec<(i64, i64, i64, i64)> {
    (state["monitors"][m]["slots"].as_array().expect("slots").iter())
        .map(|s| rect(&s["rect"]))
        .collect()
}

/// Monitor `m`'s layer surfaces as `state` lists them.
fn layers(state: &Value, m: usize) -> Vec<Value> {
    state["monitors"][m]["layers"].as_array().expect("layers").clone()
}

fn layout(state: &Value) -> String {
    state["monitors"][0]["layout"].as_str().expect("a layout").to_owned()
}

/// A window drawn over its whole slot (so the pointer finds it anywhere there), with keyboard focus.
fn full_window(c: &mut Client, app_id: &str) -> usize {
    let w = c.create_window(app_id, app_id);
    assert!(c.wait_for(|s| s.keyboard_focus == Some(w) && s.windows[w].size.is_some()));
    let (width, height) = c.window(w).size.expect("configured");
    c.attach_buffer(w, width, height);
    w
}

/// Waits until layer surface `l` has a buffer of its configured size.
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

#[test]
fn a_bar_moves_the_slots_down_by_its_zone_and_unmapping_gives_the_room_back() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    let before = slot_rects(&orrery.state(), 0);
    assert_eq!(before.iter().map(|r| r.1).min(), Some(0));

    let bar = client.create_layer(&LayerSpec::bar(30));
    drawn(&mut client, bar);
    assert_eq!(
        client.layer(bar).size,
        Some((SIZE.0 as u32, 30)),
        "the screen's width, its height"
    );
    assert!(
        client.wait_for(|s| s.layers[bar].outputs > 0),
        "never entered the output"
    );

    // Every slot inside what the bar leaves: the top row starts under it, the bottom row still ends at the edge.
    let state = orrery.state();
    let after = slot_rects(&state, 0);
    assert_eq!(after.iter().map(|r| r.1).min(), Some(30), "{after:?}");
    assert_eq!(
        after.iter().map(|r| r.1 + r.3).max(),
        Some(i64::from(SIZE.1)),
        "{after:?}"
    );
    assert_eq!(
        layers(&state, 0),
        [
            json!({"namespace": "bar", "layer": "top", "rect": {"x": 0, "y": 0, "w": SIZE.0, "h": 30}, "keyboard": false})
        ]
    );
    // The window is configured to its slot's new size.
    let (_, _, w, h) = after[0];
    let want = (w as i32, h as i32);
    assert!(
        client.wait_for(|s| s.windows[a].size == Some(want)),
        "{:?} not {want:?}",
        client.window(a).size
    );

    // A bigger zone moves them further.
    client.set_exclusive_zone(bar, 50);
    let deeper = || slot_rects(&orrery.state(), 0).iter().map(|r| r.1).min() == Some(50);
    assert!(client.wait_for(|_| deeper()), "{:?}", slot_rects(&orrery.state(), 0));

    // Unmapped: the slots as they were, the window back to its old size; mapped again, down once more.
    client.unmap_layer(bar);
    assert!(client.wait_for(|_| slot_rects(&orrery.state(), 0) == before));
    assert!(
        layers(&orrery.state(), 0).is_empty(),
        "an unmapped layer surface is not listed"
    );
    let old = (before[0].2 as i32, before[0].3 as i32);
    assert!(client.wait_for(|s| s.windows[a].size == Some(old)));
    client.remap_layer(bar);
    drawn(&mut client, bar);
    assert!(client.wait_for(|_| deeper()));

    // Destroyed: the room comes back too.
    client.destroy_layer(bar);
    assert!(client.wait_for(|_| slot_rects(&orrery.state(), 0) == before));
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn an_exclusive_overlay_takes_the_keyboard_from_the_window_and_gives_it_back() {
    let (orrery, mut client) = start();
    let a = full_window(&mut client, "a");

    let mut spec = LayerSpec::centred("prompt", Layer::Overlay, (200, 100));
    spec.keyboard = KeyboardInteractivity::Exclusive;
    let o = client.create_layer(&spec);
    drawn(&mut client, o);
    assert!(client.wait_for(|s| s.keyboard_layer == Some(o) && s.keyboard_focus.is_none()));
    assert_eq!(layers(&orrery.state(), 0)[0]["keyboard"], true);

    // Typing reaches it; so does a bind's key, whose action does not run (only quit, reclaim and vt do).
    chord(&orrery, &[A]);
    chord(&orrery, &[SUPER, CTRL, TWO]);
    assert!(
        client.wait_for(|s| s.keys.contains(&(evdev(A), true)) && s.keys.contains(&(evdev(TWO), true))),
        "{:?}",
        client.state.keys
    );
    assert_eq!(layout(&orrery.state()), "wall");

    // A click on the window does not take the keyboard back.
    point(&orrery, 20, 20);
    click(&orrery);
    assert!(client.wait_for(|s| s.pointer_focus == Some(a) && s.buttons.len() == 2));
    assert_eq!(client.state.keyboard_layer, Some(o));

    // The most recent exclusive one wins; when it goes, the other has it again.
    let o2 = client.create_layer(&spec);
    drawn(&mut client, o2);
    assert!(client.wait_for(|s| s.keyboard_layer == Some(o2)));
    client.destroy_layer(o2);
    assert!(client.wait_for(|s| s.keyboard_layer == Some(o)));

    // Unmapped: back to the window.
    client.unmap_layer(o);
    assert!(client.wait_for(|s| s.keyboard_focus == Some(a) && s.keyboard_layer.is_none()));
    chord(&orrery, &[SUPER, CTRL, TWO]);
    assert!(
        client.wait_for(|_| layout(&orrery.state()) == "halves"),
        "binds run again"
    );
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn on_demand_takes_the_keyboard_on_a_click_and_loses_it_when_a_window_is_focused() {
    let (orrery, mut client) = start();
    let a = full_window(&mut client, "a");
    // Bottom-left, over the empty slot under a's.
    let spec = LayerSpec {
        namespace: "dock",
        layer: Layer::Top,
        anchor: Anchor::Bottom | Anchor::Left,
        size: (100, 100),
        exclusive_zone: 0,
        keyboard: KeyboardInteractivity::OnDemand,
        output: None,
    };
    let d = client.create_layer(&spec);
    drawn(&mut client, d);
    assert_eq!(client.state.keyboard_focus, Some(a), "nothing taken by mapping");

    point(&orrery, 50, SIZE.1 - 50);
    click(&orrery);
    assert!(client.wait_for(|s| s.keyboard_layer == Some(d) && s.pointer_layer == Some(d)));
    let state = orrery.state();
    assert_eq!(layers(&state, 0)[0]["keyboard"], true);
    assert_eq!(
        support::focused_slot(&state),
        Some(0),
        "the slot beneath is not focused"
    );

    // Binds still run while it has the keyboard.
    chord(&orrery, &[SUPER, CTRL, TWO]);
    assert!(client.wait_for(|_| layout(&orrery.state()) == "halves"));
    chord(&orrery, &[SUPER, CTRL, ONE]);
    assert!(client.wait_for(|_| layout(&orrery.state()) == "wall"));
    assert!(client.wait_for(|s| s.windows[a].size.is_some_and(|(w, _)| w < SIZE.0 / 2)));
    let (w, h) = client.window(a).size.expect("configured");
    client.attach_buffer(a, w, h);

    // A click on the window takes it back.
    point(&orrery, 20, 20);
    click(&orrery);
    assert!(client.wait_for(|s| s.keyboard_focus == Some(a) && s.keyboard_layer.is_none()));

    // So does focusing a slot by key or request.
    point(&orrery, 50, SIZE.1 - 50);
    click(&orrery);
    assert!(client.wait_for(|s| s.keyboard_layer == Some(d)));
    orrery.msg_ok(&["focus-slot", "right"]);
    assert!(client.wait_for(|s| s.keyboard_layer.is_none()));
    orrery.msg_ok(&["focus-slot", "left"]);
    assert!(client.wait_for(|s| s.keyboard_focus == Some(a)));
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn the_pointer_hits_overlay_then_top_then_windows_then_bottom_then_background() {
    let (orrery, mut client) = start();
    let a = full_window(&mut client, "a");
    let background = client.create_layer(&LayerSpec {
        namespace: "wallpaper",
        layer: Layer::Background,
        anchor: Anchor::all(),
        size: (0, 0),
        exclusive_zone: -1,
        keyboard: KeyboardInteractivity::None,
        output: None,
    });
    // In the middle of the screen, over the empty hero slot.
    let bottom = client.create_layer(&LayerSpec::centred("desk", Layer::Bottom, (100, 100)));
    let corner = |namespace, layer, side| LayerSpec {
        anchor: Anchor::Top | Anchor::Left,
        ..LayerSpec::centred(namespace, layer, (side, side))
    };
    let top = client.create_layer(&corner("note", Layer::Top, 100));
    let overlay = client.create_layer(&corner("toast", Layer::Overlay, 50));
    for l in [background, bottom, top, overlay] {
        drawn(&mut client, l);
    }
    assert_eq!(client.layer(background).size, Some((SIZE.0 as u32, SIZE.1 as u32)));
    assert_eq!(
        slot_rects(&orrery.state(), 0).iter().map(|r| r.1).min(),
        Some(0),
        "no zone, no room taken"
    );

    let under = |client: &mut Client, x, y| {
        point(&orrery, x, y);
        assert!(client.wait_for(|s| s.pointer_focus.is_some() || s.pointer_layer.is_some()));
        (client.state.pointer_focus, client.state.pointer_layer)
    };
    assert_eq!(under(&mut client, 10, 10), (None, Some(overlay)));
    assert_eq!(under(&mut client, 75, 75), (None, Some(top)));
    assert_eq!(under(&mut client, 200, 200), (Some(a), None));
    assert_eq!(under(&mut client, SIZE.0 / 2, SIZE.1 / 2), (None, Some(bottom)));
    assert_eq!(under(&mut client, SIZE.0 - 10, SIZE.1 - 10), (None, Some(background)));

    // A click on a surface that takes no keyboard leaves it where it was; one through an empty slot focuses it.
    point(&orrery, 10, 10);
    click(&orrery);
    assert!(client.wait_for(|s| s.buttons.len() == 2));
    assert_eq!(client.state.keyboard_focus, Some(a));
    assert_eq!(support::focused_slot(&orrery.state()), Some(0));
    point(&orrery, SIZE.0 / 2, SIZE.1 / 2);
    click(&orrery);
    assert!(client.wait_for(|s| s.keyboard_focus.is_none() && s.buttons.len() == 4));
    assert_eq!(support::focused_slot(&orrery.state()), Some(3), "the hero slot");
    let names: Vec<Value> = layers(&orrery.state(), 0).iter().map(|l| l["layer"].clone()).collect();
    assert_eq!(names, ["background", "bottom", "top", "overlay"]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_popup_on_a_layer_surface_is_configured_and_takes_the_pointer() {
    let (orrery, mut client) = start();
    let bar = client.create_layer(&LayerSpec::bar(30));
    drawn(&mut client, bar);
    let p = client.create_layer_popup(bar);
    assert!(client.wait_for(|s| s.popups[p].configures > 0));
    point(&orrery, 5, 5);
    assert!(
        client.wait_for(|s| s.pointer_popup == Some(p)),
        "{:?}",
        client.state.pointer_layer
    );
    point(&orrery, 100, 15);
    assert!(client.wait_for(|s| s.pointer_layer == Some(bar)));
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn layer_surfaces_go_to_their_screen_and_a_removed_screen_closes_them() {
    let (orrery, mut client) = start();
    orrery.msg_ok(&["open-screen"]);
    assert!(client.wait_for(|s| s.globals.iter().filter(|g| *g == "wl_output").count() == 2));

    let mut on_second = LayerSpec::bar(30);
    on_second.output = Some(1);
    let second = client.create_layer(&on_second);
    let mut on_first = LayerSpec::bar(20);
    on_first.output = Some(0);
    let first = client.create_layer(&on_first);
    // No output asked for: the focused screen's.
    orrery.msg_ok(&["workspace", "2"]);
    let mut anywhere = LayerSpec::centred("menu", Layer::Overlay, (50, 50));
    anywhere.anchor = Anchor::Bottom;
    let focused = client.create_layer(&anywhere);
    for l in [second, first, focused] {
        drawn(&mut client, l);
    }
    let state = orrery.state();
    let namespaces = |m| {
        layers(&state, m)
            .iter()
            .map(|l| l["namespace"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(namespaces(0), ["bar"]);
    assert_eq!(namespaces(1), ["bar", "menu"]);
    assert_eq!(slot_rects(&state, 0).iter().map(|r| r.1).min(), Some(20));
    assert_eq!(slot_rects(&state, 1).iter().map(|r| r.1).min(), Some(30));
    // Global coordinates: the second screen sits right of the first.
    assert_eq!(
        rect(&layers(&state, 1)[0]["rect"]),
        (i64::from(SIZE.0), 0, i64::from(SIZE.0), 30)
    );

    orrery.msg_ok(&["close-screen", "headless-2"]);
    assert!(client.wait_for(|s| s.layers[second].closed && s.layers[focused].closed));
    assert!(!client.layer(first).closed);
    assert_eq!(slot_rects(&orrery.state(), 0).iter().map(|r| r.1).min(), Some(20));
    drop(client);
    assert!(orrery.stop().success());
}
