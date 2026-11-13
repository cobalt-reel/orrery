//! The KDL config: layouts, monitors, colours and key binds, parsed strictly.
//!
//! Parsing is pure (a string in, a model out); reading the file is the caller's job.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlEntry, KdlNode};
use smithay::input::keyboard::{Keysym, XkbConfig, xkb};

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
    /// `spawn ... launch=#false`: started as `Spawn` is, but recorded as no launch (for a program that opens no
    /// window of its own, so it cannot hand its launch to another program's window). Keys only.
    SpawnNoLaunch(Vec<String>),
    Close,
    ShowTabs,
    Quit,
    /// Keyboard and pointer back to this machine from another in the session (the escape key): matched before
    /// input is forwarded, so it works wherever input is.
    Reclaim,
    /// To console 1..=CONSOLES (bare metal; where the backend has none, logged and nothing else).
    Vt(u8),
}

/// The highest console a `vt` bind can name.
pub const CONSOLES: u8 = 12;

/// A `spawn-at-start` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub argv: Vec<String>,
    /// Recorded as a launch (`launch=#true`, the default), so a window it opens lands where it was started.
    pub launch: bool,
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
    /// The slot under the pointer takes the focus as the pointer moves into it (true), or only on a click (false,
    /// the default).
    pub focus_follows_mouse: bool,
    pub colors: Colors,
    pub binds: Vec<Bind>,
    /// This machine's place in a session of linked orrerys (`listen` or `primary`); None: alone.
    pub link: Option<LinkConfig>,
    /// `name`: this machine's name in a session; None: its hostname.
    pub name: Option<String>,
    pub keyboard: Keyboard,
    /// `spawn-at-start` lines, in order: programs run once when orrery starts (never on a reload).
    pub spawn_at_start: Vec<Start>,
}

/// The `keyboard` block: the xkb names, each None for `XKB_DEFAULT_*` (xkbcommon reads them, else uses its own
/// default; the variant comes with the layout, so a variant needs a layout), and key repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyboard {
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    pub model: Option<String>,
    /// Milliseconds a key is held before it repeats.
    pub repeat_delay: u32,
    /// Repeats a second; 0: keys do not repeat.
    pub repeat_rate: u32,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            layout: None,
            variant: None,
            options: None,
            model: None,
            repeat_delay: 200,
            repeat_rate: 25,
        }
    }
}

impl Keyboard {
    /// The names as Smithay takes them: an empty name is xkbcommon's default (`XKB_DEFAULT_*` first), and so are
    /// None options; `Some("")` is no options.
    #[must_use]
    pub fn xkb(&self) -> XkbConfig<'_> {
        fn name(n: &Option<String>) -> &str {
            n.as_deref().unwrap_or("")
        }
        XkbConfig {
            rules: "",
            model: name(&self.model),
            layout: name(&self.layout),
            variant: name(&self.variant),
            options: self.options.clone(),
        }
    }

    /// The keymap these names make, as xkb text; None when xkb cannot build it. The names must hold no NUL (the
    /// parser refuses control characters): xkbcommon's binding panics on one.
    #[must_use]
    pub fn keymap_text(&self) -> Option<String> {
        let xkb = self.xkb();
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
        let keymap = xkb::Keymap::new_from_names(
            &context,
            xkb.rules,
            xkb.model,
            xkb.layout,
            xkb.variant,
            xkb.options,
            flags,
        )?;
        Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
    }

    /// Key repeat as Wayland sends it: (rate, delay).
    #[must_use]
    pub fn repeat_info(&self) -> (i32, i32) {
        let wire = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
        (wire(self.repeat_rate), wire(self.repeat_delay))
    }

    /// The same layouts (and variants): a new keymap of these stays on the layout in use.
    #[must_use]
    pub fn same_layouts(&self, other: &Keyboard) -> bool {
        (&self.layout, &self.variant) == (&other.layout, &other.variant)
    }

    /// The same keymap: the same names (the repeat aside).
    #[must_use]
    pub fn same_keymap(&self, other: &Keyboard) -> bool {
        (&self.layout, &self.variant, &self.options, &self.model)
            == (&other.layout, &other.variant, &other.options, &other.model)
    }
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

/// One problem in a config, 1-based line and column (in characters), in `file` (None: text with no file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub file: Option<PathBuf>,
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

    /// `#rrggbbaa`, as a config may write it.
    #[must_use]
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
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
pub const MAX_SOURCE: usize = 1 << 20;

/// A config file's text, None when it is longer than `limit` bytes (no more than one byte past it is read). Only a
/// regular file is read, and opening never waits: a FIFO, a terminal or a device (`/dev/stdin`, `/dev/zero`) would
/// hold up or flood the event loop, which reads configs.
pub fn read_file(path: &Path, limit: usize) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut text = String::new();
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    file.take(limit.saturating_add(1)).read_to_string(&mut text)?;
    Ok((text.len() as u64 <= limit).then_some(text))
}
/// Deepest `{` nesting allowed; the config needs 2. kdl's parser recurses per level, so an unbounded depth can
/// overflow the stack and abort the process, which no error handling can catch.
const MAX_DEPTH: usize = 16;

/// Includes nest at most this deep: the main file's own are depth 1.
pub const MAX_INCLUDE_DEPTH: usize = 8;

/// Parse a config with no file behind it (so no `include`). Every problem found is reported, each with its line
/// and column.
pub fn parse(src: &str) -> Result<Config, ConfigError> {
    let mut sources = Sources::default();
    sources.read(None, src.to_owned(), &mut Vec::new());
    sources.config()
}

/// A config file and what it includes, read: the config, or every problem (each naming its file); and every file
/// read or tried, the main one first, so a watcher can look at them all (a missing include too, to see it appear).
#[derive(Debug)]
pub struct FileConfig {
    pub config: Result<Config, ConfigError>,
    pub files: Vec<PathBuf>,
}

/// Parses `src`, the text of the config file at `path`, with the files it includes (`~/` in their paths is `home`).
pub fn parse_file(src: &str, path: &Path, home: Option<&Path>) -> FileConfig {
    let mut sources = Sources {
        home,
        files: vec![path.to_owned()],
        ..Sources::default()
    };
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    sources.read(
        Some(path.to_owned()),
        src.to_owned(),
        &mut vec![(path.to_owned(), canonical)],
    );
    let config = sources.config();
    FileConfig {
        config,
        files: sources.files,
    }
}

