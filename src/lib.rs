//! orrery: a static tiling Wayland compositor. The pure core (layout, config, stacks, ipc) holds every
//! decision; the compositor layer (`comp`) only translates Wayland events into core operations.

pub mod comp;
pub mod config;
pub mod ipc;
pub mod layout;
pub mod stacks;
