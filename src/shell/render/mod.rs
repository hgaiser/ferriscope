//! Frame composition: scene signature, element building, the offscreen render and readback.

pub(in crate::shell) mod damage;
pub(in crate::shell) mod elements;
pub(in crate::shell) mod hdr_shader;
pub(in crate::shell) mod passthrough;
pub(in crate::shell) mod pool;

use std::time::{Duration, Instant};

use smithay::{
	backend::renderer::{
		Bind,
		element::{
			AsRenderElements, Element, Kind, Wrap,
			surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
			utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
		},
		gles::GlesRenderer,
	},
	desktop::{space::SpaceRenderElements, utils::send_frames_surface_tree},
	reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
	utils::{Physical, Point, Scale},
	wayland::compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward},
};

use crate::{
	colour_meta::ColourMetadata,
	error::{Error, Result},
	focus::{OPAQUE, TRANSLUCENT},
	frame::{DmaBufFrame, Frame, FrameData, FrameInfo},
	hdr::{ContentColour, ContentKind, ContentMetadata},
	scaling::OutputFill,
};

#[cfg(feature = "test-support")]
use crate::frame::{PixelBuffer, PixelFormat};
use crate::shell::OverlayLayer;
use crate::shell::render::elements::FrameRenderElements;
use crate::shell::render::elements::FrameScratch;
use crate::shell::render::elements::render_into;
use crate::shell::render::elements::window_surface_ids;
use crate::shell::render::hdr_shader::SdrOnHdr;
use crate::shell::render::pool::encode_handle;
use crate::shell::render::pool::export_acquire_fence;
use crate::shell::render::pool::pick_free_slot;
use crate::shell::{FerriscopeState, geometry::window_wl_surface};
#[cfg(feature = "test-support")]
use smithay::{
	backend::{allocator::Fourcc, renderer::ExportMem},
	utils::{Buffer as BufferCoord, Rectangle},
};

/// The space's render elements.
type SpaceElements = Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>;

impl FerriscopeState {
	/// A cheap signature of the composited scene, used to skip re-compositing
	/// unchanged content.
	///
	/// It folds the commit epoch (so any client commit is a change), each
	/// window's surface id and location, each overlay's surface id, location and
	/// layer, and the cursor position when a cursor would actually be drawn.
	/// Content updates arrive as commits, so two ticks with the same signature
	/// composite the same pixels.
	pub(in crate::shell) fn scene_signature(&self) -> u64 {
		let mut hash = 0xcbf2_9ce4_8422_2325u64;
		fnv1a(&mut hash, self.scheduler.commit_epoch());
		for window in self.space.elements() {
			if let Some(surface) = self.window_content_surface(window) {
				fnv1a(&mut hash, u64::from(surface.id().protocol_id()));
			}
			if let Some(location) = self.space.element_location(window) {
				fnv1a(&mut hash, (location.x as u32 as u64) << 32 | location.y as u32 as u64);
			}
		}
		for overlay in &self.overlays {
			if let Some(surface) = self.window_content_surface(&overlay.window) {
				fnv1a(&mut hash, u64::from(surface.id().protocol_id()));
			}
			fnv1a(
				&mut hash,
				(overlay.location.x as u32 as u64) << 32 | overlay.location.y as u32 as u64,
			);
			fnv1a(&mut hash, overlay.role as u64);
			fnv1a(&mut hash, u64::from(self.overlay_opacity(overlay)));
		}
		// A hidden cursor contributes nothing; a visible one moves the
		// signature with its position, so pointer motion re-composites.
		if self.cursor_render_target().is_some() {
			fnv1a(
				&mut hash,
				(self.cursor.location.x as u32 as u64) << 32 | self.cursor.location.y as u32 as u64,
			);
		}
		hash
	}

	/// Render one frame and hand it to the sink.
	///
	/// The caller has checked that a pool slot is free. A delivered frame
	/// reached the sink (the scene may be marked clean); a skipped one did not
	/// (the caller must retry).
	pub(in crate::shell) fn render_frame(&mut self) -> Result<RenderOutcome> {
		self.render_frame_inner(Instant::now())
	}

