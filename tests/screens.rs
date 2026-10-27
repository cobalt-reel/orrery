//! Several screens, headless: `open-screen` (or a second `orrery run` on the socket) adds one, `close-screen` takes
//! it away, and workspaces follow i3's rules across them, as a real client sees it.

mod support;

use orrery_wm::config;
use serde_json::Value;
use support::Compositor;
use support::client::Client;

const SIZE: (i32, i32) = (1280, 720);

fn start() -> (Compositor, Client) {
    let orrery = Compositor::start(config::DEFAULT, SIZE);
    let client = Client::new(orrery.connect());
    (orrery, client)
}

/// Each monitor as (name, number, the workspace it shows, focused, its first slot's x), in number order.
fn monitors(state: &Value) -> Vec<(String, u64, u64, bool, i64)> {
    (state["monitors"].as_array().expect("monitors").iter())
        .map(|m| {
            (
                m["name"].as_str().expect("name").to_owned(),
                m["number"].as_u64().expect("number"),
                m["workspace"].as_u64().expect("workspace"),
                m["focused"] == true,
                m["slots"][0]["rect"]["x"].as_i64().expect("x"),
            )
        })
        .collect()
}

/// Every workspace as (number, its monitor, shown, its windows' app ids).
fn workspaces(state: &Value) -> Vec<(u64, String, bool, Vec<String>)> {
    (state["workspaces"].as_array().expect("workspaces").iter())
        .map(|w| {
            let apps = (w["slots"].as_array().expect("slots").iter())
                .flat_map(|s| s["tabs"].as_array().expect("tabs"))
                .flat_map(|t| t["windows"].as_array().expect("windows"))
                .map(|w| w["app_id"].as_str().unwrap_or("?").to_owned())
                .collect();
            let monitor = w["monitor"].as_str().expect("monitor").to_owned();
            (w["number"].as_u64().expect("number"), monitor, w["shown"] == true, apps)
        })
        .collect()
}

fn ws(number: u64, monitor: &str, shown: bool, apps: &[&str]) -> (u64, String, bool, Vec<String>) {
    (number, monitor.to_owned(), shown, support::strs(apps))
}

fn mon(name: &str, number: u64, workspace: u64, focused: bool, x: i32) -> (String, u64, u64, bool, i64) {
    (name.to_owned(), number, workspace, focused, x.into())
}

