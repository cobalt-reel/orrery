//! Input from the primary, on a satellite holding the session's focus marker: what it sends runs through this
//! machine's own input path (`input`), binds included, on this machine's clock; everything it pressed is released
//! when input leaves (a Release, or the link drops), and this machine's keymap is back. Its pointer moves here by
//! relative motion, kept on this machine's screens; pushed out past their outer edges it stops at the edge, holds
//! still until the next Handoff, and the primary is told where it left (InputExit).
//!
//! Also `inject`, a testing aid: input as if from this machine's own devices.

use std::collections::BTreeSet;
use std::sync::PoisonError;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{Keycode, xkb};
use smithay::input::pointer::AxisFrame;

use super::Orrery;
use crate::crossing::{self, Step};
use crate::ipc::Response;
use crate::layout::Rect;
use crate::link::{Message, ScrollSource};

/// What the primary's input has down here. No Debug: it holds keycodes, and keystrokes are never logged.
#[derive(Default)]
pub struct Remote {
    /// Keys (xkb keycodes) and buttons it pressed and has not released.
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    /// This machine's keymap (xkb text) while the primary's is in use.
    local_keymap: Option<String>,
    /// The pointer left past an outer edge (InputExit sent): it holds still until the next Handoff, so motion still
    /// on its way here never pulls it back.
    exited: bool,
}

impl Remote {
    /// Its keys were released another way (`keyboard_focus_lost`): their releases from the primary go nowhere.
    pub fn forget_keys(&mut self) {
        self.keys.clear();
    }
}

fn key_state(pressed: bool) -> KeyState {
    if pressed { KeyState::Pressed } else { KeyState::Released }
}

fn button_state(pressed: bool) -> ButtonState {
    if pressed {
        ButtonState::Pressed
    } else {
        ButtonState::Released
    }
}

/// Scroll as the link carries it: amounts, and wheel steps in 120ths (0: none). A finger lifted (nothing on either
/// axis) stops both.
fn axis_frame(source: ScrollSource, (h, v): (f64, f64), (v120_h, v120_v): (i32, i32), time: u32) -> AxisFrame {
    let source = match source {
        ScrollSource::Wheel => AxisSource::Wheel,
        ScrollSource::Finger => AxisSource::Finger,
        ScrollSource::Continuous => AxisSource::Continuous,
        ScrollSource::WheelTilt => AxisSource::WheelTilt,
    };
    let mut frame = AxisFrame::new(time).source(source);
    for (axis, amount, v120) in [(Axis::Horizontal, h, v120_h), (Axis::Vertical, v, v120_v)] {
        if amount != 0.0 {
            frame = frame.value(axis, amount);
        }
        if v120 != 0 {
            frame = frame.v120(axis, v120);
        }
    }
    if source == AxisSource::Finger && h == 0.0 && v == 0.0 {
        frame = frame.stop(Axis::Horizontal).stop(Axis::Vertical);
    }
    frame
}

impl Orrery {
    /// This machine's screens, as global rectangles.
    fn screen_rects(&self) -> Vec<Rect> {
        self.stacks.monitors().iter().map(|m| m.area()).collect()
    }

    /// An input message from the primary (`Message::is_input`): Ok with the message to answer (an InputExit), or
    /// Err when the link must close (a keymap that does not compile). Its times are not used: clients get this
    /// machine's clock.
    pub(super) fn remote_input(&mut self, msg: Message) -> Result<Option<Message>, String> {
        let time = self.clock();
        match msg {
            Message::Keymap { xkb } => self.remote_keymap(xkb.0)?,
            Message::Handoff { mods, pointer } => {
                self.remote.exited = false;
                for key in mods {
                    self.remote_key(key.0, true);
                }
                if let Some(entry) = pointer
                    && let Some((x, y)) = crossing::enter(&self.screen_rects(), entry)
                {
                    self.pointer_to((x, y).into(), time);
                }
            }
            Message::Key { keycode, pressed, .. } => self.remote_key(keycode.0, pressed),
            Message::Motion { dx, dy } => return Ok(self.remote_motion(dx, dy)),
            Message::Button { code, pressed } => {
                let went = match pressed {
                    true => self.remote.buttons.insert(code),
                    false => self.remote.buttons.remove(&code),
                };
                if went {
                    self.button_input(code, button_state(pressed), time);
                }
            }
            Message::Axis {
                source,
                h,
                v,
                v120_h,
                v120_v,
            } => self.axis_input(axis_frame(source, (h, v), (v120_h, v120_v), time)),
            Message::Release => self.remote_release(),
            _ => {}
        }
        Ok(None)
    }

