//! The link's driver: the primary's listener and one connection per satellite, or a satellite's one connection to
//! the primary, on calloop beside everything else. `Link` speaks the protocol and `Session` (the primary's) keeps
//! the numbering; this file moves bytes and applies what they say to `Stacks`.
//!
//! Nothing here blocks the loop: sockets are non-blocking, a satellite resolves and connects on a short-lived thread
//! of its own, a connection is read when it is readable, and what links queue is written after each loop iteration
//! (`Orrery::link_tick`), the rest when the socket drains. The tick also reports this machine's workspaces when
//! they changed (the primary to its own session, a satellite to the primary), hands each machine the numbers the
//! others own when they changed, and drops closed connections.
//!
//! The primary's input on its way to a satellite waits in that connection's `Outbox` and goes into its link one
//! message at a time, only when the link has written everything before and fewer than `INPUT_WINDOW` input messages
//! are unacknowledged (the satellite acknowledges what it ran, `InputAck`, once per read): what waits meanwhile
//! (motion) merges. A stalled satellite or network so holds back only a few messages, never seconds of input to
//! replay late; kernel buffers alone cannot bound that (a stalled receiver's kernel keeps taking data).

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _};
use std::os::fd::AsRawFd as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};

use super::{Orrery, ipc_server};
use crate::NAME;
use crate::config::{self, Config, LinkConfig, MAX_NAME};
use crate::crossing::{Crossing, Edge, Landing, Outbox, Place, RowScreen};
use crate::ipc::{MachineView, Request, Response, SessionView};
use crate::link::{INPUT_WINDOW, Link, Message, OPEN_DEADLINE, PROTOCOL_VERSION, Secret, SendError};
use crate::session::{Role, Route, Screen, Session};
use crate::stacks::Stacks;

/// How often each end pings the other; `state` shows the round trip.
pub const PING: Duration = Duration::from_secs(2);
/// A link that hears nothing for this long is dropped: the other end went without closing (a cable, a crash).
pub const SILENCE: Duration = Duration::from_secs(10);
/// The primary's connections not yet joined, at most, in all and from one IP address: one more is closed at once.
pub const MAX_OPENING: usize = 8;
pub const MAX_OPENING_PER_ADDRESS: usize = 2;
/// A Hello naming a satellite whose link has been silent this long takes its place (it is gone, its link not yet
/// dropped) instead of being refused as a duplicate.
pub const STALE: Duration = PING.saturating_mul(2);
/// A satellite waits this long before trying the primary again, doubling each failure up to `BACKOFF_MAX`.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A request sent to another machine (`orrery msg --to`) is answered with an error when its answer takes longer.
pub const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);
/// One connection attempt to one of the primary's addresses gives up after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Read from one connection per wakeup, at most, so a busy link cannot hold the loop.
const READ_BUDGET: usize = 256 * 1024;
/// Unsent bytes a link's kernel holds before it takes no more (TCP_NOTSENT_LOWAT).
const UNSENT_LOW: libc::c_int = 8 * 1024;

/// Where a forwarded request's answer goes: called once, with the other machine's answer or an error.
pub type Reply = Box<dyn FnOnce(Response)>;

/// A machine's workspaces as it reports them: its screens and the numbers it has.
type Report = (Vec<Screen>, BTreeSet<u8>);

/// This machine in a session (`listen` or `primary` in the config).
pub struct Net {
    /// This machine's name (`machine_name`).
    name: String,
    /// The secret file, `~/` expanded; read at start and on each reload.
    secret_file: PathBuf,
    /// None while the secret file cannot be read: nothing connects or is accepted.
    secret: Option<Secret>,
    side: Side,
    conns: HashMap<u64, Conn>,
    next_conn: u64,
    next_nonce: u64,
    /// Why a link last failed, until the next one opens (`state`).
    error: Option<String>,
}

enum Side {
    Primary(Primary),
    Satellite(Satellite),
}

struct Primary {
    /// The bound address (port 0 resolved); None when listening failed.
    listen: Option<SocketAddr>,
    session: Session,
    /// Where this machine's keyboard and pointer go: kept with the session's focus marker.
    crossing: Crossing,
    /// What the primary last told its own session.
    reported: Option<Report>,
    /// Commands sent and not answered yet, by id.
    commands: HashMap<u64, Pending>,
    next_command: u64,
}

struct Satellite {
    /// The primary's HOST:PORT, as configured.
    primary: String,
    /// What this machine last told the primary; None: tell it again.
    reported: Option<Report>,
    /// The wait before the next attempt.
    backoff: Duration,
    /// Connection attempts' results, from their threads.
    connected: Sender<Result<TcpStream, String>>,
}

struct Conn {
    link: Link,
    peer: SocketAddr,
    /// The connection, to write with (the read source holds its twin).
    stream: TcpStream,
    io: Option<RegistrationToken>,
    /// While the socket is full: a source writing the rest as it drains.
    writer: Option<RegistrationToken>,
    /// `OPEN_DEADLINE`'s timer, until it fires.
    deadline: Option<RegistrationToken>,
    /// Who is at the other end once joined: the satellite's name (primary), the primary's address (satellite).
    joined: Option<String>,
    /// The foreign numbers this satellite was last given (primary).
    foreign: BTreeSet<u8>,
    /// The ping in flight, its nonce and when it went: no other is sent until it is answered, however late.
    ping: Option<(u64, Instant)>,
    latency: Option<Duration>,
    /// When it last sent anything.
    heard: Instant,
    /// Why the driver gives up on it while its link is still open (the socket failed, or it went silent).
    dropped: Option<String>,
    /// The primary's input on its way to this satellite, and the keymap (xkb text) it was last sent.
    outbox: Outbox,
    keymap: Option<String>,
    /// Input messages sent and not acknowledged (primary), or run and not acknowledged yet (satellite).
    unacked: u32,
}

/// A command sent to a satellite, waiting for its answer.
struct Pending {
    machine: String,
    /// Where the answer goes; None for a routed `workspace` (a failure is logged).
    reply: Option<Reply>,
    timer: Option<RegistrationToken>,
    /// A routed `workspace`: if it fails, the satellite is asked to report (`resync`), freeing a number the session
    /// reserved for it that it never made.
    resync: bool,
}

