//! The control protocol spoken over the socket by `orrery msg`: one JSON `Request` line in, one `Response` out.

use serde::{Deserialize, Serialize};

use crate::config::{Colors, Config, Cycle, parse_dir, workspace_number};
use crate::crossing::RawKey;
use crate::layout::{Dir, Rect};
use crate::link::{Message, ScrollSource};
use crate::session::{Role, Screen};
use crate::stacks::{Stacks, WORKSPACES, WinId, Workspace};

/// Read through `Wire`: every variant needs a twin there (a test fails to compile until it has one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case", try_from = "Wire")]
pub enum Request {
    State,
    /// The event stream: the connection stays open, and orrery writes a `state` event, then an event per change
    /// (see `events`), one JSON object per line. Only on the control socket, never `to` another machine.
    Subscribe,
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
    /// Shows workspace `number` (1..=10) on its monitor and focuses that monitor; a missing one is made on the
    /// focused monitor.
    Workspace {
        number: u8,
    },
    /// The focused slot's current tab, its whole stack, to workspace `number` (1..=10) as a new tab; focus stays.
    MoveToWorkspace {
        number: u8,
    },
    /// One more screen, where the backend can open one.
    OpenScreen,
    /// Takes the screen whose output is `name` away (headless only, for tests); its workspaces move to the focused
    /// remaining screen, hidden.
    CloseScreen {
        name: String,
    },
    /// `request` run on machine `machine` of the session (`orrery msg --to`), through the primary; never another
    /// `to`.
    To {
        machine: String,
        request: Box<Request>,
    },
    /// A testing aid, answered only by an orrery started with `--debug-inject`: `input` (a link message: key, motion,
    /// button or axis) as if from this machine's own devices.
    Inject {
        input: Box<Message>,
    },
    /// A testing aid, answered only by a primary started with `--debug-inject`: `message` (an input message: keymap,
    /// handoff, key, motion, button, axis or release) sent as it is to satellite `machine`, as forwarding would.
    InjectRemote {
        machine: String,
        message: Box<Message>,
    },
}

/// How a `Request` is read: serde ignores unknown fields on internally tagged unit variants, so here they are
/// empty struct variants, which refuse them like every other request.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case", deny_unknown_fields)]
enum Wire {
    State {},
    Subscribe {},
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
    // Any JSON number, so every one out of range (or not an integer) gets the range in its error, not serde's
    // "expected u8".
    Workspace { number: serde_json::Number },
    MoveToWorkspace { number: serde_json::Number },
    OpenScreen {},
    CloseScreen { name: String },
    To { machine: String, request: Box<Request> },
    Inject { input: Box<Message> },
    InjectRemote { machine: String, message: Box<Message> },
}

impl TryFrom<Wire> for Request {
    type Error = String;

    fn try_from(w: Wire) -> Result<Self, String> {
        Ok(match w {
            Wire::State {} => Request::State,
            Wire::Subscribe {} => Request::Subscribe,
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
            Wire::Workspace { number } => Request::Workspace {
                number: workspace(number, "workspace")?,
            },
            Wire::MoveToWorkspace { number } => Request::MoveToWorkspace {
                number: workspace(number, "move-to-workspace")?,
            },
            Wire::OpenScreen {} => Request::OpenScreen,
            Wire::CloseScreen { name } => Request::CloseScreen { name },
            Wire::To { machine, request } => {
                if machine.is_empty() {
                    return Err("`to` takes a machine name".into());
                }
                if matches!(*request, Request::To { .. }) {
                    return Err("`to` takes one machine: its request cannot be another `to`".into());
                }
                if matches!(*request, Request::Subscribe) {
                    return Err(SUBSCRIBE_HERE.into());
                }
                Request::To { machine, request }
            }
            Wire::Inject { input } => Request::Inject {
                input: injected(input)?,
            },
            Wire::InjectRemote { machine, message } => {
                if machine.is_empty() {
                    return Err("`inject-remote` takes a machine name".into());
                }
                if !message.is_input() {
                    return Err(
                        "`inject-remote` takes an input message: keymap, handoff, key, motion, button, axis or release"
                            .into(),
                    );
                }
                Request::InjectRemote { machine, message }
            }
        })
    }
}

