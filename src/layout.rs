//! Layout geometry: grids, slots, their pixel rectangles and neighbours.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlotSpec {
    pub name: Option<String>,
    pub col: u32,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
}

/// Deserialized only as part of a state view (a link carries responses); the config is what validates one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layout {
    pub name: String,
    pub cols: u32,
    pub rows: u32,
    /// Relative column widths, one per column; empty means equal columns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub col_weights: Vec<u32>,
    /// Relative row heights, one per row; empty means equal rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_weights: Vec<u32>,
    pub slots: Vec<SlotSpec>,
}

/// A grid direction, for errors about weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Cols,
    Rows,
}

impl std::fmt::Display for Axis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Axis::Cols => "columns",
            Axis::Rows => "rows",
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("layout has no slots")]
    Empty,
    #[error("grid needs at least one column and one row")]
    ZeroGrid,
    #[error("slot {slot} lies outside the grid")]
    OutOfGrid { slot: usize },
    #[error("slots {a} and {b} overlap")]
    Overlap { a: usize, b: usize },
    #[error("cell col={col} row={row} is in no slot")]
    Gap { col: u32, row: u32 },
    #[error("slot name {name:?} is used twice")]
    DuplicateName { name: String },
    #[error("grid is larger than {MAX_GRID} cells in a direction")]
    TooBig,
    /// More slots than cells: some must overlap. Checked early so a huge slot list stays cheap to refuse.
    #[error("layout has more slots than grid cells")]
    TooManySlots,
    #[error("{got} weights for {want} {axis}")]
    WeightCount { axis: Axis, got: usize, want: u32 },
    #[error("weight {} of the {axis} is not 1 to {MAX_WEIGHT}", index + 1)]
    WeightRange { axis: Axis, index: usize },
    /// Measured at `REFERENCE`, so the answer does not depend on the output.
    #[error(
        "slot {slot} is only {}x{} at {}x{} (at least {MIN_SLOT} px each way)",
        rect.w, rect.h, REFERENCE.w, REFERENCE.h
    )]
    TooSmall { slot: usize, rect: Rect },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

pub const MAX_GRID: u32 = 12;
/// The largest weight; with at most 12 of them a grid line's numerator stays far inside i64.
pub const MAX_WEIGHT: u32 = 10_000;
/// The smallest slot side at `REFERENCE`, in pixels.
pub const MIN_SLOT: i32 = 64;

impl SlotSpec {
    fn cells_overlap(&self, other: &SlotSpec) -> bool {
        self.col < other.col + other.cols
            && other.col < self.col + self.cols
            && self.row < other.row + other.rows
            && other.row < self.row + self.rows
    }

    fn covers(&self, col: u32, row: u32) -> bool {
        (self.col..self.col + self.cols).contains(&col) && (self.row..self.row + self.rows).contains(&row)
    }
}

