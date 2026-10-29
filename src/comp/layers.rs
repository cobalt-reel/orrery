//! Layer-shell (`zwlr_layer_shell_v1`): bars, docks, wallpapers, drop-downs and launchers on a screen's four layers,
//! two under the slots (background, bottom) and two over them (top, overlay). Each screen's layer surfaces live in
//! Smithay's `LayerMap` for its output, which places them as the protocol says; what their exclusive zones leave is
//! the area the screen's layout divides (`Stacks::set_area`). A layer surface is in its map from its first commit
//! (so the initial configure carries its size) until it unmaps (a null buffer) or goes; it is drawn, hit and given
//! the keyboard only while mapped (it has a buffer). Its zone counts from that first commit: one that never draws
//! keeps it (as other compositors do; the map must place it to size the configure). A buffer before the configure
//! is acked, on the first commit or the first after an unmap, is a protocol error.
//!
//! Zones are Smithay's arithmetic, which differs from the protocol's letter in corner cases: a positive zone on a
//! surface anchored to a corner, or to the top and bottom edges only (left and right only), still takes room along
//! one edge, and one anchored to all four edges takes the whole screen, which `usable_area` then ignores.
//!
//! Order is the map's, as drawn (later in front on each layer): popups of bottom and background surfaces are drawn
//! and hit over the windows, under top and overlay surfaces. The keyboard: an `exclusive` surface on the top or
//! overlay layer takes it while mapped (the frontmost one, on the focused screen first) and gives it back to the
//! focused window when it goes; while it has it, binds do not run, except quit, reclaim and vt
//! (`runs_over_exclusive`). An `on_demand` surface takes it on a click and loses it when a window (or slot) is
//! focused; `exclusive` on the bottom or background layer does the same, as the protocol allows. `none` never takes
//! it. Layer surfaces never enter the stacks.

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::{LayerMap, LayerSurface, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive as _, Logical, Point, Rectangle};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
};

use super::Orrery;
use crate::config::Action;
use crate::ipc::{LayerName, LayerView};
use crate::layout::Rect;
use crate::stacks::WinId;

/// The layers over the slots, front to back.
pub const ABOVE: [Layer; 2] = [Layer::Overlay, Layer::Top];
/// The layers under the slots, front to back.
pub const BELOW: [Layer; 2] = [Layer::Bottom, Layer::Background];

/// A layer surface orrery knows (its order is not the drawing order: that is its map's).
#[derive(Debug)]
pub struct Layered {
    pub surface: LayerSurface,
    /// The output (screen) it lives on, by name.
    pub output: String,
    /// It has a buffer: drawn, hit, and able to take the keyboard.
    pub mapped: bool,
}

/// Where the stacks' focus is (monitor, its shown workspace, that workspace's focused slot, the focused window): a
/// click gives an `on_demand` layer surface the keyboard until this moves.
pub type FocusKey = (usize, Option<u8>, Option<usize>, Option<WinId>);

/// The area a screen's layout divides: the screen less the layer surfaces' exclusive zones (`zone`, output-local, as
/// `LayerMap::non_exclusive_zone` gives it). Zones that leave nothing are ignored: the whole screen.
#[must_use]
pub fn usable_area(screen: Rect, zone: Rectangle<i32, Logical>) -> Rect {
    if zone.size.w <= 0 || zone.size.h <= 0 {
        return screen;
    }
    Rect {
        x: screen.x + zone.loc.x,
        y: screen.y + zone.loc.y,
        w: zone.size.w,
        h: zone.size.h,
    }
}

/// The binds that still run while an `exclusive` layer surface has the keyboard: quitting, the escape key and the
/// consoles, so a client grabbing the keyboard can never lock anyone out.
#[must_use]
pub fn runs_over_exclusive(action: &Action) -> bool {
    matches!(action, Action::Quit | Action::Reclaim | Action::Vt(_))
}

