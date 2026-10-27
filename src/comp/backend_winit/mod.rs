//! Nested: orrery as windows inside the running Wayland session, one window per screen. Screen k's output is
//! `winit` for 1, `winit-<k>` else (`screen_name`), sized to its window and placed right of the others in orrery's
//! global space; the host places the windows themselves, and moves the pointer between them.
//!
//! **Presenting only when the host asks.** eglSwapBuffers blocks inside Mesa (wl_display_dispatch_queue) until
//! the host releases a buffer or sends a frame callback, and a host sends neither to a hidden window (another
//! workspace, a scratchpad): one swap there stalled the whole loop, the control socket, the config watcher and
//! every client (seen in the first nested session; swap interval 0 alone did not help). So a window's only swap is
//! in winit's `RedrawRequested` for that window, and every submit goes with that window's `pre_present_notify`,
//! which asks the host for a frame callback; winit on Wayland holds the window's `RedrawRequested` back while that
//! callback is in flight. While a window is hidden its callback never comes, so that window simply stops drawing,
//! without blocking, and the others go on.
//!
//! The 16 ms calloop timer does everything else, hidden or not: housekeeping, client frame callbacks for every
//! output, and a `request_redraw` for every window (cheap; it just sets winit's flag). A redraw with no damage
//! submits nothing and so leaves no callback in flight, and the next tick's request is delivered again: idle costs
//! one damage check per window per tick.
//!
//! **One host connection, one GL context.** A single winit event loop serves every window, pumped from calloop
//! (as Smithay's own winit backend does). winit creates windows only inside its event handler, so `open-screen`
//! adds the output at once (its number taken, so a second request cannot get the same one) and queues the window,
//! which the next pump creates; a window that cannot be created takes its output away again. Creating one blocks
//! the loop until the host's first configure (milliseconds). A closed screen leaves no gap: the others are laid
//! side by side again in opening order, as on a resize.
//!
//! One EGL display (made from the event loop's connection, not a window: the display keeps what it was made from
//! alive, and closing that window would then leave it on the host) and one context and `GlesRenderer` serve every
//! window, so client textures are shared; each window has its own EGL surface, made current before its frame (EGL
//! reports the buffer age only for the current surface), with swap interval 0, and its own damage tracker.
//!
//! **Input away.** While the primary's input is on another machine (`Orrery::input_away`), the host's pointer is
//! hidden and held: locked in place (Wayland's pointer constraints; a window that cannot lock lets every lock go),
//! else confined to the window it is in, or the next of ours it enters (X11); windows opened meanwhile are held too.
//! Its relative motion (`DeviceEvent::MouseMotion`) is what moves the other machine's pointer. Here, relative motion
//! over a window only crosses: pushed past orrery's outer edges, input goes to the machine beside them. When input
//! comes back the pointer is placed where it came in (on the window holding the lock and while still locked, as
//! Wayland wants; confined, after letting go), let go and shown. Applied right after the input that moved input,
//! and by the frame timer for any other cause (a `workspace` from the control socket, a link drop). A lock takes hold
//! only while the host's pointer is in the window: crossing at a window edge with more host screen beside it, the
//! pointer may slip out first.
//!
//! None of this is reachable from the headless tests.

mod input;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Context;
use smithay::backend::egl::context::{GlAttributes, PixelFormatRequirements};
use smithay::backend::egl::native::{EGLNativeDisplay, EGLPlatform, XlibWindow};
use smithay::backend::egl::{EGLContext, EGLDisplay, EGLSurface, SwapBuffersError, ffi};
use smithay::backend::input::InputEvent;
use smithay::backend::renderer::Bind;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{self, EventLoop, EventSource, Interest, PostAction, Readiness, Token};
use smithay::reexports::winit::application::ApplicationHandler;
use smithay::reexports::winit::dpi::{LogicalSize, PhysicalPosition};
use smithay::reexports::winit::event::{DeviceEvent, DeviceId, ElementState, StartCause, WindowEvent};
use smithay::reexports::winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop as HostLoop, OwnedDisplayHandle};
use smithay::reexports::winit::platform::pump_events::{EventLoopExtPumpEvents as _, PumpStatus};
use smithay::reexports::winit::platform::scancode::PhysicalKeyExtScancode as _;
use smithay::reexports::winit::platform::wayland::WindowAttributesExtWayland as _;
use smithay::reexports::winit::raw_window_handle::{
    HasDisplayHandle as _, HasWindowHandle as _, RawDisplayHandle, RawWindowHandle,
};
use smithay::reexports::winit::window::{CursorGrabMode, Window as WinitWindow, WindowId};
use smithay::utils::{Clock, Monotonic, Physical, Rectangle, Size, Transform};
use wayland_egl::WlEglSurface;

use super::{Orrery, render, screen_name};
use crate::NAME;
use crate::layout::Rect;

/// How often housekeeping runs, clients get frame callbacks and redraws are requested: about 60 Hz.
const FRAME: Duration = Duration::from_millis(16);

/// A window's failed frames are logged at most this often: one that keeps failing would fill the log at 60 Hz.
const WARN_EVERY: Duration = Duration::from_secs(10);

/// Every screen's output is called this, numbered from the second on (`screen_name`).
const BASE: &str = "winit";

