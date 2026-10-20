//! Client-level end-to-end test for pointer-axis (scroll) injection.
//!
//! A native `wayland-client` test client (the one in [`support`]) binds
//! `wl_pointer`; the test first seeds pointer focus with `motion-abs` (a
//! `wl_pointer.axis` only reaches a focused surface), then sends `axis v` /
//! `axis h` lines over the control socket and asserts the client sees
//! `wl_pointer.axis`, the wheel source, the discrete step and `axis_stop`.
//!
//! It is `#[ignore]`d: it needs a render node. Run it alone:
//!
//! ```text
//! timeout 120 cargo test -p ferriscope --test scroll_reaches_app -- --ignored --exact scroll_reaches_wayland_client --nocapture
//! ```

use std::time::Duration;

use wayland_client::protocol::wl_pointer::{Axis, AxisSource};

use support::Target;

mod support;

/// An absolute pointer position inside the 320x240 window, to seed focus.
const ABS: (f64, f64) = (60.0, 60.0);
/// Budget for the window to map and receive pointer focus.
const POINTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the axis event to surface.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// One wheel notch up and one notch right reach the client as `wl_pointer.axis`.
///
/// The seat carries notches with the Wayland sign convention; the injector
/// expands a notch to `1` continuous unit and `120` `axis_value120`, matching
/// gamescope. The
/// client binds `wl_seat` v5, so smithay folds the v120 into `axis_discrete`
/// (`120` per step), which is what this asserts.
#[test]
#[ignore = "requires GPU; native Wayland client for scroll axis"]
fn scroll_reaches_wayland_client() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// A `wl_pointer.axis` goes only to the focused surface; seed focus first,
	// exactly as a streaming host's absolute-motion input does.
	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", ABS.0, ABS.1));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);

	// Vertical, one notch up: seat sign is positive-down, so `-1`.
	support::send_line(&mut started.control, "axis v -1\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| !log.pointer_axis.is_empty()),
		"no wl_pointer.axis after `axis v -1` within {EVENT_TIMEOUT:?}"
	);
	let log = started.client.snapshot();
	let (axis, value) = *log.pointer_axis.last().expect("vertical axis event");
	assert_eq!(axis, Axis::VerticalScroll, "wrong axis for `axis v`");
	assert!(
		(value - (-1.0)).abs() < 1e-6,
		"continuous vertical value was {value}, expected -1 (1 unit per notch, gamescope parity)"
	);
	assert!(
		log.pointer_axis_source.contains(&AxisSource::Wheel),
		"axis_source was {:?}, expected Wheel",
		log.pointer_axis_source
	);
	assert!(
		log.pointer_axis_stop.contains(&Axis::VerticalScroll),
		"no axis_stop for the vertical axis"
	);
	// A v5 pointer gets `axis_discrete`; a v8+ pointer gets `axis_value120`.
	assert!(
		log.pointer_axis_discrete.contains(&(Axis::VerticalScroll, -1))
			|| log.pointer_axis_value120.contains(&(Axis::VerticalScroll, -120)),
		"no discrete step for the vertical notch; discrete={:?} value120={:?}",
		log.pointer_axis_discrete,
		log.pointer_axis_value120
	);

	// Horizontal, one notch right: positive in both conventions.
	let before = started.client.snapshot().pointer_axis.len();
	support::send_line(&mut started.control, "axis h 1\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_axis.len() > before),
		"no wl_pointer.axis after `axis h 1` within {EVENT_TIMEOUT:?}"
	);
	let log = started.client.snapshot();
	let (axis, value) = *log.pointer_axis.last().expect("horizontal axis event");
	assert_eq!(axis, Axis::HorizontalScroll, "wrong axis for `axis h`");
	assert!(
		(value - 1.0).abs() < 1e-6,
		"continuous horizontal value was {value}, expected 1"
	);
	assert!(
		log.pointer_axis_stop.contains(&Axis::HorizontalScroll),
		"no axis_stop for the horizontal axis"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}
