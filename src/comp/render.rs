//! Drawing one output. `scene` decides what is drawn, purely from the stacks: the top window of each slot,
//! clipped to its slot, and the decorations (focus border, tab badges, held tab strips). `render_output` turns
//! that into render elements for the GL renderer.

use anyhow::Context;
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::damage::{OutputDamageTracker, RenderOutputResult};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::CropRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind, render_elements};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTarget};
use smithay::utils::{Physical, Point, Rectangle, Scale};

use super::Orrery;
use crate::config::{Color, Colors};
use crate::layout::Rect;
use crate::stacks::{Stacks, WinId};

/// The focus border's width.
pub const BORDER: i32 = 1;
/// The tab badge's side and its inset from the slot's top-right corner.
pub const BADGE: i32 = 8;
pub const BADGE_INSET: i32 = 6;
/// The held tab strip's height.
pub const STRIP: i32 = 6;
/// The bar along the top of every output while the config file is broken, and its colour (not configurable: a
/// broken config must not be able to hide its own warning).
pub const ERROR_BAR: i32 = 4;
pub const ERROR_RED: Color = Color {
    r: 0xe0,
    g: 0x20,
    b: 0x30,
    a: 0xff,
};

/// The config's sRGB-encoded 0..255 channels scaled to 0..1 (not linearised), as the renderer takes them.
#[must_use]
pub fn color32f(c: Color) -> Color32F {
    let f = |v: u8| f32::from(v) / 255.0;
    Color32F::new(f(c.r), f(c.g), f(c.b), f(c.a))
}

/// The same, premultiplied by alpha, as solid elements are blended.
#[must_use]
pub fn premultiplied(c: Color) -> Color32F {
    let a = f32::from(c.a) / 255.0;
    let f = |v: u8| f32::from(v) / 255.0 * a;
    Color32F::new(f(c.r), f(c.g), f(c.b), a)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Solid {
    pub rect: Rect,
    pub color: Color,
}

/// What one output shows, in output-local coordinates.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scene {
    /// Each slot's top window and the slot it is clipped to.
    pub windows: Vec<(WinId, Rect)>,
    /// Decorations, front to back; all drawn over the windows.
    pub solids: Vec<Solid>,
}

/// The scene for the monitor named `monitor` (empty when there is none). `config_error` adds the red bar, in
/// front of everything.
#[must_use]
pub fn scene(stacks: &Stacks, monitor: &str, colors: &Colors, show_tabs: bool, config_error: bool) -> Scene {
    let mut out = Scene::default();
    let Some((mi, mon)) = stacks.monitors().iter().enumerate().find(|(_, m)| m.name() == monitor) else {
        return out;
    };
    let area = mon.area();
    let local = |r: Rect| Rect {
        x: r.x - area.x,
        y: r.y - area.y,
        ..r
    };
    let rects = mon.layout().rects(area);
    let mut borders = Vec::new();
    let mut badges = Vec::new();
    let mut strips = Vec::new();
    for (i, (slot, rect)) in mon.slots().iter().zip(rects).enumerate() {
        let rect = local(rect);
        if let Some(top) = slot.top() {
            out.windows.push((top, rect));
        }
        // The focused slot is outlined even when empty: it is where the next window opens.
        if mi == stacks.focused_monitor() && i == mon.focus() {
            borders.extend(border(rect, colors.focus));
        }
        if slot.len() > 1 {
            let tabs: Vec<StripTab> = slot
                .tabs()
                .iter()
                .map(|stack| StripTab {
                    depth: stack.len(),
                    attention: stack.iter().any(|&w| stacks.is_marked(w)),
                })
                .collect();
            let marked = tabs.iter().any(|t| t.attention);
            badges.extend(badge(rect, if marked { colors.attention } else { colors.badge }));
            if show_tabs {
                strips.extend(strip(rect, &tabs, slot.current(), colors));
            }
        }
    }
    let bar = config_error
        .then(|| {
            non_empty(Rect {
                x: 0,
                y: 0,
                w: area.w,
                h: ERROR_BAR.min(area.h),
            })
        })
        .flatten()
        .map(|rect| Solid { rect, color: ERROR_RED });
    out.solids = [bar.into_iter().collect(), borders, badges, strips].concat();
    out
}

fn non_empty(rect: Rect) -> Option<Rect> {
    (rect.w > 0 && rect.h > 0).then_some(rect)
}

