//! The link between the primary and a satellite: one TCP connection, opened by the satellite (the Noise initiator),
//! carrying `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` keyed by a pre-shared secret file. Every frame is a u32
//! big-endian length and that many bytes: first the two handshake messages, then each `Message` as JSON in one Noise
//! transport message.
//!
//! No sockets here: a `Link` is fed the bytes read from its connection and hands out the messages decoded and the
//! bytes to write, so the compositor drives it from calloop and tests from memory. Nothing is ever sent in clear:
//! messages queued before the handshake ends wait for it, and are dropped if it fails. Any failure (a handshake
//! that fails, a frame that does not decrypt, is too big or does not decode) closes the link with a reason.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use snow::params::{HashChoice, NoiseParams};
use snow::resolvers::{CryptoResolver as _, DefaultResolver};
use snow::{HandshakeState, TransportState};

use crate::NAME;
use crate::ipc::{Request, Response};
use crate::session::{Renames, Role, Screen};

/// The link protocol's version: both ends must speak the same one (the primary refuses a satellite's other one).
pub const PROTOCOL_VERSION: u32 = 1;
pub const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// The largest frame, its length prefix aside: Noise's largest message.
pub const MAX_FRAME: usize = 65535;
/// The largest message's JSON: a frame less the cipher's tag.
pub const MAX_MESSAGE: usize = MAX_FRAME - TAG;
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
/// head -c 32 /dev/urandom | base64 > secret)`. Its contents are never logged or shown, nor is the key.
pub struct Secret([u8; 32]);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
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
    #[error(
        "the secret file {} holds {len} bytes; a secret is {SECRET_MIN} to {SECRET_MAX} (whitespace around it aside)",
        path.display()
    )]
    Size { path: PathBuf, len: u64 },
}

impl Secret {
    /// Reads the secret file at `path`; refused if group or others have any permission on it, or for its size.
    pub fn load(path: &Path) -> Result<Self, SecretError> {
        let read = |source| SecretError::Read {
            path: path.to_owned(),
            source,
        };
        let file = File::open(path).map_err(read)?;
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
        let mut contents = Vec::new();
        // One byte over the most allowed tells a file too long, however long it is.
        (file.take(SECRET_MAX as u64 + 1))
            .read_to_end(&mut contents)
            .map_err(read)?;
        Self::from_contents(&contents).ok_or(SecretError::Size {
            path: path.to_owned(),
            len: contents.len() as u64,
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
        let mut key = [0; 32];
        hash.result(&mut key);
        Some(Self(key))
    }
}

/// What crosses the link, each in one frame. Hello first (satellite), then Welcome or Refused (primary); then
/// either end sends the others as things happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "kebab-case")]
pub enum Message {
    /// Satellite: its version (`PROTOCOL_VERSION`), name, screens and workspace numbers, as for `Session::join`.
    Hello {
        version: u32,
        name: String,
        screens: Vec<Screen>,
        workspaces: BTreeSet<u8>,
    },
    /// Primary: joined. The renames and foreign numbers for the satellite's `Stacks::apply_session`.
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
    #[error("a message did not decode: {0}")]
    Undecodable(String),
    /// The other end refused this one.
    #[error("refused: {0}")]
    Refused(String),
    /// This end refused the other one.
    #[error("refused the other end: {0}")]
    WeRefused(String),
}

/// Why `Link::send` sent nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("the link is closed")]
    Closed,
    #[error("a message of {len} bytes, over the {MAX_MESSAGE} a frame holds")]
    TooLarge { len: usize },
}

/// One end of a link, without its connection: the satellite's (`Role::Satellite`, the initiator) or the primary's.
pub struct Link {
    role: Role,
    noise: Noise,
    /// Read and not yet a whole frame.
    input: Vec<u8>,
    /// Frames to write.
    output: Vec<u8>,
    /// Messages' JSON queued during the handshake.
    waiting: Vec<Vec<u8>>,
    received: VecDeque<Message>,
    /// Whether a message came in yet: the primary checks the first one's version.
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
            input: Vec::new(),
            output: Vec::new(),
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