/// A new window's size until the host configures it.
const START_SIZE: (i32, i32) = (1280, 800);

pub fn init(event_loop: &mut EventLoop<'static, Orrery>, state: &mut Orrery) -> anyhow::Result<()> {
    let host_loop = HostLoop::builder()
        .build()
        .context("cannot connect to the host session")?;
    host_loop.set_control_flow(ControlFlow::Poll);
    let x11 = matches!(
        host_loop.display_handle().map(|h| h.as_raw()),
        Ok(RawDisplayHandle::Xlib(_))
    );
    let renderer = renderer(host_loop.owned_display_handle())?;
    let damage = renderer.egl_context().display().supports_damage();
    let host = Rc::new(RefCell::new(Host {
        renderer,
        damage,
        x11,
        windows: Table::new(),
        opening: Vec::new(),
        pointer_in: None,
        hold: Hold::Free,
    }));

    // The first window now, so a session that cannot open one fails to start rather than running blind.
    open_screen(&host, state, 1);
    let mut source = HostEvents {
        host: host.clone(),
        host_loop: Generic::new(host_loop, Interest::READ, calloop::Mode::Level),
        pending: Vec::new(),
        token: None,
        clock: Clock::new(),
        keys_down: 0,
    };
    source.pump();
    if host.borrow().windows.is_empty() {
        let why = source.pending.iter().find_map(|e| match e {
            HostEvent::Failed { error, .. } => Some(error.clone()),
            _ => None,
        });
        anyhow::bail!(
            "cannot open the nested window: {}",
            why.as_deref().unwrap_or("the host did not create it")
        );
    }
    state.screens = Some(Box::new(Screens { host: host.clone() }));

    let timer_host = host.clone();
    event_loop
        .handle()
        .insert_source(Timer::from_duration(FRAME), move |_, _, state| {
            state.refresh();
            for output in state.outputs.clone() {
                state.send_frames(&output);
            }
            // Never draws or swaps: winit delivers each window's redraw when the host is ready for its frame.
            for screen in timer_host.borrow().windows.values() {
                screen.window.request_redraw();
            }
            hold_pointer(&mut timer_host.borrow_mut(), state);
            TimeoutAction::ToDuration(FRAME)
        })
        .map_err(|e| anyhow::anyhow!("cannot start the frame timer: {e}"))?;
    event_loop
        .handle()
        .insert_source(source, move |event, _, state| handle(&host, state, event))
        .map_err(|e| anyhow::anyhow!("cannot watch the nested windows: {e}"))?;
    Ok(())
}

/// The EGL display (from the host connection, see the module doc), its context and the renderer every window
/// shares. 10-bit colour if the host offers it, else 8.
fn renderer(display: OwnedDisplayHandle) -> anyhow::Result<GlesRenderer> {
    // Safety: the connection outlives the display, which holds a handle on it.
    let display = unsafe { EGLDisplay::new(HostDisplay(display)) }.context("cannot open EGL on the host")?;
    let attributes = GlAttributes {
        version: (3, 0),
        profile: None,
        debug: cfg!(debug_assertions),
        vsync: false,
    };
    let context = EGLContext::new_with_config(&display, attributes, PixelFormatRequirements::_10_bit())
        .or_else(|_| EGLContext::new_with_config(&display, attributes, PixelFormatRequirements::_8_bit()))
        .context("cannot create an EGL context")?;
    // Safety: the context is new and used on this thread only.
    unsafe { GlesRenderer::new(context) }.context("cannot create the renderer")
}

/// The host connection as EGL's native display.
struct HostDisplay(OwnedDisplayHandle);

