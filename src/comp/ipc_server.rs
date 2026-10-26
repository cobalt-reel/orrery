//! The control socket: `orrery msg` connects, sends one JSON `Request` line, gets one `Response` line back, and
//! the connection closes. Everything runs on calloop, with no threads and nothing blocking: a connection is a
//! source that reads until its line is complete, then (if the answer does not fit the socket at once) a source
//! that writes the rest. Every connection has a total deadline, and only a few may be open at once.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, bail};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};

use super::Orrery;
use crate::ipc::{Request, Response};

/// A request line longer than this is refused unread.
pub const MAX_REQUEST: usize = 64 * 1024;
/// A connection is dropped this long after it was accepted, answered or not. The deadline is total, so it also
/// cuts off a reader too slow to take a huge answer (a `state` with very many long titles) within it; `orrery
/// msg` then reports a parse error for the truncated line. Accepted: a real client reads it at once.
pub const DEADLINE: Duration = Duration::from_secs(2);
/// Connections open at once; one more is answered "busy" and closed.
pub const MAX_CONNECTIONS: usize = 16;
/// Out of file descriptors: stop accepting for this long instead of spinning on a ready listener.
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// How long `orrery msg` waits for orrery.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// `$XDG_RUNTIME_DIR/orrery.sock`.
#[must_use]
pub fn default_socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("orrery.sock"))
}

/// The bound socket and the lock that makes it ours. Dropping it (a clean exit) removes the socket, if the file
/// at the path is still the one bound here.
pub struct ControlSocket {
    listener: Option<UnixListener>,
    path: PathBuf,
    /// (dev, ino) of the socket bound here.
    id: (u64, u64),
    /// `<path>.lock`, flocked for the process's lifetime (released by the kernel however orrery ends).
    _lock: File,
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        match std::fs::symlink_metadata(&self.path) {
            Ok(m) if (m.dev(), m.ino()) == self.id => {
                if let Err(e) = std::fs::remove_file(&self.path) {
                    tracing::warn!("cannot remove {}: {e}", self.path.display());
                }
            }
            Ok(_) => tracing::warn!(
                "{} is no longer the socket orrery bound; left alone",
                self.path.display()
            ),
            Err(_) => {}
        }
    }
}

/// The socket's directory must not let anyone else replace what is in it (ssh-agent's rule): owned by us, and
/// not group/other-writable unless sticky.
fn check_dir(mode: u32, owner: u32, me: u32) -> Result<(), String> {
    if owner != me {
        return Err(format!("owned by uid {owner}, not by us (uid {me})"));
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Err(format!(
            "group or other writable (mode {:o}) and not sticky",
            mode & 0o7777
        ));
    }
    Ok(())
}

