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
  Release (input leaves the receiver), and InputBack {pointer y fraction} from a satellite whose pointer crossed back.
  Direction rules in `misplaced()`; round-trip tests; no Debug output that prints keycodes in logs (redact Key).

Commit `crossing: where input goes`.

### Task 2: the receiving side (satellite)

- Feed received Key/Motion/Button/Axis into the same input path as local input (binds included), with the received
  keymap applied to the seat at handoff (`set_keymap_from_string`), modifiers set from Handoff, everything released
  on Release or link drop. Pointer: enter at the given edge and height, move by relative motion clamped to its
  screens, cross back at the left edge of its first screen (send InputBack).
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
