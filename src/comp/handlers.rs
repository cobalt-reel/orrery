//! Protocol handlers: compositor, shm, xdg-shell, xdg-decoration, seat, data device, output. They translate
//! events into `Orrery` calls; placement is `Orrery::arrange`'s alone.

use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
use smithay::desktop::{PopupKind, Window, find_popup_root_surface, get_popup_toplevel_coords};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Resource};
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
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm, delegate_xdg_decoration,
    delegate_xdg_shell,
};

use super::{ClientState, Orrery};

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

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Popup grabs (menus closing on outside clicks, popup_done) are not handled yet.
    }
}

fn answer_state_request(surface: &ToplevelSurface) {
    if surface.is_initial_configure_sent() {
        surface.send_configure();
    }
}

impl Orrery {
    /// Keeps a popup inside its toplevel's slot (the slot it waits for, if hidden), so menus never cover a
    /// neighbouring tile.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(rect) = self.id_of_surface(&root).and_then(|id| self.slot_rect_of(id)) else {
            return;
        };
        // The toplevel sits at its slot's origin, and the positioner wants the target relative to the parent:
        // the slot, seen from the toplevel, starts at 0,0, less the popup chain's offset.
        let mut target = Rectangle::new((0, 0).into(), (rect.w, rect.h).into());
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
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

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: smithay::input::pointer::CursorImageStatus) {}

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

delegate_compositor!(Orrery);
delegate_shm!(Orrery);
delegate_xdg_shell!(Orrery);
delegate_xdg_decoration!(Orrery);
delegate_seat!(Orrery);
delegate_data_device!(Orrery);
delegate_output!(Orrery);
