//! Input from a backend that has some (winit): a key press matching a bind runs its action (neither the press
//! nor its release reaches a client), other keys go to the focused window (or the layer surface holding the
//! keyboard), the pointer to the surface under it (overlay, top, popups of bottom and background, the slots'
//! windows, bottom, background), and a click focuses the slot under it, unless it lands on something drawn over the
//! slots.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent,
};
use smithay::desktop::{LayerSurface, WindowSurfaceType};
use std::collections::HashSet;
use std::sync::PoisonError;

use smithay::input::keyboard::{FilterResult, Keycode, Keysym, ModifiersState, xkb};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use super::Orrery;
use super::layers::{ABOVE, BELOW};
use crate::config::{Action, Bind, Keyboard, Mods};
use crate::ipc::{KeyView, Request};
use crate::layout::Rect;
use crate::link::ScrollSource;
use crate::stacks::{Stacks, WinId};

/// Keys held for binds. A press matching a bind is swallowed and remembered, so its release is swallowed too;
/// show-tabs is a hold, on while its key is down.
#[derive(Debug, Default)]
pub struct Held {
    swallowed: HashSet<Keycode>,
    pub show_tabs: Option<Keycode>,
}

impl Held {
    /// A press, and the action of the bind it matched. An unmatched press forgets the key: a release that never
    /// arrived (focus lost mid-bind) must not swallow this press's release.
    pub fn press(&mut self, code: Keycode, bind: Option<&Action>) -> FilterResult<Option<Action>> {
        let Some(action) = bind else {
            self.swallowed.remove(&code);
            return FilterResult::Forward;
        };
        self.swallowed.insert(code);
        if *action == Action::ShowTabs {
            self.show_tabs = Some(code);
            return FilterResult::Intercept(None);
        }
        FilterResult::Intercept(Some(action.clone()))
    }

    pub fn release(&mut self, code: Keycode) -> FilterResult<Option<Action>> {
        if self.show_tabs == Some(code) {
            self.show_tabs = None;
        }
        if self.swallowed.remove(&code) {
            FilterResult::Intercept(None)
        } else {
            FilterResult::Forward
        }
    }

    /// Focus lost: no release will come for what is held.
    pub fn clear(&mut self) {
        self.swallowed.clear();
        self.show_tabs = None;
    }
}

/// The modifiers a bind can name; lock keys (Caps, Num) never stop a bind from firing.
#[must_use]
pub fn mods_of(m: &ModifiersState) -> Mods {
    Mods {
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
        logo: m.logo,
    }
}

/// A key whose holding counts across a handoff (`Crossing::key`): Shift, Ctrl, Alt, Meta, Super, Hyper, AltGr and the
/// like. Lock and latch keys (Caps, Num, the ISO locks) are not: each machine keeps its own lock state.
#[must_use]
pub fn held_modifier(sym: Keysym) -> bool {
    use xkb::keysyms as k;
    match sym.raw() {
        k::KEY_Caps_Lock | k::KEY_Shift_Lock => false,
        s => {
            (k::KEY_Shift_L..=k::KEY_Hyper_R).contains(&s)
                || matches!(
                    s,
                    k::KEY_ISO_Level3_Shift | k::KEY_ISO_Level5_Shift | k::KEY_Mode_switch
                )
        }
    }
}

/// The bind modifiers the held modifier keys `syms` make, as the default keymap maps them (Super and Hyper are both
/// Mod4): what the escape key is matched with while this machine's own seat does not see the keys.
#[must_use]
pub fn mods_held(syms: impl IntoIterator<Item = Keysym>) -> Mods {
    use xkb::keysyms as k;
    let mut mods = Mods::default();
    for sym in syms {
        match sym.raw() {
            k::KEY_Shift_L | k::KEY_Shift_R => mods.shift = true,
            k::KEY_Control_L | k::KEY_Control_R => mods.ctrl = true,
            k::KEY_Alt_L | k::KEY_Alt_R | k::KEY_Meta_L | k::KEY_Meta_R => mods.alt = true,
            k::KEY_Super_L | k::KEY_Super_R | k::KEY_Hyper_L | k::KEY_Hyper_R => mods.logo = true,
            _ => {}
        }
    }
    mods
}

