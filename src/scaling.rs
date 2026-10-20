//! Output fill scaling.
//!
//! gamescope does not resize a game window to the output. It lets the client
//! keep its own resolution and scales the committed buffer up (or down) to fill
//! the output, centred. Resizing the window instead fails for Proton/Wine: Wine
//! resizes the outer X11 window to the forced size but keeps its real render
//! surface (a child window) at the game's resolution, so the presented buffer
//! is the output size with the game in a corner and black borders around it.
//!
//! The transform here is aspect-preserving with the smaller axis ratio
//! (gamescope's default `Auto`/fit scaler), so a 16:9 source fills a 16:9
//! output exactly and a mismatched aspect is letterboxed/pillarboxed, centred.
//! It is pure so the policy is testable without a GPU.

use smithay::utils::{Logical, Size};

/// Fractions of a logical pixel the clamped fill inverse leaves before the
/// exclusive content edge, so a boundary output point lands inside the surface
/// hit-test. Small enough to be imperceptible and robust against f64 rounding
/// at integer coordinates.
const SNAP: f64 = 1.0e-3;

/// The transform that maps the role holder's committed content into the output:
/// `output = content * scale + offset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputFill {
	/// Uniform scale: output pixels per content pixel.
	pub scale: f64,
	/// Centring offset in output pixels: where the content's top-left lands.
	pub offset: (f64, f64),
}

impl OutputFill {
	/// The fill transform for a `source` content size on an `output`.
	///
	/// Returns [`None`] when a size is empty or the content already fills the
	/// output exactly (a scale of `1.0` with no offset), so callers can skip the
	/// transform entirely. A scale of `1.0` **with** an offset (one axis matches
	/// the output and the other is shorter) still centres, so it is a transform.
	pub fn for_source(output: Size<i32, Logical>, source: Size<i32, Logical>) -> Option<Self> {
		if output.w <= 0 || output.h <= 0 || source.w <= 0 || source.h <= 0 {
			return None;
		}
		let scale = (f64::from(output.w) / f64::from(source.w)).min(f64::from(output.h) / f64::from(source.h));
		let offset = (
			(f64::from(output.w) - f64::from(source.w) * scale) / 2.0,
			(f64::from(output.h) - f64::from(source.h) * scale) / 2.0,
		);
		if (scale - 1.0).abs() < f64::EPSILON && offset.0.abs() < f64::EPSILON && offset.1.abs() < f64::EPSILON {
			return None;
		}
		Some(Self { scale, offset })
	}

	/// Map an output-space point into content space (the inverse transform).
	pub fn output_to_content(&self, x: f64, y: f64) -> (f64, f64) {
		((x - self.offset.0) / self.scale, (y - self.offset.1) / self.scale)
	}

	/// Map an output-space point into content space, clamped to the half-open
	/// content extent `[0, source)`.
	///
	/// The whole output maps onto the content, so a point on the output's far
	/// edge maps exactly onto `source` — the exclusive right/bottom edge of a
	/// surface hit-test (`Surface::surface_under`). Clamping lands it on the
	/// last content pixel instead, so the role holder owns the whole output
	/// including the boundary.
	///
	/// The clamp uses a small fraction of a logical pixel, not `f64::EPSILON`:
	/// f64 spacing near a coordinate like `178.0` is ~3e-14, so subtracting
	/// `EPSILON` rounds straight back to `178.0` and the boundary miss remains.
	pub fn output_to_content_clamped(&self, x: f64, y: f64, source: Size<i32, Logical>) -> (f64, f64) {
		let (cx, cy) = self.output_to_content(x, y);
		let max_x = (f64::from(source.w) - SNAP).max(0.0);
		let max_y = (f64::from(source.h) - SNAP).max(0.0);
		(cx.clamp(0.0, max_x), cy.clamp(0.0, max_y))
	}

