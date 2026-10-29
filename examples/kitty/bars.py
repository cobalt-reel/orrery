#!/usr/bin/env python3
"""A bar (bar.py in a `kitten panel` along the top) on every orrery screen, kept so as screens come and go.

    bars.py [KITTEN-PANEL-OPTION...]       e.g. bars.py -o font_size=10 -o font_family=monospace

The options go to each `kitten panel` (see `kitten panel --help`). Run once by `spawn-at-start` (orrery.kdl); the
screens come from `orrery msg subscribe` ($ORRERY, else `orrery` on PATH). A bar that dies while its screen is there
is started again, waiting longer each time it dies young; a screen that goes takes its bar. Without orrery for a
minute (it has quit), the bars are stopped and this ends; so it does when another orrery has taken the socket's
place (one started after this one's quit): that one runs its own. The bars die with this script.
"""

import ctypes
import os
import selectors
import signal
import subprocess
import sys
import time

import bar

BAR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bar.py")
# A bar that ran this long before dying counts as having worked: its next start is not delayed.
HEALTHY_S = 10.0


def screens_of(ev):
    """The screens' output names an event lists (`state`, `workspace`, `layout`), else None."""
    kind = ev.get("event")
    if kind == "state":
        return {m.get("name") for m in ev.get("monitors") or []}
    if kind in ("workspace", "layout") and "screens" in ev:
        return {s.get("name") for s in ev["screens"]}
    return None


def panel_argv(screen, extra, bar_path=BAR, python=sys.executable):
    return [
        "kitten",
        "panel",
        "--edge=top",
        "--lines=1",
        f"--output-name={screen}",
        "--class=orrery-bar",
        "--focus-policy=not-allowed",
        *extra,
        python,
        bar_path,
        "--screen",
        screen,
    ]


def die_with_parent():
    """In a panel's child process before exec: SIGTERM when bars.py dies, however it dies (prctl's
    PR_SET_PDEATHSIG; Linux only, elsewhere nothing)."""
    try:
        ctypes.CDLL(None, use_errno=True).prctl(1, signal.SIGTERM, 0, 0, 0)
    except (OSError, AttributeError):
        pass


def restart_delay(deaths):
    """Seconds to wait before starting a bar again after `deaths` quick deaths in a row."""
    return 0 if deaths == 0 else min(2**deaths, 60)


class Panels:
    def __init__(self, extra):
        self.extra = extra
        self.want = set()
        self.running = {}  # screen -> (Popen, started)
        self.deaths = {}  # screen -> quick deaths in a row
        self.next_try = {}  # screen -> monotonic time it may start again

    def tend(self, start=True):
        """Stops bars whose screen went and notes the ones that died; with `start`, starts the missing ones (not
        while orrery is away: what `want` says may be gone, and a panel without orrery dies at once)."""
        now = time.monotonic()
        for screen, (proc, started) in list(self.running.items()):
            if screen not in self.want:
                stop(proc)
                del self.running[screen]
                self.deaths.pop(screen, None)
            elif proc.poll() is not None:
                del self.running[screen]
                young = now - started < HEALTHY_S
                self.deaths[screen] = self.deaths.get(screen, 0) + 1 if young else 0
                self.next_try[screen] = now + restart_delay(self.deaths[screen])
        for screen in sorted(self.want - set(self.running)):
            if not start or now < self.next_try.get(screen, 0):
                continue
            try:
                proc = subprocess.Popen(
                    panel_argv(screen, self.extra), stdin=subprocess.DEVNULL, preexec_fn=die_with_parent
                )
            except OSError as e:
                print(f"bars.py: cannot start kitten panel: {e}", file=sys.stderr)
                self.deaths[screen] = self.deaths.get(screen, 0) + 1
                self.next_try[screen] = now + restart_delay(self.deaths[screen])
                continue
            self.running[screen] = (proc, now)

    def stop_all(self):
        for proc, _ in self.running.values():
            stop(proc)
        self.running.clear()


def stop(proc):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(2)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def main(argv=None):
    extra = list(sys.argv[1:] if argv is None else argv)
    for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        signal.signal(sig, lambda *_: sys.exit(0))
    panels = Panels(extra)
    backoff, lost_since, seen = 0.5, None, bar.Seen()
    try:
        while True:
            sub = bar.Subscription(bar.orrery_program())
            connected = False
            if sub.proc is not None:
                sel = selectors.DefaultSelector()
                sel.register(sub.fileno(), selectors.EVENT_READ)
                while True:
                    panels.tend(start=connected)
                    if not sel.select(1.0):
                        continue
                    events = sub.read()
                    if events is None:
                        break
                    for ev in events:
                        if ev.get("event") == "state" and not seen.same(bar.socket_identity(bar.socket_path())):
                            sub.close()
                            return 0  # another orrery: its own bars.py runs its bars (finally stops ours)
                        screens = screens_of(ev)
                        if screens is not None:
                            panels.want = screens
                            connected = True
                sel.close()
            sub.close()
            if connected:
                backoff, lost_since = 0.5, None
            lost_since = lost_since or time.monotonic()
            if time.monotonic() - lost_since > bar.GIVE_UP_S:
                return 1
            panels.tend(start=False)
            time.sleep(backoff)
            backoff = min(backoff * 2, 5.0)
    finally:
        panels.stop_all()


if __name__ == "__main__":
    sys.exit(main())
