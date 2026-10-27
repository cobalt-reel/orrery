//! The session: one primary and its satellites, the order of their monitors, and which machine owns each workspace
//! number.
//!
//! Pure, and kept by the primary. Workspace numbers (1..=WORKSPACES) are one space across the session: each is owned
//! by at most one machine, which has it in its `Stacks`, and is foreign to every other one (`Stacks::set_foreign`).
//! A screen's number is that of the workspace it locks, so it is owned too. Numbers never change on a drop: the
//! satellite keeps them, the session only frees them. On a (re)join the satellite keeps each number still free and
//! renames each taken one. Windows never cross machines.

use std::collections::BTreeSet;

use crate::stacks::WORKSPACES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    Satellite,
}

/// A machine's screen: its output's name and its number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub name: String,
    pub number: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Unique in the session.
    name: String,
    role: Role,
    /// In number order.
    screens: Vec<Screen>,
    /// The workspace numbers it owns, its screens' among them.
    workspaces: BTreeSet<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The primary first, then the satellites in the order they joined.
    machines: Vec<Machine>,
    /// The machine holding the session's focus marker, by index.
    focused: usize,
}

/// Where `workspace N` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The primary has it: its own `Stacks::workspace` shows it.
    Local,
    /// That satellite (by name) has it and shows it.
    On(String),
    /// Nobody has it: that machine (by name), the focused one, makes it with that number (`Stacks::workspace`) and
    /// owns it from now on.
    Create(String),
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum Refused {
    #[error("a machine named {name:?} is already in the session")]
    DuplicateName { name: String },
    #[error("no machine named {name:?} in the session")]
    UnknownMachine { name: String },
    #[error("workspace {number} is not 1 to {WORKSPACES}")]
    OutOfRange { number: u8 },
    #[error("two screens numbered {number}")]
    ScreenTwice { number: u8 },
    #[error("no free workspace number left for workspace {number}")]
    Full { number: u8 },
    #[error("workspace {number} is on {machine}: windows never cross machines")]
    OtherMachine { number: u8, machine: String },
}

impl Machine {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// In number order.
    #[must_use]
    pub fn screens(&self) -> &[Screen] {
        &self.screens
    }

    #[must_use]
    pub fn workspaces(&self) -> &BTreeSet<u8> {
        &self.workspaces
    }
}

impl Session {
    /// The primary `name` alone, owning nothing until it reports.
    #[must_use]
    pub fn new(name: &str) -> Self {
        let primary = Machine {
            name: name.to_owned(),
            role: Role::Primary,
            screens: Vec::new(),
            workspaces: BTreeSet::new(),
        };
        Self {
            machines: vec![primary],
            focused: 0,
        }
    }

    /// The primary first, then the satellites in join order.
    #[must_use]
    pub fn machines(&self) -> &[Machine] {
        &self.machines
    }

    /// The machine holding the session's focus marker.
    #[must_use]
    pub fn focused(&self) -> &Machine {
        &self.machines[self.focused]
    }

    #[must_use]
    pub fn owner(&self, n: u8) -> Option<&Machine> {
        self.machines.iter().find(|m| m.workspaces.contains(&n))
    }

    /// The numbers other machines own: what machine `name`'s `Stacks` holds as foreign.
    #[must_use]
    pub fn foreign(&self, name: &str) -> BTreeSet<u8> {
        (self.machines.iter())
            .filter(|m| m.name != name)
            .flat_map(|m| m.workspaces.iter().copied())
            .collect()
    }

