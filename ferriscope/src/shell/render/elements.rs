//! Render-element types and the offscreen render call.

use std::collections::HashSet;

use smithay::{
	backend::{
		allocator::{Format, Fourcc, Modifier, dmabuf::Dmabuf, format::FormatSet},
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

/// A pool buffer format: a fourcc and every modifier the renderer can render
/// into with it.
///
/// The allocator gets the whole list and the driver picks per allocation. A
/// modifier can be renderable yet not allocatable at every size: on AMD GFX10
/// and GFX11, DCC without independent 64B blocks is limited to 2560x2560, and
/// it is the driver's first choice, so pinning one modifier fails every
/// allocation above 1440p.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::shell) struct RenderFormat {
	pub(in crate::shell) code: Fourcc,
	/// The explicit modifiers in the driver's order of preference, or only
	/// [`Modifier::Invalid`] (implicit layout) if the driver lists none.
	pub(in crate::shell) modifiers: Vec<Modifier>,
}

impl RenderFormat {
	/// `code` with its renderable modifiers from `set`, if it has any.
	fn from_set(set: &FormatSet, code: Fourcc) -> Option<Self> {
		let mut modifiers: Vec<Modifier> = set
			.iter()
			.filter(|format| format.code == code && format.modifier != Modifier::Invalid)
			.map(|format| format.modifier)
			.collect();
		if modifiers.is_empty() {
			if !set.contains(&Format {
				code,
				modifier: Modifier::Invalid,
			}) {
				return None;
			}
			modifiers.push(Modifier::Invalid);
		}
		Some(Self { code, modifiers })
	}
}

/// Pick a format the renderer can render into as a DMA-BUF.
pub(in crate::shell) fn pick_render_format(renderer: &GlesRenderer) -> Result<RenderFormat> {
	let set = <GlesRenderer as Bind<Dmabuf>>::supported_formats(renderer)
		.ok_or_else(|| Error::Gpu("renderer exposes no dmabuf render formats".into()))?;
	[Fourcc::Abgr8888, Fourcc::Argb8888, Fourcc::Xrgb8888]
		.into_iter()
		.chain(set.iter().next().map(|format| format.code))
		.find_map(|code| RenderFormat::from_set(&set, code))
		.ok_or_else(|| Error::Gpu("dmabuf render format set is empty".into()))
}

/// The render target format for HDR output of `kind`, if the renderer can draw
/// into one: 10-bit for PQ, FP16 for scRGB (ABGR/XBGR 2101010 and 16161616F),
/// layouts a Vulkan video encoder can import.
pub(in crate::shell) fn pick_hdr_format(renderer: &GlesRenderer, kind: ContentKind) -> Option<RenderFormat> {
	let codes = match kind {
		ContentKind::Sdr => return None,
		ContentKind::Pq => [Fourcc::Abgr2101010, Fourcc::Xbgr2101010],
		ContentKind::ScRgb => [Fourcc::Abgr16161616f, Fourcc::Xbgr16161616f],
	};
	let set = <GlesRenderer as Bind<Dmabuf>>::supported_formats(renderer)?;
	codes.into_iter().find_map(|code| RenderFormat::from_set(&set, code))
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

#[cfg(test)]
mod gpu_tests {
	use std::sync::Arc;

	use smithay::backend::{
		allocator::{
			Allocator,
			gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
		},
		egl::{EGLContext, EGLDisplay},
	};

	use super::*;

	/// Every picked format allocates above 2560x2560, where AMD GFX10/GFX11
	/// reject the driver's preferred DCC modifier.
	#[test]
	fn picked_formats_allocate_above_1440p() {
		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open("/dev/dri/renderD128")
			.expect("open the render node");
		let gbm = GbmDevice::new(Arc::new(file)).expect("gbm device");
		let display = unsafe { EGLDisplay::new(gbm.clone()) }.expect("egl display");
		let context = EGLContext::new(&display).expect("egl context");
		let renderer = unsafe { GlesRenderer::new(context) }.expect("gles");
		let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);

		let formats = std::iter::once(pick_render_format(&renderer).expect("an SDR format"))
			.chain(pick_hdr_format(&renderer, ContentKind::Pq))
			.chain(pick_hdr_format(&renderer, ContentKind::ScRgb));
		for format in formats {
			for (width, height) in [(3840, 2160), (3840, 2400), (5120, 2880)] {
				allocator
					.create_buffer(width, height, format.code, &format.modifiers)
					.unwrap_or_else(|e| panic!("allocate {width}x{height} {format:?}: {e}"));
			}
		}
	}
}