/// Why `subscribe` is refused anywhere but straight on the control socket.
pub const SUBSCRIBE_HERE: &str = "`subscribe` is answered on the control socket only, never `to` another machine";

/// An `inject` request's input: a key, motion, button or axis, as from a device.
fn injected(input: Box<Message>) -> Result<Box<Message>, String> {
    match *input {
        Message::Key { .. } | Message::Motion { .. } | Message::Button { .. } | Message::Axis { .. } => Ok(input),
        _ => Err("`inject` takes a key, motion, button or axis".into()),
    }
}

/// A workspace number as a request carries it: an integer, 1..=WORKSPACES.
fn workspace(number: serde_json::Number, verb: &str) -> Result<u8, String> {
    let n = (number.as_i64().map(i128::from)).or_else(|| number.as_u64().map(i128::from));
    n.and_then(workspace_number).ok_or_else(|| numbers(verb))
}

/// What a bad workspace number is told.
fn numbers(verb: &str) -> String {
    format!("`{verb}` takes a workspace number from 1 to {WORKSPACES}")
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
    /// Boxed: a link message carries responses, and the state is big.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Box<StateView>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateView {
    pub monitors: Vec<MonitorView>,
    /// Every existing workspace, shown or not, by number.
    pub workspaces: Vec<WorkspaceView>,
    /// Where a window with nowhere else to go opens: on top of the focused slot's current stack (true), or as a
    /// new tab there (false). `toggle-stacking` flips it.
    pub stacking: bool,
    pub config_error: Option<String>,
    /// How long drawing a frame takes (the renderer's backends only; absent before the first frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<FramesView>,
    /// What linux-dmabuf advertises (backends drawing with a GPU only; absent headless or when the renderer imports
    /// no client buffers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmabuf: Option<DmabufView>,
    /// The last key press the compositor saw, as its bind lookup read it (absent before the first): what to look
    /// at when a bind does nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_key: Option<KeyView>,
    /// Recent spawns and whether a window has taken each (see `placement`), oldest first: a debug aid.
    #[serde(default)]
    pub launches: Vec<LaunchView>,
    /// This machine's place in a session of linked orrerys; absent alone (no `listen` or `primary`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Box<SessionView>>,
    /// The config's colours, for programs drawing in orrery's (a bar).
    pub colors: ColorsView,
}

/// linux-dmabuf as advertised: how many format and modifier pairs clients may use, and the GPU named as the main
/// device of the default feedback (absent for a version 3 global, which has none).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufView {
    pub formats: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// The config's `colors`, each `#rrggbbaa`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorsView {
    pub focus: String,
    pub badge: String,
    pub attention: String,
    pub background: String,
}

impl From<&Colors> for ColorsView {
    fn from(c: &Colors) -> Self {
        Self {
            focus: c.focus.hex(),
            badge: c.badge.hex(),
            attention: c.attention.hex(),
            background: c.background.hex(),
        }
    }
}

/// A machine in a session: who it is, where it links, and the machines it knows. The primary knows every machine
/// in the session; a satellite only itself (and which numbers the others own).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    /// This machine's name.
    pub machine: String,
    pub role: Role,
    /// The primary: the address it listens on, the port picked when it was given 0; absent while not listening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// A satellite: the primary's address, as configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// The primary: itself, then the satellites in join order. A satellite: itself.
    pub machines: Vec<MachineView>,
    /// Where this machine's keyboard and pointer go: the machine holding the focus marker, on the primary; a
    /// satellite's own devices stay on it.
    pub input: String,
    /// The workspace numbers other machines own here: switching or moving to them is not done on this machine.
    pub foreign: Vec<u8>,
    /// Why a link last failed (a handshake, a refusal, a connection lost), until the next one opens.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineView {
    pub name: String,
    pub role: Role,
    /// Its screens, in number order.
    pub monitors: Vec<Screen>,
    /// The workspace numbers it owns, its screens' among them.
    pub workspaces: Vec<u8>,
    /// The round trip of its link to the primary (to this machine, for a satellite's own view); null for the
    /// primary itself, before the first answer to a ping and while one is overdue.
    pub latency_ms: Option<u64>,
    /// Its link is up (on the primary every machine in the session is).
    pub connected: bool,
    /// It holds the session's focus marker: `workspace N` last went to it (the primary's view only).
    pub focused: bool,
    /// The primary's input waiting for this machine's link to take more (motion merged): a stalled link shows here.
    #[serde(default)]
    pub queued: usize,
}

