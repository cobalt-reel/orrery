//! Where input goes: the keyboard and pointer of the machine the user is at stay here, or travel over the link to
//! the machine holding the session's focus marker.
//!
//! Pure. Keyboard and pointer go to one place, here (`Place::Local`) or another machine. Every key and button is
//! kept from press to release on the machine it went to. A handoff releases everything down on the machine input
//! leaves, and presses on the one it goes to only the modifier keys still held: no key is ever down on two machines,
//! no stuck keys, no missing Super. A release whose press stayed behind goes nowhere. Keycodes never show in Debug
//! (`RawKey`): keystrokes are never logged.
//!
//! The pointer crosses at the edges of a row of screens (`RowScreen`); motion on its way to a machine is coalesced
//! while the link cannot take more (`Outbox`). On a machine, relative motion moves its pointer over its own screens
//! and out at their outer edges (`step`), and the pointer comes in at one (`enter`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::layout::Rect;
use crate::link::{Message, ScrollSource};

/// A key by its raw keycode (xkb's: evdev's plus 8). Debug never shows which: keystrokes are never logged.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawKey(pub u32);

impl fmt::Debug for RawKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RawKey(..)")
    }
}

/// Where input goes: here, or the machine of that name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Local,
    Remote(String),
}

/// The edge of a machine's screens the pointer enters at: the left of its first, or the right of its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Edge {
    Left,
    Right,
}

/// Where the pointer enters a machine: at `edge`, at height `y` as a fraction of the screen's (0 the top, 1 the
/// bottom), so each end scales it to its own screen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub edge: Edge,
    pub y: f64,
}

impl Entry {
    /// The point it enters at on a `width` by `height` screen: its edge column, its height scaled, both inside.
    #[must_use]
    pub fn point(&self, width: u32, height: u32) -> (f64, f64) {
        let x = match self.edge {
            Edge::Left => 0.0,
            Edge::Right => f64::from(width.saturating_sub(1)),
        };
        let bottom = f64::from(height.saturating_sub(1));
        (x, (unit(self.y) * f64::from(height)).min(bottom))
    }
}

/// Where keyboard and pointer go, and what is down there.
#[derive(Debug, Clone)]
pub struct Crossing {
    /// This machine's name in the session.
    name: String,
    place: Place,
    /// Keys down where input is, each with whether it is a modifier key. Only there: a handoff releases or moves
    /// every one.
    keys: BTreeMap<RawKey, bool>,
    /// Buttons down where input is, by code.
    buttons: BTreeSet<u32>,
}

/// What a handoff does, in order: release what the side input left has down, then press the modifier keys still
/// held on the side it goes to.
#[derive(Debug, Clone, PartialEq)]
pub struct Handoff {
    /// Keys and buttons to release here (to the seat and clients): input left this machine.
    pub release_keys: Vec<RawKey>,
    pub release_buttons: Vec<u32>,
    /// The machine input left, to send a `Message::Release` (it releases what it has down itself). None when input
    /// left this machine, or that machine's link is gone.
    pub release: Option<String>,
    pub to: Place,
    /// The modifier keys held, to press where input goes: here, or there through `message`.
    pub held: Vec<RawKey>,
    /// Where the pointer enters; none: its cursor stays where it is.
    pub entry: Option<Entry>,
}

impl Handoff {
    /// The machine input goes to and the `Message::Handoff` to send it, with the keyboard `layout` this machine is
    /// on; none when input came here.
    #[must_use]
    pub fn message(&self, layout: u32) -> Option<(String, Message)> {
        let Place::Remote(to) = &self.to else {
            return None;
        };
        let msg = Message::Handoff {
            mods: self.held.clone(),
            pointer: self.entry,
            layout,
        };
        Some((to.clone(), msg))
    }
}

impl Crossing {
    /// Input here, nothing down.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            place: Place::Local,
            keys: BTreeMap::new(),
            buttons: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn place(&self) -> &Place {
        &self.place
    }

    /// A key of this machine's keyboard went down or up (`modifier`: Shift, Ctrl, Alt, Super and the like): where it
    /// goes. None for a press of a key already down (receivers repeat keys themselves) or a release whose press
    /// stayed behind at a handoff. Lock keys (Caps, Num, Scroll) are never modifiers here: lock and latch state is
    /// not carried, each machine keeps its own.
    pub fn key(&mut self, key: RawKey, pressed: bool, modifier: bool) -> Option<Place> {
        let went = match pressed {
            true => self.keys.insert(key, modifier).is_none(),
            false => self.keys.remove(&key).is_some(),
        };
        went.then(|| self.place.clone())
    }

    /// The modifier keys held, wherever input is: what a handoff presses where it goes, and what the escape key is
    /// matched with.
    #[must_use]
    pub fn held(&self) -> Vec<RawKey> {
        (self.keys.iter()).filter(|(_, m)| **m).map(|(k, _)| *k).collect()
    }

    /// This machine's keyboard lost its keys without their releases (the host took its focus): none is down any
    /// more, so each one's next press goes where input is instead of reading as a repeat. Buttons stay.
    pub fn forget_keys(&mut self) {
        self.keys.clear();
    }

    /// A button of this machine's pointer went down or up: where it goes, as for `key`.
    pub fn button(&mut self, code: u32, pressed: bool) -> Option<Place> {
        let went = match pressed {
            true => self.buttons.insert(code),
            false => self.buttons.remove(&code),
        };
        went.then(|| self.place.clone())
    }

