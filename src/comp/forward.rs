//! Input from this machine's own devices (a backend, `inject`): here, through the usual path (`input`), or, on the
//! primary while the session's focus marker is on another machine, over the link to it (`Crossing` decides, each
//! machine's `Outbox` in `net` carries it). The escape key (`reclaim`) is matched first and never forwarded. Input
//! follows the marker: a `workspace` that moves it, the pointer pushed past this machine's outer edges or past a
//! satellite's (its InputExit), the escape key, a link drop.
//!
//! A machine alone or a satellite has no `Crossing`: its devices' input stays here.

use smithay::backend::input::{ButtonState, KeyState};
use smithay::input::keyboard::{Keycode, Keysym};
use smithay::utils::{Logical, Point};

use super::Orrery;
use super::input::{find_bind, held_modifier, key_note, mods_held};
use super::remote::axis_frame;
use crate::config::Action;
use crate::crossing::{self, Edge, Handoff, Place, RawKey};
use crate::link::{Message, ScrollSource};

impl Orrery {
    /// Input is on another machine: this one's pointer is held (nested: the host's is hidden and locked).
    #[must_use]
    pub fn input_away(&self) -> bool {
        matches!(self.input_place(), Some(Place::Remote(_)))
    }

    /// A key of this machine's keyboard went down or up (repeats never come here: receivers repeat keys themselves).
    pub fn device_key(&mut self, code: Keycode, state: KeyState, time: u32) {
        if self.crossing().is_none() {
            return self.key_input(code, state, time);
        }
        let pressed = state == KeyState::Pressed;
        let sym = self.raw_sym(code);
        if pressed && let Some(sym) = sym {
            let mods = mods_held(self.held_syms());
            if find_bind(&self.config.binds, mods, sym).is_some_and(|b| b.action == Action::Reclaim) {
                // Not recorded in the crossing: its release goes nowhere.
                self.input_reclaim();
                self.last_key = Some(key_note(mods, Some(sym), Some(&Action::Reclaim)));
                return;
            }
        }
        let modifier = sym.is_some_and(held_modifier);
        let key = RawKey(code.raw());
        match self.crossing().and_then(|c| c.key(key, pressed, modifier)) {
            None => {}
            Some(Place::Local) => self.key_input(code, state, time),
            Some(Place::Remote(m)) => self.queue_input(
                &m,
                Message::Key {
                    keycode: key,
                    pressed,
                    time,
                },
            ),
        }
    }

    /// Relative motion of this machine's pointer: over the link while input is away; here it moves the pointer
    /// (when `moves`; a backend that reports positions too moves it by those) and, pushed past this machine's outer
    /// edges, crosses to the machine beside them.
    pub fn device_motion(&mut self, dx: f64, dy: f64, moves: bool) {
        if let Some(Place::Remote(m)) = self.input_place().cloned() {
            return self.queue_input(&m, Message::Motion { dx, dy });
        }
        let Some((exit, at)) = self.step(dx, dy) else {
            return;
        };
        if moves || exit.is_some() {
            self.pointer_to(at.into(), self.clock());
        }
        if let Some(out) = exit
            && let Some(here) = self.this_machine()
        {
            self.input_cross(&here, out.edge, out.y);
        }
    }

    /// This machine's pointer at `pos` (global), from a backend that reports positions: ignored while input is away
    /// (the pointer is held, and moves the other machine's by relative motion).
    pub fn device_pointer_at(&mut self, pos: Point<f64, Logical>, time: u32) {
        if !self.input_away() {
            self.pointer_to(pos, time);
        }
    }

    pub fn device_button(&mut self, code: u32, state: ButtonState, time: u32) {
        let pressed = state == ButtonState::Pressed;
        match self.crossing().map(|c| c.button(code, pressed)) {
            None | Some(Some(Place::Local)) => self.button_input(code, state, time),
            Some(None) => {}
            Some(Some(Place::Remote(m))) => self.queue_input(&m, Message::Button { code, pressed }),
        }
    }

    /// Scroll: amounts, wheel steps in 120ths (0: none), and which axes a lifted finger stopped (here only: the
    /// link carries no stops, its receiver reads a finger frame with nothing on either axis as both).
    pub fn device_axis(
        &mut self,
        source: ScrollSource,
        (h, v): (f64, f64),
        (v120_h, v120_v): (i32, i32),
        stops: (bool, bool),
        time: u32,
    ) {
        match self.input_place().cloned() {
            Some(Place::Remote(m)) => self.queue_input(
                &m,
                Message::Axis {
                    source,
                    h,
                    v,
                    v120_h,
                    v120_v,
                },
            ),
            _ => self.axis_input(axis_frame(source, (h, v), (v120_h, v120_v), stops, time)),
        }
    }

