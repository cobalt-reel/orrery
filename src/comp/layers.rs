//! Layer-shell (`zwlr_layer_shell_v1`): bars, docks, wallpapers, drop-downs and launchers on a screen's four layers,
//! two under the slots (background, bottom) and two over them (top, overlay). Each screen's layer surfaces live in
//! Smithay's `LayerMap` for its output, which places them as the protocol says; what their exclusive zones leave is
//! the area the screen's layout divides (`Stacks::set_area`). A layer surface is in its map from its first commit
//! (so the initial configure carries its size) until it unmaps (a null buffer) or goes; it is drawn, hit and given
//! the keyboard only while mapped (it has a buffer).
//!
//! The keyboard: an `exclusive` surface on the top or overlay layer takes it while mapped (the most recently mapped
//! wins) and gives it back to the focused window when it goes; while it has it, binds do not run, except quit,
//! reclaim and vt (`runs_over_exclusive`). An `on_demand` surface takes it on a click and loses it when a window
//! (or slot) is focused; `exclusive` on the bottom or background layer does the same, as the protocol allows. `none`
//! never takes it. Layer surfaces never enter the stacks.

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive as _, Logical, Point, Rectangle};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
};

use super::Orrery;
use crate::config::Action;
use crate::ipc::{LayerName, LayerView};
use crate::layout::Rect;
use crate::stacks::WinId;

/// The layers over the slots, front to back.
pub const ABOVE: [Layer; 2] = [Layer::Overlay, Layer::Top];
/// The layers under the slots, front to back.
pub const BELOW: [Layer; 2] = [Layer::Bottom, Layer::Background];

/// A layer surface orrery knows.
#[derive(Debug)]
pub struct Layered {
    pub surface: LayerSurface,
    /// The output (screen) it lives on, by name.
    pub output: String,
    /// It has a buffer: drawn, hit, and able to take the keyboard.
    pub mapped: bool,
}

/// Where the stacks' focus is (monitor, its shown workspace, that workspace's focused slot, the focused window): a
/// click gives an `on_demand` layer surface the keyboard until this moves.
pub type FocusKey = (usize, Option<u8>, Option<usize>, Option<WinId>);

/// The area a screen's layout divides: the screen less the layer surfaces' exclusive zones (`zone`, output-local, as
/// `LayerMap::non_exclusive_zone` gives it). Zones that leave nothing are ignored: the whole screen.
#[must_use]
pub fn usable_area(screen: Rect, zone: Rectangle<i32, Logical>) -> Rect {
    if zone.size.w <= 0 || zone.size.h <= 0 {
        return screen;
    }
    Rect {
        x: screen.x + zone.loc.x,
        y: screen.y + zone.loc.y,
        w: zone.size.w,
        h: zone.size.h,
    }
}

/// The binds that still run while an `exclusive` layer surface has the keyboard: quitting, the escape key and the
/// consoles, so a client grabbing the keyboard can never lock anyone out.
#[must_use]
pub fn runs_over_exclusive(action: &Action) -> bool {
    matches!(action, Action::Quit | Action::Reclaim | Action::Vt(_))
}

#[must_use]
pub fn layer_name(layer: Layer) -> LayerName {
    match layer {
        Layer::Background => LayerName::Background,
        Layer::Bottom => LayerName::Bottom,
        Layer::Top => LayerName::Top,
        Layer::Overlay => LayerName::Overlay,
    }
}

/// Where a mapped layer surface's own surface starts, output-local (its bounding box may reach further up-left, by
/// its subsurfaces).
pub fn layer_origin(map: &smithay::desktop::LayerMap, layer: &LayerSurface) -> Option<Point<i32, Logical>> {
    Some(map.layer_geometry(layer)?.loc - layer.bbox().loc)
}

fn rect_of(r: Rectangle<i32, Logical>) -> Rect {
    Rect {
        x: r.loc.x,
        y: r.loc.y,
        w: r.size.w,
        h: r.size.h,
    }
}

