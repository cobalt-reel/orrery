# orrery

orrery is a Wayland compositor and static tiling window manager written in Rust on
[Smithay](https://github.com/Smithay/smithay). Each monitor shows a fixed **layout** of **slots** (rectangles of a
weighted grid); every slot holds **tabs**, and each tab is a **stack** of windows. A slot shows its current tab,
and of that tab only the top window is drawn. A new window opens on top of the focused slot's current stack (or,
with stacking toggled off, as a new tab there). Windows never move by themselves: you move stacks between slots,
split a window off into a tab of its own, cycle tabs and switch layouts, with keys from a KDL config or with
`orrery msg` over a control socket.

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
`$XDG_RUNTIME_DIR/orrery.sock`; a second orrery on the same socket refuses to start), and `--backend headless
--size WxH` (no screen, for tests).

orrery presents a frame only when the host asks for one, and everything else runs on its own timer, so a hidden
orrery (on another workspace, say) keeps answering `orrery msg` and serving its clients; it just stops drawing
until shown again.

### A Hyprland window rule (suggestion)

To give orrery's window its own workspace, fullscreen it, and keep it drawing while it is not visible, something
like this in a Hyprland config (adapt to your Hyprland version's rule syntax):

```ini
windowrule = workspace 3, match:class ^(orrery)$
windowrule = fullscreen on, match:class ^(orrery)$
windowrule = render_unfocused on, match:class ^(orrery)$
```

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
| `Super+Shift+S` | toggle stacking: a new window goes on top of the focused stack (on) or opens as a new tab (off) |
| `Super+1` / `Super+2` | layout `wall` / `halves` |
| `Super+Return` | spawn `kitty` |
| `Super+Shift+Q` | close the focused window (asks it to close) |
| `Super+Space` (held) | show tab strips: one segment per tab along each multi-tab slot's top, the top tab bright |
| `Super+Shift+E` | quit orrery |

A slot holding more than one window (in any tab, at any depth) shows a small badge in its top-right corner. A
click focuses the slot under the pointer.

## `orrery msg`

One request to the running orrery; the answer is JSON on stdout (pretty for `state`), exit 0 only when it says
ok.

```sh
orrery msg state                      # monitors, layouts, slots (rects, focus), their tabs and each tab's windows
orrery msg focus-slot right           # left | right | up | down
orrery msg move right                 # the current tab, its whole stack, to the neighbour
orrery msg swap-slot down
orrery msg cycle-tab next             # next | prev
orrery msg split                      # the top window becomes a tab of its own
orrery msg toggle-stacking
orrery msg spawn kitty --single-instance   # every word after spawn is the argv (so --socket goes before spawn)
orrery msg layout halves              # optionally a monitor: orrery msg layout halves winit
orrery msg close
orrery msg reload                     # re-read the config now (it is also watched)
orrery msg quit
orrery msg '{"cmd":"layout","name":"wall"}'   # a single argument is taken as a JSON request
orrery msg --socket /path/to/orrery.sock state
```

Errors come back as `{"ok":false,"error":"..."}` (also printed on stderr), e.g. an unknown layout or monitor.

Each slot's `tabs` lists its tabs left to right, each as `{ current, attention, windows }` with `windows` (`id`,
`app_id`, `title`) bottom to top. Besides the monitors, `state` carries `stacking` (the running value),
`config_error` (why the config file was not loaded, or null), `frames`
(how long drawing a frame takes on the compositor thread: `count`, `min_us`, `mean_us`, `p95_us`, `max_us` over
about the last minute; absent before the first frame and headless, which draws nothing) and `last_key`: the last
key press as the bind lookup read it (`key`, `mods`, and the `bind` it matched, if any). When a bind does
nothing, `last_key` tells whether the key reached orrery at all and with which modifiers. Plain typing (no Ctrl,
Alt or Super) is kept only as `(plain key)`, never which key it was.

## The config, its reload and `orrery check`

orrery looks at its config file once a second (modification time and size) and reloads it when it changes, or
at once on `orrery msg reload`. A good file replaces binds, colours and layouts (`stacking` toggled at runtime is
kept, unless the file's own `stacking` value changed): each monitor takes the new definition of its current
layout by name (a monitor whose layout is gone keeps showing it; that is logged, not
an error). Monitor rules apply to new outputs only, except after a start on the built-in config (the file was
missing or broken): the first good file then also applies its monitor rules, as a start with it would have. A
broken file changes nothing: the old config stays, the error is in `orrery msg state` as `config_error`, and a
**4 px red bar** runs along the top of every output until a good config loads. At startup a broken file means
the built-in config, with the bar. A file deleted while running keeps the running config.

```sh
orrery check                          # the default path; or: orrery check --config PATH
```

prints `ok` (exit 0), or every problem as `FILE:LINE:COL: message` on stderr (exit 1). A missing file is exit 1
too (orrery would use the built-in config).

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
- `monitor "OUTPUT" layout="NAME"`: the layout an output starts with. The nested output is `winit`, the headless
  one `headless`; an output without a rule gets the first layout.
- `stacking #true|#false`: where a new window opens, on top of the focused slot's current stack (`#true`, the
  default) or as a new tab there; `toggle-stacking` flips it while running.
