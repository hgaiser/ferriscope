//! GPU integration test for the cursor gate.
//!
//! The gate for this unit: *a client-supplied cursor image is software
//! composited into the frame.* There is no hardware cursor plane, so
//! the only way to observe it is in the composited output.
//!
//! The test is `#[ignore]`d: it needs a render node, Xwayland and `xev`. Run it
//! alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test cursor_composited -- --ignored --exact cursor_image_is_composited --nocapture
//! ```
//!
//! The standalone `run` path is used with a file recorder, so the frames can be
//! read back as PPM with no encoder attached. The chain under test:
//!
//! - `xev` (an X11 client under Xwayland) creates a window; the xwm maps it
//!   fullscreen.
//! - The compositor's pointer is injected from the control stream as
//!   `motion-abs`, which sends `wl_pointer.enter`/motion to Xwayland.
//! - Xwayland loads its Xcursor image and calls `wl_pointer.set_cursor`, which
//!   reaches `SeatHandler::cursor_image` as a `CursorImageStatus::Surface`.
//! - The next composites must contain that image, so a frame recorded after the
//!   motion differs from the baseline frame — and only around the pointer.
//!
//! The strongest assertion available without extra tooling is localisation: the
//! changed pixels are confined to a box around the injected pointer position, so
//! the change cannot be a full-window redraw being mistaken for a cursor.

use std::{
	io::Write,
	os::unix::net::{UnixListener, UnixStream},
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};

use ferriscope_core::{CompositorConfig, Result, run};

/// Output width, small enough that the software readback keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Injected pointer position; inside the fullscreen window and away from edges.
const POINTER_X: f64 = 160.0;
/// Injected pointer position. Deliberately off the vertical centre: a
/// vertically flipped readback would move the cursor to `HEIGHT - POINTER_Y`,
/// outside [`CURSOR_HALF_BOX`], so the localisation check also pins
/// orientation. At exactly the centre the flip would be invisible.
const POINTER_Y: f64 = 60.0;
/// Half-size of the box a cursor may affect, generous for a themed cursor
/// (up to 64x64 plus a hotspot offset), yet far smaller than a full redraw.
const CURSOR_HALF_BOX: i32 = 80;
/// Fraction of non-black pixels that means "the window is mapped".
const MAPPED_PIXEL_DIVISOR: usize = 100;
/// Budget for the compositor to connect to our control listener.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the window to map and the frame to settle.
const MAPPED_TIMEOUT: Duration = Duration::from_secs(40);
/// Budget for a cursor-bearing frame to appear after the motion.
const CURSOR_TIMEOUT: Duration = Duration::from_secs(20);
/// Watchdog budget for the compositor thread to exit once `xev` is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll slice while waiting on frames or a connection.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Stop writing PPMs after this many frames; bounds disk use.
const RECORD_LIMIT: u64 = 1500;
/// Hard safety exit for the compositor in case cleanup does not stop it.
const STOP_AFTER_FRAMES: u64 = 3000;

/// Kill processes and remove temp state on drop, so a failed assertion does not
/// leak an `xev`, an `Xwayland`, a socket or a frame directory.
struct CursorGuard {
	control_path: PathBuf,
	record_dir: PathBuf,
	frames_read: usize,
}

impl CursorGuard {
	/// Kill the client and the X server, then remove the socket and frames.
	///
	/// [`kill_exact`] only matches our own children by exact name; the
	/// compositor notices its `xev` child exiting and stops.
	fn cleanup(&mut self) {
		kill_exact("xev");
		kill_exact("Xwayland");
		let _ = std::fs::remove_file(&self.control_path);
		let _ = std::fs::remove_dir_all(&self.record_dir);
	}
}

impl Drop for CursorGuard {
	fn drop(&mut self) {
		self.cleanup();
	}
}

/// Kill this test process's children with the exact name `name`.
///
/// `-x` forbids substring matches and `-P` limits the kill to our own
/// children, so concurrent runs (other test binaries, a desktop session) keep
/// their own Xwayland and clients.
fn kill_exact(name: &str) {
	let parent = std::process::id().to_string();
	let _ = Command::new("pkill")
		.args(["-9", "-x", "-P", &parent, name])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status();
}

