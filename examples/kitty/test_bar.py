"""The bar's model and rendering, and the bars' bookkeeping: `python3 -m unittest discover -s examples/kitty`.

Pure functions only, plus a stand-in `orrery` script in a temp dir: no orrery, no kitty, no real paths.
"""

import os
import re
import shutil
import select
import stat
import subprocess
import sys
import tempfile
import time
import unittest

import bar
import bars

HERE = os.path.dirname(os.path.abspath(__file__))
COLORS = {"focus": "#ff7edbff", "badge": "#ff7edbff", "attention": "#ffb000ff", "background": "#1a1625ff"}
SGR = re.compile(r"\x1b\[[0-9;]*m")


def slot(index, tabs, focused=False):
    return {"index": index, "name": None, "rect": {"x": 0, "y": 0, "w": 1, "h": 1}, "focused": focused, "tabs": tabs}


def tab(*windows, current=False, attention=False):
    windows = [{"id": i, "app_id": a, "title": t} for i, a, t in windows]
    return {"current": current, "attention": attention, "windows": windows}


def snapshot(session=None):
    """Two screens: A shows workspace 1 (focused; two tabs in its focused slot), B shows 2 (empty); 3 hidden, on A."""
    stack = tab((2, "kitty", "~"), (3, "kitty", "htop"), current=True)
    ws1 = [slot(0, [tab((1, "kitty", "vim")), stack], True), slot(1, [])]
    ws3 = [slot(0, []), slot(1, [tab((4, "mpv", "film"), current=True, attention=True)])]
    return {
        "event": "state",
        "monitors": [
            {"name": "A", "number": 1, "workspace": 1, "layout": "tall", "focused": True, "slots": ws1},
            {"name": "B", "number": 2, "workspace": 2, "layout": "quad", "focused": False, "slots": [slot(0, [])]},
        ],
        "workspaces": [
            {"number": 1, "monitor": "A", "shown": True, "layout": "x", "focused_slot": 0, "slots": ws1},
            {"number": 2, "monitor": "B", "shown": True, "layout": "x", "focused_slot": 0, "slots": [slot(0, [])]},
            {"number": 3, "monitor": "A", "shown": False, "layout": "x", "focused_slot": 1, "slots": ws3},
        ],
        "colors": COLORS,
        "session": session,
    }


def replace_file(path):
    """Another file at `path` (another inode, as a new orrery's socket is), put there in one step."""
    with open(path + ".new", "w") as f:
        f.write("new")
    os.replace(path + ".new", path)


def plain(line):
    return SGR.sub("", line)


def model(*events):
    m = None
    for e in [snapshot(), *events]:
        m = bar.apply(m, e)
    return m


def fg(hex6):
    r, g, b = (int(hex6[i : i + 2], 16) for i in (0, 2, 4))
    return f"\x1b[38;2;{r};{g};{b}m"


def bg(hex6):
    return fg(hex6).replace("[38;", "[48;")


class Text(unittest.TestCase):
    def test_colours_parse_with_or_without_alpha(self):
        self.assertEqual(bar.parse_color("#ff7edbff"), (255, 126, 219))
        self.assertEqual(bar.parse_color("#010203"), (1, 2, 3))
        self.assertIsNone(bar.parse_color("pink"))
        self.assertIsNone(bar.parse_color(None))

    def test_widths_count_cells(self):
        self.assertEqual(bar.cell_width("abc"), 3)
        self.assertEqual(bar.cell_width("日本"), 4)
        self.assertEqual(bar.cell_width("e\u0301"), 1, "a combining accent takes no cell")
        self.assertEqual(bar.cell_width("a\u20dd"), 1, "nor an enclosing mark")
        # An emoji selector after a narrow base makes it an emoji, two cells, as kitty draws it; after a wide one,
        # nothing.
        self.assertEqual(bar.cell_width("\u2764\ufe0f"), 2)
        self.assertEqual(bar.cell_width("\U0001f600\ufe0f"), 2)
        self.assertEqual(bar.cell_width("x\u2764\ufe0fy"), 4)
        self.assertEqual(bar.truncate("\u2764\ufe0fabc", 3), "\u2764\ufe0f…")

    def test_truncation_ends_in_an_ellipsis_and_never_overflows(self):
        self.assertEqual(bar.truncate("hello", 5), "hello")
        self.assertEqual(bar.truncate("hello", 4), "hel…")
        self.assertEqual(bar.truncate("hello", 1), "…")
        self.assertEqual(bar.truncate("hello", 0), "")
        self.assertEqual(bar.truncate("日本語", 4), "日…")

    def test_titles_lose_control_characters(self):
        # A title must never reach the terminal as an escape sequence.
        self.assertEqual(bar.clean("a\x1b]0;x\x07b\nc‮"), "a]0;xbc")