/// The texts of a config, gathered by following its `include`s.
#[derive(Default)]
struct Sources<'h> {
    home: Option<&'h Path>,
    /// Each text read and the file it came from, in reading order (a file included twice is read twice).
    texts: Vec<(Option<PathBuf>, String)>,
    /// Each text's document, at the same index (empty for one that does not parse).
    docs: Vec<KdlDocument>,
    /// Every top-level node but the includes, as (text, node index), in the order read: an included file's nodes
    /// stand where its `include` does.
    order: Vec<(usize, usize)>,
    /// Each with the index of its text, so they sort by file and then place.
    problems: Vec<(usize, Problem)>,
    files: Vec<PathBuf>,
    /// Bytes read, every inclusion counted: at most `MAX_SOURCE`, so includes cannot multiply without bound.
    bytes: usize,
}

impl Sources<'_> {
    /// Reads `text` (from `file`) and, depth first, what it includes. `chain` is the files being read, outermost
    /// first, each (path as resolved, canonical path), `file` last.
    fn read(&mut self, file: Option<PathBuf>, text: String, chain: &mut Vec<(PathBuf, PathBuf)>) {
        let at = self.texts.len();
        self.bytes += text.len();
        let doc = precheck(&text)
            .map_err(|p| vec![p])
            .and_then(|()| KdlDocument::parse_v2(&text).map_err(|e| syntax_problems(&text, &e)));
        let doc = doc.unwrap_or_else(|problems| {
            let problems = problems.into_iter().map(|p| Problem {
                file: file.clone(),
                ..p
            });
            self.problems.extend(problems.map(|p| (at, p)));
            KdlDocument::new()
        });
        self.texts.push((file, text));
        self.docs.push(doc);
        for ni in 0..self.docs[at].nodes().len() {
            let node = &self.docs[at].nodes()[ni];
            if node.name().value() != "include" {
                self.order.push((at, ni));
                continue;
            }
            match include_target(node) {
                Ok((target, offset)) => self.include(at, offset, &target, chain),
                Err(problems) => {
                    for (offset, message) in problems {
                        self.problem(at, offset, message);
                    }
                }
            }
        }
    }

    /// An `include "TARGET"` of text `at`, its path at `offset`.
    fn include(&mut self, at: usize, offset: usize, target: &str, chain: &mut Vec<(PathBuf, PathBuf)>) {
        let Some(including) = self.texts[at].0.clone() else {
            return self.problem(
                at,
                offset,
                "`include` needs a config file (its paths are relative to it)".into(),
            );
        };
        let path = match (target.strip_prefix("~/"), self.home) {
            (Some(rest), Some(home)) => home.join(rest),
            (Some(_), None) => {
                return self.problem(at, offset, "`~/` needs the home directory, and HOME is not set".into());
            }
            // An absolute target replaces the directory.
            (None, _) => including.parent().unwrap_or(Path::new(".")).join(target),
        };
        let canonical = match std::fs::canonicalize(&path) {
            Ok(c) => c,
            Err(e) => {
                self.problem(at, offset, format!("cannot read {}: {e}", path.display()));
                self.files.push(path);
                return;
            }
        };
        if chain.iter().any(|(_, c)| *c == canonical) {
            let names: Vec<String> = (chain.iter().map(|(p, _)| p))
                .chain([&path])
                .map(|p| p.display().to_string())
                .collect();
            return self.problem(at, offset, format!("include cycle: {}", names.join(" -> ")));
        }
        if chain.len() > MAX_INCLUDE_DEPTH {
            let message = format!("includes nest deeper than {MAX_INCLUDE_DEPTH}");
            return self.problem(at, offset, message);
        }
        self.files.push(path.clone());
        let text = match read_file(&path, MAX_SOURCE.saturating_sub(self.bytes)) {
            Ok(Some(t)) => t,
            Ok(None) => return self.problem(at, offset, "the config and its includes are larger than 1 MiB".into()),
            Err(e) => return self.problem(at, offset, format!("cannot read {}: {e}", path.display())),
        };
        chain.push((path.clone(), canonical));
        self.read(Some(path), text, chain);
        chain.pop();
    }

    fn problem(&mut self, at: usize, offset: usize, message: String) {
        let (file, text) = &self.texts[at];
        let problem = Problem {
            file: file.clone(),
            ..Problem::at(text, offset, message)
        };
        self.problems.push((at, problem));
    }

    /// The config the texts make, or their problems: those found gathering them alone when there are any (with a
    /// file missing or broken, more would mostly be its echoes), else every one the config has.
    fn config(&mut self) -> Result<Config, ConfigError> {
        if !self.problems.is_empty() {
            return Err(sorted(std::mem::take(&mut self.problems)));
        }
        let parser = Parser {
            texts: &self.texts,
            at: 0,
            problems: vec![],
        };
        parser.config(&self.docs, &self.order)
    }
}

/// Problems by file (in reading order), then place; stable, so problems at one place keep the order found.
fn sorted(mut problems: Vec<(usize, Problem)>) -> ConfigError {
    problems.sort_by_key(|(at, p)| (*at, p.line, p.col));
    ConfigError {
        problems: problems.into_iter().map(|(_, p)| p).collect(),
    }
}

/// An `include` node's path and where it is, or what is wrong with it as (offset, message).
fn include_target(node: &KdlNode) -> Result<(String, usize), Vec<(usize, String)>> {
    let mut problems = Vec::new();
    if node.children().is_some() {
        problems.push((node.span().offset(), "`include` takes no block".to_owned()));
    }
    for e in node.entries().iter() {
        if let Some(key) = e.name() {
            problems.push((
                e.span().offset(),
                format!("unknown property `{}` on `include`", key.value()),
            ));
        }
    }
    let target = match positional(node).as_slice() {
        [e] => (e.value().as_string())
            .filter(|s| !s.is_empty())
            .map(|s| (s.to_owned(), e.span().offset())),
        _ => None,
    };
    if target.is_none() {
        let msg = "`include` takes one path: relative to this file, absolute, or starting with ~/";
        problems.push((node.span().offset(), msg.to_owned()));
    }
    match target {
        Some(t) if problems.is_empty() => Ok(t),
        _ => Err(problems),
    }
}

