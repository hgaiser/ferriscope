//! Zero-copy passthrough: deliver the client's own DMA-BUF instead of
//! compositing.
//!
//! ferriscope has no scanout device and no hardware planes. "Passthrough" means
//! handing the sink the client's buffer unchanged, which skips the composite
//! pass and the extra full-frame copy it costs on a GPU the game is saturating
//! (on AMD, the composite queues behind the game's work on the graphics ring).
//! It applies only when compositing would reproduce that buffer exactly: one
//! opaque window whose buffer is the output, with nothing drawn over it.
//!
//! The delivered slot is a *borrowed* client buffer (see
//! [`DmabufPool::borrowed`](crate::shell::render::pool::DmabufPool)): the
//! compositor keeps its `Buffer` clone until the consumer's release, so
//! smithay's release point and `wl_buffer.release` are not signalled earlier.
//! The commit was gated on its acquire point or implicit fences before it
//! became current (`shell/sync.rs`), so the buffer is complete and the frame
//! carries no acquire fence.

use smithay::{
	backend::{
		allocator::{Buffer as _, Fourcc, dmabuf::Dmabuf, format::has_alpha},
		renderer::utils::{Buffer, RendererSurfaceStateUserData, with_renderer_surface_state},
	},
	desktop::PopupManager,
	utils::{Logical, Rectangle, Size, Transform},
	wayland::{
		compositor::{TraversalAction, with_surface_tree_downward},
		dmabuf::get_dmabuf,
	},
};

use crate::{
	hdr::ContentKind,
	shell::render::elements::pick_hdr_format,
	shell::{FerriscopeState, hdr::surface_content_colour},
};

/// Why a frame cannot be delivered as the client's own buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum Blocker {
	/// The sink does not take DMA-BUFs.
	NoDmabufSink,
	/// The scene is not exactly one window.
	NotSingleWindow,
	/// The window has subsurfaces or popups drawn with it.
	ExtraSurfaces,
	/// The window is not a mapped surface covering the output at its origin.
	NotFullOutput,
	/// The buffer is not a DMA-BUF.
	NotDmabuf,
	/// The buffer's format is not one the consumer takes for the window's
	/// colour (8-bit RGB for SDR; 10-bit or FP16 only for matching HDR content).
	UnsupportedFormat,
	/// The buffer size differs from the output, or a scale, crop or transform
	/// would be applied.
	Transformed,
	/// The buffer has alpha the composite would blend over black: its format
	/// has an alpha channel and its opaque region does not cover it.
	NotOpaque,
	/// A cursor would be drawn.
	CursorVisible,
	/// A Steam overlay or notification would be drawn.
	OverlayVisible,
}

/// The facts that decide between passthrough and compositing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) struct Conditions {
	/// The sink takes DMA-BUFs.
	pub(in crate::shell) dmabuf_sink: bool,
	/// The space holds exactly one window.
	pub(in crate::shell) single_window: bool,
	/// The window's surface tree is one surface and it has no popups.
	pub(in crate::shell) lone_surface: bool,
	/// The window covers the output exactly, at its origin.
	pub(in crate::shell) full_output: bool,
	/// The current buffer is a DMA-BUF.
	pub(in crate::shell) buffer_is_dmabuf: bool,
	/// The DMA-BUF format is one the consumer takes for the window's content.
	pub(in crate::shell) format_supported: bool,
	/// The buffer is output-sized with scale 1, no crop and no transform.
	pub(in crate::shell) identity_transform: bool,
	/// The buffer is opaque over its whole area: its format has no alpha, or
	/// its opaque region covers it.
	pub(in crate::shell) opaque: bool,
	/// A cursor image would be composited.
	pub(in crate::shell) cursor_visible: bool,
	/// An overlay or notification would be composited.
	pub(in crate::shell) overlay_visible: bool,
}

/// Decide how to get the frame to the sink: `None` means passthrough, `Some`
/// names the first reason it cannot be used.
pub(in crate::shell) fn blocker(c: &Conditions) -> Option<Blocker> {
	if !c.dmabuf_sink {
		Some(Blocker::NoDmabufSink)
	} else if !c.single_window {
		Some(Blocker::NotSingleWindow)
	} else if !c.lone_surface {
		Some(Blocker::ExtraSurfaces)
	} else if !c.full_output {
		Some(Blocker::NotFullOutput)
	} else if !c.buffer_is_dmabuf {
		Some(Blocker::NotDmabuf)
	} else if !c.format_supported {
		Some(Blocker::UnsupportedFormat)
	} else if !c.identity_transform {
		Some(Blocker::Transformed)
	} else if !c.opaque {
		Some(Blocker::NotOpaque)
	} else if c.cursor_visible {
		Some(Blocker::CursorVisible)
	} else if c.overlay_visible {
		Some(Blocker::OverlayVisible)
	} else {
		None
	}
}

