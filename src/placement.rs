//! Where a new window goes, decided at its first buffer: on its parent, as a launch's new tab, or by `stacking`.
//!
//! Pure: the compositor gathers the facts (the parent, the client's pid chain, the time) and applies the answer
//! with `Stacks`. Launches are forgotten lazily, by `expire`, on the next spawn or placement; no timers.

use std::time::{Duration, Instant};

use crate::stacks::WinId;

/// How long a launch waits for its window: after this it is forgotten, pid matching included.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(5);

/// How many pids `pid_chain` follows, the client's own included: deep enough for any launcher's wrappers, short
/// enough that a pathological /proc cannot keep a commit waiting.
pub const CHAIN_MAX: usize = 32;

/// A program orrery started (bind `spawn` or `msg spawn`) and where its window should open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub pid: u32,
    /// The xdg-activation token handed to the program, if any.
    pub token: Option<String>,
    /// The monitor (by name) and slot focused when it was started.
    pub monitor: String,
    pub slot: usize,
    pub at: Instant,
    /// A window has opened for it; a launch places one window at most.
    pub claimed: bool,
}

/// The live launches, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launches {
    launches: Vec<Launch>,
}

impl Launches {
    pub fn push(&mut self, launch: Launch) {
        self.launches.push(launch);
    }

    /// Forgets launches `LAUNCH_WAIT` old or older, claimed or not.
    pub fn expire(&mut self, now: Instant) {
        self.launches.retain(|l| young(l, now));
    }

    #[must_use]
    pub fn all(&self) -> &[Launch] {
        &self.launches
    }

    /// Whether any launch still waits for a window (as of the last `expire`).
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.launches.iter().any(|l| !l.claimed)
    }

    /// Claims the launch of the first pid in `pids` (a client's own pid, then its ancestors) that has an
    /// unclaimed one; None when none has.
    pub fn claim_by_pids(&mut self, pids: &[u32]) -> Option<&Launch> {
        let i = pids
            .iter()
            .find_map(|&pid| self.launches.iter().position(|l| !l.claimed && l.pid == pid))?;
        Some(self.claim(i))
    }

    /// Claims the oldest unclaimed launch younger than `LAUNCH_WAIT` at `now`.
    pub fn claim_oldest(&mut self, now: Instant) -> Option<&Launch> {
        let i = self.launches.iter().position(|l| !l.claimed && young(l, now))?;
        Some(self.claim(i))
    }

    fn claim(&mut self, i: usize) -> &Launch {
        let l = &mut self.launches[i];
        l.claimed = true;
        l
    }
}

/// Under `LAUNCH_WAIT` old at `now`.
fn young(l: &Launch, now: Instant) -> bool {
    now.saturating_duration_since(l.at) < LAUNCH_WAIT
}

/// Where `decide` sends a new window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    /// On top of this (placed) parent's stack.
    OnParent(WinId),
    /// A new tab in a launch's target, current and focused (`Stacks::open_tab` falls back to the focused slot
    /// when the target is gone).
    NewTab { monitor: String, slot: usize },
    /// On top of the focused slot's current stack (stacking on).
    Stacked,
    /// A new tab in the focused slot (stacking off).
    Tab,
}

/// The rules, in order: a placed parent; a launch of the client or one of its ancestors (`pid_chain`, client
/// first); the oldest unclaimed launch under `LAUNCH_WAIT`; else `stacking` decides. A launch that places the
/// window is claimed. Expired launches are dropped first.
pub fn decide(
    parent: Option<WinId>,
    parent_placed: bool,
    pid_chain: &[u32],
    launches: &mut Launches,
    now: Instant,
    stacking: bool,
) -> Landing {
    launches.expire(now);
    if let Some(p) = parent.filter(|_| parent_placed) {
        return Landing::OnParent(p);
    }
    let launch = match launches.claim_by_pids(pid_chain) {
        Some(l) => Some(l),
        None => launches.claim_oldest(now),
    };
    match launch {
        Some(l) => Landing::NewTab {
            monitor: l.monitor.clone(),
            slot: l.slot,
        },
        None if stacking => Landing::Stacked,
        None => Landing::Tab,
    }
}

