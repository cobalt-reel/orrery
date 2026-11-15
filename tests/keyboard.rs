//! The keyboard: the `keyboard` block's keymap and repeat reach clients, at start and on reload, the block winning
//! over `XKB_DEFAULT_*`; `vt` binds where the backend has no consoles.

mod support;

use orrery_wm::config;
use support::Compositor;
use support::client::Client;
use support::system::wait_log;

const SIZE: (i32, i32) = (1280, 720);

/// xkb keycodes (evdev's plus 8).
const CTRL: u32 = 37;
const ALT: u32 = 64;
const F2: u32 = 68;
const CAPS: u32 = 66;

fn with_keyboard(block: &str) -> String {
    format!("{}\nkeyboard {{\n{block}\n}}\n", config::DEFAULT)
}

/// The keymap size and repeat (rate, delay) a client is first sent.
fn first_keyboard(o: &Compositor) -> (u32, (i32, i32)) {
    let mut c = Client::new(o.connect());
    assert!(
        c.wait_for(|st| !st.keymaps.is_empty() && st.repeat.is_some()),
        "no keymap or repeat info"
    );
    (c.state.keymaps[0], c.state.repeat.expect("repeat info"))
}

#[test]
fn a_reload_brings_the_new_keymap_and_repeat_to_clients() {
    let o = Compositor::start(config::DEFAULT, SIZE);
    let mut c = Client::new(o.connect());
    assert!(c.wait_for(|st| st.keymaps.len() == 1 && st.repeat.is_some()));
    let us = c.state.keymaps[0];
    assert_eq!(c.state.repeat, Some((25, 200)), "the default repeat");

    o.write_config(&with_keyboard("layout \"de\"\nrepeat-delay 600\nrepeat-rate 30"));
    o.msg_ok(&["reload"]);
    assert!(
        c.wait_for(|st| st.keymaps.len() == 2 && st.repeat == Some((30, 600))),
        "{:?} {:?}",
        c.state.keymaps,
        c.state.repeat
    );
    assert_ne!(c.state.keymaps[1], us, "another layout");

    // A reload that leaves the keyboard alone sends no keymap; back to no block is back to us.
    o.write_config(&with_keyboard("layout \"de\"\nrepeat-delay 600\nrepeat-rate 30").replace("#ff7edb", "#00ff00"));
    o.msg_ok(&["reload"]);
    c.roundtrip();
    assert_eq!(c.state.keymaps.len(), 2, "{:?}", c.state.keymaps);
    o.write_config(config::DEFAULT);
    o.msg_ok(&["reload"]);
    assert!(
        c.wait_for(|st| st.keymaps.len() == 3 && st.repeat == Some((25, 200))),
        "{:?} {:?}",
        c.state.keymaps,
        c.state.repeat
    );
    assert_eq!(c.state.keymaps[2], us);
}

#[test]
fn a_reload_keeping_the_layouts_keeps_the_one_in_use() {
    let toggle = "layout \"us,de\"\noptions \"grp:caps_toggle\"";
    let o = Compositor::start_with(&with_keyboard(toggle), SIZE, &["--debug-inject"]);
    let mut c = Client::new(o.connect());
    // A focused window: the client is told the layout in use with wl_keyboard.modifiers.
    let w = c.create_window("typed", "T");
    assert!(c.wait_for(|st| st.keyboard_focus == Some(w) && st.keymaps.len() == 1));
    for state in ["down", "up"] {
        o.msg_ok(&["inject", "key", &CAPS.to_string(), state]);
    }
    assert!(c.wait_for(|st| st.group == 1), "group {}", c.state.group);
    // Other options, the same layouts: still on de.
    o.write_config(&with_keyboard(
        &toggle.replace("grp:caps_toggle", "grp:caps_toggle,compose:ralt"),
    ));
    o.msg_ok(&["reload"]);
    assert!(c.wait_for(|st| st.keymaps.len() == 2), "{:?}", c.state.keymaps);
    c.roundtrip();
    assert_eq!(c.state.group, 1);
    // Other layouts: the first.
    o.write_config(&with_keyboard("layout \"de,us\"\noptions \"grp:caps_toggle\""));
    o.msg_ok(&["reload"]);
    assert!(
        c.wait_for(|st| st.keymaps.len() == 3 && st.group == 0),
        "group {}",
        c.state.group
    );
}

#[test]
fn a_keymap_that_does_not_build_is_refused_and_the_old_one_stays() {
    let o = Compositor::start(config::DEFAULT, SIZE);
    let mut c = Client::new(o.connect());
    assert!(c.wait_for(|st| st.keymaps.len() == 1));
    o.write_config(&with_keyboard("layout \"no-such-layout\""));
    let err = o.msg_err(&["reload"]);
    assert!(err["error"].as_str().is_some_and(|e| e.contains("keymap")), "{err}");
    c.roundtrip();
    assert_eq!(c.state.keymaps.len(), 1);
}

#[test]
fn the_block_wins_over_the_environment_and_unset_names_come_from_it() {
    let plain = Compositor::start(config::DEFAULT, SIZE);
    let (us, _) = first_keyboard(&plain);
    let de_block = Compositor::start(&with_keyboard("layout \"de\""), SIZE);
    let (de, _) = first_keyboard(&de_block);
    assert_ne!(us, de);

    // No block: the environment's layout.
    let env = [("XKB_DEFAULT_LAYOUT", "de")];
    let from_env = Compositor::start_with_env(config::DEFAULT, SIZE, &[], &env);
    assert_eq!(first_keyboard(&from_env).0, de);
    // A block's layout wins; its repeat comes along at start.
    let block = Compositor::start_with_env(&with_keyboard("layout \"us\"\nrepeat-rate 50"), SIZE, &[], &env);
    assert_eq!(first_keyboard(&block), (us, (50, 200)));
}

#[test]
fn vt_without_consoles_is_logged_and_nothing_else() {
    let o = Compositor::start_with(config::DEFAULT, SIZE, &["--debug-inject"]);
    for (key, state) in [
        (CTRL, "down"),
        (ALT, "down"),
        (F2, "down"),
        (F2, "up"),
        (ALT, "up"),
        (CTRL, "up"),
    ] {
        o.msg_ok(&["inject", "key", &key.to_string(), state]);
    }
    wait_log(&o, "no consoles");
    assert_eq!(o.state()["last_key"]["bind"], "Vt(2)");
}