impl EGLNativeDisplay for HostDisplay {
    fn supported_platforms(&self) -> Vec<EGLPlatform<'_>> {
        let none = || vec![ffi::egl::NONE as ffi::EGLint];
        match self.0.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(h)) => {
                let display = h.display.as_ptr();
                vec![
                    EGLPlatform::new(
                        ffi::egl::PLATFORM_WAYLAND_KHR,
                        "PLATFORM_WAYLAND_KHR",
                        display,
                        none(),
                        &["EGL_KHR_platform_wayland"],
                    ),
                    EGLPlatform::new(
                        ffi::egl::PLATFORM_WAYLAND_EXT,
                        "PLATFORM_WAYLAND_EXT",
                        display,
                        none(),
                        &["EGL_EXT_platform_wayland"],
                    ),
                ]
            }
            Ok(RawDisplayHandle::Xlib(h)) => (h.display.into_iter())
                .flat_map(|display| {
                    let display = display.as_ptr();
                    [
                        EGLPlatform::new(
                            ffi::egl::PLATFORM_X11_KHR,
                            "PLATFORM_X11_KHR",
                            display,
                            none(),
                            &["EGL_KHR_platform_x11"],
                        ),
                        EGLPlatform::new(
                            ffi::egl::PLATFORM_X11_EXT,
                            "PLATFORM_X11_EXT",
                            display,
                            none(),
                            &["EGL_EXT_platform_x11"],
                        ),
                    ]
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// What every window shares, behind one `Rc<RefCell>` held by the event source (creating windows, routing their
/// events, drawing), the timer (redraw requests) and `Screens` (opening, closing). Borrowed only briefly, never
/// across a call into `Orrery` that could reach `Screens`.
struct Host {
    renderer: GlesRenderer,
    /// EGL can swap with damage; without, every frame is a full one (age 0).
    damage: bool,
    x11: bool,
    windows: Table<Screen>,
    /// Outputs added by `open-screen` whose windows the next pump creates.
    opening: Vec<Output>,
    /// The window the host's pointer is in, if one of ours.
    pointer_in: Option<WindowId>,
    /// While input is away, the host's pointer is hidden and held so.
    hold: Hold,
}

/// One host window and what drawing it keeps between frames.
struct Screen {
    // Declaration order is drop order: the EGL surface goes before the window it draws on.
    surface: EGLSurface,
    window: WinitWindow,
    /// The size the surface was last resized to.
    bind_size: Option<Size<i32, Physical>>,
    damage_tracker: OutputDamageTracker,
    decorations: render::Decorations,
    /// A frame was rendered but not submitted since the last submit (see `frame_age`).
    skipped_since_submit: bool,
    /// When a failed frame was last logged (see `warn_due`).
    warned: Option<Instant>,
}

/// Opening and closing nested screens: windows.
struct Screens {
    host: Rc<RefCell<Host>>,
}

impl super::Screens for Screens {
    /// Answers before the window exists (see the module doc); a window the host refuses is logged and its screen
    /// goes again.
    fn open(&mut self, state: &mut Orrery, number: u8) -> Result<(), String> {
        open_screen(&self.host, state, number);
        Ok(())
    }

    fn close(&mut self, state: &mut Orrery, name: &str) -> Result<(), String> {
        if close_screen(&self.host, state, name) {
            Ok(())
        } else {
            Err(format!("no screen {name:?}"))
        }
    }
}

/// Screen `number`: its output now, right of the others, its window at the next pump.
fn open_screen(host: &RefCell<Host>, state: &mut Orrery, number: u8) {
    let name = screen_name(BASE, number);
    let output = Output::new(
        name,
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: NAME.into(),
            model: BASE.into(),
        },
    );
    let (w, h) = START_SIZE;
    let mode = Mode {
        size: (w, h).into(),
        refresh: 60_000,
    };
    let x = state.next_screen_x();
    // GL draws bottom-up; the window's surface wants it flipped (as in Smithay's smallvil).
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((x, 0).into()));
    output.set_preferred(mode);
    state.add_output(output.clone(), Rect { x, y: 0, w, h });
    host.borrow_mut().opening.push(output);
}

/// Closes the window of output `name` (or drops it from the queue, not yet created) and takes the screen away; the
/// others close the gap. The last one closed quits. False when there is no such window.
fn close_screen(host: &RefCell<Host>, state: &mut Orrery, name: &str) -> bool {
    let (closed, last) = {
        let mut host = host.borrow_mut();
        let screen = host.windows.remove_named(name);
        // Off the context first: a surface still current would outlive its host window inside EGL.
        if screen.as_ref().is_some_and(|s| s.surface.is_current())
            && let Err(e) = host.renderer.egl_context().unbind()
        {
            tracing::warn!("cannot release the closed window's EGL surface: {e}");
        }
        let closed = screen.is_some() || unqueue(&mut host.opening, name);
        drop(screen);
        (closed, host.windows.is_empty() && host.opening.is_empty())
    };
    if closed {
        remove_screen(state, name);
        if last {
            tracing::info!("the last nested window closed");
            state.loop_signal.stop();
        }
    }
    closed
}

/// Takes output `name` out of the queue of windows to create; false when it is not there.
fn unqueue(opening: &mut Vec<Output>, name: &str) -> bool {
    let before = opening.len();
    opening.retain(|o| o.name() != name);
    opening.len() != before
}

/// Takes output `name` away and lays the others side by side again.
fn remove_screen(state: &mut Orrery, name: &str) {
    state.remove_output(name);
    lay_side_by_side(state, None);
}

/// What the pump saw, kept until the event source can hand it to `handle` with the state.
enum HostEvent {
    Resized(WindowId, Size<i32, Physical>),
    Redraw(WindowId),
    Close(WindowId),
    Focus(WindowId, bool),
    Input(WindowId, InputEvent<input::Nested>),
    /// The host pointer's relative motion, over one of our windows or held by one.
    Relative(f64, f64),
    /// A queued window could not be created.
    Failed {
        output: String,
        error: String,
    },
}

