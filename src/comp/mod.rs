//! The compositor: Smithay state, the event loop, and `arrange`, the one place where the core's placements become
//! Wayland state. Every decision about where a window goes is made by `Stacks`; this layer only maps, unmaps and
//! configures what `placements()` says.

mod backend_headless;
mod backend_winit;
mod frames;
mod handlers;
mod input;
pub mod ipc_server;
mod render;
mod spawn;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::pointer::MotionEvent;
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{IsAlive, Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::{CompositorClientState, CompositorState, with_states};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{XdgShellState, XdgToplevelSurfaceData};
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use crate::NAME;
use crate::config::{self, Config};
use crate::ipc::{Request, Response, StateView};
use crate::layout::{Layout, Rect};
use crate::stacks::{Stacks, WinId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    /// A window inside the running Wayland session.
    Winit,
    /// A virtual output with no rendering and no input, for tests.
    Headless,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub backend: Backend,
    /// None: `$XDG_CONFIG_HOME/orrery/config.kdl` (or `~/.config/...`), the built-in config when missing.
    pub config: Option<PathBuf>,
    /// The headless output's size; the winit output follows its window.
    pub size: (i32, i32),
    /// The control socket. None: `$XDG_RUNTIME_DIR/orrery.sock`.
    pub socket: Option<PathBuf>,
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

/// Reads and parses the config file at `path`; None when there is no file. The error names the file and lists
/// every problem with its line:col.
pub fn read_config_file(path: &Path) -> Result<Option<Config>, String> {
    match std::fs::read_to_string(path) {
        Ok(src) => config::parse(&src)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// `read_config_file` as at startup: no path, or no file there, is the built-in config.
pub fn read_config(path: Option<&Path>) -> Result<Config, String> {
    let Some(path) = path else {
        return Ok(config::default_config());
    };
    read_config_file(path).map(|c| c.unwrap_or_else(config::default_config))
}

/// The config to start with: the file's, or the built-in one when there is no file or it is broken (a broken
/// or unreadable file must not stop orrery from starting). Also why the file was not used, when broken (shown
/// in `state` and as the red bar), and whether this is the built-in fallback (`Loaded::fallback`).
pub fn load_config(path: Option<&Path>) -> Loaded {
    let fallback = |error| Loaded {
        config: config::default_config(),
        error,
        fallback: true,
    };
    let Some(path) = path else {
        return fallback(None);
    };
    match read_config_file(path) {
        Ok(Some(config)) => Loaded {
            config,
            error: None,
            fallback: false,
        },
        Ok(None) => fallback(None),
        Err(msg) => {
            tracing::error!("{msg}; using the built-in config");
            fallback(Some(msg))
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
}

/// What the config watcher compares: modification time and size (no inotify). None when there is no file.
pub fn config_stamp(path: Option<&Path>) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path?).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The first good config after a fallback start: each monitor takes the layout its monitor rule (or the first
/// layout) names, as if orrery had started with this config. Stacks follow `set_layout`'s rules.
pub fn apply_monitor_rules(stacks: &mut Stacks, config: &Config) {
    let names: Vec<String> = stacks.monitors().iter().map(|m| m.name().to_owned()).collect();
    for name in names {
        let current = stacks
            .monitors()
            .iter()
            .find(|m| m.name() == name)
            .map(|m| m.layout().clone());
        if let Some(layout) = layout_for_output(config, &name)
            && current.as_ref() != Some(&layout)
        {
            stacks.set_layout(&name, layout);
        }
    }
}

/// After a reload, each monitor takes the new definition of its layout, by name. A monitor whose layout the new
/// config no longer has keeps the one it shows (not an error); those are returned as (monitor, layout).
pub fn reapply_layouts(stacks: &mut Stacks, config: &Config) -> Vec<(String, String)> {
    let mut kept = Vec::new();
    let current: Vec<(String, Layout)> = stacks
        .monitors()
        .iter()
        .map(|m| (m.name().to_owned(), m.layout().clone()))
        .collect();
    for (monitor, layout) in current {
        match config.layouts.iter().find(|l| l.name == layout.name) {
            // Unchanged: nothing to do (set_layout would keep the stacks anyway, but this is plainly a no-op).
            Some(new) if *new == layout => {}
            Some(new) => stacks.set_layout(&monitor, new.clone()),
            None => kept.push((monitor, layout.name)),
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

    pub space: Space<Window>,
    pub popups: PopupManager,
    /// Every live toplevel, mapped or not; `Stacks` knows them by id only.
    pub windows: HashMap<WinId, Window>,
    /// Toplevel surface -> id, so commits find their window without a scan.
    surfaces: HashMap<WlSurface, WinId>,
    /// Windows that have committed a buffer and so are in the stacks. The rest wait, configured to the slot they
    /// would open into, without hiding anything or taking focus.
    opened: HashSet<WinId>,
    /// A real pointer has moved over orrery. Until then (always, headless) the pointer has no position, so no
    /// surface gets pointer focus from arrange.
    pub pointer_seen: bool,
    /// The surface (and its origin) the pointer was last pointed at by arrange or real motion.
    pub pointer_target: Option<(WlSurface, Point<f64, Logical>)>,
    /// Keys held for binds (swallowed, show-tabs).
    pub held: input::Held,
    /// The last key press, for `state` (see `input::key_note`).
    pub last_key: Option<crate::ipc::KeyView>,
    /// Spawned programs not yet reaped.
    children: Vec<std::process::Child>,
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
    /// The config file as last seen by the watcher.
    config_stamp: Option<(SystemTime, u64)>,
    /// The running config is the built-in stand-in for a missing or broken file (see `Loaded::fallback`).
    config_is_fallback: bool,
    pub outputs: Vec<Output>,
    /// How long frames take to draw (backends with a renderer record them).
    frames: frames::FrameStats,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
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
    fn new(
        event_loop: &mut EventLoop<'static, Orrery>,
        display: Display<Orrery>,
        config_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        // The stamp first: an edit landing while the file is read is then seen again by the watcher.
        let config_stamp = config_stamp(config_path.as_deref());
        let Loaded {
            config,
            error: config_error,
            fallback: config_is_fallback,
        } = load_config(config_path.as_deref());
        let dh = display.handle();
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), 200, 25)
            .context("cannot set up the keyboard (is xkeyboard-config installed?)")?;
        seat.add_pointer();

        let socket_name = listen(display, event_loop)?;

        Ok(Self {
            start_time: Instant::now(),
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            space: Space::default(),
            popups: PopupManager::default(),
            windows: HashMap::new(),
            surfaces: HashMap::new(),
            opened: HashSet::new(),
            pointer_seen: false,
            pointer_target: None,
            held: input::Held::default(),
            last_key: None,
            children: Vec::new(),
            next_id: 1,
            stacks: Stacks::new(),
            stacking: config.stacking,
            config,
            config_error,
            config_path,
            config_stamp,
            config_is_fallback,
            outputs: Vec::new(),
            frames: frames::FrameStats::new(Instant::now()),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
        })
    }

    /// Puts an output in the space at `area` and gives it a monitor in the stacks.
    pub fn add_output(&mut self, output: Output, area: Rect) {
        self.space.map_output(&output, (area.x, area.y));
        if let Some(layout) = layout_for_output(&self.config, &output.name()) {
            self.stacks.add_monitor(&output.name(), area, layout);
        }
        self.outputs.push(output);
        self.arrange();
    }

    /// An output changed size: the monitor's slots follow.
    pub fn resize_output(&mut self, name: &str, area: Rect) {
        self.stacks.set_area(name, area);
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

    /// A toplevel's own surface committed. The first buffer opens it on top of the focused slot's current stack,
    /// or with stacking off as a new tab there; a null buffer (unmap) takes it out of the stacks and back to
    /// waiting.
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
            if self.stacking {
                self.stacks.open_stacked(id);
            } else {
                self.stacks.open_tab(id, None, None);
            }
            self.arrange();
        }
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

    /// Makes Wayland match the stacks, in one pass over `placements()`: the top of a slot's current tab is mapped
    /// at the slot's rectangle; every other window is unmapped but stays alive. Both are configured to the slot's
    /// size (so showing one later needs no round trip), tiled on every edge, activated only when focused. Waiting
    /// windows (no buffer yet) are unmapped and configured to where they would open (the focused slot). Opened
    /// windows in no slot (pending without a monitor) are unmapped and keep their size; they are neither drawn
    /// nor focused. Keyboard and pointer focus then follow.
    pub fn arrange(&mut self) {
        let mut placed = HashSet::with_capacity(self.windows.len());
        for p in self.stacks.placements() {
            let Some(window) = self.windows.get(&p.win) else {
                continue;
            };
            placed.insert(p.win);
            configure(window, p.rect, p.focused);
            if p.on_top {
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

    fn update_keyboard_focus(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let want = (self.stacks.focused_window())
            .and_then(|id| self.windows.get(&id))
            .and_then(|w| w.toplevel())
            .map(|t| t.wl_surface().clone());
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
        let time = u32::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u32::MAX);
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

    /// Per-frame housekeeping shared by the backends: forget dead windows (arranging once), refresh the space
    /// (output enter and leave), clean popups.
    pub fn refresh(&mut self) {
        let dead: Vec<WinId> = self
            .windows
            .iter()
            .filter(|(_, w)| !w.alive())
            .map(|(&id, _)| id)
            .collect();
        if !dead.is_empty() {
            for id in dead {
                self.forget_window(id);
            }
            self.arrange();
        }
        self.space.refresh();
        self.popups.cleanup();
    }

    /// Frame callbacks for every window on `output`, so clients keep drawing.
    pub fn send_frames(&self, output: &Output) {
        let now = self.start_time.elapsed();
        for window in self.space.elements() {
            window.send_frame(output, now, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    /// One request, from the socket or a key. Pure requests go through the stacks and are arranged.
    pub fn handle_request(&mut self, request: Request) -> Response {
        match request {
            Request::State => Response::with_state(self.state_view()),
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
            pure => match pure.apply(&mut self.stacks, &self.config) {
                Ok(()) => {
                    self.arrange();
                    Response::ok()
                }
                Err(e) => Response::error(e),
            },
        }
    }

    /// Reads the config file again. A good one replaces the config (binds, colours, layouts: each monitor takes
    /// its layout's new definition by name, or after a fallback start its monitor rule; `stacking` as
    /// `stacking_after_reload` says) and clears `config_error`; a broken one leaves everything as it was and sets
    /// `config_error`, which shows as the red bar until a good config loads. A file gone while running (an editor
    /// between delete and write, say) keeps the running config: the built-in one stands in only at startup. That
    /// is an error answer for `reload` but not a `config_error`.
    pub fn reload(&mut self) -> Result<(), String> {
        let Some(path) = self.config_path.clone() else {
            return Err("no config path (neither --config, XDG_CONFIG_HOME nor HOME): nothing to reload".into());
        };
        match read_config_file(&path) {
            Ok(None) => {
                let msg = format!("{} is gone; keeping the current config", path.display());
                tracing::warn!("{msg}");
                Err(msg)
            }
            Ok(Some(config)) => {
                if std::mem::take(&mut self.config_is_fallback) {
                    apply_monitor_rules(&mut self.stacks, &config);
                } else {
                    for (monitor, layout) in reapply_layouts(&mut self.stacks, &config) {
                        tracing::info!("the new config has no layout {layout:?}; monitor {monitor} keeps it");
                    }
                }
                self.stacking = stacking_after_reload(self.stacking, &self.config, &config);
                self.config = config;
                self.config_error = None;
                self.arrange();
                tracing::info!("config reloaded");
                Ok(())
            }
            Err(e) => {
                tracing::error!("{e}; keeping the old config");
                self.config_error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The watcher's tick: reload when the file's stamp changed since last seen (a broken file is not retried
    /// until it changes again).
    fn check_config_file(&mut self) {
        let stamp = config_stamp(self.config_path.as_deref());
        if stamp != self.config_stamp {
            self.config_stamp = stamp;
            // A failure is logged and kept in config_error by reload itself.
            let _ = self.reload();
        }
    }

    /// The `state` answer.
    pub fn state_view(&self) -> StateView {
        let mut view = StateView::new(&self.stacks, self.stacking, self.config_error.clone(), |id| {
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
        view.frames = self.frames.view();
        view.last_key = self.last_key.clone();
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
    let config_path = opts.config.clone().or_else(default_config_path);

    // First, before any socket of ours exists: a second orrery must refuse without side effects. Dropped (and
    // so removed) when run returns.
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

    let mut state = Orrery::new(&mut event_loop, display, config_path)?;

    event_loop
        .handle()
        .insert_source(Timer::from_duration(CONFIG_POLL), |_, _, state| {
            state.check_config_file();
            TimeoutAction::ToDuration(CONFIG_POLL)
        })
        .map_err(|e| anyhow::anyhow!("cannot watch the config file: {e}"))?;

    control.serve(&event_loop.handle())?;
    tracing::info!("control socket {}", control_path.display());

    match opts.backend {
        Backend::Winit => backend_winit::init(&mut event_loop, &mut state)?,
        Backend::Headless => backend_headless::init(&mut event_loop, &mut state, opts.size)?,
    }

    let Some(name) = state.socket_name.to_str().map(str::to_owned) else {
        bail!("the Wayland socket name is not UTF-8");
    };
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "WAYLAND_DISPLAY={name}").context("cannot write to stdout")?;
    stdout.flush().context("cannot write to stdout")?;
    drop(stdout);
    tracing::info!("listening on WAYLAND_DISPLAY={name}");

    event_loop
        .run(None, &mut state, |state| {
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

        // Unchanged layouts: nothing moves, nothing kept.
        assert!(reapply_layouts(&mut s, &c).is_empty());
        assert_eq!(s.monitors()[0].layout(), &wall);

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
        assert_eq!(reapply_layouts(&mut s, &new), [("b".to_owned(), "halves".to_owned())]);
        assert_eq!(s.monitors()[0].layout(), &wide);
        assert_eq!(s.monitors()[1].layout(), &halves);
        assert_eq!(s.placements().map(|p| (p.monitor, p.win)).collect::<Vec<_>>(), [(0, 1)]);
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
        assert!(err.starts_with(&format!("{}: ", broken.display())), "{err}");
        assert!(err.contains("2:1"), "{err}");
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
        let mut s = Stacks::new();
        s.add_monitor("winit", area, halves.clone());
        s.add_monitor("other", area, halves.clone());
        s.open_stacked(1);
        apply_monitor_rules(&mut s, &c);
        assert_eq!(s.monitors()[0].layout().name, "wall", "the winit rule");
        assert_eq!(s.monitors()[1].layout().name, "wall", "no rule: the first layout");
        assert_eq!(s.placements().map(|p| (p.monitor, p.win)).collect::<Vec<_>>(), [(0, 1)]);
    }
}
