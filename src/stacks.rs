//! Workspaces of slots, slots of tabs, tabs of stacked windows: the state machine behind every window move.
//!
//! Pure: windows are opaque ids, monitors are numbered names with an area (a screen keeps its number while it
//! lives; a new one takes the lowest free). Each monitor shows one workspace. Workspace k is locked to the screen
//! numbered k while that screen exists, and exists meanwhile; any other lives on the monitor it was made on (or was
//! moved to when its screen went) and goes away once it is not locked, not shown and holds no window. Every
//! operation acts on the focused monitor's shown workspace unless it names another, keeps each window in exactly
//! one place, and does nothing when it cannot apply (an edge, an empty slot, an unknown id). The state is private
//! so nothing outside these methods can break that; callers read it through accessors.
//!
//! In a session (`session.rs`) some numbers are owned by other machines: they never exist here, and switching or
//! moving to one says so (`Outcome::Remote`) instead of making it. Alone, no number is.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Cycle;
use crate::layout::{Dir, Layout, Rect};

pub type WinId = u64;

/// Workspaces and screens are numbered 1..=WORKSPACES. A monitor past that many has no number and is ignored.
pub const WORKSPACES: u8 = 10;

/// One layout slot: its tabs, left to right, each a stack of windows (bottom to top), and the one shown. Only
/// the top window of the current tab is drawn; a tab is never empty, so an empty slot has no tabs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Slot {
    tabs: Vec<Vec<WinId>>,
    /// The tab shown; 0 when there is none.
    current: usize,
}

/// A layout's worth of slots, on one monitor, drawn only while that monitor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// 1..=WORKSPACES.
    number: u8,
    /// The monitor it lives on, by index: the one it is locked to, else the one it was made on or moved to.
    monitor: usize,
    layout: Layout,
    /// One per layout slot, in slot order.
    slots: Vec<Slot>,
    /// The focused slot.
    focus: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    name: String,
    /// Its screen number, 1..=WORKSPACES, kept while it lives; it locks the workspace of that number.
    number: u8,
    area: Rect,
    /// What a workspace made on this monitor starts with.
    default_layout: Layout,
    /// The number of the workspace it shows, always one living on it.
    shown: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stacks {
    /// In number order.
    monitors: Vec<Monitor>,
    /// The workspaces that exist, by number; none without a monitor.
    workspaces: Vec<Workspace>,
    focused_monitor: usize,
    /// Windows with no monitor to live on (none yet, or the last one went away), oldest first. The next monitor
    /// added takes them, as one stack, into its focused slot.
    pending: Vec<WinId>,
    /// Windows that asked to be seen while not focused, and how they were then (see `Mark`, which says what clears
    /// each). A mark also goes when its window closes or loses its monitor, and never comes back by itself.
    attention: BTreeMap<WinId, Mark>,
    /// Workspace numbers another machine owns: never a workspace or screen here.
    foreign: BTreeSet<u8>,
}

/// What `workspace` and `move_to_workspace` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Nothing to do (see each method).
    Nothing,
    /// Another machine owns that number: nothing was done here, the session decides.
    Remote(u8),
}

/// What a window was when it was marked, which decides what clears the mark (see `settle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Beneath its slot's top, or in a hidden workspace: cleared once it is shown, focused or not.
    Waiting,
    /// Shown in a slot without focus: cleared once it is focused. Covered or hidden meanwhile, it stays marked.
    Unfocused,
}

/// What is drawn: the top window of a non-empty slot's current tab in a shown workspace, at the slot's rectangle.
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

/// Where a placed window is: every window in a slot of any workspace has one, whether it is drawn (`shown` and
/// `on_top`) or waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub win: WinId,
    /// Its workspace's number.
    pub workspace: u8,
    /// The monitor that workspace lives on.
    pub monitor: usize,
    pub slot: usize,
    pub tab: usize,
    /// The slot's rectangle: waiting windows are kept at this size so showing them is instant.
    pub rect: Rect,
    /// Its workspace is the one its monitor shows.
    pub shown: bool,
    /// On top of its slot's current tab (drawn only when `shown`).
    pub on_top: bool,
    /// On top of the focused slot of the focused monitor's shown workspace.
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

impl Workspace {
    /// A fresh one: empty slots, the first focused.
    fn new(number: u8, monitor: usize, layout: Layout) -> Self {
        Self {
            number,
            monitor,
            slots: vec![Slot::default(); layout.slots.len()],
            layout,
            focus: 0,
        }
    }

    #[must_use]
    pub fn number(&self) -> u8 {
        self.number
    }

    #[must_use]
    pub fn monitor(&self) -> usize {
        self.monitor
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

    fn is_empty(&self) -> bool {
        self.slots.iter().all(Slot::is_empty)
    }
}

impl Monitor {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn number(&self) -> u8 {
        self.number
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        self.area
    }

    #[must_use]
    pub fn default_layout(&self) -> &Layout {
        &self.default_layout
    }