fn handle(host: &RefCell<Host>, state: &mut Orrery, event: HostEvent) {
    let name_of = |id| host.borrow().windows.name_of(id).map(str::to_owned);
    match event {
        // The scale factor is ignored: orrery runs at scale 1 for now.
        HostEvent::Resized(id, size) => {
            // A minimised or mid-resize window can report an empty size; slots need a real area.
            if let Some(name) = name_of(id)
                && size.w > 0
                && size.h > 0
            {
                resize_screen(state, &name, size);
            }
        }
        // The host is ready for this window's frame (or none is in flight): the only place that swaps.
        HostEvent::Redraw(id) => draw(&mut host.borrow_mut(), id, state),
        HostEvent::Close(id) => {
            if let Some(name) = name_of(id) {
                close_screen(host, state, &name);
            }
        }
        // Releases while unfocused never arrive: release what is down (or Alt would stick in the seat and in
        // clients) and forget held binds (or show-tabs would stick on).
        HostEvent::Focus(_, false) => state.keyboard_focus_lost(),
        // The host focused this window: its screen is the focused one, as on a click into it.
        HostEvent::Focus(id, true) => {
            if let Some(name) = name_of(id) {
                state.stacks.focus_monitor(&name);
                state.arrange();
            }
        }
        // Input may have gone away or come back: the host's pointer is held or let go at once, not a frame later.
        HostEvent::Input(id, event) => {
            if let Some(name) = name_of(id) {
                state.process_input_event(&name, event);
            }
            hold_pointer(&mut host.borrow_mut(), state);
        }
        // The pointer moves by positions (`Input`): relative motion only crosses here, or goes to the machine away.
        HostEvent::Relative(dx, dy) => {
            state.device_motion(dx, dy, false);
            hold_pointer(&mut host.borrow_mut(), state);
        }
        HostEvent::Failed { output, error } => {
            tracing::warn!("cannot open a nested window for {output}: {error}");
            let last = {
                let host = host.borrow();
                host.windows.is_empty() && host.opening.is_empty()
            };
            remove_screen(state, &output);
            if last {
                state.loop_signal.stop();
            }
        }
    }
}

/// Output `name` follows its window's new size; every screen is laid side by side again (`lay_side_by_side`).
fn resize_screen(state: &mut Orrery, name: &str, size: Size<i32, Physical>) {
    if let Some(output) = state.outputs.iter().find(|o| o.name() == name) {
        output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
    }
    lay_side_by_side(state, Some(name));
}

/// Every screen side by side again in opening order, so a wider window never overlaps the next one in global space
/// (where pointer positions are hit-tested) and a closed one leaves no gap. The monitors of the screens that moved
/// follow, and that of `resized` (its size changed) even where it stayed.
fn lay_side_by_side(state: &mut Orrery, resized: Option<&str>) {
    let sizes: Vec<_> = (state.outputs.iter())
        .map(|o| o.current_mode().map_or((0, 0), |m| (m.size.w, m.size.h)))
        .collect();
    let xs = side_by_side(&sizes.iter().map(|&(w, _)| w).collect::<Vec<_>>());
    for ((output, (w, h)), x) in state.outputs.clone().into_iter().zip(sizes).zip(xs) {
        let moved = output.current_location().x != x;
        if moved {
            output.change_current_state(None, None, None, Some((x, 0).into()));
            state.space.map_output(&output, (x, 0));
        }
        if moved || resized == Some(output.name().as_str()) {
            state.resize_output(&output.name(), Rect { x, y: 0, w, h });
        }
    }
}

/// One frame of window `id`, on its `RedrawRequested` only (see the module doc): render, and submit (swap) only
/// real damage. A failed frame is logged and the next one tried; the session keeps running.
fn draw(host: &mut Host, id: WindowId, state: &mut Orrery) {
    let Host {
        renderer,
        damage: swaps_damage,
        windows,
        ..
    } = host;
    let Some((name, screen)) = windows.get_mut(id) else {
        return;
    };
    // Resized before it is made current: Mesa latches the back buffer when a surface is made current.
    let size = window_size(&screen.window);
    if screen.bind_size != Some(size) {
        screen.surface.resize(size.w, size.h, 0, 0);
        screen.bind_size = Some(size);
    }
    // Safety: one thread, and the context is never moved off it.
    let current = unsafe { renderer.egl_context().make_current_with_surface(&screen.surface) };
    let buffer_age = match current {
        Ok(()) if *swaps_damage => screen.surface.buffer_age().and_then(|a| usize::try_from(a).ok()),
        _ => None,
    };
    let age = frame_age(buffer_age, screen.skipped_since_submit);
    let started = Instant::now();
    // Copied out so the framebuffer's borrow of the surface ends before submit.
    let damage = match renderer.bind(&mut screen.surface) {
        Ok(mut framebuffer) => render::render_output(
            name,
            state,
            renderer,
            &mut framebuffer,
            &mut screen.damage_tracker,
            &mut screen.decorations,
            age,
        )
        .map(|r| r.damage.cloned()),
        Err(e) => Err(anyhow::anyhow!("cannot bind the window: {e}")),
    };
    let took = started.elapsed();
    match damage {
        Ok(Some(damage)) if !damage.is_empty() => match submit(screen, *swaps_damage, &damage) {
            Ok(()) => {
                screen.skipped_since_submit = false;
                // Only frames that went to the screen count: idle damage checks would make the times look tiny.
                state.frame_drawn(took);
            }
            // The frame callback `pre_present_notify` asked for went out with no commit, so winit holds this
            // window's redraws back for good.
            Err(e) => tracing::error!("{name}: presenting the frame: {e}; this window will stop drawing"),
        },
        Ok(_) => screen.skipped_since_submit = true,
        Err(e) => {
            let now = Instant::now();
            if warn_due(screen.warned, now) {
                screen.warned = Some(now);
                tracing::warn!("{name}: {e:#} (logged at most every {} s)", WARN_EVERY.as_secs());
            }
        }
    }
}

