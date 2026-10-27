//! The KDL config: layouts, monitors, colours and key binds, parsed strictly.
//!
//! Parsing is pure (a string in, a model out); reading the file is the caller's job.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlEntry, KdlNode};
use smithay::input::keyboard::{Keysym, xkb};

use crate::layout::{Dir, Layout, LayoutError, MAX_WEIGHT, SlotSpec};
use crate::session::Role;
use crate::stacks::WORKSPACES;

/// The built-in config, used when no file exists. Kept as a file so it can be copied to start a config; the
/// README shows it verbatim (tested).
pub const DEFAULT: &str = include_str!("../examples/config.kdl");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub focus: Color,
    pub badge: Color,
    /// A slot or tab holding a window that asked for attention.
    pub attention: Color,
    pub background: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cycle {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FocusSlot(Dir),
    Move(Dir),
    SwapSlot(Dir),
    CycleTab(Cycle),
    Split,
    ToggleStacking,
    Layout(String),
    /// 1..=WORKSPACES.
    Workspace(u8),
    /// 1..=WORKSPACES.
    MoveToWorkspace(u8),
    Spawn(Vec<String>),
    Close,
    ShowTabs,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub mods: Mods,
    /// The key as written in the config.
    pub key: String,
    /// `key` resolved case-folded and unshifted: `"Mod+Shift+H"` stores `h` with Shift in `mods`. Matching must
    /// compare the raw, unshifted keysym of a key press (e.g. `KeysymHandle::raw_latin_sym_or_raw_current_sym`),
    /// not the shifted one.
    pub keysym: Keysym,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub modifier: Mods,
    pub layouts: Vec<Layout>,
    pub monitors: Vec<MonitorRule>,
    /// Where a window with nowhere else to go opens at startup: on top of the focused stack, or (false) as a new
    /// tab there. The running value is the compositor's (`toggle-stacking`).
    pub stacking: bool,
    pub colors: Colors,
    pub binds: Vec<Bind>,
    /// This machine's place in a session of linked orrerys (`listen` or `primary`); None: alone.
    pub link: Option<LinkConfig>,
    /// `name`: this machine's name in a session; None: its hostname.
    pub name: Option<String>,
}

/// A machine name (`name`, a satellite's Hello) is at most this many bytes.
pub const MAX_NAME: usize = 64;

/// `listen "ADDR:PORT" secret-file="PATH"` (the primary, `Role::Primary`) or `primary "HOST:PORT"
/// secret-file="PATH"` (a satellite, `Role::Satellite`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    pub role: Role,
    /// The primary: the address it listens on (an IP address and a port; port 0 picks one). A satellite: where the
    /// primary is (a host name or IP address, and a port).
    pub addr: String,
    /// As written: absolute, or `~/` for the home directory (see `secret_path`).
    pub secret_file: String,
}

impl LinkConfig {
    /// The secret file's path, `~/` taken as `home` (left as written without one).
    #[must_use]
    pub fn secret_path(&self, home: Option<&Path>) -> PathBuf {
        match (self.secret_file.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(&self.secret_file),
        }
    }
}

/// Whether `name` can name a machine: 1 to `MAX_NAME` bytes, no control characters.
#[must_use]
pub fn valid_machine_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && !name.chars().any(char::is_control)
}

/// The layout an output starts with (outputs without a rule get the first layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorRule {
    pub output: String,
    pub layout: String,
}

/// One problem in a config, 1-based line and column (in characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", .problems.iter().map(Problem::to_string).collect::<Vec<_>>().join("\n"))]
pub struct ConfigError {
    pub problems: Vec<Problem>,
}

impl Color {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xff }
    }

    /// `#rrggbb` or `#rrggbbaa`.
    fn parse(s: &str) -> Option<Self> {
        let hex = s.strip_prefix('#')?;
        if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 0xff },
        })
    }
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            focus: Color::rgb(0xff, 0x7e, 0xdb),
            badge: Color::rgb(0xff, 0x7e, 0xdb),
            attention: Color::rgb(0xff, 0xb0, 0x00),
            background: Color::rgb(0x1a, 0x16, 0x25),
        }
    }
}

impl Mods {
    const SUPER: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        logo: true,
    };

    fn union(self, o: Mods) -> Mods {
        Mods {
            ctrl: self.ctrl || o.ctrl,
            alt: self.alt || o.alt,
            shift: self.shift || o.shift,
            logo: self.logo || o.logo,
        }
    }

    /// One modifier name, case-insensitive. `Mod` is handled by the caller.
    fn named(name: &str) -> Option<Mods> {
        let mut m = Mods::default();
        match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => m.ctrl = true,
            "alt" => m.alt = true,
            "shift" => m.shift = true,
            "super" | "logo" => m.logo = true,
            _ => return None,
        }
        Some(m)
    }
}

/// Sources above this are refused before parsing.
const MAX_SOURCE: usize = 1 << 20;
/// Deepest `{` nesting allowed; the config needs 2. kdl's parser recurses per level, so an unbounded depth can
/// overflow the stack and abort the process, which no error handling can catch.
const MAX_DEPTH: usize = 16;

/// Parse a config. Every problem found is reported, each with its line and column.
pub fn parse(src: &str) -> Result<Config, ConfigError> {
    precheck(src).map_err(|p| ConfigError { problems: vec![p] })?;
    let doc = match KdlDocument::parse_v2(src) {
        Ok(doc) => doc,
        Err(e) => {
            let problems = e
                .diagnostics
                .iter()
                .map(|d| {
                    let mut message = d.message.clone().unwrap_or_else(|| "syntax error".into());
                    if let Some(help) = &d.help {
                        message = format!("{message} ({help})");
                    }
                    // kdl 6.7.1 gives some errors (e.g. a missing property value) a span from the start of the
                    // document to where it stopped; a span across lines marks its end, not its start.
                    let (start, len) = (d.span.offset(), d.span.len());
                    let spans_lines = src.get(start..start + len).is_some_and(|t| t.contains('\n'));
                    Problem::at(src, if spans_lines { start + len } else { start }, message)
                })
                .collect();
            return Err(ConfigError { problems });
        }
    };
    Parser { src, problems: vec![] }.config(&doc)
}

/// Size and nesting limits, checked before kdl sees the source. Braces are counted naively (inside strings and
/// comments too): the limit is generous, and erring towards refusal is safe.
fn precheck(src: &str) -> Result<(), Problem> {
    if src.len() > MAX_SOURCE {
        return Err(Problem::at(src, 0, "the config is larger than 1 MiB".into()));
    }
    let mut depth = 0usize;
    for (i, b) in src.bytes().enumerate() {
        match b {
            b'{' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(Problem::at(
                        src,
                        i,
                        format!("blocks nest too deep (more than {MAX_DEPTH})"),
                    ));
                }
            }
            b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// The built-in config. `DEFAULT` is proven to parse by the tests, so this cannot fail at runtime.
pub fn default_config() -> Config {
    parse(DEFAULT).expect("the built-in config parses")
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl Problem {
    fn at(src: &str, offset: usize, message: String) -> Self {
        let before = &src[..src.floor_char_boundary(offset)];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Problem { line, col, message }
    }
}

/// A bind as written, before `Mod` is known (the `mod` node may come later in the file).
struct RawBind<'a> {
    node: &'a KdlNode,
    mods: Mods,
    uses_mod: bool,
    key: String,
    keysym: Keysym,
    action: Action,
    /// Where a `layout` action's name is, for the unknown-layout check.
    target_at: usize,
}

/// A `monitor` node, checked against the layouts once all are known.
struct RawMonitor {
    output: String,
    layout: String,
    layout_at: usize,
}

struct Parser<'a> {
    src: &'a str,
    problems: Vec<Problem>,
}

impl<'a> Parser<'a> {
    fn err(&mut self, node: &KdlNode, message: impl Into<String>) {
        self.err_at(node.span().offset(), message);
    }

    fn err_at(&mut self, offset: usize, message: impl Into<String>) {
        self.problems.push(Problem::at(self.src, offset, message.into()));
    }