/// Whether the consumer takes `format` as `content` without conversion.
///
/// SDR content is 8-bit RGB. HDR content is passed through only with HDR
/// output enabled, and only in the layouts the consumer's encoder imports for
/// its encoding: 10-bit ABGR/XBGR for PQ and FP16 ABGR/XBGR for scRGB. The
/// frame then carries the content's colour metadata (`frame_info`).
pub(in crate::shell) fn format_supported(format: Fourcc, content: ContentKind, hdr_enabled: bool) -> bool {
	match content {
		ContentKind::Sdr => matches!(
			format,
			Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888
		),
		ContentKind::Pq => hdr_enabled && matches!(format, Fourcc::Abgr2101010 | Fourcc::Xbgr2101010),
		ContentKind::ScRgb => hdr_enabled && matches!(format, Fourcc::Abgr16161616f | Fourcc::Xbgr16161616f),
	}
}

/// Whether `regions` (disjoint, as smithay keeps them) cover all of `size`.
pub(in crate::shell) fn covers(regions: &[Rectangle<i32, Logical>], size: Size<i32, Logical>) -> bool {
	let area = |rect: &Rectangle<i32, Logical>| i64::from(rect.size.w) * i64::from(rect.size.h);
	let bounds = Rectangle::from_size(size);
	regions
		.iter()
		.filter_map(|region| region.intersection(bounds))
		.map(|region| area(&region))
		.sum::<i64>()
		>= area(&bounds)
}

/// A client buffer that can be delivered as the frame.
pub(in crate::shell) struct Candidate {
	/// Held until the consumer releases the frame.
	pub(in crate::shell) buffer: Buffer,
	/// The client's DMA-BUF.
	pub(in crate::shell) dmabuf: Dmabuf,
}

impl FerriscopeState {
	/// The client buffer to deliver as-is, or `None` when the frame must be
	/// composited.
	///
	/// Logs each switch between passthrough and compositing, with the reason.
	pub(in crate::shell) fn passthrough_candidate(&mut self) -> Option<Candidate> {
		let (conditions, candidate) = self.passthrough_conditions();
		let blocker = blocker(&conditions);
		if self.passthrough_blocker != Some(blocker) {
			self.passthrough_blocker = Some(blocker);
			match blocker {
				None => tracing::debug!(target: "ferriscope::passthrough", "passing the client buffer through"),
				Some(blocker) => tracing::debug!(
					target: "ferriscope::passthrough",
					?blocker,
					format = ?candidate.as_ref().map(|candidate| candidate.dmabuf.format()),
					"compositing"
				),
			}
		}
		match blocker {
			None => candidate,
			Some(_) => None,
		}
	}

	fn passthrough_conditions(&self) -> (Conditions, Option<Candidate>) {
		let mut conditions = Conditions {
			dmabuf_sink: self.use_dmabuf && self.sink.wants_dmabuf(),
			single_window: false,
			lone_surface: false,
			full_output: false,
			buffer_is_dmabuf: false,
			format_supported: false,
			identity_transform: false,
			opaque: false,
			cursor_visible: self.cursor_render_target().is_some(),
			overlay_visible: self.overlay_drawn(),
		};
		if !conditions.dmabuf_sink {
			return (conditions, None);
		}

		let mut windows = self.space.elements();
		let (Some(window), None) = (windows.next(), windows.next()) else {
			return (conditions, None);
		};
		conditions.single_window = true;
		let Some(surface) = self.window_content_surface(window) else {
			return (conditions, None);
		};

		let mut mapped_surfaces = 0usize;
		with_surface_tree_downward(
			&surface,
			(),
			|_, _, _| TraversalAction::DoChildren(()),
			|_, states, _| {
				if states
					.data_map
					.get::<RendererSurfaceStateUserData>()
					.is_some_and(|data| data.lock().unwrap().buffer().is_some())
				{
					mapped_surfaces += 1;
				}
			},
			|_, _, _| true,
		);
		conditions.lone_surface = mapped_surfaces == 1 && PopupManager::popups_for_surface(&surface).next().is_none();

		let output = self.output_rect();
		let at_origin = self
			.space
			.element_location(window)
			.is_some_and(|location| location - window.geometry().loc == (0, 0).into());
		conditions.full_output =
			at_origin && self.space.element_bbox(window) == Some(output) && self.output_fill().is_none();

		let state = with_renderer_surface_state(&surface, |state| {
			let buffer = state.buffer()?.clone();
			let view = state.view()?;
			let identity = state.buffer_scale() == 1
				&& state.buffer_transform() == Transform::Normal
				&& view.dst == output.size
				&& view.src == Rectangle::from_size(output.size.to_f64());
			let opaque = state
				.opaque_regions()
				.is_some_and(|regions| covers(regions, output.size));
			Some((buffer, identity, opaque))
		})
		.flatten();
		let Some((buffer, identity, opaque)) = state else {
			return (conditions, None);
		};
		let Ok(dmabuf) = get_dmabuf(&buffer).cloned() else {
			return (conditions, None);
		};
		conditions.buffer_is_dmabuf = true;
		// The renderer samples a format without alpha as opaque.
		conditions.opaque = opaque || !has_alpha(dmabuf.format().code);
		// The frame is labelled with the encoding `frame_info` settles on, which
		// is HDR only where the renderer can also draw it.
		let content = surface_content_colour(&surface).kind;
		let hdr_output = self.hdr && (!content.is_hdr() || pick_hdr_format(&self.renderer, content).is_some());
		conditions.format_supported = format_supported(dmabuf.format().code, content, hdr_output);
		conditions.identity_transform =
			identity && dmabuf.width() == self.frame_size.w as u32 && dmabuf.height() == self.frame_size.h as u32;
		(conditions, Some(Candidate { buffer, dmabuf }))
	}

