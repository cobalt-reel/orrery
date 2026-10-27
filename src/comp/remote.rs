//! Input from the primary, on a satellite holding the session's focus marker: what it sends runs through this
//! machine's own input path (`input`), binds included, on this machine's clock; everything it pressed is released
//! when input leaves (a Release, or the link drops), and this machine's keymap is back, on the layout it was on. The
//! primary's keymap comes once per link (and when it changes) and is used again at each Handoff. Its pointer moves
//! here by relative motion, kept on this machine's screens; pushed out past their outer edges it stops at the edge,
//! holds still until the next Handoff, and the primary is told where it left (InputExit).
//!
//! Also `inject`, a testing aid: input as if from this machine's own devices (`forward`).

use std::collections::BTreeSet;
use std::sync::PoisonError;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{Keycode, Layout, xkb};
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
    /// This machine's keymap (xkb text) and the layout it was on, while the primary's is in use.
    local_keymap: Option<(String, Layout)>,
    /// The primary's keymap, as last sent: used at each Handoff, until the link drops.
    primary_keymap: Option<String>,
    /// The pointer left past an outer edge (InputExit sent): it holds still until the next Handoff, so motion still
    /// on its way here never pulls it back.
    exited: bool,
    /// The primary's input is here (a Handoff, until a Release or the link drops): orrery draws the pointer.
    active: bool,
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

/// Scroll: amounts, wheel steps in 120ths (0: none), and which axes a lifted finger stopped.
pub(super) fn axis_frame(
    source: ScrollSource,
    (h, v): (f64, f64),
    (v120_h, v120_v): (i32, i32),
    (stop_h, stop_v): (bool, bool),
    time: u32,
) -> AxisFrame {
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
    for (axis, stop) in [(Axis::Horizontal, stop_h), (Axis::Vertical, stop_v)] {
        if stop {
            frame = frame.stop(axis);
        }
    }
    frame
}

/// The axes scroll from the link stops: the link carries no stops, so a finger frame with nothing on either axis
/// (the finger lifted) stops both.
pub(super) fn remote_stops(source: ScrollSource, (h, v): (f64, f64)) -> (bool, bool) {
    let lifted = source == ScrollSource::Finger && h == 0.0 && v == 0.0;
    (lifted, lifted)
}

impl Orrery {
    /// This machine's screens, as global rectangles.
    pub(super) fn screen_rects(&self) -> Vec<Rect> {
        self.stacks.monitors().iter().map(|m| m.area()).collect()
    }

    /// An input message from the primary (`Message::is_input`): Ok with the message to answer (an InputExit), or
    /// Err when the link must close (a keymap that does not compile). Its times are not used: clients get this
    /// machine's clock.
    pub(super) fn remote_input(&mut self, msg: Message) -> Result<Option<Message>, String> {
        let time = self.clock();
        match msg {
            Message::Keymap { xkb } => {
                self.remote_keymap(xkb.0.clone())?;
                self.remote.primary_keymap = Some(xkb.0);
            }
            Message::Handoff { mods, pointer, layout } => {
                self.remote.exited = false;
                self.remote.active = true;
                if self.remote.local_keymap.is_none()
                    && let Some(text) = self.remote.primary_keymap.clone()
                    && let Err(e) = self.remote_keymap(text)
                {
                    tracing::warn!("cannot use the primary's keymap again: {e}");
                }
                if self.remote.local_keymap.is_some() {
                    self.select_layout(Layout(layout));
                }
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
            } => {
                let stops = remote_stops(source, (h, v));
                self.axis_input(axis_frame(source, (h, v), (v120_h, v120_v), stops, time));
            }
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
            None => self.keymap(),
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
    pub(super) fn step(&self, dx: f64, dy: f64) -> Option<(Option<crossing::Exit>, (f64, f64))> {
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
        if let Some((local, layout)) = self.remote.local_keymap.take()
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            match keyboard.set_keymap_from_string(self, local) {
                // A new keymap starts on its first layout: back to the one it was on.
                Ok(()) => self.select_layout(layout),
                Err(e) => tracing::warn!("cannot go back to this machine's keymap: {e}"),
            }
        }
        self.remote.exited = false;
        self.remote.active = false;
    }

    /// The primary's input is here: the pointer it moves is drawn here (`render::cursor_draw`).
    #[must_use]
    pub fn remote_input_active(&self) -> bool {
        self.remote.active
    }

    /// The keymap in use to its layout `layout`, if it has that many.
    fn select_layout(&mut self, layout: Layout) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        keyboard.with_xkb_state(self, |mut context| {
            let layouts = (context.xkb().lock().unwrap_or_else(PoisonError::into_inner))
                .layouts()
                .count();
            if usize::try_from(layout.0).is_ok_and(|l| l < layouts) {
                context.set_layout(layout);
            }
        });
    }

    /// The link to the primary dropped, joined or not: everything it had down here goes up, and its keymap is
    /// forgotten (the next link sends it again).
    pub(super) fn remote_lost(&mut self) {
        self.remote_release();
        self.remote.primary_keymap = None;
    }

    /// This machine's keymap as xkb text, and the layout it is on.
    pub(super) fn keymap(&mut self) -> Option<(String, Layout)> {
        let keyboard = self.seat.get_keyboard()?;
        Some(keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap_or_else(PoisonError::into_inner);
            // Safety: the keymap is only borrowed to write it out; no reference to it outlives `xkb`.
            let text = unsafe { xkb.keymap() }.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
            (text, xkb.active_layout())
        }))
    }

    /// `inject` (a testing aid; the caller checked `--debug-inject`): `input` as if from this machine's own
    /// devices, forwarded as theirs is (`forward`). Its own pointer stays on its screens: past an outer edge, it stops
    /// there, or crosses to the machine beside it.
    pub(super) fn inject(&mut self, input: Message) -> Response {
        let time = self.clock();
        match input {
            Message::Key { keycode, pressed, .. } => self.device_key(Keycode::new(keycode.0), key_state(pressed), time),
            Message::Motion { dx, dy } => self.device_motion(dx, dy, true),
            Message::Button { code, pressed } => self.device_button(code, button_state(pressed), time),
            Message::Axis {
                source,
                h,
                v,
                v120_h,
                v120_v,
            } => {
                let stops = remote_stops(source, (h, v));
                self.device_axis(source, (h, v), (v120_h, v120_v), stops, time);
            }
            _ => return Response::error("`inject` takes a key, motion, button or axis"),
        }
        Response::ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finger_lifted_stops_the_axes_it_names() {
        // From this machine's own touchpad: each axis that ended stops, on its own.
        let f = axis_frame(ScrollSource::Finger, (0.0, 4.0), (0, 0), (true, false), 9);
        assert_eq!((f.axis, f.stop, f.time), ((0.0, 4.0), (true, false), 9));
        assert_eq!(f.source, Some(AxisSource::Finger));
        // A wheel's steps go along with its amounts; nothing stops.
        let w = axis_frame(ScrollSource::Wheel, (0.0, 15.0), (0, 120), (false, false), 1);
        assert_eq!((w.axis, w.v120, w.stop), ((0.0, 15.0), Some((0, 120)), (false, false)));
        // From the link (which carries no stops): a finger frame with nothing on either axis stops both.
        assert_eq!(remote_stops(ScrollSource::Finger, (0.0, 0.0)), (true, true));
        assert_eq!(remote_stops(ScrollSource::Finger, (0.0, 2.0)), (false, false));
        assert_eq!(remote_stops(ScrollSource::Wheel, (0.0, 0.0)), (false, false));
    }
}
