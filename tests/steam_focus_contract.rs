//! GPU integration test for the focused-window contract.
//!
//! The gate for this unit: with `--steam` on, ferriscope publishes the same root
//! properties gamescope does for Steam's controller routing —
//! `GAMESCOPE_FOCUSED_APP`/`GAMESCOPE_FOCUSED_APP_GFX`,
//! `GAMESCOPE_FOCUSED_WINDOW`, the three `*_FOCUS_DISPLAY` names and the
//! `GAMESCOPE_FOCUSABLE_APPS`/`GAMESCOPE_FOCUSABLE_WINDOWS` lists — and they
//! follow focus. The display names are asserted to be the exact
//! `XA_CARDINAL`/format-32 shape gamescope writes, not `UTF8_STRING`. When an
//! interactive overlay holds focus the input app id splits
//! to the Steam UI (769) while `_GFX` stays on the game, and Big Picture (769)
//! is dropped from the focusable lists. The interactive overlay is also
//! asserted to be raised above the game in `_NET_CLIENT_LIST_STACKING`
//! (Xwayland's own X stacking, which it routes pointer events by), and the game
//! to be raised back when the overlay closes.
//!
//! `--steam` means Steam-controlled focus, as gamescope's `-e` does
//! (`steamcompmgr.cpp:4254-4286`, `:4483`): only windows with an app id
//! (`STEAM_GAME`, ...) or the Steam UI are focus candidates, and the base layer
//! is whatever Steam names in the root property
//! `GAMESCOPECTRL_BASELAYER_APPID` (app ids in priority order). Without a
//! match there is no base layer, so the hosted `xev`, which has no app id, never
//! takes the role. There is no real Steam here, so the test speaks the same
//! above-the-wire interface Steam does with `x11rb` (re-exported by smithay, no
//! new dependency): it creates X11 windows tagged with the Steam atoms, maps
//! them next to the `xev`, writes the base-layer request the way Steam does, and
//! reads the root properties back. The pid in the
//! focusable-window triplets comes from `_NET_WM_PID`, which the test sets to a
//! known value; a window without it contributes 0.
//!
//! A third test covers the iconify round trip. Wine iconifies a fullscreen game
//! that loses the foreground to the Steam overlay (`WM_CHANGE_STATE` to
//! `IconicState`) and restores it only when `WM_STATE` goes back to
//! `NormalState`; the compositor acknowledges the request and takes the window
//! out of iconic when the overlay closes.
//!
//! This is the oracle for the focus contract; it has not been validated against
//! a real Steam session.
//!
//! It is `#[ignore]`d: it needs a render node, Xwayland and `xev`. The
//! tests each own an `xev`/`Xwayland` and clean up by exact process name, so
//! they must not run concurrently — run them with `--test-threads=1`:
//!
//! ```text
//! cargo test -p ferriscope --test steam_focus_contract -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};

use ferriscope::{CompositorConfig, Result, display_id_to_server_id, run};
use smithay::reexports::x11rb::{
	self, COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, connection::Connection, protocol::xproto::*,
	rust_connection::RustConnection, wrapper::ConnectionExt as _,
};

/// Output size; the software readback is not used, so this only needs to be
/// non-degenerate. The interactive overlay below is this wide so it is the
/// full-width interactive overlay, not a notification.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
/// The `STEAM_GAME` app id of the game window.
const GAME_APP_ID: u32 = 548_430;
/// The `STEAM_GAME` app id set late on an already-mapped peer.
const LATE_APP_ID: u32 = 11_000;
/// The `_NET_WM_PID` the game window advertises. gamescope ignores it and
/// publishes the pid the X server reports for the owning client
/// (`get_win_pid`, `steamcompmgr.cpp:5858`, XRes), so the triplets carry
/// [`client_pid`] instead; the property is kept to prove it is not trusted.
const GAME_PID: u32 = 4242;
/// The `_NET_WM_PID` the Big Picture window advertises (see [`GAME_PID`]).
const BPM_PID: u32 = 4243;

/// The pid the X server reports for every window this test creates: its own.
fn client_pid() -> u32 {
	std::process::id()
}
/// Steam's own app id (Big Picture).
const BIG_PICTURE_APP_ID: u32 = 769;
/// Budget for the compositor to start Xwayland and write the display file.
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for a window to take focus after it maps.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a root property to reach the expected value.
const PROPERTY_TIMEOUT: Duration = Duration::from_secs(10);
/// Settle time after a map before reading back state in the off case.
const SETTLE: Duration = Duration::from_millis(600);
/// Watchdog budget for the compositor thread to exit once `xev` is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard safety exit for the compositor in case cleanup does not stop it.
const STOP_AFTER_FRAMES: u64 = 4000;
/// Poll slice while waiting on X state.
const POLL_SLICE: Duration = Duration::from_millis(25);

/// The interned atoms the test uses, including the focus-contract properties.
struct Atoms {
	net_active_window: Atom,
	net_client_list_stacking: Atom,
	game: Atom,
	overlay: Atom,
	input_focus: Atom,
	focused_app: Atom,
	focused_app_gfx: Atom,
	focused_window: Atom,
	focus_display: Atom,
	mouse_focus_display: Atom,
	keyboard_focus_display: Atom,
	focusable_apps: Atom,
	focusable_windows: Atom,
	net_wm_pid: Atom,
	xwayland_server_id: Atom,
	gamescope_pid: Atom,
	baselayer_appid: Atom,
}