	/// Whether any overlay or notification would put pixels in the frame: it is
	/// not hidden by opacity 0 and has a buffer.
	fn overlay_drawn(&self) -> bool {
		self.overlays.iter().any(|overlay| {
			self.overlay_opacity(overlay) != crate::focus::TRANSLUCENT
				&& self
					.window_content_surface(&overlay.window)
					.and_then(|surface| with_renderer_surface_state(&surface, |state| state.buffer().is_some()))
					.unwrap_or(false)
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn eligible() -> Conditions {
		Conditions {
			dmabuf_sink: true,
			single_window: true,
			lone_surface: true,
			full_output: true,
			buffer_is_dmabuf: true,
			format_supported: true,
			identity_transform: true,
			opaque: true,
			cursor_visible: false,
			overlay_visible: false,
		}
	}

	#[test]
	fn one_opaque_fullscreen_dmabuf_is_passthrough() {
		assert_eq!(blocker(&eligible()), None);
	}

	#[test]
	fn each_condition_blocks() {
		let cases: [(Conditions, Blocker); 10] = [
			(
				Conditions {
					dmabuf_sink: false,
					..eligible()
				},
				Blocker::NoDmabufSink,
			),
			(
				Conditions {
					single_window: false,
					..eligible()
				},
				Blocker::NotSingleWindow,
			),
			(
				Conditions {
					lone_surface: false,
					..eligible()
				},
				Blocker::ExtraSurfaces,
			),
			(
				Conditions {
					full_output: false,
					..eligible()
				},
				Blocker::NotFullOutput,
			),
			(
				Conditions {
					buffer_is_dmabuf: false,
					..eligible()
				},
				Blocker::NotDmabuf,
			),
			(
				Conditions {
					format_supported: false,
					..eligible()
				},
				Blocker::UnsupportedFormat,
			),
			(
				Conditions {
					identity_transform: false,
					..eligible()
				},
				Blocker::Transformed,
			),
			(
				Conditions {
					opaque: false,
					..eligible()
				},
				Blocker::NotOpaque,
			),
			(
				Conditions {
					cursor_visible: true,
					..eligible()
				},
				Blocker::CursorVisible,
			),
			(
				Conditions {
					overlay_visible: true,
					..eligible()
				},
				Blocker::OverlayVisible,
			),
		];
		for (conditions, expected) in cases {
			assert_eq!(blocker(&conditions), Some(expected));
		}
	}

	#[test]
	fn sdr_content_passes_only_eight_bit_rgb() {
		assert!(format_supported(Fourcc::Xrgb8888, ContentKind::Sdr, false));
		assert!(format_supported(Fourcc::Abgr8888, ContentKind::Sdr, true));
		assert!(!format_supported(Fourcc::Abgr2101010, ContentKind::Sdr, true));
		assert!(!format_supported(Fourcc::Abgr16161616f, ContentKind::Sdr, true));
	}

	#[test]
	fn hdr_content_passes_in_the_encoders_layout_with_hdr_enabled() {
		assert!(format_supported(Fourcc::Abgr2101010, ContentKind::Pq, true));
		assert!(format_supported(Fourcc::Xbgr2101010, ContentKind::Pq, true));
		assert!(format_supported(Fourcc::Abgr16161616f, ContentKind::ScRgb, true));
		assert!(format_supported(Fourcc::Xbgr16161616f, ContentKind::ScRgb, true));
		// Not without HDR output.
		assert!(!format_supported(Fourcc::Abgr2101010, ContentKind::Pq, false));
		// Not in a layout the encoder reads as something else.
		assert!(!format_supported(Fourcc::Argb2101010, ContentKind::Pq, true));
		assert!(!format_supported(Fourcc::Abgr8888, ContentKind::Pq, true));
		// Not content that disagrees with the format.
		assert!(!format_supported(Fourcc::Abgr16161616f, ContentKind::Pq, true));
		assert!(!format_supported(Fourcc::Abgr2101010, ContentKind::ScRgb, true));
	}

	#[test]
	fn opaque_coverage_needs_the_whole_surface() {
		let size = Size::from((100, 50));
		assert!(covers(&[Rectangle::from_size(size)], size));
		assert!(covers(
			&[
				Rectangle::new((0, 0).into(), (100, 20).into()),
				Rectangle::new((0, 20).into(), (100, 30).into()),
			],
			size
		));
		assert!(!covers(&[Rectangle::new((0, 0).into(), (100, 49).into())], size));
		assert!(!covers(&[], size));
	}
}
