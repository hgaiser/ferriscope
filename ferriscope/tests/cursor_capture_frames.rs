//! GPU integration test for frame-level cursor survival across pointer capture.
//!
//! gamescope-parity rule: the cursor is drawn **iff** the client has
//! set an image and has not hidden it. Pointer capture — lock or confine —
//! affects position only and must never remove a client's cursor. This is the
//! frame-level evidence: while the pointer is **locked** the cursor must stay in
//! the composited frame, exactly as a **confined** pointer keeps it.
//!
//! This drives a native Wayland client (reusing the shared harness in
//! `tests/support/`) against the in-process `Compositor::launch` path with
//! `record_dir` set, so the frames come back as PPM through the existing
//! readback path while the `Ready` handshake still tells the client which
//! dynamic `wayland-N` socket to use. The chain:
//!
//! - The client maps one fullscreen toplevel (a solid opaque fill) and, on
//!   `wl_pointer.enter`, sets a solid-colour cursor image with
//!   `wl_pointer.set_cursor` using the enter serial.
//! - Baseline (no cursor) vs cursor frames differ only in a box around the
//!   injected pointer; that is the "composited cursor" reference.
//! - A persistent `zwp_locked_pointer_v1` (the mouse-look path the compositor
//!   wires in `shell.rs`) must **not** remove the cursor: every frame composed
//!   while the lock is active stays distinct from the no-cursor baseline and
//!   differs only inside the cursor box.
//!
//! The test is `#[ignore]`d: it needs a render node. Run it alone under a hard
//! timeout:
//!
//! ```text
//! timeout 180 cargo test -p ferriscope --test cursor_capture_frames -- --ignored --exact cursor_survives_pointer_lock --nocapture
//! ```
//!
//! Determinism: the base window never redraws, so a lock that hid the cursor
//! would produce a frame byte-identical to the baseline; the cursor frames
//! differ only inside [`CURSOR_HALF_BOX`]. The assertion is localisation with a
//! generous box rather than exact cursor pixels, so a themed/hotspot-offset
//! image cannot fail it for the wrong reason.

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
/// Injected pointer position, inside the window and away from the edges.
const POINTER_X: f64 = 160.0;
/// Injected pointer position. Off the vertical centre so a vertically flipped
/// readback would move the cursor to `HEIGHT - POINTER_Y`, outside the box.
const POINTER_Y: f64 = 60.0;
/// Half-size of the box the cursor may change. Generous for a 16x16 image plus
/// a hotspot offset, yet far smaller than a full redraw (`WIDTH/4`).
const CURSOR_HALF_BOX: i32 = 40;
/// Fraction of non-black pixels that means "the window is mapped".
const MAPPED_PIXEL_DIVISOR: usize = 100;
/// Stop writing after this many frames; bounds disk use on tmpfs.
const RECORD_LIMIT: u64 = 1500;
/// Budget for the window to map and a stable baseline to appear.
const MAPPED_TIMEOUT: Duration = Duration::from_secs(40);
/// Budget for the window to receive pointer focus.
const POINTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a cursor-bearing frame to appear after the image is set.
const CURSOR_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for a cursor-or-moving frame to appear after a confine/motion.
const SUPPRESS_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a frame composed *while a lock is active* to appear. Must exceed
/// the one-second static keepalive so the lock state is observable even though
/// the locked pointer cannot be moved.
const LOCK_FRAME_TIMEOUT: Duration = Duration::from_millis(2500);
/// Poll slice while waiting on frames.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Settle after the baseline before observing an idle scene: lets any startup
/// commit still in flight land.
const IDLE_SETTLE: Duration = Duration::from_millis(300);
/// Idle observation window; kept below the one-second static keepalive so a
/// keepalive frame cannot be mistaken for a duplicate.
const IDLE_WINDOW: Duration = Duration::from_millis(250);

/// Removes the recorded frames on drop, so a failed assertion leaks nothing.
struct FramesGuard {
	record_dir: PathBuf,
	frames_read: usize,
}

