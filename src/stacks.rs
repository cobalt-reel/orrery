//! Slots as tab stacks: the state machine behind every window move.
//!
//! Pure: windows are opaque ids, monitors are names with an area. Every operation keeps each window in exactly
//! one place, and an operation that cannot apply (an edge, an empty slot, an unknown id) does nothing. The state
//! is private so nothing outside these methods can break that; callers read it through accessors.

use crate::config::Cycle;
use crate::layout::{Dir, Layout, Rect};

pub type WinId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    name: String,
    area: Rect,
    layout: Layout,
    /// One stack per layout slot, in slot order; the last window of a stack is its top.
    stacks: Vec<Vec<WinId>>,
    /// The focused slot.
    focus: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stacks {
    monitors: Vec<Monitor>,
    focused_monitor: usize,
    /// Windows with no monitor to live on (none yet, or the last one went away), oldest first. The next monitor
    /// added takes them into its focused slot.
    pending: Vec<WinId>,
}

/// What is drawn: the top window of a non-empty slot, at the slot's rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub monitor: usize,
    pub slot: usize,
    pub rect: Rect,
    pub win: WinId,
    pub focused: bool,
    /// The stack's length: more than 1 earns a badge.
    pub depth: usize,
}

/// Where a placed window is: every window in a slot has one, whether it is drawn (`on_top`) or waiting beneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub win: WinId,
    pub monitor: usize,
    pub slot: usize,
    /// The slot's rectangle: windows beneath the top are kept at this size so raising them is instant.
    pub rect: Rect,
    pub on_top: bool,
    /// On top of the focused slot of the focused monitor.
    pub focused: bool,
}

impl Monitor {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        self.area
    }

    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// One stack per layout slot, bottom to top.
    #[must_use]
    pub fn stacks(&self) -> &[Vec<WinId>] {
        &self.stacks
    }

    #[must_use]
    pub fn focus(&self) -> usize {
        self.focus
    }
}

impl Stacks {
    #[must_use]
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    #[must_use]
    pub fn focused_monitor(&self) -> usize {
        self.focused_monitor
    }

    #[must_use]
    pub fn pending(&self) -> &[WinId] {
        &self.pending
    }

    /// An output was resized; unknown names are ignored.
    pub fn set_area(&mut self, monitor: &str, area: Rect) {
        if let Some(i) = self.index_of(monitor) {
            self.monitors[i].area = area;
        }
    }

    /// Focuses a slot by pointer, empty or not; an unknown monitor or slot does nothing.
    pub fn focus_at(&mut self, monitor: &str, slot: usize) {
        if let Some(i) = self.index_of(monitor)
            && slot < self.monitors[i].stacks.len()
        {
            self.focused_monitor = i;
            self.monitors[i].focus = slot;
        }
    }

    /// Focuses a monitor, keeping its focused slot; unknown names do nothing.
    pub fn focus_monitor(&mut self, monitor: &str) {
        if let Some(i) = self.index_of(monitor) {
            self.focused_monitor = i;
        }
    }

    /// Every window in a slot, in one pass: monitor by monitor, slot by slot, bottom to top.
    // No #[must_use]: iterators already carry it.
    pub fn placements(&self) -> impl Iterator<Item = Placement> + '_ {
        self.monitors.iter().enumerate().flat_map(move |(mi, mon)| {
            let focused_monitor = mi == self.focused_monitor;
            let rects = mon.layout.rects(mon.area);
            mon.stacks
                .iter()
                .zip(rects)
                .enumerate()
                .flat_map(move |(si, (stack, rect))| {
                    stack.iter().enumerate().map(move |(pos, &win)| {
                        let on_top = pos + 1 == stack.len();
                        Placement {
                            win,
                            monitor: mi,
                            slot: si,
                            rect,
                            on_top,
                            focused: on_top && focused_monitor && si == mon.focus,
                        }
                    })
                })
        })
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an output. Layouts must be valid (`Layout::validate`); one with no slots, or a name already present,
    /// is ignored. Pending windows move onto it.
    pub fn add_monitor(&mut self, name: &str, area: Rect, layout: Layout) {
        // Zero slots would break "one stack per slot" even in release; anything else invalid is a caller bug.
        if layout.slots.is_empty() || self.monitors.iter().any(|m| m.name == name) {
            return;
        }
        debug_assert!(
            layout.validate().is_ok(),
            "add_monitor needs a valid layout: {layout:?}"
        );
        let mut stacks = vec![Vec::new(); layout.slots.len()];
        stacks[0] = std::mem::take(&mut self.pending);
        self.monitors.push(Monitor {
            name: name.to_owned(),
            area,
            layout,
            stacks,
            focus: 0,
        });
    }

