//! GPU integration test for the `steam` gate.
//!
//! The gate for this unit: with `--steam` on, the compositor publishes the
//! focused app id to Steam over the root properties `GAMESCOPE_FOCUSED_APP`
//! (input-focus window) and `GAMESCOPE_FOCUSED_APP_GFX` (base focus window), and
//! the values follow focus. With `--steam` off neither property is written —
//! gamescope's own `steamMode` gate (`steamcompmgr.cpp:5376-5383`).
//!
//! There is no real Steam here, so the test speaks the same above-the-wire
//! interface Steam does with `x11rb` (re-exported by smithay, no new
//! dependency): it creates X11 windows tagged with the Steam atoms, maps them
//! over a fullscreen `xev`, and reads the root properties back. The app id is
//! derived from the atoms only; the process/cgroup-derived id gamescope also
//! uses is deliberately deferred, so a window without a Steam
//! atom has no app id and its property is absent.
//!
//! This is the oracle for `steam`; it has not been validated against a real
//! Steam session.
//!
//! It is `#[ignore]`d: it needs a render node, Xwayland and `xev`. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test steam_backchannel -- --ignored --nocapture
//! cargo test -p ferriscope-core --test steam_backchannel -- --ignored --exact focused_app_backchannel_tracks_focus_with_steam --nocapture
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

/// Output size; the software readback is not used, so this only needs to be
/// non-degenerate.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
/// The `STEAM_GAME` app id of the first game window.
const GAME_A_APP_ID: u32 = 440;
/// The `STEAM_GAME` app id of the second, later-mapped game window.
const GAME_B_APP_ID: u32 = 548_430;
/// Budget for the compositor to start Xwayland and write the display file.
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for a window to take focus after it maps.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the backchannel to reach the expected value.
const PROPERTY_TIMEOUT: Duration = Duration::from_secs(10);
/// Settle time after a map before reading back focus state.
const SETTLE: Duration = Duration::from_millis(500);
/// Watchdog budget for the compositor thread to exit once `xev` is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard safety exit for the compositor in case cleanup does not stop it.
const STOP_AFTER_FRAMES: u64 = 4000;
/// Poll slice while waiting on X state.
const POLL_SLICE: Duration = Duration::from_millis(25);

/// The interned atoms the test uses, including the two backchannel properties.
struct Atoms {
	net_active_window: Atom,
	game: Atom,
	big_picture: Atom,
	focused_app: Atom,
	focused_app_gfx: Atom,
}

impl Atoms {
	fn new(conn: &RustConnection) -> Self {
		Self {
			net_active_window: intern(conn, b"_NET_ACTIVE_WINDOW"),
			game: intern(conn, b"STEAM_GAME"),
			big_picture: intern(conn, b"STEAM_BIGPICTURE"),
			focused_app: intern(conn, b"GAMESCOPE_FOCUSED_APP"),
			focused_app_gfx: intern(conn, b"GAMESCOPE_FOCUSED_APP_GFX"),
		}
	}
}

fn intern(conn: &RustConnection, name: &[u8]) -> Atom {
	conn.intern_atom(false, name)
		.expect("intern atom")
		.reply()
		.expect("intern atom reply")
		.atom
}

/// Kill processes and remove temp state on drop, so a failed assertion does not
/// leak an `xev`, an `Xwayland` or a display file.
struct SteamGuard {
	display_path: PathBuf,
}

impl SteamGuard {
	fn cleanup(&mut self) {
		kill_exact("xev");
		kill_exact("Xwayland");
		let _ = std::fs::remove_file(&self.display_path);
	}
}

impl Drop for SteamGuard {
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
	std::env::temp_dir().join(format!("ferriscope-steam-{tag}-{}-{nanos}", std::process::id()))
}

/// Start the compositor with `steam` set, hosting an `xev` that records its
/// display, and return the compositor thread and the display file.
fn start_compositor(steam: bool) -> (thread::JoinHandle<Result<()>>, PathBuf) {
	let display_path = unique_path("display");
	let _ = std::fs::remove_file(&display_path);

	let script = format!("printf %s \"$DISPLAY\" > {}; exec xev", shell_quote(&display_path));
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		steam,
		command: Some(vec!["sh".to_string(), "-c".to_string(), script]),
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope_core::RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};
	let runner = thread::spawn(move || run(config));
	(runner, display_path)
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

