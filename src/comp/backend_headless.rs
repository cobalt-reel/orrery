//! A virtual output with no input and no rendering, for end-to-end tests: clients connect, commit and get
//! configured exactly as on screen, and a 16 ms timer stands in for vblank.

use std::time::Duration;

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::Transform;

use super::Orrery;
use crate::NAME;
use crate::layout::Rect;

const FRAME: Duration = Duration::from_millis(16);

pub fn init(event_loop: &mut EventLoop<'static, Orrery>, state: &mut Orrery, (w, h): (i32, i32)) -> anyhow::Result<()> {
    let output = Output::new(
        "headless".to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: NAME.into(),
            model: "headless".into(),
        },
    );
    // The global lives as long as the display; the id is not needed again.
    let _global = output.create_global::<Orrery>(&state.display_handle);
    let mode = Mode {
        size: (w, h).into(),
        refresh: 60_000,
    };
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.add_output(output.clone(), Rect { x: 0, y: 0, w, h });

    // Clients wait for frame callbacks before drawing again, and the space must refresh to send output
    // enter/leave.
    let mut deadline = std::time::Instant::now() + FRAME;
    event_loop
        .handle()
        .insert_source(Timer::from_deadline(deadline), move |_, _, state| {
            state.refresh();
            state.send_frames(&output);
            deadline = next_tick(deadline, std::time::Instant::now());
            TimeoutAction::ToInstant(deadline)
        })
        .map_err(|e| anyhow::anyhow!("cannot start the frame timer: {e}"))?;
    Ok(())
}

/// The tick after the one due at `due`, at a fixed rate: a tick's own work (a refresh, frame callbacks)
/// must not stretch the period (`ToDuration` re-arms from the end of the work: 16 ms + 4 ms of work
/// would be 50 Hz). Ticks missed by more than a period are skipped rather than run back to back.
fn next_tick(due: std::time::Instant, now: std::time::Instant) -> std::time::Instant {
    let next = due + FRAME;
    if next <= now { now + FRAME } else { next }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FRAME, next_tick};

    #[test]
    fn ticks_keep_a_fixed_rate_and_skip_what_they_missed() {
        let t0 = Instant::now();
        // Drawing took 4 ms: the next tick is still a period after the last one was due.
        assert_eq!(next_tick(t0, t0 + Duration::from_millis(4)), t0 + FRAME);
        // A stall of three periods: no burst of catch-up ticks.
        let late = t0 + FRAME * 3;
        assert_eq!(next_tick(t0, late), late + FRAME);
    }
}
