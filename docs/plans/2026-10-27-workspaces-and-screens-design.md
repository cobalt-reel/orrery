# orrery milestone 3: workspaces and several screens on one machine (design)

Fills in section 1 of `2026-10-26-orrery-design.md` for one machine (no link yet). Agreed 2026-10-27.

## Screens

- Nested, **each orrery window is one screen** (a monitor). The host window manager places and sizes it; orrery
  never tries to match host monitors. The screen follows its window's size; closing a window removes that screen
  (its workspaces go to a remaining screen, as on any monitor removal, below); closing the last one quits.
- **`orrery run` with an orrery already on the control socket** asks it to open one more window (request
  `open-screen`) and exits 0, instead of refusing to start. `orrery msg open-screen` does the same.
- Screens keep their number while they live (i3-like): a new screen takes the lowest free number, so closing
  screen 1 of 3 leaves 2 and 3, and the next screen opened is 1 again. Output names follow the number: screen 1
  is `winit` (so existing `monitor "winit"` rules keep working), screen k is `winit-<k>`. Headless (tests) does
  the same with its own names and the `--size` of the first screen.
- Screens sit side by side in orrery's global space in opening order (x offsets), not in number order: a screen 1
  reopened after 2 and 3 sits right of them. This only matters for Smithay's bookkeeping: nested, the pointer
  moves between screens through the host.

## Workspaces

- **10 workspaces**, numbered 1..10 (key `0` is 10), i3's rules:
  - Workspace k is **locked** to the screen numbered k while that screen exists, and exists meanwhile.
  - Each screen shows one workspace. Switching to a workspace (`workspace N`) shows it on its screen and focuses
    that screen.
  - Switching to a workspace that does not exist creates it **on the focused screen**, where it stays tied.
  - A workspace that is not locked, not shown and holds no windows goes away.
  - Each workspace has its own layout, slots, tabs and focused slot; it starts from its screen's default layout
    (the `monitor` rule, else the first layout). A layout switch changes only the shown workspace.
- **When screens come and go:** a new screen numbered k takes workspace k (moving it there intact if it existed
  elsewhere, shown; the screen it left shows its lowest remaining workspace, at worst its own locked one); a
  removed screen's workspaces move intact (tabs, layouts, focused slot) to the focused remaining screen (the first
  remaining one when the focused screen is the one removed), hidden: that screen keeps showing what it shows. No
  other screen's content ever moves. With no screen left, windows wait for one, as today.
- **Reload:** a layout redefined by name changes every workspace using it, hidden ones too, and every screen's
  default layout.
- **`move-to-workspace N`** carries the focused stack to workspace N (created on the focused screen if needed) as
  a new tab after the current one in that workspace's focused slot. Focus stays where it was (i3's default).
- Windows only ever live in workspaces; hidden workspaces' windows are unmapped but stay configured to their
  slot's size.

## Keys and commands

| Keys | Action |
|------|--------|
| `Super+1..0` | `workspace 1..10` |
| `Super+Shift+1..0` | `move-to-workspace 1..10` |
| `Super+Ctrl+1..9` | layouts (were `Super+1/2`) |

`orrery msg workspace N`, `move-to-workspace N`, `open-screen`. `state`: `monitors[]` gain `workspace` (the shown
one); a new top-level `workspaces: [{ number, monitor, shown, layout, slots }]` carries every existing workspace
(slots as today). `monitors[].slots` stays the shown workspace's slots.

## Testing

Pure unit tests for the workspace rules (locking, creation, disposal, screens joining and leaving, moving stacks,
a random walk keeping the one-place invariant). Headless end-to-end through `msg`: `open-screen`, switching,
moving a window to a workspace on the other screen, closing a screen. The nested multi-window backend is checked
by hand (two windows, resize, close one), as nothing on a real host is reachable from the tests.