    /// Input goes to machine `to` (this one by its own name), the pointer entering at `entry`: the focus marker
    /// moved. None when it is there already.
    pub fn handoff(&mut self, to: &str, entry: Option<Entry>) -> Option<Handoff> {
        self.go(to, entry, true)
    }

    /// The pointer crossed an edge onto machine `to` (`exit`, `landing`): a handoff entering at `entry`. None while
    /// a button is down, so a drag stays on its machine.
    pub fn cross(&mut self, to: &str, entry: Entry) -> Option<Handoff> {
        if !self.buttons.is_empty() {
            return None;
        }
        self.go(to, Some(entry), true)
    }

    /// The escape key: input comes back here. None when it is here.
    pub fn reclaim(&mut self) -> Option<Handoff> {
        let here = self.name.clone();
        self.go(&here, None, true)
    }

    /// Machine `machine`'s link dropped: if input was there, it comes back here, with no Release to send (that
    /// machine releases what it has down on its own drop). None otherwise.
    pub fn link_lost(&mut self, machine: &str) -> Option<Handoff> {
        if !matches!(&self.place, Place::Remote(m) if m == machine) {
            return None;
        }
        let here = self.name.clone();
        self.go(&here, None, false)
    }

    /// Input to `to`; the machine it leaves is sent a Release when `release`. Keys down where it was are released
    /// there, modifier keys pressed again where it goes; buttons only released.
    fn go(&mut self, to: &str, entry: Option<Entry>, release: bool) -> Option<Handoff> {
        let to = match to == self.name {
            true => Place::Local,
            false => Place::Remote(to.to_owned()),
        };
        if to == self.place {
            return None;
        }
        let from = std::mem::replace(&mut self.place, to.clone());
        let buttons = std::mem::take(&mut self.buttons);
        let (release_keys, release_buttons, release) = match from {
            Place::Local => (self.keys.keys().copied().collect(), buttons.into_iter().collect(), None),
            Place::Remote(m) => (Vec::new(), Vec::new(), release.then_some(m)),
        };
        self.keys.retain(|_, modifier| *modifier);
        Some(Handoff {
            release_keys,
            release_buttons,
            release,
            to,
            held: self.keys.keys().copied().collect(),
            entry,
        })
    }
}

/// A screen in the row the pointer crosses, left to right: the session's monitors in order (`Session::monitors`),
/// each machine's together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowScreen {
    pub machine: String,
    pub screen: String,
    pub width: u32,
    pub height: u32,
}

/// The pointer left its machine's screens: past `edge` (the left of its first, the right of its last) at height `y`
/// (a fraction). A satellite sends it as `Message::InputExit`; the primary finds where it lands with `landing`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exit {
    pub edge: Edge,
    pub y: f64,
}

/// The pointer onto another machine: its name, the screen it enters (by its index in the row) and where.
#[derive(Debug, Clone, PartialEq)]
pub struct Landing {
    pub machine: String,
    pub screen: usize,
    pub entry: Entry,
}

/// The pointer on `row[at]` pushed to `(x, y)` there (that screen's coordinates; past it when pushed out): the
/// exit, if that leaves its machine. Moves between a machine's own screens are its own business: None.
#[must_use]
pub fn exit(row: &[RowScreen], at: usize, x: f64, y: f64) -> Option<Exit> {
    let here = row.get(at)?;
    let last = row.get(at + 1).is_none_or(|s| s.machine != here.machine);
    let first = at == 0 || row[at - 1].machine != here.machine;
    let edge = match () {
        () if x >= f64::from(here.width) && last => Edge::Right,
        () if x < 0.0 && first => Edge::Left,
        () => return None,
    };
    let y = fraction(y, here.height);
    Some(Exit { edge, y })
}

/// Machine `machine`'s pointer left past `edge` at height `y` (a fraction): onto the machine beside it there,
/// entering at the facing edge. None past the row's ends, or for a machine not in it.
#[must_use]
pub fn landing(row: &[RowScreen], machine: &str, edge: Edge, y: f64) -> Option<Landing> {
    let (screen, entering) = match edge {
        Edge::Left => (
            row.iter().position(|s| s.machine == machine)?.checked_sub(1)?,
            Edge::Right,
        ),
        Edge::Right => (row.iter().rposition(|s| s.machine == machine)? + 1, Edge::Left),
    };
    Some(Landing {
        machine: row.get(screen)?.machine.clone(),
        screen,
        entry: Entry {
            edge: entering,
            y: unit(y),
        },
    })
}

/// Where relative motion takes this machine's pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// To this point, on its screens.
    To(f64, f64),
    /// Out past an outer edge: the pointer goes to it, at `at`, and leaves.
    Exit { out: Exit, at: (f64, f64) },
}

