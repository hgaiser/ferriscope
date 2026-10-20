//! Pen (graphics tablet) input through `zwp_tablet_v2`.
//!
//! A deliberate ferriscope extension: gamescope has no tablet support. The seat
//! owns one virtual tablet and one tool per [`PenTool`]; [`InputEvent::Pen`]
//! samples drive them. Xwayland binds the same global and exposes the tablet to
//! X clients as an XI2 tablet device.
//!
//! Routing mirrors the pointer and touch paths: the sample position is an
//! output logical coordinate resolved by [`FerriscopeState::pointer_under`], so
//! overlays win over the window and the output-fill transform maps the point to
//! surface-local coordinates. The semantics are ported from Moonshine's former
//! compositor (`process_pen_input`): proximity begins on the first sample over a
//! surface, a tool switch leaves proximity first, and a leave/cancel lifts the
//! tip and releases buttons.

use smithay::{
	backend::input::{ButtonState, InputTime, TabletToolCapabilities, TabletToolDescriptor, TabletToolType},
	input::tablet::{
		TabletDescriptor, TabletSeatTrait,
		tool::{
			AxisFrame, ButtonEvent, DownEvent, MotionEvent, ProximityInEvent, ProximityOutEvent, TabletToolHandle,
			UpEvent,
		},
	},
	utils::{Point, SERIAL_COUNTER},
};

use crate::{
	seat::{PenEvent, PenTool},
	shell::FerriscopeState,
};

/// `BTN_STYLUS`, `BTN_STYLUS2` and `BTN_STYLUS3` by [`PenEvent::buttons`] bit.
const STYLUS_BUTTONS: [(u32, u32); 3] = [(0x01, 0x14b), (0x02, 0x14c), (0x04, 0x149)];

/// Pen routing state.
#[derive(Debug, Default)]
pub(in crate::shell) struct PenState {
	/// The tool in proximity, if any.
	active: Option<PenTool>,
	/// Whether the tip is down.
	contact: bool,
	/// The stylus buttons reported pressed to the client.
	buttons: u32,
}

/// The descriptor of the virtual tablet.
fn tablet_descriptor() -> TabletDescriptor {
	TabletDescriptor {
		name: "ferriscope pen".to_owned(),
		usb_id: None,
		syspath: None,
	}
}

/// The descriptor of a tool. The serial keeps the two tools distinct.
fn tool_descriptor(tool: PenTool) -> TabletToolDescriptor {
	let (tool_type, hardware_serial) = match tool {
		PenTool::Pen => (TabletToolType::Pen, 1),
		PenTool::Eraser => (TabletToolType::Eraser, 2),
	};
	TabletToolDescriptor {
		tool_type,
		hardware_serial,
		hardware_id_wacom: 0,
		capabilities: TabletToolCapabilities::PRESSURE
			| TabletToolCapabilities::DISTANCE
			| TabletToolCapabilities::TILT,
	}
}

/// The `(button, pressed)` changes between two button masks, in bit order.
fn button_changes(old: u32, new: u32) -> Vec<(u32, bool)> {
	STYLUS_BUTTONS
		.iter()
		.filter(|(mask, _)| old & mask != new & mask)
		.map(|(mask, code)| (*code, new & mask != 0))
		.collect()
}

/// The axis values a sample reports: touching reports its pressure (and a zero
/// distance), hovering its distance (and a zero pressure), so a contact change
/// never leaves the client with the other state's stale value.
fn axis_values(event: &PenEvent) -> (Option<f64>, Option<f64>) {
	let clamp = |value: f64| value.clamp(0.0, 1.0);
	let (pressure, distance) = (event.pressure.map(clamp), event.distance.map(clamp));
	if event.contact {
		(pressure, distance.or(Some(0.0)))
	} else {
		(pressure.or(Some(0.0)), distance)
	}
}

impl FerriscopeState {
	/// Add the virtual tablet and both tools to the seat, and advertise them to
	/// the clients that bind the tablet manager.
	///
	/// Smithay only exposes a tablet or tool to Wayland clients when it is added
	/// through the `add_wp_*` calls; the plain `add_tablet`/`add_tool` create
	/// handles no client ever hears about.
	pub(in crate::shell) fn add_virtual_tablet(&mut self) {
		let tablet_seat = self.seat.tablet_seat();
		let display = self.display_handle.clone();
		tablet_seat.add_wp_tablet(&display, &tablet_descriptor());
		for tool in [PenTool::Pen, PenTool::Eraser] {
			tablet_seat.add_wp_tool(self, &display, &tool_descriptor(tool));
		}
	}

	fn pen_tool(&self, tool: PenTool) -> Option<TabletToolHandle<Self>> {
		self.seat.tablet_seat().get_tool(&tool_descriptor(tool))
	}

