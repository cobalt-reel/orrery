# orrery milestone 6: bare metal (design)

The first item of section 5's "later" list: orrery on the hardware itself (DRM/KMS, GBM, libinput) instead of inside
another compositor. Agreed 2026-10-29. Clipboard sharing, layer-shell, lock and idle, and a UDP motion path stay later.

## Starting

- `orrery run` picks the backend: nested when `WAYLAND_DISPLAY` or `DISPLAY` is set, bare metal otherwise (a text
  console); `--backend drm` forces bare metal, `--backend winit` nested, `--backend headless` stays for tests.
- Device access goes through libseat (logind/elogind or seatd): orrery never runs as root and never opens a device
  itself. No seat → a clear error.
- Not in this milestone: Xwayland, and being a login session (it is started by hand from a console).

## Screens

- Every connected connector becomes a screen at its preferred mode, one CRTC each; screens are numbered as
  milestone 3 says (lowest free number, kept while connected). Output names are the connector names (`DP-3`,
  `HDMI-A-1`, `eDP-1`), so `monitor "DP-3" …` rules apply as they always have.
- Hotplug adds or removes screens with milestone 3's rules (a removed screen's workspaces move intact).
- Frames are paced by vblank (page-flip events): a screen renders when something changed and its last flip is done;
  idle costs nothing. The headless and nested backends are unchanged.
- The primary GPU is the boot VGA / first render node; one GPU only in this milestone.

## Input

- libinput through the seat: keyboards, pointers, touchpads, into the same input path as the other backends (binds,
  crossing, inject).
- Keyboard layout: the `XKB_DEFAULT_*` environment, or a `keyboard { layout "..." variant "..." options "..."
  model "..." repeat-delay 600 repeat-rate 25 }` block in the config (the block wins; reload applies it).
- Pointer motion is clamped to the screens; screens sit side by side in opening order, as nested.

## Cursor and consoles

- orrery draws the cursor: the client's cursor surface, else the named cursor from the xcursor theme
  (`XCURSOR_THEME`/`XCURSOR_SIZE`, default theme and 24), else milestone 5's fallback square.
- Console switching: bind action `vt N`; defaults `Mod+Ctrl+F1..F12` plus the usual `Ctrl+Alt+F1..F12` (handled as
  binds, so they work with any modifier setup). Leaving the console pauses rendering and input and releases every
  key and button; coming back resets the outputs and redraws everything.
- `Mod+Shift+E` quits as always; a crash gives the console back (the seat restores it).

## Testing

Pure parts unit-tested (backend choice, connector naming, the keyboard block, `vt` binds, xcursor fallback). The
headless suite stays the main suite. The DRM backend is checked by hand on a spare console of a machine whose host
session keeps running on its own console.
