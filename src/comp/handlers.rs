//! Protocol handlers: compositor, shm, xdg-shell, xdg-decoration, xdg-activation, layer-shell, seat, data device,
//! output. They translate events into `Orrery` calls; placement is `Orrery::arrange`'s alone (and Smithay's
//! `LayerMap`'s for layer surfaces, see `layers`).

use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window, find_popup_root_surface,
    get_popup_toplevel_coords,
};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::xdg::activation::v1::server::{xdg_activation_token_v1, xdg_activation_v1};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, Resource, delegate_dispatch, delegate_global_dispatch,
};
use smithay::utils::{Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
    ActivationTokenData, XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_layer_shell, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
};

use super::{ClientState, Orrery};

/// At most this many client tokens are kept at once (5 s each): a client asking in a loop cannot grow the pool.
const KEPT_TOKENS: usize = 64;

impl CompositorHandler for Orrery {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every client is inserted with ClientState (see `listen`), so this cannot miss.
        &client
            .get_data::<ClientState>()
            .expect("every client carries ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // Mapped or not: windows beneath the top still commit and need their state cached.
            if let Some(window) = self.window_of_surface(&root) {
                window.on_commit();
            }
        }

        // A toplevel's own surface: configure, open on its first buffer, wait again after a null one.
        if let Some(id) = self.id_of_surface(surface) {
            let has_buffer = with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);
            self.toplevel_committed(id, has_buffer);
        }
        // A layer surface's own: placed, configured, mapped or unmapped.
        self.layer_committed(surface);

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(e) = popup.send_configure()
        {
            tracing::warn!("popup configure: {e}");
        }
    }
}

impl BufferHandler for Orrery {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Orrery {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for Orrery {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Server-side by default, for clients that never bind xdg-decoration; this is no request to them.
        surface.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        self.add_window(Window::new_wayland_window(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id_of_surface(surface.wl_surface()) {
            self.remove_window(id);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("cannot track a popup: {e}");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    // Static tiling: interactive move/resize, maximize and fullscreen are not honoured; the window keeps its
    // slot. The protocol wants a configure in answer to (un)maximize and (un)fullscreen, but none may go out
    // before the initial commit, which Smithay's default handlers do not check.

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        answer_state_request(&surface);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        answer_state_request(&surface);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        answer_state_request(&surface);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        answer_state_request(&surface);
    }

    /// A popup's grab (a menu opened on a click): it takes the keyboard and the pointer until it closes, and a click
    /// outside its client closes the whole chain (`popup_done`). Refused (the popup closed at once) when the seat is
    /// held by another grab this one does not follow from (a submenu follows its menu's).
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Orrery>::from_resource(&seat) else {
            return;
        };
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else {
            return;
        };
        let mut grab = match self.popups.grab_popup(root, popup, &seat, serial) {
            Ok(grab) => grab,
            Err(e) => {
                tracing::debug!("popup grab refused: {e:?}");
                return;
            }
        };
        if let Some(keyboard) = seat.get_keyboard() {
            let follows = keyboard.has_grab(serial) || keyboard.has_grab(grab.previous_serial().unwrap_or(serial));
            if keyboard.is_grabbed() && !follows {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            let previous = grab.previous_serial().unwrap_or_else(|| grab.serial());
            if pointer.is_grabbed() && !(pointer.has_grab(serial) || pointer.has_grab(previous)) {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}

fn answer_state_request(surface: &ToplevelSurface) {
    if surface.is_initial_configure_sent() {
        surface.send_configure();
    }
}

impl Orrery {
    /// Keeps a popup inside its toplevel's slot (the slot it waits for, if hidden), so menus never cover a
    /// neighbouring tile; a layer surface's popup inside its screen. Without a parent yet (a layer surface's popup
    /// before `get_popup`) there is nothing to do.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        // The positioner wants the target relative to the parent. A toplevel sits at its slot's origin: the slot,
        // seen from it, starts at 0,0; a layer surface's screen starts at minus its place. Less the chain's offset.
        let slot = (self.id_of_surface(&root).and_then(|id| self.slot_rect_of(id)))
            .map(|rect| Rectangle::new((0, 0).into(), (rect.w, rect.h).into()));
        let Some(mut target) = slot.or_else(|| self.layer_screen_from(&root)) else {
            return;
        };
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

impl WlrLayerShellHandler for Orrery {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        self.new_layer(surface, output, namespace);
    }

    // The popup was tracked by xdg-shell's new_popup, without a parent; now it has one, it can be kept on screen.
    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        self.forget_layer(surface.wl_surface());
    }
}

impl XdgDecorationHandler for Orrery {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_server_side(&toplevel);
    }

    // Whatever the client asks for, orrery draws (or omits) the decorations: always server-side.
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        set_server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_server_side(&toplevel);
    }
}

fn set_server_side(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
    // A decoration object needs a configure to learn its mode; before the initial one, that one carries it.
    if toplevel.is_initial_configure_sent() {
        toplevel.send_configure();
    }
}

impl SeatHandler for Orrery {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Orrery> {
        &mut self.seat_state
    }

    /// Kept for drawing the pointer where orrery draws it (`Orrery::pointer_drawn`, `render::cursor_draw`).
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_image = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // The clipboard goes to the client with keyboard focus.
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

impl SelectionHandler for Orrery {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Orrery {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Orrery {}
impl ServerDndGrabHandler for Orrery {}

impl OutputHandler for Orrery {}

impl XdgActivationHandler for Orrery {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    // A client's own token (xdg_activation_v1.get_activation_token, to pass focus to another of its windows or to
    // a program it starts) is never a launch's. It is kept, and can then focus a shown window once, only when it
    // comes from input to the focused client (`Orrery::input_backed`) and fewer than `KEPT_TOKENS` are kept; any
    // other is answered but not kept, so activating with it only marks. Old ones are dropped here and on each
    // activation.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.forget_old_tokens(std::time::Instant::now());
        self.xdg_activation_state.tokens().count() < KEPT_TOKENS && self.input_backed(&data)
    }

    // Unreached: orrery dispatches xdg_activation_v1 itself (below). Routed the same way should that change.
    fn request_activation(&mut self, token: XdgActivationToken, _data: XdgActivationTokenData, surface: WlSurface) {
        self.activation_requested(&token, &surface);
    }
}

/// Smithay's dispatch, except `activate`: Smithay passes on only tokens in its pool and drops the rest silently,
/// but to orrery every request counts, if only for a mark (`Orrery::activation_requested` decides what a token is
/// worth).
impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for Orrery {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &xdg_activation_v1::XdgActivationV1,
        request: xdg_activation_v1::Request,
        data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_activation_v1::Request::Activate { token, surface } => state.activation_requested(&token, &surface),
            other => <XdgActivationState as Dispatch<xdg_activation_v1::XdgActivationV1, (), Self>>::request(
                state, client, resource, other, data, dh, data_init,
            ),
        }
    }
}

delegate_compositor!(Orrery);
delegate_shm!(Orrery);
delegate_xdg_shell!(Orrery);
delegate_xdg_decoration!(Orrery);
delegate_layer_shell!(Orrery);
delegate_seat!(Orrery);
delegate_data_device!(Orrery);
delegate_output!(Orrery);
// delegate_xdg_activation! less the xdg_activation_v1 requests (see its Dispatch above).
delegate_global_dispatch!(Orrery: [xdg_activation_v1::XdgActivationV1: ()] => XdgActivationState);
delegate_dispatch!(Orrery: [xdg_activation_token_v1::XdgActivationTokenV1: ActivationTokenData] => XdgActivationState);
