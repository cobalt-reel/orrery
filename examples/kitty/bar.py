#!/usr/bin/env python3
"""A one-line bar for one orrery screen, drawn in the terminal it runs in (a `kitten panel`, see bars.py).

    bar.py --screen NAME

Left the workspaces (the one shown on this screen highlighted, occupied ones plain, empty ones dim, one asking for
attention in the attention colour), middle the tabs of the focused slot of this screen's workspace (the current one
bright), right the session's machines (only in a session; where input is in the focus colour) and the time. Driven
by `orrery msg subscribe` ($ORRERY, else `orrery` on PATH); colours from orrery's config. Python 3 standard library
only.
"""

import argparse
import json
import os
import selectors
import signal
import subprocess
import sys
import time
import unicodedata

RESET = "\x1b[0m"
BOLD = "\x1b[1m"
DIM = "\x1b[2m"
ITALIC = "\x1b[3m"
SEP = " │ "
# Without a connection for this long (orrery has quit, say), stop.
GIVE_UP_S = 60.0


def orrery_program():
    return os.environ.get("ORRERY") or "orrery"


def socket_path(env=os.environ):
    """orrery's control socket, as `orrery msg` finds it: $ORRERY_SOCKET, else $XDG_RUNTIME_DIR/orrery.sock."""
    if env.get("ORRERY_SOCKET"):
        return env["ORRERY_SOCKET"]
    if env.get("XDG_RUNTIME_DIR"):
        return os.path.join(env["XDG_RUNTIME_DIR"], "orrery.sock")
    return None


def socket_identity(path):
    """Which socket file `path` is now (a new orrery makes a new one at the same path), or None."""
    try:
        st = os.stat(path) if path else None
    except OSError:
        return None
    return (st.st_dev, st.st_ino, st.st_ctime_ns) if st else None


class Seen:
    """The orrery first connected to, by its socket's identity: a later connection to another one (started at the
    same path after this one quit) is not ours to follow."""

    def __init__(self):
        self.first = None

    def same(self, identity):
        if identity is None:
            return True
        if self.first is None:
            self.first = identity
        return identity == self.first


# --- text -----------------------------------------------------------------------------------------------------------


def parse_color(s):
    """`#rrggbb` or `#rrggbbaa` as (r, g, b); None for anything else."""
    if not isinstance(s, str) or len(s) not in (7, 9) or not s.startswith("#"):
        return None
    try:
        return tuple(int(s[i : i + 2], 16) for i in (1, 3, 5))
    except ValueError:
        return None


def fg(rgb):
    return f"\x1b[38;2;{rgb[0]};{rgb[1]};{rgb[2]}m" if rgb else ""


def bg(rgb):
    return f"\x1b[48;2;{rgb[0]};{rgb[1]};{rgb[2]}m" if rgb else ""


EMOJI_PRESENTATION = "\ufe0f"


def char_widths(s):
    """Each character of `s` with the cells it takes: marks none, wide characters two, and an emoji selector one
    more after a narrow base (it turns the base into a two-cell emoji, as kitty draws it)."""
    out, base = [], 0  # the width of the last base character, while a selector may still widen it
    for c in s:
        if c == EMOJI_PRESENTATION:
            w, base = (1 if base == 1 else 0), 0
        elif unicodedata.category(c) in ("Mn", "Me"):
            w = 0
        else:
            w = base = 2 if unicodedata.east_asian_width(c) in ("W", "F") else 1
        out.append((c, w))
    return out


def cell_width(s):
    return sum(w for _, w in char_widths(s))


def truncate(s, cells):
    """`s` in at most `cells` cells, ending in "…" when cut (never between a character and its marks)."""
    if cell_width(s) <= cells:
        return s
    if cells <= 0:
        return ""
    widths = char_widths(s)
    out, used = [], 0
    for i, (c, w) in enumerate(widths):
        # A character goes with the zero-width marks after it, and with an emoji selector's extra cell.
        if w == 0 or c == EMOJI_PRESENTATION:
            continue
        group = [c]
        for c2, w2 in widths[i + 1 :]:
            if w2 and c2 != EMOJI_PRESENTATION:
                break
            group.append(c2)
            w += w2
        if used + w > cells - 1:
            break
        out += group
        used += w
    return "".join(out) + "…"


