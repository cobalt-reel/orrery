//! The link between the primary and a satellite: one TCP connection, opened by the satellite (the Noise initiator),
//! carrying `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` keyed by a pre-shared secret file. Every frame is a u32
//! big-endian length and that many bytes: first the two handshake messages, then each `Message` as JSON in one Noise
//! transport message.
//!
//! No sockets here: a `Link` is fed the bytes read from its connection and hands out the messages decoded and the
//! bytes to write, so the compositor drives it from calloop and tests from memory. Nothing is ever sent in clear:
//! messages queued before the handshake ends wait for it, and are dropped if it fails. Any failure (a handshake
//! that fails, a frame that does not decrypt, is too big or does not decode, a message out of place, a reader that
//! stopped reading) closes the link with a reason.
//!
//! Trust: NN gives neither end an identity. The secret is the only authentication, and holding it is a shell on
//! every machine in the session (a `Command` can spawn programs). Generate it from /dev/urandom, never from a
//! passphrase, and guard it like a private key.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use snow::params::{HashChoice, NoiseParams};
use snow::resolvers::{CryptoResolver as _, DefaultResolver};
use snow::{HandshakeState, TransportState};
use zeroize::{Zeroize as _, Zeroizing};

use crate::NAME;
use crate::crossing::{Edge, Entry, RawKey};
use crate::ipc::{Request, Response};
use crate::session::{Renames, Role, Screen};

/// The link protocol's version: both ends must speak the same one (the primary refuses a satellite's other one).
pub const PROTOCOL_VERSION: u32 = 3;
pub const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// The largest frame, its length prefix aside: Noise's largest message.
pub const MAX_FRAME: usize = 65535;
/// The largest message's JSON: a frame less the cipher's tag.
pub const MAX_MESSAGE: usize = MAX_FRAME - TAG;
/// Bytes queued and not yet written, at most: one more message fails the link (`Closed::Stalled`).
pub const MAX_OUTPUT: usize = 1 << 20;
/// What the other end sent, quoted in a close reason, is cut to this many bytes.
pub const MAX_REASON: usize = 200;
/// A link not open this long after its connection was made is closed (`Link::expire`): the driver keeps the time.
/// It covers the handshake and, on the primary, the Hello.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// A secret file holds this many bytes or more (whitespace at either end aside), and at most `SECRET_MAX` in all.
pub const SECRET_MIN: usize = 32;
pub const SECRET_MAX: usize = 4096;
/// ChaChaPoly's authentication tag, on every Noise message.
const TAG: usize = 16;
/// Mixed into the key and the handshake, so neither matches another protocol's on the same secret. Part of the
/// protocol, so not `NAME`.
const LABEL: &[u8] = b"orrery link";

/// The pre-shared secret, as Noise's 32-byte PSK: BLAKE2s over `LABEL` and the secret file's contents, trimmed.
///
/// A secret file is any 32 to 4096 bytes (binary or text; whitespace at either end is not part of it, so a trailing
/// newline does not matter), the same on both machines, with no permission for group or others: e.g. `(umask 077;
/// head -c 32 /dev/urandom | base64 > secret)`. Its contents are never logged or shown, nor is the key, and both
/// are zeroed when dropped.
pub struct Secret([u8; 32]);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Whether two secrets are the same key (a reload compares them); in time independent of where they differ.
impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0.iter().zip(&other.0).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
    }
}

impl Eq for Secret {}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Why a secret file was refused; each names the file, never what is in it.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("cannot read the secret file {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("the secret file {} is not a regular file", path.display())]
    NotAFile { path: PathBuf },
    #[error("the secret file {} is open to group or others (mode {mode:o}): chmod 600 it", path.display())]
    Open { path: PathBuf, mode: u32 },
    /// `len`: whitespace at either end aside.
    #[error("the secret file {} holds a {len}-byte secret; it needs at least {SECRET_MIN}", path.display())]
    TooShort { path: PathBuf, len: usize },
    #[error("the secret file {} is over {SECRET_MAX} bytes", path.display())]
    TooLong { path: PathBuf },
}

impl Secret {
    /// Reads the secret file at `path` (through a symlink: the file's own mode counts); refused if group or others
    /// have any permission on it, if it is not a regular file, or for its size.
    pub fn load(path: &Path) -> Result<Self, SecretError> {
        let read = |source| SecretError::Read {
            path: path.to_owned(),
            source,
        };
        // Non-blocking: a FIFO opens at once (and is refused below) instead of waiting for a writer.
        let file = (File::options().read(true))
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .map_err(read)?;
        // The opened file's own metadata: what is read below is what was checked.
        let meta = file.metadata().map_err(read)?;
        if !meta.is_file() {
            return Err(SecretError::NotAFile { path: path.to_owned() });
        }
        if meta.mode() & 0o077 != 0 {
            return Err(SecretError::Open {
                path: path.to_owned(),
                mode: meta.mode() & 0o7777,
            });
        }
        // Room for the most read, so the buffer never moves and leaves an unzeroed copy behind.
        let mut contents = Zeroizing::new(Vec::with_capacity(SECRET_MAX + 1));
        // One byte over the most allowed tells a file too long, however long it is.
        (file.take(SECRET_MAX as u64 + 1))
            .read_to_end(&mut contents)
            .map_err(read)?;
        if contents.len() > SECRET_MAX {
            return Err(SecretError::TooLong { path: path.to_owned() });
        }
        Self::from_contents(&contents).ok_or_else(|| SecretError::TooShort {
            path: path.to_owned(),
            len: contents.trim_ascii().len(),
        })
    }

    /// The secret in `contents`; None when too short or too long.
    fn from_contents(contents: &[u8]) -> Option<Self> {
        let secret = contents.trim_ascii();
        if contents.len() > SECRET_MAX || secret.len() < SECRET_MIN {
            return None;
        }
        let mut hash = (DefaultResolver.resolve_hash(&HashChoice::Blake2s)).expect("built with BLAKE2s");
        hash.input(LABEL);
        hash.input(secret);
        let mut key = Self([0; 32]);
        hash.result(&mut key.0);
        Some(key)
    }
}

