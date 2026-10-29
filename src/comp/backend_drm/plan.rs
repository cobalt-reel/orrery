//! What the DRM backend does with the hardware, decided without it: which connectors become or stop being screens,
//! which CRTC drives each, which mode it runs, and where the pointer is among the screens. Generic over the handles,
//! so the tests need no device.

use std::time::{Duration, Instant};

use crate::layout::Rect;

/// The screens to close and the connectors to open, given the connectors with screens (in opening order) and those
/// connected now (in the device's order): gone ones in opening order, new ones in the device's.
#[must_use]
pub fn connector_changes<C: Copy + Eq>(open: &[C], connected: &[C]) -> (Vec<C>, Vec<C>) {
    let gone = open.iter().filter(|c| !connected.contains(c)).copied().collect();
    let new = connected.iter().filter(|c| !open.contains(c)).copied().collect();
    (gone, new)
}

/// The connectors that should be screens, given those connected and those closed for good (`Recovery::Close`):
/// the connected ones not closed, and the closed ones still connected (one unplugged leaves the list, so plugging it
/// in again opens it).
#[must_use]
pub fn screens_wanted<C: Copy + Eq>(connected: &[C], dark: &[C]) -> (Vec<C>, Vec<C>) {
    let wanted = connected.iter().filter(|c| !dark.contains(c)).copied().collect();
    let dark = dark.iter().filter(|c| connected.contains(c)).copied().collect();
    (wanted, dark)
}

/// A CRTC for each connector wanting one, in order, from the CRTCs it can use (best first: the one already driving it
/// leads), none in `taken` and none given twice; None for a connector left without.
#[must_use]
pub fn assign_crtcs<C: Copy + Eq>(wanted: &[Vec<C>], taken: &[C]) -> Vec<Option<C>> {
    let mut used = taken.to_vec();
    (wanted.iter())
        .map(|usable| {
            let crtc = usable.iter().find(|c| !used.contains(c)).copied()?;
            used.push(crtc);
            Some(crtc)
        })
        .collect()
}

/// The mode a screen runs, from whether each of its connector's modes is marked preferred: the first preferred, else
/// the first; None without modes.
#[must_use]
pub fn preferred_mode(preferred: &[bool]) -> Option<usize> {
    preferred
        .iter()
        .position(|&p| p)
        .or((!preferred.is_empty()).then_some(0))
}

/// How long after a flip a screen draws again, `refresh` its rate in mHz: 60% of a frame, so clients have most of it
/// to draw after their frame callback and the next frame still makes the vblank after. Unknown (0) counts as 60 Hz.
#[must_use]
pub fn repaint_delay(refresh: i32) -> Duration {
    let hz = if refresh > 0 { f64::from(refresh) / 1000.0 } else { 60.0 };
    Duration::from_secs_f64(0.6 / hz)
}

/// How long a queued frame's flip is waited for: a page-flip event that never comes must not stop a screen for good.
pub const FLIP_TIMEOUT: Duration = Duration::from_secs(1);

/// Whether a screen whose last frame was queued at `queued` (None: its flip came) still waits for that flip.
#[must_use]
pub fn flip_waits(queued: Option<Instant>, now: Instant) -> bool {
    queued.is_some_and(|t| now.saturating_duration_since(t) < FLIP_TIMEOUT)
}

/// Whether a page-flip event stamped `event` (CLOCK_MONOTONIC; None or zero: no usable stamp) belongs to the frame
/// queued at `queued` on that clock: not when it happened before (a late event of a frame given up on, `flip_waits`).
#[must_use]
pub fn flip_event_matches(queued: Duration, event: Option<Duration>) -> bool {
    event.filter(|t| !t.is_zero()).is_none_or(|t| t >= queued)
}

/// What to do when the kernel refused a screen's new state (an atomic test failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Reset every output once (another session left the CRTCs wired otherwise).
    ResetAll,
    /// The screen failed again after a reset made for it: close it, as if unplugged.
    Close,
    /// A reset was just made (for another screen) and nothing has drawn since: try it once more as it is.
    Wait,
}

