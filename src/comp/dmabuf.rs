//! linux-dmabuf (`zwp_linux_dmabuf_v1`): clients hand orrery buffers that live on the GPU, so OpenGL and Vulkan
//! clients render on the GPU instead of in software, and orrery draws their buffers without copying them.
//!
//! A backend that draws with a GPU (bare metal and nested) advertises the global at init with
//! `Orrery::advertise_dmabuf`: the formats its renderer can import, the GPU's render node as the main device of the
//! default feedback (version 4; without a known node, a version 3 global with the formats only), and an importer
//! over its renderer. Headless has no renderer and advertises nothing, so its clients use shared memory.
//! Every buffer a client creates is imported into the renderer once (`DmabufHandler::dmabuf_imported`): a buffer
//! the GPU cannot use is refused there, at creation, rather than drawn wrong later. Drawing needs nothing more: the
//! renderer's surface import (`on_commit_buffer_handler`, then the render elements) handles dmabuf buffers like
//! shared memory ones.
//!
//! Limits: one GPU; client buffers are composited, never scanned out directly; implicit sync only.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer, Format};
use smithay::backend::drm::DrmNode;
use smithay::delegate_dmabuf;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};

use super::Orrery;
use crate::ipc::DmabufView;

/// A backend's way to try a client's buffer in its renderer, handed over with `Orrery::advertise_dmabuf`.
pub trait DmabufImport {
    /// Imports `dmabuf` into the renderer; the error says why it cannot be used.
    fn import(&mut self, dmabuf: &Dmabuf) -> Result<(), String>;
}

/// What to advertise for a renderer that imports `formats` formats, the GPU's render node known or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advertise {
    /// Nothing: the renderer imports no dmabufs (clients keep to shared memory).
    Nothing,
    /// A version 3 global with the formats only (no main device to name).
    Formats,
    /// A version 4 global whose default feedback names the render node.
    Feedback,
}

pub fn advertise(formats: usize, render_node: bool) -> Advertise {
    match (formats, render_node) {
        (0, _) => Advertise::Nothing,
        (_, true) => Advertise::Feedback,
        (_, false) => Advertise::Formats,
    }
}

/// The global orrery advertises, and what it was made from.
pub struct Advertised {
    _global: DmabufGlobal,
    importer: Box<dyn DmabufImport>,
    formats: usize,
    /// The render node named by the default feedback (`None`: a version 3 global).
    device: Option<String>,
}

impl Orrery {
    /// Advertises linux-dmabuf for a renderer importing `formats`, with feedback naming `render_node` when known;
    /// `importer` tries each client buffer in that renderer. Called once, by a backend that draws with a GPU.
    pub fn advertise_dmabuf(
        &mut self,
        formats: Vec<Format>,
        render_node: Option<DrmNode>,
        importer: Box<dyn DmabufImport>,
    ) {
        let count = formats.len();
        let (global, device) = match advertise(count, render_node.is_some()) {
            Advertise::Nothing => {
                tracing::info!("dmabuf: the renderer imports no client buffers; OpenGL clients render in software");
                return;
            }
            Advertise::Feedback => {
                let node = render_node.expect("feedback needs a render node");
                let device = node.dev_path().map(|p| p.display().to_string());
                match DmabufFeedbackBuilder::new(node.dev_id(), formats.iter().copied()).build() {
                    Ok(feedback) => (
                        self.dmabuf_state
                            .create_global_with_default_feedback::<Orrery>(&self.display_handle, &feedback),
                        device,
                    ),
                    Err(e) => {
                        tracing::warn!("dmabuf: cannot build the feedback ({e}); advertising the formats only");
                        let global = self.dmabuf_state.create_global::<Orrery>(&self.display_handle, formats);
                        (global, None)
                    }
                }
            }
            Advertise::Formats => (
                self.dmabuf_state.create_global::<Orrery>(&self.display_handle, formats),
                None,
            ),
        };
        match &device {
            Some(path) => tracing::info!("dmabuf: {count} formats, main device {path}"),
            None => tracing::info!("dmabuf: {count} formats, no main device (version 3)"),
        }
        self.dmabuf = Some(Advertised {
            _global: global,
            importer,
            formats: count,
            device,
        });
    }

    /// The `state` answer's `dmabuf`: absent when nothing is advertised.
    pub fn dmabuf_view(&self) -> Option<DmabufView> {
        (self.dmabuf.as_ref()).map(|a| DmabufView {
            formats: a.formats,
            device: a.device.clone(),
        })
    }
}

impl DmabufHandler for Orrery {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let result = match self.dmabuf.as_mut() {
            Some(advertised) => advertised.importer.import(&dmabuf),
            None => Err("nothing advertised".to_string()),
        };
        match result {
            Ok(()) => {
                // Only fails when the client is already gone.
                let _ = notifier.successful::<Orrery>();
            }
            Err(e) => {
                tracing::debug!(
                    "dmabuf: refused a {}x{} {:?} buffer: {e}",
                    dmabuf.width(),
                    dmabuf.height(),
                    dmabuf.format().code
                );
                notifier.failed();
            }
        }
    }
}

delegate_dmabuf!(Orrery);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_without_formats() {
        assert_eq!(advertise(0, true), Advertise::Nothing);
        assert_eq!(advertise(0, false), Advertise::Nothing);
    }

    #[test]
    fn feedback_names_the_render_node_when_there_is_one() {
        assert_eq!(advertise(12, true), Advertise::Feedback);
    }

    #[test]
    fn formats_only_without_a_render_node() {
        assert_eq!(advertise(12, false), Advertise::Formats);
    }
}
