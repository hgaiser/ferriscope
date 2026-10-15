//! GPU integration test for output-fill scaling.
//!
//! gamescope lets a client keep its own resolution and scales the committed
//! buffer to fill the output. ferriscope first tried to force the window to the
//! output size, which Proton/Wine defeats: it keeps its real render surface at
//! the game's resolution inside the forced window. This test is the frame-level
//! evidence for the scaling replacement.
//!
//! A native Wayland client commits a buffer **smaller than the output** while
//! the toplevel is configured fullscreen. The composited frame must show the
//! content across the **full output extent**, not confined to the top-left
//! quadrant. The same run asserts the pointer mapping: an absolute motion at an
//! output point must reach the client at the scaled surface-local point, so
//! input stays correct under the transform.
//!
//! It is `#[ignore]`d: it needs a render node. Run it alone:
//!
//! ```text
//! timeout 180 cargo test -p ferriscope-core --test output_fill -- --ignored --exact smaller_surface_is_scaled_to_fill_the_output --nocapture
//! ```

use std::{
	path::{Path, PathBuf},
	thread,
	time::{Duration, Instant},
};

use support::{ClientOptions, CommandKind, Target};

mod support;

/// Output width; matches the harness.
const WIDTH: usize = 320;
/// Output height.
const HEIGHT: usize = 240;
/// The committed surface size: exactly half the output on each axis, so an
/// unscaled composite would occupy the top-left quadrant.
const SURFACE: (i32, i32) = (160, 120);
/// The output point injected for the input-mapping assertion.
const POINTER: (f64, f64) = (240.0, 180.0);
/// The expected surface-local point: `POINTER / 2` (the fill scale is 2.0).
const POINTER_LOCAL: (f64, f64) = (120.0, 90.0);
/// Stop writing after this many frames; bounds disk use on tmpfs.
const RECORD_LIMIT: u64 = 1500;
/// Budget for the window to map and a content-bearing frame to appear.
const MAPPED_TIMEOUT: Duration = Duration::from_secs(40);
/// Budget for the injected pointer motion to reach the client.
const POINTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a cursor-bearing recorded frame to appear after a motion.
const CURSOR_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll slice while waiting on frames.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Settle time before asserting that a focus target did *not* change.
const SETTLE: Duration = Duration::from_millis(700);

/// A decoded PPM frame: dimensions and 8-bit RGB body.
struct Ppm {
	width: usize,
	height: usize,
	rgb: Vec<u8>,
}

/// Parse the `P6` frames the `FileRecorder` writes (`P6\n<w> <h>\n255\n<body>`).
fn parse_ppm(bytes: &[u8]) -> Option<Ppm> {
	if !bytes.starts_with(b"P6\n") {
		return None;
	}
	let dims_end = bytes[3..].iter().position(|&b| b == b'\n')? + 3;
	let dims = std::str::from_utf8(&bytes[3..dims_end]).ok()?;
	let mut tokens = dims.split_whitespace();
	let width: usize = tokens.next()?.parse().ok()?;
	let height: usize = tokens.next()?.parse().ok()?;
	let maxval_end = bytes[dims_end + 1..].iter().position(|&b| b == b'\n')? + dims_end + 1;
	let rgb = &bytes[maxval_end + 1..];
	if rgb.len() < width * height * 3 {
		return None;
	}
	Some(Ppm {
		width,
		height,
		rgb: rgb.to_vec(),
	})
}

/// Read and decode a recorded frame, or `None` if it is not written yet.
fn read_frame(dir: &Path, sequence: u64) -> Option<Ppm> {
	let bytes = std::fs::read(dir.join(format!("frame_{sequence:06}.ppm"))).ok()?;
	parse_ppm(&bytes)
}

/// The highest recorded sequence number so far, or `None`.
fn latest_sequence(dir: &Path) -> Option<u64> {
	let mut latest: Option<u64> = None;
	for entry in std::fs::read_dir(dir).ok()?.flatten() {
		let name = entry.file_name();
		let name = name.to_string_lossy();
		if let Some(rest) = name.strip_prefix("frame_") {
			if let Some(digits) = rest.strip_suffix(".ppm") {
				if let Ok(sequence) = digits.parse::<u64>() {
					latest = Some(latest.map_or(sequence, |current| current.max(sequence)));
				}
			}
		}
	}
	latest
}

/// Whether all pixels of a frame are non-black (the scaled content fills it).
fn fills_output(frame: &Ppm) -> bool {
	frame.width == WIDTH && frame.height == HEIGHT && frame.rgb.chunks_exact(3).all(|pixel| pixel != [0, 0, 0])
}