impl Atoms {
	fn new(conn: &RustConnection) -> Self {
		Self {
			net_active_window: intern(conn, b"_NET_ACTIVE_WINDOW"),
			net_client_list_stacking: intern(conn, b"_NET_CLIENT_LIST_STACKING"),
			game: intern(conn, b"STEAM_GAME"),
			overlay: intern(conn, b"STEAM_OVERLAY"),
			input_focus: intern(conn, b"STEAM_INPUT_FOCUS"),
			focused_app: intern(conn, b"GAMESCOPE_FOCUSED_APP"),
			focused_app_gfx: intern(conn, b"GAMESCOPE_FOCUSED_APP_GFX"),
			focused_window: intern(conn, b"GAMESCOPE_FOCUSED_WINDOW"),
			focus_display: intern(conn, b"GAMESCOPE_FOCUS_DISPLAY"),
			mouse_focus_display: intern(conn, b"GAMESCOPE_MOUSE_FOCUS_DISPLAY"),
			keyboard_focus_display: intern(conn, b"GAMESCOPE_KEYBOARD_FOCUS_DISPLAY"),
			focusable_apps: intern(conn, b"GAMESCOPE_FOCUSABLE_APPS"),
			focusable_windows: intern(conn, b"GAMESCOPE_FOCUSABLE_WINDOWS"),
			net_wm_pid: intern(conn, b"_NET_WM_PID"),
			xwayland_server_id: intern(conn, b"GAMESCOPE_XWAYLAND_SERVER_ID"),
			gamescope_pid: intern(conn, b"GAMESCOPE_PID"),
			baselayer_appid: intern(conn, b"GAMESCOPECTRL_BASELAYER_APPID"),
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
struct ContractGuard {
	display_path: PathBuf,
}

impl ContractGuard {
	fn cleanup(&mut self) {
		kill_exact("xev");
		kill_exact("Xwayland");
		let _ = std::fs::remove_file(&self.display_path);
	}
}

impl Drop for ContractGuard {
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
	std::env::temp_dir().join(format!("ferriscope-contract-{tag}-{}-{nanos}", std::process::id()))
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
		output: ferriscope::FrameOutput::Recorder(ferriscope::RecorderOutput {
			stop_after_frames: Some(STOP_AFTER_FRAMES),
			..ferriscope::RecorderOutput::default()
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

/// Steam's base-layer choice: `GAMESCOPECTRL_BASELAYER_APPID` on the root, the
/// app ids in priority order (`steamcompmgr.cpp:4275`). Under Steam-controlled
/// focus (`-e`) this is the only thing that makes a window the base layer: the
/// first listed app id that has a focusable window wins, and with no match the
/// global focus is empty.
fn set_base_layer(conn: &RustConnection, atoms: &Atoms, app_ids: &[u32]) {
	let root = conn.setup().roots[0].root;
	conn.change_property32(
		PropMode::REPLACE,
		root,
		atoms.baselayer_appid,
		AtomEnum::CARDINAL,
		app_ids,
	)
	.expect("set base layer");
	conn.flush().expect("flush");
}

/// Map a window and flush.
fn map(conn: &RustConnection, window: Window) {
	conn.map_window(window).expect("map window");
	conn.flush().expect("flush");
}

fn destroy(conn: &RustConnection, window: Window) {
	conn.destroy_window(window).expect("destroy window");
	conn.flush().expect("flush");
}

/// Delete a property (e.g. clear `STEAM_OVERLAY` the way Steam does when the
/// overlay closes) and flush.
fn delete_property(conn: &RustConnection, window: Window, atom: Atom) {
	conn.delete_property(window, atom).expect("delete property");
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

/// A root property's single 32-bit value, or `None` when absent.
fn root_cardinal(conn: &RustConnection, atom: Atom) -> Option<u32> {
	let root = conn.setup().roots[0].root;
	let reply = conn
		.get_property(false, root, atom, AtomEnum::ANY, 0, 1)
		.ok()?
		.reply()
		.ok()?;
	if reply.type_ == u32::from(AtomEnum::NONE) || reply.format != 32 {
		return None;
	}
	reply.value32().and_then(|mut values| values.next())
}

/// A root property's 32-bit array, or an empty vector when absent.
fn root_cardinals(conn: &RustConnection, atom: Atom) -> Vec<u32> {
	let root = conn.setup().roots[0].root;
	let Some(reply) = conn
		.get_property(false, root, atom, AtomEnum::ANY, 0, 256)
		.ok()
		.and_then(|cookie| cookie.reply().ok())
	else {
		return Vec::new();
	};
	if reply.type_ == u32::from(AtomEnum::NONE) || reply.format != 32 {
		return Vec::new();
	}
	reply.value32().map(|values| values.collect()).unwrap_or_default()
}

/// The root `_NET_CLIENT_LIST_STACKING`: the managed client windows, bottom to
/// top. The XWM rewrites it on map, unmap and raise, so it mirrors Xwayland's
/// own X stacking — which is what Xwayland uses to route pointer events.
fn client_stacking(conn: &RustConnection, atoms: &Atoms) -> Vec<u32> {
	root_cardinals(conn, atoms.net_client_list_stacking)
}

/// Whether `upper` is stacked above `lower` in `_NET_CLIENT_LIST_STACKING`.
fn stacked_above(stacking: &[u32], lower: u32, upper: u32) -> bool {
	match (
		stacking.iter().position(|window| *window == lower),
		stacking.iter().position(|window| *window == upper),
	) {
		(Some(lower), Some(upper)) => upper > lower,
		_ => false,
	}
}

/// A `GAMESCOPE_*_FOCUS_DISPLAY` value decoded the way gamescope writes it:
/// `XA_CARDINAL`, format 32, the name bytes (NUL terminated) reinterpreted as
/// little-endian 32-bit words (`steamcompmgr.cpp:5388-5395`). Panics if the
/// property is present with any other type or format, so every read asserts
/// the exact gamescope type the reference commit's `UTF8_STRING` got wrong.
fn root_focus_display(conn: &RustConnection, atom: Atom) -> Option<String> {
	let root = conn.setup().roots[0].root;
	let reply = conn
		.get_property(false, root, atom, AtomEnum::ANY, 0, 1024)
		.ok()?
		.reply()
		.ok()?;
	if reply.type_ == u32::from(AtomEnum::NONE) {
		return None;
	}
	assert_eq!(
		reply.type_,
		intern(conn, b"CARDINAL"),
		"the focus display property must be XA_CARDINAL, like gamescope"
	);
	assert_eq!(reply.format, 32, "the focus display property must have format 32");
	let mut name = Vec::new();
	'words: for word in reply.value32().into_iter().flatten() {
		for shift in [0u32, 8, 16, 24] {
			let byte = (word >> shift) as u8;
			if byte == 0 {
				break 'words;
			}
			name.push(byte);
		}
	}
	Some(String::from_utf8_lossy(&name).into_owned())
}

/// Poll `check` until it returns `Some` or the timeout elapses.
fn wait_until<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(value) = check() {
			return Some(value);
		}
		if Instant::now() >= deadline {
			return None;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for a non-zero `_NET_ACTIVE_WINDOW`.
///
/// Xwayland briefly reports `0` before the first managed window maps, so a bare
/// "property present" check is not enough.
fn wait_for_active_window(conn: &RustConnection, atoms: &Atoms, timeout: Duration) -> Option<Window> {
	wait_until(timeout, || active_window(conn, atoms).filter(|window| *window != 0))
}

/// Whether the flattened triplets contain `[window, app, pid]`.
fn has_triplet(windows: &[u32], window: u32, app: u32, pid: u32) -> bool {
	windows.chunks_exact(3).any(|triplet| triplet == [window, app, pid])
}

/// Poll `window`'s geometry until it is `w` x `h`, or the timeout elapses.
fn wait_for_geometry(conn: &RustConnection, window: Window, w: u16, h: u16, timeout: Duration) -> bool {
	wait_until(timeout, || {
		let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
		(geometry.width == w && geometry.height == h).then_some(())
	})
	.is_some()
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
fn focus_contract_tracks_focus_and_publishes_focusable_lists() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let display_name = format!(":{display}");
	let (conn, _screen_num) = x11rb::connect(Some(&display_name)).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	// `GAMESCOPE_XWAYLAND_SERVER_ID` is published at connection time so WSI
	// clients can find the server (unconditional, not `--steam`-gated).
	assert!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_cardinal(&conn, atoms.xwayland_server_id) == Some(display_id_to_server_id(display))).then_some(())
		})
		.is_some(),
		"GAMESCOPE_XWAYLAND_SERVER_ID did not become {}",
		display_id_to_server_id(display)
	);

	// With `--steam` on, `GAMESCOPE_PID` carries the compositor's pid, which
	// Steam's gamescope composer reads to recognise the compositor. The
	// compositor runs in this test process, so the pid is `std::process::id()`.
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_cardinal(&conn, atoms.gamescope_pid) == Some(std::process::id())).then_some(())
		}),
		Some(()),
		"GAMESCOPE_PID did not become the compositor pid"
	);

	// Under Steam-controlled focus (gamescope `-e`: `steamcompmgr.cpp:4483`)
	// only windows with an app id or the Steam UI are focus candidates, so the
	// plain `xev` window Xwayland hosts never takes the role. Before Steam maps
	// anything there is no app id, no focusable app and no focused window.
	thread::sleep(SETTLE);
	assert_eq!(
		active_window(&conn, &atoms).filter(|window| *window != 0),
		None,
		"a window without a Steam identity must not become active"
	);
	assert_eq!(root_cardinal(&conn, atoms.focused_app), None, "nothing is focused yet");
	assert_eq!(
		root_cardinal(&conn, atoms.focused_app_gfx),
		None,
		"nothing is focused yet"
	);
	assert!(
		root_cardinals(&conn, atoms.focusable_apps).is_empty(),
		"no window has an app id, so GAMESCOPE_FOCUSABLE_APPS must be empty"
	);

	// A game maps and Steam names it the base layer
	// (`GAMESCOPECTRL_BASELAYER_APPID`); the window, app and gfx properties
	// become its id/app, the displays are published and it is the only focusable
	// candidate, with its `_NET_WM_PID` in the triplet. (The control write comes
	// last, as Steam writes it once it knows the game has a window; the
	// `xev` window is not a candidate either way.)
	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	set_cardinal(&conn, game, atoms.net_wm_pid, GAME_PID);
	map(&conn, game);
	set_base_layer(&conn, &atoms, &[GAME_APP_ID]);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the game did not take focus within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_focus_display(&conn, atoms.focus_display) == Some(display_name.clone())).then_some(())
		})
		.is_some(),
		"GAMESCOPE_FOCUS_DISPLAY did not become {display_name}"
	);
	assert_eq!(
		root_focus_display(&conn, atoms.mouse_focus_display),
		Some(display_name.clone())
	);
	assert_eq!(
		root_focus_display(&conn, atoms.keyboard_focus_display),
		Some(display_name.clone())
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_app)
			== Some(GAME_APP_ID))
		.then_some(())),
		Some(()),
		"GAMESCOPE_FOCUSED_APP did not become {GAME_APP_ID}"
	);
	assert_eq!(root_cardinal(&conn, atoms.focused_app_gfx), Some(GAME_APP_ID));
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_window)
			== Some(game))
		.then_some(())),
		Some(()),
		"GAMESCOPE_FOCUSED_WINDOW did not become the game window {game:#x}"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_cardinals(&conn, atoms.focusable_apps) == vec![GAME_APP_ID]).then_some(())
		}),
		Some(()),
		"GAMESCOPE_FOCUSABLE_APPS did not become [{GAME_APP_ID}]"
	);
	assert_eq!(
		root_cardinals(&conn, atoms.focusable_windows),
		vec![game, GAME_APP_ID, client_pid()]
	);

	// A window that maps before its `STEAM_GAME` atom appears has no app id, so
	// it is not a focus candidate even when Steam already ranks its future app
	// id first. A late `STEAM_GAME` dirties focus and re-runs the pass (gamescope
	// `steamcompmgr.cpp:6824,6915`): the peer becomes a game and, as Steam's
	// first choice, takes the role. Clearing the atom hands the role back to the
	// remaining game.
	let late = create_window(&conn, 15, 15, 200, 150, None);
	map(&conn, late);
	set_base_layer(&conn, &atoms, &[LATE_APP_ID, GAME_APP_ID]);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"a plain peer must not take focus from the game"
	);
	set_cardinal(&conn, late, atoms.game, LATE_APP_ID);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(late))
			.then_some(()))
		.is_some(),
		"a late STEAM_GAME did not promote the peer"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_app)
			== Some(LATE_APP_ID))
		.then_some(())),
		Some(()),
		"GAMESCOPE_FOCUSED_APP did not follow the late STEAM_GAME"
	);
	assert_eq!(root_cardinal(&conn, atoms.focused_window), Some(late));
	// Clear the atom: the window loses its app id, so it is no candidate and the
	// next app in Steam's list (the game) takes the role back.
	set_cardinal(&conn, late, atoms.game, 0);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"clearing STEAM_GAME did not hand the role back to the game"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_app)
			== Some(GAME_APP_ID))
		.then_some(())),
		Some(()),
		"GAMESCOPE_FOCUSED_APP did not return to the game"
	);
	destroy(&conn, late);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"focus did not stay on the game after the late window was destroyed"
	);

	// A Big Picture window maps and joins the focusable lists. It is newer than
	// the game, but Steam's base-layer list still names the game first, so the
	// role stays put until Steam ranks the Steam UI first; then it takes the
	// role from the game.
	set_base_layer(&conn, &atoms, &[GAME_APP_ID, BIG_PICTURE_APP_ID]);
	let bpm = create_window(&conn, 10, 10, 200, 150, None);
	set_cardinal(&conn, bpm, atoms.game, BIG_PICTURE_APP_ID);
	set_cardinal(&conn, bpm, atoms.net_wm_pid, BPM_PID);
	map(&conn, bpm);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || {
			let apps = root_cardinals(&conn, atoms.focusable_apps);
			(apps.contains(&GAME_APP_ID) && apps.contains(&BIG_PICTURE_APP_ID)).then_some(())
		})
		.is_some(),
		"Big Picture did not join GAMESCOPE_FOCUSABLE_APPS"
	);
	assert_eq!(
		active_window(&conn, &atoms),
		Some(game),
		"a newer window must not take the role from Steam's first choice"
	);
	set_base_layer(&conn, &atoms, &[BIG_PICTURE_APP_ID, GAME_APP_ID]);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(bpm))
			.then_some(()))
		.is_some(),
		"the newer Big Picture window did not take the role from the game"
	);
	// Steam puts the game first again and the game re-maps; Big Picture is a
	// displaced peer behind it, which must not move focus.
	set_base_layer(&conn, &atoms, &[GAME_APP_ID, BIG_PICTURE_APP_ID]);
	conn.unmap_window(game).expect("unmap game");
	conn.flush().expect("flush");
	map(&conn, game);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the re-mapped game did not take the role back"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_app)
			== Some(GAME_APP_ID))
		.then_some(())),
		Some(()),
		"a peer must not move focus"
	);
	let windows = root_cardinals(&conn, atoms.focusable_windows);
	assert!(has_triplet(&windows, game, GAME_APP_ID, client_pid()));
	assert!(has_triplet(&windows, bpm, BIG_PICTURE_APP_ID, client_pid()));

	// A full-width interactive overlay takes focus. The input app id splits to
	// the Steam UI (769) while `_GFX` stays on the game, and Big Picture is
	// dropped from the focusable lists so Steam routes the gamepad to the
	// overlay.
	let overlay = create_window(&conn, 0, 0, WIDTH as u16, 60, None);
	set_cardinal(&conn, overlay, atoms.overlay, 1);
	set_cardinal(&conn, overlay, atoms.input_focus, 1);
	map(&conn, overlay);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(overlay))
			.then_some(()))
		.is_some(),
		"the interactive overlay did not take focus within {FOCUS_TIMEOUT:?}"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_cardinal(&conn, atoms.focused_app) == Some(BIG_PICTURE_APP_ID)).then_some(())
		}),
		Some(()),
		"GAMESCOPE_FOCUSED_APP did not split to the Steam UI {BIG_PICTURE_APP_ID}"
	);
	assert_eq!(root_cardinal(&conn, atoms.focused_app_gfx), Some(GAME_APP_ID));
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_window)
			== Some(game))
		.then_some(())),
		Some(()),
		"GAMESCOPE_FOCUSED_WINDOW must stay the base window while the overlay holds input focus"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_cardinals(&conn, atoms.focusable_apps) == vec![GAME_APP_ID]).then_some(())
		}),
		Some(()),
		"Big Picture (769) was not dropped from GAMESCOPE_FOCUSABLE_APPS"
	);
	assert_eq!(
		root_cardinals(&conn, atoms.focusable_windows),
		vec![game, GAME_APP_ID, client_pid()]
	);

	// Destroying the overlay returns focus to the game and restores the lists.
	destroy(&conn, overlay);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"focus did not return to the game after the overlay was destroyed"
	);
	assert_eq!(
		wait_until(PROPERTY_TIMEOUT, || (root_cardinal(&conn, atoms.focused_app)
			== Some(GAME_APP_ID))
		.then_some(())),
		Some(()),
		"GAMESCOPE_FOCUSED_APP did not return to the game"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || {
			let apps = root_cardinals(&conn, atoms.focusable_apps);
			(apps.contains(&GAME_APP_ID) && apps.contains(&BIG_PICTURE_APP_ID)).then_some(())
		})
		.is_some(),
		"Big Picture did not return to GAMESCOPE_FOCUSABLE_APPS"
	);

	// Destroying the game (the role holder) must re-promote the next app in
	// Steam's base-layer list that has a window, exactly as gamescope re-runs
	// its focus pass (`steamcompmgr.cpp:4275`). The Steam UI (`bpm`, app 769) is
	// listed after the game, so it is the successor; the role must move to it rather
	// than being left empty (which clears `GAMESCOPE_FOCUSED_APP` and strands
	// `_NET_ACTIVE_WINDOW` on the dead game, so Steam no longer routes the
	// gamepad to the UI).
	destroy(&conn, game);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(bpm))
			.then_some(()))
		.is_some(),
		"the Steam UI did not become _NET_ACTIVE_WINDOW after the game was destroyed"
	);
	assert_eq!(
		root_cardinal(&conn, atoms.focused_app),
		Some(BIG_PICTURE_APP_ID),
		"the Steam UI was not re-promoted after the game was destroyed"
	);
	assert_eq!(
		root_cardinal(&conn, atoms.focused_window),
		Some(bpm),
		"GAMESCOPE_FOCUSED_WINDOW did not move to the re-promoted Steam UI"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || {
			(root_focus_display(&conn, atoms.focus_display) == Some(display_name.clone())).then_some(())
		})
		.is_some(),
		"the focus display name was not preserved after the game was destroyed \\
		 (focus_display={:?}, focused_window={:?}, focused_app={:?})",
		root_focus_display(&conn, atoms.focus_display),
		root_cardinal(&conn, atoms.focused_window),
		root_cardinal(&conn, atoms.focused_app),
	);

	// With no Steam window left the focusable lists clear: the `xev` window
	// carries no app id, so destroying the now-holder Steam UI leaves no
	// candidate with a Steam identity.
	destroy(&conn, bpm);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || {
			root_cardinals(&conn, atoms.focusable_apps).is_empty().then_some(())
		})
		.is_some(),
		"GAMESCOPE_FOCUSABLE_APPS was not cleared after every Steam window was destroyed"
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
fn focus_contract_absent_when_steam_disabled() {
	let (runner, display_path) = start_compositor(false);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	let _base = wait_for_active_window(&conn, &atoms, FOCUS_TIMEOUT).expect("a base window became active");
	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	map(&conn, game);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the game did not take focus within {FOCUS_TIMEOUT:?}"
	);

	thread::sleep(SETTLE);
	// With the mode off, gamescope (and ferriscope) write no focus contract at
	// all — there is no `--steam` mode to impersonate.
	assert_eq!(root_cardinal(&conn, atoms.focused_app), None);
	assert_eq!(root_cardinal(&conn, atoms.focused_app_gfx), None);
	assert_eq!(root_cardinal(&conn, atoms.focused_window), None);
	assert_eq!(root_focus_display(&conn, atoms.focus_display), None);
	assert_eq!(root_focus_display(&conn, atoms.mouse_focus_display), None);
	assert_eq!(root_focus_display(&conn, atoms.keyboard_focus_display), None);
	assert!(root_cardinals(&conn, atoms.focusable_apps).is_empty());
	assert!(root_cardinals(&conn, atoms.focusable_windows).is_empty());
	// `GAMESCOPE_XWAYLAND_SERVER_ID` is server discovery, not a `--steam`
	// behaviour, so it is published regardless of the mode.
	assert_eq!(
		root_cardinal(&conn, atoms.xwayland_server_id),
		Some(display_id_to_server_id(display))
	);
	// `GAMESCOPE_PID` is compositor identity, not a `--steam` behaviour:
	// gamescope writes it unconditionally at X11 init, so it is published with
	// the mode off too (`steamcompmgr.cpp:9021-9022`).
	assert_eq!(root_cardinal(&conn, atoms.gamescope_pid), Some(std::process::id()));

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// A Steam UI window displaced by a game and re-promoted when the game exits
/// must be sized to the output again.
///
/// gamescope re-applies its Steam UI resize on **every** focus pass
/// (`steamcompmgr.cpp:4816-4831`), not only when the window maps. Steam
/// re-asserts its windowed size (e.g. `1280x800`) while Big Picture is a
/// displaced peer behind a game, so without the re-apply the re-promoted Steam
/// UI keeps that size and the output-fill fit pillarboxes it (the ultrawide
/// Pummel Party report: a 16:9 buffer on a 2424x1080 output). The `320x240`
/// output stands in for the ultrawide one; the windowed size is `160x120`.
///
/// Real Steam maps Big Picture before the game launches. A window that maps
/// *later* wins focus among windows that all have an app id
/// (`is_focus_priority_greater`, `steamcompmgr.cpp:4102`), so Big Picture maps
/// first, the game maps after it and displaces it, and the game exits to
/// trigger the re-promotion.
#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn steam_ui_is_resized_to_the_output_on_repromotion() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);
	// The Steam UI maps first, as it does before a game launches, and Steam
	// names it the base layer (its app id is 769). It is sized to the output.
	set_base_layer(&conn, &atoms, &[BIG_PICTURE_APP_ID]);
	let bpm = create_window(&conn, 0, 0, 160, 120, None);
	set_cardinal(&conn, bpm, atoms.game, BIG_PICTURE_APP_ID);
	set_cardinal(&conn, bpm, atoms.net_wm_pid, BPM_PID);
	map(&conn, bpm);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(bpm))
			.then_some(()))
		.is_some(),
		"the Steam UI did not take the role within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_geometry(&conn, bpm, WIDTH as u16, HEIGHT as u16, FOCUS_TIMEOUT),
		"the Steam UI was not sized to the output at map time"
	);

	// A game maps after it and Steam ranks it first, so it takes the role: the
	// Steam UI is now a displaced peer, but ferriscope keeps it sized to the
	// output at map time so it covers the output like gamescope's
	// force-fullscreen Steam UI.
	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	map(&conn, game);
	set_base_layer(&conn, &atoms, &[GAME_APP_ID, BIG_PICTURE_APP_ID]);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the game did not take the role within {FOCUS_TIMEOUT:?}"
	);

	// Steam re-asserts the displaced Big Picture windowed size, which the
	// compositor honours for a client configure request.
	conn.configure_window(bpm, &ConfigureWindowAux::new().width(160).height(120))
		.expect("resize the displaced Steam UI");
	conn.flush().expect("flush");
	assert!(
		wait_for_geometry(&conn, bpm, 160, 120, FOCUS_TIMEOUT),
		"the displaced Steam UI did not shrink to its windowed size"
	);

	// The game exits: the Steam UI is re-promoted (next in Steam's list) and must
	// be sized back to the output, mirroring gamescope's per-focus-pass resize.
	// Without the fix it stays `160x120` and the output-fill fit pillarboxes it.
	destroy(&conn, game);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(bpm))
			.then_some(()))
		.is_some(),
		"the Steam UI did not regain the role after the game was destroyed"
	);
	assert!(
		wait_for_geometry(&conn, bpm, WIDTH as u16, HEIGHT as u16, FOCUS_TIMEOUT),
		"the re-promoted Steam UI was not sized to the output within {FOCUS_TIMEOUT:?}"
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// A focus window that drifts from the output size must be corrected on the
/// **next dirty pass**, not only at role entry.
///
/// gamescope re-applies the geometry to its focus window on every
/// `DetermineAndApplyFocus` (`steamcompmgr.cpp:4816-4831`), so a Steam UI window
/// that Steam re-sizes while it already holds the role is forced back to the
/// output when any focus-affecting event marks focus dirty. ferriscope's pass
/// must do the same: the resize is produced by the pass, not only by
/// `enter_role`. This is the residual the role-entry-only fix left open.
///
/// Setup: a game is the base layer (Steam's `GAMESCOPECTRL_BASELAYER_APPID`
/// lists it, then 769), Big Picture (769) maps as a displaced peer, and the game
/// exits to promote Big Picture.
/// Steam then re-asserts a small windowed size with a bare configure event on
/// the role-holding Steam UI and the pass must restore the output size without
/// any role change.
#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn focused_steam_ui_geometry_is_reapplied_on_a_later_dirty_pass() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	// A game maps and Steam names it the base layer, ahead of the Steam UI. (The
	// hosted `xev` has no app id, so it is never a candidate.)
	set_base_layer(&conn, &atoms, &[GAME_APP_ID, BIG_PICTURE_APP_ID]);
	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	map(&conn, game);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the game did not take the role within {FOCUS_TIMEOUT:?}"
	);

	// Big Picture maps as a displaced peer; the map path sizes it to the output.
	let bpm = create_window(&conn, 0, 0, 160, 120, None);
	set_cardinal(&conn, bpm, atoms.game, BIG_PICTURE_APP_ID);
	set_cardinal(&conn, bpm, atoms.net_wm_pid, BPM_PID);
	map(&conn, bpm);
	assert!(
		wait_for_geometry(&conn, bpm, WIDTH as u16, HEIGHT as u16, FOCUS_TIMEOUT),
		"the Steam UI was not sized to the output at map time"
	);

	// The game exits; the Steam UI is re-promoted into the role.
	destroy(&conn, game);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(bpm))
			.then_some(()))
		.is_some(),
		"the Steam UI did not regain the role after the game exited"
	);
	assert!(
		wait_for_geometry(&conn, bpm, WIDTH as u16, HEIGHT as u16, FOCUS_TIMEOUT),
		"the promoted Steam UI was not sized to the output"
	);

	// Steam re-asserts a windowed size on the role-holding Steam UI. The
	// configure marks focus dirty; the pass must re-apply the output geometry
	// even though the role did not change. Without the per-pass re-apply the
	// Steam UI stays windowed and the output-fill fit pillarboxes it.
	conn.configure_window(bpm, &ConfigureWindowAux::new().width(160).height(120))
		.expect("drift the focused Steam UI");
	conn.flush().expect("flush");
	assert!(
		wait_for_geometry(&conn, bpm, WIDTH as u16, HEIGHT as u16, FOCUS_TIMEOUT),
		"the focus pass did not restore the drifted Steam UI to the output within {FOCUS_TIMEOUT:?}"
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// The interactive Steam overlay must be raised above the game in Xwayland's X
/// stacking while it holds input focus.
///
/// Xwayland routes pointer motion/buttons by the X window under its X pointer
/// (`_NET_CLIENT_LIST_STACKING` order), **not** by the Wayland pointer focus.
/// Real Steam creates Big Picture before the game launches, so the game's XWM
/// frame ends up above the overlay's: without the raise, motion and clicks are
/// delivered to the game sitting under the overlay and the overlay's buttons
/// never respond (keyboard still works, because the X input focus is set
/// directly). gamescope keeps the `STEAM_INPUT_FOCUS` overlay as the topmost
/// input window (`inputFocus` = `overlayWindow`, raised above the client list,
/// `steamcompmgr.cpp:5141,4788`); ferriscope must match.
///
/// Closing the overlay returns input to the game, so the game must be raised
/// back above the demoted Big Picture window — otherwise its clicks would be
/// shadowed by the still-mapped overlay.
#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn interactive_overlay_is_raised_above_the_game_in_x_stacking() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	// Big Picture (the Steam UI, 769) maps first, exactly as it does before a
	// game launches, and is the base layer.
	set_base_layer(&conn, &atoms, &[BIG_PICTURE_APP_ID]);
	let bpm = create_window(&conn, 0, 0, WIDTH as u16, HEIGHT as u16, None);
	set_cardinal(&conn, bpm, atoms.game, BIG_PICTURE_APP_ID);
	set_cardinal(&conn, bpm, atoms.net_wm_pid, BPM_PID);
	map(&conn, bpm);

	// The game launches after Big Picture: its XWM frame is created (and
	// stacked) above the overlay's, and Steam ranks it first, so it takes the
	// role.
	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	set_cardinal(&conn, game, atoms.net_wm_pid, GAME_PID);
	map(&conn, game);
	set_base_layer(&conn, &atoms, &[GAME_APP_ID, BIG_PICTURE_APP_ID]);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the game did not take the role within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || stacked_above(
			&client_stacking(&conn, &atoms),
			bpm,
			game
		)
		.then_some(()))
		.is_some(),
		"the game did not start above the displaced Big Picture window (stacking={:?})",
		client_stacking(&conn, &atoms)
	);

	// Opening the overlay (`STEAM_OVERLAY` + `STEAM_INPUT_FOCUS`) makes Big
	// Picture the interactive input focus; the overlay must be raised above the
	// game so Xwayland routes pointer events to it.
	set_cardinal(&conn, bpm, atoms.overlay, 1);
	set_cardinal(&conn, bpm, atoms.input_focus, 1);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(bpm))
			.then_some(()))
		.is_some(),
		"the interactive overlay did not take input focus within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || stacked_above(
			&client_stacking(&conn, &atoms),
			game,
			bpm
		)
		.then_some(()))
		.is_some(),
		"the interactive overlay was not raised above the game in X stacking (stacking={:?})",
		client_stacking(&conn, &atoms)
	);

	// Closing the overlay returns input to the game; the game must be raised
	// back above the still-mapped, demoted Big Picture window.
	delete_property(&conn, bpm, atoms.overlay);
	delete_property(&conn, bpm, atoms.input_focus);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"focus did not return to the game after the overlay closed"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || stacked_above(
			&client_stacking(&conn, &atoms),
			bpm,
			game
		)
		.then_some(()))
		.is_some(),
		"the game was not restored above the closed overlay in X stacking (stacking={:?})",
		client_stacking(&conn, &atoms)
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// The X server's current input focus window.
fn input_focus(conn: &RustConnection) -> Option<Window> {
	conn.get_input_focus().ok()?.reply().ok().map(|reply| reply.focus)
}

