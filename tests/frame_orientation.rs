//! Deterministic frame-orientation test for the file-recorder path.
//!
//! The contract under test: a `FileRecorder` PPM is in **output coordinates**,
//! with the top-left origin — row 0 of the PPM is row 0 of the composited
//! output, left to right is unchanged. An element placed at output `(x, y)`
//! must appear at the same `(x, y)` in the PPM.
//!
//! `TextureMapping::flipped()` must not be used as a flip signal: for the
//! `GlesMapping` that `ExportMem::copy_framebuffer` returns it is a constant
//! `true`, while `glReadPixels` on a target rendered through Smithay's
//! projection already comes out top-down. Trusting it yields a vertically
//! mirrored PPM.
//!
//! The test does not trust any single pixel: it creates a *non-square,
//! non-centred* override-redirect X11 window at a known `(x, y)` and asserts the
//! colour block lands at exactly `(x, y)`. Because the block is neither centred
//! nor square, agreeing on its top-left pins both axes: a vertical flip moves it
//! to `y = HEIGHT - y - h`, a horizontal flip to `x = WIDTH - x - w`, and a 180°
//! rotation to both. The two readback paths are covered separately: the
//! offscreen renderbuffer and the CPU-readback DMA-BUF.
//!
//! It is `#[ignore]`d: it needs a render node, Xwayland and `xev`. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope --test frame_orientation -- --ignored --nocapture
//! ```

use std::{
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};

use ferriscope::{CompositorConfig, Result, run};
use smithay::reexports::x11rb::{
	self, COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, connection::Connection, protocol::xproto::*,
	rust_connection::RustConnection, wrapper::ConnectionExt as _,
};

/// Output width, small enough that the software readback keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// The probe window's top-left in output coordinates, deliberately off-centre
/// and off both axes.
const BLOCK_X: i16 = 30;
/// The probe window's top-left in output coordinates.
const BLOCK_Y: i16 = 40;
/// Probe width; non-square with the height.
const BLOCK_W: u16 = 80;
/// Probe height; non-square with the width.
const BLOCK_H: u16 = 60;
/// A distinctive green: `0xAARRGGBB`. Chosen so a single-channel swap in the
/// readback cannot turn it into `xev`'s white, and so it is not the black
/// clear colour.
const PROBE_COLOR: u32 = 0xff_10_d0_14;
/// Fraction of non-black pixels that means "the base window is mapped".
const MAPPED_PIXEL_DIVISOR: usize = 100;
/// Budget for the compositor to start Xwayland and write the display file.
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the base window to map and the frame to settle.
const MAPPED_TIMEOUT: Duration = Duration::from_secs(40);
/// Budget for the two-colour probe to appear after the map.
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
/// Watchdog budget for the compositor thread to exit once `xev` is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll slice while waiting on frames.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Stop writing PPMs after this many frames; bounds disk use.
const RECORD_LIMIT: u64 = 2000;
/// Hard safety exit for the compositor in case cleanup does not stop it.
const STOP_AFTER_FRAMES: u64 = 4000;

/// Kill processes and remove temp state on drop, so a failed assertion does not
/// leak an `xev`, an `Xwayland` or a frame directory.
struct OrientationGuard {
	display_path: PathBuf,
	record_dir: PathBuf,
}

impl OrientationGuard {
	fn cleanup(&mut self) {
		kill_exact("xev");
		kill_exact("Xwayland");
		let _ = std::fs::remove_file(&self.display_path);
		let _ = std::fs::remove_dir_all(&self.record_dir);
	}
}

