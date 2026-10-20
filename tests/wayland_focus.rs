//! GPU integration tests for the Wayland focus/capture behaviour.
//!
//! The X11 half of the focus contract is covered by `focus_policy.rs`. These
//! tests are the native-Wayland counterpart and close the gap that the spec
//! lists as untested: pointer lock plus `zwp_relative_pointer`,
//! `xdg_activation_v1`, a parented transient taking keyboard focus while the
//! window keeps the fullscreen role, and the dangling-focus rule
//! when the focused toplevel is destroyed. They are also the
//! first tests that drive the compositor over its own Wayland socket instead of
//! through Xwayland.
//!
//! Each test is `#[ignore]`d: it needs a render node. Run one alone, under a hard
//! timeout:
//!
//! ```text
//! timeout 120 cargo test -p ferriscope --test wayland_focus -- --ignored --exact wayland_relative_pointer_lock --nocapture
//! ```
//!
//! The client lives in [`support`]: a minimal raw `wayland-client` /
//! `wayland-protocols` program (no `smithay-client-toolkit`) that binds the
//! globals, maps one or two `xdg_toplevel`s backed by a real `wl_shm` buffer,
//! and records the events the assertions need into a shared log. The compositor
//! is launched as an in-process library with a control socket, so the tests
//! inject `motion-abs` / `motion-rel` exactly as an embedding host would.

use std::{thread, time::Duration};

use support::{CommandKind, Target};

mod support;

/// An absolute pointer position inside the window, used to seed focus.
const ABS_A: (f64, f64) = (60.0, 60.0);
/// A different absolute position, used to prove a locked pointer still receives it.
const ABS_B: (f64, f64) = (240.0, 180.0);
/// A relative motion injected as mouselook.
const REL: (f64, f64) = (12.0, -7.0);
/// A surface-local confine region: a 50x50 block well inside the 320x240
/// window, so the region/surface intersection is the region itself.
const CONFINE_REGION: (i32, i32, i32, i32) = (100, 100, 50, 50);
/// A point inside the window but outside [`CONFINE_REGION`], used to establish
/// pointer focus before the constraint so activation must confine the pointer.
const CONFINE_FOCUS: (f64, f64) = (40.0, 40.0);
/// A point inside [`CONFINE_REGION`], which must pass through unchanged.
const CONFINE_INSIDE: (f64, f64) = (120.0, 120.0);
/// A point beyond the region's exclusive bottom-right edge; the region's last
/// inside coordinate is 149, so the nearest allowed point is `(149, 149)`.
const CONFINE_CLAMPED: (f64, f64) = (149.0, 149.0);
/// A point outside both the window and the region, used to force a clamp.
const CONFINE_FAR: (f64, f64) = (400.0, 300.0);
/// Budget for the window to map and receive pointer focus.
const POINTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for lock/capture events and relative motion to arrive.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Settle time before asserting that an event did *not* happen.
const SETTLE: Duration = Duration::from_millis(700);

/// Whether `log` recorded pointer motion within `epsilon` of `point`.
fn has_motion_near(log: &support::Log, point: (f64, f64), epsilon: f64) -> bool {
	log.pointer_motion
		.iter()
		.any(|(x, y)| (x - point.0).abs() <= epsilon && (y - point.1).abs() <= epsilon)
}