/// The recovery for a screen whose frame the kernel refused: `had_reset`, a reset was already made for this screen
/// (and it has drawn nothing since); `reset_spent`, a reset was made since the last frame of any screen (or the last
/// resume or hotplug). At most one reset per round, and never one for a screen that already had its own.
#[must_use]
pub fn recovery(had_reset: bool, reset_spent: bool) -> Recovery {
    match (had_reset, reset_spent) {
        (true, _) => Recovery::Close,
        (false, false) => Recovery::ResetAll,
        (false, true) => Recovery::Wait,
    }
}

/// The screen (by index into `screens`, global rectangles) holding the point.
#[must_use]
pub fn screen_at(screens: &[Rect], (x, y): (f64, f64)) -> Option<usize> {
    let within = |v: f64, from: i32, len: i32| v >= f64::from(from) && v < f64::from(from) + f64::from(len);
    screens.iter().position(|r| within(x, r.x, r.w) && within(y, r.y, r.h))
}

/// The point kept on the screens: as it is when a screen holds it, else the nearest point of the nearest screen (a
/// screen went away under the pointer). None without screens.
#[must_use]
pub fn clamp_to_screens(screens: &[Rect], (x, y): (f64, f64)) -> Option<(f64, f64)> {
    if screen_at(screens, (x, y)).is_some() {
        return Some((x, y));
    }
    // The last pixel of each screen, as `crossing::step` keeps the pointer.
    let clamp = |v: f64, from: i32, len: i32| v.clamp(f64::from(from), f64::from(from) + f64::from(len - 1));
    (screens.iter())
        .filter(|r| r.w > 0 && r.h > 0)
        .map(|r| (clamp(x, r.x, r.w), clamp(y, r.y, r.h)))
        .min_by(|a, b| distance(*a, (x, y)).total_cmp(&distance(*b, (x, y))))
}