/// One recent spawn: the program's pid, the xdg-activation token it was handed, its age, and whether a window has
/// taken it (opened for it, or moved to it by an activation with its token). Forgotten lazily, after 5 s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchView {
    pub pid: u32,
    pub token: String,
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
    /// Its screen number, which locks the workspace of that number to it.
    pub number: u8,
    /// The workspace it shows; `layout` and `slots` are that workspace's.
    pub workspace: u8,
    pub layout: String,
    pub focused: bool,
    pub slots: Vec<SlotView>,
    /// Its mapped layer surfaces, back to front as drawn.
    #[serde(default)]
    pub layers: Vec<LayerView>,
}

/// A layer surface (layer-shell): the namespace its client gave, its layer, where it is drawn (global
/// coordinates), and whether it has the keyboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerView {
    pub namespace: String,
    pub layer: LayerName,
    pub rect: Rect,
    pub keyboard: bool,
}

/// The four layers, back to front: two under the slots, two over them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerName {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// One workspace, wherever it is: the monitor it lives on (by name), whether that monitor shows it, and its slots
/// as a monitor lists them (a hidden workspace's never focused).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub number: u8,
    pub monitor: String,
    pub shown: bool,
    pub layout: String,
    pub focused_slot: usize,
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
            state: Some(Box::new(state)),
            ..Self::ok()
        }
    }
}

impl StateView {
    /// The state as `orrery msg state` shows it; `info` gives a window's (app_id, title). A monitor lists the
    /// slots of the workspace it shows, and `workspaces` every workspace's. Pending windows have no slot and are
    /// not listed.
    pub fn new(
        stacks: &Stacks,
        stacking: bool,
        config_error: Option<String>,
        colors: &Colors,
        info: impl Fn(WinId) -> (Option<String>, Option<String>),
    ) -> Self {
        // A workspace's slots on its monitor's area; `focused` when it is the focused monitor's shown one.
        let slots = |ws: &Workspace, focused: bool| -> Vec<SlotView> {
            let area = stacks.monitors()[ws.monitor()].area();
            (ws.layout().slots.iter())
                .zip(ws.layout().rects(area))
                .zip(ws.slots())
                .enumerate()
                .map(|(index, ((spec, rect), slot))| SlotView {
                    index,
                    name: spec.name.clone(),
                    rect,
                    focused: focused && index == ws.focus(),
                    tabs: (slot.tabs().iter().enumerate())
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
                .collect()
        };
        let monitors = (stacks.monitors().iter().enumerate())
            .filter_map(|(mi, mon)| Some((mi, mon, stacks.shown(mi)?)))
            .map(|(mi, mon, ws)| {
                let focused = mi == stacks.focused_monitor();
                MonitorView {
                    name: mon.name().to_owned(),
                    number: mon.number(),
                    workspace: ws.number(),
                    layout: ws.layout().name.clone(),
                    focused,
                    slots: slots(ws, focused),
                    layers: Vec::new(),
                }
            })
            .collect();
        let workspaces = (stacks.workspaces().iter())
            .map(|ws| {
                let mon = &stacks.monitors()[ws.monitor()];
                let shown = mon.shown() == ws.number();
                WorkspaceView {
                    number: ws.number(),
                    monitor: mon.name().to_owned(),
                    shown,
                    layout: ws.layout().name.clone(),
                    focused_slot: ws.focus(),
                    slots: slots(ws, shown && ws.monitor() == stacks.focused_monitor()),
                }
            })
            .collect();
        StateView {
            monitors,
            workspaces,
            stacking,
            config_error,
            frames: None,
            dmabuf: None,
            last_key: None,
            launches: Vec::new(),
            session: None,
            colors: colors.into(),
        }
    }
}

impl Request {
    /// Runs a pure request against the stacks, layout names resolved in `config`. On error nothing changed.
    /// `state`, `reload`, `quit`, `toggle-stacking`, `spawn` and the screens' are the compositor's to answer, and so
    /// is `close`: closing asks the client to go, and its window leaves the stacks only when it is destroyed.
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
            Request::Workspace { number } => {
                if stacks.monitors().is_empty() {
                    return Err(format!("no monitor to show workspace {number} on"));
                }
                stacks.workspace(*number);
            }
            // No monitor means no focused stack: nothing to move, a no-op like moving from an empty slot, whereas
            // `workspace` would have nowhere to show what it asks for.
            Request::MoveToWorkspace { number } => {
                stacks.move_to_workspace(*number);
            }
            Request::State
            | Request::Subscribe
            | Request::Close
            | Request::Reload
            | Request::Quit
            | Request::ToggleStacking
            | Request::Spawn { .. }
            | Request::OpenScreen
            | Request::CloseScreen { .. }
            | Request::To { .. }
            | Request::Inject { .. }
            | Request::InjectRemote { .. } => {
                return Err(format!("{} is answered by the compositor", self.verb()));
            }
        }
        Ok(())
    }