- `colors { focus "#rrggbb"; badge "#rrggbb"; attention "#rrggbb"; background "#rrggbb" }`: each also
  `#rrggbbaa`. `focus` is the 1 px border inside the focused slot, `badge` the badge on a slot holding more than
  one window and the tab strips, `attention` the badge of a slot holding a window that asked for attention,
  `background` what empty slots show. Defaults as in the example.
- `binds { "COMBO" ACTION [ARGS...] }`: `COMBO` is modifiers and one key joined by `+`, e.g. `"Mod+Shift+H"`:
  modifiers `Mod`, `Alt`, `Ctrl`/`Control`, `Shift`, `Super`/`Logo`; the key is an xkb keysym name, any case
  (`H`, `Return`, `Tab`, `Space`, `1`, `F5`); the `+` key is written `plus` (`"Mod+plus"`). Write the unshifted key
  with `Shift`, not the shifted symbol. Caps and Num Lock are ignored. Actions:

  | Action | Arguments | Does |
  |--------|-----------|------|
  | `focus-slot` | `"left"`/`"right"`/`"up"`/`"down"` | focus the neighbouring slot (nothing at the edge) |
  | `move` | a direction | move the focused slot's current tab, its whole stack, to the neighbour as a new tab |
  | `swap-slot` | a direction | swap everything (all tabs) of the focused slot and its neighbour |
  | `cycle-tab` | `"next"`/`"prev"` | show the focused slot's next / previous tab |
  | `split` | | the top window leaves its stack for a new tab right after it |
  | `toggle-stacking` | | flip `stacking` |
  | `layout` | a layout name | switch the focused monitor's layout (fewer slots: the extra slots' tabs join the last) |
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

stacking #true            // a new window goes on top of the focused stack; #false: a new tab there

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
    "Mod+1" layout "wall"
    "Mod+2" layout "halves"
    "Mod+Return" spawn "kitty"
    "Mod+Shift+Q" close           // as Hyprland's killactive
    "Mod+Space" show-tabs        // held: tab strips visible while down
    "Mod+Shift+E" quit
}
```

## Limits

- No clipboard bridge to the host: copy and paste work between clients inside orrery, not across.
- The host's cursor only: orrery draws no cursor of its own and ignores cursor images clients set.
- No popup grabs: menus open and work, but a click elsewhere does not dismiss them by itself.
- Maximize and fullscreen requests are ignored: a window is always its slot's size.
- A layout round trip is not an identity: switching to a layout with fewer slots appends the extra slots' tabs to
  its last slot, and they stay there when switching back.
- With the host passing every key through, orrery takes its `Mod` (Super) keys before clients see them.
- One output (the window), scale 1; no layer-shell, no Xwayland.

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
- [ ] `Super+Return` three times: three kittys in one stack in the top-left slot, only the last visible; the slot
      shows the badge; `orrery msg state` lists one tab there holding three windows.
- [ ] `Super+S` twice: the top two kittys leave the stack, each a tab of its own (three tabs). `Super+Tab` shows
      the next tab (typing goes to the visible one); `Super+Shift+Tab` goes back; holding `Super+Space` shows the
      tab strip.
- [ ] `Super+Shift+L`: the current tab moves to the hero (the big 2x2 slot) and the focus follows; it fills the
      hero.
- [ ] `Super+Ctrl+H` from the hero swaps everything in the hero with the top-left slot.
- [ ] `Super+2`: layout `halves`, every kitty resizes (the left half keeps slot 0's tabs, the right half gets
      every other slot's tabs); `Super+1`: back to `wall`, with the tabs now in slots 0 and 1 only (they stay
      there: the round trip is not an identity, by design).
- [ ] `Super+Shift+S`, then `Super+Return`: the new kitty opens as a new tab, not on top of the stack;
      `orrery msg state` shows `"stacking": false`.
- [ ] Hold `Super`, click a window outside orrery, then click back into orrery: no stuck Super (typing in kitty is
      plain text).
- [ ] Move orrery to another workspace and, from a terminal outside it, run `orrery msg state` a few times: it
      answers at once (a hidden orrery does not stall).
- [ ] Break the config (e.g. `echo 'bogus 1' >> /tmp/orrery-try.kdl`): within a second a red bar runs along the
      top; `orrery msg state` shows `config_error` with the line; keys still work as before;
      `orrery check --config /tmp/orrery-try.kdl` names the problem.
- [ ] Fix it (delete the line): the red bar goes, `config_error` is null.
- [ ] `Super+Shift+Q` closes the focused kitty; `Super+Shift+E` (or closing the window) quits orrery and removes
      `$XDG_RUNTIME_DIR/orrery.sock`.
- [ ] Idle, `top` shows orrery near 0 % CPU.