/// This machine's name in a session: the config's `name`, else the hostname, else `NAME`.
#[must_use]
pub fn machine_name(config: &Config) -> String {
    (config.name.clone())
        .or_else(|| hostname().filter(|h| config::valid_machine_name(h)))
        .unwrap_or_else(|| NAME.to_owned())
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // Safety: the buffer is valid for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// `link`'s secret file, `~/` taken from `$HOME`.
#[must_use]
pub fn secret_path(link: &LinkConfig) -> PathBuf {
    link.secret_path(super::home_dir().as_deref())
}

/// What `stacks` reports: its screens and every number it has.
fn report(stacks: &Stacks) -> Report {
    let screens = (stacks.monitors().iter())
        .map(|m| Screen {
            name: m.name().to_owned(),
            number: m.number(),
        })
        .collect();
    (screens, stacks.workspaces().iter().map(|w| w.number()).collect())
}

/// Resolves the primary's HOST:PORT and connects to the first address that answers (blocking: a thread's job).
fn connect(addr: &str) -> Result<TcpStream, String> {
    let addrs = addr
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr}: {e}"))?;
    let mut why = format!("{addr} resolves to no address");
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => why = format!("cannot connect to {a}: {e}"),
        }
    }
    Err(why)
}

/// Queues `msg`; a link that cannot take it is past caring (closed: the tick drops it), or it was too big.
fn send(conn: &mut Conn, msg: &Message) -> Result<(), SendError> {
    let sent = conn.link.send(msg);
    if let Err(e) = &sent {
        tracing::debug!("link to {}: {e}", conn.peer);
    }
    sent
}

/// Sets `stream`'s TCP_NOTSENT_LOWAT: the kernel takes more only while little of what it holds is unsent, so the
/// link's own queue (and the outbox before it) is where input waits on a slow network.
fn low_unsent(stream: &TcpStream) -> std::io::Result<()> {
    let size = UNSENT_LOW;
    let len = libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("an int's size fits");
    // Safety: a socket we own, and the option's value is a c_int of that length.
    let set = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NOTSENT_LOWAT,
            (&raw const size).cast(),
            len,
        )
    };
    if set == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The link's round trip for `state`: unknown (None) while a ping is overdue, so a link that stopped answering does
/// not show its last good time.
fn latency_ms(conn: &Conn) -> Option<u64> {
    match conn.ping {
        Some((_, at)) if at.elapsed() > PING => None,
        _ => conn.latency.map(millis),
    }
}

/// Asks a satellite to report what it has: any `Renames` does (`satellite_got`), and its report replaces what the
/// session holds for it.
fn resync(conn: &mut Conn, foreign: BTreeSet<u8>) {
    conn.foreign.clone_from(&foreign);
    let _ = send(
        conn,
        &Message::Renames {
            renames: Vec::new(),
            foreign,
        },
    );
}

impl Pending {
    fn answer(self, response: Response) {
        match self.reply {
            Some(reply) => reply(response),
            None => {
                if let Some(e) = response.error {
                    tracing::info!("{}: {e}", self.machine);
                }
            }
        }
    }
}