    fn config(mut self, doc: &'a KdlDocument) -> Result<Config, ConfigError> {
        self.no_type_annotations(doc);
        let mut modifier: Option<Mods> = None;
        let mut layouts = Vec::new();
        // Every declared layout name, valid or not, so a broken layout doesn't also flag its references.
        let mut layout_names: Vec<String> = Vec::new();
        let mut monitors: Vec<RawMonitor> = Vec::new();
        let mut stacking: Option<bool> = None;
        let mut colors: Option<Colors> = None;
        let mut raw_binds: Vec<RawBind> = Vec::new();
        let mut seen_binds = false;
        let mut link: Option<LinkConfig> = None;
        let mut seen_link: Option<&str> = None;
        let mut name: Option<String> = None;
        let mut seen_name = false;

        for node in doc.nodes() {
            match node.name().value() {
                "mod" => {
                    if modifier.is_some() {
                        self.err(node, "`mod` is set twice");
                    }
                    self.no_children(node);
                    if let Some((text, at)) = self.only_string(node, "`mod` takes one modifier name, like \"Alt\"") {
                        modifier = Some(self.mods_value(at, &text));
                    }
                }
                "layout" => {
                    let Some((name, _)) = self.only_string(node, "`layout` takes one name") else {
                        continue;
                    };
                    if layout_names.contains(&name) {
                        self.err(node, format!("layout {name:?} is defined twice"));
                        continue;
                    }
                    layout_names.push(name.clone());
                    if let Some(l) = self.layout(node, name) {
                        layouts.push(l);
                    }
                }
                "monitor" => {
                    if let Some(m) = self.monitor(node) {
                        if monitors.iter().any(|o| o.output == m.output) {
                            self.err(node, format!("monitor {:?} is set twice", m.output));
                        } else {
                            monitors.push(m);
                        }
                    }
                }
                "stacking" => {
                    if stacking.is_some() {
                        self.err(node, "`stacking` is set twice");
                    }
                    self.no_children(node);
                    self.props(node, &[]);
                    match positional(node).as_slice() {
                        [e] if e.value().as_bool().is_some() => stacking = e.value().as_bool(),
                        _ => self.err(node, "`stacking` takes #true or #false"),
                    }
                }
                "colors" => {
                    if colors.is_some() {
                        self.err(node, "`colors` is set twice");
                    }
                    colors = Some(self.colors(node));
                }
                which @ ("listen" | "primary") => {
                    match seen_link {
                        Some(w) if w == which => self.err(node, format!("`{which}` is set twice")),
                        Some(_) => self.err(
                            node,
                            "`listen` and `primary` are both set: a machine is the primary or a satellite",
                        ),
                        None => {}
                    }
                    seen_link = Some(which);
                    if let Some(l) = self.link(node) {
                        link = Some(l);
                    }
                }
                "name" => {
                    if seen_name {
                        self.err(node, "`name` is set twice");
                    }
                    seen_name = true;
                    self.no_children(node);
                    let msg = format!("`name` takes one machine name, 1 to {MAX_NAME} bytes, no control characters");
                    if let Some((text, at)) = self.only_string(node, &msg) {
                        if valid_machine_name(&text) {
                            name = Some(text);
                        } else {
                            self.err_at(at, msg);
                        }
                    }
                }
                "binds" => {
                    if seen_binds {
                        self.err(node, "`binds` is set twice");
                    }
                    seen_binds = true;
                    self.binds(node, &mut raw_binds);
                }
                other => self.err(node, format!("unknown node `{other}`")),
            }
        }

        if layout_names.is_empty() {
            self.err_at(0, "the config defines no layout");
        }
        for m in &monitors {
            if !layout_names.contains(&m.layout) {
                let msg = format!("monitor {:?} uses unknown layout {:?}", m.output, m.layout);
                self.err_at(m.layout_at, msg);
            }
        }

        let modifier = modifier.unwrap_or(Mods::SUPER);
        let binds = self.resolve_binds(raw_binds, modifier, &layout_names);

        if !self.problems.is_empty() {
            // Stable sort: file order, with problems on one node kept in the order found.
            self.problems.sort_by_key(|p| (p.line, p.col));
            return Err(ConfigError {
                problems: self.problems,
            });
        }
        Ok(Config {
            modifier,
            layouts,
            monitors: monitors
                .into_iter()
                .map(|m| MonitorRule {
                    output: m.output,
                    layout: m.layout,
                })
                .collect(),
            stacking: stacking.unwrap_or(true),
            colors: colors.unwrap_or_default(),
            binds,
            link,
            name,
        })
    }

    /// A `listen` or `primary` node.
    fn link(&mut self, node: &KdlNode) -> Option<LinkConfig> {
        self.no_children(node);
        let which = node.name().value();
        let (role, msg) = match which {
            "listen" => (
                Role::Primary,
                "`listen` takes the address and port to listen on, like \"0.0.0.0:7340\"",
            ),
            _ => (
                Role::Satellite,
                "`primary` takes the primary's HOST:PORT, like \"desk.lan:7340\"",
            ),
        };
        let file = self.props(node, &["secret-file"])[0];
        let addr = self.one_string(node, msg).and_then(|(addr, at)| {
            let ok = match role {
                Role::Primary => addr.parse::<SocketAddr>().is_ok(),
                Role::Satellite => host_port(&addr),
            };
            if !ok {
                self.err_at(at, msg);
            }
            ok.then_some(addr)
        });
        let secret_file = match file {
            Some(e) => self.string_value(e, "secret-file").and_then(|f| {
                let ok = f.starts_with('/') || f.starts_with("~/");
                if !ok {
                    let msg = "`secret-file` must be an absolute path, or start with ~/ for the home directory";
                    self.err_at(e.span().offset(), msg);
                }
                ok.then_some(f)
            }),
            None => {
                self.err(node, format!("`{which}` needs secret-file=\"PATH\""));
                None
            }
        };
        Some(LinkConfig {
            role,
            addr: addr?,
            secret_file: secret_file?,
        })
    }

    /// Type annotations like `(u8)1` mean nothing here; refuse them rather than ignore them.
    fn no_type_annotations(&mut self, doc: &KdlDocument) {
        for node in doc.nodes() {
            if node.ty().is_some() {
                self.err(node, "type annotations are not supported");
            }
            for e in node.entries().iter().filter(|e| e.ty().is_some()) {
                self.err_at(e.span().offset(), "type annotations are not supported");
            }
            if let Some(children) = node.children() {
                self.no_type_annotations(children);
            }
        }
    }

    fn monitor(&mut self, node: &KdlNode) -> Option<RawMonitor> {
        self.no_children(node);
        let output = self.one_string(node, "`monitor` takes an output name");
        let layout = match self.props(node, &["layout"])[0] {
            Some(e) => self.string_value(e, "layout").map(|l| (l, e.span().offset())),
            None => {
                self.err(node, "`monitor` needs layout=\"<name>\"");
                None
            }
        };
        let ((output, _), (layout, layout_at)) = (output?, layout?);
        Some(RawMonitor {
            output,
            layout,
            layout_at,
        })
    }

    fn binds(&mut self, node: &'a KdlNode, out: &mut Vec<RawBind<'a>>) {
        self.no_args(node, "binds");
        for child in children(node) {
            if let Some(b) = self.bind(child) {
                out.push(b);
            }
        }
    }

    /// Expand `Mod`, check layout targets and refuse duplicates (same mods and keysym).
    fn resolve_binds(&mut self, raw_binds: Vec<RawBind>, modifier: Mods, layout_names: &[String]) -> Vec<Bind> {
        let mut binds: Vec<Bind> = Vec::new();
        for raw in raw_binds {
            if let Action::Layout(name) = &raw.action
                && !layout_names.contains(name)
            {
                self.err_at(raw.target_at, format!("bind uses unknown layout {name:?}"));
            }
            let mods = if raw.uses_mod {
                raw.mods.union(modifier)
            } else {
                raw.mods
            };
            if binds.iter().any(|b| b.mods == mods && b.keysym == raw.keysym) {
                let written = raw.node.name().value();
                self.err(raw.node, format!("{written:?} is bound twice"));
                continue;
            }
            binds.push(Bind {
                mods,
                key: raw.key,
                keysym: raw.keysym,
                action: raw.action,
            });
        }
        binds
    }