/// What crosses the link, each in one frame. Hello first (satellite), then Welcome or Refused (primary); then
/// either end sends its others as things happen. A message to the wrong end, or a first one other than these,
/// closes the link (`Closed::Protocol`).
///
/// Input (milestone 5) goes from the primary to the satellite holding the session's focus marker: a Keymap when it
/// changed, a Handoff, then Key, Motion, Button and Axis, until a Release. Keycodes never show in Debug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "kebab-case")]
pub enum Message {
    /// Satellite, first and once: its version (`PROTOCOL_VERSION`), name, screens and workspace numbers, as for
    /// `Session::join`.
    Hello {
        version: u32,
        name: String,
        screens: Vec<Screen>,
        workspaces: BTreeSet<u8>,
    },
    /// Primary, first and once: joined. The renames and foreign numbers for the satellite's `Stacks::apply_session`.
    Welcome {
        renames: Renames,
        foreign: BTreeSet<u8>,
    },
    /// Either end, then it closes: the other end is not taken (a version mismatch, a duplicate name). Never handed
    /// out by `Link::recv`: it closes the link with `Closed::Refused`. Its shape never changes across versions.
    Refused {
        reason: String,
    },
    /// Satellite: what it has now, after a change, as for `Session::report`.
    WorkspaceState {
        screens: Vec<Screen>,
        workspaces: BTreeSet<u8>,
    },
    /// Primary: the answer to a report (or the session changed): renames and foreign numbers to apply.
    Renames {
        renames: Renames,
        foreign: BTreeSet<u8>,
    },
    /// Primary: run `request` there (`workspace N` routed there, `orrery msg --to`).
    Command {
        id: u64,
        request: Request,
    },
    /// Satellite: the answer to command `id`.
    CommandAnswer {
        id: u64,
        response: Response,
    },
    /// Either end; the other answers a Pong with the same nonce (its driver does: a `Link` answers nothing itself).
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Primary: the keymap input is typed with from now on, as xkb keymap text (`KEYMAP_FORMAT_TEXT_V1`), so the
    /// user's layout follows them. Sent before a Handoff when it changed.
    Keymap {
        xkb: Xkb,
    },
    /// Primary: input comes here now. `mods`: the modifier keys held, to press. `pointer`: where the pointer
    /// enters; none: its cursor stays where it is.
    Handoff {
        mods: Vec<RawKey>,
        pointer: Option<Entry>,
    },
    /// Primary: a key went down or up; `time` in milliseconds, the sender's clock.
    Key {
        keycode: RawKey,
        pressed: bool,
        time: u32,
    },
    /// Primary: relative pointer motion, in logical pixels.
    Motion {
        dx: f64,
        dy: f64,
    },
    /// Primary: a pointer button (its evdev code) went down or up.
    Button {
        code: u32,
        pressed: bool,
    },
    /// Primary: scroll, horizontal and vertical: the amounts, and wheel steps in 120ths of a notch (0: none).
    Axis {
        source: ScrollSource,
        h: f64,
        v: f64,
        v120_h: i32,
        v120_v: i32,
    },
    /// Primary: input leaves; release everything down.
    Release,
    /// Satellite: its pointer left its screens past `edge` (the left of its first, the right of its last) at height
    /// `y` (a fraction); the primary hands it to the machine beside it there (`crossing::landing`).
    InputExit {
        edge: Edge,
        y: f64,
    },
    /// Satellite: `request` changes the session (`workspace N` for a number another machine has, from its keys or
    /// its control socket): the primary routes it as its own.
    SessionRequest {
        request: Request,
    },
}

/// A keymap as xkb text. Debug shows only its length: it is long.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Xkb(pub String);

impl fmt::Debug for Xkb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Xkb({} bytes)", self.0.len())
    }
}

/// What a scroll came from, as Wayland's `wl_pointer.axis_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollSource {
    Wheel,
    Finger,
    Continuous,
    WheelTilt,
}

impl Message {
    /// Input the primary sends a satellite: Keymap, Handoff, Key, Motion, Button, Axis or Release.
    #[must_use]
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Self::Keymap { .. }
                | Self::Handoff { .. }
                | Self::Key { .. }
                | Self::Motion { .. }
                | Self::Button { .. }
                | Self::Axis { .. }
                | Self::Release
        )
    }

    /// Its tag on the wire.
    fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Welcome { .. } => "welcome",
            Self::Refused { .. } => "refused",
            Self::WorkspaceState { .. } => "workspace-state",
            Self::Renames { .. } => "renames",
            Self::Command { .. } => "command",
            Self::CommandAnswer { .. } => "command-answer",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Keymap { .. } => "keymap",
            Self::Handoff { .. } => "handoff",
            Self::Key { .. } => "key",
            Self::Motion { .. } => "motion",
            Self::Button { .. } => "button",
            Self::Axis { .. } => "axis",
            Self::Release => "release",
            Self::InputExit { .. } => "input-exit",
            Self::SessionRequest { .. } => "session-request",
        }
    }
}

/// Why a link closed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Closed {
    #[error("the handshake failed (not the same secret at both ends?)")]
    Handshake,
    #[error("the connection closed during the handshake (a different secret at the other end?)")]
    EofInHandshake,
    #[error("the connection closed")]
    Eof,
    #[error("a frame did not decrypt")]
    Decrypt,
    #[error("a frame of {len} bytes, over the {MAX_FRAME} allowed")]
    Oversize { len: usize },
    /// serde's error, cut to `MAX_REASON` bytes.
    #[error("a message did not decode: {0}")]
    Undecodable(String),
    /// A message to the wrong end, or out of order.
    #[error("a protocol error: {0}")]
    Protocol(String),
    #[error("the other end stopped reading ({MAX_OUTPUT} bytes waiting to be written)")]
    Stalled,
    #[error("not open within {OPEN_DEADLINE:?}")]
    Timeout,
    /// The other end refused this one; its reason cut to `MAX_REASON` bytes.
    #[error("refused: {0}")]
    Refused(String),
    /// This end refused the other one.
    #[error("refused the other end: {0}")]
    WeRefused(String),
}

/// Why `Link::send` sent nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// Closed before, or by this message (`Closed::Stalled`).
    #[error("the link is closed")]
    Closed,
    #[error("a message of {len} bytes, over the {MAX_MESSAGE} a frame holds")]
    TooLarge { len: usize },
}

/// One end of a link, without its connection: the satellite's (`Role::Satellite`, the initiator) or the primary's.
pub struct Link {
    role: Role,
    noise: Noise,
    /// The other end has shown it holds the secret: a frame it sent decrypted after the handshake (primary), or the
    /// handshake's answer did (satellite). NNpsk0's first message can be replayed, so the primary waits for more.
    proven: bool,
    /// Read and not yet a whole frame.
    input: Vec<u8>,
    /// Frames to write, from `written` on.
    output: Vec<u8>,
    written: usize,
    /// Messages' JSON queued during the handshake.
    waiting: Vec<Vec<u8>>,
    received: VecDeque<Message>,
    /// Whether a message came in yet: the first must be the Hello (primary) or the Welcome (satellite).
    first_in: bool,
    closed: Option<Closed>,
}

