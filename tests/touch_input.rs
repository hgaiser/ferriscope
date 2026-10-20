//! Client-level end-to-end tests for touch injection.
//!
//! A native `wayland-client` test client (the one in [`support`]) binds
//! `wl_pointer` and `wl_touch`; the test sends `touch-*` lines over the control
//! socket and asserts what the client sees in each click mode. gamescope's
//! default mode (`Left`) turns a touch into a pointer warp plus a left-button
//! hold; `Passthrough` delivers `wl_touch`; `Disabled` delivers nothing.
//!
//! The tests are `#[ignore]`d: they need a render node. Run them with
//!
//! ```text
//! timeout 300 cargo test -p ferriscope --test touch_input -- --ignored --test-threads=1 --nocapture
//! ```

use std::time::Duration;

use ferriscope::TouchClickMode;
use support::{Started, Target, TouchEvent};

mod support;

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;
/// Budget for an event to surface.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait before concluding an event is not coming.
const QUIET: Duration = Duration::from_millis(600);

fn near(a: f64, b: f64) -> bool {
	(a - b).abs() < 1.0
}

fn start(mode: TouchClickMode, buffer_size: Option<(i32, i32)>) -> Started {
	let options = support::ClientOptions {
		buffer_size,
		..support::ClientOptions::default()
	};
	support::start_configured(options, |config| config.seat.touch_click_mode = mode)
}

/// In gamescope's default mode a touch warps the pointer and holds the left
/// button until the finger lifts; no `wl_touch` event is sent.
#[test]
#[ignore = "requires GPU; native Wayland client for touch"]
fn left_mode_touch_is_a_pointer_click() {
	let mut started = start(TouchClickMode::Left, None);
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	support::send_line(&mut started.control, "touch-down 0 100 80\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_button
			.contains(&(BTN_LEFT, true))),
		"no left press after touch-down"
	);
	let log = started.client.snapshot();
	assert!(log.pointer_enter.contains(&parent), "pointer did not enter the window");
	let (x, y) = *log.pointer_motion.last().expect("pointer position");
	assert!(
		near(x, 100.0) && near(y, 80.0),
		"pointer at ({x}, {y}), expected (100, 80)"
	);

	support::send_line(&mut started.control, "touch-motion 0 140 90\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_motion
			.last()
			.is_some_and(|(x, y)| near(*x, 140.0) && near(*y, 90.0))),
		"touch-motion did not move the pointer"
	);

	support::send_line(&mut started.control, "touch-up 0\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_button
			.contains(&(BTN_LEFT, false))),
		"no left release after touch-up"
	);
	support::send_line(&mut started.control, "touch-frame\n");
	assert!(
		started.client.snapshot().touch.is_empty(),
		"wl_touch events in a click mode: {:?}",
		started.client.snapshot().touch
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Passthrough delivers the touch point to the surface under it, with the same
/// coordinates the pointer would have, and no pointer button.
#[test]
#[ignore = "requires GPU; native Wayland client for touch"]
fn passthrough_mode_delivers_wl_touch() {
	let mut started = start(TouchClickMode::Passthrough, None);

	support::send_line(&mut started.control, "touch-down 3 100 80\n");
	support::send_line(&mut started.control, "touch-frame\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.touch
			.contains(&TouchEvent::Frame)),
		"no wl_touch frame after touch-down"
	);
	let log = started.client.snapshot();
	let down = log
		.touch
		.iter()
		.find_map(|event| match event {
			TouchEvent::Down { id, x, y } => Some((*id, *x, *y)),
			_ => None,
		})
		.expect("wl_touch.down");
	assert_eq!(down.0, 3);
	assert!(near(down.1, 100.0) && near(down.2, 80.0), "down at {down:?}");

	support::send_line(&mut started.control, "touch-motion 3 150 95\n");
	support::send_line(&mut started.control, "touch-frame\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.touch.iter().any(
			|event| matches!(event, TouchEvent::Motion { id: 3, x, y } if near(*x, 150.0) && near(*y, 95.0))
		)),
		"no wl_touch.motion"
	);

	support::send_line(&mut started.control, "touch-up 3\n");
	support::send_line(&mut started.control, "touch-frame\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.touch
			.contains(&TouchEvent::Up { id: 3 })),
		"no wl_touch.up"
	);
	assert!(
		started.client.snapshot().pointer_button.is_empty(),
		"passthrough must not press a pointer button"
	);

	// A cancel drops the touch points.
	support::send_line(&mut started.control, "touch-down 4 20 20\n");
	support::send_line(&mut started.control, "touch-frame\n");
	// Cancel after the frame, as a host sends it: the live point is cancelled.
	support::send_line(&mut started.control, "touch-cancel\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.touch
			.contains(&TouchEvent::Cancel)),
		"no wl_touch.cancel"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Touch coordinates take the pointer's output-to-scene mapping: a window
