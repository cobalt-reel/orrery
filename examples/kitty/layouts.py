#!/usr/bin/env python3
"""A layout picker: fzf over the config's layouts, each drawn to the focused screen's proportions as it is passed
over, in a kitty panel over everything that takes the keyboard (orrery.kdl binds this to Mod+backslash). Enter lays
out the workspace on the screen that was focused when the picker opened (`orrery msg layout NAME SCREEN`); Escape
closes it. Needs fzf.

    layouts.py                          open the panel (it runs `layouts.py --pick` inside)
    layouts.py --pick                   the picker itself, in this terminal
    layouts.py --list                   print the entries: NAME <tab> NOTE <tab> DRAWING-SPEC
    layouts.py --draw SPEC COLS LINES   draw one entry's layout in a box COLS x LINES (the preview)

$ORRERY names orrery (orrery sets it; default: `orrery` on PATH). Python 3 standard library only.
"""

import json
import os
import shutil
import subprocess
import sys

# A terminal cell is about twice as tall as it is wide.
CELL_ASPECT = 2.0

# Box-drawing characters by the directions their lines leave a cell in: (up, down, left, right).
BOX = {
    (0, 0, 1, 1): "─", (1, 1, 0, 0): "│", (0, 1, 0, 1): "┌", (0, 1, 1, 0): "┐", (1, 0, 0, 1): "└",
    (1, 0, 1, 0): "┘", (1, 1, 0, 1): "├", (1, 1, 1, 0): "┤", (0, 1, 1, 1): "┬", (1, 0, 1, 1): "┴",
    (1, 1, 1, 1): "┼", (0, 0, 1, 0): "─", (0, 0, 0, 1): "─", (1, 0, 0, 0): "│", (0, 1, 0, 0): "│",
}


def orrery_program():
    return os.environ.get("ORRERY") or "orrery"


def lines_at(n, length, weights):
    """The n + 1 grid lines across `length` cells (0 .. length - 1), weighted as orrery weights its grids."""
    if len(weights) != n or not any(w > 0 for w in weights):
        weights = [1] * n
    total = sum(weights)
    out, before = [], 0
    for i in range(n + 1):
        out.append(round((length - 1) * before / total))
        if i < n:
            before += weights[i]
    return out


def draw(layout, cols, lines):
    """`layout` (as `orrery msg state` lists one) as lines of text, `cols` x `lines` cells: every slot a box, its
    number (1-based, in config order) and name inside when they fit."""
    if cols < 2 or lines < 2:
        return []
    xs = lines_at(layout["cols"], cols, layout.get("col_weights") or [])
    ys = lines_at(layout["rows"], lines, layout.get("row_weights") or [])
    dirs = [[[0, 0, 0, 0] for _ in range(cols)] for _ in range(lines)]
    labels = []
    for i, s in enumerate(layout["slots"]):
        x0, x1 = xs[s["col"]], xs[min(s["col"] + s.get("cols", 1), len(xs) - 1)]
        y0, y1 = ys[s["row"]], ys[min(s["row"] + s.get("rows", 1), len(ys) - 1)]
        for y in (y0, y1):
            for x in range(x0, x1 + 1):
                dirs[y][x][2] |= x > x0
                dirs[y][x][3] |= x < x1
        for x in (x0, x1):
            for y in range(y0, y1 + 1):
                dirs[y][x][0] |= y > y0
                dirs[y][x][1] |= y < y1
        label = str(i + 1) + (f" {s['name']}" if s.get("name") else "")
        labels.append((x0, x1, y0, y1, label))
    grid = [[BOX.get(tuple(d), " ") for d in row] for row in dirs]
    for x0, x1, y0, y1, label in labels:
        room = x1 - x0 - 1
        if y1 - y0 < 2 or room < 1:
            continue
        text = label if len(label) <= room else label.split(" ")[0][:room]
        y = (y0 + y1) // 2
        x = x0 + 1 + (room - len(text)) // 2
        grid[y][x : x + len(text)] = list(text)
    return ["".join(row) for row in grid]