/// A key press for `state`'s `last_key`, as the bind lookup read it. Typing (no Ctrl, Alt or Super, not a modifier
/// key itself) is kept only as `(plain key)`: which key was typed never is.
#[must_use]
pub fn key_note(mods: Mods, sym: Option<Keysym>, bind: Option<&Action>) -> KeyView {
    let shortcut = mods.ctrl || mods.alt || mods.logo || bind.is_some();
    let key = match sym {
        Some(s) if shortcut || s.is_modifier_key() => xkb::keysym_get_name(s),
        _ => "(plain key)".to_owned(),
    };
    let names = [
        (mods.ctrl, "Ctrl"),
        (mods.alt, "Alt"),
        (mods.shift, "Shift"),
        (mods.logo, "Super"),
    ];
    let held: Vec<&str> = names.iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect();
    KeyView {
        key,
        mods: held.join("+"),
        bind: bind.map(|a| format!("{a:?}")),
    }
}

/// A keysym folded the way bind keysyms are (lower case when the key is a character with a one-character
/// lower case), so `H` from a layout whose base level is upper case still matches `Mod+H`.
#[must_use]
pub fn fold(sym: Keysym) -> Keysym {
    let Some(c) = char::from_u32(xkb::keysym_to_utf32(sym)).filter(|&c| c != '\0') else {
        return sym;
    };
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) if l != c => {
            let folded = xkb::utf32_to_keysym(u32::from(l));
            if folded.raw() == xkb::keysyms::KEY_NoSymbol {
                sym
            } else {
                folded
            }
        }
        _ => sym,
    }
}

/// Exactly these modifiers and this key. `sym` must be the raw (unshifted) keysym of the press: binds store
/// `Mod+Shift+H` as `h` with Shift, and Shift+Tab's shifted sym is ISO_Left_Tab.
#[must_use]
pub fn matches(bind: &Bind, mods: Mods, sym: Keysym) -> bool {
    bind.mods == mods && bind.keysym == fold(sym)
}

#[must_use]
pub fn find_bind(binds: &[Bind], mods: Mods, sym: Keysym) -> Option<&Bind> {
    binds.iter().find(|b| matches(b, mods, sym))
}

/// The control-socket request an action is, so keys and `orrery msg` take one path. Show-tabs, reclaim, vt and a
/// spawn without a launch are keys only.
#[must_use]
pub fn action_request(action: &Action) -> Option<Request> {
    Some(match action {
        Action::FocusSlot(dir) => Request::FocusSlot { dir: *dir },
        Action::Move(dir) => Request::Move { dir: *dir },
        Action::SwapSlot(dir) => Request::SwapSlot { dir: *dir },
        Action::CycleTab(cycle) => Request::CycleTab { cycle: *cycle },
        Action::Split => Request::Split,
        Action::ToggleStacking => Request::ToggleStacking,
        Action::Spawn(argv) => Request::Spawn { argv: argv.clone() },
        Action::Layout(name) => Request::Layout {
            name: name.clone(),
            monitor: None,
        },
        Action::Workspace(number) => Request::Workspace { number: *number },
        Action::MoveToWorkspace(number) => Request::MoveToWorkspace { number: *number },
        Action::Close => Request::Close,
        Action::Quit => Request::Quit,
        Action::ShowTabs | Action::Reclaim | Action::Vt(_) | Action::SpawnNoLaunch(_) => return None,
    })
}

/// A window-local pointer position in global space: on the output whose window it came from, at `origin`.
#[must_use]
pub fn on_output(local: Point<f64, Logical>, origin: Point<i32, Logical>) -> Point<f64, Logical> {
    local + origin.to_f64()
}

/// The monitor and slot (of its shown workspace) whose rectangle holds the point, empty slots included.
#[must_use]
pub fn slot_at(stacks: &Stacks, x: i32, y: i32) -> Option<(String, usize)> {
    let inside = |r: Rect| x >= r.x && y >= r.y && x - r.x < r.w && y - r.y < r.h;
    stacks.monitors().iter().enumerate().find_map(|(mi, m)| {
        if !inside(m.area()) {
            return None;
        }
        let slot = stacks
            .shown(mi)?
            .layout()
            .rects(m.area())
            .into_iter()
            .position(inside)?;
        Some((m.name().to_owned(), slot))
    })
}

/// The focused monitor's name and the focused slot of the workspace it shows.
#[must_use]
pub fn focused_slot(stacks: &Stacks) -> Option<(&str, usize)> {
    let m = stacks.focused_monitor();
    Some((stacks.monitors().get(m)?.name(), stacks.shown(m)?.focus()))
}

