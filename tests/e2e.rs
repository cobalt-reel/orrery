//! End to end, headless only: a real orrery, a real Wayland client, and what the client is told.

mod support;

use orrery_wm::layout::Rect;
use orrery_wm::{NAME, config};
use support::Compositor;
use support::client::{Client, DecorationMode};

const SIZE: (i32, i32) = (1280, 720);

/// The slot rectangles orrery must use for `layout` on the headless output, from the same pure core.
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

fn size_of(r: Rect) -> Option<(i32, i32)> {
    Some((r.w, r.h))
}

fn start() -> (Compositor, Client) {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let client = Client::new(orrery.connect());
    (orrery, client)
}

/// Three windows open into the focused slot (slot 0 of `wall`, the headless output's first layout): all are
/// configured to that slot, tiled, server-side decorated; only the last is on top (mapped, activated).
fn open_three(client: &mut Client) -> [usize; 3] {
    let a = client.create_window("a", "A");
    let b = client.create_window("b", "B");
    let c = client.create_window("c", "C");
    let slot = size_of(rects("wall")[0]);
    let ok = client.wait_for(|s| {
        s.windows.iter().all(|w| w.size == slot && w.tiled())
            && s.windows[c].mapped()
            && !s.windows[a].mapped()
            && !s.windows[b].mapped()
    });
    assert!(ok, "three windows in slot 0, top = c: {:#?}", client.state.windows);
    [a, b, c]
}

#[test]
fn three_windows_stack_in_the_focused_slot() {
    let (orrery, mut client) = start();
    let [a, b, c] = open_three(&mut client);
    assert!(client.window(c).activated(), "the top is focused");
    assert!(!client.window(a).activated() && !client.window(b).activated());
    for i in [a, b, c] {
        assert_eq!(
            client.window(i).decoration_mode,
            Some(DecorationMode::ServerSide),
            "asked for client-side, told server-side"
        );
    }
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn three_windows_state_over_the_socket() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    let out = orrery.msg(&["state"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let state: serde_json::Value = serde_json::from_slice(&out.stdout).expect("state is JSON");
    let slot0 = &state["state"]["monitors"][0]["slots"][0];
    assert_eq!(slot0["tabs"].as_array().map(Vec::len), Some(1), "one tab: {slot0:#}");
    assert_eq!(slot0["tabs"][0]["current"], true);
    let windows: Vec<&str> = slot0["tabs"][0]["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .filter_map(|w| w["app_id"].as_str())
        .collect();
    assert_eq!(windows, ["a", "b", "c"], "bottom to top");
    assert_eq!(slot0["rect"]["w"], rects("wall")[0].w);
    assert_eq!(slot0["rect"]["h"], rects("wall")[0].h);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn closing_the_top_shows_the_next() {
    let (orrery, mut client) = start();
    let [a, b, c] = open_three(&mut client);
    client.destroy_window(c);
    let ok = client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated() && !s.windows[a].mapped());
    assert!(ok, "b is on top after c closed: {:#?}", client.state.windows);
    assert_eq!(client.window(b).size, size_of(rects("wall")[0]));
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_disconnected_client_leaves_orrery_running() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    drop(client);
    // A second client still gets windows, on top of the now-empty slot.
    let mut second = Client::new(orrery.connect());
    let w = second.create_window("d", "D");
    let slot = size_of(rects("wall")[0]);
    assert!(second.wait_for(|s| s.windows[w].mapped() && s.windows[w].activated() && s.windows[w].size == slot));
    drop(second);
    assert!(orrery.stop().success());
}

#[test]
fn switching_layout_reconfigures_to_the_new_rects() {
    let (orrery, mut client) = start();
    let [a, b, c] = open_three(&mut client);
    let out = orrery.msg(&["layout", "halves"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // wall slot 0 becomes halves slot 0: the whole stack is reconfigured to the left half.
    let half = size_of(rects("halves")[0]);
    let ok = client.wait_for(|s| [a, b, c].iter().all(|&i| s.windows[i].size == half));
    assert!(ok, "sizes follow the new layout: {:#?}", client.state.windows);
    assert!(client.window(c).mapped() && client.window(c).activated());
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_toplevel_without_a_buffer_neither_hides_nor_steals_focus() {
    let (orrery, mut client) = start();
    let slot = size_of(rects("wall")[0]);
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped() && s.windows[a].activated()));

    let b = client.create_unmapped_window("b", "B");
    // Configured to the slot it would open into, but not opened yet.
    assert!(client.wait_for(|s| s.windows[b].configures > 0 && s.windows[b].size == slot));
    assert!(!client.window(b).activated());
    // Several frames later a is still on top and focused.
    std::thread::sleep(std::time::Duration::from_millis(100));
    client.roundtrip();
    assert!(
        client.window(a).mapped() && client.window(a).activated(),
        "{:#?}",
        client.state.windows
    );

    // Its first buffer opens it on top.
    client.attach_buffer(b, 1, 1);
    let ok = client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated() && !s.windows[a].mapped());
    assert!(ok, "b opens on its first buffer: {:#?}", client.state.windows);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_null_buffer_unmaps_and_reveals_the_one_beneath() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped() && !s.windows[a].mapped()));

    client.unmap(b);
    let ok = client.wait_for(|s| s.windows[a].mapped() && s.windows[a].activated() && !s.windows[b].mapped());
    assert!(ok, "a shows once b unmaps: {:#?}", client.state.windows);

    // Mapping again goes through the initial configure and back on top.
    client.remap(b);
    let ok = client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated() && !s.windows[a].mapped());
    assert!(ok, "b back on top after remapping: {:#?}", client.state.windows);
    drop(client);
    assert!(orrery.stop().success());
}