/// kdl's syntax errors as problems in `src`.
fn syntax_problems(src: &str, e: &kdl::KdlError) -> Vec<Problem> {
    (e.diagnostics.iter())
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
        .collect()
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
        if let Some(file) = &self.file {
            write!(f, "{}:", file.display())?;
        }
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl Problem {
    fn at(src: &str, offset: usize, message: String) -> Self {
        let before = &src[..src.floor_char_boundary(offset)];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Problem {
            file: None,
            line,
            col,
            message,
        }
    }
}

/// A bind as written, before `Mod` is known (the `mod` node may come later in the file).
struct RawBind<'a> {
    node: &'a KdlNode,
    /// The text it is in.
    at: usize,
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
    /// The text it is in.
    at: usize,
    output: String,
    layout: String,
    layout_at: usize,
}

struct Parser<'a> {
    /// Each text and its file (`Sources::texts`).
    texts: &'a [(Option<PathBuf>, String)],
    /// The text whose nodes are being read: problems are placed in it.
    at: usize,
    problems: Vec<(usize, Problem)>,
}

impl<'a> Parser<'a> {
    fn err(&mut self, node: &KdlNode, message: impl Into<String>) {
        self.err_at(node.span().offset(), message);
    }

    fn err_at(&mut self, offset: usize, message: impl Into<String>) {
        let (file, src) = &self.texts[self.at];
        let problem = Problem {
            file: file.clone(),
            ..Problem::at(src, offset, message.into())
        };
        self.problems.push((self.at, problem));
    }

