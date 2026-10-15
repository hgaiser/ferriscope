//! Native Wayland `xdg_popup` (menus, dropdowns, combo boxes).
//!
//! A popup must get its first configure, or it never maps; it must be kept on
//! the output; and an explicit grab must take the keyboard.
//!
//! A click on the client's own window does not dismiss its popup: the click
//! goes to the window and the client closes its popup itself (xdg-shell; GTK
//! does). Only a click on another client's surface ends the grab.
//!
//! `#[ignore]`d: needs a render node. Run with:
//!
//! ```text
//! cargo test -p ferriscope-core --test popup -- --ignored --test-threads=1
//! ```

mod support;

use std::time::Duration;

use support::{CommandKind, Target};

/// Output size (`support`'s compositor config).
const OUTPUT: (i32, i32) = (320, 240);
/// Budget for the window to map and receive pointer focus.
const POINTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a popup event to arrive.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Move the pointer to `(x, y)` and wait for it to enter `surface`.
fn point_at(started: &mut support::Started, (x, y): (i32, i32), surface: u32) {
	support::send_line(&mut started.control, &format!("motion-abs {x} {y}\n"));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&surface)),
		"the pointer did not enter surface {surface} at ({x}, {y})"
	);
}

fn click(started: &mut support::Started) {
	support::send_line(&mut started.control, "button 272 1\n");
	support::send_line(&mut started.control, "button 272 0\n");
}

#[test]
#[ignore = "requires GPU; native Wayland popup"]
fn a_popup_is_configured_and_receives_the_pointer() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	point_at(&mut started, (10, 10), parent);

	assert!(started.client.command(CommandKind::CreatePopup {
		x: 40,
		y: 40,
		width: 100,
		height: 60,
		grab: false,
	}));
	let popup = started.client.surface_id(Target::Popup).expect("popup surface id");
	assert_eq!(
		started.client.snapshot().popup_configure,
		Some((40, 40, 100, 60)),
		"the popup was not configured where its positioner asked"
	);

	// Over the popup, the pointer is the popup's, not the window's.
	point_at(&mut started, (80, 70), popup);
	assert!(started.guard.stop(), "compositor did not stop cleanly");
}

#[test]
#[ignore = "requires GPU; native Wayland popup"]
fn a_popup_at_the_edge_of_the_output_is_kept_on_it() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	point_at(&mut started, (10, 10), parent);

	// Opening down-right from near the bottom-right corner would overflow; the
	// popup flips up-left instead.
	assert!(started.client.command(CommandKind::CreatePopup {
		x: OUTPUT.0 - 20,
		y: OUTPUT.1 - 20,
		width: 100,
		height: 60,
		grab: false,
	}));
	let (x, y, width, height) = started.client.snapshot().popup_configure.expect("popup configured");
	assert!(
		x >= 0 && y >= 0 && x + width <= OUTPUT.0 && y + height <= OUTPUT.1,
		"the popup ({x}, {y}, {width}x{height}) is not on the {OUTPUT:?} output"
	);
	assert!(started.guard.stop(), "compositor did not stop cleanly");
}

#[test]
#[ignore = "requires GPU; native Wayland popup"]
fn a_grabbing_popup_takes_the_keyboard_and_a_click_on_its_window_reaches_the_window() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	point_at(&mut started, (10, 10), parent);
	// The click whose serial the grab uses, like a menu button.
	click(&mut started);
	assert!(support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
		.pointer_button
		.len() >= 2));

	assert!(started.client.command(CommandKind::CreatePopup {
		x: 40,
		y: 40,
		width: 100,
		height: 60,
		grab: true,
	}));
	let popup = started.client.surface_id(Target::Popup).expect("popup surface id");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&popup)),
		"the grabbing popup did not take the keyboard"
	);

	// Over the popup the pointer is the popup's; back on the window, the
	// window gets the click, and the popup stays until the client closes it.
	point_at(&mut started, (80, 70), popup);
	let enters = started.client.snapshot().pointer_enter.len();
	support::send_line(&mut started.control, "motion-abs 250 200\n");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_enter[enters..]
			.contains(&parent)),
		"the pointer did not return to the window"
	);
	click(&mut started);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_button.len() >= 4),
		"the click on the window did not reach it"
	);
	assert_eq!(
		started.client.snapshot().popup_done,
		0,
		"the compositor dismissed the popup itself"
	);
	assert!(started.guard.stop(), "compositor did not stop cleanly");
}