	/// The render body behind [`Self::render_frame`].
	pub(in crate::shell) fn render_frame_inner(&mut self, now: Instant) -> Result<RenderOutcome> {
		let mut scratch = std::mem::take(&mut self.render_scratch);
		let outcome = self.render_frame_with(now, &mut scratch);
		// Drop the element references now rather than holding surfaces alive
		// until the next frame; the allocation is kept.
		scratch.elements.clear();
		self.render_scratch = scratch;
		outcome
	}

	/// The frame's metadata. Also settles the output encoding for the frame:
	/// the pool follows the base layer's HDR encoding (see
	/// [`Self::select_output_encoding`]), so this runs before a slot is picked.
	fn frame_info(&mut self, now: Instant) -> FrameInfo {
		let content = self.base_layer_colour();
		self.publish_app_wants_hdr(content.kind.is_hdr());
		let encoding = self.select_output_encoding(content.kind);
		let colour = match encoding {
			ContentKind::Sdr => ColourMetadata::sdr(),
			// The metadata belongs to the content the pool encoding carries.
			hdr => ContentColour {
				kind: hdr,
				metadata: if content.kind == hdr {
					content.metadata
				} else {
					ContentMetadata::default()
				},
			}
			.frame_metadata(),
		};
		FrameInfo {
			sequence: self.scheduler.frames_delivered(),
			captured_at: now,
			width: self.frame_size.w as u32,
			height: self.frame_size.h as u32,
			colour,
		}
	}

	/// Deliver the client's own buffer as the frame when the scene is exactly
	/// that buffer (see [`passthrough`]). `None` means the frame must be
	/// composited.
	fn try_passthrough(&mut self, now: Instant) -> Result<Option<RenderOutcome>> {
		let Some(candidate) = self.passthrough_candidate() else {
			return Ok(None);
		};
		// First, as for a composite: it settles the output encoding.
		let info = self.frame_info(now);
		// A borrowed buffer occupies a pool slot like a composited one, so the
		// consumer's backlog stays bounded by the pool either way.
		let Some(id) = pick_free_slot(&self.pool.delivered_generation) else {
			tracing::trace!("no free DMA-BUF slot, skipping frame");
			return Ok(Some(RenderOutcome::Skipped));
		};
		self.cursor.visible = false;
		let generation = self.pool.next_generation;
		self.pool.next_generation = self.pool.next_generation.wrapping_add(1);
		// The client's commit was gated on its acquire point or implicit fences
		// before it became current, so the buffer is complete.
		self.sink.submit(Frame {
			info,
			data: FrameData::DmaBuf(DmaBufFrame {
				buffer_id: encode_handle(id, generation),
				dmabuf: candidate.dmabuf,
				acquire_fence: None,
				release: None,
			}),
		})?;
		self.pool.delivered_generation[id] = Some(generation);
		self.pool.borrowed[id] = Some(candidate.buffer);
		tracing::trace!(target: "ferriscope::passthrough", slot = id, sequence = self.scheduler.frames_delivered(), "delivered client buffer");
		// Composited slots no longer match what the damage tracker saw.
		for slot in 0..self.pool.slots.len() {
			self.damage.ages.invalidate(slot);
		}
		Ok(Some(RenderOutcome::Delivered { zero_copy: true }))
	}