class Model(unittest.TestCase):
    def test_the_snapshot_gives_screens_workspaces_and_each_workspaces_tabs(self):
        m = model()
        self.assertEqual(m.screens["A"], {"number": 1, "workspace": 1, "focused": True})
        self.assertEqual(m.workspaces[1], {"screen": "A", "shown": True, "occupied": True, "attention": False})
        self.assertEqual(m.workspaces[2]["occupied"], False)
        self.assertEqual(m.workspaces[3]["attention"], True)
        self.assertEqual([t["windows"][-1]["title"] for t in m.tabs[1]], ["vim", "htop"])
        self.assertEqual([t["windows"][-1]["title"] for t in m.tabs[3]], ["film"], "from its focused slot")
        self.assertEqual(m.colors["focus"], (255, 126, 219))

    def test_events_update_it(self):
        m = model(
            {"event": "workspace", "screens": [{"name": "A", "number": 1, "workspace": 3, "focused": True}],
             "workspaces": [{"number": 3, "screen": "A", "shown": True, "occupied": True, "attention": False}]},
        )
        self.assertEqual(list(m.screens), ["A"], "screen B went")
        self.assertEqual(list(m.workspaces), [3])
        self.assertNotIn(1, m.tabs, "a workspace that went takes its tabs")

        m = model({"event": "focus", "screen": "B", "workspace": 2, "slot": 0, "slot_name": None, "window": None})
        self.assertEqual((m.screens["A"]["focused"], m.screens["B"]["focused"]), (False, True))

        new = [tab((9, "foot", "new"), current=True)]
        m = model({"event": "tabs", "screen": "B", "workspace": 2, "slot": 0, "tabs": new})
        self.assertEqual(m.tabs[2][0]["windows"][0]["title"], "new")

        m = model({"event": "title", "id": 3, "app_id": "kitty", "title": "top"})
        self.assertEqual(m.tabs[1][1]["windows"][-1]["title"], "top")

        m = model({"event": "closed", "id": 1})
        self.assertEqual(len(m.tabs[1]), 1, "a tab left without windows goes")
        m = model({"event": "closed", "id": 3})
        self.assertEqual([w["id"] for w in m.tabs[1][1]["windows"]], [2])

        m = model({"event": "attention", "windows": [1], "workspaces": [1]})
        self.assertEqual([t["attention"] for t in m.tabs[1]], [True, False])
        self.assertEqual([m.workspaces[n]["attention"] for n in (1, 3)], [True, False])

        m = model({"event": "config", "error": None, "colors": dict(COLORS, focus="#000001ff")})
        self.assertEqual(m.colors["focus"], (0, 0, 1))

        m = model({"event": "session", "session": {"machine": "desk", "input": "desk", "machines": []}})
        self.assertEqual(m.session["machine"], "desk")

    def test_each_screens_layout_comes_from_the_snapshot_and_layout_events(self):
        self.assertEqual(model().layouts, {"A": "tall", "B": "quad"})
        m = model({"event": "layout", "screens": [{"name": "A", "workspace": 1, "layout": "wide", "slots": []}]})
        self.assertEqual(m.layouts, {"A": "wide"})
        m = model({"event": "workspace", "screens": [{"name": "A", "number": 1, "workspace": 3, "focused": True}],
                   "workspaces": [{"number": 3, "screen": "A", "shown": True, "occupied": True, "attention": False}]})
        self.assertEqual(m.layouts["A"], "tall", "kept until a layout event says otherwise")

    def test_unknown_or_partial_events_change_nothing(self):
        m = model({"event": "layers", "screens": []}, {"event": "who-knows"}, {"event": "title", "id": 99})
        self.assertEqual(m.tabs, model().tabs)

    def test_no_snapshot_yet_means_no_model(self):
        self.assertIsNone(bar.apply(None, {"event": "focus", "screen": "A"}))