/// Create a managed X11 window; `parent` sets `WM_TRANSIENT_FOR`.
fn create_window(conn: &RustConnection, x: i16, y: i16, w: u16, h: u16, parent: Option<Window>) -> Window {
	let root = conn.setup().roots[0].root;
	let window = conn.generate_id().expect("window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		root,
		x,
		y,
		w,
		h,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(0x00202020)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create window");
	if let Some(parent) = parent {
		conn.change_property32(
			PropMode::REPLACE,
			window,
			intern(conn, b"WM_TRANSIENT_FOR"),
			AtomEnum::WINDOW,
			&[parent],
		)
		.expect("set WM_TRANSIENT_FOR");
	}
	conn.flush().expect("flush");
	window
}

/// Set a `CARDINAL` property on a window and flush.
fn set_cardinal(conn: &RustConnection, window: Window, atom: Atom, value: u32) {
	conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[value])
		.expect("set property");
	conn.flush().expect("flush");
}

/// Delete a property from a window and flush.
fn clear_property(conn: &RustConnection, window: Window, atom: Atom) {
	conn.delete_property(window, atom).expect("delete property");
	conn.flush().expect("flush");
}

fn map(conn: &RustConnection, window: Window) {
	conn.map_window(window).expect("map window");
	conn.flush().expect("flush");
}

fn destroy(conn: &RustConnection, window: Window) {
	conn.destroy_window(window).expect("destroy window");
	conn.flush().expect("flush");
}

/// The root `_NET_ACTIVE_WINDOW`, if set.
fn active_window(conn: &RustConnection, atoms: &Atoms) -> Option<Window> {
	let root = conn.setup().roots[0].root;
	let reply = conn
		.get_property(false, root, atoms.net_active_window, AtomEnum::WINDOW, 0, 1)
		.ok()?
		.reply()
		.ok()?;
	if reply.type_ == u32::from(AtomEnum::NONE) {
		return None;
	}
	reply.value32().and_then(|mut values| values.next())
}