	fn render_frame_with(&mut self, now: Instant, scratch: &mut FrameScratch) -> Result<RenderOutcome> {
		if let Some(outcome) = self.try_passthrough(now)? {
			return Ok(outcome);
		}
		#[cfg(feature = "test-support")]
		let buffer_size = self.frame_size;
		// First, so the output encoding is settled before elements are built.
		let info = self.frame_info(now);
		let conversion = self.sdr_on_hdr_conversion();

		// Build elements before binding the target: the target holds a mutable
		// borrow of the renderer.
		let space_elements = self.space_elements()?;

		// `cursor.visible` means "an image was actually drawn", not merely "a
		// client set a cursor". A surface with no attached buffer yields no
		// element and leaves it hidden.
		//
		// The damage tracker takes elements front-to-back and draws them in
		// reverse, so the cursor comes first, then the overlays, then the space.
		let elements = &mut scratch.elements;
		elements.clear();
		self.cursor.visible = false;
		if let Some((surface, location)) = self.cursor_render_target() {
			let cursor_elements = render_elements_from_surface_tree::<
				GlesRenderer,
				WaylandSurfaceRenderElement<GlesRenderer>,
			>(&mut self.renderer, &surface, location, 1.0, 1.0, Kind::Cursor);
			self.cursor.visible = !cursor_elements.is_empty();
			elements.extend(
				cursor_elements
					.into_iter()
					.map(|element| FrameRenderElements::Cursor(SdrOnHdr::new(element, conversion.clone()))),
			);
		}
		if self.cursor_state.image().is_some() && !self.cursor.visible {
			// An image is set, yet produced no element: the client has not
			// committed a buffer to the cursor surface (yet).
			tracing::debug!(cursor_location = ?self.cursor.location, "cursor image set but not compositable");
		}

		// Steam overlays and notifications are client-drawn surfaces composited
		// as their own layer, over the fullscreen window and under the cursor,
		// in gamescope's order: base -> overlay -> notifications, cursor last.
		// Notifications precede the interactive overlay in the slice so they
		// draw later (on top). Each is rendered from its surface tree, like the
		// cursor, so both Wayland and Xwayland overlays work.
		//
		// A window the WSI layer drives is drawn from its override surface
		// (`window_content_surface`). The role holder's committed content is scaled to fill the output.
		// Only its own surface tree is transformed; dialogs, peers and the
		// cursor keep their size and position.
		let role_fill = self.output_fill();
		let output_size = self.output_rect().size;

		for role in [OverlayLayer::Notification, OverlayLayer::Interactive] {
			for overlay in self.overlays.iter().filter(|overlay| overlay.role == role) {
				let Some(surface) = self.window_content_surface(&overlay.window) else {
					continue;
				};
				// An overlay is painted at `_NET_WM_WINDOW_OPACITY / OPAQUE`
				// (`steamcompmgr.cpp:2660`); opacity 0 is how Steam hides it, and
				// `:3260`/`:3301` skip such a layer.
				let opacity = self.overlay_opacity(overlay);
				if opacity == TRANSLUCENT {
					continue;
				}
				let alpha = opacity as f32 / OPAQUE as f32;
				let location = Point::<i32, Physical>::from((overlay.location.x, overlay.location.y));
				let surface_elements = render_elements_from_surface_tree::<
					GlesRenderer,
					Wrap<WaylandSurfaceRenderElement<GlesRenderer>>,
				>(&mut self.renderer, &surface, location, 1.0, alpha, Kind::Unspecified);

				// gamescope paints an overlay with its own committed texture
				// fit to the output, centred, ignoring the window's position
				// (`steamcompmgr.cpp:3262`/`:3303` call `paint_window(overlay,
				// overlay)`, so `w == scaleW`; `:2575-2580` takes the source from
				// the committed texture, `:2603-2604`/`:2615` zero the window
				// offset, and `:2624`/`:2644` centre). This is independent of the
				// role holder: the fit is by the overlay's *own* content, so an
				// output-sized overlay is the identity and a smaller one is
				// scaled up. The fit is never gated on the role fill: the
				// Xwayland screen is the output, so a game can commit an
				// output-sized buffer (role fill identity) while its overlay is
				// sized from the game's render resolution and still needs scaling.
				let fill = self
					.window_source_size(&overlay.window)
					.and_then(|size| OutputFill::for_source(output_size, size));
				let (scale, target) = match fill {
					Some(fill) => (
						(fill.scale, fill.scale),
						Point::<i32, Physical>::from((fill.offset.0.round() as i32, fill.offset.1.round() as i32)),
					),
					None => ((1.0, 1.0), Point::<i32, Physical>::from((0, 0))),
				};
				elements.extend(surface_elements.into_iter().map(|element| {
					let rescaled = RescaleRenderElement::from_element(element, location, scale);
					let relocated =
						RelocateRenderElement::from_element(rescaled, target - location, Relocate::Relative);
					FrameRenderElements::ScaledOverlay(SdrOnHdr::new(relocated, conversion.clone()))
				}));
			}
		}

		match self
			.focus
			.fullscreen()
			.and_then(|window| self.window_content_surface(window))
		{
			Some(root) => window_surface_ids(&root, &mut scratch.role_ids),
			None => scratch.role_ids.clear(),
		}
		let role_ids = &scratch.role_ids;
		let role_origin = self.role_origin();
		let role_origin = Point::<i32, Physical>::from((role_origin.x.round() as i32, role_origin.y.round() as i32));
		let role_offset = role_fill
			.map(|fill| Point::<i32, Physical>::from((fill.offset.0.round() as i32, fill.offset.1.round() as i32)))
			.unwrap_or_default();
		elements.extend(space_elements.into_iter().map(|element| match role_fill {
			Some(fill) if role_ids.contains(element.id()) => {
				let rescaled = RescaleRenderElement::from_element(element, role_origin, (fill.scale, fill.scale));
				let relocated =
					RelocateRenderElement::from_element(rescaled, role_offset - role_origin, Relocate::Relative);
				FrameRenderElements::ScaledSpace(relocated)
			},
			_ => FrameRenderElements::Space(element),
		}));

		// A DMA-BUF sink takes the pool buffer directly. Every other sink keeps
		// the CPU readback path, which only the `test-support` recorder needs.
		let handoff = self.sink.wants_dmabuf();

		if self.use_dmabuf {
			// Pick a slot not held by the consumer. If all are busy the frame is
			// skipped: waiting on the consumer would stall the compositor.
			let Some(id) = pick_free_slot(&self.pool.delivered_generation) else {
				tracing::trace!("no free DMA-BUF slot, skipping frame");
				return Ok(RenderOutcome::Skipped);
			};
			if self.pool.slots[id].is_none() {
				// A fresh buffer holds nothing to build damage on.
				self.damage.ages.invalidate(id);
			}
			self.ensure_slot(id)?;

			let sync = {
				let dmabuf = self.pool.slots[id]
					.as_mut()
					.ok_or_else(|| Error::Render("dmabuf slot missing".into()))?;
				let mut target =
					Bind::bind(&mut self.renderer, dmabuf).map_err(|e| Error::Render(format!("bind dmabuf: {e:?}")))?;
				render_into(&mut self.renderer, &mut target, elements, &mut self.damage, id)?
			};
			// Client buffers (and their release points) outlive the composite's
			// GPU reads.
			self.hold_frame_buffers(&sync);
			// Diagnostic: with cadence tracing on, time the composite's GPU work
			// (submit to completion). Blocks the loop, so it is trace-only.
			if tracing::enabled!(target: "ferriscope::cadence", tracing::Level::TRACE) {
				let submitted = Instant::now();
				let _ = sync.wait();
				tracing::trace!(
					target: "ferriscope::cadence",
					composite_gpu_us = submitted.elapsed().as_micros() as u64,
					"composite gpu"
				);
			}

			if handoff {
				// Each delivery gets a fresh generation, so a late release of an
				// earlier delivery of the slot is recognised as stale. Generations
				// start at one so that a handle is never zero.
				let generation = self.pool.next_generation;
				self.pool.next_generation = self.pool.next_generation.wrapping_add(1);
				let acquire_fence = export_acquire_fence(&mut self.renderer, &sync)?;
				let dmabuf = self.pool.slots[id]
					.as_ref()
					.ok_or_else(|| Error::Render("dmabuf slot missing".into()))?
					.clone();
				self.sink.submit(Frame {
					info,
					data: FrameData::DmaBuf(DmaBufFrame {
						buffer_id: encode_handle(id, generation),
						dmabuf,
						acquire_fence,
						release: None,
					}),
				})?;
				self.pool.delivered_generation[id] = Some(generation);
				return Ok(RenderOutcome::Delivered { zero_copy: false });
			}

			// The CPU readback path reads the slot back through the GPU, instead
			// of mapping plane 0. A GBM buffer's modifier may be a tiled one
			// (NVIDIA block-linear on the reference renderer); its memory is not a
			// linear image, so mapping it as rows would produce garbage.
			// `copy_framebuffer` resolves the modifier through the GPU and returns
			// the same top-down row order as the renderbuffer readback. The pixel
			// path holds no slot: the sink copies the pixels synchronously.
			#[cfg(feature = "test-support")]
			{
				if !sync.is_reached() {
					if let Err(e) = sync.wait() {
						tracing::warn!("readback fence wait: {e:?}");
					}
				}
				let dmabuf = self.pool.slots[id]
					.as_mut()
					.ok_or_else(|| Error::Render("dmabuf slot missing".into()))?;
				let target =
					Bind::bind(&mut self.renderer, dmabuf).map_err(|e| Error::Render(format!("bind dmabuf: {e:?}")))?;
				let region = Rectangle::<i32, BufferCoord>::from_size(buffer_size);
				let mapping = self
					.renderer
					.copy_framebuffer(&target, region, Fourcc::Abgr8888)
					.map_err(|e| Error::Render(format!("readback: {e:?}")))?;
				drop(target);
				let pixels = {
					let slice = self
						.renderer
						.map_texture(&mapping)
						.map_err(|e| Error::Render(format!("map: {e:?}")))?;
					copy_top_down(slice, buffer_size.w as usize, buffer_size.h as usize)
				};
				self.sink.submit(Frame {
					info,
					data: FrameData::Pixels(PixelBuffer {
						format: PixelFormat::Rgba8,
						stride: buffer_size.w as usize * 4,
						data: pixels,
					}),
				})?;
				return Ok(RenderOutcome::Delivered { zero_copy: false });
			}
			#[cfg(not(feature = "test-support"))]
			unreachable!("a DMA-BUF-only build always hands the pool buffer over");
		}

		// The renderbuffer readback: a non-DMA-BUF render target, reachable only
		// with the `test-support` recorder.
		#[cfg(feature = "test-support")]
		{
			if self.frame_target.is_none() {
				self.damage.ages.invalidate(0);
			}
			self.ensure_renderbuffer()?;
			let mapping = {
				let target_buf = self
					.frame_target
					.as_mut()
					.ok_or_else(|| Error::Render("frame target missing".into()))?;
				let mut target =
					Bind::bind(&mut self.renderer, target_buf).map_err(|e| Error::Render(format!("bind: {e:?}")))?;
				let sync = render_into(&mut self.renderer, &mut target, elements, &mut self.damage, 0)?;
				if !sync.is_reached() {
					if let Err(e) = sync.wait() {
						tracing::warn!("readback fence wait: {e:?}");
					}
				}
				let region = Rectangle::<i32, BufferCoord>::from_size(buffer_size);
				self.renderer
					.copy_framebuffer(&target, region, Fourcc::Abgr8888)
					.map_err(|e| Error::Render(format!("readback: {e:?}")))?
			};
			// `copy_framebuffer` reads the framebuffer bottom-up in GL window
			// coordinates, but the render projection maps output row 0 to the GL
			// bottom row, so the mapping is already top-down in output
			// coordinates. `TextureMapping::flipped()` must not be used as a
			// signal: the GLES mapping returned by `copy_framebuffer` reports a
			// constant `true` regardless of the actual orientation.
			let pixels = {
				let slice = self
					.renderer
					.map_texture(&mapping)
					.map_err(|e| Error::Render(format!("map: {e:?}")))?;
				copy_top_down(slice, buffer_size.w as usize, buffer_size.h as usize)
			};
			self.sink.submit(Frame {
				info,
				data: FrameData::Pixels(PixelBuffer {
					format: PixelFormat::Rgba8,
					stride: buffer_size.w as usize * 4,
					data: pixels,
				}),
			})?;
			// The pixel path holds no slot: the sink copies the pixels
			// synchronously.
			Ok(RenderOutcome::Delivered { zero_copy: false })
		}
		#[cfg(not(feature = "test-support"))]
		unreachable!("a DMA-BUF-only build always renders into the pool");
	}

