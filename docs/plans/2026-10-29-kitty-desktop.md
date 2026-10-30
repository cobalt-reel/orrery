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

## As built

Commits, by task: 1 `3ba067d`, `c6bb008`; 2 `76df9b3`, `2fcee35`; 3 `745e8a0`, `e56db71`; 4 this one (design and
plan: `9301d03`). 465 Rust tests (343 unit, 122 integration, most of them headless end to end; 456 after task 2, 461
after task 3, 465 after its review), one more ignored (slow: every prefix of the example config); 30 Python tests in
`examples/kitty/test_bar.py` (also run by `tests/kitty.rs` when python3 is installed); fmt, clippy and the release
build clean; `sh -n` on the shell scripts clean.

Deviations from the design and this plan (folded into the design as "As built" notes):
- Layer-shell: placement and exclusive zones are Smithay's `LayerMap`; popups of bottom and background surfaces are
  drawn and hit over the windows (under top and overlay); `exclusive` on the bottom or background layer acts as
  `on_demand`; a buffer before an acked configure is a protocol error; `state` lists each monitor's layer surfaces.
- The event stream also has `closed` and `layers` events; changes are found by comparing what the events carry with
  what was last sent, published at most every 8 ms; the backlog is 1 MiB, at most 16 subscribers; `state` and the
  `config` event carry `colors`.
- Programs orrery starts also get `ORRERY_SOCKET`. `include` takes relative, absolute and `~/` paths (8 deep, 1 MiB,
  regular files, watched); `binds` blocks add up across files and an included bind cannot be overridden.
- Added beyond the plan: a program written `./...` or `../...` in a `spawn` bind or `spawn-at-start` is relative to
  the config file it is written in (so the snippet needs one `include` line and nothing on `PATH`); `launch=#false`
  on either records no launch; a launched program's layer surface uses its launch up when it first maps, and a layer
  surface activating with a launch's token spends it, so panels and launchers never leave a launch for a stranger's
  window.
- `examples/kitty/`: the bar is `bar.py` (one screen) run per screen in a `kitten panel` by `bars.py`; both stop when
  the orrery they follow is gone or replaced by another at the same socket (see the live test below), and the panels die with
  `bars.py`. The launcher honours TryExec, OnlyShowIn and NotShowIn and the desktop-file escapes. The desktop-ui
  steps are a README section (with `orrery-portals.conf` as a fallback), not a separate file.

Known limits:
- No popup grabs, for layer surfaces' popups as for windows': a click elsewhere does not close a menu.
- Smithay's zone arithmetic differs from the protocol's letter in corner cases (a positive zone on a surface
  anchored to a corner, or to two opposite edges only, still takes room along one edge; one leaving nothing is
  ignored); a surface that never draws keeps its zone.
- Nothing in `examples/kitty/` has been run against real kitty yet: no panel, drop-down, launcher or desktop-ui
  portal; whether kitty's `--output-name` matches orrery's output names is the first thing to see.
- A spawn with a launch whose program opens no surface at all still waits 5 s for a stranger's window; the snippet
  marks all its programs `launch=#false`, a user's own binds need the same.

Live test, first round: no bar. Fixed after it (one commit):
- `bars.py` ran but could not start `orrery msg subscribe`: orrery was not on `PATH`, and with no stderr the failure
  was invisible. Programs orrery starts now get `ORRERY` (orrery's own program, read once at startup; unset, logged,
  if the system cannot tell), and their stderr is orrery's (stdin and stdout stay null), as in most compositors: a
  failing program's complaint lands in orrery's log. `bar.py`/`bars.py` say once on stderr when `orrery msg` cannot
  be started, naming `$ORRERY` and `PATH`.
- A `bars.py` whose connection never succeeded had recorded no socket identity, so it followed a new orrery started
  at the same socket later and every screen got a second bar. Both scripts now record the control socket's identity
  at process start (orrery binds it before it starts its programs) and stop, panels with them, as soon as the socket
  is gone or another one is there at a reconnect; the minute without orrery applies only when no socket was known.
- 467 Rust tests (2 more: the `ORRERY` environment, and an end-to-end start that finds the binary under test through
  `$ORRERY` with a child's stderr in orrery's log); 34 Python tests (4 more: the message and its stderr line, the
  identity taken at start, both scripts stopping on a replaced or removed socket before any connection).

Live test, second round: Mod+grave opened kitty's quick-access terminal (an `exclusive` overlay), and then the user
was stuck: only quit, `reclaim` and `vt` ran while it had the keyboard, so Mod+grave could not close it, nor could
Mod+D, the workspace keys or any other bind run. Fixed after it (one commit):
- Every bind now runs whoever has the keyboard, an `exclusive` or `on_demand` layer surface as much as a window, as in
  other compositors; a bind's keys (press and release, as `Held` already kept them) never reach the surface, every
  other key does. `runs_over_exclusive` and the flag behind it are gone.
- 467 Rust tests (one unit test of the old rule gone; the end-to-end test that asserted binds were swallowed now
  asserts they run, and a new one has the bind that opened an `exclusive` overlay run again, a workspace bind switch
  and a plain key reach the overlay, with no bind's key reaching it); 34 Python tests.

Live test: pending again (nested and bare metal).
