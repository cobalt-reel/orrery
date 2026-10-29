//! The event stream (`orrery msg subscribe`): a `state` snapshot, then events for what changed. Pure: the
//! compositor takes a `Summary` of its state once per event-loop iteration and sends the events that lead from the
//! last one sent to it, so every change is covered, however it was made, and changes within one iteration coalesce.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ipc::{ColorsView, LayerView, SessionView, StateView, TabView, WindowView};
use crate::layout::Rect;
use crate::stacks::WinId;

/// One line of the stream. Each carries all a bar needs to redraw its part without asking again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    /// The whole state as `state` answers it, but never `last_key`: the first line of every subscription.
    State(Box<StateView>),
    /// What each screen shows, and every existing workspace.
    Workspace {
        screens: Vec<ScreenView>,
        workspaces: Vec<WorkspaceMark>,
    },
    /// The focused slot and the window it shows.
    Focus(Focus),
    /// The focused slot's tabs (another slot focused, or its tabs or their titles changed).
    Tabs(Tabs),
    /// A window's title or app id changed (one event per window).
    Title(WindowView),
    /// The windows asking for attention, and the workspaces holding them.
    Attention { windows: Vec<WinId>, workspaces: Vec<u8> },
    /// Each screen's layout and its slots' rectangles (a layout switched, a screen resized, a bar's zone).
    Layout { screens: Vec<ScreenLayout> },
    /// Each screen's layer surfaces.
    Layers { screens: Vec<ScreenLayers> },
    /// The session (machines, links, where input is); null alone.
    Session { session: Option<Box<SessionView>> },
    /// The config was read again: why it was not used (null when it was), and the colours in force.
    Config { error: Option<String>, colors: ColorsView },
}

/// A screen: its output's name, its number, the workspace it shows, and whether it has the focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenView {
    pub name: String,
    pub number: u8,
    pub workspace: u8,
    pub focused: bool,
}

/// An existing workspace: the screen it lives on, whether that screen shows it, whether it holds windows, and
/// whether one of them asked for attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMark {
    pub number: u8,
    pub screen: String,
    pub shown: bool,
    pub occupied: bool,
    pub attention: bool,
}

/// Where the focus is: screen, workspace, slot (index and name) and the window the slot shows; all null without a
/// screen, `window` null for an empty slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Focus {
    pub screen: Option<String>,
    pub workspace: Option<u8>,
    pub slot: Option<usize>,
    pub slot_name: Option<String>,
    pub window: Option<WindowView>,
}

/// The focused slot (as in `Focus`) and its tabs, left to right, as `state` lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tabs {
    pub screen: Option<String>,
    pub workspace: Option<u8>,
    pub slot: Option<usize>,
    pub tabs: Vec<TabView>,
}

/// A screen's shown workspace, its layout and its slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLayout {
    pub name: String,
    pub workspace: u8,
    pub layout: String,
    pub slots: Vec<SlotRect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRect {
    pub index: usize,
    pub name: Option<String>,
    pub rect: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLayers {
    pub name: String,
    pub layers: Vec<LayerView>,
}

/// What events are derived from: the parts of the state the events carry, compared part by part.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    workspace: Event,
    focus: Focus,
    tabs: Tabs,
    /// Every window in a slot, by id.
    titles: BTreeMap<WinId, WindowView>,
    attention: Event,
    layout: Event,
    layers: Event,
    session: Option<SessionView>,
    /// The number of config reads so far, and the event telling about the last.
    config: (u64, Event),
}

