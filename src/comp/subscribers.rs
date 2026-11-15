//! The event stream's subscribers: control connections that sent `subscribe`. Each gets a `state` event, then the
//! events of every loop iteration that changed something (`events::Summary`, taken at most every
//! `events::PUBLISH_EVERY`: a change within that waits for a timer), one JSON object per line. Writes
//! never block: what the socket does not take waits in the subscriber's backlog and goes as it drains; a backlog past
//! `MAX_BACKLOG` drops the subscriber (its reader sees EOF and may subscribe again). Unlike a request, a
//! subscription has no deadline.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::time::Instant;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};

use super::Orrery;
use super::ipc_server::write_some;
use crate::events::{Event, Summary, publish_wait};

/// Bytes a subscriber may leave unread before it is dropped.
pub const MAX_BACKLOG: usize = 1 << 20;
/// Subscribers at once; one more is refused.
pub const MAX_SUBSCRIBERS: usize = 16;

#[derive(Default)]
pub struct Subscribers {
    next: u64,
    live: BTreeMap<u64, Subscriber>,
    /// What the last events were derived from; None while nobody subscribes.
    sent: Option<Summary>,
    /// When the last summary was taken.
    last: Option<Instant>,
    /// The timer waking the loop for a summary put off by `PUBLISH_EVERY`, while one is set.
    timer: Option<RegistrationToken>,
}

struct Subscriber {
    stream: UnixStream,
    /// Not written yet: `out[done..]`.
    out: Vec<u8>,
    done: usize,
    /// Its `state` is still to come, with the next iteration's events (so both are taken at the same moment).
    fresh: bool,
    /// The source noticing that it hangs up (reading what it sends until it half-closes, then only its hang-up).
    hangup: Option<RegistrationToken>,
    /// The source writing its backlog as the socket drains, while there is one.
    writer: Option<RegistrationToken>,
}

/// How a flush left a subscriber.
#[derive(Debug, PartialEq)]
enum Flushed {
    Drained,
    /// A backlog is left, for when the socket drains.
    Waiting,
    /// Dropped (its backlog too long, or a write failed).
    Gone,
}

/// Whether the peer of `stream` has closed its end entirely (not merely shut down its writing: a subscriber may
/// say `subscribe` and close its sending side, and still read).
fn hung_up(stream: &UnixStream) -> bool {
    let mut fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // Safety: one valid pollfd, no waiting.
    let ready = unsafe { libc::poll(&raw mut fd, 1, 0) };
    ready > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// `events`, one JSON line each.
fn lines<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        match serde_json::to_writer(&mut out, event) {
            Ok(()) => out.push(b'\n'),
            Err(e) => tracing::warn!("cannot encode an event: {e}"),
        }
    }
    out
}

impl Orrery {
    /// `stream`, a control connection that sent `subscribe`, subscribes; refused past `MAX_SUBSCRIBERS`.
    pub(super) fn subscribe(&mut self, stream: UnixStream) -> Result<(), String> {
        if self.subscribers.live.len() >= MAX_SUBSCRIBERS {
            return Err(format!("too many subscribers ({MAX_SUBSCRIBERS} at once)"));
        }
        let watched = stream.try_clone().map_err(|e| format!("cannot subscribe: {e}"))?;
        self.subscribers.next += 1;
        let id = self.subscribers.next;
        let hangup = self.watch(id, watched, Interest::READ)?;
        tracing::info!("subscriber {id} subscribed");
        let subscriber = Subscriber {
            stream,
            out: Vec::new(),
            done: 0,
            fresh: true,
            hangup: Some(hangup),
            writer: None,
        };
        self.subscribers.live.insert(id, subscriber);
        Ok(())
    }

    /// A source on subscriber `id`'s socket noticing its hang-up: with `Interest::READ`, what it sends is read and
    /// ignored (a few reads a wake) up to its end; there, a hang-up drops it, while a half-close (it said `subscribe`
    /// and shut its sending side, still reading) leaves a source with no interest, which epoll still wakes on hang-up.
    fn watch(&mut self, id: u64, stream: UnixStream, interest: Interest) -> Result<RegistrationToken, String> {
        let token =
            (self.loop_handle).insert_source(Generic::new(stream, interest, Mode::Level), move |_, stream, state| {
                let stream: &UnixStream = stream;
                if interest.readable {
                    let mut chunk = [0u8; 1024];
                    let mut ended = false;
                    for _ in 0..4 {
                        match (&*stream).read(&mut chunk) {
                            Ok(0) => ended = true,
                            Ok(_) => continue,
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                            Err(_) => ended = true,
                        }
                        break;
                    }
                    if !ended {
                        return Ok(PostAction::Continue);
                    }
                } else if !hung_up(stream) {
                    return Ok(PostAction::Continue);
                }
                // This source goes by returning Remove, never by its token.
                if let Some(sub) = state.subscribers.live.get_mut(&id) {
                    sub.hangup = None;
                }
                if hung_up(stream) {
                    state.unsubscribe(id, "hung up");
                    return Ok(PostAction::Remove);
                }
                let rewatched = stream.try_clone().map_err(|e| e.to_string());
                match rewatched.and_then(|s| state.watch(id, s, Interest::EMPTY)) {
                    Ok(token) => match state.subscribers.live.get_mut(&id) {
                        Some(sub) => sub.hangup = Some(token),
                        None => state.loop_handle.remove(token),
                    },
                    Err(e) => state.unsubscribe(id, &format!("dropped: {e}")),
                }
                Ok(PostAction::Remove)
            });
        token.map_err(|e| format!("cannot watch a subscriber: {e}"))
    }

