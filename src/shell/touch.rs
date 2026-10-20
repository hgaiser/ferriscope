//! Touch input, ported from gamescope's `wlserver_touchdown`,
//! `wlserver_touchmotion` and `wlserver_touchup` (`wlserver.cpp:3229-3385`).
//!
//! What a touch does depends on the [`TouchClickMode`]. In every mode but
//! `Passthrough` and `Disabled` the finger is a mouse: down/motion warp the
//! pointer to the touch position (`Trackpad` moves it relatively instead) and
//! down holds a button until the touch lifts. `Passthrough` delivers real
//! `wl_touch` events to the surface under the touch.
//!
//! Touch coordinates are output logical coordinates and take the same
//! output-to-scene mapping as the absolute pointer (gamescope applies
//! `focusedWindowOffset` and scale to the touch position before routing it).

use std::collections::BTreeMap;

use smithay::{
	backend::input::InputTime,
	input::touch::{DownEvent, MotionEvent, UpEvent},
	utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{seat::InputEvent, shell::FerriscopeState, touch::TouchClickMode};

/// Touch routing state.
#[derive(Debug, Default)]
pub(in crate::shell) struct TouchState {
	/// The active click mode.
	pub(in crate::shell) mode: TouchClickMode,
	/// Buttons held by a touch in a pointer-emulating mode.
	///
	/// A touch up releases whatever a touch pressed, whichever mode pressed it
	/// (`wlserver_touchup`), so a mode change mid-touch cannot strand a button.
	held_buttons: Vec<u32>,
	/// Touch points delivered to a client as `wl_touch` (`touch_down_ids`), with
	/// their last position in the frame the client events use.
	passthrough_slots: BTreeMap<u32, Point<f64, Logical>>,
	/// Whether a `wl_touch` event was sent that a frame (or cancel) has not
	/// closed yet. Smithay warns about a frame or cancel without prior events.
	frame_open: bool,
}

impl TouchState {
	pub(in crate::shell) fn new(mode: TouchClickMode) -> Self {
		Self {
			mode,
			..Self::default()
		}
	}
}

impl FerriscopeState {
	/// Set the touch click mode, as Steam does through `STEAM_TOUCH_CLICK_MODE`.
	pub(in crate::shell) fn set_touch_click_mode(&mut self, mode: TouchClickMode) {
		tracing::debug!(?mode, "touch click mode");
		self.touch.mode = mode;
	}

	/// A touch point appeared (`wlserver_touchdown`).
	pub(in crate::shell) fn touch_down(&mut self, slot: u32, x: f64, y: f64) {
		let position = Point::from((x, y));
		match self.touch.mode {
			TouchClickMode::Disabled => {},
			TouchClickMode::Passthrough => {
				let (focus, location) = self.pointer_under(position);
				if focus.is_none() {
					return;
				}
				if let Some(touch) = self.seat.get_touch() {
					touch.down(
						self,
						focus,
						&DownEvent {
							slot: Some(slot).into(),
							location,
							serial: SERIAL_COUNTER.next_serial(),
							time: InputTime::now(),
						},
					);
					self.touch.passthrough_slots.insert(slot, location);
					self.touch.frame_open = true;
				}
			},
			mode => {
				// `Trackpad` leaves the pointer where it is.
				if mode != TouchClickMode::Trackpad {
					self.warp_pointer(position.x, position.y);
				}
				if let Some(button) = mode.button() {
					if !self.touch.held_buttons.contains(&button) {
						self.touch.held_buttons.push(button);
						self.pointer_button(button, true);
					}
				}
			},
		}
	}

	/// A touch point moved (`wlserver_touchmotion`).
	pub(in crate::shell) fn touch_motion(&mut self, slot: u32, x: f64, y: f64) {
		// gamescope clamps the touch position to the focused surface before
		// any mode looks at it.
		let position = self.clamp_to_capture(Point::from((x, y)));
		match self.touch.mode {
			TouchClickMode::Disabled => {},
			TouchClickMode::Passthrough => {
				let (focus, location) = self.pointer_under(position);
				// Only a point that went down on a client has one to move.
				let Some(last) = self.touch.passthrough_slots.get_mut(&slot) else {
					return;
				};
				*last = location;
				self.touch.frame_open = true;
				if let Some(touch) = self.seat.get_touch() {
					touch.motion(
						self,
						focus,
						&MotionEvent {
							slot: Some(slot).into(),
							location,
							time: InputTime::now(),
						},
					);
				}
			},
			TouchClickMode::Trackpad => {
				// The relative motion is the distance, in the focused surface's
				// own units, from the pointer to the touch.
				let delta = self.output_to_scene(position) - self.output_to_scene(self.cursor.location);
				self.apply_input(InputEvent::PointerMotionDelta {
					dx: delta.x,
					dy: delta.y,
				});
			},
			_ => self.warp_pointer(position.x, position.y),
		}
	}

	/// A touch point lifted (`wlserver_touchup`).
	pub(in crate::shell) fn touch_up(&mut self, slot: u32) {
		// Release whatever a touch pressed even if its surface is gone, or the
		// seat keeps the button down.
		for button in std::mem::take(&mut self.touch.held_buttons) {
			self.pointer_button(button, false);
		}
		if self.touch.passthrough_slots.remove(&slot).is_some() {
			self.touch.frame_open = true;
			if let Some(touch) = self.seat.get_touch() {
				touch.up(
					self,
					&UpEvent {
						slot: Some(slot).into(),
						serial: SERIAL_COUNTER.next_serial(),
						time: InputTime::now(),
					},
				);
			}
		}
	}

	/// The end of a group of touch events. Only `wl_touch` clients see frames;
	/// the pointer-emulating modes already frame their pointer events.
	pub(in crate::shell) fn touch_frame(&mut self) {
		if !std::mem::take(&mut self.touch.frame_open) {
			return;
		}
		if let Some(touch) = self.seat.get_touch() {
			touch.frame(self);
		}
	}

	/// The touch session was cancelled: drop every touch point and release the
	/// buttons the touches held.
	pub(in crate::shell) fn touch_cancel(&mut self) {
		for button in std::mem::take(&mut self.touch.held_buttons) {
			self.pointer_button(button, false);
		}
		// Smithay cancels only the points touched in the open frame, so touch
		// each live point where it last was before cancelling them all.
		let live = std::mem::take(&mut self.touch.passthrough_slots);
		if live.is_empty() && !self.touch.frame_open {
			return;
		}
		self.touch.frame_open = false;
		if let Some(touch) = self.seat.get_touch() {
			for (slot, location) in live {
				touch.motion(
					self,
					None,
					&MotionEvent {
						slot: Some(slot).into(),
						location,
						time: InputTime::now(),
					},
				);
			}
			touch.cancel(self);
		}
	}
}
