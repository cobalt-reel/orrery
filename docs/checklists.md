# Live-test checklists

One checklist per milestone, to run by hand: the things the automated tests cannot see (a real GPU, real monitors,
real keys). Everything they refer to is described in the [manual](manual.md).

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
- [ ] In a linked orrery (Linking machines): the bar's right shows the machines, the one with input in the focus
      colour; `Super+2` to the planet moves it.
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

## Fullscreen: checklist

- [ ] `Super+F` on a terminal: it covers the whole screen, bar included, with no border; `Super+F` again: back in
      its slot, the bar back, the other windows as they were.
- [ ] A video in the browser, its fullscreen button (or `F11`): the whole screen; Escape (or the button) gives the
      tiles back. `mpv --fs FILE` opens fullscreen; `f` in mpv leaves and enters it.
- [ ] With a window fullscreen on one screen, the pointer and focus go to the other screen and back: it stays
      fullscreen, and moving the pointer across it (top edge too, where the bar is) changes nothing.
- [ ] `Super+D` over a fullscreen window: the launcher shows over it and takes the keys; starting a program from it
      ends the fullscreen (the new window is on top or in another slot).
- [ ] `Super+2` then `Super+1` (workspaces): the other workspace is tiled with its bar; back, the window is
      fullscreen still.
- [ ] `Super+H`/`J`/`K`/`L` to another slot of the same workspace: the tiles are back.

## Layouts: checklist

With the kitty desktop and `examples/layouts.kdl` included (and a `monitor` rule per screen, if you like).

- [ ] `orrery check` says `ok`; `orrery msg state` lists `layouts` in config order.
- [ ] Each screen's bar shows its layout's name before the clock; it changes with every switch below, on that
      screen only.
- [ ] `Super+]` steps through every layout in config order and wraps round to the first; `Super+[` goes back. The
      windows follow into the new slots (slot 0's tabs stay in slot 0, the main slot where a layout has one).
- [ ] On the other screen (focus it first), the same: only that screen's workspace changes.
- [ ] `Super+\`: the picker appears centred, starting on the layout in use, and draws each layout beside the list in
      the screen's proportions as you move through it (arrows, or type a name). Enter: that layout, on the screen
      that was focused. `Super+\` then Escape: nothing changes.
- [ ] `orrery msg cycle-layout next DP-1` (an output name from `state`) switches that screen even when it is not
      focused.