/// The pointer at `(x, y)` moved by `(dx, dy)` over this machine's `screens` (global rectangles, in any order): to a
/// point on them (past their top or bottom, or into a gap, it slides along the edge of the screen under it), or out
/// past the left of the leftmost or the right of the rightmost (`exit` over them, left to right). None without
/// screens.
#[must_use]
pub fn step(screens: &[Rect], (x, y): (f64, f64), (dx, dy): (f64, f64)) -> Option<Step> {
    let row = left_to_right(screens);
    let (first, last) = (row.first()?, row.last()?);
    let (nx, ny) = (x + dx, y + dy);
    if !(nx.is_finite() && ny.is_finite()) {
        return Some(Step::To(x, y));
    }
    let column = |r: &Rect, px: f64| px >= f64::from(r.x) && px < f64::from(r.x + r.w);
    let inside = |r: &Rect, px: f64, py: f64| column(r, px) && py >= f64::from(r.y) && py < f64::from(r.y + r.h);
    // The screen the motion ends over: the outer one past either end, else the one under it, else the one in its
    // column, else (a gap) the one it left.
    let at = match () {
        () if nx < f64::from(first.x) => 0,
        () if nx >= f64::from(last.x + last.w) => row.len() - 1,
        () => (row.iter().position(|r| inside(r, nx, ny)))
            .or_else(|| row.iter().position(|r| column(r, nx)))
            .or_else(|| row.iter().position(|r| inside(r, x, y)))
            .unwrap_or(0),
    };
    let r = row[at];
    // One machine's screens, as `exit` reads a row.
    let named: Vec<RowScreen> = (row.iter().enumerate())
        .map(|(i, r)| RowScreen {
            machine: String::new(),
            screen: i.to_string(),
            width: u32::try_from(r.w).unwrap_or(0),
            height: u32::try_from(r.h).unwrap_or(0),
        })
        .collect();
    let clamp = |v: f64, from: i32, len: i32| v.clamp(f64::from(from), f64::from(from + len - 1));
    let kept = (clamp(nx, r.x, r.w), clamp(ny, r.y, r.h));
    Some(match exit(&named, at, nx - f64::from(r.x), ny - f64::from(r.y)) {
        Some(out) => Step::Exit { out, at: kept },
        None if inside(&r, nx, ny) => Step::To(nx, ny),
        None => Step::To(kept.0, kept.1),
    })
}

/// Where the pointer enters this machine's `screens` (global rectangles, in any order) at `entry`: on the leftmost
/// at the left edge, the rightmost at the right. None without screens.
#[must_use]
pub fn enter(screens: &[Rect], entry: Entry) -> Option<(f64, f64)> {
    let row = left_to_right(screens);
    let r = match entry.edge {
        Edge::Left => row.first()?,
        Edge::Right => row.last()?,
    };
    let (x, y) = entry.point(u32::try_from(r.w).unwrap_or(0), u32::try_from(r.h).unwrap_or(0));
    Some((f64::from(r.x) + x, f64::from(r.y) + y))
}

/// `screens` that have an area, left to right.
fn left_to_right(screens: &[Rect]) -> Vec<Rect> {
    let mut row: Vec<Rect> = screens.iter().copied().filter(|r| r.w > 0 && r.h > 0).collect();
    row.sort_by_key(|r| (r.x, r.y));
    row
}

/// `y` as a fraction of `height`, inside 0..=1; 0 for a screen of no height.
fn fraction(y: f64, height: u32) -> f64 {
    if height == 0 { 0.0 } else { unit(y / f64::from(height)) }
}

/// `f` inside 0..=1; 0 when it is not a number.
fn unit(f: f64) -> f64 {
    if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) }
}

/// Input on its way to one machine, in order. Motion merges into motion queued last, so a link that stalls sends one
/// step when it moves again, never a late replay; finger scroll merges the same way, except its stop (nothing on
/// either axis, as the receiver reads it) and a sum that would read as one. Anything else (keys, buttons, wheel
/// scroll, handoffs) waits as it is.
#[derive(Debug, Default)]
pub struct Outbox {
    queue: VecDeque<Message>,
}

impl Outbox {
    /// Queues `msg`. Motion or scroll that is not finite is dropped: it would not encode.
    pub fn push(&mut self, msg: Message) {
        match msg {
            Message::Motion { dx, dy } if !(dx.is_finite() && dy.is_finite()) => {}
            Message::Axis { h, v, .. } if !(h.is_finite() && v.is_finite()) => {}
            Message::Motion { dx, dy } => match self.queue.back_mut() {
                Some(Message::Motion { dx: x, dy: y }) if (*x + dx).is_finite() && (*y + dy).is_finite() => {
                    *x += dx;
                    *y += dy;
                }
                _ => self.queue.push_back(msg),
            },
            Message::Axis {
                source: ScrollSource::Finger,
                h,
                v,
                v120_h: 0,
                v120_v: 0,
            } if (h, v) != (0.0, 0.0) => match self.queue.back_mut() {
                Some(Message::Axis {
                    source: ScrollSource::Finger,
                    h: x,
                    v: y,
                    v120_h: 0,
                    v120_v: 0,
                }) if (*x, *y) != (0.0, 0.0)
                    && (*x + h).is_finite()
                    && (*y + v).is_finite()
                    && (*x + h, *y + v) != (0.0, 0.0) =>
                {
                    *x += h;
                    *y += v;
                }
                _ => self.queue.push_back(msg),
            },
            msg => self.queue.push_back(msg),
        }
    }