    /// Its windows (slot by slot, bottom to top) go on top of the focused monitor's focused slot, or the first
    /// monitor's when the focused one is the one removed, or to `pending` when none is left. The focused monitor
    /// is where the user is looking, so orphans land in sight.
    pub fn remove_monitor(&mut self, name: &str) {
        let Some(i) = self.index_of(name) else {
            return;
        };
        let gone = self.monitors.remove(i);
        if self.focused_monitor > i {
            self.focused_monitor -= 1;
        } else if self.focused_monitor == i {
            self.focused_monitor = 0;
        }
        let windows = gone.stacks.into_iter().flatten();
        match self.monitors.get_mut(self.focused_monitor) {
            Some(m) => m.stacks[m.focus].extend(windows),
            None => self.pending.extend(windows),
        }
    }

    /// On top of the focused slot's stack (or `pending` with no monitor). An id already present is left where it
    /// is, so a window can never be in two places.
    pub fn open(&mut self, w: WinId) {
        if self.where_is(w).is_some() || self.pending.contains(&w) {
            return;
        }
        match self.focused_stack_mut() {
            Some(stack) => stack.push(w),
            None => self.pending.push(w),
        }
    }

    /// Removed wherever it is; focus stays on that slot.
    pub fn close(&mut self, w: WinId) {
        if let Some((m, s, p)) = self.where_is(w) {
            self.monitors[m].stacks[s].remove(p);
        }
        self.pending.retain(|&x| x != w);
    }

    /// Via `Layout::neighbour`; a slot at the edge has no neighbour past it.
    pub fn focus_slot(&mut self, d: Dir) {
        if let Some((m, to)) = self.neighbour(d) {
            self.monitors[m].focus = to;
        }
    }

    /// Rotates the focused stack: next sends the top to the bottom, prev brings the bottom to the top.
    pub fn cycle_tab(&mut self, c: Cycle) {
        if let Some(stack) = self.focused_stack_mut()
            && !stack.is_empty()
        {
            match c {
                Cycle::Next => stack.rotate_right(1),
                Cycle::Prev => stack.rotate_left(1),
            }
        }
    }

    /// The focused top window onto the neighbour's top; focus follows. Nothing at the edge or from an empty slot.
    pub fn move_tab(&mut self, d: Dir) {
        let Some((m, to)) = self.neighbour(d) else {
            return;
        };
        let mon = &mut self.monitors[m];
        if let Some(w) = mon.stacks[mon.focus].pop() {
            mon.stacks[to].push(w);
            mon.focus = to;
        }
    }

    /// Swaps the focused stack with the neighbour's; focus follows the focused stack.
    pub fn swap_slot(&mut self, d: Dir) {
        if let Some((m, to)) = self.neighbour(d) {
            let mon = &mut self.monitors[m];
            mon.stacks.swap(mon.focus, to);
            mon.focus = to;
        }
    }

    /// Stacks keep their order and fill the new slots in sequence; with fewer slots the extras go, in order, on
    /// top of the last slot's stack; with more, the new slots start empty. Focus is clamped. So a round trip
    /// through a smaller layout is not an identity: merged stacks stay merged. An unknown monitor or a layout
    /// with no slots does nothing.
    pub fn set_layout(&mut self, monitor: &str, l: Layout) {
        let n = l.slots.len();
        let Some(mon) = self.monitors.iter_mut().find(|m| m.name == monitor) else {
            return;
        };
        if n == 0 {
            return;
        }
        debug_assert!(l.validate().is_ok(), "set_layout needs a valid layout: {l:?}");
        let extras: Vec<WinId> = if mon.stacks.len() > n {
            mon.stacks.drain(n..).flatten().collect()
        } else {
            Vec::new()
        };
        mon.stacks.resize_with(n, Vec::new);
        mon.stacks[n - 1].extend(extras);
        mon.focus = mon.focus.min(n - 1);
        mon.layout = l;
    }