#[test]
fn a_second_run_on_the_socket_adds_a_screen_and_exits() {
    let (orrery, client) = start();
    let (status, stdout, stderr, runtime) = orrery.run_again();
    assert!(status.success(), "{stdout}{stderr}");
    assert!(stdout.contains(r#""ok":true"#), "{stdout}");
    // It joined: no Wayland socket of its own, and the first one has two screens side by side.
    let leftovers: Vec<_> = std::fs::read_dir(runtime.path()).expect("runtime dir").collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let state = orrery.state();
    assert_eq!(
        monitors(&state),
        [mon("headless", 1, 1, true, 0), mon("headless-2", 2, 2, false, SIZE.0)]
    );
    // Both screens' own workspaces exist (locked), shown.
    assert_eq!(
        workspaces(&state),
        [ws(1, "headless", true, &[]), ws(2, "headless-2", true, &[])]
    );
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn workspaces_are_made_and_moved_across_screens_and_keyboard_focus_follows() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped() && s.windows[a].activated()));
    orrery.msg_ok(&["open-screen"]);

    // Workspace 2 is screen 2's: switching there focuses that screen, and a new window opens there.
    orrery.msg_ok(&["workspace", "2"]);
    assert!(client.wait_for(|s| s.windows[a].mapped() && !s.windows[a].activated()));
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated()));
    assert_eq!(
        workspaces(&orrery.state()),
        [ws(1, "headless", true, &["a"]), ws(2, "headless-2", true, &["b"])]
    );

    // Workspace 3 is new: made on the focused screen (2), shown there; 2 hides, 1 stays shown.
    orrery.msg_ok(&["workspace", "3"]);
    assert!(client.wait_for(|s| !s.windows[b].mapped() && s.windows[a].mapped()));
    let c = client.create_window("c", "C");
    assert!(client.wait_for(|s| s.windows[c].mapped() && s.windows[c].activated()));
    let state = orrery.state();
    assert_eq!(
        monitors(&state),
        [mon("headless", 1, 1, false, 0), mon("headless-2", 2, 3, true, SIZE.0)]
    );
    assert_eq!(
        workspaces(&state),
        [
            ws(1, "headless", true, &["a"]),
            ws(2, "headless-2", false, &["b"]),
            ws(3, "headless-2", true, &["c"]),
        ]
    );

    // From screen 2, c moves to workspace 1 on screen 1: shown there at once, but focus stays on screen 2.
    orrery.msg_ok(&["move-to-workspace", "1"]);
    assert!(client.wait_for(|s| s.windows[c].mapped() && !s.windows[c].activated()));
    let state = orrery.state();
    assert_eq!(state["workspaces"][0]["monitor"], "headless");
    assert_eq!(workspaces(&state)[0], ws(1, "headless", true, &["a", "c"]));
    assert_eq!(
        monitors(&state),
        [mon("headless", 1, 1, false, 0), mon("headless-2", 2, 3, true, SIZE.0)],
        "focus stayed on screen 2"
    );

    // The keyboard follows `workspace N` from screen to screen.
    orrery.msg_ok(&["workspace", "1"]);
    assert!(client.wait_for(|s| s.windows[c].activated() && s.keyboard_focus == Some(c)));
    orrery.msg_ok(&["workspace", "2"]);
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated() && s.keyboard_focus == Some(b)));
    assert!(!client.window(c).activated() && client.window(c).mapped());
    orrery.msg_ok(&["workspace", "1"]);
    assert!(client.wait_for(|s| s.windows[c].activated() && s.keyboard_focus == Some(c) && !s.windows[b].activated()));
    assert!(client.window(b).mapped(), "screen 2 still shows workspace 2");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn a_closed_screens_workspaces_wait_hidden_and_the_next_screen_takes_its_number_back() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["open-screen"]);
    orrery.msg_ok(&["workspace", "2"]);
    let b = client.create_window("b", "B");
    assert!(client.wait_for(|s| s.windows[b].mapped() && s.windows[b].activated()));
    orrery.msg_ok(&["layout", "halves"]);
    orrery.msg_ok(&["workspace", "3"]);
    let c = client.create_window("c", "C");
    assert!(client.wait_for(|s| s.windows[c].mapped() && s.windows[c].activated()));

    // Screen 2 goes while focused: its workspaces go to the first remaining screen, hidden, intact.
    orrery.msg_ok(&["close-screen", "headless-2"]);
    assert!(client.wait_for(|s| !s.windows[b].mapped() && !s.windows[c].mapped() && s.windows[a].mapped()));
    assert!(client.wait_for(|s| s.windows[a].activated()));
    let state = orrery.state();
    assert_eq!(monitors(&state), [mon("headless", 1, 1, true, 0)]);
    assert_eq!(
        workspaces(&state),
        [
            ws(1, "headless", true, &["a"]),
            ws(2, "headless", false, &["b"]),
            ws(3, "headless", false, &["c"]),
        ]
    );
    assert_eq!(state["workspaces"][1]["layout"], "halves");

    // The next screen is number 2 again and takes workspace 2 back, shown; 3 stays where it is.
    orrery.msg_ok(&["open-screen"]);
    assert!(client.wait_for(|s| s.windows[b].mapped() && !s.windows[c].mapped()));
    let state = orrery.state();
    assert_eq!(
        monitors(&state),
        [mon("headless", 1, 1, true, 0), mon("headless-2", 2, 2, false, SIZE.0)]
    );
    assert_eq!(
        workspaces(&state),
        [
            ws(1, "headless", true, &["a"]),
            ws(2, "headless-2", true, &["b"]),
            ws(3, "headless", false, &["c"]),
        ]
    );
    assert_eq!(state["workspaces"][1]["layout"], "halves");
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn screens_sit_in_opening_order_and_close_screen_checks_its_name() {
    let (orrery, client) = start();
    orrery.msg_ok(&["open-screen"]);
    orrery.msg_ok(&["close-screen", "headless"]);
    let err = orrery.msg_err(&["close-screen", "headless"]);
    assert!(err["error"].as_str().is_some_and(|e| e.contains("headless")), "{err}");
    // Screen 1 opened again is named and numbered 1, but sits right of screen 2, which opened before it.
    orrery.msg_ok(&["open-screen"]);
    assert_eq!(
        monitors(&orrery.state()),
        [
            mon("headless", 1, 1, false, 2 * SIZE.0),
            mon("headless-2", 2, 2, true, SIZE.0)
        ]
    );
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn with_no_screen_left_windows_wait_for_the_next_one() {
    let (orrery, mut client) = start();
    let a = client.create_window("a", "A");
    assert!(client.wait_for(|s| s.windows[a].mapped()));
    orrery.msg_ok(&["close-screen", "headless"]);
    assert!(client.wait_for(|s| !s.windows[a].mapped()));
    let state = orrery.state();
    assert_eq!((monitors(&state), workspaces(&state)), (vec![], vec![]));
    orrery.msg_ok(&["open-screen"]);
    assert!(client.wait_for(|s| s.windows[a].mapped() && s.windows[a].activated()));
    assert_eq!(monitors(&orrery.state()), [mon("headless", 1, 1, true, 0)]);
    drop(client);
    assert!(orrery.stop().success());
}

#[test]
fn ten_screens_at_most() {
    let (orrery, client) = start();
    for _ in 2..=10 {
        orrery.msg_ok(&["open-screen"]);
    }
    let err = orrery.msg_err(&["open-screen"]);
    assert!(err["error"].as_str().is_some_and(|e| e.contains("1 to 10")), "{err}");
    let state = orrery.state();
    assert_eq!(state["monitors"][9]["name"], "headless-10");
    assert_eq!(state["monitors"][9]["slots"][0]["rect"]["x"], 9 * SIZE.0);
    drop(client);
    assert!(orrery.stop().success());
}