/// Relative pointer + pointer lock.
///
/// With the pointer over the window, a persistent lock is created and
/// `motion-rel` is injected: the client must
/// `zwp_relative_pointer.relative_motion` with the matching deltas. A lock
/// gates the **relative** cursor advance only: absolute motion still
/// reaches the client while locked,
/// and the client cursor image is unaffected.
#[test]
#[ignore = "requires GPU; native Wayland client for pointer lock"]
fn wayland_relative_pointer_lock() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// Establish pointer focus so the lock does not have to seed it.
	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", ABS_A.0, ABS_A.1));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::LockRelative),
		"the lock command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.locked >= 1),
		"no zwp_locked_pointer.locked event within {EVENT_TIMEOUT:?}"
	);

	let before = started.client.snapshot().relative_motion.len();
	support::send_line(&mut started.control, &format!("motion-rel {} {}\n", REL.0, REL.1));
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.relative_motion.len() > before),
		"no relative_motion event after motion-rel within {EVENT_TIMEOUT:?}"
	);
	let (dx, dy) = *started
		.client
		.snapshot()
		.relative_motion
		.last()
		.expect("relative motion");
	assert!(
		(dx - REL.0).abs() < 0.5 && (dy - REL.1).abs() < 0.5,
		"relative_motion deltas were ({dx}, {dy}), expected {REL:?}"
	);

	// Locked: absolute motion is still delivered (gamescope `wlserver_mousewarp`
	// ignores the constraint; only the relative-path advance is gated).
	let motions = started.client.snapshot().pointer_motion.len();
	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", ABS_B.0, ABS_B.1));
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_motion.len() > motions),
		"a locked pointer did not receive absolute motion to {ABS_B:?} within {EVENT_TIMEOUT:?}"
	);
	assert!(
		has_motion_near(&started.client.snapshot(), ABS_B, 2.0),
		"a locked pointer did not move to {ABS_B:?}"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A relative motion with **no** pointer constraint advances the absolute
/// pointer and notifies it, matching gamescope.
///
/// Suppressing the absolute notify for `capture == None` would keep an
/// unbounded cursor from drifting off the output, but it leaves a
/// relative-only client's drawn cursor stuck at its initial position; the
/// bound lives in `clamp_to_capture` (the focused window's rendered rect)
/// instead. Steam Big Picture is unconstrained and Moonlight's
/// default mode is relative, so Xwayland moved its own X pointer through
/// `zwp_relative_pointer` (clicks landed correctly) while the composited cursor
/// never moved and hover never updated. gamescope always advances its
/// surface-local cursor and calls `wlr_seat_pointer_notify_motion` on the same
/// event (`wlserver.cpp:3073-3085`); ferriscope must do the same.
#[test]
#[ignore = "requires GPU; native Wayland client for pointer capture"]
fn wayland_relative_motion_without_a_constraint_advances_the_absolute_pointer() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", ABS_A.0, ABS_A.1));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::RelativePointer),
		"the relative pointer command was not serviced"
	);
	let motions = started.client.snapshot().pointer_motion.len();
	let relatives = started.client.snapshot().relative_motion.len();
	support::send_line(&mut started.control, &format!("motion-rel {} {}\n", REL.0, REL.1));
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_motion.len() > motions),
		"an unconstrained relative event did not notify absolute motion within {EVENT_TIMEOUT:?}"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.relative_motion.len()
			> relatives),
		"no relative_motion event after motion-rel within {EVENT_TIMEOUT:?}"
	);

	// The absolute pointer advanced by the relative delta from its seeded point.
	let (x, y) = *started.client.snapshot().pointer_motion.last().expect("pointer motion");
	assert!(
		(x - (ABS_A.0 + REL.0)).abs() < 0.5 && (y - (ABS_A.1 + REL.1)).abs() < 0.5,
		"the unconstrained cursor was ({x}, {y}), expected ({}, {})",
		ABS_A.0 + REL.0,
		ABS_A.1 + REL.1
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Relative-only motion must establish pointer focus even with no prior
/// absolute event.
///
/// Moonlight's default mouse mode is relative (`LiSendMouseMoveEvent`), so the
/// real client may never send `MouseMoveAbsolute`. The unconstrained relative
/// path must still deliver `wl_pointer.motion`: without pointer focus
/// `wl_pointer.enter` is never sent, so the client's `zwp_relative_pointer` and
/// pointer buttons have no target and only keyboard input works. Since the unconstrained
/// advance notifies absolute motion on every relative event, the first event
/// establishes focus directly; the remaining case is a locked pointer, whose
/// relative-only path seeds focus without advancing.
///
/// This test injects the relative delta with **no** preceding `motion-abs`: the
/// window must get `wl_pointer.enter` and the `zwp_relative_pointer.relative_motion`
/// delta.
#[test]
#[ignore = "requires GPU; native Wayland client for pointer focus seeding"]
fn wayland_relative_motion_without_a_constraint_seeds_pointer_focus() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	assert!(
		started.client.command(CommandKind::RelativePointer),
		"the relative pointer command was not serviced"
	);

	// No motion-abs: the relative event is the first pointer input.
	assert!(
		started.client.snapshot().pointer_enter.is_empty(),
		"the window had pointer focus before any input"
	);
	let relatives = started.client.snapshot().relative_motion.len();
	support::send_line(&mut started.control, &format!("motion-rel {} {}\n", REL.0, REL.1));

	// The seed establishes focus: the client must receive wl_pointer.enter.
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"relative-only motion did not establish pointer focus within {POINTER_TIMEOUT:?}"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.relative_motion.len()
			> relatives),
		"no relative_motion event after motion-rel within {EVENT_TIMEOUT:?}"
	);

	// The delta itself still flows unscaled.
	let (dx, dy) = *started
		.client
		.snapshot()
		.relative_motion
		.last()
		.expect("relative motion");
	assert!(
		(dx - REL.0).abs() < 0.5 && (dy - REL.1).abs() < 0.5,
		"relative_motion deltas were ({dx}, {dy}), expected {REL:?}"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A locked pointer still receives absolute motion.
///
/// A lock only holds the cursor on the **relative** path. Absolute motion is
/// what a streaming client sends in absolute mouse mode; a game that locks the
/// pointer for mouse-look must still see it or look dies. After the lock, injecting
/// `motion-abs` to a different position must produce a new `wl_pointer.motion`
/// at that position, clamped to the focused window.
#[test]
#[ignore = "requires GPU; native Wayland client for pointer lock"]
fn wayland_locked_pointer_receives_absolute_motion() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", ABS_A.0, ABS_A.1));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| {
			log.pointer_enter.contains(&parent) && has_motion_near(log, ABS_A, 2.0)
		}),
		"the window did not receive pointer focus at {ABS_A:?} within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::LockRelative),
		"the lock command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.locked >= 1),
		"no zwp_locked_pointer.locked event within {EVENT_TIMEOUT:?}"
	);

	let motions = started.client.snapshot().pointer_motion.len();
	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", ABS_B.0, ABS_B.1));
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_motion.len() > motions),
		"a locked pointer did not receive absolute motion to {ABS_B:?} within {EVENT_TIMEOUT:?}"
	);
	assert!(
		has_motion_near(&started.client.snapshot(), ABS_B, 2.0),
		"the locked pointer did not move to {ABS_B:?}"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A confined pointer is clamped to its `wl_region`.
///
/// `zwp_pointer_constraints_v1.confine` constrains the pointer to the
/// intersection of the region and the surface while the surface has focus. The
/// test maps a 320x240 window, focuses the pointer outside a 50x50 region,
/// creates the constraint, then injects motion beyond the region and the
/// window: the client's last `wl_pointer.motion` must be at the region's
/// nearest corner, not the raw position. A point inside the region passes
/// through, and destroying the constraint frees motion again.
#[test]
#[ignore = "requires GPU; native Wayland client for pointer confine"]
fn wayland_pointer_confine_clamps() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// Focus the pointer inside the window but outside the confine region, so
	// activation has to confine it rather than merely preserve it.
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", CONFINE_FOCUS.0, CONFINE_FOCUS.1),
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::Confine {
			x: CONFINE_REGION.0,
			y: CONFINE_REGION.1,
			width: CONFINE_REGION.2,
			height: CONFINE_REGION.3,
		}),
		"the confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {EVENT_TIMEOUT:?}"
	);

	// Inject beyond the region and the window: the pointer must be clamped to
	// the region's nearest corner, not follow the raw position.
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", CONFINE_FAR.0, CONFINE_FAR.1),
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| has_motion_near(
			log,
			CONFINE_CLAMPED,
			1.0
		)),
		"the confined pointer was not clamped to {CONFINE_CLAMPED:?} after injecting {CONFINE_FAR:?} \
		 within {EVENT_TIMEOUT:?}"
	);
	let log = started.client.snapshot();
	let (last_x, last_y) = *log.pointer_motion.last().expect("pointer motion");
	assert!(
		(100.0..=150.0).contains(&last_x) && (100.0..=150.0).contains(&last_y),
		"the confined pointer left its region; last motion was ({last_x}, {last_y})"
	);
	assert!(
		(last_x - CONFINE_CLAMPED.0).abs() <= 1.0 && (last_y - CONFINE_CLAMPED.1).abs() <= 1.0,
		"the confined pointer was not clamped to the region edge; last motion was ({last_x}, {last_y})"
	);

	// A point inside the region is not moved.
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", CONFINE_INSIDE.0, CONFINE_INSIDE.1),
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| has_motion_near(
			log,
			CONFINE_INSIDE,
			1.0
		)),
		"a point inside the confine region did not pass through within {EVENT_TIMEOUT:?}"
	);
	let (last_x, last_y) = *started.client.snapshot().pointer_motion.last().expect("pointer motion");
	assert!(
		(last_x - CONFINE_INSIDE.0).abs() <= 1.0 && (last_y - CONFINE_INSIDE.1).abs() <= 1.0,
		"a point inside the confine region was moved; last motion was ({last_x}, {last_y})"
	);

	// Removing the constraint frees the pointer: motion outside the region is
	// no longer clamped.
	assert!(
		started.client.command(CommandKind::Unconfine),
		"the unconfine command was not serviced"
	);
	let motions = started.client.snapshot().pointer_motion.len();
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", CONFINE_FOCUS.0, CONFINE_FOCUS.1),
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.pointer_motion.len() > motions),
		"no wl_pointer.motion after removing the constraint within {EVENT_TIMEOUT:?}"
	);
	let (last_x, last_y) = *started.client.snapshot().pointer_motion.last().expect("pointer motion");
	assert!(
		(last_x - CONFINE_FOCUS.0).abs() <= 1.0 && (last_y - CONFINE_FOCUS.1).abs() <= 1.0,
		"the pointer stayed clamped after the constraint was removed; last motion was ({last_x}, {last_y})"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A **regionless** confine clamps to the whole surface.
///
/// Xwayland sends a NULL region for an X confine-to grab (`xwayland-input.c`
/// `xwl_seat_confine_pointer`); the protocol default is the whole surface. The
/// pointer must clamp to the surface's own edges on every side, and reaching an
/// edge must not release the constraint (which would draw the OS cursor).
#[test]
#[ignore = "requires GPU; native Wayland client for pointer confine"]
fn wayland_regionless_confine_clamps_to_surface() {
	// The harness output is 320x240 and, with no explicit buffer size, the
	// client commits a surface of that size, so surface-local and output
	// coordinates coincide and the surface edges are at 319/239.
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	support::send_line(&mut started.control, "motion-abs 40 40\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {EVENT_TIMEOUT:?}"
	);

	let cases: [((f64, f64), (f64, f64)); 4] = [
		((-10.0, 120.0), (0.0, 120.0)),
		((400.0, 120.0), (319.0, 120.0)),
		((160.0, -10.0), (160.0, 0.0)),
		((160.0, 300.0), (160.0, 239.0)),
	];
	for (output, expected) in cases {
		support::send_line(&mut started.control, &format!("motion-abs {} {}\n", output.0, output.1));
		assert!(
			support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
				.pointer_motion
				.last()
				.is_some_and(|&(x, y)| (x - expected.0).abs() <= 1.0 && (y - expected.1).abs() <= 1.0)),
			"a regionless confine did not clamp output {output:?} to {expected:?}: {:?}",
			started.client.snapshot().pointer_motion
		);
	}

	let log = started.client.snapshot();
	assert_eq!(
		log.unconfined, 0,
		"the constraint was released at a surface edge (unconfined={})",
		log.unconfined
	);
	for &(x, y) in &log.pointer_motion {
		assert!(
			(0.0..=320.0).contains(&x) && (0.0..=240.0).contains(&y),
			"a delivered point ({x}, {y}) left the surface"
		);
	}

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// At a confine boundary the axes are independent, and the exclusive
/// right/bottom edge neither freezes the other axis nor drops the capture
///at 1:1 as well as under output-fill.
#[test]
#[ignore = "requires GPU; native Wayland client for pointer confine"]
fn wayland_regionless_confine_axes_are_independent() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {EVENT_TIMEOUT:?}"
	);

	// Each pair is (at/over an edge, then along the other axis).
	let cases: [((f64, f64), (f64, f64)); 8] = [
		((400.0, 120.0), (319.0, 120.0)),
		((400.0, 200.0), (319.0, 200.0)),
		((160.0, 300.0), (160.0, 239.0)),
		((240.0, 300.0), (240.0, 239.0)),
		((160.0, -10.0), (160.0, 0.0)),
		((160.0, 50.0), (160.0, 50.0)),
		((-10.0, 120.0), (0.0, 120.0)),
		((50.0, 120.0), (50.0, 120.0)),
	];
	for (output, expected) in cases {
		support::send_line(&mut started.control, &format!("motion-abs {} {}\n", output.0, output.1));
		assert!(
			support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
				.pointer_motion
				.last()
				.is_some_and(|&(x, y)| (x - expected.0).abs() <= 1.0 && (y - expected.1).abs() <= 1.0)),
			"a regionless confine did not deliver output {output:?} at {expected:?}: {:?}",
			started.client.snapshot().pointer_motion
		);
	}

	assert_eq!(
		started.client.snapshot().unconfined,
		0,
		"the capture was released at an edge"
	);
	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// `xdg_activation_v1` routed to the single window.
