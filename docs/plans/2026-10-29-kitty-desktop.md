# Layer-shell and a kitty desktop Implementation Plan

**Goal:** layer-shell, an event stream, session environment and config additions in orrery, and a kitty desktop in
`examples/kitty/` (spec: `docs/plans/2026-10-29-kitty-desktop-design.md`).

**Architecture:** Smithay's `WlrLayerShellState` with orrery's own per-screen layer bookkeeping (exclusive zones feed
the area `Stacks` divides); rendering and hit-testing gain the four layers; the control socket gains a streaming
request; examples are plain scripts plus kitty configs.

**Tech stack:** Rust 2024, Smithay 0.7 (`wayland/shell/wlr_layer`, `desktop::LayerMap`), wayland-protocols-wlr for
the test client; examples in POSIX sh + Python 3 (standard library only) + kitty 0.43+.

**Ground rules for every task**
- TDD; the full suite (`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`) and
  `cargo build --release` before every commit.
- Terse explanatory doc comments (≤120 columns), surrounding naming and idioms.
- orrery stays clean: no references to any other project, person, machine or personal setup (kitty is a public
  program; naming it in `examples/kitty/` and the README is fine).
- Tests never touch real user paths; no test starts kitty.

---

### Task 1: layer-shell

Layer surfaces per screen, the four layers in rendering and hit-testing, exclusive zones shrinking the screen's
usable area (`Stacks::set_area` gets the usable area; windows reconfigured), keyboard interactivity rules, popups,
screen removal closing them; `state` lists layer surfaces per screen (namespace, layer, rect). e2e with a test client.

Commit `layer-shell`.

### Task 2: the event stream and the session environment

`orrery msg subscribe` (snapshot then events, coalesced per loop iteration, bounded backlog, slow subscribers
dropped), `XDG_CURRENT_DESKTOP=orrery` and `XDG_SESSION_TYPE=wayland` for spawned programs, config `include` and
`spawn-at-start`. Unit and e2e tests; README.

Commit `subscribe, include, spawn-at-start`.

### Task 3: `examples/kitty/`

The bar (script + panel config), drop-down, launcher, desktop-ui steps, `orrery.kdl` snippet, a README section
"A kitty desktop". Script logic that does not need kitty (rendering a bar line from events) unit-tested with the
standard library (`python3 -m unittest`) and run from `cargo test` only if cheap; otherwise documented.

Commit `examples: a kitty desktop`.

### Task 4: as built

Checks, cleanliness scan, README vs design, "As built"; then the user's live test (nested and bare metal).