impl Orrery {
    /// Joins or starts the session the config asks for. After the backend: the primary's session starts from its
    /// screens. A secret file that cannot be read is a `config_error`, and nothing links until a reload reads it.
    pub fn start_link(&mut self) {
        let Some(link) = self.config.link.clone() else {
            return;
        };
        let name = machine_name(&self.config);
        let secret_file = secret_path(&link);
        let secret = match Secret::load(&secret_file) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!("{e}; not linking until it can be read (reload)");
                self.config_error = Some(e.to_string());
                None
            }
        };
        let side = match link.role {
            Role::Primary => {
                let (screens, numbers) = report(&self.stacks);
                let session = match Session::new(&name, screens.clone(), numbers.iter().copied()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("cannot start a session: {e}");
                        return;
                    }
                };
                Side::Primary(Primary {
                    listen: None,
                    session,
                    crossing: Crossing::new(&name),
                    reported: Some((screens, numbers)),
                    commands: HashMap::new(),
                    next_command: 1,
                })
            }
            Role::Satellite => {
                let (connected, results) = channel::channel();
                let watched = self.loop_handle.insert_source(results, |event, _, state| {
                    if let channel::Event::Msg(result) = event {
                        state.link_connected(result);
                    }
                });
                if let Err(e) = watched {
                    tracing::error!("cannot link: {e}");
                    return;
                }
                Side::Satellite(Satellite {
                    primary: link.addr.clone(),
                    reported: None,
                    backoff: BACKOFF_FIRST,
                    connected,
                })
            }
        };
        tracing::info!("in a session as {name:?}, the {:?}", link.role);
        self.net = Some(Net {
            name,
            secret_file,
            secret,
            side,
            conns: HashMap::new(),
            next_conn: 1,
            next_nonce: 1,
            error: None,
        });
        let pings = self
            .loop_handle
            .insert_source(Timer::from_duration(PING), |_, _, state| {
                state.link_ping();
                TimeoutAction::ToDuration(PING)
            });
        if let Err(e) = pings {
            tracing::error!("cannot ping the link: {e}");
        }
        match link.role {
            Role::Primary => self.link_listen(&link.addr),
            Role::Satellite => self.link_connect(),
        }
    }

    /// Reads the secret file again (on reload). A different secret drops every link (opened on the old one): the
    /// satellites connect again, and get in once they have it too. The error, for `config_error`, when it cannot be
    /// read (the old secret stays).
    pub fn link_reload(&mut self) -> Option<String> {
        let net = self.net.as_mut()?;
        match Secret::load(&net.secret_file) {
            Ok(s) => {
                if net.secret.as_ref().is_some_and(|old| *old != s) && !net.conns.is_empty() {
                    tracing::warn!("the secret changed: dropping every link (they open again on the new one)");
                    for conn in net.conns.values_mut() {
                        conn.dropped.get_or_insert_with(|| "the secret changed".into());
                    }
                }
                net.secret = Some(s);
                None
            }
            Err(e) => {
                tracing::error!("{e}; keeping the secret read before");
                Some(e.to_string())
            }
        }
    }

    /// After each loop iteration: drops closed connections, reports this machine's workspaces if they changed,
    /// hands out foreign numbers that changed, and writes what the links queued.
    pub fn link_tick(&mut self) {
        if self.net.is_none() {
            return;
        }
        self.link_reap();
        self.link_report();
        let ids: Vec<u64> = (self.net.iter().flat_map(|n| n.conns.iter()))
            .filter(|(_, c)| c.writer.is_none() && !(c.link.outgoing().is_empty() && c.outbox.is_empty()))
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            if self.link_flush(id) {
                self.link_drain(id);
            }
        }
    }

    /// Hands connection `id`'s link the input waiting for it, one message at a time, each only once the link has
    /// written everything before and while fewer than `INPUT_WINDOW` are unacknowledged (so what waits meanwhile
    /// merges). A message the link cannot take (too large, or the link closed) drops the connection with the rest:
    /// never half a handoff.
    fn link_drain(&mut self, id: u64) {
        loop {
            let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) else {
                return;
            };
            if conn.dropped.is_some() || !conn.link.outgoing().is_empty() || conn.unacked >= INPUT_WINDOW {
                return;
            }
            let Some(msg) = conn.outbox.pop() else {
                return;
            };
            if let Err(e) = send(conn, &msg) {
                conn.dropped = Some(format!("cannot send it input: {e}"));
                conn.outbox = Outbox::default();
                return;
            }
            conn.unacked += 1;
            if !self.link_flush(id) {
                return;
            }
        }
    }

    fn link_listen(&mut self, addr: &str) {
        let bound = TcpListener::bind(addr).and_then(|l| {
            l.set_nonblocking(true)?;
            Ok((l.local_addr()?, l))
        });
        let (local, listener) = match bound {
            Ok(b) => b,
            Err(e) => {
                let why = format!("cannot listen on {addr}: {e}");
                tracing::error!("{why}");
                if let Some(net) = &mut self.net {
                    net.error = Some(why);
                }
                return;
            }
        };
        let own: Rc<Cell<Option<RegistrationToken>>> = Rc::default();
        let token = own.clone();
        let h = self.loop_handle.clone();
        let registered = self.loop_handle.insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                loop {
                    match listener.accept() {
                        Ok((stream, peer)) => state.link_accepted(stream, peer),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => {
                            // Out of descriptors, the listener stays ready: pause instead of spinning.
                            let (Some(pause), Some(t)) = (ipc_server::accept_backoff(&e), token.get()) else {
                                tracing::warn!("link accept: {e}");
                                break;
                            };
                            tracing::warn!("link accept: {e}; pausing {pause:?}");
                            let h2 = h.clone();
                            let resumed = h.insert_source(Timer::from_duration(pause), move |_, _, _| {
                                if let Err(e) = h2.enable(&t) {
                                    tracing::error!("cannot resume the link listener: {e}");
                                }
                                TimeoutAction::Drop
                            });
                            if resumed.is_ok() {
                                return Ok(PostAction::Disable);
                            }
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        );
        match registered {
            Ok(t) => {
                own.set(Some(t));
                tracing::info!("listening for satellites on {local}");
                if let Some(Net {
                    side: Side::Primary(p), ..
                }) = &mut self.net
                {
                    p.listen = Some(local);
                }
            }
            Err(e) => tracing::error!("cannot watch the link listener: {e}"),
        }
    }

    fn link_accepted(&mut self, stream: TcpStream, peer: SocketAddr) {
        let Some(net) = &self.net else {
            return;
        };
        let Some(secret) = &net.secret else {
            tracing::debug!("no secret: closing the connection from {peer}");
            return;
        };
        let opening = || net.conns.values().filter(|c| c.joined.is_none());
        if opening().count() >= MAX_OPENING {
            tracing::warn!("{MAX_OPENING} connections still joining: closing the one from {peer}");
            return;
        }
        if opening().filter(|c| c.peer.ip() == peer.ip()).count() >= MAX_OPENING_PER_ADDRESS {
            tracing::debug!(
                "{MAX_OPENING_PER_ADDRESS} connections from {} still joining: closing one",
                peer.ip()
            );
            return;
        }
        let link = Link::new(Role::Primary, secret);
        self.link_add(stream, peer, link);
    }

    /// Watches a new connection carrying `link`; None (the connection closed) when it cannot.
    fn link_add(&mut self, stream: TcpStream, peer: SocketAddr, link: Link) -> Option<u64> {
        let prepared = stream
            .set_nonblocking(true)
            .and_then(|()| stream.set_nodelay(true))
            .and_then(|()| stream.try_clone());
        if let Err(e) = low_unsent(&stream) {
            tracing::debug!("link to {peer}: no TCP_NOTSENT_LOWAT ({e}); the input window still bounds input");
        }
        let writer = match prepared {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("link to {peer}: {e}");
                return None;
            }
        };
        let net = self.net.as_mut()?;
        let id = net.next_conn;
        net.next_conn += 1;
        let io = self.loop_handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, state| Ok(state.link_readable(id, stream)),
        );
        let deadline = self
            .loop_handle
            .insert_source(Timer::from_duration(OPEN_DEADLINE), move |_, _, state| {
                state.link_deadline(id);
                TimeoutAction::Drop
            });
        let (io, deadline) = match (io, deadline) {
            (Ok(io), Ok(deadline)) => (io, deadline),
            (io, deadline) => {
                tracing::warn!("cannot watch the link to {peer}");
                for t in [io.ok(), deadline.ok()].into_iter().flatten() {
                    self.loop_handle.remove(t);
                }
                return None;
            }
        };
        let conn = Conn {
            link,
            peer,
            stream: writer,
            io: Some(io),
            writer: None,
            deadline: Some(deadline),
            joined: None,
            foreign: BTreeSet::new(),
            ping: None,
            latency: None,
            heard: Instant::now(),
            dropped: None,
            outbox: Outbox::default(),
            keymap: None,
            unacked: 0,
        };
        self.net.as_mut()?.conns.insert(id, conn);
        Some(id)
    }

    /// Connection `id` is readable: what it sent goes to its link, and the messages decoded are acted on.
    fn link_readable(&mut self, id: u64, stream: &TcpStream) -> PostAction {
        let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) else {
            return PostAction::Disable;
        };
        let mut chunk = [0u8; 16 * 1024];
        let mut read = 0;
        while read < READ_BUDGET && conn.link.closed().is_none() {
            match (&*stream).read(&mut chunk) {
                Ok(0) => conn.link.eof(),
                Ok(n) => {
                    read += n;
                    conn.heard = Instant::now();
                    conn.link.feed(&chunk[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    conn.dropped = Some(format!("the connection failed: {e}"));
                    break;
                }
            }
        }
        self.link_messages(id);
        // Closed: nothing more to read; the tick drops it once its last bytes are written.
        match self.net.as_ref().and_then(|n| n.conns.get(&id)) {
            Some(c) if c.link.closed().is_none() && c.dropped.is_none() => PostAction::Continue,
            _ => PostAction::Disable,
        }
    }

    fn link_deadline(&mut self, id: u64) {
        if let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) {
            conn.deadline = None;
            conn.link.expire();
        }
    }

    /// Acts on every message connection `id` has decoded.
    fn link_messages(&mut self, id: u64) {
        loop {
            let Some(net) = &mut self.net else {
                return;
            };
            let primary = matches!(net.side, Side::Primary(_));
            let Some(conn) = net.conns.get_mut(&id) else {
                return;
            };
            let Some(msg) = conn.link.recv() else {
                // A satellite acknowledges the input it ran, once for all it read.
                if conn.unacked > 0 && !primary {
                    let count = std::mem::take(&mut conn.unacked);
                    let _ = send(conn, &Message::InputAck { count });
                }
                return;
            };
            match msg {
                Message::Ping { nonce } => {
                    if let Some(conn) = net.conns.get_mut(&id) {
                        let _ = send(conn, &Message::Pong { nonce });
                    }
                }
                Message::Pong { nonce } => {
                    if let Some(conn) = net.conns.get_mut(&id)
                        && let Some((sent, at)) = conn.ping
                        && sent == nonce
                    {
                        conn.ping = None;
                        conn.latency = Some(at.elapsed());
                    }
                }
                msg if primary => self.primary_got(id, msg),
                msg => self.satellite_got(id, msg),
            }
        }
    }

    /// A satellite's message, on the primary. Its link has checked it may come here now.
    fn primary_got(&mut self, id: u64, msg: Message) {
        let from = (self.net.as_ref())
            .and_then(|n| n.conns.get(&id))
            .and_then(|c| c.joined.clone());
        match (msg, from) {
            (Message::InputExit { edge, y }, Some(from)) => {
                let side = match edge {
                    Edge::Left => "left",
                    Edge::Right => "right",
                };
                tracing::info!(
                    "{from}'s pointer left its screens past the {side} edge, {:.0} % down",
                    y * 100.0
                );
                self.input_cross(&from, edge, y);
            }
            (Message::SessionRequest { request }, Some(from)) => self.session_request(&from, request),
            (Message::InputAck { count }, _) => {
                if let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) {
                    conn.unacked = conn.unacked.saturating_sub(count);
                }
            }
            (Message::InputExit { .. } | Message::SessionRequest { .. }, None) => {}
            (msg, _) => self.primary_got_link(id, msg),
        }
    }

    /// `primary_got` for what the link and the session alone answer.
    fn primary_got_link(&mut self, id: u64, msg: Message) {
        if let Message::Hello { name, .. } = &msg {
            self.link_take_over(id, name);
        }
        let handle = self.loop_handle.clone();
        let Some(Net {
            side: Side::Primary(p),
            conns,
            error,
            ..
        }) = &mut self.net
        else {
            return;
        };
        let Some(conn) = conns.get_mut(&id) else {
            return;
        };
        match msg {
            Message::Hello {
                name,
                screens,
                workspaces,
                ..
            } => {
                let joined = if config::valid_machine_name(&name) {
                    p.session.join(&name, screens, workspaces).map_err(|e| e.to_string())
                } else {
                    Err(format!(
                        "a machine name is 1 to {MAX_NAME} bytes, no control characters"
                    ))
                };
                match joined {
                    Ok(renames) => {
                        let foreign = p.session.foreign(&name);
                        let _ = send(
                            conn,
                            &Message::Welcome {
                                renames: renames.clone(),
                                foreign: foreign.clone(),
                            },
                        );
                        tracing::info!("{name} joined the session from {} (renumbered: {renames:?})", conn.peer);
                        if let Some(t) = conn.deadline.take() {
                            handle.remove(t);
                        }
                        conn.foreign = foreign;
                        conn.joined = Some(name);
                        *error = None;
                    }
                    Err(why) => {
                        // Logged once it is dropped (`link_lost`), with repeats quieter.
                        tracing::debug!("refused {name:?} from {}: {why}", conn.peer);
                        conn.link.refuse(why);
                    }
                }
            }
            Message::WorkspaceState { screens, workspaces } => {
                let Some(name) = &conn.joined else {
                    return;
                };
                match p.session.report(name, screens, workspaces) {
                    Ok(renames) if renames.is_empty() => {}
                    Ok(renames) => {
                        let foreign = p.session.foreign(name);
                        let _ = send(
                            conn,
                            &Message::Renames {
                                renames,
                                foreign: foreign.clone(),
                            },
                        );
                        conn.foreign = foreign;
                    }
                    Err(e) => tracing::warn!("{name}'s workspaces: {e}"),
                }
            }
            Message::CommandAnswer { id: command, response } => {
                // Only the machine it was sent to answers a command.
                let ours =
                    (p.commands.get(&command)).is_some_and(|c| Some(c.machine.as_str()) == conn.joined.as_deref());
                let Some(pending) = ours.then(|| p.commands.remove(&command)).flatten() else {
                    tracing::debug!("{}: an answer to no command of its own ({command})", conn.peer);
                    return;
                };
                if let Some(t) = &pending.timer {
                    handle.remove(*t);
                }
                if pending.resync && !response.ok {
                    resync(conn, p.session.foreign(&pending.machine));
                }
                pending.answer(response);
            }
            other => tracing::debug!("unexpected on the primary: {other:?}"),
        }
    }

    /// The primary's message, on a satellite. Its link has checked it may come here now.
    fn satellite_got(&mut self, id: u64, msg: Message) {
        match msg {
            Message::Welcome { renames, foreign } | Message::Renames { renames, foreign } => {
                let failed = self.stacks.apply_session(&renames, foreign);
                self.arrange();
                let Some(Net {
                    side: Side::Satellite(s),
                    conns,
                    error,
                    name,
                    ..
                }) = &mut self.net
                else {
                    return;
                };
                if !failed.is_empty() {
                    tracing::info!("renames that failed here ({failed:?}): reporting again");
                }
                // Told again what this machine has, so the session sees anything that changed meanwhile.
                s.reported = None;
                if let Some(conn) = conns.get_mut(&id)
                    && conn.joined.is_none()
                {
                    tracing::info!(
                        "joined the session at {} as {name:?} (renumbered: {renames:?})",
                        s.primary
                    );
                    conn.joined = Some(s.primary.clone());
                    s.backoff = BACKOFF_FIRST;
                    *error = None;
                }
            }
            Message::Command { id: command, request } => {
                let response = self.handle_request(request);
                let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) else {
                    return;
                };
                if let Err(SendError::TooLarge { len }) = send(conn, &Message::CommandAnswer { id: command, response })
                {
                    let response = Response::error(format!("the answer is {len} bytes: too large for the link"));
                    let _ = send(conn, &Message::CommandAnswer { id: command, response });
                }
            }
            msg if msg.is_input() => {
                let answer = self.remote_input(msg);
                let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) else {
                    return;
                };
                conn.unacked += 1;
                match answer {
                    Ok(Some(answer)) => {
                        let _ = send(conn, &answer);
                    }
                    Ok(None) => {}
                    // Dropped by the tick, which releases what the primary's input has down here.
                    Err(why) => conn.link.protocol_error(why),
                }
            }
            other => tracing::debug!("unexpected on a satellite: {other:?}"),
        }
    }

    /// Writes what connection `id` has queued, as much as the socket takes; past that, a writer source takes the
    /// rest as it drains. True when nothing is left (or nothing can be written any more).
    fn link_flush(&mut self, id: u64) -> bool {
        let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) else {
            return true;
        };
        while !conn.link.outgoing().is_empty() {
            match (&conn.stream).write(conn.link.outgoing()) {
                Ok(0) => {
                    conn.dropped = Some("the connection takes no more".into());
                    return true;
                }
                Ok(n) => conn.link.wrote(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    conn.dropped = Some(format!("the connection failed: {e}"));
                    return true;
                }
            }
        }
        if conn.link.outgoing().is_empty() {
            return true;
        }
        if conn.writer.is_none() {
            let stream = match conn.stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    conn.dropped = Some(format!("the connection failed: {e}"));
                    return true;
                }
            };
            let writer = self.loop_handle.insert_source(
                Generic::new(stream, Interest::WRITE, Mode::Level),
                move |_, _, state| {
                    if !state.link_flush(id) {
                        return Ok(PostAction::Continue);
                    }
                    if let Some(conn) = state.net.as_mut().and_then(|n| n.conns.get_mut(&id)) {
                        conn.writer = None;
                    }
                    Ok(PostAction::Remove)
                },
            );
            let Some(conn) = self.net.as_mut().and_then(|n| n.conns.get_mut(&id)) else {
                return true;
            };
            match writer {
                Ok(t) => conn.writer = Some(t),
                Err(e) => conn.dropped = Some(format!("cannot watch the connection: {e}")),
            }
        }
        false
    }

    /// Drops every connection that is done: closed with nothing left to write, or given up on.
    fn link_reap(&mut self) {
        let Some(net) = &mut self.net else {
            return;
        };
        let done: Vec<u64> = (net.conns.iter())
            .filter(|(_, c)| c.dropped.is_some() || c.link.is_finished())
            .map(|(&id, _)| id)
            .collect();
        for id in done {
            self.link_drop(id, None);
        }
    }

    /// Drops connection `id` now, for `why` (else the reason it was given up on, or its link closed for).
    fn link_drop(&mut self, id: u64, why: Option<String>) {
        let Some(conn) = self.net.as_mut().and_then(|n| n.conns.remove(&id)) else {
            return;
        };
        for t in [conn.io, conn.writer, conn.deadline].into_iter().flatten() {
            self.loop_handle.remove(t);
        }
        let why = why.unwrap_or_else(|| match (&conn.dropped, conn.link.closed()) {
            (Some(why), _) => why.clone(),
            (None, Some(closed)) => closed.to_string(),
            (None, None) => "closed".into(),
        });
        self.link_lost(conn, why);
    }

    /// A Hello on connection `id` names `name`, held by another link silent for `STALE`: that machine is gone (its
    /// link not dropped yet), so that link is dropped now and the newcomer joins in its place.
    fn link_take_over(&mut self, id: u64, name: &str) {
        let Some(net) = &self.net else {
            return;
        };
        let stale = (net.conns.iter())
            .find(|(other, c)| **other != id && c.joined.as_deref() == Some(name) && c.heard.elapsed() > STALE)
            .map(|(&other, _)| other);
        if let (Some(stale), Some(peer)) = (stale, net.conns.get(&id).map(|c| c.peer)) {
            self.link_drop(stale, Some(format!("replaced by a new connection from {peer}")));
        }
    }

    fn link_lost(&mut self, conn: Conn, why: String) {
        let Some(net) = &mut self.net else {
            return;
        };
        match &mut net.side {
            Side::Primary(p) => match conn.joined {
                Some(name) => {
                    tracing::info!("{name} left the session: {why}");
                    p.session.drop_satellite(&name);
                    // Its outbox went with the connection; input there comes home.
                    self.input_lost(&name);
                    let Some(Net {
                        side: Side::Primary(p), ..
                    }) = &mut self.net
                    else {
                        return;
                    };
                    let lost: Vec<u64> = (p.commands.iter())
                        .filter(|(_, c)| c.machine == name)
                        .map(|(&id, _)| id)
                        .collect();
                    let lost: Vec<Pending> = lost.iter().filter_map(|id| p.commands.remove(id)).collect();
                    for pending in lost {
                        if let Some(t) = pending.timer {
                            self.loop_handle.remove(t);
                        }
                        let response = Response::error(format!("the link to {name} closed before it answered"));
                        Pending { timer: None, ..pending }.answer(response);
                    }
                }
                None => {
                    // The same failure from the same address again (a satellite retrying) is logged quietly.
                    let error = format!("{}: {why}", conn.peer.ip());
                    if net.error.as_ref() == Some(&error) {
                        tracing::debug!("a connection from {} did not join: {why}", conn.peer);
                    } else {
                        tracing::warn!("a connection from {} did not join: {why}", conn.peer);
                    }
                    net.error = Some(error);
                }
            },
            Side::Satellite(s) => {
                let joined = conn.joined.is_some();
                if joined {
                    tracing::warn!("lost the primary at {}: {why}", s.primary);
                    s.backoff = BACKOFF_FIRST;
                    self.stacks.clear_foreign();
                }
                // Nothing more comes from the primary's keyboard and pointer: what they have down goes up.
                self.remote_lost();
                if joined {
                    self.arrange();
                }
                self.link_retry(why);
            }
        }
    }

    /// A satellite's attempt failed for `why` (or its link went): the next one after the backoff.
    fn link_retry(&mut self, why: String) {
        let Some(Net {
            side: Side::Satellite(s),
            error,
            ..
        }) = &mut self.net
        else {
            return;
        };
        if error.as_ref() == Some(&why) {
            tracing::debug!("the primary at {}: {why}", s.primary);
        } else {
            tracing::warn!("the primary at {}: {why}; trying again", s.primary);
        }
        *error = Some(why);
        s.reported = None;
        let wait = s.backoff;
        s.backoff = (s.backoff * 2).min(BACKOFF_MAX);
        let later = self
            .loop_handle
            .insert_source(Timer::from_duration(wait), |_, _, state| {
                state.link_connect();
                TimeoutAction::Drop
            });
        if let Err(e) = later {
            tracing::error!("cannot retry the link: {e}");
        }
    }

    /// A satellite tries the primary, on a thread of its own (resolving and connecting may take a while).
    fn link_connect(&mut self) {
        let Some(Net {
            side: Side::Satellite(s),
            secret,
            ..
        }) = &self.net
        else {
            return;
        };
        if secret.is_none() {
            return self.link_retry("no secret (see config_error)".into());
        }
        let (addr, results) = (s.primary.clone(), s.connected.clone());
        // The thread inherits this one's signal mask (calloop's signals blocked), so they never land there.
        let started = std::thread::Builder::new()
            .name(format!("{NAME}-connect"))
            .spawn(move || {
                let _ = results.send(connect(&addr));
            });
        if let Err(e) = started {
            self.link_retry(format!("cannot start connecting: {e}"));
        }
    }

    /// A satellite's attempt is over: a connection gets a link and says Hello; a failure tries again later.
    fn link_connected(&mut self, result: Result<TcpStream, String>) {
        let stream = match result {
            Ok(s) => s,
            Err(why) => return self.link_retry(why),
        };
        let Some(net) = &mut self.net else {
            return;
        };
        let Some(secret) = &net.secret else {
            return self.link_retry("no secret (see config_error)".into());
        };
        let Ok(peer) = stream.peer_addr() else {
            return self.link_retry("the connection closed at once".into());
        };
        let mut link = Link::new(Role::Satellite, secret);
        let (screens, workspaces) = report(&self.stacks);
        let hello = Message::Hello {
            version: PROTOCOL_VERSION,
            name: net.name.clone(),
            screens,
            workspaces,
        };
        if let Err(e) = link.send(&hello) {
            return self.link_retry(format!("cannot say hello: {e}"));
        }
        if self.link_add(stream, peer, link).is_none() {
            self.link_retry("cannot watch the connection".into());
        }
    }

    /// Every `PING`: each joined link is pinged, and one silent for `SILENCE` is dropped.
    fn link_ping(&mut self) {
        let Some(net) = &mut self.net else {
            return;
        };
        let now = Instant::now();
        for conn in net.conns.values_mut() {
            if now.saturating_duration_since(conn.heard) > SILENCE {
                conn.dropped = Some(format!("nothing heard for {} s", SILENCE.as_secs()));
            } else if conn.joined.is_some() && conn.ping.is_none() {
                let nonce = net.next_nonce;
                net.next_nonce += 1;
                if send(conn, &Message::Ping { nonce }).is_ok() {
                    conn.ping = Some((nonce, now));
                }
            }
        }
    }

    /// Tells the session what this machine has if that changed, and each machine the numbers the others own if
    /// those changed.
    fn link_report(&mut self) {
        let current = report(&self.stacks);
        let Some(net) = &mut self.net else {
            return;
        };
        match &mut net.side {
            Side::Primary(p) => {
                if p.reported.as_ref() != Some(&current) {
                    let (screens, numbers) = current.clone();
                    p.reported = Some(current);
                    match p.session.report(&net.name, screens, numbers) {
                        Ok(renames) if renames.is_empty() => {}
                        Ok(renames) => {
                            // Another machine took some of these meanwhile: renamed here, then told again.
                            let failed = self.stacks.apply_session(&renames, p.session.foreign(&net.name));
                            tracing::info!("renumbered here: {renames:?} (failed: {failed:?})");
                            p.reported = None;
                            self.arrange();
                        }
                        Err(e) => tracing::warn!("this machine's workspaces: {e}"),
                    }
                }
                let Some(Net {
                    side: Side::Primary(p),
                    conns,
                    name,
                    ..
                }) = &mut self.net
                else {
                    return;
                };
                let mine = p.session.foreign(name);
                if &mine != self.stacks.foreign() {
                    self.stacks.set_foreign(mine);
                }
                for conn in conns.values_mut() {
                    let Some(machine) = &conn.joined else {
                        continue;
                    };
                    let foreign = p.session.foreign(machine);
                    if foreign != conn.foreign {
                        let _ = send(
                            conn,
                            &Message::Renames {
                                renames: Vec::new(),
                                foreign: foreign.clone(),
                            },
                        );
                        conn.foreign = foreign;
                    }
                }
            }
            Side::Satellite(s) => {
                if s.reported.as_ref() == Some(&current) {
                    return;
                }
                let Some(conn) = net.conns.values_mut().find(|c| c.joined.is_some()) else {
                    return;
                };
                let (screens, workspaces) = current.clone();
                if send(conn, &Message::WorkspaceState { screens, workspaces }).is_ok() {
                    s.reported = Some(current);
                }
            }
        }
    }

    /// Sends `request` to satellite `machine`, its answer for `reply`; without one it is a routed `workspace`, a
    /// failure logged and resynced. Some(error) when it cannot be sent; None when sent: the answer, or an error
    /// after `FORWARD_TIMEOUT`, comes later.
    fn link_command(&mut self, machine: &str, request: Request, reply: Option<Reply>) -> Option<Response> {
        let handle = self.loop_handle.clone();
        let Some(Net {
            side: Side::Primary(p),
            conns,
            ..
        }) = &mut self.net
        else {
            return Some(Response::error("only the primary sends requests to other machines"));
        };
        let Some(conn) = conns.values_mut().find(|c| c.joined.as_deref() == Some(machine)) else {
            return Some(Response::error(format!("no machine named {machine:?} in the session")));
        };
        let id = p.next_command;
        p.next_command += 1;
        if let Err(e) = send(conn, &Message::Command { id, request }) {
            return Some(Response::error(format!("cannot send to {machine}: {e}")));
        }
        let timer = handle.insert_source(Timer::from_duration(FORWARD_TIMEOUT), move |_, _, state| {
            if let Some(Net {
                side: Side::Primary(p),
                conns,
                ..
            }) = &mut state.net
                && let Some(pending) = p.commands.remove(&id)
            {
                if pending.resync
                    && let Some(conn) = conns.values_mut().find(|c| c.joined.as_ref() == Some(&pending.machine))
                {
                    resync(conn, p.session.foreign(&pending.machine));
                }
                let response = Response::error(format!(
                    "no answer from {} within {} s",
                    pending.machine,
                    FORWARD_TIMEOUT.as_secs()
                ));
                Pending { timer: None, ..pending }.answer(response);
            }
            TimeoutAction::Drop
        });
        let pending = Pending {
            machine: machine.to_owned(),
            resync: reply.is_none(),
            reply,
            timer: timer.ok(),
        };
        p.commands.insert(id, pending);
        None
    }

    /// `request` run on machine `machine` (`orrery msg --to`): here when it names this machine, else sent there by
    /// the primary. Some(answer) now, or None: `reply` is called once, later, with the other machine's answer.
    pub fn link_forward(&mut self, machine: &str, request: Request, reply: Reply) -> Option<Response> {
        if matches!(request, Request::To { .. }) {
            return Some(Response::error(
                "`to` takes one machine: its request cannot be another `to`",
            ));
        }
        let Some(net) = &self.net else {
            return Some(Response::error(
                "not in a session: the config has neither `listen` nor `primary`",
            ));
        };
        if net.name == machine {
            return Some(self.handle_request(request));
        }
        if let Side::Satellite(_) = net.side {
            return Some(Response::error(format!(
                "this is {:?}, a satellite: requests to other machines go from the primary",
                net.name
            )));
        }
        self.link_command(machine, request, Some(reply))
    }

    /// `inject-remote` (a testing aid; the caller checked `--debug-inject`): `message`, an input message, sent as it
    /// is to satellite `machine`, as forwarding would.
    pub(super) fn inject_remote(&mut self, machine: &str, message: &Message) -> Response {
        let Some(Net {
            side: Side::Primary(_),
            conns,
            ..
        }) = &mut self.net
        else {
            return Response::error("`inject-remote` sends from the primary: this is no primary");
        };
        let Some(conn) = conns.values_mut().find(|c| c.joined.as_deref() == Some(machine)) else {
            return Response::error(format!("no machine named {machine:?} in the session"));
        };
        match send(conn, message) {
            Ok(()) => Response::ok(),
            Err(e) => Response::error(format!("cannot send to {machine}: {e}")),
        }
    }

    /// `workspace n` in a session: shown here, or by the machine that has it (the session's focus marker moves to
    /// it, and the primary's input with it), or made on the focused machine. A satellite asks the primary for a
    /// number another machine has (`SessionRequest`), answered ok once sent.
    pub(super) fn session_workspace(&mut self, n: u8) -> Response {
        let here = Request::Workspace { number: n };
        let Some(net) = &mut self.net else {
            return self.apply(here);
        };
        let route = match &mut net.side {
            Side::Satellite(_) if self.stacks.foreign().contains(&n) => {
                let Some(conn) = net.conns.values_mut().find(|c| c.joined.is_some()) else {
                    return Response::error(format!("workspace {n} is on another machine, and the primary is gone"));
                };
                return match send(conn, &Message::SessionRequest { request: here }) {
                    Ok(()) => Response::ok(),
                    Err(e) => Response::error(format!("cannot ask the primary for workspace {n}: {e}")),
                };
            }
            Side::Satellite(_) => return self.apply(here),
            Side::Primary(p) => p.session.workspace(n),
        };
        let response = match route {
            Err(e) => Response::error(e.to_string()),
            Ok(Route::Local) => self.apply(here),
            Ok(Route::Create(m)) if m == net.name => self.apply(here),
            Ok(Route::On(m) | Route::Create(m)) => self.link_command(&m, here, None).unwrap_or_else(Response::ok),
        };
        self.input_follow_marker();
        response
    }

    /// Satellite `from` asks the primary to route `request` (a `workspace` it does not have): as the primary's own.
    fn session_request(&mut self, from: &str, request: Request) {
        let response = match request {
            Request::Workspace { number } => self.session_workspace(number),
            other => Response::error(format!("{} is no session request", other.verb())),
        };
        if let Some(e) = response.error {
            tracing::info!("{from}'s request: {e}");
        }
    }

    /// The primary's crossing: where its input goes.
    pub(super) fn crossing(&mut self) -> Option<&mut Crossing> {
        match &mut self.net {
            Some(Net {
                side: Side::Primary(p), ..
            }) => Some(&mut p.crossing),
            _ => None,
        }
    }

    /// Where the primary's input goes; None elsewhere.
    pub(super) fn input_place(&self) -> Option<&Place> {
        match &self.net {
            Some(Net {
                side: Side::Primary(p), ..
            }) => Some(p.crossing.place()),
            _ => None,
        }
    }

    /// This machine's name in a session.
    pub(super) fn this_machine(&self) -> Option<String> {
        self.net.as_ref().map(|n| n.name.clone())
    }

    /// The machine holding the session's focus marker (the primary's view).
    pub(super) fn marker(&self) -> Option<String> {
        match &self.net {
            Some(Net {
                side: Side::Primary(p), ..
            }) => Some(p.session.focused().name().to_owned()),
            _ => None,
        }
    }

    pub(super) fn focus_marker(&mut self, machine: &str) {
        if let Some(Net {
            side: Side::Primary(p), ..
        }) = &mut self.net
        {
            p.session.focus(machine);
        }
    }

    /// Where machine `from`'s pointer lands, leaving its screens past `edge` at height `y`: the session's monitors
    /// in a row (satellites' sizes never cross the link, and landing needs none).
    pub(super) fn landing(&self, from: &str, edge: Edge, y: f64) -> Option<Landing> {
        let Some(Net {
            side: Side::Primary(p), ..
        }) = &self.net
        else {
            return None;
        };
        let row: Vec<RowScreen> = (p.session.monitors())
            .map(|(m, s)| RowScreen {
                machine: m.name().to_owned(),
                screen: s.name.clone(),
                width: 0,
                height: 0,
            })
            .collect();
        crate::crossing::landing(&row, from, edge, y)
    }

    /// Queues input for satellite `machine` (dropped when it is gone: its drop brings input home).
    pub(super) fn queue_input(&mut self, machine: &str, msg: Message) {
        if let Some(conn) = self.joined_conn(machine) {
            conn.outbox.push(msg);
        }
    }

    /// Queues `keymap` (this machine's, when that satellite was not sent it yet) and `handoff` for `machine`; false
    /// when it is gone.
    pub(super) fn queue_handoff(&mut self, machine: &str, keymap: Option<String>, handoff: Message) -> bool {
        let Some(conn) = self.joined_conn(machine) else {
            return false;
        };
        if let Some(keymap) = keymap
            && conn.keymap.as_ref() != Some(&keymap)
        {
            conn.outbox.push(Message::Keymap {
                xkb: crate::link::Xkb(keymap.clone()),
            });
            conn.keymap = Some(keymap);
        }
        conn.outbox.push(handoff);
        true
    }

    fn joined_conn(&mut self, machine: &str) -> Option<&mut Conn> {
        (self.net.as_mut()?.conns.values_mut()).find(|c| c.joined.as_deref() == Some(machine) && c.dropped.is_none())
    }

    /// `move-to-workspace n` in a session: refused for another machine's number (windows never cross machines).
    pub(super) fn session_move(&mut self, n: u8) -> Response {
        let here = Request::MoveToWorkspace { number: n };
        let refused = match &self.net {
            None => None,
            Some(Net {
                side: Side::Primary(p),
                name,
                ..
            }) => p.session.move_to_workspace(name, n).err().map(|e| e.to_string()),
            Some(_) => (self.stacks.foreign().contains(&n))
                .then(|| format!("workspace {n} is on another machine: windows never cross machines")),
        };
        match refused {
            Some(e) => Response::error(e),
            None => self.apply(here),
        }
    }

    /// `state`'s session: None alone.
    pub(super) fn session_view(&self) -> Option<SessionView> {
        let net = self.net.as_ref()?;
        let foreign = self.stacks.foreign().iter().copied().collect();
        let (listen, primary, machines) = match &net.side {
            Side::Primary(p) => {
                let latency = |name: &str| {
                    (net.conns.values())
                        .find(|c| c.joined.as_deref() == Some(name))
                        .and_then(latency_ms)
                };
                let machines = (p.session.machines().iter())
                    .map(|m| MachineView {
                        name: m.name().to_owned(),
                        role: m.role(),
                        monitors: m.screens().to_vec(),
                        workspaces: m.workspaces().iter().copied().collect(),
                        latency_ms: latency(m.name()),
                        connected: true,
                        focused: p.session.focused().name() == m.name(),
                        queued: (net.conns.values())
                            .find(|c| c.joined.as_deref() == Some(m.name()))
                            .map_or(0, |c| c.outbox.len()),
                    })
                    .collect();
                (p.listen.map(|a| a.to_string()), None, machines)
            }
            Side::Satellite(s) => {
                let conn = net.conns.values().find(|c| c.joined.is_some());
                let (monitors, workspaces) = report(&self.stacks);
                let me = MachineView {
                    name: net.name.clone(),
                    role: Role::Satellite,
                    monitors,
                    workspaces: workspaces.into_iter().collect(),
                    latency_ms: conn.and_then(latency_ms),
                    connected: conn.is_some(),
                    focused: false,
                    queued: 0,
                };
                (None, Some(s.primary.clone()), vec![me])
            }
        };
        Some(SessionView {
            machine: net.name.clone(),
            role: match net.side {
                Side::Primary(_) => Role::Primary,
                Side::Satellite(_) => Role::Satellite,
            },
            listen,
            primary,
            machines,
            input: match &net.side {
                Side::Primary(p) => match p.crossing.place() {
                    Place::Local => net.name.clone(),
                    Place::Remote(m) => m.clone(),
                },
                Side::Satellite(_) => net.name.clone(),
            },
            foreign,
            error: net.error.clone(),
        })
    }
}