/// Wait, bounded, for `window` to become `_NET_ACTIVE_WINDOW`.
fn wait_for_active(conn: &RustConnection, atoms: &Atoms, window: Window, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if active_window(conn, atoms) == Some(window) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for any window to become `_NET_ACTIVE_WINDOW`.
///
/// The xwm initialises the property to `0` (no window) at start-up, so only a
/// non-zero value is a window. Without this a game could map before the hosted
/// `xev` and, with the newest-map-wins rule of gamescope's
/// `is_focus_priority_greater` (`steamcompmgr.cpp:4102`), win focus from it.
fn wait_for_any_active(conn: &RustConnection, atoms: &Atoms, timeout: Duration) -> Option<Window> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(window) = active_window(conn, atoms).filter(|window| *window != 0) {
			return Some(window);
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// A root property's single 32-bit value, or `None` when absent.
fn root_cardinal(conn: &RustConnection, atom: Atom) -> Option<u32> {
	let root = conn.setup().roots[0].root;
	let reply = conn
		.get_property(false, root, atom, AtomEnum::ANY, 0, 1)
		.ok()?
		.reply()
		.ok()?;
	if reply.type_ == u32::from(AtomEnum::NONE) {
		return None;
	}
	if reply.format != 32 {
		return None;
	}
	reply.value32().and_then(|mut values| values.next())
}

/// Wait, bounded, for a root property to become `expected`.
fn wait_for_root_cardinal(conn: &RustConnection, atom: Atom, expected: Option<u32>, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if root_cardinal(conn, atom) == expected {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
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

/// Quote a path for `sh -c`, so a temp path with spaces still works.
fn shell_quote(path: &Path) -> String {
	format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn focused_app_backchannel_tracks_focus_with_steam() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = SteamGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	// The `xev` base window has no Steam atom: with the mode on but no app id,
	// both properties are absent, exactly as gamescope deletes a zero id.
	let _base = wait_for_any_active(&conn, &atoms, FOCUS_TIMEOUT).expect("a base window became active");
	assert_eq!(root_cardinal(&conn, atoms.focused_app), None, "base has no app id");
	assert_eq!(root_cardinal(&conn, atoms.focused_app_gfx), None, "base has no app id");

	// A game maps and takes the role from the non-game base; both properties
	// become its app id.
	let game_a = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game_a, atoms.game, GAME_A_APP_ID);
	map(&conn, game_a);
	assert!(
		wait_for_active(&conn, &atoms, game_a, FOCUS_TIMEOUT),
		"the first game did not take focus within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app, Some(GAME_A_APP_ID), PROPERTY_TIMEOUT),
		"GAMESCOPE_FOCUSED_APP did not become {GAME_A_APP_ID}"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app_gfx, Some(GAME_A_APP_ID), PROPERTY_TIMEOUT),
		"GAMESCOPE_FOCUSED_APP_GFX did not become {GAME_A_APP_ID}"
	);

	// A newer game displaces the older one: both properties follow focus.
	let game_b = create_window(&conn, 10, 10, 200, 150, None);
	set_cardinal(&conn, game_b, atoms.game, GAME_B_APP_ID);
	map(&conn, game_b);
	assert!(
		wait_for_active(&conn, &atoms, game_b, FOCUS_TIMEOUT),
		"the second game did not take focus within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app, Some(GAME_B_APP_ID), PROPERTY_TIMEOUT),
		"GAMESCOPE_FOCUSED_APP did not follow the newer game"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app_gfx, Some(GAME_B_APP_ID), PROPERTY_TIMEOUT),
		"GAMESCOPE_FOCUSED_APP_GFX did not follow the newer game"
	);

	// A parented dialog (no Steam atom) takes keyboard focus while the game
	// keeps the role: `_APP` is absent (the process-derived id is deferred) and
	// `_GFX` stays on the game.
	let dialog = create_window(&conn, 40, 40, 120, 90, Some(game_b));
	map(&conn, dialog);
	assert!(
		wait_for_active(&conn, &atoms, dialog, FOCUS_TIMEOUT),
		"the dialog did not take focus within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app, None, PROPERTY_TIMEOUT),
		"a dialog without an atom app id must leave GAMESCOPE_FOCUSED_APP absent"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app_gfx, Some(GAME_B_APP_ID), PROPERTY_TIMEOUT),
		"the base game must keep GAMESCOPE_FOCUSED_APP_GFX while the dialog is focused"
	);

	// Destroying the dialog returns focus to the game; both properties are back.
	destroy(&conn, dialog);
	assert!(
		wait_for_active(&conn, &atoms, game_b, FOCUS_TIMEOUT),
		"focus did not return to the game after the dialog was destroyed"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app, Some(GAME_B_APP_ID), PROPERTY_TIMEOUT),
		"GAMESCOPE_FOCUSED_APP did not return to the game"
	);

	// A `STEAM_BIGPICTURE` property change on the focused window republishes its
	// app id without a focus transition; clearing it falls back to `STEAM_GAME`.
	set_cardinal(&conn, game_b, atoms.big_picture, 1);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app, Some(769), PROPERTY_TIMEOUT),
		"STEAM_BIGPICTURE did not republish GAMESCOPE_FOCUSED_APP as 769"
	);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app_gfx, Some(769), PROPERTY_TIMEOUT),
		"STEAM_BIGPICTURE did not republish GAMESCOPE_FOCUSED_APP_GFX as 769"
	);
	clear_property(&conn, game_b, atoms.big_picture);
	assert!(
		wait_for_root_cardinal(&conn, atoms.focused_app, Some(GAME_B_APP_ID), PROPERTY_TIMEOUT),
		"clearing STEAM_BIGPICTURE did not restore the game app id"
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn focused_app_backchannel_absent_when_steam_disabled() {
	let (runner, display_path) = start_compositor(false);
	let mut guard = SteamGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	let _base = wait_for_any_active(&conn, &atoms, FOCUS_TIMEOUT).expect("a base window became active");

	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_A_APP_ID);
	map(&conn, game);
	assert!(
		wait_for_active(&conn, &atoms, game, FOCUS_TIMEOUT),
		"the game did not take focus within {FOCUS_TIMEOUT:?}"
	);

	// With the mode off, gamescope writes neither property; neither does
	// ferriscope, even though the atom classification still selected the game.
	thread::sleep(SETTLE);
	assert_eq!(
		root_cardinal(&conn, atoms.focused_app),
		None,
		"GAMESCOPE_FOCUSED_APP must stay absent with --steam off"
	);
	assert_eq!(
		root_cardinal(&conn, atoms.focused_app_gfx),
		None,
		"GAMESCOPE_FOCUSED_APP_GFX must stay absent with --steam off"
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}