impl Summary {
    /// The summary of `view` (as `state` gives it, `frames`, `launches` and `last_key` not needed); `marked` says
    /// whether a window asked for attention, `reads` counts the config reads (a reload, good or not).
    pub fn new(view: &StateView, marked: impl Fn(WinId) -> bool, reads: u64) -> Self {
        let monitor = view.monitors.iter().find(|m| m.focused);
        let slot = monitor.and_then(|m| m.slots.iter().find(|s| s.focused));
        let (screen, workspace) = (monitor.map(|m| m.name.clone()), monitor.map(|m| m.workspace));
        let focus = Focus {
            screen: screen.clone(),
            workspace,
            slot: slot.map(|s| s.index),
            slot_name: slot.and_then(|s| s.name.clone()),
            window: slot
                .and_then(|s| s.tabs.iter().find(|t| t.current))
                .and_then(|t| t.windows.last().cloned()),
        };
        let tabs = Tabs {
            screen,
            workspace,
            slot: slot.map(|s| s.index),
            tabs: slot.map(|s| s.tabs.clone()).unwrap_or_default(),
        };
        let marks: Vec<WorkspaceMark> = (view.workspaces.iter())
            .map(|w| {
                let tabs = || w.slots.iter().flat_map(|s| &s.tabs);
                WorkspaceMark {
                    number: w.number,
                    screen: w.monitor.clone(),
                    shown: w.shown,
                    occupied: tabs().next().is_some(),
                    attention: tabs().any(|t| t.attention),
                }
            })
            .collect();
        let titles: BTreeMap<WinId, WindowView> = (view.workspaces.iter())
            .flat_map(|w| &w.slots)
            .flat_map(|s| &s.tabs)
            .flat_map(|t| &t.windows)
            .map(|w| (w.id, w.clone()))
            .collect();
        let attention = Event::Attention {
            windows: titles.keys().copied().filter(|&w| marked(w)).collect(),
            workspaces: marks.iter().filter(|m| m.attention).map(|m| m.number).collect(),
        };
        let workspace = Event::Workspace {
            screens: (view.monitors.iter())
                .map(|m| ScreenView {
                    name: m.name.clone(),
                    number: m.number,
                    workspace: m.workspace,
                    focused: m.focused,
                })
                .collect(),
            workspaces: marks,
        };
        let layout = Event::Layout {
            screens: (view.monitors.iter())
                .map(|m| ScreenLayout {
                    name: m.name.clone(),
                    workspace: m.workspace,
                    layout: m.layout.clone(),
                    slots: (m.slots.iter())
                        .map(|s| SlotRect {
                            index: s.index,
                            name: s.name.clone(),
                            rect: s.rect,
                        })
                        .collect(),
                })
                .collect(),
        };
        let layers = Event::Layers {
            screens: (view.monitors.iter())
                .map(|m| ScreenLayers {
                    name: m.name.clone(),
                    layers: m.layers.clone(),
                })
                .collect(),
        };
        let config = Event::Config {
            error: view.config_error.clone(),
            colors: view.colors.clone(),
        };
        Self {
            workspace,
            focus,
            tabs,
            titles,
            attention,
            layout,
            layers,
            session: view.session.as_deref().cloned(),
            config: (reads, config),
        }
    }

    /// The events leading from `self` (sent last) to `now`, in this order: workspace, layout, focus, tabs, titles
    /// (by window id), attention, layers, session, config.
    #[must_use]
    pub fn events(&self, now: &Summary) -> Vec<Event> {
        let changed = |was: &Event, is: &Event| (was != is).then(|| is.clone());
        // A new title of the focused window is a `title` event, not a move of the focus.
        let at = |f: &Focus| {
            (
                f.screen.clone(),
                f.workspace,
                f.slot,
                f.slot_name.clone(),
                f.window.as_ref().map(|w| w.id),
            )
        };
        let moved = at(&self.focus) != at(&now.focus);
        let titles = (now.titles.iter())
            .filter(|(w, is)| self.titles.get(w).is_some_and(|was| was != *is))
            .map(|(_, is)| Event::Title(is.clone()));
        let session = self.session.as_ref().map(steady) != now.session.as_ref().map(steady);
        let mut events: Vec<Event> = [
            changed(&self.workspace, &now.workspace),
            changed(&self.layout, &now.layout),
            moved.then(|| Event::Focus(now.focus.clone())),
            (self.tabs != now.tabs).then(|| Event::Tabs(now.tabs.clone())),
        ]
        .into_iter()
        .flatten()
        .chain(titles)
        .collect();
        events.extend(
            [
                changed(&self.attention, &now.attention),
                changed(&self.layers, &now.layers),
                session.then(|| Event::Session {
                    session: now.session.clone().map(Box::new),
                }),
                (self.config != now.config).then(|| now.config.1.clone()),
            ]
            .into_iter()
            .flatten(),
        );
        events
    }
}