    /// The escape key: keyboard and pointer back here, and the marker with them. Nothing when they are here.
    pub fn input_reclaim(&mut self) {
        if let Some(h) = self.crossing().and_then(|c| c.reclaim()) {
            self.input_moved(h);
        }
    }

    /// The session's focus marker may have moved (a `workspace`): input goes where it is now.
    pub(super) fn input_follow_marker(&mut self) {
        let Some(to) = self.marker() else {
            return;
        };
        if let Some(h) = self.crossing().and_then(|c| c.handoff(&to, None)) {
            self.input_moved(h);
        }
    }

    /// Machine `from`'s pointer left its screens past `edge` at height `y`: onto the machine beside it there, the
    /// marker along. While input is on a satellite that never lands (past the row's end, or a button down), it is
    /// handed input again, or its pointer would stay held at the edge. Only from where input is: an exit sent before
    /// input left that machine moves nothing.
    pub(super) fn input_cross(&mut self, from: &str, edge: Edge, y: f64) {
        let here = self.this_machine();
        let at_from = match self.input_place() {
            Some(Place::Local) => here.as_deref() == Some(from),
            Some(Place::Remote(m)) => m == from,
            None => false,
        };
        if !at_from {
            return;
        }
        let landing = self.landing(from, edge, y);
        let crossed = landing.and_then(|l| {
            let h = self.crossing()?.cross(&l.machine, l.entry)?;
            Some((l.machine, h))
        });
        match crossed {
            Some((to, h)) => {
                self.focus_marker(&to);
                self.input_go(h);
            }
            None => {
                if let Some(Place::Remote(m)) = self.input_place().cloned()
                    && m == from
                {
                    let mods = self.crossing().map(|c| c.held()).unwrap_or_default();
                    let layout = self.keymap().map_or(0, |(_, l)| l.0);
                    let handoff = Message::Handoff {
                        mods,
                        pointer: None,
                        layout,
                    };
                    self.queue_input(&m, handoff);
                }
            }
        }
    }

    /// Satellite `machine`'s link dropped: input comes back here if it was there (it releases what it has down on
    /// its own drop). The session moved the marker home itself.
    pub(super) fn input_lost(&mut self, machine: &str) {
        if let Some(h) = self.crossing().and_then(|c| c.link_lost(machine)) {
            self.input_go(h);
        }
    }

    /// A handoff the marker did not make: the marker follows input.
    fn input_moved(&mut self, h: Handoff) {
        let to = match &h.to {
            Place::Local => self.this_machine(),
            Place::Remote(m) => Some(m.clone()),
        };
        if let Some(to) = to {
            self.focus_marker(&to);
        }
        self.input_go(h);
    }

    /// Carries out a handoff: what input had down here goes up, the machine it left is told (Release), and the
    /// modifier keys held go down where it goes: here, or there after this machine's keymap (when that machine does
    /// not have it yet) and a Handoff, through its `Outbox`, so nothing overtakes them.
    fn input_go(&mut self, h: Handoff) {
        let time = self.clock();
        self.release_keys(h.release_keys.iter().map(|k| Keycode::new(k.0)));
        for &code in &h.release_buttons {
            self.button_input(code, ButtonState::Released, time);
        }
        if let Some(m) = &h.release {
            self.queue_input(m, Message::Release);
        }
        let keymap = self.keymap();
        match h.message(keymap.as_ref().map_or(0, |(_, l)| l.0)) {
            None => {
                tracing::info!("input is here");
                for k in &h.held {
                    self.key_input(Keycode::new(k.0), KeyState::Pressed, time);
                }
                if let Some(entry) = h.entry
                    && let Some((x, y)) = crossing::enter(&self.screen_rects(), entry)
                {
                    self.pointer_to((x, y).into(), time);
                }
            }
            Some((to, handoff)) => {
                tracing::info!("input goes to {to}");
                if !self.queue_handoff(&to, keymap.map(|(text, _)| text), handoff) {
                    // Gone meanwhile (its drop not handled yet): as if handled now.
                    self.input_lost(&to);
                }
            }
        }
    }

    /// The keysym a key makes on its own (no modifiers, the layout in use).
    fn raw_sym(&mut self, code: Keycode) -> Option<Keysym> {
        let keyboard = self.seat.get_keyboard()?;
        keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            xkb.raw_syms_for_key_in_layout(code, xkb.active_layout())
                .first()
                .copied()
        })
    }

    /// The keysyms of the modifier keys held, wherever input is.
    fn held_syms(&mut self) -> Vec<Keysym> {
        let held = self.crossing().map(|c| c.held()).unwrap_or_default();
        held.into_iter()
            .filter_map(|k| self.raw_sym(Keycode::new(k.0)))
            .collect()
    }
}