/// Make `window` an ICCCM **GloballyActive** client: `WM_HINTS.input = False`
/// plus `WM_TAKE_FOCUS` in `WM_PROTOCOLS`, the model Proton/Wine games (and
/// Steam's CEF subwindows) use.
///
/// Smithay's `X11Surface::enter` only sends `WM_TAKE_FOCUS` for these and never
/// calls `XSetInputFocus`, so the X input focus is left where it was; gamescope
/// forces it unconditionally (`steamcompmgr.cpp:4693`), and so must ferriscope.
fn make_globally_active(conn: &RustConnection, window: Window) {
	let wm_hints = intern(conn, b"WM_HINTS");
	// WM_HINTS fields: flags, input, initial_state, ... Bit 0 is InputHint, so
	// `input = 0` with the flag set means "GloballyActive".
	conn.change_property32(
		PropMode::REPLACE,
		window,
		wm_hints,
		wm_hints,
		&[1, 0, 0, 0, 0, 0, 0, 0, 0],
	)
	.expect("set WM_HINTS");
	let protocols = intern(conn, b"WM_PROTOCOLS");
	let take_focus = intern(conn, b"WM_TAKE_FOCUS");
	conn.change_property32(PropMode::REPLACE, window, protocols, AtomEnum::ATOM, &[take_focus])
		.expect("set WM_PROTOCOLS");
	conn.flush().expect("flush");
}

