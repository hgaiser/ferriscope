//! The input events the compositor's seat accepts.
//!
//! ferriscope has no libinput backend. Keyboard, pointer and touch events
//! arrive from the embedder (over [`crate::InputSender`]) and are injected into
//! the Smithay [`smithay::input::Seat`]. Controller/gamepad input is
//! deliberately **not** handled here: it has no Wayland or X11 route and must
//! reach the application as a virtual device.
//!
//! Coordinates are logical output coordinates. `PointerAxis` carries a scroll
//! amount in **wheel notches** (one detent) using the Wayland `wl_pointer.axis`
//! sign convention: horizontal positive is right, vertical positive is down
//! (content moves up). The injector expands a notch into the continuous axis
//! value and the discrete `axis_value120` step; the mapping from a host's native
//! sign/units lives in the host adapter, not here.
//!
//! Touch is routed according to the touch click mode
//! ([`crate::TouchClickMode`], gamescope's default is a left click at the touch
//! position); `slot` is the touch point id and the coordinates map to the scene
//! exactly like `PointerMotion`. Send [`InputEvent::TouchFrame`] after each
//! batch of touch events: it is what flushes them to a `wl_touch` client.

/// An input event for the compositor's seat.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
	/// A key press or release, by Linux evdev keycode.
	Key {
		/// evdev keycode (`KEY_*`).
		code: u32,
		/// Whether the key went down.
		pressed: bool,
	},
	/// Absolute pointer motion, in output logical coordinates.
	PointerMotion {
		/// X coordinate.
		x: f64,
		/// Y coordinate.
		y: f64,
	},
	/// A pointer button press or release, by evdev button code.
	PointerButton {
		/// evdev button code (`BTN_*`).
		code: u32,
		/// Whether the button went down.
		pressed: bool,
	},
	/// Relative pointer motion (mouselook).
	PointerMotionDelta {
		/// Delta X.
		dx: f64,
		/// Delta Y.
		dy: f64,
	},
	/// A scroll-wheel detent on one axis.
	///
	/// `value` is in wheel notches (one detent) with the `wl_pointer.axis` sign
	/// convention: positive is right for [`horizontal`](Self::PointerAxis::horizontal)
	/// and down for vertical. The injector expands it to the continuous axis
	/// value and the discrete `axis_value120` step.
	PointerAxis {
		/// Whether this is the horizontal axis (`h`) rather than vertical (`v`).
		horizontal: bool,
		/// Scroll amount in wheel notches.
		value: f64,
	},
	/// A touch point appeared, in output logical coordinates.
	TouchDown {
		/// The touch point id.
		slot: u32,
		/// X coordinate.
		x: f64,
		/// Y coordinate.
		y: f64,
	},
	/// A touch point moved, in output logical coordinates.
	TouchMotion {
		/// The touch point id.
		slot: u32,
		/// X coordinate.
		x: f64,
		/// Y coordinate.
		y: f64,
	},
	/// A touch point lifted.
	TouchUp {
		/// The touch point id.
		slot: u32,
	},
	/// The touch session was cancelled: every touch point is dropped.
	TouchCancel,
	/// The end of a group of touch events that belong together.
	TouchFrame,
}
