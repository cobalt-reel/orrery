//! Drawing one output. `scene` decides what is drawn, purely from the stacks: the top window of each slot,
//! clipped to its slot, and the decorations (focus border, tab badges, held tab strips). `output_elements` turns
//! that into render elements for the GL renderer, between the screen's layer surfaces (background and bottom
//! behind, top and overlay in front; a fullscreen window alone on black, under only the layer surfaces
//! `over_fullscreen` names), with the pointer in front of everything where orrery draws it
//! (`cursor_draw`): while input from the primary moves it, and on bare metal, where no host cursor stands for this
//! machine's own. `render_output` draws them into a framebuffer (nested); bare metal hands them to its
//! `DrmCompositor`.

use std::rc::Rc;

use anyhow::Context;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::damage::{OutputDamageTracker, RenderOutputResult};
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree};
use smithay::backend::renderer::element::utils::CropRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind, render_elements};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTarget};
use smithay::desktop::{PopupManager, WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive as _, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::Layer;

use super::Orrery;
use super::cursor_theme::Image;
use super::layers::{ABOVE, BELOW, layer_origin, over_fullscreen};
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

/// The built-in cursor's side: a light square in a 1 px dark outline, its top-left corner the pointer's point, so it
/// shows on any background.
pub const CURSOR: i32 = 10;
pub const CURSOR_DARK: Color = Color {
    r: 0x10,
    g: 0x10,
    b: 0x10,
    a: 0xff,
};
pub const CURSOR_LIGHT: Color = Color {
    r: 0xf0,
    g: 0xf0,
    b: 0xf0,
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

/// What a fullscreen window is drawn on: it may be smaller than its screen, or see-through.
pub const BACKDROP: Color = Color {
    r: 0,
    g: 0,
    b: 0,
    a: 0xff,
};

/// What one output shows, in output-local coordinates.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scene {
    /// Each slot's top window and the slot it is clipped to; a fullscreen window alone, and the whole screen.
    pub windows: Vec<(WinId, Rect)>,
    /// Decorations, front to back; all drawn over the windows. None around a fullscreen window.
    pub solids: Vec<Solid>,
    /// The black a fullscreen window is drawn on, the whole screen: nothing behind it shows.
    pub backdrop: Option<Solid>,
    /// The red bar of a broken config, in front of everything (layer surfaces too).
    pub bar: Option<Solid>,
}

/// The scene for the monitor named `monitor`, its shown workspace (empty when there is none), on its screen
/// `screen` (global; the monitor's area is only what its layout divides). `config_error` adds the red bar along the
/// screen's top.
#[must_use]
pub fn scene(
    stacks: &Stacks,
    monitor: &str,
    screen: Rect,
    colors: &Colors,
    show_tabs: bool,
    config_error: bool,
) -> Scene {
    let mut out = Scene::default();
    let Some((mi, mon)) = stacks.monitors().iter().enumerate().find(|(_, m)| m.name() == monitor) else {
        return out;
    };
    let Some(ws) = stacks.shown(mi) else {
        return out;
    };
    let local = |r: Rect| Rect {
        x: r.x - screen.x,
        y: r.y - screen.y,
        ..r
    };
    let bar = config_error
        .then(|| {
            non_empty(Rect {
                x: 0,
                y: 0,
                w: screen.w,
                h: ERROR_BAR.min(screen.h),
            })
        })
        .flatten()
        .map(|rect| Solid { rect, color: ERROR_RED });
    if let Some(full) = stacks.fullscreen_on(mi) {
        let rect = local(mon.screen());
        return Scene {
            windows: vec![(full, rect)],
            solids: Vec::new(),
            backdrop: non_empty(rect).map(|rect| Solid { rect, color: BACKDROP }),
            bar,
        };
    }
    let rects = ws.layout().rects(mon.area());
    let mut borders = Vec::new();
    let mut badges = Vec::new();
    let mut strips = Vec::new();
    for (i, (slot, rect)) in ws.slots().iter().zip(rects).enumerate() {
        let rect = local(rect);
        if let Some(top) = slot.top() {
            out.windows.push((top, rect));
        }
        // The focused slot is outlined even when empty: it is where the next window opens.
        if mi == stacks.focused_monitor() && i == ws.focus() {
            borders.extend(border(rect, colors.focus));
        }
        let tabs: Vec<StripTab> = slot
            .tabs()
            .iter()
            .map(|stack| StripTab {
                depth: stack.len(),
                attention: stack.iter().any(|&w| stacks.is_marked(w)),
            })
            .collect();
        let marked = tabs.iter().any(|t| t.attention);
        // A lone window marked (shown, in a slot without focus) has only the badge to say so.
        if slot.len() > 1 || marked {
            badges.extend(badge(rect, if marked { colors.attention } else { colors.badge }));
        }
        if slot.len() > 1 && show_tabs {
            strips.extend(strip(rect, &tabs, slot.current(), colors));
        }
    }
    out.bar = bar;
    out.solids = [borders, badges, strips].concat();
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

/// The same colour at `tenths` of its alpha: a tab not shown.
fn faded(c: Color, tenths: u16) -> Color {
    Color {
        a: (u16::from(c.a) * tenths / 10) as u8,
        ..c
    }
}

/// A STRIP-high bar along the slot's top, one segment per tab, left to right, split with the layout's integer
/// math so segments tile the width exactly; a 1 px gap separates neighbours when there is room. The current tab
/// is bright, the others dim. A tab holding a marked window is in `attention`, else `badge`; it dims to 70%
/// rather than 40%, so a background tab asking for attention stands out. A stack of more than one window
/// punches one background-coloured pip per window into its segment, each in front of it.
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
        let (color, dim) = if tab.attention {
            (colors.attention, 7)
        } else {
            (colors.badge, 4)
        };
        if tab.depth > 1 {
            out.extend(pips(rect, tab.depth, colors.background));
        }
        out.push(Solid {
            rect,
            color: if i == current { color } else { faded(color, dim) },
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

/// The built-in cursor at `at` (output-local), front to back: the light fill, then the dark outline.
#[must_use]
pub fn fallback_cursor((x, y): (i32, i32)) -> [Solid; 2] {
    let fill = Rect {
        x: x + 1,
        y: y + 1,
        w: CURSOR - 2,
        h: CURSOR - 2,
    };
    let outline = Rect {
        x,
        y,
        w: CURSOR,
        h: CURSOR,
    };
    [
        Solid {
            rect: fill,
            color: CURSOR_LIGHT,
        },
        Solid {
            rect: outline,
            color: CURSOR_DARK,
        },
    ]
}

/// What orrery draws for the pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorDraw {
    None,
    /// A named cursor: the xcursor theme's image (`cursor_theme`), else the built-in one (`fallback_cursor`).
    Named(CursorIcon),
    /// The cursor surface a client set.
    Surface(WlSurface),
}

/// What stands for the pointer: nothing where orrery does not draw it (`drawn` false: nested input of this machine's
/// own, the host's cursor stands for it), else the client's cursor surface, or the named cursor (the default one for a
/// surface gone). A cursor a client hid stays hidden.
#[must_use]
pub fn cursor_draw(drawn: bool, image: &CursorImageStatus) -> CursorDraw {
    match image {
        _ if !drawn => CursorDraw::None,
        CursorImageStatus::Hidden => CursorDraw::None,
        CursorImageStatus::Surface(s) if s.alive() => CursorDraw::Surface(s.clone()),
        CursorImageStatus::Surface(_) => CursorDraw::Named(CursorIcon::Default),
        CursorImageStatus::Named(icon) => CursorDraw::Named(*icon),
    }
}

render_elements! {
    pub OrreryElement<=GlesRenderer>;
    Window=CropRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    /// Surfaces drawn whole: the cursor's, layer surfaces' (and their popups).
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Image=MemoryRenderBufferRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
}

/// Solid buffers kept across frames, so an unchanged decoration keeps its id and is not damage (and a moved one is
/// damage only where it was and is); the theme cursor's buffer likewise, while its image stays the same.
#[derive(Default)]
pub struct Decorations {
    buffers: Vec<SolidColorBuffer>,
    bar: Vec<SolidColorBuffer>,
    backdrop: Vec<SolidColorBuffer>,
    cursor: Vec<SolidColorBuffer>,
    cursor_image: Option<(Rc<Image>, MemoryRenderBuffer)>,
}

impl Decorations {
    /// The buffer holding `image`: the kept one while it is the same image, a new one (a new id) when it changed.
    fn cursor_buffer(&mut self, image: Rc<Image>) -> MemoryRenderBuffer {
        if let Some((kept, buffer)) = &self.cursor_image
            && Rc::ptr_eq(kept, &image)
        {
            return buffer.clone();
        }
        // xcursor pixels are little-endian ARGB words, which is DRM's Argb8888.
        let size = (
            i32::try_from(image.width).unwrap_or(0),
            i32::try_from(image.height).unwrap_or(0),
        );
        let buffer =
            MemoryRenderBuffer::from_slice(&image.pixels_rgba, Fourcc::Argb8888, size, 1, Transform::Normal, None);
        self.cursor_image = Some((image, buffer.clone()));
        buffer
    }
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

/// The pointer on output `output`, if `cursor_draw` says to draw it and it is there.
fn cursor_elements(
    output: &str,
    state: &Orrery,
    renderer: &mut GlesRenderer,
    decorations: &mut Decorations,
) -> Vec<OrreryElement> {
    let draw = cursor_draw(state.pointer_drawn(), &state.cursor_image);
    let geometry = (state.outputs.iter())
        .find(|o| o.name() == output)
        .and_then(|o| state.space.output_geometry(o));
    let at = state.seat.get_pointer().map(|p| p.current_location());
    let (Some(geometry), Some(at)) = (geometry, at) else {
        return Vec::new();
    };
    if draw == CursorDraw::None || !geometry.to_f64().contains(at) {
        return Vec::new();
    }
    let local = at - geometry.loc.to_f64();
    let (x, y) = (local.x.floor() as i32, local.y.floor() as i32);
    match draw {
        CursorDraw::None => Vec::new(),
        CursorDraw::Named(icon) => {
            let image = state.cursor_theme.borrow_mut().get(icon);
            let Some(image) = image else {
                return solid_elements(&mut decorations.cursor, &fallback_cursor((x, y)));
            };
            let hotspot = (
                i32::try_from(image.xhot).unwrap_or(0),
                i32::try_from(image.yhot).unwrap_or(0),
            );
            let buffer = decorations.cursor_buffer(image);
            let at = Point::<f64, Physical>::from((f64::from(x - hotspot.0), f64::from(y - hotspot.1)));
            match MemoryRenderBufferRenderElement::from_buffer(renderer, at, &buffer, None, None, None, Kind::Cursor) {
                Ok(element) => vec![OrreryElement::Image(element)],
                Err(e) => {
                    tracing::debug!("cannot upload the cursor image: {e}");
                    solid_elements(&mut decorations.cursor, &fallback_cursor((x, y)))
                }
            }
        }
        CursorDraw::Surface(surface) => {
            let hotspot = with_states(&surface, |states| {
                (states.data_map.get::<CursorImageSurfaceData>())
                    .map(|d| d.lock().unwrap_or_else(std::sync::PoisonError::into_inner).hotspot)
                    .unwrap_or_default()
            });
            let at = Point::<i32, Physical>::from((x - hotspot.x, y - hotspot.y));
            let scale = Scale::from(1.0);
            render_elements_from_surface_tree(renderer, &surface, at, scale, 1.0, Kind::Cursor)
                .into_iter()
                .map(OrreryElement::Surface)
                .collect()
        }
    }
}

/// The layer surfaces of `output` on `layers` (front to back), front to back, as much of each as `kinds` says: its
/// popups (in front) and its own surface with its subsurfaces. Over a `fullscreen` window, only those
/// `over_fullscreen` names.
fn layer_elements(
    output: &Output,
    layers: &[Layer],
    kinds: WindowSurfaceType,
    fullscreen: bool,
    renderer: &mut GlesRenderer,
) -> Vec<OrreryElement> {
    let map = layer_map_for_output(output);
    let scale = Scale::from(1.0);
    let mut out = Vec::new();
    let mut tree = |surface: &WlSurface, at: Point<i32, Logical>| {
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(renderer, surface, at.to_physical(1), scale, 1.0, Kind::Unspecified);
        out.extend(elements.into_iter().map(OrreryElement::Surface));
    };
    for &layer in layers {
        for surface in map.layers_on(layer).rev() {
            if fullscreen && !over_fullscreen(layer, surface.cached_state().keyboard_interactivity) {
                continue;
            }
            let Some(at) = layer_origin(&map, surface) else {
                continue;
            };
            if kinds.contains(WindowSurfaceType::POPUP) {
                for (popup, offset) in PopupManager::popups_for_surface(surface.wl_surface()) {
                    tree(popup.wl_surface(), at + offset - popup.geometry().loc);
                }
            }
            if kinds.contains(WindowSurfaceType::TOPLEVEL) {
                tree(surface.wl_surface(), at);
            }
        }
    }
    out
}

/// What the monitor named after `output` shows (scale 1, output-local coordinates), front to back: the pointer,
/// the red bar, overlay and top layer surfaces, popups of bottom and background ones (menus of a bar there must
/// show over the windows), decorations, windows, bottom and background layer surfaces. With a fullscreen window:
/// the pointer, the red bar, the layer surfaces drawn over it (`over_fullscreen`), it, and black.
pub fn output_elements(
    output: &str,
    state: &Orrery,
    renderer: &mut GlesRenderer,
    decorations: &mut Decorations,
) -> Vec<OrreryElement> {
    let screen = state.output_named(output);
    let geometry = screen.as_ref().and_then(|o| state.space.output_geometry(o));
    let screen_rect = geometry.map_or_else(Rect::default, |g| Rect {
        x: g.loc.x,
        y: g.loc.y,
        w: g.size.w,
        h: g.size.h,
    });
    let scene = scene(
        &state.stacks,
        output,
        screen_rect,
        &state.config.colors,
        state.held.show_tabs.is_some(),
        state.config_error.is_some(),
    );
    let scale = Scale::from(1.0);
    let mut elements = cursor_elements(output, state, renderer, decorations);
    elements.extend(solid_elements(&mut decorations.bar, scene.bar.as_slice()));
    let fullscreen = scene.backdrop.is_some();
    if let Some(screen) = &screen {
        elements.extend(layer_elements(
            screen,
            &ABOVE,
            WindowSurfaceType::ALL,
            fullscreen,
            renderer,
        ));
        elements.extend(layer_elements(
            screen,
            &BELOW,
            WindowSurfaceType::POPUP,
            fullscreen,
            renderer,
        ));
    }
    elements.extend(solid_elements(&mut decorations.buffers, &scene.solids));

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
    elements.extend(solid_elements(&mut decorations.backdrop, scene.backdrop.as_slice()));
    if let Some(screen) = &screen {
        let own = WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE;
        elements.extend(layer_elements(screen, &BELOW, own, fullscreen, renderer));
    }
    elements
}

/// What shows where nothing is drawn.
#[must_use]
pub fn clear_color(state: &Orrery) -> Color32F {
    color32f(state.config.colors.background)
}

/// Draws the monitor named after `output` (`output_elements`) into `framebuffer`.
pub fn render_output<'d>(
    output: &str,
    state: &Orrery,
    renderer: &mut GlesRenderer,
    framebuffer: &mut GlesTarget<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    decorations: &mut Decorations,
    age: usize,
) -> anyhow::Result<RenderOutputResult<'d>> {
    let elements = output_elements(output, state, renderer, decorations);
    damage_tracker
        .render_output(renderer, framebuffer, age, &elements, clear_color(state))
        .context("rendering the output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use smithay::backend::renderer::element::Element as _;

    #[test]
    fn the_fallback_cursor_points_at_its_corner_and_keeps_its_ids_as_it_moves() {
        let [fill, outline] = fallback_cursor((5, 7));
        assert_eq!(
            outline,
            Solid {
                rect: Rect {
                    x: 5,
                    y: 7,
                    w: CURSOR,
                    h: CURSOR
                },
                color: CURSOR_DARK,
            }
        );
        assert_eq!(
            fill.rect,
            Rect {
                x: 6,
                y: 8,
                w: CURSOR - 2,
                h: CURSOR - 2
            }
        );
        assert_eq!(fill.color, CURSOR_LIGHT);
        // Moved, its elements keep their ids (so only its old and new places are damage) and follow it.
        let mut buffers = Vec::new();
        let before = solid_elements(&mut buffers, &fallback_cursor((5, 7)));
        let after = solid_elements(&mut buffers, &fallback_cursor((300, 200)));
        let ids = |e: &[OrreryElement]| e.iter().map(|e| e.id().clone()).collect::<Vec<_>>();
        assert_eq!(ids(&before), ids(&after));
        let scale = Scale::from(1.0);
        assert_eq!(
            after[1].geometry(scale),
            physical(Rect {
                x: 300,
                y: 200,
                w: CURSOR,
                h: CURSOR
            })
        );
    }

    #[test]
    fn a_cursor_is_drawn_only_where_orrery_draws_it_and_never_one_a_client_hid() {
        let named = CursorImageStatus::Named(CursorIcon::Text);
        assert_eq!(
            cursor_draw(false, &named),
            CursorDraw::None,
            "the host's cursor stands for local input"
        );
        assert_eq!(cursor_draw(true, &named), CursorDraw::Named(CursorIcon::Text));
        assert_eq!(cursor_draw(true, &CursorImageStatus::Hidden), CursorDraw::None);
    }

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

    #[test]
    fn a_fullscreen_window_is_drawn_alone_on_black_over_its_whole_screen() {
        let (mut s, _, colors) = wall();
        // A bar takes the top 30 px: the layout divides the rest, a fullscreen window covers it too.
        let screen = Rect { x: 500, ..AREA };
        s.set_area(
            "m",
            Rect {
                x: 500,
                y: 30,
                h: 870,
                ..AREA
            },
        );
        s.set_screen("m", screen);
        s.open_stacked(1);
        s.open_stacked(2);
        s.focus_slot(crate::layout::Dir::Right);
        s.open_tab(3, None, None);
        s.open_tab(4, None, None);
        let tiled = scene(&s, "m", screen, &colors, true, false);
        assert_eq!(tiled.windows.len(), 2);
        assert!(tiled.backdrop.is_none() && !tiled.solids.is_empty());

        assert!(s.toggle_fullscreen());
        let whole = Rect { x: 0, y: 0, ..AREA };
        let full = scene(&s, "m", screen, &colors, true, true);
        assert_eq!(
            full.windows,
            [(4, whole)],
            "alone, at the screen's origin, the bar's strip too"
        );
        assert_eq!(
            full.backdrop,
            Some(Solid {
                rect: whole,
                color: BACKDROP
            })
        );
        assert!(full.solids.is_empty(), "no border, badge or tab strip around it");
        assert_eq!(
            full.bar.map(|b| b.color),
            Some(ERROR_RED),
            "a broken config still shows"
        );

        // Another workspace on that screen is drawn as ever.
        s.workspace(5);
        let other = scene(&s, "m", screen, &colors, false, false);
        assert!(other.backdrop.is_none() && other.windows.is_empty());
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
        let scene = scene(&s, "m", AREA, &colors, true, false);
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
            super::scene(&s, "m", AREA, &colors, false, false),
            "no strips without tabs"
        );
        assert_eq!(super::scene(&s, "nope", AREA, &colors, true, false), Scene::default());
    }

    #[test]
    fn windows_are_the_tops_clipped_to_their_slots() {
        let (mut s, rects, colors) = wall();
        for id in [1, 2, 3] {
            s.open_stacked(id);
        }
        s.focus_slot(crate::layout::Dir::Right);
        s.open_stacked(4);
        let scene = scene(&s, "m", AREA, &colors, false, false);
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
    fn only_the_shown_workspace_is_drawn() {
        let (mut s, rects, colors) = wall();
        s.open_stacked(1);
        s.workspace(2);
        s.focus_slot(crate::layout::Dir::Right);
        s.open_stacked(2);
        let scene = scene(&s, "m", AREA, &colors, false, false);
        assert_eq!(scene.windows, [(2, rects[3])]);
        assert!(of_color(&scene, colors.focus).iter().all(|e| inside(*e, rects[3])));
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
        let scene = scene(&s, "m", AREA, &colors, true, false);
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

    fn dim(c: Color) -> Color {
        Color {
            a: (u16::from(c.a) * 2 / 5) as u8,
            ..c
        }
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
        let scene = scene(&s, "m", AREA, &colors, true, false);
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
        let first_pip = scene
            .solids
            .iter()
            .position(|s| s.color == colors.background)
            .expect("pip");
        let segment = scene.solids.iter().position(|s| s.rect.h == STRIP).expect("segment");
        assert!(first_pip < segment, "pips in front of their segment");

        let hidden = super::scene(&s, "m", AREA, &colors, false, false);
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
        let plain = scene(&s, "m", AREA, &colors, false, false);
        assert_eq!(of_color(&plain, colors.attention), [badge]);
        assert!(of_color(&plain, colors.badge).is_empty());

        let held = scene(&s, "m", AREA, &colors, true, false);
        let waiting = Color {
            a: (u16::from(colors.attention.a) * 7 / 10) as u8,
            ..colors.attention
        };
        let colours: Vec<Color> = segments(&held).iter().map(|s| s.color).collect();
        assert_eq!(
            colours,
            [colors.attention, dim(colors.badge), waiting],
            "bright or dim by current, attention by mark; a marked background tab dims less"
        );
        assert_eq!(of_color(&held, colors.background).len(), 2, "the current tab's depth");

        // Shown windows lose their marks, and the colours go with them.
        s.cycle_tab(crate::config::Cycle::Prev);
        s.cycle_tab(crate::config::Cycle::Next);
        s.close(2);
        let after = scene(&s, "m", AREA, &colors, true, false);
        assert!(of_color(&after, colors.attention).is_empty());
        assert!(of_color(&after, waiting).is_empty());
    }

    #[test]
    fn a_marked_window_alone_in_its_slot_still_gets_the_badge() {
        let (mut s, rects, colors) = wall();
        s.open_stacked(1);
        s.focus_slot(crate::layout::Dir::Right);
        s.open_stacked(2);
        s.mark(1);
        let r = rects[0];
        let badge = Rect {
            x: r.x + r.w - 14,
            y: r.y + 6,
            w: 8,
            h: 8,
        };
        let held = scene(&s, "m", AREA, &colors, true, false);
        assert_eq!(
            of_color(&held, colors.attention),
            [badge],
            "a badge, but no strip for one window"
        );
        assert!(of_color(&held, colors.badge).is_empty());
    }

    #[test]
    fn a_config_error_puts_a_red_bar_over_everything() {
        let (mut s, _, colors) = wall();
        s.open_stacked(1);
        s.open_stacked(2);
        let plain = scene(&s, "m", AREA, &colors, true, false);
        let flagged = scene(&s, "m", AREA, &colors, true, true);
        assert_eq!(
            flagged.bar,
            Some(Solid {
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: AREA.w,
                    h: ERROR_BAR
                },
                color: ERROR_RED
            }),
            "the output's whole width"
        );
        assert_eq!(plain.bar, None);
        assert_eq!(flagged.solids, plain.solids, "everything else unchanged");
        assert_eq!(flagged.windows, plain.windows);
        assert_eq!(
            scene(&s, "nope", AREA, &colors, false, true),
            Scene::default(),
            "no monitor, no area"
        );
    }

    #[test]
    fn slots_shrunk_by_layer_zones_stay_where_they_are_on_the_screen_and_the_bar_tops_the_screen() {
        let (mut s, _, colors) = wall();
        s.open_stacked(1);
        let usable = Rect {
            y: 30,
            h: AREA.h - 30,
            ..AREA
        };
        s.set_area("m", usable);
        let scene = scene(&s, "m", AREA, &colors, false, true);
        let first = wall().0.shown(0).unwrap().layout().rects(usable)[0];
        assert_eq!(scene.windows, [(1, first)], "screen-local, which here is global");
        assert_eq!(scene.windows[0].1.y, 30);
        assert!(of_color(&scene, colors.focus).iter().all(|e| inside(*e, first)));
        assert_eq!(scene.bar.map(|b| (b.rect.y, b.rect.w)), Some((0, AREA.w)));
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
        let scene = scene(&s, "right", area, &c.colors, false, false);
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
