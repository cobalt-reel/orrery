//! A tiny Wayland client for the end-to-end tests: xdg toplevels with app ids and titles, a 1×1 shm buffer each
//! (sizes follow configures; the tests check state, not pixels), layer surfaces (wlr-layer-shell) with a buffer of
//! their configured size, and a record of what the compositor told each one.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle, WEnum};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::TIMEOUT;

pub use zwlr_layer_shell_v1::Layer;
pub use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
pub use zxdg_toplevel_decoration_v1::Mode as DecorationMode;

/// What the compositor has told one toplevel.
#[derive(Debug, Default)]
pub struct Win {
    pub app_id: String,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    /// Attach the buffer on the next configure (false: the window stays unmapped until told otherwise).
    auto_buffer: bool,
    /// A buffer is attached (committed) right now.
    attached: bool,
    /// From the latest xdg_toplevel.configure, applied when its xdg_surface.configure arrives.
    pending: Option<((i32, i32), Vec<xdg_toplevel::State>)>,
    /// The last acked configure's size and states.
    pub size: Option<(i32, i32)>,
    pub states: Vec<xdg_toplevel::State>,
    pub configures: usize,
    pub decoration_mode: Option<DecorationMode>,
    /// wl_surface.enter minus leave: above zero while the window is mapped on an output.
    pub outputs: i32,
    pub close_requested: bool,
    pub destroyed: bool,
}

impl Win {
    pub fn activated(&self) -> bool {
        self.states.contains(&xdg_toplevel::State::Activated)
    }

    pub fn tiled(&self) -> bool {
        use xdg_toplevel::State::*;
        [TiledLeft, TiledRight, TiledTop, TiledBottom]
            .iter()
            .all(|s| self.states.contains(s))
    }

    pub fn mapped(&self) -> bool {
        self.outputs > 0
    }
}

#[derive(Default)]
pub struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    decorations: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    activation: Option<xdg_activation_v1::XdgActivationV1>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    /// The token the compositor gave for the last `activation_token` request.
    token: Option<String>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The window with keyboard focus, and the serial of the latest wl_keyboard.enter (kept after a leave).
    pub keyboard_focus: Option<usize>,
    /// The layer surface with keyboard focus.
    pub keyboard_layer: Option<usize>,
    pub enter_serial: Option<u32>,
    /// wl_keyboard.key events in order: (evdev keycode, pressed).
    pub keys: Vec<(u32, bool)>,
    /// The latest wl_keyboard.modifiers' depressed mask and layout (group).
    pub mods_depressed: u32,
    pub group: u32,
    /// Each wl_keyboard.keymap's size, in order.
    pub keymaps: Vec<u32>,
    /// The latest wl_keyboard.repeat_info: (rate, delay).
    pub repeat: Option<(i32, i32)>,
    pointer: Option<wl_pointer::WlPointer>,
    /// The window under the pointer (wl_pointer.enter, until its leave).
    pub pointer_focus: Option<usize>,
    /// The layer surface, or popup, under the pointer.
    pub pointer_layer: Option<usize>,
    pub pointer_popup: Option<usize>,
    /// wl_pointer.button events in order: (button, pressed).
    pub buttons: Vec<(u32, bool)>,
    /// wl_pointer.motion events so far, and the latest one's position (surface-local).
    pub motions: usize,
    pub pointer_at: Option<(f64, f64)>,
    outputs: Vec<wl_output::WlOutput>,
    pub globals: Vec<String>,
    pub windows: Vec<Win>,
    pub popups: Vec<Popup>,
    pub layers: Vec<LayerSurf>,
}

/// An xdg_popup's user data, so its objects dispatch apart from the windows'.
#[derive(Debug, Clone, Copy)]
pub struct PopupId(pub usize);

#[derive(Debug, Default)]
pub struct Popup {
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    popup: Option<xdg_popup::XdgPopup>,
    buffer: Option<wl_buffer::WlBuffer>,
    pub configures: usize,
    pub done: bool,
}

/// A layer surface's user data, so its objects dispatch apart from the windows' and popups'.
#[derive(Debug, Clone, Copy)]
pub struct LayerId(pub usize);

/// What a layer surface asks for.
#[derive(Debug, Clone)]
pub struct LayerSpec {
    pub namespace: &'static str,
    pub layer: Layer,
    pub anchor: Anchor,
    /// 0 along an axis anchored on both its edges: the compositor's size.
    pub size: (u32, u32),
    pub exclusive_zone: i32,
    pub keyboard: KeyboardInteractivity,
    /// An output by its index in bind order; None leaves the choice to the compositor.
    pub output: Option<usize>,
}

