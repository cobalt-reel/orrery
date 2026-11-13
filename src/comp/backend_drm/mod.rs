//! Bare metal: orrery on the hardware itself, started from a text console. Devices come through the seat (libseat:
//! logind/elogind or seatd), never opened directly, so orrery needs no privileges; a crash or a quit gives the
//! console back (the seat restores it).
//!
//! **One GPU.** The seat's primary GPU (the boot VGA), else its first: one `DrmDevice`, a `GbmDevice` on the same
//! descriptor, and one EGL context and `GlesRenderer` on that, shared by every screen. Every connected connector is a
//! screen at its preferred mode, on a CRTC of its own (`plan::assign_crtcs`), with its own `DrmCompositor` (a GBM
//! swapchain); its output is named after the connector (`connector_name`: `DP-3`), so monitor rules match it, and it
//! is added with `add_output` (milestone 3's numbering) right of the others. Everything is composited by GL: no
//! planes are used for windows or the cursor (no direct scanout, no hardware cursor).
//!
//! **Frames follow vblank.** A screen draws only when its last flip is done (the page-flip event came, or
//! `plan::FLIP_TIMEOUT` passed without it), and only what changed: its `DrmCompositor` tracks damage, so a frame
//! with nothing new queues nothing and idle costs one damage check per screen per tick. Draws are tried by the 16 ms
//! tick (which also does the housekeeping), once per loop iteration after input (the pointer moved), and 60% of a
//! frame after each flip (`plan::repaint_delay`), clients having had their frame callbacks at the flip. A check that
//! finds nothing to draw (or fails) sends the frame callbacks instead, so a client waiting for one is never left
//! waiting for damage. Each frame carries a sequence number, and a flip event stamped before the frame waited for was
//! queued (a frame given up on) is ignored.
//!
//! **Screens follow the hardware.** A udev change on the GPU rescans its connectors: new ones open, gone ones close
//! with `remove_output` (milestone 3's rules), the others close the gap, and a pointer left off the screens comes back
//! to the nearest. `open-screen` and `close-screen` answer that monitors decide here.
//!
//! **Input.** libinput through the session. Relative motion goes through `device_motion` (`crossing::step` keeps the
//! pointer on the screens, and crosses at the outer edges in a session of linked machines); keys, buttons, scrolling
//! and absolute motion go through `process_input_event`, with the screen the pointer is on. Keystrokes are never
//! logged. orrery draws the pointer itself (`Orrery::draws_pointer`).
//!
//! **Consoles.** Switching away (`vt N`, or another session taking the seat) pauses: input stops (libinput
//! suspended), every key and button down is released (no release would come), the DRM device is let go and nothing
//! draws. Coming back takes the GPU again with every output reset (another session may have rewired the CRTCs),
//! resumes input, rescans the connectors (monitors may have changed meanwhile) and redraws everything.
//!
//! **Errors.** No seat, no GPU or no connected screen at start: a message and a non-zero exit, the console given back.
//! While running, failures are logged (a failing screen at most every `WARN_EVERY`) and orrery goes on; a GPU removed
//! stops it. A frame the kernel refuses resets every output, at most once per resume, hotplug or frame drawn; a
//! screen refused again after a reset made for it is closed as if unplugged (`plan::recovery`) and stays dark until
//! it is plugged in again.
//!
//! None of this is reachable from the tests: only `plan` is.