	/// Send frame callbacks to every tracked window.
	///
	/// With a `GAMESCOPE_FPS_LIMIT`, a window the limiter covers receives them
	/// only once its target cycle has elapsed since it last received requested
	/// ones (`steamcompmgr_should_vblank_window`, `steamcompmgr.cpp:6736`).
	pub(in crate::shell) fn send_frame_callbacks(&mut self) -> Vec<WlSurface> {
		let now = Instant::now();
		let time = self.start_time.elapsed();
		let output = &self.output;
		let limit = self.fps_limit;
		let mut ticked = Vec::new();
		// Every tracked window gets callbacks, not only the drawn ones: like
		// gamescope, a window behind the focus window or an overlay waiting to be
		// shown keeps running so it can present as soon as it is picked.
		for managed in self.windows.iter_mut() {
			if limit != 0
				&& crate::shell::schedule::limiter_covers(&managed.meta)
				&& !crate::shell::schedule::frame_callback_due(limit, managed.last_frame_done, now)
			{
				continue;
			}
			// The window's own surface and the WSI layer's override surface share
			// one throttle (`steamcompmgr_flush_frame_done`, `steamcompmgr.cpp:6690-6699`).
			let main = window_wl_surface(&managed.window);
			let content_override = self.content_overrides.override_of(&managed.window);
			let surfaces: Vec<WlSurface> = main.into_iter().chain(content_override.clone()).collect();
			// A FIFO barrier is released on this tick too, so it paces the
			// window exactly like a frame callback does.
			if limit != 0
				&& surfaces
					.iter()
					.any(|surface| has_frame_callbacks(surface) || crate::shell::fifo::has_fifo_barrier(surface))
			{
				managed.last_frame_done = Some(now);
			}
			managed
				.window
				.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
			if let Some(surface) = &content_override {
				send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
			}
			ticked.extend(surfaces);
		}
		ticked
	}