/// The top window of the slot holding the point, and that slot's rectangle.
#[must_use]
pub fn top_at(stacks: &Stacks, x: i32, y: i32) -> Option<(WinId, Rect)> {
    let (name, slot) = slot_at(stacks, x, y)?;
    let mi = stacks.monitors().iter().position(|m| m.name() == name)?;
    let ws = stacks.shown(mi)?;
    let top = ws.slots().get(slot)?.top()?;
    let rect = *ws.layout().rects(stacks.monitors()[mi].area()).get(slot)?;
    Some((top, rect))
}

/// What the pointer is over: a surface, its origin (global), the layer surface it belongs to, if any, and whether
/// it is drawn over the slots.
pub struct Under {
    pub surface: WlSurface,
    pub origin: Point<f64, Logical>,
    pub layer: Option<LayerSurface>,
    pub over_slots: bool,
}

impl Orrery {
    /// The surface under `pos`, as drawn (`under`).
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.under(pos).map(|u| (u.surface, u.origin))
    }

    /// What is under `pos`, as drawn, front to back: overlay and top layer surfaces, popups of bottom and
    /// background ones, the slots' windows (`window_under`), bottom and background layer surfaces themselves.
    pub fn under(&self, pos: Point<f64, Logical>) -> Option<Under> {
        let layer = |over_slots| {
            move |(layer, surface, origin)| Under {
                surface,
                origin,
                layer: Some(layer),
                over_slots,
            }
        };
        let own = WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE;
        (self.layer_under(&ABOVE, WindowSurfaceType::ALL, pos).map(layer(true)))
            .or_else(|| (self.layer_under(&BELOW, WindowSurfaceType::POPUP, pos)).map(layer(true)))
            .or_else(|| {
                (self.window_under(pos)).map(|(surface, origin)| Under {
                    surface,
                    origin,
                    layer: None,
                    over_slots: false,
                })
            })
            .or_else(|| self.layer_under(&BELOW, own, pos).map(layer(false)))
    }

    /// The window surface under `pos`: only the top window of the slot holding `pos`, and only its parts inside
    /// that slot (render clips everything, popups included, to the slot).
    fn window_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (id, slot) = top_at(&self.stacks, pos.x.floor() as i32, pos.y.floor() as i32)?;
        let window = self.windows.get(&id)?;
        // Where render puts the window's surfaces: its geometry origin at the slot's.
        let origin = Point::<i32, Logical>::from((slot.x, slot.y)) - window.geometry().loc;
        window
            .surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, p)| (s, (p + origin).to_f64()))
    }

    /// Decides a key's fate (see `Held`). Every bind runs whoever has the keyboard, a layer surface (`exclusive` or
    /// `on_demand`) as much as a window, as other compositors do; only keys matching no bind reach it.
    fn filter_key(
        &mut self,
        key_state: KeyState,
        mods: &ModifiersState,
        code: Keycode,
        sym: Option<Keysym>,
    ) -> FilterResult<Option<Action>> {
        match key_state {
            KeyState::Pressed => {
                let bind = sym.and_then(|sym| find_bind(&self.config.binds, mods_of(mods), sym));
                self.last_key = Some(key_note(mods_of(mods), sym, bind.map(|b| &b.action)));
                self.held.press(code, bind.map(|b| &b.action))
            }
            KeyState::Released => self.held.release(code),
        }
    }

    /// A bind's action. Failures are logged: a key has nowhere to answer. A program that would not start is a
    /// warning; a request with nothing to act on (no window to close, say) is not.
    pub fn run_action(&mut self, action: Action) {
        match action {
            Action::Reclaim => return self.input_reclaim(),
            Action::Vt(vt) => return self.switch_vt(vt),
            Action::SpawnNoLaunch(argv) => {
                if let Err(e) = self.spawn_with(&argv, false) {
                    tracing::warn!("spawn (no launch): {e}");
                }
                return;
            }
            _ => {}
        }
        if let Some(request) = action_request(&action) {
            let response = self.handle_request(request);
            if let Some(e) = response.error {
                if matches!(action, Action::Spawn(_)) {
                    tracing::warn!("{action:?}: {e}");
                } else {
                    tracing::info!("{action:?}: {e}");
                }
            }
        }
    }

    /// To console `vt`, where the backend has consoles.
    fn switch_vt(&mut self, vt: u8) {
        match &mut self.consoles {
            Some(consoles) => {
                if let Err(e) = consoles.switch(vt) {
                    tracing::warn!("cannot switch to console {vt}: {e}");
                }
            }
            None => tracing::info!("vt {vt}: this backend has no consoles to switch to"),
        }
    }

    /// The config's `keyboard` on the seat where it differs from `old`: a new keymap goes to every client (while the
    /// primary's keymap is in use, it is the one that comes back after), on the layout in use when the layouts are
    /// the same (other options, say), new repeat info too. Err when xkb cannot
    /// build the keymap (the old one stays).
    pub fn apply_keyboard(&mut self, old: &Keyboard) -> Result<(), String> {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return Ok(());
        };
        let new = self.config.keyboard.clone();
        if new.repeat_info() != old.repeat_info() {
            let (rate, delay) = new.repeat_info();
            keyboard.change_repeat_info(rate, delay);
        }
        if new.same_keymap(old) {
            return Ok(());
        }
        let failed = || "xkb cannot build the keymap of the `keyboard` block; keeping the old one".to_owned();
        let same_layouts = new.same_layouts(old);
        if self.keymap_kept() {
            let text = new.keymap_text().ok_or_else(failed)?;
            self.replace_kept_keymap(text, same_layouts);
            return Ok(());
        }
        let layout = keyboard.with_xkb_state(self, |context| {
            (context.xkb().lock().unwrap_or_else(PoisonError::into_inner)).active_layout()
        });
        keyboard.set_xkb_config(self, new.xkb()).map_err(|_| failed())?;
        // A new keymap starts on its first layout: back to the one in use, when the layouts are the same.
        if same_layouts {
            self.select_layout(layout);
        }
        Ok(())
    }

    /// The host window lost keyboard focus: no release will arrive for what is down now. Input away comes back (the
    /// other machine lets go of everything, nothing is pressed here); every pressed key is released through the usual
    /// path (a swallowed key stays swallowed, others reach the client), so neither the seat's modifier state nor a
    /// client is left with Alt held; then held binds and keys are forgotten.
    pub fn keyboard_focus_lost(&mut self) {
        // Forgotten first: input coming home presses none of them here only to release them at once.
        if let Some(crossing) = self.crossing() {
            crossing.forget_keys();
        }
        self.input_reclaim();
        if let Some(keyboard) = self.seat.get_keyboard() {
            self.release_keys(keyboard.pressed_keys());
        }
        // Released too: the primary's release of one of them now goes nowhere.
        self.remote.forget_keys();
        self.held.clear();
    }

    /// Releases `keys` through the usual path: a swallowed key stays swallowed, others reach the client.
    pub fn release_keys(&mut self, keys: impl IntoIterator<Item = Keycode>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let time = self.clock();
        for code in keys {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<Option<Action>, _>(self, code, KeyState::Released, serial, time, |orrery, _, _| {
                orrery.held.release(code)
            });
        }
    }

    /// Milliseconds since start: the time of input this machine makes up (releases, input from the link).
    #[must_use]
    pub fn clock(&self) -> u32 {
        u32::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u32::MAX)
    }

    /// A key went down or up, on this machine's keyboard or the primary's: a press matching a bind runs its action
    /// (see `Held`), anything else goes to the focused client.
    pub fn key_input(&mut self, code: Keycode, key_state: KeyState, time: u32) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let action = keyboard
            .input::<Option<Action>, _>(self, code, key_state, serial, time, |orrery, mods, key| {
                orrery.filter_key(key_state, mods, key.raw_code(), key.raw_latin_sym_or_raw_current_sym())
            })
            .flatten();
        // Run outside the filter: actions re-focus the keyboard, which must not happen mid-input.
        if let Some(action) = action {
            self.run_action(action);
        }
    }

    /// The pointer to `pos` (global): the surface under it there gets the motion.
    pub fn pointer_to(&mut self, pos: Point<f64, Logical>, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let under = self.surface_under(pos);
        self.pointer_seen = true;
        self.pointer_target = under.clone();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        if self.config.focus_follows_mouse && !pointer.is_grabbed() {
            self.hover_focus(pos);
        }
    }

    /// `focus-follows-mouse`: the pointer moving into another slot focuses it (empty or not), as a click would;
    /// not over a layer surface above the slots (a bar never takes the focus by hovering). Only motion calls it, so
    /// a focus moved by keys stays until the pointer moves; a grab (a menu, a drag) holds it off.
    fn hover_focus(&mut self, at: Point<f64, Logical>) {
        if self.under(at).is_some_and(|u| u.over_slots) {
            return;
        }
        let Some((monitor, slot)) = slot_at(&self.stacks, at.x as i32, at.y as i32) else {
            return;
        };
        if focused_slot(&self.stacks) == Some((monitor.as_str(), slot)) {
            return;
        }
        self.stacks.focus_at(&monitor, slot);
        self.layer_focus = None;
        self.arrange();
    }

    /// A pointer button went down or up: a press first focuses the slot under the pointer (empty or not), through
    /// the stacks, unless a layer surface over the slots is hit; a layer surface hit that takes the keyboard on demand
    /// gets it (`layer_clicked`). Then it goes to the surface under the pointer.
    pub fn button_input(&mut self, button: u32, state: ButtonState, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        if state == ButtonState::Pressed && !pointer.is_grabbed() {
            let at = pointer.current_location();
            let under = self.under(at);
            let over_slots = under.as_ref().is_some_and(|u| u.over_slots);
            let layer = under.and_then(|u| u.layer);
            let slot = (!over_slots)
                .then(|| slot_at(&self.stacks, at.x as i32, at.y as i32))
                .flatten();
            if let Some((monitor, slot)) = &slot {
                self.stacks.focus_at(monitor, *slot);
                self.layer_focus = None;
            }
            if let Some(layer) = &layer {
                self.layer_clicked(layer);
            }
            if slot.is_some() || layer.is_some() {
                self.arrange();
            }
        }
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// Scroll, to the surface under the pointer.
    pub fn axis_input(&mut self, frame: AxisFrame) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// One input event from the window showing `output`, from this machine's devices (`forward`); pointer
    /// positions are local to that window.
    pub fn process_input_event<I: InputBackend>(&mut self, output: &str, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                self.device_key(event.key_code(), event.state(), Event::time_msec(&event));
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let output = self.outputs.iter().find(|o| o.name() == output);
                let Some(output_geo) = output.and_then(|o| self.space.output_geometry(o)) else {
                    return;
                };
                let pos = on_output(event.position_transformed(output_geo.size), output_geo.loc);
                self.device_pointer_at(pos, event.time_msec());
            }
            InputEvent::PointerButton { event } => {
                self.device_button(event.button_code(), event.state(), event.time_msec());
            }
            InputEvent::PointerAxis { event } => {
                let source = match event.source() {
                    AxisSource::Wheel => ScrollSource::Wheel,
                    AxisSource::Finger => ScrollSource::Finger,
                    AxisSource::Continuous => ScrollSource::Continuous,
                    AxisSource::WheelTilt => ScrollSource::WheelTilt,
                };
                // An amount for each axis that moved (a wheel's from its steps), its steps in 120ths, and whether a
                // lifted finger stopped it (a finger axis reported as 0).
                let [h, v] = [Axis::Horizontal, Axis::Vertical].map(|axis| {
                    let discrete = event.amount_v120(axis);
                    let amount = (event.amount(axis)).unwrap_or_else(|| discrete.unwrap_or(0.0) * 15.0 / 120.0);
                    let steps = discrete.filter(|_| amount != 0.0).map_or(0, |d| d as i32);
                    let stop = source == ScrollSource::Finger && event.amount(axis) == Some(0.0);
                    (amount, steps, stop)
                });
                self.device_axis(source, (h.0, v.0), (h.1, v.1), (h.2, v.2), event.time_msec());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Cycle};
    use crate::layout::Dir;
    use smithay::input::keyboard::keysyms as k;

    fn sym(raw: u32) -> Keysym {
        Keysym::new(raw)
    }

    const MOD: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        logo: true,
    };
    const MOD_SHIFT: Mods = Mods { shift: true, ..MOD };
    const MOD_CTRL: Mods = Mods { ctrl: true, ..MOD };

    fn action_for(mods: Mods, raw: u32) -> Option<Action> {
        let c = config::default_config();
        find_bind(&c.binds, mods, sym(raw)).map(|b| b.action.clone())
    }

    #[test]
    fn f_keys_with_mod_ctrl_or_ctrl_alt_switch_consoles() {
        let ctrl_alt = Mods {
            ctrl: true,
            alt: true,
            ..Mods::default()
        };
        for n in 1..=12u8 {
            let f = k::KEY_F1 + u32::from(n) - 1;
            assert_eq!(action_for(MOD_CTRL, f), Some(Action::Vt(n)));
            assert_eq!(action_for(ctrl_alt, f), Some(Action::Vt(n)));
        }
        assert_eq!(action_request(&Action::Vt(2)), None, "a key only");
    }

    #[test]
    fn mod_expands_to_the_configured_modifier() {
        assert_eq!(action_for(MOD, k::KEY_h), Some(Action::FocusSlot(Dir::Left)));
        // Alt is not Mod in the default config, and no modifier is no bind.
        let alt = Mods {
            alt: true,
            logo: false,
            ..MOD
        };
        assert_eq!(action_for(alt, k::KEY_h), None);
        assert_eq!(action_for(Mods::default(), k::KEY_h), None);
        // Extra modifiers are a different bind (or none).
        let alt_ctrl_shift = Mods {
            ctrl: true,
            ..MOD_SHIFT
        };
        assert_eq!(action_for(alt_ctrl_shift, k::KEY_h), None);

        let custom = config::parse(&config::DEFAULT.replace("mod \"Super\"", "mod \"Alt\"")).unwrap();
        assert!(find_bind(&custom.binds, alt, sym(k::KEY_h)).is_some());
        assert!(find_bind(&custom.binds, MOD, sym(k::KEY_h)).is_none());
    }

    #[test]
    fn shifted_binds_match_the_unshifted_sym() {
        // The raw sym of Shift+H is h, of Shift+Tab is Tab (its shifted sym, ISO_Left_Tab, must not be needed).
        assert_eq!(action_for(MOD_SHIFT, k::KEY_h), Some(Action::Move(Dir::Left)));
        assert_eq!(action_for(MOD, k::KEY_s), Some(Action::Split));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_s), Some(Action::ToggleStacking));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_Tab), Some(Action::CycleTab(Cycle::Prev)));
        assert_eq!(action_for(MOD, k::KEY_Tab), Some(Action::CycleTab(Cycle::Next)));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_e), Some(Action::Quit));
        assert_eq!(action_for(MOD, k::KEY_e), None, "Mod+E is not Mod+Shift+E");
        // Shift+1's raw sym is 1, not exclam; 0 is workspace 10; Ctrl+digits are the layouts.
        assert_eq!(action_for(MOD, k::KEY_1), Some(Action::Workspace(1)));
        assert_eq!(action_for(MOD, k::KEY_0), Some(Action::Workspace(10)));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_1), Some(Action::MoveToWorkspace(1)));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_0), Some(Action::MoveToWorkspace(10)));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_exclam), None);
        assert_eq!(action_for(MOD_CTRL, k::KEY_1), Some(Action::Layout("wall".into())));
        assert_eq!(action_for(MOD_CTRL, k::KEY_2), Some(Action::Layout("halves".into())));
    }

    #[test]
    fn case_does_not_matter() {
        assert_eq!(action_for(MOD, k::KEY_H), Some(Action::FocusSlot(Dir::Left)));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_Q), Some(Action::Close));
        assert_eq!(
            action_for(MOD, k::KEY_q),
            None,
            "close is Mod+Shift+Q, as Hyprland's killactive"
        );
        assert_eq!(action_for(MOD_CTRL, k::KEY_l), Some(Action::SwapSlot(Dir::Right)));
        assert_eq!(action_for(MOD_SHIFT, k::KEY_l), Some(Action::Move(Dir::Right)));
        assert_eq!(fold(sym(k::KEY_Return)), sym(k::KEY_Return));
        assert_eq!(fold(sym(k::KEY_Eacute)), sym(k::KEY_eacute));
        assert_eq!(fold(sym(k::KEY_1)), sym(k::KEY_1));
    }

    #[test]
    fn a_key_note_tells_what_the_bind_lookup_saw_and_never_plain_typing() {
        let focus = Action::FocusSlot(Dir::Left);
        assert_eq!(
            key_note(MOD, Some(sym(k::KEY_h)), Some(&focus)),
            KeyView {
                key: "h".into(),
                mods: "Super".into(),
                bind: Some("FocusSlot(Left)".into()),
            }
        );
        // Super alone arrives as its own press: that it shows proves the host passed it.
        assert_eq!(key_note(MOD, Some(sym(k::KEY_Super_L)), None).key, "Super_L");
        // A shortcut without a bind still names its key and every modifier.
        let all = Mods {
            ctrl: true,
            alt: true,
            shift: true,
            logo: true,
        };
        let note = key_note(all, Some(sym(k::KEY_q)), None);
        assert_eq!(
            (note.key.as_str(), note.mods.as_str(), note.bind),
            ("q", "Ctrl+Alt+Shift+Super", None)
        );
        // Typing (no Ctrl, Alt or Super, not a modifier key): which key is never kept.
        let none = Mods::default();
        for (mods, raw) in [
            (none, k::KEY_a),
            (Mods { shift: true, ..none }, k::KEY_A),
            (none, k::KEY_1),
        ] {
            assert_eq!(key_note(mods, Some(sym(raw)), None).key, "(plain key)");
        }
        assert_eq!(key_note(none, None, None).key, "(plain key)");
    }

    #[test]
    fn modifier_keys_are_held_lock_keys_are_not() {
        for raw in [
            k::KEY_Shift_L,
            k::KEY_Control_R,
            k::KEY_Alt_L,
            k::KEY_Meta_R,
            k::KEY_Super_L,
            k::KEY_Hyper_R,
            k::KEY_ISO_Level3_Shift,
            k::KEY_ISO_Level5_Shift,
            k::KEY_Mode_switch,
        ] {
            assert!(held_modifier(sym(raw)), "{raw:#x}");
        }
        for raw in [
            k::KEY_Caps_Lock,
            k::KEY_Shift_Lock,
            k::KEY_Num_Lock,
            k::KEY_ISO_Level3_Lock,
            k::KEY_ISO_Group_Lock,
            k::KEY_ISO_Level3_Latch,
            k::KEY_a,
            k::KEY_Escape,
        ] {
            assert!(!held_modifier(sym(raw)), "{raw:#x}");
        }
    }

    #[test]
    fn held_modifier_keys_make_a_binds_modifiers() {
        let of = |raws: &[u32]| mods_held(raws.iter().map(|r| sym(*r)));
        assert_eq!(of(&[]), Mods::default());
        assert_eq!(of(&[k::KEY_Super_R]), MOD);
        assert_eq!(of(&[k::KEY_Super_L, k::KEY_Shift_R]), MOD_SHIFT);
        assert_eq!(of(&[k::KEY_Control_L, k::KEY_Super_L]), MOD_CTRL);
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        assert_eq!(of(&[k::KEY_Alt_R]), alt);
        assert_eq!(of(&[k::KEY_Meta_L]), alt);
        assert_eq!(of(&[k::KEY_Hyper_L]), MOD, "Hyper is Mod4, as Super");
        assert_eq!(
            of(&[k::KEY_ISO_Level3_Shift]),
            Mods::default(),
            "AltGr names no bind modifier"
        );
    }

    #[test]
    fn a_bind_swallows_its_release_and_show_tabs_holds() {
        let mut h = Held::default();
        let (q, space, a) = (Keycode::new(24), Keycode::new(65), Keycode::new(38));
        assert!(matches!(
            h.press(q, Some(&Action::Close)),
            FilterResult::Intercept(Some(Action::Close))
        ));
        assert!(matches!(h.press(a, None), FilterResult::Forward));
        assert!(matches!(h.release(a), FilterResult::Forward));
        assert!(matches!(h.release(q), FilterResult::Intercept(None)));
        assert!(matches!(h.release(q), FilterResult::Forward), "only once");

        assert!(matches!(
            h.press(space, Some(&Action::ShowTabs)),
            FilterResult::Intercept(None)
        ));
        assert_eq!(h.show_tabs, Some(space));
        assert!(matches!(h.release(space), FilterResult::Intercept(None)));
        assert_eq!(h.show_tabs, None);
    }

    #[test]
    fn a_lost_release_does_not_swallow_a_later_one() {
        let mut h = Held::default();
        let q = Keycode::new(24);
        let _ = h.press(q, Some(&Action::Close));
        // Its release went to another window; the next press of q matches nothing (Mod is up).
        assert!(matches!(h.press(q, None), FilterResult::Forward));
        assert!(
            matches!(h.release(q), FilterResult::Forward),
            "the client saw the press; it sees the release"
        );
    }

    #[test]
    fn losing_focus_forgets_held_keys() {
        let mut h = Held::default();
        let (q, space) = (Keycode::new(24), Keycode::new(65));
        let _ = h.press(q, Some(&Action::Close));
        let _ = h.press(space, Some(&Action::ShowTabs));
        h.clear();
        assert_eq!(h.show_tabs, None);
        assert!(matches!(h.release(q), FilterResult::Forward));
    }

    #[test]
    fn hit_testing_finds_the_top_of_the_slot_under_the_point() {
        let c = config::default_config();
        let wall = c.layouts.iter().find(|l| l.name == "wall").unwrap().clone();
        let area = Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 900,
        };
        let rects = wall.rects(area);
        let mut s = Stacks::new();
        s.add_monitor("m", area, wall);
        s.open_stacked(1);
        s.open_stacked(2);
        s.focus_slot(Dir::Right);
        s.open_stacked(3);
        assert_eq!(top_at(&s, 10, 10), Some((2, rects[0])), "the top of slot 0");
        // Just across slot 0's right edge is the hero, whatever slot 0's window drew there.
        assert_eq!(top_at(&s, rects[0].w, 10), Some((3, rects[3])));
        assert_eq!(top_at(&s, rects[0].w - 1, 10), Some((2, rects[0])));
        assert_eq!(
            top_at(&s, 10, rects[0].h + 5),
            None,
            "an empty slot has nothing under the pointer"
        );
        assert_eq!(top_at(&s, 900, 10), None);
    }

    #[test]
    fn actions_map_to_requests() {
        assert_eq!(
            action_request(&Action::Layout("wall".into())),
            Some(Request::Layout {
                name: "wall".into(),
                monitor: None
            })
        );
        assert_eq!(action_request(&Action::Close), Some(Request::Close));
        assert_eq!(action_request(&Action::Quit), Some(Request::Quit));
        assert_eq!(
            action_request(&Action::SwapSlot(Dir::Up)),
            Some(Request::SwapSlot { dir: Dir::Up })
        );
        assert_eq!(
            action_request(&Action::Spawn(vec!["kitty".into()])),
            Some(Request::Spawn {
                argv: vec!["kitty".into()]
            })
        );
        assert_eq!(action_request(&Action::Split), Some(Request::Split));
        assert_eq!(action_request(&Action::ToggleStacking), Some(Request::ToggleStacking));
        assert_eq!(action_request(&Action::ShowTabs), None);
        assert_eq!(action_request(&Action::Reclaim), None);
        assert_eq!(
            action_request(&Action::Workspace(10)),
            Some(Request::Workspace { number: 10 })
        );
        assert_eq!(
            action_request(&Action::MoveToWorkspace(3)),
            Some(Request::MoveToWorkspace { number: 3 })
        );
    }

    #[test]
    fn focused_slot_follows_focus_at() {
        let c = config::default_config();
        let wall = c.layouts.iter().find(|l| l.name == "wall").unwrap().clone();
        let mut s = Stacks::new();
        let area = Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 900,
        };
        let last = wall.rects(area).len() - 1;
        s.add_monitor("m", area, wall);
        assert_eq!(focused_slot(&s).map(|(m, _)| m), Some("m"));
        s.focus_at("m", last);
        assert_eq!(focused_slot(&s), Some(("m", last)));
    }

    #[test]
    fn slot_at_finds_slots_and_misses_outside() {
        let c = config::default_config();
        let wall = c.layouts.iter().find(|l| l.name == "wall").unwrap().clone();
        let mut s = Stacks::new();
        let area = Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 900,
        };
        s.add_monitor("m", area, wall.clone());
        let rects = wall.rects(area);
        for (i, r) in rects.iter().enumerate() {
            assert_eq!(slot_at(&s, r.x, r.y), Some(("m".into(), i)), "top-left of slot {i}");
            assert_eq!(
                slot_at(&s, r.x + r.w - 1, r.y + r.h - 1),
                Some(("m".into(), i)),
                "bottom-right of slot {i}"
            );
        }
        assert_eq!(slot_at(&s, 900, 10), None);
        assert_eq!(slot_at(&s, -1, 10), None);
    }

    #[test]
    fn top_at_reaches_only_the_shown_workspace() {
        let c = config::default_config();
        let wall = c.layouts.iter().find(|l| l.name == "wall").unwrap().clone();
        let mut s = Stacks::new();
        let area = Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 900,
        };
        s.add_monitor("m", area, wall.clone());
        s.open_stacked(1);
        let r = wall.rects(area)[0];
        assert_eq!(top_at(&s, r.x, r.y), Some((1, r)));
        s.workspace(2);
        assert_eq!(top_at(&s, r.x, r.y), None, "1 is hidden");
        assert_eq!(slot_at(&s, r.x, r.y), Some(("m".into(), 0)), "the slot is still there");
    }

    #[test]
    fn pointer_positions_are_offset_by_their_outputs_place() {
        let local = Point::<f64, Logical>::from((10.5, 20.0));
        assert_eq!(
            on_output(local, (0, 0).into()),
            local,
            "the first screen sits at the origin"
        );
        assert_eq!(
            on_output(local, (1280, 0).into()),
            (1290.5, 20.0).into(),
            "the second right of it"
        );
    }
}