mod plan;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameError, FrameFlags, PrimaryPlaneElement, RenderFrameError};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, NodeType,
};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::{ButtonState, InputEvent, PointerButtonEvent, PointerMotionEvent};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, Renderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu};
use smithay::output::{Mode, Output, PhysicalProperties};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::drm::control::{Device as _, ModeTypeFlags, ResourceHandles, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::{Clock, DeviceFd, Monotonic, Transform};

use super::frames::{WARN_EVERY, warn_due};
use super::{Orrery, connector_name, render};
use crate::NAME;
use crate::layout::Rect;

/// How often housekeeping runs and idle screens are checked for damage: about 60 Hz.
const FRAME: Duration = Duration::from_millis(16);

/// The swapchain formats tried, in order: 8 bits per channel, which every display engine scans out.
const FORMATS: [Fourcc; 2] = [Fourcc::Argb8888, Fourcc::Abgr8888];

/// Each queued frame carries its sequence number (`Screen::frames`), checked when its flip comes.
type Compositor = DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, u64, DrmDeviceFd>;

pub fn init(event_loop: &mut EventLoop<'static, Orrery>, state: &mut Orrery) -> anyhow::Result<()> {
    let (mut session, session_events) = LibSeatSession::new().map_err(|e| {
        anyhow!(
            "cannot open a seat session ({e}): bare metal needs a seat (logind, elogind or seatd running) and {NAME} \
             started from a text console; inside a desktop session use `--backend winit`"
        )
    })?;
    let seat = session.seat();
    let path = gpu_path(&seat)?;
    let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd =
        (session.open(&path, flags)).map_err(|e| anyhow!("cannot open {} through the seat: {e}", path.display()))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (drm, drm_events) =
        DrmDevice::new(fd.clone(), true).with_context(|| format!("cannot drive the displays of {}", path.display()))?;
    let gbm = GbmDevice::new(fd).with_context(|| format!("cannot allocate buffers on {}", path.display()))?;
    // Safety: the GBM device outlives the display, which holds a handle on it.
    let display = unsafe { EGLDisplay::new(gbm.clone()) }.context("cannot open EGL on the GPU")?;
    let context = EGLContext::new(&display).context("cannot create an EGL context")?;
    // Safety: the context is new and used on this thread only.
    let renderer = unsafe { GlesRenderer::new(context) }.context("cannot create the renderer")?;
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    tracing::info!("GPU {} on seat {seat}", path.display());

    let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|()| anyhow!("libinput cannot take the input devices of seat {seat}"))?;
    let udev = UdevBackend::new(&seat).context("cannot watch for monitors (udev)")?;

    let backend = Rc::new(RefCell::new(Backend {
        screens: Vec::new(),
        renderer,
        allocator: GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT),
        // No import node: client buffers are never scanned out directly.
        exporter: GbmFramebufferExporter::new(gbm, None),
        render_formats,
        drm,
        path,
        libinput: libinput.clone(),
        active: true,
        redraw_queued: false,
        buttons: HashSet::new(),
        dark: Vec::new(),
        reset_spent: false,
        clock: Clock::new(),
    }));
    scan(&backend, state);
    if backend.borrow().screens.is_empty() {
        bail!(
            "no screen connected to {} (or none could be driven)",
            backend.borrow().path.display()
        );
    }
    // linux-dmabuf: what the renderer imports, the GPU's render node (else its card node) as the main device.
    let (formats, node) = {
        let b = backend.borrow();
        let formats = b.renderer.dmabuf_formats().iter().copied().collect();
        let node = DrmNode::from_path(&b.path).ok().map(|card| {
            (card.node_with_type(NodeType::Render))
                .and_then(Result::ok)
                .unwrap_or(card)
        });
        (formats, node)
    };
    state.advertise_dmabuf(
        formats,
        node,
        Box::new(GpuImport {
            backend: backend.clone(),
        }),
    );
    state.screens = Some(Box::new(Screens));
    state.consoles = Some(Box::new(Consoles { session }));
    state.draws_pointer = true;

    let handle = event_loop.handle();
    let b = backend.clone();
    handle
        .insert_source(drm_events, move |event, meta, state| match event {
            DrmEvent::VBlank(crtc) => vblank(&b, state, crtc, meta.as_ref()),
            DrmEvent::Error(e) => tracing::warn!("the GPU reported: {e}"),
        })
        .map_err(|e| anyhow!("cannot watch the GPU: {e}"))?;
    let b = backend.clone();
    handle
        .insert_source(session_events, move |event, (), state| match event {
            SessionEvent::PauseSession => pause(&b, state),
            SessionEvent::ActivateSession => resume(&b, state),
        })
        .map_err(|e| anyhow!("cannot watch the seat: {e}"))?;
    let b = backend.clone();
    let dev = backend.borrow().drm.device_id();
    handle
        .insert_source(udev, move |event, (), state| match event {
            UdevEvent::Changed { device_id } if device_id == dev => {
                scan(&b, state);
                queue_redraw(&b, state);
            }
            UdevEvent::Removed { device_id } if device_id == dev => {
                tracing::error!("the GPU went away; stopping");
                state.loop_signal.stop();
            }
            _ => {}
        })
        .map_err(|e| anyhow!("cannot watch for monitors: {e}"))?;
    let b = backend.clone();
    handle
        .insert_source(LibinputInputBackend::new(libinput), move |event, (), state| {
            input(&b, state, event)
        })
        .map_err(|e| anyhow!("cannot watch the input devices: {e}"))?;
    let b = backend;
    handle
        .insert_source(Timer::from_duration(FRAME), move |_, _, state| {
            state.refresh();
            draw_all(&b, state);
            TimeoutAction::ToDuration(FRAME)
        })
        .map_err(|e| anyhow!("cannot start the frame timer: {e}"))?;
    Ok(())
}