/// A fresh client still gets a window mapped and focused.
fn assert_serves(orrery: &Compositor) {
    let mut client = Client::new(orrery.connect());
    let w = client.create_window("probe", "probe");
    let slot = size_of(rects("wall")[0]);
    let ok = client.wait_for(|s| s.windows[w].mapped() && s.windows[w].activated() && s.windows[w].size == slot);
    assert!(ok, "{NAME} still serves clients: {:#?}", client.state.windows);
}

#[test]
fn misbehaving_clients_do_not_take_orrery_down() {
    let orrery = Compositor::start(config::DEFAULT, SIZE);

    // A buffer attached before the first configure is acked.
    let mut c = Client::new(orrery.connect());
    c.create_window_attaching_early("early");
    drop(c);
    assert_serves(&orrery);

    // The wl_surface destroyed before its xdg_toplevel.
    let mut c = Client::new(orrery.connect());
    let w = c.create_window("surface-first", "x");
    assert!(c.wait_for(|s| s.windows[w].mapped()));
    c.destroy_surface_first(w);
    drop(c);
    assert_serves(&orrery);

    // Survival check: a buffer far larger than the slot (still configured to the slot; drawing is clipped to
    // the slot, see render::scene).
    let mut c = Client::new(orrery.connect());
    let w = c.create_window("huge", "x");
    assert!(c.wait_for(|s| s.windows[w].mapped()));
    c.attach_buffer(w, 4000, 3000);
    c.roundtrip();
    assert_eq!(c.window(w).size, size_of(rects("wall")[0]));
    drop(c);
    assert_serves(&orrery);

    // A popup whose parent is hidden beneath another window.
    let mut c = Client::new(orrery.connect());
    let hidden = c.create_window("hidden", "x");
    let top = c.create_window("top", "x");
    assert!(c.wait_for(|s| s.windows[top].mapped() && !s.windows[hidden].mapped()));
    let p = c.create_popup(hidden);
    assert!(
        c.wait_for(|s| s.popups[p].configures > 0),
        "the hidden parent's popup is configured"
    );
    drop(c);
    assert_serves(&orrery);

    // A disconnect with several windows open.
    let mut c = Client::new(orrery.connect());
    for name in ["one", "two", "three"] {
        c.create_window(name, name);
    }
    c.roundtrip();
    drop(c);
    assert_serves(&orrery);

    assert!(orrery.stop().success());
}

/// The default config with `wall` redefined as `grid` (cols, rows) of 1×1 slots.
fn with_wall(cols: u32, rows: u32) -> String {
    let mut slots = String::new();
    for row in 0..rows {
        for col in 0..cols {
            slots.push_str(&format!("    slot col={col} row={row}\n"));
        }
    }
    let start = config::DEFAULT.find("layout \"wall\"").expect("wall");
    let end = start + config::DEFAULT[start..].find("}\n").expect("wall ends") + 2;
    format!(
        "{}layout \"wall\" {{\n    grid cols={cols} rows={rows}\n{slots}}}\n{}",
        &config::DEFAULT[..start],
        &config::DEFAULT[end..]
    )
}

