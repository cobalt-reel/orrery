# Workspaces and screens Implementation Plan

**Goal:** 10 i3-style workspaces and several screens on one machine (spec:
`docs/plans/2026-10-27-workspaces-and-screens-design.md`; read it first, and section 1 of
`docs/plans/2026-10-26-orrery-design.md`).

**Architecture:** the pure core `src/stacks.rs` gains workspaces between monitors and slots: a monitor shows one
workspace; a workspace owns the layout, slots and focused slot that `Monitor` owns today. Every existing operation
keeps acting on the focused monitor's shown workspace. The compositor maps only shown workspaces. The nested
backend becomes a multi-window one of orrery's own (Smithay's winit backend drives a single window); headless gains
extra screens for tests.

**Tech stack:** Rust 2024, Smithay 0.7 (winit 0.30 re-exported), KDL config, wayland-client in tests.

**Ground rules for every task**
- TDD: write the failing test first, see it fail, then implement.
- Match the surrounding code: terse explanatory doc comments (≤120 columns), same naming, no new deps unless the
  task says so.
- Before committing: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` (the FULL
  suite, every integration binary). Report the test count.
- orrery stays clean: no references to any other project, person, machine or personal setup.
- Tests never touch real user paths; use the `tests/support` harness (temp dirs).

---

### Task 1: workspaces in the core (`src/stacks.rs`) and its callers

**Model:** `Workspace { number: u8 (1..=10), monitor: usize, layout, slots: Vec<Slot>, focus: usize }`,
`Monitor { name, area, default_layout: Layout, shown: u8 }`, `Stacks { monitors, workspaces: Vec<Workspace>
(existing ones, sorted by number), focused_monitor, pending, attention, … }`.

- `add_monitor(name, area, default_layout)`: monitor N (1-based) locks workspace N. If workspace N exists on
  another monitor it moves here and is shown (its old monitor shows its lowest remaining workspace, or a fresh
  locked one); else it is created from `default_layout`. Pending windows go to it as one tab (as today).
- `remove_monitor(name)`: its workspaces' tabs go to the focused remaining monitor's shown workspace's focused
  slot (slot by slot, workspace by workspace in number order); monitors after it shift down and locks are
  renumbered (workspace k ≤ N is locked to monitor k: a locked workspace whose monitor index changed moves to its
  new monitor; a monitor that lost its shown workspace shows its locked one). None left → pending, as today.
- `workspace(n)`: exists → show it on its monitor, focus that monitor; else create it on the focused monitor
  from that monitor's `default_layout`, show it. The workspace left behind is dropped if not locked and empty.
- `move_to_workspace(n)`: focused stack → workspace n (created on the focused monitor if needed, not shown) as a
  new tab after its focused slot's current one; focus stays. Same workspace → nothing. The source workspace is
  not dropped (it is shown).
- `set_layout(monitor, l)` acts on that monitor's shown workspace; `set_area` unchanged.
- `placements()` covers every window in every workspace; `Placement` gains `shown: bool` (its workspace is shown)
  and `on_top` stays "top of its slot's current tab"; the compositor maps only `shown && on_top`. Rects come from
  the workspace's monitor's area.
- `where_is` returns the workspace number instead of the monitor (`(workspace, slot, tab, pos)`); update every
  caller. Marks: a window in a hidden workspace counts as not seen (a buried-style mark clears when it is shown
  on top).
- Unit tests for every bullet + extend the random walk (switch, move_to_workspace, add/remove monitor) and the
  invariants (each window once; workspaces 1..N exist and are locked to 1..N; each monitor shows exactly one
  workspace that is on it; no unlocked hidden empty workspace).
- Callers (`comp/`, `ipc.rs`, `render.rs`, `input.rs`, `placement` targets): behaviour-preserving with one
  monitor; `arrange` unmaps windows of hidden workspaces; render draws the shown workspace. Launch targets record
  the workspace number + slot instead of the monitor name (target gone → focused slot). `move_window_to_tab`
  likewise.

Commit `stacks: workspaces between monitors and slots`.

### Task 2: the command surface

- `Request`/`Action`: `workspace <1-10>`, `move-to-workspace <1-10>`, `open-screen` (msg only for now; answered
  with an error "this backend cannot open screens" until Tasks 3/4 implement it per backend).
- Config: binds `Mod+1..0` → workspace, `Mod+Shift+1..0` → move-to-workspace, `Mod+Ctrl+1/2` → layouts in
  `examples/config.kdl`; numbers outside 1..10 refused by config and msg.
- `state`: `monitors[].workspace`, top-level `workspaces: [{number, monitor, shown, layout, focused_slot, slots}]`.
- README: keys table, msg list, state, the model paragraph (screens, workspaces), checklist steps.
- Tests: parsing/refusals, e2e in `tests/control.rs` with one screen (switch to 3, open a window, switch back:
  hidden and unmapped; `move-to-workspace`).

Commit `msg and binds: workspaces`.

### Task 3: several headless screens + `orrery run` joins a running orrery

- Headless: `open-screen` adds an output named `headless-2`, `headless-3`, … (first stays as today), same size as
  `--size`, placed after the previous one in global space; `close-screen <name>` (msg, headless only, for tests)
  removes it.
- `orrery run`: when the control socket answers, send `open-screen`, print the answer, exit 0 (non-zero if it
  refused); otherwise start as today. Unit/e2e test with headless (`orrery run --backend headless` twice on the
  same socket → one process, two monitors).
- e2e: two screens → workspaces 1 and 2 locked; `workspace 3` from screen 2 creates it there; `move-to-workspace
  1` from screen 2 moves a window to screen 1's workspace; closing screen 2 brings its windows to screen 1;
  keyboard focus follows `workspace N` across screens (test client `activated` flags).

Commit `screens: several per orrery, run joins a running one`.

### Task 4: the nested multi-window backend

- Replace `backend_winit.rs`'s use of `smithay::backend::winit::init_from_attributes` with orrery's own backend on
  the winit 0.30 re-export: one `EventLoop` pumped from calloop (as Smithay's `WinitEventLoop` does with
  `pump_app_events`; read `~/.cargo/registry/src/*/smithay-0.7.0/src/backend/winit/{mod,input}.rs` and port what
  is needed), one `EGLDisplay`/`EGLContext`/`GlesRenderer` shared by every window, one `EGLSurface` +
  `OutputDamageTracker` + `Decorations` per window. Keep every rule in today's module doc (swap only on that
  window's `RedrawRequested` with `pre_present_notify`, swap interval 0, frame age after idle frames, the 16 ms
  timer requesting redraws for every window).
- Windows: the first opens at start; `open-screen` opens another (named `winit-2`, …, app id and title as today
  so host rules match); `CloseRequested` on a window removes its screen (`remove_monitor`, destroy the output
  global) and quits only when it was the last; `Resized` per window; input per window: pointer positions are
  window-local, so add the window's output offset in global space; `Focus(false)` per window as today, and a
  `Focus(true)` focuses that window's monitor.
- No automated test can reach a real host: keep pure helpers (naming, offsets, which-window-for-event) unit-tested,
  and add a manual checklist to the README (two windows on two host monitors, resize, close one, Super+N across).

Commit `nested: one window per screen`.

### Task 5: as built

Full suite + clippy + fmt, cleanliness scan (given by the controller), README vs design check, an "As built"
section here. Commit `workspaces and screens: as built`.
