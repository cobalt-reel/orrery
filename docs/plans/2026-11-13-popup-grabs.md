# Popup grabs (design, plan and as built)

orrery ignored `xdg_popup.grab`, so menus opened on a click stayed open until the client closed them itself: a
click on another window or an empty slot left a browser's context menu hanging. Decided 2026-11-13 with the
defaults below (the README listed it under Limits).

## Design

- `XdgShellHandler::grab` grabs through Smithay's `PopupManager::grab_popup` with the popup chain's root (the
  toplevel or layer surface it belongs to), then sets the keyboard focus to the grabbed popup with a
  `PopupKeyboardGrab` and the pointer with a `PopupPointerGrab` (focus kept). A grab another grab does not follow
  from (by its serial, or the previous popup's) is refused: the chain is ungrabbed and the menu closes.
- The pointer grab closes the chain on a press whose target is another client (or nothing: an empty slot); presses
  inside the menu's client go to it, which closes its own menus as it sees fit.
- orrery's own focus rule (`update_keyboard_focus`) keeps a grab while the grab's root is still the surface to
  focus, and otherwise closes its menus (`PopupManager::dismiss_popup` on the root) before moving the focus: another
  slot, workspace or layer surface ends the menu instead of leaving the keyboard on a hidden popup. While the grab
  holds, the rule no longer re-asserts the focus every frame.
- orrery's binds still run first (the keyboard filter comes before the grab), and the click that closes a menu does
  not also focus the slot under it (the slot click-to-focus runs only without a pointer grab): a second click does.

## Plan

1. The handler and the focus rule (`src/comp/handlers.rs`, `src/comp/mod.rs`).
2. Test client: the serial of the latest button press; `create_grabbed_popup` grabs before the popup's first commit
   (a mapped popup cannot be grabbed).
3. `tests/popups.rs` end to end with `--debug-inject`: a click on the window, a grabbed menu, then a click on an
   empty slot closes it; and a workspace switch closes it.
4. README (a Menus section; the limit is gone), full suite, clippy, fmt.

## As built

As planned, one commit. Both tests fail without the change (no `popup_done` within the wait) and pass with it;
473 tests, clippy and fmt clean. Not covered by tests: nested and bare-metal behaviour with real clients (a
browser's context menu, a GTK menu bar), left to the live test.