    /// Focuses the slot (and monitor) holding `w` and raises `w` to its top: a client asked for focus, or a click.
    pub fn activate(&mut self, w: WinId) {
        let Some((m, s, p)) = self.where_is(w) else {
            return;
        };
        self.focused_monitor = m;
        let mon = &mut self.monitors[m];
        mon.focus = s;
        let stack = &mut mon.stacks[s];
        stack.remove(p);
        stack.push(w);
    }

    /// One per non-empty slot: its top window at the slot's rectangle.
    #[must_use]
    pub fn tiles(&self) -> Vec<Tile> {
        self.placements()
            .filter(|p| p.on_top)
            .map(|p| Tile {
                monitor: p.monitor,
                slot: p.slot,
                rect: p.rect,
                win: p.win,
                focused: p.focused,
                depth: self.monitors[p.monitor].stacks[p.slot].len(),
            })
            .collect()
    }

    #[must_use]
    pub fn focused_window(&self) -> Option<WinId> {
        let mon = self.monitors.get(self.focused_monitor)?;
        mon.stacks.get(mon.focus)?.last().copied()
    }

    /// The focused slot's rectangle: where `open` would put a window. A toplevel that has not mapped yet is
    /// configured to it.
    #[must_use]
    pub fn focused_rect(&self) -> Option<Rect> {
        let mon = self.monitors.get(self.focused_monitor)?;
        mon.layout.rects(mon.area).get(mon.focus).copied()
    }

    /// (monitor, slot, position from the bottom) of `w`; None for unknown or pending windows.
    #[must_use]
    pub fn where_is(&self, w: WinId) -> Option<(usize, usize, usize)> {
        self.monitors.iter().enumerate().find_map(|(mi, mon)| {
            mon.stacks
                .iter()
                .enumerate()
                .find_map(|(si, stack)| stack.iter().position(|&x| x == w).map(|p| (mi, si, p)))
        })
    }

    fn index_of(&self, monitor: &str) -> Option<usize> {
        self.monitors.iter().position(|m| m.name == monitor)
    }

    fn focused_stack_mut(&mut self) -> Option<&mut Vec<WinId>> {
        let mon = self.monitors.get_mut(self.focused_monitor)?;
        mon.stacks.get_mut(mon.focus)
    }

