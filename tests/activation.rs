//! xdg-activation end to end, headless: a window asking to be seen is never raised; it is focused only when it is
//! what its slot shows and its token comes from input to the focused client, else marked for attention; a launch's
//! token moves a window to where the launch was made.

mod support;

use orrery_wm::config;
use serde_json::Value;
use support::client::{Client, LayerSpec};
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

/// Window `a` alone in slot 0 and `b` alone in the focused slot 3, the client's keyboard on `b`.
fn two_slots(orrery: &Compositor, client: &mut Client) -> (usize, usize) {
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["focus-slot", "right"]);
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].activated() && s.keyboard_focus == Some(b)));
    (a, b)
}

#[test]
fn a_buried_window_asking_is_marked_not_raised_until_it_comes_on_top() {
    let (orrery, mut client) = start();
    let [a, b] = [client.create_window("a", "A"), client.create_window("b", "B")];
    assert!(client.wait_for(|s| s.windows[b].mapped() && !s.windows[a].mapped()));
    // Even a token from the focused client's input raises nothing.
    let serial = client.state.enter_serial;
    assert!(serial.is_some());
    let token = client.activation_token(serial);
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
fn a_shown_window_asking_without_input_behind_it_is_only_marked() {
    let (orrery, mut client) = start();
    let (a, b) = two_slots(&orrery, &mut client);
    // An unknown token, then a token of the client's own but with no serial: no focus taken, a mark each time.
    client.activate(a, "garbage");
    let unbacked = client.activation_token(None);
    client.activate(a, &unbacked);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(3));
    assert_eq!(attention(&state)[0], [true]);
    assert!(client.window(b).activated() && !client.window(a).activated() && client.window(a).mapped());
    // Its slot taking focus clears the mark.
    orrery.msg_ok(&["focus-slot", "left"]);
    assert!(client.wait_for(|s| s.windows[a].activated()));
    assert_eq!(attention(&orrery.state())[0], [false]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_token_from_the_focused_clients_input_focuses_a_shown_window_once() {
    let (orrery, mut client) = start();
    let (a, b) = two_slots(&orrery, &mut client);
    let first_enter = client.state.enter_serial.expect("an enter serial");
    let token = client.activation_token(Some(first_enter));
    client.activate(a, &token);
    assert!(client.wait_for(|s| s.windows[a].activated() && !s.windows[b].activated()));
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(0));
    assert_eq!(attention(&state)[0], [false]);

    // Back to b (a new enter serial): the used token is spent, and one with the serial of an earlier focus is stale.
    orrery.msg_ok(&["focus-slot", "right"]);
    assert!(client.wait_for(|s| s.keyboard_focus == Some(b) && s.enter_serial != Some(first_enter)));
    client.activate(a, &token);
    let stale = client.activation_token(Some(first_enter));
    client.activate(a, &stale);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(3));
    assert_eq!(attention(&state)[0], [true]);

    // A client without keyboard focus cannot back a token, even with the focused client's serial.
    let serial = client.state.enter_serial;
    let mut other = Client::new(orrery.connect());
    let borrowed = other.activation_token(serial);
    client.activate(a, &borrowed);
    assert_eq!(focused_slot(&orrery.state()), Some(3));

    // A fresh one from the focused client works again.
    let fresh = client.activation_token(serial);
    client.activate(a, &fresh);
    assert!(client.wait_for(|s| s.windows[a].activated()));
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(0));
    assert_eq!(attention(&state)[0], [false]);
    drop(other);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_popup_asks_for_its_toplevel() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    let p = client.create_popup(a);
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped() && !s.windows[a].mapped()));
    client.activate_popup(p, "garbage");
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a", "b"])]);
    assert_eq!(attention(&state)[0], [true], "{state:#}");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_layer_surface_with_a_launch_token_uses_the_launch_up() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["spawn", "true"]);
    let token = launches(&orrery.state())[0]["token"]
        .as_str()
        .expect("a token")
        .to_owned();
    // A panel (the launch's program, say) activating with the token: nothing to move, but the launch is spent.
    let bar = client.create_layer(&LayerSpec::bar(20));
    assert!(client.wait_for(|s| s.layers[bar].attached.is_some()));
    client.activate_layer(bar, &token);
    let state = orrery.state();
    assert_eq!(launches(&state)[0]["claimed"], true, "{state:#}");
    // So the next stranger's window stacks instead of opening as the launch's tab.
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped()));
    assert_eq!(tabs(&orrery.state())[0], [strs(&["a", "b"])]);
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
    // From here to the activation with the launch's token must take under 5 s (LAUNCH_WAIT), or the launch is
    // forgotten; these few requests take milliseconds.
    orrery.msg_ok(&["spawn", "true"]);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(3), "the launch's target");
    let launch = &launches(&state)[0];
    assert_eq!(launch["claimed"], false);
    assert!(launch["pid"].as_u64().is_some_and(|p| p > 0) && launch["age_ms"].as_u64().is_some());
    let token = launch["token"].as_str().expect("a token").to_owned();
    assert!(!token.is_empty());
    orrery.msg_ok(&["focus-slot", "down"]);

    // A client's own token is never a launch's: w, shown in slot 0 without focus, is only marked.
    let own = client.activation_token(None);
    client.activate(w, &own);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(4));
    assert_eq!(tabs(&state)[0], [strs(&["v", "w"])]);
    assert_eq!(attention(&state)[0], [true]);
    assert_eq!(
        launches(&state)[0]["claimed"],
        false,
        "a client's token claims no launch"
    );

    client.activate(w, &token);
    assert!(client.wait_for(|s| s.windows[w].activated() && s.windows[v].mapped()));
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["v"])], "{state:#}");
    assert_eq!(tabs(&state)[3], [strs(&["w"])], "{state:#}");
    assert_eq!(focused_slot(&state), Some(3));
    assert_eq!(attention(&state)[3], [false], "shown and focused");
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

#[test]
fn at_most_64_client_tokens_are_kept() {
    let (orrery, mut client) = start();
    let (a, _) = two_slots(&orrery, &mut client);
    let serial = client.state.enter_serial;
    let tokens: Vec<String> = (0..65).map(|_| client.activation_token(serial)).collect();
    // The 65th was answered but not kept: it only marks.
    client.activate(a, &tokens[64]);
    let state = orrery.state();
    assert_eq!(focused_slot(&state), Some(3));
    assert_eq!(attention(&state)[0], [true]);
    // The first is still good.
    client.activate(a, &tokens[0]);
    assert!(client.wait_for(|s| s.windows[a].activated()));
    assert_eq!(focused_slot(&orrery.state()), Some(0));
    drop(client);
    assert!(orrery.stop().success());
}
