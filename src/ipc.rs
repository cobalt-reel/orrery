//! The control protocol spoken over the socket by `orrery msg`: one JSON `Request` line in, one `Response` out.

use serde::{Deserialize, Serialize};

use crate::config::{Config, Cycle, parse_dir};
use crate::layout::{Dir, Rect};
use crate::stacks::{Stacks, WinId};

/// Read through `Wire`: every variant needs a twin there (a test fails to compile until it has one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case", try_from = "Wire")]
pub enum Request {
    State,
    FocusSlot {
        dir: Dir,
    },
    /// The focused slot's current tab, its whole stack, to the neighbour.
    Move {
        dir: Dir,
    },
    SwapSlot {
        dir: Dir,
    },
    CycleTab {
        cycle: Cycle,
    },
    /// `monitor` by output name; the focused monitor when absent.
    Layout {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        monitor: Option<String>,
    },
    Close,
    Reload,
    Quit,
    /// The focused stack's top window becomes a tab of its own.
    Split,
    ToggleStacking,
    /// Launches a program as a `spawn` bind does; never empty, no empty program.
    Spawn {
        argv: Vec<String>,
    },
}

/// How a `Request` is read: serde ignores unknown fields on internally tagged unit variants, so here they are
/// empty struct variants, which refuse them like every other request.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case", deny_unknown_fields)]
enum Wire {
    State {},
    FocusSlot { dir: Dir },
    Move { dir: Dir },
    SwapSlot { dir: Dir },
    CycleTab { cycle: Cycle },
    // A missing Option field reads as None without `#[serde(default)]`.
    Layout { name: String, monitor: Option<String> },
    Close {},
    Reload {},
    Quit {},
    Split {},
    ToggleStacking {},
    Spawn { argv: Vec<String> },
}

impl TryFrom<Wire> for Request {
    type Error = String;

    fn try_from(w: Wire) -> Result<Self, String> {
        Ok(match w {
            Wire::State {} => Request::State,
            Wire::FocusSlot { dir } => Request::FocusSlot { dir },
            Wire::Move { dir } => Request::Move { dir },
            Wire::SwapSlot { dir } => Request::SwapSlot { dir },
            Wire::CycleTab { cycle } => Request::CycleTab { cycle },
            Wire::Layout { name, monitor } => Request::Layout { name, monitor },
            Wire::Close {} => Request::Close,
            Wire::Reload {} => Request::Reload,
            Wire::Quit {} => Request::Quit,
            Wire::Split {} => Request::Split,
            Wire::ToggleStacking {} => Request::ToggleStacking,
            Wire::Spawn { argv } => Request::Spawn {
                argv: spawn_argv(argv)?,
            },
        })
    }
}

/// A spawn's argv as a request carries it: a program, which is not empty, then its arguments.
fn spawn_argv(argv: Vec<String>) -> Result<Vec<String>, String> {
    match argv.first() {
        Some(program) if !program.is_empty() => Ok(argv),
        _ => Err("`spawn` takes a program and its arguments".into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateView {
    pub monitors: Vec<MonitorView>,
    /// Where a window with nowhere else to go opens: on top of the focused slot's current stack (true), or as a
    /// new tab there (false). `toggle-stacking` flips it.
    pub stacking: bool,
    pub config_error: Option<String>,
    /// How long drawing a frame takes (the renderer's backends only; absent before the first frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<FramesView>,
    /// The last key press the compositor saw, as its bind lookup read it (absent before the first): what to look
    /// at when a bind does nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_key: Option<KeyView>,
    /// Recent spawns and whether a window has taken each (see `placement`), oldest first: a debug aid.
    #[serde(default)]
    pub launches: Vec<LaunchView>,
}

/// One recent spawn: the program's pid, the xdg-activation token it was handed, its age, and whether a window has
/// taken it (opened for it, or moved to it by an activation with its token). Forgotten lazily, after 5 s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchView {
    pub pid: u32,
    pub token: Option<String>,
    pub age_ms: u64,
    pub claimed: bool,
}

/// A key press as the bind lookup read it: the key (its keysym's name; `(plain key)` for typing, which is never
/// kept), the modifiers held (`Ctrl+Alt+Shift+Super`, empty for none) and the action of the bind it matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyView {
    pub key: String,
    pub mods: String,
    pub bind: Option<String>,
}

/// Frame times on the compositor thread (`render_output`), in microseconds, over the
/// latest frames (about a minute); `count` is every frame drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramesView {
    pub count: u64,
    pub min_us: u64,
    pub mean_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorView {
    pub name: String,
    pub layout: String,
    pub focused: bool,
    pub slots: Vec<SlotView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotView {
    pub index: usize,
    pub name: Option<String>,
    pub rect: Rect,
    pub focused: bool,
    /// Left to right.
    pub tabs: Vec<TabView>,
}

/// One tab: a stack of windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabView {
    /// The tab the slot shows.
    pub current: bool,
    /// A window of the stack asked for attention.
    pub attention: bool,
    /// Bottom to top, like the stack.
    pub windows: Vec<WindowView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowView {
    pub id: u64,
    pub app_id: Option<String>,
    pub title: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            state: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            state: None,
        }
    }

    pub fn with_state(state: StateView) -> Self {
        Self {
            state: Some(state),
            ..Self::ok()
        }
    }
}