    /// The handshake is done and the link not closed: messages sent now go out at once.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.closed.is_none() && matches!(self.noise, Noise::Transport(_))
    }

    /// Why the link closed, once it has.
    #[must_use]
    pub fn closed(&self) -> Option<&Closed> {
        self.closed.as_ref()
    }

    /// Closed with nothing left to write: the connection can go.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.closed.is_some() && self.output.is_empty()
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

    /// The next message decoded, in order.
    pub fn recv(&mut self) -> Option<Message> {
        self.received.pop_front()
    }

    /// Queues `msg`: written once the handshake is done (at once if it is). A Refused closes the link after it.
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
        Ok(())
    }

    /// Refuses the other end with `reason`, written after anything queued (if the handshake is done), and closes.
    pub fn refuse(&mut self, reason: impl Into<String>) {
        if self.closed.is_some() {
            return;
        }
        let reason = reason.into();
        let json = serde_json::to_vec(&Message::Refused { reason: reason.clone() }).expect("messages serialize");
        // A reason too long for a frame is cut: the refusal still goes.
        if json.len() <= MAX_MESSAGE {
            self.queue(json);
        } else {
            self.queue(serde_json::to_vec(&Message::Refused { reason: String::new() }).expect("serializes"));
        }
        self.closed = Some(Closed::WeRefused(reason));
        self.waiting.clear();
        self.noise = Noise::Gone;
    }

    /// The bytes to write to the connection, in order; `wrote` takes those written off.
    #[must_use]
    pub fn outgoing(&self) -> &[u8] {
        &self.output
    }

    /// The first `n` bytes of `outgoing` were written.
    pub fn wrote(&mut self, n: usize) {
        self.output.drain(..n);
    }

    /// Queues a message's `json`, checked for size by `send`.
    fn queue(&mut self, json: Vec<u8>) {
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
        match serde_json::from_slice(json) {
            Ok(Message::Refused { reason }) => self.fail(Closed::Refused(reason)),
            Ok(msg) => self.received.push_back(msg),
            Err(e) => self.fail(Closed::Undecodable(e.to_string())),
        }
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

    /// Moves what `from` has to write to `to`, in pieces of `chunk` bytes, onto `wire` too; false when nothing.
    fn pass(from: &mut Link, to: &mut Link, wire: &mut Vec<u8>, chunk: usize) -> bool {
        let out = from.outgoing().to_vec();
        from.wrote(out.len());
        wire.extend(&out);
        out.chunks(chunk).for_each(|c| to.feed(c));
        !out.is_empty()
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
        let (mut sat, mut pri) = pair();
        let mut wire = Vec::new();
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
        assert!(sat.is_open() && pri.is_open());

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
        let (mut sat, mut pri) = pair();
        pump(&mut sat, &mut pri, &mut wire, usize::MAX);
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

        ends[1].0.send(&Message::Ping { nonce: 4 }).expect("sent");
        let mut got = Vec::new();
        until(&mut ends, |s, _| {
            got.extend(drain(s));
            !got.is_empty()
        });
        assert_eq!(got, [Message::Ping { nonce: 4 }]);

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
        assert_eq!(format!("{s:?}"), "Secret(..)");
        chmod(0o400);
        Secret::load(&path).expect("read-only is fine");
        chmod(0o600);

        std::fs::write(&path, " short \n").expect("write");
        let e = Secret::load(&path).expect_err("too short").to_string();
        assert!(e.contains(&path.display().to_string()) && !e.contains("short"), "{e}");
        std::fs::write(&path, vec![b'z'; SECRET_MAX + 1]).expect("write");
        assert!(matches!(Secret::load(&path), Err(SecretError::Size { .. })));
        std::fs::write(&path, vec![b'z'; SECRET_MAX]).expect("write");
        Secret::load(&path).expect("the most allowed");

        assert!(matches!(Secret::load(dir.path()), Err(SecretError::NotAFile { .. })));
        let gone = dir.path().join("missing");
        let e = Secret::load(&gone).expect_err("missing").to_string();
        assert!(e.contains(&gone.display().to_string()), "{e}");
    }
}
