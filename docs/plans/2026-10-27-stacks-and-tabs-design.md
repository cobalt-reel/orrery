# orrery milestone 2: stacks and tabs (design)

Fills in section 2 of `2026-10-26-orrery-design.md` with the decisions it leaves open. Agreed 2026-10-27.

## Model

- A slot holds a list of **tabs** and the index of its **current** tab; a tab is one **stack** of windows, bottom
  to top. An empty slot has no tabs. Only the top window of the current tab is drawn and reachable; every other
  window is unmapped but stays configured to the slot's size.
- The core (`stacks.rs`) stays pure and keeps its invariant: every window is in exactly one place, and an
  operation that cannot apply does nothing.
- A new tab goes **right after the current tab** and becomes current (as hy3 does). A tab whose last window
  closes goes away; the slot shows the tab that took its place (the next one, else the previous).

## Operations

| Operation | What it does |
|-----------|--------------|
| `cycle-tab next/prev` | next/previous tab of the focused slot, wrapping; nothing with one tab |
| `move <dir>` (`Super+Shift+HJKL`) | the focused slot's current tab, the whole stack, goes to the neighbour as a new tab, current and focused |
| `swap-slot <dir>` | swaps everything (all tabs, current index) of two slots; focus follows |
| `split` (`Super+S`) | the top window leaves its stack and becomes a new tab right after it, current; nothing for a one-window stack |
| `toggle-stacking` (`Super+Shift+S`) | flips stacking; config `stacking #true\|#false`, default on |
| layout switch to fewer slots | the extra slots' tabs, in order, are appended as tabs of the last slot |
| monitor removed | its tabs become tabs of the focused slot (pending without a monitor) |

`move-tab` is gone (renamed `move`). `orrery msg spawn <argv...>` is new (launches like a bind's `spawn`).

## Where a new window goes (decided at its first buffer, as today)

1. **Has a parent** (`xdg_toplevel.set_parent`) that is placed: on top of the parent's stack. The parent's tab
   is not made current if it is in the background; the window waits there.
2. **A launch's window:** orrery records each launch (bind `spawn` or `msg spawn`): pid, an xdg-activation token
   (passed as `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID`), the target (monitor name and slot focused at the
   press), and the time. A window matches a launch when its client's pid is the launched pid or a descendant
   (parent chain read from `/proc`), else when an unclaimed launch is under 5 s old (oldest first). It opens as a
   new tab in the target slot (the focused slot if the target is gone), current, and that slot is focused. Each
   launch claims at most one window; launches older than 5 s are forgotten (pid matching ends with them).
3. **Anything else:** with stacking on, on top of the focused slot's current stack; off, a new tab there. An empty
   focused slot gets it as its first tab either way.

xdg-activation (new): a placed window that activates with an unclaimed launch's token moves to that launch's
target as a new tab (the launch is then claimed). Any other activation request:
- the window is the top of its slot's current tab: that slot is focused;
- otherwise it is never raised: it is marked **attention**, and its slot shows the badge in the new
  `attention` colour. The mark clears when the window becomes the top of its slot's current tab.
- A mark is also dropped when its window closes or loses its monitor (goes pending); it does not come back.

`stacking` toggled at runtime survives a reload unless the config's own `stacking` value changed.

## Drawing

- The corner badge shows when a slot holds more than one window (any tab or depth), in `attention` colour when a
  window of the slot is marked.
- The held `show-tabs` strip has one segment per tab, the current one bright, each with small pips for its depth
  when the stack has more than one window, and attention-coloured when its stack holds a marked window.

## `state`

`slots[].tabs` becomes a list of tabs: `{ current, attention, windows: [{ id, app_id, title }] }`, windows bottom to
top. Scripts reading the old shape (one `tabs` entry per window) break; orrery has no users yet.

## Testing

Pure unit tests for every operation and placement rule (launch matching takes the pid chain as data). Headless
end-to-end tests through `msg` and the test client: dialogs with parents, `msg spawn` with the 5 s fallback and the
pid match (the test binary re-run as a helper client), activation tokens and attention, `split`, `move`,
`toggle-stacking`, layout switches. Tests never touch real user paths.
