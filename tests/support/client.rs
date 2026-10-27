//! A tiny Wayland client for the end-to-end tests: xdg toplevels with app ids and titles, a 1×1 shm buffer each
//! (sizes follow configures; the tests check state, not pixels), and a record of what the compositor told each one.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};

use super::TIMEOUT;

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
    /// The token the compositor gave for the last `activation_token` request.
    token: Option<String>,
    outputs: Vec<wl_output::WlOutput>,
    pub globals: Vec<String>,
    pub windows: Vec<Win>,
    pub popups: Vec<Popup>,
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

    /// A token of this client's own, from xdg_activation_v1.get_activation_token (no serial, no surface).
    pub fn activation_token(&mut self) -> String {
        let qh = self.queue.handle();
        self.state.token = None;
        let activation = self.state.activation.as_ref().expect("xdg_activation_v1");
        let token = activation.get_activation_token(&qh, ());
        token.commit();
        assert!(self.wait_for(|s| s.token.is_some()), "no token came");
        token.destroy();
        self.state.token.take().expect("the token")
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
    xdg_activation_v1::XdgActivationV1
);
