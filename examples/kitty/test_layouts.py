"""The layout picker's drawing and listing: `python3 -m unittest discover -s examples/kitty`.

Pure functions only: no orrery, no kitty, no fzf.
"""

import json
import unittest

import layouts


def slot(col, row, cols=1, rows=1, name=None):
    return {"name": name, "col": col, "row": row, "cols": cols, "rows": rows}


TALL = {"name": "tall", "cols": 2, "rows": 3, "col_weights": [3, 2],
        "slots": [slot(0, 0, rows=3, name="main"), slot(1, 0), slot(1, 1), slot(1, 2)]}
HALVES = {"name": "halves", "cols": 2, "rows": 1, "slots": [slot(0, 0), slot(1, 0)]}


def state(current="tall"):
    rect = lambda x, w: {"x": x, "y": 30, "w": w, "h": 2130}  # noqa: E731
    return {
        "monitors": [
            {"name": "DP-1", "focused": True, "layout": current,
             "slots": [{"rect": rect(0, 2304)}, {"rect": rect(2304, 1536)}]},
            {"name": "HDMI-A-3", "focused": False, "layout": "halves", "slots": []},
        ],
        "layouts": [HALVES, TALL],
    }


class Draw(unittest.TestCase):
    def test_grid_lines_span_the_box_and_follow_the_weights(self):
        self.assertEqual(layouts.lines_at(2, 11, []), [0, 5, 10])
        self.assertEqual(layouts.lines_at(2, 11, [3, 2]), [0, 6, 10])
        self.assertEqual(layouts.lines_at(2, 11, [1]), [0, 5, 10], "a weight count that does not match: equal")

    def test_slots_are_boxes_sharing_their_borders(self):
        self.assertEqual(layouts.draw(HALVES, 9, 3), ["┌───┬───┐", "│ 1 │ 2 │", "└───┴───┘"])

    def test_every_line_is_the_width_asked_and_labels_sit_inside(self):
        lines = layouts.draw(TALL, 40, 13)
        self.assertEqual(len(lines), 13)
        self.assertTrue(all(len(line) == 40 for line in lines))
        self.assertIn("1 main", "\n".join(lines))
        for n in "234":
            self.assertIn(n, "\n".join(lines))
        self.assertEqual(lines[0][0] + lines[0][-1] + lines[-1][0] + lines[-1][-1], "┌┐└┘")
        self.assertIn("├", "".join(line[23] for line in lines), "the right column's slots meet at tees")

    def test_a_label_too_long_keeps_its_number_and_a_box_too_small_has_none(self):
        self.assertIn("1", "".join(layouts.draw(TALL, 9, 13)))
        self.assertNotIn("main", "".join(layouts.draw(TALL, 9, 13)))
        self.assertEqual(layouts.draw(TALL, 1, 1), [])

    def test_the_box_keeps_the_screens_proportions_in_cells(self):
        self.assertEqual(layouts.fit({"w": 3840, "h": 2160}, 80, 40), (80, 22))
        self.assertEqual(layouts.fit({"w": 3840, "h": 2160}, 80, 10), (36, 10), "short: narrower")
        self.assertEqual(layouts.fit({"w": 1000, "h": 1000}, 80, 40), (80, 40))


class List(unittest.TestCase):
    def test_entries_are_in_config_order_with_the_current_one_noted(self):
        rows = [e.split("\t") for e in layouts.entries(state())]
        self.assertEqual([(r[0], r[1]) for r in rows], [("halves", "2 slots"), ("tall", "4 slots  (now)")])
        spec = json.loads(rows[1][2])
        self.assertEqual(spec["layout"], TALL)
        self.assertEqual(spec["area"], {"w": 3840, "h": 2130}, "the focused screen's slots' extent")

    def test_no_layouts_listed_means_no_entries(self):
        self.assertEqual(layouts.entries({"monitors": []}), [])

    def test_a_name_with_a_tab_stays_one_field(self):
        st = state()
        st["layouts"] = [dict(HALVES, name="a\tb")]
        self.assertEqual(layouts.entries(st)[0].split("\t")[0], "a b")


if __name__ == "__main__":
    unittest.main()