    /// Subscriber `id` goes, with its sources (its socket closes: its reader sees EOF).
    fn unsubscribe(&mut self, id: u64, why: &str) {
        let Some(sub) = self.subscribers.live.remove(&id) else {
            return;
        };
        for token in [sub.hangup, sub.writer].into_iter().flatten() {
            self.loop_handle.remove(token);
        }
        tracing::info!("subscriber {id} {why}");
    }

    /// At the end of every loop iteration: the events since the last summary (or, to a new subscriber, the `state`)
    /// to every subscriber. Within `PUBLISH_EVERY` of the last summary a timer is set for the rest instead: its wake
    /// ends an iteration too, so the last change still goes out. Nothing is looked at while nobody subscribes.
    pub fn publish_events(&mut self) {
        if self.subscribers.live.is_empty() {
            self.subscribers.sent = None;
            return;
        }
        let at = Instant::now();
        if let Some(wait) = publish_wait(self.subscribers.last, at) {
            if self.subscribers.timer.is_none() {
                let timer = (self.loop_handle).insert_source(Timer::from_duration(wait), |_, _, state| {
                    state.subscribers.timer = None;
                    TimeoutAction::Drop
                });
                match timer {
                    Ok(t) => self.subscribers.timer = Some(t),
                    Err(e) => tracing::warn!("cannot time the event stream: {e}"),
                }
            }
            return;
        }
        self.subscribers.last = Some(at);
        let view = self.summary_view();
        let now = Summary::new(&view, |w| self.stacks.is_marked(w), self.config_reads);
        let events = (self.subscribers.sent.as_ref()).map_or_else(Vec::new, |sent| sent.events(&now));
        self.subscribers.sent = Some(now);
        let fresh = self.subscribers.live.values().any(|s| s.fresh);
        if events.is_empty() && !fresh {
            return;
        }
        let changes = lines(&events);
        let state = fresh.then(|| {
            let mut full = self.state_view();
            full.last_key = None;
            lines([&Event::State(Box::new(full))])
        });
        let ids: Vec<u64> = self.subscribers.live.keys().copied().collect();
        for id in ids {
            let Some(sub) = self.subscribers.live.get_mut(&id) else {
                continue;
            };
            let out = match &state {
                Some(state) if sub.fresh => state,
                _ => &changes,
            };
            if out.is_empty() {
                continue;
            }
            sub.fresh = false;
            sub.out.extend_from_slice(out);
            match self.flush_subscriber(id) {
                Flushed::Waiting => self.write_later(id),
                Flushed::Drained | Flushed::Gone => {}
            }
        }
    }

    /// Writes what subscriber `id`'s socket takes now. A backlog past `MAX_BACKLOG`, or a write error, drops it.
    fn flush_subscriber(&mut self, id: u64) -> Flushed {
        let Some(sub) = self.subscribers.live.get_mut(&id) else {
            return Flushed::Gone;
        };
        match write_some(&sub.stream, &sub.out, &mut sub.done) {
            Ok(true) => {
                sub.out.clear();
                sub.done = 0;
                Flushed::Drained
            }
            Ok(false) if sub.out.len() - sub.done > MAX_BACKLOG => {
                self.unsubscribe(id, "dropped: more than 1 MiB unread");
                Flushed::Gone
            }
            Ok(false) => {
                sub.out.drain(..sub.done);
                sub.done = 0;
                Flushed::Waiting
            }
            Err(e) => {
                self.unsubscribe(id, &format!("dropped: {e}"));
                Flushed::Gone
            }
        }
    }

    /// Subscriber `id`'s backlog goes as its socket drains, on a source of its own (unless one runs already).
    fn write_later(&mut self, id: u64) {
        let Some(sub) = self.subscribers.live.get_mut(&id) else {
            return;
        };
        if sub.writer.is_some() {
            return;
        }
        let writer = match sub.stream.try_clone() {
            Ok(w) => w,
            Err(e) => return self.unsubscribe(id, &format!("dropped: {e}")),
        };
        let token = (self.loop_handle).insert_source(
            Generic::new(writer, Interest::WRITE, Mode::Level),
            move |_, _, state| {
                // Taken out, so a drop while flushing leaves this source to go by returning Remove.
                let own = (state.subscribers.live.get_mut(&id)).and_then(|s| s.writer.take());
                if state.flush_subscriber(id) == Flushed::Waiting
                    && let Some(sub) = state.subscribers.live.get_mut(&id)
                {
                    sub.writer = own;
                    return Ok(PostAction::Continue);
                }
                Ok(PostAction::Remove)
            },
        );
        match token {
            Ok(t) => sub.writer = Some(t),
            Err(e) => self.unsubscribe(id, &format!("dropped: cannot wait to write ({e})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn a_half_close_is_no_hang_up() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs.write_all(b"{\"cmd\":\"subscribe\"}\n").unwrap();
        theirs.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(!hung_up(&ours), "it still reads");
        drop(theirs);
        assert!(hung_up(&ours));
    }

    #[test]
    fn events_go_one_line_each() {
        let events = [
            Event::Orrery { orrery: None },
            Event::Attention {
                windows: vec![1],
                workspaces: vec![2],
            },
        ];
        assert_eq!(
            String::from_utf8(lines(&events)).unwrap(),
            "{\"event\":\"orrery\",\"orrery\":null}\n{\"event\":\"attention\",\"windows\":[1],\"workspaces\":[2]}\n"
        );
    }
}
