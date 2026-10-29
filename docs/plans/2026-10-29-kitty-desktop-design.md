# orrery milestone 7: layer-shell and a kitty desktop (design)

orrery hosts desktop pieces drawn by other programs, and ships a small desktop built from kitty (bar, drop-down
terminal, launcher, file chooser) as examples. orrery stays a general window manager: everything kitty-specific lives
in `examples/kitty/`. Agreed 2026-10-29. Notifications, lock and idle stay later.

## Layer-shell (`zwlr_layer_shell_v1`)

- The four layers, drawn in order: background, bottom, the slots, top, overlay. A layer surface lives on one screen
  (the one it asks for, else the focused one), anchored to edges with margins and the size it asks for, as the
  protocol says.
- **Exclusive zones** shrink the area a screen's layout divides: a bar of height h along the top moves every slot
  down by h. The static layouts are recomputed live when a bar appears, resizes or goes; windows are reconfigured to
  their new slot sizes. (As built: placement and zones are Smithay's `LayerMap`, whose arithmetic differs from the
  protocol's letter in corner cases: a positive zone on a surface anchored to a corner, or to two opposite edges
  only, still takes room along one edge; a zone leaving nothing is ignored. A zone counts from the surface's first
  commit until it unmaps or goes, so one that never draws keeps its room.)
- **Keyboard:** `exclusive` takes the keyboard while the surface is mapped on the top or overlay layer (a drop-down,
  a file chooser), and gives it back to the focused window when it goes; `on_demand` takes it on click; `none` never.
  Binds still work while a layer surface has the keyboard, except that `exclusive` overlays get every key but the
  escape, quit and console (`vt`) binds. (As built: the frontmost `exclusive` surface wins, the focused screen's
  first; `exclusive` on the bottom or background layer acts as `on_demand`, as the protocol allows.)
- **Pointer:** hit-testing goes overlay → top → slots → bottom → background.
- Popups of layer surfaces (menus) work as for windows. Layer surfaces never enter stacks, tabs or workspaces. (As
  built: popups of bottom and background surfaces are drawn and hit over the windows, under top and overlay; no
  popup grabs, as for windows: a click elsewhere does not close a menu. A buffer before an acked configure is a
  protocol error. `state` lists each monitor's mapped layer surfaces. A launched program's layer surface uses its
  launch up when it first maps, as does a layer surface activating with a launch's token, so a panel or launcher
  never leaves a launch waiting for a stranger's window.)
- Bare metal, nested and headless alike; a removed screen closes its layer surfaces (the protocol's `closed`).

## The event stream (`orrery msg subscribe`)

- The connection stays open and orrery writes one JSON object per line: first a full `state` snapshot, then an event
  for each change: `workspace` (shown/created/gone, per screen), `focus` (screen, workspace, slot, window), `tabs` (a
  slot's tabs and titles changed), `title`, `attention`, `layout`, `session` (machines, link up/down, where input is),
  `config` (reloaded). Each event carries enough to redraw a bar without asking again. (As built: also `closed` (a
  window leaves the slots) and `layers` (a layer surface appears, goes, moves or takes the keyboard); `config`
  carries the colours, and `state` has `colors` (each `#rrggbbaa`). The snapshot is everything `state` carries but
  `last_key`.)
- Events are coalesced per loop iteration, at most every 8 ms. A subscriber that does not read keeps at most a
  bounded backlog; past it, it is dropped (its reader sees EOF and can reconnect). Keystrokes never appear. (As
  built: after each loop turn orrery compares what the events carry with what it last sent, publishing at most every
  8 ms; the backlog is 1 MiB; at most 16 subscribers; not for `--to`.)

## Session environment

- Programs orrery starts get `XDG_CURRENT_DESKTOP=orrery` (and `XDG_SESSION_TYPE=wayland`), besides
  `WAYLAND_DISPLAY`. (As built: and `ORRERY_SOCKET`, orrery's control socket, which `orrery msg` uses first.)
- The config gains `include "PATH"` (relative to the including file; cycles and depth limited), so a shipped snippet
  can be added to a user's config with one line. (As built: also absolute or `~/` paths; 8 deep; 1 MiB with
  everything included; regular files only; included files are watched. `binds` blocks add up across files, and an
  included bind cannot be overridden: binding its keys to something else is an error naming both places. A program
  written starting with `./` or `../` in a `spawn` bind or `spawn-at-start` is taken from the directory of the file
  it is written in, so a snippet starts the scripts beside it. `launch=#false` on either records no launch and hands
  no token, for programs that open no window of their own.)

## `examples/kitty/`

- **Bar** (`kitten panel` along the top of each screen, one per screen): left the workspaces (shown one highlighted,
  occupied ones marked, attention in the attention colour), middle the focused slot's tabs with titles (current one
  bright), right the session's machines with where input is, then a clock. A small script driven by
  `orrery msg subscribe`; colours from orrery's config (read once from `state`).
- **Drop-down terminal**: `kitten quick-access-terminal` on `Mod+grave`.
- **Launcher**: `Mod+D` opens an fzf picker in a kitty overlay over desktop applications and commands on `PATH`; the
  choice starts through `orrery msg spawn`, so it lands as a new tab like any launch.
- **File chooser and light/dark**: the `desktop-ui` kitten, with the steps to enable it (portals config, environment).
- **`orrery.kdl`**: an includable snippet with the binds and a `spawn-at-start` for the bar. (As built: the bar is
  `bar.py` (one screen, `--screen NAME`) run in a `kitten panel` per screen by `bars.py`, which keeps one per screen
  as screens come and go; both stop when the orrery they follow is gone or replaced. The snippet's programs are
  `./bars.py`, `./dropdown` and `./launcher.sh`, all `launch=#false`, plus a `dbus-update-activation-environment`
  line; `orrery-portals.conf` is a fallback portals config; the desktop-ui steps are in the README.)
- A config line `spawn-at-start "..."` (any number) runs programs when orrery starts (not on reload).

## Testing

Layer-shell through a test client on headless orrerys: exclusive zones resizing slots, layering and hit-testing,
keyboard interactivity, popups, screen removal. The event stream end to end (snapshot, events after changes, a slow
subscriber dropped). `include` and `spawn-at-start` unit- and e2e-tested. The kitty examples are checked by hand
(nested and bare metal). (As built: the bar's model and rendering, the bars' bookkeeping and the launcher's
desktop-entry reading are unit-tested with `python3 -m unittest` (no kitty, no orrery), run from `cargo test` when
python3 is there; the snippet is checked by `orrery check` and its programs for being executable.)