impl LayerSpec {
    /// A bar `height` high along the top edge, reserving its height.
    pub fn bar(height: u32) -> Self {
        Self {
            namespace: "bar",
            layer: Layer::Top,
            anchor: Anchor::Top | Anchor::Left | Anchor::Right,
            size: (0, height),
            exclusive_zone: i32::try_from(height).expect("a small height"),
            keyboard: KeyboardInteractivity::None,
            output: None,
        }
    }

    /// `size`, centred on the screen (no anchors), on `layer`.
    pub fn centred(namespace: &'static str, layer: Layer, size: (u32, u32)) -> Self {
        Self {
            namespace,
            layer,
            anchor: Anchor::empty(),
            size,
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::None,
            output: None,
        }
    }
}

/// What the compositor has told one layer surface.
#[derive(Debug, Default)]
pub struct LayerSurf {
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    /// Attach a buffer of the configured size on each configure (false: it stays unmapped).
    auto_buffer: bool,
    /// The size of the buffer attached right now; None while unmapped.
    pub attached: Option<(i32, i32)>,
    /// The latest configure's size.
    pub size: Option<(u32, u32)>,
    pub configures: usize,
    pub closed: bool,
    /// wl_surface.enter minus leave.
    pub outputs: i32,
}

pub struct Client {
    _conn: Connection,
    queue: EventQueue<State>,
    pub state: State,
}