/// The seat's primary GPU (the boot VGA), else its first.
fn gpu_path(seat: &str) -> anyhow::Result<PathBuf> {
    let primary = primary_gpu(seat).with_context(|| format!("cannot list the GPUs of seat {seat} (udev)"))?;
    let path = match primary {
        Some(path) => Some(path),
        None => (all_gpus(seat).with_context(|| format!("cannot list the GPUs of seat {seat} (udev)"))?)
            .into_iter()
            .next(),
    };
    path.with_context(|| format!("no GPU on seat {seat} (no /dev/dri/card* belongs to it)"))
}

/// What the event sources share, behind one `Rc<RefCell>`. Borrowed only briefly, never across a call into `Orrery`
/// that could reach back here (`Screens` and `Consoles` keep their own; `Orrery` holds it only through the dmabuf
/// importer, which yields to a borrow by letting the buffer through untried).
struct Backend {
    // Declaration order is drop order: the screens' surfaces and swapchains go before the renderer, the buffers'
    // device and the DRM device they were made on.
    screens: Vec<Screen>,
    renderer: GlesRenderer,
    allocator: GbmAllocator<DrmDeviceFd>,
    exporter: GbmFramebufferExporter<DrmDeviceFd>,
    /// What the renderer can draw into, for the swapchains.
    render_formats: FormatSet,
    drm: DrmDevice,
    path: PathBuf,
    libinput: Libinput,
    /// The seat is ours: false while another console has it.
    active: bool,
    /// An idle callback will try every screen (`queue_redraw`).
    redraw_queued: bool,
    /// Pointer buttons down, released when the seat goes away.
    buttons: HashSet<u32>,
    /// Connectors closed for good (`plan::Recovery::Close`): dark until unplugged (`plan::screens_wanted`).
    dark: Vec<connector::Handle>,
    /// An output reset was made since any screen last queued a frame, or since the last resume or hotplug
    /// (`plan::recovery`).
    reset_spent: bool,
    /// The clock page-flip events are stamped on.
    clock: Clock<Monotonic>,
}

/// Client dmabufs into the renderer (see `super::dmabuf`).
struct GpuImport {
    backend: Rc<RefCell<Backend>>,
}

