# orrery

orrery is a Wayland compositor and static tiling window manager written in Rust on
[Smithay](https://github.com/Smithay/smithay). Each screen shows one **workspace**, and each workspace a fixed
**layout** of **slots** (rectangles of a weighted grid); every slot holds **tabs**, and each tab is a **stack** of
windows. A slot shows its current tab, and of that tab only the top window is drawn. A new window opens by three
rules: a dialog on top of its parent's stack; a window of a program orrery launched (a `spawn` bind or
`orrery msg spawn`) as a new tab in the slot focused at the launch (matched by process, else the first window within
5 s); anything else by `stacking`, on top of the focused slot's current stack (on, the default) or as a new tab
there (off). Windows never move by themselves: you move stacks between slots and workspaces, split a window off into
a tab of its own, cycle tabs and switch layouts and workspaces, with keys from a KDL config or with `orrery msg`
over a control socket.

There are 10 workspaces, numbered 1 to 10. Each screen has a number, kept while it lives (a new screen takes the
lowest free one), and workspace k is locked to screen k while that screen exists. Switching to a workspace shows it
on its screen and focuses that screen; one that does not exist is made on the focused screen and stays there. Each
workspace has its own layout, slots, tabs and focused slot, starting from its screen's layout (the `monitor` rule,
else the first layout); a layout switch changes only the workspace shown. A workspace neither locked, shown nor
holding a window goes away. Windows of a hidden workspace are unmapped but keep their slot's size, so showing them
again is instant. When a screen goes, its workspaces move intact to the focused remaining screen, hidden there.
On bare metal each connected monitor is a screen, coming and going as monitors are plugged in and out.
Nested, each screen is one orrery window: `open-screen` (or a second `orrery run`) opens another, which the host
places and sizes like any window, and closing one takes its screen away. Headless (tests) orrery opens and closes
screens on request too (`open-screen`, `close-screen`).

orrery runs on **bare metal** (DRM/KMS and libinput, started from a text console), **nested** as a window inside
another Wayland compositor, or **headless** (no screen) for tests. The design and the build order ahead are in
`docs/plans/2026-10-26-orrery-design.md`.

## Build and test

```sh
cargo build --release
cargo test                     # unit tests + headless end-to-end tests (no screen needed)
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

The tests run orrery with `--backend headless` in temp dirs with temp configs; they never read your
`~/.config/orrery` or bind your control socket.

## Running nested

```sh
cargo run --release -- run      # or: ./target/release/orrery run   (`run` is also the default verb)
```

The examples below write `orrery`: that is `./target/release/orrery` from the checkout, or put it on your PATH
(e.g. `cargo install --path .`).

This opens a window titled `orrery` (app id `orrery`) and prints `WAYLAND_DISPLAY=wayland-N` on stdout (logs go
to stderr). Clients go inside by pointing them at that display:

```sh
env -u DISPLAY WAYLAND_DISPLAY=wayland-N kitty
```

(`-u DISPLAY`, so no toolkit falls back to the host's Xwayland and opens outside orrery), or from inside orrery
with `Super+Return` (programs orrery spawns get its `WAYLAND_DISPLAY` and no `DISPLAY`). An idle orrery stays
near 0 % CPU.

Options: `--config PATH` (default `$XDG_CONFIG_HOME/orrery/config.kdl`, else `~/.config/orrery/config.kdl`; no
file means the built-in config, which is `examples/config.kdl`), `--socket PATH` (the control socket, default
`$XDG_RUNTIME_DIR/orrery.sock`), `--backend auto|drm|winit|headless` (below), `--size WxH` (the headless screen's
size), and `--debug-inject` (a testing aid, off by default: orrery then answers `inject` and `inject-remote`, below).

`--backend`: `auto` (the default) runs nested (`winit`) when `WAYLAND_DISPLAY` or `DISPLAY` is set and bare metal
(`drm`) otherwise, from a text console; `winit` and `drm` force one; `headless` has no screen and no input devices
(tests). Bare metal has a section of its own below.

A second `orrery run` on the same control socket does not start another orrery: it asks the running one for one
more screen (`open-screen`), prints the answer and exits, 0 if it was opened (its other options are not used).
Nested, that is one more window (same title and app id, so host window rules match it too), screen k's output
named `winit-k` (screen 1 is `winit`); put it on another host monitor and orrery has two screens. Closing a window
takes its screen away (its workspaces move, hidden, to the focused remaining screen, and the screens right of it
move left, so orrery's global space has no gap); closing the last one quits. A `run` that finds an orrery still
starting (it holds the lock but does not answer yet) waits up to a second for it to answer. A stale socket left by
a killed orrery (nothing listening) is replaced as usual.

orrery presents a window's frame only when the host asks for one, and everything else runs on its own timer, so a
hidden orrery window (on another workspace, say) neither stalls the others nor stops orrery answering `orrery msg`
and serving its clients; it just stops drawing until shown again.

### A Hyprland window rule (suggestion)

To give orrery's window its own workspace, fullscreen it, and keep it drawing while it is not visible, something
like this in a Hyprland config (adapt to your Hyprland version's rule syntax):

```ini
windowrule = workspace 3, match:class ^(orrery)$
windowrule = fullscreen on, match:class ^(orrery)$
windowrule = render_unfocused on, match:class ^(orrery)$
```

These match every orrery window: with several screens, leave out the `workspace` rule (or move the windows by hand
afterwards), or every screen lands on the same host workspace.

## Bare metal

```sh
orrery run      # from a text console: `auto` picks bare metal there (or force it: `--backend drm`)
```

Requirements: a seat, from logind, elogind or seatd (orrery opens the GPU and the input devices through it, never
directly, and needs no privileges), a GPU with KMS and GBM (Mesa), and a text console: log in on a free one
(`Ctrl+Alt+F3`, or `doas chvt 3` / `sudo chvt 3` from a running session) and run `orrery run` there. stdout prints
`WAYLAND_DISPLAY=...` as nested, logs go to stderr (redirect them to a file to keep them off the console:
`orrery run 2>/tmp/orrery.log`). Clients are started from inside with `Super+Return` (a terminal that runs on Wayland,
such as kitty or foot) or with `orrery msg spawn`.

- **Screens:** every connected monitor at its preferred mode, named after its connector as the kernel names it
  (`DP-3`, `HDMI-A-1`, `eDP-1`; `monitor "DP-3" layout="..."` rules match), side by side in the order they were
  found. Plugging one in adds a screen (the lowest free number), unplugging one takes it away with its workspaces
  moving as for any screen; the others close the gap. `open-screen` and `close-screen` answer that the monitors
  decide, and so does a second `orrery run` (it joins the running orrery, which says so).
- **Input:** keyboards, mice and touchpads through libinput, with the `keyboard` block (or `XKB_DEFAULT_*`). The pointer
  moves over the screens and stops at their edges (in a session of linked machines, pushed past the outer edges it
  crosses as nested). orrery draws the pointer: the cursor the client under it asked for, else the named cursor from
  the xcursor theme (`XCURSOR_THEME` and `XCURSOR_SIZE`, default `default` and 24), else a small light square.
- **Consoles:** `Super+Ctrl+F1` ... `F12` and `Ctrl+Alt+F1` ... `F12` switch consoles (they are binds, so they work
  with any `mod`). Switching away pauses orrery: it stops drawing and reading input, and lets go of every key and
  button held; switching back redraws everything (monitors plugged meanwhile are picked up then).
- `Super+Shift+E` quits, giving the console back; so does a crash (the seat restores it).
- No seat, no GPU or no connected monitor: orrery says which and exits non-zero.
- Limits: one GPU (the boot one); no Xwayland; no hardware cursor (the pointer is drawn into each frame); client
  buffers are drawn into each frame, never put on a monitor directly (see linux-dmabuf); scale 1,
  each monitor at its preferred mode; no lock or idle; touchpads keep libinput's
  defaults (no tap-to-click, no natural scrolling); keyboard LEDs (Caps Lock, Num Lock) do not light; touch and
  tablet tools are ignored, and absolute pointers (a virtual machine's, say) map onto the screen the pointer is on.
- A monitor the kernel keeps refusing (after one reset of every output) is closed as if unplugged and stays dark
  until it is plugged in again.

## Default keys

`Mod` is **Super** by default. Nested, the host compositor's own Super binds win before orrery sees a key, so
hand the keys over first. In Hyprland a passthrough submap lets every key reach the nested window:

```
bind = SUPER, F12, submap, passthru
submap = passthru
bind = SUPER, F12, submap, reset
submap = reset
```

Press `Super+F12` with orrery focused (every key now goes to orrery) and `Super+F12` again to give the host its
keys back. Or change the modifier with `mod "..."` in the config (a combination works too: `mod "Super+Ctrl"`).

| Keys | Action |
|------|--------|
| `Super+H` / `J` / `K` / `L` | focus the slot left / down / up / right |
| `Super+Shift+H` / `J` / `K` / `L` | move the current tab (its whole stack) to the slot left / down / up / right, as a new tab there (focus follows) |
| `Super+Ctrl+H` / `J` / `K` / `L` | swap everything (all tabs) with the slot left / down / up / right (focus follows) |
| `Super+Tab` / `Super+Shift+Tab` | show the focused slot's next / previous tab (wrapping; tabs never move) |
| `Super+S` | split: the top window leaves its stack for a new tab of its own, right after |
| `Super+Shift+S` | toggle stacking: a window orrery did not launch and with no parent goes on top of the focused stack (on) or opens as a new tab (off) |
| `Super+1` ... `Super+9`, `Super+0` | show workspace 1 ... 9, 10 (made on the focused screen if it does not exist) |
| `Super+Shift+1` ... `Super+Shift+0` | move the current tab (its whole stack) to workspace 1 ... 10, as a new tab in its focused slot (focus stays) |
| `Super+Ctrl+1` / `Super+Ctrl+2` | layout `wall` / `halves` (of the workspace shown) |
| `Super+Return` | spawn `kitty` |
| `Super+Shift+Q` | close the focused window (asks it to close) |
| `Super+Space` (held) | show tab strips along the top of each slot holding more than one window: one segment per tab, the current tab bright, pips for its depth |
| `Super+Shift+E` | quit orrery |
| `Super+Escape` | in a session: keyboard and pointer back to this machine (see Input crossing) |
| `Super+Ctrl+F1` ... `F12`, `Ctrl+Alt+F1` ... `F12` | switch to console 1 ... 12 (bare metal; nested and headless they do nothing but log) |

A slot holding more than one window (in any tab, at any depth), or a marked one (below), shows a small badge in its
top-right corner. A click focuses the slot under the pointer.

A window asking to be seen (xdg-activation) is never raised, and takes focus only with input behind it: when it is
what its slot shows and its token came from input to the focused program (a token carrying a serial given since that
program got the keyboard, used within 5 s, once), its slot is focused. Otherwise it is marked for attention: its slot
shows the badge in the `attention` colour (its tab's strip segment too) until the window is seen: brought on top (a
`cycle-tab`, a close above it) if it was beneath, focused if it was already shown. The mark does not come back. A
dialog opening where it is not seen (on a parent in a background tab, or in a slot without focus) is marked the same
way. One exception: each spawn hands its program a token (`XDG_ACTIVATION_TOKEN`, `DESKTOP_STARTUP_ID`), and a window
asking with the token of a spawn under 5 s old that no window has taken yet moves to where the spawn was made, as a
new tab there (a single-instance program opening its existing window from a new launch, say).

## `orrery msg`

One request to the running orrery; the answer is JSON on stdout (pretty for `state`), exit 0 only when it says
ok.

```sh
orrery msg state                      # monitors, workspaces, layouts, slots (rects, focus), tabs and their windows
orrery msg subscribe                  # the event stream: the state, then a line per change, until it ends (below)
orrery msg focus-slot right           # left | right | up | down
orrery msg move right                 # the current tab, its whole stack, to the neighbour
orrery msg swap-slot down
orrery msg cycle-tab next             # next | prev
orrery msg split                      # the top window becomes a tab of its own
orrery msg toggle-stacking
orrery msg spawn kitty --single-instance   # every word after spawn is the argv (so --socket goes before spawn)
orrery msg layout halves              # optionally a monitor: orrery msg layout halves winit
orrery msg workspace 3                # 1 to 10: show it on its screen (made on the focused screen if new)
orrery msg move-to-workspace 3        # the current tab, its whole stack, to workspace 3; focus stays
orrery msg open-screen                # one more screen, numbered the lowest free (nested: one more window)
orrery msg close-screen headless-2    # take a screen away by output name (for tests; nested, close the window)
orrery msg close
orrery msg reload                     # re-read the config now (it is also watched)
orrery msg quit
orrery msg '{"cmd":"layout","name":"wall"}'   # a single argument is taken as a JSON request
orrery msg --socket /path/to/orrery.sock state   # default: $ORRERY_SOCKET, else $XDG_RUNTIME_DIR/orrery.sock
orrery msg --to side state            # any request, run on that machine of the session (see Linking machines)
orrery msg inject key 38 down         # testing aid (--debug-inject only): input as if from this machine's devices
                                      #   (forwarded as theirs is): key XKB-KEYCODE down|up, motion DX DY (relative),
                                      #   button CODE down|up, axis H V
