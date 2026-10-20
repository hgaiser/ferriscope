//! The `zwp_tablet_v2` side of the test client: it binds the tablet manager,
//! asks for the seat's tablet seat and records every tablet and tool event.

use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child, protocol::wl_surface};
use wayland_protocols::wp::tablet::zv2::client::{
	zwp_tablet_manager_v2, zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2, zwp_tablet_v2,
};

use super::App;

/// A client-observed `zwp_tablet_tool_v2` event. Axis values keep the
/// protocol's raw units (`pressure` and `distance` are `0..=65535`).
#[derive(Debug, Clone, PartialEq)]
pub enum PenLogEvent {
	/// `proximity_in` on the surface with this protocol id.
	ProximityIn {
		surface: u32,
		tool: zwp_tablet_tool_v2::Type,
	},
	ProximityOut,
	Down,
	Up,
	Motion {
		x: f64,
		y: f64,
	},
	Pressure(u32),
	Distance(u32),
	Tilt {
		x: f64,
		y: f64,
	},
	Button {
		button: u32,
		pressed: bool,
	},
	Frame,
}

/// What the client learned about the tablets and tools advertised to it.
#[derive(Debug, Clone, Default)]
pub struct TabletInfo {
	/// Names from `zwp_tablet_v2.name`.
	pub tablets: Vec<String>,
	/// Tool object ids and their types from `zwp_tablet_tool_v2.type`.
	pub tools: Vec<(u32, zwp_tablet_tool_v2::Type)>,
}

/// Bind the manager and create the tablet seat for `seat`.
pub(super) fn bind_tablet_seat(
	manager: &zwp_tablet_manager_v2::ZwpTabletManagerV2,
	seat: &wayland_client::protocol::wl_seat::WlSeat,
	qh: &QueueHandle<App>,
) -> zwp_tablet_seat_v2::ZwpTabletSeatV2 {
	manager.get_tablet_seat(seat, qh, ())
}

impl Dispatch<zwp_tablet_manager_v2::ZwpTabletManagerV2, ()> for App {
	fn event(
		_: &mut Self,
		_: &zwp_tablet_manager_v2::ZwpTabletManagerV2,
		_: zwp_tablet_manager_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for App {
	fn event(
		_: &mut Self,
		_: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
		_: zwp_tablet_seat_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}

	event_created_child!(App, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
		zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
		zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
		zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, ()),
	]);
}

impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for App {
	fn event(
		state: &mut Self,
		_: &zwp_tablet_v2::ZwpTabletV2,
		event: zwp_tablet_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let zwp_tablet_v2::Event::Name { name } = event {
			state.log.lock().expect("log lock").tablet.tablets.push(name);
		}
	}
}

impl Dispatch<zwp_tablet_pad_v2::ZwpTabletPadV2, ()> for App {
	fn event(
		_: &mut Self,
		_: &zwp_tablet_pad_v2::ZwpTabletPadV2,
		_: zwp_tablet_pad_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}

	event_created_child!(App, zwp_tablet_pad_v2::ZwpTabletPadV2, [
		zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()),
	]);
}

impl Dispatch<wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()> for App {
	fn event(
		_: &mut Self,
		_: &wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
		_: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for App {
	fn event(
		state: &mut Self,
		tool: &zwp_tablet_tool_v2::ZwpTabletToolV2,
		event: zwp_tablet_tool_v2::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		use zwp_tablet_tool_v2::Event;
		let mut log = state.log.lock().expect("log lock");
		let pen = match event {
			Event::Type {
				tool_type: WEnum::Value(tool_type),
			} => {
				log.tablet.tools.push((tool.id().protocol_id(), tool_type));
				return;
			},
			Event::ProximityIn { surface, .. } => {
				let surface: &wl_surface::WlSurface = &surface;
				let id = tool.id().protocol_id();
				PenLogEvent::ProximityIn {
					surface: surface.id().protocol_id(),
					tool: log
						.tablet
						.tools
						.iter()
						.find_map(|(tool_id, tool_type)| (*tool_id == id).then_some(*tool_type))
						.expect("proximity_in on a tool with no type"),
				}
			},
			Event::ProximityOut => PenLogEvent::ProximityOut,
			Event::Down { .. } => PenLogEvent::Down,
			Event::Up => PenLogEvent::Up,
			Event::Motion { x, y } => PenLogEvent::Motion { x, y },
			Event::Pressure { pressure } => PenLogEvent::Pressure(pressure),
			Event::Distance { distance } => PenLogEvent::Distance(distance),
			Event::Tilt { tilt_x, tilt_y } => PenLogEvent::Tilt { x: tilt_x, y: tilt_y },
			Event::Button {
				button,
				state: WEnum::Value(button_state),
				..
			} => PenLogEvent::Button {
				button,
				pressed: button_state == zwp_tablet_tool_v2::ButtonState::Pressed,
			},
			Event::Frame { .. } => PenLogEvent::Frame,
			_ => return,
		};
		log.pen.push(pen);
	}
}