    /// The focused monitor and the slot next to its focused one in `d`.
    fn neighbour(&self, d: Dir) -> Option<(usize, usize)> {
        let m = self.focused_monitor;
        let mon = self.monitors.get(m)?;
        let to = mon.layout.neighbour(mon.focus, d)?;
        (to < mon.stacks.len()).then_some((m, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use std::collections::BTreeSet;

    #[test]
    fn focused_rect_is_where_open_would_put_a_window() {
        assert_eq!(Stacks::new().focused_rect(), None, "no monitor, no slot");
        let mut s = wall_monitor();
        let rects = layout("wall").rects(UHD);
        assert_eq!(s.focused_rect(), Some(rects[0]));
        s.focus_at("winit", 3);
        assert_eq!(s.focused_rect(), Some(rects[3]));
        s.open(7);
        assert_eq!(s.placements().find(|p| p.win == 7).map(|p| p.rect), Some(rects[3]));
        assert_invariants(&s);
    }

    fn layout(name: &str) -> Layout {
        default_config()
            .layouts
            .into_iter()
            .find(|l| l.name == name)
            .expect("layout in the default config")
    }

    fn single() -> Layout {
        Layout {
            name: "single".into(),
            cols: 1,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![crate::layout::SlotSpec {
                name: None,
                col: 0,
                row: 0,
                cols: 1,
                rows: 1,
            }],
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    const UHD: Rect = Rect {
        x: 0,
        y: 0,
        w: 3840,
        h: 2160,
    };

    fn wall_monitor() -> Stacks {
        let mut s = Stacks::new();
        s.add_monitor("winit", UHD, layout("wall"));
        assert_invariants(&s);
        s
    }

    /// Each id exactly once; every monitor has one stack per slot and its focus in range; tiles and placements
    /// agree with the stacks and with focus.
    fn assert_invariants(s: &Stacks) {
        let mut seen = BTreeSet::new();
        for m in &s.monitors {
            assert!(!m.stacks.is_empty(), "{}: no slots", m.name);
            assert_eq!(m.stacks.len(), m.layout.slots.len(), "{}: stacks vs slots", m.name);
            assert!(m.focus < m.stacks.len(), "{}: focus {} out of range", m.name, m.focus);
            for w in m.stacks.iter().flatten() {
                assert!(seen.insert(*w), "window {w} twice");
            }
        }
        for w in &s.pending {
            assert!(seen.insert(*w), "window {w} twice (pending)");
        }
        if s.monitors.is_empty() {
            assert_eq!(s.focused_monitor, 0);
        } else {
            assert!(s.focused_monitor < s.monitors.len(), "focused monitor out of range");
            assert!(s.pending.is_empty(), "pending windows while a monitor exists");
        }

        let non_empty = s
            .monitors
            .iter()
            .flat_map(|m| &m.stacks)
            .filter(|st| !st.is_empty())
            .count();
        let tiles = s.tiles();
        assert_eq!(tiles.len(), non_empty, "one tile per non-empty slot");
        let focused: Vec<&Tile> = tiles.iter().filter(|t| t.focused).collect();
        assert!(focused.len() <= 1, "{} focused tiles", focused.len());
        assert_eq!(
            focused.first().map(|t| t.win),
            s.focused_window(),
            "focused tile vs focused_window"
        );

        let placements: Vec<Placement> = s.placements().collect();
        assert_eq!(
            placements.len(),
            seen.len() - s.pending.len(),
            "every placed window once"
        );
        for p in &placements {
            let (m, sl, pos) = s.where_is(p.win).expect("placed windows are found");
            assert_eq!((m, sl), (p.monitor, p.slot));
            assert_eq!(p.on_top, pos + 1 == s.monitors[m].stacks[sl].len());
        }
        let tops: Vec<(usize, usize, Rect, WinId, bool)> = placements
            .iter()
            .filter(|p| p.on_top)
            .map(|p| (p.monitor, p.slot, p.rect, p.win, p.focused))
            .collect();
        let from_tiles: Vec<_> = tiles
            .iter()
            .map(|t| (t.monitor, t.slot, t.rect, t.win, t.focused))
            .collect();
        assert_eq!(tops, from_tiles, "tiles are the top placements");
    }

    fn ids(s: &Stacks) -> BTreeSet<WinId> {
        s.monitors
            .iter()
            .flat_map(|m| m.stacks.iter().flatten())
            .chain(&s.pending)
            .copied()
            .collect()
    }

    #[test]
    fn open_puts_the_newest_on_top() {
        let mut s = wall_monitor();
        for w in 1..=3 {
            s.open(w);
            assert_invariants(&s);
        }
        assert_eq!(s.monitors[0].stacks[0], vec![1, 2, 3]);
        assert_eq!(s.focused_window(), Some(3));
        let tiles = s.tiles();
        assert_eq!(tiles.len(), 1);
        assert_eq!(
            tiles[0],
            Tile {
                monitor: 0,
                slot: 0,
                rect: rect(0, 0, 1280, 720),
                win: 3,
                focused: true,
                depth: 3,
            }
        );
        assert_eq!(s.where_is(1), Some((0, 0, 0)));
        assert_eq!(s.where_is(3), Some((0, 0, 2)));
        assert_eq!(s.where_is(9), None);
    }

    #[test]
    fn placements_list_every_placed_window() {
        let mut s = wall_monitor();
        s.open(1);
        s.open(2);
        s.focus_slot(Dir::Right);
        s.open(3);
        let cell = rect(0, 0, 1280, 720);
        let hero = rect(1280, 0, 2560, 1440);
        let p = |win, slot, rect, on_top, focused| Placement {
            win,
            monitor: 0,
            slot,
            rect,
            on_top,
            focused,
        };
        assert_eq!(
            s.placements().collect::<Vec<_>>(),
            vec![
                p(1, 0, cell, false, false),
                p(2, 0, cell, true, false),
                p(3, 3, hero, true, true)
            ]
        );
        assert_invariants(&s);
    }

    #[test]
    fn opening_an_id_twice_keeps_one() {
        let mut s = wall_monitor();
        s.open(1);
        s.open(2);
        s.open(1);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![1, 2]);
    }

    #[test]
    fn closing_the_top_reveals_the_one_beneath() {
        let mut s = wall_monitor();
        (1..=3).for_each(|w| s.open(w));
        s.close(3);
        assert_invariants(&s);
        assert_eq!(s.focused_window(), Some(2));
        assert_eq!(s.tiles()[0].win, 2);
        assert_eq!(s.tiles()[0].depth, 2);
        s.close(1);
        s.close(2);
        assert_invariants(&s);
        assert_eq!(s.focused_window(), None);
        assert!(s.tiles().is_empty());
        // Closing an unknown id does nothing.
        s.close(42);
        assert_invariants(&s);
    }

    #[test]
    fn closing_elsewhere_keeps_focus() {
        let mut s = wall_monitor();
        s.open(1);
        s.focus_slot(Dir::Right);
        s.open(2);
        s.close(1);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].focus, 3);
        assert_eq!(s.focused_window(), Some(2));
    }

    #[test]
    fn cycle_next_sends_the_top_to_the_bottom_and_prev_undoes_it() {
        let mut s = wall_monitor();
        (1..=3).for_each(|w| s.open(w));
        s.cycle_tab(Cycle::Next);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![3, 1, 2]);
        assert_eq!(s.focused_window(), Some(2));
        s.cycle_tab(Cycle::Prev);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![1, 2, 3]);
        s.cycle_tab(Cycle::Prev);
        assert_eq!(s.monitors[0].stacks[0], vec![2, 3, 1]);
        assert_eq!(s.focused_window(), Some(1));
        assert_invariants(&s);
    }

