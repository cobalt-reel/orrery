# Fullscreen (design, plan and as built)

Until now maximize and fullscreen requests were answered and ignored: a window was always its slot's size. A video
or a game wants the whole screen, bar included. This adds fullscreen as a property of a window, set by a bind, by
`orrery msg` or by the client (`xdg_toplevel.set_fullscreen`).

## Design

- **State in the core.** `Stacks` keeps the set of fullscreen windows. A placed one is always the top of its
  workspace's focused slot: `settle` (which every changing method ends with) drops the flag of any that is not. So a
  workspace has at most one, and every way of leaving (a window opening on top, another tab, the focus to another
  slot, a move) needs no code of its own: it is the one rule. A window in no slot yet (waiting for its first
  buffer) keeps the wish; `close` drops it.
- **The whole screen.** A monitor's `area` is what its layout divides (the screen less the exclusive zones). The
  compositor now also tells the stacks the whole screen (`set_screen`; the area until then). A fullscreen window's
  `Placement::rect` is that screen, and the other windows of its workspace are `covered`. `arrange`, the popup
  constraint, hit-testing and the scene all read placements, so they follow without knowing why.
- **Other screens and workspaces are untouched.** The rule is per workspace: the focus may be on another screen, and
  another workspace may be shown on this one; the flag stays and applies again when its workspace is shown.
- **Requests.** `toggle-fullscreen` (action, `orrery msg`, default bind `Mod+F`) flips the focused window. A client's
  `set_fullscreen` is granted to the focused slot's top and to a window not placed yet, refused otherwise; the
  output it names is ignored (fullscreen is the window's own screen). Each request is answered with a configure,
  never before the initial commit. `unset_fullscreen` clears it. Unmapping (a null buffer) closes the window in the
  stacks, which drops the flag, as the protocol's reset of an unmapped toplevel asks.
- **Configure.** Fullscreen windows get the screen's size and the `fullscreen` state (still tiled on every edge);
  ending it sends the slot's size without the state, so the client always knows. A waiting window that asked is
  configured to the focused screen, fullscreen, so its first buffer is the right size.
- **Drawing.** `scene` returns the window alone at the whole screen, a black backdrop behind it (it may be smaller
  or see-through), no decorations, the red bar of a broken config as ever. `output_elements` draws, front to back:
  pointer, red bar, the layer surfaces `over_fullscreen` names, the window, the backdrop. Bottom and background
  layers are skipped. Covered windows are unmapped from the space, so they get no frame callbacks.
- **Layer surfaces.** Over a fullscreen window: every `overlay` one, and a `top` one only when its keyboard
  interactivity is `exclusive` (a launcher must be seen while it holds the keys). A bar (`top`, no keyboard) is
  covered. The same rule decides hit-testing (`layer_under`) and whether a clicked `on_demand` surface keeps the
  keyboard, so what is not drawn is neither hit nor typed into.
- **Pointer.** `slot_at` gives the whole screen to the fullscreen window's slot and `top_at` returns that window
  with the screen as its rectangle: clicks and `focus-follows-mouse` over it (the bar's strip too) keep the focus
  where it is, so fullscreen does not end by hovering.
- **State.** `WindowView.fullscreen` (left out when false, so existing readers see no change).

Not done: direct scanout of the fullscreen buffer on a plane; maximize (still ignored); a fullscreen spanning
screens.

## Plan and tests

1. `Stacks`: the flag, `set_screen`, `Placement::{fullscreen, covered}`, the `settle` rule. Unit tests for each way
   in and out; the invariants and the random walk learn the rule (two new operations).
2. `toggle-fullscreen`: action, request and its wire twin, parser, default bind, example config.
3. Compositor: configure, `arrange`, the xdg-shell requests, hit-testing, layer rule, scene and elements; unit tests
   for `scene`, `slot_at`/`top_at` and `over_fullscreen`.
4. `tests/fullscreen.rs` with a real client: size and state on toggle and by request, before the first commit,
   refused when covered, the pointer over the bar's strip, what shows over it, workspaces, the ways it ends.
5. README: the section, the keys, the action, the limits, the checklist.

## As built

As designed. One thing found on the way: a move to another workspace lands as the top of that workspace's focused
slot, so the window stays fullscreen there by the rule; kept, and documented.