/// scaled to fill the output sees surface-local touch positions.
#[test]
#[ignore = "requires GPU; native Wayland client for touch"]
fn passthrough_touch_is_mapped_through_the_output_fill() {
	// A 160x120 window on the 320x240 output is scaled by 2.
	let mut started = start(TouchClickMode::Passthrough, Some((160, 120)));

	support::send_line(&mut started.control, "touch-down 0 200 100\n");
	support::send_line(&mut started.control, "touch-frame\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.touch
			.iter()
			.any(|event| matches!(event, TouchEvent::Down { .. }))),
		"no wl_touch.down"
	);
	let log = started.client.snapshot();
	let Some(TouchEvent::Down { x, y, .. }) = log.touch.iter().find(|event| matches!(event, TouchEvent::Down { .. }))
	else {
		unreachable!("checked above")
	};
	assert!(
		near(*x, 100.0) && near(*y, 50.0),
		"down at ({x}, {y}), expected surface-local (100, 50)"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Disabled ignores touch altogether.
#[test]
#[ignore = "requires GPU; native Wayland client for touch"]
fn disabled_mode_ignores_touch() {
	let mut started = start(TouchClickMode::Disabled, None);

	support::send_line(&mut started.control, "touch-down 0 100 80\n");
	support::send_line(&mut started.control, "touch-frame\n");
	support::send_line(&mut started.control, "touch-up 0\n");
	std::thread::sleep(QUIET);
	let log = started.client.snapshot();
	assert!(log.pointer_button.is_empty(), "button in disabled mode");
	assert!(
		log.touch.iter().all(|event| *event == TouchEvent::Frame),
		"touch events in disabled mode: {:?}",
		log.touch
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Trackpad holds the left button without warping; a touch motion then moves
/// the pointer by the distance from the pointer to the touch.
#[test]
#[ignore = "requires GPU; native Wayland client for touch"]
fn trackpad_mode_presses_without_warping() {
	let mut started = start(TouchClickMode::Trackpad, None);
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// Seed the pointer so it has focus and a known position.
	support::send_line(&mut started.control, "motion-abs 50 50\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus"
	);
	let before = started.client.snapshot().pointer_motion.len();

	support::send_line(&mut started.control, "touch-down 0 200 150\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_button
			.contains(&(BTN_LEFT, true))),
		"no left press after touch-down"
	);
	assert_eq!(
		started.client.snapshot().pointer_motion.len(),
		before,
		"trackpad touch-down must not move the pointer"
	);

	support::send_line(&mut started.control, "touch-motion 0 200 150\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_motion
			.last()
			.is_some_and(|(x, y)| near(*x, 200.0) && near(*y, 150.0))),
		"touch-motion did not move the pointer to the touch distance"
	);

	support::send_line(&mut started.control, "touch-up 0\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.pointer_button
			.contains(&(BTN_LEFT, false))),
		"no left release after touch-up"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}
