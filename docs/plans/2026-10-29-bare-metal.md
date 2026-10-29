# Bare metal Implementation Plan

**Goal:** orrery runs directly on DRM/KMS + libinput (spec: `docs/plans/2026-10-29-bare-metal-design.md`).

**Architecture:** a new backend `src/comp/backend_drm/` on Smithay's udev + libseat session + DRM/GBM
(`DrmCompositor` per connector) + libinput, plugging into the existing `Screens` hook, render path and input path;
pure helpers for everything decidable without hardware.

**Tech stack:** Rust 2024, Smithay 0.7 features `backend_drm`, `backend_gbm`, `backend_libinput`,
`backend_session_libseat`, `backend_udev` (system libs: libseat, libinput, libudev, libgbm, libdrm); the `xcursor`
crate for themes.

**Ground rules for every task**
- TDD for everything pure; the DRM paths cannot run in tests and must keep the headless suite green.
- Match the surrounding code: terse explanatory doc comments (≤120 columns), same naming.
- Before committing: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` (full suite) and
  `cargo build --release`.
- orrery stays clean: no references to any other project, person, machine or personal setup.
- Never run the DRM backend from a test or on the live desktop; the user tests it by hand on a spare console.

---

### Task 1: the pure parts

- Backend choice (`auto` → nested if `WAYLAND_DISPLAY`/`DISPLAY`, else drm; explicit flags win), the `keyboard`
  config block (parse, defaults, errors, reload applies it to the seat on every backend), `vt N` action (1–12) with
  default binds `Mod+Ctrl+F1..F12` and `Ctrl+Alt+F1..F12` in `examples/config.kdl` (a no-op with a log line where the
  backend has no consoles), xcursor loading with the fallback chain (theme → default → square), screen naming from
  connectors. Unit tests; README for the config additions.

Commit `bare metal: the pure parts`.

### Task 2: the DRM backend

- Session (libseat) + udev: primary GPU, `DrmDevice` + `GbmDevice` + `GlesRenderer`, one `DrmCompositor` per
  connected connector (preferred mode, a free CRTC), outputs named by connector, added through `add_output` with the
  milestone 3 numbering; vblank-paced rendering through the existing `render_output` + `Decorations`; hotplug (udev
  change events → rescan connectors, add/remove screens); `open-screen`/`close-screen` answer that screens follow
  the hardware here.
- libinput through the session into `process_input_event`; pointer clamped to the screens; device add/remove.
- Session pause/resume: pause stops rendering and input and releases keys/buttons; resume re-activates the DRM
  device, resets the compositors, redraws all.
- `vt N` switches consoles through the session.
- Cursor: the client's surface or the xcursor image as a render element on every screen (hardware cursor planes are
  later).
- `--backend drm|winit|headless|auto`, README: a "Bare metal" section and a manual checklist (switch to a free
  console, log in, `orrery run`, open a terminal, move between screens, `Mod+Ctrl+F<n>` back to the host session and
  return, unplug/replug a monitor if available, `Mod+Shift+E`).

Commit `bare metal: DRM, GBM and libinput`.

### Task 3: as built

Full suite, clippy, fmt, release build, cleanliness scan, README vs design, "As built"; then the user's live test.
