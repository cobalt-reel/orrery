//! Nested: orrery as a window inside the running Wayland session. One output, `winit`, sized to the window.
//!
//! **Presenting only when the host asks.** eglSwapBuffers blocks inside Mesa (wl_display_dispatch_queue) until
//! the host releases a buffer or sends a frame callback, and a host sends neither to a hidden window (another
//! workspace, a scratchpad): one swap there stalled the whole loop, the control socket, the config watcher and
//! every client (seen in the first nested session; swap interval 0 alone did not help). So the only swap is
//! in winit's `RedrawRequested`, and every submit goes with `pre_present_notify` (inside `submit`), which asks
//! the host for a frame callback; winit on Wayland holds `RedrawRequested` back while that callback is in
//! flight. While hidden, the callback never comes, so orrery simply stops drawing, without blocking.
//!
//! The 16 ms calloop timer does everything else, hidden or not: housekeeping, client frame callbacks, and a
//! `request_redraw` (cheap; it just sets winit's flag). A redraw with no damage submits nothing and so leaves no
//! callback in flight, and the next tick's request is delivered again: idle costs one damage check per tick.
//! None of this is reachable from the headless tests.

use std::cell::RefCell;
use std::rc::Rc;

use std::time::Duration;

use anyhow::Context;
use smithay::backend::egl::ffi;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::platform::wayland::WindowAttributesExtWayland as _;
use smithay::reexports::winit::window::Window as WinitWindow;
use smithay::utils::Transform;

use super::{Orrery, render};
use crate::layout::Rect;

/// How often housekeeping runs, clients get frame callbacks and a redraw is requested: about 60 Hz.
const FRAME: Duration = Duration::from_millis(16);

pub fn init(event_loop: &mut EventLoop<'static, Orrery>, state: &mut Orrery) -> anyhow::Result<()> {
    let attributes = WinitWindow::default_attributes()
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_title("orrery")
        // The app id, so host window rules can match `class:^(orrery)$`.
        .with_name("orrery", "orrery")
        .with_visible(true);
    let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)
        .map_err(|e| anyhow::anyhow!("cannot open the nested window: {e}"))?;

    disable_vsync(&mut backend);

    let size = backend.window_size();
    let mode = Mode { size, refresh: 60_000 };
    let output = Output::new(
        "winit".to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "orrery".into(),
            model: "winit".into(),
        },
    );
    let _global = output.create_global::<Orrery>(&state.display_handle);
    // GL draws bottom-up; winit's surface wants it flipped (as in Smithay's smallvil).
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.add_output(
        output.clone(),
        Rect {
            x: 0,
            y: 0,
            w: size.w,
            h: size.h,
        },
    );

    let mut frame = Frames {
        damage_tracker: OutputDamageTracker::from_output(&output),
        decorations: render::Decorations::default(),
        skipped_since_submit: false,
    };

    // Shared by the timer (request_redraw) and the winit source (drawing); one thread, never borrowed across.
    let backend = Rc::new(RefCell::new(backend));
    let timer_backend = backend.clone();
    let timer_output = output.clone();
    event_loop
        .handle()
        .insert_source(Timer::from_duration(FRAME), move |_, _, state| {
            state.refresh();
            state.send_frames(&timer_output);
            // Never draws or swaps: winit delivers the redraw when the host is ready for a frame.
            timer_backend.borrow().window().request_redraw();
            TimeoutAction::ToDuration(FRAME)
        })
        .map_err(|e| anyhow::anyhow!("cannot start the frame timer: {e}"))?;
    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            // The scale factor is ignored: orrery runs at scale 1 for now.
            WinitEvent::Resized { size, .. } => {
                // A minimised or mid-resize window can report an empty size; slots need a real area.
                if size.w <= 0 || size.h <= 0 {
                    return;
                }
                output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
                let name = output.name();
                state.resize_output(
                    &name,
                    Rect {
                        x: 0,
                        y: 0,
                        w: size.w,
                        h: size.h,
                    },
                );
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            // The host is ready for a frame (or none is in flight): the only place that swaps.
            WinitEvent::Redraw => draw(&mut backend.borrow_mut(), &output, state, &mut frame),
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            // Releases while unfocused never arrive: release what is down (or Alt would stick in the seat and
            // in clients) and forget held binds (or show-tabs would stick on).
            WinitEvent::Focus(false) => state.keyboard_focus_lost(),
            WinitEvent::Focus(true) => {}
        })
        .map_err(|e| anyhow::anyhow!("cannot watch the nested window: {e}"))?;
    Ok(())
}

/// What drawing keeps between frames.
struct Frames {
    damage_tracker: OutputDamageTracker,
    decorations: render::Decorations,
    /// A frame was rendered but not submitted since the last submit (see `frame_age`).
    skipped_since_submit: bool,
}

/// One frame, on winit's `RedrawRequested` only (see the module doc): render, and submit (swap) only real
/// damage. A failed frame is logged and the next one tried; the session keeps running.
fn draw(backend: &mut WinitGraphicsBackend<GlesRenderer>, output: &Output, state: &mut Orrery, frame: &mut Frames) {
    let age = frame_age(backend.buffer_age(), frame.skipped_since_submit);
    let started = std::time::Instant::now();
    // Copied out so the framebuffer's borrow of the backend ends before submit.
    let damage = match backend.bind() {
        Ok((renderer, mut framebuffer)) => render::render_output(
            &output.name(),
            state,
            renderer,
            &mut framebuffer,
            &mut frame.damage_tracker,
            &mut frame.decorations,
            age,
        )
        .map(|r| r.damage.cloned()),
        Err(e) => Err(anyhow::anyhow!("cannot bind the window: {e}")),
    };
    let took = started.elapsed();
    let presented = damage.and_then(|damage| match damage {
        // submit calls pre_present_notify: no further redraw until the host's frame callback.
        Some(damage) if !damage.is_empty() => {
            let r = backend.submit(Some(&damage)).context("presenting the frame");
            if r.is_ok() {
                frame.skipped_since_submit = false;
                // Only frames that went to the screen count: idle damage checks would make the times look tiny.
                state.frame_drawn(took);
            }
            r
        }
        _ => {
            frame.skipped_since_submit = true;
            Ok(())
        }
    });
    if let Err(e) = presented {
        tracing::warn!("{e:#}");
    }
}

/// Swap interval 0 on the window's EGL surface (it applies to the surface current on the context, so bind first),
/// so Mesa adds no frame-callback wait of its own: winit's redraw throttling is the pacing. Not what keeps a hidden
/// orrery alive (that is swapping only on `RedrawRequested`); failure is logged and changes nothing else.
fn disable_vsync(backend: &mut WinitGraphicsBackend<GlesRenderer>) {
    match backend.bind() {
        Ok((renderer, _framebuffer)) => {
            let display = renderer.egl_context().display().get_display_handle();
            // Safety: the context is current with the window's surface (bind), and the display handle is alive.
            let ok = unsafe { ffi::egl::SwapInterval(**display, 0) };
            if ok == ffi::egl::TRUE {
                tracing::info!("EGL swap interval 0: frames paced by the host's frame callbacks via winit");
            } else {
                tracing::warn!("cannot set the EGL swap interval to 0; Mesa will also wait for frame callbacks");
            }
        }
        Err(e) => tracing::warn!("cannot bind the window to set the swap interval: {e}"),
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

#[cfg(test)]
mod tests {
    use super::frame_age;

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
}
