# Input crossing Implementation Plan

**Goal:** keyboard and pointer follow the session's focus marker across machines (spec:
`docs/plans/2026-10-27-input-crossing-design.md`; read it first, and section 4 of `docs/plans/2026-10-26-orrery-design.md`).

**Architecture:** a pure crossing core (`src/crossing.rs`) decides where input goes, tracks keys and buttons down,
detects edge crossings and coalesces motion; new link messages carry input; the satellite feeds received input into
its normal input path; the primary's backends hold the pointer while it is away.

**Tech stack:** Rust 2024, Smithay 0.7 (seat, keyboard keymaps), the link from milestone 4.

**Ground rules for every task**
- TDD: write the failing test first, see it fail, then implement.
- Match the surrounding code: terse explanatory doc comments (≤120 columns), same naming, no new deps.
- Before committing: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` (full suite).
- orrery stays clean: no references to any other project, person, machine or personal setup.
- Tests never touch real user paths and only use loopback. Keystrokes are never logged.

---

### Task 1: the crossing core (`src/crossing.rs`) and the input messages (`src/link.rs`)

- Pure: `Crossing` with where keyboard/pointer go (local or a machine), keys down and buttons down per side, the
  modifier state to announce at handoff, `handoff(to)` returning the releases to apply locally and the Handoff to
  send, `reclaim()`, link-lost handling, edge crossing over a row of screens (machine, screen, width, height) with
  the entry height scaled, crossing back, and a motion coalescer (pending dx/dy merged while the link is busy; keys
  and buttons queue in order, never merged). Unit tests for each.
- Link messages (bump PROTOCOL_VERSION): Keymap {xkb string}, Handoff {mods, pointer entry (edge, y fraction) or
  none}, Key {keycode, pressed, time}, Motion {dx, dy}, Button {code, pressed}, Axis {source, h, v, v120_h, v120_v},
  Release (input leaves the receiver), and InputExit {edge, pointer y fraction} from a satellite whose pointer left
  its screens.
  Direction rules in `misplaced()`; round-trip tests; no Debug output that prints keycodes in logs (redact Key).

Commit `crossing: where input goes`.

### Task 2: the receiving side (satellite)

- Feed received Key/Motion/Button/Axis into the same input path as local input (binds included), with the received
  keymap applied to the seat at handoff (`set_keymap_from_string`), modifiers set from Handoff, everything released
  on Release or link drop. Pointer: enter at the given edge and height, move by relative motion clamped to its
  screens; past the left edge of its first screen or the right edge of its last, send InputExit (the primary hands
  the pointer to the machine beside that edge).
- A test-only `inject` request (keys, motion, buttons) usable on any orrery started with `--debug-inject` (refused
  otherwise), for the e2e tests.
- e2e (`tests/crossing.rs`): headless primary + satellite; marker to the satellite; keys injected on the primary
  arrive at a test client on the satellite (wl_keyboard events) and not on the primary's client; held Super at
  handoff; release on Release; link drop with keys down releases them on the satellite.

Commit `crossing: the receiving side`.

### Task 3: the sending side (primary) and the escape key

- Forwarding: while the marker is remote, keyboard and pointer input (from both backends) goes over the link instead
  of the local seat (escape key excepted); handoff on marker moves (releases locally, sends Keymap once per change +
  Handoff); edge crossing of the pointer moves only the pointer (and the marker, so the keyboard follows: design says
  focus decides — crossing the pointer onto a machine moves the marker there).
- Nested backend: on crossing out, hide the cursor and lock the host pointer (`set_cursor_grab(Locked)`, falling back
  to `Confined`), switch to relative motion (winit `DeviceEvent::MouseMotion`); on return, unlock and warp/show.
- `reclaim` bind action (default `Mod+Escape`), handled before forwarding; link drop while away = reclaim.
- `state.session.input`.
- e2e: pointer crossing and crossing back via injected motion; coalescing under a stalled link (stop reading on the
  satellite side via SIGSTOP) — motion arrives as one step, keys all arrive; reclaim; marker follows the pointer.
- README: input crossing section (what crosses, the escape key, nested pointer lock, live test steps on one machine).

Commit `crossing: the sending side`.

### Task 4: as built

Full suite, clippy, fmt, cleanliness scan, README vs design, "As built". Then the live one-machine test by hand.

## Notes for Task 3

- Match `reclaim` (the escape key) before `Crossing::key`, and never record the escape key there: its release then
  goes nowhere.
- Filter winit's key-repeat events before `Crossing::key`: receivers repeat keys themselves.
- On a machine's link loss, clear its `Outbox` along with `Crossing::link_lost`.
- Keymap and Handoff go through the same `Outbox` as keys, so nothing overtakes them (nor a Release what came
  before it).
- A Handoff that fails to send (a keymap too large for a frame included) is a link loss (`link_lost`), never half a
  handoff.
- The primary only logs an InputExit so far (`primary_got`): hand it on with `landing` and `Crossing::cross`.
- A satellite that sent an InputExit holds its pointer still (motion still in flight is dropped there) until its
  next Handoff. An exit that lands nowhere (past the row's end) must send that satellite a Handoff again (its held
  modifiers: keys already down there are not pressed twice), or its pointer stays stuck at the edge.
- `inject` calls the input path directly (`key_input`, `pointer_to`, `button_input`, `axis_input`; `comp/remote.rs`):
  send it through forwarding as device input goes, so the forwarding tests can drive the primary with it.
- A bind typed while input is on a satellite runs there: `workspace N` for another machine's number is refused there
  ("switch to it from the primary"); no message carries a session bind to the primary yet.

## As built

Commits, by task: 1 `8edede8`, `3052cf5`; 2 `902fb74`; 3 `7caf2d9`, `71bbcb5`, `ccb7260`, `e0fef8d`, `7cc52fd`; 4 this
one (design and plan: `e1c3996`). 379 tests (287 unit, 92 integration, 23 of them linked orrerys over loopback), one
more ignored (slow: every prefix of the example config); fmt and clippy clean. Link protocol 3.

Deviations from the design and the plan (folded into the design):
- InputExit instead of an edge that "leads back": a satellite knows only its own screens and reports where its
  pointer left; the primary lands it over the session's row (itself included), or answers with a Handoff when it
  lands nowhere, so the satellite's pointer is never left held.
- SessionRequest (satellite to primary): `workspace N` for another machine's number, typed on the satellite (its
  keys, forwarded or its own, or its control socket), is routed by the primary, so `Super+1` there comes home.
  `move-to-workspace` to such a number stays refused on the satellite.
- Motion coalescing by an InputAck credit window, not kernel buffers: the satellite acknowledges the input it ran
  (once per read), the primary keeps at most 32 input messages unacknowledged and the rest wait in its Outbox, where
  motion (and touchpad scroll, its stop kept apart) merges. TCP_NOTSENT_LOWAT (8 KiB) is set, kernel buffers are
  the defaults. Measured on loopback against a stopped satellite: NOTSENT_LOWAT alone took 19,200 motions to fill
  the link, the window takes 40.
- The receiver draws its cursor while it has the primary's input: the client's cursor surface at its hotspot, or a
  built-in square for a named cursor; with its own input the host's cursor stands for it.
- The Handoff carries the keyboard layout the primary is on (its index); the keymap goes once per link and the
  satellite uses it again at each Handoff, then its own keymap comes back on the layout it was on.
- Modifiers travel as the modifier keys held (keycodes pressed there), not a modifier mask; lock and latch keys stay
  each machine's own.
- Nothing crosses while a button is down (a drag stays on its machine); a marker move still hands input off.
- Crossing the pointer moves the marker, so the keyboard follows; the escape key brings the marker home too.
- The nested primary's input comes home when its window loses the host's keyboard focus (its keys forgotten, no
  releases will come).
- `inject` answers only with `--debug-inject` (no test build), and `inject-remote` was added for the receiving side.
- `state.session` has `input` and each machine's `queued`.

Known limits:
- A keyboard layout switched while input is on a satellite (a group toggle) switches only there: the primary comes
  home on the layout it was on, and the next handoff sends that one.
- While the primary's input is on a satellite, the satellite's own keyboard types with the primary's keymap.
- Two people at once: the satellite's own keyboard and the primary's forwarded one share its seat, so the same key
  pressed on both is one key there (a release from either lets it go).
- Scroll that does not merge (wheel steps, other sources) is replayed late after a stall, within the 32-message
  window.
- Nested-only behaviour is untested until the live test: the host pointer hidden, locked or confined (or loose, on a
  host with neither) and placed on return; relative motion crossing at orrery's outer edge (a window edge with more
  host screen beside it lets the pointer slip out before the lock holds); the receiver's drawn cursor; focus loss
  reclaiming.

Live one-machine test, 2026-10-29: passed. Two nested orrerys side by side in a host compositor (primary left,
satellite right): typing follows `workspace 2`; Super held across the handoff opens a terminal on the satellite;
`Super+Escape` brings input home with nothing left pressed; the pointer crosses past the primary's right edge (the
satellite draws its cursor) and back past the satellite's left edge; no crossing while a button is held; `Super+1`
typed on the satellite comes home; closing the satellite while input is there brings input home at once.