impl Client {
    pub fn new(conn: Connection) -> Self {
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).expect("registry roundtrip");
        // Bind events (wl_output's) arrive on the second trip.
        queue.roundtrip(&mut state).expect("bind roundtrip");
        assert!(state.compositor.is_some() && state.shm.is_some() && state.wm_base.is_some());
        // A keyboard, so the client learns its focus and the enter serials activation tokens can carry, and keys;
        // a pointer, for buttons.
        state.keyboard = state.seat.as_ref().map(|seat| seat.get_keyboard(&queue.handle(), ()));
        state.pointer = state.seat.as_ref().map(|seat| seat.get_pointer(&queue.handle(), ()));
        queue.roundtrip(&mut state).expect("keyboard roundtrip");
        Self {
            _conn: conn,
            queue,
            state,
        }
    }

    /// A new toplevel with a decoration request that maps (attaches its buffer) on its first configure;
    /// returns its index in `windows`.
    pub fn create_window(&mut self, app_id: &str, title: &str) -> usize {
        self.create_window_with(app_id, title, true, false, None)
    }

    /// `create_window` with `xdg_toplevel.set_parent(parent)` before its first commit, as a dialog does.
    pub fn create_dialog(&mut self, app_id: &str, title: &str, parent: usize) -> usize {
        self.create_window_with(app_id, title, true, false, Some(parent))
    }

    /// A toplevel that acks its configures but never attaches a buffer, so it never maps.
    pub fn create_unmapped_window(&mut self, app_id: &str, title: &str) -> usize {
        self.create_window_with(app_id, title, false, false, None)
    }

    /// Misbehaving: attaches a buffer in the initial commit, before any configure (a protocol error).
    pub fn create_window_attaching_early(&mut self, app_id: &str) -> usize {
        self.create_window_with(app_id, app_id, true, true, None)
    }

    fn create_window_with(
        &mut self,
        app_id: &str,
        title: &str,
        auto_buffer: bool,
        early: bool,
        parent: Option<usize>,
    ) -> usize {
        let qh = self.queue.handle();
        let i = self.state.windows.len();
        let s = &self.state;
        let surface = s.compositor.as_ref().expect("compositor").create_surface(&qh, i);
        let xdg_surface = s.wm_base.as_ref().expect("wm_base").get_xdg_surface(&surface, &qh, i);
        let toplevel = xdg_surface.get_toplevel(&qh, i);
        toplevel.set_app_id(app_id.to_owned());
        toplevel.set_title(title.to_owned());
        if let Some(p) = parent {
            toplevel.set_parent(Some(s.windows[p].toplevel.as_ref().expect("live parent")));
        }
        let decoration = s.decorations.as_ref().map(|d| {
            let deco = d.get_toplevel_decoration(&toplevel, &qh, i);
            // Asks for client-side on purpose: orrery must answer server-side anyway.
            deco.set_mode(DecorationMode::ClientSide);
            deco
        });
        let mut buffer = None;
        if early {
            let b = Client::buffer(&self.state, &qh, 1, 1);
            surface.attach(Some(&b), 0, 0);
            buffer = Some(b);
        }
        // The initial commit, with no buffer: the compositor answers with the first configure.
        surface.commit();
        self.state.windows.push(Win {
            app_id: app_id.to_owned(),
            surface: Some(surface),
            xdg_surface: Some(xdg_surface),
            toplevel: Some(toplevel),
            decoration,
            attached: early,
            buffer,
            auto_buffer,
            ..Win::default()
        });
        // Errors are allowed here: a misbehaving window may get the client disconnected.
        let _ = self.try_roundtrip();
        i
    }

    /// A new title for window `i`, as a client sets it while running.
    pub fn set_title(&mut self, i: usize, title: &str) {
        let toplevel = self.state.windows[i].toplevel.as_ref().expect("live window");
        toplevel.set_title(title.to_owned());
        self.roundtrip();
    }

    /// Unmaps by committing a null buffer; the window stays unmapped until `remap`.
    pub fn unmap(&mut self, i: usize) {
        let w = &mut self.state.windows[i];
        w.auto_buffer = false;
        w.attached = false;
        let surface = w.surface.as_ref().expect("live window");
        surface.attach(None, 0, 0);
        surface.commit();
        self.roundtrip();
    }

    /// After an unmap: the initial commit again; the buffer goes on with the configure that answers it.
    pub fn remap(&mut self, i: usize) {
        let w = &mut self.state.windows[i];
        w.auto_buffer = true;
        w.surface.as_ref().expect("live window").commit();
        self.roundtrip();
    }

    /// Commits a `width`×`height` buffer, whatever the configured size.
    pub fn attach_buffer(&mut self, i: usize, width: i32, height: i32) {
        let qh = self.queue.handle();
        let b = Client::buffer(&self.state, &qh, width, height);
        let w = &mut self.state.windows[i];
        let surface = w.surface.as_ref().expect("live window");
        surface.attach(Some(&b), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        if let Some(old) = w.buffer.replace(b) {
            old.destroy();
        }
        w.attached = true;
        self.roundtrip();
    }

    /// Misbehaving: destroys the wl_surface while its xdg_toplevel and xdg_surface still exist.
    pub fn destroy_surface_first(&mut self, i: usize) {
        let w = &mut self.state.windows[i];
        if let Some(s) = w.surface.take() {
            s.destroy();
        }
        w.destroyed = true;
        let _ = self.try_roundtrip();
    }

    /// A 10×10 popup on window `parent`, mapped on its first configure; returns its index in `popups`.
    pub fn create_popup(&mut self, parent: usize) -> usize {
        let qh = self.queue.handle();
        let i = self.state.popups.len();
        let id = PopupId(i);
        let s = &self.state;
        let surface = s.compositor.as_ref().expect("compositor").create_surface(&qh, id);
        let wm_base = s.wm_base.as_ref().expect("wm_base");
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, id);
        let positioner = wm_base.create_positioner(&qh, ());
        positioner.set_size(10, 10);
        positioner.set_anchor_rect(0, 0, 1, 1);
        let parent = s.windows[parent].xdg_surface.as_ref().expect("live parent");
        let popup = xdg_surface.get_popup(Some(parent), &positioner, &qh, id);
        positioner.destroy();
        surface.commit();
        self.state.popups.push(Popup {
            surface: Some(surface),
            xdg_surface: Some(xdg_surface),
            popup: Some(popup),
            ..Popup::default()
        });
        self.roundtrip();
        i
    }

    /// xdg_activation_v1.activate for window `i` with `token`, whoever made it.
    pub fn activate(&mut self, i: usize, token: &str) {
        let surface = self.state.windows[i].surface.as_ref().expect("live window");
        let activation = self.state.activation.as_ref().expect("xdg_activation_v1");
        activation.activate(token.to_owned(), surface);
        self.roundtrip();
    }

    /// xdg_activation_v1.activate for popup `i`'s surface with `token`.
    pub fn activate_popup(&mut self, i: usize, token: &str) {
        let surface = self.state.popups[i].surface.as_ref().expect("live popup");
        let activation = self.state.activation.as_ref().expect("xdg_activation_v1");
        activation.activate(token.to_owned(), surface);
        self.roundtrip();
    }

    /// A token of this client's own, from xdg_activation_v1.get_activation_token, with `serial` (an input event's,
    /// on this client's seat) if given; no surface.
    pub fn activation_token(&mut self, serial: Option<u32>) -> String {
        let qh = self.queue.handle();
        self.state.token = None;
        let activation = self.state.activation.as_ref().expect("xdg_activation_v1");
        let token = activation.get_activation_token(&qh, ());
        if let Some(serial) = serial {
            token.set_serial(serial, self.state.seat.as_ref().expect("wl_seat"));
        }
        token.commit();
        assert!(self.wait_for(|s| s.token.is_some()), "no token came");
        token.destroy();
        self.state.token.take().expect("the token")
    }

    /// A layer surface as `spec` says, given a buffer of its configured size on each configure; returns its index
    /// in `layers`.
    pub fn create_layer(&mut self, spec: &LayerSpec) -> usize {
        self.create_layer_with(spec, false)
    }

    /// Misbehaving: attaches a buffer in the initial commit, before any configure (a protocol error).
    pub fn create_layer_attaching_early(&mut self, spec: &LayerSpec) -> usize {
        self.create_layer_with(spec, true)
    }

    fn create_layer_with(&mut self, spec: &LayerSpec, early: bool) -> usize {
        let qh = self.queue.handle();
        let i = self.state.layers.len();
        let id = LayerId(i);
        let s = &self.state;
        let surface = s.compositor.as_ref().expect("compositor").create_surface(&qh, id);
        let output = spec.output.map(|o| &s.outputs[o]);
        let shell = s.layer_shell.as_ref().expect("zwlr_layer_shell_v1");
        let layer_surface = shell.get_layer_surface(&surface, output, spec.layer, spec.namespace.to_owned(), &qh, id);
        layer_surface.set_size(spec.size.0, spec.size.1);
        layer_surface.set_anchor(spec.anchor);
        layer_surface.set_exclusive_zone(spec.exclusive_zone);
        layer_surface.set_keyboard_interactivity(spec.keyboard);
        let mut buffer = None;
        if early {
            let b = Client::buffer(&self.state, &qh, 10, 10);
            surface.attach(Some(&b), 0, 0);
            buffer = Some(b);
        }
        // The initial commit, with no buffer: the compositor answers with the first configure.
        surface.commit();
        self.state.layers.push(LayerSurf {
            surface: Some(surface),
            layer_surface: Some(layer_surface),
            auto_buffer: true,
            attached: early.then_some((10, 10)),
            buffer,
            ..LayerSurf::default()
        });
        // Errors are allowed here: a misbehaving client gets disconnected.
        let _ = self.try_roundtrip();
        i
    }

    /// Misbehaving after `unmap_layer`: attaches a buffer and commits without the commit-configure-ack sequence
    /// (a protocol error).
    pub fn attach_layer_buffer_unasked(&mut self, i: usize) -> Result<(), String> {
        let qh = self.queue.handle();
        let b = Client::buffer(&self.state, &qh, 10, 10);
        let l = &mut self.state.layers[i];
        let surface = l.surface.as_ref().expect("live layer surface");
        surface.attach(Some(&b), 0, 0);
        surface.commit();
        l.buffer = Some(b);
        self.try_roundtrip()
    }

    /// Unmaps layer surface `i` by committing a null buffer; it stays unmapped until `remap_layer`.
    pub fn unmap_layer(&mut self, i: usize) {
        let l = &mut self.state.layers[i];
        l.auto_buffer = false;
        l.attached = None;
        let surface = l.surface.as_ref().expect("live layer surface");
        surface.attach(None, 0, 0);
        surface.commit();
        self.roundtrip();
    }

    /// After an unmap: the initial commit again; the buffer goes on with the configure that answers it.
    pub fn remap_layer(&mut self, i: usize) {
        let l = &mut self.state.layers[i];
        l.auto_buffer = true;
        l.surface.as_ref().expect("live layer surface").commit();
        self.roundtrip();
    }

    /// A new exclusive zone for layer surface `i`, committed.
    pub fn set_exclusive_zone(&mut self, i: usize, zone: i32) {
        let l = &self.state.layers[i];
        l.layer_surface
            .as_ref()
            .expect("live layer surface")
            .set_exclusive_zone(zone);
        l.surface.as_ref().expect("live layer surface").commit();
        self.roundtrip();
    }

    pub fn destroy_layer(&mut self, i: usize) {
        let l = &mut self.state.layers[i];
        if let Some(ls) = l.layer_surface.take() {
            ls.destroy();
        }
        if let Some(s) = l.surface.take() {
            s.destroy();
        }
        if let Some(b) = l.buffer.take() {
            b.destroy();
        }
        self.roundtrip();
    }

    /// A 10×10 popup on layer surface `parent` (zwlr_layer_surface_v1.get_popup), its top-left at `at` in the
    /// parent; returns its index in `popups`.
    pub fn create_layer_popup(&mut self, parent: usize, at: (i32, i32)) -> usize {
        let qh = self.queue.handle();
        let i = self.state.popups.len();
        let id = PopupId(i);
        let s = &self.state;
        let surface = s.compositor.as_ref().expect("compositor").create_surface(&qh, id);
        let wm_base = s.wm_base.as_ref().expect("wm_base");
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, id);
        let positioner = wm_base.create_positioner(&qh, ());
        positioner.set_size(10, 10);
        positioner.set_anchor_rect(at.0, at.1, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        let popup = xdg_surface.get_popup(None, &positioner, &qh, id);
        positioner.destroy();
        let parent = s.layers[parent].layer_surface.as_ref().expect("live parent");
        parent.get_popup(&popup);
        surface.commit();
        self.state.popups.push(Popup {
            surface: Some(surface),
            xdg_surface: Some(xdg_surface),
            popup: Some(popup),
            ..Popup::default()
        });
        self.roundtrip();
        i
    }

    pub fn layer(&self, i: usize) -> &LayerSurf {
        &self.state.layers[i]
    }

    pub fn popup(&self, i: usize) -> &Popup {
        &self.state.popups[i]
    }

    /// Destroys the toplevel and its surface, as a client closing a window does.
    pub fn destroy_window(&mut self, i: usize) {
        let w = &mut self.state.windows[i];
        if let Some(d) = w.decoration.take() {
            d.destroy();
        }
        if let Some(t) = w.toplevel.take() {
            t.destroy();
        }
        if let Some(x) = w.xdg_surface.take() {
            x.destroy();
        }
        if let Some(s) = w.surface.take() {
            s.destroy();
        }
        if let Some(b) = w.buffer.take() {
            b.destroy();
        }
        w.destroyed = true;
        self.roundtrip();
    }

    pub fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).expect("roundtrip");
    }

    /// A roundtrip that may fail: after a protocol error the compositor disconnects the client.
    pub fn try_roundtrip(&mut self) -> Result<(), String> {
        self.queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Roundtrips until `cond` holds; false on timeout. Some changes (output enter on the next frame, a layout
    /// switch from another connection) arrive on the compositor's own schedule.
    pub fn wait_for(&mut self, mut cond: impl FnMut(&State) -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.roundtrip();
            if cond(&self.state) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn window(&self, i: usize) -> &Win {
        &self.state.windows[i]
    }

    /// An ARGB buffer in a temp file (shm needs an fd; no memfd crate needed). The file is sparse, so a large
    /// one costs nothing.
    fn buffer(state: &State, qh: &QueueHandle<State>, width: i32, height: i32) -> wl_buffer::WlBuffer {
        let file = tempfile::tempfile().expect("shm file");
        let len = width * height * 4;
        file.set_len(len as u64).expect("size the shm file");
        let pool = state.shm.as_ref().expect("shm").create_pool(file.as_fd(), len, qh, ());
        let buffer = pool.create_buffer(0, width, height, width * 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        buffer
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(6), qh, ())),
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(6), qh, ())),
            "zxdg_decoration_manager_v1" => state.decorations = Some(registry.bind(name, 1, qh, ())),
            "xdg_activation_v1" => state.activation = Some(registry.bind(name, 1, qh, ())),
            "zwlr_layer_shell_v1" => state.layer_shell = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
            // Without a bound wl_output, wl_surface.enter cannot be sent, and mapping could not be observed.
            "wl_output" => state.outputs.push(registry.bind(name, version.min(4), qh, ())),
            _ => {}
        }
        state.globals.push(interface);
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        &i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let w = &mut state.windows[i];
        match event {
            xdg_toplevel::Event::Configure { width, height, states } => {
                // An array of u32 in native byte order.
                let states = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter_map(|&c| xdg_toplevel::State::try_from(u32::from_ne_bytes(c)).ok())
                    .collect();
                w.pending = Some(((width, height), states));
            }
            xdg_toplevel::Event::Close => w.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        &i: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg_surface.ack_configure(serial);
        let w = &state.windows[i];
        let attach = w.auto_buffer && !w.attached;
        let new_buffer = (attach && w.buffer.is_none()).then(|| Client::buffer(state, qh, 1, 1));
        let w = &mut state.windows[i];
        if let Some((size, states)) = w.pending.take() {
            w.size = Some(size);
            w.states = states;
        }
        w.configures += 1;
        if let Some(b) = new_buffer {
            w.buffer = Some(b);
        }
        let Some(surface) = &w.surface else {
            return;
        };
        if attach && let Some(buffer) = &w.buffer {
            surface.attach(Some(buffer), 0, 0);
            surface.damage_buffer(0, 0, 1, 1);
            w.attached = true;
        }
        surface.commit();
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        &i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure {
            mode: WEnum::Value(mode),
        } = event
        {
            state.windows[i].decoration_mode = Some(mode);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        &i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => state.windows[i].outputs += 1,
            wl_surface::Event::Leave { .. } => state.windows[i].outputs -= 1,
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, PopupId> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        &PopupId(i): &PopupId,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg_surface.ack_configure(serial);
        let new_buffer = state.popups[i]
            .buffer
            .is_none()
            .then(|| Client::buffer(state, qh, 10, 10));
        let p = &mut state.popups[i];
        p.configures += 1;
        let Some(surface) = &p.surface else {
            return;
        };
        if let Some(b) = new_buffer {
            surface.attach(Some(&b), 0, 0);
            p.buffer = Some(b);
        }
        surface.commit();
    }
}

