# Design notes

`2026-10-26-orrery-design.md` is the design as a whole. After it, every milestone has a dated design (`*-design.md`),
a plan, and an "As built" section added once it was done; the later, smaller changes keep all three in one file.
They are records of how orrery was built, in the order it was built, and are not kept up to date: the
[manual](../manual.md) says what orrery does today.

Where they say "the README", they mean what is now the [manual](../manual.md) and the
[checklists](../checklists.md).

| | |
|---|---|
| [orrery: design](2026-10-26-orrery-design.md) | the session, slots, stacks and tabs, the link, input crossing, the build order |
| stacks and tabs: [design](2026-10-27-stacks-and-tabs-design.md), [plan](2026-10-27-stacks-and-tabs.md) | slots that hold tabs, tabs that are stacks, where a new window opens |
| workspaces and screens: [design](2026-10-27-workspaces-and-screens-design.md), [plan](2026-10-27-workspaces-and-screens.md) | ten workspaces, several screens, a second `run` joining the first |
| the link: [design](2026-10-27-link-design.md), [plan](2026-10-27-link.md) | a primary and satellites, Noise over TCP, one space of workspace numbers |
| input crossing: [design](2026-10-27-input-crossing-design.md), [plan](2026-10-27-input-crossing.md) | one keyboard and pointer across the session's machines |
| bare metal: [design](2026-10-29-bare-metal-design.md), [plan](2026-10-29-bare-metal.md) | DRM/KMS, GBM, libinput, a seat |
| a kitty desktop: [design](2026-10-29-kitty-desktop-design.md), [plan](2026-10-29-kitty-desktop.md) | layer-shell, the event stream, `include`, `spawn-at-start`, the bar and its friends |
| linux-dmabuf: [design](2026-11-13-dmabuf-design.md), [plan](2026-11-13-dmabuf.md) | GPU buffers from OpenGL and Vulkan clients |
| [popup grabs](2026-11-13-popup-grabs.md) | menus that close on a click outside |
| [focus-follows-mouse](2026-11-13-focus-follows-mouse.md) | the slot under the pointer takes the focus |
| [layouts](2026-11-13-layouts.md) | a library of layouts, `cycle-layout`, the layout in the bar, a picker |
| [fullscreen](2026-11-14-fullscreen.md) | a window over its whole screen |