impl super::dmabuf::DmabufImport for GpuImport {
    fn import(&mut self, dmabuf: &Dmabuf) -> Result<(), String> {
        // Client requests are dispatched outside any borrow of the backend; should that change, the buffer is let
        // through untried and imported when first drawn.
        let Ok(mut backend) = self.backend.try_borrow_mut() else {
            return Ok(());
        };
        // Buffers enter the renderer's texture cache here, at creation; it is otherwise purged only after a drawn
        // frame, so a client recreating buffers while nothing is drawn (another console, a hidden window) would pin
        // the dead ones' memory.
        let _ = backend.renderer.cleanup_texture_cache();
        (backend.renderer.import_dmabuf(dmabuf, None))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// One connector driven as a screen, in opening order.
struct Screen {
    compositor: Compositor,
    output: Output,
    connector: connector::Handle,
    decorations: render::Decorations,
    /// The queued frame whose flip has not come yet: nothing is drawn until it comes (`plan::flip_waits`).
    pending: Option<Pending>,
    /// Frames queued so far: the next one's sequence number.
    frames: u64,
    /// An output reset was made for this screen and it has queued nothing since (`plan::recovery`).
    had_reset: bool,
    /// When a failed frame was last logged (`warn_due`).
    warned: Option<Instant>,
}

/// A queued frame waiting for its flip.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// Its sequence number, its compositor user data.
    seq: u64,
    /// When it was queued, for `plan::flip_waits`.
    since: Instant,
    /// The same on the clock flip events are stamped on, for `plan::flip_event_matches`.
    at: Duration,
}

/// Screens follow the monitors: they cannot be opened or closed by request.
struct Screens;

impl super::Screens for Screens {
    fn open(&mut self, _state: &mut Orrery, _number: u8) -> Result<(), String> {
        Err("on bare metal screens follow the monitors: connect one to add a screen".into())
    }

    fn close(&mut self, _state: &mut Orrery, name: &str) -> Result<(), String> {
        Err(format!(
            "on bare metal screens follow the monitors: disconnect {name}'s to close it"
        ))
    }
}

/// Console switching through the seat.
struct Consoles {
    session: LibSeatSession,
}

impl super::Consoles for Consoles {
    fn switch(&mut self, vt: u8) -> Result<(), String> {
        self.session.change_vt(i32::from(vt)).map_err(|e| e.to_string())
    }
}

/// Makes the screens match the connected connectors (`plan::connector_changes`, less those closed for good:
/// `plan::screens_wanted`): gone ones close, the others close the gap and the pointer stays on them; new ones open
/// on free CRTCs. A change allows one more output reset (`plan::recovery`). Nothing while the seat is away (`resume`
/// scans again).
fn scan(backend: &RefCell<Backend>, state: &mut Orrery) {
    let mut b = backend.borrow_mut();
    if !b.active {
        return;
    }
    let resources = match b.drm.resource_handles() {
        Ok(r) => r,
        Err(e) => return tracing::warn!("cannot list the connectors: {e}"),
    };
    // Probed (the kernel asks the monitor), so a monitor plugged in is seen with its modes.
    let connected: Vec<connector::Info> = (resources.connectors().iter())
        .filter_map(|&c| b.drm.get_connector(c, true).ok())
        .filter(|c| c.state() == connector::State::Connected && !c.modes().is_empty())
        .collect();
    let open: Vec<connector::Handle> = b.screens.iter().map(|s| s.connector).collect();
    let handles: Vec<connector::Handle> = connected.iter().map(connector::Info::handle).collect();
    let (wanted, dark) = plan::screens_wanted(&handles, &b.dark);
    b.dark = dark;
    let (gone, new) = plan::connector_changes(&open, &wanted);
    if !gone.is_empty() || !new.is_empty() {
        b.reset_spent = false;
        for screen in &mut b.screens {
            screen.had_reset = false;
        }
    }

    for c in &gone {
        if let Some(i) = b.screens.iter().position(|s| s.connector == *c) {
            let name = close_screen(&mut b, state, i);
            tracing::info!("{name} disconnected");
        }
    }
    if !gone.is_empty() {
        state.lay_side_by_side(None);
        keep_pointer_on_screens(state);
    }

    let new: Vec<&connector::Info> = (new.iter())
        .filter_map(|h| connected.iter().find(|c| c.handle() == *h))
        .collect();
    let usable: Vec<Vec<crtc::Handle>> = new.iter().map(|c| usable_crtcs(&b.drm, &resources, c)).collect();
    let taken: Vec<crtc::Handle> = b.screens.iter().map(|s| s.compositor.crtc()).collect();
    for (info, crtc) in new.into_iter().zip(plan::assign_crtcs(&usable, &taken)) {
        let name = connector_name(u32::from(info.interface()), info.interface_id());
        let Some(crtc) = crtc else {
            tracing::warn!("{name}: no free CRTC to drive it; it stays dark");
            continue;
        };
        if let Err(e) = open_screen(&mut b, state, info, crtc, &name) {
            tracing::warn!("{name}: {e:#}; it stays dark");
        }
    }
}

/// Takes screen `i` away (its output with `remove_output`); its name. The caller lays the others side by side.
fn close_screen(b: &mut Backend, state: &mut Orrery, i: usize) -> String {
    let name = b.screens.remove(i).output.name();
    state.remove_output(&name);
    name
}

/// The CRTCs that can drive connector `info`, the one driving it now first (no modeset needed for it).
fn usable_crtcs(drm: &DrmDevice, resources: &ResourceHandles, info: &connector::Info) -> Vec<crtc::Handle> {
    let current = (info.current_encoder())
        .and_then(|e| drm.get_encoder(e).ok())
        .and_then(|e| e.crtc());
    let mut out: Vec<crtc::Handle> = current.into_iter().collect();
    for encoder in info.encoders().iter().filter_map(|&e| drm.get_encoder(e).ok()) {
        for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
            if !out.contains(&crtc) {
                out.push(crtc);
            }
        }
    }
    out
}

