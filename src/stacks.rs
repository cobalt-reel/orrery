//! Slots of tabs, tabs of stacked windows: the state machine behind every window move.
//!
//! Pure: windows are opaque ids, monitors are names with an area. Every operation keeps each window in exactly
//! one place, and an operation that cannot apply (an edge, an empty slot, an unknown id) does nothing. The state
//! is private so nothing outside these methods can break that; callers read it through accessors.

use std::collections::BTreeSet;

use crate::config::Cycle;
use crate::layout::{Dir, Layout, Rect};

pub type WinId = u64;

/// One layout slot: its tabs, left to right, each a stack of windows (bottom to top), and the one shown. Only
/// the top window of the current tab is drawn; a tab is never empty, so an empty slot has no tabs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Slot {
    tabs: Vec<Vec<WinId>>,
    /// The tab shown; 0 when there is none.
    current: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    name: String,
    area: Rect,
    layout: Layout,
    /// One per layout slot, in slot order.
    slots: Vec<Slot>,
    /// The focused slot.
    focus: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stacks {
    monitors: Vec<Monitor>,
    focused_monitor: usize,
    /// Windows with no monitor to live on (none yet, or the last one went away), oldest first. The next monitor
    /// added takes them, as one stack, into its focused slot.
    pending: Vec<WinId>,
    /// Windows that asked to be seen while waiting: placed, never the top of their slot's current tab. A mark
    /// goes for good once its window is shown (see `settle`), or when it closes or loses its monitor.
    attention: BTreeSet<WinId>,
}

/// What is drawn: the top window of a non-empty slot's current tab, at the slot's rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub monitor: usize,
    pub slot: usize,
    pub rect: Rect,
    pub win: WinId,
    pub focused: bool,
    /// The slot's tab count.
    pub tabs: usize,
    /// The current tab's stack length.
    pub depth: usize,
    /// Every window in the slot, any tab, any depth: more than 1 earns a badge.
    pub windows: usize,
}

/// Where a placed window is: every window in a slot has one, whether it is drawn (`on_top`) or waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub win: WinId,
    pub monitor: usize,
    pub slot: usize,
    pub tab: usize,
    /// The slot's rectangle: waiting windows are kept at this size so showing them is instant.
    pub rect: Rect,
    /// On top of its slot's current tab.
    pub on_top: bool,
    /// On top of the focused slot of the focused monitor.
    pub focused: bool,
}

impl Slot {
    /// Left to right, each bottom to top.
    #[must_use]
    pub fn tabs(&self) -> &[Vec<WinId>] {
        &self.tabs
    }

    #[must_use]
    pub fn current(&self) -> usize {
        self.current
    }

    /// The top of the current tab: the slot's one drawn window.
    #[must_use]
    pub fn top(&self) -> Option<WinId> {
        self.tabs.get(self.current)?.last().copied()
    }

