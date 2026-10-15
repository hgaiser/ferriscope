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
//!
//! Pen input ([`InputEvent::Pen`]) is a ferriscope extension: gamescope has no
//! graphics-tablet support. It is delivered through `zwp_tablet_v2` to the
//! surface under the pen (Xwayland turns it into an XI2 tablet device), with the
//! same output-to-scene mapping as the pointer and touch. See [`PenEvent`].

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
	/// Text to type into the focused client, any layout and any character.
	///
	/// It is typed the way Steam's on-screen keyboard is
	/// (`gamescope_input_method`): a character the seat keymap produces is its
	/// plain key (with Shift/Ctrl/Alt as its level needs), anything else goes
	/// through a temporary keymap that is reset once typing stops. A paste may
	/// arrive as one event per character or as one event: text is gathered for
	/// a few milliseconds and typed together, so a keymap change is shared by
	/// many characters, and a [`Key`](Self::Key) sent after it still comes
	/// after it. At most 64 KiB wait to be typed at once; the excess is
	/// ignored.
	Text(String),
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
	/// A pen (graphics tablet tool) sample: hover, contact or movement.
	///
	/// A ferriscope extension (gamescope has no tablet support).
	Pen(PenEvent),
	/// The pen's stylus buttons changed while it stays where it was.
	///
	/// Ignored while no pen is in proximity.
	PenButtons {
		/// Stylus button bitmask, see [`PenEvent::buttons`].
		buttons: u32,
	},
	/// The pen left proximity (hover leave or cancel): the tip lifts, held
	/// buttons release and the client sees `proximity_out`.
	PenLeave,
}

/// Which end of the stylus is in use.
///
/// Switching tools while one is in proximity ends the first tool's proximity
/// before the second one enters, as a real tablet does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PenTool {
	/// The pen tip (`zwp_tablet_tool_v2.type.pen`).
	Pen,
	/// The eraser end (`zwp_tablet_tool_v2.type.eraser`).
	Eraser,
}

/// One pen sample.
///
/// Moonlight mapping: `hover` is `contact: false` with `distance`; `down` and
/// `move` are `contact: true` with `pressure`; `up` is `contact: false`. The
/// first sample after [`InputEvent::PenLeave`] brings the pen into proximity
/// on the surface under it (nothing happens if no surface is there).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenEvent {
	/// The tool in use.
	pub tool: PenTool,
	/// X coordinate, in output logical coordinates (like touch).
	pub x: f64,
	/// Y coordinate, in output logical coordinates (like touch).
	pub y: f64,
	/// Whether the tip touches the tablet. A change sends `down` or `up`.
	pub contact: bool,
	/// Tip pressure, `0.0..=1.0`; clamped. `None` while touching leaves the
	/// pressure axis unreported, hovering reports `0.0`.
	pub pressure: Option<f64>,
	/// Hover distance, `0.0..=1.0`; clamped. `None` while hovering leaves the
	/// distance axis unreported, touching reports `0.0`.
	pub distance: Option<f64>,
	/// Tilt `(x, y)` in degrees (`-90..=90`), as `zwp_tablet_tool_v2.tilt`:
	/// `x` is positive when the stylus leans right, `y` positive when it leans
	/// towards the user. [`pen_tilt`] converts Moonlight's rotation and tilt.
	pub tilt: Option<(f64, f64)>,
	/// Stylus buttons held: bit 0 is `BTN_STYLUS`, bit 1 `BTN_STYLUS2`, bit 2
	/// `BTN_STYLUS3` (Moonlight's `PEN_BUTTON_*` order). Changed bits send
	/// press/release events.
	pub buttons: u32,
}

/// Convert Moonlight's polar pen orientation to `zwp_tablet_tool_v2` tilt.
///
/// `rotation` is the azimuth in degrees (0 is up on the screen, clockwise) and
/// `tilt` the angle from the surface normal in degrees, clamped to `0..=90`.
/// Returns the `(x, y)` tilt for [`PenEvent::tilt`]. Port of Moonshine's
/// previous compositor (`magnitude * sin(angle)`, `-magnitude * cos(angle)`).
pub fn pen_tilt(rotation: f64, tilt: f64) -> (f64, f64) {
	let angle = rotation.to_radians();
	let magnitude = tilt.clamp(0.0, 90.0);
	(magnitude * angle.sin(), -magnitude * angle.cos())
}

#[cfg(test)]
mod tests {
	use super::pen_tilt;

	fn close(a: (f64, f64), b: (f64, f64)) -> bool {
		(a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
	}

	#[test]
	fn tilt_follows_the_azimuth() {
		assert!(close(pen_tilt(0.0, 30.0), (0.0, -30.0)));
		assert!(close(pen_tilt(90.0, 30.0), (30.0, 0.0)));
		assert!(close(pen_tilt(180.0, 30.0), (0.0, 30.0)));
		assert!(close(pen_tilt(270.0, 30.0), (-30.0, 0.0)));
	}

	#[test]
	fn tilt_magnitude_is_clamped() {
		assert!(close(pen_tilt(90.0, 200.0), (90.0, 0.0)));
		assert!(close(pen_tilt(90.0, -5.0), (0.0, 0.0)));
	}
}