/// Whether a failed frame is logged, the last one having been logged at `last`.
fn warn_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= WARN_EVERY)
}

/// Swaps the window's surface (current since `draw`), with `pre_present_notify` first: it asks the host for the
/// frame callback that holds the next `RedrawRequested` back.
fn submit(screen: &Screen, swaps_damage: bool, damage: &[Rectangle<i32, Physical>]) -> Result<(), SwapBuffersError> {
    let height = screen.bind_size.map_or(0, |s| s.h);
    let mut damage = swaps_damage.then(|| flipped(damage, height));
    screen.window.pre_present_notify();
    screen.surface.swap_buffers(damage.as_deref_mut())
}

fn window_size(window: &WinitWindow) -> Size<i32, Physical> {
    let (w, h): (i32, i32) = window.inner_size().into();
    (w, h).into()
}

/// Calloop's view of the host connection: pumps winit (creating queued windows, collecting events) before calloop
/// sleeps and when the connection is readable, and hands what it collected to the callback.
struct HostEvents {
    // Shared with the timer and `Screens`, so this field's order does not decide when windows and EGL go; any order
    // is safe: the EGL display owns a display handle and every window holds the connection, which outlives them.
    host: Rc<RefCell<Host>>,
    host_loop: Generic<HostLoop<()>>,
    pending: Vec<HostEvent>,
    /// Lets `before_sleep` wake `process_events` when the pump left events behind.
    token: Option<Token>,
    clock: Clock<Monotonic>,
    keys_down: u32,
}

impl HostEvents {
    fn pump(&mut self) -> PumpStatus {
        // Safety: the event loop is never dropped or replaced through this reference.
        let host_loop = unsafe { self.host_loop.get_mut() };
        let mut host = self.host.borrow_mut();
        host_loop.pump_app_events(
            Some(Duration::ZERO),
            &mut Pump {
                host: &mut host,
                events: &mut self.pending,
                clock: &self.clock,
                keys_down: &mut self.keys_down,
            },
        )
    }
}

impl EventSource for HostEvents {
    type Event = HostEvent;
    type Metadata = ();
    type Ret = ();
    type Error = std::io::Error;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        // Drained before sleeping, so events another reader of the connection queued are not left waiting, and
        // windows queued by `open-screen` get created.
        self.pump();
        Ok(self
            .token
            .filter(|_| !self.pending.is_empty())
            .map(|t| (Readiness::EMPTY, t)))
    }

    fn process_events<F>(&mut self, _: Readiness, _: Token, mut callback: F) -> std::io::Result<PostAction>
    where
        F: FnMut(HostEvent, &mut ()),
    {
        for event in std::mem::take(&mut self.pending) {
            callback(event, &mut ());
        }
        let status = self.pump();
        for event in std::mem::take(&mut self.pending) {
            callback(event, &mut ());
        }
        Ok(match status {
            PumpStatus::Continue => PostAction::Continue,
            PumpStatus::Exit(_) => PostAction::Remove,
        })
    }

    fn register(&mut self, poll: &mut calloop::Poll, factory: &mut calloop::TokenFactory) -> calloop::Result<()> {
        self.token = Some(factory.token());
        self.host_loop.register(poll, factory)
    }

    fn reregister(&mut self, poll: &mut calloop::Poll, factory: &mut calloop::TokenFactory) -> calloop::Result<()> {
        self.host_loop.reregister(poll, factory)
    }

    fn unregister(&mut self, poll: &mut calloop::Poll) -> calloop::Result<()> {
        self.host_loop.unregister(poll)
    }
}

/// winit's handler for one pump: creates queued windows and turns window events into `HostEvent`s.
struct Pump<'a> {
    host: &'a mut Host,
    events: &'a mut Vec<HostEvent>,
    clock: &'a Clock<Monotonic>,
    keys_down: &'a mut u32,
}

impl Pump<'_> {
    fn time(&self) -> u64 {
        self.clock.now().as_micros()
    }

    /// Creates every queued window (winit allows it only here, with the active loop).
    fn open_windows(&mut self, active: &ActiveEventLoop) {
        for output in std::mem::take(&mut self.host.opening) {
            match open_window(active, self.host, &output) {
                Ok(screen) => {
                    let id = screen.window.id();
                    // Opened while input is away: held as the others are.
                    if self.host.hold != Hold::Free {
                        screen.window.set_cursor_visible(false);
                        if self.host.hold == Hold::Locked
                            && let Err(e) = screen.window.set_cursor_grab(CursorGrabMode::Locked)
                        {
                            tracing::warn!("cannot lock the host's pointer in the new window: {e}");
                        }
                    }
                    // The output was added at the start size; the window may already be another.
                    self.events.push(HostEvent::Resized(id, window_size(&screen.window)));
                    self.host.windows.insert(id, output.name(), screen);
                }
                Err(e) => self.events.push(HostEvent::Failed {
                    output: output.name(),
                    error: format!("{e:#}"),
                }),
            }
        }
    }
}