def clean(s):
    """Text from a window, safe to print: no control or format characters (no escape sequences)."""
    return "".join(c for c in str(s) if unicodedata.category(c) not in ("Cc", "Cf", "Zl", "Zp"))


# --- the model --------------------------------------------------------------------------------------------------------


class Model:
    """What the bar shows, kept from the stream: screens by name, workspaces by number, and for each workspace the
    tabs of its focused slot as last seen (the stream only sends the focused slot's, so another screen's are kept
    from when it was focused, with titles and closed windows applied since)."""

    def __init__(self):
        self.screens = {}  # name -> {number, workspace, focused}
        self.workspaces = {}  # number -> {screen, shown, occupied, attention}
        self.tabs = {}  # workspace number -> [{current, attention, windows: [{id, app_id, title}]}]
        self.colors = {}  # name -> (r, g, b)
        self.session = None

    @classmethod
    def from_state(cls, ev):
        m = cls()
        for mon in ev.get("monitors") or []:
            m.screens[mon.get("name")] = {
                "number": mon.get("number"),
                "workspace": mon.get("workspace"),
                "focused": bool(mon.get("focused")),
            }
        for w in ev.get("workspaces") or []:
            slots = w.get("slots") or []
            tabs = [t for s in slots for t in s.get("tabs") or []]
            m.workspaces[w.get("number")] = {
                "screen": w.get("monitor"),
                "shown": bool(w.get("shown")),
                "occupied": bool(tabs),
                "attention": any(t.get("attention") for t in tabs),
            }
            focused = next((s for s in slots if s.get("index") == w.get("focused_slot")), None)
            m.tabs[w.get("number")] = (focused or {}).get("tabs") or []
        m.set_colors(ev.get("colors"))
        m.session = ev.get("session")
        return m

    def set_colors(self, colors):
        parsed = {k: parse_color(v) for k, v in (colors or {}).items()}
        self.colors = {k: v for k, v in parsed.items() if v}

    def windows(self):
        return (w for tabs in self.tabs.values() for t in tabs for w in t.get("windows") or [])

    def apply(self, ev):
        kind = ev.get("event")
        if kind == "workspace" and "screens" in ev and "workspaces" in ev:
            self.screens = {
                s.get("name"): {
                    "number": s.get("number"),
                    "workspace": s.get("workspace"),
                    "focused": bool(s.get("focused")),
                }
                for s in ev["screens"]
            }
            self.workspaces = {
                w.get("number"): {k: w.get(k) for k in ("screen", "shown", "occupied", "attention")}
                for w in ev["workspaces"]
            }
            self.tabs = {n: t for n, t in self.tabs.items() if n in self.workspaces}
        elif kind == "focus":
            for name, s in self.screens.items():
                s["focused"] = name == ev.get("screen")
                if s["focused"] and ev.get("workspace") is not None:
                    s["workspace"] = ev["workspace"]
        elif kind == "tabs" and ev.get("workspace") is not None:
            self.tabs[ev["workspace"]] = ev.get("tabs") or []
        elif kind == "title" and "id" in ev:
            for w in self.windows():
                if w.get("id") == ev["id"]:
                    w["title"] = ev.get("title", w.get("title"))
                    w["app_id"] = ev.get("app_id", w.get("app_id"))
        elif kind == "closed" and "id" in ev:
            for n, tabs in self.tabs.items():
                for t in tabs:
                    t["windows"] = [w for w in t.get("windows") or [] if w.get("id") != ev["id"]]
                self.tabs[n] = [t for t in tabs if t["windows"]]
        elif kind == "attention":
            asking = set(ev.get("windows") or [])
            for tabs in self.tabs.values():
                for t in tabs:
                    t["attention"] = any(w.get("id") in asking for w in t.get("windows") or [])
            numbers = set(ev.get("workspaces") or [])
            for n, w in self.workspaces.items():
                w["attention"] = n in numbers
        elif kind == "session":
            self.session = ev.get("session")
        elif kind == "config" and ev.get("colors"):
            self.set_colors(ev["colors"])
        return self


