# orrery: design

orrery is a static tiling Wayland compositor whose session spans every screen on every machine it is given. Each
monitor shows a fixed layout of slots; every slot is a tab stack; windows move only when you move them. What the
first version built (static weighted layouts, slots, tab stacks, the KDL config with reload and `check`, the
control socket and `orrery msg`, the nested and headless backends) is the starting point.

Two decisions frame everything:

- **Nested first.** Running inside an existing compositor (Hyprland, GNOME, KDE, any Wayland desktop),
  fullscreen, is the main way orrery runs. Running on bare metal is a bonus that comes last.
- **For others, not just one setup.** Nothing assumes a particular network, machine or desktop: people bring
  their own.

## 1. The session: machines, screens, workspaces

- **One orrery per machine** owns all of that machine's screens: native monitors, or, when nested, one host
  window per host monitor. A second `orrery run` on the same machine adds a screen to the running orrery instead of
  starting another compositor (as `kitty --single-instance` does). Only one compositor per machine can let windows
  move freely between its screens.
- **A session** is one **primary** orrery plus **satellites**: orrerys on other machines whose config names the
  primary. Every screen of every machine is a **monitor** in one ordered list: the primary's config gives the
  order, and monitors it does not list are added as they join.
- **10 workspaces across the whole session**, numbered like i3's:
  - With N monitors, workspaces 1..N are locked to monitors 1..N.
  - Switching to a workspace moves focus to its monitor, whichever machine that is on. This is also how the
    keyboard and mouse cross to another machine (section 4).
  - Switching to an unused workspace creates it on the monitor you switched from, and it stays tied there
    (workspace 7 made from 4 lives on monitor 4; from 1, `Super+7` jumps to monitor 4 showing 7).
  - Each workspace has its own static layout and slots, starting from its monitor's default layout.
  - A window can move to any workspace on its own machine. A move to a workspace on another machine is refused
    with a message: windows never cross machines.
  - `Super+1..0` are workspace keys, as in Hyprland; switching layouts moves to other keys.

## 2. Slots: stacks and tabs

- **A slot holds one or more stacks; a stack holds one or more windows.** Several stacks in a slot are its tabs.
  The default is one stack and no tabs. Nothing floats.
- **A stack is not switchable.** Only its top window is reachable; the window beneath shows again when the top
  one closes. A stack whose last window closes goes away, and the slot shows its next tab.
- **Tabs switch between stacks.** `cycle-tab` goes to the next or previous stack of the slot (nothing with one
  stack). The held `show-tabs` key shows one strip segment per stack, with its depth.