impl StateView {
    /// The state as `orrery msg state` shows it; `info` gives a window's (app_id, title). Pending windows have
    /// no slot and are not listed.
    pub fn new(
        stacks: &Stacks,
        stacking: bool,
        config_error: Option<String>,
        info: impl Fn(WinId) -> (Option<String>, Option<String>),
    ) -> Self {
        let monitors = stacks
            .monitors()
            .iter()
            .enumerate()
            .map(|(mi, mon)| {
                let focused = mi == stacks.focused_monitor();
                let slots = mon
                    .layout()
                    .slots
                    .iter()
                    .zip(mon.layout().rects(mon.area()))
                    .zip(mon.slots())
                    .enumerate()
                    .map(|(index, ((spec, rect), slot))| SlotView {
                        index,
                        name: spec.name.clone(),
                        rect,
                        focused: focused && index == mon.focus(),
                        tabs: slot
                            .tabs()
                            .iter()
                            .enumerate()
                            .map(|(t, stack)| TabView {
                                current: t == slot.current(),
                                attention: stack.iter().any(|&w| stacks.is_marked(w)),
                                windows: stack
                                    .iter()
                                    .map(|&id| {
                                        let (app_id, title) = info(id);
                                        WindowView { id, app_id, title }
                                    })
                                    .collect(),
                            })
                            .collect(),
                    })
                    .collect();
                MonitorView {
                    name: mon.name().to_owned(),
                    layout: mon.layout().name.clone(),
                    focused,
                    slots,
                }
            })
            .collect();
        StateView {
            monitors,
            stacking,
            config_error,
            frames: None,
            last_key: None,
            launches: Vec::new(),
        }
    }
}

impl Request {
    /// Runs a pure request against the stacks, layout names resolved in `config`. On error nothing changed.
    /// `state`, `reload`, `quit`, `toggle-stacking` and `spawn` are the compositor's to answer, and so is
    /// `close`: closing asks the client to go, and its window leaves the stacks only when it is destroyed.
    pub fn apply(&self, stacks: &mut Stacks, config: &Config) -> Result<(), String> {
        match self {
            Request::FocusSlot { dir } => stacks.focus_slot(*dir),
            Request::Move { dir } => stacks.move_stack(*dir),
            Request::Split => stacks.split(),
            Request::SwapSlot { dir } => stacks.swap_slot(*dir),
            Request::CycleTab { cycle } => stacks.cycle_tab(*cycle),
            Request::Layout { name, monitor } => {
                let layout = config
                    .layouts
                    .iter()
                    .find(|l| &l.name == name)
                    .ok_or_else(|| format!("unknown layout {name:?}"))?;
                let target = match monitor {
                    Some(m) if stacks.monitors().iter().any(|x| x.name() == m) => m.clone(),
                    Some(m) => return Err(format!("unknown monitor {m:?}")),
                    None => stacks
                        .monitors()
                        .get(stacks.focused_monitor())
                        .map(|x| x.name().to_owned())
                        .ok_or("no monitor to lay out")?,
                };
                stacks.set_layout(&target, layout.clone());
            }
            Request::State
            | Request::Close
            | Request::Reload
            | Request::Quit
            | Request::ToggleStacking
            | Request::Spawn { .. } => {
                return Err(format!("{} is answered by the compositor", self.verb()));
            }
        }
        Ok(())
    }