/// A unique control-socket path under the temp dir.
fn unique_control_path() -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-cursor-{}-{nanos}.sock", std::process::id()))
}

/// A unique frame-recording directory under the temp dir.
fn unique_record_dir() -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-cursor-frames-{}-{nanos}", std::process::id()))
}

/// Accept the compositor's control connection, bounded.
fn accept_control(listener: &UnixListener) -> UnixStream {
	listener.set_nonblocking(true).expect("listener nonblocking");
	let deadline = Instant::now() + ACCEPT_TIMEOUT;
	loop {
		match listener.accept() {
			Ok((stream, _)) => return stream,
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
			Err(e) => panic!("accept control connection: {e}"),
		}
		assert!(
			Instant::now() < deadline,
			"compositor did not connect to the control socket within {ACCEPT_TIMEOUT:?}"
		);
		thread::sleep(POLL_SLICE);
	}
}

/// Send one control line, bounded by the caller's socket write.
fn send_line(stream: &mut UnixStream, line: &str) {
	stream
		.write_all(line.as_bytes())
		.unwrap_or_else(|e| panic!("write control line {line:?}: {e}"));
	stream
		.flush()
		.unwrap_or_else(|e| panic!("flush control line {line:?}: {e}"));
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
	// Skip the maxval line ("255") and take the rest as the body.
	let maxval_end = bytes[dims_end + 1..].iter().position(|&b| b == b'\n')? + dims_end + 1;
	let rgb = &bytes[maxval_end + 1..];
	// A frame still being written is short; report it as not-yet-available
	// rather than comparing a truncated body.
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
/// predecessor (so a mapping/redraw in progress is never used as the baseline).
fn wait_for_mapped_frame(dir: &Path, guard: &mut CursorGuard, timeout: Duration) -> Option<(u64, Ppm)> {
	let threshold = WIDTH as usize * HEIGHT as usize / MAPPED_PIXEL_DIVISOR;
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

/// Wait, bounded, for the first frame after `after` that differs from `baseline`.
fn wait_for_cursor_frame(
	dir: &Path,
	guard: &mut CursorGuard,
	after: u64,
	baseline: &Ppm,
	timeout: Duration,
) -> Option<(u64, Ppm)> {
	let deadline = Instant::now() + timeout;
	let mut next = after + 1;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			while next <= latest {
				if let Some(frame) = read_frame(dir, next) {
					guard.frames_read += 1;
					if !diff_positions(baseline, &frame).is_empty() {
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

/// Join the compositor thread under a watchdog, returning whether it exited.
fn join_with_timeout(handle: thread::JoinHandle<Result<()>>, timeout: Duration) -> bool {
	let (tx, rx) = mpsc::channel();
	thread::spawn(move || {
		let _ = tx.send(handle.join());
	});
	rx.recv_timeout(timeout).is_ok()
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn cursor_image_is_composited() {
	let record_dir = unique_record_dir();
	let control_path = unique_control_path();
	let _ = std::fs::remove_file(&control_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let listener = UnixListener::bind(&control_path).expect("bind control listener");

	let mut guard = CursorGuard {
		control_path: control_path.clone(),
		record_dir: record_dir.clone(),
		frames_read: 0,
	};

	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		// The shell supplies `DISPLAY` once Xwayland is ready, so the test does
		// not need to know the display number.
		command: Some(vec!["xev".to_string()]),
		control_socket: Some(control_path.clone()),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));
	let mut control = accept_control(&listener);

	// Baseline: the xev window is mapped and static, and no pointer has entered
	// it yet, so there is no cursor image.
	let (baseline_sequence, baseline) = wait_for_mapped_frame(&record_dir, &mut guard, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black frame within {MAPPED_TIMEOUT:?}"));

	// Injecting absolute motion sends `wl_pointer.enter` to Xwayland, which then
	// supplies its Xcursor image through `wl_pointer.set_cursor`.
	send_line(&mut control, &format!("motion-abs {POINTER_X} {POINTER_Y}\n"));

	let (_cursor_sequence, cursor_frame) =
		wait_for_cursor_frame(&record_dir, &mut guard, baseline_sequence, &baseline, CURSOR_TIMEOUT)
			.unwrap_or_else(|| panic!("no frame changed after the pointer motion within {CURSOR_TIMEOUT:?}"));

	let diffs = diff_positions(&baseline, &cursor_frame);
	assert!(
		!diffs.is_empty(),
		"the composited frame did not change after motion; the cursor image was not drawn"
	);

	// Localisation: a cursor changes only its own pixels. Any difference outside
	// the box means the window redrew, which would invalidate the observation.
	let outside: Vec<(i32, i32)> = diffs.iter().copied().filter(|(x, y)| !in_cursor_box(*x, *y)).collect();
	assert!(
		outside.is_empty(),
		"{} of {} changed pixels were outside the cursor box (e.g. {:?}); the change is not the cursor",
		outside.len(),
		diffs.len(),
		outside.first()
	);

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
	eprintln!("cursor test: read {} recorded frames", guard.frames_read);
}

/// Idle time after which the test compositor hides the cursor.
const HIDE_TIME: Duration = Duration::from_millis(2500);

/// Wait, bounded, for the first frame after `after` that equals `baseline`.
fn wait_for_baseline_frame(dir: &Path, after: u64, baseline: &Ppm, timeout: Duration) -> Option<u64> {
	let deadline = Instant::now() + timeout;
	let mut next = after + 1;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			while next <= latest {
				if let Some(frame) = read_frame(dir, next) {
					if diff_positions(baseline, &frame).is_empty() {
						return Some(next);
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

/// The cursor disappears after `cursor_hide_time` without motion
/// (`steamcompmgr.cpp:2013`), is not composited while hidden, and reappears on
/// the next motion.
#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn cursor_hides_when_idle_and_returns_on_motion() {
	let record_dir = unique_record_dir();
	let control_path = unique_control_path();
	let _ = std::fs::remove_file(&control_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let listener = UnixListener::bind(&control_path).expect("bind control listener");
	let mut guard = CursorGuard {
		control_path: control_path.clone(),
		record_dir: record_dir.clone(),
		frames_read: 0,
	};
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["xev".to_string()]),
		control_socket: Some(control_path.clone()),
		seat: ferriscope_core::SeatConfig {
			cursor_hide_time: HIDE_TIME,
			..ferriscope_core::SeatConfig::default()
		},
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};
	let runner = thread::spawn(move || run(config));
	let mut control = accept_control(&listener);

	let (baseline_sequence, baseline) = wait_for_mapped_frame(&record_dir, &mut guard, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black frame within {MAPPED_TIMEOUT:?}"));

	send_line(&mut control, &format!("motion-abs {POINTER_X} {POINTER_Y}\n"));
	let moved_at = Instant::now();
	let (cursor_sequence, _) =
		wait_for_cursor_frame(&record_dir, &mut guard, baseline_sequence, &baseline, CURSOR_TIMEOUT)
			.expect("the cursor is drawn after motion");

	let hidden_sequence = wait_for_baseline_frame(&record_dir, cursor_sequence, &baseline, HIDE_TIME * 4)
		.expect("the cursor disappears after the idle time");
	assert!(
		moved_at.elapsed() >= HIDE_TIME,
		"the cursor hid after {:?}, before the {HIDE_TIME:?} idle time",
		moved_at.elapsed()
	);

	// Hidden: every later frame (keepalives) stays cursor-free.
	thread::sleep(Duration::from_millis(1500));
	let latest = latest_sequence(&record_dir).expect("frames exist");
	for sequence in hidden_sequence..=latest {
		if let Some(frame) = read_frame(&record_dir, sequence) {
			assert!(
				diff_positions(&baseline, &frame).is_empty(),
				"frame {sequence} shows a cursor while it is hidden"
			);
		}
	}

	// Motion shows it again.
	send_line(
		&mut control,
		&format!("motion-abs {} {}\n", POINTER_X + 20.0, POINTER_Y),
	);
	wait_for_cursor_frame(&record_dir, &mut guard, latest, &baseline, CURSOR_TIMEOUT)
		.expect("the cursor returns after motion");

	kill_exact("xev");
	assert!(join_with_timeout(runner, STOP_TIMEOUT), "compositor did not stop");
	guard.cleanup();
}