/// A GloballyActive game must still receive the X input focus, and a focus
/// stolen to another toplevel must be corrected.
///
/// Regression for ESC reaching Steam instead of a Proton game under
/// ferriscope: the focused-app contract named the game, but the X
/// input focus stayed on the Steam UI, because (a) ferriscope never forced
/// `XSetInputFocus` for a GloballyActive client (gamescope always does,
/// `steamcompmgr.cpp:4693`) and (b) it did not correct a focus that escaped the
/// focused window (gamescope's `FocusOut` handler, `:8602-8638`).
#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn globally_active_game_keeps_the_x_input_focus() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};

	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	// A GloballyActive game maps and Steam names it the base layer. The
	// compositor must force the X input focus to it, even though Smithay only
	// sends `WM_TAKE_FOCUS` for this input model. (The hosted `xev` has no app
	// id, so it is never a candidate and never holds the role.)
	let game = create_window(&conn, 5, 5, 200, 150, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	make_globally_active(&conn, game);
	map(&conn, game);
	set_base_layer(&conn, &atoms, &[GAME_APP_ID]);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (root_cardinal(&conn, atoms.focused_window)
			== Some(game))
		.then_some(()))
		.is_some(),
		"the GloballyActive game did not take the role within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (input_focus(&conn) == Some(game)).then_some(())).is_some(),
		"the compositor did not force the X input focus to the GloballyActive game (stayed at {:#x})",
		input_focus(&conn).unwrap_or(0)
	);

	// A second toplevel steals the X input focus. The compositor must notice the
	// `FocusOut` on the game and put the focus back (gamescope's correction).
	let thief = create_window(&conn, 15, 15, 200, 150, None);
	map(&conn, thief);
	thread::sleep(SETTLE);
	conn.set_input_focus(InputFocus::NONE, thief, x11rb::CURRENT_TIME)
		.expect("steal input focus");
	conn.flush().expect("flush");
	assert_eq!(input_focus(&conn), Some(thief), "the test failed to steal the focus");
	assert!(
		wait_until(FOCUS_TIMEOUT, || (input_focus(&conn) == Some(game)).then_some(())).is_some(),
		"the compositor did not correct the stolen focus back to the game (focus is {:#x})",
		input_focus(&conn).unwrap_or(0)
	);
	assert_eq!(
		active_window(&conn, &atoms),
		Some(game),
		"the thief toplevel (no app id, not a candidate) must not take the role from the game"
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}