enum Noise {
    Handshake(Box<HandshakeState>),
    Transport(Box<TransportState>),
    /// Closed: the keys are gone.
    Gone,
}

impl Link {
    /// A link on `secret`; a satellite's has its first handshake message ready to write at once.
    #[must_use]
    pub fn new(role: Role, secret: &Secret) -> Self {
        let params: NoiseParams = NOISE_PARAMS.parse().expect("valid Noise parameters");
        let builder = (snow::Builder::new(params).prologue(LABEL))
            .and_then(|b| b.psk(0, &secret.0))
            .expect("a prologue and a 32-byte PSK");
        let handshake = match role {
            Role::Satellite => builder.build_initiator(),
            Role::Primary => builder.build_responder(),
        };
        let mut link = Self {
            role,
            noise: Noise::Handshake(Box::new(handshake.expect("a built-in resolver"))),
            proven: false,
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
            waiting: Vec::new(),
            received: VecDeque::new(),
            first_in: true,
            closed: None,
        };
        link.handshake_turn();
        link
    }

    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// The handshake is done, the other end has shown it holds the secret, and the link is not closed. On the
    /// primary that takes the satellite's first message, its Hello (decoded and of this version): a replayed
    /// handshake never gets this far. The driver closes a link not open within `OPEN_DEADLINE` (`expire`).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.closed.is_none() && self.proven
    }

    /// Why the link closed, once it has.
    #[must_use]
    pub fn closed(&self) -> Option<&Closed> {
        self.closed.as_ref()
    }

    /// Closed with nothing left to write: the connection can go.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.closed.is_some() && self.outgoing().is_empty()
    }

    /// Bytes read from the connection, in any pieces. Ignored once closed.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.closed.is_some() {
            return;
        }
        self.input.extend_from_slice(bytes);
        while self.closed.is_none() && self.input.len() >= 4 {
            let len = u32::from_be_bytes([self.input[0], self.input[1], self.input[2], self.input[3]]) as usize;
            if len > MAX_FRAME {
                return self.fail(Closed::Oversize { len });
            }
            if self.input.len() < 4 + len {
                return;
            }
            let frame: Vec<u8> = self.input.drain(..4 + len).skip(4).collect();
            self.frame(&frame);
        }
    }

    /// The other end closed the connection.
    pub fn eof(&mut self) {
        if self.closed.is_none() {
            let why = match self.noise {
                Noise::Handshake(_) => Closed::EofInHandshake,
                _ => Closed::Eof,
            };
            self.fail(why);
        }
    }

    /// `OPEN_DEADLINE` has passed since the connection was made: closes the link (`Closed::Timeout`) unless it is
    /// open (or already closed).
    pub fn expire(&mut self) {
        if self.closed.is_none() && !self.proven {
            self.fail(Closed::Timeout);
        }
    }

    /// The next message decoded, in order.
    pub fn recv(&mut self) -> Option<Message> {
        self.received.pop_front()
    }

    /// Queues `msg`: written once the handshake is done (at once if it is). A Refused closes the link after it.
    /// Past `MAX_OUTPUT` unwritten, the link fails instead (`Closed::Stalled`).
    pub fn send(&mut self, msg: &Message) -> Result<(), SendError> {
        if self.closed.is_some() {
            return Err(SendError::Closed);
        }
        if let Message::Refused { reason } = msg {
            self.refuse(reason.clone());
            return Ok(());
        }
        let json = serde_json::to_vec(msg).expect("messages serialize");
        if json.len() > MAX_MESSAGE {
            return Err(SendError::TooLarge { len: json.len() });
        }
        self.queue(json);
        match self.closed {
            Some(_) => Err(SendError::Closed),
            None => Ok(()),
        }
    }

    /// Refuses the other end with `reason` and closes. The refusal is written after anything queued once the
    /// handshake is done; before, nothing more is written.
    pub fn refuse(&mut self, reason: impl Into<String>) {
        if self.closed.is_some() {
            return;
        }
        let mut reason = reason.into();
        if !matches!(self.noise, Noise::Transport(_)) {
            return self.fail(Closed::WeRefused(reason));
        }
        truncate(&mut reason);
        let json = serde_json::to_vec(&Message::Refused { reason: reason.clone() }).expect("messages serialize");
        self.queue(json);
        if self.closed.is_none() {
            self.closed = Some(Closed::WeRefused(reason));
            self.noise = Noise::Gone;
        }
    }

    /// This end cannot act on a message it was sent (a keymap that does not compile): closes the link as a protocol
    /// error, dropping what was received after it. Nothing if closed already.
    pub fn protocol_error(&mut self, why: impl Into<String>) {
        if self.closed.is_none() {
            self.received.clear();
            self.fail(Closed::Protocol(why.into()));
        }
    }

    /// The bytes to write to the connection, in order; `wrote` takes those written off.
    #[must_use]
    pub fn outgoing(&self) -> &[u8] {
        &self.output[self.written..]
    }

    /// The first `n` bytes of `outgoing` were written. Panics if `n` is more than `outgoing` holds.
    pub fn wrote(&mut self, n: usize) {
        assert!(
            n <= self.outgoing().len(),
            "wrote {n} bytes of {} outgoing",
            self.outgoing().len()
        );
        self.written += n;
        // Written bytes go once they are the larger part, so each byte moves at most once on average.
        if self.written == self.output.len() {
            self.output.clear();
            self.written = 0;
        } else if self.written > self.output.len() / 2 {
            self.output.drain(..self.written);
            self.written = 0;
        }
    }

    /// Queues a message's `json`, checked for size by `send`; fails the link past `MAX_OUTPUT` unwritten.
    fn queue(&mut self, json: Vec<u8>) {
        let pending = self.outgoing().len() + self.waiting.iter().map(Vec::len).sum::<usize>();
        if pending + 4 + json.len() + TAG > MAX_OUTPUT {
            return self.fail(Closed::Stalled);
        }
        let mut frame = vec![0; json.len() + TAG];
        let n = match &mut self.noise {
            Noise::Transport(t) => (t.write_message(&json, &mut frame)).expect("a message under MAX_MESSAGE encrypts"),
            Noise::Handshake(_) => return self.waiting.push(json),
            Noise::Gone => return,
        };
        push_frame(&mut self.output, &frame[..n]);
    }

    /// Closes for `why`, dropping anything unwritten and the keys; messages decoded before stay to `recv`.
    fn fail(&mut self, why: Closed) {
        self.closed = Some(why);
        self.noise = Noise::Gone;
        self.input.clear();
        self.output.clear();
        self.written = 0;
        self.waiting.clear();
    }

    /// One whole frame read.
    fn frame(&mut self, frame: &[u8]) {
        let mut payload = vec![0; frame.len()];
        match &mut self.noise {
            Noise::Handshake(h) => {
                // Our handshake messages carry no payload; one that does is not ours to read, and is ignored.
                if h.read_message(frame, &mut payload).is_err() {
                    return self.fail(Closed::Handshake);
                }
                self.handshake_turn();
            }
            Noise::Transport(t) => match t.read_message(frame, &mut payload) {
                Ok(n) => self.decode(&payload[..n]),
                Err(_) => self.fail(Closed::Decrypt),
            },
            Noise::Gone => {}
        }
    }

    /// Writes this end's handshake message if it is its turn, then, once the handshake is done, what waited.
    fn handshake_turn(&mut self) {
        let Noise::Handshake(h) = &mut self.noise else {
            return;
        };
        // NN takes two messages: the satellite's, then the primary's (`is_my_turn` stays true once finished).
        if !h.is_handshake_finished() && h.is_my_turn() {
            let mut msg = [0; 64];
            let n = (h.write_message(&[], &mut msg)).expect("an empty handshake payload");
            push_frame(&mut self.output, &msg[..n]);
        }
        if !h.is_handshake_finished() {
            return;
        }
        let Noise::Handshake(h) = std::mem::replace(&mut self.noise, Noise::Gone) else {
            unreachable!()
        };
        self.noise = Noise::Transport(Box::new(h.into_transport_mode().expect("a finished handshake")));
        // The primary's answer decrypted at the satellite: only the secret's holder could have made it.
        self.proven = self.role == Role::Satellite;
        for json in std::mem::take(&mut self.waiting) {
            self.queue(json);
        }
    }

    /// One message's JSON, decrypted. The primary checks the first one's version before anything else, so a
    /// satellite of another version is refused for that even when its Hello does not decode here.
    fn decode(&mut self, json: &[u8]) {
        /// What every version's Hello has.
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let first = std::mem::replace(&mut self.first_in, false);
        if first
            && self.role == Role::Primary
            && let Ok(Versioned { version }) = serde_json::from_slice(json)
            && version != PROTOCOL_VERSION
        {
            return self.refuse(format!(
                "the satellite speaks link protocol {version}, the primary {PROTOCOL_VERSION}: run the same {NAME} \
                 release on both"
            ));
        }
        let msg = match serde_json::from_slice(json) {
            Ok(Message::Refused { mut reason }) => {
                truncate(&mut reason);
                return self.fail(Closed::Refused(reason));
            }
            Ok(msg) => msg,
            Err(e) => {
                let mut e = e.to_string();
                truncate(&mut e);
                return self.fail(Closed::Undecodable(e));
            }
        };
        if let Some(why) = self.misplaced(&msg, first) {
            return self.fail(Closed::Protocol(why));
        }
        self.proven = true;
        self.received.push_back(msg);
    }

    /// Why `msg` may not come to this end now, if it may not.
    fn misplaced(&self, msg: &Message, first: bool) -> Option<String> {
        use Message as M;
        let (opening, end) = match self.role {
            Role::Primary => ("hello", "the primary"),
            Role::Satellite => ("welcome", "a satellite"),
        };
        let fits = match (self.role, msg) {
            (Role::Primary, M::Hello { .. }) | (Role::Satellite, M::Welcome { .. }) => first,
            (Role::Primary, M::WorkspaceState { .. } | M::CommandAnswer { .. } | M::InputExit { .. })
            | (Role::Primary, M::SessionRequest { .. })
            | (Role::Satellite, M::Renames { .. } | M::Command { .. })
            | (Role::Satellite, M::Keymap { .. } | M::Handoff { .. } | M::Key { .. } | M::Motion { .. })
            | (Role::Satellite, M::Button { .. } | M::Axis { .. } | M::Release)
            | (_, M::Ping { .. } | M::Pong { .. }) => !first,
            _ => false,
        };
        (!fits).then(|| match first {
            true => format!("the first message to {end} must be a {opening}, not a {}", msg.kind()),
            false => format!("a {} to {end}", msg.kind()),
        })
    }
}