def fit(area, cols, lines):
    """The largest box of at most `cols` x `lines` cells with the proportions of `area` ({w, h} in pixels)."""
    w, h = max(1, area.get("w", 16)), max(1, area.get("h", 9))
    box_cols = cols
    box_lines = round(cols * h / w / CELL_ASPECT)
    if box_lines > lines:
        box_lines = lines
        box_cols = round(lines * CELL_ASPECT * w / h)
    return max(2, min(box_cols, cols)), max(2, min(box_lines, lines))


def state():
    out = subprocess.run([orrery_program(), "msg", "state"], capture_output=True, text=True, check=False)
    reply = json.loads(out.stdout or "{}")
    if not reply.get("ok"):
        raise SystemExit(f"layouts.py: orrery msg state: {reply.get('error') or out.stderr.strip() or 'no answer'}")
    return reply["state"]


def focused_screen(st):
    return next((m for m in st.get("monitors") or [] if m.get("focused")), None)


def entries(st):
    """NAME <tab> NOTE <tab> SPEC lines, in config order; SPEC is the layout and the screen's area, as JSON."""
    screen = focused_screen(st) or {}
    area = {"w": 16, "h": 9}
    slots = screen.get("slots") or []
    if slots:
        right = max(s["rect"]["x"] + s["rect"]["w"] for s in slots)
        bottom = max(s["rect"]["y"] + s["rect"]["h"] for s in slots)
        area = {"w": right - min(s["rect"]["x"] for s in slots), "h": bottom - min(s["rect"]["y"] for s in slots)}
    out = []
    for layout in st.get("layouts") or []:
        n = len(layout.get("slots") or [])
        note = f"{n} slot{'s' if n != 1 else ''}"
        if layout.get("name") == screen.get("layout"):
            note += "  (now)"
        spec = json.dumps({"layout": layout, "area": area}, separators=(",", ":"))
        name = layout.get("name", "?").replace("\t", " ").replace("\n", " ")
        out.append(f"{name}\t{note}\t{spec}")
    return out


def pick():
    if shutil.which("fzf") is None:
        print("layouts.py: needs fzf", file=sys.stderr)
        return 1
    st = state()
    screen = focused_screen(st)
    if screen is None:
        print("layouts.py: no screen", file=sys.stderr)
        return 1
    listed = entries(st)
    if not listed:
        print("layouts.py: orrery lists no layouts (an orrery older than cycle-layout?)", file=sys.stderr)
        return 1
    me = os.path.abspath(__file__)
    preview = f"{sys.executable} {me} --draw {{3}} $FZF_PREVIEW_COLUMNS $FZF_PREVIEW_LINES"
    current = next((i for i, e in enumerate(listed) if e.split("\t")[1].endswith("(now)")), 0)
    done = subprocess.run(
        ["fzf", "--delimiter=\t", "--with-nth=1,2", "--nth=1", "--tiebreak=index", "--layout=reverse",
         "--info=inline", f"--prompt=layout ({screen['name']}): ", "--preview", preview,
         "--preview-window=right,65%,border-left", f"--bind=load:pos({current + 1})"],
        input="\n".join(listed) + "\n", stdout=subprocess.PIPE, text=True, check=False,
    )
    if done.returncode != 0 or not done.stdout.strip():
        return 0
    name = done.stdout.split("\t")[0]
    subprocess.run([orrery_program(), "msg", "layout", name, screen["name"]], stdout=subprocess.DEVNULL, check=False)
    return 0


def main(argv):
    if not argv:
        os.execvp("kitten", ["kitten", "panel", "--edge=center-sized", "--lines=24", "--columns=100",
                             "--layer=overlay", "--focus-policy=exclusive", "--class=orrery-layouts",
                             sys.executable, os.path.abspath(__file__), "--pick"])
    if argv == ["--pick"]:
        return pick()
    if argv == ["--list"]:
        print("\n".join(entries(state())))
        return 0
    if len(argv) == 4 and argv[0] == "--draw":
        spec = json.loads(argv[1])
        cols, lines = fit(spec.get("area") or {}, int(argv[2]), int(argv[3]) - 1)
        print("\n".join(draw(spec["layout"], cols, lines)))
        return 0
    print("usage: layouts.py [--pick | --list | --draw SPEC COLS LINES]", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