/// The parent pid in the text of `/proc/<pid>/stat`: the field after the state, which follows the command name
/// in parentheses. The name may itself hold `)` and spaces, so the split is at the last `)`.
#[must_use]
pub fn ppid_of(stat: &str) -> Option<u32> {
    let (_, after_name) = stat.rsplit_once(')')?;
    let mut fields = after_name.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// `pid`, then its parent, grandparent and so on, as `stat_of` (a pid's `/proc/<pid>/stat`, None when gone)
/// tells them; it stops before pid 1 or 0, at a pid it cannot read, and after `CHAIN_MAX` pids.
pub fn pid_chain(pid: u32, mut stat_of: impl FnMut(u32) -> Option<String>) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut pid = pid;
    while pid > 1 && chain.len() < CHAIN_MAX {
        chain.push(pid);
        match stat_of(pid).as_deref().and_then(ppid_of) {
            Some(ppid) => pid = ppid,
            None => break,
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(pid: u32, slot: usize, at: Instant) -> Launch {
        Launch {
            pid,
            token: None,
            monitor: "m".into(),
            slot,
            at,
            claimed: false,
        }
    }

    fn tab(slot: usize) -> Landing {
        Landing::NewTab {
            monitor: "m".into(),
            slot,
        }
    }

    fn launches(list: &[Launch]) -> Launches {
        let mut l = Launches::default();
        for x in list {
            l.push(x.clone());
        }
        l
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn nothing_known_follows_stacking() {
        let now = Instant::now();
        let mut none = Launches::default();
        assert_eq!(decide(None, false, &[40], &mut none, now, true), Landing::Stacked);
        assert_eq!(decide(None, false, &[40], &mut none, now, false), Landing::Tab);
    }

    #[test]
    fn a_placed_parent_wins_and_claims_no_launch() {
        let t0 = Instant::now();
        let mut l = launches(&[launch(40, 2, t0)]);
        assert_eq!(decide(Some(7), true, &[40], &mut l, t0, true), Landing::OnParent(7));
        assert_eq!(decide(Some(7), true, &[40], &mut l, t0, false), Landing::OnParent(7));
        assert!(!l.all()[0].claimed, "the launch still waits for its own window");
        // A parent that is not placed (not mapped yet, or pending with no monitor) is no parent: the next rule applies.
        assert_eq!(decide(Some(7), false, &[40], &mut l, t0, true), tab(2));
        assert_eq!(decide(Some(7), false, &[], &mut l, t0, true), Landing::Stacked);
    }

    #[test]
    fn the_launched_pid_or_an_ancestor_matches_its_launch() {
        let t0 = Instant::now();
        let mut l = launches(&[launch(10, 1, t0), launch(20, 2, t0), launch(30, 3, t0)]);
        // The client is a grandchild of 30: chain 99 -> 98 -> 30.
        assert_eq!(decide(None, false, &[99, 98, 30], &mut l, t0, true), tab(3));
        // The client's own pid comes before its ancestors.
        assert_eq!(decide(None, false, &[20, 10], &mut l, t0, true), tab(2));
        assert_eq!(decide(None, false, &[10], &mut l, t0, true), tab(1));
    }

    #[test]
    fn a_pid_match_beats_an_older_launch() {
        let t0 = Instant::now();
        let mut l = launches(&[launch(10, 1, t0), launch(20, 2, t0 + MS)]);
        assert_eq!(decide(None, false, &[20], &mut l, t0 + 2 * MS, true), tab(2));
        // The older one is still there for the next stranger.
        assert_eq!(decide(None, false, &[55], &mut l, t0 + 2 * MS, true), tab(1));
    }

    #[test]
    fn a_stranger_takes_the_oldest_unclaimed_launch_in_order() {
        let t0 = Instant::now();
        let mut l = launches(&[launch(10, 1, t0), launch(20, 2, t0 + MS)]);
        let now = t0 + 10 * MS;
        assert_eq!(decide(None, false, &[55], &mut l, now, false), tab(1));
        assert_eq!(decide(None, false, &[56], &mut l, now, false), tab(2));
        // Both claimed: back to stacking.
        assert_eq!(decide(None, false, &[57], &mut l, now, true), Landing::Stacked);
        assert_eq!(decide(None, false, &[57], &mut l, now, false), Landing::Tab);
    }

    #[test]
    fn each_launch_places_one_window() {
        let t0 = Instant::now();
        let mut l = launches(&[launch(10, 1, t0)]);
        assert_eq!(decide(None, false, &[10], &mut l, t0, true), tab(1));
        assert!(l.all()[0].claimed);
        // Its second window, by pid or as a stranger, is just a window.
        assert_eq!(decide(None, false, &[10], &mut l, t0, true), Landing::Stacked);
        assert_eq!(decide(None, false, &[55], &mut l, t0, true), Landing::Stacked);
        assert!(!l.waiting());
    }

    #[test]
    fn five_seconds_is_the_edge_for_both_rules() {
        let t0 = Instant::now();
        let just_under = t0 + LAUNCH_WAIT - MS;
        let at_edge = t0 + LAUNCH_WAIT;
        // Under 5 s: both rules still match.
        let mut l = launches(&[launch(10, 1, t0)]);
        assert_eq!(decide(None, false, &[55], &mut l, just_under, true), tab(1));
        let mut l = launches(&[launch(10, 1, t0)]);
        assert_eq!(decide(None, false, &[10], &mut l, just_under, true), tab(1));
        // At 5 s: forgotten, pid matching included.
        let mut l = launches(&[launch(10, 1, t0)]);
        assert_eq!(decide(None, false, &[55], &mut l, at_edge, true), Landing::Stacked);
        assert!(l.all().is_empty());
        let mut l = launches(&[launch(10, 1, t0)]);
        assert_eq!(decide(None, false, &[10], &mut l, at_edge, true), Landing::Stacked);
    }

    #[test]
    fn expire_drops_old_launches_claimed_or_not_and_keeps_young_ones() {
        let t0 = Instant::now();
        let mut l = launches(&[launch(10, 1, t0), launch(20, 2, t0 + MS), launch(30, 3, t0 + 2 * MS)]);
        assert_eq!(l.claim_by_pids(&[20]).map(|x| x.pid), Some(20));
        assert!(l.waiting());
        l.expire(t0 + LAUNCH_WAIT + MS);
        assert_eq!(l.all().iter().map(|x| x.pid).collect::<Vec<_>>(), [30]);
        assert!(l.waiting());
        l.expire(t0 + 2 * MS + LAUNCH_WAIT);
        assert!(l.all().is_empty() && !l.waiting());
    }

    #[test]
    fn claim_oldest_skips_claimed_and_too_old_launches() {
        let t0 = Instant::now();
        // Not expired yet (expire was not called), but too old to claim.
        let mut l = launches(&[launch(10, 1, t0), launch(20, 2, t0 + 3 * LAUNCH_WAIT / 2)]);
        let now = t0 + 2 * LAUNCH_WAIT;
        assert_eq!(l.claim_oldest(now).map(|x| x.pid), Some(20));
        assert_eq!(l.claim_oldest(now), None);
        assert_eq!(l.claim_by_pids(&[20]), None, "claimed once");
    }

    #[test]
    fn ppid_is_the_field_after_the_command_name() {
        assert_eq!(ppid_of("1234 (sleep) S 99 1234 1234 0 -1 4194304"), Some(99));
        // A name with spaces and parentheses: only the last `)` ends it.
        assert_eq!(ppid_of("77 (a) b (c)) R 5 77 77 0"), Some(5));
        assert_eq!(ppid_of("77 (x) 3) S 6 7"), Some(6), "a name ending `) 3`");
        assert_eq!(ppid_of("1 (init) S 0 1 1"), Some(0));
        for bad in ["", "12 (x", "12 (x) S", "12 (x) S -1 0", "12 (x) S abc"] {
            assert_eq!(ppid_of(bad), None, "{bad:?}");
        }
    }

    /// A process table as /proc would tell it: pid -> ppid.
    fn table(edges: &[(u32, u32)]) -> impl FnMut(u32) -> Option<String> {
        let edges = edges.to_vec();
        move |pid| {
            let (_, ppid) = edges.iter().find(|(p, _)| *p == pid)?;
            Some(format!("{pid} (proc {pid}) S {ppid} 0 0"))
        }
    }

    #[test]
    fn pid_chain_walks_up_to_init() {
        let chain = pid_chain(50, table(&[(50, 40), (40, 30), (30, 1), (1, 0)]));
        assert_eq!(chain, [50, 40, 30]);
        // A parent that is gone (no stat) ends the chain there.
        assert_eq!(pid_chain(50, table(&[(50, 40)])), [50, 40]);
        // No stat for the client itself: still its own pid.
        assert_eq!(pid_chain(50, table(&[])), [50]);
        // In a pid namespace, the top may be 0's child without a 1 in between.
        assert_eq!(pid_chain(50, table(&[(50, 9), (9, 0)])), [50, 9]);
        assert_eq!(pid_chain(0, table(&[])), Vec::<u32>::new());
        assert_eq!(pid_chain(1, table(&[(1, 0)])), Vec::<u32>::new());
    }

    #[test]
    fn pid_chain_stops_after_its_cap_even_in_a_loop() {
        let deep: Vec<(u32, u32)> = (2..200).map(|p| (p, p - 1)).collect();
        let chain = pid_chain(199, table(&deep));
        assert_eq!(chain.len(), CHAIN_MAX);
        assert_eq!(chain[0], 199);
        assert_eq!(chain[CHAIN_MAX - 1], 199 - (CHAIN_MAX as u32 - 1));
        // A table that lies (a loop) cannot keep it going.
        assert_eq!(pid_chain(5, table(&[(5, 6), (6, 5)])).len(), CHAIN_MAX);
    }
}
