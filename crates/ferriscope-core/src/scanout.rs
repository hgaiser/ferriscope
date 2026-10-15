//! Cursor state owned by the compositor.

use smithay::utils::{Logical, Point};

/// Cursor state owned by the compositor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cursor {
	/// Pointer position in output logical coordinates.
	pub location: Point<f64, Logical>,
	/// Hotspot within the cursor image.
	pub hotspot: Point<i32, Logical>,
	/// Whether the cursor should be drawn.
	pub visible: bool,
}

impl Default for Cursor {
	fn default() -> Self {
		Self {
			location: Point::from((0.0, 0.0)),
			hotspot: Point::from((0, 0)),
			visible: false,
		}
	}
}