    /// The number of the workspace it shows.
    #[must_use]
    pub fn shown(&self) -> u8 {
        self.shown
    }
}

impl Stacks {
    #[must_use]
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    /// Every existing workspace, by number.
    #[must_use]
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// The workspace monitor `monitor` (an index) shows; None for no such monitor.
    #[must_use]
    pub fn shown(&self, monitor: usize) -> Option<&Workspace> {
        Some(&self.workspaces[self.shown_index(monitor)?])
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

    /// Focuses a slot of a monitor's shown workspace by pointer, empty or not; an unknown monitor or slot does
    /// nothing.
    pub fn focus_at(&mut self, monitor: &str, slot: usize) {
        if let Some(m) = self.index_of(monitor)
            && let Some(wi) = self.shown_index(m)
            && slot < self.workspaces[wi].slots.len()
        {
            self.focused_monitor = m;
            self.workspaces[wi].focus = slot;
        }
        self.settle();
    }

    /// Focuses a monitor, keeping its shown workspace's focused slot; unknown names do nothing.
    pub fn focus_monitor(&mut self, monitor: &str) {
        if let Some(i) = self.index_of(monitor) {
            self.focused_monitor = i;
        }
        self.settle();
    }

    /// Every window in a slot, in one pass: workspace by workspace, slot by slot, tab by tab, bottom to top.
    // No #[must_use]: iterators already carry it.
    pub fn placements(&self) -> impl Iterator<Item = Placement> + '_ {
        self.workspaces.iter().flat_map(move |ws| {
            let mon = &self.monitors[ws.monitor];
            let shown = mon.shown == ws.number;
            let focused_ws = shown && ws.monitor == self.focused_monitor;
            let rects = ws.layout.rects(mon.area);
            ws.slots
                .iter()
                .zip(rects)
                .enumerate()
                .flat_map(move |(si, (slot, rect))| {
                    slot.tabs.iter().enumerate().flat_map(move |(ti, stack)| {
                        stack.iter().enumerate().map(move |(pos, &win)| {
                            let on_top = ti == slot.current && pos + 1 == stack.len();
                            Placement {
                                win,
                                workspace: ws.number,
                                monitor: ws.monitor,
                                slot: si,
                                tab: ti,
                                rect,
                                shown,
                                on_top,
                                focused: on_top && focused_ws && si == ws.focus,
                            }
                        })
                    })
                })
        })
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// The number the next screen added takes: the lowest one neither a screen's nor owned elsewhere, None when
    /// all are taken. Backends name an output after it before adding it.
    #[must_use]
    pub fn next_screen_number(&self) -> Option<u8> {
        (1..=WORKSPACES).find(|&k| self.monitors.iter().all(|m| m.number != k) && !self.foreign.contains(&k))
    }

    /// Adds an output as the screen numbered `next_screen_number` (see `add_monitor_numbered`); with none free it
    /// is ignored.
    pub fn add_monitor(&mut self, name: &str, area: Rect, default_layout: Layout) {
        if let Some(number) = self.next_screen_number() {
            self.add_monitor_numbered(name, area, default_layout, number);
        }
    }

    /// Adds an output as the screen numbered `number`, which locks workspace `number` and shows it: moved here
    /// intact if it lived on another monitor (which, if it showed it, then shows its lowest remaining one), else
    /// made from `default_layout`. Nothing else moves. Pending windows move into it, as one tab of its first slot.
    /// Layouts must be valid (`Layout::validate`). `false` (and nothing done) for a layout with no slots, a name
    /// already present, or a number out of range, already a screen's or owned elsewhere.
    pub fn add_monitor_numbered(&mut self, name: &str, area: Rect, default_layout: Layout, number: u8) -> bool {
        // Zero slots would break "one slot per layout slot" even in release; anything else invalid is a caller
        // bug.
        let taken = self.monitors.iter().any(|m| m.number == number || m.name == name);
        if !(1..=WORKSPACES).contains(&number)
            || taken
            || self.foreign.contains(&number)
            || default_layout.slots.is_empty()
        {
            return false;
        }
        debug_assert!(
            default_layout.validate().is_ok(),
            "add_monitor needs a valid layout: {default_layout:?}"
        );
        // Monitors stay in number order, so the ones after it shift up.
        let at = self.monitors.partition_point(|m| m.number < number);
        for w in &mut self.workspaces {
            if w.monitor >= at {
                w.monitor += 1;
            }
        }
        if !self.monitors.is_empty() && self.focused_monitor >= at {
            self.focused_monitor += 1;
        }
        let monitor = Monitor {
            name: name.to_owned(),
            number,
            area,
            default_layout,
            shown: number,
        };
        self.monitors.insert(at, monitor);
        match self.index_of_workspace(number) {
            Some(wi) => {
                let from = self.workspaces[wi].monitor;
                self.workspaces[wi].monitor = at;
                if self.monitors[from].shown == number {
                    // Its own locked workspace is there at least.
                    let lowest = self.workspaces.iter().find(|w| w.monitor == from);
                    self.monitors[from].shown = lowest.map_or(self.monitors[from].number, |w| w.number);
                }
            }
            None => {
                self.make_workspace(number, at);
            }
        }
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty()
            && let Some(wi) = self.index_of_workspace(number)
        {
            self.workspaces[wi].slots[0].insert_tab(pending);
        }
        self.settle();
        true
    }

    /// Its workspaces move intact (tabs, layout, focused slot) to the focused monitor, or the first one when the
    /// focused one is the one removed, hidden there: that monitor keeps showing what it shows, and nothing else
    /// moves. With no monitor left their windows go to `pending`, workspace by workspace in number order.
    pub fn remove_monitor(&mut self, name: &str) {
        let Some(i) = self.index_of(name) else {
            return;
        };
        self.monitors.remove(i);
        if self.focused_monitor > i {
            self.focused_monitor -= 1;
        } else if self.focused_monitor == i {
            self.focused_monitor = 0;
        }
        if self.monitors.is_empty() {
            let workspaces = std::mem::take(&mut self.workspaces);
            let windows = workspaces.into_iter().flat_map(|w| w.slots).flat_map(|s| s.tabs);
            self.pending.extend(windows.flatten());
        } else {
            for w in &mut self.workspaces {
                if w.monitor == i {
                    w.monitor = self.focused_monitor;
                } else if w.monitor > i {
                    w.monitor -= 1;
                }
            }
        }
        self.settle();
    }

    /// Shows workspace `n` on the monitor it lives on and focuses that monitor; one that does not exist is made on
    /// the focused monitor from its default layout. The workspace that monitor showed goes if it is not locked and
    /// holds no window. Nothing outside 1..=WORKSPACES or without a monitor; `Remote` for a number owned elsewhere.
    // No #[must_use]: alone it is never `Remote`, and the outcome is only news to a session.
    pub fn workspace(&mut self, n: u8) -> Outcome {
        if !(1..=WORKSPACES).contains(&n) {
            return Outcome::Nothing;
        }
        if self.foreign.contains(&n) {
            return Outcome::Remote(n);
        }
        if self.monitors.is_empty() {
            return Outcome::Nothing;
        }
        let m = match self.index_of_workspace(n) {
            Some(wi) => self.workspaces[wi].monitor,
            None => {
                self.make_workspace(n, self.focused_monitor);
                self.focused_monitor
            }
        };
        self.monitors[m].shown = n;
        self.focused_monitor = m;
        self.settle();
        Outcome::Done
    }

    /// The focused slot's current tab, its whole stack, becomes a tab of workspace `n` (made hidden on the focused
    /// monitor if it does not exist) right after the current one of that workspace's focused slot. Focus stays,
    /// and the workspace left is shown, so it stays too. Nothing from an empty slot or no monitor, into the same
    /// workspace, or outside 1..=WORKSPACES; `Remote` for a number owned elsewhere (windows never cross machines).
    pub fn move_to_workspace(&mut self, n: u8) -> Outcome {
        if !(1..=WORKSPACES).contains(&n) {
            return Outcome::Nothing;
        }
        if self.foreign.contains(&n) {
            return Outcome::Remote(n);
        }
        let Some(from) = self.focused_index() else {
            return Outcome::Nothing;
        };
        let ws = &self.workspaces[from];
        if ws.number == n || ws.slots[ws.focus].is_empty() {
            return Outcome::Nothing;
        }
        let to = match self.index_of_workspace(n) {
            Some(to) => to,
            None => self.make_workspace(n, self.focused_monitor),
        };
        // Making one may have shifted the source.
        let Some(from) = self.focused_index() else {
            return Outcome::Nothing;
        };
        let ws = &mut self.workspaces[from];
        let slot = &mut ws.slots[ws.focus];
        let tab = slot.remove_tab(slot.current);
        let ws = &mut self.workspaces[to];
        ws.slots[ws.focus].insert_tab(tab);
        self.settle();
        Outcome::Done
    }

    /// The numbers another machine owns, from now on: `workspace` and `move_to_workspace` leave them alone and new
    /// screens skip them. Those out of range or existing here are left out (here wins; a join renames first).
    pub fn set_foreign(&mut self, numbers: impl IntoIterator<Item = u8>) {
        self.foreign = (numbers.into_iter())
            .filter(|n| (1..=WORKSPACES).contains(n) && self.index_of_workspace(*n).is_none())
            .collect();
    }

    /// No number is owned elsewhere: alone again (the link dropped).
    pub fn clear_foreign(&mut self) {
        self.foreign.clear();
    }

    /// What the session says, applied in the one safe order: the old foreign numbers go, then `renames` (from a
    /// join or a report) are made with `renumber_workspace`, then `foreign` is set. Returns the renames that failed
    /// (a workspace made here meanwhile on the target, or the source gone): the caller reports again, and the
    /// session heals the collision then.
    pub fn apply_session(&mut self, renames: &[(u8, u8)], foreign: impl IntoIterator<Item = u8>) -> Vec<(u8, u8)> {
        self.clear_foreign();
        let failed = (renames.iter().copied())
            .filter(|&(from, to)| !self.renumber_workspace(from, to))
            .collect();
        self.set_foreign(foreign);
        failed
    }

    #[must_use]
    pub fn foreign(&self) -> &BTreeSet<u8> {
        &self.foreign
    }

    /// Workspace `from` takes number `to`, intact; a screen numbered `from` takes `to` too, so it keeps locking it
    /// (monitors stay in number order, focus stays on its screen). How a join's collisions are applied. `false`
    /// (and nothing done) for an unknown `from`, or `to` out of range, existing here or owned elsewhere.
    pub fn renumber_workspace(&mut self, from: u8, to: u8) -> bool {
        let Some(wi) = self.index_of_workspace(from) else {
            return false;
        };
        if !(1..=WORKSPACES).contains(&to) || self.index_of_workspace(to).is_some() || self.foreign.contains(&to) {
            return false;
        }
        self.workspaces[wi].number = to;
        self.workspaces.sort_by_key(|w| w.number);
        for m in &mut self.monitors {
            if m.number == from {
                m.number = to;
            }
            if m.shown == from {
                m.shown = to;
            }
        }
        // Back in number order: `at[old]` is each monitor's new index.
        let mut order: Vec<usize> = (0..self.monitors.len()).collect();
        order.sort_by_key(|&i| self.monitors[i].number);
        let mut at = vec![0; order.len()];
        for (new, &old) in order.iter().enumerate() {
            at[old] = new;
        }
        self.monitors.sort_by_key(|m| m.number);
        for w in &mut self.workspaces {
            w.monitor = at[w.monitor];
        }
        if let Some(&f) = at.get(self.focused_monitor) {
            self.focused_monitor = f;
        }
        self.settle();
        true
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

    /// A new tab right after the current one in `slot` of workspace `workspace` (that workspace's focused slot
    /// without one, the focused workspace without either), made current and its workspace's focused slot; the
    /// monitor is focused too when that workspace is shown, else it stays hidden. A target that is not there means
    /// the focused slot; `pending` with no monitor.
    pub fn open_tab(&mut self, w: WinId, workspace: Option<u8>, slot: Option<usize>) {
        if self.has(w) {
            return;
        }
        let Some((wi, s)) = self.target(workspace, slot) else {
            self.pending.push(w);
            return;
        };
        if self.is_shown(wi) {
            self.focused_monitor = self.workspaces[wi].monitor;
        }
        let ws = &mut self.workspaces[wi];
        ws.focus = s;
        ws.slots[s].insert_tab(vec![w]);
        self.settle();
    }

    /// On top of `parent`'s stack, wherever it is; the current tab and focus stay. `false` (and nothing done)
    /// when the parent is not placed or `w` already is somewhere, so the caller can fall back.
    pub fn open_on(&mut self, w: WinId, parent: WinId) -> bool {
        if self.has(w) {
            return false;
        }
        let Some((wi, s, t, _)) = self.find(parent) else {
            return false;
        };
        self.workspaces[wi].slots[s].tabs[t].push(w);
        self.settle();
        true
    }

    /// Removed wherever it is; a tab left empty goes (see `Slot::remove_tab`). Focus stays on that slot.
    pub fn close(&mut self, w: WinId) {
        if let Some((wi, s, t, p)) = self.find(w) {
            self.workspaces[wi].slots[s].remove(t, p);
        }
        self.pending.retain(|&x| x != w);
        self.settle();
    }

    /// Via `Layout::neighbour`; a slot at the edge has no neighbour past it.
    pub fn focus_slot(&mut self, d: Dir) {
        if let Some((wi, to)) = self.neighbour(d) {
            self.workspaces[wi].focus = to;
        }
        self.settle();
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
        let Some((wi, to)) = self.neighbour(d) else {
            return;
        };
        let ws = &mut self.workspaces[wi];
        let from = &mut ws.slots[ws.focus];
        if from.is_empty() {
            return;
        }
        let tab = from.remove_tab(from.current);
        ws.slots[to].insert_tab(tab);
        ws.focus = to;
        self.settle();
    }

    /// Swaps the focused slot with the neighbour, every tab and the current one; focus follows.
    pub fn swap_slot(&mut self, d: Dir) {
        if let Some((wi, to)) = self.neighbour(d) {
            let ws = &mut self.workspaces[wi];
            ws.slots.swap(ws.focus, to);
            ws.focus = to;
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

    /// Lays out the workspace `monitor` shows (see `set_workspace_layout`); the others keep theirs. An unknown
    /// monitor does nothing.
    pub fn set_layout(&mut self, monitor: &str, l: Layout) {
        if let Some(m) = self.index_of(monitor) {
            self.set_workspace_layout(self.monitors[m].shown, l);
        }
    }

    /// Lays out workspace `n`, shown or not. Slots keep their order and fill the new layout's in sequence; with
    /// fewer slots the extras' tabs are appended, in order, to the last slot, which keeps its current tab; with
    /// more, the new slots start empty. Focus is clamped. So a round trip through a smaller layout is not an
    /// identity: merged tabs stay merged. An unknown workspace or a layout with no slots does nothing.
    pub fn set_workspace_layout(&mut self, n: u8, l: Layout) {
        let count = l.slots.len();
        let Some(wi) = self.index_of_workspace(n) else {
            return;
        };
        if count == 0 {
            return;
        }
        debug_assert!(l.validate().is_ok(), "set_layout needs a valid layout: {l:?}");
        let ws = &mut self.workspaces[wi];
        let extras: Vec<Vec<WinId>> = if ws.slots.len() > count {
            ws.slots.drain(count..).flat_map(|s| s.tabs).collect()
        } else {
            Vec::new()
        };
        ws.slots.resize_with(count, Slot::default);
        ws.slots[count - 1].tabs.extend(extras);
        ws.focus = ws.focus.min(count - 1);
        ws.layout = l;
        self.settle();
    }

    /// What workspaces made on `monitor` from now on start with; those that exist keep theirs. An unknown monitor
    /// or a layout with no slots does nothing.
    pub fn set_default_layout(&mut self, monitor: &str, l: Layout) {
        if l.slots.is_empty() {
            return;
        }
        debug_assert!(l.validate().is_ok(), "set_default_layout needs a valid layout: {l:?}");
        if let Some(m) = self.index_of(monitor) {
            self.monitors[m].default_layout = l;
        }
    }

    /// Focuses the slot (and monitor) holding `w` when `w` is what it shows, the top of its current tab in a shown
    /// workspace, and says whether it did. Nothing is ever raised or shown: a window beneath or hidden stays so.
    pub fn focus_window(&mut self, w: WinId) -> bool {
        let Some((wi, s, _, _)) = self.find(w) else {
            return false;
        };
        if !self.is_shown(wi) || self.workspaces[wi].slots[s].top() != Some(w) {
            return false;
        }
        self.focused_monitor = self.workspaces[wi].monitor;
        self.workspaces[wi].focus = s;
        self.settle();
        true
    }

    /// `w` leaves wherever it is (a tab it leaves empty goes, as on a close) for a new tab right after the current
    /// one in `slot` of workspace `workspace` (the focused slot if that is not there), shown, that workspace shown
    /// on its monitor, and that slot focused: where an activation with a launch's token sends a window. Already a
    /// tab of its own there: that tab is shown, nothing moves. An unknown or pending `w` does nothing.
    pub fn move_window_to_tab(&mut self, w: WinId, workspace: u8, slot: usize) {
        let Some((wi, s, t, p)) = self.find(w) else {
            return;
        };
        // w is placed, so there is a monitor and a target.
        let Some((ti, ts)) = self.target(Some(workspace), Some(slot)) else {
            return;
        };
        let from = &mut self.workspaces[wi].slots[s];
        if (wi, s) == (ti, ts) && from.tabs[t].len() == 1 {
            from.current = t;
        } else {
            // Workspaces only go in `settle`, so the indices hold.
            from.remove(t, p);
            self.workspaces[ti].slots[ts].insert_tab(vec![w]);
        }
        let ws = &mut self.workspaces[ti];
        ws.focus = ts;
        let (m, n) = (ws.monitor, ws.number);
        self.monitors[m].shown = n;
        self.focused_monitor = m;
        self.settle();
    }

    /// Marks `w` as asking for attention. Any placed window but the focused one can be: that one is already seen,
    /// and one not placed has no slot to show it. A window already marked keeps its first mark (and what clears it).
    pub fn mark(&mut self, w: WinId) {
        let Some(p) = self.placements().find(|p| p.win == w) else {
            return;
        };
        if !p.focused {
            let seen = p.shown && p.on_top;
            (self.attention)
                .entry(w)
                .or_insert(if seen { Mark::Unfocused } else { Mark::Waiting });
        }
    }

    /// Drops `w`'s mark, if it has one. Only tests take marks away by hand; `settle` drops the seen ones.
    #[cfg(test)]
    pub fn clear_mark(&mut self, w: WinId) {
        self.attention.remove(&w);
    }

    #[must_use]
    pub fn is_marked(&self, w: WinId) -> bool {
        self.attention.contains_key(&w)
    }

    /// One per non-empty slot of a shown workspace: the top of its current tab at the slot's rectangle.
    #[must_use]
    pub fn tiles(&self) -> Vec<Tile> {
        self.placements()
            .filter(|p| p.shown && p.on_top)
            .filter_map(|p| {
                let ws = &self.workspaces[self.index_of_workspace(p.workspace)?];
                let slot = &ws.slots[p.slot];
                Some(Tile {
                    monitor: p.monitor,
                    slot: p.slot,
                    rect: p.rect,
                    win: p.win,
                    focused: p.focused,
                    tabs: slot.tabs.len(),
                    depth: slot.tabs[p.tab].len(),
                    windows: slot.len(),
                })
            })
            .collect()
    }

    /// The focused slot's top.
    #[must_use]
    pub fn focused_window(&self) -> Option<WinId> {
        let ws = &self.workspaces[self.focused_index()?];
        ws.slots.get(ws.focus)?.top()
    }

    /// The focused slot's rectangle: where `open_stacked` would put a window. A toplevel that has not mapped yet
    /// is configured to it.
    #[must_use]
    pub fn focused_rect(&self) -> Option<Rect> {
        let ws = &self.workspaces[self.focused_index()?];
        let area = self.monitors[ws.monitor].area;
        ws.layout.rects(area).get(ws.focus).copied()
    }

    /// (workspace number, slot, tab, position from the bottom) of `w`; None for unknown or pending windows.
    #[must_use]
    pub fn where_is(&self, w: WinId) -> Option<(u8, usize, usize, usize)> {
        let (wi, s, t, p) = self.find(w)?;
        Some((self.workspaces[wi].number, s, t, p))
    }

    /// Drops what is no longer needed: workspaces neither locked nor shown that hold no window, and the marks `Mark`
    /// says are seen (a waiting window now shown, whatever showed it; one marked while shown now focused) or whose
    /// windows closed or went to `pending`, for good. Every method that changes a slot's tabs, current tab, focus or
    /// a shown workspace ends with this (their early returns change nothing); the random walk checks both rules
    /// after every operation, so a method that forgets it fails there.
    fn settle(&mut self) {
        let monitors = &self.monitors;
        let locked = |n: u8| monitors.iter().any(|m| m.number == n);
        self.workspaces
            .retain(|w| locked(w.number) || monitors[w.monitor].shown == w.number || !w.is_empty());
        if self.attention.is_empty() {
            return;
        }
        let placed: BTreeMap<WinId, Placement> = self.placements().map(|p| (p.win, p)).collect();
        self.attention.retain(|w, mark| {
            placed.get(w).is_some_and(|p| match mark {
                Mark::Waiting => !(p.shown && p.on_top),
                Mark::Unfocused => !p.focused,
            })
        });
    }

    /// A fresh workspace `n` on monitor `m` from its default layout, in number order; its index.
    fn make_workspace(&mut self, n: u8, m: usize) -> usize {
        let at = self.workspaces.partition_point(|w| w.number < n);
        let ws = Workspace::new(n, m, self.monitors[m].default_layout.clone());
        self.workspaces.insert(at, ws);
        at
    }

    /// (workspace index, slot, tab, position from the bottom) of `w`.
    fn find(&self, w: WinId) -> Option<(usize, usize, usize, usize)> {
        self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
            ws.slots.iter().enumerate().find_map(|(si, slot)| {
                slot.tabs
                    .iter()
                    .enumerate()
                    .find_map(|(ti, stack)| stack.iter().position(|&x| x == w).map(|p| (wi, si, ti, p)))
            })
        })
    }

    /// Placed or pending: every open refuses such an id, so a window can never be in two places.
    fn has(&self, w: WinId) -> bool {
        self.find(w).is_some() || self.pending.contains(&w)
    }

    /// (workspace index, slot) for `slot` of workspace `workspace`, each defaulting to the focused one; the focused
    /// slot when that is not there; None with no monitor.
    fn target(&self, workspace: Option<u8>, slot: Option<usize>) -> Option<(usize, usize)> {
        let wi = match workspace {
            Some(n) => self.index_of_workspace(n),
            None => self.focused_index(),
        };
        let target = wi.and_then(|wi| {
            let ws = &self.workspaces[wi];
            let s = slot.unwrap_or(ws.focus);
            (s < ws.slots.len()).then_some((wi, s))
        });
        target.or_else(|| {
            let wi = self.focused_index()?;
            Some((wi, self.workspaces[wi].focus))
        })
    }

    fn index_of(&self, monitor: &str) -> Option<usize> {
        self.monitors.iter().position(|m| m.name == monitor)
    }

    fn index_of_workspace(&self, n: u8) -> Option<usize> {
        self.workspaces.iter().position(|w| w.number == n)
    }

    /// The index of the workspace monitor `m` shows.
    fn shown_index(&self, m: usize) -> Option<usize> {
        self.index_of_workspace(self.monitors.get(m)?.shown)
    }

    /// The index of the focused monitor's shown workspace.
    fn focused_index(&self) -> Option<usize> {
        self.shown_index(self.focused_monitor)
    }

    fn is_shown(&self, wi: usize) -> bool {
        let ws = &self.workspaces[wi];
        self.monitors[ws.monitor].shown == ws.number
    }

    fn focused_slot_mut(&mut self) -> Option<&mut Slot> {
        let wi = self.focused_index()?;
        let ws = &mut self.workspaces[wi];
        ws.slots.get_mut(ws.focus)
    }

    /// The focused workspace (an index) and the slot next to its focused one in `d`.
    fn neighbour(&self, d: Dir) -> Option<(usize, usize)> {
        let wi = self.focused_index()?;
        let ws = &self.workspaces[wi];
        let to = ws.layout.neighbour(ws.focus, d)?;
        (to < ws.slots.len()).then_some((wi, to))
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

    /// The workspace monitor `m` shows.
    fn shown(s: &Stacks, m: usize) -> &Workspace {
        s.shown(m).expect("a monitor shows a workspace")
    }

    /// A slot's tabs in monitor `m`'s shown workspace, bottom to top each.
    fn tabs(s: &Stacks, m: usize, slot: usize) -> Vec<Vec<WinId>> {
        shown(s, m).slots[slot].tabs.clone()
    }

    fn current(s: &Stacks, m: usize, slot: usize) -> usize {
        shown(s, m).slots[slot].current
    }

    /// Each id exactly once; screen numbers unique, in range and in order; workspace numbers too; workspace k
    /// exists on the screen numbered k for every screen; each monitor shows one workspace on it; no workspace is
    /// unlocked, hidden and empty; every workspace has one slot per layout slot and its focus in range; no tab is
    /// empty and every slot's current tab is in range (0 when it has none); tiles and placements agree with the
    /// slots, with what is shown and with focus; numbers owned elsewhere are in range and neither a workspace's nor
    /// a screen's here.
    fn assert_invariants(s: &Stacks) {
        let n = s.monitors.len();
        let in_order = |k: &[u8]| k.windows(2).all(|p| p[0] < p[1]) && k.iter().all(|k| (1..=WORKSPACES).contains(k));
        let screens = numbers(s);
        assert!(in_order(&screens), "screen numbers {screens:?}");
        let numbers: Vec<u8> = s.workspaces.iter().map(|w| w.number).collect();
        assert!(in_order(&numbers), "workspace numbers {numbers:?}");
        for (m, mon) in s.monitors.iter().enumerate() {
            let locked = s.workspaces.iter().find(|w| w.number == mon.number);
            assert_eq!(locked.map(|w| w.monitor), Some(m), "{}: its locked workspace", mon.name);
            let shown = s.workspaces.iter().find(|w| w.number == mon.shown);
            assert_eq!(
                shown.map(|w| w.monitor),
                Some(m),
                "{}: shows {} not on it",
                mon.name,
                mon.shown
            );
        }
        let mut seen = BTreeSet::new();
        for w in &s.workspaces {
            let name = format!("workspace {}", w.number);
            assert!(w.monitor < n, "{name}: monitor {} out of range", w.monitor);
            let shown = s.monitors[w.monitor].shown == w.number;
            assert!(
                screens.contains(&w.number) || shown || !w.is_empty(),
                "{name}: unlocked, hidden and empty"
            );
            assert!(!w.slots.is_empty(), "{name}: no slots");
            assert_eq!(w.slots.len(), w.layout.slots.len(), "{name}: slots vs layout");
            assert!(w.focus < w.slots.len(), "{name}: focus {} out of range", w.focus);
            for (i, slot) in w.slots.iter().enumerate() {
                assert!(slot.tabs.iter().all(|t| !t.is_empty()), "{name} slot {i}: an empty tab");
                if slot.tabs.is_empty() {
                    assert_eq!(slot.current, 0, "{name} slot {i}: current without tabs");
                } else {
                    assert!(slot.current < slot.tabs.len(), "{name} slot {i}: current out of range");
                }
                assert_eq!(slot.len(), slot.tabs.iter().map(Vec::len).sum::<usize>());
                assert_eq!(slot.is_empty(), slot.tabs.is_empty());
                assert_eq!(slot.top(), slot.tabs.get(slot.current).and_then(|t| t.last().copied()));
            }
            for win in w.slots.iter().flat_map(|sl| sl.tabs.iter().flatten()) {
                assert!(seen.insert(*win), "window {win} twice");
            }
        }
        for w in &s.pending {
            assert!(seen.insert(*w), "window {w} twice (pending)");
        }
        if s.monitors.is_empty() {
            assert_eq!(s.focused_monitor, 0);
            assert!(s.workspaces.is_empty(), "workspaces without a monitor");
        } else {
            assert!(s.focused_monitor < s.monitors.len(), "focused monitor out of range");
            assert!(s.pending.is_empty(), "pending windows while a monitor exists");
        }

        let non_empty = (s.monitors.iter().enumerate())
            .flat_map(|(m, _)| &shown(s, m).slots)
            .filter(|sl| !sl.is_empty())
            .count();
        let tiles = s.tiles();
        assert_eq!(tiles.len(), non_empty, "one tile per non-empty shown slot");
        for t in &tiles {
            let slot = &shown(s, t.monitor).slots[t.slot];
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
            let (k, sl, tab, pos) = s.where_is(p.win).expect("placed windows are found");
            assert_eq!((k, sl, tab), (p.workspace, p.slot, p.tab));
            let ws = s.workspaces.iter().find(|w| w.number == k).expect("its workspace");
            assert_eq!(p.monitor, ws.monitor);
            assert_eq!(p.shown, s.monitors[ws.monitor].shown == k);
            let slot = &ws.slots[sl];
            assert_eq!(p.on_top, tab == slot.current && pos + 1 == slot.tabs[tab].len());
            assert!(!p.focused || (p.shown && p.on_top), "window {} focused unseen", p.win);
        }
        let tops: Vec<(usize, usize, Rect, WinId, bool)> = placements
            .iter()
            .filter(|p| p.shown && p.on_top)
            .map(|p| (p.monitor, p.slot, p.rect, p.win, p.focused))
            .collect();
        let from_tiles: Vec<_> = tiles
            .iter()
            .map(|t| (t.monitor, t.slot, t.rect, t.win, t.focused))
            .collect();
        assert_eq!(tops, from_tiles, "tiles are the shown top placements");

        for (&w, &mark) in &s.attention {
            let p = placements.iter().find(|p| p.win == w);
            let unseen = match mark {
                Mark::Waiting => p.is_some_and(|p| !(p.shown && p.on_top)),
                Mark::Unfocused => p.is_some_and(|p| !p.focused),
            };
            assert!(unseen, "window {w} marked {mark:?} but seen or not placed");
        }

        for k in &s.foreign {
            assert!((1..=WORKSPACES).contains(k), "foreign {k} out of range");
            assert!(!numbers.contains(k) && !screens.contains(k), "foreign {k} exists here");
        }
    }

    fn ids(s: &Stacks) -> BTreeSet<WinId> {
        s.workspaces
            .iter()
            .flat_map(|w| w.slots.iter().flat_map(|sl| sl.tabs.iter().flatten()))
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
        assert_eq!(s.where_is(1), Some((1, 0, 0, 0)));
        assert_eq!(s.where_is(3), Some((1, 0, 0, 2)));
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
        s.open_tab(2, Some(2), Some(1));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 1, 1), [vec![2]]);
        assert_eq!((s.focused_monitor, shown(&s, 1).focus), (1, 1));
        assert_eq!(s.focused_window(), Some(2));
        // A monitor alone: its focused slot.
        s.focus_at("winit", 3);
        s.open_tab(3, Some(2), None);
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
        s.open_tab(1, Some(7), Some(1));
        s.open_tab(2, Some(2), Some(9));
        s.open_tab(3, None, Some(9));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 3), [vec![1], vec![2], vec![3]]);
        assert_eq!((s.focused_monitor, shown(&s, 0).focus), (0, 3));
        // No monitor at all: pending, as any open.
        let mut none = Stacks::new();
        none.open_tab(1, Some(7), Some(0));
        assert_eq!(none.pending, [1]);
        assert_invariants(&none);
    }