impl Drop for OrientationGuard {
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

/// A unique path under the temp dir.
fn unique_path(tag: &str) -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-orientation-{tag}-{}-{nanos}", std::process::id()))
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

/// The highest recorded sequence number so far, or `None` if no frame exists.
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

/// Count pixels whose RGB is not all zero.
fn non_black_pixels(ppm: &Ppm) -> usize {
	ppm.rgb.chunks_exact(3).filter(|pixel| pixel != &[0, 0, 0]).count()
}

/// Whether a pixel is the probe's green, tolerant of rounding.
fn is_probe_color(rgb: [u8; 3]) -> bool {
	rgb[1] >= 180 && rgb[0] <= 80 && rgb[2] <= 80
}

/// Wait, bounded, for a stable mapped window: a non-black frame identical to its
/// predecessor (so a map/redraw in progress is never used as the baseline).
fn wait_for_mapped_frame(dir: &Path, timeout: Duration) -> Option<(u64, Ppm)> {
	let threshold = WIDTH as usize * HEIGHT as usize / MAPPED_PIXEL_DIVISOR;
	let deadline = Instant::now() + timeout;
	loop {
		if Instant::now() >= deadline {
			return None;
		}
		if let Some(sequence) = latest_sequence(dir) {
			if let Some(frame) = read_frame(dir, sequence) {
				if non_black_pixels(&frame) > threshold {
					if let Some(previous) = read_frame(dir, sequence.saturating_sub(1)) {
						if previous.rgb == frame.rgb {
							return Some((sequence, frame));
						}
					}
				}
			}
		}
		thread::sleep(POLL_SLICE);
	}
}

/// An inclusive `(x0, y0, x1, y1)` pixel bounding box.
type Bbox = (usize, usize, usize, usize);

/// The bounding box `(x0, y0, x1, y1)` of the probe colour, or `None` if the
/// frame does not contain a `BLOCK_W`×`BLOCK_H` block of it.
fn find_probe(frame: &Ppm) -> Option<Bbox> {
	let mut bbox: Option<(usize, usize, usize, usize)> = None;
	for (i, p) in frame.rgb.chunks_exact(3).enumerate() {
		if !is_probe_color([p[0], p[1], p[2]]) {
			continue;
		}
		let (x, y) = (i % frame.width, i / frame.width);
		bbox = Some(match bbox {
			Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
			None => (x, y, x, y),
		});
	}
	let (x0, y0, x1, y1) = bbox?;
	if x1 - x0 + 1 == BLOCK_W as usize && y1 - y0 + 1 == BLOCK_H as usize {
		Some((x0, y0, x1, y1))
	} else {
		None
	}
}

/// Wait, bounded, for the first frame that contains the two-colour probe,
/// returning it and the probe's bounding box.
fn wait_for_probe(dir: &Path, after: u64, timeout: Duration) -> Option<(u64, Ppm, Bbox)> {
	let deadline = Instant::now() + timeout;
	let mut next = after + 1;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			while next <= latest {
				if let Some(frame) = read_frame(dir, next) {
					if let Some(region) = find_probe(&frame) {
						return Some((next, frame, region));
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

/// Wait, bounded, for `path` to contain a non-empty X display value.
fn wait_for_display(path: &Path, timeout: Duration) -> Option<u32> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Ok(contents) = std::fs::read_to_string(path) {
			let value = contents.trim();
			if let Some(number) = value.strip_prefix(':').and_then(|rest| rest.parse::<u32>().ok()) {
				return Some(number);
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// The `STEAM_GAME` app id shared by `xev` and the probe.
const SHARED_APP_ID: u32 = 4321;

/// Tag `window` with `STEAM_GAME`.
fn set_steam_game(conn: &RustConnection, window: Window, app_id: u32) {
	let atom = conn
		.intern_atom(false, b"STEAM_GAME")
		.expect("intern STEAM_GAME")
		.reply()
		.expect("STEAM_GAME reply")
		.atom;
	conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[app_id])
		.expect("set STEAM_GAME");
	conn.flush().expect("flush");
}

/// The window the compositor made `_NET_ACTIVE_WINDOW` (the hosted `xev`).
fn active_window(conn: &RustConnection) -> Option<Window> {
	let root = conn.setup().roots[0].root;
	let atom = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
	let reply = conn
		.get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
		.ok()?
		.reply()
		.ok()?;
	reply
		.value32()
		.and_then(|mut values| values.next())
		.filter(|window| *window != 0)
}

/// Create and map a solid [`PROBE_COLOR`] override-redirect X11 window at
/// `(BLOCK_X, BLOCK_Y)`.
///
/// Override-redirect is deliberate: the xwm maps it where it asked to be
/// (a normal peer window would be fullscreened by the single-window policy),
/// so its position is known rather than a hint. gamescope only draws an
/// override-redirect window that belongs to the focus window's process or app
/// (`is_good_override_candidate`, `steamcompmgr.cpp:4153`); this probe is a
/// different process than `xev`, so it is tagged with the same `STEAM_GAME` as
/// `xev` to stand for one of the game's own popups.
fn create_probe(conn: &RustConnection) -> Window {
	let screen = &conn.setup().roots[0];
	let window = conn.generate_id().expect("probe window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		BLOCK_X,
		BLOCK_Y,
		BLOCK_W,
		BLOCK_H,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.override_redirect(1)
			.background_pixel(PROBE_COLOR)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create probe window");
	set_steam_game(conn, window, SHARED_APP_ID);
	conn.map_window(window).expect("map probe window");
	conn.flush().expect("flush");

	// Draw the fill after the map request so Xwayland presents a committed
	// buffer the compositor can pick up; the background alone may never be.
	let gc = conn.generate_id().expect("probe gc id");
	conn.create_gc(gc, window, &CreateGCAux::new().foreground(PROBE_COLOR))
		.expect("create probe gc");
	conn.poly_fill_rectangle(
		gc,
		window,
		&[Rectangle {
			x: 0,
			y: 0,
			width: BLOCK_W,
			height: BLOCK_H,
		}],
	)
	.expect("fill probe window");
	conn.free_gc(gc).expect("free probe gc");
	conn.flush().expect("flush");
	window
}

/// Join the compositor thread under a watchdog, returning whether it exited.
fn join_with_timeout(handle: thread::JoinHandle<Result<()>>, timeout: Duration) -> bool {
	let (tx, rx) = mpsc::channel();
	thread::spawn(move || {
		let _ = tx.send(handle.join());
	});
	rx.recv_timeout(timeout).is_ok()
}

/// Quote a path for `sh -c`, so a temp path with spaces still works.
fn shell_quote(path: &Path) -> String {
	format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Run the probe against one storage path: the offscreen renderbuffer readback
/// (`dmabuf == false`) or the GBM DMA-BUF mapped to CPU pixels (`dmabuf ==
/// true`). Both feed the same `FileRecorder`, so both must be top-down.
fn assert_ppm_orientation(tag: &str, dmabuf: bool) {
	let display_path = unique_path(&format!("{tag}-display"));
	let record_dir = unique_path(&format!("{tag}-frames"));
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");

	let mut guard = OrientationGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};

	let script = format!("printf %s \"$DISPLAY\" > {}; exec xev", shell_quote(&display_path));
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope::FrameOutput::Recorder(ferriscope::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			render_to_dmabuf: dmabuf,
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	// The base fullscreen client is `xev`, which is uniform; the probe is the
	// only coloured content, so the block's origin is unambiguous.
	let (baseline_sequence, _baseline) = wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black base frame within {MAPPED_TIMEOUT:?}"));

	let base = active_window(&conn).expect("the hosted xev is the active window");
	set_steam_game(&conn, base, SHARED_APP_ID);
	let probe = create_probe(&conn);

	let (_sequence, frame, (x0, y0, _x1, _y1)) = wait_for_probe(&record_dir, baseline_sequence, PROBE_TIMEOUT)
		.unwrap_or_else(|| panic!("no frame contained the {BLOCK_W}x{BLOCK_H} probe within {PROBE_TIMEOUT:?}"));

	// The probe is at a known *non-centred* output position and is not square,
	// so agreeing on its top-left pins both axes: a vertical flip would put it
	// at `y = HEIGHT - BLOCK_Y - BLOCK_H`, a horizontal flip at
	// `x = WIDTH - BLOCK_X - BLOCK_W`, and a 180° rotation at both. The size
	// check in `find_probe` already rejects a full-window or partial fill.
	assert_eq!(
		(x0, y0),
		(BLOCK_X as usize, BLOCK_Y as usize),
		"the PPM is not in output coordinates: an element at ({BLOCK_X}, {BLOCK_Y}) \
		 appeared at ({x0}, {y0}); the frame is flipped"
	);
	assert_eq!(
		(frame.width, frame.height),
		(WIDTH as usize, HEIGHT as usize),
		"the PPM is not the output size"
	);

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	let _ = conn.destroy_window(probe);
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn renderbuffer_ppm_is_top_down_in_output_coordinates() {
	assert_ppm_orientation("renderbuffer", false);
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn dmabuf_ppm_is_top_down_in_output_coordinates() {
	assert_ppm_orientation("dmabuf", true);
}