    /// A `layout` node: the valid layout, or its problems reported.
    fn layout(&mut self, node: &'a KdlNode, name: String) -> Option<Layout> {
        let mut grid: Option<(u32, u32)> = None;
        // `widths` and `heights`: the node (for the count check against the grid) and the weights.
        let mut weights: [Option<(&KdlNode, Vec<u32>)>; 2] = [None, None];
        let mut slots: Vec<(&KdlNode, SlotSpec)> = Vec::new();
        let mut ok = true;
        for child in children(node) {
            match child.name().value() {
                which @ ("widths" | "heights") => {
                    let i = usize::from(which == "heights");
                    if weights[i].is_some() {
                        self.err(child, format!("`{which}` is set twice"));
                        ok = false;
                    }
                    match self.weights(child) {
                        Some(w) => weights[i] = Some((child, w)),
                        None => ok = false,
                    }
                }
                "grid" => {
                    self.no_children(child);
                    if let Some(e) = positional(child).first() {
                        self.err_at(e.span().offset(), "`grid` takes only cols= and rows=");
                    }
                    let p = self.props(child, &["cols", "rows"]);
                    let cols = self.required_u32(child, "cols", p[0]);
                    let rows = self.required_u32(child, "rows", p[1]);
                    if grid.is_some() {
                        self.err(child, "`grid` is set twice");
                        ok = false;
                    }
                    match (cols, rows) {
                        (Some(c), Some(r)) => grid = Some((c, r)),
                        _ => ok = false,
                    }
                }
                "slot" => {
                    self.no_children(child);
                    let slot_name = match positional(child).as_slice() {
                        [] => None,
                        [e] => match e.value().as_string() {
                            Some(s) => Some(s.to_owned()),
                            None => {
                                self.err_at(e.span().offset(), "a slot's name must be a string");
                                ok = false;
                                None
                            }
                        },
                        [_, extra, ..] => {
                            self.err_at(extra.span().offset(), "`slot` takes at most one name");
                            ok = false;
                            None
                        }
                    };
                    let p = self.props(child, &["col", "row", "cols", "rows"]);
                    let col = self.required_u32(child, "col", p[0]);
                    let row = self.required_u32(child, "row", p[1]);
                    let cols = p[2].map_or(Some(1), |e| self.u32_value(e, "cols"));
                    let rows = p[3].map_or(Some(1), |e| self.u32_value(e, "rows"));
                    match (col, row, cols, rows) {
                        (Some(col), Some(row), Some(cols), Some(rows)) => slots.push((
                            child,
                            SlotSpec {
                                name: slot_name,
                                col,
                                row,
                                cols,
                                rows,
                            },
                        )),
                        _ => ok = false,
                    }
                }
                other => {
                    self.err(child, format!("unknown node `{other}` in layout {name:?}"));
                    ok = false;
                }
            }
        }
        let Some((cols, rows)) = grid else {
            if ok {
                self.err(node, format!("layout {name:?} needs a `grid cols=N rows=N`"));
            }
            return None;
        };
        for ((what, unit), (weights, want)) in [("widths", "column"), ("heights", "row")]
            .into_iter()
            .zip(weights.iter().zip([cols, rows]))
        {
            if let Some((at, w)) = weights
                && w.len() != want as usize
            {
                let plural = if want == 1 { "" } else { "s" };
                self.err(at, format!("{} {what} for {want} {unit}{plural}", w.len()));
                ok = false;
            }
        }
        if !ok {
            return None;
        }
        let [col_weights, row_weights] = weights.map(|w| w.map(|(_, w)| w).unwrap_or_default());
        let (nodes, slots): (Vec<&KdlNode>, Vec<SlotSpec>) = slots.into_iter().unzip();
        let layout = Layout {
            name,
            cols,
            rows,
            col_weights,
            row_weights,
            slots,
        };
        match layout.validate() {
            Ok(()) => Some(layout),
            Err(e) => {
                // Point at the slot at fault where there is one.
                let at = match &e {
                    LayoutError::OutOfGrid { slot }
                    | LayoutError::Overlap { b: slot, .. }
                    | LayoutError::TooSmall { slot, .. } => nodes[*slot],
                    LayoutError::DuplicateName { name } => layout
                        .slots
                        .iter()
                        .rposition(|s| s.name.as_ref() == Some(name))
                        .map_or(node, |i| nodes[i]),
                    _ => node,
                };
                self.err(at, format!("layout {:?}: {e}", layout.name));
                None
            }
        }
    }

    /// `widths`/`heights`: one or more whole numbers 1..=10_000, nothing else.
    fn weights(&mut self, node: &KdlNode) -> Option<Vec<u32>> {
        self.no_children(node);
        let what = node.name().value();
        let mut ok = node.children().is_none();
        self.props(node, &[]);
        ok &= node.entries().iter().all(|e| e.name().is_none());
        let args = positional(node);
        if args.is_empty() {
            self.err(node, format!("`{what}` takes one weight per cell, like `{what} 2 1 1`"));
            return None;
        }
        let mut out = Vec::with_capacity(args.len());
        for e in args {
            match e.value().as_integer().and_then(|i| u32::try_from(i).ok()) {
                Some(w) if (1..=MAX_WEIGHT).contains(&w) => out.push(w),
                _ => {
                    self.err_at(
                        e.span().offset(),
                        format!("a weight must be a whole number from 1 to {MAX_WEIGHT}"),
                    );
                    ok = false;
                }
            }
        }
        ok.then_some(out)
    }

    fn colors(&mut self, node: &KdlNode) -> Colors {
        self.no_args(node, "colors");
        let mut colors = Colors::default();
        let mut seen: Vec<&str> = Vec::new();
        for child in children(node) {
            let key = child.name().value();
            let slot = match key {
                "focus" => &mut colors.focus,
                "badge" => &mut colors.badge,
                "attention" => &mut colors.attention,
                "background" => &mut colors.background,
                other => {
                    self.err(child, format!("unknown colour `{other}`"));
                    continue;
                }
            };
            if seen.contains(&key) {
                self.err(child, format!("colour `{key}` is set twice"));
            }
            seen.push(key);
            self.no_children(child);
            let Some((text, at)) = self.only_string(child, "a colour takes one \"#rrggbb\" or \"#rrggbbaa\" string")
            else {
                continue;
            };
            match Color::parse(&text) {
                Some(c) => *slot = c,
                None => self.err_at(at, format!("{text:?} is not a colour (#rrggbb or #rrggbbaa)")),
            }
        }
        colors
    }