impl Orrery {
    /// A new layer surface: on the screen it asks for (its wl_output), else the focused one. One asking for a screen
    /// that is gone, or coming when there is none, is closed at once. It is placed on its first commit.
    pub fn new_layer(&mut self, surface: WlrLayerSurface, output: Option<WlOutput>, namespace: String) {
        let screen = match output {
            Some(wl) => Output::from_resource(&wl).map(|o| o.name()),
            None => (self.stacks.monitors().get(self.stacks.focused_monitor()))
                .map(|m| m.name().to_owned())
                .or_else(|| self.outputs.first().map(Output::name)),
        };
        let Some(output) = screen.filter(|name| self.output_named(name).is_some()) else {
            surface.send_close();
            return;
        };
        self.layers.push(Layered {
            surface: LayerSurface::new(surface, namespace),
            output,
            mapped: false,
        });
    }

    pub(super) fn output_named(&self, name: &str) -> Option<Output> {
        self.outputs.iter().find(|o| o.name() == name).cloned()
    }

    /// A layer surface's own surface committed. The first commit (or the first after an unmap) puts it in its
    /// screen's map and is answered with a configure carrying its size; a buffer maps it (the most recent one last,
    /// for the keyboard); a null buffer unmaps it: out of the map, and back to waiting for a first commit. Every
    /// commit arranges the screen's layer surfaces again (size, anchors, margins, zone), and a changed usable area or
    /// mapping arranges the windows.
    pub fn layer_committed(&mut self, surface: &WlSurface) {
        let Some(i) = self.layers.iter().position(|l| l.surface.wl_surface() == surface) else {
            return;
        };
        let Some(output) = self.output_named(&self.layers[i].output) else {
            return;
        };
        let has_buffer = with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);
        let layer = self.layers[i].surface.clone();
        let was_mapped = self.layers[i].mapped;
        {
            let mut map = layer_map_for_output(&output);
            if was_mapped && !has_buffer {
                map.unmap_layer(&layer);
                // The client starts over (a commit without a buffer, then the configure): answered as a first one.
                with_states(surface, |states| {
                    if let Some(data) = states.data_map.get::<LayerSurfaceData>() {
                        data.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .initial_configure_sent = false;
                    }
                });
            } else {
                // Placed now if new (`layers_arranged` below arranges it again with the rest otherwise).
                if map.layer_geometry(&layer).is_none()
                    && let Err(e) = map.map_layer(&layer)
                {
                    tracing::warn!("cannot place a layer surface: {e}");
                }
                let configured = with_states(surface, |states| {
                    (states.data_map.get::<LayerSurfaceData>()).is_some_and(|d| {
                        d.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .initial_configure_sent
                    })
                });
                if !configured {
                    layer.layer_surface().send_configure();
                }
            }
        }
        if has_buffer != was_mapped {
            let mut record = self.layers.remove(i);
            record.mapped = has_buffer;
            self.layers.push(record);
        }
        if self.layers_arranged(&output) || has_buffer != was_mapped {
            self.arrange();
        } else {
            self.update_keyboard_focus();
        }
    }

    /// A layer surface was destroyed (or its client went): out of its map, its zone given back.
    pub fn forget_layer(&mut self, surface: &WlSurface) {
        let Some(i) = self.layers.iter().position(|l| l.surface.wl_surface() == surface) else {
            return;
        };
        let record = self.layers.remove(i);
        if let Some(output) = self.output_named(&record.output) {
            layer_map_for_output(&output).unmap_layer(&record.surface);
            self.layers_arranged(&output);
        }
        self.arrange();
    }

    /// Forgets layer surfaces whose client went without destroying them; true when there were any (the caller
    /// arranges).
    pub(super) fn forget_dead_layers(&mut self) -> bool {
        if self.layers.iter().all(|l| l.surface.alive()) {
            return false;
        }
        self.layers.retain(|l| l.surface.alive());
        for output in self.outputs.clone() {
            layer_map_for_output(&output).cleanup();
            self.layers_arranged(&output);
        }
        true
    }

    /// A screen going away: its layer surfaces are told (`closed`) and forgotten; the client may make new ones
    /// elsewhere.
    pub(super) fn close_layers(&mut self, output: &Output) {
        let name = output.name();
        {
            let mut map = layer_map_for_output(output);
            for l in self.layers.iter().filter(|l| l.output == name) {
                l.surface.layer_surface().send_close();
                map.unmap_layer(&l.surface);
            }
        }
        self.layers.retain(|l| l.output != name);
    }

    /// The area `output`'s layout divides when the screen is `screen` (global): its layer surfaces arranged again
    /// (Smithay's `LayerMap::arrange`, which also reconfigures them to new sizes), less their exclusive zones.
    pub(super) fn usable_area_of(&self, output: &Output, screen: Rect) -> Rect {
        let mut map = layer_map_for_output(output);
        map.arrange();
        usable_area(screen, map.non_exclusive_zone())
    }

    /// Arranges `output`'s layer surfaces again and gives its monitor what their zones leave; true when that changed
    /// (the caller arranges the windows).
    pub(super) fn layers_arranged(&mut self, output: &Output) -> bool {
        let Some(screen) = self.space.output_geometry(output).map(rect_of) else {
            return false;
        };
        let area = self.usable_area_of(output, screen);
        let name = output.name();
        let old = (self.stacks.monitors().iter())
            .find(|m| m.name() == name)
            .map(|m| m.area());
        if old.is_none_or(|old| old == area) {
            return false;
        }
        self.stacks.set_area(&name, area);
        true
    }

    /// Where the stacks' focus is now (see `FocusKey`).
    pub(super) fn focus_key(&self) -> FocusKey {
        let mi = self.stacks.focused_monitor();
        let ws = self.stacks.shown(mi);
        (
            mi,
            ws.map(crate::stacks::Workspace::number),
            ws.map(crate::stacks::Workspace::focus),
            self.stacks.focused_window(),
        )
    }

    /// A click on layer surface `layer`: the keyboard is its until the stacks' focus moves, if it takes it on demand
    /// (`exclusive` ones over the slots have it already).
    pub(super) fn layer_clicked(&mut self, layer: &LayerSurface) {
        if layer.can_receive_keyboard_focus() {
            self.layer_focus = Some((layer.wl_surface().clone(), self.focus_key()));
        }
    }

    /// The layer surface that should have the keyboard, and whether it holds it exclusively: the most recently mapped
    /// `exclusive` one over the slots, else the one a click gave it while the stacks' focus has not moved since and
    /// it still takes the keyboard (forgotten otherwise).
    pub(super) fn layer_keyboard(&mut self) -> Option<(WlSurface, bool)> {
        let exclusive = (self.layers.iter().rev().filter(|l| l.mapped)).find(|l| {
            let state = l.surface.cached_state();
            state.keyboard_interactivity == KeyboardInteractivity::Exclusive && ABOVE.contains(&state.layer)
        });
        if let Some(l) = exclusive {
            return Some((l.surface.wl_surface().clone(), true));
        }
        let key = self.focus_key();
        let (surface, at) = self.layer_focus.as_ref()?;
        let takes = (self.layers.iter())
            .any(|l| l.mapped && l.surface.wl_surface() == surface && l.surface.can_receive_keyboard_focus());
        if takes && *at == key {
            return Some((surface.clone(), false));
        }
        self.layer_focus = None;
        None
    }

    /// The layer surface on one of `layers` (front to back) under `pos` (global), on the screen there, with the
    /// surface hit (its own, a subsurface's or a popup's) and that surface's origin.
    pub(super) fn layer_under(
        &self,
        layers: &[Layer],
        pos: Point<f64, Logical>,
    ) -> Option<(LayerSurface, WlSurface, Point<f64, Logical>)> {
        let (output, geo) = self.outputs.iter().find_map(|o| {
            let geo = self.space.output_geometry(o)?;
            geo.to_f64().contains(pos).then_some((o, geo))
        })?;
        let map = layer_map_for_output(output);
        let local = pos - geo.loc.to_f64();
        for &layer in layers {
            for l in map.layers_on(layer).rev() {
                let Some(at) = layer_origin(&map, l) else {
                    continue;
                };
                if let Some((surface, loc)) = l.surface_under(local - at.to_f64(), WindowSurfaceType::ALL) {
                    return Some((l.clone(), surface, (loc + at + geo.loc).to_f64()));
                }
            }
        }
        None
    }

    /// The screen of the layer surface whose own surface is `root`, seen from that surface's origin: where its
    /// popups must stay.
    pub(super) fn layer_screen_from(&self, root: &WlSurface) -> Option<Rectangle<i32, Logical>> {
        let record = self.layers.iter().find(|l| l.surface.wl_surface() == root)?;
        let output = self.output_named(&record.output)?;
        let size = self.space.output_geometry(&output)?.size;
        let at = layer_origin(&layer_map_for_output(&output), &record.surface)?;
        Some(Rectangle::new((-at.x, -at.y).into(), size))
    }

    /// The mapped layer surfaces on `output`, back to front as drawn, for `state`.
    pub(super) fn layer_views(&self, output: &str) -> Vec<LayerView> {
        let Some(o) = self.output_named(output) else {
            return Vec::new();
        };
        let Some(geo) = self.space.output_geometry(&o) else {
            return Vec::new();
        };
        let keyboard = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let map = layer_map_for_output(&o);
        let mut views = Vec::new();
        for &layer in BELOW.iter().rev().chain(ABOVE.iter().rev()) {
            let on: Vec<&Layered> = (self.layers.iter())
                .filter(|l| l.mapped && l.output == output && l.surface.layer() == layer)
                .collect();
            for l in on {
                let Some(mut rect) = map.layer_geometry(&l.surface) else {
                    continue;
                };
                rect.loc += geo.loc;
                views.push(LayerView {
                    namespace: l.surface.namespace().to_owned(),
                    layer: layer_name(layer),
                    rect: rect_of(rect),
                    keyboard: keyboard.as_ref() == Some(l.surface.wl_surface()),
                });
            }
        }
        views
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Dir;

    const SCREEN: Rect = Rect {
        x: 1280,
        y: 0,
        w: 1920,
        h: 1080,
    };

    #[test]
    fn exclusive_zones_shrink_the_screen_in_global_coordinates() {
        let zone = Rectangle::new((0, 30).into(), (1920, 1050).into());
        assert_eq!(
            usable_area(SCREEN, zone),
            Rect {
                x: 1280,
                y: 30,
                w: 1920,
                h: 1050
            }
        );
        assert_eq!(
            usable_area(SCREEN, Rectangle::from_size((1920, 1080).into())),
            SCREEN,
            "no zones"
        );
    }

    #[test]
    fn a_zone_leaving_nothing_is_ignored() {
        for (w, h) in [(0, 0), (1920, 0), (-5, 100)] {
            // Smithay's zone arithmetic can go negative, which its constructors refuse.
            let mut zone = Rectangle::from_size((0, 0).into());
            (zone.size.w, zone.size.h) = (w, h);
            assert_eq!(usable_area(SCREEN, zone), SCREEN, "{w}x{h}");
        }
    }

    #[test]
    fn only_quit_reclaim_and_vt_run_over_an_exclusive_surface() {
        assert!(runs_over_exclusive(&Action::Quit));
        assert!(runs_over_exclusive(&Action::Reclaim));
        assert!(runs_over_exclusive(&Action::Vt(3)));
        for other in [
            Action::FocusSlot(Dir::Left),
            Action::Close,
            Action::ShowTabs,
            Action::Workspace(2),
            Action::Spawn(vec!["x".into()]),
        ] {
            assert!(!runs_over_exclusive(&other), "{other:?}");
        }
    }
}