class Render(unittest.TestCase):
    def test_a_line_has_workspaces_tabs_and_the_clock_in_exactly_the_width(self):
        line = bar.render(model(), "A", 60, "12:34")
        text = plain(line)
        self.assertEqual(bar.cell_width(text), 60)
        self.assertTrue(text.startswith(" 1  2  3 "), text)
        self.assertIn("vim │ htop", text)
        self.assertTrue(text.endswith(" 12:34 "), text)
        self.assertTrue(line.endswith(bar.RESET))

    def test_workspace_marks(self):
        m = model()
        line = bar.render(m, "A", 60, "12:34")
        # 1 is shown on this focused screen: a block in the focus colour.
        self.assertIn(bg("ff7edb") + fg("1a1625") + bar.BOLD + " 1 ", line)
        # 2 is shown on another screen: the badge colour; 3 asks for attention.
        self.assertIn(fg("ff7edb") + " 2 ", line)
        self.assertIn(fg("ffb000") + bar.BOLD + " 3 ", line)
        # From B (not focused) its own workspace is in the focus colour without the block; 1 is elsewhere.
        line = bar.render(m, "B", 60, "12:34")
        self.assertIn(fg("ff7edb") + bar.BOLD + " 2 ", line)
        self.assertNotIn(bg("ff7edb"), line)
        # An empty workspace nobody shows is dim; an occupied one plain.
        m = model({"event": "workspace", "screens": [{"name": "A", "number": 1, "workspace": 1, "focused": True}],
                   "workspaces": [{"number": 1, "screen": "A", "shown": True, "occupied": True, "attention": False},
                                  {"number": 4, "screen": "A", "shown": False, "occupied": False, "attention": False},
                                  {"number": 5, "screen": "A", "shown": False, "occupied": True, "attention": False}]})
        line = bar.render(m, "A", 60, "12:34")
        self.assertIn(bar.DIM + " 4 ", line)
        self.assertIn(bar.RESET + " 5 ", line)

    def test_the_layout_sits_before_the_clock_in_the_badge_colour(self):
        line = bar.render(model(), "A", 60, "12:34")
        self.assertTrue(plain(line).endswith(" tall  12:34 "), plain(line))
        self.assertIn(fg("ff7edb") + "tall", line)
        self.assertTrue(plain(bar.render(model(), "B", 60, "12:34")).endswith(" quad  12:34 "))
        self.assertNotIn("tall", plain(bar.render(model(), "A", 12, "12:34")), "too narrow: the clock only")

    def test_tabs_are_from_this_screens_workspace(self):
        self.assertIn("vim │ htop", plain(bar.render(model(), "A", 60, "00:00")))
        self.assertNotIn("vim", plain(bar.render(model(), "B", 60, "00:00")), "workspace 2 has no tabs")
        line = bar.render(model(), "A", 60, "00:00")
        self.assertIn(bar.BOLD + "htop", line, "the current tab is bright")
        self.assertIn(bar.DIM + "vim", line)

    def test_an_attention_tab_is_in_the_attention_colour(self):
        m = model({"event": "attention", "windows": [1], "workspaces": [1]})
        self.assertIn(fg("ffb000") + "vim", bar.render(m, "A", 60, "00:00"))

    def test_long_titles_are_truncated_to_fit(self):
        long = "a very long title that goes on and on"
        m = model({"event": "title", "id": 1, "app_id": "kitty", "title": long})
        for width in (40, 30, 24):
            text = plain(bar.render(m, "A", width, "12:34"))
            self.assertEqual(bar.cell_width(text), width, text)
            self.assertIn("…", text)
            self.assertTrue(text.endswith(" 12:34 "))

    def test_too_many_tabs_shrink_to_the_current_one_and_its_place(self):
        tabs = [tab((i, "k", f"t{i}"), current=i == 7) for i in range(20)]
        m = model({"event": "tabs", "screen": "A", "workspace": 1, "slot": 0, "tabs": tabs})
        text = plain(bar.render(m, "A", 30, "12:34"))
        self.assertIn("8/20 t7", text)
        self.assertEqual(bar.cell_width(text), 30)

    def test_fit_tabs(self):
        self.assertEqual(bar.fit_tabs(["ab", "cd"], 1, 7), [(0, "ab"), (1, "cd")])
        self.assertEqual(bar.fit_tabs(["abcdef", "cd"], 1, 7), [(0, "a…"), (1, "cd")])
        self.assertEqual(bar.fit_tabs(["abcdef", "ghijkl"], 0, 9), [(0, "ab…"), (1, "gh…")])
        self.assertEqual(bar.fit_tabs(["a", "b", "c"], 2, 5), [(2, "3/3 c")])
        self.assertEqual(bar.fit_tabs([], 0, 10), [])
        self.assertEqual(bar.fit_tabs(["a"], 0, 0), [])

    def test_a_narrow_bar_keeps_the_clock_and_never_overflows(self):
        for width in (12, 8, 6, 3, 0):
            text = plain(bar.render(model(), "A", width, "12:34"))
            self.assertEqual(bar.cell_width(text), width, repr(text))

    def test_the_session_shows_only_in_one_with_input_marked(self):
        self.assertNotIn("desk", plain(bar.render(model(), "A", 80, "12:34")))
        session = {"machine": "desk", "role": "primary", "input": "side", "machines": [
            {"name": "desk", "connected": True},
            {"name": "side", "connected": True},
            {"name": "far", "connected": False},
        ]}
        m = bar.apply(None, snapshot(session))
        line = bar.render(m, "A", 80, "12:34")
        self.assertTrue(plain(line).endswith(" desk side far  tall  12:34 "), plain(line))
        self.assertIn(fg("ff7edb") + bar.BOLD + "side", line, "where input is")
        self.assertIn(bar.DIM + "far", line, "a machine whose link is down")

    def test_a_screen_not_known_yet_or_no_model_still_draws_the_clock(self):
        self.assertTrue(plain(bar.render(model(), "C", 20, "12:34")).endswith(" 12:34 "))
        self.assertEqual(plain(bar.render(None, "A", 10, "12:34")), "    12:34 ")

    def test_the_background_is_set_from_the_colours(self):
        self.assertEqual(bar.background_osc(model().colors), "\x1b]11;#1a1625\x1b\\")
        self.assertEqual(bar.background_osc({}), "")