/// Connector `info` as a screen: its preferred mode on `crtc`, a swapchain, and its output right of the others.
fn open_screen(
    b: &mut Backend,
    state: &mut Orrery,
    info: &connector::Info,
    crtc: crtc::Handle,
    name: &str,
) -> anyhow::Result<()> {
    let preferred: Vec<bool> = (info.modes().iter())
        .map(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .collect();
    let drm_mode = (plan::preferred_mode(&preferred))
        .and_then(|i| info.modes().get(i))
        .copied()
        .context("it has no modes")?;
    let surface = (b.drm)
        .create_surface(crtc, drm_mode, &[info.handle()])
        .context("cannot set up its CRTC")?;
    let (mm_w, mm_h) = info.size().unwrap_or((0, 0));
    let output = Output::new(
        name.to_owned(),
        PhysicalProperties {
            size: (i32::try_from(mm_w).unwrap_or(0), i32::try_from(mm_h).unwrap_or(0)).into(),
            subpixel: info.subpixel().into(),
            make: "Unknown".into(),
            model: name.to_owned(),
        },
    );
    let mode = Mode::from(drm_mode);
    let x = state.next_screen_x();
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((x, 0).into()));
    output.set_preferred(mode);
    let compositor = Compositor::new(
        &output,
        surface,
        None,
        b.allocator.clone(),
        b.exporter.clone(),
        FORMATS,
        b.render_formats.iter().copied(),
        b.drm.cursor_size(),
        None::<GbmDevice<DrmDeviceFd>>,
    )
    .map_err(|e| anyhow!("cannot set up its swapchain: {e}"))?;
    b.screens.push(Screen {
        compositor,
        output: output.clone(),
        connector: info.handle(),
        decorations: render::Decorations::default(),
        pending: None,
        frames: 0,
        had_reset: false,
        warned: None,
    });
    let (w, h) = (mode.size.w, mode.size.h);
    tracing::info!("{name}: {w}x{h} at {:.2} Hz", f64::from(mode.refresh) / 1000.0);
    state.add_output(output, Rect { x, y: 0, w, h });
    Ok(())
}

/// The pointer back on the screens when it is off them (a screen went away under it).
fn keep_pointer_on_screens(state: &mut Orrery) {
    let Some(at) = state.seat.get_pointer().map(|p| p.current_location()) else {
        return;
    };
    if let Some((x, y)) = plan::clamp_to_screens(&state.screen_rects(), (at.x, at.y))
        && (x, y) != (at.x, at.y)
    {
        let time = state.clock();
        state.pointer_to((x, y).into(), time);
    }
}

