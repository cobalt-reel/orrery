//! Where new windows go, end to end, headless: dialogs onto their parent, launches' windows as new tabs where the
//! launch was made (by pid, through a helper client this test binary runs as, or by the 5 s fallback).

mod support;

use std::time::{Duration, Instant};

use orrery_wm::config;
use serde_json::Value;
use support::client::Client;
use support::{Compositor, TIMEOUT, WATCHDOG, current_tabs, focused_slot, strs, tabs};

const SIZE: (i32, i32) = (1280, 720);

/// Set (to the window's app id) in the helper client's environment; without it `helper_client` does nothing.
const HELPER: &str = "ORRERY_TEST_HELPER";

fn start() -> (Compositor, Client) {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let client = Client::new(orrery.connect());
    (orrery, client)
}

/// Polls `state` until `cond` holds; the last state either way.
fn wait_state(orrery: &Compositor, cond: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = orrery.state();
        if cond(&state) || Instant::now() > deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_dialog_stacks_on_its_parent_wherever_focus_is() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["focus-slot", "right"]);
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped()));
    let d = client.create_dialog("d", "D", a);
    assert!(client.wait_for(|s| s.windows[d].mapped() && !s.windows[a].mapped()));
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a", "d"])]);
    assert_eq!(tabs(&state)[3], [strs(&["b"])]);
    assert_eq!(focused_slot(&state), Some(3), "focus stays where it was");
    assert!(client.window(b).activated() && !client.window(d).activated());

    // A parent in a background tab: the dialog waits on top of it, the shown tab stays.
    orrery.msg_ok(&["focus-slot", "left"]);
    orrery.msg_ok(&["toggle-stacking"]);
    let c = client.create_window("c", "C");
    assert!(client.wait_for(|s| s.windows[c].mapped()));
    let e = client.create_dialog("e", "E", a);
    client.roundtrip();
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a", "d", "e"]), strs(&["c"])]);
    assert_eq!(current_tabs(&state)[0], Some(1));
    assert!(client.window(c).mapped() && !client.window(e).mapped());
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_window_soon_after_a_launch_opens_as_a_tab_where_the_launch_was_made() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    // A launch outlives its program: a launcher that hands off and exits still gets its window placed.
    orrery.msg_ok(&["spawn", "true"]);
    orrery.msg_ok(&["focus-slot", "right"]);
    let w = client.create_window("w", "W");
    assert!(client.wait_for(|s| s.windows[w].mapped() && s.windows[w].activated()));
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["a"]), strs(&["w"])], "a new tab");
    assert_eq!(current_tabs(&state)[0], Some(1));
    assert_eq!(focused_slot(&state), Some(0), "its slot is focused");
    // The launch has its window: the next one is placed by stacking.
    let x = client.create_window("x", "X");
    assert!(client.wait_for(|s| s.windows[x].mapped()));
    assert_eq!(tabs(&orrery.state())[0], [strs(&["a"]), strs(&["w", "x"])]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_launched_programs_window_finds_its_launch_by_pid() {
    let (orrery, mut client) = start();
    // The older launch, in slot 0: the fallback would hand it the helper's window.
    orrery.msg_ok(&["spawn", "true"]);
    orrery.msg_ok(&["focus-slot", "right"]);
    // The helper, from slot 3, as a child of a shell: its window comes from a descendant of the launched pid.
    let exe = std::env::current_exe().expect("the test binary");
    let script = format!("{HELPER}=helper \"$0\" --exact helper_client --quiet & wait");
    let exe = exe.to_str().expect("a UTF-8 path");
    orrery.msg_ok(&["spawn", "sh", "-c", &script, exe]);
    orrery.msg_ok(&["focus-slot", "down"]);
    let state = wait_state(&orrery, |s| tabs(s)[3].concat().contains(&"helper".to_owned()));
    assert_eq!(tabs(&state)[3], [strs(&["helper"])], "{state:#}");
    assert!(tabs(&state)[0].is_empty());
    // The older launch is still unclaimed, and takes the next stranger's window.
    let w = client.create_window("w", "W");
    assert!(client.wait_for(|s| s.windows[w].mapped()));
    let state = orrery.state();
    assert_eq!(tabs(&state)[0], [strs(&["w"])], "{state:#}");
    assert_eq!(focused_slot(&state), Some(0));
    drop(client);
    // Closing orrery ends the helper (its connection goes).
    assert!(orrery.stop().success());
}

/// Not a test of its own: `a_launched_programs_window_finds_its_launch_by_pid` runs this binary through orrery
/// with `HELPER` set, and this maps one window with that app id and stays until orrery goes away.
#[test]
fn helper_client() {
    let Some(app_id) = std::env::var(HELPER).ok() else {
        return;
    };
    let conn = wayland_client::Connection::connect_to_env().expect("orrery's WAYLAND_DISPLAY");
    let mut client = Client::new(conn);
    let w = client.create_window(&app_id, &app_id);
    assert!(client.wait_for(|s| s.windows[w].mapped()));
    // Never outlive the harness's watchdog, whatever happens to orrery.
    let deadline = Instant::now() + WATCHDOG;
    while client.try_roundtrip().is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}
