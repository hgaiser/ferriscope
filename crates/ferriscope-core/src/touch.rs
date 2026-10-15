//! Touch click modes (gamescope's `TouchClickMode`, `backend.h:100-113`).
//!
//! gamescope's default is [`TouchClickMode::Left`]: a touch moves the pointer
//! to the touch position and presses the left button for as long as the finger
//! is down, so a finger behaves as a mouse click-drag. [`Passthrough`]
//! delivers real `wl_touch` events instead. Steam changes the mode at runtime
//! through the `STEAM_TOUCH_CLICK_MODE` root property
//! (`steamcompmgr.cpp:6844`).
//!
//! [`Passthrough`]: TouchClickMode::Passthrough

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;
/// `BTN_RIGHT`.
const BTN_RIGHT: u32 = 0x111;
/// `BTN_MIDDLE`.
const BTN_MIDDLE: u32 = 0x112;

/// What a touch does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TouchClickMode {
	/// Touch moves the pointer but presses no button.
	Hover,
	/// Touch moves the pointer and holds the left button (gamescope's default).
	#[default]
	Left,
	/// Touch moves the pointer and holds the right button.
	Right,
	/// Touch moves the pointer and holds the middle button.
	Middle,
	/// Touch points are delivered as `wl_touch` events.
	Passthrough,
	/// Touch is ignored.
	Disabled,
	/// The screen is a trackpad: the finger moves the pointer relative to where
	/// it is and holds the left button, without warping the pointer.
	Trackpad,
}

impl TouchClickMode {
	/// Decode the numeric value Steam writes to `STEAM_TOUCH_CLICK_MODE` and the
	/// `--default-touch-mode` option carries.
	///
	/// gamescope casts any value to its enum, and an unknown one falls into the
	/// `default:` arms, which behave as [`Hover`](Self::Hover).
	pub fn from_raw(value: u32) -> Self {
		match value {
			1 => Self::Left,
			2 => Self::Right,
			3 => Self::Middle,
			4 => Self::Passthrough,
			5 => Self::Disabled,
			6 => Self::Trackpad,
			_ => Self::Hover,
		}
	}

	/// The Linux button a touch presses in this mode, if any
	/// (`TouchClickModeToLinuxButton`).
	pub fn button(self) -> Option<u32> {
		match self {
			Self::Left | Self::Trackpad => Some(BTN_LEFT),
			Self::Right => Some(BTN_RIGHT),
			Self::Middle => Some(BTN_MIDDLE),
			Self::Hover | Self::Passthrough | Self::Disabled => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_default_mode_is_left_as_in_gamescope() {
		assert_eq!(TouchClickMode::default(), TouchClickMode::Left);
	}

	#[test]
	fn raw_values_follow_the_gamescope_enum_order() {
		let modes = [
			TouchClickMode::Hover,
			TouchClickMode::Left,
			TouchClickMode::Right,
			TouchClickMode::Middle,
			TouchClickMode::Passthrough,
			TouchClickMode::Disabled,
			TouchClickMode::Trackpad,
		];
		for (raw, mode) in modes.into_iter().enumerate() {
			assert_eq!(TouchClickMode::from_raw(raw as u32), mode);
		}
	}

	#[test]
	fn an_unknown_raw_value_behaves_as_hover() {
		assert_eq!(TouchClickMode::from_raw(7), TouchClickMode::Hover);
		assert_eq!(TouchClickMode::from_raw(u32::MAX), TouchClickMode::Hover);
	}

	#[test]
	fn buttons_match_the_mode() {
		assert_eq!(TouchClickMode::Left.button(), Some(0x110));
		assert_eq!(TouchClickMode::Trackpad.button(), Some(0x110));
		assert_eq!(TouchClickMode::Right.button(), Some(0x111));
		assert_eq!(TouchClickMode::Middle.button(), Some(0x112));
		assert_eq!(TouchClickMode::Hover.button(), None);
		assert_eq!(TouchClickMode::Passthrough.button(), None);
		assert_eq!(TouchClickMode::Disabled.button(), None);
	}
}