/// The output name of the screen the pointer is on, else the first's (empty without screens).
fn pointer_screen(state: &Orrery) -> String {
    let at = state.seat.get_pointer().map(|p| p.current_location());
    let geometries: Vec<Rect> = (state.outputs.iter())
        .map(|o| {
            state
                .space
                .output_geometry(o)
                .map_or(Rect { x: 0, y: 0, w: 0, h: 0 }, |g| Rect {
                    x: g.loc.x,
                    y: g.loc.y,
                    w: g.size.w,
                    h: g.size.h,
                })
        })
        .collect();
    let i = at
        .and_then(|at| plan::screen_at(&geometries, (at.x, at.y)))
        .unwrap_or(0);
    state.outputs.get(i).map(Output::name).unwrap_or_default()
}

/// A flip is done on `crtc`: its screen may draw again, its clients get their frame callbacks, and it draws again
/// most of a frame later (`plan::repaint_delay`). A late event of a frame given up on (`plan::flip_waits`) is
/// ignored: it must not be taken for the frame queued since (`plan::flip_event_matches`).
fn vblank(backend: &Rc<RefCell<Backend>>, state: &mut Orrery, crtc: crtc::Handle, meta: Option<&DrmEventMetadata>) {
    let stamp = meta.and_then(|m| match m.time {
        DrmEventTime::Monotonic(t) => Some(t),
        DrmEventTime::Realtime(_) => None,
    });
    let output = {
        let mut b = backend.borrow_mut();
        let Some(screen) = b.screens.iter_mut().find(|s| s.compositor.crtc() == crtc) else {
            return;
        };
        let name = screen.output.name();
        let Some(pending) = screen.pending.filter(|p| plan::flip_event_matches(p.at, stamp)) else {
            tracing::debug!("{name}: a late flip event, ignored");
            return;
        };
        screen.pending = None;
        match screen.compositor.frame_submitted() {
            Ok(Some(seq)) if seq != pending.seq => {
                tracing::warn!(
                    "{name}: the flip came for frame {seq}, frame {} was waited for",
                    pending.seq
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("{name}: finishing the frame: {e}"),
        }
        screen.output.clone()
    };
    state.send_frames(&output);
    let delay = plan::repaint_delay(output.current_mode().map_or(0, |m| m.refresh));
    let b = backend.clone();
    let name = output.name();
    let later = (state.loop_handle).insert_source(Timer::from_duration(delay), move |_, _, state| {
        let mut b = b.borrow_mut();
        if let Some(i) = b.screens.iter().position(|s| s.output.name() == name)
            && draw(&mut b, i, state) == Drawn::Refused
        {
            recover(&mut b, state, std::slice::from_ref(&name));
        }
        TimeoutAction::Drop
    });
    if let Err(e) = later {
        tracing::warn!("cannot schedule the next frame: {e}");
    }
}

/// Every screen tried once after this loop iteration (input moved the pointer, say), however many ask.
fn queue_redraw(backend: &Rc<RefCell<Backend>>, state: &Orrery) {
    {
        let mut b = backend.borrow_mut();
        if b.redraw_queued || !b.active {
            return;
        }
        b.redraw_queued = true;
    }
    let b = backend.clone();
    state.loop_handle.insert_idle(move |state| {
        b.borrow_mut().redraw_queued = false;
        draw_all(&b, state);
    });
}

/// Every screen tried (`draw`); those the kernel refused recover (`recover`).
fn draw_all(backend: &RefCell<Backend>, state: &mut Orrery) {
    let mut b = backend.borrow_mut();
    let mut refused = Vec::new();
    for i in 0..b.screens.len() {
        if draw(&mut b, i, state) == Drawn::Refused {
            refused.push(b.screens[i].output.name());
        }
    }
    if !refused.is_empty() {
        recover(&mut b, state, &refused);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drawn {
    /// A frame was queued, nothing changed, the screen may not draw now, or drawing failed otherwise.
    Done,
    /// The kernel refused the new state (an atomic test failed): `recover`.
    Refused,
}

/// The screens named in `refused`, whose new state the kernel refused, as `plan::recovery` says: every output reset
/// once (another session may have left the CRTCs wired otherwise), and a screen refused again after a reset made for
/// it closed as if unplugged, dark until it is plugged in again; the others are never reset for it again.
fn recover(b: &mut Backend, state: &mut Orrery, refused: &[String]) {
    let mut closed = false;
    for name in refused {
        let Some(i) = b.screens.iter().position(|s| s.output.name() == *name) else {
            continue;
        };
        match plan::recovery(b.screens[i].had_reset, b.reset_spent) {
            plan::Recovery::ResetAll => {
                tracing::warn!("{name}: the kernel refused its frame; resetting every output once");
                reset(b);
                b.reset_spent = true;
                b.screens[i].had_reset = true;
            }
            plan::Recovery::Wait => b.screens[i].had_reset = true,
            plan::Recovery::Close => {
                let connector = b.screens[i].connector;
                b.dark.push(connector);
                close_screen(b, state, i);
                tracing::error!(
                    "{name}: the kernel keeps refusing it; closed, it stays dark until it is plugged in again"
                );
                closed = true;
            }
        }
    }
    if closed {
        state.lay_side_by_side(None);
        keep_pointer_on_screens(state);
    }
}

/// Screen `i`'s frame, when the seat is ours and its last flip is done: rendered and queued when something changed
/// (its sequence number as the user data); else, or when drawing failed, the frame callbacks go out now (none will
/// come from a flip). Failures are logged at most every `WARN_EVERY`; the next try comes with the next tick.
fn draw(b: &mut Backend, i: usize, state: &mut Orrery) -> Drawn {
    let Backend {
        screens,
        renderer,
        active,
        reset_spent,
        clock,
        ..
    } = b;
    let now = Instant::now();
    let Some(screen) = screens
        .get_mut(i)
        .filter(|s| *active && !plan::flip_waits(s.pending.map(|p| p.since), now))
    else {
        return Drawn::Done;
    };
    if screen.pending.take().is_some() {
        tracing::warn!("{}: a flip's event never came; drawing again", screen.output.name());
        if let Err(e) = screen.compositor.frame_submitted() {
            tracing::debug!("{}: dropping the lost frame: {e}", screen.output.name());
        }
    }
    let name = screen.output.name();
    let started = Instant::now();
    let elements = render::output_elements(&name, state, renderer, &mut screen.decorations);
    let frame = screen
        .compositor
        .render_frame(renderer, &elements, render::clear_color(state), FrameFlags::empty());
    let queued = match frame {
        Ok(frame) if frame.is_empty => Ok(false),
        Ok(frame) => {
            // Without fences the kernel cannot wait for the GPU: wait here, or the flip may show half a frame.
            if frame.needs_sync()
                && let PrimaryPlaneElement::Swapchain(element) = &frame.primary_element
                && element.sync.wait().is_err()
            {
                tracing::debug!("{name}: waiting for the GPU was interrupted");
            }
            let seq = screen.frames;
            // Stamped before queueing: its flip event comes after.
            let at = Duration::from(clock.now());
            match screen.compositor.queue_frame(seq) {
                Ok(()) => {
                    screen.frames += 1;
                    screen.pending = Some(Pending {
                        seq,
                        since: Instant::now(),
                        at,
                    });
                    Ok(true)
                }
                Err(e) => Err((
                    matches!(e, FrameError::DrmError(DrmError::TestFailed(_))),
                    e.to_string(),
                )),
            }
        }
        Err(e) => {
            let failed = matches!(
                e,
                RenderFrameError::PrepareFrame(FrameError::DrmError(DrmError::TestFailed(_)))
            );
            Err((failed, e.to_string()))
        }
    };
    match queued {
        Ok(true) => {
            *reset_spent = false;
            screen.had_reset = false;
            state.frame_drawn(started.elapsed());
            Drawn::Done
        }
        Ok(false) => {
            state.send_frames(&screen.output);
            Drawn::Done
        }
        Err((failed, e)) => {
            let now = Instant::now();
            if warn_due(screen.warned, now) {
                screen.warned = Some(now);
                tracing::warn!(
                    "{name}: drawing the frame: {e} (logged at most every {} s)",
                    WARN_EVERY.as_secs()
                );
            }
            // No flip will bring them: clients would wait for good.
            state.send_frames(&screen.output);
            if failed { Drawn::Refused } else { Drawn::Done }
        }
    }
}

/// Every output set up from scratch: connectors and planes off, then each screen's next frame a full modeset.
fn reset(b: &mut Backend) {
    if let Err(e) = b.drm.reset_state() {
        tracing::warn!("cannot reset the outputs: {e}");
    }
    for screen in &mut b.screens {
        if let Err(e) = screen.compositor.reset_state() {
            tracing::warn!("{}: cannot reset: {e}", screen.output.name());
        }
    }
}

/// Another console took the seat: input and drawing stop, and what is held down is let go (no release will come).
fn pause(backend: &RefCell<Backend>, state: &mut Orrery) {
    tracing::info!("the console was switched away: pausing");
    let buttons = {
        let mut b = backend.borrow_mut();
        b.active = false;
        b.libinput.suspend();
        b.drm.pause();
        std::mem::take(&mut b.buttons)
    };
    state.keyboard_focus_lost();
    let time = state.clock();
    for code in buttons {
        state.device_button(code, ButtonState::Released, time);
    }
}

/// The seat is back: the GPU taken again with every output reset, input resumed, the monitors rescanned and
/// everything redrawn.
fn resume(backend: &Rc<RefCell<Backend>>, state: &mut Orrery) {
    tracing::info!("the console is back: resuming");
    {
        let mut b = backend.borrow_mut();
        if let Err(e) = b.drm.activate(true) {
            tracing::error!("cannot take the GPU back: {e}");
        }
        b.reset_spent = false;
        for screen in &mut b.screens {
            screen.had_reset = false;
            if let Err(e) = screen.compositor.reset_state() {
                tracing::warn!("{}: cannot reset: {e}", screen.output.name());
            }
            // A flip whose event never came (the device was let go first) is taken as done, or the screen would
            // wait for it for good.
            if screen.pending.take().is_some()
                && let Err(e) = screen.compositor.frame_submitted()
            {
                tracing::debug!("{}: dropping the frame in flight: {e}", screen.output.name());
            }
        }
        if b.libinput.resume().is_err() {
            tracing::error!("cannot resume the input devices");
        }
        b.active = true;
    }
    scan(backend, state);
    queue_redraw(backend, state);
}

/// One libinput event. Devices coming and going are logged by name; relative motion moves the pointer by
/// `device_motion`; the rest goes through `process_input_event` on the screen the pointer is on. Every event may have
/// moved the pointer or changed what is shown: the screens are tried after this loop iteration.
fn input(backend: &Rc<RefCell<Backend>>, state: &mut Orrery, event: InputEvent<LibinputInputBackend>) {
    match event {
        InputEvent::DeviceAdded { device } => tracing::info!("input device: {}", device.name()),
        InputEvent::DeviceRemoved { device } => tracing::info!("input device gone: {}", device.name()),
        InputEvent::PointerMotion { event } => state.device_motion(event.delta_x(), event.delta_y(), true),
        InputEvent::PointerButton { event } => {
            {
                let mut b = backend.borrow_mut();
                match event.state() {
                    ButtonState::Pressed => b.buttons.insert(event.button_code()),
                    ButtonState::Released => b.buttons.remove(&event.button_code()),
                };
            }
            let output = pointer_screen(state);
            state.process_input_event(&output, InputEvent::<LibinputInputBackend>::PointerButton { event });
        }
        event => {
            let output = pointer_screen(state);
            state.process_input_event(&output, event);
        }
    }
    queue_redraw(backend, state);
}