impl FramesGuard {
	fn cleanup(&mut self) {
		let _ = std::fs::remove_dir_all(&self.record_dir);
	}
}

impl Drop for FramesGuard {
	fn drop(&mut self) {
		self.cleanup();
	}
}

/// A unique frame-recording directory under the temp dir.
fn unique_record_dir() -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-cursor-capture-{}-{nanos}", std::process::id()))
}

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
	// A frame still being written is short; report it as not-yet-available.
	if rgb.len() < width * height * 3 {
		return None;
	}
	Some(Ppm {
		width,
		height,
		rgb: rgb.to_vec(),
	})
}

/// The path of a recorded frame.
fn frame_path(dir: &Path, sequence: u64) -> PathBuf {
	dir.join(format!("frame_{sequence:06}.ppm"))
}

/// Read and decode a recorded frame, or `None` if it is not written yet.
fn read_frame(dir: &Path, sequence: u64) -> Option<Ppm> {
	let bytes = std::fs::read(frame_path(dir, sequence)).ok()?;
	parse_ppm(&bytes)
}

/// The highest recorded sequence number so far, or `None` if no frame exists.
fn latest_sequence(dir: &Path) -> Option<u64> {
	let entries = std::fs::read_dir(dir).ok()?;
	let mut latest: Option<u64> = None;
	for entry in entries.flatten() {
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

/// Count pixels whose RGB is not all zero.
fn non_black_pixels(ppm: &Ppm) -> usize {
	ppm.rgb.chunks_exact(3).filter(|pixel| pixel != &[0, 0, 0]).count()
}

/// Positions (x, y) where two same-sized frames differ.
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

/// Whether `(x, y)` is inside the box a cursor could cover.
fn in_cursor_box(x: i32, y: i32) -> bool {
	(x - POINTER_X as i32).abs() <= CURSOR_HALF_BOX && (y - POINTER_Y as i32).abs() <= CURSOR_HALF_BOX
}

/// Wait, bounded, for a stable mapped window: a non-black frame identical to its
/// predecessor, so a mapping/redraw in progress is never the baseline.
fn wait_for_stable_frame(dir: &Path, guard: &mut FramesGuard, timeout: Duration) -> Option<(u64, Ppm)> {
	let threshold = WIDTH * HEIGHT / MAPPED_PIXEL_DIVISOR;
	let deadline = Instant::now() + timeout;
	loop {
		if Instant::now() >= deadline {
			return None;
		}
		if let Some(sequence) = latest_sequence(dir) {
			if let Some(frame) = read_frame(dir, sequence) {
				guard.frames_read += 1;
				if non_black_pixels(&frame) > threshold {
					if let Some(previous) = read_frame(dir, sequence.saturating_sub(1)) {
						guard.frames_read += 1;
						if diff_positions(&previous, &frame).is_empty() {
							return Some((sequence, frame));
						}
					}
				}
			}
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for the first frame after `after` whose pixel diff from
/// `reference` is non-empty.
fn wait_for_diff_frame(
	dir: &Path,
	guard: &mut FramesGuard,
	after: u64,
	reference: &Ppm,
	timeout: Duration,
) -> Option<(u64, Ppm)> {
	let deadline = Instant::now() + timeout;
	let mut next = after + 1;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			while next <= latest {
				if let Some(frame) = read_frame(dir, next) {
					guard.frames_read += 1;
					if !diff_positions(reference, &frame).is_empty() {
						return Some((next, frame));
					}
				}
				next += 1;
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for a frame whose sequence is greater than `after`,
/// returning the newest such frame.
///
/// Used to observe a scene composed *after* a state change whose only effect is
/// the absence of a recompose trigger (a lock on a static cursor).
fn wait_for_frame_after(dir: &Path, guard: &mut FramesGuard, after: u64, timeout: Duration) -> Option<(u64, Ppm)> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			if latest > after {
				if let Some(frame) = read_frame(dir, latest) {
					guard.frames_read += 1;
					return Some((latest, frame));
				}
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for the **newest** frame to differ from `reference` and to
/// have been stable for one tick (so an in-flight redraw is not read).
///
/// Unlike [`wait_for_diff_frame`], this returns the settled frame rather than
/// the first transitional one, which matters when a motion crosses several
/// positions: the first differing frame may still show a previous position.
fn wait_for_settled_diff(
	dir: &Path,
	guard: &mut FramesGuard,
	reference: &Ppm,
	timeout: Duration,
) -> Option<(u64, Ppm)> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			if let Some(frame) = read_frame(dir, latest) {
				guard.frames_read += 1;
				if !diff_positions(reference, &frame).is_empty() {
					// Require the newest frame to be the last written so a frame
					// mid-motion is not mistaken for the settled one.
					if latest_sequence(dir) == Some(latest) {
						return Some((latest, frame));
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

/// The cursor is composited and **stays composited** across a pointer lock: a
/// lock never hides a client cursor.
#[test]
#[ignore = "requires GPU; native Wayland client and in-process recorder"]
fn cursor_survives_pointer_lock() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let mut guard = FramesGuard {
		record_dir: record_dir.clone(),
		frames_read: 0,
	};

	let mut started =
		support::start_with_record(ClientOptions::default(), Some(record_dir.clone()), Some(RECORD_LIMIT));
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// 1. Baseline: the window is mapped and static, no pointer has entered it
	//    yet, so no cursor image is drawn. This is the no-cursor reference.
	let (baseline_sequence, baseline) = wait_for_stable_frame(&record_dir, &mut guard, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black baseline within {MAPPED_TIMEOUT:?}"));

	// 2. Pointer enters the window, then the client sets a cursor image with the
	//    enter serial. The next frames must change only in the cursor box.
	support::send_line(&mut started.control, &format!("motion-abs {POINTER_X} {POINTER_Y}\n"));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);
	assert!(
		started.client.command(CommandKind::SetCursor),
		"the set-cursor command was not serviced"
	);
	let (cursor_sequence, cursor_frame) =
		wait_for_diff_frame(&record_dir, &mut guard, baseline_sequence, &baseline, CURSOR_TIMEOUT)
			.unwrap_or_else(|| panic!("no cursor-bearing frame within {CURSOR_TIMEOUT:?}"));
	let cursor_diffs = diff_positions(&baseline, &cursor_frame);
	assert!(
		!cursor_diffs.is_empty(),
		"the frame did not change after the cursor image was set; it was not composited"
	);
	let cursor_outside: Vec<(i32, i32)> = cursor_diffs
		.iter()
		.copied()
		.filter(|(x, y)| !in_cursor_box(*x, *y))
		.collect();
	assert!(
		cursor_outside.is_empty(),
		"{} of {} cursor-frame changes were outside the cursor box (e.g. {:?}); the window redrew",
		cursor_outside.len(),
		cursor_diffs.len(),
		cursor_outside.first()
	);

	// 3. Pointer capture (lock, the mouse-look path). The lock affects position
	//    only: the cursor must stay drawn. A locked pointer cannot be moved, so
	//    wait past the one-second static keepalive for a frame composed with the
	//    lock active rather than re-reading the pre-lock cursor frame.
	assert!(
		started.client.command(CommandKind::LockRelative),
		"the lock command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.locked >= 1),
		"no zwp_locked_pointer.locked event within {POINTER_TIMEOUT:?}"
	);
	let after_lock = latest_sequence(&record_dir).expect("a frame exists");
	let (locked_sequence, locked_frame) = wait_for_frame_after(&record_dir, &mut guard, after_lock, LOCK_FRAME_TIMEOUT)
		.unwrap_or_else(|| panic!("no frame composed while the lock was active within {LOCK_FRAME_TIMEOUT:?}"));

	// No frame while the lock was active may revert to the no-cursor baseline.
	for sequence in (cursor_sequence + 1)..=locked_sequence {
		let Some(frame) = read_frame(&record_dir, sequence) else {
			continue;
		};
		guard.frames_read += 1;
		assert!(
			!diff_positions(&baseline, &frame).is_empty(),
			"frame #{sequence} reverted to the no-cursor baseline while the pointer was locked"
		);
	}
	let locked_diffs = diff_positions(&baseline, &locked_frame);
	assert!(!locked_diffs.is_empty(), "the cursor vanished under a locked pointer");
	let locked_outside: Vec<(i32, i32)> = locked_diffs
		.iter()
		.copied()
		.filter(|(x, y)| !in_cursor_box(*x, *y))
		.collect();
	assert!(
		locked_outside.is_empty(),
		"{} of {} locked-frame changes were outside the cursor box (e.g. {:?}); the window redrew",
		locked_outside.len(),
		locked_diffs.len(),
		locked_outside.first()
	);

	eprintln!(
		"cursor lock frames: baseline #{baseline_sequence} ({} non-black), cursor #{cursor_sequence} ({} changed), \
		 locked #{locked_sequence} ({} changed); read {} frames",
		non_black_pixels(&baseline),
		cursor_diffs.len(),
		locked_diffs.len(),
		guard.frames_read
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
	guard.cleanup();
}

/// A confined pointer keeps the cursor.
///
/// This is the frame-level regression, now subsumed by 's
/// "capture never hides" rule. Xwayland confines the pointer on an X confine-to
/// grab (`xwayland-input.c:3570`) — Wine does this for a game that merely clips
/// the mouse to its window (Stick Fight's main menu). gamescope never ties
/// cursor visibility to a confine (`wlserver.cpp:2944-2947` only sets the
/// region), so ferriscope must keep drawing the client cursor. Before the fix
/// `suppresses_cursor` returned true for `Confined` too, and the cursor vanished
/// while it was still interactive.
///
/// The client sets a solid cursor image, then installs the exact regionless
/// confine Xwayland uses and moves the pointer to force a recompose; the new
/// frame must still carry the cursor, and only inside the cursor box. If the
/// cursor were hidden the confined frame would be byte-identical to the
/// no-cursor baseline and no differing frame would appear.
#[test]
#[ignore = "requires GPU; native Wayland client and in-process recorder"]
fn cursor_survives_pointer_confine() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let mut guard = FramesGuard {
		record_dir: record_dir.clone(),
		frames_read: 0,
	};

	let mut started =
		support::start_with_record(ClientOptions::default(), Some(record_dir.clone()), Some(RECORD_LIMIT));
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	let (baseline_sequence, baseline) = wait_for_stable_frame(&record_dir, &mut guard, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black baseline within {MAPPED_TIMEOUT:?}"));

	// Enter and set the client cursor image; the cursor frame is the reference
	// for "a cursor is drawn".
	support::send_line(&mut started.control, &format!("motion-abs {POINTER_X} {POINTER_Y}\n"));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);
	assert!(
		started.client.command(CommandKind::SetCursor),
		"the set-cursor command was not serviced"
	);
	let (cursor_sequence, cursor_frame) = wait_for_diff_frame(&record_dir, &mut guard, 0, &baseline, CURSOR_TIMEOUT)
		.unwrap_or_else(|| panic!("no cursor-bearing frame within {CURSOR_TIMEOUT:?}"));
	assert!(
		!diff_positions(&baseline, &cursor_frame).is_empty(),
		"the cursor was not composited after set_cursor"
	);

	// Install the regionless confine Xwayland uses for an X confine-to grab.
	assert!(
		started.client.command(CommandKind::ConfineSurface),
		"the confine command was not serviced"
	);
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log.confined >= 1),
		"no zwp_confined_pointer.confined event within {POINTER_TIMEOUT:?}"
	);

	// Force a recompose. The client cursor surface is static, so a confine that
	// (wrongly) hid the cursor would not by itself change the scene signature on
	// the server without a recompose trigger; an absolute pointer move changes
	// `cursor.location` and therefore does. Move to another point inside the
	// confine region: the new frame must carry the cursor, not be
	// byte-identical to the no-cursor baseline.
	let before_confine_move = latest_sequence(&record_dir).expect("a frame exists");
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", POINTER_X - 20.0, POINTER_Y - 20.0),
	);
	let (confined_sequence, confined_frame) = wait_for_diff_frame(
		&record_dir,
		&mut guard,
		before_confine_move,
		&baseline,
		SUPPRESS_TIMEOUT,
	)
	.unwrap_or_else(|| panic!("the cursor vanished under a confine"));
	let confined_diffs = diff_positions(&baseline, &confined_frame);
	assert!(
		!confined_diffs.is_empty(),
		"the cursor is not drawn under a confined pointer"
	);
	let confined_outside: Vec<(i32, i32)> = confined_diffs
		.iter()
		.copied()
		.filter(|(x, y)| !in_cursor_box(*x, *y))
		.collect();
	assert!(
		confined_outside.is_empty(),
		"{} of {} confined-frame changes were outside the cursor box (e.g. {:?}); the window redrew",
		confined_outside.len(),
		confined_diffs.len(),
		confined_outside.first()
	);

	// Releasing the confine must leave the cursor drawn too.
	assert!(
		started.client.command(CommandKind::Unconfine),
		"the unconfine command was not serviced"
	);
	let before_unconfine_move = latest_sequence(&record_dir).expect("a frame exists");
	support::send_line(
		&mut started.control,
		&format!("motion-abs {} {}\n", POINTER_X + 20.0, POINTER_Y + 20.0),
	);
	let (after_sequence, after_frame) = wait_for_diff_frame(
		&record_dir,
		&mut guard,
		before_unconfine_move,
		&baseline,
		CURSOR_TIMEOUT,
	)
	.unwrap_or_else(|| panic!("the cursor vanished after unconfine within {CURSOR_TIMEOUT:?}"));
	assert!(
		!diff_positions(&baseline, &after_frame).is_empty(),
		"the cursor was not drawn after the confine was released"
	);

	eprintln!(
		"cursor confine frames: baseline #{baseline_sequence}, cursor #{cursor_sequence}, confined #{confined_sequence} \
		 ({} changed), after #{after_sequence}; read {} frames",
		confined_diffs.len(),
		guard.frames_read
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
	guard.cleanup();
}

/// The role holder owns the whole output, so the cursor is never erased by a
/// pointer leave at the output edge or in the letterbox.
///
/// Two bugs met here. First, `pointer_under` inverse-mapped an
/// output point onto the role's content with `fill.output_to_content`, but a
/// point on the output's far edge maps exactly onto `source` — the *exclusive*
/// right/bottom edge of the surface hit-test — so the role lost focus on the
/// boundary even though the whole output pixel is drawn from it. Second,
/// Smithay emits `CursorImageStatus::Named(Default)` on every pointer
/// `leave`/`replace`, and ferriscope mapped `Named` to "clear the image", so a
/// client that set a cursor once and does not re-set it on enter (Xwayland
/// re-sets only on its own pointer enter) would lose the cursor permanently
/// after the first leave. The client
/// here commits a square surface that letterboxes on the 4:3 output, then the
/// pointer is driven to a letterbox point and to the far content edge; the
/// cursor must stay drawn at every point.
#[test]
#[ignore = "requires GPU; native Wayland client and in-process recorder"]
fn cursor_survives_pointer_leave_and_reenter() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let mut guard = FramesGuard {
		record_dir: record_dir.clone(),
		frames_read: 0,
	};

	// A square surface on a 4:3 output letterboxes horizontally (scale
	// `min(320/160, 240/160) = 1.5`, content 240x240 centred at x offset 40),
	// so output `x < 40` and `x >= 280` are outside the surface. Under the fill
	// transform the role still owns them.
	let options = ClientOptions {
		buffer_size: Some((160, 160)),
		..Default::default()
	};
	let mut started = support::start_with_record(options, Some(record_dir.clone()), Some(RECORD_LIMIT));
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	let (baseline_sequence, baseline) = wait_for_stable_frame(&record_dir, &mut guard, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black baseline within {MAPPED_TIMEOUT:?}"));

	// Enter the surface, then set the client cursor image.
	support::send_line(&mut started.control, &format!("motion-abs {POINTER_X} {POINTER_Y}\n"));
	assert!(
		support::wait_log(&started.client, POINTER_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the window did not receive pointer focus within {POINTER_TIMEOUT:?}"
	);
	assert!(
		started.client.command(CommandKind::SetCursor),
		"the set-cursor command was not serviced"
	);
	let (cursor_sequence, cursor_frame) = wait_for_settled_diff(&record_dir, &mut guard, &baseline, CURSOR_TIMEOUT)
		.unwrap_or_else(|| panic!("no cursor-bearing frame within {CURSOR_TIMEOUT:?}"));
	assert!(
		!diff_positions(&baseline, &cursor_frame).is_empty(),
		"the cursor was not composited after set_cursor"
	);

	// A letterbox point outside the surface's drawn extent. The role must keep
	// pointer focus and the cursor image, so the settled frame still differs
	// from the no-cursor baseline.
	support::send_line(&mut started.control, "motion-abs 5 120\n");
	let (left_sequence, left_frame) = wait_for_settled_diff(&record_dir, &mut guard, &baseline, CURSOR_TIMEOUT)
		.unwrap_or_else(|| panic!("the cursor vanished at a letterbox point"));
	let left_diffs = diff_positions(&baseline, &left_frame);
	assert!(
		!left_diffs.is_empty(),
		"the cursor must remain drawn while the pointer is in the letterbox"
	);

	// The far content edge: output `x = 280` maps to content `x = 178`, the
	// exclusive edge. The role must still own it.
	support::send_line(&mut started.control, "motion-abs 280 120\n");
	let (edge_sequence, edge_frame) = wait_for_settled_diff(&record_dir, &mut guard, &baseline, CURSOR_TIMEOUT)
		.unwrap_or_else(|| panic!("the cursor vanished at the content's far edge"));
	let edge_diffs = diff_positions(&baseline, &edge_frame);
	assert!(
		!edge_diffs.is_empty(),
		"the cursor must remain drawn at the content's far edge"
	);

	eprintln!(
		"cursor whole-output: baseline #{baseline_sequence} ({} non-black), cursor #{cursor_sequence}, \
		 letterbox #{left_sequence} ({} changed), edge #{edge_sequence} ({} changed); read {} frames",
		non_black_pixels(&baseline),
		left_diffs.len(),
		edge_diffs.len(),
		guard.frames_read
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
	guard.cleanup();
}

/// An unchanged scene must not be re-composited.
///
/// Compositing on every tick whenever a window exists would deliver each frame
/// of a slower client several times over. The cadence gate emits one frame per
/// client commit (plus a one-second static keepalive); this asserts it
/// directly: while the mapped window is idle, the recorder must not gain
/// frames.
#[test]
#[ignore = "requires GPU; native Wayland client and in-process recorder"]
fn idle_scene_is_not_recomposited() {
	let record_dir = unique_record_dir();
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let mut guard = FramesGuard {
		record_dir: record_dir.clone(),
		frames_read: 0,
	};

	let mut started =
		support::start_with_record(ClientOptions::default(), Some(record_dir.clone()), Some(RECORD_LIMIT));
	let _parent = started.client.surface_id(Target::Parent).expect("parent surface id");

	// Mapped and stable: the window's startup commits have settled.
	let (baseline, _frame) = wait_for_stable_frame(&record_dir, &mut guard, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black baseline within {MAPPED_TIMEOUT:?}"));

	// Let any in-flight startup commit land, then watch an idle window shorter
	// than the keepalive. Without the gate the recorder fills at ~60 fps here.
	thread::sleep(IDLE_SETTLE);
	let settled = latest_sequence(&record_dir).expect("a frame exists");
	assert!(settled >= baseline, "the recorded sequence must not go backwards");
	thread::sleep(IDLE_WINDOW);
	let after = latest_sequence(&record_dir).expect("a frame exists");
	assert_eq!(
		after, settled,
		"an unchanged scene was re-composited: sequence advanced {settled} -> {after} over \
		 {IDLE_WINDOW:?} with no client commit"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
	guard.cleanup();
}
