<h1 align="center">orrery</h1>

<p align="center">
  <b>A static tiling Wayland compositor.</b><br>
  Fixed layouts of slots, tabs in every slot, a stack in every tab.<br>
  Windows never move by themselves.
</p>

<p align="center">
  <a href="docs/manual.md">Manual</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="examples/config.kdl">Example config</a> ·
  <a href="docs/plans/">Design notes</a>
</p>

<p align="center">
  <img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Rust 2024" src="https://img.shields.io/badge/rust-2024-orange">
  <img alt="Built on Smithay" src="https://img.shields.io/badge/built%20on-Smithay-8a2be2">
</p>

---

orrery is a Wayland compositor and tiling window manager written in Rust on
[Smithay](https://github.com/Smithay/smithay).

Most tiling compositors are *dynamic*: open or close a window and the others are rearranged around it. orrery is
*static*. You describe each layout once, as a grid of **slots**, and a slot stays exactly where you put it. What
changes is what a slot holds:

```
┌────────────────────────────────────────────────────────────────────────┐
│ 1  2  3        editor │ docs │ shell                   tall    21:07   │  a bar (any layer-shell client)
├───────────────────────────────────────────┬────────────────────────────┤
│                                           │ slot 1                     │
│  slot 0 "main"                            │   one tab, one window      │
│                                           │                            │
│    three tabs:  editor · docs · shell     ├────────────────────────────┤
│                                           │ slot 2                     │
│    the editor tab is a stack:             │   two tabs                 │
│      a file dialog       (on top, drawn)  │                            │
│      the editor window   (beneath)        ├────────────────────────────┤
│                                           │ slot 3                     │
│                                           │   empty                    │
└───────────────────────────────────────────┴────────────────────────────┘
        a screen shows one workspace; a workspace has one layout
```

- A **screen** shows one **workspace**; there are ten, and workspace *k* is locked to screen *k*.
- A workspace has one **layout**: a weighted grid whose rectangles are the **slots**.
- A slot holds **tabs**; it shows its current tab.
- A tab is a **stack** of windows; only the top one is drawn. Close it and the one beneath shows again.

A new window goes where you would expect and nowhere else: a dialog lands on top of its parent, a program you
launched opens as a new tab in the slot that had the focus *when you launched it* (even if the focus has moved on
by the time its window appears), and everything else joins the focused slot. From there you move things, with keys
or from a script.

## What is in it

- **Layouts you write yourself**, in a [KDL](https://kdl.dev) config: grids with weighted columns and rows, slots
  spanning cells, a starting layout per monitor. A [library of eight](examples/layouts.kdl) is ready to include,
  and `cycle-layout` steps through them.
- **A config that cannot lock you out.** The file is watched and reloads within a second. A broken file changes
  nothing: the old config keeps running, a red bar along the top says so, and `orrery check` names every problem
  as `FILE:LINE:COL`.
- **Scriptable.** `orrery msg` runs the actions the keys run, over a control socket: focus, move, split, layouts,
  workspaces, spawn. `orrery msg state` is the whole desktop as JSON; `orrery msg subscribe` is an event stream,
  one JSON line per change, made for bars.
- **Three ways to run:** on **bare metal** (DRM/KMS, libinput, a seat from logind, elogind or seatd), **nested**
  as a window inside another Wayland compositor, and **headless** for tests.
- **Several monitors**, each a screen with its own workspace and layout, coming and going as they are plugged in.
- **One session across machines.** orrerys on several machines join over an encrypted link (Noise over TCP):
  workspace numbers span all of them, and one keyboard and pointer cross from machine to machine, by workspace key
  or by pushing the pointer past the edge of a screen.
- **A desktop made of kitty.** [`examples/kitty/`](examples/kitty) adds a bar on every screen, a drop-down
  terminal, an application launcher and a layout picker that draws each layout, all as small scripts driven by
  `orrery msg`. One `include` line brings it in.
- **The protocols a desktop needs:** xdg-shell with popups and menu grabs, fullscreen, layer-shell (bars,
  launchers, wallpapers), xdg-activation (a window asking for attention is marked, never raised),
  xdg-decoration, linux-dmabuf (OpenGL and Vulkan clients render on the GPU), and copy and paste between clients.
- **Tested without a screen:** about 500 tests, most of them end to end: a headless orrery driven through real
  Wayland client connections and its control socket.

## Quick start

### Build

You need a current stable Rust toolchain and the development files for `libseat`, `libinput`, `libudev`,
`libgbm`, `libdrm` and `libxkbcommon`; at run time, Mesa (or another EGL and GLES implementation) and, to run
nested, the Wayland client libraries.

```sh
git clone https://github.com/cobalt-reel/orrery
cd orrery
cargo build --release
```

The binary is `target/release/orrery`; put it on your `PATH`, or `cargo install --path .`.

### Try it nested

From a terminal inside the Wayland desktop you already use:

```sh
orrery run
```

orrery opens as a window. `Super+Return` starts a terminal in it (kitty, unless your config says otherwise),
`Super+H/J/K/L` moves the focus between slots, and `Super+Shift+E` quits. If your desktop keeps the Super key for
itself, hand the keys over (the [manual](docs/manual.md#default-keys) shows how in Hyprland) or pick another
modifier with `mod "Alt"` in the config.

### Run it on bare metal

Log in on a text console and run the same command:

```sh
orrery run 2>~/orrery.log
```

It needs a seat (logind, elogind or seatd) and a GPU with KMS; it needs no privileges of its own. Every connected
monitor becomes a screen. See [Bare metal](docs/manual.md#bare-metal) for the details and the limits.

### Configure

orrery reads `~/.config/orrery/config.kdl`. Without one it uses the built-in config, which is
[`examples/config.kdl`](examples/config.kdl) word for word: copy that file and change it.

```kdl
layout "tall" {
    grid cols=2 rows=3
    widths 3 2
    slot "main" col=0 row=0 rows=3
    slot col=1 row=0
    slot col=1 row=1
    slot col=1 row=2
}

monitor "DP-1" layout="tall"      // each monitor starts on a layout of its own

binds {
    "Mod+Return" spawn "kitty"
    "Mod+L" focus-slot "right"
    "Mod+Shift+L" move "right"   // the current tab, its whole stack, to the slot on the right
    "Mod+Tab" cycle-tab "next"
    "Mod+F" toggle-fullscreen
}
```

```sh
orrery check        # ok, or every problem as FILE:LINE:COL
```

## The keys you will use most

`Mod` is Super unless the config says otherwise. The [full table](docs/manual.md#default-keys) is in the manual.

| Keys | |
|------|---|
| `Mod+Return` | start a terminal, as a new tab in the focused slot |
| `Mod+H` `J` `K` `L` | focus the slot left, down, up, right |
| `Mod+Shift+H` `J` `K` `L` | move the current tab to that slot |
| `Mod+Ctrl+H` `J` `K` `L` | swap everything with that slot |
| `Mod+Tab`, `Mod+Shift+Tab` | next and previous tab of the focused slot |
| `Mod+S` | split: the top window leaves its stack for a tab of its own |
| `Mod+F` | fullscreen, and back |
| `Mod+1` … `Mod+0` | show workspace 1 … 10 |
| `Mod+Shift+1` … `Mod+Shift+0` | move the current tab to that workspace |
| `Mod+]`, `Mod+[` | next and previous layout |
| `Mod+Space` (held) | show the tab strips |
| `Mod+Shift+Q` | close the focused window |
| `Mod+Shift+E` | quit |

## Scripting

```sh
orrery msg state                  # monitors, workspaces, layouts, slots, tabs and windows, as JSON
orrery msg subscribe              # the same, then one JSON line per change
orrery msg workspace 3
orrery msg spawn kitty --single-instance
orrery msg layout tall DP-1
orrery msg cycle-tab next
```

```json
{"event":"focus","screen":"DP-1","workspace":1,"slot":0,"slot_name":"main","window":{"id":7,"app_id":"kitty","title":"~"}}
{"event":"title","id":7,"app_id":"kitty","title":"vim"}
```

Programs orrery starts are handed its control socket (`ORRERY_SOCKET`) and its own path (`ORRERY`), so a script
talks to the orrery that started it. See [`orrery msg`](docs/manual.md#orrery-msg) and
[the event stream](docs/manual.md#the-event-stream).

## A kitty desktop

orrery draws windows, a focus border and little else; the rest of a desktop comes from layer-shell clients. The
one in [`examples/kitty/`](examples/kitty) is built from [kitty](https://sw.kovidgoyal.net/kitty/)'s panels:

- a **bar** on every screen: workspaces, the focused slot's tabs, the layout, the time, in orrery's own colours;
- a **drop-down terminal** on `Mod+grave`;
- a **launcher** on `Mod+D`: fzf over your desktop applications and the commands on `PATH`;
- a **layout picker** on `Mod+backslash`: fzf over the layouts, each drawn to the screen's proportions.

```kdl
include "/path/to/orrery/examples/kitty/orrery.kdl"
include "/path/to/orrery/examples/layouts.kdl"
```

It needs kitty 0.43 or later, python3 and fzf. Details are under
[A kitty desktop](docs/manual.md#a-kitty-desktop). Any other layer-shell bar, launcher or wallpaper works too.

## One session across machines

Give two machines the same secret and name one the primary:

```kdl
// on the desk
name "desk"
listen "0.0.0.0:7340" secret-file="~/.config/orrery/secret"

// on the machine beside it
name "side"
primary "desk.lan:7340" secret-file="~/.config/orrery/secret"
```

Now the desk's screens are workspaces 1, 2, … and the other machine's follow on. `Mod+3` on the desk's keyboard
shows workspace 3 wherever it lives, and the keyboard and pointer go there with it; so does pushing the pointer
off the edge of the last screen. Windows never cross machines; input does, encrypted and authenticated with the
secret. See [Linking machines](docs/manual.md#linking-machines) and
[Input crossing](docs/manual.md#input-crossing).

## Status

orrery is young: version 0.1. It runs a desktop on bare metal, and these are the things it does not do yet:

- no Xwayland: Wayland clients only;
- no screen capture protocol, so no screenshots or screen sharing;
- no screen lock and no idle handling;
- scale 1 only, each monitor at its preferred mode, one GPU;
- no hardware cursor planes and no direct scanout: everything is composited;
- touchscreens and tablets are ignored; touchpads keep libinput's defaults.

The manual lists the [limits](docs/manual.md#limits) in full.

## Documentation

| | |
|---|---|
| [docs/manual.md](docs/manual.md) | everything orrery does: running it, the keys, `orrery msg`, the config reference, linking machines, layer-shell, fullscreen, linux-dmabuf |
| [docs/checklists.md](docs/checklists.md) | by-hand test lists for what needs a real screen and real keys |
| [docs/plans/](docs/plans) | the design, and a dated design, plan and as-built note for every milestone |
| [examples/](examples) | the built-in config, the layout library and the kitty desktop |

## Working on it

```sh
cargo test                     # unit tests and headless end-to-end tests; no screen needed
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

The tests run orrery with `--backend headless` in temporary directories with temporary configs; they never read
your `~/.config/orrery` or touch your control socket.

## License

[MIT](LICENSE).