def apply(model, ev):
    """The model after event `ev`: a `state` starts a new one; anything before the first `state` is ignored."""
    if not isinstance(ev, dict):
        return model
    if ev.get("event") == "state":
        return Model.from_state(ev)
    return model.apply(ev) if model is not None else None


# --- rendering ------------------------------------------------------------------------------------------------------


def fit_tabs(labels, current, avail):
    """The tab labels that fit in `avail` cells with SEP between them, as (index, text): all as they are, else each
    cut to the same longest width that fits, else only the current one with its place ("3/12 title")."""
    n = len(labels)
    if n == 0 or avail <= 0:
        return []
    widths = [cell_width(lab) for lab in labels]
    room = avail - cell_width(SEP) * (n - 1)
    if sum(widths) <= room:
        return list(enumerate(labels))
    if room >= 2 * n:
        cap = max(widths)
        while sum(min(w, cap) for w in widths) > room:
            cap -= 1
        return [(i, truncate(lab, cap)) for i, lab in enumerate(labels)]
    current = min(max(current, 0), n - 1)
    place = f"{current + 1}/{n} "
    return [(current, truncate(place + labels[current], avail))]


def tab_label(t):
    windows = t.get("windows") or []
    top = windows[-1] if windows else {}
    return clean(top.get("title") or top.get("app_id") or "?")


def segments_width(segs):
    return sum(cell_width(text) for _, text in segs)


def workspace_segments(model, screen):
    c = model.colors
    here = model.screens.get(screen)
    focused = bool(here and here["focused"])
    session = model.session or {}
    numbers = sorted(n for n in model.workspaces if isinstance(n, int))
    foreign = sorted(set(n for n in session.get("foreign") or [] if isinstance(n, int)) - set(numbers))
    segs = []
    for n in sorted(numbers + foreign):
        w = model.workspaces.get(n)
        if w is None:
            style = DIM + ITALIC  # on another machine of the session
        elif here and here["workspace"] == n:
            style = bg(c.get("focus")) + fg(c.get("background")) + BOLD if focused else fg(c.get("focus")) + BOLD
        elif w.get("attention"):
            style = fg(c.get("attention")) + BOLD
        elif w.get("shown"):
            style = fg(c.get("badge"))  # shown on another screen
        elif w.get("occupied"):
            style = ""
        else:
            style = DIM
        segs.append((style, f" {n} "))
    return segs


def tab_segments(model, screen, avail):
    here = model.screens.get(screen)
    tabs = model.tabs.get(here["workspace"], []) if here else []
    labels = [tab_label(t) for t in tabs]
    current = next((i for i, t in enumerate(tabs) if t.get("current")), 0)
    segs = []
    for k, (i, text) in enumerate(fit_tabs(labels, current, avail)):
        if k:
            segs.append((DIM, SEP))
        if tabs[i].get("attention"):
            style = fg(model.colors.get("attention"))
        elif i == current:
            style = BOLD
        else:
            style = DIM
        segs.append((style, text))
    return segs


def session_segments(model):
    session = model.session
    if not session:
        return []
    segs = []
    for m in session.get("machines") or []:
        name = clean(m.get("name") or "?")
        if segs:
            segs.append(("", " "))
        if name == session.get("input"):
            segs.append((fg(model.colors.get("focus")) + BOLD, name))
        elif m.get("connected") is False:
            segs.append((DIM, name))
        else:
            segs.append(("", name))
    return segs


def render(model, screen, width, clock):
    """The bar's line for `screen`, exactly `width` cells wide, ending in RESET (SGR sequences only, no movement)."""
    right = [("", clock), ("", " ")]
    if model is not None:
        session = session_segments(model)
        if session and segments_width(session) + 2 + segments_width(right) + 1 <= width:
            right = session + [("", "  ")] + right
    if segments_width(right) + 1 > width:
        right = []
    left = workspace_segments(model, screen) if model is not None else []
    gap_right = 1 if right else 0
    if segments_width(left) + segments_width(right) + gap_right > width:
        left = []
    used = segments_width(left) + segments_width(right) + gap_right
    middle = []
    if model is not None and left:
        middle = tab_segments(model, screen, width - used - 1)
        if middle:
            middle = [("", " ")] + middle
    pad = width - segments_width(left) - segments_width(middle) - segments_width(right)
    line = left + middle + [("", " " * pad)] + right
    return "".join(RESET + style + text for style, text in line if text) + RESET