	/// The space's render elements, top-most first, like
	/// [`smithay::desktop::space_render_elements`]; a window the WSI layer drives
	/// contributes its override surface's tree instead of Xwayland's own.
	fn space_elements(&mut self) -> Result<SpaceElements> {
		let Some(output_geo) = self.space.output_geometry(&self.output) else {
			return Err(Error::Render("space elements: output is not mapped".into()));
		};
		let mut elements = Vec::new();
		for window in self.space.elements().rev() {
			let Some(location) = self.space.element_location(window) else {
				continue;
			};
			let render_location =
				(location - window.geometry().loc - output_geo.loc).to_physical_precise_round(Scale::from(1.0));
			if let Some(surface) = self.content_overrides.presented(window) {
				elements.extend(
					render_elements_from_surface_tree::<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>(
						&mut self.renderer,
						&surface,
						render_location,
						1.0,
						1.0,
						Kind::Unspecified,
					)
					.into_iter()
					.map(|element| SpaceRenderElements::Element(Wrap::from(element))),
				);
			} else if self
				.space
				.element_bbox(window)
				.is_some_and(|bbox| output_geo.overlaps(bbox))
			{
				elements.extend(
					window
						.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
							&mut self.renderer,
							render_location,
							Scale::from(1.0),
							1.0,
						)
						.into_iter()
						.map(|element| SpaceRenderElements::Element(Wrap::from(element))),
				);
			}
		}
		Ok(elements)
	}

	/// The overlay window's current `_NET_WM_WINDOW_OPACITY`.
	pub(in crate::shell) fn overlay_opacity(&self, overlay: &crate::shell::OverlayWindow) -> u32 {
		self.windows
			.get(overlay.id)
			.map(|managed| managed.meta.opacity)
			.unwrap_or(OPAQUE)
	}

	/// The cursor surface and its top-left in physical output coordinates, or
	/// `None` when no cursor image should be drawn (none set, or the client
	/// hid it).
	///
	/// The output scale is fixed at 1.0, so logical and physical coordinates
	/// coincide up to rounding.
	pub(in crate::shell) fn cursor_render_target(&self) -> Option<(WlSurface, Point<i32, Physical>)> {
		// `MouseCursor::paint` returns early while `bCursorHidden`
		// (`steamcompmgr.cpp:2327`).
		if self.cursor_suspension.hidden() {
			return None;
		}
		let surface = self.cursor_state.image()?.surface.clone();
		let location = self.cursor_state.image_location(self.cursor.location)?;
		Some((
			surface,
			Point::from((location.x.round() as i32, location.y.round() as i32)),
		))
	}
}

