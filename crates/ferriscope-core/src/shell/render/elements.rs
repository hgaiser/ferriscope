//! Render-element types and the offscreen render call.

use std::collections::HashSet;

use smithay::{
	backend::{
		allocator::{Fourcc, Modifier, dmabuf::Dmabuf},
		renderer::{
			Bind, Color32F,
			element::{
				Id, Wrap, render_elements,
				surface::WaylandSurfaceRenderElement,
				utils::{RelocateRenderElement, RescaleRenderElement},
			},
			gles::{GlesRenderer, GlesTarget},
			sync::SyncPoint,
		},
	},
	desktop::space::SpaceRenderElements,
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	wayland::compositor::{TraversalAction, with_surface_tree_downward},
};

use crate::{
	error::{Error, Result},
	hdr::ContentKind,
};

use crate::shell::render::{damage::FrameDamage, hdr_shader::SdrOnHdr};

/// Every `wl_surface` id in a window's surface tree (root plus subsurfaces).
///
/// Fills `ids` (cleared first) so the set's allocation is reused across frames.
/// Used to pick the role holder's render elements out of the composited space
/// for the output-fill transform; the ids match the ones
/// `render_elements_from_surface_tree` builds for each surface.
pub(in crate::shell) fn window_surface_ids(root: &WlSurface, ids: &mut HashSet<Id>) {
	ids.clear();
	with_surface_tree_downward(
		root,
		(),
		|_, _, _| TraversalAction::DoChildren(()),
		|surface, _, _| {
			ids.insert(Id::from_wayland_resource(surface));
		},
		|_, _, _| true,
	);
}

/// Pick a `(format, modifier)` pair the renderer can render into as a DMA-BUF.
pub(in crate::shell) fn pick_render_format(renderer: &GlesRenderer) -> Result<(Fourcc, Modifier)> {
	let set = <GlesRenderer as Bind<Dmabuf>>::supported_formats(renderer)
		.ok_or_else(|| Error::Gpu("renderer exposes no dmabuf render formats".into()))?;
	for code in [Fourcc::Abgr8888, Fourcc::Argb8888, Fourcc::Xrgb8888] {
		if let Some(format) = set.iter().find(|format| format.code == code) {
			return Ok((format.code, format.modifier));
		}
	}
	set.iter()
		.next()
		.map(|format| (format.code, format.modifier))
		.ok_or_else(|| Error::Gpu("dmabuf render format set is empty".into()))
}

/// The render target format for HDR output of `kind`, if the renderer can draw
/// into one: 10-bit for PQ, FP16 for scRGB (ABGR/XBGR 2101010 and 16161616F),
/// layouts a Vulkan video encoder can import.
pub(in crate::shell) fn pick_hdr_format(renderer: &GlesRenderer, kind: ContentKind) -> Option<(Fourcc, Modifier)> {
	let codes = match kind {
		ContentKind::Sdr => return None,
		ContentKind::Pq => [Fourcc::Abgr2101010, Fourcc::Xbgr2101010],
		ContentKind::ScRgb => [Fourcc::Abgr16161616f, Fourcc::Xbgr16161616f],
	};
	let set = <GlesRenderer as Bind<Dmabuf>>::supported_formats(renderer)?;
	codes
		.into_iter()
		.find_map(|code| set.iter().find(|format| format.code == code))
		.map(|format| (format.code, format.modifier))
}

render_elements! {
	/// One frame's render elements: the composited space, the Steam
	/// overlay/notification layer, and the software cursor drawn over both.
	///
	/// The cursor and every overlay are a [`WaylandSurfaceRenderElement`] (a
	/// client-drawn layer is just a surface buffer), differentiated by a
	/// [`Wrap`] so they can be variants of one enum and drawn in a single pass.
	/// They are SDR surfaces: over an HDR output they are drawn through the
	/// [`SdrOnHdr`] conversion.
	pub FrameRenderElements<=GlesRenderer>;
	Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
	ScaledSpace=RelocateRenderElement<RescaleRenderElement<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>>,
	ScaledOverlay=SdrOnHdr<RelocateRenderElement<RescaleRenderElement<Wrap<WaylandSurfaceRenderElement<GlesRenderer>>>>>,
	Cursor=SdrOnHdr<WaylandSurfaceRenderElement<GlesRenderer>>,
}

/// The frame's render elements, front to back.
pub(in crate::shell) type FrameElements = Vec<FrameRenderElements>;

/// Buffers reused across frames so the hot loop does not reallocate them.
#[derive(Default)]
pub(in crate::shell) struct FrameScratch {
	pub(in crate::shell) elements: FrameElements,
	pub(in crate::shell) role_ids: HashSet<Id>,
}

/// Render `elements` into a bound target, returning the GPU sync point for the
/// finished render.
///
/// `slot` names the target in `damage`'s buffer-age bookkeeping, so only the
/// region that changed since that target was last drawn is redrawn; an
/// unknown target is redrawn fully.
pub(in crate::shell) fn render_into(
	renderer: &mut GlesRenderer,
	target: &mut GlesTarget<'_>,
	elements: &[FrameRenderElements],
	damage: &mut FrameDamage,
	slot: usize,
) -> Result<SyncPoint> {
	let age = damage.ages.age(slot);
	let result = damage
		.tracker
		.render_output(renderer, target, age, elements, Color32F::new(0.0, 0.0, 0.0, 1.0));
	match result {
		Ok(result) => {
			let sync = result.sync;
			damage.ages.rendered(slot);
			Ok(sync)
		},
		Err(e) => {
			damage.ages.failed(slot);
			Err(Error::Render(format!("render_output: {e:?}")))
		},
	}
}