/// Start of grid line `i` of `n` across `len` from `origin`: `origin + len * sum(w[..i]) / sum(w)`, with all
/// weights 1 when `weights` is not one per line gap (empty, or an unvalidated mismatch). Integer math, so the lines
/// tile `len` exactly.
fn line(origin: i32, len: i32, i: u32, n: u32, weights: &[u32]) -> i32 {
    let (before, total) = if weights.len() == n as usize && weights.iter().any(|&w| w > 0) {
        let upto = (i as usize).min(weights.len());
        let sum = |w: &[u32]| w.iter().map(|&w| i64::from(w)).sum::<i64>();
        // Lines past the grid (unvalidated spans) sit one full length per extra step, as unweighted ones do.
        let extra = i64::from(i) - upto as i64;
        let total = sum(weights);
        (sum(&weights[..upto]) + extra * total, total)
    } else {
        (i64::from(i), i64::from(n))
    };
    // In i64 because origin + len * before can leave i32; out-of-range results saturate.
    let v = i64::from(origin) + i64::from(len) * before / total;
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Length of the overlap of `[a0, a1)` and `[b0, b1)`, 0 when disjoint.
fn overlap(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

impl Layout {
    /// Grid 1..=12 each way; weights, where given, one per column/row and each 1..=10_000; every slot inside it,
    /// at least `MIN_SLOT` px each way at `REFERENCE` with a non-zero span; no overlaps; every cell covered;
    /// slot names unique.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.cols == 0 || self.rows == 0 {
            return Err(LayoutError::ZeroGrid);
        }
        if self.cols > MAX_GRID || self.rows > MAX_GRID {
            return Err(LayoutError::TooBig);
        }
        if self.slots.is_empty() {
            return Err(LayoutError::Empty);
        }
        if self.slots.len() > (self.cols * self.rows) as usize {
            return Err(LayoutError::TooManySlots);
        }
        for (axis, weights, want) in [
            (Axis::Cols, &self.col_weights, self.cols),
            (Axis::Rows, &self.row_weights, self.rows),
        ] {
            if weights.is_empty() {
                continue;
            }
            if weights.len() != want as usize {
                return Err(LayoutError::WeightCount {
                    axis,
                    got: weights.len(),
                    want,
                });
            }
            if let Some(index) = weights.iter().position(|w| !(1..=MAX_WEIGHT).contains(w)) {
                return Err(LayoutError::WeightRange { axis, index });
            }
        }
        for (i, s) in self.slots.iter().enumerate() {
            let inside = s.cols > 0
                && s.rows > 0
                && s.col.checked_add(s.cols).is_some_and(|end| end <= self.cols)
                && s.row.checked_add(s.rows).is_some_and(|end| end <= self.rows);
            if !inside {
                return Err(LayoutError::OutOfGrid { slot: i });
            }
        }
        for (i, s) in self.slots.iter().enumerate() {
            if let Some(name) = &s.name
                && self.slots[..i].iter().any(|t| t.name.as_ref() == Some(name))
            {
                return Err(LayoutError::DuplicateName { name: name.clone() });
            }
        }
        for a in 0..self.slots.len() {
            for b in a + 1..self.slots.len() {
                if self.slots[a].cells_overlap(&self.slots[b]) {
                    return Err(LayoutError::Overlap { a, b });
                }
            }
        }
        for row in 0..self.rows {
            for col in 0..self.cols {
                if !self.slots.iter().any(|s| s.covers(col, row)) {
                    return Err(LayoutError::Gap { col, row });
                }
            }
        }
        if let Some((slot, rect)) =
            (self.rects(REFERENCE).into_iter().enumerate()).find(|(_, r)| r.w < MIN_SLOT || r.h < MIN_SLOT)
        {
            return Err(LayoutError::TooSmall { slot, rect });
        }
        Ok(())
    }

    /// Slot rectangles in slot order. For a valid layout, `area.w, area.h >= 0` and `area` within i32, they tile
    /// `area` exactly; outside that the coordinates saturate rather than panic.
    pub fn rects(&self, area: Rect) -> Vec<Rect> {
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);
        self.slots
            .iter()
            .map(|s| {
                let (cw, rw) = (&self.col_weights, &self.row_weights);
                let x0 = line(area.x, area.w, s.col, cols, cw);
                let x1 = line(area.x, area.w, s.col.saturating_add(s.cols), cols, cw);
                let y0 = line(area.y, area.h, s.row, rows, rw);
                let y1 = line(area.y, area.h, s.row.saturating_add(s.rows), rows, rw);
                Rect {
                    x: x0,
                    y: y0,
                    w: x1.saturating_sub(x0),
                    h: y1.saturating_sub(y0),
                }
            })
            .collect()
    }

    /// The slot reached by moving from `from` in `dir`: among slots wholly beyond that edge which overlap it on
    /// the other axis, the nearest wins, then the largest overlap, then the lowest index. Measured in grid cells,
    /// so the answer does not depend on the output size or its remainder pixels.
    pub fn neighbour(&self, from: usize, dir: Dir) -> Option<usize> {
        // (start, end) of a slot along x and y, in cells; i64 so spans of an unvalidated layout cannot overflow.
        let span = |s: &SlotSpec| {
            let (c, r) = (i64::from(s.col), i64::from(s.row));
            ((c, c + i64::from(s.cols)), (r, r + i64::from(s.rows)))
        };
        let (fx, fy) = span(self.slots.get(from)?);
        self.slots
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != from)
            .filter_map(|(i, s)| {
                let (x, y) = span(s);
                let (gap, shared) = match dir {
                    Dir::Left => (fx.0 - x.1, overlap(fy.0, fy.1, y.0, y.1)),
                    Dir::Right => (x.0 - fx.1, overlap(fy.0, fy.1, y.0, y.1)),
                    Dir::Up => (fy.0 - y.1, overlap(fx.0, fx.1, x.0, x.1)),
                    Dir::Down => (y.0 - fy.1, overlap(fx.0, fx.1, x.0, x.1)),
                };
                (gap >= 0 && shared > 0).then_some((gap, std::cmp::Reverse(shared), i))
            })
            .min()
            .map(|(_, _, i)| i)
    }
}

