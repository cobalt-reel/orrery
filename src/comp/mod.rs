//! The compositor: Smithay state, the event loop, and `arrange`, the one place where the core's placements become
//! Wayland state. Every decision about where a window goes is made by the core (`placement` for a new one, then
//! `Stacks`); this layer gathers the facts they need and maps, unmaps and configures what `placements()` says.

mod backend_drm;
mod backend_headless;
mod backend_winit;
pub mod cursor_theme;
mod dmabuf;
mod forward;
mod frames;
mod handlers;
mod input;
pub mod ipc_server;
mod layers;
pub mod net;
mod remote;
mod render;
mod spawn;
mod subscribers;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use smithay::desktop::{PopupManager, Space, Window, find_popup_root_surface};
use smithay::input::pointer::{CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle, Resource as _};
use smithay::utils::{IsAlive, Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::{CompositorClientState, CompositorState, get_parent, with_states};
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState, XdgToplevelSurfaceData};
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::xdg_activation::{XdgActivationState, XdgActivationToken, XdgActivationTokenData};

use crate::NAME;
use crate::config::{self, Config};
use crate::ipc::{LaunchView, Request, Response, StateView};
use crate::layout::{Layout, Rect};
use crate::placement::{self, Landing, Launches};
use crate::stacks::{Stacks, WinId, Workspace};

/// What orrery runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The hardware itself: DRM/KMS, GBM and libinput through the seat (from a text console).
    Drm,
    /// A window inside the running Wayland session.
    Winit,
    /// A virtual output with no rendering and no input, for tests.
    Headless,
}

/// `--backend`: a backend, or `auto` to pick one from the environment (`BackendChoice::backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BackendChoice {
    /// Nested inside a running session (`WAYLAND_DISPLAY` or `DISPLAY` set), else bare metal.
    Auto,
    /// Bare metal, from a text console.
    Drm,
    /// A window inside the running session.
    Winit,
    /// No screen and no input devices (tests).
    Headless,
}

impl BackendChoice {
    /// The backend, given `WAYLAND_DISPLAY` and `DISPLAY` (empty counts as unset): an explicit choice as it is;
    /// `auto` nested when either is set, else bare metal.
    #[must_use]
    pub fn backend(self, wayland_display: Option<&OsStr>, display: Option<&OsStr>) -> Backend {
        let set = |v: Option<&OsStr>| v.is_some_and(|v| !v.is_empty());
        match self {
            Self::Auto if set(wayland_display) || set(display) => Backend::Winit,
            Self::Auto | Self::Drm => Backend::Drm,
            Self::Winit => Backend::Winit,
            Self::Headless => Backend::Headless,
        }
    }
}

/// A DRM connector's name as the kernel (and so every other compositor) gives it: its kind's name, from the
/// `DRM_MODE_CONNECTOR_*` number, and its id among the connectors of that kind (`DP-3`, `HDMI-A-1`, `eDP-1`). The
/// one source of output names on bare metal: the DRM backend gives it `u32::from(interface)` and `interface_id`.
#[must_use]
pub fn connector_name(kind: u32, id: u32) -> String {
    const KINDS: [&str; 21] = [
        "Unknown",
        "VGA",
        "DVI-I",
        "DVI-D",
        "DVI-A",
        "Composite",
        "SVIDEO",
        "LVDS",
        "Component",
        "DIN",
        "DP",
        "HDMI-A",
        "HDMI-B",
        "TV",
        "eDP",
        "Virtual",
        "DSI",
        "DPI",
        "Writeback",
        "SPI",
        "USB",
    ];
    let kind = usize::try_from(kind)
        .ok()
        .and_then(|k| KINDS.get(k))
        .unwrap_or(&KINDS[0]);
    format!("{kind}-{id}")
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub backend: BackendChoice,
    /// None: `$XDG_CONFIG_HOME/orrery/config.kdl` (or `~/.config/...`), the built-in config when missing.
    pub config: Option<PathBuf>,
    /// The headless output's size; the winit output follows its window.
    pub size: (i32, i32),
    /// The control socket. None: `$XDG_RUNTIME_DIR/orrery.sock`.
    pub socket: Option<PathBuf>,
    /// Answer `inject` and `inject-remote` (a testing aid).
    pub debug_inject: bool,
}

/// `WxH`, both positive.
pub fn parse_size(s: &str) -> Result<(i32, i32), String> {
    let bad = || format!("expected WxH with positive numbers, got {s:?}");
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: i32 = w.trim().parse().map_err(|_| bad())?;
    let h: i32 = h.trim().parse().map_err(|_| bad())?;
    if w <= 0 || h <= 0 || w > 16384 || h > 16384 {
        return Err(bad());
    }
    Ok((w, h))
}

/// The config file under a config base directory: `<base>/<NAME>/config.kdl`.
#[must_use]
pub fn config_path_in(base: &Path) -> PathBuf {
    base.join(NAME).join("config.kdl")
}

/// The config path when none is given: XDG first, then `~/.config`.
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_path_in(&base))
}

/// How often the config file is looked at for changes.
pub const CONFIG_POLL: Duration = Duration::from_secs(1);

/// The home directory, `$HOME`; None when unset or empty.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// The config file at `path`, read: the config (None when there is no file) or why it cannot be used, and the
/// files it is made of (`config::FileConfig::files`; just `path` when it is missing or unreadable).
#[derive(Debug)]
pub struct ConfigRead {
    pub config: Result<Option<Config>, String>,
    pub files: Vec<PathBuf>,
}

/// Reads and parses the config file at `path` with what it includes. The error lists every problem as
/// `FILE:LINE:COL: message`.
pub fn read_config_file(path: &Path) -> ConfigRead {
    match config::read_file(path, config::MAX_SOURCE) {
        Ok(None) => ConfigRead {
            config: Err(format!("{}: the config is larger than 1 MiB", path.display())),
            files: vec![path.to_owned()],
        },
        Ok(Some(src)) => {
            let read = config::parse_file(&src, path, home_dir().as_deref());
            ConfigRead {
                config: read.config.map(Some).map_err(|e| e.to_string()),
                files: read.files,
            }
        }
        Err(e) => ConfigRead {
            config: match e.kind() {
                std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(format!("cannot read {}: {e}", path.display())),
            },
            files: vec![path.to_owned()],
        },
    }
}

/// `read_config_file` as at startup: no path, or no file there, is the built-in config.
pub fn read_config(path: Option<&Path>) -> Result<Config, String> {
    let Some(path) = path else {
        return Ok(config::default_config());
    };
    read_config_file(path)
        .config
        .map(|c| c.unwrap_or_else(config::default_config))
}