    /// A program to run, as written in the text being read: starting with `./` or `../`, it is taken from that
    /// file's directory, so a snippet can start the scripts beside it; anything else (or with no file) as written.
    fn program(&self, program: &str) -> String {
        let dir = self.texts[self.at].0.as_deref().and_then(Path::parent);
        match dir {
            Some(dir) if program.starts_with("./") || program.starts_with("../") => {
                // `config.kdl` alone has the parent "": the current directory, never a bare name found on PATH.
                let dir = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    dir
                };
                let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
                dir.join(program.strip_prefix("./").unwrap_or(program))
                    .to_string_lossy()
                    .into_owned()
            }
            _ => program.to_owned(),
        }
    }

    /// `[FILE:]LINE:COL` of `offset` in text `at`, as a problem there would show it.
    fn place(&self, (at, offset): (usize, usize)) -> String {
        let (file, src) = &self.texts[at];
        let p = Problem::at(src, offset, String::new());
        match file {
            Some(f) => format!("{}:{}:{}", f.display(), p.line, p.col),
            None => format!("{}:{}", p.line, p.col),
        }
    }

    /// The config from the top-level `nodes` of `docs` (`Sources::order`).
    fn config(mut self, docs: &'a [KdlDocument], nodes: &[(usize, usize)]) -> Result<Config, ConfigError> {
        for (at, doc) in docs.iter().enumerate() {
            self.at = at;
            self.no_type_annotations(doc);
        }
        let mut modifier: Option<Mods> = None;
        let mut layouts = Vec::new();
        // Every declared layout name, valid or not, so a broken layout doesn't also flag its references.
        let mut layout_names: Vec<String> = Vec::new();
        let mut monitors: Vec<RawMonitor> = Vec::new();
        let mut stacking: Option<bool> = None;
        let mut focus_follows_mouse: Option<bool> = None;
        let mut colors: Option<Colors> = None;
        let mut raw_binds: Vec<RawBind> = Vec::new();
        let mut link: Option<LinkConfig> = None;
        let mut seen_link: Option<&str> = None;
        let mut name: Option<String> = None;
        let mut seen_name = false;
        let mut keyboard: Option<Keyboard> = None;
        let mut spawn_at_start = Vec::new();

        for &(at, ni) in nodes {
            self.at = at;
            let node = &docs[at].nodes()[ni];
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
                "focus-follows-mouse" => {
                    if focus_follows_mouse.is_some() {
                        self.err(node, "`focus-follows-mouse` is set twice");
                    }
                    self.no_children(node);
                    self.props(node, &[]);
                    match positional(node).as_slice() {
                        [e] if e.value().as_bool().is_some() => focus_follows_mouse = e.value().as_bool(),
                        _ => self.err(node, "`focus-follows-mouse` takes #true or #false"),
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
                "keyboard" => {
                    if keyboard.is_some() {
                        self.err(node, "`keyboard` is set twice");
                    }
                    keyboard = Some(self.keyboard(node));
                }
                // Any number of blocks, so an included file can bring binds of its own.
                "binds" => self.binds(node, &mut raw_binds),
                "spawn-at-start" => {
                    self.no_children(node);
                    let [launch] = self.props(node, &["launch"])[..] else {
                        unreachable!("one property asked for")
                    };
                    let launch = launch.map_or(Some(true), |e| self.launch_value(e));
                    let argv: Option<Vec<String>> = (positional(node).iter())
                        .map(|e| e.value().as_string().filter(|a| !a.is_empty()).map(str::to_owned))
                        .collect();
                    match argv {
                        Some(mut argv) if !argv.is_empty() => {
                            argv[0] = self.program(&argv[0]);
                            if let Some(launch) = launch {
                                spawn_at_start.push(Start { argv, launch });
                            }
                        }
                        _ => self.err(
                            node,
                            "`spawn-at-start` takes a program and its arguments, as non-empty strings",
                        ),
                    }
                }
                other => self.err(node, format!("unknown node `{other}`")),
            }
        }

        if layout_names.is_empty() {
            self.at = 0;
            self.err_at(0, "the config defines no layout");
        }
        for m in &monitors {
            if !layout_names.contains(&m.layout) {
                self.at = m.at;
                let msg = format!("monitor {:?} uses unknown layout {:?}", m.output, m.layout);
                self.err_at(m.layout_at, msg);
            }
        }

        let modifier = modifier.unwrap_or(Mods::SUPER);
        let binds = self.resolve_binds(raw_binds, modifier, &layout_names);

        if !self.problems.is_empty() {
            return Err(sorted(self.problems));
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
            focus_follows_mouse: focus_follows_mouse.unwrap_or(false),
            colors: colors.unwrap_or_default(),
            binds,
            link,
            name,
            keyboard: keyboard.unwrap_or_default(),
            spawn_at_start,
        })
    }

    /// A `keyboard` block. A keymap xkb cannot build is refused here, so neither `check` nor a reload lets one
    /// through.
    fn keyboard(&mut self, node: &KdlNode) -> Keyboard {
        self.no_args(node, "keyboard");
        let mut keyboard = Keyboard::default();
        let mut seen: Vec<&str> = Vec::new();
        let mut names_ok = true;
        let mut variant_at = None;
        for child in children(node) {
            let key = child.name().value();
            if seen.contains(&key) {
                self.err(child, format!("`{key}` is set twice"));
            }
            seen.push(key);
            self.no_children(child);
            match key {
                "layout" | "variant" | "options" | "model" => {
                    let msg = format!("`{key}` takes one string of xkb names");
                    let Some((text, at)) = self.only_string(child, &msg) else {
                        names_ok = false;
                        continue;
                    };
                    if text.chars().any(char::is_control) {
                        self.err_at(at, format!("`{key}` must not hold control characters"));
                        names_ok = false;
                        continue;
                    }
                    // An empty variant or options is "none"; an empty layout or model would only be the default.
                    if text.is_empty() && matches!(key, "layout" | "model") {
                        self.err_at(at, format!("`{key}` is empty: leave it out for the default"));
                        names_ok = false;
                        continue;
                    }
                    if key == "variant" {
                        variant_at = Some(child.span().offset());
                    }
                    *match key {
                        "layout" => &mut keyboard.layout,
                        "variant" => &mut keyboard.variant,
                        "options" => &mut keyboard.options,
                        _ => &mut keyboard.model,
                    } = Some(text);
                }
                "repeat-delay" => {
                    let msg = "`repeat-delay` takes milliseconds, a whole number from 1 to 10000";
                    if let Some(n) = self.only_number(child, 1..=10_000, msg) {
                        keyboard.repeat_delay = n;
                    }
                }
                "repeat-rate" => {
                    let msg = "`repeat-rate` takes repeats a second, a whole number from 0 to 1000 (0: none)";
                    if let Some(n) = self.only_number(child, 0..=1000, msg) {
                        keyboard.repeat_rate = n;
                    }
                }
                other => self.err(child, format!("unknown keyboard setting `{other}`")),
            }
        }
        // xkb takes a variant only with the layout it is for: without one, both come from the environment.
        if let Some(at) = variant_at.filter(|_| keyboard.layout.is_none()) {
            self.err_at(at, "`variant` needs `layout` (it is a variant of that layout)");
            names_ok = false;
        }
        if names_ok && keyboard.keymap_text().is_none() {
            self.err(
                node,
                "xkb cannot build a keymap from these names (an unknown layout, variant or model?)",
            );
        }
        keyboard
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
            at: self.at,
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

    /// Expand `Mod`, check layout targets and refuse duplicates (same mods and keysym) that disagree.
    fn resolve_binds(&mut self, raw_binds: Vec<RawBind>, modifier: Mods, layout_names: &[String]) -> Vec<Bind> {
        let mut binds: Vec<Bind> = Vec::new();
        // Where each bind in `binds` was written: (text, offset).
        let mut written_at: Vec<(usize, usize)> = Vec::new();
        for raw in raw_binds {
            self.at = raw.at;
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
            if let Some(i) = binds.iter().position(|b| b.mods == mods && b.keysym == raw.keysym) {
                // The same action twice is one bind: with `mod "Alt"`, `Mod+Ctrl+F1` and `Ctrl+Alt+F1` are.
                if binds[i].action == raw.action {
                    continue;
                }
                let written = raw.node.name().value();
                let other = self.place(written_at[i]);
                self.err(raw.node, format!("{written:?} is bound twice (also at {other})"));
                continue;
            }
            written_at.push((raw.at, raw.node.span().offset()));
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
        // `launch=#true|#false`, `spawn`'s only property; other actions take none.
        let mut launch: Option<(bool, usize)> = None;
        for e in node.entries().iter().filter(|e| e.name().is_some()) {
            let at = e.span().offset();
            if e.name().is_some_and(|n| n.value() != "launch") {
                self.err_at(at, "binds take no properties but `launch` (on `spawn`)");
                return None;
            }
            if launch.is_some() {
                self.err_at(at, "`launch` is set twice");
                return None;
            }
            launch = Some((self.launch_value(e)?, at));
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
        let action = match (self.action(node), launch) {
            (Some(Action::Spawn(argv)), Some((false, _))) => Some(Action::SpawnNoLaunch(argv)),
            (Some(Action::Spawn(argv)), _) => Some(Action::Spawn(argv)),
            (Some(_), Some((_, at))) => {
                self.err_at(at, "`launch` is only for `spawn`");
                None
            }
            (action, _) => action,
        };
        let target_at = positional(node)
            .get(1)
            .map_or(node.span().offset(), |e| e.span().offset());
        Some(RawBind {
            node,
            at: self.at,
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
        if verb == "vt" {
            let n = match rest {
                [e] => (e.value().as_integer()).and_then(|n| u8::try_from(n).ok()),
                _ => None,
            };
            let Some(n) = n.filter(|n| (1..=CONSOLES).contains(n)) else {
                self.err_at(arg_at, format!("`vt` takes a console number from 1 to {CONSOLES}"));
                return None;
            };
            return Some(Action::Vt(n));
        }
        if let "workspace" | "move-to-workspace" = verb {
            // A number, not a string, as `vt`'s.
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
            ("spawn", Some([program, args @ ..])) if !program.is_empty() && args.iter().all(|a| !a.is_empty()) => {
                let program = self.program(program);
                Action::Spawn(
                    std::iter::once(program)
                        .chain(args.iter().map(|s| (*s).to_owned()))
                        .collect(),
                )
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
            ("reclaim", Some([])) => Action::Reclaim,
            ("split" | "toggle-stacking" | "close" | "show-tabs" | "quit" | "reclaim", _) => {
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

    /// Exactly one positional whole number in `range` and no properties; else `msg` is the problem.
    fn only_number(&mut self, node: &KdlNode, range: std::ops::RangeInclusive<u32>, msg: &str) -> Option<u32> {
        self.props(node, &[]);
        let n = match positional(node).as_slice() {
            [e] => (e.value().as_integer()).and_then(|n| u32::try_from(n).ok()),
            _ => None,
        };
        let n = n.filter(|n| range.contains(n));
        if n.is_none() {
            self.err(node, msg);
        }
        n
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

    /// A `launch` property's value, or None with the problem reported.
    fn launch_value(&mut self, e: &KdlEntry) -> Option<bool> {
        let value = e.value().as_bool();
        if value.is_none() {
            self.err_at(e.span().offset(), "`launch` takes #true or #false");
        }
        value
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
    const CTRL_ALT: Mods = Mods {
        ctrl: true,
        alt: true,
        shift: false,
        logo: false,
    };
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
            focus_follows_mouse: false,
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
                bind(MOD, "Escape", k::KEY_Escape, Action::Reclaim),
            ])
            .chain(f_key_binds(MOD_CTRL))
            .chain(f_key_binds(CTRL_ALT))
            .collect(),
            link: None,
            name: None,
            keyboard: Keyboard::default(),
            spawn_at_start: vec![],
        }
    }

    /// `mods` with F1..F12, for consoles 1..12.
    fn f_key_binds(mods: Mods) -> Vec<Bind> {
        (1..=12u8)
            .map(|n| bind(mods, &format!("F{n}"), k::KEY_F1 + u32::from(n) - 1, Action::Vt(n)))
            .collect()
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
    /// The `spawn-at-start` argvs.
    fn argvs(starts: &[Start]) -> Vec<Vec<String>> {
        starts.iter().map(|s| s.argv.clone()).collect()
    }

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
    fn focus_follows_mouse_is_read_and_off_by_default() {
        let n = line_of("stacking #true");
        assert!(
            !parse(&with_line(n, "stacking #true"))
                .expect("parses")
                .focus_follows_mouse
        );
        let on = parse(&with_line(n, "stacking #true\nfocus-follows-mouse #true")).expect("parses");
        assert!(on.focus_follows_mouse);
        for bad in [
            "focus-follows-mouse",
            "focus-follows-mouse 1",
            "focus-follows-mouse #true #false",
        ] {
            assert_problem(
                &with_line(n, &format!("stacking #true\n{bad}")),
                n + 1,
                "focus-follows-mouse",
            );
        }
        let twice = "stacking #true\nfocus-follows-mouse #true\nfocus-follows-mouse #false";
        assert_problem(&with_line(n, twice), n + 2, "twice");
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
        let n = line_of("\"Mod+Escape\"");
        assert_problem(&with_line(n, "    \"Mod+Escape\" reclaim \"side\""), n, "no arguments");
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
    fn a_combination_bound_twice_to_the_same_action_is_one_bind() {
        // With Mod as Alt, the example's two console binds are the same keys.
        let c = parse(&DEFAULT.replace("mod \"Super\"", "mod \"Alt\"")).expect("parses");
        let f1: Vec<&Bind> = (c.binds.iter())
            .filter(|b| b.mods == CTRL_ALT && b.key == "F1")
            .collect();
        assert_eq!(f1.len(), 1);
        assert_eq!(f1[0].action, Action::Vt(1));
        let n = line_of("\"Mod+Shift+Q\"");
        let twice = with_line(n, "    \"Mod+Shift+Q\" close\n    \"Super+Shift+q\" close");
        assert_eq!(parse(&twice).map(|c| c.binds.len()), Ok(expected_default().binds.len()));
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
            file: None,
            line: 5,
            col: 22,
            message: "boom".into(),
        };
        assert_eq!(p.to_string(), "5:22: boom");
        let e = ConfigError {
            problems: vec![p.clone(), p.clone()],
        };
        assert_eq!(e.to_string(), "5:22: boom\n5:22: boom");
        // From a file: prefixed with it, as editors jump to.
        let p = Problem {
            file: Some(PathBuf::from("/c/inc.kdl")),
            ..p
        };
        assert_eq!(p.to_string(), "/c/inc.kdl:5:22: boom");
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
    fn the_keyboard_block_sets_xkb_names_and_repeat() {
        // Left out: every name from the environment (or xkb's own default), the repeat as before.
        let default = Keyboard {
            layout: None,
            variant: None,
            options: None,
            model: None,
            repeat_delay: 200,
            repeat_rate: 25,
        };
        assert_eq!(default_config().keyboard, default);
        assert_eq!(Keyboard::default(), default);
        let c = parse(&format!(
            "{DEFAULT}\nkeyboard {{\n    layout \"us,de\"\n    variant \",nodeadkeys\"\n    \
             options \"grp:alt_shift_toggle\"\n    model \"pc105\"\n    repeat-delay 600\n    repeat-rate 0\n}}\n"
        ))
        .expect("parses");
        assert_eq!(
            c.keyboard,
            Keyboard {
                layout: Some("us,de".into()),
                variant: Some(",nodeadkeys".into()),
                options: Some("grp:alt_shift_toggle".into()),
                model: Some("pc105".into()),
                repeat_delay: 600,
                repeat_rate: 0,
            }
        );
        // Any subset; empty options and variant mean none (not the environment's).
        let c = parse(&format!(
            "{DEFAULT}\nkeyboard {{\n    options \"\"\n    repeat-rate 40\n}}\n"
        ))
        .expect("parses");
        assert_eq!(
            c.keyboard,
            Keyboard {
                options: Some(String::new()),
                repeat_rate: 40,
                ..default.clone()
            }
        );
        let xkb = c.keyboard.xkb();
        assert_eq!((xkb.rules, xkb.model, xkb.layout, xkb.variant), ("", "", "", ""));
        assert_eq!(xkb.options, Some(String::new()));
        assert_eq!(default.xkb().options, None, "None: the environment's options");
    }

    #[test]
    fn keyboard_lines_are_strict() {
        let at_end = |extra: &str| format!("{DEFAULT}\n{extra}");
        let last = DEFAULT.lines().count() + 2;
        assert_problem(&at_end("keyboard \"de\" {\n}"), last, "no arguments");
        assert_problem(&at_end("keyboard {\n}\nkeyboard {\n}"), last + 2, "twice");
        for (bad, word) in [
            ("layout", "layout"),
            ("layout 1", "layout"),
            ("layout \"de\" \"us\"", "layout"),
            ("layout \"\"", "layout"),
            ("model \"\"", "model"),
            ("variant \"a\\u{0}b\"", "control"),
            ("options \"ctrl:nocaps\" x=1", "unknown property"),
            ("layout \"de\" {\n}", "block"),
            // xkb takes the variant with the layout: without one, both would come from the environment.
            ("variant \"nodeadkeys\"", "`variant` needs `layout`"),
            ("variant \"\"", "`variant` needs `layout`"),
            ("repeat-delay 0", "1 to 10000"),
            ("repeat-delay 10001", "1 to 10000"),
            ("repeat-delay \"600\"", "1 to 10000"),
            ("repeat-rate -1", "0 to 1000"),
            ("repeat-rate 1001", "0 to 1000"),
            ("repeat-rate 2.5", "0 to 1000"),
            ("speed 3", "unknown"),
        ] {
            assert_problem(&at_end(&format!("keyboard {{\n{bad}\n}}")), last + 1, word);
        }
        assert_problem(
            &at_end("keyboard {\nlayout \"de\"\nlayout \"us\"\n}"),
            last + 2,
            "twice",
        );
        // A keymap xkb cannot build is refused here, so neither `check` nor a reload lets it through.
        assert_problem(&at_end("keyboard {\nlayout \"no-such-layout\"\n}"), last, "keymap");
        assert_problem(
            &at_end("keyboard {\nlayout \"us\"\nvariant \"no-such-variant\"\n}"),
            last,
            "keymap",
        );
    }

    #[test]
    fn vt_binds_take_a_console_from_1_to_12() {
        let n = line_of("\"Mod+Ctrl+F3\"");
        for bad in ["0", "13", "-1", "\"3\"", "1.5", "", "1 2"] {
            assert_problem(&with_line(n, &format!("    \"Mod+Ctrl+F3\" vt {bad}")), n, "1 to 12");
        }
        let c = parse(&with_line(n, "    \"Mod+Ctrl+F3\" vt 12")).expect("in range");
        let b = (c.binds.iter())
            .find(|b| b.key == "F3" && b.mods == MOD_CTRL)
            .expect("the bind");
        assert_eq!(b.action, Action::Vt(12));
    }

    /// A one-layout config, enough to parse on its own.
    const ONE: &str = "layout \"a\" {\n grid cols=1 rows=1\n slot col=0 row=0\n}\n";

    /// Writes `files` (relative path, text) under a new temp dir, returned.
    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// `parse_file` on `<dir>/main.kdl`, with `home` for `~/`.
    fn parse_main(dir: &Path, home: Option<&Path>) -> FileConfig {
        let main = dir.join("main.kdl");
        parse_file(&std::fs::read_to_string(&main).unwrap(), &main, home)
    }

    #[test]
    fn includes_are_read_relative_to_the_including_file() {
        let dir = tree(&[
            (
                "main.kdl",
                &format!("{ONE}include \"kitty/desk.kdl\"\nstacking #false\n"),
            ),
            (
                "kitty/desk.kdl",
                "binds {\n \"Mod+D\" spawn \"menu\"\n}\ninclude \"../colors.kdl\"\n",
            ),
            ("colors.kdl", "colors {\n focus \"#01020304\"\n}\n"),
        ]);
        let read = parse_main(dir.path(), None);
        let c = read.config.expect("parses");
        assert_eq!(c.binds[0].action, Action::Spawn(vec!["menu".into()]));
        assert_eq!(c.colors.focus, Color { r: 1, g: 2, b: 3, a: 4 });
        assert!(!c.stacking, "the main file's own nodes after the include count too");
        let d = dir.path();
        assert_eq!(
            read.files,
            [
                d.join("main.kdl"),
                d.join("kitty/desk.kdl"),
                d.join("kitty/../colors.kdl")
            ]
        );
    }

    #[test]
    fn includes_take_absolute_and_home_paths() {
        let home = tree(&[("snippets/binds.kdl", "binds {\n \"Mod+D\" quit\n}\n")]);
        let other = tree(&[("colors.kdl", "colors {\n badge \"#010203\"\n}\n")]);
        let main = format!(
            "{ONE}include \"~/snippets/binds.kdl\"\ninclude \"{}\"\n",
            other.path().join("colors.kdl").display()
        );
        let dir = tree(&[("main.kdl", &main)]);
        let c = parse_main(dir.path(), Some(home.path())).config.expect("parses");
        assert_eq!(c.binds[0].action, Action::Quit);
        assert_eq!(c.colors.badge, Color::rgb(1, 2, 3));
        // Without a home directory, `~/` is a problem at the path.
        let err = parse_main(dir.path(), None).config.unwrap_err();
        assert_eq!((err.problems[0].line, err.problems[0].col), (5, 9), "{err}");
        assert!(err.problems[0].message.contains("HOME"), "{err}");
    }

    #[test]
    fn include_cycles_are_refused_with_the_chain() {
        let dir = tree(&[
            ("main.kdl", &format!("{ONE}include \"a.kdl\"\n")),
            ("a.kdl", "include \"b.kdl\"\n"),
            ("b.kdl", "// b\ninclude \"a.kdl\"\n"),
        ]);
        let read = parse_main(dir.path(), None);
        let err = read.config.unwrap_err();
        assert_eq!(err.problems.len(), 1, "{err}");
        let p = &err.problems[0];
        assert_eq!(p.file.as_deref(), Some(dir.path().join("b.kdl").as_path()));
        assert_eq!((p.line, p.col), (2, 9), "{err}");
        let [main, a, b] = ["main.kdl", "a.kdl", "b.kdl"].map(|f| dir.path().join(f).display().to_string());
        assert!(p.message.contains(&format!("{main} -> {a} -> {b} -> {a}")), "{err}");
        // A file included twice side by side is no cycle (its layouts then clash as any duplicate would).
        let dir = tree(&[
            ("main.kdl", &format!("{ONE}include \"c.kdl\"\ninclude \"c.kdl\"\n")),
            ("c.kdl", "spawn-at-start \"bar\"\n"),
        ]);
        let c = parse_main(dir.path(), None).config.expect("parses");
        assert_eq!(
            argvs(&c.spawn_at_start),
            [vec!["bar".to_owned()], vec!["bar".to_owned()]]
        );
        // Nor is a file including itself through another name of it: the canonical path is compared.
        let dir = tree(&[("main.kdl", &format!("{ONE}include \"./main.kdl\"\n"))]);
        let err = parse_main(dir.path(), None).config.unwrap_err();
        assert!(err.problems[0].message.contains("cycle"), "{err}");
    }

    #[test]
    fn includes_nest_at_most_8_deep() {
        let chain = |n: usize| {
            let mut files: Vec<(String, String)> = vec![("main.kdl".into(), format!("{ONE}include \"1.kdl\"\n"))];
            for i in 1..n {
                files.push((format!("{i}.kdl"), format!("include \"{}.kdl\"\n", i + 1)));
            }
            files.push((format!("{n}.kdl"), "stacking #false\n".into()));
            let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
            tree(&refs)
        };
        let deep = chain(MAX_INCLUDE_DEPTH);
        assert!(!parse_main(deep.path(), None).config.expect("8 deep is fine").stacking);
        let deeper = chain(MAX_INCLUDE_DEPTH + 1);
        let read = parse_main(deeper.path(), None);
        let err = read.config.unwrap_err();
        assert_eq!(err.problems.len(), 1, "{err}");
        assert!(err.problems[0].message.contains("deeper than 8"), "{err}");
        assert_eq!(
            err.problems[0].file.as_deref(),
            Some(deeper.path().join("8.kdl").as_path())
        );
        assert_eq!(read.files.len(), MAX_INCLUDE_DEPTH + 1, "the 9th is never read");
    }

    #[test]
    fn problems_in_an_included_file_name_it() {
        let dir = tree(&[
            ("main.kdl", &format!("{ONE}colors {{\n}}\ninclude \"inc.kdl\"\n")),
            ("inc.kdl", "stacking #true\nbogus 1\ncolors {\n}\n"),
        ]);
        let err = parse_main(dir.path(), None).config.unwrap_err();
        let inc = dir.path().join("inc.kdl");
        let said: Vec<String> = err.problems.iter().map(Problem::to_string).collect();
        assert_eq!(said.len(), 2, "{err}");
        assert!(
            said[0].starts_with(&format!("{}:2:1: unknown node", inc.display())),
            "{err}"
        );
        assert!(
            said[1].starts_with(&format!("{}:3:1: `colors` is set twice", inc.display())),
            "{err}"
        );
        // A missing file is a problem at its include, and is still watched for.
        let dir = tree(&[("main.kdl", &format!("{ONE}include \"gone.kdl\"\n"))]);
        let read = parse_main(dir.path(), None);
        let err = read.config.unwrap_err();
        let main = dir.path().join("main.kdl");
        assert!(
            err.to_string()
                .starts_with(&format!("{}:5:9: cannot read", main.display())),
            "{err}"
        );
        assert!(err.to_string().contains("gone.kdl"), "{err}");
        assert_eq!(read.files, [main, dir.path().join("gone.kdl")]);
        // A syntax error in an included file names it too.
        let dir = tree(&[
            ("main.kdl", &format!("{ONE}include \"inc.kdl\"\n")),
            ("inc.kdl", "colors {\n"),
        ]);
        let err = parse_main(dir.path(), None).config.unwrap_err();
        assert_eq!(
            err.problems[0].file.as_deref(),
            Some(dir.path().join("inc.kdl").as_path()),
            "{err}"
        );
    }

    #[test]
    fn include_is_strict() {
        let dir = tree(&[("x.kdl", "")]);
        let main = dir.path().join("main.kdl");
        for (line, word) in [
            ("include", "one path"),
            ("include \"x.kdl\" \"y.kdl\"", "one path"),
            ("include 1", "one path"),
            ("include \"x.kdl\" optional=#true", "unknown property"),
            ("include \"x.kdl\" {\n}", "no block"),
            ("include \"\"", "one path"),
        ] {
            let src = format!("{ONE}{line}\n");
            let err = parse_file(&src, &main, None).config.expect_err(line);
            assert!(
                err.problems.iter().any(|p| p.line == 5 && p.message.contains(word)),
                "{line}: {err}"
            );
        }
        // Text with no file behind it has nothing to be relative to.
        assert_problem(&format!("{ONE}include \"/etc/x.kdl\"\n"), 5, "config file");
    }

    #[test]
    fn included_files_count_towards_the_size_limit() {
        let big = format!("// {}\n", "x".repeat(MAX_SOURCE / 2));
        let dir = tree(&[
            ("main.kdl", &format!("{ONE}include \"big.kdl\"\ninclude \"big.kdl\"\n")),
            ("big.kdl", &big),
        ]);
        let err = parse_main(dir.path(), None).config.unwrap_err();
        assert_eq!(err.problems.len(), 1, "{err}");
        assert!(err.problems[0].message.contains("1 MiB"), "{err}");
        assert_eq!(err.problems[0].line, 6, "at the include that went over: {err}");
    }

    #[test]
    fn spawn_at_start_takes_argvs() {
        let c = parse(&format!(
            "{ONE}spawn-at-start \"bar\"\nspawn-at-start \"kitty\" \"--title\" \"x y\"\n"
        ))
        .expect("parses");
        assert_eq!(
            argvs(&c.spawn_at_start),
            [
                vec!["bar".to_owned()],
                vec!["kitty".into(), "--title".into(), "x y".into()]
            ]
        );
        assert!(parse(ONE).unwrap().spawn_at_start.is_empty());
        for (line, word) in [
            ("spawn-at-start", "program"),
            ("spawn-at-start \"\"", "program"),
            ("spawn-at-start \"bar\" \"\"", "program"),
            ("spawn-at-start \"bar\" 1", "program"),
            ("spawn-at-start \"bar\" at=1", "unknown property"),
            ("spawn-at-start \"bar\" {\n}", "no block"),
        ] {
            assert_problem(&format!("{ONE}{line}\n"), 5, word);
        }
    }

    #[test]
    fn spawns_can_record_no_launch() {
        let c = parse(&format!(
            "{ONE}spawn-at-start \"a\" launch=#false\nspawn-at-start \"b\" launch=#true\nspawn-at-start \"c\"\nbinds {{\n \"Mod+D\" spawn \"menu\" \"-x\" launch=#false\n \"Mod+E\" spawn \"e\" launch=#true\n}}\n"
        ))
        .expect("parses");
        let launches: Vec<(&str, bool)> = (c.spawn_at_start.iter())
            .map(|s| (s.argv[0].as_str(), s.launch))
            .collect();
        assert_eq!(launches, [("a", false), ("b", true), ("c", true)]);
        assert_eq!(
            c.binds[0].action,
            Action::SpawnNoLaunch(vec!["menu".into(), "-x".into()])
        );
        assert_eq!(c.binds[1].action, Action::Spawn(vec!["e".into()]));
        let n = line_of("\"Mod+Return\"");
        for (line, word) in [
            ("    \"Mod+D\" spawn \"x\" launch=1", "#true or #false"),
            ("    \"Mod+D\" spawn \"x\" launch=#false launch=#true", "set twice"),
            ("    \"Mod+D\" close launch=#false", "only for `spawn`"),
            ("    \"Mod+D\" spawn \"x\" other=#false", "no properties but"),
        ] {
            assert_problem(&with_line(n, line), n, word);
        }
        assert_problem(
            &format!("{ONE}spawn-at-start \"a\" launch=\"no\"\n"),
            5,
            "#true or #false",
        );
    }

    #[test]
    fn programs_starting_with_a_dot_are_relative_to_their_file() {
        let dir = tree(&[
            (
                "main.kdl",
                &format!("{ONE}include \"kitty/desk.kdl\"\nspawn-at-start \"./mine\" \"./arg\"\n"),
            ),
            (
                "kitty/desk.kdl",
                "spawn-at-start \"./bars\" \"x\"\nspawn-at-start \"bar\"\nbinds {\n \"Mod+D\" spawn \"../menu\" \"./y\"\n}\n",
            ),
        ]);
        let c = parse_main(dir.path(), None).config.expect("parses");
        let (d, k) = (dir.path().display(), dir.path().join("kitty"));
        let k = k.display();
        assert_eq!(
            argvs(&c.spawn_at_start),
            [
                vec![format!("{k}/bars"), "x".into()],
                vec!["bar".to_owned()],
                vec![format!("{d}/mine"), "./arg".into()],
            ],
            "only the program, and only `./` or `../`"
        );
        assert_eq!(
            c.binds[0].action,
            Action::Spawn(vec![format!("{k}/../menu"), "./y".into()])
        );
        // A config with no file behind it has nothing to be relative to: as written.
        let c = parse(&format!("{ONE}spawn-at-start \"./bars\"\n")).expect("parses");
        assert_eq!(argvs(&c.spawn_at_start), [vec!["./bars".to_owned()]]);
        // A config named without a directory is in the current one: never a bare name looked up on PATH.
        let src = format!("{ONE}spawn-at-start \"./bars\"\n");
        let c = parse_file(&src, Path::new("main.kdl"), None).config.expect("parses");
        let here = std::env::current_dir().unwrap().join("bars");
        assert_eq!(argvs(&c.spawn_at_start), [vec![here.display().to_string()]]);
    }

    #[test]
    fn binds_blocks_add_up() {
        let src = format!("{ONE}binds {{\n \"Mod+Q\" quit\n}}\nbinds {{\n \"Mod+W\" close\n \"Mod+Q\" quit\n}}\n");
        let c = parse(&src).expect("parses");
        let actions: Vec<&Action> = c.binds.iter().map(|b| &b.action).collect();
        assert_eq!(actions, [&Action::Quit, &Action::Close], "the same bind twice is one");
        let clash = format!("{ONE}binds {{\n \"Mod+Q\" quit\n}}\nbinds {{\n \"Mod+Q\" close\n}}\n");
        assert_problem(&clash, 9, "bound twice (also at 6:2)");
        // Across files, the other place names its file: a snippet's bind is not overridden, it clashes.
        let dir = tree(&[
            (
                "main.kdl",
                &format!("{ONE}include \"keys.kdl\"\nbinds {{\n \"Mod+Q\" close\n}}\n"),
            ),
            ("keys.kdl", "binds {\n \"Mod+Q\" quit\n}\n"),
        ]);
        let err = parse_main(dir.path(), None).config.unwrap_err();
        let (main, keys) = (dir.path().join("main.kdl"), dir.path().join("keys.kdl"));
        let want = format!(
            "{}:7:2: \"Mod+Q\" is bound twice (also at {}:2:2)",
            main.display(),
            keys.display()
        );
        assert_eq!(err.to_string(), want);
    }

    #[test]
    fn only_regular_files_are_read_and_never_past_the_limit() {
        let dir = tree(&[("sub/x.kdl", "")]);
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // Safety: a valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // A FIFO nobody writes to would block the reader for ever; a device may never end.
        for target in [fifo.display().to_string(), "/dev/zero".into(), "sub".into()] {
            std::fs::write(dir.path().join("main.kdl"), format!("{ONE}include \"{target}\"\n")).unwrap();
            let err = parse_main(dir.path(), None).config.unwrap_err();
            assert!(err.to_string().contains("not a regular file"), "{target}: {err}");
        }
        assert!(
            read_file(&fifo, 10)
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
        let ten = dir.path().join("ten");
        std::fs::write(&ten, "0123456789").unwrap();
        assert_eq!(read_file(&ten, 10).unwrap().as_deref(), Some("0123456789"));
        assert_eq!(read_file(&ten, 9).unwrap(), None, "longer than the limit");
        assert_eq!(
            read_file(&dir.path().join("none"), 9).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn problem_columns_survive_a_mid_character_offset() {
        // Offset 1 is inside "é"; it rounds down to the character's start.
        let p = Problem::at("é\nx", 1, "m".into());
        assert_eq!((p.line, p.col), (1, 1));
    }
}
