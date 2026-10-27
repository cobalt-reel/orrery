//! orrery: a static tiling Wayland compositor. The pure core (layout, config, stacks, placement, session, ipc, link)
//! holds every decision; the compositor layer (`comp`) only translates Wayland events into core operations.

pub mod comp;
pub mod config;
pub mod ipc;
pub mod layout;
pub mod link;
pub mod placement;
pub mod session;
pub mod stacks;

/// The program's outward name: the command, the control socket's file (`$XDG_RUNTIME_DIR/<NAME>.sock`), the
/// config directory (`$XDG_CONFIG_HOME/<NAME>/`), the window title and app id, and the name in its messages.
/// Repackaging under another name is this one line.
pub const NAME: &str = "orrery";
