//! The colour of what the compositor presents.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::{
	colour_management::surface_colour,
	hdr::{ContentColour, ContentKind},
	shell::{
		FerriscopeState,
		render::{elements::pick_hdr_format, hdr_shader::Conversion},
	},
	swapchain,
};

/// The colour a surface's client declared: the WSI swapchain's report combined
/// with the colour management image description (see [`ContentColour::resolve`]).
pub(in crate::shell) fn surface_content_colour(surface: &WlSurface) -> ContentColour {
	ContentColour::resolve(swapchain::swapchain_feedback(surface).as_ref(), surface_colour(surface))
}

impl FerriscopeState {
	/// The colour of the base layer: the focused fullscreen window's content
	/// (gamescope's `HeldCommits[HELD_COMMIT_BASE]`, `steamcompmgr.cpp:10393`).
	/// SDR when nothing is presented.
	pub(in crate::shell) fn base_layer_colour(&self) -> ContentColour {
		self.focus
			.fullscreen()
			.and_then(|window| self.window_content_surface(window))
			.map(|surface| surface_content_colour(&surface))
			.unwrap_or_default()
	}

	/// The conversion SDR elements (cursor, overlays) go through to sit in the
	/// output encoding, or `None` while the output is SDR.
	pub(in crate::shell) fn sdr_on_hdr_conversion(&self) -> Option<Conversion> {
		let program = self.sdr_program.as_ref()?;
		Conversion::new(program, self.output_encoding, self.sdr_white_nits)
	}

	/// Tell Steam whether the base layer wants HDR
	/// (`GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK`, `steamcompmgr.cpp:10402`),
	/// on change only.
	pub(in crate::shell) fn publish_app_wants_hdr(&mut self, wants: bool) {
		if wants == self.app_wants_hdr {
			return;
		}
		self.app_wants_hdr = wants;
		if let Some(steam) = self.steam.as_ref() {
			steam.set_app_wants_hdr(wants);
		}
	}

	/// Make the pool hold `content`'s encoding: HDR10 PQ as `Abgr2101010`, scRGB
	/// as `Abgr16161616f`, anything else (or HDR with HDR disabled, or a
	/// renderer without the format) as the 8-bit SDR format. Returns the
	/// encoding in effect.
	///
	/// A switch drops every pool slot and the damage ages with it: a buffer of
	/// another format holds nothing a partial redraw could build on. Slots the
	/// consumer still holds keep their buffers alive through the frames it
	/// has, and are reallocated when they come back.
	pub(in crate::shell) fn select_output_encoding(&mut self, content: ContentKind) -> ContentKind {
		let wanted = if self.hdr && self.use_dmabuf {
			content
		} else {
			ContentKind::Sdr
		};
		if wanted == self.output_encoding {
			return wanted;
		}
		let format = match wanted {
			ContentKind::Sdr => Some(self.sdr_format),
			hdr => pick_hdr_format(&self.renderer, hdr),
		};
		let Some(format) = format else {
			if !self.hdr_format_warned {
				self.hdr_format_warned = true;
				tracing::warn!(encoding = ?wanted, "the renderer cannot draw this HDR format; staying SDR");
			}
			return self.output_encoding;
		};
		tracing::info!(from = ?self.output_encoding, to = ?wanted, format = ?format.0, "output encoding changed");
		self.render_format = format;
		self.output_encoding = wanted;
		self.reset_pool_buffers();
		wanted
	}

	/// Log what a committed window surface declares, for diagnosing HDR
	/// content.
	pub(in crate::shell) fn trace_content_colour(&self, root: &WlSurface) {
		if !tracing::enabled!(tracing::Level::DEBUG) {
			return;
		}
		let colour = surface_content_colour(root);
		if colour.kind.is_hdr() {
			tracing::debug!(surface = ?root, colour = ?colour.frame_metadata(), "surface declares HDR content");
		}
	}
}
