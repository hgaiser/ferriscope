//! Client-level end-to-end tests for pen (graphics tablet) injection.
//!
//! A native `wayland-client` test client (the one in [`support`]) binds
//! `zwp_tablet_manager_v2`; the test sends `pen*` lines over the control socket
//! and asserts what the client sees. Pen input is a ferriscope extension
//! (gamescope has no tablet support).
//!
//! The tests are `#[ignore]`d: they need a render node. Run them with
//!
//! ```text
//! timeout 300 cargo test -p ferriscope-core --test pen_input -- --ignored --test-threads=1 --nocapture
//! ```

use std::time::Duration;

use support::{PenLogEvent, Started, Target};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::Type;

mod support;

/// `BTN_STYLUS`.
const BTN_STYLUS: u32 = 0x14b;
/// `BTN_STYLUS2`.
const BTN_STYLUS2: u32 = 0x14c;
/// Budget for an event to surface.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

fn near(a: f64, b: f64) -> bool {
	(a - b).abs() < 1.0
}

fn start(buffer_size: Option<(i32, i32)>) -> Started {
	let options = support::ClientOptions {
		buffer_size,
		..support::ClientOptions::default()
	};
	support::start_configured(options, |_| {})
}

fn frame_count(log: &support::Log) -> usize {
	log.pen.iter().filter(|event| **event == PenLogEvent::Frame).count()
}

/// Send a pen line and wait until the client has seen its `frame`.
fn send_and_frame(started: &mut Started, line: &str) {
	let before = frame_count(&started.client.snapshot());
	support::send_line(&mut started.control, line);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| frame_count(log) > before),
		"no frame after {line:?}"
	);
}

fn last_motion(started: &Started) -> Option<(f64, f64)> {
	started
		.client
		.snapshot()
		.pen
		.iter()
		.rev()
		.find_map(|event| match event {
			PenLogEvent::Motion { x, y } => Some((*x, *y)),
			_ => None,
		})
}