    #[test]
    fn cycling_an_empty_slot_does_nothing() {
        let mut s = wall_monitor();
        s.cycle_tab(Cycle::Next);
        s.cycle_tab(Cycle::Prev);
        assert_invariants(&s);
        assert_eq!(s, wall_monitor());
    }

    #[test]
    fn focus_slot_follows_the_layout_and_stops_at_the_edge() {
        let mut s = wall_monitor();
        s.focus_slot(Dir::Right);
        assert_eq!(s.monitors[0].focus, 3);
        s.focus_slot(Dir::Down);
        assert_eq!(s.monitors[0].focus, 4);
        s.focus_slot(Dir::Left);
        assert_eq!(s.monitors[0].focus, 2);
        s.focus_slot(Dir::Left);
        assert_eq!(s.monitors[0].focus, 2);
        s.focus_slot(Dir::Down);
        assert_eq!(s.monitors[0].focus, 2);
        assert_invariants(&s);
    }

    #[test]
    fn focus_at_and_focus_monitor_take_names() {
        let mut s = wall_monitor();
        s.add_monitor("second", UHD, layout("halves"));
        s.focus_at("second", 1);
        assert_invariants(&s);
        assert_eq!((s.focused_monitor, s.monitors[1].focus), (1, 1));
        // An empty slot can take focus: a new window then opens there.
        s.open(7);
        assert_eq!(s.monitors[1].stacks[1], vec![7]);
        s.focus_monitor("winit");
        assert_invariants(&s);
        assert_eq!((s.focused_monitor, s.monitors[0].focus), (0, 0));
        s.focus_monitor("second");
        assert_eq!(s.focused_window(), Some(7));
        // Unknown monitor or slot: nothing.
        let before = s.clone();
        s.focus_at("second", 2);
        s.focus_at("nope", 0);
        s.focus_monitor("nope");
        assert_invariants(&s);
        assert_eq!(s, before);
    }