    /// Windows in the slot, every tab.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.iter().map(Vec::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    fn current_mut(&mut self) -> Option<&mut Vec<WinId>> {
        self.tabs.get_mut(self.current)
    }

    /// `tab` right after the current one (first in an empty slot), made current.
    fn insert_tab(&mut self, tab: Vec<WinId>) {
        let at = if self.tabs.is_empty() { 0 } else { self.current + 1 };
        self.tabs.insert(at, tab);
        self.current = at;
    }

    /// Takes tab `i` out; if it was current, the tab now at its index is shown, else the previous one.
    fn remove_tab(&mut self, i: usize) -> Vec<WinId> {
        let tab = self.tabs.remove(i);
        if i < self.current {
            self.current -= 1;
        }
        self.current = self.current.min(self.tabs.len().saturating_sub(1));
        tab
    }

    /// Removes the window at `pos` of tab `t`; a tab left empty goes.
    fn remove(&mut self, t: usize, pos: usize) {
        self.tabs[t].remove(pos);
        if self.tabs[t].is_empty() {
            self.remove_tab(t);
        }
    }
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

    /// One per layout slot.
    #[must_use]
    pub fn slots(&self) -> &[Slot] {
        &self.slots
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
            && slot < self.monitors[i].slots.len()
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

    /// Every window in a slot, in one pass: monitor by monitor, slot by slot, tab by tab, bottom to top.
    // No #[must_use]: iterators already carry it.
    pub fn placements(&self) -> impl Iterator<Item = Placement> + '_ {
        self.monitors.iter().enumerate().flat_map(move |(mi, mon)| {
            let focused_monitor = mi == self.focused_monitor;
            let rects = mon.layout.rects(mon.area);
            mon.slots
                .iter()
                .zip(rects)
                .enumerate()
                .flat_map(move |(si, (slot, rect))| {
                    slot.tabs.iter().enumerate().flat_map(move |(ti, stack)| {
                        stack.iter().enumerate().map(move |(pos, &win)| {
                            let on_top = ti == slot.current && pos + 1 == stack.len();
                            Placement {
                                win,
                                monitor: mi,
                                slot: si,
                                tab: ti,
                                rect,
                                on_top,
                                focused: on_top && focused_monitor && si == mon.focus,
                            }
                        })
                    })
                })
        })
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an output. Layouts must be valid (`Layout::validate`); one with no slots, or a name already present,
    /// is ignored. Pending windows move onto it, as one tab of its first slot.
    pub fn add_monitor(&mut self, name: &str, area: Rect, layout: Layout) {
        // Zero slots would break "one slot per layout slot" even in release; anything else invalid is a caller
        // bug.
        if layout.slots.is_empty() || self.monitors.iter().any(|m| m.name == name) {
            return;
        }
        debug_assert!(
            layout.validate().is_ok(),
            "add_monitor needs a valid layout: {layout:?}"
        );
        let mut slots = vec![Slot::default(); layout.slots.len()];
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            slots[0].insert_tab(pending);
        }
        self.monitors.push(Monitor {
            name: name.to_owned(),
            area,
            layout,
            slots,
            focus: 0,
        });
        self.settle();
    }

    /// Its tabs (slot by slot) are appended to the focused monitor's focused slot, or the first monitor's when
    /// the focused one is the one removed; that slot keeps showing its current tab. With no monitor left their
    /// windows go to `pending`. The focused monitor is where the user is looking, so orphans land in sight.
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
        let tabs = gone.slots.into_iter().flat_map(|s| s.tabs);
        match self.monitors.get_mut(self.focused_monitor) {
            Some(m) => m.slots[m.focus].tabs.extend(tabs),
            None => self.pending.extend(tabs.flatten()),
        }
        self.settle();
    }

    /// On top of the focused slot's current stack (its first tab when empty), or `pending` with no monitor.
    pub fn open_stacked(&mut self, w: WinId) {
        if self.has(w) {
            return;
        }
        let Some(slot) = self.focused_slot_mut() else {
            self.pending.push(w);
            return;
        };
        match slot.current_mut() {
            Some(stack) => stack.push(w),
            None => slot.insert_tab(vec![w]),
        }
        self.settle();
    }

    /// A new tab right after the current one in `slot` of `monitor` (that monitor's focused slot without one),
    /// made current, and the slot and monitor focused. A target that is not there means the focused slot;
    /// `pending` with no monitor.
    pub fn open_tab(&mut self, w: WinId, monitor: Option<&str>, slot: Option<usize>) {
        if self.has(w) {
            return;
        }
        let m = match monitor {
            Some(name) => self.index_of(name),
            None => Some(self.focused_monitor),
        };
        let target = m.and_then(|m| {
            let mon = self.monitors.get(m)?;
            let s = slot.unwrap_or(mon.focus);
            (s < mon.slots.len()).then_some((m, s))
        });
        let Some((m, s)) = target.or_else(|| {
            let mon = self.monitors.get(self.focused_monitor)?;
            Some((self.focused_monitor, mon.focus))
        }) else {
            self.pending.push(w);
            return;
        };
        self.focused_monitor = m;
        let mon = &mut self.monitors[m];
        mon.focus = s;
        mon.slots[s].insert_tab(vec![w]);
        self.settle();
    }

    /// On top of `parent`'s stack, wherever it is; the current tab and focus stay. `false` (and nothing done)
    /// when the parent is not placed or `w` already is somewhere, so the caller can fall back.
    pub fn open_on(&mut self, w: WinId, parent: WinId) -> bool {
        if self.has(w) {
            return false;
        }
        let Some((m, s, t, _)) = self.where_is(parent) else {
            return false;
        };
        self.monitors[m].slots[s].tabs[t].push(w);
        self.settle();
        true
    }

    /// Removed wherever it is; a tab left empty goes (see `Slot::remove_tab`). Focus stays on that slot.
    pub fn close(&mut self, w: WinId) {
        if let Some((m, s, t, p)) = self.where_is(w) {
            self.monitors[m].slots[s].remove(t, p);
        }
        self.pending.retain(|&x| x != w);
        self.settle();
    }

    /// Via `Layout::neighbour`; a slot at the edge has no neighbour past it.
    pub fn focus_slot(&mut self, d: Dir) {
        if let Some((m, to)) = self.neighbour(d) {
            self.monitors[m].focus = to;
        }
    }

    /// Shows the focused slot's next or previous tab, wrapping. Tabs never move.
    pub fn cycle_tab(&mut self, c: Cycle) {
        if let Some(slot) = self.focused_slot_mut() {
            let n = slot.tabs.len();
            if n > 1 {
                slot.current = match c {
                    Cycle::Next => (slot.current + 1) % n,
                    Cycle::Prev => (slot.current + n - 1) % n,
                };
            }
        }
        self.settle();
    }

    /// The focused slot's current tab, its whole stack, becomes a tab of the neighbour right after its current
    /// one, shown there; focus follows. Nothing at the edge or from an empty slot.
    pub fn move_stack(&mut self, d: Dir) {
        let Some((m, to)) = self.neighbour(d) else {
            return;
        };
        let mon = &mut self.monitors[m];
        let from = &mut mon.slots[mon.focus];
        if from.is_empty() {
            return;
        }
        let tab = from.remove_tab(from.current);
        mon.slots[to].insert_tab(tab);
        mon.focus = to;
        self.settle();
    }

    /// Swaps the focused slot with the neighbour, every tab and the current one; focus follows.
    pub fn swap_slot(&mut self, d: Dir) {
        if let Some((m, to)) = self.neighbour(d) {
            let mon = &mut self.monitors[m];
            mon.slots.swap(mon.focus, to);
            mon.focus = to;
        }
        self.settle();
    }

    /// The focused stack's top window leaves it for a tab of its own right after, made current. Nothing for a
    /// stack of one or an empty slot.
    pub fn split(&mut self) {
        if let Some(slot) = self.focused_slot_mut()
            && let Some(stack) = slot.current_mut()
            && stack.len() > 1
            && let Some(top) = stack.pop()
        {
            slot.insert_tab(vec![top]);
        }
        self.settle();
    }

    /// Slots keep their order and fill the new layout's in sequence; with fewer slots the extras' tabs are
    /// appended, in order, to the last slot, which keeps its current tab; with more, the new slots start empty.
    /// Focus is clamped. So a round trip through a smaller layout is not an identity: merged tabs stay merged.
    /// An unknown monitor or a layout with no slots does nothing.
    pub fn set_layout(&mut self, monitor: &str, l: Layout) {
        let n = l.slots.len();
        let Some(mon) = self.monitors.iter_mut().find(|m| m.name == monitor) else {
            return;
        };
        if n == 0 {
            return;
        }
        debug_assert!(l.validate().is_ok(), "set_layout needs a valid layout: {l:?}");
        let extras: Vec<Vec<WinId>> = if mon.slots.len() > n {
            mon.slots.drain(n..).flat_map(|s| s.tabs).collect()
        } else {
            Vec::new()
        };
        mon.slots.resize_with(n, Slot::default);
        mon.slots[n - 1].tabs.extend(extras);
        mon.focus = mon.focus.min(n - 1);
        mon.layout = l;
        self.settle();
    }

    /// Focuses the slot (and monitor) holding `w` when `w` is what it shows, the top of its current tab, and
    /// says whether it did. Nothing is ever raised: a window beneath stays there.
    pub fn focus_window(&mut self, w: WinId) -> bool {
        let Some((m, s, _, _)) = self.where_is(w) else {
            return false;
        };
        if self.monitors[m].slots[s].top() != Some(w) {
            return false;
        }
        self.focused_monitor = m;
        self.monitors[m].focus = s;
        true
    }

    /// Marks `w` as asking for attention. Only a waiting window can be: one on top is already seen, and one not
    /// placed has no slot to show it.
    pub fn mark(&mut self, w: WinId) {
        self.attention.insert(w);
        self.settle();
    }

    pub fn clear_mark(&mut self, w: WinId) {
        self.attention.remove(&w);
    }

    #[must_use]
    pub fn is_marked(&self, w: WinId) -> bool {
        self.attention.contains(&w)
    }

    /// One per non-empty slot: the top of its current tab at the slot's rectangle.
    #[must_use]
    pub fn tiles(&self) -> Vec<Tile> {
        self.placements()
            .filter(|p| p.on_top)
            .map(|p| {
                let slot = &self.monitors[p.monitor].slots[p.slot];
                Tile {
                    monitor: p.monitor,
                    slot: p.slot,
                    rect: p.rect,
                    win: p.win,
                    focused: p.focused,
                    tabs: slot.tabs.len(),
                    depth: slot.tabs[p.tab].len(),
                    windows: slot.len(),
                }
            })
            .collect()
    }

    /// The focused slot's top.
    #[must_use]
    pub fn focused_window(&self) -> Option<WinId> {
        let mon = self.monitors.get(self.focused_monitor)?;
        mon.slots.get(mon.focus)?.top()
    }

    /// The focused slot's rectangle: where `open_stacked` would put a window. A toplevel that has not mapped yet
    /// is configured to it.
    #[must_use]
    pub fn focused_rect(&self) -> Option<Rect> {
        let mon = self.monitors.get(self.focused_monitor)?;
        mon.layout.rects(mon.area).get(mon.focus).copied()
    }

    /// (monitor, slot, tab, position from the bottom) of `w`; None for unknown or pending windows.
    #[must_use]
    pub fn where_is(&self, w: WinId) -> Option<(usize, usize, usize, usize)> {
        self.monitors.iter().enumerate().find_map(|(mi, mon)| {
            mon.slots.iter().enumerate().find_map(|(si, slot)| {
                slot.tabs
                    .iter()
                    .enumerate()
                    .find_map(|(ti, stack)| stack.iter().position(|&x| x == w).map(|p| (mi, si, ti, p)))
            })
        })
    }

    /// Keeps only the marks of windows waiting in a slot: a window shown (whatever showed it), closed or gone to
    /// `pending` loses its mark for good. Every method that changes a slot's tabs or current tab ends with this
    /// (their early returns change nothing); the random walk checks the rule after every operation, so a method
    /// that forgets it fails there.
    fn settle(&mut self) {
        if self.attention.is_empty() {
            return;
        }
        let waiting: BTreeSet<WinId> = self.placements().filter(|p| !p.on_top).map(|p| p.win).collect();
        self.attention.retain(|w| waiting.contains(w));
    }

    /// Placed or pending: every open refuses such an id, so a window can never be in two places.
    fn has(&self, w: WinId) -> bool {
        self.where_is(w).is_some() || self.pending.contains(&w)
    }

    fn index_of(&self, monitor: &str) -> Option<usize> {
        self.monitors.iter().position(|m| m.name == monitor)
    }

    fn focused_slot_mut(&mut self) -> Option<&mut Slot> {
        let mon = self.monitors.get_mut(self.focused_monitor)?;
        mon.slots.get_mut(mon.focus)
    }

    /// The focused monitor and the slot next to its focused one in `d`.
    fn neighbour(&self, d: Dir) -> Option<(usize, usize)> {
        let m = self.focused_monitor;
        let mon = self.monitors.get(m)?;
        let to = mon.layout.neighbour(mon.focus, d)?;
        (to < mon.slots.len()).then_some((m, to))
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
        s.open_stacked(7);
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

    /// A slot's tabs, bottom to top each.
    fn tabs(s: &Stacks, m: usize, slot: usize) -> Vec<Vec<WinId>> {
        s.monitors[m].slots[slot].tabs.clone()
    }

    fn current(s: &Stacks, m: usize, slot: usize) -> usize {
        s.monitors[m].slots[slot].current
    }

    /// Each id exactly once; every monitor has one slot per layout slot and its focus in range; no tab is empty
    /// and every slot's current tab is in range (0 when it has none); tiles and placements agree with the slots
    /// and with focus.
    fn assert_invariants(s: &Stacks) {
        let mut seen = BTreeSet::new();
        for m in &s.monitors {
            assert!(!m.slots.is_empty(), "{}: no slots", m.name);
            assert_eq!(m.slots.len(), m.layout.slots.len(), "{}: slots vs layout", m.name);
            assert!(m.focus < m.slots.len(), "{}: focus {} out of range", m.name, m.focus);
            for (i, slot) in m.slots.iter().enumerate() {
                assert!(
                    slot.tabs.iter().all(|t| !t.is_empty()),
                    "{} slot {i}: an empty tab",
                    m.name
                );
                if slot.tabs.is_empty() {
                    assert_eq!(slot.current, 0, "{} slot {i}: current without tabs", m.name);
                } else {
                    assert!(
                        slot.current < slot.tabs.len(),
                        "{} slot {i}: current out of range",
                        m.name
                    );
                }
                assert_eq!(slot.len(), slot.tabs.iter().map(Vec::len).sum::<usize>());
                assert_eq!(slot.is_empty(), slot.tabs.is_empty());
                assert_eq!(slot.top(), slot.tabs.get(slot.current).and_then(|t| t.last().copied()));
            }
            for w in m.slots.iter().flat_map(|sl| sl.tabs.iter().flatten()) {
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
            .flat_map(|m| &m.slots)
            .filter(|sl| !sl.is_empty())
            .count();
        let tiles = s.tiles();
        assert_eq!(tiles.len(), non_empty, "one tile per non-empty slot");
        for t in &tiles {
            let slot = &s.monitors[t.monitor].slots[t.slot];
            assert_eq!(
                (t.tabs, t.depth, t.windows),
                (slot.tabs.len(), slot.tabs[slot.current].len(), slot.len())
            );
        }
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
            let (m, sl, tab, pos) = s.where_is(p.win).expect("placed windows are found");
            assert_eq!((m, sl, tab), (p.monitor, p.slot, p.tab));
            let slot = &s.monitors[m].slots[sl];
            assert_eq!(p.on_top, tab == slot.current && pos + 1 == slot.tabs[tab].len());
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

        for &w in &s.attention {
            let p = placements.iter().find(|p| p.win == w);
            assert!(
                p.is_some_and(|p| !p.on_top),
                "window {w} marked but not waiting in a slot"
            );
        }
    }

    fn ids(s: &Stacks) -> BTreeSet<WinId> {
        s.monitors
            .iter()
            .flat_map(|m| m.slots.iter().flat_map(|sl| sl.tabs.iter().flatten()))
            .chain(&s.pending)
            .copied()
            .collect()
    }

    /// wall with slot 0 holding the tabs [1], [2, 3], [4], the middle one current.
    fn three_tabs() -> Stacks {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.open_tab(2, None, None);
        s.open_stacked(3);
        s.open_tab(4, None, None);
        s.cycle_tab(Cycle::Prev);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3], vec![4]]);
        assert_eq!(current(&s, 0, 0), 1);
        assert_invariants(&s);
        s
    }

    #[test]
    fn open_stacked_puts_the_newest_on_top_of_one_tab() {
        let mut s = wall_monitor();
        for w in 1..=3 {
            s.open_stacked(w);
            assert_invariants(&s);
        }
        assert_eq!(tabs(&s, 0, 0), [vec![1, 2, 3]]);
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
                tabs: 1,
                depth: 3,
                windows: 3,
            }
        );
        assert_eq!(s.where_is(1), Some((0, 0, 0, 0)));
        assert_eq!(s.where_is(3), Some((0, 0, 0, 2)));
        assert_eq!(s.where_is(9), None);
    }

    #[test]
    fn open_stacked_goes_on_the_current_tab() {
        let mut s = three_tabs();
        s.open_stacked(5);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3, 5], vec![4]]);
        assert_eq!(current(&s, 0, 0), 1);
        assert_eq!(s.focused_window(), Some(5));
    }

    #[test]
    fn open_tab_goes_right_after_the_current_tab_and_becomes_current() {
        let mut s = three_tabs();
        s.open_tab(5, None, None);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3], vec![5], vec![4]]);
        assert_eq!(current(&s, 0, 0), 2);
        assert_eq!(s.focused_window(), Some(5));
        let tile = s.tiles()[0];
        assert_eq!((tile.tabs, tile.depth, tile.windows), (4, 1, 5));
    }

    #[test]
    fn open_tab_targets_a_slot_and_focuses_it() {
        let mut s = wall_monitor();
        s.add_monitor("second", UHD, layout("halves"));
        s.open_stacked(1);
        s.open_tab(2, Some("second"), Some(1));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 1, 1), [vec![2]]);
        assert_eq!((s.focused_monitor, s.monitors[1].focus), (1, 1));
        assert_eq!(s.focused_window(), Some(2));
        // A monitor alone: its focused slot.
        s.focus_at("winit", 3);
        s.open_tab(3, Some("second"), None);
        assert_eq!(tabs(&s, 1, 1), [vec![2], vec![3]]);
        assert_eq!(s.focused_monitor, 1);
        // Nothing given: the focused slot.
        s.open_tab(4, None, None);
        assert_eq!(tabs(&s, 1, 1), [vec![2], vec![3], vec![4]]);
        assert_invariants(&s);
    }

    #[test]
    fn open_tab_with_an_unknown_target_uses_the_focused_slot() {
        let mut s = wall_monitor();
        s.add_monitor("second", UHD, layout("halves"));
        s.focus_at("winit", 3);
        s.open_tab(1, Some("nope"), Some(1));
        s.open_tab(2, Some("second"), Some(9));
        s.open_tab(3, None, Some(9));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 3), [vec![1], vec![2], vec![3]]);
        assert_eq!((s.focused_monitor, s.monitors[0].focus), (0, 3));
        // No monitor at all: pending, as any open.
        let mut none = Stacks::new();
        none.open_tab(1, Some("nope"), Some(0));
        assert_eq!(none.pending, [1]);
        assert_invariants(&none);
    }

    #[test]
    fn open_on_puts_a_window_on_its_parents_stack_without_raising_it() {
        let mut s = three_tabs();
        s.focus_slot(Dir::Right);
        s.open_stacked(9);
        let before = (s.focused_window(), s.monitors[0].focus);
        // The parent's tab is in the background: the window waits there.
        assert!(s.open_on(5, 1));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1, 5], vec![2, 3], vec![4]]);
        assert_eq!(current(&s, 0, 0), 1);
        assert_eq!((s.focused_window(), s.monitors[0].focus), before);
        // On the parent's stack even when the parent is beneath its top.
        assert!(s.open_on(6, 2));
        assert_eq!(tabs(&s, 0, 0), [vec![1, 5], vec![2, 3, 6], vec![4]]);
        assert_eq!(s.tiles()[0].win, 6);
        assert_eq!(s.focused_window(), Some(9), "focus stays");
        assert_invariants(&s);
        // An unknown parent: nothing, for the caller to fall back.
        let before = s.clone();
        assert!(!s.open_on(7, 42));
        assert_eq!(s, before);
    }

    #[test]
    fn opening_an_id_twice_keeps_one() {
        let mut s = three_tabs();
        let before = s.clone();
        s.open_stacked(1);
        s.open_tab(2, None, None);
        assert!(!s.open_on(3, 4));
        assert!(!s.open_on(4, 4));
        assert_invariants(&s);
        assert_eq!(s, before);
        let mut pending = Stacks::new();
        pending.open_stacked(1);
        pending.open_tab(1, None, None);
        pending.open_stacked(1);
        assert_eq!(pending.pending, [1]);
    }

    #[test]
    fn placements_list_every_placed_window() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.open_stacked(2);
        s.open_tab(3, None, None);
        s.focus_slot(Dir::Right);
        s.open_stacked(4);
        let cell = rect(0, 0, 1280, 720);
        let hero = rect(1280, 0, 2560, 1440);
        let p = |win, slot, tab, rect, on_top, focused| Placement {
            win,
            monitor: 0,
            slot,
            tab,
            rect,
            on_top,
            focused,
        };
        assert_eq!(
            s.placements().collect::<Vec<_>>(),
            vec![
                p(1, 0, 0, cell, false, false),
                p(2, 0, 0, cell, false, false),
                p(3, 0, 1, cell, true, false),
                p(4, 3, 0, hero, true, true)
            ]
        );
        assert_invariants(&s);
    }

    #[test]
    fn closing_the_top_reveals_the_one_beneath() {
        let mut s = wall_monitor();
        (1..=3).for_each(|w| s.open_stacked(w));
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
        assert!(tabs(&s, 0, 0).is_empty(), "the emptied tab went");
        // Closing an unknown id does nothing.
        s.close(42);
        assert_invariants(&s);
    }

    #[test]
    fn closing_the_current_tab_shows_the_next_else_the_previous() {
        let mut s = three_tabs();
        s.close(3);
        s.close(2);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![4]]);
        assert_eq!(s.focused_window(), Some(4), "the next took its place");
        s.close(4);
        assert_invariants(&s);
        assert_eq!(s.focused_window(), Some(1), "no next: the previous");
    }

    #[test]
    fn closing_another_tab_keeps_the_current_one() {
        let mut s = three_tabs();
        s.close(1);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![2, 3], vec![4]]);
        assert_eq!(s.focused_window(), Some(3), "a tab before it went");
        s.close(4);
        assert_invariants(&s);
        assert_eq!(s.focused_window(), Some(3), "a tab after it went");
        // A window beneath the top of the current tab: the top stays.
        s.close(2);
        assert_eq!(tabs(&s, 0, 0), [vec![3]]);
        assert_invariants(&s);
    }

    #[test]
    fn closing_elsewhere_keeps_focus() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.focus_slot(Dir::Right);
        s.open_stacked(2);
        s.close(1);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].focus, 3);
        assert_eq!(s.focused_window(), Some(2));
    }

    #[test]
    fn cycle_tab_walks_the_tabs_and_wraps() {
        let mut s = three_tabs();
        let windows = tabs(&s, 0, 0);
        s.cycle_tab(Cycle::Next);
        assert_eq!((current(&s, 0, 0), s.focused_window()), (2, Some(4)));
        s.cycle_tab(Cycle::Next);
        assert_eq!((current(&s, 0, 0), s.focused_window()), (0, Some(1)), "wraps forward");
        s.cycle_tab(Cycle::Prev);
        assert_eq!(current(&s, 0, 0), 2, "wraps back");
        assert_eq!(tabs(&s, 0, 0), windows, "tabs never move");
        assert_invariants(&s);
    }

    #[test]
    fn cycling_one_tab_or_an_empty_slot_does_nothing() {
        let mut s = wall_monitor();
        s.cycle_tab(Cycle::Next);
        s.cycle_tab(Cycle::Prev);
        assert_invariants(&s);
        assert_eq!(s, wall_monitor());
        (1..=3).for_each(|w| s.open_stacked(w));
        let before = s.clone();
        s.cycle_tab(Cycle::Next);
        s.cycle_tab(Cycle::Prev);
        assert_eq!(s, before, "one tab, however deep");
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
        s.open_stacked(7);
        assert_eq!(tabs(&s, 1, 1), [vec![7]]);
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
    fn move_stack_takes_the_whole_current_tab_and_focus_follows() {
        let mut s = wall_monitor();
        (1..=2).for_each(|w| s.open_stacked(w));
        s.move_stack(Dir::Right);
        assert_invariants(&s);
        assert!(tabs(&s, 0, 0).is_empty());
        assert_eq!(tabs(&s, 0, 3), [vec![1, 2]]);
        assert_eq!(s.monitors[0].focus, 3);
        assert_eq!(s.focused_window(), Some(2));
        let hero = s.tiles().into_iter().find(|t| t.win == 2).expect("hero tile");
        assert_eq!(hero.rect, rect(1280, 0, 2560, 1440));
        assert!(hero.focused);
    }

    #[test]
    fn move_stack_lands_as_a_tab_after_the_neighbours_current() {
        let mut s = three_tabs();
        s.focus_slot(Dir::Right);
        s.open_stacked(5);
        s.open_tab(6, None, None);
        s.cycle_tab(Cycle::Prev);
        s.focus_slot(Dir::Left);
        s.move_stack(Dir::Right);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 3), [vec![5], vec![2, 3], vec![6]]);
        assert_eq!(current(&s, 0, 3), 1);
        assert_eq!(s.focused_window(), Some(3));
        // The source shows the tab that took the moved one's place.
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![4]]);
        assert_eq!(s.tiles()[0].win, 4);
    }

    #[test]
    fn move_stack_at_the_edge_or_from_an_empty_slot_does_nothing() {
        let mut s = wall_monitor();
        s.move_stack(Dir::Right);
        assert_eq!(s.monitors[0].focus, 0);
        s.open_stacked(1);
        s.move_stack(Dir::Left);
        s.move_stack(Dir::Up);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1]]);
        assert_eq!(s.monitors[0].focus, 0);
    }

    #[test]
    fn swap_slot_swaps_whole_slots_and_focus_follows() {
        let mut s = three_tabs();
        s.focus_slot(Dir::Right);
        s.open_stacked(5);
        s.focus_slot(Dir::Left);
        s.swap_slot(Dir::Right);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![5]]);
        assert_eq!(tabs(&s, 0, 3), [vec![1], vec![2, 3], vec![4]]);
        assert_eq!(current(&s, 0, 3), 1, "the current tab goes along");
        assert_eq!(s.monitors[0].focus, 3);
        assert_eq!(s.focused_window(), Some(3));
        // At the edge: nothing.
        s.swap_slot(Dir::Up);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].focus, 3);
    }

    #[test]
    fn split_makes_the_top_a_tab_of_its_own_after_its_stack() {
        let mut s = three_tabs();
        s.open_stacked(5);
        s.split();
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3], vec![5], vec![4]]);
        assert_eq!(current(&s, 0, 0), 2);
        assert_eq!(s.focused_window(), Some(5));
        s.cycle_tab(Cycle::Prev);
        s.split();
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2], vec![3], vec![5], vec![4]]);
        assert_eq!(current(&s, 0, 0), 2);
        assert_invariants(&s);
    }

    #[test]
    fn split_of_one_window_or_an_empty_slot_does_nothing() {
        let mut s = wall_monitor();
        s.split();
        assert_eq!(s, wall_monitor());
        s.open_stacked(1);
        s.open_tab(2, None, None);
        let before = s.clone();
        s.split();
        assert_eq!(s, before);
        assert_invariants(&s);
    }

    /// wall with windows in all six slots: slot i holds one tab [10i+1, 10i+2].
    fn full_wall() -> Stacks {
        let mut s = wall_monitor();
        for i in 0..6 {
            s.focus_at("winit", i);
            s.open_stacked(10 * i as u64 + 1);
            s.open_stacked(10 * i as u64 + 2);
        }
        assert_invariants(&s);
        s
    }

    #[test]
    fn set_layout_appends_the_extra_slots_tabs_to_the_last_slot() {
        let mut s = full_wall();
        s.focus_at("winit", 1);
        s.open_tab(13, None, None);
        s.focus_at("winit", 5);
        s.set_layout("winit", layout("halves"));
        assert_invariants(&s);
        let m = &s.monitors[0];
        assert_eq!(m.layout.name, "halves");
        assert_eq!(tabs(&s, 0, 0), [vec![1, 2]]);
        assert_eq!(
            tabs(&s, 0, 1),
            [
                vec![11, 12],
                vec![13],
                vec![21, 22],
                vec![31, 32],
                vec![41, 42],
                vec![51, 52]
            ]
        );
        assert_eq!(current(&s, 0, 1), 1, "the last slot's current tab stays");
        // Focus was on slot 5, clamped to the last slot.
        assert_eq!(s.monitors[0].focus, 1);
        assert_eq!(s.focused_window(), Some(13));
        assert_eq!(
            s.tiles().iter().map(|t| t.rect).collect::<Vec<_>>(),
            vec![rect(0, 0, 1920, 2160), rect(1920, 0, 1920, 2160)]
        );
    }

    #[test]
    fn set_layout_into_an_empty_last_slot_shows_the_first_moved_tab() {
        let mut s = wall_monitor();
        s.focus_at("winit", 2);
        s.open_stacked(1);
        s.focus_at("winit", 3);
        s.open_stacked(2);
        s.set_layout("winit", layout("halves"));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 1), [vec![1], vec![2]]);
        assert_eq!(s.tiles()[0].win, 1);
    }

    #[test]
    fn set_layout_round_trip_is_not_an_identity() {
        // Going back fills the new slots in order; the merged tabs stay merged in slot 1 and slots 2..5 come back
        // empty. Nothing is closed, but the original spread is not restored.
        let mut s = full_wall();
        s.set_layout("winit", layout("halves"));
        s.set_layout("winit", layout("wall"));
        assert_invariants(&s);
        assert_eq!(s.monitors[0].layout.name, "wall");
        assert_eq!(tabs(&s, 0, 0), [vec![1, 2]]);
        assert_eq!(tabs(&s, 0, 1).concat(), [11, 12, 21, 22, 31, 32, 41, 42, 51, 52]);
        assert!((2..6).all(|i| tabs(&s, 0, i).is_empty()));
        assert_eq!(s.monitors[0].focus, 1);
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
    fn focus_window_focuses_a_top_and_raises_nothing() {
        let mut s = three_tabs();
        s.add_monitor("second", UHD, single());
        s.focus_monitor("second");
        s.open_stacked(9);
        assert!(s.focus_window(3), "the top of the current tab");
        assert_invariants(&s);
        assert_eq!((s.focused_monitor, s.monitors[0].focus), (0, 0));
        assert_eq!(s.focused_window(), Some(3));
        // Beneath its top, in a background tab, or unknown: nothing.
        s.focus_monitor("second");
        let before = s.clone();
        assert!(!s.focus_window(2));
        assert!(!s.focus_window(4));
        assert!(!s.focus_window(99));
        assert_eq!(s, before);
        assert_invariants(&s);
    }

    #[test]
    fn remove_the_focused_monitor_sends_its_tabs_to_the_first() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.add_monitor("second", UHD, layout("halves"));
        s.focus_monitor("second");
        s.open_stacked(2);
        s.open_tab(3, None, None);
        s.focus_slot(Dir::Right);
        s.open_stacked(4);
        s.remove_monitor("second");
        assert_invariants(&s);
        assert_eq!(s.monitors.len(), 1);
        assert_eq!(s.focused_monitor, 0);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2], vec![3], vec![4]]);
        assert_eq!(s.focused_window(), Some(1), "the current tab stays");
        // Unknown names do nothing.
        s.remove_monitor("nope");
        assert_invariants(&s);
    }

    #[test]
    fn remove_another_monitor_sends_its_tabs_to_the_focused_one() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.add_monitor("b", UHD, single());
        s.add_monitor("c", UHD, layout("halves"));
        s.focus_at("c", 1);
        s.open_stacked(2);
        s.remove_monitor("winit");
        assert_invariants(&s);
        assert_eq!(s.monitors[s.focused_monitor].name, "c");
        assert_eq!(tabs(&s, s.focused_monitor, 1), [vec![2], vec![1]]);
        assert_eq!(s.focused_window(), Some(2));
        assert!(tabs(&s, 0, 0).is_empty(), "b got nothing");
    }

    #[test]
    fn windows_without_a_monitor_wait_for_one() {
        let mut s = Stacks::new();
        assert_invariants(&s);
        s.open_stacked(1);
        s.open_tab(2, None, None);
        assert_invariants(&s);
        assert_eq!(s.pending, vec![1, 2]);
        assert_eq!(s.focused_window(), None);
        assert!(s.tiles().is_empty());
        s.close(1);
        s.add_monitor("winit", UHD, layout("wall"));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![2]]);
        s.open_tab(3, None, None);
        s.remove_monitor("winit");
        assert_invariants(&s);
        assert_eq!(s.pending, vec![2, 3]);
        s.add_monitor("next", UHD, single());
        assert_eq!(tabs(&s, 0, 0), [vec![2, 3]], "one tab, as a stack");
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
        s.open_stacked(1);
        s.focus_slot(Dir::Right);
        s.open_stacked(2);
        s.open_stacked(3);
        s.open_tab(4, None, None);
        assert_invariants(&s);
        assert_eq!(s.monitors().len(), 1);
        assert_eq!(s.focused_monitor(), 0);
        assert!(s.pending().is_empty());
        let m = &s.monitors()[0];
        assert_eq!(m.name(), "winit");
        assert_eq!(m.area(), UHD);
        assert_eq!(m.layout().name, "wall");
        assert_eq!(m.focus(), 3);
        assert_eq!(m.slots()[0].tabs(), [vec![1]]);
        let hero = &m.slots()[3];
        assert_eq!(hero.tabs(), [vec![2, 3], vec![4]]);
        assert_eq!((hero.current(), hero.top(), hero.len()), (1, Some(4), 3));
        assert!(!hero.is_empty());
        assert!(m.slots()[1].is_empty());
        assert_eq!(m.slots()[1].top(), None);
        let mut empty = Stacks::new();
        empty.open_stacked(5);
        assert_invariants(&empty);
        assert_eq!(empty.pending(), &[5]);
    }

    #[test]
    fn set_area_moves_the_tiles() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        let small = rect(0, 0, 1920, 1080);
        s.set_area("winit", small);
        s.set_area("nope", UHD);
        assert_invariants(&s);
        assert_eq!(s.monitors[0].area, small);
        assert_eq!(s.tiles()[0].rect, rect(0, 0, 640, 360));
    }

    #[test]
    fn only_a_waiting_window_can_be_marked() {
        let mut s = three_tabs();
        s.mark(3);
        assert!(!s.is_marked(3), "the top of the current tab is already seen");
        s.mark(99);
        assert!(!s.is_marked(99), "unknown");
        s.mark(2);
        s.mark(4);
        assert!(
            s.is_marked(2) && s.is_marked(4),
            "beneath the top, and in a background tab"
        );
        assert_invariants(&s);
        s.clear_mark(2);
        s.clear_mark(99);
        assert!(!s.is_marked(2) && s.is_marked(4));
        assert_invariants(&s);

        let mut none = Stacks::new();
        none.open_stacked(5);
        none.mark(5);
        assert!(!none.is_marked(5), "pending is not placed");
    }

    #[test]
    fn a_mark_clears_for_good_when_its_window_comes_on_top() {
        // Tabs [1], [2, 3], [4], the middle one current.
        let mut s = three_tabs();
        s.mark(1);
        s.mark(2);
        s.mark(4);
        s.cycle_tab(Cycle::Next);
        assert!(!s.is_marked(4), "shown by cycle_tab");
        s.cycle_tab(Cycle::Prev);
        assert!(!s.is_marked(4), "going back beneath does not bring it back");
        s.close(3);
        assert!(!s.is_marked(2), "uncovered by a close");
        s.move_stack(Dir::Right);
        // [2] left; the tab that took its place, [4], shows.
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![4]]);
        assert_eq!(s.monitors[0].slots[0].top(), Some(4));
        assert!(s.is_marked(1));
        s.focus_at("winit", 0);
        s.move_stack(Dir::Right);
        assert_eq!(s.monitors[0].slots[0].top(), Some(1));
        assert!(!s.is_marked(1), "shown when the last tab left");
        assert_invariants(&s);
    }

    #[test]
    fn closing_a_marked_window_drops_its_mark() {
        let mut s = three_tabs();
        s.mark(2);
        s.close(2);
        assert!(!s.is_marked(2));
        assert!(s.attention.is_empty());
        assert_invariants(&s);
    }

    #[test]
    fn layout_merges_and_monitor_loss_clear_what_they_show() {
        // Slot 4 holds the tabs [1], [2, 3], the second current; 1 and 2 are marked.
        let mut s = wall_monitor();
        s.focus_at("winit", 4);
        s.open_stacked(1);
        s.open_tab(2, None, None);
        s.open_stacked(3);
        s.mark(1);
        s.mark(2);
        // halves' last slot was empty, so it shows the first merged tab: 1 comes on top, 2 stays beneath.
        s.set_layout("winit", layout("halves"));
        assert_eq!(tabs(&s, 0, 1), [vec![1], vec![2, 3]]);
        assert_eq!(current(&s, 0, 1), 0);
        assert!(!s.is_marked(1) && s.is_marked(2));
        assert_invariants(&s);

        // The same when a monitor's tabs land in an empty focused slot.
        s.focus_at("winit", 1);
        s.cycle_tab(Cycle::Next);
        s.mark(1);
        assert!(s.is_marked(1));
        s.add_monitor("other", UHD, layout("wall"));
        s.focus_monitor("other");
        s.remove_monitor("winit");
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3]]);
        assert_eq!(current(&s, 0, 0), 0);
        assert!(!s.is_marked(1) && s.is_marked(2));
        assert_invariants(&s);

        // With no monitor left the windows wait in `pending`, which holds no marks.
        let mut s = three_tabs();
        s.mark(2);
        s.remove_monitor("winit");
        assert!(!s.is_marked(2));
        assert_invariants(&s);
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

    /// Every slot's tabs: what only focus and current-tab changes must keep.
    fn all_tabs(s: &Stacks) -> Vec<Vec<Vec<Vec<WinId>>>> {
        s.monitors
            .iter()
            .map(|m| m.slots.iter().map(|sl| sl.tabs.clone()).collect())
            .collect()
    }

    /// One seeded walk of 1,000 operations; returns the most windows open at once and the steps ending with a
    /// mark.
    fn walk(seed: u64) -> (usize, usize) {
        let layouts = [layout("wall"), layout("halves"), single()];
        let mut rng = Rng(seed);
        let mut s = wall_monitor();
        let mut live: BTreeSet<WinId> = BTreeSet::new();
        let mut next_id: WinId = 1;
        let mut next_monitor = 0;
        let mut most = 0;
        let mut marked_steps = 0;
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
                rng.below(20)
            };
            match op {
                0 | 1 => {
                    s.open_stacked(next_id);
                    live.insert(next_id);
                    next_id += 1;
                }
                2 => {
                    let m = name(&mut rng, &s);
                    s.open_tab(next_id, Some(&m), Some(rng.below(7) as usize));
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
                    assert_eq!(all_tabs(&s), all_tabs(&before), "seed {seed} step {step}: focus_slot");
                }
                5 => {
                    s.cycle_tab(if rng.below(2) == 0 { Cycle::Next } else { Cycle::Prev });
                    assert_eq!(all_tabs(&s), all_tabs(&before), "seed {seed} step {step}: cycle_tab");
                }
                6 => s.move_stack(rng.dir()),
                7 => s.swap_slot(rng.dir()),
                8 => {
                    let m = name(&mut rng, &s);
                    s.set_layout(&m, layouts[rng.below(3) as usize].clone());
                }
                9 => {
                    s.focus_window(pick(&mut rng, &live));
                    assert_eq!(all_tabs(&s), all_tabs(&before), "seed {seed} step {step}: focus_window");
                }
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
                13 => {
                    let m = name(&mut rng, &s);
                    s.focus_monitor(&m);
                }
                14 | 15 => {
                    let parent = pick(&mut rng, &live);
                    if s.open_on(next_id, parent) {
                        live.insert(next_id);
                        next_id += 1;
                    } else {
                        assert_eq!(s, before, "seed {seed} step {step}: refused open_on changed things");
                    }
                }
                16 => s.split(),
                17 | 18 => {
                    let w = pick(&mut rng, &live);
                    s.mark(w);
                    let waiting = before.placements().any(|p| p.win == w && !p.on_top);
                    assert_eq!(s.is_marked(w), waiting, "seed {seed} step {step}: mark {w}");
                }
                _ => s.clear_mark(pick(&mut rng, &live)),
            }
            if !matches!(op, 17 | 18) {
                assert!(
                    s.attention.is_subset(&before.attention),
                    "seed {seed} step {step}: only mark marks"
                );
            }
            assert_invariants(&s);
            assert_eq!(ids(&s), live, "seed {seed} step {step}");
            most = most.max(live.len());
            marked_steps += usize::from(!s.attention.is_empty());
        }
        (most, marked_steps)
    }

    #[test]
    fn random_walks_lose_nothing() {
        let mut seed: u64 = 0x7472_616e_6365;
        for _ in 0..32 {
            let (most, marked_steps) = walk(seed);
            assert!(
                marked_steps > 50,
                "seed {seed}: marks held on {marked_steps} steps only"
            );
            assert!(
                (4..=40).contains(&most),
                "seed {seed}: at most {most} open, the walk is unbalanced"
            );
            seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1) | 1;
        }
    }
}
