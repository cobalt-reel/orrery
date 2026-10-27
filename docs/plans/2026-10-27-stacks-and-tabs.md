# Stacks and tabs Implementation Plan

**Goal:** slots hold tabs, tabs are stacks (design: `docs/plans/2026-10-27-stacks-and-tabs-design.md`, which is the
spec; read it first, and section 2 of `docs/plans/2026-10-26-orrery-design.md`).

**Architecture:** the pure core `src/stacks.rs` changes from "one `Vec<WinId>` per slot" to "a slot = tabs
(`Vec<Vec<WinId>>`) + current index"; the compositor (`src/comp/`) keeps mapping exactly what `placements()` says.
Placement of new windows becomes a pure decision (`src/placement.rs`, new) fed by facts the compositor gathers
(parent, client pid chain, launches). xdg-activation is added with Smithay's `XdgActivationState`.

**Tech stack:** Rust 2024, Smithay (see `Cargo.toml` for the pinned version), wayland-client in tests, KDL config.

**Ground rules for every task**
- TDD: write the failing test first, see it fail, then implement.
- Match the surrounding code: doc comments in the same terse, explanatory style, same naming, no new deps unless
  the task says so.
- Before committing: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` (the FULL suite,
  every integration binary, every time). Report the test count.
- orrery stays clean: no references to any other project, person, machine or personal setup.
- Tests never touch real user paths (`~/.config/orrery`, the real `$XDG_RUNTIME_DIR` socket); use the
  `tests/support` harness, which already uses temp dirs.
- Commit messages end with the attribution lines the controller gives you.

---

### Task 1: the core model (`src/stacks.rs`) and its callers

**Files:** modify `src/stacks.rs` (model + all unit tests), and just enough of `src/ipc.rs`, `src/comp/render.rs`,
`src/comp/mod.rs`, `src/comp/input.rs` to compile with today's behaviour mapped onto the new model.

**Model:**
```rust
pub struct Slot { tabs: Vec<Vec<WinId>>, current: usize }   // empty slot: tabs empty, current 0
pub struct Monitor { name, area, layout, slots: Vec<Slot>, focus: usize }
```
Accessors: `Monitor::slots() -> &[Slot]`, `Slot::tabs() -> &[Vec<WinId>]`, `Slot::current() -> usize`,
`Slot::top() -> Option<WinId>` (top of the current tab), `Slot::len()` (windows in the slot).
`Placement` gains `tab: usize`; `on_top` = top of the CURRENT tab. `Tile` gains `tabs: usize` and keeps `depth`
(= current stack's length) plus `windows` (total in the slot). `where_is` returns `(monitor, slot, tab, pos)`.
`pending` stays a flat `Vec<WinId>` of windows; the next monitor takes them as ONE tab of its focused slot (in
order, as a stack), as today.

**Operations (spec in the design's tables; write a unit test for each bullet):**
- `open_stacked(w)`: on top of the focused slot's current stack; empty slot → first tab.
- `open_tab(w, monitor: Option<&str>, slot: Option<usize>)`: a new tab right after the current one in that slot
  (unknown/out-of-range target → the focused slot), made current, and that slot (and monitor) focused.
- `open_on(w, parent)`: on top of the parent's stack; current tab and focus unchanged. Unknown parent → `false`
  returned, nothing done (caller falls back).
- Every open refuses an id already present (as today).
- `close(w)`: removes it; an emptied tab goes; current: if the removed tab was current, the tab now at that index,
  else the previous (clamped); if before current, `current -= 1`.
- `cycle_tab(Next|Prev)`: changes `current`, wrapping; one tab → nothing.
- `move_stack(d)` (replaces `move_tab`): the focused slot's current tab → the neighbour as a new tab after its
  current, current there, focus follows. Empty slot or edge → nothing.
- `swap_slot(d)`: swaps whole `Slot`s; focus follows.
- `split()`: the focused stack's top window → new tab right after, current. Stack of one → nothing.
- `set_layout`: extra slots' tabs appended (in order) to the last slot; the last slot's `current` unchanged.
- `remove_monitor`: its tabs (slot by slot) appended as tabs of the focused slot of the new focused monitor; none
  left → their windows go to `pending` flat.
- `activate(w)` is REMOVED in this task's meaning: replace with `focus_window(w)` that focuses the slot holding
  `w` only if `w` is its slot's top (returns bool); nothing is ever raised. (Clicks already go through
  `focus_at`.)
- `focused_window()` = focused slot's top.

**Callers, behaviour-preserving for now:** `comp/mod.rs` opens with `open_stacked` (as today's `open`);
render draws a badge when `windows > 1` and the strip with one segment per TAB (current bright) for now;
`ipc.rs` `move-tab` calls `move_stack` (renamed in Task 2); `StateView` keeps the old shape but lists every
window of the slot (tabs flattened in order) so `tests/` stay green, unless a test asserted cycle order, in which
case update the assertion to the new semantics and say so in the report.

**Steps:** write the new unit tests (replace the old ones that encode rotation), run → fail, implement, run full
suite, commit `stacks: slots hold tabs, tabs are stacks`.

---

### Task 2: the command surface (config, binds, msg, state)

**Files:** `src/config.rs`, `src/ipc.rs`, `src/comp/input.rs`, `src/comp/mod.rs`, `examples/config.kdl`,
`README.md`, `tests/control.rs`, `tests/check.rs` as needed.

- `Action`/`Request`: `MoveTab` → `Move` (`move <dir>` in binds and msg; `move-tab` is an error — no alias), new
  `Split`, `ToggleStacking`, `Request::Spawn { argv: Vec<String> }` (`orrery msg spawn kitty --single-instance`:
  every word after `spawn` is argv; empty argv is an error; it runs through the same `Orrery::spawn`).
- Config: `stacking #true|#false` (top level, default true), colour `attention` in the colours block (default: a
  clearly different colour from focus/badge, e.g. amber `#ffb000`; follow how the other colours parse/default).
