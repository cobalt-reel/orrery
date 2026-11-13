# orrery milestone 8: linux-dmabuf (design)

Until now orrery offered clients only shared memory (`wl_shm`). An OpenGL client then cannot hand its GPU-rendered
buffers to orrery, so Mesa falls back to software rendering (llvmpipe) for every GL client, and Vulkan clients copy
each frame through shared memory: slow, and CPU (power, heat) that a desktop should not spend. Milestone 8 offers
`zwp_linux_dmabuf_v1` so clients render on the GPU and orrery draws their buffers directly. Decided 2026-11-13,
taking the defaults below.

## What orrery advertises

- **Who:** the backends that draw with a GPU: bare metal (the GPU orrery drives) and nested (the host connection's
  GPU, through the shared renderer). Headless has no renderer and advertises nothing; its clients keep to shared
  memory, which keeps the test suite free of GPU needs.
- **Formats:** what the renderer can import as textures (`ImportDma::dmabuf_formats`), all of them. No filtering:
  the renderer's list is what it can draw.
- **Feedback (version 5):** a default feedback whose main device is the GPU's render node: bare metal derived from
  the card node orrery opened (the card node itself if it has no render node); nested from the EGL device of the
  host display (`EGLDevice::device_for_display` → render node). One tranche, the main device's formats. No
  per-surface feedback and no scanout tranche: orrery never puts client buffers on a plane (below).
- **No main device known** (nested on an EGL without a device, say): a version 3 global with the formats only.
  Mesa's OpenGL clients need a render node (they derive it from the main device, and fall back to software without
  one), so with a card node only or a version 3 global, only Vulkan clients gain; the log warns then.
- **No formats** (a renderer without dmabuf import): no global at all, and a log line saying GL clients render in
  software.

## Import

Every buffer a client creates (`create`/`create_immed`) is imported into the renderer once, in
`DmabufHandler::dmabuf_imported`; a failure refuses the buffer then (`failed`), so a client learns at creation that
a buffer will not work instead of showing garbage later. The renderer lives in the backend (behind its own
`Rc<RefCell>`), so the backend hands `Orrery` a small importer at init, like the existing `Screens` and `Consoles`
hooks. Client requests are dispatched outside any borrow of the backend; should that ever change, the importer lets
the buffer through untried (it is imported when first drawn) rather than panic.

Drawing needs no change: commits already go through Smithay's `on_commit_buffer_handler`, and the render elements
import a surface's buffer by its kind (shared memory or dmabuf) through the renderer.

## Visible

- Log at start: `dmabuf: <n> format/modifier pairs, main device <path>` (or why there is none).
- `orrery msg state` gains `dmabuf: { formats, device }`, absent when nothing is advertised (headless).

## Not in this milestone

- Direct scanout of client buffers (a fullscreen video on a plane, saving the composition): needs per-surface
  feedback with scanout tranches and the DRM compositor's direct scanout; later.
- Explicit synchronization (`wp_linux_drm_syncobj_v1`); implicit sync is what Mesa uses by default.
- Several GPUs: a client rendering on another GPU may have its buffers refused.
- `wl_drm` (the legacy Mesa protocol): current Mesa does not need it.

## Testing

- Unit: what to advertise from (formats, render node known) — nothing / formats only / feedback.
- Headless end to end: no `zwp_linux_dmabuf_v1` global, no `dmabuf` in `state`.
- The state view round-trips with a `dmabuf` value.
- GPU, by hand (no monitor needed): orrery nested inside a headless compositor with a GL renderer; inside it,
  `eglinfo -p wayland` names the GPU (not llvmpipe), a dmabuf test client and a Vulkan client run and draw correctly,
  `state` shows `dmabuf`. Then the user's live test (README checklist), nested and bare metal.