/// Cuts `s` to `MAX_REASON` bytes (at a character), marked with an ellipsis.
fn truncate(s: &mut String) {
    if s.len() > MAX_REASON {
        let cut = (0..=MAX_REASON).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        s.truncate(cut);
        s.push('…');
    }
}

/// Appends `frame` to `output`, its length first.
fn push_frame(output: &mut Vec<u8>, frame: &[u8]) {
    let len = u32::try_from(frame.len()).expect("frames are under MAX_FRAME");
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(frame);
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use super::*;

    const MARKER: &str = "plaintext-marker-5b1e";

    fn secret(s: &str) -> Secret {
        Secret::from_contents(s.as_bytes()).expect("a valid secret")
    }

    fn pair() -> (Link, Link) {
        let s = "the shared secret of these two ends, at least 32 bytes";
        (
            Link::new(Role::Satellite, &secret(s)),
            Link::new(Role::Primary, &secret(s)),
        )
    }

    fn hello(version: u32, name: &str) -> Message {
        Message::Hello {
            version,
            name: name.into(),
            screens: vec![Screen {
                name: "out-1".into(),
                number: 2,
            }],
            workspaces: BTreeSet::from([2, 5]),
        }
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// Moves what `from` has to write to `to`, written and fed in pieces of `chunk` bytes, onto `wire` too; false
    /// when nothing.
    fn pass(from: &mut Link, to: &mut Link, wire: &mut Vec<u8>, chunk: usize) -> bool {
        let mut moved = false;
        while !from.outgoing().is_empty() {
            let piece = from.outgoing()[..from.outgoing().len().min(chunk)].to_vec();
            from.wrote(piece.len());
            wire.extend(&piece);
            to.feed(&piece);
            moved = true;
        }
        moved
    }

    /// A satellite and a primary through the handshake and the Hello, both open.
    fn joined() -> (Link, Link) {
        let (mut sat, mut pri) = pair();
        sat.send(&hello(PROTOCOL_VERSION, "s")).expect("queued");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert_eq!(drain(&mut pri), [hello(PROTOCOL_VERSION, "s")]);
        assert!(sat.is_open() && pri.is_open());
        (sat, pri)
    }

    /// Both ways until neither end has anything to write.
    fn pump(a: &mut Link, b: &mut Link, wire: &mut Vec<u8>, chunk: usize) {
        while pass(a, b, wire, chunk) | pass(b, a, wire, chunk) {}
    }

    fn drain(l: &mut Link) -> Vec<Message> {
        std::iter::from_fn(|| l.recv()).collect()
    }

    /// Every message both ways: Hello queued before the handshake, then the rest; returns what crossed.
    fn converse(chunk: usize) -> Vec<u8> {
        let (mut sat, mut pri) = pair();
        let mut wire = Vec::new();
        assert!(!sat.is_open() && !pri.is_open());
        sat.send(&hello(PROTOCOL_VERSION, MARKER)).expect("queued");
        pump(&mut sat, &mut pri, &mut wire, chunk);
        assert!(sat.is_open() && pri.is_open());
        assert_eq!(drain(&mut pri), [hello(PROTOCOL_VERSION, MARKER)]);

        let to_sat = [
            Message::Welcome {
                renames: vec![(2, 3)],
                foreign: BTreeSet::from([1, 2]),
            },
            Message::Renames {
                renames: vec![],
                foreign: BTreeSet::from([1]),
            },
            Message::Command {
                id: 7,
                request: Request::Spawn {
                    argv: vec!["foot".into()],
                },
            },
            Message::Ping { nonce: 9 },
        ];
        to_sat.iter().for_each(|m| pri.send(m).expect("sent"));
        pump(&mut sat, &mut pri, &mut wire, chunk);
        assert_eq!(drain(&mut sat), to_sat);

        let to_pri = [
            Message::WorkspaceState {
                screens: vec![],
                workspaces: BTreeSet::from([3]),
            },
            Message::CommandAnswer {
                id: 7,
                response: Response::error(MARKER),
            },
            Message::Pong { nonce: 9 },
            Message::Ping { nonce: 10 },
        ];
        to_pri.iter().for_each(|m| sat.send(m).expect("sent"));
        pump(&mut sat, &mut pri, &mut wire, chunk);
        assert_eq!(drain(&mut pri), to_pri);
        assert!(sat.closed().is_none() && pri.closed().is_none());
        wire
    }

    #[test]
    fn messages_cross_both_ways_encrypted() {
        let wire = converse(usize::MAX);
        assert!(!contains(&wire, MARKER.as_bytes()), "nothing in clear");
        assert!(!contains(&wire, b"hello"));
    }

    #[test]
    fn bytes_fed_one_at_a_time_make_the_same_messages() {
        converse(1);
    }

    #[test]
    fn a_wrong_secret_fails_the_handshake_with_nothing_in_clear() {
        let mut sat = Link::new(
            Role::Satellite,
            &secret("the satellite's secret, at least 32 bytes long"),
        );
        let mut pri = Link::new(Role::Primary, &secret("the primary's secret, also at least 32 bytes"));
        let mut wire = Vec::new();
        sat.send(&hello(PROTOCOL_VERSION, MARKER)).expect("queued");
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert_eq!(pri.closed(), Some(&Closed::Handshake));
        assert!(pri.is_finished(), "nothing to say without a key");
        assert!(drain(&mut pri).is_empty());
        // The primary drops the connection; the satellite sees it go.
        assert!(sat.closed().is_none() && !sat.is_open());
        sat.eof();
        assert_eq!(sat.closed(), Some(&Closed::EofInHandshake));
        assert!(sat.is_finished());
        assert_eq!(sat.send(&Message::Ping { nonce: 1 }), Err(SendError::Closed));
        assert!(
            !wire.is_empty() && !contains(&wire, MARKER.as_bytes()),
            "only the first handshake message"
        );
        assert!(!contains(&wire, b"hello"));
    }

    #[test]
    fn a_version_mismatch_is_refused_with_the_reason_on_both_ends() {
        let (mut sat, mut pri) = pair();
        let mut wire = Vec::new();
        sat.send(&hello(PROTOCOL_VERSION + 1, "s")).expect("queued");
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        let Some(Closed::WeRefused(reason)) = pri.closed() else {
            panic!("refused: {:?}", pri.closed());
        };
        assert!(reason.contains(&format!("{}", PROTOCOL_VERSION + 1)), "{reason}");
        assert_eq!(sat.closed(), Some(&Closed::Refused(reason.clone())));
        assert!(drain(&mut pri).is_empty() && drain(&mut sat).is_empty());
        assert!(sat.is_finished() && pri.is_finished());

        // A later version's Hello that does not even decode here is still refused for its version.
        let (mut sat, mut pri) = pair();
        sat.queue(br#"{"msg":"hello","version":99,"shape":"new"}"#.to_vec());
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert!(
            matches!(pri.closed(), Some(Closed::WeRefused(r)) if r.contains("99")),
            "{:?}",
            pri.closed()
        );
        assert!(matches!(sat.closed(), Some(Closed::Refused(r)) if r.contains("99")));
    }

    #[test]
    fn a_refusal_is_written_then_the_link_closes() {
        let (mut sat, mut pri) = pair();
        let mut wire = Vec::new();
        sat.send(&hello(PROTOCOL_VERSION, "s")).expect("queued");
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert_eq!(drain(&mut pri), [hello(PROTOCOL_VERSION, "s")]);
        pri.refuse("a machine named \"s\" is already in the session");
        assert!(!pri.is_finished(), "the refusal still to write");
        assert_eq!(pri.send(&Message::Ping { nonce: 1 }), Err(SendError::Closed));
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        let why = Closed::Refused("a machine named \"s\" is already in the session".into());
        assert_eq!(sat.closed(), Some(&why));
        assert!(pri.is_finished());
    }

    #[test]
    fn an_oversize_frame_or_one_that_does_not_decrypt_closes_the_link() {
        let (mut sat, mut pri) = joined();
        let mut wire = Vec::new();

        // Too big to send: refused, the link stays open.
        let big = Message::CommandAnswer {
            id: 1,
            response: Response::error("x".repeat(MAX_MESSAGE)),
        };
        assert!(matches!(sat.send(&big), Err(SendError::TooLarge { .. })));
        assert!(sat.is_open() && sat.outgoing().is_empty());
        // The largest that fits goes.
        let json_len = serde_json::to_vec(&big).expect("json").len();
        let fits = Message::CommandAnswer {
            id: 1,
            response: Response::error("x".repeat(2 * MAX_MESSAGE - json_len)),
        };
        sat.send(&fits).expect("fits one frame");
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert_eq!(drain(&mut pri), [fits]);

        // Too big to read: the length alone closes it.
        pri.feed(&u32::try_from(MAX_FRAME + 1).expect("u32").to_be_bytes());
        assert_eq!(pri.closed(), Some(&Closed::Oversize { len: MAX_FRAME + 1 }));
        pri.feed(&[0; 64]);
        assert_eq!(
            pri.closed(),
            Some(&Closed::Oversize { len: MAX_FRAME + 1 }),
            "ignored once closed"
        );

        // A flipped bit: the frame does not decrypt.
        let (mut sat, mut pri) = joined();
        sat.send(&Message::Ping { nonce: 1 }).expect("sent");
        let mut frame = sat.outgoing().to_vec();
        *frame.last_mut().expect("a frame") ^= 1;
        pri.feed(&frame);
        assert_eq!(pri.closed(), Some(&Closed::Decrypt));
        assert!(drain(&mut pri).is_empty());
    }

    #[test]
    fn a_message_that_does_not_decode_closes_the_link() {
        let (mut sat, mut pri) = pair();
        let mut wire = Vec::new();
        sat.send(&hello(PROTOCOL_VERSION, "s")).expect("queued");
        sat.queue(br#"{"msg":"teleport"}"#.to_vec());
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert_eq!(drain(&mut pri), [hello(PROTOCOL_VERSION, "s")]);
        assert!(matches!(pri.closed(), Some(Closed::Undecodable(e)) if e.contains("teleport")));

        // What the other end sent is cut short in the reason.
        let (mut sat, mut pri) = joined();
        sat.queue(format!(r#"{{"msg":"{}"}}"#, "é".repeat(5000)).into_bytes());
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        let Some(Closed::Undecodable(e)) = pri.closed() else {
            panic!("{:?}", pri.closed());
        };
        assert!(e.len() <= MAX_REASON + 3 && e.ends_with('…'), "{} bytes", e.len());
    }

    #[test]
    fn the_first_message_is_the_hello_or_the_welcome_and_messages_go_one_way() {
        // To the primary: a Hello first, then only a satellite's messages; to the satellite: a Welcome first.
        let wrong_first = |role, first: Message| {
            let (mut sat, mut pri) = pair();
            let (from, to) = if role == Role::Primary {
                (&mut sat, &mut pri)
            } else {
                (&mut pri, &mut sat)
            };
            from.send(&first).expect("queued");
            pump(from, to, &mut Vec::new(), usize::MAX);
            assert!(
                matches!(to.closed(), Some(Closed::Protocol(_))),
                "{first:?}: {:?}",
                to.closed()
            );
            assert!(!to.is_open() && drain(to).is_empty());
        };
        wrong_first(Role::Primary, Message::Ping { nonce: 1 });
        wrong_first(
            Role::Primary,
            Message::WorkspaceState {
                screens: vec![],
                workspaces: BTreeSet::new(),
            },
        );
        wrong_first(
            Role::Satellite,
            Message::Command {
                id: 1,
                request: Request::State,
            },
        );
        wrong_first(Role::Satellite, Message::Ping { nonce: 1 });

        let welcome = Message::Welcome {
            renames: vec![],
            foreign: BTreeSet::new(),
        };
        let renames = Message::Renames {
            renames: vec![],
            foreign: BTreeSet::new(),
        };
        let command = Message::Command {
            id: 1,
            request: Request::State,
        };
        let answer = Message::CommandAnswer {
            id: 1,
            response: Response::ok(),
        };
        let state = Message::WorkspaceState {
            screens: vec![],
            workspaces: BTreeSet::new(),
        };
        for wrong in [hello(PROTOCOL_VERSION, "s"), welcome.clone(), renames, command] {
            let (mut sat, mut pri) = joined();
            sat.send(&wrong).expect("queued");
            pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
            assert!(
                matches!(pri.closed(), Some(Closed::Protocol(_))),
                "{wrong:?}: {:?}",
                pri.closed()
            );
        }
        for wrong in [hello(PROTOCOL_VERSION, "s"), welcome.clone(), state, answer] {
            let (mut sat, mut pri) = joined();
            pri.send(&welcome).expect("sent");
            pri.send(&wrong).expect("sent");
            pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
            assert_eq!(drain(&mut sat), std::slice::from_ref(&welcome));
            assert!(
                matches!(sat.closed(), Some(Closed::Protocol(_))),
                "{wrong:?}: {:?}",
                sat.closed()
            );
        }
    }

    /// Every input message, as the primary sends them.
    fn input() -> Vec<Message> {
        vec![
            Message::Keymap {
                xkb: Xkb("xkb_keymap { };".into()),
            },
            Message::Handoff {
                mods: vec![RawKey(133), RawKey(50)],
                pointer: Some(Entry {
                    edge: Edge::Left,
                    y: 0.375,
                }),
            },
            Message::Handoff {
                mods: vec![],
                pointer: None,
            },
            Message::Key {
                keycode: RawKey(38),
                pressed: true,
                time: 7,
            },
            Message::Motion { dx: -3.5, dy: 0.25 },
            Message::Button {
                code: 272,
                pressed: false,
            },
            Message::Axis {
                source: ScrollSource::WheelTilt,
                h: 15.0,
                v: -7.5,
                v120_h: 120,
                v120_v: -60,
            },
            Message::Release,
        ]
    }

    #[test]
    fn input_goes_to_a_satellite_and_comes_back_from_it() {
        let (mut sat, mut pri) = joined();
        let welcome = Message::Welcome {
            renames: vec![],
            foreign: BTreeSet::new(),
        };
        pri.send(&welcome).expect("sent");
        let mut wire = Vec::new();
        input().iter().for_each(|m| pri.send(m).expect("sent"));
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert_eq!(drain(&mut sat)[1..], input());
        let exits = [
            Message::InputExit {
                edge: Edge::Left,
                y: 0.5,
            },
            Message::InputExit {
                edge: Edge::Right,
                y: 0.0,
            },
        ];
        exits.iter().for_each(|m| sat.send(m).expect("sent"));
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert_eq!(drain(&mut pri), exits);
        assert!(sat.is_open() && pri.is_open());
        assert!(!contains(&wire, b"xkb_keymap"), "nothing in clear");

        // Input never goes the other way, nor InputExit to a satellite.
        for wrong in input() {
            let (mut sat, mut pri) = joined();
            sat.send(&wrong).expect("queued");
            pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
            assert!(
                matches!(pri.closed(), Some(Closed::Protocol(_))),
                "{wrong:?}: {:?}",
                pri.closed()
            );
        }
        let (mut sat, mut pri) = joined();
        pri.send(&welcome).expect("sent");
        pri.send(&exits[0]).expect("sent");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert_eq!(drain(&mut sat), [welcome]);
        let Some(Closed::Protocol(why)) = sat.closed() else {
            panic!("{:?}", sat.closed());
        };
        assert_eq!(why, "a input-exit to a satellite");
        // Nor before the Welcome.
        let (mut sat, mut pri) = joined();
        pri.send(&Message::Release).expect("sent");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert!(matches!(sat.closed(), Some(Closed::Protocol(_))));
    }

    #[test]
    fn a_message_this_end_cannot_act_on_closes_the_link_and_drops_what_came_after() {
        let (mut sat, mut pri) = joined();
        let welcome = Message::Welcome {
            renames: Vec::new(),
            foreign: BTreeSet::new(),
        };
        pri.send(&welcome).expect("sent");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert_eq!(drain(&mut sat), [welcome]);
        pri.send(&Message::Keymap {
            xkb: Xkb("nonsense".into()),
        })
        .expect("sent");
        pri.send(&Message::Release).expect("sent");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert!(matches!(sat.recv(), Some(Message::Keymap { .. })));
        sat.protocol_error("a keymap that does not compile");
        assert_eq!(
            sat.closed(),
            Some(&Closed::Protocol("a keymap that does not compile".into()))
        );
        assert_eq!(sat.recv(), None, "the Release after it is dropped");
        assert!(sat.is_finished());
        sat.protocol_error("again");
        assert_eq!(
            sat.closed(),
            Some(&Closed::Protocol("a keymap that does not compile".into())),
            "the first reason stays"
        );
    }

    #[test]
    fn a_session_request_goes_from_a_satellite_to_the_primary_only() {
        let request = Message::SessionRequest {
            request: Request::Workspace { number: 3 },
        };
        let (mut sat, mut pri) = joined();
        sat.send(&request).expect("queued");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert_eq!(drain(&mut pri), std::slice::from_ref(&request));
        assert!(pri.is_open() && !request.is_input());
        // Never the other way, nor first.
        let (mut sat, mut pri) = joined();
        let welcome = Message::Welcome {
            renames: vec![],
            foreign: BTreeSet::new(),
        };
        pri.send(&welcome).expect("sent");
        pri.send(&request).expect("sent");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert_eq!(drain(&mut sat), [welcome]);
        let Some(Closed::Protocol(why)) = sat.closed() else {
            panic!("{:?}", sat.closed());
        };
        assert_eq!(why, "a session-request to a satellite");
        let (mut sat, mut pri) = pair();
        sat.send(&request).expect("queued");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert!(matches!(pri.closed(), Some(Closed::Protocol(_))));
    }

    #[test]
    fn a_key_never_shows_its_keycode() {
        let key = Message::Key {
            keycode: RawKey(38),
            pressed: true,
            time: 7,
        };
        let shown = format!("{key:?} {key:#?}");
        assert!(!shown.contains("38"), "{shown}");
        assert!(shown.contains("RawKey(..)"), "{shown}");
        // On the wire it is the number.
        assert_eq!(
            serde_json::to_string(&key).expect("json"),
            r#"{"msg":"key","keycode":38,"pressed":true,"time":7}"#
        );
    }

    #[test]
    fn a_keymap_of_several_layouts_fits_one_frame() {
        use smithay::input::keyboard::xkb;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
        let keymap = xkb::Keymap::new_from_names(&context, "", "", "us,de,ru", "", None, flags).expect("a keymap");
        let xkb = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let (_, mut pri) = joined();
        let len = xkb.len();
        let msg = Message::Keymap { xkb: Xkb(xkb) };
        assert_eq!(
            format!("{msg:?}"),
            format!("Keymap {{ xkb: Xkb({len} bytes) }}"),
            "only its length"
        );
        assert!(
            serde_json::to_string(&msg)
                .expect("json")
                .starts_with(r#"{"msg":"keymap","xkb":"xkb_keymap"#)
        );
        pri.send(&msg).expect("fits");
    }

    #[test]
    fn a_replayed_first_handshake_message_never_opens_the_primary() {
        // Recorded from a real exchange: the satellite's first handshake frame, then its Hello.
        let (mut sat, mut pri) = pair();
        let first = sat.outgoing().to_vec();
        sat.send(&hello(PROTOCOL_VERSION, "s")).expect("queued");
        pump(&mut sat, &mut pri, &mut Vec::new(), usize::MAX);
        assert!(pri.is_open());
        sat.send(&Message::Ping { nonce: 1 }).expect("sent");
        let later = sat.outgoing().to_vec();

        // Replayed to a fresh primary: it answers (it cannot tell yet), but the link is not open...
        let s = "the shared secret of these two ends, at least 32 bytes";
        let mut replayed = Link::new(Role::Primary, &secret(s));
        replayed.feed(&first);
        assert!(!replayed.outgoing().is_empty() && replayed.closed().is_none());
        assert!(!replayed.is_open(), "the other end has not shown the secret");
        // ...and nothing recorded after it decrypts under the fresh keys.
        replayed.feed(&later);
        assert_eq!(replayed.closed(), Some(&Closed::Decrypt));
        assert!(!replayed.is_open() && drain(&mut replayed).is_empty());

        // The driver's deadline: a link not open by then is closed for it; an open one is left alone.
        let mut stalled = Link::new(Role::Primary, &secret(s));
        stalled.feed(&first);
        stalled.expire();
        assert_eq!(stalled.closed(), Some(&Closed::Timeout));
        let (mut sat, mut pri) = joined();
        sat.expire();
        pri.expire();
        assert!(sat.is_open() && pri.is_open());
        assert!(OPEN_DEADLINE >= Duration::from_secs(1));
    }

    #[test]
    fn a_stalled_reader_fails_the_link() {
        let (_sat, mut pri) = joined();
        let mut sent = 0;
        while pri.send(&Message::Ping { nonce: sent }).is_ok() {
            sent += 1;
            assert!(pri.outgoing().len() <= MAX_OUTPUT);
        }
        assert_eq!(pri.closed(), Some(&Closed::Stalled));
        assert!(pri.is_finished(), "nothing more to write");
        assert!(sent > 1000, "{sent}");

        // Queued during the handshake counts too.
        let (mut sat, _) = pair();
        let big = Message::CommandAnswer {
            id: 1,
            response: Response::error("z".repeat(60_000)),
        };
        let mut n = 0;
        while sat.send(&big).is_ok() {
            n += 1;
        }
        assert_eq!(sat.closed(), Some(&Closed::Stalled));
        assert!((15..=18).contains(&n), "{n}");
    }

    #[test]
    fn a_refusal_during_the_handshake_writes_nothing() {
        for role in [Role::Satellite, Role::Primary] {
            let mut l = Link::new(role, &secret("the shared secret of these two ends, at least 32 bytes"));
            l.refuse("no");
            assert_eq!(l.closed(), Some(&Closed::WeRefused("no".into())));
            assert!(l.is_finished() && l.outgoing().is_empty(), "{role:?}");
        }
    }

    #[test]
    #[should_panic(expected = "outgoing")]
    fn writing_more_than_outgoing_is_a_bug() {
        let (mut sat, _) = pair();
        sat.wrote(sat.outgoing().len() + 1);
    }

    #[test]
    fn the_other_end_going_is_a_close() {
        let (mut sat, mut pri) = pair();
        pri.eof();
        assert_eq!(pri.closed(), Some(&Closed::EofInHandshake));
        let (mut s2, mut p2) = pair();
        pump(&mut s2, &mut p2, &mut Vec::new(), usize::MAX);
        s2.eof();
        assert_eq!(s2.closed(), Some(&Closed::Eof));
        sat.eof();
        sat.eof();
        assert_eq!(sat.closed(), Some(&Closed::EofInHandshake), "the first reason stays");
    }

    /// One non-blocking pass over `sock`: write what `link` has, feed it what arrived.
    fn drive(link: &mut Link, sock: &mut TcpStream) {
        if !link.outgoing().is_empty() {
            match sock.write(link.outgoing()) {
                Ok(n) => link.wrote(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("write: {e}"),
            }
        }
        let mut buf = [0; 4096];
        match sock.read(&mut buf) {
            Ok(0) => link.eof(),
            Ok(n) => link.feed(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => panic!("read: {e}"),
        }
    }

    /// Drives both ends until `done` says so, for up to 5 s.
    fn until(ends: &mut [(&mut Link, &mut TcpStream); 2], mut done: impl FnMut(&mut Link, &mut Link) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for (link, sock) in ends.iter_mut() {
                drive(link, sock);
            }
            let [(a, _), (b, _)] = ends;
            if done(a, b) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_link_over_loopback_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let mut s_sock = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (mut p_sock, _) = listener.accept().expect("accept");
        s_sock.set_nonblocking(true).expect("non-blocking");
        p_sock.set_nonblocking(true).expect("non-blocking");
        let (mut sat, mut pri) = pair();
        sat.send(&hello(PROTOCOL_VERSION, "s")).expect("queued");

        let mut got = Vec::new();
        let mut ends = [(&mut sat, &mut s_sock), (&mut pri, &mut p_sock)];
        until(&mut ends, |_, p| {
            got.extend(drain(p));
            !got.is_empty()
        });
        assert_eq!(got, [hello(PROTOCOL_VERSION, "s")]);

        // An answer far bigger than one read: many partial reads.
        let big = Message::CommandAnswer {
            id: 3,
            response: Response::error("y".repeat(60_000)),
        };
        ends[0].0.send(&big).expect("sent");
        let mut got = Vec::new();
        until(&mut ends, |_, p| {
            got.extend(drain(p));
            !got.is_empty()
        });
        assert_eq!(got, [big]);

        let welcome = Message::Welcome {
            renames: vec![],
            foreign: BTreeSet::from([1]),
        };
        ends[1].0.send(&welcome).expect("sent");
        ends[1].0.send(&Message::Ping { nonce: 4 }).expect("sent");
        let mut got = Vec::new();
        until(&mut ends, |s, _| {
            got.extend(drain(s));
            got.len() == 2
        });
        assert_eq!(got, [welcome, Message::Ping { nonce: 4 }]);

        // The primary goes: the satellite sees the connection close.
        drop(p_sock);
        let deadline = Instant::now() + Duration::from_secs(5);
        while sat.closed().is_none() {
            drive(&mut sat, &mut s_sock);
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(sat.closed(), Some(&Closed::Eof));
    }

    #[test]
    fn a_secret_file_is_refused_open_to_others_and_never_shown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("secret");
        let contents = format!("{MARKER}-and-enough-more-to-be-32-bytes");
        std::fs::write(&path, format!("{contents}\n")).expect("write");
        let chmod = |mode| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");

        for mode in [0o644, 0o640, 0o604, 0o610] {
            chmod(mode);
            let e = Secret::load(&path).expect_err("refused").to_string();
            assert!(
                e.contains(&path.display().to_string()) && e.contains("chmod 600"),
                "{e}"
            );
            assert!(!e.contains(MARKER), "{e}");
        }
        chmod(0o600);
        let s = Secret::load(&path).expect("owner only");
        assert_eq!(s.0, secret(&contents).0, "the newline is not part of it");
        assert_ne!(s.0, secret(&format!("{contents}!")).0);
        // Compared as keys: what a reload checks.
        assert!(s == secret(&contents) && s != secret(&format!("{contents}!")));
        assert_eq!(format!("{s:?}"), "Secret(..)");
        chmod(0o400);
        Secret::load(&path).expect("read-only is fine");
        chmod(0o600);

        std::fs::write(&path, " short \n").expect("write");
        let e = Secret::load(&path).expect_err("too short");
        assert!(matches!(e, SecretError::TooShort { len: 5, .. }), "{e:?}");
        let e = e.to_string();
        assert!(e.contains(&path.display().to_string()) && !e.contains("short"), "{e}");
        std::fs::write(&path, vec![b'z'; SECRET_MAX + 1]).expect("write");
        let e = Secret::load(&path).expect_err("too long").to_string();
        assert!(e.contains(&format!("over {SECRET_MAX}")), "{e}");
        std::fs::write(&path, vec![b'z'; SECRET_MAX]).expect("write");
        Secret::load(&path).expect("the most allowed");

        // A link to a file open to others is refused for the file's mode.
        let open = dir.path().join("open");
        std::fs::write(&open, "o".repeat(40)).expect("write");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&open, &link).expect("symlink");
        assert!(matches!(
            Secret::load(&link),
            Err(SecretError::Open { mode: 0o644, .. })
        ));
        // A FIFO with no writer: refused at once, not waited on.
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("no NUL");
        // Safety: a valid C string for the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        assert!(matches!(Secret::load(&fifo), Err(SecretError::NotAFile { .. })));

        assert!(matches!(Secret::load(dir.path()), Err(SecretError::NotAFile { .. })));
        let gone = dir.path().join("missing");
        let e = Secret::load(&gone).expect_err("missing").to_string();
        assert!(e.contains(&gone.display().to_string()), "{e}");
    }
}
