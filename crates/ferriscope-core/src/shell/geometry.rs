//! Output-fill helpers and the scene/output coordinate maps.

use smithay::{
	backend::renderer::utils::RendererSurfaceStateUserData,
	desktop::Window,
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	utils::{Logical, Point, Rectangle, Size},
	wayland::compositor::{get_parent, with_states},
};

use crate::scaling::OutputFill;

use crate::shell::FerriscopeState;

/// The `wl_surface` backing a window, whether Wayland or X11.
pub(in crate::shell) fn window_wl_surface(window: &Window) -> Option<WlSurface> {
	if let Some(toplevel) = window.toplevel() {
		Some(toplevel.wl_surface().clone())
	} else {
		window.x11_surface().and_then(|surface| surface.wl_surface())
	}
}

/// The union of two half-open output-space rects `(x0, y0, x1, y1)`.
pub(in crate::shell) fn union_rect(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
	(a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

/// Map an output-space pointer position into an overlay's surface-local
/// (content) space for the pointer hit-test.
///
/// The overlay is drawn through its own centred content-to-output fit
/// (`OutputFill`), independent of the role holder, so the input region must be
/// hit-tested in the content space that fit maps from. With no fit (the content
/// already fills the output) the overlay is placed at its scene location and
/// the point is simply relative to the window origin.
pub(in crate::shell) fn overlay_pointer_point(
	pos: Point<f64, Logical>,
	fill: Option<OutputFill>,
	location: Point<i32, Logical>,
) -> Point<f64, Logical> {
	match fill {
		Some(fill) => {
			let (cx, cy) = fill.output_to_content(pos.x, pos.y);
			Point::from((cx, cy))
		},
		None => pos - location.to_f64(),
	}
}

/// Walk a surface to its root, bounded so a malformed parent chain cannot loop.
pub(in crate::shell) fn root_surface(surface: &WlSurface) -> WlSurface {
	let mut root = surface.clone();
	for _ in 0..128 {
		match get_parent(&root) {
			Some(parent) => root = parent,
			None => break,
		}
	}
	root
}

impl FerriscopeState {
	/// The output's own size at the output origin — the true output size, not
	/// the bounding box of mapped windows.
	///
	/// The mode is created from `config.width`/`config.height` and never
	/// changed, so those are exact. Used for classification and as the target of
	/// the output-fill transform.
	pub(in crate::shell) fn output_rect(&self) -> Rectangle<i32, Logical> {
		Rectangle::from_size(Size::from((self.config.width as i32, self.config.height as i32)))
	}

	/// The scene origin a window's surface tree is mapped at.
	///
	/// The client's own requested geometry origin (X11 `last_configure().loc`,
	/// Wayland `geometry().loc`), not a fixed `(0, 0)`: a Wayland toplevel may
	/// place itself away from the output origin after a `xdg_surface.set_window_geometry`,
	/// and pinning it at `(0, 0)` would leave the surface, its input region and
	/// its subsurfaces at a different origin than the compositor maps the
	/// element at. X11 windows already carry their configure origin through
	/// [`Self::place_x11_window`]; this makes the Wayland map paths agree.
	pub(in crate::shell) fn window_map_origin(window: &Window) -> Point<i32, Logical> {
		window.geometry().loc
	}

	/// The committed content size of the role holder, in logical pixels.
	///
	/// The committed buffer is the actual content a client presents; for
	/// Proton/Wine the outer window can be larger than the game's render
	/// surface, so the window geometry is not a reliable content size. Falls
	/// back to the window geometry before the first buffer is committed.
	pub(in crate::shell) fn role_source_size(&self) -> Option<Size<i32, Logical>> {
		self.window_source_size(self.focus.fullscreen()?)
	}

	/// The committed content size of `window`, in logical pixels.
	///
	/// This is the surface size, the viewport destination when the client set
	/// one, because that is the size the surface is drawn and hit-tested at; the
	/// buffer size would mis-scale a viewported surface. The committed buffer is
	/// the actual content a client presents; for
	/// Proton/Wine the outer window can be larger than the game's render
	/// surface, so the window geometry is not a reliable content size. Falls
	/// back to the window geometry before the first buffer is committed. Used
	/// for the role holder's output fill and for an overlay's own
	/// content-to-output fill while it is active.
	pub(in crate::shell) fn window_source_size(&self, window: &Window) -> Option<Size<i32, Logical>> {
		let surface = self.window_content_surface(window)?;
		let buffer = with_states(&surface, |states| {
			states
				.data_map
				.get::<RendererSurfaceStateUserData>()
				.and_then(|data| data.lock().unwrap().surface_size())
		});
		buffer.or_else(|| Some(window.geometry().size))
	}

	/// The transform that scales the role holder's content to fill the output,
	/// or [`None`] when it already fills it.
	///
	/// The role holder is always a [`SurfaceKind::Toplevel`] (it enters the role
	/// through `map_toplevel`), so `should_fill_output` holds by construction and
	/// the (X11-round-tripping) Steam classification is not needed here.
	pub(in crate::shell) fn output_fill(&self) -> Option<OutputFill> {
		OutputFill::for_source(self.output_rect().size, self.role_source_size()?)
	}

	/// The role holder's top-left in scene coordinates.
	pub(in crate::shell) fn role_origin(&self) -> Point<f64, Logical> {
		self.focus
			.fullscreen()
			.and_then(|window| self.space.element_location(window))
			.map(|location| location.to_f64())
			.unwrap_or_default()
	}

	/// Map an output-space point into the role holder's scene space, inverting
	/// the output-fill transform.
	///
	/// A no-op when the role holder already fills the output, so unscaled
	/// sessions are unchanged.
	pub(in crate::shell) fn output_to_scene(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
		match self.output_fill() {
			Some(fill) => {
				let (x, y) = fill.output_to_content(pos.x, pos.y);
				let origin = self.role_origin();
				Point::from((x + origin.x, y + origin.y))
			},
			None => pos,
		}
	}

	/// Map a role-holder scene-space point back into output space.
	pub(in crate::shell) fn scene_to_output(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
		match self.output_fill() {
			Some(fill) => {
				let origin = self.role_origin();
				let (x, y) = fill.content_to_output(pos.x - origin.x, pos.y - origin.y);
				Point::from((x, y))
			},
			None => pos,
		}
	}

	/// Map a role-holder surface-local motion delta into output space for the
	/// capture cursor position (`* fill.scale`; identity without fill).
	///
	/// The delta a client's relative pointer receives is left in surface-local
	/// units; only the compositor cursor lives in output space and is scaled
	/// back up, because the role holder's content is scaled down from the
	/// output.: this replaces `scene_input_delta`, which
	/// divided the *delivered* delta and reduced the look range.
	pub(in crate::shell) fn cursor_input_delta(&self, dx: f64, dy: f64) -> (f64, f64) {
		match self.output_fill() {
			Some(fill) => (dx * fill.scale, dy * fill.scale),
			None => (dx, dy),
		}
	}

	/// The window whose root surface is `surface`, if any.
	pub(in crate::shell) fn window_for_wl_surface(&self, surface: &WlSurface) -> Option<Window> {
		let root = root_surface(surface);
		self.space
			.elements()
			.find(|window| window_wl_surface(window).as_ref() == Some(&root))
			.cloned()
	}

	/// The output-space origin of the window `surface` belongs to.
	pub(in crate::shell) fn window_origin_for_surface(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
		let window = self.window_for_wl_surface(surface)?;
		self.space.element_location(&window).map(|location| location.to_f64())
	}

	/// The output-space centre of the window `surface` belongs to.
	pub(in crate::shell) fn window_center_for_surface(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
		let window = self.window_for_wl_surface(surface)?;
		let location = self.space.element_location(&window)?.to_f64();
		let size = window.geometry().size;
		Some(location + Point::from((f64::from(size.w) / 2.0, f64::from(size.h) / 2.0)))
	}

	/// A window's rendered content rect in output space, half-open
	/// (`x0..x1`, `y0..y1`).
	///
	/// The role holder and Steam overlays are drawn with their own
	/// content-to-output fill: the role holder while it is fit-scaled
	///and every overlay always; dialogs, peers and popups keep their scene placement.
	pub(in crate::shell) fn rendered_window_rect(&self, window: &Window) -> (f64, f64, f64, f64) {
		let output = self.output_rect().size;
		let is_role = self.focus.fullscreen() == Some(window);
		let overlay = self.overlays.iter().find(|overlay| &overlay.window == window);
		// The role holder is transformed only while its output fill is active;
		// a Steam overlay is always fit to the output, whether or not the role
		// holder is scaled.
		let fits_output = overlay.is_some() || (is_role && self.output_fill().is_some());
		if fits_output {
			let source = if is_role {
				self.role_source_size()
			} else {
				self.window_source_size(window)
			};
			if let Some(fill) = source.and_then(|size| OutputFill::for_source(output, size)) {
				let source = source.unwrap_or_default();
				let (x0, y0) = fill.content_to_output(0.0, 0.0);
				let (x1, y1) = fill.content_to_output(f64::from(source.w), f64::from(source.h));
				return (x0, y0, x1, y1);
			}
			// Identity fill for this window: it fills the output.
			return (0.0, 0.0, f64::from(output.w), f64::from(output.h));
		}
		let location = match overlay {
			Some(overlay) => overlay.location.to_f64(),
			None => self
				.space
				.element_location(window)
				.map(|p| p.to_f64())
				.unwrap_or_default(),
		};
		let size = self
			.window_source_size(window)
			.unwrap_or_else(|| window.geometry().size);
		(
			location.x,
			location.y,
			location.x + f64::from(size.w),
			location.y + f64::from(size.h),
		)
	}

	/// A surface's committed content size in logical (surface-local) pixels,
	/// falling back to its window geometry before the first buffer commit.
	pub(in crate::shell) fn surface_logical_size(&self, surface: &WlSurface) -> Option<Size<i32, Logical>> {
		with_states(surface, |states| {
			states
				.data_map
				.get::<RendererSurfaceStateUserData>()
				.and_then(|data| data.lock().unwrap().surface_size())
		})
		.or_else(|| self.window_for_wl_surface(surface).map(|window| window.geometry().size))
	}
}

#[cfg(test)]
mod tests {

	use smithay::utils::{Logical, Point, Size};

	use super::*;
	use crate::scaling::OutputFill;

	#[test]
	fn overlay_pointer_point_inverts_the_overlay_fit() {
		// A 60x60 overlay content on a 320x240 output is fit `4x`, centred at
		// x offset 40 (`steam_overlay_is_composited_over_a_scaled_role`). The
		// point (150, 120) is over the overlay's *drawn* area; it maps into the
		// 60x60 content it was rendered from.
		let fill = OutputFill::for_source(Size::from((320, 240)), Size::from((60, 60))).expect("fit");
		let point = Point::<f64, Logical>::from((150.0, 120.0));
		let local = overlay_pointer_point(point, Some(fill), Point::from((10, 10)));
		assert!(
			local.x >= 0.0 && local.x < 60.0 && local.y >= 0.0 && local.y < 60.0,
			"a point over the drawn overlay must map into its content: {local:?}"
		);
		// A raw hit-test relative to the window origin would fall outside the
		// 60x60 content and miss the overlay.
		let raw = point - Point::<f64, Logical>::from((10.0, 10.0));
		assert!(raw.x >= 60.0 || raw.y >= 60.0, "the raw point is {raw:?}");
	}

	#[test]
	fn overlay_pointer_point_without_a_fit_is_relative_to_the_window() {
		// With no fit the overlay keeps its scene placement: the point is
		// relative to the window origin.
		let point = Point::<f64, Logical>::from((250.0, 130.0));
		let local = overlay_pointer_point(point, None, Point::from((10, 10)));
		assert_eq!(local, Point::from((240.0, 120.0)));
	}
}