class Subscription(unittest.TestCase):
    def test_lines_are_read_whole_and_eof_ends_it(self):
        with tempfile.TemporaryDirectory() as d:
            fake = os.path.join(d, "orrery")
            with open(fake, "w") as f:
                # Half a line, then the rest: one event; a broken line is skipped.
                f.write("#!/bin/sh\nprintf '{\"event\":\"st'\nsleep 0.1\n")
                f.write("printf 'ate\"}\\nnot json\\n{\"event\":\"focus\"}\\n'\n")
            os.chmod(fake, stat.S_IRWXU)
            sub = bar.Subscription(fake)
            events = []
            while (got := sub.read()) is not None:
                events += got
            self.assertEqual([e["event"] for e in events], ["state", "focus"])
            self.assertEqual(sub.close(), 0)

    def test_a_missing_program_is_a_failed_connection(self):
        with tempfile.TemporaryDirectory() as d:
            sub = bar.Subscription(os.path.join(d, "none"))
            self.assertIsNone(sub.read())
            self.assertNotEqual(sub.close(), 0)

    def test_a_missing_program_says_how_orrery_is_found(self):
        with tempfile.TemporaryDirectory() as d:
            missing = os.path.join(d, "none")
            sub = bar.Subscription(missing)
            self.assertIn(missing, sub.error)
            self.assertIn("$ORRERY", sub.error)
            self.assertIn("PATH", sub.error)
            self.assertNotIn("\n", sub.error)
        started = bar.Subscription("true")
        self.assertIsNone(started.error)
        started.close()

    def test_both_scripts_say_so_on_stderr(self):
        with tempfile.TemporaryDirectory() as d:
            missing = os.path.join(d, "none")
            for script, args in (("bar.py", ["--screen", "x"]), ("bars.py", [])):
                with self.subTest(script):
                    env = {**os.environ, "ORRERY": missing, "ORRERY_SOCKET": os.path.join(d, "absent.sock")}
                    proc = subprocess.Popen(
                        [sys.executable, "-B", os.path.join(HERE, script), *args],
                        env=env,
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.PIPE,
                        text=True,
                    )
                    try:
                        ready, _, _ = select.select([proc.stderr], [], [], 10)
                        self.assertTrue(ready, f"{script} said nothing on stderr")
                        line = proc.stderr.readline()
                    finally:
                        proc.terminate()
                        proc.wait(5)
                        proc.stderr.close()
                    self.assertTrue(line.startswith(f"{script}: "), line)
                    self.assertIn(missing, line)
                    self.assertIn("$ORRERY", line)