/// Wait, bounded, for a recorded frame whose content spans the whole output.
fn wait_for_fill_frame(dir: &Path, timeout: Duration) -> Option<Ppm> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(sequence) = latest_sequence(dir) {
			if let Some(frame) = read_frame(dir, sequence) {
				if fills_output(&frame) {
					return Some(frame);
				}
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Count pixels whose RGB is not all zero.
fn non_black_pixels(ppm: &Ppm) -> usize {
	ppm.rgb.chunks_exact(3).filter(|pixel| pixel != &[0, 0, 0]).count()
}

/// Positions `(x, y)` where two same-sized frames differ.
fn diff_positions(a: &Ppm, b: &Ppm) -> Vec<(i32, i32)> {
	let mut positions = Vec::new();
	for y in 0..a.height.min(b.height) {
		for x in 0..a.width.min(b.width) {
			let offset = (y * a.width + x) * 3;
			let (Some(pa), Some(pb)) = (a.rgb.get(offset..offset + 3), b.rgb.get(offset..offset + 3)) else {
				continue;
			};
			if pa != pb {
				positions.push((x as i32, y as i32));
			}
		}
	}
	positions
}

/// The `(min_x, min_y, max_x, max_y)` bounding box of the pixels where `a` and
/// `b` differ, or `None` when they are identical.
fn diff_bbox(a: &Ppm, b: &Ppm) -> Option<(i32, i32, i32, i32)> {
	let positions = diff_positions(a, b);
	let first = *positions.first()?;
	Some(positions.into_iter().fold(
		(first.0, first.1, first.0, first.1),
		|(min_x, min_y, max_x, max_y), (x, y)| (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y)),
	))
}

/// Wait, bounded, for a stable mapped, non-black frame (the no-cursor
/// baseline): a frame identical to its predecessor so a mapping redraw in
/// progress is not used as the reference.
fn wait_for_stable_frame(dir: &Path, timeout: Duration) -> Option<Ppm> {
	let threshold = WIDTH * HEIGHT / 100;
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(sequence) = latest_sequence(dir) {
			if let Some(frame) = read_frame(dir, sequence) {
				if non_black_pixels(&frame) > threshold {
					if let Some(previous) = read_frame(dir, sequence.saturating_sub(1)) {
						if diff_positions(&previous, &frame).is_empty() {
							return Some(frame);
						}
					}
				}
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, until the newest recorded frame differs from `baseline` and
/// its changed-pixel bounding box's left edge is within `left_edge_range`.
///
/// The baseline has no cursor image, so the diff is exactly the composited
/// cursor; its left edge is the compositor cursor's output-space x. Polling the
/// **newest** frame waits out the motion without racing a still-old frame.
fn wait_for_cursor_left_edge(
	dir: &Path,
	baseline: &Ppm,
	left_edge_range: (i32, i32),
	timeout: Duration,
) -> Option<Ppm> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(sequence) = latest_sequence(dir) {
			if let Some(frame) = read_frame(dir, sequence) {
				if let Some((min_x, _, _, _)) = diff_bbox(baseline, &frame) {
					if min_x >= left_edge_range.0 && min_x <= left_edge_range.1 {
						return Some(frame);
					}
				}
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// A unique frame-recording directory under the temp dir.
fn unique_record_dir() -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-output-fill-{}-{nanos}", std::process::id()))
}

#[test]
#[ignore = "requires GPU"]
fn smaller_surface_is_scaled_to_fill_the_output() {
	let record_dir = unique_record_dir();
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start_with_record(options, Some(record_dir.clone()), Some(RECORD_LIMIT));

	assert!(
		wait_for_fill_frame(&record_dir, MAPPED_TIMEOUT).is_some(),
		"a {SURFACE:?} surface on a {WIDTH}x{HEIGHT} output did not fill the frame; \
		 recorded frames in {}",
		record_dir.display()
	);

	// The pointer transform must invert the fill: an output point maps to the
	// scaled surface-local point, not the raw output one.
	support::send_line(&mut started.control, "motion-abs 240 180\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| {
			log.pointer_motion
				.last()
				.is_some_and(|&(x, y)| (x - POINTER_LOCAL.0).abs() < 2.0 && (y - POINTER_LOCAL.1).abs() < 2.0)
		}),
		"output point {POINTER:?} did not reach the client at {POINTER_LOCAL:?}: {:?}",
		started.client.snapshot().pointer_motion
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A plain peer normal window mapped **above** a scaled role holder never takes
/// pointer or keyboard focus.
///
/// `pointer_under` preferred the role only when it was at or above
/// the element the unscaled hit-test found. `Space::map_element` appends at the
/// top, so a peer mapped or re-raised after the role (real Steam Big Picture
/// does this on a `ConfigureNotify`) sorted above it, won the hit-test, and
/// stole both pointer and keyboard focus. A peer must be input-transparent
/// while a role holder exists.
///
/// The role here commits a `160x120` buffer on the `320x240` output (fill
/// `2.0`) and a full-output peer is mapped after it. Every output point must
/// reach the role at the fill-inverse point; the peer never receives a
/// `wl_pointer.enter`, an activation request from it is ignored, and a
/// transient/dialog still takes keyboard focus afterwards.
#[test]
#[ignore = "requires GPU"]
fn peer_above_scaled_role_never_takes_focus() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);
	let role = started.client.surface_id(Target::Parent).expect("role surface id");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&role)),
		"the role holder never received keyboard focus"
	);

	assert!(
		started.client.command(CommandKind::CreatePeerToplevel),
		"the create-peer command was not serviced"
	);
	let peer = started.client.surface_id(Target::Second).expect("peer surface id");
	assert_ne!(role, peer, "the peer must be a distinct surface");
	thread::sleep(SETTLE);

	// Output points across the whole output, including every edge, and the
	// surface-local point the fill inverse must produce. A flip to the peer
	// would deliver the raw output point (the peer is unscaled).
	let cases: [((f64, f64), (f64, f64)); 5] = [
		((5.0, 5.0), (2.5, 2.5)),
		((315.0, 5.0), (157.5, 2.5)),
		((5.0, 235.0), (2.5, 117.5)),
		((315.0, 235.0), (157.5, 117.5)),
		((160.0, 120.0), (80.0, 60.0)),
	];
	for (output, local) in cases {
		support::send_line(&mut started.control, &format!("motion-abs {} {}\n", output.0, output.1));
		assert!(
			support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
				.pointer_motion
				.last()
				.is_some_and(|&(x, y)| (x - local.0).abs() <= 1.0 && (y - local.1).abs() <= 1.0)),
			"output {output:?} did not reach the scaled role at {local:?}; motions: {:?}",
			started.client.snapshot().pointer_motion
		);
	}

	// A click over the peer must not move pointer focus to it.
	support::send_line(&mut started.control, "button 272 1\n");
	support::send_line(&mut started.control, "button 272 0\n");
	thread::sleep(SETTLE);
	let log = started.client.snapshot();
	assert!(
		!log.pointer_enter.contains(&peer),
		"the peer above the role took pointer focus: {:?}",
		log.pointer_enter
	);
	assert!(
		!log.keyboard_enter.contains(&peer),
		"the peer above the role took keyboard focus: {:?}",
		log.keyboard_enter
	);
	assert!(
		log.keyboard_enter.contains(&role) && !log.keyboard_leave.contains(&role),
		"the role lost keyboard focus to the peer: enter={:?} leave={:?}",
		log.keyboard_enter,
		log.keyboard_leave
	);

	// An explicit activation request from the peer is ignored.
	assert!(
		started.client.command(CommandKind::ActivateSecond),
		"the activate-second command was not serviced"
	);
	thread::sleep(SETTLE);
	let log = started.client.snapshot();
	assert!(
		!log.keyboard_enter.contains(&peer) && !log.keyboard_leave.contains(&role),
		"activation moved keyboard focus to the peer: enter={:?} leave={:?}",
		log.keyboard_enter,
		log.keyboard_leave
	);

	// A parented transient/dialog still takes keyboard focus.
	assert!(
		started.client.command(CommandKind::CreateChildToplevel),
		"the create-child command was not serviced"
	);
	let child = started.client.surface_id(Target::Child).expect("child surface id");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&child)),
		"the parented transient did not take keyboard focus: {:?}",
		started.client.snapshot().keyboard_enter
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A scaled role holder owns pointer focus across the **whole** output,
/// including the letterbox and the content's far edge.
///
/// `pointer_under` inverse-mapped the output point with
/// `fill.output_to_content`, but a point on the output's far edge maps exactly
/// onto the source size — the *exclusive* right/bottom edge of the surface
/// hit-test — so the role would lose focus on the last output pixel/row, and
/// any letterbox output point would map outside the surface entirely, ending
/// the confine/lock lifetime.
///
/// The role here commits a square `160x160` buffer on the `320x240` output:
/// the fit scale is `min(2.0, 1.5) = 1.5`, the content is `240x240` centred at
/// x offset `40`, so `x < 40` and `x >= 280` are letterbox. Every injected
/// output point — interior, both letterbox bars and the far content edge — must
/// reach the role at the clamped surface-local point and never drop focus.
#[test]
#[ignore = "requires GPU"]
fn scaled_role_owns_pointer_focus_across_the_whole_output() {
	let options = ClientOptions {
		buffer_size: Some((160, 160)),
		..Default::default()
	};
	let mut started = support::start(options);
	let role = started.client.surface_id(Target::Parent).expect("role surface id");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&role)),
		"the role holder never received keyboard focus"
	);

	// The pointer enters once. A leave anywhere in the output would make the
	// client observe a pointer leave; the role must keep it for every point.
	// (interior, left bar, right bar, far edge, lower interior) -> surface-local.
	// The fill scale is `min(320/160, 240/160) = 1.5` with an x offset of 40,
	// so the inverse is `((x - 40) / 1.5, y / 1.5)` clamped to `[0, 160)`.
	let cases: [((f64, f64), (f64, f64)); 5] = [
		((160.0, 120.0), (80.0, 80.0)),
		((5.0, 120.0), (0.0, 80.0)),
		((315.0, 120.0), (160.0, 80.0)),
		((280.0, 120.0), (160.0, 80.0)),
		((160.0, 235.0), (80.0, 235.0 / 1.5)),
	];
	for (output, local) in cases {
		support::send_line(&mut started.control, &format!("motion-abs {} {}\n", output.0, output.1));
		assert!(
			support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
				.pointer_motion
				.last()
				.is_some_and(|&(x, y)| (x - local.0).abs() <= 1.0 && (y - local.1).abs() <= 1.0)),
			"output {output:?} did not reach the scaled role at {local:?}; motions: {:?}",
			started.client.snapshot().pointer_motion
		);
	}

	// A click at the far edge must still reach the role and not move focus.
	support::send_line(&mut started.control, "motion-abs 315 235\n");
	support::send_line(&mut started.control, "button 272 1\n");
	support::send_line(&mut started.control, "button 272 0\n");
	thread::sleep(SETTLE);
	let log = started.client.snapshot();
	// A focus loss inside the output would show as the pointer leaving and
	// re-entering the role, i.e. more than one `wl_pointer.enter` for it. The
	// role must have entered exactly once across every injected point.
	assert_eq!(
		log.pointer_enter.iter().filter(|&&id| id == role).count(),
		1,
		"the role was entered more than once; pointer focus left the output extent: {:?}",
		log.pointer_enter
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A plain peer normal window mapped above an **unscaled** role holder never
/// takes pointer focus.
///
/// Without an output-fill transform the role holder's input extent is its own
/// scene rectangle, which is the whole output here, so the peer must be
/// excluded from the hit-test rather than shadowing the role by z-order.
#[test]
#[ignore = "requires GPU"]
fn peer_above_unscaled_role_never_takes_pointer_focus() {
	let mut started = support::start(ClientOptions::default());
	let role = started.client.surface_id(Target::Parent).expect("role surface id");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&role)),
		"the role holder never received keyboard focus"
	);

	assert!(
		started.client.command(CommandKind::CreatePeerToplevel),
		"the create-peer command was not serviced"
	);
	let peer = started.client.surface_id(Target::Second).expect("peer surface id");
	thread::sleep(SETTLE);

	let cases: [((f64, f64), (f64, f64)); 5] = [
		((10.0, 10.0), (10.0, 10.0)),
		((310.0, 10.0), (310.0, 10.0)),
		((10.0, 230.0), (10.0, 230.0)),
		((310.0, 230.0), (310.0, 230.0)),
		((160.0, 120.0), (160.0, 120.0)),
	];
	for (output, local) in cases {
		support::send_line(&mut started.control, &format!("motion-abs {} {}\n", output.0, output.1));
		assert!(
			support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
				.pointer_motion
				.last()
				.is_some_and(|&(x, y)| (x - local.0).abs() <= 1.0 && (y - local.1).abs() <= 1.0)),
			"output {output:?} did not reach the unscaled role at {local:?}; motions: {:?}",
			started.client.snapshot().pointer_motion
		);
	}

	support::send_line(&mut started.control, "button 272 1\n");
	support::send_line(&mut started.control, "button 272 0\n");
	thread::sleep(SETTLE);
	let log = started.client.snapshot();
	assert!(
		!log.pointer_enter.contains(&peer),
		"the peer above the unscaled role took pointer focus: {:?}",
		log.pointer_enter
	);
	assert!(
		!log.keyboard_enter.contains(&peer),
		"the peer above the unscaled role took keyboard focus: {:?}",
		log.keyboard_enter
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Output-space injection points and the surface-local point each must reach.
///
/// `(injected output, expected surface-local)` at fill scale 2.0 with the
/// explicit region [`CONFINE_REGION`]; the last-inside coordinate of the
/// region's exclusive right/bottom edge is `(119, 89)`.
const CONFINE_CASES: [((f64, f64), (f64, f64)); 5] = [
	((-30.0, 120.0), (40.0, 60.0)),
	((350.0, 120.0), (119.0, 60.0)),
	((160.0, -30.0), (80.0, 30.0)),
	((160.0, 270.0), (80.0, 89.0)),
	((160.0, 120.0), (80.0, 60.0)),
];

/// The same points with no region: the whole surface, so they clamp to its
/// edges (`0..=159`, `0..=119`) instead of the smaller region.
const SURFACE_CASES: [((f64, f64), (f64, f64)); 4] = [
	((-30.0, 120.0), (0.0, 60.0)),
	((350.0, 120.0), (159.0, 60.0)),
	((160.0, -30.0), (80.0, 0.0)),
	((160.0, 270.0), (80.0, 119.0)),
];

/// The confine region in surface-local coordinates: a sub-rectangle of the
/// `160x120` surface, so a clamp to it is distinguishable from a clamp to the
/// surface edge.
const CONFINE_REGION: (i32, i32, i32, i32) = (40, 30, 80, 60);
/// A seed point inside the surface but outside [`CONFINE_REGION`], so creating
/// the constraint has to clamp it (in surface-local coords, `(10, 10)`).
const CONFINE_SEED: (f64, f64) = (20.0, 20.0);

/// Inject `output` and assert the delivered surface-local point is
/// `expected`, the pointer stayed inside `region`, and the capture was never
/// released.
fn assert_confined_injection(
	started: &mut support::Started,
	output: (f64, f64),
	expected: (f64, f64),
	region: (f64, f64, f64, f64),
	label: &str,
) {
	let before = started.client.snapshot().pointer_motion.len();
	support::send_line(&mut started.control, &format!("motion-abs {} {}\n", output.0, output.1));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_motion
			.last()
			.is_some_and(|&(x, y)| (x - expected.0).abs() <= 1.0 && (y - expected.1).abs() <= 1.0)),
		"{label}: output {output:?} did not reach the client at {expected:?}; motions: {:?}",
		started.client.snapshot().pointer_motion
	);

	let log = started.client.snapshot();
	assert_eq!(
		log.unconfined, 0,
		"{label}: the capture was released at the confine edge (unconfined={})",
		log.unconfined
	);
	for &(x, y) in log.pointer_motion.iter().skip(before) {
		assert!(
			x >= region.0 - 1.0 && x <= region.1 + 1.0 && y >= region.2 - 1.0 && y <= region.3 + 1.0,
			"{label}: a delivered point ({x}, {y}) left the region {region:?}"
		);
	}
}

