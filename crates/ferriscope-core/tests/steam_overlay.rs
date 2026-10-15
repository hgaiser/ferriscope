//! GPU integration test for the `overlay` gate.
//!
//! The gate for this unit: a Steam overlay/notification — an ordinary X11
//! client surface tagged with the `STEAM_OVERLAY` atom — is classified and
//! *composited over* the single fullscreen window, without becoming it and
//! without taking keyboard focus. The compositor never draws the pixels; the
//! client does.
//!
//! There is no overlay protocol and no real Steam here, so the test speaks the
//! same above-the-wire interface Steam does: it creates a second X11 window with
//! `x11rb` (re-exported by smithay, no new dependency), tags it with
//! `STEAM_OVERLAY`, fills it a distinct colour and maps it over a fullscreen
//! `xev`. It then asserts, from the composited frames and the X server state:
//!
//! - **(classification)** the tagged window is an overlay, not the fullscreen
//!   window: it is never `_NET_WM_STATE_FULLSCREEN`;
//! - **(focus)** a narrow notification leaves `_NET_ACTIVE_WINDOW` and the X
//!   input focus on `xev`; a full-width interactive overlay takes both, and
//!   releasing it returns focus to `xev`;
//! - **(compositing)** a frame recorded after the map contains a solid block of
//!   the overlay's colour, sized to the overlay rather than the output, where
//!   the base frame did not, so it is drawn over the base.
//!
//! Both halves of the overlay/notification split are exercised: a narrow window
//! without `STEAM_INPUT_FOCUS` is a notification, and a full-width one is the
//! interactive overlay.
//!
//! The recorded `FileRecorder` PPMs are in output coordinates, top-left origin
//! (see `tests/frame_orientation.rs`), so the test can also assert *where* the
//! overlay lands: the colour block's top-left must match the X11 window's
//! actual geometry. A vertically flipped readback would move it to
//! `HEIGHT - y - h` and fail that assertion.
//!
//! This is the oracle for `overlay`; it has not been validated against a real
//! Steam Big Picture session.
//!
//! It is `#[ignore]`d: it needs a render node, Xwayland, `xev`, `xdotool` and
//! `xprop`. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test steam_overlay -- --ignored --exact steam_overlay_is_composited --nocapture
//! ```

use std::{
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};

use ferriscope_core::{CompositorConfig, Result, run};
use smithay::reexports::x11rb::{
	self, COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, connection::Connection, protocol::xproto::*,
	rust_connection::RustConnection, wrapper::ConnectionExt as _,
};

/// Output width, small enough that the software readback keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// The overlay's fill colour: opaque green. The alpha byte matters because
/// Xwayland's rootless root is 32-bit ARGB; only the green channel is set, so an
/// R/B channel swap in the readback cannot make a false match and `xev`'s content
/// is not it.
const OVERLAY_COLOR: u32 = 0xff_00_ff_00;
/// The game window's fill: opaque blue. Distinct from [`OVERLAY_COLOR`] and
/// not matched by [`is_overlay_color`], so the two layers are separable.
const BASE_COLOR: u32 = 0xff_00_00_ff;
/// Fraction of non-black pixels that means "the base window is mapped".
const MAPPED_PIXEL_DIVISOR: usize = 100;
/// Budget for the compositor to start Xwayland and write the display file.
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the base window to map and the frame to settle.
const MAPPED_TIMEOUT: Duration = Duration::from_secs(40);
/// Budget for an overlay-bearing frame to appear after the map.
const OVERLAY_TIMEOUT: Duration = Duration::from_secs(25);
/// Budget for focus/activation to settle on the interactive overlay or return
/// to the base window.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Watchdog budget for the compositor thread to exit once `xev` is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll slice while waiting on frames or X state.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Settle time after mapping before checking focus, and after destroying before
/// the next case.
const SETTLE: Duration = Duration::from_millis(600);
/// Stop writing PPMs after this many frames; bounds disk use.
const RECORD_LIMIT: u64 = 2000;
/// Hard safety exit for the compositor in case cleanup does not stop it.
const STOP_AFTER_FRAMES: u64 = 4000;

/// Kill processes and remove temp state on drop, so a failed assertion does not
/// leak an `xev`, an `Xwayland`, a socket or a frame directory.
struct OverlayGuard {
	display_path: PathBuf,
	record_dir: PathBuf,
}

impl OverlayGuard {
	/// Kill the client and the X server, then remove the temp files.
	fn cleanup(&mut self) {
		kill_exact("xev");
		kill_exact("Xwayland");
		let _ = std::fs::remove_file(&self.display_path);
		let _ = std::fs::remove_dir_all(&self.record_dir);
	}
}