- **Where a new window goes:**
  - **Opened by orrery** (a bind's `spawn`, e.g. `Super+Return` kitty, a browser on another key): a **new tab**, a
    new stack in the slot that was focused when the key was pressed. orrery recognises the window by the launched
    process's ID or by the xdg-activation token it hands each launch. Fallback, for single-instance programs like
    browsers whose window comes from the already-running process: the first new window within about 5 s of a
    launch counts as that launch's window (a window that appears on its own in those 5 s may take the tab;
    accepted).
  - **A dialog with a parent** (a browser's save-as): **stacked on its parent's stack**, wherever that is.
  - **Anything else** (a program run from a terminal, a tab dragged out of a browser): **stacked** on the focused
    stack.
- **Moving takes the whole stack.** `move` (`Super+Shift+H/J/K/L`) carries the focused stack, every window in it,
  to the neighbouring slot, where it arrives as a new tab, shown and focused (a program moves and the terminal that
  started it follows behind it). Moving to another workspace works the same way, within the machine. `swap-slot`
  swaps everything in two slots. A layout switch with fewer slots turns the extra slots' stacks into tabs of the
  last slot.
- **`split`** takes the top window off its stack and makes it its own stack, a new tab beside it. Folding a tab
  back onto another stack is left out until it is missed.
- **Attention:** a buried window asking for attention stays buried; the badge marks it.
- **`toggle-stacking`** (and `stacking #true|#false` in the config, default on): off, every new window opens as a
  new tab, except dialogs, which still stack on their parent. For people and programs that want tabs only.

## 3. The link

- A satellite's config names the primary: `primary "host:port" secret-file="..."`. The primary listens only when
  its config enables it. Screens on the same machine need no link.
- **Encrypted and authenticated** with a Noise handshake on the pre-shared secret (the WireGuard approach): no
  certificates, nothing to set up beyond the secret file. The file must not be readable by others; the secret is
  never logged.
- **What travels:** from the satellite, a hello (machine name, its screens and sizes, protocol version) and then
  its workspaces' state as it changes; from the primary, the session's workspace table and the commands
  `orrery msg` gives (which makes `orrery msg --to <machine>` possible). Input travels as section 4 says.
- **Failures:** a version mismatch is refused at the hello with a clear message; a wrong secret just fails the
  handshake, with nothing sent in clear. When the link drops, the satellite keeps working as a normal window
  manager for its own windows, its workspaces drop out of the session, focus that was there snaps back to where
  the keyboard is, and it keeps reconnecting.

## 4. Input crossing

orrery is the compositor on both ends, so it needs no desktop portal, no uinput and no special permission: the
sending orrery already owns the keyboard and pointer, and the receiving one feeds its own windows.

- **Focus decides.** The session always knows which monitor has focus. Each machine handles its own physical
  keyboard and mouse: with focus local, input works as always; with focus on another machine, it goes over the link
  and the receiver runs it through the same path as its own input, so binds and stacks work there. A bind that
  changes the session (`Super+1` to a workspace elsewhere) goes to the primary, which moves focus.
- **Keyboard:** raw keycodes travel, and at handoff the sender's keymap goes along once, so the user's layout
  follows them. Handoff is explicit: the machine losing focus releases every key it had down; the one gaining
  focus is told which modifiers are held. No stuck keys, no missing Super. Key repeat happens on the receiver;
  nothing repeats over the network.
- **Pointer:** monitor positions in the primary's config say which screen edge leads where. Crossing an edge to
  another machine: the sender hides its cursor and holds the pointer (nested: the host's pointer lock; native: it
  just stops moving its cursor) and sends relative motion, buttons and high-resolution scroll; the receiver moves
  its own cursor, and hitting an edge that leads back hands focus back. Motion is coalesced: if the link stalls,
  pending movement merges into one step and is never replayed late. Keys and buttons are never merged or dropped.
- **Safety:** an escape key, handled locally and never forwarded, always pulls input back to the machine the user
  is at (a hung satellite cannot trap it). Keystrokes are encrypted in the link and never logged (the rule
  `last_key` already follows).
- **Latency:** version 1 sends everything over the one link, with motion coalesced, and `state` shows a latency
  readout from the start. If real networks show TCP hurting, motion moves to an encrypted UDP path that tolerates
  loss; keys and buttons stay on the reliable link.
- **Later:** clipboard sharing.

## 5. Build order

Each milestone ships something usable and gets its own plan, built task by task with reviews.

1. **Extract** (this repository): the window manager as a standalone project, crate `orrery-wm`, command
   `orrery`, config under `~/.config/orrery/`.
2. **Stacks and tabs** (section 2): the biggest daily win.
3. **Workspaces and several screens on one machine:** 10 workspaces with the i3 rules; a second `orrery run` adds a
   screen. Risk: Smithay's winit backend drives a single host window, so orrery needs its own small nested backend
   that opens several.
4. **The link and satellites** (section 3).
5. **Input crossing** (section 4).
6. **Later:** bare metal (DRM/KMS, libinput), clipboard sharing, the UDP motion path if the latency readout calls for
   it, layer-shell (bars, launchers), lock and idle.

## 6. Testing

Most tests on the pure core (workspaces and their i3 rules, stacks and tabs, where new windows go, the link's
messages); headless orrerys driven through `msg`, including two linked over loopback with input injected into one
and checked at a test window on the other (handoff mid-keypress, held modifiers, link drop with keys down, the
escape key, the cursor crossing back). Tests never touch real user paths.