/// Polls `state` until `cond` holds (the config watcher looks once a second).
fn wait_state(orrery: &Compositor, mut cond: impl FnMut(&serde_json::Value) -> bool) -> serde_json::Value {
    let deadline = std::time::Instant::now() + support::TIMEOUT;
    loop {
        let state = orrery.state();
        if cond(&state) {
            return state;
        }
        assert!(std::time::Instant::now() < deadline, "state never got there: {state:#}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn slot_count(state: &serde_json::Value) -> usize {
    state["monitors"][0]["slots"].as_array().map_or(0, Vec::len)
}

/// Windows in every slot, any tab.
fn window_count(state: &serde_json::Value) -> usize {
    let len = |v: &serde_json::Value| v.as_array().map_or(0, Vec::len);
    state["monitors"][0]["slots"].as_array().map_or(0, |slots| {
        slots
            .iter()
            .flat_map(|s| s["tabs"].as_array().into_iter().flatten())
            .map(|t| len(&t["windows"]))
            .sum()
    })
}

#[test]
fn an_edited_config_is_picked_up_and_a_broken_one_keeps_the_old() {
    let (orrery, mut client) = start();
    open_three(&mut client);
    assert_eq!(slot_count(&orrery.state()), 6);

    // A good edit: wall becomes 2×2; the windows stay, resized to the new slot 0.
    orrery.write_config(&with_wall(2, 2));
    let state = wait_state(&orrery, |s| slot_count(s) == 4);
    assert!(state["config_error"].is_null(), "{state:#}");
    assert_eq!(state["monitors"][0]["layout"], "wall");
    assert_eq!(window_count(&state), 3);
    let quarter = Some((SIZE.0 / 2, SIZE.1 / 2));
    assert!(client.wait_for(|s| s.windows.iter().all(|w| w.size == quarter)));

    // A broken edit: the error shows, the 2×2 wall stays.
    orrery.write_config("layout \"wall\" {\n    grid cols=0 rows=1\n");
    let state = wait_state(&orrery, |s| !s["config_error"].is_null());
    let error = state["config_error"].as_str().expect("an error string");
    assert!(error.contains("config.kdl"), "{error}");
    assert_eq!(slot_count(&state), 4, "the old config still rules");

    // Fixed: the error clears and the change applies.
    orrery.write_config(&with_wall(3, 1));
    let state = wait_state(&orrery, |s| s["config_error"].is_null());
    assert_eq!(slot_count(&state), 3);
    assert_eq!(window_count(&state), 3, "no window lost on the way");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_monitor_whose_layout_vanished_keeps_it() {
    let (orrery, client) = start();
    // Only halves left (and no bind naming wall).
    let start = config::DEFAULT.find("layout \"wall\"").expect("wall");
    let end = start + config::DEFAULT[start..].find("}\n").expect("wall ends") + 2;
    let src = format!("{}{}", &config::DEFAULT[..start], &config::DEFAULT[end..])
        .replace("monitor \"winit\" layout=\"wall\"", "")
        .replace("\"Mod+1\" layout \"wall\"", "");
    config::parse(&src).expect("the edited config is good");
    orrery.write_config(&src);
    let reload = orrery.msg_ok(&["reload"]);
    assert_eq!(reload["ok"], true);
    let state = orrery.state();
    assert!(state["config_error"].is_null(), "not an error: {state:#}");
    assert_eq!(state["monitors"][0]["layout"], "wall");
    assert_eq!(slot_count(&state), 6);
    // The new config's layouts are what `layout` knows now.
    orrery.msg_ok(&["layout", "halves"]);
    let err = orrery.msg_err(&["layout", "wall"]);
    assert!(err["error"].as_str().is_some_and(|e| e.contains("wall")), "{err}");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_config_file_removed_while_running_keeps_the_current_config() {
    let (orrery, client) = start();
    orrery.write_config(&with_wall(2, 1));
    wait_state(&orrery, |s| slot_count(s) == 2);
    std::fs::remove_file(orrery.config_path()).expect("remove the config");
    // Once the watcher has seen it gone: still the 2×1 wall, not the built-in 6-slot one, and not an error.
    let deadline = std::time::Instant::now() + support::TIMEOUT;
    while !orrery.log().contains("is gone") {
        assert!(
            std::time::Instant::now() < deadline,
            "the watcher never noticed: {}",
            orrery.log()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = orrery.state();
    assert_eq!(slot_count(&state), 2, "{state:#}");
    assert!(state["config_error"].is_null(), "{state:#}");
    let err = orrery.msg_err(&["reload"]);
    assert!(err["error"].as_str().is_some_and(|e| e.contains("gone")), "{err}");
    assert!(orrery.state()["config_error"].is_null(), "a missing file is no red bar");
    // Back again: picked up.
    orrery.write_config(&with_wall(3, 1));
    wait_state(&orrery, |s| slot_count(s) == 3);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn the_first_good_config_after_a_broken_start_applies_monitor_rules() {
    let orrery = Compositor::start("bogus 1\n", SIZE);
    let state = orrery.state();
    assert!(state["config_error"].is_string(), "{state:#}");
    assert_eq!(
        state["monitors"][0]["layout"], "wall",
        "the built-in config's first layout"
    );
    let rule = "monitor \"headless\" layout=\"halves\"\n";
    orrery.write_config(&format!("{}{rule}", config::DEFAULT));
    let state = wait_state(&orrery, |s| s["config_error"].is_null());
    assert_eq!(
        state["monitors"][0]["layout"], "halves",
        "the rule for this output applies"
    );
    // Later reloads keep whatever the monitor shows.
    orrery.msg_ok(&["layout", "wall"]);
    orrery.write_config(&format!("{}{rule}\n", config::DEFAULT));
    orrery.msg_ok(&["reload"]);
    assert_eq!(orrery.state()["monitors"][0]["layout"], "wall");
    assert!(orrery.stop().success());
}