/// The config to start with: the file's, or the built-in one when there is no file or it is broken (a broken
/// or unreadable file must not stop orrery from starting). Also why the file was not used, when broken (shown
/// in `state` and as the red bar), and whether this is the built-in fallback (`Loaded::fallback`).
pub fn load_config(path: Option<&Path>) -> Loaded {
    let fallback = |error, files| Loaded {
        config: config::default_config(),
        error,
        fallback: true,
        files,
    };
    let Some(path) = path else {
        return fallback(None, Vec::new());
    };
    let read = read_config_file(path);
    match read.config {
        Ok(Some(config)) => Loaded {
            config,
            error: None,
            fallback: false,
            files: read.files,
        },
        Ok(None) => fallback(None, read.files),
        Err(msg) => {
            tracing::error!("{msg}; using the built-in config");
            fallback(Some(msg), read.files)
        }
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub error: Option<String>,
    /// The built-in config stands in for a missing or broken file: the first good reload then also applies the
    /// file's monitor rules, as a start with that file would have.
    pub fallback: bool,
    /// The files the config is made of, to watch (`ConfigRead::files`).
    pub files: Vec<PathBuf>,
}

/// A file as the config watcher compares it: modification time and size (no inotify); None when it is missing.
pub type Stamp = Option<(SystemTime, u64)>;

/// Each file with its stamp now.
#[must_use]
pub fn config_stamps(files: &[PathBuf]) -> Vec<(PathBuf, Stamp)> {
    let stamp = |path: &Path| {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    };
    files.iter().map(|f| (f.clone(), stamp(f))).collect()
}

/// The stamps of `files`, read just now, to watch from here on: a file stamped `before` the read keeps that stamp,
/// so an edit that landed while it was read is seen as a change; one new to the list is stamped now.
#[must_use]
pub fn restamp(before: &[(PathBuf, Stamp)], files: &[PathBuf]) -> Vec<(PathBuf, Stamp)> {
    let fresh = config_stamps(files);
    (fresh.into_iter())
        .map(|(f, now)| match before.iter().find(|(b, _)| *b == f) {
            Some((_, then)) => (f, *then),
            None => (f, now),
        })
        .collect()
}

/// The first good config after a fallback start: each monitor takes the layout its monitor rule (or the first
/// layout) names as its default, and so do its shown workspace and its hidden ones still on the old default, as if
/// orrery had started with this config. Stacks follow `set_layout`'s rules.
pub fn apply_monitor_rules(stacks: &mut Stacks, config: &Config) {
    let monitors: Vec<(String, Layout)> = (stacks.monitors().iter())
        .map(|m| (m.name().to_owned(), m.default_layout().clone()))
        .collect();
    for (mi, (name, old)) in monitors.into_iter().enumerate() {
        let Some(layout) = layout_for_output(config, &name) else {
            continue;
        };
        let shown = stacks.monitors()[mi].shown();
        let follow: Vec<u8> = (stacks.workspaces().iter())
            .filter(|w| w.monitor() == mi && w.layout() != &layout)
            .filter(|w| w.number() == shown || w.layout() == &old)
            .map(Workspace::number)
            .collect();
        for n in follow {
            stacks.set_workspace_layout(n, layout.clone());
        }
        stacks.set_default_layout(&name, layout);
    }
}

/// After a reload, every workspace (hidden ones too) and every monitor's default takes the new definition of its
/// layout, by name. One whose layout the new config no longer has keeps it (not an error); the workspaces that do
/// are returned as (workspace, layout).
pub fn reapply_layouts(stacks: &mut Stacks, config: &Config) -> Vec<(u8, String)> {
    let by_name = |l: &Layout| config.layouts.iter().find(|x| x.name == l.name);
    let mut kept = Vec::new();
    let workspaces: Vec<(u8, Layout)> = (stacks.workspaces().iter())
        .map(|w| (w.number(), w.layout().clone()))
        .collect();
    for (n, layout) in workspaces {
        match by_name(&layout) {
            // Unchanged: nothing to do (the layout would keep the stacks anyway, but this is plainly a no-op).
            Some(new) if *new == layout => {}
            Some(new) => stacks.set_workspace_layout(n, new.clone()),
            None => kept.push((n, layout.name)),
        }
    }
    let defaults: Vec<(String, Layout)> = (stacks.monitors().iter())
        .map(|m| (m.name().to_owned(), m.default_layout().clone()))
        .collect();
    for (monitor, layout) in defaults {
        if let Some(new) = by_name(&layout).filter(|new| **new != layout) {
            stacks.set_default_layout(&monitor, new.clone());
        }
    }
    kept
}

/// The running `stacking` after a reload from `old` to `new`: what `toggle-stacking` set survives, unless the
/// config's own `stacking` changed, which then wins.
#[must_use]
pub fn stacking_after_reload(running: bool, old: &Config, new: &Config) -> bool {
    if old.stacking == new.stacking {
        running
    } else {
        new.stacking
    }
}

/// The output name of screen `number` on a backend whose outputs are called `base`: `base` for screen 1 (so monitor
/// rules written for one screen keep working), `base-k` for screen k.
#[must_use]
pub fn screen_name(base: &str, number: u8) -> String {
    if number == 1 {
        base.to_owned()
    } else {
        format!("{base}-{number}")
    }
}

/// How long a removed output's global stays disabled (announced as gone) before it is destroyed, so a client
/// binding it meanwhile gets no protocol error.
const GLOBAL_GRACE: Duration = Duration::from_secs(10);

/// Each screen's x in global space, from their widths in opening order: side by side from 0.
#[must_use]
pub fn side_by_side(widths: &[i32]) -> Vec<i32> {
    (widths.iter())
        .scan(0, |x, w| {
            let at = *x;
            *x += w;
            Some(at)
        })
        .collect()
}

/// A backend's own way to open and close screens past the one it starts with, set by the backend at init. None
/// on `Orrery` means the backend cannot (`open-screen` and `close-screen` are answered with an error).
pub trait Screens {
    /// Opens screen `number` (the lowest free, `Stacks::next_screen_number`), its output named with `screen_name`,
    /// and adds it with `Orrery::add_output` at `Orrery::next_screen_x`.
    fn open(&mut self, state: &mut Orrery, number: u8) -> Result<(), String>;
    /// Closes the screen whose output is `name`, taking it away with `Orrery::remove_output`.
    fn close(&mut self, state: &mut Orrery, name: &str) -> Result<(), String>;
}

/// A backend's consoles (bare metal), set by the backend at init. None on `Orrery`: there are none to switch to
/// (`vt` binds are logged and do nothing).
pub trait Consoles {
    /// Switches to console `vt` (1..=`config::CONSOLES`).
    fn switch(&mut self, vt: u8) -> Result<(), String>;
}

/// The layout an output starts with: its monitor rule, else the first layout.
pub fn layout_for_output(config: &Config, output: &str) -> Option<Layout> {
    let by_rule = config
        .monitors
        .iter()
        .find(|r| r.output == output)
        .and_then(|r| config.layouts.iter().find(|l| l.name == r.layout));
    by_rule.or_else(|| config.layouts.first()).cloned()
}

pub struct Orrery {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    loop_handle: LoopHandle<'static, Orrery>,

    pub space: Space<Window>,
    pub popups: PopupManager,
    /// Every live toplevel, mapped or not; `Stacks` knows them by id only.
    pub windows: HashMap<WinId, Window>,
    /// Toplevel surface -> id, so commits find their window without a scan.
    surfaces: HashMap<WlSurface, WinId>,
    /// Every layer surface orrery knows, wherever it is (drawing order is its screen's map's); see `layers`.
    layers: Vec<layers::Layered>,
    /// The layer surface a click gave the keyboard, and where the stacks' focus was then (`Orrery::layer_keyboard`).
    layer_focus: Option<(WlSurface, layers::FocusKey)>,
    /// Windows that have committed a buffer and so are in the stacks. The rest wait, configured to the slot they
    /// would open into, without hiding anything or taking focus.
    opened: HashSet<WinId>,
    /// Windows that have opened at least once: one mapped again (after a null buffer) is no launch's window.
    opened_before: HashSet<WinId>,
    /// A real pointer has moved over orrery. Until then (always, headless) the pointer has no position, so no
    /// surface gets pointer focus from arrange.
    pub pointer_seen: bool,
    /// The surface (and its origin) the pointer was last pointed at by arrange or real motion.
    pub pointer_target: Option<(WlSurface, Point<f64, Logical>)>,
    /// The cursor the client under the pointer asked for, drawn where orrery draws the pointer (`pointer_drawn`).
    pub cursor_image: CursorImageStatus,
    /// The backend shows no cursor of its own (bare metal): orrery draws the pointer for this machine's input too.
    pub draws_pointer: bool,
    /// Where named cursors come from (loaded as they are first drawn).
    pub cursor_theme: RefCell<cursor_theme::CursorTheme>,
    /// Keys held for binds (swallowed, show-tabs).
    pub held: input::Held,
    /// The last key press, for `state` (see `input::key_note`).
    pub last_key: Option<crate::ipc::KeyView>,
    /// Spawned programs not yet reaped.
    children: Vec<std::process::Child>,
    /// Recent spawns waiting for their window (see `placement`); expired on the next spawn or placement.
    launches: Launches,
    next_id: WinId,
    /// Where every window is.
    pub stacks: Stacks,
    /// Where a window with nowhere else to go opens (see `Config::stacking`); `toggle-stacking` flips it.
    pub stacking: bool,
    pub config: Config,
    /// Why the config file was not loaded (startup or the last reload); drawn as a red bar, shown in `state`.
    pub config_error: Option<String>,
    /// The file `reload` reads and the watcher watches; None when there is no path at all (the built-in config).
    config_path: Option<PathBuf>,
    /// The config file and the files it includes, as last read (`restamp`): what the watcher watches.
    config_stamps: Vec<(PathBuf, Stamp)>,
    /// The running config is the built-in stand-in for a missing or broken file (see `Loaded::fallback`).
    config_is_fallback: bool,
    /// The config file reads since startup (reloads, good or not): each is a `config` event.
    config_reads: u64,
    /// The event stream's subscribers.
    subscribers: subscribers::Subscribers,
    /// The control socket's path, handed to the programs orrery starts.
    control_socket: Option<PathBuf>,
    /// orrery's own program, handed to the programs it starts (`spawn::own_exe`).
    exe: Option<PathBuf>,
    /// In opening order, which is not the monitors' (number) order: look monitors up by name, never by index.
    pub outputs: Vec<Output>,
    /// Each output's global, by output name, to take away with it.
    output_globals: HashMap<String, GlobalId>,
    /// The backend's screens beyond the first; None when it cannot open any.
    pub screens: Option<Box<dyn Screens>>,
    /// The backend's consoles; None when it has none.
    pub consoles: Option<Box<dyn Consoles>>,
    /// How long frames take to draw (backends with a renderer record them).
    frames: frames::FrameStats,
    /// This machine in a session of linked orrerys; None alone.
    net: Option<net::Net>,
    /// What the primary's input has down here (a satellite holding the focus marker).
    remote: remote::Remote,
    /// `inject` and `inject-remote` are answered (`--debug-inject`, a testing aid).
    debug_inject: bool,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_activation_state: XdgActivationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    /// The linux-dmabuf global a GPU backend advertised (`advertise_dmabuf`); None headless or without imports.
    dmabuf: Option<dmabuf::Advertised>,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Orrery>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Orrery>,
}

/// Per-client data Smithay requires.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl Orrery {
    /// The state for `dh`'s display, with no Wayland socket yet (`run` opens it with `listen`).
    fn new(
        event_loop: &EventLoop<'static, Orrery>,
        dh: DisplayHandle,
        config_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        // The stamp first: an edit landing while the file is read is then seen again by the watcher.
        let before = config_stamps(config_path.as_slice());
        let Loaded {
            config,
            error: config_error,
            fallback: config_is_fallback,
            files,
        } = load_config(config_path.as_deref());
        let config_stamps = restamp(&before, &files);
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let dmabuf_state = DmabufState::new();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        let (rate, delay) = config.keyboard.repeat_info();
        // The config's keymap was built once when it was read; should it fail now, xkb's defaults are better than
        // no keyboard.
        if seat.add_keyboard(config.keyboard.xkb(), delay, rate).is_err() {
            tracing::error!("cannot build the keymap of the `keyboard` block; using xkb's default");
            seat.add_keyboard(Default::default(), delay, rate)
                .context("cannot set up the keyboard (is xkeyboard-config installed?)")?;
        }
        seat.add_pointer();

        Ok(Self {
            start_time: Instant::now(),
            socket_name: OsString::new(),
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            loop_handle: event_loop.handle(),
            space: Space::default(),
            popups: PopupManager::default(),
            windows: HashMap::new(),
            surfaces: HashMap::new(),
            layers: Vec::new(),
            layer_focus: None,
            opened: HashSet::new(),
            opened_before: HashSet::new(),
            pointer_seen: false,
            pointer_target: None,
            cursor_image: CursorImageStatus::default_named(),
            draws_pointer: false,
            cursor_theme: RefCell::new(cursor_theme::CursorTheme::from_env()),
            held: input::Held::default(),
            last_key: None,
            children: Vec::new(),
            launches: Launches::default(),
            next_id: 1,
            stacks: Stacks::new(),
            stacking: config.stacking,
            config,
            config_error,
            config_path,
            config_stamps,
            config_is_fallback,
            config_reads: 0,
            subscribers: subscribers::Subscribers::default(),
            control_socket: None,
            exe: None,
            outputs: Vec::new(),
            output_globals: HashMap::new(),
            screens: None,
            consoles: None,
            frames: frames::FrameStats::new(Instant::now()),
            net: None,
            remote: remote::Remote::default(),
            debug_inject: false,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            xdg_activation_state,
            layer_shell_state,
            shm_state,
            dmabuf_state,
            dmabuf: None,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
        })
    }

    /// Announces an output to clients, puts it in the space at `area` and gives it a monitor in the stacks (the
    /// screen numbered `Stacks::next_screen_number`), whose layout divides what layer surfaces leave of it.
    pub fn add_output(&mut self, output: Output, area: Rect) {
        let global = output.create_global::<Orrery>(&self.display_handle);
        let replaced = self.output_globals.insert(output.name(), global);
        debug_assert!(replaced.is_none(), "two outputs named {}", output.name());
        self.space.map_output(&output, (area.x, area.y));
        if let Some(layout) = layout_for_output(&self.config, &output.name()) {
            let usable = self.usable_area_of(&output, area);
            self.stacks.add_monitor(&output.name(), usable, layout);
        }
        self.outputs.push(output);
        self.arrange();
    }

    /// Takes an output away: its layer surfaces are closed, its global goes (disabled now, destroyed after
    /// `GLOBAL_GRACE`), and its monitor's workspaces move as `Stacks::remove_monitor` says. False when there is no
    /// such output.
    pub fn remove_output(&mut self, name: &str) -> bool {
        let Some(i) = self.outputs.iter().position(|o| o.name() == name) else {
            return false;
        };
        let output = self.outputs.remove(i);
        self.close_layers(&output);
        self.space.unmap_output(&output);
        if let Some(global) = self.output_globals.remove(name) {
            self.display_handle.disable_global::<Orrery>(global.clone());
            let dh = self.display_handle.clone();
            let id = global.clone();
            let later = (self.loop_handle).insert_source(Timer::from_duration(GLOBAL_GRACE), move |_, _, _| {
                dh.remove_global::<Orrery>(id.clone());
                TimeoutAction::Drop
            });
            if later.is_err() {
                self.display_handle.remove_global::<Orrery>(global);
            }
        }
        self.stacks.remove_monitor(name);
        self.arrange();
        true
    }

    /// The x where the next screen goes in global space: right of the rightmost output, so screens sit in
    /// opening order, whatever their numbers.
    pub fn next_screen_x(&self) -> i32 {
        (self.outputs.iter())
            .filter_map(|o| self.space.output_geometry(o))
            .map(|g| g.loc.x + g.size.w)
            .max()
            .unwrap_or(0)
    }

    /// Every screen side by side again in opening order, so a wider one never overlaps the next one in global space
    /// (where pointer positions are hit-tested) and a closed one leaves no gap. The monitors of the screens that moved
    /// follow, and that of `resized` (its size changed) even where it stayed.
    pub fn lay_side_by_side(&mut self, resized: Option<&str>) {
        let sizes: Vec<_> = (self.outputs.iter())
            .map(|o| o.current_mode().map_or((0, 0), |m| (m.size.w, m.size.h)))
            .collect();
        let xs = side_by_side(&sizes.iter().map(|&(w, _)| w).collect::<Vec<_>>());
        for ((output, (w, h)), x) in self.outputs.clone().into_iter().zip(sizes).zip(xs) {
            let moved = output.current_location().x != x;
            if moved {
                output.change_current_state(None, None, None, Some((x, 0).into()));
                self.space.map_output(&output, (x, 0));
            }
            if moved || resized == Some(output.name().as_str()) {
                self.resize_output(&output.name(), Rect { x, y: 0, w, h });
            }
        }
    }

    /// Runs one of the backend's screen operations, `what` naming it in the error when the backend has none.
    fn with_screens(
        &mut self,
        what: &str,
        op: impl FnOnce(&mut dyn Screens, &mut Self) -> Result<(), String>,
    ) -> Response {
        let Some(mut screens) = self.screens.take() else {
            return Response::error(format!("this backend cannot {what} screens"));
        };
        let done = op(screens.as_mut(), self);
        self.screens = Some(screens);
        match done {
            Ok(()) => Response::ok(),
            Err(e) => Response::error(e),
        }
    }

    /// An output changed size (or place): its layer surfaces are arranged again, and the monitor's slots follow what
    /// they leave of `area`.
    pub fn resize_output(&mut self, name: &str, area: Rect) {
        let usable = self.output_named(name).map_or(area, |o| self.usable_area_of(&o, area));
        self.stacks.set_area(name, usable);
        self.arrange();
    }

    /// A new toplevel gets an id and waits: it is configured to the focused slot but enters the stacks only on
    /// its first buffer (`toplevel_committed`), so a window that never maps hides nothing and takes no focus.
    pub fn add_window(&mut self, window: Window) -> WinId {
        let id = self.next_id;
        self.next_id += 1;
        if let Some(t) = window.toplevel() {
            self.surfaces.insert(t.wl_surface().clone(), id);
        }
        self.windows.insert(id, window);
        self.arrange();
        id
    }

    /// A toplevel's own surface committed. The first buffer opens it where `place` says; a null buffer (unmap)
    /// takes it out of the stacks and back to waiting.
    pub fn toplevel_committed(&mut self, id: WinId, has_buffer: bool) {
        let Some(toplevel) = self.windows.get(&id).and_then(|w| w.toplevel()).cloned() else {
            return;
        };
        if !has_buffer && self.opened.remove(&id) {
            self.stacks.close(id);
            // The client must redo the initial commit-configure sequence; that next commit gets the configure.
            toplevel.reset_initial_configure_sent();
            self.arrange();
            return;
        }
        // The first commit (or the first after an unmap) is answered with a configure carrying what arrange set.
        if !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        if has_buffer && self.opened.insert(id) {
            self.place(id, &toplevel);
            self.arrange();
        }
    }

    /// Puts a window into the stacks as `placement::decide` says, from its parent (a placed one), its client's
    /// pid chain and the launches. A window mapped again sees no launches: only its parent or `stacking` count.
    /// A dialog that does not land as the focused window is marked for attention.
    fn place(&mut self, id: WinId, toplevel: &ToplevelSurface) {
        let now = Instant::now();
        let parent = toplevel.parent().and_then(|s| self.id_of_surface(&s));
        let parent_placed = parent.is_some_and(|p| self.stacks.where_is(p).is_some());
        let first_time = self.opened_before.insert(id);
        self.launches.expire(now);
        let chain = if first_time {
            self.launch_pid_chain(toplevel.wl_surface())
        } else {
            Vec::new()
        };
        let mut none = Launches::default();
        let launches = if first_time { &mut self.launches } else { &mut none };
        match placement::decide(parent, parent_placed, &chain, launches, now, self.stacking) {
            Landing::OnParent(p) => {
                // decide checked the parent is placed, so this cannot miss; stacked if it ever does. A dialog
                // that is not the focused window (in a background tab, or in a slot without focus) is marked, so
                // a hidden modal shows as a badge.
                if self.stacks.open_on(id, p) {
                    self.stacks.mark(id);
                } else {
                    self.stacks.open_stacked(id);
                }
            }
            Landing::NewTab { workspace, slot } => self.stacks.open_tab(id, Some(workspace), Some(slot)),
            Landing::Stacked => self.stacks.open_stacked(id),
            Landing::Tab => self.stacks.open_tab(id, None, None),
        }
    }

    /// The pid chain (`placement::pid_chain`) of `surface`'s client; empty when no launch waits, so the client and
    /// /proc are asked only when a launch could match.
    fn launch_pid_chain(&self, surface: &WlSurface) -> Vec<u32> {
        let pid = self.launches.waiting().then(|| self.client_pid(surface)).flatten();
        pid.map_or_else(Vec::new, |pid| {
            placement::pid_chain(pid, |p| {
                // The name in it may be any bytes; lossy keeps the fields after it readable.
                let stat = std::fs::read(format!("/proc/{p}/stat")).ok()?;
                Some(String::from_utf8_lossy(&stat).into_owned())
            })
        })
    }

    /// A layer surface mapped for the first time: a launch of its client (or an ancestor) is used up, as a window
    /// would use it, though nothing is placed. Otherwise a launched panel or launcher, which opens no window, would
    /// leave its launch waiting, and the 5 s fallback would hand it the next stranger's window.
    pub(super) fn claim_launch_of(&mut self, surface: &WlSurface) {
        self.launches.expire(Instant::now());
        let chain = self.launch_pid_chain(surface);
        self.launches.claim_by_pids(&chain);
    }

    /// An xdg-activation request for the window of `surface` (its own, a subsurface's or a popup's) with `token`:
    /// - a placed window bringing the token of a launch still waiting (`Launches::claim_token`) moves to that
    ///   launch's target as a new tab, shown and focused, and the launch is claimed;
    /// - a client token kept by `token_created` (input to the focused client, under `LAUNCH_WAIT` old) focuses the
    ///   window's slot when the window is what it shows; the token is spent either way;
    /// - anything else (no such token, an old, spent or unbacked one, or a window beneath) only marks the window.
    ///
    /// Nothing beneath is ever raised, and without input behind it no request takes focus. A layer surface is never
    /// placed or focused this way, but a launch's token it brings is spent (the launch claimed), for the reason
    /// `claim_launch_of` gives.
    pub fn activation_requested(&mut self, token: &str, surface: &WlSurface) {
        let now = Instant::now();
        let Some(id) = self.window_of(surface) else {
            if self.is_layer_surface(surface) {
                self.launches.expire(now);
                self.launches.claim_token(token, now);
            }
            return;
        };
        self.launches.expire(now);
        self.forget_old_tokens(now);
        let placed = self.stacks.where_is(id).is_some();
        let target = placed
            .then(|| self.launches.claim_token(token, now))
            .flatten()
            .map(|l| (l.workspace, l.slot));
        if let Some((workspace, slot)) = target {
            self.stacks.move_window_to_tab(id, workspace, slot);
        } else {
            let backed = (self.xdg_activation_state).remove_token(&XdgActivationToken::from(token.to_owned()));
            if !(backed && self.stacks.focus_window(id)) {
                self.stacks.mark(id);
            }
        }
        self.arrange();
    }

    /// Whether a client's new token comes from input to the client with keyboard focus: it carries a serial on
    /// orrery's seat no older than the keyboard's latest enter (so given since the focus came), and the client
    /// asking for it owns the focused surface. Only such tokens are kept (`token_created`).
    pub fn input_backed(&self, data: &XdgActivationTokenData) -> bool {
        let Some((serial, seat)) = &data.serial else {
            return false;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let ours = Seat::<Self>::from_resource(seat).is_some_and(|s| s == self.seat);
        let fresh = keyboard
            .last_enter()
            .is_some_and(|enter| serial.is_no_older_than(&enter));
        let focused = (keyboard.current_focus())
            .and_then(|s| self.display_handle.get_client(s.id()).ok())
            .map(|c| c.id());
        ours && fresh && focused.is_some() && focused == data.client_id
    }

    /// Drops the kept client tokens `LAUNCH_WAIT` old or older, so the pool stays small and old tokens focus nothing.
    pub fn forget_old_tokens(&mut self, now: Instant) {
        (self.xdg_activation_state)
            .retain_tokens(|_, data| now.saturating_duration_since(data.timestamp) < placement::LAUNCH_WAIT);
    }

    /// The window `surface` belongs to: its own toplevel surface, or a subsurface or popup (or chain of them) on
    /// one.
    fn window_of(&self, surface: &WlSurface) -> Option<WinId> {
        let mut root = surface.clone();
        loop {
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(id) = self.id_of_surface(&root) {
                return Some(id);
            }
            root = find_popup_root_surface(&self.popups.find_popup(&root)?).ok()?;
        }
    }

    /// The pid of the client owning `surface`, from its socket's credentials.
    fn client_pid(&self, surface: &WlSurface) -> Option<u32> {
        let client = self.display_handle.get_client(surface.id()).ok()?;
        let creds = client.get_credentials(&self.display_handle).ok()?;
        u32::try_from(creds.pid).ok()
    }

    /// A toplevel went away (destroyed, or its client disconnected).
    pub fn remove_window(&mut self, id: WinId) {
        self.forget_window(id);
        self.arrange();
    }

    fn forget_window(&mut self, id: WinId) {
        if let Some(window) = self.windows.remove(&id) {
            self.space.unmap_elem(&window);
        }
        self.surfaces.retain(|_, &mut w| w != id);
        self.opened.remove(&id);
        self.opened_before.remove(&id);
        self.stacks.close(id);
    }

    pub fn id_of_surface(&self, surface: &WlSurface) -> Option<WinId> {
        self.surfaces.get(surface).copied()
    }

    pub fn window_of_surface(&self, surface: &WlSurface) -> Option<&Window> {
        self.id_of_surface(surface).and_then(|id| self.windows.get(&id))
    }

    /// The slot rectangle a window lives in, or would open into while it waits.
    pub fn slot_rect_of(&self, id: WinId) -> Option<Rect> {
        self.stacks
            .placements()
            .find(|p| p.win == id)
            .map(|p| p.rect)
            .or_else(|| self.stacks.focused_rect())
    }

    /// Makes Wayland match the stacks, in one pass over `placements()`: the top of a slot's current tab in a shown
    /// workspace is mapped at the slot's rectangle; every other window (hidden workspaces' too) is unmapped but
    /// stays alive. Both are configured to the slot's size (so showing one later needs no round trip), tiled on
    /// every edge, activated only when focused. Waiting windows (no buffer yet) are unmapped and configured to where
    /// they would open (the focused slot). Opened windows in no slot (pending without a monitor) are unmapped and
    /// keep their size; they are neither drawn nor focused. Keyboard and pointer focus then follow.
    pub fn arrange(&mut self) {
        let mut placed = HashSet::with_capacity(self.windows.len());
        for p in self.stacks.placements() {
            let Some(window) = self.windows.get(&p.win) else {
                continue;
            };
            placed.insert(p.win);
            configure(window, p.rect, p.focused);
            if p.shown && p.on_top {
                let at = (p.rect.x, p.rect.y).into();
                if self.space.element_location(window) != Some(at) {
                    self.space.map_element(window.clone(), at, false);
                }
            } else {
                self.space.unmap_elem(window);
            }
        }
        let waiting_rect = self.stacks.focused_rect();
        for (id, window) in &self.windows {
            if !placed.contains(id) {
                self.space.unmap_elem(window);
                if let Some(rect) = waiting_rect.filter(|_| !self.opened.contains(id)) {
                    configure(window, rect, false);
                }
            }
        }
        self.update_keyboard_focus();
        self.update_pointer_focus();
    }

    /// The keyboard to the layer surface that should have it (`layer_keyboard`), else the focused window. A popup
    /// grab (an open menu) keeps the keyboard while its window is still the one to focus; when the focus moves away
    /// (another slot, workspace or layer surface), its menus are closed first, ending the grab.
    fn update_keyboard_focus(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let want = self.layer_keyboard().or_else(|| {
            (self.stacks.focused_window())
                .and_then(|id| self.windows.get(&id))
                .and_then(|w| w.toplevel())
                .map(|t| t.wl_surface().clone())
        });
        if keyboard.is_grabbed() {
            let popup = keyboard.current_focus().and_then(|s| self.popups.find_popup(&s));
            if let Some(popup) = popup
                && let Ok(root) = find_popup_root_surface(&popup)
            {
                if want.as_ref() == Some(&root) {
                    return;
                }
                let _ = PopupManager::dismiss_popup(&root, &popup);
            }
        }
        if keyboard.current_focus() != want {
            keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
        }
    }

    /// The pointer stays where it is but the surface under it may have changed (a window hidden or shown):
    /// re-evaluate, so buttons and scrolling never reach a hidden window.
    fn update_pointer_focus(&mut self) {
        if !self.pointer_seen {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = pointer.current_location();
        let under = self.surface_under(location);
        // The same surface at the same origin: nothing for the client to learn. A moved surface (a window
        // remapped elsewhere) needs the motion even if the surface is unchanged.
        if under == self.pointer_target {
            return;
        }
        self.pointer_target = under.clone();
        let time = self.clock();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    /// Per-frame housekeeping shared by the backends: forget dead windows and layer surfaces (arranging once),
    /// refresh the space (output enter and leave), clean popups.
    pub fn refresh(&mut self) {
        let dead: Vec<WinId> = self
            .windows
            .iter()
            .filter(|(_, w)| !w.alive())
            .map(|(&id, _)| id)
            .collect();
        let dead_layers = self.forget_dead_layers();
        if !dead.is_empty() || dead_layers {
            for id in dead {
                self.forget_window(id);
            }
            self.arrange();
        }
        self.space.refresh();
        self.popups.cleanup();
    }

    /// orrery draws the pointer: input from the primary moves it, or the backend has no cursor of its own and input is
    /// here (while it is away the pointer is held, and hidden as nested).
    #[must_use]
    pub fn pointer_drawn(&self) -> bool {
        self.remote_input_active() || (self.draws_pointer && !self.input_away())
    }

    /// Frame callbacks for every window and layer surface on `output`, and the cursor surface while it is drawn, so
    /// clients keep drawing.
    pub fn send_frames(&self, output: &Output) {
        let now = self.start_time.elapsed();
        for window in self.space.elements() {
            window.send_frame(output, now, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.send_frame(output, now, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }
        if let render::CursorDraw::Surface(surface) = render::cursor_draw(self.pointer_drawn(), &self.cursor_image) {
            smithay::desktop::utils::send_frames_surface_tree(&surface, output, now, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    /// One request, from the socket, a key or the primary. Pure requests go through the stacks and are arranged;
    /// in a session `workspace` and `move-to-workspace` go through it first. A `to` for another machine is sent
    /// there and answered ok at once (its answer, an error, is logged): the control socket uses `link_forward`.
    pub fn handle_request(&mut self, request: Request) -> Response {
        match request {
            Request::Workspace { number } if self.net.is_some() => self.session_workspace(number),
            Request::MoveToWorkspace { number } if self.net.is_some() => self.session_move(number),
            Request::To { machine, request } => {
                let to = machine.clone();
                let logged = Box::new(move |r: Response| {
                    if let Some(e) = r.error {
                        tracing::info!("{to}: {e}");
                    }
                });
                self.link_forward(&machine, *request, logged)
                    .unwrap_or_else(Response::ok)
            }
            Request::State => Response::with_state(self.state_view()),
            // The control socket hands its connection to `subscribe`: one here came another way.
            Request::Subscribe => Response::error(crate::ipc::SUBSCRIBE_HERE),
            Request::Close => {
                // A request to the client; the window leaves the stacks when it is destroyed or unmapped.
                let toplevel = (self.stacks.focused_window())
                    .and_then(|id| self.windows.get(&id))
                    .and_then(|w| w.toplevel());
                match toplevel {
                    Some(t) => {
                        t.send_close();
                        Response::ok()
                    }
                    None => Response::error("no focused window to close"),
                }
            }
            Request::Reload => match self.reload() {
                Ok(()) => Response::ok(),
                Err(e) => Response::error(e),
            },
            Request::Quit => {
                self.loop_signal.stop();
                Response::ok()
            }
            Request::ToggleStacking => {
                self.stacking = !self.stacking;
                tracing::info!("stacking {}", if self.stacking { "on" } else { "off" });
                Response::ok()
            }
            Request::Spawn { argv } => match self.spawn(&argv) {
                Ok(()) => Response::ok(),
                Err(e) => Response::error(e),
            },
            // Each backend opens its own kind of screen.
            Request::OpenScreen => self.with_screens("open", |screens, state| {
                let number = (state.stacks.next_screen_number())
                    .ok_or_else(|| format!("every screen number (1 to {}) is taken", crate::stacks::WORKSPACES))?;
                screens.open(state, number)
            }),
            Request::CloseScreen { name } => self.with_screens("close", |screens, state| screens.close(state, &name)),
            Request::Inject { .. } | Request::InjectRemote { .. } if !self.debug_inject => Response::error(format!(
                "`inject` and `inject-remote` are testing aids: start {NAME} with --debug-inject to use them"
            )),
            Request::Inject { input } => self.inject(*input),
            Request::InjectRemote { machine, message } => self.inject_remote(&machine, &message),
            pure => self.apply(pure),
        }
    }

    /// A pure request applied to the stacks, then arranged.
    fn apply(&mut self, pure: Request) -> Response {
        match pure.apply(&mut self.stacks, &self.config) {
            Ok(()) => {
                self.arrange();
                Response::ok()
            }
            Err(e) => Response::error(e),
        }
    }

    /// Reads the config file again. A good one replaces the config (binds, colours, layouts: each monitor takes
    /// its layout's new definition by name, or after a fallback start its monitor rule; `stacking` as
    /// `stacking_after_reload` says; the keyboard as `apply_keyboard` says) and clears `config_error`; a broken one
    /// leaves everything as it was and sets `config_error`, which shows as the red bar until a good config loads. A
    /// file gone while running (an editor between delete and write, say) keeps the running config: the built-in one
    /// stands in only at startup. That is an error answer for `reload` but not a `config_error`. In a session the
    /// secret file is read again too: a different secret drops every link (they open again on it), and one that
    /// cannot be read is the `config_error` and this answer's error, the config being applied all the same.
    /// `listen`, `primary` (the `secret-file` path too) and `name` take effect on the next start.
    pub fn reload(&mut self) -> Result<(), String> {
        let Some(path) = self.config_path.clone() else {
            return Err("no config path (neither --config, XDG_CONFIG_HOME nor HOME): nothing to reload".into());
        };
        // Stamped before reading, so the watcher sees an edit landing meanwhile; the includes may have changed.
        let before = config_stamps(&self.watched());
        let read = read_config_file(&path);
        self.config_stamps = restamp(&before, &read.files);
        match read.config {
            Ok(None) => {
                let msg = format!("{} is gone; keeping the current config", path.display());
                tracing::warn!("{msg}");
                Err(msg)
            }
            Ok(Some(config)) => {
                self.config_reads += 1;
                if std::mem::take(&mut self.config_is_fallback) {
                    apply_monitor_rules(&mut self.stacks, &config);
                } else {
                    for (workspace, layout) in reapply_layouts(&mut self.stacks, &config) {
                        tracing::info!("the new config has no layout {layout:?}; workspace {workspace} keeps it");
                    }
                }
                self.stacking = stacking_after_reload(self.stacking, &self.config, &config);
                if (&self.config.link, &self.config.name) != (&config.link, &config.name) {
                    tracing::warn!(
                        "`listen`, `primary`, their `secret-file` path or `name` changed: they take effect when {NAME} \
                         starts again (the secret file's contents are read again now)"
                    );
                }
                let old = std::mem::replace(&mut self.config, config);
                let keyboard = self.apply_keyboard(&old.keyboard).err();
                self.config_error = match (keyboard, self.link_reload()) {
                    (Some(k), Some(l)) => Some(format!("{k}; {l}")),
                    (k, l) => k.or(l),
                };
                self.arrange();
                tracing::info!("config reloaded");
                match &self.config_error {
                    Some(e) => Err(e.clone()),
                    None => Ok(()),
                }
            }
            Err(e) => {
                self.config_reads += 1;
                tracing::error!("{e}; keeping the old config");
                self.config_error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The watcher's tick: reload when the stamp of the file or of one it includes changed since last read (a
    /// broken file is not retried until one changes again).
    fn check_config_file(&mut self) {
        if config_stamps(&self.watched()) != self.config_stamps {
            // A failure is logged and kept in config_error by reload itself, which stamps the files again.
            let _ = self.reload();
        }
    }

    /// The files the watcher watches: the config file and what it included when last read.
    fn watched(&self) -> Vec<PathBuf> {
        self.config_stamps.iter().map(|(f, _)| f.clone()).collect()
    }

    /// Starts the config's `spawn-at-start` programs, once, as `spawn` does; one that cannot start is logged.
    fn spawn_at_start(&mut self) {
        for start in self.config.spawn_at_start.clone() {
            if let Err(e) = self.spawn_with(&start.argv, start.launch) {
                tracing::warn!("spawn-at-start: {e}");
            }
        }
    }

    /// The `state` answer.
    pub fn state_view(&self) -> StateView {
        let mut view = self.summary_view();
        view.frames = self.frames.view();
        view.dmabuf = self.dmabuf_view();
        view.last_key = self.last_key.clone();
        let now = Instant::now();
        view.launches = (self.launches.all().iter())
            .map(|l| LaunchView {
                pid: l.pid,
                token: l.token.clone(),
                age_ms: u64::try_from(now.saturating_duration_since(l.at).as_millis()).unwrap_or(u64::MAX),
                claimed: l.claimed,
            })
            .collect();
        view
    }

    /// The state the event stream follows, taken every loop iteration while anyone subscribes: `state` without
    /// `frames`, `launches` and `last_key`.
    fn summary_view(&self) -> StateView {
        let colors = &self.config.colors;
        let mut view = StateView::new(&self.stacks, self.stacking, self.config_error.clone(), colors, |id| {
            self.windows
                .get(&id)
                .and_then(|w| w.toplevel())
                .map(|t| {
                    with_states(t.wl_surface(), |states| {
                        states
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .and_then(|d| d.lock().ok())
                            .map(|d| (d.app_id.clone(), d.title.clone()))
                            .unwrap_or_default()
                    })
                })
                .unwrap_or_default()
        });
        for monitor in &mut view.monitors {
            monitor.layers = self.layer_views(&monitor.name);
        }
        view.session = self.session_view().map(Box::new);
        view
    }

    /// A frame took `took` to draw (`render_output`): the statistics, and their log line every 10 s.
    pub fn frame_drawn(&mut self, took: Duration) {
        if let Some(line) = self.frames.record(took, Instant::now()) {
            tracing::info!("{line}");
        }
    }
}

/// Sets a toplevel's pending size and states and sends them if they changed. Before the initial commit the
/// protocol forbids configures; `toplevel_committed` sends the first.
fn configure(window: &Window, rect: Rect, focused: bool) {
    let Some(toplevel) = window.toplevel() else {
        return;
    };
    toplevel.with_pending_state(|s| {
        s.size = Some(configure_size(rect).into());
        for edge in TILED {
            s.states.set(edge);
        }
        if focused {
            s.states.set(xdg_toplevel::State::Activated);
        } else {
            s.states.unset(xdg_toplevel::State::Activated);
        }
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

/// A slot's size as a configure carries it: at least 1x1 (defence in depth: layouts refuse tiny slots at the
/// reference screen, but a small output can still make one 0 px).
fn configure_size(rect: Rect) -> (i32, i32) {
    (rect.w.max(1), rect.h.max(1))
}

const TILED: [xdg_toplevel::State; 4] = [
    xdg_toplevel::State::TiledLeft,
    xdg_toplevel::State::TiledRight,
    xdg_toplevel::State::TiledTop,
    xdg_toplevel::State::TiledBottom,
];

/// Opens the Wayland socket (in `$XDG_RUNTIME_DIR`) and wires the display into the loop.
fn listen(display: Display<Orrery>, event_loop: &mut EventLoop<'static, Orrery>) -> anyhow::Result<OsString> {
    let socket = ListeningSocketSource::new_auto()
        .context("cannot open a Wayland socket (is XDG_RUNTIME_DIR set and writable?)")?;
    let name = socket.socket_name().to_os_string();
    let handle = event_loop.handle();
    handle
        .insert_source(socket, |stream, _, state| {
            if let Err(e) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("cannot accept a client: {e}");
            }
        })
        .map_err(|e| anyhow::anyhow!("cannot watch the Wayland socket: {e}"))?;
    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // Safety: the display lives in this source and is never dropped while the loop runs.
                if let Err(e) = unsafe { display.get_mut().dispatch_clients(state) } {
                    tracing::error!("dispatching Wayland clients failed, stopping: {e}");
                    return Err(e);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("cannot watch the Wayland display: {e}"))?;
    Ok(name)
}

/// `orrery run`: builds the state, starts a backend, prints `WAYLAND_DISPLAY=<name>`, runs until quit or
/// SIGTERM/SIGINT.
pub fn run(opts: RunOptions) -> anyhow::Result<()> {
    let wayland_display = std::env::var_os("WAYLAND_DISPLAY");
    let backend = (opts.backend).backend(wayland_display.as_deref(), std::env::var_os("DISPLAY").as_deref());
    let config_path = opts.config.clone().or_else(default_config_path);

    // First, before any socket of ours exists: a second orrery must refuse without side effects (one answering
    // there was joined instead, see `ipc_server::join`; this is one still starting). Dropped (and so removed)
    // when run returns.
    let Some(control_path) = opts.socket.clone().or_else(ipc_server::default_socket_path) else {
        bail!("no control socket path: pass --socket or set XDG_RUNTIME_DIR");
    };
    let mut control = ipc_server::bind(&control_path)?;

    let mut event_loop: EventLoop<'static, Orrery> = EventLoop::try_new().context("cannot create the event loop")?;
    let display: Display<Orrery> = Display::new().context("cannot create the Wayland display")?;
    // A clean stop on SIGTERM/SIGINT, so the socket is removed; SIGCHLD reaps spawned children. calloop blocks
    // these signals in this thread and children inherit the mask, so spawn clears it in the child. Before any
    // thread starts: threads inherit the mask too, so the signals can only arrive here, never kill orrery by
    // landing in a thread that doesn't block them.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD]).context("cannot watch signals")?;
    event_loop
        .handle()
        .insert_source(signals, |event, _, state| match event.signal() {
            Signal::SIGCHLD => state.reap_children(),
            _ => state.loop_signal.stop(),
        })
        .map_err(|e| anyhow::anyhow!("cannot watch signals: {e}"))?;

    let mut state = Orrery::new(&event_loop, display.handle(), config_path)?;
    state.socket_name = listen(display, &mut event_loop)?;
    state.debug_inject = opts.debug_inject;
    state.control_socket = Some(control_path.clone());
    state.exe = spawn::own_exe();

    event_loop
        .handle()
        .insert_source(Timer::from_duration(CONFIG_POLL), |_, _, state| {
            state.check_config_file();
            TimeoutAction::ToDuration(CONFIG_POLL)
        })
        .map_err(|e| anyhow::anyhow!("cannot watch the config file: {e}"))?;

    control.serve(&event_loop.handle())?;
    tracing::info!("control socket {}", control_path.display());

    // After the control socket: a second orrery has refused (or joined the first) before opening a seat session.
    match backend {
        Backend::Drm => backend_drm::init(&mut event_loop, &mut state)?,
        Backend::Winit => backend_winit::init(&mut event_loop, &mut state)?,
        Backend::Headless => backend_headless::init(&mut event_loop, &mut state, opts.size)?,
    }
    state.start_link();
    state.spawn_at_start();

    let Some(name) = state.socket_name.to_str().map(str::to_owned) else {
        bail!("the Wayland socket name is not UTF-8");
    };
    // For whoever started orrery (scripts read it). Best effort: bare metal, stdout is often the console, which can
    // refuse writes once orrery has the screen (EIO); that must not stop orrery, and the log line below has it too.
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "WAYLAND_DISPLAY={name}").and_then(|()| stdout.flush()) {
        tracing::warn!("cannot print WAYLAND_DISPLAY to stdout ({e}); the next line has it");
    }
    drop(stdout);
    tracing::info!("listening on WAYLAND_DISPLAY={name}");

    event_loop
        .run(None, &mut state, |state| {
            state.link_tick();
            // What this iteration changed, to the event stream's subscribers.
            state.publish_events();
            // Everything queued for clients during this iteration goes out now.
            if let Err(e) = state.display_handle.flush_clients() {
                tracing::warn!("flushing clients: {e}");
            }
        })
        .context("the event loop failed")?;
    tracing::info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_and_config_paths_come_from_the_name() {
        let dir = Path::new("/run/user/1000");
        assert_eq!(ipc_server::socket_path_in(dir), dir.join(format!("{NAME}.sock")));
        let base = Path::new("/home/someone/.config");
        assert_eq!(config_path_in(base), base.join(NAME).join("config.kdl"));
    }

    #[test]
    fn screens_sit_side_by_side_in_opening_order() {
        assert_eq!(side_by_side(&[]), Vec::<i32>::new());
        assert_eq!(side_by_side(&[1280, 800, 1920]), vec![0, 1280, 2080]);
    }

    #[test]
    fn screens_are_named_after_their_number_the_first_as_the_backend() {
        assert_eq!(screen_name("headless", 1), "headless");
        assert_eq!(screen_name("headless", 2), "headless-2");
        assert_eq!(screen_name("winit", 10), "winit-10");
    }

    #[test]
    fn auto_is_nested_inside_a_session_and_bare_metal_on_a_console() {
        let set = Some(OsStr::new("wayland-1"));
        let x = Some(OsStr::new(":0"));
        let empty = Some(OsStr::new(""));
        assert_eq!(BackendChoice::Auto.backend(set, None), Backend::Winit);
        assert_eq!(BackendChoice::Auto.backend(None, x), Backend::Winit);
        assert_eq!(BackendChoice::Auto.backend(None, None), Backend::Drm);
        assert_eq!(
            BackendChoice::Auto.backend(empty, empty),
            Backend::Drm,
            "empty is unset"
        );
        // An explicit choice wins whatever the environment says.
        assert_eq!(BackendChoice::Drm.backend(set, x), Backend::Drm);
        assert_eq!(BackendChoice::Winit.backend(None, None), Backend::Winit);
        assert_eq!(BackendChoice::Headless.backend(set, None), Backend::Headless);
    }

    #[test]
    fn connectors_are_named_as_the_kernel_names_them() {
        // DRM_MODE_CONNECTOR_* numbers (drm_mode.h) and the connector's id among its kind.
        assert_eq!(connector_name(10, 3), "DP-3");
        assert_eq!(connector_name(11, 1), "HDMI-A-1");
        assert_eq!(connector_name(14, 1), "eDP-1");
        assert_eq!(connector_name(1, 1), "VGA-1");
        assert_eq!(connector_name(3, 2), "DVI-D-2");
        assert_eq!(connector_name(7, 1), "LVDS-1");
        assert_eq!(connector_name(15, 1), "Virtual-1");
        assert_eq!(connector_name(16, 1), "DSI-1");
        assert_eq!(connector_name(20, 1), "USB-1");
        assert_eq!(connector_name(0, 1), "Unknown-1");
        assert_eq!(connector_name(99, 2), "Unknown-2", "a kind newer than this table");
    }

    #[test]
    fn configure_sizes_are_never_zero() {
        // xdg-shell reads 0 as "pick your own size"; a slot never asks that, however small an output gets.
        assert_eq!(configure_size(Rect { x: 0, y: 0, w: 0, h: 0 }), (1, 1));
        assert_eq!(
            configure_size(Rect {
                x: 5,
                y: 5,
                w: -3,
                h: 7
            }),
            (1, 7)
        );
        assert_eq!(
            configure_size(Rect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720
            }),
            (1280, 720)
        );
    }

    #[test]
    fn size_parses() {
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert_eq!(parse_size("1920X1080"), Ok((1920, 1080)));
    }

    #[test]
    fn size_refuses_nonsense() {
        for bad in ["", "1280", "x720", "0x720", "1280x-1", "axb", "99999x10"] {
            assert!(parse_size(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn output_layout_by_rule_then_first() {
        let c = config::default_config();
        assert_eq!(layout_for_output(&c, "winit").map(|l| l.name), Some("wall".into()));
        assert_eq!(
            layout_for_output(&c, "headless").map(|l| l.name),
            c.layouts.first().map(|l| l.name.clone())
        );
    }

    #[test]
    fn config_missing_is_default_broken_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.kdl");
        let l = load_config(Some(&missing));
        assert_eq!(l.config, config::default_config());
        assert!(l.error.is_none() && l.fallback);

        let broken = dir.path().join("broken.kdl");
        std::fs::write(&broken, "layout {").unwrap();
        let l = load_config(Some(&broken));
        assert_eq!(l.config, config::default_config());
        assert!(l.fallback && l.error.is_some_and(|e| e.contains("broken.kdl")));

        let good = dir.path().join("good.kdl");
        std::fs::write(&good, config::DEFAULT).unwrap();
        let l = load_config(Some(&good));
        assert!(!l.fallback && l.error.is_none());
    }

    #[test]
    fn reload_takes_layouts_by_name_and_keeps_a_vanished_one() {
        let c = config::default_config();
        let wall = c.layouts.iter().find(|l| l.name == "wall").unwrap().clone();
        let halves = c.layouts.iter().find(|l| l.name == "halves").unwrap().clone();
        let area = Rect {
            x: 0,
            y: 0,
            w: 1200,
            h: 900,
        };
        let mut s = Stacks::new();
        s.add_monitor("a", area, wall.clone());
        s.add_monitor("b", area, halves.clone());
        s.open_stacked(1);
        // Hidden on a: 3 on wall, 4 on halves.
        s.workspace(3);
        s.open_stacked(2);
        s.workspace(4);
        s.set_layout("a", halves.clone());
        s.open_stacked(3);
        s.workspace(1);

        // Unchanged layouts: nothing moves, nothing kept.
        assert!(reapply_layouts(&mut s, &c).is_empty());
        assert_eq!(s.shown(0).unwrap().layout(), &wall);

        // wall redefined, halves gone.
        let mut wide = wall.clone();
        wide.cols = 2;
        wide.rows = 1;
        wide.slots = halves.slots.clone();
        let new = Config {
            layouts: vec![wide.clone()],
            monitors: vec![],
            binds: vec![],
            ..c
        };
        let kept = [(2, "halves".to_owned()), (4, "halves".to_owned())];
        assert_eq!(reapply_layouts(&mut s, &new), kept);
        let layouts: Vec<(u8, &str)> = (s.workspaces().iter())
            .map(|w| (w.number(), w.layout().name.as_str()))
            .collect();
        assert_eq!(layouts, [(1, "wall"), (2, "halves"), (3, "wall"), (4, "halves")]);
        assert_eq!(s.shown(0).unwrap().layout(), &wide);
        assert_eq!(s.workspaces()[2].layout(), &wide, "hidden, redefined all the same");
        assert_eq!(s.shown(1).unwrap().layout(), &halves);
        // Defaults follow too; a vanished one is kept.
        assert_eq!(s.monitors()[0].default_layout(), &wide);
        assert_eq!(s.monitors()[1].default_layout(), &halves);
        let shown: Vec<(usize, WinId)> = s.placements().filter(|p| p.shown).map(|p| (p.monitor, p.win)).collect();
        assert_eq!(shown, [(0, 1)]);
    }

    #[test]
    fn a_toggled_stacking_survives_a_reload_unless_the_config_changes_it() {
        let on = config::default_config();
        let off = Config {
            stacking: false,
            ..on.clone()
        };
        // Toggled off at runtime: an unchanged config keeps it off, and the other way round.
        assert!(!stacking_after_reload(false, &on, &on));
        assert!(stacking_after_reload(true, &off, &off));
        // The config's own value changed: it wins, whatever the runtime said.
        assert!(!stacking_after_reload(true, &on, &off));
        assert!(stacking_after_reload(false, &off, &on));
    }

    #[test]
    fn read_config_names_the_file_and_the_place() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_config(None), Ok(config::default_config()));
        assert_eq!(
            read_config(Some(&dir.path().join("none.kdl"))),
            Ok(config::default_config())
        );
        let broken = dir.path().join("broken.kdl");
        std::fs::write(&broken, "mod \"Alt\"\nbogus 1\n").unwrap();
        let err = read_config(Some(&broken)).unwrap_err();
        assert!(
            err.contains(&format!("{}:2:1: unknown node", broken.display())),
            "{err}"
        );
    }

    #[test]
    fn the_config_file_must_be_a_regular_file_of_at_most_1_mib() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo.kdl");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // Safety: a valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Read on the event loop: a FIFO must not wait for a writer.
        let err = read_config_file(&fifo).config.unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        let big = dir.path().join("big.kdl");
        std::fs::write(&big, format!("// {}\n", "x".repeat(config::MAX_SOURCE))).unwrap();
        let err = read_config_file(&big).config.unwrap_err();
        assert!(err.contains("1 MiB") && err.contains("big.kdl"), "{err}");
    }

    #[test]
    fn the_watcher_watches_included_files_too() {
        let dir = tempfile::tempdir().unwrap();
        let (main, inc, gone) = (
            dir.path().join("c.kdl"),
            dir.path().join("i.kdl"),
            dir.path().join("g.kdl"),
        );
        std::fs::write(&main, format!("{}include \"i.kdl\"\n", config::DEFAULT)).unwrap();
        std::fs::write(&inc, "spawn-at-start \"bar\"\n").unwrap();
        let l = load_config(Some(&main));
        assert_eq!(l.config.spawn_at_start[0].argv, ["bar"], "{:?}", l.error);
        assert_eq!(l.files, [main.clone(), inc.clone()]);
        // A missing include is watched for, so that creating it reloads.
        std::fs::write(&main, format!("{}include \"g.kdl\"\n", config::DEFAULT)).unwrap();
        let read = read_config_file(&main);
        assert!(read.config.is_err_and(|e| e.contains("g.kdl")));
        assert_eq!(read.files, [main.clone(), gone.clone()]);
        // No file at all: the path alone.
        assert_eq!(read_config_file(&gone).files, std::slice::from_ref(&gone));

        // Stamps taken before a read stay; a file new to the list is stamped as it is now.
        let before = config_stamps(&[main.clone(), gone.clone()]);
        assert!(before[0].1.is_some() && before[1].1.is_none());
        let then = (SystemTime::UNIX_EPOCH, 1);
        let before = vec![(main.clone(), Some(then))];
        let after = restamp(&before, &[main.clone(), inc.clone()]);
        assert_eq!(after[0], (main, Some(then)));
        assert_eq!(after[1], config_stamps(&[inc])[0]);
    }

    #[test]
    fn monitor_rules_pick_each_monitors_layout() {
        let c = config::default_config();
        let area = Rect {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        };
        let halves = c.layouts.iter().find(|l| l.name == "halves").unwrap().clone();
        let one = Layout {
            name: "one".into(),
            cols: 1,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![crate::layout::SlotSpec {
                name: None,
                col: 0,
                row: 0,
                cols: 1,
                rows: 1,
            }],
        };
        let mut s = Stacks::new();
        s.add_monitor("winit", area, halves.clone());
        s.add_monitor("other", area, halves.clone());
        s.open_stacked(1);
        // Hidden on winit: 3 still on the old default, 4 on a layout of its own.
        s.workspace(3);
        s.open_stacked(2);
        s.workspace(4);
        s.set_layout("winit", one);
        s.open_stacked(3);
        s.workspace(1);
        apply_monitor_rules(&mut s, &c);
        assert_eq!(s.shown(0).unwrap().layout().name, "wall", "the winit rule");
        assert_eq!(s.shown(1).unwrap().layout().name, "wall", "no rule: the first layout");
        let layouts: Vec<(u8, &str)> = (s.workspaces().iter())
            .map(|w| (w.number(), w.layout().name.as_str()))
            .collect();
        assert_eq!(layouts, [(1, "wall"), (2, "wall"), (3, "wall"), (4, "one")]);
        let defaults: Vec<&str> = s.monitors().iter().map(|m| m.default_layout().name.as_str()).collect();
        assert_eq!(defaults, ["wall", "wall"]);
        let shown: Vec<(usize, WinId)> = s.placements().filter(|p| p.shown).map(|p| (p.monitor, p.win)).collect();
        assert_eq!(shown, [(0, 1)]);
    }
}