impl ApplicationHandler for Pump<'_> {
    fn new_events(&mut self, active: &ActiveEventLoop, _: StartCause) {
        self.open_windows(active);
    }

    fn resumed(&mut self, active: &ActiveEventLoop) {
        self.open_windows(active);
    }

    fn about_to_wait(&mut self, active: &ActiveEventLoop) {
        self.open_windows(active);
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        // Only over our windows (X11 reports it wherever the pointer is while a window has focus), or held.
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
            && (self.host.hold != Hold::Free || self.host.pointer_in.is_some())
        {
            self.events.push(HostEvent::Relative(dx, dy));
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let time = self.time();
        match event {
            WindowEvent::CursorEntered { .. } | WindowEvent::CursorMoved { .. } => {
                self.host.pointer_in = Some(id);
                // Held but not yet confined (the pointer was outside our windows): confined to this one now.
                if self.host.hold == Hold::Confined(None) {
                    self.host.hold = Hold::Confined(confine(self.host, id).then_some(id));
                }
            }
            WindowEvent::CursorLeft { .. } if self.host.pointer_in == Some(id) => self.host.pointer_in = None,
            _ => {}
        }
        let event = match event {
            WindowEvent::Resized(size) => {
                let (w, h): (i32, i32) = size.into();
                HostEvent::Resized(id, (w, h).into())
            }
            WindowEvent::ScaleFactorChanged { .. } => match self.host.windows.get_mut(id) {
                Some((_, screen)) => HostEvent::Resized(id, window_size(&screen.window)),
                None => return,
            },
            WindowEvent::RedrawRequested => HostEvent::Redraw(id),
            WindowEvent::CloseRequested => HostEvent::Close(id),
            WindowEvent::Focused(focused) => HostEvent::Focus(id, focused),
            WindowEvent::KeyboardInput {
                event, is_synthetic, ..
            } if !is_synthetic && !event.repeat => {
                *self.keys_down = match event.state {
                    ElementState::Pressed => self.keys_down.saturating_add(1),
                    ElementState::Released => self.keys_down.saturating_sub(1),
                };
                HostEvent::Input(
                    id,
                    InputEvent::Keyboard {
                        event: input::Key {
                            time,
                            scancode: event.physical_key.to_scancode().unwrap_or(0),
                            count: *self.keys_down,
                            state: event.state,
                        },
                    },
                )
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some((_, screen)) = self.host.windows.get_mut(id) else {
                    return;
                };
                let size = screen.window.inner_size();
                if size.width == 0 || size.height == 0 {
                    return;
                }
                HostEvent::Input(
                    id,
                    InputEvent::PointerMotionAbsolute {
                        event: input::Motion {
                            time,
                            x: position.x / f64::from(size.width),
                            y: position.y / f64::from(size.height),
                            local: (position.x, position.y),
                        },
                    },
                )
            }
            WindowEvent::MouseWheel { delta, .. } => HostEvent::Input(
                id,
                InputEvent::PointerAxis {
                    event: input::Wheel { time, delta },
                },
            ),
            WindowEvent::MouseInput { state, button, .. } => HostEvent::Input(
                id,
                InputEvent::PointerButton {
                    event: input::Button {
                        time,
                        code: input::button_code(button, self.host.x11),
                        state,
                    },
                },
            ),
            _ => return,
        };
        self.events.push(event);
    }
}

/// A host window for `output` (app id and title `orrery`, so host window rules match every one), its EGL surface
/// with swap interval 0, and a damage tracker following the output.
fn open_window(active: &ActiveEventLoop, host: &Host, output: &Output) -> anyhow::Result<Screen> {
    let (w, h) = START_SIZE;
    let attributes = WinitWindow::default_attributes()
        .with_inner_size(LogicalSize::new(f64::from(w), f64::from(h)))
        .with_title(NAME)
        // The app id, so host window rules can match `class:^(orrery)$`.
        .with_name(NAME, NAME)
        .with_visible(true);
    let window = active.create_window(attributes).context("cannot create the window")?;
    let context = host.renderer.egl_context();
    let format = context.pixel_format().context("the EGL context has no pixel format")?;
    let handle = window.window_handle().context("the window has no handle")?.as_raw();
    // Safety (both): the native window lives as long as the surface (`Screen` drops the surface first), and the
    // config is the context's own.
    let surface = match handle {
        RawWindowHandle::Wayland(handle) => {
            let size = window_size(&window);
            let native = unsafe { WlEglSurface::new_from_raw(handle.surface.as_ptr().cast(), size.w, size.h) }
                .context("cannot create the window's EGL window")?;
            unsafe { EGLSurface::new(context.display(), format, context.config_id(), native) }
        }
        RawWindowHandle::Xlib(handle) => unsafe {
            EGLSurface::new(
                context.display(),
                format,
                context.config_id(),
                XlibWindow(handle.window),
            )
        },
        other => anyhow::bail!("unsupported host window {other:?}"),
    }
    .context("cannot create the window's EGL surface")?;
    disable_vsync(context, &surface);
    Ok(Screen {
        surface,
        window,
        bind_size: None,
        damage_tracker: OutputDamageTracker::from_output(output),
        decorations: render::Decorations::default(),
        skipped_since_submit: false,
        warned: None,
    })
}

