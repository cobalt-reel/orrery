# focus-follows-mouse (design, plan and as built)

Focus on mouse-over, not only on a click. A
config option, off by default so the built-in behaviour and the tests stay as they were.

## Design

- `focus-follows-mouse #true|#false` (top level, once; default `#false`), read and reloaded like `stacking`.
- On pointer motion (`Orrery::pointer_to`: bare metal, nested, injected and remote motion alike), when the option is on
  and no grab holds the pointer: the slot under the pointer, if it is not already the focused slot of the focused
  screen, is focused as a click would (`Stacks::focus_at`, layer keyboard focus cleared, arranged).
- Not over a layer surface above the slots (a bar never takes the focus by hovering): the same `over_slots` test as the
  click. Not under a grab: an open menu (popup grab) would otherwise be closed by the focus move, and a drag keeps its
  target.
- Only motion triggers it (re-layouts re-evaluate the pointer through `update_pointer_focus`, which does not), so a
  focus moved by keys or `orrery msg` stays until the pointer moves, as in i3/sway.

## Plan and tests

1. Config option + parse tests (read, default off, bad values, twice).
2. `input::focused_slot` (pure, unit-tested) and `Orrery::hover_focus` from `pointer_to`.
3. `tests/hover.rs` end to end with `--debug-inject`: moving focuses slot after slot; off by default nothing moves; with
   a grabbed menu the focus stays and the menu stays open.
4. README (the config section), a commented line in `examples/config.kdl`.

## As built

As planned. Mutation checks: with the hook disabled the focusing test fails; without the grab check the menu test fails.