    #[test]
    fn move_tab_right_from_top_left_lands_on_the_hero_and_focus_follows() {
        let mut s = wall_monitor();
        (1..=2).for_each(|w| s.open(w));
        s.move_tab(Dir::Right);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![1]);
        assert_eq!(s.monitors[0].stacks[3], vec![2]);
        assert_eq!(s.monitors[0].focus, 3);
        assert_eq!(s.focused_window(), Some(2));
        let hero = s.tiles().into_iter().find(|t| t.win == 2).expect("hero tile");
        assert_eq!(hero.rect, rect(1280, 0, 2560, 1440));
        assert!(hero.focused);
        // Onto an occupied slot: it goes on top.
        s.focus_slot(Dir::Left);
        s.move_tab(Dir::Right);
        assert_eq!(s.monitors[0].stacks[3], vec![2, 1]);
        assert_invariants(&s);
    }

    #[test]
    fn move_tab_at_the_edge_or_from_an_empty_slot_does_nothing() {
        let mut s = wall_monitor();
        s.move_tab(Dir::Right);
        assert_eq!(s.monitors[0].focus, 0);
        s.open(1);
        s.move_tab(Dir::Left);
        s.move_tab(Dir::Up);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![1]);
        assert_eq!(s.monitors[0].focus, 0);
    }

    #[test]
    fn swap_slot_swaps_whole_stacks_and_focus_follows() {
        let mut s = wall_monitor();
        (1..=2).for_each(|w| s.open(w));
        s.focus_slot(Dir::Right);
        s.open(3);
        s.focus_slot(Dir::Left);
        s.swap_slot(Dir::Right);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![3]);
        assert_eq!(s.monitors[0].stacks[3], vec![1, 2]);
        assert_eq!(s.monitors[0].focus, 3);
        assert_eq!(s.focused_window(), Some(2));
        // At the edge: nothing.
        s.swap_slot(Dir::Up);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].focus, 3);
    }

    /// wall with windows in all six slots: slot i holds [10i+1, 10i+2].
    fn full_wall() -> Stacks {
        let mut s = wall_monitor();
        for i in 0..6 {
            s.focus_at("winit", i);
            s.open(10 * i as u64 + 1);
            s.open(10 * i as u64 + 2);
        }
        assert_invariants(&s);
        s
    }

    #[test]
    fn set_layout_merges_extra_stacks_into_the_last_slot() {
        let mut s = full_wall();
        s.set_layout("winit", layout("halves"));
        assert_invariants(&s);
        let m = &s.monitors[0];
        assert_eq!(m.layout.name, "halves");
        assert_eq!(m.stacks[0], vec![1, 2]);
        assert_eq!(m.stacks[1], vec![11, 12, 21, 22, 31, 32, 41, 42, 51, 52]);
        // Focus was on slot 5, clamped to the last slot.
        assert_eq!(m.focus, 1);
        assert_eq!(s.focused_window(), Some(52));
        assert_eq!(
            s.tiles().iter().map(|t| t.rect).collect::<Vec<_>>(),
            vec![rect(0, 0, 1920, 2160), rect(1920, 0, 1920, 2160)]
        );
    }

    #[test]
    fn set_layout_round_trip_is_not_an_identity() {
        // Going back fills the new slots with the stacks in order; the merged stack stays merged in slot 1 and
        // slots 2..5 come back empty. Nothing is closed, but the original spread is not restored.
        let mut s = full_wall();
        s.set_layout("winit", layout("halves"));
        s.set_layout("winit", layout("wall"));
        assert_invariants(&s);
        let m = &s.monitors[0];
        assert_eq!(m.layout.name, "wall");
        assert_eq!(
            m.stacks,
            vec![
                vec![1, 2],
                vec![11, 12, 21, 22, 31, 32, 41, 42, 51, 52],
                vec![],
                vec![],
                vec![],
                vec![],
            ]
        );
        assert_eq!(m.focus, 1);
    }

    #[test]
    fn set_layout_ignores_a_bad_monitor_or_an_empty_layout() {
        let mut s = full_wall();
        let before = s.clone();
        s.set_layout("nope", layout("halves"));
        let mut empty = single();
        empty.slots.clear();
        s.set_layout("winit", empty);
        assert_invariants(&s);
        assert_eq!(s, before);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "valid layout")]
    fn set_layout_asserts_a_valid_layout_in_debug() {
        let mut s = wall_monitor();
        let mut gap = layout("wall");
        gap.slots.pop();
        s.set_layout("winit", gap);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "valid layout")]
    fn add_monitor_asserts_a_valid_layout_in_debug() {
        let mut s = Stacks::new();
        let mut gap = layout("wall");
        gap.slots.pop();
        s.add_monitor("winit", UHD, gap);
    }

    #[test]
    fn activate_focuses_the_slot_and_raises_the_window() {
        let mut s = wall_monitor();
        (1..=3).for_each(|w| s.open(w));
        s.focus_slot(Dir::Right);
        s.open(4);
        s.activate(1);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].focus, 0);
        assert_eq!(s.monitors[0].stacks[0], vec![2, 3, 1]);
        assert_eq!(s.focused_window(), Some(1));
        s.activate(99);
        assert_invariants(&s);
        assert_eq!(s.focused_window(), Some(1));
    }

    #[test]
    fn activate_switches_monitor() {
        let mut s = wall_monitor();
        s.open(1);
        s.add_monitor("second", UHD, single());
        assert_eq!(s.focused_monitor, 0);
        s.focus_monitor("second");
        s.open(2);
        s.activate(1);
        assert_invariants(&s);
        assert_eq!(s.focused_monitor, 0);
        assert_eq!(s.focused_window(), Some(1));
        let focused: Vec<_> = s.tiles().into_iter().filter(|t| t.focused).collect();
        assert_eq!(focused.len(), 1);
        assert_eq!(focused[0].win, 1);
    }

    #[test]
    fn remove_the_focused_monitor_sends_its_windows_to_the_first() {
        let mut s = wall_monitor();
        s.open(1);
        s.add_monitor("second", UHD, layout("halves"));
        s.focus_monitor("second");
        s.open(2);
        s.open(3);
        s.focus_slot(Dir::Right);
        s.open(4);
        s.remove_monitor("second");
        assert_invariants(&s);
        assert_eq!(s.monitors.len(), 1);
        assert_eq!(s.focused_monitor, 0);
        assert_eq!(s.monitors[0].stacks[0], vec![1, 2, 3, 4]);
        // Unknown names do nothing.
        s.remove_monitor("nope");
        assert_invariants(&s);
    }

    #[test]
    fn remove_another_monitor_sends_its_windows_to_the_focused_one() {
        let mut s = wall_monitor();
        s.open(1);
        s.add_monitor("b", UHD, single());
        s.add_monitor("c", UHD, layout("halves"));
        s.focus_at("c", 1);
        s.open(2);
        s.remove_monitor("winit");
        assert_invariants(&s);
        assert_eq!(s.monitors[s.focused_monitor].name, "c");
        assert_eq!(s.monitors[s.focused_monitor].stacks[1], vec![2, 1]);
        assert_eq!(s.focused_window(), Some(1));
        assert!(s.monitors[0].stacks[0].is_empty(), "b got nothing");
    }

    #[test]
    fn windows_without_a_monitor_wait_for_one() {
        let mut s = Stacks::new();
        assert_invariants(&s);
        s.open(1);
        s.open(2);
        assert_invariants(&s);
        assert_eq!(s.pending, vec![1, 2]);
        assert_eq!(s.focused_window(), None);
        assert!(s.tiles().is_empty());
        s.close(1);
        s.add_monitor("winit", UHD, layout("wall"));
        assert_invariants(&s);
        assert_eq!(s.monitors[0].stacks[0], vec![2]);
        s.open(3);
        s.remove_monitor("winit");
        assert_invariants(&s);
        assert_eq!(s.pending, vec![2, 3]);
        s.add_monitor("next", UHD, single());
        assert_eq!(s.monitors[0].stacks[0], vec![2, 3]);
        assert_invariants(&s);
    }

    #[test]
    fn add_monitor_ignores_a_duplicate_name_or_an_empty_layout() {
        let mut s = wall_monitor();
        s.add_monitor("winit", UHD, single());
        let mut empty = single();
        empty.slots.clear();
        s.add_monitor("other", UHD, empty);
        assert_invariants(&s);
        assert_eq!(s, wall_monitor());
    }

    #[test]
    fn accessors_read_the_state() {
        let mut s = wall_monitor();
        s.open(1);
        s.focus_slot(Dir::Right);
        s.open(2);
        assert_invariants(&s);
        assert_eq!(s.monitors().len(), 1);
        assert_eq!(s.focused_monitor(), 0);
        assert!(s.pending().is_empty());
        let m = &s.monitors()[0];
        assert_eq!(m.name(), "winit");
        assert_eq!(m.area(), UHD);
        assert_eq!(m.layout().name, "wall");
        assert_eq!(m.focus(), 3);
        assert_eq!(m.stacks()[0], vec![1]);
        assert_eq!(m.stacks()[3], vec![2]);
        let mut empty = Stacks::new();
        empty.open(5);
        assert_invariants(&empty);
        assert_eq!(empty.pending(), &[5]);
    }

    #[test]
    fn set_area_moves_the_tiles() {
        let mut s = wall_monitor();
        s.open(1);
        let small = rect(0, 0, 1920, 1080);
        s.set_area("winit", small);
        s.set_area("nope", UHD);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].area, small);
        assert_eq!(s.tiles()[0].rect, rect(0, 0, 640, 360));
    }

    /// xorshift64: enough randomness for a reproducible walk, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn dir(&mut self) -> Dir {
            [Dir::Left, Dir::Right, Dir::Up, Dir::Down][self.below(4) as usize]
        }
    }

    fn all_stacks(s: &Stacks) -> Vec<Vec<Vec<WinId>>> {
        s.monitors.iter().map(|m| m.stacks.clone()).collect()
    }

    /// Each stack as a sorted list: what a rotation must keep.
    fn multisets(s: &Stacks) -> Vec<Vec<Vec<WinId>>> {
        let mut v = all_stacks(s);
        v.iter_mut().flatten().for_each(|st| st.sort_unstable());
        v
    }

    /// One seeded walk of 1,000 operations; returns the most windows open at once.
    fn walk(seed: u64) -> usize {
        let layouts = [layout("wall"), layout("halves"), single()];
        let mut rng = Rng(seed);
        let mut s = wall_monitor();
        let mut live: BTreeSet<WinId> = BTreeSet::new();
        let mut next_id: WinId = 1;
        let mut next_monitor = 0;
        let mut most = 0;
        for step in 0..1000 {
            let pick = |rng: &mut Rng, live: &BTreeSet<WinId>| -> WinId {
                // Mostly a live id; sometimes one that never existed.
                if live.is_empty() || rng.below(8) == 0 {
                    1_000_000 + rng.below(10)
                } else {
                    *live
                        .iter()
                        .nth(rng.below(live.len() as u64) as usize)
                        .expect("in range")
                }
            };
            // A monitor name: mostly a real one, sometimes one that is not there.
            let name = |rng: &mut Rng, s: &Stacks| -> String {
                let n = s.monitors.len() as u64;
                match rng.below(n + 1) {
                    i if i < n => s.monitors[i as usize].name.clone(),
                    _ => "ghost".into(),
                }
            };
            let before = s.clone();
            // Close more once many are open, so the walk keeps visiting small and empty stacks.
            let op = if live.len() > 12 && rng.below(2) == 0 {
                3
            } else {
                rng.below(14)
            };
            match op {
                0..=2 => {
                    s.open(next_id);
                    live.insert(next_id);
                    next_id += 1;
                }
                3 => {
                    let w = pick(&mut rng, &live);
                    s.close(w);
                    live.remove(&w);
                }
                4 => {
                    s.focus_slot(rng.dir());
                    assert_eq!(
                        all_stacks(&s),
                        all_stacks(&before),
                        "seed {seed} step {step}: focus_slot moved"
                    );
                }
                5 => {
                    s.cycle_tab(if rng.below(2) == 0 { Cycle::Next } else { Cycle::Prev });
                    assert_eq!(multisets(&s), multisets(&before), "seed {seed} step {step}: cycle_tab");
                }
                6 => s.move_tab(rng.dir()),
                7 => s.swap_slot(rng.dir()),
                8 => {
                    let m = name(&mut rng, &s);
                    s.set_layout(&m, layouts[rng.below(3) as usize].clone());
                }
                9 => s.activate(pick(&mut rng, &live)),
                10 => {
                    next_monitor += 1;
                    let l = layouts[rng.below(3) as usize].clone();
                    s.add_monitor(&format!("m{next_monitor}"), UHD, l);
                }
                11 => {
                    let m = name(&mut rng, &s);
                    s.remove_monitor(&m);
                }
                12 => {
                    let m = name(&mut rng, &s);
                    s.focus_at(&m, rng.below(7) as usize);
                }
                _ => {
                    let m = name(&mut rng, &s);
                    s.focus_monitor(&m);
                }
            }
            assert_invariants(&s);
            assert_eq!(ids(&s), live, "seed {seed} step {step}");
            most = most.max(live.len());
        }
        most
    }

    #[test]
    fn random_walks_lose_nothing() {
        let mut seed: u64 = 0x7472_616e_6365;
        for _ in 0..32 {
            let most = walk(seed);
            assert!(
                (4..=40).contains(&most),
                "seed {seed}: at most {most} open, the walk is unbalanced"
            );
            seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1) | 1;
        }
    }
}