/// Swap interval 0 on a window's EGL surface (it applies to the surface current on the context, so made current
/// first), so Mesa adds no frame-callback wait of its own: winit's redraw throttling is the pacing. Not what keeps a
/// hidden window from stalling orrery (that is swapping only on `RedrawRequested`); failure is logged and changes
/// nothing else.
fn disable_vsync(context: &EGLContext, surface: &EGLSurface) {
    // Safety: one thread, and the context is never moved off it.
    if let Err(e) = unsafe { context.make_current_with_surface(surface) } {
        tracing::warn!("cannot make the window current to set the swap interval: {e}");
        return;
    }
    let display = context.display().get_display_handle();
    // Safety: the context is current with the window's surface, and the display handle is alive.
    let ok = unsafe { ffi::egl::SwapInterval(**display, 0) };
    if ok == ffi::egl::TRUE {
        tracing::info!("EGL swap interval 0: frames paced by the host's frame callbacks via winit");
    } else {
        tracing::warn!("cannot set the EGL swap interval to 0; Mesa will also wait for frame callbacks");
    }
}

/// How the host's pointer is held while input is away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Input is here: nothing held.
    Free,
    /// Locked in every window (each lock takes hold while the pointer is in it).
    Locked,
    /// Where the host cannot lock (X11): confined to that window, or (None) to the next one the pointer enters.
    Confined(Option<WindowId>),
}

/// Input went away or came back (`Orrery::input_away`): the host's pointer is hidden and held, or placed where the
/// seat's pointer is (where it came in), let go and shown.
fn hold_pointer(host: &mut Host, state: &Orrery) {
    let away = state.input_away();
    if away == (host.hold != Hold::Free) {
        return;
    }
    if away {
        for screen in host.windows.values() {
            screen.window.set_cursor_visible(false);
        }
        host.hold = hold(host);
        return;
    }
    let at = state.seat.get_pointer().map(|p| p.current_location());
    let window_of = |id: WindowId| {
        let name = host.windows.name_of(id)?;
        let output = state.outputs.iter().find(|o| o.name() == name)?;
        Some((host.windows.get(id)?, state.space.output_geometry(output)?))
    };
    // Wayland takes a locked pointer's position (a hint on the window holding the lock) for where it is let go:
    // set before unlocking, kept inside that window.
    if host.hold == Hold::Locked
        && let Some(at) = at
        && let Some((screen, geometry)) = host.pointer_in.and_then(window_of)
    {
        let local = at - geometry.loc.to_f64();
        let inside = |v: f64, len: i32| v.clamp(0.0, f64::from((len - 1).max(0)));
        let local = (inside(local.x, geometry.size.w), inside(local.y, geometry.size.h));
        if let Err(e) = screen
            .window
            .set_cursor_position(PhysicalPosition::new(local.0, local.1))
        {
            tracing::debug!("cannot place the host's pointer: {e}");
        }
    }
    for screen in host.windows.values() {
        if let Err(e) = screen.window.set_cursor_grab(CursorGrabMode::None) {
            tracing::debug!("cannot let the host's pointer go: {e}");
        }
        screen.window.set_cursor_visible(true);
    }
    // Confined (X11): let go first, then warped into the window showing that point.
    if matches!(host.hold, Hold::Confined(_))
        && let Some(at) = at
        && let Some((screen, geometry)) = (host.windows.ids())
            .filter_map(window_of)
            .find(|(_, g)| g.to_f64().contains(at))
    {
        let local = at - geometry.loc.to_f64();
        if let Err(e) = screen
            .window
            .set_cursor_position(PhysicalPosition::new(local.x, local.y))
        {
            tracing::debug!("cannot place the host's pointer: {e}");
        }
    }
    host.hold = Hold::Free;
}

/// Holds the host's pointer: locked in every window, or, where a lock fails, none locked (those that took it let go)
/// and confined to the window the pointer is in, or the next one it enters.
fn hold(host: &Host) -> Hold {
    let screens: Vec<&Screen> = host.windows.values().collect();
    let took = (screens.iter())
        .take_while(|s| s.window.set_cursor_grab(CursorGrabMode::Locked).is_ok())
        .count();
    if took == screens.len() {
        return Hold::Locked;
    }
    for screen in &screens[..took] {
        let _ = screen.window.set_cursor_grab(CursorGrabMode::None);
    }
    match host.pointer_in {
        Some(id) => Hold::Confined(confine(host, id).then_some(id)),
        None => Hold::Confined(None),
    }
}

/// Confines the host's pointer to window `id`; false (logged) when the host cannot.
fn confine(host: &Host, id: WindowId) -> bool {
    let Some(screen) = host.windows.get(id) else {
        return false;
    };
    match screen.window.set_cursor_grab(CursorGrabMode::Confined) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                "the host can neither lock nor confine the pointer ({e}): it may wander while input is away"
            );
            false
        }
    }
}

/// The buffer age to render with. The damage tracker records every render, also those that found nothing to
/// draw and were never submitted, so after an idle frame its history no longer matches what the buffers hold:
/// redraw fully (age 0) until a frame is submitted again. Continuous updates keep partial damage.
fn frame_age(buffer_age: Option<usize>, skipped_since_submit: bool) -> usize {
    if skipped_since_submit {
        0
    } else {
        buffer_age.unwrap_or(0)
    }
}

/// The host windows, each known by its winit id and its output's name, in opening order.
struct Table<T> {
    rows: Vec<(WindowId, String, T)>,
}