/// A confined pointer clamps to its region on all four edges under output-fill
/// scaling, and a **regionless** confine (the shape Xwayland sends
/// for an X confine-to grab, `xwayland-input.c:3570`) clamps to the surface.
///
/// The role holder commits a `160x120` buffer on the `320x240` output, so the
/// output-fill scale is `2.0`. The confine region is a sub-rectangle, so the
/// left/top and right/bottom edges are all strictly inside the surface and a
/// failure cannot be masked by the output edge. Every injection past an edge
/// must arrive clamped, the pointer must never be delivered outside the
/// region, and the capture must not be released.
#[test]
#[ignore = "requires GPU"]
fn confine_clamps_on_all_edges_with_output_fill() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);

	// Seed focus inside the surface but outside the region, so activation
	// itself must clamp (in surface-local coords, `(10, 10)` outside the
	// region, whose first inside point is `(40, 30)`).
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", CONFINE_SEED.0, CONFINE_SEED.1),
	);
	thread::sleep(Duration::from_millis(300));

	assert!(
		started.client.command(support::CommandKind::Confine {
			x: CONFINE_REGION.0,
			y: CONFINE_REGION.1,
			width: CONFINE_REGION.2,
			height: CONFINE_REGION.3,
		}),
		"the confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {POINTER_TIMEOUT:?}"
	);
	// Activation clamped the seed onto the region's first inside point.
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_motion
			.last()
			.is_some_and(|&(x, y)| (x - 40.0).abs() <= 1.0 && (y - 30.0).abs() <= 1.0)),
		"activation did not clamp the outside seed to the region: {:?}",
		started.client.snapshot().pointer_motion
	);

	let region = (40.0, 119.0, 30.0, 89.0);
	for (output, expected) in CONFINE_CASES {
		assert_confined_injection(&mut started, output, expected, region, "explicit region");
	}

	// Now the regionless (Xwayland) shape: the whole surface is the region.
	assert!(
		started.client.command(support::CommandKind::Unconfine),
		"the unconfine command was not serviced"
	);
	assert!(
		started.client.command(support::CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 2),
		"no second zwp_confined_pointer_v1.confined event within {POINTER_TIMEOUT:?}"
	);

	let surface = (0.0, 159.0, 0.0, 119.0);
	for (output, expected) in SURFACE_CASES {
		assert_confined_injection(&mut started, output, expected, surface, "regionless surface");
	}

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// At a confine boundary the two axes are independent, and reaching the
/// exclusive right/bottom edge neither freezes the other axis nor drops the
/// capture.
///
/// The earlier all-edges test only proves each edge clamps; it does not prove
/// that a later motion *along* the boundary still arrives. A bug that clamps
/// the whole point (or loses pointer focus at the exclusive edge) would freeze
/// both axes there.
#[test]
#[ignore = "requires GPU"]
fn confine_axes_are_independent_at_the_exclusive_edges() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	thread::sleep(Duration::from_millis(300));
	assert!(
		started.client.command(support::CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {POINTER_TIMEOUT:?}"
	);

	let surface = (0.0, 159.0, 0.0, 119.0);
	// Right edge, then move vertically: the exclusive right edge must not
	// freeze the other axis.
	assert_confined_injection(&mut started, (350.0, 120.0), (159.0, 60.0), surface, "right edge");
	assert_confined_injection(
		&mut started,
		(350.0, 200.0),
		(159.0, 100.0),
		surface,
		"right edge vertical",
	);
	// Bottom edge, then move horizontally.
	assert_confined_injection(&mut started, (160.0, 270.0), (80.0, 119.0), surface, "bottom edge");
	assert_confined_injection(
		&mut started,
		(240.0, 270.0),
		(120.0, 119.0),
		surface,
		"bottom edge horizontal",
	);
	// The inclusive left/top edges keep both axes live too.
	assert_confined_injection(&mut started, (160.0, -30.0), (80.0, 0.0), surface, "top edge");
	assert_confined_injection(&mut started, (160.0, 30.0), (80.0, 15.0), surface, "top edge vertical");
	assert_confined_injection(&mut started, (-30.0, 120.0), (0.0, 60.0), surface, "left edge");
	assert_confined_injection(
		&mut started,
		(30.0, 120.0),
		(15.0, 60.0),
		surface,
		"left edge horizontal",
	);

	assert_eq!(
		started.client.snapshot().unconfined,
		0,
		"the capture was released at an edge"
	);
	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A confined role holder's reachable surface-local range covers the whole
/// output on both axes.
///
/// The `160x120` role is scaled `2x` onto the `320x240` output, so the confine
/// must map the whole surface (`0..=159` x `0..=119`) onto the whole output.
/// Injecting at the output's far corners must deliver points at the surface's
/// far edges: a confine that clamps to a sub-rectangle (a too-small
/// `surface_logical_size`, or a half-open clamp) would collapse the range and
/// limit the in-game look range.
#[test]
#[ignore = "requires GPU"]
fn confined_role_reaches_the_whole_output_range() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	thread::sleep(Duration::from_millis(300));
	assert!(
		started.client.command(support::CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {POINTER_TIMEOUT:?}"
	);

	// Sweep to each output edge; the delivered surface-local point must reach
	// the corresponding surface edge.
	for (x, y) in [(0.0, 120.0), (319.0, 120.0), (160.0, 0.0), (160.0, 239.0)] {
		support::send_line(&mut started.control, &format!("motion-abs {x} {y}\n"));
		thread::sleep(Duration::from_millis(120));
	}

	let log = started.client.snapshot();
	let xs: Vec<f64> = log.pointer_motion.iter().map(|point| point.0).collect();
	let ys: Vec<f64> = log.pointer_motion.iter().map(|point| point.1).collect();
	let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
	let max_x = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
	let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min);
	let max_y = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
	assert!(
		min_x <= 1.0 && max_x >= 158.0,
		"the confined x range {min_x}..={max_x} did not cover the surface; motions: {:?}",
		log.pointer_motion
	);
	assert!(
		min_y <= 1.0 && max_y >= 118.0,
		"the confined y range {min_y}..={max_y} did not cover the surface; motions: {:?}",
		log.pointer_motion
	);
	assert_eq!(log.unconfined, 0, "the capture was released during the sweep");

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A relative motion delta is delivered to the client's relative pointer
/// **unchanged** under output-fill.
///
/// The delta is a device/surface-local delta: the fill transform applies to the
/// absolute cursor (and to the confined cursor position), not to the relative
/// mouselook path. Dividing the delivered delta by the fill scale would reduce
/// the look range. At a `2x` fill, `motion-rel 60 30`
/// must arrive as `(60, 30)`, not `(30, 15)`.
#[test]
#[ignore = "requires GPU"]
fn relative_motion_is_delivered_unscaled_under_output_fill() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	thread::sleep(Duration::from_millis(300));
	assert!(
		started.client.command(support::CommandKind::LockRelative),
		"the lock command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.locked >= 1),
		"no zwp_locked_pointer.locked event within {POINTER_TIMEOUT:?}"
	);

	let before = started.client.snapshot().relative_motion.len();
	support::send_line(&mut started.control, "motion-rel 60 30\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.relative_motion.len()
			> before),
		"no relative_motion event after motion-rel within {POINTER_TIMEOUT:?}"
	);
	let (dx, dy) = *started
		.client
		.snapshot()
		.relative_motion
		.last()
		.expect("relative motion");
	assert!(
		(dx - 60.0).abs() < 0.5 && (dy - 30.0).abs() < 0.5,
		"relative_motion deltas were ({dx}, {dy}), expected (60, 30): the fill scale must not reduce the relative path"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A confined pointer's cursor advances from a relative delta and the client is
/// notified of the new absolute position, matching gamescope.
///
/// gamescope applies the relative delta to its surface-local cursor and then
/// calls `wlr_seat_pointer_notify_motion` (`wlserver.cpp:3073-3082`), so a
/// client that reads the absolute pointer tracks the delta even though only
/// relative input was injected. Under output-fill the delta is in surface-local
/// units, so a `40x20` delta moves the `2x`-scaled output cursor by `80x40`:
/// `(160,120)` -> `(240,160)` output -> `(120,80)` surface-local.
#[test]
#[ignore = "requires GPU"]
fn confined_cursor_advances_on_relative_motion_under_output_fill() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	thread::sleep(Duration::from_millis(300));
	assert!(
		started.client.command(support::CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {POINTER_TIMEOUT:?}"
	);

	let before = started.client.snapshot().pointer_motion.len();
	support::send_line(&mut started.control, "motion-rel 40 20\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.pointer_motion.len()
			> before),
		"the confined cursor did not advance on relative motion within {POINTER_TIMEOUT:?}"
	);
	let (x, y) = *started.client.snapshot().pointer_motion.last().expect("pointer motion");
	assert!(
		(x - 120.0).abs() <= 1.0 && (y - 80.0).abs() <= 1.0,
		"the confined cursor was delivered at ({x}, {y}), expected (120, 80)"
	);
	assert_eq!(started.client.snapshot().unconfined, 0, "the capture was released");

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A confined pointer survives a full relative sweep and reaches every surface
/// edge, and each relative delta is delivered **unscaled**.
///
/// This is the confinement/look lifetime assertion for the path under
/// output-fill: while confined, relative motion advances the surface-local
/// cursor in the client's units and must never release the capture or collapse
/// the reachable range. The
/// role is `160x120` on the `320x240` output (fill scale `2.0`); the confine is
/// regionless (Xwayland's shape), so the surface maps the whole output.
#[test]
#[ignore = "requires GPU"]
fn confined_relative_sweep_keeps_the_capture_and_covers_every_edge() {
	let options = ClientOptions {
		buffer_size: Some(SURFACE),
		..Default::default()
	};
	let mut started = support::start(options);

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	thread::sleep(Duration::from_millis(300));
	assert!(
		started.client.command(CommandKind::ConfineSurface),
		"the regionless confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer_v1.confined event within {POINTER_TIMEOUT:?}"
	);

	// Sweep the relative cursor to each edge and past it. A relative step of
	// `40` surface-local units is delivered unchanged; `12` steps cross the
	// surface and the clamp holds.
	for _ in 0..12 {
		support::send_line(&mut started.control, "motion-rel 40 0\n");
	}
	for _ in 0..24 {
		support::send_line(&mut started.control, "motion-rel -40 0\n");
	}
	for _ in 0..12 {
		support::send_line(&mut started.control, "motion-rel 0 40\n");
	}
	for _ in 0..24 {
		support::send_line(&mut started.control, "motion-rel 0 -40\n");
	}

	let log = started.client.snapshot();
	assert_eq!(
		log.unconfined, 0,
		"the confine was released during the sweep: {:?}",
		log.pointer_motion
	);
	let xs: Vec<f64> = log.pointer_motion.iter().map(|point| point.0).collect();
	let ys: Vec<f64> = log.pointer_motion.iter().map(|point| point.1).collect();
	let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
	let max_x = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
	let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min);
	let max_y = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
	assert!(
		min_x <= 1.0 && max_x >= 158.0,
		"the confined x range {min_x}..={max_x} did not cover the surface"
	);
	assert!(
		min_y <= 1.0 && max_y >= 118.0,
		"the confined y range {min_y}..={max_y} did not cover the surface"
	);
	assert!(
		log.relative_motion.iter().all(|&(dx, dy)| {
			((dx.abs() - 40.0).abs() < 0.5 || dx.abs() < 0.5) && ((dy.abs() - 40.0).abs() < 0.5 || dy.abs() < 0.5)
		}),
		"a relative delta was scaled under output-fill: {:?}",
		log.relative_motion
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A scaled role holder owns the whole output even when an unscaled peer below
/// it overlaps a corner.
///
/// `pointer_under` must not pick a peer merely because the peer's *unscaled*
/// scene rectangle contains the output point, which would skip the role's
/// output-fill mapping: a larger peer under a smaller role would steal pointer
/// focus on the top/left edges and make look and confine asymmetric.
///
/// The peer here is a transient left behind after its parent (the first role
/// holder) is destroyed; a fresh toplevel then takes the role and commits a
/// `160x120` buffer, so it is scaled `2x` to fill the `320x240` output. Output
/// `(100,100)` lies inside the peer's unscaled rect at `(0,0)` and must reach
/// the role at surface-local `(50,50)`; a focus flip to the peer would deliver
/// `(100,100)`.
#[test]
#[ignore = "requires GPU"]
fn scaled_role_keeps_pointer_focus_over_a_shifted_peer() {
	let options = ClientOptions {
		with_child: true,
		buffer_size: Some(SURFACE),
	};
	let mut started = support::start(options);

	assert!(
		started.client.command(support::CommandKind::DestroyParent),
		"the destroy-parent command was not serviced"
	);
	// Let the compositor's per-tick liveness check release the window role
	// before the fresh toplevel maps, or the fresh one would be a peer.
	thread::sleep(Duration::from_millis(700));
	assert!(
		started.client.command(support::CommandKind::CreateSecondToplevel),
		"the create-second command was not serviced"
	);
	thread::sleep(Duration::from_millis(300));

	support::send_line(&mut started.control, "motion-abs 100 100\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_motion
			.last()
			.is_some_and(|&(x, y)| (x - 50.0).abs() <= 1.0 && (y - 50.0).abs() <= 1.0)),
		"output (100,100) did not reach the scaled role at (50,50); motions: {:?}",
		started.client.snapshot().pointer_motion
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A `160x160` role on the `320x240` output leaves a `40px` pillarbox on each
/// side (fill scale `1.5`), so an output-clamped cursor (`0..319`) is
/// distinguishable from a content-clamped one (`40..279`).
const PILLARBOX_SURFACE: (i32, i32) = (160, 160);
/// The rendered content rect's inclusive output-space x bounds.
const CONTENT_X_MIN: i32 = 40;
/// The rendered content rect's inclusive right edge (`offset + source*scale - 1`).
const CONTENT_X_MAX: i32 = 279;

/// Without an active pointer constraint the cursor clamps to the role holder's
/// rendered content rect, not the whole output.
///
/// ferriscope clamps the unconstrained cursor to the output rect. When the role
/// holder is fit-scaled its rendered content is narrower than the
/// output, so the cursor could drift into the pillarbox/letterbox bars once a
/// clip is released (`capture=None`). gamescope clamps to the focused window's
/// extent (`wlserver_clampcursor`), keeping the cursor on the game.
///
/// The role commits a `160x160` buffer on the `320x240` output (fill `1.5`,
/// `40px` pillarbox each side). A client cursor image is set, then absolute
/// motion is injected past each side; the composited cursor's changed-pixel
/// bounding box must stay inside `40..279`, not reach `0`/`319`.
#[test]
#[ignore = "requires GPU"]
fn unconstrained_cursor_clamps_to_the_scaled_role_content_rect() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let options = ClientOptions {
		buffer_size: Some(PILLARBOX_SURFACE),
		..Default::default()
	};
	let mut started = support::start_with_record(options, Some(record_dir.clone()), Some(RECORD_LIMIT));

	// No cursor image yet: this is the no-cursor baseline the cursor diff is
	// measured against.
	let baseline = wait_for_stable_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no stable mapped frame within {MAPPED_TIMEOUT:?}"));

	// Enter the role, then give it a cursor image so the compositor-drawn
	// cursor is observable in the recorded frames.
	support::send_line(&mut started.control, "motion-abs 160 120\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| !log.pointer_enter.is_empty()),
		"the role never received pointer focus"
	);
	assert!(
		started.client.command(CommandKind::SetCursor),
		"the set-cursor command was not serviced"
	);

	// Centre: the blob is near the injected point, so the observation is valid
	// before the edge cases.
	let centered = wait_for_cursor_left_edge(&record_dir, &baseline, (150, 175), CURSOR_TIMEOUT)
		.unwrap_or_else(|| panic!("no centred cursor frame within {CURSOR_TIMEOUT:?}"));
	let centered_bbox = diff_bbox(&baseline, &centered).expect("centred cursor blob");
	assert!(
		centered_bbox.0 <= 175,
		"the cursor did not follow the centred motion: blob {:?}",
		centered_bbox
	);

	// Left: without the content clamp the cursor sits at output x≈2 (blob
	// `2..17`); with it, at the content's left edge x=40.
	support::send_line(&mut started.control, "motion-abs 2 120\n");
	let left = wait_for_cursor_left_edge(
		&record_dir,
		&baseline,
		(CONTENT_X_MIN - 2, CONTENT_X_MIN + 20),
		CURSOR_TIMEOUT,
	)
	.unwrap_or_else(|| {
		let (min_x, _, max_x, _) =
			diff_bbox(&baseline, &read_latest(&record_dir).expect("a frame")).unwrap_or((0, 0, 0, 0));
		panic!(
			"the unconstrained cursor entered the left pillarbox: blob x {min_x}..{max_x} \
				 (content starts at {CONTENT_X_MIN})"
		)
	});
	let left_bbox = diff_bbox(&baseline, &left).expect("left cursor blob");
	assert!(
		left_bbox.0 >= CONTENT_X_MIN - 2,
		"the unconstrained cursor entered the left pillarbox: blob {:?} (content starts at {CONTENT_X_MIN})",
		left_bbox
	);

	// Right: clamp to the content's right edge x=279, not the output edge 319.
	support::send_line(&mut started.control, "motion-abs 318 120\n");
	let right = wait_for_cursor_left_edge(
		&record_dir,
		&baseline,
		(CONTENT_X_MAX - 12, CONTENT_X_MAX + 12),
		CURSOR_TIMEOUT,
	)
	.unwrap_or_else(|| {
		let (min_x, _, max_x, _) =
			diff_bbox(&baseline, &read_latest(&record_dir).expect("a frame")).unwrap_or((0, 0, 0, 0));
		panic!(
			"the unconstrained cursor entered the right pillarbox: blob x {min_x}..{max_x} \
					 (content ends at {CONTENT_X_MAX})"
		)
	});
	let right_bbox = diff_bbox(&baseline, &right).expect("right cursor blob");
	assert!(
		right_bbox.1 <= CONTENT_X_MAX + 16,
		"the unconstrained cursor entered the right pillarbox: blob {:?} (content ends at {CONTENT_X_MAX})",
		right_bbox
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// An unconstrained relative motion advances the composited cursor, matching
/// gamescope, instead of leaving the drawn cursor where it was.
///
/// Moonlight's default mouse mode is relative and Steam Big Picture is
/// unconstrained. Suppressing the absolute cursor advance on a relative event
/// left the drawn cursor stuck at its initial position while
/// Xwayland's own X pointer moved through `zwp_relative_pointer`, so clicks
/// landed correctly but the composited cursor never moved and hover never
/// updated. gamescope advances the surface-local cursor and notifies the
/// absolute position on the same event (`wlserver_mousemotion`); the role here
/// fills the output (identity fill), so the drawn cursor must track the
/// accumulated delta. Before the fix no frame follows the relative event at
/// all, so the wait times out.
#[test]
#[ignore = "requires GPU"]
fn unconstrained_relative_motion_advances_the_drawn_cursor() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let mut started =
		support::start_with_record(ClientOptions::default(), Some(record_dir.clone()), Some(RECORD_LIMIT));

	let baseline = wait_for_stable_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no stable mapped frame within {MAPPED_TIMEOUT:?}"));

	// Place the pointer and give the role a cursor image so the composited
	// cursor is observable in the recorded frames.
	support::send_line(&mut started.control, "motion-abs 100 100\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| !log.pointer_enter.is_empty()),
		"the role never received pointer focus"
	);
	assert!(
		started.client.command(CommandKind::SetCursor),
		"the set-cursor command was not serviced"
	);
	assert!(
		wait_for_cursor_left_edge(&record_dir, &baseline, (95, 120), CURSOR_TIMEOUT).is_some(),
		"the placed cursor was not drawn within {CURSOR_TIMEOUT:?}"
	);

	// A relative event with no constraint must move the drawn cursor by the
	// delta (identity fill): (100, 100) + (60, 40) = (160, 140).
	assert!(
		started.client.command(CommandKind::RelativePointer),
		"the relative pointer command was not serviced"
	);
	support::send_line(&mut started.control, "motion-rel 60 40\n");
	let moved = wait_for_cursor_left_edge(&record_dir, &baseline, (155, 185), CURSOR_TIMEOUT).unwrap_or_else(|| {
		let bbox = diff_bbox(&baseline, &read_latest(&record_dir).expect("a frame")).unwrap_or((0, 0, 0, 0));
		panic!("the unconstrained cursor did not follow the relative motion: blob {bbox:?}")
	});
	let bbox = diff_bbox(&baseline, &moved).expect("moved cursor blob");
	assert!(
		bbox.0 >= 140,
		"the cursor did not advance on relative motion: blob {bbox:?}"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// An unconstrained **relative** sweep clamps to the role holder's rendered
/// content rect, not the output.
///
/// Off-output accumulation would come from relative events with
/// `capture == None` advancing an unbounded cursor. The unconstrained advance
/// is enabled again for gamescope parity, so it must keep the clamp: a
/// large relative delta past either pillarbox edge stops at the content rect
/// and the drawn cursor never enters the bars.
///
/// The role commits a `160x160` buffer on the `320x240` output (fill `1.5`,
/// `40px` pillarbox each side); relative deltas far past each edge must clamp
/// the composited cursor blob inside `40..279`.
#[test]
#[ignore = "requires GPU"]
fn unconstrained_relative_motion_clamps_to_the_scaled_role_content_rect() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let options = ClientOptions {
		buffer_size: Some(PILLARBOX_SURFACE),
		..Default::default()
	};
	let mut started = support::start_with_record(options, Some(record_dir.clone()), Some(RECORD_LIMIT));

	let baseline = wait_for_stable_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no stable mapped frame within {MAPPED_TIMEOUT:?}"));

	support::send_line(&mut started.control, "motion-abs 160 120\n");
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| !log.pointer_enter.is_empty()),
		"the role never received pointer focus"
	);
	assert!(
		started.client.command(CommandKind::SetCursor),
		"the set-cursor command was not serviced"
	);
	assert!(
		started.client.command(CommandKind::RelativePointer),
		"the relative pointer command was not serviced"
	);
	assert!(
		wait_for_cursor_left_edge(&record_dir, &baseline, (150, 175), CURSOR_TIMEOUT).is_some(),
		"the placed cursor was not drawn within {CURSOR_TIMEOUT:?}"
	);

	// A relative delta far past the left edge clamps to the content's left edge
	// (x=40), not the output edge (x=0).
	support::send_line(&mut started.control, "motion-rel -400 0\n");
	let left = wait_for_cursor_left_edge(
		&record_dir,
		&baseline,
		(CONTENT_X_MIN - 2, CONTENT_X_MIN + 20),
		CURSOR_TIMEOUT,
	)
	.unwrap_or_else(|| {
		let (min_x, _, max_x, _) =
			diff_bbox(&baseline, &read_latest(&record_dir).expect("a frame")).unwrap_or((0, 0, 0, 0));
		panic!("the unconstrained relative cursor entered the left pillarbox: blob x {min_x}..{max_x}");
	});
	let left_bbox = diff_bbox(&baseline, &left).expect("left cursor blob");
	assert!(
		left_bbox.0 >= CONTENT_X_MIN - 2,
		"the unconstrained relative cursor entered the left pillarbox: blob {left_bbox:?}"
	);

	// A relative delta far past the right edge clamps to the content's right
	// edge (x=279), not the output edge (x=319).
	support::send_line(&mut started.control, "motion-rel 400 0\n");
	let right = wait_for_cursor_left_edge(
		&record_dir,
		&baseline,
		(CONTENT_X_MAX - 12, CONTENT_X_MAX + 12),
		CURSOR_TIMEOUT,
	)
	.unwrap_or_else(|| {
		let (min_x, _, max_x, _) =
			diff_bbox(&baseline, &read_latest(&record_dir).expect("a frame")).unwrap_or((0, 0, 0, 0));
		panic!("the unconstrained relative cursor entered the right pillarbox: blob x {min_x}..{max_x}");
	});
	let right_bbox = diff_bbox(&baseline, &right).expect("right cursor blob");
	assert!(
		right_bbox.2 <= CONTENT_X_MAX + 16,
		"the unconstrained relative cursor entered the right pillarbox: blob {right_bbox:?}"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// Read and decode the newest recorded frame, if any.
fn read_latest(dir: &Path) -> Option<Ppm> {
	latest_sequence(dir).and_then(|sequence| read_frame(dir, sequence))
}