/// The area `validate` measures slots at (a 4K screen): no slot may be smaller than `MIN_SLOT` there.
pub const REFERENCE: Rect = Rect {
    x: 0,
    y: 0,
    w: 3840,
    h: 2160,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(col: u32, row: u32, cols: u32, rows: u32) -> SlotSpec {
        SlotSpec {
            name: None,
            col,
            row,
            cols,
            rows,
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    /// A 4K wall: 3x3 cells, a 2x2 hero top-right, five 1x1 slots.
    fn wall() -> Layout {
        Layout {
            name: "wall".into(),
            cols: 3,
            rows: 3,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![
                slot(0, 0, 1, 1),
                slot(0, 1, 1, 1),
                slot(0, 2, 1, 1),
                slot(1, 0, 2, 2),
                slot(1, 2, 1, 1),
                slot(2, 2, 1, 1),
            ],
        }
    }

    const UHD: Rect = Rect {
        x: 0,
        y: 0,
        w: 3840,
        h: 2160,
    };

    #[test]
    fn wall_rects_are_exact() {
        let l = wall();
        assert_eq!(l.validate(), Ok(()));
        assert_eq!(
            l.rects(UHD),
            vec![
                rect(0, 0, 1280, 720),
                rect(0, 720, 1280, 720),
                rect(0, 1440, 1280, 720),
                rect(1280, 0, 2560, 1440),
                rect(1280, 1440, 1280, 720),
                rect(2560, 1440, 1280, 720),
            ]
        );
    }

    #[test]
    fn rects_follow_the_area_origin() {
        let l = wall();
        let r = l.rects(rect(100, 50, 3840, 2160));
        assert_eq!(r[3], rect(1380, 50, 2560, 1440));
    }

    #[test]
    fn remainder_columns_tile_without_gaps() {
        let l = Layout {
            name: "thirds".into(),
            cols: 3,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![slot(0, 0, 1, 1), slot(1, 0, 1, 1), slot(2, 0, 1, 1)],
        };
        let r = l.rects(rect(0, 0, 1000, 100));
        assert_eq!(r.iter().map(|r| r.w).collect::<Vec<_>>(), vec![333, 333, 334]);
        assert_eq!(r.iter().map(|r| r.w).sum::<i32>(), 1000);
        assert_eq!(r[0].x + r[0].w, r[1].x);
        assert_eq!(r[1].x + r[1].w, r[2].x);
    }

    #[test]
    fn validate_refuses_empty() {
        let l = Layout {
            name: "e".into(),
            cols: 1,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![],
        };
        assert_eq!(l.validate(), Err(LayoutError::Empty));
    }

    #[test]
    fn validate_refuses_zero_grid() {
        let l = Layout {
            name: "z".into(),
            cols: 0,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![slot(0, 0, 1, 1)],
        };
        assert_eq!(l.validate(), Err(LayoutError::ZeroGrid));
    }

    #[test]
    fn validate_refuses_slot_outside_grid() {
        let mut l = wall();
        l.slots[5] = slot(2, 2, 2, 1);
        assert_eq!(l.validate(), Err(LayoutError::OutOfGrid { slot: 5 }));
    }

    #[test]
    fn validate_refuses_zero_span_slot() {
        let mut l = wall();
        l.slots[5] = slot(2, 2, 0, 1);
        assert_eq!(l.validate(), Err(LayoutError::OutOfGrid { slot: 5 }));
    }

    #[test]
    fn validate_refuses_overlap() {
        let mut l = wall();
        l.slots[4] = slot(1, 1, 1, 2);
        assert_eq!(l.validate(), Err(LayoutError::Overlap { a: 3, b: 4 }));
    }

    #[test]
    fn validate_refuses_gap() {
        let mut l = wall();
        l.slots.pop();
        assert_eq!(l.validate(), Err(LayoutError::Gap { col: 2, row: 2 }));
    }

    #[test]
    fn validate_refuses_duplicate_names() {
        let mut l = wall();
        l.slots[0].name = Some("hero".into());
        l.slots[3].name = Some("hero".into());
        assert_eq!(l.validate(), Err(LayoutError::DuplicateName { name: "hero".into() }));
    }

    #[test]
    fn validate_refuses_thirteen_columns() {
        let l = Layout {
            name: "big".into(),
            cols: 13,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: (0..13).map(|c| slot(c, 0, 1, 1)).collect(),
        };
        assert_eq!(l.validate(), Err(LayoutError::TooBig));
        let ok = Layout {
            name: "max".into(),
            cols: 12,
            rows: 12,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![slot(0, 0, 12, 12)],
        };
        assert_eq!(ok.validate(), Ok(()));
    }

    #[test]
    fn neighbour_right_from_top_left_is_the_hero() {
        assert_eq!(wall().neighbour(0, Dir::Right), Some(3));
    }

    #[test]
    fn neighbour_down_from_hero_ties_to_lowest_index() {
        // Both bottom slots share one grid column with the hero: the lower index wins, whatever the pixels.
        assert_eq!(wall().neighbour(3, Dir::Down), Some(4));
    }

    #[test]
    fn neighbour_left_from_bottom_left_is_none() {
        assert_eq!(wall().neighbour(2, Dir::Left), None);
    }

    #[test]
    fn neighbour_prefers_nearest_then_largest_overlap() {
        let l = wall();
        // From the middle-left slot right: only the hero overlaps it vertically.
        assert_eq!(l.neighbour(1, Dir::Right), Some(3));
        // From bottom-right up: the hero is the nearest above.
        assert_eq!(l.neighbour(5, Dir::Up), Some(3));
        // From bottom-right left: bottom-middle is nearer than bottom-left.
        assert_eq!(l.neighbour(5, Dir::Left), Some(4));
        // From the hero left: all three left slots are adjacent; each shares one row with it, tie -> 0.
        assert_eq!(l.neighbour(3, Dir::Left), Some(0));
        // Nothing above the top row.
        assert_eq!(l.neighbour(0, Dir::Up), None);
    }

    #[test]
    fn neighbour_out_of_range_is_none() {
        assert_eq!(wall().neighbour(99, Dir::Right), None);
    }

    /// 4x4: a full-height left column, a 3x3 block, then a 1x1 and a 2x1 along the bottom.
    fn ragged() -> Layout {
        Layout {
            name: "ragged".into(),
            cols: 4,
            rows: 4,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![slot(0, 0, 1, 4), slot(1, 0, 3, 3), slot(1, 3, 1, 1), slot(2, 3, 2, 1)],
        }
    }

    #[test]
    fn neighbour_in_a_ragged_layout() {
        let l = ragged();
        assert_eq!(l.validate(), Ok(()));
        // The 2x1 shares two columns with the block, the 1x1 one.
        assert_eq!(l.neighbour(1, Dir::Down), Some(3));
        // The block shares three rows with the left column, the 1x1 one.
        assert_eq!(l.neighbour(0, Dir::Right), Some(1));
        // The 1x1 is adjacent; the left column is a cell further.
        assert_eq!(l.neighbour(3, Dir::Left), Some(2));
        assert_eq!(l.neighbour(2, Dir::Up), Some(1));
        assert_eq!(l.neighbour(3, Dir::Right), None);
        assert_eq!(l.neighbour(2, Dir::Left), Some(0));
    }

    #[test]
    fn validate_refuses_more_slots_than_cells() {
        let l = Layout {
            name: "crowd".into(),
            cols: 2,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![slot(0, 0, 1, 1), slot(1, 0, 1, 1), slot(1, 0, 1, 1)],
        };
        assert_eq!(l.validate(), Err(LayoutError::TooManySlots));
    }

    #[test]
    fn rects_saturate_instead_of_panicking() {
        let l = Layout {
            name: "one".into(),
            cols: 1,
            rows: 1,
            col_weights: vec![],
            row_weights: vec![],
            slots: vec![slot(0, 0, 1, 1)],
        };
        let r = l.rects(rect(i32::MAX, i32::MAX, i32::MAX, i32::MAX));
        assert_eq!((r[0].x, r[0].y), (i32::MAX, i32::MAX));
    }

    fn weighted(cols: Vec<u32>, rows: Vec<u32>) -> Layout {
        let (nc, nr) = (cols.len() as u32, rows.len() as u32);
        Layout {
            name: "weighted".into(),
            cols: nc,
            rows: nr,
            col_weights: cols,
            row_weights: rows,
            slots: (0..nr).flat_map(|r| (0..nc).map(move |c| slot(c, r, 1, 1))).collect(),
        }
    }

    /// Every rect's edges meet its neighbours' and the outer ones meet the area's: no gaps, no overlaps.
    fn assert_tiles(l: &Layout, area: Rect) {
        let r = l.rects(area);
        let area_px: i64 = i64::from(area.w) * i64::from(area.h);
        assert_eq!(
            r.iter().map(|r| i64::from(r.w) * i64::from(r.h)).sum::<i64>(),
            area_px,
            "{r:?}"
        );
        for (i, a) in r.iter().enumerate() {
            assert!(a.x >= area.x && a.y >= area.y, "{a:?}");
            assert!(a.x + a.w <= area.x + area.w && a.y + a.h <= area.y + area.h, "{a:?}");
            for b in &r[i + 1..] {
                let apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn weighted_columns_split_by_their_share() {
        let l = weighted(vec![81, 81, 94], vec![1, 1]);
        assert_eq!(l.validate(), Ok(()));
        let r = l.rects(UHD);
        // 3840 * 81 / 256 = 1215, 3840 * 162 / 256 = 2430.
        assert_eq!(r[0], rect(0, 0, 1215, 1080));
        assert_eq!(r[1], rect(1215, 0, 1215, 1080));
        assert_eq!(r[2], rect(2430, 0, 1410, 1080));
        assert_eq!(r[5], rect(2430, 1080, 1410, 1080));
        assert_tiles(&l, UHD);
        assert_tiles(&l, rect(7, -3, 1001, 997));
    }

    #[test]
    fn weighted_rows_and_spans_tile_exactly() {
        // The wall with a wider left column and a shorter bottom row.
        let mut l = wall();
        l.col_weights = vec![3, 2, 2];
        l.row_weights = vec![2, 2, 1];
        assert_eq!(l.validate(), Ok(()));
        let r = l.rects(UHD);
        // Columns 0, 1645, 2742, 3840 (3840 * 3 / 7 rounds down); rows 0, 864, 1728, 2160.
        assert_eq!(r[0], rect(0, 0, 1645, 864));
        assert_eq!(r[3], rect(1645, 0, 2195, 1728));
        assert_eq!(r[4], rect(1645, 1728, 1097, 432));
        assert_eq!(r[5], rect(2742, 1728, 1098, 432));
        assert_tiles(&l, UHD);
        assert_tiles(&l, rect(0, 0, 1919, 1081));
    }

    #[test]
    fn equal_weights_give_the_unweighted_rects() {
        for area in [UHD, rect(0, 0, 1000, 100), rect(5, 9, 1919, 1079)] {
            let mut l = wall();
            let plain = l.rects(area);
            l.col_weights = vec![7, 7, 7];
            l.row_weights = vec![1, 1, 1];
            assert_eq!(l.rects(area), plain);
        }
    }

    #[test]
    fn validate_refuses_bad_weights() {
        let mut l = wall();
        l.col_weights = vec![1, 1];
        assert_eq!(
            l.validate(),
            Err(LayoutError::WeightCount {
                axis: Axis::Cols,
                got: 2,
                want: 3
            })
        );
        let mut l = wall();
        l.row_weights = vec![1, 0, 1];
        assert_eq!(
            l.validate(),
            Err(LayoutError::WeightRange {
                axis: Axis::Rows,
                index: 1
            })
        );
        let mut l = wall();
        l.col_weights = vec![1, MAX_WEIGHT + 1, 1];
        assert_eq!(
            l.validate(),
            Err(LayoutError::WeightRange {
                axis: Axis::Cols,
                index: 1
            })
        );
        l.col_weights = vec![MAX_WEIGHT; 3];
        assert_eq!(l.validate(), Ok(()));
    }

    #[test]
    fn heavy_weights_on_a_huge_area_do_not_overflow() {
        let l = weighted(vec![MAX_WEIGHT; 12], vec![1]);
        let r = l.rects(rect(0, 0, i32::MAX, 1));
        assert_eq!(r[11].x + r[11].w, i32::MAX);
    }

    #[test]
    fn mismatched_weights_on_an_unvalidated_layout_do_not_panic() {
        let mut l = wall();
        l.col_weights = vec![1];
        l.row_weights = vec![0, 0, 0];
        let _ = l.rects(UHD);
    }

    #[test]
    fn empty_weights_are_not_serialized() {
        let json = serde_json::to_string(&wall()).expect("serializes");
        assert!(!json.contains("weights"), "{json}");
        let mut l = wall();
        l.col_weights = vec![1, 2, 3];
        assert!(
            serde_json::to_string(&l)
                .expect("serializes")
                .contains("\"col_weights\":[1,2,3]")
        );
    }
    #[test]
    fn slots_under_64_px_at_the_reference_are_refused() {
        // widths 1 10000: the first column is 3840 / 10001 = 0 px wide.
        let mut l = weighted(vec![1, MAX_WEIGHT], vec![1]);
        let err = l.validate().expect_err("a 0 px slot");
        assert_eq!(
            err,
            LayoutError::TooSmall {
                slot: 0,
                rect: rect(0, 0, 0, 2160)
            }
        );
        assert!(err.to_string().contains("0x2160"), "{err}");
        // 63 px is too small, 64 px is fine: 3840 * 21 / 1280 = 63, 3840 * 64 / 3840 = 64.
        l.col_weights = vec![21, 1259];
        assert!(matches!(l.validate(), Err(LayoutError::TooSmall { slot: 0, .. })));
        l.col_weights = vec![64, 3776];
        assert_eq!(l.validate(), Ok(()));
        // Rows too.
        let mut l = weighted(vec![1], vec![MAX_WEIGHT, 1]);
        assert!(matches!(l.validate(), Err(LayoutError::TooSmall { slot: 1, .. })));
        l.row_weights = vec![1, 1];
        assert_eq!(l.validate(), Ok(()));
        // The largest grid is far above the floor.
        assert_eq!(weighted(vec![1; 12], vec![1; 12]).validate(), Ok(()));
    }

    #[test]
    fn rect_serializes() {
        let r = rect(1, 2, 3, 4);
        let json = serde_json::to_string(&r).expect("serializes");
        assert_eq!(json, r#"{"x":1,"y":2,"w":3,"h":4}"#);
        assert_eq!(serde_json::from_str::<Rect>(&json).expect("deserializes"), r);
        assert_eq!(Rect::default(), rect(0, 0, 0, 0));
        assert!(serde_json::to_string(&ragged()).is_ok());
    }
}
