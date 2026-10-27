//! xdg-activation end to end, headless: a window asking to be seen is focused when it is what its slot shows,
//! else marked for attention (never raised); a launch's token moves a window to where the launch was made.

mod support;

use orrery_wm::config;
use serde_json::Value;
use support::client::Client;
use support::{Compositor, attention, current_tabs, focused_slot, strs, tabs};

const SIZE: (i32, i32) = (1280, 720);

fn start() -> (Compositor, Client) {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let client = Client::new(orrery.connect());
    (orrery, client)
}

/// `state.launches`, oldest first.
fn launches(state: &Value) -> Vec<Value> {
    state["launches"].as_array().expect("launches").clone()
}

#[test]
fn a_buried_window_asking_is_marked_not_raised_until_it_comes_on_top() {
    let (orrery, mut client) = start();
    let [a, b] = [client.create_window("a", "A"), client.create_window("b", "B")];
    assert!(client.wait_for(|s| s.windows[b].mapped() && !s.windows[a].mapped()));
    let token = client.activation_token();
    client.activate(a, &token);
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a", "b"])], "nothing moved");
    assert_eq!(attention(&state)[0], [true]);
    assert!(!client.window(a).mapped() && client.window(b).activated());
    // Uncovered by a close: shown, and the mark is gone for good.
    client.destroy_window(b);
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    assert_eq!(attention(&orrery.state())[0], [false]);

    // In a background tab: the same, until cycle-tab shows it.
    orrery.msg_ok(&["toggle-stacking"]);
    let c = client.create_window("c", "C");
    assert!(client.wait_for(|s| s.windows[c].mapped() && !s.windows[a].mapped()));
    client.activate(a, "a token orrery never made");
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a"]), strs(&["c"])]);
    assert_eq!(current_tabs(&state)[0], Some(1));
    assert_eq!(attention(&state)[0], [true, false]);
    orrery.msg_ok(&["cycle-tab", "next"]);
    assert!(client.wait_for(|s| s.windows[a].mapped() && s.windows[a].activated()));
    assert_eq!(attention(&orrery.state())[0], [false, false]);
    // A shown window's mark does not come back when it goes beneath again.
    orrery.msg_ok(&["cycle-tab", "next"]);
    assert_eq!(attention(&orrery.state())[0], [false, false]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_shown_window_asking_gets_its_slot_focused() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["focus-slot", "right"]);
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].activated() && !s.windows[a].activated()));
    client.activate(a, "unknown");
    assert!(client.wait_for(|s| s.windows[a].activated() && !s.windows[b].activated()));
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(0));
    assert_eq!(tabs(&state)[0], [strs(&["a"])]);
    assert_eq!(attention(&state)[0], [false]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_launch_token_moves_a_window_to_the_launch_as_a_new_tab_once() {
    let (orrery, mut client) = start();
    // Mapped before the spawn, so the 5 s fallback cannot hand it the launch.
    let [v, w] = [client.create_window("v", "V"), client.create_window("w", "W")];
    assert!(client.wait_for(|s| s.windows[w].mapped() && !s.windows[v].mapped()));
    orrery.msg_ok(&["focus-slot", "right"]);
    orrery.msg_ok(&["spawn", "true"]);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(3), "the launch's target");
    let launch = &launches(&state)[0];
    assert_eq!(launch["claimed"], false);
    assert!(launch["pid"].as_u64().is_some_and(|p| p > 0) && launch["age_ms"].as_u64().is_some());
    let token = launch["token"].as_str().expect("a token").to_owned();
    assert!(!token.is_empty());
    orrery.msg_ok(&["focus-slot", "down"]);

    // A client's own token is a plain request: w shows in slot 0, so slot 0 is focused, and nothing moves.
    let own = client.activation_token();
    client.activate(w, &own);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(0));
    assert_eq!(tabs(&state)[0], [strs(&["v", "w"])]);
    assert_eq!(
        launches(&state)[0]["claimed"],
        false,
        "a client's token claims no launch"
    );
    orrery.msg_ok(&["focus-slot", "right"]);
    orrery.msg_ok(&["focus-slot", "down"]);

    client.activate(w, &token);
    assert!(client.wait_for(|s| s.windows[w].activated() && s.windows[v].mapped()));
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["v"])], "{state:#}");
    assert_eq!(tabs(&state)[3], [strs(&["w"])], "{state:#}");
    assert_eq!(focused_slot(&state), Some(3));
    assert_eq!(launches(&state)[0]["claimed"], true);

    // Claimed: a fresh window is no longer sent there, and the token moves nothing again.
    orrery.msg_ok(&["focus-slot", "left"]);
    let x = client.create_window("x", "X");
    assert!(client.wait_for(|s| s.windows[x].mapped()));
    client.activate(v, &token);
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["v", "x"])], "{state:#}");
    assert_eq!(tabs(&state)[3], [strs(&["w"])]);
    assert_eq!(attention(&state)[0], [true], "v, beneath x, asked with a spent token");
    drop(client);
    assert!(orrery.stop().success());
}