/// Binds `path` mode 0600, after checking its directory and taking `<path>.lock`. A held lock or a live orrery
/// answering at the path is a refusal; a stale socket (nobody listening) is replaced; anything that is not a
/// socket is left alone and refused.
pub fn bind(path: &Path) -> anyhow::Result<ControlSocket> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let meta = std::fs::metadata(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    // Safety: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if let Err(why) = check_dir(meta.mode(), meta.uid(), me) {
        bail!("refusing a control socket in {}: {why}", dir.display());
    }

    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("cannot open {}", Path::new(&lock_path).display()))?;
    // Safety: a valid fd for the life of `lock`.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("orrery already running (it holds {})", Path::new(&lock_path).display());
    }

    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if UnixStream::connect(path).is_ok() {
            bail!("orrery already running (its control socket {} answers)", path.display());
        }
        if !meta.file_type().is_socket() {
            bail!("{} exists and is not a socket; not touching it", path.display());
        }
        tracing::info!("replacing the stale control socket {}", path.display());
        std::fs::remove_file(path).with_context(|| format!("cannot remove the stale {}", path.display()))?;
    }
    // The mode comes from umask at bind time, so the socket is never connectable by others, even briefly.
    // Safety: umask touches no memory. It is process-wide, so it would race with another thread creating
    // files; orrery calls this at startup, while it is still single-threaded.
    let old = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(path);
    unsafe { libc::umask(old) };
    let listener = bound.with_context(|| format!("cannot listen on {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("cannot make the control socket non-blocking")?;
    let m = std::fs::symlink_metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    Ok(ControlSocket {
        listener: Some(listener),
        path: path.to_owned(),
        id: (m.dev(), m.ino()),
        _lock: lock,
    })
}

/// The live connections: the source currently serving each (reader, then maybe writer) and its deadline timer.
#[derive(Default)]
struct Conns {
    next: u64,
    live: HashMap<u64, Live>,
}

struct Live {
    io: Option<RegistrationToken>,
    deadline: Option<RegistrationToken>,
}

type Registry = Rc<RefCell<Conns>>;

/// A connection is over: its deadline timer goes (the io source removes itself by returning Remove).
fn finish(handle: &LoopHandle<'static, Orrery>, conns: &Registry, id: u64) {
    if let Some(live) = conns.borrow_mut().live.remove(&id)
        && let Some(timer) = live.deadline
    {
        handle.remove(timer);
    }
}

/// What to do when accept fails: back off on fd exhaustion (the listener stays ready, so retrying at once
/// would spin), carry on otherwise.
fn accept_backoff(e: &std::io::Error) -> Option<Duration> {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
    .then_some(ACCEPT_BACKOFF)
}

impl ControlSocket {
    /// Accepts connections on the loop from now on.
    pub fn serve(&mut self, handle: &LoopHandle<'static, Orrery>) -> anyhow::Result<()> {
        let listener = self.listener.take().context("the control socket is already served")?;
        let conns: Registry = Rc::default();
        let own_token: Rc<Cell<Option<RegistrationToken>>> = Rc::default();
        let h = handle.clone();
        let token = own_token.clone();
        let registered = handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                move |_, listener, _| {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => accept(&h, &conns, stream),
                            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == ErrorKind::Interrupted => {}
                            Err(e) => {
                                let Some(pause) = accept_backoff(&e) else {
                                    tracing::warn!("control socket accept: {e}");
                                    break;
                                };
                                tracing::warn!("control socket accept: {e}; pausing {pause:?}");
                                if let Some(t) = token.get() {
                                    let h2 = h.clone();
                                    let resumed = h.insert_source(Timer::from_duration(pause), move |_, _, _| {
                                        if let Err(e) = h2.enable(&t) {
                                            tracing::error!("cannot resume the control socket: {e}");
                                        }
                                        TimeoutAction::Drop
                                    });
                                    if resumed.is_ok() {
                                        return Ok(PostAction::Disable);
                                    }
                                }
                                break;
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("cannot watch the control socket: {e}"))?;
        own_token.set(Some(registered));
        Ok(())
    }
}

/// What a connection has sent so far. Newlines are searched for once per byte, however it arrives.
#[derive(Debug, Default)]
struct LineReader {
    buf: Vec<u8>,
    /// `buf[..scanned]` holds no newline.
    scanned: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// Not a whole line yet: read more.
    Partial,
    /// The request, or the error answer for a bad one.
    Done(Result<Request, String>),
}

impl LineReader {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn full(&self) -> bool {
        self.buf.len() > MAX_REQUEST
    }

    /// The request once its line is complete (`eof`: nothing more will come, so the rest is the line).
    fn take(&mut self, eof: bool) -> Line {
        let end = match self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
            Some(i) => self.scanned + i,
            None => {
                self.scanned = self.buf.len();
                if !self.full() && !eof {
                    return Line::Partial;
                }
                self.buf.len()
            }
        };
        if end > MAX_REQUEST {
            return Line::Done(Err("bad request: longer than 64 KiB".into()));
        }
        Line::Done(serde_json::from_slice(&self.buf[..end]).map_err(|e| format!("bad request: {e}")))
    }
}

fn encode(response: &Response) -> Vec<u8> {
    let mut line = serde_json::to_vec(response).unwrap_or_else(|e| {
        tracing::warn!("cannot encode a response: {e}");
        br#"{"ok":false,"error":"cannot encode the response"}"#.to_vec()
    });
    line.push(b'\n');
    line
}

/// Writes as much of `out[*done..]` as fits now. Ok(true) when all is written.
fn write_some(stream: &UnixStream, out: &[u8], done: &mut usize) -> std::io::Result<bool> {
    while *done < out.len() {
        match (&*stream).write(&out[*done..]) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => *done += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn accept(handle: &LoopHandle<'static, Orrery>, conns: &Registry, stream: UnixStream) {
    if let Err(e) = stream.set_nonblocking(true) {
        tracing::warn!("control connection: {e}");
        return;
    }
    if conns.borrow().live.len() >= MAX_CONNECTIONS {
        // One attempt, never waiting: the answer fits an empty socket buffer.
        let _ = write_some(&stream, &encode(&Response::error("busy")), &mut 0);
        return;
    }
    let id = {
        let mut c = conns.borrow_mut();
        c.next += 1;
        let id = c.next;
        c.live.insert(
            id,
            Live {
                io: None,
                deadline: None,
            },
        );
        id
    };

    let mut reader = LineReader::default();
    let (h, registry) = (handle.clone(), conns.clone());
    let io = handle.insert_source(
        Generic::new(stream, Interest::READ, Mode::Level),
        move |_, stream, state| {
            let stream: &UnixStream = stream;
            // Never an error out of here: calloop would stop the whole loop with it.
            let mut chunk = [0u8; 4096];
            let mut eof = false;
            loop {
                match (&*stream).read(&mut chunk) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        reader.push(&chunk[..n]);
                        if reader.full() || chunk[..n].contains(&b'\n') {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => {
                        tracing::debug!("control connection read: {e}");
                        finish(&h, &registry, id);
                        return Ok(PostAction::Remove);
                    }
                }
            }
            let response = match reader.take(eof) {
                Line::Partial => return Ok(PostAction::Continue),
                Line::Done(Ok(request)) => state.handle_request(request),
                Line::Done(Err(_)) if eof && reader.buf.is_empty() => {
                    // Connected and left without a word: nobody to answer.
                    finish(&h, &registry, id);
                    return Ok(PostAction::Remove);
                }
                Line::Done(Err(e)) => Response::error(e),
            };
            let out = encode(&response);
            let mut done = 0;
            match write_some(stream, &out, &mut done) {
                Ok(false) => {
                    // This source runs only after `accept` set its deadline, and the deadline removes this source
                    // with the entry, so the entry is there for the writer to hand over to.
                    debug_assert!(
                        registry.borrow().live.get(&id).is_some_and(|l| l.deadline.is_some()),
                        "a live connection without a deadline"
                    );
                    // The rest goes out when the socket is writable again, on a source of its own.
                    if let Ok(writer) = stream.try_clone()
                        && let Some(token) = write_later(&h, &registry, id, writer, out, done)
                        && let Some(live) = registry.borrow_mut().live.get_mut(&id)
                    {
                        live.io = Some(token);
                        return Ok(PostAction::Remove);
                    }
                    finish(&h, &registry, id);
                }
                Ok(true) => finish(&h, &registry, id),
                Err(e) => {
                    tracing::debug!("control connection write: {e}");
                    finish(&h, &registry, id);
                }
            }
            Ok(PostAction::Remove)
        },
    );
    let Ok(io) = io else {
        tracing::warn!("cannot watch a control connection");
        conns.borrow_mut().live.remove(&id);
        return;
    };

    let (h, registry) = (handle.clone(), conns.clone());
    let deadline = handle.insert_source(Timer::from_duration(DEADLINE), move |_, _, _| {
        // Still live: whatever serves it now goes, with the connection.
        if let Some(live) = registry.borrow_mut().live.remove(&id)
            && let Some(io) = live.io
        {
            tracing::debug!("control connection {id} timed out");
            h.remove(io);
        }
        TimeoutAction::Drop
    });
    let mut c = conns.borrow_mut();
    match (deadline, c.live.get_mut(&id)) {
        (Ok(timer), Some(live)) => {
            live.io = Some(io);
            live.deadline = Some(timer);
        }
        (Ok(timer), None) => handle.remove(timer),
        (Err(_), _) => {
            // No deadline, no connection.
            tracing::warn!("cannot time a control connection");
            c.live.remove(&id);
            handle.remove(io);
        }
    }
}

/// A source writing the rest of an answer as the socket drains; removed when done, on error, or by the deadline.
fn write_later(
    handle: &LoopHandle<'static, Orrery>,
    conns: &Registry,
    id: u64,
    stream: UnixStream,
    out: Vec<u8>,
    mut done: usize,
) -> Option<RegistrationToken> {
    let (h, registry) = (handle.clone(), conns.clone());
    handle
        .insert_source(
            Generic::new(stream, Interest::WRITE, Mode::Level),
            move |_, stream, _| {
                match write_some(stream, &out, &mut done) {
                    Ok(false) => return Ok(PostAction::Continue),
                    Ok(true) => {}
                    Err(e) => tracing::debug!("control connection write: {e}"),
                }
                finish(&h, &registry, id);
                Ok(PostAction::Remove)
            },
        )
        .ok()
}

/// `orrery msg`: one request to the orrery at `path`, its answer back.
pub fn send(path: &Path, request: &Request) -> anyhow::Result<Response> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("cannot reach orrery at {} (is it running?)", path.display()))?;
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .context("socket timeout")?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .context("socket timeout")?;
    let mut line = serde_json::to_vec(request).context("encoding the request")?;
    line.push(b'\n');
    // orrery may answer (a refusal) and close before taking all of a request: its answer beats our error.
    let sent = stream.write_all(&line);
    let answer = read_answer(&mut stream);
    let answer = match (sent, answer) {
        (_, Ok(a)) if a.contains(&b'\n') => a,
        (Err(e), _) => return Err(e).context("sending the request"),
        (Ok(()), a) => a.context("reading the answer")?,
    };
    let first = answer.split(|&b| b == b'\n').next().unwrap_or_default();
    serde_json::from_slice(first).with_context(|| {
        format!(
            "orrery answered something that is not a response: {:?}",
            String::from_utf8_lossy(first)
        )
    })
}

/// Reads up to EOF. A reset after the answer line still yields it: orrery closes a refused (oversized) request
/// with input unread, and the kernel then reports a reset once the answer has been read.
fn read_answer(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(answer),
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) if answer.contains(&b'\n') => return Ok(answer),
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(bytes: &[u8], eof: bool) -> Line {
        let mut r = LineReader::default();
        r.push(bytes);
        r.take(eof)
    }

    #[test]
    fn lines_complete_on_newline_or_eof() {
        assert_eq!(take(br#"{"cmd":"st"#, false), Line::Partial);
        assert_eq!(take(b"{\"cmd\":\"state\"}\n", false), Line::Done(Ok(Request::State)));
        assert_eq!(take(br#"{"cmd":"quit"}"#, true), Line::Done(Ok(Request::Quit)));
        // Only the first line counts.
        assert_eq!(
            take(b"{\"cmd\":\"close\"}\n{\"cmd\":\"quit\"}\n", false),
            Line::Done(Ok(Request::Close))
        );
    }

    #[test]
    fn a_line_arriving_in_pieces_is_scanned_once() {
        let mut r = LineReader::default();
        for piece in [&b"{\"cmd\""[..], b":", b"\"state\"", b"}"] {
            r.push(piece);
            assert_eq!(r.take(false), Line::Partial);
            assert_eq!(r.scanned, r.buf.len(), "everything so far scanned, nothing again");
        }
        r.push(b"\n");
        assert_eq!(r.take(false), Line::Done(Ok(Request::State)));
    }

    #[test]
    fn bad_and_huge_requests_are_refused() {
        for bad in [
            &b"nope\n"[..],
            b"{\"cmd\":\"fly\"}\n",
            b"{\"cmd\":\"state\",\"x\":1}\n",
            b"\n",
        ] {
            let Line::Done(Err(e)) = take(bad, false) else {
                panic!("{bad:?} refused");
            };
            assert!(e.starts_with("bad request"), "{e}");
        }
        let huge = vec![b' '; MAX_REQUEST + 1];
        let Line::Done(Err(e)) = take(&huge, false) else {
            panic!("refused before its newline arrives");
        };
        assert!(e.contains("64 KiB"));
        let mut long_line = vec![b' '; MAX_REQUEST + 10];
        long_line.extend_from_slice(b"{\"cmd\":\"state\"}\n");
        assert!(matches!(take(&long_line, false), Line::Done(Err(_))));
    }

    #[test]
    fn the_socket_dir_must_be_ours_and_closed() {
        assert!(check_dir(0o40700, 1000, 1000).is_ok());
        assert!(check_dir(0o40755, 1000, 1000).is_ok());
        assert!(check_dir(0o41777, 1000, 1000).is_ok(), "sticky and ours");
        assert!(check_dir(0o40777, 1000, 1000).unwrap_err().contains("writable"));
        assert!(check_dir(0o40770, 1000, 1000).is_err());
        assert!(check_dir(0o41777, 0, 1000).unwrap_err().contains("owned by uid 0"));
        assert!(check_dir(0o40700, 1001, 1000).is_err());
    }

    #[test]
    fn fd_exhaustion_backs_off_other_errors_do_not() {
        for errno in [libc::EMFILE, libc::ENFILE] {
            assert_eq!(
                accept_backoff(&std::io::Error::from_raw_os_error(errno)),
                Some(ACCEPT_BACKOFF)
            );
        }
        assert_eq!(
            accept_backoff(&std::io::Error::from_raw_os_error(libc::ECONNABORTED)),
            None
        );
    }

    #[test]
    fn msg_prefers_the_answer_to_its_own_write_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // A server that refuses after a little of a huge request, as orrery does past 64 KiB.
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut some = [0u8; 1024];
            s.read_exact(&mut some).unwrap();
            s.write_all(b"{\"ok\":false,\"error\":\"bad request: longer than 64 KiB\"}\n")
                .unwrap();
        });
        let huge = Request::Layout {
            name: "x".repeat(8 << 20),
            monitor: None,
        };
        let response = send(&path, &huge).expect("the answer, not the broken pipe");
        assert_eq!(response, Response::error("bad request: longer than 64 KiB"));
        server.join().unwrap();
    }

    #[test]
    fn write_some_stops_when_full_and_resumes() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let out = vec![b'x'; 4 << 20];
        let mut done = 0;
        assert!(
            !write_some(&a, &out, &mut done).unwrap(),
            "4 MiB does not fit a socket buffer"
        );
        assert!(done > 0 && done < out.len());
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            (&b).read_to_end(&mut got).unwrap();
            got.len()
        });
        while !write_some(&a, &out, &mut done).unwrap() {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(a);
        assert_eq!(reader.join().unwrap(), out.len());
    }
}