/// Four BORDER-wide edges just inside the slot, so a border never covers a neighbour; a slot too small for
/// four gets the ones that fit.
fn border(r: Rect, color: Color) -> impl Iterator<Item = Solid> {
    let b = BORDER;
    let mut edges = vec![Rect { h: b.min(r.h), ..r }];
    if r.h > b {
        edges.push(Rect {
            y: r.y + r.h - b,
            h: b,
            ..r
        });
    }
    if r.h > 2 * b {
        let side = Rect {
            y: r.y + b,
            w: b.min(r.w),
            h: r.h - 2 * b,
            ..r
        };
        edges.push(side);
        if r.w > b {
            edges.push(Rect {
                x: r.x + r.w - b,
                w: b,
                ..side
            });
        }
    }
    edges
        .into_iter()
        .filter_map(non_empty)
        .map(move |rect| Solid { rect, color })
}

/// The BADGE square, inset from the top-right corner; none when the slot is too small to hold it.
fn badge(r: Rect, color: Color) -> Option<Solid> {
    let rect = Rect {
        x: r.x + r.w - BADGE_INSET - BADGE,
        y: r.y + BADGE_INSET,
        w: BADGE,
        h: BADGE,
    };
    (r.w >= BADGE + 2 * BADGE_INSET && r.h >= BADGE + 2 * BADGE_INSET).then_some(Solid { rect, color })
}

/// One tab as its strip segment shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StripTab {
    /// Windows in its stack.
    depth: usize,
    /// Its stack holds a marked window.
    attention: bool,
}

/// The same colour at two fifths of its alpha: a tab not shown.
fn dim(c: Color) -> Color {
    Color {
        a: (u16::from(c.a) * 2 / 5) as u8,
        ..c
    }
}

/// A STRIP-high bar along the slot's top, one segment per tab, left to right, split with the layout's integer
/// math so segments tile the width exactly; a 1 px gap separates neighbours when there is room. The current tab
/// is bright, the others dim; a tab holding a marked window is in `attention`, else `badge`. A stack of more
/// than one window punches one background-coloured pip per window into its segment, each in front of it.
fn strip(r: Rect, tabs: &[StripTab], current: usize, colors: &Colors) -> Vec<Solid> {
    let n = i64::try_from(tabs.len()).unwrap_or(i64::MAX).max(1);
    let w = i64::from(r.w);
    let edge = |i: i64| r.x + i32::try_from(w * i / n).unwrap_or(r.w);
    let mut out = Vec::new();
    for (i, tab) in tabs.iter().enumerate() {
        let k = i64::try_from(i).unwrap_or(i64::MAX);
        let (x0, x1) = (edge(k), edge(k + 1));
        let gap = i32::from(k + 1 < n && x1 - x0 > 1);
        let Some(rect) = non_empty(Rect {
            x: x0,
            y: r.y,
            w: x1 - x0 - gap,
            h: STRIP.min(r.h),
        }) else {
            continue;
        };
        let color = if tab.attention { colors.attention } else { colors.badge };
        if tab.depth > 1 {
            out.extend(pips(rect, tab.depth, colors.background));
        }
        out.push(Solid {
            rect,
            color: if i == current { color } else { dim(color) },
        });
    }
    out
}

/// Up to `depth` squares a third of the segment high, left-aligned and inset by the space above them, a pip
/// apart; those that would not fit are dropped.
fn pips(seg: Rect, depth: usize, color: Color) -> impl Iterator<Item = Solid> {
    let side = (seg.h / 3).max(1);
    let inset = (seg.h - side) / 2;
    let room = seg.w - 2 * inset - side;
    let fit = if room < 0 { 0 } else { room / (2 * side) + 1 };
    let count = usize::try_from(fit).unwrap_or(0).min(depth);
    (0..count).map(move |i| Solid {
        rect: Rect {
            x: seg.x + inset + i32::try_from(i).unwrap_or(0) * 2 * side,
            y: seg.y + inset,
            w: side,
            h: side,
        },
        color,
    })
}

render_elements! {
    pub OrreryElement<=GlesRenderer>;
    Window=CropRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Solid=SolidColorRenderElement,
}

/// Solid buffers kept across frames, so an unchanged decoration keeps its id and is not damage.
#[derive(Default)]
pub struct Decorations {
    buffers: Vec<SolidColorBuffer>,
}

/// Puts `solids` into `buffers` (reusing them in order) and returns their elements.
fn solid_elements(buffers: &mut Vec<SolidColorBuffer>, solids: &[Solid]) -> Vec<OrreryElement> {
    let scale = Scale::from(1.0);
    buffers.truncate(solids.len());
    let mut out = Vec::with_capacity(solids.len());
    for (i, solid) in solids.iter().enumerate() {
        let size = (solid.rect.w, solid.rect.h);
        let color = premultiplied(solid.color);
        if let Some(buffer) = buffers.get_mut(i) {
            buffer.update(size, color);
        } else {
            buffers.push(SolidColorBuffer::new(size, color));
        }
        if let Some(buffer) = buffers.get(i) {
            let at: Point<i32, Physical> = (solid.rect.x, solid.rect.y).into();
            out.push(SolidColorRenderElement::from_buffer(buffer, at, scale, 1.0, Kind::Unspecified).into());
        }
    }
    out
}

