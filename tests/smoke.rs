//! orrery starts headless, offers the globals clients need, and stops cleanly on SIGTERM.

mod support;

use support::Compositor;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Default)]
struct Globals(Vec<String>);

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            state.0.push(interface);
        }
    }
}

#[test]
fn headless_offers_shell_and_decorations_and_stops_cleanly() {
    let orrery = Compositor::start(orrery_wm::config::DEFAULT, (1280, 720));
    assert!(orrery.socket_path().exists(), "the socket is in our temp runtime dir");

    let conn = orrery.connect();
    let mut queue = conn.new_event_queue();
    let _registry = conn.display().get_registry(&queue.handle(), ());
    let mut globals = Globals::default();
    queue.roundtrip(&mut globals).expect("roundtrip");

    for want in [
        "wl_compositor",
        "wl_shm",
        "xdg_wm_base",
        "zxdg_decoration_manager_v1",
        "wl_output",
        "wl_seat",
    ] {
        assert!(
            globals.0.iter().any(|g| g == want),
            "{want} offered; got {:?}",
            globals.0
        );
    }
    drop(queue);
    drop(conn);

    let status = orrery.stop();
    assert!(status.success(), "clean exit, got {status}");
}