/// A window's `WM_STATE` value (`NormalState` = 1, `IconicState` = 3), if set.
fn wm_state(conn: &RustConnection, window: Window) -> Option<u32> {
	let wm_state = intern(conn, b"WM_STATE");
	let reply = conn
		.get_property(false, window, wm_state, wm_state, 0, 2)
		.ok()?
		.reply()
		.ok()?;
	reply.value32().and_then(|mut values| values.next())
}

/// Send `WM_CHANGE_STATE` for `window` to the root, the way `XIconifyWindow`
/// (and Wine) request iconification.
fn request_iconic(conn: &RustConnection, window: Window) {
	let root = conn.setup().roots[0].root;
	let event = ClientMessageEvent::new(32, window, intern(conn, b"WM_CHANGE_STATE"), [3u32, 0, 0, 0, 0]);
	conn.send_event(
		false,
		root,
		EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
		event,
	)
	.expect("send WM_CHANGE_STATE");
	conn.flush().expect("flush");
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn an_iconified_game_is_restored_when_the_overlay_closes() {
	let (runner, display_path) = start_compositor(true);
	let mut guard = ContractGuard {
		display_path: display_path.clone(),
	};
	let display = wait_for_display(&display_path, DISPLAY_TIMEOUT)
		.unwrap_or_else(|| panic!("the compositor did not write an X display within {DISPLAY_TIMEOUT:?}"));
	let (conn, _screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let atoms = Atoms::new(&conn);

	let game = create_window(&conn, 0, 0, WIDTH as u16, HEIGHT as u16, None);
	set_cardinal(&conn, game, atoms.game, GAME_APP_ID);
	map(&conn, game);
	set_base_layer(&conn, &atoms, &[GAME_APP_ID]);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"the game did not take focus"
	);

	// The Steam overlay takes input focus; the game loses the foreground and
	// asks to be iconified, which is acknowledged by writing `WM_STATE`
	// (`handle_wm_change_state`, `steamcompmgr.cpp:6431`).
	let overlay = create_window(&conn, 0, 0, WIDTH as u16, 60, None);
	set_cardinal(&conn, overlay, atoms.overlay, 1);
	set_cardinal(&conn, overlay, atoms.input_focus, 1);
	map(&conn, overlay);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(overlay))
			.then_some(()))
		.is_some(),
		"the overlay did not take focus"
	);
	request_iconic(&conn, game);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || (wm_state(&conn, game) == Some(3)).then_some(())).is_some(),
		"the iconify request was not acknowledged with WM_STATE = IconicState"
	);

	// The overlay closes and the game takes the keyboard back: the window is
	// taken out of iconic, which is what lets the client restore itself.
	destroy(&conn, overlay);
	assert!(
		wait_until(FOCUS_TIMEOUT, || (active_window(&conn, &atoms) == Some(game))
			.then_some(()))
		.is_some(),
		"focus did not return to the game"
	);
	assert!(
		wait_until(PROPERTY_TIMEOUT, || (wm_state(&conn, game) == Some(1)).then_some(())).is_some(),
		"the game was not taken out of IconicState when the overlay closed"
	);

	kill_exact("xev");
	assert!(
		join_with_timeout(runner, STOP_TIMEOUT),
		"the compositor thread did not exit within {STOP_TIMEOUT:?} after xev was killed"
	);
	guard.cleanup();
}