fn physical(r: Rect) -> Rectangle<i32, Physical> {
    Rectangle::new((r.x, r.y).into(), (r.w, r.h).into())
}

/// Draws the monitor named after `output` (scale 1, output-local coordinates).
pub fn render_output<'d>(
    output: &str,
    state: &Orrery,
    renderer: &mut GlesRenderer,
    framebuffer: &mut GlesTarget<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    decorations: &mut Decorations,
    age: usize,
) -> anyhow::Result<RenderOutputResult<'d>> {
    let scene = scene(
        &state.stacks,
        output,
        &state.config.colors,
        state.held.show_tabs.is_some(),
        state.config_error.is_some(),
    );
    let scale = Scale::from(1.0);
    // Front to back: decorations, then windows.
    let mut elements = solid_elements(&mut decorations.buffers, &scene.solids);

    for (id, slot) in &scene.windows {
        let Some(window) = state.windows.get(id) else {
            continue;
        };
        // The window's geometry origin sits at the slot's origin (as arrange maps it); its surfaces start
        // that much further up-left.
        let at = Point::<i32, Physical>::from((slot.x, slot.y)) - window.geometry().loc.to_physical(1);
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(renderer, at, scale, 1.0);
        // A client drawing beyond its configured size, or a popup reaching past the slot, stays in the slot.
        elements.extend(
            surfaces
                .into_iter()
                .filter_map(|e| CropRenderElement::from_element(e, scale, physical(*slot)))
                .map(OrreryElement::from),
        );
    }

    damage_tracker
        .render_output(
            renderer,
            framebuffer,
            age,
            &elements,
            color32f(state.config.colors.background),
        )
        .context("rendering the output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[test]
    fn colour_is_scaled_to_unit_range() {
        let c = color32f(Color {
            r: 255,
            g: 0,
            b: 51,
            a: 255,
        });
        assert_eq!(c, Color32F::new(1.0, 0.0, 0.2, 1.0));
        let half = premultiplied(Color {
            r: 255,
            g: 0,
            b: 0,
            a: 51,
        });
        assert_eq!(half, Color32F::new(0.2, 0.0, 0.0, 0.2));
    }

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1200,
        h: 900,
    };

    fn wall() -> (Stacks, Vec<Rect>, Colors) {
        let c = config::default_config();
        let wall = c.layouts.iter().find(|l| l.name == "wall").unwrap().clone();
        let rects = wall.rects(AREA);
        let mut s = Stacks::new();
        s.add_monitor("m", AREA, wall);
        // The default focus and badge colours are the same pink; tell them apart here.
        let colors = Colors {
            badge: Color {
                r: 1,
                g: 2,
                b: 3,
                a: 200,
            },
            ..c.colors
        };
        (s, rects, colors)
    }

    fn of_color(scene: &Scene, color: Color) -> Vec<Rect> {
        scene
            .solids
            .iter()
            .filter(|s| s.color == color)
            .map(|s| s.rect)
            .collect()
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.w <= outer.x + outer.w
            && inner.y + inner.h <= outer.y + outer.h
    }

    #[test]
    fn empty_wall_outlines_the_focused_slot_only() {
        let (s, rects, colors) = wall();
        let scene = scene(&s, "m", &colors, true, false);
        assert!(scene.windows.is_empty());
        let r = rects[0];
        let mut edges = of_color(&scene, colors.focus);
        edges.sort_by_key(|e| (e.x, e.y));
        assert_eq!(
            edges,
            [
                Rect {
                    x: 0,
                    y: 0,
                    w: r.w,
                    h: 1
                },
                Rect {
                    x: 0,
                    y: 1,
                    w: 1,
                    h: r.h - 2
                },
                Rect {
                    x: 0,
                    y: r.h - 1,
                    w: r.w,
                    h: 1
                },
                Rect {
                    x: r.w - 1,
                    y: 1,
                    w: 1,
                    h: r.h - 2
                },
            ]
        );
        assert_eq!(
            scene,
            super::scene(&s, "m", &colors, false, false),
            "no strips without tabs"
        );
        assert_eq!(super::scene(&s, "nope", &colors, true, false), Scene::default());
    }

    #[test]
    fn windows_are_the_tops_clipped_to_their_slots() {
        let (mut s, rects, colors) = wall();
        for id in [1, 2, 3] {
            s.open_stacked(id);
        }
        s.focus_slot(crate::layout::Dir::Right);
        s.open_stacked(4);
        let scene = scene(&s, "m", &colors, false, false);
        assert_eq!(scene.windows, [(3, rects[0]), (4, rects[3])]);
        // The border moved to the hero; slot 0 (three windows) has a badge, the hero (one) none.
        let border = of_color(&scene, colors.focus);
        assert_eq!(border.len(), 4);
        assert!(border.iter().all(|e| inside(*e, rects[3])));
        let r = rects[0];
        assert_eq!(
            of_color(&scene, colors.badge),
            [Rect {
                x: r.x + r.w - 14,
                y: r.y + 6,
                w: 8,
                h: 8
            }]
        );
    }

    #[test]
    fn held_tab_strips_split_the_top_edge_per_tab() {
        let (mut s, rects, colors) = wall();
        s.open_stacked(1);
        s.open_stacked(2);
        s.open_tab(3, None, None);
        s.open_tab(4, None, None);
        s.cycle_tab(crate::config::Cycle::Next);
        // Tabs [1, 2], [3], [4], the first current: one segment per tab, not per window.
        let scene = scene(&s, "m", &colors, true, false);
        let r = rects[0];
        let dim = Color {
            a: (u16::from(colors.badge.a) * 2 / 5) as u8,
            ..colors.badge
        };
        let segments: Vec<Solid> = scene.solids.iter().filter(|s| s.rect.h == STRIP).copied().collect();
        assert_eq!(segments.len(), 3);
        assert_eq!(
            [segments[0].color, segments[1].color, segments[2].color],
            [colors.badge, dim, dim],
            "the current tab is bright"
        );
        // Exact tiling of the width with the layout's integer math, a 1 px gap between neighbours.
        let w = r.w;
        let edges = [0, w / 3, w * 2 / 3, w];
        for (i, seg) in segments.iter().enumerate() {
            let gap = i32::from(i < 2);
            assert_eq!(
                seg.rect,
                Rect {
                    x: edges[i],
                    y: 0,
                    w: edges[i + 1] - edges[i] - gap,
                    h: STRIP
                }
            );
        }
        // Front to back: the border over the badge over the strips.
        assert_eq!(scene.solids[0].color, colors.focus);
        assert_eq!(scene.solids.last().map(|s| s.color), Some(dim));
    }

    /// The strip's segments: the full-height solids along the slot's top.
    fn segments(scene: &Scene) -> Vec<Solid> {
        scene.solids.iter().filter(|s| s.rect.h == STRIP).copied().collect()
    }

    #[test]
    fn one_tab_of_depth_three_is_one_segment_with_three_pips() {
        let (mut s, rects, colors) = wall();
        for id in [1, 2, 3] {
            s.open_stacked(id);
        }
        let scene = scene(&s, "m", &colors, true, false);
        let r = rects[0];
        assert_eq!(
            segments(&scene),
            [Solid {
                rect: Rect {
                    x: r.x,
                    y: r.y,
                    w: r.w,
                    h: STRIP
                },
                color: colors.badge
            }],
            "the whole width, bright"
        );
        // A third of the strip high, inset as much as they are below the strip's edge, a pip apart.
        let pips: Vec<Rect> = (0..3)
            .map(|i| Rect {
                x: r.x + 2 + 4 * i,
                y: r.y + 2,
                w: 2,
                h: 2,
            })
            .collect();
        assert_eq!(of_color(&scene, colors.background), pips);
        let first_pip = scene.solids.iter().position(|s| s.color == colors.background);
        let segment = scene.solids.iter().position(|s| s.rect.h == STRIP);
        assert!(first_pip < segment, "pips in front of their segment");

        let hidden = super::scene(&s, "m", &colors, false, false);
        assert!(of_color(&hidden, colors.background).is_empty(), "no strip, no pips");
    }

    #[test]
    fn pips_left_align_and_drop_what_does_not_fit() {
        let colors = config::default_config().colors;
        let r = Rect {
            x: 10,
            y: 20,
            w: 25,
            h: 30,
        };
        let tabs = [
            StripTab {
                depth: 50,
                attention: false,
            },
            StripTab {
                depth: 1,
                attention: false,
            },
        ];
        let solids = strip(r, &tabs, 0, &colors);
        let pips: Vec<Rect> = solids
            .iter()
            .filter(|s| s.color == colors.background)
            .map(|s| s.rect)
            .collect();
        // The first segment is 12 wide less its 1 px gap: room for two 2 px pips with 2 px margins.
        let seg = solids
            .iter()
            .find(|s| s.rect.h == STRIP && s.rect.x == r.x)
            .expect("first segment")
            .rect;
        assert_eq!(seg.w, 11);
        assert_eq!(
            pips,
            [
                Rect {
                    x: 12,
                    y: 22,
                    w: 2,
                    h: 2
                },
                Rect {
                    x: 16,
                    y: 22,
                    w: 2,
                    h: 2
                }
            ],
            "a depth of one has none"
        );
        assert!(pips.iter().all(|p| inside(*p, seg)));
    }

    #[test]
    fn attention_colours_the_badge_and_the_marked_tabs() {
        let (mut s, rects, colors) = wall();
        s.open_stacked(1);
        s.open_stacked(2);
        s.open_tab(3, None, None);
        s.open_tab(4, None, None);
        s.cycle_tab(crate::config::Cycle::Next);
        // Tabs [1, 2], [3], [4], the first current; 1 waits beneath 2 and 4 in the background.
        s.mark(1);
        s.mark(4);
        let r = rects[0];
        let badge = Rect {
            x: r.x + r.w - 14,
            y: r.y + 6,
            w: 8,
            h: 8,
        };
        let plain = scene(&s, "m", &colors, false, false);
        assert_eq!(of_color(&plain, colors.attention), [badge]);
        assert!(of_color(&plain, colors.badge).is_empty());

        let held = scene(&s, "m", &colors, true, false);
        let colours: Vec<Color> = segments(&held).iter().map(|s| s.color).collect();
        assert_eq!(
            colours,
            [colors.attention, dim(colors.badge), dim(colors.attention)],
            "bright or dim by current, attention by mark"
        );
        assert_eq!(of_color(&held, colors.background).len(), 2, "the current tab's depth");

        // Shown windows lose their marks, and the colours go with them.
        s.cycle_tab(crate::config::Cycle::Prev);
        s.cycle_tab(crate::config::Cycle::Next);
        s.close(2);
        let after = scene(&s, "m", &colors, true, false);
        assert!(of_color(&after, colors.attention).is_empty());
        assert!(of_color(&after, dim(colors.attention)).is_empty());
    }

    #[test]
    fn a_config_error_puts_a_red_bar_over_everything() {
        let (mut s, _, colors) = wall();
        s.open_stacked(1);
        s.open_stacked(2);
        let plain = scene(&s, "m", &colors, true, false);
        let flagged = scene(&s, "m", &colors, true, true);
        assert_eq!(
            flagged.solids[0],
            Solid {
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: AREA.w,
                    h: ERROR_BAR
                },
                color: ERROR_RED
            },
            "frontmost, the output's whole width"
        );
        assert_eq!(flagged.solids[1..], plain.solids[..], "everything else unchanged");
        assert_eq!(flagged.windows, plain.windows);
        assert_eq!(
            scene(&s, "nope", &colors, false, true),
            Scene::default(),
            "no monitor, no area"
        );
    }

    #[test]
    fn coordinates_are_output_local() {
        let c = config::default_config();
        let halves = c.layouts.iter().find(|l| l.name == "halves").unwrap().clone();
        let mut s = Stacks::new();
        let area = Rect {
            x: 1000,
            y: 50,
            w: 800,
            h: 600,
        };
        s.add_monitor("right", area, halves);
        s.open_stacked(9);
        let scene = scene(&s, "right", &c.colors, false, false);
        assert_eq!(
            scene.windows,
            [(
                9,
                Rect {
                    x: 0,
                    y: 0,
                    w: 400,
                    h: 600
                }
            )]
        );
        assert!(scene.solids.iter().all(|e| inside(e.rect, Rect { x: 0, y: 0, ..area })));
    }

    #[test]
    fn tiny_slots_get_what_fits_and_never_spill() {
        let r = Rect { x: 5, y: 5, w: 1, h: 1 };
        let colors = config::default_config().colors;
        let edges: Vec<Solid> = border(r, colors.focus).collect();
        assert!(
            !edges.is_empty() && edges.iter().all(|e| inside(e.rect, r)),
            "{edges:?}"
        );
        assert!(badge(Rect { w: 19, ..r }, colors.badge).is_none());
        let tabs = [StripTab {
            depth: 3,
            attention: true,
        }; 5];
        let segs = strip(Rect { w: 2, h: 3, ..r }, &tabs, 4, &colors);
        assert!(
            segs.iter().all(|e| inside(e.rect, Rect { w: 2, h: 3, ..r })),
            "{segs:?}"
        );
    }
}