impl Drop for OverlayGuard {
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
	std::env::temp_dir().join(format!("ferriscope-overlay-{tag}-{}-{nanos}", std::process::id()))
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

/// The RGB at `(x, y)`, if the frame covers it.
fn pixel(ppm: &Ppm, x: usize, y: usize) -> Option<[u8; 3]> {
	if x >= ppm.width || y >= ppm.height {
		return None;
	}
	let offset = (y * ppm.width + x) * 3;
	let slice = ppm.rgb.get(offset..offset + 3)?;
	Some([slice[0], slice[1], slice[2]])
}

/// Whether a pixel is the overlay's green, tolerant of rounding.
fn is_overlay_color(rgb: [u8; 3]) -> bool {
	rgb[1] >= 180 && rgb[0] <= 80 && rgb[2] <= 80
}

/// Wait, bounded, for a specific frame to be fully written and readable.
///
/// The recorder writes each PPM non-atomically, so the newest sequence can be
/// caught mid-write; a direct read would then return `None` for a frame that is
/// about to be complete.
fn wait_for_frame(dir: &Path, sequence: u64, timeout: Duration) -> Option<Ppm> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(frame) = read_frame(dir, sequence) {
			return Some(frame);
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
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
						if pixel(&previous, 0, 0).is_some() && previous.rgb == frame.rgb {
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

/// The bounding box `(x0, y0, x1, y1)` of a solid `width`×`height` block of the
/// overlay colour, or `None` if the frame does not contain exactly that block.
///
/// The region is located rather than assumed, then its origin is checked
/// against the window's X11 geometry by the caller. Finding the block by its
/// size still proves it is the overlay and not a window-sized fill.
fn overlay_region(frame: &Ppm, width: usize, height: usize) -> Option<Bbox> {
	let expected = width * height;
	let mut count = 0usize;
	let mut bbox: Option<Bbox> = None;
	for (i, p) in frame.rgb.chunks_exact(3).enumerate() {
		if !is_overlay_color([p[0], p[1], p[2]]) {
			continue;
		}
		count += 1;
		let (x, y) = (i % frame.width, i / frame.width);
		bbox = Some(match bbox {
			Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
			None => (x, y, x, y),
		});
	}
	match bbox {
		Some((x0, y0, x1, y1)) if count == expected && x1 - x0 + 1 == width && y1 - y0 + 1 == height => {
			Some((x0, y0, x1, y1))
		},
		_ => None,
	}
}

/// Wait, bounded, for the first frame after `after` that contains a solid
/// `width`×`height` overlay-coloured region, returning it and that region.
fn wait_for_overlay_region(
	dir: &Path,
	after: u64,
	width: usize,
	height: usize,
	timeout: Duration,
) -> Option<(u64, Ppm, Bbox)> {
	let deadline = Instant::now() + timeout;
	let mut next = after + 1;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			while next <= latest {
				if let Some(frame) = read_frame(dir, next) {
					if let Some(region) = overlay_region(&frame, width, height) {
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

/// Run an X11 tool against display `:N`, returning trimmed stdout on success.
fn x_tool(display: u32, program: &str, args: &[&str]) -> Option<String> {
	let output = Command::new(program)
		.env("DISPLAY", format!(":{display}"))
		.args(args)
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The current `_NET_ACTIVE_WINDOW`, if any.
fn active_window(display: u32) -> Option<String> {
	x_tool(display, "xdotool", &["getactivewindow"]).filter(|id| !id.is_empty() && id != "0")
}

/// The window with input focus, if any.
fn focused_window(display: u32) -> Option<String> {
	x_tool(display, "xdotool", &["getwindowfocus"]).filter(|id| !id.is_empty() && id != "0")
}

/// Wait, bounded, for `_NET_ACTIVE_WINDOW` to become `window`.
fn wait_for_active(display: u32, window: &str, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if active_window(display).as_deref() == Some(window) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for the X server to report `window` at `w`×`h`.
fn wait_for_window_size(conn: &RustConnection, window: Window, w: u16, h: u16, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(reply) = conn.get_geometry(window).ok().and_then(|cookie| cookie.reply().ok()) {
			if reply.width == w && reply.height == h {
				return true;
			}
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Whether `_NET_WM_STATE` of `window` contains `state`.
fn has_net_wm_state(display: u32, window: &str, state: &str) -> bool {
	x_tool(display, "xprop", &["-id", window, "_NET_WM_STATE"])
		.map(|value| value.contains(state))
		.unwrap_or(false)
}

/// Create, tag and map a `STEAM_OVERLAY` X11 window, then fill it.
///
/// The property is set before the map request, so the xwm classifies the window
/// when it maps it — exactly the order a Steam client uses.
fn create_overlay(conn: &RustConnection, x: i16, y: i16, w: u16, h: u16, input_focus: bool) -> Window {
	let screen = &conn.setup().roots[0];
	let window = conn.generate_id().expect("overlay window id");
	let overlay_atom = conn
		.intern_atom(false, b"STEAM_OVERLAY")
		.expect("intern STEAM_OVERLAY")
		.reply()
		.expect("STEAM_OVERLAY atom")
		.atom;
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		x,
		y,
		w,
		h,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(OVERLAY_COLOR)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create overlay window");
	conn.change_property32(PropMode::REPLACE, window, overlay_atom, AtomEnum::CARDINAL, &[1])
		.expect("set STEAM_OVERLAY");
	if input_focus {
		let input_atom = conn
			.intern_atom(false, b"STEAM_INPUT_FOCUS")
			.expect("intern STEAM_INPUT_FOCUS")
			.reply()
			.expect("STEAM_INPUT_FOCUS atom")
			.atom;
		conn.change_property32(PropMode::REPLACE, window, input_atom, AtomEnum::CARDINAL, &[1])
			.expect("set STEAM_INPUT_FOCUS");
	}
	conn.map_window(window).expect("map overlay window");
	conn.flush().expect("flush");

	// Draw content after the map request, so Xwayland presents a buffer the
	// compositor can pick up; the background alone may never be committed.
	let gc = conn.generate_id().expect("overlay gc id");
	conn.create_gc(gc, window, &CreateGCAux::new().foreground(OVERLAY_COLOR))
		.expect("create overlay gc");
	let rectangle = Rectangle {
		x: 0,
		y: 0,
		width: w,
		height: h,
	};
	conn.poly_fill_rectangle(gc, window, &[rectangle])
		.expect("fill overlay window");
	conn.free_gc(gc).expect("free overlay gc");
	conn.flush().expect("flush");
	window
}

/// Fill `window` with `color`, after its map request, so Xwayland presents a
/// buffer the compositor can pick up.
fn fill_window(conn: &RustConnection, window: Window, w: u16, h: u16, color: u32) {
	let gc = conn.generate_id().expect("window gc id");
	conn.create_gc(gc, window, &CreateGCAux::new().foreground(color))
		.expect("create window gc");
	let rectangle = Rectangle {
		x: 0,
		y: 0,
		width: w,
		height: h,
	};
	conn.poly_fill_rectangle(gc, window, &[rectangle]).expect("fill window");
	conn.free_gc(gc).expect("free window gc");
	conn.flush().expect("flush");
}

/// Create and map an X11 window with **no Steam atom**, filled `color`, then
/// return it. `parent` sets `WM_TRANSIENT_FOR` so the window keeps its
/// requested size: a normal toplevel is configured to the full output by the
/// xwm, which would make a narrow window look full-width to the overlay
/// classification. Real Steam maps its Big Picture window this way and only
/// tags it with `STEAM_OVERLAY` when the overlay opens.
fn create_untagged_window(
	conn: &RustConnection,
	parent: Option<Window>,
	x: i16,
	y: i16,
	w: u16,
	h: u16,
	color: u32,
) -> Window {
	let screen = &conn.setup().roots[0];
	let window = conn.generate_id().expect("untagged window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		x,
		y,
		w,
		h,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(color)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create untagged window");
	if let Some(parent) = parent {
		conn.change_property32(
			PropMode::REPLACE,
			window,
			intern_atom(conn, "WM_TRANSIENT_FOR"),
			AtomEnum::WINDOW,
			&[parent],
		)
		.expect("set WM_TRANSIENT_FOR");
	}
	conn.map_window(window).expect("map untagged window");
	conn.flush().expect("flush");
	fill_window(conn, window, w, h, color);
	window
}

/// Create and map a fullscreen game window (`STEAM_GAME`) filled `color`, so a
/// later-mapped game out-prioritises the untagged window and covers it — the
/// real ordering: the Big Picture window is mapped, then the game, then the
/// overlay atom flips on the already-mapped Big Picture window.
fn create_game_window(conn: &RustConnection, w: u16, h: u16, color: u32) -> Window {
	let screen = &conn.setup().roots[0];
	let window = conn.generate_id().expect("game window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		0,
		0,
		w,
		h,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(color)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create game window");
	conn.change_property32(
		PropMode::REPLACE,
		window,
		intern_atom(conn, "STEAM_GAME"),
		AtomEnum::CARDINAL,
		&[548430],
	)
	.expect("set STEAM_GAME");
	conn.map_window(window).expect("map game window");
	conn.flush().expect("flush");
	fill_window(conn, window, w, h, color);
	window
}

/// Create, tag and map an interactive **Steam UI** overlay, the window Steam
/// maps as "Steam Big Picture Mode": `STEAM_GAME=769` plus `STEAM_OVERLAY=1`
/// and, when `input_focus`, `STEAM_INPUT_FOCUS=1`.
///
/// gamescope treats the Steam UI (appID 769) as fullscreen and sizes the
/// focused Steam UI window to the output (`steamcompmgr.cpp:1920-1924`,
/// `:4816-4831`), so ferriscope must too. A plain `STEAM_OVERLAY` window with
/// no Steam UI id is not resized; it is only fit at composite time.
fn create_steam_ui_overlay(conn: &RustConnection, x: i16, y: i16, w: u16, h: u16, input_focus: bool) -> Window {
	let screen = &conn.setup().roots[0];
	let window = conn.generate_id().expect("steam ui overlay id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		x,
		y,
		w,
		h,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(OVERLAY_COLOR)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create steam ui overlay");
	set_cardinal(conn, window, "STEAM_OVERLAY", 1);
	if input_focus {
		set_cardinal(conn, window, "STEAM_INPUT_FOCUS", 1);
	}
	set_cardinal(conn, window, "STEAM_GAME", 769);
	conn.map_window(window).expect("map steam ui overlay");
	conn.flush().expect("flush");
	fill_window(conn, window, w, h, OVERLAY_COLOR);
	window
}

/// Intern an atom by name.
fn intern_atom(conn: &RustConnection, name: &str) -> Atom {
	conn.intern_atom(false, name.as_bytes())
		.expect("intern atom")
		.reply()
		.expect("atom reply")
		.atom
}

/// Replace a 32-bit `CARDINAL` property by atom name, as Steam does with
/// `STEAM_OVERLAY`/`STEAM_INPUT_FOCUS` on an already-mapped window.
fn set_cardinal(conn: &RustConnection, window: Window, name: &str, value: u32) {
	conn.change_property32(
		PropMode::REPLACE,
		window,
		intern_atom(conn, name),
		AtomEnum::CARDINAL,
		&[value],
	)
	.expect("set cardinal");
	conn.flush().expect("flush");
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
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn steam_overlay_is_composited() {
	let display_path = unique_path("display");
	let record_dir = unique_path("frames");
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");

	let mut guard = OverlayGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};

	// `xev` is spawned by the compositor (so a client always exists); the shell
	// wrapper writes the X display it was given to a file the test can read.
	// The window is output-sized, so the role holder is *not* output-fill scaled
	// A Steam overlay is nonetheless fit to the output by its own
	// content (, gamescope's `paint_window(overlay, overlay)`), so the
	// composited block is the overlay's texture scaled to the output, centred —
	// not the raw window rect.
	let script = format!(
		"printf %s \"$DISPLAY\" > {}; exec xev -geometry {WIDTH}x{HEIGHT}",
		shell_quote(&display_path)
	);
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	// Case 1: a narrow STEAM_OVERLAY window without input focus is a
	// notification. Case 2: a full-width one with input focus is the interactive
	// overlay, case 3 the same without it. Each
	// is fit to the output by its own texture: the `80x60` notification is
	// `4x` and fills `320x240` at `(0,0)`; the `320x60` overlay is `1x` and is
	// centred at `(0,90)`.
	for (label, w, h, input_focus, interactive, block_w, block_h, block_x, block_y) in [
		(
			"notification",
			80u16,
			60u16,
			false,
			false,
			320usize,
			240usize,
			0usize,
			0usize,
		),
		(
			"interactive overlay",
			WIDTH as u16,
			60u16,
			true,
			true,
			320usize,
			60usize,
			0usize,
			90usize,
		),
		// A full-width `STEAM_OVERLAY` is the overlay window (drawn as such) but
		// only `STEAM_INPUT_FOCUS` gives it input: gamescope makes an overlay the
		// input focus only when `inputFocusMode` is set
		// (`steamcompmgr.cpp:4549`, `:5143`), so the base keeps focus here.
		(
			"full-width overlay without input focus",
			WIDTH as u16,
			60u16,
			false,
			false,
			320usize,
			60usize,
			0usize,
			90usize,
		),
	] {
		let (baseline_sequence, baseline) = wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT)
			.unwrap_or_else(|| panic!("[{label}] no mapped, stable, non-black base frame within {MAPPED_TIMEOUT:?}"));

		// The base window (xev) holds focus before the overlay maps; for the
		// interactive overlay it is the target focus returns to afterwards.
		let active_before = active_window(display).expect("a base window is active before the overlay maps");
		let focused_before = focused_window(display);

		let overlay = create_overlay(&conn, 10, 10, w, h, input_focus);
		thread::sleep(SETTLE);

		// Classification: an overlay is drawn over the window, never made
		// fullscreen (a misclassified peer toplevel would be fullscreened).
		let id = format!("0x{overlay:x}");
		assert!(
			!has_net_wm_state(display, &id, "_NET_WM_STATE_FULLSCREEN"),
			"[{label}] the overlay was fullscreened; it was classified as the fullscreen window"
		);

		// Focus: a notification never takes it; the interactive overlay does
		if interactive {
			assert!(
				wait_for_active(display, &overlay.to_string(), FOCUS_TIMEOUT),
				"[{label}] the interactive overlay did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
			);
			assert_eq!(
				focused_window(display).as_deref(),
				Some(overlay.to_string().as_str()),
				"[{label}] the interactive overlay did not take the X input focus"
			);
		} else {
			assert_eq!(
				active_window(display).as_deref(),
				Some(active_before.as_str()),
				"[{label}] mapping the overlay changed _NET_ACTIVE_WINDOW"
			);
			assert_eq!(
				focused_window(display).as_deref(),
				focused_before.as_deref(),
				"[{label}] mapping the overlay changed the X input focus"
			);
		}

		// Compositing: a later frame contains a solid `block_w`×`block_h` block
		// of the overlay's colour, the overlay's own texture fit to the output
		// and centred. Its size (not the raw window size) shows it is fit to
		// the output, the position shows the recorder is in output coordinates,
		// and the base frame was not that colour there, so it is drawn over the
		// fullscreen window.
		let (_, _frame, region) =
			wait_for_overlay_region(&record_dir, baseline_sequence, block_w, block_h, OVERLAY_TIMEOUT).unwrap_or_else(
				|| {
					panic!(
						"[{label}] no frame contained a {block_w}x{block_h} overlay-coloured region within {OVERLAY_TIMEOUT:?}; \
				 the overlay was not fit to the output"
					)
				},
			);
		assert_eq!(
			(region.0, region.1),
			(block_x, block_y),
			"[{label}] the overlay was composited at ({}, {}), expected ({block_x}, {block_y})",
			region.0,
			region.1
		);
		let center = ((region.0 + region.2) / 2, (region.1 + region.3) / 2);
		assert!(
			!pixel(&baseline, center.0, center.1).is_some_and(is_overlay_color),
			"[{label}] the base frame already showed the overlay colour at {center:?}"
		);

		// Tear the overlay down before the next case, and let the compositor
		// return to a base-only frame. A focused interactive overlay returns
		// keyboard focus to the base window on unmap.
		conn.destroy_window(overlay).expect("destroy overlay");
		conn.flush().expect("flush");
		if interactive {
			assert!(
				wait_for_active(display, &active_before, FOCUS_TIMEOUT),
				"[{label}] focus did not return to the base window after unmap within {FOCUS_TIMEOUT:?}"
			);
		}
		thread::sleep(SETTLE);
	}

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// Real Steam maps its Big Picture window, then the game maps over it, and only
/// when the overlay opens does Steam set `STEAM_OVERLAY` on the already-mapped
/// Big Picture window. [`steam_overlay_is_composited`] tags before the map, so
/// it never exercises that promotion.
///
/// This mirrors Steam's ordering: `xev` maps first, an untagged window maps over
/// it, then a `STEAM_GAME` window out-prioritises both and covers the untagged
/// one — so the untagged window is invisible until it is promoted. Setting
/// `STEAM_OVERLAY` on it must move it into the overlay layer, draw it over the
/// game, and give it keyboard focus iff it is interactive; clearing the atom
/// must demote it and release focus back to the game.
#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn steam_overlay_tagged_after_map_is_promoted_and_demoted() {
	let display_path = unique_path("display-promote");
	let record_dir = unique_path("frames-promote");
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");

	let mut guard = OverlayGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};

	let script = format!("printf %s \"$DISPLAY\" > {}; exec xev", shell_quote(&display_path));
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	let _ = wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped xev base frame within {MAPPED_TIMEOUT:?}"));
	// xev is the first window and is active now; a narrow untagged window needs
	// it as a transient parent to keep its requested size.
	let xev_id: u32 = active_window(display)
		.expect("xev is active after its map")
		.parse()
		.expect("xev window id");

	// A full-size untagged window becomes the interactive overlay once tagged; a
	// narrow untagged transient (no `STEAM_INPUT_FOCUS`) becomes a notification.
	// Both are then fit to the output by their own texture: the
	// `320x240` overlay is the identity and the `80x60` notification is `4x`, so
	// each composites as a solid `320x240` block at the origin, over the game.
	for (label, transient, w, h, input_focus, interactive, block_w, block_h) in [
		(
			"tagged-after-map interactive overlay",
			false,
			WIDTH as u16,
			HEIGHT as u16,
			true,
			true,
			320usize,
			240usize,
		),
		(
			"tagged-after-map notification",
			true,
			80u16,
			60u16,
			false,
			false,
			320usize,
			240usize,
		),
	] {
		// The untagged window maps with no Steam atom. A normal toplevel is a
		// peer of `xev` and the xwm configures it to the full output; a
		// transient (notification) keeps its requested geometry.
		let parent = transient.then_some(xev_id);
		let window = create_untagged_window(&conn, parent, 10, 10, w, h, OVERLAY_COLOR);
		thread::sleep(SETTLE);

		// The game maps afterwards and takes the single-window role, covering
		// the untagged window.
		let game = create_game_window(&conn, WIDTH as u16, HEIGHT as u16, BASE_COLOR);
		thread::sleep(SETTLE);
		let game_id = game.to_string();
		assert!(
			wait_for_active(display, &game_id, FOCUS_TIMEOUT),
			"[{label}] the game did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
		);

		let id = format!("0x{window:x}");

		// Baseline: the game covers the untagged window, so no overlay-coloured
		// block is composited yet.
		let after_game = latest_sequence(&record_dir).expect("a frame after the game mapped");
		let baseline = wait_for_frame(&record_dir, after_game, FOCUS_TIMEOUT).expect("the baseline frame");
		assert!(
			overlay_region(&baseline, block_w, block_h).is_none(),
			"[{label}] the untagged window was composited over the game before its atom was set"
		);

		// Tag it after the map, exactly as Steam's overlay toggle does.
		set_cardinal(&conn, window, "STEAM_OVERLAY", 1);
		if input_focus {
			set_cardinal(&conn, window, "STEAM_INPUT_FOCUS", 1);
		}

		// Classification: promoted, never made fullscreen.
		assert!(
			!has_net_wm_state(display, &id, "_NET_WM_STATE_FULLSCREEN"),
			"[{label}] the tagged window was fullscreened; it was misclassified"
		);

		// Focus: an interactive overlay takes it; a notification never does.
		if interactive {
			assert!(
				wait_for_active(display, &window.to_string(), FOCUS_TIMEOUT),
				"[{label}] the interactive overlay did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
			);
			assert_eq!(
				focused_window(display).as_deref(),
				Some(window.to_string().as_str()),
				"[{label}] the interactive overlay did not take the X input focus"
			);
		} else {
			assert_ne!(
				focused_window(display).as_deref(),
				Some(window.to_string().as_str()),
				"[{label}] tagging a notification gave it the X input focus"
			);
		}

		// Compositing: a later frame contains the solid block at the output
		// origin, its own texture fit to the output, drawn over the game.
		let (_, _frame, region) = wait_for_overlay_region(&record_dir, after_game, block_w, block_h, OVERLAY_TIMEOUT)
			.unwrap_or_else(|| {
				panic!("[{label}] the tagged window was not promoted into the overlay layer within {OVERLAY_TIMEOUT:?}")
			});
		assert_eq!(
			(region.0, region.1),
			(0, 0),
			"[{label}] the promoted overlay was composited at ({}, {}), expected the output origin",
			region.0,
			region.1
		);
		let center = ((region.0 + region.2) / 2, (region.1 + region.3) / 2);
		assert!(
			!pixel(&baseline, center.0, center.1).is_some_and(is_overlay_color),
			"[{label}] the baseline already showed the overlay colour at {center:?}"
		);

		// Clear the atom: demotion returns focus to the game. Without the
		// demotion the tracked interactive overlay would keep focus. (A demoted
		// notification becomes an ordinary transient/dialog again and focuses
		// per the normal transient rule, so only the interactive case asserts.)
		set_cardinal(&conn, window, "STEAM_OVERLAY", 0);
		set_cardinal(&conn, window, "STEAM_INPUT_FOCUS", 0);
		if interactive {
			assert!(
				wait_for_active(display, &game_id, FOCUS_TIMEOUT),
				"[{label}] focus did not return to the game after demotion within {FOCUS_TIMEOUT:?}"
			);
		}

		conn.destroy_window(window).expect("destroy untagged window");
		conn.destroy_window(game).expect("destroy game window");
		conn.flush().expect("flush");
		thread::sleep(SETTLE);
	}

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// A Steam overlay is composited over the **output-fill scaled** role holder at
/// its own content-to-output fit, not at its raw X11 position.
///
/// Real Steam maps its Big Picture overlay as a fixed `1280x800` window (with
/// `STEAM_INPUT_FOCUS`) while the game keeps its own resolution. Ferriscope
/// scales the game's committed content to fill the output, so an overlay left at
/// its raw location and 1:1 scale lands beside the game, in the letterbox.
/// gamescope instead paints each overlay with its
/// own committed texture fit to the output, centred, ignoring the window
/// position (`steamcompmgr.cpp:3262`/`:3303` call `paint_window(overlay,
/// overlay)`, so `w == scaleW`; `:2575-2580` takes the source from the committed
/// texture; `:2603-2604`/`:2615` zero the window offset; `:2624`/`:2644`
/// centre).
///
/// The role is a `160x120` `xev` on the `320x240` output (fit `2x`, no
/// letterbox), so the game fills the frame. An interactive `60x60` overlay is
/// fit `4x`, centred at x offset `40`: a solid `240x240` block at `(40,0)`, over
/// the role. A `80x40` notification is fit `4x`, centred at y offset `40`: a
/// solid `320x160` block at `(0,40)`. Before the fix both drew window-sized at
/// their raw geometry, so neither block exists.
#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn steam_overlay_is_composited_over_a_scaled_role() {
	/// The role's committed content: half the output on each axis (`2x` fit).
	const ROLE_GEOMETRY: &str = "160x120";

	let display_path = unique_path("display-scaled");
	let record_dir = unique_path("frames-scaled");
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");

	let mut guard = OverlayGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};

	let script = format!(
		"printf %s \"$DISPLAY\" > {}; exec xev -geometry {ROLE_GEOMETRY}",
		shell_quote(&display_path)
	);
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	let (baseline_sequence, baseline) = wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black base frame within {MAPPED_TIMEOUT:?}"));

	// (label, overlay w, overlay h, input focus, expected block w, h, x, y)
	for (label, w, h, input_focus, block_w, block_h, block_x, block_y) in [
		(
			"interactive overlay",
			60u16,
			60u16,
			true,
			240usize,
			240usize,
			40usize,
			0usize,
		),
		("notification", 80u16, 40u16, false, 320usize, 160usize, 0usize, 40usize),
	] {
		let overlay = create_overlay(&conn, 10, 10, w, h, input_focus);
		thread::sleep(SETTLE);

		// Classification: never made fullscreen (a misclassified peer would be).
		let id = format!("0x{overlay:x}");
		assert!(
			!has_net_wm_state(display, &id, "_NET_WM_STATE_FULLSCREEN"),
			"[{label}] the overlay was fullscreened; it was classified as the fullscreen window"
		);
		if input_focus {
			assert!(
				wait_for_active(display, &overlay.to_string(), FOCUS_TIMEOUT),
				"[{label}] the interactive overlay did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
			);
		}

		// Compositing: the overlay's committed `w x h` content fit to the output
		// is a solid `block_w x block_h` region at the centred offset, drawn
		// over the scaled role.
		let (_, _frame, region) =
			wait_for_overlay_region(&record_dir, baseline_sequence, block_w, block_h, OVERLAY_TIMEOUT).unwrap_or_else(
				|| {
					panic!(
						"[{label}] no frame contained a {block_w}x{block_h} overlay-coloured region within {OVERLAY_TIMEOUT:?}; \
				 the overlay was not fit to the output"
					)
				},
			);
		assert_eq!(
			(region.0, region.1),
			(block_x, block_y),
			"[{label}] the scaled overlay was composited at ({}, {}), expected ({block_x}, {block_y})",
			region.0,
			region.1
		);
		let center = ((region.0 + region.2) / 2, (region.1 + region.3) / 2);
		assert!(
			!pixel(&baseline, center.0, center.1).is_some_and(is_overlay_color),
			"[{label}] the baseline already showed the overlay colour at {center:?}"
		);

		conn.destroy_window(overlay).expect("destroy overlay");
		conn.flush().expect("flush");
		thread::sleep(SETTLE);
	}

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// An overlay is fit to the output **even when the role holder is not
/// output-fill scaled**.
///
/// Under ferriscope the Xwayland screen is the output (`320x240` here): a game that asks for a smaller render mode
/// (`1280x720`) still commits an output-sized window buffer, so the role holder's
/// `OutputFill` is the identity and applies no transform. Steam sizes
/// its separate overlay window from the game's *render* resolution (smaller than
/// the output). gamescope still paints the overlay through its own
/// content-to-output fit (`paint_window(overlay, overlay)`, `w == scaleW`,
/// `steamcompmgr.cpp:3262`/`:3303`; `:2575-2580` takes the source from the
/// committed texture and `:2624`/`:2644` centre), so it fills the output;
/// gating the fit on the role fill would leave the overlay at its raw window
/// size in the top-left.
///
/// The role is an output-sized `320x240` `xev` (fit identity). An interactive
/// `60x60` overlay is fit `4x`, centred at x offset `40`: a solid `240x240`
/// block at `(40,0)`; an `80x40` notification is fit `4x`, centred at y offset
/// `40`: a solid `320x160` block at `(0,40)`. Drawn window-sized at the
/// overlay's raw X11 position, neither block would exist.
#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn steam_overlay_is_fit_to_the_output_when_the_role_is_not_scaled() {
	/// The role's committed content fills the output (`1x` fit, identity).
	const ROLE_GEOMETRY: &str = "320x240";

	let display_path = unique_path("display-unscaled");
	let record_dir = unique_path("frames-unscaled");
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");

	let mut guard = OverlayGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};

	let script = format!(
		"printf %s \"$DISPLAY\" > {}; exec xev -geometry {ROLE_GEOMETRY}",
		shell_quote(&display_path)
	);
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	let (baseline_sequence, baseline) = wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black base frame within {MAPPED_TIMEOUT:?}"));

	// (label, overlay w, overlay h, input focus, expected block w, h, x, y)
	for (label, w, h, input_focus, block_w, block_h, block_x, block_y) in [
		(
			"interactive overlay",
			60u16,
			60u16,
			true,
			240usize,
			240usize,
			40usize,
			0usize,
		),
		("notification", 80u16, 40u16, false, 320usize, 160usize, 0usize, 40usize),
	] {
		let overlay = create_overlay(&conn, 10, 10, w, h, input_focus);
		thread::sleep(SETTLE);

		let id = format!("0x{overlay:x}");
		assert!(
			!has_net_wm_state(display, &id, "_NET_WM_STATE_FULLSCREEN"),
			"[{label}] the overlay was fullscreened; it was classified as the fullscreen window"
		);
		if input_focus {
			assert!(
				wait_for_active(display, &overlay.to_string(), FOCUS_TIMEOUT),
				"[{label}] the interactive overlay did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
			);
		}

		let (_, _frame, region) =
			wait_for_overlay_region(&record_dir, baseline_sequence, block_w, block_h, OVERLAY_TIMEOUT).unwrap_or_else(
				|| {
					panic!(
						"[{label}] no frame contained a {block_w}x{block_h} overlay-coloured region within {OVERLAY_TIMEOUT:?}; \
				 the overlay was not fit to the output when the role was not scaled"
					)
				},
			);
		assert_eq!(
			(region.0, region.1),
			(block_x, block_y),
			"[{label}] the scaled overlay was composited at ({}, {}), expected ({block_x}, {block_y})",
			region.0,
			region.1
		);
		let center = ((region.0 + region.2) / 2, (region.1 + region.3) / 2);
		assert!(
			!pixel(&baseline, center.0, center.1).is_some_and(is_overlay_color),
			"[{label}] the baseline already showed the overlay colour at {center:?}"
		);

		conn.destroy_window(overlay).expect("destroy overlay");
		conn.flush().expect("flush");
		thread::sleep(SETTLE);
	}

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// The Steam UI window is sized to the output, so an interactive Steam overlay
/// covers the whole output at the output origin; a notification is never
/// resized; and no overlay carries
/// `_NET_WM_STATE_FOCUSED` (gamescope writes none).
///
/// gamescope treats the Steam UI (appID 769) as fullscreen and sizes the
/// **focused** Steam UI window to the output (`steamcompmgr.cpp:1920-1924`,
/// `:4816-4831`); `win_has_game_id` is `appID != 0` (`:3931`), so the Big
/// Picture window qualifies. A `STEAM_OVERLAY` notification is skipped from
/// focus (`:4466`) and never resized. The role is a `160x120` `xev` on the
/// `320x240` output (fit `2x`).
#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn steam_ui_overlay_is_sized_to_the_output() {
	/// The role's committed content: half the output on each axis (`2x` fit).
	const ROLE_GEOMETRY: &str = "160x120";

	let display_path = unique_path("display-steamui");
	let record_dir = unique_path("frames-steamui");
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");

	let mut guard = OverlayGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};

	let script = format!(
		"printf %s \"$DISPLAY\" > {}; exec xev -geometry {ROLE_GEOMETRY}",
		shell_quote(&display_path)
	);
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};

	let runner = thread::spawn(move || run(config));

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	let (baseline_sequence, baseline) = wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT)
		.unwrap_or_else(|| panic!("no mapped, stable, non-black base frame within {MAPPED_TIMEOUT:?}"));

	// Case 1: an interactive Steam UI overlay. Its client-chosen `60x60` is
	// overridden to the output; it covers the whole output at the origin and
	// takes the X input focus without `_NET_WM_STATE_FOCUSED`.
	let overlay = create_steam_ui_overlay(&conn, 10, 10, 60, 60, true);
	assert!(
		wait_for_window_size(&conn, overlay, WIDTH as u16, HEIGHT as u16, FOCUS_TIMEOUT),
		"the Steam UI overlay was not sized to {WIDTH}x{HEIGHT} within {FOCUS_TIMEOUT:?}"
	);
	// Redraw at the output size so the committed content matches the window.
	fill_window(&conn, overlay, WIDTH as u16, HEIGHT as u16, OVERLAY_COLOR);
	thread::sleep(SETTLE);

	let id = format!("0x{overlay:x}");
	assert!(
		!has_net_wm_state(display, &id, "_NET_WM_STATE_FOCUSED"),
		"the interactive Steam UI overlay carries _NET_WM_STATE_FOCUSED"
	);
	assert!(
		wait_for_active(display, &overlay.to_string(), FOCUS_TIMEOUT),
		"the interactive Steam UI overlay did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
	);
	let (_, _frame, region) = wait_for_overlay_region(
		&record_dir,
		baseline_sequence,
		WIDTH as usize,
		HEIGHT as usize,
		OVERLAY_TIMEOUT,
	)
	.unwrap_or_else(|| {
		panic!("the Steam UI overlay did not cover the {WIDTH}x{HEIGHT} output within {OVERLAY_TIMEOUT:?}")
	});
	assert_eq!(
		(region.0, region.1),
		(0, 0),
		"the Steam UI overlay was composited at ({}, {}), expected the output origin",
		region.0,
		region.1
	);
	let center = (WIDTH as usize / 2, HEIGHT as usize / 2);
	assert!(
		!pixel(&baseline, center.0, center.1).is_some_and(is_overlay_color),
		"the baseline already showed the overlay colour at {center:?}"
	);

	conn.destroy_window(overlay).expect("destroy steam ui overlay");
	conn.flush().expect("flush");
	thread::sleep(SETTLE);

	// Case 2: a narrow notification that also carries `STEAM_GAME=769` is not
	// resized — gamescope skips notifications from focus and never resizes
	// them — and never takes focus. still fits its `80x40` content to
	// a `320x160` block at `(0,40)`.
	let notification = create_steam_ui_overlay(&conn, 10, 10, 80, 40, false);
	thread::sleep(SETTLE);
	let geometry = conn
		.get_geometry(notification)
		.expect("notification get_geometry")
		.reply()
		.expect("notification geometry reply");
	assert_eq!(
		(geometry.width, geometry.height),
		(80, 40),
		"a notification was resized; only the Steam UI window is sized to the output"
	);
	let nid = format!("0x{notification:x}");
	assert!(
		!has_net_wm_state(display, &nid, "_NET_WM_STATE_FOCUSED"),
		"a notification carries _NET_WM_STATE_FOCUSED"
	);
	assert_ne!(
		focused_window(display).as_deref(),
		Some(notification.to_string().as_str()),
		"a notification took the X input focus"
	);
	let (_, _frame, region) =
		wait_for_overlay_region(&record_dir, baseline_sequence, WIDTH as usize, 160, OVERLAY_TIMEOUT)
			.unwrap_or_else(|| panic!("the notification was not fit to the output within {OVERLAY_TIMEOUT:?}"));
	assert_eq!(
		(region.0, region.1),
		(0, 40),
		"the notification was composited at ({}, {}), expected (0, 40)",
		region.0,
		region.1
	);

	conn.destroy_window(notification).expect("destroy notification");
	conn.flush().expect("flush");

	// Explicit cleanup, then a bounded join; `xev` dying is what stops the loop.
	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// Quote a path for `sh -c`, so a temp path with spaces still works.
fn shell_quote(path: &Path) -> String {
	format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Wait, bounded, for a frame after `after` whose pixel at `(x, y)` satisfies
/// `predicate`, returning that frame's sequence and pixel.
fn wait_for_pixel(
	dir: &Path,
	after: u64,
	(x, y): (usize, usize),
	predicate: impl Fn([u8; 3]) -> bool,
	timeout: Duration,
) -> Option<(u64, [u8; 3])> {
	let deadline = Instant::now() + timeout;
	let mut next = after + 1;
	loop {
		if let Some(latest) = latest_sequence(dir) {
			while next <= latest {
				if let Some(rgb) = read_frame(dir, next).and_then(|frame| pixel(&frame, x, y)) {
					if predicate(rgb) {
						return Some((next, rgb));
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

/// An overlay is drawn at `_NET_WM_WINDOW_OPACITY / OPAQUE` and not at all at 0
/// (`steamcompmgr.cpp:2660`, `:3260`).
#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn steam_overlay_opacity_is_applied() {
	let display_path = unique_path("display-opacity");
	let record_dir = unique_path("frames-opacity");
	let _ = std::fs::remove_file(&display_path);
	std::fs::create_dir_all(&record_dir).expect("create record dir");
	let mut guard = OverlayGuard {
		display_path: display_path.clone(),
		record_dir: record_dir.clone(),
	};
	let script = format!(
		"printf %s \"$DISPLAY\" > {}; exec xev -geometry {WIDTH}x{HEIGHT}",
		shell_quote(&display_path)
	);
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: Some(record_dir.clone()),
			max_frames: Some(RECORD_LIMIT),
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};
	let runner = thread::spawn(move || run(config));
	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT).expect("X display");
	let (conn, _) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");

	let (baseline_sequence, baseline) =
		wait_for_mapped_frame(&record_dir, MAPPED_TIMEOUT).expect("a stable mapped base frame");
	// The notification (80x60 fit to the output) covers the whole frame.
	let probe = (WIDTH as usize / 2, HEIGHT as usize / 2);
	let base_pixel = pixel(&baseline, probe.0, probe.1).expect("probe inside the frame");
	let overlay = create_overlay(&conn, 10, 10, 80, 60, false);
	let (mut sequence, _, _) = wait_for_overlay_region(
		&record_dir,
		baseline_sequence,
		WIDTH as usize,
		HEIGHT as usize,
		OVERLAY_TIMEOUT,
	)
	.expect("the opaque overlay is composited");

	// 50%: the overlay's green blended over the base window.
	set_cardinal(&conn, overlay, "_NET_WM_WINDOW_OPACITY", u32::MAX / 2);
	let half = |rgb: [u8; 3]| {
		let expected = |over: u8, under: u8| (f32::from(over) + f32::from(under)) / 2.0;
		let near = |got: u8, want: f32| (f32::from(got) - want).abs() <= 12.0;
		near(rgb[0], expected(0, base_pixel[0]))
			&& near(rgb[1], expected(255, base_pixel[1]))
			&& near(rgb[2], expected(0, base_pixel[2]))
	};
	let (seq, _) = wait_for_pixel(&record_dir, sequence, probe, half, OVERLAY_TIMEOUT)
		.expect("a frame with the overlay blended at 50% opacity");
	sequence = seq;

	// 0: hidden, the base shows through unchanged.
	set_cardinal(&conn, overlay, "_NET_WM_WINDOW_OPACITY", 0);
	let (seq, _) = wait_for_pixel(&record_dir, sequence, probe, |rgb| rgb == base_pixel, OVERLAY_TIMEOUT)
		.expect("a frame with the overlay hidden at opacity 0");
	sequence = seq;

	// 100%: fully opaque again.
	set_cardinal(&conn, overlay, "_NET_WM_WINDOW_OPACITY", u32::MAX);
	wait_for_pixel(&record_dir, sequence, probe, is_overlay_color, OVERLAY_TIMEOUT)
		.expect("a frame with the overlay opaque again");

	kill_exact("xev");
	assert!(join_with_timeout(runner, STOP_TIMEOUT), "compositor did not stop");
	guard.cleanup();
}