    pub(crate) fn verb(&self) -> &'static str {
        match self {
            Request::State => "state",
            Request::Subscribe => "subscribe",
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
            Request::Workspace { .. } => "workspace",
            Request::MoveToWorkspace { .. } => "move-to-workspace",
            Request::OpenScreen => "open-screen",
            Request::CloseScreen { .. } => "close-screen",
            Request::To { .. } => "to",
            Request::Inject { .. } => "inject",
            Request::InjectRemote { .. } => "inject-remote",
        }
    }
}

/// `orrery msg` arguments: words (`focus-slot left`, `layout wall [MONITOR]`, `workspace 3`, `close-screen OUTPUT`,
/// `spawn PROGRAM [ARGS...]`, `inject key 38 down`, `inject-remote MACHINE release`) or a single JSON request.
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
    let number = |args: &[&str]| match args {
        [n] => (n.parse().ok())
            .and_then(workspace_number)
            .ok_or_else(|| format!("{}, not {n:?}", numbers(verb))),
        _ => Err(numbers(verb)),
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
        "subscribe" => none(Request::Subscribe),
        "close" => none(Request::Close),
        "reload" => none(Request::Reload),
        "quit" => none(Request::Quit),
        "split" => none(Request::Split),
        "toggle-stacking" => none(Request::ToggleStacking),
        "open-screen" => none(Request::OpenScreen),
        "close-screen" => match args.as_slice() {
            [name] => Ok(Request::CloseScreen {
                name: (*name).to_owned(),
            }),
            _ => Err("`close-screen` takes one output name".into()),
        },
        "workspace" => Ok(Request::Workspace { number: number(&args)? }),
        "move-to-workspace" => Ok(Request::MoveToWorkspace { number: number(&args)? }),
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
        "inject" => Ok(Request::Inject {
            input: injected(Box::new(input_words(&args)?))?,
        }),
        "inject-remote" => {
            let Some((machine, args)) = args.split_first() else {
                return Err("`inject-remote` takes a machine name and an input message".into());
            };
            Ok(Request::InjectRemote {
                machine: (*machine).to_owned(),
                message: Box::new(input_words(args)?),
            })
        }
        other => Err(format!("unknown request `{other}`")),
    }
}