    /// Every monitor in the session with its machine: the primary's first, then each satellite's in join order,
    /// each machine's in number order.
    // No #[must_use]: iterators already carry it.
    pub fn monitors(&self) -> impl Iterator<Item = (&Machine, &Screen)> + '_ {
        (self.machines.iter()).flat_map(|m| m.screens.iter().map(move |s| (m, s)))
    }

    /// Machine `name` (the primary too) says what it has now, after any change: its numbers become these and its
    /// screens' (claims made by `workspace` and `move_to_workspace` included, so a workspace gone frees its
    /// number). Numbers another machine owns are left out and returned: it should not have made them. Refused, with
    /// nothing changed, for an unknown machine or bad numbers.
    pub fn report(
        &mut self,
        name: &str,
        screens: Vec<Screen>,
        workspaces: impl IntoIterator<Item = u8>,
    ) -> Result<Vec<u8>, Refused> {
        let i = self
            .index_of(name)
            .ok_or_else(|| Refused::UnknownMachine { name: name.to_owned() })?;
        let (screens, owned) = checked(screens, workspaces)?;
        let others = self.foreign(name);
        let (taken, mine): (BTreeSet<u8>, BTreeSet<u8>) = owned.into_iter().partition(|n| others.contains(n));
        let m = &mut self.machines[i];
        m.screens = screens.into_iter().filter(|s| !taken.contains(&s.number)).collect();
        m.workspaces = mine;
        Ok(taken.into_iter().collect())
    }

    /// Satellite `name` joins with its screens and workspaces. Each of its numbers still free is kept; each taken one
    /// moves, in number order, to the lowest number neither taken nor kept. Returns those renames (from, to) for the
    /// satellite to apply (`Stacks::renumber_workspace`, in any order: no rename lands on another's source). Its
    /// monitors come after every machine already in. Refused, with nothing changed, for a name already in the
    /// session, bad numbers, or more renames than free numbers.
    pub fn join(
        &mut self,
        name: &str,
        screens: Vec<Screen>,
        workspaces: impl IntoIterator<Item = u8>,
    ) -> Result<Vec<(u8, u8)>, Refused> {
        if self.index_of(name).is_some() {
            return Err(Refused::DuplicateName { name: name.to_owned() });
        }
        let (mut screens, owned) = checked(screens, workspaces)?;
        let mut taken = self.foreign(name);
        let (collide, mut mine): (BTreeSet<u8>, BTreeSet<u8>) = owned.into_iter().partition(|n| taken.contains(n));
        taken.extend(&mine);
        let mut renames = Vec::new();
        for from in collide {
            let to = (1..=WORKSPACES)
                .find(|k| !taken.contains(k))
                .ok_or(Refused::Full { number: from })?;
            taken.insert(to);
            mine.insert(to);
            renames.push((from, to));
        }
        for s in &mut screens {
            if let Some(&(_, to)) = renames.iter().find(|(from, _)| *from == s.number) {
                s.number = to;
            }
        }
        screens.sort_by_key(|s| s.number);
        self.machines.push(Machine {
            name: name.to_owned(),
            role: Role::Satellite,
            screens,
            workspaces: mine,
        });
        Ok(renames)
    }

    /// Satellite `name` lost its link: its monitors leave the session and its numbers are free (it keeps them, and
    /// asks for them back when it rejoins). The focus marker goes to the primary if it was there. `false` (and
    /// nothing done) for the primary or an unknown name.
    pub fn drop_satellite(&mut self, name: &str) -> bool {
        let Some(i) = self.index_of(name).filter(|&i| i > 0) else {
            return false;
        };
        self.machines.remove(i);
        if self.focused == i {
            self.focused = 0;
        } else if self.focused > i {
            self.focused -= 1;
        }
        true
    }

    /// Where `workspace n` goes: to the machine owning it, which the focus marker moves to, else made on the focused
    /// machine, which owns it from now on. None (and nothing done) outside 1..=WORKSPACES.
    pub fn workspace(&mut self, n: u8) -> Option<Route> {
        if !(1..=WORKSPACES).contains(&n) {
            return None;
        }
        let Some(owner) = self.machines.iter().position(|m| m.workspaces.contains(&n)) else {
            let m = &mut self.machines[self.focused];
            m.workspaces.insert(n);
            return Some(Route::Create(m.name.clone()));
        };
        self.focused = owner;
        Some(match self.machines[owner].role {
            Role::Primary => Route::Local,
            Role::Satellite => Route::On(self.machines[owner].name.clone()),
        })
    }

    /// `move-to-workspace n` run on machine `from`: refused when another machine owns `n`, as windows never cross
    /// machines; a number nobody owns is claimed for `from`, which makes it. The focus marker stays.
    pub fn move_to_workspace(&mut self, from: &str, n: u8) -> Result<(), Refused> {
        let i = self
            .index_of(from)
            .ok_or_else(|| Refused::UnknownMachine { name: from.to_owned() })?;
        if !(1..=WORKSPACES).contains(&n) {
            return Err(Refused::OutOfRange { number: n });
        }
        match self.owner(n) {
            Some(m) if m.name != from => Err(Refused::OtherMachine {
                number: n,
                machine: m.name.clone(),
            }),
            _ => {
                self.machines[i].workspaces.insert(n);
                Ok(())
            }
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.machines.iter().position(|m| m.name == name)
    }
}