	/// Map a content-space point into output space.
	pub fn content_to_output(&self, x: f64, y: f64) -> (f64, f64) {
		(x * self.scale + self.offset.0, y * self.scale + self.offset.1)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use smithay::utils::Size;

	#[test]
	fn a_smaller_16_9_source_scales_uniformly_and_fills() {
		let fill = OutputFill::for_source(Size::from((1920, 1080)), Size::from((1280, 720))).expect("scales");
		assert_eq!(fill.scale, 1.5);
		assert_eq!(fill.offset, (0.0, 0.0));
	}

	#[test]
	fn a_mismatched_aspect_letterboxes_and_centres() {
		// 1280x600 on 1920x1200: min(1.5, 2.0) = 1.5, centred vertically.
		let fill = OutputFill::for_source(Size::from((1920, 1200)), Size::from((1280, 600))).expect("scales");
		assert_eq!(fill.scale, 1.5);
		assert_eq!(fill.offset, (0.0, 150.0));
	}

	#[test]
	fn a_larger_source_scales_down() {
		let fill = OutputFill::for_source(Size::from((1920, 1080)), Size::from((3840, 2160))).expect("scales");
		assert_eq!(fill.scale, 0.5);
		assert_eq!(fill.offset, (0.0, 0.0));
	}

	#[test]
	fn an_output_sized_source_is_identity() {
		assert!(OutputFill::for_source(Size::from((1920, 1080)), Size::from((1920, 1080))).is_none());
	}

	#[test]
	fn a_width_matching_shorter_source_centres_without_scaling() {
		// 320x60 on 320x240: the widths match (scale 1.0) but the height is
		// shorter, so it is a centred transform, not the identity. A Steam
		// overlay full output width but shorter than the output must centre.
		let fill = OutputFill::for_source(Size::from((320, 240)), Size::from((320, 60))).expect("centres");
		assert_eq!(fill.scale, 1.0);
		assert_eq!(fill.offset, (0.0, 90.0));
	}

	#[test]
	fn empty_sizes_are_identity() {
		assert!(OutputFill::for_source(Size::from((0, 0)), Size::from((1280, 720))).is_none());
		assert!(OutputFill::for_source(Size::from((1920, 1080)), Size::from((0, 720))).is_none());
	}

	#[test]
	fn output_content_round_trips() {
		let fill = OutputFill::for_source(Size::from((1920, 1200)), Size::from((1280, 600))).expect("scales");
		let (cx, cy) = fill.output_to_content(960.0, 750.0);
		assert_eq!((cx, cy), (640.0, 400.0));
		assert_eq!(fill.content_to_output(cx, cy), (960.0, 750.0));
	}

	#[test]
	fn the_far_edge_maps_inside_the_clamped_content() {
		// 1280x720 on 1920x1080: scale 1.5, no offset, so output x = 1920 maps
		// to content 1280 — the exclusive surface edge. The clamp must land it
		// strictly inside, or the role loses focus on the last output pixel
		let fill = OutputFill::for_source(Size::from((1920, 1080)), Size::from((1280, 720))).expect("scales");
		let source = Size::from((1280, 720));
		let (cx, cy) = fill.output_to_content_clamped(1919.0, 1079.0, source);
		assert!(cx < 1280.0, "x {cx} must be inside the surface");
		assert!(cy < 720.0, "y {cy} must be inside the surface");
		// The exact far edge (a rounding artefact) also clamps inside.
		let (ex, ey) = fill.output_to_content_clamped(1920.0, 1080.0, source);
		assert!(ex < 1280.0 && ey < 720.0, "exact edge ({ex}, {ey}) must clamp inside");
	}

	#[test]
	fn a_letterbox_point_inside_the_output_owns_the_role() {
		// 160x160 on 320x240: scale 1.5, content 240x240 centred at x offset 40.
		// A point in the left letterbox (x < 40) maps to a negative content x;
		// the clamp lands it at the surface's left edge so the role still owns
		// the whole output.
		let fill = OutputFill::for_source(Size::from((320, 240)), Size::from((160, 160))).expect("scales");
		let source = Size::from((160, 160));
		let (cx, _) = fill.output_to_content_clamped(5.0, 120.0, source);
		assert_eq!(cx, 0.0, "a letterbox point clamps to the surface edge");
	}
}