/// A layer as `state` names it.
#[must_use]
pub fn layer_name(layer: Layer) -> LayerName {
    match layer {
        Layer::Background => LayerName::Background,
        Layer::Bottom => LayerName::Bottom,
        Layer::Top => LayerName::Top,
        Layer::Overlay => LayerName::Overlay,
    }
}

/// Where a layer surface's own surface starts in `map`, output-local (its bounding box may reach further up-left, by
/// its subsurfaces); None when it is not in the map.
pub fn layer_origin(map: &LayerMap, layer: &LayerSurface) -> Option<Point<i32, Logical>> {
    Some(map.layer_geometry(layer)?.loc - layer.bbox().loc)
}

fn rect_of(r: Rectangle<i32, Logical>) -> Rect {
    Rect {
        x: r.loc.x,
        y: r.loc.y,
        w: r.size.w,
        h: r.size.h,
    }
}

impl Orrery {
    /// A new layer surface: on the screen it asks for (its wl_output), else the focused one. One asking for a screen
    /// that is gone, or coming when there is none, is closed at once. It is placed on its first commit.
    pub fn new_layer(&mut self, surface: WlrLayerSurface, output: Option<WlOutput>, namespace: String) {
        let screen = match output {
            Some(wl) => Output::from_resource(&wl).map(|o| o.name()),
            None => (self.stacks.monitors().get(self.stacks.focused_monitor()))
                .map(|m| m.name().to_owned())
                .or_else(|| self.outputs.first().map(Output::name)),
        };
        let Some(output) = screen.filter(|name| self.output_named(name).is_some()) else {
            surface.send_close();
            return;
        };
        self.layers.push(Layered {
            surface: LayerSurface::new(surface, namespace),
            output,
            mapped: false,
        });
    }

    pub(super) fn output_named(&self, name: &str) -> Option<Output> {
        self.outputs.iter().find(|o| o.name() == name).cloned()
    }