class Identity(unittest.TestCase):
    def test_the_socket_is_orrery_socket_else_the_default(self):
        self.assertEqual(bar.socket_path({"ORRERY_SOCKET": "/s/o.sock", "XDG_RUNTIME_DIR": "/r"}), "/s/o.sock")
        self.assertEqual(bar.socket_path({"XDG_RUNTIME_DIR": "/r"}), "/r/orrery.sock")
        self.assertIsNone(bar.socket_path({}))

    def test_a_socket_made_again_is_another_orrery(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "orrery.sock")
            self.assertIsNone(bar.socket_identity(path))
            open(path, "w").close()
            first = bar.socket_identity(path)
            self.assertEqual(bar.socket_identity(path), first)
            os.remove(path)
            time.sleep(0.01)
            open(path, "w").close()
            self.assertNotEqual(bar.socket_identity(path), first)

    def test_the_first_orrery_is_kept_and_another_or_none_is_noticed(self):
        seen = bar.Seen()
        self.assertTrue(seen.same((1, 2, 3)), "the first one is taken")
        self.assertTrue(seen.same((1, 2, 3)))
        self.assertFalse(seen.same((1, 2, 4)))
        self.assertFalse(seen.same(None), "its socket is gone: so is it")
        unknown = bar.Seen()
        self.assertTrue(unknown.same(None), "none known yet: no reason to stop")
        self.assertTrue(unknown.same((1, 2, 3)), "the first known one is taken")

    def test_the_orrery_is_the_one_whose_socket_was_there_at_start(self):
        # orrery binds its socket before it starts its programs: the socket there when a script starts is its
        # orrery's, even before a connection ever succeeds.
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "orrery.sock")
            open(path, "w").close()
            seen = bar.Seen(bar.socket_identity(path))
            self.assertTrue(seen.same(bar.socket_identity(path)))
            replace_file(path)
            self.assertFalse(seen.same(bar.socket_identity(path)), "replaced: another orrery")
            os.remove(path)
            self.assertFalse(bar.Seen(bar.socket_identity(path) or (1, 2, 3)).same(bar.socket_identity(path)))

    def test_both_scripts_stop_when_the_socket_is_replaced_or_gone_before_any_connection(self):
        # The stand-in orrery never answers (as when `orrery msg subscribe` cannot connect); meanwhile the socket is
        # replaced by another orrery's, or removed. The script must stop at its next try, not follow the new one.
        for change in ("replace", "remove"):
            for script, args in (("bar.py", ["--screen", "x"]), ("bars.py", [])):
                with self.subTest(change=change, script=script), tempfile.TemporaryDirectory() as d:
                    sock = os.path.join(d, "orrery.sock")
                    open(sock, "w").close()
                    runs = os.path.join(d, "runs")
                    fake = os.path.join(d, "orrery")
                    with open(fake, "w") as f:
                        f.write(f"#!/bin/sh\necho run >> '{runs}'\n")
                        if change == "replace":
                            f.write(f"echo new > '{sock}.new' && mv '{sock}.new' '{sock}'\n")
                        else:
                            f.write(f"rm -f '{sock}'\n")
                        f.write("exit 1\n")
                    os.chmod(fake, stat.S_IRWXU)
                    env = {**os.environ, "ORRERY": fake, "ORRERY_SOCKET": sock}
                    out = subprocess.run(
                        [sys.executable, "-B", os.path.join(HERE, script), *args],
                        env=env,
                        stdin=subprocess.DEVNULL,
                        capture_output=True,
                        timeout=20,
                    )
                    self.assertEqual(out.returncode, 0, out.stderr)
                    with open(runs) as f:
                        self.assertEqual(f.read(), "run\n", "not tried again")