    #[test]
    fn open_on_puts_a_window_on_its_parents_stack_without_raising_it() {
        let mut s = three_tabs();
        s.focus_slot(Dir::Right);
        s.open_stacked(9);
        let before = (s.focused_window(), shown(&s, 0).focus);
        // The parent's tab is in the background: the window waits there.
        assert!(s.open_on(5, 1));
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1, 5], vec![2, 3], vec![4]]);
        assert_eq!(current(&s, 0, 0), 1);
        assert_eq!((s.focused_window(), shown(&s, 0).focus), before);
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
            workspace: 1,
            monitor: 0,
            slot,
            tab,
            rect,
            shown: true,
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
        assert_eq!(shown(&s, 0).focus, 3);
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
        assert_eq!(shown(&s, 0).focus, 3);
        s.focus_slot(Dir::Down);
        assert_eq!(shown(&s, 0).focus, 4);
        s.focus_slot(Dir::Left);
        assert_eq!(shown(&s, 0).focus, 2);
        s.focus_slot(Dir::Left);
        assert_eq!(shown(&s, 0).focus, 2);
        s.focus_slot(Dir::Down);
        assert_eq!(shown(&s, 0).focus, 2);
        assert_invariants(&s);
    }

    #[test]
    fn focus_at_and_focus_monitor_take_names() {
        let mut s = wall_monitor();
        s.add_monitor("second", UHD, layout("halves"));
        s.focus_at("second", 1);
        assert_invariants(&s);
        assert_eq!((s.focused_monitor, shown(&s, 1).focus), (1, 1));
        // An empty slot can take focus: a new window then opens there.
        s.open_stacked(7);
        assert_eq!(tabs(&s, 1, 1), [vec![7]]);
        s.focus_monitor("winit");
        assert_invariants(&s);
        assert_eq!((s.focused_monitor, shown(&s, 0).focus), (0, 0));
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
        assert_eq!(shown(&s, 0).focus, 3);
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
        assert_eq!(shown(&s, 0).focus, 0);
        s.open_stacked(1);
        s.move_stack(Dir::Left);
        s.move_stack(Dir::Up);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1]]);
        assert_eq!(shown(&s, 0).focus, 0);
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
        assert_eq!(shown(&s, 0).focus, 3);
        assert_eq!(s.focused_window(), Some(3));
        // At the edge: nothing.
        s.swap_slot(Dir::Up);
        assert_invariants(&s);
        assert_eq!(shown(&s, 0).focus, 3);
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
        assert_eq!(shown(&s, 0).layout.name, "halves");
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
        assert_eq!(shown(&s, 0).focus, 1);
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
        assert_eq!(shown(&s, 0).layout.name, "wall");
        assert_eq!(tabs(&s, 0, 0), [vec![1, 2]]);
        assert_eq!(tabs(&s, 0, 1).concat(), [11, 12, 21, 22, 31, 32, 41, 42, 51, 52]);
        assert!((2..6).all(|i| tabs(&s, 0, i).is_empty()));
        assert_eq!(shown(&s, 0).focus, 1);
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
        assert_eq!((s.focused_monitor, shown(&s, 0).focus), (0, 0));
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
    fn move_window_to_tab_takes_it_out_and_shows_it_as_a_new_tab_there() {
        // Tabs [1], [2, 3], [4] in slot 0 (the middle one current); slot 3 holds [5], [6] (6 current).
        let mut s = three_tabs();
        s.focus_slot(Dir::Right);
        s.open_stacked(5);
        s.open_tab(6, None, None);
        s.focus_at("winit", 1);
        // Beneath the top of a stack: it leaves the stack.
        s.mark(2);
        s.move_window_to_tab(2, 1, 3);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![3], vec![4]]);
        assert_eq!(tabs(&s, 0, 3), [vec![5], vec![6], vec![2]]);
        assert_eq!(current(&s, 0, 3), 2);
        assert_eq!((s.focused_monitor, shown(&s, 0).focus), (0, 3));
        assert_eq!(s.focused_window(), Some(2));
        assert!(!s.is_marked(2), "shown");
        // The last window of its tab: the tab goes, as on a close.
        s.move_window_to_tab(4, 1, 5);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![3]]);
        assert_eq!(current(&s, 0, 0), 1);
        assert_eq!(tabs(&s, 0, 5), [vec![4]]);
        assert_eq!((shown(&s, 0).focus, s.focused_window()), (5, Some(4)));
    }

    #[test]
    fn move_window_to_tab_shows_a_window_already_alone_in_a_tab_there_where_it_is() {
        // Tabs [1], [2, 3], [4], the middle one current.
        let mut s = three_tabs();
        s.focus_at("winit", 2);
        s.move_window_to_tab(4, 1, 0);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3], vec![4]], "nothing moves");
        assert_eq!(current(&s, 0, 0), 2);
        assert_eq!((shown(&s, 0).focus, s.focused_window()), (0, Some(4)));
        // Already shown: only focus changes.
        s.focus_at("winit", 2);
        s.move_window_to_tab(4, 1, 0);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![2, 3], vec![4]]);
        assert_eq!((shown(&s, 0).focus, s.focused_window()), (0, Some(4)));
        // In a stack of the target slot, not alone: it leaves the stack for a tab of its own after the current.
        s.cycle_tab(Cycle::Prev);
        s.move_window_to_tab(2, 1, 0);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 0, 0), [vec![1], vec![3], vec![2], vec![4]]);
        assert_eq!(current(&s, 0, 0), 2);
    }

    #[test]
    fn move_window_to_tab_with_the_target_gone_uses_the_focused_slot() {
        let mut s = three_tabs();
        s.add_monitor("second", UHD, layout("halves"));
        s.focus_at("second", 1);
        s.move_window_to_tab(1, 7, 0);
        s.move_window_to_tab(4, 1, 9);
        assert_invariants(&s);
        assert_eq!(tabs(&s, 1, 1), [vec![1], vec![4]]);
        assert_eq!(current(&s, 1, 1), 1);
        assert_eq!(tabs(&s, 0, 0), [vec![2, 3]]);
        assert_eq!(s.focused_window(), Some(4));
        // Unknown or pending windows: nothing.
        let before = s.clone();
        s.move_window_to_tab(99, 1, 0);
        assert_eq!(s, before);
        let mut none = Stacks::new();
        none.open_stacked(1);
        let before = none.clone();
        none.move_window_to_tab(1, 1, 0);
        assert_eq!(none, before);
    }

    #[test]
    fn removing_the_focused_screen_moves_its_workspaces_intact_to_the_first_hidden() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.add_monitor("second", SIDE, layout("halves"));
        s.focus_monitor("second");
        s.open_stacked(2);
        s.open_tab(3, None, None);
        s.focus_slot(Dir::Right);
        s.open_stacked(4);
        let moved = shown(&s, 1).clone();
        s.remove_monitor("second");
        assert_invariants(&s);
        assert_eq!(s.monitors.len(), 1);
        assert_eq!(s.focused_monitor, 0);
        assert_eq!(workspaces(&s), [(1, 0), (2, 0)]);
        assert_eq!(shown_numbers(&s), [1], "it keeps showing what it shows");
        let w = &s.workspaces[1];
        assert_eq!(
            (&w.layout, &w.slots, w.focus),
            (&moved.layout, &moved.slots, moved.focus),
            "intact"
        );
        assert_eq!(s.focused_window(), Some(1));
        assert!(!placement(&s, 4).shown);
        s.workspace(2);
        assert_eq!(s.focused_window(), Some(4));
        assert_invariants(&s);
        // Unknown names do nothing.
        let before = s.clone();
        s.remove_monitor("nope");
        assert_eq!(s, before);
    }

    #[test]
    fn removing_another_screen_moves_its_workspaces_to_the_focused_one() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.add_monitor("b", UHD, single());
        s.add_monitor("c", UHD, layout("halves"));
        s.focus_at("c", 1);
        s.open_stacked(2);
        s.remove_monitor("winit");
        assert_invariants(&s);
        assert_eq!(numbers(&s), [2, 3]);
        assert_eq!(workspaces(&s), [(1, 1), (2, 0), (3, 1)]);
        assert_eq!(shown_numbers(&s), [2, 3], "nothing else moves");
        assert_eq!(ws_tabs(&s, 1, 0), [vec![1]]);
        assert_eq!(s.focused_window(), Some(2));
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
        assert_eq!(m.default_layout().name, "wall");
        assert_eq!(m.shown(), 1);
        assert_eq!(s.workspaces().len(), 1);
        assert_eq!(s.shown(1), None, "no such monitor");
        let w = s.shown(0).expect("monitor 0 shows one");
        assert_eq!((w.number(), w.monitor()), (1, 0));
        assert_eq!(w.layout().name, "wall");
        assert_eq!(w.focus(), 3);
        assert_eq!(w.slots()[0].tabs(), [vec![1]]);
        let hero = &w.slots()[3];
        assert_eq!(hero.tabs(), [vec![2, 3], vec![4]]);
        assert_eq!((hero.current(), hero.top(), hero.len()), (1, Some(4), 3));
        assert!(!hero.is_empty());
        assert!(w.slots()[1].is_empty());
        assert_eq!(w.slots()[1].top(), None);
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
    fn the_focused_window_and_unplaced_ones_cannot_be_marked() {
        let mut s = three_tabs();
        s.mark(3);
        assert!(!s.is_marked(3), "the focused window is already seen");
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
    fn a_shown_window_without_focus_keeps_its_mark_until_it_is_focused() {
        // Tabs [1], [2, 3], [4] in slot 0, showing 3; slot 3 holds 5 and has focus.
        let mut s = three_tabs();
        s.focus_slot(Dir::Right);
        s.open_stacked(5);
        s.mark(5);
        assert!(!s.is_marked(5), "the focused window is seen");
        s.mark(3);
        assert!(s.is_marked(3), "shown, but in a slot without focus");
        assert_invariants(&s);
        // Covered and shown again, never focused: still marked.
        assert!(s.open_on(6, 3));
        assert!(s.is_marked(3));
        s.close(6);
        assert!(s.is_marked(3));
        assert_invariants(&s);
        // Its slot takes focus: gone for good.
        s.focus_slot(Dir::Left);
        assert!(!s.is_marked(3));
        s.focus_slot(Dir::Right);
        assert!(!s.is_marked(3), "it does not come back");
        // Marked again while covered: the first mark stands, so showing it again (unfocused) keeps it.
        s.focus_slot(Dir::Right);
        s.mark(3);
        assert!(s.open_on(6, 3));
        s.mark(3);
        s.close(6);
        assert!(s.is_marked(3), "still the mark of a shown window");
        assert_invariants(&s);
        // Any way of focusing it clears it.
        s.mark(3);
        assert!(s.focus_window(3) && !s.is_marked(3));
        s.focus_at("winit", 3);
        s.mark(3);
        s.focus_at("winit", 0);
        assert!(!s.is_marked(3));
        s.add_monitor("second", UHD, single());
        s.focus_monitor("second");
        s.mark(3);
        s.focus_monitor("winit");
        assert!(!s.is_marked(3));
        assert_invariants(&s);
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
        assert_eq!(shown(&s, 0).slots[0].top(), Some(4));
        assert!(s.is_marked(1));
        s.focus_at("winit", 0);
        s.move_stack(Dir::Right);
        assert_eq!(shown(&s, 0).slots[0].top(), Some(1));
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
    fn layout_merges_clear_what_they_show_and_monitor_loss_only_pending() {
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

        // A screen lost hides its workspaces: what was marked stays marked.
        s.add_monitor("other", SIDE, layout("wall"));
        s.focus_monitor("other");
        s.remove_monitor("winit");
        assert!(s.is_marked(2) && !placement(&s, 2).shown);
        assert_invariants(&s);

        // With no monitor left the windows wait in `pending`, which holds no marks.
        let mut s = three_tabs();
        s.mark(2);
        s.remove_monitor("winit");
        assert!(!s.is_marked(2));
        assert_invariants(&s);
    }

    /// A second screen, right of `UHD`.
    const SIDE: Rect = Rect {
        x: 3840,
        y: 0,
        w: 1920,
        h: 1080,
    };

    /// Every existing workspace as (number, monitor).
    fn workspaces(s: &Stacks) -> Vec<(u8, usize)> {
        s.workspaces.iter().map(|w| (w.number, w.monitor)).collect()
    }

    /// Each monitor's number.
    fn numbers(s: &Stacks) -> Vec<u8> {
        s.monitors.iter().map(|m| m.number).collect()
    }

    /// What each monitor shows.
    fn shown_numbers(s: &Stacks) -> Vec<u8> {
        s.monitors.iter().map(|m| m.shown).collect()
    }

    /// Workspace `n`'s slot's tabs.
    fn ws_tabs(s: &Stacks, n: u8, slot: usize) -> Vec<Vec<WinId>> {
        let ws = s.workspaces.iter().find(|w| w.number == n).expect("workspace exists");
        ws.slots[slot].tabs.clone()
    }

    fn placement(s: &Stacks, w: WinId) -> Placement {
        s.placements().find(|p| p.win == w).expect("placed")
    }

    #[test]
    fn monitors_lock_workspaces_1_to_n_and_show_them() {
        let mut s = wall_monitor();
        assert_eq!(workspaces(&s), [(1, 0)]);
        assert_eq!(shown_numbers(&s), [1]);
        s.add_monitor("second", SIDE, layout("halves"));
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1)]);
        assert_eq!(shown_numbers(&s), [1, 2]);
        assert_eq!(
            s.workspaces[1].layout.name, "halves",
            "from the monitor's default layout"
        );
        assert_eq!(s.focused_monitor, 0, "a new monitor takes no focus");
    }

    #[test]
    fn workspace_creates_a_missing_one_on_the_focused_monitor_and_shows_it() {
        let mut s = wall_monitor();
        s.add_monitor("second", SIDE, layout("halves"));
        s.focus_monitor("second");
        s.workspace(5);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1), (5, 1)]);
        assert_eq!(shown_numbers(&s), [1, 5]);
        assert_eq!(s.workspaces[2].layout.name, "halves");
        s.open_stacked(1);
        assert_eq!(ws_tabs(&s, 5, 0), [vec![1]]);
        // Leaving it: it holds a window, so it stays, hidden, and so does its window.
        s.workspace(2);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1), (5, 1)]);
        assert_eq!(shown_numbers(&s), [1, 2]);
        let p = placement(&s, 1);
        assert_eq!(
            (p.workspace, p.monitor, p.shown, p.on_top, p.focused),
            (5, 1, false, true, false)
        );
        assert!(s.tiles().is_empty(), "nothing hidden is drawn");
        assert_eq!(s.focused_window(), None);
    }

    #[test]
    fn workspace_shows_an_existing_one_on_its_monitor_and_focuses_that_monitor() {
        let mut s = wall_monitor();
        s.add_monitor("second", SIDE, layout("halves"));
        s.workspace(3);
        s.open_stacked(1);
        s.workspace(1);
        s.focus_monitor("second");
        s.workspace(3);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1), (3, 0)], "it stays where it was made");
        assert_eq!(shown_numbers(&s), [3, 2]);
        assert_eq!(s.focused_monitor, 0);
        assert_eq!(s.focused_window(), Some(1));
        // Back to 2: monitor 0 keeps showing 3.
        s.workspace(2);
        assert_invariants(&s);
        assert_eq!(shown_numbers(&s), [3, 2]);
        assert_eq!(s.focused_monitor, 1);
    }

    #[test]
    fn workspace_out_of_range_shown_or_without_a_monitor_does_nothing() {
        let mut s = three_tabs();
        let before = s.clone();
        s.workspace(0);
        s.workspace(11);
        s.workspace(1);
        assert_eq!(s, before);
        let mut none = Stacks::new();
        none.open_stacked(1);
        let before = none.clone();
        none.workspace(2);
        assert_eq!(none, before);
        assert_invariants(&none);
    }

    #[test]
    fn an_unlocked_hidden_empty_workspace_goes_away() {
        let mut s = wall_monitor();
        s.workspace(3);
        s.workspace(1);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0)], "left empty");
        s.workspace(4);
        s.open_stacked(1);
        s.workspace(1);
        assert_eq!(workspaces(&s), [(1, 0), (4, 0)], "it holds a window");
        s.close(1);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0)], "its last window closed while hidden");
        // A locked one stays, hidden and empty; an empty one shown stays too.
        s.workspace(6);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (6, 0)]);
    }

    #[test]
    fn a_workspace_starts_from_its_monitors_default_layout_and_set_layout_changes_only_the_shown_one() {
        let mut s = wall_monitor();
        s.set_layout("winit", layout("halves"));
        s.workspace(2);
        assert_eq!(s.workspaces[1].layout.name, "wall", "the default, not what 1 shows");
        s.set_layout("winit", single());
        assert_invariants(&s);
        assert_eq!(s.workspaces[0].layout.name, "halves");
        assert_eq!(s.workspaces[1].layout.name, "single");
    }

    #[test]
    fn a_new_monitor_takes_its_workspace_from_another_and_shows_it() {
        let mut s = wall_monitor();
        s.workspace(2);
        s.open_stacked(1);
        s.add_monitor("second", SIDE, layout("halves"));
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1)]);
        assert_eq!(shown_numbers(&s), [1, 2], "monitor 0 shows its lowest remaining");
        assert_eq!(s.workspaces[1].layout.name, "wall", "it keeps its own layout");
        let p = placement(&s, 1);
        assert_eq!((p.monitor, p.shown, p.rect), (1, true, rect(3840, 0, 640, 360)));
        // Hidden where it was: shown on the new monitor all the same.
        s.focus_monitor("second");
        s.workspace(3);
        s.open_stacked(2);
        s.workspace(2);
        s.add_monitor("third", UHD, single());
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1), (3, 2)]);
        assert_eq!(shown_numbers(&s), [1, 2, 3]);
        assert!(placement(&s, 2).shown);
    }

    #[test]
    fn screens_keep_their_numbers_and_the_lowest_free_one_is_reused() {
        // winit (1): 1 shows [1], 4 hides [4]; b (2): 2 shows [2]; c (3): 3 shows [3]. b has focus.
        let mut s = wall_monitor();
        s.add_monitor("b", SIDE, layout("halves"));
        s.add_monitor("c", UHD, single());
        s.workspace(4);
        s.open_stacked(4);
        s.workspace(1);
        s.open_stacked(1);
        s.focus_monitor("c");
        s.open_stacked(3);
        s.focus_monitor("b");
        s.open_stacked(2);
        assert_eq!(s.next_screen_number(), Some(4));
        s.remove_monitor("winit");
        assert_invariants(&s);
        // b keeps 2 and c keeps 3; winit's 1 and 4 go to b, hidden and intact.
        assert_eq!(numbers(&s), [2, 3]);
        assert_eq!(s.next_screen_number(), Some(1));
        assert_eq!(workspaces(&s), [(1, 0), (2, 0), (3, 1), (4, 0)]);
        assert_eq!(shown_numbers(&s), [2, 3]);
        assert_eq!((ws_tabs(&s, 1, 0), ws_tabs(&s, 4, 0)), (vec![vec![1]], vec![vec![4]]));
        assert_eq!(s.workspaces[0].layout.name, "wall");
        assert_eq!((s.focused_monitor, s.focused_window()), (0, Some(2)));
        // A new screen takes the lowest free number and its workspace, shown; b keeps showing 2, focus stays.
        s.add_monitor("d", UHD, single());
        assert_invariants(&s);
        assert_eq!(numbers(&s), [1, 2, 3], "monitors in number order");
        assert_eq!(s.monitors[0].name, "d");
        assert_eq!(workspaces(&s), [(1, 0), (2, 1), (3, 2), (4, 1)]);
        assert_eq!(shown_numbers(&s), [1, 2, 3]);
        assert_eq!(s.workspaces[0].layout.name, "wall", "it keeps its own layout");
        assert_eq!((s.focused_monitor, s.focused_window()), (1, Some(2)));
        // The screen a workspace is taken from while showing it shows its lowest remaining one.
        s.remove_monitor("d");
        s.workspace(1);
        s.focus_monitor("c");
        s.add_monitor("e", UHD, single());
        assert_invariants(&s);
        assert_eq!(numbers(&s), [1, 2, 3]);
        assert_eq!(shown_numbers(&s), [1, 2, 3]);
        assert!(placement(&s, 1).shown);
        // The last screen goes: every window waits.
        for name in ["b", "c", "e"] {
            s.remove_monitor(name);
            assert_invariants(&s);
        }
        assert_eq!(s.pending, [1, 2, 3, 4]);
        assert!(s.workspaces.is_empty());
    }

    #[test]
    fn set_workspace_layout_reaches_a_hidden_one_and_set_default_layout_the_next_made() {
        let mut s = wall_monitor();
        s.workspace(2);
        s.open_stacked(1);
        s.workspace(1);
        s.set_workspace_layout(2, layout("halves"));
        assert_invariants(&s);
        assert_eq!(s.workspaces[1].layout.name, "halves");
        assert_eq!(s.workspaces[0].layout.name, "wall");
        assert_eq!(shown_numbers(&s), [1]);
        let mut empty = single();
        empty.slots.clear();
        let before = s.clone();
        s.set_workspace_layout(3, single());
        s.set_workspace_layout(2, empty.clone());
        s.set_default_layout("nope", single());
        s.set_default_layout("winit", empty);
        assert_eq!(s, before);
        s.set_default_layout("winit", single());
        assert_eq!(s.monitors[0].default_layout.name, "single");
        assert_eq!(s.workspaces[0].layout.name, "wall", "what exists keeps its layout");
        s.workspace(5);
        assert_eq!(shown(&s, 0).layout.name, "single");
        assert_invariants(&s);
    }

    #[test]
    fn move_to_workspace_carries_the_focused_stack_and_focus_stays() {
        // Tabs [1], [2, 3], [4] in slot 0, the middle one current.
        let mut s = three_tabs();
        s.move_to_workspace(3);
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (3, 0)]);
        assert_eq!(shown_numbers(&s), [1], "made hidden, not shown");
        assert_eq!(ws_tabs(&s, 3, 0), [vec![2, 3]]);
        assert_eq!(ws_tabs(&s, 1, 0), [vec![1], vec![4]]);
        assert_eq!(s.focused_window(), Some(4), "focus stays on the slot");
        // Into an existing workspace's focused slot, after its current tab; the emptied source is shown, so stays.
        s.workspace(3);
        s.move_to_workspace(1);
        assert_invariants(&s);
        assert_eq!(ws_tabs(&s, 1, 0), [vec![1], vec![4], vec![2, 3]]);
        assert_eq!(s.workspaces[0].slots[0].current, 2);
        assert_eq!(workspaces(&s), [(1, 0), (3, 0)]);
        assert_eq!(shown_numbers(&s), [3]);
        s.workspace(1);
        assert_eq!(workspaces(&s), [(1, 0)], "then left empty");
        assert_invariants(&s);
    }

    #[test]
    fn move_to_workspace_shown_on_another_monitor_shows_it_there() {
        let mut s = wall_monitor();
        s.add_monitor("second", SIDE, layout("halves"));
        s.focus_at("second", 1);
        s.focus_monitor("winit");
        s.open_stacked(1);
        s.move_to_workspace(2);
        assert_invariants(&s);
        assert_eq!(ws_tabs(&s, 2, 1), [vec![1]]);
        assert!(placement(&s, 1).shown);
        assert_eq!((s.focused_monitor, s.focused_window()), (0, None));
    }

    #[test]
    fn move_to_workspace_from_an_empty_slot_the_same_workspace_or_out_of_range_does_nothing() {
        let mut s = wall_monitor();
        s.move_to_workspace(2);
        assert_eq!(s, wall_monitor(), "nothing to carry, nothing made");
        let mut s = three_tabs();
        let before = s.clone();
        s.move_to_workspace(1);
        s.move_to_workspace(0);
        s.move_to_workspace(11);
        assert_eq!(s, before);
        let mut none = Stacks::new();
        none.open_stacked(1);
        none.move_to_workspace(2);
        assert_eq!(none.pending, [1]);
        assert_invariants(&none);
    }

    #[test]
    fn placements_cover_hidden_workspaces() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.workspace(2);
        s.open_stacked(2);
        let cell = rect(0, 0, 1280, 720);
        let p = |win, workspace, shown, focused| Placement {
            win,
            workspace,
            monitor: 0,
            slot: 0,
            tab: 0,
            rect: cell,
            shown,
            on_top: true,
            focused,
        };
        assert_eq!(
            s.placements().collect::<Vec<_>>(),
            [p(1, 1, false, false), p(2, 2, true, true)]
        );
        assert_eq!(s.tiles().iter().map(|t| t.win).collect::<Vec<_>>(), [2]);
        assert_eq!(s.where_is(1), Some((1, 0, 0, 0)));
        assert_eq!(s.where_is(2), Some((2, 0, 0, 0)));
        assert_invariants(&s);
    }

    #[test]
    fn a_window_in_a_hidden_workspace_is_not_seen() {
        // 1 alone in workspace 1's slot 0; 5 in its slot 3, focused.
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.focus_slot(Dir::Right);
        s.open_stacked(5);
        s.workspace(2);
        s.open_stacked(2);
        assert!(!s.focus_window(1), "hidden: nothing to focus");
        s.mark(1);
        s.mark(5);
        s.cycle_tab(Cycle::Next);
        assert!(s.is_marked(1) && s.is_marked(5), "on top, but hidden: not seen");
        assert_invariants(&s);
        // Shown: 1 comes on top, its mark goes; 5 is on top but not focused, so it was marked as waiting too.
        s.workspace(1);
        assert!(!s.is_marked(1) && !s.is_marked(5));
        // Marked while shown without focus, then hidden and shown again: still marked until focused.
        s.mark(1);
        s.workspace(2);
        s.workspace(1);
        assert!(s.is_marked(1));
        s.focus_at("winit", 0);
        assert!(!s.is_marked(1));
        // Beneath in a hidden workspace: showing the workspace is not enough.
        s.open_stacked(3);
        s.focus_slot(Dir::Right);
        s.workspace(2);
        s.mark(1);
        s.workspace(1);
        assert!(s.is_marked(1));
        assert_invariants(&s);
    }

    #[test]
    fn open_tab_into_a_hidden_workspace_leaves_it_hidden() {
        let mut s = wall_monitor();
        s.workspace(2);
        s.open_stacked(1);
        s.workspace(1);
        s.open_tab(2, Some(2), Some(3));
        assert_invariants(&s);
        assert_eq!(ws_tabs(&s, 2, 3), [vec![2]]);
        assert_eq!(s.workspaces[1].focus, 3, "its slot is focused in its workspace");
        assert_eq!(shown_numbers(&s), [1]);
        assert_eq!(s.focused_window(), None);
        // Gone: the focused slot.
        s.open_tab(3, Some(7), Some(0));
        assert_eq!(ws_tabs(&s, 1, 0), [vec![3]]);
        assert_invariants(&s);
    }

    #[test]
    fn move_window_to_tab_shows_the_target_workspace() {
        let mut s = wall_monitor();
        s.open_stacked(1);
        s.workspace(2);
        s.open_stacked(2);
        s.move_window_to_tab(2, 1, 3);
        assert_invariants(&s);
        assert_eq!(ws_tabs(&s, 1, 3), [vec![2]]);
        assert_eq!(shown_numbers(&s), [1]);
        assert_eq!(workspaces(&s), [(1, 0)], "the one left behind went, empty");
        assert_eq!(s.focused_window(), Some(2));
    }

    #[test]
    fn monitors_past_the_last_workspace_are_ignored() {
        let mut s = Stacks::new();
        assert_eq!(s.next_screen_number(), Some(1));
        for i in 0..12 {
            s.add_monitor(&format!("m{i}"), UHD, single());
        }
        assert_invariants(&s);
        assert_eq!(s.next_screen_number(), None);
        assert_eq!(s.monitors.len(), usize::from(WORKSPACES));
        assert_eq!(s.workspaces.len(), usize::from(WORKSPACES));
    }

    #[test]
    fn workspace_and_move_to_workspace_say_what_they_did() {
        let mut s = three_tabs();
        assert_eq!(s.workspace(0), Outcome::Nothing);
        assert_eq!(s.workspace(11), Outcome::Nothing);
        assert_eq!(s.workspace(1), Outcome::Done, "already shown: shown all the same");
        assert_eq!(s.move_to_workspace(1), Outcome::Nothing, "the same workspace");
        assert_eq!(s.move_to_workspace(11), Outcome::Nothing);
        assert_eq!(s.move_to_workspace(3), Outcome::Done);
        assert_eq!(s.workspace(3), Outcome::Done);
        s.focus_slot(Dir::Right);
        assert_eq!(s.move_to_workspace(1), Outcome::Nothing, "an empty slot");
        let mut none = Stacks::new();
        assert_eq!(none.workspace(2), Outcome::Nothing);
        assert_eq!(none.move_to_workspace(2), Outcome::Nothing);
        assert_invariants(&s);
    }

    #[test]
    fn numbers_owned_elsewhere_are_never_made_here() {
        let mut s = three_tabs();
        s.set_foreign([2, 3, 0, 11]);
        assert_eq!(s.foreign(), &BTreeSet::from([2, 3]), "only numbers in range");
        assert_invariants(&s);
        let before = s.clone();
        assert_eq!(s.workspace(2), Outcome::Remote(2));
        assert_eq!(s.move_to_workspace(3), Outcome::Remote(3));
        assert_eq!(s, before, "nothing done here");
        // A screen added meanwhile skips them.
        assert_eq!(s.next_screen_number(), Some(4));
        s.add_monitor("second", SIDE, layout("halves"));
        assert_eq!(numbers(&s), [1, 4]);
        // Free numbers work as before.
        assert_eq!(s.move_to_workspace(5), Outcome::Done);
        assert_eq!(ws_tabs(&s, 5, 0), [vec![2, 3]]);
        assert_invariants(&s);
        // A number that exists here is never foreign: the local workspace wins.
        s.set_foreign([1, 5, 6]);
        assert_eq!(s.foreign(), &BTreeSet::from([6]));
        // Without a monitor a foreign number is still elsewhere.
        let mut none = Stacks::new();
        none.set_foreign([4]);
        assert_eq!(none.workspace(4), Outcome::Remote(4));
        // Cleared: alone again, exactly as before.
        s.clear_foreign();
        assert!(s.foreign().is_empty());
        assert_eq!(s.workspace(6), Outcome::Done);
        assert_eq!(shown_numbers(&s), [6, 4], "made on the focused screen");
        assert_invariants(&s);
    }

    #[test]
    fn add_monitor_numbered_takes_the_number_given() {
        let mut s = wall_monitor();
        s.workspace(3);
        s.open_stacked(1);
        s.workspace(1);
        assert!(s.add_monitor_numbered("third", SIDE, layout("halves"), 3));
        assert_invariants(&s);
        assert_eq!(numbers(&s), [1, 3]);
        assert_eq!(workspaces(&s), [(1, 0), (3, 1)], "its workspace moves to it, intact");
        assert_eq!(shown_numbers(&s), [1, 3]);
        assert_eq!(s.next_screen_number(), Some(2));
        assert!(s.add_monitor_numbered("second", SIDE, single(), 2));
        assert_eq!(numbers(&s), [1, 2, 3], "monitors in number order");
        assert_eq!(s.monitors[1].name, "second");
        assert_eq!(s.focused_monitor, 0);
        assert_invariants(&s);
        // Refused, nothing done: a number taken, owned elsewhere or out of range, a name taken, no slots.
        s.set_foreign([5]);
        let before = s.clone();
        let mut empty = single();
        empty.slots.clear();
        assert!(!s.add_monitor_numbered("x", SIDE, single(), 2));
        assert!(!s.add_monitor_numbered("x", SIDE, single(), 5));
        assert!(!s.add_monitor_numbered("x", SIDE, single(), 0));
        assert!(!s.add_monitor_numbered("x", SIDE, single(), 11));
        assert!(!s.add_monitor_numbered("winit", SIDE, single(), 4));
        assert!(!s.add_monitor_numbered("x", SIDE, empty, 4));
        assert_eq!(s, before);
    }

    #[test]
    fn renumber_workspace_moves_a_workspace_and_its_screen() {
        // winit (1) shows 1 and hides 5 holding [1]; second (2) shows 2 holding [2], focused.
        let mut s = wall_monitor();
        s.add_monitor("second", SIDE, layout("halves"));
        s.workspace(5);
        s.open_stacked(1);
        s.workspace(1);
        s.focus_monitor("second");
        s.open_stacked(2);
        assert!(s.renumber_workspace(5, 7));
        assert_invariants(&s);
        assert_eq!(workspaces(&s), [(1, 0), (2, 1), (7, 0)]);
        assert_eq!(ws_tabs(&s, 7, 0), [vec![1]]);
        // A screen's own workspace: the screen takes the number too, and the monitors stay in number order.
        assert!(s.renumber_workspace(1, 9));
        assert_invariants(&s);
        assert_eq!(numbers(&s), [2, 9]);
        assert_eq!(s.monitors[1].name, "winit");
        assert_eq!(workspaces(&s), [(2, 0), (7, 1), (9, 1)]);
        assert_eq!(shown_numbers(&s), [2, 9]);
        assert_eq!(
            (s.focused_monitor, s.focused_window()),
            (0, Some(2)),
            "focus stays on its screen"
        );
        assert_eq!(s.next_screen_number(), Some(1));
        // A shown workspace keeps being shown under its new number.
        s.focus_monitor("winit");
        s.workspace(7);
        assert!(s.renumber_workspace(7, 4));
        assert_eq!(shown_numbers(&s), [2, 4]);
        assert_eq!(s.focused_window(), Some(1));
        assert_invariants(&s);
        // Refused, nothing done: an unknown one, a number taken, owned elsewhere or out of range, the same number.
        s.set_foreign([6]);
        let before = s.clone();
        assert!(!s.renumber_workspace(3, 8));
        assert!(!s.renumber_workspace(4, 2));
        assert!(!s.renumber_workspace(4, 6));
        assert!(!s.renumber_workspace(4, 0));
        assert!(!s.renumber_workspace(4, 11));
        assert!(!s.renumber_workspace(4, 4));
        assert_eq!(s, before);
    }

    #[test]
    fn apply_session_renames_then_sets_what_is_owned_elsewhere() {
        // Screen 1 shows 1; 3 hidden holds [1]. Stale foreign numbers from before.
        let mut s = wall_monitor();
        s.workspace(3);
        s.open_stacked(1);
        s.workspace(1);
        s.set_foreign([4, 5]);
        // 4 was foreign: the old set goes first, so renaming onto it works.
        assert_eq!(s.apply_session(&[(1, 4), (3, 5)], [1, 2, 3]), []);
        assert_invariants(&s);
        assert_eq!(numbers(&s), [4]);
        assert_eq!(workspaces(&s), [(4, 0), (5, 0)]);
        assert_eq!(s.foreign(), &BTreeSet::from([1, 2, 3]));
        // A rename onto a number made here meanwhile, or from one gone: returned, nothing renamed.
        s.set_foreign([]);
        s.workspace(6);
        s.open_stacked(2);
        assert_eq!(s.apply_session(&[(5, 6), (8, 9)], [5, 7]), [(5, 6), (8, 9)]);
        assert_eq!(workspaces(&s), [(4, 0), (5, 0), (6, 0)]);
        assert_eq!(s.foreign(), &BTreeSet::from([7]), "here wins");
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

    /// Every slot's tabs, by workspace (empty ones left out, as they come and go): what only focus, current-tab and
    /// workspace switches must keep.
    fn all_tabs(s: &Stacks) -> Vec<(u8, Vec<Vec<Vec<WinId>>>)> {
        (s.workspaces.iter())
            .filter(|w| !w.is_empty())
            .map(|w| (w.number, w.slots.iter().map(|sl| sl.tabs.clone()).collect()))
            .collect()
    }

    /// Each workspace holding a window, and the name of the monitor it lives on.
    fn homes(s: &Stacks) -> BTreeMap<u8, String> {
        (s.workspaces.iter())
            .filter(|w| !w.is_empty())
            .map(|w| (w.number, s.monitors[w.monitor].name.clone()))
            .collect()
    }

    /// What one walk visited, to check it went everywhere.
    #[derive(Default)]
    struct Visits {
        /// The most windows open at once.
        most: usize,
        /// Steps ending with a mark, with a window in a hidden workspace, with numbers owned elsewhere.
        marked: usize,
        hidden: usize,
        foreign: usize,
        /// `Remote` outcomes, successful renumbers.
        remote: usize,
        renumbered: usize,
    }

    /// One seeded walk of 1,000 operations.
    fn walk(seed: u64) -> Visits {
        let layouts = [layout("wall"), layout("halves"), single()];
        let mut rng = Rng(seed);
        let mut s = wall_monitor();
        let mut live: BTreeSet<WinId> = BTreeSet::new();
        let mut next_id: WinId = 1;
        let mut next_monitor = 0;
        let mut v = Visits::default();
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
            // A workspace number: mostly in range, sometimes 0 or past the last.
            let number = |rng: &mut Rng| rng.below(u64::from(WORKSPACES) + 2) as u8;
            let before = s.clone();
            // Close more once many are open, so the walk keeps visiting small and empty stacks.
            let op = if live.len() > 12 && rng.below(2) == 0 {
                3
            } else {
                rng.below(28)
            };
            match op {
                0 | 1 => {
                    s.open_stacked(next_id);
                    live.insert(next_id);
                    next_id += 1;
                }
                2 => {
                    let n = number(&mut rng);
                    s.open_tab(next_id, Some(n), Some(rng.below(7) as usize));
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
                    let m = format!("m{next_monitor}");
                    s.add_monitor(&m, UHD, l);
                    // Only the new screen's own workspace moves, intact.
                    let k = s.monitors.iter().find(|x| x.name == m).map(|x| x.number);
                    if !before.monitors.is_empty() {
                        assert_eq!(all_tabs(&s), all_tabs(&before), "seed {seed} step {step}: add_monitor");
                        let (now, then) = (homes(&s), homes(&before));
                        assert!(
                            then.iter().all(|(w, home)| Some(*w) == k || now[w] == *home),
                            "seed {seed} step {step}: add_monitor moved another's workspace"
                        );
                        // Only the screen workspace k left may show something else; focus stays on its screen.
                        let left = (before.workspaces.iter())
                            .find(|w| Some(w.number) == k)
                            .map(|w| before.monitors[w.monitor].name.clone());
                        let kept = |x: &Stacks| -> Vec<(String, u8)> {
                            (x.monitors.iter())
                                .filter(|mon| mon.name != m && Some(&mon.name) != left.as_ref())
                                .map(|mon| (mon.name.clone(), mon.shown))
                                .collect()
                        };
                        assert_eq!(
                            kept(&s),
                            kept(&before),
                            "seed {seed} step {step}: add_monitor changed a view"
                        );
                        assert_eq!(
                            s.monitors[s.focused_monitor].name, before.monitors[before.focused_monitor].name,
                            "seed {seed} step {step}: add_monitor moved focus"
                        );
                    }
                }
                11 => {
                    let m = name(&mut rng, &s);
                    s.remove_monitor(&m);
                    // The others keep showing what they showed.
                    let mut views = shown_numbers(&before);
                    if let Some(i) = before.index_of(&m) {
                        views.remove(i);
                    }
                    assert_eq!(
                        shown_numbers(&s),
                        views,
                        "seed {seed} step {step}: remove_monitor changed a view"
                    );
                    // Its workspaces go intact to the focused screen; nothing else moves.
                    if let Some(focused) = s.monitors.get(s.focused_monitor) {
                        assert_eq!(
                            all_tabs(&s),
                            all_tabs(&before),
                            "seed {seed} step {step}: remove_monitor"
                        );
                        let (now, then) = (homes(&s), homes(&before));
                        assert!(
                            then.iter()
                                .all(|(w, home)| &now[w] == if *home == m { &focused.name } else { home }),
                            "seed {seed} step {step}: remove_monitor moved the wrong workspaces"
                        );
                    }
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
                19 => {
                    let n = number(&mut rng);
                    s.move_window_to_tab(pick(&mut rng, &live), n, rng.below(7) as usize);
                }
                20 | 21 => {
                    let n = number(&mut rng);
                    let outcome = s.workspace(n);
                    v.remote += usize::from(matches!(outcome, Outcome::Remote(_)));
                    if before.foreign.contains(&n) {
                        assert_eq!(outcome, Outcome::Remote(n), "seed {seed} step {step}: workspace {n}");
                        assert_eq!(s, before, "seed {seed} step {step}: remote workspace {n} acted here");
                    }
                    assert_eq!(
                        all_tabs(&s),
                        all_tabs(&before),
                        "seed {seed} step {step}: workspace {n}"
                    );
                    if let Some(w) = s.workspaces.iter().find(|w| w.number == n) {
                        assert_eq!(
                            (s.focused_monitor, s.monitors[w.monitor].shown),
                            (w.monitor, n),
                            "seed {seed} step {step}: workspace {n} shown and focused"
                        );
                    }
                }
                22 => {
                    let n = number(&mut rng);
                    let outcome = s.move_to_workspace(n);
                    v.remote += usize::from(matches!(outcome, Outcome::Remote(_)));
                    if before.foreign.contains(&n) {
                        assert_eq!(outcome, Outcome::Remote(n), "seed {seed} step {step}: move {n}");
                        assert_eq!(s, before, "seed {seed} step {step}: remote move {n} acted here");
                    }
                    assert_eq!(
                        (s.focused_monitor, shown_numbers(&s)),
                        (before.focused_monitor, shown_numbers(&before)),
                        "seed {seed} step {step}: move_to_workspace {n} keeps what is shown and focused"
                    );
                }
                17 | 18 => {
                    let w = pick(&mut rng, &live);
                    s.mark(w);
                    let unseen = before.placements().any(|p| p.win == w && !p.focused);
                    assert_eq!(s.is_marked(w), unseen, "seed {seed} step {step}: mark {w}");
                }
                23 => s.clear_mark(pick(&mut rng, &live)),
                24 => {
                    let mask = rng.below(1 << WORKSPACES);
                    s.set_foreign((1..=WORKSPACES).filter(|k| mask & (1 << (k - 1)) != 0));
                    let mut only = s.clone();
                    only.foreign.clone_from(&before.foreign);
                    assert_eq!(only, before, "seed {seed} step {step}: set_foreign changed more");
                }
                25 => s.clear_foreign(),
                26 => {
                    // Mostly onto a free number, so renumbers happen; sometimes anything, so refusals do.
                    let from = number(&mut rng);
                    let free: Vec<u8> = (1..=WORKSPACES)
                        .filter(|k| s.index_of_workspace(*k).is_none() && !s.foreign.contains(k))
                        .collect();
                    let to = match free.len() as u64 {
                        n if n > 0 && rng.below(4) > 0 => free[rng.below(n) as usize],
                        _ => number(&mut rng),
                    };
                    let fits = before.index_of_workspace(from).is_some()
                        && (1..=WORKSPACES).contains(&to)
                        && before.index_of_workspace(to).is_none()
                        && !before.foreign.contains(&to);
                    assert_eq!(
                        s.renumber_workspace(from, to),
                        fits,
                        "seed {seed} step {step}: renumber {from} to {to}"
                    );
                    v.renumbered += usize::from(fits);
                    if fits {
                        // Everything as it was under the new number: tabs, homes, views, focus.
                        let renamed = |k: u8| if k == from { to } else { k };
                        let mut tabs: Vec<_> = all_tabs(&before).into_iter().map(|(k, t)| (renamed(k), t)).collect();
                        tabs.sort();
                        assert_eq!(all_tabs(&s), tabs, "seed {seed} step {step}: renumber");
                        let homes_then: BTreeMap<u8, String> =
                            homes(&before).into_iter().map(|(k, m)| (renamed(k), m)).collect();
                        assert_eq!(homes(&s), homes_then, "seed {seed} step {step}: renumber moved one");
                        let views = |x: &Stacks, f: &dyn Fn(u8) -> u8| -> BTreeMap<String, (u8, u8)> {
                            (x.monitors.iter())
                                .map(|m| (m.name.clone(), (f(m.number), f(m.shown))))
                                .collect()
                        };
                        assert_eq!(views(&s, &|k| k), views(&before, &renamed));
                        assert_eq!(
                            s.monitors[s.focused_monitor].name, before.monitors[before.focused_monitor].name,
                            "seed {seed} step {step}: renumber moved focus"
                        );
                    } else {
                        assert_eq!(s, before, "seed {seed} step {step}: refused renumber changed things");
                    }
                }
                _ => {
                    let n = number(&mut rng);
                    next_monitor += 1;
                    let l = layouts[rng.below(3) as usize].clone();
                    let fits = (1..=WORKSPACES).contains(&n)
                        && before.monitors.iter().all(|m| m.number != n)
                        && !before.foreign.contains(&n);
                    assert_eq!(
                        s.add_monitor_numbered(&format!("m{next_monitor}"), UHD, l, n),
                        fits,
                        "seed {seed} step {step}: add_monitor_numbered {n}"
                    );
                    if !fits {
                        assert_eq!(s, before, "seed {seed} step {step}: refused add_monitor_numbered");
                    }
                }
            }
            if !matches!(op, 17 | 18) {
                assert!(
                    s.attention.keys().all(|w| before.attention.contains_key(w)),
                    "seed {seed} step {step}: only mark marks"
                );
            }
            assert_invariants(&s);
            assert_eq!(ids(&s), live, "seed {seed} step {step}");
            v.most = v.most.max(live.len());
            v.marked += usize::from(!s.attention.is_empty());
            v.hidden += usize::from(s.placements().any(|p| !p.shown));
            v.foreign += usize::from(!s.foreign.is_empty());
        }
        v
    }

    #[test]
    fn random_walks_lose_nothing() {
        let mut seed: u64 = 0x51c3_9a07_d2e4;
        let (mut remote, mut renumbered) = (0, 0);
        for _ in 0..32 {
            let v = walk(seed);
            assert!(v.marked > 50, "seed {seed}: marks held on {} steps only", v.marked);
            assert!(v.hidden > 50, "seed {seed}: windows hidden on {} steps only", v.hidden);
            assert!(
                v.foreign > 50,
                "seed {seed}: numbers owned elsewhere on {} steps only",
                v.foreign
            );
            assert!(
                (4..=40).contains(&v.most),
                "seed {seed}: at most {} open, the walk is unbalanced",
                v.most
            );
            remote += v.remote;
            renumbered += v.renumbered;
            seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1) | 1;
        }
        assert!(remote > 100, "{remote} remote outcomes in all");
        assert!(renumbered > 40, "{renumbered} renumbers in all");
    }
}