    /// The next message to send. The driver takes one only when the link has written everything before
    /// (`Link::outgoing` empty), so what waits meanwhile is what merges.
    pub fn pop(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPER: RawKey = RawKey(133);
    const SHIFT: RawKey = RawKey(50);
    const A: RawKey = RawKey(38);
    const X: RawKey = RawKey(53);
    const LEFT: u32 = 272;

    fn remote(name: &str) -> Place {
        Place::Remote(name.into())
    }

    #[test]
    fn input_stays_here_until_a_handoff() {
        let mut c = Crossing::new("p");
        assert_eq!(c.place(), &Place::Local);
        assert_eq!(c.key(A, true, false), Some(Place::Local));
        assert_eq!(
            c.key(A, true, false),
            None,
            "a repeat: receivers repeat keys themselves"
        );
        assert_eq!(c.key(A, false, false), Some(Place::Local));
        assert_eq!(c.key(A, false, false), None, "not down");
        assert_eq!(c.button(LEFT, true), Some(Place::Local));
        assert_eq!(c.button(LEFT, false), Some(Place::Local));
        assert_eq!(c.button(LEFT, false), None);
        assert_eq!(c.handoff("p", None), None, "here already");
        assert_eq!(c.reclaim(), None);
    }

    #[test]
    fn a_handoff_releases_here_and_announces_the_modifiers_held() {
        let mut c = Crossing::new("p");
        c.key(SUPER, true, true);
        c.key(A, true, false);
        c.button(LEFT, true);
        let entry = Entry {
            edge: Edge::Left,
            y: 0.5,
        };
        let h = c.handoff("a", Some(entry)).expect("moves");
        assert_eq!(
            h,
            Handoff {
                release_keys: vec![A, SUPER],
                release_buttons: vec![LEFT],
                release: None,
                to: remote("a"),
                held: vec![SUPER],
                entry: Some(entry),
            }
        );
        let msg = Message::Handoff {
            mods: vec![SUPER],
            pointer: Some(entry),
            layout: 2,
        };
        assert_eq!(h.message(2), Some(("a".into(), msg)), "with the keyboard layout in use");
        assert_eq!(c.place(), &remote("a"));
        // A's press stayed behind (released here); Super's went along.
        assert_eq!(c.key(A, false, false), None);
        assert_eq!(c.button(LEFT, false), None);
        assert_eq!(c.key(SUPER, false, true), Some(remote("a")));
        assert_eq!(c.key(A, true, false), Some(remote("a")));
        assert_eq!(c.handoff("a", None), None, "there already");
        // Neither shows which keys: keystrokes are never logged.
        let shown = format!("{c:?} {h:?}");
        assert!(!shown.contains("38") && !shown.contains("133"), "{shown}");
    }

    #[test]
    fn a_handoff_between_machines_sends_the_one_left_a_release() {
        let mut c = Crossing::new("p");
        c.handoff("a", None);
        c.key(SHIFT, true, true);
        c.key(X, true, false);
        c.button(LEFT, true);
        let h = c.handoff("b", None).expect("moves");
        assert_eq!(
            h,
            Handoff {
                release_keys: vec![],
                release_buttons: vec![],
                release: Some("a".into()),
                to: remote("b"),
                held: vec![SHIFT],
                entry: None,
            }
        );
        assert_eq!(c.key(X, false, false), None, "released on a by its Release");
        assert_eq!(c.button(LEFT, false), None);
        assert_eq!(c.key(SHIFT, false, true), Some(remote("b")));
    }

    #[test]
    fn a_lock_key_is_not_a_modifier_and_its_lock_stays_on_each_machine() {
        const CAPS: RawKey = RawKey(66);
        let mut c = Crossing::new("p");
        c.key(CAPS, true, false);
        c.key(SHIFT, true, true);
        let h = c.handoff("a", None).expect("moves");
        assert_eq!((h.release_keys, h.held), (vec![SHIFT, CAPS], vec![SHIFT]));
        assert_eq!(c.key(CAPS, false, false), None, "released here at the handoff");
        // Pressed there, it goes there; held at a handoff home, it is released there and not pressed here.
        assert_eq!(c.key(CAPS, true, false), Some(remote("a")));
        let h = c.reclaim().expect("comes home");
        assert_eq!((h.release, h.held), (Some("a".into()), vec![SHIFT]));
        assert_eq!(c.key(CAPS, false, false), None);
        assert_eq!(c.key(CAPS, true, false), Some(Place::Local));
    }

    #[test]
    fn reclaim_brings_input_home_with_the_modifiers_held() {
        let mut c = Crossing::new("p");
        c.handoff("a", None);
        c.key(SUPER, true, true);
        c.key(A, true, false);
        let h = c.reclaim().expect("comes home");
        assert_eq!(
            h,
            Handoff {
                release_keys: vec![],
                release_buttons: vec![],
                release: Some("a".into()),
                to: Place::Local,
                held: vec![SUPER],
                entry: None,
            }
        );
        assert_eq!(h.message(0), None, "nothing to send for input coming here");
        assert_eq!(c.place(), &Place::Local);
        assert_eq!(
            c.key(SUPER, false, true),
            Some(Place::Local),
            "pressed here by the handoff"
        );
        assert_eq!(c.key(A, false, false), None);
        assert_eq!(c.reclaim(), None);
    }

    #[test]
    fn the_modifiers_held_are_known_wherever_input_is_and_a_lost_keyboard_forgets_its_keys() {
        let mut c = Crossing::new("p");
        c.key(SUPER, true, true);
        c.key(A, true, false);
        assert_eq!(c.held(), [SUPER]);
        c.handoff("a", None);
        c.key(SHIFT, true, true);
        assert_eq!(
            c.held(),
            [SHIFT, SUPER],
            "the escape key and a resumed handoff read them"
        );
        // The keyboard lost its keys without releases (the host took its focus): nothing is down any more, so the
        // next press of each goes where input is again instead of reading as a repeat.
        c.button(LEFT, true);
        c.forget_keys();
        assert_eq!(c.held(), []);
        assert_eq!(c.key(SUPER, true, true), Some(remote("a")));
        assert_eq!(
            c.button(LEFT, false),
            Some(remote("a")),
            "buttons are the pointer's: kept"
        );
    }

    #[test]
    fn a_lost_link_reclaims_only_from_its_machine_and_sends_nothing() {
        let mut c = Crossing::new("p");
        c.handoff("a", None);
        c.key(SUPER, true, true);
        c.key(A, true, false);
        assert_eq!(c.link_lost("b"), None);
        assert_eq!(c.place(), &remote("a"));
        let h = c.link_lost("a").expect("comes home");
        assert_eq!((h.release, h.to, h.held), (None, Place::Local, vec![SUPER]));
        assert!(h.release_keys.is_empty() && h.release_buttons.is_empty());
        assert_eq!(c.link_lost("a"), None);
        assert_eq!(c.link_lost("p"), None, "never this machine");
    }

    #[test]
    fn the_pointer_does_not_cross_while_a_button_is_down() {
        let mut c = Crossing::new("p");
        let entry = Entry {
            edge: Edge::Left,
            y: 0.25,
        };
        c.button(LEFT, true);
        assert_eq!(c.cross("a", entry), None, "a drag stays on its machine");
        assert_eq!(c.place(), &Place::Local);
        c.button(LEFT, false);
        let h = c.cross("a", entry).expect("crosses");
        assert_eq!((h.to, h.entry), (remote("a"), Some(entry)));
        assert_eq!(c.cross("a", entry), None, "there already");
        // A marker move is not a drag: it goes, the button released where it was.
        c.button(LEFT, true);
        let h = c.handoff("p", None).expect("moves");
        assert_eq!((h.release, h.release_buttons), (Some("a".into()), vec![]));
    }

    fn screen(machine: &str, screen: &str, width: u32, height: u32) -> RowScreen {
        RowScreen {
            machine: machine.into(),
            screen: screen.into(),
            width,
            height,
        }
    }

    /// p: two 1920x1080 screens; a: one 2560x1440; b: one 1280x1024.
    fn row() -> Vec<RowScreen> {
        vec![
            screen("p", "p1", 1920, 1080),
            screen("p", "p2", 1920, 1080),
            screen("a", "a1", 2560, 1440),
            screen("b", "b1", 1280, 1024),
        ]
    }

    #[test]
    fn the_pointer_leaves_at_a_machines_outer_edges_with_its_height_scaled() {
        let row = row();
        let out = |edge, y| Some(Exit { edge, y });
        // Inside, or between a machine's own screens: not an exit.
        assert_eq!(exit(&row, 1, 100.0, 540.0), None);
        assert_eq!(exit(&row, 0, 1925.0, 540.0), None, "onto p2: p's own business");
        assert_eq!(exit(&row, 1, -3.0, 540.0), None, "back onto p1");
        // Past p's last screen: onto a's first, at the same height scaled to it.
        assert_eq!(exit(&row, 1, 1925.0, 540.0), out(Edge::Right, 0.5));
        let onto = landing(&row, "p", Edge::Right, 0.5).expect("lands");
        assert_eq!(
            onto,
            Landing {
                machine: "a".into(),
                screen: 2,
                entry: Entry {
                    edge: Edge::Left,
                    y: 0.5
                },
            }
        );
        assert_eq!(onto.entry.point(2560, 1440), (0.0, 720.0));
        // Past the left of a machine's first screen.
        assert_eq!(exit(&row, 2, -1.0, 360.0), out(Edge::Left, 0.25));
        assert_eq!(exit(&row, 0, -1.0, 270.0), out(Edge::Left, 0.25));
        assert_eq!(landing(&row, "p", Edge::Left, 0.25), None, "nothing before the primary");
        // Past the last machine: an exit to nowhere.
        assert_eq!(exit(&row, 3, 1280.0, 10.0), out(Edge::Right, 10.0 / 1024.0));
        assert_eq!(landing(&row, "b", Edge::Right, 0.5), None);
        // A satellite knows only its own screens: both its outer edges are exits, which it reports (InputExit).
        let own = [screen("a", "a1", 2560, 1440)];
        assert_eq!(exit(&own, 0, 2560.0, 360.0), out(Edge::Right, 0.25));
        assert_eq!(exit(&own, 0, -0.5, 360.0), out(Edge::Left, 0.25));
        // Pushed out at a corner: the height is kept inside the screen.
        assert_eq!(exit(&row, 2, 2600.0, 1500.0), out(Edge::Right, 1.0));
        let corner = landing(&row, "a", Edge::Right, 1.0).expect("lands");
        assert_eq!(corner.entry.point(1280, 1024), (0.0, 1023.0));
        assert_eq!(exit(&row, 2, -5.0, -40.0), out(Edge::Left, 0.0));
    }

    #[test]
    fn an_exit_lands_on_the_machine_beside_it_both_ways() {
        let row = row();
        let at = |machine: &str, edge, y| landing(&row, machine, edge, y).map(|l| (l.machine, l.screen, l.entry));
        let entry = |edge, y| Entry { edge, y };
        // p -> a -> b and back, by edge.
        assert_eq!(at("a", Edge::Right, 0.5), Some(("b".into(), 3, entry(Edge::Left, 0.5))));
        assert_eq!(
            at("b", Edge::Left, 0.75),
            Some(("a".into(), 2, entry(Edge::Right, 0.75)))
        );
        let back = landing(&row, "a", Edge::Left, 0.25).expect("lands");
        assert_eq!((back.machine.as_str(), back.screen), ("p", 1), "p's last screen");
        assert_eq!(back.entry, entry(Edge::Right, 0.25));
        assert_eq!(back.entry.point(1920, 1080), (1919.0, 270.0));
        assert_eq!(landing(&row, "x", Edge::Left, 0.5), None, "not in the row");
        // A machine with no screens is not in the row: the pointer passes it by.
        let mut row = row;
        row.remove(2);
        assert_eq!(
            landing(&row, "p", Edge::Right, 0.0).map(|l| l.machine),
            Some("b".into())
        );
        assert_eq!(landing(&row, "b", Edge::Left, 0.0).map(|l| l.machine), Some("p".into()));
        // Heights out of range or not a number never come out: a screen of no height (not yet sized) enters at the
        // top, a height reported out of range is kept inside.
        let flat = [screen("p", "p1", 1920, 0), screen("a", "a1", 10, 10)];
        assert_eq!(
            exit(&flat, 0, 1920.0, 5.0),
            Some(Exit {
                edge: Edge::Right,
                y: 0.0
            })
        );
        assert_eq!(
            exit(&flat, 0, 1920.0, f64::NAN),
            Some(Exit {
                edge: Edge::Right,
                y: 0.0
            })
        );
        assert_eq!(entry(Edge::Right, 0.5).point(0, 0), (0.0, 0.0));
        assert_eq!(entry(Edge::Left, f64::NAN).point(100, 100), (0.0, 0.0));
        assert_eq!(entry(Edge::Left, 7.0).point(100, 100), (0.0, 99.0));
        for y in [f64::NAN, f64::INFINITY, -3.0, 9.0] {
            let l = landing(&flat, "p", Edge::Right, y).expect("lands");
            assert!((0.0..=1.0).contains(&l.entry.y), "{y}: {}", l.entry.y);
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn relative_motion_stays_on_this_machines_screens_or_leaves_at_its_outer_edges() {
        let one = [rect(0, 0, 640, 480)];
        assert_eq!(step(&one, (10.0, 10.0), (5.0, 5.0)), Some(Step::To(15.0, 15.0)));
        // Past the top or the bottom: it slides along the edge.
        assert_eq!(step(&one, (10.0, 10.0), (0.0, 1000.0)), Some(Step::To(10.0, 479.0)));
        assert_eq!(step(&one, (10.0, 10.0), (3.0, -50.0)), Some(Step::To(13.0, 0.0)));
        // Past the left or the right: to the edge and out there, at its height (a fraction).
        let out = |edge, y, at| {
            Some(Step::Exit {
                out: Exit { edge, y },
                at,
            })
        };
        assert_eq!(
            step(&one, (5.0, 120.0), (-20.0, 0.0)),
            out(Edge::Left, 0.25, (0.0, 120.0))
        );
        assert_eq!(
            step(&one, (630.0, 240.0), (10.0, 0.0)),
            out(Edge::Right, 0.5, (639.0, 240.0))
        );
        assert_eq!(step(&one, (630.0, 240.0), (9.5, 0.0)), Some(Step::To(639.5, 240.0)));
        // Two screens, the second taller, given in any order: across to it, and back onto the shorter one at its
        // bottom; out only past the outer edges, wherever the motion started.
        let two = [rect(640, 0, 800, 600), rect(0, 0, 640, 480)];
        assert_eq!(step(&two, (630.0, 100.0), (20.0, 0.0)), Some(Step::To(650.0, 100.0)));
        assert_eq!(step(&two, (700.0, 550.0), (-100.0, 0.0)), Some(Step::To(600.0, 479.0)));
        assert_eq!(
            step(&two, (1430.0, 300.0), (20.0, 0.0)),
            out(Edge::Right, 0.5, (1439.0, 300.0))
        );
        assert_eq!(
            step(&two, (100.0, 300.0), (5000.0, 0.0)),
            out(Edge::Right, 0.5, (1439.0, 300.0))
        );
        assert_eq!(
            step(&two, (1000.0, 120.0), (-5000.0, -500.0)),
            out(Edge::Left, 0.0, (0.0, 0.0))
        );
        // Motion that is not a number goes nowhere; no screens, no pointer.
        assert_eq!(step(&one, (10.0, 10.0), (f64::NAN, 1.0)), Some(Step::To(10.0, 10.0)));
        assert_eq!(
            step(&one, (10.0, 10.0), (f64::INFINITY, 1.0)),
            Some(Step::To(10.0, 10.0))
        );
        assert_eq!(step(&[], (0.0, 0.0), (1.0, 1.0)), None);
    }

    #[test]
    fn the_pointer_enters_at_the_outer_screen_on_its_side() {
        let two = [rect(640, 0, 800, 600), rect(0, 0, 640, 480)];
        let at = |edge, y| enter(&two, Entry { edge, y });
        assert_eq!(at(Edge::Left, 0.5), Some((0.0, 240.0)));
        assert_eq!(at(Edge::Right, 0.5), Some((1439.0, 300.0)));
        assert_eq!(at(Edge::Right, 1.0), Some((1439.0, 599.0)));
        assert_eq!(
            enter(
                &[],
                Entry {
                    edge: Edge::Left,
                    y: 0.5
                }
            ),
            None
        );
    }

    fn motion(dx: f64, dy: f64) -> Message {
        Message::Motion { dx, dy }
    }

    fn key(k: RawKey, pressed: bool) -> Message {
        Message::Key {
            keycode: k,
            pressed,
            time: 1,
        }
    }

    #[test]
    fn motion_merges_while_it_waits_and_keys_and_buttons_never_do() {
        let mut o = Outbox::default();
        assert_eq!(o.pop(), None);
        o.push(motion(1.0, 2.0));
        o.push(motion(3.0, -1.0));
        o.push(motion(0.5, 0.0));
        assert_eq!(o.len(), 1, "one step");
        o.push(key(A, true));
        o.push(key(A, true));
        o.push(motion(1.0, 1.0));
        o.push(Message::Button {
            code: LEFT,
            pressed: true,
        });
        o.push(Message::Button {
            code: LEFT,
            pressed: true,
        });
        o.push(motion(2.0, 2.0));
        o.push(motion(-1.0, 0.0));
        let got: Vec<Message> = std::iter::from_fn(|| o.pop()).collect();
        let button = Message::Button {
            code: LEFT,
            pressed: true,
        };
        assert_eq!(
            got,
            [
                motion(4.5, 1.0),
                key(A, true),
                key(A, true),
                motion(1.0, 1.0),
                button.clone(),
                button,
                motion(1.0, 2.0),
            ],
            "in order; motion never merges across a key or a button"
        );
        assert!(o.is_empty());
        // Taken at once, nothing merges.
        o.push(motion(1.0, 0.0));
        assert_eq!(o.pop(), Some(motion(1.0, 0.0)));
        o.push(motion(1.0, 0.0));
        assert_eq!(o.pop(), Some(motion(1.0, 0.0)));
        // Not a number: dropped, never merged into what waits.
        o.push(motion(1.0, 1.0));
        o.push(motion(f64::NAN, 0.0));
        o.push(motion(0.0, f64::INFINITY));
        let axis = |v| Message::Axis {
            source: ScrollSource::Wheel,
            h: 0.0,
            v,
            v120_h: 0,
            v120_v: 120,
        };
        o.push(axis(f64::NAN));
        o.push(axis(15.0));
        assert_eq!(
            std::iter::from_fn(|| o.pop()).collect::<Vec<_>>(),
            [motion(1.0, 1.0), axis(15.0)]
        );
        // A sum that would not be finite is not made: the step waits as its own.
        o.push(motion(f64::MAX, 1.0));
        o.push(motion(f64::MAX, 1.0));
        o.push(motion(-1.0, 1.0));
        assert_eq!(
            std::iter::from_fn(|| o.pop()).collect::<Vec<_>>(),
            [motion(f64::MAX, 1.0), motion(f64::MAX - 1.0, 2.0)]
        );
    }

    #[test]
    fn finger_scroll_merges_while_it_waits_and_its_stop_never_does() {
        let axis = |source, h, v| Message::Axis {
            source,
            h,
            v,
            v120_h: 0,
            v120_v: 0,
        };
        let finger = |h, v| axis(ScrollSource::Finger, h, v);
        let mut o = Outbox::default();
        o.push(finger(1.0, 2.0));
        o.push(finger(0.5, -1.0));
        o.push(finger(0.0, 3.0));
        assert_eq!(o.len(), 1, "one step of scroll");
        // A stop (nothing on either axis) waits as it is, and nothing merges into it.
        o.push(finger(0.0, 0.0));
        o.push(finger(2.0, 0.0));
        // Scroll that would sum to nothing would read as a stop: kept apart.
        o.push(finger(-2.0, 0.0));
        // Another source, or motion between, keeps them apart.
        o.push(axis(ScrollSource::Continuous, 0.0, 1.0));
        o.push(finger(0.0, 1.0));
        o.push(motion(1.0, 0.0));
        o.push(finger(0.0, 1.0));
        assert_eq!(
            std::iter::from_fn(|| o.pop()).collect::<Vec<_>>(),
            [
                finger(1.5, 4.0),
                finger(0.0, 0.0),
                finger(2.0, 0.0),
                finger(-2.0, 0.0),
                axis(ScrollSource::Continuous, 0.0, 1.0),
                finger(0.0, 1.0),
                motion(1.0, 0.0),
                finger(0.0, 1.0),
            ]
        );
    }

    /// xorshift64*: the random walks are the same on every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) % n
        }
    }

    /// What one machine's seat has down.
    #[derive(Default)]
    struct Seat {
        keys: BTreeSet<RawKey>,
        buttons: BTreeSet<u32>,
    }

    #[test]
    fn a_random_walk_never_leaves_a_key_down_on_two_machines_or_stuck() {
        const MACHINES: [&str; 3] = ["p", "a", "b"];
        // Keycodes 8..=10 are modifiers.
        let modifier = |k: RawKey| k.0 <= 10;
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut c = Crossing::new("p");
        let mut seats: BTreeMap<&str, Seat> = MACHINES.iter().map(|m| (*m, Seat::default())).collect();
        // What the user holds on p's keyboard and pointer.
        let (mut keys, mut buttons) = (BTreeSet::new(), BTreeSet::new());
        let name = |p: &Place| match p {
            Place::Local => "p".to_owned(),
            Place::Remote(m) => m.clone(),
        };
        let apply = |seats: &mut BTreeMap<&str, Seat>, h: Handoff| {
            let here = seats.get_mut("p").expect("p");
            for k in &h.release_keys {
                assert!(here.keys.remove(k), "released here, so down here");
            }
            for b in &h.release_buttons {
                assert!(here.buttons.remove(b));
            }
            if let Some(m) = &h.release {
                assert_ne!(m, "p");
                let seat = seats.get_mut(m.as_str()).expect("a machine");
                seat.keys.clear();
                seat.buttons.clear();
            }
            let to = seats.get_mut(name(&h.to).as_str()).expect("a machine");
            for k in h.held {
                assert!(to.keys.insert(k), "pressed where input goes, so not down there");
            }
        };
        for step in 0..20_000 {
            match rng.below(10) {
                0..=3 => {
                    let k = RawKey(8 + rng.below(8) as u32);
                    let pressed = keys.insert(k) || !keys.remove(&k);
                    if let Some(p) = c.key(k, pressed, modifier(k)) {
                        let seat = seats.get_mut(name(&p).as_str()).expect("a machine");
                        let fine = if pressed {
                            seat.keys.insert(k)
                        } else {
                            seat.keys.remove(&k)
                        };
                        assert!(fine, "step {step}: a press where up, a release where down");
                    }
                }
                4 | 5 => {
                    let b = LEFT + rng.below(3) as u32;
                    let pressed = buttons.insert(b) || !buttons.remove(&b);
                    if let Some(p) = c.button(b, pressed) {
                        let seat = seats.get_mut(name(&p).as_str()).expect("a machine");
                        let fine = if pressed {
                            seat.buttons.insert(b)
                        } else {
                            seat.buttons.remove(&b)
                        };
                        assert!(fine, "step {step}");
                    }
                }
                6 => {
                    let to = MACHINES[rng.below(3) as usize];
                    if let Some(h) = c.handoff(to, None) {
                        apply(&mut seats, h);
                    }
                }
                7 => {
                    let to = MACHINES[rng.below(3) as usize];
                    let entry = Entry {
                        edge: Edge::Left,
                        y: 0.5,
                    };
                    if let Some(h) = c.cross(to, entry) {
                        apply(&mut seats, h);
                    }
                }
                8 => {
                    if let Some(h) = c.reclaim() {
                        apply(&mut seats, h);
                    }
                    for m in ["a", "b"] {
                        assert!(seats[m].keys.is_empty() && seats[m].buttons.is_empty(), "step {step}");
                    }
                }
                _ => {
                    let lost = MACHINES[1 + rng.below(2) as usize];
                    let there = c.place() == &remote(lost);
                    let h = c.link_lost(lost);
                    assert_eq!(h.is_some(), there, "step {step}");
                    if let Some(h) = h {
                        assert_eq!(h.release, None, "step {step}: no link to send it on");
                        apply(&mut seats, h);
                        // That machine releases what it has down on its own drop; the handoff released nothing.
                        let seat = seats.get_mut(lost).expect("a machine");
                        seat.keys.clear();
                        seat.buttons.clear();
                    }
                    assert_ne!(c.place(), &remote(lost));
                }
            }
            // Nothing down on any machine but the one input is at; nothing down that the user does not hold; every
            // modifier held is down where input is.
            for (m, seat) in &seats {
                if *m != name(c.place()) {
                    assert!(
                        seat.keys.is_empty() && seat.buttons.is_empty(),
                        "step {step}: left down on {m}"
                    );
                }
            }
            for k in (8..16).map(RawKey) {
                let n = seats.values().filter(|s| s.keys.contains(&k)).count();
                assert!(n <= 1, "step {step}: a key down on {n} machines");
                assert!(n == 0 || keys.contains(&k), "step {step}: a stuck key");
            }
            for b in LEFT..LEFT + 3 {
                let n = seats.values().filter(|s| s.buttons.contains(&b)).count();
                assert!(n <= 1 && (n == 0 || buttons.contains(&b)), "step {step}: button {b}");
            }
            let at = &seats[name(c.place()).as_str()];
            assert!(
                keys.iter().filter(|k| modifier(**k)).all(|k| at.keys.contains(k)),
                "step {step}: a modifier held but not down where input is"
            );
        }
    }

    #[test]
    fn a_random_walk_of_motion_sums_to_what_went_in() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let mut o = Outbox::default();
        let (mut went_in, mut came_out) = ((0.0, 0.0), (0.0, 0.0));
        let (mut others_in, mut others_out) = (Vec::new(), Vec::new());
        let (mut motions_in, mut motions_out) = (0, 0);
        let mut take = |m: Message, came_out: &mut (f64, f64), motions_out: &mut usize| match m {
            Message::Motion { dx, dy } => {
                came_out.0 += dx;
                came_out.1 += dy;
                *motions_out += 1;
            }
            other => others_out.push(other),
        };
        for i in 0..20_000 {
            if rng.below(8) == 0 {
                let m = key(RawKey(8 + rng.below(4) as u32), rng.below(2) == 0);
                others_in.push(m.clone());
                o.push(m);
            } else {
                let (dx, dy) = (rng.below(41) as f64 - 20.0, rng.below(41) as f64 - 20.0);
                went_in = (went_in.0 + dx, went_in.1 + dy);
                motions_in += 1;
                o.push(motion(dx, dy));
            }
            // A link that stalls now and then: most of the time it takes everything, some stretches nothing.
            if (i / 500) % 3 != 2
                && let Some(m) = o.pop()
            {
                take(m, &mut came_out, &mut motions_out);
            }
        }
        while let Some(m) = o.pop() {
            take(m, &mut came_out, &mut motions_out);
        }
        assert_eq!(went_in, came_out);
        assert_eq!(others_in, others_out, "every key, in order");
        assert!(motions_out < motions_in, "{motions_out} of {motions_in}: stalls merged");
    }
}
