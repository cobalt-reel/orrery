//! Input from a backend that has some (winit): a key press matching a bind runs its action (neither the press
//! nor its release reaches a client), other keys go to the focused window, the pointer to the surface under it,
//! and a click focuses the slot under it.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent,
};
use smithay::desktop::WindowSurfaceType;
use std::collections::HashSet;

use smithay::input::keyboard::{FilterResult, Keycode, Keysym, ModifiersState, xkb};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use super::Orrery;
use crate::config::{Action, Bind, Mods};
use crate::ipc::{KeyView, Request};
use crate::layout::Rect;
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

/// The control-socket request an action is, so keys and `orrery msg` take one path. Show-tabs is a key only.
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
        Action::Close => Request::Close,
        Action::Quit => Request::Quit,
        Action::ShowTabs => return None,
    })
}

/// The monitor and slot whose rectangle holds the point, empty slots included.
#[must_use]
pub fn slot_at(stacks: &Stacks, x: i32, y: i32) -> Option<(String, usize)> {
    let inside = |r: Rect| x >= r.x && y >= r.y && x - r.x < r.w && y - r.y < r.h;
    stacks.monitors().iter().find_map(|m| {
        if !inside(m.area()) {
            return None;
        }
        let slot = m.layout().rects(m.area()).into_iter().position(inside)?;
        Some((m.name().to_owned(), slot))
    })
}

/// The top window of the slot holding the point, and that slot's rectangle.
#[must_use]
pub fn top_at(stacks: &Stacks, x: i32, y: i32) -> Option<(WinId, Rect)> {
    let (name, slot) = slot_at(stacks, x, y)?;
    let mon = stacks.monitors().iter().find(|m| m.name() == name)?;
    let top = mon.slots().get(slot)?.top()?;
    let rect = *mon.layout().rects(mon.area()).get(slot)?;
    Some((top, rect))
}

impl Orrery {
    /// The surface under `pos`, as drawn: only the top window of the slot holding `pos`, and only its parts
    /// inside that slot (render clips everything, popups included, to the slot).
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (id, slot) = top_at(&self.stacks, pos.x.floor() as i32, pos.y.floor() as i32)?;
        let window = self.windows.get(&id)?;
        // Where render puts the window's surfaces: its geometry origin at the slot's.
        let origin = Point::<i32, Logical>::from((slot.x, slot.y)) - window.geometry().loc;
        window
            .surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, p)| (s, (p + origin).to_f64()))
    }

    /// Decides a key's fate (see `Held`).
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

    /// A bind's action. Failures are logged: a key has nowhere to answer.
    pub fn run_action(&mut self, action: Action) {
        if let Some(request) = action_request(&action) {
            let response = self.handle_request(request);
            if let Some(e) = response.error {
                tracing::info!("{action:?}: {e}");
            }
        }
    }

    /// The host window lost keyboard focus: no release will arrive for what is down now. Release every pressed
    /// key through the usual path (a swallowed key stays swallowed, others reach the client), so neither the
    /// seat's modifier state nor a client is left with Alt held, then forget held binds.
    pub fn keyboard_focus_lost(&mut self) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            let time = u32::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u32::MAX);
            for code in keyboard.pressed_keys() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.input::<Option<Action>, _>(self, code, KeyState::Released, serial, time, |orrery, _, _| {
                    orrery.held.release(code)
                });
            }
        }
        self.held.clear();
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();
                let action = keyboard
                    .input::<Option<Action>, _>(self, event.key_code(), key_state, serial, time, |orrery, mods, key| {
                        orrery.filter_key(key_state, mods, key.raw_code(), key.raw_latin_sym_or_raw_current_sym())
                    })
                    .flatten();
                // Run outside the filter: actions re-focus the keyboard, which must not happen mid-input.
                if let Some(action) = action {
                    self.run_action(action);
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(output_geo) = self.outputs.first().and_then(|o| self.space.output_geometry(o)) else {
                    return;
                };
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let under = self.surface_under(pos);
                self.pointer_seen = true;
                self.pointer_target = under.clone();
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerButton { event } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                if event.state() == ButtonState::Pressed && !pointer.is_grabbed() {
                    // A click focuses the slot under the pointer (empty or not), through the stacks.
                    let at = pointer.current_location();
                    if let Some((monitor, slot)) = slot_at(&self.stacks, at.x as i32, at.y as i32) {
                        self.stacks.focus_at(&monitor, slot);
                        self.arrange();
                    }
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let source = event.source();
                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let discrete = event.amount_v120(axis);
                    let amount = event
                        .amount(axis)
                        .unwrap_or_else(|| discrete.unwrap_or(0.0) * 15.0 / 120.0);
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                        if let Some(d) = discrete {
                            frame = frame.v120(axis, d as i32);
                        }
                    }
                    if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }
                pointer.axis(self, frame);
                pointer.frame(self);
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
}