/// A machine's screens, in number order, and the numbers it owns, theirs included; refused for a number out of
/// range or two screens with one number.
fn checked(
    mut screens: Vec<Screen>,
    workspaces: impl IntoIterator<Item = u8>,
) -> Result<(Vec<Screen>, BTreeSet<u8>), Refused> {
    let mut owned = BTreeSet::new();
    for n in workspaces.into_iter().chain(screens.iter().map(|s| s.number)) {
        if !(1..=WORKSPACES).contains(&n) {
            return Err(Refused::OutOfRange { number: n });
        }
        owned.insert(n);
    }
    screens.sort_by_key(|s| s.number);
    if let Some(w) = screens.windows(2).find(|w| w[0].number == w[1].number) {
        return Err(Refused::ScreenTwice { number: w[0].number });
    }
    Ok((screens, owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use crate::layout::{Layout, Rect};
    use crate::stacks::{Outcome, Stacks};

    fn screen(name: &str, number: u8) -> Screen {
        Screen {
            name: name.into(),
            number,
        }
    }

    /// The primary "p": screen "p1" numbered 1, workspaces 1 and 3.
    fn primary() -> Session {
        let mut s = Session::new("p");
        assert_eq!(s.report("p", vec![screen("p1", 1)], [1, 3]), Ok(vec![]));
        s
    }

    /// Each machine's name and the numbers it owns.
    fn owners(s: &Session) -> Vec<(&str, Vec<u8>)> {
        (s.machines().iter())
            .map(|m| (m.name(), m.workspaces().iter().copied().collect()))
            .collect()
    }

    /// The session's monitors as (machine, screen name, number), in order.
    fn monitors(s: &Session) -> Vec<(&str, &str, u8)> {
        s.monitors()
            .map(|(m, sc)| (m.name(), sc.name.as_str(), sc.number))
            .collect()
    }

    #[test]
    fn the_primary_alone_owns_what_it_reports() {
        let mut s = Session::new("p");
        assert_eq!(s.machines().len(), 1);
        assert_eq!(s.machines()[0].role(), Role::Primary);
        assert_eq!(s.focused().name(), "p");
        // Its screens' numbers are its workspaces too.
        assert_eq!(s.report("p", vec![screen("p2", 2), screen("p1", 1)], [5]), Ok(vec![]));
        assert_eq!(owners(&s), [("p", vec![1, 2, 5])]);
        assert_eq!(
            monitors(&s),
            [("p", "p1", 1), ("p", "p2", 2)],
            "screens in number order"
        );
        assert_eq!(s.owner(5).map(Machine::name), Some("p"));
        assert_eq!(s.owner(3), None);
        assert!(s.foreign("p").is_empty());
        // Refused, nothing changed: an unknown machine, a number out of range, a screen number twice.
        let before = s.clone();
        assert_eq!(
            s.report("x", vec![], [1]),
            Err(Refused::UnknownMachine { name: "x".into() })
        );
        assert_eq!(s.report("p", vec![], [11]), Err(Refused::OutOfRange { number: 11 }));
        assert_eq!(
            s.report("p", vec![screen("a", 0)], []),
            Err(Refused::OutOfRange { number: 0 })
        );
        assert_eq!(
            s.report("p", vec![screen("a", 2), screen("b", 2)], []),
            Err(Refused::ScreenTwice { number: 2 })
        );
        assert_eq!(s, before);
    }

    #[test]
    fn a_join_keeps_free_numbers_and_monitors_follow_the_join_order() {
        let mut s = primary();
        assert_eq!(s.join("a", vec![screen("a1", 2)], [2, 5]), Ok(vec![]));
        assert_eq!(s.join("b", vec![screen("b1", 4)], [4, 6]), Ok(vec![]));
        assert_eq!(owners(&s), [("p", vec![1, 3]), ("a", vec![2, 5]), ("b", vec![4, 6])]);
        assert_eq!(s.machines()[1].role(), Role::Satellite);
        assert_eq!(monitors(&s), [("p", "p1", 1), ("a", "a1", 2), ("b", "b1", 4)]);
        assert_eq!(s.foreign("a"), BTreeSet::from([1, 3, 4, 6]));
        assert_eq!(s.foreign("p"), BTreeSet::from([2, 4, 5, 6]));
        assert_eq!(s.focused().name(), "p", "a join takes no focus");
    }

    #[test]
    fn a_taken_number_moves_to_the_lowest_one_neither_taken_nor_kept() {
        let mut s = primary();
        // 1 and 3 are the primary's; 2 is kept, so 1 goes to 4 and 3 to 5.
        let renames = s.join("a", vec![screen("a1", 1), screen("a2", 2)], [1, 2, 3]);
        assert_eq!(renames, Ok(vec![(1, 4), (3, 5)]));
        assert_eq!(owners(&s), [("p", vec![1, 3]), ("a", vec![2, 4, 5])]);
        assert_eq!(monitors(&s), [("p", "p1", 1), ("a", "a2", 2), ("a", "a1", 4)]);
    }

    #[test]
    fn a_join_is_refused_for_a_name_taken_bad_numbers_or_too_few_free() {
        let mut s = primary();
        assert_eq!(s.join("a", vec![], [2, 4, 5, 6, 7, 8]), Ok(vec![]));
        let before = s.clone();
        for name in ["p", "a"] {
            assert_eq!(
                s.join(name, vec![], [9]),
                Err(Refused::DuplicateName { name: name.into() })
            );
        }
        assert_eq!(s.join("b", vec![], [0]), Err(Refused::OutOfRange { number: 0 }));
        assert_eq!(
            s.join("b", vec![screen("b1", 9), screen("b2", 9)], []),
            Err(Refused::ScreenTwice { number: 9 })
        );
        // 9 and 10 are free, but three of b's numbers are taken.
        assert_eq!(s.join("b", vec![], [1, 2, 3]), Err(Refused::Full { number: 3 }));
        assert_eq!(s, before);
        assert_eq!(s.join("b", vec![], [1, 2]), Ok(vec![(1, 9), (2, 10)]));
    }

    #[test]
    fn a_drop_frees_numbers_and_monitors_and_the_focus_goes_home() {
        let mut s = primary();
        s.join("a", vec![screen("a1", 2)], [2]).expect("joins");
        s.join("b", vec![screen("b1", 4)], [4, 5]).expect("joins");
        assert_eq!(s.workspace(5), Some(Route::On("b".into())));
        assert_eq!(s.focused().name(), "b");
        assert!(s.drop_satellite("b"));
        assert_eq!(owners(&s), [("p", vec![1, 3]), ("a", vec![2])]);
        assert_eq!(monitors(&s), [("p", "p1", 1), ("a", "a1", 2)]);
        assert_eq!(s.owner(5), None);
        assert_eq!(s.focused().name(), "p");
        // The marker on a machine after the one dropped stays there.
        s.join("b", vec![], [4]).expect("joins");
        s.workspace(4);
        assert!(s.drop_satellite("a"));
        assert_eq!(s.focused().name(), "b");
        // Never the primary, nor an unknown machine.
        let before = s.clone();
        assert!(!s.drop_satellite("p"));
        assert!(!s.drop_satellite("x"));
        assert_eq!(s, before);
    }

    #[test]
    fn a_rejoin_keeps_its_numbers_unless_one_was_taken_meanwhile() {
        let mut s = primary();
        s.join("a", vec![screen("a1", 2)], [2, 4, 5]).expect("joins");
        s.drop_satellite("a");
        assert_eq!(
            s.join("a", vec![screen("a1", 2)], [2, 4, 5]),
            Ok(vec![]),
            "nothing taken"
        );
        assert_eq!(owners(&s), [("p", vec![1, 3]), ("a", vec![2, 4, 5])]);
        // Dropped again; meanwhile the primary makes 2 and 4 (its screen 2 among them).
        s.drop_satellite("a");
        assert_eq!(
            s.report("p", vec![screen("p1", 1), screen("p2", 2)], [1, 3, 4]),
            Ok(vec![])
        );
        let renames = s.join("a", vec![screen("a1", 2)], [2, 4, 5]);
        assert_eq!(renames, Ok(vec![(2, 6), (4, 7)]));
        assert_eq!(monitors(&s), [("p", "p1", 1), ("p", "p2", 2), ("a", "a1", 6)]);
        assert_eq!(owners(&s), [("p", vec![1, 2, 3, 4]), ("a", vec![5, 6, 7])]);
    }

    #[test]
    fn workspace_goes_to_its_owner_or_is_made_on_the_focused_machine() {
        let mut s = primary();
        s.join("a", vec![screen("a1", 2)], [2, 5]).expect("joins");
        assert_eq!(s.workspace(3), Some(Route::Local));
        assert_eq!(s.workspace(5), Some(Route::On("a".into())));
        assert_eq!(s.focused().name(), "a", "the marker moves to its machine");
        // Nobody has 7: the focused machine makes it, and owns it from now on.
        assert_eq!(s.workspace(7), Some(Route::Create("a".into())));
        assert_eq!(s.owner(7).map(Machine::name), Some("a"));
        assert_eq!(s.workspace(7), Some(Route::On("a".into())));
        assert_eq!(s.workspace(1), Some(Route::Local));
        assert_eq!(s.focused().name(), "p");
        assert_eq!(s.workspace(8), Some(Route::Create("p".into())));
        assert_eq!(s.owner(8).map(Machine::name), Some("p"));
        // Out of range: nowhere, nothing changed.
        let before = s.clone();
        assert_eq!(s.workspace(0), None);
        assert_eq!(s.workspace(11), None);
        assert_eq!(s, before);
    }

    #[test]
    fn moves_never_cross_machines() {
        let mut s = primary();
        s.join("a", vec![screen("a1", 2)], [2, 5]).expect("joins");
        let refused = s.move_to_workspace("p", 5);
        assert_eq!(
            refused,
            Err(Refused::OtherMachine {
                number: 5,
                machine: "a".into()
            })
        );
        let refused = s.move_to_workspace("a", 1).expect_err("the primary's");
        assert_eq!(refused.to_string(), "workspace 1 is on p: windows never cross machines");
        // Its own, or one nobody has, which it makes and now owns.
        assert_eq!(s.move_to_workspace("p", 3), Ok(()));
        assert_eq!(s.move_to_workspace("a", 9), Ok(()));
        assert_eq!(s.owner(9).map(Machine::name), Some("a"));
        assert_eq!(
            s.move_to_workspace("x", 9),
            Err(Refused::UnknownMachine { name: "x".into() })
        );
        assert_eq!(s.move_to_workspace("p", 11), Err(Refused::OutOfRange { number: 11 }));
        assert_eq!(s.focused().name(), "p", "a move moves no focus");
    }

    #[test]
    fn a_report_replaces_what_a_machine_owns_but_never_takes_anothers() {
        let mut s = primary();
        s.join("a", vec![screen("a1", 2)], [2, 5]).expect("joins");
        // a's 5 went away (empty, hidden), it made 6; it also claims the primary's 3, which it must not have.
        assert_eq!(s.report("a", vec![screen("a1", 2)], [2, 3, 6]), Ok(vec![3]));
        assert_eq!(owners(&s), [("p", vec![1, 3]), ("a", vec![2, 6])]);
        assert_eq!(s.owner(5), None);
    }

    fn layout(name: &str) -> Layout {
        (default_config().layouts.into_iter())
            .find(|l| l.name == name)
            .expect("layout in the default config")
    }

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    /// What a machine's `Stacks` tells the session: its screens and workspaces.
    fn state(st: &Stacks) -> (Vec<Screen>, Vec<u8>) {
        let screens = st.monitors().iter().map(|m| screen(m.name(), m.number())).collect();
        (screens, st.workspaces().iter().map(|w| w.number()).collect())
    }

    #[test]
    fn stacks_follow_the_session_through_a_join_a_drop_and_a_rejoin() {
        // The primary: screen 1, workspace 3 holding a window. The satellite: screen 1, workspace 2 holding one.
        let mut p = Stacks::new();
        p.add_monitor("p1", AREA, layout("wall"));
        p.workspace(3);
        p.open_stacked(1);
        p.workspace(1);
        let mut a = Stacks::new();
        a.add_monitor("a1", AREA, layout("halves"));
        a.workspace(2);
        a.open_stacked(10);
        a.workspace(1);
        let mut s = Session::new("p");
        let (screens, numbers) = state(&p);
        s.report("p", screens, numbers).expect("valid");
        // Join: a's 1 collides and goes to 4 (2 is kept), screen and all.
        let (screens, numbers) = state(&a);
        let renames = s.join("a", screens, numbers).expect("joins");
        assert_eq!(renames, [(1, 4)]);
        for (from, to) in renames {
            assert!(a.renumber_workspace(from, to));
        }
        a.set_foreign(s.foreign("a"));
        p.set_foreign(s.foreign("p"));
        assert_eq!(a.monitors()[0].number(), 4);
        assert_eq!(
            state(&a).1,
            s.machines()[1].workspaces().iter().copied().collect::<Vec<_>>()
        );
        // workspace 2 asked on the primary: a has it; on a itself it is local.
        assert_eq!(p.workspace(2), Outcome::Remote(2));
        assert_eq!(s.workspace(2), Some(Route::On("a".into())));
        assert_eq!(a.workspace(2), Outcome::Done);
        assert_eq!(a.move_to_workspace(3), Outcome::Remote(3));
        assert!(s.move_to_workspace("a", 3).is_err());
        // Drop: a keeps everything, the primary forgets a's numbers and may make one of them.
        assert!(s.drop_satellite("a"));
        a.clear_foreign();
        p.set_foreign(s.foreign("p"));
        assert!(p.foreign().is_empty());
        assert_eq!(p.workspace(2), Outcome::Done);
        p.open_stacked(2);
        let (screens, numbers) = state(&p);
        s.report("p", screens, numbers).expect("valid");
        // Rejoin: 4 kept, 2 taken meanwhile, so it goes to the lowest free one, 5; the windows go with it.
        let (screens, numbers) = state(&a);
        let renames = s.join("a", screens, numbers).expect("joins");
        assert_eq!(renames, [(2, 5)]);
        for (from, to) in renames {
            assert!(a.renumber_workspace(from, to));
        }
        assert_eq!(a.where_is(10).map(|w| w.0), Some(5));
        assert_eq!(p.where_is(2).map(|w| w.0), Some(2));
    }
}