- Runtime `stacking: bool` on `Orrery`; `toggle-stacking` flips it; on reload it is reset to the config value only
  if the config's `stacking` differs from the previously loaded config's.
- `examples/config.kdl` (= the built-in config): `Mod+Shift+H/J/K/L` → `move`, `Mod+S` → `split`,
  `Mod+Shift+S` → `toggle-stacking`. `orrery check` must accept it.
- `state`: `slots[].tabs: [{ current: bool, attention: bool, windows: [{id, app_id, title}] }]` (attention
  always false until Task 5) plus top-level `stacking: bool`. Update `tests/` readers of the old shape.
- README: keys table, `orrery msg` list (move, split, toggle-stacking, spawn), the model paragraph at the top
  ("every slot holds tabs; each tab is a stack…").
- Tests: parse/round-trip of each new request, `move-tab` refused, config `stacking`/`attention` parse +
  defaults + errors, reload rule for stacking, `msg split`/`move`/`toggle-stacking` end-to-end through
  `tests/control.rs` with real test-client windows.

Commit `msg and binds: move, split, toggle-stacking, spawn; state shows tabs`.

---

### Task 3: drawing tabs (`src/comp/render.rs`)

- Badge: when the slot holds more than one window; colour `attention` if any of its windows is marked
  (`Stacks` needs `is_marked(w)`/marks — add a pure `attention: HashSet<WinId>`-like set to `Stacks` in this task
  with `mark(w)`, `clear_mark(w)`, and auto-clear when `w` becomes a slot's top via any operation; unit tests).
- Strip (held show-tabs): one segment per tab, current bright, others dim; a segment whose stack has depth `d > 1`
  carries `d` small pips (square, strip-height-derived, left-aligned inside the segment, capped so they fit; drop
  pips that would not fit); segment colour `attention` when its stack holds a marked window.
- Scene unit tests in the style of the existing ones (`of_color`, rect math): pips count, cap, attention colours,
  one tab of depth 3 → one segment with 3 pips, current segment bright.

Commit `render: tab strips with depth pips, attention badge`.

---

### Task 4: where new windows go (launch tracking + parents + stacking)

**Files:** create `src/placement.rs` (pure), modify `src/lib.rs`, `src/comp/spawn.rs`, `src/comp/mod.rs`,
`src/comp/handlers.rs`.

Pure part (`placement.rs`, fully unit-tested, time and pid chains passed in):
```rust
pub struct Launch { pub pid: u32, pub token: Option<String>, pub monitor: String, pub slot: usize, pub at: Instant, pub claimed: bool }
pub struct Launches { … }            // push, expire(now) (drops > 5 s), claim_by_pids(&[u32]) , claim_oldest(now), claim_token(&str)
pub enum Landing { OnParent(WinId), NewTab { monitor: Option<String>, slot: Option<usize> }, Stacked, Tab }
pub fn decide(parent: Option<WinId>, parent_placed: bool, pid_chain: &[u32], launches: &mut Launches, now: Instant, stacking: bool) -> Landing
```
Order: placed parent → `OnParent`; pid chain (client pid then ancestors) matches an unclaimed launch → `NewTab` at
its target; else oldest unclaimed launch < 5 s → `NewTab`; else `Stacked` if stacking else `Tab` (a new tab in the
focused slot). Test each rule, the 5 s edge, each launch claimed once, ordering of two launches.

Compositor part:
- `spawn` records a `Launch` (target = focused monitor/slot at the call) with the child's pid.
- pid chain: client pid from `client.get_credentials(&display_handle)`; ancestors by reading
  `/proc/<pid>/stat` field 4 (ppid) up to pid 1 or 32 steps; a pure parser `ppid_of(stat: &str) -> Option<u32>`
  (handles `)` in comm: split at the LAST `)`), unit-tested.
- Parent: `ToplevelSurface::parent()` at first buffer → `WinId` via `surfaces`; placed only if `where_is` finds it.
- `toplevel_committed` first buffer: run `decide` and call `open_on`/`open_tab`/`open_stacked`/`open_tab(focused)`.
  Waiting windows are still configured to the focused rect (fine: they get a configure on placement).
- e2e (`tests/e2e.rs` or a new `tests/stacks.rs`): window with a placed parent stacks on it even when the focused
  slot is elsewhere; `msg spawn sleep 30` then a test-client window within 5 s → new tab in the slot focused at the
  spawn (focus moved before the window appears); after `toggle-stacking`, a plain window opens as a new tab;
  pid match: `msg spawn <current test exe> --exact <helper test> --nocapture` with an env var
  (`ORRERY_TEST_HELPER=1`, `WAYLAND_DISPLAY` from orrery) where the helper test connects and maps a window, and
  is a no-op without the env var — the window lands as a new tab even after the 5 s window would have let
  another launch take it (spawn a `sleep 30` launch FIRST, then the helper, then assert the helper's window went
  to the helper's target, not the sleep's).

Commit `new windows: dialogs stack on their parent, launches open tabs`.

---

### Task 5: xdg-activation (tokens and attention)

**Files:** `src/comp/handlers.rs`, `src/comp/mod.rs`, `src/comp/spawn.rs`, `src/placement.rs`, `Cargo.toml` only if
a Smithay feature flag is needed, tests.

- Add `XdgActivationState` + `XdgActivationHandler` + `delegate_xdg_activation!`.
- Each launch gets a token created by orrery (`XdgActivationState::create_external_token`), stored in its
  `Launch`, passed to the child as `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID` (spawn's `command` gains an
  optional token argument; unit-test the env).
- `request_activation(token, surface)`:
  - token belongs to an unclaimed launch and the window is placed → `Stacks::move_window_to_tab(w, monitor, slot)`
    (new pure op: take `w` out of wherever it is, new tab after current at the target, current, focused; unit
    tests incl. it being the last window of its old tab), launch claimed;
  - else if `focus_window(w)` succeeds (w is its slot's top) → done;
  - else `mark(w)`.
  Tokens older than 5 s or unknown: treat as a plain request (focus-if-top else mark). Never raise.
- `state` tab `attention` becomes real.
- e2e: the test client gains `xdg_activation_v1` support (get token? not needed — it activates with a given token
  string); window buried under another then activates → stays buried, `attention` true, badge colour not tested
  here; it clears after `cycle-tab`/close brings it to top; top window of an unfocused slot activates → slot
  focused; launch token relocation: `msg spawn sleep 30`, wait > 5 s? NO — instead read the token from `state`?
  Simpler: expose launches in `state` as `launches: [{pid, token, age_ms, claimed}]` (debug aid, tokens are not
  secrets of the user's), take the token from there, create a window in another slot (claimed by fallback? —
  first map the window BEFORE the spawn so the fallback cannot claim it), then activate with the token → it moves
  to the launch's target as a new tab.

Commit `xdg-activation: launch tokens and attention`.

---

### Task 6: as-built notes and full verification

- Run the full suite + clippy + fmt, and the cleanliness scan the controller gives you (no references to other
  projects, people or personal setups).
- Append an "As built" section to this plan (deviations, test count, anything deferred).
- README check against the design.

Commit `stacks and tabs: as built`.

---

## As built

250 tests (193 unit, 57
integration, most of them headless end to end), one more ignored (slow: every prefix of the example config); fmt and
clippy clean.

Deviations from the plan:
- `Landing::NewTab { monitor: String, slot: usize }` and `Launch::token: String`: fields required, not `Option`
  (every launch has a target and a token).
- Launches live until they expire (5 s), outliving their program: a launcher that hands off and exits still gets
  its window placed. Pid matching ends with the launch, not with the process.
- A window mapped again (unmap, then a new buffer) sees no launches: only its parent or `stacking` place it.
- xdg-activation: Smithay's global and token dispatch, but `activate` dispatched by orrery itself instead of the
  `delegate_xdg_activation!` macro. Smithay passes on only tokens in its pool and drops the rest silently; to
  orrery every request counts, if only for a mark.
- Only input-backed client tokens can focus: a serial of orrery's seat no older than the keyboard's latest enter,
  from the client with keyboard focus; 64 kept at most, single use, 5 s. Any other token only marks.
- A mark on a window shown in a slot without focus (`Mark::Unfocused`) as well as one beneath (`Mark::Waiting`);
  each clears on its own rule. The badge shows for a single marked window too (it has nothing else to say so).
- `orrery msg spawn` is a clap subcommand so its argv is taken verbatim, flags included; `--socket` goes before
  `spawn`.
- A dialog that lands out of sight (its parent's tab in the background, or a slot without focus) is marked
  (the design's placement rule 1 says so now).
- `Stacks::clear_mark` is test-only: in the compositor `settle` drops the marks of windows seen.

Known limits:
- A waiting window (no buffer yet) is configured to the focused slot, so a launch's window aimed at another slot
  (focus moved since the launch) needs one more configure once placed.
- Monitor removal (tabs to the focused slot, else pending) is in the core only: nested there is one output, which
  never goes while orrery runs.