/// The tablet and both tools are advertised to a client that binds the manager.
#[test]
#[ignore = "requires GPU; native Wayland client for the tablet protocol"]
fn tablet_and_tools_are_advertised() {
	let mut started = start(None);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.tablet.tools.len() >= 2),
		"tools were not advertised: {:?}",
		started.client.snapshot().tablet
	);
	let info = started.client.snapshot().tablet;
	assert_eq!(info.tablets.len(), 1, "tablets: {:?}", info.tablets);
	let types: Vec<Type> = info.tools.iter().map(|(_, tool_type)| *tool_type).collect();
	assert!(types.contains(&Type::Pen) && types.contains(&Type::Eraser), "{types:?}");

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Hover, contact, buttons, tool switch and leave reach the surface under the pen.
#[test]
#[ignore = "requires GPU; native Wayland client for the tablet protocol"]
fn pen_lifecycle_reaches_the_client() {
	let mut started = start(None);
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// Hover enters proximity on the surface and reports the distance.
	send_and_frame(&mut started, "pen pen hover 100 80 distance=0.5 tilt=10,-20\n");
	let log = started.client.snapshot();
	assert!(
		log.pen.contains(&PenLogEvent::ProximityIn {
			surface: parent,
			tool: Type::Pen
		}),
		"no proximity_in: {:?}",
		log.pen
	);
	let (x, y) = last_motion(&started).expect("motion");
	assert!(near(x, 100.0) && near(y, 80.0), "motion at ({x}, {y})");
	assert!(log.pen.contains(&PenLogEvent::Distance(32767)), "{:?}", log.pen);
	assert!(
		log.pen.contains(&PenLogEvent::Tilt { x: 10.0, y: -20.0 }),
		"{:?}",
		log.pen
	);
	assert!(!log.pen.contains(&PenLogEvent::Down), "hover must not send down");

	// Contact: pressure, tip down at the new position, a stylus button.
	send_and_frame(&mut started, "pen pen contact 120 90 pressure=0.5 buttons=1\n");
	let log = started.client.snapshot();
	assert!(log.pen.contains(&PenLogEvent::Pressure(32767)), "{:?}", log.pen);
	assert!(log.pen.contains(&PenLogEvent::Down), "{:?}", log.pen);
	assert!(
		log.pen.contains(&PenLogEvent::Button {
			button: BTN_STYLUS,
			pressed: true
		}),
		"{:?}",
		log.pen
	);
	let (x, y) = last_motion(&started).expect("motion");
	assert!(near(x, 120.0) && near(y, 90.0), "motion at ({x}, {y})");

	// A second contact sample moves without another down.
	send_and_frame(&mut started, "pen pen contact 130 95 pressure=0.75 buttons=3\n");
	let log = started.client.snapshot();
	assert_eq!(log.pen.iter().filter(|event| **event == PenLogEvent::Down).count(), 1);
	assert!(
		log.pen.contains(&PenLogEvent::Button {
			button: BTN_STYLUS2,
			pressed: true
		}),
		"{:?}",
		log.pen
	);

	// A button-only change releases the first button.
	let frames_before = frame_count(&log);
	support::send_line(&mut started.control, "pen-buttons 2\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pen.contains(
			&PenLogEvent::Button {
				button: BTN_STYLUS,
				pressed: false
			}
		)),
		"no stylus release"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| frame_count(log) > frames_before),
		"no frame after pen-buttons"
	);

	// Lift the tip.
	send_and_frame(&mut started, "pen pen hover 130 95 distance=0.1\n");
	assert!(started.client.snapshot().pen.contains(&PenLogEvent::Up));

	// Switching to the eraser leaves the pen's proximity and enters the eraser's.
	let mark = started.client.snapshot().pen.len();
	send_and_frame(&mut started, "pen eraser hover 40 30 distance=0.2\n");
	let log = started.client.snapshot();
	let after = &log.pen[mark..];
	let out = after
		.iter()
		.position(|event| *event == PenLogEvent::ProximityOut)
		.expect("proximity_out for the pen");
	let enter = after
		.iter()
		.position(|event| {
			*event
				== PenLogEvent::ProximityIn {
					surface: parent,
					tool: Type::Eraser,
				}
		})
		.expect("proximity_in for the eraser");
	assert!(out < enter, "pen must leave before the eraser enters: {after:?}");

	// Leave ends proximity.
	let mark = started.client.snapshot().pen.len();
	support::send_line(&mut started.control, "pen-leave\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pen[mark..]
			.contains(&PenLogEvent::ProximityOut)),
		"no proximity_out after pen-leave"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Leaving while touching lifts the tip and releases the held buttons first.
#[test]
#[ignore = "requires GPU; native Wayland client for the tablet protocol"]
fn leave_while_touching_lifts_and_releases() {
	let mut started = start(None);
	send_and_frame(&mut started, "pen pen contact 100 80 pressure=0.5 buttons=1\n");
	let mark = started.client.snapshot().pen.len();
	support::send_line(&mut started.control, "pen-leave\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pen[mark..]
			.contains(&PenLogEvent::ProximityOut)),
		"no proximity_out"
	);
	let log = started.client.snapshot();
	let after = &log.pen[mark..];
	assert!(after.contains(&PenLogEvent::Up), "{after:?}");
	assert!(
		after.contains(&PenLogEvent::Button {
			button: BTN_STYLUS,
			pressed: false
		}),
		"{after:?}"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Pen coordinates take the pointer's output-to-scene mapping: a window scaled
/// to fill the output sees surface-local positions.
#[test]
#[ignore = "requires GPU; native Wayland client for the tablet protocol"]
fn pen_is_mapped_through_the_output_fill() {
	// A 160x120 window on the 320x240 output is scaled by 2.
	let mut started = start(Some((160, 120)));
	send_and_frame(&mut started, "pen pen hover 200 100 distance=0.5\n");
	let (x, y) = last_motion(&started).expect("motion");
	assert!(
		near(x, 100.0) && near(y, 50.0),
		"motion at ({x}, {y}), expected (100, 50)"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}
