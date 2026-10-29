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

## As built

Commits, by task: 1 `729a2f9`, `7a32c67`; 2 `2c98a0c`, `ee2d4cc`; 3 this one (design and plan: `d9e95a2`). 407 tests
(308 unit, 99 integration, most of them headless end to end), one more ignored (slow: every prefix of the example
config); fmt and clippy clean, release build clean. The DRM backend itself is not reachable from the tests: its pure
parts are (`src/comp/backend_drm/plan.rs`: connector changes, CRTC assignment, preferred mode, the screen under a
point, keeping the pointer on the screens, repaint delay, flip watchdog, late flip events, the refused-frame
recovery).

Deviations from the design and this plan:
- One `DrmCompositor` per screen with no planes (`FrameFlags::empty()`, no cursor plane): everything is composited by
  GL; no direct scanout. Swapchains are 8 bits per channel (ARGB, else ABGR).
- Rendering is split: `render::output_elements` builds a screen's elements, `render_output` draws them (nested) and
  bare metal hands them to its `DrmCompositor`, whose own damage tracking decides whether a frame is queued.
- Pacing: a screen draws only when its last flip is done; draws are tried by the 16 ms tick, once after input, and
  60% of a frame after each flip. A check finding nothing to draw, or failing, sends the frame callbacks itself. A
  flip whose event does not come within 1 s is given up on; each frame carries a sequence number, and a flip event
  stamped before the frame waited for was queued is ignored.
- A frame the kernel refuses (atomic test failed) resets every output, at most once per resume, hotplug or frame
  drawn; a screen refused again after its reset is closed as if unplugged and stays dark until plugged in again.
- Resume takes the GPU back with every output reset (`activate(true)`), then rescans the connectors.
- Relative motion goes through `device_motion` (so `crossing::step` both clamps the pointer and crosses in a
  session); a screen unplugged under the pointer brings it back to the nearest screen. Absolute motion maps onto the
  screen the pointer is on.
- The theme cursor (`render::CursorDraw::Named`) is used wherever orrery draws the pointer, nested remote input too;
  `Orrery::draws_pointer` and `pointer_drawn` decide where it is drawn (hidden on bare metal while input is away).
- `open-screen` and `close-screen` (and a second `orrery run`, which joins) answer that monitors decide on bare
  metal. Screens close the gap after an unplug: `lay_side_by_side` moved from the nested backend to `Orrery`.

Known limits:
- One GPU; no Xwayland; no hardware cursor; no linux-dmabuf (OpenGL clients render in software, Vulkan clients fail);
  scale 1, preferred mode only; no layer-shell, lock or idle.
- libinput defaults (no tap-to-click, no natural scrolling); keyboard LEDs do not light; touch and tablet tools are
  ignored.
- Make and model of monitors are not read (no EDID parsing): outputs say `Unknown` and the connector name.

Live test: pending.