/// An input message in words: `key CODE down|up` (an xkb keycode: evdev's plus 8), `motion DX DY`, `button CODE
/// down|up` (evdev's), `axis H V` (continuous scroll), `handoff [KEYCODE...]` (the modifier keys held, the pointer
/// staying where it is) or `release`. A keymap or a pointer entry takes JSON.
fn input_words(args: &[&str]) -> Result<Message, String> {
    let pressed = |s: &str| match s {
        "down" => Ok(true),
        "up" => Ok(false),
        _ => Err(format!("a key or button goes down or up, not {s:?}")),
    };
    let pair = |what: &str, a: &str, b: &str| match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(a), Ok(b)) if a.is_finite() && b.is_finite() => Ok((a, b)),
        _ => Err(format!("`{what}` takes two numbers")),
    };
    match args {
        ["key", code, state] => Ok(Message::Key {
            keycode: RawKey(
                code.parse()
                    .map_err(|_| format!("a key is its keycode, not {code:?}"))?,
            ),
            pressed: pressed(state)?,
            time: 0,
        }),
        ["button", code, state] => Ok(Message::Button {
            code: code
                .parse()
                .map_err(|_| format!("a button is its code, not {code:?}"))?,
            pressed: pressed(state)?,
        }),
        ["motion", dx, dy] => pair("motion", dx, dy).map(|(dx, dy)| Message::Motion { dx, dy }),
        ["axis", h, v] => pair("axis", h, v).map(|(h, v)| Message::Axis {
            source: ScrollSource::Continuous,
            h,
            v,
            v120_h: 0,
            v120_v: 0,
        }),
        ["release"] => Ok(Message::Release),
        ["handoff", mods @ ..] => Ok(Message::Handoff {
            mods: (mods.iter())
                .map(|m| {
                    m.parse()
                        .map(RawKey)
                        .map_err(|_| format!("a held key is its keycode, not {m:?}"))
                })
                .collect::<Result<_, _>>()?,
            pointer: None,
            layout: 0,
        }),
        ["key" | "button", ..] => Err("`key` and `button` take a code and down or up".into()),
        ["motion" | "axis", ..] => Err("`motion` and `axis` take two numbers".into()),
        _ => Err(concat!(
            "an input message: key CODE down|up, motion DX DY, button CODE down|up, axis H V, ",
            "handoff [KEYCODE...] or release"
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Color, default_config};
    use crate::crossing::{Edge, Entry, RawKey};
    use crate::link::{ScrollSource, Xkb};

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
            Request::Workspace { number } => (12, Wire::Workspace { number: number.into() }),
            Request::MoveToWorkspace { number } => (13, Wire::MoveToWorkspace { number: number.into() }),
            Request::OpenScreen => (14, Wire::OpenScreen {}),
            Request::CloseScreen { name } => (15, Wire::CloseScreen { name }),
            Request::To { machine, request } => (16, Wire::To { machine, request }),
            Request::Inject { input } => (17, Wire::Inject { input }),
            Request::InjectRemote { machine, message } => (18, Wire::InjectRemote { machine, message }),
            Request::Subscribe => (19, Wire::Subscribe {}),
        }
    }

    const VARIANTS: usize = 20;

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
            Request::Subscribe,
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
            Request::Workspace { number: 1 },
            Request::Workspace { number: 10 },
            Request::MoveToWorkspace { number: 3 },
            Request::OpenScreen,
            Request::CloseScreen {
                name: "headless-2".into(),
            },
            Request::To {
                machine: "desk".into(),
                request: Box::new(Request::Workspace { number: 2 }),
            },
            inject(key(38, true)),
            inject(Message::Motion { dx: -3.5, dy: 2.0 }),
            inject(Message::Button {
                code: 272,
                pressed: false,
            }),
            inject(axis(0.0, 15.0)),
            remote("side", Message::Release),
            remote(
                "side",
                Message::Handoff {
                    mods: vec![RawKey(133)],
                    pointer: Some(Entry {
                        edge: Edge::Left,
                        y: 0.5,
                    }),
                    layout: 1,
                },
            ),
            remote(
                "side",
                Message::Keymap {
                    xkb: Xkb("xkb_keymap { };".into()),
                },
            ),
        ]
    }

    fn inject(input: Message) -> Request {
        Request::Inject { input: Box::new(input) }
    }

    fn remote(machine: &str, message: Message) -> Request {
        Request::InjectRemote {
            machine: machine.into(),
            message: Box::new(message),
        }
    }

    fn key(code: u32, pressed: bool) -> Message {
        Message::Key {
            keycode: RawKey(code),
            pressed,
            time: 0,
        }
    }

    fn axis(h: f64, v: f64) -> Message {
        Message::Axis {
            source: ScrollSource::Continuous,
            h,
            v,
            v120_h: 0,
            v120_v: 0,
        }
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
        assert_eq!(json(&Request::Subscribe), r#"{"cmd":"subscribe"}"#);
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
        assert_eq!(
            json(&Request::Workspace { number: 3 }),
            r#"{"cmd":"workspace","number":3}"#
        );
        assert_eq!(
            json(&Request::MoveToWorkspace { number: 10 }),
            r#"{"cmd":"move-to-workspace","number":10}"#
        );
        assert_eq!(json(&Request::OpenScreen), r#"{"cmd":"open-screen"}"#);
        assert_eq!(
            json(&Request::To {
                machine: "desk".into(),
                request: Box::new(Request::State)
            }),
            r#"{"cmd":"to","machine":"desk","request":{"cmd":"state"}}"#
        );
        assert_eq!(
            json(&Request::CloseScreen {
                name: "headless-2".into()
            }),
            r#"{"cmd":"close-screen","name":"headless-2"}"#
        );
        assert_eq!(
            json(&inject(key(38, true))),
            r#"{"cmd":"inject","input":{"msg":"key","keycode":38,"pressed":true,"time":0}}"#
        );
        assert_eq!(
            json(&remote("side", Message::Release)),
            r#"{"cmd":"inject-remote","machine":"side","message":{"msg":"release"}}"#
        );
    }

    #[test]
    fn every_out_of_range_workspace_number_gets_the_range() {
        for n in [
            "0",
            "11",
            "256",
            "-1",
            "70000",
            "-9999999999",
            "9223372036854775808",
            "99999999999999999999999",
            "1e20",
            "1.5",
            "1.0",
        ] {
            for verb in ["workspace", "move-to-workspace"] {
                let json = format!(r#"{{"cmd":"{verb}","number":{n}}}"#);
                let err = serde_json::from_str::<Request>(&json).expect_err(&json).to_string();
                assert!(err.contains("1 to 10"), "{json}: {err}");
            }
        }
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
            r#"{"cmd":"workspace"}"#,
            r#"{"cmd":"workspace","number":0}"#,
            r#"{"cmd":"workspace","number":11}"#,
            r#"{"cmd":"workspace","number":256}"#,
            r#"{"cmd":"workspace","number":-1}"#,
            r#"{"cmd":"workspace","number":"3"}"#,
            r#"{"cmd":"move-to-workspace","number":0}"#,
            r#"{"cmd":"move-to-workspace","number":11}"#,
            r#"{"cmd":"open-screen","name":"x"}"#,
            r#"{"cmd":"close-screen"}"#,
            r#"{"cmd":"close-screen","name":2}"#,
            r#"{"cmd":"to","machine":"desk"}"#,
            r#"{"cmd":"to","request":{"cmd":"state"}}"#,
            r#"{"cmd":"to","machine":"","request":{"cmd":"state"}}"#,
            r#"{"cmd":"to","machine":"desk","request":{"cmd":"bogus"}}"#,
            r#"{"cmd":"to","machine":"a","request":{"cmd":"to","machine":"b","request":{"cmd":"state"}}}"#,
            r#"{"cmd":"to","machine":"a","request":{"cmd":"subscribe"}}"#,
            r#"{"cmd":"subscribe","x":1}"#,
            r#"{"cmd":"inject"}"#,
            r#"{"cmd":"inject","input":{"msg":"release"}}"#,
            r#"{"cmd":"inject","input":{"msg":"ping","nonce":1}}"#,
            r#"{"cmd":"inject","input":{"msg":"key","keycode":38,"pressed":true}}"#,
            r#"{"cmd":"inject-remote","message":{"msg":"release"}}"#,
            r#"{"cmd":"inject-remote","machine":"","message":{"msg":"release"}}"#,
            r#"{"cmd":"inject-remote","machine":"side","message":{"msg":"input-exit","edge":"left","y":0.5}}"#,
            r#"{"cmd":"inject-remote","machine":"side","message":{"msg":"command","id":1,"request":{"cmd":"quit"}}}"#,
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
                number: 1,
                workspace: 1,
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
                layers: vec![LayerView {
                    namespace: "panel".into(),
                    layer: LayerName::Top,
                    rect: Rect {
                        x: 0,
                        y: 0,
                        w: 3840,
                        h: 30,
                    },
                    keyboard: false,
                }],
            }],
            workspaces: vec![WorkspaceView {
                number: 4,
                monitor: "winit".into(),
                shown: false,
                layout: "halves".into(),
                focused_slot: 1,
                slots: Vec::new(),
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
            dmabuf: Some(DmabufView {
                formats: 42,
                device: Some("/dev/dri/renderD128".into()),
            }),
            last_key: Some(KeyView {
                key: "h".into(),
                mods: "Super".into(),
                bind: Some("focus-slot left".into()),
            }),
            launches: vec![LaunchView {
                pid: 4242,
                token: "tok".into(),
                age_ms: 120,
                claimed: false,
            }],
            session: Some(Box::new(SessionView {
                machine: "desk".into(),
                role: Role::Primary,
                listen: Some("127.0.0.1:7340".into()),
                primary: None,
                machines: vec![MachineView {
                    name: "desk".into(),
                    role: Role::Primary,
                    monitors: vec![Screen {
                        name: "winit".into(),
                        number: 1,
                    }],
                    workspaces: vec![1, 4],
                    latency_ms: None,
                    connected: true,
                    focused: true,
                    queued: 0,
                }],
                input: "desk".into(),
                foreign: vec![2],
                error: None,
            })),
            colors: ColorsView::from(&Colors {
                background: Color {
                    a: 0x80,
                    ..Colors::default().background
                },
                ..Colors::default()
            }),
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
        let session = concat!(
            r#""session":{"machine":"desk","role":"primary","listen":"127.0.0.1:7340","machines":[{"name":"desk","#,
            r#""role":"primary","monitors":[{"name":"winit","number":1}],"workspaces":[1,4],"latency_ms":null,"#,
            r#""connected":true,"focused":true,"queued":0}],"input":"desk","foreign":[2],"error":null}"#
        );
        assert!(json(&r).contains(session), "{}", json(&r));
        let alone = Response::with_state(StateView {
            session: None,
            ..state_view()
        });
        assert!(!json(&alone).contains("session"), "absent alone");
        let workspaces = concat!(
            r#""workspaces":[{"number":4,"monitor":"winit","shown":false,"layout":"halves","#,
            r#""focused_slot":1,"slots":[]}]"#
        );
        assert!(json(&r).contains(workspaces), "{}", json(&r));
        let layers =
            r#""layers":[{"namespace":"panel","layer":"top","rect":{"x":0,"y":0,"w":3840,"h":30},"keyboard":false}]"#;
        assert!(json(&r).contains(layers), "{}", json(&r));
        let colors = concat!(
            r##""colors":{"focus":"#ff7edbff","badge":"#ff7edbff","attention":"#ffb000ff","##,
            r##""background":"#1a162580"}"##
        );
        assert!(json(&r).contains(colors), "{}", json(&r));
    }

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn words_parse_for_every_verb() {
        let cases = [
            ("state", Request::State),
            ("subscribe", Request::Subscribe),
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
            ("workspace 1", Request::Workspace { number: 1 }),
            ("workspace 10", Request::Workspace { number: 10 }),
            ("move-to-workspace 4", Request::MoveToWorkspace { number: 4 }),
            ("open-screen", Request::OpenScreen),
            (
                "close-screen headless-2",
                Request::CloseScreen {
                    name: "headless-2".into(),
                },
            ),
            ("inject key 38 down", inject(key(38, true))),
            ("inject key 38 up", inject(key(38, false))),
            ("inject motion -10 2.5", inject(Message::Motion { dx: -10.0, dy: 2.5 })),
            (
                "inject button 272 down",
                inject(Message::Button {
                    code: 272,
                    pressed: true,
                }),
            ),
            ("inject axis 0 15", inject(axis(0.0, 15.0))),
            ("inject-remote side key 50 up", remote("side", key(50, false))),
            ("inject-remote side release", remote("side", Message::Release)),
            (
                "inject-remote side handoff 133 50",
                remote(
                    "side",
                    Message::Handoff {
                        mods: vec![RawKey(133), RawKey(50)],
                        pointer: None,
                        layout: 0,
                    },
                ),
            ),
            (
                "inject-remote side handoff",
                remote(
                    "side",
                    Message::Handoff {
                        mods: vec![],
                        pointer: None,
                        layout: 0,
                    },
                ),
            ),
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
            ("workspace", "1 to 10"),
            ("workspace 0", "1 to 10"),
            ("workspace 11", "1 to 10"),
            ("workspace -3", "1 to 10"),
            ("workspace three", "not \"three\""),
            ("workspace 11", "not \"11\""),
            ("workspace 1 2", "1 to 10"),
            ("move-to-workspace", "1 to 10"),
            ("move-to-workspace 300", "1 to 10"),
            ("open-screen now", "no arguments"),
            ("close-screen", "output name"),
            ("close-screen a b", "output name"),
            ("inject", "key"),
            ("inject key", "key"),
            ("inject key a down", "key"),
            ("inject key 38 sideways", "down"),
            ("inject motion 1", "motion"),
            ("inject motion 1 x", "motion"),
            ("inject motion inf 0", "motion"),
            ("inject axis 1", "axis"),
            ("inject button 272", "button"),
            ("inject release", "key"),
            ("inject handoff", "key"),
            ("inject-remote", "machine"),
            ("inject-remote side", "release"),
            ("inject-remote side handoff x", "keycode"),
            ("inject-remote side explode", "release"),
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
        let cases: [(Request, Op); 9] = [
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
            (Request::Workspace { number: 4 }, |s| {
                s.workspace(4);
            }),
            (Request::MoveToWorkspace { number: 2 }, |s| {
                s.move_to_workspace(2);
            }),
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
        assert_eq!(s.shown(1).unwrap().layout().name, "wall");
        assert_eq!(s.shown(0).unwrap().layout().name, "wall");
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
    fn workspace_without_a_monitor_is_an_error() {
        let config = default_config();
        let mut s = Stacks::new();
        let err = Request::Workspace { number: 2 }
            .apply(&mut s, &config)
            .expect_err("no monitor");
        assert!(err.contains("monitor"), "{err}");
        assert_eq!(Request::MoveToWorkspace { number: 2 }.apply(&mut s, &config), Ok(()));
        assert_eq!(s, Stacks::new());
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
            Request::OpenScreen,
            Request::CloseScreen { name: "other".into() },
            Request::To {
                machine: "desk".into(),
                request: Box::new(Request::Split),
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
        let v = StateView::new(&s, false, Some("broken".into()), &Colors::default(), |w| {
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

    #[test]
    fn state_view_lists_every_workspace_and_what_each_monitor_shows() {
        let mut s = Stacks::new();
        let uhd = Rect {
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
        };
        let side = Rect { x: 3840, ..uhd };
        s.add_monitor("winit", uhd, layout("wall"));
        s.add_monitor("second", side, layout("halves"));
        // winit shows 3 (one window, its slot 0 focused) and hides 1 (two tabs in slot 0); second shows 2.
        s.open_stacked(1);
        s.open_tab(2, None, None);
        s.focus_slot(Dir::Right);
        s.workspace(3);
        s.open_stacked(3);
        let v = StateView::new(&s, true, None, &Colors::default(), |_| (None, None));
        let shows: Vec<(&str, u8, u8, bool)> = (v.monitors.iter())
            .map(|m| (m.name.as_str(), m.number, m.workspace, m.focused))
            .collect();
        assert_eq!(shows, [("winit", 1, 3, true), ("second", 2, 2, false)]);
        let all: Vec<(u8, &str, bool, &str, usize, usize)> = (v.workspaces.iter())
            .map(|w| {
                (
                    w.number,
                    w.monitor.as_str(),
                    w.shown,
                    w.layout.as_str(),
                    w.focused_slot,
                    w.slots.len(),
                )
            })
            .collect();
        assert_eq!(
            all,
            [
                (1, "winit", false, "wall", 3, 6),
                (2, "second", true, "halves", 0, 2),
                (3, "winit", true, "wall", 0, 6),
            ]
        );
        // Slots as the monitors list them: the shown ones the same, a hidden one's never focused.
        assert_eq!(v.workspaces[2].slots, v.monitors[0].slots);
        assert_eq!(v.workspaces[1].slots, v.monitors[1].slots);
        let hidden = &v.workspaces[0].slots;
        assert!(hidden.iter().all(|sl| !sl.focused));
        assert_eq!(hidden[0].tabs.len(), 2);
        assert_eq!(hidden[0].tabs[1].windows[0].id, 2);
        assert_eq!(hidden[3].rect, v.monitors[0].slots[3].rect, "on its monitor's area");
    }
}