	/// A pen sample.
	pub(in crate::shell) fn pen_input(&mut self, event: PenEvent) {
		let time = InputTime::now();
		// A different tool replaces the one in proximity, like a real tablet.
		if self.pen.active.is_some_and(|active| active != event.tool) {
			self.pen_leave();
		}
		let position = Point::from((event.x, event.y));
		let (focus, location) = self.pointer_under(position);
		let Some(tool) = self.pen_tool(event.tool) else {
			return;
		};
		let Some(tablet) = self.seat.tablet_seat().get_tablet(&tablet_descriptor()) else {
			return;
		};

		if self.pen.active.is_none() {
			// Nothing under the pen: it has nothing to enter.
			if focus.is_none() {
				return;
			}
			tool.proximity_in(
				self,
				focus.clone(),
				tablet,
				&ProximityInEvent {
					location,
					axis: None,
					serial: SERIAL_COUNTER.next_serial(),
					time,
				},
			);
			self.pen.active = Some(event.tool);
		}

		// The visible cursor follows the pen, as it did in Moonshine's former
		// compositor; the `wl_pointer` is left alone.
		self.cursor.location = position;
		self.note_pointer_moved();

		let (pressure, distance) = axis_values(&event);
		let mut axis = AxisFrame::new();
		if let Some(pressure) = pressure {
			axis = axis.pressure(pressure);
		}
		if let Some(distance) = distance {
			axis = axis.distance(distance);
		}
		if let Some((x, y)) = event.tilt {
			axis = axis.tilt(x.clamp(-90.0, 90.0), y.clamp(-90.0, 90.0));
		}
		tool.axis(self, axis);
		tool.motion(
			self,
			focus,
			&MotionEvent {
				location,
				serial: SERIAL_COUNTER.next_serial(),
				time,
			},
		);
		self.sync_pen_buttons(&tool, event.buttons, time);
		if event.contact != self.pen.contact {
			self.pen.contact = event.contact;
			if event.contact {
				tool.down(
					self,
					&DownEvent {
						serial: SERIAL_COUNTER.next_serial(),
						time,
					},
				);
			} else {
				tool.up(
					self,
					&UpEvent {
						serial: SERIAL_COUNTER.next_serial(),
						time,
					},
				);
			}
		}
		tool.frame(self, time);
	}

	/// The stylus buttons changed without the pen moving.
	pub(in crate::shell) fn pen_buttons(&mut self, buttons: u32) {
		let Some(tool) = self.pen.active.and_then(|active| self.pen_tool(active)) else {
			return;
		};
		let time = InputTime::now();
		self.sync_pen_buttons(&tool, buttons, time);
		tool.frame(self, time);
	}

	/// Press and release the stylus buttons that differ from what the client has.
	fn sync_pen_buttons(&mut self, tool: &TabletToolHandle<Self>, buttons: u32, time: InputTime) {
		for (button, pressed) in button_changes(self.pen.buttons, buttons) {
			tool.button(
				self,
				&ButtonEvent {
					serial: SERIAL_COUNTER.next_serial(),
					button,
					state: if pressed {
						ButtonState::Pressed
					} else {
						ButtonState::Released
					},
					time,
				},
			);
		}
		self.pen.buttons = buttons;
	}

	/// The pen left proximity (hover leave or cancel).
	pub(in crate::shell) fn pen_leave(&mut self) {
		let state = std::mem::take(&mut self.pen);
		let Some(tool) = state.active.and_then(|active| self.pen_tool(active)) else {
			return;
		};
		// Smithay releases pressed buttons and lifts the tip before leaving.
		tool.proximity_out(
			self,
			&ProximityOutEvent {
				serial: SERIAL_COUNTER.next_serial(),
				time: InputTime::now(),
			},
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample(contact: bool, pressure: Option<f64>, distance: Option<f64>) -> PenEvent {
		PenEvent {
			tool: PenTool::Pen,
			x: 0.0,
			y: 0.0,
			contact,
			pressure,
			distance,
			tilt: None,
			buttons: 0,
		}
	}

	#[test]
	fn button_bits_map_to_stylus_codes() {
		assert_eq!(button_changes(0, 0b001), vec![(0x14b, true)]);
		assert_eq!(button_changes(0b011, 0b010), vec![(0x14b, false)]);
		assert_eq!(button_changes(0, 0b110), vec![(0x14c, true), (0x149, true)]);
		assert!(button_changes(0b101, 0b101).is_empty());
		// Bits beyond the three stylus buttons are ignored.
		assert!(button_changes(0, 0b1000).is_empty());
	}

	#[test]
	fn hover_reports_distance_and_zero_pressure() {
		assert_eq!(axis_values(&sample(false, None, Some(0.4))), (Some(0.0), Some(0.4)));
	}

	#[test]
	fn contact_reports_pressure_and_zero_distance() {
		assert_eq!(axis_values(&sample(true, Some(0.7), None)), (Some(0.7), Some(0.0)));
	}

	#[test]
	fn axes_are_clamped() {
		assert_eq!(axis_values(&sample(true, Some(3.0), None)).0, Some(1.0));
		assert_eq!(axis_values(&sample(false, None, Some(-1.0))).1, Some(0.0));
	}

	#[test]
	fn contact_without_pressure_leaves_it_unreported() {
		assert_eq!(axis_values(&sample(true, None, None)), (None, Some(0.0)));
	}

	#[test]
	fn tools_have_distinct_descriptors() {
		assert_ne!(tool_descriptor(PenTool::Pen), tool_descriptor(PenTool::Eraser));
	}
}