impl Dispatch<xdg_popup::XdgPopup, PopupId> for State {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        &PopupId(i): &PopupId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_popup::Event::PopupDone = event {
            state.popups[i].done = true;
        }
    }
}

impl Dispatch<wl_surface::WlSurface, PopupId> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &PopupId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { serial, surface, .. } => {
                state.enter_serial = Some(serial);
                state.keyboard_focus = surface.data::<usize>().copied();
                state.keyboard_layer = surface.data::<LayerId>().map(|l| l.0);
            }
            wl_keyboard::Event::Leave { .. } => {
                state.keyboard_focus = None;
                state.keyboard_layer = None;
            }
            wl_keyboard::Event::Key { key, state: k, .. } => {
                state
                    .keys
                    .push((key, k == WEnum::Value(wl_keyboard::KeyState::Pressed)));
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed, group, ..
            } => {
                state.mods_depressed = mods_depressed;
                state.group = group;
            }
            wl_keyboard::Event::Keymap { size, .. } => state.keymaps.push(size),
            wl_keyboard::Event::RepeatInfo { rate, delay } => state.repeat = Some((rate, delay)),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => {
                state.pointer_focus = surface.data::<usize>().copied();
                state.pointer_layer = surface.data::<LayerId>().map(|l| l.0);
                state.pointer_popup = surface.data::<PopupId>().map(|p| p.0);
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_focus = None;
                state.pointer_layer = None;
                state.pointer_popup = None;
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                state.motions += 1;
                state.pointer_at = Some((surface_x, surface_y));
            }
            wl_pointer::Event::Button { button, state: b, .. } => {
                state
                    .buttons
                    .push((button, b == WEnum::Value(wl_pointer::ButtonState::Pressed)));
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, LayerId> for State {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &LayerId(i): &LayerId,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer_surface.ack_configure(serial);
                let size = (
                    i32::try_from(width).unwrap_or(1).max(1),
                    i32::try_from(height).unwrap_or(1).max(1),
                );
                let l = &state.layers[i];
                // A buffer of the new size, as a client redrawing for its configure does.
                let new_buffer =
                    (l.auto_buffer && l.attached != Some(size)).then(|| Client::buffer(state, qh, size.0, size.1));
                let l = &mut state.layers[i];
                l.size = Some((width, height));
                l.configures += 1;
                let Some(surface) = &l.surface else {
                    return;
                };
                if let Some(b) = new_buffer {
                    surface.attach(Some(&b), 0, 0);
                    surface.damage_buffer(0, 0, size.0, size.1);
                    if let Some(old) = l.buffer.replace(b) {
                        old.destroy();
                    }
                    l.attached = Some(size);
                }
                surface.commit();
            }
            zwlr_layer_surface_v1::Event::Closed => state.layers[i].closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, LayerId> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        &LayerId(i): &LayerId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => state.layers[i].outputs += 1,
            wl_surface::Event::Leave { .. } => state.layers[i].outputs -= 1,
            _ => {}
        }
    }
}

/// Objects whose events the tests do not need.
macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection,
                     _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_output::WlOutput,
    xdg_positioner::XdgPositioner,
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    xdg_activation_v1::XdgActivationV1,
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    wl_seat::WlSeat
);