    fn verb(&self) -> &'static str {
        match self {
            Request::State => "state",
            Request::FocusSlot { .. } => "focus-slot",
            Request::Move { .. } => "move",
            Request::SwapSlot { .. } => "swap-slot",
            Request::CycleTab { .. } => "cycle-tab",
            Request::Layout { .. } => "layout",
            Request::Close => "close",
            Request::Reload => "reload",
            Request::Quit => "quit",
            Request::Split => "split",
            Request::ToggleStacking => "toggle-stacking",
            Request::Spawn { .. } => "spawn",
        }
    }
}

/// `orrery msg` arguments: words (`focus-slot left`, `layout wall [MONITOR]`, `spawn PROGRAM [ARGS...]`) or a
/// single JSON request.
pub fn parse_words(words: &[String]) -> Result<Request, String> {
    if let [one] = words
        && one.trim_start().starts_with('{')
    {
        return serde_json::from_str(one).map_err(|e| format!("bad JSON request: {e}"));
    }
    let Some((verb, args)) = words.split_first() else {
        return Err("empty request: give a verb like `state` or `focus-slot left`".into());
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let dir = |args: &[&str]| match args {
        [d] => parse_dir(d).ok_or_else(|| format!("`{verb}` takes a direction (left, right, up, down), not {d:?}")),
        _ => Err(format!("`{verb}` takes one direction (left, right, up, down)")),
    };
    let none = |r: Request| {
        if args.is_empty() {
            Ok(r)
        } else {
            Err(format!("`{verb}` takes no arguments"))
        }
    };
    match verb.as_str() {
        "state" => none(Request::State),
        "close" => none(Request::Close),
        "reload" => none(Request::Reload),
        "quit" => none(Request::Quit),
        "split" => none(Request::Split),
        "toggle-stacking" => none(Request::ToggleStacking),
        "spawn" => Ok(Request::Spawn {
            argv: spawn_argv(args.iter().map(|a| (*a).to_owned()).collect())?,
        }),
        "focus-slot" => Ok(Request::FocusSlot { dir: dir(&args)? }),
        "move" => Ok(Request::Move { dir: dir(&args)? }),
        "swap-slot" => Ok(Request::SwapSlot { dir: dir(&args)? }),
        "cycle-tab" => match args.as_slice() {
            ["next"] => Ok(Request::CycleTab { cycle: Cycle::Next }),
            ["prev"] => Ok(Request::CycleTab { cycle: Cycle::Prev }),
            _ => Err("`cycle-tab` takes next or prev".into()),
        },
        "layout" => match args.as_slice() {
            [name] => Ok(Request::Layout {
                name: (*name).to_owned(),
                monitor: None,
            }),
            [name, monitor] => Ok(Request::Layout {
                name: (*name).to_owned(),
                monitor: Some((*monitor).to_owned()),
            }),
            _ => Err("`layout` takes a layout name and optionally a monitor".into()),
        },
        other => Err(format!("unknown request `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    fn layout(name: &str) -> crate::layout::Layout {
        default_config()
            .layouts
            .into_iter()
            .find(|l| l.name == name)
            .expect("layout in the default config")
    }

    /// Exhaustive over `Request`, so a new variant fails to compile here until it has a `Wire` twin; its number
    /// must then appear in `all_requests` (checked against `VARIANTS`).
    fn twin(r: &Request) -> (usize, Wire) {
        match r.clone() {
            Request::State => (0, Wire::State {}),
            Request::FocusSlot { dir } => (1, Wire::FocusSlot { dir }),
            Request::Move { dir } => (2, Wire::Move { dir }),
            Request::SwapSlot { dir } => (3, Wire::SwapSlot { dir }),
            Request::CycleTab { cycle } => (4, Wire::CycleTab { cycle }),
            Request::Layout { name, monitor } => (5, Wire::Layout { name, monitor }),
            Request::Close => (6, Wire::Close {}),
            Request::Reload => (7, Wire::Reload {}),
            Request::Quit => (8, Wire::Quit {}),
            Request::Split => (9, Wire::Split {}),
            Request::ToggleStacking => (10, Wire::ToggleStacking {}),
            Request::Spawn { argv } => (11, Wire::Spawn { argv }),
        }
    }

    const VARIANTS: usize = 12;

    #[test]
    fn every_request_has_a_wire_twin_and_a_sample() {
        let covered: std::collections::BTreeSet<usize> = all_requests()
            .iter()
            .map(|r| {
                let (i, w) = twin(r);
                assert_eq!(Request::try_from(w).as_ref(), Ok(r));
                i
            })
            .collect();
        assert_eq!(covered, (0..VARIANTS).collect(), "all_requests misses a variant");
    }

    fn all_requests() -> Vec<Request> {
        vec![
            Request::State,
            Request::FocusSlot { dir: Dir::Left },
            Request::Move { dir: Dir::Right },
            Request::SwapSlot { dir: Dir::Up },
            Request::CycleTab { cycle: Cycle::Next },
            Request::CycleTab { cycle: Cycle::Prev },
            Request::Layout {
                name: "halves".into(),
                monitor: None,
            },
            Request::Layout {
                name: "wall".into(),
                monitor: Some("winit".into()),
            },
            Request::Close,
            Request::Reload,
            Request::Quit,
            Request::Split,
            Request::ToggleStacking,
            Request::Spawn {
                argv: vec!["kitty".into(), "--single-instance".into()],
            },
        ]
    }

    #[test]
    fn every_request_round_trips_through_json() {
        for r in all_requests() {
            let json = serde_json::to_string(&r).expect("serializes");
            assert_eq!(serde_json::from_str::<Request>(&json).expect(&json), r, "{json}");
        }
    }

    #[test]
    fn requests_have_the_documented_shape() {
        let json = |r: &Request| serde_json::to_string(r).expect("serializes");
        assert_eq!(json(&Request::State), r#"{"cmd":"state"}"#);
        assert_eq!(
            json(&Request::FocusSlot { dir: Dir::Down }),
            r#"{"cmd":"focus-slot","dir":"down"}"#
        );
        assert_eq!(
            json(&Request::CycleTab { cycle: Cycle::Prev }),
            r#"{"cmd":"cycle-tab","cycle":"prev"}"#
        );
        assert_eq!(
            json(&Request::Layout {
                name: "wall".into(),
                monitor: None
            }),
            r#"{"cmd":"layout","name":"wall"}"#
        );
        assert_eq!(
            json(&Request::Move { dir: Dir::Left }),
            r#"{"cmd":"move","dir":"left"}"#
        );
        assert_eq!(json(&Request::Split), r#"{"cmd":"split"}"#);
        assert_eq!(json(&Request::ToggleStacking), r#"{"cmd":"toggle-stacking"}"#);
        assert_eq!(
            json(&Request::Spawn {
                argv: vec!["foot".into(), "-e".into()]
            }),
            r#"{"cmd":"spawn","argv":["foot","-e"]}"#
        );
    }

    #[test]
    fn malformed_requests_are_refused() {
        for bad in [
            r#"{"cmd":"explode"}"#,
            r#"{"cmd":"focus-slot"}"#,
            r#"{"cmd":"focus-slot","dir":"sideways"}"#,
            r#"{"cmd":"close","force":true}"#,
            r#"{"cmd":"focus-slot","dir":"left","x":1}"#,
            r#"{"cmd":"layout"}"#,
            r#"{"cmd":"move-tab","dir":"left"}"#,
            r#"{"cmd":"split","dir":"left"}"#,
            r#"{"cmd":"spawn"}"#,
            r#"{"cmd":"spawn","argv":[]}"#,
            r#"{"cmd":"spawn","argv":[""]}"#,
            r#"{"dir":"left"}"#,
            r#"[]"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad} accepted");
        }
    }

    fn state_view() -> StateView {
        StateView {
            monitors: vec![MonitorView {
                name: "winit".into(),
                layout: "wall".into(),
                focused: true,
                slots: vec![SlotView {
                    index: 3,
                    name: Some("hero".into()),
                    rect: Rect {
                        x: 1280,
                        y: 0,
                        w: 2560,
                        h: 1440,
                    },
                    focused: true,
                    tabs: vec![TabView {
                        current: true,
                        attention: false,
                        windows: vec![WindowView {
                            id: 7,
                            app_id: Some("kitty".into()),
                            title: None,
                        }],
                    }],
                }],
            }],
            stacking: true,
            config_error: None,
            frames: Some(FramesView {
                count: 10,
                min_us: 100,
                mean_us: 200,
                p95_us: 300,
                max_us: 400,
            }),
            last_key: Some(KeyView {
                key: "h".into(),
                mods: "Super".into(),
                bind: Some("focus-slot left".into()),
            }),
            launches: vec![LaunchView {
                pid: 4242,
                token: Some("tok".into()),
                age_ms: 120,
                claimed: false,
            }],
        }
    }

    #[test]
    fn responses_leave_out_what_they_lack() {
        let json = |r: &Response| serde_json::to_string(r).expect("serializes");
        assert_eq!(json(&Response::ok()), r#"{"ok":true}"#);
        assert_eq!(json(&Response::error("no")), r#"{"ok":false,"error":"no"}"#);
        let r = Response::with_state(state_view());
        assert!(r.ok);
        let back: Response = serde_json::from_str(&json(&r)).expect("deserializes");
        assert_eq!(back, r);
        assert!(
            json(&r).contains(r#""launches":[{"pid":4242,"token":"tok","age_ms":120,"claimed":false}]"#),
            "{}",
            json(&r)
        );
    }

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn words_parse_for_every_verb() {
        let cases = [
            ("state", Request::State),
            ("focus-slot left", Request::FocusSlot { dir: Dir::Left }),
            ("move right", Request::Move { dir: Dir::Right }),
            ("split", Request::Split),
            ("toggle-stacking", Request::ToggleStacking),
            (
                "spawn kitty --single-instance",
                Request::Spawn {
                    argv: vec!["kitty".into(), "--single-instance".into()],
                },
            ),
            ("swap-slot up", Request::SwapSlot { dir: Dir::Up }),
            ("focus-slot down", Request::FocusSlot { dir: Dir::Down }),
            ("cycle-tab next", Request::CycleTab { cycle: Cycle::Next }),
            ("cycle-tab prev", Request::CycleTab { cycle: Cycle::Prev }),
            (
                "layout halves",
                Request::Layout {
                    name: "halves".into(),
                    monitor: None,
                },
            ),
            (
                "layout wall winit",
                Request::Layout {
                    name: "wall".into(),
                    monitor: Some("winit".into()),
                },
            ),
            ("close", Request::Close),
            ("reload", Request::Reload),
            ("quit", Request::Quit),
        ];
        for (text, want) in cases {
            assert_eq!(parse_words(&words(text)), Ok(want), "{text}");
        }
    }

    #[test]
    fn a_single_json_argument_is_parsed_as_json() {
        let arg = vec![r#"{"cmd":"move","dir":"left"}"#.to_owned()];
        assert_eq!(parse_words(&arg), Ok(Request::Move { dir: Dir::Left }));
        let bad = vec![r#"{"cmd":"nope"}"#.to_owned()];
        assert!(parse_words(&bad).is_err());
    }

    #[test]
    fn bad_words_are_refused_with_a_reason() {
        let cases = [
            ("", "request"),
            ("explode", "unknown"),
            ("focus-slot", "direction"),
            ("focus-slot sideways", "direction"),
            ("move left right", "direction"),
            ("move", "direction"),
            ("move-tab left", "unknown"),
            ("split now", "no arguments"),
            ("toggle-stacking on", "no arguments"),
            ("spawn", "program"),
            ("cycle-tab", "next"),
            ("cycle-tab forward", "next"),
            ("layout", "layout"),
            ("layout a b c", "layout"),
            ("close now", "no arguments"),
            ("state please", "no arguments"),
        ];
        for (text, word) in cases {
            let err = parse_words(&words(text)).expect_err(text);
            assert!(err.contains(word), "{text:?}: {err:?} lacks {word:?}");
        }
    }

    fn setup() -> (Stacks, Config) {
        let config = default_config();
        let mut s = Stacks::new();
        let area = Rect {
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
        };
        s.add_monitor("winit", area, layout("wall"));
        s.add_monitor("other", area, layout("halves"));
        // Three tabs, so cycling has somewhere to go, the current one two deep, so split has something to split.
        s.open_stacked(1);
        s.open_tab(2, None, None);
        s.open_tab(3, None, None);
        s.open_stacked(4);
        (s, config)
    }

    #[test]
    fn apply_mirrors_the_stacks_methods() {
        type Op = fn(&mut Stacks);
        let cases: [(Request, Op); 7] = [
            (Request::FocusSlot { dir: Dir::Right }, |s| s.focus_slot(Dir::Right)),
            (Request::Move { dir: Dir::Right }, |s| s.move_stack(Dir::Right)),
            (Request::SwapSlot { dir: Dir::Down }, |s| s.swap_slot(Dir::Down)),
            (Request::CycleTab { cycle: Cycle::Next }, |s| s.cycle_tab(Cycle::Next)),
            (Request::CycleTab { cycle: Cycle::Prev }, |s| s.cycle_tab(Cycle::Prev)),
            (Request::Split, |s| s.split()),
            (
                Request::Layout {
                    name: "halves".into(),
                    monitor: None,
                },
                |s| s.set_layout("winit", layout("halves")),
            ),
        ];
        for (req, op) in cases {
            let (mut via_ipc, config) = setup();
            let (mut direct, _) = setup();
            assert_eq!(req.apply(&mut via_ipc, &config), Ok(()), "{req:?}");
            op(&mut direct);
            assert_eq!(via_ipc, direct, "{req:?}");
            assert_ne!(via_ipc, setup().0, "{req:?} changed nothing");
        }
    }

    #[test]
    fn layout_on_a_named_monitor() {
        let (mut s, config) = setup();
        let req = Request::Layout {
            name: "wall".into(),
            monitor: Some("other".into()),
        };
        assert_eq!(req.apply(&mut s, &config), Ok(()));
        assert_eq!(s.monitors()[1].layout().name, "wall");
        assert_eq!(s.monitors()[0].layout().name, "wall");
        assert_eq!(s.focused_monitor(), 0);
    }

    #[test]
    fn an_unknown_layout_or_monitor_is_an_error_and_changes_nothing() {
        let (mut s, config) = setup();
        let before = s.clone();
        let err = Request::Layout {
            name: "nope".into(),
            monitor: None,
        }
        .apply(&mut s, &config)
        .expect_err("unknown layout");
        assert!(err.contains("nope"), "{err}");
        let err = Request::Layout {
            name: "halves".into(),
            monitor: Some("hdmi".into()),
        }
        .apply(&mut s, &config)
        .expect_err("unknown monitor");
        assert!(err.contains("hdmi"), "{err}");
        assert_eq!(s, before);
    }

    #[test]
    fn layout_without_a_monitor_is_an_error() {
        let config = default_config();
        let mut s = Stacks::new();
        let req = Request::Layout {
            name: "wall".into(),
            monitor: None,
        };
        assert!(req.apply(&mut s, &config).is_err());
        assert_eq!(s, Stacks::new());
    }

    #[test]
    fn compositor_requests_are_not_applied_here() {
        for req in [
            Request::State,
            Request::Close,
            Request::Reload,
            Request::Quit,
            Request::ToggleStacking,
            Request::Spawn {
                argv: vec!["true".into()],
            },
        ] {
            let (mut s, config) = setup();
            let err = req.apply(&mut s, &config).expect_err("not pure");
            assert!(err.contains("compositor"), "{err}");
            assert_eq!(s, setup().0, "{req:?}");
        }
    }

    #[test]
    fn state_view_lists_every_slot_and_its_tabs() {
        let mut s = Stacks::new();
        let uhd = Rect {
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
        };
        s.add_monitor("winit", uhd, layout("wall"));
        s.open_stacked(1);
        s.open_stacked(2);
        s.open_tab(3, None, None);
        s.mark(1);
        s.add_monitor("second", uhd, layout("halves"));
        let v = StateView::new(&s, false, Some("broken".into()), |w| {
            (Some(format!("app{w}")), (w == 2).then(|| "two".into()))
        });
        assert_eq!(v.config_error.as_deref(), Some("broken"));
        assert!(!v.stacking);
        assert_eq!(v.monitors.len(), 2);
        let m = &v.monitors[0];
        assert_eq!((m.name.as_str(), m.layout.as_str(), m.focused), ("winit", "wall", true));
        assert_eq!(m.slots.len(), 6);
        assert_eq!(
            m.slots[0],
            SlotView {
                index: 0,
                name: None,
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 1280,
                    h: 720
                },
                focused: true,
                tabs: vec![
                    TabView {
                        current: false,
                        attention: true,
                        windows: vec![
                            WindowView {
                                id: 1,
                                app_id: Some("app1".into()),
                                title: None
                            },
                            WindowView {
                                id: 2,
                                app_id: Some("app2".into()),
                                title: Some("two".into())
                            },
                        ],
                    },
                    TabView {
                        current: true,
                        attention: false,
                        windows: vec![WindowView {
                            id: 3,
                            app_id: Some("app3".into()),
                            title: None
                        }],
                    },
                ],
            }
        );
        assert_eq!(m.slots[3].name.as_deref(), Some("hero"));
        assert!(!m.slots[3].focused);
        assert!(m.slots[3].tabs.is_empty());
        assert!(!v.monitors[1].focused);
        assert_eq!(v.monitors[1].slots.len(), 2);
        assert!(v.monitors[1].slots.iter().all(|sl| !sl.focused));
    }
}