    fn bind(&mut self, node: &'a KdlNode) -> Option<RawBind<'a>> {
        self.no_children(node);
        if let Some(e) = node.entries().iter().find(|e| e.name().is_some()) {
            self.err_at(e.span().offset(), "binds take no properties");
            return None;
        }
        let combo = node.name().value();
        let parts: Vec<&str> = combo.split('+').collect();
        let (key, mod_names) = parts.split_last()?;
        if key.is_empty() {
            self.err(
                node,
                format!("{combo:?} has no key after its modifiers (the + key is written `plus`)"),
            );
            return None;
        }
        let keysym = resolve_key(key);
        let mut ok = keysym.is_some();
        if !ok {
            self.err(node, format!("unknown key {key:?} in {combo:?}"));
        }
        let mut mods = Mods::default();
        let mut uses_mod = false;
        for name in mod_names {
            if name.eq_ignore_ascii_case("mod") {
                uses_mod = true;
            } else if let Some(m) = Mods::named(name) {
                mods = mods.union(m);
            } else {
                self.err(node, format!("unknown modifier {name:?} in {combo:?}"));
                ok = false;
            }
        }
        let action = self.action(node);
        let target_at = positional(node)
            .get(1)
            .map_or(node.span().offset(), |e| e.span().offset());
        Some(RawBind {
            node,
            mods,
            uses_mod,
            key: (*key).to_owned(),
            keysym: keysym?,
            action: action.filter(|_| ok)?,
            target_at,
        })
    }

    fn action(&mut self, node: &KdlNode) -> Option<Action> {
        let args = positional(node);
        let Some((verb_entry, rest)) = args.split_first() else {
            self.err(node, "a bind needs an action");
            return None;
        };
        let verb_at = verb_entry.span().offset();
        let Some(verb) = verb_entry.value().as_string() else {
            self.err_at(verb_at, "a bind's action must be a name, like `close`");
            return None;
        };
        // The first argument's position, for errors about its value.
        let arg_at = rest.first().map_or(verb_at, |e| e.span().offset());
        if let "workspace" | "move-to-workspace" = verb {
            // A number, not a string: the one action that takes one.
            let n = match rest {
                [e] => e.value().as_integer().and_then(workspace_number),
                _ => None,
            };
            let Some(n) = n else {
                self.err_at(
                    arg_at,
                    format!("`{verb}` takes a workspace number from 1 to {WORKSPACES}"),
                );
                return None;
            };
            return Some(if verb == "workspace" {
                Action::Workspace(n)
            } else {
                Action::MoveToWorkspace(n)
            });
        }
        let strings: Option<Vec<&str>> = rest.iter().map(|e| e.value().as_string()).collect();
        let action = match (verb, strings.as_deref()) {
            ("focus-slot" | "move" | "swap-slot", Some([dir])) => {
                let Some(dir) = parse_dir(dir) else {
                    self.err_at(arg_at, format!("{dir:?} is not a direction (left, right, up, down)"));
                    return None;
                };
                match verb {
                    "focus-slot" => Action::FocusSlot(dir),
                    "move" => Action::Move(dir),
                    _ => Action::SwapSlot(dir),
                }
            }
            ("cycle-tab", Some([c])) => match *c {
                "next" => Action::CycleTab(Cycle::Next),
                "prev" => Action::CycleTab(Cycle::Prev),
                other => {
                    self.err_at(arg_at, format!("{other:?} is not a cycle (next, prev)"));
                    return None;
                }
            },
            ("layout", Some([name])) => Action::Layout((*name).to_owned()),
            ("spawn", Some(argv @ [_, ..])) if argv.iter().all(|a| !a.is_empty()) => {
                Action::Spawn(argv.iter().map(|s| (*s).to_owned()).collect())
            }
            ("focus-slot" | "move" | "swap-slot", _) => {
                self.err_at(verb_at, format!("`{verb}` takes one direction"));
                return None;
            }
            ("cycle-tab", _) => {
                self.err_at(verb_at, "`cycle-tab` takes \"next\" or \"prev\"");
                return None;
            }
            ("layout", _) => {
                self.err_at(verb_at, "`layout` takes one layout name");
                return None;
            }
            ("spawn", _) => {
                self.err_at(
                    verb_at,
                    "`spawn` takes a program and its arguments, as non-empty strings",
                );
                return None;
            }
            ("split", Some([])) => Action::Split,
            ("toggle-stacking", Some([])) => Action::ToggleStacking,
            ("close", Some([])) => Action::Close,
            ("show-tabs", Some([])) => Action::ShowTabs,
            ("quit", Some([])) => Action::Quit,
            ("split" | "toggle-stacking" | "close" | "show-tabs" | "quit", _) => {
                self.err_at(arg_at, format!("`{verb}` takes no arguments"));
                return None;
            }
            (other, _) => {
                self.err_at(verb_at, format!("unknown action `{other}`"));
                return None;
            }
        };
        Some(action)
    }

    /// A `mod` value: modifier names joined by `+`, without `Mod` itself.
    fn mods_value(&mut self, at: usize, text: &str) -> Mods {
        let mut mods = Mods::default();
        for name in text.split('+') {
            match Mods::named(name) {
                Some(m) => mods = mods.union(m),
                None => self.err_at(at, format!("unknown modifier {name:?}")),
            }
        }
        mods
    }

    fn no_children(&mut self, node: &KdlNode) {
        if node.children().is_some() {
            self.err(node, format!("`{}` takes no block", node.name().value()));
        }
    }

    fn no_args(&mut self, node: &KdlNode, what: &str) {
        if let Some(e) = node.entries().first() {
            self.err_at(e.span().offset(), format!("`{what}` takes no arguments"));
        }
    }

    /// Exactly one positional string (properties are the caller's business), with its position.
    fn one_string(&mut self, node: &KdlNode, msg: &str) -> Option<(String, usize)> {
        match positional(node).as_slice() {
            [e] => match e.value().as_string() {
                Some(s) => Some((s.to_owned(), e.span().offset())),
                None => {
                    self.err_at(e.span().offset(), msg);
                    None
                }
            },
            _ => {
                self.err(node, msg);
                None
            }
        }
    }

    /// Exactly one positional string and no properties.
    fn only_string(&mut self, node: &KdlNode, msg: &str) -> Option<(String, usize)> {
        self.props(node, &[]);
        self.one_string(node, msg)
    }

    /// The `allowed` properties, in that order; any other property, or one set twice, is a problem.
    fn props<'n>(&mut self, node: &'n KdlNode, allowed: &[&str]) -> Vec<Option<&'n KdlEntry>> {
        let mut out = vec![None; allowed.len()];
        for e in node.entries() {
            let Some(key) = e.name() else { continue };
            let key = key.value();
            let at = e.span().offset();
            match allowed.iter().position(|a| *a == key) {
                Some(i) if out[i].is_some() => self.err_at(at, format!("`{key}` is set twice")),
                Some(i) => out[i] = Some(e),
                None => self.err_at(at, format!("unknown property `{key}` on `{}`", node.name().value())),
            }
        }
        out
    }

    fn string_value(&mut self, e: &KdlEntry, key: &str) -> Option<String> {
        match e.value().as_string() {
            Some(s) => Some(s.to_owned()),
            None => {
                self.err_at(e.span().offset(), format!("`{key}` must be a string"));
                None
            }
        }
    }

    fn required_u32(&mut self, node: &KdlNode, key: &str, e: Option<&KdlEntry>) -> Option<u32> {
        match e {
            Some(e) => self.u32_value(e, key),
            None => {
                self.err(node, format!("`{}` needs {key}=N", node.name().value()));
                None
            }
        }
    }

    fn u32_value(&mut self, e: &KdlEntry, key: &str) -> Option<u32> {
        match e.value().as_integer().and_then(|i| u32::try_from(i).ok()) {
            Some(n) => Some(n),
            None => {
                self.err_at(e.span().offset(), format!("`{key}` must be a whole number from 0 up"));
                None
            }
        }
    }
}

/// `HOST:PORT`: a host (a name, an IPv4 address, or an IPv6 one in brackets) and a port, 1 to 65535.
fn host_port(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    let host_ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => v6.parse::<std::net::Ipv6Addr>().is_ok(),
        None => !host.is_empty() && !host.contains([':', '[', ']']) && !host.chars().any(char::is_whitespace),
    };
    host_ok && port.parse::<u16>().is_ok_and(|p| p > 0)
}

fn children(node: &KdlNode) -> &[KdlNode] {
    node.children().map_or(&[], |d| d.nodes())
}

fn positional(node: &KdlNode) -> Vec<&KdlEntry> {
    node.entries().iter().filter(|e| e.name().is_none()).collect()
}

/// An xkb keysym by name, case-insensitively; a single non-ASCII character (`ä`) maps by its lower-case code
/// point.
/// None when there is no such keysym.
fn resolve_key(name: &str) -> Option<Keysym> {
    // xkbcommon's binding panics on an interior NUL; no keysym name has one.
    if name.contains('\0') {
        return None;
    }
    let mut chars = name.chars();
    let sym = match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_ascii() => {
            // Bind keysyms are unshifted: fold "É" to "é" when it lowers to exactly one character.
            let mut lower = c.to_lowercase();
            let c = match (lower.next(), lower.next()) {
                (Some(l), None) => l,
                _ => c,
            };
            xkb::utf32_to_keysym(u32::from(c))
        }
        _ => xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE),
    };
    (sym.raw() != xkb::keysyms::KEY_NoSymbol).then_some(sym)
}