/// What [`FerriscopeState::render_frame_inner`] did with one frame.
pub(in crate::shell) enum RenderOutcome {
	/// A frame reached the sink. `zero_copy` when it is the client's own
	/// buffer ([`passthrough`]) rather than a composite.
	Delivered { zero_copy: bool },
	/// Nothing was handed to the sink (no free pool slot); the caller should
	/// retry.
	Skipped,
}

/// Whether any surface in `root`'s tree has a frame callback waiting.
fn has_frame_callbacks(root: &WlSurface) -> bool {
	let mut pending = false;
	with_surface_tree_downward(
		root,
		(),
		|_, _, ()| TraversalAction::DoChildren(()),
		|_, states, ()| {
			pending |= !states
				.cached_state
				.get::<SurfaceAttributes>()
				.current()
				.frame_callbacks
				.is_empty();
		},
		|_, _, ()| true,
	);
	pending
}

/// Fold `value` into an FNV-1a hash, used for the cheap scene signature.
pub(in crate::shell) fn fnv1a(hash: &mut u64, value: u64) {
	*hash ^= value;
	*hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
}

/// Take ownership of a tightly-packed, top-down readback in one copy.
///
/// Both readback paths (`ExportMem::copy_framebuffer` on the offscreen
/// renderbuffer and on the CPU-readback DMA-BUF) already return rows in output
/// order, top row first; this only copies the mapped slice out and zero-pads
/// missing rows (a short mapping yields blank rows rather than aborting the
/// compositor). A partial trailing row counts as missing.
#[cfg(feature = "test-support")]
pub(in crate::shell) fn copy_top_down(data: &[u8], width: usize, height: usize) -> Vec<u8> {
	let row = width * 4;
	let whole_rows = data.len().checked_div(row).unwrap_or(0).min(height);
	let mut out = Vec::with_capacity(row * height);
	out.extend_from_slice(&data[..whole_rows * row]);
	out.resize(row * height, 0);
	out
}

#[cfg(all(test, feature = "test-support"))]
mod tests {

	use super::*;

	#[test]
	fn top_down_rows_are_copied_in_order() {
		// Two rows of two RGBA pixels, already top row first.
		let data = vec![
			1, 1, 1, 1, 2, 2, 2, 2, // top row
			3, 3, 3, 3, 4, 4, 4, 4, // bottom row
		];
		let out = copy_top_down(&data, 2, 2);
		assert_eq!(out[0..4], [1, 1, 1, 1]);
		assert_eq!(out[8..12], [3, 3, 3, 3]);
	}

	#[test]
	fn single_row_is_unchanged() {
		let data = vec![1, 2, 3, 4, 5, 6, 7, 8];
		assert_eq!(copy_top_down(&data, 2, 1), data);
	}

	#[test]
	fn short_slice_is_padded_not_panicking() {
		// A mapping shorter than width * height must not panic or index out of
		// bounds; missing rows come back zeroed.
		let data = vec![1, 2, 3];
		let out = copy_top_down(&data, 2, 2);
		assert_eq!(out.len(), 16);
		assert!(out.iter().all(|byte| *byte == 0));
	}
}