    /// A layer surface's own surface committed. The first commit (or the first after an unmap) puts it in its
    /// screen's map (in front on its layer) and is answered with a configure carrying its size; a buffer maps it, but
    /// one before that configure is acked is a protocol error; a null buffer unmaps it: out of the map, and back to
    /// waiting for a first commit. Every commit arranges the screen's layer surfaces again (size, anchors, margins,
    /// zone); a changed usable area or mapping arranges the windows, anything else at least the focus.
    pub fn layer_committed(&mut self, surface: &WlSurface) {
        let Some(i) = self.layers.iter().position(|l| l.surface.wl_surface() == surface) else {
            return;
        };
        let Some(output) = self.output_named(&self.layers[i].output) else {
            return;
        };
        let has_buffer = with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);
        let layer = self.layers[i].surface.clone();
        // Posts the error (the client is disconnected, and the surface forgotten when it goes).
        if has_buffer && !layer.layer_surface().ensure_configured() {
            return;
        }
        let was_mapped = self.layers[i].mapped;
        {
            let mut map = layer_map_for_output(&output);
            if was_mapped && !has_buffer {
                map.unmap_layer(&layer);
                // The client starts over (a commit without a buffer, then the configure to ack before drawing): its
                // next commit is answered as a first one, and a buffer before that ack is refused.
                with_states(surface, |states| {
                    if let Some(data) = states.data_map.get::<LayerSurfaceData>() {
                        let mut data = data.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        data.initial_configure_sent = false;
                        data.configured = false;
                    }
                });
            } else {
                // Placed now if new (`layers_arranged` below arranges it again with the rest otherwise).
                if map.layer_geometry(&layer).is_none()
                    && let Err(e) = map.map_layer(&layer)
                {
                    tracing::warn!("cannot place a layer surface: {e}");
                }
                let configured = with_states(surface, |states| {
                    (states.data_map.get::<LayerSurfaceData>()).is_some_and(|d| {
                        d.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .initial_configure_sent
                    })
                });
                if !configured {
                    layer.layer_surface().send_configure();
                }
            }
        }
        self.layers[i].mapped = has_buffer;
        if self.layers_arranged(&output) || has_buffer != was_mapped {
            self.arrange();
        } else {
            // It may have moved, grown or changed its keyboard interactivity.
            self.update_keyboard_focus();
            self.update_pointer_focus();
        }
    }

    /// A layer surface was destroyed (or its client went): out of its map, its zone given back.
    pub fn forget_layer(&mut self, surface: &WlSurface) {
        let Some(i) = self.layers.iter().position(|l| l.surface.wl_surface() == surface) else {
            return;
        };
        let record = self.layers.remove(i);
        if let Some(output) = self.output_named(&record.output) {
            layer_map_for_output(&output).unmap_layer(&record.surface);
            self.layers_arranged(&output);
        }
        self.arrange();
    }

    /// Forgets layer surfaces whose client went without destroying them; true when there were any (the caller
    /// arranges).
    pub(super) fn forget_dead_layers(&mut self) -> bool {
        if self.layers.iter().all(|l| l.surface.alive()) {
            return false;
        }
        self.layers.retain(|l| l.surface.alive());
        for output in self.outputs.clone() {
            layer_map_for_output(&output).cleanup();
            self.layers_arranged(&output);
        }
        true
    }

    /// A screen going away: its layer surfaces are told (`closed`) and forgotten; the client may make new ones
    /// elsewhere.
    pub(super) fn close_layers(&mut self, output: &Output) {
        let name = output.name();
        {
            let mut map = layer_map_for_output(output);
            for l in self.layers.iter().filter(|l| l.output == name) {
                l.surface.layer_surface().send_close();
                map.unmap_layer(&l.surface);
            }
        }
        self.layers.retain(|l| l.output != name);
    }

    /// The area `output`'s layout divides when the screen is `screen` (global): its layer surfaces arranged again
    /// (Smithay's `LayerMap::arrange`, which also reconfigures them to new sizes), less their exclusive zones.
    pub(super) fn usable_area_of(&self, output: &Output, screen: Rect) -> Rect {
        let mut map = layer_map_for_output(output);
        map.arrange();
        usable_area(screen, map.non_exclusive_zone())
    }

    /// Arranges `output`'s layer surfaces again and gives its monitor what their zones leave; true when that changed
    /// (the caller arranges the windows).
    pub(super) fn layers_arranged(&mut self, output: &Output) -> bool {
        let Some(screen) = self.space.output_geometry(output).map(rect_of) else {
            return false;
        };
        let area = self.usable_area_of(output, screen);
        let name = output.name();
        let old = (self.stacks.monitors().iter())
            .find(|m| m.name() == name)
            .map(|m| m.area());
        if old.is_none_or(|old| old == area) {
            return false;
        }
        self.stacks.set_area(&name, area);
        true
    }

    /// Where the stacks' focus is now (see `FocusKey`).
    pub(super) fn focus_key(&self) -> FocusKey {
        let mi = self.stacks.focused_monitor();
        let ws = self.stacks.shown(mi);
        (
            mi,
            ws.map(crate::stacks::Workspace::number),
            ws.map(crate::stacks::Workspace::focus),
            self.stacks.focused_window(),
        )
    }

    /// A click on layer surface `layer`: the keyboard is its until the stacks' focus moves, if it takes it on demand
    /// (`exclusive` ones over the slots have it already).
    pub(super) fn layer_clicked(&mut self, layer: &LayerSurface) {
        if layer.can_receive_keyboard_focus() {
            self.layer_focus = Some((layer.wl_surface().clone(), self.focus_key()));
        }
    }

    fn is_mapped(&self, layer: &LayerSurface) -> bool {
        self.layers.iter().any(|l| l.mapped && l.surface == *layer)
    }

    /// The frontmost mapped `exclusive` layer surface over the slots, as drawn: the focused screen's first, then the
    /// others in opening order.
    fn exclusive_layer(&self) -> Option<WlSurface> {
        let focused = (self.stacks.monitors().get(self.stacks.focused_monitor())).map(|m| m.name().to_owned());
        let mut outputs: Vec<&Output> = self.outputs.iter().collect();
        outputs.sort_by_key(|o| Some(o.name()) != focused);
        outputs.into_iter().find_map(|o| {
            let map = layer_map_for_output(o);
            (ABOVE.iter())
                .flat_map(|&layer| map.layers_on(layer).rev())
                .find(|l| {
                    l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive && self.is_mapped(l)
                })
                .map(|l| l.wl_surface().clone())
        })
    }

    /// The layer surface that should have the keyboard, and whether it holds it exclusively: `exclusive_layer`, else
    /// the one a click gave it while the stacks' focus has not moved since and it still takes the keyboard
    /// (forgotten otherwise).
    pub(super) fn layer_keyboard(&mut self) -> Option<(WlSurface, bool)> {
        if let Some(surface) = self.exclusive_layer() {
            return Some((surface, true));
        }
        let key = self.focus_key();
        let (surface, at) = self.layer_focus.as_ref()?;
        let takes = (self.layers.iter())
            .any(|l| l.mapped && l.surface.wl_surface() == surface && l.surface.can_receive_keyboard_focus());
        if takes && *at == key {
            return Some((surface.clone(), false));
        }
        self.layer_focus = None;
        None
    }

    /// The layer surface on one of `layers` (front to back) under `pos` (global), on the screen there, with the
    /// surface hit (of `kinds`: its own, a subsurface's, a popup's) and that surface's origin.
    pub(super) fn layer_under(
        &self,
        layers: &[Layer],
        kinds: WindowSurfaceType,
        pos: Point<f64, Logical>,
    ) -> Option<(LayerSurface, WlSurface, Point<f64, Logical>)> {
        let (output, geo) = self.outputs.iter().find_map(|o| {
            let geo = self.space.output_geometry(o)?;
            geo.to_f64().contains(pos).then_some((o, geo))
        })?;
        let map = layer_map_for_output(output);
        let local = pos - geo.loc.to_f64();
        for &layer in layers {
            for l in map.layers_on(layer).rev() {
                let Some(at) = layer_origin(&map, l) else {
                    continue;
                };
                if let Some((surface, loc)) = l.surface_under(local - at.to_f64(), kinds) {
                    return Some((l.clone(), surface, (loc + at + geo.loc).to_f64()));
                }
            }
        }
        None
    }

    /// The screen of the layer surface whose own surface is `root`, seen from that surface's origin: where its
    /// popups must stay.
    pub(super) fn layer_screen_from(&self, root: &WlSurface) -> Option<Rectangle<i32, Logical>> {
        let record = self.layers.iter().find(|l| l.surface.wl_surface() == root)?;
        let output = self.output_named(&record.output)?;
        let size = self.space.output_geometry(&output)?.size;
        let at = layer_origin(&layer_map_for_output(&output), &record.surface)?;
        Some(Rectangle::new((-at.x, -at.y).into(), size))
    }

    /// The mapped layer surfaces on `output`, back to front as drawn, for `state`.
    pub(super) fn layer_views(&self, output: &str) -> Vec<LayerView> {
        let Some(o) = self.output_named(output) else {
            return Vec::new();
        };
        let Some(geo) = self.space.output_geometry(&o) else {
            return Vec::new();
        };
        let keyboard = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let map = layer_map_for_output(&o);
        let mut views = Vec::new();
        for &layer in BELOW.iter().rev().chain(ABOVE.iter().rev()) {
            for l in map.layers_on(layer).filter(|l| self.is_mapped(l)) {
                let Some(mut rect) = map.layer_geometry(l) else {
                    continue;
                };
                rect.loc += geo.loc;
                views.push(LayerView {
                    namespace: l.namespace().to_owned(),
                    layer: layer_name(layer),
                    rect: rect_of(rect),
                    keyboard: keyboard.as_ref() == Some(l.wl_surface()),
                });
            }
        }
        views
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Dir;

    const SCREEN: Rect = Rect {
        x: 1280,
        y: 0,
        w: 1920,
        h: 1080,
    };

    #[test]
    fn exclusive_zones_shrink_the_screen_in_global_coordinates() {
        let zone = Rectangle::new((0, 30).into(), (1920, 1050).into());
        assert_eq!(
            usable_area(SCREEN, zone),
            Rect {
                x: 1280,
                y: 30,
                w: 1920,
                h: 1050
            }
        );
        assert_eq!(
            usable_area(SCREEN, Rectangle::from_size((1920, 1080).into())),
            SCREEN,
            "no zones"
        );
    }

    #[test]
    fn a_zone_leaving_nothing_is_ignored() {
        for (w, h) in [(0, 0), (1920, 0), (-5, 100)] {
            // Smithay's zone arithmetic can go negative, which its constructors refuse.
            let mut zone = Rectangle::from_size((0, 0).into());
            (zone.size.w, zone.size.h) = (w, h);
            assert_eq!(usable_area(SCREEN, zone), SCREEN, "{w}x{h}");
        }
    }

    /// An orrery in this process, with no socket, backend or loop running: its display is pumped by hand against a
    /// client on a socket pair.
    mod in_process {
        use std::os::fd::AsFd as _;
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;

        use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
        use smithay::reexports::calloop::EventLoop;
        use smithay::reexports::wayland_server::Display;
        use smithay::utils::Transform;
        use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
        use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
        use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

        use super::super::Orrery;
        use crate::comp::ClientState;
        use crate::layout::Rect;

        /// A bar client: what it bound, and the latest configure (serial, width, height).
        #[derive(Default)]
        struct Bar {
            compositor: Option<wl_compositor::WlCompositor>,
            shm: Option<wl_shm::WlShm>,
            shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
            configure: Option<(u32, u32, u32)>,
        }

        impl Dispatch<wl_registry::WlRegistry, ()> for Bar {
            fn event(
                bar: &mut Self,
                registry: &wl_registry::WlRegistry,
                event: wl_registry::Event,
                _: &(),
                _: &Connection,
                qh: &QueueHandle<Self>,
            ) {
                if let wl_registry::Event::Global { name, interface, .. } = event {
                    match interface.as_str() {
                        "wl_compositor" => bar.compositor = Some(registry.bind(name, 4, qh, ())),
                        "wl_shm" => bar.shm = Some(registry.bind(name, 1, qh, ())),
                        "zwlr_layer_shell_v1" => bar.shell = Some(registry.bind(name, 4, qh, ())),
                        _ => {}
                    }
                }
            }
        }

        impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Bar {
            fn event(
                bar: &mut Self,
                _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
                event: zwlr_layer_surface_v1::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let zwlr_layer_surface_v1::Event::Configure { serial, width, height } = event {
                    bar.configure = Some((serial, width, height));
                }
            }
        }

        delegate_noop!(Bar: ignore wl_compositor::WlCompositor);
        delegate_noop!(Bar: ignore wl_shm::WlShm);
        delegate_noop!(Bar: ignore wl_shm_pool::WlShmPool);
        delegate_noop!(Bar: ignore wl_buffer::WlBuffer);
        delegate_noop!(Bar: ignore wl_surface::WlSurface);
        delegate_noop!(Bar: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

        /// Requests to orrery and events back, a few times over (the socket pair never blocks).
        fn pump(
            display: &mut Display<Orrery>,
            state: &mut Orrery,
            conn: &Connection,
            queue: &mut EventQueue<Bar>,
            bar: &mut Bar,
        ) {
            for _ in 0..4 {
                conn.flush().expect("client flush");
                display.dispatch_clients(state).expect("dispatch");
                display.flush_clients().expect("server flush");
                if let Some(guard) = conn.prepare_read() {
                    // Nothing to read is fine.
                    let _ = guard.read();
                }
                queue.dispatch_pending(bar).expect("client dispatch");
            }
        }

        fn mode(w: i32, h: i32) -> Mode {
            Mode {
                size: (w, h).into(),
                refresh: 60_000,
            }
        }

        #[test]
        fn a_resized_screen_with_a_bar_divides_what_the_bar_leaves_of_its_new_size() {
            let event_loop: EventLoop<'static, Orrery> = EventLoop::try_new().expect("loop");
            let mut display: Display<Orrery> = Display::new().expect("display");
            let mut state = Orrery::new(&event_loop, display.handle(), None).expect("state");
            let output = Output::new(
                "screen".into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                },
            );
            output.change_current_state(
                Some(mode(1280, 720)),
                Some(Transform::Normal),
                None,
                Some((0, 0).into()),
            );
            state.add_output(
                output.clone(),
                Rect {
                    x: 0,
                    y: 0,
                    w: 1280,
                    h: 720,
                },
            );

            let (server, client) = UnixStream::pair().expect("socket pair");
            state
                .display_handle
                .insert_client(server, Arc::new(ClientState::default()))
                .expect("client");
            let conn = Connection::from_socket(client).expect("connection");
            let mut queue = conn.new_event_queue();
            let qh = queue.handle();
            let mut bar = Bar::default();
            conn.display().get_registry(&qh, ());
            pump(&mut display, &mut state, &conn, &mut queue, &mut bar);

            let surface = bar.compositor.as_ref().expect("wl_compositor").create_surface(&qh, ());
            let shell = bar.shell.as_ref().expect("zwlr_layer_shell_v1");
            let layer = shell.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Top, "bar".into(), &qh, ());
            use zwlr_layer_surface_v1::Anchor;
            layer.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
            layer.set_size(0, 30);
            layer.set_exclusive_zone(30);
            surface.commit();
            pump(&mut display, &mut state, &conn, &mut queue, &mut bar);
            let (serial, width, height) = bar.configure.expect("the initial configure");
            assert_eq!((width, height), (1280, 30));
            layer.ack_configure(serial);
            let file = tempfile::tempfile().expect("shm file");
            file.set_len(1280 * 30 * 4).expect("size the shm file");
            let pool = bar
                .shm
                .as_ref()
                .expect("wl_shm")
                .create_pool(file.as_fd(), 1280 * 30 * 4, &qh, ());
            let buffer = pool.create_buffer(0, 1280, 30, 1280 * 4, wl_shm::Format::Argb8888, &qh, ());
            surface.attach(Some(&buffer), 0, 0);
            surface.commit();
            pump(&mut display, &mut state, &conn, &mut queue, &mut bar);
            let area = |state: &Orrery| state.stacks.monitors()[0].area();
            assert_eq!(
                area(&state),
                Rect {
                    x: 0,
                    y: 30,
                    w: 1280,
                    h: 690
                }
            );

            // The screen grows (as a nested window resized, or a new mode): the bar is told its new width, and the
            // layout divides what it leaves of the new size.
            output.change_current_state(Some(mode(1920, 1080)), None, None, None);
            state.resize_output(
                "screen",
                Rect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
            );
            assert_eq!(
                area(&state),
                Rect {
                    x: 0,
                    y: 30,
                    w: 1920,
                    h: 1050
                }
            );
            pump(&mut display, &mut state, &conn, &mut queue, &mut bar);
            assert_eq!(bar.configure.map(|(_, w, h)| (w, h)), Some((1920, 30)));
        }
    }

    #[test]
    fn only_quit_reclaim_and_vt_run_over_an_exclusive_surface() {
        assert!(runs_over_exclusive(&Action::Quit));
        assert!(runs_over_exclusive(&Action::Reclaim));
        assert!(runs_over_exclusive(&Action::Vt(3)));
        for other in [
            Action::FocusSlot(Dir::Left),
            Action::Close,
            Action::ShowTabs,
            Action::Workspace(2),
            Action::Spawn(vec!["x".into()]),
        ] {
            assert!(!runs_over_exclusive(&other), "{other:?}");
        }
    }
}