impl<T> Table<T> {
    fn new() -> Self {
        Self { rows: Vec::new() }
    }

    fn insert(&mut self, id: WindowId, name: String, value: T) {
        self.rows.push((id, name, value));
    }

    /// The output of window `id`; None for a window already closed (its last events can still arrive).
    fn name_of(&self, id: WindowId) -> Option<&str> {
        self.rows.iter().find(|r| r.0 == id).map(|r| r.1.as_str())
    }

    fn get_mut(&mut self, id: WindowId) -> Option<(&str, &mut T)> {
        (self.rows.iter_mut())
            .find(|r| r.0 == id)
            .map(|(_, name, value)| (name.as_str(), value))
    }

    fn get(&self, id: WindowId) -> Option<&T> {
        self.rows.iter().find(|r| r.0 == id).map(|r| &r.2)
    }

    fn ids(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.rows.iter().map(|r| r.0)
    }

    fn remove_named(&mut self, name: &str) -> Option<T> {
        let i = self.rows.iter().position(|r| r.1 == name)?;
        Some(self.rows.remove(i).2)
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.rows.iter().map(|(_, _, v)| v)
    }
}

/// Each screen's x in global space, from their widths in opening order: side by side from 0.
fn side_by_side(widths: &[i32]) -> Vec<i32> {
    (widths.iter())
        .scan(0, |x, w| {
            let at = *x;
            *x += w;
            Some(at)
        })
        .collect()
}

/// Damage in GL's bottom-up coordinates, as EGL's swap with damage wants it, on a surface `height` tall.
fn flipped(damage: &[Rectangle<i32, Physical>], height: i32) -> Vec<Rectangle<i32, Physical>> {
    (damage.iter())
        .map(|r| Rectangle::new((r.loc.x, height - r.loc.y - r.size.h).into(), r.size))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn after_an_idle_frame_the_next_one_is_full() {
        assert_eq!(frame_age(Some(2), false), 2, "continuous frames keep the buffer age");
        assert_eq!(
            frame_age(Some(2), true),
            0,
            "an unsubmitted frame in between: full redraw"
        );
        assert_eq!(frame_age(None, false), 0, "an unknown age is a full redraw");
    }

    #[test]
    fn windows_are_found_by_id_and_by_output_name() {
        let (a, b) = (WindowId::from(1), WindowId::from(2));
        let mut t = Table::new();
        assert!(t.is_empty());
        t.insert(a, "winit".into(), 'a');
        t.insert(b, "winit-2".into(), 'b');
        assert_eq!(t.name_of(b), Some("winit-2"));
        assert_eq!(t.get(b), Some(&'b'));
        assert_eq!(t.get(WindowId::from(3)), None);
        assert_eq!(t.ids().collect::<Vec<_>>(), [a, b], "in opening order");
        assert_eq!(
            t.name_of(WindowId::from(3)),
            None,
            "an unknown window (just closed) has no name"
        );
        assert_eq!(
            t.get_mut(a).map(|(n, v)| (n.to_owned(), *v)),
            Some(("winit".into(), 'a'))
        );
        assert_eq!(t.remove_named("winit"), Some('a'));
        assert_eq!(t.remove_named("winit"), None, "closed once");
        assert_eq!(t.name_of(a), None);
        assert!(!t.is_empty(), "winit-2 is still open");
        assert_eq!(t.values().collect::<Vec<_>>(), vec![&'b']);
        t.remove_named("winit-2");
        assert!(t.is_empty(), "the last one closed");
    }

    #[test]
    fn a_queued_window_can_be_unqueued() {
        let output = |name: &str| {
            Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: NAME.into(),
                    model: BASE.into(),
                },
            )
        };
        let mut opening = vec![output("winit-2"), output("winit-3")];
        assert!(unqueue(&mut opening, "winit-2"));
        assert!(!unqueue(&mut opening, "winit-2"), "once");
        assert!(!unqueue(&mut opening, "winit"), "not queued");
        assert_eq!(opening.iter().map(Output::name).collect::<Vec<_>>(), ["winit-3"]);
    }

    #[test]
    fn a_failing_window_is_logged_once_per_ten_seconds() {
        let t = Instant::now();
        assert!(warn_due(None, t), "the first failure");
        assert!(!warn_due(Some(t), t + Duration::from_secs(9)));
        assert!(warn_due(Some(t), t + Duration::from_secs(10)));
    }

    #[test]
    fn screens_sit_side_by_side_in_opening_order() {
        assert_eq!(side_by_side(&[]), Vec::<i32>::new());
        assert_eq!(side_by_side(&[1280, 800, 1920]), vec![0, 1280, 2080]);
    }

    #[test]
    fn damage_is_flipped_for_gl() {
        let r = |x, y, w, h| Rectangle::<i32, Physical>::new((x, y).into(), (w, h).into());
        // A strip at the top of an 800 px window is at the bottom for GL.
        assert_eq!(flipped(&[r(10, 0, 100, 20)], 800), vec![r(10, 780, 100, 20)]);
        assert_eq!(flipped(&[r(0, 0, 50, 800)], 800), vec![r(0, 0, 50, 800)]);
    }
}