class Launcher(unittest.TestCase):
    """`launcher.sh --list` on desktop entries in a temp dir, with only the tools it needs on PATH."""

    def entries(self, files):
        with tempfile.TemporaryDirectory() as d:
            apps = os.path.join(d, "data", "applications")
            os.makedirs(apps)
            for name, text in files.items():
                with open(os.path.join(apps, name), "w") as f:
                    f.write("[Desktop Entry]\n" + text)
            tools = os.path.join(d, "bin")
            os.mkdir(tools)
            for tool in ("find", "sort", "awk", "sed", "sh"):
                os.symlink(shutil.which(tool), os.path.join(tools, tool))
            env = {
                "PATH": tools,
                "HOME": d,
                "XDG_DATA_HOME": os.path.join(d, "data"),
                "XDG_DATA_DIRS": os.path.join(d, "none"),
                "XDG_CURRENT_DESKTOP": "orrery",
            }
            script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "launcher.sh")
            out = subprocess.run([shutil.which("sh"), script, "--list"], env=env, capture_output=True, text=True)
            self.assertEqual(out.returncode, 0, out.stderr)
            return [tuple(line.split("\t")) for line in out.stdout.splitlines() if "\tcmd\t" not in line]

    def test_exec_lines_are_unescaped_and_lose_their_field_codes(self):
        got = self.entries({
            "a.desktop": 'Type=Application\nName=A\nExec=a --x "%u" %F 100%%\n',
            # \s is a space and \\ a backslash (the desktop file's own escapes), then sh's quoting stays as it is.
            "b.desktop": 'Name=B\nExec=b\\sc \\\\\\\\ "q\\\\\\\\$x"\nTerminal=true\n',
            "c.desktop": "Name=C\nExec=c\n[Desktop Action new]\nName=Other\nExec=other\n",
        })
        self.assertEqual(got, [
            ("A", "app", "a --x 100%"),
            ("B", "term", 'b c \\\\ "q\\\\$x"'),
            ("C", "app", "c"),
        ])

    def test_hidden_missing_and_other_desktops_entries_are_left_out(self):
        got = self.entries({
            "hidden.desktop": "Name=H\nExec=h\nNoDisplay=true\n",
            "gone.desktop": "Name=G\nExec=g\nTryExec=no-such-program-here\n",
            "gone-path.desktop": "Name=P\nExec=p\nTryExec=/no/such/program\n",
            "there.desktop": "Name=T\nExec=t\nTryExec=awk\n",
            "only.desktop": "Name=O\nExec=o\nOnlyShowIn=GNOME;KDE;\n",
            "ours.desktop": "Name=U\nExec=u\nOnlyShowIn=GNOME;orrery;\n",
            "not.desktop": "Name=N\nExec=n\nNotShowIn=orrery;\n",
            "link.desktop": "Type=Link\nName=L\nURL=x\nExec=l\n",
        })
        self.assertEqual([g[0] for g in got], ["T", "U"])


class Bars(unittest.TestCase):
    def test_screens_come_from_the_snapshot_and_workspace_and_layout_events(self):
        self.assertEqual(bars.screens_of(snapshot()), {"A", "B"})
        self.assertEqual(bars.screens_of({"event": "workspace", "screens": [{"name": "A"}]}), {"A"})
        self.assertEqual(bars.screens_of({"event": "layout", "screens": [{"name": "C"}]}), {"C"})
        self.assertIsNone(bars.screens_of({"event": "focus", "screen": "A"}))

    def test_a_panel_per_screen(self):
        argv = bars.panel_argv("DP-1", ["-o", "font_size=10"], "/x/bar.py", "py")
        self.assertEqual(argv[:2], ["kitten", "panel"])
        self.assertIn("--output-name=DP-1", argv)
        self.assertIn("--edge=top", argv)
        self.assertEqual(argv[-6:], ["-o", "font_size=10", "py", "/x/bar.py", "--screen", "DP-1"])

    def test_panels_are_asked_to_die_with_bars_py(self):
        # The panel's preexec_fn: the parent-death signal (prctl, Linux; elsewhere it does nothing).
        self.assertTrue(callable(bars.die_with_parent))

    def test_restarts_back_off(self):
        self.assertEqual([bars.restart_delay(n) for n in (0, 1, 2, 3, 10)], [0, 2, 4, 8, 60])


if __name__ == "__main__":
    unittest.main()