/// A workspace number as written: 1..=WORKSPACES, else None.
pub(crate) fn workspace_number(n: i128) -> Option<u8> {
    u8::try_from(n).ok().filter(|n| (1..=WORKSPACES).contains(n))
}

pub(crate) fn parse_dir(s: &str) -> Option<Dir> {
    match s {
        "left" => Some(Dir::Left),
        "right" => Some(Dir::Right),
        "up" => Some(Dir::Up),
        "down" => Some(Dir::Down),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SlotSpec;
    use smithay::input::keyboard::keysyms as k;

    fn slot(name: Option<&str>, col: u32, row: u32, cols: u32, rows: u32) -> SlotSpec {
        SlotSpec {
            name: name.map(String::from),
            col,
            row,
            cols,
            rows,
        }
    }

    const MOD: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        logo: true,
    };
    const MOD_SHIFT: Mods = Mods { shift: true, ..MOD };
    const MOD_CTRL: Mods = Mods { ctrl: true, ..MOD };
    const PINK: Color = Color {
        r: 0xff,
        g: 0x7e,
        b: 0xdb,
        a: 0xff,
    };
    const AMBER: Color = Color {
        r: 0xff,
        g: 0xb0,
        b: 0x00,
        a: 0xff,
    };

    fn bind(mods: Mods, key: &str, keysym: u32, action: Action) -> Bind {
        Bind {
            mods,
            key: key.into(),
            keysym: Keysym::new(keysym),
            action,
        }
    }

    fn expected_default() -> Config {
        Config {
            modifier: MOD,
            layouts: vec![
                Layout {
                    name: "wall".into(),
                    cols: 3,
                    rows: 3,
                    col_weights: vec![],
                    row_weights: vec![],
                    slots: vec![
                        slot(None, 0, 0, 1, 1),
                        slot(None, 0, 1, 1, 1),
                        slot(None, 0, 2, 1, 1),
                        slot(Some("hero"), 1, 0, 2, 2),
                        slot(None, 1, 2, 1, 1),
                        slot(None, 2, 2, 1, 1),
                    ],
                },
                Layout {
                    name: "halves".into(),
                    cols: 2,
                    rows: 1,
                    col_weights: vec![],
                    row_weights: vec![],
                    slots: vec![slot(None, 0, 0, 1, 1), slot(None, 1, 0, 1, 1)],
                },
            ],
            monitors: vec![MonitorRule {
                output: "winit".into(),
                layout: "wall".into(),
            }],
            stacking: true,
            colors: Colors {
                focus: PINK,
                badge: PINK,
                attention: AMBER,
                background: Color {
                    r: 0x1a,
                    g: 0x16,
                    b: 0x25,
                    a: 0xff,
                },
            },
            binds: vec![
                bind(MOD, "H", k::KEY_h, Action::FocusSlot(Dir::Left)),
                bind(MOD, "J", k::KEY_j, Action::FocusSlot(Dir::Down)),
                bind(MOD, "K", k::KEY_k, Action::FocusSlot(Dir::Up)),
                bind(MOD, "L", k::KEY_l, Action::FocusSlot(Dir::Right)),
                bind(MOD_SHIFT, "H", k::KEY_h, Action::Move(Dir::Left)),
                bind(MOD_SHIFT, "J", k::KEY_j, Action::Move(Dir::Down)),
                bind(MOD_SHIFT, "K", k::KEY_k, Action::Move(Dir::Up)),
                bind(MOD_SHIFT, "L", k::KEY_l, Action::Move(Dir::Right)),
                bind(MOD_CTRL, "H", k::KEY_h, Action::SwapSlot(Dir::Left)),
                bind(MOD_CTRL, "J", k::KEY_j, Action::SwapSlot(Dir::Down)),
                bind(MOD_CTRL, "K", k::KEY_k, Action::SwapSlot(Dir::Up)),
                bind(MOD_CTRL, "L", k::KEY_l, Action::SwapSlot(Dir::Right)),
                bind(MOD, "Tab", k::KEY_Tab, Action::CycleTab(Cycle::Next)),
                bind(MOD_SHIFT, "Tab", k::KEY_Tab, Action::CycleTab(Cycle::Prev)),
                bind(MOD, "S", k::KEY_s, Action::Split),
                bind(MOD_SHIFT, "S", k::KEY_s, Action::ToggleStacking),
                bind(MOD_CTRL, "1", k::KEY_1, Action::Layout("wall".into())),
                bind(MOD_CTRL, "2", k::KEY_2, Action::Layout("halves".into())),
            ]
            .into_iter()
            .chain(digit_binds(MOD, Action::Workspace))
            .chain(digit_binds(MOD_SHIFT, Action::MoveToWorkspace))
            .chain([
                bind(MOD, "Return", k::KEY_Return, Action::Spawn(vec!["kitty".into()])),
                bind(MOD_SHIFT, "Q", k::KEY_q, Action::Close),
                bind(MOD, "Space", k::KEY_space, Action::ShowTabs),
                bind(MOD_SHIFT, "E", k::KEY_e, Action::Quit),
            ])
            .collect(),
            link: None,
            name: None,
        }
    }

    /// `mods` with keys 1..9 then 0, for workspaces 1..10.
    fn digit_binds(mods: Mods, action: fn(u8) -> Action) -> Vec<Bind> {
        (1..=10u8)
            .map(|n| {
                let digit = n % 10;
                bind(mods, &digit.to_string(), k::KEY_0 + u32::from(digit), action(n))
            })
            .collect()
    }

    /// The source with one line replaced, so a broken line keeps a known number.
    fn with_line(n: usize, text: &str) -> String {
        DEFAULT
            .lines()
            .enumerate()
            .map(|(i, l)| if i + 1 == n { text } else { l })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_of(needle: &str) -> usize {
        DEFAULT
            .lines()
            .position(|l| l.contains(needle))
            .expect("needle in DEFAULT")
            + 1
    }

    /// Parse must fail with a problem on `line` whose message contains `word`.
    fn assert_problem(src: &str, line: usize, word: &str) {
        let err = parse(src).expect_err("config should be refused");
        assert!(
            err.problems.iter().any(|p| p.line == line && p.message.contains(word)),
            "no problem on line {line} mentioning {word:?}: {err}"
        );
        assert!(
            err.to_string().contains(&format!("{line}:")),
            "display lacks line: {err}"
        );
    }

    #[test]
    fn the_readme_shows_the_built_in_config_verbatim() {
        let readme = include_str!("../README.md");
        assert!(
            readme.contains(&format!("```kdl\n{DEFAULT}```")),
            "README's example drifted from examples/config.kdl"
        );
    }

    #[test]
    fn the_example_parses_to_the_expected_model() {
        assert_eq!(parse(DEFAULT), Ok(expected_default()));
    }

    #[test]
    fn default_config_is_the_example() {
        assert_eq!(default_config(), expected_default());
    }

    #[test]
    fn mod_can_be_set_after_the_binds_and_combines() {
        let src = "layout \"a\" {\n grid cols=1 rows=1\n slot col=0 row=0\n}\nbinds {\n \"Mod+Shift+x\" quit\n}\nmod \"Super\"\n";
        let c = parse(src).expect("parses");
        assert_eq!(
            c.binds,
            vec![bind(
                Mods {
                    logo: true,
                    shift: true,
                    ..Mods::default()
                },
                "x",
                k::KEY_x,
                Action::Quit
            )]
        );
    }

    #[test]
    fn spawn_takes_an_argv_and_colors_take_alpha() {
        let src = "layout \"a\" {\n grid cols=1 rows=1\n slot col=0 row=0\n}\ncolors {\n focus \"#01020304\"\n}\nbinds {\n \"Alt+T\" spawn \"kitty\" \"-e\" \"htop\"\n}\n";
        let c = parse(src).expect("parses");
        assert_eq!(
            c.binds[0].action,
            Action::Spawn(vec!["kitty".into(), "-e".into(), "htop".into()])
        );
        assert_eq!(c.colors.focus, Color { r: 1, g: 2, b: 3, a: 4 });
        // Unset colours keep their defaults, and so does stacking.
        assert_eq!(c.colors.badge, PINK);
        assert_eq!(c.colors.attention, AMBER);
        assert!(c.stacking);
    }

    #[test]
    fn stacking_and_attention_are_read() {
        let n = line_of("stacking #true");
        let c = parse(&with_line(n, "stacking #false")).expect("parses");
        assert!(!c.stacking);
        // Left out, it is on.
        let c = parse(&with_line(n, "")).expect("parses");
        assert!(c.stacking);
        let n = line_of("attention \"#ffb000\"");
        let c = parse(&with_line(n, "    attention \"#00ff0080\"")).expect("parses");
        assert_eq!(
            c.colors.attention,
            Color {
                r: 0,
                g: 0xff,
                b: 0,
                a: 0x80
            }
        );
    }

    #[test]
    fn stacking_takes_one_bool() {
        let n = line_of("stacking #true");
        for bad in [
            "stacking",
            "stacking \"yes\"",
            "stacking 1",
            "stacking #true #false",
            "stacking x=#true",
        ] {
            assert_problem(&with_line(n, bad), n, "stacking");
        }
        assert_problem(&with_line(n, "stacking #true {\n}"), n, "block");
        assert_problem(&with_line(n, "stacking #true\nstacking #false"), n + 1, "twice");
    }

    #[test]
    fn move_tab_is_gone_and_split_and_toggle_stacking_take_nothing() {
        let n = line_of("\"Mod+Shift+H\"");
        assert_problem(&with_line(n, "    \"Mod+Shift+H\" move-tab \"left\""), n, "move-tab");
        assert_problem(&with_line(n, "    \"Mod+Shift+H\" move"), n, "direction");
        let n = line_of("\"Mod+S\"");
        assert_problem(&with_line(n, "    \"Mod+S\" split \"now\""), n, "no arguments");
        let n = line_of("\"Mod+Shift+S\"");
        assert_problem(
            &with_line(n, "    \"Mod+Shift+S\" toggle-stacking #true"),
            n,
            "toggle-stacking",
        );
    }

    #[test]
    fn syntax_errors_carry_the_line() {
        let src = with_line(5, "    grid cols=3 rows=");
        assert_problem(&src, 5, "property value");
        let err = parse(&src).expect_err("refused");
        assert_eq!((err.problems[0].line, err.problems[0].col), (5, 22), "{err}");
    }

    #[test]
    fn an_invalid_layout_is_refused() {
        let n = line_of("slot col=2 row=2");
        assert_problem(&with_line(n, "    slot col=2 row=2 cols=2"), n, "outside the grid");
        let n = line_of("slot col=1 row=2");
        assert_problem(&with_line(n, "    slot col=1 row=1"), n, "overlap");
    }

    #[test]
    fn layout_names_are_unique() {
        let n = line_of("layout \"halves\"");
        assert_problem(&with_line(n, "layout \"wall\" {"), n, "wall");
    }

    #[test]
    fn monitor_layout_must_exist() {
        let n = line_of("monitor \"winit\"");
        assert_problem(&with_line(n, "monitor \"winit\" layout=\"nope\""), n, "nope");
    }

    #[test]
    fn bind_layout_must_exist() {
        let n = line_of("\"Mod+Ctrl+2\"");
        assert_problem(&with_line(n, "    \"Mod+Ctrl+2\" layout \"nope\""), n, "nope");
    }

    #[test]
    fn workspace_binds_take_a_number_from_1_to_10() {
        let n = line_of("\"Mod+3\"");
        for action in ["workspace", "move-to-workspace"] {
            for bad in ["0", "11", "-1", "300", "\"3\"", "1.5", "", "1 2"] {
                let line = format!("    \"Mod+3\" {action} {bad}");
                assert_problem(&with_line(n, &line), n, "1 to 10");
            }
        }
        let c = parse(&with_line(n, "    \"Mod+3\" move-to-workspace 10")).expect("in range");
        let b = c
            .binds
            .iter()
            .find(|b| b.key == "3" && b.mods == MOD)
            .expect("the bind");
        assert_eq!(b.action, Action::MoveToWorkspace(10));
    }

    #[test]
    fn colors_must_be_hex() {
        let n = line_of("focus \"#ff7edb\"");
        assert_problem(&with_line(n, "    focus \"pink\""), n, "colour");
        let n = line_of("attention \"#ffb000\"");
        assert_problem(&with_line(n, "    attention \"amber\""), n, "colour");
        assert_problem(&with_line(n, "    focus \"#ff7ed\""), n, "colour");
        assert_problem(&with_line(n, "    focus \"#gg7edb\""), n, "colour");
    }

    #[test]
    fn spawn_needs_a_program() {
        let n = line_of("\"Mod+Return\"");
        assert_problem(&with_line(n, "    \"Mod+Return\" spawn"), n, "spawn");
        assert_problem(&with_line(n, "    \"Mod+Return\" spawn 3"), n, "spawn");
    }

    #[test]
    fn unknown_nodes_are_refused() {
        assert_problem(&with_line(2, "gaps 4"), 2, "gaps");
        let n = line_of("grid cols=2");
        assert_problem(&with_line(n, "    grod cols=2 rows=1"), n, "grod");
        let n = line_of("background");
        assert_problem(&with_line(n, "    border \"#000000\""), n, "border");
    }

    #[test]
    fn unknown_properties_are_refused() {
        let n = line_of("slot \"hero\"");
        assert_problem(
            &with_line(n, "    slot \"hero\" col=1 row=0 cols=2 rows=2 kind=\"tabs\""),
            n,
            "kind",
        );
    }

    #[test]
    fn unknown_actions_and_arguments_are_refused() {
        let n = line_of("\"Mod+Shift+Q\"");
        assert_problem(&with_line(n, "    \"Mod+Q\" kill"), n, "kill");
        assert_problem(&with_line(n, "    \"Mod+Q\" close \"now\""), n, "close");
        let n = line_of("\"Mod+H\"");
        assert_problem(&with_line(n, "    \"Mod+H\" focus-slot \"west\""), n, "west");
        let n = line_of("\"Mod+Tab\"");
        assert_problem(&with_line(n, "    \"Mod+Tab\" cycle-tab \"sideways\""), n, "sideways");
    }

    #[test]
    fn unknown_modifiers_are_refused() {
        let n = line_of("\"Mod+Shift+Q\"");
        assert_problem(&with_line(n, "    \"Hyper+Q\" close"), n, "Hyper");
        assert_problem(&with_line(2, "mod \"Meta\""), 2, "Meta");
        assert_problem(&with_line(n, "    \"Mod+\" close"), n, "key");
    }

    #[test]
    fn bind_keys_must_be_real_keysyms() {
        let n = line_of("\"Mod+Return\"");
        assert_problem(&with_line(n, "    \"Mod+Retrun\" spawn \"kitty\""), n, "unknown key");
        assert_problem(
            &with_line(n, "    \"Mod+Ret\\u{0}urn\" spawn \"kitty\""),
            n,
            "unknown key",
        );
        let c = default_config();
        let sym = |key: &str| c.binds.iter().find(|b| b.key == key).map(|b| b.keysym.raw());
        assert_eq!(sym("Return"), Some(k::KEY_Return));
        assert_eq!(sym("Tab"), Some(k::KEY_Tab));
        assert_eq!(sym("1"), Some(k::KEY_1));
        assert_eq!(sym("Space"), Some(k::KEY_space));
    }

    #[test]
    fn duplicate_bindings_compare_keysyms() {
        // "return" and "Return" name the same keysym.
        let n = line_of("\"Mod+Shift+Q\"");
        assert_problem(&with_line(n, "    \"Super+return\" close"), n, "bound twice");
    }

    #[test]
    fn duplicate_bindings_are_refused() {
        let n = line_of("\"Mod+Shift+Q\"");
        // Mod is Alt, and keys compare case-insensitively.
        assert_problem(&with_line(n, "    \"Super+h\" close"), n, "Super+h");
    }

    #[test]
    fn every_problem_is_listed() {
        let src = with_line(line_of("\"Mod+Shift+Q\""), "    \"Mod+Q\" kill");
        let src = src.replace("monitor \"winit\" layout=\"wall\"", "monitor \"winit\" layout=\"nope\"");
        let err = parse(&src).expect_err("refused");
        assert_eq!(err.problems.len(), 2, "{err}");
        assert!(
            err.problems[0].line < err.problems[1].line,
            "problems in file order: {err}"
        );
    }

    #[test]
    fn a_config_needs_a_layout() {
        assert_problem("binds {\n}\n", 1, "layout");
    }

    /// Where a token can start or end: next to whitespace or KDL punctuation, and the two ends.
    fn token_boundaries(src: &str) -> Vec<usize> {
        let punct = |c: char| c.is_whitespace() || "{}=\";()/#".contains(c);
        let mut out = vec![0, src.len()];
        let mut prev: Option<char> = None;
        for (i, c) in src.char_indices() {
            if punct(c) || prev.is_some_and(punct) {
                out.push(i);
            }
            prev = Some(c);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    #[test]
    fn truncated_input_never_panics() {
        // Cut at every token boundary; the exhaustive version (every byte) is `truncated_anywhere_never_panics`.
        let cuts = token_boundaries(DEFAULT);
        assert!(cuts.len() > 500, "{} cuts", cuts.len());
        for i in cuts {
            let _ = parse(&DEFAULT[..i]);
        }
    }

    #[test]
    #[ignore = "slow: every byte prefix of the example (cargo test -- --ignored)"]
    fn truncated_anywhere_never_panics() {
        for i in 0..=DEFAULT.len() {
            if let Some(prefix) = DEFAULT.get(..i) {
                let _ = parse(prefix);
            }
        }
    }

    fn problem_at(src: &str, line: usize, col: usize, word: &str) {
        let err = parse(src).expect_err("config should be refused");
        assert!(
            err.problems
                .iter()
                .any(|p| (p.line, p.col) == (line, col) && p.message.contains(word)),
            "no problem at {line}:{col} mentioning {word:?}: {err}"
        );
    }

    #[test]
    fn deep_nesting_is_refused_without_recursing() {
        let start = std::time::Instant::now();
        let err = parse(&"a {".repeat(100_000)).expect_err("refused");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        // The 17th `{` is the first too deep: "a {" repeated, so it sits at column 16 * 3 + 3.
        assert_eq!((err.problems[0].line, err.problems[0].col), (1, 51), "{err}");
        assert!(err.problems[0].message.contains("deep"), "{err}");
        // 16 levels are still fine for the depth check (kdl then reports the missing `}`s).
        let err = parse(&"a {".repeat(16)).expect_err("unclosed");
        assert!(!err.to_string().contains("deep"), "{err}");
    }

    #[test]
    fn huge_sources_are_refused() {
        let src = format!("{DEFAULT}{}", "// padding\n".repeat(100_000));
        let err = parse(&src).expect_err("refused");
        assert!(err.to_string().contains("MiB"), "{err}");
    }

    #[test]
    fn semantic_errors_point_at_the_entry() {
        let n = line_of("slot col=2 row=2");
        problem_at(&with_line(n, "    slot col=2 row=2 cols=\"x\""), n, 22, "cols");
        let n = line_of("monitor \"winit\"");
        problem_at(&with_line(n, "monitor \"winit\" layout=\"nope\""), n, 17, "nope");
        let n = line_of("\"Mod+Ctrl+2\"");
        problem_at(&with_line(n, "    \"Mod+Ctrl+2\" layout \"nope\""), n, 25, "nope");
        let n = line_of("\"Mod+H\"");
        problem_at(&with_line(n, "    \"Mod+H\" focus-slot \"west\""), n, 24, "west");
    }

    #[test]
    fn problems_display_as_line_col_message() {
        let p = Problem {
            line: 5,
            col: 22,
            message: "boom".into(),
        };
        assert_eq!(p.to_string(), "5:22: boom");
        let e = ConfigError {
            problems: vec![p.clone(), p],
        };
        assert_eq!(e.to_string(), "5:22: boom\n5:22: boom");
    }

    #[test]
    fn things_set_twice_are_refused() {
        let n = line_of("grid cols=3");
        assert_problem(
            &with_line(n, "    grid cols=3 rows=3\n    grid cols=3 rows=3"),
            n + 1,
            "twice",
        );
        assert_problem(&with_line(2, "mod \"Alt\"\nmod \"Alt\""), 3, "twice");
        let n = line_of("colors {");
        assert_problem(&with_line(n, "colors {\n}\ncolors {"), n + 2, "twice");
        let n = line_of("monitor \"winit\"");
        assert_problem(
            &with_line(
                n,
                "monitor \"winit\" layout=\"wall\"\nmonitor \"winit\" layout=\"wall\"",
            ),
            n + 1,
            "twice",
        );
        let n = line_of("binds {");
        assert_problem(&with_line(n, "binds {\n}\nbinds {"), n + 2, "twice");
        let n = line_of("focus \"#ff7edb\"");
        assert_problem(
            &with_line(n, "    focus \"#ff7edb\"\n    focus \"#ff7edb\""),
            n + 1,
            "twice",
        );
    }

    #[test]
    fn a_layout_needs_a_grid() {
        let n = line_of("grid cols=2");
        assert_problem(&with_line(n, ""), line_of("layout \"halves\""), "grid");
    }

    #[test]
    fn a_slot_name_must_be_a_string() {
        let n = line_of("slot \"hero\"");
        assert_problem(&with_line(n, "    slot 7 col=1 row=0 cols=2 rows=2"), n, "name");
    }

    #[test]
    fn a_gap_points_at_the_layout() {
        let n = line_of("slot col=1 row=0");
        assert_problem(&with_line(n, ""), line_of("layout \"halves\""), "in no slot");
    }

    #[test]
    fn crlf_line_numbers() {
        let n = line_of("\"Mod+Shift+Q\"");
        let src = with_line(n, "    \"Mod+Q\" kill").replace('\n', "\r\n");
        problem_at(&src, n, 13, "kill");
    }

    #[test]
    fn type_annotations_are_refused() {
        let n = line_of("slot col=0 row=0");
        assert_problem(&with_line(n, "    slot col=(u8)0 row=0"), n, "type annotation");
        let n = line_of("layout \"halves\"");
        assert_problem(&with_line(n, "(fancy)layout \"halves\" {"), n, "type annotation");
    }

    #[test]
    fn spawn_needs_a_non_empty_program() {
        let n = line_of("\"Mod+Return\"");
        assert_problem(&with_line(n, "    \"Mod+Return\" spawn \"\""), n, "spawn");
    }

    #[test]
    fn a_non_ascii_key_resolves_by_character() {
        let n = line_of("\"Mod+Shift+Q\"");
        let c = parse(&with_line(n, "    \"Mod+ä\" close")).expect("parses");
        let b = c.binds.iter().find(|b| b.key == "ä").expect("bound");
        assert_eq!(b.keysym.raw(), k::KEY_adiaeresis);
    }

    #[test]
    fn a_non_ascii_key_is_case_folded() {
        // "É" and "é" are one key: both resolve to the unshifted keysym, so binding both is a duplicate.
        let n = line_of("\"Mod+Shift+Q\"");
        let c = parse(&with_line(n, "    \"Mod+É\" close")).expect("parses");
        let b = c.binds.iter().find(|b| b.key == "É").expect("bound");
        assert_eq!(b.keysym.raw(), k::KEY_eacute);
        let both = with_line(n, "    \"Alt+é\" close\n    \"Alt+É\" quit");
        assert_problem(&both, n + 1, "bound twice");
    }

    #[test]
    fn spawn_refuses_an_empty_argument() {
        let n = line_of("\"Mod+Return\"");
        assert_problem(&with_line(n, "    \"Mod+Return\" spawn \"kitty\" \"\""), n, "non-empty");
    }

    #[test]
    fn a_plus_key_hints_at_its_name() {
        let n = line_of("\"Mod+Shift+Q\"");
        assert_problem(&with_line(n, "    \"Alt++\" close"), n, "plus");
        assert!(parse(&with_line(n, "    \"Alt+plus\" close")).is_ok());
    }

    /// A one-layout config whose layout body is `body`, on lines 2 onwards.
    fn one_layout(body: &str) -> String {
        format!("layout \"w\" {{\n{body}\n}}\n")
    }

    #[test]
    fn widths_and_heights_weight_the_grid() {
        let src = one_layout(
            "    grid cols=3 rows=2\n    widths 81 81 94\n    heights 1 2\n    slot col=0 row=0 cols=3 rows=2",
        );
        let c = parse(&src).expect("parses");
        assert_eq!(c.layouts[0].col_weights, vec![81, 81, 94]);
        assert_eq!(c.layouts[0].row_weights, vec![1, 2]);
        // Either may come before the grid, and either alone is fine.
        let src = one_layout("    heights 1 3\n    grid cols=1 rows=2\n    slot col=0 row=0 rows=2");
        let c = parse(&src).expect("parses");
        assert_eq!(
            (c.layouts[0].col_weights.len(), c.layouts[0].row_weights.clone()),
            (0, vec![1, 3])
        );
    }

    #[test]
    fn weights_must_match_the_grid() {
        let src = one_layout("    grid cols=3 rows=1\n    widths 1 1\n    slot col=0 row=0 cols=3");
        problem_at(&src, 3, 5, "2 widths for 3 columns");
        let src = one_layout("    grid cols=1 rows=1\n    heights 1 1 1\n    slot col=0 row=0");
        problem_at(&src, 3, 5, "3 heights for 1 row");
    }

    #[test]
    fn weights_must_be_1_to_10000() {
        let src = one_layout("    grid cols=3 rows=1\n    widths 1 0 1\n    slot col=0 row=0 cols=3");
        problem_at(&src, 3, 14, "1 to 10000");
        let src = one_layout("    grid cols=3 rows=1\n    widths 1 1 10001\n    slot col=0 row=0 cols=3");
        problem_at(&src, 3, 16, "1 to 10000");
        let src = one_layout("    grid cols=3 rows=1\n    widths 1 -1 1\n    slot col=0 row=0 cols=3");
        problem_at(&src, 3, 14, "1 to 10000");
        let src = one_layout("    grid cols=3 rows=1\n    widths 1 \"2\" 1\n    slot col=0 row=0 cols=3");
        problem_at(&src, 3, 14, "1 to 10000");
        let src = one_layout("    grid cols=3 rows=1\n    widths 1000 1000 10000\n    slot col=0 row=0 cols=3");
        assert!(parse(&src).is_ok());
    }

    #[test]
    fn weights_are_strict() {
        let src = one_layout("    grid cols=1 rows=1\n    widths\n    slot col=0 row=0");
        problem_at(&src, 3, 5, "widths");
        let src = one_layout("    grid cols=1 rows=1\n    widths 1\n    widths 1\n    slot col=0 row=0");
        problem_at(&src, 4, 5, "twice");
        let src = one_layout("    grid cols=1 rows=1\n    widths 1 x=2\n    slot col=0 row=0");
        problem_at(&src, 3, 14, "unknown property");
        let src = one_layout("    grid cols=1 rows=1\n    widths 1 {\n    }\n    slot col=0 row=0");
        problem_at(&src, 3, 5, "block");
    }

    #[test]
    fn a_slot_under_64_px_is_refused_at_the_slot() {
        // widths 1 10000: slot 0 is 3840 / 10001 = 0 px wide at the reference screen.
        let src = one_layout("    grid cols=2 rows=1\n    widths 1 10000\n    slot col=0 row=0\n    slot col=1 row=0");
        problem_at(
            &src,
            4,
            5,
            "slot 0 is only 0x2160 at 3840x2160 (at least 64 px each way)",
        );
    }

    #[test]
    fn a_machine_listens_or_follows_a_primary_and_has_a_name() {
        assert_eq!(
            (default_config().link, default_config().name),
            (None, None),
            "alone by default"
        );
        let c = parse(&format!(
            "{DEFAULT}\nlisten \"0.0.0.0:7340\" secret-file=\"~/.config/orrery/secret\"\nname \"desk\"\n"
        ))
        .expect("parses");
        assert_eq!(
            c.link,
            Some(LinkConfig {
                role: Role::Primary,
                addr: "0.0.0.0:7340".into(),
                secret_file: "~/.config/orrery/secret".into(),
            })
        );
        assert_eq!(c.name.as_deref(), Some("desk"));
        let c = parse(&format!(
            "{DEFAULT}\nprimary \"desk.lan:7340\" secret-file=\"/etc/orrery-secret\"\n"
        ))
        .expect("parses");
        assert_eq!(
            c.link,
            Some(LinkConfig {
                role: Role::Satellite,
                addr: "desk.lan:7340".into(),
                secret_file: "/etc/orrery-secret".into(),
            })
        );
        // A bracketed IPv6 address works for both.
        parse(&format!("{DEFAULT}\nprimary \"[::1]:7340\" secret-file=\"/s\"\n")).expect("parses");
        parse(&format!("{DEFAULT}\nlisten \"[::]:7340\" secret-file=\"/s\"\n")).expect("parses");
        // `~/` is the home directory; any other path is taken as written.
        let link = c.link.expect("a link");
        assert_eq!(
            link.secret_path(Some(Path::new("/home/me"))),
            PathBuf::from("/etc/orrery-secret")
        );
        let home = LinkConfig {
            secret_file: "~/s".into(),
            ..link
        };
        assert_eq!(
            home.secret_path(Some(Path::new("/home/me"))),
            PathBuf::from("/home/me/s")
        );
    }

    #[test]
    fn link_lines_are_strict() {
        let at_end = |extra: &str| format!("{DEFAULT}\n{extra}");
        let last = DEFAULT.lines().count() + 2;
        for (bad, word) in [
            ("listen \"0.0.0.0:7340\"", "secret-file"),
            ("listen secret-file=\"/s\"", "address"),
            ("listen \"desk:7340\" secret-file=\"/s\"", "address"),
            ("listen \"0.0.0.0\" secret-file=\"/s\"", "address"),
            ("listen \"0.0.0.0:70000\" secret-file=\"/s\"", "address"),
            ("primary \"desk\" secret-file=\"/s\"", "HOST:PORT"),
            ("primary \":7340\" secret-file=\"/s\"", "HOST:PORT"),
            ("primary \"desk:port\" secret-file=\"/s\"", "HOST:PORT"),
            ("primary \"desk:7340\" secret-file=\"s\"", "absolute"),
            ("primary \"desk:7340\" secret-file=1", "string"),
            ("primary \"desk:7340\" secret-file=\"/s\" key=\"x\"", "unknown property"),
            ("primary \"desk:7340\" secret-file=\"/s\" {\n}", "block"),
            ("name", "name"),
            ("name \"\"", "name"),
            ("name \"a\\tb\"", "name"),
            ("name \"x\" \"y\"", "name"),
        ] {
            assert_problem(&at_end(bad), last, word);
        }
        assert_problem(&at_end(&"name \"x\"\n".repeat(2)), last + 1, "twice");
        let long = format!("name \"{}\"", "n".repeat(MAX_NAME + 1));
        assert_problem(&at_end(&long), last, "name");
        assert_problem(
            &at_end("listen \"0.0.0.0:1\" secret-file=\"/s\"\nlisten \"0.0.0.0:2\" secret-file=\"/s\""),
            last + 1,
            "twice",
        );
        let both = at_end("listen \"0.0.0.0:1\" secret-file=\"/s\"\nprimary \"desk:1\" secret-file=\"/s\"");
        assert_problem(&both, last + 1, "both");
    }

    #[test]
    fn problem_columns_survive_a_mid_character_offset() {
        // Offset 1 is inside "é"; it rounds down to the character's start.
        let p = Problem::at("é\nx", 1, "m".into());
        assert_eq!((p.line, p.col), (1, 1));
    }
}