/// A session without what moves all the time (link latencies, queued input): what a change of is news.
fn steady(session: &SessionView) -> SessionView {
    let mut s = session.clone();
    for m in &mut s.machines {
        m.latency_ms = None;
        m.queued = 0;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Colors, default_config};
    use crate::ipc::{LayerName, MachineView};
    use crate::layout::Dir;
    use crate::session::Role;
    use crate::stacks::Stacks;
    use std::collections::{BTreeSet, HashMap};

    const UHD: Rect = Rect {
        x: 0,
        y: 0,
        w: 3840,
        h: 2160,
    };

    /// A compositor's worth of state for tests: stacks, titles by window, marks come from the stacks.
    struct World {
        stacks: Stacks,
        titles: HashMap<WinId, String>,
        reads: u64,
        error: Option<String>,
        colors: Colors,
        layers: Vec<LayerView>,
        session: Option<SessionView>,
    }

    impl World {
        fn new() -> Self {
            let layout = |name: &str| default_config().layouts.into_iter().find(|l| l.name == name).unwrap();
            let mut stacks = Stacks::new();
            stacks.add_monitor("one", UHD, layout("wall"));
            stacks.add_monitor("two", UHD, layout("halves"));
            stacks.focus_monitor("one");
            Self {
                stacks,
                titles: HashMap::new(),
                reads: 0,
                error: None,
                colors: Colors::default(),
                layers: Vec::new(),
                session: None,
            }
        }

        fn open(&mut self, id: WinId, title: &str) {
            self.titles.insert(id, title.into());
            self.stacks.open_tab(id, None, None);
        }

        fn view(&self) -> StateView {
            let mut v = StateView::new(&self.stacks, true, self.error.clone(), &self.colors, |w| {
                (Some(format!("app{w}")), self.titles.get(&w).cloned())
            });
            v.monitors[0].layers = self.layers.clone();
            v.session = self.session.clone().map(Box::new);
            v
        }

        fn summary(&self) -> Summary {
            Summary::new(&self.view(), |w| self.stacks.is_marked(w), self.reads)
        }
    }

    /// The events `change` makes, by name.
    fn after(world: &mut World, change: impl FnOnce(&mut World)) -> Vec<Event> {
        let before = world.summary();
        change(world);
        before.events(&world.summary())
    }

    fn names(events: &[Event]) -> Vec<String> {
        (events.iter())
            .map(|e| serde_json::to_value(e).unwrap()["event"].as_str().unwrap().to_owned())
            .collect()
    }

    fn window(id: WinId, title: &str) -> WindowView {
        WindowView {
            id,
            app_id: Some(format!("app{id}")),
            title: Some(title.into()),
        }
    }

    #[test]
    fn nothing_changed_is_no_event() {
        let mut w = World::new();
        w.open(1, "one");
        assert_eq!(after(&mut w, |_| {}), []);
    }

    #[test]
    fn a_new_window_changes_the_workspace_focus_and_tabs() {
        let mut w = World::new();
        let events = after(&mut w, |w| w.open(1, "one"));
        assert_eq!(names(&events), ["workspace", "focus", "tabs"]);
        let Event::Workspace { screens, workspaces } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(
            screens,
            &[
                ScreenView {
                    name: "one".into(),
                    number: 1,
                    workspace: 1,
                    focused: true
                },
                ScreenView {
                    name: "two".into(),
                    number: 2,
                    workspace: 2,
                    focused: false
                }
            ]
        );
        assert_eq!(
            workspaces[0],
            WorkspaceMark {
                number: 1,
                screen: "one".into(),
                shown: true,
                occupied: true,
                attention: false
            }
        );
        assert!(!workspaces[1].occupied);
        assert_eq!(
            events[1],
            Event::Focus(Focus {
                screen: Some("one".into()),
                workspace: Some(1),
                slot: Some(0),
                slot_name: None,
                window: Some(window(1, "one")),
            })
        );
        let Event::Tabs(tabs) = &events[2] else {
            panic!("{events:?}");
        };
        assert_eq!(
            (tabs.screen.as_deref(), tabs.workspace, tabs.slot),
            (Some("one"), Some(1), Some(0))
        );
        assert_eq!(tabs.tabs.len(), 1);
        assert_eq!(tabs.tabs[0].windows, [window(1, "one")]);
    }

    #[test]
    fn a_new_tab_changes_focus_and_tabs_only() {
        let mut w = World::new();
        w.open(1, "one");
        let events = after(&mut w, |w| w.open(2, "two"));
        assert_eq!(names(&events), ["focus", "tabs"]);
        let Event::Tabs(tabs) = &events[1] else {
            panic!("{events:?}");
        };
        let current: Vec<bool> = tabs.tabs.iter().map(|t| t.current).collect();
        assert_eq!(current, [false, true]);
    }

    #[test]
    fn a_title_change_is_a_title_event_and_the_tabs_showing_it() {
        let mut w = World::new();
        w.open(1, "one");
        w.open(2, "two");
        let events = after(&mut w, |w| {
            w.titles.insert(1, "uno".into());
        });
        assert_eq!(names(&events), ["tabs", "title"]);
        assert_eq!(events[1], Event::Title(window(1, "uno")));
        // In another slot than the focused one: the title alone.
        w.stacks.focus_slot(Dir::Right);
        let events = after(&mut w, |w| {
            w.titles.insert(2, "dos".into());
        });
        assert_eq!(events, [Event::Title(window(2, "dos"))]);
        // The focused window's own: the focus stays where it was, so no focus event.
        let events = after(&mut w, |w| w.stacks.focus_slot(Dir::Left));
        assert_eq!(names(&events), ["focus", "tabs"]);
        let events = after(&mut w, |w| {
            w.titles.insert(2, "zwei".into());
        });
        assert_eq!(names(&events), ["tabs", "title"]);
    }

    #[test]
    fn a_workspace_switch_changes_the_workspace_layout_focus_and_tabs() {
        let mut w = World::new();
        w.open(1, "one");
        let events = after(&mut w, |w| {
            w.stacks.workspace(3);
        });
        assert_eq!(names(&events), ["workspace", "layout", "focus", "tabs"]);
        let Event::Workspace { screens, workspaces } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(screens[0].workspace, 3);
        let listed: Vec<(u8, bool, bool)> = workspaces.iter().map(|m| (m.number, m.shown, m.occupied)).collect();
        assert_eq!(listed, [(1, false, true), (2, true, false), (3, true, false)]);
        let Event::Layout { screens } = &events[1] else {
            panic!("{events:?}");
        };
        assert_eq!((screens[0].workspace, screens[0].layout.as_str()), (3, "wall"));
        assert_eq!(screens[0].slots.len(), 6);
        assert_eq!(screens[0].slots[3].name.as_deref(), Some("hero"));
        assert_eq!(
            events[2],
            Event::Focus(Focus {
                screen: Some("one".into()),
                workspace: Some(3),
                slot: Some(0),
                slot_name: None,
                window: None,
            })
        );
    }

    #[test]
    fn attention_marks_the_window_its_tab_and_its_workspace() {
        let mut w = World::new();
        w.open(1, "one");
        w.stacks.workspace(3);
        let events = after(&mut w, |w| w.stacks.mark(1));
        assert_eq!(names(&events), ["workspace", "attention"]);
        assert_eq!(
            events[1],
            Event::Attention {
                windows: vec![1],
                workspaces: vec![1]
            }
        );
        let Event::Workspace { workspaces, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert!(workspaces[0].attention);
        // Seen: the mark goes.
        let events = after(&mut w, |w| {
            w.stacks.workspace(1);
        });
        assert!(events.contains(&Event::Attention {
            windows: vec![],
            workspaces: vec![]
        }));
    }

    #[test]
    fn layers_session_and_config_have_their_events() {
        let mut w = World::new();
        let bar = LayerView {
            namespace: "bar".into(),
            layer: LayerName::Top,
            rect: Rect {
                x: 0,
                y: 0,
                w: 3840,
                h: 30,
            },
            keyboard: false,
        };
        let events = after(&mut w, |w| w.layers = vec![bar.clone()]);
        assert_eq!(names(&events), ["layers"]);
        let Event::Layers { screens } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!((screens[0].name.as_str(), &screens[0].layers), ("one", &vec![bar]));
        assert!(screens[1].layers.is_empty());

        let machine = MachineView {
            name: "desk".into(),
            role: Role::Primary,
            monitors: vec![],
            workspaces: vec![1],
            latency_ms: None,
            connected: true,
            focused: true,
            queued: 0,
        };
        let session = SessionView {
            machine: "desk".into(),
            role: Role::Primary,
            listen: None,
            primary: None,
            machines: vec![machine],
            input: "desk".into(),
            foreign: vec![],
            error: None,
        };
        let events = after(&mut w, |w| w.session = Some(session.clone()));
        assert_eq!(
            events,
            [Event::Session {
                session: Some(Box::new(session))
            }]
        );
        // Pings and queues move all the time: not news by themselves, though the next event carries them.
        let events = after(&mut w, |w| {
            let m = &mut w.session.as_mut().unwrap().machines[0];
            m.latency_ms = Some(3);
            m.queued = 2;
        });
        assert_eq!(events, []);
        let events = after(&mut w, |w| w.session.as_mut().unwrap().input = "side".into());
        let Event::Session { session: Some(s) } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!((s.input.as_str(), s.machines[0].latency_ms), ("side", Some(3)));
        assert_eq!(after(&mut w, |w| w.session = None), [Event::Session { session: None }]);

        // Every read of the config is an event, the same result again too.
        let events = after(&mut w, |w| w.reads += 1);
        assert_eq!(
            events,
            [Event::Config {
                error: None,
                colors: (&Colors::default()).into()
            }]
        );
        let events = after(&mut w, |w| {
            w.reads += 1;
            w.error = Some("c.kdl:2:1: no".into());
        });
        assert_eq!(names(&events), ["config"]);
        assert!(matches!(&events[0], Event::Config { error: Some(e), .. } if e.contains("no")));
    }

    #[test]
    fn events_have_the_documented_shape() {
        let json = |e: &Event| serde_json::to_string(e).unwrap();
        assert_eq!(
            json(&Event::Focus(Focus {
                screen: Some("one".into()),
                workspace: Some(3),
                slot: Some(0),
                slot_name: None,
                window: Some(window(1, "one")),
            })),
            concat!(
                r#"{"event":"focus","screen":"one","workspace":3,"slot":0,"slot_name":null,"#,
                r#""window":{"id":1,"app_id":"app1","title":"one"}}"#
            )
        );
        assert_eq!(
            json(&Event::Title(window(4, "x"))),
            r#"{"event":"title","id":4,"app_id":"app4","title":"x"}"#
        );
        assert_eq!(
            json(&Event::Attention {
                windows: vec![4],
                workspaces: vec![2]
            }),
            r#"{"event":"attention","windows":[4],"workspaces":[2]}"#
        );
        assert_eq!(
            json(&Event::Session { session: None }),
            r#"{"event":"session","session":null}"#
        );
        let mut w = World::new();
        w.open(1, "one");
        w.stacks.focus_slot(Dir::Right);
        let state = Event::State(Box::new(w.view()));
        let text = json(&state);
        assert!(
            text.starts_with(r#"{"event":"state","monitors":[{"name":"one","#),
            "{text}"
        );
        assert!(!text.contains("last_key"), "{text}");
        // And back: a client may read them with the same types.
        for e in [state, Event::Title(window(4, "x"))] {
            assert_eq!(serde_json::from_str::<Event>(&json(&e)).unwrap(), e);
        }
        let all: BTreeSet<String> = names(&after(&mut World::new(), |w| w.open(1, "one")))
            .into_iter()
            .collect();
        assert!(all.contains("workspace"));
    }
}