def background_osc(colors):
    """OSC 11: the terminal's default background in orrery's background colour, so the whole panel matches."""
    rgb = colors.get("background")
    return "\x1b]11;#{:02x}{:02x}{:02x}\x1b\\".format(*rgb) if rgb else ""


# --- the stream -----------------------------------------------------------------------------------------------------


class Subscription:
    """`orrery msg subscribe` running, read as whole JSON lines."""

    def __init__(self, program):
        self.buf = b""
        try:
            self.proc = subprocess.Popen(
                [program, "msg", "subscribe"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
        except OSError:
            self.proc = None

    def fileno(self):
        return self.proc.stdout.fileno()

    def read(self):
        """The events in what can be read now (blocking until something can), or None at the end."""
        if self.proc is None:
            return None
        data = os.read(self.fileno(), 65536)
        if not data:
            return None
        *lines, self.buf = (self.buf + data).split(b"\n")
        events = []
        for line in lines:
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            if isinstance(ev, dict):
                events.append(ev)
        return events

    def close(self):
        """Stops it; its exit status (127 when it never ran)."""
        if self.proc is None:
            return 127
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(1)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.proc.stdout.close()
        return self.proc.wait()


# --- running --------------------------------------------------------------------------------------------------------


def terminal_width(out):
    try:
        return os.get_terminal_size(out.fileno()).columns
    except (OSError, ValueError):
        pass
    try:
        return int(os.environ.get("COLUMNS", ""))
    except ValueError:
        return 80


def until_next_minute():
    return 60.05 - time.time() % 60


def main(argv=None):
    ap = argparse.ArgumentParser(description="A bar for one orrery screen, in this terminal.")
    ap.add_argument("--screen", required=True, help="the screen's output name, as `orrery msg state` lists it")
    args = ap.parse_args(argv)
    out = sys.stdout

    # SIGWINCH (and the stop signals) wake the selector through a pipe.
    wake_r, wake_w = os.pipe()
    os.set_blocking(wake_r, False)
    os.set_blocking(wake_w, False)
    signal.set_wakeup_fd(wake_w)
    signal.signal(signal.SIGWINCH, lambda *_: None)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda *_: sys.exit(0))

    out.write("\x1b[?25l\x1b[?7l")  # no cursor, no wrap
    model, drawn_bg = None, ""

    def draw():
        nonlocal drawn_bg
        if model is not None and background_osc(model.colors) != drawn_bg:
            drawn_bg = background_osc(model.colors)
            out.write(drawn_bg)
        out.write("\r\x1b[2K" + render(model, args.screen, terminal_width(out), time.strftime("%H:%M")))
        out.flush()

    backoff, lost_since, seen = 0.5, None, Seen()
    try:
        while True:
            sub = Subscription(orrery_program())
            sel = selectors.DefaultSelector()
            sel.register(wake_r, selectors.EVENT_READ)
            if sub.proc is not None:
                sel.register(sub.fileno(), selectors.EVENT_READ, sub)
            connected = False
            while sub.proc is not None:
                draw()
                ready = sel.select(until_next_minute())
                for key, _ in ready:
                    if key.data is None:
                        while True:
                            try:
                                if not os.read(wake_r, 512):
                                    break
                            except BlockingIOError:
                                break
                if any(key.data is sub for key, _ in ready):
                    events = sub.read()
                    if events is None:
                        break
                    for ev in events:
                        if ev.get("event") == "state" and not seen.same(socket_identity(socket_path())):
                            sub.close()
                            return 0  # another orrery: this bar's has gone
                        model = apply(model, ev)
                        connected = connected or ev.get("event") == "state"
            sel.close()
            sub.close()
            if connected:
                backoff, lost_since = 0.5, None
            lost_since = lost_since or time.monotonic()
            if time.monotonic() - lost_since > GIVE_UP_S:
                return 1
            draw()
            time.sleep(backoff)
            backoff = min(backoff * 2, 5.0)
    finally:
        try:
            out.write(RESET + "\x1b[?7h\x1b[?25h")
            out.flush()
        except OSError:
            pass  # the terminal is gone


if __name__ == "__main__":
    sys.exit(main())