///
/// The activation request is a re-assertion while the window is focused — there
/// is exactly one window and it is always focused — so the observable contract
/// is: the token is issued, the request is accepted, and the toplevel is
/// `Activated` with keyboard focus.
#[test]
#[ignore = "requires GPU; native Wayland client for xdg_activation_v1"]
fn wayland_activation_keeps_window_focused() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.activated.contains(&parent)),
		"the window was not Activated within {POINTER_TIMEOUT:?}"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&parent)),
		"the window never received wl_keyboard.enter within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::Activate),
		"the activation command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| !log.activation_tokens.is_empty()),
		"xdg_activation_v1 issued no token within {EVENT_TIMEOUT:?}"
	);

	let log = started.client.snapshot();
	assert!(
		log.activated.contains(&parent),
		"the window lost Activated after activation"
	);
	assert!(
		log.keyboard_enter.contains(&parent),
		"the window lost keyboard focus after activation"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A parented transient takes keyboard focus while the window keeps the
/// fullscreen role.
///
/// The transient is drawn over the window and never becomes a second fullscreen
/// window; when it maps it becomes `Activated`, receives `wl_keyboard.enter`,
/// and the window receives `wl_keyboard.leave`. Only the parent is ever
/// fullscreen, so the single-window invariant holds.
#[test]
#[ignore = "requires GPU; native Wayland client with a parented toplevel"]
fn wayland_transient_takes_focus() {
	let mut started = support::start(support::ClientOptions {
		with_child: true,
		..Default::default()
	});
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	let child = started.client.surface_id(Target::Child).expect("child surface id");

	assert_ne!(parent, child, "parent and child must be distinct surfaces");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.activated.contains(&child)),
		"the parented transient was not Activated within {POINTER_TIMEOUT:?}"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&child)),
		"the parented transient never received wl_keyboard.enter within {POINTER_TIMEOUT:?}"
	);

	// Give the focus move time to settle, then assert the window handed focus
	// over rather than keeping it.
	thread::sleep(SETTLE);
	let log = started.client.snapshot();
	assert!(
		log.keyboard_enter.contains(&child),
		"the parented transient never received keyboard focus: {:?}",
		log.keyboard_enter
	);
	assert!(
		log.keyboard_leave.contains(&parent),
		"the window did not lose keyboard focus to the transient: {:?}",
		log.keyboard_leave
	);

	// The transient is not a second fullscreen window: only the parent is ever
	// fullscreen, so the single-window invariant holds.
	let mut fullscreen = log.fullscreen.clone();
	fullscreen.sort_unstable();
	fullscreen.dedup();
	assert_eq!(
		fullscreen,
		vec![parent],
		"more than one surface was fullscreen; the single-window invariant broke"
	);
	assert!(
		!log.fullscreen.contains(&child),
		"the parented transient became a fullscreen window"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Focus clears with the focused toplevel, and the next toplevel takes over.
#[test]
#[ignore = "requires GPU; native Wayland client destroying and remapping a toplevel"]
fn wayland_focus_moves_to_next_window() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&parent)),
		"the window never received wl_keyboard.enter within {POINTER_TIMEOUT:?}"
	);

	assert!(
		started.client.command(CommandKind::DestroyParent),
		"destroy command serviced"
	);
	// Let the compositor's per-tick liveness check release the window role.
	thread::sleep(SETTLE);

	assert!(
		started.client.command(CommandKind::CreateSecondToplevel),
		"second toplevel command serviced"
	);
	let second = started.client.surface_id(Target::Second).expect("second surface id");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&second)),
		"the next toplevel did not receive keyboard focus within {POINTER_TIMEOUT:?}; \
		 focus dangled on the destroyed window"
	);
	assert!(
		started.client.snapshot().activated.contains(&second),
		"the next toplevel was not Activated"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}