fn distance((ax, ay): (f64, f64), (bx, by): (f64, f64)) -> f64 {
    (ax - bx).hypot(ay - by)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn plugged_connectors_open_and_unplugged_ones_close() {
        assert_eq!(connector_changes::<u32>(&[], &[]), (vec![], vec![]));
        assert_eq!(
            connector_changes(&[], &[3, 1]),
            (vec![], vec![3, 1]),
            "the device's order"
        );
        assert_eq!(connector_changes(&[1, 3], &[3, 1]), (vec![], vec![]), "nothing changed");
        assert_eq!(connector_changes(&[1, 3, 5], &[2, 5]), (vec![1, 3], vec![2]));
        assert_eq!(
            connector_changes(&[1], &[]),
            (vec![1], vec![]),
            "the last one unplugged"
        );
    }

    #[test]
    fn each_connector_gets_a_crtc_of_its_own() {
        assert_eq!(assign_crtcs::<u32>(&[], &[]), vec![]);
        // The first takes its best; the second, sharing it, its next.
        assert_eq!(
            assign_crtcs(&[vec![10, 11], vec![10, 11]], &[]),
            vec![Some(10), Some(11)]
        );
        // CRTCs of screens already open are never given.
        assert_eq!(assign_crtcs(&[vec![10, 11, 12]], &[10, 11]), vec![Some(12)]);
        // Run out: that connector stays dark, the next still gets one.
        assert_eq!(
            assign_crtcs(&[vec![10], vec![10], vec![11]], &[]),
            vec![Some(10), None, Some(11)]
        );
        assert_eq!(
            assign_crtcs::<u32>(&[vec![]], &[]),
            vec![None],
            "a connector no CRTC can drive"
        );
    }

    #[test]
    fn screens_run_their_preferred_mode_else_the_first() {
        assert_eq!(preferred_mode(&[]), None);
        assert_eq!(preferred_mode(&[false, true, true]), Some(1));
        assert_eq!(preferred_mode(&[false, false]), Some(0));
    }

    #[test]
    fn a_screen_draws_again_most_of_a_frame_after_its_flip() {
        assert_eq!(repaint_delay(60_000), Duration::from_millis(10));
        assert_eq!(repaint_delay(0), Duration::from_millis(10), "unknown: 60 Hz");
        assert_eq!(repaint_delay(120_000), Duration::from_millis(5));
    }

    #[test]
    fn a_screen_waits_for_its_flip_but_not_for_ever() {
        let t = Instant::now();
        assert!(!flip_waits(None, t), "no flip pending");
        assert!(flip_waits(Some(t), t + Duration::from_millis(999)));
        assert!(!flip_waits(Some(t), t + FLIP_TIMEOUT), "the event is taken as lost");
    }

    #[test]
    fn a_connector_closed_for_good_stays_dark_until_unplugged() {
        assert_eq!(screens_wanted::<u32>(&[1, 2], &[]), (vec![1, 2], vec![]));
        assert_eq!(
            screens_wanted(&[1, 2], &[2]),
            (vec![1], vec![2]),
            "still plugged: still dark"
        );
        assert_eq!(screens_wanted(&[1], &[2]), (vec![1], vec![]), "unplugged: forgotten");
        assert_eq!(
            screens_wanted(&[1, 2], &[]),
            (vec![1, 2], vec![]),
            "plugged in again: opens"
        );
    }

    #[test]
    fn a_late_flip_event_is_not_taken_for_the_next_frame() {
        let queued = Duration::from_millis(5000);
        assert!(flip_event_matches(queued, Some(Duration::from_millis(5016))));
        assert!(flip_event_matches(queued, Some(queued)));
        assert!(
            !flip_event_matches(queued, Some(Duration::from_millis(4000))),
            "a frame given up on"
        );
        assert!(flip_event_matches(queued, None), "no stamp: taken as it comes");
        assert!(flip_event_matches(queued, Some(Duration::ZERO)), "a zero stamp is none");
    }

    #[test]
    fn a_refused_screen_gets_one_reset_then_goes_dark() {
        assert_eq!(recovery(false, false), Recovery::ResetAll, "the first failure");
        assert_eq!(recovery(true, true), Recovery::Close, "failed again after its reset");
        assert_eq!(
            recovery(true, false),
            Recovery::Close,
            "others drew since: still its second failure"
        );
        assert_eq!(
            recovery(false, true),
            Recovery::Wait,
            "another screen's reset was just made"
        );
    }

    const TWO: [Rect; 2] = [rect(0, 0, 1920, 1080), rect(1920, 0, 1280, 1024)];

    #[test]
    fn the_screen_under_a_point() {
        assert_eq!(screen_at(&TWO, (0.0, 0.0)), Some(0));
        assert_eq!(screen_at(&TWO, (1919.9, 1079.9)), Some(0));
        assert_eq!(
            screen_at(&TWO, (1920.0, 500.0)),
            Some(1),
            "edges belong to the screen they start"
        );
        assert_eq!(screen_at(&TWO, (2000.0, 1050.0)), None, "below the shorter screen");
        assert_eq!(screen_at(&TWO, (-1.0, 5.0)), None);
        assert_eq!(screen_at(&[], (0.0, 0.0)), None);
    }

    #[test]
    fn a_pointer_off_the_screens_comes_back_to_the_nearest() {
        assert_eq!(
            clamp_to_screens(&TWO, (100.5, 200.25)),
            Some((100.5, 200.25)),
            "on a screen: kept"
        );
        // The second screen went away under it: onto the first's right edge, at the same height.
        assert_eq!(clamp_to_screens(&TWO[..1], (2500.0, 300.0)), Some((1919.0, 300.0)));
        // Below the shorter screen: the nearest is its bottom edge.
        assert_eq!(clamp_to_screens(&TWO, (2000.0, 1050.0)), Some((2000.0, 1023.0)));
        assert_eq!(clamp_to_screens(&TWO, (-50.0, -50.0)), Some((0.0, 0.0)));
        assert_eq!(clamp_to_screens(&[], (5.0, 5.0)), None);
        assert_eq!(
            clamp_to_screens(&[rect(0, 0, 0, 0)], (5.0, 5.0)),
            None,
            "an empty screen holds nothing"
        );
    }
}
