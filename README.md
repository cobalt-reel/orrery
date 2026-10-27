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
Nested, each screen is one orrery window: `open-screen` (or a second `orrery run`) opens another, which the host
places and sizes like any window, and closing one takes its screen away. Headless (tests) orrery opens and closes
screens on request too (`open-screen`, `close-screen`).

For now orrery runs **nested**, as a window inside another Wayland compositor, or **headless** (no screen) for
tests. The design and the build order ahead are in `docs/plans/2026-10-26-orrery-design.md`.

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
`$XDG_RUNTIME_DIR/orrery.sock`), `--backend headless --size WxH` (no screen, for tests), and `--debug-inject` (a
testing aid, off by default: orrery then answers `inject` and `inject-remote`, below).

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
orrery msg --socket /path/to/orrery.sock state
orrery msg --to side state            # any request, run on that machine of the session (see Linking machines)
orrery msg inject key 38 down         # testing aid (--debug-inject only): input as if from this machine's devices
                                      #   (forwarded as theirs is): key XKB-KEYCODE down|up, motion DX DY (relative),
                                      #   button CODE down|up, axis H V
orrery msg inject-remote side release # testing aid (--debug-inject, primary): an input message sent to a satellite
                                      #   as it is: the same four, handoff [KEYCODE...], release; keymap in JSON
```

Errors come back as `{"ok":false,"error":"..."}` (also printed on stderr), e.g. an unknown layout or monitor.

`state` lists `monitors`, each as `{ name, number, workspace, layout, focused, slots }`: its screen number, the
workspace it shows, and that workspace's layout and slots; and `workspaces`, every existing one by number, as
`{ number, monitor, shown, layout, focused_slot, slots }` (`monitor` by name; `slots` as a monitor lists them, a
hidden workspace's never focused). Each slot is `{ index, name, rect, focused, tabs }`. Each slot's `tabs` lists its
tabs left to right, each as `{ current, attention, windows }` with `windows` (`id`, `app_id`, `title`) bottom to
top; `attention` is true when a window of that tab asked to be seen. Besides these, `state` carries
`stacking` (the running value), `config_error` (why the config file was not loaded, or null), `launches` (recent
spawns, oldest first, as `{ pid, token, age_ms, claimed }`: the activation token each was handed and whether a
window has taken it; a debug aid; one over 5 s old no longer counts and goes on the next spawn, new window or
activation), `frames` (how long drawing a frame takes on the compositor thread: `count`, `min_us`, `mean_us`,
`p95_us`, `max_us` over about the last minute; absent before the first frame and headless, which draws nothing) and
`last_key`: the last key press as the bind lookup read it (`key`, `mods`, and the `bind` it matched, if any). When a
bind does nothing, `last_key` tells whether the key reached orrery at all and with which modifiers. Plain typing (no
Ctrl, Alt or Super) is kept only as `(plain key)`, never which key it was. In a session, `state` also carries
`session` (see [Linking machines](#linking-machines)); alone it has none.

## The config, its reload and `orrery check`

orrery looks at its config file once a second (modification time and size) and reloads it when it changes, or at
once on `orrery msg reload`. A good file replaces binds, colours and layouts (`stacking` toggled at runtime is kept,
unless the file's own `stacking` value changed): every workspace, hidden ones too, takes the new definition of its
layout by name (a workspace whose layout is gone keeps it; that is logged, not an error). Monitor rules apply to new
outputs only, except after a start on the built-in config (the file was missing or broken): the first good file then
also applies its monitor rules, as a start with it would have. A broken file changes nothing: the old config stays,
the error is in `orrery msg state` as `config_error`, and a **4 px red bar** runs along the top of every output
until a good config loads. At startup a broken file means the built-in config, with the bar. A file deleted while
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
errors, as are duplicates. Every node is optional except at least one `layout`.

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
- `colors { focus "#rrggbb"; badge "#rrggbb"; attention "#rrggbb"; background "#rrggbb" }`: each also
  `#rrggbbaa`. `focus` is the 1 px border inside the focused slot, `badge` the badge on a slot holding more than
  one window and the tab strips, `attention` the badge of a slot holding a window that asked for attention,
  `background` what empty slots show. Defaults as in the example.
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
  | `spawn` | program, then its arguments | run it (no shell) with `WAYLAND_DISPLAY` set to orrery and no `DISPLAY` |
  | `close` | | ask the focused window to close |
  | `show-tabs` | | while held: the tab strips |
  | `quit` | | stop orrery |

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
  has it: the client's cursor, or a plain square where a client names one (orrery has no cursor theme).
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
draws the pointer itself: the cursor its client under it asked for, else a small light square in a dark outline.

- [ ] `Super+2` on `desk`: `state` shows `input` and the marker on `side`; typing lands in `side`'s terminal, in
      `desk`'s layout; the host's pointer is hidden and stays put.
- [ ] Hold `Super`, press `2`, release `2`, press `Return` (still holding Super): a terminal opens on `side`.
- [ ] `Super+Escape`: input is back on `desk`, the pointer shown where it was; nothing stays pressed on `side`.
- [ ] Push the pointer past `desk`'s right edge: input goes to `side` (it enters at its left edge, at the same
      height); push past `side`'s left edge: back on `desk`, the host's pointer at `desk`'s right edge.
- [ ] Holding a button while pushing past the edge: nothing crosses.
- [ ] With input on `side`, `Super+1`: back on `desk`.
- [ ] With input on `side`, quit `side` (`orrery msg --socket /tmp/orrery-side.sock quit`): input comes home at once.

## Limits

- No clipboard bridge to the host: copy and paste work between clients inside orrery, not across.
- The host's cursor stands for orrery's own pointer: orrery draws a cursor only for a pointer moved from another
  machine (the client's cursor surface, or a plain square for a named cursor: no cursor theme).
- No popup grabs: menus open and work, but a click elsewhere does not dismiss them by itself.
- Maximize and fullscreen requests are ignored: a window is always its slot's size.
- A layout round trip is not an identity: switching to a layout with fewer slots appends the extra slots' tabs to
  its last slot, and they stay there when switching back.
- With the host passing every key through, orrery takes its `Mod` (Super) keys before clients see them.
- Nested, one output per window, scale 1; no layer-shell, no Xwayland, no touch input.
- Nested, `open-screen` holds orrery until the host has configured the new window (milliseconds).

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