    /// A key from the primary: a press of a key it has down already (a modifier a Handoff announced again), or a
    /// release of one it does not, goes nowhere.
    fn remote_key(&mut self, code: u32, pressed: bool) {
        let went = match pressed {
            true => self.remote.keys.insert(code),
            false => self.remote.keys.remove(&code),
        };
        if went {
            self.key_input(Keycode::new(code), key_state(pressed), self.clock());
        }
    }

    /// The primary's keymap, for its keys from now on; this machine's own is kept to go back to.
    fn remote_keymap(&mut self, text: String) -> Result<(), String> {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return Ok(());
        };
        let local = match self.remote.local_keymap {
            Some(_) => None,
            None => Some(keyboard.with_xkb_state(self, |context| {
                let xkb = context.xkb().lock().unwrap_or_else(PoisonError::into_inner);
                // Safety: the keymap is only borrowed to write it out; no reference to it outlives `xkb`.
                unsafe { xkb.keymap() }.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            })),
        };
        keyboard
            .set_keymap_from_string(self, text)
            .map_err(|_| "a keymap that does not compile".to_owned())?;
        if local.is_some() {
            self.remote.local_keymap = local;
        }
        Ok(())
    }

    /// Relative motion from the primary: the pointer moves on this machine's screens, or to an outer edge and
    /// out (the InputExit to send), then holds still until the next Handoff.
    fn remote_motion(&mut self, dx: f64, dy: f64) -> Option<Message> {
        if self.remote.exited {
            return None;
        }
        let (exit, (x, y)) = self.step(dx, dy)?;
        self.pointer_to((x, y).into(), self.clock());
        let out = exit?;
        self.remote.exited = true;
        Some(Message::InputExit {
            edge: out.edge,
            y: out.y,
        })
    }

    /// Where relative motion takes the pointer (`crossing::step`), and the exit when it leaves.
    fn step(&self, dx: f64, dy: f64) -> Option<(Option<crossing::Exit>, (f64, f64))> {
        let at = self.seat.get_pointer()?.current_location();
        Some(match crossing::step(&self.screen_rects(), (at.x, at.y), (dx, dy))? {
            Step::To(x, y) => (None, (x, y)),
            Step::Exit { out, at } => (Some(out), at),
        })
    }

    /// Input left (a Release, or the link to the primary dropped): every key and button the primary has down is
    /// released, through the usual path, and this machine's keymap is back.
    pub(super) fn remote_release(&mut self) {
        let keys = std::mem::take(&mut self.remote.keys);
        self.release_keys(keys.into_iter().map(Keycode::new));
        let time = self.clock();
        for code in std::mem::take(&mut self.remote.buttons) {
            self.button_input(code, ButtonState::Released, time);
        }
        if let Some(local) = self.remote.local_keymap.take()
            && let Some(keyboard) = self.seat.get_keyboard()
            && let Err(e) = keyboard.set_keymap_from_string(self, local)
        {
            tracing::warn!("cannot go back to this machine's keymap: {e}");
        }
        self.remote.exited = false;
    }

    /// `inject` (a testing aid; the caller checked `--debug-inject`): `input` as if from this machine's own
    /// devices. Its own pointer stays on its screens: past an outer edge, it stops there.
    pub(super) fn inject(&mut self, input: Message) -> Response {
        let time = self.clock();
        match input {
            Message::Key { keycode, pressed, .. } => self.key_input(Keycode::new(keycode.0), key_state(pressed), time),
            Message::Motion { dx, dy } => {
                if let Some((_, (x, y))) = self.step(dx, dy) {
                    self.pointer_to((x, y).into(), time);
                }
            }
            Message::Button { code, pressed } => self.button_input(code, button_state(pressed), time),
            Message::Axis {
                source,
                h,
                v,
                v120_h,
                v120_v,
            } => self.axis_input(axis_frame(source, (h, v), (v120_h, v120_v), time)),
            _ => return Response::error("`inject` takes a key, motion, button or axis"),
        }
        Response::ok()
    }
}