orrery msg inject-remote side release # testing aid (--debug-inject, primary): an input message sent to a satellite
                                      #   as it is: the same four, handoff [KEYCODE...], release; keymap in JSON
```

Errors come back as `{"ok":false,"error":"..."}` (also printed on stderr), e.g. an unknown layout or monitor.

`state` lists `monitors`, each as `{ name, number, workspace, layout, focused, slots, layers }`: its screen number,
the workspace it shows, that workspace's layout and slots, and its mapped layer surfaces back to front as drawn
(`{ namespace, layer, rect, keyboard }`, see [Layer-shell](#layer-shell)); and `workspaces`, every existing one by number, as
`{ number, monitor, shown, layout, focused_slot, slots }` (`monitor` by name; `slots` as a monitor lists them, a
hidden workspace's never focused). Each slot is `{ index, name, rect, focused, tabs }`. Each slot's `tabs` lists its
tabs left to right, each as `{ current, attention, windows }` with `windows` (`id`, `app_id`, `title`) bottom to
top; `attention` is true when a window of that tab asked to be seen. Besides these, `state` carries
`stacking` (the running value), `config_error` (why the config file was not loaded, or null), `launches` (recent
spawns, oldest first, as `{ pid, token, age_ms, claimed }`: the activation token each was handed and whether a
window has taken it; a debug aid; one over 5 s old no longer counts and goes on the next spawn, new window or
activation), `frames` (how long drawing a frame takes on the compositor thread: `count`, `min_us`, `mean_us`,
`p95_us`, `max_us` over about the last minute; absent before the first frame and headless, which draws nothing),
`dmabuf` (what [linux-dmabuf](#gpu-buffers-linux-dmabuf) advertises: `formats`, how many format and modifier pairs
clients may use, and `device`, the GPU named as the main device; absent headless) and
`last_key`: the last key press as the bind lookup read it (`key`, `mods`, and the `bind` it matched, if any). When a
bind does nothing, `last_key` tells whether the key reached orrery at all and with which modifiers. Plain typing (no
Ctrl, Alt or Super) is kept only as `(plain key)`, never which key it was. `colors` holds the config's colours
(`focus`, `badge`, `attention`, `background`, each `#rrggbbaa`), for programs drawing in orrery's. In a session,
`state` also carries `session` (see [Linking machines](#linking-machines)); alone it has none.

### The event stream

`orrery msg subscribe` keeps its connection open and prints one JSON object per line as they come, until orrery
ends the stream (it quits, or drops a subscriber that leaves more than 1 MiB unread: reconnect then); it exits 0
then, 1 when nothing answers or the subscription is refused (at most 16 subscribers at once). It is not for `--to`.
The first line is `{"event":"state", ...}`: everything `state` carries, at the top level, except `last_key`: no
keystroke ever appears in the stream. Then a line per change: after each turn of its event loop, at most every
8 ms, orrery compares what the events carry with what it sent last, so changes in between come as one event, and
every change a bar shows is seen however it was made (a key, `msg`, a client, a reload, another machine). Each event
carries all a bar needs to redraw its part without asking again; at one time they come in this order:

| `event` | When | Carries |
|---------|------|---------|
| `workspace` | a screen shows another workspace, or one appears, goes, gains or loses windows or attention | `screens`, each `{ name, number, workspace, focused }`; `workspaces`, every existing one by number as `{ number, screen, shown, occupied, attention }` |
| `layout` | a screen's layout or its slots' rectangles change (a layout or workspace switch, a resize, a bar's exclusive zone) | `screens`, each `{ name, workspace, layout, slots }`, the slots as `{ index, name, rect }` |
| `focus` | the focus moves to another screen, workspace, slot or window | `screen`, `workspace`, `slot`, `slot_name`, `window` (`{ id, app_id, title }`, null for an empty slot); all null without a screen |
| `tabs` | another slot is focused, or the focused slot's tabs, current tab, windows or titles change | `screen`, `workspace`, `slot` and its `tabs`, as `state` lists them (`{ current, attention, windows }`) |
| `title` | a window enters a slot (opened, or mapped again), or its title or app id changes; one per window | `id`, `app_id`, `title` |
| `closed` | a window leaves the slots (closed, or unmapped by its client); one per window | `id` |
| `attention` | a window asks for attention, or its mark goes | `windows` (ids) and `workspaces` (numbers) asking |
| `layers` | a layer surface appears, goes, moves or takes the keyboard | `screens`, each `{ name, layers }`, each layer `{ namespace, layer, rect, keyboard }` |
| `session` | a machine joins or leaves, a link goes up or down, input moves, the link error changes (latency alone does not) | `session` as in `state`, null alone |
| `config` | the config file was read again (a reload, good or broken) | `error` (as `config_error`) and `colors` |

```json
{"event":"focus","screen":"winit","workspace":1,"slot":3,"slot_name":"hero","window":{"id":7,"app_id":"kitty","title":"~"}}
{"event":"title","id":7,"app_id":"kitty","title":"vim"}
{"event":"workspace","screens":[{"name":"winit","number":1,"workspace":2,"focused":true}],"workspaces":[{"number":1,"screen":"winit","shown":false,"occupied":true,"attention":false},{"number":2,"screen":"winit","shown":true,"occupied":false,"attention":false}]}
```

## Programs orrery starts

`spawn` binds, `orrery msg spawn` and `spawn-at-start` run a program directly (no shell: write `"sh" "-c" "..."` for
one), detached (a process group of its own, no stdin or stdout), in orrery's own environment with:
`WAYLAND_DISPLAY` set to orrery's socket; `DISPLAY` removed, so no toolkit falls back to the host's X server;
`XDG_CURRENT_DESKTOP=orrery` and `XDG_SESSION_TYPE=wayland`, which portals, toolkits and desktop entries read;
`ORRERY_SOCKET` set to orrery's control socket, which `orrery msg` uses when not given `--socket` (so a program talks
to the orrery that started it); `ORRERY` set to the absolute path of orrery's own program, so a script runs
`"$ORRERY" msg ...` whether or not orrery is on `PATH`; and an xdg-activation token as `XDG_ACTIVATION_TOKEN` and
`DESKTOP_STARTUP_ID`, so its window opens where it was started. Their stderr is orrery's: what a program says when it
fails lands where orrery's own log goes (start orrery with `2>>orrery.log` to keep both out of a terminal). A program written in the config starting with `./` or `../` is taken from the
directory of the config file it is written in (so an included snippet can start the scripts beside it, see
[A kitty desktop](#a-kitty-desktop)); its arguments are passed as written.

Every start is recorded as a **launch** waiting up to 5 s for its window, and a window that cannot be matched to a
launch by process takes the oldest waiting one. A launched program's layer surface (a panel, a launcher) uses its
launch up when it first maps, as does a layer surface activating with a launch's token, though nothing is placed. A
program that opens no surface at all (a toggle that signals another process and exits, say) would leave its launch
waiting for a stranger's window: start it with `launch=#false` (on a `spawn` bind or `spawn-at-start`), which records
no launch and hands it no token.

## The config, its reload and `orrery check`

orrery looks at its config file and the files it includes once a second (modification time and size) and reloads it when
one changes, or at once on `orrery msg reload`. A good file replaces binds, colours and layouts (`stacking` toggled at
runtime is kept, unless the file's own `stacking` value changed): every workspace, hidden ones too, takes the new
definition of its layout by name (a workspace whose layout is gone keeps it; that is logged, not an error). Monitor
rules apply to new outputs only, except after a start on the built-in config (the file was missing or broken): the first
good file then also applies its monitor rules, as a start with it would have. A broken file changes nothing: the old
config stays, the error is in `orrery msg state` as `config_error`, and a **4 px red bar** runs along the top of every
output until a good config loads. At startup a broken file means the built-in config, with the bar. A file deleted while
running keeps the running config.

```sh
orrery check                          # the default path; or: orrery check --config PATH
```

prints `ok` (exit 0), or every problem as `FILE:LINE:COL: message` on stderr (exit 1). A missing file is exit 1
too (orrery would use the built-in config). With `listen` or `primary` it also reads the secret file as `run` would
(what is wrong with it is an error, exit 1) and prints a `link:` line: the machine's name, its role and address,
and the secret file's path.

### Reference

The config is [KDL v2](https://kdl.dev), read strictly: unknown nodes, properties, actions and modifiers are
errors, as are duplicates (several `binds` blocks are fine, their binds adding up, and a key combination bound
twice to the same action is one bind: with `mod "Alt"`, the
example's `Mod+Ctrl+F1` and `Ctrl+Alt+F1` are). Every node is optional except at least one `layout`.

- `mod "NAME"`: what `Mod` means in binds. One of `Alt`, `Ctrl` (or `Control`), `Shift`, `Super` (or `Logo`),
  or several joined by `+` (`"Alt+Shift"`); default `Super`.
- `layout "NAME" { ... }`: a named grid, names unique. Children:
  - `grid cols=N rows=N`: 1 to 12 each way.
  - `slot ["NAME"] col=N row=N [cols=N] [rows=N]`: a rectangle of cells, 0-based from the top-left, `cols`
    and `rows` default 1. Slots must stay inside the grid, not overlap, and cover every cell, and be at least
    64 px each way at 3840x2160; slot names are optional and unique. Slot order is the order written (the first
    is slot 0).
  - `widths W...` and `heights W...` (optional): relative column widths and row heights, one whole number from
    1 to 10000 per column (row); e.g. `widths 2 1 1` makes the first column half the width. Without them the
    columns (rows) are equal. Column *i* starts at `x + width * sum(widths before i) / sum(widths)`.

  Columns split the width with integer math, so slots tile the output exactly; focus moves and other neighbour
  questions are answered in grid cells, whatever the weights.
- `monitor "OUTPUT" layout="NAME"`: the layout an output's workspaces start with. The nested outputs are `winit`,
  `winit-2`, ... (one per window, screen k is `winit-k`), the headless ones `headless`, `headless-2`, ...; an output
  without a rule gets the first layout.
- `stacking #true|#false`: where a window that orrery did not launch and that has no parent opens: on top of the
  focused slot's current stack (`#true`, the default) or as a new tab there; `toggle-stacking` flips it while
  running. Dialogs go on their parent's stack and launched programs' windows open as new tabs where the launch was
  made, whatever `stacking` says.
- `listen "ADDR:PORT" secret-file="PATH"`: this machine is a session's primary, listening for satellites on that
  IP address and port (`0.0.0.0:7340` for every interface; port 0 picks one, shown in `state`).
- `primary "HOST:PORT" secret-file="PATH"`: this machine is a satellite of the primary there (a host name or an IP
  address, IPv6 in brackets). A machine has `listen` or `primary`, never both. `PATH` is absolute, or starts with
  `~/` for the home directory.
- `name "NAME"`: this machine's name in a session, 1 to 64 bytes, unique in it; default: the hostname.
- `keyboard { layout "..."; variant "..."; options "..."; model "..."; repeat-delay MS; repeat-rate N }`: every
  child optional. The four xkb names are as xkb takes them (`layout "us,de"`, `options "grp:alt_shift_toggle"`);
  `options` and `model` left out come from `XKB_DEFAULT_OPTIONS` and `XKB_DEFAULT_MODEL`, else xkb's own default.
  The variant comes with the layout: both from the block (`variant` needs `layout`), or both from
  `XKB_DEFAULT_LAYOUT` and `XKB_DEFAULT_VARIANT` (else `us`). An empty `variant` or `options` means none. A keymap
  xkb cannot build is a config error (so `orrery check` catches a misspelt layout). `repeat-delay` is the hold
  before a key repeats, 1 to 10000 ms (default 200); `repeat-rate` the repeats a second, 0 (no repeat) to 1000
  (default 25). A reload that changes the block sends the new keymap (on the layout in use, when the layouts are
  the same) and repeat to every client (in a session, while the primary's input is here, the new keymap comes back
  with this machine's own input).
- `colors { focus "#rrggbb"; badge "#rrggbb"; attention "#rrggbb"; background "#rrggbb" }`: each also
  `#rrggbbaa`. `focus` is the 1 px border inside the focused slot, `badge` the badge on a slot holding more than
  one window and the tab strips, `attention` the badge of a slot holding a window that asked for attention,
  `background` what empty slots show. Defaults as in the example.
- `include "PATH"`: reads another config file as if its nodes stood in place of the line, so a snippet joins a
  config with one line. `PATH` is relative to the including file's directory, absolute, or starts with `~/` for the
  home directory. An included file may hold anything the main one may (its `binds` add to the others'; anything else
  set twice is an error wherever the two are) and include more, up to 8 deep; a cycle is an error naming the chain
  of files, and the config with everything it includes is at most 1 MiB. Only regular files are read (not a FIFO or
  a device). Problems in an included file, or a missing one, are config errors like any (`FILE:LINE:COL` names the
  file); the watcher watches included files too, a missing one included (creating it reloads), and `orrery check`
  follows includes. An included bind cannot be overridden: binding its keys to something else is an error naming
  both places, so change such a bind by removing it from the snippet (or not including the snippet).
- `spawn-at-start "PROGRAM" ["ARG"...]`: run once when orrery starts, with its screens and control socket up, as a
  `spawn` bind runs it (see [Programs orrery starts](#programs-orrery-starts)); any number, in order. A `PROGRAM`
  starting with `./` or `../` (here and in `spawn` binds) is relative to the file the line is in. Never again on
  a reload (nor when a config broken at startup is fixed by one). `launch=#false` after the arguments records no
  launch for it (see [Programs orrery starts](#programs-orrery-starts)).
- `binds { "COMBO" ACTION [ARGS...] }`: `COMBO` is modifiers and one key joined by `+`, e.g. `"Mod+Shift+H"`:
  modifiers `Mod`, `Alt`, `Ctrl`/`Control`, `Shift`, `Super`/`Logo`; the key is an xkb keysym name, any case
  (`H`, `Return`, `Tab`, `Space`, `1`, `F5`); the `+` key is written `plus` (`"Mod+plus"`). Write the unshifted key
  with `Shift`, not the shifted symbol: binds match the key's unshifted symbol on the current keyboard layout, so
  where digits need Shift (AZERTY, say) write the base symbol, e.g. `"Mod+ampersand"` for the `1` key. Caps and Num
  Lock are ignored. Actions:

  | Action | Arguments | Does |
  |--------|-----------|------|
  | `focus-slot` | `"left"`/`"right"`/`"up"`/`"down"` | focus the neighbouring slot (nothing at the edge) |
  | `move` | a direction | move the focused slot's current tab, its whole stack, to the neighbour as a new tab |
  | `swap-slot` | a direction | swap everything (all tabs) of the focused slot and its neighbour |
  | `cycle-tab` | `"next"`/`"prev"` | show the focused slot's next / previous tab |
  | `split` | | the top window leaves its stack for a new tab right after it |
  | `toggle-stacking` | | flip `stacking` (windows orrery did not launch, with no parent) |
  | `layout` | a layout name | switch the layout of the focused screen's workspace (fewer slots: the extra slots' tabs join the last) |
  | `workspace` | a number, 1 to 10 (`workspace 3`, not `"3"`) | show that workspace on its screen and focus it (made on the focused screen if new) |
  | `move-to-workspace` | a number, 1 to 10 | move the focused slot's current tab, its whole stack, to that workspace as a new tab (focus stays) |
  | `spawn` | program, then its arguments; optionally `launch=#false` | run it (no shell; see [Programs orrery starts](#programs-orrery-starts)); `launch=#false`: record no launch for it, the one property a bind takes |
  | `close` | | ask the focused window to close |
  | `show-tabs` | | while held: the tab strips |
  | `quit` | | stop orrery |
  | `vt` | a console number, 1 to 12 (`vt 2`) | switch to that console (bare metal; elsewhere it is logged and does nothing) |

The example, which is also the built-in config (`examples/config.kdl`; copy it to `~/.config/orrery/config.kdl`
to start your own):

```kdl
// ~/.config/orrery/config.kdl
mod "Super"               // the bind modifier written as `Mod`; nested, let the host pass it through

layout "wall" {
    grid cols=3 rows=3
    slot col=0 row=0
    slot col=0 row=1
    slot col=0 row=2
    slot "hero" col=1 row=0 cols=2 rows=2
    slot col=1 row=2
    slot col=2 row=2
}
layout "halves" {
    grid cols=2 rows=1
    slot col=0 row=0
    slot col=1 row=0
}

monitor "winit" layout="wall"     // output name -> starting layout; unknown outputs get the first layout

// keyboard { layout "us"; options "ctrl:nocaps"; repeat-delay 200; repeat-rate 25 }  // unset: XKB_DEFAULT_*

stacking #true            // windows orrery did not launch, with no parent: on the focused stack; #false: a new tab

colors {
    focus "#ff7edb"
    badge "#ff7edb"
    attention "#ffb000"
    background "#1a1625"
}

binds {
    "Mod+H" focus-slot "left"
    "Mod+J" focus-slot "down"
    "Mod+K" focus-slot "up"
    "Mod+L" focus-slot "right"
    "Mod+Shift+H" move "left"
    "Mod+Shift+J" move "down"
    "Mod+Shift+K" move "up"
    "Mod+Shift+L" move "right"
    "Mod+Ctrl+H" swap-slot "left"
    "Mod+Ctrl+J" swap-slot "down"
    "Mod+Ctrl+K" swap-slot "up"
    "Mod+Ctrl+L" swap-slot "right"
    "Mod+Tab" cycle-tab "next"
    "Mod+Shift+Tab" cycle-tab "prev"
    "Mod+S" split
    "Mod+Shift+S" toggle-stacking
    "Mod+Ctrl+1" layout "wall"
    "Mod+Ctrl+2" layout "halves"
    "Mod+1" workspace 1
    "Mod+2" workspace 2
    "Mod+3" workspace 3
    "Mod+4" workspace 4
    "Mod+5" workspace 5
    "Mod+6" workspace 6
    "Mod+7" workspace 7
    "Mod+8" workspace 8
    "Mod+9" workspace 9
    "Mod+0" workspace 10
    "Mod+Shift+1" move-to-workspace 1
    "Mod+Shift+2" move-to-workspace 2
    "Mod+Shift+3" move-to-workspace 3
    "Mod+Shift+4" move-to-workspace 4
    "Mod+Shift+5" move-to-workspace 5
    "Mod+Shift+6" move-to-workspace 6
    "Mod+Shift+7" move-to-workspace 7
    "Mod+Shift+8" move-to-workspace 8
    "Mod+Shift+9" move-to-workspace 9
    "Mod+Shift+0" move-to-workspace 10
    "Mod+Return" spawn "kitty"
    "Mod+Shift+Q" close           // as Hyprland's killactive
    "Mod+Space" show-tabs        // held: tab strips visible while down
    "Mod+Shift+E" quit
    "Mod+Escape" reclaim         // keyboard and pointer back to this machine (in a session)
    "Mod+Ctrl+F1" vt 1           // to console 1 ... 12 (bare metal; elsewhere it does nothing)
    "Mod+Ctrl+F2" vt 2
    "Mod+Ctrl+F3" vt 3
    "Mod+Ctrl+F4" vt 4
    "Mod+Ctrl+F5" vt 5
    "Mod+Ctrl+F6" vt 6
    "Mod+Ctrl+F7" vt 7
    "Mod+Ctrl+F8" vt 8
    "Mod+Ctrl+F9" vt 9
    "Mod+Ctrl+F10" vt 10
    "Mod+Ctrl+F11" vt 11
    "Mod+Ctrl+F12" vt 12
    "Ctrl+Alt+F1" vt 1           // the usual console keys, as binds
    "Ctrl+Alt+F2" vt 2
    "Ctrl+Alt+F3" vt 3
    "Ctrl+Alt+F4" vt 4
    "Ctrl+Alt+F5" vt 5
    "Ctrl+Alt+F6" vt 6
    "Ctrl+Alt+F7" vt 7
    "Ctrl+Alt+F8" vt 8
    "Ctrl+Alt+F9" vt 9
    "Ctrl+Alt+F10" vt 10
    "Ctrl+Alt+F11" vt 11
    "Ctrl+Alt+F12" vt 12
}
```

## Linking machines

orrerys on several machines make one **session**: one **primary** and any number of **satellites**. Workspace
numbers are one space across the session: the primary's screens come first, then each satellite's in the order they
join, and workspace k is locked to screen k wherever that screen is. Each machine keeps its own windows; the
primary's keyboard and pointer go where the session's focus marker is (Input crossing, below).

```kdl
// the primary
name "desk"
listen "0.0.0.0:7340" secret-file="~/.config/orrery/secret"

// each satellite
name "side"
primary "desk.lan:7340" secret-file="~/.config/orrery/secret"
```

The **secret file** holds the same secret on every machine, 32 to 4096 bytes (whitespace at either end aside, so a
trailing newline does not matter), and gives group and others no permission at all (orrery refuses it otherwise,
naming the file, and never logs or shows its contents):

```sh
(umask 077; head -c 32 /dev/urandom | base64 > ~/.config/orrery/secret)   # then copy it to each machine, 0600
```

Holding it is a shell on every machine in the session (`--to` can spawn programs), so guard it like a private key.
The link is TCP, encrypted and authenticated with Noise on that secret: a machine with another secret never joins
(the primary logs the failed handshake) and nothing crosses in clear. The satellite connects; the primary only
listens. To cross a firewall or NAT one way is an ssh reverse tunnel from the primary, so the satellite reaches it at
its own loopback:

```sh
ssh -R 7340:127.0.0.1:7340 side       # on the primary; the satellite's config says primary "127.0.0.1:7340"
```

- **Joining:** a satellite keeps its workspace numbers where they are free in the session; a number already taken
  moves, windows and screen with it, to the lowest free one (two machines that both start with screen 1: the
  satellite's becomes 2). A name already in the session, or another link protocol version, is refused with a reason
  shown on both ends (the satellite's in `state`).
- **`workspace N`** (keys or `orrery msg`) on the primary goes to the machine that has N: that machine shows it, and
  the session's focus marker moves there, the primary's keyboard and pointer with it. A number nobody has is made on
  the machine holding the marker. On a satellite, a number another machine has goes to the primary, which routes it
  the same way (answered ok once sent; a failure is logged on the primary).
- **`move-to-workspace N`** to a number on another machine is refused: windows never cross machines.
- **`orrery msg --to MACHINE <request>`** runs any request on that machine and prints its answer (`--to side
  spawn foot`, `--to side state`); on the primary, through the link (an error after 5 s without an answer); naming
  the machine itself runs it there; a satellite sends nothing on. An answer must fit one link frame (about 64 KB):
  a `--to … state` of a machine with very many windows comes back as an error saying so.
- **Dropping:** a satellite that loses the primary keeps working alone with every workspace number it has, and
  tries again every 1 s, backing off to every 5 s; the primary frees its numbers meanwhile. On the rejoin it keeps
  them where still free. A link silent for 10 s counts as lost.
- `listen`, `primary`, `name` and the `secret-file` path take effect when orrery starts. A reload reads the secret
  file's contents again: a different secret drops every link at once, and satellites get back in once they have
  it too (rewrite the file on every machine, then reload each); a secret that cannot be read is the
  `config_error`, and the one read before stays in use.
- The primary refuses a connection beyond 8 still joining, or beyond 2 from one IP address. A satellite whose
  name is held by a link silent for 4 s (a machine that froze or lost power) takes its place instead of being
  refused.

`state.session` shows this machine's place: `machine` (its name), `role` (`primary` or `satellite`), `listen` (the
primary's address, with the port it picked for 0), `primary` (a satellite's primary, as configured), `input` (the
machine this one's keyboard and pointer go to: on the primary, the one holding the focus marker; a satellite's own
stay on it), `foreign` (the workspace numbers other machines own), `error` (why a link last failed, until one opens),
and `machines`: on the primary every machine in the session (itself first, then the satellites in join order), on a
satellite itself, each as `{ name, role, monitors, workspaces, latency_ms, connected, focused, queued }` (`monitors`
as `{ name, number }`; `latency_ms` the link's round trip, pinged every 2 s, null for the primary itself, before the
first answer and while a ping is overdue; `focused`: the session's focus marker is there, known on the primary only:
a satellite shows false; `queued`: the primary's input waiting for that machine's link to take more, a stalled link
shows here).

## Input crossing

The primary's keyboard and pointer go to the machine holding the session's focus marker. With the marker on the
primary, input works as always; with it on a satellite, every key, button, scroll and pointer movement of the
primary's own devices travels over the link and runs there through the satellite's own input path, binds included,
as if typed on it. The satellite's own keyboard and pointer keep working on it all along (only the primary forwards).

- **What moves input:** `workspace N` for a number on another machine (keys or `orrery msg`; typed on the satellite,
  through the primary, so `Super+1` there comes home), and the pointer: the session's screens sit in a row, the
  primary's then each satellite's in join order, and pushing the pointer past the right edge of a machine's last
  screen crosses onto the next machine's first, past the left edge of a machine's first onto the previous machine's
  last, at the same height (as a fraction of the screen's). The marker crosses with it, so the keyboard follows.
  Past either end of the row nothing happens. While a button is down the pointer stays on its machine (a drag).
- **Keys:** raw keycodes travel, with the primary's keymap (once per link) and the layout it is on (at each
  handoff), so the user's layout follows them. At each handoff everything down on the machine input leaves is
  released there, and the machine it goes to is told which modifiers are held: hold `Super`, press `2`, and Super is
  still down on the satellite. Lock keys (Caps, Num) stay each machine's own. Receivers repeat keys themselves;
  nothing repeats over the link. While the primary's input is on a satellite, the satellite's own keyboard types
  with the primary's layout too; its own comes back, on the layout it was on, when input leaves.
- **`Super+Escape`** (bind action `reclaim`) is matched on the primary before anything is forwarded and never
  crosses: keyboard and pointer come home, the marker with them, and the satellite lets go of everything the
  primary had down. Losing the link while input is away does the same at once, and so does the nested window
  losing the host's keyboard focus.
- **The pointer while away:** nested, the primary hides the host's pointer and holds it (locked in place where the
  host supports pointer constraints, else confined to the window it is in, or the next of orrery's it enters) and
  sends its relative motion; on return it is let go and shown where it came in. Pointer crossing nested needs the
  host's relative-pointer protocol (most Wayland compositors have it; X11 reports raw motion): pushing past orrery's
  outer edge counts. Headless there is nothing to hold. The machine input went to draws the pointer itself while it
  has it: the client's cursor, or where a client names one, that cursor from the xcursor theme (else a plain square).
- **Motion merges, keys never do:** when a link stalls, pointer movement waiting for it merges into one step and is
  never replayed late, and so does touchpad scroll (its stop kept apart); keys, buttons and wheel steps all arrive,
  in order. The satellite acknowledges the input it ran,
  and the primary keeps at most 32 input messages unacknowledged: the rest wait on the primary, where they merge
  (`queued` in `state` shows them).
- Plain typing never shows in `last_key` on the primary while it is forwarded, and keystrokes are never logged.

**Trying it on one machine:** two nested orrerys side by side in the host, each with its own config and control
socket (a satellite named `side` whose `primary` is the primary's `listen`):

```sh
(umask 077; head -c 32 /dev/urandom | base64 > /tmp/orrery-secret)
{ cat examples/config.kdl; echo 'name "desk"'; echo 'listen "127.0.0.1:7340" secret-file="/tmp/orrery-secret"'; } \
    > /tmp/orrery-desk.kdl
{ cat examples/config.kdl; echo 'name "side"'; echo 'primary "127.0.0.1:7340" secret-file="/tmp/orrery-secret"'; } \
    > /tmp/orrery-side.kdl
./target/release/orrery run --config /tmp/orrery-desk.kdl --socket /tmp/orrery-desk.sock &
./target/release/orrery run --config /tmp/orrery-side.kdl --socket /tmp/orrery-side.sock &
./target/release/orrery msg --socket /tmp/orrery-desk.sock state    # session.input, the machines
```

Put the `desk` window against the right edge of a host monitor (the host stops its pointer there, so `desk` can hold
it; at a window edge with more host screen beside it, the pointer slips out before the lock takes hold) and `side`'s
anywhere visible. Open a terminal in each (`Super+Return`) and hand `desk` the keys. Both nested windows also take
the host's input directly when they have it, so drive everything from `desk`'s window. While input is on `side` it
draws the pointer itself: the cursor its client under it asked for, else the theme's (or a small light square).

- [ ] `Super+2` on `desk`: `state` shows `input` and the marker on `side`; typing lands in `side`'s terminal, in
      `desk`'s layout; the host's pointer is hidden and stays put.
- [ ] Hold `Super`, press `2`, release `2`, press `Return` (still holding Super): a terminal opens on `side`.
- [ ] `Super+Escape`: input is back on `desk`, the pointer shown where it was; nothing stays pressed on `side`.
- [ ] Push the pointer past `desk`'s right edge: input goes to `side` (it enters at its left edge, at the same
      height); push past `side`'s left edge: back on `desk`, the host's pointer at `desk`'s right edge.
- [ ] Holding a button while pushing past the edge: nothing crosses.
- [ ] With input on `side`, `Super+1`: back on `desk`.
- [ ] With input on `side`, quit `side` (`orrery msg --socket /tmp/orrery-side.sock quit`): input comes home at once.

## Layer-shell

Bars, docks, wallpapers, drop-downs and launchers come as layer surfaces (`zwlr_layer_shell_v1`), on a screen's four
layers: two under the slots (`background`, `bottom`) and two over them (`top`, `overlay`), drawn and hit in that
order (later ones in front on each layer). A layer surface lives on the screen it asks for, else the focused one,
placed by its anchors, margins and size as the protocol says; it never enters the stacks, tabs or workspaces. A
screen that goes closes its layer surfaces.

- **Exclusive zones** shrink the area the screen's layout divides: a bar 30 px high along the top moves every slot
  down by 30 px, and the windows are resized when a bar appears, changes or goes. A surface's zone counts from its
  first commit until it unmaps (a null buffer) or goes, so one that never draws keeps its room. The zone arithmetic
  is Smithay's, which differs from the protocol's letter in corner cases: a positive zone on a surface anchored to a
  corner, or to two opposite edges only, still takes room along one edge, and one anchored to all four edges would
  take the whole screen, which orrery ignores (the layout keeps the whole screen).
- **Keyboard:** an `exclusive` surface on the `top` or `overlay` layer takes it while mapped (the frontmost one, the
  focused screen's first) and gives it back to the focused window when it goes. An `on_demand` surface takes it on a
  click and keeps it until a window or slot is focused; `exclusive` on the lower layers does the same. `none` never
  takes it. Every bind still runs while a layer surface has the keyboard, as in other compositors (so the bind that
  opened a drop-down closes it again, and workspace keys work over it): a bind's keys, press and release, never
  reach the surface; every other key does.
- **Popups** (menus) open and are kept on screen as windows' are; those of `bottom` and `background` surfaces are
  drawn and hit over the windows (a bar's menu along the bottom), under the `top` and `overlay` layers.
- A buffer attached before the configure is acked (at the first commit, or the first after an unmap) is a protocol
  error: the client is disconnected.
- A launched program's layer surface uses its launch up (see [Programs orrery starts](#programs-orrery-starts)).

## A kitty desktop

`examples/kitty/` turns orrery and [kitty](https://sw.kovidgoyal.net/kitty/) into a small desktop, each part a
script or a kitty kitten driven by `orrery msg`:

- **A bar** along the top of every screen (`bars.py` starts `bar.py` in a `kitten panel` per screen, and keeps one
  per screen as screens come and go): left the workspaces (the one this screen shows highlighted, in a block when the
  screen has the focus; one shown on another screen in the badge colour; occupied ones plain, empty ones dim; one
  asking for attention in the attention colour; in a session, other machines' numbers dim), middle the tabs of the
  focused slot of this screen's workspace (the current one bright, titles cut to fit), right the session's machines
  (only in a session; where input is in the focus colour, one whose link is down dim) and the time. It follows
  `orrery msg subscribe`, takes its colours from orrery's (a reload recolours it) and reconnects when the stream
  ends. It belongs to the orrery whose control socket was there when it started: when that socket is gone or
  another orrery's has taken its place (one started at the same path after its quit, which runs its own bars), it
  stops at once; with no socket known, after a minute without orrery. The bars die with `bars.py`. Its panel
  reserves its line, so the slots start below it.
- **A drop-down terminal** on `Mod+grave` (`dropdown`): kitty's quick-access terminal, shown and hidden by the key;
  set it up in `~/.config/kitty/quick-access-terminal.conf`.
- **A launcher** on `Mod+D` (`launcher.sh`): fzf in a kitty panel over everything, over the desktop applications
  (desktop entries in `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, hidden ones left out) and the commands on `PATH`.
  Enter starts the choice through `orrery msg spawn`, so it opens as a new tab in the focused slot; Escape closes it.
  `launcher.sh --list` prints what it offers.
- **File chooser and light/dark** through the desktop-ui kitten (below).
- **`orrery.kdl`**, the snippet that brings it in: the two binds, the bars as `spawn-at-start` (each with
  `launch=#false`: none of them opens a window), and a
  `spawn-at-start "dbus-update-activation-environment" ...` so programs D-Bus starts (portals, the kitten) see
  orrery's `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE` and `ORRERY_SOCKET`.

**Needs** kitty 0.43 or later (`kitten panel`, `kitten quick-access-terminal`), python3 (the bar; standard library
only), fzf (the launcher) and, for the environment line, `dbus-update-activation-environment` (it comes with D-Bus)
with a session bus: run orrery as `dbus-run-session orrery run` when your login has none. On a systemd login, add
`"--systemd"` after the program name in that line so user units get the variables too.

**To use it**, add one line to your config (an absolute path, `~/...`, or relative to your config file):

```kdl
include "/path/to/orrery/examples/kitty/orrery.kdl"
```

The snippet's programs are written `./bars.py`, `./dropdown` and `./launcher.sh`, which orrery takes from the
snippet's own directory (see [Programs orrery starts](#programs-orrery-starts)), so nothing goes on `PATH` and the
scripts can stay where they are (or be copied together, snippet included, anywhere). Options for the bars' panels
go after `./bars.py` (`spawn-at-start "./bars.py" "-o" "font_size=10" launch=#false`), but a snippet line cannot
be changed from your config: to change the snippet, copy `examples/kitty/` and include your copy. Its binds clash
with yours on the same keys (an error naming both places); take such a bind out of your copy. `spawn-at-start`
runs at startup only, so a newly included snippet's bars come with the next start (or run `./bars.py` from a
terminal in orrery once). The scripts find orrery as `$ORRERY`, which orrery sets for the programs it starts, so
orrery need not be on `PATH` (run by hand outside orrery, they fall back to `orrery` on `PATH`); one that cannot
start `orrery msg` says so on its stderr, which is orrery's (see [Programs orrery starts](#programs-orrery-starts)).

### The file chooser and light/dark: the desktop-ui kitten

kitty's `desktop-ui` kitten is an xdg-desktop-portal backend: programs that ask the portal for a file chooser get
kitty's `choose-files` kitten in a panel, and the portal's light/dark setting is the kitten's. With
xdg-desktop-portal installed:

1. Inside orrery (so `XDG_CURRENT_DESKTOP` is `orrery`), run `kitten desktop-ui enable-portal`. It registers the
   kitten with D-Bus and writes the portal configuration for this desktop name. If the portal still picks other
   backends, put `examples/kitty/orrery-portals.conf` in `~/.config/xdg-desktop-portal/`: xdg-desktop-portal reads
   `<desktop>-portals.conf` for `XDG_CURRENT_DESKTOP=orrery` (see `man portals.conf`; with several desktop names,
   the first that has a file wins).
2. Start the kitten's server early, before other programs, from your config (after the environment line, which the
   snippet's include brings): `spawn-at-start "kitten" "desktop-ui" "run-server" launch=#false`.
3. Have toolkits use the portal: `GTK_USE_PORTAL=1` and `QT_QPA_PLATFORMTHEME=xdgdesktopportal` in the environment
   orrery starts with (your login's profile or `/etc/environment`), so every program it starts has them.
4. Start orrery again (a new session, so D-Bus and the portal start with the new environment).

Then `kitten desktop-ui set-color-scheme dark` (or `light`, `no-preference`, `toggle`) switches every program that
follows the portal's colour scheme; bind it if you like:
`"Mod+F10" spawn "kitten" "desktop-ui" "set-color-scheme" "toggle" launch=#false`. Its starting values (colour
scheme, accent colour, the chooser's size and kitty options) are in `~/.config/kitty/desktop-ui-portal.conf`.
`/usr/lib/xdg-desktop-portal -r -v` (the path varies by distribution) logs which backend the portal picks for what,
and why.

## GPU buffers (linux-dmabuf)

Bare metal and nested, orrery offers `zwp_linux_dmabuf_v1`: OpenGL and Vulkan clients render on the GPU and hand
orrery their buffers, which it draws without copying. It advertises the formats its renderer can import, with the
GPU's render node as the main device of the default feedback (bare metal the GPU orrery drives, nested the host's
GPU); with no render node to name it offers the formats alone (version 3). Every buffer a client creates is tried
in the renderer once; one the GPU cannot use is refused there, at creation. The log names the formats and the device
at start (`dmabuf: 212 format/modifier pairs, main device /dev/dri/renderD128`), and so does `orrery msg state`
(`dmabuf`). Mesa's OpenGL clients need that device to be a render node: with only a card node, or none, they keep
rendering in software and only Vulkan clients use the GPU (the log warns).
Headless has no renderer and offers none: its clients use shared memory, as clients without GPU drivers do anywhere.

Limits: one GPU (a client rendering on another GPU may have its buffers refused); client buffers are always drawn into
orrery's frame, never put on a monitor plane directly; implicit synchronization only (no explicit sync protocol).

## Limits

- No clipboard bridge to the host: copy and paste work between clients inside orrery, not across.
- Nested, the host's cursor stands for orrery's own pointer: orrery draws a cursor only for a pointer moved from
  another machine. Bare metal it always draws it (see Bare metal).
- No popup grabs: menus open and work, but a click elsewhere does not dismiss them by itself.
- Maximize and fullscreen requests are ignored: a window is always its slot's size.
- A layout round trip is not an identity: switching to a layout with fewer slots appends the extra slots' tabs to
  its last slot, and they stay there when switching back.
- With the host passing every key through, orrery takes its `Mod` (Super) keys before clients see them.
- Nested, one output per window, scale 1; no Xwayland, no touch input (bare metal: see its section).
- Nested, `open-screen` holds orrery until the host has configured the new window (milliseconds).
- Input crossing: a keyboard layout switched while input is on a satellite (a group toggle key, say) switches only
  there; the primary comes home on the layout it was on, and the next handoff sends that one.

## First nested session: checklist

To run by hand inside a Wayland compositor (copy the example to a scratch file so your real config is not
involved):

```sh
cargo build --release
cp examples/config.kdl /tmp/orrery-try.kdl
./target/release/orrery run --config /tmp/orrery-try.kdl
# in a second terminal: watch the state
./target/release/orrery msg state
```

- [ ] A window titled `orrery` opens; the top-left slot has a pink 1 px border; stdout shows
      `WAYLAND_DISPLAY=...`.
- [ ] Hand orrery the keys (a passthrough submap in Hyprland, see Default keys). Press `Super+H`:
      `orrery msg state` shows it under `last_key`.
- [ ] `Super+Return`: a kitty opens in the top-left slot. In it run `kitty &` twice: three kittys in one stack there,
      only the last visible; the slot shows the badge; `orrery msg state` lists one tab there holding three windows.
      (A window orrery did not launch goes by `stacking`; one that appears while a spawn under 5 s old still waits
      for its window opens as that spawn's tab instead, so wait for each `Super+Return` kitty first.)
- [ ] `Super+S` twice: the top two kittys leave the stack, each a tab of its own (three tabs). `Super+Tab` shows the
      next tab (typing goes to the visible one); `Super+Shift+Tab` goes back; holding `Super+Space` shows the tab
      strip, one segment per tab.
- [ ] `Super+Return` twice: each new kitty opens as a new tab right after the current one (five tabs), and has the
      focus.
- [ ] `Super+Shift+L`: the current tab moves to the hero (the big 2x2 slot) and the focus follows; it fills the hero.
- [ ] `Super+Ctrl+H` from the hero swaps everything in the hero with the top-left slot.
- [ ] `Super+Ctrl+2`: layout `halves`, every kitty resizes (the left half keeps slot 0's tabs, the right half gets
      every other slot's tabs); `Super+Ctrl+1`: back to `wall`, with the tabs now in slots 0 and 1 only (they stay
      there: the round trip is not an identity, by design).
- [ ] `Super+3`: an empty workspace 3 (`orrery msg state` shows `"workspace": 3` on the monitor); `Super+Return`
      opens a kitty there. `Super+1`: back to the kittys of workspace 1; `Super+3` shows the lone kitty again.
- [ ] `Super+Shift+1` from workspace 3: its kitty goes to workspace 1, and workspace 3 stays shown and focused, now
      empty. `Super+1`: the kitty is there, as a new tab in the focused slot; `orrery msg state` no longer lists
      workspace 3 under `workspaces` (left empty, it went).
- [ ] `Super+Ctrl+2` on workspace 2 (`Super+2` first), then `Super+1`: workspace 1 still shows `wall`.
- [ ] `Super+Shift+S`, then `kitty &` in the focused kitty: the new kitty opens as a new tab, not on top of the
      stack; `orrery msg state` shows `"stacking": false`. `Super+Shift+S` again and `kitty &` once more: it goes on
      top of the stack (as in the first step).
- [ ] Hold `Super`, click a window outside orrery, then click back into orrery: no stuck Super (typing in kitty is
      plain text).
- [ ] Move orrery to another workspace and, from a terminal outside it, run `orrery msg state` a few times: it
      answers at once (a hidden orrery does not stall).
- [ ] Break the config (e.g. `echo 'bogus 1' >> /tmp/orrery-try.kdl`): within a second a red bar runs along the
      top; `orrery msg state` shows `config_error` with the line; keys still work as before;
      `orrery check --config /tmp/orrery-try.kdl` names the problem.
- [ ] Fix it (delete the line): the red bar goes, `config_error` is null.
- [ ] `Super+Shift+Q` closes the focused kitty; `Super+Shift+E` (or closing its window) quits orrery and removes
      `$XDG_RUNTIME_DIR/orrery.sock`.
- [ ] Idle, `top` shows orrery near 0 % CPU.

## Several nested screens: checklist

Start as above (`./target/release/orrery run --config /tmp/orrery-try.kdl`, keys handed over), with a kitty on
workspace 1.

- [ ] From a terminal outside orrery, `./target/release/orrery run` again: it prints `ok` and exits 0; a second
      `orrery` window opens. `orrery msg state` lists monitors `winit` (workspace 1) and `winit-2` (workspace 2).
- [ ] Move the second window to another host monitor (or beside the first): both draw; idle CPU stays near 0 %.
- [ ] Click into the second window: `orrery msg state` shows `winit-2` focused; `Super+Return` opens a kitty
      there. The pointer works in both windows (hover and click a kitty in each: the right one takes focus).
- [ ] In the first window, `Super+2`: screen 2 gets the focus (`orrery msg state`), typing goes to its kitty.
      `Super+1` brings it back.
- [ ] From workspace 1, `Super+Shift+2`: the focused kitty moves to the second window, as a new tab there.
- [ ] Resize either window (or tile it on the host): its slots follow its size, the other window's do not change,
      and clicks still land on the window under the pointer.
- [ ] Hide one window (another host workspace): the other keeps drawing and `orrery msg state` answers at once.
- [ ] Close the second window: `orrery msg state` shows `winit` only, and workspace 2 listed on it, hidden, its
      kitty intact; `Super+2` shows it in the first window.
- [ ] `orrery run` again: the new window is `winit-2` again and takes workspace 2 back.
- [ ] Close the first window: `winit-2` remains, with workspace 1 hidden on it. Close that one too: orrery quits
      and `$XDG_RUNTIME_DIR/orrery.sock` is gone.

## Bare metal: checklist

On a machine whose own desktop session keeps running on its console. Copy the example config to a scratch file
first (`cp examples/config.kdl /tmp/orrery-try.kdl`), and have a Wayland terminal installed (kitty or foot).

- [ ] Switch to a free console: `doas chvt 3` from the desktop session (or `Ctrl+Alt+F3`), and log in.
- [ ] `./target/release/orrery run --config /tmp/orrery-try.kdl 2>/tmp/orrery.log` (auto picks bare metal): every
      monitor shows orrery's background and the focused slot's border; `/tmp/orrery.log` names the GPU and each
      screen (`DP-3: 2560x1440 at 59.95 Hz`, say). The pointer moves (the theme's arrow, or a small square) and stops
      at the screens' edges.
- [ ] `Super+Return`: a terminal opens in the top-left slot and takes typing; the pointer over it shows its cursor
      (a text beam over the text, say).
- [ ] With two monitors: move the pointer to the other one and click into it; `Super+2` and `Super+1` move the
      focus between screens; `Super+Shift+2` moves the focused terminal there.
- [ ] `Super+Ctrl+F<n>` (the desktop session's console, often 1 or 2; `Ctrl+Alt+F<n>` too): the desktop session is
      back and works normally (no stuck keys). Return to console 3 (`Ctrl+Alt+F3`, or `doas chvt 3`): orrery
      redraws everything, the terminal still takes typing, nothing is held down.
- [ ] If possible, unplug a monitor: its screen goes, its workspaces move to the remaining one (`Super+<n>` shows
      them), the pointer stays on the screens. Plug it back in: the screen comes back and takes its workspace again.
- [ ] From the terminal in orrery, `orrery msg state` lists the monitors by connector name.
- [ ] `Super+Shift+E`: orrery quits, the console comes back as it was; switch back to the desktop session.

## A kitty desktop: checklist

Nested (as in the first checklist) and then bare metal, with kitty 0.43 or later, python3 and fzf installed. Add the
snippet to the scratch config first:

```sh
echo "include \"$PWD/examples/kitty/orrery.kdl\"" >> /tmp/orrery-try.kdl
./target/release/orrery check --config /tmp/orrery-try.kdl      # ok
```

- [ ] Start orrery: a one-line bar runs along the top of the screen, in orrery's background colour; the slots start
      below it (`orrery msg state`: slot 0's `y` is the bar's height). It shows `1` highlighted and the time.
- [ ] `Super+Return` twice with `Super+S` between (two tabs): the bar's middle shows both titles, the current one
      bright; `Super+Tab` moves the brightness; `vim` in one changes its title there.
- [ ] `Super+3`: the bar shows `1` plain (occupied), `3` highlighted; `Super+1` back. A window asking for
      attention on a hidden workspace (in a kitty there, `sleep 3; printf '\a'`, then `Super+2` at once: its bell
      asks) turns its number to the attention colour.
- [ ] Narrow the window (nested): the titles shorten with `…`, the time stays; widen it: they come back.
- [ ] Change `colors { focus ... }` in the config: the bar recolours within a second.
- [ ] Open a second screen (`orrery run` again nested; a second monitor bare metal): it gets its own bar, showing
      its own workspace highlighted (without the block until it has the focus). Close it: its bar goes; `ps` shows
      one `kitten panel` per screen.
- [ ] `Super+grave`: the drop-down slides in and takes typing; `Super+grave` again hides it, focus back where it
      was.
- [ ] `Super+D`: the launcher appears centred over everything and takes typing; type a name, Enter: the program
      opens as a new tab in the focused slot; `Super+D` then Escape: it goes, nothing started.
- [ ] In a linked session (Linking machines): the bar's right shows the machines, the one with input in the focus
      colour; `Super+2` to the satellite moves it.
- [ ] Quit orrery: within a minute no `bars.py` or `bar.py` is left running (`pgrep -f bars.py`).
- [ ] desktop-ui (optional): after the steps above, a file dialog (e.g. a browser's Save As) is kitty's chooser,
      and `kitten desktop-ui set-color-scheme dark` turns a portal-following program dark.

## linux-dmabuf: checklist

Nested first (inside your desktop), then bare metal (the Bare metal checklist's console). Have `eglinfo`
(mesa-utils/mesa-progs), `vkcube` (vulkan-tools) and kitty at hand.

- [ ] Start orrery: its log says `dmabuf: <n> format/modifier pairs, main device /dev/dri/renderD<n>` (no warning);
      `orrery msg state` shows the same as `dmabuf`.
- [ ] In a terminal inside orrery, `eglinfo -B -p wayland`: the OpenGL renderer is your GPU (`Mesa Intel(R) ...`,
      say), not `llvmpipe`.
- [ ] `kitty` opens and types normally; scrolling a long `ls -lR /usr` stays smooth, and `top` (outside orrery
      nested, another console bare metal) shows kitty and orrery using little CPU.
- [ ] `vkcube --wsi wayland`: the cube spins smoothly in its slot.
- [ ] A browser: `chrome://gpu` (Chromium-based) or `about:support` (Firefox) reports hardware acceleration for
      compositing and WebGL; a video plays smoothly.
- [ ] Switch workspaces and back, move a GPU client's stack to another slot, resize by switching layouts: its
      content follows, never black or garbled.
- [ ] Bare metal: switch consoles away and back (`Ctrl+Alt+F<n>`) while `vkcube` runs: it keeps spinning after.
